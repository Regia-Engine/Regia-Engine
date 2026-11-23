// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of networked rigid bodies (ADR-2902; `book/agents/net-bodies.md`):
//! `net.bodies`.
//!
//! Errors are `F0002` with `data.reason`: `net_bodies_not_installed` (no `NetBodyPlugin` on this
//! process) and `limit_out_of_range` (above 10000); a `limit` that is not a non-negative integer is
//! `F0002` with `data.param`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::net_body::{net_bodies_installed, net_body_report};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Rows `net.bodies` returns when `limit` is not given.
const DEFAULT_LIMIT: u64 = 64;
/// The largest `limit`.
const MAX_LIMIT: u64 = 10_000;

fn reason(reason: &str, message: &str, hint: &str) -> RpcError {
    let mut r = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = r.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    r
}

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `net.bodies`: counts, asleep bodies, bytes per second and the first `limit` rows.
pub(crate) fn bodies(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(DEFAULT_LIMIT);
    if limit > MAX_LIMIT {
        return Err(reason(
            "limit_out_of_range",
            "limit must be a whole number in 0..=10000",
            "omit it for 64 rows",
        ));
    }
    if !net_bodies_installed(world) {
        return Err(reason(
            "net_bodies_not_installed",
            "networked bodies are not installed on this process",
            "add regia_framework::net_body::NetBodyPlugin after NetPlugin and PhysicsPlugin on every process",
        ));
    }
    let r = net_body_report(world, limit as usize);
    let rows: Vec<Value> = r
        .rows
        .iter()
        .map(|row| {
            json!({
                "entity": row.entity.to_bits(),
                "net_id": row.net_id.map(|id| id.0),
                "role": row.role,
                "kind": row.kind,
                "asleep": row.asleep,
                "position": vec3(row.position),
                "velocity": vec3(row.velocity),
                "bytes_per_second": row.bytes_per_second,
            })
        })
        .collect();
    Ok(json!({
        "side": if r.server { "server" } else { "client" },
        "bodies": r.bodies,
        "asleep": r.asleep,
        "awake": r.bodies - r.asleep,
        "proxies": r.proxies,
        "bytes_per_second": r.bytes_per_second,
        "fell_asleep": r.stats.fell_asleep,
        "woke": r.stats.woke,
        "moved_steps": r.stats.moved_steps,
        "proxies_followed": r.stats.proxies_followed,
        "teleports": r.stats.teleports,
        "rows": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::net_body::{NetBodyShape, ReplicatedBody, install_net_body};
    use regia_scene::transform::Transform;

    fn reason_of(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The method round-trips on a world with bodies, and each error reason is produced.
    #[test]
    fn net_bodies_round_trips_and_its_reasons() {
        let mut bare = World::new();
        let e = bodies(&mut bare, &json!({})).expect_err("not installed");
        assert_eq!(reason_of(&e).as_deref(), Some("net_bodies_not_installed"));

        let mut world = World::new();
        install_net_body(&mut world);
        for x in 0..3 {
            world.spawn((
                Transform::from_translation(Vec3::new(x as f32, 0.5, 0.0)),
                ReplicatedBody::with_shape(NetBodyShape::Ball { radius: 0.5 }),
            ));
        }
        let out = bodies(&mut world, &json!({ "limit": 2 })).expect("bodies");
        assert_eq!(out["bodies"], 3);
        assert_eq!(out["rows"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["side"], "client");
        assert_eq!(out["rows"][0]["role"], "local");
        assert!(out["bytes_per_second"].is_null());

        let e = bodies(&mut world, &json!({ "limit": -1 })).expect_err("negative limit");
        assert_eq!(
            e.data.as_ref().and_then(|d| d.get("param")),
            Some(&json!("limit"))
        );
        let e = bodies(&mut world, &json!({ "limit": 10_001 })).expect_err("bad limit");
        assert_eq!(reason_of(&e).as_deref(), Some("limit_out_of_range"));
    }
}
