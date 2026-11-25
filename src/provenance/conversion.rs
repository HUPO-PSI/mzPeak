use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use der::Enumerated;
use der::asn1::OctetString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use super::statement::{ClaimedSource, ConversionStatement};
use super::{Result, digest_bytes, invalid, validate_path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub size: u64,
    pub sha512: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Enumerated)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum SourceResult {
    Passed = 0,
    Failed = 1,
    Unknown = 2,
}

/// A vendor attests that verification, capture and conversion used the same
/// stable dataset. Capture by itself does not perform vendor verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conversion {
    pub vendor: String,
    pub format: String,
    pub method: String,
    pub verifier: String,
    pub verifier_version: String,
    pub result: SourceResult,
    pub sources: Vec<SourceFile>,
    pub report_path: Option<String>,
}

impl Conversion {
    pub(super) fn encode(&self, report_row: Option<u64>) -> Result<ConversionStatement> {
        let mut sources = self
            .sources
            .iter()
            .map(|source| {
                Ok(ClaimedSource {
                    path: source.path.clone(),
                    size: source.size,
                    digest: OctetString::new(digest_bytes(&source.sha512)?.to_vec())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        sources.sort_by(|a, b| a.path.cmp(&b.path));
        let result = ConversionStatement {
            vendor: self.vendor.clone(),
            format: self.format.clone(),
            method: self.method.clone(),
            verifier: self.verifier.clone(),
            verifier_version: self.verifier_version.clone(),
            result: self.result,
            sources,
            report_row,
        };
        validate(&result)?;
        Ok(result)
    }
}

pub(super) fn validate(conversion: &ConversionStatement) -> Result<()> {
    if [
        &conversion.vendor,
        &conversion.format,
        &conversion.method,
        &conversion.verifier,
        &conversion.verifier_version,
    ]
    .iter()
    .any(|s| s.is_empty())
        || conversion.sources.is_empty()
        || conversion
            .sources
            .windows(2)
            .any(|pair| pair[0].path >= pair[1].path)
    {
        return Err(invalid("invalid vendor identity or source inventory"));
    }
    let mut folded = BTreeSet::new();
    for source in &conversion.sources {
        validate_path(&source.path)?;
        if source.digest.as_bytes().len() != 64 || !folded.insert(source.path.to_ascii_lowercase())
        {
            return Err(invalid("invalid source digest or case-aliased source path"));
        }
    }
    Ok(())
}

pub(super) fn verify_source(path: &Path, claim: &ConversionStatement) -> Result<bool> {
    let actual = capture_source(path)?;
    Ok(actual.len() == claim.sources.len()
        && actual.iter().zip(&claim.sources).all(|(a, b)| {
            a.path == b.path && a.size == b.size && a.sha512 == hex::encode(b.digest.as_bytes())
        }))
}

fn inventory(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    fn walk(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<String, PathBuf>,
        names: &mut BTreeSet<String>,
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(current)?;
        if linked(&metadata) {
            return Err(invalid("symbolic links are not RAW source content"));
        }
        // Validate directory names too, including empty directories. Otherwise
        // Rust could accept a dataset the Python capture rejects as nonportable.
        if current != root {
            let relative = current
                .strip_prefix(root)
                .map_err(|_| invalid("source path is outside its root"))?;
            let parts = relative
                .components()
                .map(|part| match part {
                    std::path::Component::Normal(value) => value
                        .to_str()
                        .ok_or_else(|| invalid("source paths must be valid UTF-8")),
                    _ => Err(invalid("unsafe source path")),
                })
                .collect::<Result<Vec<_>>>()?;
            validate_path(&parts.join("/"))?;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(current)? {
                walk(root, &entry?.path(), files, names)?;
            }
        } else if metadata.is_file() {
            let relative = current
                .strip_prefix(root)
                .map_err(|_| invalid("source path is outside its root"))?;
            let mut components = Vec::new();
            for part in relative.components() {
                let std::path::Component::Normal(value) = part else {
                    return Err(invalid("unsafe source path"));
                };
                components.push(
                    value
                        .to_str()
                        .ok_or_else(|| invalid("source paths must be valid UTF-8"))?,
                );
            }
            let name = components.join("/");
            validate_path(&name)?;
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(invalid("case-colliding source paths"));
            }
            files.insert(name, current.to_owned());
        } else {
            return Err(invalid(
                "RAW source must contain only regular files and directories",
            ));
        }
        Ok(())
    }
    let metadata = fs::symlink_metadata(root)?;
    if linked(&metadata) {
        return Err(invalid("symbolic links are not RAW source content"));
    }
    let mut files = BTreeMap::new();
    if metadata.is_file() {
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("source file needs a portable name"))?;
        validate_path(name)?;
        files.insert(name.to_owned(), root.to_owned());
    } else if metadata.is_dir() {
        walk(root, root, &mut files, &mut BTreeSet::new())?;
    } else {
        return Err(invalid("RAW source must be a regular file or directory"));
    }
    Ok(files)
}

#[cfg(windows)]
pub(super) fn linked(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
pub(super) fn linked(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(not(unix))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    // This preliminary metadata comparison is supplemented below with the
    // actual open handle's volume and 128-bit file identity on Windows.
    a.created().ok() == b.created().ok()
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    volume: u64,
    id: u128,
}
#[cfg(unix)]
pub(super) fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        id: metadata.ino().into(),
    })
}
#[cfg(windows)]
pub(super) fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    // File owns a live handle; the buffer matches FileIdInfo's documented
    // layout and is read only after a successful API call.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        id: u128::from_le_bytes(info.FileId.Identifier),
    })
}
#[cfg(not(any(unix, windows)))]
pub(super) fn file_identity(_file: &fs::File) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "source file identity unavailable",
    ))
}
pub(super) fn matches_snapshot(
    path: &Path,
    before: &fs::Metadata,
    identity: FileIdentity,
) -> io::Result<bool> {
    let after = fs::symlink_metadata(path)?;
    if !after.is_file()
        || linked(&after)
        || !same_file(before, &after)
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        return Ok(false);
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    Ok(file_identity(&file)? == identity
        && same_file(before, &opened)
        && opened.len() == before.len()
        && opened.modified()? == before.modified()?)
}
fn hash_file(name: String, path: &Path) -> Result<(SourceFile, fs::Metadata, FileIdentity)> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || linked(&before) {
        return Err(invalid("RAW source changed during capture"));
    }
    let mut file = fs::File::open(path)?;
    let opened = file.metadata()?;
    let identity = file_identity(&file)?;
    if !same_file(&before, &opened)
        || opened.len() != before.len()
        || opened.modified()? != before.modified()?
    {
        return Err(invalid("RAW source changed during capture"));
    }
    let mut hash = Sha512::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        let count = match file.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            break;
        }
        read = read
            .checked_add(count as u64)
            .ok_or_else(|| invalid("source length overflow"))?;
        hash.update(&buffer[..count]);
    }
    let after = fs::symlink_metadata(path)?;
    let retained = file.metadata()?;
    if !after.is_file()
        || linked(&after)
        || !same_file(&opened, &after)
        || !same_file(&opened, &retained)
        || read != opened.len()
        || after.len() != opened.len()
        || retained.len() != opened.len()
        || after.modified()? != opened.modified()?
        || retained.modified()? != opened.modified()?
        || !matches_snapshot(path, &opened, identity)?
    {
        return Err(invalid("RAW source changed during capture"));
    }
    Ok((
        SourceFile {
            path: name,
            size: read,
            sha512: hex::encode(hash.finalize()),
        },
        retained,
        identity,
    ))
}
/// Hash the caller's explicit local RAW file or dataset directory.
pub fn capture_source(path: impl AsRef<Path>) -> Result<Vec<SourceFile>> {
    let path = path.as_ref();
    let paths = inventory(path)?;
    if paths.is_empty() {
        return Err(invalid("RAW source inventory is empty"));
    }
    let mut files = Vec::new();
    let mut snapshots = Vec::new();
    for (name, full) in &paths {
        let (file, metadata, identity) = hash_file(name.clone(), full)?;
        files.push(file);
        snapshots.push((full, metadata, identity));
    }
    if inventory(path)? != paths {
        return Err(invalid("RAW source inventory changed during capture"));
    }
    for (full, before, identity) in snapshots {
        if !matches_snapshot(full, &before, identity)? {
            return Err(invalid("RAW source changed during capture"));
        }
    }
    Ok(files)
}
