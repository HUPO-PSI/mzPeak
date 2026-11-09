"""Real CMS, PKI and timestamp qualification for vendor-attested conversion."""

from dataclasses import replace
import json
from pathlib import Path
import zipfile

from mzpeak.embedded import ProvenanceBundle, embed_provenance, read_embedded_provenance
from mzpeak.evidence import EvidenceBundle, TsaPolicy
from mzpeak.historical import verify_historical_history
from mzpeak.signing import SignatureEnvelope, SignaturePolicy
from mzpeak.stages import Stage
from mzpeak.trust import Authorization, CheckStatus, SignerRole, TrustPolicy
from mzpeak.verification import (
    ConversionPolicy,
    ReleaseStatement,
    SignedRelease,
    SignedStage,
    verify_archive_history,
    verify_history,
)

DATA = Path(__file__).resolve().parents[2] / "test/data/provenance/conversion"


def read(name):
    return json.loads((DATA / name).read_bytes())


def fixture():
    records = [
        SignedStage(
            Stage.from_dict(r["stage"]),
            SignatureEnvelope.from_json(json.dumps(r["envelope"]).encode()),
        )
        for r in read("records.json")
    ]
    releases = {
        key: SignedRelease(
            ReleaseStatement(**r["statement"]),
            SignatureEnvelope.from_json(json.dumps(r["envelope"]).encode()),
        )
        for key, r in read("releases.json").items()
    }
    trust = read("trust.json")
    trust["authorizations"] = [
        Authorization(**(a | {"role": SignerRole(a["role"])}))
        for a in trust["authorizations"]
    ]
    trust = TrustPolicy(**trust)
    files = {key: bytes.fromhex(data) for key, data in read("files.json").items()}
    kwargs = dict(
        releases=releases,
        publishers=read("publishers.json"),
        expected_heads=read("heads.json"),
        conversion_policy=ConversionPolicy.from_dict(read("conversion-policy.json")),
    )
    return records, files, trust, kwargs


def check(records, files, trust, kwargs):
    return verify_history(
        records, files, trust, SignaturePolicy.ECDSA, read("identities.json"), **kwargs
    )


def test_real_conversion_root_then_processing_keeps_claims_separate():
    records, files, trust, kwargs = fixture()
    report = check(records, files, trust, kwargs)
    assert report.accepted, report
    assert not report.lineage.acquisitions
    assert report.lineage.conversions == (records[0].stage.digest(),)
    conversion = report.stages[0]
    assert conversion.kind == "conversion"
    assert conversion.signature == CheckStatus.VALID
    assert conversion.conversion.vendor_attestation == CheckStatus.VALID
    assert conversion.conversion.source_integrity == CheckStatus.UNKNOWN
    assert conversion.conversion.claimed_acquisition_time < min(
        c.validation_time for c in conversion.certificates
    )
    assert report.stages[1].kind == "processing" and report.stages[1].conversion is None


def test_external_raw_source_matching_mismatch_missing_and_required(tmp_path):
    records, files, trust, kwargs = fixture()
    root = tmp_path / "original.raw.d"
    root.mkdir()
    for name, encoded in read("raw-files.json").items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(bytes.fromhex(encoded))
    policy = kwargs["conversion_policy"]
    stage_id = records[0].stage.manifest.stage_id
    kwargs["conversion_policy"] = replace(policy, require_source_bytes=True)
    assert not check(records, files, trust, kwargs).accepted
    kwargs["conversion_policy"] = replace(
        policy, require_source_bytes=True, sources={stage_id: root}
    )
    report = check(records, files, trust, kwargs)
    assert (
        report.accepted
        and report.stages[0].conversion.source_integrity == CheckStatus.VALID
    )
    name = next(iter(read("raw-files.json")))
    (root / name).write_bytes(b"changed raw bytes")
    report = check(records, files, trust, kwargs)
    assert (
        not report.accepted
        and report.stages[0].conversion.source_integrity == CheckStatus.INVALID
    )
    kwargs["conversion_policy"] = replace(policy, sources={stage_id: root / "missing"})
    assert not check(records, files, trust, kwargs).accepted


def test_conversion_does_not_borrow_processing_or_publisher_authority():
    records, files, trust, kwargs = fixture()
    grant = next(
        a for a in trust.authorizations if a.role == SignerRole.VENDOR_CONVERSION
    )
    for role in (
        SignerRole.PROCESSING,
        SignerRole.SOFTWARE_RELEASE,
        SignerRole.ACQUISITION,
    ):
        changed = [
            replace(a, role=role) if a == grant else a for a in trust.authorizations
        ]
        report = check(records, files, replace(trust, authorizations=changed), kwargs)
        assert not report.accepted
        assert report.stages[0].signature == CheckStatus.VALID
        assert all(
            c.authorization == CheckStatus.INVALID
            for c in report.stages[0].certificates
        )
    kwargs.pop("conversion_policy")
    report = check(records, files, trust, kwargs)
    assert (
        not report.accepted
        and report.stages[0].conversion.vendor_attestation == CheckStatus.INVALID
    )


def test_tampered_signed_source_method_report_and_output_are_rejected():
    records, files, trust, kwargs = fixture()
    first = records[0]
    claim = first.stage.conversion
    changed_source = replace(
        claim.source,
        files=(replace(claim.source.files[0], sha256="f" * 64),)
        + claim.source.files[1:],
    )
    # Replacing any signed vendor assertion invalidates the actual CMS proof.
    variants = [
        replace(
            claim,
            source=changed_source,
            verification=replace(
                claim.verification, source_sha256=changed_source.digest()
            ),
        ),
        replace(claim, claimed_acquisition_time=claim.claimed_acquisition_time + 1),
        replace(claim, verification=replace(claim.verification, method="other-method")),
        replace(claim, verification=replace(claim.verification, result="failed")),
    ]
    for changed in variants:
        stage = replace(first.stage, conversion=changed)
        # Keep only the conversion root so changed digest does not merely fail
        # the child's parent binding before the signature is actually checked.
        selected = [replace(first, stage=stage)]
        selected_files = {f.path: files[f.path] for f in stage.manifest.files}
        selected_kwargs = kwargs | {"expected_heads": [stage.digest()]}
        report = check(selected, selected_files, trust, selected_kwargs)
        assert not report.accepted and report.stages[0].signature == CheckStatus.INVALID
    for name in records[0].stage.manifest.files:
        changed = files | {name.path: files[name.path] + b"tampered"}
        assert not check(records, changed, trust, kwargs).accepted
    report = check(records, files, trust, kwargs | {"releases": {}})
    assert (
        not report.accepted and report.stages[0].software_release == CheckStatus.INVALID
    )


def test_vendor_method_policy_is_external_and_exact():
    records, files, trust, kwargs = fixture()
    policy = kwargs["conversion_policy"]
    for field in ("vendor", "format", "method", "verifier_name", "verifier_version"):
        grant = replace(policy.allowed_verifications[0], **{field: "different"})
        report = check(
            records,
            files,
            trust,
            kwargs
            | {"conversion_policy": replace(policy, allowed_verifications=(grant,))},
        )
        assert (
            not report.accepted
            and report.stages[0].conversion.vendor_attestation == CheckStatus.INVALID
        )


def test_conversion_embedding_and_archive_coverage_roundtrip(tmp_path):
    records, files, trust, kwargs = fixture()
    archive = tmp_path / "converted.mzpeak"
    with zipfile.ZipFile(archive, "w") as writer:
        for name, content in files.items():
            writer.writestr(name, content)
    bundle = ProvenanceBundle(records, dict(kwargs["releases"]))
    embed_provenance(archive, bundle)
    loaded = read_embedded_provenance(archive)
    assert loaded.to_dict() == bundle.to_dict()
    assert verify_archive_history(
        archive,
        loaded.records,
        trust,
        SignaturePolicy.ECDSA,
        read("identities.json"),
        **kwargs,
    ).accepted
    with zipfile.ZipFile(archive, "a") as writer:
        writer.writestr("unsigned.bin", b"uncovered")
    assert not verify_archive_history(
        archive,
        loaded.records,
        trust,
        SignaturePolicy.ECDSA,
        read("identities.json"),
        **kwargs,
    ).accepted


def test_real_historical_conversion_uses_attestation_time_and_explicit_role():
    records, files, trust, kwargs = fixture()
    metadata = read("metadata.json")
    trust = replace(trust, validation_time=metadata["historical_validation_time"])
    assert not check(records, files, trust, kwargs).accepted
    historical = dict(
        tsa_policy=TsaPolicy(**read("tsa-policy.json")),
        policy_id=metadata["policy_id"],
        policy_bytes=(DATA / "policy.json").read_bytes(),
        stage_evidence={
            key: EvidenceBundle(**e) for key, e in read("stage-evidence.json").items()
        },
        release_evidence={
            key: EvidenceBundle(**e) for key, e in read("release-evidence.json").items()
        },
    )
    report = verify_historical_history(
        records,
        files,
        trust,
        SignaturePolicy.ECDSA,
        read("identities.json"),
        **kwargs,
        **historical,
    )
    assert report.accepted, report
    assert (
        report.history.stages[0].conversion.claimed_acquisition_time
        == metadata["claimed_acquisition_time"]
    )
    for certificate in report.history.stages[0].certificates:
        assert (
            metadata["claimed_acquisition_time"]
            < certificate.validation_time
            < trust.validation_time
        )
    grant = next(
        a for a in trust.authorizations if a.role == SignerRole.VENDOR_CONVERSION
    )
    bad = replace(
        trust,
        authorizations=[
            replace(a, role=SignerRole.PROCESSING) if a == grant else a
            for a in trust.authorizations
        ],
    )
    assert not verify_historical_history(
        records,
        files,
        bad,
        SignaturePolicy.ECDSA,
        read("identities.json"),
        **kwargs,
        **historical,
    ).accepted
