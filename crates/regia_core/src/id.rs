// SPDX-License-Identifier: MPL-2.0
//! Identifiers (`docs/specs/10-core.md` § 1.2): [`Guid`], [`Name`], [`TypePathHash`].

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A 128-bit globally unique identifier. `EntityGuid` and `AssetId` are newtypes over it.
///
/// Text form: 32 lowercase hex digits in the 8-4-4-4-12 grouping
/// (`67e55044-10b1-426f-9247-bb680e5fe0c8`); parsing accepts upper case and optional dashes.
/// Serialised as that string in human-readable formats (RON, JSON) and as a `u128` in binary formats
/// (postcard).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct Guid(pub u128);

/// Errors from parsing a [`Guid`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a GUID (32 hex digits, dashes optional)")]
pub struct GuidParseError(pub String);

impl Guid {
    /// The all-zero GUID, used as "none".
    pub const NIL: Self = Self(0);

    /// A random (version 4) GUID.
    pub fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }

    /// Wraps a raw value.
    #[inline]
    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    /// The raw value.
    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// `true` for [`Guid::NIL`].
    #[inline]
    pub const fn is_nil(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (v >> 96) as u32,
            (v >> 80) as u16,
            (v >> 64) as u16,
            (v >> 48) as u16,
            v & 0xffff_ffff_ffff
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Guid({self})")
    }
}

impl FromStr for Guid {
    type Err = GuidParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || GuidParseError(s.to_owned());
        let mut value: u128 = 0;
        let mut digits = 0;
        for c in s.chars().filter(|&c| c != '-') {
            let d = c.to_digit(16).ok_or_else(err)?;
            digits += 1;
            if digits > 32 {
                return Err(err());
            }
            value = (value << 4) | u128::from(d);
        }
        if digits == 32 { Ok(Self(value)) } else { Err(err()) }
    }
}

impl Serialize for Guid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            serializer.serialize_u128(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Guid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Guid;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a GUID string or u128")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Guid, E> {
                v.parse().map_err(E::custom)
            }

            fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<Guid, E> {
                Ok(Guid(v))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Guid, E> {
                Ok(Guid(u128::from(v)))
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_str(Visitor)
        } else {
            deserializer.deserialize_u128(Visitor)
        }
    }
}

/// A human-readable name: an immutable shared string whose clone is a reference-count increment.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Arc<str>);

impl Name {
    /// Creates a name.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self(name.into())
    }

    /// The name as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Name {
    fn default() -> Self {
        Self(Arc::from(""))
    }
}

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for Name {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl From<Arc<str>> for Name {
    fn from(s: Arc<str>) -> Self {
        Self(s)
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `Box<str>` accepts both borrowed and owned strings, so this works with every format.
        Ok(Self(Arc::from(Box::<str>::deserialize(deserializer)?)))
    }
}

/// A stable 64-bit hash of a type path string (FNV-1a), for compact binary references to types.
/// A leading engine crate name (`regia` or `regia_*`) is hashed in its pre-rename spelling, so an
/// external FNV-1a of the string as written differs from [`TypePathHash::of`] for engine types.
/// Stable across builds, platforms and Rust versions, unlike `TypeId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct TypePathHash(pub u64);

impl TypePathHash {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Hashes `type_path` (e.g. `"regia_scene::Transform"`). Usable in `const` contexts.
    ///
    /// Engine crate names are hashed in their spelling from before the rename to Regia Engine
    /// ([`brand::fingerprint_type_path`](crate::brand::fingerprint_type_path), ADR-1429): cooked
    /// asset headers, paks and cooked scenes store these hashes, so a file written before the rename
    /// keeps its tag, and a file written now has the bytes it had then. Every other crate name is
    /// hashed as written.
    pub const fn of(type_path: &str) -> Self {
        use crate::brand::{CRATE_PREFIX, LEGACY_CRATE_PREFIX, LEGACY_MAIN_CRATE, MAIN_CRATE};
        let bytes = type_path.as_bytes();
        let mut hash = Self::OFFSET_BASIS;
        let mut i = 0;
        while i < bytes.len() {
            if !is_ident_byte(bytes[i]) {
                hash = Self::mix(hash, bytes, i, i + 1);
                i += 1;
                continue;
            }
            // `i` starts an identifier: a crate name when `::` follows and no `::` precedes it
            // (the rule of `brand::rebrand_type_path`).
            let mut end = i;
            while end < bytes.len() && is_ident_byte(bytes[end]) {
                end += 1;
            }
            let after_separator = i >= 2 && bytes[i - 2] == b':' && bytes[i - 1] == b':';
            let is_crate =
                end + 1 < bytes.len() && bytes[end] == b':' && bytes[end + 1] == b':' && !after_separator;
            let mut rest = i;
            if is_crate && bytes_eq(bytes, i, end, MAIN_CRATE.as_bytes()) {
                hash = Self::mix(hash, LEGACY_MAIN_CRATE.as_bytes(), 0, LEGACY_MAIN_CRATE.len());
                rest = end;
            } else if is_crate && bytes_eq(bytes, i, i + CRATE_PREFIX.len(), CRATE_PREFIX.as_bytes()) {
                hash = Self::mix(hash, LEGACY_CRATE_PREFIX.as_bytes(), 0, LEGACY_CRATE_PREFIX.len());
                rest = i + CRATE_PREFIX.len();
            }
            hash = Self::mix(hash, bytes, rest, end);
            i = end;
        }
        Self(hash)
    }

    /// FNV-1a over `bytes[start..end]`, continuing from `hash`.
    const fn mix(mut hash: u64, bytes: &[u8], start: usize, end: usize) -> u64 {
        let mut i = start;
        while i < end {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(Self::PRIME);
            i += 1;
        }
        hash
    }

    /// The raw hash.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Whether `b` can be part of a Rust identifier (ASCII only; type paths of reflected types are).
const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `bytes[start..end]` equals `want` (`false` when the range is out of bounds).
const fn bytes_eq(bytes: &[u8], start: usize, end: usize, want: &[u8]) -> bool {
    if end > bytes.len() || end < start || end - start != want.len() {
        return false;
    }
    let mut k = 0;
    while k < want.len() {
        if bytes[start + k] != want[k] {
            return false;
        }
        k += 1;
    }
    true
}

impl fmt::Display for TypePathHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SAMPLE: Guid = Guid(0x67e5_5044_10b1_426f_9247_bb68_0e5f_e0c8);

    #[test]
    fn guid_text_round_trip() {
        assert_eq!(SAMPLE.to_string(), "67e55044-10b1-426f-9247-bb680e5fe0c8");
        assert_eq!("67e55044-10b1-426f-9247-bb680e5fe0c8".parse(), Ok(SAMPLE));
        assert_eq!("67E5504410B1426F9247BB680E5FE0C8".parse(), Ok(SAMPLE));
        assert_eq!(Guid::NIL.to_string(), "00000000-0000-0000-0000-000000000000");
        assert_eq!(
            format!("{SAMPLE:?}"),
            "Guid(67e55044-10b1-426f-9247-bb680e5fe0c8)"
        );
        assert!("67e55044".parse::<Guid>().is_err());
        assert!("67e55044-10b1-426f-9247-bb680e5fe0c8a".parse::<Guid>().is_err());
        assert!("x7e55044-10b1-426f-9247-bb680e5fe0c8".parse::<Guid>().is_err());
        assert!("".parse::<Guid>().is_err());
        for _ in 0..100 {
            let g = Guid::new_random();
            assert!(!g.is_nil());
            assert_eq!(g.to_string().parse(), Ok(g));
            // Version 4, RFC 4122 variant.
            assert_eq!((g.0 >> 76) & 0xf, 4);
            assert_eq!((g.0 >> 62) & 0b11, 0b10);
        }
        assert_ne!(Guid::new_random(), Guid::new_random());
    }

    #[test]
    fn guid_serde_is_string_in_text_and_u128_in_binary() {
        assert_eq!(
            ron::to_string(&SAMPLE).expect("ron"),
            "\"67e55044-10b1-426f-9247-bb680e5fe0c8\""
        );
        assert_eq!(
            ron::from_str::<Guid>("\"67e55044-10b1-426f-9247-bb680e5fe0c8\"").expect("ron"),
            SAMPLE
        );
        assert_eq!(
            serde_json::from_str::<Guid>(&serde_json::to_string(&SAMPLE).expect("json")).expect("json"),
            SAMPLE
        );
        assert!(ron::from_str::<Guid>("\"nope\"").is_err());

        let bytes = postcard::to_allocvec(&SAMPLE).expect("postcard");
        assert_eq!(
            bytes,
            postcard::to_allocvec(&SAMPLE.0).expect("postcard"),
            "encoded as a u128"
        );
        assert_eq!(postcard::from_bytes::<Guid>(&bytes).expect("postcard"), SAMPLE);
    }

    #[test]
    fn name_behaves_like_a_str() {
        let n = Name::from("Player Start");
        let m = n.clone();
        assert!(
            std::ptr::eq(n.as_str(), m.as_str()),
            "clone shares the allocation"
        );
        assert_eq!(n, "Player Start");
        assert_eq!(n.len(), 12);
        assert_eq!(n.to_string(), "Player Start");
        assert_eq!(format!("{n:?}"), "\"Player Start\"");
        let mut map = HashMap::new();
        map.insert(n, 1);
        assert_eq!(map.get("Player Start"), Some(&1));
        assert_eq!(Name::default().as_str(), "");
        let ron_text = ron::to_string(&m).expect("ron");
        assert_eq!(ron::from_str::<Name>(&ron_text).expect("ron"), m);
        let bin = postcard::to_allocvec(&m).expect("postcard");
        assert_eq!(postcard::from_bytes::<Name>(&bin).expect("postcard"), m);
        assert_eq!(
            serde_json::from_str::<Name>("\"a\\nb\"").expect("escaped string"),
            "a\nb"
        );
    }

    #[test]
    fn type_path_hash_matches_fnv1a_reference_values() {
        // Reference values from the published FNV-1a 64-bit test suite.
        assert_eq!(TypePathHash::of("").get(), 0xcbf29ce484222325);
        assert_eq!(TypePathHash::of("a").get(), 0xaf63dc4c8601ec8c);
        assert_eq!(TypePathHash::of("foobar").get(), 0x85944171f73967e8);
        const H: TypePathHash = TypePathHash::of("regia_scene::Transform");
        assert_eq!(H, TypePathHash::of("regia_scene::Transform"));
        assert_ne!(H, TypePathHash::of("regia_scene::GlobalTransform"));
        assert_eq!(TypePathHash(0xab).to_string(), "00000000000000ab");
    }

    /// FNV-1a of the bytes as written, with no brand mapping.
    fn raw_fnv(text: &str) -> u64 {
        TypePathHash::mix(TypePathHash::OFFSET_BASIS, text.as_bytes(), 0, text.len())
    }

    #[test]
    fn type_path_hash_keeps_the_tags_written_before_the_rename() {
        use crate::brand::{CRATE_PREFIX, LEGACY_CRATE_PREFIX, LEGACY_MAIN_CRATE, MAIN_CRATE};
        // The type tag every cooked mesh header carried before the rename (FNV-1a of the legacy
        // crate spelling of `CookedMesh`'s path), as the v0.1.x engines wrote it.
        let legacy_mesh = format!("{LEGACY_CRATE_PREFIX}asset::CookedMesh");
        assert_eq!(raw_fnv(&legacy_mesh), 0x196b_6b59_6713_3bc7);
        assert_eq!(TypePathHash::of(&legacy_mesh).get(), 0x196b_6b59_6713_3bc7);
        let current_mesh = format!("{CRATE_PREFIX}asset::CookedMesh");
        assert_eq!(TypePathHash::of(&current_mesh).get(), 0x196b_6b59_6713_3bc7);
        // Every crate segment maps, the main crate too; anything else hashes as written.
        for path in [
            format!("{CRATE_PREFIX}scene::transform::Transform"),
            format!("{MAIN_CRATE}::prelude::Thing"),
            format!("core::option::Option<{CRATE_PREFIX}render::PointLight>"),
            format!("{CRATE_PREFIX}core::Handle<{CRATE_PREFIX}asset::CpuMesh>"),
            format!("my_game::{CRATE_PREFIX}bits::Thing"),
            format!("my_game::{MAIN_CRATE}"),
            format!("{MAIN_CRATE}:1.2.3"),
            CRATE_PREFIX.to_owned(),
            "f32".to_owned(),
            String::new(),
        ] {
            let legacy = crate::brand::fingerprint_type_path(&path);
            assert_eq!(TypePathHash::of(&path).get(), raw_fnv(&legacy), "{path}");
            assert_eq!(TypePathHash::of(&legacy), TypePathHash::of(&path), "{path}");
        }
        let game_type = "my_game::Door";
        assert_eq!(TypePathHash::of(game_type).get(), raw_fnv(game_type));
        assert_eq!(
            TypePathHash::of(&format!("{MAIN_CRATE}::X")).get(),
            raw_fnv(&format!("{LEGACY_MAIN_CRATE}::X"))
        );
    }
}
