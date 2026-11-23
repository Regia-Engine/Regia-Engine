// SPDX-License-Identifier: MPL-2.0
//! `procgen.tiles*` and `procgen.anchors`: tiled and anchored runtime generation (specs/66 § 6, book
//! `agents/procgen.md` section "Tiles and anchors").
//!
//! Errors answer `F0002` (invalid params) or `F0003` (unknown entity, tier or cell) with
//! `data.reason` set to a lowercase code listed on the book page.

use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use regia_ecs::prelude::*;
use regia_procgen::debug_view::{ProcGenDebug, ProcGenGizmos};
use regia_procgen::tiles::{
    AnchorSource, AnchorView, CellCoord, ProcGenExtraAnchors, ProcGenTileHost, ProcGenTiles, TierReach,
    TileKey, TileState, gather_anchors, gather_source_anchors, rebuild_cells,
};
use regia_procgen::world::{ProcGenSettings, ProcGenerator, request_run};
use regia_render::gizmos::{GizmoConfigGroup, GizmoConfigStore};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, Response, RpcError};

/// Most pending `procgen.tiles_wait` calls (the same cap as `play.wait_until`).
pub(crate) const MAX_TILE_WAITS: usize = 64;

fn reason(code: i64, regia_code: &str, why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": why, "hint": "see book/agents/procgen.md" })),
    }
}

fn bad(why: &str, message: impl Into<String>) -> RpcError {
    reason(INVALID_PARAMS, "F0002", why, message)
}

fn missing(why: &str, message: impl Into<String>) -> RpcError {
    reason(NOT_FOUND, "F0003", why, message)
}

fn available(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<ProcGenSettings>() && world.contains_resource::<ProcGenTiles>() {
        Ok(())
    } else {
        Err(missing(
            "procgen_unavailable",
            "procedural generation is not installed (ProcGenPlugin)",
        ))
    }
}

/// The tiled generator named by `entity`.
fn tiled(world: &World, params: &Value) -> Result<(Entity, ProcGenerator), RpcError> {
    available(world)?;
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("procgen_not_generator", "`entity` (an entity id) is required"))?;
    let e = Entity::try_from_bits(bits)
        .ok_or_else(|| bad("procgen_not_generator", format!("{bits} is not an entity id")))?;
    let g = world.get::<ProcGenerator>(e).cloned().ok_or_else(|| {
        missing(
            "procgen_not_generator",
            format!("entity {bits} has no ProcGenerator"),
        )
    })?;
    if g.tiles.is_none() {
        return Err(bad(
            "procgen_not_tiled",
            format!("entity {bits} has no tile settings (ProcGenerator::tiles)"),
        ));
    }
    Ok((e, g))
}

/// The tiers of a generator: `(index, cell_m)` (`None` = whole).
fn tiers_of(world: &World, e: Entity) -> Vec<(u8, Option<f64>)> {
    if let Some(h) = world.get::<ProcGenTileHost>(e) {
        return h.plan().tiers.iter().map(|t| (t.index, t.cell_m)).collect();
    }
    world
        .resource::<ProcGenTiles>()
        .generators
        .get(&e)
        .map(|g| g.tiers.iter().map(|t| (t.index, t.cell_m)).collect())
        .unwrap_or_default()
}

pub(crate) fn tier_param(world: &World, e: Entity, params: &Value) -> Result<Option<u8>, RpcError> {
    let Some(v) = params.get("tier").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let t = v
        .as_u64()
        .and_then(|t| u8::try_from(t).ok())
        .ok_or_else(|| bad("procgen_tier_unknown", "`tier` is a tier index (0 = coarsest)"))?;
    if !tiers_of(world, e).iter().any(|(i, _)| *i == t) {
        return Err(missing(
            "procgen_tier_unknown",
            format!("the generator has no tier {t}"),
        ));
    }
    Ok(Some(t))
}

/// A tier's reach as JSON, with the values that take effect (`release_factor` is clamped to its
/// hysteresis minimum where it is read; audit L12).
fn reach_json(r: &TierReach) -> Value {
    json!({ "build_m": r.build_m, "release_factor": r.release_factor(), "view_bias": r.view_bias })
}

/// Validation warnings of a scheduled generator's anchor settings against its tiers (spec 66 § 4.2:
/// `tier_reach_order` when a finer tier builds farther than a coarser one), as
/// `[{ reason, message }]`. Empty until the scheduler has planned the generator.
fn reach_warnings(world: &World, e: Entity, g: &ProcGenerator) -> Vec<Value> {
    let (Some(host), Some(runtime)) = (
        world.get::<ProcGenTileHost>(e),
        g.tiles.as_ref().and_then(|t| t.runtime.as_ref()),
    ) else {
        return Vec::new();
    };
    host.plan()
        .check_reach(runtime)
        .into_iter()
        .map(|(code, message)| json!({ "reason": code, "message": message }))
        .collect()
}

/// `procgen.tiles { entity, tier?, state? }`.
pub(crate) fn tiles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (e, g) = tiled(world, params)?;
    let tier = tier_param(world, e, params)?;
    let state = match crate::params::opt_str(params, "state")? {
        Some(s) => Some(TileState::from_name(s).ok_or_else(|| {
            bad(
                "procgen_unknown_state",
                format!("`{s}` is not a cell state (queued, capturing, running, applying, resident, releasing, failed)"),
            )
        })?),
        None => None,
    };
    let tiers: Vec<Value> = tiers_of(world, e)
        .into_iter()
        .map(|(i, c)| json!({ "index": i, "cell_m": c }))
        .collect();
    let pool_cells = g
        .tiles
        .as_ref()
        .and_then(|t| t.runtime.as_ref())
        .map_or(0, |r| r.pool_cells);
    let tiles = world.resource::<ProcGenTiles>();
    let (cells, pool, memory) = match tiles.generators.get(&e) {
        Some(gt) => {
            let cells: Vec<Value> = gt
                .entries
                .iter()
                .filter(|(k, _)| tier.is_none_or(|t| k.tier == t))
                .filter(|(_, en)| state.is_none_or(|s| en.state == s))
                .map(|(k, en)| {
                    json!({
                        "i": k.cell.x, "j": k.cell.z, "tier": k.tier, "state": en.state.name(),
                        "instances": en.elements, "ms": en.ms, "mb": en.bytes as f64 / 1_048_576.0,
                        "priority": en.priority,
                    })
                })
                .collect();
            let used = gt
                .entries
                .values()
                .filter(|en| matches!(en.state, TileState::Resident | TileState::Applying))
                .count();
            (
                cells,
                json!({ "free": gt.pool_free, "used": used, "cells": pool_cells }),
                gt.resident_bytes() as f64 / 1_048_576.0,
            )
        }
        None => (
            Vec::new(),
            json!({ "free": 0, "used": 0, "cells": pool_cells }),
            0.0,
        ),
    };
    let error = tiles
        .errors
        .get(&e)
        .map(|(code, message)| json!({ "reason": code, "message": message }));
    let warnings = reach_warnings(world, e, &g);
    Ok(json!({
        "entity": e.to_bits(),
        "error": error,
        "warnings": warnings,
        "cell_m": g.tiles.as_ref().map(|t| t.cell_m),
        "tiers": tiers,
        "cells": cells,
        "pool": pool,
        "memory_mb": memory,
        "cells_loaded": world
            .resource::<ProcGenTiles>()
            .generators
            .get(&e)
            .map_or(0, |gt| gt.counters.cells_loaded),
    }))
}

fn anchor_json(a: &AnchorView) -> Value {
    json!({
        "position": [a.pos.x, a.pos.y, a.pos.z],
        "view_dir": a.view_dir.map(|v| [v.x, v.y, v.z]),
        "reach_scale": a.reach_scale,
    })
}

/// `procgen.anchors {}`: the primary view, every `ProcGenAnchor` entity, and any extra anchors a
/// host supplies (`ProcGenExtraAnchors`), each under its own key.
pub(crate) fn anchors(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let primary = gather_source_anchors(world, AnchorSource::PrimaryView);
    let tagged = gather_source_anchors(world, AnchorSource::Tagged);
    let extra = world
        .get_resource::<ProcGenExtraAnchors>()
        .map(|e| e.0.iter().map(anchor_json).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({
        "primary_view": primary.first().map(anchor_json),
        "tagged": tagged.iter().map(anchor_json).collect::<Vec<_>>(),
        "extra": extra,
    }))
}

/// `procgen.tiles_set { entity, tier, build_m?, release_factor?, view_bias? }`: live tuning.
pub(crate) fn tiles_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (e, g) = tiled(world, params)?;
    let tier =
        tier_param(world, e, params)?.ok_or_else(|| bad("procgen_tier_unknown", "`tier` is required"))?;
    let cell_m = tiers_of(world, e)
        .into_iter()
        .find(|(i, _)| *i == tier)
        .and_then(|(_, c)| c);
    let num = |k: &str| -> Result<Option<f32>, RpcError> {
        match params.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|x| x.is_finite() && *x >= 0.0 && (*x as f32).is_finite())
                .map(|x| Some(x as f32))
                .ok_or_else(|| {
                    bad(
                        "procgen_param_type",
                        format!("`{k}` must be a finite number ≥ 0 within f32 range"),
                    )
                }),
        }
    };
    let (build, release, bias) = (num("build_m")?, num("release_factor")?, num("view_bias")?);
    let Some(mut runtime) = g.tiles.as_ref().and_then(|t| t.runtime.clone()) else {
        return Err(bad(
            "procgen_not_tiled",
            "the generator has no runtime anchor settings (TileSettings::runtime)",
        ));
    };
    let before = runtime.reach_for(cell_m);
    let mut after = before;
    after.cell_m = cell_m.unwrap_or(0.0) as f32;
    if let Some(v) = build {
        after.build_m = v;
    }
    if let Some(v) = release {
        after.release_factor = v;
    }
    if let Some(v) = bias {
        after.view_bias = v.min(1.0);
    }
    match runtime
        .tiers
        .iter_mut()
        .find(|t| cell_m.is_some_and(|c| (f64::from(t.cell_m) - c).abs() <= 1e-6 * c))
    {
        Some(t) => *t = after,
        None if cell_m.is_some() => runtime.tiers.push(after),
        None => runtime.base_reach_m = after.build_m,
    }
    if let Some(mut gen_c) = world.get_mut::<ProcGenerator>(e)
        && let Some(t) = gen_c.tiles.as_mut()
    {
        t.runtime = Some(runtime);
    }
    Ok(json!({ "before": reach_json(&before), "after": reach_json(&after) }))
}

/// The optional `cells` list of `[i, j]` pairs.
pub(crate) fn cells_param(params: &Value) -> Result<Option<Vec<CellCoord>>, RpcError> {
    match params.get("cells") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|c| {
                let pair = c.as_array().filter(|a| a.len() == 2);
                match pair.map(|a| (a[0].as_i64(), a[1].as_i64())) {
                    Some((Some(i), Some(j))) => Ok(CellCoord::new(i, j)),
                    _ => Err(bad("procgen_unknown_cell", "`cells` is a list of [i, j] pairs")),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(bad("procgen_unknown_cell", "`cells` is a list of [i, j] pairs")),
    }
}

/// `procgen.set_debug { tiles?, regions? }`: toggles the runtime overlays and the `"procgen"` gizmo
/// group that draws them.
pub(crate) fn set_debug(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    available(world)?;
    let flag = |name: &str| match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad("procgen_param_type", format!("`{name}` must be a boolean"))),
    };
    let (tiles, regions) = (flag("tiles")?, flag("regions")?);
    let before = world.get_resource::<ProcGenDebug>().copied().unwrap_or_default();
    let after = ProcGenDebug {
        tiles: tiles.unwrap_or(before.tiles),
        regions: regions.unwrap_or(before.regions),
    };
    world.insert_resource(after);
    let drawn = world
        .get_resource_mut::<GizmoConfigStore>()
        .is_some_and(|mut store| store.set_enabled(ProcGenGizmos::NAME, after.tiles || after.regions));
    let json_of = |d: ProcGenDebug| json!({ "tiles": d.tiles, "regions": d.regions });
    Ok(json!({ "before": json_of(before), "after": json_of(after), "drawn": drawn }))
}

/// `procgen.tiles_rebuild { entity, cells?, tier? }`: re-runs the named cells (or all resident).
pub(crate) fn tiles_rebuild(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (e, g) = tiled(world, params)?;
    let tier = tier_param(world, e, params)?;
    let cells = cells_param(params)?;
    if !regia_procgen::tiles::is_scheduled(&g) {
        // Baked / on-load tiled generators run whole; a rebuild is a run.
        request_run(world, e);
        return Ok(json!({ "requeued": [], "run_requested": true }));
    }
    if let Some(cs) = &cells {
        let tiles = world.resource::<ProcGenTiles>();
        let known = |c: &CellCoord| {
            tiles.generators.get(&e).is_some_and(|gt| {
                gt.entries
                    .keys()
                    .any(|k| k.cell == *c && tier.is_none_or(|t| k.tier == t))
            })
        };
        if let Some(c) = cs.iter().find(|c| !known(c)) {
            return Err(missing(
                "procgen_unknown_cell",
                format!("cell [{}, {}] has no entry (it is not wanted)", c.x, c.z),
            ));
        }
    }
    let keys = rebuild_cells(world, e, tier, cells.as_deref());
    Ok(json!({
        "requeued": keys.iter().map(|k| json!([k.cell.x, k.cell.z, k.tier])).collect::<Vec<_>>(),
        "run_requested": false,
    }))
}

/// One pending `procgen.tiles_wait`.
struct TileWait {
    entity: Option<Entity>,
    anchor: Option<[f32; 3]>,
    tier: Option<u8>,
    deadline: Instant,
    id: Value,
    reply: Sender<Response>,
}

/// Pending `procgen.tiles_wait` calls, polled by [`advance_tile_waits`].
#[derive(Resource, Default)]
pub(crate) struct TileWaits(Vec<TileWait>);

/// `procgen.tiles_wait { entity?, anchor?, tier?, timeout_ms? }`: deferred until every wanted cell
/// around the anchor (or the current anchors) is resident.
pub(crate) fn tiles_wait(
    world: &mut World,
    params: &Value,
    id: Value,
    reply: Sender<Response>,
) -> Result<(), RpcError> {
    available(world)?;
    let entity = match params.get("entity").filter(|v| !v.is_null()) {
        Some(_) => Some(tiled(world, params)?.0),
        None => None,
    };
    let tier = match entity {
        Some(e) => tier_param(world, e, params)?,
        None => crate::params::opt_u64(params, "tier")?.and_then(|t| u8::try_from(t).ok()),
    };
    let anchor = match params.get("anchor") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let a = v
                .as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| {
                    Some([
                        a[0].as_f64()? as f32,
                        a[1].as_f64()? as f32,
                        a[2].as_f64()? as f32,
                    ])
                })
                .ok_or_else(|| bad("procgen_param_type", "`anchor` is [x, y, z]"))?;
            Some(a)
        }
    };
    let timeout = Duration::from_millis(
        crate::params::opt_u64(params, "timeout_ms")?
            .unwrap_or(10_000)
            .min(120_000),
    );
    let pending = world.get_resource::<TileWaits>().map_or(0, |q| q.0.len());
    if pending >= MAX_TILE_WAITS {
        return Err(crate::seeing::too_many_pending(
            "procgen.tiles_wait",
            MAX_TILE_WAITS,
        ));
    }
    world
        .get_resource_or_insert_with(TileWaits::default)
        .0
        .push(TileWait {
            entity,
            anchor,
            tier,
            deadline: Instant::now() + timeout,
            id,
            reply,
        });
    Ok(())
}

/// `(ready, cells counted)` for one wait.
fn wait_state(world: &mut World, w: &TileWait) -> (bool, usize) {
    let gens: Vec<(Entity, ProcGenerator)> = world
        .query::<(Entity, &ProcGenerator)>()
        .iter(world)
        .filter(|(e, g)| w.entity.is_none_or(|x| x == *e) && regia_procgen::tiles::is_anchored_runtime(g))
        .map(|(e, g)| (e, g.clone()))
        .collect();
    let mut cells = 0;
    let mut ready = true;
    for (e, g) in gens {
        let Some(settings) = g.tiles.as_ref().and_then(|t| t.runtime.clone()) else {
            continue;
        };
        let anchors: Vec<AnchorView> = match w.anchor {
            Some(p) => vec![AnchorView::at(regia_core::math::Vec3::from(p))],
            None => gather_anchors(world, settings.anchors),
        };
        let tiles = world.resource::<ProcGenTiles>();
        let Some(gt) = tiles.generators.get(&e) else {
            ready = false;
            continue;
        };
        let keys: Vec<TileKey> = gt
            .wanted_keys(&anchors, &settings)
            .into_iter()
            .filter(|k| w.tier.is_none_or(|t| k.tier == t))
            .collect();
        cells += keys.len();
        if !keys.iter().all(|k| gt.state(*k) == Some(TileState::Resident)) {
            ready = false;
        }
    }
    (ready, cells)
}

/// `Last`: answers every pending `procgen.tiles_wait` that is ready or timed out.
pub fn advance_tile_waits(world: &mut World) {
    let Some(mut queue) = world.remove_resource::<TileWaits>() else {
        return;
    };
    let mut keep = Vec::new();
    for w in std::mem::take(&mut queue.0) {
        let (ready, cells) = wait_state(world, &w);
        if ready {
            let _ = w.reply.send(Response::ok(
                w.id.clone(),
                json!({ "ready": true, "cells": cells }),
            ));
        } else if Instant::now() >= w.deadline {
            let _ = w.reply.send(Response::err(
                w.id.clone(),
                missing(
                    "procgen_tiles_timeout",
                    format!("{cells} wanted cells were not all resident before the timeout"),
                ),
            ));
        } else {
            keep.push(w);
        }
    }
    queue.0 = keep;
    world.insert_resource(queue);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_procgen::tiles::{AnchorSettings, ProcGenAnchor, TileSettings};
    use regia_procgen::world::{ProcBounds, ProcGenMode, ProcGenPlugin, ProcGraphSource};
    use regia_scene::Transform;
    use std::collections::BTreeMap;

    fn graph() -> String {
        let n = |ty: &str, k: &str, v: &str| {
            let e = regia_procgen::NodeEntry::new(ty);
            if k.is_empty() {
                e
            } else {
                e.with(
                    k,
                    regia_procgen::SettingValue::parse(v).unwrap_or(regia_procgen::SettingValue::Int(0)),
                )
            }
        };
        regia_procgen::ProcGraph {
            nodes: [
                (1, n("in.self", "", "")),
                (2, n("sample.region", "spacing_m", "8.0")),
                (3, n("spawn.mesh", "entries", "[(mesh: \"rock.glb\")]")),
            ]
            .into_iter()
            .collect(),
            edges: vec![
                regia_procgen::Edge::new(1, "out", 2, "region"),
                regia_procgen::Edge::new(2, "out", 3, "in"),
            ],
            ..regia_procgen::ProcGraph::default()
        }
        .to_ron()
    }

    fn app() -> App {
        let mut app = App::new();
        let mut files = BTreeMap::new();
        files.insert("rocks.pgraph.ron".to_string(), graph());
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app
    }

    fn anchored(app: &mut App) -> Entity {
        let generator = ProcGenerator {
            mode: ProcGenMode::Anchored,
            bounds: ProcBounds::Box {
                half_extents: regia_core::math::Vec3::new(512.0, 10.0, 512.0),
            },
            tiles: Some(TileSettings {
                cell_m: 64.0,
                halo_m: Some(4.0),
                runtime: Some(AnchorSettings {
                    anchors: AnchorSource::Tagged,
                    base_reach_m: 100.0,
                    ..AnchorSettings::default()
                }),
            }),
            seed: Some(5),
            ..ProcGenerator::new("rocks.pgraph.ron")
        };
        app.world_mut().spawn((generator, Transform::default())).id()
    }

    /// `procgen.set_debug` answers before and after, keeps the flag it was not given, and refuses a
    /// value that is not a boolean. Without the renderer's gizmos nothing draws (`drawn: false`).
    #[test]
    fn set_debug_toggles_the_overlays() {
        let mut app = app();
        let world = app.world_mut();
        let answer = set_debug(world, &json!({ "tiles": true })).expect("set");
        assert_eq!(answer["before"], json!({ "tiles": false, "regions": false }));
        assert_eq!(answer["after"], json!({ "tiles": true, "regions": false }));
        assert_eq!(answer["drawn"], false);
        let answer = set_debug(world, &json!({ "regions": true })).expect("set");
        assert_eq!(answer["after"], json!({ "tiles": true, "regions": true }));
        assert_eq!(
            *world.resource::<ProcGenDebug>(),
            ProcGenDebug {
                tiles: true,
                regions: true
            }
        );
        let error = set_debug(world, &json!({ "tiles": "yes" })).expect_err("not a boolean");
        assert_eq!(error.data.expect("data")["reason"], "procgen_param_type");
    }

    /// A tiled generator bakes through `procgen.bake`, per cell, and a named cell re-bakes.
    #[test]
    fn bake_writes_cells_and_re_bakes_a_named_cell() {
        let dir = regia_test_utils::unique_temp_dir("agent-procgen-cells");
        let mut app = app();
        app.world_mut().resource_mut::<ProcGenSettings>().bake_dir = Some(dir.clone());
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: regia_core::math::Vec3::new(64.0, 10.0, 64.0),
                    },
                    tiles: Some(TileSettings {
                        cell_m: 64.0,
                        ..TileSettings::default()
                    }),
                    seed: Some(5),
                    ..ProcGenerator::new("rocks.pgraph.ron")
                },
                Transform::default(),
            ))
            .id();
        let world = app.world_mut();
        let bits = e.to_bits();
        let all = crate::procgen::bake(world, &json!({ "entity": bits })).expect("bake");
        assert_eq!(all["ok"], true, "{all}");
        let one = crate::procgen::bake(world, &json!({ "entity": bits, "cells": [[0, 0]] })).expect("cell");
        assert_eq!(one["ok"], true, "{one}");
        assert_eq!(one["baked"][0]["cells"], 1);
        let bad = crate::procgen::bake(world, &json!({ "entity": bits, "cells": [[0]] })).expect_err("shape");
        assert_eq!(bad.data.expect("data")["reason"], "procgen_unknown_cell");
        // A run with debug data answers per cell, and a tiled run has no whole-run data to read.
        world.resource_mut::<ProcGenSettings>().keep_debug = true;
        regia_procgen::world::run_now(world, e);
        let cell = crate::procgen::inspect(world, &json!({ "entity": bits, "node": 2, "cell": [0, 0] }))
            .expect("the cell's points");
        let points = cell["items"][0]["len"].as_u64().expect("len");
        assert!(points > 0 && points < 100, "the cell plus its halo: {cell}");
        let whole =
            crate::procgen::inspect(world, &json!({ "entity": bits, "node": 2 })).expect_err("no cell");
        assert_eq!(whole.data.expect("data")["reason"], "procgen_no_debug_data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_tiles_set_rejects_f32_overflow_without_changing_reach() {
        let mut app = app();
        let e = anchored(&mut app);
        for _ in 0..4 {
            app.update();
        }
        let w = app.world_mut();
        let params = json!({ "entity": e.to_bits(), "tier": 0 });
        let before = tiles_set(w, &params).expect("reach")["after"].clone();
        for key in ["build_m", "release_factor", "view_bias"] {
            for value in [1e308, -1e-100] {
                let mut invalid = params.clone();
                invalid["build_m"] = json!(20);
                invalid[key] = json!(value);
                let err = tiles_set(w, &invalid).expect_err("out of f32 range");
                assert_eq!(err.code, INVALID_PARAMS);
                assert_eq!(err.data.expect("data")["reason"], "procgen_param_type");
                assert_eq!(tiles_set(w, &params).expect("unchanged reach")["after"], before);
            }
        }
        let after =
            tiles_set(w, &json!({ "entity": e.to_bits(), "tier": 0, "build_m": 20 })).expect("valid tuning");
        assert_eq!(after["after"]["build_m"], 20.0);
    }

    #[test]
    fn anchored_generator_streams_cells_and_answers_the_tile_methods() {
        let mut app = app();
        let e = anchored(&mut app);
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(regia_core::math::Vec3::new(10.0, 0.0, 10.0)),
        ));
        for _ in 0..12 {
            app.update();
        }
        let w = app.world_mut();
        let r = tiles(w, &json!({ "entity": e.to_bits() })).unwrap_or_else(|e| panic!("{e:?}"));
        let cells = r["cells"].as_array().map_or(0, Vec::len);
        assert!(cells >= 9, "{r}");
        assert!(
            r["cells"]
                .as_array()
                .is_some_and(|c| c.iter().all(|c| c["state"] == "resident")),
            "{r}"
        );
        let a = anchors(w, &json!({})).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(a["tagged"].as_array().map(Vec::len), Some(1));
        // Live tuning: a smaller radius releases far cells.
        let s = tiles_set(w, &json!({ "entity": e.to_bits(), "tier": 0, "build_m": 20.0 }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(s["before"]["build_m"], 100.0);
        let rb = tiles_rebuild(w, &json!({ "entity": e.to_bits(), "cells": [[0, 0]] }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(rb["requeued"].as_array().map(Vec::len), Some(1));
        let unknown = tiles_rebuild(w, &json!({ "entity": e.to_bits(), "cells": [[90, 90]] }));
        assert!(unknown.is_err_and(|e| { e.data.is_some_and(|d| d["reason"] == "procgen_unknown_cell") }));
        for _ in 0..12 {
            app.update();
        }
        let w = app.world_mut();
        let r = tiles(w, &json!({ "entity": e.to_bits(), "state": "resident" }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        let after = r["cells"].as_array().map_or(0, Vec::len);
        assert!(after < cells, "{after} vs {cells}");
        let bad_tier = tiles(w, &json!({ "entity": e.to_bits(), "tier": 9 }));
        assert!(bad_tier.is_err_and(|e| { e.data.is_some_and(|d| d["reason"] == "procgen_tier_unknown") }));
    }

    /// Rescue review item 2: `procgen.anchors` buckets by source, so a `ProcGenExtraAnchors` entry
    /// (the editor's "generate around view" camera) must appear only under `extra`, not inside
    /// `primary_view`/`tagged` as if it were one of each.
    #[test]
    fn anchors_reports_extra_anchors_under_their_own_key() {
        let mut app = app();
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(regia_core::math::Vec3::new(10.0, 0.0, 10.0)),
        ));
        let extra_pos = regia_core::math::Vec3::new(99.0, 2.0, 99.0);
        app.world_mut()
            .insert_resource(ProcGenExtraAnchors(vec![AnchorView::at(extra_pos)]));
        let a = anchors(app.world_mut(), &json!({})).unwrap_or_else(|e| panic!("{e:?}"));
        // No window camera exists, so primary_view stays empty rather than adopting the extra.
        assert_eq!(a["primary_view"], Value::Null, "{a}");
        let tagged = a["tagged"].as_array().unwrap_or_else(|| panic!("{a}"));
        assert_eq!(tagged.len(), 1, "{a}");
        let extra = a["extra"].as_array().unwrap_or_else(|| panic!("{a}"));
        assert_eq!(extra.len(), 1, "{a}");
        assert_eq!(extra[0]["position"], json!([99.0, 2.0, 99.0]), "{a}");
    }

    /// Audit L12: `tiles_set` answers the values that take effect, so a `release_factor` below the
    /// hysteresis minimum reads back as the minimum (1.1), not as the number sent.
    #[test]
    fn tiles_set_echoes_the_effective_release_factor() {
        let mut app = app();
        let e = anchored(&mut app);
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(regia_core::math::Vec3::new(10.0, 0.0, 10.0)),
        ));
        for _ in 0..4 {
            app.update();
        }
        let s = tiles_set(
            app.world_mut(),
            &json!({ "entity": e.to_bits(), "tier": 0, "release_factor": 1.0, "view_bias": 3.0 }),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let release = s["after"]["release_factor"].as_f64().unwrap_or(0.0);
        assert!((release - 1.1).abs() < 1e-6, "{s}");
        let bias = s["after"]["view_bias"].as_f64().unwrap_or(0.0);
        assert!((bias - 1.0).abs() < 1e-6, "{s}");
    }

    #[test]
    fn untiled_generator_is_refused() {
        let mut app = app();
        let e = app.world_mut().spawn(ProcGenerator::new("rocks.pgraph.ron")).id();
        let r = tiles(app.world_mut(), &json!({ "entity": e.to_bits() }));
        assert!(r.is_err_and(|e| e.data.is_some_and(|d| d["reason"] == "procgen_not_tiled")));
    }

    #[test]
    fn tiles_wait_answers_when_cells_are_resident() {
        let mut app = app();
        let e = anchored(&mut app);
        let (tx, rx) = crossbeam_channel::unbounded();
        let p = json!({ "entity": e.to_bits(), "anchor": [10.0, 0.0, 10.0], "timeout_ms": 60000 });
        tiles_wait(app.world_mut(), &p, json!(1), tx).unwrap_or_else(|e| panic!("{e:?}"));
        app.add_systems(Last, advance_tile_waits);
        // No anchor entity: nothing is wanted by the scheduler, so add one at the waited position.
        app.world_mut().spawn((
            ProcGenAnchor::default(),
            Transform::from_translation(regia_core::math::Vec3::new(10.0, 0.0, 10.0)),
        ));
        let mut answer = None;
        for _ in 0..20 {
            app.update();
            if let Ok(r) = rx.try_recv() {
                answer = Some(r);
                break;
            }
        }
        let r = answer.unwrap_or_else(|| panic!("tiles_wait never answered"));
        let v = r.result.unwrap_or_default();
        assert_eq!(v["ready"], true, "{v}");
    }

    /// Spec 66 § 6: a wait whose cells are not resident by its deadline answers
    /// `procgen_tiles_timeout`. `timeout_ms: 0` makes the deadline the moment of the call, so the
    /// first poll times out without a clock in the test.
    #[test]
    fn tiles_wait_times_out_with_procgen_tiles_timeout() {
        let mut app = app();
        let e = anchored(&mut app);
        let (tx, rx) = crossbeam_channel::unbounded();
        let p = json!({ "entity": e.to_bits(), "anchor": [10.0, 0.0, 10.0], "timeout_ms": 0 });
        tiles_wait(app.world_mut(), &p, json!(7), tx).unwrap_or_else(|e| panic!("{e:?}"));
        // Polled before the scheduler ever ran: nothing is resident.
        advance_tile_waits(app.world_mut());
        let r = rx
            .try_recv()
            .unwrap_or_else(|_| panic!("the wait did not answer"));
        assert_eq!(r.id, json!(7));
        let err = r
            .error
            .unwrap_or_else(|| panic!("expected a timeout, got {:?}", r.result));
        assert_eq!(err.code, NOT_FOUND);
        assert_eq!(err.data.expect("data")["reason"], "procgen_tiles_timeout");
        assert!(
            app.world().resource::<TileWaits>().0.is_empty(),
            "answered waits leave the list"
        );
    }

    /// Spec 66 § 6: `procgen.tiles_wait` shares the `play.wait_until` cap of 64 pending calls; the
    /// 65th answers `too_many_pending`.
    #[test]
    fn tiles_wait_refuses_a_65th_pending_call() {
        let mut app = app();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let p = json!({ "anchor": [0.0, 0.0, 0.0], "timeout_ms": 60000 });
        for i in 0..MAX_TILE_WAITS {
            tiles_wait(app.world_mut(), &p, json!(i), tx.clone()).unwrap_or_else(|e| panic!("{i}: {e:?}"));
        }
        let err = tiles_wait(app.world_mut(), &p, json!(64), tx).expect_err("the list is full");
        let reason = err.data.as_ref().and_then(|d| d.get("reason")).cloned();
        let code = err.data.as_ref().and_then(|d| d.get("code")).cloned();
        assert!(
            reason == Some(json!("too_many_pending")) || code == Some(json!("too_many_pending")),
            "{err:?}"
        );
        assert_eq!(app.world().resource::<TileWaits>().0.len(), MAX_TILE_WAITS);
    }

    /// Two tiers: `in.self → sample.region → tiles.tier(256) → tiles.tier(32) → spawn.mesh`.
    fn two_tier_graph() -> String {
        let n = |ty: &str, k: &str, v: &str| {
            let e = regia_procgen::NodeEntry::new(ty);
            match regia_procgen::SettingValue::parse(v) {
                Ok(s) if !k.is_empty() => e.with(k, s),
                _ => e,
            }
        };
        regia_procgen::ProcGraph {
            nodes: [
                (1, n("in.self", "", "")),
                (2, n("sample.region", "spacing_m", "16.0")),
                (3, n("tiles.tier", "cell_m", "Meters(256.0)")),
                (4, n("tiles.tier", "cell_m", "Meters(32.0)")),
                (5, n("spawn.mesh", "entries", "[(mesh: \"rock.glb\")]")),
            ]
            .into_iter()
            .collect(),
            edges: vec![
                regia_procgen::Edge::new(1, "out", 2, "region"),
                regia_procgen::Edge::new(2, "out", 3, "in"),
                regia_procgen::Edge::new(3, "out", 4, "in"),
                regia_procgen::Edge::new(4, "out", 5, "in"),
            ],
            ..regia_procgen::ProcGraph::default()
        }
        .to_ron()
    }

    /// Spec 66 § 4.2: the validation warns `tier_reach_order` when a finer tier's `build_m` exceeds
    /// a coarser tier's; `procgen.tiles` reports it under `warnings`, and the right order reports
    /// none.
    #[test]
    fn tiles_reports_tier_reach_order_when_a_fine_tier_outreaches_its_parent() {
        let reach = |cell_m: f32, build_m: f32| TierReach {
            cell_m,
            build_m,
            ..TierReach::default()
        };
        let spawn = |app: &mut App, fine_build: f32| {
            let generator = ProcGenerator {
                mode: ProcGenMode::Anchored,
                bounds: ProcBounds::Box {
                    half_extents: regia_core::math::Vec3::new(256.0, 10.0, 256.0),
                },
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    halo_m: Some(4.0),
                    runtime: Some(AnchorSettings {
                        anchors: AnchorSource::Tagged,
                        tiers: vec![reach(256.0, 100.0), reach(32.0, fine_build)],
                        ..AnchorSettings::default()
                    }),
                }),
                seed: Some(5),
                ..ProcGenerator::new("tiers.pgraph.ron")
            };
            app.world_mut().spawn((generator, Transform::default())).id()
        };
        let mut app = app();
        let mut files = BTreeMap::new();
        files.insert("tiers.pgraph.ron".to_string(), two_tier_graph());
        app.world_mut().resource_mut::<ProcGenSettings>().source = ProcGraphSource::Memory(files);
        let wrong = spawn(&mut app, 300.0);
        let right = spawn(&mut app, 50.0);
        app.world_mut()
            .spawn((ProcGenAnchor::default(), Transform::default()));
        for _ in 0..4 {
            app.update();
        }
        let w = app.world_mut();
        let r = tiles(w, &json!({ "entity": wrong.to_bits() })).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(r["error"], Value::Null, "{r}");
        let warnings = r["warnings"].as_array().unwrap_or_else(|| panic!("{r}"));
        assert_eq!(warnings.len(), 1, "{r}");
        assert_eq!(warnings[0]["reason"], "tier_reach_order", "{r}");
        let r = tiles(w, &json!({ "entity": right.to_bits() })).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(r["warnings"], json!([]), "{r}");
    }
}
