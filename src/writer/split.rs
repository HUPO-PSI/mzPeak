use std::{collections::HashMap, fs, io, marker::PhantomData, path::PathBuf, sync::Arc};

use arrow::{
    array::{ArrayBuilder, AsArray, RecordBatch},
    datatypes::{FieldRef, Schema, SchemaRef},
};
use mzpeaks::{CentroidPeak, DeconvolutedPeak};
use parquet::{
    arrow::{ArrowWriter, arrow_writer::ArrowWriterOptions},
    basic::Compression,
    encryption::encrypt::FileEncryptionProperties,
    file::metadata::KeyValue,
};

use mzdata::{meta::FileMetadataConfig, params::ControlledVocabulary, prelude::*};

use crate::{
    BufferContext, ToMzPeakDataSeries, archive::{FileIndex, MzPeakArchiveType}, chunk_series::ChunkingStrategy, constants::SPECTRUM_ARRAY_INDEX, param::ControlledVocabularyEntry, peak_series::ArrayIndex, writer::{
        AbstractMzPeakWriter, ArrayBufferWriter, ArrayBufferWriterVariants, ArrayBuffersBuilder,
        ChromatogramBuilder, MiniPeakWriterType, SpectrumBuilder, VisitorBase,
        WavelengthSpectrumBuilder, WriteBatchConfig, base::GenericDataArrayWriter,
        builder::SpectrumFieldVisitors, implement_mz_metadata,
    }
};

/// Writer for the MzPeak format that writes the different data types to separate files
/// in an unarchived format.
pub struct UnpackedMzPeakWriterType<
    C: CentroidLike + ToMzPeakDataSeries = CentroidPeak,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries = DeconvolutedPeak,
> {
    path: PathBuf,
    file_index: FileIndex,
    controlled_vocabularies: Vec<ControlledVocabularyEntry>,
    spectrum_data_writer: ArrowWriter<fs::File>,
    spectrum_metadata_writer: ArrowWriter<fs::File>,

    spectrum_buffers: ArrayBufferWriterVariants,
    chromatogram_buffers: ArrayBufferWriterVariants,
    separate_peak_writer: Option<MiniPeakWriterType<fs::File>>,

    spectrum_metadata_buffer: SpectrumBuilder,
    chromatogram_metadata_buffer: ChromatogramBuilder,
    wavelength_metadata_buffer: WavelengthSpectrumBuilder,

    use_chunked_encoding: Option<ChunkingStrategy>,
    use_chromatogram_chunked_encoding: Option<ChunkingStrategy>,

    spectrum_data_point_counter: u64,

    #[allow(unused)]
    chromatogram_data_point_counter: u64,

    buffer_size: usize,
    compression: Compression,
    shuffle_mz: bool,
    encryption_properties: HashMap<String, Arc<FileEncryptionProperties>>,

    #[allow(unused)]
    write_batch_config: WriteBatchConfig,
    mz_metadata: FileMetadataConfig,
    _t: PhantomData<(C, D)>,
}

impl<C: CentroidLike + ToMzPeakDataSeries, D: DeconvolutedCentroidLike + ToMzPeakDataSeries> Drop
    for UnpackedMzPeakWriterType<C, D>
{
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            log::trace!("While dropping UnpackedMzPeakWriterType: {e}")
        }
    }
}

impl<C: CentroidLike + ToMzPeakDataSeries, D: DeconvolutedCentroidLike + ToMzPeakDataSeries>
    MSDataFileMetadata for UnpackedMzPeakWriterType<C, D>
{
    mzdata::delegate_impl_metadata_trait!(mz_metadata);
}

impl<C: CentroidLike + ToMzPeakDataSeries, D: DeconvolutedCentroidLike + ToMzPeakDataSeries>
    AbstractMzPeakWriter for UnpackedMzPeakWriterType<C, D>
{
    fn append_key_value_metadata(&mut self, key: String, value: Option<String>) {
        self.append_key_value_metadata(key, value);
    }

    fn spectrum_counter(&self) -> u64 {
        self.spectrum_metadata_buffer.index_counter()
    }

    fn spectrum_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants {
        &mut self.spectrum_buffers
    }

    fn spectrum_entry_buffer_mut(&mut self) -> &mut SpectrumBuilder {
        &mut self.spectrum_metadata_buffer
    }

    fn check_data_buffer(&mut self) -> io::Result<()> {
        if self.spectrum_counter() % (self.buffer_size as u64) == 0 {
            self.flush_spectrum_data_arrays()?;
        }
        Ok(())
    }

    fn spectrum_peak_writer(&mut self) -> Option<&mut MiniPeakWriterType<fs::File>> {
        self.separate_peak_writer.as_mut()
    }

    fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.use_chunked_encoding.as_ref()
    }

    fn use_chromatogram_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.use_chromatogram_chunked_encoding.as_ref()
    }

    fn spectrum_precursor_counter(&self) -> u64 {
        self.spectrum_metadata_buffer.precursor_index_counter()
    }

    fn chromatogram_counter(&self) -> u64 {
        self.chromatogram_metadata_buffer.index_counter()
    }

    fn chromatogram_entry_buffer_mut(&mut self) -> &mut ChromatogramBuilder {
        &mut self.chromatogram_metadata_buffer
    }

    fn chromatogram_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants {
        &mut self.chromatogram_buffers
    }

    fn wavelength_data_buffer_mut(&mut self) -> &mut GenericDataArrayWriter {
        todo!()
    }

    fn wavelength_entry_buffer_mut(&mut self) -> &mut WavelengthSpectrumBuilder {
        &mut self.wavelength_metadata_buffer
    }

    fn set_spectrum_peak_writer(&mut self, writer: MiniPeakWriterType<fs::File>) {
        self.separate_peak_writer = Some(writer);
    }

    fn write_batch_config(&self) -> WriteBatchConfig {
        self.write_batch_config
    }

    fn compression(&self) -> Compression {
        self.compression
    }

    fn shuffle_mz(&self) -> bool {
        self.shuffle_mz
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    fn encryption_properties(&self) -> &HashMap<String, Arc<FileEncryptionProperties>> {
        &self.encryption_properties
    }

    fn add_index_metadata(&mut self, key: &str, value: &impl serde::Serialize) -> Result<(), serde_json::Error> {
        self.file_index.add_metadata(key, serde_json::to_value(value)?);
        Ok(())
    }

    fn mz_metadata(&self) -> &FileMetadataConfig {
        &self.mz_metadata
    }

    fn controlled_vocabularies(&self) -> &[crate::param::ControlledVocabularyEntry] {
        &self.controlled_vocabularies
    }

    fn controlled_vocabularies_mut(&mut self) -> &mut Vec<crate::param::ControlledVocabularyEntry> {
        &mut self.controlled_vocabularies
    }
}

impl<C: CentroidLike + ToMzPeakDataSeries, D: DeconvolutedCentroidLike + ToMzPeakDataSeries>
    UnpackedMzPeakWriterType<C, D>
{
    pub fn new(
        path: PathBuf,
        spectrum_buffers_builder: ArrayBuffersBuilder,
        chromatogram_buffers_builder: ArrayBuffersBuilder,
        buffer_size: usize,
        mask_zero_intensity_runs: bool,
        shuffle_mz: bool,
        use_chunked_encoding: Option<ChunkingStrategy>,
        use_chromatogram_chunked_encoding: Option<ChunkingStrategy>,
        compression: Compression,
        store_peaks_and_profiles_apart: Option<ArrayBuffersBuilder>,
        write_batch_config: WriteBatchConfig,
        spectrum_fields: SpectrumFieldVisitors,
    ) -> Self {
        let data_writer_path = path.join(MzPeakArchiveType::SpectrumDataArrays.tag_file_suffix());
        let metadata_writer_path = path.join(MzPeakArchiveType::SpectrumMetadata.tag_file_suffix());

        let spectrum_data_writer = fs::File::create(data_writer_path).unwrap();
        let spectrum_metadata_writer = fs::File::create(metadata_writer_path).unwrap();

        let mut spectrum_metadata_buffer = SpectrumBuilder::default();
        spectrum_metadata_buffer
            .spectrum
            .extend_extra_fields(spectrum_fields.spectrum_fields);
        spectrum_metadata_buffer
            .scan
            .extend_extra_fields(spectrum_fields.spectrum_scan_fields);
        spectrum_metadata_buffer
            .selected_ion
            .extend_extra_fields(spectrum_fields.spectrum_selected_ion_fields);
        spectrum_metadata_buffer
            .precursor
            .extend_extra_activation_fields(spectrum_fields.spectrum_activation_fields);

        let fields: Vec<FieldRef> = spectrum_metadata_buffer.fields();
        let metadata_fields: SchemaRef = Arc::new(Schema::new(fields));
        let spectrum_buffers = spectrum_buffers_builder.build(
            Arc::new(Schema::empty()),
            BufferContext::Spectrum,
            mask_zero_intensity_runs,
        );

        let chromatogram_buffers = if let Some(_encoding) = use_chromatogram_chunked_encoding {
            ArrayBufferWriterVariants::ChunkBuffers(chromatogram_buffers_builder.build_chunked(
                Arc::new(Schema::empty()),
                BufferContext::Chromatogram,
                false,
            ))
        } else {
            ArrayBufferWriterVariants::PointBuffers(chromatogram_buffers_builder.build(
                Arc::new(Schema::empty()),
                BufferContext::Chromatogram,
                false,
            ))
        };

        let data_props = Self::spectrum_data_writer_props(
            &spectrum_buffers,
            spectrum_buffers.index_path(),
            shuffle_mz,
            &use_chunked_encoding,
            compression,
            write_batch_config,
            None,
        );

        let encryption_properties = Default::default();

        let separate_peak_writer = if let Some(peak_buffer_builder) = store_peaks_and_profiles_apart
        {
            let peak_buffer_file = fs::File::create(
                path.join(MzPeakArchiveType::SpectrumPeakDataArrays.tag_file_suffix()),
            )
            .unwrap();

            let peak_writer = Self::make_peaks_writer(
                peak_buffer_file,
                peak_buffer_builder,
                write_batch_config,
                compression,
                spectrum_buffers.include_time(),
                shuffle_mz,
                buffer_size,
                &encryption_properties,
            );
            peak_writer.ok()
        } else {
            None
        };

        let metadata_props = Self::spectrum_metadata_writer_props(&metadata_fields, None);

        let mut this = Self {
            path,
            file_index: Default::default(),
            spectrum_data_writer: ArrowWriter::try_new_with_options(
                spectrum_data_writer,
                spectrum_buffers.schema().clone(),
                ArrowWriterOptions::new().with_properties(data_props),
            )
            .unwrap(),
            spectrum_metadata_writer: ArrowWriter::try_new_with_options(
                spectrum_metadata_writer,
                metadata_fields.clone(),
                ArrowWriterOptions::new().with_properties(metadata_props),
            )
            .unwrap(),
            separate_peak_writer,
            spectrum_metadata_buffer,
            spectrum_buffers: spectrum_buffers.into(),
            chromatogram_buffers,
            chromatogram_metadata_buffer: Default::default(),
            spectrum_data_point_counter: 0,
            wavelength_metadata_buffer: Default::default(),

            use_chunked_encoding,
            use_chromatogram_chunked_encoding,
            shuffle_mz,
            encryption_properties: Default::default(),
            chromatogram_data_point_counter: 0,
            compression,
            write_batch_config,
            buffer_size: buffer_size,
            mz_metadata: Default::default(),
            _t: PhantomData,
            controlled_vocabularies: vec![ControlledVocabulary::MS.into(), ControlledVocabulary::UO.into()],
        };
        this.add_spectrum_array_index();
        this
    }

    implement_mz_metadata!();

    fn add_spectrum_array_index(&mut self) {
        let spectrum_array_index: ArrayIndex = self.spectrum_buffers.as_array_index();
        self.spectrum_data_writer
            .append_key_value_metadata(KeyValue::new(
                SPECTRUM_ARRAY_INDEX.to_string(),
                spectrum_array_index.to_json(),
            ));
    }

    pub fn append_key_value_metadata(
        &mut self,
        key: impl Into<String>,
        value: impl Into<Option<String>>,
    ) {
        self.spectrum_metadata_writer
            .append_key_value_metadata(KeyValue::new(key.into(), value));
    }

    pub fn write_spectrum<
        A: ToMzPeakDataSeries + CentroidLike,
        B: ToMzPeakDataSeries + DeconvolutedCentroidLike,
        S: SpectrumLike<A, B> + 'static,
    >(
        &mut self,
        spectrum: &S,
    ) -> io::Result<()> {
        AbstractMzPeakWriter::write_spectrum(self, spectrum)
    }

    fn flush_spectrum_data_arrays(&mut self) -> io::Result<()> {
        for batch in self.spectrum_buffers.drain() {
            self.spectrum_data_point_counter += batch.num_rows() as u64;
            self.spectrum_data_writer.write(&batch)?;
        }
        Ok(())
    }

    fn flush_chromatogram_data_records<W: Write + Send>(
        &mut self,
        writer: &mut ArrowWriter<W>,
    ) -> io::Result<()> {
        for batch in self.chromatogram_buffers.drain() {
            self.chromatogram_data_point_counter += batch.num_rows() as u64;
            writer.write(&batch)?;
        }
        Ok(())
    }

    fn flush_spectrum_metadata_records(&mut self) -> io::Result<()> {
        let batch = RecordBatch::from(self.spectrum_metadata_buffer.finish().as_struct());
        self.spectrum_metadata_writer.write(&batch)?;
        Ok(())
    }

    fn flush_chromatogram_metadata_records<W: Write + Send>(
        &mut self,
        writer: &mut ArrowWriter<W>,
    ) -> io::Result<()> {
        let batch = RecordBatch::from(self.chromatogram_metadata_buffer.finish().as_struct());
        writer.write(&batch)?;
        Ok(())
    }

    pub fn finish(
        &mut self,
    ) -> Result<parquet::file::metadata::ParquetMetaData, parquet::errors::ParquetError> {
        self.flush_spectrum_data_arrays()?;
        self.flush_spectrum_metadata_records()?;
        self.append_metadata();
        self.append_key_value_metadata("spectrum_count", Some(self.spectrum_counter().to_string()));
        self.append_key_value_metadata(
            "spectrum_data_point_count",
            Some(self.spectrum_data_point_counter.to_string()),
        );
        self.spectrum_data_writer.finish()?;
        self.file_index.push(MzPeakArchiveType::SpectrumDataArrays.into());
        self.file_index.push(MzPeakArchiveType::SpectrumMetadata.into());
        if let Some(peak_file_writer) = self.separate_peak_writer.take() {
            let peak_file = peak_file_writer.finish()?;
            drop(peak_file);
            self.file_index.push(MzPeakArchiveType::SpectrumPeakDataArrays.into());
        }
        let meta = self.spectrum_metadata_writer.finish()?;
        if !self.chromatogram_metadata_buffer.is_empty() {
            let metadata_fields = self.chromatogram_metadata_buffer.schema();
            let mut writer = ArrowWriter::try_new_with_options(
                fs::File::create(
                    self.path
                        .join(MzPeakArchiveType::ChromatogramMetadata.tag_file_suffix()),
                )?,
                metadata_fields.clone(),
                ArrowWriterOptions::new()
                    .with_properties(Self::spectrum_metadata_writer_props(&metadata_fields, None)),
            )?;
            self.file_index.push(MzPeakArchiveType::ChromatogramMetadata.into());
            self.flush_chromatogram_metadata_records(&mut writer)?;
            self.append_key_value_metadata(
                "chromatogram_count",
                Some(self.chromatogram_counter().to_string()),
            );
            self.append_key_value_metadata(
                "chromatogram_data_point_count",
                Some(self.chromatogram_data_point_counter.to_string()),
            );
            writer.finish()?;

            let mut writer = ArrowWriter::try_new_with_options(
                fs::File::create(
                    self.path
                        .join(MzPeakArchiveType::ChromatogramDataArrays.tag_file_suffix()),
                )?,
                self.chromatogram_buffers.schema().clone(),
                ArrowWriterOptions::new().with_properties(Self::chromatogram_data_writer_props(
                    &self.chromatogram_buffers,
                    BufferContext::Chromatogram.index_field().name().to_string(),
                    &None,
                    self.compression,
                    None,
                )),
            )?;

            self.flush_chromatogram_data_records(&mut writer)?;
            let chromatogram_array_index: ArrayIndex = self.chromatogram_buffers.as_array_index();
            writer.append_key_value_metadata(KeyValue::new(
                "chromatogram_array_index".to_string(),
                Some(chromatogram_array_index.to_json()),
            ));
            writer.append_key_value_metadata(KeyValue::new(
                "chromatogram_data_point_count".into(),
                Some(self.chromatogram_data_point_counter.to_string()),
            ));
        }
        self.file_index.push(MzPeakArchiveType::ChromatogramDataArrays.into());
        Ok(meta)
    }
}

impl<C: CentroidLike + ToMzPeakDataSeries, D: DeconvolutedCentroidLike + ToMzPeakDataSeries>
    SpectrumWriter<C, D> for UnpackedMzPeakWriterType<C, D>
{
    fn write<S: SpectrumLike<C, D> + 'static>(&mut self, spectrum: &S) -> io::Result<usize> {
        self.write_spectrum(spectrum).map(|_| 1)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.spectrum_data_writer.flush()?;
        self.spectrum_metadata_writer.flush()?;
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        self.finish()?;
        Ok(())
    }
}
