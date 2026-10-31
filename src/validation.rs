use std::io;
use std::sync::Arc;

use parquet::{arrow::arrow_reader::ParquetRecordBatchReaderBuilder, file::reader::ChunkReader};
use sha2::{self, Digest};

use arrow::{
    array::{
        Array, ArrayRef, LargeBinaryArray, LargeStringArray, LargeStringBuilder, RecordBatch,
        UInt64Array, UInt64Builder,
    },
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
pub struct SegmentSignature {
    /// The name of the file the column chunk was read from
    pub filename: String,
    /// The index of the row group this column chunk belongs to
    pub row_group: Option<usize>,
    /// The index of the column within the row group
    pub column: Option<usize>,
    /// The dotted path of the column in the Parquet schema
    pub path: Option<String>,
    /// The byte offset of the first page of the chunk in the file
    pub offset: Option<u64>,
    /// The number of bytes in the chunk (total compressed size)
    pub length: Option<u64>,
    /// The SHA-512 digest of the chunk, or its signed form
    pub signature: Vec<u8>,
}

impl SegmentSignature {
    pub fn new(
        filename: String,
        row_group: Option<usize>,
        column: Option<usize>,
        path: Option<String>,
        offset: Option<u64>,
        length: Option<u64>,
        signature: Vec<u8>,
    ) -> Self {
        Self {
            filename,
            row_group,
            column,
            path,
            offset,
            length,
            signature,
        }
    }

    pub fn read_digest_from<R: io::Read + io::Seek>(&self, source: &mut R) -> io::Result<Vec<u8>> {
        if let Some(offset) = self.offset {
            source.seek(io::SeekFrom::Start(offset))?;
        }
        let mut read_length = if let Some(z) = self.length {
            z
        } else {
            let offset = source.stream_position()?;
            source.seek(io::SeekFrom::End(0))?;
            let z = source.stream_position()?;
            source.seek(io::SeekFrom::Start(offset))?;
            z
        };
        let mut chk = sha2::Sha512::new();
        let mut buffer = vec![0u8; 65536];
        while read_length > 0 {
            let upper = read_length.min(buffer.len() as u64) as usize;
            let z = source.read(&mut buffer[..upper])?;
            if z == 0 {
                break;
            }
            chk.update(&buffer[..z]);
            read_length -= z as u64;
        }
        Ok(chk.finalize().to_vec())
    }
}

/// Compute a SHA-512 checksum of every column chunk in every row group of a Parquet file
///
/// # Note
/// `filename` should be the file's name relative to the root of the archive, without
///  any leading slashes.
pub fn checksum_parquet_segments<T: ChunkReader + 'static>(
    reader: &'static T,
    filename: &str,
) -> io::Result<Vec<SegmentSignature>>
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
            checksums.push(SegmentSignature::new(
                filename.to_string(),
                Some(row_group),
                Some(column),
                Some(col.column_path().string()),
                Some(offset),
                Some(length),
                sha2::Sha512::digest(&blob).to_vec(),
            ));
        }
    }
    Ok(checksums)
}

/// The Arrow schema used to store [`SegmentSignature`] records
pub fn segment_signature_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("filename", DataType::LargeUtf8, false),
        Field::new("row_group", DataType::UInt64, false),
        Field::new("column", DataType::UInt64, false),
        Field::new("path", DataType::LargeUtf8, false),
        Field::new("offset", DataType::UInt64, false),
        Field::new("length", DataType::UInt64, false),
        Field::new("signature", DataType::LargeBinary, false),
    ]))
}

/// Convert a slice of [`SegmentSignature`] into a [`RecordBatch`] with the
/// schema from [`segment_signature_schema`], suitable for writing to Parquet.
pub fn segment_signature_to_record_batch(checksums: &[SegmentSignature]) -> RecordBatch {
    let filenames =
        LargeStringArray::from_iter_values(checksums.iter().map(|c| c.filename.as_str()));
    let signatures =
        LargeBinaryArray::from_iter_values(checksums.iter().map(|c| c.signature.as_slice()));

    let mut row_groups = UInt64Builder::new();
    let mut columns = UInt64Builder::new();
    let mut paths = LargeStringBuilder::new();
    let mut offsets = UInt64Builder::new();
    let mut lengths = UInt64Builder::new();
    for seg in checksums {
        row_groups.append_option(seg.row_group.map(|v| v as u64));
        columns.append_option(seg.column.map(|v| v as u64));
        offsets.append_option(seg.offset.map(|v| v as u64));
        lengths.append_option(seg.length.map(|v| v as u64));
        paths.append_option(seg.path.as_deref());
    }
    RecordBatch::try_new(
        segment_signature_schema(),
        vec![
            Arc::new(filenames) as ArrayRef,
            Arc::new(row_groups.finish()),
            Arc::new(columns.finish()),
            Arc::new(paths.finish()),
            Arc::new(offsets.finish()),
            Arc::new(lengths.finish()),
            Arc::new(signatures),
        ],
    )
    .unwrap()
}

/// Convert a [`RecordBatch`] produced by [`segment_signature_to_record_batch`]
/// (or read back from a Parquet file holding it) into [`SegmentSignature`] records.
///
/// Columns are looked up by name, and an error is returned if one is missing,
/// has the wrong type, or contains nulls.
pub fn segment_signature_from_record_batch(
    batch: &RecordBatch,
) -> io::Result<Vec<SegmentSignature>> {
    fn column<'a, A: 'static>(batch: &'a RecordBatch, name: &str) -> io::Result<&'a A> {
        let array = batch.column_by_name(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Missing column {name:?}"),
            )
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
    let signatures = column::<LargeBinaryArray>(batch, "signature")?;

    Ok((0..batch.num_rows())
        .map(|i| SegmentSignature {
            filename: filenames.value(i).to_string(),
            row_group: row_groups.is_valid(i).then(|| row_groups.value(i) as usize),
            column: columns.is_valid(i).then(|| columns.value(i) as usize),
            path: paths.is_valid(i).then(|| paths.value(i).to_string()),
            offset: offsets.is_valid(i).then(|| offsets.value(i)),
            length: lengths.is_valid(i).then(|| lengths.value(i)),
            signature: signatures.value(i).to_vec(),
        })
        .collect())
}

#[cfg(feature = "async")]
mod async_impl {
    use super::*;
    use parquet::arrow::async_reader::AsyncFileReader;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

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

    impl SegmentSignature {
        pub async fn read_digest_from_async<R: AsyncReadExt + AsyncSeekExt + Unpin>(&self, source: &mut R) -> io::Result<Vec<u8>> {
            if let Some(offset) = self.offset {
                source.seek(io::SeekFrom::Start(offset)).await?;
            }
            let mut read_length = if let Some(z) = self.length {
                z
            } else {
                let offset = source.stream_position().await?;
                source.seek(io::SeekFrom::End(0)).await?;
                let z = source.stream_position().await?;
                source.seek(io::SeekFrom::Start(offset)).await?;
                z
            };
            let mut chk = sha2::Sha512::new();
            let mut buffer = vec![0u8; 65536];
            while read_length > 0 {
                let upper = read_length.min(buffer.len() as u64) as usize;
                let z = source.read(&mut buffer[..upper]).await?;
                if z == 0 {
                    break;
                }
                chk.update(&buffer[..z]);
                read_length -= z as u64;
            }
            Ok(chk.finalize().to_vec())
        }
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
    ) -> io::Result<Vec<SegmentSignature>> {
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
                checksums.push(SegmentSignature::new(
                    filename.to_string(),
                    Some(row_group),
                    Some(column),
                    Some(col.column_path().string()),
                    Some(offset),
                    Some(length),
                    sha2::Sha512::digest(&blob).to_vec(),
                ));
            }
        }
        Ok(checksums)
    }
}
#[cfg(feature = "async")]
pub use async_impl::{checksum_parquet_segments_async, checksum_stream_async};
