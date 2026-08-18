use std::{collections::HashMap, ops::Deref, str::FromStr};

use mzdata::{
    meta::{FileMetadataConfig, MSDataFileMetadata, MassSpectrometryRun},
    params::CURIE,
};
use serde::{Deserialize, Serialize};
use serde_with::DeserializeFromStr;

use crate::{
    constants::{
        DATA_PROCESSING_METHOD_LIST_KEY, FILE_DESCRIPTION_KEY, INSTRUMENT_CONFIGURATION_LIST_KEY,
        MS_RUN_KEY, MZPEAK_VERSION, SAMPLE_LIST_KEY, SCAN_SETTINGS_LIST_KEY, SOFTWARE_LIST_KEY,
        VERSION_KEY,
    },
    param::{MetaParam, MetadataColumn, MetadataColumnCollection},
};

/// The facet of the thing being described in this file
#[derive(Debug, Serialize, DeserializeFromStr, Clone, PartialEq, Eq)]
pub enum DataKind {
    #[serde(rename = "data_arrays")]
    #[serde(alias = "data arrays")]
    DataArray,
    #[serde(rename = "peaks")]
    Peaks,
    #[serde(rename = "metadata")]
    Metadata,
    #[serde(rename = "scans")]
    Scans,
    #[serde(rename = "precursors")]
    Precursors,
    #[serde(rename = "selected_ions")]
    SelectedIons,
    #[serde(rename = "products")]
    Products,
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
            "data arrays" | "data_arrays" => Self::DataArray,
            "peaks" => Self::Peaks,
            "metadata" => Self::Metadata,
            "scans" => Self::Scans,
            "precursors" => Self::Precursors,
            "selected_ions" => Self::SelectedIons,
            "products" => Self::Products,
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
    #[serde(alias = "mass_spectrum")]
    Spectrum,
    #[serde(rename = "chromatogram")]
    Chromatogram,
    #[serde(rename = "wavelength_spectrum")]
    #[serde(alias = "wavelength spectrum")]
    WavelengthSpectrum,
    #[serde(rename = "other")]
    #[serde(untagged)]
    Other(String),
}

impl FromStr for EntityType {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().trim() {
            "spectrum" | "mass spectrum" | "mass_spectrum" => Self::Spectrum,
            "wavelength spectrum" | "wavelength_spectrum" => Self::WavelengthSpectrum,
            "chromatogram" => Self::Chromatogram,
            "other" => Self::Other("other".into()),
            _ => {
                log::warn!("Found entity type {s}, treating as 'other'");
                Self::Other(s.to_string())
            }
        })
    }
}

/// A single file in the mzPeak archive of a certain type.
///
/// This is distinct from a [`mzdata::meta::SourceFile`]. This describes a file *in* the mzPeak Archive
/// while a `SourceFile` describes a file that was consumed to create the mzPeak Archive. A file can be
/// recorded as both.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The name of the file, relative to the root of the archive
    pub name: String,
    /// The entity this file describes
    pub entity_type: EntityType,
    /// The data this file describes
    pub data_kind: DataKind,
    /// The relationship between columns in this file and controlled vocabulary terms
    #[serde(default)]
    #[serde(alias="metadata_mapping")]
    pub column_mapping: MetadataColumnCollection,
    /// Additional descriptive parameters for this file.
    #[serde(default)]
    pub parameters: Vec<MetaParam>,
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
            (EntityType::Spectrum, DataKind::Scans) => {
                super::MzPeakArchiveType::SpectrumMetadataScans
            }
            (EntityType::Spectrum, DataKind::Precursors) => {
                super::MzPeakArchiveType::SpectrumMetadataPrecursors
            }
            (EntityType::Spectrum, DataKind::SelectedIons) => {
                super::MzPeakArchiveType::SpectrumMetadataSelectedIons
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
            (EntityType::Chromatogram, DataKind::Precursors) => {
                super::MzPeakArchiveType::ChromatogramMetadataPrecursors
            }
            (EntityType::Chromatogram, DataKind::SelectedIons) => {
                super::MzPeakArchiveType::ChromatogramMetadataSelectedIons
            }
            (EntityType::WavelengthSpectrum, DataKind::DataArray) => {
                super::MzPeakArchiveType::WavelengthSpectrumDataArrays
            }
            (EntityType::WavelengthSpectrum, DataKind::Metadata) => {
                super::MzPeakArchiveType::WavelengthSpectrumMetadata
            }
            (EntityType::WavelengthSpectrum, DataKind::Scans) => {
                super::MzPeakArchiveType::WavelengthSpectrumMetadataScans
            }
            (EntityType::Other(_), _) => super::MzPeakArchiveType::Other,
            (_, _) => {
                if matches!(self.data_kind, DataKind::Proprietary) {
                    log::debug!("Could not map {self:?} to an archive type");
                } else {
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
            column_mapping: Default::default(),
            parameters: Default::default(),
        }
    }

    pub fn new_with_meta(
        name: String,
        entity_type: EntityType,
        data_kind: DataKind,
        column_mapping: MetadataColumnCollection,
        parameters: Vec<MetaParam>,
    ) -> Self {
        Self {
            name,
            entity_type,
            data_kind,
            column_mapping,
            parameters,
        }
    }

    pub fn column_mapping_for(&self, curie: CURIE) -> Option<&MetadataColumn> {
        self.column_mapping.find(curie)
    }

    pub fn column_with_path(&self, path: &[&str]) -> Option<&MetadataColumn> {
        self.column_mapping.iter().find(|v| v.path == path)
    }
}


pub(crate) const fn entity_data_kind_to_archive_type(entity_type: &EntityType, data_kind: &DataKind) -> super::MzPeakArchiveType {
    match entity_type {
            EntityType::Spectrum => match data_kind {
                DataKind::DataArray => super::MzPeakArchiveType::SpectrumDataArrays,
                DataKind::Peaks => super::MzPeakArchiveType::SpectrumPeakDataArrays,
                DataKind::Metadata => super::MzPeakArchiveType::SpectrumMetadata,
                DataKind::Scans => super::MzPeakArchiveType::SpectrumMetadataScans,
                DataKind::Precursors => super::MzPeakArchiveType::SpectrumMetadataPrecursors,
                DataKind::SelectedIons => super::MzPeakArchiveType::SpectrumMetadataSelectedIons,
                _ => super::MzPeakArchiveType::Other,
            },
            EntityType::Chromatogram => match data_kind {
                DataKind::DataArray => super::MzPeakArchiveType::ChromatogramDataArrays,
                DataKind::Metadata => super::MzPeakArchiveType::ChromatogramMetadata,
                DataKind::Precursors => super::MzPeakArchiveType::ChromatogramMetadataPrecursors,
                DataKind::SelectedIons => super::MzPeakArchiveType::ChromatogramMetadataSelectedIons,
                _ => super::MzPeakArchiveType::Other,
            },
            EntityType::WavelengthSpectrum => match data_kind {
                DataKind::DataArray => super::MzPeakArchiveType::WavelengthSpectrumDataArrays,
                DataKind::Metadata => super::MzPeakArchiveType::WavelengthSpectrumMetadata,
                DataKind::Scans => super::MzPeakArchiveType::WavelengthSpectrumMetadataScans,
                _ => super::MzPeakArchiveType::Other,
            },
            EntityType::Other(_) => super::MzPeakArchiveType::Other,
        }
}


impl From<(EntityType, DataKind)> for super::MzPeakArchiveType {
    fn from(value: (EntityType, DataKind)) -> Self {
        entity_data_kind_to_archive_type(&value.0, &value.1)
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
            super::MzPeakArchiveType::SpectrumMetadataScans => FileEntry::new(value.tag_file_suffix().into(), EntityType::Spectrum, DataKind::Scans),
            super::MzPeakArchiveType::SpectrumMetadataPrecursors => FileEntry::new(value.tag_file_suffix().into(), EntityType::Spectrum, DataKind::Precursors),
            super::MzPeakArchiveType::SpectrumMetadataSelectedIons => FileEntry::new(value.tag_file_suffix().into(), EntityType::Spectrum, DataKind::SelectedIons),
            super::MzPeakArchiveType::ChromatogramMetadataPrecursors => FileEntry::new(value.tag_file_suffix().into(), EntityType::Chromatogram, DataKind::Precursors),
            super::MzPeakArchiveType::ChromatogramMetadataSelectedIons => FileEntry::new(value.tag_file_suffix().into(), EntityType::Chromatogram, DataKind::SelectedIons),
            super::MzPeakArchiveType::ChromatogramMetadataProducts => FileEntry::new(value.tag_file_suffix().into(), EntityType::Chromatogram, DataKind::Products),
            super::MzPeakArchiveType::WavelengthSpectrumMetadataScans => FileEntry::new(value.tag_file_suffix().into(), EntityType::WavelengthSpectrum, DataKind::Scans),
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

    pub fn add_metadata(
        &mut self,
        key: &str,
        value: serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.metadata.insert(key.to_string(), value)
    }

    pub fn find_entry(&self, entity_type: &EntityType, data_kind: &DataKind) -> Option<&FileEntry> {
        self.files.iter().find(|e| e.entity_type == *entity_type && e.data_kind == *data_kind)
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

    pub fn version(&self) -> Option<&str> {
        self.metadata.get(VERSION_KEY).and_then(|v| v.as_str())
    }

    pub fn as_file_metadata(&self) -> Result<FileMetadataConfig, serde_json::Error> {
        let mut mz_metadata = FileMetadataConfig::default();
        for (k, val) in self.metadata.iter() {
            match k.as_str() {
                FILE_DESCRIPTION_KEY => {
                    let file_description: crate::param::FileDescription =
                        serde_json::from_value(val.clone())?;
                    *mz_metadata.file_description_mut() = file_description.into();
                }
                INSTRUMENT_CONFIGURATION_LIST_KEY => {
                    let instrument_configurations: Vec<crate::param::InstrumentConfiguration> =
                        serde_json::from_value(val.clone())?;
                    for ic in instrument_configurations {
                        mz_metadata
                            .instrument_configurations_mut()
                            .insert(ic.id, ic.into());
                    }
                }
                DATA_PROCESSING_METHOD_LIST_KEY => {
                    let data_processing_list: Vec<crate::param::DataProcessing> =
                        serde_json::from_value(val.clone())?;
                    for dp in data_processing_list {
                        mz_metadata.data_processings_mut().push(dp.into());
                    }
                }
                SAMPLE_LIST_KEY => {
                    let meta_list: Vec<crate::param::Sample> = serde_json::from_value(val.clone())?;
                    for sw in meta_list {
                        mz_metadata.samples_mut().push(sw.into());
                    }
                }
                SOFTWARE_LIST_KEY => {
                    let software_list: Vec<crate::param::Software> =
                        serde_json::from_value(val.clone())?;
                    for sw in software_list {
                        mz_metadata.softwares_mut().push(sw.into());
                    }
                }
                SCAN_SETTINGS_LIST_KEY => {
                    let settings: Vec<crate::param::ScanSettings> =
                        serde_json::from_value(val.clone())?;
                    match mz_metadata.scan_settings_mut() {
                        Some(confs) => confs.extend(settings.into_iter().map(|v| v.into())),
                        None => {
                            panic!("Cannot inject scan settings. This should never happen");
                        }
                    }
                }
                MS_RUN_KEY => {
                    let run: MassSpectrometryRun = serde_json::from_value(val.clone())?;
                    *mz_metadata.run_description_mut().unwrap() = run;
                }
                _ => {}
            }
        }
        Ok(mz_metadata)
    }
}

impl Deref for FileIndex {
    type Target = [FileEntry];

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

impl AsMut<Vec<FileEntry>> for FileIndex {
    fn as_mut(&mut self) -> &mut Vec<FileEntry> {
        &mut self.files
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
        assert_eq!(spec, r#""data_arrays""#);
        assert_eq!(dup, DataKind::DataArray);

        let src = DataKind::Other("foobarbazbang".into());
        let other = serde_json::to_string(&src).unwrap();
        let dup: DataKind = serde_json::from_str(&other).unwrap();
        assert_eq!(other, r#""foobarbazbang""#);
        assert_eq!(dup, src);
    }
}
