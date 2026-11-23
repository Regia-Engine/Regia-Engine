// SPDX-License-Identifier: MPL-2.0
//! `.fracture.ron` against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/fracture/`.
//! The set lives in the desktop-only pipeline crate (`COVERED_ELSEWHERE`), so the four checks run here; so do the
//! pipeline's other formats (validation, generator, provenance-policy, import-rules).

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_asset_pipeline::fracture::{FRACTURE_FORMAT_VERSION, FractureSet};

/// Read (migrating older versions) and write the set again.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let set = FractureSet::from_ron(text).map_err(|e| e.to_string())?;
    set.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

const FRACTURE_RON: &str = "(format_version: 1, source_volume: 0.16666667, chunks: [(site: (0.25, 0.25, 0.25), \
     positions: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)], \
     indices: [0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3], interior_start: 4, volume: 0.16666667, \
     centroid: (0.25, 0.25, 0.25), bounds_min: (0.0, 0.0, 0.0), bounds_max: (1.0, 1.0, 1.0), \
     interior_area: 0.0)], joints: [])";

const VALIDATION_RON: &str = r#"(
    format_version: 1,
    validate_on_save: true,
    cook: Warn,
    exclude: ["assets/dev/**"],
    roots: ["assets/levels/**"],
    limits: (max_texture_dimension: 4096, texture_power_of_two: false, mesh_triangle_budget: 200000, max_path_len: 200),
    rules: [
        PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error),
        SizeBudget(id: "texture-cap", kind: Texture, max_bytes: 16777216, severity: Warning),
    ],
)"#;

const GENERATOR_TOML: &str = r#"format_version = 1

[generator]
kind = "command"
seed = 42
description = "corpus fixture"
command = ["tool", "--seed", "{seed}", "--out", "{out}"]
timeout_secs = 30
inputs = ["data/a.ron", "data/b/"]
outputs = ["assets/gen/a.ron", "assets/gen/b/"]
"#;

const PROVENANCE_RON: &str = "(format_version: 1)";

const IMPORT_RULES_RON: &str = r#"(
    format_version: 1,
    rules: [
        (glob: "assets/ui/**", importer: "png", settings: "(compress: Some(false), max_size: Some(512))"),
        (glob: "assets/**", settings: "(quality: Some(Low))"),
        (glob: "assets/props/*", importer: "gltf", settings: "(meshlets: true, collision: ConvexDecomposition)"),
    ],
)"#;

fn validation_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let file = regia_asset_pipeline::validation_rules::ValidationFile::from_ron(text)
        .map_err(|e| format!("{e:?}"))?;
    file.to_ron()
        .map(String::into_bytes)
        .map_err(|e| format!("{e:?}"))
}

fn provenance_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use regia_asset_pipeline::provenance_policy::ProvenancePolicy;
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let policy = ProvenancePolicy::from_ron(text).map_err(|e| e.to_string())?;
    policy.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

/// `config/import_rules.ron` (`import-rules`, read by `regia_asset::import_rules` and resolved by this crate at
/// cook time): read, every rule matched and merged as the cook does, then written again.
fn import_rules_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use regia_asset_pipeline::import_rules::ImportRulesFile;
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let file = ImportRulesFile::from_ron(text).map_err(|e| e.to_string())?;
    for (path, importer) in [("assets/ui/a.png", "png"), ("assets/props/crate.gltf", "gltf")] {
        let matched = file.matched_rules(path, importer);
        let _ = regia_asset::import_rules::resolve_settings(None, &matched);
    }
    file.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

/// The generator definition has no writer: a file that parses is its own canonical form, so the honest
/// checks are load + future-refusal (`check_load_only`).
fn generator_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    regia_asset_pipeline::generators::parse_generator("corpus", bytes).map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

/// `.regia_assetpkg.ron` (`assetpkg`, the asset package manifest, ADR-7788): read and written again.
fn assetpkg_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use regia_asset_pipeline::asset_package::AssetPackageManifest;
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let manifest = AssetPackageManifest::from_ron(text).map_err(|e| e.to_string())?;
    manifest
        .to_ron()
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    let fracture = load(FRACTURE_RON.as_bytes()).expect("the v1 fracture migrates");
    format_corpus_support::bless_current(
        "fracture",
        FRACTURE_FORMAT_VERSION,
        "ron",
        &fracture,
        "the v1 text of crates/regia_asset_pipeline/tests/fuzz.rs loaded and written by FractureSet at the current version",
    );
    let src = "frozen copy of the fixture text of crates/regia_asset_pipeline/tests/fuzz.rs";
    format_corpus_support::bless_current("validation", 1, "ron", VALIDATION_RON.as_bytes(), src);
    format_corpus_support::bless_current("generator", 1, "toml", GENERATOR_TOML.as_bytes(), src);
    format_corpus_support::bless_current("provenance-policy", 1, "ron", PROVENANCE_RON.as_bytes(), src);
    let import_rules = import_rules_load(IMPORT_RULES_RON.as_bytes()).expect("the import rules load");
    format_corpus_support::bless_current(
        "import-rules",
        regia_asset_pipeline::import_rules::IMPORT_RULES_FORMAT_VERSION,
        "ron",
        &import_rules,
        "the IMPORT_RULES_RON text of crates/regia_asset_pipeline/tests/fuzz.rs loaded and written by ImportRulesFile at the current version",
    );
}

#[test]
fn the_other_pipeline_formats_load_and_refuse_the_future() {
    use regia_asset_pipeline::generators::GENERATOR_FORMAT_VERSION;
    use regia_asset_pipeline::provenance_policy::PROVENANCE_POLICY_FORMAT_VERSION;
    use regia_asset_pipeline::validation_rules::VALIDATION_FILE_FORMAT_VERSION;
    let mut problems =
        format_corpus_support::check_format("validation", VALIDATION_FILE_FORMAT_VERSION, &validation_load);
    problems.extend(format_corpus_support::check_format(
        "provenance-policy",
        PROVENANCE_POLICY_FORMAT_VERSION,
        &provenance_load,
    ));
    problems.extend(format_corpus_support::check_format(
        "import-rules",
        regia_asset_pipeline::import_rules::IMPORT_RULES_FORMAT_VERSION,
        &import_rules_load,
    ));
    problems.extend(format_corpus_support::check_load_only(
        "generator",
        GENERATOR_FORMAT_VERSION,
        &generator_load,
    ));
    // `.regia_assetpkg.ron` (ADR-7788): read and written again.
    problems.extend(format_corpus_support::check_format(
        "assetpkg",
        regia_asset_pipeline::asset_package::ASSET_PACKAGE_FORMAT_VERSION,
        &assetpkg_load,
    ));
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn every_fracture_fixture_loads_round_trips_and_refuses_the_future() {
    let problems = format_corpus_support::check_format("fracture", FRACTURE_FORMAT_VERSION, &load);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn launch_profiles_corpus_round_trips_and_refuses_the_future() {
    let problems = format_corpus_support::check_format("launch-profiles", 1, &|bytes| {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        regia_asset_pipeline::launch_profiles::LaunchProfiles::from_ron(text)?
            .to_ron()
            .map(String::into_bytes)
    });
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn source_lint_corpus_loads_and_refuses_the_future() {
    let problems = format_corpus_support::check_load_only("source-lint", 1, &|bytes| {
        let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        regia_asset_pipeline::source_lint::SourceLintPolicy::from_ron(text)
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    });
    assert!(problems.is_empty(), "{problems:#?}");
}
