// SPDX-License-Identifier: MPL-2.0
//! The form's value tree with non-finite numbers kept (rev-claude-59 inspector-forms finding 4).
//!
//! `serde_json::to_value` turns a non-finite `f32`/`f64` (`NaN`, `inf`, `-inf` in hand-written RON)
//! into `null`, so the form showed "none" and every typed edit then failed, because the tree no
//! longer fit the format. [`to_tree`] writes the serde form like `serde_json::to_value` but writes a
//! non-finite number as `{ "non_finite": "NaN" | "inf" | "-inf" }` (RON's spellings), and
//! [`from_tree`] reads that record back where the format takes a number, so an edit of another
//! field keeps the value and a number typed over it corrects it. Everything else is read and
//! written exactly as `serde_json` does.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde::de::{self, IntoDeserializer, Visitor};
use serde::ser::{self, Serialize};
use serde_json::{Map, Number, Value};

/// The key of the record that stands for a non-finite number.
pub const NON_FINITE: &str = "non_finite";

type Error = serde_json::Error;

/// RON's spelling of a non-finite number.
fn spelling(value: f64) -> &'static str {
    if value.is_nan() {
        "NaN"
    } else if value > 0.0 {
        "inf"
    } else {
        "-inf"
    }
}

/// The number a `{ "non_finite": spelling }` record stands for, when `value` is one.
#[must_use]
pub fn non_finite(value: &Value) -> Option<f64> {
    let fields = value.as_object()?;
    if fields.len() != 1 {
        return None;
    }
    match fields.get(NON_FINITE)?.as_str()? {
        "NaN" => Some(f64::NAN),
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

fn number(value: f64) -> Value {
    match Number::from_f64(value) {
        Some(n) => Value::Number(n),
        None => {
            let mut record = Map::new();
            record.insert(NON_FINITE.to_owned(), Value::String(spelling(value).to_owned()));
            Value::Object(record)
        }
    }
}

/// `value`'s serde form as a value tree, non-finite numbers as `{ "non_finite": spelling }`.
///
/// # Errors
///
/// A map key that is not a string, number or boolean (the formats have none).
pub fn to_tree<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    value.serialize(Tree)
}

/// Reads a `T` from a tree [`to_tree`] wrote (or an edited copy).
///
/// # Errors
///
/// The tree does not fit `T` (the message names what did not fit).
pub fn from_tree<T: de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    T::deserialize(Lenient::plain(value))
}

// ------------------------------------------------------------------------------------- writing

struct Tree;

/// A sequence, tuple or tuple variant being written.
struct SeqTree {
    items: Vec<Value>,
    variant: Option<&'static str>,
}

/// A map, struct or struct variant being written.
struct MapTree {
    map: Map<String, Value>,
    key: Option<String>,
    variant: Option<&'static str>,
}

fn tagged(variant: Option<&'static str>, value: Value) -> Value {
    match variant {
        Some(name) => {
            let mut record = Map::new();
            record.insert(name.to_owned(), value);
            Value::Object(record)
        }
        None => value,
    }
}

impl ser::Serializer for Tree {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = SeqTree;
    type SerializeTuple = SeqTree;
    type SerializeTupleStruct = SeqTree;
    type SerializeTupleVariant = SeqTree;
    type SerializeMap = MapTree;
    type SerializeStruct = MapTree;
    type SerializeStructVariant = MapTree;

    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        Ok(Value::from(v))
    }
    fn serialize_f32(self, v: f32) -> Result<Value, Error> {
        Ok(number(f64::from(v)))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, Error> {
        Ok(number(v))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(Value::String(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Array(v.iter().map(|b| Value::from(*b)).collect()))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Value, Error> {
        Ok(Value::String(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, Error> {
        Ok(tagged(Some(variant), value.serialize(Tree)?))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqTree, Error> {
        Ok(SeqTree {
            items: Vec::with_capacity(len.unwrap_or(0)),
            variant: None,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqTree, Error> {
        ser::Serializer::serialize_seq(self, Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqTree, Error> {
        ser::Serializer::serialize_seq(self, Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SeqTree, Error> {
        Ok(SeqTree {
            items: Vec::with_capacity(len),
            variant: Some(variant),
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<MapTree, Error> {
        Ok(MapTree {
            map: Map::new(),
            key: None,
            variant: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<MapTree, Error> {
        ser::Serializer::serialize_map(self, Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<MapTree, Error> {
        Ok(MapTree {
            map: Map::new(),
            key: None,
            variant: Some(variant),
        })
    }
}

impl SeqTree {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.items.push(value.serialize(Tree)?);
        Ok(())
    }
    fn done(self) -> Value {
        tagged(self.variant, Value::Array(self.items))
    }
}

impl ser::SerializeSeq for SeqTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

impl ser::SerializeTuple for SeqTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

impl ser::SerializeTupleStruct for SeqTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

impl ser::SerializeTupleVariant for SeqTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push(value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

/// A map key as `serde_json` writes one: strings as they are, numbers and booleans as text.
fn key_text(key: Value) -> Result<String, Error> {
    match key {
        Value::String(text) => Ok(text),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ser::Error::custom(
            "a map key must be a string, number or boolean",
        )),
    }
}

impl MapTree {
    fn insert<T: Serialize + ?Sized>(&mut self, key: String, value: &T) -> Result<(), Error> {
        self.map.insert(key, value.serialize(Tree)?);
        Ok(())
    }
    fn done(self) -> Value {
        tagged(self.variant, Value::Object(self.map))
    }
}

impl ser::SerializeMap for MapTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(key_text(key.serialize(Tree)?)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("a map value came before its key"))?;
        self.insert(key, value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

impl ser::SerializeStruct for MapTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        self.insert(key.to_owned(), value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

impl ser::SerializeStructVariant for MapTree {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        self.insert(key.to_owned(), value)
    }
    fn end(self) -> Result<Value, Error> {
        Ok(self.done())
    }
}

// ------------------------------------------------------------------------------------- reading

/// What a traced read ([`filled`], [`enum_variants`]) learns: the enum variants each path takes, and the
/// fields a filled-in record must be given (the ones with no serde default).
#[derive(Default)]
struct Ctx {
    /// Every path read as an enum, with the enum's variants.
    enums: RefCell<BTreeMap<String, &'static [&'static str]>>,
    /// Per filled-in record path, the fields to give it (learned from `missing field` refusals).
    required: RefCell<BTreeMap<String, Vec<&'static str>>>,
    /// The innermost filled-in record that refused for a missing field, and the field.
    missing: RefCell<Option<(String, &'static str)>>,
}

/// A tree being read: containers are walked here so every nested number can be a
/// `{ "non_finite": spelling }` record; leaves are read by `serde_json`. A traced read (`ctx`) also
/// tracks the form path, records enums, and fills in a [`FILL`] record from the type being read.
struct Lenient {
    value: Value,
    path: String,
    ctx: Option<Rc<Ctx>>,
}

impl<'de> IntoDeserializer<'de, Error> for Lenient {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}

/// `path.key` (`key` at the top).
fn key_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

impl Lenient {
    fn plain(value: Value) -> Self {
        Self {
            value,
            path: String::new(),
            ctx: None,
        }
    }

    fn child(&self, path: impl FnOnce(&str) -> String, value: Value) -> Self {
        Self {
            value,
            path: if self.ctx.is_some() {
                path(&self.path)
            } else {
                String::new()
            },
            ctx: self.ctx.clone(),
        }
    }

    /// The value filled in for a [`FILL`] record of a traced read.
    fn fill(&self) -> Option<Synth> {
        let ctx = self.ctx.as_ref()?;
        let variant = fill_marker(&self.value)?;
        Some(Synth {
            ctx: Rc::clone(ctx),
            path: self.path.clone(),
            variant,
        })
    }

    fn float<'de, V: Visitor<'de>>(self, visitor: V, as_f32: bool) -> Result<V::Value, Error> {
        if let Some(synth) = self.fill() {
            return de::Deserializer::deserialize_f64(synth, visitor);
        }
        match non_finite(&self.value) {
            Some(value) if as_f32 => visitor.visit_f32(value as f32),
            Some(value) => visitor.visit_f64(value),
            None => de::Deserializer::deserialize_any(self, visitor),
        }
    }
}

/// Forwards each method to the filled-in value for a [`FILL`] record, else to `deserialize_any`.
macro_rules! fill_or_any {
    ($($method:ident)*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self.fill() {
                Some(synth) => de::Deserializer::$method(synth, visitor),
                None => de::Deserializer::deserialize_any(self, visitor),
            }
        }
    )*};
}

impl<'de> de::Deserializer<'de> for Lenient {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some(synth) = self.fill() {
            return de::Deserializer::deserialize_any(synth, visitor);
        }
        let parent = Self {
            value: Value::Null,
            path: self.path,
            ctx: self.ctx,
        };
        match self.value {
            Value::Array(items) => {
                let items = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| parent.child(|p| format!("{p}[{i}]"), v));
                let mut seq = de::value::SeqDeserializer::<_, Error>::new(items);
                let out = visitor.visit_seq(&mut seq)?;
                seq.end()?;
                Ok(out)
            }
            Value::Object(fields) => {
                let fields = fields.into_iter().map(|(k, v)| {
                    let child = parent.child(|p| key_path(p, &k), v);
                    (k, child)
                });
                let mut map = de::value::MapDeserializer::<_, Error>::new(fields);
                let out = visitor.visit_map(&mut map)?;
                map.end()?;
                Ok(out)
            }
            leaf => de::Deserializer::deserialize_any(leaf, visitor),
        }
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.float(visitor, true)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.float(visitor, false)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some(synth) = self.fill() {
            return de::Deserializer::deserialize_option(synth, visitor);
        }
        match self.value {
            Value::Null => visitor.visit_none(),
            value => visitor.visit_some(Self {
                value,
                path: self.path,
                ctx: self.ctx,
            }),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some(synth) = self.fill() {
            return de::Deserializer::deserialize_unit(synth, visitor);
        }
        match self.value {
            Value::Null => visitor.visit_unit(),
            value => de::Deserializer::deserialize_unit(value, visitor),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_unit(self, visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        match self.fill() {
            Some(synth) => de::Deserializer::deserialize_tuple(synth, len, visitor),
            None => de::Deserializer::deserialize_any(self, visitor),
        }
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.fill() {
            Some(synth) => de::Deserializer::deserialize_tuple_struct(synth, name, len, visitor),
            None => de::Deserializer::deserialize_any(self, visitor),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.fill() {
            Some(synth) => de::Deserializer::deserialize_struct(synth, name, fields, visitor),
            None => de::Deserializer::deserialize_any(self, visitor),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if let Some(synth) = self.fill() {
            return de::Deserializer::deserialize_enum(synth, name, variants, visitor);
        }
        if let Some(ctx) = &self.ctx {
            ctx.enums.borrow_mut().insert(self.path.clone(), variants);
        }
        match self.value {
            Value::Object(fields) if fields.len() == 1 => {
                let Some((variant, value)) = fields.into_iter().next() else {
                    return Err(de::Error::custom("an enum record needs one key"));
                };
                let value = Self {
                    path: if self.ctx.is_some() {
                        key_path(&self.path, &variant)
                    } else {
                        String::new()
                    },
                    value,
                    ctx: self.ctx,
                };
                visitor.visit_enum(Variant { variant, value })
            }
            // A unit variant (a string) and every refusal read as `serde_json` reads them.
            value => de::Deserializer::deserialize_enum(value, name, variants, visitor),
        }
    }

    fill_or_any! {
        deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64 deserialize_i128
        deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64 deserialize_u128 deserialize_char
        deserialize_str deserialize_string deserialize_bytes deserialize_byte_buf deserialize_seq
        deserialize_map deserialize_identifier deserialize_ignored_any
    }
}

/// An enum variant with data: `{ "Variant": value }`.
struct Variant {
    variant: String,
    value: Lenient,
}

impl<'de> de::EnumAccess<'de> for Variant {
    type Error = Error;
    type Variant = Lenient;
    fn variant_seed<S: de::DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Lenient), Error> {
        let name: de::value::StringDeserializer<Error> = self.variant.into_deserializer();
        let variant = seed.deserialize(name)?;
        Ok((variant, self.value))
    }
}

impl<'de> de::VariantAccess<'de> for Lenient {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        match self.value {
            Value::Null => Ok(()),
            _ => Err(de::Error::invalid_type(de::Unexpected::Map, &"a unit variant")),
        }
    }
    fn newtype_variant_seed<S: de::DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, Error> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_tuple(self, len, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_struct(self, "", fields, visitor)
    }
}

// ------------------------------------------------------------------------------------- filling in

/// The key of the record that marks a value for the form to fill in: `{ FILL: null }` (an enum takes its
/// first variant) or `{ FILL: "Variant" }`. Only [`filled`] reads it.
pub const FILL: &str = "\u{1}fill";

/// The record [`filled`] replaces with a default-filled value of the type found there (an enum in
/// `variant`, or its first variant).
#[must_use]
pub fn fill_record(variant: Option<&str>) -> Value {
    let mut record = Map::new();
    record.insert(
        FILL.to_owned(),
        variant.map_or(Value::Null, |v| Value::String(v.to_owned())),
    );
    Value::Object(record)
}

/// The variant a fill record asks for (`Some(None)`: any), or `None` when `value` is not one.
fn fill_marker(value: &Value) -> Option<Option<String>> {
    let fields = value.as_object()?;
    if fields.len() != 1 {
        return None;
    }
    match fields.get(FILL)? {
        Value::Null => Some(None),
        Value::String(v) => Some(Some(v.clone())),
        _ => None,
    }
}

/// The field a `missing field` refusal names.
fn missing_field(error: &Error) -> Option<String> {
    let text = error.to_string();
    let rest = &text[text.find("missing field `")? + "missing field `".len()..];
    Some(rest[..rest.find('`')?].to_owned())
}

/// Reads `T` from `value` (a tree [`to_tree`] wrote, with [`fill_record`]s where values are to be made)
/// and writes it back as a tree: each fill record becomes a default-filled value of the type `T` has
/// there. A record gets its serde defaults; a field with none gets the type's zero value (`0`, `false`,
/// `""`, an empty list or map, `None`), an enum its first variant (or the one the record names) with
/// the variant's fields filled the same way.
///
/// # Errors
///
/// The tree does not fit `T`, or a filled-in value cannot be made (a type with no zero value, a value
/// whose type the format leaves open, a variant the enum lacks).
pub fn filled<T: de::DeserializeOwned + Serialize>(value: &Value) -> Result<Value, Error> {
    let mut required: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    // Each refusal names one more field to give a record: the formats' records are small.
    for _ in 0..256 {
        let ctx = Rc::new(Ctx {
            required: RefCell::new(required.clone()),
            ..Ctx::default()
        });
        let read = T::deserialize(Lenient {
            value: value.clone(),
            path: String::new(),
            ctx: Some(Rc::clone(&ctx)),
        });
        match read {
            Ok(typed) => return to_tree(&typed),
            Err(e) => {
                let Some((path, field)) = ctx.missing.borrow_mut().take() else {
                    return Err(e);
                };
                let fields = required.entry(path).or_default();
                if fields.contains(&field) {
                    return Err(e);
                }
                fields.push(field);
            }
        }
    }
    Err(de::Error::custom("the form cannot fill this value in"))
}

/// Every path of `value` (a tree [`to_tree`] wrote) that `T` reads as an enum, with the enum's
/// variants (form paths: `lifetime`, `live_mods[0].op`, `lifetime.Timed`).
///
/// # Errors
///
/// The tree does not fit `T`.
pub fn enum_variants<T: de::DeserializeOwned>(
    value: &Value,
) -> Result<BTreeMap<String, &'static [&'static str]>, Error> {
    let ctx = Rc::new(Ctx::default());
    T::deserialize(Lenient {
        value: value.clone(),
        path: String::new(),
        ctx: Some(Rc::clone(&ctx)),
    })?;
    Ok(ctx.enums.take())
}

/// The value made for a fill record at `path`: the type's zero value, a record's serde defaults.
struct Synth {
    ctx: Rc<Ctx>,
    path: String,
    variant: Option<String>,
}

impl<'de> IntoDeserializer<'de, Error> for Synth {
    type Deserializer = Self;
    fn into_deserializer(self) -> Self {
        self
    }
}

impl Synth {
    fn at(&self, path: String) -> Self {
        Self {
            ctx: Rc::clone(&self.ctx),
            path,
            variant: None,
        }
    }

    fn cannot(&self, what: &str) -> Error {
        de::Error::custom(format!("the form cannot fill in `{}`: {what}", self.path))
    }

    fn tuple<'de, V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        let items: Vec<Self> = (0..len).map(|i| self.at(format!("{}[{i}]", self.path))).collect();
        let mut seq = de::value::SeqDeserializer::<_, Error>::new(items.into_iter());
        let out = visitor.visit_seq(&mut seq)?;
        seq.end()?;
        Ok(out)
    }

    /// A record with only the fields learned to be required, each filled in.
    fn record<'de, V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let given = self
            .ctx
            .required
            .borrow()
            .get(&self.path)
            .cloned()
            .unwrap_or_default();
        let entries: Vec<(&'static str, Self)> = given
            .into_iter()
            .map(|f| (f, self.at(key_path(&self.path, f))))
            .collect();
        let mut map = de::value::MapDeserializer::<_, Error>::new(entries.into_iter());
        match visitor.visit_map(&mut map) {
            Ok(out) => {
                map.end()?;
                Ok(out)
            }
            Err(e) => {
                let mut missing = self.ctx.missing.borrow_mut();
                // The innermost record names the field first; an outer one passes the error on.
                if missing.is_none()
                    && let Some(name) = missing_field(&e)
                    && let Some(field) = fields.iter().find(|f| **f == name).copied()
                {
                    *missing = Some((self.path.clone(), field));
                }
                Err(e)
            }
        }
    }
}

impl<'de> de::Deserializer<'de> for Synth {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(self.cannot("the format does not say what type it is"))
    }
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_bool(false)
    }
    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(0)
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(0)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(0)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i64(0)
    }
    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_i128(0)
    }
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(0)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(0)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(0)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u64(0)
    }
    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_u128(0)
    }
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f32(0.0)
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_f64(0.0)
    }
    fn deserialize_char<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(self.cannot("a character has no empty value"))
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_str("")
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_str("")
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_bytes(&[])
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_bytes(&[])
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_none()
    }
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.tuple(0, visitor)
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        self.tuple(len, visitor)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.tuple(len, visitor)
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let mut map = de::value::MapDeserializer::<_, Error>::new(std::iter::empty::<(String, Self)>());
        let out = visitor.visit_map(&mut map)?;
        map.end()?;
        Ok(out)
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.record(fields, visitor)
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.ctx.enums.borrow_mut().insert(self.path.clone(), variants);
        let name = match &self.variant {
            Some(wanted) => variants
                .iter()
                .find(|v| **v == wanted.as_str())
                .copied()
                .ok_or_else(|| self.cannot(&format!("no variant `{wanted}` (one of {variants:?})")))?,
            None => variants
                .first()
                .copied()
                .ok_or_else(|| self.cannot("the enum has no variants"))?,
        };
        let value = self.at(key_path(&self.path, name));
        visitor.visit_enum(SynthVariant { name, value })
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(self.cannot("an identifier has no default"))
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
}

/// The variant a [`Synth`] enum takes.
struct SynthVariant {
    name: &'static str,
    value: Synth,
}

impl<'de> de::EnumAccess<'de> for SynthVariant {
    type Error = Error;
    type Variant = Synth;
    fn variant_seed<S: de::DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Synth), Error> {
        let name: de::value::StrDeserializer<Error> = self.name.into_deserializer();
        Ok((seed.deserialize(name)?, self.value))
    }
}

impl<'de> de::VariantAccess<'de> for Synth {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }
    fn newtype_variant_seed<S: de::DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, Error> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        self.tuple(len, visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.record(fields, visitor)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Unit,
        Newtype(f32),
        Tuple(f32, u8),
        Record { r: f64 },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
        chance: f32,
        maybe: Option<f32>,
        list: Vec<Shape>,
        pair: (u8, f32),
    }

    fn sample() -> Sample {
        Sample {
            name: "inf".into(),
            chance: 0.25,
            maybe: None,
            list: vec![
                Shape::Unit,
                Shape::Newtype(1.5),
                Shape::Tuple(2.0, 3),
                Shape::Record { r: -0.5 },
            ],
            pair: (7, 0.75),
        }
    }

    /// Finite values: the tree is `serde_json::to_value`'s, and it reads back to the same value.
    #[test]
    fn finite_values_match_serde_json_both_ways() {
        let s = sample();
        let tree = to_tree(&s).expect("tree");
        assert_eq!(tree, serde_json::to_value(&s).expect("json"));
        assert_eq!(from_tree::<Sample>(tree).expect("read"), s);
    }

    /// Non-finite values become records and read back bit for bit (NaN as NaN); a string field
    /// spelled like one stays a string, and a record never fits a string or an integer.
    #[test]
    fn non_finite_numbers_round_trip_as_records() {
        let mut s = sample();
        s.chance = f32::NAN;
        s.maybe = Some(f32::NEG_INFINITY);
        s.list[1] = Shape::Newtype(f32::INFINITY);
        s.list[3] = Shape::Record { r: f64::NAN };
        let tree = to_tree(&s).expect("tree");
        assert_eq!(tree["chance"], serde_json::json!({ "non_finite": "NaN" }));
        assert_eq!(tree["maybe"], serde_json::json!({ "non_finite": "-inf" }));
        assert_eq!(
            tree["list"][1]["Newtype"],
            serde_json::json!({ "non_finite": "inf" })
        );
        assert_eq!(tree["name"], "inf");
        let back: Sample = from_tree(tree.clone()).expect("read");
        assert!(back.chance.is_nan());
        assert_eq!(back.maybe, Some(f32::NEG_INFINITY));
        assert_eq!(back.list[1], Shape::Newtype(f32::INFINITY));
        assert!(matches!(back.list[3], Shape::Record { r } if r.is_nan()));
        assert_eq!(back.name, "inf");

        let mut wrong = tree.clone();
        wrong["name"] = serde_json::json!({ "non_finite": "NaN" });
        assert!(from_tree::<Sample>(wrong).is_err(), "a record is no string");
        let mut wrong = tree;
        wrong["pair"][0] = serde_json::json!({ "non_finite": "inf" });
        assert!(from_tree::<Sample>(wrong).is_err(), "a record is no integer");
        let mut bad = to_tree(&sample()).expect("tree");
        bad["chance"] = serde_json::json!({ "non_finite": "huge" });
        assert!(from_tree::<Sample>(bad).is_err(), "only RON's spellings");
    }

    fn two() -> f32 {
        2.0
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        name: String,
        #[serde(default = "two")]
        scale: f32,
        #[serde(default)]
        tags: Vec<String>,
        maybe: Option<u8>,
        shape: Shape,
        pair: (u8, bool),
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Holder {
        entries: Vec<Entry>,
        shape: Shape,
    }

    fn holder() -> Value {
        to_tree(&Holder {
            entries: Vec::new(),
            shape: Shape::Newtype(0.5),
        })
        .expect("tree")
    }

    /// A fill record becomes a value of the type found there: serde defaults where the type has them
    /// (`scale` 2.0, `tags` empty), zero values for the required fields, `None`, an enum's first
    /// variant, a tuple of zeros; the rest of the tree is read unchanged.
    #[test]
    fn a_fill_record_takes_serde_defaults_and_zero_values() {
        let mut tree = holder();
        tree["entries"] = serde_json::json!([fill_record(None)]);
        let filled = filled::<Holder>(&tree).expect("filled");
        assert_eq!(
            filled["entries"][0],
            serde_json::json!({ "name": "", "scale": 2.0, "tags": [], "maybe": null, "shape": "Unit", "pair": [0, false] })
        );
        assert_eq!(filled["shape"], serde_json::json!({ "Newtype": 0.5 }));
    }

    /// A named variant is filled the same way; an unknown name is refused, and so is a type with no
    /// zero value (`char`).
    #[test]
    fn a_named_variant_is_filled_and_unknown_ones_are_refused() {
        let mut tree = holder();
        for (variant, expected) in [
            ("Record", serde_json::json!({ "Record": { "r": 0.0 } })),
            ("Tuple", serde_json::json!({ "Tuple": [0.0, 0] })),
            ("Newtype", serde_json::json!({ "Newtype": 0.0 })),
            ("Unit", serde_json::json!("Unit")),
        ] {
            tree["shape"] = fill_record(Some(variant));
            assert_eq!(
                filled::<Holder>(&tree).expect(variant)["shape"],
                expected,
                "{variant}"
            );
        }
        tree["shape"] = fill_record(Some("Sideways"));
        assert!(filled::<Holder>(&tree).is_err());

        #[derive(Debug, Serialize, Deserialize)]
        struct Letters {
            letters: Vec<char>,
        }
        let tree = serde_json::json!({ "letters": [fill_record(None)] });
        assert!(filled::<Letters>(&tree).is_err(), "a char has no zero value");
    }

    /// Every path read as an enum is listed with its variants, inside lists and variants too.
    #[test]
    fn enum_paths_are_recorded_with_their_variants() {
        let s = sample();
        let enums = enum_variants::<Sample>(&to_tree(&s).expect("tree")).expect("read");
        let variants: &[&str] = &["Unit", "Newtype", "Tuple", "Record"];
        for i in 0..4 {
            assert_eq!(
                enums.get(&format!("list[{i}]")).copied(),
                Some(variants),
                "{enums:?}"
            );
        }
        assert_eq!(enums.len(), 4, "{enums:?}");
        // A plain read records nothing and reads as before.
        assert_eq!(from_tree::<Sample>(to_tree(&s).expect("tree")).expect("read"), s);
    }
}
