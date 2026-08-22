//! Canonical regular-file snapshots shared by read-only native plugin APIs.

use std::{
    fs::{self, File, Metadata},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileFingerprint {
    pub(super) len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_seconds: i64,
    #[cfg(unix)]
    change_nanoseconds: i64,
}

impl FileFingerprint {
    pub(super) fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            change_seconds: metadata.ctime(),
            #[cfg(unix)]
            change_nanoseconds: metadata.ctime_nsec(),
        }
    }

    pub(super) fn identity(&self) -> String {
        let modified = self
            .modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
        let modified_seconds = modified.map_or(0, |duration| duration.as_secs());
        let modified_nanos = modified.map_or(0, |duration| duration.subsec_nanos());

        #[cfg(unix)]
        return format!(
            "{modified_seconds}:{modified_nanos:09}:{}:{}:{}:{:09}",
            self.device,
            self.inode,
            self.change_seconds,
            self.change_nanoseconds.unsigned_abs()
        );

        #[cfg(not(unix))]
        format!("{modified_seconds}:{modified_nanos:09}")
    }
}

pub(super) fn open_canonical_regular(
    path: &str,
    file_owner: &str,
    viewer_owner: &str,
) -> anyhow::Result<(File, PathBuf, FileFingerprint)> {
    if path.trim().is_empty() || path.contains("://") || path.starts_with("file:") {
        anyhow::bail!("{file_owner} path must be a non-empty local path");
    }
    let canonical = fs::canonicalize(path)?;
    let file = File::open(&canonical)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("{viewer_owner} accepts regular files only");
    }
    if metadata.len() > usize::MAX as u64 {
        anyhow::bail!("{file_owner} is too large for this platform");
    }
    let fingerprint = FileFingerprint::from_metadata(&metadata);
    if FileFingerprint::from_metadata(&fs::metadata(&canonical)?) != fingerprint {
        anyhow::bail!("{file_owner} changed while it was being opened");
    }
    Ok((file, canonical, fingerprint))
}

pub(super) fn snapshot_stale(
    file: &File,
    path: &Path,
    fingerprint: &FileFingerprint,
    closed: bool,
) -> bool {
    closed
        || file
            .metadata()
            .map(|metadata| FileFingerprint::from_metadata(&metadata))
            .and_then(|opened| {
                fs::metadata(path)
                    .map(|metadata| (opened, FileFingerprint::from_metadata(&metadata)))
            })
            .map(|(opened, current)| opened != *fingerprint || current != *fingerprint)
            .unwrap_or(true)
}

pub(super) fn ensure_snapshot_fresh(
    file: &File,
    path: &Path,
    fingerprint: &FileFingerprint,
    closed: bool,
    owner: &str,
    refresh_action: &str,
) -> anyhow::Result<()> {
    if closed {
        anyhow::bail!("{owner} handle is closed");
    }
    if snapshot_stale(file, path, fingerprint, false) {
        anyhow::bail!("{owner} file changed on disk; {refresh_action}");
    }
    Ok(())
}
