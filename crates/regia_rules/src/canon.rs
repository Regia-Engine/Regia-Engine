// SPDX-License-Identifier: MPL-2.0
//! Canonical bytes of a serde value, for content hashes every peer computes the same way (ADR-8694).
//!
//! The encoding walks the serde data model: every value starts with a one-byte tag, integers are
//! little-endian at 64 (or 128) bits, an `f32` is its bit pattern (never widened, so the bits the
//! loader parsed are the bits hashed), strings are length-prefixed UTF-8, and sequences, maps and
//! structs end with an end tag. Struct field names and enum variant names are written; Rust type
//! names are not, so renaming a type does not move a hash, while renaming a serialized field does
//! (it is a format change anyway). Maps are written in their iteration order, so callers hash
//! `BTreeMap`s and sorted `Vec`s only.

use serde::ser::{self, Serialize};

const BOOL: u8 = 0x01;
const INT: u8 = 0x02;
const UINT: u8 = 0x03;
const INT128: u8 = 0x04;
const UINT128: u8 = 0x05;
const F32: u8 = 0x06;
const F64: u8 = 0x07;
const CHAR: u8 = 0x08;
const STR: u8 = 0x09;
const BYTES: u8 = 0x0a;
const NONE: u8 = 0x0b;
const SOME: u8 = 0x0c;
const UNIT: u8 = 0x0d;
const UNIT_VARIANT: u8 = 0x0f;
const NEWTYPE_VARIANT: u8 = 0x11;
const SEQ: u8 = 0x12;
const END: u8 = 0x13;
const TUPLE_VARIANT: u8 = 0x15;
const MAP: u8 = 0x16;
const STRUCT: u8 = 0x17;
const STRUCT_VARIANT: u8 = 0x18;

/// The canonical bytes of `value`.
///
/// # Errors
///
/// A message when a `Serialize` impl reports its own error.
pub fn to_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    let mut canon = Canon { out: Vec::new() };
    value.serialize(&mut canon).map_err(|e| e.0)?;
    Ok(canon.out)
}

/// FNV-1a, 64 bit, over `bytes` (the public-domain Fowler-Noll-Vo hash).
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A serializer error: the message a `Serialize` impl gave.
#[derive(Debug)]
pub struct CanonError(String);

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CanonError {}

impl ser::Error for CanonError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

struct Canon {
    out: Vec<u8>,
}

impl Canon {
    fn tag(&mut self, tag: u8) {
        self.out.push(tag);
    }

    fn len(&mut self, len: usize) {
        self.out
            .extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
    }

    fn text(&mut self, text: &str) {
        self.len(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }
}

type Res = Result<(), CanonError>;

impl ser::Serializer for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, v: bool) -> Res {
        self.tag(BOOL);
        self.out.push(u8::from(v));
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Res {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i16(self, v: i16) -> Res {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i32(self, v: i32) -> Res {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i64(self, v: i64) -> Res {
        self.tag(INT);
        self.out.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Res {
        self.tag(INT128);
        self.out.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Res {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u16(self, v: u16) -> Res {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u32(self, v: u32) -> Res {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u64(self, v: u64) -> Res {
        self.tag(UINT);
        self.out.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Res {
        self.tag(UINT128);
        self.out.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Res {
        self.tag(F32);
        self.out.extend_from_slice(&v.to_bits().to_le_bytes());
        Ok(())
    }
    fn serialize_f64(self, v: f64) -> Res {
        self.tag(F64);
        self.out.extend_from_slice(&v.to_bits().to_le_bytes());
        Ok(())
    }
    fn serialize_char(self, v: char) -> Res {
        self.tag(CHAR);
        self.out.extend_from_slice(&u32::from(v).to_le_bytes());
        Ok(())
    }
    fn serialize_str(self, v: &str) -> Res {
        self.tag(STR);
        self.text(v);
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Res {
        self.tag(BYTES);
        self.len(v.len());
        self.out.extend_from_slice(v);
        Ok(())
    }
    fn serialize_none(self) -> Res {
        self.tag(NONE);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Res {
        self.tag(SOME);
        value.serialize(self)
    }
    fn serialize_unit(self) -> Res {
        self.tag(UNIT);
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Res {
        self.serialize_unit()
    }
    fn serialize_unit_variant(self, _name: &'static str, _index: u32, variant: &'static str) -> Res {
        self.tag(UNIT_VARIANT);
        self.text(variant);
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _name: &'static str, value: &T) -> Res {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Res {
        self.tag(NEWTYPE_VARIANT);
        self.text(variant);
        value.serialize(self)
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self, CanonError> {
        self.tag(SEQ);
        Ok(self)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self, CanonError> {
        self.tag(SEQ);
        Ok(self)
    }
    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Self, CanonError> {
        self.tag(SEQ);
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self, CanonError> {
        self.tag(TUPLE_VARIANT);
        self.text(variant);
        Ok(self)
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self, CanonError> {
        self.tag(MAP);
        Ok(self)
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self, CanonError> {
        self.tag(STRUCT);
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self, CanonError> {
        self.tag(STRUCT_VARIANT);
        self.text(variant);
        Ok(self)
    }
}

impl ser::SerializeSeq for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Res {
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeTuple for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Res {
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Res {
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Res {
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeMap for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Res {
        key.serialize(&mut **self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Res {
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Res {
        self.text(key);
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Canon {
    type Ok = ();
    type Error = CanonError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Res {
        self.text(key);
        value.serialize(&mut **self)
    }
    fn end(self) -> Res {
        self.tag(END);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct Pair {
        a: f32,
        b: Option<u8>,
    }

    #[test]
    fn f32_bits_are_hashed_unwidened_and_fields_are_named() {
        let one = to_bytes(&Pair { a: 0.1, b: None }).expect("bytes");
        let mut expected = vec![STRUCT];
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.push(b'a');
        expected.push(F32);
        expected.extend_from_slice(&0.1f32.to_bits().to_le_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.push(b'b');
        expected.push(NONE);
        expected.push(END);
        assert_eq!(one, expected);
        let two = to_bytes(&Pair { a: 0.1, b: Some(0) }).expect("bytes");
        assert_ne!(fnv1a64(&one), fnv1a64(&two));
        // -0.0 and 0.0 are different bits, so they hash differently.
        assert_ne!(
            to_bytes(&Pair { a: -0.0, b: None }).expect("bytes"),
            to_bytes(&Pair { a: 0.0, b: None }).expect("bytes")
        );
    }

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
