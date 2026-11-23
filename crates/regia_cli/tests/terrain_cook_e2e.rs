// SPDX-License-Identifier: MPL-2.0
//! Terrain cook end-to-end: `regia cook` on synthetic terrain folders cooks every slot through
//! the tiled path, the tiles resolve through `AssetServer`/pak + `regia_terrain::stream`, the
//! generic-path files no longer appear for those inputs, and the cooked bytes are deterministic
//! across two `--clean` runs.
//!
//! Also covers the round-2 send-back cases: the visual-sibling heightfield + extents layout,
//! missing optional slots (warning + neutral defaults / omission), stale generic-cooked entries
//! dropped on an incremental recook, an unresolvable layer file left uncooked, and the Android
//! ASTC 4×4 tiled cook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use image::ExtendedColorType;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use regia_asset::{AssetServer, AssetServerSettings, AssetSource, load_texture_bytes};
use regia_terrain::layers::{LayerDesc, TERRAIN_LAYERS_FORMAT_VERSION, TerrainLayers};
use regia_terrain::stream::{
    PakTileSource, TileKey, TileSource, load_terrain_index_bytes, tail_asset_id, tail_key, tile_asset_id,
};

/// The `regia` CLI binary (cargo builds it for this test).
fn regia_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_regia"))
}

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Square test resolution (power of two, a multiple of the 256 tile size → 2×2 tiles at mip 0).
const SIZE: u32 = 512;

/// A deterministic 8-bit LCG for synthesising test images (mirrors the pipeline's `synth_rgba`).
fn synth(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *state
}

fn write_png(path: &Path, width: u32, height: u32, data: &[u8], color: ExtendedColorType) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let file = std::fs::File::create(path).expect("create png");
    PngEncoder::new(file)
        .write_image(data, width, height, color)
        .expect("encode png");
}

fn write_rgba_png(path: &Path, seed: u32) {
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let mut state = seed | 1;
    for _ in 0..SIZE * SIZE {
        let s = synth(&mut state);
        rgba.extend_from_slice(&[(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8, s as u8]);
    }
    write_png(path, SIZE, SIZE, &rgba, ExtendedColorType::Rgba8);
}

fn write_l8_png(path: &Path, seed: u32) {
    let mut l = Vec::with_capacity((SIZE * SIZE) as usize);
    let mut state = seed | 1;
    for _ in 0..SIZE * SIZE {
        l.push((synth(&mut state) >> 24) as u8);
    }
    write_png(path, SIZE, SIZE, &l, ExtendedColorType::L8);
}

fn write_l16_png(path: &Path, seed: u32) {
    let mut raw = Vec::with_capacity((SIZE * SIZE * 2) as usize);
    let mut state = seed | 1;
    for _ in 0..SIZE * SIZE {
        let s = (synth(&mut state) >> 16) as u16;
        raw.extend_from_slice(&s.to_le_bytes());
    }
    write_png(path, SIZE, SIZE, &raw, ExtendedColorType::L16);
}

/// Writes `Regia.toml` for a test project.
fn write_project_ron(project: &Path) {
    std::fs::create_dir_all(project).expect("project dir");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"terrain-cook-e2e\"\nid = \"dev.regia.terrain_cook_e2e\"\nversion = \"0.1.0\"\n",
    )
    .expect("Regia.toml");
}

/// Writes a frozen v1 layer stack with the given layer names (single `ground`-like descriptor).
fn write_layers_ron(project: &Path, names: &[&str]) {
    let tex = project.join("assets").join("textures");
    std::fs::create_dir_all(&tex).expect("assets/textures");
    let layers = TerrainLayers {
        version: TERRAIN_LAYERS_FORMAT_VERSION,
        wetness_default: 0.0,
        layers: names
            .iter()
            .enumerate()
            .map(|(i, name)| LayerDesc {
                name: (*name).to_string(),
                albedo: if i == 0 {
                    [0.18, 0.28, 0.10]
                } else {
                    [0.42, 0.38, 0.34]
                },
                roughness: if i == 0 { 0.95 } else { 0.85 },
                ..LayerDesc::ground()
            })
            .collect(),
        puddles: true,
    };
    std::fs::write(
        tex.join("terrain.layers.ron"),
        layers.to_ron().expect("valid stack"),
    )
    .expect("layer ron");
}

/// Builds a synthetic terrain folder with every slot (2 layers).
fn build_fixture(project: &Path) {
    write_project_ron(project);
    write_layers_ron(project, &["grass", "rock"]);
    let tex = project.join("assets").join("textures");

    // Heightfield + world extents.
    write_l16_png(&tex.join("heightfield.png"), 7);
    std::fs::write(
        tex.join("heightfield.json"),
        r#"{"size_m": 1000.0, "origin": [0.0, 0.0, 0.0], "extra": "ignored"}"#,
    )
    .expect("heightfield.json");

    // Macro slots: albedo at the top level, the rest under `macro/`.
    write_rgba_png(&tex.join("macro_albedo.png"), 11);
    write_rgba_png(&tex.join("macro").join("normal.png"), 12);
    write_l8_png(&tex.join("macro").join("ao.png"), 13);
    write_rgba_png(&tex.join("macro").join("pack0.png"), 14);
    write_rgba_png(&tex.join("macro").join("pack1.png"), 15);

    // Two layers × (albedo, normal, orm, height).
    for (i, name) in ["grass", "rock"].into_iter().enumerate() {
        let base = tex.join("detail").join(name);
        write_rgba_png(&base.join("albedo.png"), 20 + i as u32);
        write_rgba_png(&base.join("normal.png"), 22 + i as u32);
        write_rgba_png(&base.join("orm.png"), 24 + i as u32);
        write_l8_png(&base.join("height.png"), 26 + i as u32);
    }
}

/// Runs `regia cook` with extra args and returns the captured output.
fn cook_output(project: &Path, extra: &[&str]) -> Output {
    let mut cmd = Command::new(regia_cli());
    cmd.arg("cook").arg("--project").arg(project);
    cmd.args(extra);
    cmd.output().expect("run regia cook")
}

fn assert_cook_ok(out: &Output, ctx: &str) {
    assert!(
        out.status.success(),
        "{ctx}: regia cook failed:\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs `regia cook` (the real CLI entry) with `--clean`.
fn run_cook(project: &Path) {
    assert_cook_ok(
        &cook_output(project, &["--platform", "linux", "--clean"]),
        "clean cook",
    );
}

/// Opens an AssetServer reading the project's cooked cache (no paks mounted unless `content_dir`).
fn open_server(project: &Path, platform: &str, content_dir: Option<&Path>) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: project.to_path_buf(),
        platform: platform.to_string(),
        content_dir: content_dir.map(Path::to_path_buf),
    })
    .expect("open asset server")
}

/// Snapshots every cooked `.fckd` file in a cache directory (name → bytes).
fn snapshot_cache(cache: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for entry in std::fs::read_dir(cache).expect("read cache dir") {
        let entry = entry.expect("entry");
        if entry.path().extension().and_then(|e| e.to_str()) == Some("fckd") {
            let bytes = std::fs::read(entry.path()).expect("read fckd");
            map.insert(entry.file_name().to_string_lossy().into_owned(), bytes);
        }
    }
    map
}

#[test]
fn terrain_cook_tiles_resolve_and_are_deterministic() {
    let project = std::env::temp_dir().join(format!("regia_terrain_cook_e2e_{}", std::process::id()));
    let _cleanup = Cleanup(project.clone());
    build_fixture(&project);

    let cache = regia_asset::cooked_cache_dir(&project, "linux");

    // Two full `--clean` cooks must produce byte-identical output.
    run_cook(&project);
    let first = snapshot_cache(&cache);
    assert!(!first.is_empty(), "cook produced no cooked assets");
    run_cook(&project);
    let second = snapshot_cache(&cache);
    assert_eq!(first, second, "two cooks must be byte-identical");

    // Load through the AssetServer with the generated pak mounted.
    let pak_dir = project.join("build").join("paks").join("linux");
    let mut server = open_server(&project, "linux", Some(&pak_dir));
    assert!(server.has_paks(), "the cook must produce a pak");
    assert_eq!(server.pak_count(), 1);

    // The terrain index is the manifest entry for the layer file.
    let index_id = server
        .id_for_path("assets/textures/terrain.layers.ron")
        .expect("terrain index manifest entry");
    let index_bytes = std::fs::read(cache.join(format!("{index_id}.fckd"))).expect("index file");
    let index = load_terrain_index_bytes(&index_bytes).expect("decode terrain index");
    let map_id = index.map_id;

    assert_eq!(
        index.world_size_m,
        Some(1000.0),
        "extents read from heightfield.json"
    );
    assert_eq!(
        index.world_origin,
        Some([0.0, 0.0, 0.0]),
        "world origin read from heightfield.json"
    );
    assert_eq!(index.layers, vec!["grass".to_string(), "rock".to_string()]);
    let hf = index.heightfield.as_ref().expect("heightfield cooked");
    assert_eq!(hf.width, SIZE);
    assert_eq!(hf.tiles_x, 2, "512² heightfield at 256 → 2×2 tiles");

    // The macro + 2 layers × 4 slots all cooked: 5 macro + 8 layer slots.
    assert_eq!(index.slots.len(), 5 + 8, "every slot cooked");
    let slot_names: Vec<&str> = index.slots.iter().map(|s| s.name.as_str()).collect();
    for expected in [
        "macro_albedo",
        "macro_normal",
        "macro_ao",
        "macro_pack0",
        "macro_pack1",
        "layer0_albedo",
        "layer0_orm",
        "layer1_height",
    ] {
        assert!(
            slot_names.contains(&expected),
            "missing slot {expected}: {slot_names:?}"
        );
    }

    // A mip-0 tile is a 256×256 KTX2 (the tile size), not the 512 source size.
    let tile_id = tile_asset_id(map_id, &index.slots[0].name, 0, 0, 0);
    let (_h, tex) = load_texture_bytes(
        &std::fs::read(cache.join(regia_asset::cooked_file_name(tile_id))).expect("tile file"),
    )
    .expect("decode tile");
    assert_eq!(tex.width, 256, "mip-0 tile KTX2 width must be the tile size");
    assert_eq!(tex.height, 256, "mip-0 tile KTX2 height must be the tile size");

    // Tiles resolve through the stream (paks first, then cache).
    let source = PakTileSource::new(map_id, Some(&pak_dir), cache.clone());
    for (i, slot) in index.slots.iter().enumerate() {
        let texture = index.texture(i as u8).expect("texture");
        let tail = source.load(&texture, tail_key(i as u8)).expect("tail resolves");
        assert!(!tail.is_empty(), "tail empty for {}", slot.name);
        if texture.tiles_per_side(0) > 0 {
            for (x, y) in [(0u32, 0u32), (1, 1)] {
                let tile = source
                    .load(
                        &texture,
                        TileKey {
                            texture: i as u8,
                            mip: 0,
                            x,
                            y,
                        },
                    )
                    .expect("tile resolves");
                assert!(!tile.is_empty(), "tile ({x},{y}) empty for {}", slot.name);
            }
        }
    }

    // Generic-path files no longer appear for the terrain inputs.
    for path in [
        "assets/textures/heightfield.png",
        "assets/textures/macro_albedo.png",
        "assets/textures/macro/pack0.png",
        "assets/textures/detail/grass/albedo.png",
    ] {
        assert!(
            server.id_for_path(path).is_none(),
            "{path} must not be cooked generically"
        );
    }

    // A tile is loadable through the AssetServer and served from the pak.
    let tail_id = tail_asset_id(map_id, &index.slots[0].name);
    let _handle = server
        .load_texture(tail_id)
        .expect("load tile texture via AssetServer");
    assert_eq!(server.source_of(tail_id), AssetSource::Pak("base".to_string()));

    // `--stats` reports the actual tiled sizes per slot and per map. The tiled block is the only
    // place the word `tiles` appears (the budget block uses MiB/GiB), so its presence proves the
    // tiled stats block rendered rather than the budget header.
    let stats = cook_output(&project, &["--stats", "--platform", "linux"]);
    assert!(stats.status.success());
    let stdout = String::from_utf8_lossy(&stats.stdout);
    assert!(stdout.contains("cooked terrain maps:"), "stats: {stdout}");
    assert!(stdout.contains("macro_albedo"), "stats lists slots: {stdout}");
    assert!(stdout.contains("tiles"), "stats lists tiled sizes: {stdout}");
}

#[test]
fn visual_sibling_heightfield_reads_its_own_extents() {
    let project = std::env::temp_dir().join(format!("regia_terrain_visual_{}", std::process::id()));
    let _cleanup = Cleanup(project.clone());
    write_project_ron(&project);
    write_layers_ron(&project, &["ground"]);

    // Heightfield + extents live in the sibling `visual/` folder, not next to the layer file.
    let visual = project.join("assets").join("visual");
    write_l16_png(&visual.join("heightfield.png"), 7);
    std::fs::write(
        visual.join("heightfield.json"),
        r#"{"size_m": 2000.0, "origin": [-500.0, 10.0, -500.0]}"#,
    )
    .expect("visual heightfield.json");

    // One layer texture so the terrain is a real map (heightfield alone would also suffice).
    let tex = project.join("assets").join("textures");
    write_rgba_png(&tex.join("detail").join("ground").join("albedo.png"), 21);

    run_cook(&project);
    let cache = regia_asset::cooked_cache_dir(&project, "linux");
    let server = open_server(&project, "linux", None);
    let index_id = server
        .id_for_path("assets/textures/terrain.layers.ron")
        .expect("terrain index");
    let index =
        load_terrain_index_bytes(&std::fs::read(cache.join(format!("{index_id}.fckd"))).expect("index file"))
            .expect("decode index");

    // The extents come from the *visual* sibling json (the round-2 bug: they stayed None).
    assert_eq!(index.world_size_m, Some(2000.0));
    assert_eq!(index.world_origin, Some([-500.0, 10.0, -500.0]));
    let hf = index.heightfield.as_ref().expect("heightfield cooked");
    assert_eq!(hf.width, SIZE);
}

#[test]
fn missing_optional_slots_warn_without_failing() {
    let project = std::env::temp_dir().join(format!("regia_terrain_missing_{}", std::process::id()));
    let _cleanup = Cleanup(project.clone());
    write_project_ron(&project);
    write_layers_ron(&project, &["ground"]);
    let tex = project.join("assets").join("textures");

    // Heightfield only; a layer with albedo + normal but no orm/height; no macro at all.
    write_l16_png(&tex.join("heightfield.png"), 7);
    let base = tex.join("detail").join("ground");
    write_rgba_png(&base.join("albedo.png"), 21);
    write_rgba_png(&base.join("normal.png"), 22);

    let out = cook_output(&project, &["--platform", "linux", "--clean"]);
    assert_cook_ok(&out, "missing-slots cook");
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Missing orm/height warn (neutral default), missing macro slots warn and are omitted.
    assert!(stderr.contains("cooking neutral default"), "stderr: {stderr}");
    assert!(stderr.contains("missing macro slot"), "stderr: {stderr}");

    let cache = regia_asset::cooked_cache_dir(&project, "linux");
    let server = open_server(&project, "linux", None);
    let index_id = server
        .id_for_path("assets/textures/terrain.layers.ron")
        .expect("terrain index");
    let index =
        load_terrain_index_bytes(&std::fs::read(cache.join(format!("{index_id}.fckd"))).expect("index file"))
            .expect("decode index");

    let names: Vec<&str> = index.slots.iter().map(|s| s.name.as_str()).collect();
    // The neutral-default orm/height slots are still in the index…
    assert!(
        names.contains(&"layer0_orm"),
        "orm neutral default present: {names:?}"
    );
    assert!(
        names.contains(&"layer0_height"),
        "height neutral default present: {names:?}"
    );
    assert!(names.contains(&"layer0_albedo"));
    assert!(names.contains(&"layer0_normal"));
    // …and the missing macro slots are omitted entirely.
    assert!(
        !names.iter().any(|n| n.starts_with("macro_")),
        "no macro slots: {names:?}"
    );
    assert_eq!(index.slots.len(), 4, "only the four layer slots: {names:?}");
}

#[test]
fn incremental_recook_drops_stale_generic_terrain_inputs() {
    let project = std::env::temp_dir().join(format!("regia_terrain_stale_{}", std::process::id()));
    let _cleanup = Cleanup(project.clone());
    write_project_ron(&project);
    // No layer file yet: the PNGs cook through the generic single-texture path.
    let tex = project.join("assets").join("textures");
    std::fs::create_dir_all(tex.join("detail").join("grass")).expect("mkdir");
    write_l16_png(&tex.join("heightfield.png"), 7);
    write_rgba_png(&tex.join("macro_albedo.png"), 11);
    write_rgba_png(&tex.join("detail").join("grass").join("albedo.png"), 21);
    write_rgba_png(&tex.join("detail").join("grass").join("normal.png"), 22);

    run_cook(&project);
    let server = open_server(&project, "linux", None);
    assert!(
        server.id_for_path("assets/textures/heightfield.png").is_some(),
        "generic heightfield entry planted"
    );
    assert!(
        server.id_for_path("assets/textures/macro_albedo.png").is_some(),
        "generic texture entry planted"
    );

    // Add the layer file: those PNGs become terrain inputs.
    write_layers_ron(&project, &["grass"]);

    // Incremental recook (no --clean): the stale generic entries must be dropped.
    let out = cook_output(&project, &["--platform", "linux"]);
    assert_cook_ok(&out, "incremental recook");

    let server = open_server(&project, "linux", None);
    for path in [
        "assets/textures/heightfield.png",
        "assets/textures/macro_albedo.png",
        "assets/textures/detail/grass/albedo.png",
        "assets/textures/detail/grass/normal.png",
    ] {
        assert!(
            server.id_for_path(path).is_none(),
            "{path} must not keep a stale generic entry"
        );
    }
    // The terrain itself cooked through the tiled path.
    assert!(
        server.id_for_path("assets/textures/terrain.layers.ron").is_some(),
        "terrain index present after recook"
    );
}

#[test]
fn unresolvable_layer_file_stays_uncooked() {
    let project = std::env::temp_dir().join(format!("regia_terrain_empty_{}", std::process::id()));
    let _cleanup = Cleanup(project.clone());
    write_project_ron(&project);
    write_layers_ron(&project, &["ground"]);
    // No heightfield, no layer textures, no macro: an empty terrain.

    let out = cook_output(&project, &["--platform", "linux", "--clean"]);
    assert_cook_ok(&out, "empty-terrain cook");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no resolvable heightfield or layer texture"),
        "stderr: {stderr}"
    );

    // No terrain index: the layer file has no manifest entry.
    let server = open_server(&project, "linux", None);
    assert!(
        server.id_for_path("assets/textures/terrain.layers.ron").is_none(),
        "empty terrain must not produce a terrain index"
    );
}

fn write_constant_rgba(path: &Path, size: u32, texel: [u8; 4]) {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for _ in 0..size * size {
        rgba.extend_from_slice(&texel);
    }
    write_png(path, size, size, &rgba, ExtendedColorType::Rgba8);
}

fn write_constant_l8(path: &Path, size: u32, value: u8) {
    let l = vec![value; (size * size) as usize];
    write_png(path, size, size, &l, ExtendedColorType::L8);
}

#[test]
fn android_cooks_tiled_astc_for_height_and_splat() {
    let project = regia_test_utils::unique_temp_dir("regia_terrain_android_astc");
    let _cleanup = Cleanup(project.clone());
    write_project_ron(&project);
    write_layers_ron(&project, &["ground"]);
    let tex = project.join("assets").join("textures");
    write_l16_png(&tex.join("heightfield.png"), 7);
    // 256 divides the default tile, and a constant color takes the void-extent path.
    let edge = 256u32;
    write_constant_rgba(&tex.join("macro_albedo.png"), edge, [10, 20, 30, 255]);
    write_constant_rgba(&tex.join("macro").join("pack0.png"), edge, [40, 50, 60, 255]);
    write_constant_l8(&tex.join("detail").join("ground").join("height.png"), edge, 128);

    let out = cook_output(&project, &["--platform", "android"]);
    assert_cook_ok(&out, "android cook");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("ASTC tiled cook is not implemented"),
        "the tiled ASTC path must cook, not warn: {stderr}"
    );

    // Every terrain input is a tiled-path input on Android too (ADR-5294): no generic rows; the
    // level's named terrain still finds its tiled heightfield (ADR-4914's contract).
    let server = open_server(&project, "android", None);
    assert!(
        server.id_for_path("assets/textures/heightfield.png").is_none(),
        "heightfield is a terrain input, not a generic cook"
    );
    assert!(
        server.id_for_path("assets/textures/macro_albedo.png").is_none(),
        "albedo is a terrain input, not a generic cook"
    );
    assert!(
        server
            .id_for_path("assets/textures/terrain.layers.ron#terrain/heightfield")
            .is_some(),
        "tiled heightfield on android"
    );
    let index_id = server
        .id_for_path("assets/textures/terrain.layers.ron")
        .expect("terrain index");
    let cache = regia_asset::cooked_cache_dir(&project, "android");
    let index_bytes = std::fs::read(cache.join(format!("{index_id}.fckd"))).expect("index file");
    let index = load_terrain_index_bytes(&index_bytes).expect("decode terrain index");
    assert!(index.heightfield.is_some(), "heightfield tiled");
    let names: Vec<&str> = index.slots.iter().map(|s| s.name.as_str()).collect();
    for slot in ["macro_albedo", "macro_pack0", "layer0_height"] {
        assert!(names.contains(&slot), "missing {slot}: {names:?}");
    }
    for slot in &index.slots {
        if !matches!(
            slot.name.as_str(),
            "macro_albedo" | "macro_pack0" | "layer0_height"
        ) {
            continue;
        }
        let tile_id = tile_asset_id(index.map_id, &slot.name, 0, 0, 0);
        let file = std::fs::read(cache.join(format!("{tile_id}.fckd"))).expect("tile");
        let (_, texture) = load_texture_bytes(&file).expect("cooked texture");
        assert!(texture.ktx2.len() >= 16, "{} ktx2 too short", slot.name);
        let vk = u32::from_le_bytes(texture.ktx2[12..16].try_into().expect("vkFormat"));
        let expect = if slot.name == "macro_albedo" { 158 } else { 157 };
        assert_eq!(vk, expect, "{} vkFormat", slot.name);
        assert!(slot.tile_count >= 1, "{}", slot.name);
    }
}

/// The terrain size rule is the same on Android as on desktop (fixrev-terrain-tiles-g, review
/// finding 2 on ADR-5294): `regia cook` tiles every slot at 256, so a slot smaller than the tile
/// (128², a power of two and a multiple of the ASTC block) or not a power of two (96²) fails the
/// cook with the tile-size error. `terrain_astc_alignment` is an API-level guard of
/// `cook_tiled_texture` that this cook never raises.
#[test]
fn a_slot_smaller_than_the_tile_fails_with_the_tile_size_error_on_both_platforms() {
    for platform in ["android", "linux"] {
        for edge in [128u32, 96] {
            let project = regia_test_utils::unique_temp_dir("regia_terrain_small_slot");
            let _cleanup = Cleanup(project.clone());
            write_project_ron(&project);
            write_layers_ron(&project, &["ground"]);
            let tex = project.join("assets").join("textures");
            write_l16_png(&tex.join("heightfield.png"), 7);
            write_constant_rgba(&tex.join("macro_albedo.png"), edge, [10, 20, 30, 255]);

            let out = cook_output(&project, &["--platform", platform]);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !out.status.success(),
                "{platform} {edge}²: a slot under the tile size must fail the cook: {stderr}"
            );
            assert!(
                stderr.contains(&format!(
                    "tile size 256 and the map size {edge} must both be powers of two, the tile dividing the map"
                )),
                "{platform} {edge}²: {stderr}"
            );
            assert!(
                !stderr.contains("terrain_astc_alignment"),
                "{platform} {edge}²: {stderr}"
            );
        }
    }
}
