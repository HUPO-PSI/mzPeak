use std::{collections::HashMap, ops::Deref, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_with::DeserializeFromStr;

use crate::constants::{MZPEAK_VERSION, VERSION_KEY};

/// The facet of the thing being described in this file
#[derive(Debug, Serialize, DeserializeFromStr, Clone, PartialEq, Eq)]
pub enum DataKind {
    #[serde(rename = "data arrays")]
    DataArray,
    #[serde(rename = "peaks")]
    Peaks,
    #[serde(rename = "metadata")]
    Metadata,
    #[serde(rename = "proprietary")]
    Proprietary,
    #[serde(rename = "other")]
    #[serde(untagged)]
    Other(String),
}

impl FromStr for DataKind {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().trim() {
            "data arrays" => Self::DataArray,
            "peaks" => Self::Peaks,
            "metadata" => Self::Metadata,
            "proprietary" => Self::Proprietary,
            "other" => Self::Other("other".into()),
            _ => Self::Other(s.to_string()),
        })
    }
}

/// The things being described in one facet or another by this file
#[derive(Debug, Serialize, DeserializeFromStr, Clone, PartialEq, Eq)]
pub enum EntityType {
    #[serde(rename = "spectrum")]
    #[serde(alias = "mass spectrum")]
    Spectrum,
    #[serde(rename = "chromatogram")]
    Chromatogram,
    #[serde(rename = "wavelength spectrum")]
    WavelengthSpectrum,
    #[serde(rename = "other")]
    #[serde(untagged)]
    Other(String),
}

impl FromStr for EntityType {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().trim() {
            "spectrum" => Self::Spectrum,
            "mass spectrum" => Self::Spectrum,
            "wavelength spectrum" => Self::WavelengthSpectrum,
            "chromatogram" => Self::Chromatogram,
            "other" => Self::Other("other".into()),
            _ => {
                log::warn!("Found entity type {s}, treating as 'other'");
                Self::Other(s.to_string())
            }
        })
    }
}

/// A single file in the mzPeak archive of a certain type
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The name of the file, relative to the root of the archive
    pub name: String,
    /// The entity this file describes
    pub entity_type: EntityType,
    /// The data this file describes
    pub data_kind: DataKind,
}

impl FileEntry {
    pub fn archive_type(&self) -> super::MzPeakArchiveType {
        match (&self.entity_type, &self.data_kind) {
            (EntityType::Spectrum, DataKind::DataArray) => {
                super::MzPeakArchiveType::SpectrumDataArrays
            }
            (EntityType::Spectrum, DataKind::Metadata) => {
                super::MzPeakArchiveType::SpectrumMetadata
            }
            (EntityType::Spectrum, DataKind::Peaks) => {
                super::MzPeakArchiveType::SpectrumPeakDataArrays
            }
            (EntityType::Chromatogram, DataKind::DataArray) => {
                super::MzPeakArchiveType::ChromatogramDataArrays
            }
            (EntityType::Chromatogram, DataKind::Metadata) => {
                super::MzPeakArchiveType::ChromatogramMetadata
            }
            (EntityType::WavelengthSpectrum, DataKind::DataArray) => {
                super::MzPeakArchiveType::WavelengthSpectrumDataArrays
            }
            (EntityType::WavelengthSpectrum, DataKind::Metadata) => {
                super::MzPeakArchiveType::WavelengthSpectrumMetadata
            }
            (EntityType::Other(_), _) => super::MzPeakArchiveType::Other,
            (_, _) => {
                if matches!(self.data_kind, DataKind::Proprietary) {
                    log::debug!("Could not map {self:?} to an archive type");
                }
                else {
                    log::warn!("Could not map {self:?} to an archive type");
                }
                super::MzPeakArchiveType::Other
            }
        }
    }

    pub fn new(name: String, entity_type: EntityType, data_kind: DataKind) -> Self {
        Self {
            name,
            entity_type,
            data_kind,
        }
    }
}

impl From<super::MzPeakArchiveType> for FileEntry {
    fn from(value: super::MzPeakArchiveType) -> Self {
        match value {
            super::MzPeakArchiveType::SpectrumMetadata => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::Spectrum,
                DataKind::Metadata,
            ),
            super::MzPeakArchiveType::SpectrumDataArrays => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::Spectrum,
                DataKind::DataArray,
            ),
            super::MzPeakArchiveType::SpectrumPeakDataArrays => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::Spectrum,
                DataKind::Peaks,
            ),
            super::MzPeakArchiveType::ChromatogramMetadata => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::Chromatogram,
                DataKind::Metadata,
            ),
            super::MzPeakArchiveType::ChromatogramDataArrays => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::Chromatogram,
                DataKind::DataArray,
            ),
            super::MzPeakArchiveType::WavelengthSpectrumDataArrays => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::WavelengthSpectrum,
                DataKind::DataArray,
            ),
            super::MzPeakArchiveType::WavelengthSpectrumMetadata => FileEntry::new(
                value.tag_file_suffix().into(),
                EntityType::WavelengthSpectrum,
                DataKind::Metadata,
            ),
            super::MzPeakArchiveType::Other => FileEntry::new(
                "".into(),
                "other".parse().unwrap(),
                DataKind::Other("other".into()),
            ),
            super::MzPeakArchiveType::Proprietary => FileEntry::new(
                "".into(),
                EntityType::Other("".into()),
                DataKind::Proprietary,
            ),
        }
    }
}

/// A collection of [`FileEntry`] and associated JSON-compatible metadata
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub files: Vec<FileEntry>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl From<Vec<FileEntry>> for FileIndex {
    fn from(value: Vec<FileEntry>) -> Self {
        Self::new(value, Default::default())
    }
}

impl FileIndex {
    pub const fn index_file_name() -> &'static str {
        "mzpeak_index.json"
    }

    pub fn new(files: Vec<FileEntry>, metadata: HashMap<String, serde_json::Value>) -> Self {
        Self { files, metadata }
    }

    pub fn push(&mut self, entry: FileEntry) {
        self.files.push(entry);
    }

    pub fn add_metadata(&mut self, key: &str, value: serde_json::Value) -> Option<serde_json::Value> {
        self.metadata.insert(key.to_string(), value)
    }

    pub fn remove_metadata(&mut self, key: &str) -> Option<serde_json::Value> {
        self.metadata.remove(key)
    }

    pub fn iter_metadata(&self) -> std::collections::hash_map::Iter<'_, String, serde_json::Value> {
        self.metadata.iter()
    }

    pub fn add_version(&mut self) {
        self.add_metadata(VERSION_KEY, MZPEAK_VERSION.into());
    }
}

impl Deref for FileIndex {
    type Target = [FileEntry];

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_entity_type_conversion() {
        let spec = serde_json::to_string(&EntityType::Spectrum).unwrap();
        let dup: EntityType = serde_json::from_str(&spec).unwrap();
        assert_eq!(spec, r#""spectrum""#);
        assert_eq!(dup, EntityType::Spectrum);

        let src = EntityType::Other("foobarbazbang".into());
        let other = serde_json::to_string(&src).unwrap();
        let dup: EntityType = serde_json::from_str(&other).unwrap();
        assert_eq!(other, r#""foobarbazbang""#);
        assert_eq!(dup, src);
    }

    #[test]
    fn test_data_type_conversion() {
        let spec = serde_json::to_string(&DataKind::DataArray).unwrap();
        let dup: DataKind = serde_json::from_str(&spec).unwrap();
        assert_eq!(spec, r#""data arrays""#);
        assert_eq!(dup, DataKind::DataArray);

        let src = DataKind::Other("foobarbazbang".into());
        let other = serde_json::to_string(&src).unwrap();
        let dup: DataKind = serde_json::from_str(&other).unwrap();
        assert_eq!(other, r#""foobarbazbang""#);
        assert_eq!(dup, src);
    }
}