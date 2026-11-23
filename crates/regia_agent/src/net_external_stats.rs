// SPDX-License-Identifier: MPL-2.0
//! Agent method `net.external_stats` (`book/agents/net-external-replication.md`, ADR-8892,
//! KI-NT-012).
//!
//! External replication (spec 120 § 8.6, ADR-8186) lives in the game's process, outside any ECS
//! world, so the engine cannot find the `ExternalReplication` value on its own. The game publishes
//! `ExternalReplication::stats()` under a label into the `regia_net::NetExternalStats` resource
//! (`ExternalReplication::publish_stats` or `NetExternalStats::publish`), and this handler reads
//! the published snapshots: entities, band radii and each client's rate, newest ack and counters,
//! plus totals over the clients. It opens no socket and changes nothing. Without the resource or a
//! snapshot the answer is an empty list.

use regia_app::prelude::*;
use regia_net::NetExternalStats;
use regia_net::external_replication::ExternalStatsEntry;
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

const METHOD: &str = "net.external_stats";

fn bad_params(message: String) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "external_bad_params",
        message,
        "pass {} for every published replication, or { \"replication\": \"<label>\" }",
    )
}

/// Reads `{}` or `{replication: label}`.
fn label(params: &Value) -> Result<Option<&str>, RpcError> {
    match params {
        Value::Null => Ok(None),
        Value::Object(map) => {
            if let Some(key) = map.keys().find(|k| k.as_str() != "replication") {
                return Err(bad_params(format!("{METHOD}: unknown parameter `{key}`")));
            }
            match map.get("replication") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
                Some(_) => Err(bad_params(format!(
                    "{METHOD}: `replication` must be a non-empty string"
                ))),
            }
        }
        _ => Err(bad_params(format!("{METHOD} takes an object"))),
    }
}

/// One published snapshot as JSON: `ExternalStats` as it serializes (`entities`, `band_radii_m`,
/// `clients`), with `label`, `tick`, `publishes` and `totals` (sums over the clients) added.
pub fn entry_json(label: &str, entry: &ExternalStatsEntry) -> Value {
    // `ExternalStats` is a serde struct of integers, floats and lists: it always writes an object.
    let mut object = match serde_json::to_value(&entry.stats) {
        Ok(Value::Object(map)) => map,
        other => Map::from_iter([("stats".to_owned(), other.unwrap_or(Value::Null))]),
    };
    let clients = &entry.stats.clients;
    let sum = |f: fn(&regia_net::external_replication::ExternalClientCounters) -> u64| -> u64 {
        clients.iter().map(|c| f(&c.counters)).sum()
    };
    let mut tier_sent = [0u64; 3];
    for c in clients {
        for (total, n) in tier_sent.iter_mut().zip(c.counters.tier_sent) {
            *total += n;
        }
    }
    let totals = json!({
        "clients": clients.len(),
        "packets": sum(|c| c.packets),
        "bytes": sum(|c| c.bytes),
        "full_packets": sum(|c| c.full_packets),
        "records": sum(|c| c.records),
        "omitted": sum(|c| c.omitted),
        "tier_held": sum(|c| c.tier_held),
        "tier_sent": tier_sent,
        "budget_skips": sum(|c| c.budget_skips),
        "acks": sum(|c| c.acks),
        "stale_acks": sum(|c| c.stale_acks),
        "relevant": sum(|c| u64::from(c.relevant)),
    });
    let mut out = Map::new();
    out.insert("label".into(), json!(label));
    out.insert("tick".into(), json!(entry.tick));
    out.insert("publishes".into(), json!(entry.publishes));
    out.append(&mut object);
    out.insert("totals".into(), totals);
    Value::Object(out)
}

/// `net.external_stats`: every published external replication snapshot, or the one named by
/// `replication`.
pub fn external_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let label = label(params)?;
    let published = world.get_resource::<NetExternalStats>();
    if let Some(label) = label
        && published.and_then(|p| p.get(label)).is_none()
    {
        let known: Vec<&str> =
            published.map_or_else(Vec::new, |p| p.entries().keys().map(String::as_str).collect());
        return Err(RpcError::new(
            NOT_FOUND,
            "external_unknown",
            format!("no external replication snapshot named `{label}`"),
            format!("published: {known:?} ({METHOD} with {{}} lists them)"),
        ));
    }
    let mut replications = Vec::new();
    if let Some(published) = published {
        for (l, entry) in published.entries() {
            if label.is_none_or(|t| t == l.as_str()) {
                replications.push(entry_json(l, entry));
            }
        }
    }
    Ok(json!({ "replications": replications }))
}

#[cfg(test)]
mod tests;
