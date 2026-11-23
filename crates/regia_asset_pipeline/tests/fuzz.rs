// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the glTF/GLB importer. See `docs/evidence/m18/robustness.md`.

use regia_asset::AssetId;
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_glb_bytes};
use regia_test_utils::fuzz::{
    fuzz_headerless_loader, fuzz_loader, fuzz_text_loader, fuzz_text_values_loader, fuzz_tolerant_loader,
};

const N: usize = 1_000;

/// A minimal valid GLB 2.0 (a single JSON chunk, no BIN) — enough for the importer to parse and
/// then report "no mesh found" (an `Err`, not a panic) before any mutation.
fn minimal_glb() -> Vec<u8> {
    let json = r#"{"asset":{"version":"2.0"},"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"name":"m"}]}"#;
    let mut json_bytes = json.as_bytes().to_vec();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&0x4654_6C67u32.to_le_bytes()); // magic "glTF"
    out.extend_from_slice(&2u32.to_le_bytes()); // version 2
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
    out.extend_from_slice(&json_bytes);
    out
}

#[test]
fn glb_import_never_panics() {
    let fixture = minimal_glb();
    fuzz_loader(&fixture, 0x31, N, |bytes| {
        import_gltf_glb_bytes(
            bytes,
            AssetId::new_random(),
            Forward::Convert,
            "desktop",
            TextureCookQuality::High,
        )
    });
}

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

const FRACTURE_RON: &str = "(format_version: 1, source_volume: 0.16666667, chunks: [(site: (0.25, 0.25, 0.25), \
     positions: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)], \
     indices: [0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3], interior_start: 4, volume: 0.16666667, \
     centroid: (0.25, 0.25, 0.25), bounds_min: (0.0, 0.0, 0.0), bounds_max: (1.0, 1.0, 1.0), \
     interior_area: 0.0)], joints: [])";

fn validation_load(s: &str) -> Result<(), String> {
    regia_asset_pipeline::validation_rules::ValidationFile::from_ron(s)
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

fn fracture_load(s: &str) -> Result<usize, String> {
    let set = regia_asset_pipeline::fracture::FractureSet::from_ron(s).map_err(|e| e.to_string())?;
    let descs = set.chunk_descs(1000.0);
    assert!(
        descs.iter().all(|d| d.mass.is_finite()),
        "non-finite mass from a set that loaded"
    );
    Ok(descs.len())
}

const IMPORT_RULES_RON: &str = r#"(
    format_version: 1,
    rules: [
        (glob: "assets/ui/**", importer: "png", settings: "(compress: Some(false), max_size: Some(512))"),
        (glob: "assets/**", settings: "(quality: Some(Low))"),
        (glob: "assets/props/*", importer: "gltf", settings: "(meshlets: true, collision: ConvexDecomposition)"),
    ],
)"#;

fn import_rules_load(s: &str) -> Result<String, String> {
    use regia_asset::import_rules::{ImportRulesFile, resolve_settings};
    let file = ImportRulesFile::from_ron(s).map_err(|e| e.to_string())?;
    // Consume: resolve every fixture path through the rules and the settings-object scanner
    // (the merge), not only the serde parse.
    for (path, importer) in [
        ("assets/ui/a.png", "png"),
        ("assets/props/crate.gltf", "gltf"),
        ("assets/x.wav", "audio"),
    ] {
        let matched = file.matched_rules(path, importer);
        let _ = resolve_settings(Some("(usage: Some(normal))"), &matched);
    }
    file.to_ron().map_err(|e| e.to_string())
}

#[test]
fn import_rules_never_panic() {
    fuzz_text_loader(IMPORT_RULES_RON, 0x1D02_5001, N / 4, import_rules_load);
    fuzz_text_values_loader(IMPORT_RULES_RON, 0x1D02_5002, N / 4, import_rules_load);
}

#[test]
fn validation_rules_never_panic() {
    fuzz_text_loader(VALIDATION_RON, 0x5A11_D001, N / 4, validation_load);
    fuzz_text_values_loader(VALIDATION_RON, 0x5A11_D002, N / 4, validation_load);
}

#[test]
fn fracture_ron_never_panics() {
    fuzz_text_loader(FRACTURE_RON, 0xF2AC_0001, N / 4, fracture_load);
    fuzz_text_values_loader(FRACTURE_RON, 0xF2AC_0002, N / 4, fracture_load);
    // The v1 text above takes the migration; the current version (with render meshes) is fuzzed too.
    let current = regia_asset_pipeline::fracture::FractureSet::from_ron(FRACTURE_RON)
        .and_then(|set| set.to_ron())
        .expect("the v1 fixture migrates and writes");
    assert!(
        current.contains(&format!(
            "format_version: {}",
            regia_asset_pipeline::fracture::FRACTURE_FORMAT_VERSION
        )),
        "{current}"
    );
    fracture_load(&current).expect("the current-version fixture loads");
    fuzz_text_loader(&current, 0xF2AC_0003, N / 4, fracture_load);
    fuzz_text_values_loader(&current, 0xF2AC_0004, N / 4, fracture_load);
}

/// ADR-2084: the registry entry points `fuzz_entry::glb_load` and `obj_load` (the editor's `import.gltf` and
/// `import.obj` targets) accept their valid fixtures and survive the time-boxed harness: the GLB with a real
/// indexed triangle reaches the mesh cook, unlike `minimal_glb` above, which stops at "no mesh found".
#[test]
fn registry_import_entries_never_panic() {
    use regia_asset_pipeline::fuzz_entry::{glb_fixture, glb_load, obj_fixture, obj_load};
    glb_load(&glb_fixture()).expect("the GLB fixture imports");
    obj_load(&obj_fixture()).expect("the OBJ fixture imports");
    fuzz_loader(&glb_fixture(), 0x32, N, glb_load);
    fuzz_text_loader(&obj_fixture(), 0x33, N, obj_load);
    fuzz_text_values_loader(&obj_fixture(), 0x34, N, obj_load);
}

/// The importer soak (ADR-2084): `REGIA_FUZZ_SOAK_SEEDS` seeds (default 3000) of 4 mutations each, per entry point,
/// in-process (no per-call thread). Ignored in the gate; `cargo test -p regia_asset_pipeline --test fuzz -- --ignored
/// import_soak --nocapture`.
#[test]
#[ignore = "long soak; run by hand"]
fn import_soak_over_many_seeds() {
    use regia_asset_pipeline::fuzz_entry::{glb_fixture, glb_load, obj_fixture, obj_load};
    use regia_test_utils::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};
    let seeds: u64 = std::env::var("REGIA_FUZZ_SOAK_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let (glb, obj) = (glb_fixture(), obj_fixture());
    let (mut accepted, mut rejected) = (0u64, 0u64);
    let mut panics = Vec::new();
    for seed in 1..=seeds {
        let mut rng = FuzzRng::new(seed);
        for i in 0..4 {
            let bytes = mutate_bytes(&glb, &mut rng);
            let text = if i % 2 == 0 {
                mutate_text(&obj, &mut rng)
            } else {
                mutate_text_values(&obj, &mut rng)
            };
            for (what, outcome) in [
                ("glb", std::panic::catch_unwind(|| glb_load(&bytes).is_ok())),
                ("obj", std::panic::catch_unwind(|| obj_load(&text).is_ok())),
            ] {
                match outcome {
                    Ok(true) => accepted += 1,
                    Ok(false) => rejected += 1,
                    Err(_) => panics.push(format!("{what} seed {seed} mutation {i}")),
                }
            }
        }
    }
    eprintln!(
        "import soak: {seeds} seeds, accepted {accepted}, rejected {rejected}, panics {}",
        panics.len()
    );
    assert!(panics.is_empty(), "panics: {panics:?}");
    assert!(accepted > 0 && rejected > 0);
}

fn provenance_policy_load(s: &str) -> Result<String, String> {
    let policy =
        regia_asset_pipeline::provenance_policy::ProvenancePolicy::from_ron(s).map_err(|e| e.to_string())?;
    let text = policy.to_ron().map_err(|e| e.to_string())?;
    // A policy that loads re-encodes to text that loads to the same policy.
    let again = regia_asset_pipeline::provenance_policy::ProvenancePolicy::from_ron(&text)
        .map_err(|e| format!("re-encoding does not load: {e}"))?;
    assert_eq!(again, policy, "round trip");
    Ok(text)
}

/// `config/provenance.ron` (format `provenance-policy`, ADR-2428), fuzzed from the checked-in example.
#[test]
fn provenance_policy_never_panics() {
    let fixture = regia_asset_pipeline::provenance_policy::EXAMPLE_POLICY;
    provenance_policy_load(fixture).expect("the example loads");
    fuzz_text_loader(fixture, 0x9E0F_0001, N / 4, provenance_policy_load);
    fuzz_text_values_loader(fixture, 0x9E0F_0002, N / 4, provenance_policy_load);
}

/// A `generators/<name>.toml` (ADR-2412) with every field of the command kind; `outputs` is last,
/// so any truncation drops a required field.
const GENERATOR_TOML: &str = r#"format_version = 1

[generator]
kind = "command"
seed = 42
description = "fuzz fixture"
command = ["tool", "--seed", "{seed}", "--out", "{out}"]
timeout_secs = 30
inputs = ["data/a.ron", "data/b/"]
outputs = ["assets/gen/a.ron", "assets/gen/b/"]
"#;

/// The built-in heightfield kind, with its parameter table.
const GENERATOR_HEIGHTFIELD_TOML: &str = r#"format_version = 1

[generator]
kind = "heightfield"
seed = 7
outputs = ["assets/gen/ground.r16"]

[generator.heightfield]
width = 65
height = 33
octaves = 4
frequency = 0.03125
lacunarity = 2.0
gain = 0.5
"#;

fn generator_load(s: &str) -> Result<usize, String> {
    let def =
        regia_asset_pipeline::generators::parse_generator("fuzz", s.as_bytes()).map_err(|e| e.to_string())?;
    // Consume: the declared outputs and the version probe agree with a successful parse.
    assert_eq!(
        regia_asset_pipeline::generators::generator_file_version(s.as_bytes()),
        Ok(Some(1))
    );
    Ok(def.output_paths().len())
}

#[test]
fn generator_toml_never_panics() {
    fuzz_text_loader(GENERATOR_TOML, 0x6E40_0001, N / 4, generator_load);
    fuzz_text_values_loader(GENERATOR_TOML, 0x6E40_0002, N / 4, generator_load);
    fuzz_text_loader(GENERATOR_HEIGHTFIELD_TOML, 0x6E40_0003, N / 4, generator_load);
    fuzz_text_values_loader(GENERATOR_HEIGHTFIELD_TOML, 0x6E40_0004, N / 4, generator_load);
}

/// ADR-2697: the `import.font_subset` registry entry accepts its fixture and survives mutated fonts.
#[test]
fn registry_font_subset_entry_never_panics() {
    use regia_asset_pipeline::fuzz_entry::{font_fixture, font_load};
    font_load(&font_fixture()).expect("the font fixture subsets");
    fuzz_loader(&font_fixture(), 0xF047, N, font_load);
}

/// ADR-2532: the `import.bvh` registry entry accepts its fixture and survives mutated text and mutated numbers.
#[test]
fn registry_bvh_entry_never_panics() {
    use regia_asset_pipeline::fuzz_entry::{bvh_fixture, bvh_load};
    bvh_load(&bvh_fixture()).expect("the BVH fixture imports");
    fuzz_text_loader(&bvh_fixture(), 0xB7A1, N, bvh_load);
    fuzz_text_values_loader(&bvh_fixture(), 0xB7A2, N, bvh_load);
}

/// ADR-3350: the `import.svg` registry entry accepts its fixture and survives mutated documents and numbers.
#[test]
fn registry_svg_entry_never_panics() {
    use regia_asset_pipeline::fuzz_entry::{svg_fixture, svg_load};
    svg_load(&svg_fixture()).expect("the SVG fixture imports");
    fuzz_text_loader(&svg_fixture(), 0x5C61, N * 3, svg_load);
    fuzz_text_values_loader(&svg_fixture(), 0x5C62, N * 3, svg_load);
}

/// ADR-3266: the `import.dds` registry entry accepts its fixture (a BC1 cube map) and survives mutated files.
#[test]
fn registry_dds_entry_never_panics() {
    use regia_asset_pipeline::fuzz_entry::{dds_fixture, dds_load};
    dds_load(&dds_fixture()).expect("the DDS fixture imports");
    fuzz_loader(&dds_fixture(), 0xDD5C, 3 * N, dds_load);
}

/// ADR-3244: the `import.ply`, `import.stl` and `import.psd` registry entries accept their fixtures and survive
/// mutated bytes.
#[test]
fn registry_ply_stl_psd_entries_never_panic() {
    use regia_asset_pipeline::fuzz_entry::{
        ply_fixture, ply_load, psd_fixture, psd_load, stl_fixture, stl_load,
    };
    ply_load(&ply_fixture()).expect("the PLY fixture imports");
    stl_load(&stl_fixture()).expect("the STL fixture imports");
    psd_load(&psd_fixture()).expect("the PSD fixture imports");
    fuzz_loader(&ply_fixture(), 0x91A1, N, ply_load);
    fuzz_text_loader(&stl_fixture(), 0x91A2, N, stl_load);
    fuzz_text_values_loader(&stl_fixture(), 0x91A4, N, stl_load);
    fuzz_loader(&psd_fixture(), 0x91A3, N, psd_load);
}

/// ADR-3235: the `import.usd` and `import.usdz` registry entries accept their fixtures and survive mutated text, mutated
/// numbers and mutated package bytes (a binary crate layer inside); the reader never panics on them (a caught panic
/// would surface as `usd_reader_failed`).
#[test]
fn registry_usd_entries_never_panic() {
    use regia_asset_pipeline::fuzz_entry::{usd_fixture, usd_load, usdz_fixture, usdz_load};
    usd_load(&usd_fixture()).expect("the USD fixture imports");
    usdz_load(&usdz_fixture()).expect("the USDZ fixture imports");
    let no_reader_failure_text = |text: &str| -> Result<String, String> {
        let result = usd_load(text);
        if let Err(error) = &result {
            assert!(
                !error.contains("usd_reader_failed"),
                "the reader panicked: {error}"
            );
        }
        result
    };
    let no_reader_failure_bytes = |bytes: &[u8]| -> Result<Vec<u8>, String> {
        let result = usdz_load(bytes);
        if let Err(error) = &result {
            assert!(
                !error.contains("usd_reader_failed"),
                "the reader panicked: {error}"
            );
        }
        result
    };
    fuzz_text_loader(&usd_fixture(), 0x05D0_0001, N, no_reader_failure_text);
    fuzz_text_values_loader(&usd_fixture(), 0x05D0_0002, N, no_reader_failure_text);
    fuzz_loader(&usdz_fixture(), 0x05D0_0003, N, no_reader_failure_bytes);
}

/// ADR-3262: every importer entry of `fuzz_entry::more_entries` (Radiance HDR, EXR, Tiled, IES, audio, WebM, raw
/// heightmaps, atlas, FBX) accepts its fixture and survives mutated bytes (or text, and text with corrupted values).
#[test]
fn more_import_entries_never_panic() {
    use regia_asset_pipeline::fuzz_entry::{ImportEntryKind, more_entries};
    for (i, entry) in more_entries().into_iter().enumerate() {
        let seed = 0x1A90_0000 + i as u64 * 2;
        // The file-backed entries (Tiled, atlas) write a scratch directory per input, so they get fewer.
        let n = if entry.name.contains("tiled") || entry.name.contains("atlas") {
            N / 4
        } else {
            N / 2
        };
        match entry.kind {
            ImportEntryKind::Bytes { fixture, load } => {
                load(&fixture())
                    .unwrap_or_else(|e| panic!("{}: the fixture does not import: {e}", entry.name));
                // A raw heightmap has no magic (any leading word is a height), and a WebM reads up to its
                // last complete packet (a cut is valid input): the harness's headerless and tolerant loops.
                match entry.name {
                    "import.heightmap_r16" => {
                        fuzz_headerless_loader(&fixture(), seed, n, load);
                    }
                    "import.webm" => {
                        fuzz_tolerant_loader(&fixture(), seed, n, load);
                    }
                    _ => {
                        fuzz_loader(&fixture(), seed, n, load);
                    }
                }
            }
            ImportEntryKind::Text { fixture, load } => {
                load(&fixture())
                    .unwrap_or_else(|e| panic!("{}: the fixture does not import: {e}", entry.name));
                fuzz_text_loader(&fixture(), seed, n, load);
                fuzz_text_values_loader(&fixture(), seed + 1, n, load);
            }
        }
    }
}

/// The soak of the ADR-3262 entries: `REGIA_FUZZ_SOAK_SEEDS` seeds (default 3000) of 4 mutations each per entry, in
/// process. Prints one line per entry. `cargo test -p regia_asset_pipeline --test fuzz -- --ignored more_import_soak
/// --nocapture`; `REGIA_FUZZ_SOAK_PREFIX=import.tiled` limits it to the entries with that name prefix, and
/// `REGIA_FUZZ_SOAK_FIRST_SEED` moves the first seed.
#[test]
#[ignore = "long soak; run by hand"]
fn more_import_soak_over_many_seeds() {
    use regia_asset_pipeline::fuzz_entry::{ImportEntryKind, more_entries};
    use regia_test_utils::fuzz::{FuzzRng, mutate_bytes, mutate_text, mutate_text_values};
    let env = |name: &str| std::env::var(name).ok();
    let seeds: u64 = env("REGIA_FUZZ_SOAK_SEEDS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let first: u64 = env("REGIA_FUZZ_SOAK_FIRST_SEED")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let prefix = env("REGIA_FUZZ_SOAK_PREFIX").unwrap_or_default();
    let mut all_panics = Vec::new();
    for entry in more_entries().into_iter().filter(|e| e.name.starts_with(&prefix)) {
        let (mut accepted, mut rejected) = (0u64, 0u64);
        let mut panics = Vec::new();
        for seed in first..first + seeds {
            let mut rng = FuzzRng::new(seed);
            for i in 0..4 {
                let outcome = match entry.kind {
                    ImportEntryKind::Bytes { fixture, load } => {
                        let input = mutate_bytes(&fixture(), &mut rng);
                        std::panic::catch_unwind(|| load(&input).is_ok())
                    }
                    ImportEntryKind::Text { fixture, load } => {
                        let input = if i % 2 == 0 {
                            mutate_text(&fixture(), &mut rng)
                        } else {
                            mutate_text_values(&fixture(), &mut rng)
                        };
                        std::panic::catch_unwind(|| load(&input).is_ok())
                    }
                };
                match outcome {
                    Ok(true) => accepted += 1,
                    Ok(false) => rejected += 1,
                    Err(_) => panics.push(format!("{} seed {seed} mutation {i}", entry.name)),
                }
            }
        }
        eprintln!(
            "{}: {seeds} seeds, accepted {accepted}, rejected {rejected}, panics {}",
            entry.name,
            panics.len()
        );
        all_panics.extend(panics);
    }
    assert!(all_panics.is_empty(), "panics: {all_panics:?}");
}

/// A `.texpack.ron` (ADR-3090) loads, plans its own structure, or fails with a documented
/// `texpack_*` code; never a panic. Mutations also reach `texpack_file_version`'s text scan.
fn texpack_load(s: &str) -> Result<String, String> {
    use regia_asset_pipeline::texture_pack::{TexturePackRecipe, texpack_file_version};
    let _ = texpack_file_version(s);
    match TexturePackRecipe::from_ron(s) {
        Ok(recipe) => {
            recipe
                .validate()
                .map_err(|e| format!("loaded but does not validate: {e}"))?;
            Ok(format!("{recipe:?}"))
        }
        Err(e) if e.code().starts_with("texpack_") => Err(e.to_string()),
        Err(e) => Err(format!("undocumented code {}", e.code())),
    }
}

#[test]
fn texpack_ron_never_panics() {
    let fixture = regia_asset_pipeline::texture_pack::EXAMPLE_RECIPE;
    texpack_load(fixture).expect("the example loads");
    fuzz_text_loader(fixture, 0x7E9A_0001, N / 4, texpack_load);
    fuzz_text_values_loader(fixture, 0x7E9A_0002, N / 4, texpack_load);
}

#[test]
fn launch_profiles_ron_never_panics() {
    use regia_asset_pipeline::launch_profiles::LaunchProfiles;
    let fixture = include_str!("../../regia_agent/tests/fixtures/format-corpus/launch-profiles/v1.ron");
    let check = |text: &str| {
        if let Ok(file) = LaunchProfiles::from_ron(text) {
            let encoded = file.to_ron().expect("accepted input serializes");
            assert_eq!(LaunchProfiles::from_ron(&encoded).expect("round trip"), file);
        }
    };
    check(fixture);
    for (index, _) in fixture.char_indices() {
        check(&fixture[..index]);
        for mask in [1, 32, 127] {
            let mut bytes = fixture.as_bytes().to_vec();
            bytes[index] ^= mask;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                check(text);
            }
        }
    }
}

/// `config/source_lint.ron` (ADR-8248) loads or returns `content_lint_policy`; it never panics.
#[test]
fn source_lint_policy_never_panic() {
    use regia_asset_pipeline::source_lint::SourceLintPolicy;
    let fixture = include_str!("../../regia_agent/tests/fixtures/format-corpus/source-lint/v1.ron");
    let load = |text: &str| SourceLintPolicy::from_ron(text).map(|policy| policy.format_version);
    assert_eq!(load(fixture).expect("the v1 corpus seed loads"), 1);
    fuzz_text_loader(fixture, 0xC702_0001, N / 4, load);
    fuzz_text_values_loader(fixture, 0xC702_0002, N / 4, load);
}
