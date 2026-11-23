// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for game states (`regia_ecs::state`): `state.list`, `state.get`,
//! `state.set`, and the `{ state, op, value }` predicate atom of `play.wait_until` /
//! `play.expect`.
//!
//! States are found through the type registry: `App::init_state` and friends register
//! `ReflectState` (every state) and `ReflectFreelyMutableState` (settable states: plain and
//! sub-states; computed states are not settable). Values travel as reflected JSON, so a unit enum
//! variant is a string (`"Paused"`). Handlers are plain functions over `&mut World`.

use regia_app::prelude::*;
use regia_ecs::reflect::enums::VariantInfo;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{PartialReflect, ReflectFromReflect, TypeInfo, TypeRegistration, TypeRegistry};
use regia_ecs::state::{ReflectFreelyMutableState, ReflectState};
use serde::de::DeserializeSeed;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Every registered state type: `(full type path, short type path)`, sorted by full path.
fn state_types(registry: &TypeRegistry) -> Vec<(&'static str, &'static str)> {
    let mut types: Vec<_> = registry
        .iter_with_data::<ReflectState>()
        .map(|(registration, _)| {
            let table = registration.type_info().type_path_table();
            (table.path(), table.short_path())
        })
        .collect();
    types.sort_unstable();
    types
}

/// Resolves `name` (full type path, short type path, or a unique suffix such as `GameFlow`).
fn resolve<'r>(registry: &'r TypeRegistry, name: &str) -> Result<&'r TypeRegistration, RpcError> {
    // A full path written before the rename to Regia Engine names the legacy crates (ADR-1429).
    let name = &*regia_core::brand::current_type_path(name);
    let candidates: Vec<&TypeRegistration> = registry
        .iter_with_data::<ReflectState>()
        .map(|(registration, _)| registration)
        .filter(|registration| {
            let table = registration.type_info().type_path_table();
            table.path() == name || table.short_path() == name || table.path().ends_with(&format!("::{name}"))
        })
        .collect();
    match candidates.as_slice() {
        [one] => Ok(one),
        [] => Err(RpcError::not_found(
            format!("no state type named {name:?}"),
            "state.list shows every registered state (registered with App::init_state, insert_state, \
             add_sub_state or add_computed_state)",
        )),
        many => Err(RpcError::invalid_params(format!(
            "state name {name:?} is ambiguous: {}; use the full type path",
            many.iter()
                .map(|r| r.type_info().type_path())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn to_json(value: &dyn PartialReflect, registry: &TypeRegistry) -> Value {
    serde_json::to_value(TypedReflectSerializer::new(value, registry)).unwrap_or(Value::Null)
}

/// The current value of a state, or `None` while it does not exist (a sub-state or computed state
/// whose source does not match).
fn current(world: &World, registration: &TypeRegistration, registry: &TypeRegistry) -> Option<Value> {
    let data = registration.data::<ReflectState>()?;
    data.reflect(world)
        .map(|value| to_json(value.as_partial_reflect(), registry))
}

/// The pending `NextState<S>` request (`"Unchanged"`, `{ "Pending": v }`,
/// `{ "PendingIfDifferent": v }`), or `None` for computed states.
fn pending(world: &World, registration: &TypeRegistration, registry: &TypeRegistry) -> Option<Value> {
    let suffix = format!("NextState<{}>", registration.type_info().type_path());
    // Resources are components of a resource entity (bevy_ecs 0.20): find `NextState<S>`'s
    // registration, its component id and entity, then reflect it as a component.
    let (next, reflect) = registry
        .iter_with_data::<ReflectComponent>()
        .find(|(r, _)| r.type_info().type_path().ends_with(&suffix))?;
    let id = world.components().get_id(next.type_id())?;
    let entity = world.resource_entities().get(id)?;
    let value = reflect.reflect(world.get_entity(entity).ok()?)?;
    Some(to_json(value.as_partial_reflect(), registry))
}

/// Unit-variant names of an enum state (the values `state.set` accepts as plain strings).
fn variants(registration: &TypeRegistration) -> Vec<&'static str> {
    match registration.type_info() {
        TypeInfo::Enum(info) => info
            .iter()
            .filter(|variant| matches!(variant, VariantInfo::Unit(_)))
            .map(VariantInfo::name)
            .collect(),
        _ => Vec::new(),
    }
}

fn describe(world: &World, registration: &TypeRegistration, registry: &TypeRegistry) -> Value {
    let current = current(world, registration, registry);
    let settable = registration.data::<ReflectFreelyMutableState>().is_some();
    json!({
        "state": registration.type_info().type_path(),
        "name": registration.type_info().type_path_table().short_path(),
        "exists": current.is_some(),
        "current": current,
        "settable": settable,
        "pending": pending(world, registration, registry),
        "variants": variants(registration),
    })
}

/// `state.list`: every registered state with its current value.
pub fn state_list(world: &mut World) -> Result<Value, RpcError> {
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let states: Vec<Value> = state_types(&registry)
        .into_iter()
        .filter_map(|(path, _)| registry.get_with_type_path(path))
        .map(|registration| describe(world, registration, &registry))
        .collect();
    Ok(json!({ "count": states.len(), "states": states }))
}

/// `state.get`: one state (`state`: full or short type path).
pub fn state_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = state_name(params)?;
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = resolve(&registry, name)?;
    Ok(describe(world, registration, &registry))
}

/// `state.set`: requests a new value for a settable state; applied by the next `StateTransition`
/// (after the next `PreUpdate`). Without `force`, setting the current value is a no-op; with it,
/// the state's `OnExit`/`OnEnter` re-run.
pub fn state_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = state_name(params)?;
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required (e.g. \"Paused\")"))?;
    let force = crate::params::opt_bool(params, "force")?.unwrap_or(false);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = resolve(&registry, name)?;
    let path = registration.type_info().type_path();
    let Some(setter) = registration.data::<ReflectFreelyMutableState>().cloned() else {
        return Err(RpcError::invalid_params(format!(
            "{path} is a computed state; set one of its source states instead"
        )));
    };
    let options = variants(registration);
    let dynamic = TypedReflectDeserializer::new(registration, &registry)
        .deserialize(value)
        .map_err(|e| {
            RpcError::invalid_params(format!(
                "value {value} does not fit {path}: {e}{}",
                if options.is_empty() {
                    String::new()
                } else {
                    format!(" (variants: {})", options.join(", "))
                }
            ))
        })?;
    // Convert up front: `set_next_state` falls back to a panic when `FromReflect` fails.
    let concrete = registration
        .data::<ReflectFromReflect>()
        .and_then(|from| from.from_reflect(dynamic.as_partial_reflect()))
        .ok_or_else(|| RpcError::invalid_params(format!("value {value} does not build a {path}")))?;
    let before = current(world, registration, &registry);
    if force {
        setter.set_next_state(world, concrete.as_ref(), &registry);
    } else {
        setter.set_next_state_if_different(world, concrete.as_ref(), &registry);
    }
    let requested = to_json(concrete.as_partial_reflect(), &registry);
    let mut result = json!({
        "state": path,
        "before": before,
        "requested": requested,
        "force": force,
        "applies": "on the next StateTransition (after the next PreUpdate); poll state.get or wait_until { state, value }",
    });
    if before.is_none() {
        result["warning"] = json!(
            "the state does not exist now (a sub-state whose source does not match); the request is consumed by the next StateTransition and applied only if the source state then makes the sub-state exist"
        );
    }
    Ok(result)
}

fn state_name(params: &Value) -> Result<&str, RpcError> {
    crate::params::opt_str(params, "state")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`state` (a state type, e.g. \"GameFlow\") is required"))
}

/// The predicate snapshot of a state for `{ state, op, value }` atoms: its current value, `null`
/// while it does not exist. Unknown names are an error.
pub(crate) fn state_snapshot(world: &mut World, name: &str) -> Result<Value, String> {
    state_get(world, &json!({ "state": name }))
        .map(|described| described["current"].clone())
        .map_err(|e| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::flow::{GameFlow, GameFlowPlugin, GameFlowSettings};

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(GameFlowSettings {
            skip_main_menu: true,
            ..Default::default()
        })
        .add_plugins(GameFlowPlugin);
        for _ in 0..3 {
            app.update();
        }
        app
    }

    #[test]
    fn list_shows_every_state() {
        let mut app = app();
        let list = state_list(app.world_mut()).map_err(|e| e.message);
        let list = list.as_ref().map(|l| &l["states"]);
        let names: Vec<&str> = list
            .ok()
            .and_then(Value::as_array)
            .map(|s| s.iter().filter_map(|s| s["name"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(names.len(), 3, "{list:?}");
        for name in ["GameFlow", "InGame", "PausePage"] {
            assert!(names.contains(&name), "{name} in {names:?}");
        }
    }

    #[test]
    fn get_reports_current_variants_and_pending() {
        let mut app = app();
        let flow = state_get(app.world_mut(), &json!({ "state": "GameFlow" })).map_err(|e| e.message);
        let flow = flow.unwrap_or_default();
        assert_eq!(flow["current"], "Playing");
        assert_eq!(flow["exists"], true);
        assert_eq!(flow["settable"], true);
        assert_eq!(flow["pending"], "Unchanged");
        assert_eq!(
            flow["variants"],
            json!(["Boot", "MainMenu", "Loading", "Playing", "Paused"])
        );
        let in_game = state_get(app.world_mut(), &json!({ "state": "InGame" })).unwrap_or_default();
        assert_eq!(in_game["exists"], true);
        assert_eq!(in_game["settable"], false);
        let page = state_get(app.world_mut(), &json!({ "state": "PausePage" })).unwrap_or_default();
        assert_eq!(page["exists"], false, "no pause page while playing");
        assert_eq!(page["current"], Value::Null);
    }

    #[test]
    fn set_requests_a_transition() {
        let mut app = app();
        let set = state_set(
            app.world_mut(),
            &json!({ "state": "regia_framework::flow::GameFlow", "value": "Paused" }),
        )
        .map_err(|e| e.message);
        let set = set.unwrap_or_default();
        assert_eq!(set["before"], "Playing");
        assert_eq!(set["requested"], "Paused");
        let flow = state_get(app.world_mut(), &json!({ "state": "GameFlow" })).unwrap_or_default();
        assert_eq!(flow["pending"], json!({ "PendingIfDifferent": "Paused" }));
        app.update();
        assert_eq!(app.world().resource::<State<GameFlow>>().get(), &GameFlow::Paused);
        assert!(app.world().resource::<Time>().paused);
        assert_eq!(state_snapshot(app.world_mut(), "PausePage"), Ok(json!("Main")));
    }

    #[test]
    fn set_rejects_bad_requests() {
        let mut app = app();
        let world = app.world_mut();
        let bad_value = state_set(world, &json!({ "state": "GameFlow", "value": "Flying" }));
        assert!(
            bad_value
                .as_ref()
                .is_err_and(|e| e.message.contains("variants: Boot, MainMenu")),
            "{:?}",
            bad_value.map_err(|e| e.message)
        );
        let computed = state_set(world, &json!({ "state": "InGame", "value": null }));
        assert!(computed.is_err_and(|e| e.message.contains("computed state")));
        let unknown = state_get(world, &json!({ "state": "Weather" }));
        assert!(unknown.is_err_and(|e| e.message.contains("no state type")));
        let missing = state_set(world, &json!({ "value": "Paused" }));
        assert!(missing.is_err());
        // Nothing was requested.
        assert!(matches!(
            *world.resource::<NextState<GameFlow>>(),
            NextState::Unchanged
        ));
    }

    #[test]
    fn force_reenters_the_same_state() {
        let mut app = app();
        #[derive(Resource, Default)]
        struct Entered(u32);
        app.init_resource::<Entered>()
            .add_systems(OnEnter(GameFlow::Playing), |mut e: ResMut<Entered>| e.0 += 1);
        let same = json!({ "state": "GameFlow", "value": "Playing" });
        assert!(state_set(app.world_mut(), &same).is_ok());
        app.update();
        assert_eq!(
            app.world().resource::<Entered>().0,
            0,
            "same value without force: no-op"
        );
        let forced = json!({ "state": "GameFlow", "value": "Playing", "force": true });
        assert!(state_set(app.world_mut(), &forced).is_ok());
        app.update();
        assert_eq!(app.world().resource::<Entered>().0, 1);
    }
}
