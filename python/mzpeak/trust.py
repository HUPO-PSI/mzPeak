"""Explicit relying-party trust policy for experimental mzPeak provenance.

Certificates embedded in an archive are untrusted inputs. Root certificates and
role/identity grants MUST come from local configuration, never from the archive.
OpenSSL performs RFC 5280 path/CRL validation; pinned grants constrain the demo
until the standard assigns interoperable certificate role/identity profiles.
"""
from dataclasses import dataclass, field
from enum import Enum
from hashlib import sha256
from pathlib import Path
import subprocess
import tempfile

from cryptography import x509
from cryptography.hazmat.primitives.serialization import Encoding
from cryptography.x509.oid import ObjectIdentifier


class CheckStatus(str, Enum):
    VALID = "valid"
    INVALID = "invalid"
    UNKNOWN = "unknown"


class SignerRole(str, Enum):
    ACQUISITION = "acquisition"
    PROCESSING = "processing"
    SOFTWARE_RELEASE = "software_release"


@dataclass(frozen=True)
class Authorization:
    certificate_sha256: str
    role: SignerRole
    identity: str
    required_eku: str | None = None


@dataclass(frozen=True)
class TrustPolicy:
    """External policy for one signer.

    ``require_pqc_chain`` selects the strict PQC-only profile: the leaf and all
    supplied intermediates/anchors must use ML-DSA keys and signatures. Use a
    separate PQC trust domain; a mixed ECDSA/PQC signature envelope alone does
    not make a classically issued certificate quantum resistant.
    """
    roots_pem: list[str]
    authorizations: list[Authorization]
    validation_time: int
    crls_pem: list[str] = field(default_factory=list)
    require_pqc_chain: bool = False


@dataclass
class CertificateReport:
    certificate_sha256: str
    path: CheckStatus
    authorization: CheckStatus
    revocation: CheckStatus
    validation_time: int
    details: list[str] = field(default_factory=list)

    @property
    def accepted(self) -> bool:
        """Unknown/missing revocation evidence never qualifies as accepted."""
        return all(x == CheckStatus.VALID for x in
                   (self.path, self.authorization, self.revocation))


_PQ_OIDS = {"2.16.840.1.101.3.4.3.17", "2.16.840.1.101.3.4.3.18", "2.16.840.1.101.3.4.3.19"}


def _certificate(pem: str) -> x509.Certificate:
    if (pem.count("-----BEGIN CERTIFICATE-----") != 1 or
            pem.count("-----END CERTIFICATE-----") != 1 or
            not pem.strip().startswith("-----BEGIN CERTIFICATE-----") or
            not pem.strip().endswith("-----END CERTIFICATE-----")):
        raise ValueError("expected exactly one PEM certificate")
    return x509.load_pem_x509_certificate(pem.encode("ascii"))


def certificate_fingerprint(pem: str) -> str:
    return sha256(_certificate(pem).public_bytes(Encoding.DER)).hexdigest()


def _run(openssl: str, args: list[str]) -> subprocess.CompletedProcess:
    # No shell, no network discovery, no PINs or private keys on this path.
    return subprocess.run([openssl, *args], capture_output=True, timeout=30)


def verify_certificate(leaf_pem: str, intermediates_pem: list[str],
                       policy: TrustPolicy, role: SignerRole, identity: str,
                       *, openssl: str = "openssl") -> CertificateReport:
    """Validate at an explicit time; this time is NOT proof of signing time.

    This validates identity eligibility only. Call the signature and content
    verifiers as well. CRLs are checked for the full path, including issuing CAs.
    No AIA/CRL URL from an untrusted certificate is fetched automatically.
    """
    if type(policy.validation_time) is not int or policy.validation_time < 0:
        raise ValueError("validation_time must be a nonnegative Unix second")
    result = CertificateReport("", CheckStatus.INVALID, CheckStatus.INVALID,
                               CheckStatus.UNKNOWN, policy.validation_time)
    try:
        leaf = _certificate(leaf_pem)
        result.certificate_sha256 = sha256(leaf.public_bytes(Encoding.DER)).hexdigest()
        roots = [_certificate(p) for p in policy.roots_pem]
        chain = [_certificate(p) for p in intermediates_pem]
        if not roots:
            raise ValueError("no externally configured trust anchors")
        for root in roots:
            if not root.extensions.get_extension_for_class(x509.BasicConstraints).value.ca:
                raise ValueError("trust anchors must be CA certificates")
        bc = leaf.extensions.get_extension_for_class(x509.BasicConstraints).value
        ku = leaf.extensions.get_extension_for_class(x509.KeyUsage).value
        if bc.ca or not ku.digital_signature or ku.key_cert_sign:
            raise ValueError("signer must be an end entity with digitalSignature key usage")
        if policy.require_pqc_chain:
            for cert in [leaf, *chain, *roots]:
                if (cert.public_key_algorithm_oid.dotted_string not in _PQ_OIDS or
                        cert.signature_algorithm_oid.dotted_string not in _PQ_OIDS):
                    raise ValueError("PQC identity policy rejects a classical certificate path")
    except (ValueError, x509.ExtensionNotFound, x509.DuplicateExtension) as exc:
        result.details.append(str(exc))
        return result
    # A role or EKU denial must not suppress independent path and CRL checks.
    try:
        grants = [a for a in policy.authorizations if
                  a.certificate_sha256 == result.certificate_sha256 and
                  a.role == role and a.identity == identity and bool(identity)]
        if len(grants) != 1:
            result.details.append("no unique locally configured role and identity grant")
        else:
            grant = grants[0]
            required_eku = grant.required_eku
            if role == SignerRole.SOFTWARE_RELEASE and required_eku is None:
                required_eku = "1.3.6.1.5.5.7.3.3"  # id-kp-codeSigning
            if required_eku is not None:
                required_oid = ObjectIdentifier(required_eku)
                eku = leaf.extensions.get_extension_for_class(x509.ExtendedKeyUsage).value
                if required_oid not in eku:
                    raise ValueError("certificate lacks the locally required extended key usage")
                # OpenSSL's purpose=any checks the general path but cannot
                # express arbitrary instrument/software application EKUs.
                # Constrain every supplied issuer/anchor carrying EKU; this is
                # conservative if unrelated path-building inputs are supplied.
                for issuer in [*chain, *roots]:
                    try:
                        issuer_eku = issuer.extensions.get_extension_for_class(x509.ExtendedKeyUsage).value
                    except x509.ExtensionNotFound:
                        continue
                    if (required_oid not in issuer_eku and
                            ObjectIdentifier("2.5.29.37.0") not in issuer_eku):
                        raise ValueError("issuer extended key usage excludes the locally required purpose")
            result.authorization = CheckStatus.VALID
    except (ValueError, x509.ExtensionNotFound, x509.DuplicateExtension) as exc:
        result.details.append(str(exc))
    with tempfile.TemporaryDirectory(prefix="mzpeak-trust-") as temp:
        directory = Path(temp)
        (directory / "leaf.pem").write_text(leaf_pem, encoding="ascii")
        (directory / "roots.pem").write_text("\n".join(policy.roots_pem), encoding="ascii")
        args = ["verify", "-no-CApath", "-no-CAstore", "-CAfile", str(directory / "roots.pem"),
                "-x509_strict", "-check_ss_sig", "-purpose", "any", "-attime", str(policy.validation_time)]
        if intermediates_pem:
            (directory / "chain.pem").write_text("\n".join(intermediates_pem), encoding="ascii")
            args.extend(["-untrusted", str(directory / "chain.pem")])
        checked = _run(openssl, [*args, str(directory / "leaf.pem")])
        if checked.returncode:
            result.details.append(checked.stderr.decode("utf-8", "replace")[:4096])
            return result
        result.path = CheckStatus.VALID
        if not policy.crls_pem:
            result.details.append("revocation evidence not supplied")
            return result
        (directory / "crls.pem").write_text("\n".join(policy.crls_pem), encoding="ascii")
        checked = _run(openssl, [*args, "-CRLfile", str(directory / "crls.pem"),
                                 "-crl_check_all", str(directory / "leaf.pem")])
        result.revocation = CheckStatus.VALID if checked.returncode == 0 else CheckStatus.INVALID
        if checked.returncode:
            result.details.append(checked.stderr.decode("utf-8", "replace")[:4096])
    return result
