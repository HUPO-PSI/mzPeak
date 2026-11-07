use std::io;
use std::sync::Arc;

use parquet::{arrow::arrow_reader::ParquetRecordBatchReaderBuilder, file::reader::ChunkReader};
use sha2::{self, Digest};

use arrow::{
    array::{
        Array, ArrayRef, LargeBinaryArray, LargeStringArray, LargeStringBuilder,
        RecordBatch, UInt64Array, UInt64Builder,
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
        let written = self.stream.write(buf)?;
        self.hasher.update(&buf[..written]);
        if let Some(s) = self.salted_hasher.as_mut() {
            s.update(&buf[..written]);
        }
        Ok(written)
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
    /// In-memory completion state; never stored or used as a trust decision.
    pub signed: bool,
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
        signed: bool,
    ) -> Self {
        Self {
            filename,
            row_group,
            column,
            path,
            offset,
            length,
            signature,
            signed,
        }
    }

    pub fn read_digest_from<R: io::Read + io::Seek>(&self, source: &mut R) -> io::Result<Vec<u8>> {
        if let Some(offset) = self.offset {
            source.seek(io::SeekFrom::Start(offset))?;
        }
        let offset = source.stream_position()?;
        let end = source.seek(io::SeekFrom::End(0))?;
        source.seek(io::SeekFrom::Start(offset))?;
        let remaining = end.checked_sub(offset).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "segment starts beyond the stream",
            )
        })?;
        let mut read_length = self.length.unwrap_or(remaining);
        if read_length > remaining {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "segment extends beyond the stream",
            ));
        }
        let mut chk = sha2::Sha512::new();
        let mut buffer = vec![0u8; 65536];
        while read_length > 0 {
            let upper = read_length.min(buffer.len() as u64) as usize;
            let z = source.read(&mut buffer[..upper])?;
            if z == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete segment",
                ));
            }
            chk.update(&buffer[..z]);
            read_length -= z as u64;
        }
        Ok(chk.finalize().to_vec())
    }
}

#[cfg(feature = "verification")]
mod verify {
    use std::ops::Not;

    use super::*;
    use cms::{
        cert::{
            CertificateChoices,
            x509::{
                attr::{Attribute, AttributeValue},
                der::{
                    Decode, Encode,
                    asn1::{OctetString, SetOfVec, UtcTime},
                },
            },
        },
        signed_data::{SignedData, SignerIdentifier},
    };
    use const_oid::db::{rfc5911, rfc5912, rfc8410};
    use ed25519_dalek::{VerifyingKey, pkcs8::DecodePublicKey};

    /// Cryptographic integrity of a legacy envelope, NOT certificate trust or coverage.
    /// The optional time is a signer assertion, not a trusted timestamp.
    #[derive(Debug, Clone)]
    pub enum VerificationStatus {
        VerifiedSignature(CertificateChoices, Option<UtcTime>),
        Invalid,
    }

    impl VerificationStatus {
        pub fn is_valid(&self) -> bool {
            matches!(self, Self::VerifiedSignature(_, _))
        }
    }

    impl Not for VerificationStatus {
        type Output = bool;
        fn not(self) -> bool {
            !self.is_valid()
        }
    }

    #[derive(Debug, Clone, thiserror::Error)]
    pub enum VerificationError {
        #[error("DER decoding error: {0}")]
        DerError(
            #[source]
            #[from]
            cms::cert::x509::der::Error,
        ),
        #[error("incorrect digest algorithm: {0:?}")]
        IncorrectDigestAlgorithm(const_oid::ObjectIdentifier),
        #[error("incorrect signing algorithm: {0:?}")]
        IncorrectSignatureAlgorithm(const_oid::ObjectIdentifier),
        #[error("incorrect message content type: {0:?}")]
        IncorrectContentType(const_oid::ObjectIdentifier),
        #[error("expected exactly one signer")]
        MissingSigner,
        #[error("expected exactly one matching signer certificate")]
        MissingCertificate,
        #[error("signed attributes are required")]
        MissingSignedAttributes,
        #[error("expected one value for signed attribute {0:?}")]
        MissingSignedAttribute(const_oid::ObjectIdentifier),
        #[error("cannot verify an unsigned segment")]
        Unsigned,
        #[error("malformed legacy signature: {0}")]
        Malformed(&'static str),
        #[error("invalid Ed25519 key or signature")]
        InvalidSignature,
    }

    fn single_attr(
        attrs: &SetOfVec<Attribute>,
        oid: const_oid::ObjectIdentifier,
    ) -> Result<Option<&AttributeValue>, VerificationError> {
        let mut matching = attrs.iter().filter(|attr| attr.oid == oid);
        let Some(attr) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() || attr.values.len() != 1 {
            return Err(VerificationError::MissingSignedAttribute(oid));
        }
        Ok(attr.values.get(0))
    }

    impl SegmentSignature {
        /// Verify legacy CMS integrity only. Identity and coordinates remain unauthenticated.
        pub fn verify_against(
            &self,
            digest: &[u8],
        ) -> Result<VerificationStatus, VerificationError> {
            if !self.signed {
                return Err(VerificationError::Unsigned);
            }
            if digest.len() != 64 {
                return Err(VerificationError::Malformed(
                    "SHA-512 digest must be 64 bytes",
                ));
            }
            let outer = cms::content_info::ContentInfo::from_der(&self.signature)?;
            if outer.content_type != rfc5911::ID_SIGNED_DATA {
                return Err(VerificationError::IncorrectContentType(outer.content_type));
            }
            let content = outer.content.decode_as::<SignedData>()?;
            if content.encap_content_info.econtent_type != rfc5911::ID_DATA {
                return Err(VerificationError::IncorrectContentType(
                    content.encap_content_info.econtent_type,
                ));
            }
            if content.encap_content_info.econtent.is_some() {
                return Err(VerificationError::Malformed(
                    "expected a detached signature",
                ));
            }
            if content.signer_infos.as_ref().len() != 1 {
                return Err(VerificationError::MissingSigner);
            }
            let signer_info = content
                .signer_infos
                .as_ref()
                .get(0)
                .ok_or(VerificationError::MissingSigner)?;
            if content.digest_algorithms.len() != 1
                || content
                    .digest_algorithms
                    .get(0)
                    .is_none_or(|a| a.oid != rfc5912::ID_SHA_512)
            {
                return Err(VerificationError::Malformed(
                    "SignedData digest algorithms must be SHA-512 only",
                ));
            }
            if signer_info.digest_alg.oid != rfc5912::ID_SHA_512 {
                return Err(VerificationError::IncorrectDigestAlgorithm(
                    signer_info.digest_alg.oid,
                ));
            }
            if signer_info.signature_algorithm.oid != rfc8410::ID_ED_25519
                || signer_info.signature_algorithm.parameters.is_some()
            {
                return Err(VerificationError::IncorrectSignatureAlgorithm(
                    signer_info.signature_algorithm.oid,
                ));
            }
            let SignerIdentifier::IssuerAndSerialNumber(issuer_serial) = &signer_info.sid else {
                return Err(VerificationError::Malformed(
                    "unsupported signer identifier",
                ));
            };
            let certificates = content
                .certificates
                .as_ref()
                .ok_or(VerificationError::MissingCertificate)?;
            let mut matching = certificates
                .as_ref()
                .iter()
                .filter_map(|choice| match choice {
                    CertificateChoices::Certificate(cert)
                        if cert.tbs_certificate.issuer == issuer_serial.issuer
                            && cert.tbs_certificate.serial_number
                                == issuer_serial.serial_number =>
                    {
                        Some(cert)
                    }
                    _ => None,
                });
            let cert = matching
                .next()
                .ok_or(VerificationError::MissingCertificate)?;
            if matching.next().is_some() {
                return Err(VerificationError::MissingCertificate);
            }
            let key = VerifyingKey::from_public_key_der(
                &cert.tbs_certificate.subject_public_key_info.to_der()?,
            )
            .map_err(|_| VerificationError::InvalidSignature)?;
            let signed_attrs = signer_info
                .signed_attrs
                .as_ref()
                .ok_or(VerificationError::MissingSignedAttributes)?;
            // DER parsing rejects non-canonical encodings. SignedAttributes encodes as SET OF here.
            let bytes = signed_attrs.to_der()?;
            let signature = ed25519::Signature::from_slice(signer_info.signature.as_bytes())
                .map_err(|_| VerificationError::InvalidSignature)?;
            key.verify_strict(&bytes, &signature)
                .map_err(|_| VerificationError::InvalidSignature)?;
            let content_type = single_attr(signed_attrs, rfc5911::ID_CONTENT_TYPE)?
                .ok_or(VerificationError::MissingSignedAttribute(
                    rfc5911::ID_CONTENT_TYPE,
                ))?
                .decode_as::<const_oid::ObjectIdentifier>()?;
            if content_type != rfc5911::ID_DATA {
                return Err(VerificationError::IncorrectContentType(content_type));
            }
            let message_digest = single_attr(signed_attrs, rfc5911::ID_MESSAGE_DIGEST)?
                .ok_or(VerificationError::MissingSignedAttribute(
                    rfc5911::ID_MESSAGE_DIGEST,
                ))?
                .decode_as::<OctetString>()?;
            if digest != message_digest.as_bytes() {
                return Ok(VerificationStatus::Invalid);
            }
            let signing_time = single_attr(signed_attrs, rfc5911::ID_SIGNING_TIME)?
                .map(|value| value.decode_as::<UtcTime>())
                .transpose()?;
            Ok(VerificationStatus::VerifiedSignature(
                CertificateChoices::Certificate(cert.clone()),
                signing_time,
            ))
        }
    }
}
#[cfg(feature = "verification")]
pub use verify::{VerificationError, VerificationStatus};

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
                false,
            ));
        }
    }
    Ok(checksums)
}

/// The Arrow schema used to store completed [`SegmentSignature`] records.
/// Schema revision 1 is documented in `doc/provenance-signatures.md`.
pub fn segment_signature_schema() -> Arc<Schema> {
    Arc::new(
        Schema::new(vec![
            Field::new("filename", DataType::LargeUtf8, false),
            Field::new("row_group", DataType::UInt64, true),
            Field::new("column", DataType::UInt64, true),
            Field::new("path", DataType::LargeUtf8, true),
            Field::new("offset", DataType::UInt64, true),
            Field::new("length", DataType::UInt64, true),
            Field::new("signature", DataType::LargeBinary, false),
        ])
        .with_metadata(std::collections::HashMap::from([(
            "mzpeak.provenance".into(),
            "1".into(),
        )])),
    )
}

/// Parse a complete CMS SignedData payload without asserting validity or trust.
fn parse_cms_signature(signature: &[u8]) -> io::Result<()> {
    use cms::{
        cert::x509::der::{Decode, asn1::ObjectIdentifier},
        content_info::ContentInfo,
        signed_data::SignedData,
    };

    let invalid = |error| io::Error::new(io::ErrorKind::InvalidData, error);
    let content = ContentInfo::from_der(signature).map_err(invalid)?;
    if content.content_type != ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected CMS SignedData",
        ));
    }
    let signed_data = content.content.decode_as::<SignedData>().map_err(invalid)?;
    if signed_data.signer_infos.as_ref().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected at least one CMS signer",
        ));
    }
    Ok(())
}

/// Store completed signatures using [`segment_signature_schema`].
/// Unsigned records and malformed CMS payloads return an error. Structural
/// parsing does not establish cryptographic validity or trust.
pub fn segment_signature_to_record_batch(
    checksums: &[SegmentSignature],
) -> io::Result<RecordBatch> {
    for record in checksums {
        if !record.signed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cannot serialize an unsigned segment",
            ));
        }
        parse_cms_signature(&record.signature)?;
    }
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
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Convert a [`RecordBatch`] produced by [`segment_signature_to_record_batch`]
/// (or read back from a Parquet file holding it) into [`SegmentSignature`] records.
///
/// Columns are looked up by name, and an error is returned if one is missing,
/// has the wrong type, or a required field contains nulls. Every payload must parse
/// as complete CMS SignedData before restoring its in-memory `signed` state.
/// This does not establish cryptographic validity or trust. Unmarked tables are
/// accepted under the same rules; unsupported explicit schema revisions are rejected.
pub fn segment_signature_from_record_batch(
    batch: &RecordBatch,
) -> io::Result<Vec<SegmentSignature>> {
    fn column<'a, A: 'static>(
        batch: &'a RecordBatch,
        name: &str,
        nullable: bool,
    ) -> io::Result<&'a A> {
        let array = batch.column_by_name(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Missing column {name:?}"),
            )
        })?;
        if !nullable && array.null_count() > 0 {
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

    if let Some(revision) = batch.schema().metadata().get("mzpeak.provenance") {
        if revision != "1" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported provenance schema revision {revision:?}"),
            ));
        }
    }
    let names = batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect::<std::collections::HashSet<_>>();
    if names.len() != batch.num_columns() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate column names",
        ));
    }
    let filenames = column::<LargeStringArray>(batch, "filename", false)?;
    let row_groups = column::<UInt64Array>(batch, "row_group", true)?;
    let columns = column::<UInt64Array>(batch, "column", true)?;
    let paths = column::<LargeStringArray>(batch, "path", true)?;
    let offsets = column::<UInt64Array>(batch, "offset", true)?;
    let lengths = column::<UInt64Array>(batch, "length", true)?;
    let signatures = column::<LargeBinaryArray>(batch, "signature", false)?;

    if row_groups
        .iter()
        .chain(columns.iter())
        .flatten()
        .any(|value| usize::try_from(value).is_err())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "coordinate does not fit this platform",
        ));
    }
    (0..batch.num_rows())
        .map(|i| {
            parse_cms_signature(signatures.value(i))?;
            Ok(SegmentSignature {
                filename: filenames.value(i).to_string(),
                row_group: row_groups.is_valid(i).then(|| row_groups.value(i) as usize),
                column: columns.is_valid(i).then(|| columns.value(i) as usize),
                path: paths.is_valid(i).then(|| paths.value(i).to_string()),
                offset: offsets.is_valid(i).then(|| offsets.value(i)),
                length: lengths.is_valid(i).then(|| lengths.value(i)),
                signature: signatures.value(i).to_vec(),
                signed: true,
            })
        })
        .collect()
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
        pub async fn read_digest_from_async<R: AsyncReadExt + AsyncSeekExt + Unpin>(
            &self,
            source: &mut R,
        ) -> io::Result<Vec<u8>> {
            if let Some(offset) = self.offset {
                source.seek(io::SeekFrom::Start(offset)).await?;
            }
            let offset = source.stream_position().await?;
            let end = source.seek(io::SeekFrom::End(0)).await?;
            source.seek(io::SeekFrom::Start(offset)).await?;
            let remaining = end.checked_sub(offset).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "segment starts beyond the stream",
                )
            })?;
            let mut read_length = self.length.unwrap_or(remaining);
            if read_length > remaining {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "segment extends beyond the stream",
                ));
            }
            let mut chk = sha2::Sha512::new();
            let mut buffer = vec![0u8; 65536];
            while read_length > 0 {
                let upper = read_length.min(buffer.len() as u64) as usize;
                let z = source.read(&mut buffer[..upper]).await?;
                if z == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete segment",
                    ));
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
                let blob = reader
                    .get_bytes(
                        offset..offset.checked_add(length).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "segment range overflows")
                        })?,
                    )
                    .await?;
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
                    false,
                ));
            }
        }
        Ok(checksums)
    }
}
#[cfg(feature = "async")]
pub use async_impl::{checksum_parquet_segments_async, checksum_stream_async};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    fn segment(offset: Option<u64>, length: Option<u64>) -> SegmentSignature {
        SegmentSignature {
            filename: "metadata.json".into(),
            offset,
            length,
            ..Default::default()
        }
    }

    #[test]
    fn digest_ranges_respect_zero_and_remaining_length() {
        let mut source = Cursor::new(b"abcdef");
        source.set_position(3);
        assert_eq!(
            segment(Some(0), Some(6))
                .read_digest_from(&mut source)
                .unwrap(),
            sha2::Sha512::digest(b"abcdef").to_vec()
        );
        assert_eq!(
            segment(Some(0), Some(0))
                .read_digest_from(&mut source)
                .unwrap(),
            sha2::Sha512::digest(b"").to_vec()
        );
        source.set_position(3);
        assert_eq!(
            segment(None, None).read_digest_from(&mut source).unwrap(),
            sha2::Sha512::digest(b"def").to_vec()
        );
        for (offset, length) in [(7, Some(0)), (4, Some(3)), (0, Some(7)), (7, None)] {
            assert_eq!(
                segment(Some(offset), length)
                    .read_digest_from(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[test]
    fn partial_reads_accumulate_and_early_eof_fails() {
        struct Partial {
            source: Cursor<Vec<u8>>,
            truncate: bool,
        }
        impl io::Read for Partial {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.truncate && self.source.position() >= 2 {
                    return Ok(0);
                }
                let length = buf.len().min(2);
                io::Read::read(&mut self.source, &mut buf[..length])
            }
        }
        impl io::Seek for Partial {
            fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
                self.source.seek(pos)
            }
        }
        let mut source = Partial {
            source: Cursor::new(b"abcdef".to_vec()),
            truncate: false,
        };
        assert_eq!(
            segment(Some(0), Some(6))
                .read_digest_from(&mut source)
                .unwrap(),
            sha2::Sha512::digest(b"abcdef").to_vec()
        );
        source.truncate = true;
        assert_eq!(
            segment(Some(0), Some(6))
                .read_digest_from(&mut source)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn hashing_writer_hashes_only_successfully_written_bytes() {
        struct PartialWriter {
            data: Vec<u8>,
            fail: bool,
        }
        impl io::Write for PartialWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                if self.fail {
                    return Err(io::Error::other("write failed"));
                }
                let count = data.len().min(2);
                self.data.extend_from_slice(&data[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = SHA512HashingStream::new_salted(
            PartialWriter {
                data: Vec::new(),
                fail: false,
            },
            b"salt",
        );
        writer.write_all(b"abcdef").unwrap();
        let expected = hex::encode(sha2::Sha512::digest(b"abcdef"));
        assert_eq!(writer.digest().digest, expected);
        assert_eq!(
            writer.digest().salted_digest,
            Some(hex::encode(sha2::Sha512::digest(b"saltabcdef")))
        );
        writer.get_mut().fail = true;
        assert!(writer.write(b"ignored").is_err());
        assert_eq!(writer.digest().digest, expected);
    }

    #[cfg(feature = "verification")]
    #[test]
    fn python_cms_fixture_verifies_and_malformed_signatures_fail() {
        use cms::{
            cert::x509::der::{Any, Decode, Encode, asn1::OctetString},
            content_info::ContentInfo,
            signed_data::SignedData,
        };
        // Produced by Python's SegmentSignature.sign_with using a fixed Ed25519 key.
        // It demonstrates interoperability, not certificate trust.
        let payload = hex::decode(concat!(
            "3082023306092a864886f70d010702a082022430820220020101310f300d06096086480165030402030500300b06092a8648",
            "86f70d010701a081e93081e6308199a00302010202017b300506032b6570301d311b301906035504030c124c656761637920",
            "74657374207369676e6572301e170d3235303130313030303030305a170d3237303130313030303030305a301d311b301906",
            "035504030c124c65676163792074657374207369676e6572302a300506032b657003210003a107bff3ce10be1d70dd18e74b",
            "c09967e4d6309ba50d5f1ddc8664125531b8300506032b65700341002734fbcbde8879b6781612d6f1e30a1c63a953430473",
            "3b7d845d1a7c970fc8d78e5b50ba4c1dac396c301b9db1dec1b35d8ab5f7e609226db484ac19432a4e0d3182010f3082010b",
            "0201013022301d311b301906035504030c124c65676163792074657374207369676e657202017b300d060960864801650304",
            "02030500a08189301806092a864886f70d010903310b06092a864886f70d010701301c06092a864886f70d010905310f170d",
            "3236313130373131313033305a304f06092a864886f70d01090431420440e32ef19623e8ed9d267f657a81944b3d07adbb76",
            "8518068e88435745564e8d4150a0a703be2a7d88b61e3d390c2bb97e2d4c311fdc69d6b1267f05f59aa920e7300506032b65",
            "7004406c66ba723a638f8f381f5deba1f1d63e34e317731363c8caba5a465a3039d514a137db2d5f0625e38614782800ce16",
            "2b6dcb4ca7d527ba94ab7fec211aeeb30a",
        )).unwrap();
        let record = SegmentSignature {
            signed: true,
            signature: payload,
            ..segment(None, None)
        };
        let digest = sha2::Sha512::digest(b"abcdef");
        assert!(record.verify_against(&digest).unwrap().is_valid());
        assert!(
            !record
                .verify_against(&sha2::Sha512::digest(b"tampered"))
                .unwrap()
                .is_valid()
        );
        let mut outer = ContentInfo::from_der(&record.signature).unwrap();
        let mut data = outer.content.decode_as::<SignedData>().unwrap();
        let mut signer = data.signer_infos.as_ref().get(0).unwrap().clone();
        signer.signature = OctetString::new(vec![0u8; 3]).unwrap();
        data.signer_infos = vec![signer].try_into().unwrap();
        outer.content = Any::encode_from(&data).unwrap();
        let broken = SegmentSignature {
            signature: outer.to_der().unwrap(),
            ..record
        };
        assert!(matches!(
            broken.verify_against(&digest),
            Err(VerificationError::InvalidSignature)
        ));
    }

    #[cfg(feature = "verification")]
    #[test]
    fn malformed_and_unsigned_cms_return_errors() {
        let unsigned = segment(None, None);
        assert!(matches!(
            unsigned.verify_against(&[0; 64]),
            Err(VerificationError::Unsigned)
        ));
        for payload in [b"".as_slice(), b"garbage", b"\x30\x00", b"\x30\x80"] {
            let record = SegmentSignature {
                signed: true,
                signature: payload.into(),
                ..unsigned.clone()
            };
            assert!(record.verify_against(&[0; 64]).is_err());
        }
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn async_ranges_match_sync_behavior() {
        let mut source = Cursor::new(b"abcdef".to_vec());
        source.set_position(3);
        assert_eq!(
            segment(Some(0), Some(6))
                .read_digest_from_async(&mut source)
                .await
                .unwrap(),
            sha2::Sha512::digest(b"abcdef").to_vec()
        );
        assert_eq!(
            segment(Some(0), Some(0))
                .read_digest_from_async(&mut source)
                .await
                .unwrap(),
            sha2::Sha512::digest(b"").to_vec()
        );
        source.set_position(3);
        assert_eq!(
            segment(None, None)
                .read_digest_from_async(&mut source)
                .await
                .unwrap(),
            sha2::Sha512::digest(b"def").to_vec()
        );
        for (offset, length) in [(7, Some(0)), (4, Some(3)), (0, Some(7)), (7, None)] {
            assert_eq!(
                segment(Some(offset), length)
                    .read_digest_from_async(&mut source)
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }
}
