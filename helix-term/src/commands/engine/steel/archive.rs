//! Cancellable, bounded, read-only archive indexing and entry reads.

use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use steel::{
    rvals::{Custom, IntoSteelVal, SteelByteVector},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelErr, SteelVal,
};
use xz2::read::XzDecoder;
use zip::ZipArchive;

use super::{
    file_snapshot::{
        ensure_snapshot_fresh, open_canonical_regular, snapshot_stale, FileFingerprint,
    },
    Context, CTX,
};

const MAX_ENTRIES: usize = 100_000;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
const MAX_TAR_SCAN_BYTES: u64 = 512 * 1024 * 1024;
const MAX_READ_OFFSET: u64 = 64 * 1024 * 1024;
const MAX_READ_BYTES: usize = 1024 * 1024;
const IO_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveFormat {
    Zip,
    Jar,
    Wheel,
    Tar,
    TarGzip,
    TarBzip2,
    TarXz,
    TarZstd,
}

impl ArchiveFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Jar => "jar",
            Self::Wheel => "wheel",
            Self::Tar => "tar",
            Self::TarGzip => "tar-gzip",
            Self::TarBzip2 => "tar-bzip2",
            Self::TarXz => "tar-xz",
            Self::TarZstd => "tar-zstd",
        }
    }

    fn compressed_tar(self) -> bool {
        matches!(
            self,
            Self::TarGzip | Self::TarBzip2 | Self::TarXz | Self::TarZstd
        )
    }
}

#[derive(Clone, Debug)]
struct ArchiveEntryMetadata {
    index: usize,
    display_path: String,
    kind: &'static str,
    size: u64,
    compressed_size: Option<u64>,
    mode: Option<u32>,
    timestamp: Option<String>,
    link_target: Option<String>,
    compression: String,
    encrypted: bool,
    raw_file_position: Option<u64>,
    readable: bool,
}

#[derive(Debug)]
struct ArchiveHandle {
    file: File,
    path: PathBuf,
    fingerprint: FileFingerprint,
    format: ArchiveFormat,
    entries: Vec<ArchiveEntryMetadata>,
    zip_patches: Arc<BTreeMap<u64, u8>>,
    closed: AtomicBool,
}

#[derive(Clone)]
struct SteelArchiveHandle(Arc<ArchiveHandle>);

impl Custom for SteelArchiveHandle {}

#[derive(Clone, Default)]
struct SteelArchiveCancelToken(Arc<AtomicBool>);

impl Custom for SteelArchiveCancelToken {}

enum OpenOutcome {
    Opened(SteelArchiveHandle),
    Cancelled,
    Stale,
    Error(String),
}

struct ArchiveOpenCallbackValue(OpenOutcome);

impl TryInto<SteelVal> for ArchiveOpenCallbackValue {
    type Error = SteelErr;

    fn try_into(self) -> Result<SteelVal, Self::Error> {
        let values = match self.0 {
            OpenOutcome::Opened(handle) => {
                vec!["ok".into_steelval()?, handle.into_steelval()?]
            }
            OpenOutcome::Cancelled => vec!["cancelled".into_steelval()?, false.into_steelval()?],
            OpenOutcome::Stale => vec!["stale".into_steelval()?, false.into_steelval()?],
            OpenOutcome::Error(message) => {
                vec!["error".into_steelval()?, message.into_steelval()?]
            }
        };
        Ok(SteelVal::ListV(values.into()))
    }
}

enum ReadOutcome {
    Read(Vec<u8>, bool),
    Cancelled,
    Stale,
    Error(String),
}

struct ArchiveReadCallbackValue(ReadOutcome);

impl TryInto<SteelVal> for ArchiveReadCallbackValue {
    type Error = SteelErr;

    fn try_into(self) -> Result<SteelVal, Self::Error> {
        let values = match self.0 {
            ReadOutcome::Read(bytes, truncated) => vec![
                "ok".into_steelval()?,
                SteelVal::ByteVector(SteelByteVector::new(bytes)),
                truncated.into_steelval()?,
            ],
            ReadOutcome::Cancelled => vec![
                "cancelled".into_steelval()?,
                false.into_steelval()?,
                false.into_steelval()?,
            ],
            ReadOutcome::Stale => vec![
                "stale".into_steelval()?,
                false.into_steelval()?,
                false.into_steelval()?,
            ],
            ReadOutcome::Error(message) => vec![
                "error".into_steelval()?,
                message.into_steelval()?,
                false.into_steelval()?,
            ],
        };
        Ok(SteelVal::ListV(values.into()))
    }
}

struct ScanBudgetReader<R> {
    inner: R,
    consumed: u64,
    limit: u64,
    cancelled: Arc<AtomicBool>,
}

impl<R> ScanBudgetReader<R> {
    fn new(inner: R, limit: u64, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            consumed: 0,
            limit,
            cancelled,
        }
    }
}

impl<R: Read> Read for ScanBudgetReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "archive operation cancelled",
            ));
        }
        if self.consumed >= self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TAR decompressed scan budget exceeded",
            ));
        }
        let available = (self.limit - self.consumed).min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..available])?;
        self.consumed += read as u64;
        Ok(read)
    }
}

#[derive(Debug)]
struct PatchedZipReader {
    file: File,
    patches: Arc<BTreeMap<u64, u8>>,
}

impl PatchedZipReader {
    fn new(file: File, patches: Arc<BTreeMap<u64, u8>>) -> Self {
        Self { file, patches }
    }
}

impl Read for PatchedZipReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let start = self.file.stream_position()?;
        let read = self.file.read(buffer)?;
        let end = start.saturating_add(read as u64);
        for (&offset, &value) in self.patches.range(start..end) {
            buffer[(offset - start) as usize] = value;
        }
        Ok(read)
    }
}

impl Seek for PatchedZipReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

#[derive(Debug)]
struct ZipCentralDirectory {
    names: Vec<Vec<u8>>,
    patches: Arc<BTreeMap<u64, u8>>,
}

fn u16_at(bytes: &[u8], offset: usize) -> anyhow::Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow::anyhow!("truncated ZIP metadata"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow::anyhow!("truncated ZIP metadata"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn u64_at(bytes: &[u8], offset: usize) -> anyhow::Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow::anyhow!("truncated ZIP metadata"))?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn read_window(file: &mut File, offset: u64, length: usize) -> anyhow::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn eocd_position(file: &mut File, size: u64) -> anyhow::Result<(u64, Vec<u8>)> {
    let window_len = size.min(65_557) as usize;
    let start = size - window_len as u64;
    let bytes = read_window(file, start, window_len)?;
    let relative = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .ok_or_else(|| anyhow::anyhow!("ZIP end-of-central-directory record is missing"))?;
    if relative + 22 > bytes.len() {
        anyhow::bail!("truncated ZIP end-of-central-directory record");
    }
    let comment_length = u16_at(&bytes, relative + 20)? as usize;
    if relative + 22 + comment_length != bytes.len() {
        anyhow::bail!("invalid ZIP end-of-central-directory comment length");
    }
    Ok((start + relative as u64, bytes[relative..].to_vec()))
}

fn central_directory_location(file: &mut File, size: u64) -> anyhow::Result<(u64, u64, usize)> {
    let (eocd_offset, eocd) = eocd_position(file, size)?;
    let disk = u16_at(&eocd, 4)?;
    let central_disk = u16_at(&eocd, 6)?;
    let entries_on_disk = u16_at(&eocd, 8)?;
    let entries_total = u16_at(&eocd, 10)?;
    let central_size = u32_at(&eocd, 12)?;
    let central_offset = u32_at(&eocd, 16)?;
    if disk != 0 || central_disk != 0 || entries_on_disk != entries_total {
        anyhow::bail!("multi-disk ZIP archives are not supported");
    }
    if entries_total != u16::MAX && central_size != u32::MAX && central_offset != u32::MAX {
        return Ok((
            central_offset as u64,
            central_size as u64,
            entries_total as usize,
        ));
    }

    if eocd_offset < 20 {
        anyhow::bail!("ZIP64 locator is missing");
    }
    let locator = read_window(file, eocd_offset - 20, 20)?;
    if &locator[..4] != b"PK\x06\x07" {
        anyhow::bail!("ZIP64 locator is missing");
    }
    if u32_at(&locator, 4)? != 0 || u32_at(&locator, 16)? != 1 {
        anyhow::bail!("multi-disk ZIP64 archives are not supported");
    }
    let zip64_offset = u64_at(&locator, 8)?;
    let zip64 = read_window(file, zip64_offset, 56)?;
    if &zip64[..4] != b"PK\x06\x06" {
        anyhow::bail!("ZIP64 end-of-central-directory record is missing");
    }
    if u32_at(&zip64, 16)? != 0 || u32_at(&zip64, 20)? != 0 {
        anyhow::bail!("multi-disk ZIP64 archives are not supported");
    }
    let entries_on_disk = u64_at(&zip64, 24)?;
    let entries_total = u64_at(&zip64, 32)?;
    if entries_on_disk != entries_total || entries_total > usize::MAX as u64 {
        anyhow::bail!("invalid ZIP64 entry count");
    }
    Ok((
        u64_at(&zip64, 48)?,
        u64_at(&zip64, 40)?,
        entries_total as usize,
    ))
}

fn replacement_name(
    length: usize,
    used: &mut HashSet<Vec<u8>>,
    next_candidate: &mut u64,
) -> anyhow::Result<Vec<u8>> {
    if length == 0 {
        anyhow::bail!("duplicate empty ZIP member names cannot be indexed");
    }
    let attempts = used.len().saturating_add(257);
    for _ in 0..attempts {
        let mut value = *next_candidate;
        *next_candidate = next_candidate.wrapping_add(1);
        let mut candidate = vec![0; length];
        for byte in &mut candidate {
            *byte = value as u8;
            value = value.rotate_right(7).wrapping_add(0x9e37_79b9_7f4a_7c15);
        }
        if used.insert(candidate.clone()) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("too many duplicate ZIP names of length {length}")
}

fn zip_central_directory(file: &File, size: u64) -> anyhow::Result<ZipCentralDirectory> {
    let mut reader = file.try_clone()?;
    let (central_offset, central_size, entry_count) =
        central_directory_location(&mut reader, size)?;
    if entry_count > MAX_ENTRIES {
        anyhow::bail!("archives are limited to {MAX_ENTRIES} entries");
    }
    let central_size_usize = usize::try_from(central_size)
        .map_err(|_| anyhow::anyhow!("ZIP central directory is too large"))?;
    if central_size_usize > MAX_METADATA_BYTES.saturating_mul(4) {
        anyhow::bail!("ZIP central directory exceeds its bounded scan budget");
    }
    let bytes = read_window(&mut reader, central_offset, central_size_usize)?;
    let mut names = Vec::with_capacity(entry_count);
    let mut name_offsets = Vec::with_capacity(entry_count);
    let mut cursor = 0usize;
    for _ in 0..entry_count {
        if bytes.get(cursor..cursor + 4) != Some(b"PK\x01\x02") {
            anyhow::bail!("invalid ZIP central-directory entry signature");
        }
        let name_length = u16_at(&bytes, cursor + 28)? as usize;
        let extra_length = u16_at(&bytes, cursor + 30)? as usize;
        let comment_length = u16_at(&bytes, cursor + 32)? as usize;
        let disk_start = u16_at(&bytes, cursor + 34)?;
        if disk_start != 0 && disk_start != u16::MAX {
            anyhow::bail!("multi-disk ZIP archives are not supported");
        }
        let name_start = cursor + 46;
        let record_end = name_start
            .checked_add(name_length)
            .and_then(|value| value.checked_add(extra_length))
            .and_then(|value| value.checked_add(comment_length))
            .ok_or_else(|| anyhow::anyhow!("ZIP central-directory length overflow"))?;
        if record_end > bytes.len() {
            anyhow::bail!("truncated ZIP central-directory entry");
        }
        names.push(bytes[name_start..name_start + name_length].to_vec());
        name_offsets.push(central_offset + name_start as u64);
        cursor = record_end;
    }
    if cursor > bytes.len() {
        anyhow::bail!("invalid ZIP central-directory size");
    }

    let mut used: HashSet<Vec<u8>> = names.iter().cloned().collect();
    let mut seen = HashSet::new();
    let mut patches = BTreeMap::new();
    let mut next_candidate = 0u64;
    for (name, &offset) in names.iter().zip(&name_offsets) {
        if !seen.insert(name.clone()) {
            let replacement = replacement_name(name.len(), &mut used, &mut next_candidate)?;
            for (relative, value) in replacement.into_iter().enumerate() {
                patches.insert(offset + relative as u64, value);
            }
        }
    }
    Ok(ZipCentralDirectory {
        names,
        patches: Arc::new(patches),
    })
}

fn cancelled_error(error: &anyhow::Error, token: &AtomicBool) -> bool {
    token.load(Ordering::Acquire) || error.to_string().contains("operation cancelled")
}

fn archive_fresh(handle: &ArchiveHandle) -> bool {
    !snapshot_stale(
        &handle.file,
        &handle.path,
        &handle.fingerprint,
        handle.closed.load(Ordering::Acquire),
    )
}

fn ensure_archive_fresh(handle: &ArchiveHandle) -> anyhow::Result<()> {
    ensure_snapshot_fresh(
        &handle.file,
        &handle.path,
        &handle.fingerprint,
        handle.closed.load(Ordering::Acquire),
        "archive",
        "refresh the archive explorer",
    )
}

fn prefix(file: &mut File) -> io::Result<[u8; 8]> {
    let mut bytes = [0; 8];
    file.seek(SeekFrom::Start(0))?;
    let mut filled = 0;
    while filled < bytes.len() {
        match file.read(&mut bytes[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(bytes)
}

fn path_display(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() > MAX_PATH_BYTES {
        anyhow::bail!("archive entry paths are limited to {MAX_PATH_BYTES} bytes");
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn path_is_metadata_only(bytes: &[u8]) -> bool {
    if bytes.is_empty()
        || std::str::from_utf8(bytes).is_err()
        || bytes.contains(&0)
        || matches!(bytes.first(), Some(b'/') | Some(b'\\'))
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
    {
        return true;
    }

    bytes
        .split(|byte| matches!(byte, b'/' | b'\\'))
        .any(|component| component == b"..")
}

fn account_metadata(total: &mut usize, entry: &ArchiveEntryMetadata) -> anyhow::Result<()> {
    let added = entry
        .display_path
        .len()
        .saturating_add(entry.link_target.as_ref().map_or(0, String::len))
        .saturating_add(entry.compression.len())
        .saturating_add(entry.timestamp.as_ref().map_or(0, String::len))
        .saturating_add(96);
    *total = total.saturating_add(added);
    if *total > MAX_METADATA_BYTES {
        anyhow::bail!("archive metadata exceeds the {MAX_METADATA_BYTES} byte budget");
    }
    Ok(())
}

fn check_index_progress(
    file: &File,
    path: &Path,
    fingerprint: &FileFingerprint,
    token: &AtomicBool,
) -> anyhow::Result<()> {
    if token.load(Ordering::Acquire) {
        anyhow::bail!("archive operation cancelled");
    }
    ensure_snapshot_fresh(
        file,
        path,
        fingerprint,
        false,
        "archive",
        "retry opening it",
    )
}

fn zip_kind<R: Read>(file: &zip::read::ZipFile<'_, R>) -> (&'static str, bool) {
    if file.is_dir() {
        ("directory", false)
    } else if file.is_symlink() {
        ("symlink", false)
    } else if file.is_file() {
        ("file", true)
    } else {
        ("special", false)
    }
}

fn index_zip(
    file: &File,
    path: &Path,
    fingerprint: &FileFingerprint,
    token: &AtomicBool,
) -> anyhow::Result<(
    ArchiveFormat,
    Vec<ArchiveEntryMetadata>,
    Arc<BTreeMap<u64, u8>>,
)> {
    let central = zip_central_directory(file, fingerprint.len)?;
    let reader = PatchedZipReader::new(file.try_clone()?, central.patches.clone());
    let mut zip = ZipArchive::new(reader)?;
    if zip.len() != central.names.len() {
        anyhow::bail!("ZIP member index could not preserve every central-directory entry");
    }
    let mut entries = Vec::with_capacity(central.names.len());
    let mut metadata_bytes = 0;
    let mut jar = false;
    let mut wheel = false;

    for index in 0..zip.len() {
        check_index_progress(file, path, fingerprint, token)?;
        let metadata_only_path = path_is_metadata_only(&central.names[index]);
        // Raw access exposes central-directory metadata even for encrypted
        // members without enabling any decryption implementation.
        let (
            display_path,
            kind,
            readable,
            encrypted,
            size,
            compressed_size,
            mode,
            timestamp,
            compression,
        ) = {
            let member = zip.by_index_raw(index)?;
            let (kind, readable) = zip_kind(&member);
            (
                path_display(&central.names[index])?,
                kind,
                readable,
                member.encrypted(),
                member.size(),
                member.compressed_size(),
                member.unix_mode(),
                member.last_modified().map(|value| value.to_string()),
                member.compression().to_string().to_ascii_lowercase(),
            )
        };
        let link_target = if kind == "symlink" && !encrypted {
            if size > MAX_PATH_BYTES as u64 {
                anyhow::bail!("archive link targets are limited to {MAX_PATH_BYTES} bytes");
            }
            let mut member = zip.by_index(index)?;
            let mut bytes = Vec::with_capacity(size as usize);
            member.read_to_end(&mut bytes)?;
            Some(path_display(&bytes)?)
        } else {
            None
        };
        let kind = if metadata_only_path {
            "unsafe-path"
        } else {
            kind
        };
        let entry = ArchiveEntryMetadata {
            index,
            display_path: display_path.clone(),
            kind,
            size,
            compressed_size: Some(compressed_size),
            mode,
            timestamp,
            link_target,
            compression,
            encrypted,
            raw_file_position: None,
            readable: readable && !encrypted && !metadata_only_path,
        };
        jar |= display_path.eq_ignore_ascii_case("META-INF/MANIFEST.MF");
        wheel |= display_path.ends_with(".dist-info/WHEEL");
        account_metadata(&mut metadata_bytes, &entry)?;
        entries.push(entry);
    }

    check_index_progress(file, path, fingerprint, token)?;
    let format = if wheel {
        ArchiveFormat::Wheel
    } else if jar {
        ArchiveFormat::Jar
    } else {
        ArchiveFormat::Zip
    };
    Ok((format, entries, central.patches))
}

fn tar_kind(entry_type: tar::EntryType) -> (&'static str, bool) {
    if entry_type.is_dir() {
        ("directory", false)
    } else if entry_type.is_symlink() {
        ("symlink", false)
    } else if entry_type.is_hard_link() {
        ("hard-link", false)
    } else if entry_type.is_gnu_sparse() {
        ("sparse", false)
    } else if entry_type.is_file() || entry_type.is_contiguous() {
        ("file", true)
    } else {
        ("special", false)
    }
}

fn index_tar_reader<R: Read>(
    reader: R,
    format: ArchiveFormat,
    source_file: &File,
    source_path: &Path,
    fingerprint: &FileFingerprint,
    token: Arc<AtomicBool>,
) -> anyhow::Result<Vec<ArchiveEntryMetadata>> {
    let budgeted = ScanBudgetReader::new(reader, MAX_TAR_SCAN_BYTES, token.clone());
    let mut archive = tar::Archive::new(budgeted);
    let mut entries = Vec::new();
    let mut metadata_bytes = 0;

    for candidate in archive.entries()? {
        check_index_progress(source_file, source_path, fingerprint, &token)?;
        if entries.len() >= MAX_ENTRIES {
            anyhow::bail!("archives are limited to {MAX_ENTRIES} entries");
        }
        let entry = candidate?;
        let header = entry.header();
        let entry_type = header.entry_type();
        let (kind, readable) = tar_kind(entry_type);
        let path_bytes = entry.path_bytes();
        let metadata_only_path = path_is_metadata_only(&path_bytes);
        let display_path = path_display(&path_bytes)?;
        let link_target = entry
            .link_name_bytes()
            .map(|bytes| path_display(&bytes))
            .transpose()?;
        let kind = if metadata_only_path {
            "unsafe-path"
        } else {
            kind
        };
        let metadata = ArchiveEntryMetadata {
            index: entries.len(),
            display_path,
            kind,
            size: entry.size(),
            compressed_size: None,
            mode: header.mode().ok(),
            timestamp: header.mtime().ok().map(|value| value.to_string()),
            link_target,
            compression: format.label().to_string(),
            encrypted: false,
            raw_file_position: (!format.compressed_tar()).then(|| entry.raw_file_position()),
            readable: readable && !metadata_only_path,
        };
        account_metadata(&mut metadata_bytes, &metadata)?;
        entries.push(metadata);
    }
    let budgeted = archive.into_inner();
    if budgeted.consumed == 0 {
        anyhow::bail!("input is not a structurally valid TAR archive");
    }
    check_index_progress(source_file, source_path, fingerprint, &token)?;
    Ok(entries)
}

fn index_tar(
    file: &File,
    path: &Path,
    fingerprint: &FileFingerprint,
    format: ArchiveFormat,
    token: Arc<AtomicBool>,
) -> anyhow::Result<Vec<ArchiveEntryMetadata>> {
    let reader = file.try_clone()?;
    match format {
        ArchiveFormat::Tar => index_tar_reader(reader, format, file, path, fingerprint, token),
        ArchiveFormat::TarGzip => index_tar_reader(
            GzDecoder::new(reader),
            format,
            file,
            path,
            fingerprint,
            token,
        ),
        ArchiveFormat::TarBzip2 => index_tar_reader(
            BzDecoder::new(reader),
            format,
            file,
            path,
            fingerprint,
            token,
        ),
        ArchiveFormat::TarXz => index_tar_reader(
            XzDecoder::new(reader),
            format,
            file,
            path,
            fingerprint,
            token,
        ),
        ArchiveFormat::TarZstd => index_tar_reader(
            zstd::stream::read::Decoder::new(reader)?,
            format,
            file,
            path,
            fingerprint,
            token,
        ),
        _ => anyhow::bail!("not a TAR archive format"),
    }
}

fn detect_tar_format(bytes: &[u8; 8]) -> ArchiveFormat {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        ArchiveFormat::TarGzip
    } else if bytes.starts_with(b"BZh") {
        ArchiveFormat::TarBzip2
    } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        ArchiveFormat::TarXz
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        ArchiveFormat::TarZstd
    } else {
        ArchiveFormat::Tar
    }
}

fn index_archive(path: String, token: SteelArchiveCancelToken) -> OpenOutcome {
    let opened = (|| -> anyhow::Result<SteelArchiveHandle> {
        if token.0.load(Ordering::Acquire) {
            anyhow::bail!("archive operation cancelled");
        }
        let (mut file, canonical, fingerprint) =
            open_canonical_regular(&path, "archive", "archive explorer")?;
        let bytes = prefix(&mut file)?;
        let (format, entries, zip_patches) = if bytes.starts_with(b"PK\x03\x04")
            || bytes.starts_with(b"PK\x05\x06")
            || bytes.starts_with(b"PK\x06\x06")
        {
            index_zip(&file, &canonical, &fingerprint, &token.0)?
        } else if bytes.starts_with(b"PK\x07\x08") {
            anyhow::bail!("multi-disk ZIP archives are not supported");
        } else {
            let format = detect_tar_format(&bytes);
            let entries = index_tar(&file, &canonical, &fingerprint, format, token.0.clone())?;
            (format, entries, Arc::new(BTreeMap::new()))
        };
        ensure_snapshot_fresh(
            &file,
            &canonical,
            &fingerprint,
            false,
            "archive",
            "retry opening it",
        )?;
        Ok(SteelArchiveHandle(Arc::new(ArchiveHandle {
            file,
            path: canonical,
            fingerprint,
            format,
            entries,
            zip_patches,
            closed: AtomicBool::new(false),
        })))
    })();

    match opened {
        Ok(handle) => OpenOutcome::Opened(handle),
        Err(error) if cancelled_error(&error, &token.0) => OpenOutcome::Cancelled,
        Err(error) if error.to_string().contains("changed") => OpenOutcome::Stale,
        Err(error) => OpenOutcome::Error(error.to_string()),
    }
}

fn archive_cancel_token() -> SteelArchiveCancelToken {
    SteelArchiveCancelToken::default()
}

fn archive_cancel(token: &SteelArchiveCancelToken) -> bool {
    !token.0.swap(true, Ordering::AcqRel)
}

fn archive_open_async(
    cx: &mut Context,
    path: String,
    token: SteelArchiveCancelToken,
    callback: SteelVal,
) -> anyhow::Result<()> {
    let rooted = callback.as_rooted();
    let future = async move {
        let outcome = tokio::task::spawn_blocking(move || index_archive(path, token))
            .await
            .unwrap_or_else(|error| OpenOutcome::Error(error.to_string()));
        Ok::<_, helix_lsp::Error>(ArchiveOpenCallbackValue(outcome))
    };
    super::super::create_callback(cx, future, rooted)
}

fn archive_close(handle: &SteelArchiveHandle) -> bool {
    !handle.0.closed.swap(true, Ordering::AcqRel)
}

fn archive_path(handle: &SteelArchiveHandle) -> anyhow::Result<String> {
    handle
        .0
        .path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("archive path is not valid UTF-8"))
}

fn archive_format(handle: &SteelArchiveHandle) -> String {
    handle.0.format.label().to_string()
}

fn archive_fingerprint(handle: &SteelArchiveHandle) -> (String, u64) {
    (handle.0.fingerprint.identity(), handle.0.fingerprint.len)
}

fn archive_entry_count(handle: &SteelArchiveHandle) -> usize {
    handle.0.entries.len()
}

fn archive_closed(handle: &SteelArchiveHandle) -> bool {
    handle.0.closed.load(Ordering::Acquire)
}

fn archive_stale(handle: &SteelArchiveHandle) -> bool {
    !archive_fresh(&handle.0)
}

fn optional_value<T: IntoSteelVal>(value: Option<T>) -> Result<SteelVal, SteelErr> {
    match value {
        Some(value) => value.into_steelval(),
        None => false.into_steelval(),
    }
}

fn metadata_value(entry: &ArchiveEntryMetadata) -> Result<SteelVal, SteelErr> {
    Ok(SteelVal::ListV(
        vec![
            entry.index.into_steelval()?,
            entry.display_path.clone().into_steelval()?,
            entry.kind.into_steelval()?,
            entry.size.into_steelval()?,
            optional_value(entry.compressed_size)?,
            optional_value(entry.mode.map(u64::from))?,
            optional_value(entry.timestamp.clone())?,
            optional_value(entry.link_target.clone())?,
            entry.compression.clone().into_steelval()?,
            entry.encrypted.into_steelval()?,
        ]
        .into(),
    ))
}

fn archive_entries_page(
    handle: &SteelArchiveHandle,
    start: usize,
    count: usize,
) -> anyhow::Result<SteelVal> {
    ensure_archive_fresh(&handle.0)?;
    if count > 10_000 {
        anyhow::bail!("archive metadata pages are limited to 10000 entries");
    }
    if start > handle.0.entries.len() {
        anyhow::bail!("archive metadata page starts past the entry count");
    }
    let end = start.saturating_add(count).min(handle.0.entries.len());
    let values = handle.0.entries[start..end]
        .iter()
        .map(metadata_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SteelVal::ListV(values.into()))
}

fn check_read_progress(handle: &ArchiveHandle, token: &AtomicBool) -> Result<(), ReadOutcome> {
    if token.load(Ordering::Acquire) {
        return Err(ReadOutcome::Cancelled);
    }
    if !archive_fresh(handle) {
        return Err(ReadOutcome::Stale);
    }
    Ok(())
}

fn read_stream_range<R: Read>(
    reader: &mut R,
    handle: &ArchiveHandle,
    token: &AtomicBool,
    offset: u64,
    wanted: usize,
) -> Result<Vec<u8>, ReadOutcome> {
    let mut discard = vec![0; IO_CHUNK_BYTES];
    let mut skipped = 0u64;
    while skipped < offset {
        check_read_progress(handle, token)?;
        let amount = (offset - skipped).min(discard.len() as u64) as usize;
        let read = reader
            .read(&mut discard[..amount])
            .map_err(|error| ReadOutcome::Error(error.to_string()))?;
        if read == 0 {
            return Err(ReadOutcome::Error(
                "archive entry ended before the requested offset".into(),
            ));
        }
        skipped += read as u64;
    }

    let mut result = Vec::with_capacity(wanted);
    while result.len() < wanted {
        check_read_progress(handle, token)?;
        let amount = (wanted - result.len()).min(IO_CHUNK_BYTES);
        let old_len = result.len();
        result.resize(old_len + amount, 0);
        let read = reader
            .read(&mut result[old_len..])
            .map_err(|error| ReadOutcome::Error(error.to_string()))?;
        result.truncate(old_len + read);
        if read == 0 {
            break;
        }
    }
    check_read_progress(handle, token)?;
    Ok(result)
}

fn read_zip_entry(
    handle: &ArchiveHandle,
    token: &AtomicBool,
    index: usize,
    offset: u64,
    wanted: usize,
) -> Result<Vec<u8>, ReadOutcome> {
    let file = File::open(&handle.path).map_err(|error| ReadOutcome::Error(error.to_string()))?;
    let reader = PatchedZipReader::new(file, handle.zip_patches.clone());
    let mut zip = ZipArchive::new(reader).map_err(|error| ReadOutcome::Error(error.to_string()))?;
    let mut member = zip
        .by_index(index)
        .map_err(|error| ReadOutcome::Error(error.to_string()))?;
    read_stream_range(&mut member, handle, token, offset, wanted)
}

fn read_raw_tar_entry(
    handle: &ArchiveHandle,
    token: &AtomicBool,
    entry: &ArchiveEntryMetadata,
    offset: u64,
    wanted: usize,
) -> Result<Vec<u8>, ReadOutcome> {
    check_read_progress(handle, token)?;
    let position = entry
        .raw_file_position
        .and_then(|position| position.checked_add(offset))
        .ok_or_else(|| ReadOutcome::Error("invalid raw TAR entry position".into()))?;
    let mut file = handle
        .file
        .try_clone()
        .map_err(|error| ReadOutcome::Error(error.to_string()))?;
    file.seek(SeekFrom::Start(position))
        .map_err(|error| ReadOutcome::Error(error.to_string()))?;
    read_stream_range(&mut file, handle, token, 0, wanted)
}

fn read_tar_reader<R: Read>(
    reader: R,
    handle: &ArchiveHandle,
    token: Arc<AtomicBool>,
    index: usize,
    offset: u64,
    wanted: usize,
) -> Result<Vec<u8>, ReadOutcome> {
    let budgeted = ScanBudgetReader::new(reader, MAX_TAR_SCAN_BYTES, token.clone());
    let mut archive = tar::Archive::new(budgeted);
    let candidates = archive
        .entries()
        .map_err(|error| ReadOutcome::Error(error.to_string()))?;
    for (candidate_index, candidate) in candidates.enumerate() {
        check_read_progress(handle, &token)?;
        let mut entry = candidate.map_err(|error| {
            if token.load(Ordering::Acquire) {
                ReadOutcome::Cancelled
            } else {
                ReadOutcome::Error(error.to_string())
            }
        })?;
        if candidate_index == index {
            return read_stream_range(&mut entry, handle, &token, offset, wanted);
        }
    }
    Err(ReadOutcome::Error(
        "archive entry index is no longer present".into(),
    ))
}

fn read_compressed_tar_entry(
    handle: &ArchiveHandle,
    token: Arc<AtomicBool>,
    index: usize,
    offset: u64,
    wanted: usize,
) -> Result<Vec<u8>, ReadOutcome> {
    let reader = File::open(&handle.path).map_err(|error| ReadOutcome::Error(error.to_string()))?;
    match handle.format {
        ArchiveFormat::TarGzip => {
            read_tar_reader(GzDecoder::new(reader), handle, token, index, offset, wanted)
        }
        ArchiveFormat::TarBzip2 => {
            read_tar_reader(BzDecoder::new(reader), handle, token, index, offset, wanted)
        }
        ArchiveFormat::TarXz => {
            read_tar_reader(XzDecoder::new(reader), handle, token, index, offset, wanted)
        }
        ArchiveFormat::TarZstd => {
            let decoder = zstd::stream::read::Decoder::new(reader)
                .map_err(|error| ReadOutcome::Error(error.to_string()))?;
            read_tar_reader(decoder, handle, token, index, offset, wanted)
        }
        _ => Err(ReadOutcome::Error("archive is not a compressed TAR".into())),
    }
}

fn read_archive_entry(
    handle: SteelArchiveHandle,
    token: SteelArchiveCancelToken,
    index: usize,
    offset: u64,
    length: usize,
) -> ReadOutcome {
    let result = (|| -> Result<(Vec<u8>, bool), ReadOutcome> {
        check_read_progress(&handle.0, &token.0)?;
        if offset > MAX_READ_OFFSET {
            return Err(ReadOutcome::Error(format!(
                "archive entry offsets are limited to {MAX_READ_OFFSET} bytes"
            )));
        }
        if length > MAX_READ_BYTES {
            return Err(ReadOutcome::Error(format!(
                "archive entry reads are limited to {MAX_READ_BYTES} bytes"
            )));
        }
        let entry = handle
            .0
            .entries
            .get(index)
            .ok_or_else(|| ReadOutcome::Error("archive entry index is out of range".into()))?;
        if entry.encrypted {
            return Err(ReadOutcome::Error(
                "encrypted archive entries are metadata-only".into(),
            ));
        }
        if !entry.readable {
            return Err(ReadOutcome::Error(format!(
                "archive {} entries are metadata-only",
                entry.kind
            )));
        }
        if offset > entry.size {
            return Err(ReadOutcome::Error(
                "archive entry offset exceeds its uncompressed size".into(),
            ));
        }
        let wanted = length.min((entry.size - offset).min(usize::MAX as u64) as usize);
        let bytes = match handle.0.format {
            ArchiveFormat::Zip | ArchiveFormat::Jar | ArchiveFormat::Wheel => {
                read_zip_entry(&handle.0, &token.0, index, offset, wanted)?
            }
            ArchiveFormat::Tar => read_raw_tar_entry(&handle.0, &token.0, entry, offset, wanted)?,
            format if format.compressed_tar() => {
                read_compressed_tar_entry(&handle.0, token.0.clone(), index, offset, wanted)?
            }
            _ => return Err(ReadOutcome::Error("unsupported archive format".into())),
        };
        check_read_progress(&handle.0, &token.0)?;
        Ok((bytes, offset.saturating_add(wanted as u64) < entry.size))
    })();

    match result {
        Ok((bytes, truncated)) => ReadOutcome::Read(bytes, truncated),
        Err(outcome) => outcome,
    }
}

fn archive_entry_read_async(
    cx: &mut Context,
    handle: SteelArchiveHandle,
    token: SteelArchiveCancelToken,
    index: usize,
    offset: u64,
    length: usize,
    callback: SteelVal,
) -> anyhow::Result<()> {
    let rooted = callback.as_rooted();
    let future = async move {
        let outcome = tokio::task::spawn_blocking(move || {
            read_archive_entry(handle, token, index, offset, length)
        })
        .await
        .unwrap_or_else(|error| ReadOutcome::Error(error.to_string()));
        Ok::<_, helix_lsp::Error>(ArchiveReadCallbackValue(outcome))
    };
    super::super::create_callback(cx, future, rooted)
}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn("archive-cancel-token", archive_cancel_token)
        .register_fn("archive-cancel!", archive_cancel)
        .register_fn_with_ctx(CTX, "archive-open-async", archive_open_async)
        .register_fn("archive-close!", archive_close)
        .register_fn("archive-path", archive_path)
        .register_fn("archive-format", archive_format)
        .register_fn("archive-fingerprint", archive_fingerprint)
        .register_fn("archive-entry-count", archive_entry_count)
        .register_fn("archive-closed?", archive_closed)
        .register_fn("archive-stale?", archive_stale)
        .register_fn("archive-entries-page", archive_entries_page)
        .register_fn_with_ctx(CTX, "archive-entry-read-async", archive_entry_read_async);
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use bzip2::Compression as BzipCompression;
    use flate2::Compression as GzipCompression;
    use tempfile::NamedTempFile;
    use zip::{
        write::{SimpleFileOptions, ZipWriter},
        CompressionMethod,
    };

    use super::*;

    fn fixture(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file
    }

    fn opened(file: &NamedTempFile) -> SteelArchiveHandle {
        match index_archive(
            file.path().to_string_lossy().into_owned(),
            SteelArchiveCancelToken::default(),
        ) {
            OpenOutcome::Opened(handle) => handle,
            OpenOutcome::Cancelled => panic!("unexpected archive cancellation"),
            OpenOutcome::Stale => panic!("unexpected stale archive"),
            OpenOutcome::Error(error) => panic!("unable to open archive fixture: {error}"),
        }
    }

    fn zip_bytes(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let cursor = io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        for (name, bytes, method) in entries {
            let options = SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn make_second_zip_name_duplicate(mut bytes: Vec<u8>) -> Vec<u8> {
        let position = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        let first_name_length = u16_at(&bytes, position + 28).unwrap() as usize;
        let first_extra_length = u16_at(&bytes, position + 30).unwrap() as usize;
        let first_comment_length = u16_at(&bytes, position + 32).unwrap() as usize;
        let second = position + 46 + first_name_length + first_extra_length + first_comment_length;
        assert_eq!(&bytes[second..second + 4], b"PK\x01\x02");
        let second_name = second + 46;
        assert_eq!(&bytes[second_name..second_name + 4], b"sAme");
        bytes[second_name..second_name + 4].copy_from_slice(b"same");
        bytes
    }

    fn mark_first_zip_member_encrypted(mut bytes: Vec<u8>) -> Vec<u8> {
        assert_eq!(&bytes[..4], b"PK\x03\x04");
        let local_flags = u16_at(&bytes, 6).unwrap() | 1;
        bytes[6..8].copy_from_slice(&local_flags.to_le_bytes());
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        let central_flags = u16_at(&bytes, central + 8).unwrap() | 1;
        bytes[central + 8..central + 10].copy_from_slice(&central_flags.to_le_bytes());
        bytes
    }

    fn append_raw_tar_entry(
        builder: &mut tar::Builder<Vec<u8>>,
        name: &[u8],
        entry_type: tar::EntryType,
        link: Option<&[u8]>,
        contents: &[u8],
    ) {
        let mut header = tar::Header::new_gnu();
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        if let Some(link) = link {
            header.as_mut_bytes()[157..157 + link.len()].copy_from_slice(link);
        }
        header.set_entry_type(entry_type);
        header.set_mode(0o644);
        header.set_mtime(1_700_000_000);
        header.set_size(contents.len() as u64);
        header.set_cksum();
        builder.append(&header, contents).unwrap();
    }

    fn tar_bytes() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        append_raw_tar_entry(
            &mut builder,
            b"folder/file.txt",
            tar::EntryType::Regular,
            None,
            b"abcdefghij",
        );
        append_raw_tar_entry(
            &mut builder,
            b"duplicate",
            tar::EntryType::Regular,
            None,
            b"one",
        );
        append_raw_tar_entry(
            &mut builder,
            b"duplicate",
            tar::EntryType::Regular,
            None,
            b"two",
        );
        append_raw_tar_entry(
            &mut builder,
            b"../hostile",
            tar::EntryType::Regular,
            None,
            b"metadata only path",
        );
        append_raw_tar_entry(
            &mut builder,
            b"/absolute",
            tar::EntryType::Regular,
            None,
            b"absolute path",
        );
        append_raw_tar_entry(
            &mut builder,
            "unicode-界".as_bytes(),
            tar::EntryType::Regular,
            None,
            b"unicode path",
        );
        append_raw_tar_entry(
            &mut builder,
            b"invalid-\xff-name",
            tar::EntryType::Regular,
            None,
            b"non utf8",
        );
        append_raw_tar_entry(
            &mut builder,
            b"symlink",
            tar::EntryType::Symlink,
            Some(b"../target"),
            b"",
        );
        append_raw_tar_entry(
            &mut builder,
            b"hardlink",
            tar::EntryType::Link,
            Some(b"folder/file.txt"),
            b"",
        );
        builder.into_inner().unwrap()
    }

    fn encode_tar(bytes: &[u8], format: ArchiveFormat) -> Vec<u8> {
        match format {
            ArchiveFormat::Tar => bytes.to_vec(),
            ArchiveFormat::TarGzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), GzipCompression::best());
                encoder.write_all(bytes).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveFormat::TarBzip2 => {
                let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), BzipCompression::best());
                encoder.write_all(bytes).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveFormat::TarXz => {
                let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
                encoder.write_all(bytes).unwrap();
                encoder.finish().unwrap()
            }
            ArchiveFormat::TarZstd => zstd::stream::encode_all(bytes, 3).unwrap(),
            _ => panic!("not a TAR encoding"),
        }
    }

    fn read_bytes(
        handle: &SteelArchiveHandle,
        index: usize,
        offset: u64,
        length: usize,
    ) -> (Vec<u8>, bool) {
        match read_archive_entry(
            handle.clone(),
            SteelArchiveCancelToken::default(),
            index,
            offset,
            length,
        ) {
            ReadOutcome::Read(bytes, truncated) => (bytes, truncated),
            ReadOutcome::Cancelled => panic!("unexpected read cancellation"),
            ReadOutcome::Stale => panic!("unexpected stale read"),
            ReadOutcome::Error(error) => panic!("archive read failed: {error}"),
        }
    }

    #[test]
    fn detects_zip_jar_and_wheel_from_validated_members() {
        let zip = fixture(&zip_bytes(&[(
            "plain.txt",
            b"plain",
            CompressionMethod::Stored,
        )]));
        assert_eq!(opened(&zip).0.format, ArchiveFormat::Zip);

        let jar = fixture(&zip_bytes(&[(
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\n",
            CompressionMethod::Deflated,
        )]));
        assert_eq!(opened(&jar).0.format, ArchiveFormat::Jar);

        let wheel = fixture(&zip_bytes(&[(
            "demo-1.0.dist-info/WHEEL",
            b"Wheel-Version: 1.0\n",
            CompressionMethod::Bzip2,
        )]));
        assert_eq!(opened(&wheel).0.format, ArchiveFormat::Wheel);
    }

    #[test]
    fn indexes_and_range_reads_every_tar_codec() {
        let tar = tar_bytes();
        for format in [
            ArchiveFormat::Tar,
            ArchiveFormat::TarGzip,
            ArchiveFormat::TarBzip2,
            ArchiveFormat::TarXz,
            ArchiveFormat::TarZstd,
        ] {
            let file = fixture(&encode_tar(&tar, format));
            let handle = opened(&file);
            assert_eq!(handle.0.format, format);
            assert_eq!(handle.0.entries.len(), 9);
            let (bytes, truncated) = read_bytes(&handle, 0, 2, 4);
            assert_eq!(bytes, b"cdef");
            assert!(truncated);
        }
    }

    #[test]
    fn preserves_duplicates_hostile_paths_links_and_lossy_names() {
        let file = fixture(&tar_bytes());
        let handle = opened(&file);
        assert_eq!(handle.0.entries[1].display_path, "duplicate");
        assert_eq!(handle.0.entries[2].display_path, "duplicate");
        assert_eq!(handle.0.entries[3].display_path, "../hostile");
        assert_eq!(handle.0.entries[3].kind, "unsafe-path");
        assert!(!handle.0.entries[3].readable);
        assert_eq!(handle.0.entries[4].display_path, "/absolute");
        assert_eq!(handle.0.entries[4].kind, "unsafe-path");
        assert!(!handle.0.entries[4].readable);
        assert_eq!(handle.0.entries[5].display_path, "unicode-界");
        assert!(handle.0.entries[5].readable);
        assert!(handle.0.entries[6].display_path.contains('\u{fffd}'));
        assert_eq!(handle.0.entries[6].kind, "unsafe-path");
        assert!(!handle.0.entries[6].readable);
        assert_eq!(handle.0.entries[7].kind, "symlink");
        assert_eq!(
            handle.0.entries[7].link_target.as_deref(),
            Some("../target")
        );
        assert!(!handle.0.entries[7].readable);
        assert_eq!(handle.0.entries[8].kind, "hard-link");
        assert_eq!(
            handle.0.entries[8].link_target.as_deref(),
            Some("folder/file.txt")
        );
        assert_eq!(tar_kind(tar::EntryType::GNUSparse), ("sparse", false));

        let zip_file = fixture(&zip_bytes(&[(
            "../zip-escape",
            b"metadata only",
            CompressionMethod::Stored,
        )]));
        let zip_handle = opened(&zip_file);
        assert_eq!(zip_handle.0.entries[0].kind, "unsafe-path");
        assert!(!zip_handle.0.entries[0].readable);
        assert!(matches!(
            read_archive_entry(
                zip_handle,
                SteelArchiveCancelToken::default(),
                0,
                0,
                32
            ),
            ReadOutcome::Error(message) if message.contains("metadata-only")
        ));
    }

    #[test]
    fn zip_compression_and_duplicate_indexes_are_stable() {
        let bytes = make_second_zip_name_duplicate(zip_bytes(&[
            ("same", b"stored", CompressionMethod::Stored),
            ("sAme", b"deflated", CompressionMethod::Deflated),
            ("other", b"zstandard", CompressionMethod::Zstd),
        ]));
        let file = fixture(&bytes);
        let handle = opened(&file);
        assert_eq!(handle.0.entries[0].index, 0);
        assert_eq!(handle.0.entries[1].index, 1);
        assert_eq!(handle.0.entries[0].display_path, "same");
        assert_eq!(handle.0.entries[1].display_path, "same");
        assert_eq!(read_bytes(&handle, 1, 0, 32).0, b"deflated");
        assert_eq!(read_bytes(&handle, 2, 1, 4).0, b"stan");
    }

    #[test]
    fn cancellation_limits_malformed_input_and_resource_closure_are_safe() {
        let malformed = fixture(b"this is not an archive");
        assert!(matches!(
            index_archive(
                malformed.path().to_string_lossy().into_owned(),
                SteelArchiveCancelToken::default()
            ),
            OpenOutcome::Error(_)
        ));

        let cancelled = SteelArchiveCancelToken::default();
        assert!(archive_cancel(&cancelled));
        assert!(!archive_cancel(&cancelled));
        assert!(matches!(
            index_archive(malformed.path().to_string_lossy().into_owned(), cancelled),
            OpenOutcome::Cancelled
        ));
        assert!(path_display(&vec![b'x'; MAX_PATH_BYTES + 1]).is_err());
        assert!(!path_is_metadata_only(b"safe/path"));
        assert!(!path_is_metadata_only("unicode-界".as_bytes()));
        assert!(path_is_metadata_only(b"../escape"));
        assert!(path_is_metadata_only(b"safe/../escape"));
        assert!(path_is_metadata_only(b"..\\escape"));
        assert!(path_is_metadata_only(b"/absolute"));
        assert!(path_is_metadata_only(b"\\\\server\\share"));
        assert!(path_is_metadata_only(b"C:\\absolute"));
        assert!(path_is_metadata_only(b"invalid-\xff"));
        assert!(path_is_metadata_only(b"nul\0name"));

        let file = fixture(&tar_bytes());
        let handle = opened(&file);
        let read_cancelled = SteelArchiveCancelToken::default();
        archive_cancel(&read_cancelled);
        assert!(matches!(
            read_archive_entry(handle.clone(), read_cancelled, 0, 0, 1),
            ReadOutcome::Cancelled
        ));
        assert!(matches!(
            read_archive_entry(
                handle.clone(),
                SteelArchiveCancelToken::default(),
                0,
                MAX_READ_OFFSET + 1,
                1
            ),
            ReadOutcome::Error(_)
        ));
        assert!(matches!(
            read_archive_entry(
                handle.clone(),
                SteelArchiveCancelToken::default(),
                0,
                0,
                MAX_READ_BYTES + 1
            ),
            ReadOutcome::Error(_)
        ));
        assert!(archive_entries_page(&handle, 0, 10_001).is_err());
        let mut budget = ScanBudgetReader::new(
            io::Cursor::new(vec![0; 32]),
            8,
            Arc::new(AtomicBool::new(false)),
        );
        let mut scratch = [0; 16];
        assert_eq!(budget.read(&mut scratch).unwrap(), 8);
        assert!(budget.read(&mut scratch).is_err());
        assert!(archive_close(&handle));
        assert!(!archive_close(&handle));
        assert!(archive_closed(&handle));
        assert!(archive_stale(&handle));
        assert!(matches!(
            read_archive_entry(handle, SteelArchiveCancelToken::default(), 0, 0, 1),
            ReadOutcome::Stale
        ));
    }

    #[test]
    fn source_changes_are_detected_before_reads() {
        let mut file = fixture(&tar_bytes());
        let handle = opened(&file);
        file.as_file_mut().write_all(b"changed").unwrap();
        file.as_file_mut().flush().unwrap();
        assert!(archive_stale(&handle));
        assert!(matches!(
            read_archive_entry(handle, SteelArchiveCancelToken::default(), 0, 0, 4),
            ReadOutcome::Stale
        ));
    }

    #[test]
    fn encrypted_zip_members_are_metadata_only() {
        let bytes = mark_first_zip_member_encrypted(zip_bytes(&[(
            "secret.txt",
            b"not actually encrypted",
            CompressionMethod::Stored,
        )]));
        let file = fixture(&bytes);
        let handle = opened(&file);
        assert!(handle.0.entries[0].encrypted);
        assert!(!handle.0.entries[0].readable);
        assert!(matches!(
            read_archive_entry(
                handle,
                SteelArchiveCancelToken::default(),
                0,
                0,
                4
            ),
            ReadOutcome::Error(message) if message.contains("encrypted")
        ));
    }
}
