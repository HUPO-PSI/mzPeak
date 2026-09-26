use std::io;
use std::sync::Arc;

use parquet::{
    arrow::arrow_reader::ParquetRecordBatchReaderBuilder,
    basic::ZstdLevel,
    encryption::encrypt::FileEncryptionProperties,
    file::{properties::WriterPropertiesBuilder, reader::ChunkReader},
};
use sha2::{self, Digest};

use crate::archive::{FileEntry, FileIndex};

use arrow::{
    array::{Array, ArrayRef, LargeStringArray, RecordBatch, UInt64Array},
    datatypes::{DataType, Field, Schema},
};

/// A helper that computes a SHA-512 checksum of a readable stream
pub fn checksum_stream<R: io::Read>(stream: &mut R) -> io::Result<String> {
    let mut context: sha2::Sha512 = sha2::Sha512::new();
    let mut buf = [0u8; 65536];
    loop {
        let z = stream.read(&mut buf)?;
        if z == 0 {
            break;
        }
        context.update(&buf[..z]);
    }
    Ok(hex::encode(context.finalize()))
}

/// A writable stream that keeps a running SHA-512 checksum of all bytes
#[derive(Clone)]
pub struct SHA512HashingStream<T> {
    pub stream: T,
    pub hasher: sha2::Sha512,
    pub salted_hasher: Option<sha2::Sha512>,
    salt: Option<Vec<u8>>,
}

#[derive(Debug, Default, Clone)]
pub struct DigestSummary {
    pub digest: String,
    pub salted_digest: Option<String>,
}

impl DigestSummary {
    pub fn new(digest: String, salted_digest: Option<String>) -> Self {
        Self {
            digest,
            salted_digest,
        }
    }
}

impl<T> SHA512HashingStream<T> {
    pub fn new(file: T) -> SHA512HashingStream<T> {
        Self {
            stream: file,
            hasher: sha2::Sha512::new(),
            salted_hasher: None,
            salt: None,
        }
    }

    pub fn has_salt(&self) -> bool {
        self.salt.is_some()
    }

    pub fn set_salt(&mut self, salt: &[u8]) {
        self.salted_hasher = Some(sha2::Sha512::new_with_prefix(salt));
        self.salt = Some(salt.to_vec());
    }

    pub fn new_salted(file: T, salt: &[u8]) -> SHA512HashingStream<T> {
        let mut this = Self::new(file);
        this.set_salt(salt);
        this
    }

    pub fn digest(&self) -> DigestSummary {
        let digest = hex::encode(self.hasher.clone().finalize());
        let salted = self
            .salted_hasher
            .clone()
            .map(|v| hex::encode(v.finalize()));
        DigestSummary::new(digest, salted)
    }

    pub fn hasher(&self) -> &sha2::Sha512 {
        &self.hasher
    }

    pub fn reset_hasher(&mut self) {
        self.hasher = sha2::Sha512::new();
        self.salted_hasher = self.salt.as_ref().map(|s| sha2::Sha512::new_with_prefix(s));
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.stream
    }

    pub fn into_inner(self) -> T {
        self.stream
    }
}

impl<T: io::Write> io::Write for SHA512HashingStream<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        if let Some(s) = self.salted_hasher.as_mut() {
            s.update(buf);
        }
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl<T: io::Seek + io::Write> io::Seek for SHA512HashingStream<T> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.stream.seek(pos)
    }
}

/// The SHA-512 checksum of the raw bytes of a single Parquet column chunk
/// (dictionary page, if any, plus all data pages), as laid out in the file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ColumnChunkChecksum {
    /// The name of the file the column chunk was read from
    pub filename: String,
    /// The index of the row group this column chunk belongs to
    pub row_group: usize,
    /// The index of the column within the row group
    pub column: usize,
    /// The dotted path of the column in the Parquet schema
    pub path: String,
    /// The byte offset of the first page of the chunk in the file
    pub offset: u64,
    /// The number of bytes in the chunk (total compressed size)
    pub length: u64,
    /// The hex-encoded SHA-512 digest of the chunk's bytes
    pub digest: String,
}

/// Compute a SHA-512 checksum of every column chunk in every row group of a Parquet file
///
/// # Note
/// `filename` should be the file's name relative to the root of the archive, without
///  any leading slashes.
pub fn checksum_parquet_segments<T: ChunkReader + 'static>(
    reader: &'static T,
    filename: &str,
) -> io::Result<Vec<ColumnChunkChecksum>>
where
    &'static T: ChunkReader + 'static,
{
    let builder = ParquetRecordBatchReaderBuilder::try_new(reader)?;
    let meta = builder.metadata();
    let mut checksums = Vec::new();
    for (row_group, rg) in meta.row_groups().iter().enumerate() {
        for (column, col) in rg.columns().iter().enumerate() {
            // `byte_range` starts at the dictionary page when present, else the first data page
            let (offset, length) = col.byte_range();
            let blob = reader.get_bytes(offset, length as usize)?;
            if blob.len() as u64 != length {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "Expected {length} bytes for row group {row_group} column {}, read {}",
                        col.column_path(),
                        blob.len()
                    ),
                ));
            }
            checksums.push(ColumnChunkChecksum {
                filename: filename.to_string(),
                row_group,
                column,
                path: col.column_path().string(),
                offset,
                length,
                digest: hex::encode(sha2::Sha512::digest(&blob)),
            });
        }
    }
    Ok(checksums)
}

/// The Arrow schema used to store [`ColumnChunkChecksum`] records
pub fn column_chunk_checksum_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("filename", DataType::LargeUtf8, false),
        Field::new("row_group", DataType::UInt64, false),
        Field::new("column", DataType::UInt64, false),
        Field::new("path", DataType::LargeUtf8, false),
        Field::new("offset", DataType::UInt64, false),
        Field::new("length", DataType::UInt64, false),
        Field::new("digest", DataType::LargeUtf8, false),
    ]))
}

/// Convert a slice of [`ColumnChunkChecksum`] into a [`RecordBatch`] with the
/// schema from [`column_chunk_checksum_schema`], suitable for writing to Parquet.
pub fn column_chunk_checksums_to_record_batch(checksums: &[ColumnChunkChecksum]) -> RecordBatch {
    let filenames = LargeStringArray::from_iter_values(checksums.iter().map(|c| c.filename.as_str()));
    let row_groups = UInt64Array::from_iter_values(checksums.iter().map(|c| c.row_group as u64));
    let columns = UInt64Array::from_iter_values(checksums.iter().map(|c| c.column as u64));
    let paths = LargeStringArray::from_iter_values(checksums.iter().map(|c| c.path.as_str()));
    let offsets = UInt64Array::from_iter_values(checksums.iter().map(|c| c.offset));
    let lengths = UInt64Array::from_iter_values(checksums.iter().map(|c| c.length));
    let digests = LargeStringArray::from_iter_values(checksums.iter().map(|c| c.digest.as_str()));

    RecordBatch::try_new(
        column_chunk_checksum_schema(),
        vec![
            Arc::new(filenames) as ArrayRef,
            Arc::new(row_groups),
            Arc::new(columns),
            Arc::new(paths),
            Arc::new(offsets),
            Arc::new(lengths),
            Arc::new(digests),
        ],
    )
    .unwrap()
}

/// Convert a [`RecordBatch`] produced by [`column_chunk_checksums_to_record_batch`]
/// (or read back from a Parquet file holding it) into [`ColumnChunkChecksum`] records.
///
/// Columns are looked up by name, and an error is returned if one is missing,
/// has the wrong type, or contains nulls.
pub fn column_chunk_checksums_from_record_batch(
    batch: &RecordBatch,
) -> io::Result<Vec<ColumnChunkChecksum>> {
    fn column<'a, A: 'static>(batch: &'a RecordBatch, name: &str) -> io::Result<&'a A> {
        let array = batch.column_by_name(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing column {name:?}"))
        })?;
        if array.null_count() > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Column {name:?} contains nulls"),
            ));
        }
        array.as_any().downcast_ref::<A>().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Column {name:?} has unexpected type {}", array.data_type()),
            )
        })
    }

    let filenames = column::<LargeStringArray>(batch, "filename")?;
    let row_groups = column::<UInt64Array>(batch, "row_group")?;
    let columns = column::<UInt64Array>(batch, "column")?;
    let paths = column::<LargeStringArray>(batch, "path")?;
    let offsets = column::<UInt64Array>(batch, "offset")?;
    let lengths = column::<UInt64Array>(batch, "length")?;
    let digests = column::<LargeStringArray>(batch, "digest")?;

    Ok((0..batch.num_rows())
        .map(|i| ColumnChunkChecksum {
            filename: filenames.value(i).to_string(),
            row_group: row_groups.value(i) as usize,
            column: columns.value(i) as usize,
            path: paths.value(i).to_string(),
            offset: offsets.value(i),
            length: lengths.value(i),
            digest: digests.value(i).to_string(),
        })
        .collect())
}

pub fn join_summaries_with_index<'a>(
    summaries: &'a [DigestSummary],
    file_index: &'a FileIndex,
) -> Vec<(&'a DigestSummary, &'a FileEntry)> {
    let mut joined_entries = Vec::new();
    for fe in file_index.iter() {
        if let Some(c) = fe.checksum.as_ref() {
            if let Some(s) = summaries.iter().find(|s| s.digest == *c) {
                joined_entries.push((s, fe))
            }
        }
    }
    joined_entries
}

pub fn build_provenance_table<'a>(
    mut entries: Vec<(&'a DigestSummary, &'a FileEntry)>,
) -> RecordBatch {
    let mut salts = Vec::new();
    let mut names = Vec::new();
    let mut digests = Vec::new();

    for (d, e) in entries.iter() {
        if d.salted_digest.is_none() {
            continue;
        }
        salts.push(d.salted_digest.clone().unwrap());
        names.push(e.name.clone());
        digests.push(d.digest.clone());
    }

    entries.sort_by(|a, b| a.0.digest.cmp(&b.0.digest));

    let salts = Arc::new(arrow::array::LargeStringArray::from_iter_values(salts));
    let names = Arc::new(arrow::array::LargeStringArray::from_iter_values(names));
    let digests = Arc::new(arrow::array::LargeStringArray::from_iter_values(digests));

    let schema = Arc::new(Schema::new(vec![
        Arc::new(Field::new("name", DataType::LargeUtf8, false)),
        Arc::new(Field::new("salted_digest", DataType::LargeUtf8, false)),
        Arc::new(Field::new("digest", DataType::LargeUtf8, false)),
    ]));

    RecordBatch::try_new(schema, vec![names as ArrayRef, salts, digests]).unwrap()
}

pub fn write_provenance_table<W: io::Write + Send>(
    stream: &mut W,
    provenance_table: RecordBatch,
    encryption_props: Arc<FileEncryptionProperties>,
) -> io::Result<()> {
    let props = WriterPropertiesBuilder::default()
        .set_compression(parquet::basic::Compression::ZSTD(ZstdLevel::default()))
        .with_file_encryption_properties(encryption_props)
        .build();
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(stream, provenance_table.schema(), Some(props))?;
    writer.write(&provenance_table)?;
    writer.finish()?;
    Ok(())
}

#[cfg(feature = "async")]
mod async_impl {
    use super::*;
    use parquet::arrow::async_reader::AsyncFileReader;

    /// A helper that computes a SHA-512 checksum of a readable asynchronous stream
    pub async fn checksum_stream_async<R: tokio::io::AsyncReadExt + Unpin>(
        stream: &mut R,
    ) -> io::Result<String> {
        let mut context: sha2::Sha512 = sha2::Sha512::new();
        let mut buf = [0u8; 65536];
        loop {
            let z = stream.read(&mut buf).await?;
            if z == 0 {
                break;
            }
            context.update(&buf[..z]);
        }
        Ok(hex::encode(context.finalize()))
    }

    /// Compute a SHA-512 checksum of every column chunk in every row group of a Parquet file
    /// read through an [`AsyncFileReader`]
    ///
    /// # Note
    /// `filename` should be the file's name relative to the root of the archive, without
    ///  any leading slashes.
    pub async fn checksum_parquet_segments_async<R: AsyncFileReader>(
        reader: &mut R,
        filename: &str,
    ) -> io::Result<Vec<ColumnChunkChecksum>> {
        let meta = reader.get_metadata(None).await?;
        let mut checksums = Vec::new();
        for (row_group, rg) in meta.row_groups().iter().enumerate() {
            for (column, col) in rg.columns().iter().enumerate() {
                // `byte_range` starts at the dictionary page when present, else the first data page
                let (offset, length) = col.byte_range();
                let blob = reader.get_bytes(offset..offset + length).await?;
                if blob.len() as u64 != length {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "Expected {length} bytes for row group {row_group} column {}, read {}",
                            col.column_path(),
                            blob.len()
                        ),
                    ));
                }
                checksums.push(ColumnChunkChecksum {
                    filename: filename.to_string(),
                    row_group,
                    column,
                    path: col.column_path().string(),
                    offset,
                    length,
                    digest: hex::encode(sha2::Sha512::digest(&blob)),
                });
            }
        }
        Ok(checksums)
    }
}
#[cfg(feature = "async")]
pub use async_impl::{checksum_parquet_segments_async, checksum_stream_async};
