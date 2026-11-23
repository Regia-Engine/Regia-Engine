// SPDX-License-Identifier: MPL-2.0
//! Asset identity (`docs/specs/12-assets.md` § 2).

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// Stable identifier for a cooked asset. Assigned at first import; never changes when the source moves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct AssetId(pub Guid);

impl AssetId {
    /// A new random v4 id.
    ///
    /// ```
    /// use regia_asset::AssetId;
    ///
    /// let id = AssetId::new_random();
    /// assert!(!id.is_nil());
    /// assert_ne!(id, AssetId::new_random());
    /// // The text form round-trips.
    /// assert_eq!(id.to_string().parse::<AssetId>().unwrap(), id);
    /// ```
    pub fn new_random() -> Self {
        Self(Guid::new_random())
    }

    /// Deterministic sub-asset id: `hash(parent_id, sub_asset_path)` per spec § 2.
    ///
    /// Uses BLAKE3 over the parent guid bytes and the UTF-8 path, then takes the first 16 bytes as a
    /// `Guid` (same technique as UUID v5-style derivation from public RFC 4122 namespace ideas).
    pub fn sub_asset(parent: AssetId, sub_asset_path: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&parent.0.as_u128().to_le_bytes());
        hasher.update(sub_asset_path.as_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash.as_bytes()[..16]);
        // Set RFC 4122 variant and version 4 bits so the value looks like a random guid.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let value = u128::from_le_bytes(bytes);
        Self(Guid::from_u128(value))
    }

    /// The nil id.
    pub const NIL: Self = Self(Guid::NIL);

    /// True when this is [`AssetId::NIL`].
    pub fn is_nil(self) -> bool {
        self.0.is_nil()
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({})", self.0)
    }
}

impl FromStr for AssetId {
    type Err = regia_core::id::GuidParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// Serializable typed reference to an asset (`docs/specs/12-assets.md` § 2). Reflected as one
/// opaque value (`(id: "<guid>")` in a scene), so a component field of this type persists.
#[derive(Serialize, Deserialize, Reflect)]
#[reflect(opaque, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRef<T> {
    /// Target asset id.
    pub id: AssetId,
    #[serde(skip)]
    _ty: PhantomData<T>,
}

impl<T> AssetRef<T> {
    /// Wrap an id.
    ///
    /// ```
    /// use regia_asset::{AssetId, AssetRef};
    ///
    /// struct Texture;
    /// let id = AssetId::new_random();
    /// let a: AssetRef<Texture> = AssetRef::new(id);
    /// let b = a; // `Copy` without `Texture: Copy`
    /// assert_eq!(a, b);
    /// assert_eq!(b.id, id);
    /// ```
    pub fn new(id: AssetId) -> Self {
        Self { id, _ty: PhantomData }
    }
}

// By hand: `T` is only a marker, so none of these may ask anything of it.
impl<T> Clone for AssetRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for AssetRef<T> {}

impl<T> PartialEq for AssetRef<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for AssetRef<T> {}

impl<T> std::hash::Hash for AssetRef<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T> Default for AssetRef<T> {
    fn default() -> Self {
        Self::new(AssetId::default())
    }
}

impl<T> fmt::Debug for AssetRef<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetRef<{}>({})", std::any::type_name::<T>(), self.id)
    }
}

/// Runtime strong handle to a loaded asset.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle<T> {
    /// Index into the `Assets<T>` slab.
    pub index: u32,
    /// Generation for stale-handle detection.
    pub generation: u32,
    pub(crate) _ty: PhantomData<T>,
}

impl<T> Handle<T> {
    /// Construct from index and generation (storage layer).
    pub(crate) fn from_raw(index: u32, generation: u32) -> Self {
        Self {
            index,
            generation,
            _ty: PhantomData,
        }
    }
}

impl<T> Handle<T> {
    /// Sentinel invalid handle.
    pub const INVALID: Self = Self {
        index: u32::MAX,
        generation: 0,
        _ty: PhantomData,
    };

    /// True when this is [`Handle::INVALID`].
    pub fn is_invalid(self) -> bool {
        self.index == u32::MAX
    }
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Handle<{}>({}, gen {})",
            std::any::type_name::<T>(),
            self.index,
            self.generation
        )
    }
}

/// Weak runtime reference; does not keep the asset alive.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct WeakHandle<T> {
    inner: Handle<T>,
}

impl<T> WeakHandle<T> {
    /// Create from a strong handle (does not change refcount).
    pub fn new(strong: Handle<T>) -> Self {
        Self { inner: strong }
    }

    /// The underlying index/generation pair.
    pub fn inner(&self) -> Handle<T> {
        Handle::from_raw(self.inner.index, self.inner.generation)
    }
}

/// Type-erased strong handle.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UntypedHandle {
    /// Slab index.
    pub index: u32,
    /// Generation.
    pub generation: u32,
    /// Cooked type tag.
    pub type_tag: regia_core::id::TypePathHash,
}

/// The content hash the asset tools record for a file (BLAKE3, lowercase hex). `regia loc compile` writes
/// it into `loc/index.ron` and `Localization::load_bundles` checks it (spec 36 § 4).
#[must_use]
pub fn content_hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
