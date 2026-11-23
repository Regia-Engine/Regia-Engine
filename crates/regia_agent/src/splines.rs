// SPDX-License-Identifier: MPL-2.0
//! `spline.*`: read, write, edit and query `Spline` components (specs/64 § 2.3, book
//! `agents/splines.md`).
//!
//! Errors answer `F0002` (invalid params) or `F0003` (unknown entity, or no `Spline` on it) with
//! `data.reason` set to a lowercase code listed on the book page. `spline.set` and `spline.edit`
//! validate the whole result before writing, so a failed call changes nothing.

use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;
use regia_scene::spline::{
    SPLINE_MAX_POINTS, Spline, SplineAttrDecl, SplineAttrType, SplineCurve, SplineError, SplinePoint,
    SplineSample, SplineUp, SplineValue, TangentMode,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most samples one `spline.sample` call returns.
pub(crate) const MAX_SAMPLES: usize = 100_000;
/// Most edit ops per `spline.edit` call.
pub(crate) const MAX_OPS: usize = 4096;

/// One `spline.set` or `spline.edit` that changed a [`Spline`].
#[derive(Clone, Debug, PartialEq)]
pub struct SplineEdit {
    /// The entity.
    pub entity: Entity,
    /// The component before the call (`None` when `spline.set` added it).
    pub before: Option<Spline>,
    /// The component after the call.
    pub after: Spline,
}

/// The `spline.*` edits since the last drain, recorded only while this resource exists: the editor
/// inserts it and turns each edit into an undo entry (spec 64 § 2.3). A game never pays for it.
#[derive(Resource, Debug, Default)]
pub struct SplineEditLog(pub Vec<SplineEdit>);

/// Writes `s` to `e`, noting the edit when the host keeps a [`SplineEditLog`].
fn write_spline(world: &mut World, e: Entity, s: Spline) {
    if world.contains_resource::<SplineEditLog>() {
        let before = world.get::<Spline>(e).cloned();
        if before.as_ref() != Some(&s) {
            world.resource_mut::<SplineEditLog>().0.push(SplineEdit {
                entity: e,
                before,
                after: s.clone(),
            });
        }
    }
    world.entity_mut(e).insert(s);
}

fn reason(code: i64, regia_code: &str, why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": why, "hint": "see book/agents/splines.md" })),
    }
}

fn bad(why: &str, message: impl Into<String>) -> RpcError {
    reason(INVALID_PARAMS, "F0002", why, message)
}

fn missing(message: impl Into<String>) -> RpcError {
    reason(NOT_FOUND, "F0003", "not_a_spline", message)
}

fn entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("spline_invalid", "`entity` (an entity id) is required"))?;
    let e = Entity::try_from_bits(bits).ok_or_else(|| missing(format!("{bits} is not an entity id")))?;
    if world.get_entity(e).is_err() {
        return Err(missing(format!("entity {bits} does not exist")));
    }
    Ok(e)
}

fn spline_of(world: &World, e: Entity) -> Result<Spline, RpcError> {
    world
        .get::<Spline>(e)
        .cloned()
        .ok_or_else(|| missing(format!("entity {} has no Spline", e.to_bits())))
}

fn curve(world: &World, e: Entity, s: &Spline, params: &Value) -> Result<SplineCurve, RpcError> {
    let world_space = crate::params::opt_bool(params, "world")?.unwrap_or(false);
    Ok(match world.get::<GlobalTransform>(e) {
        Some(gt) if world_space => s.world_curve(&gt.0),
        _ => s.curve(),
    })
}

fn v3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn value_json(v: &SplineValue) -> Value {
    match v {
        SplineValue::F32(x) => json!(x),
        SplineValue::Vec3(x) => v3(*x),
        SplineValue::Name(n) => json!(n),
    }
}

fn point_json(p: &SplinePoint) -> Value {
    json!({
        "position": v3(p.position),
        "tangent_in": v3(p.tangent_in),
        "tangent_out": v3(p.tangent_out),
        "mode": p.mode.name(),
        "roll_deg": p.roll_deg,
        "scale": [p.scale.x, p.scale.y],
        "values": p.values.iter().map(value_json).collect::<Vec<_>>(),
    })
}

fn sample_json(s: &SplineSample) -> Value {
    json!({
        "position": v3(s.position),
        "tangent": v3(s.tangent),
        "up": v3(s.up),
        "right": v3(s.right),
        "roll_deg": s.roll_deg,
        "scale": [s.scale.x, s.scale.y],
        "curvature": s.curvature,
        "along_m": s.along_m,
        "key": s.key,
        "values": s.values.iter().map(value_json).collect::<Vec<_>>(),
    })
}

fn up_name(u: SplineUp) -> &'static str {
    match u {
        SplineUp::RotationMinimizing => "rotation_minimizing",
        SplineUp::WorldY => "world_y",
    }
}

fn spline_json(s: &Spline, c: &SplineCurve) -> Value {
    json!({
        "points": s.points.iter().map(point_json).collect::<Vec<_>>(),
        "closed": s.closed,
        "up": up_name(s.up),
        "attrs": s.attrs.iter().map(|a| json!({ "name": a.name, "ty": a.ty.name() })).collect::<Vec<_>>(),
        "length": c.length(),
        "segment_lengths": (0..c.segment_count()).map(|i| c.segment_length(i)).collect::<Vec<_>>(),
    })
}

fn read_v3(v: &Value, what: &str) -> Result<Vec3, RpcError> {
    let a = v
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| bad("spline_invalid", format!("`{what}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        let f = x
            .as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| bad("spline_invalid", format!("`{what}` needs finite numbers")))?;
        *o = f as f32;
    }
    Ok(Vec3::from(out))
}

fn read_value(v: &Value, ty: SplineAttrType) -> Result<SplineValue, RpcError> {
    match ty {
        SplineAttrType::F32 => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| SplineValue::F32(f as f32))
            .ok_or_else(|| bad("spline_invalid", "an f32 value must be a finite number")),
        SplineAttrType::Vec3 => read_v3(v, "value").map(SplineValue::Vec3),
        SplineAttrType::Name => v
            .as_str()
            .map(|s| SplineValue::Name(s.to_string()))
            .ok_or_else(|| bad("spline_invalid", "a name value must be a string")),
    }
}

/// A point from JSON; missing fields default; values follow `attrs`.
fn read_point(v: &Value, attrs: &[SplineAttrDecl]) -> Result<SplinePoint, RpcError> {
    let mut p = SplinePoint::default();
    if let Some(x) = v.get("position") {
        p.position = read_v3(x, "position")?;
    } else {
        p.position = read_v3(v, "point")?;
        p.values = attrs.iter().map(|a| SplineValue::default_of(a.ty)).collect();
        return Ok(p);
    }
    if let Some(x) = v.get("tangent_in") {
        p.tangent_in = read_v3(x, "tangent_in")?;
    }
    if let Some(x) = v.get("tangent_out") {
        p.tangent_out = read_v3(x, "tangent_out")?;
    }
    if let Some(m) = v.get("mode") {
        p.mode = m
            .as_str()
            .and_then(TangentMode::from_name)
            .ok_or_else(|| bad("spline_invalid", "mode is auto, linear, free, aligned or step"))?;
    }
    if let Some(r) = v.get("roll_deg") {
        p.roll_deg =
            r.as_f64()
                .filter(|f| f.is_finite())
                .ok_or_else(|| bad("spline_invalid", "roll_deg must be a finite number"))? as f32;
    }
    if let Some(s) = v.get("scale") {
        let a = s
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| Some(Vec2::new(a[0].as_f64()? as f32, a[1].as_f64()? as f32)))
            .filter(|s| s.is_finite())
            .ok_or_else(|| bad("spline_invalid", "scale must be [width, height]"))?;
        p.scale = a;
    }
    p.values = match crate::params::opt_array(v, "values")? {
        Some(vals) => {
            if vals.len() != attrs.len() {
                return Err(bad("spline_invalid", "values must match attrs one to one"));
            }
            vals.iter()
                .zip(attrs)
                .map(|(x, a)| read_value(x, a.ty))
                .collect::<Result<_, _>>()?
        }
        None => attrs.iter().map(|a| SplineValue::default_of(a.ty)).collect(),
    };
    Ok(p)
}

fn check(s: &Spline) -> Result<(), RpcError> {
    s.validate().map_err(|e| match e {
        SplineError::TooManyPoints { .. } => bad(e.code(), e.to_string()),
        _ => bad("spline_invalid", e.to_string()),
    })
}

fn index(op: &Value, len: usize, inclusive: bool) -> Result<usize, RpcError> {
    let i = op
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("spline_invalid", "`index` is required"))? as usize;
    if i > len || (!inclusive && i == len) {
        return Err(bad(
            "spline_index_out_of_range",
            format!("index {i} with {len} points"),
        ));
    }
    Ok(i)
}

/// `spline.get`.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity(world, params)?;
    let s = spline_of(world, e)?;
    let c = curve(world, e, &s, params)?;
    Ok(spline_json(&s, &c))
}

/// `spline.set`: replaces points, closed and attrs (any subset); inserts a `Spline` when the
/// entity has none.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity(world, params)?;
    let mut s = world.get::<Spline>(e).cloned().unwrap_or_default();
    if let Some(attrs) = params.get("attrs") {
        let list = attrs
            .as_array()
            .ok_or_else(|| bad("spline_invalid", "attrs must be a list of { name, ty }"))?;
        s.attrs = list
            .iter()
            .map(|a| {
                let name = crate::params::opt_str(a, "name")?.unwrap_or_default().to_string();
                let ty = crate::params::opt_str(a, "ty")?
                    .and_then(SplineAttrType::from_name)
                    .ok_or_else(|| bad("spline_invalid", "attr ty is f32, vec3 or name"))?;
                if name.is_empty() {
                    return Err(bad("spline_invalid", "attr names must not be empty"));
                }
                Ok(SplineAttrDecl { name, ty })
            })
            .collect::<Result<_, _>>()?;
        if params.get("points").is_none() {
            // Keep the points; reset their values to the new declarations.
            for p in &mut s.points {
                p.values = s.attrs.iter().map(|a| SplineValue::default_of(a.ty)).collect();
            }
        }
    }
    if let Some(points) = params.get("points") {
        let list = points
            .as_array()
            .ok_or_else(|| bad("spline_invalid", "points must be a list"))?;
        if list.len() > SPLINE_MAX_POINTS {
            return Err(bad(
                "spline_too_many_points",
                format!("{} points, the limit is {SPLINE_MAX_POINTS}", list.len()),
            ));
        }
        s.points = list
            .iter()
            .map(|p| read_point(p, &s.attrs))
            .collect::<Result<_, _>>()?;
    }
    if let Some(c) = params.get("closed") {
        s.closed = c
            .as_bool()
            .ok_or_else(|| bad("spline_invalid", "closed must be a boolean"))?;
    }
    if let Some(u) = params.get("up") {
        s.up = match u.as_str() {
            Some("rotation_minimizing") => SplineUp::RotationMinimizing,
            Some("world_y") => SplineUp::WorldY,
            _ => return Err(bad("spline_invalid", "up is rotation_minimizing or world_y")),
        };
    }
    check(&s)?;
    let length = s.length();
    write_spline(world, e, s);
    Ok(json!({ "length": length }))
}

/// `spline.edit`: insert, remove, move, set_mode and set_value ops, all or nothing.
pub(crate) fn edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity(world, params)?;
    let mut s = spline_of(world, e)?;
    let ops = params
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("spline_invalid", "`ops` (a list) is required"))?;
    if ops.len() > MAX_OPS {
        return Err(bad("spline_invalid", format!("at most {MAX_OPS} ops per call")));
    }
    for op in ops {
        let kind = crate::params::opt_str(op, "op")?.unwrap_or_default();
        match kind {
            "insert" => {
                let i = index(op, s.points.len(), true)?;
                let p = read_point(op.get("point").unwrap_or(&Value::Null), &s.attrs)?;
                if s.points.len() >= SPLINE_MAX_POINTS {
                    return Err(bad(
                        "spline_too_many_points",
                        format!("the limit is {SPLINE_MAX_POINTS}"),
                    ));
                }
                s.points.insert(i, p);
            }
            "remove" => {
                let i = index(op, s.points.len(), false)?;
                s.points.remove(i);
            }
            "move" => {
                let i = index(op, s.points.len(), false)?;
                s.points[i].position = read_v3(op.get("position").unwrap_or(&Value::Null), "position")?;
            }
            "set_mode" => {
                let i = index(op, s.points.len(), false)?;
                s.points[i].mode = crate::params::opt_str(op, "mode")?
                    .and_then(TangentMode::from_name)
                    .ok_or_else(|| bad("spline_invalid", "mode is auto, linear, free, aligned or step"))?;
            }
            "set_value" => {
                let i = index(op, s.points.len(), false)?;
                let name = crate::params::opt_str(op, "attr")?.unwrap_or_default();
                let k = s
                    .attr_index(name)
                    .ok_or_else(|| bad("spline_attr_unknown", format!("no attribute `{name}`")))?;
                let ty = s.attrs[k].ty;
                s.points[i].values[k] = read_value(op.get("value").unwrap_or(&Value::Null), ty)?;
            }
            other => {
                return Err(bad(
                    "spline_invalid",
                    format!("unknown op `{other}` (insert, remove, move, set_mode, set_value)"),
                ));
            }
        }
    }
    check(&s)?;
    let length = s.length();
    let points: Vec<Value> = s.points.iter().map(point_json).collect();
    write_spline(world, e, s);
    Ok(json!({ "length": length, "points": points }))
}

/// `spline.sample`: samples at distances (`at`) or `count` evenly spaced ones (ends included).
pub(crate) fn sample(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity(world, params)?;
    let s = spline_of(world, e)?;
    let c = curve(world, e, &s, params)?;
    let distances: Vec<f64> = match (params.get("at"), params.get("count")) {
        (Some(at), _) => {
            let a = at
                .as_array()
                .ok_or_else(|| bad("spline_invalid", "`at` must be a list of distances"))?;
            if a.len() > MAX_SAMPLES {
                return Err(bad(
                    "spline_too_many_samples",
                    format!("at most {MAX_SAMPLES} samples"),
                ));
            }
            a.iter()
                .map(|d| {
                    d.as_f64()
                        .filter(|f| f.is_finite())
                        .ok_or_else(|| bad("spline_invalid", "distances must be finite numbers"))
                })
                .collect::<Result<_, _>>()?
        }
        (None, Some(n)) => {
            let n = n
                .as_u64()
                .filter(|n| *n >= 1)
                .ok_or_else(|| bad("spline_invalid", "`count` must be at least 1"))?
                as usize;
            if n > MAX_SAMPLES {
                return Err(bad(
                    "spline_too_many_samples",
                    format!("at most {MAX_SAMPLES} samples"),
                ));
            }
            let len = c.length_f64();
            (0..n)
                .map(|k| {
                    if n == 1 {
                        0.0
                    } else {
                        len * k as f64 / (n - 1) as f64
                    }
                })
                .collect()
        }
        (None, None) => return Err(bad("spline_invalid", "give `at` or `count`")),
    };
    let samples: Vec<Value> = distances
        .iter()
        .map(|d| sample_json(&c.at_distance_f64(*d)))
        .collect();
    Ok(json!({ "length": c.length(), "samples": samples }))
}

/// `spline.closest`.
pub(crate) fn closest(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity(world, params)?;
    let s = spline_of(world, e)?;
    let p = read_v3(params.get("point").unwrap_or(&Value::Null), "point")?;
    let c = curve(world, e, &s, params)?;
    let hit = c.closest(p);
    Ok(json!({
        "key": hit.key,
        "distance_m": hit.distance_m,
        "along_m": hit.along_m,
        "position": v3(hit.position),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_spline() -> (World, Entity) {
        let mut w = World::new();
        let e = w
            .spawn(Spline::through([Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0)]))
            .id();
        (w, e)
    }

    fn reason_of(e: &RpcError) -> &str {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    #[test]
    fn edits_reach_the_log_only_when_the_host_keeps_one() {
        let (mut w, e) = world_with_spline();
        let id = e.to_bits();
        let moved =
            json!({ "entity": id, "ops": [{ "op": "move", "index": 1, "position": [20.0, 0.0, 0.0] }] });
        edit(&mut w, &moved).expect("no log, no cost");
        assert!(!w.contains_resource::<SplineEditLog>());

        w.init_resource::<SplineEditLog>();
        let before = w.get::<Spline>(e).cloned();
        let moved =
            json!({ "entity": id, "ops": [{ "op": "move", "index": 1, "position": [30.0, 0.0, 0.0] }] });
        edit(&mut w, &moved).expect("edit");
        let log = w.resource::<SplineEditLog>();
        assert_eq!(log.0.len(), 1);
        assert_eq!(log.0[0].entity, e);
        assert_eq!(log.0[0].before, before);
        assert_eq!(Some(&log.0[0].after), w.get::<Spline>(e));

        // The same values again change nothing, and a failed call changes nothing.
        edit(&mut w, &moved).expect("same edit");
        let bad = json!({ "entity": id, "ops": [{ "op": "remove", "index": 99 }] });
        assert!(edit(&mut w, &bad).is_err());
        assert_eq!(w.resource::<SplineEditLog>().0.len(), 1);

        let plain = w.spawn_empty().id();
        set(
            &mut w,
            &json!({ "entity": plain.to_bits(), "points": [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]] }),
        )
        .expect("set");
        assert_eq!(
            w.resource::<SplineEditLog>().0[1].before,
            None,
            "spline.set added it"
        );
    }

    #[test]
    fn get_sample_closest() {
        let (mut w, e) = world_with_spline();
        let id = e.to_bits();
        let g = get(&mut w, &json!({ "entity": id })).expect("get");
        assert!((g["length"].as_f64().unwrap_or(0.0) - 10.0).abs() < 1e-4);
        let s = sample(&mut w, &json!({ "entity": id, "count": 3 })).expect("sample");
        assert_eq!(s["samples"].as_array().map(Vec::len), Some(3));
        assert!((s["samples"][1]["position"][0].as_f64().unwrap_or(0.0) - 5.0).abs() < 1e-4);
        let c = closest(&mut w, &json!({ "entity": id, "point": [4.0, 3.0, 0.0] })).expect("closest");
        assert!((c["distance_m"].as_f64().unwrap_or(0.0) - 3.0).abs() < 1e-4);
    }

    #[test]
    fn set_and_edit_are_all_or_nothing() {
        let (mut w, e) = world_with_spline();
        let id = e.to_bits();
        let r = set(
            &mut w,
            &json!({ "entity": id, "attrs": [{ "name": "w", "ty": "f32" }],
                     "points": [{ "position": [0, 0, 0], "values": [1.0] }, { "position": [0, 0, 4], "values": [2.0] }] }),
        )
        .expect("set");
        assert!((r["length"].as_f64().unwrap_or(0.0) - 4.0).abs() < 1e-4);
        let r = edit(
            &mut w,
            &json!({ "entity": id, "ops": [
                { "op": "insert", "index": 2, "point": { "position": [4, 0, 4] } },
                { "op": "set_mode", "index": 0, "mode": "linear" },
                { "op": "set_value", "index": 2, "attr": "w", "value": 3.0 },
            ] }),
        )
        .expect("edit");
        assert_eq!(r["points"].as_array().map(Vec::len), Some(3));
        let before = w.get::<Spline>(e).cloned();
        let err = edit(
            &mut w,
            &json!({ "entity": id, "ops": [{ "op": "remove", "index": 0 }, { "op": "remove", "index": 9 }] }),
        )
        .expect_err("range");
        assert_eq!(reason_of(&err), "spline_index_out_of_range");
        assert_eq!(
            w.get::<Spline>(e).cloned(),
            before,
            "a failed edit changes nothing"
        );
        let err = edit(
            &mut w,
            &json!({ "entity": id, "ops": [{ "op": "set_value", "index": 0, "attr": "x", "value": 1 }] }),
        )
        .expect_err("attr");
        assert_eq!(reason_of(&err), "spline_attr_unknown");
        let many: Vec<Value> = (0..SPLINE_MAX_POINTS + 1).map(|i| json!([i, 0, 0])).collect();
        let err = set(&mut w, &json!({ "entity": id, "points": many })).expect_err("too many");
        assert_eq!(reason_of(&err), "spline_too_many_points");
        let plain = w.spawn_empty().id();
        let err = get(&mut w, &json!({ "entity": plain.to_bits() })).expect_err("no spline");
        assert_eq!(reason_of(&err), "not_a_spline");
    }
}
