// SPDX-License-Identifier: MPL-2.0
//! Per-platform cook key ledger and atomic cache writes (ADR-875).
//!
//! The asset database (`.regia/assetdb/index.bin`, [`AssetDatabase`]) is shared by every
//! platform a project cooks for, and the desktop platforms compute the same cache key for the same
//! source. A database row therefore says "some platform last cooked these bytes", not "this
//! platform's cache holds them". Cooking Linux, then Windows, then editing a source and cooking
//! Linux again left a row whose key matched the edited source while the Windows cache still held
//! the old output, and the next Windows cook skipped it: a stale pak.
//!
//! The ledger is the per-platform answer. It lives in the platform's cooked cache directory
//! (`cook_keys.bin`, beside `asset_manifest.bin`, never packed: the pak writer packs manifest
//! entries only) and records the cache key each cooked output in *that* directory was written
//! with. An incremental cook skips a source only when the ledger, not the shared row, holds the
//! current key.
//!
//! The ledger only vouches for sources the database also has a row for, so deleting the database
//! (or one that no longer decodes) still rebuilds everything, as `docs/specs/12-assets.md` § 6
//! promises.
//!
//! A cache written before the ledger existed has none. Its rows are adopted when this platform's
//! manifest is at least as new as the database, which means this platform cooked last and the rows
//! describe its cache; otherwise the ledger starts empty and the platform recooks once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_asset::asset_db_path;
use serde::{Deserialize, Serialize};

use crate::db::AssetDatabase;

/// File name of the ledger inside a platform's cooked cache directory.
pub(crate) const LEDGER_FILE: &str = "cook_keys.bin";

/// Layout version of `cook_keys.bin`. Another version is treated as no ledger (a cache file:
/// the platform recooks once).
const LEDGER_VERSION: u32 = 1;

/// The on-disk ledger (postcard).
#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    keys: BTreeMap<String, [u8; 32]>,
}

/// Version of the ledger's optional trailing section (the last shared cook cache summary,
/// ADR-2612). Older engines decode the [`LedgerFile`] and ignore the bytes after it; a trailer of
/// another version is ignored.
const TRAILER_VERSION: u32 = 1;

/// The section after the [`LedgerFile`] (postcard), written once a cook used a shared cache.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerTrailer {
    version: u32,
    shared_cache: crate::cook_cache::SharedCacheSummary,
}

/// Cache keys of the cooked outputs in one platform's cache directory, by source path.
#[derive(Debug)]
pub(crate) struct CookLedger {
    path: PathBuf,
    keys: BTreeMap<String, [u8; 32]>,
    /// The last shared cook cache summary, kept byte for byte by cooks without one.
    shared: Option<crate::cook_cache::SharedCacheSummary>,
}

/// Decodes a ledger file: the keys, and the trailer when one follows.
fn decode_ledger(
    bytes: &[u8],
) -> postcard::Result<(LedgerFile, Option<crate::cook_cache::SharedCacheSummary>)> {
    let (file, rest) = postcard::take_from_bytes::<LedgerFile>(bytes)?;
    let shared = postcard::from_bytes::<LedgerTrailer>(rest)
        .ok()
        .filter(|t| t.version == TRAILER_VERSION)
        .map(|t| t.shared_cache);
    Ok((file, shared))
}

/// The shared cook cache summary of the last cook of `cache_dir` that used one (ADR-2612).
pub(crate) fn last_shared_summary(cache_dir: &Path) -> Option<crate::cook_cache::SharedCacheSummary> {
    let bytes = std::fs::read(cache_dir.join(LEDGER_FILE)).ok()?;
    decode_ledger(&bytes).ok()?.1
}

impl CookLedger {
    /// Opens the ledger of `cache_dir`. A missing ledger adopts the database rows when this
    /// platform cooked last (see the module docs); a damaged one starts empty. Keys for sources
    /// without a database row are dropped.
    pub(crate) fn open(cache_dir: &Path, project_root: Option<&Path>, db: &AssetDatabase) -> Self {
        let path = cache_dir.join(LEDGER_FILE);
        let mut shared = None;
        let mut keys = match std::fs::read(&path) {
            Ok(bytes) => match decode_ledger(&bytes) {
                Ok((file, trailer)) if file.version == LEDGER_VERSION => {
                    shared = trailer;
                    file.keys
                }
                Ok((file, _)) => {
                    tracing::warn!(
                        target: "regia_asset_pipeline",
                        path = %path.display(),
                        version = file.version,
                        "cook key ledger has another version; this platform recooks"
                    );
                    BTreeMap::new()
                }
                Err(error) => {
                    tracing::warn!(
                        target: "regia_asset_pipeline",
                        path = %path.display(),
                        %error,
                        "cook key ledger is damaged; this platform recooks"
                    );
                    BTreeMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if legacy_rows_describe(cache_dir, project_root) {
                    db.entries
                        .iter()
                        .map(|(path, row)| (path.clone(), row.cache_key))
                        .collect()
                } else {
                    BTreeMap::new()
                }
            }
            Err(_) => BTreeMap::new(),
        };
        keys.retain(|path, _| db.entries.contains_key(path));
        Self { path, keys, shared }
    }

    /// Replaces the last shared cook cache summary.
    pub(crate) fn set_shared_summary(&mut self, summary: crate::cook_cache::SharedCacheSummary) {
        self.shared = Some(summary);
    }

    /// True when the output cooked for `rel_path` in this cache was written with `key`.
    pub(crate) fn is_current(&self, rel_path: &str, key: &[u8; 32]) -> bool {
        self.keys.get(rel_path) == Some(key)
    }

    /// Records that `rel_path`'s output in this cache now matches `key`.
    pub(crate) fn record(&mut self, rel_path: &str, key: [u8; 32]) {
        self.keys.insert(rel_path.to_string(), key);
    }

    /// Drops `rel_path` (its output was removed from this cache).
    pub(crate) fn forget(&mut self, rel_path: &str) {
        self.keys.remove(rel_path);
    }

    /// Writes the ledger atomically, unless the file already holds these bytes: a cook that
    /// changed nothing leaves every file of the cache untouched.
    pub(crate) fn save(&self) -> std::io::Result<()> {
        let file = LedgerFile {
            version: LEDGER_VERSION,
            keys: self.keys.clone(),
        };
        let mut bytes = postcard::to_allocvec(&file).map_err(std::io::Error::other)?;
        if let Some(summary) = &self.shared {
            let trailer = LedgerTrailer {
                version: TRAILER_VERSION,
                shared_cache: summary.clone(),
            };
            bytes.extend(postcard::to_allocvec(&trailer).map_err(std::io::Error::other)?);
        }
        if std::fs::read(&self.path).is_ok_and(|current| current == bytes) {
            return Ok(());
        }
        write_atomic(&self.path, &bytes)
    }
}

/// True when the shared database rows describe `cache_dir`: its manifest was written at or after
/// the database (`cook_project` and `cook_sources` save the database, then the manifest). Without
/// a project root (the public [`crate::cook_terrain`] entry point) the rows are trusted, as they
/// were before the ledger.
fn legacy_rows_describe(cache_dir: &Path, project_root: Option<&Path>) -> bool {
    let Some(root) = project_root else {
        return true;
    };
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    match (
        modified(&cache_dir.join("asset_manifest.bin")),
        modified(&asset_db_path(root)),
    ) {
        (Some(manifest), Some(db)) => manifest >= db,
        _ => false,
    }
}

static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` through a sibling temporary file and a rename, so a reader (the
/// running game's dev-assets watcher, a second cook) sees the old file or the new one, never a
/// torn one, and a cook killed mid-write leaves the previous file in place. The temporary name
/// carries the process id and a counter, so two cooks of one project do not share it.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Like [`write_atomic`], but never replaces an existing file: `Ok(false)` when `path` already
/// exists (another cook created it first), `Ok(true)` when this call published it. The bytes go
/// to a uniquely named temp file that is then hard-linked into place (on Windows moved into place
/// with a no-replace rename, ADR-8504), so a reader sees either no file or the whole file. A volume without hard links falls back to `create_new` on the target
/// (still no-replace; a reader may then see the file before its last byte).
pub(crate) fn write_new_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    // Windows moves the temp into place without replacing: a hard link plus an unlink of the
    // temp leaves the temp name delete-pending while a scanner reads the new file, and every
    // replacing write of `path` (the cook's own `.meta` rewrite, the editor's import settings)
    // is then refused with access denied for up to ~1.3 s (ADR-8504).
    #[cfg(windows)]
    let linked = regia_platform::rename_no_replace(&tmp, path);
    #[cfg(not(windows))]
    let linked = std::fs::hard_link(&tmp, path);
    if cfg!(not(windows)) || linked.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => {
            use std::io::Write;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    if let Err(e) = file.write_all(bytes) {
                        // Never leave a partial file standing: every later reader would fail on
                        // it and `create_new` would never replace it (audit F8).
                        drop(file);
                        let _ = std::fs::remove_file(path);
                        return Err(e);
                    }
                    Ok(true)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbEntry;
    use regia_asset::AssetId;

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-ledger-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn db_with(path: &str, key: [u8; 32]) -> AssetDatabase {
        let mut db = AssetDatabase::default();
        db.entries.insert(
            path.to_string(),
            DbEntry {
                source_path: path.to_string(),
                asset_id: AssetId::new_random(),
                source_hash: [0; 32],
                settings_hash: [0; 32],
                importer: "test".into(),
                importer_version: 1,
                cache_key: key,
                cooked_rel: "x.fckd".into(),
            },
        );
        db
    }

    #[test]
    fn a_saved_ledger_reopens_with_its_keys() {
        let dir = temp_dir("roundtrip");
        let mut db = db_with("assets/a.png", [9; 32]);
        db.entries.extend(db_with("assets/b.png", [9; 32]).entries);
        let mut ledger = CookLedger::open(&dir, Some(&dir), &db);
        assert!(!ledger.is_current("assets/a.png", &[1; 32]));
        ledger.record("assets/a.png", [1; 32]);
        ledger.record("assets/b.png", [2; 32]);
        ledger.forget("assets/b.png");
        ledger.save().expect("save");
        let reopened = CookLedger::open(&dir, Some(&dir), &db);
        assert!(reopened.is_current("assets/a.png", &[1; 32]));
        assert!(!reopened.is_current("assets/a.png", &[2; 32]));
        assert!(!reopened.is_current("assets/b.png", &[2; 32]));

        // Without the database rows (deleted or damaged index) the ledger vouches for nothing.
        let empty = CookLedger::open(&dir, Some(&dir), &AssetDatabase::default());
        assert!(!empty.is_current("assets/a.png", &[1; 32]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_ledger_trusts_nothing() {
        let dir = temp_dir("damaged");
        std::fs::write(dir.join(LEDGER_FILE), [0xff; 7]).expect("write");
        let db = db_with("assets/a.png", [3; 32]);
        let ledger = CookLedger::open(&dir, None, &db);
        assert!(!ledger.is_current("assets/a.png", &[3; 32]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_ledger_adopts_rows_only_when_this_platform_cooked_last() {
        let root = temp_dir("legacy");
        let db = db_with("assets/a.png", [4; 32]);
        db.save(&root).expect("db");
        let cache = root.join(".regia").join("cooked").join("linux");
        std::fs::create_dir_all(&cache).expect("cache");

        // No manifest in this cache: another platform's rows cannot be trusted here.
        let ledger = CookLedger::open(&cache, Some(&root), &db);
        assert!(!ledger.is_current("assets/a.png", &[4; 32]));

        // The manifest written after the database: this platform cooked last.
        std::fs::write(cache.join("asset_manifest.bin"), [0u8]).expect("manifest");
        let db_time = std::fs::metadata(asset_db_path(&root))
            .and_then(|m| m.modified())
            .expect("db mtime");
        let manifest = std::fs::File::options()
            .write(true)
            .open(cache.join("asset_manifest.bin"))
            .expect("open manifest");
        manifest
            .set_modified(db_time + std::time::Duration::from_secs(5))
            .expect("set mtime");
        let ledger = CookLedger::open(&cache, Some(&root), &db);
        assert!(ledger.is_current("assets/a.png", &[4; 32]));

        // The manifest older than the database: another platform cooked since.
        manifest
            .set_modified(db_time - std::time::Duration::from_secs(5))
            .expect("set mtime");
        let ledger = CookLedger::open(&cache, Some(&root), &db);
        assert!(!ledger.is_current("assets/a.png", &[4; 32]));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-2612: the shared cache summary rides after the keys. A decoder of the keys alone (an
    /// older engine) still reads the file, a cook without a shared cache keeps the summary byte for
    /// byte, and a trailer of another version is ignored.
    #[test]
    fn the_shared_cache_summary_trails_the_keys_compatibly() {
        let dir = temp_dir("trailer");
        let db = db_with("assets/a.png", [5; 32]);
        let mut ledger = CookLedger::open(&dir, None, &db);
        ledger.record("assets/a.png", [6; 32]);
        ledger.save().expect("save");
        let plain = std::fs::read(dir.join(LEDGER_FILE)).expect("read");
        assert_eq!(last_shared_summary(&dir), None);

        let summary = crate::cook_cache::SharedCacheSummary {
            location: "/share".into(),
            mode: "read_write".into(),
            hits: 3,
            bytes_read: 99,
            ..Default::default()
        };
        ledger.set_shared_summary(summary.clone());
        ledger.save().expect("save");
        let with = std::fs::read(dir.join(LEDGER_FILE)).expect("read");
        assert!(with.starts_with(&plain) && with.len() > plain.len());
        let old: LedgerFile = postcard::from_bytes(&with).expect("an older engine reads the keys");
        assert_eq!(old.keys.get("assets/a.png"), Some(&[6; 32]));
        assert_eq!(last_shared_summary(&dir), Some(summary.clone()));

        // A cook without a shared cache keeps the trailer unchanged.
        let reopened = CookLedger::open(&dir, None, &db);
        assert!(reopened.is_current("assets/a.png", &[6; 32]));
        reopened.save().expect("save");
        assert_eq!(std::fs::read(dir.join(LEDGER_FILE)).expect("read"), with);

        // Another trailer version: the keys stay, the summary is ignored.
        let mut other = plain.clone();
        other.extend(
            postcard::to_allocvec(&LedgerTrailer {
                version: TRAILER_VERSION + 1,
                shared_cache: summary,
            })
            .expect("encode"),
        );
        std::fs::write(dir.join(LEDGER_FILE), &other).expect("write");
        assert_eq!(last_shared_summary(&dir), None);
        assert!(CookLedger::open(&dir, None, &db).is_current("assets/a.png", &[6; 32]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_writes_replace_the_whole_file_and_leave_no_temporary() {
        let dir = temp_dir("atomic");
        let path = dir.join("out.bin");
        write_atomic(&path, b"first, longer contents").expect("write");
        write_atomic(&path, b"second").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "only the target remains: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `write_new_atomic` publishes once (a second writer finds the name taken and the first
    /// file stays), leaves no temporary, and the published file is replaceable at once, as the
    /// cook's `.meta` rewrite and the editor's import settings need. On Windows the publish was a
    /// hard link whose unlinked temp could stay delete-pending and refuse that replace for up to
    /// ~1.3 s (ADR-8504); the loop gives the old way many chances to show it.
    #[test]
    fn a_new_file_is_published_once_and_is_replaceable_at_once() {
        let dir = temp_dir("new-atomic");
        for i in 0..100 {
            let path = dir.join(format!("source{i}.png.meta"));
            assert!(write_new_atomic(&path, b"first").expect("publish"), "published");
            assert!(
                !write_new_atomic(&path, b"second").expect("taken"),
                "the name is taken"
            );
            assert_eq!(
                std::fs::read(&path).expect("read"),
                b"first",
                "the first file stays"
            );
            write_atomic(&path, b"replaced").expect("the new file is replaceable at once");
            assert_eq!(std::fs::read(&path).expect("read"), b"replaced");
        }
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "no temporary is left: {temps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
