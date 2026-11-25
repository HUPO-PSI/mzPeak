use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

use mzpeak_prototyping::provenance::{self, AppendStage, AttributeOids, Policy, SigningIdentity};

#[derive(Parser)]
#[command(about = "Sign and verify §6.4 Parquet provenance in packed mzPeak archives")]
struct Args {
    #[arg(long, default_value = "openssl", global = true)]
    openssl: OsString,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Sign an acquisition, conversion, or processing stage.
    Append {
        archive: PathBuf,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        stage: PathBuf,
    },
    /// Verify coverage, history and the recipient's trust policy.
    Verify {
        archive: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        /// Explicit reference time in Unix seconds. Defaults to current time.
        #[arg(long)]
        at: Option<i64>,
    },
    /// Inventory a local vendor RAW file or directory.
    CaptureSource { source: PathBuf },
    /// Make an independent publisher proof for the exact artifact bytes.
    SignRelease {
        artifact: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Countersign the terminal glob as a separately authorized archive approver.
    Approve {
        archive: PathBuf,
        #[arg(long)]
        profile: PathBuf,
    },
    /// Add an independent publisher proof to the terminal glob.
    AttachRelease {
        archive: PathBuf,
        #[arg(long)]
        oids: PathBuf,
        proof: PathBuf,
    },
    /// Create one RFC 3161 request per primary signature. No network access.
    TimestampRequests {
        input: PathBuf,
        /// Read CMS directly instead of the terminal glob of a mzPeak archive.
        #[arg(long)]
        cms: bool,
        #[arg(long)]
        policy_oid: Option<String>,
    },
    /// Add a granted RFC 3161 response to the matching terminal signature.
    AttachTimestamp {
        archive: PathBuf,
        #[arg(long)]
        oids: PathBuf,
        response: PathBuf,
        #[arg(long)]
        query: Option<PathBuf>,
    },
    /// Add a timestamp to a publisher proof before it is embedded.
    TimestampRelease {
        proof: PathBuf,
        response: PathBuf,
        #[arg(long)]
        query: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SigningProfile {
    oids: AttributeOids,
    signers: Vec<SigningIdentity>,
}

fn load<T: DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    Ok(serde_json::from_reader(fs::File::open(path)?)?)
}

fn print<T: serde::Serialize>(value: T) -> Result<(), serde_json::Error> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn digest(path: &Path) -> io::Result<String> {
    mzpeak_prototyping::validation::checksum_stream(&mut fs::File::open(path)?)
}

fn run(openssl: &OsStr, action: Action) -> Result<bool, Box<dyn std::error::Error>> {
    match action {
        Action::Append {
            archive,
            profile,
            stage,
        } => {
            let signing: SigningProfile = load(&profile)?;
            print(provenance::append_stage(
                openssl,
                &archive,
                &signing.oids,
                &signing.signers,
                &load::<AppendStage>(&stage)?,
            )?)?;
        }
        Action::Verify {
            archive,
            policy,
            at,
        } => {
            let mut policy: Policy = load(&policy)?;
            let time = at.unwrap_or_else(|| chrono::Utc::now().timestamp());
            if time < 0 {
                return Err("validation time must be nonnegative".into());
            }
            policy.trust.validation_time = time;
            if let Some(tsa) = &mut policy.historical {
                tsa.validation_time = time;
            }
            let report = provenance::verify_archive(openssl, &archive, &policy)?;
            let accepted = report.accepted;
            print(report)?;
            return Ok(accepted);
        }
        Action::Approve { archive, profile } => {
            let signing: SigningProfile = load(&profile)?;
            print(provenance::approve_archive(
                openssl,
                &archive,
                &signing.oids,
                &signing.signers,
            )?)?;
        }
        Action::CaptureSource { source } => print(provenance::capture_source(source)?)?,
        Action::SignRelease {
            artifact,
            name,
            version,
            profile,
            output,
        } => {
            let signers: SigningProfile = load(&profile)?;
            let sha512 = digest(&artifact)?;
            let cms =
                provenance::sign_release(openssl, &signers.signers, &name, &version, &sha512)?;
            fs::write(output, cms)?;
            print(serde_json::json!({ "sha512": sha512 }))?;
        }
        Action::AttachRelease {
            archive,
            oids,
            proof,
        } => {
            print(provenance::attach_release(
                openssl,
                &archive,
                &load(&oids)?,
                &fs::read(proof)?,
            )?)?;
        }
        Action::TimestampRequests {
            input,
            cms,
            policy_oid,
        } => {
            let bytes = if cms {
                fs::read(input)?
            } else {
                provenance::terminal_glob(&input)?
            };
            print(provenance::timestamp_requests(
                openssl,
                &bytes,
                policy_oid.as_deref(),
            )?)?;
        }
        Action::AttachTimestamp {
            archive,
            oids,
            response,
            query,
        } => {
            let query = query.map(fs::read).transpose()?;
            print(provenance::attach_timestamp(
                openssl,
                &archive,
                &load(&oids)?,
                &fs::read(response)?,
                query.as_deref(),
            )?)?;
        }
        Action::TimestampRelease {
            proof,
            response,
            query,
            output,
        } => {
            let query = query.map(fs::read).transpose()?;
            fs::write(
                output,
                provenance::timestamp_release(
                    &fs::read(proof)?,
                    &fs::read(response)?,
                    query.as_deref(),
                )?,
            )?;
        }
    }
    Ok(true)
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args.openssl, args.action) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"accepted":false,"error":error.to_string()})
            );
            ExitCode::from(2)
        }
    }
}
