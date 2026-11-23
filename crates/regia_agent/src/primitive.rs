// SPDX-License-Identifier: MPL-2.0
//! `scene.spawn_primitive` (specs/23 § 3.2): spawn a greybox shape — a
//! [`regia_render::Primitive`] with a `Transform` — in one call.
//!
//! Shape parameters are applied through reflection onto the shape's enum variant, so every field
//! [`regia_render::PrimitiveShape`] has (and any it gains) is settable by name with the same
//! JSON shapes `scene.get_entity` returns (`[x, y, z]` for vectors).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::reflect::serde::TypedReflectDeserializer;
use regia_ecs::reflect::{PartialReflect, ReflectMut};
use regia_render::{Primitive, PrimitiveShape};
use regia_scene::Transform;
use serde::de::DeserializeSeed;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Reads a `[x, y, z]` (or `[x, y, z, w]`) number array.
fn floats<const N: usize>(value: &Value, what: &str) -> Result<[f32; N], RpcError> {
    let numbers: Option<Vec<f32>> = value
        .as_array()
        .filter(|a| a.len() == N)
        .map(|a| a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect());
    numbers
        .filter(|n| n.len() == N && n.iter().all(|v| v.is_finite()))
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be an array of {N} finite numbers")))
}

/// Builds the `Transform` from `{ translation, rotation | euler_deg, scale }` (all optional).
fn transform_from(value: Option<&Value>) -> Result<Transform, RpcError> {
    let mut transform = Transform::default();
    let Some(value) = value else {
        return Ok(transform);
    };
    let Some(map) = value.as_object() else {
        return Err(RpcError::invalid_params(
            "`transform` must be an object { translation, rotation | euler_deg, scale }",
        ));
    };
    if let Some(t) = map.get("translation") {
        transform.translation = Vec3::from_array(floats::<3>(t, "transform.translation")?);
    }
    match (map.get("rotation"), map.get("euler_deg")) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params(
                "give `transform.rotation` (quaternion) or `transform.euler_deg`, not both",
            ));
        }
        (Some(q), None) => {
            let q = Quat::from_array(floats::<4>(q, "transform.rotation")?);
            if q.length_squared() < 1.0e-12 {
                return Err(RpcError::invalid_params(
                    "`transform.rotation` is a zero quaternion",
                ));
            }
            transform.rotation = q.normalize();
        }
        (None, Some(e)) => {
            let [x, y, z] = floats::<3>(e, "transform.euler_deg")?;
            // Yaw (Y), then pitch (X), then roll (Z), intrinsic.
            transform.rotation = Quat::from_rotation_y(y.to_radians())
                * Quat::from_rotation_x(x.to_radians())
                * Quat::from_rotation_z(z.to_radians());
        }
        (None, None) => {}
    }
    if let Some(s) = map.get("scale") {
        transform.scale = match s.as_f64() {
            Some(uniform) => Vec3::splat(uniform as f32),
            None => Vec3::from_array(floats::<3>(s, "transform.scale")?),
        };
        if !transform.scale.is_finite() || transform.scale.min_element().abs() < 1.0e-6 {
            return Err(RpcError::invalid_params(
                "`transform.scale` must be finite and non-zero",
            ));
        }
    }
    Ok(transform)
}

/// The shape from `shape` + `params`, validated.
pub(crate) fn shape_from(world: &World, params: &Value) -> Result<PrimitiveShape, RpcError> {
    let kind = params.get("shape").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "`shape` is required: one of {}",
            PrimitiveShape::KINDS.join(", ")
        ))
    })?;
    let mut shape = PrimitiveShape::default_of(kind).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    if let Some(overrides) = params.get("params") {
        let Some(overrides) = overrides.as_object() else {
            return Err(RpcError::invalid_params(
                "`params` must be an object of { \"field\": value }",
            ));
        };
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let ReflectMut::Enum(variant) = shape.reflect_mut() else {
            return Err(RpcError::internal("PrimitiveShape is not a reflected enum"));
        };
        let fields: Vec<String> = (0..variant.field_len())
            .filter_map(|i| variant.name_at(i).map(str::to_string))
            .collect();
        for (name, value) in overrides {
            let Some(field) = variant.field_mut(name) else {
                return Err(RpcError::invalid_params(format!(
                    "{kind} has no parameter `{name}` (it has: {})",
                    fields.join(", ")
                )));
            };
            let field_type = field
                .get_represented_type_info()
                .map(|info| info.type_id())
                .and_then(|t| registry.get(t))
                .ok_or_else(|| RpcError::internal(format!("parameter `{name}` has no registered type")))?;
            let new_value: Box<dyn PartialReflect> = TypedReflectDeserializer::new(field_type, &registry)
                .deserialize(value)
                .map_err(|e| {
                    RpcError::invalid_params(format!(
                        "`{name}` does not fit {}: {e}",
                        field_type.type_info().type_path()
                    ))
                })?;
            field
                .try_apply(new_value.as_ref())
                .map_err(|e| RpcError::invalid_params(format!("cannot apply `{name}`: {e}")))?;
        }
    }
    shape
        .validate()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    Ok(shape)
}

/// `scene.spawn_primitive`: `{ shape, params?, transform?, name?, color?, parent? }` →
/// `{ entity, shape, vertices, triangles }`.
pub(crate) fn spawn_primitive(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // The shape's field types must be in the registry to deserialise `params`; a world without
    // the render plugin (headless tools, tests) registers them here.
    {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let mut registry = registry.write();
        registry.register::<Primitive>();
        registry.register::<Transform>();
    }
    let shape = shape_from(world, params)?;
    let transform = transform_from(params.get("transform"))?;
    let mut primitive = Primitive::new(shape);
    if let Some(color) = params.get("color") {
        primitive.color = match color.as_array().map(Vec::len) {
            Some(3) => {
                let [r, g, b] = floats::<3>(color, "color")?;
                [r, g, b, 1.0]
            }
            _ => floats::<4>(color, "color")?,
        };
    }
    let parent = match crate::params::opt_u64(params, "parent")? {
        Some(bits) => {
            let parent = Entity::try_from_bits(bits)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
            if world.get_entity(parent).is_err() {
                return Err(RpcError::not_found(
                    format!("entity {bits} does not exist"),
                    "use scene.query_entities",
                ));
            }
            Some(parent)
        }
        None => None,
    };
    // Mesh once now so the response can report the size (the render sync reuses its own cache).
    let mesh = primitive
        .shape
        .mesh()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let kind = primitive.shape.kind();
    let name = crate::params::opt_str(params, "name")?
        .filter(|n| !n.is_empty())
        .map_or_else(|| default_name(kind), str::to_string);

    let mut entity = world.spawn((Name::new(name), transform, primitive));
    if let Some(parent) = parent {
        entity.insert(ChildOf(parent));
    }
    Ok(json!({
        "entity": entity.id().to_bits(),
        "shape": kind,
        "vertices": mesh.vertices.len(),
        "triangles": mesh.indices.len() / 3,
    }))
}

/// "conical_frustum" → "Conical Frustum".
fn default_name(kind: &str) -> String {
    kind.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app
    }

    #[test]
    fn spawns_each_kind_with_defaults() {
        let mut app = app();
        let world = app.world_mut();
        for kind in PrimitiveShape::KINDS {
            let result = spawn_primitive(world, &json!({ "shape": kind })).expect("spawned");
            assert_eq!(result["shape"], *kind);
            let expected = PrimitiveShape::default_of(kind)
                .and_then(|shape| shape.mesh())
                .map(|mesh| mesh.indices.len() / 3)
                .expect("default mesh");
            assert_eq!(
                result["triangles"].as_u64(),
                Some(expected as u64),
                "{kind}: {result}"
            );
            let entity = result["entity"]
                .as_u64()
                .and_then(Entity::try_from_bits)
                .expect("entity id");
            let primitive = world.get::<Primitive>(entity).expect("primitive");
            assert_eq!(primitive.shape.kind(), *kind);
            assert!(world.get::<Transform>(entity).is_some());
        }
    }

    #[test]
    fn params_and_transform_apply() {
        let mut app = app();
        let world = app.world_mut();
        let result = spawn_primitive(
            world,
            &json!({
                "shape": "cylinder",
                "params": { "radius": 0.25, "height": 3.0, "resolution": 8, "caps": false },
                "transform": { "translation": [1.0, 2.0, 3.0], "euler_deg": [0.0, 90.0, 0.0], "scale": 2.0 },
                "name": "Pillar",
                "color": [0.8, 0.2, 0.1],
            }),
        )
        .expect("spawned");
        assert_eq!(result["triangles"], 16);
        let entity = result["entity"]
            .as_u64()
            .and_then(Entity::try_from_bits)
            .expect("entity id");
        let primitive = world.get::<Primitive>(entity).expect("primitive");
        assert_eq!(
            primitive.shape,
            PrimitiveShape::Cylinder {
                radius: 0.25,
                height: 3.0,
                resolution: 8,
                segments: 1,
                caps: false
            }
        );
        assert_eq!(primitive.color, [0.8, 0.2, 0.1, 1.0]);
        let transform = world.get::<Transform>(entity).expect("transform");
        assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(transform.scale, Vec3::splat(2.0));
        assert!((transform.rotation * Vec3::X - Vec3::NEG_Z).length() < 1.0e-5);
        assert_eq!(world.get::<Name>(entity).map(Name::as_str), Some("Pillar"));
    }

    #[test]
    fn vector_params_use_arrays() {
        let mut app = app();
        let world = app.world_mut();
        let result = spawn_primitive(
            world,
            &json!({ "shape": "ramp", "params": { "size": [6.0, 1.5, 3.0] } }),
        )
        .expect("spawned");
        let entity = result["entity"]
            .as_u64()
            .and_then(Entity::try_from_bits)
            .expect("entity id");
        assert_eq!(
            world.get::<Primitive>(entity).map(|p| p.shape.clone()),
            Some(PrimitiveShape::Ramp {
                size: Vec3::new(6.0, 1.5, 3.0)
            })
        );
        assert_eq!(world.get::<Name>(entity).map(Name::as_str), Some("Ramp"));
    }

    #[test]
    fn errors_name_the_problem() {
        let mut app = app();
        let world = app.world_mut();
        let cases = [
            (json!({}), "`shape` is required"),
            (json!({ "shape": "blob" }), "unknown primitive shape"),
            (
                json!({ "shape": "sphere", "params": { "size": 1.0 } }),
                "has no parameter `size`",
            ),
            (
                json!({ "shape": "sphere", "params": { "radius": "big" } }),
                "does not fit",
            ),
            (
                json!({ "shape": "sphere", "params": { "radius": -1.0 } }),
                "out of range",
            ),
            (
                json!({ "shape": "cuboid", "transform": { "translation": [1.0] } }),
                "array of 3",
            ),
        ];
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let cases = cases.into_iter().chain([(
            json!({ "shape": "cuboid", "parent": gone.to_bits() }),
            "does not exist",
        )]);
        for (params, needle) in cases {
            let err = spawn_primitive(world, &params).expect_err("rejected");
            let text = format!("{err:?}");
            assert!(text.contains(needle), "{params}: {text}");
        }
        assert_eq!(
            world.query::<&Primitive>().iter(world).count(),
            0,
            "nothing spawned on error"
        );
    }

    #[test]
    fn default_names_are_title_case() {
        assert_eq!(default_name("conical_frustum"), "Conical Frustum");
        assert_eq!(default_name("cuboid"), "Cuboid");
    }
}
