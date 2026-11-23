// SPDX-License-Identifier: MPL-2.0
//! Agent method `net.relay_status` (`book/agents/net-relay.md`, spec 120 § 10.6, ADR-8183).
//!
//! The handler reads the relay reports a process publishes into `regia_net::NetAgentState`
//! (`NetAgentState::publish_relay`): a relay this process runs (`--relay local`, a relay tool),
//! and a host's registration with a relay (its join code, the clients the relay announced and
//! the allocations the host confirmed). It opens no socket. Without the resource or a report the
//! answer is an empty list.

use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::relay::RelayReport;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn bad_params(message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "relay_bad_params",
        message.to_owned(),
        "pass {} for every relay, or { \"relay\": \"<label>\" }",
    )
}

/// `net.relay_status`: every published relay report, or the one named by `relay`.
pub fn relay_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = match params {
        Value::Null => None,
        Value::Object(map) => {
            if let Some(key) = map.keys().find(|k| k.as_str() != "relay") {
                return Err(bad_params(&format!(
                    "net.relay_status: unknown parameter `{key}`"
                )));
            }
            match map.get("relay") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
                Some(_) => return Err(bad_params("net.relay_status: `relay` must be a non-empty string")),
            }
        }
        _ => return Err(bad_params("net.relay_status takes an object")),
    };
    let state = world.get_resource::<NetAgentState>();
    let relays = state.map(NetAgentState::relays);
    if let Some(label) = label
        && !relays.is_some_and(|r| r.contains_key(label))
    {
        let known: Vec<&str> = relays.map_or_else(Vec::new, |r| r.keys().map(String::as_str).collect());
        return Err(RpcError::new(
            NOT_FOUND,
            "relay_unknown",
            format!("no relay report named `{label}`"),
            format!("published relays: {known:?} (net.relay_status with {{}} lists them)"),
        ));
    }
    let list: Vec<Value> = relays
        .map(|r| {
            r.iter()
                .filter(|(l, _)| label.is_none_or(|t| t == l.as_str()))
                .map(|(l, report)| report_json(l, report))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "relays": list }))
}

/// One report: the host's registration, and when the process runs the relay its counters, caps,
/// refusals by reason and every allocation with its bytes each way.
pub fn report_json(label: &str, report: &RelayReport) -> Value {
    let mut out = json!({
        "label": label,
        "role": report.role,
        "relay": report.relay,
        "join_code": report.join_code,
        "relayed_clients": report.relayed_clients,
        "confirmed_allocations": report.confirmed_allocations,
        "runs_relay": report.status.is_some(),
    });
    let Value::Object(map) = &mut out else {
        return out;
    };
    // A host's NAT traversal (spec 120 § 10.7, ADR-8340): candidates, introductions, checks.
    if let Some(nat) = &report.nat {
        map.insert("nat".into(), serde_json::to_value(nat).unwrap_or(Value::Null));
    }
    if let Some(s) = &report.status {
        map.insert("protocol".into(), json!(s.protocol));
        map.insert("lobbies".into(), json!(s.lobbies));
        map.insert("allocations".into(), json!(s.allocations));
        map.insert("ports".into(), json!(s.ports));
        map.insert("ports_free".into(), json!(s.ports_free));
        map.insert("hosts".into(), json!(s.hosts));
        map.insert(
            "most_clients_of_one_host".into(),
            json!(s.most_clients_of_one_host),
        );
        map.insert("forwarded".into(), json!(s.forwarded));
        map.insert("refreshed".into(), json!(s.refreshed));
        map.insert(
            "refusals".into(),
            json!({
                "full": s.full_refused,
                "permission": s.permission_dropped,
                "refresh_key": s.refresh_refused,
                "version": s.version_refused,
                "rate_limited": s.rate_limited,
                "allocation_rate": s.forward_dropped,
                "host_rate": s.host_rate_dropped,
                "relay_rate": s.relay_rate_dropped,
            }),
        );
    }
    if let Some(c) = &report.config {
        map.insert(
            "caps".into(),
            json!({
                "allocation_ports": c.allocation_ports,
                "max_clients_per_host": c.max_clients_per_host,
                "max_allocations_per_ip": c.max_allocations_per_ip,
                "allocation_bytes_per_second": c.allocation_bytes_per_second,
                "host_bytes_per_second": c.host_bytes_per_second,
                "relay_bytes_per_second": c.relay_bytes_per_second,
            }),
        );
    }
    if report.status.is_some() {
        let rows: Vec<Value> = report
            .allocations
            .iter()
            .map(|a| {
                json!({
                    "port": a.port,
                    "host": a.host,
                    "client": a.client,
                    "bytes_to_host": a.bytes_to_host,
                    "bytes_to_client": a.bytes_to_client,
                })
            })
            .collect();
        map.insert("allocation_list".into(), Value::Array(rows));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::relay::{RelayAllocationView, RelayConfig, RelayStatus};

    fn world_with(report: Option<RelayReport>) -> World {
        let mut world = World::new();
        if let Some(r) = report {
            let mut state = NetAgentState::default();
            state.publish_relay("server_host", r);
            world.insert_resource(state);
        }
        world
    }

    fn code_of(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
    }

    #[test]
    fn no_state_answers_an_empty_list() {
        let mut world = world_with(None);
        let v = relay_status(&mut world, &json!({})).expect("answers");
        assert_eq!(v["relays"], json!([]));
        let v = relay_status(&mut world, &Value::Null).expect("null");
        assert_eq!(v["relays"], json!([]));
    }

    #[test]
    fn bad_params_and_unknown_relays_have_codes() {
        let mut world = world_with(Some(RelayReport::default()));
        for bad in [
            json!(3),
            json!({"relay": 4}),
            json!({"nope": 1}),
            json!({"relay": ""}),
        ] {
            let err = relay_status(&mut world, &bad).expect_err("refused");
            assert_eq!(code_of(&err), Some("relay_bad_params"), "{bad}");
        }
        let err = relay_status(&mut world, &json!({"relay": "other"})).expect_err("unknown");
        assert_eq!(code_of(&err), Some("relay_unknown"));
    }

    #[test]
    fn a_relay_report_lists_caps_refusals_and_allocations() {
        let report = RelayReport {
            role: "host+relay".into(),
            relay: "127.0.0.1:7800".into(),
            join_code: Some("ABCDEF".into()),
            relayed_clients: 2,
            confirmed_allocations: 2,
            status: Some(RelayStatus {
                protocol: 2,
                allocations: 2,
                full_refused: 1,
                permission_dropped: 5,
                ..RelayStatus::default()
            }),
            config: Some(RelayConfig::default()),
            allocations: vec![RelayAllocationView {
                port: 50_000,
                host: "127.0.0.1:7777".into(),
                client: "127.0.0.1:5000".into(),
                bytes_to_host: 10,
                bytes_to_client: 20,
            }],
            nat: Some(regia_net::nat::NatReport {
                candidates: vec![regia_net::nat::CandidateView {
                    kind: "host".into(),
                    addr: "192.168.1.20:7777".into(),
                }],
                sessions: 3,
                checks_sent: 12,
                checks_answered: 9,
                nominated: 2,
                not_nominated: 1,
                ..regia_net::nat::NatReport::default()
            }),
            host_check: None,
        };
        let mut world = world_with(Some(report));
        let v = relay_status(&mut world, &json!({"relay": "server_host"})).expect("answers");
        let r = &v["relays"][0];
        assert_eq!(r["label"], "server_host");
        assert_eq!(r["nat"]["candidates"][0]["kind"], "host");
        assert_eq!(r["nat"]["sessions"], 3);
        assert_eq!(r["nat"]["nominated"], 2);
        assert_eq!(r["nat"]["checks_sent"], 12);
        assert_eq!(r["allocations"], 2);
        assert_eq!(r["caps"]["max_clients_per_host"], 40);
        assert_eq!(r["refusals"]["full"], 1);
        assert_eq!(r["refusals"]["permission"], 5);
        assert_eq!(r["allocation_list"][0]["bytes_to_client"], 20);
        assert_eq!(r["join_code"], "ABCDEF");
    }
}
