use std::{collections::HashMap, fs, io, sync::Arc};

use arrow::{
    array::Array,
    datatypes::{Schema, SchemaRef},
};
use mzdata::{
    meta::FileMetadataConfig,
    prelude::*,
    spectrum::{
        ArrayType, BinaryArrayMap, Chromatogram, DataArray, RefPeakDataLevel, SignalContinuity,
        bindata::ArrayRetrievalError,
    },
};
use parquet::{
    arrow::{ArrowSchemaConverter, ArrowWriter, arrow_writer::ArrowWriterOptions},
    basic::{Compression, Encoding, ZstdLevel},
    encryption::encrypt::FileEncryptionProperties,
    file::{
        metadata::SortingColumn,
        properties::{
            DEFAULT_DICTIONARY_PAGE_SIZE_LIMIT, EnabledStatistics, WriterProperties, WriterVersion,
        },
    },
};

use crate::{
    BufferContext, ToMzPeakDataSeries,
    archive::{FileEntry, MzPeakArchiveType},
    chunk_series::{ArrowArrayChunk, ChunkingStrategy},
    filter::select_delta_model,
    peak_series::{INTENSITY_ARRAY, WAVELENGTH_ARRAY, array_map_to_schema_arrays_and_excess},
    spectrum::AuxiliaryArray,
    writer::{
        ArrayBufferWriter, ArrayBufferWriterVariants, ArrayBuffersBuilder, ChromatogramBuilder,
        MiniPeakWriterType, SpectrumBuilder, WavelengthSpectrumBuilder, WriteBatchConfig,
    },
};

macro_rules! implement_mz_metadata {
    () => {
        pub(crate) fn append_metadata(&mut self) {
            self.append_key_value_metadata(
                "file_description".to_string(),
                Some(
                    serde_json::to_string_pretty(&$crate::param::FileDescription::from(
                        self.mz_metadata.file_description(),
                    ))
                    .unwrap(),
                ),
            );
            let tmp: Vec<_> = self
                .mz_metadata
                .instrument_configurations()
                .values()
                .map(|v| $crate::param::InstrumentConfiguration::from(v))
                .collect();
            self.append_key_value_metadata(
                "instrument_configuration_list".to_string(),
                Some(serde_json::to_string_pretty(&tmp).unwrap()),
            );

            let tmp: Vec<_> = self
                .mz_metadata
                .data_processings()
                .iter()
                .map(|v| $crate::param::DataProcessing::from(v))
                .collect();
            self.append_key_value_metadata(
                "data_processing_method_list".to_string(),
                Some(serde_json::to_string_pretty(&tmp).unwrap()),
            );

            let tmp: Vec<_> = self
                .mz_metadata
                .softwares()
                .iter()
                .map(|v| $crate::param::Software::from(v))
                .collect();
            self.append_key_value_metadata(
                "software_list".to_string(),
                Some(serde_json::to_string_pretty(&tmp).unwrap()),
            );

            let tmp: Vec<_> = self
                .mz_metadata
                .samples()
                .iter()
                .map(|v| $crate::param::Sample::from(v))
                .collect();

            self.append_key_value_metadata(
                "sample_list".to_string(),
                Some(serde_json::to_string_pretty(&tmp).unwrap()),
            );

            let tmp: Vec<_> = self
                .mz_metadata
                .scan_settings()
                .map(|vs| {
                    vs.iter()
                        .map(|v| $crate::param::ScanSettings::from(v))
                        .collect()
                })
                .unwrap_or_default();
            self.append_key_value_metadata(
                "scan_settings_list".to_string(),
                Some(serde_json::to_string_pretty(&tmp).unwrap()),
            );

            self.append_key_value_metadata(
                "run".to_string(),
                Some(
                    serde_json::to_string_pretty(self.mz_metadata.run_description().unwrap())
                        .unwrap(),
                ),
            );
        }
    };
}

pub(crate) use implement_mz_metadata;

#[derive(Default)]
pub struct EntryMetadataDerivedFromData {
    pub mz_delta_model: Option<Vec<f64>>,
    pub auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
    pub data_point_count: Option<usize>,
    pub peak_count: Option<usize>,
}

impl From<Vec<AuxiliaryArray>> for EntryMetadataDerivedFromData {
    fn from(value: Vec<AuxiliaryArray>) -> Self {
        if value.is_empty() {
            Self::default()
        } else {
            Self::new(None, Some(value), None, None)
        }
    }
}

impl EntryMetadataDerivedFromData {
    pub fn new(
        mz_delta_model: Option<Vec<f64>>,
        auxiliary_arrays: Option<Vec<AuxiliaryArray>>,
        data_point_count: Option<usize>,
        peak_count: Option<usize>,
    ) -> Self {
        Self {
            mz_delta_model,
            auxiliary_arrays,
            data_point_count,
            peak_count,
        }
    }
}

pub struct GenericDataArrayWriter {
    data_buffers: ArrayBufferWriterVariants,
}

impl GenericDataArrayWriter {
    pub fn new(data_buffers: ArrayBufferWriterVariants) -> Self {
        Self { data_buffers }
    }

    /// Fit an [`MZDeltaModel`] instance on the provided (sparse) spectrum signal, and return the parameter
    /// buffer.
    ///
    /// If an intensity array is available, it will be used to weight the parameter estimation procedure.
    ///
    /// If no m/z array is available, `None` is returned
    fn build_delta_model(&self, binary_array_map: &BinaryArrayMap) -> Option<Vec<f64>> {
        if let Ok(mzs) = binary_array_map.mzs() {
            let delta_model = if let Ok(ints) = binary_array_map.intensities() {
                let weights: Vec<f64> =
                    ints.iter().map(|i| (*i + 1.0).ln().sqrt() as f64).collect();
                select_delta_model(&mzs, Some(&weights))
            } else {
                select_delta_model(&mzs, None)
            };
            Some(delta_model)
        } else {
            None
        }
    }

    pub fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.data_buffers.chunking_strategy()
    }

    /// Write a [`BinaryArrayMap`] to the data buffer
    pub fn write_data_arrays(
        &mut self,
        binary_array_map: &BinaryArrayMap,
        is_profile: bool,
        series_time: Option<f32>,
        series_index: u64,
    ) -> io::Result<EntryMetadataDerivedFromData> {
        let main_axis_array = binary_array_map
            .get(&self.data_buffers.buffer_context().default_sorted_array())
            .unwrap();

        let n_points = main_axis_array.data_len()?;
        let sorted = is_data_array_sorted(main_axis_array)?;

        let mut tmp_binary_array_map = BinaryArrayMap::new();
        if !sorted {
            log::warn!(
                "{} {series_index} was not sorted, sorting {n_points} values",
                self.data_buffers.buffer_context().main_struct_name()
            );
            binary_array_map.clone_into(&mut tmp_binary_array_map);
            tmp_binary_array_map
                .sort_by_array(&self.data_buffers.buffer_context().default_sorted_array())?;
        }

        let delta_model = if self.data_buffers.nullify_zero_intensity() {
            self.build_delta_model(if sorted {
                binary_array_map
            } else {
                &tmp_binary_array_map
            })
        } else {
            None
        };

        let (extra_arrays, n_points) =
            if let Some(chunk_encoding) = self.use_chunked_encoding().copied() {
                let buffer = &mut self.data_buffers;

                let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                    series_index,
                    series_time,
                    buffer.buffer_context(),
                    if sorted {
                        binary_array_map
                    } else {
                        &tmp_binary_array_map
                    },
                    chunk_encoding,
                    buffer.overrides(),
                    buffer.drop_zero_intensity(),
                    buffer.nullify_zero_intensity(),
                    buffer.fields(),
                )?;

                if let Some(chunks) = chunks {
                    let size = chunks.len();
                    let (fields, arrays, _nulls) = chunks.into_parts();
                    buffer.add_arrays(fields, arrays, size, is_profile);
                }

                (auxiliary_arrays, n_pts)
            } else {
                let buffer = &mut self.data_buffers;
                let (fields, data, auxiliary_arrays) = array_map_to_schema_arrays_and_excess(
                    buffer.buffer_context(),
                    if sorted {
                        binary_array_map
                    } else {
                        &tmp_binary_array_map
                    },
                    n_points,
                    series_index,
                    series_time,
                    Some(buffer.fields()),
                    buffer.overrides(),
                )?;

                let pts_written = buffer.add_arrays(fields, data, n_points, is_profile);
                for aux in auxiliary_arrays.iter() {
                    log::debug!("{:?} {:?} {:?}", aux.name, aux.data_type, aux.unit);
                }
                (auxiliary_arrays, pts_written)
            };

        Ok(EntryMetadataDerivedFromData::new(
            delta_model,
            Some(extra_arrays),
            Some(n_points),
            None,
        ))
    }

    /// Write a peak list to the data buffer.
    pub fn write_peaks<C: ToMzPeakDataSeries>(
        &mut self,
        peaks: &[C],
        series_index: u64,
        series_time: Option<f32>,
    ) -> Result<EntryMetadataDerivedFromData, ArrayRetrievalError> {
        let ctx = self.buffers().buffer_context();
        if let Some(encoding) = self.use_chunked_encoding().copied() {
            let arrays = C::as_arrays(peaks);
            let buffer_ref = &mut self.data_buffers;

            let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                series_index,
                series_time,
                ctx,
                &arrays,
                encoding,
                buffer_ref.overrides(),
                buffer_ref.drop_zero_intensity(),
                buffer_ref.nullify_zero_intensity(),
                buffer_ref.fields(),
            )?;
            if let Some(chunks) = chunks {
                let size = chunks.len();
                let (fields, arrays, _nulls) = chunks.into_parts();
                buffer_ref.add_arrays(fields, arrays, size, false);
            }
            Ok(EntryMetadataDerivedFromData::new(
                None,
                Some(auxiliary_arrays),
                None,
                Some(n_pts),
            ))
        } else {
            let (aux, n_pts) = self.data_buffers.add(series_index, series_time, peaks);
            Ok(EntryMetadataDerivedFromData::new(
                None,
                Some(aux),
                None,
                Some(n_pts),
            ))
        }
    }

    pub fn point_count(&self) -> u64 {
        self.data_buffers.point_count()
    }

    pub fn as_array_index(&self) -> crate::peak_series::ArrayIndex {
        self.data_buffers.as_array_index()
    }

    pub fn schema(&self) -> &Arc<Schema> {
        self.data_buffers.schema()
    }

    pub fn buffers(&self) -> &ArrayBufferWriterVariants {
        &self.data_buffers
    }

    pub fn drain_into<W: io::Write + Send>(
        &mut self,
        writer: &mut ArrowWriter<W>,
    ) -> io::Result<()> {
        let use_chunks = self.use_chunked_encoding().is_some();
        for batch in self.data_buffers.drain() {
            writer.write(&batch)?;
            if writer.in_progress_size() > 512_000_000 && use_chunks {
                log::debug!(
                    "Flushing row group buffer with approximately {} bytes",
                    writer.in_progress_size()
                );
                writer.flush()?;
            }
        }
        Ok(())
    }
}

pub(crate) fn is_data_array_sorted(array: &DataArray) -> Result<bool, ArrayRetrievalError> {
    Ok(match array.dtype() {
        mzdata::spectrum::BinaryDataArrayType::Unknown
        | mzdata::spectrum::BinaryDataArrayType::ASCII => true,
        mzdata::spectrum::BinaryDataArrayType::Float64 => array.to_f64()?.is_sorted(),
        mzdata::spectrum::BinaryDataArrayType::Float32 => array.to_f32()?.is_sorted(),
        mzdata::spectrum::BinaryDataArrayType::Int64 => array.to_i64()?.is_sorted(),
        mzdata::spectrum::BinaryDataArrayType::Int32 => array.to_i32()?.is_sorted(),
    })
}

pub trait AbstractMzPeakWriter {
    /// Append an arbitrary key bytestring with an optional value to the (current) Parquet file
    fn append_key_value_metadata(&mut self, key: String, value: Option<String>);

    fn add_index_metadata(
        &mut self,
        key: &str,
        value: &impl serde::Serialize,
    ) -> Result<(), serde_json::Error>;

    /// Access the [`mzdata`] metadata bundle for the data file being written
    fn mz_metadata(&self) -> &FileMetadataConfig;

    /// Copy all run-level metadata to the file index
    fn copy_metadata_to_index(&mut self) -> Result<(), serde_json::Error> {
        self.add_index_metadata(
            "file_description",
            &crate::param::FileDescription::from(self.mz_metadata().file_description()),
        )?;
        let tmp: Vec<_> = self
            .mz_metadata()
            .instrument_configurations()
            .values()
            .map(|v| crate::param::InstrumentConfiguration::from(v))
            .collect();
        self.add_index_metadata(
            "instrument_configuration_list",
            &tmp,
        )?;

        let tmp: Vec<_> = self
            .mz_metadata()
            .data_processings()
            .iter()
            .map(|v| crate::param::DataProcessing::from(v))
            .collect();
        self.add_index_metadata(
            "data_processing_method_list",
            &tmp,
        )?;

        let tmp: Vec<_> = self
            .mz_metadata()
            .softwares()
            .iter()
            .map(|v| crate::param::Software::from(v))
            .collect();
        self.add_index_metadata(
            "software_list",
            &tmp,
        )?;

        let tmp: Vec<_> = self
            .mz_metadata()
            .samples()
            .iter()
            .map(|v| crate::param::Sample::from(v))
            .collect();

        self.add_index_metadata(
            "sample_list",
            &tmp,
        )?;

        let tmp: Vec<_> = self
            .mz_metadata()
            .scan_settings()
            .map(|vs| {
                vs.iter()
                    .map(|v| crate::param::ScanSettings::from(v))
                    .collect()
            })
            .unwrap_or_default();
        self.add_index_metadata(
            "scan_settings_list",
            &tmp,
        )?;

        let tmp = self.mz_metadata().run_description().unwrap().clone();
        self.add_index_metadata(
            "run",
            &tmp
        )?;
        Ok(())
    }

    /// Whether or not a chunking strategy is being used for spectra
    fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy>;

    /// Whether or not a chunking strategy is being used for chromatograms
    fn use_chromatogram_chunked_encoding(&self) -> Option<&ChunkingStrategy>;

    /// Get a mutable reference to the buffer of spectrum metadata values,
    /// for appending only
    fn spectrum_entry_buffer_mut(&mut self) -> &mut SpectrumBuilder;

    /// Get a mutable reference to the buffer of chromatogram metadata values,
    /// for appending only
    fn chromatogram_entry_buffer_mut(&mut self) -> &mut ChromatogramBuilder;

    /// Get a mutable reference to the buffer of spectrum signal data values,
    /// for appending only
    fn spectrum_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants;

    /// Get a mutable reference to the buffer of chromatogram signal data values,
    /// for appending only
    fn chromatogram_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants;

    fn wavelength_entry_buffer_mut(&mut self) -> &mut WavelengthSpectrumBuilder;
    fn wavelength_data_buffer_mut(&mut self) -> &mut GenericDataArrayWriter;

    fn make_wavelength_data_writer(&self) -> GenericDataArrayWriter {
        let buffers: ArrayBufferWriterVariants = ArrayBuffersBuilder::default()
            .add_default_fields_for_context(BufferContext::WavelengthSpectrum)
            .with_context(BufferContext::WavelengthSpectrum)
            .add_override(
                WAVELENGTH_ARRAY
                    .clone()
                    .with_dtype(mzdata::spectrum::BinaryDataArrayType::Float64),
                WAVELENGTH_ARRAY.clone(),
            )
            .add_override(
                INTENSITY_ARRAY
                    .clone()
                    .with_context(BufferContext::WavelengthSpectrum)
                    .with_dtype(mzdata::spectrum::BinaryDataArrayType::Float64),
                INTENSITY_ARRAY
                    .clone()
                    .with_context(BufferContext::WavelengthSpectrum),
            )
            .build(
                Arc::new(Schema::empty()),
                BufferContext::WavelengthSpectrum,
                false,
            )
            .into();
        GenericDataArrayWriter::new(buffers)
    }

    /// Check if the data buffers are full, and flush them if so
    fn check_data_buffer(&mut self) -> io::Result<()>;

    /// The current number of spectra having been written to the MzPeak file
    fn spectrum_counter(&self) -> u64;

    /// The current number of distinct precursors having been written to the MzPeak file
    fn spectrum_precursor_counter(&self) -> u64;

    /// The current number of chromatograms having been written to the MzPeak file or buffer
    fn chromatogram_counter(&self) -> u64;

    /// Write a [`BinaryArrayMap`] to the data buffer for chromatograms.
    ///
    /// If chunked encoding is enabled, the [`ChunkingStrategy`] will be applied, regardless of whether or not the
    /// spectrum is in profile mode. This might change in the future.
    fn write_chromatogram_arrays(
        &mut self,
        chromatogram: &impl ChromatogramLike,
        binary_array_map: &BinaryArrayMap,
    ) -> io::Result<(Option<Vec<AuxiliaryArray>>, usize)> {
        let time = binary_array_map.get(&ArrayType::TimeArray).unwrap();
        let chromatogram_index = self.chromatogram_counter();
        let n_points = time.data_len()?;
        let sorted = is_data_array_sorted(time)?;
        let mut tmp_binary_array_map = BinaryArrayMap::new();
        if !sorted {
            log::warn!(
                "Chromatogram {chromatogram_index} ({}) was not sorted, sorting {n_points} values",
                chromatogram.id()
            );
            binary_array_map.clone_into(&mut tmp_binary_array_map);
            tmp_binary_array_map.sort_by_array(&ArrayType::TimeArray)?;
        }
        let (extra_arrays, n_points) =
            if let Some(chunking) = self.use_chromatogram_chunked_encoding().copied() {
                let buffer_ref = self.chromatogram_data_buffer_mut();
                let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                    chromatogram_index,
                    None,
                    BufferContext::Chromatogram,
                    if sorted {
                        binary_array_map
                    } else {
                        &tmp_binary_array_map
                    },
                    chunking,
                    buffer_ref.overrides(),
                    buffer_ref.drop_zero_intensity(),
                    buffer_ref.nullify_zero_intensity(),
                    buffer_ref.fields(),
                )?;

                if let Some(chunks) = chunks {
                    let size = chunks.len();
                    let (fields, arrays, _nulls) = chunks.into_parts();
                    buffer_ref.add_arrays(fields, arrays, size, true);
                }

                (Some(auxiliary_arrays), n_pts)
            } else {
                let buffer = self.chromatogram_data_buffer_mut();

                let (fields, data, extra_arrays) = array_map_to_schema_arrays_and_excess(
                    BufferContext::Chromatogram,
                    if sorted {
                        binary_array_map
                    } else {
                        &tmp_binary_array_map
                    },
                    n_points,
                    chromatogram_index,
                    None,
                    Some(buffer.fields()),
                    buffer.overrides(),
                )?;

                let pts_written = buffer.add_arrays(fields, data, n_points, true);
                (Some(extra_arrays), pts_written)
            };

        Ok((extra_arrays, n_points))
    }

    /// Write a `chromatogram` to the MzPeak file
    ///
    /// Data may be buffered until the chromatogram data file is ready
    /// to be written.
    fn write_chromatogram(&mut self, chromatogram: &Chromatogram) -> io::Result<()> {
        log::trace!("Writing chromatogram {}", chromatogram.id());
        let (aux_arrays, _n_points) =
            self.write_chromatogram_arrays(chromatogram, &chromatogram.arrays)?;
        self.chromatogram_entry_buffer_mut()
            .append_value(chromatogram, aux_arrays);
        self.check_data_buffer()?;
        Ok(())
    }

    /// Write a `spectrum` to the MzPeak file
    ///
    /// Data may be buffered until the spectrum data file is ready to be written, but the spectrum data file
    /// is likely being actively written out as the the buffer grows.
    fn write_spectrum<
        C: ToMzPeakDataSeries + CentroidLike,
        D: ToMzPeakDataSeries + DeconvolutedCentroidLike,
        S: SpectrumLike<C, D> + 'static,
    >(
        &mut self,
        spectrum: &S,
    ) -> io::Result<()> {
        log::trace!("Writing spectrum {}", spectrum.id());
        if let Some(spec_type) = spectrum.spectrum_type() {
            if !spec_type.is_mass_spectrum() {
                log::trace!("Non-MS spectrum {spec_type:?}");
                if let Some(data_arrays) = spectrum.raw_arrays() {
                    let series_index = self.wavelength_entry_buffer_mut().index_counter();
                    if data_arrays
                        .has_array(&BufferContext::WavelengthSpectrum.default_sorted_array())
                    {
                        let writer = self.wavelength_data_buffer_mut();
                        let entry_meta = writer.write_data_arrays(
                            data_arrays,
                            spectrum.signal_continuity() == SignalContinuity::Profile,
                            writer
                                .buffers()
                                .include_time()
                                .then(|| spectrum.start_time() as f32),
                            series_index,
                        )?;
                        let entry_writer = self.wavelength_entry_buffer_mut();
                        entry_writer.append_value(spectrum, entry_meta.auxiliary_arrays);
                        return Ok(());
                    } else {
                        log::warn!(
                            "Non-MS spectrum did not have {:?}. Falling through to MS spectrum writing",
                            BufferContext::WavelengthSpectrum.default_sorted_array()
                        );
                    }
                }
            }
        }
        let entry_derived = self.write_spectrum_data(spectrum)?;
        self.spectrum_entry_buffer_mut()
            .append_value(spectrum, entry_derived);
        self.check_data_buffer()?;
        Ok(())
    }

    /// Fit an [`MZDeltaModel`] instance on the provided (sparse) spectrum signal, and return the parameter
    /// buffer.
    ///
    /// If an intensity array is available, it will be used to weight the parameter estimation procedure.
    ///
    /// If no m/z array is available, `None` is returned
    fn build_delta_model(&self, binary_array_map: &BinaryArrayMap) -> Option<Vec<f64>> {
        if let Ok(mzs) = binary_array_map.mzs() {
            let delta_model = if let Ok(ints) = binary_array_map.intensities() {
                let weights: Vec<f64> =
                    ints.iter().map(|i| (*i + 1.0).ln().sqrt() as f64).collect();
                select_delta_model(&mzs, Some(&weights))
            } else {
                select_delta_model(&mzs, None)
            };
            Some(delta_model)
        } else {
            None
        }
    }

    /// Write a [`BinaryArrayMap`] to the data buffer for spectra.
    ///
    /// If sparse data encoding is enabled ([`ArrayBufferWriter::nullify_zero_intensity`]), and the
    /// `spectrum` is in profile mode, this will fit a delta model with [`AbstractMzPeakWriter::build_delta_model`].
    ///
    /// If chunked encoding is enabled, the [`ChunkingStrategy`] will be applied, regardless of whether or not the
    /// spectrum is in profile mode. This might change in the future.
    ///
    /// This is a helper method for [`AbstractMzPeakWriter::write_spectrum_data`].
    fn write_spectrum_binary_array_map<
        C: ToMzPeakDataSeries + CentroidLike,
        D: ToMzPeakDataSeries + DeconvolutedCentroidLike,
    >(
        &mut self,
        spectrum: &impl SpectrumLike<C, D>,
        spectrum_count: u64,
        binary_array_map: &BinaryArrayMap,
    ) -> Result<EntryMetadataDerivedFromData, ArrayRetrievalError> {
        let mzs = binary_array_map.mzs();
        let (_had_mzs, n_points) = if let Ok(mzs) = mzs.as_ref() {
            (true, mzs.len())
        } else {
            (false, 0)
        };

        let is_profile = spectrum.signal_continuity() == SignalContinuity::Profile;
        let include_time = self.spectrum_data_buffer_mut().include_time();
        let spectrum_time = if include_time {
            Some(spectrum.start_time() as f32)
        } else {
            None
        };

        let mut tmp_binary_array_map = BinaryArrayMap::new();
        let sorted = mzs.as_ref().map(|v| v.is_sorted()).unwrap_or(true);
        if !sorted {
            log::warn!(
                "Spectrum {spectrum_count} ({}) was not sorted, sorting {n_points} values",
                spectrum.id()
            );
            binary_array_map.clone_into(&mut tmp_binary_array_map);
            tmp_binary_array_map.sort_by_array(&ArrayType::MZArray)?;
        }

        log::trace!("Writing {n_points} points for {spectrum_count}");
        let (delta_params, extra_arrays, n_pts) = if let Some(chunking) =
            self.use_chunked_encoding().copied()
        {
            // If we use the chunked encoding, we pre-encode everything
            let nullify_zero_intensity = self.spectrum_data_buffer_mut().nullify_zero_intensity();
            let delta_model = if is_profile && nullify_zero_intensity {
                self.build_delta_model(if sorted {
                    binary_array_map
                } else {
                    &tmp_binary_array_map
                })
            } else {
                None
            };
            let buffer_ref = self.spectrum_data_buffer_mut();

            let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                spectrum_count,
                spectrum_time,
                BufferContext::Spectrum,
                if sorted {
                    binary_array_map
                } else {
                    &tmp_binary_array_map
                },
                chunking,
                buffer_ref.overrides(),
                is_profile,
                nullify_zero_intensity,
                buffer_ref.fields(),
            )?;

            if let Some(chunks) = chunks {
                let size = chunks.len();
                let (fields, arrays, _nulls) = chunks.into_parts();
                buffer_ref.add_arrays(fields, arrays, size, is_profile);
            }

            (delta_model, Some(auxiliary_arrays), n_pts)
        } else {
            let nullify_zero_intensity = self.spectrum_data_buffer_mut().nullify_zero_intensity();
            let delta_model = if is_profile && nullify_zero_intensity {
                self.build_delta_model(if sorted {
                    binary_array_map
                } else {
                    &tmp_binary_array_map
                })
            } else {
                None
            };

            let buffer = self.spectrum_data_buffer_mut();

            let (fields, data, extra_arrays) = array_map_to_schema_arrays_and_excess(
                BufferContext::Spectrum,
                if sorted {
                    binary_array_map
                } else {
                    &tmp_binary_array_map
                },
                n_points,
                spectrum_count,
                spectrum_time,
                Some(buffer.fields()),
                buffer.overrides(),
            )?;

            let pts_written = buffer.add_arrays(fields, data, n_points, is_profile);
            (delta_model, Some(extra_arrays), pts_written)
        };

        Ok(EntryMetadataDerivedFromData::new(
            delta_params,
            extra_arrays,
            Some(n_pts),
            None,
        ))
    }

    /// Write a peak list to the data buffer.
    fn write_peaks<C: ToMzPeakDataSeries>(
        &mut self,
        spectrum_count: u64,
        mut spectrum_time: Option<f32>,
        peaks: &[C],
    ) -> Result<EntryMetadataDerivedFromData, ArrayRetrievalError> {
        let include_time = self.spectrum_data_buffer_mut().include_time();
        if !include_time {
            spectrum_time = None;
        }
        if let Some(encoding) = self.use_chunked_encoding().copied() {
            let arrays = C::as_arrays(peaks);
            let buffer_ref = self.spectrum_data_buffer_mut();

            let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                spectrum_count,
                spectrum_time,
                BufferContext::Spectrum,
                &arrays,
                encoding,
                buffer_ref.overrides(),
                false,
                false,
                buffer_ref.fields(),
            )?;

            if let Some(chunks) = chunks {
                let size = chunks.len();
                let (fields, arrays, _nulls) = chunks.into_parts();
                buffer_ref.add_arrays(fields, arrays, size, false);
            }
            Ok(EntryMetadataDerivedFromData::new(
                None,
                Some(auxiliary_arrays),
                None,
                Some(n_pts),
            ))
        } else {
            let (aux, n_pts) =
                self.spectrum_data_buffer_mut()
                    .add(spectrum_count, spectrum_time, peaks);
            Ok(EntryMetadataDerivedFromData::new(
                None,
                Some(aux),
                None,
                Some(n_pts),
            ))
        }
    }

    fn write_batch_config(&self) -> WriteBatchConfig;

    fn compression(&self) -> Compression;

    fn shuffle_mz(&self) -> bool;

    fn buffer_size(&self) -> usize;

    fn encryption_properties(&self) -> &HashMap<String, Arc<FileEncryptionProperties>>;

    fn get_or_create_spectrum_peak_writer(
        &mut self,
    ) -> io::Result<&mut MiniPeakWriterType<fs::File>> {
        if self.spectrum_peak_writer().is_none() {
            log::warn!("Initializing default spectrum peak writer");
            let peak_buffer_file = tempfile::tempfile()?;
            let builder = ArrayBuffersBuilder::default()
                .extend_overrides(
                    self.spectrum_data_buffer_mut()
                        .overrides()
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                )
                .with_context(BufferContext::Spectrum);
            let writer = Self::make_peaks_writer(
                peak_buffer_file,
                builder,
                self.write_batch_config(),
                self.compression(),
                self.spectrum_data_buffer_mut().include_time(),
                self.shuffle_mz(),
                self.buffer_size(),
                self.encryption_properties(),
            )?;
            self.set_spectrum_peak_writer(writer);
        }
        self.spectrum_peak_writer()
            .ok_or_else(|| io::Error::other("Cannot create peak writer"))
    }

    /// Write the spectrum data of any dimensions to the data buffer.
    ///
    /// Uses [`SpectrumLike::peaks`] to decide which kind of data to write.
    fn write_spectrum_data<
        CI: ToMzPeakDataSeries + CentroidLike,
        DI: ToMzPeakDataSeries + DeconvolutedCentroidLike,
    >(
        &mut self,
        spectrum: &impl SpectrumLike<CI, DI>,
    ) -> io::Result<EntryMetadataDerivedFromData> {
        let spectrum_index = self.spectrum_counter();

        let peaks = spectrum.peaks();

        let spectrum_time = if self.spectrum_data_buffer_mut().include_time() {
            Some(spectrum.start_time() as f32)
        } else {
            None
        };

        let entry_derived = if matches!(
            spectrum.peaks(),
            RefPeakDataLevel::Centroid(_) | RefPeakDataLevel::Deconvoluted(_)
        ) && spectrum.raw_arrays().is_some()
            && spectrum.signal_continuity() == SignalContinuity::Profile
        {
            log::trace!("Writing both profile signal and peaks for {spectrum_index}");
            let raw_arrays = spectrum.raw_arrays().unwrap();
            let entry_derived =
                self.write_spectrum_binary_array_map(spectrum, spectrum_index, raw_arrays)?;
            self.get_or_create_spectrum_peak_writer()?.write_peaks(
                spectrum_index,
                spectrum_time,
                peaks,
            )?;
            entry_derived
        } else {
            let entry_derived = match peaks {
                mzdata::spectrum::RefPeakDataLevel::Missing => {
                    log::trace!("No signal data for {spectrum_index}");
                    EntryMetadataDerivedFromData::default()
                }
                mzdata::spectrum::RefPeakDataLevel::RawData(binary_array_map) => {
                    match spectrum.signal_continuity() {
                        SignalContinuity::Profile => {
                            log::trace!(
                                "Writing {} raw arrays for {spectrum_index}",
                                binary_array_map.len()
                            );
                            self.write_spectrum_binary_array_map(
                                spectrum,
                                spectrum_index,
                                binary_array_map,
                            )?
                        }
                        SignalContinuity::Centroid | SignalContinuity::Unknown => {
                            log::trace!(
                                "Writing {} peaks from raw arrays for {spectrum_index}",
                                peaks.len()
                            );
                            let writer = self.get_or_create_spectrum_peak_writer()?;
                            // self.write_spectrum_binary_array_map(spectrum, spectrum_count, binary_array_map)?
                            writer.write_peaks(spectrum_index, spectrum_time, peaks)?
                        }
                    }
                }
                mzdata::spectrum::RefPeakDataLevel::Centroid(_) => self
                    .get_or_create_spectrum_peak_writer()?
                    .write_peaks(spectrum_index, spectrum_time, peaks)?
                    .into(),
                mzdata::spectrum::RefPeakDataLevel::Deconvoluted(_) => self
                    .get_or_create_spectrum_peak_writer()?
                    .write_peaks(spectrum_index, spectrum_time, peaks)?
                    .into(),
            };
            entry_derived
        };

        Ok(entry_derived)
    }

    /// Get the writer for the separate peak list file, if one is available
    fn spectrum_peak_writer(&mut self) -> Option<&mut MiniPeakWriterType<fs::File>>;

    fn set_spectrum_peak_writer(&mut self, writer: MiniPeakWriterType<fs::File>);

    /// Create a specrtum peak writer over the provided stream using the given configuration
    fn make_peaks_writer<S: io::Write + Send + io::Seek>(
        stream: S,
        peak_buffer_builder: ArrayBuffersBuilder,
        write_batch_config: WriteBatchConfig,
        compression: Compression,
        include_time: bool,
        shuffle_mz: bool,
        buffer_size: usize,
        encryption_properties: &HashMap<String, Arc<FileEncryptionProperties>>,
    ) -> io::Result<MiniPeakWriterType<S>> {
        let peak_buffer = peak_buffer_builder.include_time(include_time).build(
            Arc::new(Schema::empty()),
            BufferContext::Spectrum,
            false,
        );

        let peak_encrytion_props = encryption_properties
            .get(&FileEntry::from(MzPeakArchiveType::SpectrumPeakDataArrays).name)
            .cloned();

        let peak_data_props = Self::spectrum_data_writer_props(
            &peak_buffer,
            peak_buffer.index_path(),
            shuffle_mz,
            &None,
            compression,
            write_batch_config,
            peak_encrytion_props,
        );

        let peak_writer = ArrowWriter::try_new_with_options(
            stream,
            peak_buffer.schema().clone(),
            ArrowWriterOptions::new().with_properties(peak_data_props),
        )?;

        Ok(MiniPeakWriterType::new(
            peak_writer,
            peak_buffer.into(),
            buffer_size,
        ))
    }

    /// Generate the [`WriterProperties`] for the the spectrum metadata file, based upon
    /// the provided schema.
    ///
    /// This currently uses a constant Zstd compression level.
    fn spectrum_metadata_writer_props(
        metadata_fields: &SchemaRef,
        encryption_properties: Option<Arc<FileEncryptionProperties>>,
    ) -> WriterProperties {
        let parquet_schema = Arc::new(
            ArrowSchemaConverter::new()
                .convert(metadata_fields)
                .unwrap(),
        );

        let mut sorted = Vec::new();
        for (i, c) in parquet_schema.columns().iter().enumerate() {
            match c.path().string().as_ref() {
                "spectrum.index" => {
                    sorted.push(SortingColumn {
                        column_idx: i as i32,
                        descending: false,
                        nulls_first: false,
                    });
                }
                _ => {}
            }
        }

        let mut builder = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::ZSTD(
                ZstdLevel::try_new(3).unwrap(),
            ))
            .set_dictionary_enabled(true)
            .set_sorting_columns(Some(sorted))
            .set_column_bloom_filter_enabled("spectrum.id".into(), true)
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_statistics_enabled(EnabledStatistics::Page);

        if let Some(encryption_props) = encryption_properties {
            builder = builder.with_file_encryption_properties(encryption_props);
        }
        builder.build()
    }

    /// Generate the [`WriterProperties`] for a generic data arrays file, based upon
    /// the provided schema and caller configuration.
    ///
    /// If `use_chunked_encoding` is enabled, it can have far-reaching effects on all other
    /// parameters.
    fn generic_data_writer_props(
        data_buffer: &impl ArrayBufferWriter,
        index_path: String,
        use_chunked_encoding: &Option<ChunkingStrategy>,
        compression: Compression,
        byte_shuffle_needles: &[&str],
        encryption_properties: Option<Arc<FileEncryptionProperties>>,
    ) -> WriterProperties {
        let parquet_schema = Arc::new(
            ArrowSchemaConverter::new()
                .convert(data_buffer.schema())
                .unwrap(),
        );

        let mut sorted = Vec::new();
        for (i, c) in parquet_schema.columns().iter().enumerate() {
            match c.path().string().as_ref() {
                x if x == index_path => {
                    sorted.push(SortingColumn {
                        column_idx: i as i32,
                        descending: false,
                        nulls_first: false,
                    });
                }
                _ => {}
            }
        }

        let mut data_props = WriterProperties::builder()
            .set_compression(compression)
            .set_dictionary_enabled(true)
            .set_sorting_columns(Some(sorted))
            .set_column_encoding(index_path.clone().into(), Encoding::DELTA_BINARY_PACKED)
            .set_column_bloom_filter_enabled(index_path.clone().into(), true)
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_statistics_enabled(EnabledStatistics::Page);

        if use_chunked_encoding.is_some() {
            data_props = data_props.set_max_row_group_size(1024 * 100)
        }

        for c in parquet_schema.columns().iter() {
            let colpath = c.path().to_string();
            if byte_shuffle_needles.iter().any(|s| colpath.contains(s))
                && matches!(
                    c.physical_type(),
                    parquet::basic::Type::DOUBLE | parquet::basic::Type::FLOAT
                )
            {
                log::debug!("{}: shuffling", c.path());
                data_props =
                    data_props.set_column_encoding(c.path().clone(), Encoding::BYTE_STREAM_SPLIT);
            }
            if colpath.contains("ion_mobility") {
                log::debug!(
                    "{}: ion mobility detected, increasing dictionary size",
                    c.path()
                );
                data_props = data_props
                    .set_dictionary_page_size_limit(DEFAULT_DICTIONARY_PAGE_SIZE_LIMIT * 2);
            }
            if c.name().ends_with("_index") {
                log::debug!("{}: delta binary packing", c.path());
                data_props =
                    data_props.set_column_encoding(c.path().clone(), Encoding::DELTA_BINARY_PACKED);
            }
        }

        if let Some(encryption_props) = encryption_properties {
            data_props = data_props.with_file_encryption_properties(encryption_props);
        }
        data_props.build()
    }

    /// Generate the [`WriterProperties`] for the chromatogram signal data file, based upon
    /// the provided schema and caller configuration.
    ///
    /// If `use_chunked_encoding` is enabled, it can have far-reaching effects on all other
    /// parameters.
    fn chromatogram_data_writer_props(
        data_buffer: &impl ArrayBufferWriter,
        index_path: String,
        use_chunked_encoding: &Option<ChunkingStrategy>,
        compression: Compression,
        encryption_properties: Option<Arc<FileEncryptionProperties>>,
    ) -> WriterProperties {
        Self::generic_data_writer_props(
            data_buffer,
            index_path,
            use_chunked_encoding,
            compression,
            &["_time", ".time"],
            encryption_properties,
        )
    }

    /// Generate the [`WriterProperties`] for the spectrum signal data file, based upon
    /// the provided schema and caller configuration.
    ///
    /// If `use_chunked_encoding` is enabled, it can have far-reaching effects on all other
    /// parameters.
    ///
    /// If an `ion_mobility` array is detected based upon column name, it will increase
    /// the dictionary page size.
    fn spectrum_data_writer_props(
        data_buffer: &impl ArrayBufferWriter,
        index_path: String,
        shuffle_mz: bool,
        use_chunked_encoding: &Option<ChunkingStrategy>,
        compression: Compression,
        write_batch_config: WriteBatchConfig,
        encryption_properties: Option<Arc<FileEncryptionProperties>>,
    ) -> WriterProperties {
        let parquet_schema = Arc::new(
            ArrowSchemaConverter::new()
                .convert(data_buffer.schema())
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to convert {:?} to a schema: {e}",
                        data_buffer.schema()
                    )
                }),
        );

        let mut sorted = Vec::new();
        for (i, c) in parquet_schema.columns().iter().enumerate() {
            match c.path().string().as_ref() {
                x if x == index_path => {
                    sorted.push(SortingColumn {
                        column_idx: i as i32,
                        descending: false,
                        nulls_first: false,
                    });
                }
                _ => {}
            }
        }

        let max_row_group_size = write_batch_config
            .row_group_size
            .unwrap_or(parquet::file::properties::DEFAULT_MAX_ROW_GROUP_SIZE);
        let data_page_size = write_batch_config
            .page_size
            .unwrap_or(parquet::file::properties::DEFAULT_PAGE_SIZE);
        let write_batch_size = write_batch_config
            .write_batch_size
            .unwrap_or(parquet::file::properties::DEFAULT_WRITE_BATCH_SIZE);

        let mut data_props = WriterProperties::builder()
            .set_compression(compression)
            .set_dictionary_enabled(true)
            .set_sorting_columns(Some(sorted))
            .set_column_encoding(index_path.clone().into(), Encoding::DELTA_BINARY_PACKED)
            .set_column_bloom_filter_enabled(index_path.clone().into(), true)
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_statistics_enabled(EnabledStatistics::Page)
            .set_write_batch_size(write_batch_size);

        if use_chunked_encoding.is_some() {
            data_props = data_props
                .set_max_row_group_size(max_row_group_size)
                .set_data_page_size_limit(data_page_size / 4);
        } else {
            data_props = data_props
                .set_max_row_group_size(max_row_group_size)
                .set_data_page_row_count_limit(data_page_size);
        }

        for c in parquet_schema.columns().iter() {
            let colpath = c.path().to_string();
            if (colpath.contains("_mz_") || colpath.contains(".mz"))
                && shuffle_mz
                && matches!(
                    c.physical_type(),
                    parquet::basic::Type::DOUBLE | parquet::basic::Type::FLOAT
                )
            {
                log::debug!("{}: shuffling", c.path());
                data_props =
                    data_props.set_column_encoding(c.path().clone(), Encoding::BYTE_STREAM_SPLIT);
            }
            if colpath.contains("ion_mobility") {
                log::debug!(
                    "{}: ion mobility detected, increasing dictionary size",
                    c.path()
                );
                data_props = data_props
                    .set_dictionary_page_size_limit(DEFAULT_DICTIONARY_PAGE_SIZE_LIMIT * 2);
            }
            if c.name().ends_with("_index") {
                log::debug!("{}: delta binary packing", c.path());
                data_props =
                    data_props.set_column_encoding(c.path().clone(), Encoding::DELTA_BINARY_PACKED);
            }
        }

        if let Some(encryption_props) = encryption_properties {
            data_props = data_props.with_file_encryption_properties(encryption_props)
        }

        data_props.build()
    }
}
