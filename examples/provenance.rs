//! CLI for the experimental mzPeak provenance proposal. No keys are generated,
//! no archive is modified, and no trust policy is inferred from archive content.
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand};
use mzpeak_prototyping::provenance::{
    archive::capture_archive,
    manifest::Manifest,
    signing::{
        Algorithm, KeyProvider, OpenSslBackend, SignatureEnvelope, SignaturePolicy, SigningIdentity,
    },
    stages::{Stage, StageKind},
    trust::TrustPolicy,
    verification::{
        HistoryPolicy, ReleaseStatement, SignedRelease, SignedStage, verify_archive_history,
    },
};
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;

type CliResult<T> = Result<T, Box<dyn Error>>;
const MAX_JSON: u64 = 64 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "Experimental mzPeak provenance capture, signing and verification")]
struct Cli {
    #[arg(long, global = true, default_value = "openssl")]
    openssl: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Capture a ZIP's exact decompressed content inventory.
    Capture {
        archive: PathBuf,
        output: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        stage_id: String,
        #[arg(long = "select")]
        selected_paths: Option<Vec<String>>,
    },
    /// Wrap a manifest as an acquisition stage, without asserting trust.
    InitAcquisition { manifest: PathBuf, output: PathBuf },
    /// Sign a stage with one or both configured algorithms.
    Sign {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        signers: PathBuf,
        #[arg(long, value_parser = ["ecdsa", "pqc", "both"])]
        policy: String,
    },
    /// Sign a publisher's approval of a particular software release.
    SignRelease {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        signers: PathBuf,
        #[arg(long, value_parser = ["ecdsa", "pqc", "both"])]
        policy: String,
    },
    /// Verify complete content/history against externally supplied trust.
    Verify {
        archive: PathBuf,
        records: PathBuf,
        #[arg(long)]
        trust: PathBuf,
        #[arg(long)]
        identities: PathBuf,
        #[arg(long)]
        releases: Option<PathBuf>,
        #[arg(long)]
        publishers: Option<PathBuf>,
        /// Trusted head digest array; absent means no rollback assurance.
        #[arg(long)]
        heads: Option<PathBuf>,
        #[arg(long, value_parser = ["ecdsa", "pqc", "both"])]
        policy: String,
    },
}

fn read(path: impl AsRef<Path>) -> CliResult<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON {
        return Err("input exceeds the 64 MiB limit".into());
    }
    Ok(bytes)
}

// Preserve duplicate-key errors even for maps. Deserializing straight into
// serde_json::Value or BTreeMap would otherwise silently retain the last value.
struct StrictJson(Value);
impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictJson;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON without duplicate keys or floating-point numbers")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictJson(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictJson(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictJson(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(StrictJson(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((key, StrictJson(value))) = map.next_entry::<String, StrictJson>()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate JSON field"));
                    }
                }
                Ok(StrictJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> CliResult<T> {
    let StrictJson(value) = serde_json::from_slice(&read(path)?)?;
    Ok(serde_json::from_value(value)?)
}

fn write_new<T: Serialize>(path: &Path, item: &T) -> CliResult<()> {
    let mut encoded = serde_json::to_vec_pretty(item)?;
    encoded.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&encoded)?;
    Ok(())
}

fn required_option<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum ProviderConfig {
    Software {
        key_path: PathBuf,
        #[serde(deserialize_with = "required_option")]
        passphrase_file: Option<PathBuf>,
    },
    Pkcs11 {
        key_uri: String,
        config_path: PathBuf,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignerConfig {
    algorithm: Algorithm,
    provider: ProviderConfig,
    certificate_path: PathBuf,
    chain_paths: Vec<PathBuf>,
}

fn resolve(base: &Path, path: &Path) -> CliResult<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err("file path must be nonempty".into());
    }
    Ok(base.join(path))
}
fn signers(path: &Path) -> CliResult<Vec<SigningIdentity>> {
    let config: Vec<SignerConfig> = read_json(path)?;
    if config.is_empty() {
        return Err("signers must be a nonempty array".into());
    }
    let base = path.parent().unwrap_or(Path::new("."));
    config
        .into_iter()
        .map(|item| {
            let key = match item.provider {
                ProviderConfig::Software {
                    key_path,
                    passphrase_file,
                } => KeyProvider::Software {
                    key_path: resolve(base, &key_path)?,
                    passphrase_file: passphrase_file.map(|p| resolve(base, &p)).transpose()?,
                },
                ProviderConfig::Pkcs11 {
                    key_uri,
                    config_path,
                } => {
                    if key_uri.is_empty() {
                        return Err("key URI must be nonempty".into());
                    }
                    KeyProvider::Pkcs11 {
                        key_uri,
                        config_path: resolve(base, &config_path)?,
                    }
                }
            };
            let certificate_pem = String::from_utf8(read(resolve(base, &item.certificate_path)?)?)?;
            let intermediates_pem = item
                .chain_paths
                .iter()
                .map(|p| Ok(String::from_utf8(read(resolve(base, p)?)?)?))
                .collect::<CliResult<Vec<_>>>()?;
            Ok(SigningIdentity {
                algorithm: item.algorithm,
                key,
                certificate_pem,
                intermediates_pem,
            })
        })
        .collect()
}
fn signature_policy(value: &str) -> CliResult<SignaturePolicy> {
    match value {
        "ecdsa" => Ok(SignaturePolicy::Ecdsa),
        "pqc" => Ok(SignaturePolicy::Pqc),
        "both" => Ok(SignaturePolicy::Both),
        _ => Err("unsupported signature policy".into()),
    }
}
fn string_map(path: Option<&Path>) -> CliResult<BTreeMap<String, String>> {
    let map: BTreeMap<String, String> = path.map(read_json).transpose()?.unwrap_or_default();
    if map.values().any(|v| v.is_empty()) {
        return Err("identity values must be nonempty".into());
    }
    Ok(map)
}
fn records(path: &Path) -> CliResult<Vec<SignedStage>> {
    let value: Value = read_json(path)?;
    let values = match value {
        Value::Array(values) => values,
        value @ Value::Object(_) => vec![value],
        _ => return Err("records must be a signed stage or nonempty signed-stage array".into()),
    };
    if values.is_empty() {
        return Err("empty signed-stage array".into());
    }
    values
        .into_iter()
        .map(|v| {
            let record: SignedStage = serde_json::from_value(v)?;
            record.stage.validate()?;
            SignatureEnvelope::from_json(&record.envelope.to_json()?)?;
            Ok(record)
        })
        .collect()
}

fn run(cli: Cli) -> CliResult<bool> {
    let backend = OpenSslBackend {
        executable: cli.openssl,
    };
    match cli.command {
        Commands::Capture {
            archive,
            output,
            run_id,
            stage_id,
            selected_paths,
        } => {
            let manifest = capture_archive(archive, &run_id, &stage_id, selected_paths.as_deref())?;
            write_new(&output, &manifest)?;
            println!(
                "{}",
                serde_json::json!({"output":output,"manifest_sha256":manifest.digest()?})
            );
        }
        Commands::InitAcquisition { manifest, output } => {
            let manifest: Manifest = read_json(manifest)?;
            let stage = Stage {
                version: 1,
                kind: StageKind::Acquisition,
                manifest,
                parents: vec![],
                inputs: vec![],
                software: None,
            };
            stage.validate()?;
            write_new(&output, &stage)?;
            println!(
                "{}",
                serde_json::json!({"output":output,"stage_sha256":stage.digest()?})
            );
        }
        Commands::Sign {
            input,
            output,
            signers: config,
            policy,
        } => {
            let stage: Stage = read_json(input)?;
            let envelope = backend.sign(
                &stage.canonical_bytes()?,
                &signers(&config)?,
                signature_policy(&policy)?,
            )?;
            write_new(&output, &SignedStage { stage, envelope })?;
            println!("{}", serde_json::json!({"output":output}));
        }
        Commands::SignRelease {
            input,
            output,
            signers: config,
            policy,
        } => {
            let statement: ReleaseStatement = read_json(input)?;
            let envelope = backend.sign(
                &statement.canonical_bytes()?,
                &signers(&config)?,
                signature_policy(&policy)?,
            )?;
            write_new(
                &output,
                &SignedRelease {
                    statement,
                    envelope,
                },
            )?;
            println!("{}", serde_json::json!({"output":output}));
        }
        Commands::Verify {
            archive,
            records: record_path,
            trust,
            identities,
            releases,
            publishers,
            heads,
            policy,
        } => {
            let records = records(&record_path)?;
            let trust: TrustPolicy = read_json(trust)?;
            let identities = string_map(Some(&identities))?;
            let publishers = string_map(publishers.as_deref())?;
            let heads: Option<Vec<String>> = heads.map(read_json).transpose()?;
            let releases: BTreeMap<String, SignedRelease> =
                releases.map(read_json).transpose()?.unwrap_or_default();
            for (digest, release) in &releases {
                if digest != &release.statement.release_sha256 {
                    return Err("release map key does not match its statement".into());
                }
            }
            let policy = HistoryPolicy {
                trust: &trust,
                required_signatures: signature_policy(&policy)?,
                identities: &identities,
                publishers: &publishers,
                expected_heads: heads.as_deref(),
            };
            let report = verify_archive_history(archive, &records, &policy, &releases, &backend)?;
            let accepted = report.accepted();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"accepted":accepted,"report":report})
                )?
            );
            return Ok(accepted);
        }
    }
    Ok(true)
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse()) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(1),
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"accepted":false,"error":error.to_string()})
            );
            std::process::ExitCode::from(2)
        }
    }
}
