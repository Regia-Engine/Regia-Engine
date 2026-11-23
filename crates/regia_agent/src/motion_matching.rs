// SPDX-License-Identifier: MPL-2.0
//! Motion matching on the agent protocol (`book/agents/motion-matching.md`,
//! `docs/specs/52-motion-matching.md` § 5).
//!
//! - `anim.motion_match` reads one entity's [`regia_animation::motion_matching::MotionMatching`]:
//!   the database (clips, frames, feature layout, k-d tree shape), the matcher's settings, what
//!   plays, the counters and the latest search. With `trajectory` or `speed` it also runs a probe
//!   search for that path from the playing frame, without changing anything; with `candidates`
//!   it adds the candidate report (why each of the cheapest frames was or was not chosen).
//! - `anim.motion_match_set` changes settings, the desired path, the tag filter and a runtime
//!   weight overlay, requests a search or resets the matcher.
//! - `anim.motion_match_database` summarizes a database: layout, tags, spans, statistics hashes;
//!   by entity or by asset path (a registered or cooked database, ADR-2409).
//! - `anim.motion_match_databases` lists the registered databases and failed loads.
//! - `anim.mm_predict` reads an entity's
//!   [`regia_animation::motion_matching::TrajectoryPredictor`]: its desired velocity and facing,
//!   the simulated spring state, the predicted path and the recorded history (ADR-2858).

use std::sync::Arc;

use regia_animation::motion_matching::{
    CandidateRow, FLAG_PASS_THROUGH, FLAG_UNINDEXED, FacingMode, FeatureGroup, FeatureWeights,
    MotionDatabase, MotionDatabases, MotionMatchFootLocks, MotionMatchMontageScope, MotionMatching,
    SearchFilter, SearchHit, SearchMethod, TagTable, TrajectoryPoint, TrajectoryPredictor,
};
use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

fn method_name(method: SearchMethod) -> &'static str {
    method.name()
}

fn hit_json(database: &MotionDatabase, hit: Option<SearchHit>) -> Value {
    let Some(hit) = hit else {
        return Value::Null;
    };
    let frame = database.frame(hit.frame);
    json!({
        "frame": hit.frame,
        "clip": frame.map(|f| f.clip),
        "clip_name": frame.and_then(|f| database.clips().get(f.clip)).map(|c| c.name.as_str()),
        "time": frame.map(|f| f.time),
        "cost": hit.cost,
    })
}

/// The groups the schema lays out: the five baseline groups always (old output unchanged), the
/// history and pose groups only when present.
fn groups_json(database: &MotionDatabase) -> Map<String, Value> {
    let schema = database.schema();
    FeatureGroup::ALL
        .iter()
        .filter(|&&group| FeatureGroup::BASELINE.contains(&group) || !schema.group_range(group).is_empty())
        .map(|&group| {
            let range = schema.group_range(group);
            (
                group.name().to_string(),
                json!({ "start": range.start, "len": range.len(), "weight": schema.weights.get(group) }),
            )
        })
        .collect()
}

fn database_json(database: &MotionDatabase) -> Value {
    let schema = database.schema();
    let kd = database.kd_stats();
    let mut out = json!({
        "frames": database.len(),
        "dimensions": database.dimensions(),
        "sample_rate": schema.sample_rate,
        "trajectory_times": schema.trajectory_times,
        "root_joint": schema.root_joint,
        "feet": schema.feet,
        "groups": groups_json(database),
        "clips": database.clips().iter().map(|c| json!({
            "name": c.name,
            "duration": c.duration,
            "looping": c.looping,
            "start": c.start,
            "frames": c.frames,
        })).collect::<Vec<_>>(),
        "kd": { "nodes": kd.nodes, "leaves": kd.leaves, "depth": kd.depth },
    });
    if !schema.history_times.is_empty() {
        out["history_times"] = json!(schema.history_times);
    }
    if !schema.pose_joints.is_empty() {
        out["pose_joints"] = schema
            .pose_joints
            .iter()
            .map(|j| json!({ "joint": j.joint, "position": j.position, "velocity": j.velocity, "weight": j.weight }))
            .collect();
    }
    if let Some(c) = &schema.contacts {
        out["contacts"] = json!({
            "height_on": c.height_on,
            "height_off": c.height_off,
            "speed_on": c.speed_on,
            "speed_off": c.speed_off,
            "phase": c.phase,
        });
    }
    out
}

/// The database's build warnings: `[{ clip, clip_name, kind }]`.
fn warnings_json(database: &MotionDatabase) -> Vec<Value> {
    database
        .warnings()
        .iter()
        .map(|w| {
            json!({
                "clip": w.clip,
                "clip_name": database.clips().get(w.clip).map(|c| c.name.as_str()),
                "kind": w.kind.name(),
            })
        })
        .collect()
}

fn settings_json(settings: &regia_animation::motion_matching::MatcherSettings) -> Value {
    json!({
        "search_interval": settings.search_interval,
        "min_jump_interval": settings.min_jump_interval,
        "transition_cost": settings.transition_cost,
        "exclude_window": settings.exclude_window,
        "method": method_name(settings.method),
        "stay_bias": settings.stay_bias,
        "loop_clip_bias": settings.loop_clip_bias,
        "lockout_window": settings.lockout_window,
        "urgent_threshold": settings.urgent_threshold,
        "min_search_gap": settings.min_search_gap,
        "stagger": settings.stagger,
        "play_rate": settings.play_rate,
        "live_pose_blend": settings.live_pose_blend,
    })
}

fn vec2(value: &Value, what: &str) -> Result<Vec2, RpcError> {
    let bad = || {
        RpcError::invalid_params(format!(
            "[bad_trajectory] `{what}` must be [x, z], two finite numbers"
        ))
    };
    let items = value.as_array().ok_or_else(bad)?;
    if items.len() != 2 {
        return Err(bad());
    }
    let x = items[0].as_f64().filter(|v| v.is_finite()).ok_or_else(bad)?;
    let z = items[1].as_f64().filter(|v| v.is_finite()).ok_or_else(bad)?;
    Ok(Vec2::new(x as f32, z as f32))
}

/// A path: `[{ position: [x, z], direction?: [x, z] }]`, one point per trajectory time.
fn trajectory_points(points: &Value, times: &[f32]) -> Result<Vec<TrajectoryPoint>, RpcError> {
    let points = points.as_array().ok_or_else(|| {
        RpcError::invalid_params("[bad_trajectory] `trajectory` must be an array of points")
    })?;
    if points.len() != times.len() {
        return Err(RpcError::invalid_params(format!(
            "[bad_trajectory] `trajectory` has {} points, the database has {} trajectory times",
            points.len(),
            times.len()
        )));
    }
    points
        .iter()
        .map(|point| {
            let position = vec2(point.get("position").unwrap_or(&Value::Null), "position")?;
            let direction = match point.get("direction") {
                Some(d) => vec2(d, "direction")?,
                None => Vec2::Y,
            };
            Ok(TrajectoryPoint { position, direction })
        })
        .collect()
}

/// The probe path: `trajectory: [{ position: [x, z], direction?: [x, z] }]` or `speed` (meters
/// per second straight ahead at every trajectory time). `None` when neither is given.
fn probe_trajectory(params: &Value, times: &[f32]) -> Result<Option<Vec<TrajectoryPoint>>, RpcError> {
    match (params.get("trajectory"), params.get("speed")) {
        (Some(_), Some(_)) => Err(RpcError::invalid_params(
            "[bad_probe] pass `trajectory` or `speed`, not both",
        )),
        (None, None) => Ok(None),
        (None, Some(speed)) => {
            let speed = speed
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(|| RpcError::invalid_params("[bad_speed] `speed` must be a finite number"))?
                as f32;
            Ok(Some(
                times.iter().map(|&t| TrajectoryPoint::ahead(speed * t)).collect(),
            ))
        }
        (Some(points), None) => trajectory_points(points, times).map(Some),
    }
}

/// `filter: { require?: [tag], forbid?: [tag] }` (absent = `None`). Unknown names are
/// `[unknown_tag]`.
fn filter_param(params: &Value, table: &TagTable) -> Result<Option<SearchFilter>, RpcError> {
    filter_object(params, "filter", table)
}

/// `params[key]` as `{ require?: [tag], forbid?: [tag] }` (absent or null = `None`).
fn filter_object(params: &Value, key: &str, table: &TagTable) -> Result<Option<SearchFilter>, RpcError> {
    let Some(filter) = params.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if !filter.is_object() {
        return Err(RpcError::invalid_params(format!(
            "[unknown_tag] `{key}` must be {{ require?: [tag], forbid?: [tag] }}"
        )));
    }
    let side = |name: &str| -> Result<u32, RpcError> {
        let Some(value) = filter.get(name).filter(|v| !v.is_null()) else {
            return Ok(0);
        };
        let names: Vec<&str> = value
            .as_array()
            .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "[unknown_tag] `{key}.{name}` must be an array of tag names"
                ))
            })?;
        table
            .mask(&names)
            .map_err(|e| RpcError::invalid_params(format!("[unknown_tag] {e}")))
    };
    Ok(Some(SearchFilter {
        require: side("require")?,
        forbid: side("forbid")?,
    }))
}

fn filter_json(table: &TagTable, filter: SearchFilter) -> Value {
    json!({ "require": table.names_of(filter.require), "forbid": table.names_of(filter.forbid) })
}

fn row_json(database: &MotionDatabase, row: &CandidateRow) -> Value {
    let groups: Map<String, Value> = FeatureGroup::ALL
        .iter()
        .filter(|&&g| FeatureGroup::BASELINE.contains(&g) || !database.schema().group_range(g).is_empty())
        .map(|&g| (g.name().to_string(), json!(row.parts.groups[g.index()])))
        .collect();
    json!({
        "frame": row.frame,
        "clip": row.clip,
        "clip_name": database.clips().get(row.clip).map(|c| c.name.as_str()),
        "time": row.time,
        "mirrored": row.mirrored,
        "total": row.total,
        "distance": row.parts.total,
        "groups": groups,
        "bias": row.parts.bias,
        "extra_bias": row.extra_bias,
        "transition": row.transition,
        "status": row.status.name(),
    })
}

fn missing_entity(entity: Entity) -> RpcError {
    RpcError::not_found(
        format!("entity {} does not exist", entity.to_bits()),
        "list entities with scene.query_entities",
    )
}

fn missing_matcher(entity: Entity) -> RpcError {
    RpcError::not_found(
        format!("entity {} has no MotionMatching", entity.to_bits()),
        "insert regia_animation::motion_matching::MotionMatching and install MotionMatchPlugin (or add step_motion_matching to Update)",
    )
}

/// `anim.motion_match { entity, trajectory?, speed?, candidates?, filter? }`: the entity's
/// motion-matching database, settings, playback, counters and latest search; with a probe path,
/// also the frame that path would pick from the playing frame (`probe`, using `filter` when
/// given, else the matcher's filter), computed without changing the matcher; with `candidates:
/// n` (1..32) the candidate report of the latest search (statuses judged against `filter` when
/// given).
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(missing_entity(entity));
    }
    let matching = world
        .get::<MotionMatching>(entity)
        .ok_or_else(|| missing_matcher(entity))?;
    let database = &matching.database;
    let table = database.tag_table();
    let filter = filter_param(params, table)?;
    let candidates = match params.get("candidates").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_u64().filter(|n| (1..=32).contains(n)).ok_or_else(|| {
            RpcError::invalid_params("[bad_candidates] `candidates` must be an integer from 1 to 32")
        })? as usize),
    };
    let probe = match probe_trajectory(params, &database.schema().trajectory_times)? {
        None => Value::Null,
        Some(trajectory) => {
            let playing = matching.matcher.playback();
            let query = match playing {
                Some(p) => database
                    .query_from_frame(p.frame, &trajectory)
                    .map_err(|e| RpcError::internal(e.to_string()))?,
                None => database.query_with_mean_pose(&trajectory),
            };
            let normalized = database
                .normalize(&query)
                .map_err(|e| RpcError::invalid_params(format!("[bad_trajectory] {e}")))?;
            let outcome = database.search_detailed(
                &normalized,
                regia_animation::motion_matching::Exclusion::NONE,
                filter.unwrap_or(matching.matcher.filter),
                matching.matcher.settings.loop_clip_bias,
                |_| false,
                matching.matcher.settings.method,
            );
            json!({
                "best": hit_json(database, outcome.hit),
                "continuation_cost": playing.and_then(|p| database.cost(&normalized, p.frame)),
                "method_used": method_name(outcome.method_used),
            })
        }
    };
    let candidates_json = candidates.map(|k| {
        let mut matcher = matching.matcher.clone();
        if let Some(f) = filter {
            matcher.filter = f;
        }
        match matcher.candidate_report(database, k) {
            None => Value::Array(Vec::new()),
            Some(report) => report
                .rows
                .iter()
                .chain(report.continuation.iter())
                .map(|row| row_json(database, row))
                .collect(),
        }
    });
    let settings = matching.matcher.settings;
    let playback = matching.matcher.playback().map_or(Value::Null, |p| {
        let mut out = json!({
            "clip": p.clip,
            "clip_name": database.clips().get(p.clip).map(|c| c.name.as_str()),
            "time": p.time,
            "frame": p.frame,
        });
        // The contact and phase channels of the playing frame (only with contacts: old output
        // unchanged).
        if database.schema().contacts.is_some() {
            out["contacts"] = json!(database.contacts(p.frame).unwrap_or_default());
            if database.schema().phase_count() > 0 {
                out["phases"] = json!(database.phases(p.frame).unwrap_or_default());
            }
        }
        out
    });
    let last = matching.matcher.last_search();
    let last_search = last.map_or(Value::Null, |r| {
        json!({
            "best": hit_json(database, r.best),
            "continuation_cost": r.continuation_cost,
            "jumped": r.jumped,
            "held": r.held,
            "reason": r.reason.name(),
            "urgent": r.urgent,
            "method_used": method_name(r.method_used),
            "nodes_visited": r.work.nodes_visited,
            "frames_scanned": r.work.rows_scanned,
        })
    });
    let mut warnings: Vec<Value> = Vec::new();
    if last.is_some_and(|r| r.filter_ignored) {
        warnings.push(json!({ "kind": "filter_ignored" }));
    }
    warnings.extend(warnings_json(database));
    let mut out = json!({
        "entity": entity.to_bits(),
        "database": database_json(database),
        "settings": settings_json(&settings),
        "tags": {
            "names": table.names(),
            "annotated": database.is_annotated(),
            "require": table.names_of(matching.matcher.filter.require),
            "forbid": table.names_of(matching.matcher.filter.forbid),
        },
        "trajectory": matching.trajectory.iter().map(|p| json!({
            "position": [p.position.x, p.position.y],
            "direction": [p.direction.x, p.direction.y],
        })).collect::<Vec<_>>(),
        "playback": playback,
        "searches": matching.matcher.searches(),
        "jumps": matching.matcher.jumps(),
        "deferrals": matching.matcher.deferrals(),
        "weights_overlay": matching.base_database.is_some(),
        "last_search": last_search,
        "last_error": matching.last_error.as_ref().map(ToString::to_string),
        "warnings": warnings,
        "probe": probe,
    });
    if let Some(candidates) = candidates_json {
        out["candidates"] = candidates;
    }
    if let Some(scope) = matching.matcher.scope {
        out["scope"] = filter_json(table, scope);
    }
    if let Some(rules) = world.get::<MotionMatchMontageScope>(entity) {
        out["montage_scope"] = json!({
            "rules": rules.rules.iter().map(|r| json!({
                "montage": r.montage,
                "section": r.section,
                "filter": filter_json(table, r.filter),
            })).collect::<Vec<_>>(),
            "active": rules.active,
        });
    }
    if let Some(locks) = world.get::<MotionMatchFootLocks>(entity) {
        out["foot_locks"] = locks
            .locks()
            .iter()
            .map(|l| json!({ "planted": l.planted, "target": l.target.to_array(), "weight": l.weight }))
            .collect();
    }
    if world.get::<TrajectoryPredictor>(entity).is_some() {
        // The path is written by a predictor (read it with anim.mm_predict).
        out["predictor"] = json!(true);
    }
    Ok(out)
}

fn number_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("[motion_bad_number] `{name}` must be a finite number"))
            }),
    }
}

/// The mask of the tag names in `params[name]` (an array of strings; absent = `None`).
fn tag_mask_param(params: &Value, name: &str, table: &TagTable) -> Result<Option<u32>, RpcError> {
    let Some(value) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(RpcError::invalid_params(format!(
            "[motion_bad_tags] `{name}` must be an array of tag names"
        )));
    };
    let names: Vec<&str> = items
        .iter()
        .map(|v| {
            v.as_str().ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "[motion_bad_tags] `{name}` must be an array of tag names"
                ))
            })
        })
        .collect::<Result<_, _>>()?;
    table
        .mask(&names)
        .map(Some)
        .map_err(|e| RpcError::invalid_params(format!("[motion_tag_unknown] {e}")))
}

/// Apply one setting by name; `[bad_setting]` for an unknown name or a bad value.
fn apply_setting(
    settings: &mut regia_animation::motion_matching::MatcherSettings,
    name: &str,
    value: &Value,
) -> Result<(), RpcError> {
    let bad = |what: &str| RpcError::invalid_params(format!("[bad_setting] `{name}` {what}"));
    let number = || {
        value
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .ok_or_else(|| bad("must be a finite number"))
    };
    match name {
        "search_interval" => settings.search_interval = number()?,
        "min_jump_interval" => settings.min_jump_interval = number()?,
        "transition_cost" => settings.transition_cost = number()?,
        "exclude_window" => settings.exclude_window = number()?,
        "stay_bias" => settings.stay_bias = number()?,
        "loop_clip_bias" => settings.loop_clip_bias = number()?,
        "lockout_window" => settings.lockout_window = number()?,
        "min_search_gap" => settings.min_search_gap = number()?,
        "play_rate" => settings.play_rate = number()?,
        "live_pose_blend" => settings.live_pose_blend = number()?,
        "urgent_threshold" => {
            settings.urgent_threshold = if value.is_null() { None } else { Some(number()?) };
        }
        "stagger" => settings.stagger = value.as_bool().ok_or_else(|| bad("must be true or false"))?,
        "method" => {
            settings.method = value
                .as_str()
                .and_then(SearchMethod::from_name)
                .ok_or_else(|| bad("must be brute_force, kd_tree or auto"))?;
        }
        _ => return Err(bad("is not a matcher setting")),
    }
    Ok(())
}

/// `weights: { group: value }` replacing the build weights of the named groups.
fn weights_param(params: &Value, base: FeatureWeights) -> Result<Option<FeatureWeights>, RpcError> {
    let Some(value) = params.get("weights") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(base));
    }
    let Some(map) = value.as_object() else {
        return Err(RpcError::invalid_params(
            "[bad_weight] `weights` must be { group: value }",
        ));
    };
    let mut weights = base;
    for (name, v) in map {
        let group = FeatureGroup::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("[bad_weight] `{name}` is not a feature group"))
        })?;
        let w = v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite() && (0.0..=1000.0).contains(n))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "[bad_weight] `{name}` must be a finite number from 0 to 1000"
                ))
            })?;
        *weights.get_mut(group) = w;
    }
    Ok(Some(weights))
}

/// `anim.motion_match_set { entity, ... }`: change matcher settings (top-level names, kept from
/// the first version, or a `settings` object with any setting), the desired `trajectory`, the
/// tag filter (`filter: { require, forbid }`, or the older top-level `require` / `forbid`), the
/// runtime `weights` overlay (`null` removes it), request a search (`search: "now"`) or `reset`.
/// Everything is parsed and validated before anything changes. Returns what was applied and the
/// settings and filter after the change.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(missing_entity(entity));
    }
    let Some(matching) = world.get::<MotionMatching>(entity) else {
        return Err(missing_matcher(entity));
    };
    let mut applied: Vec<String> = Vec::new();
    let mut settings = matching.matcher.settings;
    let mut filter = matching.matcher.filter;
    let table = matching.database.tag_table().clone();
    for name in [
        "search_interval",
        "min_jump_interval",
        "transition_cost",
        "exclude_window",
        "stay_bias",
        "loop_clip_bias",
        "lockout_window",
    ] {
        if let Some(value) = number_param(params, name)? {
            apply_setting(&mut settings, name, &json!(value))?;
            applied.push(name.to_string());
        }
    }
    if let Some(method) = params.get("method").filter(|v| !v.is_null()) {
        let name = method.as_str().unwrap_or_default();
        settings.method = SearchMethod::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "[motion_bad_method] `method` is brute_force, kd_tree or auto, got {method}"
            ))
        })?;
        applied.push("method".into());
    }
    if let Some(object) = params.get("settings").filter(|v| !v.is_null()) {
        let Some(map) = object.as_object() else {
            return Err(RpcError::invalid_params(
                "[bad_setting] `settings` must be { name: value }",
            ));
        };
        for (name, value) in map {
            apply_setting(&mut settings, name, value)?;
            applied.push(name.clone());
        }
    }
    if let Some(mask) = tag_mask_param(params, "require", &table)? {
        filter.require = mask;
        applied.push("require".into());
    }
    if let Some(mask) = tag_mask_param(params, "forbid", &table)? {
        filter.forbid = mask;
        applied.push("forbid".into());
    }
    if let Some(f) = filter_param(params, &table)? {
        filter = f;
        applied.push("filter".into());
    }
    // `scope: { require, forbid }` sets the montage-section scope by hand; `null` clears it.
    let scope = match params.get("scope") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(_) => Some(filter_object(params, "scope", &table)?),
    };
    let predict = predict_param(params)?;
    if predict.is_some() && world.get::<TrajectoryPredictor>(entity).is_none() {
        return Err(RpcError::invalid_params(
            "[no_predictor] `predict` needs a TrajectoryPredictor on the entity",
        ));
    }
    let times = matching.database.schema().trajectory_times.clone();
    let trajectory = match params.get("trajectory").filter(|v| !v.is_null()) {
        Some(points) => Some(trajectory_points(points, &times)?),
        None => None,
    };
    let base_weights = matching
        .base_database
        .as_ref()
        .unwrap_or(&matching.database)
        .schema()
        .weights;
    let weights = weights_param(params, base_weights)?;
    let search_now = match params.get("search").filter(|v| !v.is_null()) {
        None => false,
        Some(v) if v.as_str() == Some("now") => true,
        Some(_) => {
            return Err(RpcError::invalid_params("[bad_setting] `search` must be \"now\""));
        }
    };
    let reset = match params.get("reset").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("[bad_setting] `reset` must be true or false"))?,
    };
    // Validate the overlay before changing anything.
    let overlay = match weights {
        Some(w) => {
            let base = matching
                .base_database
                .clone()
                .unwrap_or_else(|| matching.database.clone());
            if w != base.schema().weights {
                base.with_weight_overlay(w)
                    .map_err(|e| RpcError::invalid_params(format!("[bad_weight] {e}")))?;
            }
            Some(w)
        }
        None => None,
    };
    if let Some(predict) = predict
        && let Some(mut predictor) = world.get_mut::<TrajectoryPredictor>(entity)
    {
        predict.apply(&mut predictor);
        applied.push("predict".into());
    }
    let Some(mut matching) = world.get_mut::<MotionMatching>(entity) else {
        return Err(RpcError::internal("the motion matcher vanished"));
    };
    matching.matcher.settings = settings;
    matching.matcher.filter = filter;
    if let Some(scope) = scope {
        matching.matcher.scope = scope;
        applied.push("scope".into());
    }
    if let Some(trajectory) = trajectory {
        matching.trajectory = trajectory;
        applied.push("trajectory".into());
    }
    if let Some(w) = overlay {
        matching
            .set_weight_overlay(Some(w))
            .map_err(|e| RpcError::invalid_params(format!("[bad_weight] {e}")))?;
        applied.push("weights".into());
    }
    if reset {
        matching.matcher.reset();
        applied.push("reset".into());
    }
    if search_now {
        matching.matcher.request_search();
        applied.push("search".into());
    }
    let method_used = matching.database.resolve_method(settings.method, filter);
    let mut out = json!({
        "entity": entity.to_bits(),
        "applied": applied,
        "settings": settings_json(&settings),
        "filter": filter_json(&table, filter),
        "require": table.names_of(filter.require),
        "forbid": table.names_of(filter.forbid),
        "weights_overlay": matching.base_database.is_some(),
        "method_used": method_name(method_used),
    });
    if let Some(scope) = matching.matcher.scope {
        out["scope"] = filter_json(&table, scope);
    }
    Ok(out)
}

/// The parsed `predict` object of `anim.motion_match_set`.
struct PredictParams {
    velocity: Option<Vec3>,
    facing: Option<FacingMode>,
    aim_yaw: Option<f32>,
    turn_rate: Option<f32>,
}

impl PredictParams {
    fn apply(&self, predictor: &mut TrajectoryPredictor) {
        if let Some(v) = self.velocity {
            predictor.desired_velocity = v;
        }
        if let Some(f) = self.facing {
            predictor.facing = f;
        }
        if let Some(a) = self.aim_yaw {
            predictor.aim_yaw = a;
        }
        if let Some(w) = self.turn_rate {
            predictor.desired_turn_rate = w;
        }
    }
}

/// `predict: { velocity?: [x, z], facing?: "velocity" | "aim", aim_yaw? }` (`[bad_predict]`).
fn predict_param(params: &Value) -> Result<Option<PredictParams>, RpcError> {
    let Some(value) = params.get("predict").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = |what: &str| RpcError::invalid_params(format!("[bad_predict] {what}"));
    let Some(map) = value.as_object() else {
        return Err(bad("`predict` must be { velocity?, facing?, aim_yaw? }"));
    };
    let mut out = PredictParams {
        velocity: None,
        facing: None,
        aim_yaw: None,
        turn_rate: None,
    };
    for (name, v) in map {
        match name.as_str() {
            "velocity" => {
                let xz = v
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .and_then(|a| Some((a[0].as_f64()? as f32, a[1].as_f64()? as f32)))
                    .filter(|(x, z)| x.is_finite() && z.is_finite() && x.abs() <= 100.0 && z.abs() <= 100.0)
                    .ok_or_else(|| bad("`predict.velocity` must be [x, z] in m/s, each within +-100"))?;
                out.velocity = Some(Vec3::new(xz.0, 0.0, xz.1));
            }
            "facing" => {
                out.facing = Some(
                    v.as_str()
                        .and_then(FacingMode::from_name)
                        .ok_or_else(|| bad("`predict.facing` must be \"velocity\" or \"aim\""))?,
                );
            }
            "aim_yaw" => {
                out.aim_yaw = Some(
                    v.as_f64()
                        .map(|n| n as f32)
                        .filter(|n| n.is_finite())
                        .ok_or_else(|| bad("`predict.aim_yaw` must be a finite number (radians)"))?,
                );
            }
            "turn_rate" => {
                out.turn_rate = Some(
                    v.as_f64()
                        .map(|n| n as f32)
                        .filter(|n| {
                            n.is_finite() && n.abs() <= regia_animation::motion_matching::MAX_TURN_RATE
                        })
                        .ok_or_else(|| {
                            bad("`predict.turn_rate` must be a finite number within +-100 rad/s")
                        })?,
                );
            }
            other => return Err(bad(&format!("`predict.{other}` is not a predictor input"))),
        }
    }
    Ok(Some(out))
}

/// `anim.mm_predict { entity, times? }`: the entity's trajectory predictor: inputs, the spring
/// state, the predicted path at `times` (default: the matcher's trajectory times, or 1/3, 2/3
/// and 1 s) and the recorded history (newest first, at most 32 samples).
pub(crate) fn predict(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(missing_entity(entity));
    }
    let predictor = world.get::<TrajectoryPredictor>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no TrajectoryPredictor", entity.to_bits()),
            "insert regia_animation::motion_matching::TrajectoryPredictor next to MotionMatching and install MotionMatchPlugin",
        )
    })?;
    let matcher_times = world
        .get::<MotionMatching>(entity)
        .map(|m| m.database.schema().trajectory_times.clone());
    let times: Vec<f32> = match params.get("times").filter(|v| !v.is_null()) {
        Some(v) => v
            .as_array()
            .filter(|a| !a.is_empty() && a.len() <= 16)
            .and_then(|a| {
                a.iter()
                    .map(|t| {
                        t.as_f64()
                            .map(|n| n as f32)
                            .filter(|n| n.is_finite() && *n > 0.0 && *n <= 10.0)
                    })
                    .collect::<Option<Vec<f32>>>()
            })
            .ok_or_else(|| {
                RpcError::invalid_params("[bad_times] `times` must be 1 to 16 numbers in (0, 10] seconds")
            })?,
        None => matcher_times
            .clone()
            .unwrap_or_else(|| regia_animation::motion_matching::DEFAULT_TRAJECTORY_TIMES.to_vec()),
    };
    let s = predictor.state;
    let path = predictor.trajectory(&times);
    Ok(json!({
        "entity": entity.to_bits(),
        "desired_velocity": [predictor.desired_velocity.x, predictor.desired_velocity.z],
        "facing": predictor.facing.name(),
        "aim_yaw": predictor.aim_yaw,
        "desired_turn_rate": predictor.desired_turn_rate,
        "desired_yaw": predictor.desired_yaw(),
        "settings": {
            "velocity_halflife": predictor.settings.velocity_halflife,
            "facing_halflife": predictor.settings.facing_halflife,
            "history_seconds": predictor.settings.history_seconds,
        },
        "state": {
            "position": s.position.to_array(),
            "velocity": [s.velocity.x, s.velocity.z],
            "yaw": s.yaw,
            "yaw_rate": s.yaw_rate,
        },
        "trajectory": times.iter().zip(&path).map(|(t, p)| json!({
            "time": t,
            "position": [p.position.x, p.position.y],
            "direction": [p.direction.x, p.direction.y],
        })).collect::<Vec<_>>(),
        "history": predictor.history().take(32).map(|(ago, position, yaw)| json!({
            "ago": ago,
            "position": [position.x, position.z],
            "yaw": yaw,
        })).collect::<Vec<_>>(),
        "drives_matcher": matcher_times.is_some(),
    }))
}

/// The summary of one database (shared by the entity and the asset-path forms).
fn database_summary(database: &MotionDatabase) -> Map<String, Value> {
    let table = database.tag_table();
    let tags: Map<String, Value> = table
        .names()
        .iter()
        .enumerate()
        .map(|(bit, name)| (name.clone(), json!(database.tag_count(bit))))
        .collect();
    let (mut pass_through, mut unindexed, mut biased) = (0usize, 0usize, 0usize);
    for frame in 0..database.len() {
        let meta = database.frame_meta(frame).unwrap_or_default();
        pass_through += usize::from(meta.flags & FLAG_PASS_THROUGH != 0);
        unindexed += usize::from(meta.flags & FLAG_UNINDEXED != 0);
        biased += usize::from(meta.bias != 0.0);
    }
    let kd = database.kd_stats();
    let summary = json!({
        "frames": database.len(),
        "dimensions": database.dimensions(),
        "padded": database.schema().padded_dimensions(),
        "groups": groups_json(database),
        "clips": database.clips().iter().map(|c| json!({
            "name": c.name,
            "duration": c.duration,
            "looping": c.looping,
            "start": c.start,
            "frames": c.frames,
            "distance_matched": c.distance_matched,
        })).collect::<Vec<_>>(),
        "tags": tags,
        "spans": { "pass_through_frames": pass_through, "unindexed_frames": unindexed, "biased_frames": biased },
        "targets": database.target_count(),
        "normalization": database.schema().normalization.name(),
        "mean_scale_hash": format!("{:016x}", database.mean_scale_hash()),
        "content_hash": format!("{:016x}", database.content_hash()),
        "kd": { "nodes": kd.nodes, "leaves": kd.leaves, "depth": kd.depth, "leaf_size": database.kd_leaf_size() },
        "median_jump_cost": database.median_jump_cost(),
        "warnings": warnings_json(database),
        "cooked_arrays": cooked_arrays_json(database),
    });
    match summary {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Which optional cooked arrays the database carries (spec 52 § 4, ADR-7817): the per-frame
/// clip speed (always: a payload without it gets it from the raw features at load), and the
/// clips with stored arc lengths (the player samples the others on first use, with the same
/// result).
fn cooked_arrays_json(database: &MotionDatabase) -> Value {
    let arc_length_clips: Vec<&str> = database
        .clips()
        .iter()
        .enumerate()
        .filter(|(clip, _)| database.cooked_arc_lengths(*clip).is_some())
        .map(|(_, c)| c.name.as_str())
        .collect();
    json!({
        "clip_speed": database.clip_speeds().len() == database.len(),
        "arc_length_clips": arc_length_clips,
    })
}

/// The registered asset path of the database an entity's matcher shares, if any.
fn asset_path_of(world: &World, database: &Arc<MotionDatabase>) -> Value {
    world
        .get_resource::<MotionDatabases>()
        .and_then(|d| d.path_of_database(database))
        .map_or(Value::Null, |p| json!(p))
}

/// `anim.motion_match_database { entity }` (or `database: <entity>` or `database: "<asset
/// path>"`): a database summary. An asset path is looked up in the app's
/// [`MotionDatabases`] and, when it is not there and the app has an asset server, loaded from
/// the cooked cache or the paks (`[unknown_database]` when neither has it).
pub(crate) fn database(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = match (params.get("entity"), params.get("database")) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params(
                "[bad_params] pass `entity` or `database`, not both",
            ));
        }
        (None, None) => {
            return Err(RpcError::invalid_params(
                "[bad_params] pass `entity`, or `database`: an asset path or an entity with MotionMatching",
            ));
        }
        (Some(_), None) => crate::scene::entity_from(params)?,
        (None, Some(Value::String(path))) => return database_by_path(world, path),
        (None, Some(value)) => crate::scene::entity_from(&json!({ "entity": value }))?,
    };
    if world.get_entity(target).is_err() {
        return Err(missing_entity(target));
    }
    let matching = world
        .get::<MotionMatching>(target)
        .ok_or_else(|| missing_matcher(target))?;
    let database = matching
        .base_database
        .as_ref()
        .unwrap_or(&matching.database)
        .clone();
    let mut out = database_summary(&database);
    out.insert("entity".into(), json!(target.to_bits()));
    out.insert("asset".into(), asset_path_of(world, &database));
    Ok(Value::Object(out))
}

/// The asset-path form of [`database`].
fn database_by_path(world: &mut World, path: &str) -> Result<Value, RpcError> {
    if path.trim().is_empty() {
        return Err(RpcError::invalid_params(
            "[bad_params] `database` is an empty path",
        ));
    }
    world.init_resource::<MotionDatabases>();
    let known = world
        .get_resource::<MotionDatabases>()
        .is_some_and(|d| d.get(path).is_some());
    if !known {
        let loaded = world.resource_scope(|world, mut databases: Mut<MotionDatabases>| {
            world
                .get_resource_mut::<regia_asset::AssetServer>()
                .map(|mut server| databases.load_from_server(&mut server, path).map(|_| ()))
        });
        match loaded {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                return Err(RpcError::invalid_params(format!(
                    "[{}] motion database {path:?}: {error}",
                    error.code()
                )));
            }
            None => {
                return Err(RpcError::invalid_params(format!(
                    "[unknown_database] no motion database asset named {path:?} is loaded (and the app has no asset server); \
                     pass the entity that uses it"
                )));
            }
        }
    }
    let databases = world.resource::<MotionDatabases>();
    let Some(loaded) = databases.get(path) else {
        return Err(RpcError::invalid_params(format!(
            "[unknown_database] no motion database asset named {path:?}"
        )));
    };
    let mut out = database_summary(&loaded.database);
    out.insert("entity".into(), Value::Null);
    out.insert("asset".into(), json!(path));
    out.insert(
        "id".into(),
        loaded.id.map_or(Value::Null, |id| json!(id.to_string())),
    );
    out.insert("generation".into(), json!(loaded.generation));
    out.insert(
        "playable".into(),
        json!(loaded.clips.is_some() && loaded.skeleton.is_some()),
    );
    Ok(Value::Object(out))
}

/// `anim.motion_match_databases`: every motion database the app has registered (by asset path)
/// and the paths that failed to load, with their reason codes.
pub(crate) fn databases(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(registry) = world.get_resource::<MotionDatabases>() else {
        return Ok(json!({ "databases": [], "errors": [] }));
    };
    let users = |database: &Arc<MotionDatabase>, world: &World| -> Vec<u64> {
        let Some(mut query) = world.try_query::<(Entity, &MotionMatching)>() else {
            return Vec::new();
        };
        let mut out: Vec<u64> = query
            .iter(world)
            .filter(|(_, m)| Arc::ptr_eq(m.base_database.as_ref().unwrap_or(&m.database), database))
            .map(|(e, _)| e.to_bits())
            .collect();
        out.sort_unstable();
        out
    };
    let list: Vec<Value> = registry
        .iter()
        .map(|(path, d)| {
            json!({
                "asset": path,
                "id": d.id.map_or(Value::Null, |id| json!(id.to_string())),
                "frames": d.database.len(),
                "clips": d.database.clips().len(),
                "dimensions": d.database.dimensions(),
                "generation": d.generation,
                "playable": d.clips.is_some() && d.skeleton.is_some(),
                "content_hash": format!("{:016x}", d.database.content_hash()),
                "entities": users(&d.database, world),
            })
        })
        .collect();
    let errors: Vec<Value> = registry
        .errors()
        .map(|(path, e)| json!({ "asset": path, "code": e.code(), "message": e.to_string() }))
        .collect();
    Ok(json!({ "databases": list, "errors": errors }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
    use regia_animation::motion_matching::{FeatureSchema, MatcherSettings, MotionClip};
    use regia_animation::pose::translated;
    use regia_animation::skeleton::Skeleton;
    use regia_core::math::{Quat, Vec3};

    use super::*;

    /// A root and two feet; `speed` m/s straight ahead with a 1 s stride.
    fn clip(name: &str, speed: f32) -> AnimationClip {
        let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
        let mut clip = AnimationClip::empty(name, 1.0);
        clip.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times.iter().map(|&t| Vec3::new(0.0, 0.0, speed * t)).collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        for (joint, side) in [(1u32, 1.0f32), (2, -1.0)] {
            clip.tracks.push(JointTrack {
                joint,
                translations: TrackCurve::Animated {
                    times: times.clone(),
                    values: times
                        .iter()
                        .map(|&t| Vec3::new(-0.1 * side, 0.0, side * 0.3 * (std::f32::consts::TAU * t).sin()))
                        .collect(),
                    exact_rotation: false,
                },
                rotations: TrackCurve::Static(Quat::IDENTITY),
                scales: TrackCurve::Static(Vec3::ONE),
            });
        }
        clip
    }

    fn world_with_matcher() -> (World, Entity) {
        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "foot_l".into(), "foot_r".into()],
            vec![None, Some(0), Some(0)],
            vec![
                translated(Vec3::ZERO),
                translated(Vec3::new(-0.1, 0.0, 0.0)),
                translated(Vec3::new(0.1, 0.0, 0.0)),
            ],
        )
        .expect("skeleton");
        let walk = clip("walk", 1.5);
        let run = clip("run", 4.0);
        let database = MotionDatabase::build(
            &skeleton,
            &[
                MotionClip {
                    clip: &walk,
                    looping: true,
                },
                MotionClip {
                    clip: &run,
                    looping: true,
                },
            ],
            FeatureSchema::new(0, vec![1, 2]),
        )
        .expect("database");
        let mut world = World::new();
        let mut matching = MotionMatching::new(Arc::new(database), MatcherSettings::default());
        matching.trajectory = (1..=3)
            .map(|i| TrajectoryPoint::ahead(1.5 * i as f32 / 3.0))
            .collect();
        for _ in 0..10 {
            matching.step(1.0 / 30.0).expect("step");
        }
        let entity = world.spawn(matching).id();
        (world, entity)
    }

    #[test]
    fn reports_the_database_playback_and_a_probe() {
        let (mut world, entity) = world_with_matcher();
        let out = get(&mut world, &json!({ "entity": entity.to_bits() })).expect("state");
        assert_eq!(out["database"]["frames"], 60);
        assert_eq!(out["database"]["dimensions"], 27);
        assert_eq!(out["database"]["clips"][1]["name"], "run");
        assert_eq!(out["database"]["groups"]["foot_velocity"]["len"], 6);
        assert_eq!(
            out["settings"]["method"], "auto",
            "spec 52: Auto is the default method"
        );
        assert_eq!(out["playback"]["clip_name"], "walk");
        assert!(out["searches"].as_u64().unwrap_or(0) >= 2);
        assert_eq!(out["probe"], Value::Null);
        assert_eq!(out["last_error"], Value::Null);

        // A running probe picks the run; the matcher itself does not change.
        let probe = get(&mut world, &json!({ "entity": entity.to_bits(), "speed": 4.0 })).expect("probe");
        assert_eq!(probe["probe"]["best"]["clip_name"], "run");
        assert!(probe["probe"]["continuation_cost"].as_f64().is_some());
        assert_eq!(probe["playback"], out["playback"]);
        assert_eq!(probe["searches"], out["searches"]);
        let walking = get(
            &mut world,
            &json!({ "entity": entity.to_bits(), "trajectory": [
                { "position": [0.0, 0.5] },
                { "position": [0.0, 1.0], "direction": [0.0, 1.0] },
                { "position": [0.0, 1.5] },
            ] }),
        )
        .expect("probe");
        assert_eq!(walking["probe"]["best"]["clip_name"], "walk");
    }

    #[test]
    fn bad_params_and_missing_components_are_errors() {
        let (mut world, entity) = world_with_matcher();
        let code = |r: Result<Value, RpcError>| {
            r.err()
                .and_then(|e| e.data)
                .and_then(|d| d["code"].as_str().map(str::to_string))
        };
        let bits = entity.to_bits();
        assert_eq!(code(get(&mut world, &json!({}))).as_deref(), Some("F0002"));
        for params in [
            json!({ "entity": bits, "speed": "fast" }),
            json!({ "entity": bits, "speed": 1.0, "trajectory": [] }),
            json!({ "entity": bits, "trajectory": [{ "position": [0.0, 1.0] }] }),
            json!({ "entity": bits, "trajectory": "ahead" }),
            json!({ "entity": bits, "trajectory": [
                { "position": [0.0] }, { "position": [0.0, 1.0] }, { "position": [0.0, 1.5] }
            ] }),
            json!({ "entity": bits, "trajectory": [
                { "direction": [0.0, 1.0] }, { "position": [0.0, 1.0] }, { "position": [0.0, 1.5] }
            ] }),
        ] {
            assert_eq!(
                code(get(&mut world, &params)).as_deref(),
                Some("F0002"),
                "{params}"
            );
        }
        let bare = world.spawn_empty().id();
        assert_eq!(
            code(get(&mut world, &json!({ "entity": bare.to_bits() }))).as_deref(),
            Some("F0003")
        );
        world.despawn(bare);
        assert_eq!(
            code(get(&mut world, &json!({ "entity": bare.to_bits() }))).as_deref(),
            Some("F0003")
        );
    }

    /// Spec 52 § 5 / § 8.13: each probe reason code is named in the message, a weight that is not
    /// finite (1e300 is infinite as `f32`) or out of range is `bad_weight`, and
    /// `anim.motion_match_set` answers `F0003` without a `MotionMatching` component.
    #[test]
    fn probe_reasons_weight_range_and_set_without_a_matcher() {
        let (mut world, entity) = world_with_matcher();
        let bits = entity.to_bits();
        for (params, want) in [
            (json!({ "entity": bits, "speed": "fast" }), "bad_speed"),
            (
                json!({ "entity": bits, "speed": 1.0, "trajectory": [] }),
                "bad_probe",
            ),
            (
                json!({ "entity": bits, "trajectory": [{ "position": [0.0, 1.0] }] }),
                "bad_trajectory",
            ),
        ] {
            assert_eq!(reason(get(&mut world, &params)), want, "{params}");
        }
        let before = world.get::<MotionMatching>(entity).expect("c").matcher.settings;
        for weight in [1e300, 2000.0] {
            let params = json!({ "entity": bits, "weights": { "foot_position": weight } });
            assert_eq!(reason(set(&mut world, &params)), "bad_weight", "{params}");
        }
        assert_eq!(
            world.get::<MotionMatching>(entity).expect("c").matcher.settings,
            before,
            "a refused overlay changes nothing"
        );
        let code = |r: Result<Value, RpcError>| {
            r.err()
                .and_then(|e| e.data)
                .and_then(|d| d["code"].as_str().map(str::to_string))
        };
        let bare = world.spawn_empty().id();
        let params = json!({ "entity": bare.to_bits(), "search": "now" });
        assert_eq!(code(set(&mut world, &params)).as_deref(), Some("F0003"));
        world.despawn(bare);
        assert_eq!(code(set(&mut world, &params)).as_deref(), Some("F0003"));
    }

    /// The test world with the walk tagged `walk` and the run tagged `run`.
    fn world_with_tags() -> (World, Entity) {
        let (mut world, entity) = world_with_matcher();
        let mut matching = world.get_mut::<MotionMatching>(entity).expect("component");
        let mut database = (*matching.database).clone();
        database
            .annotate(
                TagTable::new(vec!["walk".into(), "run".into()]).expect("tags"),
                &[
                    regia_animation::motion_matching::ClipAnnotation {
                        tags: 0b01,
                        spans: vec![regia_animation::motion_matching::Span {
                            start: 0.0,
                            end: 0.2,
                            kind: regia_animation::motion_matching::SpanKind::PassThrough,
                        }],
                    },
                    regia_animation::motion_matching::ClipAnnotation {
                        tags: 0b10,
                        spans: Vec::new(),
                    },
                ],
            )
            .expect("annotate");
        matching.database = Arc::new(database);
        (world, entity)
    }

    /// ADR-7817, through the dispatcher: the summary says which optional cooked arrays a database
    /// carries. A source build stores the clip speed and the arc lengths of its distance-matched
    /// clips; a frozen v2 payload has no arc lengths (the player samples the clips) and its clip
    /// speeds come from the raw features at load.
    #[test]
    fn the_database_summary_names_the_cooked_arrays_through_the_dispatcher() {
        let built = regia_animation::motion_matching::procedural::procedural_locomotion()
            .build()
            .expect("procedural database");
        let stops: Vec<String> = built
            .clips()
            .iter()
            .filter(|c| c.distance_matched)
            .map(|c| c.name.clone())
            .collect();
        assert!(
            !stops.is_empty(),
            "the procedural set has a distance-matched stop"
        );
        let v2 = MotionDatabase::from_cooked_payload(include_bytes!(
            "../tests/fixtures/format-corpus/motiondb-cooked/v2.bin"
        ))
        .expect("the frozen v2 payload loads");
        let mut world = World::new();
        let current = world
            .spawn(MotionMatching::new(Arc::new(built), MatcherSettings::default()))
            .id();
        let old = world
            .spawn(MotionMatching::new(Arc::new(v2), MatcherSettings::default()))
            .id();
        let call = |world: &mut World, entity: Entity| {
            crate::dispatch::call_in_process(
                world,
                "anim.motion_match_database",
                json!({ "entity": entity.to_bits() }),
            )
            .expect("synchronous method")
            .expect("summary")
        };
        let out = call(&mut world, current);
        assert_eq!(out["cooked_arrays"]["clip_speed"], true);
        assert_eq!(out["cooked_arrays"]["arc_length_clips"], json!(stops));
        let flagged: Vec<String> = out["clips"]
            .as_array()
            .expect("clips")
            .iter()
            .filter(|c| c["distance_matched"] == true)
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(flagged, stops);
        let out = call(&mut world, old);
        assert_eq!(out["cooked_arrays"]["clip_speed"], true);
        assert_eq!(out["cooked_arrays"]["arc_length_clips"], json!([]));
    }

    fn reason(r: Result<Value, RpcError>) -> String {
        r.err()
            .map(|e| e.message)
            .unwrap_or_default()
            .split(']')
            .next()
            .unwrap_or_default()
            .trim_start_matches('[')
            .to_string()
    }

    #[test]
    fn candidates_explain_the_latest_search() {
        let (mut world, entity) = world_with_tags();
        let bits = entity.to_bits();
        let out = get(&mut world, &json!({ "entity": bits, "candidates": 4 })).expect("read");
        let rows = out["candidates"].as_array().expect("candidates");
        assert_eq!(rows.len(), 5, "four candidates and the continuation");
        assert!(rows.iter().any(|r| r["status"] == "continuing"));
        for row in rows {
            assert!(row["groups"]["trajectory_position"].as_f64().is_some());
            assert!(row["total"].as_f64().is_some());
        }
        assert!(out["last_search"]["reason"].as_str().is_some());
        assert!(out["last_search"]["method_used"].as_str().is_some());
        assert_eq!(out["tags"]["names"], json!(["walk", "run"]));
        // Old calls are unchanged: no `candidates` key without the parameter.
        let plain = get(&mut world, &json!({ "entity": bits })).expect("read");
        assert!(plain.get("candidates").is_none());
        // A filter on the read marks the other clip as filtered and steers the probe.
        let filtered = get(
            &mut world,
            &json!({ "entity": bits, "candidates": 32, "speed": 1.5, "filter": { "require": ["run"] } }),
        )
        .expect("read");
        assert_eq!(filtered["probe"]["best"]["clip_name"], "run");
        assert!(
            filtered["candidates"]
                .as_array()
                .expect("rows")
                .iter()
                .any(|r| r["status"] == "filtered")
        );
        for (params, code) in [
            (json!({ "entity": bits, "candidates": 0 }), "bad_candidates"),
            (json!({ "entity": bits, "candidates": 33 }), "bad_candidates"),
            (json!({ "entity": bits, "candidates": "all" }), "bad_candidates"),
            (
                json!({ "entity": bits, "filter": { "require": ["swim"] } }),
                "unknown_tag",
            ),
            (json!({ "entity": bits, "filter": "run" }), "unknown_tag"),
        ] {
            assert_eq!(reason(get(&mut world, &params)), code, "{params}");
        }
    }

    #[test]
    fn set_applies_settings_paths_filters_and_requests() {
        let (mut world, entity) = world_with_tags();
        let bits = entity.to_bits();
        let out = set(
            &mut world,
            &json!({
                "entity": bits,
                "settings": { "urgent_threshold": 0.5, "stagger": false, "play_rate": 1.5, "method": "auto" },
                "trajectory": [{ "position": [0.0, 1.0] }, { "position": [0.0, 2.0] }, { "position": [0.0, 3.0] }],
                "filter": { "require": ["run"] },
                "search": "now",
            }),
        )
        .expect("set");
        assert_eq!(out["settings"]["urgent_threshold"], 0.5);
        assert_eq!(out["settings"]["stagger"], false);
        assert_eq!(out["settings"]["method"], "auto");
        assert_eq!(out["filter"]["require"], json!(["run"]));
        for name in [
            "urgent_threshold",
            "stagger",
            "play_rate",
            "method",
            "trajectory",
            "filter",
            "search",
        ] {
            assert!(
                out["applied"].as_array().expect("applied").contains(&json!(name)),
                "{name}"
            );
        }
        let searches = world.get::<MotionMatching>(entity).expect("c").matcher.searches();
        {
            let mut matching = world.get_mut::<MotionMatching>(entity).expect("c");
            matching.step(0.0).expect("step");
        }
        let matching = world.get::<MotionMatching>(entity).expect("c");
        assert_eq!(
            matching.matcher.searches(),
            searches + 1,
            "search: now ran at the next update"
        );
        assert_eq!(matching.trajectory[2].position, Vec2::new(0.0, 3.0));
        // Reset keeps the counters and forgets what plays.
        let out = set(&mut world, &json!({ "entity": bits, "reset": true })).expect("reset");
        assert!(
            out["applied"]
                .as_array()
                .expect("applied")
                .contains(&json!("reset"))
        );
        assert!(
            world
                .get::<MotionMatching>(entity)
                .expect("c")
                .matcher
                .playback()
                .is_none()
        );
        // Every bad value is refused and nothing changes.
        let before = world.get::<MotionMatching>(entity).expect("c").matcher.settings;
        for (params, code) in [
            (
                json!({ "entity": bits, "settings": { "warp_speed": 1 } }),
                "bad_setting",
            ),
            (
                json!({ "entity": bits, "settings": { "stagger": 1 } }),
                "bad_setting",
            ),
            (json!({ "entity": bits, "settings": "fast" }), "bad_setting"),
            (json!({ "entity": bits, "search": "later" }), "bad_setting"),
            (
                json!({ "entity": bits, "weights": { "foot_position": -1.0 } }),
                "bad_weight",
            ),
            (
                json!({ "entity": bits, "weights": { "elbows": 1.0 } }),
                "bad_weight",
            ),
            (
                json!({ "entity": bits, "filter": { "forbid": ["fly"] } }),
                "unknown_tag",
            ),
            (
                json!({ "entity": bits, "trajectory": [{ "position": [0.0, 1.0] }] }),
                "bad_trajectory",
            ),
            (
                json!({ "entity": bits, "play_rate": 2.0, "method": "fastest" }),
                "motion_bad_method",
            ),
        ] {
            assert_eq!(reason(set(&mut world, &params)), code, "{params}");
            assert_eq!(
                world.get::<MotionMatching>(entity).expect("c").matcher.settings,
                before
            );
        }
    }

    #[test]
    fn a_weight_overlay_changes_costs_and_far_ones_scan() {
        let (mut world, entity) = world_with_tags();
        let bits = entity.to_bits();
        let hash = database(&mut world, &json!({ "entity": bits })).expect("db")["content_hash"].clone();
        let near = set(
            &mut world,
            &json!({ "entity": bits, "method": "kd_tree", "weights": { "foot_position": 1.2 } }),
        )
        .expect("set");
        assert_eq!(near["weights_overlay"], true);
        assert_eq!(
            near["method_used"], "kd_tree",
            "within +-50 percent the tree still runs"
        );
        let far = set(
            &mut world,
            &json!({ "entity": bits, "weights": { "foot_position": 3.0 } }),
        )
        .expect("set");
        assert_eq!(far["method_used"], "brute_force");
        let read = get(&mut world, &json!({ "entity": bits, "speed": 4.0 })).expect("read");
        assert_eq!(read["probe"]["method_used"], "brute_force");
        assert_eq!(read["probe"]["best"]["clip_name"], "run");
        // The database summary describes the database as built, not the overlay.
        let summary = database(&mut world, &json!({ "entity": bits })).expect("db");
        assert_eq!(summary["content_hash"], hash);
        let cleared = set(&mut world, &json!({ "entity": bits, "weights": null })).expect("set");
        assert_eq!(cleared["weights_overlay"], false);
    }

    #[test]
    fn the_database_summary_lists_tags_spans_and_hashes() {
        let (mut world, entity) = world_with_tags();
        let bits = entity.to_bits();
        let out = database(&mut world, &json!({ "entity": bits })).expect("db");
        assert_eq!(out["frames"], 60);
        assert_eq!(out["dimensions"], 27);
        assert_eq!(out["padded"], 32);
        assert_eq!(out["tags"]["walk"], 30);
        assert_eq!(out["tags"]["run"], 30);
        assert_eq!(out["spans"]["pass_through_frames"], 6);
        assert_eq!(out["targets"], 54);
        assert_eq!(out["normalization"], "group_deviation");
        assert_eq!(out["content_hash"].as_str().map(str::len), Some(16));
        let again = database(&mut world, &json!({ "database": bits })).expect("db");
        assert_eq!(again["content_hash"], out["content_hash"]);
        assert_eq!(
            reason(database(&mut world, &json!({ "database": "anim/hero.motiondb" }))),
            "unknown_database"
        );
        assert_eq!(reason(database(&mut world, &json!({}))), "bad_params");
        assert_eq!(
            reason(database(&mut world, &json!({ "entity": bits, "database": bits }))),
            "bad_params"
        );
        let bare = world.spawn_empty().id();
        let code = database(&mut world, &json!({ "entity": bare.to_bits() }))
            .err()
            .and_then(|e| e.data)
            .and_then(|d| d["code"].as_str().map(str::to_string));
        assert_eq!(code.as_deref(), Some("F0003"));
    }

    #[test]
    fn a_registered_database_answers_by_asset_path() {
        use regia_animation::motion_matching::LoadedMotionDatabase;
        use regia_animation::motion_matching::procedural::procedural_locomotion;
        let set = procedural_locomotion();
        let built = set.build().expect("database");
        let loaded = LoadedMotionDatabase::built(built, set.skeleton.clone(), set.entry_clips());
        let shared = loaded.database.clone();
        let mut world = World::new();
        let mut registry = MotionDatabases::default();
        registry.insert("anim/locomotion.motiondb.ron", loaded);
        world.insert_resource(registry);
        let user = world
            .spawn(MotionMatching::new(
                shared,
                regia_animation::motion_matching::MatcherSettings::default(),
            ))
            .id();

        let out =
            database(&mut world, &json!({ "database": "anim/locomotion.motiondb.ron" })).expect("by path");
        assert_eq!(out["asset"], "anim/locomotion.motiondb.ron");
        assert_eq!(out["entity"], Value::Null);
        assert_eq!(out["clips"].as_array().map(Vec::len), Some(6));
        assert_eq!(out["playable"], true);
        assert_eq!(out["generation"], 1);
        // `assets/` is optional, and the entity form names the same asset.
        let again = database(
            &mut world,
            &json!({ "database": "assets/anim/locomotion.motiondb.ron" }),
        )
        .expect("prefixed");
        assert_eq!(again["content_hash"], out["content_hash"]);
        let by_entity = database(&mut world, &json!({ "entity": user.to_bits() })).expect("by entity");
        assert_eq!(by_entity["asset"], "anim/locomotion.motiondb.ron");
        assert_eq!(by_entity["content_hash"], out["content_hash"]);
        assert_eq!(
            reason(database(&mut world, &json!({ "database": "nope.motiondb.ron" }))),
            "unknown_database"
        );
        assert_eq!(
            reason(database(&mut world, &json!({ "database": " " }))),
            "bad_params"
        );

        let list = databases(&mut world, &json!({})).expect("list");
        let rows = list["databases"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["asset"], "anim/locomotion.motiondb.ron");
        assert_eq!(rows[0]["entities"], json!([user.to_bits()]));
        assert_eq!(list["errors"], json!([]));
        let empty = databases(&mut World::new(), &json!({})).expect("no registry");
        assert_eq!(empty["databases"], json!([]));
    }

    /// Spec 52 § 3 (ADR-7216): `MotionMatching` is a reflected component, so the generic
    /// `scene.set_property` / `scene.get_entity` methods patch and read its tuning through the
    /// protocol; the shared database is not a reflected field.
    #[test]
    fn scene_methods_patch_and_read_the_reflected_matcher() {
        let (mut world, entity) = world_with_matcher();
        world.init_resource::<regia_app::Time>();
        let registry = AppTypeRegistry::default();
        registry.write().register::<MotionMatching>();
        world.insert_resource(registry);
        let mut call = |method, params| {
            crate::dispatch::call_in_process(&mut world, method, params)
                .expect("synchronous method")
                .expect("protocol call")
        };
        let set = call(
            "scene.set_property",
            json!({
                "entity": entity.to_bits(),
                "component": "MotionMatching",
                "path": "matcher.settings.play_rate",
                "value": 1.5,
            }),
        );
        assert_eq!(set["before"], 1.0);
        assert_eq!(set["after"], 1.5);
        let got = call(
            "scene.get_entity",
            json!({ "entity": entity.to_bits(), "components": ["MotionMatching"] }),
        );
        let value = &got["components"]["MotionMatching"]["value"];
        assert_eq!(value["matcher"]["settings"]["play_rate"], 1.5, "{got}");
        assert_eq!(value["matcher"]["settings"]["method"], "Auto", "{got}");
        assert_eq!(value["report_candidates"], 0, "{got}");
        assert!(
            value.get("database").is_none(),
            "the database is not reflected: {got}"
        );
    }
}
