"""Experimental v2 CMS signatures of canonical manifests using OpenSSL.

Each signer produces detached DER CMS SignedData with signed attributes.
ML-DSA-65 uses SHA-512 for the content digest per RFC 9882; ECDSA uses
P-256/SHA-256. OpenSSL 3.5 or newer is needed for ML-DSA. ``verify`` establishes
cryptographic integrity only: pass each returned signer to the trust validator.

The signed binary transcript binds the manifest, the declared policy, and every
signer's algorithm and certificate hash. Removing a signature therefore does
not turn a dual-signature envelope into a valid classical-only envelope.
"""

from dataclasses import dataclass
from enum import Enum
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile
from typing import Sequence

from asn1crypto import algos, cms, core, pem, x509


DOMAIN = b"mzPeak-cms-signature-v2\0"
MAX_CERTIFICATE_BYTES = 64 * 1024


class SigningError(ValueError):
    """Malformed input, unsupported profile, or a failed crypto operation."""


class _SubjectPublicKeyInfo(core.Sequence):
    # asn1crypto's legacy PublicKeyInfo decoder dispatches through a closed map
    # of known keys. A schema with opaque BIT STRING supports standardized new
    # key OIDs without modifying its process-global algorithm registry.
    _fields = [
        ("algorithm", algos.AlgorithmIdentifier),
        ("public_key", core.OctetBitString),
    ]


class _TbsCertificate(x509.TbsCertificate):
    _fields = [
        (field[0], _SubjectPublicKeyInfo, *field[2:])
        if field[0] == "subject_public_key_info"
        else field
        for field in x509.TbsCertificate._fields
    ]


class _Certificate(core.Sequence):
    _fields = [
        ("tbs_certificate", _TbsCertificate),
        ("signature_algorithm", algos.SignedDigestAlgorithm),
        ("signature_value", core.OctetBitString),
    ]


class _CMSCertificateChoices(cms.CertificateChoices):
    _alternatives = [
        (item[0], _Certificate, *item[2:]) if item[0] == "certificate" else item
        for item in cms.CertificateChoices._alternatives
    ]


class _CMSCertificateSet(core.SetOf):
    _child_spec = _CMSCertificateChoices


class _CMSSignedData(cms.SignedData):
    _fields = [
        (item[0], _CMSCertificateSet, *item[2:]) if item[0] == "certificates" else item
        for item in cms.SignedData._fields
    ]


class _CMSContentInfo(core.Sequence):
    # Local schema avoids changing asn1crypto's global algorithm registries.
    _fields = [
        ("content_type", cms.ContentType),
        ("content", _CMSSignedData, {"explicit": 0}),
    ]


class Algorithm(str, Enum):
    ECDSA_P256_SHA256 = "ecdsa-p256-sha256"
    ML_DSA_65 = "ml-dsa-65"


class SignaturePolicy(str, Enum):
    ECDSA = "ecdsa"
    PQC = "pqc"
    BOTH = "both"


@dataclass(frozen=True)
class SoftwareKey:
    key_path: Path
    passphrase_file: Path | None = None


@dataclass(frozen=True)
class Pkcs11Key:
    """Opaque key URI and trusted OpenSSL pkcs11-provider configuration.

    Configure OpenSC and a protected PIN file in ``config_path``. Inline PINs
    are rejected so they cannot enter process listings or error messages.
    The HSM private key is never exported.
    """

    key_uri: str
    config_path: Path


@dataclass(frozen=True)
class SigningIdentity:
    algorithm: Algorithm
    key: SoftwareKey | Pkcs11Key
    certificate_pem: str
    intermediates_pem: Sequence[str] = ()


@dataclass(frozen=True)
class DetachedSignature:
    algorithm: Algorithm
    certificate_pem: str
    intermediates_pem: tuple[str, ...]
    cms_der_hex: str


@dataclass(frozen=True)
class SignatureEnvelope:
    version: int
    policy: SignaturePolicy
    signatures: tuple[DetachedSignature, ...]

    def canonical_bytes(self) -> bytes:
        """Sorted-key ASCII JSON, including actual signatures, for timestamps."""
        raw = self.to_json()
        if not raw.isascii():
            raise SigningError("Envelope fields must be ASCII")
        return json.dumps(
            json.loads(raw), sort_keys=True, ensure_ascii=True, separators=(",", ":")
        ).encode("ascii")

    @classmethod
    def from_bytes(cls, data: bytes) -> "SignatureEnvelope":
        return cls.from_json(data)

    def to_json(self) -> bytes:
        return json.dumps(
            {
                "version": self.version,
                "policy": self.policy.value,
                "signatures": [
                    {
                        "algorithm": s.algorithm.value,
                        "certificate_pem": s.certificate_pem,
                        "intermediates_pem": list(s.intermediates_pem),
                        "cms_der_hex": s.cms_der_hex,
                    }
                    for s in self.signatures
                ],
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ).encode("utf-8")

    @classmethod
    def from_json(cls, data: bytes) -> "SignatureEnvelope":
        def object_pairs(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise SigningError("Duplicate JSON field")
                result[key] = value
            return result

        try:
            obj = json.loads(data, object_pairs_hook=object_pairs)
            if set(obj) != {"version", "policy", "signatures"}:
                raise SigningError("Unknown or missing envelope field")
            if type(obj["version"]) is not int or obj["version"] != 2:
                raise SigningError("Unsupported envelope version")
            if not isinstance(obj["signatures"], list):
                raise SigningError("Signatures must be an array")
            signatures = []
            for value in obj["signatures"]:
                if set(value) != {
                    "algorithm",
                    "certificate_pem",
                    "intermediates_pem",
                    "cms_der_hex",
                }:
                    raise SigningError("Unknown or missing signature field")
                if not isinstance(value["intermediates_pem"], list):
                    raise SigningError("Intermediates must be an array")
                signatures.append(
                    DetachedSignature(
                        Algorithm(value["algorithm"]),
                        value["certificate_pem"],
                        tuple(value["intermediates_pem"]),
                        value["cms_der_hex"],
                    )
                )
            return cls(
                obj["version"], SignaturePolicy(obj["policy"]), tuple(signatures)
            )
        except (ValueError, TypeError, KeyError, UnicodeError) as error:
            raise SigningError("Invalid signature envelope") from error


@dataclass(frozen=True)
class CryptographicSigner:
    """Valid signature evidence; **not** a trusted or authorized identity."""

    algorithm: Algorithm
    certificate_pem: str
    intermediates_pem: tuple[str, ...]
    certificate_sha256: str


def _certificate_der(certificate_pem: str) -> bytes:
    if not isinstance(certificate_pem, str):
        raise SigningError("Certificate must be PEM text")
    try:
        encoded = certificate_pem.encode("ascii")
        stripped = encoded.strip()
        if (
            len(encoded) > MAX_CERTIFICATE_BYTES
            or not stripped.startswith(b"-----BEGIN CERTIFICATE-----")
            or not stripped.endswith(b"-----END CERTIFICATE-----")
            or encoded.count(b"-----BEGIN") != 1
            or encoded.count(b"-----END") != 1
        ):
            raise SigningError("Expected exactly one PEM certificate")
        kind, _, der = pem.unarmor(encoded)
        if kind != "CERTIFICATE":
            raise SigningError("Expected a certificate")
        certificate = _Certificate.load(der, strict=True)
        if certificate.dump(force=True) != der:
            raise SigningError("Certificate is not canonical DER")
        return der
    except (ValueError, TypeError, UnicodeError) as error:
        raise SigningError("Invalid PEM certificate") from error


def _check_certificate(certificate_pem: str, algorithm: Algorithm) -> bytes:
    der = _certificate_der(certificate_pem)
    try:
        spki = x509.Certificate.load(der, strict=True)["tbs_certificate"][
            "subject_public_key_info"
        ]
        identifier = _SubjectPublicKeyInfo.load(spki.dump(), strict=True)["algorithm"]
        oid = identifier["algorithm"].dotted
        params = identifier["parameters"]
        if algorithm == Algorithm.ECDSA_P256_SHA256:
            if (
                oid != "1.2.840.10045.2.1"
                or core.ObjectIdentifier.load(params.dump(), strict=True).dotted
                != "1.2.840.10045.3.1.7"
            ):
                raise SigningError("Certificate key is not named-curve P-256")
        elif algorithm == Algorithm.ML_DSA_65:
            if oid != "2.16.840.1.101.3.4.3.18" or params.dump() != b"":
                raise SigningError(
                    "Certificate key is not ML-DSA-65 with absent parameters"
                )
        else:
            raise SigningError("Unsupported signature algorithm")
    except (ValueError, TypeError, KeyError) as error:
        raise SigningError("Invalid certificate key algorithm") from error
    return der


def _satisfies(algorithms: set[Algorithm], policy: SignaturePolicy) -> bool:
    if policy == SignaturePolicy.ECDSA:
        return Algorithm.ECDSA_P256_SHA256 in algorithms
    if policy == SignaturePolicy.PQC:
        return Algorithm.ML_DSA_65 in algorithms
    if policy == SignaturePolicy.BOTH:
        return {Algorithm.ECDSA_P256_SHA256, Algorithm.ML_DSA_65} <= algorithms
    raise SigningError("Unsupported signature policy")


def _signer_key(algorithm, certificate_pem):
    if not isinstance(algorithm, Algorithm):
        raise SigningError("Unsupported signature algorithm")
    return algorithm.value, hashlib.sha256(
        _check_certificate(certificate_pem, algorithm)
    ).digest()


def signing_payload(manifest: bytes, envelope: SignatureEnvelope) -> bytes:
    """Return the CMS v2 content, binding the manifest and complete signer plan."""
    if not isinstance(manifest, bytes):
        raise SigningError("Manifest must be bytes")
    if type(envelope.version) is not int or envelope.version != 2:
        raise SigningError("Unsupported envelope version")
    if not isinstance(envelope.policy, SignaturePolicy):
        raise SigningError("Unsupported signature policy")
    if not 1 <= len(envelope.signatures) <= 0xFFFFFFFF:
        raise SigningError("Expected a nonempty signer set representable as u32")
    algorithms = [s.algorithm for s in envelope.signatures]
    if any(not isinstance(a, Algorithm) for a in algorithms):
        raise SigningError("Unsupported signature algorithm")
    keys = [_signer_key(s.algorithm, s.certificate_pem) for s in envelope.signatures]
    if keys != sorted(set(keys)):
        raise SigningError(
            "Signers must be unique and sorted by algorithm and certificate digest"
        )
    if not _satisfies(set(algorithms), envelope.policy):
        raise SigningError("Signatures do not satisfy the declared policy")
    output = bytearray(DOMAIN)
    output += bytes(
        [
            {SignaturePolicy.ECDSA: 1, SignaturePolicy.PQC: 2, SignaturePolicy.BOTH: 3}[
                envelope.policy
            ]
        ]
    )
    output += struct.pack(">I", len(algorithms))
    for signature, (_, certificate_hash) in zip(envelope.signatures, keys):
        algorithm = signature.algorithm.value.encode("ascii")
        if len(signature.intermediates_pem) > 8:
            raise SigningError("Too many intermediate certificates")
        for certificate in signature.intermediates_pem:
            _certificate_der(certificate)
        output += struct.pack(">I", len(algorithm)) + algorithm
        output += certificate_hash
    output += struct.pack(">Q", len(manifest)) + manifest
    return bytes(output)


MAX_CMS_BYTES = 1024 * 1024


def _cms_der(signature: DetachedSignature) -> bytes:
    """Enforce our bounded CMS profile before invoking the crypto backend."""
    try:
        value = signature.cms_der_hex
        if not isinstance(value, str) or len(value) > 2 * MAX_CMS_BYTES:
            raise ValueError("CMS exceeds profile limit")
        raw = bytes.fromhex(value)
        if raw.hex() != value:
            raise ValueError("CMS requires lowercase hex")
        info = _CMSContentInfo.load(raw, strict=True)
        if info["content_type"].native != "signed_data" or info.dump(force=True) != raw:
            raise ValueError("Expected canonical DER SignedData")
        data = info["content"]
        content = data["encap_content_info"]
        if (
            data["version"].native != "v1"
            or content["content_type"].native != "data"
            or content["content"].native is not None
            or data["crls"].native is not None
            or len(data["signer_infos"]) != 1
        ):
            raise ValueError("Expected detached single-signer CMS without CRLs")
        signer = data["signer_infos"][0]
        leaf_der = _check_certificate(signature.certificate_pem, signature.algorithm)
        leaf = _Certificate.load(leaf_der)
        sid = signer["sid"]
        if (
            signer["version"].native != "v1"
            or sid.name != "issuer_and_serial_number"
            or sid.chosen["issuer"].dump() != leaf["tbs_certificate"]["issuer"].dump()
            or sid.chosen["serial_number"].native
            != leaf["tbs_certificate"]["serial_number"].native
            or signer["unsigned_attrs"].native is not None
        ):
            raise ValueError("CMS signer does not match the bound certificate/profile")
        expected_certs = [leaf_der] + [
            _certificate_der(p) for p in signature.intermediates_pem
        ]
        actual_certs = []
        for item in data["certificates"]:
            if item.name != "certificate":
                raise ValueError("Only X.509 certificates are allowed")
            actual_certs.append(item.chosen.dump())
        if len(set(expected_certs)) != len(expected_certs) or sorted(
            actual_certs
        ) != sorted(expected_certs):
            raise ValueError("CMS certificate set differs from signer descriptor")
        digest_oid, signature_oid, size = (
            ("2.16.840.1.101.3.4.2.1", "1.2.840.10045.4.3.2", 32)
            if signature.algorithm == Algorithm.ECDSA_P256_SHA256
            else ("2.16.840.1.101.3.4.2.3", "2.16.840.1.101.3.4.3.18", 64)
        )

        def digest_ok(identifier):
            return identifier["algorithm"].dotted == digest_oid and identifier[
                "parameters"
            ].dump() in (b"", b"\x05\x00")

        if (
            len(data["digest_algorithms"]) != 1
            or not digest_ok(data["digest_algorithms"][0])
            or not digest_ok(signer["digest_algorithm"])
            or signer["signature_algorithm"]["algorithm"].dotted != signature_oid
            or signer["signature_algorithm"]["parameters"].dump() != b""
        ):
            raise ValueError("CMS algorithm identifiers differ from the profile")
        attrs = {}
        for attr in signer["signed_attrs"]:
            oid = attr["type"].dotted
            if oid in attrs or len(attr["values"]) != 1:
                raise ValueError("Duplicate or multivalued CMS signed attribute")
            attrs[oid] = attr["values"][0]
        if (
            not {"1.2.840.113549.1.9.3", "1.2.840.113549.1.9.4"} <= attrs.keys()
            or not attrs.keys()
            <= {"1.2.840.113549.1.9.3", "1.2.840.113549.1.9.4", "1.2.840.113549.1.9.5"}
            or attrs["1.2.840.113549.1.9.3"].native != "data"
            or len(attrs["1.2.840.113549.1.9.4"].native) != size
        ):
            raise ValueError("Missing or unsupported CMS signed attributes")
        signature_bytes = signer["signature"].native
        if (
            signature.algorithm == Algorithm.ML_DSA_65 and len(signature_bytes) != 3309
        ) or (
            signature.algorithm == Algorithm.ECDSA_P256_SHA256
            and not 8 <= len(signature_bytes) <= 72
        ):
            raise ValueError("Invalid CMS signature length")
        return raw
    except (ValueError, TypeError, KeyError, IndexError) as error:
        raise SigningError("Invalid CMS signature/profile") from error


@dataclass(frozen=True)
class OpenSslBackend:
    """Use an explicitly selected OpenSSL executable with argument vectors.

    Configurations and executable paths are administrator-controlled inputs,
    never taken from archives. ``timeout_seconds`` also bounds token failures.
    """

    executable: str = "openssl"
    timeout_seconds: float = 30.0

    def _run(self, args: list[str]) -> bytes:
        try:
            result = subprocess.run(
                [self.executable, *args],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=self.timeout_seconds,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise SigningError("OpenSSL unavailable or operation timed out") from error
        if result.returncode:
            # Providers can include URI/PIN/configuration details in diagnostics.
            raise SigningError(
                "OpenSSL operation failed (check backend, key and profile)"
            )
        return result.stdout

    def _verify_one(self, payload: Path, signature: DetachedSignature, directory: Path):
        certificate = directory / "certificate.pem"
        signature_path = directory / "signature.der"
        certificate.write_text(signature.certificate_pem, encoding="ascii")
        signature_path.write_bytes(_cms_der(signature))
        # -noverify disables only certificate path validation, performed by our
        # separate trust layer. CMS signature/content/attribute checks stay on.
        # -nointern forces selection of the exact bound leaf, not a CMS-supplied key.
        self._run(
            [
                "cms",
                "-verify",
                "-binary",
                "-inform",
                "DER",
                "-in",
                str(signature_path),
                "-content",
                str(payload),
                "-noverify",
                "-nointern",
                "-certfile",
                str(certificate),
                "-out",
                str(directory / "verified.bin"),
            ]
        )

    def sign(
        self,
        manifest: bytes,
        identities: Sequence[SigningIdentity],
        policy: SignaturePolicy,
    ) -> SignatureEnvelope:
        identities = sorted(
            identities,
            key=lambda identity: _signer_key(
                identity.algorithm, identity.certificate_pem
            ),
        )
        envelope = SignatureEnvelope(
            2,
            policy,
            tuple(
                DetachedSignature(
                    identity.algorithm,
                    identity.certificate_pem,
                    tuple(identity.intermediates_pem),
                    "",
                )
                for identity in identities
            ),
        )
        payload = signing_payload(manifest, envelope)
        signatures = []
        with tempfile.TemporaryDirectory(prefix="mzpeak-sign-") as work:
            directory = Path(work)
            payload_path = directory / "payload.bin"
            payload_path.write_bytes(payload)
            for identity, descriptor in zip(identities, envelope.signatures):
                certificate_path = directory / "signer.pem"
                certificate_path.write_text(identity.certificate_pem, encoding="ascii")
                args = [
                    "cms",
                    "-sign",
                    "-binary",
                    "-nosmimecap",
                    "-outform",
                    "DER",
                    "-in",
                    str(payload_path),
                    "-signer",
                    str(certificate_path),
                    "-md",
                    "sha256"
                    if identity.algorithm == Algorithm.ECDSA_P256_SHA256
                    else "sha512",
                ]
                if identity.intermediates_pem:
                    chain_path = directory / "chain.pem"
                    chain_path.write_text(
                        "\n".join(identity.intermediates_pem), encoding="ascii"
                    )
                    args += ["-certfile", str(chain_path)]
                if isinstance(identity.key, SoftwareKey):
                    args += ["-inkey", str(identity.key.key_path)]
                    if identity.key.passphrase_file is not None:
                        args += ["-passin", "file:" + str(identity.key.passphrase_file)]
                elif isinstance(identity.key, Pkcs11Key):
                    if (
                        not identity.key.key_uri.startswith("pkcs11:")
                        or "pin-value" in identity.key.key_uri.lower()
                    ):
                        raise SigningError(
                            "Expected a PKCS#11 URI without an inline PIN"
                        )
                    if identity.algorithm != Algorithm.ECDSA_P256_SHA256:
                        raise SigningError(
                            "The PKCS#11 backend profile currently supports ECDSA only"
                        )
                    args += [
                        "-inkey",
                        identity.key.key_uri,
                        "-config",
                        str(identity.key.config_path),
                    ]
                else:
                    raise SigningError("Unsupported key provider")
                raw = self._run(args)
                signature = DetachedSignature(
                    descriptor.algorithm,
                    descriptor.certificate_pem,
                    descriptor.intermediates_pem,
                    raw.hex(),
                )
                # This also rejects a private key that does not match the supplied leaf.
                self._verify_one(payload_path, signature, directory)
                signatures.append(signature)
        return SignatureEnvelope(2, policy, tuple(signatures))

    def verify(
        self,
        manifest: bytes,
        envelope: SignatureEnvelope,
        required_policy: SignaturePolicy,
    ) -> tuple[CryptographicSigner, ...]:
        payload = signing_payload(manifest, envelope)
        if not _satisfies({s.algorithm for s in envelope.signatures}, required_policy):
            raise SigningError("Signatures do not satisfy the relying-party policy")
        verified = []
        with tempfile.TemporaryDirectory(prefix="mzpeak-verify-") as work:
            directory = Path(work)
            payload_path = directory / "payload.bin"
            payload_path.write_bytes(payload)
            for signature in envelope.signatures:
                self._verify_one(payload_path, signature, directory)
                verified.append(
                    CryptographicSigner(
                        signature.algorithm,
                        signature.certificate_pem,
                        signature.intermediates_pem,
                        hashlib.sha256(
                            _certificate_der(signature.certificate_pem)
                        ).hexdigest(),
                    )
                )
        return tuple(verified)
