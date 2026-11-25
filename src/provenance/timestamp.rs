//! RFC 3161 verification over the exact CMS signature-value octets.
use super::trust::CheckStatus;
use cms::{content_info::ContentInfo, signed_data::SignedData};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
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
fn invalid(message: impl ToString) -> super::Error {
    super::invalid(message.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimestampPolicy {
    pub roots_pem: Vec<String>,
    #[serde(default)]
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

fn create_timestamp_query_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    signature_value: &[u8],
    policy_oid: Option<&str>,
) -> Result<Vec<u8>, super::Error> {
    let openssl = openssl.as_ref();
    if signature_value.is_empty() || signature_value.len() > MAX_EVIDENCE_BYTES {
        return Err(invalid("empty or oversized signature_value"));
    }
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("signature_value");
    let query = directory.path().join("request.der");
    fs::write(&source, signature_value)?;
    let mut command = Command::new(openssl);
    command
        .args(["ts", "-query", "-data"])
        .arg(&source)
        .args(["-sha512", "-cert", "-out"])
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
fn check_pem(pem: &str, kind: &str) -> Result<(), super::Error> {
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
fn sequence(value: &Any) -> Result<Vec<Any>, super::Error> {
    value.decode_as::<Vec<Any>>().map_err(invalid)
}
fn imprint(value: &Any, expected: &[u8]) -> Result<(), super::Error> {
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
            != "2.16.840.1.101.3.4.2.3"
        || (algorithm.len() == 2 && algorithm[1].tag() != Tag::Null)
        || fields[1]
            .decode_as::<OctetString>()
            .map_err(invalid)?
            .as_bytes()
            != expected
    {
        return Err(invalid(
            "timestamp imprint does not match the signature value",
        ));
    }
    Ok(())
}
struct TimestampInfo {
    time: i64,
    policy_oid: String,
    nonce: Option<Uint>,
}
fn timestamp_info(token_der: &[u8], expected: &[u8]) -> Result<TimestampInfo, super::Error> {
    let token = ContentInfo::from_der(token_der).map_err(invalid)?;
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
fn check_query(query: &[u8], info: &TimestampInfo, expected: &[u8]) -> Result<(), super::Error> {
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

pub(super) fn verify_timestamp_with(
    openssl: impl AsRef<std::ffi::OsStr>,
    signature_value: &[u8],
    response: Option<&[u8]>,
    query: Option<&[u8]>,
    policy: &TimestampPolicy,
) -> Result<TimestampReport, super::Error> {
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
    let inspected = (|| -> Result<TimestampInfo, super::Error> {
        if policy.validation_time < 0
            || signature_value.is_empty()
            || signature_value.len() > MAX_EVIDENCE_BYTES
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
        let expected = Sha512::digest(signature_value);
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
    let response_path = directory.path().join("token.der");
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
    let run = |crls: bool| -> Result<bool, super::Error> {
        let mut command = Command::new(openssl);
        command
            .args(["ts", "-verify", "-token_in", "-in"])
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
                .arg(hex::encode(Sha512::digest(signature_value)));
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

/// One query per primary signature, keyed by SHA-512 of the signature value.
pub fn timestamp_requests(
    openssl: &std::ffi::OsStr,
    cms_der: &[u8],
    policy_oid: Option<&str>,
) -> super::Result<std::collections::BTreeMap<String, String>> {
    let data = super::cms::parse(cms_der)?;
    data.signer_infos
        .0
        .iter()
        .map(|info| {
            let value = info.signature.as_bytes();
            Ok((
                hex::encode(Sha512::digest(value)),
                hex::encode(create_timestamp_query_with(openssl, value, policy_oid)?),
            ))
        })
        .collect()
}

/// Match the response to its signature before editing the unsigned attributes.
pub(super) fn attach(
    data: &mut SignedData,
    response: &[u8],
    query: Option<&[u8]>,
) -> super::Result<()> {
    let fields = Vec::<Any>::from_der(response)?;
    if fields.len() != 2 {
        return Err(invalid("expected granted TimeStampResp with token"));
    }
    let fields_status = sequence(&fields[0])?;
    if fields_status.is_empty() || fields_status[0].decode_as::<u32>()? > 1 {
        return Err(invalid("TSA did not grant the request"));
    }
    let encoding = der::Encode::to_der(&fields[1])?;
    let mut signers: Vec<_> = data.signer_infos.0.iter().cloned().collect();
    let mut matched = false;
    for info in &mut signers {
        let imprint = Sha512::digest(info.signature.as_bytes());
        if let Ok(timestamp) = timestamp_info(&encoding, &imprint) {
            if matched {
                return Err(invalid("ambiguous timestamp target"));
            }
            if let Some(query) = query {
                check_query(query, &timestamp, &imprint)?;
            }
            if let Some(attrs) = &info.unsigned_attrs
                && let Some(previous) = super::cms::one(attrs, super::cms::TIMESTAMP)?
                && *previous != fields[1]
            {
                return Err(invalid("cannot replace an existing timestamp token"));
            }
            super::cms::add_unsigned(info, super::cms::TIMESTAMP, fields[1].clone())?;
            matched = true;
        }
    }
    if !matched {
        return Err(invalid("timestamp imprint matches no primary signature"));
    }
    data.signer_infos = cms::signed_data::SignerInfos::try_from(signers)?;
    Ok(())
}

pub fn timestamp_release(
    cms_der: &[u8],
    response: &[u8],
    query: Option<&[u8]>,
) -> super::Result<Vec<u8>> {
    let mut data = super::cms::parse(cms_der)?;
    super::cms::read_release(&data)?;
    attach(&mut data, response, query)?;
    super::cms::encode(&data)
}
