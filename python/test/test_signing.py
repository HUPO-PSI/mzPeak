"""Real OpenSSL signatures and shared Rust/Python interoperability vectors."""

from dataclasses import replace
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from mzpeak.signing import (
    Algorithm,
    OpenSslBackend,
    Pkcs11Key,
    SignatureEnvelope,
    SignaturePolicy,
    SigningError,
    SigningIdentity,
    SoftwareKey,
    signing_payload,
)


FIXTURES = (
    Path(__file__).resolve().parents[2] / "test" / "data" / "provenance" / "signing"
)
BACKEND = OpenSslBackend()


def run(*args):
    result = subprocess.run(
        ["openssl", *map(str, args)], capture_output=True, check=True
    )
    return result.stdout


@pytest.fixture(scope="module")
def identities(tmp_path_factory):
    if shutil.which("openssl") is None:
        if os.environ.get("MZPEAK_REQUIRE_OPENSSL_PQC") == "1":
            pytest.fail("PQC qualification requires the OpenSSL backend")
        pytest.skip("OpenSSL backend unavailable")
    root = tmp_path_factory.mktemp("signatures")
    result = []
    for algorithm, keygen in [
        (Algorithm.ECDSA_P256_SHA256, ["EC", "-pkeyopt", "ec_paramgen_curve:P-256"]),
        (Algorithm.ML_DSA_65, ["ML-DSA-65"]),
    ]:
        key = root / (algorithm.value + ".pem")
        certificate = root / (algorithm.value + ".crt")
        try:
            run("genpkey", "-algorithm", *keygen, "-out", key)
        except subprocess.CalledProcessError:
            if algorithm == Algorithm.ML_DSA_65:
                if os.environ.get("MZPEAK_REQUIRE_OPENSSL_PQC") == "1":
                    pytest.fail("PQC qualification requires OpenSSL 3.5+ ML-DSA")
                pytest.skip("ML-DSA backend requires OpenSSL 3.5+")
            raise
        run(
            "req",
            "-new",
            "-x509",
            "-key",
            key,
            "-out",
            certificate,
            "-days",
            "1",
            "-subj",
            "/CN=mzPeak signing test",
        )
        result.append(
            SigningIdentity(algorithm, SoftwareKey(key), certificate.read_text())
        )
    return tuple(result)


def test_shared_transcript_and_both_signatures(identities):
    manifest = (FIXTURES / "manifest.json").read_bytes()
    wire = (FIXTURES / "envelope.json").read_bytes()
    envelope = SignatureEnvelope.from_json(wire)
    assert envelope.to_json() == wire
    assert (
        hashlib.sha256(signing_payload(manifest, envelope)).hexdigest()
        == (FIXTURES / "transcript.sha256").read_text().strip()
    )
    assert (
        hashlib.sha256(envelope.canonical_bytes()).hexdigest()
        == (FIXTURES / "canonical-envelope.sha256").read_text().strip()
    )
    signers = BACKEND.verify(manifest, envelope, SignaturePolicy.BOTH)
    assert [s.algorithm for s in signers] == [
        Algorithm.ECDSA_P256_SHA256,
        Algorithm.ML_DSA_65,
    ]
    # Self-signed fixture leaves are valid crypto evidence, not trusted identities.
    assert all(len(s.certificate_sha256) == 64 for s in signers)


@pytest.mark.parametrize(
    "policy,selected",
    [
        (SignaturePolicy.ECDSA, [0]),
        (SignaturePolicy.PQC, [1]),
        (SignaturePolicy.BOTH, [0, 1]),
    ],
)
def test_real_round_trip_and_changed_manifest(identities, policy, selected):
    manifest = b"canonical manifest bytes"
    envelope = BACKEND.sign(manifest, [identities[i] for i in selected], policy)
    assert len(BACKEND.verify(manifest, envelope, policy)) == len(selected)
    with pytest.raises(SigningError):
        BACKEND.verify(manifest + b"tampered", envelope, policy)


def test_stripping_and_policy_downgrade_fail(identities):
    manifest = b"bound dual signer plan"
    envelope = BACKEND.sign(manifest, identities, SignaturePolicy.BOTH)
    stripped = replace(envelope, signatures=envelope.signatures[:1])
    with pytest.raises(SigningError):
        BACKEND.verify(manifest, stripped, SignaturePolicy.ECDSA)
    with pytest.raises(SigningError):
        BACKEND.verify(
            manifest,
            replace(stripped, policy=SignaturePolicy.ECDSA),
            SignaturePolicy.ECDSA,
        )
    with pytest.raises(SigningError):
        BACKEND.verify(
            manifest,
            replace(envelope, policy=SignaturePolicy.ECDSA),
            SignaturePolicy.ECDSA,
        )


def test_policy_enforced_and_bad_signature_not_ignored(identities):
    manifest = b"policy test"
    classical = BACKEND.sign(manifest, identities[:1], SignaturePolicy.ECDSA)
    with pytest.raises(SigningError):
        BACKEND.verify(manifest, classical, SignaturePolicy.BOTH)
    envelope = BACKEND.sign(manifest, identities, SignaturePolicy.BOTH)
    bad = replace(envelope.signatures[1], cms_der_hex="00" * 3309)
    with pytest.raises(SigningError):
        BACKEND.verify(
            manifest,
            replace(envelope, signatures=(envelope.signatures[0], bad)),
            SignaturePolicy.ECDSA,
        )


def test_key_certificate_mismatch_and_wrong_curve(identities, tmp_path):
    wrong = replace(identities[0], key=identities[1].key)
    with pytest.raises(SigningError):
        BACKEND.sign(b"mismatch", [wrong], SignaturePolicy.ECDSA)
    key = tmp_path / "p384.key"
    cert = tmp_path / "p384.crt"
    run(
        "genpkey",
        "-algorithm",
        "EC",
        "-pkeyopt",
        "ec_paramgen_curve:P-384",
        "-out",
        key,
    )
    run(
        "req",
        "-new",
        "-x509",
        "-key",
        key,
        "-out",
        cert,
        "-days",
        "1",
        "-subj",
        "/CN=wrong curve",
    )
    with pytest.raises(SigningError):
        BACKEND.sign(
            b"wrong curve",
            [
                SigningIdentity(
                    Algorithm.ECDSA_P256_SHA256, SoftwareKey(key), cert.read_text()
                )
            ],
            SignaturePolicy.ECDSA,
        )


def test_invalid_envelope_profiles(identities):
    envelope = BACKEND.sign(b"validation", identities[:1], SignaturePolicy.ECDSA)
    for changed in [
        replace(envelope, version=1),
        replace(envelope, signatures=()),
        replace(envelope, signatures=envelope.signatures * 2),
        replace(
            envelope,
            signatures=(
                replace(envelope.signatures[0], algorithm=Algorithm.ML_DSA_65),
            ),
        ),
        replace(
            envelope,
            signatures=(replace(envelope.signatures[0], cms_der_hex="not-hex"),),
        ),
    ]:
        with pytest.raises(SigningError):
            BACKEND.verify(b"validation", changed, SignaturePolicy.ECDSA)
    with pytest.raises(SigningError):
        SignatureEnvelope.from_json(
            b'{"version":1,"version":1,"policy":"ecdsa","signatures":[]}'
        )
    with pytest.raises(SigningError):
        SignatureEnvelope.from_json(envelope.to_json()[:-1] + b',"extension":true}')
    obj = json.loads(envelope.to_json())
    obj["version"] = True
    with pytest.raises(SigningError):
        SignatureEnvelope.from_json(json.dumps(obj).encode())


def test_pkcs11_rejects_inline_pin_before_process(identities):
    identity = replace(
        identities[0],
        key=Pkcs11Key("pkcs11:object=test?pin-value=secret", Path("missing.cnf")),
    )
    with pytest.raises(SigningError, match="without an inline PIN"):
        OpenSslBackend(executable="does-not-exist").sign(
            b"no secret exposure", [identity], SignaturePolicy.ECDSA
        )


def test_backend_missing_and_pem_replacement(identities):
    with pytest.raises(SigningError, match="unavailable"):
        OpenSslBackend(executable="does-not-exist").sign(
            b"backend", identities[:1], SignaturePolicy.ECDSA
        )
    envelope = BACKEND.sign(b"leaf binding", identities[:1], SignaturePolicy.ECDSA)
    changed = replace(
        envelope.signatures[0], certificate_pem=identities[1].certificate_pem
    )
    with pytest.raises(SigningError):
        BACKEND.verify(
            b"leaf binding",
            replace(envelope, signatures=(changed,)),
            SignaturePolicy.ECDSA,
        )


def test_non_der_certificate_is_rejected_before_crypto():
    from asn1crypto import pem

    envelope = SignatureEnvelope.from_bytes((FIXTURES / "envelope.json").read_bytes())
    for signature in envelope.signatures:
        _, _, der = pem.unarmor(signature.certificate_pem.encode())
        assert der[1] & 0x80
        content = der[2 + (der[1] & 0x7F) :]
        ber = pem.armor("CERTIFICATE", b"\x30\x80" + content + b"\0\0").decode()
        changed = replace(signature, certificate_pem=ber)
        candidate = replace(
            envelope,
            signatures=tuple(
                changed if entry.algorithm == signature.algorithm else entry
                for entry in envelope.signatures
            ),
        )
        with pytest.raises(SigningError):
            signing_payload(b"DER-only", candidate)


def test_complete_pqc_certificate_and_crl_path(identities):
    from mzpeak.trust import (
        Authorization,
        CheckStatus,
        SignerRole,
        TrustPolicy,
        certificate_fingerprint,
        verify_certificate,
    )

    fixtures = FIXTURES / "pqc-trust"
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


def test_pkcs11_software_token_integration_when_configured():
    """Opt-in provider test; CI can use SoftHSM, hardware runs use Nitrokey."""
    import os

    config = os.environ.get("MZPEAK_TEST_PKCS11_CONFIG")
    key_uri = os.environ.get("MZPEAK_TEST_PKCS11_URI")
    certificate = os.environ.get("MZPEAK_TEST_PKCS11_CERTIFICATE")
    if not all((config, key_uri, certificate)):
        pytest.skip("PKCS#11 config, URI and certificate not supplied")
    identity = SigningIdentity(
        Algorithm.ECDSA_P256_SHA256,
        Pkcs11Key(key_uri, Path(config)),
        Path(certificate).read_text(),
    )
    envelope = BACKEND.sign(b"PKCS11 qualification", [identity], SignaturePolicy.ECDSA)
    assert (
        len(BACKEND.verify(b"PKCS11 qualification", envelope, SignaturePolicy.ECDSA))
        == 1
    )


@pytest.mark.parametrize(
    "name,value", json.loads((FIXTURES / "cms-invalid.json").read_text()).items()
)
def test_invalid_cms_is_rejected(name, value):
    envelope = SignatureEnvelope.from_json((FIXTURES / "envelope.json").read_bytes())
    changed = replace(
        envelope,
        signatures=(
            replace(envelope.signatures[0], cms_der_hex=value),
            *envelope.signatures[1:],
        ),
    )
    with pytest.raises(SigningError):
        BACKEND.verify(
            (FIXTURES / "manifest.json").read_bytes(), changed, SignaturePolicy.ECDSA
        )


def test_cms_interoperates_with_standard_openssl(identities, tmp_path):
    envelope = SignatureEnvelope.from_json((FIXTURES / "envelope.json").read_bytes())
    for stem, signature in zip(("ecdsa", "pqc"), envelope.signatures):
        assert (
            FIXTURES / (stem + ".cms.der")
        ).read_bytes().hex() == signature.cms_der_hex
        # Independent standard CMS verification. This does not assert certificate trust.
        output = run(
            "cms",
            "-verify",
            "-binary",
            "-inform",
            "DER",
            "-in",
            FIXTURES / (stem + ".cms.der"),
            "-content",
            FIXTURES / "cms-content.bin",
            "-noverify",
        )
        assert output == (FIXTURES / "cms-content.bin").read_bytes()


def test_old_raw_envelope_is_not_silently_accepted(identities):
    envelope = BACKEND.sign(b"migration", identities[:1], SignaturePolicy.ECDSA)
    obj = json.loads(envelope.to_json())
    obj["version"] = 1
    obj["signatures"][0]["signature_hex"] = obj["signatures"][0].pop("cms_der_hex")
    with pytest.raises(SigningError):
        SignatureEnvelope.from_json(json.dumps(obj).encode())


@pytest.mark.parametrize(
    "declared,indices",
    [
        (SignaturePolicy.ECDSA, [0]),
        (SignaturePolicy.PQC, [1]),
        (SignaturePolicy.BOTH, [0, 1]),
    ],
)
@pytest.mark.parametrize("required", list(SignaturePolicy))
def test_cms_relying_party_policy_matrix(identities, declared, indices, required):
    envelope = BACKEND.sign(
        b"policy matrix", [identities[i] for i in indices], declared
    )
    if declared == SignaturePolicy.BOTH or declared == required:
        assert BACKEND.verify(b"policy matrix", envelope, required)
    else:
        with pytest.raises(SigningError):
            BACKEND.verify(b"policy matrix", envelope, required)
