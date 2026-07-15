use std::{
    collections::{HashMap, HashSet},
    io,
    ops::Deref,
    sync::Arc,
};

use crate::{
    BufferContext,
    archive::{ArchiveReader, ArchiveSource, DataKind, FileIndex},
    buffer_descriptors::{ArrayIndex, SerializedArrayIndex, arrow_to_array_type},
    constants::{
        CHROMATOGRAM, CHROMATOGRAM_ARRAY_INDEX, INDEX,
        PRECURSOR, SCAN, SELECTED_ION, SOURCE_INDEX, SPECTRUM,
        SPECTRUM_ARRAY_INDEX, SPECTRUM_INDEX, WAVELENGTH_SPECTRUM_ARRAY_INDEX,
    },
    filter::RegressionDeltaModel,
    param::MetadataMapping,
    reader::{
        index::{QueryIndex, SpectrumDataIndex, SpectrumMetadataIndexLike, SpectrumPointIndex},
        utils::MaskSet,
        visitor::{
            CompoundIndexVisitor, DoubleIndexed, Indexed, MzChromatogramBuilder,
            MzPrecursorVisitor, MzScanVisitor, MzSelectedIonVisitor, MzSpectrumVisitor,
        },
    },
};
use arrow::{
    array::{Array, AsArray, RecordBatch, StructArray, UInt64Array},
    datatypes::{DataType, Float32Type, Float64Type, Int32Type, Int64Type, UInt32Type, UInt64Type},
};
use identity_hash::BuildIdentityHasher;
use itertools::Itertools;
use mzdata::{
    curie, io::OffsetIndex, meta, prelude::*, spectrum::{
        ArrayType, BinaryDataArrayType, ChromatogramDescription, DataArray, Precursor, ScanEvent,
        SelectedIon, SpectrumDescription,
    }
};
use mzpeaks::coordinate::SimpleInterval;
use parquet::{
    arrow::{
        ProjectionMask,
        arrow_reader::{
            ArrowPredicateFn, ArrowReaderBuilder, ParquetRecordBatchReaderBuilder, RowSelection,
        },
    },
    file::{
        metadata::ParquetMetaData,
        reader::ChunkReader,
    },
    schema::types::SchemaDescPtr,
};

pub trait ReaderFacetMetadataLike {
    fn array_indices(&self) -> &Arc<ArrayIndex>;
    fn id_index(&self) -> &OffsetIndex;
    fn primary_metadata_map(&self) -> Option<&MetadataMapping>;
    fn precursor_metadata_map(&self) -> Option<&MetadataMapping>;
    fn scan_metadata_map(&self) -> Option<&MetadataMapping>;
    fn selected_ion_metadata_map(&self) -> Option<&MetadataMapping>;
    fn auxiliary_array_counts(&self) -> &[u32];
}

#[derive(Debug, Clone, Default)]
pub struct SpectrumMetadataFacet {
    pub(crate) array_indices: Arc<ArrayIndex>,
    pub(crate) id_index: OffsetIndex,
    pub(crate) mz_model_deltas: Vec<Option<Vec<f64>>>,
    pub(crate) auxiliary_array_counts: Vec<u32>,
    pub(crate) spectrum_metadata_map: Option<MetadataMapping>,
    pub(crate) scan_metadata_map: Option<MetadataMapping>,
    pub(crate) precursor_metadata_map: Option<MetadataMapping>,
    pub(crate) selected_ion_metadata_map: Option<MetadataMapping>,
    pub(crate) peak_indices: Option<PeakMetadata>,
    pub(crate) data_point_counts: Vec<u64>,
    pub(crate) peak_counts: Vec<u64>,
}

impl ReaderFacetMetadataLike for SpectrumMetadataFacet {
    fn array_indices(&self) -> &Arc<ArrayIndex> {
        &self.array_indices
    }

    fn id_index(&self) -> &OffsetIndex {
        &self.id_index
    }

    fn primary_metadata_map(&self) -> Option<&MetadataMapping> {
        self.spectrum_metadata_map.as_ref()
    }

    fn scan_metadata_map(&self) -> Option<&MetadataMapping> {
        self.scan_metadata_map.as_ref()
    }

    fn precursor_metadata_map(&self) -> Option<&MetadataMapping> {
        self.precursor_metadata_map.as_ref()
    }

    fn selected_ion_metadata_map(&self) -> Option<&MetadataMapping> {
        self.selected_ion_metadata_map.as_ref()
    }

    fn auxiliary_array_counts(&self) -> &[u32] {
        &self.auxiliary_array_counts
    }
}

impl SpectrumMetadataFacet {
    pub fn new(
        spectrum_array_indices: Arc<ArrayIndex>,
        spectrum_id_index: OffsetIndex,
        mz_model_deltas: Vec<Option<Vec<f64>>>,
        spectrum_auxiliary_array_counts: Vec<u32>,
        spectrum_metadata_map: Option<MetadataMapping>,
        scan_metadata_map: Option<MetadataMapping>,
        precursor_metadata_map: Option<MetadataMapping>,
        selected_ion_metadata_map: Option<MetadataMapping>,
        peak_indices: Option<PeakMetadata>,
        data_point_counts: Vec<u64>,
        peak_counts: Vec<u64>,
    ) -> Self {
        Self {
            array_indices: spectrum_array_indices,
            id_index: spectrum_id_index,
            mz_model_deltas,
            auxiliary_array_counts: spectrum_auxiliary_array_counts,
            spectrum_metadata_map,
            scan_metadata_map,
            precursor_metadata_map,
            selected_ion_metadata_map,
            peak_indices,
            data_point_counts,
            peak_counts,
        }
    }

    pub fn data_point_counts(&self) -> &[u64] {
        &self.data_point_counts
    }

    pub fn peak_counts(&self) -> &[u64] {
        &self.peak_counts
    }
}

#[derive(Debug, Clone, Default)]
pub struct WavelengthSpectrumMetadataFacet {
    pub(crate) array_indices: Arc<ArrayIndex>,
    pub(crate) id_index: OffsetIndex,
    pub(crate) auxiliary_array_counts: Vec<u32>,
    pub(crate) spectrum_metadata_map: Option<MetadataMapping>,
    pub(crate) scan_metadata_map: Option<MetadataMapping>,
}

impl ReaderFacetMetadataLike for WavelengthSpectrumMetadataFacet {
    fn array_indices(&self) -> &Arc<ArrayIndex> {
        &self.array_indices
    }

    fn id_index(&self) -> &OffsetIndex {
        &self.id_index
    }

    fn primary_metadata_map(&self) -> Option<&MetadataMapping> {
        self.spectrum_metadata_map.as_ref()
    }

    fn scan_metadata_map(&self) -> Option<&MetadataMapping> {
        self.scan_metadata_map.as_ref()
    }

    fn precursor_metadata_map(&self) -> Option<&MetadataMapping> {
        None
    }

    fn selected_ion_metadata_map(&self) -> Option<&MetadataMapping> {
        None
    }

    fn auxiliary_array_counts(&self) -> &[u32] {
        &self.auxiliary_array_counts
    }
}

#[derive(Debug, Clone, Default)]
pub struct ChromatogramMetadataFacet {
    pub(crate) array_indices: Arc<ArrayIndex>,
    pub(crate) id_index: OffsetIndex,
    pub(crate) auxiliary_array_counts: Vec<u32>,
    pub(crate) chromatogram_metadata_map: Option<MetadataMapping>,
    pub(crate) precursor_metadata_map: Option<MetadataMapping>,
    pub(crate) selected_ion_metadata_map: Option<MetadataMapping>,
}

impl ChromatogramMetadataFacet {
    pub fn new(
        array_indices: Arc<ArrayIndex>,
        id_index: OffsetIndex,
        auxiliary_array_counts: Vec<u32>,
        chromatogram_metadata_map: Option<MetadataMapping>,
        precursor_metadata_map: Option<MetadataMapping>,
        selected_ion_metadata_map: Option<MetadataMapping>,
    ) -> Self {
        Self {
            array_indices,
            id_index,
            auxiliary_array_counts,
            chromatogram_metadata_map,
            precursor_metadata_map,
            selected_ion_metadata_map,
        }
    }
}

impl ReaderFacetMetadataLike for ChromatogramMetadataFacet {
    fn array_indices(&self) -> &Arc<ArrayIndex> {
        &self.array_indices
    }

    fn id_index(&self) -> &OffsetIndex {
        &self.id_index
    }

    fn primary_metadata_map(&self) -> Option<&MetadataMapping> {
        self.chromatogram_metadata_map.as_ref()
    }

    fn scan_metadata_map(&self) -> Option<&MetadataMapping> {
        None
    }

    fn precursor_metadata_map(&self) -> Option<&MetadataMapping> {
        self.precursor_metadata_map.as_ref()
    }

    fn selected_ion_metadata_map(&self) -> Option<&MetadataMapping> {
        self.selected_ion_metadata_map.as_ref()
    }

    fn auxiliary_array_counts(&self) -> &[u32] {
        &self.auxiliary_array_counts
    }
}

#[derive(Debug, Clone)]
pub struct ReaderMetadata {
    pub(crate) mz_metadata: mzdata::meta::FileMetadataConfig,
    pub(crate) spectra: SpectrumMetadataFacet,
    pub(crate) chromatograms: ChromatogramMetadataFacet,
    pub(crate) wavelength_spectra: Option<Box<WavelengthSpectrumMetadataFacet>>,
}

const EMPTY_U32_SLC: &'static [u32] = &[];

impl ReaderMetadata {
    pub fn new(
        mz_metadata: mzdata::meta::FileMetadataConfig,
        spectra: SpectrumMetadataFacet,
        chromatograms: ChromatogramMetadataFacet,
        wavelength_spectra: Option<Box<WavelengthSpectrumMetadataFacet>>,
    ) -> Self {
        Self {
            mz_metadata,
            spectra,
            chromatograms,
            wavelength_spectra,
        }
    }

    pub fn model_deltas_for(&self, index: usize) -> Option<RegressionDeltaModel<f64>> {
        self.spectra
            .mz_model_deltas
            .get(index)
            .cloned()
            .unwrap_or_default()
            .map(|v| RegressionDeltaModel::from(v))
    }

    pub fn spectrum_auxiliary_array_counts(&self) -> &[u32] {
        &self.spectra.auxiliary_array_counts
    }

    pub fn chromatogram_auxiliary_array_counts(&self) -> &[u32] {
        &self.chromatograms.auxiliary_array_counts
    }

    pub fn wavelength_auxiliary_array_counts(&self) -> &[u32] {
        if let Some(props) = self.wavelength_spectra.as_ref() {
            &props.auxiliary_array_counts
        } else {
            EMPTY_U32_SLC
        }
    }

    pub fn peak_array_indices(&self) -> Option<&ArrayIndex> {
        self.spectra
            .peak_indices
            .as_ref()
            .map(|v| v.array_indices.as_ref())
    }

    pub fn spectrum_array_indices(&self) -> &ArrayIndex {
        &self.spectra.array_indices
    }

    pub fn chromatogram_array_indices(&self) -> &ArrayIndex {
        &self.chromatograms.array_indices()
    }

    pub fn wavelength_spectrum_array_index(&self) -> Option<&ArrayIndex> {
        self.wavelength_spectra
            .as_ref()
            .map(|s| s.array_indices.deref())
    }

    pub fn file_metadata(&self) -> &mzdata::meta::FileMetadataConfig {
        &self.mz_metadata
    }
}

impl MSDataFileMetadata for ReaderMetadata {
    mzdata::delegate_impl_metadata_trait!(mz_metadata);
}

pub(crate) fn build_id_index<T: ArchiveSource>(
    handle: ParquetRecordBatchReaderBuilder<T::File>,
    prefix: &str,
) -> io::Result<OffsetIndex> {
    let mut id_index = OffsetIndex::new(prefix.to_string());
    let pq_schema = handle.parquet_schema();
    let mask = ProjectionMask::columns(
        pq_schema,
        [format!("id").as_str(), format!("index").as_str()],
    );
    for batch in handle.with_projection(mask).build()?.flatten() {
        let root = batch;

        let indices: &UInt64Array = root
            .column_by_name(INDEX)
            .unwrap()
            .as_any()
            .downcast_ref()
            .unwrap();
        let ids = root.column_by_name("id").unwrap();
        macro_rules! read_ids {
            ($ids:expr) => {
                for (id, idx) in $ids.iter().zip(indices.iter()) {
                    if let Some(id) = id {
                        id_index.insert(id, idx.unwrap());
                    }
                }
            };
        }
        if let Some(ids) = ids.as_string_opt::<i64>() {
            read_ids!(ids);
        } else if let Some(ids) = ids.as_string_opt::<i32>() {
            read_ids!(ids);
        } else {
            panic!("Unsupported data type: {:?}", ids.data_type());
        }
    }
    id_index.init = true;
    Ok(id_index)
}

#[derive(Debug, Default, Clone)]
pub struct PeakMetadata {
    pub array_indices: Arc<ArrayIndex>,
    pub query_index: SpectrumDataIndex,
}

impl PeakMetadata {
    pub fn new(array_indices: Arc<ArrayIndex>, query_index: SpectrumDataIndex) -> Self {
        Self {
            array_indices,
            query_index,
        }
    }

    pub fn from_metadata<T>(reader: &ArrowReaderBuilder<T>) -> Option<Self> {
        let metadata = reader.metadata();
        let mut this = Self::default();
        let mut has_arrays = false;
        if let Some(kvs) = metadata.file_metadata().key_value_metadata() {
            for kv in kvs {
                match kv.key.as_str() {
                    "spectrum_array_index" => {
                        if let Some(data) = kv.value.as_deref() {
                            this.array_indices = Arc::new(ArrayIndex::from_json(data));
                            has_arrays = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if has_arrays {
            let index = SpectrumDataIndex::Point(SpectrumPointIndex::from_reader(
                reader,
                &this.array_indices,
            ));
            this.query_index = index;
            Some(this)
        } else {
            None
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ParquetIndexExtractor {
    pub mz_metadata: meta::FileMetadataConfig,

    pub spectra: SpectrumMetadataFacet,
    pub chromatograms: ChromatogramMetadataFacet,
    pub wavelength_spectra: Option<Box<WavelengthSpectrumMetadataFacet>>,

    pub query_index: QueryIndex,
}

impl ParquetIndexExtractor {
    pub(crate) fn load_spectrum_metadata_indices<T>(
        &mut self,
        spectrum_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        self.query_index
            .populate_spectrum_metadata_indices(spectrum_metadata_reader);
    }

    pub(crate) fn load_spectrum_metadata_scan_indices<T>(
        &mut self,
        spectrum_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        self.query_index
            .populate_spectrum_scan_indices(spectrum_metadata_reader);
    }

    pub(crate) fn load_spectrum_metadata_precursor_indices<T>(
        &mut self,
        spectrum_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        self.query_index
            .populate_spectrum_precursor_indices(spectrum_metadata_reader);
    }

    pub(crate) fn load_spectrum_metadata_selected_ion_indices<T>(
        &mut self,
        spectrum_metadata_reader: &ArrowReaderBuilder<T>,
    ) {
        self.query_index
            .populate_spectrum_selected_ion_indices(spectrum_metadata_reader);
    }

    pub(crate) fn load_metadata_mapping_from_index(&mut self, file_index: &FileIndex) {
        for f in file_index.iter() {
            log::trace!("Visiting {f:?} from file index");
            match &f.entity_type {
                crate::archive::EntityType::Spectrum => match f.data_kind {
                    crate::archive::DataKind::DataArray
                    | crate::archive::DataKind::Peaks
                    | crate::archive::DataKind::Other(_)
                    | crate::archive::DataKind::Proprietary => {}
                    crate::archive::DataKind::Metadata => {
                        self.spectra.spectrum_metadata_map = Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::Scans => {
                        self.spectra.scan_metadata_map = Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::Precursors => {
                        self.spectra.precursor_metadata_map =
                            Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::SelectedIons => {
                        self.spectra.selected_ion_metadata_map =
                            Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::Products => {}
                },
                crate::archive::EntityType::Chromatogram => match f.data_kind {
                    crate::archive::DataKind::DataArray
                    | crate::archive::DataKind::Peaks
                    | crate::archive::DataKind::Other(_)
                    | crate::archive::DataKind::Scans
                    | crate::archive::DataKind::Proprietary => {}
                    crate::archive::DataKind::Metadata => {
                        self.chromatograms.chromatogram_metadata_map =
                            Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::Precursors => {
                        self.chromatograms.precursor_metadata_map =
                            Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::SelectedIons => {
                        self.chromatograms.selected_ion_metadata_map =
                            Some(f.column_mapping.clone().into())
                    }
                    crate::archive::DataKind::Products => todo!(),
                },
                crate::archive::EntityType::WavelengthSpectrum => {
                    let v = self.wavelength_spectra.get_or_insert_default();
                    match f.data_kind {
                        crate::archive::DataKind::DataArray
                        | crate::archive::DataKind::Peaks
                        | crate::archive::DataKind::Other(_)
                        | crate::archive::DataKind::Products
                        | crate::archive::DataKind::Precursors
                        | crate::archive::DataKind::SelectedIons
                        | crate::archive::DataKind::Proprietary => {}
                        crate::archive::DataKind::Metadata => {
                            v.spectrum_metadata_map = Some(f.column_mapping.clone().into())
                        }
                        crate::archive::DataKind::Scans => {
                            v.scan_metadata_map = Some(f.column_mapping.clone().into())
                        }
                    }
                }
                crate::archive::EntityType::Other(_) => {},
            }
        }
    }

    pub(crate) fn visit_spectrum_data_reader<T>(
        &mut self,
        spectrum_data_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        for kv in spectrum_data_reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .into_iter()
            .flatten()
        {
            match kv.key.as_str() {
                SPECTRUM_ARRAY_INDEX => {
                    if let Some(val) = kv.value.as_ref() {
                        let array_index: SerializedArrayIndex = serde_json::from_str(&val)?;
                        self.spectra.array_indices = Arc::new(array_index.into());
                    } else {
                        log::warn!("spectrum array index was empty");
                    }
                }
                _ => {}
            }
        }
        self.query_index
            .populate_spectrum_data_indices(&spectrum_data_reader, &self.spectra.array_indices);
        Ok(())
    }

    pub(crate) fn visit_wavelength_spectrum_data_reader<T>(
        &mut self,
        wavelength_spectrum_data_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        for kv in wavelength_spectrum_data_reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .into_iter()
            .flatten()
        {
            match kv.key.as_str() {
                WAVELENGTH_SPECTRUM_ARRAY_INDEX => {
                    if let Some(val) = kv.value.as_ref() {
                        let array_index: SerializedArrayIndex = serde_json::from_str(&val)?;
                        let mut meta = self.wavelength_spectra.take().unwrap_or_default();
                        meta.array_indices = Arc::new(array_index.into());
                        self.query_index.populate_wavelength_spectrum_data_indices(
                            &wavelength_spectrum_data_reader,
                            &meta.array_indices,
                        );
                        self.wavelength_spectra = Some(meta);
                    } else {
                        log::warn!("wavelength spectrum array index was empty");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn visit_wavelength_spectrum_metadata_reader<T>(
        &mut self,
        wavelength_spectrum_metadata_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        self.query_index
            .populate_wavelength_spectrum_metadata_indices(&wavelength_spectrum_metadata_reader);
        Ok(())
    }

    pub(crate) fn visit_chromatogram_metadata_reader<T>(
        &mut self,
        chromatogram_metadata_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        self.query_index
            .populate_chromatogram_metadata_indices(&chromatogram_metadata_reader);
        Ok(())
    }

    pub(crate) fn visit_chromatogram_data_reader<T>(
        &mut self,
        chromatogram_data_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        for kv in chromatogram_data_reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .into_iter()
            .flatten()
        {
            match kv.key.as_str() {
                CHROMATOGRAM_ARRAY_INDEX => {
                    if let Some(val) = kv.value.as_ref() {
                        let array_index: SerializedArrayIndex = serde_json::from_str(&val)?;
                        self.chromatograms.array_indices = Arc::new(array_index.into());
                    } else {
                        log::warn!("chromatogram array index was empty");
                    }
                }
                _ => {}
            }
        }
        self.query_index.populate_chromatogram_data_indices(
            &chromatogram_data_reader,
            &self.chromatograms.array_indices(),
        );
        Ok(())
    }

    pub(crate) fn visit_spectrum_peaks<T>(
        &mut self,
        spectrum_peaks_data_reader: ArrowReaderBuilder<T>,
    ) -> io::Result<()> {
        self.spectra.peak_indices = PeakMetadata::from_metadata(&spectrum_peaks_data_reader);
        Ok(())
    }
}

/// Load the various metadata, indices and reference data
pub(crate) fn load_indices_from<T: ArchiveSource>(
    handle: &mut ArchiveReader<T>,
) -> io::Result<(ReaderMetadata, QueryIndex)> {
    log::trace!("Loading indices");
    let spectrum_data_reader = handle.spectrum_data()?;

    let mut this = ParquetIndexExtractor::default();
    this.load_metadata_mapping_from_index(handle.file_index());

    log::trace!("Loading spectrum metadata indices");
    let spectrum_metadata_reader = handle.spectrum_metadata()?;
    this.load_spectrum_metadata_indices(&spectrum_metadata_reader);
    let spectrum_id_index = build_id_index::<T>(spectrum_metadata_reader, SPECTRUM)?;
    this.load_spectrum_metadata_scan_indices(&handle.spectrum_metadata_scans()?);
    this.load_spectrum_metadata_precursor_indices(&handle.spectrum_metadata_precursors()?);
    this.load_spectrum_metadata_selected_ion_indices(&handle.spectrum_metadata_selected_ions()?);

    log::trace!("Loading spectrum data indices");
    this.visit_spectrum_data_reader(spectrum_data_reader)?;

    if let Ok(chromatogram_metadata_reader) = handle.chromatograms_metadata() {
        log::trace!("Loading chromatogram metadata indices");
        this.visit_chromatogram_metadata_reader(chromatogram_metadata_reader)?;
        this.chromatograms.id_index =
            build_id_index::<T>(handle.chromatograms_metadata()?, CHROMATOGRAM)?;
    }
    if let Ok(chromatogram_metadata_reader) = handle.chromatograms_metadata_precursors() {
        this.query_index
            .populate_chromatogram_metadata_precursor_indices(&chromatogram_metadata_reader);
    }
    if let Ok(chromatogram_metadata_reader) = handle.chromatograms_metadata_selected_ions() {
        this.query_index
            .populate_chromatogram_metadata_selected_ion_indices(&chromatogram_metadata_reader);
    }
    if let Ok(chromatogram_data_reader) = handle.chromatograms_data() {
        log::trace!("Loading chromatogram indices");
        this.visit_chromatogram_data_reader(chromatogram_data_reader)?;
    }

    handle.spectrum_peaks().ok().and_then(|r| {
        this.visit_spectrum_peaks(r)
            .inspect_err(|e| {
                log::trace!("Failed to load spectrum peak indices: {e}");
            })
            .ok()
    });

    if let Some(Ok(dat)) = handle.wavelength_spectrum_data() {
        log::trace!("Loading wavelength spectrum indices");
        this.visit_wavelength_spectrum_data_reader(dat)?;
    }

    if let Some(Ok(dat)) = handle.wavelength_spectrum_metadata() {
        log::trace!("Loading wavelength spectrum metadata");
        this.visit_wavelength_spectrum_metadata_reader(dat)?;
    }

    this.spectra.id_index = spectrum_id_index;

    if let Some(Ok(dat)) = handle.wavelength_spectrum_metadata() {
        let id_index = build_id_index::<T>(dat, "wavelength_spectrum")?;
        let mut meta = this.wavelength_spectra.take().unwrap_or_default();
        meta.id_index = id_index;
        this.wavelength_spectra = Some(meta);
    }

    let bundle = ReaderMetadata::new(
        this.mz_metadata,
        this.spectra,
        this.chromatograms,
        this.wavelength_spectra,
    );

    log::trace!("Finished loading reader metadata");
    Ok((bundle, this.query_index))
}

pub(crate) trait BaseMetadataQuerySource {
    fn metadata(&self) -> &ParquetMetaData;

    fn parquet_schema(&self) -> SchemaDescPtr {
        self.metadata().file_metadata().schema_descr_ptr()
    }
}

/// Defines shared logic for constructing traversals of the spectrum metadata
/// table(s) independent of underlying reader component.
pub(crate) trait SpectrumMetadataQuerySource: BaseMetadataQuerySource {
    fn prepare_predicate_for_all(
        &self,
    ) -> ArrowPredicateFn<
        impl FnMut(
            arrow::array::RecordBatch,
        ) -> Result<arrow::array::BooleanArray, arrow::error::ArrowError>
        + 'static,
    > {
        let schema = self.parquet_schema();
        let index_col = schema.column(0);
        let predicate_mask = ProjectionMask::columns(&self.parquet_schema(), [index_col.name()]);

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let primary_index: &UInt64Array = batch.column(0).as_primitive::<UInt64Type>();

            let it = primary_index.iter().map(|val| val.is_some());
            Ok(it.map(Some).collect())
        });
        predicate
    }

    fn prepare_rows_for_all(
        &self,
        query_indices: &impl SpectrumMetadataIndexLike,
        facet: DataKind,
    ) -> RowSelection {
        match facet {
            DataKind::Metadata => query_indices.index_index().row_selection_is_not_null(),
            DataKind::Scans => query_indices
                .scan_index()
                .map(|s| s.row_selection_is_not_null())
                .unwrap_or_default(),
            DataKind::Precursors => query_indices
                .precursor_index()
                .map(|s| s.row_selection_is_not_null())
                .unwrap_or_default(),
            DataKind::SelectedIons => query_indices
                .selected_ion_index()
                .map(|s| s.row_selection_is_not_null())
                .unwrap_or_default(),
            _ => unimplemented!(
                "{facet:?} row selection is not supported by SpectrumMetadataQuerySource",
            ),
        }
    }

    fn prepare_rows_for(
        &self,
        index: u64,
        query_indices: &impl SpectrumMetadataIndexLike,
        facet: DataKind,
    ) -> RowSelection {
        match facet {
            DataKind::Metadata => query_indices.index_index().row_selection_contains(index),
            DataKind::Scans => query_indices
                .scan_index()
                .map(|s| s.row_selection_contains(index))
                .unwrap_or_default(),
            DataKind::Precursors => query_indices
                .precursor_index()
                .map(|s| s.row_selection_contains(index))
                .unwrap_or_default(),
            DataKind::SelectedIons => query_indices
                .selected_ion_index()
                .map(|s| s.row_selection_contains(index))
                .unwrap_or_default(),
            _ => unimplemented!(
                "{facet:?} row selection is not supported by SpectrumMetadataQuerySource",
            ),
        }
    }

    fn prepare_predicate_for(
        &self,
        index: u64,
    ) -> ArrowPredicateFn<
        impl FnMut(
            arrow::array::RecordBatch,
        ) -> Result<arrow::array::BooleanArray, arrow::error::ArrowError>
        + 'static,
    > {
        let schema = self.parquet_schema();
        let index_col = schema.column(0);
        let index_col_name = index_col.name();

        let predicate_mask = ProjectionMask::columns(&schema, [index_col_name]);

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let primary_index: &UInt64Array = batch.column(0).as_primitive::<UInt64Type>();

            let it = primary_index
                .iter()
                .map(|val| val.is_some_and(|val| val == index));

            return Ok(it.map(Some).collect());
        });
        predicate
    }
}

/// An IO independent driver for parsing the spectrum metadata
/// table(s) into [`SpectrumDescription`] instances
#[derive(Debug)]
pub struct SpectrumMetadataDecoder<'a, T: ReaderFacetMetadataLike + 'a> {
    pub descriptions: Vec<SpectrumDescription>,
    pub precursors: Vec<DoubleIndexed<Precursor>>,
    pub selected_ions: Vec<DoubleIndexed<SelectedIon>>,
    pub scan_events: Vec<Indexed<ScanEvent>>,
    metadata: &'a T,
    empty_map: MetadataMapping,
    ticks: u32,
}

#[allow(unused)]
fn segment_by_index_array(
    group: &StructArray,
    index_array: &UInt64Array,
    target: u64,
) -> Result<Vec<StructArray>, arrow::error::ArrowError> {
    let mask = arrow::compute::kernels::cmp::eq(index_array, &UInt64Array::new_scalar(target))?;
    let it = arrow::compute::SlicesIterator::new(&mask);

    Ok(it
        .map(|(start, end)| group.slice(start, end - start))
        .collect())
}

impl<'a, T: ReaderFacetMetadataLike + 'a> SpectrumMetadataDecoder<'a, T> {
    pub fn new(metadata: &'a T) -> Self {
        Self {
            descriptions: Vec::new(),
            precursors: Vec::new(),
            selected_ions: Vec::new(),
            scan_events: Vec::new(),
            metadata,
            empty_map: MetadataMapping::default(),
            ticks: 0,
        }
    }

    fn load_precursors_from(
        &self,
        precursor_arr: &StructArray,
        acc: &mut Vec<(u64, Option<u64>, Precursor)>,
    ) {
        let n = precursor_arr
            .column_by_name(SPECTRUM_INDEX)
            .or_else(|| precursor_arr.column_by_name(SOURCE_INDEX))
            .map(|a| a.len() - a.null_count())
            .unwrap_or_default();
        if acc.is_empty() && n > 0 {
            acc.resize(n, Default::default());
        }
        let metadata_map = self
            .metadata
            .precursor_metadata_map()
            .unwrap_or(&self.empty_map);
        if n > 0 {
            MzPrecursorVisitor::new(acc, metadata_map, 0, Vec::new()).visit(&precursor_arr);
        }
    }

    fn load_selected_ions_from(
        &self,
        si_arr: &StructArray,
        acc: &mut Vec<(u64, Option<u64>, SelectedIon)>,
    ) {
        let metacols = self
            .metadata
            .selected_ion_metadata_map()
            .unwrap_or(&self.empty_map);
        let n = si_arr
            .column_by_name(SPECTRUM_INDEX)
            .or_else(|| si_arr.column_by_name(SOURCE_INDEX))
            .map(|a| a.len() - a.null_count())
            .unwrap_or_default();
        if acc.is_empty() && n > 0 {
            acc.resize(n, Default::default());
        }
        if n > 0 {
            MzSelectedIonVisitor::new(acc, &metacols, 0, Vec::new()).visit(&si_arr);
        }
    }

    fn load_scan_events_from(
        &self,
        scan_arr: &StructArray,
        scan_accumulator: &mut Vec<(u64, ScanEvent)>,
    ) {
        let metacols = self.metadata.scan_metadata_map().unwrap_or(&self.empty_map);
        let n = scan_arr
            .column_by_name(SPECTRUM_INDEX)
            .or_else(|| scan_arr.column_by_name(SOURCE_INDEX))
            .map(|a| a.len() - a.null_count())
            .unwrap_or_default();
        if scan_accumulator.is_empty() && n > 0 {
            scan_accumulator.resize(n, Default::default());
        }
        let mut builder = MzScanVisitor::new(scan_accumulator, &metacols, 0, Vec::new());
        builder.visit(scan_arr);
    }

    pub fn decode_batch_spectrum(&mut self, batch: RecordBatch) {
        let spec_arr = StructArray::from(batch);
        let index_arr: &UInt64Array = spec_arr.column_by_name(INDEX).unwrap().as_primitive();
        let n_spec = index_arr.len() - index_arr.null_count();
        if n_spec > 0 {
            let mut local_descr = vec![SpectrumDescription::default(); n_spec];
            let mut builder = MzSpectrumVisitor::new(
                &mut local_descr,
                &self
                    .metadata
                    .primary_metadata_map()
                    .unwrap_or(&self.empty_map),
                0,
            );
            builder.visit(&spec_arr);
            if self.descriptions.is_empty() {
                self.descriptions = local_descr;
            } else {
                self.descriptions.extend(local_descr);
            }
        }
    }

    pub fn decode_batch_scan(&mut self, batch: RecordBatch) {
        let scan_arr: StructArray = batch.into();
        let mut acc = Vec::new();
        self.load_scan_events_from(&scan_arr, &mut acc);
        if self.scan_events.is_empty() {
            self.scan_events = acc;
        } else {
            self.scan_events.extend(acc);
        }
    }

    pub fn decode_batch_precursor(&mut self, batch: RecordBatch) {
        let precursor_arr = StructArray::from(batch);
        let mut precursor_acc = Vec::new();
        self.load_precursors_from(&precursor_arr, &mut precursor_acc);
        if self.precursors.is_empty() {
            self.precursors = precursor_acc
        } else {
            self.precursors.extend(precursor_acc);
        }
    }

    pub fn decode_batch_selected_ion(&mut self, batch: RecordBatch) {
        let selected_ion_arr = StructArray::from(batch);
        let mut acc = Vec::new();
        self.load_selected_ions_from(&selected_ion_arr, &mut acc);
        if self.selected_ions.is_empty() {
            self.selected_ions = acc;
        } else {
            self.selected_ions.extend(acc);
        }
    }

    #[allow(unused)]
    // This function is almost right, but something is missing during the decoding process
    pub fn decode_batch_for(&mut self, batch: RecordBatch, spectrum_index: u64) {
        self.ticks += 1;
        let empty = MetadataMapping::default();
        let spec_arr = batch.column_by_name(SPECTRUM).unwrap().as_struct();
        let index_arr: &UInt64Array = spec_arr.column_by_name(INDEX).unwrap().as_primitive();
        let spec_arrays = segment_by_index_array(spec_arr, index_arr, spectrum_index).unwrap();
        for spec_arr in spec_arrays {
            let n_spec = index_arr.len() - index_arr.null_count();
            if n_spec > 0 {
                let mut local_descr = vec![SpectrumDescription::default()];
                let mut builder = MzSpectrumVisitor::new(
                    &mut local_descr,
                    &self.metadata.primary_metadata_map().unwrap_or(&empty),
                    0,
                );
                builder.visit(&spec_arr);
                if self.descriptions.is_empty() {
                    self.descriptions = local_descr;
                } else {
                    self.descriptions.extend(local_descr);
                }
            }
        }

        if let Some(scan_arr) = batch.column_by_name(SCAN).map(|arr| arr.as_struct()) {
            let index_arr: &UInt64Array = scan_arr
                .column_by_name(SOURCE_INDEX)
                .or_else(|| scan_arr.column_by_name(SPECTRUM_INDEX))
                .unwrap()
                .as_primitive();
            for scan_arr in segment_by_index_array(scan_arr, index_arr, spectrum_index).unwrap() {
                let mut acc = Vec::new();
                self.load_scan_events_from(&scan_arr, &mut acc);
                if self.scan_events.is_empty() {
                    self.scan_events = acc;
                } else {
                    self.scan_events.extend(acc);
                }
            }
        }

        if let Some(precursor_arr) = batch.column_by_name(PRECURSOR).map(|v| v.as_struct()) {
            let index_arr: &UInt64Array = precursor_arr
                .column_by_name(SOURCE_INDEX)
                .or_else(|| precursor_arr.column_by_name(SPECTRUM_INDEX))
                .unwrap()
                .as_primitive();
            for precursor_arr in
                segment_by_index_array(precursor_arr, index_arr, spectrum_index).unwrap()
            {
                let mut precursor_acc = Vec::new();
                self.load_precursors_from(&precursor_arr, &mut precursor_acc);
                if self.precursors.is_empty() {
                    self.precursors = precursor_acc
                } else {
                    self.precursors.extend(precursor_acc);
                }
            }
        }

        if let Some(selected_ion_arr) = batch.column_by_name(SELECTED_ION).map(|v| v.as_struct()) {
            let index_arr: &UInt64Array = selected_ion_arr
                .column_by_name(SOURCE_INDEX)
                .or_else(|| selected_ion_arr.column_by_name(SPECTRUM_INDEX))
                .unwrap()
                .as_primitive();
            for selected_ion_arr in
                segment_by_index_array(selected_ion_arr, &index_arr, spectrum_index).unwrap()
            {
                let mut acc = Vec::new();
                self.load_selected_ions_from(&selected_ion_arr, &mut acc);
                if self.selected_ions.is_empty() {
                    self.selected_ions = acc;
                } else {
                    self.selected_ions.extend(acc);
                }
            }
        }
    }

    #[allow(unused)]
    /// Visit a [`RecordBatch`], splitting it into separate streams passed
    /// through distinct visitors for *any* spectra.
    pub fn decode_batch(&mut self, batch: RecordBatch, facet: DataKind) {
        self.ticks += 1;
        match facet {
            DataKind::Metadata => self.decode_batch_spectrum(batch),
            DataKind::Scans => self.decode_batch_scan(batch),
            DataKind::Precursors => self.decode_batch_precursor(batch),
            DataKind::SelectedIons => self.decode_batch_selected_ion(batch),
            _ => unimplemented!("{facet:?} is not supported")
        }
    }

    /// Consume the decoder to produce the final construction
    pub fn finish(mut self) -> Vec<SpectrumDescription> {
        // There should be a more efficient method for this, but it would require
        // more work and assuming that things are sorted
        let index_map: HashMap<u64, usize, BuildIdentityHasher<u64>> = self
            .descriptions
            .iter()
            .enumerate()
            .map(|(i, desc)| (desc.index as u64, i))
            .collect();

        self.precursors =
            PrecursorSelectedIonAssembler::new(self.precursors, self.selected_ions).build();

        for (idx, scan) in self.scan_events {
            if let Some(i) = index_map.get(&idx).copied() {
                if let Some(spec) = self.descriptions.get_mut(i) {
                    spec.acquisition.scans.push(scan);
                }
            }
        }

        // Reversed traversal to guarantee that the lowest order precursor is *last*
        for (idx, _, precursor) in self
            .precursors
            .into_iter()
            .rev()
            .map(CompoundIndexVisitor::unpack)
        {
            if let Some(i) = index_map.get(&idx).copied() {
                if let Some(spec) = self.descriptions.get_mut(i) {
                    spec.precursor.push(precursor);
                }
            }
        }
        log::debug!(
            "Finished decoding {} spectrum descriptions after {} batches",
            self.descriptions.len(),
            self.ticks
        );
        self.descriptions
    }
}

/// Encapsulate the procedure for reconstructing the precursor->selected ion hierarchy
/// into a shared component. An implementation detail of [`SpectrumMetadataReader`] and
/// [`ChromatogramMetadataReader`].
struct PrecursorSelectedIonAssembler {
    pub precursors: Vec<DoubleIndexed<Precursor>>,
    pub selected_ions: Vec<DoubleIndexed<SelectedIon>>,
    last_precursor_i: usize,
    spec_idx_match: Option<usize>,
}

impl PrecursorSelectedIonAssembler {
    pub fn new(
        precursors: Vec<DoubleIndexed<Precursor>>,
        selected_ions: Vec<DoubleIndexed<SelectedIon>>,
    ) -> Self {
        Self {
            precursors,
            selected_ions,
            last_precursor_i: 0,
            spec_idx_match: None,
        }
    }

    fn sort_precursors(&mut self) {
        self.precursors.sort_unstable_by(|a, b| {
            a.source_index()
                .cmp(&b.source_index())
                .then(a.secondary_index().cmp(&b.secondary_index()))
        });
    }

    pub fn build(mut self) -> Vec<DoubleIndexed<Precursor>> {
        self.sort_precursors();

        self.last_precursor_i = 0;
        let n = self.precursors.len();
        for (spec_idx, prec_idx, si) in self.selected_ions {
            let mut si = Some(si);
            let mut hit = false;
            self.spec_idx_match = None;
            for precursor_i in self.last_precursor_i..n {
                if let Some((precursor_rec_spec_i, precursor_rec_prec_i, prec)) =
                    self.precursors.get_mut(precursor_i)
                {
                    if *precursor_rec_spec_i == spec_idx {
                        self.spec_idx_match = Some(precursor_i);
                    }
                    if (*precursor_rec_spec_i) == spec_idx && (*precursor_rec_prec_i) == prec_idx {
                        self.last_precursor_i = precursor_i;
                        prec.add_ion(si.take().unwrap());
                        hit = true;
                        break;
                    } else if *precursor_rec_spec_i > spec_idx {
                        if !hit {
                            log::debug!(
                                "Fallback assignment of selected ion {spec_idx}:{prec_idx:?}:{si:?}"
                            );
                            if let Some(spec_idx_match) = self.spec_idx_match {
                                if let Some((_, _, prec)) = self.precursors.get_mut(spec_idx_match)
                                {
                                    prec.add_ion(si.take().unwrap());
                                    self.last_precursor_i = spec_idx_match;
                                }
                            }
                            hit = true;
                        }
                        break;
                    }
                }
            }
            if !hit && si.is_some() {
                if let Some(spec_idx_match) = self.spec_idx_match {
                    log::debug!(
                        "Fallback assignment of selected ion {spec_idx}:{prec_idx:?}:{si:?}"
                    );
                    if let Some((_, _, prec)) = self.precursors.get_mut(spec_idx_match) {
                        prec.add_ion(si.take().unwrap());
                        self.last_precursor_i = spec_idx_match;
                    }
                } else {
                    log::debug!(
                        "Did not find an owner for selected ion {spec_idx}:{prec_idx:?}:{si:?}"
                    )
                }
            }
        }
        self.precursors
    }
}

pub(crate) struct SpectrumMetadataReader<T: ChunkReader + 'static>(
    pub(crate) ParquetRecordBatchReaderBuilder<T>,
);

impl<T: ChunkReader + 'static> BaseMetadataQuerySource for SpectrumMetadataReader<T> {
    fn metadata(&self) -> &ParquetMetaData {
        self.0.metadata()
    }
}

impl<T: ChunkReader + 'static> SpectrumMetadataQuerySource for SpectrumMetadataReader<T> {}

/// Defines shared logic for constructing traversals of the chromatogram metadata
/// table(s) independent of underlying reader component.
pub(crate) trait ChromatogramMetadataQuerySource: BaseMetadataQuerySource {
    fn prepare_predicate_for_all(
        &self,
    ) -> ArrowPredicateFn<
        impl FnMut(RecordBatch) -> Result<arrow::array::BooleanArray, arrow::error::ArrowError>
        + 'static,
    > {
        let schema = self.parquet_schema();
        let index_col = schema.column(0);
        let predicate_mask = ProjectionMask::columns(&self.parquet_schema(), [index_col.name()]);

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let primary_index: &UInt64Array = batch.column(0).as_primitive::<UInt64Type>();

            let it = primary_index.iter().map(|val| val.is_some());
            Ok(it.map(Some).collect())
        });
        predicate
    }
}

pub struct ChromatogramMetadataDecoder<'a> {
    pub descriptions: Vec<ChromatogramDescription>,
    pub precursors: Vec<DoubleIndexed<Precursor>>,
    pub selected_ions: Vec<DoubleIndexed<SelectedIon>>,
    metadata: &'a ReaderMetadata,
}

impl<'a> ChromatogramMetadataDecoder<'a> {
    pub fn new(metadata: &'a ReaderMetadata) -> Self {
        Self {
            descriptions: Vec::new(),
            precursors: Vec::new(),
            selected_ions: Vec::new(),
            metadata,
        }
    }

    fn load_precursors_from(
        &self,
        precursor_arr: &StructArray,
        acc: &mut Vec<(u64, Option<u64>, Precursor)>,
    ) {
        let n = precursor_arr
            .column_by_name(SOURCE_INDEX)
            .or_else(|| precursor_arr.column_by_name(SPECTRUM_INDEX))
            .map(|a| a.len() - a.null_count())
            .unwrap_or_default();
        if acc.is_empty() && n > 0 {
            acc.resize(n, Default::default());
        }
        let empty = MetadataMapping::default();
        if n > 0 {
            MzPrecursorVisitor::new(acc, &empty, 0, Vec::new()).visit(&precursor_arr);
        }
    }

    fn load_selected_ions_from(
        &self,
        si_arr: &StructArray,
        acc: &mut Vec<(u64, Option<u64>, SelectedIon)>,
    ) {
        let empty = MetadataMapping::default();
        let metacols = self
            .metadata
            .spectra
            .selected_ion_metadata_map
            .as_ref()
            .unwrap_or(&empty);
        let n = si_arr
            .column_by_name(SOURCE_INDEX)
            .or_else(|| si_arr.column_by_name(SPECTRUM_INDEX))
            .map(|a| a.len() - a.null_count())
            .unwrap_or_default();
        if acc.is_empty() && n > 0 {
            acc.resize(n, Default::default());
        }

        if n > 0 {
            MzSelectedIonVisitor::new(acc, &metacols, 0, Vec::new()).visit(&si_arr);
        }
    }

    pub fn decode_batch_chromatogram(&mut self, batch: RecordBatch) {
        let empty = MetadataMapping::default();
        let chrom_arr = StructArray::from(batch);
        let index_arr: &UInt64Array = chrom_arr
            .column_by_name(INDEX)
            .unwrap()
            .as_any()
            .downcast_ref()
            .unwrap();
        let n_spec = index_arr.len() - index_arr.null_count();
        let mut local_descr = vec![ChromatogramDescription::default(); n_spec];
        let mut builder = MzChromatogramBuilder::new(
            &mut local_descr,
            &self
                .metadata
                .chromatograms
                .primary_metadata_map()
                .unwrap_or(&empty),
            0,
        );
        builder.visit(&chrom_arr);
        self.descriptions.extend(local_descr);
    }

    pub fn decode_batch_precursor(&mut self, batch: RecordBatch) {
        let precursor_arr = StructArray::from(batch);
        {
            let mut acc = Vec::new();
            self.load_precursors_from(&precursor_arr, &mut acc);
            self.precursors.extend(acc);
        }
    }

    pub fn decode_batch_selected_ion(&mut self, batch: RecordBatch) {
        let selected_ion_arr = StructArray::from(batch);
        let mut acc = Vec::new();
        self.load_selected_ions_from(&selected_ion_arr, &mut acc);
        self.selected_ions.extend(acc);
    }

    #[allow(unused)]
    pub fn decode_batch(&mut self, batch: RecordBatch) {
        let empty = MetadataMapping::default();
        let chrom_arr = batch.column_by_name(CHROMATOGRAM).unwrap().as_struct();
        let index_arr: &UInt64Array = chrom_arr
            .column_by_name(INDEX)
            .unwrap()
            .as_any()
            .downcast_ref()
            .unwrap();
        let n_spec = index_arr.len() - index_arr.null_count();
        let mut local_descr = vec![ChromatogramDescription::default(); n_spec];
        let mut builder = MzChromatogramBuilder::new(
            &mut local_descr,
            &self
                .metadata
                .chromatograms
                .primary_metadata_map()
                .unwrap_or(&empty),
            0,
        );
        builder.visit(chrom_arr);
        self.descriptions.extend(local_descr);

        let precursor_arr = batch.column_by_name(PRECURSOR).unwrap().as_struct();
        {
            let mut acc = Vec::new();
            self.load_precursors_from(precursor_arr, &mut acc);
            self.precursors.extend(acc);
        }

        let selected_ion_arr = batch.column_by_name(SELECTED_ION).unwrap().as_struct();
        {
            let mut acc = Vec::new();
            self.load_selected_ions_from(selected_ion_arr, &mut acc);
            self.selected_ions.extend(acc);
        }
    }

    pub fn finish(mut self) -> Vec<ChromatogramDescription> {
        let index_map: HashMap<u64, usize, BuildIdentityHasher<u64>> = self
            .descriptions
            .iter()
            .enumerate()
            .map(|(i, desc)| (desc.index as u64, i))
            .collect();

        // This sorts the precursor list in addition to merging in the selected ions
        self.precursors =
            PrecursorSelectedIonAssembler::new(self.precursors, self.selected_ions).build();

        // Reversed traversal to guarantee that the lowest order precursor is *last*
        for (idx, _prec_idx, precursor) in self.precursors.into_iter().rev() {
            if let Some(i) = index_map.get(&idx).copied() {
                self.descriptions[i].precursor.push(precursor);
            }
        }
        self.descriptions
    }
}

pub(crate) struct ChromatogramMetadataReader<T: ChunkReader + 'static>(
    pub(crate) ParquetRecordBatchReaderBuilder<T>,
);

impl<T: ChunkReader + 'static> ChromatogramMetadataQuerySource for ChromatogramMetadataReader<T> {}

impl<T: ChunkReader + 'static> BaseMetadataQuerySource for ChromatogramMetadataReader<T> {
    fn metadata(&self) -> &ParquetMetaData {
        self.0.metadata()
    }
}

#[derive(Debug)]
pub struct TimeIndexDecoder {
    times: HashMap<u64, f64, BuildIdentityHasher<u64>>,
    time_range: SimpleInterval<f64>,
    min: u64,
    max: u64,
    ms_level_range: Option<SimpleInterval<u8>>,
    indices: HashSet<u64, BuildIdentityHasher<u64>>,
}

impl TimeIndexDecoder {
    pub fn new(
        time_range: SimpleInterval<f64>,
        ms_level_range: Option<SimpleInterval<u8>>,
    ) -> Self {
        Self {
            time_range,
            min: u64::MAX,
            max: 0,
            times: Default::default(),
            ms_level_range,
            indices: Default::default(),
        }
    }

    pub fn from_descriptions(&mut self, descriptions: &[SpectrumDescription]) {
        let n = descriptions.len();
        let offset_start = match descriptions.binary_search_by(|descr| {
            self.time_range
                .start()
                .total_cmp(&(descr.acquisition.start_time() as f64))
                .reverse()
        }) {
            Ok(i) => i,
            Err(i) => i.min(n),
        }
        .saturating_sub(1);
        let offset = offset_start;
        if let Some(ms_level_range) = self.ms_level_range {
            for (i, descr) in descriptions
                .iter()
                .enumerate()
                .skip(offset)
                .filter(|(_, v)| ms_level_range.contains(&v.ms_level))
            {
                let i = i as u64;
                let t = descr.acquisition.start_time();
                if self.time_range.contains(&t) {
                    self.min = self.min.min(i);
                    self.max = self.max.max(i);
                    self.times.insert(i, t);
                    self.indices.insert(i);
                } else if !self.times.is_empty() {
                    break;
                }
            }
        } else {
            for (i, descr) in descriptions.iter().enumerate().skip(offset) {
                let i = i as u64;
                let t = descr.acquisition.start_time();
                if self.time_range.contains(&t) {
                    self.min = self.min.min(i);
                    self.max = self.max.max(i);
                    self.times.insert(i, t);
                } else if !self.times.is_empty() {
                    break;
                }
            }
        }
    }

    pub fn decode_batch(
        &mut self,
        batch: RecordBatch,
    ) -> Result<(), parquet::errors::ParquetError> {
        let root = batch;
        let arr: &UInt64Array = root.column(0).as_primitive();
        let time_arr = root.column(1);

        macro_rules! add {
            ($val:ident, $time:expr) => {
                // Re-check the time interval constraint for consistency, but the predicate should have
                // dealt with this
                if self.time_range.contains(&$time) {
                    self.min = self.min.min($val);
                    self.max = self.max.max($val);
                    self.times.insert($val, $time);
                    // We assume that if we are building a sparse index set, then the batches have been pre-filtered
                    // exactly for the ms level range constraint.
                    if self.ms_level_range.is_some() {
                        self.indices.insert($val);
                    }
                }
            };
        }

        macro_rules! traverse {
            ($($dtype:ty)+) => {
                $(
                    if let Some(time_arr) = time_arr.as_primitive_opt::<$dtype>() {
                        for (val, time) in arr.iter().flatten().zip(time_arr.iter().flatten()) {
                            add!(val, time as f64);
                        }
                        return Ok(())
                    }
                )+
            };
        }

        traverse!(
            Float32Type
            Float64Type
        );
        Err(parquet::errors::ParquetError::ArrowError(format!(
            "Invalid time array data type: {:?}",
            time_arr.data_type()
        ))
        .into())
    }

    pub fn finish(self) -> (HashMap<u64, f64, BuildIdentityHasher<u64>>, MaskSet) {
        let range = SimpleInterval::new(self.min, self.max);
        if self.ms_level_range.is_some() {
            log::debug!("Building mask set with {:?}", self.indices);
            (self.times, MaskSet::new(range, Some(self.indices)))
        } else {
            (self.times, MaskSet::new(range, None))
        }
    }
}

#[derive(Debug)]
pub struct SelectedIonIndexDecoder {
    mz_range: SimpleInterval<f64>,
    indices: HashSet<u64, BuildIdentityHasher<u64>>,
}

#[allow(unused)]
impl SelectedIonIndexDecoder {
    pub fn new(mz_range: SimpleInterval<f64>) -> Self {
        Self {
            mz_range,
            indices: Default::default(),
        }
    }

    pub fn decode_batch(
        &mut self,
        batch: &RecordBatch,
    ) -> Result<(), parquet::errors::ParquetError> {
        let root = batch;
        let arr: &UInt64Array = root.column(0).as_primitive();
        let selected_mz_arr = root.column(1);

        macro_rules! add {
            ($val:ident, $mz:ident) => {
                // Re-check the m/z interval constraint for consistency, but the predicate should have
                // dealt with this
                if self.mz_range.contains(&$mz) {
                    self.indices.insert($val);
                }
            };
        }

        if let Some(selected_mz_arr) = selected_mz_arr.as_primitive_opt::<Float64Type>() {
            for (val, mz) in arr.iter().flatten().zip(selected_mz_arr.iter().flatten()) {
                add!(val, mz);
            }
        } else if let Some(selected_mz_arr) = selected_mz_arr.as_primitive_opt::<Float32Type>() {
            for (val, mz) in arr
                .iter()
                .flatten()
                .zip(selected_mz_arr.iter().flatten().map(|v| v as f64))
            {
                add!(val, mz);
            }
        } else {
            return Err(parquet::errors::ParquetError::ArrowError(format!(
                "Invalid selected ion m/z array data type: {:?}",
                selected_mz_arr.data_type()
            ))
            .into());
        }
        Ok(())
    }

    pub fn from_descriptions(&mut self, descriptions: &[SpectrumDescription]) {
        for descr in descriptions {
            if let Some(ion) = descr.precursor.first().and_then(|p| p.ion()) {
                if self.mz_range.contains(&ion.mz) {
                    self.indices.insert(descr.index as u64);
                }
            }
        }
    }

    pub fn finish(self) -> MaskSet {
        match self.indices.iter().minmax() {
            itertools::MinMaxResult::NoElements => MaskSet::empty(),
            itertools::MinMaxResult::OneElement(val) => {
                MaskSet::new(SimpleInterval::new(*val, *val), Some(self.indices))
            }
            itertools::MinMaxResult::MinMax(min, max) => {
                MaskSet::new(SimpleInterval::new(*min, *max), Some(self.indices))
            }
        }
    }
}

pub struct AuxiliaryArrayCountDecoder {
    context: BufferContext,
    counts: Vec<u32>,
}

impl AuxiliaryArrayCountDecoder {
    pub fn new(context: BufferContext) -> Self {
        Self {
            context,
            counts: Vec::new(),
        }
    }

    pub fn build_projection<T>(&self, builder: &ArrowReaderBuilder<T>) -> Option<ProjectionMask> {
        let schema = builder.parquet_schema();
        let mut index_i = None;
        let mut auxiliary_count_i = None;
        for (i, c) in schema.columns().iter().enumerate() {
            let parts = c.path().parts();
            if parts == [INDEX] {
                index_i = Some(i);
            } else if parts
                .iter()
                .zip(["number_of_auxiliary_arrays"])
                .all(|(a, b)| a == b)
            {
                auxiliary_count_i = Some(i);
            }
        }

        let proj = match (index_i, auxiliary_count_i) {
            (Some(i), Some(j)) => ProjectionMask::leaves(schema, [i, j]),
            _ => {
                return {
                    log::warn!(
                        "No 'number_of_auxiliary_arrays' column found for {}",
                        self.context.name()
                    );
                    None
                };
            }
        };
        Some(proj)
    }

    pub fn resize(&mut self, n: usize) {
        self.counts.resize(n, 0);
    }

    pub fn decode_batch(&mut self, batch: &RecordBatch) {
        macro_rules! unpack {
            ($index_array:ident, $values_array:ident, $dtype:ty) => {
                if let Some(values) = $values_array.as_primitive_opt::<$dtype>() {
                    for (i, c) in $index_array.iter().zip(values.iter()) {
                        let i = if let Some(i) = i {
                            i as usize
                        } else {
                            continue;
                        };
                        if i >= self.counts.len() {
                            panic!(
                                "Cannot fit {} rows into {} bins",
                                batch.num_rows(),
                                self.counts.len()
                            );
                        }
                        self.counts[i] = c.unwrap_or_default() as u32;
                    }
                    true
                } else {
                    false
                }
            };
        }

        let index_array: &UInt64Array = batch.column(0).as_primitive();
        let values_array = batch.column(1);

        if unpack!(index_array, values_array, UInt32Type) {
        } else if unpack!(index_array, values_array, UInt64Type) {
        } else if unpack!(index_array, values_array, Int32Type) {
        } else if unpack!(index_array, values_array, Int64Type) {
        } else {
            unimplemented!(
                "auxiliary array count stored as {:?}",
                values_array.data_type()
            )
        }
    }

    pub fn finish(self) -> Vec<u32> {
        self.counts
    }
}

#[derive(Debug)]
pub struct PeakInfoDecoder<'a> {
    pub model_parameters: Vec<Option<Vec<f64>>>,
    pub data_point_counts: Vec<u64>,
    pub peak_counts: Vec<u64>,
    pub has_data_point_counts: bool,
    pub has_peaks: bool,
    pub has_models: bool,

    data_point_column_name: String,
    peak_column_name: String,
    model_column_name: String,

    metadata_mapping: &'a MetadataMapping
}

impl<'a> PeakInfoDecoder<'a> {
    pub fn new(metadata_mapping: &'a MetadataMapping) -> Self {
        Self {
            model_parameters: Default::default(),
            data_point_counts: Default::default(),
            peak_counts: Default::default(),
            has_data_point_counts: false,
            has_peaks: false,
            has_models: false,
            data_point_column_name: String::new(),
            peak_column_name: String::new(),
            model_column_name: String::new(),
            metadata_mapping
        }
    }

    pub fn resize(&mut self, n: usize) {
        if self.has_models {
            self.model_parameters.resize(n, None);
        }
        if self.has_data_point_counts {
            self.data_point_counts.resize(n, 0);
        }
        if self.has_peaks {
            self.peak_counts.resize(n, 0);
        }
    }

    pub fn build_projection<T>(
        &mut self,
        builder: &ArrowReaderBuilder<T>,
    ) -> Option<ProjectionMask> {
        let schema = builder.parquet_schema();
        let mut index_i = None;
        let mut median_i = None;
        let mut dp_i = None;
        let mut peaks_i = None;

        let number_of_peaks_col = self.metadata_mapping.find(curie!(MS:1003059));
        let number_of_dp_col = self.metadata_mapping.find(curie!(MS:1003060));
        let spacing_model_col = self.metadata_mapping.find(curie!(MS:1003820));

        for (i, c) in schema.columns().iter().enumerate() {
            let parts = c.path().parts();
            if parts == [INDEX] {
                index_i = Some(i);
            }
            if parts
                .iter()
                .zip(["median_delta"])
                .all(|(a, b)| a == b)
                || parts
                    .iter()
                    .zip(["mz_delta_model"])
                    .all(|(a, b)| a == b)
                || spacing_model_col.is_some_and(|c| c.path == parts)
            {
                median_i = Some(i);
                self.has_models = true;
                self.model_column_name = parts[0].to_string();
            }

            if number_of_dp_col.is_some_and(|c| c.path == parts) {
                dp_i = Some(i);
                self.has_data_point_counts = true;
                self.data_point_column_name = c.name().to_string();
            }
            if number_of_peaks_col.is_some_and(|c| c.path == parts) {
                peaks_i = Some(i);
                self.has_peaks = true;
                self.peak_column_name = c.name().to_string();
            }
        }
        if let Some(i) = index_i {
            let mut indices = Vec::with_capacity(4);
            indices.push(i);
            indices.extend(median_i);
            indices.extend(dp_i);
            indices.extend(peaks_i);
            if indices.len() == 1 {
                None
            } else {
                Some(ProjectionMask::leaves(schema, indices))
            }
        } else {
            None
        }
    }

    pub fn decode_batch(&mut self, batch: &RecordBatch) {
        let root = batch;
        let index_array: &UInt64Array = root.column(0).as_primitive();

        if self.has_models {
            if let Some(col) = root
                .column_by_name(&self.model_column_name)
            {
                macro_rules! process_list {
                    ($val_array:expr) => {
                        match $val_array.value_type() {
                            DataType::Float32 => {
                                for (i, val) in index_array.iter().zip($val_array.iter()) {
                                    if let Some(i) = i {
                                        self.model_parameters[i as usize] = val
                                            .map(|v| -> Vec<f64> {
                                                v.as_primitive::<Float32Type>()
                                                    .iter()
                                                    .map(|i| i.unwrap() as f64)
                                                    .collect()
                                            })
                                            .filter(|v| !v.is_empty());
                                    }
                                }
                            }
                            DataType::Float64 => {
                                for (i, val) in index_array.iter().zip($val_array.iter()) {
                                    if let Some(i) = i {
                                        self.model_parameters[i as usize] = val
                                            .map(|v| -> Vec<f64> {
                                                let val = v.as_primitive::<Float64Type>();
                                                val.values().to_vec()
                                            })
                                            .filter(|v| !v.is_empty());
                                    }
                                }
                            }
                            _ => {}
                        }
                    };
                }

                if let Some(val_array) = col.as_list_opt::<i64>() {
                    process_list!(val_array);
                } else if let Some(val_array) = col.as_list_opt::<i32>() {
                    process_list!(val_array);
                } else if let Some(val_array) = col.as_primitive_opt::<Float32Type>() {
                    for (i, val) in index_array.iter().zip(val_array) {
                        if let Some(i) = i {
                            self.model_parameters[i as usize] =
                                val.map(|v| vec![v as f64]).filter(|v| !v.is_empty());
                        }
                    }
                } else if let Some(val_array) = col.as_primitive_opt::<Float64Type>() {
                    for (i, val) in index_array.iter().zip(val_array) {
                        if let Some(i) = i {
                            self.model_parameters[i as usize] =
                                val.map(|v| vec![v]).filter(|v| !v.is_empty());
                        }
                    }
                }
            }
        }

        if self.has_data_point_counts {
            let col = root.column_by_name(&self.data_point_column_name).unwrap();
            macro_rules! extract {
                ($dtype:ty) => {
                    if let Some(col) = col.as_primitive_opt::<$dtype>() {
                        for (val, i) in col.iter().zip(index_array.iter()) {
                            if let Some(i) = i {
                                self.data_point_counts[i as usize] =
                                    (val.unwrap_or_default() as u64);
                            }
                        }
                        true
                    } else {
                        false
                    }
                };
            }

            if extract!(UInt64Type) {
            } else if extract!(UInt32Type) {
            } else if extract!(Int64Type) {
            } else if extract!(Int32Type) {
            }
        }

        if self.has_peaks {
            let col = root.column_by_name(&self.peak_column_name).unwrap();
            macro_rules! extract {
                ($dtype:ty) => {
                    if let Some(col) = col.as_primitive_opt::<$dtype>() {
                        for (val, i) in col.iter().zip(index_array.iter()) {
                            if let Some(i) = i {
                                self.peak_counts[i as usize] = (val.unwrap_or_default() as u64);
                            }
                        }
                        true
                    } else {
                        false
                    }
                };
            }

            if extract!(UInt64Type) {
            } else if extract!(UInt32Type) {
            } else if extract!(Int64Type) {
            } else if extract!(Int32Type) {
            }
        }
    }
}

pub struct TimeEncodedSeriesDecoder {
    time_array: Vec<u8>,
    measure_array: Vec<u8>,
    time_index: usize,
    measure_index: usize,
    dtype: DataType,
}

impl TimeEncodedSeriesDecoder {
    pub fn new(time_index: usize, measure_index: usize) -> Self {
        Self {
            time_array: Vec::new(),
            measure_array: Vec::new(),
            time_index,
            measure_index,
            dtype: DataType::Null,
        }
    }

    pub fn decode_batch(&mut self, batch: RecordBatch) {
        let time_array = batch.column(self.time_index);
        if let Some(arr) = time_array.as_primitive_opt::<Float32Type>() {
            for val in arr {
                if let Some(val) = val {
                    self.time_array
                        .extend_from_slice(&(val as f64).to_le_bytes());
                }
            }
        } else if let Some(arr) = time_array.as_primitive_opt::<Float64Type>() {
            for val in arr {
                if let Some(val) = val {
                    self.time_array.extend_from_slice(&val.to_le_bytes());
                }
            }
        }

        let measure_array = batch.column(self.measure_index);
        self.dtype = measure_array.data_type().clone();

        macro_rules! consume {
            ($arr:ident) => {
                for (i, val) in $arr.into_iter().enumerate() {
                    if time_array.is_null(i) {
                        continue;
                    }
                    if let Some(val) = val {
                        self.measure_array.extend_from_slice(&val.to_le_bytes());
                    }
                }
            };
        }

        macro_rules! consume_measurement {
            ($($dtype:ty)+) => {
                $(
                    if let Some(arr) = measure_array.as_primitive_opt::<$dtype>() {
                        consume!(arr);
                        return
                    }
                )+
            };
        }

        consume_measurement!(
            Float32Type
            Float64Type
            Int32Type
            Int64Type
            UInt32Type
            UInt64Type
        );

        unimplemented!("Cannot decode {:?}", self.dtype);
    }

    pub fn finish(self, output_array_type: &ArrayType) -> (DataArray, DataArray) {
        let time_array = DataArray::wrap(
            &ArrayType::TimeArray,
            BinaryDataArrayType::Float64,
            self.time_array,
        );
        let dtype = arrow_to_array_type(&self.dtype).unwrap();
        let measure_array = DataArray::wrap(output_array_type, dtype, self.measure_array);
        (time_array, measure_array)
    }
}
