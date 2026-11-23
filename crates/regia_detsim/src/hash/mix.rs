// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/desync_hash.rs lines 345-618, commit d6ed37677,
// tag regia-absorb-1), ADR-8012. Mark: adapt (ADR-8047), for one named issue: the float words go through
// the canonical float rule below (-0.0 and every NaN fold to one pattern), so two peers whose states
// differ only in the sign of a zero or a NaN payload do not report a false split. Everything else (the
// word mix, the finish, the serializer's encoding of every serde shape) is DO27's, so a DO27 state with
// no -0.0 and no NaN hashes to the same words. Added: `Mix::f64`, `Clone`/`Debug`, docs. Since ADR-8078
// the canonical float rule and its two use sites are DO27's own text from commit 7a751bac9 (NET_DESYNC2),
// so every word, -0.0 and NaN included, equals DO27's at that commit. Added by ADR-8547 (Regia's own code):
// `FloatRule` (an opt-in raw-bits rule for the diagnose ring; the canonical words do not change) and the
// count of failed `Serialize` calls (`Mix::errors`); `Mix` became a named-field struct for them.

//! `Mix`: the streaming hash behind every system word, and the canonical float rule.

use serde::Serialize;

/// The one `f32` NaN pattern every NaN folds to (the quiet NaN with a zero payload, sign clear).
pub const CANONICAL_NAN_F32: u32 = 0x7fc0_0000;

/// The one `f64` NaN pattern every NaN folds to (the quiet NaN with a zero payload, sign clear).
pub const CANONICAL_NAN_F64: u64 = 0x7ff8_0000_0000_0000;

// Portions copied from Direct Order 27 (ir-sim/src/desync_hash.rs lines 345-363, commit
// 7a751bac91aa36cb0a3e3df718fbce1c9ba52ff8), ADR-8012. Mark: as-is (ADR-8078): the two-line doc and both
// bodies are DO27's text; added `pub const` (Regia's public API since F-04), `#[inline]` and the doc lines
// below DO27's.

/// Float identity for lockstep checksums and subsystem diagnostics. Preserve every
/// finite value (including subnormals) and infinity; ignore zero signs and NaN payloads.
///
/// The bits [`Mix`] folds for an `f32`: `-0.0` becomes `+0.0`, every NaN becomes
/// [`CANONICAL_NAN_F32`], every other value keeps its IEEE bits. IEEE arithmetic may produce either zero
/// sign, and any NaN payload, for results that compare equal or are equally meaningless; two peers that
/// agree on the value must not report a split over the bits. The tests are integer tests on the bits, so
/// no host float mode can change the answer (ADR-8078).
///
/// ```
/// use regia_detsim::hash::{canonical_f32_bits, CANONICAL_NAN_F32};
/// assert_eq!(canonical_f32_bits(-0.0), 0);
/// assert_eq!(canonical_f32_bits(f32::from_bits(0xffc0_0001)), CANONICAL_NAN_F32);
/// assert_eq!(canonical_f32_bits(1.5), 1.5f32.to_bits());
/// ```
#[inline]
pub const fn canonical_f32_bits(value: f32) -> u32 {
    let bits = value.to_bits();
    match bits & 0x7fff_ffff {
        0 => 0,
        magnitude if magnitude > 0x7f80_0000 => 0x7fc0_0000,
        _ => bits,
    }
}

/// The bits [`Mix`] folds for an `f64`: the same rule as [`canonical_f32_bits`], with
/// [`CANONICAL_NAN_F64`].
#[inline]
pub const fn canonical_f64_bits(value: f64) -> u64 {
    let bits = value.to_bits();
    match bits & 0x7fff_ffff_ffff_ffff {
        0 => 0,
        magnitude if magnitude > 0x7ff0_0000_0000_0000 => 0x7ff8_0000_0000_0000,
        _ => bits,
    }
}

/// Which bits [`Mix`] folds for a float.
///
/// [`FloatRule::Canonical`] is the lockstep rule (the wire world hash and every system word; DO27's
/// NET_DESYNC2 rule, unchanged until a Stage B ADR): `-0.0` and every NaN fold to one pattern.
/// [`FloatRule::Raw`] folds the IEEE bits as they are, so a zero-sign or NaN-payload split is visible.
/// It is a diagnosis rule only: a [`SystemRing`](super::SystemRing) built with
/// [`SystemRing::with_raw`](super::SystemRing::with_raw) keeps raw words beside the canonical ones, so
/// the probe can name the tick a zero sign first split, before the canonical words split (a later
/// `1/x` turns the zero into opposite infinities), or when they never do. A state with no `-0.0` and
/// no NaN folds to the same words under both rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FloatRule {
    /// [`canonical_f32_bits`] / [`canonical_f64_bits`] (the default; the wire rule).
    #[default]
    Canonical,
    /// The IEEE bits as they are (`to_bits`); diagnosis only, never on the wire.
    Raw,
}

/// Streaming hash. One multiply per word; the finish mixes the high bits down.
///
/// `Mix` is also a [`serde::Serializer`] (through `&mut Mix`): [`Mix::fold`] walks any
/// `Serialize` value and folds its field values (never field or type names) with no allocation.
/// Integers fold as one `u64` word (signed values sign-extended, 128-bit values as two words), floats
/// by their canonical bits (or their raw bits under [`FloatRule::Raw`]), strings and byte slices as
/// their length then 8-byte little-endian words, `None` as 0 and `Some` as 1 then the value, enum
/// variants by index, sequences and maps by their length then their items. Every word is an integer,
/// so the result does not depend on the host's byte order.
///
/// A `Serialize` impl that fails (a `serde::ser::Error::custom` from user code) stops its value's
/// walk part way: the word then covers part of the state. [`Mix::errors`] counts those failures, so a
/// caller can tell a partial word from a whole one.
///
/// Quality (measured by `mixer_has_no_collisions_over_seeded_single_bit_flips`): two streams of the
/// same length that differ in one word never fold to the same final word (every step is a bijection
/// of the state). Avalanche is within the sampling bound for a flip followed by at least one more
/// word; a flip in the top byte of the *last* word reaches only part of the output bits. A desync is
/// still detected (no collision); only the spread of the difference is narrower.
///
/// ```
/// use regia_detsim::hash::Mix;
///
/// let mut a = Mix::new(1);
/// a.fold(&(3u32, [1.0f32, 2.0], "crate"));
/// let mut b = Mix::new(1);
/// b.fold(&(3u32, [1.0f32, 2.0], "crate"));
/// assert_eq!(b.errors(), 0);
/// assert_eq!(a.finish(), b.finish());
/// ```
#[derive(Clone, Debug)]
pub struct Mix {
    state: u64,
    rule: FloatRule,
    errors: u32,
}

impl Mix {
    /// A new mixer with the canonical float rule. The tag separates streams (a system's index plus
    /// one, for the system vector).
    pub fn new(tag: u64) -> Self {
        Self::with_rule(tag, FloatRule::Canonical)
    }

    /// A new mixer that folds floats by their raw IEEE bits ([`FloatRule::Raw`], diagnosis only).
    /// The same tag and the same non-float words give the same state as [`Mix::new`].
    ///
    /// ```
    /// use regia_detsim::hash::Mix;
    /// let (mut canonical, mut raw) = (Mix::new(1), Mix::new_raw(1));
    /// canonical.f32(-0.0);
    /// raw.f32(-0.0);
    /// let (mut plus, mut raw_plus) = (Mix::new(1), Mix::new_raw(1));
    /// plus.f32(0.0);
    /// raw_plus.f32(0.0);
    /// assert_eq!(canonical.finish(), plus.finish(), "the wire rule hides the zero sign");
    /// assert_ne!(raw.finish(), raw_plus.finish(), "the raw rule shows it");
    /// ```
    pub fn new_raw(tag: u64) -> Self {
        Self::with_rule(tag, FloatRule::Raw)
    }

    /// A new mixer with float rule `rule`.
    pub fn with_rule(tag: u64, rule: FloatRule) -> Self {
        Self {
            state: 0x9e37_79b9_7f4a_7c15 ^ tag.wrapping_mul(0xff51_afd7_ed55_8ccd),
            rule,
            errors: 0,
        }
    }

    /// The float rule this mixer folds with.
    pub fn rule(&self) -> FloatRule {
        self.rule
    }

    /// `Serialize` calls that failed inside [`Mix::fold`] since this mixer was made (saturating).
    /// Non-zero means the word covers only part of the folded values.
    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Count `n` failed `Serialize` calls that happened in a sub-mixer whose word this mixer folds
    /// ([`Mix::errors`], saturating), so the partial flag of the outer word stays set. Folds nothing.
    ///
    /// ```
    /// use regia_detsim::hash::Mix;
    /// let mut row = Mix::new(0);
    /// row.fold(&3u32);
    /// let (errors, word) = (row.errors(), row.finish());
    /// let mut outer = Mix::new(1);
    /// outer.u64(word);
    /// outer.note_errors(errors);
    /// assert_eq!(outer.errors(), 0);
    /// ```
    pub fn note_errors(&mut self, n: u32) {
        self.errors = self.errors.saturating_add(n);
    }

    /// Fold one word.
    #[inline]
    pub fn u64(&mut self, value: u64) {
        self.state = (self.state ^ value).wrapping_mul(0xff51_afd7_ed55_8ccd);
        self.state ^= self.state >> 29;
    }

    /// Fold an `f32` by its canonical bits ([`canonical_f32_bits`]), or its raw bits under
    /// [`FloatRule::Raw`].
    #[inline]
    pub fn f32(&mut self, value: f32) {
        let bits = match self.rule {
            FloatRule::Canonical => canonical_f32_bits(value),
            FloatRule::Raw => value.to_bits(),
        };
        self.u64(u64::from(bits));
    }

    /// Fold an `f64` by its canonical bits ([`canonical_f64_bits`]), or its raw bits under
    /// [`FloatRule::Raw`].
    #[inline]
    pub fn f64(&mut self, value: f64) {
        let bits = match self.rule {
            FloatRule::Canonical => canonical_f64_bits(value),
            FloatRule::Raw => value.to_bits(),
        };
        self.u64(bits);
    }

    /// Fold a byte string: its length, then 8-byte little-endian words (the last one zero-padded).
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.u64(u64::from_le_bytes(word));
        }
    }

    /// Fold any serializable value: field values only, in order, with no field names, type names or
    /// field counts (a unit field and a skipped field write nothing; ADR-8621). A failing `Serialize`
    /// stops the value's walk where it failed and counts one error ([`Mix::errors`]).
    pub fn fold<T: Serialize + ?Sized>(&mut self, value: &T) {
        if value.serialize(&mut *self).is_err() {
            self.errors = self.errors.saturating_add(1);
        }
    }

    /// The final word.
    pub fn finish(self) -> u64 {
        let mut z = self.state;
        z ^= z >> 32;
        z = z.wrapping_mul(0xd6e8_feb8_6659_fd93);
        z ^ (z >> 32)
    }
}

/// The serializer's error type. `Mix` itself never fails; a `Serialize` impl may still return
/// `serde::ser::Error::custom`, which [`Mix::fold`] counts ([`Mix::errors`]).
#[derive(Debug)]
pub struct NeverFails;

impl std::fmt::Display for NeverFails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hash fold failed")
    }
}

impl std::error::Error for NeverFails {}

impl serde::ser::Error for NeverFails {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        NeverFails
    }
}

impl<'a> serde::Serializer for &'a mut Mix {
    type Ok = ();
    type Error = NeverFails;
    type SerializeSeq = &'a mut Mix;
    type SerializeTuple = &'a mut Mix;
    type SerializeTupleStruct = &'a mut Mix;
    type SerializeTupleVariant = &'a mut Mix;
    type SerializeMap = &'a mut Mix;
    type SerializeStruct = &'a mut Mix;
    type SerializeStructVariant = &'a mut Mix;

    fn serialize_bool(self, v: bool) -> Result<(), NeverFails> {
        self.u64(u64::from(v));
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<(), NeverFails> {
        self.u64(v as u64);
        Ok(())
    }
    fn serialize_i16(self, v: i16) -> Result<(), NeverFails> {
        self.u64(v as u64);
        Ok(())
    }
    fn serialize_i32(self, v: i32) -> Result<(), NeverFails> {
        self.u64(v as u64);
        Ok(())
    }
    fn serialize_i64(self, v: i64) -> Result<(), NeverFails> {
        self.u64(v as u64);
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<(), NeverFails> {
        self.u64(v as u64);
        self.u64((v >> 64) as u64);
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<(), NeverFails> {
        self.u64(u64::from(v));
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<(), NeverFails> {
        self.u64(u64::from(v));
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<(), NeverFails> {
        self.u64(u64::from(v));
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<(), NeverFails> {
        self.u64(v);
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<(), NeverFails> {
        self.u64(v as u64);
        self.u64((v >> 64) as u64);
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Result<(), NeverFails> {
        self.f32(v);
        Ok(())
    }
    fn serialize_f64(self, v: f64) -> Result<(), NeverFails> {
        self.f64(v);
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<(), NeverFails> {
        self.u64(u64::from(v as u32));
        Ok(())
    }
    fn serialize_str(self, v: &str) -> Result<(), NeverFails> {
        self.bytes(v.as_bytes());
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), NeverFails> {
        self.bytes(v);
        Ok(())
    }
    fn serialize_none(self) -> Result<(), NeverFails> {
        self.u64(0);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), NeverFails> {
        self.u64(1);
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), NeverFails> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), NeverFails> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, index: u32, _: &'static str) -> Result<(), NeverFails> {
        self.u64(u64::from(index));
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), NeverFails> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        value: &T,
    ) -> Result<(), NeverFails> {
        self.u64(u64::from(index));
        value.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<&'a mut Mix, NeverFails> {
        self.u64(len.map_or(u64::MAX, |n| n as u64));
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<&'a mut Mix, NeverFails> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<&'a mut Mix, NeverFails> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<&'a mut Mix, NeverFails> {
        self.u64(u64::from(index));
        Ok(self)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<&'a mut Mix, NeverFails> {
        self.u64(len.map_or(u64::MAX, |n| n as u64));
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<&'a mut Mix, NeverFails> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<&'a mut Mix, NeverFails> {
        self.u64(u64::from(index));
        Ok(self)
    }
    fn is_human_readable(&self) -> bool {
        false
    }
}

impl serde::ser::SerializeSeq for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeTuple for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeTupleStruct for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeTupleVariant for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeMap for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), NeverFails> {
        key.serialize(&mut **self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeStruct for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}

impl serde::ser::SerializeStructVariant for &mut Mix {
    type Ok = ();
    type Error = NeverFails;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _: &'static str,
        value: &T,
    ) -> Result<(), NeverFails> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), NeverFails> {
        Ok(())
    }
}
