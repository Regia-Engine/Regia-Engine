// SPDX-License-Identifier: MPL-2.0
//! Serde shape fingerprinting for frozen content formats (`docs/specs/12-assets.md` § 4a).
//!
//! [`serde_field_names`] returns the ordered list of top-level field names a `#[derive(Deserialize)]`
//! struct declares. It reads the field list straight from the derive-generated `deserialize_struct`
//! call (the `FIELDS` array) rather than from a serialised value, so fields hidden behind
//! `skip_serializing_if` or optional defaults are still captured. `cargo xtask check-formats` hashes
//! this list to detect a serde-shape change that landed without a format-version bump and migrator.
//!
//! The wire *spelling* matters as much as the field list, so a fingerprint also includes
//! [`serde_variant_names`] for every serde enum the format reaches: the derive passes the
//! post-`rename`/`rename_all` `VARIANTS` array to `deserialize_enum` — canonical names *and*
//! `#[serde(alias = "...")]` strings, as the tests here pin. Dropping a `rename_all` (the
//! 2026-10-06 `MaterialShaderRef` miss: `standard` silently became `Standard`) or editing an
//! alias therefore changes the hash even though no field was added or removed.
//!
//! Internally tagged (`#[serde(tag = "kind")]`) and adjacently tagged (`#[serde(tag, content)]`)
//! enums never reach `deserialize_enum`, so [`serde_variant_names`] and [`serde_variant_shapes`]
//! fall back to probing them (ADR-8799): the derive's own errors name the tag (`missing_field`),
//! the variants (`unknown_variant`) and, key by key, each struct-variant field (`duplicate_field`
//! after a field index is sent twice). A `rules-ron` field such as `ZoneMode::Hardpoint`'s
//! `rotate_ticks` is therefore part of the fingerprint. The representation itself (which key holds
//! the tag) is not hashed; a fixture round-trip covers that (`docs/formats/`).

use serde::de::{self, Deserialize, Deserializer, Visitor};

/// The error type for the capture deserializers. It only ever signals "we captured the fields and
/// stopped early"; callers ignore the error and read the captured names.
type CaptureError = de::value::Error;

/// A deserializer that captures a struct's declared field names and stops.
///
/// `T::deserialize` is driven once and abandoned: the derive-generated code calls
/// [`FieldCapture::deserialize_struct`], which records the `FIELDS` array, then returns an error so
/// no value is ever materialised (no `Default` needed, no required fields to invent).
struct FieldCapture {
    fields: Vec<String>,
    captured: bool,
}

/// Returns the ordered top-level field names of the serde struct `T`.
///
/// Only the field *names* (and their order) of `T` itself are returned. A format whose root struct
/// contains nested structs must add those nested types' field lists to its fingerprint too (see
/// [`join_field_lists`]) so a change to a nested struct — e.g. adding a field to a material's
/// `StandardMaterialParams` — still changes the hash.
///
/// The names are the **wire** names: serde's derive passes the post-`rename`/`rename_all`
/// identifiers in the `FIELDS` array, so renaming a field changes this list.
pub fn serde_field_names<T: Deserialize<'static>>() -> Vec<String> {
    try_serde_field_names::<T>().unwrap_or_default()
}

/// Like [`serde_field_names`], but fails when `T` never reached `deserialize_struct` (a tuple
/// struct, newtype, unit struct or untagged enum), where an empty list would silently fingerprint
/// as "no fields". A fingerprint check should use this and treat `Err` as a failure.
pub fn try_serde_field_names<T: Deserialize<'static>>() -> Result<Vec<String>, String> {
    let mut capture = FieldCapture {
        fields: Vec::new(),
        captured: false,
    };
    let _ = T::deserialize(&mut capture);
    if capture.captured {
        Ok(capture.fields)
    } else {
        Err(format!(
            "`{}` does not deserialize through `deserialize_struct`; its shape cannot be fingerprinted",
            std::any::type_name::<T>()
        ))
    }
}

impl<'de> Deserializer<'de> for &mut FieldCapture {
    type Error = CaptureError;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.fields = fields.iter().map(|s| (*s).to_owned()).collect();
        self.captured = true;
        Err(de::Error::custom("field capture complete"))
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("expected a struct at the top level"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

/// A deserializer that captures an enum's variant names and stops.
///
/// Mirrors [`FieldCapture`] for enums: the derive-generated `deserialize_enum` call receives the
/// renamed `VARIANTS` array before any payload is visited.
struct VariantCapture {
    variants: Vec<String>,
    captured: bool,
}

/// Returns the variant names of the serde enum `T` as serde's derive spells them **on the
/// wire**: for each variant the canonical name (after `rename` / `rename_all`) and its
/// `#[serde(alias = "...")]` strings. Serde keeps these strings in a sorted set per variant, so the
/// order within one variant is lexicographic (`"Standard"` before `"standard"`), not canonical-first.
///
/// Renaming a variant, dropping a container `rename_all`, or adding/removing an alias all
/// change this list (serde's derive puts aliases in the same `VARIANTS` array), so a
/// fingerprint that includes it catches every wire-spelling change the field list cannot see.
pub fn serde_variant_names<T: Deserialize<'static>>() -> Vec<String> {
    try_serde_variant_names::<T>().unwrap_or_default()
}

/// Like [`serde_variant_names`], but fails when `T` is not an enum serde can name the variants of
/// (a struct, or an untagged enum) instead of returning an empty list. Internally and adjacently
/// tagged enums are probed (see the module docs).
pub fn try_serde_variant_names<T: Deserialize<'static>>() -> Result<Vec<String>, String> {
    let mut capture = VariantCapture {
        variants: Vec::new(),
        captured: false,
    };
    let _ = T::deserialize(&mut capture);
    if capture.captured {
        return Ok(capture.variants);
    }
    if let Some(layout) = tagged::layout::<T>()
        && let Some(names) = tagged::names::<T>(&layout)
    {
        return Ok(names);
    }
    Err(format!(
        "`{}` does not deserialize through `deserialize_enum` and is not a tagged enum; its variants cannot be fingerprinted",
        std::any::type_name::<T>()
    ))
}

/// Concatenates several field-name lists (one per struct type of a format) into a single
/// fingerprint string.
///
/// `cargo xtask check-formats` hashes this string. The `;` boundary between lists keeps each
/// type's fields distinct, so adding or renaming a field in *any* listed struct — including a
/// nested one — changes the hash and trips the guard. Format crates list their root struct first,
/// then every struct reachable from it (transitively), so a shape change anywhere in the tree is
/// visible.
pub fn join_field_lists(lists: &[Vec<String>]) -> String {
    lists
        .iter()
        .map(|fields| fields.join(","))
        .collect::<Vec<_>>()
        .join(";")
}

impl<'de> Deserializer<'de> for &mut VariantCapture {
    type Error = CaptureError;

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.variants = variants.iter().map(|s| (*s).to_owned()).collect();
        self.captured = true;
        Err(de::Error::custom("variant capture complete"))
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("expected an enum at the top level"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

/// Returns the payload shape of every variant of the serde enum `T`, in variant-index order:
/// `"0:unit"`, `"1:newtype"`, `"2:tuple(3)"` or `"3:{first,second}"` (a struct variant's wire field
/// names). [`serde_variant_names`] gives the names; this gives what each variant carries, so adding,
/// removing or renaming a field of a struct variant (`BuildSpec::Cargo { package, profile }`) changes
/// a fingerprint that lists both (ADR-2316).
///
/// Internally and adjacently tagged enums are probed (ADR-8799; see the module docs). Their
/// shapes use the same words with two differences that follow the wire: an internally tagged
/// newtype variant lists the fields of the struct it wraps (they sit beside the tag), and a field
/// the probe cannot fill with any simple value is written `?` (its position still counts). Empty
/// when `T` is not an enum.
pub fn serde_variant_shapes<T: Deserialize<'static>>() -> Vec<String> {
    let mut shapes = Vec::new();
    // An enum has fewer variants than `u32::MAX`; the derive refuses the first index past the last.
    for index in 0..u32::MAX {
        let mut capture = ShapeCapture { index, shape: None };
        let _ = T::deserialize(&mut capture);
        match capture.shape {
            Some(shape) => shapes.push(format!("{index}:{shape}")),
            None => break,
        }
    }
    if shapes.is_empty()
        && let Some(layout) = tagged::layout::<T>()
        && tagged::names::<T>(&layout).is_some()
    {
        shapes = tagged::shapes::<T>(&layout);
    }
    shapes
}

/// Probes for `#[serde(tag)]` and `#[serde(tag, content)]` enums, which the derive deserializes
/// through `deserialize_any` (internal) or `deserialize_struct` with the two keys (adjacent) instead
/// of `deserialize_enum`. Every probe drives `T::deserialize` with a small synthetic map and reads
/// the structured error the derive raises; no value is kept.
mod tagged {
    use serde::de::{self, Deserialize, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    /// A tag value no enum uses, so the derive answers with `unknown_variant` and its `VARIANTS`.
    const UNKNOWN_TAG: &str = "\u{0}regia-schema-probe";
    /// Field indices probed per variant before giving up (no derive struct is this wide).
    const MAX_FIELDS: u64 = 256;
    /// Longest sequence a candidate value grows to while the derive asks for more elements.
    const MAX_SEQ: usize = 32;

    /// How the enum puts its tag on the wire.
    pub(super) enum Layout {
        /// `#[serde(tag = "..")]`: the tag key sits beside the variant's fields.
        Internal { tag: String },
        /// `#[serde(tag = "..", content = "..")]`: the payload sits under the content key.
        Adjacent { tag: String, content: String },
    }

    /// The serde error kinds the probes read. Everything else is `Other`.
    #[derive(Debug, Clone, PartialEq)]
    enum ProbeError {
        MissingField(String),
        DuplicateField(String),
        UnknownVariant(Vec<String>),
        /// `invalid_value` with an unsigned value: an index past the last variant or field.
        IndexPastEnd,
        /// `invalid_length`: a sequence was too short.
        Length,
        /// The adjacently tagged derive asked for its two keys (tag, content).
        Adjacent(String, String),
        /// The adjacently tagged tag seed asked for the variants (enum name, variants).
        Variants(String, Vec<String>),
        /// The adjacently tagged payload was read with this deserializer method.
        Payload(String),
        /// Anything else (the probe only needs to know it was not one of the above).
        Other,
    }

    impl fmt::Display for ProbeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{self:?}")
        }
    }

    impl std::error::Error for ProbeError {}

    impl de::Error for ProbeError {
        fn custom<M: fmt::Display>(_msg: M) -> Self {
            Self::Other
        }
        fn missing_field(field: &'static str) -> Self {
            Self::MissingField(field.to_owned())
        }
        fn duplicate_field(field: &'static str) -> Self {
            Self::DuplicateField(field.to_owned())
        }
        fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
            Self::UnknownVariant(expected.iter().map(|s| (*s).to_owned()).collect())
        }
        fn invalid_value(unexp: de::Unexpected<'_>, _exp: &dyn de::Expected) -> Self {
            match unexp {
                de::Unexpected::Unsigned(_) => Self::IndexPastEnd,
                _ => Self::Other,
            }
        }
        fn invalid_length(_len: usize, _exp: &dyn de::Expected) -> Self {
            Self::Length
        }
    }

    type Outcome = Result<(), ProbeError>;

    /// A simple value offered for a struct-variant field.
    #[derive(Debug, Clone)]
    enum Candidate {
        U64,
        Bool,
        Str(String),
        Seq(usize),
        Map,
        None,
        Unit,
        F64,
        I64,
    }

    const CANDIDATES: [Candidate; 9] = [
        Candidate::U64,
        Candidate::Bool,
        Candidate::Str(String::new()),
        Candidate::Seq(0),
        Candidate::Map,
        Candidate::None,
        Candidate::Unit,
        Candidate::F64,
        Candidate::I64,
    ];

    #[derive(Debug, Clone)]
    enum Key {
        Name(String),
        Index(u64),
    }

    #[derive(Debug, Clone)]
    enum Value {
        /// The tag, asking for the variant list.
        TagNames,
        /// The tag, selecting variant `n` by index.
        TagIndex(u32),
        Cand(Candidate),
        /// An adjacently tagged payload given as a map of these entries.
        Map(Vec<(Key, Value)>),
        /// An adjacently tagged payload that reports how the derive reads it.
        Recorder,
    }

    /// The top-level deserializer: `None` discovers the layout, `Some` feeds these map entries.
    struct Top(Option<Vec<(Key, Value)>>);

    impl<'de> Deserializer<'de> for Top {
        type Error = ProbeError;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            visitor.visit_map(Entries::new(self.0.unwrap_or_default()))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            match (self.0, fields) {
                (Some(entries), _) => visitor.visit_map(Entries::new(entries)),
                (None, [tag, content]) => Err(ProbeError::Adjacent((*tag).to_owned(), (*content).to_owned())),
                (None, _) => Err(ProbeError::Other),
            }
        }

        fn deserialize_ignored_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, ProbeError> {
            Err(ProbeError::Other)
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier
        }
    }

    /// A map access over a fixed list of probe entries.
    struct Entries {
        entries: std::vec::IntoIter<(Key, Value)>,
        value: Option<Value>,
    }

    impl Entries {
        fn new(entries: Vec<(Key, Value)>) -> Self {
            Self {
                entries: entries.into_iter(),
                value: None,
            }
        }
    }

    impl<'de> MapAccess<'de> for Entries {
        type Error = ProbeError;

        fn next_key_seed<K: DeserializeSeed<'de>>(
            &mut self,
            seed: K,
        ) -> Result<Option<K::Value>, ProbeError> {
            match self.entries.next() {
                Some((key, value)) => {
                    self.value = Some(value);
                    seed.deserialize(KeyDe(key)).map(Some)
                }
                None => Ok(None),
            }
        }

        fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, ProbeError> {
            match self.value.take() {
                Some(value) => seed.deserialize(ValueDe(value)),
                None => Err(ProbeError::Other),
            }
        }
    }

    /// A map key: a name is a string, an index is the field index the derive accepts.
    struct KeyDe(Key);

    impl<'de> Deserializer<'de> for KeyDe {
        type Error = ProbeError;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            match self.0 {
                Key::Name(name) => visitor.visit_str(&name),
                Key::Index(index) => visitor.visit_u64(index),
            }
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct enum identifier ignored_any
        }
    }

    /// A map value.
    struct ValueDe(Value);

    impl ValueDe {
        /// Every typed read except `any`, `identifier`, `enum` and `tuple`.
        fn typed<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            match self.0 {
                Value::Recorder => Err(ProbeError::Payload("newtype".to_owned())),
                _ => self.deserialize_any(visitor),
            }
        }
    }

    macro_rules! typed {
        ($($method:ident)*) => {
            $(
                fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
                    self.typed(visitor)
                }
            )*
        };
    }

    impl<'de> Deserializer<'de> for ValueDe {
        type Error = ProbeError;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            match self.0 {
                Value::Cand(candidate) => candidate.visit(visitor),
                Value::Map(entries) => visitor.visit_map(Entries::new(entries)),
                Value::Recorder => Err(ProbeError::Payload("any".to_owned())),
                Value::TagNames | Value::TagIndex(_) => Err(ProbeError::Other),
            }
        }

        fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            match self.0 {
                Value::TagNames => visitor.visit_str(UNKNOWN_TAG),
                Value::TagIndex(index) => visitor.visit_u64(u64::from(index)),
                _ => self.typed(visitor),
            }
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            name: &'static str,
            variants: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            match self.0 {
                Value::TagNames => Err(ProbeError::Variants(
                    name.to_owned(),
                    variants.iter().map(|s| (*s).to_owned()).collect(),
                )),
                Value::TagIndex(index) => visitor.visit_enum(IndexAccess(index)),
                _ => self.typed(visitor),
            }
        }

        fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, ProbeError> {
            match self.0 {
                Value::Recorder => Err(ProbeError::Payload(format!("tuple({len})"))),
                _ => self.deserialize_any(visitor),
            }
        }

        fn deserialize_unit_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            self.typed(visitor)
        }

        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            self.typed(visitor)
        }

        fn deserialize_tuple_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            _len: usize,
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            self.typed(visitor)
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            _fields: &'static [&'static str],
            visitor: V,
        ) -> Result<V::Value, ProbeError> {
            self.typed(visitor)
        }

        typed! {
            deserialize_bool deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64
            deserialize_i128 deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64
            deserialize_u128 deserialize_f32 deserialize_f64 deserialize_char deserialize_str
            deserialize_string deserialize_bytes deserialize_byte_buf deserialize_option
            deserialize_unit deserialize_seq deserialize_map deserialize_ignored_any
        }
    }

    impl Candidate {
        fn visit<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, ProbeError> {
            match self {
                Self::U64 => visitor.visit_u64(0),
                Self::Bool => visitor.visit_bool(false),
                Self::Str(text) => visitor.visit_str(&text),
                Self::Seq(len) => visitor.visit_seq(Zeros(len)),
                Self::Map => visitor.visit_map(Entries::new(Vec::new())),
                Self::None => visitor.visit_none(),
                Self::Unit => visitor.visit_unit(),
                Self::F64 => visitor.visit_f64(0.0),
                Self::I64 => visitor.visit_i64(-1),
            }
        }
    }

    /// A sequence of `n` zeros.
    struct Zeros(usize);

    impl<'de> SeqAccess<'de> for Zeros {
        type Error = ProbeError;

        fn next_element_seed<S: DeserializeSeed<'de>>(
            &mut self,
            seed: S,
        ) -> Result<Option<S::Value>, ProbeError> {
            if self.0 == 0 {
                return Ok(None);
            }
            self.0 -= 1;
            seed.deserialize(ValueDe(Value::Cand(Candidate::U64))).map(Some)
        }
    }

    /// Selects variant `n` for the adjacently tagged tag seed (it reads a unit variant).
    struct IndexAccess(u32);

    impl<'de> de::EnumAccess<'de> for IndexAccess {
        type Error = ProbeError;
        type Variant = Self;

        fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self), ProbeError> {
            let variant = seed.deserialize(de::value::U32Deserializer::<ProbeError>::new(self.0))?;
            Ok((variant, self))
        }
    }

    impl<'de> de::VariantAccess<'de> for IndexAccess {
        type Error = ProbeError;

        fn unit_variant(self) -> Result<(), ProbeError> {
            Ok(())
        }

        fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, _seed: S) -> Result<S::Value, ProbeError> {
            Err(ProbeError::Other)
        }

        fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value, ProbeError> {
            Err(ProbeError::Other)
        }

        fn struct_variant<V: Visitor<'de>>(
            self,
            _fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, ProbeError> {
            Err(ProbeError::Other)
        }
    }

    fn run<T: Deserialize<'static>>(entries: Option<Vec<(Key, Value)>>) -> Outcome {
        T::deserialize(Top(entries)).map(|_| ())
    }

    /// The last path segment of `T`'s Rust name, without generics (what the adjacently tagged
    /// derive passes as the enum name).
    fn short_name<T>() -> &'static str {
        let full = std::any::type_name::<T>();
        let base = full.split('<').next().unwrap_or(full);
        base.rsplit("::").next().unwrap_or(base)
    }

    /// The tag layout of `T`, or `None` when `T` is not a tagged enum.
    pub(super) fn layout<T: Deserialize<'static>>() -> Option<Layout> {
        match run::<T>(None) {
            Err(ProbeError::MissingField(tag)) => Some(Layout::Internal { tag }),
            Err(ProbeError::Adjacent(tag, content)) => Some(Layout::Adjacent { tag, content }),
            _ => None,
        }
    }

    /// The variant names (with aliases) of a tagged enum, in the derive's `VARIANTS` order.
    pub(super) fn names<T: Deserialize<'static>>(layout: &Layout) -> Option<Vec<String>> {
        match layout {
            Layout::Internal { tag } => match run::<T>(Some(vec![(Key::Name(tag.clone()), Value::TagNames)]))
            {
                Err(ProbeError::UnknownVariant(names)) => Some(names),
                _ => None,
            },
            Layout::Adjacent { tag, .. } => {
                match run::<T>(Some(vec![(Key::Name(tag.clone()), Value::TagNames)])) {
                    // A plain two-field struct whose first field is an enum would answer with that
                    // field's enum: only `T`'s own name counts.
                    Err(ProbeError::Variants(name, names)) if name == short_name::<T>() => Some(names),
                    _ => None,
                }
            }
        }
    }

    /// The shapes of a tagged enum's variants, in index order (`"0:unit"`, `"1:{a,b}"`, ...).
    pub(super) fn shapes<T: Deserialize<'static>>(layout: &Layout) -> Vec<String> {
        let mut shapes = Vec::new();
        for index in 0..u32::MAX {
            let shape = match layout {
                Layout::Internal { tag } => internal_shape::<T>(tag, index),
                Layout::Adjacent { tag, content } => adjacent_shape::<T>(tag, content, index),
            };
            match shape {
                Some(shape) => shapes.push(format!("{index}:{shape}")),
                None => break,
            }
        }
        shapes
    }

    fn internal_shape<T: Deserialize<'static>>(tag: &str, index: u32) -> Option<String> {
        let with = |extra: Vec<(Key, Value)>| {
            let mut entries = vec![(Key::Name(tag.to_owned()), Value::TagIndex(index))];
            entries.extend(extra);
            run::<T>(Some(entries))
        };
        let base = with(Vec::new());
        if base == Err(ProbeError::IndexPastEnd) {
            return None;
        }
        Some(fields_shape(&base, &with))
    }

    fn adjacent_shape<T: Deserialize<'static>>(tag: &str, content: &str, index: u32) -> Option<String> {
        let with = |payload: Value| {
            run::<T>(Some(vec![
                (Key::Name(tag.to_owned()), Value::TagIndex(index)),
                (Key::Name(content.to_owned()), payload),
            ]))
        };
        let tag_only = run::<T>(Some(vec![(Key::Name(tag.to_owned()), Value::TagIndex(index))]));
        match tag_only {
            Err(ProbeError::IndexPastEnd) => return None,
            // A unit variant needs no payload.
            Ok(()) => return Some("unit".to_owned()),
            Err(_) => {}
        }
        match with(Value::Recorder) {
            // A struct variant reads its payload with `deserialize_any`.
            Err(ProbeError::Payload(kind)) if kind == "any" => {
                let in_map = |extra: Vec<(Key, Value)>| with(Value::Map(extra));
                Some(fields_shape(&in_map(Vec::new()), &in_map))
            }
            Err(ProbeError::Payload(kind)) => Some(kind),
            _ => Some("newtype".to_owned()),
        }
    }

    /// The field list of a struct-like payload, found index by index: field `k` sent twice makes
    /// the derive name it (`duplicate_field`) once a candidate value fits its type.
    fn fields_shape(base: &Outcome, with: &dyn Fn(Vec<(Key, Value)>) -> Outcome) -> String {
        let mut fields: Vec<String> = Vec::new();
        for k in 0..MAX_FIELDS {
            if let Some(name) = field_name(k, with) {
                fields.push(name);
                continue;
            }
            // No candidate fits: past the last field the key is ignored (or refused as an index
            // when unknown fields are denied); a real field refuses the value instead.
            let alone = with(vec![(Key::Index(k), Value::Cand(Candidate::Unit))]);
            if &alone == base || alone == Err(ProbeError::IndexPastEnd) {
                break;
            }
            fields.push("?".to_owned());
        }
        if fields.is_empty() {
            if base.is_ok() {
                "unit".to_owned()
            } else {
                "newtype".to_owned()
            }
        } else {
            format!("{{{}}}", fields.join(","))
        }
    }

    fn field_name(k: u64, with: &dyn Fn(Vec<(Key, Value)>) -> Outcome) -> Option<String> {
        for start in CANDIDATES {
            let mut candidate = start;
            // A candidate is adjusted a bounded number of times: a nested unit enum gets its first
            // variant name, a short sequence grows one element at a time.
            for _ in 0..=MAX_SEQ {
                let twice = vec![
                    (Key::Index(k), Value::Cand(candidate.clone())),
                    (Key::Index(k), Value::Cand(candidate.clone())),
                ];
                let next = match (with(twice), &candidate) {
                    (Err(ProbeError::DuplicateField(name)), _) => return Some(name),
                    (Err(ProbeError::UnknownVariant(names)), Candidate::Str(text)) if text.is_empty() => {
                        names.into_iter().next().map(Candidate::Str)
                    }
                    (Err(ProbeError::Length), Candidate::Seq(len)) if *len < MAX_SEQ => {
                        Some(Candidate::Seq(len + 1))
                    }
                    _ => None,
                };
                match next {
                    Some(adjusted) => candidate = adjusted,
                    None => break,
                }
            }
        }
        None
    }
}

/// A deserializer that selects variant `index` of an enum and records the shape of its payload.
struct ShapeCapture {
    index: u32,
    shape: Option<String>,
}

impl<'de> Deserializer<'de> for &mut ShapeCapture {
    type Error = CaptureError;

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_enum(self)
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("expected an enum at the top level"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

impl<'de> de::EnumAccess<'de> for &mut ShapeCapture {
    type Error = CaptureError;
    type Variant = Self;

    fn variant_seed<S: de::DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self), Self::Error> {
        // The derive's variant identifier accepts the variant index (`visit_u64`); an index past
        // the last variant is refused, which ends the walk.
        let variant = seed.deserialize(de::value::U32Deserializer::<CaptureError>::new(self.index))?;
        Ok((variant, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut ShapeCapture {
    type Error = CaptureError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        self.shape = Some("unit".to_owned());
        Ok(())
    }

    fn newtype_variant_seed<S: de::DeserializeSeed<'de>>(self, _seed: S) -> Result<S::Value, Self::Error> {
        self.shape = Some("newtype".to_owned());
        Err(de::Error::custom("shape capture complete"))
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, _visitor: V) -> Result<V::Value, Self::Error> {
        self.shape = Some(format!("tuple({len})"));
        Err(de::Error::custom("shape capture complete"))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.shape = Some(format!("{{{}}}", fields.join(",")));
        Err(de::Error::custom("shape capture complete"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum Mixed {
        Unit,
        #[serde(alias = "wrapped")]
        Wrapped(u32),
        Pair(u32, String),
        Named {
            first: u32,
            #[serde(rename = "renamed")]
            second: String,
        },
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum MixedFieldRenamed {
        Unit,
        #[serde(alias = "wrapped")]
        Wrapped(u32),
        Pair(u32, String),
        Named {
            first: u32,
            second: String,
        },
    }

    #[test]
    fn variant_shapes_list_every_payload_in_index_order() {
        assert_eq!(
            serde_variant_shapes::<Mixed>(),
            vec!["0:unit", "1:newtype", "2:tuple(2)", "3:{first,renamed}"]
        );
        // A field rename inside a struct variant changes the shapes, not the names.
        assert_eq!(
            serde_variant_names::<Mixed>(),
            serde_variant_names::<MixedFieldRenamed>()
        );
        assert_ne!(
            serde_variant_shapes::<Mixed>(),
            serde_variant_shapes::<MixedFieldRenamed>()
        );
        // Not an enum: nothing, not a panic.
        assert!(serde_variant_shapes::<Plain>().is_empty());
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Plain {
        format_version: u32,
        name: Option<String>,
        #[serde(default)]
        layers: Vec<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<u32>,
    }

    #[test]
    fn captures_field_names_in_declaration_order() {
        assert_eq!(
            serde_field_names::<Plain>(),
            vec!["format_version", "name", "layers", "hidden"]
        );
    }

    #[test]
    fn captures_optional_and_skipped_fields() {
        // `hidden` would be absent from a serialised default value (skip_serializing_if); the
        // deserializer path still sees it, which is the point of this module.
        let names = serde_field_names::<Plain>();
        assert!(names.contains(&"hidden".to_owned()));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    struct Renamed {
        #[serde(default)]
        first_field: u32,
        #[serde(default)]
        second_field: u32,
    }

    #[test]
    fn struct_field_names_are_the_wire_names() {
        // rename_all reaches the FIELDS array the capture reads, so a struct-side rename
        // changes the fingerprint without any field being added or removed.
        assert_eq!(
            serde_field_names::<Renamed>(),
            vec!["first_field", "second_field"]
        );
    }

    /// The shape the material format had before 2026-10-06: snake_case variants plus aliases
    /// for the documented UpperCamel spellings.
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum WithRenameAll {
        #[serde(alias = "Standard")]
        Standard,
        #[serde(alias = "Custom")]
        Custom,
    }

    /// The same enum with `rename_all` dropped (the wire-format change `check-formats` missed).
    #[derive(Deserialize)]
    enum WithoutRenameAll {
        Standard,
        Custom,
    }

    #[test]
    fn enum_variant_names_are_the_wire_names_plus_aliases() {
        // Canonical (rename_all) spelling first, each alias after its variant.
        assert_eq!(
            serde_variant_names::<WithRenameAll>(),
            vec!["Standard", "standard", "Custom", "custom"]
        );
        assert_eq!(
            serde_variant_names::<WithoutRenameAll>(),
            vec!["Standard", "Custom"]
        );
    }

    /// The regression the task names: removing a `rename_all` must change the fingerprint
    /// string a format crate hashes, so `cargo xtask check-formats` fails the unchanged
    /// version (`MaterialShaderRef`, 2026-10-06).
    #[test]
    fn removing_a_rename_all_changes_the_fingerprint() {
        let with = serde_variant_names::<WithRenameAll>().join(",");
        let without = serde_variant_names::<WithoutRenameAll>().join(",");
        assert_ne!(with, without, "the wire spelling changed; the hash must too");
    }

    #[derive(Deserialize)]
    struct Newtype(#[allow(dead_code)] u32);

    #[test]
    fn non_struct_shapes_are_reported_not_empty() {
        assert!(try_serde_field_names::<Plain>().is_ok());
        assert!(try_serde_field_names::<Newtype>().is_err());
        assert!(try_serde_variant_names::<WithRenameAll>().is_ok());
        assert!(try_serde_variant_names::<Plain>().is_err());
        assert!(serde_field_names::<Newtype>().is_empty());
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Inner {
        a: u32,
        b: String,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(rename_all = "snake_case")]
    enum Nested {
        First,
        Second,
    }

    /// Internally tagged, every variant style it allows (ADR-8799).
    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Internal {
        Plain,
        Hardpoint {
            rotation: Vec<u32>,
            rotate_ticks: u64,
        },
        Defaults {
            #[serde(default)]
            ticks: Option<u64>,
            #[serde(default, rename = "flag")]
            on: bool,
        },
        Wrapped(Inner),
        #[serde(alias = "odd")]
        Typed {
            radius: f32,
            nested: Nested,
            color: [u8; 4],
            pair: (u8, u8),
        },
    }

    /// `Internal` with one field added to `Hardpoint` (the review's `hold_ticks` case).
    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum InternalPlusField {
        Plain,
        Hardpoint {
            rotation: Vec<u32>,
            rotate_ticks: u64,
            hold_ticks: u64,
        },
        Defaults {
            #[serde(default)]
            ticks: Option<u64>,
            #[serde(default, rename = "flag")]
            on: bool,
        },
        Wrapped(Inner),
        #[serde(alias = "odd")]
        Typed {
            radius: f32,
            nested: Nested,
            color: [u8; 4],
            pair: (u8, u8),
        },
    }

    #[test]
    fn internally_tagged_enums_list_names_and_payload_fields() {
        assert_eq!(
            serde_variant_names::<Internal>(),
            vec!["plain", "hardpoint", "defaults", "wrapped", "odd", "typed"]
        );
        assert_eq!(
            serde_variant_shapes::<Internal>(),
            vec![
                "0:unit",
                "1:{rotation,rotate_ticks}",
                "2:{ticks,flag}",
                "3:{a,b}",
                "4:{radius,nested,color,pair}"
            ]
        );
        assert_ne!(
            serde_variant_shapes::<Internal>(),
            serde_variant_shapes::<InternalPlusField>(),
            "a new struct-variant field must move the shapes"
        );
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum InternalDenied {
        AfterCleared {
            #[serde(default)]
            delay_ticks: u64,
        },
        AtTick {
            tick: u64,
        },
        Surrender,
    }

    #[test]
    fn deny_unknown_fields_does_not_change_the_tagged_shapes() {
        assert_eq!(
            serde_variant_names::<InternalDenied>(),
            vec!["after_cleared", "at_tick", "surrender"]
        );
        assert_eq!(
            serde_variant_shapes::<InternalDenied>(),
            vec!["0:{delay_ticks}", "1:{tick}", "2:unit"]
        );
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "policy", content = "n", rename_all = "snake_case")]
    enum Adjacent {
        Never,
        WhenUnbalancedBy(u8),
        Pair(u8, u16),
        Named { first: u32, second: Option<String> },
        Wrapped(Inner),
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "policy", content = "n", rename_all = "snake_case", deny_unknown_fields)]
    enum AdjacentDenied {
        Never,
        BetweenMatches,
        WhenUnbalancedBy(u8),
    }

    #[test]
    fn adjacently_tagged_enums_list_names_and_payloads() {
        assert_eq!(
            serde_variant_names::<Adjacent>(),
            vec!["never", "when_unbalanced_by", "pair", "named", "wrapped"]
        );
        assert_eq!(
            serde_variant_shapes::<Adjacent>(),
            vec![
                "0:unit",
                "1:newtype",
                "2:tuple(2)",
                "3:{first,second}",
                "4:newtype"
            ]
        );
        assert_eq!(
            serde_variant_shapes::<AdjacentDenied>(),
            vec!["0:unit", "1:unit", "2:newtype"]
        );
    }

    /// A two-field struct looks like an adjacently tagged enum to `deserialize_struct`; its enum
    /// field must not be reported as the struct's variants.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct TwoFields {
        mode: Nested,
        count: u32,
    }

    #[test]
    fn structs_and_untagged_enums_are_still_not_tagged_enums() {
        assert!(try_serde_variant_names::<TwoFields>().is_err());
        assert!(serde_variant_shapes::<TwoFields>().is_empty());
        assert!(try_serde_variant_names::<Plain>().is_err());
        assert!(try_serde_variant_names::<Newtype>().is_err());
        // The externally tagged path is unchanged.
        assert_eq!(serde_variant_names::<Nested>(), vec!["first", "second"]);
        assert_eq!(serde_variant_shapes::<Nested>(), vec!["0:unit", "1:unit"]);
    }

    #[test]
    fn removing_an_alias_changes_the_variant_names() {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum NoAlias {
            Standard,
            Custom,
        }
        assert_ne!(
            serde_variant_names::<WithRenameAll>(),
            serde_variant_names::<NoAlias>(),
            "dropping the aliases must change the captured list"
        );
    }
}
