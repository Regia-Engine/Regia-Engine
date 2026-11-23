// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the determinism state hash (ADR-537, `book/agents/determinism.md`):
//! `sim.state_hash` and `sim.state_diff`.
//!
//! Both read [`regia_framework::state_hash::world_snapshot`]: every reflected component of every
//! entity as canonical text keyed `"<entity>/<Component>"` (entities by `Name`, map and set
//! entries sorted). An agent checks a run for determinism by stepping two runs of the same seed
//! and input to the same frame and comparing `sim.state_hash`; when the hashes differ, it passes
//! the first run's per-key hashes (`keys: true`) to `sim.state_diff` in the second run to learn
//! which components differ.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_framework::state_hash::{StateSnapshot, key_hashes, snapshot_hash, world_snapshot};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// Default and largest number of keys `sim.state_diff` lists per category.
const DIFF_LIMIT_DEFAULT: usize = 50;
const DIFF_LIMIT_MAX: usize = 1000;

/// What made this session not reproducible (ADR-9850 (b)): `[{ code, source, detail }]`, empty
/// when nothing did. Two runs whose hashes differ while one lists an event are not comparable.
fn nondeterministic(world: &World) -> Value {
    world
        .get_resource::<regia_framework::session_determinism::SessionDeterminism>()
        .map_or_else(|| json!([]), |record| json!(record.events))
}

fn hex(value: u64) -> String {
    format!("{value:016x}")
}

fn bad(method: &str, code: &str, message: &str) -> RpcError {
    RpcError::invalid_params(format!("{method}: {message} [{code}]"))
}

/// Retains the keys selected by the `prefix` and `ignore` parameters.
fn retain_keys<T>(snapshot: &mut BTreeMap<String, T>, method: &str, params: &Value) -> Result<(), RpcError> {
    let prefix = match params.get("prefix") {
        None | Some(Value::Null) => None,
        Some(Value::String(prefix)) => Some(prefix.as_str()),
        Some(_) => return Err(bad(method, "bad_prefix", "`prefix` must be a string")),
    };
    let ignore: BTreeSet<&str> = match params.get("ignore") {
        None | Some(Value::Null) => BTreeSet::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().ok_or_else(|| {
                    bad(
                        method,
                        "bad_ignore",
                        "`ignore` must be an array of component names",
                    )
                })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(bad(
                method,
                "bad_ignore",
                "`ignore` must be an array of component names",
            ));
        }
    };
    snapshot.retain(|key, _| {
        let component = key.rsplit_once('/').map_or(key.as_str(), |(_, c)| c);
        prefix.is_none_or(|p| key.starts_with(p)) && !ignore.contains(component)
    });
    Ok(())
}

/// The live snapshot limited by the request's filters.
fn filtered(world: &World, method: &str, params: &Value) -> Result<StateSnapshot, RpcError> {
    let mut snapshot = world_snapshot(world);
    retain_keys(&mut snapshot, method, params)?;
    Ok(snapshot)
}

fn entity_count(snapshot: &StateSnapshot) -> usize {
    snapshot
        .keys()
        .filter_map(|key| key.rsplit_once('/').map(|(entity, _)| entity))
        .collect::<BTreeSet<_>>()
        .len()
}

fn frame(world: &World) -> u64 {
    world.get_resource::<Time>().map_or(0, Time::frame_count)
}

/// `sim.state_hash { prefix?, ignore?, keys? }`: the hash of the world's reflected state, with
/// per-key hashes when `keys` is true.
pub fn sim_state_hash(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "sim.state_hash";
    let with_keys = match params.get("keys") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(METHOD, "bad_keys", "`keys` must be a boolean")),
    };
    let snapshot = filtered(world, METHOD, params)?;
    let mut out = json!({
        "hash": hex(snapshot_hash(&snapshot)),
        "frame": frame(world),
        "entities": entity_count(&snapshot),
        "values": snapshot.len(),
        "nondeterministic": nondeterministic(world),
    });
    if with_keys {
        let keys: Map<String, Value> = key_hashes(&snapshot)
            .into_iter()
            .map(|(key, hash)| (key, Value::String(hex(hash))))
            .collect();
        out["keys"] = Value::Object(keys);
    }
    Ok(out)
}

/// `sim.state_diff { against, prefix?, ignore?, limit? }`: which keys differ from `against` (the
/// `keys` object of an earlier `sim.state_hash`).
pub fn sim_state_diff(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    const METHOD: &str = "sim.state_diff";
    let mut against: BTreeMap<String, u64> = match params.get("against") {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .and_then(|text| u64::from_str_radix(text, 16).ok())
                    .map(|hash| (key.clone(), hash))
                    .ok_or_else(|| {
                        bad(
                            METHOD,
                            "bad_against",
                            &format!("`against.{key}` must be a 16-digit hex hash from sim.state_hash"),
                        )
                    })
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(bad(
                METHOD,
                "bad_against",
                "`against` (the `keys` object of sim.state_hash { keys: true }) is required",
            ));
        }
    };
    let limit = match params.get("limit") {
        None | Some(Value::Null) => DIFF_LIMIT_DEFAULT,
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=DIFF_LIMIT_MAX).contains(n))
            .ok_or_else(|| bad(METHOD, "bad_limit", "`limit` must be an integer from 1 to 1000"))?,
    };
    retain_keys(&mut against, METHOD, params)?;
    let snapshot = filtered(world, METHOD, params)?;
    let now = key_hashes(&snapshot);
    let changed: Vec<&String> = now
        .iter()
        .filter(|(key, hash)| against.get(*key).is_some_and(|old| old != *hash))
        .map(|(key, _)| key)
        .collect();
    let added: Vec<&String> = now.keys().filter(|key| !against.contains_key(*key)).collect();
    let removed: Vec<&String> = against.keys().filter(|key| !now.contains_key(*key)).collect();
    let truncated = changed.len() > limit || added.len() > limit || removed.len() > limit;
    Ok(json!({
        "hash": hex(snapshot_hash(&snapshot)),
        "frame": frame(world),
        "same": changed.is_empty() && added.is_empty() && removed.is_empty(),
        "counts": { "changed": changed.len(), "added": added.len(), "removed": removed.len() },
        "changed": changed.iter().take(limit).collect::<Vec<_>>(),
        "added": added.iter().take(limit).collect::<Vec<_>>(),
        "removed": removed.iter().take(limit).collect::<Vec<_>>(),
        "truncated": truncated,
        "nondeterministic": nondeterministic(world),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component, Reflect, Default)]
    #[reflect(Component)]
    struct Speed(f32);

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.register_type::<Speed>();
        app.world_mut()
            .spawn((regia_scene::components::Name::new("runner"), Speed(1.0)));
        app.world_mut()
            .spawn((regia_scene::components::Name::new("walker"), Speed(0.5)));
        app.update();
        app
    }

    #[test]
    fn ignore_and_prefix_do_not_report_filtered_keys_as_removed() {
        let mut app = app();
        let before = sim_state_hash(app.world_mut(), &json!({ "keys": true })).expect("hash");
        let world = app.world_mut();
        let mut speeds = world.query::<(&regia_scene::components::Name, &mut Speed)>();
        for (name, mut speed) in speeds.iter_mut(world) {
            if name.as_str() == "runner" {
                speed.0 = 3.0;
            }
        }
        for mut params in [
            json!({ "ignore": ["Speed"] }),
            json!({ "prefix": "walker/" }),
            json!({ "prefix": "runner/", "ignore": ["Speed"] }),
        ] {
            params["against"] = before["keys"].clone();
            let diff = sim_state_diff(world, &params).expect("filtered diff");
            assert_eq!(diff["same"], true, "{params}: {diff}");
            assert_eq!(diff["counts"], json!({ "added": 0, "removed": 0, "changed": 0 }));
        }
        let mut against = before["keys"].clone();
        against["walker/Removed"] = json!("0000000000000000");
        let diff = sim_state_diff(world, &json!({ "against": against, "prefix": "walker/" })).expect("diff");
        assert_eq!(diff["removed"], json!(["walker/Removed"]));
        assert_eq!(diff["same"], false);
    }

    #[test]
    fn a_session_marked_nondeterministic_reports_it_beside_the_hash() {
        // ADR-9850 (b): an extension hot reload marks the session; the determinism tools say so.
        let mut app = app();
        let clean = sim_state_hash(app.world_mut(), &json!({})).expect("hash");
        assert_eq!(clean["nondeterministic"], json!([]));
        regia_framework::session_determinism::mark_nondeterministic(
            app.world_mut(),
            regia_framework::session_determinism::NONDETERMINISTIC_RELOAD,
            "live_ext",
            "generation 1",
        );
        let marked = sim_state_hash(app.world_mut(), &json!({})).expect("hash");
        assert_eq!(marked["hash"], clean["hash"], "the mark is not part of the state");
        assert_eq!(
            marked["nondeterministic"],
            json!([{ "code": "nondeterministic_reload", "source": "live_ext", "detail": "generation 1" }])
        );
        let diff = sim_state_diff(app.world_mut(), &json!({ "against": {} })).expect("diff");
        assert_eq!(diff["nondeterministic"][0]["source"], "live_ext");
    }

    #[test]
    fn the_hash_is_stable_and_follows_the_state() {
        let mut app = app();
        let first = sim_state_hash(app.world_mut(), &json!({})).expect("hash");
        let again = sim_state_hash(app.world_mut(), &json!({})).expect("hash");
        assert_eq!(first["hash"], again["hash"]);
        assert!(first["values"].as_u64().is_some_and(|v| v >= 2), "{first}");
        assert!(first.get("keys").is_none(), "keys only on request");

        let world = app.world_mut();
        let mut speeds = world.query::<&mut Speed>();
        for mut speed in speeds.iter_mut(world) {
            speed.0 *= 2.0;
        }
        let changed = sim_state_hash(app.world_mut(), &json!({})).expect("hash");
        assert_ne!(first["hash"], changed["hash"]);
    }

    #[test]
    fn the_diff_names_the_changed_component_and_respects_filters() {
        let mut app = app();
        let before = sim_state_hash(app.world_mut(), &json!({ "keys": true })).expect("hash");
        assert!(before["keys"]["runner/Speed"].is_string(), "{before}");

        let same = sim_state_diff(app.world_mut(), &json!({ "against": before["keys"] })).expect("diff");
        assert_eq!(same["same"], json!(true), "{same}");

        let world = app.world_mut();
        let mut speeds = world.query::<(&regia_scene::components::Name, &mut Speed)>();
        for (name, mut speed) in speeds.iter_mut(world) {
            if name.as_str() == "runner" {
                speed.0 = 3.0;
            }
        }
        let diff = sim_state_diff(app.world_mut(), &json!({ "against": before["keys"] })).expect("diff");
        assert_eq!(diff["same"], json!(false));
        assert_eq!(diff["changed"], json!(["runner/Speed"]));
        assert_eq!(diff["counts"]["added"], json!(0));

        // Ignoring the component (or limiting to the other entity) hides the change.
        let ignored = sim_state_diff(
            app.world_mut(),
            &json!({ "against": before["keys"], "ignore": ["Speed"] }),
        )
        .expect("diff");
        assert_eq!(ignored["changed"], json!([]));
        let walker = sim_state_hash(app.world_mut(), &json!({ "prefix": "walker/" })).expect("hash");
        assert_eq!(walker["entities"], json!(1));
    }

    #[test]
    fn malformed_parameters_name_their_code() {
        let mut app = app();
        for (method, params, code) in [
            ("sim.state_hash", json!({ "keys": 1 }), "[bad_keys]"),
            ("sim.state_hash", json!({ "prefix": 3 }), "[bad_prefix]"),
            ("sim.state_hash", json!({ "ignore": "Speed" }), "[bad_ignore]"),
            ("sim.state_diff", json!({}), "[bad_against]"),
            (
                "sim.state_diff",
                json!({ "against": { "a/B": "zz" } }),
                "[bad_against]",
            ),
            (
                "sim.state_diff",
                json!({ "against": {}, "limit": 0 }),
                "[bad_limit]",
            ),
        ] {
            let result = if method == "sim.state_hash" {
                sim_state_hash(app.world_mut(), &params)
            } else {
                sim_state_diff(app.world_mut(), &params)
            };
            let error = result.expect_err("malformed");
            assert!(
                error.message.contains(code),
                "{method} {params}: {}",
                error.message
            );
        }
    }
}
