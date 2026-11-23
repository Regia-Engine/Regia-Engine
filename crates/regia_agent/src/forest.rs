// SPDX-License-Identifier: MPL-2.0
//! The forest flythrough on the agent protocol (ADR-1503, `book/agents/forest-flythrough.md`,
//! guide topic `forest_flythrough`):
//!
//! - `terrain.forest_query`: scatter instances (trunk, crown, height) in a box or along a segment,
//!   from placement (nothing has to be streamed in);
//! - `terrain.forest_corridor`: clearance of a camera path per segment (trunks, crowns, ground,
//!   canopy cover), optionally with the carve that would clear it;
//! - `terrain.forest_carve`: computes that carve and installs it into the running field;
//! - `terrain.forest_flight` / `terrain.set_forest_flight`: motion LOD, wake sway, pop meter and
//!   a flight-speed check of the set.
//!
//! The host inserts [`ForestFlight`] (with its [`regia_terrain::scatter::forest::ScatterQuery`])
//! and adds [`regia_terrain::scatter::forest::ForestFlightPlugin`]. Without the resource every
//! method answers `F0003`; bad parameters are `F0002`.

use regia_ecs::prelude::*;
use regia_terrain::scatter::forest::{
    CameraPath, CorridorSpec, FlightPlan, ForestFlight, MotionLod, ScatterHit, SwaySettings, WakeModel,
    carve_corridor, corridor_report, flight_check, tune_fade_bands,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most hits `terrain.forest_query` returns.
const MAX_HITS: usize = 20_000;

fn missing() -> RpcError {
    RpcError::not_found(
        "no forest flythrough in this app",
        "the host inserts regia_terrain::scatter::forest::ForestFlight::with_query(..) and adds ForestFlightPlugin",
    )
}

fn no_query() -> RpcError {
    RpcError::not_found(
        "the forest flythrough has no scatter query",
        "insert ForestFlight with ForestFlight::with_query(ScatterQuery::new(set, source, library), ..)",
    )
}

fn vec3(params: &Value, name: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(v) = params.get(name) else {
        return Ok(None);
    };
    point(v, name).map(Some)
}

fn point(v: &Value, name: &str) -> Result<[f32; 3], RpcError> {
    let bad = || RpcError::invalid_params(format!("`{name}` must be [x, y, z] in metres"));
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        let f = x
            .as_f64()
            .filter(|f| f.is_finite() && f.abs() <= 1.0e7)
            .ok_or_else(bad)?;
        *o = f as f32;
    }
    Ok(out)
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f as f32)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number")))
        })
        .transpose()
}

fn boolean(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))
        })
        .transpose()
}

fn class_names(params: &Value) -> Result<Vec<String>, RpcError> {
    let Some(v) = params.get("classes") else {
        return Ok(Vec::new());
    };
    let bad = || RpcError::invalid_params("`classes` must be an array of class names");
    v.as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|c| c.as_str().map(str::to_string).ok_or_else(bad))
        .collect()
}

fn hit_json(flight: &ForestFlight, h: &ScatterHit) -> Value {
    let class = flight.query().map_or("", |q| q.class_name(h.class));
    json!({
        "class": class,
        "mesh": h.mesh,
        "id": h.id,
        "position": h.position,
        "height": h.height,
        "radius": h.radius(),
        "trunk_radius": h.obstacle.trunk_radius,
        "crown_base_y": h.obstacle.trunk_end[1],
        "crown_radius": h.obstacle.crown_radius,
        "top_y": h.obstacle.top_y,
    })
}

/// `terrain.forest_query`: `box_min` + `box_max`, or `from` + `to` (+ `radius`), and optional
/// `classes`, `limit`.
pub(crate) fn query(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let box_min = vec3(params, "box_min")?;
    let box_max = vec3(params, "box_max")?;
    let from = vec3(params, "from")?;
    let to = vec3(params, "to")?;
    let radius = number(params, "radius")?.unwrap_or(10.0);
    if !(0.0..=1000.0).contains(&radius) {
        return Err(RpcError::invalid_params(
            "`radius` must be within 0..=1000 metres",
        ));
    }
    let limit = match params.get("limit") {
        None => 1000,
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= 1)
            .map(|n| (n as usize).min(MAX_HITS))
            .ok_or_else(|| RpcError::invalid_params("`limit` must be a positive integer"))?,
    };
    let names = class_names(params)?;
    let flight = world.get_resource::<ForestFlight>().ok_or_else(missing)?;
    let q = flight.query().ok_or_else(no_query)?;
    let mut classes = Vec::with_capacity(names.len());
    for name in &names {
        classes.push(q.class_index(name).ok_or_else(|| {
            RpcError::not_found(
                format!("no scatter class `{name}`"),
                "terrain.scatter lists the classes",
            )
        })?);
    }
    let filter = (!classes.is_empty()).then_some(classes.as_slice());
    let (hits, count) = match (box_min, box_max, from, to) {
        (Some(lo), Some(hi), None, None) => {
            let area = ((hi[0] - lo[0]) * (hi[2] - lo[2])).abs();
            if area > 4.0e6 {
                return Err(RpcError::invalid_params(
                    "the box covers more than 4 km²; query smaller boxes",
                ));
            }
            let all = q.in_box(lo, hi, filter);
            let count = all.len();
            let hits: Vec<Value> = all.iter().take(limit).map(|h| hit_json(flight, h)).collect();
            (hits, count)
        }
        (None, None, Some(a), Some(b)) => {
            let len = ((b[0] - a[0]).powi(2) + (b[2] - a[2]).powi(2)).sqrt();
            if len > 5000.0 {
                return Err(RpcError::invalid_params(
                    "the segment is longer than 5 km; split it",
                ));
            }
            let all = q.along_segment(a, b, radius, filter);
            let count = all.len();
            let hits: Vec<Value> = all
                .iter()
                .take(limit)
                .map(|s| {
                    let mut v = hit_json(flight, &s.hit);
                    v["distance"] = json!(s.distance);
                    v["part"] = json!(s.part.name());
                    v["t"] = json!(s.t);
                    v
                })
                .collect();
            (hits, count)
        }
        _ => {
            return Err(RpcError::invalid_params(
                "pass `box_min` + `box_max`, or `from` + `to` (with an optional `radius`)",
            ));
        }
    };
    Ok(json!({ "count": count, "truncated": count > hits.len(), "hits": hits }))
}

fn path_and_spec(params: &Value) -> Result<(CameraPath, CorridorSpec), RpcError> {
    let raw = params
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`points` must be an array of [x, y, z] points"))?;
    if raw.len() < 2 {
        return Err(RpcError::invalid_params("`points` needs at least two points"));
    }
    let points = raw
        .iter()
        .map(|p| point(p, "points[]"))
        .collect::<Result<Vec<_>, _>>()?;
    let times = match params.get("times") {
        None => None,
        Some(v) => {
            let bad = || RpcError::invalid_params("`times` must be one finite number per point, in seconds");
            let a = v.as_array().filter(|a| a.len() == points.len()).ok_or_else(bad)?;
            Some(
                a.iter()
                    .map(|t| {
                        t.as_f64()
                            .filter(|f| f.is_finite())
                            .map(|f| f as f32)
                            .ok_or_else(bad)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
    };
    let length: f32 = points
        .windows(2)
        .map(|w| {
            ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2) + (w[1][2] - w[0][2]).powi(2)).sqrt()
        })
        .sum();
    if length > 50_000.0 {
        return Err(RpcError::invalid_params(
            "the path is longer than 50 km; split it",
        ));
    }
    let mut spec = CorridorSpec::default();
    if let Some(v) = number(params, "clearance_m")? {
        spec.clearance_m = v;
    }
    if let Some(v) = number(params, "ground_clearance_m")? {
        spec.ground_clearance_m = v;
    }
    if let Some(v) = number(params, "search_m")? {
        spec.search_m = v;
    }
    for (name, v) in [
        ("clearance_m", spec.clearance_m),
        ("ground_clearance_m", spec.ground_clearance_m),
        ("search_m", spec.search_m),
    ] {
        if !(0.0..=200.0).contains(&v) {
            return Err(RpcError::invalid_params(format!(
                "`{name}` must be within 0..=200 metres"
            )));
        }
    }
    spec.classes = class_names(params)?;
    Ok((CameraPath { points, times }, spec.sanitized()))
}

fn check_classes(flight: &ForestFlight, spec: &CorridorSpec) -> Result<(), RpcError> {
    let q = flight.query().ok_or_else(no_query)?;
    for name in &spec.classes {
        if q.class_index(name).is_none() {
            return Err(RpcError::not_found(
                format!("no scatter class `{name}`"),
                "terrain.scatter lists the classes",
            ));
        }
    }
    Ok(())
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v)
        .map_err(|e| RpcError::internal(format!("forest report serialization failed: {e}")))
}

/// `terrain.forest_corridor`: `points` (+ `times`, `clearance_m`, `ground_clearance_m`,
/// `search_m`, `classes`, `carve`).
pub(crate) fn corridor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (path, spec) = path_and_spec(params)?;
    let with_carve = boolean(params, "carve")?.unwrap_or(false);
    let flight = world.get_resource::<ForestFlight>().ok_or_else(missing)?;
    check_classes(flight, &spec)?;
    let q = flight.query().ok_or_else(no_query)?;
    if with_carve {
        let carve = carve_corridor(q, &path, &spec);
        Ok(json!({ "report": to_json(&carve.before)?, "carve": to_json(&carve.classes)? }))
    } else {
        Ok(json!({ "report": to_json(&corridor_report(q, &path, &spec))? }))
    }
}

/// `terrain.forest_carve`: like `terrain.forest_corridor`, then installs the carve into the
/// running field (rebuilt on the next frame) and returns the report after it.
pub(crate) fn carve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (path, spec) = path_and_spec(params)?;
    let mut flight = world.get_resource_mut::<ForestFlight>().ok_or_else(missing)?;
    check_classes(&flight, &spec)?;
    if !flight.can_install() {
        return Err(RpcError::not_found(
            "the forest flythrough cannot rebuild the field (no mesh library)",
            "insert ForestFlight::with_query(query, Some(library), key_base)",
        ));
    }
    let q = flight.query().ok_or_else(no_query)?;
    let carve = carve_corridor(q, &path, &spec);
    let mut set = q.set().clone();
    let added = carve.apply(&mut set);
    let ron = set
        .to_ron()
        .map_err(|e| RpcError::internal(format!("carved set does not serialize: {e}")))?;
    if added > 0 {
        flight
            .install_set(set)
            .map_err(|e| RpcError::internal(format!("carve install failed: {e}")))?;
    }
    let q = flight.query().ok_or_else(no_query)?;
    let after = corridor_report(q, &path, &spec);
    Ok(json!({
        "before": to_json(&carve.before)?,
        "carve": to_json(&carve.classes)?,
        "exclusions_added": added,
        "removed": carve.removed(),
        "after": to_json(&after)?,
        "scatter_ron": ron,
        "installed": added > 0,
    }))
}

fn state(flight: &ForestFlight) -> Result<Value, RpcError> {
    Ok(json!({
        "motion": to_json(&flight.motion)?,
        "sway": to_json(&flight.sway)?,
        "wake": to_json(&flight.wake)?,
        "fps": flight.fps,
        "meter_pops": flight.meter_pops,
        "pop_threshold_px": flight.pop_threshold_px,
        "has_query": flight.query().is_some(),
        "can_install": flight.can_install(),
        "pending_carve": flight.has_pending_set(),
        "stats": to_json(&flight.stats)?,
    }))
}

/// `terrain.forest_flight`: settings and last-frame stats; with `speed_m_s` (+ `fps`,
/// `height_px`, `fov_deg`), the flight-speed check of the set.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let speed = number(params, "speed_m_s")?;
    let fps = number(params, "fps")?;
    let height = number(params, "height_px")?;
    let fov = number(params, "fov_deg")?;
    let flight = world.get_resource::<ForestFlight>().ok_or_else(missing)?;
    let mut out = state(flight)?;
    if let Some(speed) = speed {
        if !(0.0..=10_000.0).contains(&speed) {
            return Err(RpcError::invalid_params("`speed_m_s` must be within 0..=10000"));
        }
        let d = FlightPlan::default();
        let plan = FlightPlan {
            speed_m_s: speed,
            fps: fps.unwrap_or(d.fps).clamp(1.0, 1000.0),
            height_px: height.unwrap_or(d.height_px).clamp(1.0, 100_000.0),
            fov_y: fov.map_or(d.fov_y, |f| f.clamp(1.0, 170.0).to_radians()),
        };
        let q = flight.query().ok_or_else(no_query)?;
        out["check"] = to_json(&flight_check(q.set(), flight.library(), &plan))?;
        // Dry run: the fade-band starts that would pass the check (edit the .scatter.ron to apply).
        out["band_changes"] = to_json(&tune_fade_bands(&mut q.set().clone(), &plan))?;
    }
    Ok(out)
}

fn object_or_bool<T: serde::de::DeserializeOwned + Default>(
    params: &Value,
    name: &str,
) -> Result<Option<Option<T>>, RpcError> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::Bool(false)) | Some(Value::Null) => Ok(Some(None)),
        Some(Value::Bool(true)) => Ok(Some(Some(T::default()))),
        Some(v @ Value::Object(_)) => serde_json::from_value(v.clone())
            .map(|t| Some(Some(t)))
            .map_err(|e| RpcError::invalid_params(format!("`{name}`: {e}"))),
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{name}` must be true, false or an object of settings"
        ))),
    }
}

/// `terrain.set_forest_flight`: any of `motion`, `sway` (true / false / settings object),
/// `wake` (object), `fps`, `meter_pops`, `pop_threshold_px`, `reset_pops`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let motion = object_or_bool::<MotionLod>(params, "motion")?;
    let sway = object_or_bool::<SwaySettings>(params, "sway")?;
    let wake = match params.get("wake") {
        None => None,
        Some(v @ Value::Object(_)) => Some(
            serde_json::from_value::<WakeModel>(v.clone())
                .map_err(|e| RpcError::invalid_params(format!("`wake`: {e}")))?,
        ),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`wake` must be an object of wake settings",
            ));
        }
    };
    let fps = number(params, "fps")?;
    if fps.is_some_and(|f| !(1.0..=1000.0).contains(&f)) {
        return Err(RpcError::invalid_params("`fps` must be within 1..=1000"));
    }
    let meter = boolean(params, "meter_pops")?;
    let threshold = number(params, "pop_threshold_px")?;
    if threshold.is_some_and(|t| !(0.0..=10_000.0).contains(&t)) {
        return Err(RpcError::invalid_params(
            "`pop_threshold_px` must be within 0..=10000",
        ));
    }
    let reset = boolean(params, "reset_pops")?.unwrap_or(false);
    if motion.is_none()
        && sway.is_none()
        && wake.is_none()
        && fps.is_none()
        && meter.is_none()
        && threshold.is_none()
        && !reset
    {
        return Err(RpcError::invalid_params(
            "pass at least one of `motion`, `sway`, `wake`, `fps`, `meter_pops`, `pop_threshold_px`, `reset_pops`",
        ));
    }
    let mut flight = world.get_resource_mut::<ForestFlight>().ok_or_else(missing)?;
    let before = state(&flight)?;
    if let Some(m) = motion {
        flight.motion = m.map(MotionLod::sanitized);
    }
    if let Some(s) = sway {
        flight.sway = s.map(SwaySettings::sanitized);
    }
    if let Some(w) = wake {
        flight.wake = w;
    }
    if let Some(f) = fps {
        flight.fps = f;
    }
    if let Some(m) = meter {
        flight.meter_pops = m;
    }
    if let Some(t) = threshold {
        flight.pop_threshold_px = t;
    }
    if reset {
        flight.reset_pops();
    }
    let after = state(&flight)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::scatter::forest::{ForestSpec, ScatterQuery, synthetic_forest};

    fn code(err: &RpcError) -> Option<Value> {
        err.data.as_ref().map(|d| d["code"].clone())
    }

    fn world() -> World {
        let forest = synthetic_forest(&ForestSpec {
            world_width_m: 512.0,
            samples: 65,
            ..ForestSpec::default()
        })
        .expect("forest");
        let q = ScatterQuery::new(&forest.set, forest.source.clone(), &forest.library).expect("query");
        let mut world = World::new();
        world.insert_resource(ForestFlight::with_query(
            q,
            Some(std::sync::Arc::new(forest.library)),
            0x1503,
        ));
        world
    }

    #[test]
    fn methods_answer_f0003_without_the_resource() {
        let mut empty = World::new();
        let params = json!({ "box_min": [0, 0, 0], "box_max": [10, 50, 10] });
        assert_eq!(
            code(&query(&mut empty, &params).expect_err("no resource")),
            Some(json!("F0003"))
        );
        let path = json!({ "points": [[0, 50, 0], [100, 50, 0]] });
        assert_eq!(
            code(&corridor(&mut empty, &path).expect_err("none")),
            Some(json!("F0003"))
        );
        assert_eq!(
            code(&carve(&mut empty, &path).expect_err("none")),
            Some(json!("F0003"))
        );
        assert_eq!(
            code(&get(&mut empty, &json!({})).expect_err("none")),
            Some(json!("F0003"))
        );
        assert_eq!(
            code(&set(&mut empty, &json!({ "meter_pops": true })).expect_err("none")),
            Some(json!("F0003"))
        );
    }

    #[test]
    fn query_corridor_and_carve_round_trip() {
        let mut world = world();
        let hits = query(
            &mut world,
            &json!({ "box_min": [-30, -100, -30], "box_max": [30, 500, 30] }),
        )
        .expect("box");
        assert!(hits["count"].as_u64().is_some_and(|n| n > 10), "{hits}");
        let seg = query(
            &mut world,
            &json!({ "from": [-60, 55, 0], "to": [60, 55, 0], "radius": 5, "classes": ["canopy_broadleaf"] }),
        )
        .expect("segment");
        assert!(
            seg["hits"]
                .as_array()
                .is_some_and(|h| h.iter().all(|x| x["class"] == "canopy_broadleaf"))
        );
        // A path through the crowns: violations, then a carve that clears them.
        let ground = |x: f32| {
            world
                .resource::<ForestFlight>()
                .query()
                .map_or(0.0, |q| q.source().map.sample_bilinear(x, 3.0))
        };
        let points: Vec<[f32; 3]> = (0..=8)
            .map(|i| {
                let x = -120.0 + 30.0 * i as f32;
                [x, ground(x) + 12.0, 3.0]
            })
            .collect();
        let params = json!({ "points": points, "clearance_m": 2.5 });
        let report = corridor(&mut world, &params).expect("corridor");
        assert!(
            report["report"]["obstacle_violations"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{report}"
        );
        let out = carve(&mut world, &params).expect("carve");
        assert_eq!(out["after"]["obstacle_violations"], json!(0), "{out}");
        assert_eq!(out["installed"], json!(true));
        assert!(world.resource::<ForestFlight>().has_pending_set());
        assert!(
            out["scatter_ron"]
                .as_str()
                .is_some_and(|r| r.contains("exclusions"))
        );
        // Errors.
        assert_eq!(
            code(&corridor(&mut world, &json!({ "points": [[0, 0, 0]] })).expect_err("one point")),
            Some(json!("F0002"))
        );
        assert_eq!(
            code(
                &corridor(&mut world, &json!({ "points": points, "classes": ["nope"] })).expect_err("class")
            ),
            Some(json!("F0003"))
        );
        assert_eq!(
            code(&query(&mut world, &json!({})).expect_err("no region")),
            Some(json!("F0002"))
        );
    }

    #[test]
    fn flight_settings_round_trip_and_the_check_runs() {
        let mut world = world();
        let out = set(
            &mut world,
            &json!({ "motion": true, "sway": { "max_tilt_rad": 0.2 }, "meter_pops": true }),
        )
        .expect("set");
        assert_eq!(out["after"]["motion"]["lookahead_frames"], json!(8.0));
        assert!((out["after"]["sway"]["max_tilt_rad"].as_f64().unwrap_or(0.0) - 0.2).abs() < 1e-6);
        assert_eq!(out["after"]["meter_pops"], json!(true));
        let read = get(&mut world, &json!({ "speed_m_s": 750.0 })).expect("get");
        let classes = read["check"]["classes"].as_array().map_or(0, Vec::len);
        assert_eq!(classes, 3);
        assert!(read["check"]["warned_classes"].as_u64().is_some_and(|n| n > 0));
        // At 750 m/s the understory's 40 m band is short: a wider band is proposed.
        assert!(
            read["band_changes"]
                .as_array()
                .is_some_and(|c| c.iter().any(|x| x["class"] == "understory")),
            "{read}"
        );
        let off = set(&mut world, &json!({ "motion": false })).expect("off");
        assert_eq!(off["after"]["motion"], Value::Null);
        assert_eq!(
            code(&set(&mut world, &json!({})).expect_err("empty")),
            Some(json!("F0002"))
        );
        assert_eq!(
            code(&set(&mut world, &json!({ "motion": 3 })).expect_err("bad")),
            Some(json!("F0002"))
        );
        assert_eq!(
            code(&set(&mut world, &json!({ "fps": 0 })).expect_err("fps")),
            Some(json!("F0002"))
        );
    }
}
