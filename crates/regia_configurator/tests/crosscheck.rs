// SPDX-License-Identifier: MPL-2.0
//! Regia's JSON Schema validator against a reference implementation (ADR-2480).
//!
//! `tests/crosscheck.jsonl` holds mutations of the committed fixtures (values swapped for other types,
//! extremes, empty strings, traversal paths; fields deleted; an unknown field added) with the verdict of
//! python-jsonschema (2020-12) on each, recorded by a generator outside the repo. The kit builder validates
//! with that library, so the two must agree on every case: schema only, no contract checks.

use regia_configurator::Interface;
use serde_json::Value;

#[test]
fn schema_verdicts_match_the_reference_validator() {
    let text = include_str!("crosscheck.jsonl");
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap_or_default()).expect("header");
    let mut checked = 0;
    let mut valid = 0;
    let mut disagree = Vec::new();
    for (i, line) in lines.enumerate() {
        let case: Value = serde_json::from_str(line).expect("case");
        let wire = case["interface"].as_str().unwrap_or_default();
        let interface = Interface::from_wire_name(wire).expect("interface");
        let schema = interface.schema().expect("schema");
        let want = case["valid"].as_bool().expect("verdict");
        let got = schema.validate(&case["doc"]);
        if got.is_empty() != want {
            disagree.push(format!(
                "case {i} ({wire}): reference valid={want}, regia {got:?}"
            ));
        }
        checked += 1;
        valid += usize::from(want);
    }
    assert_eq!(header["cases"].as_u64(), Some(checked as u64), "case count");
    assert!(
        valid > 0 && valid < checked,
        "the corpus mixes valid and invalid cases"
    );
    assert!(
        disagree.is_empty(),
        "{} disagreements:\n{}",
        disagree.len(),
        disagree.join("\n")
    );
}
