//! Experimental v1 detached signatures of canonical manifest bytes.
//!
//! This is a raw-signature envelope, **not CMS**. The OpenSSL subprocess backend
//! uses P-256/SHA-256 (DER signatures) and pure ML-DSA-65 with an empty context.
//! OpenSSL 3.5+ is required for ML-DSA. No private key material is exported from
//! a PKCS#11 provider. [`OpenSslBackend::verify`] establishes cryptographic
//! integrity only; its returned signers still need certificate trust, role,
//! revocation and coverage validation.

use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x509_cert::{
    Certificate,
    der::{DecodePem, Encode, asn1::ObjectIdentifier},
};

const DOMAIN: &[u8] = b"mzPeak-signature-v1\0";
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
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
    pub signature_hex: String,
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
        if envelope.version != 1 {
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

/// The v1 binary transcript: domain, one-byte policy, u32 signer count, sorted
/// entries (u32 algorithm-name length, ASCII name, 32-byte leaf DER SHA-256),
/// then u64 manifest length and exact manifest bytes. Integers are big endian.
/// This binds the complete signer plan, preventing stripping or relabeling.
pub fn signing_payload(
    manifest: &[u8],
    envelope: &SignatureEnvelope,
) -> Result<Vec<u8>, SigningError> {
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(SigningError::Invalid("manifest exceeds 64 MiB limit"));
    }
    if envelope.version != 1 {
        return Err(SigningError::Invalid("unsupported envelope version"));
    }
    if !(1..=2).contains(&envelope.signatures.len()) {
        return Err(SigningError::Invalid("expected one or two signatures"));
    }
    let algorithms: Vec<_> = envelope.signatures.iter().map(|s| s.algorithm).collect();
    if algorithms.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SigningError::Invalid(
            "signatures must have unique, sorted algorithms",
        ));
    }
    if !envelope.policy.satisfied_by(&algorithms) {
        return Err(SigningError::Invalid(
            "signatures do not satisfy declared policy",
        ));
    }
    let mut output = Vec::from(DOMAIN);
    output.push(envelope.policy.tag());
    output.extend_from_slice(&(algorithms.len() as u32).to_be_bytes());
    for signature in &envelope.signatures {
        let name = signature.algorithm.as_str().as_bytes();
        let der = certificate_der(&signature.certificate_pem, signature.algorithm)?;
        if signature.intermediates_pem.len() > 8 {
            return Err(SigningError::Invalid("too many intermediate certificates"));
        }
        for intermediate in &signature.intermediates_pem {
            certificate(intermediate)?;
        }
        output.extend_from_slice(&(name.len() as u32).to_be_bytes());
        output.extend_from_slice(name);
        output.extend_from_slice(&Sha256::digest(&der));
    }
    output.extend_from_slice(&(manifest.len() as u64).to_be_bytes());
    output.extend_from_slice(manifest);
    Ok(output)
}

/// Optional prototype backend. Executable and provider configuration are trusted
/// local settings, not archive fields. Process arguments never invoke a shell.
#[derive(Debug, Clone)]
pub struct OpenSslBackend {
    pub executable: PathBuf,
}

impl Default for OpenSslBackend {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("openssl"),
        }
    }
}

impl OpenSslBackend {
    fn run(&self, args: &[String]) -> Result<Vec<u8>, SigningError> {
        let output = Command::new(&self.executable)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            // Provider diagnostics may contain a PIN/configuration/key URI.
            return Err(SigningError::Backend);
        }
        Ok(output.stdout)
    }

    fn verify_one(
        &self,
        payload: &std::path::Path,
        signature: &DetachedSignature,
        directory: &std::path::Path,
    ) -> Result<(), SigningError> {
        let certificate_path = directory.join("certificate.pem");
        let public_key = directory.join("public.pem");
        let signature_path = directory.join("signature.bin");
        fs::write(&certificate_path, &signature.certificate_pem)?;
        let max_hex_length = match signature.algorithm {
            Algorithm::MlDsa65 => 6618,
            Algorithm::EcdsaP256Sha256 => 144,
        };
        if signature.signature_hex.len() > max_hex_length {
            return Err(SigningError::Invalid("signature exceeds profile length"));
        }
        let raw = hex::decode(&signature.signature_hex)
            .map_err(|_| SigningError::Invalid("invalid signature hex"))?;
        if hex::encode(&raw) != signature.signature_hex
            || match signature.algorithm {
                Algorithm::MlDsa65 => raw.len() != 3309,
                Algorithm::EcdsaP256Sha256 => !(8..=72).contains(&raw.len()),
            }
        {
            return Err(SigningError::Invalid(
                "invalid signature encoding or length",
            ));
        }
        fs::write(&signature_path, raw)?;
        fs::write(
            &public_key,
            self.run(&[
                "x509".into(),
                "-in".into(),
                certificate_path.to_string_lossy().into_owned(),
                "-pubkey".into(),
                "-noout".into(),
            ])?,
        )?;
        let mut args = vec![
            "pkeyutl".into(),
            "-verify".into(),
            "-pubin".into(),
            "-inkey".into(),
            public_key.to_string_lossy().into_owned(),
            "-rawin".into(),
            "-in".into(),
            payload.to_string_lossy().into_owned(),
            "-sigfile".into(),
            signature_path.to_string_lossy().into_owned(),
        ];
        if signature.algorithm == Algorithm::EcdsaP256Sha256 {
            args.extend(["-digest".into(), "sha256".into()]);
        }
        self.run(&args)?;
        Ok(())
    }

    pub fn sign(
        &self,
        manifest: &[u8],
        identities: &[SigningIdentity],
        policy: SignaturePolicy,
    ) -> Result<SignatureEnvelope, SigningError> {
        let mut identities: Vec<_> = identities.iter().collect();
        identities.sort_by_key(|identity| identity.algorithm);
        let mut envelope = SignatureEnvelope {
            version: 1,
            policy,
            signatures: identities
                .iter()
                .map(|identity| DetachedSignature {
                    algorithm: identity.algorithm,
                    certificate_pem: identity.certificate_pem.clone(),
                    intermediates_pem: identity.intermediates_pem.clone(),
                    signature_hex: String::new(),
                })
                .collect(),
        };
        let payload = signing_payload(manifest, &envelope)?;
        let work = tempfile::tempdir()?;
        let payload_path = work.path().join("payload.bin");
        fs::write(&payload_path, payload)?;
        for (identity, signature) in identities.iter().zip(envelope.signatures.iter_mut()) {
            let mut args = vec![
                "pkeyutl".into(),
                "-sign".into(),
                "-rawin".into(),
                "-in".into(),
                payload_path.to_string_lossy().into_owned(),
            ];
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
            if identity.algorithm == Algorithm::EcdsaP256Sha256 {
                args.extend(["-digest".into(), "sha256".into()]);
            }
            signature.signature_hex = hex::encode(self.run(&args)?);
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
        envelope.signatures[1].signature_hex = "00".repeat(3309);
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
        changed.version = 2;
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
        };
        assert!(matches!(
            backend.sign(b"manifest", &[identity], SignaturePolicy::Ecdsa),
            Err(SigningError::Invalid(
                "expected PKCS#11 URI without inline PIN"
            ))
        ));
    }

    #[test]
    fn complete_pqc_certificate_and_crl_path() {
        use crate::provenance::trust::{
            Authorization, CheckStatus, SignerRole, TrustPolicy, certificate_fingerprint,
            verify_certificate,
        };
        if !available(true) {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test/data/provenance/signing/pqc-trust");
        let pem = |name: &str| fs::read_to_string(base.join(name)).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(base.join("validation.json")).unwrap()).unwrap();
        let policy = |root: &str, issuer: &str, leaf: &str, strict: bool| TrustPolicy {
            roots_pem: vec![pem(&format!("{root}.pem"))],
            authorizations: vec![Authorization {
                certificate_sha256: certificate_fingerprint(&pem(&format!("{leaf}.pem"))).unwrap(),
                role: SignerRole::Acquisition,
                identity: "pqc-instrument-1".into(),
                required_eku: None,
            }],
            validation_time: metadata["validation_time"].as_i64().unwrap(),
            crls_pem: vec![
                pem(&format!("{root}.crl.pem")),
                pem(&format!("{issuer}.crl.pem")),
            ],
            require_pqc_chain: strict,
        };
        let check = |issuer: &str, leaf: &str, policy: &TrustPolicy| {
            verify_certificate(
                &pem(&format!("{leaf}.pem")),
                &[pem(&format!("{issuer}.pem"))],
                policy,
                SignerRole::Acquisition,
                "pqc-instrument-1",
            )
            .unwrap()
        };
        let mut strict = policy("root", "issuer", "leaf", true);
        let accepted = check("issuer", "leaf", &strict);
        assert!(accepted.accepted(), "{accepted:?}");
        assert_eq!(
            (accepted.path, accepted.authorization, accepted.revocation),
            (CheckStatus::Valid, CheckStatus::Valid, CheckStatus::Valid)
        );
        strict.crls_pem.clear();
        let absent = check("issuer", "leaf", &strict);
        assert!(!absent.accepted());
        assert_eq!(absent.revocation, CheckStatus::Unknown);
        for (root, issuer, leaf) in [
            ("classical-root", "mixed-issuer", "mixed-leaf"),
            ("root", "classical-issuer", "classical-issuer-leaf"),
        ] {
            let mut configured = policy(root, issuer, leaf, false);
            let ordinary = check(issuer, leaf, &configured);
            assert!(ordinary.accepted(), "{ordinary:?}");
            configured.require_pqc_chain = true;
            let rejected = check(issuer, leaf, &configured);
            assert!(!rejected.accepted());
            assert!(
                rejected
                    .details
                    .iter()
                    .any(|detail| detail.contains("PQC identity policy rejects"))
            );
        }
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
