// SPDX-License-Identifier: MPL-2.0
//! Named load sets: residency and progress of everything a menu, lobby or map needs
//! (`docs/specs/12-assets.md` § 5b).
//!
//! A [`LoadSet`] lists the items a screen needs before it can show without a loading screen:
//! textures, meshes, materials, terrain tiles, audio banks and the pipelines that draw them. Each
//! subsystem marks its items resident in the shared [`LoadSets`] registry as they arrive (the
//! asset server for cooked assets, the terrain tile cache for tiles, the pipeline warm queue for
//! pipelines, streaming workers from their own threads); a game asks [`LoadSets::is_resident`]
//! ("can Ready switch to this map now?") and [`LoadSets::load_progress`] (a progress bar, the
//! agent method `assets.load_progress`).
//!
//! An item is keyed by its kind and a string: an asset path for assets, the tile key for terrain
//! tiles, the warm job label for pipelines. Items carry their estimated RAM and VRAM cost, which
//! the memory budget's admission check sums (`regia_render::memory_budget`).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use regia_app::prelude::*;

/// What a load-set item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LoadKind {
    /// A texture (GPU-resident when marked by the renderer, CPU-decoded when tracked from the
    /// asset server).
    Texture,
    /// A mesh.
    Mesh,
    /// A material (and its cooked graph).
    Material,
    /// A terrain tile (`regia_terrain` tile cache).
    TerrainTile,
    /// An audio bank.
    AudioBank,
    /// A pipeline warm job (`regia_render::PipelineWarmQueue` label).
    Pipeline,
    /// Anything else the game tracks.
    Other,
}

impl LoadKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 7] = [
        Self::Texture,
        Self::Mesh,
        Self::Material,
        Self::TerrainTile,
        Self::AudioBank,
        Self::Pipeline,
        Self::Other,
    ];

    /// Stable snake-case name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Texture => "texture",
            Self::Mesh => "mesh",
            Self::Material => "material",
            Self::TerrainTile => "terrain_tile",
            Self::AudioBank => "audio_bank",
            Self::Pipeline => "pipeline",
            Self::Other => "other",
        }
    }

    /// The kind named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Kinds the asset server can report (cooked assets addressed by path).
    #[must_use]
    pub fn is_asset(self) -> bool {
        matches!(
            self,
            Self::Texture | Self::Mesh | Self::Material | Self::AudioBank
        )
    }
}

/// One thing a load set needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadItem {
    /// What it is.
    pub kind: LoadKind,
    /// Its key: asset path, tile key or warm job label.
    pub key: String,
    /// Estimated system memory while resident.
    pub ram_bytes: u64,
    /// Estimated GPU memory while resident.
    pub vram_bytes: u64,
}

/// A named set of items (a menu, a lobby, a map).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadSet {
    /// Name (`"map:switchbacks"`).
    pub name: String,
    /// Items.
    pub items: Vec<LoadItem>,
}

impl LoadSet {
    /// An empty set.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            items: Vec::new(),
        }
    }

    /// Adds an item.
    #[must_use]
    pub fn with(mut self, kind: LoadKind, key: impl Into<String>, ram_bytes: u64, vram_bytes: u64) -> Self {
        self.push(kind, key, ram_bytes, vram_bytes);
        self
    }

    /// Adds an item (duplicates of the same kind and key are ignored).
    pub fn push(&mut self, kind: LoadKind, key: impl Into<String>, ram_bytes: u64, vram_bytes: u64) {
        let key = key.into();
        if self.items.iter().any(|i| i.kind == kind && i.key == key) {
            return;
        }
        self.items.push(LoadItem {
            kind,
            key,
            ram_bytes,
            vram_bytes,
        });
    }

    /// Total estimated `(ram, vram)` bytes.
    #[must_use]
    pub fn bytes(&self) -> (u64, u64) {
        self.items.iter().fold((0u64, 0u64), |(r, v), i| {
            (r.saturating_add(i.ram_bytes), v.saturating_add(i.vram_bytes))
        })
    }
}

/// One kind's share of a set's progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindProgress {
    /// Kind name.
    pub kind: &'static str,
    /// Resident items.
    pub resident: u32,
    /// Items in the set.
    pub total: u32,
    /// Bytes (RAM + VRAM estimate) of the resident items.
    pub resident_bytes: u64,
    /// Bytes of all items.
    pub total_bytes: u64,
}

/// Progress of one load set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadProgress {
    /// Set name.
    pub name: String,
    /// Resident items.
    pub resident: u32,
    /// Items in the set.
    pub total: u32,
    /// Fraction resident: by bytes when the set has byte estimates, else by items.
    pub fraction: f32,
    /// Per kind (kinds with no items are omitted).
    pub kinds: Vec<KindProgress>,
    /// Items that failed to load (`kind:key: error`).
    pub failed: Vec<String>,
}

impl LoadProgress {
    /// Whether every item is resident.
    #[must_use]
    pub fn is_resident(&self) -> bool {
        self.resident == self.total
    }
}

/// Largest `f32` below 1.0: partial byte progress never reads as complete.
const ONE_BELOW: f32 = f32::from_bits(0x3f7f_ffff);

thread_local! {
    static PENDING_KEY_CLONES: Cell<u64> = const { Cell::new(0) };
}

fn note_pending_key_clone() {
    PENDING_KEY_CLONES.with(|n| n.set(n.get().saturating_add(1)));
}

/// Times [`LoadSets::pending`] cloned a key on this thread since
/// [`reset_load_set_pending_key_clones`]. A borrowed residency probe does not count. The `String`
/// returned to the caller does.
// Not re-exported. `cfg(test)` keeps the non-test lib build free of a dead_code warning.
#[cfg(test)]
#[must_use]
pub fn load_set_pending_key_clones() -> u64 {
    PENDING_KEY_CLONES.with(Cell::get)
}

/// Zeroes [`load_set_pending_key_clones`] on this thread.
#[cfg(test)]
pub fn reset_load_set_pending_key_clones() {
    PENDING_KEY_CLONES.with(|n| n.set(0));
}

type Callback = Arc<dyn Fn(&LoadProgress) + Send + Sync>;

/// Identifies a callback registered with [`LoadSets::on_progress`], for
/// [`LoadSets::remove_progress_callback`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProgressCallbackId(u64);

fn kind_contains(map: &HashMap<LoadKind, HashSet<String>>, kind: LoadKind, key: &str) -> bool {
    map.get(&kind).is_some_and(|set| set.contains(key))
}

fn kind_insert(map: &mut HashMap<LoadKind, HashSet<String>>, kind: LoadKind, key: &str) {
    map.entry(kind).or_default().insert(key.to_string());
}

fn kind_remove(map: &mut HashMap<LoadKind, HashSet<String>>, kind: LoadKind, key: &str) -> bool {
    let Some(set) = map.get_mut(&kind) else {
        return false;
    };
    let removed = set.remove(key);
    if set.is_empty() {
        map.remove(&kind);
    }
    removed
}

fn failure_get<'a>(
    map: &'a HashMap<LoadKind, HashMap<String, String>>,
    kind: LoadKind,
    key: &str,
) -> Option<&'a str> {
    map.get(&kind).and_then(|slot| slot.get(key)).map(String::as_str)
}

fn failure_remove(map: &mut HashMap<LoadKind, HashMap<String, String>>, kind: LoadKind, key: &str) -> bool {
    let Some(slot) = map.get_mut(&kind) else {
        return false;
    };
    let removed = slot.remove(key).is_some();
    if slot.is_empty() {
        map.remove(&kind);
    }
    removed
}

#[derive(Default)]
struct Registry {
    sets: BTreeMap<String, LoadSet>,
    resident: HashMap<LoadKind, HashSet<String>>,
    failed: HashMap<LoadKind, HashMap<String, String>>,
    /// Items a subsystem evicted on purpose: the asset server's `Loaded` state (CPU decode) must
    /// not mark them resident again until `mark_resident` does.
    evicted: HashMap<LoadKind, HashSet<String>>,
    callbacks: Vec<(u64, Callback)>,
    next_callback: u64,
}

impl Registry {
    fn progress(&self, set: &LoadSet) -> LoadProgress {
        let mut kinds: Vec<KindProgress> = Vec::new();
        let mut failed = Vec::new();
        for item in &set.items {
            let k = match kinds.iter_mut().find(|k| k.kind == item.kind.name()) {
                Some(k) => k,
                None => {
                    kinds.push(KindProgress {
                        kind: item.kind.name(),
                        ..KindProgress::default()
                    });
                    kinds.last_mut().expect("just pushed") // invariant: pushed above
                }
            };
            let bytes = item.ram_bytes.saturating_add(item.vram_bytes);
            k.total += 1;
            k.total_bytes = k.total_bytes.saturating_add(bytes);
            if kind_contains(&self.resident, item.kind, &item.key) {
                k.resident += 1;
                k.resident_bytes = k.resident_bytes.saturating_add(bytes);
            } else if let Some(err) = failure_get(&self.failed, item.kind, &item.key) {
                failed.push(format!("{}:{}: {err}", item.kind.name(), item.key));
            }
        }
        kinds.sort_by_key(|k| LoadKind::from_name(k.kind));
        let resident: u32 = kinds.iter().map(|k| k.resident).sum();
        let total: u32 = kinds.iter().map(|k| k.total).sum();
        let total_bytes = kinds.iter().fold(0u64, |a, k| a.saturating_add(k.total_bytes));
        let resident_bytes = kinds.iter().fold(0u64, |a, k| a.saturating_add(k.resident_bytes));
        let fraction = if total == 0 {
            1.0
        } else if total_bytes > 0 && total_bytes < u64::MAX {
            // A total pinned at `u64::MAX` saturated: the byte ratio means nothing, count items.
            if resident_bytes >= total_bytes && resident == total {
                1.0
            } else {
                // Divide in f64 and keep f32 rounding from reaching 1.0 early.
                ((resident_bytes as f64 / total_bytes as f64) as f32).min(ONE_BELOW)
            }
        } else {
            resident as f32 / total as f32
        };
        LoadProgress {
            name: set.name.clone(),
            resident,
            total,
            fraction,
            kinds,
            failed,
        }
    }
}

/// How a residency change touches the eviction record.
#[derive(Clone, Copy)]
enum Mark {
    /// `mark_resident`: clears the record.
    Resident,
    /// `mark_evicted`: sets the record.
    Evicted,
    /// The asset-server refresh: no change when the item is recorded as evicted.
    ResidentUnlessEvicted,
    /// A failure: the record stays as it is.
    Other,
}

/// The load-set registry. Cloning shares it, so streaming workers can mark residency from their
/// own threads.
#[derive(Resource, Clone, Default)]
pub struct LoadSets {
    inner: Arc<RwLock<Registry>>,
}

impl LoadSets {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Registry> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Registry> {
        self.inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Defines (or replaces) a set.
    pub fn define(&self, set: LoadSet) {
        let mut reg = self.write();
        reg.sets.insert(set.name.clone(), set);
    }

    /// Forgets a set (its items' residency stays known for other sets). Recorded failures of
    /// items that no remaining set names are dropped with it.
    pub fn remove(&self, name: &str) {
        let mut reg = self.write();
        if reg.sets.remove(name).is_some() {
            let Registry { sets, failed, .. } = &mut *reg;
            failed.retain(|kind, map| {
                map.retain(|key, _| {
                    sets.values()
                        .any(|s| s.items.iter().any(|i| i.kind == *kind && i.key == *key))
                });
                !map.is_empty()
            });
        }
    }

    /// The set named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<LoadSet> {
        self.read().sets.get(name).cloned()
    }

    /// Every set name.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.read().sets.keys().cloned().collect()
    }

    /// Calls `callback` with a set's progress whenever an item of that set changes residency.
    /// Runs on the thread that marked the item, after the registry lock is released: a callback
    /// may read or change this registry (`load_progress`, `mark_resident`, `define`, ...).
    /// Callbacks for marks made on different threads may run at the same time and in either
    /// order, so keep them short.
    /// Returns an id for [`LoadSets::remove_progress_callback`]; a screen that registers one per
    /// visit should remove it when it closes.
    pub fn on_progress(
        &self,
        callback: impl Fn(&LoadProgress) + Send + Sync + 'static,
    ) -> ProgressCallbackId {
        let mut reg = self.write();
        let id = reg.next_callback;
        reg.next_callback += 1;
        reg.callbacks.push((id, Arc::new(callback)));
        ProgressCallbackId(id)
    }

    /// Unregisters a callback from [`LoadSets::on_progress`]; `false` when it was already removed.
    ///
    /// Marks call a snapshot of the callbacks taken under the registry lock, outside it: a mark
    /// made on another thread just before this call may still run the callback once after it
    /// returns. A callback that must not outlive its screen should check its own "closed" flag.
    pub fn remove_progress_callback(&self, id: ProgressCallbackId) -> bool {
        let mut reg = self.write();
        let before = reg.callbacks.len();
        reg.callbacks.retain(|(i, _)| *i != id.0);
        reg.callbacks.len() != before
    }

    /// Marks an item resident (idempotent).
    pub fn mark_resident(&self, kind: LoadKind, key: &str) {
        self.set_state(kind, key, Some(true), None, Mark::Resident);
    }

    /// Marks an item no longer resident (evicted).
    pub fn mark_evicted(&self, kind: LoadKind, key: &str) {
        self.set_state(kind, key, Some(false), None, Mark::Evicted);
    }

    /// Records a load failure (the item stays non-resident).
    pub fn mark_failed(&self, kind: LoadKind, key: &str, error: impl Into<String>) {
        self.set_state(kind, key, Some(false), Some(error.into()), Mark::Other);
    }

    /// The eviction record and the residency change under one write lock (audit
    /// concurrency-1103 H2): two acquisitions let a racing mark leave an item neither resident
    /// nor recorded as evicted, and a refresh then resurrected it.
    fn set_state(
        &self,
        kind: LoadKind,
        key: &str,
        resident: Option<bool>,
        error: Option<String>,
        mark: Mark,
    ) {
        // Snapshot under the lock, call without it (the `regia_core::tasks` lock rule): the lock
        // is not reentrant, so a callback that marks, defines or waits on a thread that does would
        // otherwise hang.
        let (progress, callbacks) = {
            let mut reg = self.write();
            match mark {
                Mark::Resident => {
                    kind_remove(&mut reg.evicted, kind, key);
                }
                Mark::Evicted => {
                    kind_insert(&mut reg.evicted, kind, key);
                }
                // Checked under the same lock as the mark: an eviction cannot land in between.
                Mark::ResidentUnlessEvicted if kind_contains(&reg.evicted, kind, key) => return,
                Mark::ResidentUnlessEvicted | Mark::Other => {}
            }
            let was = kind_contains(&reg.resident, kind, key);
            let now = resident.unwrap_or(was);
            if now {
                kind_insert(&mut reg.resident, kind, key);
                failure_remove(&mut reg.failed, kind, key);
            } else {
                kind_remove(&mut reg.resident, kind, key);
            }
            let error_changed = match &error {
                // A new text for an item that already failed changes the progress too (L3).
                Some(e) => {
                    let slot = reg.failed.entry(kind).or_default();
                    match slot.get(key) {
                        Some(old) if old == e => false,
                        _ => {
                            slot.insert(key.to_string(), e.clone());
                            true
                        }
                    }
                }
                // An eviction (non-resident, no new error) clears the stale failure.
                None if !now => failure_remove(&mut reg.failed, kind, key),
                None => false,
            };
            if (was == now && !error_changed) || reg.callbacks.is_empty() {
                return;
            }
            let reg = &*reg;
            let progress: Vec<LoadProgress> = reg
                .sets
                .values()
                .filter(|set| set.items.iter().any(|i| i.kind == kind && i.key == key))
                .map(|set| reg.progress(set))
                .collect();
            if progress.is_empty() {
                return;
            }
            (progress, reg.callbacks.clone())
        };
        for p in &progress {
            for (_, callback) in &callbacks {
                callback(p);
            }
        }
    }

    /// Whether `kind:key` is resident.
    #[must_use]
    pub fn item_resident(&self, kind: LoadKind, key: &str) -> bool {
        kind_contains(&self.read().resident, kind, key)
    }

    /// Whether every item of the set is resident (`false` for an unknown set).
    #[must_use]
    pub fn is_resident(&self, name: &str) -> bool {
        self.load_progress(name).is_some_and(|p| p.is_resident())
    }

    /// The set's progress (`None` for an unknown set).
    #[must_use]
    pub fn load_progress(&self, name: &str) -> Option<LoadProgress> {
        let reg = self.read();
        reg.sets.get(name).map(|set| reg.progress(set))
    }

    /// Keys of `kind` that some set needs and that are not resident yet (what a subsystem
    /// should report on or fetch next), in set order.
    #[must_use]
    pub fn pending(&self, kind: LoadKind) -> Vec<String> {
        let reg = self.read();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let resident = reg.resident.get(&kind);
        for set in reg.sets.values() {
            for item in set.items.iter().filter(|i| i.kind == kind) {
                let known = resident.is_some_and(|set| set.contains(item.key.as_str()));
                if !known && seen.insert(item.key.as_str()) {
                    note_pending_key_clone();
                    out.push(item.key.clone());
                }
            }
        }
        out
    }

    /// Every distinct key of `kind` across all sets, resident or not, in set order.
    #[must_use]
    pub fn keys(&self, kind: LoadKind) -> Vec<String> {
        let reg = self.read();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for set in reg.sets.values() {
            for item in set.items.iter().filter(|i| i.kind == kind) {
                if seen.insert(item.key.clone()) {
                    out.push(item.key.clone());
                }
            }
        }
        out
    }

    /// Updates asset items (textures, meshes, materials, audio banks keyed by asset path) from
    /// the asset server's load states. Items the server does not know stay as they are, so a
    /// subsystem that marks them itself (the renderer for GPU residency) is not overridden.
    pub fn refresh_from_assets(&self, server: &crate::AssetServer) {
        for kind in LoadKind::ALL.into_iter().filter(|k| k.is_asset()) {
            for key in self.pending(kind) {
                if kind_contains(&self.read().evicted, kind, &key) {
                    continue;
                }
                let Some(id) = server.id_for_path(&key) else {
                    continue;
                };
                match server.state(id) {
                    // Re-checks the eviction record under the mark's own lock: an eviction that
                    // landed after the check above wins.
                    crate::LoadState::Loaded => {
                        self.set_state(kind, &key, Some(true), None, Mark::ResidentUnlessEvicted);
                    }
                    crate::LoadState::Failed(err) => self.mark_failed(kind, &key, err),
                    _ => {}
                }
            }
        }
    }
}

/// `PreUpdate`: asset items of every load set follow the asset server.
pub fn refresh_load_sets(sets: Option<Res<LoadSets>>, server: Option<Res<crate::AssetServer>>) {
    let (Some(sets), Some(server)) = (sets, server) else {
        return;
    };
    let _span = tracing::info_span!("refresh_load_sets").entered();
    sets.refresh_from_assets(&server);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn map_set() -> LoadSet {
        LoadSet::new("map:switchbacks")
            .with(LoadKind::Texture, "maps/sb/rock.ktx2", 0, 300)
            .with(LoadKind::Mesh, "maps/sb/pad.mesh", 50, 50)
            .with(LoadKind::TerrainTile, "0:0:3:4", 0, 100)
            .with(LoadKind::AudioBank, "audio/sb.bank", 400, 0)
            .with(LoadKind::Pipeline, "map:switchbacks frame", 0, 100)
    }

    #[test]
    fn residency_and_progress_by_kind_and_bytes() {
        let sets = LoadSets::default();
        sets.define(map_set());
        assert!(!sets.is_resident("map:switchbacks"));
        assert!(!sets.is_resident("unknown"));
        let p = sets.load_progress("map:switchbacks").expect("set");
        assert_eq!((p.resident, p.total), (0, 5));
        assert_eq!(p.kinds.len(), 5);
        assert_eq!(p.kinds[0].kind, "texture");

        sets.mark_resident(LoadKind::Texture, "maps/sb/rock.ktx2");
        sets.mark_resident(LoadKind::AudioBank, "audio/sb.bank");
        let p = sets.load_progress("map:switchbacks").expect("set");
        assert_eq!(p.resident, 2);
        assert!((p.fraction - 700.0 / 1000.0).abs() < 1e-6, "{}", p.fraction);
        assert_eq!(sets.pending(LoadKind::Texture), Vec::<String>::new());
        assert_eq!(sets.pending(LoadKind::Pipeline), ["map:switchbacks frame"]);

        for (kind, key) in [
            (LoadKind::Mesh, "maps/sb/pad.mesh"),
            (LoadKind::TerrainTile, "0:0:3:4"),
            (LoadKind::Pipeline, "map:switchbacks frame"),
        ] {
            sets.mark_resident(kind, key);
        }
        assert!(sets.is_resident("map:switchbacks"));
        sets.mark_evicted(LoadKind::TerrainTile, "0:0:3:4");
        assert!(!sets.is_resident("map:switchbacks"));
        sets.mark_failed(LoadKind::TerrainTile, "0:0:3:4", "bad tile");
        let p = sets.load_progress("map:switchbacks").expect("set");
        assert_eq!(p.failed, ["terrain_tile:0:0:3:4: bad tile"]);
        assert_eq!(map_set().bytes(), (450, 550));
    }

    #[test]
    fn callback_may_mark_the_next_item_without_deadlock() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("chain").with(LoadKind::Texture, "a", 0, 1).with(
            LoadKind::Texture,
            "b",
            0,
            1,
        ));
        let again = sets.clone();
        sets.on_progress(move |_| again.mark_resident(LoadKind::Texture, "b"));
        sets.mark_resident(LoadKind::Texture, "a");
        assert!(sets.is_resident("chain"));
    }

    #[test]
    fn byte_fraction_is_not_one_while_a_byte_is_outstanding() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("big")
                .with(LoadKind::Texture, "a", 33_554_433, 0)
                .with(LoadKind::Texture, "b", 1, 0),
        );
        sets.mark_resident(LoadKind::Texture, "a");
        let p = sets.load_progress("big").expect("set");
        assert!(p.fraction < 1.0, "{}", p.fraction);
        sets.mark_resident(LoadKind::Texture, "b");
        assert_eq!(sets.load_progress("big").expect("set").fraction, 1.0);
    }

    #[test]
    fn zero_byte_item_outstanding_keeps_fraction_below_one() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("mixed")
                .with(LoadKind::Texture, "a", 1_048_576, 0)
                .with(LoadKind::Pipeline, "p", 0, 0),
        );
        sets.mark_resident(LoadKind::Texture, "a");
        let p = sets.load_progress("mixed").expect("set");
        assert_eq!((p.resident, p.total), (1, 2));
        assert!(p.fraction < 1.0, "{}", p.fraction);
        sets.mark_resident(LoadKind::Pipeline, "p");
        assert_eq!(sets.load_progress("mixed").expect("set").fraction, 1.0);
    }

    #[test]
    fn callbacks_fire_on_changes_of_member_items_only() {
        let sets = LoadSets::default();
        sets.define(map_set());
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "ui/logo.ktx2", 0, 10));
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        sets.on_progress(move |p| {
            if p.name == "menu" {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2");
        sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2"); // no change, no call
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sets.is_resident("menu"));
        // Marks from another thread (streaming workers).
        let worker = sets.clone();
        std::thread::spawn(move || worker.mark_resident(LoadKind::Mesh, "maps/sb/pad.mesh"))
            .join()
            .expect("worker");
        assert!(sets.item_resident(LoadKind::Mesh, "maps/sb/pad.mesh"));
    }

    /// Before the fix the callbacks ran under the registry's read guard, so the first
    /// `mark_resident` below (a write lock on the same thread) hung forever.
    #[test]
    fn callbacks_may_call_back_into_the_registry() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "ui/logo.ktx2", 0, 10));
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        // Weak, so the registry does not own a cycle through its own callback.
        let registry = Arc::downgrade(&sets.inner);
        sets.on_progress(move |p| {
            let Some(inner) = registry.upgrade() else {
                return;
            };
            let reentrant = LoadSets { inner };
            assert!(reentrant.load_progress(&p.name).is_some_and(|q| q.is_resident()));
            reentrant.mark_resident(LoadKind::Other, "seen:menu");
            reentrant.define(LoadSet::new("follow-up").with(LoadKind::Mesh, "ui/frame.mesh", 1, 0));
            reentrant.on_progress(|_| {});
            seen.fetch_add(1, Ordering::SeqCst);
        });
        sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sets.item_resident(LoadKind::Other, "seen:menu"));
        assert_eq!(sets.names(), ["follow-up", "menu"]);
        // The callback added inside a callback runs from the next change on.
        sets.mark_resident(LoadKind::Mesh, "ui/frame.mesh");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(sets.is_resident("follow-up"));
    }

    /// A callback blocked on a worker thread must not stop another thread from marking items.
    /// Before the fix the worker held the read guard while blocked, so the main thread's
    /// `mark_resident` (a write lock) hung and the worker was never released.
    #[test]
    fn a_blocked_callback_does_not_block_marks_from_other_threads() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("menu")
                .with(LoadKind::Texture, "ui/logo.ktx2", 0, 10)
                .with(LoadKind::Mesh, "ui/frame.mesh", 10, 0),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        sets.on_progress(move |p| {
            // Only the worker's mark (the first item) blocks.
            if p.resident == 1 {
                entered_tx.send(()).expect("main waits for the callback");
                release_rx
                    .lock()
                    .expect("only the worker locks it")
                    .recv()
                    .expect("main releases the callback");
            }
        });
        let worker_sets = sets.clone();
        let worker = std::thread::spawn(move || worker_sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2"));
        entered_rx.recv().expect("worker enters its callback");
        sets.mark_resident(LoadKind::Mesh, "ui/frame.mesh");
        assert!(sets.is_resident("menu"));
        release_tx.send(()).expect("worker waits for the release");
        worker.join().expect("worker");
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in LoadKind::ALL {
            assert_eq!(LoadKind::from_name(kind.name()), Some(kind));
        }
        assert!(LoadKind::Texture.is_asset() && !LoadKind::Pipeline.is_asset());
        let empty = LoadSets::default();
        empty.define(LoadSet::new("empty"));
        assert!(empty.is_resident("empty"));
    }

    #[test]
    fn eviction_and_removal_drop_stale_failures() {
        let sets = LoadSets::default();
        sets.define(map_set());
        sets.mark_failed(LoadKind::TerrainTile, "0:0:3:4", "bad tile");
        assert_eq!(
            sets.load_progress("map:switchbacks").expect("set").failed.len(),
            1
        );
        // Evicting the item clears its stale failure.
        sets.mark_evicted(LoadKind::TerrainTile, "0:0:3:4");
        assert!(
            sets.load_progress("map:switchbacks")
                .expect("set")
                .failed
                .is_empty()
        );
        // Removing the only set naming a failed item forgets the failure.
        sets.mark_failed(LoadKind::TerrainTile, "0:0:3:4", "bad tile");
        sets.remove("map:switchbacks");
        sets.define(map_set());
        assert!(
            sets.load_progress("map:switchbacks")
                .expect("set")
                .failed
                .is_empty()
        );
        // A failure of an item another set still names survives the removal.
        sets.define(LoadSet::new("other").with(LoadKind::TerrainTile, "0:0:3:4", 0, 1));
        sets.mark_failed(LoadKind::TerrainTile, "0:0:3:4", "bad tile");
        sets.remove("other");
        assert_eq!(
            sets.load_progress("map:switchbacks").expect("set").failed.len(),
            1
        );
    }

    #[test]
    fn a_removed_progress_callback_stops_firing() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "ui/logo.ktx2", 0, 10));
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let id = sets.on_progress(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(sets.remove_progress_callback(id));
        assert!(!sets.remove_progress_callback(id));
        sets.mark_evicted(LoadKind::Texture, "ui/logo.ktx2");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Audit concurrency-1103 asset H2: a resident mark racing an eviction must leave a
    /// consistent item, resident and not evicted, or evicted and not resident. Before the fix
    /// the eviction record and the residency were updated under two separate lock acquisitions,
    /// so `evict{insert evicted} -> resident{remove evicted, set resident} -> evict{clear}` left
    /// the item neither resident nor marked evicted (the next refresh then resurrected it).
    #[test]
    fn racing_resident_and_evicted_marks_stay_consistent() {
        const ROUNDS: usize = 20_000;
        let sets = LoadSets::default();
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "ui/logo.ktx2", 0, 10));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other = sets.clone();
        let gate = barrier.clone();
        let evictor = std::thread::spawn(move || {
            for _ in 0..ROUNDS {
                gate.wait();
                other.mark_evicted(LoadKind::Texture, "ui/logo.ktx2");
                gate.wait();
            }
        });
        let mut broken = 0usize;
        for _ in 0..ROUNDS {
            barrier.wait();
            sets.mark_resident(LoadKind::Texture, "ui/logo.ktx2");
            barrier.wait();
            let reg = sets.read();
            let resident = kind_contains(&reg.resident, LoadKind::Texture, "ui/logo.ktx2");
            let evicted = kind_contains(&reg.evicted, LoadKind::Texture, "ui/logo.ktx2");
            if resident == evicted {
                broken += 1;
            }
        }
        evictor.join().expect("evictor");
        assert_eq!(
            broken, 0,
            "{broken} of {ROUNDS} rounds ended neither resident nor evicted (or both)"
        );
    }

    /// Audit concurrency-1103 asset L3: a second failure of the same item with a different error
    /// text changes `LoadProgress::failed`, so the callbacks hear about it.
    #[test]
    fn a_new_error_text_for_a_failed_item_notifies_callbacks() {
        let sets = LoadSets::default();
        sets.define(LoadSet::new("menu").with(LoadKind::Texture, "ui/logo.ktx2", 0, 10));
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        sets.on_progress(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        sets.mark_failed(LoadKind::Texture, "ui/logo.ktx2", "not found");
        sets.mark_failed(LoadKind::Texture, "ui/logo.ktx2", "not found"); // same text: no call
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        sets.mark_failed(LoadKind::Texture, "ui/logo.ktx2", "decode error");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a changed error text must notify"
        );
        let failed = sets.load_progress("menu").expect("set").failed;
        assert_eq!(failed.len(), 1);
        assert!(failed[0].contains("decode error"), "{failed:?}");
    }

    /// Unknown asset-kind keys stay pending, so a second refresh must not walk the manifest again.
    /// A path that already uses `/` must not allocate a normalised copy.
    #[test]
    fn asset1_perf_unknown_load_set_keys_are_not_rescanned() {
        use crate::{AssetManifest, AssetServer, AssetServerSettings, ManifestEntry, cooked_cache_dir};
        // Its compare counts need its manifest's index to survive the count (ADR-7386).
        let _index = crate::manifest::index_counting_test();

        let root = regia_test_utils::unique_temp_dir("asset1_perf_load_set");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let manifest = AssetManifest {
            entries: (0..80)
                .map(|i| {
                    let path = format!("assets/tex/{i}.ktx2");
                    ManifestEntry {
                        id: crate::AssetId::sub_asset(crate::AssetId::NIL, &path),
                        path,
                        type_path: "t".into(),
                    }
                })
                .collect(),
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server");
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("level")
                .with(LoadKind::Texture, "missing/a.ktx2", 1, 1)
                .with(LoadKind::Texture, "missing/b.ktx2", 1, 1)
                .with(LoadKind::Mesh, "missing/c.mesh", 1, 1)
                .with(LoadKind::Other, "cell:1", 1, 1),
        );
        crate::reset_manifest_lookup_counts();
        sets.refresh_from_assets(&server);
        let compares = crate::manifest_path_compares();
        assert!(compares >= 80, "the first refresh walked {compares} entries");
        let hits = crate::manifest_negative_hits();
        sets.refresh_from_assets(&server);
        let second = crate::manifest_path_compares();
        let second_hits = crate::manifest_negative_hits();
        crate::reset_asset_path_slash_copies();
        let forward = server.id_for_path("assets/tex/0.ktx2");
        let slash_forward = crate::asset_path_slash_copies();
        let back = server.id_for_path("assets\\tex\\0.ktx2");
        let slash_back = crate::asset_path_slash_copies();
        assert!(
            second == compares
                && second_hits == hits + 3
                && forward.is_some()
                && forward == back
                && slash_forward == 0
                && slash_back == 1,
            "compares {compares}->{second} hits {hits}->{second_hits} slash {slash_forward}->{slash_back} ids {forward:?} {back:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fully resident kind probes residency without cloning its keys.
    #[test]
    fn asset1_perf_resident_pending_does_not_clone_keys() {
        let sets = LoadSets::default();
        sets.define(
            LoadSet::new("menu")
                .with(LoadKind::Texture, "ui/a.ktx2", 1, 1)
                .with(LoadKind::Texture, "ui/b.ktx2", 1, 1),
        );
        sets.mark_resident(LoadKind::Texture, "ui/a.ktx2");
        sets.mark_resident(LoadKind::Texture, "ui/b.ktx2");
        reset_load_set_pending_key_clones();
        assert!(sets.pending(LoadKind::Texture).is_empty());
        assert_eq!(
            load_set_pending_key_clones(),
            0,
            "resident probes cloned {} keys",
            load_set_pending_key_clones()
        );
    }
}
