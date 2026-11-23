// SPDX-License-Identifier: MPL-2.0
//! `procgen.fingerprint`: digest of a procedural evaluation the caller already quantized
//! (`regia_procgen::fingerprint`, book `agents/procgen.md`, guide topic `procgen`, ADR-4593).
//!
//! Stateless: nothing in the world is read or written. Errors answer `F0002` with `data.reason`
//! `procgen_fingerprint_bad_params` or `procgen_fingerprint_overflow`.

use regia_ecs::prelude::*;
use regia_procgen::fingerprint::{
    EMPTY_DIGEST, EvalRecord, FINGERPRINT_CAP, FingerprintError, digest_hex, fingerprint as mix_records,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn reason(why: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": why,
            "hint": "see book/agents/procgen.md",
        })),
    }
}

fn bad(message: impl Into<String>) -> RpcError {
    reason("procgen_fingerprint_bad_params", message)
}

fn overflow() -> RpcError {
    reason(
        "procgen_fingerprint_overflow",
        format!("`records` holds more than {FINGERPRINT_CAP} entries; compare a smaller set"),
    )
}

fn answer(digest: u64, count: usize) -> Value {
    json!({
        "digest": digest_hex(digest),
        "count": count,
    })
}

fn u32_field(item: &Value, key: &str, what: &str) -> Result<u32, RpcError> {
    let Some(value) = item.get(key) else {
        return Err(bad(format!("{what} needs `{key}` (u32)")));
    };
    let Some(number) = value.as_u64() else {
        return Err(bad(format!("{what}.{key} must be a non-negative integer")));
    };
    u32::try_from(number).map_err(|_| bad(format!("{what}.{key} is outside u32")))
}

fn i32_at(value: &Value, what: &str) -> Result<i32, RpcError> {
    let Some(number) = value.as_i64() else {
        return Err(bad(format!("{what} must be an integer")));
    };
    i32::try_from(number).map_err(|_| bad(format!("{what} is outside i32")))
}

fn parse_record(item: &Value, index: usize) -> Result<EvalRecord, RpcError> {
    let what = format!("records[{index}]");
    if !item.is_object() {
        return Err(bad(format!("{what} must be an object")));
    }
    let node_id = u32_field(item, "node", &what)?;
    let instance_index = u32_field(item, "instance", &what)?;
    let Some(yaw) = item.get("yaw_mdeg") else {
        return Err(bad(format!("{what} needs `yaw_mdeg` (i32 millidegrees)")));
    };
    let yaw_mdeg = i32_at(yaw, &format!("{what}.yaw_mdeg"))?;
    let position = item.get("position_mm").and_then(Value::as_array).ok_or_else(|| {
        bad(format!(
            "{what}.position_mm must be three millimeter integers [x, y, z]"
        ))
    })?;
    if position.len() != 3 {
        return Err(bad(format!(
            "{what}.position_mm must be three millimeter integers [x, y, z]"
        )));
    }
    Ok(EvalRecord {
        node_id,
        instance_index,
        position_mm: [
            i32_at(&position[0], &format!("{what}.position_mm[0]"))?,
            i32_at(&position[1], &format!("{what}.position_mm[1]"))?,
            i32_at(&position[2], &format!("{what}.position_mm[2]"))?,
        ],
        yaw_mdeg,
    })
}

/// `procgen.fingerprint { records? }`: digest of already-quantized instances.
///
/// `records` defaults to an empty list, whose digest is [`EMPTY_DIGEST`]. Each record is
/// `{ node, instance, position_mm: [x, y, z], yaw_mdeg }`. The answer is `{ digest, count }` with
/// `digest` sixteen lowercase hex digits. A list longer than [`FINGERPRINT_CAP`] answers
/// `procgen_fingerprint_overflow` before any record is parsed and before the mix buffer exists.
pub(crate) fn fingerprint(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent.procgen.fingerprint").entered();
    let list = match params.get("records") {
        None | Some(Value::Null) => return Ok(answer(EMPTY_DIGEST, 0)),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(bad("`records` must be an array")),
    };
    if list.len() > FINGERPRINT_CAP {
        return Err(overflow());
    }
    let mut records = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        records.push(parse_record(item, index)?);
    }
    match mix_records(&records) {
        Ok(digest) => Ok(answer(digest, records.len())),
        Err(FingerprintError::Overflow) => Err(overflow()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason_of(error: &RpcError) -> Option<&str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
    }

    fn call(params: &Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        crate::dispatch::call_in_process(&mut world, "procgen.fingerprint", params.clone())
            .expect("fingerprint replies synchronously")
    }

    #[test]
    fn procgen_fingerprint_orders_and_one_millimeter() {
        let first = json!({
            "records": [
                { "node": 2, "instance": 0, "position_mm": [10, 0, 0], "yaw_mdeg": 0 },
                { "node": 1, "instance": 4, "position_mm": [0, -3, 8], "yaw_mdeg": 90000 }
            ]
        });
        let reversed = json!({
            "records": [
                { "node": 1, "instance": 4, "position_mm": [0, -3, 8], "yaw_mdeg": 90000 },
                { "node": 2, "instance": 0, "position_mm": [10, 0, 0], "yaw_mdeg": 0 }
            ]
        });
        let moved = json!({
            "records": [
                { "node": 2, "instance": 0, "position_mm": [11, 0, 0], "yaw_mdeg": 0 },
                { "node": 1, "instance": 4, "position_mm": [0, -3, 8], "yaw_mdeg": 90000 }
            ]
        });
        let a = call(&first).expect("digest");
        let b = call(&reversed).expect("same records");
        assert_eq!(
            a["digest"], b["digest"],
            "procgen.fingerprint ignores input order"
        );
        assert_eq!(a["count"], 2);
        assert_ne!(call(&moved).expect("moved")["digest"], a["digest"]);
    }

    #[test]
    fn procgen_fingerprint_empty_is_stable() {
        let once = call(&json!({})).expect("omitted records");
        let twice = call(&json!({ "records": [] })).expect("empty records");
        assert_eq!(once, twice);
        assert_eq!(once["digest"], digest_hex(EMPTY_DIGEST));
        assert_eq!(once["count"], 0);
    }

    #[test]
    fn procgen_fingerprint_overflow_before_mix() {
        let records = vec![Value::Null; FINGERPRINT_CAP + 1];
        let error = call(&json!({ "records": records })).expect_err("one past the cap");
        assert_eq!(reason_of(&error), Some("procgen_fingerprint_overflow"));
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str),
            Some("F0002")
        );
    }

    #[test]
    fn procgen_fingerprint_integer_boundaries_and_reference_digest() {
        let result = call(&json!({ "records": [{
            "node": u32::MAX, "instance": 0,
            "position_mm": [i32::MIN, i32::MAX, -1], "yaw_mdeg": i32::MIN
        }] }))
        .expect("all integer boundaries are valid");
        assert_eq!(result, json!({ "digest": "8c434496344e7f61", "count": 1 }));

        for (field, value) in [
            ("node", json!(u64::from(u32::MAX) + 1)),
            ("instance", json!(-1)),
            ("yaw_mdeg", json!(i64::from(i32::MIN) - 1)),
            ("yaw_mdeg", json!(1.5)),
            ("position_mm", json!([0, 0, i64::from(i32::MAX) + 1])),
        ] {
            let mut record = json!({
                "node": 0, "instance": 0, "position_mm": [0, 0, 0], "yaw_mdeg": 0
            });
            record[field] = value;
            let error = call(&json!({ "records": [record] })).expect_err("invalid integer");
            assert_eq!(reason_of(&error), Some("procgen_fingerprint_bad_params"));
        }
    }

    #[test]
    fn procgen_fingerprint_rejects_a_bad_record() {
        let error = call(&json!({
            "records": [{ "node": 1, "instance": 0, "position_mm": [0, 0], "yaw_mdeg": 0 }]
        }))
        .expect_err("two components");
        assert_eq!(reason_of(&error), Some("procgen_fingerprint_bad_params"));
        let error = call(&json!({ "records": 4 })).expect_err("not an array");
        assert_eq!(reason_of(&error), Some("procgen_fingerprint_bad_params"));
        let error = call(&json!({
            "records": [{ "node": -1, "instance": 0, "position_mm": [0, 0, 0], "yaw_mdeg": 0 }]
        }))
        .expect_err("negative node");
        assert_eq!(reason_of(&error), Some("procgen_fingerprint_bad_params"));
    }
}
