from pathlib import Path
from dataclasses import replace
import json
import zipfile
import pytest
from mzpeak.stages import Stage
from mzpeak.signing import SignatureEnvelope, SignaturePolicy
from mzpeak.trust import TrustPolicy, Authorization, SignerRole
from mzpeak.verification import (
    SignedStage,
    SignedRelease,
    ReleaseStatement,
    verify_history,
    verify_archive_history,
)

P = Path(__file__).resolve().parents[2] / "test/data/provenance/history"


def fixture(path=P):
    records = [
        SignedStage(
            Stage.from_dict(r["stage"]),
            SignatureEnvelope.from_json(json.dumps(r["envelope"]).encode()),
        )
        for r in json.loads((path / "records.json").read_text())
    ]
    releases = {
        k: SignedRelease(
            ReleaseStatement(**v["statement"]),
            SignatureEnvelope.from_json(json.dumps(v["envelope"]).encode()),
        )
        for k, v in json.loads((path / "releases.json").read_text()).items()
    }
    obj = json.loads((path / "trust.json").read_text())
    obj["authorizations"] = [
        Authorization(**{**a, "role": SignerRole(a["role"])})
        for a in obj["authorizations"]
    ]
    trust = TrustPolicy(**obj)
    files = {
        k: bytes.fromhex(v)
        for k, v in json.loads((path / "files.json").read_text()).items()
    }
    heads = json.loads((path / "heads.json").read_text())
    kwargs = dict(
        releases=releases,
        publishers={k: "publisher-1" for k in releases},
        expected_heads=heads,
    )
    return records, files, trust, kwargs


def check(records, files, trust, kwargs):
    return verify_history(
        records,
        files,
        trust,
        SignaturePolicy.ECDSA,
        {"acquisition": "instrument-1", "processing": "installation-1"},
        **kwargs,
    )


def test_certified_acquisition_processing_and_release():
    records, files, trust, kwargs = fixture()
    report = check(records, files, trust, kwargs)
    assert report.accepted
    assert report.lineage.checkpoint_checked
    assert len(report.stages) == 2


def test_content_changed_missing_uncovered():
    records, files, trust, kwargs = fixture()
    for name in list(files):
        modified = dict(files)
        modified[name] += b"tampered"
        assert not check(records, modified, trust, kwargs).accepted
    modified = dict(files)
    modified.pop("raw.bin")
    modified["extra.bin"] = b"extra"
    report = check(records, modified, trust, kwargs)
    assert not report.accepted
    assert "raw.bin" in report.coverage.missing
    assert "extra.bin" in report.coverage.uncovered


def test_publisher_approval_is_separate_from_execution():
    records, files, trust, kwargs = fixture()
    kwargs["releases"] = {}
    report = check(records, files, trust, kwargs)
    assert report.stages[0].accepted
    assert not report.stages[1].accepted


def test_stage_signature_cannot_be_removed_or_transferred():
    records, files, trust, kwargs = fixture()
    records[0] = replace(records[0], envelope=records[1].envelope)
    assert not check(records, files, trust, kwargs).accepted


def test_no_implicit_roles_or_revocation():
    records, files, trust, kwargs = fixture()
    assert not check(records, files, replace(trust, authorizations=[]), kwargs).accepted
    assert not check(records, files, replace(trust, crls_pem=[]), kwargs).accepted


def test_external_head_catches_removal_of_processing():
    records, files, trust, kwargs = fixture()
    with pytest.raises(ValueError):
        check(records[:1], files, trust, kwargs)


def test_streaming_zip_verification(tmp_path):
    records, files, trust, kwargs = fixture()
    path = tmp_path / "signed.mzpeak"
    with zipfile.ZipFile(path, "w") as z:
        for name, data in files.items():
            z.writestr(name, data)
        z.writestr("provenance/stages.json", (P / "records.json").read_bytes())
    report = verify_archive_history(
        path,
        records,
        trust,
        SignaturePolicy.ECDSA,
        {"acquisition": "instrument-1", "processing": "installation-1"},
        **kwargs,
    )
    assert report.accepted


def test_repeated_file_cannot_replace_previously_signed_chunk_descriptors():
    from mzpeak.manifest import ChunkDigest

    records, files, trust, kwargs = fixture()
    original = records[0].stage.manifest.files[0]
    repeated = replace(original, chunks=(ChunkDigest(0, 1, "0" * 64),))
    later = records[1].stage
    later = replace(
        later,
        manifest=replace(later.manifest, files=later.manifest.files + (repeated,)),
    )
    records[1] = replace(records[1], stage=later)
    kwargs.pop("expected_heads")
    # Fail before signature evaluation: a union must never silently choose one
    # stage's chunk claims and discard a different stage's signed descriptors.
    with pytest.raises(ValueError, match="inconsistent repeated file descriptor"):
        check(records, files, trust, kwargs)


def test_reordered_repeated_chunks_preserve_signed_history_and_checkpoint(tmp_path):
    records, files, trust, kwargs = fixture(P / "chunk-order")
    assert check(records, files, trust, kwargs).accepted
    original_stage = records[1].stage
    repeated = next(f for f in original_stage.manifest.files if f.path == "raw.bin")
    assert len(repeated.chunks) == 2
    reordered = replace(repeated, chunks=tuple(reversed(repeated.chunks)))
    manifest = replace(
        original_stage.manifest,
        files=tuple(
            reordered if f.path == repeated.path else f
            for f in original_stage.manifest.files
        ),
    )
    records[1] = replace(records[1], stage=replace(original_stage, manifest=manifest))
    assert records[1].stage.canonical_bytes() == original_stage.canonical_bytes()
    assert records[1].stage.digest() == kwargs["expected_heads"][0]
    report = check(records, files, trust, kwargs)
    assert report.accepted
    assert report.lineage.checkpoint_checked

    archive = tmp_path / "reordered.mzpeak"
    with zipfile.ZipFile(archive, "w") as writer:
        for name, data in files.items():
            writer.writestr(name, data)
    archived = verify_archive_history(
        archive,
        records,
        trust,
        SignaturePolicy.ECDSA,
        {"acquisition": "instrument-1", "processing": "installation-1"},
        **kwargs,
    )
    assert archived.accepted
    assert archived.lineage.checkpoint_checked


@pytest.mark.parametrize(
    "changed_fields", [{"sha256": "0" * 64}, {"column_path": "different.column"}]
)
def test_repeated_chunk_claim_changes_remain_inconsistent(changed_fields):
    records, files, trust, kwargs = fixture(P / "chunk-order")
    later = records[1].stage
    repeated = next(f for f in later.manifest.files if f.path == "raw.bin")
    changed = replace(
        repeated,
        chunks=(replace(repeated.chunks[0], **changed_fields), *repeated.chunks[1:]),
    )
    records[1] = replace(
        records[1],
        stage=replace(
            later,
            manifest=replace(
                later.manifest,
                files=tuple(
                    changed if f.path == repeated.path else f
                    for f in later.manifest.files
                ),
            ),
        ),
    )
    assert records[1].stage.digest() != kwargs["expected_heads"][0]
    kwargs.pop("expected_heads")
    # The union must reject conflicting signed claims, even when no external
    # checkpoint is supplied, rather than silently choosing one descriptor.
    with pytest.raises(ValueError, match="inconsistent repeated file descriptor"):
        check(records, files, trust, kwargs)
