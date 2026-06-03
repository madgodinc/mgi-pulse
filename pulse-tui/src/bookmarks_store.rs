//! Persistent bookmark sidecar.
//!
//! Bookmarks are line_ids on the in-memory engine. For a single-file
//! source they coincide with file-arrival order, so as long as the
//! file only grows (append-only) the saved line_ids still point at
//! the same records on the next open. We validate this by storing
//! `inode` and `size_at_save` alongside each source: if the inode
//! changed (rotated) or the file shrank (truncated / rewritten), the
//! saved line_ids are stale and dropped.
//!
//! Scope:
//!
//! - **Single file only.** Merge sources, stdin, pipes have no stable
//!   identity, so persistence is skipped for them. The in-memory
//!   bookmarks still work, just don't survive restart.
//! - **Per-tab.** A tab is identified by its index; restoration ignores
//!   tab indices past the current count (e.g. saved 5 tabs, opened
//!   with default 5, restore all; opened with `--tabs=3` if we ever
//!   add it, restore the first 3).
//! - **Bounded.** Capped at `MAX_SOURCES` (256). Eviction is LRU by
//!   `updated`. Files in the saved set that don't get opened don't
//!   expire on their own; they only get pushed out when something new
//!   needs the slot.
//! - **Flush-on-quit, not on toggle.** A toggle that doesn't end in a
//!   clean quit (crash, kill -9) loses the unsaved bookmarks. The
//!   alternative — write on every toggle — burns a syscall for every
//!   `b` and is not worth it for the bookmark use case.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MAX_SOURCES: usize = 256;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Sidecar {
    /// Indexed by canonical source path.
    #[serde(default)]
    pub sources: HashMap<String, SourceEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SourceEntry {
    /// Inode (or Windows equivalent — unused there for now).
    pub inode: u64,
    /// File size at the moment of save. Used to detect truncation /
    /// rewrite in place.
    pub size: u64,
    /// Per-tab bookmark line_ids. Index = tab index. Length can be
    /// less than the current tab count (extra tabs get empty
    /// bookmarks) or more (excess is ignored on restore).
    pub tabs: Vec<Vec<u64>>,
    /// RFC3339 of when this entry was last touched, for LRU.
    pub updated: String,
}

impl Sidecar {
    /// Default path: `$XDG_DATA_HOME/mgi-pulse/bookmarks.json` or
    /// `~/.local/share/mgi-pulse/bookmarks.json`.
    pub fn default_path() -> Option<PathBuf> {
        let base = if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(xdg)
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".local/share")
        } else {
            return None;
        };
        Some(base.join("mgi-pulse").join("bookmarks.json"))
    }

    /// Load a sidecar from disk. Missing file → empty sidecar (not an
    /// error). Malformed file → empty sidecar with a tracing warning,
    /// because we never want a corrupted sidecar to block the binary
    /// from starting.
    pub fn load_or_empty(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Sidecar>(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(),
                        "bookmark sidecar parse failed, starting fresh");
                    Sidecar::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Sidecar::default(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(),
                    "bookmark sidecar read failed, starting fresh");
                Sidecar::default()
            }
        }
    }

    /// Write the sidecar atomically (tmpfile + rename). Parent dir is
    /// created if it doesn't exist.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("serialize sidecar")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    /// Look up bookmarks for the given source. Returns the per-tab
    /// vectors only if the stored inode matches and the file hasn't
    /// shrunk (append-only growth is OK). On any mismatch the stale
    /// entry is removed and `None` returned.
    pub fn restore(
        &mut self,
        canonical_path: &str,
        current_inode: u64,
        current_size: u64,
    ) -> Option<Vec<Vec<u64>>> {
        let entry = self.sources.get(canonical_path)?;
        if entry.inode != current_inode {
            // Rotated or replaced — drop stale.
            self.sources.remove(canonical_path);
            return None;
        }
        if entry.size > current_size {
            // Truncated / rewritten shorter — line_ids past the new
            // size are gone; safer to drop the whole set.
            self.sources.remove(canonical_path);
            return None;
        }
        Some(entry.tabs.clone())
    }

    /// Insert / update an entry. Empty entries (no bookmarks anywhere)
    /// are removed instead — keeps the sidecar tidy. Applies LRU
    /// eviction if we'd exceed `MAX_SOURCES`.
    pub fn upsert(
        &mut self,
        canonical_path: &str,
        inode: u64,
        size: u64,
        tabs: Vec<Vec<u64>>,
        now_rfc3339: &str,
    ) {
        let empty = tabs.iter().all(|t| t.is_empty());
        if empty {
            self.sources.remove(canonical_path);
            return;
        }
        self.sources.insert(
            canonical_path.to_string(),
            SourceEntry {
                inode,
                size,
                tabs,
                updated: now_rfc3339.to_string(),
            },
        );
        self.evict_if_oversize();
    }

    fn evict_if_oversize(&mut self) {
        if self.sources.len() <= MAX_SOURCES {
            return;
        }
        // Collect (path, updated) pairs and sort ascending by `updated`
        // — oldest first. RFC3339 strings sort lexicographically the
        // same way they sort temporally, so a plain string sort works.
        let mut pairs: Vec<(String, String)> = self
            .sources
            .iter()
            .map(|(k, v)| (k.clone(), v.updated.clone()))
            .collect();
        pairs.sort_by(|a, b| a.1.cmp(&b.1));
        let to_drop = self.sources.len() - MAX_SOURCES;
        for (path, _) in pairs.into_iter().take(to_drop) {
            self.sources.remove(&path);
        }
    }
}

/// RFC3339 timestamp at second precision, UTC. Avoids pulling in chrono
/// — `std::time::SystemTime` + manual ymd math is overkill, so we use
/// a tiny formatter that's good enough for "the sidecar got touched".
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    format_unix_seconds_utc(secs)
}

fn format_unix_seconds_utc(unix_seconds: i64) -> String {
    let mut s = unix_seconds;
    let day_secs = 86_400_i64;
    let mut days = s.div_euclid(day_secs);
    s = s.rem_euclid(day_secs);
    let hour = s / 3600;
    let minute = (s % 3600) / 60;
    let second = s % 60;
    // Days since 1970-01-01 → (y, m, d). Howard Hinnant's algorithm.
    days += 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, minute, second
    )
}

/// Best-effort canonicalisation. Symlinks resolved when possible,
/// otherwise fall back to the input. The string form is what's used
/// as the sidecar key.
pub fn canonical_key(path: &Path) -> String {
    match fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Read the inode and current size of a path. Returns `None` for
/// platforms without an inode concept (Windows) — bookmarks
/// persistence is then disabled on that platform.
#[cfg(unix)]
pub fn source_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::metadata(path).ok()?;
    Some((md.ino(), md.size()))
}

#[cfg(not(unix))]
pub fn source_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempfile_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mgi-pulse-bookmarks-test-{}-{}.json", std::process::id(), name));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn load_missing_returns_empty() {
        let p = tempfile_path("missing");
        let s = Sidecar::load_or_empty(&p);
        assert!(s.sources.is_empty());
    }

    #[test]
    fn load_malformed_returns_empty() {
        let p = tempfile_path("malformed");
        fs::write(&p, b"this is not json").unwrap();
        let s = Sidecar::load_or_empty(&p);
        assert!(s.sources.is_empty());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let p = tempfile_path("roundtrip");
        let mut s = Sidecar::default();
        s.upsert(
            "/some/path.log",
            1234,
            5000,
            vec![vec![10, 20, 30], vec![], vec![100]],
            "2026-06-03T00:00:00Z",
        );
        s.save(&p).unwrap();
        let loaded = Sidecar::load_or_empty(&p);
        assert_eq!(loaded.sources.len(), 1);
        let entry = &loaded.sources["/some/path.log"];
        assert_eq!(entry.inode, 1234);
        assert_eq!(entry.size, 5000);
        assert_eq!(entry.tabs, vec![vec![10, 20, 30], vec![], vec![100]]);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn restore_matches_inode_and_size() {
        let mut s = Sidecar::default();
        s.upsert(
            "/p",
            42,
            1000,
            vec![vec![1, 2, 3]],
            "2026-06-03T00:00:00Z",
        );
        // Same inode, equal size → restored.
        assert!(s.restore("/p", 42, 1000).is_some());
        // Same inode, grew → restored (append-only).
        assert!(s.restore("/p", 42, 1500).is_some());
    }

    #[test]
    fn restore_drops_on_inode_change() {
        let mut s = Sidecar::default();
        s.upsert("/p", 42, 1000, vec![vec![1]], "2026-06-03T00:00:00Z");
        assert!(s.restore("/p", 99, 1000).is_none());
        // Stale entry removed.
        assert!(!s.sources.contains_key("/p"));
    }

    #[test]
    fn restore_drops_on_shrink() {
        let mut s = Sidecar::default();
        s.upsert("/p", 42, 1000, vec![vec![1]], "2026-06-03T00:00:00Z");
        // size 500 < saved 1000 → truncated/rewritten, drop.
        assert!(s.restore("/p", 42, 500).is_none());
        assert!(!s.sources.contains_key("/p"));
    }

    #[test]
    fn empty_tabs_remove_entry() {
        let mut s = Sidecar::default();
        s.upsert("/p", 42, 1000, vec![vec![1]], "2026-06-03T00:00:00Z");
        assert!(s.sources.contains_key("/p"));
        // Now upsert with empty bookmarks → entry removed.
        s.upsert("/p", 42, 1000, vec![vec![], vec![]], "2026-06-03T00:00:01Z");
        assert!(!s.sources.contains_key("/p"));
    }

    #[test]
    fn lru_evicts_oldest_over_cap() {
        let mut s = Sidecar::default();
        // Insert MAX_SOURCES + 1 entries with monotonic timestamps.
        for i in 0..=MAX_SOURCES {
            let ts = format!("2026-06-03T00:{:02}:{:02}Z", i / 60, i % 60);
            s.upsert(&format!("/p{}", i), i as u64, 100, vec![vec![1]], &ts);
        }
        assert_eq!(s.sources.len(), MAX_SOURCES);
        // The oldest (/p0) should be gone, the newest (/pMAX) should be present.
        assert!(!s.sources.contains_key("/p0"));
        assert!(s.sources.contains_key(&format!("/p{}", MAX_SOURCES)));
    }

    #[test]
    fn rfc3339_format_is_lex_sortable() {
        // 2026-06-03T00:01:00Z < 2026-06-03T00:02:00Z lexicographically.
        let a = format_unix_seconds_utc(1_770_667_260);
        let b = format_unix_seconds_utc(1_770_667_320);
        assert!(a < b);
    }
}
