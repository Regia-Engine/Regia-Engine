// SPDX-License-Identifier: MPL-2.0
//! Deserialization side: lenient structs and enum struct variants (unknown fields ignored with a
//! warning, missing fields filled from defaults), stable id → `Entity`, unknown top-level types.

use std::any::TypeId;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Mutex, OnceLock};

use bevy_ecs::entity::Entity;
use bevy_reflect::enums::{DynamicEnum, DynamicVariant, EnumInfo, VariantInfo};
use bevy_reflect::map::{DynamicMap, Map};
use bevy_reflect::serde::{
    ReflectDeserializeWithRegistry, ReflectDeserializerProcessor, SerializationData, TypedReflectDeserializer,
};
use bevy_reflect::set::{DynamicSet, Set};
use bevy_reflect::std_traits::ReflectDefault;
use bevy_reflect::structs::{DynamicStruct, Struct, StructInfo};
use bevy_reflect::tuple::DynamicTuple;
use bevy_reflect::{
    NamedField, PartialReflect, Reflect, ReflectDeserialize, ReflectFromReflect, ReflectRef, TypeInfo,
    TypeRegistration, TypeRegistry, UnnamedField,
};
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, Error as _, IgnoredAny, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use super::{EntityIdMapper, OpaqueValue, UnknownTypePolicy};

/// Deserializer processor carrying the lenient rules.
pub(super) struct Lenient<'w, 'm, 'a> {
    pub mapper: Option<&'m mut (dyn EntityIdMapper + 'a)>,
    pub warnings: &'w mut Vec<String>,
    /// Struct/enum nesting currently being decoded (bounded by [`MAX_DEPTH`]).
    pub depth: u32,
    /// Length of `warnings` when this decode started: the cap applies to this decode only.
    pub warn_base: usize,
}

/// Deepest struct/enum nesting a decode accepts. RON enforces its own limit; this bounds postcard,
/// which has none, so a recursive registered type cannot overflow the stack.
pub(super) const MAX_DEPTH: u32 = 64;

/// Most unknown-field warnings kept per decode; further ones are counted in a final summary line.
pub(super) const MAX_WARNINGS: usize = 256;

impl Lenient<'_, '_, '_> {
    fn warn(&mut self, message: String) {
        let added = self.warnings.len().saturating_sub(self.warn_base);
        if added < MAX_WARNINGS {
            tracing::warn!(target: "regia_ecs::serialize", "{message}");
            self.warnings.push(message);
        } else if added == MAX_WARNINGS {
            let message = format!("further warnings suppressed after {MAX_WARNINGS}");
            tracing::warn!(target: "regia_ecs::serialize", "{message}");
            self.warnings.push(message);
        }
    }
}

fn is_option(info: &EnumInfo) -> bool {
    let table = info.type_path_table();
    table.module_path() == Some("core::option") && table.ident() == Some("Option")
}

fn registration_of<'r, E: de::Error>(
    registry: &'r TypeRegistry,
    type_id: TypeId,
    type_path: &str,
) -> Result<&'r TypeRegistration, E> {
    registry
        .get(type_id)
        .ok_or_else(|| E::custom(format_args!("type `{type_path}` is not registered")))
}

impl ReflectDeserializerProcessor for Lenient<'_, '_, '_> {
    fn try_deserialize<'de, D>(
        &mut self,
        registration: &TypeRegistration,
        registry: &TypeRegistry,
        deserializer: D,
    ) -> Result<Result<Box<dyn PartialReflect>, D>, D::Error>
    where
        D: Deserializer<'de>,
    {
        if registration.type_id() == TypeId::of::<Entity>()
            && let Some(mapper) = self.mapper.as_deref_mut()
        {
            let id = if deserializer.is_human_readable() {
                deserializer.deserialize_any(StableIdVisitor)?
            } else {
                deserializer.deserialize_u128(StableIdVisitor)?
            };
            return Ok(Ok(Box::new(mapper.id_to_entity(id))));
        }
        if !deserializer.is_human_readable() && unbounded_recursion(registry, registration) {
            return Err(D::Error::custom(format_args!(
                "type `{}` nests itself through a path the depth limit cannot count; it cannot be decoded from a binary stream",
                registration.type_info().type_path()
            )));
        }
        if has_serde(registration) {
            return Ok(Err(deserializer));
        }
        if self.depth >= MAX_DEPTH
            && matches!(registration.type_info(), TypeInfo::Struct(_) | TypeInfo::Enum(_))
        {
            return Err(D::Error::custom(format_args!(
                "nesting deeper than {MAX_DEPTH} levels at `{}`",
                registration.type_info().type_path()
            )));
        }
        match registration.type_info() {
            TypeInfo::Map(info) => {
                let key = registration_of::<D::Error>(registry, info.key_ty().id(), info.key_ty().path())?;
                let value =
                    registration_of::<D::Error>(registry, info.value_ty().id(), info.value_ty().path())?;
                let mut map = deserializer.deserialize_map(MapVisitor {
                    key,
                    value,
                    registry,
                    processor: self,
                })?;
                map.set_represented_type(Some(registration.type_info()));
                Ok(Ok(Box::new(map)))
            }
            TypeInfo::Set(info) => {
                let item =
                    registration_of::<D::Error>(registry, info.value_ty().id(), info.value_ty().path())?;
                let mut set = deserializer.deserialize_seq(SetVisitor {
                    item,
                    registry,
                    processor: self,
                })?;
                set.set_represented_type(Some(registration.type_info()));
                Ok(Ok(Box::new(set)))
            }
            TypeInfo::Struct(info) => {
                let ident = info.type_path_table().ident().unwrap_or("Struct");
                self.depth += 1;
                let result = deserializer.deserialize_struct(
                    ident,
                    info.field_names(),
                    StructVisitor {
                        fields: info,
                        registration: Some(registration),
                        registry,
                        processor: self,
                    },
                );
                self.depth -= 1;
                let mut value = result?;
                value.set_represented_type(Some(registration.type_info()));
                Ok(Ok(Box::new(value)))
            }
            TypeInfo::Enum(info) if !is_option(info) => {
                let ident = info.type_path_table().ident().unwrap_or("Enum");
                self.depth += 1;
                let result = deserializer.deserialize_enum(
                    ident,
                    info.variant_names(),
                    EnumVisitor {
                        info,
                        registry,
                        processor: self,
                    },
                );
                self.depth -= 1;
                let mut value = result?;
                value.set_represented_type(Some(registration.type_info()));
                Ok(Ok(Box::new(value)))
            }
            _ => Ok(Err(deserializer)),
        }
    }
}

/// Phrase for a top-level path that did not resolve. An ambiguous short name is not "not registered":
/// both types are in the registry, and only the full path selects one.
fn unresolved_type_phrase(registry: &TypeRegistry, type_path: &str) -> String {
    if registry.is_ambiguous(type_path) {
        format!("ambiguous short type path `{type_path}`")
    } else {
        format!("type `{type_path}` is not registered")
    }
}

fn has_serde(registration: &TypeRegistration) -> bool {
    registration.data::<ReflectDeserialize>().is_some()
        || registration.data::<ReflectDeserializeWithRegistry>().is_some()
}

/// Types a decoded value can hold, for the recursion walk.
fn child_types(info: &TypeInfo) -> Vec<TypeId> {
    match info {
        TypeInfo::Struct(i) => i.iter().map(|f| f.ty().id()).collect(),
        TypeInfo::TupleStruct(i) => i.iter().map(|f| f.ty().id()).collect(),
        TypeInfo::Tuple(i) => i.iter().map(|f| f.ty().id()).collect(),
        TypeInfo::Enum(i) => {
            let mut ids = Vec::new();
            for variant in i.iter() {
                match variant {
                    VariantInfo::Struct(fields) => ids.extend(fields.iter().map(|field| field.ty().id())),
                    VariantInfo::Tuple(fields) => ids.extend(fields.iter().map(|field| field.ty().id())),
                    VariantInfo::Unit(_) => {}
                }
            }
            ids
        }
        TypeInfo::List(i) => vec![i.item_ty().id()],
        TypeInfo::Array(i) => vec![i.item_ty().id()],
        TypeInfo::Map(i) => vec![i.key_ty().id(), i.value_ty().id()],
        TypeInfo::Set(i) => vec![i.value_ty().id()],
        _ => Vec::new(),
    }
}

fn cycle_cache() -> &'static Mutex<HashMap<TypeId, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<TypeId, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_cycle(id: TypeId) -> Option<bool> {
    let cache = cycle_cache().lock().unwrap_or_else(|err| err.into_inner());
    cache.get(&id).copied()
}

fn store_cycle(id: TypeId, cyclic: bool) {
    let mut cache = cycle_cache().lock().unwrap_or_else(|err| err.into_inner());
    cache.insert(id, cyclic);
}

/// True when decoding `registration` can recurse without the processor's depth counter seeing it:
/// serde-decoded types (their whole subtree is decoded by serde) that can reach a cycle, and
/// tuple structs, tuples, lists, arrays, maps and sets that sit on a cycle made only of such types.
/// Registered structs and enums without serde count their own levels (`MAX_DEPTH`).
///
/// An opaque type has no reflected children, so this walk cannot see a cycle through it and returns
/// before allocating. A finished answer is remembered for the process when every child type on the
/// walk was registered: `TypeInfo` is static, so a later registry that also has those types gets
/// the same answer. A walk that skipped an unregistered child is not stored.
fn unbounded_recursion(registry: &TypeRegistry, registration: &TypeRegistration) -> bool {
    if matches!(registration.type_info(), TypeInfo::Opaque(_)) {
        return false;
    }
    let serde = has_serde(registration);
    let uncounted_container = matches!(
        registration.type_info(),
        TypeInfo::TupleStruct(_)
            | TypeInfo::Tuple(_)
            | TypeInfo::List(_)
            | TypeInfo::Array(_)
            | TypeInfo::Map(_)
            | TypeInfo::Set(_)
    );
    if !serde && !uncounted_container {
        return false;
    }
    note_cycle_check();
    let id = registration.type_id();
    if let Some(cyclic) = cached_cycle(id) {
        return cyclic;
    }
    note_cycle_walk();
    let (cyclic, closed) = cycle_walk(registry, registration, serde);
    if closed {
        store_cycle(id, cyclic);
    }
    cyclic
}

/// Iterative DFS. `closed` is false when a child type id was not in `registry`.
fn cycle_walk(registry: &TypeRegistry, registration: &TypeRegistration, serde: bool) -> (bool, bool) {
    let mut done: HashSet<TypeId> = HashSet::new();
    let mut path: Vec<(TypeId, Vec<TypeId>)> =
        vec![(registration.type_id(), child_types(registration.type_info()))];
    let mut closed = true;
    while let Some((_, children)) = path.last_mut() {
        let Some(next) = children.pop() else {
            if let Some((finished, _)) = path.pop() {
                done.insert(finished);
            }
            continue;
        };
        if path.iter().any(|(id, _)| *id == next) {
            return (true, closed);
        }
        if done.contains(&next) {
            continue;
        }
        let Some(child) = registry.get(next) else {
            closed = false;
            continue;
        };
        // Without serde, a counted struct or enum below the start type bounds the walk.
        if !serde && !has_serde(child) && matches!(child.type_info(), TypeInfo::Struct(_) | TypeInfo::Enum(_))
        {
            continue;
        }
        path.push((next, child_types(child.type_info())));
    }
    (false, closed)
}

/// Checks a decoded map key or set item before it goes into a `DynamicMap`/`DynamicSet`.
/// `bevy_reflect` inserts by reflected hash and asserts that the key equals itself; a key with no
/// reflected hash, or one that is not reflexively equal (a NaN anywhere in it, including a float
/// that reflection cannot see), would panic there, which aborts the process (`panic = abort`).
/// The check runs on the value itself, so it also covers opaque types and ignored fields.
fn check_key<E: de::Error>(key: &dyn PartialReflect, ty: &TypeRegistration, what: &str) -> Result<(), E> {
    if key.reflect_hash().is_some() && key.reflect_partial_eq(key) == Some(true) {
        return Ok(());
    }
    Err(E::custom(format_args!(
        "{what} type `{}` cannot be loaded: it needs `#[reflect(Hash, PartialEq)]` with serde support \
         (`#[reflect(Deserialize)]`), and every key must equal itself (a NaN never does)",
        ty.type_info().type_path()
    )))
}

/// A reflected map, with every key checked by [`check_key`] before insertion.
struct MapVisitor<'a, 'p, 'w, 'm, 'x> {
    key: &'a TypeRegistration,
    value: &'a TypeRegistration,
    registry: &'a TypeRegistry,
    processor: &'p mut Lenient<'w, 'm, 'x>,
}

impl<'de> Visitor<'de> for MapVisitor<'_, '_, '_, '_, '_> {
    type Value = DynamicMap;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a reflected map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<DynamicMap, A::Error> {
        let mut out = DynamicMap::default();
        while let Some(key) = map.next_key_seed(TypedReflectDeserializer::with_processor(
            self.key,
            self.registry,
            &mut *self.processor,
        ))? {
            check_key::<A::Error>(&*key, self.key, "map key")?;
            let value = map.next_value_seed(TypedReflectDeserializer::with_processor(
                self.value,
                self.registry,
                &mut *self.processor,
            ))?;
            out.insert_boxed(key, value);
        }
        Ok(out)
    }
}

/// A reflected set, with every item checked by [`check_key`] before insertion.
struct SetVisitor<'a, 'p, 'w, 'm, 'x> {
    item: &'a TypeRegistration,
    registry: &'a TypeRegistry,
    processor: &'p mut Lenient<'w, 'm, 'x>,
}

impl<'de> Visitor<'de> for SetVisitor<'_, '_, '_, '_, '_> {
    type Value = DynamicSet;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a reflected set")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<DynamicSet, A::Error> {
        let mut out = DynamicSet::default();
        while let Some(item) = seq.next_element_seed(TypedReflectDeserializer::with_processor(
            self.item,
            self.registry,
            &mut *self.processor,
        ))? {
            check_key::<A::Error>(&*item, self.item, "set item")?;
            out.insert_boxed(item);
        }
        Ok(out)
    }
}

/// Reads an entity's stable id: a 32-digit hex string (RON) or a `u128` (binary).
struct StableIdVisitor;

impl Visitor<'_> for StableIdVisitor {
    type Value = u128;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a stable entity id (hex string or u128)")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<u128, E> {
        u128::from_str_radix(v, 16).map_err(|e| E::custom(format_args!("bad entity id `{v}`: {e}")))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<u128, E> {
        Ok(u128::from(v))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<u128, E> {
        Ok(v)
    }
}

/// A field identifier.
struct Ident(String);

impl<'de> de::Deserialize<'de> for Ident {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdentVisitor;
        impl Visitor<'_> for IdentVisitor {
            type Value = Ident;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a field name")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Ident, E> {
                Ok(Ident(v.to_string()))
            }
        }
        deserializer.deserialize_identifier(IdentVisitor)
    }
}

/// Struct-like field list: a struct or a struct variant.
trait Fields {
    fn path(&self) -> &str;
    fn field_named(&self, name: &str) -> Option<&NamedField>;
    fn fields(&self) -> &[NamedField];
}

impl Fields for StructInfo {
    fn path(&self) -> &str {
        self.type_path()
    }
    fn field_named(&self, name: &str) -> Option<&NamedField> {
        self.field(name)
    }
    fn fields(&self) -> &[NamedField] {
        self.iter().as_slice()
    }
}

impl Fields for bevy_reflect::enums::StructVariantInfo {
    fn path(&self) -> &str {
        self.name()
    }
    fn field_named(&self, name: &str) -> Option<&NamedField> {
        self.field(name)
    }
    fn fields(&self) -> &[NamedField] {
        self.iter().as_slice()
    }
}

struct StructVisitor<'a, 'p, 'w, 'm, 'x, F: Fields + ?Sized> {
    fields: &'a F,
    /// The struct's own registration (`None` for enum variants).
    registration: Option<&'a TypeRegistration>,
    registry: &'a TypeRegistry,
    processor: &'p mut Lenient<'w, 'm, 'x>,
}

impl<F: Fields + ?Sized> StructVisitor<'_, '_, '_, '_, '_, F> {
    fn skipped(&self, index: usize) -> bool {
        self.registration
            .and_then(|r| r.data::<SerializationData>())
            .is_some_and(|d| d.is_field_skipped(index))
    }

    /// Fills every field the input did not provide: serialization-skipped fields from their
    /// declared default, others from the container's `Default`, then the field type's `Default`.
    ///
    /// A value that already holds every field (the postcard path for a type with no skipped
    /// field) returns without constructing the container `Default`.
    fn fill_missing<E: de::Error>(self, mut out: DynamicStruct) -> Result<DynamicStruct, E> {
        if self
            .fields
            .fields()
            .iter()
            .all(|field| out.field(field.name()).is_some())
        {
            return Ok(out);
        }
        let container_default = self
            .registration
            .and_then(|r| r.data::<ReflectDefault>())
            .map(ReflectDefault::default);
        let serialization = self.registration.and_then(|r| r.data::<SerializationData>());
        for (index, field) in self.fields.fields().iter().enumerate() {
            let name = field.name();
            if out.field(name).is_some() {
                continue;
            }
            if let Some(value) = serialization.and_then(|d| d.generate_default(index)) {
                out.insert_boxed(name, value.into_partial_reflect());
                continue;
            }
            if !self.skipped(index) {
                tracing::debug!(
                    target: "regia_ecs::serialize",
                    "missing field `{name}` on `{}` filled with its default",
                    self.fields.path()
                );
            }
            let from_container = container_default.as_deref().and_then(|d| match d.reflect_ref() {
                ReflectRef::Struct(s) => s.field(name),
                _ => None,
            });
            if let Some(field_default) = from_container {
                // A field that will not clone (its type holds `#[reflect(ignore)]` state) is left
                // out: the container is `#[reflect(Default)]`, so `FromReflect` (or `Default` +
                // apply) starts from the container's `Default` and keeps this field whole. A
                // dynamic copy would drop the ignored state and rebuild it from the field
                // type's own `Default` instead.
                if let Ok(value) = field_default.reflect_clone() {
                    out.insert_boxed(name, value.into_partial_reflect());
                }
                continue;
            }
            let ty = field.ty();
            let registration = registration_of::<E>(self.registry, ty.id(), ty.path())?;
            let Some(default) = registration.data::<ReflectDefault>() else {
                return Err(E::custom(format_args!(
                    "missing field `{name}` on `{}` and neither it nor its type has a registered Default",
                    self.fields.path()
                )));
            };
            out.insert_boxed(name, field_type_default(registration, default));
        }
        Ok(out)
    }
}

/// The field type's `Default`, in a form `FromReflect` can rebuild without dropping nested
/// `#[reflect(ignore)]` state.
///
/// A concrete default is correct when every reflected field clones: `FromReflect` copies those
/// fields, keeps the struct's own ignored fields (they are not reflected), and does not run a
/// field-level `#[reflect(default = path)]` because the field is present. A reflected field that
/// will not clone holds ignored state further down. Inserting the concrete value makes the parent
/// rebuild that field and reset the ignored state to the nested type's `Default`. An empty dynamic
/// struct of the field's type leaves the field out of that rebuild, so `FromReflect` keeps the
/// field type's `Default` whole. The empty struct is used only when that conversion succeeds.
fn field_type_default(registration: &TypeRegistration, default: &ReflectDefault) -> Box<dyn PartialReflect> {
    let concrete = default.default();
    if let Some(shell) = empty_struct_if_nested_ignored(registration, concrete.as_ref()) {
        return shell;
    }
    concrete.into_partial_reflect()
}

/// An empty dynamic struct of `registration`'s type when a reflected field of `concrete` will not
/// `reflect_clone`, and `FromReflect` of that empty struct succeeds. `None` otherwise: the caller
/// inserts `concrete`.
fn empty_struct_if_nested_ignored(
    registration: &TypeRegistration,
    concrete: &dyn Reflect,
) -> Option<Box<dyn PartialReflect>> {
    if !matches!(registration.type_info(), TypeInfo::Struct(_)) {
        return None;
    }
    if !reflected_field_refuses_clone(concrete.as_partial_reflect()) {
        return None;
    }
    let mut shell = DynamicStruct::default();
    shell.set_represented_type(Some(registration.type_info()));
    registration.data::<ReflectFromReflect>()?.from_reflect(&shell)?;
    Some(Box::new(shell))
}

/// A reflected field of `value` (not an ignored field of `value` itself) refuses `reflect_clone`.
fn reflected_field_refuses_clone(value: &dyn PartialReflect) -> bool {
    let ReflectRef::Struct(fields) = value.reflect_ref() else {
        return false;
    };
    fields
        .iter_fields()
        .any(|(_, field)| field.reflect_clone().is_err())
}

impl<'de, F: Fields + ?Sized> Visitor<'de> for StructVisitor<'_, '_, '_, '_, '_, F> {
    type Value = DynamicStruct;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "struct `{}`", self.fields.path())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<DynamicStruct, A::Error> {
        let mut out = DynamicStruct::default();
        while let Some(Ident(key)) = map.next_key::<Ident>()? {
            let Some(field) = self.fields.field_named(&key) else {
                let message = format!("unknown field `{key}` on `{}` ignored", self.fields.path());
                self.processor.warn(message);
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            if out.field(&key).is_some() {
                return Err(A::Error::custom(format_args!("duplicate field `{key}`")));
            }
            let ty = field.ty();
            let registration = registration_of::<A::Error>(self.registry, ty.id(), ty.path())?;
            let value = map.next_value_seed(TypedReflectDeserializer::with_processor(
                registration,
                self.registry,
                &mut *self.processor,
            ))?;
            out.insert_boxed(&key, value);
        }
        self.fill_missing(out)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<DynamicStruct, A::Error> {
        let mut out = DynamicStruct::default();
        let len = self.fields.fields().len();
        for index in 0..len {
            if self.skipped(index) {
                continue;
            }
            let field = &self.fields.fields()[index];
            let ty = field.ty();
            let registration = registration_of::<A::Error>(self.registry, ty.id(), ty.path())?;
            let value = seq
                .next_element_seed(TypedReflectDeserializer::with_processor(
                    registration,
                    self.registry,
                    &mut *self.processor,
                ))?
                .ok_or_else(|| A::Error::invalid_length(index, &self))?;
            out.insert_boxed(field.name(), value);
        }
        self.fill_missing(out)
    }
}

struct EnumVisitor<'a, 'p, 'w, 'm, 'x> {
    info: &'static EnumInfo,
    registry: &'a TypeRegistry,
    processor: &'p mut Lenient<'w, 'm, 'x>,
}

impl<'de> Visitor<'de> for EnumVisitor<'_, '_, '_, '_, '_> {
    type Value = DynamicEnum;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "enum `{}`", self.info.type_path())
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<DynamicEnum, A::Error> {
        let (variant_info, variant) = data.variant_seed(VariantSeed(self.info))?;
        let value: DynamicVariant = match variant_info {
            VariantInfo::Unit(_) => {
                variant.unit_variant()?;
                DynamicVariant::Unit
            }
            VariantInfo::Struct(fields) => variant
                .struct_variant(
                    fields.field_names(),
                    StructVisitor {
                        fields,
                        registration: None,
                        registry: self.registry,
                        processor: self.processor,
                    },
                )?
                .into(),
            VariantInfo::Tuple(fields) if fields.field_len() == 1 => {
                let field = fields
                    .field_at(0)
                    .ok_or_else(|| A::Error::custom("tuple variant without field 0"))?;
                let registration = unnamed_registration::<A::Error>(self.registry, field)?;
                let value = variant.newtype_variant_seed(TypedReflectDeserializer::with_processor(
                    registration,
                    self.registry,
                    self.processor,
                ))?;
                let mut tuple = DynamicTuple::default();
                tuple.insert_boxed(value);
                tuple.into()
            }
            VariantInfo::Tuple(fields) => variant
                .tuple_variant(
                    fields.field_len(),
                    TupleVisitor {
                        fields: fields.iter().as_slice(),
                        registry: self.registry,
                        processor: self.processor,
                    },
                )?
                .into(),
        };
        let name = variant_info.name();
        let index = self
            .info
            .index_of(name)
            .ok_or_else(|| A::Error::custom(format_args!("no variant `{name}`")))?;
        let mut out = DynamicEnum::default();
        out.set_variant_with_index(index, name, value);
        Ok(out)
    }
}

fn unnamed_registration<'r, E: de::Error>(
    registry: &'r TypeRegistry,
    field: &UnnamedField,
) -> Result<&'r TypeRegistration, E> {
    let ty = field.ty();
    registration_of(registry, ty.id(), ty.path())
}

struct TupleVisitor<'a, 'p, 'w, 'm, 'x> {
    fields: &'a [UnnamedField],
    registry: &'a TypeRegistry,
    processor: &'p mut Lenient<'w, 'm, 'x>,
}

impl<'de> Visitor<'de> for TupleVisitor<'_, '_, '_, '_, '_> {
    type Value = DynamicTuple;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "a tuple of {} fields", self.fields.len())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<DynamicTuple, A::Error> {
        let mut out = DynamicTuple::default();
        for (index, field) in self.fields.iter().enumerate() {
            let registration = unnamed_registration::<A::Error>(self.registry, field)?;
            let value = seq
                .next_element_seed(TypedReflectDeserializer::with_processor(
                    registration,
                    self.registry,
                    &mut *self.processor,
                ))?
                .ok_or_else(|| A::Error::invalid_length(index, &self))?;
            out.insert_boxed(value);
        }
        Ok(out)
    }
}

/// Reads a variant by name (text formats) or index (binary formats).
struct VariantSeed(&'static EnumInfo);

impl<'de> DeserializeSeed<'de> for VariantSeed {
    type Value = &'static VariantInfo;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_identifier(self)
    }
}

impl Visitor<'_> for VariantSeed {
    type Value = &'static VariantInfo;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "a variant of `{}`", self.0.type_path())
    }

    fn visit_u32<E: de::Error>(self, index: u32) -> Result<Self::Value, E> {
        self.0.variant_at(index as usize).ok_or_else(|| {
            E::custom(format_args!(
                "no variant at index {index} on `{}`",
                self.0.type_path()
            ))
        })
    }

    fn visit_u64<E: de::Error>(self, index: u64) -> Result<Self::Value, E> {
        let index = u32::try_from(index).map_err(E::custom)?;
        self.visit_u32(index)
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Self::Value, E> {
        self.0.variant(name).ok_or_else(|| {
            E::custom(format_args!(
                "unknown variant `{name}` on `{}`, expected one of {:?}",
                self.0.type_path(),
                self.0.variant_names()
            ))
        })
    }
}

/// What the top-level map entry held.
pub(super) enum TopLevel {
    Known(&'static str, Box<dyn PartialReflect>),
    Opaque(OpaqueValue),
    Skipped(String),
}

/// The `{ "type::path": value }` wrapper.
pub(super) struct TopLevelSeed<'a, 'p, 'w, 'm, 'x> {
    pub registry: &'a TypeRegistry,
    pub processor: &'p mut Lenient<'w, 'm, 'x>,
    pub policy: UnknownTypePolicy,
    /// True when reading RON (raw values can be preserved).
    pub text: bool,
}

impl<'de> DeserializeSeed<'de> for TopLevelSeed<'_, '_, '_, '_, '_> {
    type Value = TopLevel;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<TopLevel, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for TopLevelSeed<'_, '_, '_, '_, '_> {
    type Value = TopLevel;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a map with one entry: type path → value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TopLevel, A::Error> {
        let Some(type_path) = map.next_key::<String>()? else {
            return Err(A::Error::invalid_length(0, &self));
        };
        let registration =
            regia_core::brand::lookup_type_path(&type_path, |p| self.registry.get_with_type_path(p))
                .or_else(|| self.registry.get_with_short_type_path(&type_path));
        let value = match registration {
            Some(registration) => TopLevel::Known(
                registration.type_info().type_path(),
                map.next_value_seed(TypedReflectDeserializer::with_processor(
                    registration,
                    self.registry,
                    &mut *self.processor,
                ))?,
            ),
            None if !self.text => {
                return Err(A::Error::custom(format_args!(
                    "{} (binary data cannot skip unknown types)",
                    unresolved_type_phrase(self.registry, &type_path)
                )));
            }
            None if self.policy == UnknownTypePolicy::Preserve => {
                let raw = map.next_value::<Box<ron::value::RawValue>>()?;
                self.processor.warn(format!(
                    "{}; value preserved",
                    unresolved_type_phrase(self.registry, &type_path)
                ));
                TopLevel::Opaque(OpaqueValue {
                    type_path,
                    ron: raw.get_ron().trim().to_string(),
                })
            }
            None => {
                map.next_value::<IgnoredAny>()?;
                self.processor.warn(format!(
                    "{}; value skipped",
                    unresolved_type_phrase(self.registry, &type_path)
                ));
                TopLevel::Skipped(type_path)
            }
        };
        if map.next_key::<IgnoredAny>()?.is_some() {
            return Err(A::Error::custom("expected exactly one type-path entry"));
        }
        Ok(value)
    }
}

#[cfg(test)]
thread_local! {
    static CYCLE_CHECKS: Cell<u32> = const { Cell::new(0) };
    static CYCLE_WALKS: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
fn note_cycle_check() {
    CYCLE_CHECKS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(not(test))]
fn note_cycle_check() {}

#[cfg(test)]
fn note_cycle_walk() {
    CYCLE_WALKS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(not(test))]
fn note_cycle_walk() {}

/// How many binary values entered the cycle walk since the last take.
///
/// Opaque leaves and counted structs do not increment this. One `Vec` value counts once, not once
/// per element.
#[cfg(test)]
pub(super) fn take_cycle_checks() -> u32 {
    CYCLE_CHECKS.with(|count| count.replace(0))
}

/// How many times the cycle DFS itself ran since the last take.
///
/// A cache hit still counts as a check, but not as a walk.
#[cfg(test)]
pub(super) fn take_cycle_walks() -> u32 {
    CYCLE_WALKS.with(|count| count.replace(0))
}
