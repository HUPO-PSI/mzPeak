//! Full history verification with separate trusted timestamp evidence per signed envelope.
use super::{
    evidence::{
        EvidenceBundle, EvidenceError, TimestampReport, TsaPolicy, verify_historical_signer_with,
    },
    manifest::ManifestError,
    signing::{OpenSslBackend, SignatureEnvelope},
    trust::{CertificateReport, CheckStatus, SignerRole},
    verification::{
        HistoryError, HistoryPolicy, HistoryReport, SignedRelease, SignedStage,
        verify_archive_history, verify_history,
    },
};
use serde::Serialize;
use std::collections::BTreeMap;

pub struct HistoricalPolicy<'a> {
    pub tsa: &'a TsaPolicy,
    /// These exact policy bytes and identifier are selected by the relying party.
    pub policy_id: &'a str,
    pub policy_bytes: &'a [u8],
    pub stage_evidence: &'a BTreeMap<String, EvidenceBundle>,
    pub release_evidence: &'a BTreeMap<String, EvidenceBundle>,
}
#[derive(Debug, Serialize)]
pub struct HistoricalHistoryReport {
    pub history: HistoryReport,
    pub stage_timestamps: BTreeMap<String, TimestampReport>,
    pub release_timestamps: BTreeMap<String, TimestampReport>,
}
impl HistoricalHistoryReport {
    pub fn accepted(&self) -> bool {
        self.history.accepted()
            && self.stage_timestamps.len() == self.history.stages.len()
            && self
                .stage_timestamps
                .values()
                .all(TimestampReport::accepted)
            && self
                .release_timestamps
                .values()
                .all(TimestampReport::accepted)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum HistoricalError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Evidence(#[from] EvidenceError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}
fn unknown(reason: &str) -> TimestampReport {
    TimestampReport {
        status: CheckStatus::Unknown,
        trusted_time: None,
        policy_oid: None,
        details: vec![reason.into()],
    }
}
fn invalid(reason: String) -> TimestampReport {
    TimestampReport {
        status: CheckStatus::Invalid,
        trusted_time: None,
        policy_oid: None,
        details: vec![reason],
    }
}
#[allow(clippy::too_many_arguments)]
fn bound_signers(
    payload: &[u8],
    envelope: &SignatureEnvelope,
    bundle: Option<&EvidenceBundle>,
    policy: &HistoryPolicy<'_>,
    historical: &HistoricalPolicy<'_>,
    role: SignerRole,
    identity: &str,
    backend: &OpenSslBackend,
) -> Result<(TimestampReport, Vec<CertificateReport>), HistoricalError> {
    let Some(bundle) = bundle else {
        return Ok((
            unknown("historical timestamp evidence not supplied"),
            vec![],
        ));
    };
    // Reverify the actual envelope; the bundle's certificate lists cannot select signers.
    let signers = match backend.verify(payload, envelope, policy.required_signatures) {
        Ok(signers) => signers,
        Err(error) => return Ok((invalid(error.to_string()), vec![])),
    };
    let canonical_envelope = match envelope.canonical_bytes() {
        Ok(bytes) => bytes,
        Err(error) => return Ok((invalid(error.to_string()), vec![])),
    };
    let mut certificates = vec![];
    let mut timestamp = unknown("no cryptographic signer");
    for signer in signers {
        match verify_historical_signer_with(
            &backend.executable,
            bundle,
            payload,
            &canonical_envelope,
            historical.policy_id,
            historical.policy_bytes,
            historical.tsa,
            policy.trust,
            &signer.certificate_pem,
            role,
            identity,
        ) {
            Ok((stamp, certificate)) => {
                if !stamp.accepted() || certificate.is_none() {
                    return Ok((stamp, vec![]));
                }
                timestamp = stamp;
                if let Some(certificate) = certificate {
                    certificates.push(certificate);
                }
            }
            Err(EvidenceError::Invalid(message)) => return Ok((invalid(message), vec![])),
            Err(error) => return Err(error.into()),
        }
    }
    Ok((timestamp, certificates))
}
fn historical(
    mut report: HistoryReport,
    records: &[SignedStage],
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
    historical: &HistoricalPolicy<'_>,
) -> Result<HistoricalHistoryReport, HistoricalError> {
    let mut stamps = BTreeMap::new();
    let mut release_stamps = BTreeMap::new();
    for (record, stage_report) in records.iter().zip(report.stages.iter_mut()) {
        let stage = &record.stage;
        let identifier = &stage.manifest.stage_id;
        if stage_report.signature != CheckStatus::Valid {
            stamps.insert(
                identifier.clone(),
                unknown("stage signature or local identity did not pass"),
            );
            continue;
        }
        let Some(identity) = policy.identities.get(identifier).filter(|v| !v.is_empty()) else {
            stamps.insert(
                identifier.clone(),
                unknown("local stage identity is absent"),
            );
            stage_report.certificates.clear();
            continue;
        };
        let role = SignerRole::for_stage_kind(&stage.kind);
        let (stamp, certs) = bound_signers(
            &stage.canonical_bytes()?,
            &record.envelope,
            historical.stage_evidence.get(identifier),
            policy,
            historical,
            role,
            identity,
            backend,
        )?;
        if !stamp.accepted() {
            stage_report
                .details
                .push("historical stage timestamp was not established".into());
        }
        stamps.insert(identifier.clone(), stamp);
        stage_report.certificates = certs;
        let Some(software) = &stage.software else {
            continue;
        };
        let digest = &software.release_sha256;
        stage_report.software_release = Some(CheckStatus::Invalid);
        stage_report.release_certificates.clear();
        match (
            releases.get(digest),
            policy.publishers.get(digest).filter(|v| !v.is_empty()),
        ) {
            (Some(release), Some(publisher))
                if release.statement.name == software.name
                    && release.statement.version == software.version
                    && release.statement.release_sha256 == *digest =>
            {
                // Processing timestamps do not prove that a distinct publisher approval existed.
                let (stamp, certs) = bound_signers(
                    &release.statement.canonical_bytes()?,
                    &release.envelope,
                    historical.release_evidence.get(digest),
                    policy,
                    historical,
                    SignerRole::SoftwareRelease,
                    publisher,
                    backend,
                )?;
                if stamp.accepted()
                    && !certs.is_empty()
                    && certs.iter().all(CertificateReport::accepted)
                {
                    stage_report.software_release = Some(CheckStatus::Valid);
                } else {
                    stage_report.details.push(
                        "independent historical publisher approval was not established".into(),
                    );
                }
                release_stamps.insert(digest.clone(), stamp);
                stage_report.release_certificates = certs;
            }
            _ => {
                release_stamps.insert(
                    digest.clone(),
                    unknown("matching publisher approval is missing"),
                );
            }
        }
    }
    Ok(HistoricalHistoryReport {
        history: report,
        stage_timestamps: stamps,
        release_timestamps: release_stamps,
    })
}

/// Every supplied stage and publisher approval requires its own valid historical evidence.
pub fn verify_historical_history(
    records: &[SignedStage],
    files: &BTreeMap<String, Vec<u8>>,
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
    evidence: &HistoricalPolicy<'_>,
) -> Result<HistoricalHistoryReport, HistoricalError> {
    let report = verify_history(records, files, policy, releases, backend)?;
    historical(report, records, policy, releases, backend, evidence)
}
/// Streaming archive variant; historical evidence never bypasses content or lineage checks.
pub fn verify_historical_archive_history(
    archive: impl AsRef<std::path::Path>,
    records: &[SignedStage],
    policy: &HistoryPolicy<'_>,
    releases: &BTreeMap<String, SignedRelease>,
    backend: &OpenSslBackend,
    evidence: &HistoricalPolicy<'_>,
) -> Result<HistoricalHistoryReport, HistoricalError> {
    let report = verify_archive_history(archive, records, policy, releases, backend)?;
    historical(report, records, policy, releases, backend, evidence)
}

#[cfg(test)]
mod tests {
    use super::super::{signing::SignaturePolicy, trust::TrustPolicy};
    use super::*;
    use std::{fs, io::Write};
    fn read(name: &str) -> Vec<u8> {
        fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/timestamps/history")
                .join(name),
        )
        .unwrap()
    }
    struct Fixture {
        records: Vec<SignedStage>,
        releases: BTreeMap<String, SignedRelease>,
        trust: TrustPolicy,
        files: BTreeMap<String, Vec<u8>>,
        heads: Vec<String>,
        tsa: TsaPolicy,
        stages: BTreeMap<String, EvidenceBundle>,
        release_evidence: BTreeMap<String, EvidenceBundle>,
    }
    impl Fixture {
        fn new() -> Self {
            let encoded: BTreeMap<String, String> =
                serde_json::from_slice(&read("files.json")).unwrap();
            Self {
                records: serde_json::from_slice(&read("records.json")).unwrap(),
                releases: serde_json::from_slice(&read("releases.json")).unwrap(),
                trust: serde_json::from_slice(&read("trust.json")).unwrap(),
                files: encoded
                    .into_iter()
                    .map(|(k, v)| (k, hex::decode(v).unwrap()))
                    .collect(),
                heads: serde_json::from_slice(&read("heads.json")).unwrap(),
                tsa: serde_json::from_slice(&read("tsa-policy.json")).unwrap(),
                stages: serde_json::from_slice(&read("stage-evidence.json")).unwrap(),
                release_evidence: serde_json::from_slice(&read("release-evidence.json")).unwrap(),
            }
        }
        fn check(
            &self,
            archive: Option<&std::path::Path>,
        ) -> Result<HistoricalHistoryReport, HistoricalError> {
            let identities = BTreeMap::from([
                ("acquisition".into(), "instrument-1".into()),
                ("processing".into(), "installation-1".into()),
            ]);
            let publishers = self
                .releases
                .keys()
                .map(|k| (k.clone(), "publisher-1".into()))
                .collect();
            let policy = HistoryPolicy {
                trust: &self.trust,
                required_signatures: SignaturePolicy::Ecdsa,
                identities: &identities,
                publishers: &publishers,
                expected_heads: Some(&self.heads),
                conversion: None,
            };
            let policy_bytes = read("policy.json");
            let evidence = HistoricalPolicy {
                tsa: &self.tsa,
                policy_id: "historical-demo-v1",
                policy_bytes: &policy_bytes,
                stage_evidence: &self.stages,
                release_evidence: &self.release_evidence,
            };
            let backend = OpenSslBackend::default();
            if let Some(path) = archive {
                verify_historical_archive_history(
                    path,
                    &self.records,
                    &policy,
                    &self.releases,
                    &backend,
                    &evidence,
                )
            } else {
                verify_historical_history(
                    &self.records,
                    &self.files,
                    &policy,
                    &self.releases,
                    &backend,
                    &evidence,
                )
            }
        }
    }
    #[test]
    fn signed_history_and_separate_release_approval_survive_current_leaf_expiry() {
        let fixture = Fixture::new();
        let report = fixture.check(None).unwrap();
        assert!(report.accepted(), "{report:?}");
        assert!(report.history.lineage.checkpoint_checked);
        assert_eq!(report.stage_timestamps.len(), 2);
        assert_eq!(report.release_timestamps.len(), 1);
        for stage in &report.history.stages {
            for cert in &stage.certificates {
                assert!(cert.validation_time < fixture.trust.validation_time);
            }
        }
        let identities = BTreeMap::from([
            ("acquisition".into(), "instrument-1".into()),
            ("processing".into(), "installation-1".into()),
        ]);
        let publishers = fixture
            .releases
            .keys()
            .map(|k| (k.clone(), "publisher-1".into()))
            .collect();
        let policy = HistoryPolicy {
            trust: &fixture.trust,
            required_signatures: SignaturePolicy::Ecdsa,
            identities: &identities,
            publishers: &publishers,
            expected_heads: Some(&fixture.heads),
            conversion: None,
        };
        assert!(
            !verify_history(
                &fixture.records,
                &fixture.files,
                &policy,
                &fixture.releases,
                &OpenSslBackend::default()
            )
            .unwrap()
            .accepted()
        );
    }
    #[test]
    fn timestamp_transfer_and_missing_publisher_proof_fail_closed() {
        let mut f = Fixture::new();
        f.stages
            .insert("acquisition".into(), f.stages["processing"].clone());
        let report = f.check(None).unwrap();
        assert!(!report.accepted());
        assert_eq!(
            report.stage_timestamps["acquisition"].status,
            CheckStatus::Invalid
        );
        assert!(report.history.stages[0].certificates.is_empty());
        let mut f = Fixture::new();
        f.release_evidence.clear();
        let report = f.check(None).unwrap();
        assert!(!report.accepted());
        assert!(report.history.stages[0].accepted());
        assert_eq!(
            report.release_timestamps.values().next().unwrap().status,
            CheckStatus::Unknown
        );
        let key = f.releases.keys().next().unwrap().clone();
        f.release_evidence
            .insert(key, f.stages["processing"].clone());
        assert!(!f.check(None).unwrap().accepted());
        f.stages.remove("acquisition");
        let report = f.check(None).unwrap();
        assert_eq!(
            report.stage_timestamps["acquisition"].status,
            CheckStatus::Unknown
        );
        assert!(!report.accepted());
    }
    #[test]
    fn historical_proofs_never_override_content_signature_or_external_grants() {
        let mut f = Fixture::new();
        f.files.get_mut("raw.bin").unwrap().push(0);
        assert!(!f.check(None).unwrap().accepted());
        let mut f = Fixture::new();
        f.records[0].envelope = f.records[1].envelope.clone();
        let report = f.check(None).unwrap();
        assert!(!report.accepted());
        assert_eq!(report.history.stages[0].signature, CheckStatus::Invalid);
        let mut f = Fixture::new();
        f.trust.authorizations.clear();
        assert!(!f.check(None).unwrap().accepted());
        let mut f = Fixture::new();
        f.tsa.roots_pem = f.trust.roots_pem.clone();
        assert!(!f.check(None).unwrap().accepted());
    }
    #[test]
    fn release_statement_must_match_exact_software_identity() {
        let mut f = Fixture::new();
        f.releases.values_mut().next().unwrap().statement.version = "other-version".into();
        assert!(!f.check(None).unwrap().accepted());
    }
    #[test]
    fn historical_archive_streaming_and_checkpoint_protection() {
        let mut fixture = Fixture::new();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.mzpeak");
        let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        for (name, content) in &fixture.files {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
        assert!(fixture.check(Some(&path)).unwrap().accepted());
        fixture.records.pop();
        assert!(fixture.check(None).is_err());
    }
}
