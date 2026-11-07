"""RFC 3161 timestamps and archived validation evidence for the demo profile.

A timestamp is over SignatureEnvelope.canonical_bytes(), including every signature byte.
It establishes an upper bound on existence, not the acquisition or execution time.
Trust anchors and policy selection always come from the relying party.
"""

from dataclasses import asdict, dataclass, field, replace
from hashlib import sha256
import json
from pathlib import Path
import subprocess
import tempfile

from asn1crypto import tsp
from cryptography import x509
from .trust import CheckStatus, TrustPolicy, SignerRole, verify_certificate

MAX_EVIDENCE_BYTES = 16 * 1024 * 1024


@dataclass(frozen=True)
class TsaPolicy:
    roots_pem: list[str]
    validation_time: int
    allowed_policy_oids: list[str]
    crls_pem: list[str] = field(default_factory=list)
    intermediates_pem: list[str] = field(default_factory=list)


@dataclass
class TimestampReport:
    status: CheckStatus
    trusted_time: int | None = None
    policy_oid: str | None = None
    details: list[str] = field(default_factory=list)

    @property
    def accepted(self):
        return self.status == CheckStatus.VALID and self.trusted_time is not None


def _run(openssl, args):
    return subprocess.run([openssl, *map(str, args)], capture_output=True, timeout=30)


def create_timestamp_query(
    envelope_bytes: bytes, *, policy_oid: str | None = None, openssl: str = "openssl"
) -> bytes:
    """Create a SHA-256 request with a fresh nonce for canonical envelope bytes."""
    if not envelope_bytes or len(envelope_bytes) > MAX_EVIDENCE_BYTES:
        raise ValueError("empty or oversized envelope")
    with tempfile.TemporaryDirectory(prefix="mzpeak-ts-query-") as temp:
        directory = Path(temp)
        source, query = directory / "envelope", directory / "query.der"
        source.write_bytes(envelope_bytes)
        args = ["ts", "-query", "-data", source, "-sha256", "-cert", "-out", query]
        if policy_oid is not None:
            x509.ObjectIdentifier(policy_oid)
            args += ["-tspolicy", policy_oid]
        checked = _run(openssl, args)
        if checked.returncode:
            raise ValueError("OpenSSL could not create a timestamp query")
        return query.read_bytes()


def _check_pem(value, kind):
    begin, end = f"-----BEGIN {kind}-----", f"-----END {kind}-----"
    if (
        not isinstance(value, str)
        or value.count("-----BEGIN") != 1
        or value.count("-----END") != 1
        or not value.strip().startswith(begin)
        or not value.strip().endswith(end)
    ):
        raise ValueError(f"expected exactly one {kind} PEM object")
    raw = value.encode("ascii")
    if kind == "CERTIFICATE":
        return x509.load_pem_x509_certificate(raw)
    return x509.load_pem_x509_crl(raw)


def verify_timestamp(
    envelope_bytes: bytes,
    response_der: bytes | None,
    query_der: bytes | None,
    policy: TsaPolicy,
    *,
    openssl: str = "openssl",
) -> TimestampReport:
    """Validate imprint, optional request/nonce, TSA signature/path/policy and CRLs.

    genTime is parsed as ASN.1 and is never exposed as trusted until all checks
    succeed. The demo profile uses whole UTC seconds and no nonzero accuracy.
    Missing evidence is unknown; an invalid response never supplies a trusted time.
    """
    report = TimestampReport(CheckStatus.UNKNOWN)
    if response_der is None:
        report.details.append("timestamp evidence not supplied")
        return report
    report.status = CheckStatus.INVALID
    try:
        if (
            type(policy.validation_time) is not int
            or policy.validation_time < 0
            or not envelope_bytes
            or len(envelope_bytes) > MAX_EVIDENCE_BYTES
            or len(response_der) > MAX_EVIDENCE_BYTES
            or (query_der is not None and len(query_der) > MAX_EVIDENCE_BYTES)
        ):
            raise ValueError("invalid time or timestamp input size")
        if not policy.roots_pem or not policy.allowed_policy_oids:
            raise ValueError("external TSA roots and allowed policies are required")
        for value in policy.roots_pem:
            cert = _check_pem(value, "CERTIFICATE")
            if not cert.extensions.get_extension_for_class(
                x509.BasicConstraints
            ).value.ca:
                raise ValueError("TSA root must be a CA")
        for value in policy.intermediates_pem:
            _check_pem(value, "CERTIFICATE")
        for value in policy.crls_pem:
            _check_pem(value, "X509 CRL")
        response = tsp.TimeStampResp.load(response_der, strict=True)
        if response["status"]["status"].native not in ("granted", "granted_with_mods"):
            raise ValueError("TSA did not grant the request")
        token = response["time_stamp_token"]
        if token["content_type"].native != "signed_data":
            raise ValueError("timestamp token is not SignedData")
        encapsulated = token["content"]["encap_content_info"]
        if encapsulated["content_type"].native != "tst_info":
            raise ValueError("timestamp token does not contain TSTInfo")
        info = encapsulated["content"].parsed
        if info["version"].native != "v1":
            raise ValueError("unsupported TSTInfo version")
        imprint = info["message_imprint"]
        if (
            imprint["hash_algorithm"]["algorithm"].native != "sha256"
            or imprint["hashed_message"].native != sha256(envelope_bytes).digest()
        ):
            raise ValueError(
                "timestamp imprint does not match the complete signature envelope"
            )
        oid = info["policy"].dotted
        if oid not in policy.allowed_policy_oids:
            raise ValueError("TSA policy is not locally authorized")
        time = info["gen_time"]
        if len(time.contents) != 15 or not time.contents.endswith(b"Z"):
            raise ValueError("demo timestamp profile requires whole UTC seconds")
        if info["accuracy"].native and any(
            value for value in info["accuracy"].native.values()
        ):
            raise ValueError("demo timestamp profile does not support nonzero accuracy")
        claimed_time = int(time.native.timestamp())
        if claimed_time < 0 or claimed_time > policy.validation_time:
            raise ValueError("timestamp is in the future or before the Unix epoch")
        if query_der is not None:
            query = tsp.TimeStampReq.load(query_der, strict=True)
            if (
                query["version"].native != "v1"
                or query["message_imprint"]["hash_algorithm"]["algorithm"].native
                != "sha256"
                or query["message_imprint"]["hashed_message"].native
                != imprint["hashed_message"].native
            ):
                raise ValueError("stored timestamp request has a different imprint")
            if (
                query["nonce"].native is None
                or query["nonce"].native != info["nonce"].native
            ):
                raise ValueError(
                    "stored timestamp request nonce is absent or different"
                )
            if (
                query["req_policy"].native is not None
                and query["req_policy"].dotted != oid
            ):
                raise ValueError("timestamp response changed the requested policy")
        with tempfile.TemporaryDirectory(prefix="mzpeak-ts-verify-") as temp:
            directory = Path(temp)
            roots, response_path = directory / "roots.pem", directory / "response.der"
            empty = directory / "empty-store"
            empty.mkdir()
            roots.write_text(
                "\n".join([*policy.roots_pem, *policy.crls_pem]), encoding="ascii"
            )
            response_path.write_bytes(response_der)
            args = [
                "ts",
                "-verify",
                "-in",
                response_path,
                "-CAfile",
                roots,
                "-CApath",
                empty,
                "-CAstore",
                empty,
                "-attime",
                str(claimed_time),
                "-x509_strict",
                "-check_ss_sig",
            ]
            if policy.intermediates_pem:
                chain = directory / "chain.pem"
                chain.write_text("\n".join(policy.intermediates_pem), encoding="ascii")
                args += ["-untrusted", chain]
            if query_der is not None:
                query_path = directory / "query.der"
                query_path.write_bytes(query_der)
                args += ["-queryfile", query_path]
            else:
                args += ["-digest", sha256(envelope_bytes).hexdigest()]
            checked = _run(openssl, args)
            if checked.returncode:
                raise ValueError(
                    "TSA signature, request, or certification path is invalid"
                )
            if not policy.crls_pem:
                report.status = CheckStatus.UNKNOWN
                report.details.append("TSA revocation evidence not supplied")
                return report
            checked = _run(openssl, [*args, "-crl_check_all"])
            if checked.returncode:
                raise ValueError(
                    "TSA full-chain revocation evidence is invalid, incomplete, or stale"
                )
        report.status = CheckStatus.VALID
        report.trusted_time = claimed_time
        report.policy_oid = oid
    except (
        ValueError,
        TypeError,
        KeyError,
        IndexError,
        OverflowError,
        x509.ExtensionNotFound,
        x509.DuplicateExtension,
    ) as exc:
        report.details.append(str(exc))
    return report


@dataclass(frozen=True)
class EvidenceBundle:
    version: int
    manifest_sha256: str
    envelope_sha256: str
    policy_id: str
    policy_sha256: str
    timestamp_response_hex: str | None = None
    timestamp_query_hex: str | None = None
    signer_intermediates_pem: list[str] = field(default_factory=list)
    signer_crls_pem: list[str] = field(default_factory=list)
    tsa_intermediates_pem: list[str] = field(default_factory=list)
    tsa_crls_pem: list[str] = field(default_factory=list)

    @classmethod
    def create(
        cls,
        manifest: bytes,
        envelope: bytes,
        policy_id: str,
        policy_bytes: bytes,
        **evidence,
    ):
        return cls(
            1,
            sha256(manifest).hexdigest(),
            sha256(envelope).hexdigest(),
            policy_id,
            sha256(policy_bytes).hexdigest(),
            **evidence,
        )

    def to_json(self):
        return json.dumps(
            asdict(self), ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")

    @classmethod
    def from_json(cls, data):
        def unique(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError("duplicate evidence field")
                result[key] = value
            return result

        if len(data) > MAX_EVIDENCE_BYTES:
            raise ValueError("evidence bundle too large")
        try:
            return cls(**json.loads(data, object_pairs_hook=unique))
        except (TypeError, json.JSONDecodeError) as exc:
            raise ValueError("malformed evidence bundle") from exc

    def check_binding(
        self, manifest: bytes, envelope: bytes, policy_id: str, policy_bytes: bytes
    ):
        """Compare bindings to independently supplied inputs, never archive-selected policy."""
        if (
            type(self.version) is not int
            or self.version != 1
            or not policy_id
            or self.manifest_sha256 != sha256(manifest).hexdigest()
            or self.envelope_sha256 != sha256(envelope).hexdigest()
            or self.policy_id != policy_id
            or self.policy_sha256 != sha256(policy_bytes).hexdigest()
        ):
            raise ValueError(
                "evidence binding does not match manifest, envelope, or local policy"
            )
        for encoded in (self.timestamp_response_hex, self.timestamp_query_hex):
            if encoded is not None and (
                not isinstance(encoded, str)
                or not encoded
                or len(encoded) > 2 * MAX_EVIDENCE_BYTES
                or bytes.fromhex(encoded).hex() != encoded
            ):
                raise ValueError(
                    "timestamp evidence must be canonical nonempty hexadecimal DER"
                )
        for values, kind in [
            (self.signer_intermediates_pem, "CERTIFICATE"),
            (self.tsa_intermediates_pem, "CERTIFICATE"),
            (self.signer_crls_pem, "X509 CRL"),
            (self.tsa_crls_pem, "X509 CRL"),
        ]:
            if not isinstance(values, list):
                raise ValueError("archived certificates and CRLs must be lists")
            for value in values:
                _check_pem(value, kind)


def verify_historical_signer(
    bundle: EvidenceBundle,
    manifest: bytes,
    envelope: bytes,
    policy_id: str,
    policy_bytes: bytes,
    tsa_policy: TsaPolicy,
    signer_policy: TrustPolicy,
    leaf_pem: str,
    role: SignerRole,
    identity: str,
    *,
    openssl="openssl",
):
    """Validate archived identity eligibility only after verified timestamp/bindings.

    The caller must separately verify the manifest signature and content coverage.
    Local roots/grants remain authoritative; archived CRLs supply historical evidence.
    Returns (timestamp_report, certificate_report_or_none).
    """
    bundle.check_binding(manifest, envelope, policy_id, policy_bytes)
    timestamp = verify_timestamp(
        envelope,
        bytes.fromhex(bundle.timestamp_response_hex)
        if bundle.timestamp_response_hex
        else None,
        bytes.fromhex(bundle.timestamp_query_hex)
        if bundle.timestamp_query_hex
        else None,
        replace(
            tsa_policy,
            crls_pem=bundle.tsa_crls_pem,
            intermediates_pem=bundle.tsa_intermediates_pem,
        ),
        openssl=openssl,
    )
    if not timestamp.accepted:
        return timestamp, None
    historic = replace(
        signer_policy,
        validation_time=timestamp.trusted_time,
        crls_pem=bundle.signer_crls_pem,
    )
    return timestamp, verify_certificate(
        leaf_pem,
        bundle.signer_intermediates_pem,
        historic,
        role,
        identity,
        openssl=openssl,
    )
