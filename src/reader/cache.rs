use std::collections::VecDeque;
use std::io;

use mzdata::prelude::*;
use mzdata::spectrum::BinaryArrayMap;
use mzpeaks::coordinate::SimpleInterval;

use crate::BufferContext;
use crate::archive::ArchiveSource;
use crate::filter::RegressionDeltaModel;
use crate::reader::chunk::ChunkDataReader;
use crate::reader::index::SpectrumMetadataIndexLike;
use crate::reader::metadata::SpectrumMetadataLike;
use crate::reader::point::{PointDataArrayReader, PointDataReader};
use crate::reader::{MzPeakReaderTypeOfSource, MzPeakSpectrumFacet};

use super::chunk::DataChunkCache;
use super::point::DataPointCache;

#[cfg(feature = "async")]
use crate::{archive::AsyncArchiveSource, reader::{AsyncMzPeakReaderType, point::AsyncPointDataReader, chunk::AsyncSpectrumChunkReader}};


// This value can be made larger for a modest (<10%) improvement in linear reading performance
// but the trade-off in memory load makes this impractical, especially if spectra are very,
// very dense.
pub(crate) const CHUNK_CACHE_BLOCK_SIZE: u64 = 100;

pub(crate) enum DataCache {
    Point(DataPointCache),
    Chunk(DataChunkCache),
}

impl DataCache {
    pub fn last_query_index(&self) -> Option<u64> {
        match self {
            DataCache::Point(data_point_cache) => data_point_cache.last_query_index,
            DataCache::Chunk(data_chunk_cache) => data_chunk_cache.last_query_index,
        }
    }

    pub fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap> {
        if self.contains(row_group_index, index) {
            match self {
                DataCache::Point(spectrum_data_point_cache) => {
                    spectrum_data_point_cache.slice_to_arrays_of(index, delta_model)
                }
                DataCache::Chunk(spectrum_data_chunk_cache) => {
                    spectrum_data_chunk_cache.slice_to_arrays_of(index, delta_model)
                }
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Entries not found for {row_group_index}:{index}"),
            ))
        }
    }

    pub fn contains(&self, row_group_index: usize, index: u64) -> bool {
        match self {
            DataCache::Point(spectrum_data_point_cache) => {
                spectrum_data_point_cache.row_group_index == row_group_index
            }
            DataCache::Chunk(spectrum_data_chunk_cache) => {
                spectrum_data_chunk_cache.index_range.contains(&index)
            }
        }
    }

    pub fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<Option<Self>> {
        if let Some(_query_index) = reader.query_indices.spectrum.data_index.as_point() {
            let builder = reader.handle.spectrum_data()?;
            let builder = PointDataReader::new(builder, BufferContext::Spectrum);
            let rg = builder.load_cache_block_into(row_group_index)?;
            let cache = DataPointCache::new(
                rg,
                reader.metadata.spectra.array_indices.clone(),
                row_group_index,
                None,
                None,
                BufferContext::Spectrum,
            );

            Ok(Some(Self::Point(cache)))
        } else if let Some(query_index) = reader.query_indices.spectrum.data_index.as_chunked() {
            let builder = reader.handle.spectrum_data()?;
            let builder = ChunkDataReader::new(builder, BufferContext::Spectrum);
            let cache = builder.load_cache_block(
                SimpleInterval::new(index, index + CHUNK_CACHE_BLOCK_SIZE),
                reader.metadata.spectra.array_indices.clone(),
                query_index,
            )?;
            Ok(Some(Self::Chunk(cache)))
        } else {
            Ok(None)
        }
    }

    #[allow(unused)]
    #[cfg(feature = "async")]
    pub async fn load_data_for_async<
        T: AsyncArchiveSource + Sync + Send,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom + Sync + Send,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom + Sync + Send,
    >(
        reader: &AsyncMzPeakReaderType<T, C, D>,
        row_group_index: usize,
        spectrum_index: u64,
    ) -> io::Result<Option<Self>> {
        if reader.query_indices.spectrum.data_index.is_point() {
            let builder = reader.handle.spectra_data().await?;
            let builder = AsyncPointDataReader(builder, BufferContext::Spectrum);
            let rg = builder.load_cache_block_into(row_group_index).await?;
            let cache = DataPointCache::new(
                rg,
                reader.metadata.spectra.array_indices.clone(),
                row_group_index,
                None,
                None,
                BufferContext::Spectrum,
            );

            Ok(Some(Self::Point(cache)))
        } else if let Some(query_index) = reader.query_indices.spectrum.data_index.as_chunked() {
            let builder = reader.handle.spectra_data().await?;
            let builder = AsyncSpectrumChunkReader::new(builder);
            let cache = builder
                .load_cache_block(
                    SimpleInterval::new(spectrum_index, spectrum_index + CHUNK_CACHE_BLOCK_SIZE),
                    &reader.metadata,
                    query_index,
                )
                .await?;
            Ok(Some(Self::Chunk(cache)))
        } else {
            Ok(None)
        }
    }

    // TODO: A facet-specific cache builder. Add a facet wrapping layer that allows them to also take advantage of caching
    #[allow(unused)]
    pub fn load_data_for_facet<T: MzPeakSpectrumFacet>(
        reader: &T,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<Option<Self>> {
        if let Some(_query_index) = reader.metadata_index().data_index().as_point() {
            let builder = PointDataReader(reader.data_reader()?, reader.buffer_context());
            let rg = builder.load_cache_block(reader.data_reader()?, row_group_index)?;
            let cache = DataPointCache::new(
                rg,
                reader.metadata().array_indices().clone(),
                row_group_index,
                None,
                None,
                reader.buffer_context(),
            );

            Ok(Some(Self::Point(cache)))
        } else if let Some(query_index) = reader.metadata_index().data_index().as_chunked() {
            let builder = reader.data_reader()?;
            let builder = ChunkDataReader::new(builder, reader.buffer_context());
            let cache = builder.load_cache_block(
                SimpleInterval::new(index, index + CHUNK_CACHE_BLOCK_SIZE),
                reader.metadata().array_indices().clone(),
                query_index,
            )?;
            Ok(Some(Self::Chunk(cache)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Default)]
pub(crate) struct OneCache(Option<DataCache>);

#[allow(unused)]
impl OneCache {
    pub(crate) fn new(data_cache: Option<DataCache>) -> Self {
        Self(data_cache)
    }

    pub(crate) fn as_mut(&mut self) -> Option<&mut DataCache> {
        self.0.as_mut()
    }

    pub(crate) fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.0
            .as_ref()
            .map(|b| b.contains(row_group_index, index))
            .unwrap_or_default()
    }

    pub(crate) fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap> {
        self.0
            .as_mut()
            .map(|b| b.slice_to_arrays_of(row_group_index, index, delta_model))
            .unwrap_or_else(|| {
                Err(io::Error::other(format!(
                    "Cache block not found for {index}:{row_group_index}"
                )))
            })
    }

    pub(crate) fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()> {
        if let Some(block) = DataCache::load_data_for(reader, row_group_index, index)? {
            self.0.replace(block);
            Ok(())
        } else {
            Ok(())
        }
    }

    pub(crate) fn accept(&mut self, block: DataCache) {
        if let Some(evicted) = self.0.replace(block) {
            log::debug!("Evicting {:?}", evicted.last_query_index());
        }
    }
}

#[allow(unused)]
pub(crate) struct CacheBuffer {
    blocks: VecDeque<DataCache>,
    max_size: usize,
}

impl Default for CacheBuffer {
    fn default() -> Self {
        Self { blocks: Default::default(), max_size: 3 }
    }
}

#[allow(unused)]
impl CacheBuffer {
    pub(crate) fn new(blocks: VecDeque<DataCache>, max_size: usize) -> Self {
        Self { blocks, max_size }
    }

    pub(crate) fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.blocks
            .iter()
            .any(|b| b.contains(row_group_index, index))
    }

    fn move_to_front(&mut self, i: usize) {
        let block = self.blocks.remove(i).unwrap();
        self.blocks.push_front(block);
    }

    pub(crate) fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCache> {
        if let Some(i) = self.blocks.iter().position(|b| b.contains(row_group_index, index)) {
            self.move_to_front(i);
            self.blocks.front_mut()
        } else {
            None
        }
    }

    pub(crate) fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap> {
        for (i, b) in self.blocks.iter_mut().enumerate() {
            if b.contains(row_group_index, index) {
                let result = b.slice_to_arrays_of(row_group_index, index, delta_model)?;
                if let Some(b) = self.blocks.remove(i) {
                    self.blocks.push_front(b);
                }
                return Ok(result);
            }
        }

        Err(io::Error::other(format!(
            "Cache block not found for {index}:{row_group_index}"
        )))
    }

    pub(crate) fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()> {
        if let Some(block) = DataCache::load_data_for(reader, row_group_index, index)? {
            self.accept(block);
            Ok(())
        } else {
            Ok(())
        }
    }

    fn evict(&mut self) {
        while self.blocks.len() >= self.max_size {
            if let Some(evicted) = self.blocks.pop_back() {
                log::trace!("Evicting {:?}", evicted.last_query_index())
            }
        }
    }

    pub(crate) fn accept(&mut self, block: DataCache) {
        self.evict();
        self.blocks.push_back(block);
    }

    pub(crate) fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
        self.evict();
    }
}


#[allow(unused)]
pub trait DataCacheFrontend {
    fn contains(&self, row_group_index: usize, index: u64) -> bool;
    fn accept(&mut self, block: DataCache);
    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCache>;
    fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()>;

    fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap>;
}


impl DataCacheFrontend for OneCache {
    fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.contains(row_group_index, index)
    }

    fn accept(&mut self, block: DataCache) {
        self.accept(block);
    }

    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCache> {
        if self.contains(row_group_index, index) {
            self.as_mut()
        } else {
            None
        }
    }

    fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()> {
        self.load_data_for(reader, row_group_index, index)
    }

    fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap> {
        self.slice_to_arrays_of(row_group_index, index, delta_model)
    }
}

impl DataCacheFrontend for CacheBuffer {
    fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.contains(row_group_index, index)
    }

    fn accept(&mut self, block: DataCache) {
        self.accept(block);
    }

    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCache> {
        self.get_mut(row_group_index, index)
    }

    fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()> {
        self.load_data_for(reader, row_group_index, index)
    }

    fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<BinaryArrayMap> {
        self.slice_to_arrays_of(row_group_index, index, delta_model)
    }
}