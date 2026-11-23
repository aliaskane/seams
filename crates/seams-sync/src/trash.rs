//! Trash: deleted content is parked under `.seams-trash/<timestamp>/<name>` and purged
//! after the retention window (CONFLICTS.md §4).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use seams_identity::DeviceId;

use super::scanner::TRASH_DIR_NAME;

pub const TRASH_RETENTION_DAYS: i64 = 14;

const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S";

/// Where a deleted file's content is parked, e.g.
/// `.seams-trash/20261123T101530/photos/a.jpg` (sub-paths of the original are kept).
pub fn trash_path(original: &str, now: DateTime<Utc>) -> PathBuf {
    Path::new(TRASH_DIR_NAME)
        .join(now.format(STAMP_FORMAT).to_string())
        .join(original)
}

/// `thesis.docx (conflict from abcdefg, 2026-11-23)` — readable, sortable, and never
/// scanned back as a fresh file because the engine treats the suffix as conflict residue.
pub fn conflict_name(original: &str, holder: &DeviceId, now: DateTime<Utc>) -> String {
    let suffix = format!(" (conflict from {}, {})", holder.short(), now.format("%Y-%m-%d"));
    match original.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}{suffix}.{ext}"),
        _ => format!("{original}{suffix}"),
    }
}

/// Removes trash directories older than the retention window. Returns how many were
/// removed. Unparsable directory names are left alone.
pub fn purge_trash(root: &Path, now: DateTime<Utc>) -> std::io::Result<u32> {
    let trash_dir = root.join(TRASH_DIR_NAME);
    let cutoff = now - Duration::days(TRASH_RETENTION_DAYS);
    let mut removed = 0;

    if !trash_dir.is_dir() {
        return Ok(0);
    }

    for entry in std::fs::read_dir(&trash_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(stamp) = NaiveDateTime::parse_from_str(&name, STAMP_FORMAT) else {
            continue;
        };
        let created = DateTime::<Utc>::from_naive_utc_and_offset(stamp, Utc);
        if created < cutoff {
            std::fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }

    Ok(removed)
}
