// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the gameplay framework (`docs/specs/14` M9/M20, `specs/25` § 3):
//! `scripts.*`, `camera.*`, `settings.*`, `sim.*`, `players.*`, `scene.possess`, `save.*` handlers.
//!
//! Every handler degrades gracefully when the framework plugin it reads is not installed
//! (`NOT_FOUND` with a hint naming the plugin), so the protocol surface stays uniform across
//! apps. Handlers are plain functions over `&mut World` so tests drive them without a server.

use regia_app::prelude::*;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{GetPath, PartialReflect};
use serde::de::DeserializeSeed;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Strips a leading dot from authoring-style paths (`.input.sensitivity`), the form
/// `GetPath::reflect_path` expects.
fn normalized_path(path: &str) -> &str {
    path.trim_start_matches('.')
}

/// `scripts.list`: every registered script type and its execution order.
pub fn scripts_list(world: &mut World) -> Result<Value, RpcError> {
    let Some(registry) = world.get_resource::<regia_framework::script::ScriptRegistry>() else {
        return Err(RpcError::not_found(
            "no ScriptRegistry in this app",
            "add regia_framework::script::ScriptPlugin and register script types with register_script",
        ));
    };
    let scripts: Vec<Value> = registry
        .iter()
        .map(|(type_path, desc)| json!({ "type": type_path, "order": desc.order }))
        .collect();
    Ok(json!({ "scripts": scripts, "count": scripts.len() }))
}

/// `scripts.get`: the script slots of one entity (type, order, started, enabled, running).
pub fn scripts_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let Some(scripts) = world.get::<regia_framework::script::Scripts>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} has no Scripts component", entity.to_bits()),
            "scripts attach through KindRegistry presentation scripts or by spawning Scripts",
        ));
    };
    let slots: Vec<Value> = scripts
        .0
        .iter()
        .map(|slot| {
            json!({
                "type": slot.type_path,
                "order": slot.order,
                "started": slot.started,
                "enabled": slot.enabled,
                "running": slot.instance.is_some(),
            })
        })
        .collect();
    Ok(json!({ "entity": entity.to_bits(), "slots": slots }))
}

/// `camera.get`: the lowest-entity-id rig's mode, angles, and target (or by `entity`).
pub fn camera_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let rig_entity = rig_entity(world, params)?;
    let (rig, target) = {
        let rig = world
            .get::<regia_framework::camera::CameraRig>(rig_entity)
            .ok_or_else(|| {
                RpcError::not_found(
                    format!("entity {} has no CameraRig", rig_entity.to_bits()),
                    "query entities with component CameraRig, or omit `entity` to read the lowest-entity-id rig",
                )
            })?;
        let target = world
            .get::<regia_framework::camera::ChaseCamera>(rig_entity)
            .map(|chase| chase.target.to_bits());
        (*rig, target)
    };
    let blend = world
        .get_resource::<regia_framework::camera::CameraDirector>()
        .and_then(regia_framework::camera::CameraDirector::blend_progress);
    // Each local player's director: its own shakes and blend (spec 14 § B5, ADR-7263).
    let players: Vec<Value> = world
        .get_resource::<regia_framework::player::PlayerManager>()
        .map(|manager| {
            manager
                .camera_directors()
                .map(|(index, director)| {
                    json!({
                        "player": index,
                        "shaking": director.is_shaking(),
                        "shakes": director.shake_count(),
                        "strength": director.strength(),
                        "blend": director.blend_progress(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "entity": rig_entity.to_bits(),
        "mode": rig.mode.name(),
        "yaw": rig.yaw,
        "pitch": rig.pitch,
        "follow": rig.follow.name(),
        "target": target,
        "blend": blend,
        "players": players,
    }))
}

/// Reads a `[x, y, z]` (or `[x, y, z, w]` with `len` 4) number array.
fn number_array(params: &Value, key: &str, len: usize) -> Result<Option<Vec<f32>>, RpcError> {
    let Some(value) = params.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let numbers: Option<Vec<f32>> = value
        .as_array()
        .filter(|items| items.len() == len)
        .and_then(|items| items.iter().map(|n| n.as_f64().map(|n| n as f32)).collect());
    match numbers {
        Some(numbers) if numbers.iter().all(|n| n.is_finite()) => Ok(Some(numbers)),
        _ => Err(RpcError::wrong_type(
            key,
            &format!("an array of {len} finite numbers"),
        )),
    }
}

/// `camera.blend_to`: blends the shared director's cameras (or local player `player`'s) to a
/// pose or an entity over `duration` seconds along `curve` (spec 14 § B5, ADR-7203).
pub fn camera_blend_to(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_core::math::{Easing, Quat, Vec3};
    use regia_framework::camera::blend::{BlendTarget, CameraPose};
    let duration = crate::params::opt_f64(params, "duration")?
        .filter(|d| d.is_finite() && *d >= 0.0)
        .ok_or_else(|| RpcError::invalid_params("`duration` (seconds, >= 0) is required"))?
        as f32;
    let curve_name = crate::params::opt_str(params, "curve")?.unwrap_or("linear");
    let wanted: String = curve_name.chars().filter(|c| *c != '_').collect();
    let curve = Easing::ALL
        .iter()
        .copied()
        .find(|easing| format!("{easing:?}").eq_ignore_ascii_case(&wanted))
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown `curve` {curve_name:?} (linear, quad_in, cubic_in_out, sine_out, ... as in Easing)"
            ))
        })?;
    let target = if let Some(bits) = crate::params::opt_u64(params, "target_entity")? {
        let entity = Entity::try_from_bits(bits)
            .filter(|entity| world.get_entity(*entity).is_ok())
            .ok_or_else(|| {
                RpcError::not_found(
                    format!("entity {bits} does not exist"),
                    "use scene.query_entities",
                )
            })?;
        BlendTarget::Entity(entity)
    } else {
        let translation = number_array(params, "translation", 3)?.ok_or_else(|| {
            RpcError::invalid_params(
                "give `translation` [x, y, z] (and optional `rotation`) or `target_entity`",
            )
        })?;
        let rotation = match number_array(params, "rotation", 4)? {
            Some(q) => {
                let quat = Quat::from_xyzw(q[0], q[1], q[2], q[3]);
                if quat.length_squared() < 1.0e-12 {
                    return Err(RpcError::invalid_params("`rotation` is a zero quaternion"));
                }
                quat.normalize()
            }
            None => Quat::IDENTITY,
        };
        BlendTarget::Pose(CameraPose {
            translation: Vec3::new(translation[0], translation[1], translation[2]),
            rotation,
        })
    };
    let player = crate::params::opt_u64(params, "player")?;
    if let Some(index) = player {
        let index = u8::try_from(index).map_err(|_| RpcError::invalid_params("`player` is 0..=255"))?;
        let mut manager = world
            .get_resource_mut::<regia_framework::player::PlayerManager>()
            .ok_or_else(|| {
                RpcError::not_found(
                    "no PlayerManager in this app",
                    "add regia_framework::player::PlayerPlugin",
                )
            })?;
        let director = manager.camera_director_mut(index).ok_or_else(|| {
            RpcError::not_found(
                format!("player {index} is not a local player"),
                "players.list shows the local players",
            )
        })?;
        director.blend_to(target, duration, curve);
    } else {
        let mut director = world
            .get_resource_mut::<regia_framework::camera::CameraDirector>()
            .ok_or_else(|| {
                RpcError::not_found(
                    "no CameraDirector in this app",
                    "add regia_framework::camera::CameraPlugin",
                )
            })?;
        director.blend_to(target, duration, curve);
    }
    Ok(json!({
        "player": player,
        "duration": duration,
        "curve": format!("{curve:?}"),
        "target_entity": match target {
            BlendTarget::Entity(entity) => Some(entity.to_bits()),
            BlendTarget::Pose(_) => None,
        },
    }))
}

/// `camera.set_mode`: switches a rig (lowest-id rig, or `entity`) to a named mode with default
/// parameters: `chase`, `overhead`, `first_person`, `free` (aliases accepted, see
/// [`regia_framework::camera::CameraMode::from_name`]). Optional `follow` (`smoothed` |
/// `simulated`) picks the pose of a predicted target the rig follows (spec 73 § 6.4).
pub fn camera_set_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode_name = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`mode` (chase | overhead | first_person | free) is required")
    })?;
    let mode = regia_framework::camera::CameraMode::from_name(mode_name).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "unknown mode {mode_name:?} (aliases: third_person, top_down, satellite, eye, spectator)"
        ))
    })?;
    let follow = match crate::params::opt_str(params, "follow")? {
        None => None,
        Some(name) => Some(
            regia_framework::camera::CameraFollow::from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!("unknown follow {name:?} (smoothed | simulated)"))
            })?,
        ),
    };
    let rig_entity = rig_entity(world, params)?;
    let mut rig = world
        .get_mut::<regia_framework::camera::CameraRig>(rig_entity)
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no CameraRig", rig_entity.to_bits()),
                "query entities with component CameraRig, or omit `entity` to drive the lowest-entity-id rig",
            )
        })?;
    let before = rig.mode.name().to_string();
    rig.switch_to(mode);
    if let Some(follow) = follow {
        rig.follow = follow;
    }
    Ok(json!({
        "entity": rig_entity.to_bits(),
        "before": before,
        "after": rig.mode.name(),
        "follow": rig.follow.name(),
    }))
}

/// Resolves the rig to act on: `params.entity` when given, else the lowest-id `CameraRig` entity.
fn rig_entity(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        return Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")));
    }
    let mut query = world.query_filtered::<Entity, With<regia_framework::camera::CameraRig>>();
    query.iter(world).min_by_key(|e| e.to_bits()).ok_or_else(|| {
        RpcError::not_found(
            "no CameraRig in this app",
            "add regia_framework::camera::CameraPlugin and a camera entity with a CameraRig",
        )
    })
}

/// The extension settings resource `path` names (spec 57 § 3.3.5): its first segment is an
/// extension id that declared settings and is not a [`GameSettings`] field (a `GameSettings`
/// field of the same name wins). Returns the id, the access and the rest of the path.
///
/// [`GameSettings`]: regia_framework::settings::GameSettings
fn extension_settings<'p>(
    world: &World,
    path: &'p str,
) -> Option<(&'p str, regia_framework::extension::SettingsAccess, &'p str)> {
    let path = normalized_path(path);
    if path.is_empty() {
        return None;
    }
    let (id, rest) = path.split_once('.').unwrap_or((path, ""));
    if world
        .get_resource::<regia_framework::settings::GameSettings>()
        .is_some_and(|settings| settings.reflect_path(id).is_ok())
    {
        return None;
    }
    let access = world
        .get_resource::<regia_framework::extension::ExtensionRegistry>()?
        .settings_access(id)?;
    Some((id, access, rest))
}

/// `settings.get` on an extension's settings resource.
fn extension_settings_get(
    world: &World,
    path: &str,
    id: &str,
    access: regia_framework::extension::SettingsAccess,
    rest: &str,
) -> Result<Value, RpcError> {
    let root = (access.get)(world).ok_or_else(|| {
        RpcError::not_found(
            format!("the settings resource of extension `{id}` is not in the world"),
            "plugin.info lists the extension's settings_type",
        )
    })?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let field = if rest.is_empty() {
        root.as_partial_reflect()
    } else {
        root.reflect_path(rest).map_err(|e| {
            RpcError::not_found(
                format!("no field `{rest}` in the settings of extension `{id}`: {e}"),
                format!("pass `{id}` to see the whole object"),
            )
        })?
    };
    let value = serde_json::to_value(TypedReflectSerializer::new(field, &registry))
        .map_err(|e| RpcError::internal(format!("settings serialization failed: {e}")))?;
    Ok(json!({ "path": path, "value": value, "provider": format!("extension:{id}") }))
}

/// `settings.get`: the whole [`GameSettings`](regia_framework::settings::GameSettings), or one
/// reflected field (`path`, e.g. `"input.sensitivity"`). An extension's declared settings are
/// reached as `<id>` or `<id>.<field>` (spec 57 § 3.3.5).
pub fn settings_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(path) = crate::params::opt_str(params, "path")?
        && let Some((id, access, rest)) = extension_settings(world, path)
    {
        return extension_settings_get(world, path, id, access, rest);
    }
    let Some(settings) = world.get_resource::<regia_framework::settings::GameSettings>() else {
        return Err(RpcError::not_found(
            "no GameSettings resource in this app",
            "add regia_framework::settings::SettingsPlugin",
        ));
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let path = crate::params::opt_str(params, "path")?.unwrap_or("");

    if path.is_empty() {
        let value = serde_json::to_value(TypedReflectSerializer::new(
            settings.as_partial_reflect(),
            &registry,
        ))
        .map_err(|e| RpcError::internal(format!("settings serialization failed: {e}")))?;
        return Ok(
            json!({ "settings": value, "runtime": runtime_settings(world), "applied": applied_json(world) }),
        );
    }
    let field = settings.reflect_path(normalized_path(path)).map_err(|e| {
        RpcError::not_found(
            format!("no field `{path}` in GameSettings: {e}"),
            "omit `path` to see the whole object",
        )
    })?;
    let value = serde_json::to_value(TypedReflectSerializer::new(field.as_partial_reflect(), &registry))
        .map_err(|e| RpcError::internal(format!("settings serialization failed: {e}")))?;
    // A display field also answers what the window and its swapchain took (ADR-7880).
    if normalized_path(path).starts_with("display") {
        return Ok(json!({ "path": path, "value": value, "applied": applied_json(world) }));
    }
    Ok(json!({ "path": path, "value": value }))
}

fn window_present_name(mode: regia_platform::PresentMode) -> &'static str {
    match mode {
        regia_platform::PresentMode::Vsync => "vsync",
        regia_platform::PresentMode::VsyncRelaxed => "vsync_relaxed",
        regia_platform::PresentMode::Immediate => "immediate",
    }
}

fn window_mode_name(mode: regia_platform::WindowMode) -> &'static str {
    match mode {
        regia_platform::WindowMode::Windowed => "windowed",
        regia_platform::WindowMode::BorderlessFullscreen => "borderless_fullscreen",
    }
}

/// `applied.display` of `settings.get` (ADR-7880): what the display settings did to the primary
/// window and its swapchain ([`regia_framework::settings::DisplayApplied`]); `null` fields while
/// there is no window or no surface.
fn applied_json(world: &World) -> Value {
    let Some(applied) = world.get_resource::<regia_framework::settings::DisplayApplied>() else {
        return json!({ "display": null });
    };
    let pair = |v: Option<(f64, f64)>| v.map(|(w, h)| json!([w, h]));
    json!({ "display": {
        "window": applied.window.map(Entity::to_bits),
        "logical_size": pair(applied.logical_size),
        "applied_logical_size": pair(applied.applied_logical_size),
        "physical_size": applied.physical_size.map(|(w, h)| json!([w, h])),
        "monitor_logical_size": pair(applied.monitor_logical_size),
        "mode": applied.mode.map(window_mode_name),
        "refused": applied.refused.map(|(code, (w, h))| json!({ "code": code, "requested_size": [w, h] })),
        "present_mode": applied.present_mode.map(window_present_name),
        "swapchain": applied.present.map(|p| json!({
            "planned": p.planned,
            "effective": p.effective,
            "vsync": p.waits_for_vblank,
            "offers_immediate": p.offers_immediate,
            "offers_mailbox": p.offers_mailbox,
        })),
        "vsync_note": applied.vsync_note,
    } })
}

/// Deserializes `value` into the type of `field` (the validation every `settings.set` runs) and
/// serializes the field's current value: `(new value, before)`. Reads only.
fn prepare_reflected(
    field: &dyn PartialReflect,
    path: &str,
    value: &Value,
    registry: &regia_ecs::reflect::TypeRegistry,
) -> Result<(Box<dyn PartialReflect>, Value), RpcError> {
    let field_type = field
        .get_represented_type_info()
        .map(|info| info.type_id())
        .and_then(|t| registry.get(t))
        .ok_or_else(|| RpcError::internal(format!("field `{path}` has no registered type")))?;
    let new_value = TypedReflectDeserializer::new(field_type, registry)
        .deserialize(value)
        .map_err(|e| RpcError::invalid_params(format!("value does not fit `{path}`: {e}")))?;
    let before = serde_json::to_value(TypedReflectSerializer::new(field, registry)).unwrap_or(Value::Null);
    Ok((new_value, before))
}

/// The `{ path, before, after }` answer of a dry run: `after` is the value that would be stored.
fn dry_run_answer(
    path: &str,
    before: Value,
    new_value: &dyn PartialReflect,
    registry: &regia_ecs::reflect::TypeRegistry,
) -> Value {
    let after = serde_json::to_value(TypedReflectSerializer::new(new_value, registry)).unwrap_or(Value::Null);
    json!({ "path": path, "before": before, "after": after })
}

/// Deserializes `value` into the type of `field`, then (unless `dry_run`) applies it, and returns
/// `{ path, before, after }`.
fn set_reflected(
    field: &mut dyn PartialReflect,
    path: &str,
    value: &Value,
    dry_run: bool,
    registry: &regia_ecs::reflect::TypeRegistry,
) -> Result<Value, RpcError> {
    let (new_value, before) = prepare_reflected(field.as_partial_reflect(), path, value, registry)?;
    if dry_run {
        return Ok(dry_run_answer(
            path,
            before,
            new_value.as_partial_reflect(),
            registry,
        ));
    }
    field
        .try_apply(new_value.as_partial_reflect())
        .map_err(|e| RpcError::invalid_params(format!("cannot apply value: {e}")))?;
    let after = serde_json::to_value(TypedReflectSerializer::new(field.as_partial_reflect(), registry))
        .unwrap_or(Value::Null);
    Ok(json!({ "path": path, "before": before, "after": after }))
}

/// `settings.set` on an extension's settings resource (not persisted; ADR-5783).
///
/// The path, the value and a dry run go through the read-only `SettingsAccess::get`; only a
/// real write calls `SettingsAccess::edit`, which re-inserts the resource and so marks it added
/// and changed. A `dry_run` or a refused write therefore leaves an extension system gated on
/// `resource_changed` / `resource_added` alone (rev-kimi-14 finding 3, ADR-5936).
fn extension_settings_set(
    world: &mut World,
    path: &str,
    (id, access, rest): (&str, regia_framework::extension::SettingsAccess, &str),
    value: &Value,
    dry_run: bool,
) -> Result<Value, RpcError> {
    let absent = || {
        RpcError::not_found(
            format!("the settings resource of extension `{id}` is not in the world"),
            "plugin.info lists the extension's settings_type",
        )
    };
    let no_field = |e: &dyn std::fmt::Display| {
        RpcError::not_found(
            format!("no field `{rest}` in the settings of extension `{id}`: {e}"),
            "see settings.get for field names",
        )
    };
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    {
        let root = (access.get)(world).ok_or_else(absent)?;
        let field = if rest.is_empty() {
            root.as_partial_reflect()
        } else {
            root.reflect_path(rest).map_err(|e| no_field(&e))?
        };
        let (new_value, before) = prepare_reflected(field, path, value, &registry)?;
        if dry_run {
            let mut answer = dry_run_answer(path, before, new_value.as_partial_reflect(), &registry);
            if let Some(object) = answer.as_object_mut() {
                object.insert("provider".into(), json!(format!("extension:{id}")));
            }
            return Ok(answer);
        }
        // Apply to a copy first, so a value that deserializes but cannot be applied is refused
        // before the resource is touched (a type without `reflect_clone` skips this probe).
        if let Ok(mut probe) = field.reflect_clone() {
            probe
                .try_apply(new_value.as_partial_reflect())
                .map_err(|e| RpcError::invalid_params(format!("cannot apply value: {e}")))?;
        }
    }
    let mut outcome = None;
    let present = (access.edit)(world, &mut |root: &mut dyn Reflect| {
        let field = if rest.is_empty() {
            Ok(root.as_partial_reflect_mut())
        } else {
            root.reflect_path_mut(rest).map_err(|e| no_field(&e))
        };
        outcome = Some(field.and_then(|field| set_reflected(field, path, value, false, &registry)));
    });
    if !present {
        return Err(absent());
    }
    let mut answer = outcome.unwrap_or_else(|| Err(RpcError::internal("settings edit did not run")))?;
    if let Some(object) = answer.as_object_mut() {
        object.insert("provider".into(), json!(format!("extension:{id}")));
    }
    Ok(answer)
}

/// The project settings of `Regia.toml`'s `[runtime]` table as the running app applies them
/// (`docs/specs/11` §§ 2-3, ADR-7070): the live `PauseOnFocusLoss` and `KeepScreenOn` values.
fn runtime_settings(world: &World) -> Value {
    json!({
        "pause_on_focus_loss": world
            .get_resource::<regia_platform::PauseOnFocusLoss>()
            .is_some_and(|p| p.0),
        "keep_screen_on": world.get_resource::<regia_platform::KeepScreenOn>().is_some_and(|k| k.0),
    })
}

/// `settings.set`: writes one reflected field of [`GameSettings`] and returns before/after. The
/// change is picked up live by the settings plugin (apply + persist) the same frame. `dry_run`
/// deserializes the new value and returns before/after without writing or applying it. An
/// extension's declared settings are written as `<id>.<field>` (spec 57 § 3.3.5); the framework
/// saves the change to the user settings file (`GameSettings::extensions`, ADR-7237).
pub fn settings_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = crate::params::opt_str(params, "path")?
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            RpcError::invalid_params(
                "`path` (a GameSettings field path, e.g. \"input.sensitivity\") is required",
            )
        })?;
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    if let Some(target) = extension_settings(world, path) {
        return extension_settings_set(world, path, target, value, dry_run);
    }

    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();

    let mut settings = world
        .get_resource_mut::<regia_framework::settings::GameSettings>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no GameSettings resource in this app",
                "add regia_framework::settings::SettingsPlugin",
            )
        })?;

    let field = settings.reflect_path_mut(normalized_path(path)).map_err(|e| {
        RpcError::not_found(
            format!("no field `{path}` in GameSettings: {e}"),
            "see settings.get for field names",
        )
    })?;
    // `dry_run` reports the value that would be stored and leaves the resource alone
    // (spec 23: a mutating tool with `dry_run: true` returns the diff without applying).
    set_reflected(field, path, value, dry_run, &registry)
}

/// `sim.counters`: the bridge instrumentation and the app clock, the inputs of the
/// frame-rate-independence check.
pub fn sim_counters(world: &mut World) -> Result<Value, RpcError> {
    let counters = world
        .get_resource::<regia_framework::sim_bridge::SimCounters>()
        .ok_or_else(|| {
            RpcError::not_found(
                "no SimCounters in this app",
                "add a regia_framework::sim_bridge::SimBridgePlugin",
            )
        })?;
    let counters = *counters;
    // The router's applied reactions (ADR-7210); all zero without a router.
    let r = world
        .get_resource::<regia_framework::sim_bridge::RoutedReactionStats>()
        .copied()
        .unwrap_or_default();
    let time = world.resource::<Time>();
    Ok(json!({
        "frames": counters.frames,
        "submit_input_calls": counters.submit_input_calls,
        "snapshot_reads": counters.snapshot_reads,
        "snapshot_ticks": counters.snapshot_ticks,
        "events_drained": counters.events_drained,
        "input_edges": counters.input_edges,
        "elapsed_secs": time.elapsed_secs(),
        "reactions": {
            "vfx": r.vfx,
            "vfx_unknown": r.vfx_unknown,
            "decals": r.decals,
            "decals_unknown": r.decals_unknown,
            "sounds": r.sounds,
            "sounds_refused": r.sounds_refused,
            "announcer_lines": r.announcer_lines,
            "announcer_rejected": r.announcer_rejected,
            "unplaced": r.unplaced,
            "no_subsystem": r.no_subsystem,
        },
    }))
}

/// `sim.check_framerate_independence`: DO27 rule A1 (`specs/25` § 3) — the sim-side tick rate
/// must match within `tolerance` (default 2%) across two measurement windows taken at different
/// frame rates. Each window: `{ ticks, wall_secs }` (from `sim.counters` snapshots).
pub fn sim_check_framerate_independence(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let parse_sample =
        |value: &Value, name: &str| -> Result<regia_framework::sim_bridge::FramerateSample, RpcError> {
            let ticks = value.get("ticks").and_then(Value::as_u64).ok_or_else(|| {
                RpcError::invalid_params(format!("sample {name} needs `ticks` (from sim.counters)"))
            })?;
            let wall_secs = value.get("wall_secs").and_then(Value::as_f64).ok_or_else(|| {
                RpcError::invalid_params(format!("sample {name} needs `wall_secs` (from sim.counters)"))
            })?;
            Ok(regia_framework::sim_bridge::FramerateSample { ticks, wall_secs })
        };
    let a = params
        .get("a")
        .ok_or_else(|| RpcError::invalid_params("`a` (first window: { ticks, wall_secs }) is required"))?;
    let b = params
        .get("b")
        .ok_or_else(|| RpcError::invalid_params("`b` (second window: { ticks, wall_secs }) is required"))?;
    let tolerance = crate::params::opt_f64(params, "tolerance")?.unwrap_or(0.02);
    let sample_a = parse_sample(a, "a")?;
    let sample_b = parse_sample(b, "b")?;

    let report = regia_framework::sim_bridge::check_framerate_independence(sample_a, sample_b, tolerance);
    // The ratio itself is pure. The app clock is the liveness check: a missing `Time` is
    // `NOT_FOUND`, the same as every other handler whose resource is absent. `SimBridgePlugin`
    // is not consulted here (`sim.counters` is the method that reads `SimCounters`).
    if world.get_resource::<Time>().is_none() {
        return Err(RpcError::not_found(
            "no Time in this app",
            "the app clock must exist",
        ));
    }
    Ok(json!({
        "a_rate": report.a_rate,
        "b_rate": report.b_rate,
        "ratio": report.ratio,
        "tolerance": report.tolerance,
        "pass": report.pass,
    }))
}

// ---------------------------------------------------------------------------
// The threaded sim host (ADR-8083)
// ---------------------------------------------------------------------------

fn sim_host_handle(world: &World) -> Result<regia_framework::sim_host::SimHostHandle, RpcError> {
    world
        .get_resource::<regia_framework::sim_host::SimHostHandle>()
        .cloned()
        .ok_or_else(|| {
            RpcError::not_found(
                "no SimHostHandle in this app",
                "start a regia_framework::sim_host::ThreadedSimHost and insert its handle() as a resource",
            )
        })
}

fn sim_host_status_json(status: regia_framework::sim_host::SimHostStatus) -> Value {
    json!({
        "running": status.running,
        "panicked": status.panicked,
        "held": status.held,
        "manual": status.manual,
        "tick_ns": status.tick_ns,
        "ticks": status.ticks,
        "publishes": status.publishes,
        "curr_tick": status.curr_tick,
        "granted": status.granted,
        "debt_drops": status.debt_drops,
        "ticks_dropped": status.ticks_dropped,
        "client_waits": status.client_waits,
        "events_dropped": status.events_dropped,
        "edges_dropped": status.edges_dropped,
        "lockstep": status.lockstep,
        "fp_env_supported": status.fp_env_supported,
        "fp_env_reset": status.fp_env_reset,
        "fp_checks": status.fp_checks,
        "fp_env_changed": status.fp_env_changed,
        "frames_queued": status.frames_queued,
        "frame_stalls": status.frame_stalls,
        "frames_refused": status.frames_refused,
        "catch_up_budget": status.catch_up_budget,
        "catch_up_ticks": status.catch_up_ticks,
        "snapshots": status.snapshots,
        "restores": status.restores,
        "last_step_ns": status.last_step_ns,
        "max_step_ns": status.max_step_ns,
    })
}

fn sim_host_error(err: regia_framework::sim_host::SimHostError) -> RpcError {
    use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, UNSUPPORTED};
    use regia_framework::sim_host::SimHostError;
    let message = err.to_string();
    match err {
        SimHostError::NotManual => RpcError::new(
            UNSUPPORTED,
            "sim_host_not_manual",
            message,
            "this host steps on the wall clock; start it with HostPacing::Manual to step it by grants",
        ),
        SimHostError::BadGrant(_) => RpcError::new(
            INVALID_PARAMS,
            "sim_host_bad_ticks",
            message,
            "pass ticks from 1 to 1000000",
        ),
        SimHostError::SimPanicked => RpcError::new(
            INTERNAL_ERROR,
            "sim_host_panicked",
            message,
            "the sim panicked on its thread; logs.read shows the panic, and the game must restart the host",
        ),
        SimHostError::NoSnapshots => RpcError::new(
            UNSUPPORTED,
            "sim_host_no_snapshots",
            message,
            "start the host with ThreadedSimHost::start_with_snapshots or start_lockstep (the sim implements DetState)",
        ),
        SimHostError::RestoreFailed(err) => RpcError::new(
            INVALID_PARAMS,
            "sim_host_restore_failed",
            format!("{message} (det-state code {})", err.code()),
            "the bytes were refused and the sim is unchanged; take a fresh snapshot with sim.host_snapshot",
        ),
        SimHostError::SnapshotFailed(err) => RpcError::new(
            INTERNAL_ERROR,
            "sim_host_snapshot_failed",
            format!("{message} (det-state code {})", err.code()),
            "the sim's DetState::save could not write its state; fix the game's save",
        ),
        SimHostError::SnapshotTickPassed { .. } => RpcError::new(
            INVALID_PARAMS,
            "sim_host_snapshot_tick_passed",
            message,
            "pass a tick at or after sim.host_status curr_tick, or omit tick for a snapshot now",
        ),
        SimHostError::Busy => RpcError::new(
            crate::protocol::TIMEOUT,
            "sim_host_busy",
            message,
            "the host thread is inside a long step; call again, or read sim.host_status for last_step_ns",
        ),
        // Stopped, and any start-up error a later handle could see.
        _ => RpcError::new(
            UNSUPPORTED,
            "sim_host_stopped",
            message,
            "the host thread has stopped; the game must start a new host",
        ),
    }
}

/// `sim.host_status`: the threaded sim host's counters (ADR-8083): running, held, pacing, ticks,
/// publishes and the newest published tick, manual grants pending, dropped clock debt, stale
/// presentation waits, and dropped events and input edges.
pub fn sim_host_status(world: &mut World) -> Result<Value, RpcError> {
    let handle = sim_host_handle(world)?;
    Ok(sim_host_status_json(handle.status()))
}

/// `sim.host_hold { held }`: hold (pause) or release the threaded sim host. A held host publishes
/// a still pair and drops the input it is offered. Answers `{ held, was_held, status }`.
pub fn sim_host_hold(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let held = crate::params::opt_bool(params, "held")?
        .ok_or_else(|| RpcError::invalid_params("`held` (true to hold, false to release) is required"))?;
    let handle = sim_host_handle(world)?;
    let status = handle.status();
    if status.panicked {
        return Err(sim_host_error(
            regia_framework::sim_host::SimHostError::SimPanicked,
        ));
    }
    if !status.running {
        return Err(sim_host_error(regia_framework::sim_host::SimHostError::Stopped));
    }
    let was_held = handle.set_held(held);
    Ok(json!({
        "held": held,
        "was_held": was_held,
        "status": sim_host_status_json(handle.status()),
    }))
}

/// `sim.host_step { ticks? }`: grant `ticks` (default 1, at most 1 000 000) more steps to a
/// manually paced sim host. Answers `{ granted, status }`; `granted` counts every grant not
/// stepped yet. The steps run on the host's thread; poll `sim.host_status` (or `wait_until`) for
/// `curr_tick`.
pub fn sim_host_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ticks = crate::params::opt_u64(params, "ticks")?.unwrap_or(1);
    let handle = sim_host_handle(world)?;
    let granted = handle.grant_ticks(ticks).map_err(sim_host_error)?;
    Ok(json!({
        "granted": granted,
        "status": sim_host_status_json(handle.status()),
    }))
}

/// Largest `count` `sim.tick_profile` accepts. Matches the profiler's default ring.
const TICK_PROFILE_MAX_COUNT: u64 = 3600;

fn tick_profile_bad_count() -> RpcError {
    use crate::protocol::INVALID_PARAMS;
    RpcError::new(
        INVALID_PARAMS,
        "bad_count",
        "sim.tick_profile: `count` must be an integer from 1 to 3600 [bad_count]",
        "omit count to return every over-budget tick the ring still holds",
    )
}

fn tick_profile_missing() -> RpcError {
    use crate::protocol::NOT_FOUND;
    RpcError::new(
        NOT_FOUND,
        "no_tick_profile",
        "sim.tick_profile: this app records no tick profile [no_tick_profile]",
        "insert DetTickProfile, or start a sim host whose sim publishes tick_profile()",
    )
}

fn tick_profile_count(params: &Value) -> Result<Option<usize>, RpcError> {
    let Some(value) = params.get("count").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value.as_u64() {
        Some(count) if (1..=TICK_PROFILE_MAX_COUNT).contains(&count) => Ok(usize::try_from(count).ok()),
        _ => Err(tick_profile_bad_count()),
    }
}

fn load_tick_profile(world: &World) -> Result<regia_framework::sim_host::ProfileReport, RpcError> {
    if let Some(handle) = world.get_resource::<regia_framework::sim_host::SimHostHandle>()
        && let Some(report) = handle.tick_profile()
    {
        return Ok(report);
    }
    if let Some(stored) = world.get_resource::<regia_framework::sim_host::DetTickProfile>() {
        return Ok(stored.report.clone());
    }
    Err(tick_profile_missing())
}

/// `sim.tick_profile { count? }`: per-system nearest-rank rows for the ticks the ring still
/// holds (ADR-8498). `count` keeps the newest over-budget ticks (1 to 3600). Omit it for every
/// retained over-budget tick. The host's published report wins over a `DetTickProfile` resource.
pub fn sim_tick_profile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let count = tick_profile_count(params)?;
    let mut report = load_tick_profile(world)?;
    if let Some(count) = count {
        let len = report.over_budget.len();
        if len > count {
            report.over_budget.drain(0..len - count);
        }
    }
    serde_json::to_value(&report)
        .map_err(|err| RpcError::internal(format!("sim.tick_profile: the report did not serialize ({err})")))
}

/// How long `sim.host_snapshot` and `sim.host_restore` wait for the host thread to answer.
const SIM_HOST_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The newest snapshot `sim.host_snapshot` took; `sim.host_restore` without bytes loads it.
#[derive(Resource, Debug, Clone)]
struct HostSnapshotSlot {
    bytes: Vec<u8>,
}

/// A `sim.host_snapshot { tick }` request for a tick the sim had not reached yet (KI-GP-005,
/// ADR-10062): the host thread answers right after that tick's step; the next call with the same
/// tick collects it.
#[derive(Resource)]
struct PendingHostSnapshot {
    tick: u64,
    reply: std::sync::Mutex<regia_framework::sim_host::HostReply<regia_framework::sim_host::HostSnapshot>>,
}

/// `sim.host_snapshot { tick?, include_bytes? }`: a framed snapshot of the threaded sim host's sim,
/// taken on the host thread between two steps (ADR-8526). Answers `{ tick, size, header_hash, tag,
/// state_version, kept }` and keeps the bytes for `sim.host_restore`; `include_bytes: true` adds
/// `bytes_hex` in debug builds only (`host_snapshot_bytes_disabled` in release builds).
///
/// With `tick` (ADR-10062) the snapshot is taken at that tick boundary: at once when the sim is
/// there, `sim_host_snapshot_tick_passed` when it is past it, and for a tick still ahead the call
/// answers `{ tick, pending: true, kept: false }` and keeps the request; call again with the same
/// tick to collect it (a call with another tick replaces the request).
pub fn sim_host_snapshot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    sim_host_snapshot_with(world, params, cfg!(debug_assertions))
}

/// [`sim_host_snapshot`] with the debug-build gate passed in (the release path is tested in debug
/// builds this way).
pub(crate) fn sim_host_snapshot_with(
    world: &mut World,
    params: &Value,
    debug_build: bool,
) -> Result<Value, RpcError> {
    let include_bytes = crate::params::opt_bool(params, "include_bytes")?.unwrap_or(false);
    if include_bytes && !debug_build {
        return Err(RpcError::new(
            crate::protocol::UNSUPPORTED,
            "host_snapshot_bytes_disabled",
            "sim.host_snapshot returns the bytes in debug builds only",
            "omit include_bytes; the size, tick and header hash identify the snapshot",
        ));
    }
    let tick = crate::params::opt_u64(params, "tick")?;
    let handle = sim_host_handle(world)?;
    let snapshot = match tick {
        None => handle
            .request_snapshot_now()
            .map_err(sim_host_error)?
            .wait_timeout(SIM_HOST_REPLY_TIMEOUT)
            .map_err(sim_host_error)?,
        Some(tick) => {
            let pending = world
                .remove_resource::<PendingHostSnapshot>()
                .filter(|p| p.tick == tick);
            let reply = match pending {
                Some(p) => p.reply,
                None => std::sync::Mutex::new(handle.request_snapshot(tick).map_err(sim_host_error)?),
            };
            let reached = tick <= handle.status().curr_tick;
            let answer = {
                let guard = reply.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if reached {
                    // At or behind the sim: the host thread answers between this step and the next.
                    Some(guard.wait_timeout(SIM_HOST_REPLY_TIMEOUT))
                } else {
                    guard.try_wait()
                }
            };
            match answer {
                Some(result) => result.map_err(sim_host_error)?,
                None => {
                    world.insert_resource(PendingHostSnapshot { tick, reply });
                    return Ok(json!({ "tick": tick, "pending": true, "kept": false }));
                }
            }
        }
    };
    let tag: String = String::from_utf8_lossy(&snapshot.header.tag)
        .trim_end_matches('\0')
        .to_string();
    let mut out = json!({
        "tick": snapshot.tick,
        "size": snapshot.bytes.len(),
        "header_hash": format!("{:016x}", snapshot.header.body_hash),
        "tag": tag,
        "state_version": snapshot.header.version,
        "kept": true,
    });
    if include_bytes {
        out["bytes_hex"] = Value::String(hex_encode(&snapshot.bytes));
    }
    world.insert_resource(HostSnapshotSlot {
        bytes: snapshot.bytes,
    });
    Ok(out)
}

/// `sim.host_restore { bytes_hex? }` (debug builds only; `host_restore_disabled` in release builds):
/// load a framed snapshot on the host thread, replace the sim and republish a still pair at the
/// restored tick (ADR-8526). Without `bytes_hex` it loads the snapshot the last `sim.host_snapshot`
/// kept (`host_snapshot_none` when there is none). Answers `{ tick, status }`.
pub fn sim_host_restore(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    sim_host_restore_with(world, params, cfg!(debug_assertions))
}

/// [`sim_host_restore`] with the debug-build gate passed in.
pub(crate) fn sim_host_restore_with(
    world: &mut World,
    params: &Value,
    debug_build: bool,
) -> Result<Value, RpcError> {
    if !debug_build {
        return Err(RpcError::new(
            crate::protocol::UNSUPPORTED,
            "host_restore_disabled",
            "sim.host_restore is a debug-build method",
            "use a debug build; a release build never lets an agent replace the running sim",
        ));
    }
    let bytes = match crate::params::opt_str(params, "bytes_hex")? {
        Some(text) => hex_decode(text)?,
        None => world
            .get_resource::<HostSnapshotSlot>()
            .map(|slot| slot.bytes.clone())
            .ok_or_else(|| {
                RpcError::new(
                    crate::protocol::NOT_FOUND,
                    "host_snapshot_none",
                    "no snapshot was kept and no bytes_hex was given",
                    "call sim.host_snapshot first, or pass bytes_hex",
                )
            })?,
    };
    let handle = sim_host_handle(world)?;
    let tick = handle
        .restore(bytes)
        .map_err(sim_host_error)?
        .wait_timeout(SIM_HOST_REPLY_TIMEOUT)
        .map_err(sim_host_error)?;
    Ok(json!({
        "tick": tick,
        "status": sim_host_status_json(handle.status()),
    }))
}

fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>, RpcError> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(RpcError::invalid_params(
            "`bytes_hex` must have an even number of hex digits",
        ));
    }
    let (pairs, _) = digits.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[hi, lo]| match (nibble(hi), nibble(lo)) {
            (Some(hi), Some(lo)) => Ok((hi << 4) | lo),
            _ => Err(RpcError::invalid_params(
                "`bytes_hex` holds a character that is not a hex digit",
            )),
        })
        .collect()
}

fn det_debug_missing() -> RpcError {
    use crate::protocol::NOT_FOUND;
    RpcError::new(
        NOT_FOUND,
        "no_det_debug",
        "sim.det_debug_draw: this app has no deterministic-index debug draw [no_det_debug]",
        "insert DetDebugDraw and call record() with the indexes, or start a sim host whose sim implements det_debug()",
    )
}

fn det_debug_bad_what(detail: &str) -> RpcError {
    use crate::protocol::INVALID_PARAMS;
    RpcError::new(
        INVALID_PARAMS,
        "bad_what",
        format!(
            "sim.det_debug_draw: {detail}; `what` takes a list of grid, occupancy, candidates, contacts, queries, flow, or \"all\" or \"none\" [bad_what]"
        ),
        "pass for example { \"what\": [\"grid\", \"contacts\"] }",
    )
}

fn det_debug_what(params: &Value) -> Result<Option<regia_framework::det_debug::DebugWhat>, RpcError> {
    use regia_framework::det_debug::DebugWhat;
    let Some(value) = params.get("what").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let named = |name: &str| {
        DebugWhat::from_name(name).ok_or_else(|| det_debug_bad_what(&format!("unknown part `{name}`")))
    };
    match value {
        Value::String(name) => named(name).map(Some),
        Value::Array(items) => {
            let mut what = DebugWhat::NONE;
            for item in items {
                let name = item
                    .as_str()
                    .ok_or_else(|| det_debug_bad_what("every entry must be a string"))?;
                what = what.union(named(name)?);
            }
            Ok(Some(what))
        }
        _ => Err(det_debug_bad_what("`what` must be a string or a list of strings")),
    }
}

/// `sim.det_debug_draw { what? }`: set (or read, without `what`) what the deterministic indexes
/// draw, and count the latest frame's primitives (ADR-8562). A sim host whose sim draws wins over
/// a game's [`regia_framework::det_debug::DetDebugDraw`] resource; setting `what` on a host also
/// stores it in that resource (inserting it) so the gizmo overlay follows.
pub fn sim_det_debug_draw(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::det_debug::{DebugKind, DebugRecorder, DetDebugDraw};
    use regia_framework::sim_host::SimHostHandle;
    let what = det_debug_what(params)?;
    let host = world
        .get_resource::<SimHostHandle>()
        .filter(|handle| handle.det_debug_supported())
        .cloned();
    if host.is_none() && world.get_resource::<DetDebugDraw>().is_none() {
        return Err(det_debug_missing());
    }
    if let Some(what) = what {
        if let Some(host) = &host {
            host.set_det_debug(what);
        }
        world.get_resource_or_insert_with(DetDebugDraw::default).what = what;
    }
    let (source, current, frame): (&str, _, Option<(u64, DebugRecorder)>) = match &host {
        Some(host) => ("host", host.det_debug(), host.det_debug_frame()),
        None => {
            let draw = world
                .get_resource::<DetDebugDraw>()
                .ok_or_else(det_debug_missing)?;
            (
                "resource",
                draw.what,
                draw.tick.map(|tick| (tick, draw.frame.clone())),
            )
        }
    };
    let (tick, recorder) = match frame {
        Some((tick, recorder)) => (Some(tick), recorder),
        None => (None, DebugRecorder::default()),
    };
    let counts = recorder.counts();
    let kinds: serde_json::Map<String, Value> = DebugKind::ALL
        .iter()
        .map(|kind| (kind.name().to_string(), json!(recorder.count_kind(*kind))))
        .collect();
    Ok(json!({
        "source": source,
        "what": current.names().collect::<Vec<_>>(),
        "tick": tick,
        "primitives": {
            "lines": counts.lines,
            "circles": counts.circles,
            "boxes": counts.boxes,
            "texts": counts.texts,
        },
        "kinds": kinds,
    }))
}

// ---------------------------------------------------------------------------
// The sim bridge's presentation settings (ADR-8456)
// ---------------------------------------------------------------------------

/// A delay in milliseconds, from whole nanoseconds (25 ms reads back as exactly 25.0).
fn delay_ms(delay: std::time::Duration) -> f64 {
    delay.as_nanos() as f64 / 1.0e6
}

fn sim_bridge_settings_json(world: &World) -> Value {
    use regia_framework::sim_blend::{BlendPolicy, SimBlendStats, SimBridgeSettings};
    let settings = world
        .get_resource::<SimBridgeSettings>()
        .copied()
        .unwrap_or_default();
    let stats = world.get_resource::<SimBlendStats>().copied().unwrap_or_default();
    let kinds: serde_json::Map<String, Value> = world
        .get_resource::<regia_framework::sim_bridge::KindRegistry>()
        .map(|registry| {
            registry
                .kinds
                .iter()
                .map(|(kind, entry)| (kind.clone(), json!(entry.blend.as_str())))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "handedness": settings.handedness.as_str(),
        "teleport_distance": settings.teleport_distance,
        "presentation_delay_ms": delay_ms(settings.presentation_delay),
        "default_blend": BlendPolicy::default().as_str(),
        "kinds": kinds,
        "stats": {
            "pair_reads": stats.pair_reads,
            "single_reads": stats.single_reads,
            "lerped": stats.lerped,
            "snapped": stats.snapped,
            "held": stats.held,
            "at_curr": stats.at_curr,
            "teleports": stats.teleports,
        },
    })
}

fn require_sim_bridge(world: &World) -> Result<(), RpcError> {
    if world
        .get_resource::<regia_framework::sim_bridge::SimCounters>()
        .is_none()
    {
        return Err(RpcError::not_found(
            "no sim bridge in this app",
            "add a regia_framework::sim_bridge::SimBridgePlugin",
        ));
    }
    Ok(())
}

/// `sim.bridge_settings`: the sim bridge's presentation settings (ADR-8456): the sim's
/// handedness, the teleport distance (metres, or `null`), the presentation delay, every kind's
/// blend policy and the blend counters (pair and single reads; lerped, snapped, held, at-`curr`
/// and teleported poses).
pub fn sim_bridge_settings(world: &mut World) -> Result<Value, RpcError> {
    require_sim_bridge(world)?;
    Ok(sim_bridge_settings_json(world))
}

/// `sim.set_bridge_settings { handedness?, teleport_distance?, presentation_delay_ms?, kind?,
/// blend?, dry_run? }`: change the sim bridge's presentation settings from the next frame
/// (ADR-8456): the sim's handedness, the teleport distance (metres; `null` turns it off), the
/// presentation delay, and one registered kind's blend policy (`kind` with `blend`). Answers
/// `{ before, after, applied }` in the `sim.bridge_settings` shape.
pub fn sim_set_bridge_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::protocol::{INVALID_PARAMS, NOT_FOUND};
    use regia_framework::sim_blend::{BlendPolicy, SimBridgeSettings};
    use regia_framework::sim_coords::SimHandedness;

    require_sim_bridge(world)?;
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let mut settings = world
        .get_resource::<SimBridgeSettings>()
        .copied()
        .unwrap_or_default();

    if let Some(name) = params.get("handedness") {
        let parsed = name.as_str().and_then(SimHandedness::parse).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "sim_bad_handedness",
                format!("`handedness` must be right_handed or left_handed_reflect_x, not {name}"),
                "pass right_handed or left_handed_reflect_x",
            )
        })?;
        settings.handedness = parsed;
    }
    if let Some(value) = params.get("teleport_distance") {
        settings.teleport_distance = match value {
            Value::Null => None,
            value => match value.as_f64() {
                Some(m) if m.is_finite() && m >= 0.0 => Some(m as f32),
                _ => {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        "sim_bad_teleport_distance",
                        format!(
                            "`teleport_distance` must be a non-negative number of metres or null, not {value}"
                        ),
                        "pass metres (for example 10.0), or null to never snap",
                    ));
                }
            },
        };
    }
    if let Some(value) = params.get("presentation_delay_ms") {
        let delay = value
            .as_f64()
            .filter(|ms| ms.is_finite() && (0.0..=10_000.0).contains(ms))
            .ok_or_else(|| {
                RpcError::new(
                    INVALID_PARAMS,
                    "sim_bad_delay",
                    format!("`presentation_delay_ms` must be 0 to 10000 milliseconds, not {value}"),
                    "pass milliseconds; one 60 Hz tick is 16.667",
                )
            })?;
        // Whole nanoseconds, so a round number of milliseconds reads back exactly.
        settings.presentation_delay = std::time::Duration::from_nanos((delay * 1.0e6).round() as u64);
    }
    let blend = match params.get("blend") {
        None => None,
        Some(name) => Some(name.as_str().and_then(BlendPolicy::parse).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "sim_bad_blend",
                format!("`blend` must be lerp, snap or hold, not {name}"),
                "pass lerp, snap or hold",
            )
        })?),
    };
    let kind = crate::params::opt_str(params, "kind")?;
    let kind_change = match (kind, blend) {
        (None, None) => None,
        (Some(kind), Some(blend)) => {
            let known = world
                .get_resource::<regia_framework::sim_bridge::KindRegistry>()
                .is_some_and(|registry| registry.entry(kind).is_some());
            if !known {
                return Err(RpcError::new(
                    NOT_FOUND,
                    "sim_kind_unknown",
                    format!("the sim bridge's KindRegistry has no kind `{kind}`"),
                    "sim.bridge_settings lists the registered kinds",
                ));
            }
            Some((kind.to_string(), blend))
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`kind` and `blend` go together: name the kind and its new policy",
            ));
        }
    };

    let before = sim_bridge_settings_json(world);
    let changes = settings
        != world
            .get_resource::<SimBridgeSettings>()
            .copied()
            .unwrap_or_default()
        || kind_change.is_some();
    if !dry_run {
        world.insert_resource(settings);
        if let Some((kind, blend)) = &kind_change
            && let Some(mut registry) = world.get_resource_mut::<regia_framework::sim_bridge::KindRegistry>()
            && let Some(entry) = registry.kinds.get_mut(kind)
        {
            entry.blend = *blend;
        }
    }
    let after = if dry_run {
        let mut after = before.clone();
        after["handedness"] = json!(settings.handedness.as_str());
        after["teleport_distance"] = json!(settings.teleport_distance);
        after["presentation_delay_ms"] = json!(delay_ms(settings.presentation_delay));
        if let Some((kind, blend)) = &kind_change {
            after["kinds"][kind] = json!(blend.as_str());
        }
        after
    } else {
        sim_bridge_settings_json(world)
    };
    Ok(json!({
        "before": before,
        "after": after,
        "applied": changes && !dry_run,
    }))
}

// ---------------------------------------------------------------------------
// M20: players, possess, save/load
// ---------------------------------------------------------------------------

/// `players.list`: every active player with index, kind, and controller entity.
pub fn players_list(world: &mut World) -> Result<Value, RpcError> {
    let Some(pm) = world.get_resource::<regia_framework::player::PlayerManager>() else {
        return Err(RpcError::not_found(
            "no PlayerManager in this app",
            "add regia_framework::player::PlayerPlugin",
        ));
    };
    let players: Vec<Value> = pm
        .iter()
        .filter_map(|(index, entity)| {
            let player = world.get::<regia_framework::player::Player>(entity)?;
            let controller_entity = player.controller;
            let possessed = world
                .get::<regia_framework::player::PlayerController>(controller_entity)
                .map(|pc| pc.possessed.to_bits());
            Some(json!({
                "index": index,
                "entity": entity.to_bits(),
                "kind": format!("{:?}", player.kind),
                "controller": controller_entity.to_bits(),
                "possessed": possessed,
                // The split-screen rect (`null` for a player without a local view) and the cameras
                // tagged for this player (ADR-7880).
                "viewport": pm.layout.viewports.get(&index).map(|&(x, y, w, h)| json!([x, y, w, h])),
                "cameras": split_screen_cameras(world, index),
            }))
        })
        .collect();
    Ok(json!({
        "players": players,
        "count": players.len(),
        "max_local": pm.max_local,
    }))
}

/// The cameras tagged [`regia_framework::player::SplitScreenCamera`] for `player`, with the viewport
/// each one has now (entity order).
fn split_screen_cameras(world: &World, player: u8) -> Vec<Value> {
    let mut cameras: Vec<(Entity, Option<[f32; 4]>)> = world
        .iter_entities()
        .filter(|e| {
            e.get::<regia_framework::player::SplitScreenCamera>()
                .is_some_and(|tag| tag.player == player)
        })
        .map(|e| (e.id(), e.get::<regia_render::Camera>().map(|c| c.viewport)))
        .collect();
    cameras.sort_by_key(|(entity, _)| *entity);
    cameras
        .into_iter()
        .map(|(entity, viewport)| json!({ "entity": entity.to_bits(), "viewport": viewport }))
        .collect()
}

/// `players.get`: details of one player by `index` or `entity`.
pub fn players_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?
    } else if let Some(index) = params.get("index").and_then(|v| v.as_u64()) {
        let index = u8::try_from(index).map_err(|_| {
            RpcError::invalid_params(format!("`index` {index} is not a player index (0..=255)"))
        })?;
        let pm = world
            .get_resource::<regia_framework::player::PlayerManager>()
            .ok_or_else(|| {
                RpcError::not_found(
                    "no PlayerManager in this app",
                    "add regia_framework::player::PlayerPlugin",
                )
            })?;
        pm.player_entity(index).ok_or_else(|| {
            RpcError::not_found(
                format!("no player with index {index}"),
                "use players.list to see active indices",
            )
        })?
    } else {
        return Err(RpcError::invalid_params("`entity` or `index` is required"));
    };

    let Some(player) = world.get::<regia_framework::player::Player>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} is not a player", entity.to_bits()),
            "query entities with component Player",
        ));
    };
    let controller = player.controller;
    let pc = world.get::<regia_framework::player::PlayerController>(controller);
    Ok(json!({
        "entity": entity.to_bits(),
        "index": player.index,
        "kind": format!("{:?}", player.kind),
        "controller": controller.to_bits(),
        "possessed": pc.map(|p| p.possessed.to_bits()),
        "view_target": pc.map(|p| p.view_target.to_bits()),
        "context": pc.map(|p| p.active_context()),
    }))
}

/// `scene.possess`: makes a [`PlayerController`] possess an entity.
///
/// Parameters: `controller` (entity id of the `PlayerController`), `entity` (entity id of the
/// character to possess).
pub fn scene_possess(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let controller_bits = params
        .get("controller")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`controller` (PlayerController entity id) is required"))?;
    let controller_entity = Entity::try_from_bits(controller_bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{controller_bits} is not an entity id")))?;

    let target_bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (the character to possess) is required"))?;
    let target = Entity::try_from_bits(target_bits)
        .ok_or_else(|| RpcError::invalid_params(format!("{target_bits} is not an entity id")))?;

    let mut pc = world
        .get_mut::<regia_framework::player::PlayerController>(controller_entity)
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no PlayerController", controller_entity.to_bits()),
                "query entities with component PlayerController",
            )
        })?;
    let before = pc.possessed;
    pc.possess(target);
    Ok(json!({
        "controller": controller_entity.to_bits(),
        "before": before.to_bits(),
        "after": target.to_bits(),
    }))
}

/// `save.save`: serialises the world to a named slot.
///
/// Parameters: `slot` (slot name, e.g. "Auto", "Slot1").
pub fn save_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = world
        .get_resource::<regia_framework::save_game::SaveDirectory>()
        .cloned()
        .ok_or_else(|| {
            RpcError::not_found(
                "no SaveDirectory in this app",
                "add regia_framework::save_game::SavePlugin",
            )
        })?;
    let slot_name = crate::params::opt_str(params, "slot")?.unwrap_or("Auto");
    // A game that installed a save store (Steam Remote Storage, spec 91) saves there.
    if world.contains_resource::<regia_framework::save_flow::SaveStore>() {
        let result = regia_framework::save_flow::save_to_store(world, slot_name)
            .map_err(|e| RpcError::internal(format!("save failed: {e}")))?;
        return Ok(json!({
            "slot": regia_framework::save_flow::sanitize_slot_name(slot_name),
            "entity_count": result.entity_count,
            "resource_count": result.resource_count,
            "bytes_written": result.bytes_written,
        }));
    }
    let slot = regia_framework::save_game::SaveGame::slot(&dir, slot_name);
    let spec = regia_framework::save_game::SaveSpec::default();
    let result = regia_framework::save_game::SaveGame::save(&slot, world, &spec)
        .map_err(|e| RpcError::internal(format!("save failed: {e}")))?;
    Ok(json!({
        "slot": slot.name,
        "entity_count": result.entity_count,
        "resource_count": result.resource_count,
        "bytes_written": result.bytes_written,
    }))
}

/// `save.load`: deserialises a named slot into the world.
///
/// Parameters: `slot` (slot name).
pub fn save_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = world
        .get_resource::<regia_framework::save_game::SaveDirectory>()
        .cloned()
        .ok_or_else(|| {
            RpcError::not_found(
                "no SaveDirectory in this app",
                "add regia_framework::save_game::SavePlugin",
            )
        })?;
    let slot_name = params
        .get("slot")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`slot` (save slot name) is required"))?;
    if world.contains_resource::<regia_framework::save_flow::SaveStore>() {
        return save_load_from_store(world, slot_name);
    }
    let slot = regia_framework::save_game::SaveGame::slot(&dir, slot_name);
    // A slot with a damaged current file still loads its previous copy (ADR-3102).
    if slot.header.is_none() && !slot.path.is_file() && !slot.has_previous() {
        return Err(RpcError::not_found(
            format!("save slot {slot_name:?} does not exist"),
            "use save.list_slots to see available slots",
        ));
    }
    let spec = regia_framework::save_game::SaveSpec::default();
    let result = regia_framework::save_game::SaveGame::load(&slot, world, &spec)
        .map_err(|e| RpcError::internal(format!("load failed: {e}")))?;
    Ok(json!({
        "slot": slot.name,
        "entity_count": result.entity_count,
        "resource_count": result.resource_count,
        "bytes_written": result.bytes_written,
        "copy": result.copy.as_str(),
    }))
}

/// `save.load` through an installed save store (spec 91).
fn save_load_from_store(world: &mut World, slot_name: &str) -> Result<Value, RpcError> {
    use regia_framework::save_flow;
    let slot = save_flow::sanitize_slot_name(slot_name);
    let storage = save_flow::active_storage(world);
    let bytes = storage
        .read(&slot)
        .map_err(|e| RpcError::internal(format!("load failed: {e}")))?;
    // A slot with a damaged current copy still loads its previous copy (ADR-3102).
    if bytes.as_deref().and_then(save_flow::header_from_bytes).is_none() && !storage.has_previous(&slot) {
        return Err(RpcError::not_found(
            format!("save slot {slot_name:?} does not exist"),
            "use save.list_slots to see available slots",
        ));
    }
    let result = save_flow::load_from_store(world, slot_name)
        .map_err(|e| RpcError::internal(format!("load failed: {e}")))?;
    Ok(json!({
        "slot": slot,
        "entity_count": result.entity_count,
        "resource_count": result.resource_count,
        "bytes_written": result.bytes_written,
        "copy": result.copy.as_str(),
    }))
}

/// `save.list_slots`: every save slot with its header metadata.
pub fn save_list_slots(world: &mut World) -> Result<Value, RpcError> {
    if world.contains_resource::<regia_framework::save_flow::SaveStore>() {
        let slots = regia_framework::save_flow::list_store(world)
            .map_err(|e| RpcError::internal(format!("cannot list save slots: {e}")))?;
        let list: Vec<Value> = slots
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "game_version": s.header.as_ref().map(|h| &h.game_version),
                    "engine_version": s.header.as_ref().map(|h| &h.engine_version),
                    "timestamp": s.header.as_ref().map(|h| h.timestamp_secs),
                    "has_thumbnail": s.header.as_ref().map(|h| h.has_thumbnail),
                    "has_previous": s.has_previous,
                })
            })
            .collect();
        return Ok(json!({ "slots": list, "count": list.len() }));
    }
    let dir = world
        .get_resource::<regia_framework::save_game::SaveDirectory>()
        .cloned()
        .ok_or_else(|| {
            RpcError::not_found(
                "no SaveDirectory in this app",
                "add regia_framework::save_game::SavePlugin",
            )
        })?;
    let slots = regia_framework::save_game::SaveGame::list_slots(&dir);
    let list: Vec<Value> = slots
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "game_version": s.header.as_ref().map(|h| &h.game_version),
                "engine_version": s.header.as_ref().map(|h| &h.engine_version),
                "timestamp": s.header.as_ref().map(|h| h.timestamp_secs),
                "has_thumbnail": s.header.as_ref().map(|h| h.has_thumbnail),
                "has_previous": s.has_previous(),
            })
        })
        .collect();
    Ok(json!({ "slots": list, "count": list.len() }))
}

#[cfg(test)]
#[allow(clippy::needless_borrow)]
mod tests {
    use super::*;
    use regia_framework::camera::{CameraPlugin, CameraRig, ChaseCamera};
    use regia_framework::script::ScriptPlugin;
    use regia_framework::settings::{GameSettings, SettingsPath, SettingsPlugin};
    use regia_framework::sim_bridge::SimBridgePlugin;
    use regia_scene::Transform;

    /// A trivial sim so the bridge runs headless.
    #[derive(Resource)]
    struct IdleSim;
    impl regia_framework::sim_bridge::ExternalSim for IdleSim {
        type Snapshot = Snap;
        type Event = String;
        fn submit_input(&mut self, _input: &regia_framework::sim_bridge::BridgeInput) {}
        fn read_snapshot(
            &mut self,
            _t: std::time::Duration,
            _out: &mut Snap,
        ) -> regia_framework::sim_bridge::SnapshotTick {
            regia_framework::sim_bridge::SnapshotTick(1)
        }
        fn drain_events(&mut self, _out: &mut Vec<String>) {}
    }
    #[derive(Default)]
    struct Snap;
    impl regia_framework::sim_bridge::SimSnapshot for Snap {
        fn for_each_entity(&self, _f: impl FnMut(regia_framework::sim_bridge::SimEntityView<'_>)) {}
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            ScriptPlugin,
            CameraPlugin,
            SettingsPlugin,
            SimBridgePlugin::<IdleSim>::new(IdleSim),
        ))
        .insert_resource(SettingsPath(None))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        // A camera rig chasing a target.
        let target = app.world_mut().spawn(Transform::default()).id();
        app.world_mut()
            .spawn((Transform::default(), CameraRig::default(), ChaseCamera { target }));
        app
    }

    #[test]
    fn camera_get_and_set_mode_round_trip() {
        let mut app = app();
        app.update();

        let got = camera_get(&mut app.world_mut(), &json!({})).expect("camera.get works");
        assert_eq!(got["mode"], "chase");

        let set =
            camera_set_mode(&mut app.world_mut(), &json!({ "mode": "overhead" })).expect("set_mode works");
        assert_eq!(set["before"], "chase");
        assert_eq!(set["after"], "overhead");
        app.update(); // runs the new mode path

        // Aliases resolve; unknown modes are invalid params.
        assert!(camera_set_mode(&mut app.world_mut(), &json!({ "mode": "spectator" })).is_ok());
        assert!(camera_set_mode(&mut app.world_mut(), &json!({ "mode": "warp" })).is_err());

        // Spec 73 § 6.4 (ADR-7213): `follow` picks the pose of a predicted target; the default
        // is the smoothed pose, an omitted `follow` leaves it, an unknown one is refused.
        assert_eq!(got["follow"], "smoothed");
        let set = camera_set_mode(
            &mut app.world_mut(),
            &json!({ "mode": "chase", "follow": "simulated" }),
        )
        .expect("follow");
        assert_eq!(set["follow"], "simulated");
        let set = camera_set_mode(&mut app.world_mut(), &json!({ "mode": "overhead" })).expect("keeps");
        assert_eq!(set["follow"], "simulated");
        assert_eq!(
            camera_get(&mut app.world_mut(), &json!({})).expect("get")["follow"],
            "simulated"
        );
        assert!(
            camera_set_mode(
                &mut app.world_mut(),
                &json!({ "mode": "chase", "follow": "lerp" })
            )
            .is_err()
        );
    }

    #[derive(Component)]
    struct Tag;

    #[test]
    fn rig_entity_picks_the_lowest_id_even_after_a_component_add() {
        let mut app = app();
        let first = {
            let mut q = app.world_mut().query_filtered::<Entity, With<CameraRig>>();
            q.iter(app.world()).next().expect("the app() rig")
        };
        let other = app
            .world_mut()
            .spawn((Transform::default(), CameraRig::default()))
            .id();
        // "Lowest" is `Entity`'s order (its bits), which is not spawn order in bevy_ecs 0.20.
        let lowest = first.min(other);
        assert!(lowest.to_bits() < first.max(other).to_bits());
        // Moving the lower rig to a new archetype must not change the choice.
        app.world_mut().entity_mut(lowest).insert(Tag);
        assert_eq!(rig_entity(app.world_mut(), &json!({})).ok(), Some(lowest));
    }

    #[test]
    fn settings_get_reports_the_runtime_table() {
        let mut app = app();
        app.update();
        let whole = settings_get(app.world_mut(), &json!({})).expect("get whole");
        assert_eq!(
            whole["runtime"],
            json!({ "pause_on_focus_loss": false, "keep_screen_on": false })
        );
        regia_framework::project_runtime::ProjectRuntimeConfig {
            pause_on_focus_loss: true,
            keep_screen_on: true,
        }
        .apply(app.world_mut());
        let whole = settings_get(app.world_mut(), &json!({})).expect("get whole");
        assert_eq!(
            whole["runtime"],
            json!({ "pause_on_focus_loss": true, "keep_screen_on": true })
        );
    }

    #[test]
    fn settings_get_and_set_through_reflection() {
        let mut app = app();
        app.update(); // baseline

        let whole = settings_get(&mut app.world_mut(), &json!({})).expect("get whole");
        assert!(whole["settings"]["input"]["sensitivity"].as_f64() == Some(1.0));

        let set = settings_set(
            &mut app.world_mut(),
            &json!({ "path": "input.sensitivity", "value": 2.5 }),
        )
        .expect("set works");
        assert_eq!(set["before"], json!(1.0));
        assert_eq!(set["after"], json!(2.5));
        assert_eq!(app.world().resource::<GameSettings>().input.sensitivity, 2.5);

        let dry = settings_set(
            &mut app.world_mut(),
            &json!({ "path": "input.sensitivity", "value": 0.0, "dry_run": true }),
        )
        .expect("dry_run");
        assert_eq!(dry["before"], json!(2.5));
        assert_eq!(dry["after"], json!(0.0));
        assert_eq!(
            app.world().resource::<GameSettings>().input.sensitivity,
            2.5,
            "dry_run does not write"
        );

        // Live change applied: the settings plugin sends SettingsChanged.
        app.update();
        use regia_ecs::ecs::message::MessageCursor;
        let mut cursor = MessageCursor::default();
        let events: Vec<_> = cursor
            .read(
                app.world()
                    .resource::<Messages<regia_framework::settings::SettingsChanged>>(),
            )
            .cloned()
            .collect();
        assert_eq!(events.len(), 1, "live change applied and announced");
        assert_eq!(events[0].after.input.sensitivity, 2.5);

        // Bad path and bad value are clean errors, not panics.
        assert!(settings_set(&mut app.world_mut(), &json!({ "path": "nope.field", "value": 1 })).is_err());
        assert!(
            settings_set(
                &mut app.world_mut(),
                &json!({ "path": "input.sensitivity", "value": "fast" })
            )
            .is_err()
        );
    }

    #[test]
    fn settings_set_writes_scene_motion_and_bus_fields() {
        let mut app = app();
        app.update();
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "accessibility.camera_shake_scale", "value": 0.25 }),
        )
        .expect("shake scale");
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "audio.voice", "value": 0.5 }),
        )
        .expect("voice");
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "audio.mute_voice", "value": true }),
        )
        .expect("mute");
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "accessibility.vibration_scale", "value": 0.0 }),
        )
        .expect("vibration");
        app.update();
        let settings = app.world().resource::<regia_framework::settings::GameSettings>();
        assert_eq!(settings.accessibility.camera_shake_scale, 0.25);
        assert_eq!(settings.audio.voice, 0.5);
        assert!(settings.audio.mute_voice);
        assert_eq!(settings.accessibility.vibration_scale, 0.0);
        assert_eq!(
            app.world()
                .resource::<regia_framework::settings::SceneMotion>()
                .shake_scale,
            0.25
        );
    }

    /// `ui-parity-roadmap.md` lane 2 acceptance ("Agent: `settings.set` for the three fields"):
    /// `text_scale`, `reduced_motion` and `high_contrast` go in through `settings.set` and reach the
    /// UI's live `UiAccessibility` resource on the next update (ADR-341).
    #[test]
    fn settings_set_drives_the_three_ui_accessibility_fields() {
        let mut app = app();
        app.insert_resource(regia_ui::UiAccessibility::default());
        app.update();
        for (path, value) in [
            ("accessibility.text_scale", json!(1.5)),
            ("accessibility.reduced_motion", json!(true)),
            ("accessibility.high_contrast", json!(true)),
        ] {
            let out = settings_set(&mut app.world_mut(), &json!({ "path": path, "value": value }))
                .unwrap_or_else(|e| panic!("{path}: {e:?}"));
            assert_eq!(out["after"], value, "{path}");
        }
        app.update();
        let settings = &app.world().resource::<GameSettings>().accessibility;
        assert_eq!(settings.text_scale, 1.5);
        assert!(settings.reduced_motion && settings.high_contrast);
        let live = *app.world().resource::<regia_ui::UiAccessibility>();
        assert_eq!(live.text_scale, 1.5);
        assert!(live.reduced_motion, "reduced motion reaches the UI");
        assert!(live.high_contrast, "high contrast reaches the UI");
        let readback = settings_get(&mut app.world_mut(), &json!({})).expect("get");
        assert_eq!(readback["settings"]["accessibility"]["text_scale"], json!(1.5));
        // A value of the wrong type is a clean error and changes nothing.
        assert!(
            settings_set(
                &mut app.world_mut(),
                &json!({ "path": "accessibility.reduced_motion", "value": "yes" })
            )
            .is_err()
        );
        assert!(
            app.world()
                .resource::<GameSettings>()
                .accessibility
                .reduced_motion
        );
    }

    #[test]
    fn scripts_list_reports_registered_types() {
        use regia_framework::script::RegisterScriptExt;

        #[derive(regia_macros::Script, Reflect, Default, Clone)]
        #[script(order = -7)]
        #[allow(dead_code)]
        struct Probe {
            field: f32,
        }
        impl regia_framework::script::Script for Probe {}

        let mut app = app();
        app.register_script::<Probe>();
        let listed = scripts_list(&mut app.world_mut()).expect("list works");
        assert!(listed["count"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn sim_counters_and_framerate_check() {
        let mut app = app();
        for _ in 0..5 {
            app.update();
        }
        let counters = sim_counters(&mut app.world_mut()).expect("counters present");
        assert_eq!(counters["frames"], json!(5));
        // The router's reaction counts ride along (ADR-7210): none routed here.
        assert_eq!(counters["reactions"]["vfx"], json!(0));
        assert_eq!(counters["reactions"]["unplaced"], json!(0));

        let check = sim_check_framerate_independence(
            &mut app.world_mut(),
            &json!({ "a": { "ticks": 120, "wall_secs": 2.0 }, "b": { "ticks": 119, "wall_secs": 2.0 } }),
        )
        .expect("check works");
        assert_eq!(check["pass"], json!(true));
        let ratio = check["ratio"].as_f64().expect("ratio");
        assert!((ratio - (60.0 / 59.5)).abs() < 1e-9, "{ratio}");

        let fail = sim_check_framerate_independence(
            &mut app.world_mut(),
            &json!({ "a": { "ticks": 60, "wall_secs": 2.0 }, "b": { "ticks": 600, "wall_secs": 2.0 } }),
        )
        .expect("check works");
        assert_eq!(fail["pass"], json!(false));
    }

    /// Bad samples stay `F0002` when the clock is absent. A well-formed check without `Time` is
    /// `NOT_FOUND` and does not panic.
    #[test]
    fn framerate_check_without_time_is_not_found() {
        let mut world = World::new();
        let bad = sim_check_framerate_independence(&mut world, &json!({})).expect_err("params");
        assert_eq!(bad.code, crate::protocol::INVALID_PARAMS);
        let missing = sim_check_framerate_independence(
            &mut world,
            &json!({ "a": { "ticks": 60, "wall_secs": 1.0 }, "b": { "ticks": 60, "wall_secs": 1.0 } }),
        )
        .expect_err("no clock");
        assert_eq!(missing.code, crate::protocol::NOT_FOUND);
    }

    /// ADR-7880: `settings.get` answers what the display settings did to the primary window.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn settings_get_reports_the_applied_display_values() {
        let mut app = app();
        let window = app
            .world_mut()
            .spawn((regia_platform::Window::default(), regia_platform::PrimaryWindow))
            .id();
        app.update();
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "display.vsync", "value": false }),
        )
        .expect("set vsync");
        settings_set(
            &mut app.world_mut(),
            &json!({ "path": "display.resolution", "value": [1600, 900] }),
        )
        .expect("set resolution");
        app.update();
        let whole = settings_get(&mut app.world_mut(), &json!({})).expect("get whole");
        let display = &whole["applied"]["display"];
        assert_eq!(display["window"], json!(window.to_bits()));
        assert_eq!(display["present_mode"], "immediate");
        assert_eq!(display["logical_size"], json!([1600.0, 900.0]));
        assert_eq!(display["mode"], "windowed");
        assert_eq!(display["swapchain"], Value::Null, "no renderer surface here");
        assert_eq!(display["refused"], Value::Null);
        let one = settings_get(&mut app.world_mut(), &json!({ "path": "display.vsync" })).expect("get one");
        assert_eq!(one["value"], false);
        assert_eq!(one["applied"]["display"]["present_mode"], "immediate");
        let other = settings_get(&mut app.world_mut(), &json!({ "path": "audio.master" })).expect("get");
        assert!(
            other.get("applied").is_none(),
            "only display paths carry the report"
        );
    }

    /// ADR-7880: `players.list` gives each player's split-screen rect and the tagged cameras.
    #[test]
    fn players_list_reports_split_screen_viewports_and_cameras() {
        use regia_framework::player::{PlayerKind, PlayerManager, PlayerPlugin, SplitScreenCamera};
        let mut app = app();
        app.add_plugins(PlayerPlugin);
        for _ in 0..2 {
            PlayerManager::spawn_player_static(app.world_mut(), PlayerKind::Local).expect("can join");
        }
        let camera = app
            .world_mut()
            .spawn((regia_render::Camera::default(), SplitScreenCamera { player: 1 }))
            .id();
        app.update();
        let list = players_list(&mut app.world_mut()).expect("players.list works");
        let second = &list["players"][1];
        assert_eq!(second["index"], 1);
        assert_eq!(second["viewport"], json!([0.5, 0.0, 0.5, 1.0]));
        assert_eq!(
            second["cameras"],
            json!([{ "entity": camera.to_bits(), "viewport": [0.5, 0.0, 0.5, 1.0] }])
        );
        assert_eq!(list["players"][0]["cameras"], json!([]));
    }

    #[test]
    fn players_list_and_get_with_possession() {
        use regia_framework::player::{PlayerManager, PlayerPlugin};

        let mut app = app();
        app.add_plugins(PlayerPlugin);

        // Spawn a player + controller through the manager static helper.
        let (player_entity, controller) =
            PlayerManager::spawn_player_static(app.world_mut(), regia_framework::player::PlayerKind::Local)
                .expect("can join");
        // Wire controller to player.
        app.world_mut()
            .get_mut::<regia_framework::player::Player>(player_entity)
            .expect("player entity has Player component")
            .controller = controller;

        let list = players_list(&mut app.world_mut()).expect("players.list works");
        assert_eq!(list["count"], json!(1));
        assert_eq!(list["max_local"], json!(4));

        // Get by index.
        let got =
            players_get(&mut app.world_mut(), &json!({ "index": 0 })).expect("players.get by index works");
        assert_eq!(got["entity"], json!(player_entity.to_bits()));

        // Spawn a dummy character to possess.
        let character = app.world_mut().spawn_empty().id();
        let possess = scene_possess(
            &mut app.world_mut(),
            &json!({ "controller": controller.to_bits(), "entity": character.to_bits() }),
        )
        .expect("possess works");
        assert_eq!(possess["after"], json!(character.to_bits()));

        // Verify the controller now holds the possessed entity.
        let pc = app
            .world()
            .get::<regia_framework::player::PlayerController>(controller)
            .expect("controller entity has PlayerController component");
        assert_eq!(pc.possessed, character);
    }

    #[test]
    fn save_save_and_list_round_trip() {
        use regia_framework::save_game::{SaveDirectory, SavePlugin};

        let mut app = app();
        app.init_resource::<AppTypeRegistry>();
        app.add_plugins(SavePlugin::default());

        // Point to a temp directory.
        let dir = std::env::temp_dir().join(format!("regia-agent-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        app.world_mut().insert_resource(SaveDirectory(dir.clone()));

        // Register a saveable type.
        #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
        #[reflect(Component, Default)]
        struct Coin {
            count: u32,
        }
        app.register_type::<Coin>();
        app.register_type::<regia_framework::save_game::Saveable>();
        app.world_mut()
            .spawn((regia_framework::save_game::Saveable, Coin { count: 42 }));

        let saved = save_save(&mut app.world_mut(), &json!({ "slot": "Test" })).expect("save.save works");
        assert_eq!(saved["slot"], json!("Test"));

        let slots = save_list_slots(&mut app.world_mut()).expect("list slots works");
        assert!(slots["count"].as_u64().unwrap_or(0) >= 1, "slot listed: {slots}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_missing_slot_is_not_found() {
        use regia_framework::save_game::{SaveDirectory, SavePlugin};

        let mut app = app();
        app.init_resource::<AppTypeRegistry>();
        app.add_plugins(SavePlugin::default());

        let dir = std::env::temp_dir().join(format!("regia-agent-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        app.world_mut().insert_resource(SaveDirectory(dir.clone()));

        let err = save_load(&mut app.world_mut(), &json!({ "slot": "Nope" }))
            .expect_err("loading a missing slot must fail");
        assert!(err.message.contains("does not exist"), "{err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handlers_degrade_without_plugins() {
        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        assert!(scripts_list(&mut bare.world_mut()).is_err());
        assert!(camera_get(&mut bare.world_mut(), &json!({})).is_err());
        assert!(settings_get(&mut bare.world_mut(), &json!({})).is_err());
        assert!(sim_counters(&mut bare.world_mut()).is_err());
        assert!(players_list(&mut bare.world_mut()).is_err());
        assert!(save_list_slots(&mut bare.world_mut()).is_err());
    }
}
