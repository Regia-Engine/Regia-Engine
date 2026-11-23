// SPDX-License-Identifier: MPL-2.0
//! Acceptance: an authored up axis other than the contract up axis fails.

use std::collections::BTreeMap;

use regia_asset::delivery_contract::{
    AxisSettings, DeliveryContract, DeliveryObservation, DeliveryReport, FootprintRule, LodCap, LodRatio,
    LodSettings, MeasuredSkin, MeasuredSocket, MeasuredTexture, NameCount, OriginSettings, ScaleSettings,
    SignedAxis, SocketClearance, check_delivery, socket_opening_for_radius, write_vendor_report,
};

fn prop_contract() -> DeliveryContract {
    DeliveryContract {
        format_version: 1,
        class_name: "prop".into(),
        scale: ScaleSettings {
            meters_per_unit: 1.0,
            identity_tolerance: 1e-3,
        },
        axes: AxisSettings {
            up: SignedAxis::PosY,
            forward: SignedAxis::PosZ,
        },
        origin: OriginSettings {
            ground_offset_m: 0.0,
            ground_tolerance_m: 0.05,
            coarse_floor_rise_m: 0.25,
            center_fraction: 0.1,
        },
        lod: LodSettings {
            required: vec!["lod0".into()],
            optional: vec![],
            budget_lod: "lod0".into(),
            lod0_min_triangles: 1,
            lod0_max_triangles: 100_000,
            chain: vec!["lod0".into()],
            ratios: vec![],
            hard_caps: vec![],
            require_decreasing: true,
        },
        max_joint_influences: 4,
        required_nodes: vec![],
        required_sockets: vec![],
        required_clips: vec![],
        max_materials: 4,
        material_warn_above: 3,
        texture_max_edge: 2048,
        footprint: None,
        socket_clearance: vec![],
    }
}

fn sitting_prop() -> DeliveryObservation {
    let mut lod_triangles = BTreeMap::new();
    lod_triangles.insert("lod0".into(), 12);
    DeliveryObservation {
        node_scales: vec![[1.0, 1.0, 1.0]],
        authored_up: SignedAxis::PosY,
        authored_forward: None,
        bounds_min: [-0.5, 0.0, -0.5],
        bounds_max: [0.5, 1.0, 0.5],
        bounds_present: true,
        coarse_lod: false,
        lod_triangles,
        ..DeliveryObservation::default()
    }
}

fn error_codes(report: &DeliveryReport) -> Vec<&str> {
    report.errors().map(|finding| finding.code).collect()
}

#[test]
fn wrong_up_axis_fails_contract() {
    let contract = prop_contract();
    let mut observation = sitting_prop();
    observation.authored_up = SignedAxis::PosZ;
    let report = match check_delivery(&contract, &observation) {
        Ok(report) => report,
        Err(err) => panic!("a finite observation must be judged: {err}"),
    };
    let codes = error_codes(&report);
    assert!(
        codes.contains(&"wrong_up_axis"),
        "an authored up axis that is not the contract up axis must fail; findings: {codes:?} {}",
        report
            .findings
            .iter()
            .map(|finding| finding.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    );
}

fn judge(contract: &DeliveryContract, observation: &DeliveryObservation) -> DeliveryReport {
    match check_delivery(contract, observation) {
        Ok(report) => report,
        Err(err) => panic!("a finite observation must be judged: {err}"),
    }
}

fn has(report: &DeliveryReport, code: &str) -> bool {
    report.findings.iter().any(|finding| finding.code == code)
}

#[test]
fn a_matching_up_axis_passes() {
    let report = judge(&prop_contract(), &sitting_prop());
    assert!(report.passed(), "{:?}", error_codes(&report));
    assert!(!has(&report, "wrong_up_axis"));
    assert!(!has(&report, "nose_axis"));
}

#[test]
fn nose_warning_is_not_an_up_axis_failure() {
    let mut observation = sitting_prop();
    observation.bounds_min[2] = -0.05;
    observation.bounds_max[2] = 0.0;
    let report = judge(&prop_contract(), &observation);
    assert!(report.passed(), "{:?}", error_codes(&report));
    assert!(has(&report, "nose_axis"));
    assert!(!has(&report, "wrong_up_axis"));
}

#[test]
fn a_wrong_forward_axis_fails_when_one_was_authored() {
    let mut observation = sitting_prop();
    observation.authored_forward = Some(SignedAxis::PosX);
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "wrong_forward_axis"));
    assert!(!has(&report, "wrong_up_axis"));
}

#[test]
fn ground_follows_the_contract_up_axis() {
    let mut contract = prop_contract();
    contract.axes.up = SignedAxis::PosZ;
    contract.axes.forward = SignedAxis::PosX;
    let mut observation = sitting_prop();
    observation.authored_up = SignedAxis::PosZ;
    observation.bounds_min = [-0.5, -0.5, 0.0];
    observation.bounds_max = [0.5, 0.5, 1.0];
    let report = judge(&contract, &observation);
    assert!(!has(&report, "origin_ground"), "{:?}", error_codes(&report));
    assert!(!has(&report, "wrong_up_axis"));
}

#[test]
fn scale_ground_and_center_failures_are_errors() {
    let mut observation = sitting_prop();
    observation.node_scales = vec![[1.002, 1.0, 1.0]];
    observation.bounds_min[1] = 1.0;
    observation.bounds_min[0] = -0.39;
    observation.bounds_max[0] = 0.61;
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "unit_scale"));
    assert!(has(&report, "origin_ground"));
    assert!(has(&report, "origin_center"));
    assert!(!report.passed());
}

#[test]
fn a_scale_inside_tolerance_and_a_floor_on_the_boundary_pass() {
    let mut observation = sitting_prop();
    observation.node_scales = vec![[1.0005, 1.0, 1.0]];
    observation.bounds_min[1] = -0.05;
    let report = judge(&prop_contract(), &observation);
    assert!(!has(&report, "unit_scale"));
    assert!(!has(&report, "origin_ground"));
}

#[test]
fn a_coarse_floor_may_sit_higher() {
    let mut low = sitting_prop();
    low.coarse_lod = true;
    low.bounds_min[1] = 0.2;
    assert!(!has(&judge(&prop_contract(), &low), "origin_ground"));
    let mut high = low.clone();
    high.bounds_min[1] = 0.26;
    assert!(has(&judge(&prop_contract(), &high), "origin_ground"));
}

#[test]
fn lod_chain_budget_ratio_and_cap() {
    let mut contract = prop_contract();
    contract.lod.optional = vec!["lod1".into()];
    contract.lod.chain = vec!["lod0".into(), "lod1".into()];
    contract.lod.ratios = vec![LodRatio {
        name: "lod1".into(),
        of: "lod0".into(),
        nominal: 0.5,
    }];
    contract.lod.hard_caps = vec![LodCap {
        name: "lod1".into(),
        max_triangles_exclusive: 10,
    }];
    contract.lod.require_decreasing = false;
    let mut observation = sitting_prop();
    observation.lod_triangles.insert("lod1".into(), 101);
    let over = judge(&contract, &observation);
    assert!(has(&over, "lod_ratio"));
    assert!(has(&over, "lod_cap"));
    observation.lod_triangles.insert("lod1".into(), 9);
    let inside = judge(&contract, &observation);
    assert!(!has(&inside, "lod_ratio"), "9/12 is inside half to double of 0.5");
    assert!(!has(&inside, "lod_cap"));
    contract.lod.require_decreasing = true;
    observation.lod_triangles.insert("lod0".into(), 12);
    observation.lod_triangles.insert("lod1".into(), 12);
    assert!(has(&judge(&contract, &observation), "lod_chain"));
}

#[test]
fn a_missing_budget_lod_skips_caps() {
    let mut contract = prop_contract();
    contract.lod.required = vec!["lod1".into()];
    contract.lod.optional = vec!["lod0".into()];
    contract.lod.budget_lod = "lod0".into();
    contract.lod.hard_caps = vec![LodCap {
        name: "lod1".into(),
        max_triangles_exclusive: 1,
    }];
    let mut observation = sitting_prop();
    observation.lod_triangles.clear();
    observation.lod_triangles.insert("lod1".into(), 100);
    let report = judge(&contract, &observation);
    assert!(!has(&report, "lod_cap"));
    assert!(!has(&report, "lod0_budget"));
    assert!(!has(&report, "lod_presence"));
}

#[test]
fn a_missing_required_lod_is_reported() {
    let mut observation = sitting_prop();
    observation.lod_triangles.clear();
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "lod_presence"));
    assert!(has(&report, "lod_count"));
    assert!(!has(&report, "lod0_budget"));
}

#[test]
fn an_extra_lod_fails_the_count() {
    let mut observation = sitting_prop();
    observation.lod_triangles.insert("lod9".into(), 4);
    assert!(has(&judge(&prop_contract(), &observation), "lod_count"));
}

#[test]
fn the_budget_lod_must_sit_in_range() {
    let mut observation = sitting_prop();
    observation.lod_triangles.insert("lod0".into(), 0);
    assert!(has(&judge(&prop_contract(), &observation), "lod0_budget"));
}

#[test]
fn a_ratio_above_two_to_the_53_is_not_compared() {
    let huge = (1_u64 << 53) + 1;
    let mut contract = prop_contract();
    contract.lod.require_decreasing = false;
    contract.lod.lod0_max_triangles = huge;
    contract.lod.optional = vec!["lod1".into()];
    contract.lod.ratios = vec![LodRatio {
        name: "lod1".into(),
        of: "lod0".into(),
        nominal: 0.5,
    }];
    let mut observation = sitting_prop();
    observation.lod_triangles.insert("lod0".into(), huge);
    observation.lod_triangles.insert("lod1".into(), huge);
    let report = judge(&contract, &observation);
    assert!(has(&report, "lod_ratio"));
    assert!(report.passed(), "{:?}", error_codes(&report));
}

#[test]
fn non_finite_and_inverted_bounds_are_refused() {
    let mut observation = sitting_prop();
    observation.node_scales = vec![[f64::NAN, 1.0, 1.0]];
    match check_delivery(&prop_contract(), &observation) {
        Err(err) => assert_eq!(err.code(), "observation_invalid"),
        Ok(_) => panic!("NaN must be refused"),
    }
    observation = sitting_prop();
    observation.bounds_max[0] = -1.0;
    match check_delivery(&prop_contract(), &observation) {
        Err(err) => assert_eq!(err.code(), "observation_invalid"),
        Ok(_) => panic!("inverted bounds must be refused"),
    }
}

#[test]
fn parallel_axes_are_an_invalid_contract() {
    let mut contract = prop_contract();
    contract.axes.forward = SignedAxis::NegY;
    match check_delivery(&contract, &sitting_prop()) {
        Err(err) => assert_eq!(err.code(), "contract_invalid"),
        Ok(_) => panic!("parallel up and forward must be refused"),
    }
}

#[test]
fn ron_round_trip_and_version_refusal() {
    let contract = prop_contract();
    let text = match contract.to_ron_string() {
        Ok(text) => text,
        Err(err) => panic!("{err}"),
    };
    let again = match DeliveryContract::from_ron_str(&text) {
        Ok(again) => again,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(again, contract);
    let twice = match again.to_ron_string() {
        Ok(text) => text,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(twice, text);
    let zero = text.replace("format_version: 1", "format_version: 0");
    match DeliveryContract::from_ron_str(&zero) {
        Err(err) => assert_eq!(err.code(), "contract_invalid"),
        Ok(_) => panic!("format_version 0 must be refused"),
    }
    let newer = text.replace("format_version: 1", "format_version: 2");
    match DeliveryContract::from_ron_str(&newer) {
        Err(err) => {
            assert_eq!(err.code(), "contract_version_newer");
            let message = err.to_string();
            assert!(message.contains("newer"), "{message}");
            assert!(message.contains("format_version 2"), "{message}");
        }
        Ok(_) => panic!("format_version 2 must be refused"),
    }
}

fn report_text(asset: &str, report: &DeliveryReport) -> (String, String) {
    let dir = regia_test_utils::unique_temp_dir("delivery-contract-report");
    write_vendor_report(&dir, asset, report).expect("report writes");
    let markdown = std::fs::read_to_string(dir.join("report.md")).expect("markdown");
    let json = std::fs::read_to_string(dir.join("report.json")).expect("json");
    let _ = std::fs::remove_dir_all(&dir);
    (markdown, json)
}

#[test]
fn missing_socket_named_in_report() {
    let mut contract = prop_contract();
    contract.required_sockets = vec![NameCount {
        pattern: "socket_mount".into(),
        count: 1,
    }];
    let mut observation = sitting_prop();
    observation.asset_name = "prop.glb".into();
    observation.sockets = vec![MeasuredSocket {
        name: "socket_other".into(),
        width_m: None,
        height_m: None,
        source: String::new(),
    }];
    let report = judge(&contract, &observation);
    assert!(has(&report, "missing_socket"), "{:?}", error_codes(&report));
    let (markdown, json) = report_text("prop.glb", &report);
    assert!(
        markdown.contains("socket_mount"),
        "the vendor report must name the missing socket:\n{markdown}"
    );
    assert!(
        json.contains("socket_mount"),
        "the json report must name the missing socket:\n{json}"
    );
    assert!(json.contains("\"expected\": \"1\""));
    assert!(json.contains("\"found\": \"0\""));
    assert!(markdown.contains("prop.glb"));
}

#[test]
fn five_influences_fail_the_contract() {
    let mut observation = sitting_prop();
    observation.skins = vec![MeasuredSkin {
        mesh: "body".into(),
        primitive: 0,
        influences: 5,
        second_set: true,
        weight_sum: None,
        weight_out_of_range: false,
        joint_out_of_range: false,
        unpaired: false,
    }];
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "joint_influences"), "{:?}", error_codes(&report));
    assert!(!report.passed());
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "joint_influences")
        .expect("finding");
    assert_eq!(finding.expected, "4");
    assert_eq!(finding.found, "5");
    assert_eq!(finding.subject, "body");
}

#[test]
fn a_missing_clip_is_named() {
    let mut contract = prop_contract();
    contract.required_clips = vec![NameCount {
        pattern: "hatch_open".into(),
        count: 1,
    }];
    let mut observation = sitting_prop();
    observation.clips = vec!["idle".into()];
    let report = judge(&contract, &observation);
    assert!(has(&report, "missing_clip"));
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "missing_clip")
        .expect("finding");
    assert_eq!(finding.subject, "hatch_open");
    assert!(finding.message.contains("hatch_open"));
}

#[test]
fn material_count_over_cap_fails() {
    let mut observation = sitting_prop();
    observation.material_count = 5;
    observation.material_names = vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()];
    let over = judge(&prop_contract(), &observation);
    assert!(has(&over, "material_count"));
    assert!(!over.passed());
    let finding = over
        .findings
        .iter()
        .find(|finding| finding.code == "material_count")
        .expect("finding");
    assert_eq!(finding.expected, "4");
    assert_eq!(finding.found, "5");
    observation.material_count = 4;
    observation.material_names.pop();
    let warned = judge(&prop_contract(), &observation);
    assert!(warned.passed(), "four materials are inside the cap of 4");
    assert!(
        has(&warned, "material_count"),
        "four materials warn above the nominal 3"
    );
    observation.material_count = 3;
    observation.material_names.pop();
    let inside = judge(&prop_contract(), &observation);
    assert!(!has(&inside, "material_count"));
}

#[test]
fn unnormalized_weights_and_a_second_influence_set_fail() {
    let mut observation = sitting_prop();
    observation.skins = vec![MeasuredSkin {
        mesh: "body".into(),
        primitive: 0,
        influences: 4,
        second_set: false,
        weight_sum: Some(0.5),
        weight_out_of_range: false,
        joint_out_of_range: false,
        unpaired: false,
    }];
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "joint_weights"));
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.message.contains("0.500"))
    );
}

#[test]
fn a_texture_over_the_edge_cap_fails() {
    let mut observation = sitting_prop();
    observation.textures = vec![MeasuredTexture {
        name: "body".into(),
        width: Some(4096),
        height: Some(2048),
    }];
    let report = judge(&prop_contract(), &observation);
    assert!(has(&report, "texture_edge"));
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "texture_edge")
        .expect("finding");
    assert_eq!(finding.subject, "body");
    assert_eq!(finding.expected, "2048");
    assert_eq!(finding.found, "4096");
    observation.textures[0].width = None;
    let unreadable = judge(&prop_contract(), &observation);
    assert!(unreadable.passed());
    assert!(has(&unreadable, "texture_unreadable"));
}

#[test]
fn an_opening_at_the_minimum_is_clear_and_a_narrow_one_is_not() {
    let mut contract = prop_contract();
    contract.socket_clearance = vec![SocketClearance {
        pattern: "socket_hatch*".into(),
        min_width_m: 0.9,
        min_height_m: 1.2,
    }];
    let socket = |width: Option<f64>, height: Option<f64>| MeasuredSocket {
        name: "socket_hatch_main".into(),
        width_m: width,
        height_m: height,
        source: "extras.size".into(),
    };
    let mut observation = sitting_prop();
    observation.sockets = vec![socket(Some(0.9), Some(1.2))];
    assert!(!has(&judge(&contract, &observation), "socket_opening"));
    observation.sockets = vec![socket(Some(0.8), Some(1.5))];
    assert!(has(&judge(&contract, &observation), "socket_opening"));
    observation.sockets = vec![socket(Some(1.2), Some(1.0))];
    assert!(has(&judge(&contract, &observation), "socket_opening"));
    observation.sockets = vec![socket(None, None)];
    let closed = judge(&contract, &observation);
    assert!(has(&closed, "socket_opening"));
    assert!(
        closed
            .findings
            .iter()
            .any(|finding| finding.message.contains("fails closed"))
    );
    observation.sockets.clear();
    assert!(
        !has(&judge(&contract, &observation), "socket_opening"),
        "no matching socket skips the opening check"
    );
}

#[test]
fn footprint_within_ten_percent_passes_and_the_report_names_the_suggestion() {
    let mut contract = prop_contract();
    contract.footprint = Some(FootprintRule {
        radius_m: 2.9,
        tolerance: 0.10,
    });
    let mut observation = sitting_prop();
    let measured = 2.828_427_124_746_190_3_f64;
    observation.footprint_radius_m = Some(measured);
    assert!(!has(&judge(&contract, &observation), "footprint_radius"));
    contract.footprint = Some(FootprintRule {
        radius_m: 2.0,
        tolerance: 0.10,
    });
    let report = judge(&contract, &observation);
    assert!(has(&report, "footprint_radius"));
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "footprint_radius")
        .expect("finding");
    assert!(finding.message.contains("2.9"), "{}", finding.message);
    assert!(finding.found.starts_with("2.828"));
    observation.footprint_radius_m = Some(0.0);
    assert!(has(&judge(&contract, &observation), "footprint_radius"));
}

#[test]
fn opening_for_a_radius_is_the_diameter_plus_clearance() {
    let required = socket_opening_for_radius(2.0, 1.0);
    assert!((required - 5.0).abs() < 1e-9, "{required}");
    let mut contract = prop_contract();
    contract.socket_clearance = vec![SocketClearance {
        pattern: "socket_door*".into(),
        min_width_m: required,
        min_height_m: required,
    }];
    contract.required_sockets = vec![NameCount {
        pattern: "socket_door*".into(),
        count: 1,
    }];
    let mut open = sitting_prop();
    open.sockets = vec![MeasuredSocket {
        name: "socket_door_bay".into(),
        width_m: Some(7.0),
        height_m: Some(7.0),
        source: "extras.size".into(),
    }];
    assert!(
        judge(&contract, &open).passed(),
        "{:?}",
        error_codes(&judge(&contract, &open))
    );
    open.sockets[0].width_m = Some(3.0);
    open.sockets[0].height_m = Some(3.0);
    assert!(has(&judge(&contract, &open), "socket_opening"));
    open.sockets.clear();
    let missing = judge(&contract, &open);
    assert!(has(&missing, "missing_socket"));
    assert!(
        missing
            .findings
            .iter()
            .any(|finding| finding.subject == "socket_door*")
    );
}

#[test]
fn a_missing_node_is_named() {
    let mut contract = prop_contract();
    contract.required_nodes = vec![NameCount {
        pattern: "root".into(),
        count: 1,
    }];
    let mut observation = sitting_prop();
    observation.nodes = vec!["body".into()];
    let report = judge(&contract, &observation);
    assert!(has(&report, "missing_node"));
    assert!(report.findings.iter().any(|finding| finding.subject == "root"));
}

#[test]
fn a_v1_file_without_kit_fields_still_loads() {
    let text = r#"(
        format_version: 1,
        class_name: "prop",
        scale: (meters_per_unit: 1.0, identity_tolerance: 0.001),
        axes: (up: pos_y, forward: pos_z),
        origin: (ground_offset_m: 0.0, ground_tolerance_m: 0.05, coarse_floor_rise_m: 0.25, center_fraction: 0.1),
        lod: (required: ["lod0"], budget_lod: "lod0", lod0_min_triangles: 1, lod0_max_triangles: 100000, chain: ["lod0"], require_decreasing: true),
    )"#;
    let contract = match DeliveryContract::from_ron_str(text) {
        Ok(contract) => contract,
        Err(err) => panic!("v1 without kit fields must load: {err}"),
    };
    assert_eq!(contract.max_joint_influences, 4);
    assert_eq!(contract.max_materials, 4);
    assert_eq!(contract.texture_max_edge, 2048);
    assert!(contract.required_sockets.is_empty());
    assert!(contract.footprint.is_none());
}

/// Unknown fields are refused (ADR-8553): a misspelled key is `contract_parse` naming it, never a
/// silent default. Forward compatibility is a version bump plus a migrator.
#[test]
fn contract_unknown_field_is_refused() {
    let text = match prop_contract().to_ron_string() {
        Ok(text) => text,
        Err(err) => panic!("{err}"),
    };
    let cases = [
        (
            "optionall",
            text.replacen(
                "optional: [],",
                "optional: [],\n        optionall: [\"lod1\"],",
                1,
            ),
        ),
        (
            "clas_name",
            text.replacen("class_name:", "clas_name: \"x\",\n    class_name:", 1),
        ),
        (
            "tolerance",
            text.replacen(
                "identity_tolerance:",
                "tolerance: 1.0,\n        identity_tolerance:",
                1,
            ),
        ),
        (
            "right",
            text.replacen("forward:", "right: pos_x,\n        forward:", 1),
        ),
    ];
    for (field, bad) in cases {
        assert_ne!(bad, text, "{field}: the edit applied");
        match DeliveryContract::from_ron_str(&bad) {
            Err(err) => {
                assert_eq!(err.code(), "contract_parse", "{field}: {err}");
                assert!(err.to_string().contains(field), "{field}: {err}");
            }
            Ok(_) => panic!("{field}: an unknown field must be refused"),
        }
    }
    // A newer version with an unknown field is still the version error (the peek runs first).
    let newer = text.replace("format_version: 1", "format_version: 2").replacen(
        "optional: [],",
        "optional: [],\n        optionall: [\"lod1\"],",
        1,
    );
    match DeliveryContract::from_ron_str(&newer) {
        Err(err) => assert_eq!(err.code(), "contract_version_newer", "{err}"),
        Ok(_) => panic!("format_version 2 must be refused"),
    }
}

/// The frozen corpus bytes (hand-compacted, LF) read, the writer's text of them is a fixed point that
/// reads back equal, and a CRLF copy reads the same contract and writes LF (review nit).
#[test]
fn the_corpus_fixture_round_trips_and_crlf_reads_the_same() {
    let fixture = include_str!("../../regia_agent/tests/fixtures/format-corpus/delivery-contract/v1.ron");
    assert!(!fixture.contains('\r'), "the frozen fixture is LF");
    let contract = match DeliveryContract::from_ron_str(fixture) {
        Ok(contract) => contract,
        Err(err) => panic!("{err}"),
    };
    let text = match contract.to_ron_string() {
        Ok(text) => text,
        Err(err) => panic!("{err}"),
    };
    match DeliveryContract::from_ron_str(&text) {
        Ok(again) => {
            assert_eq!(again, contract, "the writer's text reads back equal");
            match again.to_ron_string() {
                Ok(twice) => assert_eq!(twice, text, "the writer's text is a fixed point"),
                Err(err) => panic!("{err}"),
            }
        }
        Err(err) => panic!("{err}"),
    }
    let crlf = fixture.replace('\n', "\r\n");
    match DeliveryContract::from_ron_str(&crlf) {
        Ok(again) => {
            assert_eq!(again, contract);
            match again.to_ron_string() {
                Ok(written) => assert_eq!(written, text, "a CRLF source writes the same LF text"),
                Err(err) => panic!("{err}"),
            }
        }
        Err(err) => panic!("CRLF: {err}"),
    }
}

/// A contract whose `kit_line` puts `extra: 1` inside one kit struct must be `contract_parse`
/// naming `extra` (ADR-8553 on the kit structs of ADR-8450; review delivery-contract-b B).
fn assert_kit_unknown_field_refused(label: &str, kit_line: &str) {
    let text = format!(
        r#"(
        format_version: 1,
        class_name: "prop",
        scale: (meters_per_unit: 1.0, identity_tolerance: 0.001),
        axes: (up: pos_y, forward: pos_z),
        origin: (ground_offset_m: 0.0, ground_tolerance_m: 0.05, coarse_floor_rise_m: 0.25, center_fraction: 0.1),
        lod: (required: ["lod0"], budget_lod: "lod0", lod0_min_triangles: 1, lod0_max_triangles: 100000, require_decreasing: true),
        {kit_line}
    )"#
    );
    match DeliveryContract::from_ron_str(&text) {
        Err(err) => {
            assert_eq!(err.code(), "contract_parse", "{label}: {err}");
            assert!(err.to_string().contains("extra"), "{label}: {err}");
        }
        Ok(contract) => panic!(
            "{label}: unknown field `extra` loaded; nodes {:?} footprint {:?} clearance {:?}",
            contract.required_nodes, contract.footprint, contract.socket_clearance
        ),
    }
    // The same line without `extra` loads, so the refusal is the unknown key and nothing else.
    let clean = text.replace(", extra: 1", "");
    assert_ne!(clean, text, "{label}: the clean copy differs");
    if let Err(err) = DeliveryContract::from_ron_str(&clean) {
        panic!("{label}: the line without `extra` must load: {err}");
    }
}

#[test]
fn unknown_field_inside_name_count_is_refused() {
    assert_kit_unknown_field_refused(
        "required_nodes",
        r#"required_nodes: [(pattern: "root", count: 1, extra: 1)],"#,
    );
    assert_kit_unknown_field_refused(
        "required_sockets",
        r#"required_sockets: [(pattern: "socket_*", count: 1, extra: 1)],"#,
    );
    assert_kit_unknown_field_refused(
        "required_clips",
        r#"required_clips: [(pattern: "idle", count: 1, extra: 1)],"#,
    );
}

#[test]
fn unknown_field_inside_footprint_is_refused() {
    assert_kit_unknown_field_refused(
        "footprint",
        "footprint: Some((radius_m: 1.0, tolerance: 0.1, extra: 1)),",
    );
}

#[test]
fn unknown_field_inside_socket_clearance_is_refused() {
    assert_kit_unknown_field_refused(
        "socket_clearance",
        r#"socket_clearance: [(pattern: "hatch", min_width_m: 0.4, min_height_m: 0.6, extra: 1)],"#,
    );
}

/// The kit fold fixture (`delivery-contract/v1-kit.ron`, ADR-8450 "folded into v1", review
/// delivery-contract-b A) reads with every kit field it sets, and the writer's text of it reads
/// back equal. Read at run time so a missing fixture is a failing test, not a build error.
#[test]
fn the_kit_corpus_fixture_reads_every_kit_field() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_agent/tests/fixtures/format-corpus/delivery-contract/v1-kit.ron");
    let fixture = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => panic!("{}: {err}", path.display()),
    };
    assert!(!fixture.contains('\r'), "the frozen fixture is LF");
    let contract = match DeliveryContract::from_ron_str(&fixture) {
        Ok(contract) => contract,
        Err(err) => panic!("{err}"),
    };
    assert_eq!(contract.format_version, 1);
    assert_eq!(contract.class_name, "vehicle");
    assert_eq!(contract.max_joint_influences, 8);
    let rows = |list: &[NameCount]| -> Vec<(String, u32)> {
        list.iter().map(|row| (row.pattern.clone(), row.count)).collect()
    };
    assert_eq!(rows(&contract.required_nodes), [("root".to_string(), 1)]);
    assert_eq!(
        rows(&contract.required_sockets),
        [("socket_hatch*".to_string(), 2)]
    );
    assert_eq!(rows(&contract.required_clips), [("hatch_open".to_string(), 1)]);
    assert_eq!(contract.max_materials, 6);
    assert_eq!(contract.material_warn_above, 5);
    assert_eq!(contract.texture_max_edge, 4096);
    assert_eq!(
        contract.footprint,
        Some(FootprintRule {
            radius_m: 2.5,
            tolerance: 0.15,
        })
    );
    assert_eq!(
        contract.socket_clearance,
        [SocketClearance {
            pattern: "socket_hatch*".into(),
            min_width_m: 0.6,
            min_height_m: 0.9,
        }]
    );
    let text = match contract.to_ron_string() {
        Ok(text) => text,
        Err(err) => panic!("{err}"),
    };
    match DeliveryContract::from_ron_str(&text) {
        Ok(again) => assert_eq!(again, contract, "the writer's text reads back equal"),
        Err(err) => panic!("{err}"),
    }
}
