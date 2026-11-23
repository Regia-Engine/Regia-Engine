// SPDX-License-Identifier: MPL-2.0
//! Gizmo debug overlays on the agent protocol (`book/agents/gizmos.md`): `debug.gizmos` lists
//! every gizmo config group with its settings, `debug.set_gizmos` toggles and styles one group
//! (the debug overlays `physics`, `nav_mesh`, `audio`, `ai_perception`, the `default` group, or a
//! game's own group) through [`GizmoConfigStore`]. The `locomotion` group (ADR-8729) also lists and
//! takes its sub-toggles (`gait`, `foot_placement`, `ik`) as `toggles`.

use regia_app::prelude::*;
use regia_framework::locomotion_gizmos::LocomotionGizmos;
use regia_render::gizmos::{GizmoConfig, GizmoConfigStore, GizmoLineJoint, GizmoLineStyle};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no GizmoConfigStore resource in this app",
        "add regia_render::RenderPlugin (it installs the gizmo plugin)",
    )
}

fn config_json(name: &str, config: &GizmoConfig) -> Value {
    let style = match config.line.style {
        GizmoLineStyle::Solid => json!("solid"),
        GizmoLineStyle::Dotted => json!("dotted"),
        GizmoLineStyle::Dashed {
            gap_scale,
            line_scale,
        } => {
            json!({ "dashed": { "gap_scale": gap_scale, "line_scale": line_scale } })
        }
    };
    let joints = match config.line.joints {
        GizmoLineJoint::None => json!("none"),
        GizmoLineJoint::Miter => json!("miter"),
        GizmoLineJoint::Bevel => json!("bevel"),
        GizmoLineJoint::Round(resolution) => json!({ "round": resolution }),
    };
    json!({
        "name": name,
        "enabled": config.enabled,
        "line_width": config.line.width,
        "perspective": config.line.perspective,
        "style": style,
        "joints": joints,
        "depth_bias": config.depth_bias,
        "on_top": config.on_top,
    })
}

/// The sub-toggles of group `name`, for groups that have them (`locomotion`).
fn toggles_json(store: &GizmoConfigStore, name: &str) -> Option<Value> {
    if name != <LocomotionGizmos as regia_render::gizmos::GizmoConfigGroup>::NAME {
        return None;
    }
    let (_, group) = store.config::<LocomotionGizmos>()?;
    let mut out = serde_json::Map::new();
    for toggle in LocomotionGizmos::TOGGLES {
        out.insert(toggle.to_owned(), json!(group.toggle(toggle).unwrap_or(false)));
    }
    Some(Value::Object(out))
}

/// Reads `toggles` (an object of sub-toggle name to boolean), checked against `group`.
fn parse_toggles(params: &Value, group: &str) -> Result<Option<Vec<(String, bool)>>, RpcError> {
    let Some(value) = params.get("toggles") else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Err(RpcError::invalid_params(
            "`toggles` must be an object of sub-toggle name to boolean",
        ));
    };
    if group != <LocomotionGizmos as regia_render::gizmos::GizmoConfigGroup>::NAME {
        return Err(RpcError::invalid_params(format!(
            "gizmo group `{group}` has no sub-toggles (`locomotion` has {})",
            LocomotionGizmos::TOGGLES.join(", ")
        )));
    }
    let mut out = Vec::with_capacity(object.len());
    for (name, on) in object {
        if !LocomotionGizmos::TOGGLES.contains(&name.as_str()) {
            return Err(RpcError::invalid_params(format!(
                "unknown `locomotion` sub-toggle `{name}` (toggles: {})",
                LocomotionGizmos::TOGGLES.join(", ")
            )));
        }
        let Some(on) = on.as_bool() else {
            return Err(RpcError::invalid_params(format!(
                "toggle `{name}` must be a boolean"
            )));
        };
        out.push((name.clone(), on));
    }
    Ok(Some(out))
}

/// `debug.gizmos`: every registered gizmo group and its configuration.
pub fn get(world: &mut World) -> Result<Value, RpcError> {
    let store = world.get_resource::<GizmoConfigStore>().ok_or_else(missing)?;
    let groups: Vec<Value> = store
        .iter()
        .into_iter()
        .map(|(name, config)| {
            let mut entry = config_json(name, config);
            if let Some(toggles) = toggles_json(store, name) {
                entry["toggles"] = toggles;
            }
            entry
        })
        .collect();
    Ok(json!({ "groups": groups }))
}

fn optional_bool(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    params
        .get(field)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean")))
        })
        .transpose()
}

fn optional_f32(
    params: &Value,
    field: &str,
    range: std::ops::RangeInclusive<f32>,
) -> Result<Option<f32>, RpcError> {
    params
        .get(field)
        .map(|v| {
            v.as_f64()
                .map(|x| x as f32)
                .filter(|x| range.contains(x))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`{field}` must be a number in {}..={}",
                        range.start(),
                        range.end()
                    ))
                })
        })
        .transpose()
}

/// `debug.set_gizmos`: enables/disables and restyles one gizmo group.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let group = crate::params::opt_str(params, "group")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`group` must be a gizmo group name (see debug.gizmos)"))?;
    let enabled = optional_bool(params, "enabled")?;
    let on_top = optional_bool(params, "on_top")?;
    let perspective = optional_bool(params, "perspective")?;
    let line_width = optional_f32(params, "line_width", 0.0..=64.0)?;
    let depth_bias = optional_f32(params, "depth_bias", -1.0..=1.0)?;
    let toggles = parse_toggles(params, group)?;
    if [enabled, on_top, perspective].iter().all(Option::is_none)
        && line_width.is_none()
        && depth_bias.is_none()
        && toggles.is_none()
    {
        return Err(RpcError::invalid_params(
            "pass at least one of `enabled`, `line_width`, `depth_bias`, `on_top`, `perspective`, `toggles`",
        ));
    }
    let mut store = world.get_resource_mut::<GizmoConfigStore>().ok_or_else(missing)?;
    let names: Vec<&'static str> = store.iter().into_iter().map(|(name, _)| name).collect();
    let Some(config) = store.config_by_name_mut(group) else {
        return Err(RpcError::not_found(
            format!("no gizmo group `{group}`"),
            format!("groups: {}", names.join(", ")),
        ));
    };
    let mut before = config_json(group, config);
    if let Some(on) = enabled {
        config.enabled = on;
    }
    if let Some(on) = on_top {
        config.on_top = on;
    }
    if let Some(on) = perspective {
        config.line.perspective = on;
    }
    if let Some(width) = line_width {
        config.line.width = width;
    }
    if let Some(bias) = depth_bias {
        config.depth_bias = bias;
    }
    let mut after = config_json(group, config);
    if let Some(toggles) = toggles_json(&store, group) {
        before["toggles"] = toggles;
    }
    if let Some(changes) = toggles
        && let Some((_, value)) = store.config_mut::<LocomotionGizmos>()
    {
        for (name, on) in changes {
            value.set_toggle(&name, on);
        }
    }
    if let Some(toggles) = toggles_json(&store, group) {
        after["toggles"] = toggles;
    }
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::gizmos::{DefaultGizmoConfigGroup, PhysicsGizmos};

    fn world() -> World {
        let mut store = GizmoConfigStore::default();
        store.register::<DefaultGizmoConfigGroup>();
        store.register::<PhysicsGizmos>();
        let mut world = World::new();
        world.insert_resource(store);
        world
    }

    #[test]
    fn set_toggles_and_styles_a_group() {
        let mut world = world();
        let out = set(
            &mut world,
            &json!({ "group": "physics", "enabled": true, "line_width": 3.5, "on_top": true }),
        )
        .expect("set");
        assert_eq!(out["before"]["enabled"], json!(false));
        assert_eq!(out["after"]["enabled"], json!(true));
        assert_eq!(out["after"]["line_width"], json!(3.5));
        assert_eq!(out["after"]["on_top"], json!(true));
        let listed = get(&mut world).expect("get");
        let physics = listed["groups"]
            .as_array()
            .and_then(|groups| groups.iter().find(|g| g["name"] == "physics"))
            .cloned()
            .expect("physics listed");
        assert_eq!(physics["enabled"], json!(true));
        assert_eq!(listed["groups"][0]["name"], json!("default"));
    }

    #[test]
    fn bad_params_and_unknown_groups_are_rejected() {
        let mut world = world();
        assert!(set(&mut world, &json!({ "enabled": true })).is_err());
        assert!(set(&mut world, &json!({ "group": "physics" })).is_err());
        assert!(set(&mut world, &json!({ "group": "physics", "enabled": "yes" })).is_err());
        assert!(set(&mut world, &json!({ "group": "physics", "depth_bias": 4.0 })).is_err());
        let err = set(&mut world, &json!({ "group": "nope", "enabled": true })).expect_err("unknown");
        assert!(
            format!("{err:?}").contains("physics"),
            "hint lists the groups: {err:?}"
        );
        let mut empty = World::new();
        assert!(get(&mut empty).is_err());
    }

    #[test]
    fn locomotion_toggles_are_listed_and_set() {
        let mut world = world();
        world
            .resource_mut::<GizmoConfigStore>()
            .register::<LocomotionGizmos>();
        let listed = get(&mut world).expect("get");
        let entry = |listed: &Value, name: &str| {
            listed["groups"]
                .as_array()
                .and_then(|groups| groups.iter().find(|g| g["name"] == name))
                .cloned()
                .expect("listed")
        };
        assert_eq!(
            entry(&listed, "locomotion")["toggles"],
            json!({ "gait": true, "foot_placement": true, "ik": true })
        );
        assert!(entry(&listed, "physics").get("toggles").is_none());
        let out = set(
            &mut world,
            &json!({ "group": "locomotion", "toggles": { "foot_placement": false } }),
        )
        .expect("set");
        assert_eq!(out["before"]["toggles"]["foot_placement"], json!(true));
        assert_eq!(out["after"]["toggles"]["foot_placement"], json!(false));
        assert_eq!(
            out["after"]["enabled"],
            json!(false),
            "toggles alone leave the group off"
        );
        assert!(
            set(
                &mut world,
                &json!({ "group": "locomotion", "toggles": { "x": true } })
            )
            .is_err()
        );
        assert!(
            set(
                &mut world,
                &json!({ "group": "physics", "toggles": { "ik": true } })
            )
            .is_err()
        );
        let still = get(&mut world).expect("get");
        assert_eq!(
            entry(&still, "locomotion")["toggles"]["foot_placement"],
            json!(false)
        );
    }
}
