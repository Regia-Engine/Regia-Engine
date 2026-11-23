// SPDX-License-Identifier: MPL-2.0
//! Replicated asset handles (spec 71 § 3, ADR-2415): the world's asset-server context
//! ([`NetAssets`]) and the stock resolver ([`NetAssetTable`]).
//!
//! A component field of type `Handle<T>` travels as the asset's stable key: its guid, or its
//! project path when the handle was registered by path. The server's resolver turns the handle
//! into the key; each receiver's resolver turns the key back into a handle of its own asset
//! server, loading through the table's loader when the asset is not loaded yet. A key that does
//! not resolve leaves the handle unset (`Handle::INVALID`), logs `net.asset_missing` once per key
//! with its code, and is listed by `net.assets`.
//!
//! ```
//! use std::sync::Arc;
//! use regia_net::assets::{NetAssetTable, NetAssets};
//!
//! let table = Arc::new(NetAssetTable::new());
//! // After loading: table.insert_id(&handle, asset_id) or table.insert_path(&handle, "assets/a.glb").
//! let assets = NetAssets::new(table.clone());
//! assert!(assets.context().is_some());
//! ```

use std::any::TypeId;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use regia_asset::{AssetId, Handle};
use regia_ecs::prelude::*;

use crate::codec::{ASSET_MISSING_LOG, AssetContext, AssetKey, AssetMiss, AssetResolveError, AssetResolver};
use crate::model::NetTick;

/// Most distinct missing keys [`NetAssets`] remembers; later ones are counted only.
pub const MAX_MISSING_KEYS: usize = 1024;

/// Most keys the loader may add to a [`NetAssetTable`]; past it unknown keys resolve as
/// `asset_unknown` without running the loader.
pub const MAX_LOADED_KEYS: usize = 65_536;

/// Most failed keys a [`NetAssetTable`] remembers at once.
pub const MAX_FAILED_KEYS: usize = 1024;

/// How many resolve calls a remembered loader failure lasts before the loader may run again.
pub const FAILED_KEY_LIFETIME: u64 = 4096;

/// Loads an asset the table does not hold yet: `(asset type, key)` to the new handle's slot.
pub type AssetLoader = dyn Fn(TypeId, &AssetKey) -> Result<(u32, u32), AssetResolveError> + Send + Sync;

/// The keys of one live handle.
struct SlotKeys {
    generation: u32,
    /// Every key registered for this handle; the first is the one it travels as.
    keys: Vec<AssetKey>,
}

/// Where a key resolves to.
#[derive(Clone, Copy)]
struct Binding {
    index: u32,
    generation: u32,
    /// Added by the loader (counted against [`MAX_LOADED_KEYS`]).
    loaded: bool,
}

#[derive(Default)]
struct Tables {
    /// `(type, slot index)` to the handle's generation and keys.
    by_handle: HashMap<(TypeId, u32), SlotKeys>,
    /// `(type, key)` to the slot.
    by_key: HashMap<(TypeId, AssetKey), Binding>,
    /// The type each key was registered with, for `asset_wrong_type`.
    key_types: HashMap<AssetKey, TypeId>,
    /// Keys the loader refused: `(type, key)` to `(resolve call it failed at, error)`.
    failed: HashMap<(TypeId, AssetKey), (u64, AssetResolveError)>,
    /// Keys the loader added that still resolve (bounded by [`MAX_LOADED_KEYS`]).
    loaded: usize,
}

impl Tables {
    /// Binds `key` to the handle `(index, generation)` of type `ty`. A handle bound twice keeps
    /// its first key (the one it travels as) until that key is bound to a different live handle.
    /// The key then resolves to the new handle, and the old handle travels as its next key, or
    /// as none when it has no key left. A reused slot (new generation) takes the new key, and
    /// every key of the dead handle stops resolving (audit net3-correctness #2): a later resolve
    /// of one of them loads it again instead of answering the dead generation.
    fn bind(&mut self, ty: TypeId, index: u32, generation: u32, key: &AssetKey, loaded: bool) {
        let dead = match self.by_handle.get_mut(&(ty, index)) {
            Some(slot) if slot.generation != generation => {
                let old = slot.generation;
                slot.generation = generation;
                Some((old, std::mem::take(&mut slot.keys)))
            }
            Some(_) => None,
            None => {
                self.by_handle.insert(
                    (ty, index),
                    SlotKeys {
                        generation,
                        keys: Vec::new(),
                    },
                );
                None
            }
        };
        if let Some((old, keys)) = dead {
            for k in &keys {
                self.unbind(ty, k, index, old);
            }
        }
        if let Some(slot) = self.by_handle.get_mut(&(ty, index))
            && !slot.keys.contains(key)
        {
            slot.keys.push(key.clone());
        }
        self.key_types.entry(key.clone()).or_insert(ty);
        let before = self.by_key.insert(
            (ty, key.clone()),
            Binding {
                index,
                generation,
                loaded,
            },
        );
        // The same key bound to another live handle used to stay at the front of that handle's
        // list, so `key_of` replicated the old handle as the new asset.
        if let Some(prev) = before
            && (prev.index != index || prev.generation != generation)
            && let Some(slot) = self.by_handle.get_mut(&(ty, prev.index))
            && slot.generation == prev.generation
        {
            slot.keys.retain(|k| k != key);
        }
        let was_loaded = before.is_some_and(|b| b.loaded);
        if loaded && !was_loaded {
            self.loaded += 1;
        } else if !loaded && was_loaded {
            self.loaded = self.loaded.saturating_sub(1);
        }
    }

    /// Removes `key`'s binding when it still points at the handle `(index, generation)`.
    fn unbind(&mut self, ty: TypeId, key: &AssetKey, index: u32, generation: u32) {
        let id = (ty, key.clone());
        if !self
            .by_key
            .get(&id)
            .is_some_and(|b| b.index == index && b.generation == generation)
        {
            return;
        }
        if self.by_key.remove(&id).is_some_and(|b| b.loaded) {
            self.loaded = self.loaded.saturating_sub(1);
        }
        if self.key_types.get(key) == Some(&ty) {
            self.key_types.remove(key);
        }
    }
}

/// The stock [`AssetResolver`]: a two-way table of the handles an asset server handed out, with
/// an optional loader for keys it does not hold yet. Thread-safe; share it with an `Arc`.
#[derive(Default)]
pub struct NetAssetTable {
    tables: RwLock<Tables>,
    loader: Option<Box<AssetLoader>>,
    /// Resolve calls so far (the clock for remembered failures).
    calls: AtomicU64,
}

impl std::fmt::Debug for NetAssetTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetAssetTable")
            .field("handles", &self.len())
            .field("loader", &self.loader.is_some())
            .finish()
    }
}

impl NetAssetTable {
    /// An empty table with no loader.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty table that loads unknown keys with `loader` (for example through the
    /// receiver's asset server) and remembers what it loaded.
    pub fn with_loader(
        loader: impl Fn(TypeId, &AssetKey) -> Result<(u32, u32), AssetResolveError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            tables: RwLock::default(),
            loader: Some(Box::new(loader)),
            calls: AtomicU64::new(0),
        }
    }

    /// Registers a handle under a key. A handle registered twice keeps its first key (the one it
    /// travels as) until that key is registered for a different live handle; it then travels as
    /// its next key. Every key resolves to the handle it was most recently registered for. A
    /// handle of a reused slot (new generation) replaces the slot's old handle: the old handle's
    /// keys stop resolving.
    pub fn insert<T: 'static>(&self, handle: &Handle<T>, key: AssetKey) {
        if handle.index == u32::MAX {
            return;
        }
        let mut t = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        t.bind(TypeId::of::<T>(), handle.index, handle.generation, &key, false);
    }

    /// Registers a handle by its asset guid (what it travels as).
    pub fn insert_id<T: 'static>(&self, handle: &Handle<T>, id: AssetId) {
        self.insert(handle, AssetKey::Guid(id));
    }

    /// Registers a handle by its project path.
    pub fn insert_path<T: 'static>(&self, handle: &Handle<T>, path: &str) {
        self.insert(handle, AssetKey::Path(path.to_string()));
    }

    /// Handles registered.
    pub fn len(&self) -> usize {
        self.tables.read().map_or(0, |t| t.by_handle.len())
    }

    /// Whether no handle is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AssetResolver for NetAssetTable {
    fn key_of(&self, asset_type: TypeId, index: u32, generation: u32) -> Option<AssetKey> {
        let t = self.tables.read().ok()?;
        t.by_handle
            .get(&(asset_type, index))
            .filter(|s| s.generation == generation)
            .and_then(|s| s.keys.first().cloned())
    }

    fn resolve(&self, asset_type: TypeId, key: &AssetKey) -> Result<(u32, u32), AssetResolveError> {
        match self.resolve_loaded(asset_type, key) {
            Err(AssetResolveError::Unknown) => {}
            done => return done,
        }
        let Some(loader) = &self.loader else {
            return Err(AssetResolveError::Unknown);
        };
        let now = self.calls.fetch_add(1, Ordering::Relaxed);
        {
            // A key the loader refused lately is answered from memory, and a full table or a full
            // failure list stops the loader: a server naming endless keys costs bounded work.
            let t = self.tables.read().unwrap_or_else(PoisonError::into_inner);
            if let Some((at, err)) = t.failed.get(&(asset_type, key.clone()))
                && now.saturating_sub(*at) < FAILED_KEY_LIFETIME
            {
                return Err(err.clone());
            }
            if t.loaded >= MAX_LOADED_KEYS {
                return Err(AssetResolveError::Unknown);
            }
            if t.failed.len() >= MAX_FAILED_KEYS
                && t.failed
                    .values()
                    .all(|(at, _)| now.saturating_sub(*at) < FAILED_KEY_LIFETIME)
            {
                return Err(AssetResolveError::Unknown);
            }
        }
        let (index, generation) = match loader(asset_type, key) {
            Ok(slot) => slot,
            Err(e) => {
                let mut t = self.tables.write().unwrap_or_else(PoisonError::into_inner);
                if t.failed.len() >= MAX_FAILED_KEYS {
                    t.failed
                        .retain(|_, (at, _)| now.saturating_sub(*at) < FAILED_KEY_LIFETIME);
                }
                if t.failed.len() < MAX_FAILED_KEYS {
                    t.failed.insert((asset_type, key.clone()), (now, e.clone()));
                }
                return Err(e);
            }
        };
        let mut t = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        t.failed.remove(&(asset_type, key.clone()));
        t.bind(asset_type, index, generation, key, true);
        Ok((index, generation))
    }

    fn resolve_loaded(&self, asset_type: TypeId, key: &AssetKey) -> Result<(u32, u32), AssetResolveError> {
        let t = self.tables.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(b) = t.by_key.get(&(asset_type, key.clone())) {
            return Ok((b.index, b.generation));
        }
        if t.key_types.get(key).is_some_and(|ty| *ty != asset_type) {
            return Err(AssetResolveError::WrongType);
        }
        Err(AssetResolveError::Unknown)
    }
}

/// One key that did not resolve on this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingAsset {
    /// Why (the [`AssetMiss`] code of its first miss).
    pub code: &'static str,
    /// How many handles decoded unset because of it.
    pub count: u64,
    /// The tick of its first miss.
    pub first_tick: NetTick,
}

/// The world's asset-server context for replication (spec 71 § 3). Without it (or with no
/// resolver), handles travel as none and decode unset with `asset_no_resolver`.
#[derive(Resource, Debug, Default)]
pub struct NetAssets {
    context: Option<Arc<AssetContext>>,
    missing: BTreeMap<AssetKey, MissingAsset>,
    /// Misses past [`MAX_MISSING_KEYS`] distinct keys (counted, not listed).
    pub missing_overflow: u64,
}

impl NetAssets {
    /// A context over a resolver.
    pub fn new(resolver: Arc<dyn AssetResolver>) -> Self {
        Self {
            context: Some(Arc::new(AssetContext::new(resolver))),
            missing: BTreeMap::new(),
            missing_overflow: 0,
        }
    }

    /// The codec context, when a resolver is installed.
    pub fn context(&self) -> Option<Arc<AssetContext>> {
        self.context.clone()
    }

    /// Replaces the resolver (`None` removes it). Missing keys are kept.
    pub fn set_resolver(&mut self, resolver: Option<Arc<dyn AssetResolver>>) {
        self.context = resolver.map(|r| Arc::new(AssetContext::new(r)));
    }

    /// Handles sent as none because the resolver had no key for them (`asset_unmapped`).
    pub fn unmapped(&self) -> u64 {
        self.context.as_ref().map_or(0, |c| c.unmapped())
    }

    /// The keys that did not resolve here, in key order.
    pub fn missing(&self) -> impl Iterator<Item = (&AssetKey, &MissingAsset)> {
        self.missing.iter()
    }

    /// The record of one key.
    pub fn missing_key(&self, key: &AssetKey) -> Option<&MissingAsset> {
        self.missing.get(key)
    }

    /// Records misses from a decode, logging `net.asset_missing` once per key.
    pub fn record(&mut self, misses: Vec<AssetMiss>, component: &str, tick: NetTick) {
        for miss in misses {
            if let Some(m) = self.missing.get_mut(&miss.key) {
                m.count += 1;
                continue;
            }
            if self.missing.len() >= MAX_MISSING_KEYS {
                self.missing_overflow += 1;
                continue;
            }
            tracing::warn!(
                target: "net",
                code = ASSET_MISSING_LOG,
                reason = miss.code,
                key = %miss.key,
                component,
                "net: a replicated asset handle did not resolve; it stays unset"
            );
            self.missing.insert(
                miss.key,
                MissingAsset {
                    code: miss.code,
                    count: 1,
                    first_tick: tick,
                },
            );
        }
    }

    /// Forgets the missing keys (after loading content, for example).
    pub fn clear_missing(&mut self) {
        self.missing.clear();
        self.missing_overflow = 0;
    }
}

/// The asset server's handles in a [`NetAssetTable`] (ADR-2665 (3)): every mesh, image, texture
/// and material the world's `regia_asset::AssetServer` has loaded is registered by its guid, so a
/// replicated `Handle<T>` of one of those types travels without the game registering it.
///
/// [`register_asset_server_handles`] keeps it current. When the world has no [`NetAssets`]
/// resolver, it installs one over this table; a resolver the game installed itself is left
/// alone (the game's table wins, and this one is not filled).
#[derive(Resource, Debug)]
pub struct NetAssetServerTable {
    table: Arc<NetAssetTable>,
    /// The context installed over `table`, to tell it from a game's own resolver.
    context: Option<Arc<AssetContext>>,
    /// Slots of each slab already registered (meshes, images, textures, materials).
    seen: [u32; 4],
}

impl Default for NetAssetServerTable {
    fn default() -> Self {
        Self {
            table: Arc::new(NetAssetTable::new()),
            context: None,
            seen: [0; 4],
        }
    }
}

impl NetAssetServerTable {
    /// The table (shared): a game may add handles of its own types to it.
    pub fn table(&self) -> &Arc<NetAssetTable> {
        &self.table
    }

    /// Whether this table is the world's resolver (false when the game installed its own).
    pub fn is_resolver(&self) -> bool {
        self.context.is_some()
    }
}

/// Registers the handles the asset server loaded since the last call into the
/// [`NetAssetServerTable`] (ADR-2665 (3)). Runs once per frame at the start of `net_receive`;
/// a world without an `AssetServer` is untouched. Read only on the asset side: nothing about how
/// assets load changes.
pub fn register_asset_server_handles(world: &mut World) {
    let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
        return;
    };
    let counts = [
        server.meshes().slot_count(),
        server.images().slot_count(),
        server.textures().slot_count(),
        server.materials().slot_count(),
    ];
    if !world.contains_resource::<NetAssetServerTable>() {
        world.insert_resource(NetAssetServerTable::default());
    }
    // Install the table as the resolver when the world has none; stop when a game resolver
    // replaced it.
    let ours = world.resource::<NetAssetServerTable>().context.clone();
    let current = world.get_resource::<NetAssets>().and_then(NetAssets::context);
    let resolver = match (current, ours) {
        (None, _) => {
            let table = world.resource::<NetAssetServerTable>().table.clone();
            let mut assets = world.get_resource_or_insert_with(NetAssets::default);
            assets.set_resolver(Some(table));
            assets.context()
        }
        (Some(cur), Some(ours)) if Arc::ptr_eq(&cur, &ours) => Some(ours),
        (Some(_), _) => None,
    };
    let mut st = world.resource_mut::<NetAssetServerTable>();
    st.context = resolver;
    if st.context.is_none() || st.seen == counts {
        return;
    }
    let _span = tracing::trace_span!("net_register_asset_handles").entered();
    let first = std::mem::replace(&mut st.seen, counts);
    let table = st.table.clone();
    let server = world.resource::<regia_asset::AssetServer>();
    for (id, h) in server.meshes().entries_from(first[0]) {
        table.insert_id(&h, id);
    }
    for (id, h) in server.images().entries_from(first[1]) {
        table.insert_id(&h, id);
    }
    for (id, h) in server.textures().entries_from(first[2]) {
        table.insert_id(&h, id);
    }
    for (id, h) in server.materials().entries_from(first[3]) {
        table.insert_id(&h, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitWriter;
    use crate::codec::{
        ASSET_NO_RESOLVER, ASSET_UNKNOWN, ASSET_WRONG_TYPE, CodecContext, ReplicateCodec, decode_from_slice,
        encode_to_vec,
    };
    use regia_asset::Assets;
    use regia_core::id::Guid;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Mesh(u8);
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Sound(u8);

    fn id(n: u128) -> AssetId {
        AssetId(Guid::from_u128(n))
    }

    #[test]
    fn handles_travel_as_guid_or_path_and_resolve_on_the_receiver() {
        // Sender and receiver have their own slabs: the slots differ, the keys match.
        let mut send_slab = Assets::<Mesh>::new();
        let _pad = send_slab.insert(id(1), Mesh(0));
        let a = send_slab.insert(id(7), Mesh(7));
        let b = send_slab.insert(id(8), Mesh(8));
        let sender = Arc::new(NetAssetTable::new());
        sender.insert_id(&a, id(7));
        sender.insert_path(&b, "assets/b.glb");

        let mut recv_slab = Assets::<Mesh>::new();
        let ra = recv_slab.insert(id(7), Mesh(7));
        let rb = recv_slab.insert(id(8), Mesh(8));
        let receiver = Arc::new(NetAssetTable::new());
        receiver.insert_id(&ra, id(7));
        receiver.insert_path(&rb, "assets/b.glb");

        let scx = AssetContext::new(sender);
        let rcx = AssetContext::new(receiver);
        let enc = CodecContext::detached().with_assets(Some(&scx));
        let dec = CodecContext::detached().with_assets(Some(&rcx));
        let bytes = encode_to_vec(&vec![a, b, Handle::INVALID], &enc);
        let back: Vec<Handle<Mesh>> = decode_from_slice(&bytes, &dec).expect("decode");
        assert_eq!(back, vec![ra, rb, Handle::INVALID]);
        assert_eq!(recv_slab.get(&back[0]), Some(&Mesh(7)));
        assert!(dec.take_asset_misses().is_empty());
        // Wire size: a guid is 2 + 128 bits, a path 2 bits plus its length-prefixed bytes.
        let mut w = BitWriter::new();
        a.encode(&mut w, &enc);
        assert_eq!(w.bit_len(), 130);
        assert_eq!(scx.unmapped(), 0);
    }

    #[test]
    fn an_unknown_asset_decodes_unset_with_its_code() {
        let mut slab = Assets::<Mesh>::new();
        let h = slab.insert(id(9), Mesh(9));
        let sender = Arc::new(NetAssetTable::new());
        sender.insert_id(&h, id(9));
        let scx = AssetContext::new(sender);
        let bytes = encode_to_vec(&h, &CodecContext::detached().with_assets(Some(&scx)));

        let empty = AssetContext::new(Arc::new(NetAssetTable::new()));
        let dec = CodecContext::detached().with_assets(Some(&empty));
        let back: Handle<Mesh> = decode_from_slice(&bytes, &dec).expect("not a decode error");
        assert!(back.is_invalid());
        assert_eq!(
            dec.take_asset_misses(),
            vec![AssetMiss {
                key: AssetKey::Guid(id(9)),
                code: ASSET_UNKNOWN
            }]
        );
        // No resolver at all: the same, with its own code.
        let none = CodecContext::detached();
        let back: Handle<Mesh> = decode_from_slice(&bytes, &none).expect("decodes");
        assert!(back.is_invalid());
        assert_eq!(none.take_asset_misses()[0].code, ASSET_NO_RESOLVER);
        // The key names an asset of another type.
        let mut sounds = Assets::<Sound>::new();
        let s = sounds.insert(id(9), Sound(1));
        let table = NetAssetTable::new();
        table.insert_id(&s, id(9));
        let other = AssetContext::new(Arc::new(table));
        let dec = CodecContext::detached().with_assets(Some(&other));
        let back: Handle<Mesh> = decode_from_slice(&bytes, &dec).expect("decodes");
        assert!(back.is_invalid());
        assert_eq!(dec.take_asset_misses()[0].code, ASSET_WRONG_TYPE);
        assert_eq!(
            AssetResolveError::LoadFailed("x".into()).code(),
            "asset_load_failed"
        );
    }

    #[test]
    fn unmapped_handles_travel_as_none_and_count() {
        let mut slab = Assets::<Mesh>::new();
        let h = slab.insert(id(3), Mesh(3));
        let cx = AssetContext::new(Arc::new(NetAssetTable::new()));
        let enc = CodecContext::detached().with_assets(Some(&cx));
        let bytes = encode_to_vec(&h, &enc);
        assert_eq!(bytes, encode_to_vec(&Handle::<Mesh>::INVALID, &enc));
        assert_eq!(cx.unmapped(), 1);
        // No context: none, uncounted.
        assert_eq!(encode_to_vec(&h, &CodecContext::detached()), bytes);
    }

    #[test]
    fn the_loader_resolves_and_remembers() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let table = NetAssetTable::with_loader(move |ty, key| {
            seen.fetch_add(1, Ordering::Relaxed);
            match key {
                AssetKey::Path(p) if p == "assets/new.glb" && ty == TypeId::of::<Mesh>() => Ok((5, 2)),
                _ => Err(AssetResolveError::LoadFailed("not in the cook".into())),
            }
        });
        let key = AssetKey::Path("assets/new.glb".into());
        assert_eq!(table.resolve(TypeId::of::<Mesh>(), &key), Ok((5, 2)));
        assert_eq!(table.resolve(TypeId::of::<Mesh>(), &key), Ok((5, 2)));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "loaded once, then remembered");
        assert_eq!(table.key_of(TypeId::of::<Mesh>(), 5, 2), Some(key));
        assert_eq!(table.key_of(TypeId::of::<Mesh>(), 5, 3), None, "stale generation");
        let err = table
            .resolve(TypeId::of::<Mesh>(), &AssetKey::Guid(id(1)))
            .expect_err("load fails");
        assert_eq!(err.code(), "asset_load_failed");
    }

    #[test]
    fn loader_failures_are_remembered_and_the_table_is_bounded() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let table = NetAssetTable::with_loader(move |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            Err(AssetResolveError::LoadFailed("missing".into()))
        });
        let ty = TypeId::of::<Mesh>();
        let key = AssetKey::Path("assets/none.glb".into());
        for _ in 0..50 {
            assert_eq!(
                table.resolve(ty, &key).expect_err("fails").code(),
                "asset_load_failed"
            );
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "one loader run for a repeated bad key"
        );
        // Distinct bad keys: the loader runs at most MAX_FAILED_KEYS times until entries expire.
        for n in 0..(MAX_FAILED_KEYS as u128 * 3) {
            let _ = table.resolve(ty, &AssetKey::Guid(id(n + 10)));
        }
        assert!(calls.load(Ordering::Relaxed) <= MAX_FAILED_KEYS as u32 + 1);
    }

    #[test]
    fn a_reused_slot_takes_the_loaded_generation() {
        let n = std::sync::atomic::AtomicU32::new(0);
        let table = NetAssetTable::with_loader(move |_, _| {
            Ok((7, n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1))
        });
        let ty = TypeId::of::<Mesh>();
        let a = AssetKey::Path("a".into());
        let b = AssetKey::Path("b".into());
        assert_eq!(table.resolve(ty, &a), Ok((7, 1)));
        assert_eq!(table.resolve(ty, &b), Ok((7, 2)));
        assert_eq!(table.key_of(ty, 7, 2), Some(b.clone()));
        assert_eq!(table.key_of(ty, 7, 1), None);
        // Audit net3-correctness #2 (grok): `a` no longer names a live handle. It loads again
        // instead of answering the dead generation.
        assert_eq!(table.resolve_loaded(ty, &a), Err(AssetResolveError::Unknown));
        assert_eq!(table.key_of(ty, 7, 2), Some(b.clone()), "b keeps its slot");
        assert_eq!(table.resolve(ty, &a), Ok((7, 3)), "loaded again");
        assert_eq!(table.key_of(ty, 7, 3), Some(a));
        assert_eq!(table.resolve(ty, &b), Ok((7, 4)), "b lost the slot in turn");
    }

    /// Audit net3-correctness #2: every key registered for a slot (its travel key and the other
    /// keys of the same handle) stops resolving when the slot is reused.
    #[test]
    fn a_reused_slot_unbinds_every_key_of_the_old_handle() {
        let mut slab = Assets::<Mesh>::new();
        let old = slab.insert(id(1), Mesh(1));
        let table = NetAssetTable::new();
        let ty = TypeId::of::<Mesh>();
        table.insert_id(&old, id(1));
        table.insert_path(&old, "assets/one.glb");
        assert_eq!(
            table.resolve_loaded(ty, &AssetKey::Path("assets/one.glb".into())),
            Ok((old.index, old.generation))
        );
        let mut new = old;
        new.generation += 1;
        table.insert_id(&new, id(2));
        for key in [AssetKey::Guid(id(1)), AssetKey::Path("assets/one.glb".into())] {
            assert_eq!(
                table.resolve_loaded(ty, &key),
                Err(AssetResolveError::Unknown),
                "{key} still names the dead generation"
            );
        }
        assert_eq!(
            table.resolve_loaded(ty, &AssetKey::Guid(id(2))),
            Ok((new.index, new.generation))
        );
        assert_eq!(
            table.key_of(ty, new.index, new.generation),
            Some(AssetKey::Guid(id(2)))
        );
        // The old key bound to another slot later is not touched by the slot's next reuse.
        let other = slab.insert(id(3), Mesh(3));
        table.insert_id(&other, id(1));
        let mut newer = new;
        newer.generation += 1;
        table.insert_id(&newer, id(4));
        assert_eq!(
            table.resolve_loaded(ty, &AssetKey::Guid(id(1))),
            Ok((other.index, other.generation))
        );
    }

    /// Binding a key onto a second live handle must not leave it as the first handle's travel
    /// key. `key_of` is what replication sends; a stolen guid has to name the handle it now
    /// resolves to, and the old handle travels as its next key (or as none).
    #[test]
    fn a_stolen_travel_key_is_not_what_the_old_handle_replicates() {
        let mut slab = Assets::<Mesh>::new();
        let first = slab.insert(id(1), Mesh(1));
        let second = slab.insert(id(2), Mesh(2));
        let table = NetAssetTable::new();
        let ty = TypeId::of::<Mesh>();
        table.insert_id(&first, id(1));
        table.insert_path(&first, "assets/first.glb");
        table.insert_id(&second, id(1));
        assert_eq!(
            table.resolve_loaded(ty, &AssetKey::Guid(id(1))),
            Ok((second.index, second.generation))
        );
        assert_eq!(
            table.key_of(ty, first.index, first.generation),
            Some(AssetKey::Path("assets/first.glb".into())),
        );
        assert_eq!(
            table.key_of(ty, second.index, second.generation),
            Some(AssetKey::Guid(id(1)))
        );
        // The old handle's only remaining key, taken by a third handle, leaves it with nothing
        // to travel as. It must not keep sending the key that now names the third asset.
        let third = slab.insert(id(3), Mesh(3));
        table.insert_path(&third, "assets/first.glb");
        assert_eq!(table.key_of(ty, first.index, first.generation), None);
        assert_eq!(
            table.key_of(ty, third.index, third.generation),
            Some(AssetKey::Path("assets/first.glb".into()))
        );
        assert_eq!(
            table.resolve_loaded(ty, &AssetKey::Path("assets/first.glb".into())),
            Ok((third.index, third.generation))
        );
    }

    /// Audit net3-correctness #2: keys whose slot was reused do not count toward
    /// [`MAX_LOADED_KEYS`], so a loader that reuses slots keeps loading new keys.
    #[test]
    fn reused_slots_do_not_fill_the_loaded_key_bound() {
        let n = std::sync::atomic::AtomicU32::new(0);
        let table = NetAssetTable::with_loader(move |_, _| {
            Ok((7, n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1))
        });
        let ty = TypeId::of::<Mesh>();
        for k in 0..(MAX_LOADED_KEYS as u128 + 16) {
            assert!(
                table.resolve(ty, &AssetKey::Guid(id(k))).is_ok(),
                "key {k} refused"
            );
        }
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn missing_keys_are_recorded_once_and_bounded() {
        let mut assets = NetAssets::default();
        let miss = |n: u128| AssetMiss {
            key: AssetKey::Guid(id(n)),
            code: ASSET_UNKNOWN,
        };
        assets.record(vec![miss(1), miss(1), miss(2)], "Look", NetTick(4));
        assert_eq!(assets.missing().count(), 2);
        let m = assets.missing_key(&AssetKey::Guid(id(1))).expect("recorded");
        assert_eq!((m.code, m.count, m.first_tick), (ASSET_UNKNOWN, 2, NetTick(4)));
        let many: Vec<AssetMiss> = (0..(MAX_MISSING_KEYS as u128 + 10))
            .map(|n| miss(n + 100))
            .collect();
        assets.record(many, "Look", NetTick(5));
        assert_eq!(assets.missing().count(), MAX_MISSING_KEYS);
        assert_eq!(assets.missing_overflow, 12);
        assets.clear_missing();
        assert_eq!(assets.missing().count(), 0);
    }

    #[test]
    fn hostile_handle_bytes_are_errors_not_panics() {
        let cx = CodecContext::detached();
        // Present, path kind, a length past the bound.
        let mut w = BitWriter::new();
        w.write_bool(true);
        w.write_bool(true);
        w.write_varint(1 << 40);
        let bytes = w.finish();
        assert!(decode_from_slice::<Handle<Mesh>>(&bytes, &cx).is_err());
        // Present, guid kind, truncated.
        let mut w = BitWriter::new();
        w.write_bool(true);
        w.write_bool(false);
        w.write_bits(7, 64);
        let bytes = w.finish();
        assert!(decode_from_slice::<Handle<Mesh>>(&bytes, &cx).is_err());
    }
}
