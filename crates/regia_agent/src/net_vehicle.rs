// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of networked vehicles (`book/agents/net-vehicles.md`, ADR-3080):
//! `net.vehicles`.
//!
//! Errors are `F0002` with `data.reason` set to `net_not_running`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::net_vehicle::{VehicleSeatLog, net_vehicle_rows, net_vehicles_installed};
use regia_net::predict::PredictQueryError;
use serde_json::{Value, json};

use crate::net_predict::err;
use crate::protocol::RpcError;

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `net.vehicles`: every networked vehicle in this process.
pub(crate) fn vehicles(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if !net_vehicles_installed(world) {
        return Err(err(PredictQueryError::NotRunning));
    }
    let rows: Vec<Value> = net_vehicle_rows(world)
        .into_iter()
        .map(|r| {
            json!({
                "entity": r.entity.to_bits(),
                "net_id": r.net_id.map(|id| id.0),
                "tuning": r.tuning,
                "owner": r.owner,
                "mode": r.mode,
                "position": vec3(r.position),
                "rendered_position": vec3(r.rendered_position),
                "speed": r.speed,
                "gear": r.gear,
                "rpm": r.rpm,
                "last_correction_m": r.last_correction_m,
                "corrections": r.corrections,
                "snaps": r.snaps,
                "bytes_per_second": r.bytes_per_second,
                "bytes_source": r.bytes_source,
                "seats": r.seats.iter().map(|seat| seat.map(|id| id.0)).collect::<Vec<_>>(),
                "fault": r.fault,
            })
        })
        .collect();
    let log = world
        .get_resource::<VehicleSeatLog>()
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "vehicles": rows,
        "seat_requests": {
            "accepted": log.accepted,
            "refused": log.refused,
            "last_refusal": log.last_refusal,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;
    use regia_framework::net_vehicle::{
        NetVehicleTunings, VehicleNetInput, install_net_vehicle, net_vehicle_server_apply, spawn_net_vehicle,
    };
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetMode, PeerId};
    use regia_physics::PhysicsWorld;
    use regia_physics::vehicle::{VehicleConfig, drive_vehicles};
    use regia_physics::vehicle_snapshot::add_static_box;

    fn setup(world: &mut World) {
        world.insert_resource(regia_app::FixedTime::default());
        world.init_resource::<regia_physics::PhysicsSettings>();
        world.init_resource::<regia_physics::Gravity>();
        world.init_resource::<regia_physics::BodyBudget>();
        let mut physics = PhysicsWorld::default();
        add_static_box(
            &mut physics,
            Vec3::new(0.0, -1.0, 0.0),
            Vec3::new(500.0, 1.0, 500.0),
        );
        world.insert_resource(physics);
        let mut tunings = NetVehicleTunings::default();
        tunings.insert("car", VehicleConfig::default());
        world.insert_resource(tunings);
        install_net_vehicle::<VehicleNetInput>(world).expect("net vehicle");
    }

    /// The method round-trips on both sides and its error is produced.
    #[test]
    fn net_vehicles_round_trips_and_its_reason() {
        let mut bare = World::new();
        let e = vehicles(&mut bare, &json!({})).expect_err("nothing installed");
        let reason = e.data.as_ref().and_then(|d| d["reason"].as_str());
        assert_eq!(reason, Some("net_not_running"));

        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
        let a = hub.add_client();
        hub.down.delay_ticks = 2;
        hub.up.delay_ticks = 2;
        regia_net::predict::set_transport_rtt(&mut hub.clients[a].world, 5.0 * 1000.0 / 60.0, 0.0);
        hub.server.gameplay.add_systems(
            (
                net_vehicle_server_apply::<VehicleNetInput>,
                drive_vehicles,
                regia_physics::step_physics,
            )
                .chain(),
        );
        let car = spawn_net_vehicle(
            &mut hub.server.world,
            "car",
            (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
            Some(PeerId(1)),
        )
        .expect("car");
        hub.run(60);
        let r = vehicles(&mut hub.server.world, &json!({})).expect("server");
        let rows = r["vehicles"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["entity"], json!(car.to_bits()));
        assert_eq!(rows[0]["mode"], "server");
        assert_eq!(rows[0]["owner"], 1);
        assert_eq!(rows[0]["tuning"], "car");
        assert_eq!(rows[0]["bytes_source"], "codec");
        assert!(rows[0]["bytes_per_second"].as_f64().is_some_and(|b| b > 0.0));
        assert_eq!(r["seat_requests"]["refused"], 0);
        let r = vehicles(&mut hub.clients[a].world, &json!({})).expect("client");
        assert_eq!(r["vehicles"][0]["mode"], "predicted");
        assert_eq!(r["vehicles"][0]["owner"], 1);
        assert_eq!(r["vehicles"][0]["bytes_per_second"], Value::Null);
        assert_eq!(r["vehicles"][0]["fault"], Value::Null);
    }
}
