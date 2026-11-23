// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the showcase camera and look controls (ADR-2449): `camera.handheld`,
//! `camera.photographic_walk`, `scene.season`, `render.foliage_shadow` (book pages
//! `camera-handheld.md`, `camera-photographic-walk.md`, `season.md`, `foliage-shadow.md`).
//!
//! Handlers are plain functions over `&mut World`. A world without `CameraPlugin` answers
//! `UNSUPPORTED` (`F0016`) to the camera methods; one without `SeasonPlugin` to `scene.season`.
//! Categorical mistakes are `F0002` with a lowercase `data.reason` listed on the book pages.

use regia_app::prelude::*;
use regia_framework::camera::handheld::{Handheld, HandheldProfile, MAX_INTENSITY};
use regia_framework::camera::photographic::{MAX_FOCUS_DISTANCE_M, PhotographicWalk};
use regia_framework::camera::sensor::ShakeField;
use regia_framework::season::{FoliageKind, SEASONS, SeasonLook, SeasonState, SeasonalFoliage};
use regia_render::{FrameRenderer, RuntimeRenderer};
use regia_scene::Transform;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most cameras a listing returns.
const MAX_LISTED: usize = 256;

fn unsupported(what: &str, hint: &str) -> RpcError {
    RpcError::new(protocol::UNSUPPORTED, "F0016", what, hint)
}

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !n.is_finite() {
                return Err(RpcError::invalid_params(format!("`{field}` must be finite")));
            }
            Ok(Some(n as f32))
        }
    }
}

fn non_negative(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match number(params, field)? {
        Some(n) if n < 0.0 => Err(RpcError::invalid_params(format!(
            "`{field}` must not be negative"
        ))),
        other => Ok(other),
    }
}

fn boolean(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{field}` must be true or false"
        ))),
    }
}

fn entity_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

/// The camera `entity` names: it must exist and have a `Transform`.
fn camera_entity(world: &World, entity: Entity, reason: &str) -> Result<(), RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "pick a camera entity from scene.query_entities",
        ));
    }
    if world.get::<Transform>(entity).is_none() {
        return Err(refused(
            reason,
            format!(
                "entity {} has no Transform, so it is not a camera",
                entity.to_bits()
            ),
            "pick a camera entity",
        ));
    }
    Ok(())
}

fn has_fields(params: &Value, fields: &[&str]) -> bool {
    fields
        .iter()
        .any(|f| params.get(*f).is_some_and(|v| !v.is_null()))
}

// ---------------------------------------------------------------------------
// camera.handheld
// ---------------------------------------------------------------------------

const HANDHELD_FIELDS: &[&str] = &[
    "preset",
    "intensity",
    "steady",
    "seed",
    "stride_m",
    "sway_position_m",
    "sway_rotation_deg",
    "bob_vertical_m",
];

fn handheld_json(entity: Entity, handheld: &Handheld) -> Value {
    let p = &handheld.profile;
    let pose = handheld.pose();
    json!({
        "entity": entity.to_bits(),
        "profile": {
            "intensity": p.intensity,
            "effective_intensity": p.effective_intensity(),
            "max_intensity": MAX_INTENSITY,
            "steady": p.steady,
            "seed": p.seed,
            "stride_m": p.stride_m,
            "sway_position_m": p.sway_position_m,
            "sway_rotation_deg": p.sway_rotation_deg,
            "bob_vertical_m": p.bob_vertical_m,
        },
        "time_s": handheld.time_s(),
        "step_phase": handheld.step_phase(),
        "footfalls": handheld.footfalls(),
        "pose": {
            "offset_m": [pose.offset.x, pose.offset.y, pose.offset.z],
            "pitch_deg": pose.pitch.to_degrees(),
            "yaw_deg": pose.yaw.to_degrees(),
            "roll_deg": pose.roll.to_degrees(),
        },
    })
}

/// `camera.handheld`: lists or edits first-person handheld cameras.
pub fn camera_handheld(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<ShakeField>().is_none() {
        return Err(unsupported(
            "no handheld cameras in this app",
            "add regia_framework::camera::CameraPlugin",
        ));
    }
    let entity = entity_param(params)?;
    let remove = boolean(params, "remove")?.unwrap_or(false);
    let editing = has_fields(params, HANDHELD_FIELDS);
    if remove && editing {
        return Err(refused(
            "handheld_remove_with_fields",
            "`remove` cannot be combined with profile fields",
            "remove first, then add again",
        ));
    }
    let Some(entity) = entity else {
        if remove || editing {
            return Err(RpcError::invalid_params(
                "`entity` is required to change a camera",
            ));
        }
        let mut cameras: Vec<(Entity, Value)> = world
            .query::<(Entity, &Handheld)>()
            .iter(world)
            .map(|(e, h)| (e, handheld_json(e, h)))
            .collect();
        cameras.sort_by_key(|(e, _)| e.to_bits());
        let count = cameras.len();
        cameras.truncate(MAX_LISTED);
        return Ok(json!({
            "count": count,
            "truncated": count > MAX_LISTED,
            "cameras": cameras.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        }));
    };
    camera_entity(world, entity, "handheld_not_a_camera")?;
    if remove {
        let mut camera = world.entity_mut(entity);
        let had = match camera.take::<Handheld>() {
            Some(mut handheld) => {
                // Take the pose off first, or the last frame's sway stays on the camera.
                if let Some(mut transform) = camera.get_mut::<Transform>() {
                    handheld.restore(&mut transform);
                }
                true
            }
            None => false,
        };
        return Ok(json!({ "entity": entity.to_bits(), "removed": had }));
    }
    if !editing {
        return match world.get::<Handheld>(entity) {
            Some(h) => Ok(handheld_json(entity, h)),
            None => Err(RpcError::not_found(
                format!("entity {} has no handheld profile", entity.to_bits()),
                "give it one with camera.handheld { entity, preset: \"walking\" }",
            )),
        };
    }
    let mut profile = world
        .get::<Handheld>(entity)
        .map_or_else(HandheldProfile::default, |h| h.profile);
    match crate::params::opt_str(params, "preset")? {
        None => {}
        Some("walking") => profile = HandheldProfile::walking(),
        Some("steady") => profile = HandheldProfile::steady_mount(),
        Some(other) => {
            return Err(refused(
                "handheld_unknown_preset",
                format!("unknown handheld preset {other:?}"),
                "walking or steady",
            ));
        }
    }
    if params
        .get("preset")
        .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return Err(RpcError::invalid_params("`preset` must be a string"));
    }
    if let Some(n) = non_negative(params, "intensity")? {
        profile.intensity = n;
    }
    if let Some(b) = boolean(params, "steady")? {
        profile.steady = b;
    }
    if let Some(seed) = params.get("seed").filter(|v| !v.is_null()) {
        profile.seed = seed
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`seed` must be a non-negative integer"))?;
    }
    if let Some(n) = non_negative(params, "stride_m")? {
        if n < 0.2 {
            return Err(RpcError::invalid_params("`stride_m` must be at least 0.2"));
        }
        profile.stride_m = n;
    }
    if let Some(n) = non_negative(params, "sway_position_m")? {
        profile.sway_position_m = n;
    }
    if let Some(n) = non_negative(params, "sway_rotation_deg")? {
        profile.sway_rotation_deg = n;
    }
    if let Some(n) = non_negative(params, "bob_vertical_m")? {
        profile.bob_vertical_m = n;
    }
    let before = world.get::<Handheld>(entity).map(|h| handheld_json(entity, h));
    match world.get_mut::<Handheld>(entity) {
        Some(mut h) => h.profile = profile,
        None => {
            world.entity_mut(entity).insert(Handheld::new(profile));
        }
    }
    let after = world.get::<Handheld>(entity).map(|h| handheld_json(entity, h));
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

// ---------------------------------------------------------------------------
// camera.photographic_walk
// ---------------------------------------------------------------------------

const WALK_FIELDS: &[&str] = &[
    "shutter_fraction",
    "blur_speed_min_m_s",
    "blur_speed_full_m_s",
    "blur_turn_min_deg_s",
    "blur_turn_full_deg_s",
    "focus_hold_s",
    "aperture_f_stops",
    "pull_focus",
];

fn walk_json(entity: Entity, walk: &PhotographicWalk) -> Value {
    let lens = walk.lens();
    json!({
        "entity": entity.to_bits(),
        "settings": {
            "shutter_fraction": walk.shutter_fraction,
            "motion_blur_samples": walk.motion_blur_samples,
            "blur_speed_min_m_s": walk.blur_speed_min_m_s,
            "blur_speed_full_m_s": walk.blur_speed_full_m_s,
            "blur_turn_min_deg_s": walk.blur_turn_min_deg_s,
            "blur_turn_full_deg_s": walk.blur_turn_full_deg_s,
            "focus_hold_s": walk.focus_hold_s,
            "aperture_f_stops": walk.aperture_f_stops,
        },
        "lens": { "shutter_fraction": lens.shutter_fraction, "focus_m": lens.focus_m },
        "focusing": walk.focusing(),
        "pull": walk.pull().map(|p| json!({
            "from_m": p.from_m, "to_m": p.to_m, "duration_s": p.duration_s, "elapsed_s": p.elapsed_s,
        })),
    })
}

/// `camera.photographic_walk`: lists or edits photographic-walk lens drivers, and starts focus pulls.
pub fn camera_photographic_walk(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<ShakeField>().is_none() {
        return Err(unsupported(
            "no photographic walk in this app",
            "add regia_framework::camera::CameraPlugin",
        ));
    }
    let entity = entity_param(params)?;
    let remove = boolean(params, "remove")?.unwrap_or(false);
    let enable = boolean(params, "enable")?.unwrap_or(false);
    let editing = has_fields(params, WALK_FIELDS);
    if remove && (editing || enable) {
        return Err(refused(
            "walk_remove_with_fields",
            "`remove` cannot be combined with other fields",
            "remove first, then add again",
        ));
    }
    let Some(entity) = entity else {
        if remove || editing || enable {
            return Err(RpcError::invalid_params(
                "`entity` is required to change a camera",
            ));
        }
        let mut cameras: Vec<(Entity, Value)> = world
            .query::<(Entity, &PhotographicWalk)>()
            .iter(world)
            .map(|(e, w)| (e, walk_json(e, w)))
            .collect();
        cameras.sort_by_key(|(e, _)| e.to_bits());
        let count = cameras.len();
        cameras.truncate(MAX_LISTED);
        return Ok(json!({
            "count": count,
            "truncated": count > MAX_LISTED,
            "cameras": cameras.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        }));
    };
    camera_entity(world, entity, "walk_not_a_camera")?;
    if remove {
        let had = world.entity_mut(entity).take::<PhotographicWalk>().is_some();
        return Ok(json!({ "entity": entity.to_bits(), "removed": had }));
    }
    if !editing && !enable {
        return match world.get::<PhotographicWalk>(entity) {
            Some(w) => Ok(walk_json(entity, w)),
            None => Err(RpcError::not_found(
                format!("entity {} has no photographic walk", entity.to_bits()),
                "add one with camera.photographic_walk { entity, enable: true }",
            )),
        };
    }
    let mut walk = world.get::<PhotographicWalk>(entity).cloned().unwrap_or_default();
    if let Some(n) = non_negative(params, "shutter_fraction")? {
        if n > 1.0 {
            return Err(RpcError::invalid_params("`shutter_fraction` must be at most 1"));
        }
        walk.shutter_fraction = n;
    }
    if let Some(n) = non_negative(params, "blur_speed_min_m_s")? {
        walk.blur_speed_min_m_s = n;
    }
    if let Some(n) = non_negative(params, "blur_speed_full_m_s")? {
        walk.blur_speed_full_m_s = n;
    }
    if let Some(n) = non_negative(params, "blur_turn_min_deg_s")? {
        walk.blur_turn_min_deg_s = n;
    }
    if let Some(n) = non_negative(params, "blur_turn_full_deg_s")? {
        walk.blur_turn_full_deg_s = n;
    }
    if walk.blur_speed_full_m_s < walk.blur_speed_min_m_s
        || walk.blur_turn_full_deg_s < walk.blur_turn_min_deg_s
    {
        return Err(refused(
            "walk_range_inverted",
            "a `full` blur threshold is below its `min`",
            "min is where blur starts, full where it reaches the shutter",
        ));
    }
    if let Some(n) = non_negative(params, "focus_hold_s")? {
        walk.focus_hold_s = n;
    }
    if let Some(n) = non_negative(params, "aperture_f_stops")? {
        if n < 0.7 {
            return Err(RpcError::invalid_params(
                "`aperture_f_stops` must be at least 0.7",
            ));
        }
        walk.aperture_f_stops = n;
    }
    if let Some(pull) = params.get("pull_focus").filter(|v| !v.is_null()) {
        let distance = number(pull, "distance_m")?.unwrap_or(0.0);
        let duration = number(pull, "duration_s")?.unwrap_or(1.0);
        if !walk.pull_focus(distance, duration) {
            return Err(refused(
                "walk_bad_focus_pull",
                "`pull_focus` needs a positive `distance_m` and a non-negative `duration_s`",
                &format!("distance_m is at most {MAX_FOCUS_DISTANCE_M}"),
            ));
        }
    }
    world.entity_mut(entity).insert(walk);
    Ok(world
        .get::<PhotographicWalk>(entity)
        .map(|w| walk_json(entity, w))
        .unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------
// scene.season
// ---------------------------------------------------------------------------

fn season_json(world: &mut World) -> Value {
    let state = *world.resource::<SeasonState>();
    let mut looks = serde_json::Map::new();
    for kind in [FoliageKind::Broadleaf, FoliageKind::Evergreen, FoliageKind::Grass] {
        let look: SeasonLook = state.look(kind);
        looks.insert(
            kind.name().into(),
            json!({
                "tint": look.tint, "density": look.density, "leaf_fall": look.leaf_fall,
                "held_at_summer": state.hold_summer[kind as usize],
            }),
        );
    }
    let seasonal = world.query::<&SeasonalFoliage>().iter(world).count();
    json!({
        "year_fraction": state.year_fraction(),
        "season": state.name(),
        "rate_per_s": state.rate_per_s,
        "seasons": SEASONS,
        "looks": looks,
        "seasonal_meshes": seasonal,
    })
}

/// `scene.season`: reads or sets the time of year.
pub fn scene_season(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<SeasonState>().is_none() {
        return Err(unsupported(
            "no season driver in this app",
            "add regia_framework::season::SeasonPlugin",
        ));
    }
    let named = match params.get("season").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(RpcError::invalid_params("`season` must be a string")),
    };
    let fraction = number(params, "year_fraction")?;
    if named.is_some() && fraction.is_some() {
        return Err(refused(
            "season_both_time_and_name",
            "give `season` or `year_fraction`, not both",
            "season jumps to an anchor, year_fraction to any point of the year",
        ));
    }
    let rate = number(params, "rate_per_s")?;
    let hold = match params.get("hold_summer").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::Array(kinds)) => {
            let mut flags = [false; 3];
            for kind in kinds {
                let name = kind
                    .as_str()
                    .ok_or_else(|| RpcError::invalid_params("`hold_summer` entries must be strings"))?;
                let kind = FoliageKind::from_name(name).ok_or_else(|| {
                    refused(
                        "season_unknown_kind",
                        format!("unknown foliage kind {name:?}"),
                        "broadleaf, evergreen or grass",
                    )
                })?;
                flags[kind as usize] = true;
            }
            Some(flags)
        }
        Some(_) => return Err(RpcError::invalid_params("`hold_summer` must be a list of kinds")),
    };
    let before = season_json(world);
    {
        let mut state = world.resource_mut::<SeasonState>();
        if let Some(name) = named
            && !state.set_named(name)
        {
            return Err(refused(
                "season_unknown_name",
                format!("unknown season {name:?}"),
                "spring, summer, autumn or winter",
            ));
        }
        if let Some(f) = fraction {
            state.set_year_fraction(f);
        }
        if let Some(r) = rate {
            state.rate_per_s = r;
        }
        if let Some(flags) = hold {
            state.hold_summer = flags;
        }
    }
    let after = season_json(world);
    Ok(json!({ "before": before, "after": after }))
}

// ---------------------------------------------------------------------------
// render.foliage_shadow
// ---------------------------------------------------------------------------

fn shadow_json(renderer: &FrameRenderer) -> Value {
    json!({
        "softness": renderer.foliage_shadow_softness(),
        "max_softness": FrameRenderer::MAX_FOLIAGE_SHADOW_SOFTNESS,
        "alpha_tested": renderer.alpha_tested_shadows(),
    })
}

/// `render.foliage_shadow`: reads or sets the soft leaf shadow band.
pub fn render_foliage_shadow(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let softness = non_negative(params, "softness")?;
    let alpha_tested = boolean(params, "alpha_tested")?;
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Err(RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
             FrameRenderer::set_foliage_shadow_softness)",
        ));
    };
    let before = shadow_json(&renderer.0);
    if let Some(on) = alpha_tested {
        renderer.0.set_alpha_tested_shadows(on);
    }
    if let Some(s) = softness {
        renderer.0.set_foliage_shadow_softness(s);
    }
    Ok(json!({ "before": before, "after": shadow_json(&renderer.0) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::camera::CameraPlugin;
    use regia_framework::season::SeasonPlugin;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, SeasonPlugin));
        app
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn handheld_adds_edits_lists_and_removes() {
        let mut app = app();
        let cam = app.world_mut().spawn(Transform::default()).id();
        let id = cam.to_bits();
        let out = camera_handheld(
            app.world_mut(),
            &json!({ "entity": id, "preset": "steady", "intensity": 0.5 }),
        )
        .expect("add");
        assert_eq!(out["after"]["profile"]["steady"], true);
        assert_eq!(out["after"]["profile"]["intensity"], 0.5);
        let listed = camera_handheld(app.world_mut(), &json!({})).expect("list");
        assert_eq!(listed["count"], 1);
        let one = camera_handheld(app.world_mut(), &json!({ "entity": id })).expect("one");
        assert_eq!(one["footfalls"], 0);
        let e = camera_handheld(app.world_mut(), &json!({ "entity": id, "preset": "drunk" }))
            .expect_err("refused");
        assert_eq!(reason(&e), "handheld_unknown_preset");
        let e = camera_handheld(
            app.world_mut(),
            &json!({ "entity": id, "remove": true, "steady": true }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e), "handheld_remove_with_fields");
        assert!(camera_handheld(app.world_mut(), &json!({ "entity": id, "intensity": -1 })).is_err());
        let removed =
            camera_handheld(app.world_mut(), &json!({ "entity": id, "remove": true })).expect("remove");
        assert_eq!(removed["removed"], true);
        let bare = app.world_mut().spawn_empty().id();
        let e = camera_handheld(
            app.world_mut(),
            &json!({ "entity": bare.to_bits(), "steady": true }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e), "handheld_not_a_camera");
    }

    #[test]
    fn cameras_methods_are_unsupported_without_the_plugin() {
        let mut world = World::new();
        let e = camera_handheld(&mut world, &json!({})).expect_err("unsupported");
        assert_eq!(e.code, protocol::UNSUPPORTED);
        assert!(camera_photographic_walk(&mut world, &json!({})).is_err());
        assert!(scene_season(&mut world, &json!({})).is_err());
        assert!(render_foliage_shadow(&mut world, &json!({})).is_err());
    }

    #[test]
    fn the_walk_is_added_edited_and_pulls_focus() {
        let mut app = app();
        let cam = app.world_mut().spawn(Transform::default()).id();
        let id = cam.to_bits();
        let out =
            camera_photographic_walk(app.world_mut(), &json!({ "entity": id, "enable": true })).expect("add");
        assert_eq!(out["settings"]["shutter_fraction"], 0.12_f32 as f64);
        let out = camera_photographic_walk(
            app.world_mut(),
            &json!({ "entity": id, "pull_focus": { "distance_m": 3.0, "duration_s": 0.5 } }),
        )
        .expect("pull");
        assert!(out["pull"].is_object());
        let e = camera_photographic_walk(
            app.world_mut(),
            &json!({ "entity": id, "pull_focus": { "distance_m": -1 } }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e), "walk_bad_focus_pull");
        let e = camera_photographic_walk(
            app.world_mut(),
            &json!({ "entity": id, "blur_speed_min_m_s": 9, "blur_speed_full_m_s": 4 }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e), "walk_range_inverted");
        let removed = camera_photographic_walk(app.world_mut(), &json!({ "entity": id, "remove": true }))
            .expect("remove");
        assert_eq!(removed["removed"], true);
    }

    #[test]
    fn the_season_is_read_and_set() {
        let mut app = app();
        let out = scene_season(app.world_mut(), &json!({})).expect("read");
        assert_eq!(out["after"]["season"], "summer");
        let out = scene_season(
            app.world_mut(),
            &json!({ "season": "winter", "hold_summer": ["evergreen"] }),
        )
        .expect("set");
        assert_eq!(out["after"]["season"], "winter");
        assert_eq!(out["after"]["looks"]["broadleaf"]["density"], 0.0);
        assert_eq!(out["after"]["looks"]["evergreen"]["held_at_summer"], true);
        let e = scene_season(
            app.world_mut(),
            &json!({ "season": "winter", "year_fraction": 0.1 }),
        )
        .expect_err("refused");
        assert_eq!(reason(&e), "season_both_time_and_name");
        let e = scene_season(app.world_mut(), &json!({ "season": "monsoon" })).expect_err("refused");
        assert_eq!(reason(&e), "season_unknown_name");
        let e = scene_season(app.world_mut(), &json!({ "hold_summer": ["cactus"] })).expect_err("refused");
        assert_eq!(reason(&e), "season_unknown_kind");
    }
}
