// SPDX-License-Identifier: MPL-2.0
//! `physics.time_of_impact` (ADR-2423, `book/agents/physics-time-of-impact.md`): the first time
//! two convex shapes moving with constant linear and angular velocity come within a gap, by the
//! conservative-advancement angular sweep of `regia_physics::ccd::time_of_impact_motion`.
//!
//! Each side is a literal shape and pose, or an `entity` whose collider shape and world pose
//! (and, unless `motion` says otherwise, `Velocity`) are read from the app. Read-only. Literal
//! shapes need no physics world. Every refusal of the query comes back as
//! `{ status: "failed", reason }` with a lowercase code; only malformed JSON is a `F0002` error.

use regia_app::prelude::*;
use regia_core::math::{Isometry, Quat, Vec3};
use regia_physics::ccd::{ConvexInput, RigidMotion, ToiOptions, ToiResult, time_of_impact_motion};
use regia_physics::{Collider, ColliderShape, ShapeAxis, Velocity};
use regia_scene::GlobalTransform;
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// Most points a literal `hull` may have.
pub(crate) const MAX_HULL_POINTS: usize = 1024;

/// A number as f32. Values past f32 range become infinite on purpose: the query refuses them
/// with `non_finite_input`, which is the documented answer.
fn number(value: &Value, what: &str) -> Result<f32, RpcError> {
    value
        .as_f64()
        .map(|n| n as f32)
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be a number")))
}

/// `[x, y, z]` (or `{x, y, z}`) as a vector, non-finite values allowed through (see [`number`]).
fn vec3(value: &Value, what: &str) -> Result<Vec3, RpcError> {
    match value {
        Value::Array(items) if items.len() == 3 => Ok(Vec3::new(
            number(&items[0], what)?,
            number(&items[1], what)?,
            number(&items[2], what)?,
        )),
        Value::Object(map) => {
            let get = |k: &str| {
                map.get(k)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{what}` needs x, y and z")))
                    .and_then(|v| number(v, what))
            };
            Ok(Vec3::new(get("x")?, get("y")?, get("z")?))
        }
        _ => Err(RpcError::invalid_params(format!("`{what}` must be [x, y, z]"))),
    }
}

/// An optional vector field of `map`, `default` when absent.
fn opt_vec3(map: &Map<String, Value>, field: &str, what: &str, default: Vec3) -> Result<Vec3, RpcError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => vec3(v, &format!("{what}.{field}")),
    }
}

/// An optional number field of `map`, `default` when absent.
fn opt_number(map: &Map<String, Value>, field: &str, what: &str, default: f32) -> Result<f32, RpcError> {
    match map.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => number(v, &format!("{what}.{field}")),
    }
}

/// `[x, y, z, w]` quaternion (not normalized here; a zero quaternion is `invalid_pose`).
fn quat(value: &Value, what: &str) -> Result<Quat, RpcError> {
    match value {
        Value::Array(items) if items.len() == 4 => Ok(Quat::from_xyzw(
            number(&items[0], what)?,
            number(&items[1], what)?,
            number(&items[2], what)?,
            number(&items[3], what)?,
        )),
        _ => Err(RpcError::invalid_params(format!(
            "`{what}` must be a quaternion [x, y, z, w]"
        ))),
    }
}

fn axis(map: &Map<String, Value>, what: &str) -> Result<ShapeAxis, RpcError> {
    match crate::params::opt_str(map, "axis")? {
        None => Ok(ShapeAxis::Y),
        Some("x") | Some("X") => Ok(ShapeAxis::X),
        Some("y") | Some("Y") => Ok(ShapeAxis::Y),
        Some("z") | Some("Z") => Ok(ShapeAxis::Z),
        Some(other) => Err(RpcError::invalid_params(format!(
            "`{what}.axis` must be x, y or z, not {other:?}"
        ))),
    }
}

/// A literal shape: point, sphere, box, capsule, cylinder, cone, segment or hull.
fn shape(value: &Value, what: &str) -> Result<ConvexInput, RpcError> {
    let map = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be an object with a `kind`")))?;
    let kind = map
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}.kind` is required")))?;
    let req = |field: &str| -> Result<f32, RpcError> {
        map.get(field)
            .ok_or_else(|| RpcError::invalid_params(format!("`{what}.{field}` is required for a {kind}")))
            .and_then(|v| number(v, &format!("{what}.{field}")))
    };
    let req_vec = |field: &str| -> Result<Vec3, RpcError> {
        map.get(field)
            .ok_or_else(|| RpcError::invalid_params(format!("`{what}.{field}` is required for a {kind}")))
            .and_then(|v| vec3(v, &format!("{what}.{field}")))
    };
    Ok(match kind {
        "point" => ConvexInput::Point,
        "sphere" => ConvexInput::Sphere {
            radius: req("radius")?,
        },
        "box" => ConvexInput::Box {
            half_extents: req_vec("half_extents")?,
        },
        "capsule" => ConvexInput::Capsule {
            half_height: req("half_height")?,
            radius: req("radius")?,
            axis: axis(map, what)?,
        },
        "cylinder" => ConvexInput::Cylinder {
            half_height: req("half_height")?,
            radius: req("radius")?,
            axis: axis(map, what)?,
        },
        "cone" => ConvexInput::Cone {
            half_height: req("half_height")?,
            radius: req("radius")?,
            axis: axis(map, what)?,
        },
        "segment" => ConvexInput::Segment {
            a: req_vec("a")?,
            b: req_vec("b")?,
        },
        "hull" => {
            let points = map.get("points").and_then(Value::as_array).ok_or_else(|| {
                RpcError::invalid_params(format!("`{what}.points` ([[x, y, z], ...]) is required"))
            })?;
            if points.len() > MAX_HULL_POINTS {
                return Err(RpcError::invalid_params(format!(
                    "`{what}.points` has {} points; at most {MAX_HULL_POINTS}",
                    points.len()
                )));
            }
            let mut out = Vec::with_capacity(points.len());
            for p in points {
                out.push(vec3(p, &format!("{what}.points"))?);
            }
            ConvexInput::ConvexHull { points: out }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown `{what}.kind` {other:?} (point | sphere | box | capsule | cylinder | cone | segment | hull)"
            )));
        }
    })
}

/// One side: its shape, its pose at `t = 0`, and the velocity read from its entity (if any).
struct Side {
    shape: ConvexInput,
    pose: Isometry,
    velocity: Option<(Vec3, Vec3)>,
}

/// A side from `{ shape, position, rotation }` or `{ entity }`.
fn side(world: &World, value: Option<&Value>, what: &str) -> Result<Side, RpcError> {
    let map = value.and_then(Value::as_object).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "`{what}` ({{ shape, position }} or {{ entity }}) is required"
        ))
    })?;
    if let Some(id) = map.get("entity") {
        let bits = id
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{what}.entity` must be an entity id")))?;
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("`{what}.entity` {bits} is not an entity id")))?;
        let entity_ref = world.get_entity(entity).map_err(|_| {
            RpcError::not_found(format!("no entity {bits}"), "list entities with scene.query")
        })?;
        let collider = entity_ref.get::<Collider>().ok_or_else(|| {
            RpcError::not_found(
                format!("entity {bits} has no Collider"),
                "give it a regia_physics::Collider",
            )
        })?;
        let shape = convex_of(&collider.shape).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "entity {bits}'s collider is not one convex shape (trimesh, heightfield, compound or invalid numbers)"
            ))
        })?;
        let global = entity_ref
            .get::<GlobalTransform>()
            .map(GlobalTransform::to_transform)
            .ok_or_else(|| {
                RpcError::not_found(
                    format!("entity {bits} has no GlobalTransform"),
                    "spawn it with a Transform",
                )
            })?;
        let body = Isometry::new(global.rotation, global.translation);
        let offset = Isometry::new(
            collider.offset.rotation * regia_physics::shape_axis_rotation(&collider.shape),
            collider.offset.translation,
        );
        let velocity = entity_ref.get::<Velocity>().map(|v| (v.linear, v.angular));
        return Ok(Side {
            shape,
            pose: body * offset,
            velocity,
        });
    }
    let shape_value = map.get("shape").ok_or_else(|| {
        RpcError::invalid_params(format!("`{what}.shape` is required (or `{what}.entity`)"))
    })?;
    let shape = shape(shape_value, &format!("{what}.shape"))?;
    let position = opt_vec3(map, "position", what, Vec3::ZERO)?;
    let rotation = match map.get("rotation") {
        None | Some(Value::Null) => Quat::IDENTITY,
        Some(v) => quat(v, &format!("{what}.rotation"))?,
    };
    Ok(Side {
        shape,
        pose: Isometry::new(rotation, position),
        velocity: None,
    })
}

/// The collider shape as a convex query input. The shape-axis rotation is applied by the
/// caller's pose (`shape_axis_rotation`), so axis-aligned shapes are converted along Y.
fn convex_of(shape: &ColliderShape) -> Option<ConvexInput> {
    let converted = ConvexInput::from_collider_shape(shape)?;
    // `from_collider_shape` keeps the authored axis; the pose already carries the axis
    // rotation that the physics world uses, so express the shape along Y.
    Some(match converted {
        ConvexInput::Capsule {
            half_height, radius, ..
        } => ConvexInput::Capsule {
            half_height,
            radius,
            axis: ShapeAxis::Y,
        },
        ConvexInput::Cylinder {
            half_height, radius, ..
        } => ConvexInput::Cylinder {
            half_height,
            radius,
            axis: ShapeAxis::Y,
        },
        ConvexInput::Cone {
            half_height, radius, ..
        } => ConvexInput::Cone {
            half_height,
            radius,
            axis: ShapeAxis::Y,
        },
        other => other,
    })
}

/// One side's motion over the interval: `motion.<side>` velocities (per second) times
/// `duration`, else the entity's `Velocity`, else at rest.
fn side_motion(
    side: &Side,
    motion: Option<&Map<String, Value>>,
    key: &str,
    duration: f32,
) -> Result<RigidMotion, RpcError> {
    let what = format!("motion.{key}");
    let entry = match motion.and_then(|m| m.get(key)) {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(map),
        Some(_) => return Err(RpcError::invalid_params(format!("`{what}` must be an object"))),
    };
    let (default_lin, default_ang) = side.velocity.unwrap_or((Vec3::ZERO, Vec3::ZERO));
    let (linear, angular, center) = match entry {
        Some(map) => (
            opt_vec3(map, "linear", &what, default_lin)?,
            opt_vec3(map, "angular", &what, default_ang)?,
            opt_vec3(map, "center", &what, Vec3::ZERO)?,
        ),
        None => (default_lin, default_ang, Vec3::ZERO),
    };
    Ok(RigidMotion::from_velocities(
        side.pose, center, linear, angular, duration,
    ))
}

/// `[x, y, z]` as JSON.
fn arr(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `physics.time_of_impact { a, b, motion?, target_gap?, tolerance?, max_iterations? }`.
pub fn time_of_impact(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let a = side(world, params.get("a"), "a")?;
    let b = side(world, params.get("b"), "b")?;
    let motion = match params.get("motion") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(map),
        Some(_) => return Err(RpcError::invalid_params("`motion` must be an object")),
    };
    let duration = match motion {
        Some(map) => opt_number(map, "duration", "motion", 1.0)?,
        None => 1.0,
    };
    let empty = Map::new();
    let top = params.as_object().unwrap_or(&empty);
    let defaults = ToiOptions::default();
    let target_gap = opt_number(top, "target_gap", "params", defaults.target_gap)?;
    let tolerance = opt_number(top, "tolerance", "params", defaults.tolerance)?;
    let max_iterations = match top.get("max_iterations") {
        None | Some(Value::Null) => defaults.max_iterations,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=100_000).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| RpcError::invalid_params("`max_iterations` must be an integer in 1..=100000"))?,
    };
    if !duration.is_finite() || duration < 0.0 {
        return Ok(
            json!({ "status": "failed", "reason": if duration.is_finite() { "invalid_parameter" } else { "non_finite_input" } }),
        );
    }
    let motion_a = side_motion(&a, motion, "a", duration)?;
    let motion_b = side_motion(&b, motion, "b", duration)?;
    let options = ToiOptions {
        target_gap,
        tolerance,
        max_iterations,
    };
    Ok(
        match time_of_impact_motion(&a.shape, &motion_a, &b.shape, &motion_b, options) {
            ToiResult::Hit { t, normal, point } => {
                let pose_a = motion_a.pose_at(t);
                let pose_b = motion_b.pose_at(t);
                json!({
                    "status": "hit",
                    "t": t,
                    "time": t * duration,
                    "normal": arr(normal),
                    "point": arr(point),
                    "pose_a": { "position": arr(pose_a.translation), "rotation": [pose_a.rotation.x, pose_a.rotation.y, pose_a.rotation.z, pose_a.rotation.w] },
                    "pose_b": { "position": arr(pose_b.translation), "rotation": [pose_b.rotation.x, pose_b.rotation.y, pose_b.rotation.z, pose_b.rotation.w] },
                })
            }
            ToiResult::Separated => json!({ "status": "separated" }),
            ToiResult::OverlappedAtStart => json!({ "status": "overlapped_at_start", "t": 0.0, "time": 0.0 }),
            ToiResult::Failed(reason) => json!({ "status": "failed", "reason": reason.code() }),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_physics::RigidBody;
    use regia_scene::Transform;

    fn world() -> World {
        World::new()
    }

    fn status(v: &Value) -> &str {
        v.get("status").and_then(Value::as_str).unwrap_or("")
    }

    #[test]
    fn a_spinning_bar_hits_a_slab_on_its_real_path() {
        let mut w = world();
        let params = json!({
            "a": { "shape": { "kind": "box", "half_extents": [1.0, 0.02, 0.02] } },
            "b": { "shape": { "kind": "box", "half_extents": [2.0, 0.01, 2.0] }, "position": [0.0, 0.5, 0.0] },
            "motion": { "duration": 0.1, "a": { "angular": [0.0, 0.0, 188.4955592] } }
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(status(&out), "hit", "{out}");
        let t = out["t"].as_f64().unwrap_or(f64::NAN);
        // Analytic: the corner (1, 0.02) at radius |(1, 0.02)| reaches y = 0.49 (plus half the
        // tolerance band) after asin(0.49005 / r) - atan(0.02) = 0.4920 rad of the 6 pi swept.
        let r = (1.0f64 + 0.02 * 0.02).sqrt();
        let expected = ((0.49005 / r).asin() - 0.02f64.atan()) / (6.0 * std::f64::consts::PI);
        assert!((t - expected).abs() < 2e-4, "t {t} vs analytic {expected}: {out}");
        assert!(out["normal"][1].as_f64().unwrap_or(0.0) > 0.99, "{out}");
        let time = out["time"].as_f64().unwrap_or(f64::NAN);
        assert!((time - t * 0.1).abs() < 1e-6, "{out}");
    }

    #[test]
    fn separated_and_overlapped_are_reported() {
        let mut w = world();
        let apart = json!({
            "a": { "shape": { "kind": "sphere", "radius": 0.5 } },
            "b": { "shape": { "kind": "sphere", "radius": 0.5 }, "position": [5.0, 0.0, 0.0] },
        });
        assert_eq!(
            status(&time_of_impact(&mut w, &apart).unwrap_or_else(|e| panic!("{e:?}"))),
            "separated"
        );
        let inside = json!({
            "a": { "shape": { "kind": "sphere", "radius": 0.5 } },
            "b": { "shape": { "kind": "sphere", "radius": 0.5 }, "position": [0.2, 0.0, 0.0] },
        });
        assert_eq!(
            status(&time_of_impact(&mut w, &inside).unwrap_or_else(|e| panic!("{e:?}"))),
            "overlapped_at_start"
        );
    }

    #[test]
    fn non_finite_input_is_refused_with_a_code() {
        let mut w = world();
        // 1e39 is past f32 range: it becomes infinity, which the query refuses.
        let params = json!({
            "a": { "shape": { "kind": "sphere", "radius": 0.1 }, "position": [1e39, 0.0, 0.0] },
            "b": { "shape": { "kind": "box", "half_extents": [0.1, 1.0, 1.0] } },
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(status(&out), "failed");
        assert_eq!(out["reason"], "non_finite_input");
        let params = json!({
            "a": { "shape": { "kind": "sphere", "radius": 0.1 } },
            "b": { "shape": { "kind": "box", "half_extents": [0.1, 1.0, 1.0] }, "position": [3.0, 0.0, 0.0] },
            "motion": { "a": { "angular": [0.0, 0.0, 1e39] } }
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out["reason"], "non_finite_input", "{out}");
        let params = json!({
            "a": { "shape": { "kind": "sphere", "radius": 0.1 } },
            "b": { "shape": { "kind": "sphere", "radius": 0.1 }, "position": [3.0, 0.0, 0.0] },
            "motion": { "a": { "linear": [3e38, 0.0, 0.0] }, "b": { "linear": [-3e38, 0.0, 0.0] } }
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out["reason"], "degenerate_motion", "{out}");
        let params = json!({
            "a": { "shape": { "kind": "sphere", "radius": -1.0 } },
            "b": { "shape": { "kind": "sphere", "radius": 0.1 } },
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(out["reason"], "invalid_shape", "{out}");
    }

    #[test]
    fn malformed_params_are_invalid_params() {
        let mut w = world();
        for params in [
            json!({}),
            json!({ "a": { "shape": { "kind": "dodecahedron" } }, "b": { "shape": { "kind": "point" } } }),
            json!({ "a": { "shape": { "kind": "sphere" } }, "b": { "shape": { "kind": "point" } } }),
            json!({ "a": { "shape": { "kind": "point" } }, "b": { "shape": { "kind": "point" } }, "max_iterations": 0 }),
            json!({ "a": { "shape": { "kind": "point" } }, "b": { "shape": { "kind": "point" } }, "motion": 3 }),
        ] {
            let err = time_of_impact(&mut w, &params).err();
            assert!(err.is_some(), "{params} must be refused");
        }
    }

    #[test]
    fn an_entity_side_uses_its_collider_pose_and_velocity() {
        let mut w = world();
        let ball = w
            .spawn((
                Transform::from_translation(Vec3::new(-5.0, 0.0, 0.0)),
                GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    -5.0, 0.0, 0.0,
                ))),
                RigidBody::dynamic(),
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.1 },
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(100.0, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id();
        let params = json!({
            "a": { "entity": ball.to_bits() },
            "b": { "shape": { "kind": "box", "half_extents": [0.005, 2.0, 2.0] } },
            "motion": { "duration": 0.1 }
        });
        let out = time_of_impact(&mut w, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(status(&out), "hit", "{out}");
        // 10 m in 0.1 s: contact when the centre reaches x = -0.105, t = 4.895 / 10.
        let t = out["t"].as_f64().unwrap_or(f64::NAN);
        assert!((t - 0.4895).abs() < 1e-3, "{out}");
        let missing = json!({ "a": { "entity": 4_294_967_999u64 }, "b": { "shape": { "kind": "point" } } });
        assert!(time_of_impact(&mut w, &missing).is_err());
    }
}
