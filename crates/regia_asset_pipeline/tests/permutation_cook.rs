// SPDX-License-Identifier: MPL-2.0
//! The cook's shader-permutation report (ADR-2026, master plan § 5.5a): a project with two
//! graphs and two instances cooks into `permutations.json` with one enumeration per profile of
//! the platform. Instances that compile to their parent's code count once, a switch that changes
//! the code counts as its own set, and a vertex-stage graph counts its shadow and velocity
//! passes. A `Regia.toml` budget below the count flags that profile only; a recook from the
//! cache (every source skipped) reports the same list.

use std::path::Path;

use regia_asset::{cooked_cache_dir, decode_cooked_file};
use regia_asset_pipeline::permutation_cook::read_manifest;
use regia_asset_pipeline::{AssetDatabase, CookOptions, CookReport, cook_project};
use regia_matgraph::CookedMaterialGraph;
use regia_render::permutation_policy::{ProfileTarget, graph_set_passes, platform_presets};
use regia_render::{
    GraphTarget, PermutationAdmission, PermutationEnforcement, PermutationGuard, RenderProfile,
    RenderProfilePreset,
};

const PANEL: &str = r#"(
    format_version: 2,
    name: "panel",
    params: [(name: "tint", value: Color((0.9, 0.9, 0.9)))],
    switches: [(name: "worn", default: false)],
    nodes: {
        "tint": Param("tint"),
        "k": Vec3((0.6, 0.55, 0.5)),
        "worn_c": Mul("tint", "k"),
        "base": Switch(switch: "worn", on: "worn_c", off: "tint"),
    },
    output: (base_color: Some("base")),
)"#;

/// Same code as the parent (a parameter only).
const PANEL_BLUE: &str = r#"(
    format_version: 2,
    parent: "assets/materials/panel.matgraph.ron",
    params: { "tint": Color((0.1, 0.2, 1.0)) },
)"#;

/// Another code: the switch picks the other branch.
const PANEL_WORN: &str = r#"(
    format_version: 2,
    parent: "assets/materials/panel.matgraph.ron",
    switches: { "worn": true },
)"#;

const LIFTED: &str = r#"(
    format_version: 2,
    name: "lifted",
    params: [(name: "lift", value: Vec3((0.0, 0.2, 0.0)))],
    nodes: { "c": Vec3((0.8, 0.35, 0.1)), "lift": Param("lift") },
    output: (base_color: Some("c"), world_position_offset: Some("lift")),
    shading: (max_vertex_offset: 1.0),
)"#;

fn project(regia_toml: Option<&str>) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir("permutation_cook");
    let materials = root.join("assets/materials");
    std::fs::create_dir_all(&materials).expect("materials dir");
    std::fs::write(materials.join("panel.matgraph.ron"), PANEL).expect("panel");
    std::fs::write(materials.join("panel_blue.matinst.ron"), PANEL_BLUE).expect("blue");
    std::fs::write(materials.join("panel_worn.matinst.ron"), PANEL_WORN).expect("worn");
    std::fs::write(materials.join("lifted.matgraph.ron"), LIFTED).expect("lifted");
    if let Some(text) = regia_toml {
        std::fs::write(root.join("Regia.toml"), text).expect("Regia.toml");
    }
    root
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        incremental: true,
    })
    .expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

#[test]
fn the_cook_lists_every_profiles_graph_sets_once_per_code() {
    let root = project(None);
    let report = cook(&root);
    let manifest = read_manifest(&cooked_cache_dir(&root, "linux"))
        .expect("readable")
        .expect("written");
    assert_eq!(manifest.platform, "linux");
    assert_eq!(manifest.graphs, 3, "panel, panel worn, lifted: {manifest:?}");
    let presets = platform_presets("linux");
    assert_eq!(manifest.profiles.len(), presets.len());
    assert_eq!(
        report.permutations.len(),
        presets.len(),
        "{:?}",
        report.permutations
    );
    assert!(
        report.permutation_problems.is_empty(),
        "{:?}",
        report.permutation_problems
    );
    for (profile, preset) in manifest.profiles.iter().zip(presets) {
        let target = ProfileTarget::of(&RenderProfile::from_preset(preset));
        assert_eq!(profile.profile, preset.name());
        assert_eq!(profile.entries.len(), 3, "{}", profile.summary_line());
        let want = 2 * graph_set_passes(target.instanced, false).len()
            + graph_set_passes(target.instanced, true).len();
        assert_eq!(profile.pipelines as usize, want, "{}", profile.summary_line());
        let lifted = profile
            .entries
            .iter()
            .find(|e| e.name.ends_with("lifted.matgraph.ron"))
            .expect("lifted entry");
        assert!(lifted.passes.iter().any(|p| p == "velocity"), "{lifted:?}");
        assert!(
            report
                .permutations
                .iter()
                .any(|l| l.starts_with(&format!("{}:", preset.name())))
        );
    }

    // A recook from the cache reports the same list.
    let again = cook(&root);
    assert_eq!(again.cooked, 0, "{again:?}");
    assert_eq!(again.permutations, report.permutations);
    let _ = std::fs::remove_dir_all(&root);
}

/// The promise a shipping guard rests on: the key the cook lists is the key the runtime draws. A
/// mismatch would refuse every cooked graph and draw a whole game with standard surfaces.
#[test]
fn every_cooked_graph_is_admitted_with_the_key_its_material_draws() {
    let root = project(None);
    cook(&root);
    let cache = cooked_cache_dir(&root, "linux");
    let manifest = read_manifest(&cache).expect("readable").expect("written");
    let mut guard =
        PermutationGuard::from_manifest(&manifest, "DesktopHigh", PermutationEnforcement::Fallback)
            .expect("guard");
    let db = AssetDatabase::open(&root).expect("db");
    let mut drawn = std::collections::BTreeSet::new();
    for (source, entry) in &db.entries {
        if !source.ends_with(".matgraph.ron") && !source.ends_with(".matinst.ron") {
            continue;
        }
        let bytes = std::fs::read(cache.join(&entry.cooked_rel)).expect("cooked file");
        let (_, payload) = decode_cooked_file(&bytes).expect("cooked header");
        let graph = CookedMaterialGraph::from_postcard(payload).expect("cooked graph");
        let material = graph.to_material(&std::collections::BTreeMap::new());
        let key = material.surface_graph.as_ref().expect("a graph material").key;
        assert_eq!(
            guard.admit(key, &GraphTarget::HDR),
            PermutationAdmission::Listed,
            "{source} draws an unlisted key"
        );
        drawn.insert(key);
    }
    assert_eq!(
        drawn.len(),
        manifest.graphs,
        "every listed code is drawn by some material"
    );
    assert_eq!(guard.stats().refused, 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_regia_toml_budget_below_the_count_flags_that_profile_only() {
    let root = project(Some("[cook.permutation_budget.DesktopLow]\nmax_pipelines = 5\n"));
    let report = cook(&root);
    assert_eq!(
        report.permutation_problems.len(),
        1,
        "{:?}",
        report.permutation_problems
    );
    assert!(
        report.permutation_problems[0].starts_with("DesktopLow:"),
        "{:?}",
        report.permutation_problems
    );
    let manifest = read_manifest(&cooked_cache_dir(&root, "linux"))
        .expect("readable")
        .expect("written");
    let low = manifest.profile("DesktopLow").expect("DesktopLow");
    assert!(low.over_budget);
    assert_eq!(low.budget.max_pipelines, 5);
    assert!(!manifest.profile("DesktopHigh").expect("DesktopHigh").over_budget);
    assert_eq!(
        RenderProfilePreset::from_name(&low.profile),
        Some(RenderProfilePreset::DesktopLow)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_shader_cost_is_attached_per_profile_with_the_estimate_apart_from_the_measurement() {
    let root = project(Some(
        "[cook.permutation_budget.MobileLow]\nmax_shader_bytes = 1\n",
    ));
    let mut report = cook(&root);
    assert!(
        report.shader_cost.is_none(),
        "the library cook composes no shaders"
    );
    report.attach_shader_cost(&root, "android");
    let cost = report.shader_cost.as_ref().expect("attached");
    assert_eq!(cost.platform, "android");
    assert_eq!(cost.profiles.len(), 2, "MobileLow and MobileHigh");
    for row in &cost.profiles {
        let preset = regia_render::RenderProfilePreset::from_name(&row.profile).expect("a preset");
        assert_eq!(
            row.permutations as usize,
            regia_render::forward_permutations_for("android", preset).len(),
            "{}",
            row.summary_line()
        );
        assert_eq!(row.failed, 0, "{}", row.summary_line());
        assert!(row.wgsl_bytes > 0 && row.spirv_bytes > 0 && row.compile_cost_ms > 0);
        // Labelled apart: the line names the estimate and the measurement.
        let line = row.summary_line();
        assert!(
            line.contains("(estimate)") && line.contains("(measured)"),
            "{line}"
        );
    }
    let low = &cost.profiles[0];
    assert_eq!(low.profile, "MobileLow");
    assert_eq!(low.budget.max_wgsl_bytes, 1, "Regia.toml overrides the row");
    assert!(low.over_budget);
    assert!(!cost.profiles[1].over_budget);
    assert!(
        report
            .permutation_problems
            .iter()
            .any(|p| p.starts_with("MobileLow:") && p.contains("shipped forward WGSL")),
        "{:?}",
        report.permutation_problems
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-4901: the cook report carries the material-graph checks beyond the pipeline count: each
/// device limit class of the platform's profiles (Android: `mobile`, Linux: `desktop`) and the
/// static switches (the panel graph and its two instances declare `worn`; two values cooked).
#[test]
fn the_cook_reports_graph_bindings_per_class_and_the_static_switches() {
    let root = project(None);
    for (platform, class) in [("linux", "desktop"), ("android", "mobile")] {
        let report = cook_project(CookOptions {
            project_root: root.clone(),
            platform: platform.into(),
            incremental: true,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{platform}: {:?}", report.errors);
        assert!(
            report.permutation_problems.is_empty(),
            "{platform}: {:?}",
            report.permutation_problems
        );
        let lines = &report.material_graph_checks;
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("graph bindings vs `{class}`")) && l.contains("3 code(s)")),
            "{platform}: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("static switches: 3 of 4") && l.contains("2 switch permutation(s)")),
            "{platform}: {lines:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
