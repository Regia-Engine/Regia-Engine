// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the mobile lifecycle and thermal hooks (ADR-527, book
//! `agents/mobile-lifecycle.md`): `lifecycle.get`, `lifecycle.drill` ([`LifecycleDrill`],
//! [`SurfaceAudit`]), `thermal.get` and `thermal.set_override` ([`ThermalOverride`],
//! [`ThermalGovernor`]), and `input.touch_controls` ([`TouchControls`]).

use regia_app::prelude::*;
use regia_framework::thermal::ThermalGovernor;
use regia_input::{TouchControlKind, TouchControls, TouchRect, Touches};
use regia_platform::lifecycle_drill::{MAX_DRILL_CYCLES, SurfaceViolation};
use regia_platform::{
    AppLifecycle, LifecycleDrill, LifecycleState, SurfaceAudit, ThermalOverride, ThermalState, ThermalStatus,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `lifecycle.drill`'s `refused` code while the app is not running (before the first resume,
/// suspended by the OS, exiting): nothing is queued.
const NOT_RUNNING: &str = "lifecycle_not_running";

fn state_name(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::NotStarted => "not_started",
        LifecycleState::Running => "running",
        LifecycleState::Suspended => "suspended",
        LifecycleState::Terminating => "terminating",
    }
}

fn violation_json(v: &SurfaceViolation) -> Value {
    json!({
        "code": v.kind.code(),
        "event": format!("{:?}", v.event),
        "resumes": v.resumes,
        "live": v.observed.live,
        "windows": v.observed.windows,
        "created_total": v.observed.created_total,
        "dropped_total": v.observed.dropped_total,
    })
}

fn lifecycle_json(world: &World) -> Value {
    let lifecycle = world.get_resource::<AppLifecycle>().cloned().unwrap_or_default();
    let drill = world
        .get_resource::<LifecycleDrill>()
        .cloned()
        .unwrap_or_default();
    let audit = world.get_resource::<SurfaceAudit>().cloned().unwrap_or_default();
    let last = audit.last();
    let surfaces = world.get_resource::<regia_render::WindowSurfaces>().map(|s| {
        json!({
            "live": s.len(),
            "created_total": s.created_total(),
            "dropped_total": s.dropped_total(),
            "lost_streak": s.lost_streak(),
        })
    });
    json!({
        "state": state_name(lifecycle.state),
        "resume_count": lifecycle.resume_count,
        "drill": { "pending": drill.pending(), "completed": drill.completed() },
        "audit": {
            "clean": audit.is_clean(),
            "suspends": audit.suspends(),
            "resumes": audit.resumes(),
            "live": last.live,
            "windows": last.windows,
            "created_total": last.created_total,
            "dropped_total": last.dropped_total,
            "violation_count": audit.violation_count(),
            "violations": audit.violations().iter().map(violation_json).collect::<Vec<_>>(),
        },
        "surfaces": surfaces,
    })
}

/// `lifecycle.get`: lifecycle state, drill queue and the surface audit.
pub(crate) fn lifecycle_get(world: &mut World) -> Result<Value, RpcError> {
    Ok(lifecycle_json(world))
}

/// `lifecycle.drill`: queues `cycles` synthetic suspend → resume cycles (one per frame), or
/// drops the queue with `cancel: true`. While the app is not running nothing is queued and the
/// answer carries `refused: "lifecycle_not_running"`.
pub(crate) fn lifecycle_drill(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if crate::params::opt_bool(params, "cancel")? == Some(true) {
        let dropped = world
            .get_resource_mut::<LifecycleDrill>()
            .map_or(0, |mut d| d.cancel());
        return Ok(json!({ "cancelled": dropped, "pending": 0 }));
    }
    let cycles = crate::params::opt_u64(params, "cycles")?
        .and_then(|c| u32::try_from(c).ok())
        .filter(|c| (1..=MAX_DRILL_CYCLES).contains(c))
        .ok_or_else(|| {
            RpcError::invalid_params(format!("`cycles` must be an integer 1..={MAX_DRILL_CYCLES}"))
        })?;
    let state = world.get_resource::<AppLifecycle>().map(|l| l.state);
    if state != Some(LifecycleState::Running) {
        return Ok(json!({
            "queued": 0,
            "refused": NOT_RUNNING,
            "state": state.map_or("none", state_name),
        }));
    }
    let mut drill = world.get_resource_or_insert_with(LifecycleDrill::default);
    let queued = drill.request(cycles);
    Ok(json!({ "queued": queued, "pending": drill.pending(), "completed": drill.completed() }))
}

fn parse_status(name: &str) -> Option<ThermalStatus> {
    Some(match name.to_ascii_lowercase().as_str() {
        "none" => ThermalStatus::None,
        "light" => ThermalStatus::Light,
        "moderate" => ThermalStatus::Moderate,
        "severe" => ThermalStatus::Severe,
        "critical" => ThermalStatus::Critical,
        "emergency" => ThermalStatus::Emergency,
        "shutdown" => ThermalStatus::Shutdown,
        _ => return None,
    })
}

fn reading_json(state: &ThermalState) -> Value {
    json!({ "status": state.status.token(), "headroom": state.headroom })
}

fn thermal_json(world: &World) -> Value {
    let state = world.get_resource::<ThermalState>().copied().unwrap_or_default();
    let forced = world.get_resource::<ThermalOverride>().and_then(|o| o.0);
    let governor = world.get_resource::<ThermalGovernor>().map(|g| {
        json!({
            "enabled": g.enabled,
            "level": g.level(),
            "factors": g.factors,
            "min_scale": g.min_scale,
            "relax_seconds": g.relax_seconds,
            "max_fps": g.max_fps,
            "base_render_scale": g.base_scale(),
            "holding_max_fps": g.base_max_fps().is_some(),
            "base_max_fps": g.base_max_fps().flatten(),
        })
    });
    json!({
        "reading": reading_json(&state),
        "override": forced.as_ref().map(reading_json),
        "governor": governor,
    })
}

/// `thermal.get`: the reading in effect, the override and the governor.
pub(crate) fn thermal_get(world: &mut World) -> Result<Value, RpcError> {
    Ok(thermal_json(world))
}

/// `thermal.set_override`: forces a reading (`status`, optional `headroom`) or clears it
/// (`clear: true`); returns `{ before, after }`. The governor sees it on the next frame.
pub(crate) fn thermal_set_override(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = thermal_json(world);
    let forced = if crate::params::opt_bool(params, "clear")? == Some(true) {
        None
    } else {
        let name = params
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("`status` (string) or `clear: true` is required"))?;
        let status = parse_status(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown status {name:?}: none | light | moderate | severe | critical | emergency | shutdown"
            ))
        })?;
        let headroom = match params.get("headroom") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|h| h.is_finite() && *h >= 0.0)
                    .ok_or_else(|| RpcError::invalid_params("`headroom` must be a number >= 0 or null"))?
                    as f32,
            ),
        };
        Some(ThermalState { status, headroom })
    };
    world.get_resource_or_insert_with(ThermalOverride::default).0 = forced;
    Ok(json!({ "before": before, "after": thermal_json(world) }))
}

fn rect_json(r: &TouchRect) -> Value {
    json!({ "min": [r.min.x, r.min.y], "max": [r.max.x, r.max.y] })
}

/// `input.touch_controls`: the touch zones and gesture bindings, which finger holds which zone,
/// the actions held, and the fingers down.
pub(crate) fn touch_controls(world: &mut World) -> Result<Value, RpcError> {
    let fingers: Vec<Value> = world.get_resource::<Touches>().map_or_else(Vec::new, |t| {
        t.iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "phase": format!("{:?}", p.phase),
                    "start": [p.start.x, p.start.y],
                    "current": [p.current.x, p.current.y],
                })
            })
            .collect()
    });
    let Some(controls) = world.get_resource::<TouchControls>() else {
        return Ok(json!({ "installed": false, "fingers": fingers }));
    };
    let zones: Vec<Value> = controls
        .zones
        .iter()
        .map(|z| {
            let kind = match z.kind {
                TouchControlKind::Button => json!({ "type": "button" }),
                TouchControlKind::Stick { radius_px, dead_zone } => {
                    json!({ "type": "stick", "radius_px": radius_px, "dead_zone": dead_zone })
                }
            };
            json!({ "name": z.name, "action": z.action, "rect": rect_json(&z.rect), "kind": kind })
        })
        .collect();
    let gestures: Vec<Value> = controls
        .gestures
        .iter()
        .map(|g| {
            json!({
                "gesture": g.gesture.name(),
                "action": g.action,
                "rect": g.rect.as_ref().map(rect_json),
                "frames": g.frames,
            })
        })
        .collect();
    let claims: Vec<Value> = controls
        .claims()
        .map(|(finger, zone)| {
            json!({
                "finger": finger,
                "zone": controls.zones.get(zone).map(|z| z.name.as_str()),
            })
        })
        .collect();
    Ok(json!({
        "installed": true,
        "zones": zones,
        "gestures": gestures,
        "claims": claims,
        "held_actions": controls.held_actions(),
        "fingers": fingers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<AppLifecycle>();
        world.init_resource::<LifecycleDrill>();
        world.init_resource::<SurfaceAudit>();
        world
    }

    #[test]
    fn drill_needs_a_running_app_and_a_cycle_count() {
        let mut world = world();
        let refused = lifecycle_drill(&mut world, &json!({ "cycles": 3 })).expect("answered");
        assert_eq!(refused["refused"], NOT_RUNNING);
        assert_eq!(refused["queued"], 0);
        assert_eq!(refused["state"], "not_started");
        world.resource_mut::<AppLifecycle>().state = LifecycleState::Running;
        assert!(lifecycle_drill(&mut world, &json!({ "cycles": 0 })).is_err());
        assert!(lifecycle_drill(&mut world, &json!({})).is_err());
        let got = lifecycle_drill(&mut world, &json!({ "cycles": 3 })).expect("queued");
        assert_eq!(got["queued"], 3);
        let state = lifecycle_get(&mut world).expect("get");
        assert_eq!(state["state"], "running");
        assert_eq!(state["drill"]["pending"], 3);
        assert_eq!(state["audit"]["clean"], true);
        assert_eq!(state["surfaces"], Value::Null, "no renderer");
        let got = lifecycle_drill(&mut world, &json!({ "cancel": true })).expect("cancel");
        assert_eq!(got["cancelled"], 3);
    }

    #[test]
    fn touch_controls_report_absent_and_installed() {
        let mut world = World::new();
        assert_eq!(touch_controls(&mut world).expect("get")["installed"], false);
        // `claims` and `held` are private to regia_input: set the public zones on the default resource.
        world.init_resource::<TouchControls>();
        world.resource_mut::<TouchControls>().zones = vec![regia_input::TouchZone {
            name: "move".into(),
            rect: TouchRect::left_half(),
            action: "move".into(),
            kind: TouchControlKind::Stick {
                radius_px: 80.0,
                dead_zone: 0.1,
            },
        }];
        let got = touch_controls(&mut world).expect("get");
        assert_eq!(got["zones"][0]["kind"]["type"], "stick");
        assert_eq!(got["zones"][0]["rect"]["max"], json!([0.5, 1.0]));
        assert_eq!(got["claims"], json!([]));
    }

    #[test]
    fn override_round_trip() {
        let mut world = World::new();
        world.init_resource::<ThermalState>();
        let got =
            thermal_set_override(&mut world, &json!({ "status": "severe", "headroom": 0.9 })).expect("set");
        assert_eq!(got["after"]["override"]["status"], "severe");
        assert_eq!(got["before"]["override"], Value::Null);
        assert!(thermal_set_override(&mut world, &json!({ "status": "warm" })).is_err());
        assert!(thermal_set_override(&mut world, &json!({ "status": "light", "headroom": -1 })).is_err());
        let got = thermal_set_override(&mut world, &json!({ "clear": true })).expect("clear");
        assert_eq!(got["after"]["override"], Value::Null);
        assert_eq!(thermal_get(&mut world).expect("get")["governor"], Value::Null);
    }
}
