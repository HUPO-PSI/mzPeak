"""Compose full history verification with independent historical timestamp evidence."""

from dataclasses import dataclass
from .evidence import (
    EvidenceBundle,
    TsaPolicy,
    TimestampReport,
    verify_historical_signer,
)
from .signing import OpenSslBackend, SigningError
from .trust import CheckStatus, SignerRole, role_for_stage
from .verification import verify_history, verify_archive_history, HistoryReport


@dataclass
class HistoricalHistoryReport:
    history: HistoryReport
    stage_timestamps: dict[str, TimestampReport]
    release_timestamps: dict[str, TimestampReport]

    @property
    def accepted(self):
        return (
            self.history.accepted
            and len(self.stage_timestamps) == len(self.history.stages)
            and all(r.accepted for r in self.stage_timestamps.values())
            and all(r.accepted for r in self.release_timestamps.values())
        )


def _unknown(reason):
    return TimestampReport(CheckStatus.UNKNOWN, details=[reason])


def _bound_signers(
    payload,
    envelope,
    bundle,
    trust,
    role,
    identity,
    required_policy,
    tsa_policy,
    policy_id,
    policy_bytes,
    backend,
):
    if bundle is None:
        return _unknown("historical timestamp evidence not supplied"), []
    try:
        # Re-verify the actual envelope; bundle certificate lists cannot choose signers.
        signers = backend.verify(payload, envelope, required_policy)
        certificates = []
        timestamp = _unknown("no cryptographic signer")
        for signer in signers:
            timestamp, certificate = verify_historical_signer(
                bundle,
                payload,
                envelope.canonical_bytes(),
                policy_id,
                policy_bytes,
                tsa_policy,
                trust,
                signer.certificate_pem,
                role,
                identity,
                openssl=backend.executable,
            )
            if not timestamp.accepted or certificate is None:
                return timestamp, []
            certificates.append(certificate)
        return timestamp, certificates
    except (ValueError, SigningError) as exc:
        return TimestampReport(CheckStatus.INVALID, details=[str(exc)]), []


def _historical(
    report,
    records,
    trust,
    required_policy,
    identities,
    releases,
    publishers,
    stage_evidence,
    release_evidence,
    tsa_policy,
    policy_id,
    policy_bytes,
    backend,
):
    stamps, release_stamps = {}, {}
    for record, stage_report in zip(records, report.stages):
        stage = record.stage
        identifier = stage.manifest.stage_id
        # Preserve failure of signature, structural lineage, coverage and local identity checks.
        if stage_report.signature != CheckStatus.VALID:
            stamps[identifier] = _unknown(
                "stage signature or local identity did not pass"
            )
            continue
        role = role_for_stage(stage.kind)
        stamp, certs = _bound_signers(
            stage.canonical_bytes(),
            record.envelope,
            stage_evidence.get(identifier),
            trust,
            role,
            identities[identifier],
            required_policy,
            tsa_policy,
            policy_id,
            policy_bytes,
            backend,
        )
        stamps[identifier] = stamp
        stage_report.certificates = certs
        if not stamp.accepted:
            stage_report.details.append(
                "historical stage timestamp was not established"
            )
        if stage.software is None:
            continue
        software = stage.software
        digest = software.release_sha256
        release, publisher = releases.get(digest), publishers.get(digest)
        stage_report.software_release = CheckStatus.INVALID
        stage_report.release_certificates = []
        if (
            release is None
            or not publisher
            or (
                release.statement.name,
                release.statement.version,
                release.statement.release_sha256,
            )
            != (software.name, software.version, software.release_sha256)
        ):
            release_stamps[digest] = _unknown("matching publisher approval is missing")
            continue
        # A processing timestamp cannot prove the publisher approval envelope existed.
        stamp, certs = _bound_signers(
            release.statement.canonical_bytes(),
            release.envelope,
            release_evidence.get(digest),
            trust,
            SignerRole.SOFTWARE_RELEASE,
            publisher,
            required_policy,
            tsa_policy,
            policy_id,
            policy_bytes,
            backend,
        )
        release_stamps[digest] = stamp
        stage_report.release_certificates = certs
        if stamp.accepted and certs and all(c.accepted for c in certs):
            stage_report.software_release = CheckStatus.VALID
        else:
            stage_report.details.append(
                "independent historical publisher approval was not established"
            )
    return HistoricalHistoryReport(report, stamps, release_stamps)


def verify_historical_history(
    records,
    files,
    trust,
    required_policy,
    identities,
    *,
    tsa_policy: TsaPolicy,
    policy_id: str,
    policy_bytes: bytes,
    stage_evidence: dict[str, EvidenceBundle],
    release_evidence: dict[str, EvidenceBundle] | None = None,
    releases=None,
    publishers=None,
    expected_heads=None,
    backend=None,
    conversion_policy=None,
):
    """Require historical evidence for every stage and each publisher approval.

    The policy identifier/bytes and all trust anchors/grants are supplied externally.
    Timestamp absence never falls back to claimed signing times or current validity.
    """
    records, releases, publishers = list(records), releases or {}, publishers or {}
    backend = backend or OpenSslBackend()
    report = verify_history(
        records,
        files,
        trust,
        required_policy,
        identities,
        releases=releases,
        publishers=publishers,
        expected_heads=expected_heads,
        backend=backend,
        conversion_policy=conversion_policy,
    )
    return _historical(
        report,
        records,
        trust,
        required_policy,
        identities,
        releases,
        publishers,
        stage_evidence,
        release_evidence or {},
        tsa_policy,
        policy_id,
        policy_bytes,
        backend,
    )


def verify_historical_archive_history(
    archive,
    records,
    trust,
    required_policy,
    identities,
    *,
    tsa_policy: TsaPolicy,
    policy_id: str,
    policy_bytes: bytes,
    stage_evidence: dict[str, EvidenceBundle],
    release_evidence: dict[str, EvidenceBundle] | None = None,
    releases=None,
    publishers=None,
    expected_heads=None,
    backend=None,
    conversion_policy=None,
):
    """Streaming archive variant with the same mandatory historical proof policy."""
    records, releases, publishers = list(records), releases or {}, publishers or {}
    backend = backend or OpenSslBackend()
    report = verify_archive_history(
        archive,
        records,
        trust,
        required_policy,
        identities,
        releases=releases,
        publishers=publishers,
        expected_heads=expected_heads,
        backend=backend,
        conversion_policy=conversion_policy,
    )
    return _historical(
        report,
        records,
        trust,
        required_policy,
        identities,
        releases,
        publishers,
        stage_evidence,
        release_evidence or {},
        tsa_policy,
        policy_id,
        policy_bytes,
        backend,
    )
