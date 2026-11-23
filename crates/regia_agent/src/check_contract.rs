// SPDX-License-Identifier: MPL-2.0
//! `assets.check_contract` (ADR-8401, `book/agents/delivery-contract.md`).
//!
//! The call judges one measured asset against a `.contract.ron` document. A wrong up axis is a
//! finding in the result, not a transport error.

use regia_app::prelude::*;
use regia_asset::delivery_contract::{
    DeliveryContract, DeliveryObservation, DeliveryReport, FindingSeverity, check_delivery,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

const METHOD: &str = "assets.check_contract";

fn refuse(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn hint_for(code: &str) -> &'static str {
    match code {
        "bad_contract" => "pass the .contract.ron text in contract_ron",
        "contract_parse" => {
            "fix the RON; format_version, class_name, scale, axes, origin and lod are required"
        }
        "contract_version_newer" => "this build reads delivery-contract version 1",
        "contract_invalid" => {
            "fix the settings: up and forward must name different axes, and every LOD name must be listed"
        }
        "observation_invalid" => "pass finite scales, axes and bounds; inverted bounds are refused",
        _ => "see book/agents/delivery-contract.md",
    }
}

/// Judges `observation` against the `.contract.ron` text in `contract_ron`.
///
/// `passed` is true when the report has no error finding. Warnings stay in `findings`.
pub fn check_contract(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::params::check_known_and_types(METHOD, params, &[])?;
    let Some(contract_ron) = crate::params::opt_str(params, "contract_ron")?.filter(|text| !text.is_empty())
    else {
        return Err(refuse(
            "bad_contract",
            format!("{METHOD}: `contract_ron` is required"),
            hint_for("bad_contract"),
        ));
    };
    // Version and shape of the contract are reported before a broken observation, so a newer
    // file is not hidden behind a missing measured field.
    let contract = DeliveryContract::from_ron_str(contract_ron)
        .map_err(|err| refuse(err.code(), format!("{METHOD}: {err}"), hint_for(err.code())))?;
    let Some(raw) = params.get("observation").filter(|value| !value.is_null()) else {
        return Err(refuse(
            "observation_invalid",
            format!("{METHOD}: `observation` is required"),
            hint_for("observation_invalid"),
        ));
    };
    let observation: DeliveryObservation = serde_json::from_value(raw.clone()).map_err(|err| {
        refuse(
            "observation_invalid",
            format!("{METHOD}: observation does not match the measured shape: {err}"),
            hint_for("observation_invalid"),
        )
    })?;
    let report = check_delivery(&contract, &observation)
        .map_err(|err| refuse(err.code(), format!("{METHOD}: {err}"), hint_for(err.code())))?;
    Ok(report_json(&contract.class_name, &report))
}

fn report_json(class_name: &str, report: &DeliveryReport) -> Value {
    let findings: Vec<Value> = report
        .findings
        .iter()
        .map(|finding| {
            json!({
                "code": finding.code,
                "severity": match finding.severity {
                    FindingSeverity::Error => "error",
                    FindingSeverity::Warning => "warning",
                },
                "message": finding.message,
                "asset": finding.asset,
                "subject_kind": finding.subject_kind,
                "subject": finding.subject,
                "expected": finding.expected,
                "found": finding.found,
            })
        })
        .collect();
    json!({
        "passed": report.passed(),
        "class_name": class_name,
        "error_count": report.errors().count(),
        "warning_count": report.warnings().count(),
        "findings": findings,
    })
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::World;
    use serde_json::{Value, json};

    use super::*;

    const PROP_RON: &str = "(format_version:1,class_name:\"prop\",scale:(meters_per_unit:1.0,identity_tolerance:0.001),axes:(up:pos_y,forward:pos_z),origin:(ground_offset_m:0.0,ground_tolerance_m:0.05,coarse_floor_rise_m:0.25,center_fraction:0.1),lod:(required:[\"lod0\"],budget_lod:\"lod0\",lod0_min_triangles:1,lod0_max_triangles:100000,chain:[\"lod0\"],require_decreasing:true))";

    fn call(params: Value) -> Result<Value, RpcError> {
        let mut world = World::new();
        check_contract(&mut world, &params)
    }

    fn code_of(err: &RpcError) -> Option<&str> {
        err.data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
    }

    fn sitting(up: &str) -> Value {
        json!({
            "contract_ron": PROP_RON,
            "observation": {
                "node_scales": [[1.0, 1.0, 1.0]],
                "authored_up": up,
                "bounds_min": [-0.5, 0.0, -0.5],
                "bounds_max": [0.5, 1.0, 0.5],
                "bounds_present": true,
                "coarse_lod": false,
                "lod_triangles": {"lod0": 12}
            }
        })
    }

    #[test]
    fn a_matching_up_axis_passes() {
        let report = match call(sitting("pos_y")) {
            Ok(report) => report,
            Err(err) => {
                let message = err.message;
                panic!("a valid observation must be judged: {message}");
            }
        };
        assert_eq!(report["passed"], true);
        assert_eq!(report["class_name"], "prop");
        assert_eq!(report["error_count"], 0);
        assert_eq!(report["findings"], json!([]));
    }

    #[test]
    fn a_wrong_up_axis_is_a_finding() {
        let report = match call(sitting("pos_z")) {
            Ok(report) => report,
            Err(err) => {
                let message = err.message;
                panic!("a wrong up axis is a finding, not a transport error: {message}");
            }
        };
        assert_eq!(report["passed"], false);
        assert_eq!(report["findings"][0]["code"], "wrong_up_axis");
        assert_eq!(report["error_count"], 1);
    }

    #[test]
    fn missing_params_use_the_contract_codes() {
        let missing = match call(json!({})) {
            Ok(report) => panic!("missing contract_ron must fail: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&missing), Some("bad_contract"));
        assert!(
            missing
                .data
                .as_ref()
                .and_then(|d| d.get("hint"))
                .and_then(Value::as_str)
                .is_some()
        );
        let null_params = match call(Value::Null) {
            Ok(report) => panic!("null params must fail: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&null_params), Some("bad_contract"));
        let no_observation = match call(json!({ "contract_ron": PROP_RON })) {
            Ok(report) => panic!("missing observation must fail: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&no_observation), Some("observation_invalid"));
    }

    #[test]
    fn a_newer_contract_and_a_bad_observation_are_refused() {
        let newer = PROP_RON.replace("format_version:1", "format_version:2");
        let err = match call(json!({ "contract_ron": newer, "observation": {} })) {
            Ok(report) => panic!("version 2 must be refused: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&err), Some("contract_version_newer"));
        assert!(err.message.contains("newer"), "{}", err.message);
        let bad = match call(json!({
            "contract_ron": "not ron",
            "observation": {"authored_up": "pos_y"}
        })) {
            Ok(report) => panic!("broken RON must be refused: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&bad), Some("contract_parse"));
        let nan = match call(json!({
            "contract_ron": PROP_RON,
            "observation": {
                "node_scales": [[1.0, 1.0, 1.0]],
                "authored_up": "pos_y",
                "bounds_min": [-0.5, "nope", -0.5],
                "bounds_max": [0.5, 1.0, 0.5],
                "bounds_present": true,
                "coarse_lod": false,
                "lod_triangles": {"lod0": 12}
            }
        })) {
            Ok(report) => panic!("a non-numeric bound must be refused: {report}"),
            Err(err) => err,
        };
        assert_eq!(code_of(&nan), Some("observation_invalid"));
    }

    #[test]
    fn a_missing_socket_is_a_finding() {
        let contract = PROP_RON.replace(
            "require_decreasing:true)",
            "require_decreasing:true),required_sockets:[(pattern:\"socket_mount\",count:1)]",
        );
        let report = match call(json!({
            "contract_ron": contract,
            "observation": {
                "node_scales": [[1.0, 1.0, 1.0]],
                "authored_up": "pos_y",
                "bounds_min": [-0.5, 0.0, -0.5],
                "bounds_max": [0.5, 1.0, 0.5],
                "bounds_present": true,
                "coarse_lod": false,
                "lod_triangles": {"lod0": 12},
                "asset_name": "prop.glb",
                "sockets": [{"name": "socket_other"}]
            }
        })) {
            Ok(report) => report,
            Err(err) => panic!(
                "a missing socket is a finding, not a transport error: {}",
                err.message
            ),
        };
        assert_eq!(report["passed"], false);
        assert_eq!(report["findings"][0]["code"], "missing_socket");
        assert_eq!(report["findings"][0]["subject"], "socket_mount");
        assert_eq!(report["findings"][0]["expected"], "1");
        assert_eq!(report["findings"][0]["found"], "0");
        assert_eq!(report["findings"][0]["asset"], "prop.glb");
    }

    /// Rows of the table headed `header` on `book/agents/delivery-contract.md`: the backticked
    /// names in the first cell and the text of the second ("Required") cell. Review
    /// delivery-contract-b C.
    fn book_rows(header: &str) -> Vec<(Vec<String>, String)> {
        let page = include_str!("../../../book/agents/delivery-contract.md");
        let ticked =
            |cell: &str| -> Vec<String> { cell.split('`').skip(1).step_by(2).map(str::to_string).collect() };
        let Some(start) = page.find(header) else {
            panic!("the book page has no table headed {header}");
        };
        page[start..]
            .lines()
            .skip(2)
            .take_while(|line| line.starts_with('|'))
            .map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                (ticked(cells[1]), cells[2].to_string())
            })
            .collect()
    }

    fn full_observation() -> Value {
        json!({
            "node_scales": [[1.0, 1.0, 1.0]],
            "authored_up": "pos_y",
            "authored_forward": "pos_z",
            "bounds_min": [-0.5, 0.0, -0.5],
            "bounds_max": [0.5, 1.0, 0.5],
            "bounds_present": true,
            "coarse_lod": false,
            "lod_triangles": {"lod0": 12},
            "asset_name": "prop.glb",
            "nodes": ["root"],
            "sockets": [{"name": "socket_a", "width_m": 0.5, "height_m": 0.5, "source": "extras.size"}],
            "clips": ["idle"],
            "material_count": 1,
            "material_names": ["paint"],
            "textures": [{"name": "albedo", "width": 512, "height": 512}],
            "skins": [{
                "mesh": "body", "primitive": 0, "influences": 4, "second_set": false,
                "weight_sum": null, "weight_out_of_range": false, "joint_out_of_range": false,
                "unpaired": false
            }],
            "footprint_radius_m": 0.5
        })
    }

    fn observation_refused(observation: Value) -> Option<String> {
        match call(json!({"contract_ron": PROP_RON, "observation": observation})) {
            Ok(_) => None,
            Err(err) => {
                assert_eq!(code_of(&err), Some("observation_invalid"), "{}", err.message);
                Some(err.message)
            }
        }
    }

    #[test]
    fn the_book_names_the_required_observation_fields_and_skin_keys() {
        // The review's documents: scales and axes alone are refused naming `bounds_min`, and a
        // skin row with only `influences` and `second_set` is refused naming `mesh`.
        let Some(message) = observation_refused(json!({
            "node_scales": [[1.0, 1.0, 1.0]],
            "authored_up": "pos_y"
        })) else {
            panic!("scales and axes alone must be observation_invalid");
        };
        assert!(message.contains("bounds_min"), "{message}");
        let required_only = json!({
            "node_scales": [[1.0, 1.0, 1.0]],
            "authored_up": "pos_y",
            "bounds_min": [-0.5, 0.0, -0.5],
            "bounds_max": [0.5, 1.0, 0.5],
            "bounds_present": true,
            "coarse_lod": false,
            "lod_triangles": {"lod0": 12}
        });
        assert_eq!(
            observation_refused(required_only.clone()),
            None,
            "the seven required fields load"
        );
        let mut short_skin = required_only.clone();
        short_skin["skins"] = json!([{"influences": 8, "second_set": true}]);
        let Some(message) = observation_refused(short_skin) else {
            panic!("a skin row without mesh and primitive must be observation_invalid");
        };
        assert!(message.contains("mesh"), "{message}");
        let mut skin = required_only;
        skin["skins"] = json!([{"mesh": "body", "primitive": 0, "influences": 8, "second_set": true}]);
        assert_eq!(
            observation_refused(skin),
            None,
            "mesh, primitive and influences load"
        );

        // Every top-level field: dropping it is refused exactly when the book says `yes`.
        let full = full_observation();
        assert_eq!(
            observation_refused(full.clone()),
            None,
            "the full observation loads"
        );
        let Some(object) = full.as_object() else {
            panic!("the observation is an object");
        };
        let mut book_required = std::collections::BTreeSet::new();
        let mut book_listed = std::collections::BTreeSet::new();
        for (names, required) in book_rows("| Field | Required | Meaning |") {
            for name in names {
                if required == "yes" {
                    book_required.insert(name.clone());
                }
                book_listed.insert(name);
            }
        }
        let mut measured_required = std::collections::BTreeSet::new();
        for key in object.keys() {
            assert!(book_listed.contains(key), "the book's field table lists `{key}`");
            let mut without = full.clone();
            if let Some(map) = without.as_object_mut() {
                map.remove(key);
            }
            if let Some(message) = observation_refused(without) {
                assert!(message.contains(key.as_str()), "{key}: {message}");
                measured_required.insert(key.clone());
            }
        }
        assert_eq!(book_required, measured_required, "the book's required fields");

        // Each nested row: dropping a key is refused exactly when the book lists it as required.
        for (names, required_cell) in book_rows("| Row | Required keys | Optional keys |") {
            let [row] = names.as_slice() else {
                panic!("one row name per nested-row line: {names:?}");
            };
            let book_keys: std::collections::BTreeSet<String> = required_cell
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect();
            let Some(Value::Object(sample)) = full[row.as_str()].get(0).cloned() else {
                panic!("the full observation has a `{row}` row");
            };
            let mut measured = std::collections::BTreeSet::new();
            for key in sample.keys() {
                let mut without = full.clone();
                if let Some(entry) = without[row.as_str()].get_mut(0).and_then(Value::as_object_mut) {
                    entry.remove(key);
                }
                if let Some(message) = observation_refused(without) {
                    assert!(message.contains(key.as_str()), "{row}.{key}: {message}");
                    measured.insert(key.clone());
                }
            }
            assert_eq!(book_keys, measured, "the book's required `{row}` keys");
        }
    }
}
