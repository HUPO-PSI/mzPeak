"""Compose content coverage, stage signatures, certificate policy and release approval.

An accepted result covers the supplied history. Only an external head checkpoint
can detect removal of an entire later stage. Release approval attests the named
binary, not that an unmodified binary actually ran (that needs platform evidence).
"""

from dataclasses import dataclass, field
import os
from typing import Mapping, Sequence
from .conversion import _text, verify_source
from .manifest import (
    Manifest,
    CoverageReport,
    _canonical_json,
    _digest,
    _identifier,
    _keys,
    _load_json,
)
from .stages import Stage, LineageReport, verify_lineage
from .signing import OpenSslBackend, SignatureEnvelope, SignaturePolicy, SigningError
from .trust import (
    CheckStatus,
    CertificateReport,
    TrustPolicy,
    SignerRole,
    verify_certificate,
    role_for_stage,
)


@dataclass(frozen=True)
class VendorVerificationGrant:
    vendor: str
    format: str
    method: str
    verifier_name: str
    verifier_version: str

    def validate(self):
        for name in ("vendor", "format", "method", "verifier_name", "verifier_version"):
            _text(getattr(self, name), name)

    @classmethod
    def from_dict(cls, value):
        _keys(
            value, ("vendor", "format", "method", "verifier_name", "verifier_version")
        )
        result = cls(**value)
        result.validate()
        return result


@dataclass(frozen=True)
class ConversionPolicy:
    allowed_verifications: tuple[VendorVerificationGrant, ...]
    require_source_bytes: bool = False
    sources: Mapping[str, os.PathLike | str] = field(default_factory=dict)

    def validate(self):
        if type(self.require_source_bytes) is not bool:
            raise ValueError("require_source_bytes must be boolean")
        if not isinstance(self.allowed_verifications, (tuple, list)):
            raise ValueError("allowed_verifications must be an array")
        for grant in self.allowed_verifications:
            grant.validate()
        if len(set(self.allowed_verifications)) != len(self.allowed_verifications):
            raise ValueError("duplicate vendor verification grant")
        if not isinstance(self.sources, Mapping):
            raise ValueError("source paths must be a map")
        for identifier, path in self.sources.items():
            _identifier(identifier, "source stage ID")
            try:
                value = os.fspath(path)
            except TypeError:
                raise ValueError(
                    "source path must be a local filesystem path"
                ) from None
            if not isinstance(value, str) or not value or "\0" in value:
                raise ValueError("source path must be a nonempty local filesystem path")

    @classmethod
    def from_dict(cls, value):
        _keys(value, ("allowed_verifications", "require_source_bytes", "sources"))
        if not isinstance(value["allowed_verifications"], list) or not isinstance(
            value["sources"], dict
        ):
            raise ValueError("expected grant array and source path map")
        if any(not isinstance(path, str) for path in value["sources"].values()):
            raise ValueError("source paths must be strings")
        result = cls(
            tuple(
                VendorVerificationGrant.from_dict(g)
                for g in value["allowed_verifications"]
            ),
            value["require_source_bytes"],
            dict(value["sources"]),
        )
        result.validate()
        return result

    @classmethod
    def from_bytes(cls, data):
        return cls.from_dict(_load_json(data))


@dataclass
class ConversionReport:
    vendor_attestation: CheckStatus
    source_integrity: CheckStatus
    source_required: bool
    source_manifest_sha256: str
    claimed_acquisition_time: int | None
    details: list[str] = field(default_factory=list)

    @property
    def accepted(self):
        return (
            self.vendor_attestation == CheckStatus.VALID
            and self.source_integrity != CheckStatus.INVALID
            and (not self.source_required or self.source_integrity == CheckStatus.VALID)
        )


def _check_conversion(stage, identity, policy):
    claim = stage.conversion
    report = ConversionReport(
        CheckStatus.INVALID,
        CheckStatus.UNKNOWN,
        False if policy is None else policy.require_source_bytes,
        claim.source.digest(),
        claim.claimed_acquisition_time,
    )
    if policy is None:
        report.details.append("vendor conversion is not enabled by recipient policy")
        return report
    verification = claim.verification
    grant = VendorVerificationGrant(
        verification.vendor,
        claim.source.format,
        verification.method,
        verification.verifier_name,
        verification.verifier_version,
    )
    if (
        verification.vendor != identity
        or verification.result != "passed"
        or grant not in policy.allowed_verifications
    ):
        report.details.append(
            "vendor identity, verification method, result or cryptographic stage signature did not qualify"
        )
    else:
        report.vendor_attestation = CheckStatus.VALID
    source_path = policy.sources.get(stage.manifest.stage_id)
    if source_path is None:
        report.details.append(
            "original RAW source bytes were not supplied for independent checking"
        )
    else:
        try:
            checked = verify_source(source_path, claim.source)
            report.source_integrity = (
                CheckStatus.VALID if checked.complete else CheckStatus.INVALID
            )
            if not checked.complete:
                report.details.append(
                    "source inventory differs from supplied RAW bytes"
                )
        except (OSError, ValueError, EOFError) as exc:
            report.source_integrity = CheckStatus.INVALID
            report.details.append(f"supplied RAW source could not be verified: {exc}")
    return report


def _validate_conversion_policy(policy, records):
    if policy is None:
        return
    policy.validate()
    conversion_ids = {
        record.stage.manifest.stage_id
        for record in records
        if record.stage.kind == "conversion"
    }
    unused = set(policy.sources) - conversion_ids
    if unused:
        raise ValueError(
            "source path names an unknown or nonconversion stage: "
            + ", ".join(sorted(unused))
        )


@dataclass(frozen=True)
class ReleaseStatement:
    name: str
    version: str
    release_sha256: str

    def canonical_bytes(self):
        _digest(self.release_sha256)
        for text in (self.name, self.version):
            if (
                not isinstance(text, str)
                or not text
                or len(text) > 128
                or not all(32 <= ord(c) <= 126 for c in text)
            ):
                raise ValueError("invalid release name/version")
        return _canonical_json(
            {
                "type": "mzpeak-software-release",
                "version": 1,
                "name": self.name,
                "software_version": self.version,
                "release_sha256": self.release_sha256,
            }
        )


@dataclass(frozen=True)
class SignedRelease:
    statement: ReleaseStatement
    envelope: SignatureEnvelope


@dataclass(frozen=True)
class SignedStage:
    stage: Stage
    envelope: SignatureEnvelope


@dataclass
class StageReport:
    stage_id: str
    signature: CheckStatus
    certificates: list[CertificateReport]
    software_release: CheckStatus | None
    release_certificates: list[CertificateReport]
    details: list[str]
    kind: str = "acquisition"
    conversion: ConversionReport | None = None

    @property
    def accepted(self):
        return (
            self.signature == CheckStatus.VALID
            and bool(self.certificates)
            and all(c.accepted for c in self.certificates)
            and (
                self.software_release is None
                or self.software_release == CheckStatus.VALID
            )
            and (self.conversion is None or self.conversion.accepted)
            and (self.kind != "conversion" or self.conversion is not None)
        )


@dataclass
class HistoryReport:
    coverage: CoverageReport
    lineage: LineageReport
    stages: list[StageReport]

    @property
    def accepted(self):
        return (
            self.coverage.complete
            and bool(self.stages)
            and all(s.accepted for s in self.stages)
        )


def _union_manifest(records):
    first = records[0].stage.manifest
    files = {}
    for record in records:
        for descriptor in record.stage.manifest.files:
            # Chunk order has no signed meaning: FileDigest.to_dict uses the
            # same normalized order as manifest/stage canonicalization.
            if (
                descriptor.path in files
                and files[descriptor.path].to_dict() != descriptor.to_dict()
            ):
                raise ValueError("inconsistent repeated file descriptor")
            files[descriptor.path] = descriptor
    return Manifest(first.run_id, "verified-history", tuple(files.values()))


def _check_stages(
    records,
    trust,
    required_policy,
    identities,
    releases,
    publishers,
    backend,
    conversion_policy,
):
    reports = []
    for record in records:
        stage = record.stage
        report = StageReport(
            stage.manifest.stage_id, CheckStatus.INVALID, [], None, [], [], stage.kind
        )
        reports.append(report)
        identity = identities.get(stage.manifest.stage_id)
        if not identity:
            report.details.append("no externally configured stage identity")
            continue
        try:
            signers = backend.verify(
                stage.canonical_bytes(), record.envelope, required_policy
            )
            report.signature = CheckStatus.VALID
        except SigningError as exc:
            report.details.append(str(exc))
            continue
        role = role_for_stage(stage.kind)
        report.certificates = [
            verify_certificate(
                s.certificate_pem,
                list(s.intermediates_pem),
                trust,
                role,
                identity,
                openssl=backend.executable,
            )
            for s in signers
        ]
        if stage.kind == "conversion":
            report.conversion = _check_conversion(stage, identity, conversion_policy)
        if stage.software is None:
            continue
        report.software_release = CheckStatus.INVALID
        software = stage.software
        release = releases.get(software.release_sha256)
        publisher = publishers.get(software.release_sha256)
        if release is None or not publisher:
            report.details.append(
                "publisher approval for this software release is missing"
            )
            continue
        if (
            release.statement.name,
            release.statement.version,
            release.statement.release_sha256,
        ) != (software.name, software.version, software.release_sha256):
            report.details.append(
                "software release approval does not match the signed stage"
            )
            continue
        try:
            release_signers = backend.verify(
                release.statement.canonical_bytes(), release.envelope, required_policy
            )
            report.release_certificates = [
                verify_certificate(
                    s.certificate_pem,
                    list(s.intermediates_pem),
                    trust,
                    SignerRole.SOFTWARE_RELEASE,
                    publisher,
                    openssl=backend.executable,
                )
                for s in release_signers
            ]
            if report.release_certificates and all(
                c.accepted for c in report.release_certificates
            ):
                report.software_release = CheckStatus.VALID
        except SigningError as exc:
            report.details.append(str(exc))
    return reports


def verify_history(
    records: Sequence[SignedStage],
    files: Mapping[str, bytes],
    trust: TrustPolicy,
    required_policy: SignaturePolicy,
    identities: Mapping[str, str],
    *,
    releases: Mapping[str, SignedRelease] | None = None,
    publishers: Mapping[str, str] | None = None,
    expected_heads: Sequence[str] | None = None,
    backend: OpenSslBackend | None = None,
    conversion_policy: ConversionPolicy | None = None,
) -> HistoryReport:
    """Verify every stage and every supplied content file; no implicit trusted keys."""
    records = list(records)
    _validate_conversion_policy(conversion_policy, records)
    lineage = verify_lineage([r.stage for r in records], expected_heads)
    coverage = _union_manifest(records).verify_files(files)
    reports = _check_stages(
        records,
        trust,
        required_policy,
        identities,
        releases or {},
        publishers or {},
        backend or OpenSslBackend(),
        conversion_policy,
    )
    return HistoryReport(coverage, lineage, reports)


def verify_archive_history(
    archive,
    records,
    trust,
    required_policy,
    identities,
    *,
    releases=None,
    publishers=None,
    expected_heads=None,
    backend=None,
    conversion_policy=None,
):
    """Streaming ZIP variant; rejects ambiguous names before content validation."""
    from .provenance_archive import verify_archive

    records = list(records)
    _validate_conversion_policy(conversion_policy, records)
    lineage = verify_lineage([r.stage for r in records], expected_heads)
    coverage = verify_archive(archive, _union_manifest(records))
    reports = _check_stages(
        records,
        trust,
        required_policy,
        identities,
        releases or {},
        publishers or {},
        backend or OpenSslBackend(),
        conversion_policy,
    )
    return HistoryReport(coverage, lineage, reports)
