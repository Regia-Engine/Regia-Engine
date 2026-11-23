// SPDX-License-Identifier: MPL-2.0
//! Agent method `net.host_check` (`book/agents/net-host-check.md`, spec 120 § 10.7, ADR-8370).
//!
//! The host check before hosting: the port mapping the hosting flow raced (PCP, NAT-PMP, UPnP
//! IGD), whether the relay's probe reached the mapped port, and the clients the measured uplink
//! carries at the per-client budget. The handler reads the relay reports a process publishes into
//! `regia_net::NetAgentState` (`NetAgentState::publish_relay`) and lists those that carry a host
//! check. It opens no socket and starts nothing.

use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::host_check::HostCheckReport;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn bad_params(message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "host_check_bad_params",
        message.to_owned(),
        "pass {} for every host, or { \"host\": \"<label>\" }",
    )
}

/// `net.host_check`: the host check of every published host, or of the one named by `host`.
pub fn host_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = match params {
        Value::Null => None,
        Value::Object(map) => {
            if let Some(key) = map.keys().find(|k| k.as_str() != "host") {
                return Err(bad_params(&format!("net.host_check: unknown parameter `{key}`")));
            }
            match map.get("host") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
                Some(_) => return Err(bad_params("net.host_check: `host` must be a non-empty string")),
            }
        }
        _ => return Err(bad_params("net.host_check takes an object")),
    };
    let state = world.get_resource::<NetAgentState>();
    let hosts: Vec<(&String, &HostCheckReport, Option<&String>, &String)> = state
        .map(|s| {
            s.relays()
                .iter()
                .filter_map(|(l, r)| {
                    r.host_check
                        .as_ref()
                        .map(|c| (l, c, r.join_code.as_ref(), &r.relay))
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(label) = label
        && !hosts.iter().any(|(l, ..)| l.as_str() == label)
    {
        let known: Vec<&str> = hosts.iter().map(|(l, ..)| l.as_str()).collect();
        return Err(RpcError::new(
            NOT_FOUND,
            "host_check_unknown",
            format!("no host check named `{label}`"),
            format!("hosts with a check: {known:?} (net.host_check with {{}} lists them)"),
        ));
    }
    let list: Vec<Value> = hosts
        .iter()
        .filter(|(l, ..)| label.is_none_or(|t| t == l.as_str()))
        .map(|(l, check, code, relay)| check_json(l, check, code.map(String::as_str), relay))
        .collect();
    Ok(json!({ "hosts": list }))
}

/// One host's check.
pub fn check_json(label: &str, check: &HostCheckReport, join_code: Option<&str>, relay: &str) -> Value {
    json!({
        "label": label,
        "relay": relay,
        "join_code": join_code,
        "mapping": check.mapping.as_ref().map(|m| json!({
            "mapper": m.mapper,
            "external": m.external,
            "internal_port": m.internal_port,
            "lifetime_s": m.lifetime_s,
            "renewals": m.renewals,
        })),
        "mapping_error": check.mapping_error,
        "reachable": check.reachable,
        "uplink_bytes_per_second": check.uplink_bytes_per_second,
        "uplink_end": check.uplink_end,
        "per_client_bytes_per_second": check.per_client_bytes_per_second,
        "estimated_clients": check.estimated_clients,
        "done": check.done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::host_check::MappingView;
    use regia_net::relay::RelayReport;

    fn world_with(reports: &[(&str, RelayReport)]) -> World {
        let mut world = World::new();
        let mut state = NetAgentState::default();
        for (label, report) in reports {
            state.publish_relay(label, report.clone());
        }
        world.insert_resource(state);
        world
    }

    fn code_of(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
    }

    fn checked() -> RelayReport {
        RelayReport {
            role: "host".into(),
            relay: "192.0.2.1:7800".into(),
            join_code: Some("ABCDEF".into()),
            host_check: Some(HostCheckReport {
                mapping: Some(MappingView {
                    mapper: "pcp".into(),
                    external: "198.51.100.20:7777".into(),
                    internal_port: 7777,
                    lifetime_s: 7200,
                    renewals: 1,
                }),
                mapping_error: None,
                reachable: "reachable".into(),
                uplink_bytes_per_second: Some(2_500_000),
                uplink_end: Some("full_pipe".into()),
                per_client_bytes_per_second: 262_144,
                estimated_clients: Some(9),
                done: true,
            }),
            ..RelayReport::default()
        }
    }

    #[test]
    fn hosts_with_a_check_are_listed_and_others_are_not() {
        let mut world = world_with(&[("server_host", checked()), ("relay", RelayReport::default())]);
        let v = host_check(&mut world, &json!({})).expect("answers");
        let hosts = v["hosts"].as_array().expect("a list");
        assert_eq!(hosts.len(), 1, "a relay without a check is not a host: {v}");
        let h = &hosts[0];
        assert_eq!(h["label"], "server_host");
        assert_eq!(h["join_code"], "ABCDEF");
        assert_eq!(h["mapping"]["mapper"], "pcp");
        assert_eq!(h["mapping"]["external"], "198.51.100.20:7777");
        assert_eq!(h["reachable"], "reachable");
        assert_eq!(h["uplink_bytes_per_second"], 2_500_000);
        assert_eq!(h["estimated_clients"], 9);
        assert_eq!(h["done"], true);
        let one = host_check(&mut world, &json!({"host": "server_host"})).expect("answers");
        assert_eq!(one["hosts"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn bad_params_and_unknown_hosts_have_codes() {
        let mut world = world_with(&[("server_host", checked())]);
        for bad in [
            json!(3),
            json!({"host": 4}),
            json!({"nope": 1}),
            json!({"host": ""}),
        ] {
            let err = host_check(&mut world, &bad).expect_err("refused");
            assert_eq!(code_of(&err), Some("host_check_bad_params"), "{bad}");
        }
        let err = host_check(&mut world, &json!({"host": "relay"})).expect_err("unknown");
        assert_eq!(code_of(&err), Some("host_check_unknown"));
        let mut empty = World::new();
        let v = host_check(&mut empty, &Value::Null).expect("no state");
        assert_eq!(v["hosts"], json!([]));
    }
}
