// SPDX-License-Identifier: MPL-2.0
//! Lightmaps on the agent protocol (`book/agents/lightmaps.md`, ADR-860). `render.get_lightmaps`
//! reports the flag ([`LightmapSettings`]) and the installed `.flmap` ([`InstalledLightmap`]);
//! `render.set_lightmaps` switches the flag, sets the intensity, loads a `.flmap` from under the
//! project root or clears it. A file that does not load keeps the installed lightmap and reports
//! the reader's code (`lightmap_io`, `lightmap_bad_magic`, `lightmap_version`, `lightmap_header`,
//! `lightmap_payload`, `lightmap_layout`) in `error_code`. Since ADR-1556 it also switches bicubic
//! filtering (`bicubic`) and the directional rebalance (`directional`), and the status reports
//! whether the installed file has directions (`has_directions`). Since ADR-1623 the forward pass
//! shades receivers (`forward_pass` is true), and the status adds the last frame's `forward` stats
//! and the `receivers` report of the `LightmapReceiver` entities. Since ADR-2107 `forward` also says
//! how many draws took the GPU-driven batches, the visibility resolve and the lite shader, and the
//! lookup the frame's profile allowed. Since ADR-2314 the status names the installed file's
//! `texel_encoding` and `denoise` record, and `render.lightmap_encode_report` measures what a block
//! encoding (BC6H, ASTC HDR) would cost the installed lightmap without changing it. Since ADR-2656
//! `render.get_gi_mode` and `level.get_baked_gi` carry the summary [`gi_summary`] builds.
//! Since ADR-4762, `specular` switches receiver indirect specular from the bake (on by default).
//! The status reports it.

use regia_app::prelude::*;
use regia_render::lightmap::{
    InstalledLightmap, LightmapReceiver, LightmapReceiverReport, LightmapSettings, StaleReceiverPolicy,
    read_cooked_lightmap,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Largest intensity `render.set_lightmaps` accepts.
const MAX_INTENSITY: f64 = 1000.0;

/// Most entry names `render.set_lightmaps`' `unmatched_keys` lists.
const MAX_UNMATCHED_LISTED: usize = 32;

/// Most `receivers` one `render.set_lightmaps` call takes.
const MAX_RECEIVERS: usize = 4096;

/// The `receivers` parameter: `[{ entity, entry }]`, each entity existing with a `MeshRenderer`;
/// `entry: null` removes the entity's `LightmapReceiver`. `None` when absent.
/// Receivers to set: the entity and its entry's name (`None` removes the receiver).
type ReceiverEdits = Vec<(Entity, Option<String>)>;

fn receivers_param(world: &World, params: &Value) -> Result<Option<ReceiverEdits>, RpcError> {
    let items = match params.get("receivers") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(RpcError::invalid_params("`receivers` must be an array")),
    };
    if items.len() > MAX_RECEIVERS {
        return Err(RpcError::invalid_params(format!(
            "at most {MAX_RECEIVERS} receivers per call"
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let bits = item
            .get("entity")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid_params("each receiver needs `entity` (an entity id)"))?;
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        match world.get_entity(entity) {
            Ok(e) if e.contains::<regia_render::MeshRenderer>() => {}
            Ok(_) => {
                return Err(RpcError::invalid_params(format!(
                    "entity {bits} has no MeshRenderer"
                )));
            }
            Err(_) => return Err(RpcError::invalid_params(format!("entity {bits} does not exist"))),
        }
        let entry = match item.get("entry") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(_) => {
                return Err(RpcError::invalid_params(
                    "a receiver's `entry` is the entry's name (a non-empty string) or null",
                ));
            }
        };
        out.push((entity, entry));
    }
    Ok(Some(out))
}

fn status(world: &World) -> Result<Value, RpcError> {
    let settings = world
        .get_resource::<LightmapSettings>()
        .copied()
        .unwrap_or_default();
    let status = match world.get_resource::<InstalledLightmap>() {
        Some(i) => i.status(&settings),
        None => InstalledLightmap::default().status(&settings),
    };
    let internal = |e: serde_json::Error| RpcError::internal(e.to_string());
    let mut value = serde_json::to_value(status).map_err(internal)?;
    // How the installed file stores its directions (ADR-2635): `rgba8`, `bc7`, `astc_ldr`, or empty.
    let direction_encoding = world
        .get_resource::<InstalledLightmap>()
        .and_then(|i| i.lightmap.as_deref())
        .and_then(regia_asset::lightmap::Lightmap::direction_encoding)
        .map_or("", regia_asset::lightmap::LightmapDirectionEncoding::name);
    if let Some(object) = value.as_object_mut() {
        object.insert("direction_encoding".into(), json!(direction_encoding));
    }
    // The forward pass's last frame (ADR-1623) and the receiver entities.
    let forward = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| r.0.lightmap_forward_stats())
        .unwrap_or_default();
    let receivers = world
        .get_resource::<LightmapReceiverReport>()
        .cloned()
        .unwrap_or_default();
    // The open level's lightmap install (ADR-2313) and the receivers grouped by level.
    let level_state = world.get_resource::<regia_framework::lightmap_level::LevelLightmapState>();
    let level = level_state
        .map(regia_framework::lightmap_level::LevelLightmapState::report)
        .unwrap_or_default();
    // The additive levels' lightmaps (ADR-2635) under `level.additive`.
    let additive = level_state
        .map(regia_framework::lightmap_level::LevelLightmapState::additive_report)
        .unwrap_or_default();
    // The streamed cells' lightmaps (ADR-3318) under `level.cells` and `level.cell_stack`.
    let cells = level_state
        .map(regia_framework::lightmap_level::LevelLightmapState::cell_report)
        .unwrap_or_default();
    let cell_stack = level_state
        .and_then(regia_framework::lightmap_level::LevelLightmapState::cell_stack)
        .cloned();
    let levels = receivers_per_level(world);
    if let Some(object) = value.as_object_mut() {
        object.insert("forward".into(), serde_json::to_value(forward).map_err(internal)?);
        object.insert(
            "receivers".into(),
            serde_json::to_value(receivers).map_err(internal)?,
        );
        let mut level = serde_json::to_value(level).map_err(internal)?;
        if let Some(level) = level.as_object_mut() {
            level.insert(
                "additive".into(),
                serde_json::to_value(additive).map_err(internal)?,
            );
            level.insert("cells".into(), serde_json::to_value(cells).map_err(internal)?);
            level.insert(
                "cell_stack".into(),
                serde_json::to_value(cell_stack).map_err(internal)?,
            );
        }
        object.insert("level".into(), level);
        object.insert("levels".into(), levels);
    }
    Ok(value)
}

/// Most receivers `levels[].entities` lists per level.
const MAX_LISTED_PER_LEVEL: usize = 256;

/// One receiver entity as `levels[].entities` lists it.
#[derive(serde::Serialize)]
struct LevelReceiver {
    entity: u64,
    guid: Option<String>,
    part: u32,
    entry: String,
    applied: bool,
    by_level: bool,
}

/// The `LightmapReceiver` entities grouped by the level they belong to (their own `LevelMember`,
/// or their `MeshPartOf` owner's): `[{ level, current, lightmap, receivers, applied, entities }]`,
/// sorted by level (`null`, entities of no level, first) and entity.
fn receivers_per_level(world: &World) -> Value {
    use regia_render::lightmap::{AppliedLightmapReceiver, MeshPartOf};
    use regia_scene::level::LevelMember;
    use regia_scene::prelude::EntityGuid;
    // Only the receiver is queried: `try_query` is `None` while any queried component was never
    // registered in this world (a world where no part or applied receiver exists yet).
    let Some(mut query) = world.try_query::<(Entity, &LightmapReceiver)>() else {
        return json!([]);
    };
    let state = world.get_resource::<regia_framework::lightmap_level::LevelLightmapState>();
    let current = world
        .get_resource::<regia_scene::level::LevelManager>()
        .and_then(regia_scene::level::LevelManager::current);
    let mut groups: std::collections::BTreeMap<Option<u128>, Vec<LevelReceiver>> =
        std::collections::BTreeMap::new();
    for (entity, receiver) in query.iter(world) {
        let part = world.get::<MeshPartOf>(entity);
        let applied = world.get::<AppliedLightmapReceiver>(entity).is_some();
        let owner = part.map_or(entity, |p| p.owner);
        let level = world.get::<LevelMember>(owner).map(|m| m.0);
        let by_level = state.is_some_and(|s| {
            s.attached_entities()
                .iter()
                .any(|(e, name)| *e == entity && *name == receiver.entry)
        });
        groups.entry(level).or_default().push(LevelReceiver {
            entity: entity.to_bits(),
            guid: world.get::<EntityGuid>(owner).map(|g| g.0.to_string()),
            part: part.map_or(0, |p| p.part),
            entry: receiver.entry.clone(),
            applied,
            by_level,
        });
    }
    let out: Vec<Value> = groups
        .into_iter()
        .map(|(level, mut entities)| {
            entities.sort_by_key(|r| r.entity);
            let id = level.map(|l| regia_scene::level::LevelMember(l).level());
            let is_current = id.is_some() && id == current;
            let receivers = entities.len();
            let applied = entities.iter().filter(|r| r.applied).count();
            entities.truncate(MAX_LISTED_PER_LEVEL);
            json!({
                "level": id.map(|l| l.to_string()),
                "current": is_current,
                "lightmap": if is_current { state.and_then(|s| s.file.clone()) } else { None },
                "receivers": receivers,
                "applied": applied,
                "entities": entities,
            })
        })
        .collect();
    Value::Array(out)
}

/// The `lightmaps` object of `render.get_gi_mode` and `level.get_baked_gi` (ADR-2656): the installed
/// lightmap's status from `render.get_lightmaps`, and the receivers against it. `pages` counts its
/// atlas pages (texture array layers, ADR-3300; additive levels stack in one page or append pages,
/// ADR-2635), 0 without one.
/// `stale_instances` counts receivers whose mesh no longer matches their `.flmap` entry;
/// `unverified_instances` those the file has no mesh hash to check (baked before version 6);
/// `stale_receivers` is the policy for a moved mesh, and `stale_fallback` the receivers it has taken
/// off the lightmap (ADR-3006).
pub(crate) fn gi_summary(world: &World) -> Value {
    let settings = world
        .get_resource::<LightmapSettings>()
        .copied()
        .unwrap_or_default();
    let default = InstalledLightmap::default();
    let installed = world.get_resource::<InstalledLightmap>().unwrap_or(&default);
    let status = installed.status(&settings);
    let counts = regia_render::lightmap::lightmap_instance_counts(world, installed.lightmap.as_ref());
    json!({
        "installed": status.installed,
        "pages": status.pages,
        "gpu_bytes": status.gpu_bytes,
        "instances": counts.instances,
        "stale_instances": counts.stale_instances,
        "unverified_instances": counts.unverified_instances,
        "stale_receivers": status.stale_receivers,
        "stale_fallback": world
            .get_resource::<LightmapReceiverReport>()
            .map_or(0, |r| r.stale_fallback),
        "error": status.error,
        "error_code": status.error_code,
    })
}

/// `render.get_lightmaps`: the flag, the intensity and the installed lightmap's status.
pub(crate) fn get_lightmaps(world: &mut World) -> Result<Value, RpcError> {
    status(world)
}

/// The `.flmap` named by `path`, under the project root (no absolute path, no `..`, no symlink
/// leaving the root), with the label it is installed under. The file is the canonical path, or
/// the IO error that resolving it gave: a missing file is never opened by its unresolved name
/// later (a symlink created in between would be followed; audit-agent-sec Low).
type Resolved = (std::io::Result<std::path::PathBuf>, String);

fn resolve(world: &World, path: &str) -> Result<Resolved, RpcError> {
    let relative = std::path::Path::new(path);
    if regia_core::project_path::check_relative(path).is_err() {
        return Err(RpcError::invalid_params(
            "`path` must be relative to the project root, without `..`",
        ));
    }
    if !path.ends_with(".flmap") {
        return Err(RpcError::invalid_params("`path` must name a .flmap file"));
    }
    let root = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .ok_or_else(|| {
            RpcError::not_found(
                "no AssetServer resource: no project root to read from",
                "install the lightmap from the host (InstalledLightmap::install)",
            )
        })?;
    let file = match (root.join(relative).canonicalize(), root.canonicalize()) {
        (Ok(resolved), Ok(root)) if !resolved.starts_with(&root) => {
            return Err(RpcError::invalid_params(format!(
                "{path} resolves outside the project root"
            )));
        }
        (Ok(resolved), Ok(_)) => Ok(resolved),
        // Missing or unreadable: reported as the reader's `lightmap_io` (a cooked copy may
        // still be installed).
        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
    };
    Ok((file, path.to_string()))
}

/// An optional boolean parameter.
fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be a boolean"))),
    }
}

/// `render.set_lightmaps`: `enabled` (bool), `intensity` (0..=1000), `bicubic` (bool),
/// `directional` (bool), `specular` (bool, ADR-4762), `path` (a project-relative `.flmap` to load) or `clear: true` (remove the
/// installed lightmap), `receivers` (`[{ entity, entry }]`: set or, with `entry: null`, remove the
/// entities' `LightmapReceiver`, ADR-1623). Every parameter is optional; all are checked before
/// anything changes.
/// Returns the status before and after and the load error code (`null` when the load worked or
/// none was asked).
pub(crate) fn set_lightmaps(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = flag(params, "enabled")?;
    let bicubic = flag(params, "bicubic")?;
    let directional = flag(params, "directional")?;
    let specular = flag(params, "specular")?;
    let intensity = match params.get("intensity") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|x| x.is_finite() && (0.0..=MAX_INTENSITY).contains(x))
                .ok_or_else(|| RpcError::invalid_params("`intensity` must be a number in 0..=1000"))?
                as f32,
        ),
    };
    let clear = match params.get("clear") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`clear` must be a boolean")),
    };
    let path = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(RpcError::invalid_params("`path` must be a string")),
    };
    if clear && path.is_some() {
        return Err(RpcError::invalid_params("give `path` or `clear`, not both"));
    }
    let file = path.as_deref().map(|p| resolve(world, p)).transpose()?;
    let receivers = receivers_param(world, params)?;
    let auto_receivers = match params.get("auto_receivers") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`auto_receivers` must be a boolean")),
    };
    let stale_receivers = match params.get("stale_receivers") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(StaleReceiverPolicy::from_name)
                .ok_or_else(|| {
                    RpcError::invalid_params("`stale_receivers` must be \"keep_drawing\" or \"fall_back\"")
                })?,
        ),
    };
    let before = status(world)?;
    {
        let mut settings = world.get_resource_or_insert_with(LightmapSettings::default);
        if let Some(e) = enabled {
            settings.enabled = e;
        }
        if let Some(policy) = stale_receivers {
            settings.stale_receivers = policy;
        }
        if let Some(i) = intensity {
            settings.intensity = i;
        }
        if let Some(b) = bicubic {
            settings.bicubic = b;
        }
        if let Some(d) = directional {
            settings.directional = d;
        }
        if let Some(s) = specular {
            settings.specular = s;
        }
    }
    let mut load_error: Option<&'static str> = None;
    {
        if clear {
            world
                .get_resource_or_insert_with(InstalledLightmap::default)
                .clear();
        }
        if let Some((file, label)) = file {
            // A cooked copy (a packaged game's pak or the dev cache) wins over the raw file.
            let cooked = world
                .get_resource_mut::<regia_asset::AssetServer>()
                .and_then(|mut server| read_cooked_lightmap(&mut server, &label));
            let mut installed = world.get_resource_or_insert_with(InstalledLightmap::default);
            let result = match cooked {
                Some(read) => installed.install_result(label, read),
                None => match file {
                    Ok(file) => installed.load(&file, label),
                    Err(e) => installed.install_result(label, Err(e.into())),
                },
            };
            if let Err(e) = result {
                load_error = Some(e.code());
            }
        }
    }
    let receivers_set = receivers.as_ref().map_or(0, Vec::len);
    for (entity, entry) in receivers.into_iter().flatten() {
        let mut e = world.entity_mut(entity);
        match entry {
            Some(entry) => {
                e.insert(LightmapReceiver { entry });
            }
            None => {
                e.remove::<LightmapReceiver>();
            }
        }
    }
    // Runs after the load, so one call can install a level's lightmap and attach its receivers.
    let mut receivers_by_key = 0usize;
    let mut unmatched_keys: Vec<String> = Vec::new();
    if auto_receivers
        && let Some(map) = world
            .get_resource::<InstalledLightmap>()
            .and_then(|i| i.lightmap.clone())
    {
        let found = regia_render::lightmap::receivers_by_key(world, &map);
        receivers_by_key = found.len();
        unmatched_keys = map
            .entries
            .iter()
            .filter(|e| e.key.is_some() && !found.iter().any(|(_, name)| *name == e.name))
            .map(|e| e.name.clone())
            .take(MAX_UNMATCHED_LISTED)
            .collect();
        for (entity, entry) in found {
            world.entity_mut(entity).insert(LightmapReceiver { entry });
        }
    }
    Ok(json!({
        "before": before,
        "after": status(world)?,
        "load_error": load_error,
        "receivers_set": receivers_set,
        "receivers_by_key": receivers_by_key,
        "unmatched_keys": unmatched_keys,
    }))
}

/// `render.lightmap_encode_report { encoding }`: encodes a copy of the installed lightmap's texels
/// as `encoding` (`rgb9e5`, `bc6h` or `astc_hdr`) and reports the bytes and the error against its
/// texels now, and whether this device would sample the blocks as they are (`native`; else the
/// renderer uploads their RGB9E5 decode). Nothing installed changes.
///
/// Errors: `F0002` for a missing or unknown `encoding`; `F0003` (the message starts
/// `lightmap_not_installed`) when no lightmap is installed.
pub(crate) fn lightmap_encode_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_asset::lightmap::LightmapTexelEncoding;
    let encoding = crate::params::opt_str(params, "encoding")?
        .and_then(LightmapTexelEncoding::from_name)
        .ok_or_else(|| RpcError::invalid_params("`encoding` must be \"rgb9e5\", \"bc6h\" or \"astc_hdr\""))?;
    let Some(map) = world
        .get_resource::<InstalledLightmap>()
        .and_then(|i| i.lightmap.clone())
    else {
        return Err(RpcError::not_found(
            "lightmap_not_installed: no lightmap is installed",
            "install one with render.set_lightmaps { path }",
        ));
    };
    let current = map.texel_encoding();
    let mut copy = (*map).clone();
    let report = copy
        .encode_texels(encoding)
        .map_err(|e| RpcError::internal(format!("{}: {e}", e.code())))?;
    let native = world
        .get_resource::<regia_render::RenderDevice>()
        .map(|d| regia_render::lightmap::lightmap_native_blocks(&copy, d.device.features()).is_some());
    let internal = |e: serde_json::Error| RpcError::internal(e.to_string());
    Ok(json!({
        "current": current.name(),
        "report": serde_json::to_value(report).map_err(internal)?,
        "native": native,
        "width": copy.width,
        "height": copy.height,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_reports_the_flag_off_and_nothing_installed_by_default() {
        let mut world = World::new();
        let got = get_lightmaps(&mut world).expect("get works");
        assert_eq!(got["enabled"], false);
        assert_eq!(got["installed"], false);
        assert_eq!(got["forward_pass"], true);
        assert_eq!(got["forward"]["drawn"], 0);
        // Each path's share of `drawn` and the lookup the frame allowed (ADR-2107).
        for path in ["instanced", "visbuffer", "lite"] {
            assert_eq!(got["forward"][path], 0, "{path}");
        }
        assert_eq!(got["forward"]["tier"], "");
        assert_eq!(got["forward"]["active"], false);
        // What the forward pass's copy holds on the GPU (ADR-3145): nothing without a renderer.
        assert_eq!(got["forward"]["uploaded_bytes"], 0);
        assert_eq!(got["forward"]["native_blocks"], false);
        assert_eq!(got["forward"]["uploaded_directions"], false);
        assert_eq!(got["receivers"]["receivers"], 0);
        assert_eq!(got["receivers"]["errors"], serde_json::json!([]));
        assert_eq!(got["gpu_bytes"], 0);
        assert_eq!(got["bicubic"], true);
        assert_eq!(got["directional"], true);
        assert_eq!(got["has_directions"], false);
    }

    #[test]
    fn set_switches_the_filter_and_the_directional_rebalance() {
        let mut world = World::new();
        let r = set_lightmaps(&mut world, &json!({ "bicubic": false, "directional": false })).expect("set");
        assert_eq!(r["before"]["bicubic"], true);
        assert_eq!(r["after"]["bicubic"], false);
        assert_eq!(r["after"]["directional"], false);
        let s = *world.resource::<LightmapSettings>();
        assert!(!s.bicubic && !s.directional && !s.enabled);
        let r = set_lightmaps(&mut world, &json!({ "bicubic": true })).expect("set");
        assert_eq!(r["after"]["bicubic"], true);
        assert_eq!(
            r["after"]["directional"], false,
            "an absent parameter keeps its value"
        );
    }

    /// ADR-4762: the receivers' indirect specular from the bake is on by default and switches.
    #[test]
    fn set_switches_the_receiver_specular() {
        let mut world = World::new();
        let r = set_lightmaps(&mut world, &json!({ "specular": false })).expect("set");
        assert_eq!(r["before"]["specular"], true);
        assert_eq!(r["after"]["specular"], false);
        assert!(!world.resource::<LightmapSettings>().specular);
        let r = set_lightmaps(&mut world, &json!({ "bicubic": true })).expect("set");
        assert_eq!(
            r["after"]["specular"], false,
            "an absent parameter keeps its value"
        );
        let err = set_lightmaps(&mut world, &json!({ "specular": 1 })).expect_err("not a boolean");
        assert!(err.message.contains("specular"), "{}", err.message);
    }

    /// Exercise the public method routing, validation and readback, not only the handler.
    #[test]
    fn receiver_specular_round_trips_through_the_agent_protocol() {
        let mut world = World::new();
        world.init_resource::<regia_app::Time>();
        for specular in [false, true] {
            let changed = crate::dispatch::call_in_process(
                &mut world,
                "render.set_lightmaps",
                json!({ "enabled": true, "specular": specular }),
            )
            .expect("synchronous method")
            .expect("set through protocol");
            assert_eq!(changed["after"]["specular"], specular);
            let got = crate::dispatch::call_in_process(&mut world, "render.get_lightmaps", json!({}))
                .expect("synchronous method")
                .expect("get through protocol");
            assert_eq!(got["specular"], specular);
            assert_eq!(got["enabled"], true);
        }
        let invalid = crate::dispatch::call_in_process(
            &mut world,
            "render.set_lightmaps",
            json!({ "enabled": false, "specular": "yes" }),
        )
        .expect("synchronous method")
        .expect_err("invalid boolean");
        assert!(invalid.message.contains("specular"));
        let settings = world.resource::<LightmapSettings>();
        assert!(
            settings.enabled && settings.specular,
            "invalid calls change no settings"
        );
    }

    #[test]
    fn set_switches_the_flag_and_intensity() {
        let mut world = World::new();
        let r = set_lightmaps(&mut world, &json!({ "enabled": true, "intensity": 2.5 })).expect("set works");
        assert_eq!(r["before"]["enabled"], false);
        assert_eq!(r["after"]["enabled"], true);
        assert_eq!(r["after"]["intensity"], 2.5);
        assert!(r["load_error"].is_null());
        assert!(world.resource::<LightmapSettings>().enabled);
        let r = set_lightmaps(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(r["after"]["enabled"], false);
        assert_eq!(
            r["after"]["intensity"], 2.5,
            "an absent parameter keeps its value"
        );
    }

    #[test]
    fn bad_parameters_change_nothing() {
        let mut world = World::new();
        for bad in [
            json!({ "enabled": "yes" }),
            json!({ "intensity": -1.0 }),
            json!({ "intensity": 5000.0 }),
            json!({ "clear": 1 }),
            json!({ "path": 3 }),
            json!({ "path": "/etc/passwd.flmap" }),
            json!({ "path": "../outside.flmap" }),
            json!({ "path": "maps/bore.fgiv" }),
            json!({ "path": "maps/bore.flmap", "clear": true }),
            json!({ "enabled": true, "intensity": "bright" }),
            json!({ "bicubic": "on" }),
            json!({ "directional": 1, "enabled": true }),
        ] {
            assert!(set_lightmaps(&mut world, &bad).is_err(), "{bad}");
        }
        assert!(
            world.get_resource::<LightmapSettings>().is_none(),
            "nothing was applied"
        );
        // A good path without a project root: not found.
        let e = set_lightmaps(&mut world, &json!({ "path": "maps/bore.flmap" })).expect_err("no root");
        assert_eq!(e.code, crate::protocol::NOT_FOUND);
    }

    /// A `.flmap` cooked into the project's cache loads through `set_lightmaps { path }` even after
    /// the raw file is gone (a packaged game ships only the cooked copy, ADR-1556).
    #[test]
    fn set_reads_the_cooked_copy_before_the_raw_file() {
        let v1: &[u8] = include_bytes!("../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let root = regia_test_utils::unique_temp_dir("regia_agent_cooked_lightmap");
        let maps = root.join("assets").join("maps");
        std::fs::create_dir_all(&maps).expect("maps dir");
        std::fs::write(maps.join("a.flmap"), v1).expect("write");
        let platform = regia_asset_pipeline::host_platform().to_string();
        let report = regia_asset_pipeline::cook_project(regia_asset_pipeline::CookOptions {
            project_root: root.clone(),
            platform: platform.clone(),
            incremental: false,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        std::fs::remove_file(maps.join("a.flmap")).expect("remove the raw file");
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform,
            content_dir: None,
        })
        .expect("open");
        let mut world = World::new();
        world.insert_resource(server);
        let r = set_lightmaps(
            &mut world,
            &json!({ "path": "assets/maps/a.flmap", "enabled": true }),
        )
        .expect("set");
        assert!(r["load_error"].is_null(), "{r}");
        assert_eq!(r["after"]["installed"], true);
        assert_eq!(r["after"]["source"], "assets/maps/a.flmap");
        assert_eq!(r["after"]["width"], 8);
        assert_eq!(
            r["after"]["has_directions"], false,
            "the v1 fixture has no directions"
        );
        // A path the manifest does not name falls back to the raw file: missing here.
        let r = set_lightmaps(&mut world, &json!({ "path": "assets/maps/gone.flmap" })).expect("set");
        assert_eq!(r["load_error"], "lightmap_io");
        assert_eq!(
            r["after"]["installed"], true,
            "a failed load keeps the installed lightmap"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `render.lightmap_encode_report` measures a copy and leaves the installed lightmap as it
    /// is; a bad encoding and no lightmap are errors with their codes.
    #[test]
    fn encode_report_measures_a_copy_of_the_installed_lightmap() {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, LightmapTexelEncoding,
            MeshUnwrap,
        };
        let mut world = World::new();
        let e =
            lightmap_encode_report(&mut world, &json!({ "encoding": "bc6h" })).expect_err("none installed");
        assert_eq!(e.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0003")));
        assert!(e.message.starts_with("lightmap_not_installed"), "{}", e.message);
        let (w, h) = (8u32, 8u32);
        let irr: Vec<[f32; 3]> = (0..w * h).map(|i| [100.0 + i as f32, 80.0, 60.0]).collect();
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; (w * h) as usize],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        let mut installed = InstalledLightmap::default();
        installed.install("host", map.clone());
        world.insert_resource(installed);
        for encoding in ["bc6h", "astc_hdr", "rgb9e5"] {
            let r = lightmap_encode_report(&mut world, &json!({ "encoding": encoding })).expect("report");
            assert_eq!(r["current"], "rgb9e5");
            assert_eq!(r["report"]["encoding"], encoding);
            assert_eq!(r["report"]["texels"], 64);
            let want = if encoding == "rgb9e5" { 256 } else { 64 };
            assert_eq!(r["report"]["bytes"], want, "{r}");
            assert!(r["report"]["max_rel"].as_f64().expect("max") < 0.06, "{r}");
            assert!(r["native"].is_null(), "no device in this world");
        }
        let still = world
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert_eq!(still.texel_encoding(), LightmapTexelEncoding::Rgb9e5);
        assert_eq!(still.texels, map.texels, "the installed lightmap is untouched");
        let got = get_lightmaps(&mut world).expect("get");
        assert_eq!(got["texel_encoding"], "rgb9e5");
        assert!(got["denoise"].is_null());
        for bad in [json!({}), json!({ "encoding": "dxt5" }), json!({ "encoding": 3 })] {
            let e = lightmap_encode_report(&mut world, &bad).expect_err("bad encoding");
            assert_eq!(
                e.data.as_ref().map(|d| d["code"].clone()),
                Some(json!("F0002")),
                "{bad}"
            );
        }
    }

    #[test]
    fn clear_removes_an_installed_lightmap() {
        let mut world = World::new();
        world.insert_resource(InstalledLightmap {
            source: Some("host".into()),
            ..InstalledLightmap::default()
        });
        let r = set_lightmaps(&mut world, &json!({ "clear": true })).expect("clear");
        assert_eq!(r["before"]["source"], "host");
        assert!(r["after"]["source"].is_null());
    }

    /// The receiver report of `apply_lightmap_receivers` reaches `render.get_lightmaps` with its
    /// codes (ADR-1623).
    #[test]
    fn get_reports_the_receiver_entities_and_their_codes() {
        let mut world = World::new();
        world.insert_resource(LightmapReceiverReport {
            receivers: 2,
            applied: 1,
            errors: vec![regia_render::lightmap::LightmapReceiverError {
                entity: 42,
                entry: "instance_9".into(),
                code: "lightmap_entry",
                message: "the installed lightmap has no entry `instance_9`".into(),
            }],
            clusters: 0,
            no_clusters: 1,
            stale_fallback: 0,
        });
        let got = get_lightmaps(&mut world).expect("get works");
        assert_eq!(got["receivers"]["receivers"], 2);
        assert_eq!(got["receivers"]["applied"], 1);
        assert_eq!(got["receivers"]["errors"][0]["code"], "lightmap_entry");
        assert_eq!(got["receivers"]["errors"][0]["entity"], 42);
        assert_eq!(got["receivers"]["clusters"], 0);
        assert_eq!(got["receivers"]["no_clusters"], 1);
        assert_eq!(got["meshlet_entries"], 0, "no lightmap installed");
        assert_eq!(got["forward"]["receivers"], 0, "no renderer: empty stats");
    }

    /// `auto_receivers` attaches every mesh entity a version 3 lightmap was baked for, by guid,
    /// and names the keyed entries nothing in the level matched; a file with no keys attaches none.
    #[test]
    fn auto_receivers_attach_by_entity_key() {
        use regia_asset::lightmap::{
            Lightmap, LightmapBakeInfo, LightmapEntry, LightmapEntryKey, LightmapLayout, MeshUnwrap,
        };
        use regia_scene::prelude::EntityGuid;
        let guid = |s: &str| s.parse::<regia_core::id::Guid>().expect("guid");
        let (a, b) = (
            guid("00000000-0000-0000-0000-00000000000a"),
            guid("00000000-0000-0000-0000-00000000000b"),
        );
        let entry = |name: &str, rect: [u32; 4], key: Option<regia_core::id::Guid>| LightmapEntry {
            name: name.into(),
            source_vertices: 3,
            rect,
            scale_offset: [0.5, 1.0, rect[0] as f32 / 4.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 1.0,
            key: key.map(|entity| LightmapEntryKey { entity, part: 0 }),
            seams: None,
            page: 0,
        };
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
        };
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 1.0,
                padding: 1,
                entries: vec![
                    entry("entity_a_0", [0, 0, 2, 4], Some(a)),
                    entry("entity_b_0", [2, 0, 2, 4], Some(b)),
                ],
                unwraps: vec![unwrap.clone(), unwrap],
            },
            &[[1.0; 3]; 16],
            vec![regia_asset::lightmap::Coverage::BAKED; 16],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        let mut world = World::new();
        let mesh = |world: &mut World, id: Option<regia_core::id::Guid>| {
            let mesh = std::sync::Arc::new(regia_render::CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            });
            let material = std::sync::Arc::new(regia_render::LabMaterial::default());
            let mut e = world.spawn(regia_render::MeshRenderer::new(mesh, material));
            if let Some(id) = id {
                e.insert(EntityGuid(id));
            }
            e.id()
        };
        let matched = mesh(&mut world, Some(a));
        let stranger = mesh(&mut world, Some(guid("00000000-0000-0000-0000-00000000000c")));
        let anonymous = mesh(&mut world, None);
        // A guid with no mesh does not receive.
        world.spawn(EntityGuid(b));
        let mut installed = InstalledLightmap::default();
        installed
            .install_result("maps/yard.flmap", Ok(map))
            .expect("installs");
        world.insert_resource(installed);

        let got = set_lightmaps(&mut world, &json!({ "auto_receivers": true })).expect("set works");
        assert_eq!(got["receivers_by_key"], 1);
        assert_eq!(got["unmatched_keys"], json!(["entity_b_0"]));
        assert_eq!(
            world.get::<LightmapReceiver>(matched).map(|r| r.entry.as_str()),
            Some("entity_a_0")
        );
        assert!(world.get::<LightmapReceiver>(stranger).is_none());
        assert!(world.get::<LightmapReceiver>(anonymous).is_none());
        assert!(set_lightmaps(&mut world, &json!({ "auto_receivers": 1 })).is_err());
    }

    /// `render.get_lightmaps` lists receivers per level (ADR-2313): a part entity belongs to its
    /// owner's level and reports the owner's guid and its part; an entity of no level comes first
    /// under `level: null`; `level` is the open level's install (nothing by default).
    #[test]
    fn get_lists_receivers_per_level_with_their_parts() {
        use regia_render::lightmap::MeshPartOf;
        use regia_scene::level::{LevelId, LevelMember};
        use regia_scene::prelude::EntityGuid;
        let mut world = World::new();
        let mesh = std::sync::Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        let renderer = || regia_render::MeshRenderer::new(mesh.clone(), material.clone());
        let level = LevelId(regia_core::id::Guid::from_u128(0x1e7e1));
        let g = regia_core::id::Guid::from_u128(0xa1);
        let owner = world
            .spawn((
                EntityGuid(g),
                LevelMember::from_level(level),
                renderer(),
                LightmapReceiver {
                    entry: "entity_a_0".into(),
                },
            ))
            .id();
        let part = world
            .spawn((
                renderer(),
                MeshPartOf { owner, part: 1 },
                LightmapReceiver {
                    entry: "entity_a_1".into(),
                },
            ))
            .id();
        let loose = world
            .spawn((
                renderer(),
                LightmapReceiver {
                    entry: "instance_0".into(),
                },
            ))
            .id();
        let got = get_lightmaps(&mut world).expect("get works");
        assert_eq!(got["level"]["file"], Value::Null);
        assert_eq!(got["level"]["installed"], false);
        assert_eq!(got["level"]["warm"], false);
        assert_eq!(
            got["level"]["additive"],
            json!([]),
            "no additive level lightmap (ADR-2635)"
        );
        assert_eq!(got["level"]["cells"], json!([]), "no streamed cell (ADR-3318)");
        assert_eq!(got["level"]["cell_stack"], Value::Null);
        assert_eq!(got["direction_encoding"], "", "no lightmap installed");
        let levels = got["levels"].as_array().expect("levels");
        assert_eq!(levels.len(), 2, "{got}");
        assert_eq!(levels[0]["level"], Value::Null);
        assert_eq!(levels[0]["entities"][0]["entity"], loose.to_bits());
        assert_eq!(levels[1]["level"], level.to_string());
        assert_eq!(levels[1]["current"], false, "no LevelManager");
        assert_eq!(levels[1]["receivers"], 2);
        assert_eq!(levels[1]["applied"], 0, "no frame ran");
        let entities = levels[1]["entities"].as_array().expect("entities");
        let bits: Vec<u64> = entities.iter().filter_map(|e| e["entity"].as_u64()).collect();
        assert!(bits.is_sorted(), "sorted by entity: {bits:?}");
        let find = |e: Entity| {
            entities
                .iter()
                .find(|x| x["entity"] == e.to_bits())
                .cloned()
                .expect("listed")
        };
        let own = find(owner);
        assert_eq!(own["part"], 0);
        assert_eq!(own["guid"], g.to_string());
        let p = find(part);
        assert_eq!(p["part"], 1);
        assert_eq!(p["guid"], g.to_string(), "a part reports its owner's guid");
        assert_eq!(p["entry"], "entity_a_1");
        assert_eq!(p["by_level"], false);
    }

    /// A two-triangle quad's mesh and a lightmap whose one entry `quad` was baked from it, with or
    /// without the bake's mesh hash.
    fn quad_and_map(
        hashed: bool,
    ) -> (
        std::sync::Arc<regia_render::CpuMesh>,
        regia_asset::lightmap::Lightmap,
    ) {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap,
            lightmap_source_hash,
        };
        let n = [0.0, 1.0, 0.0];
        let vertices = vec![
            regia_render::LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], n, [0.0, 0.0]),
            regia_render::LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], n, [1.0, 0.0]),
            regia_render::LabVertex::from_pos_normal_uv([1.0, 0.0, 1.0], n, [1.0, 1.0]),
            regia_render::LabVertex::from_pos_normal_uv([0.0, 0.0, 1.0], n, [0.0, 1.0]),
        ];
        let indices = vec![0, 1, 2, 0, 2, 3];
        let mut map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 4.0,
                padding: 0,
                entries: vec![LightmapEntry {
                    name: "quad".into(),
                    source_vertices: 4,
                    rect: [0, 0, 4, 4],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 4.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
                    indices: indices.clone(),
                }],
            },
            &[[1.0; 3]; 16],
            vec![Coverage::BAKED; 16],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        if hashed {
            map.source_hashes = vec![lightmap_source_hash(
                vertices.iter().map(|v| v.position),
                &indices,
            )];
        }
        (
            std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
            map,
        )
    }

    fn with_receiver(mesh: std::sync::Arc<regia_render::CpuMesh>, entry: &str, world: &mut World) -> Entity {
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        world
            .spawn((
                regia_render::MeshRenderer::new(mesh, material),
                LightmapReceiver { entry: entry.into() },
            ))
            .id()
    }

    /// `mesh` with one vertex moved: the same triangles.
    fn moved_vertex(mesh: &regia_render::CpuMesh) -> std::sync::Arc<regia_render::CpuMesh> {
        let mut vertices = mesh.vertices.clone();
        vertices[2].position[1] = 0.25;
        std::sync::Arc::new(regia_render::CpuMesh {
            vertices,
            indices: mesh.indices.clone(),
        })
    }

    /// `mesh` with its first triangle wound the other way.
    fn flipped_triangle(mesh: &regia_render::CpuMesh) -> std::sync::Arc<regia_render::CpuMesh> {
        let mut indices = mesh.indices.clone();
        indices.swap(1, 2);
        std::sync::Arc::new(regia_render::CpuMesh {
            vertices: mesh.vertices.clone(),
            indices,
        })
    }

    /// `render.get_gi_mode` and `level.get_baked_gi` carry `lightmaps`, the numbers of
    /// `render.get_lightmaps`, and `stale_instances` counts what no longer matches its entry: moved
    /// vertices (the hash), other triangles and a name the file does not have.
    #[test]
    fn the_gi_summary_counts_stale_receivers() {
        let mut world = World::new();
        let none = gi_summary(&world);
        assert_eq!(none["installed"], false);
        assert_eq!(none["pages"], 0);
        assert_eq!(none["gpu_bytes"], 0);
        assert_eq!(none["instances"], 0);
        assert_eq!(none["stale_instances"], 0);
        assert!(none["error"].is_null());

        let (mesh, map) = quad_and_map(true);
        let mut installed = InstalledLightmap::default();
        installed.install("maps/quad.flmap", map);
        world.insert_resource(installed);
        with_receiver(mesh.clone(), "quad", &mut world);
        let fresh = gi_summary(&world);
        assert_eq!(fresh["installed"], true);
        assert_eq!(fresh["pages"], 1);
        assert_eq!(fresh["gpu_bytes"], 64);
        assert_eq!(fresh["instances"], 1);
        assert_eq!(fresh["stale_instances"], 0, "{fresh}");
        assert_eq!(fresh["unverified_instances"], 0, "{fresh}");
        assert_eq!(
            fresh["gpu_bytes"],
            get_lightmaps(&mut world).expect("get")["gpu_bytes"],
            "the same number as render.get_lightmaps"
        );

        // The same triangles, one vertex moved: only the hash can tell.
        with_receiver(moved_vertex(&mesh), "quad", &mut world);
        assert_eq!(gi_summary(&world)["stale_instances"], 1);
        // Other triangles.
        with_receiver(flipped_triangle(&mesh), "quad", &mut world);
        assert_eq!(gi_summary(&world)["stale_instances"], 2);
        // A name the file does not have.
        with_receiver(mesh, "gone", &mut world);
        let got = gi_summary(&world);
        assert_eq!(got["instances"], 4);
        assert_eq!(got["stale_instances"], 3, "{got}");
    }

    /// `render.set_lightmaps` `stale_receivers` sets the policy, keeps it across calls that do not
    /// name it, and refuses other values before anything changes (ADR-3006).
    #[test]
    fn the_stale_receiver_policy_is_set_and_reported() {
        let mut world = World::new();
        let got = get_lightmaps(&mut world).expect("get");
        assert_eq!(got["stale_receivers"], "keep_drawing", "{got}");
        assert_eq!(gi_summary(&world)["stale_receivers"], "keep_drawing");

        let r = set_lightmaps(&mut world, &json!({ "stale_receivers": "fall_back" })).expect("set");
        assert_eq!(r["before"]["stale_receivers"], "keep_drawing");
        assert_eq!(r["after"]["stale_receivers"], "fall_back");
        assert_eq!(gi_summary(&world)["stale_receivers"], "fall_back");

        let r = set_lightmaps(&mut world, &json!({ "intensity": 2.0 })).expect("set");
        assert_eq!(
            r["after"]["stale_receivers"], "fall_back",
            "not named, not changed"
        );

        for bad in [json!("sometimes"), json!(1), json!(true)] {
            assert!(
                set_lightmaps(&mut world, &json!({ "stale_receivers": bad, "intensity": 3.0 })).is_err(),
                "{bad}"
            );
        }
        let got = get_lightmaps(&mut world).expect("get");
        assert_eq!(got["intensity"], 2.0, "a refused call changes nothing");
        assert_eq!(got["stale_receivers"], "fall_back");
        assert_eq!(gi_summary(&world)["stale_fallback"], 0);
    }

    /// A lightmap with no mesh hash cannot tell moved vertices: the receiver is unverified, not stale,
    /// though other triangles still are.
    #[test]
    fn a_file_without_hashes_reports_unverified_receivers() {
        let (mesh, map) = quad_and_map(false);
        let mut world = World::new();
        let mut installed = InstalledLightmap::default();
        installed.install("maps/old.flmap", map);
        world.insert_resource(installed);
        with_receiver(mesh.clone(), "quad", &mut world);
        with_receiver(moved_vertex(&mesh), "quad", &mut world);
        let got = gi_summary(&world);
        assert_eq!(got["stale_instances"], 0, "{got}");
        assert_eq!(got["unverified_instances"], 2, "{got}");
        with_receiver(flipped_triangle(&mesh), "quad", &mut world);
        let got = gi_summary(&world);
        assert_eq!(got["stale_instances"], 1, "{got}");
        assert_eq!(got["unverified_instances"], 2, "{got}");
    }

    /// A failed load is the summary's `error`.
    #[test]
    fn the_gi_summary_reports_the_load_error() {
        let mut world = World::new();
        world.insert_resource(InstalledLightmap {
            error: Some(("no such file".into(), "lightmap_io")),
            ..InstalledLightmap::default()
        });
        let got = gi_summary(&world);
        assert_eq!(got["installed"], false);
        assert_eq!(got["error"], "no such file");
        assert_eq!(got["error_code"], "lightmap_io");
    }

    /// `auto_receivers` attaches every part of a keyed multi-part mesh (ADR-2313), not only part 0.
    #[test]
    fn auto_receivers_attach_every_part() {
        use regia_asset::lightmap::{
            Lightmap, LightmapBakeInfo, LightmapEntry, LightmapEntryKey, LightmapLayout, MeshUnwrap,
        };
        use regia_render::lightmap::MeshPartOf;
        use regia_scene::prelude::EntityGuid;
        let g = regia_core::id::Guid::from_u128(0xa1);
        let entry = |part: u32, x: u32| LightmapEntry {
            name: format!("entity_a_{part}"),
            source_vertices: 3,
            rect: [x, 0, 2, 4],
            scale_offset: [0.5, 1.0, x as f32 / 4.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 1.0,
            key: Some(LightmapEntryKey { entity: g, part }),
            seams: None,
            page: 0,
        };
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
        };
        let map = Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 1.0,
                padding: 1,
                entries: vec![entry(0, 0), entry(1, 2)],
                unwraps: vec![unwrap.clone(), unwrap],
            },
            &[[1.0; 3]; 16],
            vec![regia_asset::lightmap::Coverage::BAKED; 16],
            LightmapBakeInfo::default(),
        )
        .expect("valid");
        let mut world = World::new();
        let mesh = std::sync::Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        let renderer = || regia_render::MeshRenderer::new(mesh.clone(), material.clone());
        let owner = world.spawn((EntityGuid(g), renderer())).id();
        let part = world.spawn((renderer(), MeshPartOf { owner, part: 1 })).id();
        let mut installed = InstalledLightmap::default();
        installed.install("maps/yard.flmap", map);
        world.insert_resource(installed);
        let got = set_lightmaps(&mut world, &json!({ "auto_receivers": true })).expect("set works");
        assert_eq!(got["receivers_by_key"], 2);
        assert_eq!(got["unmatched_keys"], json!([]));
        assert_eq!(
            world.get::<LightmapReceiver>(part).map(|r| r.entry.as_str()),
            Some("entity_a_1")
        );
        assert_eq!(
            world.get::<LightmapReceiver>(owner).map(|r| r.entry.as_str()),
            Some("entity_a_0")
        );
    }

    /// `receivers` sets and removes `LightmapReceiver` on mesh entities; a bad entry changes
    /// nothing, not even the flag (ADR-1623).
    #[test]
    fn set_attaches_and_removes_receivers_and_checks_them_first() {
        let mut world = World::new();
        let mesh = std::sync::Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        let floor = world.spawn(regia_render::MeshRenderer::new(mesh, material)).id();
        let bare = world.spawn_empty().id();
        let bits = floor.to_bits();
        let got = set_lightmaps(
            &mut world,
            &json!({ "enabled": true, "receivers": [{ "entity": bits, "entry": "instance_0" }] }),
        )
        .expect("set works");
        assert_eq!(got["receivers_set"], 1);
        assert_eq!(
            world.get::<LightmapReceiver>(floor).map(|r| r.entry.as_str()),
            Some("instance_0")
        );
        // Each bad receiver is refused before anything changes.
        for bad in [
            json!({ "enabled": false, "receivers": [{ "entity": bare.to_bits(), "entry": "instance_0" }] }),
            json!({ "enabled": false, "receivers": [{ "entity": bits, "entry": 3 }] }),
            json!({ "enabled": false, "receivers": [{ "entity": bits, "entry": "" }] }),
            json!({ "enabled": false, "receivers": [{ "entry": "instance_0" }] }),
            json!({ "enabled": false, "receivers": { "entity": bits } }),
        ] {
            let e = set_lightmaps(&mut world, &bad).expect_err("refused");
            assert_eq!(e.code, crate::protocol::INVALID_PARAMS, "{bad}");
            assert!(
                world.resource::<LightmapSettings>().enabled,
                "{bad}: the flag is unchanged"
            );
        }
        set_lightmaps(
            &mut world,
            &json!({ "receivers": [{ "entity": bits, "entry": null }] }),
        )
        .expect("remove works");
        assert!(world.get::<LightmapReceiver>(floor).is_none());
    }

    /// A paged lightmap (ADR-3300) reports its pages in `render.get_lightmaps` and the GI summary,
    /// and its GPU bytes cover every page.
    #[test]
    fn pages_reach_get_lightmaps_and_the_gi_summary() {
        let (_, one) = quad_and_map(true);
        let mut paged = one.clone();
        let mut second = paged.entries[0].clone();
        second.name = "quad_page1".into();
        second.page = 1;
        paged.entries.push(second);
        paged.unwraps.push(paged.unwraps[0].clone());
        paged.texels.extend(one.texels.clone());
        paged.coverage.extend(one.coverage.clone());
        let dirs = one.directions.clone();
        paged.directions.extend(dirs);
        if !paged.source_hashes.is_empty() {
            paged.source_hashes.push(0);
        }
        paged.validate().expect("two pages");
        let mut world = World::new();
        let mut installed = InstalledLightmap::default();
        installed.install("maps/quad.flmap", one);
        world.insert_resource(installed);
        let single = get_lightmaps(&mut world).expect("status");
        assert_eq!(single["pages"], 1);
        let bytes = single["gpu_bytes"].as_u64().expect("bytes");
        world
            .resource_mut::<InstalledLightmap>()
            .install("maps/quad.flmap", paged);
        let v = get_lightmaps(&mut world).expect("status");
        assert_eq!(v["pages"], 2, "{v}");
        assert_eq!(v["gpu_bytes"].as_u64(), Some(2 * bytes));
        let gi = gi_summary(&world);
        assert_eq!(gi["pages"], 2);
        assert_eq!(gi["gpu_bytes"].as_u64(), Some(2 * bytes));
    }
}
