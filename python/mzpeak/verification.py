"""Compose content coverage, stage signatures, certificate policy and release approval.

An accepted result covers the supplied history. Only an external head checkpoint
can detect removal of an entire later stage. Release approval attests the named
binary, not that an unmodified binary actually ran (that needs platform evidence).
"""

from dataclasses import dataclass
from typing import Mapping, Sequence
from .manifest import Manifest, CoverageReport, _canonical_json, _digest
from .stages import Stage, LineageReport, verify_lineage
from .signing import OpenSslBackend, SignatureEnvelope, SignaturePolicy, SigningError
from .trust import (
    CheckStatus,
    CertificateReport,
    TrustPolicy,
    SignerRole,
    verify_certificate,
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
    records, trust, required_policy, identities, releases, publishers, backend
):
    reports = []
    for record in records:
        stage = record.stage
        report = StageReport(
            stage.manifest.stage_id, CheckStatus.INVALID, [], None, [], []
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
        role = (
            SignerRole.ACQUISITION
            if stage.kind == "acquisition"
            else SignerRole.PROCESSING
        )
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
) -> HistoryReport:
    """Verify every stage and every supplied content file; no implicit trusted keys."""
    records = list(records)
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
):
    """Streaming ZIP variant; rejects ambiguous names before content validation."""
    from .provenance_archive import verify_archive

    records = list(records)
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
    )
    return HistoryReport(coverage, lineage, reports)
