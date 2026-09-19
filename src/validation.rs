use std::io;
use std::sync::Arc;

use parquet::{basic::ZstdLevel, encryption::encrypt::FileEncryptionProperties, file::properties::WriterPropertiesBuilder};
use sha2::{self, Digest};

use crate::archive::{FileEntry, FileIndex};

use arrow::{
    array::{ArrayRef, RecordBatch},
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

pub fn write_provenance_table<W: io::Write + Send>(stream: &mut W, provenance_table: RecordBatch, encryption_props: Arc<FileEncryptionProperties>) -> io::Result<()> {
    let props = WriterPropertiesBuilder::default()
        .set_compression(parquet::basic::Compression::ZSTD(ZstdLevel::default()))
        .with_file_encryption_properties(encryption_props).build();
    let mut writer = parquet::arrow::ArrowWriter::try_new(stream, provenance_table.schema(), Some(props))?;
    writer.write(&provenance_table)?;
    writer.finish()?;
    Ok(())
}

#[cfg(feature = "async")]
mod async_impl {
    use super::*;

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
}
#[cfg(feature = "async")]
pub use async_impl::checksum_stream_async;
