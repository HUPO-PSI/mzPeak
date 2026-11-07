"""Shared RFC 3161 fixtures exercise the actual OpenSSL TSA verifier."""

from dataclasses import replace
import json
from pathlib import Path

from asn1crypto import tsp
import pytest
from mzpeak.evidence import (
    TsaPolicy,
    EvidenceBundle,
    create_timestamp_query,
    verify_timestamp,
    verify_historical_signer,
)
from mzpeak.trust import (
    CheckStatus,
    TrustPolicy,
    Authorization,
    SignerRole,
    certificate_fingerprint,
    verify_certificate,
)

DATA = Path(__file__).resolve().parents[2] / "test/data/provenance/timestamps"


def raw(name):
    return (DATA / name).read_bytes()


def pem(name):
    return raw(name).decode("ascii")


def metadata():
    return json.loads(raw("metadata.json"))


def policy():
    info = metadata()
    return TsaPolicy(
        [pem("tsa-root.pem")],
        info["validation_time"],
        [info["tsa_policy_oid"]],
        [pem("tsa-root.crl.pem")],
    )


def verify(p=None, envelope=None, response=None, query=None):
    return verify_timestamp(
        envelope if envelope is not None else raw("envelope.json"),
        response if response is not None else raw("response.tsr"),
        query if query is not None else raw("request.tsq"),
        p or policy(),
    )


def test_genuine_timestamp_uses_historical_tsa_validity():
    report = verify()
    assert report.accepted, report.details
    assert report.trusted_time == metadata()["generated_at"]
    assert report.trusted_time < policy().validation_time - 86400
    assert verify_timestamp(
        raw("envelope.json"), raw("response.tsr"), None, policy()
    ).accepted


@pytest.mark.parametrize(
    "change",
    [
        {"roots_pem": []},
        {"roots_pem": [pem("historical-signer.pem")]},
        {"allowed_policy_oids": ["1.2.3.4"]},
        {"crls_pem": [pem("stale.crl.pem")]},
        {"crls_pem": [pem("tsa-root.crl.pem") + pem("tsa-root.pem")]},
        {"validation_time": metadata()["generated_at"] - 1},
    ],
)
def test_invalid_tsa_evidence_never_supplies_trusted_time(change):
    report = verify(replace(policy(), **change))
    assert report.status == CheckStatus.INVALID
    assert report.trusted_time is None


def test_missing_revocation_and_absent_timestamp_are_unknown():
    assert verify(replace(policy(), crls_pem=[])).status == CheckStatus.UNKNOWN
    result = verify_timestamp(raw("envelope.json"), None, None, policy())
    assert result.status == CheckStatus.UNKNOWN and result.trusted_time is None


def test_wrong_imprint_and_tampered_signature_rejected():
    assert not verify(envelope=raw("envelope.json") + b" ").accepted
    response = bytearray(raw("response.tsr"))
    response[-1] ^= 1
    assert not verify(response=bytes(response)).accepted
    for malformed in [b"", b"garbage", b"\x30\x00"]:
        assert verify(response=malformed).trusted_time is None


def test_nonce_mismatch_or_removal_rejected():
    for nonce in [123, None]:
        query = tsp.TimeStampReq.load(raw("request.tsq"))
        query["nonce"] = nonce
        report = verify(query=query.dump())
        assert not report.accepted and report.trusted_time is None


def test_query_generation_uses_sha256_nonce_and_full_envelope():
    query = tsp.TimeStampReq.load(
        create_timestamp_query(
            raw("envelope.json"), policy_oid=metadata()["tsa_policy_oid"]
        )
    )
    from hashlib import sha256

    assert query["nonce"].native is not None
    assert query["message_imprint"]["hash_algorithm"]["algorithm"].native == "sha256"
    assert (
        query["message_imprint"]["hashed_message"].native
        == sha256(raw("envelope.json")).digest()
    )


def bundle():
    return EvidenceBundle.create(
        b"manifest",
        raw("envelope.json"),
        "demo-policy-v1",
        b"local policy",
        timestamp_response_hex=raw("response.tsr").hex(),
        timestamp_query_hex=raw("request.tsq").hex(),
        tsa_crls_pem=[pem("tsa-root.crl.pem")],
        signer_crls_pem=[pem("tsa-root.crl.pem")],
    )


def test_evidence_roundtrip_and_substitution_rejected():
    evidence = EvidenceBundle.from_json(bundle().to_json())
    assert evidence == bundle()
    evidence.check_binding(
        b"manifest", raw("envelope.json"), "demo-policy-v1", b"local policy"
    )
    for args in [
        (b"changed", raw("envelope.json"), "demo-policy-v1", b"local policy"),
        (b"manifest", b"changed", "demo-policy-v1", b"local policy"),
        (b"manifest", raw("envelope.json"), "other-policy", b"local policy"),
        (b"manifest", raw("envelope.json"), "demo-policy-v1", b"changed"),
    ]:
        with pytest.raises(ValueError):
            evidence.check_binding(*args)
    with pytest.raises(ValueError):
        EvidenceBundle.from_json(b'{"version":1,"version":1}')


def test_expired_signer_is_eligible_at_verified_time_only():
    leaf = pem("historical-signer.pem")
    signer = TrustPolicy(
        [pem("tsa-root.pem")],
        [
            Authorization(
                certificate_fingerprint(leaf), SignerRole.ACQUISITION, "instrument-1"
            )
        ],
        metadata()["validation_time"],
        [pem("tsa-root.crl.pem")],
    )
    assert not verify_certificate(
        leaf, [], signer, SignerRole.ACQUISITION, "instrument-1"
    ).accepted
    stamp, cert = verify_historical_signer(
        bundle(),
        b"manifest",
        raw("envelope.json"),
        "demo-policy-v1",
        b"local policy",
        policy(),
        signer,
        leaf,
        SignerRole.ACQUISITION,
        "instrument-1",
    )
    assert stamp.accepted and cert.accepted
    # Archived certificate material cannot install a new role grant.
    _, cert = verify_historical_signer(
        bundle(),
        b"manifest",
        raw("envelope.json"),
        "demo-policy-v1",
        b"local policy",
        policy(),
        replace(signer, authorizations=[]),
        leaf,
        SignerRole.ACQUISITION,
        "instrument-1",
    )
    assert not cert.accepted
    stamp, cert = verify_historical_signer(
        replace(bundle(), timestamp_response_hex=None),
        b"manifest",
        raw("envelope.json"),
        "demo-policy-v1",
        b"local policy",
        policy(),
        signer,
        leaf,
        SignerRole.ACQUISITION,
        "instrument-1",
    )
    assert stamp.status == CheckStatus.UNKNOWN and cert is None
