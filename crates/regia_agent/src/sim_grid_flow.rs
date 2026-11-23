// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for deterministic flow fields (ADR-8839): `sim.grid_flow`.
//!
//! The grid lives in the host's lockstep sim; the host publishes it into
//! [`regia_nav::det_view::DetFlowView`] and this method builds the flow field toward the goal an
//! agent names (or reuses the last one) and answers a capped window of it: per cell the direction
//! byte and the integration value. Without the resource, or before the first publish, it answers
//! `no_det_flow`. See book/agents/det-grid.md ("The agent surface: `sim.grid_flow`").

use regia_app::prelude::*;
use regia_nav::det::flow::{CellRect, DIR_MASK, UNREACHED};
use regia_nav::det::grid::{NavGrid, Profile};
use regia_nav::det_view::{DetFlowView, FLOW_WINDOW_MAX_CELLS, FlowQueryError, FlowQueryKind};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

const METHOD: &str = "sim.grid_flow";

/// Most goal points one call may name.
const MAX_GOALS: usize = 64;

/// Side of the default window around the first goal, in cells.
const DEFAULT_WINDOW: u32 = 32;

fn error(code: i64, reason: &str, detail: &str, hint: &str) -> RpcError {
    RpcError::new(code, reason, format!("{METHOD}: {detail} [{reason}]"), hint)
}

fn no_det_flow() -> RpcError {
    error(
        NOT_FOUND,
        "no_det_flow",
        "this app has no published deterministic grid",
        "the host inserts regia_nav::det_view::DetFlowView and calls publish(tick, &grid) after each tick's \
         sync_stamps",
    )
}

fn bad_goal(detail: &str) -> RpcError {
    error(
        INVALID_PARAMS,
        "grid_flow_bad_goal",
        detail,
        "pass goal as a world point [x, z] or a list of up to 64 points, inside the grid",
    )
}

fn bad_window(detail: &str) -> RpcError {
    error(
        INVALID_PARAMS,
        "grid_flow_bad_window",
        detail,
        "pass window as { x0, z0, x1, z1 } in cells (half open, x1 > x0, z1 > z0), overlapping the grid",
    )
}

/// One `[x, z]` world point, finite.
fn point(value: &Value) -> Option<[f32; 2]> {
    let items = value.as_array()?;
    let [x, z] = items.as_slice() else {
        return None;
    };
    let (x, z) = (x.as_f64()?, z.as_f64()?);
    (x.is_finite() && z.is_finite()).then_some([x as f32, z as f32])
}

/// The goal cells of `goal`: one `[x, z]` point or a list of them.
fn goal_cells(grid: &NavGrid, params: &Value) -> Result<Vec<u32>, RpcError> {
    let value = params
        .get("goal")
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad_goal("`goal` is required"))?;
    let points: Vec<[f32; 2]> = match point(value) {
        Some(p) => vec![p],
        None => {
            let items = value
                .as_array()
                .ok_or_else(|| bad_goal("`goal` must be [x, z] or a list of [x, z]"))?;
            if items.is_empty() || items.len() > MAX_GOALS {
                return Err(bad_goal("`goal` lists 1 to 64 points"));
            }
            items
                .iter()
                .map(|item| {
                    point(item).ok_or_else(|| bad_goal("every goal point must be [x, z], two finite numbers"))
                })
                .collect::<Result<_, _>>()?
        }
    };
    points
        .iter()
        .map(|[x, z]| {
            if grid.in_bounds_world(*x, *z) {
                Ok(grid.cell_at(*x, *z))
            } else {
                Err(bad_goal(&format!("goal point [{x}, {z}] is off the grid")))
            }
        })
        .collect()
}

/// The window: `{ x0, z0, x1, z1 }` in cells, or a `DEFAULT_WINDOW` square around the first goal.
fn window(grid: &NavGrid, params: &Value, first_goal: u32) -> Result<CellRect, RpcError> {
    let Some(value) = params.get("window").filter(|v| !v.is_null()) else {
        let (gx, gz) = grid.xz(first_goal);
        let half = DEFAULT_WINDOW / 2;
        let (gx, gz) = (gx as u32, gz as u32);
        return Ok(CellRect {
            x0: gx.saturating_sub(half),
            z0: gz.saturating_sub(half),
            x1: gx.saturating_add(half),
            z1: gz.saturating_add(half),
        });
    };
    let map = value
        .as_object()
        .ok_or_else(|| bad_window("`window` must be an object { x0, z0, x1, z1 }"))?;
    if let Some(key) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "x0" | "z0" | "x1" | "z1"))
    {
        return Err(bad_window(&format!("`window` has no field `{key}`")));
    }
    let field = |key: &str| -> Result<u32, RpcError> {
        let refuse = || {
            bad_window(&format!(
                "`window.{key}` must be a cell index (an integer from 0)"
            ))
        };
        let raw = map.get(key).and_then(Value::as_u64).ok_or_else(refuse)?;
        u32::try_from(raw).map_err(|_| refuse())
    };
    let rect = CellRect {
        x0: field("x0")?,
        z0: field("z0")?,
        x1: field("x1")?,
        z1: field("z1")?,
    };
    if rect.x1 <= rect.x0 || rect.z1 <= rect.z0 {
        return Err(bad_window(
            "`window` is empty (x1 must be above x0 and z1 above z0)",
        ));
    }
    Ok(rect)
}

/// `sim.grid_flow { goal, profile?, window?, max_cells? }`: the flow field toward `goal` for
/// `profile` on the host's published grid, as a capped window of direction bytes and integration
/// values (ADR-8839).
pub fn grid_flow(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types(METHOD, params, &["goal"])?;
    let view = world.get_resource::<DetFlowView>().ok_or_else(no_det_flow)?;
    let grid = view.grid.clone().ok_or_else(no_det_flow)?;
    let profile = match crate::params::opt_u64(params, "profile")? {
        None => 0u8,
        Some(p) if (p as usize) < grid.settings().profiles.len() => p as u8,
        Some(p) => {
            return Err(error(
                INVALID_PARAMS,
                "grid_flow_bad_profile",
                &format!(
                    "profile {p} is not one of the grid's {} profiles",
                    grid.settings().profiles.len()
                ),
                "pass a profile index from 0 (GridSettings::profiles)",
            ));
        }
    };
    let cap = match crate::params::opt_u64(params, "max_cells")? {
        None => FLOW_WINDOW_MAX_CELLS,
        Some(n) if (1..=FLOW_WINDOW_MAX_CELLS as u64).contains(&n) => n as usize,
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "{METHOD}: `max_cells` must be 1 to 4096"
            )));
        }
    };
    let goals = goal_cells(&grid, params)?;
    let rect = window(&grid, params, goals[0])?;
    let answer = view
        .query(Profile(profile), &goals, rect, cap)
        .map_err(|e| match e {
            FlowQueryError::NoGrid => no_det_flow(),
            FlowQueryError::BadProfile => error(
                INVALID_PARAMS,
                "grid_flow_bad_profile",
                "the profile is not one of the grid's profiles",
                "pass a profile index from 0 (GridSettings::profiles)",
            ),
            FlowQueryError::BadGoal => bad_goal("a goal is off the grid"),
            FlowQueryError::GoalBlocked => error(
                INVALID_PARAMS,
                "grid_flow_goal_blocked",
                "every goal cell is blocked for the profile",
                "move the goal to a cell the profile can stand in (NavGrid::nearest_passable)",
            ),
            FlowQueryError::EmptyWindow => bad_window("the window does not overlap the grid"),
        })?;
    let v = &answer.view;
    let integration: Vec<Value> = v
        .integration
        .iter()
        .map(|&i| if i == UNREACHED { Value::Null } else { json!(i) })
        .collect();
    let reached = v.integration.iter().filter(|&&i| i != UNREACHED).count();
    let goal_xz: Vec<[usize; 2]> = answer
        .goals
        .iter()
        .map(|g| {
            let (x, z) = grid.xz(*g);
            [x, z]
        })
        .collect();
    let stats = answer
        .stats
        .map(|s| json!({ "expansions": s.expansions, "reached": s.reached, "los_cells": s.los_cells }));
    let cluster = answer
        .cluster_stats
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(json!({
        "tick": answer.tick,
        "generation": answer.generation,
        "kind": match answer.kind {
            FlowQueryKind::Full => "full",
            FlowQueryKind::Clustered => "clustered",
        },
        "profile": profile,
        "goals": goal_xz,
        "grid": { "width": grid.width(), "height": grid.height(), "cell_m": grid.cell_m() },
        "window": { "x0": v.rect.x0, "z0": v.rect.z0, "x1": v.rect.x1, "z1": v.rect.z1 },
        "truncated": v.truncated,
        "dirs": v.dirs,
        "integration": integration,
        "reached": reached,
        "dir_mask": DIR_MASK,
        "cached": answer.cached,
        "memory_bytes": answer.memory_bytes,
        "stats": stats,
        "bits_hash": answer.bits_hash.map(|h| format!("{h:016x}")),
        "cluster_stats": cluster,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_nav::det::flow::{DIR_GOAL, FlowField, FlowSettings};
    use regia_nav::det::grid::{GridSettings, NavStatic, Stamp};

    use super::*;

    fn grid() -> NavGrid {
        let settings = Arc::new(GridSettings {
            canvas_cells: [128, 128],
            ..GridSettings::hover_ground_jet()
        });
        let mut g = NavGrid::new(Arc::new(NavStatic::build(None, settings).expect("canvas"))).expect("grid");
        g.sync_stamps(vec![(1, Stamp::from_world(0.0, 0.0, 25.0))]);
        g
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        crate::dispatch::call_in_process(world, METHOD, params).expect("answers now")
    }

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    /// ADR-8839 through the dispatcher: without a published grid the method answers
    /// `no_det_flow`; with one it returns the window of the field toward the goal, equal to
    /// `FlowField::view` cell for cell (unreached cells as null), names the goal cell, and the
    /// second call on the same goal answers from the kept field.
    #[test]
    fn grid_flow_answers_a_window_of_the_published_grid() {
        let mut world = World::new();
        let missing = call(&mut world, json!({ "goal": [100.0, 50.0] })).expect_err("no view");
        assert_eq!(code(&missing), Some("no_det_flow"));
        world.insert_resource(DetFlowView::default());
        let empty = call(&mut world, json!({ "goal": [100.0, 50.0] })).expect_err("no grid yet");
        assert_eq!(code(&empty), Some("no_det_flow"));

        let g = grid();
        world.resource_mut::<DetFlowView>().publish(5, &g);
        let out = call(
            &mut world,
            json!({ "goal": [100.0, 50.0], "profile": 0, "window": { "x0": 40, "z0": 40, "x1": 90, "z1": 90 } }),
        )
        .expect("a field");
        crate::contract::assert_result_named(METHOD, &out);
        let goal = g.cell_at(100.0, 50.0);
        let reference = FlowField::build(&g, Profile(0), &[goal], FlowSettings::default());
        let rect = CellRect {
            x0: 40,
            z0: 40,
            x1: 90,
            z1: 90,
        };
        let want = reference.view(rect, FLOW_WINDOW_MAX_CELLS);
        assert_eq!(out["tick"], 5);
        assert_eq!(out["kind"], "full");
        assert_eq!(out["cached"], false);
        assert_eq!(out["truncated"], want.truncated);
        assert_eq!(out["window"]["x0"], want.rect.x0);
        assert_eq!(out["window"]["z1"], want.rect.z1);
        assert_eq!(out["dirs"], json!(want.dirs));
        let ints: Vec<Option<u32>> = want
            .integration
            .iter()
            .map(|&i| (i != UNREACHED).then_some(i))
            .collect();
        assert_eq!(out["integration"], json!(ints));
        assert!(
            ints.iter().any(Option::is_none),
            "the stamp's cells are unreached"
        );
        let (gx, gz) = g.xz(goal);
        assert_eq!(out["goals"], json!([[gx, gz]]));
        assert_eq!(out["bits_hash"], json!(format!("{:016x}", reference.bits_hash())));
        assert_eq!(out["grid"]["width"], 128);

        // The default window: 32 x 32 around the goal, with the goal's byte DIR_GOAL.
        let near = call(&mut world, json!({ "goal": [100.0, 50.0] })).expect("a field");
        assert_eq!(near["cached"], true, "same goal and profile: the kept field");
        let (x0, z0) = (
            near["window"]["x0"].as_u64().expect("x0") as usize,
            near["window"]["z0"].as_u64().expect("z0") as usize,
        );
        let w = (near["window"]["x1"].as_u64().expect("x1") as usize) - x0;
        let at = (gz - z0) * w + (gx - x0);
        assert_eq!(
            near["dirs"][at].as_u64().map(|d| d as u8 & DIR_MASK),
            Some(DIR_GOAL)
        );
        assert_eq!(near["integration"][at], 0);

        // The book page's example (printed for the page; run with --nocapture).
        let example = call(
            &mut world,
            json!({ "goal": [100.0, 50.0], "profile": 0, "window": { "x0": 40, "z0": 40, "x1": 42, "z1": 41 } }),
        )
        .expect("a field");
        println!("sim.grid_flow example: {example}");
        assert_eq!(example["dirs"].as_array().map(Vec::len), Some(2));

        // The cap.
        let few = call(&mut world, json!({ "goal": [100.0, 50.0], "max_cells": 10 })).expect("capped");
        assert_eq!(few["truncated"], true);
        assert!(few["dirs"].as_array().is_some_and(|d| d.len() <= 10));
    }

    /// ADR-8839: a host's field drawn through the framework's `DetDebugDraw` reaches
    /// `sim.det_debug_draw` as `flow` primitives (one circle on the goal, three lines per other
    /// arrow), only while the switch holds `flow`.
    #[test]
    fn flow_arrows_reach_the_det_debug_draw_method() {
        use regia_framework::det_debug::{DebugWhat, DetDebugDraw};
        use regia_nav::det::flow::FlowDebug;

        let g = grid();
        let goal = g.cell_at(100.0, 50.0);
        let field = FlowField::build(&g, Profile(0), &[goal], FlowSettings::default());
        let (gx, gz) = g.xz(goal);
        let rect = CellRect {
            x0: gx as u32 - 4,
            z0: gz as u32 - 4,
            x1: gx as u32 + 4,
            z1: gz as u32 + 4,
        };
        let mut world = World::new();
        let mut draw = DetDebugDraw::new(DebugWhat::FLOW);
        let mut source = FlowDebug {
            rect,
            ..FlowDebug::new(&field)
        };
        draw.record(3, &mut [&mut source]);
        world.insert_resource(draw);
        let out = crate::dispatch::call_in_process(&mut world, "sim.det_debug_draw", json!({}))
            .expect("answers now")
            .expect("a frame");
        assert_eq!(out["what"], json!(["flow"]));
        assert_eq!(out["tick"], 3);
        assert_eq!(out["kinds"]["flow"], json!(1 + 3 * 63));
        assert_eq!(out["primitives"]["circles"], 1);
        assert_eq!(out["kinds"]["contact"], 0);
        crate::contract::assert_result_named("sim.det_debug_draw", &out);

        let mut off = DetDebugDraw::new(DebugWhat::GRID);
        off.record(4, &mut [&mut source]);
        assert_eq!(off.frame.len(), 0, "no flow part, no arrows");
    }

    /// Every refusal names its code: a missing or malformed goal, a goal off the grid, a profile
    /// past the grid's list, a blocked goal, a bad or off-grid window, an unknown parameter.
    #[test]
    fn grid_flow_refusals_name_their_codes() {
        let mut world = World::new();
        world.insert_resource(DetFlowView::default());
        let g = grid();
        world.resource_mut::<DetFlowView>().publish(1, &g);
        let cases = [
            (json!({}), "grid_flow_bad_goal"),
            (json!({ "goal": [1.0] }), "grid_flow_bad_goal"),
            (json!({ "goal": [[1.0, 2.0], "x"] }), "grid_flow_bad_goal"),
            (json!({ "goal": [] }), "grid_flow_bad_goal"),
            (json!({ "goal": [9000.0, 0.0] }), "grid_flow_bad_goal"),
            (
                json!({ "goal": [100.0, 50.0], "profile": 99 }),
                "grid_flow_bad_profile",
            ),
            (json!({ "goal": [0.0, 0.0] }), "grid_flow_goal_blocked"),
            (
                json!({ "goal": [100.0, 50.0], "window": { "x0": 5, "z0": 5, "x1": 5, "z1": 9 } }),
                "grid_flow_bad_window",
            ),
            (
                json!({ "goal": [100.0, 50.0], "window": { "x0": 500, "z0": 0, "x1": 600, "z1": 9 } }),
                "grid_flow_bad_window",
            ),
            (
                json!({ "goal": [100.0, 50.0], "window": { "x0": -1, "z0": 0, "x1": 6, "z1": 9 } }),
                "grid_flow_bad_window",
            ),
            (
                json!({ "goal": [100.0, 50.0], "window": { "x0": 0, "z0": 0, "x1": 6, "z1": 9, "w": 1 } }),
                "grid_flow_bad_window",
            ),
        ];
        for (params, want) in cases {
            let err = call(&mut world, params.clone()).expect_err("refused");
            assert_eq!(code(&err), Some(want), "{params}: {}", err.message);
            assert!(err.message.contains(&format!("[{want}]")), "{}", err.message);
        }
        let unknown = call(&mut world, json!({ "goal": [100.0, 50.0], "bogus": 1 })).expect_err("refused");
        assert_eq!(code(&unknown), Some("F0002"));
        let typed =
            call(&mut world, json!({ "goal": [100.0, 50.0], "profile": "hover" })).expect_err("refused");
        assert_eq!(code(&typed), Some("F0002"));
    }
}
