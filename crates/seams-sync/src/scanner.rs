//! Folder scanner: walks a folder and produces the authoritative list of regular
//! files, with platform-independent relative paths.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use thiserror::Error;

/// Deleted content is parked here before a deletion is applied; never scanned or synced.
pub const TRASH_DIR_NAME: &str = ".seams-trash";

/// Incoming files are assembled here before an atomic rename; never scanned or synced.
pub const TMP_DIR_NAME: &str = ".seams-tmp";

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("folder `{0}` does not exist")]
    MissingFolder(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// `/`-separated path relative to the folder root, identical on all platforms.
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: u64,
}

/// Modification time in nanoseconds since the Unix epoch; 0 when unavailable.
pub fn mtime_ns(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Walks `root` recursively, sorted by path for stable comparisons. Symlinks are
/// skipped so a folder can never contain a cycle.
pub fn scan_folder(root: &Path) -> Result<Vec<ScannedFile>, ScanError> {
    if !root.is_dir() {
        return Err(ScanError::MissingFolder(root.display().to_string()));
    }

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str());
                if name == Some(TRASH_DIR_NAME) || name == Some(TMP_DIR_NAME) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let metadata = entry.metadata()?;
                files.push(ScannedFile {
                    rel_path: relative_path(root, &path),
                    size: metadata.len(),
                    mtime_ns: mtime_ns(&metadata),
                });
            }
        }
    }

    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

fn relative_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
