use std::{collections::HashMap, io, marker::PhantomData, sync::Arc};

use arrow::{
    array::{Array, ArrayRef, AsArray, RecordBatch, UInt64Array},
    datatypes::{DataType, Float32Type, Float64Type},
    error::ArrowError,
};
use futures::{Stream, StreamExt, stream::BoxStream};
use identity_hash::BuildIdentityHasher;
use object_store::{ObjectStore, path::Path as ObjectPath};

use mzdata::{
    curie,
    io::{
        AsyncRandomAccessSpectrumIterator, AsyncSpectrumSource, DetailLevel, OffsetIndex,
        SpectrumStream,
    },
    meta::MSDataFileMetadata,
    params::Unit,
    prelude::*,
    spectrum::{
        ArrayType, BinaryArrayMap, Chromatogram, ChromatogramDescription, ChromatogramType,
        DataArray, MultiLayerSpectrum, PeakDataLevel, SpectrumDescription,
        bindata::BuildFromArrayMap,
    },
};

use mzpeaks::{
    CentroidPeak, DeconvolutedCentroidLike, DeconvolutedPeak, coordinate::SimpleInterval,
    prelude::Span1D,
};

use parquet::{
    arrow::{
        ParquetRecordBatchStreamBuilder, ProjectionMask,
        arrow_reader::{ArrowPredicateFn, RowFilter},
        async_reader::{AsyncFileReader, ParquetRecordBatchStream},
    },
    file::metadata::ParquetMetaData,
};
use url::Url;

use crate::{
    BufferContext, CURIE, archive::{
        AsyncArchiveReader, AsyncArchiveSource, AsyncZipArchiveSource, DataKind, EntityType, FileEntry,
    }, constants::{CHROMATOGRAM, SPECTRUM}, reader::{
        ReaderMetadata, SignalLoadingPreference, cache::{CacheBuffer, DataCacheBlock}, chunk::AsyncChunkReader, index::{self, BasicQueryIndex, ChromatogramQueryIndex, PageQuery, QueryIndex, SpanDynNumeric}, metadata::{
            AuxiliaryArrayCountDecoder, BaseMetadataQuerySource, ChromatogramMetadataDecoder,
            ChromatogramMetadataQuerySource, ParquetIndexExtractor, PeakInfoDecoder,
            ReaderFacetMetadataLike, SpectrumMetadataDecoder, SpectrumMetadataQuerySource,
            TimeEncodedSeriesDecoder, TimeIndexDecoder,
        }, point::{AsyncPointDataReader, PointDataArrayReader}, utils::{IntoQueryRange, MaskSet}, visitor::AuxiliaryArrayVisitor,
    },
};

pub(crate) struct SpectrumMetadataReader<T: AsyncFileReader + 'static + Unpin + Send>(
    pub(crate) ParquetRecordBatchStreamBuilder<T>,
);

impl<T: AsyncFileReader + 'static + Unpin + Send> BaseMetadataQuerySource
    for SpectrumMetadataReader<T>
{
    fn metadata(&self) -> &ParquetMetaData {
        self.0.metadata()
    }
}

impl<T: AsyncFileReader + 'static + Unpin + Send> SpectrumMetadataQuerySource
    for SpectrumMetadataReader<T>
{
}

pub(crate) struct ChromatogramMetadataReader<T: AsyncFileReader + 'static + Unpin + Send>(
    pub(crate) ParquetRecordBatchStreamBuilder<T>,
);

impl<T: AsyncFileReader + 'static + Unpin + Send> ChromatogramMetadataQuerySource
    for ChromatogramMetadataReader<T>
{
}

impl<T: AsyncFileReader + 'static + Unpin + Send> BaseMetadataQuerySource
    for ChromatogramMetadataReader<T>
{
    fn metadata(&self) -> &ParquetMetaData {
        self.0.metadata()
    }
}

pub(crate) async fn build_id_index<T: AsyncArchiveSource>(
    handle: ParquetRecordBatchStreamBuilder<T::File>,
    prefix: &str,
) -> io::Result<OffsetIndex> {
    let mut spectrum_id_index = OffsetIndex::new(prefix.into());
    let pq_schema = handle.parquet_schema();
    let mask = ProjectionMask::columns(
        pq_schema,
        [format!("id").as_str(), format!("index").as_str()],
    );
    let mut stream = handle.with_projection(mask).build()?;

    while let Some(batch) = stream.next().await.transpose()? {
        let root = batch;
        let indices: &UInt64Array = root
            .column_by_name("index")
            .unwrap()
            .as_any()
            .downcast_ref()
            .unwrap();
        let ids = root.column_by_name("id").unwrap();
        macro_rules! read_ids {
            ($ids:expr) => {
                for (id, idx) in $ids.iter().zip(indices.iter()) {
                    if let Some(id) = id {
                        spectrum_id_index.insert(id, idx.unwrap());
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
    spectrum_id_index.init = true;
    Ok(spectrum_id_index)
}

/// Load the various metadata, indices and reference data
pub(crate) async fn load_indices_from<T: AsyncArchiveSource>(
    handle: &AsyncArchiveReader<T>,
) -> io::Result<(ReaderMetadata, QueryIndex)> {
    let spectrum_data_reader = handle.spectra_data().await?;

    let spectrum_id_index =
        build_id_index::<T>(handle.spectrum_metadata().await?, SPECTRUM).await?;

    let mut this = ParquetIndexExtractor::default();

    this.load_metadata_mapping_from_index(&handle.file_index());
    if let Ok(reader) = handle.spectrum_metadata().await {
        this.query_index.populate_spectrum_metadata_indices(&reader);
    }
    if let Ok(reader) = handle.spectrum_metadata_scans().await {
        this.query_index.populate_spectrum_scan_indices(&reader);
    }
    if let Ok(reader) = handle.spectrum_metadata_precursors().await {
        this.query_index
            .populate_spectrum_precursor_indices(&reader);
    }
    if let Ok(reader) = handle.spectrum_metadata_selected_ions().await {
        this.query_index
            .populate_spectrum_selected_ion_indices(&reader);
    }

    this.visit_spectrum_data_reader(spectrum_data_reader)?;

    if let Ok(reader) = handle.chromatograms_metadata().await {
        this.query_index
            .populate_chromatogram_metadata_indices(&reader);
        this.chromatograms.id_index = build_id_index::<T>(reader, CHROMATOGRAM).await?;
    }

    if let Ok(reader) = handle.chromatograms_metadata_precursors().await {
        this.query_index
            .populate_chromatogram_metadata_precursor_indices(&reader);
    }

    if let Ok(reader) = handle.chromatograms_metadata_selected_ions().await {
        this.query_index
            .populate_chromatogram_metadata_selected_ion_indices(&reader);
    }

    if let Ok(chromatogram_data_reader) = handle.chromatograms_data().await {
        this.visit_chromatogram_data_reader(chromatogram_data_reader)?;
    }

    handle
        .spectrum_peaks()
        .await
        .ok()
        .and_then(|r| this.visit_spectrum_peaks(r).ok());

    if let Some(Ok(dat)) = handle.wavelength_spectrum_data().await {
        log::trace!("Loading wavelength spectrum indices");
        this.visit_wavelength_spectrum_data_reader(dat)?;
    }

    if let Some(Ok(dat)) = handle.wavelength_spectrum_metadata().await {
        log::trace!("Loading wavelength spectrum metadata");
        this.visit_wavelength_spectrum_metadata_reader(dat)?;
    }

    this.spectra.id_index = spectrum_id_index;

    if let Some(Ok(dat)) = handle.wavelength_spectrum_metadata().await {
        let id_index = build_id_index::<T>(dat, "wavelength_spectrum").await?;
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

    Ok((bundle, this.query_index))
}

/// A reader for mzPeak files, abstract over the source type.
pub struct AsyncMzPeakReaderType<
    T: AsyncArchiveSource + Send + Sync = AsyncZipArchiveSource,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync = CentroidPeak,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync = DeconvolutedPeak,
> {
    url: Option<url::Url>,
    pub(crate) handle: AsyncArchiveReader<T>,
    index: usize,
    detail_level: DetailLevel,
    pub metadata: Arc<ReaderMetadata>,
    pub query_indices: Arc<QueryIndex>,
    prefer_spectra_peaks: SignalLoadingPreference,
    spectrum_metadata_cache: Option<Arc<Vec<SpectrumDescription>>>,
    chromatogram_metadata_cache: Option<Arc<Vec<ChromatogramDescription>>>,
    wavelength_spectrum_metadata_cache: Option<Arc<Vec<SpectrumDescription>>>,
    spectrum_data_cache: CacheBuffer,
    spectrum_peak_cache: CacheBuffer,
    _t: PhantomData<(C, D)>,
}

impl<
    T: AsyncArchiveSource + Send + Sync,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
> PointDataArrayReader for AsyncMzPeakReaderType<T, C, D>
{
}

impl<
    T: AsyncArchiveSource + Send + Sync,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
> MSDataFileMetadata for AsyncMzPeakReaderType<T, C, D>
{
    mzdata::delegate_impl_metadata_trait!(metadata);
}

impl<
    T: AsyncArchiveSource + Send + Sync,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
> AsyncRandomAccessSpectrumIterator<C, D, MultiLayerSpectrum<C, D>>
    for AsyncMzPeakReaderType<T, C, D>
{
    async fn start_from_id(&mut self, id: &str) -> Result<&mut Self, SpectrumAccessError> {
        if let Some(idx) = self.metadata.spectra.id_index.get(id) {
            self.index = idx as usize;
            Ok(self)
        } else {
            Err(SpectrumAccessError::SpectrumIdNotFound(id.to_string()))
        }
    }

    async fn start_from_index(&mut self, index: usize) -> Result<&mut Self, SpectrumAccessError> {
        if index < self.len() {
            self.index = index;
            Ok(self)
        } else {
            Err(SpectrumAccessError::SpectrumIndexNotFound(index))
        }
    }

    async fn start_from_time(&mut self, time: f64) -> Result<&mut Self, SpectrumAccessError> {
        match self.get_spectrum_by_time(time).await {
            Some(s) => {
                self.index = s.index();
                Ok(self)
            }
            None => Err(SpectrumAccessError::SpectrumNotFound),
        }
    }
}

impl<
    T: AsyncArchiveSource + Send + Sync,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
> AsyncSpectrumSource<C, D, MultiLayerSpectrum<C, D>> for AsyncMzPeakReaderType<T, C, D>
{
    async fn reset(&mut self) {
        self.index = 0;
    }

    fn detail_level(&self) -> &DetailLevel {
        &self.detail_level
    }

    fn set_detail_level(&mut self, detail_level: DetailLevel) {
        self.detail_level = detail_level
    }

    async fn get_spectrum_by_id(&mut self, id: &str) -> Option<MultiLayerSpectrum<C, D>> {
        let index = self.metadata.spectra.id_index.get(id)?;
        self.get_spectrum(index as usize).await
    }

    async fn get_spectrum_by_index(&mut self, index: usize) -> Option<MultiLayerSpectrum<C, D>> {
        self.get_spectrum(index).await
    }

    fn get_index(&self) -> &OffsetIndex {
        &self.metadata.spectra.id_index
    }

    fn set_index(&mut self, index: OffsetIndex) {
        Arc::make_mut(&mut self.metadata).spectra.id_index = index;
    }

    async fn read_next(&mut self) -> Option<MultiLayerSpectrum<C, D>> {
        if self.spectrum_metadata_cache.is_none() {
            if let Err(e) = self.load_all_spectrum_metadata().await {
                log::error!("Failed to eagerly load spectrum metadata: {e}");
            }
        }
        if self.index >= self.len() {
            return None;
        }
        let spec = self.get_spectrum(self.index).await;
        self.index += 1;
        spec
    }
}

impl<
    T: AsyncArchiveSource + Send + Sync,
    C: CentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
    D: DeconvolutedCentroidLike + BuildArrayMapFrom + BuildFromArrayMap + Send + Sync,
> AsyncMzPeakReaderType<T, C, D>
{
    /// Create a new mzPeak reader from an [`AsyncArchiveReader`].
    ///
    /// A [`Url`] may optionally record where the archive came from.
    pub async fn from_archive_reader(
        handle: AsyncArchiveReader<T>,
        url: Option<Url>,
    ) -> io::Result<Self> {
        let (metadata, query_indices) = load_indices_from(&handle).await?;
        let mut this = Self {
            url,
            index: 0,
            detail_level: DetailLevel::Full,
            handle,
            prefer_spectra_peaks: SignalLoadingPreference::default(),
            metadata: Arc::new(metadata),
            query_indices: Arc::new(query_indices),
            spectrum_metadata_cache: None,
            chromatogram_metadata_cache: None,
            wavelength_spectrum_metadata_cache: None,
            spectrum_data_cache: Default::default(),
            spectrum_peak_cache: Default::default(),
            _t: PhantomData,
        };

        this.load_delta_models()
            .await
            .inspect_err(|e| log::debug!("Failed to load spectrum delta model: {e}"))
            .unwrap_or_default();
        let spectrum_auxiliary_array_counts = this
            .load_spectrum_auxiliary_array_count()
            .await
            .inspect_err(|e| {
                log::debug!("Failed to load spectrum auxiliary array information: {e}")
            })
            .unwrap_or_default();
        let chromatogram_auxiliary_array_counts = this
            .load_chromatogram_auxiliary_array_count()
            .await
            .inspect_err(|e| {
                log::debug!("Failed to load chromatogram auxiliary array information: {e}")
            })
            .unwrap_or_default();
        let wavelength_auxiliary_array_counts = this
            .load_wavelength_spectrum_auxiliary_array_count()
            .await
            .inspect_err(|e| {
                log::debug!("Failed to load wavelength spectrum auxiliary array information: {e}")
            })
            .unwrap_or_default();

        let meta = Arc::get_mut(&mut this.metadata).unwrap();
        meta.spectra.auxiliary_array_counts = spectrum_auxiliary_array_counts;
        meta.chromatograms.auxiliary_array_counts = chromatogram_auxiliary_array_counts;
        if let Some(wavelength) = meta.wavelength_spectra.as_mut() {
            wavelength.auxiliary_array_counts = wavelength_auxiliary_array_counts;
        }

        Ok(this)
    }

    /// Set the size of the spectrum data cache.
    ///
    /// The larger this cache is, the more *regions* of the spectrum index space that will be fast to re-visit.
    pub fn set_spectrum_row_group_cache_size(&mut self, max_size: usize) {
        self.spectrum_data_cache = CacheBuffer::with_max_size(max_size);
    }

    /// Fetch whether to prefer reading centroid data when both centroid peaks and profile spectra data
    /// are available.
    ///
    /// If only one is available, this has no effect.
    pub fn prefer_spectra_peaks(&self) -> SignalLoadingPreference {
        self.prefer_spectra_peaks
    }

    /// Set whether to prefer reading centroid data when both centroid peaks and profile spectra data
    /// are available.
    ///
    /// If only one is available, this has no effect.
    pub fn set_prefer_spectra_peaks(&mut self, prefer: SignalLoadingPreference) {
        self.prefer_spectra_peaks = prefer;
    }

    pub async fn from_store_path(
        handle: Arc<dyn ObjectStore>,
        path: ObjectPath,
    ) -> io::Result<Self> {
        let handle = AsyncArchiveReader::from_store_path(handle, path).await?;
        Self::from_archive_reader(handle, None).await
    }

    pub async fn from_url(url: Url) -> io::Result<Self> {
        let handle = AsyncArchiveReader::<T>::from_url(url.to_string()).await?;
        Self::from_archive_reader(handle, Some(url)).await
    }

    /// Get the number of spectra in the archive
    pub fn len(&self) -> usize {
        self.metadata.spectra.id_index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.metadata.spectra.id_index.is_empty()
    }

    pub fn url(&self) -> Option<&Url> {
        self.url.as_ref()
    }

    /// Check if a specific [`CURIE`] has been mapped to a column
    pub fn has_column_for_accession(
        &self,
        entity_type: &EntityType,
        data_kind: &DataKind,
        accession: CURIE,
    ) -> Option<&crate::param::MetadataColumn> {
        self.file_index()
            .find_entry(entity_type, data_kind)
            .and_then(|v| v.column_mapping.find(accession))
    }

    /// Read a specific [`MetadataColumn`] from an [`EntityType`] and [`DataKind`] into Arrow [`ParquetRecordBatchStreamBuilder`]
    ///
    /// The builder may be customized further before invoking [`ParquetRecordBatchStreamBuilder::build`] and processing the
    /// resulting [`Iterator`] of [`RecordBatch`](arrow::array::RecordBatch)
    pub async fn extract_column_for(
        &self,
        entity_type: &EntityType,
        data_kind: &DataKind,
        metadata_column: &crate::param::MetadataColumn,
    ) -> io::Result<ParquetRecordBatchStreamBuilder<T::File>> {
        let builder = self.open_parquet_entry(entity_type, data_kind).await?;
        let mask = metadata_column.as_projection_mask(
            &builder,
            match data_kind {
                DataKind::Metadata | DataKind::DataArray | DataKind::Peaks => 1,
                _ => 2,
            },
        );
        let reader = builder.with_projection(mask);
        Ok(reader)
    }

    /// Read a specific [`MetadataColumn`] from an [`EntityType`] and [`DataKind`] into Arrow [`ArrayRef`] of
    /// row group minimum and maximum values.
    pub async fn extract_row_group_statistics_for(
        &self,
        entity_type: &EntityType,
        data_kind: &DataKind,
        metadata_column: &crate::param::MetadataColumn,
    ) -> io::Result<(Option<ArrayRef>, Option<ArrayRef>)> {
        let builder = self.open_parquet_entry(entity_type, data_kind).await?;
        Ok(metadata_column.parquet_statistics(&builder))
    }

    /// Query the spectrum metadata to obtain the lowest and highest observed m/z as reported
    /// by columns mapped to `MS:1000528` and `MS:1000527`.
    ///
    /// This queries Parquet row group statistics.
    pub async fn observed_mz_range(&self) -> (Option<f64>, Option<f64>) {
        let arc = match self.handle.spectrum_metadata().await {
            Ok(arc) => arc,
            Err(e) => {
                log::error!("Failed to locate spectrum metadata file in archive: {e}");
                return (None, None);
            }
        };
        if let Some(fentry) = self
            .file_index()
            .iter()
            .find(|v| v.entity_type == EntityType::Spectrum && v.data_kind == DataKind::Metadata)
        {
            let lowest_obs = fentry
                .column_mapping_for(curie!(MS:1000528))
                .and_then(|c| c.parquet_statistics(&arc).0);
            let highest_obs = fentry
                .column_mapping_for(curie!(MS:1000527))
                .and_then(|c| c.parquet_statistics(&arc).1);
            let mut min_mz: Option<f64> = None;
            let mut max_mz: Option<f64> = None;
            if let Some(lowest_obs) = lowest_obs {
                min_mz = match lowest_obs.data_type() {
                    DataType::Float32 => {
                        arrow::compute::min(lowest_obs.as_primitive::<Float32Type>())
                            .map(|v| v as f64)
                    }
                    DataType::Float64 => {
                        arrow::compute::min(lowest_obs.as_primitive::<Float64Type>())
                    }
                    dtype => {
                        unimplemented!("Lowest observed m/z type {dtype:?} not yet implemented")
                    }
                };
            }
            if let Some(highest_obs) = highest_obs {
                max_mz = match highest_obs.data_type() {
                    DataType::Float32 => {
                        arrow::compute::max(highest_obs.as_primitive::<Float32Type>())
                            .map(|v| v as f64)
                    }
                    DataType::Float64 => {
                        arrow::compute::max(highest_obs.as_primitive::<Float64Type>())
                    }
                    dtype => {
                        unimplemented!("Lowest observed m/z type {dtype:?} not yet implemented")
                    }
                };
            }
            (min_mz, max_mz)
        } else {
            (None, None)
        }
    }

    /// Load the descriptive metadata for all spectra
    ///
    /// This method caches the data after its first use.
    pub async fn load_all_spectrum_metadata(
        &mut self,
    ) -> io::Result<Option<Arc<Vec<SpectrumDescription>>>> {
        if self.spectrum_metadata_cache.is_none() {
            self.spectrum_metadata_cache = Some(Arc::new(
                self.load_all_spectrum_metadata_impl()
                    .await
                    .inspect_err(|e| log::error!("Failed to load spectrum metadata cache: {e}"))?,
            ));
        }
        Ok(self.spectrum_metadata_cache.clone())
    }

    /// Load the descriptive metadata for all chromatograms
    ///
    /// This method caches the data after its first use.
    pub async fn load_all_chromatogram_metadata(
        &mut self,
    ) -> io::Result<Option<Arc<Vec<ChromatogramDescription>>>> {
        if self.chromatogram_metadata_cache.is_none() {
            self.chromatogram_metadata_cache = Some(Arc::new(
                self.load_all_chromatgram_metadata_impl()
                    .await
                    .inspect_err(|e| {
                        log::error!("Failed to load chromatogram metadata cache: {e}")
                    })?,
            ));
        }
        Ok(self.chromatogram_metadata_cache.clone())
    }

    /// Load the descriptive metadata for all wavelength spectra
    ///
    /// This method caches the data after its first use.
    pub async fn load_all_wavelength_spectrum_metadata(
        &mut self,
    ) -> io::Result<Option<Arc<Vec<SpectrumDescription>>>> {
        if self.wavelength_spectrum_metadata_cache.is_none() {
            self.wavelength_spectrum_metadata_cache = Some(Arc::new(
                self.load_all_wavelength_spectrum_metadata_impl()
                    .await
                    .inspect_err(|e| {
                        log::error!("Failed to load wavelength spectrum metadata cache: {e}")
                    })?,
            ));
        }
        Ok(self.wavelength_spectrum_metadata_cache.clone())
    }

    /// Load the [`DataCacheBlock`] covering a row group or retrieve the current cache block if it matches the request
    async fn read_spectrum_data_cache(
        &mut self,
        row_group_index: usize,
        spectrum_index: u64,
    ) -> io::Result<&mut DataCacheBlock> {
        if self
            .spectrum_data_cache
            .contains(row_group_index, spectrum_index)
        {
            log::trace!("Spectrum data cache hit {row_group_index:?}:{spectrum_index}");
        } else {
            log::trace!("Spectrum data cache miss {row_group_index:?}:{spectrum_index}");
            match DataCacheBlock::load_data_for_async(self, row_group_index, spectrum_index).await?
            {
                Some(cache) => self.spectrum_data_cache.accept(cache),
                None => {
                    return Err(io::Error::other(format!(
                        "Failed to load data cache for {row_group_index:?} {spectrum_index}"
                    )));
                }
            }
        }
        self.spectrum_data_cache
            .get_mut(row_group_index, spectrum_index)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "Data cache block missing for {row_group_index:?} {spectrum_index}"
                ))
            })
    }

    /// Read load descriptive metadata for the spectrum at `index`
    pub async fn get_spectrum_metadata(
        &self,
        index: u64,
    ) -> io::Result<Option<SpectrumDescription>> {
        if let Some(cache) = self.spectrum_metadata_cache.as_ref() {
            return Ok(cache.get(index as usize).cloned());
        }

        let mut decoder = SpectrumMetadataDecoder::new(&self.metadata.spectra);

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata().await?);
        let rows =
            builder.prepare_rows_for(index, &self.query_indices.spectrum, DataKind::Metadata);
        let predicate = builder.prepare_predicate_for(index);
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_spectrum(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_scans().await?);
        let rows = builder.prepare_rows_for(index, &self.query_indices.spectrum, DataKind::Scans);
        let predicate = builder.prepare_predicate_for(index);
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_scan(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_precursors().await?);
        let rows =
            builder.prepare_rows_for(index, &self.query_indices.spectrum, DataKind::Precursors);
        let predicate = builder.prepare_predicate_for(index);
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_precursor(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_selected_ions().await?);
        let rows =
            builder.prepare_rows_for(index, &self.query_indices.spectrum, DataKind::SelectedIons);
        let predicate = builder.prepare_predicate_for(index);
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_selected_ion(batch);
        }

        let descriptions = decoder.finish();
        Ok(descriptions.into_iter().find(|v| v.index as u64 == index))
    }

    /// Retrieve the metadata for a spectrum by its `nativeId`
    pub async fn get_spectrum_metadata_by_id(
        &self,
        id: &str,
    ) -> io::Result<Option<SpectrumDescription>> {
        if let Some(idx) = self.metadata.spectra.id_index.get(id) {
            return self.get_spectrum_metadata(idx).await;
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Spectrum id \"{id}\" not found"),
        ))
    }

    /// Retrieve a complete spectrum by its index
    pub async fn get_spectrum(&mut self, index: usize) -> Option<MultiLayerSpectrum<C, D>> {
        let description = self
            .get_spectrum_metadata(index as u64)
            .await
            .inspect_err(|e| log::error!("Failed to read spectrum metadata for {index}: {e}"))
            .ok()??;
        let (arrays, peaks) = if self.detail_level == DetailLevel::Full {
            let mut read_profiles = self
                .metadata
                .spectra
                .data_point_counts()
                .get(index)
                .copied()
                .unwrap_or_default()
                > 0;
            let mut read_peaks = self
                .metadata
                .spectra
                .peak_counts()
                .get(index)
                .copied()
                .unwrap_or_default()
                > 0;

            if read_profiles && read_peaks {
                match self.prefer_spectra_peaks {
                    SignalLoadingPreference::Profiles => {
                        read_peaks = false;
                    }
                    SignalLoadingPreference::Centroids => {
                        read_profiles = false;
                    }
                    SignalLoadingPreference::ProfilesAndCentroids => {}
                }
            }

            let arrays = if read_profiles {
                self.get_spectrum_arrays(index as u64)
                    .await
                    .inspect_err(|e| log::error!("Failed to read spectrum data for {index}: {e}"))
                    .ok()??
            } else {
                BinaryArrayMap::new()
            };

            let peaks = if read_peaks {
                self.get_spectrum_peaks_for(index as u64)
                    .await
                    .inspect_err(|e| {
                        log::error!("Failed to read spectrum peak data for {index}: {e}")
                    })
                    .ok()??
            } else {
                PeakDataLevel::Missing
            };
            (arrays, peaks)
        } else {
            (BinaryArrayMap::new(), PeakDataLevel::Missing)
        };

        let mut spectrum = MultiLayerSpectrum::from_arrays_and_description(arrays, description);

        match peaks {
            PeakDataLevel::Missing => {}
            PeakDataLevel::RawData(binary_array_map) => spectrum.arrays = Some(binary_array_map),
            PeakDataLevel::Centroid(peak_set_vec) => spectrum.peaks = Some(peak_set_vec),
            PeakDataLevel::Deconvoluted(peak_set_vec) => {
                spectrum.deconvoluted_peaks = Some(peak_set_vec)
            }
        }

        Some(spectrum)
    }

    /// Read peak data for a spectrum.
    ///
    /// # Returns
    /// - If this mzPeak archive does not have a peak data file, this method will return an Err([`io::Error`])
    /// - If this mzPeak archive does have a peak data file, but does not have an entry for the requested
    ///   spectrum index, this method will return `Ok(None)`. There may still be peak data available in the main
    ///   spectrum data file.
    pub async fn get_spectrum_peaks_for(
        &mut self,
        index: u64,
    ) -> io::Result<Option<PeakDataLevel<C, D>>> {
        let builder = self.handle.spectrum_peaks().await?;
        let meta_index = self
            .metadata
            .spectra
            .peak_indices
            .as_ref()
            .ok_or(io::Error::new(
                io::ErrorKind::NotFound,
                "peak data index was not found",
            ))?;

        let PageQuery {
            pages: _,
            row_group_indices,
        } = meta_index.query_index.query_pages(index);

        // If there is only one row group in the scan, take the fast path through the cache
        if row_group_indices.len() == 1 {
            let row_group_index = row_group_indices[0];
            if !self.spectrum_peak_cache.contains(row_group_index, index) {
                let block = match DataCacheBlock::load_data_from_parts_async(
                    builder,
                    &meta_index.query_index,
                    meta_index.array_indices.clone(),
                    row_group_index,
                    index,
                    BufferContext::Spectrum,
                )
                .await?
                {
                    Some(block) => block,
                    None => {
                        log::trace!(
                            "No peak cache block retrieved for {index} @ {row_group_index}"
                        );
                        return Ok(None);
                    }
                };
                self.spectrum_peak_cache.accept(block);
            }
            let arrays = self
                .spectrum_peak_cache
                .slice_to_arrays_of(row_group_index, index, None)?;
            return match arrays {
                Some(arrays) => match PeakDataLevel::try_from(&arrays) {
                    Ok(peaks) => Ok(Some(peaks)),
                    Err(e) => Err(e.into()),
                },
                None => Ok(None),
            };
        }

        match meta_index.query_index {
            index::GenericDataIndex::Point(ref _query_index) => {
                AsyncPointDataReader(builder, BufferContext::Spectrum)
                    .get_peak_list_for(index, meta_index)
                    .await
            }
            index::GenericDataIndex::Chunk(ref query_index) => {
                let reader = AsyncChunkReader::new(builder, BufferContext::Spectrum);
                let out = reader
                    .read_chunks_for(index, query_index, &meta_index.array_indices, None, None)
                    .await?;
                match PeakDataLevel::try_from(&out) {
                    Ok(val) => Ok(Some(val)),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Read all signal data within the specified `time_range`, optionally constrained to `mz_range` m/z values and/or
    /// `ion_mobility_range` IM values.
    ///
    /// # Arguments
    /// - `time_range`: A time interval to select spectra from.
    /// - `mz_range`: An optional m/z range to filter within.
    /// - `ion_mobility_range`: An optional ion mobility range to filter within.
    ///
    /// # Returns
    /// - An iterator over record batches covering the spectrum data: `Box<dyn Iterator<Item = Result<RecordBatch, ArrowError>> + '_>`.
    /// - A mapping from spectrum index to scan start time.
    pub async fn extract_signal(
        &mut self,
        time_range: impl Into<IntoQueryRange>,
        mz_range: Option<SimpleInterval<f64>>,
        ion_mobility_range: Option<SimpleInterval<f64>>,
        ms_level_range: Option<SimpleInterval<u8>>,
    ) -> io::Result<(
        BoxStream<'_, Result<RecordBatch, ArrowError>>,
        HashMap<u64, f64, BuildIdentityHasher<u64>>,
    )> {
        let (time_index, index_range) = match time_range.into() {
            IntoQueryRange::TimeRange(time_range) => {
                self.get_spectrum_index_range_for_time_range(time_range, ms_level_range).await?
            }
            IntoQueryRange::IndexRange(index_range) => {
                if let Some(time_axis) = self.spectrum_time_axis().await {
                    let time_axis = time_axis.as_primitive::<Float64Type>();
                    let start = time_axis.value(index_range.start() as usize);
                    let end = time_axis.value(index_range.end() as usize);
                    self.get_spectrum_index_range_for_time_range(SimpleInterval::new(start, end), ms_level_range).await?
                } else {

                    return Ok((futures::stream::empty().boxed(), Default::default()))
                }
            }
        };
        let builder = self.handle.spectra_data().await?;

        let ion_mobility_range = if !self.metadata.spectrum_array_indices().has_ion_mobility() {
            None
        } else {
            ion_mobility_range
        };

        if let Some(query_index) = self.query_indices.spectrum.data_index.as_chunked() {
            let it = AsyncChunkReader::new(builder, BufferContext::Spectrum).scan_chunks_for(
                index_range,
                mz_range,
                &self.metadata,
                self.metadata.spectrum_array_indices(),
                query_index,
            )?;
            let it: BoxStream<'_, Result<RecordBatch, ArrowError>> = if ion_mobility_range.is_some()
            {
                // If there is an ion mobility array constraint, the chunked encoding doesn't support filtering on this
                // dimension directly.
                if let Some(im_name) = self
                    .metadata
                    .spectra
                    .array_indices
                    .iter()
                    .find(|v| v.is_ion_mobility())
                {
                    let it = it.map(move |bat| -> Result<RecordBatch, ArrowError> {
                        let bat = bat?;
                        let arr = bat
                            .column(0)
                            .as_struct()
                            .column_by_name(&im_name.name)
                            .unwrap();
                        let mask = ion_mobility_range.unwrap().contains_dy(arr);
                        arrow::compute::filter_record_batch(&bat, &mask)
                    });
                    it.boxed()
                } else {
                    log::warn!(
                        "An ion mobility range was requested, but no ion mobility array was found"
                    );
                    it.boxed()
                }
            } else {
                it.boxed()
            };
            return Ok((it, time_index));
        }

        let reader = AsyncPointDataReader(builder, BufferContext::Spectrum)
            .query_points(
                index_range,
                mz_range,
                ion_mobility_range,
                self.query_indices.spectrum.data_index.as_point().unwrap(),
                &self.metadata.spectra.array_indices,
                &self.metadata,
            )
            .await?
            .boxed();
        Ok((reader, time_index))
    }

    /// Perform slicing random access over the peak data for spectra in this file.
    ///
    /// If there are no stored peaks for a given spectrum, there will be gaps.
    ///
    /// # Arguments
    /// - `time_range`: A time interval to select spectra from.
    /// - `mz_range`: An optional m/z range to filter within.
    /// - `ion_mobility_range`: An optional ion mobility range to filter within.
    ///
    /// # Returns
    /// - If this mzPeak archive does not have a peak data file, this method will return an Err([`io::Error`])
    /// - An iterator over record batches covering the spectrum data: `Box<dyn Iterator<Item = Result<RecordBatch, ArrowError>> + '_>`.
    /// - A mapping from spectrum index to scan start time.
    pub async fn query_peaks(
        &mut self,
        time_range: impl Into<IntoQueryRange>,
        mz_range: Option<SimpleInterval<f64>>,
        ion_mobility_range: Option<SimpleInterval<f64>>,
        ms_level_range: Option<SimpleInterval<u8>>,
    ) -> io::Result<(
        BoxStream<'_, Result<RecordBatch, ArrowError>>,
        HashMap<u64, f64, BuildIdentityHasher<u64>>,
    )> {
        let builder = self.handle.spectrum_peaks().await?;
        let meta_index = self
            .metadata
            .spectra
            .peak_indices
            .as_ref()
            .ok_or(io::Error::new(
                io::ErrorKind::NotFound,
                "peak metadata was not found",
            ))?;

        let ion_mobility_range = if !meta_index.array_indices.has_ion_mobility() {
            None
        } else {
            ion_mobility_range
        };

        let (time_index, index_range) = match time_range.into() {
            IntoQueryRange::TimeRange(time_range) => {
                self.get_spectrum_index_range_for_time_range(time_range, ms_level_range).await?
            }
            IntoQueryRange::IndexRange(index_range) => {
                if let Some(time_axis) = self.spectrum_time_axis().await {
                    let time_axis = time_axis.as_primitive::<Float64Type>();
                    let start = time_axis.value(index_range.start() as usize);
                    let end = time_axis.value(index_range.end() as usize);
                    self.get_spectrum_index_range_for_time_range(SimpleInterval::new(start, end), ms_level_range).await?
                } else {

                    return Ok((futures::stream::empty().boxed(), Default::default()))
                }
            }
        };

        let iter = AsyncPointDataReader(builder, BufferContext::Spectrum)
            .query_points(
                index_range,
                mz_range,
                ion_mobility_range,
                &meta_index.query_index,
                &meta_index.array_indices,
                &self.metadata,
            )
            .await?;
        Ok((iter, time_index))
    }

    pub async fn get_spectrum_index_range_for_time_range(
        &self,
        time_range: SimpleInterval<f64>,
        ms_level_range: Option<SimpleInterval<u8>>,
    ) -> io::Result<(HashMap<u64, f64, BuildIdentityHasher<u64>>, MaskSet)> {
        let mut time_indexer = TimeIndexDecoder::new(time_range, ms_level_range);
        if let Some(cache) = self.spectrum_metadata_cache.as_ref() {
            time_indexer.from_descriptions(cache.as_slice());
            return Ok(time_indexer.finish());
        }

        let rows = self
            .query_indices
            .spectrum
            .time_index
            .row_selection_overlaps(&time_range);

        let builder = self.handle.spectrum_metadata().await?;

        let has_ms_level_range = ms_level_range.is_some();
        let ms_level_range = ms_level_range.unwrap_or_default();

        let mut columns_for_predicate = vec![String::from("time")];

        if has_ms_level_range {
            if let Some(metadata_map) = self.metadata.spectra.primary_metadata_map() {
                if let Some(col) = metadata_map.find(curie!(MS:1000511)) {
                    let name = col.leaf().unwrap().to_string();
                    columns_for_predicate.push(name);
                } else {
                    return Err(io::Error::other("ms_level column not found"));
                }
            }
        }

        let predicate_mask = ProjectionMask::columns(
            builder.parquet_schema(),
            columns_for_predicate.iter().map(|s| s.as_str()),
        );

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let root = batch;
            let times = root.column_by_name("time").unwrap();
            if has_ms_level_range {
                let ms_levels = root
                    .column_by_name(columns_for_predicate.last().unwrap())
                    .unwrap();
                arrow::compute::and(
                    &time_range.contains_dy(times),
                    &ms_level_range.contains_dy(ms_levels),
                )
            } else {
                Ok(time_range.contains_dy(times))
            }
        });

        let proj = ProjectionMask::columns(builder.parquet_schema(), ["index", "time"]);

        let mut reader = builder
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .with_projection(proj)
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            time_indexer.decode_batch(batch)?;
        }

        Ok(time_indexer.finish())
    }

    /// Get the time dimension encoded in the spectrum metadata table
    pub async fn spectrum_time_axis(&self) -> Option<ArrayRef> {
        let builder = self.handle.spectrum_metadata().await.ok()?;

        let schema = builder.parquet_schema();
        let i = schema.columns().iter().position(|c| c.name() == "time")?;

        let mask = ProjectionMask::leaves(schema, [i]);
        let mut reader = builder
            .with_projection(mask)
            .with_batch_size(usize::MAX)
            .build()
            .ok()?;

        let batch = reader.next().await?;
        let arr = batch.ok()?.column(0).clone();
        if matches!(arr.data_type(), DataType::Float64) {
            return Some(arr);
        } else {
            return arrow::compute::cast(&arr, &DataType::Float64).ok();
        }
    }

    pub(crate) async fn load_all_spectrum_metadata_impl(
        &self,
    ) -> io::Result<Vec<SpectrumDescription>> {
        log::trace!("Loading all spectrum metadata");
        let mut decoder = SpectrumMetadataDecoder::new(&self.metadata.spectra);

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata().await?);
        let rows = builder.prepare_rows_for_all(&self.query_indices.spectrum, DataKind::Metadata);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .with_batch_size(10_000)
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_spectrum(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_scans().await?);
        let rows = builder.prepare_rows_for_all(&self.query_indices.spectrum, DataKind::Scans);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .with_batch_size(10_000)
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_scan(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_precursors().await?);
        let rows = builder.prepare_rows_for_all(&self.query_indices.spectrum, DataKind::Precursors);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .with_batch_size(10_000)
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_precursor(batch);
        }

        let builder = SpectrumMetadataReader(self.handle.spectrum_metadata_selected_ions().await?);
        let rows =
            builder.prepare_rows_for_all(&self.query_indices.spectrum, DataKind::SelectedIons);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .with_batch_size(10_000)
            .build()?;

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_selected_ion(batch);
        }

        let descriptions = decoder.finish();

        log::trace!("Finished loading all spectrum metadata");
        Ok(descriptions)
    }

    pub(crate) async fn load_all_chromatgram_metadata_impl(
        &self,
    ) -> io::Result<Vec<ChromatogramDescription>> {
        let mut decoder = ChromatogramMetadataDecoder::new(&self.metadata);

        let builder = ChromatogramMetadataReader(self.handle.chromatograms_metadata().await?);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;
        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_chromatogram(batch);
        }

        let builder =
            ChromatogramMetadataReader(self.handle.chromatograms_metadata_precursors().await?);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;
        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_precursor(batch);
        }

        let builder =
            ChromatogramMetadataReader(self.handle.chromatograms_metadata_selected_ions().await?);
        let predicate = builder.prepare_predicate_for_all();
        let mut reader = builder
            .0
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;
        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_selected_ion(batch);
        }

        Ok(decoder.finish())
    }

    pub(crate) async fn load_spectrum_auxiliary_array_count(&self) -> io::Result<Vec<u32>> {
        let builder = self.handle.spectrum_metadata().await?;

        let mut decoder = AuxiliaryArrayCountDecoder::new(BufferContext::Spectrum);

        let proj = match decoder.build_projection(&builder) {
            Some(proj) => proj,
            None => return Ok(Vec::new()),
        };

        let mut reader = builder.with_projection(proj).build()?;
        let n = self.len();
        decoder.resize(n);

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch(&batch);
        }
        Ok(decoder.finish())
    }

    pub(crate) async fn load_chromatogram_auxiliary_array_count(&self) -> io::Result<Vec<u32>> {
        let builder = match self.handle.chromatograms_metadata().await {
            Ok(builder) => builder,
            Err(e) => {
                log::trace!("{e}");
                return Ok(Vec::new());
            }
        };

        let mut decoder = AuxiliaryArrayCountDecoder::new(BufferContext::Chromatogram);

        let proj = match decoder.build_projection(&builder) {
            Some(proj) => proj,
            None => return Ok(Vec::new()),
        };

        let mut reader = builder.with_projection(proj).build()?;
        decoder.resize(self.len_chromatograms());

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch(&batch);
        }
        Ok(decoder.finish())
    }

    /// A thin wrapper around the auxiliary array count decoder for [`BufferContext::WavelengthSpectrum`]
    pub(crate) async fn load_wavelength_spectrum_auxiliary_array_count(
        &self,
    ) -> io::Result<Vec<u32>> {
        let builder = match self.handle.wavelength_spectrum_metadata().await {
            Some(builder) => builder?,
            None => return Ok(Vec::new()),
        };

        let mut decoder = AuxiliaryArrayCountDecoder::new(BufferContext::WavelengthSpectrum);

        let proj = match decoder.build_projection(&builder) {
            Some(proj) => proj,
            None => return Ok(Vec::new()),
        };

        let mut reader = builder.with_projection(proj).build()?;
        decoder.resize(self.len_wavelength_spectra());

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch(&batch);
        }
        Ok(decoder.finish())
    }

    async fn load_auxiliary_arrays_from(
        &self,
        mut reader: ParquetRecordBatchStream<T::File>,
    ) -> io::Result<Vec<DataArray>> {
        let mut results = Vec::new();

        while let Some(root) = reader.next().await.transpose()? {
            if let Some(data) = root.column(1).as_list_opt::<i64>() {
                let data = data.values().as_struct();
                let arrays = AuxiliaryArrayVisitor::default().visit(data);
                results.extend(arrays);
            } else if let Some(data) = root.column(1).as_list_opt::<i32>() {
                let data = data.values().as_struct();
                let arrays = AuxiliaryArrayVisitor::default().visit(data);
                results.extend(arrays);
            } else {
                log::warn!(
                    "Unexpected auxiliary array column type {:?}",
                    root.column(1).data_type()
                );
            }
        }
        Ok(results)
    }

    pub(crate) async fn load_auxiliary_arrays_for_chromatogram(
        &self,
        index: u64,
    ) -> io::Result<Vec<DataArray>> {
        if self
            .metadata
            .chromatogram_auxiliary_array_counts()
            .get(index as usize)
            .copied()
            .unwrap_or_default()
            == 0
        {
            return Ok(Vec::new());
        }

        let builder = self.handle.chromatograms_metadata().await?;
        let predicate_mask =
            ProjectionMask::columns(builder.parquet_schema(), ["index", "auxiliary_arrays"]);

        let proj = predicate_mask.clone();

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let index_array: &UInt64Array = batch.column(0).as_primitive();
            Ok(index_array.iter().map(|v| v.map(|i| i == index)).collect())
        });

        let filter = RowFilter::new(vec![Box::new(predicate)]);

        let reader = builder
            .with_projection(proj)
            .with_row_filter(filter)
            .build()?;

        self.load_auxiliary_arrays_from(reader).await
    }

    pub(crate) async fn load_auxiliary_arrays_for_spectrum(
        &self,
        index: u64,
    ) -> io::Result<Vec<DataArray>> {
        if self
            .metadata
            .spectrum_auxiliary_array_counts()
            .get(index as usize)
            .copied()
            .unwrap_or_default()
            == 0
        {
            return Ok(Vec::new());
        }

        let builder = self.handle.spectrum_metadata().await?;

        let rows = self
            .query_indices
            .spectrum
            .index_index
            .row_selection_contains(index);

        let predicate_mask =
            ProjectionMask::columns(builder.parquet_schema(), ["index", "auxiliary_arrays"]);

        let proj = predicate_mask.clone();

        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let spectrum_index: &UInt64Array = batch.column(0).as_primitive();
            Ok(spectrum_index
                .iter()
                .map(|v| v.map(|i| i == index))
                .collect())
        });

        let filter = RowFilter::new(vec![Box::new(predicate)]);

        let reader = builder
            .with_projection(proj)
            .with_row_filter(filter)
            .with_row_selection(rows)
            .build()?;

        self.load_auxiliary_arrays_from(reader).await
    }

    /// Load median delta coefficient column if it is present.
    pub(crate) async fn load_delta_models(&mut self) -> io::Result<()> {
        let builder = self.handle.spectrum_metadata().await?;

        let mut decoder =
            PeakInfoDecoder::new(self.metadata.spectra.primary_metadata_map().unwrap());

        let proj = match decoder.build_projection(&builder) {
            Some(proj) => proj,
            None => return Ok(()),
        };

        let mut reader = builder
            .with_projection(proj)
            .with_batch_size(10_000)
            .build()?;

        let n = self.metadata.spectra.id_index.len();
        decoder.resize(n);

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch(&batch);
        }

        let model_parameters = decoder.model_parameters;
        let data_point_counts = decoder.data_point_counts;
        let peak_counts = decoder.peak_counts;

        let meta = Arc::get_mut(&mut self.metadata).unwrap();
        meta.spectra.mz_model_deltas = model_parameters;
        meta.spectra.data_point_counts = data_point_counts;
        meta.spectra.peak_counts = peak_counts;
        Ok(())
    }

    /// Read the complete data arrays for the spectrum at `index`
    pub async fn get_spectrum_arrays(&mut self, index: u64) -> io::Result<Option<BinaryArrayMap>> {
        let delta_model = self.metadata.model_deltas_for(index as usize);
        let builder = self.handle.spectra_data().await?;

        let PageQuery {
            pages,
            row_group_indices,
        } = self.query_indices.query_pages(index);

        // If there is only one row group in the scan, take the fast path through the cache
        if row_group_indices.len() == 1 {
            let row_group_index = row_group_indices[0];
            let rg = self
                .read_spectrum_data_cache(row_group_index, index)
                .await?;
            let mut arrays = rg
                .slice_to_arrays_of(row_group_index, index, delta_model.as_ref())?
                .unwrap_or_default();
            for v in self.load_auxiliary_arrays_for_spectrum(index).await? {
                arrays.add(v);
            }
            return Ok(Some(arrays));
        }

        if let Some(query_index) = self.query_indices.spectrum.data_index.as_chunked() {
            log::trace!("Using chunk strategy for reading spectrum {index}");
            let mut out = AsyncChunkReader::new(builder, BufferContext::Spectrum)
                .read_chunks_for(
                    index,
                    query_index,
                    &self.metadata.spectra.array_indices,
                    delta_model.as_ref(),
                    Some(PageQuery::new(row_group_indices, pages)),
                )
                .await?;
            for v in self.load_auxiliary_arrays_for_spectrum(index).await? {
                out.add(v);
            }
            return Ok(Some(out));
        }

        if pages.is_empty() {
            let mut out = BinaryArrayMap::new();
            for v in self.load_auxiliary_arrays_for_spectrum(index).await? {
                out.add(v);
            }
            return Ok(Some(out));
        };

        let reader = AsyncPointDataReader(builder, crate::BufferContext::Spectrum);

        if let Some(mut out) = reader
            .read_points_of(
                index,
                self.query_indices.spectrum.data_index.as_point().unwrap(),
                self.metadata.spectrum_array_indices(),
                delta_model.as_ref(),
            )
            .await?
        {
            for v in self.load_auxiliary_arrays_for_spectrum(index).await? {
                out.add(v);
            }
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    /// Read load descriptive metadata for the chromatogram trace at `index`
    pub async fn get_chromatogram_metadata(
        &mut self,
        index: u64,
    ) -> io::Result<Option<ChromatogramDescription>> {
        Ok(self
            .load_all_chromatogram_metadata()
            .await?
            .and_then(|v| v.get(index as usize).cloned()))
    }

    /// Get the number of chromatograms stored in the archive, not counting the TIC and BPC that
    /// [`Self::encoded_tic`] and [`Self::encoded_bpc`] can derive from the spectrum table.
    ///
    /// See [`Self::count_chromatograms`] for the count including those fallbacks.
    pub fn len_chromatograms(&self) -> usize {
        self.metadata.chromatograms.id_index.len()
    }

    /// The equivalent of [`mzdata::io::ChromatogramSource::count_chromatograms`], which has no asynchronous
    /// counterpart in `mzdata`.
    ///
    /// If the archive has no chromatogram metadata table, this is `2`, the TIC and BPC that can be derived from
    /// the spectrum table.
    pub async fn count_chromatograms(&self) -> usize {
        self.handle
            .chromatograms_metadata()
            .await
            .map(|v| {
                v.metadata()
                    .row_groups()
                    .iter()
                    .map(|rg| rg.num_rows())
                    .sum::<i64>() as usize
            })
            .unwrap_or(2)
    }

    /// Retrieve a complete chromatogram by its index
    pub async fn get_chromatogram(&mut self, index: usize) -> Option<Chromatogram> {
        let description = self
            .get_chromatogram_metadata(index as u64)
            .await
            .inspect_err(|e| log::error!("Failed to read chromatogram metadata for {index}: {e}"))
            .ok()??;
        let arrays = if self.detail_level == DetailLevel::Full {
            self.get_chromatogram_arrays(index as u64)
                .await
                .inspect_err(|e| log::error!("Failed to read chromatogram data for {index}: {e}"))
                .ok()??
        } else {
            BinaryArrayMap::new()
        };

        Some(Chromatogram::new(description, arrays))
    }

    /// Retrieve a complete chromatogram by its unique ID
    pub async fn get_chromatogram_by_id(&mut self, id: &str) -> Option<Chromatogram> {
        let description = self
            .load_all_chromatogram_metadata()
            .await
            .ok()??
            .iter()
            .find(|v| v.id == id)
            .cloned()?;
        let arrays = if self.detail_level == DetailLevel::Full {
            self.get_chromatogram_arrays(description.index as u64)
                .await
                .inspect_err(|e| log::error!("Failed to read chromatogram data for {id}: {e}"))
                .ok()??
        } else {
            BinaryArrayMap::new()
        };

        Some(Chromatogram::new(description, arrays))
    }

    /// Like [`Self::get_chromatogram_by_id`], but falls back to [`Self::encoded_tic`] for `"TIC"` and
    /// [`Self::encoded_bpc`] for `"BPC"`.
    ///
    /// This mirrors the synchronous reader's [`mzdata::io::ChromatogramSource`] implementation.
    pub async fn get_chromatogram_by_id_or_encoded(&mut self, id: &str) -> Option<Chromatogram> {
        if let Some(chrom) = self.get_chromatogram_by_id(id).await {
            return Some(chrom);
        }
        match id {
            "TIC" => self.encoded_tic().await.ok(),
            "BPC" => self.encoded_bpc().await.ok(),
            _ => None,
        }
    }

    /// Like [`Self::get_chromatogram`], but falls back to [`Self::encoded_tic`] for index `0` and
    /// [`Self::encoded_bpc`] for index `1`.
    ///
    /// This mirrors the synchronous reader's [`mzdata::io::ChromatogramSource`] implementation.
    pub async fn get_chromatogram_by_index_or_encoded(
        &mut self,
        index: usize,
    ) -> Option<Chromatogram> {
        if let Some(chrom) = self.get_chromatogram(index).await {
            return Some(chrom);
        }
        match index {
            0 => self.encoded_tic().await.ok(),
            1 => self.encoded_bpc().await.ok(),
            _ => None,
        }
    }

    /// Read a time-indexed series out of the spectrum metadata table, where `targets` are the candidate
    /// column names for the time and measurement, in that order.
    async fn read_encoded_series(
        &self,
        targets: &[String],
        measure_name: ArrayType,
        id: &str,
        index: usize,
        chromatogram_type: ChromatogramType,
    ) -> io::Result<Chromatogram> {
        let builder = self.handle.spectrum_metadata().await?;
        let rows = self
            .query_indices
            .spectrum
            .index_index
            .row_selection_is_not_null();

        let proj =
            ProjectionMask::columns(builder.parquet_schema(), targets.iter().map(String::as_str));

        let mut reader = builder
            .with_projection(proj)
            .with_row_selection(rows)
            .build()?;

        let mut decoder = TimeEncodedSeriesDecoder::new(0, 1);

        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch(batch);
        }

        let (mut time_array, mut intensity_array) = decoder.finish(&measure_name);

        let descr = ChromatogramDescription {
            id: id.into(),
            index,
            ms_level: None,
            chromatogram_type,
            ..Default::default()
        };

        let mut arrays = BinaryArrayMap::new();
        time_array.unit = Unit::Minute;
        arrays.add(time_array);
        intensity_array.unit = Unit::DetectorCounts;
        arrays.add(intensity_array);

        Ok(Chromatogram::new(descr, arrays))
    }

    /// Read the total ion chromatogram from the surrogate metadata in the spectrum table. This
    /// is distinct from any equivalent chromatogram explicitly stored separately.
    pub async fn encoded_tic(&mut self) -> io::Result<Chromatogram> {
        let mut targets = vec![
            "time".to_string(),
            "total_ion_current".to_string(), // deprecated name
        ];

        if let Some(col) = self
            .metadata
            .spectra
            .primary_metadata_map()
            .and_then(|v| v.find(curie!(MS:1000285)))
        {
            targets.push(col.path.join("."))
        }

        self.read_encoded_series(
            &targets,
            ArrayType::IntensityArray,
            "TIC",
            0,
            ChromatogramType::TotalIonCurrentChromatogram,
        )
        .await
    }

    /// Read the base peak chromatogram from the surrogate metadata in the spectrum table. This
    /// is distinct from any equivalent chromatogram explicitly stored separately.
    pub async fn encoded_bpc(&mut self) -> io::Result<Chromatogram> {
        let bp_col = self
            .metadata
            .spectra
            .primary_metadata_map()
            .and_then(|v| v.find(curie!(MS:1000505)))
            .ok_or_else(|| io::Error::other("column not found"))?;

        let targets = vec![
            "time".to_string(),
            "base_peak_intensity".to_string(),
            bp_col.path.join("."),
        ];

        self.read_encoded_series(
            &targets,
            ArrayType::IntensityArray,
            "BPC",
            1,
            ChromatogramType::BasePeakChromatogram,
        )
        .await
    }

    /// Read the complete data arrays for the chromatogram at `index`
    pub async fn get_chromatogram_arrays(
        &mut self,
        index: u64,
    ) -> io::Result<Option<BinaryArrayMap>> {
        let builder = self.handle.chromatograms_data().await?;

        if let Some(query_index) = self.query_indices.chromatogram_data_index.as_chunked() {
            let PageQuery {
                pages,
                row_group_indices,
            } = self.query_indices.query_chromatrogram_pages(index);
            return AsyncChunkReader::new(builder, BufferContext::Chromatogram)
                .read_chunks_for(
                    index,
                    query_index,
                    &self.metadata.chromatograms.array_indices(),
                    None,
                    Some(PageQuery::new(row_group_indices, pages)),
                )
                .await
                .map(Some);
        }

        let reader = AsyncPointDataReader(builder, BufferContext::Chromatogram);
        let out = reader
            .read_points_of(
                index,
                self.query_indices
                    .chromatogram_data_index
                    .as_point()
                    .unwrap(),
                &self.metadata.chromatograms.array_indices(),
                None,
            )
            .await?;

        if let Some(mut out) = out {
            for v in self.load_auxiliary_arrays_for_chromatogram(index).await? {
                out.add(v);
            }
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    /// Get the number of wavelength spectra in the archive
    pub fn len_wavelength_spectra(&self) -> usize {
        self.metadata
            .wavelength_spectra
            .as_ref()
            .map(|s| s.id_index.len())
            .unwrap_or_default()
    }

    /// Load wavelength spectrum metadata for the entry at `index`, or for all entries if `index` is `None`.
    ///
    /// If the archive has no wavelength spectra, this returns an empty [`Vec`].
    async fn load_wavelength_spectrum_metadata_impl(
        &self,
        index: Option<u64>,
    ) -> io::Result<Vec<SpectrumDescription>> {
        let (Some(facet), Some(query_index)) = (
            self.metadata.wavelength_spectra.as_deref(),
            self.query_indices.wavelength_spectrum_index.as_ref(),
        ) else {
            return Ok(Vec::new());
        };
        let builder = match self.handle.wavelength_spectrum_metadata().await {
            Some(builder) => builder?,
            None => return Ok(Vec::new()),
        };

        let mut decoder = SpectrumMetadataDecoder::new(facet);

        let builder = SpectrumMetadataReader(builder);
        let (rows, filter) = match index {
            Some(index) => (
                builder.prepare_rows_for(index, query_index, DataKind::Metadata),
                RowFilter::new(vec![Box::new(builder.prepare_predicate_for(index))]),
            ),
            None => (
                builder.prepare_rows_for_all(query_index, DataKind::Metadata),
                RowFilter::new(vec![Box::new(builder.prepare_predicate_for_all())]),
            ),
        };
        let mut reader = builder
            .0
            .with_row_selection(rows)
            .with_row_filter(filter)
            .with_batch_size(10_000)
            .build()?;
        while let Some(batch) = reader.next().await.transpose()? {
            decoder.decode_batch_spectrum(batch);
        }

        if let Some(builder) = self.handle.wavelength_spectrum_metadata_scans().await {
            let builder = SpectrumMetadataReader(builder?);
            let (rows, filter) = match index {
                Some(index) => (
                    builder.prepare_rows_for(index, query_index, DataKind::Scans),
                    RowFilter::new(vec![Box::new(builder.prepare_predicate_for(index))]),
                ),
                None => (
                    builder.prepare_rows_for_all(query_index, DataKind::Scans),
                    RowFilter::new(vec![Box::new(builder.prepare_predicate_for_all())]),
                ),
            };
            let mut reader = builder
                .0
                .with_row_selection(rows)
                .with_row_filter(filter)
                .with_batch_size(10_000)
                .build()?;
            while let Some(batch) = reader.next().await.transpose()? {
                decoder.decode_batch_scan(batch);
            }
        }

        let descriptions = decoder.finish();
        Ok(match index {
            Some(index) => descriptions
                .into_iter()
                .filter(|v| v.index as u64 == index)
                .collect(),
            None => descriptions,
        })
    }

    pub(crate) async fn load_all_wavelength_spectrum_metadata_impl(
        &self,
    ) -> io::Result<Vec<SpectrumDescription>> {
        self.load_wavelength_spectrum_metadata_impl(None).await
    }

    /// Read load descriptive metadata for the wavelength spectrum at `index`
    pub async fn get_wavelength_spectrum_metadata(
        &self,
        index: u64,
    ) -> io::Result<Option<SpectrumDescription>> {
        if let Some(cache) = self.wavelength_spectrum_metadata_cache.as_ref() {
            return Ok(cache.get(index as usize).cloned());
        }
        Ok(self
            .load_wavelength_spectrum_metadata_impl(Some(index))
            .await?
            .into_iter()
            .next())
    }

    async fn load_auxiliary_arrays_for_wavelength_spectrum(
        &self,
        index: u64,
    ) -> io::Result<Vec<DataArray>> {
        if self
            .metadata
            .wavelength_auxiliary_array_counts()
            .get(index as usize)
            .copied()
            .unwrap_or_default()
            == 0
        {
            return Ok(Vec::new());
        }
        let builder = match self.handle.wavelength_spectrum_metadata().await {
            Some(builder) => builder?,
            None => return Ok(Vec::new()),
        };

        let predicate_mask =
            ProjectionMask::columns(builder.parquet_schema(), ["index", "auxiliary_arrays"]);
        let proj = predicate_mask.clone();
        let predicate = ArrowPredicateFn::new(predicate_mask, move |batch| {
            let spectrum_index: &UInt64Array = batch.column(0).as_primitive();
            Ok(spectrum_index
                .iter()
                .map(|v| v.map(|i| i == index))
                .collect())
        });

        let reader = builder
            .with_projection(proj)
            .with_row_filter(RowFilter::new(vec![Box::new(predicate)]))
            .build()?;
        self.load_auxiliary_arrays_from(reader).await
    }

    /// Read the complete data arrays for the wavelength spectrum at `index`
    pub async fn get_wavelength_spectrum_arrays(
        &mut self,
        index: u64,
    ) -> io::Result<Option<BinaryArrayMap>> {
        let (Some(facet), Some(query_index)) = (
            self.metadata.wavelength_spectra.as_deref(),
            self.query_indices.wavelength_spectrum_index.as_ref(),
        ) else {
            return Ok(None);
        };
        let builder = match self.handle.wavelength_spectrum_data().await {
            Some(builder) => builder?,
            None => return Ok(None),
        };

        let mut out = if let Some(chunk_index) = query_index.data_index.as_chunked() {
            let PageQuery {
                pages,
                row_group_indices,
            } = query_index.data_index.query_pages(index);
            AsyncChunkReader::new(builder, BufferContext::WavelengthSpectrum)
                .read_chunks_for(
                    index,
                    chunk_index,
                    facet.array_indices(),
                    None,
                    Some(PageQuery::new(row_group_indices, pages)),
                )
                .await?
        } else if let Some(point_index) = query_index.data_index.as_point() {
            match AsyncPointDataReader(builder, BufferContext::WavelengthSpectrum)
                .read_points_of(index, point_index, facet.array_indices(), None)
                .await?
            {
                Some(out) => out,
                None => return Ok(None),
            }
        } else {
            return Ok(None);
        };

        for v in self.load_auxiliary_arrays_for_wavelength_spectrum(index).await? {
            out.add(v);
        }
        Ok(Some(out))
    }

    /// Retrieve a complete wavelength spectrum by its index
    pub async fn get_wavelength_spectrum(
        &mut self,
        index: usize,
    ) -> Option<MultiLayerSpectrum<C, D>> {
        let description = self
            .get_wavelength_spectrum_metadata(index as u64)
            .await
            .inspect_err(|e| log::error!("Failed to read spectrum metadata for {index}: {e}"))
            .ok()??;
        let arrays = if self.detail_level == DetailLevel::Full {
            self.get_wavelength_spectrum_arrays(index as u64)
                .await
                .inspect_err(|e| log::error!("Failed to read spectrum data for {index}: {e}"))
                .ok()??
        } else {
            BinaryArrayMap::new()
        };

        Some(MultiLayerSpectrum::from_arrays_and_description(
            arrays,
            description,
        ))
    }

    /// Retrieve a complete wavelength spectrum by its unique ID
    pub async fn get_wavelength_spectrum_by_id(
        &mut self,
        id: &str,
    ) -> Option<MultiLayerSpectrum<C, D>> {
        let index = self
            .metadata
            .wavelength_spectra
            .as_ref()
            .and_then(|w| w.id_index().get(id))?;
        self.get_wavelength_spectrum(index as usize).await
    }

    /// Get a [`Stream`] over the wavelength spectra, in index order.
    ///
    /// This is the asynchronous analogue of the synchronous reader's `iter_wavelength_spectra`. The
    /// synchronous reader's `wavelength_facet` has no asynchronous analogue; the `*_wavelength_spectrum*`
    /// methods on this type cover its functionality.
    pub async fn wavelength_spectra_stream(
        &mut self,
    ) -> io::Result<impl Stream<Item = MultiLayerSpectrum<C, D>> + '_> {
        let n = self
            .load_all_wavelength_spectrum_metadata()
            .await?
            .map(|v| v.len())
            .unwrap_or_default();
        Ok(futures::stream::unfold((self, 0usize), move |(this, i)| async move {
            if i >= n {
                return None;
            }
            let spectrum = this.get_wavelength_spectrum(i).await?;
            Some((spectrum, (this, i + 1)))
        }))
    }

    /// Get a [`Stream`] over the mass spectra starting from the reader's current position.
    ///
    /// This is the asynchronous analogue of the synchronous reader's `iter`, which `mzdata` does not provide
    /// for [`AsyncSpectrumSource`]. It first loads all spectrum metadata (see [`Self::load_all_spectrum_metadata`])
    /// so that reading proceeds without one metadata request per spectrum.
    pub async fn spectra_stream(
        &mut self,
    ) -> impl SpectrumStream<C, D, MultiLayerSpectrum<C, D>> + Unpin + '_ {
        if let Err(e) = self.load_all_spectrum_metadata().await {
            log::error!("Failed to eagerly load spectrum metadata: {e}")
        }
        self.as_stream()
    }

    /// Retrieve multiple spectra by index, internally scheduling the reads more efficiently.
    ///
    /// The spectra are returned in the order the `indices` were requested. If any spectrum cannot be
    /// read, this returns `None`.
    pub async fn get_spectra_batch(
        &mut self,
        indices: impl IntoIterator<Item = usize>,
    ) -> Option<Vec<MultiLayerSpectrum<C, D>>> {
        let mut ii: Vec<(usize, usize)> = indices.into_iter().enumerate().collect();
        ii.sort_by_key(|(_, spectrum_index)| *spectrum_index);
        // TODO: Optimize
        let mut spectra: Vec<Option<MultiLayerSpectrum<C, D>>> =
            std::iter::repeat_with(|| None).take(ii.len()).collect();
        for (origin_idx, spectrum_index) in ii {
            spectra[origin_idx] = Some(self.get_spectrum(spectrum_index).await?);
        }
        spectra.into_iter().collect()
    }

    /// Query the spectrum metadata to obtain the lowest and highest scan start times as reported
    /// by the column mapped to `MS:1000016`.
    ///
    /// This queries Parquet row group statistics.
    pub async fn observed_time_range(&self) -> (Option<f64>, Option<f64>) {
        let arc = match self.handle.spectrum_metadata_scans().await {
            Ok(arc) => arc,
            Err(e) => {
                log::error!("Failed to locate spectrum metadata file in archive: {e}");
                return (None, None);
            }
        };
        let Some(col) = self
            .file_index()
            .find_entry(&EntityType::Spectrum, &DataKind::Scans)
            .and_then(|fentry| fentry.column_mapping_for(curie!(MS:1000016)))
        else {
            return (None, None);
        };
        let (min, max) = col.parquet_statistics(&arc);

        let min = min.and_then(|min| match min.data_type() {
            DataType::Float32 => {
                arrow::compute::min(min.as_primitive::<Float32Type>()).map(|v| v as f64)
            }
            DataType::Float64 => arrow::compute::min(min.as_primitive::<Float64Type>()),
            dtype => {
                unimplemented!("Lowest scan start time type {dtype:?} not yet implemented")
            }
        });
        let max = max.and_then(|max| match max.data_type() {
            DataType::Float32 => {
                arrow::compute::max(max.as_primitive::<Float32Type>()).map(|v| v as f64)
            }
            DataType::Float64 => arrow::compute::max(max.as_primitive::<Float64Type>()),
            dtype => {
                unimplemented!("Highest scan start time type {dtype:?} not yet implemented")
            }
        });
        (min, max)
    }

    /// Access the saved file index which classifies the files in the archive
    pub fn file_index(&self) -> &crate::archive::FileIndex {
        self.handle.file_index()
    }

    /// Get the list of file names in the archive. This may exceed what is in the file index
    pub fn list_files(&self) -> &[String] {
        self.handle.list_files()
    }

    /// An alias for [`Self::list_files`], matching the name used by the synchronous reader
    pub fn list_all_files_in_archive(&self) -> &[String] {
        self.list_files()
    }

    /// Check if all the entries in the archive match their checksums.
    ///
    /// ## Returns
    /// - The main status flag: `Some` if all entries have a checksum recorded. `None` otherwise.
    /// - Each failed entry and its computed checksum if it was resolved, None otherwise.
    pub async fn check_archive_integrity(&self) -> io::Result<(Option<bool>, Vec<(FileEntry, Option<String>)>)> {
        self.handle.check_archive_integrity().await
    }

    /// Open a file stream by it's name
    pub fn open_stream(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<<T as AsyncArchiveSource>::File, io::Error>> {
        self.handle.open_stream(name)
    }

    /// Open a [`ParquetRecordBatchStreamBuilder`] by it's name
    pub async fn open_parquet(
        &self,
        name: &str,
    ) -> Result<ParquetRecordBatchStreamBuilder<<T as AsyncArchiveSource>::File>, io::Error> {
        let stream = self.handle.open_stream(name).await?;
        let builder = parquet::arrow::async_reader::ParquetRecordBatchStreamBuilder::new(stream)
            .await
            .map_err(|e| io::Error::other(e))?;
        Ok(builder)
    }

    /// Open a [`ParquetRecordBatchStreamBuilder`] by it's [`EntityType`] and [`DataKind`]
    pub async fn open_parquet_entry(
        &self,
        entity_type: &EntityType,
        data_kind: &DataKind,
    ) -> Result<ParquetRecordBatchStreamBuilder<<T as AsyncArchiveSource>::File>, io::Error> {
        self.handle.read_entry(entity_type, data_kind).await
    }
}

pub type AsyncMzPeakReader =
    AsyncMzPeakReaderType<AsyncZipArchiveSource, CentroidPeak, DeconvolutedPeak>;

#[cfg(test)]
mod test {
    use object_store::local::LocalFileSystem;

    use super::*;

    #[tokio::test]
    async fn test_url() -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let mut handle =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from("small.mzpeak"))
                .await?;
        let _spec = handle.get_spectrum(0).await.unwrap();
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::chunked("small.chunked.mzpeak")]
    #[case::numpress("small.numpress.mzpeak")]
    async fn test_read_spectrum(#[case] path: &str) -> io::Result<()> {
        use mzdata::spectrum::SignalContinuity;

        let store = LocalFileSystem::new_with_prefix(".")?;
        let mut reader =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from(path)).await?;
        let descr = reader.get_spectrum(0).await.unwrap();
        assert_eq!(descr.index(), 0);
        assert_eq!(descr.signal_continuity(), SignalContinuity::Profile);
        let arr = descr.raw_arrays().and_then(|a| a.mzs().ok()).unwrap();
        assert_eq!(arr.len(), 13589);
        if descr.ms_level() > 1 {
            assert_eq!(descr.precursor_iter().count(), 1);
            assert_eq!(descr.precursor().unwrap().ions.len(), 1);
        }
        let descr = reader.get_spectrum(5).await.unwrap();
        assert_eq!(descr.index(), 5);
        assert_eq!(descr.peaks().len(), 650);
        if descr.ms_level() > 1 {
            assert_eq!(descr.precursor_iter().count(), 1);
            assert_eq!(descr.precursor().unwrap().ions.len(), 1);
        }
        let descr = reader.get_spectrum(25).await.unwrap();
        assert_eq!(descr.index(), 25);
        assert_eq!(descr.peaks().len(), 789);
        if descr.ms_level() > 1 {
            assert_eq!(descr.precursor_iter().count(), 1);
            assert_eq!(descr.precursor().unwrap().ions.len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::chunked("small.chunked.mzpeak")]
    #[case::numpress("small.numpress.mzpeak")]
    async fn test_integrity_check(#[case] path: &str) -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let reader =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from(path)).await?;
        let (state, failed) = reader.check_archive_integrity().await?;
        assert!(state.unwrap(), "Overall validation status failed: {failed:?}");
        assert!(failed.is_empty(), "Failed file list is not empty: {failed:?}");
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::packed_chunks("small.chunked.mzpeak")]
    async fn test_load_all_metadata(#[case] path: &str) -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let reader =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from(path)).await?;

        let out = reader.load_all_spectrum_metadata_impl().await?;
        assert_eq!(out.len(), 48);
        assert!(out.iter().any(|p| !p.precursor.is_empty()));
        let mut decoder = TimeIndexDecoder::new(
            SimpleInterval::new(0.0, 1.0),
            Some(SimpleInterval::new(0, 1)),
        );
        decoder.from_descriptions(&out);
        let (time_index, mask) = decoder.finish();
        assert!(time_index.len() > 5);
        assert!((mask.index_range.end - mask.index_range.start) > 5);
        assert!(mask.sparse_includes.is_some());
        Ok(())
    }

    async fn open(path: &str) -> io::Result<AsyncMzPeakReader> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from(path)).await
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::packed_chunks("small.chunked.mzpeak")]
    async fn test_tic(#[case] path: &str) -> io::Result<()> {
        let mut reader = open(path).await?;
        let tic = reader.encoded_tic().await?;
        assert_eq!(tic.index(), 0);
        assert_eq!(tic.time()?.len(), 48);

        let tic = reader.get_chromatogram_by_index_or_encoded(0).await.unwrap();
        assert_eq!(tic.index(), 0);
        assert_eq!(tic.time()?.len(), 48);

        let tic = reader.get_chromatogram_by_id_or_encoded("TIC").await.unwrap();
        assert_eq!(tic.index(), 0);
        assert_eq!(tic.time()?.len(), 48);

        let bpc = reader.encoded_bpc().await?;
        assert_eq!(bpc.index(), 1);
        assert_eq!(bpc.time()?.len(), 48);
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::packed_chunks("small.chunked.mzpeak")]
    async fn test_chromatogram_metadata_cache(#[case] path: &str) -> io::Result<()> {
        let mut reader = open(path).await?;
        let expected = reader.load_all_chromatgram_metadata_impl().await?;
        assert_eq!(expected.len(), 1);
        assert_eq!(reader.len_chromatograms(), 1);
        assert_eq!(reader.count_chromatograms().await, 1);
        assert!(reader.chromatogram_metadata_cache.is_none());

        // Point lookups fill the cache and agree with the uncached decoder
        let first = reader.get_chromatogram_metadata(0).await?.unwrap();
        assert_eq!(first.id, expected[0].id);
        assert_eq!(first.index, expected[0].index);
        assert_eq!(
            reader.chromatogram_metadata_cache.as_ref().map(|v| v.len()),
            Some(expected.len())
        );
        assert!(
            reader
                .get_chromatogram_metadata(expected.len() as u64)
                .await?
                .is_none()
        );

        // Bulk access shares the cache rather than rebuilding it
        let all = reader.load_all_chromatogram_metadata().await?.unwrap();
        assert!(Arc::ptr_eq(
            &all,
            reader.chromatogram_metadata_cache.as_ref().unwrap()
        ));

        let by_id = reader.get_chromatogram_by_id(&expected[0].id).await.unwrap();
        assert_eq!(by_id.index(), expected[0].index);
        assert_eq!(by_id.time()?.len(), 48);
        assert!(
            reader
                .get_chromatogram_by_id("no such chromatogram")
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_wavelength_absent() -> io::Result<()> {
        let mut reader = open("small.mzpeak").await?;
        assert_eq!(reader.len_wavelength_spectra(), 0);
        assert!(reader.get_wavelength_spectrum_metadata(0).await?.is_none());
        let all = reader.load_all_wavelength_spectrum_metadata().await?.unwrap();
        assert!(all.is_empty());
        assert!(reader.wavelength_spectrum_metadata_cache.is_some());
        assert!(reader.get_wavelength_spectrum(0).await.is_none());
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_wavelength_matches_sync() -> io::Result<()> {
        let mut reader = open("has_uv.mzpeak").await?;
        let mut sync_reader = crate::reader::MzPeakReader::new("has_uv.mzpeak")?;

        let expected = sync_reader.load_all_wavelength_spectrum_metadata()?.unwrap().to_vec();
        assert!(!expected.is_empty());
        assert_eq!(reader.len_wavelength_spectra(), expected.len());

        let first = reader.get_wavelength_spectrum_metadata(0).await?.unwrap();
        assert_eq!(first.id, expected[0].id);

        let all = reader.load_all_wavelength_spectrum_metadata().await?.unwrap();
        assert_eq!(all.len(), expected.len());
        let last = expected.len() - 1;
        assert_eq!(all[last].id, expected[last].id);
        assert!(
            reader
                .get_wavelength_spectrum_metadata(expected.len() as u64)
                .await?
                .is_none()
        );

        for i in [0, last] {
            let spec = reader.get_wavelength_spectrum(i).await.unwrap();
            let expected_spec = sync_reader.get_wavelength_spectrum(i).unwrap();
            assert_eq!(spec.id(), expected_spec.id());
            let arrays = spec.raw_arrays().unwrap();
            let expected_arrays = expected_spec.raw_arrays().unwrap();
            assert_eq!(arrays.len(), expected_arrays.len());
            for (name, array) in arrays.iter() {
                assert_eq!(
                    array.data_len()?,
                    expected_arrays.get(name).unwrap().data_len()?
                );
            }
        }

        let by_id = reader
            .get_wavelength_spectrum_by_id(&expected[last].id)
            .await
            .unwrap();
        assert_eq!(by_id.id(), expected[last].id);

        let streamed: Vec<_> = reader.wavelength_spectra_stream().await?.collect().await;
        assert_eq!(streamed.len(), expected.len());
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed("small.mzpeak")]
    #[case::packed_chunks("small.chunked.mzpeak")]
    async fn test_matches_sync_reader(#[case] path: &str) -> io::Result<()> {
        let mut reader = open(path).await?;
        let mut sync_reader = crate::reader::MzPeakReader::new(path)?;
        assert_eq!(reader.len(), sync_reader.len());

        for i in 0..reader.len() {
            let spec = reader.get_spectrum(i).await.unwrap();
            let expected = sync_reader.get_spectrum(i).unwrap();
            assert_eq!(spec.index(), expected.index());
            assert_eq!(spec.id(), expected.id());
            assert_eq!(spec.peaks().len(), expected.peaks().len());
        }

        let (min, max) = reader.observed_time_range().await;
        let (expected_min, expected_max) = sync_reader.observed_time_range();
        assert_eq!(min, expected_min);
        assert_eq!(max, expected_max);
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_get_spectra_batch() -> io::Result<()> {
        let mut reader = open("small.mzpeak").await?;
        let requested = [25, 3, 3, 10, 0];
        let batch = reader.get_spectra_batch(requested).await.unwrap();
        assert_eq!(batch.len(), requested.len());
        for (spec, i) in batch.iter().zip(requested) {
            assert_eq!(spec.index(), i);
        }
        assert!(reader.get_spectra_batch([0, 10_000]).await.is_none());
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_spectra_stream() -> io::Result<()> {
        let mut reader = open("small.mzpeak").await?;
        reader.set_detail_level(DetailLevel::MetadataOnly);
        let n = reader.spectra_stream().await.count().await;
        assert_eq!(n, 48);
        assert!(reader.spectrum_metadata_cache.is_some());
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_signal_preference() -> io::Result<()> {
        let mut reader = open("small.mzpeak").await?;
        assert!(matches!(
            reader.prefer_spectra_peaks(),
            SignalLoadingPreference::Profiles
        ));
        // Every preference must still produce a readable spectrum, and resizing the cache must not break reads
        for pref in [
            SignalLoadingPreference::Profiles,
            SignalLoadingPreference::Centroids,
            SignalLoadingPreference::ProfilesAndCentroids,
        ] {
            reader.set_prefer_spectra_peaks(pref);
            assert_eq!(
                reader.prefer_spectra_peaks().profiles(),
                pref.profiles()
            );
            reader.set_spectrum_row_group_cache_size(2);
            let spec = reader.get_spectrum(5).await.unwrap();
            assert_eq!(spec.index(), 5);
        }
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    #[case::packed_chunks("small.chunked.mzpeak")]
    async fn test_read_peaks(#[case] path: &str) -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let mut reader =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from(path)).await?;

        let peaks = reader.get_spectrum_peaks_for(1).await?.unwrap();
        assert!(peaks.len() > 0);
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    #[rstest::rstest]
    async fn test_eic() -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let mut reader =
            AsyncMzPeakReader::from_store_path(Arc::new(store), ObjectPath::from("small.mzpeak"))
                .await?;
        let (mut it, _time_index) = reader
            .extract_signal(0.3..0.4, Some((800.0..820.0).into()), None, None)
            .await?;

        let mut k = 0;
        while let Some(batch) = it.next().await.transpose().unwrap() {
            assert_eq!(batch.column(0).as_struct().num_columns(), 3);
            assert!(batch.num_rows() > 0);
            k += batch.num_rows();
        }
        assert!(k > 0);
        // Drops null points
        assert_eq!(k, 563);
        drop(it);

        let (mut it, _) = reader
            .query_peaks(
                0.3..0.4,
                Some((800.0..820.0).into()),
                None,
                Some((2u8..10).into()),
            )
            .await?;
        k = 0;
        while let Some(batch) = it.next().await.transpose().unwrap() {
            assert_eq!(batch.column(0).as_struct().num_columns(), 3);
            assert!(batch.num_rows() > 0);
            k += batch.num_rows();
        }
        assert!(k > 0);
        // All MSn spectra are centroids, no null padding
        assert_eq!(k, 96);
        Ok(())
    }

    #[tokio::test]
    #[test_log::test]
    async fn test_eic_chunked() -> io::Result<()> {
        let store = LocalFileSystem::new_with_prefix(".")?;
        let mut reader = AsyncMzPeakReader::from_store_path(
            Arc::new(store),
            ObjectPath::from("small.chunked.mzpeak"),
        )
        .await?;

        let k_models_defined = reader
            .metadata
            .spectra
            .mz_model_deltas
            .iter()
            .filter(|v| v.is_some())
            .count();
        assert!(k_models_defined > 0);

        let (mut it, _time_index) = reader
            .extract_signal(0.3..0.4, Some((800.0..820.0).into()), None, None)
            .await?;

        let mut k = 0;
        while let Some(batch) = it.next().await.transpose().unwrap() {
            assert_eq!(batch.column(0).as_struct().num_columns(), 3);
            assert!(batch.num_rows() > 0);
            k += batch.num_rows();
            let root = batch.column(0).as_struct();
            let names = root.column_names();
            assert_eq!(names, ["spectrum_index", "mz", "intensity"]);
        }
        assert!(k > 0);
        // Does not drop null points
        assert_eq!(k, 689);
        drop(it);

        let (mut it, _) = reader
            .query_peaks(
                0.3..0.4,
                Some((800.0..820.0).into()),
                None,
                Some((2u8..10).into()),
            )
            .await?;
        k = 0;
        while let Some(batch) = it.next().await.transpose().unwrap() {
            assert_eq!(batch.column(0).as_struct().num_columns(), 3);
            assert!(batch.num_rows() > 0);
            k += batch.num_rows();
        }
        assert!(k > 0);
        // All MSn spectra are centroids, no null padding
        assert_eq!(k, 96);
        drop(it);

        let (mut it, _time_index) = reader
            .query_peaks(0.3..0.4, Some((800.0..820.0).into()), None, None)
            .await?;

        k = 0;
        while let Some(batch) = it.next().await.transpose().unwrap() {
            assert_eq!(batch.column(0).as_struct().num_columns(), 3);
            assert!(batch.num_rows() > 0);
            k += batch.num_rows();
        }
        assert!(k > 0);
        assert_eq!(k, 189);
        Ok(())
    }
}
