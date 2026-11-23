// SPDX-License-Identifier: MIT-0
//! The armed tunnels' agent methods (`book/agents/showcase-tunnels.md`, ADR-8825), registered on the world's
//! `AgentExtension` at startup: `tunnels.state` (what the load cooked and armed, the view pairs, the playing
//! view) and `tunnels.view` (play one view of the scene's `perf_views.json`: an inside-tube view or its
//! matching outdoor view). A refusal carries a lowercase code in `data.code` (`CODES`).

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_framework::shots::ShotPlayer;
use serde_json::{Value, json};

use crate::app::ShowcaseScene;
use crate::perf_table::{View, parse_views};

/// The capability marker `agent.hello` lists when the methods are registered.
pub const CAPABILITY: &str = "showcase_tunnels";

/// Every method registered, in the book page's order.
pub const METHODS: [&str; 2] = ["tunnels.state", "tunnels.view"];

/// Every refusal code, in the book page's order.
pub const CODES: [&str; 4] = ["tunnels_off", "no_views", "unknown_view", "bad_view_params"];

/// The views file of the scene directory (the perf table's, [`crate::perf_table`]).
pub const VIEWS_FILE: &str = "perf_views.json";

/// Prefix of an inside-tube view's name; the matching outdoor view is `outdoor_<pair>`.
pub const TUBE_PREFIX: &str = "tube_";
/// Prefix of an outdoor view's name.
pub const OUTDOOR_PREFIX: &str = "outdoor_";

/// The scene's views (read once at startup) and the view `tunnels.view` played last.
#[derive(Resource, Clone, Debug, Default)]
pub struct TunnelViews {
    /// The views of `perf_views.json`, in file order.
    pub views: Vec<View>,
    /// Why the views file was not read (`None`: read, or absent).
    pub error: Option<String>,
    /// The view played last.
    pub playing: Option<String>,
}

impl TunnelViews {
    /// The view pairs: `(pair, tube view, outdoor view)` for every `tube_<pair>` with an `outdoor_<pair>`.
    #[must_use]
    pub fn pairs(&self) -> Vec<(String, String, String)> {
        self.views
            .iter()
            .filter_map(|v| v.name.strip_prefix(TUBE_PREFIX))
            .filter(|pair| self.view(&format!("{OUTDOOR_PREFIX}{pair}")).is_some())
            .map(|pair| {
                (
                    pair.to_owned(),
                    format!("{TUBE_PREFIX}{pair}"),
                    format!("{OUTDOOR_PREFIX}{pair}"),
                )
            })
            .collect()
    }

    /// The view named `name`.
    #[must_use]
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|v| v.name == name)
    }
}

fn schemas() -> Vec<(MethodSchema, regia_agent::extension::AgentHandler)> {
    vec![
        (
            MethodSchema {
                name: "tunnels.state",
                tool: "tunnels_state",
                description: "The showcase scene's armed tunnels (ADR-8825): the cooked mouths and whether the terrain draws the lattice cut, the tube meshes, triangles and lights, the indoor cells and portals, the sim-geometry and cooked bytes, the scene's views and view pairs, and the view tunnels.view played last.",
                params: &[],
                result: "{ armed, document, mouths: [{ id, rings, ring_points, cut_triangles, centre }], lattice_cut, meshes, triangles, lights, cells, portals, sim_bytes, cooked_bytes, views: [name], pairs: [{ pair, tube, outdoor }], playing?, views_error? }",
                mutating: false,
                permission: Permission::Read,
            },
            tunnels_state,
        ),
        (
            MethodSchema {
                name: "tunnels.view",
                tool: "tunnels_view",
                description: "Play one view of the scene's perf_views.json on the camera (a one-shot camera path, as camera.set_path with play): by `name`, or by `pair` and `side` (`tube` is the inside-tube view `tube_<pair>`, `outdoor` its matching outdoor view `outdoor_<pair>`). Start a render.out_start job after it to film or time the view (play.hitch_report during the job gives the per-pass GPU times).",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "a view name of perf_views.json (instead of pair and side)",
                    },
                    ParamSchema {
                        name: "pair",
                        kind: "string",
                        required: false,
                        description: "a view pair (tunnels.state pairs[].pair)",
                    },
                    ParamSchema {
                        name: "side",
                        kind: "string",
                        required: false,
                        description: "`tube` or `outdoor` (with pair)",
                    },
                    ParamSchema {
                        name: "play",
                        kind: "boolean",
                        required: false,
                        description: "start the path at once (default true); false loads it held at its first key",
                    },
                ],
                result: "{ view, pair?, side?, seconds, playing }",
                mutating: true,
                permission: Permission::EditScene,
            },
            tunnels_view,
        ),
    ]
}

/// `Startup`: reads the scene's views and registers the methods and the capability on the `AgentExtension`.
pub fn install(world: &mut World) {
    let views = world
        .get_resource::<ShowcaseScene>()
        .map(|scene| scene.0.dir.join(VIEWS_FILE));
    let mut state = TunnelViews::default();
    if let Some(path) = views.filter(|p| p.is_file()) {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_views(&text))
        {
            Ok(views) => state.views = views,
            Err(error) => state.error = Some(format!("{}: {error}", path.display())),
        }
    }
    world.insert_resource(state);
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    for (schema, handler) in schemas() {
        extension.register(schema, handler);
    }
    extension.add_capability(
        CAPABILITY,
        "armed tunnels in a showcase scene (tunnels.* methods, book agents/showcase-tunnels)",
    );
    world.insert_resource(extension);
}

fn refuse(kind: i64, code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        kind,
        code,
        message,
        "see tunnels.state and book/agents/showcase-tunnels.md",
    )
}

fn armed(world: &World) -> Result<std::sync::Arc<crate::tunnels::TunnelsPlan>, RpcError> {
    world
        .get_resource::<ShowcaseScene>()
        .and_then(|scene| scene.0.features.tunnels.clone())
        .ok_or_else(|| {
            refuse(
                NOT_FOUND,
                "tunnels_off",
                "the scene has no armed tunnels (no tunnels/tunnels.ron)",
            )
        })
}

fn tunnels_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let plan = armed(world)?;
    let views = world.get_resource::<TunnelViews>().cloned().unwrap_or_default();
    let mouths: Vec<Value> = plan
        .mouths
        .iter()
        .map(|m| {
            json!({
                "id": m.id, "rings": m.rings, "ring_points": m.ring_points,
                "cut_triangles": m.cut_triangles, "centre": m.centre,
            })
        })
        .collect();
    let pairs: Vec<Value> = views
        .pairs()
        .into_iter()
        .map(|(pair, tube, outdoor)| json!({ "pair": pair, "tube": tube, "outdoor": outdoor }))
        .collect();
    let mut out = json!({
        "armed": true,
        "document": plan.document.display().to_string(),
        "mouths": mouths,
        "lattice_cut": plan.lattice.is_some(),
        "meshes": plan.meshes.len(),
        "triangles": plan.triangles,
        "lights": plan.lights.len(),
        "cells": plan.viscells.cells.len(),
        "portals": plan.viscells.portals.len(),
        "sim_bytes": plan.sim_bytes,
        "cooked_bytes": plan.cooked_bytes,
        "views": views.views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        "pairs": pairs,
    });
    if let Some(playing) = &views.playing {
        out["playing"] = json!(playing);
    }
    if let Some(error) = &views.error {
        out["views_error"] = json!(error);
    }
    Ok(out)
}

fn string_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(refuse(
            INVALID_PARAMS,
            "bad_view_params",
            format!("`{key}` must be a string"),
        )),
    }
}

/// The view `params` names: `name`, or `pair` + `side`. Returns `(view, pair, side)`.
fn pick(params: &Value, views: &TunnelViews) -> Result<(String, Option<String>, Option<String>), RpcError> {
    let name = string_param(params, "name")?;
    let pair = string_param(params, "pair")?;
    let side = string_param(params, "side")?;
    let (view, pair, side) = match (name, pair, side) {
        (Some(name), None, None) => (name.to_owned(), None, None),
        (None, Some(pair), Some(side)) => {
            let prefix = match side {
                "tube" => TUBE_PREFIX,
                "outdoor" => OUTDOOR_PREFIX,
                other => {
                    return Err(refuse(
                        INVALID_PARAMS,
                        "bad_view_params",
                        format!("`side` is `tube` or `outdoor`, not `{other}`"),
                    ));
                }
            };
            (
                format!("{prefix}{pair}"),
                Some(pair.to_owned()),
                Some(side.to_owned()),
            )
        }
        _ => {
            return Err(refuse(
                INVALID_PARAMS,
                "bad_view_params",
                "give `name`, or `pair` and `side`",
            ));
        }
    };
    if views.views.is_empty() {
        return Err(refuse(
            NOT_FOUND,
            "no_views",
            views
                .error
                .clone()
                .unwrap_or_else(|| format!("the scene has no {VIEWS_FILE}")),
        ));
    }
    if views.view(&view).is_none() {
        let names: Vec<&str> = views.views.iter().map(|v| v.name.as_str()).collect();
        return Err(refuse(
            NOT_FOUND,
            "unknown_view",
            format!("no view `{view}`; the scene has: {}", names.join(", ")),
        ));
    }
    Ok((view, pair, side))
}

fn tunnels_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    armed(world)?;
    let play = match params.get("play") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(refuse(
                INVALID_PARAMS,
                "bad_view_params",
                "`play` must be a boolean",
            ));
        }
    };
    let views = world.get_resource::<TunnelViews>().cloned().unwrap_or_default();
    let (name, pair, side) = pick(params, &views)?;
    let view = views
        .view(&name)
        .cloned()
        .ok_or_else(|| refuse(NOT_FOUND, "unknown_view", format!("no view `{name}`")))?;
    let sequence =
        crate::demo::bench::view_sequence(&view).map_err(|e| refuse(INVALID_PARAMS, "bad_view_params", e))?;
    world.init_resource::<ShotPlayer>();
    {
        let mut player = world.resource_mut::<ShotPlayer>();
        player.load(sequence);
        if play {
            player.play();
        }
    }
    if let Some(mut state) = world.get_resource_mut::<TunnelViews>() {
        state.playing = Some(name.clone());
    }
    let mut out = json!({ "view": name, "seconds": view.seconds, "playing": play });
    if let (Some(pair), Some(side)) = (pair, side) {
        out["pair"] = json!(pair);
        out["side"] = json!(side);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views(names: &[&str]) -> TunnelViews {
        let list: Vec<Value> = names
            .iter()
            .map(|n| {
                json!({ "name": n, "path": { "interp": "Linear", "keys": [
                    { "time": 0, "position": [0, 1, 0] }, { "time": 2, "position": [0, 1, 4] }],
                    "look_at": { "Point": [0, 1, 10] } } })
            })
            .collect();
        let text = json!({ "version": 1, "views": list }).to_string();
        TunnelViews {
            views: parse_views(&text).unwrap_or_default(),
            ..TunnelViews::default()
        }
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn every_method_is_registered_with_the_capability() {
        let mut world = World::new();
        install(&mut world);
        let extension = world.resource::<AgentExtension>();
        let names = extension.method_names();
        for method in METHODS {
            assert!(names.contains(&method), "{method} registered");
        }
        for (schema, _) in schemas() {
            assert_eq!(schema.tool, schema.name.replace('.', "_"));
            assert!(METHODS.contains(&schema.name));
        }
        assert!(extension.capabilities().iter().any(|(c, _)| *c == CAPABILITY));
        for code in CODES {
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn pairs_need_both_sides() {
        let v = views(&["tube_west", "outdoor_west", "tube_east", "mouth_drive"]);
        assert_eq!(
            v.pairs(),
            vec![(
                "west".to_owned(),
                "tube_west".to_owned(),
                "outdoor_west".to_owned()
            )]
        );
    }

    #[test]
    fn a_view_is_picked_by_name_or_by_pair_and_side() {
        let v = views(&["tube_west", "outdoor_west"]);
        let by_side = pick(&json!({ "pair": "west", "side": "outdoor" }), &v).map(|p| p.0);
        assert_eq!(by_side.ok().as_deref(), Some("outdoor_west"));
        let by_name = pick(&json!({ "name": "tube_west" }), &v).map(|p| p.0);
        assert_eq!(by_name.ok().as_deref(), Some("tube_west"));
        let unknown = pick(&json!({ "name": "tube_east" }), &v).err();
        assert_eq!(unknown.as_ref().and_then(code), Some("unknown_view"));
        let both = pick(&json!({ "name": "tube_west", "pair": "west" }), &v).err();
        assert_eq!(both.as_ref().and_then(code), Some("bad_view_params"));
        let side = pick(&json!({ "pair": "west", "side": "roof" }), &v).err();
        assert_eq!(side.as_ref().and_then(code), Some("bad_view_params"));
        let none = pick(&json!({ "name": "tube_west" }), &TunnelViews::default()).err();
        assert_eq!(none.as_ref().and_then(code), Some("no_views"));
    }

    #[test]
    fn a_scene_without_tunnels_is_refused() {
        let mut world = World::new();
        install(&mut world);
        let err = tunnels_state(&mut world, &json!({})).err();
        assert_eq!(err.as_ref().and_then(code), Some("tunnels_off"));
        let err = tunnels_view(&mut world, &json!({ "name": "tube_west" })).err();
        assert_eq!(err.as_ref().and_then(code), Some("tunnels_off"));
    }
}
