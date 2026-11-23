// SPDX-License-Identifier: MPL-2.0
//! The `ecs` half of the replication codec (spec 120 § 3.2, ADR-8032, ADR-8501): the `Entity` codec,
//! the asset-server context and the `Handle<T>` / `AssetRef<T>` codecs, plus the `CodecContext`
//! constructors that take an entity map or an asset context. Built only with the `ecs` feature; every
//! public item is re-exported from `regia_net::codec`, so its path is the same as before the split.

use std::any::TypeId;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_ecs::prelude::Entity;

use super::{CodecContext, ENTITY_LAYOUT, LayoutHasher, ReplicateCodec, decode_str, encode_str};
use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::model::NetId;

/// Maps entities to network ids and back while encoding or decoding.
pub trait EntityMap {
    /// The `NetId` of a replicated entity (`None` when it is not replicated).
    fn to_net(&self, entity: Entity) -> Option<NetId>;
    /// The local entity of a `NetId` (`None` when it is not spawned here).
    fn to_entity(&self, id: NetId) -> Option<Entity>;
}

impl<'a> CodecContext<'a> {
    /// A context with an entity map.
    pub fn new(map: &'a dyn EntityMap) -> Self {
        Self {
            map: Some(map),
            assets: None,
            may_load: true,
            stats: RefCell::default(),
            asset_misses: RefCell::default(),
        }
    }

    /// The same context with an asset-server context, so `Handle<T>` values travel as their
    /// stable asset key (spec 71 § 3). `None` leaves the context without one.
    #[must_use]
    pub fn with_assets(mut self, assets: Option<&'a AssetContext>) -> Self {
        self.assets = assets;
        self
    }

    /// The same context for bytes a remote peer chose: a key of a `Handle<T>` resolves only to an
    /// asset the receiver already holds, and never starts a load (ADR-2665 (2)). A server decodes
    /// client messages this way, so a client cannot make it load whatever path it names.
    #[must_use]
    pub fn without_loading(mut self) -> Self {
        self.may_load = false;
        self
    }

    /// The asset-server context, when there is one.
    pub fn assets(&self) -> Option<&'a AssetContext> {
        self.assets
    }

    /// Takes the asset handles that did not resolve while decoding (each decoded unset).
    pub fn take_asset_misses(&self) -> Vec<AssetMiss> {
        std::mem::take(&mut *self.asset_misses.borrow_mut())
    }

    fn net_of(&self, entity: Entity) -> Option<NetId> {
        if entity == Entity::PLACEHOLDER {
            return None;
        }
        let id = self.map.and_then(|m| m.to_net(entity));
        if id.is_none() {
            self.stats.borrow_mut().unreplicated_refs += 1;
        }
        id
    }

    fn entity_of(&self, id: NetId) -> Entity {
        self.stats.borrow_mut().referenced.push(id);
        match self.map.and_then(|m| m.to_entity(id)) {
            Some(e) => e,
            None => {
                self.stats.borrow_mut().unresolved.push(id);
                Entity::PLACEHOLDER
            }
        }
    }
}

// ---- the asset-server context (spec 71 § 3, ADR-2415) -----------------------------------------

/// Error code: a decoded asset key names no asset the receiver's resolver knows.
pub const ASSET_UNKNOWN: &str = "asset_unknown";
/// Error code: the key names an asset of another type than the handle's.
pub const ASSET_WRONG_TYPE: &str = "asset_wrong_type";
/// Error code: the receiver's resolver knows the asset but could not load it.
pub const ASSET_LOAD_FAILED: &str = "asset_load_failed";
/// Error code: a handle arrived on a codec with no asset-server context.
pub const ASSET_NO_RESOLVER: &str = "asset_no_resolver";
/// The log code of a handle that did not resolve on a receiver (spec 71 § 3), logged once per key.
pub const ASSET_MISSING_LOG: &str = "net.asset_missing";

/// Longest asset path a handle travels as, in bytes.
pub const MAX_ASSET_PATH_LEN: usize = 1024;

/// What a `Handle<T>` travels as: the asset's stable guid, or its project path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AssetKey {
    /// The asset's `AssetId` (128-bit guid).
    Guid(regia_asset::AssetId),
    /// The asset's project-relative path (`assets/props/crate.glb`).
    Path(String),
}

impl std::fmt::Display for AssetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guid(id) => write!(f, "{:032x}", id.0.as_u128()),
            Self::Path(p) => write!(f, "path:{p}"),
        }
    }
}

/// Why a decoded asset key did not resolve. [`AssetResolveError::code`] is the stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetResolveError {
    /// No asset with that guid or path (`asset_unknown`).
    #[error("no asset with that id or path")]
    Unknown,
    /// The asset exists with another type (`asset_wrong_type`).
    #[error("the asset is not of the handle's type")]
    WrongType,
    /// The asset is known but did not load (`asset_load_failed`).
    #[error("the asset failed to load: {0}")]
    LoadFailed(String),
}

impl AssetResolveError {
    /// The stable lowercase code ([`ASSET_UNKNOWN`], [`ASSET_WRONG_TYPE`], [`ASSET_LOAD_FAILED`]).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unknown => ASSET_UNKNOWN,
            Self::WrongType => ASSET_WRONG_TYPE,
            Self::LoadFailed(_) => ASSET_LOAD_FAILED,
        }
    }
}

/// One handle that decoded unset because its key did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMiss {
    /// The key that arrived.
    pub key: AssetKey,
    /// Why: [`ASSET_UNKNOWN`], [`ASSET_WRONG_TYPE`], [`ASSET_LOAD_FAILED`] or [`ASSET_NO_RESOLVER`].
    pub code: &'static str,
}

/// Maps runtime asset handles to stable keys and back: the asset server's side of the codec.
///
/// `asset_type` is the `TypeId` of the handle's `T`; `index` and `generation` are the handle's
/// slab slot. Implementations must be cheap to call during a gather (a table lookup), and may load
/// in `resolve` (the receiver's asset server). [`crate::assets::NetAssetTable`] is the stock one.
pub trait AssetResolver: Send + Sync {
    /// The key a live handle travels as (`None` for a handle the asset server did not hand out).
    fn key_of(&self, asset_type: TypeId, index: u32, generation: u32) -> Option<AssetKey>;
    /// The handle slot of a key, loading the asset when needed.
    fn resolve(&self, asset_type: TypeId, key: &AssetKey) -> Result<(u32, u32), AssetResolveError>;
    /// As [`Self::resolve`], for a key a remote peer chose: a resolver that can load must only
    /// answer from what it already holds. The default is [`Self::resolve`], for a resolver that
    /// never loads.
    fn resolve_loaded(&self, asset_type: TypeId, key: &AssetKey) -> Result<(u32, u32), AssetResolveError> {
        self.resolve(asset_type, key)
    }
}

/// The asset-server context of a codec: the resolver, and the count of handles sent as none
/// because the resolver did not know them (`asset_unmapped`).
pub struct AssetContext {
    resolver: Arc<dyn AssetResolver>,
    unmapped: AtomicU64,
}

impl std::fmt::Debug for AssetContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetContext")
            .field("unmapped", &self.unmapped())
            .finish_non_exhaustive()
    }
}

impl AssetContext {
    /// A context over a resolver.
    pub fn new(resolver: Arc<dyn AssetResolver>) -> Self {
        Self {
            resolver,
            unmapped: AtomicU64::new(0),
        }
    }

    /// The resolver.
    pub fn resolver(&self) -> &dyn AssetResolver {
        &*self.resolver
    }

    /// Handles sent as none because the resolver had no key for them.
    pub fn unmapped(&self) -> u64 {
        self.unmapped.load(Ordering::Relaxed)
    }

    fn count_unmapped(&self) {
        self.unmapped.fetch_add(1, Ordering::Relaxed);
    }
}

/// A `Handle<T>` travels as its stable asset key (spec 71 § 3): a presence bit, a kind bit, then
/// the 128-bit guid or the length-prefixed path. The invalid handle, and a handle the sender's
/// resolver does not know (counted as `asset_unmapped`), travel as none. On the receiver the key
/// resolves through its asset-server context; a key that does not resolve decodes as
/// `Handle::INVALID` and is recorded as an [`AssetMiss`] with its code, never as a decode error.
impl<T: Clone + 'static> ReplicateCodec for regia_asset::Handle<T> {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        // `Handle<T>` is `Copy` only when `T` is, so the slot is read through the fields.
        if self.index == u32::MAX {
            w.write_bool(false);
            return;
        }
        let key = cx.assets.and_then(|a| {
            let key = a.resolver.key_of(TypeId::of::<T>(), self.index, self.generation);
            if key.is_none() {
                a.count_unmapped();
            }
            key
        });
        match key {
            None => w.write_bool(false),
            Some(AssetKey::Guid(id)) => {
                w.write_bool(true);
                w.write_bool(false);
                let v = id.0.as_u128();
                w.write_bits(v as u64, 64);
                w.write_bits((v >> 64) as u64, 64);
            }
            Some(AssetKey::Path(p)) => {
                w.write_bool(true);
                w.write_bool(true);
                encode_str(&p, MAX_ASSET_PATH_LEN, w, cx);
            }
        }
    }
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        if !r.read_bool()? {
            return Ok(Self::INVALID);
        }
        let key = if r.read_bool()? {
            AssetKey::Path(decode_str(r, MAX_ASSET_PATH_LEN)?)
        } else {
            let lo = u128::from(r.read_bits(64)?);
            let hi = u128::from(r.read_bits(64)?);
            AssetKey::Guid(regia_asset::AssetId(regia_core::id::Guid::from_u128(
                lo | (hi << 64),
            )))
        };
        let resolved = match cx.assets {
            Some(a) if cx.may_load => a.resolver.resolve(TypeId::of::<T>(), &key).map_err(|e| e.code()),
            Some(a) => a
                .resolver
                .resolve_loaded(TypeId::of::<T>(), &key)
                .map_err(|e| e.code()),
            None => Err(ASSET_NO_RESOLVER),
        };
        match resolved {
            Ok((index, generation)) if index != u32::MAX => {
                let mut h = Self::INVALID;
                h.index = index;
                h.generation = generation;
                Ok(h)
            }
            Ok(_) => {
                cx.asset_misses.borrow_mut().push(AssetMiss {
                    key,
                    code: ASSET_UNKNOWN,
                });
                Ok(Self::INVALID)
            }
            Err(code) => {
                cx.asset_misses.borrow_mut().push(AssetMiss { key, code });
                Ok(Self::INVALID)
            }
        }
    }
    /// Compares the slots: two handles to one slot always travel as the same key. (Two different
    /// handles the resolver does not know both travel as none; comparing them as different costs
    /// one redundant field, never a missed change.)
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        (self.index == u32::MAX && other.index == u32::MAX)
            || (self.index == other.index && self.generation == other.generation)
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("handle");
    }
}

/// An `Entity` travels as its `NetId` (spec 71 § 5): a presence bit plus a varint. A target that
/// is not replicated travels as none; an id with no local entity decodes as
/// `Entity::PLACEHOLDER` and is recorded in [`super::CodecStats::unresolved`].
impl ReplicateCodec for Entity {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        match cx.net_of(*self) {
            Some(id) => {
                w.write_bool(true);
                w.write_varint(u64::from(id.0));
            }
            None => w.write_bool(false),
        }
    }
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        if !r.read_bool()? {
            return Ok(Entity::PLACEHOLDER);
        }
        let id = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("net id"))?;
        Ok(cx.entity_of(NetId(id)))
    }
    fn wire_eq(&self, other: &Self, cx: &CodecContext) -> bool {
        let cx = cx.scratch();
        cx.net_of(*self) == cx.net_of(*other)
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str(ENTITY_LAYOUT);
    }
}

/// An `AssetRef<T>` travels as its `AssetId` (128 bits).
impl<T: Clone + 'static> ReplicateCodec for regia_asset::AssetRef<T> {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let v = self.id.0.as_u128();
        w.write_bits(v as u64, 64);
        w.write_bits((v >> 64) as u64, 64);
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let lo = u128::from(r.read_bits(64)?);
        let hi = u128::from(r.read_bits(64)?);
        Ok(Self::new(regia_asset::AssetId(regia_core::id::Guid::from_u128(
            lo | (hi << 64),
        ))))
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self.id == other.id
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("asset_ref");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_from_slice, encode_to_vec};

    #[test]
    fn unreplicated_entities_compare_equal() {
        let cx = CodecContext::detached();
        // Unreplicated entities all travel as none.
        assert!(Entity::PLACEHOLDER.wire_eq(&Entity::PLACEHOLDER, &cx));
    }

    #[test]
    fn entities_travel_as_net_ids() {
        struct Map;
        impl EntityMap for Map {
            fn to_net(&self, e: Entity) -> Option<NetId> {
                (e.index_u32() == 5).then_some(NetId(9))
            }
            fn to_entity(&self, id: NetId) -> Option<Entity> {
                (id == NetId(9)).then(|| Entity::from_raw_u32(77).unwrap_or(Entity::PLACEHOLDER))
            }
        }
        let map = Map;
        let cx = CodecContext::new(&map);
        let five = Entity::from_raw_u32(5).unwrap_or(Entity::PLACEHOLDER);
        let six = Entity::from_raw_u32(6).unwrap_or(Entity::PLACEHOLDER);
        let bytes = encode_to_vec(&vec![five, six], &cx);
        assert_eq!(cx.take_stats().unreplicated_refs, 1);
        let back: Vec<Entity> = decode_from_slice(&bytes, &cx).expect("decode");
        assert_eq!(back[0].index_u32(), 77);
        assert_eq!(back[1], Entity::PLACEHOLDER);
        // An id with no local entity decodes as the placeholder and is recorded.
        let detached = CodecContext::detached();
        let mut w = BitWriter::new();
        w.write_bool(true);
        w.write_varint(3);
        let bytes = w.finish();
        let e: Entity = decode_from_slice(&bytes, &detached).expect("decode");
        assert_eq!(e, Entity::PLACEHOLDER);
        assert_eq!(detached.take_stats().unresolved, vec![NetId(3)]);
    }
}
