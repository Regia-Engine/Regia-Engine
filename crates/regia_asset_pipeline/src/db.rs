// SPDX-License-Identifier: MPL-2.0
//! Asset database cache (`docs/specs/12-assets.md` § 6).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::asset_db_path;
use regia_asset::{AssetId, MetaSidecar, meta_path_for};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use walkdir::WalkDir;

/// One row in the asset database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbEntry {
    /// Project-relative source path (`assets/...`).
    pub source_path: String,
    /// Asset id from `.meta`.
    pub asset_id: AssetId,
    /// BLAKE3 of source bytes at last cook.
    pub source_hash: [u8; 32],
    /// BLAKE3 of importer settings string.
    pub settings_hash: [u8; 32],
    /// Importer name.
    pub importer: String,
    /// Packed importer version.
    pub importer_version: u32,
    /// Cache key for the cooked output on this platform.
    pub cache_key: [u8; 32],
    /// Path to cooked file relative to cache dir.
    pub cooked_rel: String,
}

/// Postcard-serialised database (`index.bin`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDatabase {
    /// Rows keyed by source path for deterministic order.
    pub entries: BTreeMap<String, DbEntry>,
}

impl AssetDatabase {
    /// Load from disk or empty when missing.
    ///
    /// Creates the `.regia/assetdb` directory when it is absent, so a fresh
    /// project (or a per-test temp copy) can be opened and later [`Self::save`]d
    /// without racing a concurrent cook that is creating the same tree.
    ///
    /// The database is a cache (`docs/specs/12-assets.md` § 6): an index that does not decode
    /// (damaged, or written by another layout) is logged and treated as empty, so the next cook
    /// rebuilds it from the sources instead of failing every cook until someone deletes it.
    ///
    /// # Errors
    ///
    /// The directory cannot be created or the index cannot be read.
    pub fn open(project_root: &Path) -> Result<Self, DbError> {
        let path = asset_db_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !path.is_file() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(&path)?;
        match postcard::from_bytes(&bytes) {
            Ok(db) => Ok(db),
            Err(error) => {
                tracing::warn!(
                    target: "regia_asset_pipeline",
                    path = %path.display(),
                    %error,
                    "asset database does not decode; rebuilding it from the sources"
                );
                Ok(Self::default())
            }
        }
    }

    /// Atomic write via a uniquely named temp file + rename (two cooks of one project, say the
    /// live cooker and `regia cook`, never share the temp file).
    ///
    /// # Errors
    ///
    /// Encoding, the write or the rename failed.
    pub fn save(&self, project_root: &Path) -> Result<(), DbError> {
        let path = asset_db_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = postcard::to_allocvec(self)?;
        // A per-process, per-call temp name: two cooks of one project (or two saves in one
        // process) must not truncate each other's half-written file before the rename.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("bin.{}.{seq}.tmp", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.into());
        }
        Ok(())
    }

    /// Scan `assets/` for sources (skips `.meta` sidecars, macOS AppleDouble `._*` files and the
    /// staging directories of `regia upgrade-project --write`, ADR-8861).
    pub fn discover_sources(assets_root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if !assets_root.is_dir() {
            return out;
        }
        for entry in WalkDir::new(assets_root)
            .into_iter()
            .filter_entry(|e| !crate::stream_cook::is_upgrade_stage_dir(e))
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if is_sidecar_or_scratch_name(name) {
                continue;
            }
            out.push(p.to_path_buf());
        }
        out.sort();
        out
    }

    /// Load or create `.meta` for a source path.
    ///
    /// Creation never replaces a `.meta` another cook published meanwhile: the loser of that race
    /// returns the winner's sidecar, so every caller gets the id that is on disk.
    pub fn load_or_create_meta(source: &Path, importer: &str) -> Result<MetaSidecar, DbError> {
        let meta_path = meta_path_for(source);
        let read = |path: &Path| -> Result<MetaSidecar, DbError> {
            let bytes = std::fs::read(path)?;
            MetaSidecar::from_ron(&bytes).map_err(DbError::Meta)
        };
        if meta_path.is_file() {
            return read(&meta_path);
        }
        let meta = MetaSidecar::new(importer, AssetId::new_random());
        let ron = meta.to_ron_string().map_err(DbError::Meta)?;
        if let Some(parent) = meta_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic and no-replace: a torn `.meta` would lose the asset's id, and a racing creator
        // that overwrote the winner's file would hand out an id that is not the one on disk.
        if crate::cook_ledger::write_new_atomic(&meta_path, ron.as_bytes())? {
            Ok(meta)
        } else {
            read(&meta_path)
        }
    }
}

/// The project's cook lock: an exclusive OS file lock on `<project state dir>/cook.lock`, held by
/// a cook from opening the asset database to its last save, so two cooks of one project (the
/// editor's live cooker, `regia cook`, another process) take turns instead of each saving a
/// whole database, ledger and manifest from a stale snapshot over the other's rows. Released
/// when dropped. Where the lock cannot be taken (no file locking on the volume) the cook goes on
/// without it, as before, with one debug line.
pub(crate) struct CookLock {
    _file: Option<std::fs::File>,
}

impl CookLock {
    /// Waits for and takes the lock of `project_root`.
    pub(crate) fn acquire(project_root: &Path) -> Self {
        let dir = regia_core::brand::project_state_dir(project_root);
        let path = dir.join("cook.lock");
        let opened = std::fs::create_dir_all(&dir).and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
        });
        let file = match opened {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(target: "regia_asset_pipeline", path = %path.display(), %error, "cook lock not opened; cooking unlocked");
                return Self { _file: None };
            }
        };
        let _span = tracing::debug_span!(target: "regia_asset_pipeline", "cook_lock_wait").entered();
        match file.lock() {
            Ok(()) => Self { _file: Some(file) },
            Err(error) => {
                tracing::debug!(target: "regia_asset_pipeline", path = %path.display(), %error, "cook lock not taken; cooking unlocked");
                Self { _file: None }
            }
        }
    }

    /// Whether another handle of this process (or another process) holds the lock of
    /// `project_root` right now, so a test can prove a code path took it.
    #[cfg(test)]
    pub(crate) fn is_held(project_root: &Path) -> bool {
        cook_lock_holder(project_root).is_some()
    }
}

/// The project's cook lock (`<project state dir>/cook.lock`) when a cook holds it right now: a
/// handle of this process (the editor's live cooker, a Content ▸ Import worker, any thread) or
/// another process (`regia cook`). Never waits: the lock is tried and released at once. `None`
/// when it is free, was never created, or the volume has no file locks (cooks then run unlocked).
///
/// A lock is an `flock` on an open file description, and a child process this process is
/// spawning shares every open description until its `exec` closes it. So right after a cook
/// releases the lock, a test that spawns `git` on another thread can keep it locked for a moment
/// with no cook holding it (ADR-8366). On Linux and Android such a lock is told apart and answered
/// `None`: it is listed in `/proc/locks` under this process's id, yet none of this process's own
/// descriptors holds it (`/proc/self/fdinfo`). Anything else stays held: a lock of this process,
/// one of another process id, or a `/proc` that cannot be read.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn cook_lock_holder(project_root: &Path) -> Option<PathBuf> {
    let path = regia_core::brand::project_state_dir(project_root).join("cook.lock");
    let file = std::fs::OpenOptions::new().write(true).open(&path).ok()?;
    // A child's copy can go (its `exec`) between the `try_lock` and the look at `/proc`: then
    // nobody is listed and the lock is tried again. A few rounds, never a wait.
    for _ in 0..4 {
        match file.try_lock() {
            Ok(()) | Err(std::fs::TryLockError::Error(_)) => return None,
            Err(std::fs::TryLockError::WouldBlock) => match lock_holder_kind(&file) {
                HolderKind::Holder => return Some(path),
                HolderKind::SpawningChild => return None,
                HolderKind::Gone => {}
            },
        }
    }
    Some(path)
}

/// The project's cook lock (`<project state dir>/cook.lock`) when a cook holds it right now: a
/// handle of this process or another process. Never waits: the lock is tried and released at
/// once. `None` when it is free, was never created, or the volume has no file locks.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[must_use]
pub fn cook_lock_holder(project_root: &Path) -> Option<PathBuf> {
    let path = regia_core::brand::project_state_dir(project_root).join("cook.lock");
    let file = std::fs::OpenOptions::new().write(true).open(&path).ok()?;
    match file.try_lock() {
        Ok(()) | Err(std::fs::TryLockError::Error(_)) => None,
        Err(std::fs::TryLockError::WouldBlock) => Some(path),
    }
}

/// Who holds a lock that `try_lock` found taken (see [`cook_lock_holder`]).
#[cfg(any(target_os = "linux", target_os = "android"))]
enum HolderKind {
    /// A handle of this process or another process: a cook holds it.
    Holder,
    /// Only a child of this process that has not reached its `exec`.
    SpawningChild,
    /// Nobody is listed any more: the lock went between the two looks.
    Gone,
}

/// Who holds the lock on `file`'s inode. [`HolderKind::Holder`] where it cannot be told.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn lock_holder_kind(file: &std::fs::File) -> HolderKind {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(inode) = file.metadata().map(|m| m.ino()) else {
        return HolderKind::Holder;
    };
    let Ok(all) = std::fs::read_to_string("/proc/locks") else {
        return HolderKind::Holder;
    };
    let pid = i64::from(std::process::id());
    let holders = lock_holders(&all, inode);
    if holders.is_empty() {
        HolderKind::Gone
    } else if holders.iter().any(|&holder| holder != pid) || held_by_own_descriptor(inode) {
        HolderKind::Holder
    } else {
        HolderKind::SpawningChild
    }
}

/// The process ids of the granted locks on inode `inode` in `/proc/locks` text (or the `lock:`
/// lines of an fdinfo file, without their `lock:` prefix). A waiting request (`->`) holds nothing.
/// The device is not compared (some file systems report another device there than `stat` does), so
/// another file system's lock on the same inode number only makes the answer more cautious.
/// An open-file-description lock (`OFDLCK`) lists process id -1: never this process.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn lock_holders(text: &str, inode: u64) -> Vec<i64> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let kind = fields.next()?;
            if kind == "->" {
                return None;
            }
            let mut fields = fields.skip(2);
            let pid = fields.next()?.parse::<i64>().ok()?;
            let at = fields.next()?.rsplit(':').next()?.parse::<u64>().ok()?;
            (at == inode).then_some(pid)
        })
        .collect()
}

/// Whether one of this process's open descriptors holds a lock on inode `inode`
/// (`/proc/self/fdinfo/<fd>` `lock:` lines). `true` when the descriptors cannot be listed.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn held_by_own_descriptor(inode: u64) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc/self/fdinfo") else {
        return true;
    };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path()).is_ok_and(|info| {
            let locks: String = info
                .lines()
                .filter_map(|line| line.strip_prefix("lock:"))
                .map(|line| format!("{line}\n"))
                .collect();
            !lock_holders(&locks, inode).is_empty()
        })
    })
}

/// How an orphaned `.meta` found its moved source (ADR-2428).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairRule {
    /// The source's bytes hash to the BLAKE3 the asset database recorded for the meta's id.
    ContentHash,
    /// The database has no row for the id; exactly one meta-less source and one orphan share the
    /// file name.
    FileName,
}

/// One `.meta` moved next to its source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MetaRepair {
    /// The orphaned sidecar, project-relative (`assets/old/hero.glb.meta`).
    pub from_meta: String,
    /// The source it now sits next to, project-relative (`assets/chars/hero.glb`).
    pub to_source: String,
    /// The asset id kept.
    pub id: AssetId,
    /// Which rule matched.
    pub rule: RepairRule,
}

/// An orphaned `.meta` left where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OrphanMeta {
    /// The sidecar, project-relative.
    pub meta: String,
    /// Its asset id, when it parses.
    pub id: Option<AssetId>,
    /// `no_match` (no meta-less source matches), `ambiguous` (several do, or several orphans
    /// claim one source) or `unreadable_meta`.
    pub reason: &'static str,
    /// The candidate sources for `ambiguous`.
    pub candidates: Vec<String>,
}

/// What [`AssetDatabase::repair_orphaned_metas`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetaRepairReport {
    /// Whether the repairs were written (false for a dry run).
    pub applied: bool,
    /// The re-paired sidecars.
    pub repaired: Vec<MetaRepair>,
    /// The sidecars still orphaned.
    pub orphans: Vec<OrphanMeta>,
}

impl MetaRepairReport {
    /// One cook warning line per repair (`asset_meta_repaired`) and per orphan (`asset_meta_orphaned`).
    #[must_use]
    pub fn warning_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.repaired {
            out.push(format!(
                "asset_meta_repaired {} -> {} (id {}, {})",
                r.from_meta,
                r.to_source,
                r.id,
                match r.rule {
                    RepairRule::ContentHash => "content hash",
                    RepairRule::FileName => "file name",
                }
            ));
        }
        for o in &self.orphans {
            let id = o.id.map_or_else(|| "unreadable".to_string(), |id| id.to_string());
            let extra = if o.candidates.is_empty() {
                String::new()
            } else {
                format!("; candidates: {}", o.candidates.join(", "))
            };
            out.push(format!(
                "asset_meta_orphaned {} (id {id}): {}{extra}; move it next to its source or delete it",
                o.meta, o.reason
            ));
        }
        out
    }
}

/// An orphaned sidecar: its path, the source path it names, its parsed form and its bytes.
type Orphan = (PathBuf, PathBuf, Option<MetaSidecar>, Vec<u8>);

/// The source an orphan matched and how, or why it matched none (reason, candidates).
type OrphanMatch = Result<(PathBuf, RepairRule), (&'static str, Vec<String>)>;

fn rel_of(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn lower_ext(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

impl AssetDatabase {
    /// Re-pair orphaned `.meta` files with their moved sources (`docs/specs/12-assets.md` § 2,
    /// ADR-2428). An orphan is a sidecar under `assets/` whose source is gone; a candidate is a
    /// source with no `.meta` and the orphan's file extension. Rules, in order:
    ///
    /// 1. **Content hash**: the database row of the meta's id recorded the source's BLAKE3; the one
    ///    candidate with those bytes gets the meta.
    /// 2. **File name**: only when the database has no row for the id (a fresh clone or a deleted
    ///    `.regia/assetdb`): the one candidate with the orphan's file name, when no other orphan
    ///    has that name.
    ///
    /// Several candidates (or several orphans for one source) leave the orphan in place as
    /// `ambiguous`. With `apply`, the sidecar's bytes are written next to the source (atomically)
    /// and the orphan removed, so the id, and every reference to it, survive; the database row moves
    /// to the new path. A source that already has a `.meta` is never touched.
    pub fn repair_orphaned_metas(&mut self, project_root: &Path, apply: bool) -> MetaRepairReport {
        let _span = tracing::info_span!("repair_orphaned_metas").entered();
        let assets_root = project_root.join("assets");
        let mut report = MetaRepairReport {
            applied: apply,
            ..MetaRepairReport::default()
        };
        if !assets_root.is_dir() {
            return report;
        }
        // Orphaned sidecars and meta-less sources.
        let mut orphans: Vec<Orphan> = Vec::new();
        for entry in WalkDir::new(&assets_root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy();
            if !entry.file_type().is_file()
                || name.starts_with("._")
                || !crate::cook::name_ends_with(&name, ".meta")
            {
                continue;
            }
            let source = p.with_file_name(&name[..name.len() - ".meta".len()]);
            if source.exists() {
                continue;
            }
            let bytes = std::fs::read(p).unwrap_or_default();
            let meta = MetaSidecar::from_ron(&bytes).ok();
            orphans.push((p.to_path_buf(), source, meta, bytes));
        }
        if orphans.is_empty() {
            return report;
        }
        let candidates: Vec<PathBuf> = Self::discover_sources(&assets_root)
            .into_iter()
            .filter(|s| !meta_path_for(s).exists())
            .collect();
        let mut hashes: BTreeMap<PathBuf, Option<[u8; 32]>> = BTreeMap::new();
        let mut hash_of = |path: &Path| -> Option<[u8; 32]> {
            *hashes
                .entry(path.to_path_buf())
                .or_insert_with(|| std::fs::read(path).ok().map(|b| *blake3::hash(&b).as_bytes()))
        };
        // Orphan index -> matched candidate, or the reason it has none.
        let mut matched: Vec<OrphanMatch> = Vec::new();
        for (meta_path, old_source, meta, _) in &orphans {
            let Some(meta) = meta else {
                matched.push(Err(("unreadable_meta", Vec::new())));
                continue;
            };
            let ext = lower_ext(old_source);
            let same_ext: Vec<&PathBuf> = candidates.iter().filter(|c| lower_ext(c) == ext).collect();
            let recorded: Vec<[u8; 32]> = self
                .entries
                .values()
                .filter(|e| e.asset_id == meta.id)
                .map(|e| e.source_hash)
                .collect();
            let found: Vec<PathBuf> = if recorded.is_empty() {
                let name = old_source.file_name();
                let twins = orphans
                    .iter()
                    .filter(|(other, src, ..)| other != meta_path && src.file_name() == name)
                    .count();
                if twins > 0 {
                    matched.push(Err(("ambiguous", Vec::new())));
                    continue;
                }
                same_ext
                    .iter()
                    .filter(|c| c.file_name() == name)
                    .map(|c| (*c).clone())
                    .collect()
            } else {
                same_ext
                    .iter()
                    .filter(|c| hash_of(c).is_some_and(|h| recorded.contains(&h)))
                    .map(|c| (*c).clone())
                    .collect()
            };
            let rule = if recorded.is_empty() {
                RepairRule::FileName
            } else {
                RepairRule::ContentHash
            };
            match found.as_slice() {
                [] => matched.push(Err(("no_match", Vec::new()))),
                [one] => matched.push(Ok((one.clone(), rule))),
                many => matched.push(Err((
                    "ambiguous",
                    many.iter().map(|c| rel_of(project_root, c)).collect(),
                ))),
            }
        }
        // Two orphans claiming one source: neither wins.
        let mut claims: BTreeMap<PathBuf, usize> = BTreeMap::new();
        for (target, _) in matched.iter().flatten() {
            *claims.entry(target.clone()).or_default() += 1;
        }
        for ((meta_path, _, meta, bytes), outcome) in orphans.iter().zip(matched) {
            let id = meta.as_ref().map(|m| m.id);
            let meta_rel = rel_of(project_root, meta_path);
            let (target, rule) = match outcome {
                Ok((target, _)) if claims.get(&target).copied().unwrap_or(0) > 1 => {
                    report.orphans.push(OrphanMeta {
                        meta: meta_rel,
                        id,
                        reason: "ambiguous",
                        candidates: vec![rel_of(project_root, &target)],
                    });
                    continue;
                }
                Ok(found) => found,
                Err((reason, candidates)) => {
                    report.orphans.push(OrphanMeta {
                        meta: meta_rel,
                        id,
                        reason,
                        candidates,
                    });
                    continue;
                }
            };
            let Some(id) = id else { continue };
            if apply {
                let written = crate::cook_ledger::write_atomic(&meta_path_for(&target), bytes)
                    .and_then(|()| std::fs::remove_file(meta_path));
                if let Err(e) = written {
                    tracing::warn!(target: "regia_asset_pipeline", meta = %meta_rel, "cannot re-pair: {e}");
                    report.orphans.push(OrphanMeta {
                        meta: meta_rel,
                        id: Some(id),
                        reason: "io",
                        candidates: vec![rel_of(project_root, &target)],
                    });
                    continue;
                }
                // The id's row (keyed by the path it was cooked from) moves to the new path.
                let new_key = rel_of(project_root, &target);
                let old_keys: Vec<String> = self
                    .entries
                    .iter()
                    .filter(|(_, e)| e.asset_id == id)
                    .map(|(k, _)| k.clone())
                    .collect();
                for old_key in old_keys {
                    if let Some(mut row) = self.entries.remove(&old_key) {
                        row.source_path = new_key.clone();
                        self.entries.insert(new_key.clone(), row);
                    }
                }
            }
            report.repaired.push(MetaRepair {
                from_meta: meta_rel,
                to_source: rel_of(project_root, &target),
                id,
                rule,
            });
        }
        report
    }
}

/// Database errors.
#[derive(Debug, Error)]
pub enum DbError {
    /// Postcard codec.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// Meta parse.
    #[error("meta error: {0}")]
    Meta(regia_asset::MetaError),

    /// IO.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// True for a file that is never a source asset: a `.meta` sidecar (any case) or a macOS
/// AppleDouble resource-fork companion (`._hero.glb`, left by Finder copies onto exFAT/SMB).
pub(crate) fn is_sidecar_or_scratch_name(name: &str) -> bool {
    name.starts_with("._") || crate::cook::name_ends_with(name, ".meta")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8366: `/proc/locks` and fdinfo `lock:` lines: granted locks by inode, waiters skipped.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn lock_holders_reads_granted_locks_by_inode() {
        let text = "1: FLOCK  ADVISORY  WRITE 4242 fd:01:131074 0 EOF\n\
                    1: -> FLOCK  ADVISORY  WRITE 4243 fd:01:131074 0 EOF\n\
                    2: POSIX  ADVISORY  WRITE 77 00:14:999 0 EOF\n\
                    3: OFDLCK ADVISORY  READ  -1 fd:01:131074 0 EOF\n";
        assert_eq!(lock_holders(text, 131_074), vec![4242, -1]);
        assert_eq!(lock_holders(text, 999), vec![77]);
        assert!(lock_holders(text, 5).is_empty());
        assert!(lock_holders("garbage\n\n1:\n", 5).is_empty());
    }

    /// ADR-8366: a cook lock held by a handle of this process is held, whichever thread or
    /// handle has it (a cook, or a test standing in for another holder); released, it is free.
    #[test]
    fn a_cook_lock_this_process_holds_is_held_until_released() {
        let root = regia_test_utils::unique_temp_dir("db-cook-lock-held");
        std::fs::create_dir_all(&root).expect("root");
        assert_eq!(cook_lock_holder(&root), None, "never created");
        let lock = CookLock::acquire(&root);
        let path = regia_core::brand::project_state_dir(&root).join("cook.lock");
        assert_eq!(cook_lock_holder(&root), Some(path.clone()), "a cook holds it");
        drop(lock);
        assert_eq!(cook_lock_holder(&root), None, "released");
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("lock file");
        other.lock().expect("a plain handle takes it");
        assert_eq!(
            cook_lock_holder(&root),
            Some(path),
            "a plain handle of this process holds it"
        );
        drop(other);
        assert_eq!(cook_lock_holder(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-8366: a cook that has released the lock while a child process still shares the lock's
    /// file description (as a child does between its spawn and its exec) leaves the lock free for
    /// `cook_lock_holder`, though a raw `try_lock` still fails. The child here keeps the
    /// description as its stdin, so the state lasts until it is killed: no timing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_released_cook_lock_a_child_still_shares_is_free() {
        let root = regia_test_utils::unique_temp_dir("db-cook-lock-child");
        std::fs::create_dir_all(&root).expect("root");
        let path = regia_core::brand::project_state_dir(&root).join("cook.lock");
        let lock = CookLock::acquire(&root);
        let shared = lock
            ._file
            .as_ref()
            .expect("locked")
            .try_clone()
            .expect("same description");
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .stdin(shared)
            .spawn()
            .expect("spawn sleep");
        drop(lock);
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("lock file");
        let raw = probe.try_lock();
        let answer = cook_lock_holder(&root);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            matches!(raw, Err(std::fs::TryLockError::WouldBlock)),
            "the child's copy keeps the flock: {raw:?}"
        );
        assert_eq!(answer, None, "no cook holds it");
        assert_eq!(cook_lock_holder(&root), None, "the child is gone");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_skips_sidecars_and_appledouble_files() {
        let root = regia_test_utils::unique_temp_dir("db-discover");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("dir");
        for name in [
            "hero.glb",
            "hero.glb.meta",
            "hero.glb.META",
            "._hero.glb",
            "Rock.PNG",
        ] {
            std::fs::write(assets.join(name), b"x").expect("write");
        }
        let found: Vec<String> = AssetDatabase::discover_sources(&assets)
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();
        assert_eq!(found, vec!["Rock.PNG".to_string(), "hero.glb".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn row(path: &str, id: AssetId, bytes: &[u8]) -> DbEntry {
        DbEntry {
            source_path: path.into(),
            asset_id: id,
            source_hash: *blake3::hash(bytes).as_bytes(),
            settings_hash: [0; 32],
            importer: "png".into(),
            importer_version: 1,
            cache_key: [0; 32],
            cooked_rel: String::new(),
        }
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, bytes).expect("write");
    }

    fn put_meta(root: &Path, source_rel: &str, id: AssetId) {
        let meta = MetaSidecar::new("png", id);
        put(
            root,
            &format!("{source_rel}.meta"),
            meta.to_ron_string().expect("ron").as_bytes(),
        );
    }

    fn meta_id(root: &Path, source_rel: &str) -> Option<AssetId> {
        let bytes = std::fs::read(root.join(format!("{source_rel}.meta"))).ok()?;
        MetaSidecar::from_ron(&bytes).ok().map(|m| m.id)
    }

    #[test]
    fn a_source_and_its_meta_moved_separately_re_pair_by_content_hash() {
        let root = regia_test_utils::unique_temp_dir("db-repair-hash");
        let id = AssetId::new_random();
        let rock = b"rock pixels";
        // Cooked once at assets/a/rock.png; then the source moved (and was renamed) to
        // assets/b/stone.png while the sidecar went to assets/c/.
        put(&root, "assets/b/stone.png", rock);
        put_meta(&root, "assets/c/rock.png", id);
        // A different file with the same extension and no meta is not taken.
        put(&root, "assets/b/other.png", b"other pixels");
        let mut db = AssetDatabase::default();
        db.entries
            .insert("assets/a/rock.png".into(), row("assets/a/rock.png", id, rock));
        let dry = db.clone().repair_orphaned_metas(&root, false);
        assert!(!dry.applied);
        assert_eq!(dry.repaired.len(), 1);
        assert!(
            root.join("assets/c/rock.png.meta").is_file(),
            "a dry run writes nothing"
        );
        let report = db.repair_orphaned_metas(&root, true);
        assert_eq!(
            report.repaired,
            vec![MetaRepair {
                from_meta: "assets/c/rock.png.meta".into(),
                to_source: "assets/b/stone.png".into(),
                id,
                rule: RepairRule::ContentHash,
            }]
        );
        assert!(report.orphans.is_empty(), "{:?}", report.orphans);
        assert_eq!(meta_id(&root, "assets/b/stone.png"), Some(id), "the GUID is kept");
        assert!(!root.join("assets/c/rock.png.meta").exists());
        assert!(meta_id(&root, "assets/b/other.png").is_none());
        assert_eq!(db.entries.get("assets/b/stone.png").map(|e| e.asset_id), Some(id));
        assert!(!db.entries.contains_key("assets/a/rock.png"));
        let lines = report.warning_lines();
        assert!(
            lines[0].starts_with("asset_meta_repaired assets/c/rock.png.meta -> assets/b/stone.png"),
            "{lines:?}"
        );
        // A second pass has nothing left to do.
        assert_eq!(
            db.repair_orphaned_metas(&root, true),
            MetaRepairReport {
                applied: true,
                ..Default::default()
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ambiguous_unmatched_and_unreadable_orphans_stay_put() {
        let root = regia_test_utils::unique_temp_dir("db-repair-ambiguous");
        let twin = b"same bytes";
        let id = AssetId::new_random();
        put(&root, "assets/x/one.png", twin);
        put(&root, "assets/y/two.png", twin);
        put_meta(&root, "assets/old.png", id);
        let lost = AssetId::new_random();
        put_meta(&root, "assets/lost.png", lost);
        put(&root, "assets/broken.png.meta", b"not ron");
        // A source with a meta is never a candidate, even with the right bytes.
        let kept = AssetId::new_random();
        put(&root, "assets/has_meta.png", twin);
        put_meta(&root, "assets/has_meta.png", kept);
        let mut db = AssetDatabase::default();
        db.entries
            .insert("assets/old.png".into(), row("assets/old.png", id, twin));
        db.entries
            .insert("assets/lost.png".into(), row("assets/lost.png", lost, b"gone"));
        let report = db.repair_orphaned_metas(&root, true);
        assert!(report.repaired.is_empty(), "{:?}", report.repaired);
        let reasons: Vec<(&str, &str)> = report
            .orphans
            .iter()
            .map(|o| (o.meta.as_str(), o.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("assets/broken.png.meta", "unreadable_meta"),
                ("assets/lost.png.meta", "no_match"),
                ("assets/old.png.meta", "ambiguous"),
            ]
        );
        let ambiguous = &report.orphans[2];
        assert_eq!(ambiguous.candidates, vec!["assets/x/one.png", "assets/y/two.png"]);
        assert!(root.join("assets/old.png.meta").is_file());
        assert_eq!(meta_id(&root, "assets/has_meta.png"), Some(kept));
        assert!(
            report
                .warning_lines()
                .iter()
                .all(|l| l.starts_with("asset_meta_orphaned"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_database_row_a_unique_file_name_re_pairs() {
        let root = regia_test_utils::unique_temp_dir("db-repair-name");
        let id = AssetId::new_random();
        put(&root, "assets/moved/hero.png", b"edited after the move");
        put_meta(&root, "assets/hero.png", id);
        let mut db = AssetDatabase::default();
        let report = db.repair_orphaned_metas(&root, true);
        assert_eq!(report.repaired.len(), 1);
        assert_eq!(report.repaired[0].rule, RepairRule::FileName);
        assert_eq!(meta_id(&root, "assets/moved/hero.png"), Some(id));
        // Two orphans with one name: neither is guessed.
        put(&root, "assets/z/tree.png", b"t");
        put_meta(&root, "assets/p/tree.png", AssetId::new_random());
        put_meta(&root, "assets/q/tree.png", AssetId::new_random());
        let report = db.repair_orphaned_metas(&root, true);
        assert!(report.repaired.is_empty());
        assert_eq!(report.orphans.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_saves_leave_no_temp_file() {
        let root = regia_test_utils::unique_temp_dir("db-save");
        let db = AssetDatabase::default();
        db.save(&root).expect("first save");
        db.save(&root).expect("second save");
        let dir = asset_db_path(&root).parent().expect("parent").to_path_buf();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit concurrency-1103 pipeline H1: concurrent first cooks of one new source must agree on
    /// one asset id, and it must be the id left in the `.meta` on disk (a racing creator used to
    /// mint its own id and rename it over the winner's file while still returning its own).
    #[test]
    fn concurrent_meta_creation_returns_the_id_on_disk() {
        const THREADS: usize = 8;
        for round in 0..16 {
            let root = regia_test_utils::unique_temp_dir("db-meta-race");
            let source = root.join("assets").join(format!("hero{round}.glb"));
            std::fs::create_dir_all(source.parent().expect("parent")).expect("dir");
            std::fs::write(&source, b"x").expect("source");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let barrier = barrier.clone();
                    let source = source.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        AssetDatabase::load_or_create_meta(&source, "gltf").map(|m| m.id)
                    })
                })
                .collect();
            let ids: Vec<AssetId> = handles
                .into_iter()
                .map(|h| h.join().expect("thread").expect("meta"))
                .collect();
            let on_disk = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("meta file"))
                .expect("parse")
                .id;
            assert!(
                ids.iter().all(|id| *id == on_disk),
                "round {round}: returned ids {ids:?}, on disk {on_disk:?}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
