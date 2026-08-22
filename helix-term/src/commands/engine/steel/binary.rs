//! Bounded, read-only binary file access for Steel plugins.
//!
//! Files remain native handles so raw bytes never pass through UTF-8 strings.
//! Every operation uses positional I/O, validates its bounds, and verifies that
//! the opened file and its path still identify the original snapshot.

use std::{
    fs::File,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use steel::{
    rvals::{Custom, IntoSteelVal, SteelByteVector},
    steel_vm::{builtin::BuiltInModule, register_fn::RegisterFn},
    SteelErr, SteelVal,
};

use super::{
    file_snapshot::{
        ensure_snapshot_fresh, open_canonical_regular, snapshot_stale, FileFingerprint,
    },
    Context, CTX,
};

const MAX_READ_BYTES: usize = 1024 * 1024;
const MAX_PATTERN_BYTES: usize = 4 * 1024;
const SEARCH_CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct BinaryFile {
    file: File,
    path: PathBuf,
    fingerprint: FileFingerprint,
    closed: AtomicBool,
}

#[derive(Clone)]
struct SteelBinaryFile(Arc<BinaryFile>);

impl Custom for SteelBinaryFile {}

#[derive(Clone, Default)]
struct SteelBinarySearchToken(Arc<AtomicBool>);

impl Custom for SteelBinarySearchToken {}

#[derive(Debug)]
enum SearchOutcome {
    Found(usize),
    NotFound,
    Cancelled,
    Stale,
    Error(String),
}

struct BinarySearchCallbackValue(SearchOutcome);

impl TryInto<SteelVal> for BinarySearchCallbackValue {
    type Error = SteelErr;

    fn try_into(self) -> Result<SteelVal, Self::Error> {
        let values = match self.0 {
            SearchOutcome::Found(offset) => {
                vec!["found".into_steelval()?, (offset as isize).into_steelval()?]
            }
            SearchOutcome::NotFound => vec!["not-found".into_steelval()?, false.into_steelval()?],
            SearchOutcome::Cancelled => vec!["cancelled".into_steelval()?, false.into_steelval()?],
            SearchOutcome::Stale => vec!["stale".into_steelval()?, false.into_steelval()?],
            SearchOutcome::Error(message) => {
                vec!["error".into_steelval()?, message.into_steelval()?]
            }
        };
        Ok(SteelVal::ListV(values.into()))
    }
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

fn read_exact_window(file: &File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    let mut filled = 0;
    while filled < length {
        match read_at(file, &mut bytes[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

fn file_stale(file: &BinaryFile) -> bool {
    snapshot_stale(
        &file.file,
        &file.path,
        &file.fingerprint,
        file.closed.load(Ordering::Acquire),
    )
}

fn ensure_fresh(file: &BinaryFile) -> anyhow::Result<()> {
    ensure_snapshot_fresh(
        &file.file,
        &file.path,
        &file.fingerprint,
        file.closed.load(Ordering::Acquire),
        "binary file",
        "refresh the viewer",
    )
}

fn binary_file_open(path: String) -> anyhow::Result<SteelBinaryFile> {
    let (file, canonical, fingerprint) =
        open_canonical_regular(&path, "binary file", "binary viewer")?;
    Ok(SteelBinaryFile(Arc::new(BinaryFile {
        file,
        path: canonical,
        fingerprint,
        closed: AtomicBool::new(false),
    })))
}

fn binary_file_close(file: &SteelBinaryFile) -> bool {
    !file.0.closed.swap(true, Ordering::AcqRel)
}

fn binary_file_path(file: &SteelBinaryFile) -> anyhow::Result<String> {
    file.0
        .path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("binary file path is not valid UTF-8"))
}

fn binary_file_size(file: &SteelBinaryFile) -> usize {
    file.0.fingerprint.len as usize
}

fn binary_file_stale(file: &SteelBinaryFile) -> bool {
    file_stale(&file.0)
}

fn binary_file_read(
    file: &SteelBinaryFile,
    offset: usize,
    length: usize,
) -> anyhow::Result<SteelVal> {
    if length > MAX_READ_BYTES {
        anyhow::bail!("binary reads are limited to {MAX_READ_BYTES} bytes");
    }
    ensure_fresh(&file.0)?;
    let size = file.0.fingerprint.len as usize;
    if offset > size {
        anyhow::bail!("binary read offset {offset} exceeds file size {size}");
    }
    let wanted = length.min(size - offset);
    let bytes = read_exact_window(&file.0.file, offset as u64, wanted)?;
    ensure_fresh(&file.0)?;
    Ok(SteelVal::ByteVector(SteelByteVector::new(bytes)))
}

fn binary_search_token() -> SteelBinarySearchToken {
    SteelBinarySearchToken::default()
}

fn binary_search_cancel(token: &SteelBinarySearchToken) -> bool {
    !token.0.swap(true, Ordering::AcqRel)
}

fn checked_pattern(pattern: Vec<usize>) -> anyhow::Result<Vec<u8>> {
    if pattern.is_empty() {
        anyhow::bail!("binary search pattern cannot be empty");
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        anyhow::bail!("binary search patterns are limited to {MAX_PATTERN_BYTES} bytes");
    }
    pattern
        .into_iter()
        .map(|byte| {
            u8::try_from(byte).map_err(|_| anyhow::anyhow!("binary search bytes must be 0..255"))
        })
        .collect()
}

fn search_forward(
    file: &BinaryFile,
    token: &AtomicBool,
    pattern: &[u8],
    start: usize,
    end: usize,
) -> SearchOutcome {
    let overlap = pattern.len().saturating_sub(1);
    let mut cursor = start;
    while cursor < end {
        if token.load(Ordering::Acquire) {
            return SearchOutcome::Cancelled;
        }
        if file_stale(file) {
            return SearchOutcome::Stale;
        }
        let candidate_bytes = (end - cursor).min(SEARCH_CHUNK_BYTES);
        let read_length = (candidate_bytes + overlap).min(end - cursor);
        let bytes = match read_exact_window(&file.file, cursor as u64, read_length) {
            Ok(bytes) => bytes,
            Err(error) => return SearchOutcome::Error(error.to_string()),
        };
        if token.load(Ordering::Acquire) {
            return SearchOutcome::Cancelled;
        }
        if file_stale(file) {
            return SearchOutcome::Stale;
        }
        if let Some(index) = bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
        {
            return SearchOutcome::Found(cursor + index);
        }
        if bytes.len() < read_length {
            break;
        }
        cursor += candidate_bytes;
    }
    if file_stale(file) {
        SearchOutcome::Stale
    } else {
        SearchOutcome::NotFound
    }
}

fn search_reverse(
    file: &BinaryFile,
    token: &AtomicBool,
    pattern: &[u8],
    start: usize,
    end: usize,
) -> SearchOutcome {
    let overlap = pattern.len().saturating_sub(1);
    let mut cursor = end;
    while cursor > start {
        if token.load(Ordering::Acquire) {
            return SearchOutcome::Cancelled;
        }
        if file_stale(file) {
            return SearchOutcome::Stale;
        }
        let base = cursor.saturating_sub(SEARCH_CHUNK_BYTES).max(start);
        let read_end = (cursor + overlap).min(end);
        let bytes = match read_exact_window(&file.file, base as u64, read_end - base) {
            Ok(bytes) => bytes,
            Err(error) => return SearchOutcome::Error(error.to_string()),
        };
        if token.load(Ordering::Acquire) {
            return SearchOutcome::Cancelled;
        }
        if file_stale(file) {
            return SearchOutcome::Stale;
        }
        if let Some(index) = bytes
            .windows(pattern.len())
            .rposition(|window| window == pattern)
        {
            let offset = base + index;
            if offset < cursor && offset + pattern.len() <= end {
                return SearchOutcome::Found(offset);
            }
        }
        cursor = base;
    }
    if file_stale(file) {
        SearchOutcome::Stale
    } else {
        SearchOutcome::NotFound
    }
}

fn search(
    file: &BinaryFile,
    token: &AtomicBool,
    pattern: &[u8],
    start: usize,
    end: usize,
    direction: &str,
) -> SearchOutcome {
    if start > end || end > file.fingerprint.len as usize {
        return SearchOutcome::Error("binary search range is outside the file".into());
    }
    if end - start < pattern.len() {
        return SearchOutcome::NotFound;
    }
    match direction {
        "forward" => search_forward(file, token, pattern, start, end),
        "reverse" => search_reverse(file, token, pattern, start, end),
        _ => SearchOutcome::Error("binary search direction must be forward or reverse".into()),
    }
}

fn binary_file_search(
    cx: &mut Context,
    file: SteelBinaryFile,
    token: SteelBinarySearchToken,
    pattern: Vec<usize>,
    start: usize,
    end: usize,
    direction: String,
    callback: SteelVal,
) -> anyhow::Result<()> {
    let pattern = checked_pattern(pattern)?;
    ensure_fresh(&file.0)?;
    if start > end || end > file.0.fingerprint.len as usize {
        anyhow::bail!("binary search range is outside the file");
    }
    if direction != "forward" && direction != "reverse" {
        anyhow::bail!("binary search direction must be forward or reverse");
    }
    let rooted = callback.as_rooted();
    let future = async move {
        let outcome = tokio::task::spawn_blocking(move || {
            search(&file.0, &token.0, &pattern, start, end, &direction)
        })
        .await
        .unwrap_or_else(|error| SearchOutcome::Error(error.to_string()));
        Ok::<_, helix_lsp::Error>(BinarySearchCallbackValue(outcome))
    };
    super::super::create_callback(cx, future, rooted)
}

pub(super) fn register(module: &mut BuiltInModule) {
    module
        .register_fn("binary-file-open", binary_file_open)
        .register_fn("binary-file-close!", binary_file_close)
        .register_fn("binary-file-path", binary_file_path)
        .register_fn("binary-file-size", binary_file_size)
        .register_fn("binary-file-stale?", binary_file_stale)
        .register_fn("binary-file-read", binary_file_read)
        .register_fn("binary-search-token", binary_search_token)
        .register_fn("binary-search-cancel!", binary_search_cancel)
        .register_fn_with_ctx(CTX, "binary-file-search", binary_file_search);
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn fixture(bytes: &[u8]) -> (tempfile::NamedTempFile, SteelBinaryFile) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        let opened = binary_file_open(file.path().to_string_lossy().into_owned()).unwrap();
        (file, opened)
    }

    #[test]
    fn reads_binary_windows_and_eof() {
        let (_fixture, file) = fixture(&[0, 0xff, b'a', 0x80, b'z']);
        let value = binary_file_read(&file, 1, 3).unwrap();
        assert_eq!(
            value,
            SteelVal::ByteVector(SteelByteVector::new(vec![0xff, b'a', 0x80]))
        );
        let value = binary_file_read(&file, 5, 16).unwrap();
        assert_eq!(
            value,
            SteelVal::ByteVector(SteelByteVector::new(Vec::new()))
        );
    }

    #[test]
    fn enforces_read_bounds_and_regular_files() {
        let (_fixture, file) = fixture(&[1, 2, 3]);
        assert!(binary_file_read(&file, 4, 1).is_err());
        assert!(binary_file_read(&file, 0, MAX_READ_BYTES + 1).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(binary_file_open(directory.path().to_string_lossy().into_owned()).is_err());
    }

    #[test]
    fn searches_in_both_directions_and_across_chunks() {
        let mut contents = vec![0; SEARCH_CHUNK_BYTES + 8];
        contents[SEARCH_CHUNK_BYTES - 1..SEARCH_CHUNK_BYTES + 3]
            .copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        contents[SEARCH_CHUNK_BYTES + 4..SEARCH_CHUNK_BYTES + 8]
            .copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let (_fixture, file) = fixture(&contents);
        let token = AtomicBool::new(false);
        assert!(matches!(
            search_forward(&file.0, &token, &[0xde, 0xad, 0xbe, 0xef], 0, contents.len()),
            SearchOutcome::Found(offset) if offset == SEARCH_CHUNK_BYTES - 1
        ));
        assert!(matches!(
            search_reverse(&file.0, &token, &[0xde, 0xad, 0xbe, 0xef], 0, contents.len()),
            SearchOutcome::Found(offset) if offset == SEARCH_CHUNK_BYTES + 4
        ));
    }

    #[test]
    fn cancellation_and_changes_are_observable() {
        let (mut fixture, file) = fixture(&[1, 2, 3, 4]);
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            search_forward(&file.0, &cancelled, &[4], 0, 4),
            SearchOutcome::Cancelled
        ));
        fixture.as_file_mut().write_all(&[5]).unwrap();
        fixture.as_file_mut().flush().unwrap();
        assert!(binary_file_stale(&file));
        assert!(binary_file_read(&file, 0, 1).is_err());
    }
}
