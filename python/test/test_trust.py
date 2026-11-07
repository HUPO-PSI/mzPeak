from dataclasses import replace
import json
from pathlib import Path
import pytest
from mzpeak.trust import (Authorization, CheckStatus, SignerRole, TrustPolicy,
                          certificate_fingerprint, verify_certificate)
FIXTURES = Path(__file__).resolve().parents[2] / 'test/data/provenance/trust'

def pem(name):
    return (FIXTURES / name).read_text()

def policy(leaf='leaf.pem', role=SignerRole.ACQUISITION):
    return TrustPolicy([pem('root.pem')], [Authorization(certificate_fingerprint(pem(leaf)), role, 'instrument-1')],
                       1893456000, [pem('root.crl.pem'), pem('issuer.crl.pem')])

def check(leaf='leaf.pem', p=None, role=SignerRole.ACQUISITION):
    return verify_certificate(pem(leaf), [pem('issuer.pem')], p or policy(leaf,role), role, 'instrument-1')

def test_valid_path_and_full_revocation_evidence():
    assert check().accepted

def test_embedded_root_cannot_establish_trust():
    assert not check(p=replace(policy(), roots_pem=[pem('other_root.pem')])).accepted
    assert not check(p=replace(policy(), roots_pem=[])).accepted

def test_unknown_revocation_is_not_certified():
    r=check(p=replace(policy(),crls_pem=[]))
    assert r.path == CheckStatus.VALID
    assert r.revocation == CheckStatus.UNKNOWN
    assert not r.accepted

@pytest.mark.parametrize('leaf', ['revoked.pem','expired.pem','bad_usage.pem','issuer.pem'])
def test_ineligible_certificates(leaf):
    assert not check(leaf).accepted

def test_stale_or_incomplete_crls_rejected():
    assert not check(p=replace(policy(),validation_time=1893628800)).accepted
    assert not check(p=replace(policy(),crls_pem=[pem('root.crl.pem')])).accepted

def test_role_identity_and_grants_must_be_local():
    reports = [
        check(p=replace(policy(),authorizations=[])),
        check(p=policy(role=SignerRole.PROCESSING)),
        verify_certificate(pem('leaf.pem'), [pem('issuer.pem')], policy(), SignerRole.ACQUISITION, 'other'),
    ]
    for denied in reports:
        assert (denied.path, denied.authorization, denied.revocation) == (
            CheckStatus.VALID, CheckStatus.INVALID, CheckStatus.VALID)
        assert not denied.accepted

def test_software_release_requires_code_signing_eku():
    denied = check(role=SignerRole.SOFTWARE_RELEASE)
    assert (denied.path, denied.authorization, denied.revocation) == (
        CheckStatus.VALID, CheckStatus.INVALID, CheckStatus.VALID)
    assert not denied.accepted
    assert check('release.pem',role=SignerRole.SOFTWARE_RELEASE).accepted


@pytest.mark.parametrize('required_eku', ['1.3.6.1.5.5.7.3.1', 'not-an-oid'])
def test_rejected_leaf_eku_does_not_hide_valid_path_or_revocation(required_eku):
    configured = policy('release.pem', SignerRole.SOFTWARE_RELEASE)
    configured = replace(configured, authorizations=[
        replace(configured.authorizations[0], required_eku=required_eku)])
    denied = check('release.pem', configured, SignerRole.SOFTWARE_RELEASE)
    assert (denied.path, denied.authorization, denied.revocation) == (
        CheckStatus.VALID, CheckStatus.INVALID, CheckStatus.VALID)
    assert not denied.accepted


@pytest.mark.parametrize('crls,expected', [([], CheckStatus.UNKNOWN), (None, CheckStatus.INVALID)])
def test_authorization_failure_does_not_hide_revocation_result(crls, expected):
    configured = policy('revoked.pem', SignerRole.SOFTWARE_RELEASE)
    if crls is not None:
        configured = replace(configured, crls_pem=crls)
    denied = check('revoked.pem', configured, SignerRole.SOFTWARE_RELEASE)
    assert (denied.path, denied.authorization, denied.revocation) == (
        CheckStatus.VALID, CheckStatus.INVALID, expected)
    assert not denied.accepted

def test_pqc_signature_does_not_imply_pqc_identity_path():
    assert not check(p=replace(policy(),require_pqc_chain=True)).accepted

def test_malformed_certificate_and_bundled_leaf():
    for malformed in ['not a cert',pem('leaf.pem')+pem('root.pem')]:
        assert not verify_certificate(malformed,[],policy(),SignerRole.ACQUISITION,'instrument-1').accepted

def test_missing_issuer():
    assert not verify_certificate(pem('leaf.pem'),[],policy(),SignerRole.ACQUISITION,'instrument-1').accepted


def test_issuer_and_anchor_eku_constrain_software_release():
    p = TrustPolicy([pem('eku-root.pem')],
                    [Authorization(certificate_fingerprint(pem('eku-leaf.pem')),
                                   SignerRole.SOFTWARE_RELEASE, 'publisher-1')],
                    1893456000, [pem('eku-root.crl.pem'), pem('eku-issuer.crl.pem')])
    def verify(issuer, configured=p):
        return verify_certificate(pem('eku-leaf.pem'), [pem(issuer)], configured,
                                  SignerRole.SOFTWARE_RELEASE, 'publisher-1')
    assert verify('eku-issuer-code.pem').accepted
    assert verify('eku-issuer-any.pem').accepted
    denied = verify('eku-issuer-server.pem')
    assert (denied.path, denied.authorization, denied.revocation) == (
        CheckStatus.VALID, CheckStatus.INVALID, CheckStatus.VALID)
    assert not denied.accepted
    assert any('issuer extended key usage excludes' in detail for detail in denied.details)
    denied = verify('eku-issuer-code.pem', replace(p, roots_pem=[pem('eku-root-server.pem')]))
    assert (denied.path, denied.authorization, denied.revocation) == (
        CheckStatus.VALID, CheckStatus.INVALID, CheckStatus.VALID)
    assert not denied.accepted


def _require_pqc_backend():
    import os
    import subprocess

    try:
        checked = subprocess.run(
            ["openssl", "list", "-signature-algorithms"], capture_output=True, timeout=30
        )
        available = checked.returncode == 0 and b"ML-DSA-65" in checked.stdout
    except OSError:
        available = False
    if not available:
        if os.environ.get("MZPEAK_REQUIRE_OPENSSL_PQC") == "1":
            pytest.fail("PQC qualification requires OpenSSL 3.5+ ML-DSA")
        pytest.skip("ML-DSA backend requires OpenSSL 3.5+")


def test_complete_pqc_certificate_and_crl_path():
    _require_pqc_backend()
    from mzpeak.trust import (
        Authorization,
        CheckStatus,
        SignerRole,
        TrustPolicy,
        certificate_fingerprint,
        verify_certificate,
    )

    fixtures = FIXTURES.parent / "signing" / "pqc-trust"
    validation_time = json.loads((fixtures / "validation.json").read_text())[
        "validation_time"
    ]

    def pem(name):
        return (fixtures / name).read_text()

    def policy(root, issuer, leaf, strict):
        return TrustPolicy(
            [pem(root + ".pem")],
            [
                Authorization(
                    certificate_fingerprint(pem(leaf + ".pem")),
                    SignerRole.ACQUISITION,
                    "pqc-instrument-1",
                )
            ],
            validation_time,
            [pem(root + ".crl.pem"), pem(issuer + ".crl.pem")],
            require_pqc_chain=strict,
        )

    strict = policy("root", "issuer", "leaf", True)
    result = verify_certificate(
        pem("leaf.pem"),
        [pem("issuer.pem")],
        strict,
        SignerRole.ACQUISITION,
        "pqc-instrument-1",
    )
    assert result.accepted, result
    assert (result.path, result.authorization, result.revocation) == (
        CheckStatus.VALID,
        CheckStatus.VALID,
        CheckStatus.VALID,
    )
    absent = verify_certificate(
        pem("leaf.pem"),
        [pem("issuer.pem")],
        replace(strict, crls_pem=[]),
        SignerRole.ACQUISITION,
        "pqc-instrument-1",
    )
    assert not absent.accepted and absent.revocation == CheckStatus.UNKNOWN

    # Both mixed chains are cryptographically valid and accepted by the ordinary
    # policy. Strict PQC policy must additionally reject their classical links.
    for root, issuer, leaf in [
        ("classical-root", "mixed-issuer", "mixed-leaf"),
        ("root", "classical-issuer", "classical-issuer-leaf"),
    ]:
        configured = policy(root, issuer, leaf, False)
        ordinary = verify_certificate(
            pem(leaf + ".pem"),
            [pem(issuer + ".pem")],
            configured,
            SignerRole.ACQUISITION,
            "pqc-instrument-1",
        )
        assert ordinary.accepted, ordinary
        rejected = verify_certificate(
            pem(leaf + ".pem"),
            [pem(issuer + ".pem")],
            replace(configured, require_pqc_chain=True),
            SignerRole.ACQUISITION,
            "pqc-instrument-1",
        )
        assert not rejected.accepted
        assert any(
            "PQC identity policy rejects" in detail for detail in rejected.details
        )
