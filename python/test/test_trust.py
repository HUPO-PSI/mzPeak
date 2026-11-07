from dataclasses import replace
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
    assert not check(p=replace(policy(),authorizations=[])).accepted
    assert not check(p=policy(role=SignerRole.PROCESSING)).accepted
    assert not verify_certificate(pem('leaf.pem'), [pem('issuer.pem')], policy(), SignerRole.ACQUISITION, 'other').accepted

def test_software_release_requires_code_signing_eku():
    assert not check(role=SignerRole.SOFTWARE_RELEASE).accepted
    assert check('release.pem',role=SignerRole.SOFTWARE_RELEASE).accepted

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
    assert not denied.accepted
    assert any('issuer extended key usage excludes' in detail for detail in denied.details)
    assert not verify('eku-issuer-code.pem', replace(p, roots_pem=[pem('eku-root-server.pem')])).accepted
