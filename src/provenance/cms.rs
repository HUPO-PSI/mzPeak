use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::revocation::{RevocationInfoChoice, RevocationInfoChoices};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use const_oid::ObjectIdentifier;
use der::asn1::{Any, GeneralizedTime, OctetString, SetOfVec, UtcTime};
use der::{DateTime, Decode, Encode, EncodePem, Tag, Tagged};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use x509_cert::attr::{Attribute, Attributes};
use x509_cert::ext::pkix::SubjectKeyIdentifier;
use x509_cert::spki::AlgorithmIdentifierOwned;
use x509_cert::time::Time;
use x509_cert::{Certificate, crl::CertificateList};

use super::statement::{RELEASE_TYPE, Release, SignerBinding, SignerSet};
use super::{Result, digest_bytes, invalid, openssl_process};

pub(super) const DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const CONTENT_TYPE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
const MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
const SIGNING_TIME: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.5");
const SHA512: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.3");
const EC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
const P256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
const P521: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.35");
const ECDSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.4");
const ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const ML_DSA_44: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.3.17");
const ML_DSA_65: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.3.18");
const ML_DSA_87: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.3.19");
pub(super) const CONTENT_IDENTIFIER: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.7");
pub(super) const CONTENT_REFERENCE: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.10");
pub(super) const TIMESTAMP: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.14");
pub(super) const COUNTERSIGNATURE: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.6");

pub(super) fn reserved(oid: ObjectIdentifier) -> bool {
    [
        DATA,
        SIGNED_DATA,
        CONTENT_TYPE,
        MESSAGE_DIGEST,
        SIGNING_TIME,
        CONTENT_IDENTIFIER,
        CONTENT_REFERENCE,
        TIMESTAMP,
        COUNTERSIGNATURE,
    ]
    .contains(&oid)
        || oid.to_string().starts_with("1.2.840.113549.1.9.16.")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Algorithm {
    Ed25519,
    EcdsaP256Sha512,
    EcdsaP521Sha512,
    #[serde(rename = "ml-dsa-44")]
    MlDsa44,
    #[serde(rename = "ml-dsa-65")]
    MlDsa65,
    #[serde(rename = "ml-dsa-87")]
    MlDsa87,
}
impl Algorithm {
    fn oid(self) -> ObjectIdentifier {
        match self {
            Self::Ed25519 => ED25519,
            Self::EcdsaP256Sha512 | Self::EcdsaP521Sha512 => ECDSA,
            Self::MlDsa44 => ML_DSA_44,
            Self::MlDsa65 => ML_DSA_65,
            Self::MlDsa87 => ML_DSA_87,
        }
    }

    fn from_certificate(cert: &Certificate, signature_oid: ObjectIdentifier) -> Result<Self> {
        let key = &cert.tbs_certificate.subject_public_key_info.algorithm;
        if signature_oid == ECDSA {
            let curve = key
                .parameters
                .as_ref()
                .ok_or_else(|| invalid("EC certificate does not identify its curve"))?
                .decode_as::<ObjectIdentifier>()?;
            return match (key.oid, curve) {
                (EC_KEY, P256) => Ok(Self::EcdsaP256Sha512),
                (EC_KEY, P521) => Ok(Self::EcdsaP521Sha512),
                _ => Err(invalid("unsupported EC certificate curve")),
            };
        }
        if key.oid != signature_oid || key.parameters.is_some() {
            return Err(invalid(
                "certificate key does not match signature algorithm",
            ));
        }
        match signature_oid {
            ED25519 => Ok(Self::Ed25519),
            ML_DSA_44 => Ok(Self::MlDsa44),
            ML_DSA_65 => Ok(Self::MlDsa65),
            ML_DSA_87 => Ok(Self::MlDsa87),
            _ => Err(invalid("unsupported signature algorithm")),
        }
    }

    fn pqc(self) -> bool {
        matches!(self, Self::MlDsa44 | Self::MlDsa65 | Self::MlDsa87)
    }
}

/// The recipient selects a minimum independently of the signer set.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlgorithmPolicy {
    #[default]
    Any,
    Classical,
    Pqc,
    Both,
}
impl AlgorithmPolicy {
    pub(super) fn accepts(self, proofs: &[Proof]) -> bool {
        let classical = proofs.iter().any(|proof| !proof.algorithm.pqc());
        let pqc = proofs.iter().any(|proof| proof.algorithm.pqc());
        match self {
            Self::Any => !proofs.is_empty(),
            Self::Classical => classical,
            Self::Pqc => pqc,
            Self::Both => classical && pqc,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeyProvider {
    Software {
        path: PathBuf,
        passphrase_file: Option<PathBuf>,
    },
    Pkcs11 {
        uri: String,
        config: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningIdentity {
    pub algorithm: Algorithm,
    pub certificate_pem: String,
    #[serde(default)]
    pub intermediates_pem: Vec<String>,
    #[serde(default)]
    pub crls_pem: Vec<String>,
    pub key: KeyProvider,
}

pub(super) struct Identity<'a> {
    config: &'a SigningIdentity,
    certificate: Certificate,
    intermediates: Vec<Certificate>,
}

pub(super) fn identities(identities: &[SigningIdentity]) -> Result<Vec<Identity<'_>>> {
    if identities.is_empty() {
        return Err(invalid("at least one signing identity is required"));
    }
    identities
        .iter()
        .map(|config| {
            let certificate =
                super::trust::certificate(&config.certificate_pem).map_err(invalid)?;
            if Algorithm::from_certificate(&certificate, config.algorithm.oid())?
                != config.algorithm
            {
                return Err(invalid(
                    "signing certificate does not match the requested algorithm",
                ));
            }
            let intermediates = config
                .intermediates_pem
                .iter()
                .map(|pem| super::trust::certificate(pem).map_err(invalid))
                .collect::<Result<Vec<_>>>()?;
            Ok(Identity {
                config,
                certificate,
                intermediates,
            })
        })
        .collect()
}

fn algorithm_id(oid: ObjectIdentifier) -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid,
        parameters: None,
    }
}

fn binding(cert: &Certificate, alg: AlgorithmIdentifierOwned) -> Result<SignerBinding> {
    Ok(SignerBinding {
        fingerprint: OctetString::new(Sha512::digest(cert.to_der()?).to_vec())?,
        algorithm: alg,
    })
}

pub(super) fn signer_set(identities: &[Identity<'_>]) -> Result<SignerSet> {
    let mut set = SignerSet::default();
    for signer in identities {
        set.insert(binding(
            &signer.certificate,
            algorithm_id(signer.config.algorithm.oid()),
        )?)
        .map_err(|_| invalid("duplicate signing identity"))?;
    }
    Ok(set)
}

pub(super) fn attribute(oid: ObjectIdentifier, value: Any) -> Result<Attribute> {
    Ok(Attribute {
        oid,
        values: SetOfVec::try_from(vec![value])?,
    })
}

pub(super) fn one(attrs: &Attributes, oid: ObjectIdentifier) -> Result<Option<&Any>> {
    let mut list = attrs.iter().filter(|attribute| attribute.oid == oid);
    let Some(attr) = list.next() else {
        return Ok(None);
    };
    if list.next().is_some() || attr.values.len() != 1 {
        return Err(invalid("duplicated or multivalued CMS attribute"));
    }
    Ok(attr.values.get(0))
}

pub(super) fn encode(data: &SignedData) -> Result<Vec<u8>> {
    Ok(ContentInfo {
        content_type: SIGNED_DATA,
        content: Any::encode_from(data)?,
    }
    .to_der()?)
}

pub(super) fn parse(encoded: &[u8]) -> Result<SignedData> {
    let info = ContentInfo::from_der(encoded)?;
    if info.content_type != SIGNED_DATA {
        return Err(invalid("expected CMS SignedData"));
    }
    let data = info.content.decode_as::<SignedData>()?;
    if encode(&data)? != encoded
        || data.signer_infos.0.is_empty()
        || data.encap_content_info.econtent_type != DATA
        || data.digest_algorithms.len() != 1
        || !data.digest_algorithms.iter().all(is_sha512)
        || !matches!(data.version, CmsVersion::V1 | CmsVersion::V3)
    {
        return Err(invalid("invalid DER CMS SignedData or digest identifiers"));
    }
    let has_ski = data
        .signer_infos
        .0
        .iter()
        .any(|s| s.version == CmsVersion::V3);
    if (data.version == CmsVersion::V3) != has_ski {
        return Err(invalid("CMS version does not match its signer identifiers"));
    }
    Ok(data)
}

fn is_sha512(alg: &AlgorithmIdentifierOwned) -> bool {
    alg.oid == SHA512
        && alg
            .parameters
            .as_ref()
            .is_none_or(|p| p.tag() == Tag::Null && p.value().is_empty())
}

fn signature_command(openssl: &std::ffi::OsStr, algorithm: Algorithm) -> Command {
    let mut command = Command::new(openssl);
    command.args(["pkeyutl", "-rawin"]);
    if algorithm.oid() == ECDSA {
        command.args(["-digest", "sha512"]);
    }
    command
}

fn create_signature(
    openssl: &std::ffi::OsStr,
    signer: &Identity<'_>,
    attrs: &Attributes,
) -> Result<OctetString> {
    let work = tempfile::tempdir()?;
    let input = work.path().join("attrs.der");
    let output = work.path().join("signature.bin");
    fs::write(&input, attrs.to_der()?)?;
    let mut command = signature_command(openssl, signer.config.algorithm);
    command
        .arg("-sign")
        .arg("-in")
        .arg(input)
        .arg("-out")
        .arg(&output);
    match &signer.config.key {
        KeyProvider::Software {
            path,
            passphrase_file,
        } => {
            command.arg("-inkey").arg(path);
            if let Some(path) = passphrase_file {
                let mut value = std::ffi::OsString::from("file:");
                value.push(path);
                command.arg("-passin").arg(value);
            }
        }
        KeyProvider::Pkcs11 { uri, config } => {
            // Use protected provider/PIN configuration, not command arguments.
            // Encoded attribute names could hide pin-value from a literal check.
            let normalized = uri.to_ascii_lowercase();
            if !normalized.starts_with("pkcs11:")
                || normalized.contains("pin-value")
                || normalized.split([';', '?', '&']).any(|part| {
                    part.split_once('=')
                        .is_some_and(|(key, _)| key.contains('%'))
                })
            {
                return Err(invalid("use a PKCS #11 URI without an inline PIN"));
            }
            command.env("OPENSSL_CONF", config).arg("-inkey").arg(uri);
        }
    }
    if !openssl_process::run(&mut command)?.status.success() {
        return Err(invalid(
            "OpenSSL could not sign with the selected key provider",
        ));
    }
    Ok(OctetString::new(fs::read(output)?)?)
}

fn signer_info(
    openssl: &std::ffi::OsStr,
    signer: &Identity<'_>,
    digest: &[u8; 64],
    extra: &[Attribute],
    countersignature: bool,
) -> Result<SignerInfo> {
    let tbs = &signer.certificate.tbs_certificate;
    let date = DateTime::from_system_time(SystemTime::now())?;
    let time = if date.year() < 2050 {
        Time::UtcTime(UtcTime::from_date_time(date)?)
    } else {
        Time::GeneralTime(GeneralizedTime::from_date_time(date))
    };
    let mut attrs = vec![
        attribute(
            MESSAGE_DIGEST,
            Any::encode_from(&OctetString::new(digest.to_vec())?)?,
        )?,
        attribute(SIGNING_TIME, Any::encode_from(&time)?)?,
    ];
    if !countersignature {
        attrs.push(attribute(CONTENT_TYPE, Any::encode_from(&DATA)?)?);
    }
    attrs.extend_from_slice(extra);
    let attrs = SetOfVec::try_from(attrs)?;
    let signature = create_signature(openssl, signer, &attrs)?;
    Ok(SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: tbs.issuer.clone(),
            serial_number: tbs.serial_number.clone(),
        }),
        digest_alg: algorithm_id(SHA512),
        signed_attrs: Some(attrs),
        signature_algorithm: algorithm_id(signer.config.algorithm.oid()),
        signature,
        unsigned_attrs: None,
    })
}

pub(super) fn sign(
    openssl: &std::ffi::OsStr,
    identities: &[Identity<'_>],
    digest: &[u8; 64],
    attached: Option<&[u8]>,
    extra: &[Attribute],
) -> Result<Vec<u8>> {
    let mut certs = Vec::new();
    let mut seen = BTreeSet::new();
    let mut signers = Vec::new();
    for signer in identities {
        for cert in std::iter::once(&signer.certificate).chain(&signer.intermediates) {
            if seen.insert(cert.to_der()?) {
                certs.push(CertificateChoices::Certificate(cert.clone()));
            }
        }
        signers.push(signer_info(openssl, signer, digest, extra, false)?);
    }
    let mut revocations = SetOfVec::default();
    for pem in identities
        .iter()
        .flat_map(|identity| &identity.config.crls_pem)
    {
        let (label, encoded) =
            der::pem::decode_vec(pem.as_bytes()).map_err(|error| invalid(error.to_string()))?;
        if label != "X509 CRL" {
            return Err(invalid("expected a PEM certificate revocation list"));
        }
        let choice = RevocationInfoChoice::Crl(CertificateList::from_der(&encoded)?);
        if !revocations.iter().any(|old| *old == choice) {
            revocations.insert(choice)?;
        }
    }
    let data = SignedData {
        version: CmsVersion::V1,
        digest_algorithms: SetOfVec::try_from(vec![algorithm_id(SHA512)])?,
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: DATA,
            econtent: attached
                .map(|value| Any::encode_from(&OctetString::new(value.to_vec())?))
                .transpose()?,
        },
        certificates: Some(CertificateSet::try_from(certs)?),
        crls: (!revocations.is_empty()).then_some(RevocationInfoChoices(revocations)),
        signer_infos: SignerInfos::try_from(signers)?,
    };
    // Detect a misconfigured signer/key pair before persisting any archive.
    verify(openssl, &data, digest)?;
    encode(&data)
}

pub(super) struct Proof<'a> {
    pub info: &'a SignerInfo,
    pub certificate: &'a Certificate,
    pub algorithm: Algorithm,
}

fn certificate_matches(info: &SignerInfo, cert: &Certificate) -> bool {
    let tbs = &cert.tbs_certificate;
    match &info.sid {
        SignerIdentifier::IssuerAndSerialNumber(identifier) => {
            info.version == CmsVersion::V1
                && identifier.issuer == tbs.issuer
                && identifier.serial_number == tbs.serial_number
        }
        SignerIdentifier::SubjectKeyIdentifier(identifier) => {
            info.version == CmsVersion::V3
                && tbs
                    .get::<SubjectKeyIdentifier>()
                    .ok()
                    .flatten()
                    .is_some_and(|(_, ski)| ski == *identifier)
        }
    }
}

pub(super) fn certificates(data: &SignedData) -> Result<Vec<&Certificate>> {
    let certs = data
        .certificates
        .as_ref()
        .ok_or_else(|| invalid("CMS has no certificates"))?;
    certs
        .0
        .iter()
        .map(|c| match c {
            CertificateChoices::Certificate(c) => Ok(c),
            _ => Err(invalid("unsupported CMS certificate type")),
        })
        .collect()
}

pub(super) fn pem(cert: &Certificate) -> Result<String> {
    Ok(cert.to_pem(der::pem::LineEnding::LF)?)
}

pub(super) fn crls(data: &SignedData) -> Result<Vec<String>> {
    data.crls
        .iter()
        .flat_map(|set| set.0.iter())
        .map(|c| match c {
            RevocationInfoChoice::Crl(crl) => {
                der::pem::encode_string("X509 CRL", der::pem::LineEnding::LF, &crl.to_der()?)
                    .map_err(|error| invalid(error.to_string()))
            }
            _ => Err(invalid("unsupported CMS revocation info")),
        })
        .collect()
}

pub(super) fn attrs(info: &SignerInfo) -> Result<&Attributes> {
    info.signed_attrs
        .as_ref()
        .ok_or_else(|| invalid("CMS signer has no authenticated attributes"))
}

pub(super) fn message_digest(info: &SignerInfo) -> Result<[u8; 64]> {
    let attrs = attrs(info)?;
    let value = one(attrs, MESSAGE_DIGEST)?
        .ok_or_else(|| invalid("missing CMS message digest"))?
        .decode_as::<OctetString>()?;
    value
        .as_bytes()
        .try_into()
        .map_err(|_| invalid("CMS message digest must be SHA-512"))
}

pub(super) fn check_signature<'a>(
    openssl: &std::ffi::OsStr,
    info: &'a SignerInfo,
    certificates: &[&'a Certificate],
    digest: &[u8; 64],
    countersignature: bool,
) -> Result<Proof<'a>> {
    let matches = certificates
        .iter()
        .copied()
        .filter(|c| certificate_matches(info, c))
        .collect::<Vec<_>>();
    let [certificate] = matches.as_slice() else {
        return Err(invalid("CMS signer certificate missing or ambiguous"));
    };
    let algorithm = Algorithm::from_certificate(certificate, info.signature_algorithm.oid)?;
    if !is_sha512(&info.digest_alg)
        || info.signature_algorithm.parameters.is_some()
        || &message_digest(info)? != digest
    {
        return Err(invalid(
            "CMS signer digest or algorithm parameters do not match",
        ));
    }
    let attrs = attrs(info)?;
    let mut seen = BTreeSet::new();
    for attr in attrs.iter() {
        if !seen.insert(attr.oid) || attr.values.is_empty() {
            return Err(invalid("duplicate or empty CMS signed attribute"));
        }
    }
    one(attrs, SIGNING_TIME)?
        .map(|value| Time::from_der(&value.to_der()?))
        .transpose()?;
    let content = one(attrs, CONTENT_TYPE)?;
    if (countersignature && content.is_some())
        || (!countersignature
            && content
                .map(|v| v.decode_as::<ObjectIdentifier>())
                .transpose()?
                != Some(DATA))
    {
        return Err(invalid("invalid CMS signed content type"));
    }
    let work = tempfile::tempdir()?;
    let input = work.path().join("attrs.der");
    let signature = work.path().join("sig.bin");
    let cert = work.path().join("cert.pem");
    fs::write(&input, attrs.to_der()?)?;
    fs::write(&signature, info.signature.as_bytes())?;
    fs::write(&cert, pem(certificate)?)?;
    let checked = openssl_process::run(
        signature_command(openssl, algorithm)
            .args(["-verify", "-certin", "-inkey"])
            .arg(cert)
            .arg("-in")
            .arg(input)
            .arg("-sigfile")
            .arg(signature),
    )?;
    if !checked.status.success() {
        return Err(invalid("CMS signature verification failed"));
    }
    Ok(Proof {
        info,
        certificate,
        algorithm,
    })
}

pub(super) fn verify<'a>(
    openssl: &std::ffi::OsStr,
    data: &'a SignedData,
    digest: &[u8; 64],
) -> Result<Vec<Proof<'a>>> {
    let certs = certificates(data)?;
    let mut proofs = Vec::new();
    for signer in data.signer_infos.0.iter() {
        proofs.push(check_signature(openssl, signer, &certs, digest, false)?);
    }
    proof_set(&proofs)?;
    Ok(proofs)
}

pub(super) fn proof_set(proofs: &[Proof<'_>]) -> Result<SignerSet> {
    let mut set = SignerSet::default();
    for proof in proofs {
        set.insert(binding(
            proof.certificate,
            proof.info.signature_algorithm.clone(),
        )?)
        .map_err(|_| invalid("duplicate CMS signer pair"))?;
    }
    Ok(set)
}

pub(super) fn detached(data: &SignedData) -> Result<()> {
    if data.encap_content_info.econtent.is_some() {
        return Err(invalid("signature-table rows require detached CMS"));
    }
    Ok(())
}

pub(super) fn unsigned_values(data: &SignedData, oid: ObjectIdentifier) -> Result<Vec<Any>> {
    let mut result = Vec::new();
    for signer in data.signer_infos.0.iter() {
        if let Some(attrs) = &signer.unsigned_attrs {
            for attr in attrs.iter().filter(|attr| attr.oid == oid) {
                result.extend(attr.values.iter().cloned());
            }
        }
    }
    Ok(result)
}

pub(super) fn add_unsigned(info: &mut SignerInfo, oid: ObjectIdentifier, value: Any) -> Result<()> {
    let attrs = info.unsigned_attrs.take().unwrap_or_default();
    let mut now: Vec<_> = attrs.iter().filter(|a| a.oid != oid).cloned().collect();
    let mut old: Vec<_> = attrs
        .iter()
        .filter(|a| a.oid == oid)
        .flat_map(|a| a.values.iter())
        .cloned()
        .collect();
    if !old.contains(&value) {
        old.push(value);
    }
    now.push(Attribute {
        oid,
        values: SetOfVec::try_from(old)?,
    });
    info.unsigned_attrs = Some(SetOfVec::try_from(now)?);
    Ok(())
}

pub fn sign_release(
    openssl: &std::ffi::OsStr,
    identities_config: &[SigningIdentity],
    name: &str,
    version: &str,
    artifact_sha512: &str,
) -> Result<Vec<u8>> {
    if name.is_empty() || version.is_empty() {
        return Err(invalid("release name and version are required"));
    }
    let identities = identities(identities_config)?;
    let release = Release {
        statement_type: RELEASE_TYPE.to_owned(),
        version: 1,
        name: name.to_owned(),
        software_version: version.to_owned(),
        digest: OctetString::new(digest_bytes(artifact_sha512)?.to_vec())?,
        signers: signer_set(&identities)?,
    }
    .to_der()?;
    sign(
        openssl,
        &identities,
        &Sha512::digest(&release).into(),
        Some(&release),
        &[],
    )
}

pub(super) fn read_release(data: &SignedData) -> Result<(Release, [u8; 64])> {
    let value = data
        .encap_content_info
        .econtent
        .as_ref()
        .ok_or_else(|| invalid("publisher approval has no attached statement"))?
        .decode_as::<OctetString>()?;
    let release = Release::from_der(value.as_bytes())?;
    if release.to_der()? != value.as_bytes()
        || release.statement_type != RELEASE_TYPE
        || release.version != 1
        || release.digest.as_bytes().len() != 64
        || release.signers.is_empty()
        || release.name.is_empty()
        || release.software_version.is_empty()
    {
        return Err(invalid("malformed publisher approval"));
    }
    Ok((release, Sha512::digest(value.as_bytes()).into()))
}

pub(super) fn countersign(
    openssl: &std::ffi::OsStr,
    data: &mut SignedData,
    signing: &[SigningIdentity],
) -> Result<()> {
    let identities = identities(signing)?;
    let mut primary: Vec<_> = data.signer_infos.0.iter().cloned().collect();
    let mut certs = data
        .certificates
        .clone()
        .ok_or_else(|| invalid("no primary certificates"))?;
    // An archive approval covers the terminal checkpoint's primary signature,
    // which itself binds the stage and exact roster.
    let signer = primary
        .first_mut()
        .ok_or_else(|| invalid("no primary signature"))?;
    let imprint = Sha512::digest(signer.signature.as_bytes()).into();
    for identity in &identities {
        let counter = signer_info(openssl, identity, &imprint, &[], true)?;
        check_signature(openssl, &counter, &[&identity.certificate], &imprint, true)?;
        for cert in std::iter::once(&identity.certificate).chain(&identity.intermediates) {
            let choice = CertificateChoices::Certificate(cert.clone());
            if !certs.0.iter().any(|v| *v == choice) {
                certs.0.insert(choice)?;
            }
        }
        add_unsigned(signer, COUNTERSIGNATURE, Any::encode_from(&counter)?)?;
    }
    data.certificates = Some(certs);
    data.signer_infos = SignerInfos::try_from(primary)?;
    Ok(())
}
