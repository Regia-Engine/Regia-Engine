// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for AI drivers (`regia_framework::vehicle_ai`, ADR-3117, specs/23 § 3).
//!
//! - `vehicle_ai.drive_to { entity, lane?, distance?, point?, reach?, loop? }` gives a vehicle a
//!   [`VehicleDriver`] (if it has none) and a target, plans the route at once and answers it.
//! - `vehicle_ai.status { entity? }` reports every driver: state, route, target and curve speed,
//!   cross-track error, gap and leader, the input it wrote, recoveries and laps.
//! - `vehicle_ai.set_params { entity, ...fields }` changes a driver's tuning.
//! - `vehicle_ai.stop { entity }` brakes the car to a stop and drops its target.
//!
//! Error codes (`data.code`) are `vehicle_ai_*`; see `book/agents/vehicle-ai.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::vehicle_ai::{DriveTarget, VehicleAiError, VehicleDriver, VehicleDriverParams};
use regia_nav::NavLaneGraph;
use regia_physics::PhysicsWorld;
use regia_physics::vehicle::{VehicleController, VehicleInput};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

const GUIDE: &str = "see agent.guide vehicle-ai (book/agents/vehicle-ai.md)";

/// A coded error: `data.code` is the reason, the message keeps it as a `[code]` prefix.
fn coded(jsonrpc: i64, code: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(jsonrpc, code, format!("[{code}] {message}"), GUIDE)
}

fn ai_error(error: &VehicleAiError) -> RpcError {
    let jsonrpc = match error {
        VehicleAiError::UnknownLane(_) | VehicleAiError::InvalidParam(_) | VehicleAiError::RouteTooLong => {
            INVALID_PARAMS
        }
        VehicleAiError::NoLaneGraph
        | VehicleAiError::NoRoute { .. }
        | VehicleAiError::OffLanes(_)
        | VehicleAiError::NoChassis => NOT_FOUND,
    };
    coded(jsonrpc, error.code(), error)
}

fn arr3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn input_json(input: VehicleInput) -> Value {
    json!({ "throttle": input.throttle, "brake": input.brake, "steer": input.steer })
}

fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let value = params.get("entity").filter(|v| !v.is_null()).ok_or_else(|| {
        RpcError::invalid_params("`entity` (a vehicle id from physics.vehicles) is required")
    })?;
    value
        .as_u64()
        .and_then(Entity::try_from_bits)
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id (a u64)"))
}

/// An optional finite number.
fn number(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| {
                coded(
                    INVALID_PARAMS,
                    "vehicle_ai_bad_param",
                    format!("`{key}` must be a finite number"),
                )
            }),
    }
}

/// An optional lane index.
fn lane_index(params: &Value, key: &str) -> Result<Option<u32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                coded(
                    INVALID_PARAMS,
                    "vehicle_ai_bad_param",
                    format!("`{key}` must be a lane index"),
                )
            }),
    }
}

/// The vehicle entity named by `entity`.
fn vehicle_of(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_param(params)?;
    if world.get::<VehicleController>(entity).is_none() {
        return Err(coded(
            NOT_FOUND,
            "vehicle_ai_not_a_vehicle",
            format!("entity {} has no VehicleController", entity.to_bits()),
        ));
    }
    Ok(entity)
}

fn target_json(target: Option<DriveTarget>) -> Value {
    match target {
        None => Value::Null,
        Some(DriveTarget::Lane { lane, distance }) => json!({ "lane": lane, "distance": distance }),
        Some(DriveTarget::Point { point, reach }) => json!({ "point": arr3(point), "reach": reach }),
        Some(DriveTarget::Loop { lane }) => json!({ "loop": lane }),
    }
}

fn driver_json(entity: Entity, driver: &VehicleDriver) -> Value {
    let s = driver.status();
    json!({
        "entity": entity.to_bits(),
        "state": s.state.name(),
        "target": target_json(driver.target()),
        "route": driver.route(),
        "path_length": driver.path_length(),
        "path_points": driver.path().len(),
        "station": s.station,
        "remaining": s.remaining,
        "speed": s.speed,
        "target_speed": s.target_speed,
        "curve_speed": s.curve_speed,
        "cross_track": s.cross_track,
        "gap": s.gap,
        "leader": s.leader.map(Entity::to_bits),
        "input": input_json(s.input),
        "recoveries": s.recoveries,
        "failed_recoveries": s.failed_recoveries,
        "laps": s.laps,
        "steps": s.steps,
        "last_error": s.last_error.as_ref().map(VehicleAiError::code),
    })
}

/// `vehicle_ai.drive_to`: sets a target and plans the route now.
pub fn drive_to(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = vehicle_of(world, params)?;
    let lane = lane_index(params, "lane")?;
    let looped = lane_index(params, "loop")?;
    let distance = number(params, "distance")?;
    let reach = number(params, "reach")?;
    let point = match params.get("point").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let xyz: Option<Vec<f32>> = v
                .as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| a.iter().map(|n| n.as_f64().map(|n| n as f32)).collect());
            match xyz {
                Some(xyz) if xyz.iter().all(|n| n.is_finite()) => Some(Vec3::new(xyz[0], xyz[1], xyz[2])),
                _ => {
                    return Err(coded(
                        INVALID_PARAMS,
                        "vehicle_ai_bad_param",
                        "`point` must be [x, y, z] of finite numbers",
                    ));
                }
            }
        }
    };
    let given = usize::from(lane.is_some()) + usize::from(point.is_some()) + usize::from(looped.is_some());
    if given != 1 {
        return Err(coded(
            INVALID_PARAMS,
            "vehicle_ai_bad_param",
            "give exactly one of `lane` (with `distance`), `point` (with `reach`) or `loop`",
        ));
    }
    let Some(graph) = world.get_resource::<NavLaneGraph>() else {
        return Err(ai_error(&VehicleAiError::NoLaneGraph));
    };
    let target = match (lane, point, looped) {
        (Some(lane), _, _) => {
            let length = graph
                .lane(lane)
                .ok_or_else(|| ai_error(&VehicleAiError::UnknownLane(lane)))?
                .length();
            DriveTarget::Lane {
                lane,
                distance: distance.unwrap_or(length),
            }
        }
        (_, Some(point), _) => {
            let reach = reach.unwrap_or(10.0);
            if reach <= 0.0 {
                return Err(coded(
                    INVALID_PARAMS,
                    "vehicle_ai_bad_param",
                    "`reach` must be > 0",
                ));
            }
            DriveTarget::Point { point, reach }
        }
        (_, _, Some(lane)) => DriveTarget::Loop { lane },
        _ => return Err(RpcError::internal("drive_to: no target after the check")),
    };
    // The chassis pose and forward speed, for planning now.
    let (position, speed) = {
        let Some(physics) = world.get_resource::<PhysicsWorld>() else {
            return Err(ai_error(&VehicleAiError::NoChassis));
        };
        let controller = world
            .get::<VehicleController>(entity)
            .ok_or_else(|| ai_error(&VehicleAiError::NoChassis))?;
        let state = controller
            .vehicle
            .state(physics)
            .map_err(|_| ai_error(&VehicleAiError::NoChassis))?;
        (state.translation, state.forward_speed)
    };
    let mut driver = world.get::<VehicleDriver>(entity).cloned().unwrap_or_default();
    driver.drive_to(target);
    let graph = world.resource::<NavLaneGraph>();
    let planned = driver.plan(graph, position, speed);
    let answer = driver_json(entity, &driver);
    // A failed plan still stores the driver (it holds the brake and reports the error).
    world.entity_mut(entity).insert(driver);
    planned.map_err(|e| ai_error(&e))?;
    Ok(answer)
}

/// `vehicle_ai.status`: every driver (or the one named by `entity`), in entity order.
pub fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("entity").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(entity_param(params)?),
    };
    let mut query = world.query::<(Entity, &VehicleDriver)>();
    let mut rows: Vec<(Entity, &VehicleDriver)> = query.iter(world).collect();
    rows.sort_by_key(|(e, _)| (e.index().index(), e.to_bits()));
    if let Some(entity) = only
        && !rows.iter().any(|(e, _)| *e == entity)
    {
        return Err(coded(
            NOT_FOUND,
            "vehicle_ai_no_driver",
            format!("entity {} has no VehicleDriver", entity.to_bits()),
        ));
    }
    let list: Vec<Value> = rows
        .into_iter()
        .filter(|(e, _)| only.is_none_or(|o| o == *e))
        .map(|(e, d)| driver_json(e, d))
        .collect();
    let count = list.len();
    Ok(json!({ "drivers": list, "count": count }))
}

fn params_json(p: &VehicleDriverParams) -> Value {
    json!({
        "flags": p.flags,
        "lateral_offset": p.lateral_offset,
        "snap_reach": p.snap_reach,
        "max_speed": p.max_speed,
        "max_lateral_accel": p.max_lateral_accel,
        "max_accel": p.max_accel,
        "max_decel": p.max_decel,
        "emergency_decel": p.emergency_decel,
        "look_ahead_min": p.look_ahead_min,
        "look_ahead_time": p.look_ahead_time,
        "look_ahead_max": p.look_ahead_max,
        "speed_preview": p.speed_preview,
        "speed_gain": p.speed_gain,
        "speed_integral": p.speed_integral,
        "time_headway": p.time_headway,
        "min_gap": p.min_gap,
        "idm_exponent": p.idm_exponent,
        "comfort_decel": p.comfort_decel,
        "follow_range": p.follow_range,
        "arrive_distance": p.arrive_distance,
        "stuck_steps": p.stuck_steps,
        "stuck_speed": p.stuck_speed,
        "reverse_steps": p.reverse_steps,
        "reverse_throttle": p.reverse_throttle,
        "max_recoveries": p.max_recoveries,
        "recovery_progress": p.recovery_progress,
    })
}

/// Applies the JSON fields of `object` to `p`; unknown fields are refused.
fn apply_params(p: &mut VehicleDriverParams, object: &Map<String, Value>) -> Result<(), RpcError> {
    for (key, value) in object {
        if key == "entity" {
            continue;
        }
        let bad = || {
            coded(
                INVALID_PARAMS,
                "vehicle_ai_bad_param",
                format!("`{key}` has the wrong type"),
            )
        };
        let float = || {
            value
                .as_f64()
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .ok_or_else(bad)
        };
        let count = || value.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(bad);
        match key.as_str() {
            "flags" => {
                p.flags = value
                    .as_u64()
                    .and_then(|v| u16::try_from(v).ok())
                    .ok_or_else(bad)?
            }
            "lateral_offset" => p.lateral_offset = float()?,
            "snap_reach" => p.snap_reach = float()?,
            "max_speed" => p.max_speed = float()?,
            "max_lateral_accel" => p.max_lateral_accel = float()?,
            "max_accel" => p.max_accel = float()?,
            "max_decel" => p.max_decel = float()?,
            "emergency_decel" => p.emergency_decel = float()?,
            "look_ahead_min" => p.look_ahead_min = float()?,
            "look_ahead_time" => p.look_ahead_time = float()?,
            "look_ahead_max" => p.look_ahead_max = float()?,
            "speed_preview" => p.speed_preview = float()?,
            "speed_gain" => p.speed_gain = float()?,
            "speed_integral" => p.speed_integral = float()?,
            "time_headway" => p.time_headway = float()?,
            "min_gap" => p.min_gap = float()?,
            "idm_exponent" => p.idm_exponent = float()?,
            "comfort_decel" => p.comfort_decel = float()?,
            "follow_range" => p.follow_range = float()?,
            "arrive_distance" => p.arrive_distance = float()?,
            "stuck_steps" => p.stuck_steps = count()?,
            "stuck_speed" => p.stuck_speed = float()?,
            "reverse_steps" => p.reverse_steps = count()?,
            "reverse_throttle" => p.reverse_throttle = float()?,
            "max_recoveries" => p.max_recoveries = count()?,
            "recovery_progress" => p.recovery_progress = float()?,
            other => {
                return Err(coded(
                    INVALID_PARAMS,
                    "vehicle_ai_bad_param",
                    format!("unknown parameter `{other}`"),
                ));
            }
        }
    }
    Ok(())
}

/// `vehicle_ai.set_params`: changes a driver's tuning (the fields given; the rest stay).
pub fn set_params(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = vehicle_of(world, params)?;
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    let mut driver = world.get::<VehicleDriver>(entity).cloned().unwrap_or_default();
    let mut next = *driver.params();
    apply_params(&mut next, object)?;
    driver.set_params(next).map_err(|e| ai_error(&e))?;
    let answer = json!({ "entity": entity.to_bits(), "params": params_json(driver.params()) });
    world.entity_mut(entity).insert(driver);
    Ok(answer)
}

/// `vehicle_ai.stop`: drops the target; the driver brakes to a stop and holds it.
pub fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = vehicle_of(world, params)?;
    let Some(mut driver) = world.get_mut::<VehicleDriver>(entity) else {
        return Err(coded(
            NOT_FOUND,
            "vehicle_ai_no_driver",
            format!("entity {} has no VehicleDriver", entity.to_bits()),
        ));
    };
    driver.stop();
    let state = driver.state().name();
    Ok(json!({ "entity": entity.to_bits(), "state": state }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_nav::NavLaneDef;
    use regia_physics::vehicle::{VehicleConfig, spawn_vehicle};

    fn world_with_car() -> (World, Entity) {
        let mut world = World::new();
        world.insert_resource(PhysicsWorld::default());
        let lanes = [
            {
                let mut l = NavLaneDef::new(&[Vec3::ZERO, Vec3::new(0.0, 0.0, -100.0)]);
                l.next = vec![1];
                l.speed = 15.0;
                l
            },
            NavLaneDef::new(&[Vec3::new(0.0, 0.0, -100.0), Vec3::new(0.0, 0.0, -200.0)]),
        ];
        world.insert_resource(NavLaneGraph::build(&lanes, 0.0).expect("graph"));
        let e = spawn_vehicle(
            &mut world,
            VehicleConfig::default(),
            Vec3::new(0.0, 0.8, -2.0),
            regia_core::math::Quat::IDENTITY,
        )
        .expect("spawn");
        (world, e)
    }

    fn code(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    #[test]
    fn drive_to_plans_and_status_reports_it() {
        let (mut world, e) = world_with_car();
        let id = e.to_bits();
        let answer =
            drive_to(&mut world, &json!({ "entity": id, "lane": 1, "distance": 50.0 })).expect("drive_to");
        assert_eq!(answer["route"], json!([0, 1]));
        assert_eq!(answer["state"], "driving");
        let status = status(&mut world, &json!({})).expect("status");
        assert_eq!(status["count"], 1);
        assert_eq!(
            status["drivers"][0]["target"],
            json!({ "lane": 1, "distance": 50.0 })
        );
        let stopped = stop(&mut world, &json!({ "entity": id })).expect("stop");
        assert_eq!(stopped["state"], "idle");
    }

    #[test]
    fn errors_carry_their_codes() {
        let (mut world, e) = world_with_car();
        let id = e.to_bits();
        let err = |r: Result<Value, RpcError>| r.err().as_ref().and_then(code);
        assert_eq!(
            err(status(&mut world, &json!({ "entity": id }))).as_deref(),
            Some("vehicle_ai_no_driver")
        );
        assert_eq!(
            err(drive_to(&mut world, &json!({ "entity": id, "lane": 9 }))).as_deref(),
            Some("vehicle_ai_unknown_lane")
        );
        assert_eq!(
            err(drive_to(&mut world, &json!({ "entity": id }))).as_deref(),
            Some("vehicle_ai_bad_param")
        );
        assert_eq!(
            err(drive_to(
                &mut world,
                &json!({ "entity": id, "point": [500.0, 0.0, 0.0], "reach": 2.0 })
            ))
            .as_deref(),
            Some("vehicle_ai_off_lanes")
        );
        assert_eq!(
            err(drive_to(
                &mut world,
                &json!({ "entity": id, "lane": 0, "distance": 1.0 })
            ))
            .as_deref(),
            Some("vehicle_ai_no_route"),
            "behind the car on a lane with no way around"
        );
        assert_eq!(
            err(set_params(
                &mut world,
                &json!({ "entity": id, "max_speed": -1.0 })
            ))
            .as_deref(),
            Some("vehicle_ai_bad_param")
        );
        assert_eq!(
            err(set_params(&mut world, &json!({ "entity": id, "max_sped": 3.0 }))).as_deref(),
            Some("vehicle_ai_bad_param")
        );
        let other = world.spawn_empty().id().to_bits();
        assert_eq!(
            err(stop(&mut world, &json!({ "entity": other }))).as_deref(),
            Some("vehicle_ai_not_a_vehicle")
        );
        world.remove_resource::<NavLaneGraph>();
        assert_eq!(
            err(drive_to(&mut world, &json!({ "entity": id, "loop": 0 }))).as_deref(),
            Some("vehicle_ai_no_lane_graph")
        );
        let changed = set_params(
            &mut world,
            &json!({ "entity": id, "max_speed": 9.0, "stuck_steps": 30 }),
        )
        .expect("set");
        assert_eq!(changed["params"]["max_speed"], 9.0);
        assert_eq!(changed["params"]["stuck_steps"], 30);
    }
}
