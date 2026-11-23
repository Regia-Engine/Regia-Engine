// SPDX-License-Identifier: MPL-2.0
//! `scene.spawn_prefab`, `level.open`, `level.unload`, `rules.get`, `timers.list`,
//! `tweens.list`: the runtime gameplay surface of the agent protocol — runtime prefab spawn,
//! level open/unload, rules, timers and tweens (`docs/specs/14-scene-and-framework.md`
//! § A3/A4/B3/B6, the M9 "all reachable through the agent protocol" acceptance;
//! `book/agents/runtime-gameplay.md`).
//!
//! The handlers are plain functions over `&mut World`; the tests drive them end to end through
//! the real dispatcher ([`crate::dispatch::process_requests`]) with the method name as the
//! JSON-RPC method, so permission checks, contract checks and the handler all run.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::format::SceneAssetRef;
use regia_scene::level::{self, LevelId, LevelLibrary, LevelManager, LevelState, LoadMode};
use regia_scene::prefab::{PendingPrefab, PrefabWorldExt};
use regia_scene::spawn::SpawnOptions;
use regia_scene::transform::Transform;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// `F0002` with `data.reason` (the structured-refusal shape of `book/agents/protocol.md`).
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// Parses a guid parameter, accepting the bare guid or the `asset:<guid>` spelling
/// ([`SceneAssetRef`]).
fn guid_param(params: &Value, key: &str, reason: &str) -> Result<Guid, RpcError> {
    let text = params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a guid or asset:<guid>) is required")))?;
    text.parse::<SceneAssetRef>()
        .map(|asset| asset.guid())
        .map_err(|_| {
            refused(
                reason,
                format!("`{key}` is not a guid: {text:?}"),
                "pass a guid like 00000000-0000-0000-0000-000000000001 (the asset: prefix is optional)",
            )
        })
}

/// A `[x, y, z]` (or `{x, y, z}`) vector from a transform object's field.
fn vec3_field(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<regia_core::math::Vec3>, RpcError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    let components: Option<[f64; 3]> = match value {
        Value::Array(items) if items.len() == 3 => {
            let get = |index: usize| items.get(index).and_then(Value::as_f64);
            match (get(0), get(1), get(2)) {
                (Some(x), Some(y), Some(z)) => Some([x, y, z]),
                _ => None,
            }
        }
        Value::Object(map) => {
            let g = |k: &str| map.get(k).and_then(Value::as_f64);
            match (g("x"), g("y"), g("z")) {
                (Some(x), Some(y), Some(z)) => Some([x, y, z]),
                _ => None,
            }
        }
        _ => None,
    };
    let components = components.ok_or_else(|| {
        RpcError::invalid_params(format!("`transform.{key}` must be [x, y, z] (or {{x, y, z}})"))
    })?;
    Ok(Some(regia_core::math::Vec3::new(
        components[0] as f32,
        components[1] as f32,
        components[2] as f32,
    )))
}

/// The optional `transform` parameter (`{translation?, rotation?, scale?}`); absent fields take
/// the [`Transform`] defaults (identity rotation, unit scale).
fn opt_transform(params: &Value) -> Result<Option<Transform>, RpcError> {
    let Some(object) = crate::params::opt_object(params, "transform")? else {
        return Ok(None);
    };
    let translation = vec3_field(object, "translation")?.unwrap_or(regia_core::math::Vec3::ZERO);
    let scale = vec3_field(object, "scale")?.unwrap_or(regia_core::math::Vec3::ONE);
    let rotation = match object.get("rotation") {
        None => regia_core::math::Quat::IDENTITY,
        Some(Value::Array(items)) if items.len() == 4 => {
            let get = |index: usize| items.get(index).and_then(Value::as_f64);
            let components: Option<[f64; 4]> = match (get(0), get(1), get(2), get(3)) {
                (Some(x), Some(y), Some(z), Some(w)) => Some([x, y, z, w]),
                _ => None,
            };
            let components = components.ok_or_else(|| {
                RpcError::invalid_params("`transform.rotation` must be [x, y, z, w] of numbers")
            })?;
            regia_core::math::Quat::from_xyzw(
                components[0] as f32,
                components[1] as f32,
                components[2] as f32,
                components[3] as f32,
            )
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`transform.rotation` must be [x, y, z, w] of numbers",
            ));
        }
    };
    Ok(Some(Transform {
        translation,
        rotation,
        scale,
    }))
}

/// `scene.spawn_prefab`: spawns a prefab instance at runtime (spec 14 § A3's `spawn_prefab`).
///
/// The instance root's guid is `root_guid` (a fresh random guid when omitted; a lockstep game
/// passes its sim RNG's next guid so peers derive identical prefab-internal guids). When the
/// template is not in the [`regia_scene::prefab::PrefabLibrary`] yet, the root carries
/// [`PendingPrefab`] and resolves on a later frame (`pending: true`). The instance is not part
/// of any level (no `LevelMember`).
pub fn spawn_prefab(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<regia_ecs::reflect::AppTypeRegistry>() {
        return Err(RpcError::not_found(
            "no AppTypeRegistry: the scene plugin is not installed",
            "add regia_scene::ScenePlugin to the app",
        ));
    }
    let prefab = guid_param(params, "prefab", "bad_guid")?;
    let root_guid = match crate::params::opt_str(params, "root_guid")? {
        Some(text) => text
            .parse::<SceneAssetRef>()
            .map(|asset| asset.guid())
            .map_err(|_| {
                refused(
                    "bad_guid",
                    format!("`root_guid` is not a guid: {text:?}"),
                    "pass a guid like 00000000-0000-0000-0000-000000000001 (the asset: prefix is optional)",
                )
            })?,
        None => Guid::new_random(),
    };
    let transform = opt_transform(params)?;
    let entity = world
        .spawn_prefab(prefab, transform, &SpawnOptions::default(), root_guid)
        .map_err(|err| {
            refused(
                "spawn_refused",
                format!("prefab {prefab}: {err}"),
                "nil or duplicate root guids, or a template that fails validation, are refused",
            )
        })?;
    let pending = world.get::<PendingPrefab>(entity).is_some();
    Ok(json!({
        "entity": entity.to_bits(),
        "prefab": SceneAssetRef(prefab).to_string(),
        "pending": pending,
    }))
}

/// No `LevelManager` in this app.
fn no_level_manager() -> RpcError {
    RpcError::not_found(
        "no LevelManager: the scene plugin is not installed",
        "add regia_scene::ScenePlugin to the app",
    )
}

/// `level.open`: opens a level at runtime through the world's [`LevelManager`]
/// (spec 14 § A4). `"replace"` (the default) unloads the current level first; `"additive"`
/// loads alongside it. Synchronous when the level's data is already in the
/// [`LevelLibrary`], otherwise the level is recorded `Loading` and completes when its data
/// arrives (watch for `LevelLoaded`).
pub fn level_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = guid_param(params, "level", "bad_guid")?;
    let mode = match crate::params::opt_str(params, "mode")? {
        None | Some("replace") => LoadMode::Replace,
        Some("additive") => LoadMode::Additive,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "unknown `mode` {other:?} (\"replace\" | \"additive\")"
            )));
        }
    };
    if !world.contains_resource::<LevelManager>() {
        return Err(no_level_manager());
    }
    let known = world
        .get_resource::<LevelLibrary>()
        .is_some_and(|library| library.get(LevelId(id)).is_some());
    let tracked = world
        .get_resource::<LevelManager>()
        .is_some_and(|manager| manager.levels().any(|(level, _)| level.0 == id));
    if !known && !tracked {
        return Err(refused(
            "level_not_found",
            format!("level {id} is not in the LevelLibrary and is not loading"),
            "open a level the app knows: its data in the LevelLibrary, or one already tracked by the LevelManager",
        ));
    }
    level::open_level(world, LevelId(id), mode).map_err(|err| {
        RpcError::new(
            protocol::INVALID_PARAMS,
            "F0002",
            format!("level {id}: {err}"),
            "the level file failed validation or its entities failed to spawn",
        )
    })?;
    // `open_level` puts the manager back even on the error path, so this reads the fresh state.
    let (current, state) = world
        .get_resource::<LevelManager>()
        .map(|manager| {
            let state = if manager
                .levels()
                .any(|(level, state)| level.0 == id && matches!(state, LevelState::Active))
            {
                "active"
            } else {
                "loading"
            };
            (
                manager
                    .current()
                    .map(|current| SceneAssetRef(current.0).to_string()),
                state,
            )
        })
        .ok_or_else(|| RpcError::internal("LevelManager missing after open_level"))?;
    Ok(json!({
        "level": SceneAssetRef(id).to_string(),
        "mode": match mode {
            LoadMode::Replace => "replace",
            LoadMode::Additive => "additive",
        },
        "current": current,
        "state": state,
    }))
}

/// `level.unload`: unloads an open level (spec 14 § A4). Fires `LevelUnloading` and despawns the
/// level's non-persistent entities; unloading the current level promotes the remaining active
/// level with the lowest id. A level that is not loaded answers `{ unloaded: false }`.
pub fn level_unload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = guid_param(params, "level", "bad_guid")?;
    if !world.contains_resource::<LevelManager>() {
        return Err(no_level_manager());
    }
    let unloaded = level::try_unload_level(world, LevelId(id)).is_ok();
    Ok(json!({
        "level": SceneAssetRef(id).to_string(),
        "unloaded": unloaded,
    }))
}

/// The live loading screen (spec 14 § A4, ADR-7263): its root entity and the level it shows.
fn loading_screen(world: &World) -> Option<(Entity, LevelId)> {
    let entity = world.get_resource::<LevelManager>()?.loading_screen_entity()?;
    let level = world.get::<regia_scene::level::LoadingScreen>(entity)?.level;
    Some((entity, level))
}

/// One level's status as the protocol shows it; `loading_screen` is the screen's entity when it
/// shows this level.
fn level_status_json(
    status: &regia_framework::level_progress::LevelStatus,
    screen: Option<(Entity, LevelId)>,
) -> Value {
    let screen = screen
        .filter(|(_, level)| *level == status.level)
        .map(|(entity, _)| entity.to_bits());
    json!({
        "level": SceneAssetRef(status.level.0).to_string(),
        "state": status.state,
        "progress": status.progress,
        "current": status.current,
        "failed": status.failed,
        "loading_screen": screen,
    })
}

/// `level.status`: a level's load state and progress `0..1` (spec 14 § A4, ADR-7203): `1` once it
/// is active; while it loads, the fraction of its `level:<guid>` load set. Without `level`, every
/// level the manager tracks. `loading_screen` names the loading-screen prefab's root entity while
/// one is up (ADR-7263), else `null`.
pub fn level_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<LevelManager>() {
        return Err(no_level_manager());
    }
    let screen = loading_screen(world);
    if params.get("level").is_some_and(|value| !value.is_null()) {
        let id = guid_param(params, "level", "bad_guid")?;
        let Some(status) = regia_framework::level_progress::level_status(world, LevelId(id)) else {
            return Err(refused(
                "level_not_found",
                format!("level {id} is not loading or loaded"),
                "level.open it first; level.status without `level` lists the tracked levels",
            ));
        };
        return Ok(level_status_json(&status, screen));
    }
    let current = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::current)
        .map(|current| SceneAssetRef(current.0).to_string());
    let levels: Vec<Value> = regia_framework::level_progress::level_statuses(world)
        .iter()
        .map(|status| level_status_json(status, screen))
        .collect();
    Ok(json!({
        "current": current,
        "levels": levels,
        "loading_screen": screen.map(|(entity, _)| entity.to_bits()),
    }))
}

/// `rules.get`: the active [`regia_framework::rules::GameRules`] — the concrete rules type's
/// path, whether the match is over (`is_match_over`), the rules name the current level
/// asked for (`LevelSettings.game_rules`, `None` when no level is open), and `match_session`:
/// whether [`regia_framework::rules::MatchSessionPlugin`] steps the hosted match session from ECS.
pub fn rules_get(world: &mut World) -> Result<Value, RpcError> {
    let Some(resource) = world.get_resource::<regia_framework::rules::GameRulesResource>() else {
        return Err(RpcError::not_found(
            "no GameRulesResource: the rules plugin is not installed",
            "add regia_framework::rules::RulesPlugin (or insert GameRulesResource) to the app",
        ));
    };
    let rules = resource.rules.type_path();
    let match_over = resource.rules.is_match_over(world);
    let level_rules = world
        .get_resource::<regia_scene::level::LevelSettings>()
        .and_then(|settings| settings.game_rules.clone());
    let match_session = world.contains_resource::<regia_framework::rules::MatchSessionAdapter>();
    Ok(json!({
        "rules": rules,
        "match_over": match_over,
        "level_rules": level_rules,
        "match_session": match_session,
    }))
}

/// `timers.list`: every scheduled timer in the [`regia_framework::timers::TimerHub`], in
/// scheduling order (spec 14 § B6). Without a hub the answer is an empty list, never an error
/// (scripts create the hub on demand).
pub fn timers_list(world: &mut World) -> Result<Value, RpcError> {
    let Some(hub) = world.get_resource::<regia_framework::timers::TimerHub>() else {
        return Ok(json!({ "timers": Value::Array(Vec::new()), "count": 0 }));
    };
    let timers: Vec<Value> = hub
        .infos()
        .iter()
        .map(|info| {
            let action = match &info.action {
                regia_framework::timers::TimerInfoAction::Call => json!({ "kind": "callback" }),
                regia_framework::timers::TimerInfoAction::Message { target, message_type } => json!({
                    "kind": "message",
                    "target": target.to_bits(),
                    "message_type": message_type,
                }),
            };
            json!({
                "remaining_secs": info.remaining_secs,
                "interval_secs": info.interval_secs,
                "owner": info.owner.map(|owner| owner.to_bits()),
                "action": action,
            })
        })
        .collect();
    let count = timers.len();
    Ok(json!({ "timers": timers, "count": count }))
}

/// `tweens.list`: every active [`regia_framework::tween::Tween`] component (spec 14 § B6):
/// the reflected `f32` leaf it animates, its values and its normalised progress.
pub fn tweens_list(world: &mut World) -> Result<Value, RpcError> {
    // Querying a component the world has never seen panics in bevy_ecs; no registration means
    // no tween was ever added, so the honest answer is an empty list.
    if world.component_id::<regia_framework::tween::Tween>().is_none() {
        return Ok(json!({ "tweens": Value::Array(Vec::new()), "count": 0 }));
    }
    let mut query = world.query::<(Entity, &regia_framework::tween::Tween)>();
    let tweens: Vec<Value> = query
        .iter(world)
        .map(|(entity, tween)| {
            json!({
                "entity": entity.to_bits(),
                "component": tween.component,
                "path": tween.path,
                "from": tween.from,
                "to": tween.to,
                "duration_secs": tween.duration_secs,
                "elapsed_secs": tween.elapsed_secs,
                "easing": format!("{:?}", tween.easing),
                "t": tween.t(tween.elapsed_secs),
            })
        })
        .collect();
    let count = tweens.len();
    Ok(json!({ "tweens": tweens, "count": count }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_app::prelude::*;
    use regia_core::id::Guid;
    use regia_framework::rules::RulesPlugin;
    use regia_framework::timers::TimerHub;
    use regia_framework::tween::Tween;
    use regia_scene::prelude::*;
    use serde_json::{Value, json};

    use crate::dispatch::{self, AgentInbox};
    use crate::playtest::AgentTimeControl;
    use crate::protocol::{Request, Response};
    use crate::safety::{AgentConfig, AgentPermissions, AuditLog};
    use crate::server::Inbound;

    /// An app with the agent plumbing the dispatcher needs (mirrors `coverage_calls::app`).
    fn agent_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentPermissions {
                config: AgentConfig {
                    rate_limit_per_second: 10_000,
                    heavy_rate_limit_per_second: 10_000,
                    ..AgentConfig::default()
                },
                source: None,
            })
            .init_resource::<AuditLog>()
            .init_resource::<crate::safety::AgentSession>()
            .init_resource::<AgentTimeControl>()
            .init_resource::<crate::dispatch::RateLimit>();
        app
    }

    /// Drives one master-token request through `process_requests`; fails on no answer.
    fn call(app: &mut App, method: &str, params: Value) -> Response {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: tx,
            })
            .expect("queued");
        app.world_mut().insert_resource(AgentInbox(inbox_rx));
        dispatch::process_requests(app.world_mut());
        drop(inbox_tx);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("the method answers")
    }

    fn ok(response: Response) -> Value {
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.expect("a result")
    }

    /// The `data.reason` of a refused call, when there is one.
    fn reason(response: &Response) -> Option<&str> {
        response
            .error
            .as_ref()
            .and_then(|error| error.data.as_ref())
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
    }

    /// Bakes a one-entity prefab template and registers it under `asset`.
    fn bake_prefab(app: &mut App, asset: Guid) {
        let file = SceneFile::from_ron_str(
            r#"(format_version: 1, entities: [
                (guid: "00000000-0000-0000-0000-0000000000e1", name: "Lamp", components: {
                    "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
                }),
            ])"#,
        )
        .expect("valid prefab RON");
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template = regia_scene::prefab::PrefabTemplate::from_scene_file(asset, &file, &registry.read())
            .expect("bake");
        app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    }

    /// spec 14 § A3: `scene.spawn_prefab` instantiates a registered prefab at runtime; an
    /// unregistered one comes back `pending` and resolves as a `PendingPrefab` root.
    #[test]
    fn spec14_a3_scene_spawn_prefab_spawns_a_registered_template() {
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let asset = Guid::from_u128(0xa1);
        bake_prefab(&mut app, asset);

        let out = ok(call(
            &mut app,
            "scene.spawn_prefab",
            json!({
                "prefab": format!("asset:{asset}"),
                "transform": { "translation": [3.0, 4.0, 5.0] },
            }),
        ));
        assert_eq!(out["prefab"], format!("asset:{asset}"));
        assert_eq!(out["pending"], false);
        let entity_bits = out["entity"].as_u64().expect("the instance root id");
        let entity = Entity::from_bits(entity_bits);
        assert!(
            app.world().get::<PrefabInstance>(entity).is_some(),
            "the root carries PrefabInstance"
        );
        let transform = app
            .world()
            .get::<Transform>(entity)
            .expect("the transform was applied");
        assert_eq!(transform.translation, regia_core::math::Vec3::new(3.0, 4.0, 5.0));

        // Not loaded yet: the root is spawned immediately, marked pending.
        let missing = Guid::from_u128(0xa2);
        let out = ok(call(
            &mut app,
            "scene.spawn_prefab",
            json!({ "prefab": format!("asset:{missing}") }),
        ));
        assert_eq!(out["pending"], true);
        let entity = Entity::from_bits(out["entity"].as_u64().expect("the root id"));
        assert!(app.world().get::<PendingPrefab>(entity).is_some());
    }

    /// spec 14 § A3: bad guids are refused before anything spawns.
    #[test]
    fn spec14_a3_scene_spawn_prefab_refuses_bad_guids() {
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let refused = call(&mut app, "scene.spawn_prefab", json!({ "prefab": "not-a-guid" }));
        assert!(refused.error.is_some());
        assert_eq!(reason(&refused), Some("bad_guid"));
        let refused = call(
            &mut app,
            "scene.spawn_prefab",
            json!({ "prefab": "asset:00000000-0000-0000-0000-000000000001", "root_guid": "nope" }),
        );
        assert_eq!(reason(&refused), Some("bad_guid"));
    }

    /// spec 14 § A4: `level.open` (replace/additive) and `level.unload` drive the
    /// `LevelManager`; unloading the current level leaves the world level-less.
    /// spec 14 § B5 (ADR-7203): `camera.blend_to` starts a blend on the shared director or a
    /// local player's; a cut lands on the target on the next frame.
    #[test]
    fn spec14_b5_camera_blend_to_drives_the_shared_and_player_directors() {
        use regia_framework::camera::blend::PlayerCamera;
        use regia_framework::camera::{CameraDirector, CameraPlugin, CameraRig};
        use regia_framework::player::{PlayerKind, PlayerManager, PlayerPlugin};
        let mut app = agent_app();
        app.add_plugins((CameraPlugin, PlayerPlugin));
        let rig = app
            .world_mut()
            .spawn((Transform::default(), CameraRig::default()))
            .id();
        let out = ok(call(
            &mut app,
            "camera.blend_to",
            json!({ "translation": [1.0, 2.0, 3.0], "duration": 0.0, "curve": "quad_in" }),
        ));
        assert_eq!(out["curve"], "QuadIn");
        assert!(app.world().resource::<CameraDirector>().is_blending());
        app.update();
        assert_eq!(
            app.world().get::<Transform>(rig).expect("rig").translation,
            regia_core::math::Vec3::new(1.0, 2.0, 3.0)
        );
        let got = ok(call(&mut app, "camera.get", json!({})));
        assert_eq!(got["blend"], 1.0);

        let mut manager = app
            .world_mut()
            .remove_resource::<PlayerManager>()
            .expect("manager");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("player 0");
        app.world_mut().insert_resource(manager);
        let cam = app
            .world_mut()
            .spawn((Transform::default(), PlayerCamera(0)))
            .id();
        ok(call(
            &mut app,
            "camera.blend_to",
            json!({ "target_entity": rig.to_bits(), "duration": 0.0, "player": 0 }),
        ));
        app.update();
        assert_eq!(
            app.world().get::<Transform>(cam).expect("cam").translation,
            regia_core::math::Vec3::new(1.0, 2.0, 3.0),
            "player 0's camera cut to the rig"
        );
        let missing = call(
            &mut app,
            "camera.blend_to",
            json!({ "duration": 1.0, "translation": [0.0, 0.0, 0.0], "player": 7 }),
        );
        assert!(missing.error.is_some(), "player 7 is not local");
        let bad = call(
            &mut app,
            "camera.blend_to",
            json!({ "duration": 1.0, "translation": [0.0, 0.0, 0.0], "curve": "wobbly" }),
        );
        assert!(bad.error.is_some());
    }

    /// spec 14 § B5 (ADR-7263): `camera.shake` with `player` routes a noise shake to that local
    /// player's director only, and `camera.get` reports each player's shakes.
    #[test]
    fn spec14_b5_camera_get_reports_per_player_shakes() {
        use regia_framework::camera::blend::PlayerCamera;
        use regia_framework::camera::{CameraPlugin, CameraRig};
        use regia_framework::player::{PlayerKind, PlayerManager, PlayerPlugin};
        let mut app = agent_app();
        app.add_plugins((CameraPlugin, PlayerPlugin));
        app.world_mut()
            .spawn((Transform::default(), CameraRig::default()));
        let mut manager = app
            .world_mut()
            .remove_resource::<PlayerManager>()
            .expect("manager");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p0");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p1");
        app.world_mut().insert_resource(manager);
        app.world_mut()
            .spawn((Transform::default(), CameraRig::default(), PlayerCamera(0)));
        app.world_mut()
            .spawn((Transform::default(), CameraRig::default(), PlayerCamera(1)));

        let out = ok(call(
            &mut app,
            "camera.shake",
            json!({ "preset": "impact", "player": 1 }),
        ));
        assert_eq!(out["player"], 1, "{out}");
        let got = ok(call(&mut app, "camera.get", json!({})));
        let players = got["players"].as_array().expect("players");
        assert_eq!(players.len(), 2, "{got}");
        assert_eq!(players[0]["player"], 0);
        assert_eq!(players[0]["shaking"], false, "{got}");
        assert_eq!(players[0]["shakes"], 0);
        assert_eq!(players[1]["player"], 1);
        assert_eq!(players[1]["shaking"], true, "{got}");
        assert_eq!(players[1]["shakes"], 1);
        assert!(players[1]["strength"].as_f64().is_some_and(|s| s > 0.0), "{got}");

        let missing = call(
            &mut app,
            "camera.shake",
            json!({ "preset": "impact", "player": 7 }),
        );
        assert_eq!(reason(&missing), Some("player_not_local"));
        let not_noise = call(
            &mut app,
            "camera.shake",
            json!({ "preset": "blast", "player": 0 }),
        );
        assert_eq!(reason(&not_noise), Some("shake_player_noise_only"));
    }

    /// spec 14 § A4 (ADR-7263): `level.status` names the loading-screen entity while a level loads.
    #[test]
    fn spec14_a4_level_status_names_the_loading_screen() {
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let screen_prefab = Guid::from_u128(0x005c_4ee7);
        let file = regia_scene::format::SceneFile::from_ron_str(
            r#"(format_version: 1, entities: [
                (guid: "8b6f2a11-0000-4000-8000-0000000010a1", name: "LoadingRoot", components: {}),
            ])"#,
        )
        .expect("prefab");
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template =
            regia_scene::prefab::PrefabTemplate::from_scene_file(screen_prefab, &file, &registry.read())
                .expect("bake");
        app.world_mut()
            .resource_mut::<regia_scene::prefab::PrefabLibrary>()
            .insert(template);
        app.world_mut()
            .resource_mut::<regia_scene::level::LevelManager>()
            .loading_screen = Some(screen_prefab);
        let level = LevelId(Guid::from_u128(0xc5));
        let name = format!("asset:{}", level.0);
        regia_scene::level::open_level(app.world_mut(), level, regia_scene::level::LoadMode::Replace)
            .expect("open");
        let screen = app
            .world()
            .resource::<regia_scene::level::LevelManager>()
            .loading_screen_entity()
            .expect("a loading screen");
        let out = ok(call(&mut app, "level.status", json!({ "level": name })));
        assert_eq!(out["loading_screen"], json!(screen.to_bits()), "{out}");
        let all = ok(call(&mut app, "level.status", json!({})));
        assert_eq!(all["loading_screen"], json!(screen.to_bits()), "{all}");

        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(level, Default::default()));
        app.update();
        let out = ok(call(&mut app, "level.status", json!({ "level": name })));
        assert_eq!(out["loading_screen"], Value::Null, "{out}");
        let all = ok(call(&mut app, "level.status", json!({})));
        assert_eq!(all["loading_screen"], Value::Null, "{all}");
    }

    /// spec 14 § A4 (ADR-7203): `level.status` reports a loading level's load-set progress and
    /// `1` once it is active.
    #[test]
    fn spec14_a4_level_status_reports_load_progress() {
        use regia_asset::{LoadKind, LoadSet, LoadSets};
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let sets = LoadSets::default();
        app.insert_resource(sets.clone());
        let level = LevelId(Guid::from_u128(0xc4));
        let name = format!("asset:{}", level.0);

        let missing = call(&mut app, "level.status", json!({ "level": name }));
        assert_eq!(reason(&missing), Some("level_not_found"));

        // Not in the library yet: the level waits in `Loading`.
        regia_scene::level::open_level(app.world_mut(), level, regia_scene::level::LoadMode::Replace)
            .expect("open");
        let mut set = LoadSet::new(regia_framework::level_progress::level_load_set_name(level));
        set.push(LoadKind::Mesh, "a", 0, 0);
        set.push(LoadKind::Mesh, "b", 0, 0);
        sets.define(set);
        sets.mark_resident(LoadKind::Mesh, "a");
        let out = ok(call(&mut app, "level.status", json!({ "level": name })));
        assert_eq!(out["state"], "loading");
        assert_eq!(out["progress"], 0.5);

        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(level, Default::default()));
        app.update();
        let all = ok(call(&mut app, "level.status", json!({})));
        assert_eq!(all["current"], name);
        assert_eq!(all["levels"][0]["state"], "active");
        assert_eq!(all["levels"][0]["progress"], 1.0);
    }

    #[test]
    fn spec14_a4_level_open_and_unload_drive_the_level_manager() {
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let level_a = LevelId(Guid::from_u128(0xb1));
        let level_b = LevelId(Guid::from_u128(0xb2));
        let text = |name: &str| {
            format!(
                r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [
                    (guid: "00000000-0000-0000-0000-0000000000c1", name: "{name}", components: {{}}),
                ])"#
            )
        };
        {
            let mut library = app.world_mut().resource_mut::<LevelLibrary>();
            library.insert(Level::from_ron_str(level_a, &text("yard")).expect("level a"));
            library.insert(Level::from_ron_str(level_b, &text("hall")).expect("level b"));
        }

        let out = ok(call(
            &mut app,
            "level.open",
            json!({ "level": format!("asset:{}", level_a.0) }),
        ));
        assert_eq!(out["mode"], "replace");
        assert_eq!(out["state"], "active");
        assert_eq!(out["current"], format!("asset:{}", level_a.0));
        let member_count = app.world_mut().query::<&LevelMember>().iter(app.world()).count();
        assert_eq!(member_count, 1, "the level's entity spawned");

        // Additive: both levels' entities coexist; the primary level stays.
        let out = ok(call(
            &mut app,
            "level.open",
            json!({ "level": format!("asset:{}", level_b.0), "mode": "additive" }),
        ));
        assert_eq!(out["state"], "active");
        assert_eq!(out["current"], format!("asset:{}", level_a.0));
        let member_count = app.world_mut().query::<&LevelMember>().iter(app.world()).count();
        assert_eq!(member_count, 2);

        let out = ok(call(
            &mut app,
            "level.unload",
            json!({ "level": format!("asset:{}", level_a.0) }),
        ));
        assert_eq!(out["unloaded"], true);
        let out = ok(call(
            &mut app,
            "level.unload",
            json!({ "level": format!("asset:{}", level_b.0) }),
        ));
        assert_eq!(out["unloaded"], true);
        // Not loaded: a graceful false, not an error.
        let out = ok(call(
            &mut app,
            "level.unload",
            json!({ "level": format!("asset:{}", level_b.0) }),
        ));
        assert_eq!(out["unloaded"], false);

        // Unknown level: refused with level_not_found.
        let refused = call(
            &mut app,
            "level.open",
            json!({ "level": format!("asset:{}", Guid::from_u128(0xb9)) }),
        );
        assert_eq!(reason(&refused), Some("level_not_found"));
    }

    /// spec 14 § B3: `rules.get` names the active `GameRules` and reports `is_match_over`.
    #[test]
    fn spec14_b3_rules_get_names_the_active_rules() {
        let mut app = agent_app();
        app.add_plugins(RulesPlugin);

        let out = ok(call(&mut app, "rules.get", json!({})));
        assert!(
            out["rules"]
                .as_str()
                .expect("the rules type path")
                .contains("DefaultRules"),
            "{}",
            out["rules"]
        );
        assert_eq!(out["match_over"], false);
        assert_eq!(out["match_session"], false);
    }

    /// GR-1.3: with `MatchSessionPlugin` and `MatchSessionRules` installed, `rules.get` names
    /// them, reports `match_session: true`, and reads the end of the match from the board.
    #[test]
    fn rules_get_reports_the_match_session_adapter() {
        use regia_framework::rules::objective::EndReason;
        use regia_framework::rules::{GameRulesResource, MatchRules, MatchSessionPlugin, MatchSessionRules};

        let mut app = agent_app();
        app.add_plugins((RulesPlugin, MatchSessionPlugin));
        app.insert_resource(GameRulesResource {
            rules: Box::new(MatchSessionRules),
        });
        let out = ok(call(&mut app, "rules.get", json!({})));
        assert!(
            out["rules"]
                .as_str()
                .is_some_and(|rules| rules.contains("MatchSessionRules"))
        );
        assert_eq!(out["match_session"], true);
        assert_eq!(out["match_over"], false);
        app.world_mut()
            .resource_mut::<MatchRules>()
            .session
            .report_condition(1, EndReason::Ended);
        let out = ok(call(&mut app, "rules.get", json!({})));
        assert_eq!(out["match_over"], true);
    }

    /// spec 14 § B6: `timers.list` reports every scheduled timer with its remaining time,
    /// repeat interval, owner and action kind.
    #[test]
    fn spec14_b6_timers_list_reports_every_scheduled_timer() {
        let mut app = agent_app();
        let owner = app.world_mut().spawn_empty().id();
        {
            let mut hub = TimerHub::default();
            hub.after(1.5, Some(owner), |_world| {});
            hub.every(0.25, None, |_world| {});
            hub.after_message(2.0, None, owner, String::from("ding"));
            app.world_mut().insert_resource(hub);
        }

        let out = ok(call(&mut app, "timers.list", json!({})));
        let timers = out["timers"].as_array().expect("a timer list");
        assert_eq!(timers.len(), 3);
        assert_eq!(out["count"], 3);
        assert_eq!(timers[0]["remaining_secs"], 1.5);
        assert_eq!(timers[0]["interval_secs"], Value::Null);
        assert_eq!(timers[0]["owner"], owner.to_bits());
        assert_eq!(timers[0]["action"], json!({ "kind": "callback" }));
        assert_eq!(timers[1]["action"], json!({ "kind": "callback" }));
        assert_eq!(timers[1]["interval_secs"], 0.25);
        assert_eq!(timers[1]["owner"], Value::Null);
        assert_eq!(
            timers[2]["action"],
            json!({ "kind": "message", "target": owner.to_bits(), "message_type": "alloc::string::String" })
        );

        // No hub: an empty list, never an error.
        app.world_mut().remove_resource::<TimerHub>();
        let out = ok(call(&mut app, "timers.list", json!({})));
        assert_eq!(out["count"], 0);
    }

    /// spec 14 § B6: `tweens.list` reports every active `Tween` with its target leaf, values
    /// and progress.
    #[test]
    fn spec14_b6_tweens_list_reports_every_active_tween() {
        let mut app = agent_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                Tween::translation_axis('y', 0.0, 10.0, 2.0, regia_core::math::Easing::Linear),
            ))
            .id();
        // Halfway: elapsed 1.0 of 2.0.
        app.world_mut()
            .get_mut::<Tween>(entity)
            .expect("the tween")
            .elapsed_secs = 1.0;

        let out = ok(call(&mut app, "tweens.list", json!({})));
        let tweens = out["tweens"].as_array().expect("a tween list");
        assert_eq!(tweens.len(), 1);
        assert_eq!(tweens[0]["entity"], entity.to_bits());
        assert_eq!(tweens[0]["component"], "Transform");
        assert_eq!(tweens[0]["path"], ".translation.y");
        assert_eq!(tweens[0]["from"], 0.0);
        assert_eq!(tweens[0]["to"], 10.0);
        assert_eq!(tweens[0]["duration_secs"], 2.0);
        assert_eq!(tweens[0]["elapsed_secs"], 1.0);
        assert_eq!(tweens[0]["easing"], "Linear");
        assert_eq!(tweens[0]["t"], 0.5);

        // No tween component registered: an empty list, never an error.
        let mut bare = agent_app();
        let out = ok(call(&mut bare, "tweens.list", json!({})));
        assert_eq!(out["count"], 0);
    }
}
