// SPDX-License-Identifier: MPL-2.0
//! Plain descriptions of registered types: shape, fields, doc comments and editor attributes.
//!
//! This is the self-describing surface of `docs/specs/23-agent-native.md` § 1: the inspector builds
//! its widgets from it and the agent protocol turns it into JSON Schema. Doc comments come from the
//! `reflect_documentation` feature; editor attributes from [`crate::attrs`].

use bevy_ecs::reflect::{ReflectComponent, ReflectResource};
use bevy_reflect::std_traits::ReflectDefault;
use bevy_reflect::{
    NamedField, ReflectDeserialize, ReflectFromReflect, ReflectSerialize, TypeInfo, TypeRegistration,
    TypeRegistry, Typed, UnnamedField, enums::VariantInfo,
};

use crate::attrs::EditorAttrs;

/// Everything the editor and agents need to know about one registered type.
#[derive(Debug, Clone)]
pub struct TypeDescription {
    /// Full, stable type path (`my_game::Door`). Values serialize under this name.
    pub type_path: &'static str,
    /// Short type path (`Door`).
    pub short_path: &'static str,
    /// Doc comment of the type, if any.
    pub docs: Option<&'static str>,
    /// Editor attributes on the type itself.
    pub attrs: EditorAttrs,
    /// Structure of the type.
    pub shape: TypeShape,
    /// Which reflected capabilities are registered.
    pub data: TypeDataFlags,
}

/// Structure of a type.
#[derive(Debug, Clone)]
pub enum TypeShape {
    /// Struct with named fields.
    Struct(Vec<FieldDescription>),
    /// Tuple struct; field names are the indices.
    TupleStruct(Vec<FieldDescription>),
    /// Tuple; field names are the indices.
    Tuple(Vec<FieldDescription>),
    /// Growable list (`Vec<T>`).
    List {
        /// Item type path.
        item: &'static str,
    },
    /// Fixed-size array.
    Array {
        /// Item type path.
        item: &'static str,
        /// Number of items.
        len: usize,
    },
    /// Map (`HashMap<K, V>`).
    Map {
        /// Key type path.
        key: &'static str,
        /// Value type path.
        value: &'static str,
    },
    /// Set (`HashSet<T>`).
    Set {
        /// Value type path.
        value: &'static str,
    },
    /// Enum, including `Option`.
    Enum(Vec<VariantDescription>),
    /// Opaque value serialized through serde (primitives, `String`, `Entity`, ...); glam types are reflected structs.
    Opaque,
}

/// One field of a struct, tuple struct, tuple or enum variant.
#[derive(Debug, Clone)]
pub struct FieldDescription {
    /// Field name; the index (`"0"`, `"1"`, ...) for unnamed fields.
    pub name: String,
    /// Field type path.
    pub type_path: &'static str,
    /// Doc comment of the field, if any.
    pub docs: Option<&'static str>,
    /// Editor attributes on the field.
    pub attrs: EditorAttrs,
}

/// One enum variant.
#[derive(Debug, Clone)]
pub struct VariantDescription {
    /// Variant name.
    pub name: &'static str,
    /// Doc comment of the variant, if any.
    pub docs: Option<&'static str>,
    /// Editor attributes on the variant.
    pub attrs: EditorAttrs,
    /// Fields: empty for unit variants, indices as names for tuple variants.
    pub fields: Vec<FieldDescription>,
    /// True for `Variant { .. }`, false for unit and tuple variants.
    pub named: bool,
}

/// Reflected capabilities registered for a type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TypeDataFlags {
    /// `#[reflect(Component)]`.
    pub component: bool,
    /// `#[reflect(Resource)]`.
    pub resource: bool,
    /// `#[reflect(Default)]`: missing fields of a named struct or named enum variant can be filled
    /// in on load. A type that also has `#[reflect(Deserialize)]` (or `DeserializeWithRegistry`) is
    /// decoded by its own serde and does not get this fill. Tuple structs are not filled.
    pub default: bool,
    /// Can be built from a dynamic value.
    pub from_reflect: bool,
    /// Has both `ReflectSerialize` and `ReflectDeserialize` (its own serde implementation in both
    /// directions); a serialize-only type is still written through its custom serializer.
    pub serde: bool,
}

/// Why [`lookup_type`] did not return a description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypePathLookup {
    /// No registration uses this full or short path.
    NotRegistered,
    /// Two or more registered types share this short path. Use a full type path.
    Ambiguous,
}

/// Describes a registered type, or why a full or short path did not resolve.
///
/// A unique short path resolves. An ambiguous short path (Bevy drops the name from the short-path
/// map once a second type registers it) is [`TypePathLookup::Ambiguous`], not "not registered".
pub fn lookup_type(registry: &TypeRegistry, type_path: &str) -> Result<TypeDescription, TypePathLookup> {
    let registration = registry
        .get_with_type_path(type_path)
        .or_else(|| registry.get_with_short_type_path(type_path))
        .or_else(|| regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p)));
    if let Some(registration) = registration {
        return Ok(describe_registration(registration));
    }
    if registry.is_ambiguous(type_path) {
        Err(TypePathLookup::Ambiguous)
    } else {
        Err(TypePathLookup::NotRegistered)
    }
}

/// Describes a registered type by full or short type path.
///
/// Returns `None` when the path is unknown or the short path is ambiguous. [`lookup_type`]
/// distinguishes those two.
pub fn describe_type(registry: &TypeRegistry, type_path: &str) -> Option<TypeDescription> {
    lookup_type(registry, type_path).ok()
}

/// Describes a registration.
pub fn describe_registration(registration: &TypeRegistration) -> TypeDescription {
    let mut description = describe_info(registration.type_info());
    description.data = TypeDataFlags {
        component: registration.data::<ReflectComponent>().is_some(),
        resource: registration.data::<ReflectResource>().is_some(),
        default: registration.data::<ReflectDefault>().is_some(),
        from_reflect: registration.data::<ReflectFromReflect>().is_some(),
        serde: registration.data::<ReflectSerialize>().is_some()
            && registration.data::<ReflectDeserialize>().is_some(),
    };
    description
}

/// Describes a type from its static type info (no registry needed; [`TypeDescription::data`] is
/// left empty).
pub fn describe_info(info: &'static TypeInfo) -> TypeDescription {
    let (attrs, shape) = match info {
        TypeInfo::Struct(s) => (
            EditorAttrs::from_custom(s.custom_attributes()),
            TypeShape::Struct(s.iter().map(named_field).collect()),
        ),
        TypeInfo::TupleStruct(s) => (
            EditorAttrs::from_custom(s.custom_attributes()),
            TypeShape::TupleStruct(s.iter().map(unnamed_field).collect()),
        ),
        TypeInfo::Tuple(t) => (
            EditorAttrs::default(),
            TypeShape::Tuple(t.iter().map(unnamed_field).collect()),
        ),
        TypeInfo::List(l) => (
            EditorAttrs::default(),
            TypeShape::List {
                item: l.item_ty().path(),
            },
        ),
        TypeInfo::Array(a) => (
            EditorAttrs::default(),
            TypeShape::Array {
                item: a.item_ty().path(),
                len: a.capacity(),
            },
        ),
        TypeInfo::Map(m) => (
            EditorAttrs::default(),
            TypeShape::Map {
                key: m.key_ty().path(),
                value: m.value_ty().path(),
            },
        ),
        TypeInfo::Set(s) => (
            EditorAttrs::default(),
            TypeShape::Set {
                value: s.value_ty().path(),
            },
        ),
        TypeInfo::Enum(e) => (
            EditorAttrs::from_custom(e.custom_attributes()),
            TypeShape::Enum(e.iter().map(variant).collect()),
        ),
        TypeInfo::Opaque(_) => (EditorAttrs::default(), TypeShape::Opaque),
    };
    TypeDescription {
        type_path: info.type_path(),
        short_path: info.type_path_table().short_path(),
        docs: info.docs(),
        attrs,
        shape,
        data: TypeDataFlags::default(),
    }
}

/// Editor attributes of the named field of struct `T`, or `None` if `T` has no such field.
pub fn field_attrs<T: Typed>(field: &str) -> Option<EditorAttrs> {
    match T::type_info() {
        TypeInfo::Struct(s) => s
            .field(field)
            .map(|f| EditorAttrs::from_custom(f.custom_attributes())),
        _ => None,
    }
}

/// Full type paths of every registered type, sorted.
pub fn registered_type_paths(registry: &TypeRegistry) -> Vec<&'static str> {
    let mut paths: Vec<_> = registry.iter().map(|r| r.type_info().type_path()).collect();
    paths.sort_unstable();
    paths
}

fn named_field(field: &NamedField) -> FieldDescription {
    FieldDescription {
        name: field.name().to_string(),
        type_path: field.type_path(),
        docs: field.docs(),
        attrs: EditorAttrs::from_custom(field.custom_attributes()),
    }
}

fn unnamed_field(field: &UnnamedField) -> FieldDescription {
    FieldDescription {
        name: field.index().to_string(),
        type_path: field.type_path(),
        docs: field.docs(),
        attrs: EditorAttrs::from_custom(field.custom_attributes()),
    }
}

fn variant(info: &'static VariantInfo) -> VariantDescription {
    let (fields, named) = match info {
        VariantInfo::Struct(v) => (v.iter().map(named_field).collect(), true),
        VariantInfo::Tuple(v) => (v.iter().map(unnamed_field).collect(), false),
        VariantInfo::Unit(_) => (Vec::new(), false),
    };
    VariantDescription {
        name: info.name(),
        docs: info.docs(),
        attrs: EditorAttrs::from_custom(info.custom_attributes()),
        fields,
        named,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::{Range, Units};
    use crate::prelude::*;

    /// A door that swings.
    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct Door {
        /// Angle when fully open.
        #[reflect(@Range(0.0..=180.0), @Units("deg"))]
        open_angle: f32,
        state: DoorState,
        tags: Vec<String>,
    }

    /// Door state.
    #[derive(Reflect, Default)]
    enum DoorState {
        /// Shut.
        #[default]
        Closed,
        /// Moving.
        Opening {
            /// 0..1.
            progress: f32,
        },
        /// Locked by a key id.
        Locked(u32),
    }

    #[test]
    fn describes_fields_docs_attrs_and_data() {
        let mut registry = TypeRegistry::default();
        registry.register::<Door>();
        let Some(door) = describe_type(&registry, "Door") else {
            panic!("short path lookup");
        };
        assert!(door.type_path.ends_with("::Door"));
        assert_eq!(door.docs, Some(" A door that swings."));
        assert!(door.data.component && door.data.default && door.data.from_reflect);
        let TypeShape::Struct(fields) = &door.shape else {
            panic!("struct shape");
        };
        assert_eq!(fields[0].name, "open_angle");
        assert_eq!(fields[0].type_path, "f32");
        assert_eq!(fields[0].docs, Some(" Angle when fully open."));
        assert_eq!(fields[0].attrs.range, Some(0.0..=180.0));
        assert_eq!(fields[0].attrs.units, Some("deg"));
        assert_eq!(fields[2].type_path, "alloc::vec::Vec<alloc::string::String>");

        let state = describe_type(&registry, fields[1].type_path);
        let Some(TypeShape::Enum(variants)) = state.map(|s| s.shape) else {
            panic!("dependency registered as enum");
        };
        assert_eq!(variants.len(), 3);
        assert!(variants[1].named);
        assert_eq!(variants[1].fields[0].name, "progress");
        assert_eq!(variants[2].fields[0].name, "0");
        assert_eq!(variants[2].docs, Some(" Locked by a key id."));

        let list = describe_type(&registry, fields[2].type_path).map(|d| d.shape);
        assert!(matches!(
            list,
            Some(TypeShape::List {
                item: "alloc::string::String"
            })
        ));
        assert!(registered_type_paths(&registry).contains(&door.type_path));
        assert_eq!(
            field_attrs::<Door>("open_angle").and_then(|a| a.range),
            Some(Range::new(0.0, 180.0).0)
        );
        let _ = Units("m");
    }

    #[test]
    fn a_full_path_with_the_legacy_crate_names_resolves() {
        let mut registry = TypeRegistry::default();
        registry.register::<Door>();
        let current = <Door as bevy_reflect::TypePath>::type_path();
        // `Door` lives in an engine crate: an agent call written before the rename names the
        // legacy crate (ADR-1429).
        let legacy = regia_core::brand::fingerprint_type_path(current);
        let Some(door) = describe_type(&registry, &legacy) else {
            panic!("{legacy} resolves");
        };
        assert_eq!(door.type_path, current);
    }
}
