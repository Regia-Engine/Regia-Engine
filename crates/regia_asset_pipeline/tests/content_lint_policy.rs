// SPDX-License-Identifier: MPL-2.0
//! CT-02: one project-resolved source-lint policy and evidence contract (ADR-8248).

#[path = "support/gltf_lint.rs"]
mod fixture;

use std::sync::atomic::{AtomicUsize, Ordering};

use regia_asset_pipeline::gltf_lint::GltfLintReport;
use regia_asset_pipeline::source_lint::{
    ProduceLimits, SourceDocument, SourceLintPolicy, SupportStatus, content_hash, produce, rules,
};
use regia_asset_pipeline::validators::{VALIDATION_REPORT_FORMAT_VERSION, ValidationReport};
use serde_json::{Value, json};

fn limits() -> ProduceLimits {
    ProduceLimits {
        max_assets: 8,
        max_findings: 100,
        max_bytes: 8 * 1024 * 1024,
    }
}

fn bare_gltf() -> Vec<u8> {
    br#"{"asset":{"version":"2.0"}}"#.to_vec()
}

fn empty_material() -> Vec<u8> {
    br#"{"asset":{"version":"2.0"},"materials":[{}]}"#.to_vec()
}

fn policy(text: &str) -> SourceLintPolicy {
    SourceLintPolicy::from_ron(text).expect("policy")
}

fn run(sources: &[SourceDocument<'_>], text: &str, target: &str, limits: ProduceLimits) -> Value {
    let report = produce(sources, &policy(text), target, &limits, &|| false).expect("produce");
    serde_json::from_slice(&report.to_json().expect("json")).expect("parse")
}

fn bytes_of(sources: &[SourceDocument<'_>], text: &str, target: &str) -> Vec<u8> {
    produce(sources, &policy(text), target, &limits(), &|| false)
        .expect("produce")
        .to_json()
        .expect("json")
}

const ENGINE: &str = "(format_version: 1)";

#[test]
fn shuffled_sources_emit_byte_identical_reports() {
    let note = b"hello".to_vec();
    let gltf = bare_gltf();
    let forward = [
        SourceDocument {
            path: "b.txt",
            bytes: &note,
        },
        SourceDocument {
            path: "a.gltf",
            bytes: &gltf,
        },
    ];
    let backward = [
        SourceDocument {
            path: "a.gltf",
            bytes: &gltf,
        },
        SourceDocument {
            path: "b.txt",
            bytes: &note,
        },
    ];
    assert_eq!(
        bytes_of(&forward, ENGINE, "default",),
        bytes_of(&backward, ENGINE, "default")
    );
}

#[test]
fn target_or_rule_settings_change_the_evidence_key() {
    let gltf = bare_gltf();
    let sources = [SourceDocument {
        path: "a.gltf",
        bytes: &gltf,
    }];
    let base = r#"(
        format_version: 1,
        profiles: {
            "mobile": (geometry: Some((quantization: Some(0.001)))),
            "same": (),
        },
    )"#;
    let changed = r#"(
        format_version: 1,
        profiles: {
            "mobile": (geometry: Some((quantization: Some(0.002)))),
            "same": (),
        },
    )"#;
    let note = b"note".to_vec();
    let notes = [SourceDocument {
        path: "n.txt",
        bytes: &note,
    }];
    let default_key = run(&sources, base, "default", limits())["evidence_key"].clone();
    let mobile_key = run(&sources, base, "mobile", limits())["evidence_key"].clone();
    let retuned = run(&sources, changed, "mobile", limits())["evidence_key"].clone();
    let same_numbers = run(&notes, base, "same", limits())["evidence_key"].clone();
    let default_note = run(&notes, base, "default", limits())["evidence_key"].clone();
    assert_ne!(default_key, mobile_key);
    assert_ne!(mobile_key, retuned);
    assert_ne!(same_numbers, default_note, "the target name is part of the key");
}

#[test]
fn missing_metrics_are_unknown_never_zero() {
    let note = b"not a model".to_vec();
    let sources = [SourceDocument {
        path: "readme.txt",
        bytes: &note,
    }];
    let report = produce(&sources, &policy(ENGINE), "default", &limits(), &|| false).expect("produce");
    assert!(report.complete);
    assert_eq!(report.sources[0].support, SupportStatus::Unsupported);
    assert_eq!(report.sources[0].reason.as_deref(), Some("not_gltf"));
    for name in ["triangles", "meshes", "materials"] {
        let metric = report.metric("readme.txt", name);
        assert!(!metric.known, "{name}");
        assert_eq!(metric.value, None, "{name}");
        assert_eq!(metric.unit, "count");
    }
    let json = report.to_json().expect("json");
    let value: Value = serde_json::from_slice(&json).expect("parse");
    let triangles = &value["sources"][0]["metrics"]["triangles"];
    assert_eq!(triangles["known"], false);
    assert!(triangles.get("value").is_none());
    assert!(!String::from_utf8(json).expect("utf8").contains("\"value\":0"));
}

#[test]
fn measured_zero_is_known() {
    let gltf = bare_gltf();
    let sources = [SourceDocument {
        path: "empty.gltf",
        bytes: &gltf,
    }];
    let report = produce(&sources, &policy(ENGINE), "default", &limits(), &|| false).expect("produce");
    assert!(report.complete);
    assert_eq!(report.sources[0].support, SupportStatus::Supported);
    let triangles = report.metric("empty.gltf", "triangles");
    assert!(triangles.known);
    assert_eq!(triangles.value, Some(0));
    assert_eq!(report.metric("empty.gltf", "meshes").value, Some(0));
    assert_eq!(report.metric("empty.gltf", "materials").value, Some(0));
    assert!(report.metric("missing.gltf", "triangles").value.is_none());
}

#[test]
fn truncation_and_cancellation_cannot_be_complete() {
    let a = b"aaa".to_vec();
    let b = b"bbb-longer".to_vec();
    let sources = [
        SourceDocument {
            path: "b.txt",
            bytes: &b,
        },
        SourceDocument {
            path: "a.txt",
            bytes: &a,
        },
    ];
    let polls = AtomicUsize::new(0);
    let immediate = produce(&sources, &policy(ENGINE), "default", &limits(), &|| {
        polls.fetch_add(1, Ordering::Relaxed) == 0
    })
    .expect("cancel at start");
    assert!(!immediate.complete);
    assert_eq!(immediate.work.sources_processed, 0);
    assert_eq!(immediate.work.sources_seen, 2);
    assert!(
        immediate
            .sources
            .iter()
            .all(|row| row.reason.as_deref() == Some("cancelled"))
    );

    let polls = AtomicUsize::new(0);
    let after_one = produce(&sources, &policy(ENGINE), "default", &limits(), &|| {
        polls.fetch_add(1, Ordering::Relaxed) >= 2
    })
    .expect("cancel after one");
    assert!(!after_one.complete);
    assert_eq!(after_one.work.sources_processed, 1);
    assert_eq!(
        after_one
            .sources
            .iter()
            .find(|row| row.path == "a.txt")
            .expect("a")
            .support,
        SupportStatus::Unsupported
    );
    assert_eq!(
        after_one
            .sources
            .iter()
            .find(|row| row.path == "b.txt")
            .expect("b")
            .reason
            .as_deref(),
        Some("cancelled")
    );

    let capped = ProduceLimits {
        max_assets: 1,
        ..limits()
    };
    let truncated = produce(&sources, &policy(ENGINE), "default", &capped, &|| false).expect("cap");
    assert!(!truncated.complete);
    assert_eq!(truncated.work.sources_processed, 1);
    assert_eq!(truncated.sources[0].path, "a.txt");
    assert_eq!(truncated.sources[1].reason.as_deref(), Some("truncated"));

    let tiny = ProduceLimits {
        max_bytes: 1,
        ..limits()
    };
    let by_bytes = produce(&sources, &policy(ENGINE), "default", &tiny, &|| false).expect("bytes");
    assert!(!by_bytes.complete);
    assert_eq!(by_bytes.work.sources_processed, 0);

    let material = empty_material();
    let mat_sources = [SourceDocument {
        path: "m.gltf",
        bytes: &material,
    }];
    let few = ProduceLimits {
        max_findings: 1,
        ..limits()
    };
    let cut = produce(&mat_sources, &policy(ENGINE), "default", &few, &|| false).expect("findings");
    assert!(!cut.complete);
    assert_eq!(cut.work.findings, 2);
    assert_eq!(cut.work.findings_kept, 1);
    assert_eq!(cut.findings.len(), 1);

    let none = ProduceLimits {
        max_findings: 0,
        ..limits()
    };
    let empty = produce(
        &[SourceDocument {
            path: "empty.gltf",
            bytes: &bare_gltf(),
        }],
        &policy(ENGINE),
        "default",
        &none,
        &|| false,
    )
    .expect("zero cap on a clean file");
    assert!(empty.complete);
    assert_eq!(empty.work.findings, 0);
}

#[test]
fn a_suppression_cannot_waive_another_asset_or_rule() {
    let bytes = empty_material();
    let hash = content_hash(&bytes);
    let text = format!(
        r#"(
            format_version: 1,
            suppressions: [
                (rule_id: "factor_missing", path: "a.gltf", content_hash: "{hash}", reason: "authored later"),
                (rule_id: "gltf_coincident_triangles", path: "a.gltf", content_hash: "{hash}", reason: "not this asset's triangles"),
            ],
        )"#
    );
    let glb = fixture::Fixture::stretched().glb();
    let sources = [
        SourceDocument {
            path: "b.gltf",
            bytes: &bytes,
        },
        SourceDocument {
            path: "a.gltf",
            bytes: &bytes,
        },
        SourceDocument {
            path: "stretch.glb",
            bytes: &glb,
        },
    ];
    let report = produce(&sources, &policy(&text), "default", &limits(), &|| false).expect("produce");
    let waived = |path: &str, rule: &str| {
        report
            .findings
            .iter()
            .filter(|finding| finding.path == path && finding.rule_id == rule && finding.suppressed)
            .count()
    };
    let open = |path: &str, rule: &str| {
        report
            .findings
            .iter()
            .filter(|finding| finding.path == path && finding.rule_id == rule && !finding.suppressed)
            .count()
    };
    assert_eq!(waived("a.gltf", "factor_missing"), 2);
    assert_eq!(open("b.gltf", "factor_missing"), 2);
    assert_eq!(waived("b.gltf", "factor_missing"), 0);
    assert!(open("stretch.glb", "gltf_coincident_triangles") >= 1);
    assert_eq!(waived("stretch.glb", "gltf_coincident_triangles"), 0);

    let narrow = format!(
        r#"(
            format_version: 1,
            suppressions: [(
                rule_id: "factor_missing",
                path: "a.gltf",
                content_hash: "{hash}",
                reason: "metallic only",
                location: Some("/materials/0/pbrMetallicRoughness/metallicFactor"),
            )],
        )"#
    );
    let narrowed = produce(
        &[SourceDocument {
            path: "a.gltf",
            bytes: &bytes,
        }],
        &policy(&narrow),
        "default",
        &limits(),
        &|| false,
    )
    .expect("narrow");
    assert!(
        narrowed
            .findings
            .iter()
            .any(|finding| { finding.location.ends_with("metallicFactor") && finding.suppressed })
    );
    assert!(
        narrowed
            .findings
            .iter()
            .any(|finding| { finding.location.ends_with("roughnessFactor") && !finding.suppressed })
    );

    let mut stale = hash.clone();
    let digit = if stale.ends_with('a') { 'b' } else { 'a' };
    stale.pop();
    stale.push(digit);
    let stale_text = format!(
        r#"(format_version: 1, suppressions: [(rule_id: "factor_missing", path: "a.gltf", content_hash: "{stale}", reason: "stale")])"#
    );
    let stale_report = produce(
        &[SourceDocument {
            path: "a.gltf",
            bytes: &bytes,
        }],
        &policy(&stale_text),
        "default",
        &limits(),
        &|| false,
    )
    .expect("stale");
    assert!(stale_report.findings.iter().all(|finding| !finding.suppressed));
}

#[test]
fn json_and_sarif_share_rule_and_location() {
    let bytes = empty_material();
    let hash = content_hash(&bytes);
    let text = format!(
        r#"(format_version: 1, suppressions: [(rule_id: "factor_missing", path: "m.gltf", content_hash: "{hash}", reason: "waived", location: Some("/materials/0/pbrMetallicRoughness/metallicFactor"))])"#
    );
    let report = produce(
        &[SourceDocument {
            path: "m.gltf",
            bytes: &bytes,
        }],
        &policy(&text),
        "default",
        &limits(),
        &|| false,
    )
    .expect("produce");
    let json: Value = serde_json::from_slice(&report.to_json().expect("json")).expect("json value");
    let sarif: Value = serde_json::from_slice(&report.to_sarif().expect("sarif")).expect("sarif value");
    assert_eq!(sarif["version"], "2.1.0");
    let results = sarif["runs"][0]["results"].as_array().expect("results");
    let findings = json["findings"].as_array().expect("findings");
    assert_eq!(results.len(), findings.len());
    for (finding, result) in findings.iter().zip(results) {
        assert_eq!(result["ruleId"], finding["rule_id"]);
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            finding["path"]
        );
        assert_eq!(
            result["locations"][0]["logicalLocations"][0]["fullyQualifiedName"],
            finding["location"]
        );
        if finding["suppressed"] == true {
            assert_eq!(
                result["suppressions"][0]["justification"],
                finding["suppression_reason"]
            );
        }
    }
}

#[test]
fn project_and_per_asset_overrides_follow_importer_precedence() {
    let text = r#"(
        format_version: 1,
        defaults: (
            materials: Some((
                default_roughness: Some(0.4),
                default_volume_thickness: Some(Some(1.0)),
                name_rules: Some([(contains: "wood", roughness: Some(0.8))]),
            )),
        ),
        profiles: {
            "mobile": (materials: Some((default_metallic: Some(0.2)))),
            "default": (materials: Some((default_roughness: Some(0.3)))),
        },
        assets: [
            (glob: "ASSETS/hero.gltf", materials: Some((default_metallic: Some(0.5)))),
            (glob: "assets/*.gltf", materials: Some((
                default_metallic: Some(0.9),
                default_volume_thickness: Some(None),
                name_rules: Some([(contains: "metal", metallic: Some(1.0))]),
            ))),
        ],
    )"#;
    let file = policy(text);
    let hero = file.resolve("mobile", Some("assets/hero.gltf")).expect("hero");
    assert_eq!(hero.materials.default_metallic, 0.9, "later matching glob wins");
    assert_eq!(
        hero.materials.default_roughness, 0.4,
        "profile did not set roughness"
    );
    assert_eq!(hero.materials.default_emissive_strength, 1.0);
    assert_eq!(hero.materials.default_volume_thickness, None, "Some(None) clears");
    assert_eq!(hero.materials.name_rules.len(), 1);
    assert_eq!(hero.materials.name_rules[0].contains, "metal");
    let other = file.resolve("mobile", Some("assets/crate.gltf")).expect("other");
    assert_eq!(other.materials.default_metallic, 0.9);
    let untouched = file.resolve("mobile", Some("props/crate.obj")).expect("obj");
    assert_eq!(untouched.materials.default_metallic, 0.2);
    assert_eq!(untouched.materials.default_volume_thickness, Some(1.0));
    assert_eq!(untouched.materials.name_rules[0].contains, "wood");
    let named_default = file.resolve("default", None).expect("default profile");
    assert_eq!(named_default.materials.default_roughness, 0.3);
    let err = file.resolve("phone", None).expect_err("unknown target");
    assert_eq!(err.code(), "content_lint_target");
    let empty = file.resolve("", None).expect_err("empty target");
    assert_eq!(empty.code(), "content_lint_target");

    let gltf = bare_gltf();
    let sources = [SourceDocument {
        path: "assets/hero.gltf",
        bytes: &gltf,
    }];
    let mobile = run(&sources, text, "mobile", limits())["evidence_key"].clone();
    let desktop = run(&sources, text, "default", limits())["evidence_key"].clone();
    assert_ne!(mobile, desktop);
}

#[test]
fn suppression_paths_are_exact_and_globs_are_case_insensitive() {
    let bytes = bare_gltf();
    let hash = content_hash(&bytes);
    let text = format!(
        r#"(
            format_version: 1,
            assets: [(glob: "ASSETS/*.GLTF", materials: Some((default_metallic: Some(0.25))))],
            suppressions: [(
                rule_id: "factor_missing",
                path: "assets/hero.gltf",
                content_hash: "{hash}",
                reason: "lowercase path only",
            )],
        )"#
    );
    let file = policy(&text);
    let folded = file.resolve("default", Some("assets/hero.gltf")).expect("fold");
    assert_eq!(folded.materials.default_metallic, 0.25);
    let material = empty_material();
    let material_hash = content_hash(&material);
    let exact = format!(
        r#"(
            format_version: 1,
            suppressions: [(
                rule_id: "factor_missing",
                path: "assets/hero.gltf",
                content_hash: "{material_hash}",
                reason: "exact path",
            )],
        )"#
    );
    let report = produce(
        &[
            SourceDocument {
                path: "assets/hero.gltf",
                bytes: &material,
            },
            SourceDocument {
                path: "Assets/Hero.gltf",
                bytes: &material,
            },
        ],
        &policy(&exact),
        "default",
        &limits(),
        &|| false,
    )
    .expect("produce");
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.path == "assets/hero.gltf" && finding.suppressed)
    );
    assert!(
        report
            .findings
            .iter()
            .filter(|finding| finding.path == "Assets/Hero.gltf")
            .all(|finding| !finding.suppressed)
    );
}

#[test]
fn geometry_budget_is_incomplete_and_metrics_stay_unknown() {
    let glb = fixture::Fixture::stretched().glb();
    let text = "(format_version: 1, defaults: (geometry: Some((max_triangles: Some(1)))))";
    let report = produce(
        &[SourceDocument {
            path: "stretch.glb",
            bytes: &glb,
        }],
        &policy(text),
        "default",
        &limits(),
        &|| false,
    )
    .expect("produce");
    assert!(!report.complete);
    assert_eq!(report.sources[0].support, SupportStatus::Incomplete);
    assert_eq!(report.sources[0].reason.as_deref(), Some("gltf_lint_limit"));
    assert!(report.metric("stretch.glb", "triangles").value.is_none());
    assert!(report.metric("stretch.glb", "meshes").value.is_none());
    assert_eq!(report.metric("stretch.glb", "materials").value, Some(0));
    assert_eq!(report.work.rules_checked, rules().len());
    assert_eq!(report.work.sources_processed, 1);
}

#[test]
fn work_counters_match_the_processed_set() {
    let note = b"xyz".to_vec();
    let gltf = bare_gltf();
    let report = produce(
        &[
            SourceDocument {
                path: "n.txt",
                bytes: &note,
            },
            SourceDocument {
                path: "a.gltf",
                bytes: &gltf,
            },
        ],
        &policy(ENGINE),
        "default",
        &limits(),
        &|| false,
    )
    .expect("produce");
    assert_eq!(report.work.sources_seen, 2);
    assert_eq!(report.work.sources_processed, 2);
    assert_eq!(report.work.bytes_hashed, (note.len() + gltf.len()) as u64);
    assert_eq!(report.work.rules_checked, rules().len());
    assert_eq!(report.work.findings, 0);
    assert_eq!(report.work.findings_kept, 0);
    assert_eq!(report.format_version, 1);
    assert_eq!(report.tool_hash.len(), 64);
}

#[test]
fn old_reports_still_parse_and_adapters_keep_locations() {
    let old = json!({
        "meshes": 1,
        "primitives": 1,
        "triangles": 2,
        "duplicate_meshes": [{"original": 0, "duplicate": 1}],
        "coincident_triangles": [{"mesh": 0, "primitive": 0, "pair": {"keeper": 0, "suspect": 1}}],
        "rig_channels": [{"code": "gltf_rig_non_joint_channel", "animation": 0, "channel": 1, "node": 2}],
        "skipped_primitives": [{"mesh": 0, "primitive": 3, "mode": 1}],
    });
    let parsed: GltfLintReport = serde_json::from_value(old).expect("old gltf report");
    assert_eq!(parsed.triangles, 2);
    let findings = regia_asset_pipeline::source_lint::findings_from_gltf_report("m.glb", &parsed);
    let locations: Vec<&str> = findings.iter().map(|finding| finding.location.as_str()).collect();
    assert!(locations.contains(&"/meshes/1/duplicate_of/0"));
    assert!(locations.contains(&"/meshes/0/primitives/0/triangles/1"));
    assert!(locations.contains(&"/animations/0/channels/1/nodes/2"));
    assert!(locations.contains(&"/meshes/0/primitives/3/mode/1"));
    let restored = regia_asset_pipeline::source_lint::gltf_report_from_findings(&findings);
    assert_eq!(restored.duplicate_meshes[0].duplicate, 1);
    assert_eq!(restored.coincident_triangles[0].pair.keeper, 0);
    assert_eq!(restored.coincident_triangles[0].pair.suspect, 1);
    assert_eq!(restored.rig_channels[0].code, "gltf_rig_non_joint_channel");
    assert_eq!(restored.skipped_primitives[0].mode, 1);
    assert_eq!(restored.triangles, 0, "examined counts are not recovered");

    let validation = json!({
        "format_version": VALIDATION_REPORT_FORMAT_VERSION,
        "summary": {
            "passed": 1, "failed": 0, "not_validated": 0, "warnings": 0,
            "requested": 1, "checked": 1, "skipped_excluded": 0, "cap_reached": false
        },
        "results": [{
            "validator": "naming_rules",
            "asset": "a.png",
            "outcome": "pass",
            "messages": []
        }]
    });
    let report: ValidationReport = serde_json::from_value(validation).expect("old validation report");
    assert!(report.is_ok());
    assert!(report.to_json().expect("json").contains("naming_rules"));
}

#[test]
fn policy_rejects_bad_identity_and_a_missing_file_is_the_engine_default() {
    let bad_hash = "(format_version: 1, suppressions: [(rule_id: \"factor_missing\", path: \"a.gltf\", content_hash: \"ABCD\", reason: \"x\")])";
    assert_eq!(
        SourceLintPolicy::from_ron(bad_hash).expect_err("hex").code(),
        "content_lint_policy"
    );
    let duplicate = format!(
        "(format_version: 1, suppressions: [{dup}, {dup}])",
        dup = r#"(rule_id: "factor_missing", path: "a.gltf", content_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", reason: "x")"#
    );
    assert_eq!(
        SourceLintPolicy::from_ron(&duplicate).expect_err("dup").code(),
        "content_lint_policy"
    );
    let dir = regia_test_utils::unique_temp_dir("content-lint-missing");
    std::fs::create_dir_all(&dir).expect("dir");
    let loaded = SourceLintPolicy::load_project(&dir).expect("missing file");
    let resolved = loaded.resolve("default", None).expect("defaults");
    assert_eq!(resolved.materials.default_roughness, 1.0);
    assert_eq!(resolved.geometry.max_triangles, 2_000_000);
    std::fs::remove_dir_all(dir).expect("cleanup");
}
