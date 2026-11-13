//! Experimental v2 CMS signatures of canonical manifest bytes.
//!
//! Each signer produces detached DER SignedData with signed attributes.
//! ECDSA uses P-256/SHA-256; ML-DSA-65 uses a SHA-512 content digest (RFC 9882).
//! OpenSSL 3.5+ is required for ML-DSA. No private key material is exported from
//! a PKCS#11 provider. [`OpenSslBackend::verify`] establishes cryptographic
//! integrity only; its returned signers still need certificate trust, role,
//! revocation and coverage validation.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x509_cert::{
    Certificate,
    der::{
        Decode, DecodePem, Encode,
        asn1::{ObjectIdentifier, OctetString},
    },
};

const MAX_CMS_BYTES: usize = 1024 * 1024;
const DOMAIN: &[u8] = b"mzPeak-cms-signature-v2\0";
const MAX_CERTIFICATE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("Invalid signing input: {0}")]
    Invalid(&'static str),
    #[error("OpenSSL operation failed (check backend, key and profile)")]
    Backend,
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid signature envelope: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Algorithm {
    #[serde(rename = "ecdsa-p256-sha256")]
    EcdsaP256Sha256,
    #[serde(rename = "ml-dsa-65")]
    MlDsa65,
}

impl Algorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EcdsaP256Sha256 => "ecdsa-p256-sha256",
            Self::MlDsa65 => "ml-dsa-65",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignaturePolicy {
    Ecdsa,
    Pqc,
    Both,
}

impl SignaturePolicy {
    fn tag(self) -> u8 {
        match self {
            Self::Ecdsa => 1,
            Self::Pqc => 2,
            Self::Both => 3,
        }
    }

    fn satisfied_by(self, algorithms: &[Algorithm]) -> bool {
        match self {
            Self::Ecdsa => algorithms.contains(&Algorithm::EcdsaP256Sha256),
            Self::Pqc => algorithms.contains(&Algorithm::MlDsa65),
            Self::Both => {
                algorithms.contains(&Algorithm::EcdsaP256Sha256)
                    && algorithms.contains(&Algorithm::MlDsa65)
            }
        }
    }
}

/// Local provider configuration, never read from an untrusted archive.
#[derive(Debug, Clone)]
pub enum KeyProvider {
    Software {
        key_path: PathBuf,
        passphrase_file: Option<PathBuf>,
    },
    /// OpenSSL pkcs11-provider configuration selects OpenSC and a protected PIN
    /// file. Inline `pin-value` URI parameters are rejected.
    Pkcs11 {
        key_uri: String,
        config_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct SigningIdentity {
    pub algorithm: Algorithm,
    pub key: KeyProvider,
    pub certificate_pem: String,
    pub intermediates_pem: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachedSignature {
    pub algorithm: Algorithm,
    pub certificate_pem: String,
    pub intermediates_pem: Vec<String>,
    pub cms_der_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEnvelope {
    pub version: u32,
    pub policy: SignaturePolicy,
    pub signatures: Vec<DetachedSignature>,
}

impl SignatureEnvelope {
    /// Sorted-key ASCII JSON including actual signatures, for timestamping.
    /// A valid envelope contains only ASCII profile names, PEM, and hex.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SigningError> {
        let value = serde_json::to_value(self)?;
        fn encode(
            value: &serde_json::Value,
            output: &mut Vec<u8>,
        ) -> Result<(), serde_json::Error> {
            match value {
                serde_json::Value::Object(fields) => {
                    output.push(b'{');
                    let mut fields: Vec<_> = fields.iter().collect();
                    fields.sort_by(|a, b| a.0.cmp(b.0));
                    for (i, (key, value)) in fields.iter().enumerate() {
                        if i != 0 {
                            output.push(b',');
                        }
                        output.extend(serde_json::to_vec(key)?);
                        output.push(b':');
                        encode(value, output)?;
                    }
                    output.push(b'}');
                }
                serde_json::Value::Array(values) => {
                    output.push(b'[');
                    for (i, value) in values.iter().enumerate() {
                        if i != 0 {
                            output.push(b',');
                        }
                        encode(value, output)?;
                    }
                    output.push(b']');
                }
                _ => output.extend(serde_json::to_vec(value)?),
            }
            Ok(())
        }
        let mut bytes = Vec::new();
        encode(&value, &mut bytes)?;
        if !bytes.is_ascii() {
            return Err(SigningError::Invalid("envelope fields must be ASCII"));
        }
        Ok(bytes)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, SigningError> {
        Self::from_json(data)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, SigningError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_json(data: &[u8]) -> Result<Self, SigningError> {
        let envelope: Self = serde_json::from_slice(data)?;
        if envelope.version != 2 {
            return Err(SigningError::Invalid("unsupported envelope version"));
        }
        Ok(envelope)
    }
}

/// A valid signature; this does **not** establish trust or signer authorization.
#[derive(Debug, Clone)]
pub struct CryptographicSigner {
    pub algorithm: Algorithm,
    pub certificate_pem: String,
    pub intermediates_pem: Vec<String>,
    pub certificate_sha256: String,
}

fn certificate(certificate_pem: &str) -> Result<Certificate, SigningError> {
    let stripped = certificate_pem.trim();
    if !certificate_pem.is_ascii()
        || certificate_pem.len() > MAX_CERTIFICATE_BYTES
        || !stripped.starts_with("-----BEGIN CERTIFICATE-----")
        || !stripped.ends_with("-----END CERTIFICATE-----")
        || certificate_pem.matches("-----BEGIN").count() != 1
        || certificate_pem.matches("-----END").count() != 1
    {
        return Err(SigningError::Invalid(
            "expected exactly one PEM certificate",
        ));
    }
    Certificate::from_pem(stripped).map_err(|_| SigningError::Invalid("invalid PEM certificate"))
}

fn certificate_der(certificate_pem: &str, algorithm: Algorithm) -> Result<Vec<u8>, SigningError> {
    let cert = certificate(certificate_pem)?;
    let identifier = &cert.tbs_certificate.subject_public_key_info.algorithm;
    match algorithm {
        Algorithm::EcdsaP256Sha256 => {
            let curve = identifier
                .parameters
                .as_ref()
                .ok_or(SigningError::Invalid("missing EC parameters"))?
                .decode_as::<ObjectIdentifier>()
                .map_err(|_| SigningError::Invalid("invalid EC parameters"))?;
            if identifier.oid != ObjectIdentifier::new_unwrap("1.2.840.10045.2.1")
                || curve != ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7")
            {
                return Err(SigningError::Invalid(
                    "certificate key is not named-curve P-256",
                ));
            }
        }
        Algorithm::MlDsa65 => {
            if identifier.oid != ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.3.18")
                || identifier.parameters.is_some()
            {
                return Err(SigningError::Invalid(
                    "certificate key is not ML-DSA-65 with absent parameters",
                ));
            }
        }
    }
    cert.to_der()
        .map_err(|_| SigningError::Invalid("invalid certificate DER"))
}

fn signer_key(algorithm: Algorithm, pem: &str) -> Result<(Algorithm, [u8; 32]), SigningError> {
    Ok((
        algorithm,
        Sha256::digest(certificate_der(pem, algorithm)?).into(),
    ))
}

/// The CMS v2 content: domain, one-byte policy, u32 signer count, sorted
/// entries (u32 algorithm-name length, ASCII name, 32-byte leaf DER SHA-256),
/// then u64 manifest length and exact manifest bytes. Integers are big endian.
/// This binds the complete signer plan, preventing stripping or relabeling.
pub fn signing_payload(
    manifest: &[u8],
    envelope: &SignatureEnvelope,
) -> Result<Vec<u8>, SigningError> {
    if envelope.version != 2 {
        return Err(SigningError::Invalid("unsupported envelope version"));
    }
    let signer_count = u32::try_from(envelope.signatures.len())
        .map_err(|_| SigningError::Invalid("signer count exceeds u32 framing"))?;
    if signer_count == 0 {
        return Err(SigningError::Invalid("expected a nonempty signer set"));
    }
    let algorithms: Vec<_> = envelope.signatures.iter().map(|s| s.algorithm).collect();
    let keys = envelope
        .signatures
        .iter()
        .map(|s| signer_key(s.algorithm, &s.certificate_pem))
        .collect::<Result<Vec<_>, _>>()?;
    if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SigningError::Invalid(
            "signers must be unique and sorted by algorithm and certificate digest",
        ));
    }
    if !envelope.policy.satisfied_by(&algorithms) {
        return Err(SigningError::Invalid(
            "signatures do not satisfy declared policy",
        ));
    }
    let mut output = Vec::from(DOMAIN);
    output.push(envelope.policy.tag());
    output.extend_from_slice(&signer_count.to_be_bytes());
    for (signature, (_, certificate_hash)) in envelope.signatures.iter().zip(&keys) {
        let name = signature.algorithm.as_str().as_bytes();
        if signature.intermediates_pem.len() > 8 {
            return Err(SigningError::Invalid("too many intermediate certificates"));
        }
        for intermediate in &signature.intermediates_pem {
            certificate(intermediate)?;
        }
        output.extend_from_slice(&(name.len() as u32).to_be_bytes());
        output.extend_from_slice(name);
        output.extend_from_slice(certificate_hash);
    }
    output.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    output.extend_from_slice(manifest);
    Ok(output)
}

/// Validate CMS structure and descriptor binding before cryptographic verification.
fn cms_der(signature: &DetachedSignature) -> Result<Vec<u8>, SigningError> {
    use cms::{
        cert::CertificateChoices,
        content_info::{CmsVersion, ContentInfo},
        signed_data::{SignedData, SignerIdentifier},
    };
    use std::collections::BTreeSet;
    let bad = || SigningError::Invalid("invalid CMS signature/profile");
    if signature.cms_der_hex.len() > 2 * MAX_CMS_BYTES {
        return Err(bad());
    }
    let raw = hex::decode(&signature.cms_der_hex).map_err(|_| bad())?;
    if hex::encode(&raw) != signature.cms_der_hex {
        return Err(bad());
    }
    let info = ContentInfo::from_der(&raw).map_err(|_| bad())?;
    if info.content_type != ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2")
        || info.to_der().map_err(|_| bad())? != raw
    {
        return Err(bad());
    }
    let data: SignedData = info.content.decode_as().map_err(|_| bad())?;
    if data.to_der().map_err(|_| bad())? != info.content.to_der().map_err(|_| bad())?
        || data.version != CmsVersion::V1
        || data.encap_content_info.econtent_type
            != ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1")
        || data.encap_content_info.econtent.is_some()
        || data.crls.is_some()
        || data.signer_infos.0.len() != 1
    {
        return Err(bad());
    }
    let signer = data.signer_infos.0.iter().next().ok_or_else(bad)?;
    let leaf = certificate(&signature.certificate_pem)?;
    certificate_der(&signature.certificate_pem, signature.algorithm)?;
    let SignerIdentifier::IssuerAndSerialNumber(sid) = &signer.sid else {
        return Err(bad());
    };
    if signer.version != CmsVersion::V1
        || sid.issuer != leaf.tbs_certificate.issuer
        || sid.serial_number != leaf.tbs_certificate.serial_number
        || signer.unsigned_attrs.is_some()
    {
        return Err(bad());
    }
    let mut expected = BTreeSet::new();
    expected.insert(leaf.to_der().map_err(|_| bad())?);
    for pem in &signature.intermediates_pem {
        if !expected.insert(certificate(pem)?.to_der().map_err(|_| bad())?) {
            return Err(bad());
        }
    }
    let certs = data.certificates.as_ref().ok_or_else(bad)?;
    let mut actual = BTreeSet::new();
    for item in certs.0.iter() {
        let CertificateChoices::Certificate(cert) = item else {
            return Err(bad());
        };
        if !actual.insert(cert.to_der().map_err(|_| bad())?) {
            return Err(bad());
        }
    }
    if expected != actual {
        return Err(bad());
    }
    let (digest_oid, signature_oid, digest_size) = match signature.algorithm {
        Algorithm::EcdsaP256Sha256 => ("2.16.840.1.101.3.4.2.1", "1.2.840.10045.4.3.2", 32),
        Algorithm::MlDsa65 => ("2.16.840.1.101.3.4.2.3", "2.16.840.1.101.3.4.3.18", 64),
    };
    let digest_oid = ObjectIdentifier::new_unwrap(digest_oid);
    for alg in data
        .digest_algorithms
        .iter()
        .chain(std::iter::once(&signer.digest_alg))
    {
        if alg.oid != digest_oid
            || alg
                .parameters
                .as_ref()
                .is_some_and(|p| p.to_der().ok().as_deref() != Some(&[5, 0]))
        {
            return Err(bad());
        }
    }
    if data.digest_algorithms.len() != 1
        || signer.signature_algorithm.oid != ObjectIdentifier::new_unwrap(signature_oid)
        || signer.signature_algorithm.parameters.is_some()
    {
        return Err(bad());
    }
    let attrs = signer.signed_attrs.as_ref().ok_or_else(bad)?;
    let mut seen = BTreeSet::new();
    for attr in attrs.iter() {
        if !seen.insert(attr.oid) || attr.values.len() != 1 {
            return Err(bad());
        }
        let value = attr.values.iter().next().ok_or_else(bad)?;
        match attr.oid.to_string().as_str() {
            "1.2.840.113549.1.9.3" => {
                if value.decode_as::<ObjectIdentifier>().map_err(|_| bad())?
                    != data.encap_content_info.econtent_type
                {
                    return Err(bad());
                }
            }
            "1.2.840.113549.1.9.4" => {
                if value
                    .decode_as::<OctetString>()
                    .map_err(|_| bad())?
                    .as_bytes()
                    .len()
                    != digest_size
                {
                    return Err(bad());
                }
            }
            "1.2.840.113549.1.9.5" => {
                x509_cert::time::Time::from_der(&value.to_der().map_err(|_| bad())?)
                    .map_err(|_| bad())?;
            }
            _ => return Err(bad()),
        }
    }
    if !seen.contains(&ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3"))
        || !seen.contains(&ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4"))
        || match signature.algorithm {
            Algorithm::MlDsa65 => signer.signature.as_bytes().len() != 3309,
            Algorithm::EcdsaP256Sha256 => !(8..=72).contains(&signer.signature.as_bytes().len()),
        }
    {
        return Err(bad());
    }
    Ok(raw)
}

/// Optional prototype backend. Executable and provider configuration are trusted
/// local settings, not archive fields. Process arguments never invoke a shell.
#[derive(Debug, Clone)]
pub struct OpenSslBackend {
    pub executable: PathBuf,
    /// Deadline for each OpenSSL child, including its provider operations.
    /// A multi-signer sign/verify call can invoke several children.
    pub timeout: Duration,
}

impl Default for OpenSslBackend {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("openssl"),
            timeout: Duration::from_secs(30),
        }
    }
}

impl OpenSslBackend {
    fn wait_for_exit(&self, child: &mut Child) -> Result<(), SigningError> {
        let started = Instant::now();
        loop {
            let remaining = self.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                // Always reap the direct child, including an exit racing kill.
                let _ = child.kill();
                let _ = child.wait();
                return Err(SigningError::Backend);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(SigningError::Backend)
                    };
                }
                Ok(None) => std::thread::sleep(remaining.min(Duration::from_millis(10))),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SigningError::Backend);
                }
            }
        }
    }

    fn execute(
        &self,
        command: &mut Command,
        capture_output: bool,
    ) -> Result<Vec<u8>, SigningError> {
        // An anonymous file avoids full stdout/stderr pipes blocking the child
        // while we enforce its deadline. Only signing needs captured CMS bytes;
        // verification discards the potentially large recovered content.
        let mut output = capture_output.then(tempfile::tempfile).transpose()?;
        command
            .stdin(Stdio::null())
            .stdout(match &output {
                Some(file) => Stdio::from(file.try_clone()?),
                None => Stdio::null(),
            })
            // Provider diagnostics can contain PIN/configuration/key references.
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|_| SigningError::Backend)?;
        self.wait_for_exit(&mut child)?;
        let mut bytes = Vec::new();
        if let Some(file) = &mut output {
            file.seek(SeekFrom::Start(0))?;
            file.take(MAX_CMS_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_CMS_BYTES {
                return Err(SigningError::Backend);
            }
        }
        Ok(bytes)
    }

    fn run(&self, args: &[String], capture_output: bool) -> Result<Vec<u8>, SigningError> {
        self.execute(Command::new(&self.executable).args(args), capture_output)
    }

    fn verify_one(
        &self,
        payload: &std::path::Path,
        signature: &DetachedSignature,
        directory: &std::path::Path,
    ) -> Result<(), SigningError> {
        let certificate_path = directory.join("certificate.pem");
        let signature_path = directory.join("signature.der");
        fs::write(&certificate_path, &signature.certificate_pem)?;
        fs::write(&signature_path, cms_der(signature)?)?;
        // Disable only path validation, handled separately by our trust layer.
        // Force the exact bound leaf; never select an unrelated embedded key.
        self.run(
            &[
                "cms".into(),
                "-verify".into(),
                "-binary".into(),
                "-inform".into(),
                "DER".into(),
                "-in".into(),
                signature_path.to_string_lossy().into_owned(),
                "-content".into(),
                payload.to_string_lossy().into_owned(),
                "-noverify".into(),
                "-nointern".into(),
                "-certfile".into(),
                certificate_path.to_string_lossy().into_owned(),
            ],
            false,
        )?;
        Ok(())
    }

    pub fn sign(
        &self,
        manifest: &[u8],
        identities: &[SigningIdentity],
        policy: SignaturePolicy,
    ) -> Result<SignatureEnvelope, SigningError> {
        let mut ordered = identities
            .iter()
            .map(|identity| {
                Ok((
                    signer_key(identity.algorithm, &identity.certificate_pem)?,
                    identity,
                ))
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        ordered.sort_by_key(|(key, _)| *key);
        let identities: Vec<_> = ordered.into_iter().map(|(_, identity)| identity).collect();
        let mut envelope = SignatureEnvelope {
            version: 2,
            policy,
            signatures: identities
                .iter()
                .map(|identity| DetachedSignature {
                    algorithm: identity.algorithm,
                    certificate_pem: identity.certificate_pem.clone(),
                    intermediates_pem: identity.intermediates_pem.clone(),
                    cms_der_hex: String::new(),
                })
                .collect(),
        };
        let payload = signing_payload(manifest, &envelope)?;
        let work = tempfile::tempdir()?;
        let payload_path = work.path().join("payload.bin");
        fs::write(&payload_path, payload)?;
        for (identity, signature) in identities.iter().zip(envelope.signatures.iter_mut()) {
            let certificate_path = work.path().join("signer.pem");
            fs::write(&certificate_path, &identity.certificate_pem)?;
            let mut args = vec![
                "cms".into(),
                "-sign".into(),
                "-binary".into(),
                "-nosmimecap".into(),
                "-outform".into(),
                "DER".into(),
                "-in".into(),
                payload_path.to_string_lossy().into_owned(),
                "-signer".into(),
                certificate_path.to_string_lossy().into_owned(),
                "-md".into(),
                match identity.algorithm {
                    Algorithm::EcdsaP256Sha256 => "sha256",
                    Algorithm::MlDsa65 => "sha512",
                }
                .into(),
            ];
            if !identity.intermediates_pem.is_empty() {
                let chain_path = work.path().join("chain.pem");
                fs::write(&chain_path, identity.intermediates_pem.join("\n"))?;
                args.extend([
                    "-certfile".into(),
                    chain_path.to_string_lossy().into_owned(),
                ]);
            }
            match &identity.key {
                KeyProvider::Software {
                    key_path,
                    passphrase_file,
                } => {
                    args.extend(["-inkey".into(), key_path.to_string_lossy().into_owned()]);
                    if let Some(path) = passphrase_file {
                        args.extend(["-passin".into(), format!("file:{}", path.display())]);
                    }
                }
                KeyProvider::Pkcs11 {
                    key_uri,
                    config_path,
                } => {
                    if !key_uri.starts_with("pkcs11:")
                        || key_uri.to_ascii_lowercase().contains("pin-value")
                    {
                        return Err(SigningError::Invalid(
                            "expected PKCS#11 URI without inline PIN",
                        ));
                    }
                    if identity.algorithm != Algorithm::EcdsaP256Sha256 {
                        return Err(SigningError::Invalid(
                            "PKCS#11 profile currently supports ECDSA only",
                        ));
                    }
                    args.extend([
                        "-inkey".into(),
                        key_uri.clone(),
                        "-config".into(),
                        config_path.to_string_lossy().into_owned(),
                    ]);
                }
            }
            signature.cms_der_hex = hex::encode(self.run(&args, true)?);
            // Also establishes that the supplied leaf matches the actual signing key.
            self.verify_one(&payload_path, signature, work.path())?;
        }
        Ok(envelope)
    }

    pub fn verify(
        &self,
        manifest: &[u8],
        envelope: &SignatureEnvelope,
        required_policy: SignaturePolicy,
    ) -> Result<Vec<CryptographicSigner>, SigningError> {
        let payload = signing_payload(manifest, envelope)?;
        let algorithms: Vec<_> = envelope.signatures.iter().map(|s| s.algorithm).collect();
        if !required_policy.satisfied_by(&algorithms) {
            return Err(SigningError::Invalid(
                "signatures do not satisfy relying-party policy",
            ));
        }
        let work = tempfile::tempdir()?;
        let payload_path = work.path().join("payload.bin");
        fs::write(&payload_path, payload)?;
        let mut verified = Vec::new();
        for signature in &envelope.signatures {
            self.verify_one(&payload_path, signature, work.path())?;
            verified.push(CryptographicSigner {
                algorithm: signature.algorithm,
                certificate_pem: signature.certificate_pem.clone(),
                intermediates_pem: signature.intermediates_pem.clone(),
                certificate_sha256: hex::encode(Sha256::digest(certificate_der(
                    &signature.certificate_pem,
                    signature.algorithm,
                )?)),
            });
        }
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend_child(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "provenance::signing::tests::backend_process_fixture",
                "--nocapture",
            ])
            .env("MZPEAK_SIGNING_CHILD_MODE", mode);
        command
    }

    // A child copy of this test executable is a portable backend fixture. It
    // needs neither a shell nor OpenSSL, including on Windows CI.
    #[test]
    fn backend_process_fixture() {
        use std::io::Write;
        let Ok(mode) = std::env::var("MZPEAK_SIGNING_CHILD_MODE") else {
            return;
        };
        match mode.as_str() {
            "stall" => {
                fs::write(
                    std::env::var_os("MZPEAK_SIGNING_CHILD_READY").unwrap(),
                    b"ready",
                )
                .unwrap();
                std::thread::sleep(Duration::from_secs(10));
            }
            "fail" => {
                eprintln!("pkcs11:object=private-key /secret/provider.cnf /secret/key.pem");
                std::process::exit(7);
            }
            "overflow" => {
                std::io::stdout()
                    .write_all(&vec![b'x'; MAX_CMS_BYTES + 1])
                    .unwrap();
                std::io::stderr()
                    .write_all(&vec![b'x'; MAX_CMS_BYTES + 1])
                    .unwrap();
            }
            "success" => println!("backend-output"),
            _ => panic!("unknown backend fixture mode"),
        }
    }

    #[test]
    fn backend_timeout_terminates_and_reaps_child() {
        let work = tempfile::tempdir().unwrap();
        let ready = work.path().join("ready");
        let mut child = backend_child("stall")
            .env("MZPEAK_SIGNING_CHILD_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let startup = Instant::now();
        while !ready.exists() && startup.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("backend fixture did not start");
        }
        let backend = OpenSslBackend {
            timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let started = Instant::now();
        let error = backend.wait_for_exit(&mut child).unwrap_err();
        assert!(matches!(error, SigningError::Backend));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!child.try_wait().unwrap().unwrap().success());
        // On Linux, a reaped process has no /proc entry (a zombie still does).
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{}", child.id())).exists());
    }

    #[test]
    fn backend_errors_are_redacted_and_output_modes_are_bounded() {
        let backend = OpenSslBackend::default();
        let error = backend
            .execute(&mut backend_child("fail"), true)
            .unwrap_err();
        assert!(matches!(error, SigningError::Backend));
        for message in [error.to_string(), format!("{error:?}")] {
            assert!(!message.contains("private-key"));
            assert!(!message.contains("/secret/"));
        }
        assert!(std::error::Error::source(&error).is_none());
        let captured = backend
            .execute(&mut backend_child("success"), true)
            .unwrap();
        assert!(
            String::from_utf8(captured)
                .unwrap()
                .contains("backend-output")
        );
        assert!(
            backend
                .execute(&mut backend_child("success"), false)
                .unwrap()
                .is_empty()
        );
        // Both streams exceed a typical pipe buffer. The child must still exit,
        // and captured CMS output must respect the existing per-object bound.
        assert!(matches!(
            backend.execute(&mut backend_child("overflow"), true),
            Err(SigningError::Backend)
        ));
        assert!(
            backend
                .execute(&mut backend_child("overflow"), false)
                .unwrap()
                .is_empty()
        );
    }

    fn available(pqc: bool) -> bool {
        let result = Command::new("openssl")
            .args(["list", "-signature-algorithms"])
            .output();
        let available = result.is_ok_and(|output| {
            output.status.success()
                && (!pqc || String::from_utf8_lossy(&output.stdout).contains("ML-DSA-65"))
        });
        if !available {
            assert_ne!(
                std::env::var("MZPEAK_REQUIRE_OPENSSL_PQC").as_deref(),
                Ok("1"),
                "PQC qualification requires the OpenSSL 3.5+ backend"
            );
            eprintln!("OpenSSL backend unavailable (ML-DSA needs OpenSSL 3.5+)");
        }
        available
    }

    fn run(args: &[&str]) {
        let result = Command::new("openssl")
            .args(args)
            .output()
            .expect("openssl executable");
        assert!(
            result.status.success(),
            "openssl failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn identity(work: &std::path::Path, algorithm: Algorithm, stem: &str) -> SigningIdentity {
        let key = work.join(format!("{stem}.key"));
        let cert = work.join(format!("{stem}.crt"));
        let key_name = key.to_str().unwrap();
        let cert_name = cert.to_str().unwrap();
        match algorithm {
            Algorithm::EcdsaP256Sha256 => run(&[
                "genpkey",
                "-algorithm",
                "EC",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-out",
                key_name,
            ]),
            Algorithm::MlDsa65 => run(&["genpkey", "-algorithm", "ML-DSA-65", "-out", key_name]),
        }
        run(&[
            "req",
            "-new",
            "-x509",
            "-key",
            key_name,
            "-out",
            cert_name,
            "-days",
            "1",
            "-subj",
            "/CN=mzPeak signing test",
        ]);
        SigningIdentity {
            algorithm,
            key: KeyProvider::Software {
                key_path: key,
                passphrase_file: None,
            },
            certificate_pem: fs::read_to_string(cert).unwrap(),
            intermediates_pem: Vec::new(),
        }
    }

    fn fixture() -> (Vec<u8>, SignatureEnvelope) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/data/provenance/signing");
        (
            fs::read(base.join("manifest.json")).unwrap(),
            SignatureEnvelope::from_bytes(&fs::read(base.join("envelope.json")).unwrap()).unwrap(),
        )
    }

    #[test]
    fn shared_python_transcript_and_envelope_canonical_bytes() {
        let (manifest, envelope) = fixture();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/data/provenance/signing");
        assert_eq!(
            hex::encode(Sha256::digest(
                signing_payload(&manifest, &envelope).unwrap()
            )),
            fs::read_to_string(base.join("transcript.sha256"))
                .unwrap()
                .trim()
        );
        assert_eq!(
            hex::encode(Sha256::digest(envelope.canonical_bytes().unwrap())),
            fs::read_to_string(base.join("canonical-envelope.sha256"))
                .unwrap()
                .trim()
        );
        assert_eq!(
            envelope.to_json().unwrap(),
            fs::read(base.join("envelope.json")).unwrap()
        );
        if available(true) {
            assert_eq!(
                OpenSslBackend::default()
                    .verify(&manifest, &envelope, SignaturePolicy::Both)
                    .unwrap()
                    .len(),
                2
            );
        }
    }

    #[test]
    fn real_ecdsa_roundtrip_and_mutated_manifest() {
        if !available(false) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let identity = identity(work.path(), Algorithm::EcdsaP256Sha256, "classical");
        let backend = OpenSslBackend::default();
        let envelope = backend
            .sign(b"manifest", &[identity], SignaturePolicy::Ecdsa)
            .unwrap();
        assert_eq!(
            backend
                .verify(b"manifest", &envelope, SignaturePolicy::Ecdsa)
                .unwrap()
                .len(),
            1
        );
        assert!(
            backend
                .verify(b"tampered", &envelope, SignaturePolicy::Ecdsa)
                .is_err()
        );
        assert!(
            backend
                .verify(b"manifest", &envelope, SignaturePolicy::Both)
                .is_err()
        );
    }

    #[test]
    fn real_mldsa_and_both_roundtrip() {
        if !available(true) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let pq = identity(work.path(), Algorithm::MlDsa65, "pqc");
        let ec = identity(work.path(), Algorithm::EcdsaP256Sha256, "ec");
        let backend = OpenSslBackend::default();
        let envelope = backend
            .sign(b"manifest", &[pq.clone()], SignaturePolicy::Pqc)
            .unwrap();
        assert_eq!(
            backend
                .verify(b"manifest", &envelope, SignaturePolicy::Pqc)
                .unwrap()
                .len(),
            1
        );
        let envelope = backend
            .sign(b"manifest", &[pq, ec], SignaturePolicy::Both)
            .unwrap();
        assert_eq!(
            backend
                .verify(b"manifest", &envelope, SignaturePolicy::Both)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn stripping_downgrading_or_bad_pq_signature_fails() {
        let (manifest, mut envelope) = fixture();
        let backend = OpenSslBackend::default();
        envelope.signatures.pop();
        assert!(
            backend
                .verify(&manifest, &envelope, SignaturePolicy::Ecdsa)
                .is_err()
        );
        if !available(false) {
            return;
        }
        envelope.policy = SignaturePolicy::Ecdsa;
        assert!(
            backend
                .verify(&manifest, &envelope, SignaturePolicy::Ecdsa)
                .is_err()
        );
        let (_, mut envelope) = fixture();
        envelope.signatures[1].cms_der_hex = "00".repeat(3309);
        assert!(
            backend
                .verify(&manifest, &envelope, SignaturePolicy::Ecdsa)
                .is_err()
        );
    }

    #[test]
    fn key_mismatch_and_wrong_curve_rejected() {
        if !available(false) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let mut one = identity(work.path(), Algorithm::EcdsaP256Sha256, "one");
        let other = identity(work.path(), Algorithm::EcdsaP256Sha256, "other");
        one.key = other.key;
        let backend = OpenSslBackend::default();
        assert!(
            backend
                .sign(b"manifest", &[one], SignaturePolicy::Ecdsa)
                .is_err()
        );
        let key = work.path().join("p384.key");
        let cert = work.path().join("p384.crt");
        run(&[
            "genpkey",
            "-algorithm",
            "EC",
            "-pkeyopt",
            "ec_paramgen_curve:P-384",
            "-out",
            key.to_str().unwrap(),
        ]);
        run(&[
            "req",
            "-new",
            "-x509",
            "-key",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
            "-days",
            "1",
            "-subj",
            "/CN=wrong curve",
        ]);
        let wrong = SigningIdentity {
            algorithm: Algorithm::EcdsaP256Sha256,
            key: KeyProvider::Software {
                key_path: key,
                passphrase_file: None,
            },
            certificate_pem: fs::read_to_string(cert).unwrap(),
            intermediates_pem: Vec::new(),
        };
        assert!(
            backend
                .sign(b"manifest", &[wrong], SignaturePolicy::Ecdsa)
                .is_err()
        );
    }

    #[test]
    fn malformed_envelopes_and_relabeling_rejected() {
        let (manifest, envelope) = fixture();
        let mut changed = envelope.clone();
        changed.version = 1;
        assert!(signing_payload(&manifest, &changed).is_err());
        changed = envelope.clone();
        changed.signatures.reverse();
        assert!(signing_payload(&manifest, &changed).is_err());
        changed.signatures.clear();
        assert!(signing_payload(&manifest, &changed).is_err());
        changed = envelope.clone();
        changed.signatures[0].algorithm = Algorithm::MlDsa65;
        assert!(signing_payload(&manifest, &changed).is_err());
        assert!(
            SignatureEnvelope::from_bytes(
                br#"{"version":1,"version":1,"policy":"ecdsa","signatures":[]}"#
            )
            .is_err()
        );
        assert!(
            SignatureEnvelope::from_bytes(
                br#"{"version":1,"policy":"ecdsa","signatures":[],"extension":true}"#
            )
            .is_err()
        );
        assert!(
            SignatureEnvelope::from_bytes(br#"{"version":true,"policy":"ecdsa","signatures":[]}"#)
                .is_err()
        );
    }

    #[test]
    fn shared_invalid_cms_cases_are_rejected() {
        let (manifest, envelope) = fixture();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test/data/provenance/signing/cms-invalid.json");
        let cases: std::collections::BTreeMap<String, String> =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        for (name, value) in cases {
            let mut changed = envelope.clone();
            changed.signatures[0].cms_der_hex = value;
            assert!(
                OpenSslBackend::default()
                    .verify(&manifest, &changed, SignaturePolicy::Ecdsa)
                    .is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn same_algorithm_signers_bind_the_complete_plan() {
        if !available(true) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let identities = vec![
            identity(work.path(), Algorithm::EcdsaP256Sha256, "ec1"),
            identity(work.path(), Algorithm::EcdsaP256Sha256, "ec2"),
            identity(work.path(), Algorithm::MlDsa65, "pq1"),
            identity(work.path(), Algorithm::MlDsa65, "pq2"),
        ];
        let backend = OpenSslBackend::default();
        for (policy, selected) in [
            (SignaturePolicy::Ecdsa, &identities[..2]),
            (SignaturePolicy::Pqc, &identities[2..]),
            (SignaturePolicy::Both, &identities[..]),
        ] {
            let payload = b"multiple same-algorithm signers";
            let envelope = backend.sign(payload, selected, policy).unwrap();
            let mut reverse = selected.to_vec();
            reverse.reverse();
            let other = backend.sign(payload, &reverse, policy).unwrap();
            assert_eq!(
                signing_payload(payload, &envelope).unwrap(),
                signing_payload(payload, &other).unwrap()
            );
            assert_eq!(
                backend.verify(payload, &envelope, policy).unwrap().len(),
                selected.len()
            );
            let mut removed = envelope.clone();
            removed.signatures.pop();
            assert!(backend.verify(payload, &removed, policy).is_err());
            let mut reordered = envelope.clone();
            reordered.signatures.reverse();
            assert!(backend.verify(payload, &reordered, policy).is_err());
            let mut duplicate = envelope.clone();
            duplicate.signatures.push(envelope.signatures[0].clone());
            assert!(backend.verify(payload, &duplicate, policy).is_err());
            if policy != SignaturePolicy::Both {
                assert!(
                    backend
                        .verify(payload, &envelope, SignaturePolicy::Both)
                        .is_err()
                );
            }
            assert!(
                backend
                    .sign(payload, &[selected[0].clone(), selected[0].clone()], policy)
                    .is_err()
            );
        }
    }

    #[test]
    fn sign_and_verify_content_above_64_mib() {
        if !available(false) {
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let signer = identity(work.path(), Algorithm::EcdsaP256Sha256, "large");
        let payload = vec![b'x'; 64 * 1024 * 1024 + 1];
        let backend = OpenSslBackend::default();
        let envelope = backend
            .sign(&payload, &[signer], SignaturePolicy::Ecdsa)
            .unwrap();
        assert_eq!(
            backend
                .verify(&payload, &envelope, SignaturePolicy::Ecdsa)
                .unwrap()
                .len(),
            1
        );
        assert!(
            backend
                .verify(
                    &payload[..payload.len() - 1],
                    &envelope,
                    SignaturePolicy::Ecdsa
                )
                .is_err()
        );
    }

    #[test]
    fn cms_verification_discards_content_without_disabling_checks() {
        if !available(false) {
            return;
        }
        let (manifest, envelope) = fixture();
        let work = tempfile::tempdir().unwrap();
        let payload = work.path().join("payload.bin");
        let mut content = signing_payload(&manifest, &envelope).unwrap();
        fs::write(&payload, &content).unwrap();
        let backend = OpenSslBackend::default();
        backend
            .verify_one(&payload, &envelope.signatures[0], work.path())
            .unwrap();
        let mut names = fs::read_dir(work.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["certificate.pem", "payload.bin", "signature.der"].map(std::ffi::OsString::from)
        );
        *content.last_mut().unwrap() ^= 1;
        fs::write(&payload, content).unwrap();
        assert!(
            backend
                .verify_one(&payload, &envelope.signatures[0], work.path())
                .is_err()
        );
    }

    #[test]
    fn inline_pin_rejected_before_backend_execution() {
        let (_, envelope) = fixture();
        let identity = SigningIdentity {
            algorithm: Algorithm::EcdsaP256Sha256,
            key: KeyProvider::Pkcs11 {
                key_uri: "pkcs11:object=test?pin-value=secret".into(),
                config_path: "missing.cnf".into(),
            },
            certificate_pem: envelope.signatures[0].certificate_pem.clone(),
            intermediates_pem: Vec::new(),
        };
        let backend = OpenSslBackend {
            executable: "does-not-exist".into(),
            ..Default::default()
        };
        assert!(matches!(
            backend.sign(b"manifest", &[identity], SignaturePolicy::Ecdsa),
            Err(SigningError::Invalid(
                "expected PKCS#11 URI without inline PIN"
            ))
        ));
    }

    #[test]
    fn pkcs11_software_token_integration_when_configured() {
        let (Ok(config), Ok(uri), Ok(cert)) = (
            std::env::var("MZPEAK_TEST_PKCS11_CONFIG"),
            std::env::var("MZPEAK_TEST_PKCS11_URI"),
            std::env::var("MZPEAK_TEST_PKCS11_CERTIFICATE"),
        ) else {
            eprintln!("PKCS#11 qualification requires config, URI and certificate variables");
            return;
        };
        let identity = SigningIdentity {
            algorithm: Algorithm::EcdsaP256Sha256,
            key: KeyProvider::Pkcs11 {
                key_uri: uri,
                config_path: config.into(),
            },
            certificate_pem: fs::read_to_string(cert).unwrap(),
            intermediates_pem: Vec::new(),
        };
        let backend = OpenSslBackend::default();
        let envelope = backend
            .sign(b"PKCS11 qualification", &[identity], SignaturePolicy::Ecdsa)
            .unwrap();
        assert_eq!(
            backend
                .verify(b"PKCS11 qualification", &envelope, SignaturePolicy::Ecdsa)
                .unwrap()
                .len(),
            1
        );
    }
}
