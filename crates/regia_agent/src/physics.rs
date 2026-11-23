// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for physics scene queries (specs/15 § 3, specs/23 § 3):
//! `physics.raycast`, `physics.overlap` and `physics.closest_point` run against the live
//! rapier world (`physics.teleport` moves a body without sweeping, ADR-7233), so agents can verify where things landed, who is inside a volume, and
//! what a point is near — without driving gameplay. `physics.stats` reports the world's
//! counts (awake/sleeping bodies, contacts, joints, the last step's cost) and a state
//! digest for determinism checks (ADR-870).
//!
//! Every handler degrades gracefully when `PhysicsPlugin` is not installed (`NOT_FOUND`
//! with a hint naming the plugin), so the protocol surface stays uniform across apps.
//! Handlers are plain functions over `&mut World`; tests drive them without a server.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Extracts a 3-component vector (`[x, y, z]`, or `{x, y, z}`) from `params[field]`.
pub(crate) fn vec3_param(params: &Value, field: &str) -> Result<regia_core::math::Vec3, RpcError> {
    let value = params
        .get(field)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` ([x, y, z]) is required")))?;
    let components = match value {
        Value::Array(items) if items.len() == 3 => {
            let mut out = [0.0f64; 3];
            for (i, item) in items.iter().enumerate() {
                out[i] = item
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three numbers")))?;
            }
            out
        }
        Value::Object(map) => {
            let g = |k: &str| map.get(k).and_then(Value::as_f64);
            let invalid = || RpcError::invalid_params(format!("`{field}` must be three numbers"));
            [
                g("x").ok_or_else(invalid)?,
                g("y").ok_or_else(invalid)?,
                g("z").ok_or_else(invalid)?,
            ]
        }
        _ => {
            return Err(RpcError::invalid_params(format!("`{field}` must be [x, y, z]")));
        }
    };
    let v = regia_core::math::Vec3::new(components[0] as f32, components[1] as f32, components[2] as f32);
    // A JSON number past f32's range becomes infinity here (and NaN in the queries).
    if !v.is_finite() {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be three finite numbers within f32 range"
        )));
    }
    Ok(v)
}

/// Extracts an optional positive-number parameter with a default.
pub(crate) fn distance_param(params: &Value, field: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None => Ok(default),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !((n as f32).is_finite() && (n as f32) > 0.0) {
                return Err(RpcError::invalid_params(format!(
                    "`{field}` must be a positive, finite number"
                )));
            }
            Ok(n as f32)
        }
    }
}

/// Extracts a required positive dimension using the distance parameter's numeric checks.
fn required_distance_param(params: &Value, field: &str) -> Result<f32, RpcError> {
    if params.get(field).is_none() {
        return Err(RpcError::invalid_params(format!("`{field}` is required")));
    }
    distance_param(params, field, 0.0)
}

/// Builds the [`regia_physics::WorldQueries`] over the app's physics world, or the
/// `NOT_FOUND` error naming the plugin when there is none.
fn queries(world: &mut World) -> Result<regia_physics::WorldQueries<'_>, RpcError> {
    if world.get_resource::<regia_physics::PhysicsWorld>().is_none() {
        return Err(RpcError::not_found(
            "no PhysicsWorld in this app",
            "add regia_physics::PhysicsPlugin so physics queries have a world to run against",
        ));
    }
    let has_layers = world.get_resource::<regia_physics::PhysicsLayers>().is_some();
    Ok(regia_physics::WorldQueries {
        // Both resources exist (checked above); the borrows are disjoint from the &mut
        // World because the values are read through raw borrows taken in order.
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: has_layers.then_some(world.resource::<regia_physics::PhysicsLayers>()),
    })
}

/// One ray hit as JSON (entity bits, point, normal, distance).
fn ray_hit_json(hit: regia_physics::RaycastHit) -> Value {
    json!({
        "entity": hit.entity.to_bits(),
        "point": [hit.point.x, hit.point.y, hit.point.z],
        "normal": [hit.normal.x, hit.normal.y, hit.normal.z],
        "distance": hit.distance,
    })
}

/// `physics.raycast`: cast a ray through the physics world.
///
/// Returns the closest hit, or every hit sorted by distance with `all: true`. Sensors are
/// skipped with `exclude_sensors: true` (the default).
pub fn raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let origin = vec3_param(params, "origin")?;
    let direction = vec3_param(params, "direction")?;
    if direction.length_squared() < f32::MIN_POSITIVE {
        return Err(RpcError::invalid_params("`direction` must not be zero"));
    }
    let max_distance = distance_param(params, "max_distance", 100.0)?;
    let all = crate::params::opt_bool(params, "all")?.unwrap_or(false);
    let exclude_sensors = crate::params::opt_bool(params, "exclude_sensors")?.unwrap_or(true);
    let mut filter = regia_physics::QueryFilter::new();
    if exclude_sensors {
        filter = filter.exclude_sensors();
    }

    let q = queries(world)?;
    if all {
        let hits: Vec<Value> = q
            .raycast_all(origin, direction, max_distance, filter)
            .into_iter()
            .map(ray_hit_json)
            .collect();
        let count = hits.len();
        return Ok(json!({ "hits": hits, "count": count }));
    }
    let hit = q.raycast(origin, direction, max_distance, filter);
    let count = usize::from(hit.is_some());
    Ok(json!({
        "hit": hit.map(ray_hit_json),
        "count": count,
    }))
}

/// Builds a [`regia_physics::ColliderShape`] from `{kind: "box"|"sphere"|"capsule", ...}`.
fn shape_from_params(value: &Value) -> Result<regia_physics::ColliderShape, RpcError> {
    use regia_core::math::Vec3;
    use regia_physics::ColliderShape;
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`shape.kind` (box | sphere | capsule) is required"))?;
    match kind {
        "box" => {
            let half = vec3_param(value, "half_extents")?;
            if half.cmple(Vec3::ZERO).any() || !half.cmpge(Vec3::splat(f32::MIN_POSITIVE)).all() {
                return Err(RpcError::invalid_params("`half_extents` must be positive"));
            }
            Ok(ColliderShape::Box { half_extents: half })
        }
        "sphere" => {
            let radius = required_distance_param(value, "radius")?;
            Ok(ColliderShape::Sphere { radius })
        }
        "capsule" => {
            let half_height = required_distance_param(value, "half_height")?;
            let radius = required_distance_param(value, "radius")?;
            Ok(ColliderShape::Capsule {
                half_height,
                radius,
                axis: regia_physics::ShapeAxis::Y,
            })
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown shape kind {other:?} (box | sphere | capsule)"
        ))),
    }
}

/// `physics.overlap`: every entity whose collider intersects a query shape at `position`.
pub fn overlap(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let shape_value = params.get("shape").ok_or_else(|| {
        RpcError::invalid_params("`shape` ({kind: \"box\"|\"sphere\"|\"capsule\", ...}) is required")
    })?;
    let shape = shape_from_params(shape_value)?;
    let position = vec3_param(params, "position")?;
    let exclude_sensors = crate::params::opt_bool(params, "exclude_sensors")?.unwrap_or(false);
    let mut filter = regia_physics::QueryFilter::new();
    if exclude_sensors {
        filter = filter.exclude_sensors();
    }
    let pose = regia_scene::Transform::from_translation(position);

    let mut entities: Vec<u64> = queries(world)?
        .overlap(&shape, pose, filter)
        .into_iter()
        .map(|e| e.to_bits())
        .collect();
    // Broadphase order shifts when bodies are added; ids are stable.
    entities.sort_unstable();
    let count = entities.len();
    Ok(json!({ "entities": entities, "count": count }))
}

/// `physics.closest_point`: the collider nearest to `point` within `max_distance`.
pub fn closest_point(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let point = vec3_param(params, "point")?;
    let max_distance = distance_param(params, "max_distance", f32::INFINITY)?;
    let result = queries(world)?.closest_point(point, max_distance, regia_physics::QueryFilter::new());
    let Some(hit) = result else {
        return Ok(json!({ "hit": null }));
    };
    Ok(json!({
        "hit": {
            "entity": hit.entity.to_bits(),
            "point": [hit.point.x, hit.point.y, hit.point.z],
            "is_inside": hit.is_inside,
            "normal": [hit.normal.x, hit.normal.y, hit.normal.z],
            "distance": hit.distance,
        }
    }))
}

/// `physics.teleport`: moves a rigid body to `position` (and `rotation`) on the next fixed step
/// without sweeping the way there, keeping or clearing its velocity (`regia_physics::teleport`,
/// ADR-7233). `F0003` when the entity does not exist or has no `RigidBody`.
pub fn teleport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .ok_or_else(|| {
            RpcError::invalid_params("`entity` (a body's id from scene.query_entities) is required")
        })?
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
    let position = vec3_param(params, "position")?;
    let velocity = match params.get("velocity") {
        None | Some(Value::Null) => regia_physics::TeleportVelocity::Keep,
        Some(value) => value
            .as_str()
            .and_then(regia_physics::TeleportVelocity::parse)
            .ok_or_else(|| RpcError::invalid_params("`velocity` must be \"keep\" or \"clear\""))?,
    };
    let Some(current) = world.get::<regia_scene::Transform>(entity).copied() else {
        return Err(RpcError::not_found(
            format!("entity {bits} does not exist or has no Transform"),
            "list bodies with scene.query_entities { with: [\"RigidBody\"] }",
        ));
    };
    let rotation = match params.get("rotation") {
        None | Some(Value::Null) => current.rotation,
        Some(Value::Array(items)) if items.len() == 4 => {
            let mut q = [0.0f32; 4];
            for (slot, item) in q.iter_mut().zip(items) {
                *slot = item
                    .as_f64()
                    .ok_or_else(|| RpcError::invalid_params("`rotation` must be four numbers [x, y, z, w]"))?
                    as f32;
            }
            let q = regia_core::math::Quat::from_array(q);
            if !q.is_finite() || q.length_squared() < 1e-12 {
                return Err(RpcError::invalid_params(
                    "`rotation` must be a finite, non-zero quaternion [x, y, z, w]",
                ));
            }
            q.normalize()
        }
        Some(_) => return Err(RpcError::invalid_params("`rotation` must be [x, y, z, w]")),
    };
    let pose = regia_scene::Transform {
        translation: position,
        rotation,
        ..current
    };
    if !regia_physics::teleport(world, entity, pose, velocity) {
        return Err(RpcError::not_found(
            format!("entity {bits} has no RigidBody"),
            "only rigid bodies teleport; list them with scene.query_entities { with: [\"RigidBody\"] }",
        ));
    }
    Ok(json!({
        "entity": bits,
        "position": [position.x, position.y, position.z],
        "rotation": [rotation.x, rotation.y, rotation.z, rotation.w],
        "velocity": velocity.as_str(),
    }))
}

/// `physics.stats`: body/collider/joint counts, sleep state, contacts, the last step's
/// cost, the solver settings, non-finite inputs dropped at the rapier seam (ADR-1507) and
/// a 64-bit state digest (16 hex digits).
///
/// Two runs with the same inputs have the same `digest` at the same `steps`; a different
/// digest means the simulations diverged (ADR-870).
pub fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(physics) = world.get_resource::<regia_physics::PhysicsWorld>() else {
        return Err(RpcError::not_found(
            "no PhysicsWorld in this app",
            "add regia_physics::PhysicsPlugin so there is a physics world to report on",
        ));
    };
    let s = physics.stats();
    let digest = format!("{:016x}", physics.state_digest());
    let (solver_iterations, ccd_substeps) = world
        .get_resource::<regia_physics::PhysicsSettings>()
        .map_or((None, None), |settings| {
            (Some(settings.solver_iterations), Some(settings.ccd_substeps))
        });
    Ok(json!({
        "bodies": s.bodies,
        "dynamic": s.dynamic,
        "kinematic": s.kinematic,
        "fixed": s.fixed,
        "awake": s.awake_dynamic,
        "sleeping": s.sleeping_dynamic,
        "ccd_bodies": s.ccd_bodies,
        "colliders": s.colliders,
        "sensors": s.sensors,
        "joints": s.joints,
        "contact_pairs": s.contact_pairs,
        "steps": s.steps,
        "last_step_ms": s.last_step_ms,
        "non_finite_inputs": s.non_finite_inputs,
        "solver_iterations": solver_iterations,
        "ccd_substeps": ccd_substeps,
        "digest": digest,
    }))
}

/// The debug-draw switch as JSON: the state plus the counters that show it is free while off.
fn debug_draw_json(world: &World) -> Value {
    let draw = world.resource::<regia_physics::debug_draw::PhysicsDebugDraw>();
    let group_enabled = world
        .get_resource::<regia_render::gizmos::GizmoConfigStore>()
        .is_some_and(|store| store.is_enabled::<regia_render::gizmos::PhysicsGizmos>());
    let stats = draw.stats();
    json!({
        "on": draw.is_on(),
        "kinds": draw.kinds().names(),
        "gizmo_group_enabled": group_enabled,
        "lines": draw.lines().len(),
        "collections": stats.collections,
        "lines_emitted": stats.lines_emitted,
        "dropped": stats.dropped,
    })
}

/// `physics.parented_bodies`: the play-world check of spec 15 § 1 — `Dynamic` [`RigidBody`]
/// entities parented under an entity with no `RigidBody`. The hierarchy and the simulation
/// fight there (spec 15 § 1 step 3 writes a `Dynamic` body's world pose back into its local
/// `Transform`); the spec recommends bodies as hierarchy roots. `Static`/`Kinematic` bodies
/// under a group node are the supported case and are not listed. Each offending entity is
/// logged once (a `tracing::warn!`, so the editor log shows it) and counted here; findings
/// are pruned when the body despawns. Read-only; needs `PhysicsPlugin` (the check runs
/// wherever the plugin runs — a play world).
pub fn parented_bodies(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(warnings) = world.get_resource::<regia_physics::ParentedBodyWarnings>() else {
        return Err(RpcError::not_found(
            "no ParentedBodyWarnings in this app",
            "add regia_physics::PhysicsPlugin so the play world runs the parented-body check",
        ));
    };
    let bodies: Vec<Value> = warnings
        .entries
        .iter()
        .map(|w| {
            json!({
                "entity": w.entity.to_bits(),
                "parent": w.parent.to_bits(),
            })
        })
        .collect();
    Ok(json!({ "count": warnings.count(), "bodies": bodies }))
}

/// `physics.debug_draw`: turns the physics debug draw (contacts, joints, centers of mass) on or
/// off and chooses its `kinds` (ADR-2424). Turning it on also enables the `physics` gizmo group so
/// the lines show (the collider outlines of that group draw too); turning it off leaves the group
/// alone, and while the draw is off nothing is collected (`collections` stays put). Both fields
/// are optional; with neither, the call reports the state. Errors carry a stable code in brackets:
/// `physics_debug_flag`, `physics_debug_kinds`, `physics_debug_kind_unknown`.
pub fn debug_draw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_physics::debug_draw::{PhysicsDebugDraw, PhysicsDebugKinds};
    if world.get_resource::<regia_physics::PhysicsWorld>().is_none() {
        return Err(RpcError::not_found(
            "no PhysicsWorld in this app",
            "add regia_physics::PhysicsPlugin so there is a physics world to draw",
        ));
    }
    let on = match params.get("on") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "[physics_debug_flag] `on` must be a boolean",
            ));
        }
    };
    let kinds = match params.get("kinds") {
        None | Some(Value::Null) => None,
        Some(Value::Array(names)) => {
            let mut kinds = PhysicsDebugKinds::NONE;
            for name in names {
                let name = name.as_str().ok_or_else(|| {
                    RpcError::invalid_params(
                        "[physics_debug_kinds] `kinds` must be strings: contacts, joints, com",
                    )
                })?;
                if !kinds.enable(name) {
                    return Err(RpcError::invalid_params(format!(
                        "[physics_debug_kind_unknown] `{name}` is not a kind (contacts, joints, com)"
                    )));
                }
            }
            Some(kinds)
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "[physics_debug_kinds] `kinds` must be an array of contacts, joints, com",
            ));
        }
    };
    {
        let mut draw = world.get_resource_or_insert_with(PhysicsDebugDraw::default);
        if let Some(kinds) = kinds {
            draw.set_kinds(kinds);
        }
        if let Some(on) = on {
            draw.set_on(on);
        }
    }
    if on == Some(true)
        && let Some(mut store) = world.get_resource_mut::<regia_render::gizmos::GizmoConfigStore>()
    {
        store.set_enabled("physics", true);
    }
    Ok(debug_draw_json(world))
}

/// `physics.material_check`: parses and validates the text of a `.physmat.ron` without registering
/// it (a summary on success, an F0002 error whose message starts with the stable code in brackets).
pub fn material_check(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("ron")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`ron` (the text of a .physmat.ron) is required"))?;
    match regia_physics::PhysicsMaterial::from_ron(text) {
        Ok(m) => Ok(material_json(&m)),
        Err(error) => Err(RpcError::invalid_params(format!("[{}] {error}", error.code()))),
    }
}

/// `physics.material_register`: validates a `.physmat.ron` text and adds the material to the
/// `PhysicsMaterials` registry (replacing one of the same name), so colliders with a
/// `PhysicsMaterialRef` naming it take its values on the next frame.
pub fn material_register(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let checked = material_check(world, params)?;
    let text = crate::params::opt_str(params, "ron")?.unwrap_or_default();
    let mut materials = world.get_resource_or_insert_with(regia_physics::PhysicsMaterials::default);
    materials
        .insert_ron(text)
        .map_err(|error| RpcError::invalid_params(format!("[{}] {error}", error.code())))?;
    let mut out = checked;
    out["registered"] = json!(materials.len());
    Ok(out)
}

fn material_json(m: &regia_physics::PhysicsMaterial) -> Value {
    json!({
        "ok": true,
        "name": m.name,
        "format_version": m.format_version,
        "friction": m.friction,
        "restitution": m.restitution,
        "density": m.density,
        "friction_combine": format!("{:?}", m.friction_combine),
        "restitution_combine": format!("{:?}", m.restitution_combine),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_ecs::prelude::ChildOf;
    use regia_physics::{Collider, ColliderShape, RigidBody};
    use regia_scene::Transform;

    /// A headless app with physics and a ground plate (top at y = 0).
    fn app_with_ground() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(10.0, 0.5, 10.0),
                },
                ..Collider::default()
            },
        ));
        app.update(); // the sync chain creates the rapier collider
        for _ in 0..9 {
            app.update();
        }
        app
    }

    #[test]
    fn raycast_finds_the_ground() {
        let mut app = app_with_ground();
        let result = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast works");
        let hit = result["hit"].as_object().expect("a hit");
        assert_eq!(hit["distance"], 5.0);
        assert_eq!(hit["normal"], json!([0.0, 1.0, 0.0]));
        assert_eq!(hit["point"], json!([0.0, 0.0, 0.0]));

        let all = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0], "all": true }),
        )
        .expect("all hits");
        assert_eq!(all["count"], 1);
    }

    #[test]
    fn queries_still_hit_a_layer_the_query_matrix_hides() {
        // `secret` collides with itself, so its collision-filter row is not empty, but no query
        // row names it. These handlers do not set `from_layer`, so the secret box stays visible.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        let file = regia_physics::LayerFile::parse(
            r#"(layers: ["world", "secret", "player"], collides: [("secret", "secret"), ("world", "world")], queries: [("player", "world")])"#,
        )
        .expect("layers");
        let layers = regia_physics::PhysicsLayers::from_file(&file).expect("table");
        let secret = layers.index_of("secret").expect("secret");
        let player = regia_physics::CollisionLayer::new(layers.index_of("player").expect("player"));
        assert_eq!(
            layers.query_groups(Some(&player), None).filter.bits() & (1u32 << u32::from(secret)),
            0,
            "the query matrix does not let player see secret"
        );
        app.insert_resource(layers);
        app.world_mut().spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                layer: regia_physics::CollisionLayer::new(secret),
                ..Collider::default()
            },
        ));
        for _ in 0..5 {
            app.update();
        }
        let ray = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
        )
        .expect("raycast");
        assert!(ray["hit"].is_object(), "raycast still hits secret: {ray}");
        let overlap = overlap(
            app.world_mut(),
            &json!({ "shape": { "kind": "sphere", "radius": 0.2 }, "position": [0.0, 0.0, 0.0] }),
        )
        .expect("overlap");
        assert_eq!(overlap["count"], 1, "overlap still hits secret: {overlap}");
        let closest = closest_point(
            app.world_mut(),
            &json!({ "point": [0.0, 2.0, 0.0], "max_distance": 5.0 }),
        )
        .expect("closest");
        assert!(
            closest["hit"].is_object(),
            "closest point still hits secret: {closest}"
        );
    }

    #[test]
    fn raycast_misses_past_max_distance() {
        let mut app = app_with_ground();
        let result = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0], "max_distance": 2.0 }),
        )
        .expect("raycast works");
        assert!(result["hit"].is_null());
    }

    #[test]
    fn raycast_validates_params() {
        let mut app = app_with_ground();
        assert!(raycast(app.world_mut(), &json!({ "origin": [0.0, 0.0, 0.0] })).is_err());
        assert!(
            raycast(
                app.world_mut(),
                &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, 0.0, 0.0] })
            )
            .is_err()
        );
        assert!(
            raycast(
                app.world_mut(),
                &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0], "max_distance": -1.0 })
            )
            .is_err()
        );
    }

    #[test]
    fn audit_overlap_requires_positive_sphere_and_capsule_dimensions() {
        let mut app = app_with_ground();
        for shape in [
            json!({ "kind": "sphere" }),
            json!({ "kind": "sphere", "radius": 0 }),
            json!({ "kind": "capsule" }),
            json!({ "kind": "capsule", "radius": 1 }),
            json!({ "kind": "capsule", "half_height": 1 }),
            json!({ "kind": "capsule", "half_height": 0, "radius": 1 }),
            json!({ "kind": "capsule", "half_height": 1, "radius": 0 }),
            json!({ "kind": "sphere", "radius": 1e-100 }),
        ] {
            let err = overlap(app.world_mut(), &json!({ "shape": shape, "position": [0, 0, 0] }))
                .expect_err("missing or zero size");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{shape}");
        }
        for shape in [
            json!({ "kind": "sphere", "radius": 1 }),
            json!({ "kind": "capsule", "half_height": 1, "radius": 1 }),
        ] {
            let result = overlap(app.world_mut(), &json!({ "shape": shape, "position": [0, 0, 0] }))
                .expect("positive dimensions");
            assert_eq!(result["count"], 1);
        }
    }

    #[test]
    fn overlap_finds_the_ground_box() {
        let mut app = app_with_ground();
        let result = overlap(
            app.world_mut(),
            &json!({ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0.0, 0.5, 0.0] }),
        )
        .expect("overlap works");
        assert_eq!(result["count"], 1, "the ground plate is inside the sphere");

        let empty = overlap(
            app.world_mut(),
            &json!({ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0.0, 5.0, 0.0] }),
        )
        .expect("overlap works");
        assert_eq!(empty["count"], 0);
    }

    #[test]
    fn overlap_supports_boxes_and_validates() {
        let mut app = app_with_ground();
        let result = overlap(
            app.world_mut(),
            &json!({ "shape": { "kind": "box", "half_extents": [0.5, 0.5, 0.5] }, "position": [0.0, 0.25, 0.0] }),
        )
        .expect("box overlap works");
        assert_eq!(result["count"], 1);

        assert!(
            overlap(
                app.world_mut(),
                &json!({ "shape": { "kind": "dodecahedron" }, "position": [0.0, 0.0, 0.0] })
            )
            .is_err()
        );
        assert!(overlap(
            app.world_mut(),
            &json!({ "shape": { "kind": "box", "half_extents": [0.0, 0.0, 0.0] }, "position": [0.0, 0.0, 0.0] })
        )
        .is_err());
    }

    #[test]
    fn closest_point_finds_the_surface() {
        let mut app = app_with_ground();
        let result = closest_point(app.world_mut(), &json!({ "point": [0.0, 2.0, 0.0] }))
            .expect("closest_point works");
        let hit = result["hit"].as_object().expect("a hit");
        assert_eq!(hit["point"], json!([0.0, 0.0, 0.0]));
        assert_eq!(hit["is_inside"], false);

        let far = closest_point(
            app.world_mut(),
            &json!({ "point": [0.0, 50.0, 0.0], "max_distance": 1.0 }),
        )
        .expect("closest_point works");
        assert!(far["hit"].is_null());
    }

    #[test]
    fn without_physics_plugin_the_handlers_hint_the_plugin() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let error = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 0.0, 0.0], "direction": [0.0, 1.0, 0.0] }),
        )
        .expect_err("no physics world");
        assert!(error.message.contains("PhysicsWorld"), "{}", error.message);
        let hint = error
            .data
            .as_ref()
            .and_then(|d| d.get("hint"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(hint.contains("PhysicsPlugin"), "{hint}");
        assert!(
            overlap(
                app.world_mut(),
                &json!({ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0.0, 0.0, 0.0] })
            )
            .is_err()
        );
        assert!(closest_point(app.world_mut(), &json!({ "point": [0.0, 0.0, 0.0] })).is_err());
    }

    #[test]
    fn sensors_are_excludable() {
        let mut app = app_with_ground();
        let sensor = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.5 },
                    sensor: true,
                    ..Collider::default()
                },
            ))
            .id();
        app.update();
        let _ = sensor;

        // Default skips sensors: only the ground is hit.
        let without = raycast(
            app.world_mut(),
            &json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0], "all": true }),
        )
        .expect("raycast works");
        assert_eq!(without["count"], 1, "sensors are excluded by default");

        // Opting in sees both.
        let with = raycast(
            app.world_mut(),
            &json!({
                "origin": [0.0, 5.0, 0.0],
                "direction": [0.0, -1.0, 0.0],
                "all": true,
                "exclude_sensors": false
            }),
        )
        .expect("raycast works");
        assert_eq!(with["count"], 2, "the sensor is visible when asked for");
    }

    #[test]
    fn stats_report_counts_sleep_and_digest() {
        let mut app = app_with_ground();
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 0.25, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.25),
                },
                ..Collider::default()
            },
        ));
        for _ in 0..240 {
            app.update();
        }
        let stats = stats(app.world_mut(), &json!({})).expect("stats");
        assert_eq!(stats["bodies"], 2, "{stats}");
        assert_eq!(stats["dynamic"], 1, "{stats}");
        assert_eq!(stats["fixed"], 1, "{stats}");
        assert_eq!(stats["colliders"], 2, "{stats}");
        assert_eq!(stats["sleeping"], 1, "a box resting for 4 s sleeps: {stats}");
        assert_eq!(stats["awake"], 0, "{stats}");
        assert_eq!(stats["solver_iterations"], 4, "{stats}");
        assert_eq!(
            stats["non_finite_inputs"], 0,
            "a healthy world drops nothing: {stats}"
        );
        let digest = stats["digest"].as_str().expect("digest string");
        assert_eq!(digest.len(), 16, "{stats}");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{stats}");

        // Asleep, the world does not change: the digest holds.
        app.update();
        let again = stats_digest(&mut app);
        assert_eq!(again, digest);
    }

    fn stats_digest(app: &mut App) -> String {
        stats(app.world_mut(), &json!({})).expect("stats")["digest"]
            .as_str()
            .expect("digest")
            .to_owned()
    }

    #[test]
    fn stats_without_physics_is_not_found() {
        let mut app = App::new();
        let error = stats(app.world_mut(), &json!({})).expect_err("no physics world");
        assert!(format!("{error:?}").contains("PhysicsPlugin"), "{error:?}");
    }

    #[test]
    fn parented_bodies_counts_the_play_world_check() {
        // Without the plugin the method refuses.
        let mut app = App::new();
        let error = parented_bodies(app.world_mut(), &json!({})).expect_err("no plugin");
        assert!(format!("{error:?}").contains("PhysicsPlugin"), "{error:?}");

        // With the resource present, the offending pair is counted once.
        let mut app = App::new();
        app.init_resource::<regia_physics::ParentedBodyWarnings>();
        let parent = app.world_mut().spawn_empty().id();
        let body = app
            .world_mut()
            .spawn((regia_physics::RigidBody::default(), ChildOf(parent)))
            .id();
        app.add_systems(PostUpdate, regia_physics::warn_parented_bodies);
        app.update();
        let out = parented_bodies(app.world_mut(), &json!({})).expect("counted");
        assert_eq!(out["count"], 1, "{out}");
        assert_eq!(out["bodies"][0]["entity"], body.to_bits());
        assert_eq!(out["bodies"][0]["parent"], parent.to_bits());
        let again = parented_bodies(app.world_mut(), &json!({})).expect("stable");
        assert_eq!(again, out, "a read does not change the count");
    }

    #[test]
    fn debug_draw_toggles_and_validates() {
        let mut app = app_with_ground();
        let off = debug_draw(app.world_mut(), &json!({})).expect("state");
        assert_eq!(off["on"], false);
        assert_eq!(off["collections"], 0);
        app.update();
        assert_eq!(
            debug_draw(app.world_mut(), &json!({})).expect("state")["collections"],
            0
        );
        let on = debug_draw(
            app.world_mut(),
            &json!({ "on": true, "kinds": ["contacts", "com"] }),
        )
        .expect("on");
        assert_eq!(on["on"], true);
        assert_eq!(on["kinds"], json!(["contacts", "com"]));
        app.update();
        let after = debug_draw(app.world_mut(), &json!({})).expect("state");
        assert!(after["collections"].as_u64().unwrap_or(0) >= 1, "{after}");
        for bad in [
            json!({ "on": 1 }),
            json!({ "kinds": "joints" }),
            json!({ "kinds": ["wheels"] }),
        ] {
            assert!(debug_draw(app.world_mut(), &bad).is_err(), "{bad}");
        }
        let unknown = debug_draw(app.world_mut(), &json!({ "kinds": ["wheels"] })).expect_err("unknown kind");
        assert!(
            format!("{unknown:?}").contains("physics_debug_kind_unknown"),
            "{unknown:?}"
        );
    }

    #[test]
    fn material_check_and_register() {
        let mut app = app_with_ground();
        let ok = material_check(
            app.world_mut(),
            &json!({ "ron": "(format_version: 1, name: \"ice\", friction: 0.02)" }),
        )
        .expect("valid");
        assert_eq!(ok["name"], "ice");
        assert_eq!(ok["friction"], 0.02f32 as f64);
        let bad = material_check(
            app.world_mut(),
            &json!({ "ron": "(format_version: 1, name: \"x\", friction: 9.0)" }),
        )
        .expect_err("out of range");
        assert!(format!("{bad:?}").contains("physmat_value"), "{bad:?}");
        let registered = material_register(
            app.world_mut(),
            &json!({ "ron": "(format_version: 1, name: \"ice\", friction: 0.02)" }),
        )
        .expect("registered");
        assert_eq!(registered["registered"], 1);
        let materials = app.world().resource::<regia_physics::PhysicsMaterials>();
        assert!(materials.get("ice").is_some());
    }

    /// ADR-7233: the closest point reports the outward normal and signed distance.
    #[test]
    fn closest_point_reports_normal_and_signed_distance() {
        let mut app = app_with_ground();
        let out = closest_point(app.world_mut(), &json!({ "point": [1.0, 2.0, 0.0] })).expect("works");
        assert_eq!(out["hit"]["normal"], json!([0.0, 1.0, 0.0]));
        assert_eq!(out["hit"]["distance"], json!(2.0));
        let inside = closest_point(app.world_mut(), &json!({ "point": [1.0, -0.25, 0.0] })).expect("works");
        assert_eq!(inside["hit"]["is_inside"], true);
        assert_eq!(inside["hit"]["normal"], json!([0.0, 1.0, 0.0]));
        assert_eq!(inside["hit"]["distance"], json!(-0.25));
    }

    /// A weightless dynamic ball moving along +x at 2 m/s, with a `Velocity` read-back.
    fn moving_ball(app: &mut App) -> regia_ecs::prelude::Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
                RigidBody {
                    gravity_scale: 0.0,
                    can_sleep: false,
                    ..RigidBody::dynamic()
                },
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.25 },
                    ..Collider::default()
                },
                regia_physics::Velocity {
                    linear: Vec3::new(2.0, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id()
    }

    #[test]
    fn teleport_moves_a_body_and_keeps_or_clears_its_velocity() {
        let mut app = app_with_ground();
        let ball = moving_ball(&mut app);
        for _ in 0..3 {
            app.update();
        }
        let out = teleport(
            app.world_mut(),
            &json!({ "entity": ball.to_bits(), "position": [3.0, 4.0, 5.0], "velocity": "clear" }),
        )
        .expect("teleports");
        assert_eq!(out["velocity"], "clear");
        assert_eq!(out["position"], json!([3.0, 4.0, 5.0]));
        for _ in 0..3 {
            app.update();
        }
        let at = app.world().get::<Transform>(ball).map(|t| t.translation);
        assert_eq!(at, Some(Vec3::new(3.0, 4.0, 5.0)), "cleared: it stays");
        // Keep (the default) carries on at 2 m/s.
        let world = app.world_mut();
        if let Some(mut v) = world.get_mut::<regia_physics::Velocity>(ball) {
            v.linear = Vec3::new(2.0, 0.0, 0.0);
        }
        teleport(
            world,
            &json!({ "entity": ball.to_bits(), "position": [0.0, 8.0, 0.0] }),
        )
        .expect("keep");
        for _ in 0..3 {
            app.update();
        }
        let x = app
            .world()
            .get::<Transform>(ball)
            .map_or(0.0, |t| t.translation.x);
        assert!(x > 0.01, "kept velocity moves it on (x = {x})");

        // Refusals: a missing entity, an entity without a body, a bad velocity name, a bad rotation.
        let plain = app.world_mut().spawn(Transform::IDENTITY).id();
        for (params, code) in [
            (
                json!({ "entity": 4294967295u64 * 2, "position": [0, 0, 0] }),
                "F0003",
            ),
            (
                json!({ "entity": plain.to_bits(), "position": [0, 0, 0] }),
                "F0003",
            ),
            (
                json!({ "entity": ball.to_bits(), "position": [0, 0, 0], "velocity": "stop" }),
                "F0002",
            ),
            (
                json!({ "entity": ball.to_bits(), "position": [0, 0, 0], "rotation": [0, 0, 0, 0] }),
                "F0002",
            ),
            (json!({ "entity": ball.to_bits() }), "F0002"),
        ] {
            let err = teleport(app.world_mut(), &params).expect_err("refused");
            assert_eq!(
                err.data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str),
                Some(code),
                "{params}"
            );
        }
    }

    /// The method end to end through the JSON-RPC dispatcher (schema row, permission, dispatch arm).
    #[test]
    fn teleport_dispatches_through_the_protocol() {
        use crate::dispatch::{AgentInbox, RateLimit, process_requests};
        use crate::protocol::Request;
        use crate::safety::{AgentConfig, AgentPermissions};
        use crate::server::Inbound;

        let mut app = app_with_ground();
        app.insert_resource(AgentPermissions {
            config: AgentConfig::default(),
            source: None,
        })
        .init_resource::<crate::safety::AuditLog>()
        .init_resource::<crate::safety::AgentSession>()
        .init_resource::<crate::playtest::AgentTimeControl>()
        .init_resource::<RateLimit>()
        .init_resource::<AppTypeRegistry>();
        let ball = moving_ball(&mut app);
        app.update();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(7)),
                    method: "physics.teleport".into(),
                    params: json!({ "entity": ball.to_bits(), "position": [-4.0, 6.0, 1.0], "rotation": [0.0, 0.0, 0.0, 1.0], "velocity": "clear" }),
                },
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        process_requests(app.world_mut());
        let response = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("answered");
        assert!(response.error.is_none(), "{:?}", response.error);
        let result = response.result.expect("a result");
        assert_eq!(result["entity"], json!(ball.to_bits()));
        for _ in 0..3 {
            app.update();
        }
        let at = app.world().get::<Transform>(ball).map(|t| t.translation);
        assert_eq!(at, Some(Vec3::new(-4.0, 6.0, 1.0)));
    }
}
