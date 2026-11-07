"""Experimental v1 detached manifest signatures using an OpenSSL backend.

This is deliberately a raw-signature envelope, not CMS. ML-DSA is the pure
ML-DSA-65 variant with an empty context; ECDSA uses P-256/SHA-256 and DER encoded
signatures. OpenSSL 3.5 or newer is needed for ML-DSA. ``verify`` establishes
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

from asn1crypto import algos, core, pem, x509


DOMAIN = b"mzPeak-signature-v1\0"
MAX_MANIFEST_BYTES = 64 * 1024 * 1024
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
    signature_hex: str


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
                        "signature_hex": s.signature_hex,
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
            if type(obj["version"]) is not int or obj["version"] != 1:
                raise SigningError("Unsupported envelope version")
            if not isinstance(obj["signatures"], list):
                raise SigningError("Signatures must be an array")
            signatures = []
            for value in obj["signatures"]:
                if set(value) != {
                    "algorithm",
                    "certificate_pem",
                    "intermediates_pem",
                    "signature_hex",
                }:
                    raise SigningError("Unknown or missing signature field")
                if not isinstance(value["intermediates_pem"], list):
                    raise SigningError("Intermediates must be an array")
                signatures.append(
                    DetachedSignature(
                        Algorithm(value["algorithm"]),
                        value["certificate_pem"],
                        tuple(value["intermediates_pem"]),
                        value["signature_hex"],
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


def signing_payload(manifest: bytes, envelope: SignatureEnvelope) -> bytes:
    """Return the v1 length-framed transcript, also used for test vectors."""
    if not isinstance(manifest, bytes) or len(manifest) > MAX_MANIFEST_BYTES:
        raise SigningError("Manifest must be bytes within the 64 MiB limit")
    if type(envelope.version) is not int or envelope.version != 1:
        raise SigningError("Unsupported envelope version")
    if not isinstance(envelope.policy, SignaturePolicy):
        raise SigningError("Unsupported signature policy")
    if not 1 <= len(envelope.signatures) <= 2:
        raise SigningError("Expected one or two signatures")
    algorithms = [s.algorithm for s in envelope.signatures]
    if any(not isinstance(a, Algorithm) for a in algorithms):
        raise SigningError("Unsupported signature algorithm")
    if algorithms != sorted(set(algorithms), key=lambda a: a.value):
        raise SigningError("Signatures must have unique, sorted algorithms")
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
    for signature in envelope.signatures:
        algorithm = signature.algorithm.value.encode("ascii")
        der = _check_certificate(signature.certificate_pem, signature.algorithm)
        if len(signature.intermediates_pem) > 8:
            raise SigningError("Too many intermediate certificates")
        for certificate in signature.intermediates_pem:
            _certificate_der(certificate)
        output += struct.pack(">I", len(algorithm)) + algorithm
        output += hashlib.sha256(der).digest()
    output += struct.pack(">Q", len(manifest)) + manifest
    return bytes(output)


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
        public_key = directory / "public.pem"
        signature_path = directory / "signature.bin"
        certificate.write_text(signature.certificate_pem, encoding="ascii")
        try:
            if not isinstance(signature.signature_hex, str):
                raise ValueError()
            limit = 6618 if signature.algorithm == Algorithm.ML_DSA_65 else 144
            if len(signature.signature_hex) > limit:
                raise ValueError()
            raw = bytes.fromhex(signature.signature_hex)
            if raw.hex() != signature.signature_hex:
                raise ValueError()
            if signature.algorithm == Algorithm.ML_DSA_65:
                if len(raw) != 3309:
                    raise ValueError()
            elif not 8 <= len(raw) <= 72:
                raise ValueError()
        except ValueError as error:
            raise SigningError("Invalid signature encoding or length") from error
        signature_path.write_bytes(raw)
        public_key.write_bytes(
            self._run(["x509", "-in", str(certificate), "-pubkey", "-noout"])
        )
        args = [
            "pkeyutl",
            "-verify",
            "-pubin",
            "-inkey",
            str(public_key),
            "-rawin",
            "-in",
            str(payload),
            "-sigfile",
            str(signature_path),
        ]
        if signature.algorithm == Algorithm.ECDSA_P256_SHA256:
            args += ["-digest", "sha256"]
        self._run(args)

    def sign(
        self,
        manifest: bytes,
        identities: Sequence[SigningIdentity],
        policy: SignaturePolicy,
    ) -> SignatureEnvelope:
        identities = sorted(identities, key=lambda identity: identity.algorithm.value)
        envelope = SignatureEnvelope(
            1,
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
                args = ["pkeyutl", "-sign", "-rawin", "-in", str(payload_path)]
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
                if identity.algorithm == Algorithm.ECDSA_P256_SHA256:
                    args += ["-digest", "sha256"]
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
        return SignatureEnvelope(1, policy, tuple(signatures))

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
