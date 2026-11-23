//! Filesystem side of the engine: staging incoming files, atomic finalize with mtime
//! restore, trash moves, and conflict-copy placement.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use seams_identity::DeviceId;

use crate::scanner::{TMP_DIR_NAME, TRASH_DIR_NAME};

const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S";

/// Creates a staging file under `.seams-tmp/` for assembling an incoming file.
/// Returns the temp path and an already-created file handle.
pub fn stage_file(root: &Path, rel_path: &str) -> io::Result<(PathBuf, fs::File)> {
    let tmp_dir = root.join(TMP_DIR_NAME);
    fs::create_dir_all(&tmp_dir)?;
    let unique = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let tmp = tmp_dir.join(format!("{file_name}.{unique}.part"));
    let file = fs::File::create(&tmp)?;
    Ok((tmp, file))
}

/// Moves a staged file into its final location and restores the source mtime.
/// The rename is atomic on all supported filesystems.
pub fn finalize_incoming(
    root: &Path,
    rel_path: &str,
    staged: &Path,
    mtime_ns: u64,
) -> io::Result<()> {
    let target = root.join(rel_path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staged, &target)?;
    set_mtime_ns(&target, mtime_ns)
}

pub fn set_mtime_ns(path: &Path, mtime_ns: u64) -> io::Result<()> {
    let times = fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(mtime_ns));
    fs::File::options().write(true).open(path)?.set_times(times)
}

/// Moves an existing file into the trash: `.seams-trash/<stamp>/<rel_path>`,
/// creating intermediate directories. Returns the trash path.
pub fn trash_file(root: &Path, rel_path: &str, now: DateTime<Utc>) -> io::Result<PathBuf> {
    let stamp = now.format(STAMP_FORMAT).to_string();
    let destination = root.join(TRASH_DIR_NAME).join(stamp).join(rel_path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let source = root.join(rel_path);
    fs::rename(&source, &destination)?;
    Ok(destination)
}

/// Writes `content` to `rel_path` under `root` (used for conflict copies), restoring
/// mtime. Parent directories are created as needed.
pub fn write_file(root: &Path, rel_path: &str, content: &[u8], mtime_ns: u64) -> io::Result<()> {
    let target = root.join(rel_path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, content)?;
    set_mtime_ns(&target, mtime_ns)
}

/// Renames our edited file to its conflict-copy name, freeing the original name for
/// the peer's version (CONFLICTS.md §3, when this side is the ConflictCopy).
pub fn rename_to_conflict_copy(root: &Path, from_rel: &str, to_rel: &str) -> io::Result<()> {
    let source = root.join(from_rel);
    let target = root.join(to_rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&source, &target)
}

/// Deterministic date used in conflict copy names: both devices derive it from the
/// max mtime of the two competing versions, so names converge without round-trips.
pub fn conflict_date(local_mtime_ns: u64, remote_mtime_ns: u64) -> DateTime<Utc> {
    let max = local_mtime_ns.max(remote_mtime_ns);
    let secs = (max / 1_000_000_000) as i64;
    let nanos = (max % 1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(Utc::now)
}

/// Full conflict-copy name for the version held by `holder` (CONFLICTS.md §3).
pub fn conflict_copy_name(
    original: &str,
    holder: &DeviceId,
    local_mtime_ns: u64,
    remote_mtime_ns: u64,
) -> String {
    crate::trash::conflict_name(original, holder, conflict_date(local_mtime_ns, remote_mtime_ns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{TMP_DIR_NAME, TRASH_DIR_NAME};
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-apply-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stage_finalize_restores_content_and_mtime() {
        let root = temp_dir("finalize");
        let (staged, mut file) = stage_file(&root, "photos/a.jpg").unwrap();
        file.write_all(b"jpeg-bytes").unwrap();
        drop(file);

        finalize_incoming(&root, "photos/a.jpg", &staged, 1_700_000_000_123_456_789).unwrap();
        assert!(!staged.exists());
        let target = root.join("photos/a.jpg");
        assert_eq!(fs::read(&target).unwrap(), b"jpeg-bytes");
        let meta = fs::metadata(&target).unwrap();
        let mtime = meta.modified().unwrap();
        let ns = mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        assert_eq!(ns, 1_700_000_000_123_456_789);
    }

    #[test]
    fn trash_move_preserves_relative_path() {
        let root = temp_dir("trash");
        fs::create_dir_all(root.join("music")).unwrap();
        fs::write(root.join("music/song.mp3"), b"data").unwrap();
        let now = Utc::now();

        let trashed = trash_file(&root, "music/song.mp3", now).unwrap();
        assert!(!root.join("music/song.mp3").exists());
        assert!(trashed.starts_with(root.join(TRASH_DIR_NAME)));
        assert!(trashed.ends_with("music/song.mp3"));
    }

    #[test]
    fn conflict_copy_name_is_deterministic_across_clocks() {
        let holder = DeviceId([9u8; 32]);
        // Two machines at different wall-clock times derive the same name.
        let a = conflict_copy_name("doc.md", &holder, 100, 200);
        let b = conflict_copy_name("doc.md", &holder, 100, 200);
        assert_eq!(a, b);
        assert!(a.contains("doc") && a.contains(".md"));
        assert_ne!(conflict_copy_name("doc.md", &holder, 100, 200), {
            let other = DeviceId([10u8; 32]);
            conflict_copy_name("doc.md", &other, 100, 200)
        });
    }

    #[test]
    fn scanner_skips_tmp_dir() {
        let root = temp_dir("tmp-skip");
        fs::create_dir_all(root.join(TMP_DIR_NAME)).unwrap();
        fs::write(root.join(TMP_DIR_NAME).join("x.part"), b"partial").unwrap();
        fs::write(root.join("real.txt"), b"data").unwrap();
        let files = crate::scanner::scan_folder(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel_path, "real.txt");
    }
}
