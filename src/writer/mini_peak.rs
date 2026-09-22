use std::io::{self, prelude::*};

use arrow::array::Array;
use mzdata::spectrum::RefPeakDataLevel;
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike};
use parquet::{arrow::ArrowWriter, file::metadata::KeyValue};

use crate::{
    ToMzPeakDataSeries, chunk_series::{ArrowArrayChunk, ChunkingStrategy}, peak_series::{ArrayIndex, array_map_to_schema_arrays_and_excess}, writer::{ArrayBufferWriter, ArrayBufferWriterVariants, base::EntryMetadataDerivedFromData},
};

/// A small helper for writing peak list data to another stream with very narrow options.
pub struct MiniPeakWriterType<W: Write + Send + Seek> {
    writer: ArrowWriter<W>,
    buffers: ArrayBufferWriterVariants,
    buffer_size: usize,
    n_points: u64,
    n_entries: u64,
}

impl<W: Write + Send + Seek> MiniPeakWriterType<W> {
    pub fn new(
        writer: ArrowWriter<W>,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
    ) -> Self {
        let mut this = Self {
            writer,
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
        };
        let spectrum_array_index: ArrayIndex = this.buffers.as_array_index();
        this.append_key_value_metadata(
            "spectrum_array_index".to_string(),
            Some(spectrum_array_index.to_json()),
        );
        this
    }

    pub fn buffers(&self) -> &ArrayBufferWriterVariants {
        &self.buffers
    }

    pub fn grid_policies(&self) -> Option<&std::collections::HashMap<mzdata::spectrum::ArrayType, crate::grid::GridPolicy>> {
        self.buffers().grid_policies()
    }

    pub fn grid_policies_mut(&mut self) -> Option<&mut std::collections::HashMap<mzdata::spectrum::ArrayType, crate::grid::GridPolicy>> {
        self.buffers.grid_policies_mut()
    }

    pub fn clear_current_grids(&mut self) {
        self.buffers.clear_current_grids();
    }

    pub fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.buffers.chunking_strategy()
    }

    pub fn append_key_value_metadata(
        &mut self,
        key: impl Into<String>,
        value: impl Into<Option<String>>,
    ) {
        self.writer
            .append_key_value_metadata(KeyValue::new(key.into(), value));
    }

    pub fn write_peaks<
        C: CentroidLike + ToMzPeakDataSeries,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
    >(
        &mut self,
        spectrum_count: u64,
        spectrum_time: Option<f32>,
        peaks: RefPeakDataLevel<C, D>,
    ) -> io::Result<EntryMetadataDerivedFromData> {
        let spectrum_time = if self.buffers.include_time() {
            spectrum_time
        } else {
            None
        };
        let n = peaks.len();
        log::trace!("Writing {n} peaks for {spectrum_count}");
        let (aux, n_peaks) = match peaks {
            RefPeakDataLevel::Centroid(peaks) => {
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Deconvoluted(peaks) => {
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Missing => unimplemented!(),
            RefPeakDataLevel::RawData(arrays) => {
                if let Some(chunk_encoding) = self.use_chunked_encoding().cloned() {
                    let buffer = &mut self.buffers;
                    let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                        spectrum_count,
                        spectrum_time,
                        buffer.buffer_context(),
                        arrays,
                        &chunk_encoding,
                        buffer.overrides(),
                        buffer.drop_zero_intensity(),
                        buffer.nullify_zero_intensity(),
                        buffer.fields(),
                        buffer.grid_policies(),
                    )?;

                    if let Some(chunks) = chunks {
                        let size = chunks.len();
                        let (fields, arrays, _nulls) = chunks.into_parts();
                        buffer.add_arrays(fields, arrays, size, false);
                    }

                    (auxiliary_arrays, n_pts)
                } else {
                    let (fields, cols, aux) = array_map_to_schema_arrays_and_excess(
                        crate::BufferContext::Spectrum,
                        arrays,
                        n,
                        spectrum_count,
                        spectrum_time,
                        Some(self.buffers.fields()),
                        self.buffers.overrides(),
                    )?;
                    let pts_written = self.buffers.add_arrays(fields, cols, n, false);
                    (aux, pts_written)
                }
            }
        };

        self.n_points += n as u64;
        self.n_entries += 1;

        if self.buffers.len() >= self.buffer_size {
            self.flush()?;
        }
        Ok(EntryMetadataDerivedFromData::new(
            None,
            Some(aux),
            None,
            Some(n_peaks),
        ))
    }

    pub fn flush(&mut self) -> io::Result<()> {
        for batch in self.buffers.drain() {
            self.writer.write(&batch)?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<W, parquet::errors::ParquetError> {
        self.append_key_value_metadata("spectrum_count", Some(self.n_entries.to_string()));
        self.append_key_value_metadata(
            "spectrum_data_point_count",
            Some(self.n_points.to_string()),
        );
        self.flush()?;
        self.writer.into_inner()
    }

    pub fn point_count(&self) -> u64 {
        self.n_points
    }

    pub fn n_entries(&self) -> u64 {
        self.n_entries
    }
}
