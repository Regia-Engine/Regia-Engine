// SPDX-License-Identifier: MPL-2.0
//! The replication value codec (spec 71 § 3): [`ReplicateCodec`], the field codecs used by
//! `#[derive(Replicate)]`, quantization, the layout hash that feeds the protocol hash, and the
//! asset-server context that lets a `Handle<T>` travel as its stable asset key (`AssetContext`).
//!
//! Determinism (spec 71 § 12): quantization is one `f32` subtract, one multiply and one round per
//! value, then integer arithmetic. No fused multiply-add is used, so the wire bytes are identical
//! on every platform.

use std::cell::RefCell;
use std::f32::consts::{PI, TAU};

use glam::{Quat, Vec2, Vec3, Vec4};

use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::model::NetId;

// The `Entity` codec, the asset-server context and the asset codecs live in the `ecs`-gated
// `codec/ecs.rs` (spec 120 § 3.2, ADR-8501); their public items keep their `codec::` paths.
#[cfg(feature = "ecs")]
mod ecs;
#[cfg(feature = "ecs")]
pub use ecs::{
    ASSET_LOAD_FAILED, ASSET_MISSING_LOG, ASSET_NO_RESOLVER, ASSET_UNKNOWN, ASSET_WRONG_TYPE, AssetContext,
    AssetKey, AssetMiss, AssetResolveError, AssetResolver, EntityMap, MAX_ASSET_PATH_LEN,
};

/// Default upper bound for `String` (bytes) and `Vec` (elements) fields (spec 71 § 3).
pub const DEFAULT_MAX_LEN: usize = 1024;

/// Counters a codec run collects (read by the replication layer after each encode or decode).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CodecStats {
    /// Values clamped by a quantizer or truncated by a length bound.
    pub clamped: u32,
    /// Entity references to entities that are not replicated (sent as none).
    pub unreplicated_refs: u32,
    /// `NetId`s decoded that have no local entity yet (left as `Entity::PLACEHOLDER`).
    pub unresolved: Vec<NetId>,
    /// Every `NetId` decoded (resolved or not), so a client knows which copies refer to which.
    pub referenced: Vec<NetId>,
}

/// What an encode or decode needs besides the bits: the entity map, the asset-server context
/// and the counters. Without the `ecs` feature (ADR-8032) it holds only the counters: entities and
/// asset handles are ECS types and their codecs are not built.
pub struct CodecContext<'a> {
    #[cfg(feature = "ecs")]
    map: Option<&'a dyn EntityMap>,
    #[cfg(feature = "ecs")]
    assets: Option<&'a AssetContext>,
    /// False when a remote peer chose the bytes: keys resolve only to assets already held.
    #[cfg(feature = "ecs")]
    may_load: bool,
    stats: RefCell<CodecStats>,
    #[cfg(feature = "ecs")]
    asset_misses: RefCell<Vec<AssetMiss>>,
    #[cfg(not(feature = "ecs"))]
    lifetime: std::marker::PhantomData<&'a ()>,
}

impl std::fmt::Debug for CodecContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("CodecContext");
        #[cfg(feature = "ecs")]
        d.field("mapped", &self.map.is_some())
            .field("assets", &self.assets.is_some());
        d.field("stats", &self.stats.borrow()).finish()
    }
}

impl<'a> CodecContext<'a> {
    /// A context with no entity map: every entity encodes as none and decodes as
    /// `Entity::PLACEHOLDER`, and every asset handle encodes as none and decodes unset.
    pub fn detached() -> CodecContext<'static> {
        CodecContext {
            #[cfg(feature = "ecs")]
            map: None,
            #[cfg(feature = "ecs")]
            assets: None,
            #[cfg(feature = "ecs")]
            may_load: true,
            stats: RefCell::default(),
            #[cfg(feature = "ecs")]
            asset_misses: RefCell::default(),
            #[cfg(not(feature = "ecs"))]
            lifetime: std::marker::PhantomData,
        }
    }

    /// The same map and asset context with fresh counters (used for comparisons that must not
    /// count).
    pub fn scratch(&self) -> CodecContext<'a> {
        CodecContext {
            #[cfg(feature = "ecs")]
            map: self.map,
            #[cfg(feature = "ecs")]
            assets: self.assets,
            #[cfg(feature = "ecs")]
            may_load: self.may_load,
            stats: RefCell::default(),
            #[cfg(feature = "ecs")]
            asset_misses: RefCell::default(),
            #[cfg(not(feature = "ecs"))]
            lifetime: std::marker::PhantomData,
        }
    }

    /// Takes the counters collected so far.
    pub fn take_stats(&self) -> CodecStats {
        std::mem::take(&mut *self.stats.borrow_mut())
    }

    /// Counts one clamped or truncated value.
    pub fn count_clamped(&self) {
        self.stats.borrow_mut().clamped += 1;
    }
}

/// What an `Entity` field writes to the layout.
const ENTITY_LAYOUT: &str = "entity";

/// Feeds a codec's shape into the protocol hash (spec 71 § 11: component list, order, audience
/// variant and every field's codec).
#[derive(Debug)]
pub struct LayoutHasher {
    hasher: blake3::Hasher,
    saw_entity: bool,
}

impl Default for LayoutHasher {
    fn default() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            saw_entity: false,
        }
    }
}

impl LayoutHasher {
    /// A fresh hasher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a string (length-prefixed, so `"ab" + "c"` differs from `"a" + "bc"`).
    pub fn write_str(&mut self, s: &str) {
        self.saw_entity |= s == ENTITY_LAYOUT;
        self.hasher.update(&(s.len() as u64).to_le_bytes());
        self.hasher.update(s.as_bytes());
    }

    /// Whether an `Entity` field went by: values of this codec can hold entity references (a recorder heals those,
    /// spec 103 § 3.6). A hand-written codec that sends an entity some other way should `write_str("entity")`.
    #[must_use]
    pub fn saw_entity(&self) -> bool {
        self.saw_entity
    }

    /// Adds an integer.
    pub fn write_u64(&mut self, v: u64) {
        self.hasher.update(&v.to_le_bytes());
    }

    /// Adds a float by its bits.
    pub fn write_f32(&mut self, v: f32) {
        self.hasher.update(&v.to_bits().to_le_bytes());
    }

    /// The first 8 bytes of the digest.
    pub fn finish(&self) -> u64 {
        let hash = self.hasher.finalize();
        let mut b = [0u8; 8];
        b.copy_from_slice(&hash.as_bytes()[..8]);
        u64::from_le_bytes(b)
    }
}

/// A value that travels in replicated components and net messages.
///
/// `decode` reads untrusted bytes: it must return an error, never panic. Implemented for the
/// field types of spec 71 § 3 and derived for structs and enums with `#[derive(Replicate)]`.
pub trait ReplicateCodec: Sized + Clone {
    /// Appends the full value.
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext);
    /// Reads a full value.
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError>;

    /// Whether two values travel as the same bits (a change smaller than a quantization step is
    /// no change, spec 71 § 4.3).
    fn wire_eq(&self, other: &Self, cx: &CodecContext) -> bool {
        let cx = cx.scratch();
        let mut a = BitWriter::new();
        self.encode(&mut a, &cx);
        let mut b = BitWriter::new();
        other.encode(&mut b, &cx);
        a.bit_len() == b.bit_len() && a.as_bytes() == b.as_bytes()
    }

    /// Appends what changed since `base`. The default sends the full value; the derive sends one
    /// change bit per field and only the changed fields.
    fn encode_delta(&self, base: &Self, w: &mut BitWriter, cx: &CodecContext) {
        let _ = base;
        self.encode(w, cx);
    }

    /// Reads a delta written by [`ReplicateCodec::encode_delta`] on top of `base`.
    fn decode_delta(base: &Self, r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let _ = base;
        Self::decode(r, cx)
    }

    /// Adds the codec's shape to the protocol hash.
    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
    }

    /// Copies the fields that never travel (`#[replicate(skip)]`) from the client's current copy
    /// `local` into this received value, so the client keeps its own value (spec 71 § 3). The
    /// default keeps nothing; the derive copies each skipped field of a struct.
    fn keep_local(&mut self, local: &Self) {
        let _ = local;
    }
}

/// How one field of a derived codec travels. `#[derive(Replicate)]` picks one per field from its
/// attributes ([`Plain`] without attributes).
pub trait FieldCodec<T> {
    /// Appends the field.
    fn encode(&self, value: &T, w: &mut BitWriter, cx: &CodecContext);
    /// Reads the field.
    fn decode(&self, r: &mut BitReader, cx: &CodecContext) -> Result<T, DecodeError>;
    /// Whether two field values travel as the same bits.
    fn eq(&self, a: &T, b: &T, cx: &CodecContext) -> bool;
    /// Adds the field codec's shape to the protocol hash.
    fn layout(&self, h: &mut LayoutHasher);
}

/// A field encoded by its type's own [`ReplicateCodec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Plain;

impl<T: ReplicateCodec> FieldCodec<T> for Plain {
    fn encode(&self, value: &T, w: &mut BitWriter, cx: &CodecContext) {
        value.encode(w, cx);
    }
    fn decode(&self, r: &mut BitReader, cx: &CodecContext) -> Result<T, DecodeError> {
        T::decode(r, cx)
    }
    fn eq(&self, a: &T, b: &T, cx: &CodecContext) -> bool {
        a.wire_eq(b, cx)
    }
    fn layout(&self, h: &mut LayoutHasher) {
        T::layout(h);
    }
}

/// `#[replicate(quantize(min = a, max = b, bits = n))]`: an `f32` as an `n`-bit fixed-point value
/// in `[min, max]`. Out-of-range values (and NaN) clamp and count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quantize {
    /// Lowest value.
    pub min: f32,
    /// Highest value.
    pub max: f32,
    /// Bits on the wire (1..=32).
    pub bits: u32,
}

impl Quantize {
    fn steps(&self) -> u64 {
        let bits = self.bits.clamp(1, 32);
        (1u64 << bits) - 1
    }

    /// The integer a value travels as, and whether it was clamped.
    pub fn quantize(&self, v: f32) -> (u64, bool) {
        let steps = self.steps();
        let span = self.max - self.min;
        if span.is_nan() || span <= 0.0 {
            return (0, true);
        }
        let (clamped_v, clamped) = if v.is_nan() || v < self.min {
            (self.min, true)
        } else if v > self.max {
            (self.max, true)
        } else {
            (v, false)
        };
        let scale = steps as f32 / span;
        let q = ((clamped_v - self.min) * scale).round();
        let q = if q <= 0.0 { 0 } else { (q as u64).min(steps) };
        (q, clamped)
    }

    /// The value an integer stands for.
    pub fn dequantize(&self, q: u64) -> f32 {
        let steps = self.steps();
        let span = self.max - self.min;
        let q = q.min(steps);
        self.min + q as f32 * (span / steps as f32)
    }
}

impl FieldCodec<f32> for Quantize {
    fn encode(&self, value: &f32, w: &mut BitWriter, cx: &CodecContext) {
        let (q, clamped) = self.quantize(*value);
        if clamped {
            cx.count_clamped();
        }
        w.write_bits(q, self.bits.clamp(1, 32));
    }
    fn decode(&self, r: &mut BitReader, _cx: &CodecContext) -> Result<f32, DecodeError> {
        Ok(self.dequantize(r.read_bits(self.bits.clamp(1, 32))?))
    }
    fn eq(&self, a: &f32, b: &f32, _cx: &CodecContext) -> bool {
        self.quantize(*a).0 == self.quantize(*b).0
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("quantize");
        h.write_f32(self.min);
        h.write_f32(self.max);
        h.write_u64(u64::from(self.bits));
    }
}

/// `#[replicate(angle(bits = n))]`: radians wrapped to `[-π, π)` in `n` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Angle {
    /// Bits on the wire (2..=32).
    pub bits: u32,
}

impl Angle {
    /// The integer an angle travels as.
    pub fn quantize(&self, v: f32) -> u64 {
        let bits = self.bits.clamp(2, 32);
        let count = 1u64 << bits;
        if !v.is_finite() {
            return 0;
        }
        // [-π, π) → [0, 1) of a turn, then n bits; the top value wraps to 0 (it is -π again).
        let turn = (v + PI).rem_euclid(TAU) / TAU;
        let q = (turn * count as f32).round() as u64;
        q % count
    }

    /// The angle an integer stands for, in `[-π, π)`.
    pub fn dequantize(&self, q: u64) -> f32 {
        let bits = self.bits.clamp(2, 32);
        let count = 1u64 << bits;
        (q % count) as f32 * (TAU / count as f32) - PI
    }
}

impl FieldCodec<f32> for Angle {
    fn encode(&self, value: &f32, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_bits(self.quantize(*value), self.bits.clamp(2, 32));
    }
    fn decode(&self, r: &mut BitReader, _cx: &CodecContext) -> Result<f32, DecodeError> {
        Ok(self.dequantize(r.read_bits(self.bits.clamp(2, 32))?))
    }
    fn eq(&self, a: &f32, b: &f32, _cx: &CodecContext) -> bool {
        self.quantize(*a) == self.quantize(*b)
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("angle");
        h.write_u64(u64::from(self.bits));
    }
}

/// `#[replicate(unit_vec(bits = n))]`: a unit `Vec3` as two `n`-bit octahedral coordinates
/// (Cigolle et al., JCGT 2014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitVec {
    /// Bits per coordinate (2..=32).
    pub bits: u32,
}

impl UnitVec {
    fn axis(&self) -> Quantize {
        Quantize {
            min: -1.0,
            max: 1.0,
            bits: self.bits.clamp(2, 32),
        }
    }

    /// The two integers a unit vector travels as.
    pub fn quantize(&self, v: Vec3) -> (u64, u64) {
        let l1 = v.x.abs() + v.y.abs() + v.z.abs();
        let p = if l1 > 0.0 && l1.is_finite() {
            Vec2::new(v.x / l1, v.y / l1)
        } else {
            Vec2::new(0.0, 0.0)
        };
        let p = if v.z < 0.0 {
            Vec2::new(
                (1.0 - p.y.abs()) * sign_not_zero(p.x),
                (1.0 - p.x.abs()) * sign_not_zero(p.y),
            )
        } else {
            p
        };
        let axis = self.axis();
        (axis.quantize(p.x).0, axis.quantize(p.y).0)
    }

    /// The unit vector two integers stand for.
    pub fn dequantize(&self, qx: u64, qy: u64) -> Vec3 {
        let axis = self.axis();
        let x = axis.dequantize(qx);
        let y = axis.dequantize(qy);
        let z = 1.0 - x.abs() - y.abs();
        let (x, y) = if z < 0.0 {
            (
                (1.0 - y.abs()) * sign_not_zero(x),
                (1.0 - x.abs()) * sign_not_zero(y),
            )
        } else {
            (x, y)
        };
        Vec3::new(x, y, z).normalize_or(Vec3::Z)
    }
}

fn sign_not_zero(v: f32) -> f32 {
    if v >= 0.0 { 1.0 } else { -1.0 }
}

impl FieldCodec<Vec3> for UnitVec {
    fn encode(&self, value: &Vec3, w: &mut BitWriter, _cx: &CodecContext) {
        let (x, y) = self.quantize(*value);
        let bits = self.bits.clamp(2, 32);
        w.write_bits(x, bits);
        w.write_bits(y, bits);
    }
    fn decode(&self, r: &mut BitReader, _cx: &CodecContext) -> Result<Vec3, DecodeError> {
        let bits = self.bits.clamp(2, 32);
        let x = r.read_bits(bits)?;
        let y = r.read_bits(bits)?;
        Ok(self.dequantize(x, y))
    }
    fn eq(&self, a: &Vec3, b: &Vec3, _cx: &CodecContext) -> bool {
        self.quantize(*a) == self.quantize(*b)
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("unit_vec");
        h.write_u64(u64::from(self.bits));
    }
}

/// `#[replicate(rotation(bits = n))]`: a `Quat` as smallest-three, `n` bits per kept component
/// plus 2 bits for the dropped index (G. Fiedler, "Snapshot Compression").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    /// Bits per kept component (2..=32, default 10).
    pub bits: u32,
}

impl Default for Rotation {
    fn default() -> Self {
        Self { bits: 10 }
    }
}

impl Rotation {
    fn axis(&self) -> Quantize {
        let r = std::f32::consts::FRAC_1_SQRT_2;
        Quantize {
            min: -r,
            max: r,
            bits: self.bits.clamp(2, 32),
        }
    }

    /// The dropped index and the three kept integers.
    pub fn quantize(&self, q: Quat) -> (u64, [u64; 3]) {
        let a = normalize4([q.x, q.y, q.z, q.w]).unwrap_or([0.0, 0.0, 0.0, 1.0]);
        let mut largest = 0usize;
        for (i, c) in a.iter().enumerate() {
            if c.abs() > a[largest].abs() {
                largest = i;
            }
        }
        let sign = if a[largest] < 0.0 { -1.0 } else { 1.0 };
        let axis = self.axis();
        let mut kept = [0u64; 3];
        let mut k = 0;
        for (i, c) in a.iter().enumerate() {
            if i != largest {
                kept[k] = axis.quantize(c * sign).0;
                k += 1;
            }
        }
        (largest as u64, kept)
    }

    /// The rotation the integers stand for.
    pub fn dequantize(&self, largest: u64, kept: [u64; 3]) -> Quat {
        let axis = self.axis();
        let small = kept.map(|q| axis.dequantize(q));
        let sum = small[0] * small[0] + small[1] * small[1] + small[2] * small[2];
        let big = (1.0 - sum).max(0.0).sqrt();
        let largest = (largest as usize).min(3);
        let mut out = [0.0f32; 4];
        let mut k = 0;
        for (i, slot) in out.iter_mut().enumerate() {
            if i == largest {
                *slot = big;
            } else {
                *slot = small[k];
                k += 1;
            }
        }
        let n = normalize4(out).unwrap_or([0.0, 0.0, 0.0, 1.0]);
        Quat::from_xyzw(n[0], n[1], n[2], n[3])
    }
}

/// Scalar normalization in a fixed operation order (no SIMD horizontal sums, no fused
/// multiply-add), so every platform rounds identically (spec 71 § 12). `None` for zero or
/// non-finite input.
pub(crate) use crate::scalar::normalize4;

impl FieldCodec<Quat> for Rotation {
    fn encode(&self, value: &Quat, w: &mut BitWriter, _cx: &CodecContext) {
        let (largest, kept) = self.quantize(*value);
        let bits = self.bits.clamp(2, 32);
        w.write_bits(largest, 2);
        for k in kept {
            w.write_bits(k, bits);
        }
    }
    fn decode(&self, r: &mut BitReader, _cx: &CodecContext) -> Result<Quat, DecodeError> {
        let bits = self.bits.clamp(2, 32);
        let largest = r.read_bits(2)?;
        let kept = [r.read_bits(bits)?, r.read_bits(bits)?, r.read_bits(bits)?];
        Ok(self.dequantize(largest, kept))
    }
    fn eq(&self, a: &Quat, b: &Quat, _cx: &CodecContext) -> bool {
        self.quantize(*a) == self.quantize(*b)
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("rotation");
        h.write_u64(u64::from(self.bits));
    }
}

/// `#[replicate(max_len = n)]`: the upper bound of a `String` (bytes) or `Vec` (elements) field.
/// Longer values are truncated on send and counted; longer input is a decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxLen(pub usize);

impl FieldCodec<String> for MaxLen {
    fn encode(&self, value: &String, w: &mut BitWriter, cx: &CodecContext) {
        encode_str(value, self.0, w, cx);
    }
    fn decode(&self, r: &mut BitReader, _cx: &CodecContext) -> Result<String, DecodeError> {
        decode_str(r, self.0)
    }
    fn eq(&self, a: &String, b: &String, _cx: &CodecContext) -> bool {
        truncate_str(a, self.0) == truncate_str(b, self.0)
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("string_max");
        h.write_u64(self.0 as u64);
    }
}

impl<T: ReplicateCodec> FieldCodec<Vec<T>> for MaxLen {
    fn encode(&self, value: &Vec<T>, w: &mut BitWriter, cx: &CodecContext) {
        encode_vec(value, self.0, w, cx);
    }
    fn decode(&self, r: &mut BitReader, cx: &CodecContext) -> Result<Vec<T>, DecodeError> {
        decode_vec(r, self.0, cx)
    }
    fn eq(&self, a: &Vec<T>, b: &Vec<T>, cx: &CodecContext) -> bool {
        let n = self.0;
        let (a, b) = (&a[..a.len().min(n)], &b[..b.len().min(n)]);
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.wire_eq(y, cx))
    }
    fn layout(&self, h: &mut LayoutHasher) {
        h.write_str("vec_max");
        h.write_u64(self.0 as u64);
        T::layout(h);
    }
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn encode_str(value: &str, max: usize, w: &mut BitWriter, cx: &CodecContext) {
    let s = truncate_str(value, max);
    if s.len() < value.len() {
        cx.count_clamped();
    }
    w.write_varint(s.len() as u64);
    w.write_bytes(s.as_bytes());
}

fn decode_str(r: &mut BitReader, max: usize) -> Result<String, DecodeError> {
    let len = r.read_len(max)?;
    let bytes = r.read_bytes(len)?;
    String::from_utf8(bytes).map_err(|_| DecodeError::Invalid("string is not UTF-8"))
}

fn encode_vec<T: ReplicateCodec>(value: &[T], max: usize, w: &mut BitWriter, cx: &CodecContext) {
    let n = value.len().min(max);
    if n < value.len() {
        cx.count_clamped();
    }
    w.write_varint(n as u64);
    for item in &value[..n] {
        item.encode(w, cx);
    }
}

fn decode_vec<T: ReplicateCodec>(
    r: &mut BitReader,
    max: usize,
    cx: &CodecContext,
) -> Result<Vec<T>, DecodeError> {
    let len = r.read_len(max)?;
    // Every element costs at least one bit: refuse a length the input cannot hold before
    // reserving memory for it.
    if len > r.remaining_bits() {
        return Err(DecodeError::Truncated {
            at: r.bit_pos(),
            needed: len,
        });
    }
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(T::decode(r, cx)?);
    }
    Ok(out)
}

// ---- impls for the field types of spec 71 § 3 ------------------------------------------------

impl ReplicateCodec for bool {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_bool(*self);
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        r.read_bool()
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self == other
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("bool");
    }
}

macro_rules! codec_unsigned {
    ($($t:ident),*) => {$(
        impl ReplicateCodec for $t {
            fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
                w.write_varint(*self as u64);
            }
            fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
                $t::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid(stringify!($t)))
            }
            fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
                self == other
            }
            fn layout(h: &mut LayoutHasher) {
                h.write_str(stringify!($t));
            }
        }
    )*};
}
codec_unsigned!(u8, u16, u32, u64, usize);

macro_rules! codec_signed {
    ($($t:ident),*) => {$(
        impl ReplicateCodec for $t {
            fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
                w.write_zigzag(*self as i64);
            }
            fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
                $t::try_from(r.read_zigzag()?).map_err(|_| DecodeError::Invalid(stringify!($t)))
            }
            fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
                self == other
            }
            fn layout(h: &mut LayoutHasher) {
                h.write_str(stringify!($t));
            }
        }
    )*};
}
codec_signed!(i8, i16, i32, i64, isize);

impl ReplicateCodec for f32 {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_f32(*self);
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        r.read_f32()
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self.to_bits() == other.to_bits()
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("f32");
    }
}

impl ReplicateCodec for f64 {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_f64(*self);
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        r.read_f64()
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self.to_bits() == other.to_bits()
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("f64");
    }
}

macro_rules! codec_floats {
    ($($t:ident [$($f:ident),*]),*) => {$(
        impl ReplicateCodec for $t {
            fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
                $(w.write_f32(self.$f);)*
            }
            fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
                Ok($t::from_array([$({ let _ = stringify!($f); r.read_f32()? }),*]))
            }
            fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
                true $(&& self.$f.to_bits() == other.$f.to_bits())*
            }
            fn layout(h: &mut LayoutHasher) {
                h.write_str(stringify!($t));
            }
        }
    )*};
}
codec_floats!(Vec2[x, y], Vec3[x, y, z], Vec4[x, y, z, w], Quat[x, y, z, w]);

impl ReplicateCodec for String {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        encode_str(self, DEFAULT_MAX_LEN, w, cx);
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        decode_str(r, DEFAULT_MAX_LEN)
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        truncate_str(self, DEFAULT_MAX_LEN) == truncate_str(other, DEFAULT_MAX_LEN)
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("string");
    }
}

impl<T: ReplicateCodec> ReplicateCodec for Vec<T> {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        encode_vec(self, DEFAULT_MAX_LEN, w, cx);
    }
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        decode_vec(r, DEFAULT_MAX_LEN, cx)
    }
    fn wire_eq(&self, other: &Self, cx: &CodecContext) -> bool {
        let a = &self[..self.len().min(DEFAULT_MAX_LEN)];
        let b = &other[..other.len().min(DEFAULT_MAX_LEN)];
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.wire_eq(y, cx))
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("vec");
        T::layout(h);
    }
}

impl<T: ReplicateCodec> ReplicateCodec for Option<T> {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        w.write_bool(self.is_some());
        if let Some(v) = self {
            v.encode(w, cx);
        }
    }
    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        if r.read_bool()? {
            Ok(Some(T::decode(r, cx)?))
        } else {
            Ok(None)
        }
    }
    fn wire_eq(&self, other: &Self, cx: &CodecContext) -> bool {
        match (self, other) {
            (None, None) => true,
            (Some(a), Some(b)) => a.wire_eq(b, cx),
            _ => false,
        }
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("option");
        T::layout(h);
    }
}

impl ReplicateCodec for NetId {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(u64::from(self.0));
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(NetId(
            u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("net id"))?,
        ))
    }
    fn wire_eq(&self, other: &Self, _cx: &CodecContext) -> bool {
        self == other
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("net_id");
    }
}

impl ReplicateCodec for () {
    fn encode(&self, _w: &mut BitWriter, _cx: &CodecContext) {}
    fn decode(_r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(())
    }
    fn wire_eq(&self, _other: &Self, _cx: &CodecContext) -> bool {
        true
    }
    fn layout(h: &mut LayoutHasher) {
        h.write_str("unit");
    }
}

/// Encodes a value into a fresh buffer (a convenience for messages and tests).
pub fn encode_to_vec<T: ReplicateCodec>(value: &T, cx: &CodecContext) -> Vec<u8> {
    let mut w = BitWriter::new();
    value.encode(&mut w, cx);
    w.finish()
}

/// Decodes a value from a buffer written by [`encode_to_vec`].
pub fn decode_from_slice<T: ReplicateCodec>(bytes: &[u8], cx: &CodecContext) -> Result<T, DecodeError> {
    let mut r = BitReader::new(bytes);
    T::decode(&mut r, cx)
}

/// The layout hash of one codec type.
pub fn layout_hash<T: ReplicateCodec>() -> u64 {
    let mut h = LayoutHasher::new();
    T::layout(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: ReplicateCodec + PartialEq + std::fmt::Debug>(v: T) {
        let cx = CodecContext::detached();
        let bytes = encode_to_vec(&v, &cx);
        let back: T = decode_from_slice(&bytes, &cx).expect("decode");
        assert_eq!(back, v);
    }

    #[test]
    fn primitives_round_trip() {
        round_trip(true);
        round_trip(200u8);
        round_trip(u64::MAX);
        round_trip(-12345i32);
        round_trip(i64::MIN);
        round_trip(3.25f32);
        round_trip(-1.0e300f64);
        round_trip(Vec3::new(1.0, -2.0, 3.5));
        round_trip(Quat::from_xyzw(0.0, 0.0, 0.0, 1.0));
        round_trip(Some(7u32));
        round_trip(None::<u32>);
        round_trip(String::from("grüße"));
        round_trip(vec![1u16, 2, 3]);
        round_trip(NetId(42));
    }

    #[test]
    fn container_wire_eq_matches_encoded_bytes() {
        let cx = CodecContext::detached();
        let same = |a: &dyn Fn() -> bool, b: bool| assert_eq!(a(), b);
        same(&|| String::from("a").wire_eq(&String::from("a"), &cx), true);
        same(&|| String::from("a").wire_eq(&String::from("b"), &cx), false);
        same(&|| vec![1u16, 2].wire_eq(&vec![1u16, 2], &cx), true);
        same(&|| vec![1u16, 2].wire_eq(&vec![1u16], &cx), false);
        same(&|| Some(3u8).wire_eq(&None, &cx), false);
        same(&|| Some(3u8).wire_eq(&Some(3), &cx), true);
        same(&|| Some(3u8).wire_eq(&Some(4), &cx), false);
        same(&|| None::<u8>.wire_eq(&None, &cx), true);
        // Over-long strings truncate to the same wire bytes, so they compare equal.
        let long_a = "x".repeat(DEFAULT_MAX_LEN + 5);
        let long_b = "x".repeat(DEFAULT_MAX_LEN + 9);
        assert!(long_a.wire_eq(&long_b, &cx));
    }

    #[test]
    fn quantize_clamps_counts_and_compares_by_step() {
        let q = Quantize {
            min: -10.0,
            max: 10.0,
            bits: 10,
        };
        let step = 20.0 / 1023.0;
        // Within half a step: same integer, so no change.
        assert!(FieldCodec::<f32>::eq(
            &q,
            &1.0,
            &(1.0 + step * 0.4),
            &CodecContext::detached()
        ));
        assert!(!FieldCodec::<f32>::eq(
            &q,
            &1.0,
            &(1.0 + step * 1.5),
            &CodecContext::detached()
        ));
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        FieldCodec::<f32>::encode(&q, &25.0, &mut w, &cx);
        FieldCodec::<f32>::encode(&q, &f32::NAN, &mut w, &cx);
        assert_eq!(cx.take_stats().clamped, 2);
        assert_eq!(w.bit_len(), 20);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(FieldCodec::<f32>::decode(&q, &mut r, &cx).expect("max"), 10.0);
        assert_eq!(FieldCodec::<f32>::decode(&q, &mut r, &cx).expect("min"), -10.0);
    }

    #[test]
    fn angle_wraps_into_half_open_range() {
        let a = Angle { bits: 12 };
        for v in [-PI, -1.0, 0.0, 1.0, PI - 0.001, 3.0 * PI, -7.0] {
            let back = a.dequantize(a.quantize(v));
            assert!((-PI..PI).contains(&back), "{v} -> {back}");
            let d = (back - v).rem_euclid(TAU);
            let d = d.min(TAU - d);
            assert!(d < TAU / 4096.0, "{v} -> {back}");
        }
    }

    #[test]
    fn unit_vectors_stay_unit_and_close() {
        let u = UnitVec { bits: 12 };
        for v in [
            Vec3::X,
            Vec3::NEG_Z,
            Vec3::new(1.0, -2.0, -3.0).normalize(),
            Vec3::new(-0.3, 0.2, 0.9).normalize(),
        ] {
            let (x, y) = u.quantize(v);
            let back = u.dequantize(x, y);
            assert!((back.length() - 1.0).abs() < 1e-4);
            assert!(back.dot(v) > 0.9999, "{v} -> {back}");
        }
    }

    #[test]
    fn smallest_three_rotation_is_close_and_sign_free() {
        let r = Rotation::default();
        for q in [
            Quat::IDENTITY,
            Quat::from_rotation_y(2.0),
            Quat::from_euler(glam::EulerRot::XYZ, 0.3, -1.2, 2.9),
            -Quat::from_rotation_x(0.5),
        ] {
            let (i, kept) = r.quantize(q);
            let back = r.dequantize(i, kept);
            assert!(back.dot(q).abs() > 0.9995, "{q} -> {back}");
        }
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        FieldCodec::<Quat>::encode(&r, &Quat::IDENTITY, &mut w, &cx);
        assert_eq!(w.bit_len(), 2 + 3 * 10);
    }

    #[test]
    fn max_len_truncates_on_send_and_refuses_on_read() {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        FieldCodec::<String>::encode(&MaxLen(4), &"héllo".to_string(), &mut w, &cx);
        assert_eq!(cx.take_stats().clamped, 1);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        let s = FieldCodec::<String>::decode(&MaxLen(4), &mut r, &cx).expect("short");
        assert_eq!(s, "hél");
        let long = encode_to_vec(&"abcdef".to_string(), &cx);
        let mut r = BitReader::new(&long);
        assert!(FieldCodec::<String>::decode(&MaxLen(4), &mut r, &cx).is_err());
        // A hostile element count larger than the input is refused before allocating.
        let mut w = BitWriter::new();
        w.write_varint(1000);
        let bytes = w.finish();
        assert!(decode_from_slice::<Vec<u64>>(&bytes, &cx).is_err());
    }
    #[test]
    fn layout_hash_separates_codecs() {
        assert_ne!(layout_hash::<u32>(), layout_hash::<i32>());
        assert_ne!(layout_hash::<Vec<u8>>(), layout_hash::<Option<u8>>());
        let mut a = LayoutHasher::new();
        Quantize {
            min: 0.0,
            max: 1.0,
            bits: 8,
        }
        .layout(&mut a);
        let mut b = LayoutHasher::new();
        Quantize {
            min: 0.0,
            max: 1.0,
            bits: 9,
        }
        .layout(&mut b);
        assert_ne!(a.finish(), b.finish());
    }
}
