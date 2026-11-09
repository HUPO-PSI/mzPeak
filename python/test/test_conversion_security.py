"""Caller-supplied RAW paths must never be silently ignored."""

from dataclasses import replace
import zipfile

import pytest

from mzpeak.evidence import EvidenceBundle, TsaPolicy
from mzpeak.historical import (
    verify_historical_archive_history,
    verify_historical_history,
)
from mzpeak.signing import SignaturePolicy
from mzpeak.verification import (
    VendorVerificationGrant,
    verify_archive_history,
    verify_history,
)
from test_vendor_conversion import DATA, fixture, read


def verify(tmp_path, records, files, trust, kwargs, *, streaming, historical):
    if historical:
        trust = replace(
            trust, validation_time=read("metadata.json")["historical_validation_time"]
        )
        kwargs = kwargs | dict(
            tsa_policy=TsaPolicy(**read("tsa-policy.json")),
            policy_id=read("metadata.json")["policy_id"],
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
    if streaming:
        path = tmp_path / "converted.mzpeak"
        with zipfile.ZipFile(path, "w") as archive:
            for name, content in files.items():
                archive.writestr(name, content)
        verifier = (
            verify_historical_archive_history if historical else verify_archive_history
        )
        return verifier(
            path, records, trust, SignaturePolicy.ECDSA, read("identities.json"), **kwargs
        )
    verifier = verify_historical_history if historical else verify_history
    return verifier(
        records, files, trust, SignaturePolicy.ECDSA, read("identities.json"), **kwargs
    )


@pytest.mark.parametrize("identifier", ["misspelled", "processing"])
@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("historical", [False, True])
def test_unknown_or_nonconversion_source_target_is_not_ignored(
    tmp_path, identifier, streaming, historical
):
    records, files, trust, kwargs = fixture()
    policy = kwargs["conversion_policy"]
    # The caller explicitly requested a RAW check. A typo or a processing-stage
    # ID must not downgrade that request to an optional source being absent.
    kwargs["conversion_policy"] = replace(
        policy,
        require_source_bytes=False,
        sources={identifier: tmp_path / "not-supplied.raw"},
    )
    with pytest.raises(ValueError):
        verify(
            tmp_path,
            records,
            files,
            trust,
            kwargs,
            streaming=streaming,
            historical=historical,
        )


@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("historical", [False, True])
def test_empty_history_fails_without_discarding_source_policy(
    tmp_path, streaming, historical
):
    _, _, trust, kwargs = fixture()
    kwargs["conversion_policy"] = replace(
        kwargs["conversion_policy"], sources={"conversion": tmp_path / "raw"}
    )
    with pytest.raises(ValueError):
        verify(
            tmp_path,
            [],
            {},
            trust,
            kwargs,
            streaming=streaming,
            historical=historical,
        )


def test_unrelated_verification_grant_remains_reusable_policy(tmp_path):
    records, files, trust, kwargs = fixture()
    policy = kwargs["conversion_policy"]
    kwargs["conversion_policy"] = replace(
        policy,
        allowed_verifications=policy.allowed_verifications
        + (VendorVerificationGrant("other-vendor", "other.raw", "other-method", "Verifier", "9"),),
    )
    assert verify(
        tmp_path, records, files, trust, kwargs, streaming=False, historical=False
    ).accepted
