import io
from dataclasses import replace
from pathlib import Path

import pytest

from mzpeak.manifest import FileDigest, Manifest
from mzpeak.stages import Stage, InputReference, SoftwareIdentity, verify_lineage

FIXTURES = Path(__file__).resolve().parents[2] / "test" / "data" / "provenance"


def chain():
    first = Stage(
        Manifest(
            "run-1",
            "acquisition-1",
            (FileDigest.from_stream("raw.parquet", io.BytesIO(b"raw")),),
        ),
        "acquisition",
    )
    second = Stage(
        Manifest(
            "run-1",
            "processing-1",
            (FileDigest.from_stream("processed.parquet", io.BytesIO(b"processed")),),
        ),
        "processing",
        (first.digest(),),
        (
            InputReference(
                first.digest(), "raw.parquet", first.manifest.files[0].sha256
            ),
        ),
        SoftwareIdentity("processor", "1.2.3", "a" * 64, "b" * 64),
    )
    return first, second


def test_shared_stage_vectors():
    first_data = (FIXTURES / "acquisition-v1.json").read_bytes()
    second_data = (FIXTURES / "processing-v1.json").read_bytes()
    first, second = Stage.from_bytes(first_data), Stage.from_bytes(second_data)
    assert first.canonical_bytes() == first_data
    assert second.canonical_bytes() == second_data
    report = verify_lineage([second, first], [second.digest()])
    assert report.stage_count == 2
    assert report.checkpoint_checked
    assert report.heads == (second.digest(),)


def test_valid_append_preserves_acquisition():
    first, second = chain()
    original = first.canonical_bytes()
    report = verify_lineage([first, second])
    assert report.acquisitions == (first.digest(),)
    assert report.heads == (second.digest(),)
    assert first.canonical_bytes() == original
    assert not report.checkpoint_checked


def test_external_checkpoint_detects_whole_stage_removal():
    first, second = chain()
    checkpoint = [second.digest()]
    assert verify_lineage([first, second], checkpoint).checkpoint_checked
    with pytest.raises(ValueError, match="checkpoint"):
        verify_lineage([first], checkpoint)
    # Without independent checkpoint state, a removed final stage is unknowable.
    assert verify_lineage([first]).stage_count == 1


def test_missing_parent_rejected():
    _, second = chain()
    with pytest.raises(ValueError, match="missing parent"):
        verify_lineage([second])


def test_exact_input_binding():
    first, second = chain()
    for ref in (
        replace(second.inputs[0], path="other.parquet"),
        replace(second.inputs[0], sha256="c" * 64),
    ):
        with pytest.raises(ValueError, match="input"):
            verify_lineage([first, replace(second, inputs=(ref,))])


def test_original_cannot_be_overwritten():
    first, second = chain()
    second = replace(
        second,
        manifest=replace(
            second.manifest,
            files=(replace(second.manifest.files[0], path="raw.parquet"),),
        ),
    )
    with pytest.raises(ValueError, match="overwrites"):
        verify_lineage([first, second])


def test_processing_identity_changes_signed_bytes():
    _, second = chain()
    digest = second.digest()
    for software in (
        replace(second.software, release_sha256="c" * 64),
        replace(second.software, parameters_sha256="c" * 64),
        replace(second.software, version="1.2.4"),
    ):
        assert replace(second, software=software).digest() != digest


def test_converter_cannot_relabel_processing_as_acquisition():
    _, second = chain()
    with pytest.raises(ValueError, match="acquisition"):
        replace(second, kind="acquisition").canonical_bytes()


def test_invalid_lineages():
    first, second = chain()
    for stages in (
        [],
        [first, first],
        [first, replace(second, manifest=replace(second.manifest, run_id="other"))],
    ):
        with pytest.raises(ValueError):
            verify_lineage(stages)
    for modified in (
        replace(second, software=None),
        replace(second, parents=()),
        replace(second, inputs=()),
        replace(second, parents=second.parents * 2),
        replace(second, inputs=second.inputs * 2),
    ):
        with pytest.raises(ValueError):
            modified.canonical_bytes()


def test_multi_parent_ordering_and_exact_heads():
    first, second = chain()
    alternate = replace(
        second,
        manifest=replace(
            second.manifest,
            stage_id="alternative",
            files=(
                FileDigest.from_stream("alternate.parquet", io.BytesIO(b"alternative")),
            ),
        ),
    )
    merged = Stage(
        Manifest(
            "run-1",
            "merged",
            (FileDigest.from_stream("merged.parquet", io.BytesIO(b"merged")),),
        ),
        "processing",
        (second.digest(), alternate.digest()),
        tuple(
            InputReference(
                s.digest(), s.manifest.files[0].path, s.manifest.files[0].sha256
            )
            for s in [second, alternate]
        ),
        second.software,
    )
    assert (
        replace(
            merged,
            parents=tuple(reversed(merged.parents)),
            inputs=tuple(reversed(merged.inputs)),
        ).canonical_bytes()
        == merged.canonical_bytes()
    )
    assert (
        verify_lineage(
            [merged, alternate, second, first], [merged.digest()]
        ).stage_count
        == 4
    )
    with pytest.raises(ValueError, match="checkpoint"):
        verify_lineage([first, second, alternate], [second.digest()])
