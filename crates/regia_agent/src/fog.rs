// SPDX-License-Identifier: MPL-2.0
//! `fog.status` and `fog.set_viewer`: the per-viewer fog of war over the sim bridge (plan row
//! S-16, ADR-8154, book/agents/fog-of-war.md).
//!
//! Both read or write the two non-generic resources `regia_framework::fog_view::FogViewPlugin`
//! installs, so they work whatever sim type the app wraps. `fog.status` also reports the server's
//! fog relevance (`regia_framework::fog_relevance`, audit gap FW-5) under `net`. The status reports counts only: never
//! a hidden entity's id, kind or position (a ghost count, never a ghost's row).

use regia_app::prelude::*;
use regia_framework::fog_relevance::{FogRelevance, FogRelevanceStatus};
use regia_framework::fog_texture::{FogDebugView, FogTextureStats};
use regia_framework::fog_view::{FogViewControl, FogViewStatus};
use regia_render::{FogLook, FogOverlayHandle};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_fog_view() -> RpcError {
    RpcError::not_found(
        "no fog view in this app",
        "wrap the sim in regia_framework::fog_view::FogViewSim and add FogViewPlugin, or add FogRelevancePlugin on a server",
    )
}

/// `fog.status`: the viewing team, the rules in force and the last filtered frame's counters, plus
/// the server's fog relevance under `net` (`null` without one). An app with only the fog relevance
/// (a dedicated server) answers `view: false` and the `net` block.
pub fn status(world: &mut World) -> Result<Value, RpcError> {
    let net = world
        .get_resource::<FogRelevance>()
        .map(|f| net_block(&f.status()));
    let Some(status) = world.get_resource::<FogViewStatus>() else {
        return match net {
            Some(net) => Ok(json!({ "view": false, "net": net })),
            None => Err(no_fog_view()),
        };
    };
    let pending = world
        .get_resource::<FogViewControl>()
        .copied()
        .unwrap_or_default();
    let s = &status.stats;
    Ok(json!({
        "view": true,
        "enabled": status.enabled,
        "viewer": status.viewer,
        "observer": status.observer,
        "team_count": status.team_count,
        "sight_kinds": status.sight_kinds,
        "line_of_sight": status.line_of_sight,
        "event_policy": status.event_policy,
        "frames": s.frames,
        "tick": s.tick,
        "visible": s.visible,
        "hidden": s.hidden,
        "unlisted": s.unlisted,
        "watchers": s.watchers,
        "events_passed": s.events_passed,
        "events_dropped": s.events_dropped,
        "detectors": s.detectors,
        "queries": s.queries,
        "index_candidates": s.index_candidates,
        "los_calls": s.los_calls,
        "ghosts": s.ghosts,
        "ghosts_dropped": s.ghosts_dropped,
        "pending": { "viewer": pending.viewer, "enabled": pending.enabled },
        "net": net,
    }))
}

/// The `net` block: the server's fog relevance (counts only, never a hidden entity's id).
fn net_block(s: &FogRelevanceStatus) -> Value {
    let teams: Vec<Value> = s
        .teams
        .iter()
        .map(|t| {
            json!({
                "team": t.team,
                "peers": t.peers,
                "shown": t.shown,
                "hidden": t.hidden,
                "linger_hits": t.linger_hits,
                "lead_hits": t.lead_hits,
                "watchers": t.watchers,
                "effects_shown": t.effects_shown,
                "effects_hidden": t.effects_hidden,
                "queries": t.queries,
            })
        })
        .collect();
    let peers: Vec<Value> = s
        .peers
        .iter()
        .map(
            |p| json!({ "peer": p.peer.0, "team": p.team, "evaluated": p.evaluated, "filtered": p.filtered }),
        )
        .collect();
    json!({
        "enabled": s.enabled,
        "team_count": s.team_count,
        "linger_ticks": s.linger_ticks,
        "lead_ticks": s.lead_ticks,
        "lead_margin_m": s.lead_margin_m,
        "event_policy": s.event_policy,
        "line_of_sight": s.line_of_sight,
        "ticks": s.ticks,
        "units": s.units,
        "unplaced": s.unplaced,
        "effects": s.effects,
        "visions_built": s.visions_built,
        "lead_visions_built": s.lead_visions_built,
        "visions_total": s.visions_total,
        "teams": teams,
        "peers": peers,
        "unmapped_refusals": s.unmapped_refusals,
        "unlisted_refusals": s.unlisted_refusals,
        "linger_hits_total": s.linger_hits_total,
        "lead_hits_total": s.lead_hits_total,
        "events_passed": s.events_passed,
        "events_dropped": s.events_dropped,
    })
}

/// `fog.set_viewer { viewer?, enabled?, dry_run? }`: switch the viewing team and/or turn fog on or
/// off, from the next frame. At least one of `viewer` and `enabled` is required.
pub fn set_viewer(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let viewer = match params.get("viewer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| {
            RpcError::invalid_params("`viewer` must be a team number from 0 to 4294967295")
        })?),
    };
    let enabled = match params.get("enabled") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be true or false"))?,
        ),
    };
    if viewer.is_none() && enabled.is_none() {
        return Err(RpcError::invalid_params("give `viewer`, `enabled` or both"));
    }
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    let before = world.get_resource::<FogViewStatus>().ok_or_else(no_fog_view)?;
    let before_json = json!({ "viewer": before.viewer, "enabled": before.enabled });
    let after_json = json!({
        "viewer": viewer.unwrap_or(before.viewer),
        "enabled": enabled.unwrap_or(before.enabled),
    });
    if !dry_run {
        let mut control = world.get_resource_or_insert_with(FogViewControl::default);
        if viewer.is_some() {
            control.viewer = viewer;
        }
        if enabled.is_some() {
            control.enabled = enabled;
        }
    }
    Ok(json!({
        "before": before_json,
        "after": after_json,
        "applied": !dry_run,
        "note": "applies at the start of the next frame; fog.status then reports it",
    }))
}

fn no_fog_presentation() -> RpcError {
    RpcError::not_found(
        "no fog presentation in this app",
        "give the FogViewSim a grid (with_grid) and add regia_framework::fog_texture::FogPresentationPlugin",
    )
}

/// `fog.texture_stats`: the viewer's fog texture (size, place, the last sync's dirty, fading and
/// uploaded rows and bytes), the world overlay's GPU counters and look when it is installed, and
/// whether the editor's debug view is on.
pub fn texture_stats(world: &mut World) -> Result<Value, RpcError> {
    let s = *world
        .get_resource::<FogTextureStats>()
        .ok_or_else(no_fog_presentation)?;
    let overlay = world.get_resource::<FogOverlayHandle>().map(|h| {
        let o = h.stats();
        let (cols, rows) = h.size();
        json!({
            "cols": cols,
            "rows": rows,
            "draws": o.draws,
            "texture_creates": o.texture_creates,
            "rows_uploaded": o.rows_uploaded,
            "bytes_uploaded": o.bytes_uploaded,
            "last_rows": o.last_rows,
            "copies": o.copies,
            "failures": o.failures,
            "pending_rows": h.pending_rows(),
        })
    });
    let look = world.get_resource::<FogLook>().copied().map(look_json);
    let debug_view = world.get_resource::<FogDebugView>().is_some_and(|d| d.enabled);
    Ok(json!({
        "cols": s.cols,
        "rows": s.rows,
        "origin": s.origin,
        "cell_m": s.cell_m,
        "dirty_rows": s.dirty_rows,
        "fading_rows": s.fading_rows,
        "rows_uploaded": s.rows_uploaded,
        "upload_bytes": s.upload_bytes,
        "runs": s.runs,
        "syncs": s.syncs,
        "total_upload_bytes": s.total_upload_bytes,
        "full_uploads": s.full_uploads,
        "overlay": overlay,
        "look": look,
        "debug_view": debug_view,
    }))
}

fn look_json(look: FogLook) -> Value {
    json!({
        "enabled": look.enabled,
        "strength": look.strength,
        "explored_darkness": look.explored_darkness,
        "explored_tint": look.explored_tint,
        "unexplored_darkness": look.unexplored_darkness,
        "unexplored_tint": look.unexplored_tint,
        "softness": look.softness,
    })
}

fn unit_param(params: &Value, key: &str, max: f64) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(x) if x.is_finite() && (0.0..=max).contains(&x) => Ok(Some(x as f32)),
            _ => Err(RpcError::invalid_params(format!(
                "`{key}` must be a number from 0 to {max}"
            ))),
        },
    }
}

/// `fog.set_look { enabled?, strength?, explored_darkness?, unexplored_darkness?, softness?,
/// dry_run? }`: changes the world overlay's look (the `FogLook` resource) from the next frame.
pub fn set_look(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = match params.get("enabled") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be true or false"))?,
        ),
    };
    let strength = unit_param(params, "strength", 1.0)?;
    let explored = unit_param(params, "explored_darkness", 1.0)?;
    let unexplored = unit_param(params, "unexplored_darkness", 1.0)?;
    let softness = unit_param(params, "softness", 2.0)?;
    let dry_run = crate::params::opt_bool(params, "dry_run")?.unwrap_or(false);
    world
        .get_resource::<FogTextureStats>()
        .ok_or_else(no_fog_presentation)?;
    let before = world.get_resource::<FogLook>().copied().unwrap_or_default();
    let mut after = before;
    if let Some(v) = enabled {
        after.enabled = v;
    }
    if let Some(v) = strength {
        after.strength = v;
    }
    if let Some(v) = explored {
        after.explored_darkness = v;
    }
    if let Some(v) = unexplored {
        after.unexplored_darkness = v;
    }
    if let Some(v) = softness {
        after.softness = v;
    }
    if !dry_run {
        world.insert_resource(after);
    }
    Ok(json!({
        "before": look_json(before),
        "after": look_json(after),
        "applied": !dry_run,
        "note": "the overlay draws only when the app installed it (FrameRenderer::enable_fog_overlay with the FogOverlayHandle resource)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stable `F0xxx` code of a failed call.
    fn code(result: Result<Value, RpcError>) -> Option<String> {
        let err = result.err()?;
        Some(err.data?.get("code")?.as_str()?.to_string())
    }

    fn world_with_fog() -> World {
        let mut world = World::new();
        world.insert_resource(FogViewStatus {
            enabled: true,
            viewer: 1,
            team_count: 4,
            line_of_sight: "open_ground".to_string(),
            event_policy: "hide_unseen",
            ..FogViewStatus::default()
        });
        world.insert_resource(FogViewControl::default());
        world
    }

    #[test]
    fn status_without_a_fog_view_is_not_found() {
        let mut world = World::new();
        assert_eq!(code(status(&mut world)).as_deref(), Some("F0003"));
    }

    #[test]
    fn set_viewer_queues_a_request_and_dry_run_does_not() {
        let mut world = world_with_fog();
        let preview = set_viewer(&mut world, &json!({ "viewer": 2, "dry_run": true }));
        assert!(matches!(&preview, Ok(v) if v["applied"] == false && v["after"]["viewer"] == 2));
        assert_eq!(world.resource::<FogViewControl>().viewer, None);
        let done = set_viewer(&mut world, &json!({ "viewer": 2, "enabled": false }));
        assert!(matches!(&done, Ok(v) if v["applied"] == true && v["before"]["viewer"] == 1));
        let control = *world.resource::<FogViewControl>();
        assert_eq!((control.viewer, control.enabled), (Some(2), Some(false)));
        let shown = status(&mut world);
        assert!(matches!(&shown, Ok(v) if v["pending"]["viewer"] == 2 && v["viewer"] == 1));
    }

    /// A two-unit sim: team 1 at the origin, team 2 at 400 m (out of the default 150 m sight).
    mod toy {
        use std::time::Duration;

        use regia_core::math::{Quat, Vec3};
        use regia_framework::fog_view::{FogEntity, FogWorld, SightClass};
        use regia_framework::sim_bridge::{
            BridgeInput, ExternalSim, SimEntityView, SimSnapshot, SnapshotTick,
        };

        const ROWS: [(u64, u32, f32); 2] = [(1, 1, 0.0), (2, 2, 400.0)];

        #[derive(Default)]
        pub struct Snap;

        impl SimSnapshot for Snap {
            fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
                for (id, team, x) in ROWS {
                    f(SimEntityView {
                        id,
                        kind: "scout",
                        team,
                        translation: Vec3::new(x, 0.0, 0.0),
                        rotation: Quat::IDENTITY,
                    });
                }
            }
        }

        impl FogWorld for Snap {
            fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
                ROWS.into_iter()
                    .map(|(id, team, x)| FogEntity::new(id, "scout", SightClass::Unit, team, [x, 0.0, 0.0]))
            }
        }

        pub struct Sim;

        impl ExternalSim for Sim {
            type Snapshot = Snap;
            type Event = ();

            fn submit_input(&mut self, _input: &BridgeInput) {}

            fn read_snapshot(&mut self, _frame_time: Duration, _out: &mut Snap) -> SnapshotTick {
                SnapshotTick(1)
            }

            fn drain_events(&mut self, _out: &mut Vec<()>) {}
        }
    }

    #[test]
    fn set_viewer_then_status_through_a_real_fog_view() {
        use regia_framework::fog_view::{FogRules, FogViewPlugin, FogViewSim};
        use regia_framework::sim_bridge::SimBridgePlugin;

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            SimBridgePlugin::new(FogViewSim::new(toy::Sim, FogRules::default(), 1)),
            FogViewPlugin::<toy::Sim>::default(),
        ));
        app.update();
        let first = status(app.world_mut());
        assert!(
            matches!(&first, Ok(v) if v["viewer"] == 1 && v["visible"] == 1 && v["hidden"] == 1 && v["watchers"] == 1),
            "{first:?}"
        );
        // The work counters: one enemy answered from sight, by one watcher (out of range: no
        // line-of-sight call), no detectors, no remembered kinds.
        assert!(
            matches!(&first, Ok(v) if v["queries"] == 1 && v["index_candidates"] == 1 && v["los_calls"] == 0
                && v["detectors"] == 0 && v["ghosts"] == 0 && v["ghosts_dropped"] == 0),
            "{first:?}"
        );
        let switched = set_viewer(app.world_mut(), &json!({ "viewer": 0 }));
        assert!(switched.is_ok());
        app.update();
        let second = status(app.world_mut());
        assert!(
            matches!(&second, Ok(v) if v["observer"] == true && v["visible"] == 2 && v["hidden"] == 0
                && v["pending"]["viewer"].is_null()),
            "{second:?}"
        );
    }

    /// A server with only the fog relevance answers `view: false` and the `net` block; a view app
    /// carries `net: null`.
    #[test]
    fn status_reports_the_net_block() {
        use regia_framework::fog_relevance::{
            FogRelevanceSettings, FogTeamOf, FogUnit, fog_relevance_prepare,
        };
        use regia_framework::fog_view::{FogRules, SightClass};
        use regia_net::PeerId;
        use regia_scene::transform::Transform;

        let mut view = world_with_fog();
        assert!(matches!(status(&mut view), Ok(v) if v["view"] == true && v["net"].is_null()));
        let mut world = World::new();
        world.insert_resource(FogRelevance::new(
            FogRules::default(),
            FogRelevanceSettings::default(),
        ));
        let mut teams = FogTeamOf::default();
        teams.set(PeerId(1), 1);
        teams.set(PeerId(2), 2);
        world.insert_resource(teams);
        world.spawn((FogUnit::new(1, "scout", SightClass::Unit), Transform::default()));
        world.spawn((
            FogUnit::new(2, "scout", SightClass::Unit),
            Transform {
                translation: regia_core::math::Vec3::new(400.0, 0.0, 0.0),
                ..Transform::default()
            },
        ));
        let mut schedule = Schedule::default();
        schedule.add_systems(fog_relevance_prepare);
        schedule.run(&mut world);
        let got = status(&mut world);
        assert!(
            matches!(&got, Ok(v) if v["view"] == false
                && v["net"]["visions_built"] == 2
                && v["net"]["lead_visions_built"] == 2
                && v["net"]["units"] == 2
                && v["net"]["teams"][0]["shown"] == 1
                && v["net"]["teams"][0]["hidden"] == 1
                && v["net"]["peers"][1]["team"] == 2
                && v["net"]["event_policy"] == "hide_unseen"),
            "{got:?}"
        );
    }

    #[test]
    fn set_viewer_refuses_bad_params() {
        let mut world = world_with_fog();
        for params in [
            json!({}),
            json!({ "viewer": -1 }),
            json!({ "viewer": "red" }),
            json!({ "enabled": 1 }),
        ] {
            assert_eq!(
                code(set_viewer(&mut world, &params)).as_deref(),
                Some("F0002"),
                "{params}"
            );
        }
    }

    #[test]
    fn texture_stats_without_a_presentation_is_not_found() {
        let mut world = World::new();
        assert_eq!(code(texture_stats(&mut world)).as_deref(), Some("F0003"));
        assert_eq!(
            code(set_look(&mut world, &json!({ "enabled": true }))).as_deref(),
            Some("F0003")
        );
    }

    #[test]
    fn texture_stats_through_a_real_fog_presentation() {
        use regia_framework::fog_texture::FogPresentationPlugin;
        use regia_framework::fog_view::{FogGrid, FogRules, FogViewPlugin, FogViewSim};
        use regia_framework::sim_bridge::SimBridgePlugin;

        let mut app = App::new();
        let handle = FogOverlayHandle::default();
        app.insert_resource(handle.clone());
        app.add_plugins((
            MinimalPlugins,
            SimBridgePlugin::new(
                FogViewSim::new(toy::Sim, FogRules::default(), 1).with_grid(FogGrid::covering(640.0, 640.0)),
            ),
            FogViewPlugin::<toy::Sim>::default(),
            FogPresentationPlugin::<toy::Sim>::default(),
        ));
        app.update();
        let first = texture_stats(app.world_mut());
        assert!(
            matches!(&first, Ok(v) if v["cols"] == 40 && v["rows"] == 40 && v["rows_uploaded"] == 40
                && v["full_uploads"] == 1 && v["upload_bytes"] == 1600 && v["overlay"]["rows"] == 40
                && v["overlay"]["pending_rows"] == 40 && v["debug_view"] == false),
            "{first:?}"
        );
        app.update();
        let steady = texture_stats(app.world_mut());
        assert!(
            matches!(&steady, Ok(v) if v["rows_uploaded"] == 0 && v["dirty_rows"] == 0 && v["syncs"] == 2),
            "{steady:?}"
        );
        let preview = set_look(
            app.world_mut(),
            &json!({ "enabled": true, "softness": 1.5, "dry_run": true }),
        );
        assert!(matches!(&preview, Ok(v) if v["applied"] == false && v["after"]["softness"] == 1.5));
        assert!(app.world().get_resource::<FogLook>().is_none());
        let applied = set_look(app.world_mut(), &json!({ "enabled": true, "strength": 0.5 }));
        assert!(applied.is_ok());
        app.update();
        assert_eq!(handle.look().strength, 0.5);
        assert!(handle.look().enabled);
        for bad in [
            json!({ "strength": 2.0 }),
            json!({ "softness": -1 }),
            json!({ "enabled": "yes" }),
            json!({ "explored_darkness": "dark" }),
        ] {
            assert_eq!(
                code(set_look(app.world_mut(), &bad)).as_deref(),
                Some("F0002"),
                "{bad}"
            );
        }
    }
}
