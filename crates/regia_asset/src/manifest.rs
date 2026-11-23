// SPDX-License-Identifier: MPL-2.0
//! Path ↔ id manifest written into the dev cache (`docs/specs/12-assets.md` § 3).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::AssetId;

thread_local! {
    static MANIFEST_PATH_COMPARES: Cell<u64> = const { Cell::new(0) };
    static MANIFEST_NEGATIVE_HITS: Cell<u64> = const { Cell::new(0) };
}

fn note_manifest_path_compare() {
    MANIFEST_PATH_COMPARES.with(|n| n.set(n.get().saturating_add(1)));
}

/// Counts a remembered miss. The linear lookup never calls this; the path index does.
fn note_manifest_negative_hit() {
    MANIFEST_NEGATIVE_HITS.with(|n| n.set(n.get().saturating_add(1)));
}

/// Path compares [`AssetManifest::id_for_path`] made on this thread since
/// [`reset_manifest_lookup_counts`]. A cached hit or a remembered miss is not a compare.
#[must_use]
pub fn manifest_path_compares() -> u64 {
    MANIFEST_PATH_COMPARES.with(Cell::get)
}

/// Remembered misses of [`AssetManifest::id_for_path`] on this thread since
/// [`reset_manifest_lookup_counts`].
#[must_use]
pub fn manifest_negative_hits() -> u64 {
    MANIFEST_NEGATIVE_HITS.with(Cell::get)
}

/// Zeroes [`manifest_path_compares`] and [`manifest_negative_hits`] on this thread.
pub fn reset_manifest_lookup_counts() {
    MANIFEST_PATH_COMPARES.with(|n| n.set(0));
    MANIFEST_NEGATIVE_HITS.with(|n| n.set(0));
}

/// The process-wide path index is shared by every test thread of this crate's unit tests. A test
/// that fills it to `MAX_INDEXED_MANIFESTS` or past it evicts other tests' indexes, so a test that
/// counts path compares on its own manifest could see a rebuild in the middle of its count (the
/// M5 run: `fixrev2_asset_k_index_cap_evicts_lru_instead_of_clearing` counted one compare while
/// `corr_asset_path_index_of_dropped_manifests_is_bounded` filled the index). Filling tests hold
/// this exclusively ([`index_filling_test`]), counting tests shared ([`index_counting_test`])
/// (ADR-7386).
#[cfg(test)]
static INDEX_TESTS: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Held by a unit test that fills the process-wide path index past its cap (`INDEX_TESTS`).
#[cfg(test)]
pub(crate) fn index_filling_test() -> std::sync::RwLockWriteGuard<'static, ()> {
    INDEX_TESTS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Held by a unit test that counts path compares on its own manifest (`INDEX_TESTS`).
#[cfg(test)]
pub(crate) fn index_counting_test() -> std::sync::RwLockReadGuard<'static, ()> {
    INDEX_TESTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum PathCache {
    Stale,
    Found(AssetId),
    Missing,
}

struct PathIndex {
    len: usize,
    stamp: u64,
    /// [`entries_identity`] when the index was built: checked before a remembered miss is trusted.
    identity: u64,
    by_path: HashMap<String, (usize, AssetId)>,
    missing: HashSet<String>,
    /// Last [`ManifestLookup::tick`] this index answered a lookup on; eviction drops the oldest.
    last_used: u64,
}

struct ManifestLookup {
    /// `entries.as_ptr()` to the index built from that allocation.
    by_entries: HashMap<usize, PathIndex>,
    /// Manifest address to the entries pointer it last indexed, so a reallocation drops the old one.
    owners: HashMap<usize, usize>,
    /// Monotonic clock bumped on every index use; eviction picks the oldest `PathIndex::last_used`.
    tick: u64,
}

fn manifest_lookup() -> &'static Mutex<ManifestLookup> {
    static LOOKUP: std::sync::OnceLock<Mutex<ManifestLookup>> = std::sync::OnceLock::new();
    LOOKUP.get_or_init(|| {
        Mutex::new(ManifestLookup {
            by_entries: HashMap::new(),
            owners: HashMap::new(),
            tick: 0,
        })
    })
}

/// Most `entries` allocations the global path index keeps. A manifest is never told when it is
/// dropped, so the index of every dropped manifest stayed until another manifest took its address;
/// past this many the least-recently-used index is evicted (never the whole process-wide index —
/// a clear rebuilt every other live manifest's index O(N) on its next lookup, and landed between
/// the two lookups of an exact-count test running on another thread).
const MAX_INDEXED_MANIFESTS: usize = 32;

/// A constant-time identity of `entries` beyond its allocation pointer and length: its capacity
/// and the first and last entries' ids and path allocations. A new manifest that reuses a dropped
/// one's address and `entries` allocation (a manifest re-read in a loop, or a vector cleared and
/// refilled) differs here, so a miss remembered for the old manifest is not reported for it.
fn entries_identity(entries: &Vec<ManifestEntry>) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    let mut mix = |v: u64| {
        hash ^= v;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    mix(entries.capacity() as u64);
    for entry in entries.first().into_iter().chain(entries.last()) {
        mix(entry.path.as_ptr() as u64);
        mix(entry.path.len() as u64);
        let id = entry.id.0.as_u128();
        mix(id as u64);
        mix((id >> 64) as u64);
    }
    hash
}

fn manifest_stamp(entries: &[ManifestEntry]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for entry in entries {
        hash ^= entry.path.as_ptr() as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        hash ^= entry.path.len() as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        let id = entry.id.0.as_u128();
        hash ^= id as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        hash ^= (id >> 64) as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Manifest entry for one source path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Project-relative path using `/` separators.
    pub path: String,
    /// Asset id.
    pub id: AssetId,
    /// Reflect path of the primary cooked type.
    pub type_path: String,
}

/// Binary manifest stored as `asset_manifest.bin` (postcard).
///
/// The path index lives beside the value, keyed by this manifest's address and its `entries`
/// allocation. It is not dropped with the manifest: a `Drop` impl would stop callers moving
/// `entries` out. A different manifest does not see that index. [`Self::note_entries_changed`]
/// forgets it, and the next manifest built at this address replaces it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetManifest {
    /// Entries sorted by path for deterministic encoding.
    pub entries: Vec<ManifestEntry>,
}

impl AssetManifest {
    /// Drops the path index for this manifest. Call it before changing `entries` in place —
    /// including an edit of a middle entry only (same length, first and last entries unchanged),
    /// which the index's O(1) identity does not detect (a replaced `Vec` is seen on the next
    /// lookup without this).
    pub fn note_entries_changed(&self) {
        self.forget_path_index();
    }

    fn forget_path_index(&self) {
        let addr = std::ptr::from_ref(self) as usize;
        let ptr = self.entries.as_ptr() as usize;
        let mut lookup = manifest_lookup()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = lookup.owners.remove(&addr) {
            lookup.by_entries.remove(&old);
        }
        lookup.by_entries.remove(&ptr);
    }

    /// Build from a map (paths must be normalised).
    pub fn from_map(map: BTreeMap<String, ManifestEntry>) -> Self {
        let entries = map.into_values().collect();
        Self { entries }
    }

    /// Lookup id by normalised project-relative path.
    ///
    /// The first call walks `entries` and keeps a path index for this allocation. Later calls
    /// reuse it, including a miss. The first entry of a duplicated path wins. [`Self::note_entries_changed`]
    /// drops the index; replacing `entries` does too, because the allocation pointer changes.
    /// An in-place edit of a middle entry — same length, first and last entries unchanged — is
    /// not detected by the index's O(1) identity, so it also needs [`Self::note_entries_changed`];
    /// without it a path remembered as missing keeps answering `None`.
    pub fn id_for_path(&self, path: &str) -> Option<AssetId> {
        for _ in 0..2 {
            match self.cached_path(path) {
                PathCache::Found(id) => return Some(id),
                PathCache::Missing => {
                    note_manifest_negative_hit();
                    return None;
                }
                PathCache::Stale => self.rebuild_path_index(),
            }
        }
        self.entries
            .iter()
            .find(|entry| {
                note_manifest_path_compare();
                entry.path == path
            })
            .map(|entry| entry.id)
    }

    fn cached_path(&self, path: &str) -> PathCache {
        let ptr = self.entries.as_ptr() as usize;
        let len = self.entries.len();
        let addr = std::ptr::from_ref(self) as usize;
        let mut lookup = manifest_lookup()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `found`: `Some(id)` is a cached hit, `Some(None)` a remembered miss, `None` a path
        // the index does not know.
        let found = {
            // The index is this manifest's. A reused `entries` allocation from a dropped
            // manifest is a different address and must walk again.
            let Some(&owned) = lookup.owners.get(&addr) else {
                return PathCache::Stale;
            };
            if owned != ptr {
                return PathCache::Stale;
            }
            let Some(index) = lookup.by_entries.get(&ptr) else {
                return PathCache::Stale;
            };
            if index.len != len || index.identity != entries_identity(&self.entries) {
                return PathCache::Stale;
            }
            if let Some(&(slot, id)) = index.by_path.get(path) {
                if self
                    .entries
                    .get(slot)
                    .is_some_and(|entry| entry.path == path && entry.id == id)
                {
                    Some(Some(id))
                } else {
                    return PathCache::Stale;
                }
            } else if index.missing.contains(path) {
                Some(None)
            } else {
                None
            }
        };
        let tick = {
            lookup.tick += 1;
            lookup.tick
        };
        if let Some(result) = found {
            // A hit or a remembered miss refreshes the eviction order.
            if let Some(index) = lookup.by_entries.get_mut(&ptr) {
                index.last_used = tick;
            }
            return match result {
                Some(id) => PathCache::Found(id),
                None => PathCache::Missing,
            };
        }
        let remember = lookup
            .by_entries
            .get(&ptr)
            .is_some_and(|index| index.stamp == manifest_stamp(&self.entries));
        if !remember {
            return PathCache::Stale;
        }
        if let Some(index) = lookup.by_entries.get_mut(&ptr) {
            index.last_used = tick;
            index.missing.insert(path.to_string());
        }
        PathCache::Missing
    }

    fn rebuild_path_index(&self) {
        let mut by_path = HashMap::with_capacity(self.entries.len());
        for (slot, entry) in self.entries.iter().enumerate() {
            note_manifest_path_compare();
            by_path.entry(entry.path.clone()).or_insert((slot, entry.id));
        }
        let mut index = PathIndex {
            len: self.entries.len(),
            stamp: manifest_stamp(&self.entries),
            identity: entries_identity(&self.entries),
            by_path,
            missing: HashSet::new(),
            last_used: 0,
        };
        let ptr = self.entries.as_ptr() as usize;
        let addr = std::ptr::from_ref(self) as usize;
        let mut lookup = manifest_lookup()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lookup.by_entries.len() >= MAX_INDEXED_MANIFESTS {
            // At the cap: evict the least-recently-used indexes. Clearing the whole
            // process-wide index instead (the old behaviour) dropped every other live
            // manifest's remembered misses, rebuilt them O(N) on their next lookup, and
            // landed between the two exact-count lookups of a test on another thread.
            let mut lru: Vec<(u64, usize)> = lookup
                .by_entries
                .iter()
                .map(|(&entries_ptr, index)| (index.last_used, entries_ptr))
                .collect();
            lru.sort_unstable();
            let excess = lookup.by_entries.len() + 1 - MAX_INDEXED_MANIFESTS;
            for (_, victim) in lru.into_iter().take(excess) {
                lookup.by_entries.remove(&victim);
                lookup.owners.retain(|_, owner| *owner != victim);
            }
        }
        lookup.tick += 1;
        index.last_used = lookup.tick;
        if lookup.owners.len() >= 4 * MAX_INDEXED_MANIFESTS {
            // Owner entries pointing at an evicted index are dead weight.
            let ManifestLookup {
                by_entries, owners, ..
            } = &mut *lookup;
            owners.retain(|_, owner| by_entries.contains_key(owner));
        }
        if let Some(old) = lookup.owners.insert(addr, ptr)
            && old != ptr
        {
            lookup.by_entries.remove(&old);
        }
        lookup.by_entries.insert(ptr, index);
    }

    /// Encode to postcard bytes.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        // Sort (stable: for a duplicated path the earlier entry stays first, as `id_for_path`
        // resolves it) so equal manifests encode to equal bytes whatever the insertion order.
        let mut sorted = self.clone();
        sorted.entries.sort_by(|a, b| a.path.cmp(&b.path));
        postcard::to_allocvec(&sorted)
    }

    /// Decode from postcard bytes. Type paths written before the rename to Regia Engine (a cook
    /// cache or pak from an earlier release) come back spelled with the current crate names, so
    /// code comparing them with the current constants finds them (ADR-1429).
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, postcard::Error> {
        let mut manifest: Self = postcard::from_bytes(bytes)?;
        rebrand_type_paths(&mut manifest.entries);
        Ok(manifest)
    }

    /// Write `asset_manifest.bin` into a cooked cache directory.
    ///
    /// The write goes through a uniquely named sibling file and a rename: a running dev game or
    /// agent reading the manifest while a cook rewrites it sees the old manifest or the new one,
    /// never a torn one, and a cook killed mid-write leaves the old one in place.
    pub fn write_to_cache(&self, cache_dir: &Path) -> Result<(), ManifestError> {
        static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let bytes = self.to_postcard()?;
        let path = cache_dir.join("asset_manifest.bin");
        let n = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = cache_dir.join(format!("asset_manifest.bin.{}-{n}.tmp", std::process::id()));
        if let Err(error) = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        Ok(())
    }

    /// Read manifest from a cache directory if present.
    pub fn read_from_cache(cache_dir: &Path) -> Result<Option<Self>, ManifestError> {
        let path = cache_dir.join("asset_manifest.bin");
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        Ok(Some(Self::from_postcard(&bytes)?))
    }
}

/// Spells every entry's type path with the current crate names: manifests written before the
/// rename to Regia Engine (cook caches, pak tables of contents) name the legacy crates (ADR-1429).
pub(crate) fn rebrand_type_paths(entries: &mut [ManifestEntry]) {
    for entry in entries {
        if let std::borrow::Cow::Owned(current) = regia_core::brand::current_type_path(&entry.type_path) {
            entry.type_path = current;
        }
    }
}

/// Normalise a project-relative asset path to forward slashes without leading `./`.
///
/// An absolute path keeps a single leading `/` (a drive prefix keeps its `C:`); the root
/// component is not joined with a second separator.
pub fn normalise_asset_path(path: &Path) -> String {
    use std::path::Component;
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::RootDir => out.push('/'),
            other => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&other.as_os_str().to_string_lossy());
            }
        }
    }
    out
}

/// Default cooked cache directory for a project and platform tag.
pub fn cooked_cache_dir(project_root: &Path, platform: &str) -> PathBuf {
    regia_core::brand::project_state_dir(project_root)
        .join("cooked")
        .join(platform)
}

/// Asset database path.
pub fn asset_db_path(project_root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project_root)
        .join("assetdb")
        .join("index.bin")
}

/// Errors reading/writing manifests.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Postcard codec error.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            id: AssetId::sub_asset(AssetId::NIL, path),
            type_path: "t".to_string(),
        }
    }

    /// A path the manifest does not contain is walked once. The next lookup is a remembered miss.
    #[test]
    fn asset1_perf_manifest_miss_is_not_scanned_twice() {
        let _index = index_counting_test();
        let manifest = AssetManifest {
            entries: (0..80).map(|i| entry(&format!("assets/tex/{i}.ktx2"))).collect(),
        };
        reset_manifest_lookup_counts();
        assert!(manifest.id_for_path("missing/one").is_none());
        let compares = manifest_path_compares();
        assert!(compares >= 80, "the first miss walked {compares} entries");
        let hits = manifest_negative_hits();
        assert!(manifest.id_for_path("missing/one").is_none());
        let second = manifest_path_compares();
        let second_hits = manifest_negative_hits();
        let present = manifest.id_for_path("assets/tex/3.ktx2");
        let after_hit = manifest_path_compares();
        assert!(
            second == compares && second_hits == hits + 1 && present.is_some() && after_hit == compares,
            "miss compares {compares}->{second}, negative hits {hits}->{second_hits}, hit compares {after_hit}, id {present:?}"
        );
    }

    /// The first of two entries with the same path wins, including after the index is built.
    #[test]
    fn path_index_keeps_the_first_duplicate() {
        let mut second = entry("a");
        second.id = AssetId::sub_asset(AssetId::NIL, "other");
        let manifest = AssetManifest {
            entries: vec![entry("a"), second],
        };
        let first = entry("a").id;
        assert_eq!(manifest.id_for_path("a"), Some(first));
        assert_eq!(manifest.id_for_path("a"), Some(first));
    }

    /// Replacing `entries` and an in-place edit announced with [`AssetManifest::note_entries_changed`]
    /// are visible to the next lookup.
    #[test]
    fn path_index_follows_a_replaced_vec_and_a_noted_edit() {
        let mut manifest = AssetManifest {
            entries: vec![entry("a"), entry("b")],
        };
        assert!(manifest.id_for_path("nope").is_none());
        let id = manifest.entries[0].id;
        manifest.entries[0].path = "nope".into();
        manifest.note_entries_changed();
        assert_eq!(manifest.id_for_path("nope"), Some(id));
        assert_eq!(manifest.id_for_path("a"), None);
        manifest.entries = vec![entry("z")];
        assert_eq!(manifest.id_for_path("z"), Some(entry("z").id));
        assert_eq!(manifest.id_for_path("nope"), None);
    }

    /// corr-asset: a new manifest at the same address that reuses the old `entries` allocation
    /// (same length) is not answered from the old manifest's remembered misses.
    #[test]
    fn corr_asset_reused_entries_allocation_is_a_new_manifest() {
        let mut manifest = AssetManifest {
            entries: vec![entry("a")],
        };
        assert!(
            manifest.id_for_path("b").is_none(),
            "b is not in the first manifest"
        );
        let mut entries = std::mem::take(&mut manifest.entries);
        entries.clear();
        entries.push(entry("b"));
        manifest = AssetManifest { entries };
        assert_eq!(
            manifest.id_for_path("b"),
            Some(entry("b").id),
            "the second manifest lists b"
        );
        assert_eq!(manifest.id_for_path("a"), None);
    }

    /// corr-asset: the global path index does not keep the index of every manifest that ever
    /// looked a path up.
    #[test]
    fn corr_asset_path_index_of_dropped_manifests_is_bounded() {
        let _index = index_filling_test();
        let manifests: Vec<Box<AssetManifest>> = (0..4 * MAX_INDEXED_MANIFESTS)
            .map(|i| {
                Box::new(AssetManifest {
                    entries: vec![entry(&format!("p{i}"))],
                })
            })
            .collect();
        for m in &manifests {
            assert!(m.id_for_path("x").is_none());
        }
        drop(manifests);
        let lookup = manifest_lookup()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            lookup.by_entries.len() <= MAX_INDEXED_MANIFESTS,
            "{} indexes kept",
            lookup.by_entries.len()
        );
        assert!(lookup.owners.len() <= 4 * MAX_INDEXED_MANIFESTS);
    }

    /// fixrev2-asset-k: reaching `MAX_INDEXED_MANIFESTS` evicts the least-recently-used index;
    /// it must not clear the whole process-wide index. The manifest touched last is still
    /// indexed right after the cap is crossed; the old `clear()` wiped every index there and
    /// rebuilt it O(N) on the next lookup, which landed between the two exact-count lookups
    /// of a test on another thread.
    #[test]
    fn fixrev2_asset_k_index_cap_evicts_lru_instead_of_clearing() {
        let _index = index_filling_test();
        let mut manifests: Vec<Box<AssetManifest>> = Vec::new();
        for i in 0..MAX_INDEXED_MANIFESTS {
            manifests.push(Box::new(AssetManifest {
                entries: vec![entry(&format!("p{i}"))],
            }));
            assert!(manifests[i].id_for_path("x").is_none());
        }
        // Make the oldest the most recently used, then cross the cap with one more manifest:
        // under `clear()` every index above is wiped; under LRU only an older one is evicted.
        assert!(manifests[0].id_for_path("x").is_none());
        let extra = Box::new(AssetManifest {
            entries: vec![entry("extra")],
        });
        assert!(extra.id_for_path("x").is_none());
        // The most recently used manifest still has its index: no rebuild, no compares.
        reset_manifest_lookup_counts();
        assert!(manifests[0].id_for_path("x").is_none());
        assert_eq!(
            manifest_path_compares(),
            0,
            "the most recently used index survived the cap"
        );
        drop(extra);
    }

    #[test]
    fn legacy_type_paths_decode_with_the_current_crate_names() {
        let legacy = format!("{}asset::CookedTexture", regia_core::brand::LEGACY_CRATE_PREFIX);
        let current = format!("{}asset::CookedTexture", regia_core::brand::CRATE_PREFIX);
        let written = AssetManifest {
            entries: vec![ManifestEntry {
                type_path: legacy,
                ..entry("a.png")
            }],
        };
        let back = AssetManifest::from_postcard(&written.to_postcard().expect("encode")).expect("decode");
        assert_eq!(back.entries[0].type_path, current);
        assert_eq!(back.entries[0].id, written.entries[0].id);
    }

    #[test]
    fn postcard_is_independent_of_entry_order() {
        let a = AssetManifest {
            entries: vec![entry("b.png"), entry("a.png"), entry("c.png")],
        };
        let b = AssetManifest {
            entries: vec![entry("c.png"), entry("b.png"), entry("a.png")],
        };
        assert_eq!(a.to_postcard().expect("a"), b.to_postcard().expect("b"));
    }

    #[test]
    fn normalise_drops_current_dir_components() {
        assert_eq!(
            normalise_asset_path(Path::new("./assets/./hero.glb")),
            "assets/hero.glb"
        );
    }
}
