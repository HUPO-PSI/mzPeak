use const_oid::ObjectIdentifier;
use der::asn1::{OctetString, SetOfVec};
use der::{Decode, Encode, Enumerated, Sequence, ValueOrd};
use serde::{Deserialize, Serialize};
use x509_cert::spki::AlgorithmIdentifierOwned;

use super::{Result, digest_bytes, invalid, validate_path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Enumerated)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Acquisition = 0,
    Processing = 1,
    Conversion = 2,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Sequence)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub stage: u64,
    pub row: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Software {
    pub name: String,
    pub version: String,
    pub release_sha512: String,
    pub parameters_sha512: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct SoftwareStatement {
    pub name: String,
    pub version: String,
    pub release: OctetString,
    pub parameters: OctetString,
}

impl TryFrom<&Software> for SoftwareStatement {
    type Error = super::Error;

    fn try_from(value: &Software) -> Result<Self> {
        if value.name.is_empty() || value.version.is_empty() {
            return Err(invalid("software name and version are required"));
        }
        Ok(Self {
            name: value.name.clone(),
            version: value.version.clone(),
            release: OctetString::new(digest_bytes(&value.release_sha512)?.to_vec())?,
            parameters: OctetString::new(digest_bytes(&value.parameters_sha512)?.to_vec())?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence, ValueOrd)]
pub(super) struct SignerBinding {
    pub fingerprint: OctetString,
    pub algorithm: AlgorithmIdentifierOwned,
}

pub(super) type SignerSet = SetOfVec<SignerBinding>;

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct Member {
    pub index: u64,
    pub path: String,
    pub size: u64,
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    pub row_group: Option<u64>,
    #[asn1(context_specific = "1", tag_mode = "EXPLICIT", optional = "true")]
    pub column: Option<u64>,
    #[asn1(context_specific = "2", tag_mode = "EXPLICIT", optional = "true")]
    pub column_path: Option<String>,
    #[asn1(context_specific = "3", tag_mode = "EXPLICIT", optional = "true")]
    pub offset: Option<u64>,
    #[asn1(context_specific = "4", tag_mode = "EXPLICIT", optional = "true")]
    pub length: Option<u64>,
}

impl Member {
    pub fn whole(&self) -> bool {
        self.offset.is_none()
            && self.length.is_none()
            && self.row_group.is_none()
            && self.column.is_none()
            && self.column_path.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        validate_path(&self.path)?;
        let coords = [
            self.row_group.is_some(),
            self.column.is_some(),
            self.column_path.is_some(),
        ];
        if self.path == "*"
            || self.path.eq_ignore_ascii_case(super::MEMBER)
            || coords.iter().any(|v| *v)
                && (coords != [true; 3] || self.offset.is_none() || self.length.is_none())
            || self
                .offset
                .unwrap_or(0)
                .checked_add(self.length.unwrap_or(0))
                .is_none_or(|end| end > self.size)
        {
            return Err(invalid("invalid signed member descriptor"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct ClaimedSource {
    pub path: String,
    pub size: u64,
    pub digest: OctetString,
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct ConversionStatement {
    pub vendor: String,
    pub format: String,
    pub method: String,
    pub verifier: String,
    pub verifier_version: String,
    pub result: super::conversion::SourceResult,
    pub sources: Vec<ClaimedSource>,
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    pub report_row: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct Stage {
    pub version: u64,
    pub kind: StageKind,
    pub index: u64,
    pub members: Vec<Member>,
    pub signers: SignerSet,
    pub inputs: Vec<Input>,
    #[asn1(context_specific = "0", tag_mode = "EXPLICIT", optional = "true")]
    pub software: Option<SoftwareStatement>,
    #[asn1(context_specific = "1", tag_mode = "EXPLICIT", optional = "true")]
    pub conversion: Option<ConversionStatement>,
}

impl Stage {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let stage = Self::from_der(bytes)?;
        if stage.to_der()? != bytes {
            return Err(invalid("stage attribute is not canonical DER"));
        }
        stage.validate()?;
        Ok(stage)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.members.is_empty() || self.signers.is_empty() {
            return Err(invalid("unsupported or empty stage statement"));
        }
        if (self.kind == StageKind::Conversion) != self.conversion.is_some() {
            return Err(invalid("vendor source belongs only to conversion stages"));
        }
        if self.kind == StageKind::Processing {
            if self.inputs.is_empty() || self.software.is_none() {
                return Err(invalid(
                    "processing requires software identity and signed inputs",
                ));
            }
        } else if !self.inputs.is_empty() {
            return Err(invalid("an origin stage has no internal inputs"));
        }
        if self.inputs.windows(2).any(|p| p[0] >= p[1])
            || self
                .inputs
                .iter()
                .any(|input| input.row >= input.stage || input.stage >= self.index)
        {
            return Err(invalid(
                "stage inputs must be unique sorted references to earlier whole-member rows",
            ));
        }
        for member in &self.members {
            member.validate()?;
        }
        if let Some(software) = &self.software
            && (software.name.is_empty()
                || software.version.is_empty()
                || software.release.as_bytes().len() != 64
                || software.parameters.as_bytes().len() != 64)
        {
            return Err(invalid("invalid software identity"));
        }
        if let Some(conversion) = &self.conversion {
            super::conversion::validate(conversion)?;
            if let Some(index) = conversion.report_row
                && !self.members.iter().any(|m| m.index == index && m.whole())
            {
                return Err(invalid("conversion report is not covered by this stage"));
            }
        }
        Ok(())
    }
}

pub(super) const RELEASE_TYPE: &str = "mzPeak-software-release";

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct Release {
    pub statement_type: String,
    pub version: u64,
    pub name: String,
    pub software_version: String,
    pub digest: OctetString,
    pub signers: SignerSet,
}

#[derive(Clone, Debug, PartialEq, Eq, Sequence)]
pub(super) struct ContentReference {
    pub content_type: ObjectIdentifier,
    pub identifier: OctetString,
    pub signature: OctetString,
}
