// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of networked characters (spec 73 §§ 6.6, 6.7, 9;
//! `book/agents/net-character.md`, ADR-1905): `net.characters`.
//!
//! Errors are `F0002` with `data.reason` set to `net_not_running`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::MovementMode;
use regia_framework::net_character::{net_character_rows, net_characters_installed};
use regia_net::predict::PredictQueryError;
use serde_json::{Value, json};

use crate::net_predict::err;
use crate::protocol::RpcError;

fn mode_name(mode: MovementMode) -> String {
    match mode {
        MovementMode::Walking => "walking".into(),
        MovementMode::Falling => "falling".into(),
        MovementMode::Flying => "flying".into(),
        MovementMode::Swimming => "swimming".into(),
        MovementMode::Custom(id) => format!("custom:{id}"),
    }
}

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `net.characters`: every networked character in this process.
pub(crate) fn characters(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if !net_characters_installed(world) {
        return Err(err(PredictQueryError::NotRunning));
    }
    let rows: Vec<Value> = net_character_rows(world)
        .into_iter()
        .map(|r| {
            json!({
                "entity": r.entity.to_bits(),
                "net_id": r.net_id.map(|id| id.0),
                "role": r.role,
                "position": vec3(r.position),
                "velocity": vec3(r.velocity),
                "grounded": r.grounded,
                "mode": mode_name(r.mode),
                "base": r.base.map(|id| id.0),
                "base_local": vec3(r.base_local),
                "montage": r.montage.map(|m| json!({
                    "name": m.montage,
                    "section": m.section,
                    "start_tick": m.start_tick,
                    "rate": m.rate,
                })),
            })
        })
        .collect();
    Ok(json!({ "characters": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::character::{CharacterCollisionQuery, CharacterState, FlatPlaneQuery};
    use regia_framework::net_character::{CharacterNetInput, NetCharacter, install_net_character};
    use regia_net::replication::NetPredicted;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetMode, NetOwner, Replicated, TransformReplication};
    use regia_scene::transform::Transform;

    fn setup(world: &mut World) {
        world.insert_resource(CharacterCollisionQuery {
            query: Box::new(FlatPlaneQuery),
        });
        install_net_character::<CharacterNetInput>(world).expect("net character");
    }

    /// Spec 73 § 13 test 16: the method round-trips on both sides and its error is produced.
    #[test]
    fn net_characters_round_trips_and_its_reason() {
        let mut bare = World::new();
        let e = characters(&mut bare, &json!({})).expect_err("nothing installed");
        let reason = e.data.as_ref().and_then(|d| d["reason"].as_str());
        assert_eq!(reason, Some("net_not_running"));

        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
        let a = hub.add_client();
        hub.down.delay_ticks = 2;
        hub.up.delay_ticks = 2;
        regia_net::predict::set_transport_rtt(&mut hub.clients[a].world, 5.0 * 1000.0 / 60.0, 0.0);
        let hero = hub
            .server
            .world
            .spawn((
                Replicated {
                    transform: TransformReplication::Movement,
                    ..Replicated::default()
                },
                Transform::from_translation(Vec3::new(0.0, 1.2, 0.0)),
                NetCharacter::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                NetPredicted,
                NetOwner(hub.clients[a].peer),
            ))
            .id();
        hub.run(30);
        let r = characters(&mut hub.server.world, &json!({})).expect("server");
        let rows = r["characters"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["entity"], json!(hero.to_bits()));
        assert_eq!(rows[0]["role"], "authority");
        assert_eq!(rows[0]["mode"], "walking");
        assert_eq!(rows[0]["base"], Value::Null);
        assert_eq!(rows[0]["montage"], Value::Null);
        let r = characters(&mut hub.clients[a].world, &json!({})).expect("client");
        assert_eq!(r["characters"][0]["role"], "predicted");
        assert_eq!(r["characters"][0]["grounded"], true);
    }
}
