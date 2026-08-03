use std::{
    borrow::Borrow,
    collections::{HashMap, VecDeque},
    ops::Deref,
    vec,
};

use arrow::array::{
    ArrayBuilder, ArrayRef, BinaryBuilder, BooleanBuilder, Float32Builder, Float64Builder,
    Int32Builder, Int64Builder,
};
use mzdata::params::{ParamDescribed, ParamLike, Unit};
use parquet::arrow::{ProjectionMask, arrow_reader::ArrowReaderBuilder};
use serde::{Deserialize, Serialize, ser::SerializeSeq};

/// A list of ion mobility point measures for scans
pub const ION_MOBILITY_SCAN_TERMS: [mzdata::params::CURIE; 4] = [
    // ion mobility drift time
    mzdata::curie!(MS:1002476),
    // inverse reduced ion mobility drift time
    mzdata::curie!(MS:1002815),
    // FAIMS compensation voltage
    mzdata::curie!(MS:1001581),
    // SELEXION compensation voltage
    mzdata::curie!(MS:1003371),
];

pub type CURIE = mzdata::params::CURIE;

pub use mzdata::curie;

// Provide a way to JSON-serialize CURIEs as nullable string
pub(crate) fn opt_curie_serialize<S>(
    curie: &Option<CURIE>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match curie {
        Some(curie) => serializer.serialize_str(&mzdata::params::CURIE::from(*curie).to_string()),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn path_or_curie_serialize<S>(
    value: &PathOrCURIE,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        PathOrCURIE::Path(items) => {
            let mut s = serializer.serialize_seq(Some(items.len()))?;
            for i in items.iter() {
                s.serialize_element(i)?;
            }
            s.end()
        }
        PathOrCURIE::CURIE(curie) => {
            serializer.serialize_str(&mzdata::params::CURIE::from(*curie).to_string())
        }
        PathOrCURIE::None => serializer.serialize_none(),
    }
}

pub(crate) fn path_or_curie_deserialize<'de, D>(deserializer: D) -> Result<PathOrCURIE, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor {}
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = PathOrCURIE;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("CURIE string, list of strings, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(PathOrCURIE::None)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            match v.parse::<mzdata::params::CURIE>() {
                Ok(v) => Ok(PathOrCURIE::CURIE(v.into())),
                Err(e) => Err(E::custom(e)),
            }
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut path = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                path.push(v);
            }
            Ok(PathOrCURIE::Path(path))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(PathOrCURIE::None)
        }
    }

    deserializer.deserialize_any(Visitor {})
}

// Provide a way to JSON-serialize CURIEs as string
pub(crate) fn curie_serialize<S>(curie: &CURIE, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&mzdata::params::CURIE::from(*curie).to_string())
}

// Provide a way to JSON-deserialize CURIEs from a nullable string
pub(crate) fn opt_curie_deserialize<'de, D>(deserializer: D) -> Result<Option<CURIE>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CURIEVisit {}
    impl<'de> serde::de::Visitor<'de> for CURIEVisit {
        type Value = Option<CURIE>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("CURIE string or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            match v.parse::<mzdata::params::CURIE>() {
                Ok(v) => Ok(Some(v.into())),
                Err(e) => Err(E::custom(e)),
            }
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }
    }

    deserializer.deserialize_any(CURIEVisit {})
}

// Provide a way to JSON-deserialize CURIEs from a string
pub(crate) fn curie_deserialize<'de, D>(deserializer: D) -> Result<CURIE, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CURIEVisit {}
    impl<'de> serde::de::Visitor<'de> for CURIEVisit {
        type Value = CURIE;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("CURIE string")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            match v.parse::<mzdata::params::CURIE>() {
                Ok(v) => Ok(v.into()),
                Err(e) => Err(E::custom(e)),
            }
        }
    }

    deserializer.deserialize_str(CURIEVisit {})
}

/// A [`serde_json`]-friendly version of [`Param`] that uses
/// [`serde_json::Value`] instead of [`ParamValueSplit`].
///
/// This type is used to represent parameters stored
/// in the metadata structures that are JSON-serialized in the
/// Parquet metadata footer.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaParam {
    pub name: Option<String>,
    #[serde(
        serialize_with = "opt_curie_serialize",
        deserialize_with = "opt_curie_deserialize"
    )]
    pub accession: Option<CURIE>,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(
        serialize_with = "opt_curie_serialize",
        deserialize_with = "opt_curie_deserialize"
    )]
    pub unit: Option<CURIE>,
}

impl Eq for MetaParam {}

impl From<MetaParam> for mzdata::Param {
    fn from(value: MetaParam) -> Self {
        let mut this = Self::default();
        this.name = value.name.unwrap_or_default();
        this.unit = value
            .unit
            .map(|acc| Unit::from_curie(&(acc.into())))
            .unwrap_or_default();
        if let Some(curie) = value.accession {
            this.controlled_vocabulary = Some(curie.controlled_vocabulary);
            this.accession = Some(curie.accession);
        }
        this.value = match value.value {
            serde_json::Value::Null => mzdata::params::Value::Empty,
            serde_json::Value::Bool(v) => mzdata::params::Value::Boolean(v),
            serde_json::Value::Number(number) => {
                if number.is_f64() {
                    mzdata::params::Value::Float(number.as_f64().unwrap())
                } else if number.is_i64() {
                    mzdata::params::Value::Int(number.as_i64().unwrap())
                } else {
                    mzdata::params::Value::Int(number.as_u64().unwrap() as i64)
                }
            }
            serde_json::Value::String(v) => mzdata::params::Value::String(v),
            serde_json::Value::Array(_) => mzdata::params::Value::String(value.value.to_string()),
            serde_json::Value::Object(_) => mzdata::params::Value::String(value.value.to_string()),
        };
        this
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlledVocabularyEntry {
    pub id: String,
    pub full_name: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl ControlledVocabularyEntry {
    pub fn new(
        id: impl ToString,
        full_name: impl ToString,
        uri: impl ToString,
        version: Option<impl ToString>,
    ) -> Self {
        Self {
            id: id.to_string(),
            full_name: full_name.to_string(),
            uri: uri.to_string(),
            version: version.map(|v| v.to_string()),
        }
    }
}

impl From<mzdata::params::ControlledVocabulary> for ControlledVocabularyEntry {
    fn from(value: mzdata::params::ControlledVocabulary) -> Self {
        match value {
            mzdata::params::ControlledVocabulary::MS => ControlledVocabularyEntry::new(
                "MS",
                "Proteomics Standards Initiative Mass Spectrometry Ontology",
                "http://purl.obolibrary.org/obo/ms/4.1.249/psi-ms.obo",
                Some("4.1.249"),
            ),
            mzdata::params::ControlledVocabulary::UO => ControlledVocabularyEntry::new(
                "UO",
                "Units of measurement ontology",
                "http://purl.obolibrary.org/obo/uo/releases/2026-01-16/uo.obo",
                Some("2026-01-16"),
            ),
            mzdata::params::ControlledVocabulary::EFO => ControlledVocabularyEntry::new(
                "EFO",
                "Experimental Factor Ontology",
                "http://www.ebi.ac.uk/efo/releases/v3.90.0/efo.obo",
                Some("v3.90.0"),
            ),
            mzdata::params::ControlledVocabulary::OBI => ControlledVocabularyEntry::new(
                "OBI",
                "Ontology for Biomedical Investigations",
                "http://purl.obolibrary.org/obo/obi/2026-05-08/obi.obo",
                Some("2026-05-08"),
            ),
            mzdata::params::ControlledVocabulary::HANCESTRO => ControlledVocabularyEntry::new(
                "HANCESTRO",
                "Human Ancestry Ontology",
                "http://purl.obolibrary.org/obo/hancestro/releases/2025-10-14/hancestro.obo",
                Some("2025-10-14"),
            ),
            mzdata::params::ControlledVocabulary::BFO => ControlledVocabularyEntry::new(
                "BFO",
                "Basic Formal Ontology",
                "http://purl.obolibrary.org/obo/bfo/2019-08-26/bfo.obo",
                Some("2019-08-26"),
            ),
            mzdata::params::ControlledVocabulary::NCIT => ControlledVocabularyEntry::new(
                "NCIT",
                "NCI Thesaurus OBO Edition",
                "http://purl.obolibrary.org/obo/ncit/releases/2026-03-19/ncit.obo",
                Some("26.02d"),
            ),
            mzdata::params::ControlledVocabulary::BTO => ControlledVocabularyEntry::new(
                "BTO",
                "The BRENDA Tissue Ontology (BTO)",
                "http://purl.obolibrary.org/obo/bto/releases/2021-10-26/bto.owl",
                Some("2021-10-26"),
            ),
            mzdata::params::ControlledVocabulary::PRIDE => ControlledVocabularyEntry::new(
                "PRIDE",
                "Proteomics Identification Database Ontology",
                "http://purl.obolibrary.org/obo/pride/releases/2026-06-01/pride.obo",
                Some("2026-06-01"),
            ),
            mzdata::params::ControlledVocabulary::IMS => ControlledVocabularyEntry::new(
                "IMS",
                "Imaging Mass Spectrometry Ontology",
                "https://raw.githubusercontent.com/imzML/imzML/refs/heads/master/imagingMS.obo",
                Some("1.1.0"),
            ),
            mzdata::params::ControlledVocabulary::Unknown => todo!(),
        }
    }
}

fn value_ref_to_serde_json_value(value: mzdata::params::ValueRef<'_>) -> serde_json::Value {
    match value {
        mzdata::params::ValueRef::String(x) => serde_json::Value::String(x.to_string()),
        mzdata::params::ValueRef::Float(x) => {
            serde_json::Value::Number(serde_json::Number::from_f64(x).unwrap())
        }
        mzdata::params::ValueRef::Int(x) => {
            serde_json::Value::Number(serde_json::Number::from_i128(x as i128).unwrap())
        }
        mzdata::params::ValueRef::Buffer(_) => unimplemented!(),
        mzdata::params::ValueRef::Empty => serde_json::Value::Null,
        mzdata::params::ValueRef::Boolean(x) => serde_json::Value::Bool(x),
        mzdata::params::ValueRef::List(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|v| {
                    let v = v.clone();
                    serde_json::to_value(v).unwrap()
                })
                .collect(),
        ),
    }
}

impl From<&mzdata::Param> for MetaParam {
    fn from(value: &mzdata::Param) -> Self {
        let curie = value.curie().map(CURIE::from);
        let val = value_ref_to_serde_json_value(value.value());
        Self {
            name: Some(value.name.clone()),
            accession: curie,
            value: val,
            unit: value.unit.to_curie().map(CURIE::from),
        }
    }
}

impl From<mzdata::Param> for MetaParam {
    fn from(value: mzdata::Param) -> Self {
        let curie = value.curie().map(CURIE::from);
        let val = value_ref_to_serde_json_value(value.value());
        Self {
            name: Some(value.name),
            accession: curie,
            value: val,
            unit: value.unit.to_curie().map(CURIE::from),
        }
    }
}

/// An adaptation of [`mzdata::meta::SourceFile`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: String,
    pub location: String,
    pub name: String,
    pub parameters: Vec<MetaParam>,
}

impl From<&mzdata::meta::SourceFile> for SourceFile {
    fn from(value: &mzdata::meta::SourceFile) -> Self {
        let mut parameters: Vec<MetaParam> = value
            .params()
            .iter()
            .cloned()
            .map(MetaParam::from)
            .collect();
        if let Some(p) = value.file_format.as_ref() {
            parameters.push(p.clone().into())
        }
        if let Some(p) = value.id_format.as_ref() {
            parameters.push(p.clone().into())
        }
        Self {
            id: value.id.clone(),
            location: value.location.clone(),
            name: value.name.clone(),
            parameters,
        }
    }
}

impl From<SourceFile> for mzdata::meta::SourceFile {
    fn from(value: SourceFile) -> Self {
        let mut params = Vec::new();
        let mut id_format = None;
        let mut file_format = None;
        for param in value.parameters {
            if let Some(curie) = param.accession {
                if let Some(term) = mzdata::meta::NativeSpectrumIdentifierFormatTerm::from_accession(
                    curie.accession,
                ) {
                    id_format = Some(term.into());
                } else if let Some(term) =
                    mzdata::meta::MassSpectrometerFileFormatTerm::from_accession(curie.accession)
                {
                    file_format = Some(term.into());
                } else {
                    params.push(param.into());
                }
            } else {
                params.push(param.into());
            }
        }

        Self {
            name: value.name,
            location: value.location,
            id: value.id,
            file_format,
            id_format,
            params,
        }
    }
}

/// An adaption of [`mzdata::meta::ScanSettings`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScanSettings {
    /// A unique identifier
    pub id: String,
    /// List with the source files containing the acquisition settings
    pub source_file_refs: Vec<String>,
    /// Target list (or 'inclusion list') configured prior to the run
    pub targets: Vec<Vec<MetaParam>>,
    /// The controlled vocabulary and user parameters of the settings
    pub parameters: Vec<MetaParam>,
}

impl From<&mzdata::meta::ScanSettings> for ScanSettings {
    fn from(value: &mzdata::meta::ScanSettings) -> Self {
        Self {
            id: value.id.clone(),
            source_file_refs: value.source_file_refs.clone(),
            targets: value
                .targets
                .iter()
                .map(|v| v.iter().map(MetaParam::from).collect())
                .collect(),
            parameters: value.params.iter().map(MetaParam::from).collect(),
        }
    }
}

impl From<ScanSettings> for mzdata::meta::ScanSettings {
    fn from(value: ScanSettings) -> Self {
        mzdata::meta::ScanSettings::new(
            value.id,
            value
                .parameters
                .into_iter()
                .map(mzdata::Param::from)
                .collect(),
            value.source_file_refs,
            value
                .targets
                .into_iter()
                .map(|v| v.into_iter().map(mzdata::Param::from).collect())
                .collect(),
        )
    }
}

/// Represents a contact person for a file
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Contact {
    /// The name of the contact person. This is equivalent to `MS:1000586|contact name` (http://purl.obolibrary.org/obo/MS_1000586)
    #[serde(default)]
    pub contact_name: Option<String>,
    /// The home institute of the contact person. This is equivalent to `MS:1000590|contact affiliation` (http://purl.obolibrary.org/obo/MS_1000590)
    #[serde(default)]
    pub contact_affiliation: Option<String>,
    #[serde(default)]
    pub parameters: Vec<MetaParam>,
}

/// An adaptation of [`mzdata::meta::FileDescription`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FileDescription {
    pub contents: Vec<MetaParam>,
    pub source_files: Vec<SourceFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    pub contacts: Vec<Contact>,
}

impl From<FileDescription> for mzdata::meta::FileDescription {
    fn from(value: FileDescription) -> Self {
        let params: Vec<mzdata::params::Param> =
            value.contents.into_iter().map(|p| p.into()).collect();
        let source_files = value.source_files.into_iter().map(|sf| sf.into()).collect();
        Self::new(params, source_files)
    }
}

impl From<&mzdata::meta::FileDescription> for FileDescription {
    fn from(value: &mzdata::meta::FileDescription) -> Self {
        let contents = value
            .contents
            .iter()
            .cloned()
            .map(MetaParam::from)
            .collect();
        let source_files = value.source_files.iter().map(SourceFile::from).collect();
        Self {
            contents,
            source_files,
            contacts: Vec::new(),
        }
    }
}

/// An adaptation of [`mzdata::meta::Software`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Software {
    /// A unique identifier for the software within processing metadata
    pub id: String,
    /// A string denoting a particular software version, but does no guarantee is given for its format
    pub version: String,
    /// Any associated vocabulary terms, including actual software name and type
    pub parameters: Vec<MetaParam>,
}

impl From<Software> for mzdata::meta::Software {
    fn from(value: Software) -> Self {
        Self::new(
            value.id,
            value.version,
            value.parameters.into_iter().map(|p| p.into()).collect(),
        )
    }
}

impl From<&mzdata::meta::Software> for Software {
    fn from(value: &mzdata::meta::Software) -> Self {
        Self {
            id: value.id.clone(),
            version: value.version.clone(),
            parameters: value.iter_params().cloned().map(MetaParam::from).collect(),
        }
    }
}

/// An adaptation of [`mzdata::meta::ProcessingMethod`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProcessingMethod {
    pub order: i8,
    pub software_reference: String,
    pub parameters: Vec<MetaParam>,
}

impl From<ProcessingMethod> for mzdata::meta::ProcessingMethod {
    fn from(value: ProcessingMethod) -> Self {
        Self {
            order: value.order,
            software_reference: value.software_reference,
            params: value.parameters.into_iter().map(|p| p.into()).collect(),
        }
    }
}

impl From<&mzdata::meta::ProcessingMethod> for ProcessingMethod {
    fn from(value: &mzdata::meta::ProcessingMethod) -> Self {
        Self {
            order: value.order,
            software_reference: value.software_reference.clone(),
            parameters: value.iter_params().cloned().map(MetaParam::from).collect(),
        }
    }
}

/// An adaptation of [`mzdata::meta::DataProcessing`]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DataProcessing {
    pub id: String,
    pub methods: Vec<ProcessingMethod>,
}

impl From<DataProcessing> for mzdata::meta::DataProcessing {
    fn from(value: DataProcessing) -> Self {
        Self {
            id: value.id,
            methods: value.methods.into_iter().map(|p| p.into()).collect(),
        }
    }
}

impl From<&mzdata::meta::DataProcessing> for DataProcessing {
    fn from(value: &mzdata::meta::DataProcessing) -> Self {
        Self {
            id: value.id.clone(),
            methods: value.methods.iter().map(|v| v.into()).collect(),
        }
    }
}

/// An adaptation of [`mzdata::meta::ComponentType`]
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentType {
    /// A mass analyzer
    Analyzer,
    /// A source for ions
    IonSource,
    /// An abundance measuring device
    Detector,
    #[default]
    Unknown,
}

/// An adaptation of [`mzdata::meta::Component`]
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    /// The kind of component this describes
    pub component_type: ComponentType,
    /// The order in the sequence of components that the analytes interact with
    pub order: u8,
    pub parameters: Vec<MetaParam>,
}

impl From<Component> for mzdata::meta::Component {
    fn from(value: Component) -> Self {
        Self {
            component_type: match value.component_type {
                ComponentType::Analyzer => mzdata::meta::ComponentType::Analyzer,
                ComponentType::IonSource => mzdata::meta::ComponentType::IonSource,
                ComponentType::Detector => mzdata::meta::ComponentType::Detector,
                ComponentType::Unknown => mzdata::meta::ComponentType::Unknown,
            },
            order: value.order,
            params: value
                .parameters
                .into_iter()
                .map(mzdata::Param::from)
                .collect(),
        }
    }
}

impl From<&mzdata::meta::Component> for Component {
    fn from(value: &mzdata::meta::Component) -> Self {
        Self {
            component_type: match value.component_type {
                mzdata::meta::ComponentType::Analyzer => ComponentType::Analyzer,
                mzdata::meta::ComponentType::IonSource => ComponentType::IonSource,
                mzdata::meta::ComponentType::Detector => ComponentType::Detector,
                mzdata::meta::ComponentType::Unknown => ComponentType::Unknown,
            },
            order: value.order,
            parameters: value.iter_params().cloned().map(MetaParam::from).collect(),
        }
    }
}

/// An adaptation of [`mzdata::meta::InstrumentConfiguration`]
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentConfiguration {
    /// The set of components involved
    pub components: Vec<Component>,
    /// A set of parameters that describe the instrument such as the model name or serial number
    pub parameters: Vec<MetaParam>,
    /// A reference to the data acquisition software involved in processing this configuration
    pub software_reference: String,
    /// A unique identifier translated to an ordinal identifying this configuration
    pub id: u32,
}

impl From<InstrumentConfiguration> for mzdata::meta::InstrumentConfiguration {
    fn from(value: InstrumentConfiguration) -> Self {
        Self {
            components: value.components.into_iter().map(|v| v.into()).collect(),
            params: value.parameters.into_iter().map(|v| v.into()).collect(),
            software_reference: value.software_reference,
            id: value.id,
        }
    }
}

impl From<&mzdata::meta::InstrumentConfiguration> for InstrumentConfiguration {
    fn from(value: &mzdata::meta::InstrumentConfiguration) -> Self {
        Self {
            components: value.components.iter().map(Component::from).collect(),
            parameters: value.iter_params().cloned().map(MetaParam::from).collect(),
            software_reference: value.software_reference.clone(),
            id: value.id,
        }
    }
}

/// An adaptation of [`mzdata::meta::Sample`]
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub id: String,
    pub name: Option<String>,
    pub parameters: Vec<MetaParam>,
}

impl From<Sample> for mzdata::meta::Sample {
    fn from(value: Sample) -> Self {
        Self {
            params: value.parameters.into_iter().map(|v| v.into()).collect(),
            name: value.name,
            id: value.id,
        }
    }
}

impl From<&mzdata::meta::Sample> for Sample {
    fn from(value: &mzdata::meta::Sample) -> Self {
        Self {
            parameters: value.iter_params().cloned().map(MetaParam::from).collect(),
            name: value.name.clone(),
            id: value.id.clone(),
        }
    }
}

/// A variadic data type meant to store a value that is either a path to a Parquet column
/// which holds the value for this entity that varies over rows, a constant [`CURIE`] or
/// no value stored, the equivalent of [`Option::None`].
///
/// Used primarily for denoting how to resolve the storage of [`Unit`] for a column.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathOrCURIE {
    /// The column path denoting where each row's [`CURIE`] for this entity lives
    Path(Vec<String>),
    /// A constant [`CURIE`] that applies to all rows
    CURIE(CURIE),
    /// No value is stored, as in [`Option::None`].
    #[default]
    None,
}

impl PathOrCURIE {
    /// The value is not [`Self::None`]
    pub fn is_defined(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// A [`Unit`] translates to just storing the CURIE for that unit.
impl From<Unit> for PathOrCURIE {
    fn from(value: Unit) -> Self {
        value.to_curie().map(|val| CURIE::from(val)).into()
    }
}

impl From<Option<CURIE>> for PathOrCURIE {
    fn from(value: Option<CURIE>) -> Self {
        match value {
            Some(v) => v.into(),
            None => Self::None,
        }
    }
}

impl From<Option<Vec<String>>> for PathOrCURIE {
    fn from(value: Option<Vec<String>>) -> Self {
        match value {
            Some(v) => v.into(),
            None => Self::None,
        }
    }
}

impl From<CURIE> for PathOrCURIE {
    fn from(v: CURIE) -> Self {
        Self::CURIE(v)
    }
}

impl From<Vec<String>> for PathOrCURIE {
    fn from(v: Vec<String>) -> Self {
        Self::Path(v)
    }
}

pub(crate) fn dot_path_ser<S>(path: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(path.join(".").as_str())
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum DotPath {
    Path(String),
    PathSegments(Vec<String>),
}

pub(crate) fn dot_path_de<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let res = DotPath::deserialize(deserializer)?;
    match res {
        DotPath::Path(v) => Ok(v.split('.').map(|v| v.to_string()).collect()),
        DotPath::PathSegments(items) => Ok(items),
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataColumn {
    /// A human-readable name for the parameter
    pub name: String,
    /// The path to the column in the Parquet file
    #[serde(serialize_with = "dot_path_ser", deserialize_with = "dot_path_de")]
    pub path: Vec<String>,
    /// The CURIE for the term this column refers to, if any
    #[serde(
        serialize_with = "opt_curie_serialize",
        deserialize_with = "opt_curie_deserialize"
    )]
    pub accession: Option<CURIE>,
    /// The CURIE for the unit of this column, the path to another column that holds it, or None
    #[serde(
        serialize_with = "path_or_curie_serialize",
        deserialize_with = "path_or_curie_deserialize",
        default
    )]
    pub unit: PathOrCURIE,
}

impl MetadataColumn {
    pub fn new(name: String, path: Vec<String>, accession: Option<CURIE>) -> Self {
        Self {
            name,
            path,
            accession,
            unit: PathOrCURIE::None,
        }
    }

    /// Specify the unit definition
    pub fn with_unit(mut self, value: impl Into<PathOrCURIE>) -> Self {
        self.unit = value.into();
        self
    }

    /// Get the last element in the path
    pub fn leaf(&self) -> Option<&str> {
        self.path.last().map(|s| s.as_str())
    }

    /// Get a "."-joined path as a string
    pub fn path_string(&self) -> String {
        self.path.join(".")
    }

    /// Create a [`ProjectionMask`] for the mapped column, plus `num_indices` index columns
    /// in the beginning of the schema.
    pub fn as_projection_mask<T>(
        &self,
        builder: &ArrowReaderBuilder<T>,
        num_indices: usize,
    ) -> ProjectionMask {
        let schema = builder.parquet_schema().clone();
        let mut cols = Vec::with_capacity(num_indices + 1);
        for i in 0..=num_indices {
            let path = schema.column(i).path().string();
            cols.push(path);
        }
        cols.push(self.path_string());
        ProjectionMask::columns(&schema, cols.iter().map(|s| s.as_str()))
    }

    /// Retrieve Parquet metadata statistics from an [`ArrowReaderBuilder`]
    pub fn parquet_statistics<T>(
        &self,
        builder: &ArrowReaderBuilder<T>,
    ) -> (Option<ArrayRef>, Option<ArrayRef>) {
        let meta = builder.metadata();
        let schema = builder.parquet_schema();
        let mut min_builder: Option<Box<dyn ArrayBuilder>> = None;
        let mut max_builder: Option<Box<dyn ArrayBuilder>> = None;

        if let Some((i, _col)) = schema
            .columns()
            .iter()
            .enumerate()
            .find(|(_, v)| v.path().parts() == self.path.as_slice())
        {
            for rg in meta.row_groups() {
                let col_meta = rg.column(i);
                if let Some(stats) = col_meta.statistics() {
                    match stats {
                        parquet::file::statistics::Statistics::Boolean(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(BooleanBuilder::new()));
                                max_builder = Some(Box::new(BooleanBuilder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BooleanBuilder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt().copied());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BooleanBuilder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt().copied());
                        }
                        parquet::file::statistics::Statistics::Int32(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(Int32Builder::new()));
                                max_builder = Some(Box::new(Int32Builder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Int32Builder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt().copied());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Int32Builder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt().copied());
                        }
                        parquet::file::statistics::Statistics::Int64(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(Int64Builder::new()));
                                max_builder = Some(Box::new(Int64Builder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Int64Builder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt().copied());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Int64Builder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt().copied());
                        }
                        parquet::file::statistics::Statistics::Int96(_value_statistics) => todo!(),
                        parquet::file::statistics::Statistics::Float(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(Float32Builder::new()));
                                max_builder = Some(Box::new(Float32Builder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Float32Builder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt().copied());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Float32Builder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt().copied());
                        }
                        parquet::file::statistics::Statistics::Double(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(Float64Builder::new()));
                                max_builder = Some(Box::new(Float64Builder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Float64Builder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt().copied());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<Float64Builder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt().copied());
                        }
                        parquet::file::statistics::Statistics::ByteArray(value_statistics) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(BinaryBuilder::new()));
                                max_builder = Some(Box::new(BinaryBuilder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BinaryBuilder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BinaryBuilder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt());
                        }
                        parquet::file::statistics::Statistics::FixedLenByteArray(
                            value_statistics,
                        ) => {
                            if min_builder.is_none() {
                                min_builder = Some(Box::new(BinaryBuilder::new()));
                                max_builder = Some(Box::new(BinaryBuilder::new()));
                            }
                            min_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BinaryBuilder>()
                                .unwrap()
                                .append_option(value_statistics.min_opt());
                            max_builder
                                .as_mut()
                                .unwrap()
                                .as_any_mut()
                                .downcast_mut::<BinaryBuilder>()
                                .unwrap()
                                .append_option(value_statistics.max_opt());
                        }
                    }
                }
            }
        }
        (
            min_builder.map(|mut v| v.finish()),
            max_builder.map(|mut v| v.finish()),
        )
    }
}

/// A thin wrapper around `Vec<MetadataColumn>` with added functionality
///
/// Implements [`Deref`] for immutable operations on the underlying data,
/// but uses [`AsMut`] for mutable ones.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetadataColumnCollection(Vec<MetadataColumn>);

impl MetadataColumnCollection {
    /// Find a column by it's [`CURIE`].
    ///
    /// This is short-hand for [`Iterator::find`]:
    /// ```no_test
    /// self.iter().find(|c| c.accession == Some(curie))
    /// ```
    pub fn find(&self, curie: CURIE) -> Option<&MetadataColumn> {
        self.0.iter().find(|c| c.accession == Some(curie))
    }

    /// See [`Vec::push`]
    pub fn push(&mut self, value: MetadataColumn) {
        self.0.push(value)
    }

    /// Create a [`ProjectionMask`] for the columns mapped by the elements of this collection, plus `num_indices` index columns
    /// in the beginning of the schema.
    ///
    /// # Warning
    /// The order of columns in the resulting [`RecordBatch`](arrow::array::RecordBatch) will match the order
    /// of the columns in the Parquet file's schema, *not* the order of the columns in this collection.
    pub fn as_projection_mask<T>(
        &self,
        builder: &ArrowReaderBuilder<T>,
        num_indices: usize,
    ) -> ProjectionMask {
        let schema = builder.parquet_schema().clone();
        let mut cols = Vec::with_capacity(num_indices + self.len());
        for i in 0..=num_indices {
            let path = schema.column(i).path().string();
            cols.push(path);
        }
        for c in self.iter() {
            cols.push(c.path_string());
        }
        ProjectionMask::columns(&schema, cols.iter().map(|s| s.as_str()))
    }
}

impl IntoIterator for MetadataColumnCollection {
    type Item = MetadataColumn;

    type IntoIter = vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a MetadataColumnCollection {
    type Item = &'a MetadataColumn;

    type IntoIter = core::slice::Iter<'a, MetadataColumn>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl From<Vec<MetadataColumn>> for MetadataColumnCollection {
    fn from(value: Vec<MetadataColumn>) -> Self {
        Self(value)
    }
}

impl Deref for MetadataColumnCollection {
    type Target = [MetadataColumn];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsMut<Vec<MetadataColumn>> for MetadataColumnCollection {
    fn as_mut(&mut self) -> &mut Vec<MetadataColumn> {
        &mut self.0
    }
}

/// A hierarchical version of [`MetadataColumnCollection`] that handles nested columns
/// more effectively.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetadataMapping {
    columns: MetadataColumnCollection,
    path: Vec<String>,
    members: HashMap<String, MetadataMapping>,
    column_map: Option<HashMap<String, usize>>,
}

struct MetadataTreeIter<'a> {
    current: Option<core::slice::Iter<'a, MetadataColumn>>,
    queue: VecDeque<&'a MetadataColumnCollection>,
}

impl<'a> ExactSizeIterator for MetadataTreeIter<'a> {
    fn len(&self) -> usize {
        let z = self.current.as_ref().map(|s| s.len()).unwrap_or_default();
        z + self.queue.iter().map(|v| v.len()).sum::<usize>()
    }
}

impl<'a> Iterator for MetadataTreeIter<'a> {
    type Item = &'a MetadataColumn;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.current.as_mut() {
                Some(it) => match it.next() {
                    Some(val) => return Some(val),
                    None => {
                        self.current = self.queue.pop_front().map(|v| v.iter());
                    }
                },
                None => return None,
            }
        }
    }
}

pub struct MetadataMappingIntoIter {
    current: Option<std::vec::IntoIter<MetadataColumn>>,
    queue: VecDeque<MetadataColumnCollection>,
}

impl Iterator for MetadataMappingIntoIter {
    type Item = MetadataColumn;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.current.as_mut() {
                Some(it) => match it.next() {
                    Some(val) => return Some(val),
                    None => {
                        self.current = self.queue.pop_front().map(|v| v.into_iter());
                    }
                },
                None => return None,
            }
        }
    }
}

impl ExactSizeIterator for MetadataMappingIntoIter {
    fn len(&self) -> usize {
        let z = self.current.as_ref().map(|s| s.len()).unwrap_or_default();
        z + self.queue.iter().map(|v| v.len()).sum::<usize>()
    }
}

impl MetadataMapping {
    pub fn new(
        columns: MetadataColumnCollection,
        path: Vec<String>,
        members: HashMap<String, MetadataMapping>,
    ) -> Self {
        Self {
            columns,
            path,
            members,
            column_map: None,
        }
    }

    fn collect_node<'a>(&'a self, queue: &mut VecDeque<&'a MetadataColumnCollection>) {
        queue.push_back(&self.columns);
        for node in self.members.values() {
            node.collect_node(queue);
        }
    }

    fn collect_node_owned(self, queue: &mut VecDeque<MetadataColumnCollection>) {
        queue.push_back(self.columns);
        for node in self.members.into_values() {
            node.collect_node_owned(queue);
        }
    }

    pub fn iter<'a>(&'a self) -> impl Iterator<Item = &'a MetadataColumn> + 'a {
        let mut queue = VecDeque::new();
        self.collect_node(&mut queue);
        let it = queue.pop_front().map(|v| v.iter());
        MetadataTreeIter { current: it, queue }
    }

    /// Rebuild the name lookup map for `columns` used by `traverse`
    pub fn rebuild_column_maps(&mut self) {
        self.column_map = Some(
            self.columns
                .iter()
                .enumerate()
                .map(|(i, v)| (v.path.last().unwrap().to_string(), i))
                .collect(),
        );
        for child in self.members.values_mut() {
            child.rebuild_column_maps();
        }
    }

    /// Traverse the tree from this node down the `path` to find `name`
    pub fn traverse<Q: Borrow<str>>(&self, path: &[Q], name: &str) -> Option<&MetadataColumn> {
        let mut node = self;
        for p in path {
            node = node.members.get(p.borrow())?;
        }
        match node.column_map.as_ref().and_then(|v| v.get(name).copied()) {
            Some(i) => self.columns.get(i),
            None => node.columns.iter().find(|v| v.path.last().unwrap() == name),
        }
    }

    /// The path to this node
    #[inline(always)]
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// Get access to the list of columns in this node
    #[inline(always)]
    pub fn columns(&self) -> &MetadataColumnCollection {
        &self.columns
    }

    pub fn find(&self, curie: CURIE) -> Option<&MetadataColumn> {
        self.columns.find(curie)
    }

    /// Get access to the child node map
    #[inline(always)]
    pub fn members(&self) -> &HashMap<String, MetadataMapping> {
        &self.members
    }

    /// Get a child node by name
    #[inline(always)]
    pub fn member(&self, key: &str) -> Option<&MetadataMapping> {
        self.members.get(key)
    }
}

impl From<Vec<MetadataColumn>> for MetadataMapping {
    fn from(value: Vec<MetadataColumn>) -> Self {
        MetadataColumnCollection::from(value).into()
    }
}

impl From<MetadataColumnCollection> for MetadataMapping {
    fn from(value: MetadataColumnCollection) -> Self {
        let mut by_prefix: HashMap<Vec<String>, Vec<MetadataColumn>> =
            HashMap::with_capacity(value.len());

        for col in value {
            let n = col.path.len();
            // This is a top-level leaf node
            if n == 1 {
                by_prefix.entry(Vec::new()).or_default().push(col);
            } else {
                // Otherwise this is a child's leaf node (no internal nodes exist)
                let prefix: Vec<String> = col.path[..n - 1].iter().map(|s| s.to_string()).collect();
                by_prefix.entry(prefix).or_default().push(col);
            }
        }

        // Re-order the paths in ascending order
        let mut by_prefix: Vec<(_, _)> = by_prefix.into_iter().collect();
        by_prefix.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));

        let mut root = MetadataMapping::default();
        for (prefix, cols) in by_prefix {
            if prefix.is_empty() {
                root.columns.as_mut().extend(cols);
            } else {
                // Walk down the path, and create nodes along the way
                let n = prefix.len();
                let mut node = &mut root;
                for i in 0..n {
                    node = node.members.entry(prefix[i].clone()).or_insert_with(|| {
                        MetadataMapping::new(
                            Default::default(),
                            // Initialize the prefix of the intermediate or leaf nodes
                            prefix[0..=i].iter().cloned().collect(),
                            Default::default(),
                        )
                    });
                }
                // When we've reached the end of the path, set the leaf node's columns
                *node.columns.as_mut() = cols.into();
            }
        }
        root.rebuild_column_maps();
        root
    }
}

impl IntoIterator for MetadataMapping {
    type Item = MetadataColumn;

    type IntoIter = MetadataMappingIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        let mut queue = VecDeque::new();
        self.collect_node_owned(&mut queue);
        let current = queue.pop_front().map(|v| v.into_iter());
        MetadataMappingIntoIter { current, queue }
    }
}

#[cfg(test)]
mod test {
    use arrow::{
        array::AsArray,
        datatypes::{DataType, Float64Type},
    };

    use super::*;
    use std::io;

    #[test]
    fn test_metadata_col_serde() -> io::Result<()> {
        let cols = crate::spectrum::SpectrumEntry::metadata_columns();
        let text = serde_json::to_string(&cols)?;

        let dups: Vec<super::MetadataColumn> = serde_json::from_str(&text)?;

        assert_eq!(cols, dups);

        Ok(())
    }

    #[test]
    fn test_spectrum_schema_map() {
        let cols: Vec<MetadataColumn> = crate::spectrum::SpectrumEntry::metadata_columns()
            .into_iter()
            .map(|mut v| {
                v.path.remove(0);
                v
            })
            .collect();
        let n = cols.len();
        let mapping = MetadataMapping::from(cols);
        assert_eq!(mapping.columns.len(), n);
        assert_eq!(mapping.members.len(), 0);
        assert_eq!(mapping.path.len(), 0);

        let cols: Vec<MetadataColumn> = serde_json::from_str(r#"[{"name": "scan start time", "path": ["scan_start_time"], "index": 0, "accession": "MS:1000016", "unit": "UO:0000031"}, {"name": "preset scan configuration", "path": ["preset_scan_configuration"], "index": null, "accession": "MS:1000616", "unit": null}, {"name": "filter string", "path": ["filter_string"], "index": null, "accession": "MS:1000512", "unit": null}, {"name": "ion injection time", "path": ["ion_injection_time"], "index": 0, "accession": "MS:1000927", "unit": "UO:0000028"}, {"name": "scan window lower limit", "path": ["scan_windows", "scan_window_lower_limit"], "index": 0, "accession": "MS:1000501", "unit": "MS:1000040"}, {"name": "scan window upper limit", "path": ["scan_windows", "scan_window_upper_limit"], "index": 0, "accession": "MS:1000500", "unit": "MS:1000040"}]"#).unwrap();
        let n = cols.len();
        let mapping = MetadataMapping::from(cols);
        assert_eq!(mapping.columns.len(), 4);
        assert_eq!(mapping.members.len(), 1);
        let window_mapping = mapping.members.get("scan_windows").unwrap();
        assert_eq!(window_mapping.columns.len(), 2);
        assert_eq!(window_mapping.columns.len() + mapping.columns.len(), n);
        assert_eq!(window_mapping.path, vec!["scan_windows"]);
    }

    use crate::{archive::AnyArchiveReader, param::MetadataColumnCollection};

    #[test_log::test]
    #[rstest::rstest]
    fn test_archive_slice_projection() -> io::Result<()> {
        let archive = AnyArchiveReader::from_path("small.mzpeak".into())?;
        let builder = archive.spectrum_metadata_selected_ions()?;
        let target = MetadataColumn::new(
            "selected ion m/z".into(),
            vec!["selected_ion_mz".into()],
            Some(curie!(MS:1000744)),
        );

        let mask = target.as_projection_mask(&builder, 2);
        let reader = builder.with_projection(mask).with_batch_size(10).build()?;

        let mut k = 0;
        for batch in reader.flatten() {
            assert_eq!(batch.num_columns(), 3);
            k += batch.num_rows();
            assert_eq!(batch.column(0).data_type().clone(), DataType::UInt64);
            assert_eq!(batch.column(1).data_type().clone(), DataType::UInt64);
            assert_eq!(batch.column(2).data_type().clone(), DataType::Float64);
        }
        assert_eq!(k, 34);
        Ok(())
    }

    #[test_log::test]
    fn test_archive_statistics() -> io::Result<()> {
        let archive = AnyArchiveReader::from_path("small.mzpeak".into())?;
        let builder = archive.spectrum_metadata_selected_ions()?;
        let target = MetadataColumn::new(
            "selected ion m/z".into(),
            vec!["selected_ion_mz".into()],
            Some(curie!(MS:1000744)),
        );
        let (min, max) = target.parquet_statistics(&builder);
        assert!(min.is_some());
        assert!(max.is_some());

        let min = min.unwrap();
        let max = max.unwrap();
        assert_eq!(min.len(), 1);
        assert_eq!(max.len(), 1);
        let min_val = min
            .as_primitive::<Float64Type>()
            .iter()
            .min_by(|a, b| a.unwrap().total_cmp(&b.unwrap()))
            .unwrap()
            .unwrap();
        let max_val = max
            .as_primitive::<Float64Type>()
            .iter()
            .max_by(|a, b| a.unwrap().total_cmp(&b.unwrap()))
            .unwrap()
            .unwrap();
        let e = min_val - 558.749389648438 ;
        assert!(e.abs() < 1e-6, "{min_val} error {e} > 1e-6");
        let e = max_val - 882.535034179688 ;
        assert!(e.abs() < 1e-6, "{max_val} error {e} > 1e-6");
        Ok(())
    }

    #[test_log::test]
    #[rstest::rstest]
    fn test_archive_slice_projection_multiple() -> io::Result<()> {
        let archive = AnyArchiveReader::from_path("small.mzpeak".into())?;
        let builder = archive.spectrum_metadata_selected_ions()?;

        let target1 = MetadataColumn::new(
            "selected ion m/z".into(),
            vec!["selected_ion_mz".into()],
            Some(curie!(MS:1000744)),
        );

        let target2 = MetadataColumn::new(
            "intensity".into(),
            vec!["peak_intensity".into()],
            Some(curie!(MS:1000042)),
        );

        let target = MetadataColumnCollection::from(vec![target2, target1]);

        let mask = target.as_projection_mask(&builder, 2);
        let reader = builder.with_projection(mask).with_batch_size(10).build()?;

        let mut k = 0;
        for batch in reader.flatten() {
            assert_eq!(batch.num_columns(), 4);
            k += batch.num_rows();
            assert_eq!(batch.column(0).data_type().clone(), DataType::UInt64);
            assert_eq!(batch.column(1).data_type().clone(), DataType::UInt64);
            assert_eq!(batch.column(2).data_type().clone(), DataType::Float64);
            assert_eq!(batch.column(3).data_type().clone(), DataType::Float32);
        }
        assert_eq!(k, 34);
        Ok(())
    }
}
