// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the ECS lockstep desync hash (ADR-8080, `book/agents/desync-hash-ecs.md`):
//! `sim.desync_rows`.
//!
//! It reads the rows [`regia_framework::desync_hash::DesyncHashPlugin`] records after every step:
//! one 64-bit word per named system, the last 240 steps. An agent steps two runs (or two peers) to
//! the same step, reads the rows of the first, and passes them as `against` to the second: the answer
//! names every system that split and the first step it did, with the `lockstep desync` log lines.
//!
//! ADR-8547: a diagnose ring's rows carry `raw` words (zero-sign and NaN-payload splits named at their
//! tick, `report.float_bits`), partial folds are flagged (`partial`, `report.partial_from`), and an
//! external sim behind the threaded sim host answers when the app has no ECS desync hash (`source`).

use regia_app::prelude::*;
use regia_framework::desync_hash::{
    DesyncRow, Diagnosis, MAX_TRACE_ROWS, desync_rows, diagnose_desync_rows, diagnosis_lines,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const METHOD: &str = "sim.desync_rows";

fn bad(code: &str, message: &str) -> RpcError {
    RpcError::invalid_params(format!("{METHOD}: {message} [{code}]"))
}

fn hex(word: u64) -> String {
    format!("{word:016x}")
}

fn opt_u64(params: &Value, key: &str, code: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad(code, &format!("`{key}` must be a step label (an integer >= 0)"))),
    }
}

fn label(params: &Value, key: &str, default: &'static str) -> Result<String, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(Value::String(text))
            if !text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Ok(text.clone())
        }
        Some(_) => Err(bad(
            "bad_label",
            &format!("`{key}` must be 1-64 printable ASCII characters without spaces"),
        )),
    }
}

fn hex_words(value: &Value) -> Option<Vec<u64>> {
    value
        .as_array()?
        .iter()
        .map(|w| w.as_str().and_then(|w| u64::from_str_radix(w, 16).ok()))
        .collect()
}

/// One `against` entry: `{ tick, words: [hex], raw?: [hex], partial?: bool }` (this method's own row
/// shape) or a `lockstep systems row <peer> <tick> <w:w:...>` log line.
fn parse_row(item: &Value) -> Option<DesyncRow> {
    match item {
        Value::Object(row) => {
            let tick = row.get("tick")?.as_u64()?;
            let words = hex_words(row.get("words")?)?;
            let raw = match row.get("raw") {
                None | Some(Value::Null) => None,
                Some(raw) => Some(hex_words(raw)?),
            };
            let partial = match row.get("partial") {
                None | Some(Value::Null) => false,
                Some(flag) => flag.as_bool()?,
            };
            Some(DesyncRow {
                tick,
                words,
                raw,
                partial,
            })
        }
        Value::String(line) => {
            let rest = line.split("lockstep systems row ").nth(1)?;
            let mut parts = rest.split_whitespace();
            let _peer = parts.next()?;
            let tick = parts.next()?.parse::<u64>().ok()?;
            let words = parts
                .next()?
                .split(':')
                .map(|w| u64::from_str_radix(w, 16).ok())
                .collect::<Option<Vec<u64>>>()?;
            parts.next().is_none().then_some(DesyncRow::new(tick, words))
        }
        _ => None,
    }
}

fn rows_json(rows: &[DesyncRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                let mut out = json!({
                    "tick": row.tick,
                    "words": row.words.iter().map(|w| hex(*w)).collect::<Vec<_>>(),
                });
                if let Some(raw) = &row.raw {
                    out["raw"] = json!(raw.iter().map(|w| hex(*w)).collect::<Vec<_>>());
                }
                if row.partial {
                    out["partial"] = json!(true);
                }
                out
            })
            .collect(),
    )
}

fn report_json(diagnosis: &Diagnosis) -> Value {
    let report = &diagnosis.canonical;
    let root = report.root();
    // `same` means agreement in every view (ADR-8621): no canonical split, no raw-bits split, no
    // partial row, and at least one shared tick. `verdict` says which view disagreed.
    json!({
        "same": diagnosis.same(),
        "verdict": diagnosis.verdict().as_str(),
        "root": root.map(|d| d.system),
        "first_tick": root.map(|d| d.first_tick),
        "compared": report.compared,
        "last_agreed": report.last_agreed,
        "systems": report
            .systems
            .iter()
            .map(|d| json!({ "system": d.system, "first_tick": d.first_tick }))
            .collect::<Vec<_>>(),
        "list": report.list(),
        "raw_list": diagnosis.raw.as_ref().map(|raw| raw.list()),
        "float_bits": diagnosis
            .float_bits
            .iter()
            .map(|f| json!({ "system": f.system, "raw_tick": f.raw_tick, "canonical_tick": f.canonical_tick }))
            .collect::<Vec<_>>(),
        "partial_from": diagnosis.partial_from,
        // Rows without a partner on the other side (ADR-8724): `left` is this run, `right` the
        // `against` rows; `index` is the row's position in `rows` or `against`.
        "unmatched": report
            .unmatched
            .iter()
            .map(|u| json!({ "side": u.side.as_str(), "tick": u.tick, "index": u.index, "tick_shared": u.tick_shared }))
            .collect::<Vec<_>>(),
    })
}

/// `sim.desync_rows { to?, count?, against?, split?, left?, right? }`: the recorded per-system rows
/// with `to - count < tick <= to` (default: the newest `MAX_TRACE_ROWS`); with `against` (another
/// run's rows), the systems that split and the first tick each did. The rows come from the app's
/// ECS desync hash or, without one, from the external sim behind its sim host (`source`).
pub fn sim_desync_rows(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let to = opt_u64(params, "to", "bad_to")?;
    let count = match params.get("count") {
        None | Some(Value::Null) => MAX_TRACE_ROWS,
        Some(value) => match value.as_u64() {
            Some(n) if (1..=MAX_TRACE_ROWS as u64).contains(&n) => n as usize,
            _ => {
                return Err(bad(
                    "bad_count",
                    &format!("`count` must be an integer from 1 to {MAX_TRACE_ROWS}"),
                ));
            }
        },
    };
    let split = opt_u64(params, "split", "bad_split")?;
    let left = label(params, "left", "self")?;
    let right = label(params, "right", "other")?;
    let against = match params.get("against") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() <= MAX_TRACE_ROWS => Some(
            items
                .iter()
                .map(parse_row)
                .collect::<Option<Vec<DesyncRow>>>()
                .ok_or_else(|| {
                    bad(
                        "bad_against",
                        "every `against` row must be { tick, words: [16-digit hex], raw?: [16-digit hex], partial?: bool } or a `lockstep systems row` line",
                    )
                })?,
        ),
        Some(_) => {
            return Err(bad(
                "bad_against",
                &format!("`against` must be an array of at most {MAX_TRACE_ROWS} rows"),
            ));
        }
    };
    // A tick given twice is ambiguous (which row is the other run's state?): refuse it (ADR-8621).
    // The join itself reads every row at a tick (ADR-8724).
    if let Some(rows) = &against {
        let mut ticks: Vec<u64> = rows.iter().map(|row| row.tick).collect();
        ticks.sort_unstable();
        if let Some(pair) = ticks.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(bad(
                "bad_against",
                &format!("`against` holds tick {} twice; send one row per tick", pair[0]),
            ));
        }
    }

    let Some(rows) = desync_rows(world, to, count) else {
        return Err(RpcError::not_found(
            format!("{METHOD}: this app records no desync hashes [no_desync_hash]"),
            "add regia_framework::desync_hash::DesyncHashPlugin with the game's lockstep components, or start the external sim with ThreadedSimHost::start_with_desync and insert its handle()",
        ));
    };
    let mut out = json!({
        "systems": rows.systems,
        "newest": rows.newest,
        "recorded": rows.recorded,
        "rows": rows_json(&rows.rows),
        "source": rows.source.as_str(),
        "raw": rows.raw,
        "partial_rows": rows.partial_rows,
    });
    if let Some(theirs) = against {
        let report = match diagnose_desync_rows(world, &rows.rows, &theirs) {
            Some(Ok(report)) => report,
            Some(Err(mismatch)) => {
                return Err(bad(
                    "word_count",
                    &format!(
                        "`against` row {} holds {} words; this app hashes {} systems",
                        mismatch.tick, mismatch.words, mismatch.systems
                    ),
                ));
            }
            None => return Err(RpcError::internal(format!("{METHOD}: the desync hash vanished"))),
        };
        let split = split.or(to).or(rows.newest).unwrap_or(0);
        out["report"] = report_json(&report);
        out["lines"] = json!(diagnosis_lines(split, &left, &right, &report));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
