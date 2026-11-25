//! Parquet provenance and authenticated history for packed mzPeak archives.
mod archive;
mod cms;
mod conversion;
mod history;
mod openssl_process;
mod statement;
mod timestamp;
pub mod trust;

pub use cms::{Algorithm, AlgorithmPolicy, KeyProvider, SigningIdentity, sign_release};
pub use conversion::{Conversion, SourceFile, SourceResult, capture_source};
pub use history::{
    AppendStage, Checkpoint, ConversionGrant, Coverage, HistoryReport, Policy, StageReport,
    append_stage, approve_archive, attach_release, attach_timestamp, terminal_glob, verify_archive,
};
pub use statement::{Input, Software, StageKind};
pub use timestamp::{TimestampPolicy, timestamp_release, timestamp_requests};

use const_oid::ObjectIdentifier;
use serde::{Deserialize, Serialize};

pub const MEMBER: &str = "provenance.parquet";
pub const ROOT_INDEX: &str = "mzpeak_index.json";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error(transparent)]
    Der(#[from] der::Error),
    #[error(transparent)]
    Trust(#[from] trust::TrustError),
    #[error("archive was replaced, but directory sync failed: {0}")]
    PostCommitSync(std::io::Error),
}

type Result<T> = std::result::Result<T, Error>;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Attribute identifiers shared by the sender and the recipient. The §6.4
/// draft does not allocate them. There are deliberately no defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeOids {
    pub stage: String,
    pub publisher_approval: String,
}

impl AttributeOids {
    fn parse(&self) -> Result<(ObjectIdentifier, ObjectIdentifier)> {
        let parse = |value: &str| {
            let oid = ObjectIdentifier::new(value).map_err(|_| invalid("invalid attribute OID"))?;
            if cms::reserved(oid) {
                return Err(invalid("draft OID conflicts with a standard CMS attribute"));
            }
            Ok(oid)
        };
        let left = parse(&self.stage)?;
        let right = parse(&self.publisher_approval)?;
        if left == right {
            return Err(invalid("stage and publisher attribute OIDs must differ"));
        }
        Ok((left, right))
    }
}

fn validate_path(path: &str) -> Result<()> {
    if path.contains(['\\', '\0'])
        || path
            .split('/')
            .enumerate()
            .any(|(i, part)| matches!(part, "" | "." | "..") || (i == 0 && part.contains(':')))
    {
        return Err(invalid("unsafe archive-relative path"));
    }
    Ok(())
}

fn digest_bytes(hex: &str) -> Result<[u8; 64]> {
    let mut digest = [0; 64];
    hex::decode_to_slice(hex, &mut digest).map_err(|_| invalid("expected a SHA-512 digest"))?;
    Ok(digest)
}
