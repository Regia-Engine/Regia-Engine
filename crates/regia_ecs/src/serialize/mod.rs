// SPDX-License-Identifier: MPL-2.0
//! Reflected serialization (`docs/specs/10-core.md` § 2.3): canonical RON for text (scenes,
//! prefabs, configs) and postcard for binary (cooked scenes, saves, snapshots, net).
//!
//! # Format
//! A value is written under its full type path: `{"my_game::Door": (open_angle: 90.0)}`. The
//! `*_typed` variants write the bare value when the type is known from context.
//!
//! # Canonical RON (`docs/specs/23-agent-native.md` § 1)
//! Struct fields in declaration order, map entries and set items sorted (by the reflected ordering
//! of the keys, then their RON text, then their canonical binary form, then the same two forms of
//! the map value, so two entries that write differently cannot tie; entries are put in that total
//! order before the reflected sort, so a reflected ordering that is only partial, such as a NaN
//! field, still gives one order that depends only on the contents, and the sort cannot panic),
//! floats in shortest round-trip form, one fixed
//! [`canonical_pretty_config`], trailing newline. Re-serializing an unchanged value is
//! byte-identical. Limit: a type serialized through its own serde implementation
//! (`#[reflect(Serialize)]`) controls its own output; such types must be deterministic themselves.
//!
//! # Loading rules
//! - **Missing fields** take the container's `Default` (`#[reflect(Default)]`), else the field
//!   type's registered `Default` (a `#[reflect(skip_serializing)]` field is the exception: it takes
//!   its own default first, ahead of the container's); otherwise loading fails. Applies to named
//!   structs and named enum variants (which use the field type's `Default`). A tuple struct is not
//!   filled: a missing element is an error. A type with `#[reflect(Deserialize)]` or
//!   `#[reflect(DeserializeWithRegistry)]` is decoded by its own serde and does not get this fill.
//!   A container-default field whose value does not reflect-clone (it holds `#[reflect(ignore)]`
//!   state) is left out of the loaded dynamic value, so `FromReflect` keeps the container
//!   `Default`'s value whole. The same applies one level down: when the field type's `Default`
//!   is a named struct and one of its reflected fields will not reflect-clone, the missing field
//!   is an empty dynamic struct of that type (only if `FromReflect` of the empty struct succeeds),
//!   so the field type's `Default` is kept whole instead of being rebuilt and losing the nested
//!   ignored state. A field whose reflected parts all clone is still the concrete `Default`, so a
//!   field-level `#[reflect(default = path)]` that differs from `Default` does not replace it.
//! - **Map keys and set items** load only if the loaded key reflects `Hash` and equals itself
//!   (`#[reflect(Hash, PartialEq)]`, and no NaN anywhere in it); otherwise loading fails with a
//!   "cannot be loaded" error. The check runs on each loaded key, so opaque keys and keys with
//!   ignored fields are covered. Structs, tuple structs and tuples without serde load as dynamic
//!   values, which do not reflect a hash: such keys need `#[reflect(Deserialize)]`.
//! - **Binary nesting** is bounded: postcard data nested deeper than 128 values (the bound RON
//!   applies to text) fails to load, including recursion inside a type's own serde implementation
//!   that reflection cannot see.
//! - **Reflected nesting** is bounded in both formats: more than 64 registered structs or enums
//!   decoded one inside another (without their own serde) fails to load.
//! - **Unknown fields** are skipped with a warning (forward compatibility), except on a type decoded
//!   by its own serde (`#[reflect(Deserialize)]` / `DeserializeWithRegistry`), which does not warn.
//!   In postcard, which is positional, the layout must match exactly: binary data is rebuilt when
//!   types change.
//! - **Unknown types** at the top level follow [`UnknownTypePolicy`]: skipped with a warning
//!   (runtime) or preserved as raw RON text in an [`OpaqueValue`] (editor), so data from an unloaded
//!   game library survives a re-save. Binary data cannot skip an unknown type and fails to load.
//!   Unknown types nested inside known values cannot occur (field types are static). An ambiguous
//!   short type path (two registrations share the short name, so the short-path map has neither) is
//!   not an unregistered type: the warning says `ambiguous short type path` and the value is skipped
//!   or preserved under the same policy. The full type path still loads.
//! - **Entities**: with an [`EntityIdMapper`], every `Entity` the codec walks (a field, list, map or
//!   option whose value is itself an `Entity`) is written as a stable `u128` id (32 hex digits in RON)
//!   and mapped back on load. Ordering a map or a set serializes the keys, and the map values only
//!   when two keys tie, before the write; those probes call [`EntityIdMapper::entity_id_for_order`]
//!   and do not count as misses.
//!   The write calls [`EntityIdMapper::entity_to_id`] once for each entity it emits, including a
//!   repeat of the same entity. A `#[reflect(Serialize)]` / `#[reflect(Deserialize)]` type is not walked,
//!   so `Entity` fields inside it stay raw bits even when a mapper is set. Without a mapper, entities
//!   use their raw bits (`Entity::to_bits()`), valid only within one process (hot-reload snapshots).

mod de;
mod depth;
mod ser;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use bevy_ecs::entity::Entity;
use bevy_reflect::serde::{ReflectSerializer, TypedReflectDeserializer, TypedReflectSerializer};
use bevy_reflect::std_traits::ReflectDefault;
use bevy_reflect::{
    FromReflect, PartialReflect, Reflect, ReflectFromReflect, TypePath, TypeRegistration, TypeRegistry,
};
use serde::de::DeserializeSeed;
use serde::ser::{SerializeMap, Serializer};

use de::{Lenient, TopLevel, TopLevelSeed};
use ser::CanonicalSer;

/// Errors from reflected (de)serialization.
#[derive(Debug, thiserror::Error)]
pub enum SerializeError {
    /// RON writing or value error.
    #[error("RON: {0}")]
    Ron(#[from] ron::Error),
    /// RON parse error with position.
    #[error("RON: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// postcard error.
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    /// Bytes left after the value.
    #[error("postcard: {0} trailing bytes after the value")]
    TrailingBytes(usize),
    /// A value without type info (a dynamic value with no represented type).
    #[error("value of `{0}` has no type info")]
    NoTypeInfo(String),
    /// The loaded data could not be turned into the concrete type.
    #[error("cannot construct `{type_path}`: {reason}")]
    Construct {
        /// The type.
        type_path: String,
        /// Why.
        reason: String,
    },
    /// The value is not of the requested type.
    #[error("expected `{expected}`, found `{found}`")]
    WrongType {
        /// Requested type.
        expected: &'static str,
        /// Type in the data.
        found: String,
    },
}

/// What to do with a value whose type path is not registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownTypePolicy {
    /// Skip it with a warning (runtime).
    #[default]
    Skip,
    /// Keep its RON text in an [`OpaqueValue`] so it can be written back unchanged (editor).
    Preserve,
}

/// A value of an unregistered type, kept as RON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueValue {
    /// Type path as written in the data.
    pub type_path: String,
    /// The value's RON text, with surrounding whitespace trimmed.
    pub ron: String,
}

impl OpaqueValue {
    /// Writes the value back in the type-path-keyed form.
    pub fn to_ron(&self) -> Result<String, SerializeError> {
        let raw = ron::value::RawValue::from_ron(&self.ron)?;
        let mut out = String::new();
        let mut serializer = ron::Serializer::new(&mut out, Some(canonical_pretty_config()))?;
        let mut map = (&mut serializer).serialize_map(Some(1))?;
        map.serialize_entry(&self.type_path, raw)?;
        SerializeMap::end(map)?;
        out.push('\n');
        Ok(out)
    }
}

/// Result of loading a type-path-keyed value.
#[derive(Debug)]
pub enum DeserializedValue {
    /// A registered type, as a concrete value.
    Value(Box<dyn Reflect>),
    /// An unregistered type kept under [`UnknownTypePolicy::Preserve`].
    Opaque(OpaqueValue),
    /// An unregistered type dropped under [`UnknownTypePolicy::Skip`].
    Skipped {
        /// Type path as written in the data.
        type_path: String,
    },
}

impl DeserializedValue {
    /// The concrete value, if the type was known.
    pub fn into_value(self) -> Option<Box<dyn Reflect>> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }
}

/// Maps entities to stable ids (an `EntityGuid`'s `u128`) and back while (de)serializing.
pub trait EntityIdMapper {
    /// The stable id to write for `entity`.
    fn entity_to_id(&mut self, entity: Entity) -> u128;
    /// The entity to use for a stable id read from data.
    fn id_to_entity(&mut self, id: u128) -> Entity;
    /// Stable id used while computing canonical order, not while writing the value.
    ///
    /// Ordering serializes each key, and — only when two keys tie on their canonical forms —
    /// each map value, before the write. Those probes must not be counted as misses and must not
    /// assign a new id: the id in the file is the one [`Self::entity_to_id`] returns for the
    /// write. The default calls `entity_to_id`. An override must return the same id the write
    /// returns for `entity`; returning a different id (for example `0` for an unmapped entity
    /// when the write assigns a fresh id) makes the canonical order disagree with the ids the
    /// file records. Override only to suppress miss counting or id assignment during a probe, as
    /// [`EntityIdMap`] does — never to invent or substitute ids.
    fn entity_id_for_order(&mut self, entity: Entity) -> u128 {
        self.entity_to_id(entity)
    }
}

/// A two-way table implementing [`EntityIdMapper`]. Unknown entities are written as id `0`;
/// unknown ids (and `0`) load as [`Entity::PLACEHOLDER`]. Both are counted in [`EntityIdMap::misses`].
#[derive(Debug, Clone, Default)]
pub struct EntityIdMap {
    to_id: HashMap<Entity, u128>,
    to_entity: HashMap<u128, Entity>,
    misses: usize,
}

impl EntityIdMap {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }
    /// Associates an entity with a stable id (both directions), keeping the table one-to-one: any
    /// earlier pairing of `entity` or of `id` is replaced. Id `0` is the miss token written for
    /// unmapped entities, so it cannot be assigned: returns `false` and changes nothing.
    pub fn insert(&mut self, entity: Entity, id: u128) -> bool {
        if id == 0 {
            return false;
        }
        if let Some(old_id) = self.to_id.insert(entity, id) {
            self.to_entity.remove(&old_id);
        }
        if let Some(old_entity) = self.to_entity.insert(id, entity)
            && old_entity != entity
        {
            self.to_id.remove(&old_entity);
        }
        true
    }
    /// Number of lookups that found nothing.
    pub fn misses(&self) -> usize {
        self.misses
    }
}

impl EntityIdMapper for EntityIdMap {
    fn entity_to_id(&mut self, entity: Entity) -> u128 {
        self.to_id.get(&entity).copied().unwrap_or_else(|| {
            self.misses += 1;
            0
        })
    }
    fn id_to_entity(&mut self, id: u128) -> Entity {
        match self.to_entity.get(&id) {
            Some(entity) if id != 0 => *entity,
            _ => {
                self.misses += 1;
                Entity::PLACEHOLDER
            }
        }
    }
    fn entity_id_for_order(&mut self, entity: Entity) -> u128 {
        self.to_id.get(&entity).copied().unwrap_or(0)
    }
}

/// The one RON layout Regia Engine writes: 4-space indent, `\n` newlines, no struct names, one item per
/// line for structs, maps, arrays and sets (tuples stay on one line), escaped strings.
pub fn canonical_pretty_config() -> ron::ser::PrettyConfig {
    regia_core::ron_pretty()
        .indentor("    ")
        .new_line("\n")
        .separator(" ")
        .struct_names(false)
        .separate_tuple_members(false)
        .enumerate_arrays(false)
        .compact_arrays(false)
        .compact_structs(false)
        .compact_maps(false)
        .escape_strings(true)
        .number_suffixes(false)
}

/// Reflected (de)serializer with options. The free functions ([`to_ron`], [`from_ron`],
/// [`to_postcard`], [`from_postcard`]) use the defaults.
pub struct ReflectCodec<'a> {
    registry: &'a TypeRegistry,
    unknown_types: UnknownTypePolicy,
    mapper: Option<&'a mut dyn EntityIdMapper>,
    warnings: Vec<String>,
}

impl<'a> ReflectCodec<'a> {
    /// A codec over `registry` that skips unknown types and writes entities as raw bits.
    pub fn new(registry: &'a TypeRegistry) -> Self {
        Self {
            registry,
            unknown_types: UnknownTypePolicy::Skip,
            mapper: None,
            warnings: Vec::new(),
        }
    }

    /// Sets the unknown-type policy.
    pub fn unknown_types(mut self, policy: UnknownTypePolicy) -> Self {
        self.unknown_types = policy;
        self
    }

    /// Maps entities through `mapper` in both directions.
    pub fn entity_mapper(mut self, mapper: &'a mut dyn EntityIdMapper) -> Self {
        self.mapper = Some(mapper);
        self
    }

    /// Warnings collected so far (unknown fields and types), also logged through `tracing`.
    ///
    /// At most 256 warnings are kept per decode; the 257th entry is the single line
    /// `further warnings suppressed after 256`, and later ones are dropped.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Takes the collected warnings.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn ser(&mut self) -> CanonicalSer<'_, 'a> {
        CanonicalSer {
            mapper: self.mapper.as_deref_mut().map(RefCell::new),
            ordering: Cell::new(false),
        }
    }

    /// Canonical RON of `value` under its type path.
    pub fn to_ron(&mut self, value: &dyn PartialReflect) -> Result<String, SerializeError> {
        type_info_of(value)?;
        let registry = self.registry;
        let processor = self.ser();
        write_ron(&ReflectSerializer::with_processor(value, registry, &processor))
    }

    /// Canonical RON of `value` alone (type known from context).
    pub fn to_ron_typed(&mut self, value: &dyn PartialReflect) -> Result<String, SerializeError> {
        let registry = self.registry;
        let processor = self.ser();
        write_ron(&TypedReflectSerializer::with_processor(
            value, registry, &processor,
        ))
    }

    /// postcard bytes of `value` under its type path.
    pub fn to_postcard(&mut self, value: &dyn PartialReflect) -> Result<Vec<u8>, SerializeError> {
        type_info_of(value)?;
        let registry = self.registry;
        let processor = self.ser();
        Ok(postcard::to_stdvec(&ReflectSerializer::with_processor(
            value, registry, &processor,
        ))?)
    }

    /// postcard bytes of `value` alone.
    pub fn to_postcard_typed(&mut self, value: &dyn PartialReflect) -> Result<Vec<u8>, SerializeError> {
        let registry = self.registry;
        let processor = self.ser();
        Ok(postcard::to_stdvec(&TypedReflectSerializer::with_processor(
            value, registry, &processor,
        ))?)
    }

    /// Loads a type-path-keyed RON value.
    pub fn from_ron(&mut self, text: &str) -> Result<DeserializedValue, SerializeError> {
        let registry = self.registry;
        let policy = self.unknown_types;
        let warn_base = self.warnings.len();
        let mut processor = Lenient {
            mapper: self.mapper.as_deref_mut(),
            warnings: &mut self.warnings,
            depth: 0,
            warn_base,
        };
        let mut deserializer = ron::de::Deserializer::from_str(text)?;
        let top = TopLevelSeed {
            registry,
            processor: &mut processor,
            policy,
            text: true,
        }
        .deserialize(&mut deserializer)
        .map_err(|e| deserializer.span_error(e))?;
        deserializer.end().map_err(|e| deserializer.span_error(e))?;
        self.finish_top(top)
    }

    /// Loads a bare RON value of the registered type `registration`.
    pub fn from_ron_typed(
        &mut self,
        text: &str,
        registration: &TypeRegistration,
    ) -> Result<Box<dyn Reflect>, SerializeError> {
        let registry = self.registry;
        let warn_base = self.warnings.len();
        let mut processor = Lenient {
            mapper: self.mapper.as_deref_mut(),
            warnings: &mut self.warnings,
            depth: 0,
            warn_base,
        };
        let mut deserializer = ron::de::Deserializer::from_str(text)?;
        let value = TypedReflectDeserializer::with_processor(registration, registry, &mut processor)
            .deserialize(&mut deserializer)
            .map_err(|e| deserializer.span_error(e))?;
        deserializer.end().map_err(|e| deserializer.span_error(e))?;
        into_concrete(value, registration)
    }

    /// Loads type-path-keyed postcard bytes. Unknown types are an error in binary data.
    pub fn from_postcard(&mut self, bytes: &[u8]) -> Result<Box<dyn Reflect>, SerializeError> {
        let registry = self.registry;
        let policy = self.unknown_types;
        let warn_base = self.warnings.len();
        let mut processor = Lenient {
            mapper: self.mapper.as_deref_mut(),
            warnings: &mut self.warnings,
            depth: 0,
            warn_base,
        };
        let mut deserializer = postcard::Deserializer::from_bytes(bytes);
        let top = TopLevelSeed {
            registry,
            processor: &mut processor,
            policy,
            text: false,
        }
        .deserialize(depth::Limited::new(&mut deserializer))?;
        check_finished(deserializer)?;
        match self.finish_top(top)? {
            DeserializedValue::Value(value) => Ok(value),
            DeserializedValue::Opaque(OpaqueValue { type_path, .. })
            | DeserializedValue::Skipped { type_path } => Err(SerializeError::Construct {
                type_path,
                reason: "type is not registered".into(),
            }),
        }
    }

    /// Loads bare postcard bytes of the registered type `registration`.
    pub fn from_postcard_typed(
        &mut self,
        bytes: &[u8],
        registration: &TypeRegistration,
    ) -> Result<Box<dyn Reflect>, SerializeError> {
        let registry = self.registry;
        let warn_base = self.warnings.len();
        let mut processor = Lenient {
            mapper: self.mapper.as_deref_mut(),
            warnings: &mut self.warnings,
            depth: 0,
            warn_base,
        };
        let mut deserializer = postcard::Deserializer::from_bytes(bytes);
        let value = TypedReflectDeserializer::with_processor(registration, registry, &mut processor)
            .deserialize(depth::Limited::new(&mut deserializer))?;
        check_finished(deserializer)?;
        into_concrete(value, registration)
    }

    /// Loads a type-path-keyed RON value that must be a `T`.
    pub fn from_ron_as<T: Reflect + TypePath>(&mut self, text: &str) -> Result<T, SerializeError> {
        match self.from_ron(text)? {
            DeserializedValue::Value(value) => take::<T>(value),
            DeserializedValue::Opaque(OpaqueValue { type_path, .. })
            | DeserializedValue::Skipped { type_path } => Err(SerializeError::WrongType {
                expected: T::type_path(),
                found: type_path,
            }),
        }
    }

    fn finish_top(&mut self, top: TopLevel) -> Result<DeserializedValue, SerializeError> {
        Ok(match top {
            TopLevel::Known(type_path, value) => {
                let registration =
                    self.registry
                        .get_with_type_path(type_path)
                        .ok_or_else(|| SerializeError::Construct {
                            type_path: type_path.to_string(),
                            reason: "type is not registered".into(),
                        })?;
                DeserializedValue::Value(into_concrete(value, registration)?)
            }
            TopLevel::Opaque(opaque) => DeserializedValue::Opaque(opaque),
            TopLevel::Skipped(type_path) => DeserializedValue::Skipped { type_path },
        })
    }
}

fn type_info_of(value: &dyn PartialReflect) -> Result<(), SerializeError> {
    match value.get_represented_type_info() {
        Some(_) => Ok(()),
        None => Err(SerializeError::NoTypeInfo(value.reflect_type_path().to_string())),
    }
}

fn write_ron<T: serde::Serialize>(value: &T) -> Result<String, SerializeError> {
    let mut out = ron::ser::to_string_pretty(value, canonical_pretty_config())?;
    out.push('\n');
    Ok(out)
}

fn check_finished<'a>(
    deserializer: postcard::Deserializer<'a, postcard::de_flavors::Slice<'a>>,
) -> Result<(), SerializeError> {
    let rest = deserializer.finalize()?;
    if rest.is_empty() {
        Ok(())
    } else {
        Err(SerializeError::TrailingBytes(rest.len()))
    }
}

fn take<T: Reflect + TypePath>(value: Box<dyn Reflect>) -> Result<T, SerializeError> {
    value
        .downcast::<T>()
        .map(|v| *v)
        .map_err(|v| SerializeError::WrongType {
            expected: T::type_path(),
            found: v.reflect_type_path().to_string(),
        })
}

/// Turns a deserialized (usually dynamic) value into the concrete registered type: directly if it
/// already is one, else through `FromReflect`, else `Default` + apply.
pub fn into_concrete(
    value: Box<dyn PartialReflect>,
    registration: &TypeRegistration,
) -> Result<Box<dyn Reflect>, SerializeError> {
    let type_path = registration.type_info().type_path();
    let value = match value.try_into_reflect() {
        Ok(concrete) if concrete.type_id() == registration.type_id() => return Ok(concrete),
        Ok(other) => other.into_partial_reflect(),
        Err(value) => value,
    };
    if let Some(from) = registration.data::<ReflectFromReflect>()
        && let Some(concrete) = from.from_reflect(value.as_partial_reflect())
    {
        return Ok(concrete);
    }
    let Some(default) = registration.data::<ReflectDefault>() else {
        return Err(SerializeError::Construct {
            type_path: type_path.to_string(),
            reason: "data does not match and the type registers neither FromReflect nor Default".into(),
        });
    };
    let mut concrete = default.default();
    concrete
        .try_apply(value.as_partial_reflect())
        .map_err(|e| SerializeError::Construct {
            type_path: type_path.to_string(),
            reason: e.to_string(),
        })?;
    Ok(concrete)
}

/// Canonical RON of `value` under its type path.
pub fn to_ron(value: &dyn PartialReflect, registry: &TypeRegistry) -> Result<String, SerializeError> {
    ReflectCodec::new(registry).to_ron(value)
}

/// Loads a type-path-keyed RON value; unknown types are skipped with a warning.
pub fn from_ron(text: &str, registry: &TypeRegistry) -> Result<DeserializedValue, SerializeError> {
    ReflectCodec::new(registry).from_ron(text)
}

/// postcard bytes of `value` under its type path.
pub fn to_postcard(value: &dyn PartialReflect, registry: &TypeRegistry) -> Result<Vec<u8>, SerializeError> {
    ReflectCodec::new(registry).to_postcard(value)
}

/// Loads type-path-keyed postcard bytes.
pub fn from_postcard(bytes: &[u8], registry: &TypeRegistry) -> Result<Box<dyn Reflect>, SerializeError> {
    ReflectCodec::new(registry).from_postcard(bytes)
}

/// Loads a `T` from a value built with [`to_ron`] (convenience for tests and tools).
pub fn from_ron_as<T: Reflect + TypePath + FromReflect>(
    text: &str,
    registry: &TypeRegistry,
) -> Result<T, SerializeError> {
    ReflectCodec::new(registry).from_ron_as::<T>(text)
}

#[cfg(test)]
mod tests;
