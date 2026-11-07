from dataclasses import replace
import json
from pathlib import Path
import zipfile

import pytest
from mzpeak.evidence import EvidenceBundle, TsaPolicy
from mzpeak.historical import (
    verify_historical_history,
    verify_historical_archive_history,
)
from mzpeak.signing import SignatureEnvelope, SignaturePolicy
from mzpeak.stages import Stage
from mzpeak.trust import Authorization, SignerRole, TrustPolicy, CheckStatus
from mzpeak.verification import (
    ReleaseStatement,
    SignedRelease,
    SignedStage,
    verify_history,
)

DATA = Path(__file__).resolve().parents[2] / "test/data/provenance/timestamps/history"
IDENTITIES = {"acquisition": "instrument-1", "processing": "installation-1"}


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
            ReleaseStatement(**value["statement"]),
            SignatureEnvelope.from_json(json.dumps(value["envelope"]).encode()),
        )
        for key, value in read("releases.json").items()
    }
    trust = read("trust.json")
    trust["authorizations"] = [
        Authorization(**(value | {"role": SignerRole(value["role"])}))
        for value in trust["authorizations"]
    ]
    trust = TrustPolicy(**trust)
    files = {key: bytes.fromhex(value) for key, value in read("files.json").items()}
    kwargs = dict(
        releases=releases,
        publishers={key: "publisher-1" for key in releases},
        expected_heads=read("heads.json"),
        tsa_policy=TsaPolicy(**read("tsa-policy.json")),
        policy_id="historical-demo-v1",
        policy_bytes=(DATA / "policy.json").read_bytes(),
        stage_evidence={
            key: EvidenceBundle(**value)
            for key, value in read("stage-evidence.json").items()
        },
        release_evidence={
            key: EvidenceBundle(**value)
            for key, value in read("release-evidence.json").items()
        },
    )
    return records, files, trust, kwargs


def check(records, files, trust, kwargs):
    return verify_historical_history(
        records, files, trust, SignaturePolicy.ECDSA, IDENTITIES, **kwargs
    )


def test_real_signed_history_accepted_after_leaf_expiry_only_with_timestamps():
    records, files, trust, kwargs = fixture()
    current = {key: kwargs[key] for key in ["releases", "publishers", "expected_heads"]}
    assert not verify_history(
        records, files, trust, SignaturePolicy.ECDSA, IDENTITIES, **current
    ).accepted
    report = check(records, files, trust, kwargs)
    assert report.accepted, report
    assert report.history.lineage.checkpoint_checked
    assert len(report.stage_timestamps) == 2 and len(report.release_timestamps) == 1
    assert all(
        c.validation_time < trust.validation_time
        for s in report.history.stages
        for c in s.certificates
    )


def test_historical_timestamps_cannot_be_transferred_between_stages():
    records, files, trust, kwargs = fixture()
    kwargs["stage_evidence"]["acquisition"] = kwargs["stage_evidence"]["processing"]
    report = check(records, files, trust, kwargs)
    assert not report.accepted
    assert report.stage_timestamps["acquisition"].status == CheckStatus.INVALID
    assert report.history.stages[0].signature == CheckStatus.VALID
    assert not report.history.stages[0].certificates


def test_missing_stage_and_independent_release_proof_never_fall_back():
    records, files, trust, kwargs = fixture()
    kwargs["release_evidence"] = {}
    report = check(records, files, trust, kwargs)
    assert not report.accepted
    assert report.history.stages[0].accepted
    assert next(iter(report.release_timestamps.values())).status == CheckStatus.UNKNOWN
    assert report.history.stages[1].software_release == CheckStatus.INVALID
    # The processing timestamp cannot serve as proof of publisher approval.
    key = next(iter(kwargs["releases"]))
    kwargs["release_evidence"][key] = kwargs["stage_evidence"]["processing"]
    assert not check(records, files, trust, kwargs).accepted
    kwargs["stage_evidence"].pop("acquisition")
    report = check(records, files, trust, kwargs)
    assert report.stage_timestamps["acquisition"].status == CheckStatus.UNKNOWN
    assert not report.accepted


def test_timestamps_never_override_signature_coverage_or_external_grants():
    records, files, trust, kwargs = fixture()
    changed = dict(files)
    changed["raw.bin"] += b"tampered"
    assert not check(records, changed, trust, kwargs).accepted
    modified = list(records)
    modified[0] = replace(records[0], envelope=records[1].envelope)
    report = check(modified, files, trust, kwargs)
    assert (
        not report.accepted
        and report.history.stages[0].signature == CheckStatus.INVALID
    )
    assert not check(records, files, replace(trust, authorizations=[]), kwargs).accepted
    kwargs["tsa_policy"] = replace(kwargs["tsa_policy"], roots_pem=trust.roots_pem)
    assert not check(records, files, trust, kwargs).accepted


def test_publisher_statement_must_match_exact_software_release():
    records, files, trust, kwargs = fixture()
    key = next(iter(kwargs["releases"]))
    release = kwargs["releases"][key]
    kwargs["releases"][key] = replace(
        release, statement=replace(release.statement, version="other-version")
    )
    assert not check(records, files, trust, kwargs).accepted


def test_historical_streaming_zip_and_head_checkpoint(tmp_path):
    records, files, trust, kwargs = fixture()
    archive = tmp_path / "signed.mzpeak"
    with zipfile.ZipFile(archive, "w") as writer:
        for name, content in files.items():
            writer.writestr(name, content)
    report = verify_historical_archive_history(
        archive, records, trust, SignaturePolicy.ECDSA, IDENTITIES, **kwargs
    )
    assert report.accepted
    with pytest.raises(ValueError):
        check(records[:1], files, trust, kwargs)
