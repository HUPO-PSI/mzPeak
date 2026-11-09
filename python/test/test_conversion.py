"""Canonical external sources, strict stage revisions and stable capture."""

from dataclasses import replace
import hashlib
import io
import json
from pathlib import Path

import pytest

from mzpeak import conversion
from mzpeak.conversion import (
    ConversionClaim,
    ReportReference,
    SourceFile,
    SourceManifest,
    VendorVerification,
    capture_source,
    verify_source,
)
from mzpeak.manifest import FileDigest, Manifest
from mzpeak.stages import InputReference, SoftwareIdentity, Stage, verify_lineage


def model():
    source = SourceManifest(
        "vendor-raw-v1",
        (SourceFile("sample.raw", 3, hashlib.sha256(b"raw").hexdigest()),),
    )
    verification = VendorVerification(
        "vendor-1", "integrity-v1", "Verifier", "1.0", source.digest(), "passed", None
    )
    claim = ConversionClaim(source, verification, 946684800)
    software = SoftwareIdentity("Converter", "2.0", "a" * 64, "b" * 64)
    stage = Stage(
        Manifest(
            "run-1",
            "conversion",
            (FileDigest.from_stream("data.bin", io.BytesIO(b"data")),),
        ),
        "conversion",
        software=software,
        version=2,
        conversion=claim,
    )
    return source, claim, stage


def test_source_capture_file_directory_and_complete_inventory(tmp_path):
    root = tmp_path / "sample.d"
    root.mkdir()
    (root / "nested").mkdir()
    (root / "nested" / "b.bin").write_bytes(b"second")
    (root / "a.bin").write_bytes(b"first")
    source = capture_source(root, "vendor-raw-v1")
    assert [f.path for f in source.files] == ["a.bin", "nested/b.bin"]
    assert SourceManifest.from_bytes(source.canonical_bytes()) == source
    assert verify_source(root, source).complete
    single = capture_source(root / "a.bin", "vendor-raw-v1")
    assert single.files[0].path == "a.bin"
    (root / "a.bin").write_bytes(b"changed")
    (root / "nested" / "b.bin").unlink()
    (root / "extra.bin").write_bytes(b"extra")
    checked = verify_source(root, source)
    assert checked.changed == ("a.bin",)
    assert checked.missing == ("nested/b.bin",)
    assert checked.uncovered == ("extra.bin",)
    (root / "a.bin").unlink()
    (root / "extra.bin").unlink()
    assert verify_source(root, source).missing == ("a.bin", "nested/b.bin")
    with pytest.raises(ValueError, match="contain files"):
        capture_source(root, "vendor-raw-v1")


def test_source_canonical_order_and_source_hash_binding():
    source, claim, _ = model()
    other = SourceFile("other.bin", 1, "c" * 64)
    assert (
        replace(source, files=source.files + (other,)).canonical_bytes()
        == replace(source, files=(other,) + source.files).canonical_bytes()
    )
    assert ConversionClaim.from_bytes(claim.canonical_bytes()) == claim
    with pytest.raises(ValueError, match="differs from source"):
        replace(
            claim, verification=replace(claim.verification, source_sha256="f" * 64)
        ).validate()


@pytest.mark.parametrize(
    "file",
    [
        SourceFile("../bad", 0, "a" * 64),
        SourceFile("/absolute", 0, "a" * 64),
        SourceFile("bad\\path", 0, "a" * 64),
        SourceFile("bad", True, "a" * 64),
    ],
)
def test_unsafe_source_descriptors_rejected(file):
    with pytest.raises(ValueError):
        SourceManifest("vendor-raw-v1", (file,)).validate()


def test_source_rejects_case_aliases_symlinks_and_unstable_capture(
    tmp_path, monkeypatch
):
    root = tmp_path / "raw"
    root.mkdir()
    path = root / "sample.raw"
    path.write_bytes(b"original")
    source = SourceManifest(
        "raw", (SourceFile("a", 0, "a" * 64), SourceFile("A", 0, "a" * 64))
    )
    with pytest.raises(ValueError, match="case-colliding"):
        source.validate()
    original_hash = conversion.sha256_stream

    def changed(stream, length):
        result = original_hash(stream, length)
        path.write_bytes(b"modified")
        return result

    monkeypatch.setattr(conversion, "sha256_stream", changed)
    with pytest.raises(ValueError, match="changed"):
        capture_source(root, "raw")
    monkeypatch.setattr(conversion, "sha256_stream", original_hash)
    link = root / "alias.raw"
    try:
        link.symlink_to(path)
    except OSError:
        return  # Windows CI may lack symbolic-link creation privileges.
    with pytest.raises(ValueError, match="symlink"):
        capture_source(root, "raw")
    with pytest.raises(ValueError, match="symlink"):
        capture_source(link, "raw")


def test_windows_reparse_points_are_rejected_without_following(tmp_path, monkeypatch):
    path = tmp_path / "junction"
    path.mkdir()
    original = Path.lstat

    class Reparse:
        def __init__(self, info):
            self.st_mode = info.st_mode
            self.st_file_attributes = 0x400

    monkeypatch.setattr(
        Path, "lstat", lambda p: Reparse(original(p)) if p == path else original(p)
    )
    with pytest.raises(ValueError, match="symlink"):
        capture_source(path, "raw")


def test_v1_wire_bytes_preserved_and_v2_requires_conversion_fields():
    fixtures = Path(__file__).resolve().parents[2] / "test/data/provenance"
    for name in ("acquisition-v1.json", "processing-v1.json"):
        original = (fixtures / name).read_bytes()
        assert Stage.from_bytes(original).canonical_bytes() == original
        value = json.loads(original)
        value["conversion"] = None
        with pytest.raises(ValueError):
            Stage.from_dict(value)
    _, claim, stage = model()
    assert Stage.from_bytes(stage.canonical_bytes()) == stage
    value = stage.to_dict()
    for replacement in (
        {k: v for k, v in value.items() if k != "conversion"},
        value | {"conversion": None},
        value | {"kind": "processing"},
        value | {"version": True},
        value | {"version": 3},
    ):
        with pytest.raises(ValueError):
            Stage.from_dict(replacement)
    with pytest.raises(ValueError):
        Stage.from_bytes(
            stage.canonical_bytes().replace(b'"version":2', b'"version":2,"version":2')
        )
    for time in (True, -1, 1.5):
        with pytest.raises(ValueError):
            replace(claim, claimed_acquisition_time=time).validate()


def test_conversion_root_can_parent_processing_but_not_mask_missing_history():
    _, _, root = model()
    child = Stage(
        Manifest(
            "run-1",
            "processing",
            (FileDigest.from_stream("processed.bin", io.BytesIO(b"processed")),),
        ),
        "processing",
        (root.digest(),),
        (InputReference(root.digest(), "data.bin", root.manifest.files[0].sha256),),
        root.software,
    )
    report = verify_lineage([child, root], [child.digest()])
    assert report.acquisitions == ()
    assert report.conversions == (root.digest(),)
    assert report.heads == (child.digest(),)
    with pytest.raises(ValueError, match="missing parent"):
        verify_lineage([child])
    with pytest.raises(ValueError, match="checkpoint"):
        verify_lineage([root], [child.digest()])
    with pytest.raises(ValueError, match="overwrites"):
        verify_lineage(
            [
                root,
                replace(
                    child,
                    manifest=replace(
                        child.manifest,
                        files=(replace(root.manifest.files[0], sha256="f" * 64),),
                    ),
                ),
            ]
        )


def test_validation_report_must_be_bound_to_own_output_manifest():
    _, claim, stage = model()
    claim = replace(
        claim,
        verification=replace(
            claim.verification, report=ReportReference("report.json", "c" * 64)
        ),
    )
    with pytest.raises(ValueError, match="not covered"):
        replace(stage, conversion=claim).validate()
    stage = replace(
        stage,
        conversion=claim,
        manifest=replace(
            stage.manifest,
            files=stage.manifest.files + (FileDigest("report.json", 12, "c" * 64),),
        ),
    )
    stage.validate()
    with pytest.raises(ValueError, match="not covered"):
        replace(
            stage,
            conversion=replace(
                claim,
                verification=replace(
                    claim.verification, report=ReportReference("report.json", "d" * 64)
                ),
            ),
        ).validate()


def test_policy_strict_types_origin_method_result_and_optional_source(tmp_path):
    from mzpeak.trust import CheckStatus
    from mzpeak.verification import (
        ConversionPolicy,
        VendorVerificationGrant,
        _check_conversion,
    )

    source, claim, stage = model()
    grant = VendorVerificationGrant(
        "vendor-1", source.format, "integrity-v1", "Verifier", "1.0"
    )
    policy = ConversionPolicy((grant,))
    denied = _check_conversion(stage, "vendor-1", None)
    assert not denied.accepted
    report = _check_conversion(stage, "vendor-1", policy)
    assert report.accepted and report.source_integrity == CheckStatus.UNKNOWN
    assert not _check_conversion(stage, "other-vendor", policy).accepted
    assert not _check_conversion(
        stage, "vendor-1", replace(policy, allowed_verifications=())
    ).accepted
    for result in ("failed", "unknown"):
        changed = replace(
            stage,
            conversion=replace(
                claim, verification=replace(claim.verification, result=result)
            ),
        )
        assert not _check_conversion(changed, "vendor-1", policy).accepted
    assert not _check_conversion(
        stage, "vendor-1", replace(policy, require_source_bytes=True)
    ).accepted
    path = tmp_path / "sample.raw"
    path.write_bytes(b"raw")
    supplied = replace(policy, sources={"conversion": path})
    assert (
        _check_conversion(stage, "vendor-1", supplied).source_integrity
        == CheckStatus.VALID
    )
    path.write_bytes(b"tampered")
    assert not _check_conversion(stage, "vendor-1", supplied).accepted
    path.unlink()
    failed = _check_conversion(stage, "vendor-1", supplied)
    assert failed.source_integrity == CheckStatus.INVALID and not failed.accepted
    value = {
        "allowed_verifications": [grant.__dict__],
        "require_source_bytes": False,
        "sources": {},
    }
    assert ConversionPolicy.from_dict(value) == policy
    for invalid in (
        value | {"require_source_bytes": 1},
        value | {"sources": {"conversion": 1}},
        value | {"unknown": True},
    ):
        with pytest.raises(ValueError):
            ConversionPolicy.from_dict(invalid)
    with pytest.raises(ValueError, match="duplicate"):
        ConversionPolicy((grant, grant)).validate()
    wire = json.dumps(value)
    with pytest.raises(ValueError, match="duplicate"):
        ConversionPolicy.from_bytes(
            wire.replace('"sources": {}', '"sources": {}, "sources": {}')
        )
