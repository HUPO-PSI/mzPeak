//! Read and atomically rewrite the Parquet signature table in a packed archive.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, LargeBinaryArray, LargeStringArray, StringArray, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use parquet::file::reader::{FileReader, SerializedFileReader};
use sha2::{Digest, Sha512};
use zip::ZipArchive;

use super::conversion::{FileIdentity, file_identity, linked, matches_snapshot};
use super::statement::Member;
use super::{Error, MEMBER, ROOT_INDEX, Result, invalid, validate_path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Row {
    pub index: u64,
    pub filename: String,
    pub row_group: Option<u64>,
    pub column: Option<u64>,
    pub path: Option<String>,
    pub offset: Option<u64>,
    pub length: Option<u64>,
    pub signature: Vec<u8>,
}
impl Row {
    pub fn glob(&self) -> bool {
        self.filename == "*"
    }

    pub fn descriptor(&self, size: u64) -> Member {
        Member {
            index: self.index,
            path: self.filename.clone(),
            size,
            row_group: self.row_group,
            column: self.column,
            column_path: self.path.clone(),
            offset: self.offset,
            length: self.length,
        }
    }
}

pub(super) struct Archive<'a> {
    pub contents: BTreeMap<String, (usize, u64)>,
    zip: ZipArchive<File>,
    source: File,
    initial: Metadata,
    identity: FileIdentity,
    path: &'a Path,
}

impl<'a> Archive<'a> {
    pub fn open(path: &'a Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || linked(&metadata) {
            return Err(invalid("provenance requires a regular packed archive"));
        }
        let source = File::open(path)?;
        let initial = source.metadata()?;
        let identity = file_identity(&source)?;
        let mut zip = open_archive_file(source.try_clone()?)?;
        let mut names = BTreeSet::new();
        let mut contents = BTreeMap::new();
        for index in 0..zip.len() {
            let entry = zip.by_index(index)?;
            let name = entry.name();
            let normalized = name.strip_suffix('/').unwrap_or(name);
            validate_path(normalized)?;
            let file_type = entry.unix_mode().unwrap_or(0) & 0o170000;
            let directory = name.ends_with('/');
            if !names.insert(normalized.to_ascii_lowercase())
                || normalized == "*"
                || !matches!(
                    (file_type, directory),
                    (0, _) | (0o100000, false) | (0o040000, true)
                )
                || directory && entry.size() != 0
                || normalized.eq_ignore_ascii_case(MEMBER) && name != MEMBER
            {
                return Err(invalid("ambiguous, aliased, or unsupported ZIP member"));
            }
            if !directory && name != MEMBER {
                contents.insert(name.to_owned(), (index, entry.size()));
            }
        }
        let &(index, _) = contents
            .get(ROOT_INDEX)
            .ok_or_else(|| invalid("missing root mzpeak_index.json"))?;
        let entry = zip.by_index(index)?;
        let root: crate::archive::FileIndex =
            serde_json::from_reader(entry).map_err(|_| invalid("invalid archive root index"))?;
        for entry in root.files {
            if entry
                .name
                .trim_end_matches('/')
                .eq_ignore_ascii_case(MEMBER)
                && (entry.name != MEMBER
                    || entry.data_kind != crate::archive::DataKind::Signature
                    || entry.entity_type != crate::archive::EntityType::Provenance)
            {
                return Err(invalid(
                    "root index assigns a data role or alias to the proof member",
                ));
            }
        }
        Ok(Self {
            contents,
            zip,
            source,
            initial,
            identity,
            path,
        })
    }

    pub fn rows(&mut self) -> Result<Vec<Row>> {
        let mut entry = match self.zip.by_name(MEMBER) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut parquet = tempfile::tempfile()?;
        io::copy(&mut entry, &mut parquet)?;
        Ok(read_rows(parquet)?)
    }

    pub fn digest(&mut self, member: &Member) -> Result<Option<[u8; 64]>> {
        let Some(&(index, size)) = self.contents.get(&member.path) else {
            return Ok(None);
        };
        if size != member.size {
            return Ok(None);
        }
        let mut entry = self.zip.by_index(index)?;
        let mut hash = Sha512::new();
        if let Some(offset) = member.offset {
            let skipped = io::copy(&mut (&mut entry).take(offset), &mut io::sink())?;
            if skipped != offset {
                return Err(invalid("short read before signed range"));
            }
            hash.update(offset.to_le_bytes());
        }
        let length = member.length.unwrap_or(size - member.offset.unwrap_or(0));
        if let Some(explicit) = member.length {
            hash.update(explicit.to_le_bytes());
        }
        let mut reader = (&mut entry).take(length);
        let mut buffer = [0; 65536];
        while reader.limit() > 0 {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Err(invalid("short read in signed member"));
            }
            hash.update(&buffer[..read]);
        }
        if member.length.is_none() && entry.read(&mut [0])? != 0 {
            return Err(invalid("incorrect ZIP member size"));
        }
        Ok(Some(hash.finalize().into()))
    }

    pub fn parquet_ranges(&mut self, name: &str) -> Result<Vec<Member>> {
        let &(index, size) = self
            .contents
            .get(name)
            .ok_or_else(|| invalid("Parquet member is absent"))?;
        let mut bytes = tempfile::tempfile()?;
        io::copy(&mut self.zip.by_index(index)?, &mut bytes)?;
        let reader = SerializedFileReader::new(bytes)?;
        let mut chunks = Vec::new();
        for (group, meta) in reader.metadata().row_groups().iter().enumerate() {
            for (column, info) in meta.columns().iter().enumerate() {
                let offset = info
                    .dictionary_page_offset()
                    .unwrap_or_else(|| info.data_page_offset());
                let length = info.compressed_size();
                let member = Member {
                    index: 0,
                    path: name.to_owned(),
                    size,
                    row_group: Some(group as u64),
                    column: Some(column as u64),
                    column_path: Some(info.column_path().string()),
                    offset: Some(
                        offset
                            .try_into()
                            .map_err(|_| invalid("negative Parquet chunk offset"))?,
                    ),
                    length: Some(
                        length
                            .try_into()
                            .map_err(|_| invalid("negative Parquet chunk length"))?,
                    ),
                };
                member.validate()?;
                chunks.push(member);
            }
        }
        Ok(chunks)
    }

    pub fn stable(&self) -> Result<()> {
        if !matches_snapshot(self.path, &self.initial, self.identity)? {
            return Err(invalid("archive changed during provenance operation"));
        }
        Ok(())
    }

    pub fn rewrite(mut self, rows: &[Row]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let mut table = tempfile::tempfile()?;
        write_rows(&mut table, rows)?;
        table.rewind()?;
        {
            let mut zip = zip::ZipWriter::new(temp.as_file_mut());
            zip.set_raw_comment(self.zip.comment().to_vec().into());
            for index in 0..self.zip.len() {
                let mut entry = self.zip.by_index(index)?;
                if entry.name() != MEMBER {
                    io::copy(&mut entry, &mut io::sink())?;
                    drop(entry);
                    zip.raw_copy_file(self.zip.by_index(index)?)?;
                }
            }
            zip.start_file(
                MEMBER,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .large_file(true),
            )?;
            io::copy(&mut table, &mut zip)?;
            zip.finish()?;
        }
        fs::set_permissions(temp.path(), self.initial.permissions())?;
        temp.as_file().sync_all()?;
        self.stable()?;
        drop(self.zip);
        drop(self.source);
        #[cfg(unix)]
        let directory = File::open(parent)?;
        temp.persist(self.path).map_err(|error| error.error)?;
        #[cfg(unix)]
        directory.sync_all().map_err(Error::PostCommitSync)?;
        Ok(())
    }
}

fn read_rows(file: File) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for batch in ParquetRecordBatchReaderBuilder::try_new(file)?.build()? {
        let batch = batch?;
        let fields = batch.schema();
        for field in [
            "index",
            "filename",
            "row_group",
            "column",
            "path",
            "offset",
            "length",
            "signature",
        ] {
            if fields.fields().iter().filter(|f| f.name() == field).count() != 1 {
                return Err(invalid(format!(
                    "provenance requires exactly one {field} column"
                )));
            }
        }
        let col = |name: &str| batch.column_by_name(name).expect("validated field");
        let number = |name: &str, row: usize| -> Result<Option<u64>> {
            let array = col(name)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| invalid(format!("provenance column {name} must be uint64")))?;
            Ok(array.is_valid(row).then(|| array.value(row)))
        };
        let text = |name: &str, row: usize| -> Result<Option<String>> {
            let raw = col(name);
            if let Some(array) = raw.as_any().downcast_ref::<LargeStringArray>() {
                Ok(array.is_valid(row).then(|| array.value(row).to_owned()))
            } else if let Some(array) = raw.as_any().downcast_ref::<StringArray>() {
                Ok(array.is_valid(row).then(|| array.value(row).to_owned()))
            } else {
                Err(invalid(format!(
                    "provenance column {name} must be a string"
                )))
            }
        };
        for row in 0..batch.num_rows() {
            let raw = col("signature");
            let signature = if let Some(array) = raw.as_any().downcast_ref::<LargeBinaryArray>() {
                array.is_valid(row).then(|| array.value(row))
            } else if let Some(array) = raw.as_any().downcast_ref::<BinaryArray>() {
                array.is_valid(row).then(|| array.value(row))
            } else {
                None
            }
            .ok_or_else(|| invalid("provenance signature must be non-null binary"))?
            .to_vec();
            let index =
                number("index", row)?.ok_or_else(|| invalid("provenance index must be present"))?;
            if index != rows.len() as u64 {
                return Err(invalid(
                    "provenance rows must have contiguous indices in table order",
                ));
            }
            let row = Row {
                index,
                filename: text("filename", row)?.ok_or_else(|| invalid("missing row filename"))?,
                row_group: number("row_group", row)?,
                column: number("column", row)?,
                path: text("path", row)?,
                offset: number("offset", row)?,
                length: number("length", row)?,
                signature,
            };
            if row.glob() && !row.descriptor(0).whole() {
                return Err(invalid("glob rows must have null coordinates and range"));
            }
            rows.push(row);
        }
    }
    if rows.last().is_none_or(|row| !row.glob()) {
        return Err(invalid("provenance table must end at a glob"));
    }
    Ok(rows)
}

fn write_rows(file: &mut File, rows: &[Row]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("index", DataType::UInt64, false),
        Field::new("filename", DataType::LargeUtf8, false),
        Field::new("row_group", DataType::UInt64, true),
        Field::new("column", DataType::UInt64, true),
        Field::new("path", DataType::LargeUtf8, true),
        Field::new("offset", DataType::UInt64, true),
        Field::new("length", DataType::UInt64, true),
        Field::new("signature", DataType::LargeBinary, false),
    ]));
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|row| row.index),
        )),
        Arc::new(LargeStringArray::from_iter_values(
            rows.iter().map(|row| &row.filename),
        )),
        Arc::new(UInt64Array::from_iter(rows.iter().map(|row| row.row_group))),
        Arc::new(UInt64Array::from_iter(rows.iter().map(|row| row.column))),
        Arc::new(LargeStringArray::from_iter(
            rows.iter().map(|row| row.path.as_deref()),
        )),
        Arc::new(UInt64Array::from_iter(rows.iter().map(|row| row.offset))),
        Arc::new(UInt64Array::from_iter(rows.iter().map(|row| row.length))),
        Arc::new(LargeBinaryArray::from_iter_values(
            rows.iter().map(|row| &row.signature),
        )),
    ];
    let mut writer = ArrowWriter::try_new(file, schema.clone(), None)?;
    writer.write(&RecordBatch::try_new(schema, arrays)?)?;
    writer.close()?;
    Ok(())
}

// zip 4.x stores entries in a name-indexed map and silently collapses exact
// duplicates. Inspect central-directory records before relying on that map. The
// library still performs decompression, CRC and ZIP64 handling; this small guard
// only preserves inventory/name identity that would otherwise be lost.
fn open_archive_file(mut file: File) -> Result<ZipArchive<File>> {
    let mut archive = ZipArchive::new(file.try_clone()?)?;
    let file_size = file.metadata()?.len();
    file.seek(SeekFrom::Start(archive.central_directory_start()))?;
    let mut entries = BTreeMap::new();
    loop {
        let position = file.stream_position()?;
        let mut signature = [0_u8; 4];
        file.read_exact(&mut signature)?;
        if matches!(
            signature,
            [0x50, 0x4b, 0x05, 0x06] | [0x50, 0x4b, 0x06, 0x06] | [0x50, 0x4b, 0x05, 0x05]
        ) {
            break;
        }
        if signature != [0x50, 0x4b, 0x01, 0x02] {
            return Err(invalid("unexpected ZIP central-directory record"));
        }
        let mut header = [0_u8; 42];
        file.read_exact(&mut header)?;
        let name_length = u16::from_le_bytes([header[24], header[25]]) as usize;
        let extra_length = u16::from_le_bytes([header[26], header[27]]) as u64;
        let comment_length = u16::from_le_bytes([header[28], header[29]]) as u64;
        let mut name = vec![0; name_length];
        file.read_exact(&mut name)?;
        let next = file
            .stream_position()?
            .checked_add(extra_length + comment_length)
            .ok_or_else(|| invalid("ZIP directory length overflow"))?;
        if next > file_size {
            return Err(invalid("truncated ZIP directory"));
        }
        entries.insert(position, name);
        file.seek(SeekFrom::Start(next))?;
    }
    if entries.len() != archive.len() {
        return Err(invalid("duplicate ZIP member name"));
    }
    for index in 0..archive.len() {
        let item = archive.by_index(index)?;
        let central_name = entries
            .get(&item.central_header_start())
            .ok_or_else(|| invalid("inconsistent ZIP central-directory position"))?;
        if central_name != item.name().as_bytes() {
            return Err(invalid("alternate ZIP filename encoding is unsupported"));
        }
        file.seek(SeekFrom::Start(item.header_start()))?;
        let mut header = [0_u8; 30];
        file.read_exact(&mut header)?;
        if header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(invalid("invalid ZIP local header"));
        }
        let name_length = u16::from_le_bytes([header[26], header[27]]) as usize;
        let mut local_name = vec![0; name_length];
        file.read_exact(&mut local_name)?;
        if &local_name != central_name {
            return Err(invalid("local and central ZIP names differ"));
        }
    }
    Ok(archive)
}
