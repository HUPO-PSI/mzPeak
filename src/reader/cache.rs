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
use crate::reader::metadata::ReaderFacetMetadataLike;
use crate::reader::point::{PointDataArrayReader, PointDataReader};
use crate::reader::{MzPeakReaderTypeOfSource, MzPeakSpectrumFacet};

use super::chunk::ChunkDataCacheBlock;
use super::point::PointDataCacheBlock;

#[cfg(feature = "async")]
use crate::{archive::AsyncArchiveSource, reader::{AsyncMzPeakReaderType, point::AsyncPointDataReader, chunk::AsyncSpectrumChunkReader}};


// This value can be made larger for a modest (<10%) improvement in linear reading performance
// but the trade-off in memory load makes this impractical, especially if spectra are very,
// very dense.
pub(crate) const CHUNK_CACHE_BLOCK_SIZE: u64 = 100;

/// A cache block for (part of) a row group. It represents a completely decoded block of data that can be used
/// to fulfill multiple read requests without repeatedly going back to the disk and re-reading, decompressing
/// and decoding the same data repeatedly for the `n+1`th item.
pub enum DataCacheBlock {
    /// A point layout cache block
    Point(PointDataCacheBlock),
    /// A chunked layout cache block
    Chunk(ChunkDataCacheBlock),
}

impl From<ChunkDataCacheBlock> for DataCacheBlock {
    fn from(v: ChunkDataCacheBlock) -> Self {
        Self::Chunk(v)
    }
}

impl From<PointDataCacheBlock> for DataCacheBlock {
    fn from(v: PointDataCacheBlock) -> Self {
        Self::Point(v)
    }
}

impl DataCacheBlock {

    /// Get the last index that was queried in this block which might hint to which half to search for
    /// another index.
    pub fn last_query_index(&self) -> Option<u64> {
        match self {
            DataCacheBlock::Point(data_point_cache) => data_point_cache.last_query_index,
            DataCacheBlock::Chunk(data_chunk_cache) => data_chunk_cache.last_query_index,
        }
    }

    /// Get the range of entry indices that are covered by this cache block.
    ///
    /// If the cache block is empty, this may not exist.
    pub fn index_range(&self) -> Option<SimpleInterval<u64>> {
        match self {
            DataCacheBlock::Point(data_point_cache) => data_point_cache.index_range(),
            DataCacheBlock::Chunk(data_chunk_cache) => Some(data_chunk_cache.index_range),
        }
    }

    /// Get the segment of this cache block corresponding to `index` and decode it into a [`BinaryArrayMap`]
    pub fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<Option<BinaryArrayMap>> {
        if self.contains(row_group_index, index) {
            match self {
                DataCacheBlock::Point(spectrum_data_point_cache) => {
                    spectrum_data_point_cache.slice_to_arrays_of(index, delta_model)
                }
                DataCacheBlock::Chunk(spectrum_data_chunk_cache) => {
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

    /// Test if the cache block covers the requested row group and entry index
    pub fn contains(&self, row_group_index: usize, index: u64) -> bool {
        match self {
            DataCacheBlock::Point(spectrum_data_point_cache) => {
                spectrum_data_point_cache.row_group_index == row_group_index
            }
            DataCacheBlock::Chunk(spectrum_data_chunk_cache) => {
                spectrum_data_chunk_cache.index_range.contains(&index)
            }
        }
    }

    /// Load a cache block for the mass spectrum data facet
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
            let cache = builder.load_cache_block_into(row_group_index, reader.metadata.spectra.array_indices.clone())?;
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

    #[cfg(feature = "async")]
    /// Load a cache block for the mass spectrum data facet asynchronously
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
            let cache = builder.load_cache_block_into(row_group_index, reader.metadata.spectra.array_indices.clone()).await?;
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

    /// Load a cache block for a [`MzPeakSpectrumFacet`]
    pub fn load_data_for_facet<T: MzPeakSpectrumFacet>(
        reader: &T,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<Option<Self>> {
        if let Some(_query_index) = reader.metadata_index().data_index().as_point() {
            let builder = PointDataReader(reader.data_reader()?, reader.buffer_context());
            let rg = builder.load_cache_block(reader.data_reader()?, row_group_index)?;
            let cache = PointDataCacheBlock::new(
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

/// A basic cache frontend that holds at most one cache block and is backed by an [`Option<DataCacheBlock>`]
#[derive(Default)]
pub(crate) struct OneCache(Option<DataCacheBlock>);

#[allow(unused)]
impl OneCache {
    pub(crate) fn new(data_cache: Option<DataCacheBlock>) -> Self {
        Self(data_cache)
    }

    pub(crate) fn as_mut(&mut self) -> Option<&mut DataCacheBlock> {
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
    ) -> io::Result<Option<BinaryArrayMap>> {
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
        if let Some(block) = DataCacheBlock::load_data_for(reader, row_group_index, index)? {
            self.0.replace(block);
            Ok(())
        } else {
            Ok(())
        }
    }

    pub(crate) fn accept(&mut self, block: DataCacheBlock) {
        if let Some(evicted) = self.0.replace(block) {
            log::debug!("Evicting {:?}", evicted.last_query_index());
        }
    }
}

/// An LRU container for multiple [`DataCacheBlock`]
pub struct CacheBuffer {
    blocks: VecDeque<DataCacheBlock>,
    max_size: usize,
}

/// The default constructor uses a
impl Default for CacheBuffer {
    fn default() -> Self {
        Self::with_max_size(3)
    }
}

#[allow(unused)]
impl CacheBuffer {
    /// Construct a [`CacheBuffer`] from parts
    pub(crate) const fn new(blocks: VecDeque<DataCacheBlock>, max_size: usize) -> Self {
        Self { blocks, max_size }
    }

    /// Construct a new, empty [`CacheBuffer`] with the requested capacity. It will not hold
    /// more than `max_size` cache blocks unless overridden with [`CacheBuffer::set_max_size`].
    pub fn with_max_size(max_size: usize) -> Self {
        Self::new(VecDeque::with_capacity(max_size), max_size)
    }

    /// Test if the cache contains a [`DataCacheBlock`] covering a specific row group and entry index
    pub fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.blocks
            .iter()
            .any(|b| b.contains(row_group_index, index))
    }

    /// Apply the LRU ordering update on a cache block at the specified internal index.
    ///
    /// ## Note
    /// After calling this method, `i` no longer points to this block, it will be located
    /// at index `0`, the front of the queue.
    fn move_to_front(&mut self, i: usize) {
        // if this is the first block, no-op
        if (i != 0) {
            let block = self.blocks.remove(i).unwrap();
            self.blocks.push_front(block);
        }
    }

    /// Get a mutable reference to the [`DataCacheBlock`] coveriung the specified row group and entry index
    /// if the cache has one.
    ///
    /// This will count as "using" the cache block, moving it to the front of the LRU queue
    pub fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCacheBlock> {
        if let Some(i) = self.blocks.iter().position(|b| b.contains(row_group_index, index)) {
            self.move_to_front(i);
            self.blocks.front_mut()
        } else {
            None
        }
    }

    /// Get the segment of this cache block corresponding to `index` and decode it into a [`BinaryArrayMap`].
    ///
    /// A wrapper around [`DataCacheBlock::slice_to_arrays_of`]
    pub fn slice_to_arrays_of(
        &mut self,
        row_group_index: usize,
        index: u64,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> io::Result<Option<BinaryArrayMap>> {
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

    /// Load a cache block for the mass spectrum data facet. A wrapper around [`DataCacheBlock::load_data_for`]
    pub fn load_data_for<
        T: ArchiveSource,
        C: CentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
        D: DeconvolutedCentroidLike + BuildFromArrayMap + BuildArrayMapFrom,
    >(
        &mut self,
        reader: &MzPeakReaderTypeOfSource<T, C, D>,
        row_group_index: usize,
        index: u64,
    ) -> io::Result<()> {
        if let Some(block) = DataCacheBlock::load_data_for(reader, row_group_index, index)? {
            self.accept(block);
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Apply the LRU capacity restriction
    fn evict(&mut self) {
        while self.blocks.len() >= self.max_size {
            if let Some(evicted) = self.blocks.pop_back() {
                log::trace!("Evicting {:?}", evicted.last_query_index())
            }
        }
    }

    /// Receive a new cache block and apply LRU capacity restriction before adding it
    /// to the cache.
    pub fn accept(&mut self, block: DataCacheBlock) {
        self.evict();
        self.blocks.push_front(block);
    }

    /// Update the maximum size of the cache and apply LRU capacity restriction
    pub fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
        self.evict();
    }
}


#[allow(unused)]
pub trait DataCacheFrontend {
    fn contains(&self, row_group_index: usize, index: u64) -> bool;
    fn accept(&mut self, block: DataCacheBlock);
    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCacheBlock>;
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
    ) -> io::Result<Option<BinaryArrayMap>>;
}


impl DataCacheFrontend for OneCache {
    fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.contains(row_group_index, index)
    }

    fn accept(&mut self, block: DataCacheBlock) {
        self.accept(block);
    }

    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCacheBlock> {
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
    ) -> io::Result<Option<BinaryArrayMap>> {
        self.slice_to_arrays_of(row_group_index, index, delta_model)
    }
}

impl DataCacheFrontend for CacheBuffer {
    fn contains(&self, row_group_index: usize, index: u64) -> bool {
        self.contains(row_group_index, index)
    }

    fn accept(&mut self, block: DataCacheBlock) {
        self.accept(block);
    }

    fn get_mut(&mut self, row_group_index: usize, index: u64) -> Option<&mut DataCacheBlock> {
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
    ) -> io::Result<Option<BinaryArrayMap>> {
        self.slice_to_arrays_of(row_group_index, index, delta_model)
    }
}