use std::collections::HashMap;
use std::io::{self, prelude::*};

use mzdata::spectrum::RefPeakDataLevel;
use mzpeaks::{CentroidLike, CoordinateLike, DeconvolutedCentroidLike, MZ};
use parquet::{arrow::ArrowWriter, file::metadata::KeyValue};

use mzdata::spectrum::ArrayType;

use crate::{
    ToMzPeakDataSeries,
    buffer_descriptors::{ArrayIndex, ArrayIndexEntry},
    peak_series::array_map_to_schema_arrays_and_excess,
    writer::{ArrayBufferWriter, ArrayBufferWriterVariants, base::EntryMetadataDerivedFromData},
};

// mzML2mzPeak fix: data-derived sorting_rank
// Demote the m/z (MZArray) column to `sorting_rank: None` when the observed primary axis
// was NOT non-decreasing across every spectrum. `ArrayIndex.entries` is private with no
// `get_mut`, so we clone entries, null the MZArray entry's pub `sorting_rank`, and rebuild
// via `ArrayIndex::new(prefix, HashMap)`. Locate the column by `ArrayType::MZArray` identity
// (not column order). If `mz_nondecreasing` holds, the index is returned unchanged.
pub(crate) fn demote_mz_if_unsorted(index: ArrayIndex, mz_nondecreasing: bool) -> ArrayIndex {
    if mz_nondecreasing {
        return index;
    }
    let prefix = index.prefix.clone();
    let mut map: HashMap<ArrayType, ArrayIndexEntry> = HashMap::new();
    for entry in index.iter() {
        let mut cloned = entry.clone();
        if cloned.array_type == ArrayType::MZArray {
            cloned.sorting_rank = None;
        }
        map.insert(cloned.array_type.clone(), cloned);
    }
    ArrayIndex::new(prefix, map)
}

// mzML2mzPeak fix: data-derived sorting_rank
// AND-accumulate whether a peak slice's m/z is internally non-decreasing. Empty/single-point
// lists leave the flag unchanged (treated as sorted). Intra-spectrum check; resets per call.
pub(crate) fn slice_mz_nondecreasing<P: CoordinateLike<MZ>>(peaks: &[P]) -> bool {
    nondecreasing_by(peaks, |p| CoordinateLike::<MZ>::coordinate(p))
}

fn nondecreasing_by<P, F: Fn(&P) -> f64>(peaks: &[P], coord: F) -> bool {
    let mut prev: Option<f64> = None;
    for p in peaks {
        let cur = coord(p);
        if let Some(prev) = prev {
            if cur < prev {
                return false;
            }
        }
        prev = Some(cur);
    }
    true
}

/// A small helper for writing peak list data to another stream with very narrow options.
pub struct MiniPeakWriterType<W: Write + Send + Seek> {
    writer: ArrowWriter<W>,
    buffers: ArrayBufferWriterVariants,
    buffer_size: usize,
    n_points: u64,
    n_entries: u64,
    // mzML2mzPeak fix: data-derived sorting_rank.
    // AND-accumulated across every spectrum's primary m/z; demotes the MZArray column
    // at finish() when any spectrum's m/z was not non-decreasing.
    mz_nondecreasing: bool,
}

impl<W: Write + Send + Seek> MiniPeakWriterType<W> {
    pub fn new(
        writer: ArrowWriter<W>,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
    ) -> Self {
        // mzML2mzPeak fix: data-derived sorting_rank.
        // The eager `spectrum_array_index` KV emission was REMOVED from `new`: emitting
        // sorting_rank: 0 before any peak is observed is precisely the bug. The KV is now
        // emitted in `finish()` once the per-file monotonicity flag is known.
        Self {
            writer,
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
            mz_nondecreasing: true,
        }
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
                // mzML2mzPeak fix: data-derived sorting_rank.
                // Fold this spectrum's centroid m/z monotonicity into the per-file accumulator.
                self.mz_nondecreasing &= slice_mz_nondecreasing(peaks.as_slice());
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Deconvoluted(peaks) => {
                // mzML2mzPeak fix: data-derived sorting_rank.
                // Deconvoluted peaks are mass-indexed; the writer's generic `D` bound does not
                // carry `MZLocated`, so the m/z is not observable here without widening the whole
                // writer API. Deconvoluted output is mass-sorted (m/z follows for fixed charge) —
                // leave the accumulator unchanged (treat as sorted) rather than over-demote. The
                // centroid-imaging case this fix targets flows through the Centroid arm above.
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Missing => unimplemented!(),
            RefPeakDataLevel::RawData(arrays) => {
                // mzML2mzPeak fix: data-derived sorting_rank.
                // Raw-array centroid path: fold the primary m/z array's sortedness.
                self.mz_nondecreasing &= arrays.mzs().map(|v| v.is_sorted()).unwrap_or(true);
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
        // mzML2mzPeak fix: data-derived sorting_rank.
        // Relocated emission: emit `spectrum_array_index` HERE (not in `new`), demoting the
        // MZArray column's sorting_rank to null if any spectrum's m/z was non-monotonic.
        let spectrum_array_index: ArrayIndex = self.buffers.as_array_index();
        let spectrum_array_index =
            demote_mz_if_unsorted(spectrum_array_index, self.mz_nondecreasing);
        self.append_key_value_metadata(
            "spectrum_array_index".to_string(),
            Some(spectrum_array_index.to_json()),
        );
        self.append_key_value_metadata("spectrum_count", Some(self.n_entries.to_string()));
        self.append_key_value_metadata(
            "spectrum_data_point_count",
            Some(self.n_points.to_string()),
        );
        self.flush()?;
        self.writer.into_inner()
    }

    pub fn n_points(&self) -> u64 {
        self.n_points
    }

    pub fn n_entries(&self) -> u64 {
        self.n_entries
    }
}
