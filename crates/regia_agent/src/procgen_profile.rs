// SPDX-License-Identifier: MPL-2.0
//! `procgen.profile`: runs a generator's graph at chosen thread counts without publishing anything
//! and answers per-node wall times plus whether every schedule produced the same bytes (spec 62
//! § 5.1, ADR-2724, book `agents/procgen.md`).
//!
//! Errors answer `F0002` (invalid params) or `F0003` (unknown entity) with `data.reason` set to a
//! lowercase code listed on the book page.

use regia_ecs::prelude::*;
use regia_procgen::profile::{MAX_PROFILE_RUNS, profile_generator};
use regia_procgen::world::{ProcGenRegistry, ProcGenSettings, ProcGenerator};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most worker threads one profiled run may ask for.
const MAX_THREADS: u64 = 256;

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

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `threads` list: absent means `[0, 1, 2, 8]` (serial, then pools of 1, 2 and 8 workers).
fn thread_list(params: &Value) -> Result<Vec<usize>, RpcError> {
    let Some(v) = params.get("threads").filter(|v| !v.is_null()) else {
        return Ok(vec![0, 1, 2, 8]);
    };
    let items: Vec<Value> = match v {
        Value::Array(a) => a.clone(),
        n @ Value::Number(_) => vec![n.clone()],
        _ => {
            return Err(bad(
                "procgen_param_type",
                "`threads` must be a number or an array of numbers",
            ));
        }
    };
    if items.is_empty() || items.len() > MAX_PROFILE_RUNS {
        return Err(bad(
            "procgen_param_type",
            format!("`threads` takes 1 to {MAX_PROFILE_RUNS} entries"),
        ));
    }
    items
        .iter()
        .map(|t| {
            t.as_u64()
                .filter(|t| *t <= MAX_THREADS)
                .and_then(|t| usize::try_from(t).ok())
                .ok_or_else(|| {
                    bad(
                        "procgen_param_type",
                        format!("`threads` entries are integers 0..={MAX_THREADS} (0 = serial)"),
                    )
                })
        })
        .collect()
}

/// `procgen.profile { entity, threads? }` → `{ entity, runs: [{ threads, ok, output_hash, total_ms,
/// nodes: [{ id, ty, ms, elements }], error? }], identical, slowest: [{ id, ty, ms }] }`.
pub(crate) fn profile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent.procgen.profile").entered();
    let threads = thread_list(params)?;
    if !world.contains_resource::<ProcGenSettings>() || !world.contains_resource::<ProcGenRegistry>() {
        return Err(missing(
            "procgen_unavailable",
            "procedural generation is not installed (ProcGenPlugin)",
        ));
    }
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("procgen_not_generator", "`entity` (an entity id) is required"))?;
    let e = Entity::try_from_bits(bits)
        .ok_or_else(|| bad("procgen_not_generator", format!("{bits} is not an entity id")))?;
    if world.get::<ProcGenerator>(e).is_none() {
        return Err(missing(
            "procgen_not_generator",
            format!("entity {bits} has no ProcGenerator"),
        ));
    }
    let runs = profile_generator(world, e, &threads).map_err(|(code, message)| bad(&code, message))?;
    let hashes: Vec<Option<[u8; 32]>> = runs.iter().map(|r| r.output_hash).collect();
    let identical = hashes.windows(2).all(|w| w[0] == w[1]) && hashes.iter().all(Option::is_some);
    // The five slowest nodes of the first successful run.
    let mut slowest: Vec<Value> = Vec::new();
    if let Some(first) = runs.iter().find(|r| r.output_hash.is_some()) {
        let mut nodes: Vec<_> = first.nodes.iter().collect();
        nodes.sort_by(|a, b| b.ms.total_cmp(&a.ms).then(a.id.cmp(&b.id)));
        slowest = nodes
            .iter()
            .take(5)
            .map(|n| json!({ "id": n.id, "ty": n.ty, "ms": n.ms }))
            .collect();
    }
    let runs_json: Vec<Value> = runs
        .iter()
        .map(|r| {
            let mut v = json!({
                "threads": r.threads,
                "ok": r.output_hash.is_some(),
                "output_hash": r.output_hash.as_ref().map(hex),
                "total_ms": r.total_ms,
                "nodes": r.nodes.iter().map(|n| json!({ "id": n.id, "ty": n.ty, "ms": n.ms, "elements": n.elements })).collect::<Vec<_>>(),
            });
            if let (Some((node, code)), Value::Object(m)) = (&r.error, &mut v) {
                m.insert("error".into(), json!({ "node": node, "reason": code }));
            }
            v
        })
        .collect();
    Ok(json!({
        "entity": bits,
        "runs": runs_json,
        "identical": identical,
        "slowest": slowest,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_lists_default_and_reject_bad_entries() {
        assert_eq!(thread_list(&json!({})).ok(), Some(vec![0, 1, 2, 8]));
        assert_eq!(thread_list(&json!({ "threads": 4 })).ok(), Some(vec![4]));
        assert_eq!(thread_list(&json!({ "threads": [0, 3] })).ok(), Some(vec![0, 3]));
        for bad_threads in [
            json!([]),
            json!([-1]),
            json!([1000]),
            json!("8"),
            json!([0, 1, 2, 3, 4, 5, 6, 7, 8]),
        ] {
            assert!(
                thread_list(&json!({ "threads": bad_threads.clone() })).is_err(),
                "{bad_threads}"
            );
        }
    }

    #[test]
    fn profile_without_procgen_answers_procgen_unavailable() {
        let mut w = World::new();
        let e = profile(&mut w, &json!({ "entity": 1 })).err();
        let why = e.and_then(|e| e.data).and_then(|d| d.get("reason").cloned());
        assert_eq!(why, Some(json!("procgen_unavailable")));
    }
}
