//! RFC 3161 timestamp evidence over `SignatureEnvelope::canonical_bytes()`.
//! TSA roots and policy selection are external relying-party configuration.
//! Timestamps establish existence by a time, not acquisition/execution time.
use super::trust::{
    CertificateReport, CheckStatus, SignerRole, TrustPolicy, verify_certificate_with,
};
use cms::{content_info::ContentInfo, signed_data::SignedData};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, process::Command};
use x509_cert::{
    Certificate,
    crl::CertificateList,
    der::{
        Any, Decode, DecodePem, Tag, Tagged,
        asn1::{GeneralizedTime, ObjectIdentifier, OctetString, Uint},
    },
    ext::pkix::BasicConstraints,
};

const MAX_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("invalid evidence: {0}")]
    Invalid(String),
    #[error("evidence I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("evidence JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("certificate verification: {0}")]
    Trust(#[from] super::trust::TrustError),
}
fn invalid(message: impl ToString) -> EvidenceError {
    EvidenceError::Invalid(message.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TsaPolicy {
    pub roots_pem: Vec<String>,
    pub validation_time: i64,
    pub allowed_policy_oids: Vec<String>,
    #[serde(default)]
    pub crls_pem: Vec<String>,
    #[serde(default)]
    pub intermediates_pem: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampReport {
    pub status: CheckStatus,
    pub trusted_time: Option<i64>,
    pub policy_oid: Option<String>,
    pub details: Vec<String>,
}
impl TimestampReport {
    pub fn accepted(&self) -> bool {
        self.status == CheckStatus::Valid && self.trusted_time.is_some()
    }
}

pub fn create_timestamp_query(
    envelope: &[u8],
    policy_oid: Option<&str>,
) -> Result<Vec<u8>, EvidenceError> {
    create_timestamp_query_with("openssl", envelope, policy_oid)
}
pub fn create_timestamp_query_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    envelope: &[u8],
    policy_oid: Option<&str>,
) -> Result<Vec<u8>, EvidenceError> {
    let openssl = openssl.as_ref();
    if envelope.is_empty() || envelope.len() > MAX_EVIDENCE_BYTES {
        return Err(invalid("empty or oversized envelope"));
    }
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("envelope");
    let query = directory.path().join("request.der");
    fs::write(&source, envelope)?;
    let mut command = Command::new(openssl);
    command
        .args(["ts", "-query", "-data"])
        .arg(&source)
        .args(["-sha256", "-cert", "-out"])
        .arg(&query);
    if let Some(oid) = policy_oid {
        ObjectIdentifier::new(oid).map_err(invalid)?;
        command.arg("-tspolicy").arg(oid);
    }
    if !super::openssl_process::run(&mut command)?.status.success() {
        return Err(invalid("OpenSSL could not create a timestamp query"));
    }
    Ok(fs::read(query)?)
}
fn check_pem(pem: &str, kind: &str) -> Result<(), EvidenceError> {
    if pem.matches("-----BEGIN").count() != 1
        || pem.matches("-----END").count() != 1
        || !pem.trim().starts_with(&format!("-----BEGIN {kind}-----"))
        || !pem.trim().ends_with(&format!("-----END {kind}-----"))
    {
        return Err(invalid(format!("expected exactly one {kind} PEM object")));
    }
    if kind == "CERTIFICATE" {
        Certificate::from_pem(pem).map_err(invalid)?;
    } else {
        let (_, der) = x509_cert::der::pem::decode_vec(pem.as_bytes()).map_err(invalid)?;
        CertificateList::from_der(&der).map_err(invalid)?;
    }
    Ok(())
}
// Decode RFC 3161 fields using the DER library's typed ASN.1 primitives, not OpenSSL text output.
fn sequence(value: &Any) -> Result<Vec<Any>, EvidenceError> {
    value.decode_as::<Vec<Any>>().map_err(invalid)
}
fn imprint(value: &Any, expected: &[u8]) -> Result<(), EvidenceError> {
    let fields = sequence(value)?;
    if fields.len() != 2 {
        return Err(invalid("invalid message imprint"));
    }
    let algorithm = sequence(&fields[0])?;
    if !(1..=2).contains(&algorithm.len())
        || algorithm[0]
            .decode_as::<ObjectIdentifier>()
            .map_err(invalid)?
            .to_string()
            != "2.16.840.1.101.3.4.2.1"
        || (algorithm.len() == 2 && algorithm[1].tag() != Tag::Null)
        || fields[1]
            .decode_as::<OctetString>()
            .map_err(invalid)?
            .as_bytes()
            != expected
    {
        return Err(invalid(
            "timestamp imprint does not match the complete signature envelope",
        ));
    }
    Ok(())
}
struct TimestampInfo {
    time: i64,
    policy_oid: String,
    nonce: Option<Uint>,
}
fn timestamp_info(response: &[u8], expected: &[u8]) -> Result<TimestampInfo, EvidenceError> {
    let fields = Vec::<Any>::from_der(response).map_err(invalid)?;
    if fields.len() != 2 {
        return Err(invalid("expected granted TimeStampResp with token"));
    }
    let status = sequence(&fields[0])?;
    if status.is_empty() || status[0].decode_as::<u32>().map_err(invalid)? > 1 {
        return Err(invalid("TSA did not grant the request"));
    }
    let token = fields[1].decode_as::<ContentInfo>().map_err(invalid)?;
    if token.content_type.to_string() != "1.2.840.113549.1.7.2" {
        return Err(invalid("timestamp token is not SignedData"));
    }
    let signed = token.content.decode_as::<SignedData>().map_err(invalid)?;
    if signed.encap_content_info.econtent_type.to_string() != "1.2.840.113549.1.9.16.1.4" {
        return Err(invalid("timestamp token does not contain TSTInfo"));
    }
    let payload = signed
        .encap_content_info
        .econtent
        .ok_or_else(|| invalid("TSTInfo missing"))?
        .decode_as::<OctetString>()
        .map_err(invalid)?;
    let info = Vec::<Any>::from_der(payload.as_bytes()).map_err(invalid)?;
    if info.len() < 5 || info[0].decode_as::<u32>().map_err(invalid)? != 1 {
        return Err(invalid("unsupported TSTInfo version"));
    }
    let policy_oid = info[1]
        .decode_as::<ObjectIdentifier>()
        .map_err(invalid)?
        .to_string();
    imprint(&info[2], expected)?;
    info[3].decode_as::<Uint>().map_err(invalid)?;
    // Existing DER GeneralizedTime intentionally enforces the demo's whole UTC second profile.
    let time = info[4]
        .decode_as::<GeneralizedTime>()
        .map_err(invalid)?
        .to_unix_duration()
        .as_secs();
    let time = i64::try_from(time).map_err(invalid)?;
    let mut position = 5;
    if info
        .get(position)
        .is_some_and(|value| value.tag() == Tag::Sequence)
    {
        let accuracy = sequence(&info[position])?;
        if accuracy
            .iter()
            .any(|value| value.value().iter().any(|byte| *byte != 0))
        {
            return Err(invalid(
                "demo timestamp profile does not support nonzero accuracy",
            ));
        }
        position += 1;
    }
    if info
        .get(position)
        .is_some_and(|value| value.tag() == Tag::Boolean)
    {
        info[position].decode_as::<bool>().map_err(invalid)?;
        position += 1;
    }
    let nonce = if info
        .get(position)
        .is_some_and(|value| value.tag() == Tag::Integer)
    {
        let value = info[position].decode_as::<Uint>().map_err(invalid)?;
        position += 1;
        Some(value)
    } else {
        None
    };
    // Optional tsa [0] and extensions [1] are also checked by OpenSSL's RFC 3161 decoder.
    for (ordinal, value) in info[position..].iter().enumerate() {
        if ordinal > 1 || !matches!(value.tag(), Tag::ContextSpecific { .. }) {
            return Err(invalid("invalid trailing TSTInfo field"));
        }
    }
    Ok(TimestampInfo {
        time,
        policy_oid,
        nonce,
    })
}
fn check_query(query: &[u8], info: &TimestampInfo, expected: &[u8]) -> Result<(), EvidenceError> {
    let fields = Vec::<Any>::from_der(query).map_err(invalid)?;
    if fields.len() < 2 || fields[0].decode_as::<u32>().map_err(invalid)? != 1 {
        return Err(invalid("invalid timestamp query"));
    }
    imprint(&fields[1], expected)?;
    let mut position = 2;
    if fields
        .get(position)
        .is_some_and(|value| value.tag() == Tag::ObjectIdentifier)
    {
        if fields[position]
            .decode_as::<ObjectIdentifier>()
            .map_err(invalid)?
            .to_string()
            != info.policy_oid
        {
            return Err(invalid("timestamp response changed requested policy"));
        }
        position += 1;
    }
    let nonce = fields
        .get(position)
        .filter(|value| value.tag() == Tag::Integer)
        .ok_or_else(|| invalid("stored timestamp query nonce missing"))?
        .decode_as::<Uint>()
        .map_err(invalid)?;
    if info.nonce.as_ref() != Some(&nonce) {
        return Err(invalid("stored timestamp query nonce differs"));
    }
    Ok(())
}

pub fn verify_timestamp(
    envelope: &[u8],
    response: Option<&[u8]>,
    query: Option<&[u8]>,
    policy: &TsaPolicy,
) -> Result<TimestampReport, EvidenceError> {
    verify_timestamp_with("openssl", envelope, response, query, policy)
}
pub fn verify_timestamp_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    envelope: &[u8],
    response: Option<&[u8]>,
    query: Option<&[u8]>,
    policy: &TsaPolicy,
) -> Result<TimestampReport, EvidenceError> {
    let openssl = openssl.as_ref();
    let mut report = TimestampReport {
        status: CheckStatus::Unknown,
        trusted_time: None,
        policy_oid: None,
        details: vec![],
    };
    let Some(response) = response else {
        report
            .details
            .push("timestamp evidence not supplied".into());
        return Ok(report);
    };
    report.status = CheckStatus::Invalid;
    let inspected = (|| -> Result<TimestampInfo, EvidenceError> {
        if policy.validation_time < 0
            || envelope.is_empty()
            || envelope.len() > MAX_EVIDENCE_BYTES
            || response.len() > MAX_EVIDENCE_BYTES
            || query.is_some_and(|v| v.len() > MAX_EVIDENCE_BYTES)
        {
            return Err(invalid("invalid time or timestamp input size"));
        }
        if policy.roots_pem.is_empty() || policy.allowed_policy_oids.is_empty() {
            return Err(invalid("external TSA roots and allowed policies required"));
        }
        for root in &policy.roots_pem {
            check_pem(root, "CERTIFICATE")?;
            if !Certificate::from_pem(root)
                .map_err(invalid)?
                .tbs_certificate
                .get::<BasicConstraints>()
                .map_err(invalid)?
                .is_some_and(|(_, bc)| bc.ca)
            {
                return Err(invalid("TSA root must be a CA"));
            }
        }
        for value in &policy.intermediates_pem {
            check_pem(value, "CERTIFICATE")?;
        }
        for value in &policy.crls_pem {
            check_pem(value, "X509 CRL")?;
        }
        let expected = Sha256::digest(envelope);
        let info = timestamp_info(response, &expected)?;
        if !policy.allowed_policy_oids.contains(&info.policy_oid) {
            return Err(invalid("TSA policy not locally authorized"));
        }
        if info.time > policy.validation_time {
            return Err(invalid("timestamp is in the future"));
        }
        if let Some(query) = query {
            check_query(query, &info, &expected)?;
        }
        Ok(info)
    })();
    let info = match inspected {
        Ok(info) => info,
        Err(error) => {
            report.details.push(error.to_string());
            return Ok(report);
        }
    };
    let directory = tempfile::tempdir()?;
    let roots = directory.path().join("roots.pem");
    let response_path = directory.path().join("response.der");
    let query_path = directory.path().join("query.der");
    let chain = directory.path().join("chain.pem");
    let empty = directory.path().join("empty-store");
    fs::create_dir(&empty)?;
    fs::write(
        &roots,
        policy
            .roots_pem
            .iter()
            .chain(policy.crls_pem.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    )?;
    fs::write(&response_path, response)?;
    if let Some(query) = query {
        fs::write(&query_path, query)?;
    }
    if !policy.intermediates_pem.is_empty() {
        fs::write(&chain, policy.intermediates_pem.join("\n"))?;
    }
    let run = |crls: bool| -> Result<bool, EvidenceError> {
        let mut command = Command::new(openssl);
        command
            .args(["ts", "-verify", "-in"])
            .arg(&response_path)
            .arg("-CAfile")
            .arg(&roots)
            .arg("-CApath")
            .arg(&empty)
            .arg("-CAstore")
            // OpenSSL 3.0 eagerly loads the store and rejects an empty directory.
            // Reuse the explicit policy bundle so default roots stay excluded.
            .arg(&roots)
            .arg("-attime")
            .arg(info.time.to_string())
            .args(["-x509_strict", "-check_ss_sig"]);
        if !policy.intermediates_pem.is_empty() {
            command.arg("-untrusted").arg(&chain);
        }
        if query.is_some() {
            command.arg("-queryfile").arg(&query_path);
        } else {
            command
                .arg("-digest")
                .arg(hex::encode(Sha256::digest(envelope)));
        }
        if crls {
            command.arg("-crl_check_all");
        }
        Ok(super::openssl_process::run(&mut command)?.status.success())
    };
    if !run(false)? {
        report
            .details
            .push("TSA signature, request, or certification path is invalid".into());
        return Ok(report);
    }
    if policy.crls_pem.is_empty() {
        report.status = CheckStatus::Unknown;
        report
            .details
            .push("TSA revocation evidence not supplied".into());
        return Ok(report);
    }
    if !run(true)? {
        report
            .details
            .push("TSA full-chain revocation evidence invalid, incomplete, or stale".into());
        return Ok(report);
    }
    report.status = CheckStatus::Valid;
    report.trusted_time = Some(info.time);
    report.policy_oid = Some(info.policy_oid);
    Ok(report)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBundle {
    pub version: u32,
    pub manifest_sha256: String,
    pub envelope_sha256: String,
    pub policy_id: String,
    pub policy_sha256: String,
    pub timestamp_response_hex: Option<String>,
    pub timestamp_query_hex: Option<String>,
    #[serde(default)]
    pub signer_intermediates_pem: Vec<String>,
    #[serde(default)]
    pub signer_crls_pem: Vec<String>,
    #[serde(default)]
    pub tsa_intermediates_pem: Vec<String>,
    #[serde(default)]
    pub tsa_crls_pem: Vec<String>,
}
impl EvidenceBundle {
    pub fn new(manifest: &[u8], envelope: &[u8], policy_id: String, policy_bytes: &[u8]) -> Self {
        Self {
            version: 1,
            manifest_sha256: hex::encode(Sha256::digest(manifest)),
            envelope_sha256: hex::encode(Sha256::digest(envelope)),
            policy_id,
            policy_sha256: hex::encode(Sha256::digest(policy_bytes)),
            timestamp_response_hex: None,
            timestamp_query_hex: None,
            signer_intermediates_pem: vec![],
            signer_crls_pem: vec![],
            tsa_intermediates_pem: vec![],
            tsa_crls_pem: vec![],
        }
    }
    pub fn to_json(&self) -> Result<Vec<u8>, EvidenceError> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn from_json(data: &[u8]) -> Result<Self, EvidenceError> {
        if data.len() > MAX_EVIDENCE_BYTES {
            return Err(invalid("evidence bundle too large"));
        }
        Ok(serde_json::from_slice(data)?)
    }
    /// Compare independently supplied inputs. This never authorizes archive-selected policy.
    pub fn check_binding(
        &self,
        manifest: &[u8],
        envelope: &[u8],
        policy_id: &str,
        policy_bytes: &[u8],
    ) -> Result<(), EvidenceError> {
        if self.version != 1
            || policy_id.is_empty()
            || self.policy_id != policy_id
            || self.manifest_sha256 != hex::encode(Sha256::digest(manifest))
            || self.envelope_sha256 != hex::encode(Sha256::digest(envelope))
            || self.policy_sha256 != hex::encode(Sha256::digest(policy_bytes))
        {
            return Err(invalid(
                "evidence binding differs from manifest, envelope, or local policy",
            ));
        }
        for encoded in [&self.timestamp_response_hex, &self.timestamp_query_hex]
            .into_iter()
            .flatten()
        {
            if encoded.is_empty()
                || encoded.len() > 2 * MAX_EVIDENCE_BYTES
                || hex::encode(hex::decode(encoded).map_err(invalid)?) != *encoded
            {
                return Err(invalid(
                    "timestamp evidence must be canonical nonempty hexadecimal DER",
                ));
            }
        }
        for pem in self
            .signer_intermediates_pem
            .iter()
            .chain(self.tsa_intermediates_pem.iter())
        {
            check_pem(pem, "CERTIFICATE")?;
        }
        for pem in self.signer_crls_pem.iter().chain(self.tsa_crls_pem.iter()) {
            check_pem(pem, "X509 CRL")?;
        }
        Ok(())
    }
}
/// Identity eligibility at verified timestamp time. Signature and coverage require separate verification.
#[allow(clippy::too_many_arguments)]
pub fn verify_historical_signer(
    bundle: &EvidenceBundle,
    manifest: &[u8],
    envelope: &[u8],
    policy_id: &str,
    policy_bytes: &[u8],
    tsa_policy: &TsaPolicy,
    signer_policy: &TrustPolicy,
    leaf: &str,
    role: SignerRole,
    identity: &str,
) -> Result<(TimestampReport, Option<CertificateReport>), EvidenceError> {
    verify_historical_signer_with(
        "openssl",
        bundle,
        manifest,
        envelope,
        policy_id,
        policy_bytes,
        tsa_policy,
        signer_policy,
        leaf,
        role,
        identity,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn verify_historical_signer_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    bundle: &EvidenceBundle,
    manifest: &[u8],
    envelope: &[u8],
    policy_id: &str,
    policy_bytes: &[u8],
    tsa_policy: &TsaPolicy,
    signer_policy: &TrustPolicy,
    leaf: &str,
    role: SignerRole,
    identity: &str,
) -> Result<(TimestampReport, Option<CertificateReport>), EvidenceError> {
    let openssl = openssl.as_ref();
    bundle.check_binding(manifest, envelope, policy_id, policy_bytes)?;
    let response = bundle
        .timestamp_response_hex
        .as_ref()
        .map(hex::decode)
        .transpose()
        .map_err(invalid)?;
    let query = bundle
        .timestamp_query_hex
        .as_ref()
        .map(hex::decode)
        .transpose()
        .map_err(invalid)?;
    let tsa = TsaPolicy {
        crls_pem: bundle.tsa_crls_pem.clone(),
        intermediates_pem: bundle.tsa_intermediates_pem.clone(),
        ..tsa_policy.clone()
    };
    let timestamp = verify_timestamp_with(
        openssl,
        envelope,
        response.as_deref(),
        query.as_deref(),
        &tsa,
    )?;
    if !timestamp.accepted() {
        return Ok((timestamp, None));
    }
    let time = timestamp
        .trusted_time
        .ok_or_else(|| invalid("timestamp lacks a trusted time"))?;
    let historic = TrustPolicy {
        validation_time: time,
        crls_pem: bundle.signer_crls_pem.clone(),
        ..signer_policy.clone()
    };
    let cert = verify_certificate_with(
        openssl,
        leaf,
        &bundle.signer_intermediates_pem,
        &historic,
        role,
        identity,
    )?;
    Ok((timestamp, Some(cert)))
}

#[cfg(test)]
mod tests {
    use super::super::trust::{Authorization, certificate_fingerprint};
    use super::*;
    fn raw(name: &str) -> Vec<u8> {
        fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test/data/provenance/timestamps")
                .join(name),
        )
        .unwrap()
    }
    fn pem(name: &str) -> String {
        String::from_utf8(raw(name)).unwrap()
    }
    fn metadata() -> serde_json::Value {
        serde_json::from_slice(&raw("metadata.json")).unwrap()
    }
    fn policy() -> TsaPolicy {
        TsaPolicy {
            roots_pem: vec![pem("tsa-root.pem")],
            validation_time: metadata()["validation_time"].as_i64().unwrap(),
            allowed_policy_oids: vec![metadata()["tsa_policy_oid"].as_str().unwrap().into()],
            crls_pem: vec![pem("tsa-root.crl.pem")],
            intermediates_pem: vec![],
        }
    }
    fn check(policy: &TsaPolicy) -> TimestampReport {
        verify_timestamp(
            &raw("envelope.json"),
            Some(&raw("response.tsr")),
            Some(&raw("request.tsq")),
            policy,
        )
        .unwrap()
    }
    fn bundle() -> EvidenceBundle {
        let mut bundle = EvidenceBundle::new(
            b"manifest",
            &raw("envelope.json"),
            "demo-policy-v1".into(),
            b"local policy",
        );
        bundle.timestamp_response_hex = Some(hex::encode(raw("response.tsr")));
        bundle.timestamp_query_hex = Some(hex::encode(raw("request.tsq")));
        bundle.tsa_crls_pem = vec![pem("tsa-root.crl.pem")];
        bundle.signer_crls_pem = vec![pem("tsa-root.crl.pem")];
        bundle
    }
    #[test]
    fn genuine_timestamp_verifies_at_historical_tsa_time() {
        let report = check(&policy());
        assert!(report.accepted(), "{:?}", report.details);
        assert_eq!(report.trusted_time, metadata()["generated_at"].as_i64());
        assert!(
            verify_timestamp(
                &raw("envelope.json"),
                Some(&raw("response.tsr")),
                None,
                &policy()
            )
            .unwrap()
            .accepted()
        );
    }
    #[test]
    fn invalid_tsa_paths_policies_future_times_and_crls_supply_no_trusted_time() {
        let mut variants = vec![];
        let mut p = policy();
        p.roots_pem.clear();
        variants.push(p);
        let mut p = policy();
        p.roots_pem = vec![pem("historical-signer.pem")];
        variants.push(p);
        let mut p = policy();
        p.allowed_policy_oids = vec!["1.2.3.4".into()];
        variants.push(p);
        let mut p = policy();
        p.crls_pem = vec![pem("stale.crl.pem")];
        variants.push(p);
        let mut p = policy();
        p.crls_pem = vec![pem("tsa-root.crl.pem") + &pem("tsa-root.pem")];
        variants.push(p);
        let mut p = policy();
        p.validation_time = metadata()["generated_at"].as_i64().unwrap() - 1;
        variants.push(p);
        for p in variants {
            let r = check(&p);
            assert_eq!(r.status, CheckStatus::Invalid);
            assert!(r.trusted_time.is_none());
        }
    }
    #[test]
    fn missing_tsa_and_revocation_evidence_remain_unknown() {
        let mut p = policy();
        p.crls_pem.clear();
        let report = check(&p);
        assert_eq!(report.status, CheckStatus::Unknown);
        assert!(report.trusted_time.is_none());
        let report = verify_timestamp(&raw("envelope.json"), None, None, &policy()).unwrap();
        assert_eq!(report.status, CheckStatus::Unknown);
        assert!(report.trusted_time.is_none());
    }
    #[test]
    fn changed_imprint_signature_nonce_and_malformed_tokens_fail() {
        let mut envelope = raw("envelope.json");
        envelope.push(b' ');
        assert!(
            !verify_timestamp(
                &envelope,
                Some(&raw("response.tsr")),
                Some(&raw("request.tsq")),
                &policy()
            )
            .unwrap()
            .accepted()
        );
        let mut response = raw("response.tsr");
        let last = response.len() - 1;
        response[last] ^= 1;
        assert!(
            !verify_timestamp(
                &raw("envelope.json"),
                Some(&response),
                Some(&raw("request.tsq")),
                &policy()
            )
            .unwrap()
            .accepted()
        );
        let query = create_timestamp_query(&raw("envelope.json"), None).unwrap();
        assert!(
            !verify_timestamp(
                &raw("envelope.json"),
                Some(&raw("response.tsr")),
                Some(&query),
                &policy()
            )
            .unwrap()
            .accepted()
        );
        for response in [b"".as_slice(), b"garbage", b"\x30\x00"] {
            assert!(
                verify_timestamp(&raw("envelope.json"), Some(response), None, &policy())
                    .unwrap()
                    .trusted_time
                    .is_none()
            );
        }
    }
    #[test]
    fn evidence_binding_roundtrips_and_rejects_substitution() {
        let b = EvidenceBundle::from_json(&bundle().to_json().unwrap()).unwrap();
        b.check_binding(
            b"manifest",
            &raw("envelope.json"),
            "demo-policy-v1",
            b"local policy",
        )
        .unwrap();
        assert!(
            b.check_binding(
                b"changed",
                &raw("envelope.json"),
                "demo-policy-v1",
                b"local policy"
            )
            .is_err()
        );
        assert!(
            b.check_binding(b"manifest", b"changed", "demo-policy-v1", b"local policy")
                .is_err()
        );
        assert!(
            b.check_binding(b"manifest", &raw("envelope.json"), "wrong", b"local policy")
                .is_err()
        );
        assert!(
            b.check_binding(
                b"manifest",
                &raw("envelope.json"),
                "demo-policy-v1",
                b"wrong"
            )
            .is_err()
        );
        assert!(EvidenceBundle::from_json(br#"{"version":1,"version":1}"#).is_err());
    }
    #[test]
    fn expired_signer_eligible_at_verified_time_with_external_grants_only() {
        let leaf = pem("historical-signer.pem");
        let mut signer = TrustPolicy {
            roots_pem: vec![pem("tsa-root.pem")],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&leaf).unwrap(),
                role: SignerRole::Acquisition,
                identity: "instrument-1".into(),
                required_eku: None,
            }],
            validation_time: policy().validation_time,
            crls_pem: vec![pem("tsa-root.crl.pem")],
            require_pqc_chain: false,
        };
        assert!(
            !verify_certificate_with(
                "openssl",
                &leaf,
                &[],
                &signer,
                SignerRole::Acquisition,
                "instrument-1"
            )
            .unwrap()
            .accepted()
        );
        let (timestamp, certificate) = verify_historical_signer(
            &bundle(),
            b"manifest",
            &raw("envelope.json"),
            "demo-policy-v1",
            b"local policy",
            &policy(),
            &signer,
            &leaf,
            SignerRole::Acquisition,
            "instrument-1",
        )
        .unwrap();
        assert!(timestamp.accepted());
        assert!(certificate.unwrap().accepted());
        signer.authorizations.clear();
        let (_, certificate) = verify_historical_signer(
            &bundle(),
            b"manifest",
            &raw("envelope.json"),
            "demo-policy-v1",
            b"local policy",
            &policy(),
            &signer,
            &leaf,
            SignerRole::Acquisition,
            "instrument-1",
        )
        .unwrap();
        assert!(!certificate.unwrap().accepted());
    }
}
