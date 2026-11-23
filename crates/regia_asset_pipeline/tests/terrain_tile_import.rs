// SPDX-License-Identifier: MPL-2.0
//! ADR-8595 (CT-22): a terrain heightfield cooked from multi-file source tiles
//! (`heightfield.tiles.json`) equals the cook of the same samples as one monolithic source, the
//! tile set is validated (overlap, gap, misregistration, seams) before anything is published, the
//! descriptor's tile order never changes the output, the charged sample buffers depend on the
//! tile size and not the world area, and a cancelled cook leaves the old map while a recook
//! rebuilds only the cooked tiles a changed source tile covers. CPU only; original fixtures.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use regia_asset::Heightfield;
use regia_asset::{AssetId, cooked_cache_dir, cooked_file_name};
use regia_asset_pipeline::terrain_cook::{SourceTileProgress, TILE_SET_FILE, cook_tile_set_heightfield};
use regia_asset_pipeline::terrain_data::cook_tiled_heightfield;
use regia_asset_pipeline::{CookOptions, CookReport, cook_project};
use regia_terrain::source::{CountMapping, ImportLimits, TerrainSource, TileSetDescriptor};
use regia_terrain::stream::{
    decode_cooked_tiled_heightfield, heightfield_asset_id, load_terrain_index_bytes,
    stitch_tiled_heightfield, terrain_index_asset_id,
};

const DIR: &str = "assets/maps/basin";
const LAYER: &str = "assets/maps/basin/basin.terrain.layers.ron";
const PLATFORM: &str = "linux";
const CELL: f32 = 0.5;
const MPC: f32 = 0.03125;
const ZERO: u16 = 20_000;
const SAMPLE0: [f32; 2] = [-10.0, 4.25];

/// The global count at a sample: deterministic, varied, nonzero datum.
fn count(col: u32, row: u32) -> u16 {
    19_000 + ((col * 37 + row * 101 + (col * row) % 13) % 2_003) as u16
}

fn mapping(cols: u32, rows: u32) -> CountMapping {
    CountMapping {
        cols,
        rows,
        meters_per_count: MPC,
        zero_counts: ZERO,
        cell_m: CELL,
        sample0_m: SAMPLE0,
    }
}

/// A source tile: origin and size in the global grid; written as JSON (sample0 at its origin's
/// world position) or as a 16-bit PNG.
#[derive(Clone, Copy)]
struct Tile {
    origin: [u32; 2],
    size: [u32; 2],
    png: bool,
}

const fn tile(x: u32, y: u32, w: u32, h: u32) -> Tile {
    Tile {
        origin: [x, y],
        size: [w, h],
        png: false,
    }
}

fn tile_name(t: &Tile) -> String {
    let ext = if t.png { "png" } else { "json" };
    format!("tiles/t_{}_{}.{ext}", t.origin[1], t.origin[0])
}

fn tile_counts(t: &Tile, f: &dyn Fn(u32, u32) -> u16) -> Vec<u16> {
    (0..t.size[1])
        .flat_map(|r| (0..t.size[0]).map(move |c| (c, r)))
        .map(|(c, r)| f(t.origin[0] + c, t.origin[1] + r))
        .collect()
}

fn write_tile(dir: &Path, t: &Tile, counts: Vec<u16>, sample0: [f32; 2]) {
    let path = dir.join(tile_name(t));
    std::fs::create_dir_all(path.parent().expect("parent")).expect("tiles dir");
    if t.png {
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(t.size[0], t.size[1], counts)
            .expect("image")
            .save(&path)
            .expect("png");
    } else {
        let doc = serde_json::json!({
            "schema": 2,
            "grid": {"cols": t.size[0], "rows": t.size[1], "cell_m": CELL,
                     "sample0_x_m": sample0[0], "sample0_z_m": sample0[1]},
            "units": {"kind": "counts", "meters_per_count": MPC, "zero_counts": ZERO},
            "counts": counts,
        });
        std::fs::write(&path, doc.to_string()).expect("tile json");
    }
}

fn descriptor_json(cols: u32, rows: u32, tiles: &[Tile]) -> String {
    let list: Vec<serde_json::Value> = tiles
        .iter()
        .map(|t| serde_json::json!({"path": tile_name(t), "origin": t.origin, "size": t.size}))
        .collect();
    serde_json::json!({
        "terrain_source_tiles": 1,
        "grid": {"cols": cols, "rows": rows, "cell_m": CELL, "sample0_m": SAMPLE0},
        "units": {"meters_per_count": MPC, "zero_counts": ZERO},
        "tiles": list,
    })
    .to_string()
}

/// Writes the tile files and the descriptor into `dir` with counts from `f`.
fn write_tile_set(dir: &Path, cols: u32, rows: u32, tiles: &[Tile], f: &dyn Fn(u32, u32) -> u16) {
    let m = mapping(cols, rows);
    for t in tiles {
        write_tile(
            dir,
            t,
            tile_counts(t, f),
            m.sample_world(t.origin[0], t.origin[1]),
        );
    }
    std::fs::write(dir.join(TILE_SET_FILE), descriptor_json(cols, rows, tiles)).expect("descriptor");
}

fn write_layer(root: &Path) {
    std::fs::create_dir_all(root.join(DIR)).expect("dirs");
    std::fs::write(
        root.join(LAYER),
        include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
    )
    .expect("layers");
}

/// The monolithic equivalent: `heightfield.png` with a schema-2 `heightfield.json`.
fn write_monolithic(root: &Path, cols: u32, rows: u32) {
    write_layer(root);
    let dir = root.join(DIR);
    let counts: Vec<u16> = (0..rows)
        .flat_map(|r| (0..cols).map(move |c| count(c, r)))
        .collect();
    image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(cols, rows, counts)
        .expect("image")
        .save(dir.join("heightfield.png"))
        .expect("png");
    let json = serde_json::json!({
        "schema": 2,
        "grid": {"cols": cols, "rows": rows, "cell_m": CELL, "sample0_x_m": SAMPLE0[0], "sample0_z_m": SAMPLE0[1]},
        "units": {"kind": "counts", "meters_per_count": MPC, "zero_counts": ZERO},
    });
    std::fs::write(dir.join("heightfield.json"), json.to_string()).expect("json");
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: PLATFORM.into(),
        incremental: false,
    })
    .expect("cook runs")
}

fn map_id(root: &Path) -> AssetId {
    let meta = std::fs::read(regia_asset::meta_path_for(&root.join(LAYER))).expect("layer meta");
    regia_asset::MetaSidecar::from_ron(&meta).expect("meta").id
}

/// Gives `to` the same layer `.meta` (map id) as `from`, so cooked ids and bytes can be compared.
fn share_meta(from: &Path, to: &Path) {
    let src = regia_asset::meta_path_for(&from.join(LAYER));
    let dst = regia_asset::meta_path_for(&to.join(LAYER));
    std::fs::copy(src, dst).expect("copy meta");
}

fn cooked_bytes(root: &Path) -> (Vec<u8>, Vec<u8>) {
    let cache = cooked_cache_dir(root, PLATFORM);
    let id = map_id(root);
    let index = std::fs::read(cache.join(cooked_file_name(terrain_index_asset_id(id)))).expect("index");
    let tiled = std::fs::read(cache.join(cooked_file_name(heightfield_asset_id(id)))).expect("tiled");
    (index, tiled)
}

/// Every file under `dir` with its bytes (to prove a refused cook changed nothing).
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// Two and three adjacent tiles (shared one-sample borders, one PNG tile) cook to the same
/// counts, mapping and world positions as the monolithic source of the same samples.
#[test]
fn adjacent_tiles_cook_like_the_equivalent_monolithic_source() {
    let (cols, rows) = (41, 30);
    let mut png_tile = tile(20, 15, 21, 15);
    png_tile.png = true;
    let layouts: [&[Tile]; 2] = [
        &[tile(0, 0, 21, 30), tile(20, 0, 21, 30)],
        &[tile(0, 0, 21, 30), tile(20, 0, 21, 16), png_tile],
    ];
    let mono = regia_test_utils::unique_temp_dir("terrain-tiles-mono");
    write_monolithic(&mono, cols, rows);
    let report = cook(&mono);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let (mono_index, mono_tiled) = cooked_bytes(&mono);
    let mono_mapping = load_terrain_index_bytes(&mono_index)
        .expect("index")
        .source_mapping
        .expect("mapping");
    for tiles in layouts {
        let root = regia_test_utils::unique_temp_dir("terrain-tiles-set");
        write_layer(&root);
        write_tile_set(&root.join(DIR), cols, rows, tiles, &count);
        share_meta(&mono, &root);
        let report = cook(&root);
        assert!(
            report.errors.is_empty(),
            "{} tiles: {:?}",
            tiles.len(),
            report.errors
        );
        let (index, tiled) = cooked_bytes(&root);
        assert_eq!(
            tiled,
            mono_tiled,
            "{} tiles: the cooked heightfield is byte-identical",
            tiles.len()
        );
        let loaded = load_terrain_index_bytes(&index).expect("index");
        let mapping = loaded.source_mapping.expect("mapping");
        assert_eq!(mapping.canonical_bytes(), mono_mapping.canonical_bytes());
        assert_eq!(
            index,
            mono_index,
            "{} tiles: the terrain index is byte-identical",
            tiles.len()
        );
        let stitched = stitch_tiled_heightfield(&decode_cooked_tiled_heightfield(&tiled).expect("decode"))
            .expect("stitch");
        assert_eq!(
            stitched.samples.len(),
            (cols * rows) as usize,
            "exact sample count"
        );
        // World positions: the tile set's mapping places every global sample where the
        // monolithic source does, and each JSON tile's own placement agrees with its origin.
        let mono_source =
            TerrainSource::from_counts(stitched.samples.clone(), mono_mapping, ImportLimits::default())
                .expect("mono source");
        for (c, r) in [(0, 0), (20, 0), (20, 15), (40, 29), (7, 23)] {
            assert_eq!(mapping.sample_world(c, r), mono_source.sample_world(c, r));
            assert_eq!(stitched.samples[(r * cols + c) as usize], count(c, r));
        }
        for t in tiles.iter().filter(|t| !t.png) {
            let bytes = std::fs::read(root.join(DIR).join(tile_name(t))).expect("tile");
            let own = TerrainSource::from_json(&bytes, ImportLimits::default()).expect("tile source");
            for (c, r) in [(0, 0), (t.size[0] - 1, t.size[1] - 1)] {
                assert_eq!(
                    own.sample_world(c, r),
                    mapping.sample_world(t.origin[0] + c, t.origin[1] + r)
                );
            }
        }
    }
}

/// Cooked at a small tile size (several cooked tiles, padded edges) the region-fed cook equals
/// `cook_tiled_heightfield` of the assembled grid, tile for tile.
#[test]
fn region_fed_cook_equals_the_whole_grid_cook_with_padding() {
    let (cols, rows) = (45, 37);
    let tiles = [
        tile(0, 0, 23, 19),
        tile(22, 0, 23, 19),
        tile(0, 18, 23, 19),
        tile(22, 18, 23, 19),
    ];
    let dir = regia_test_utils::unique_temp_dir("terrain-tiles-regions");
    write_tile_set(&dir, cols, rows, &tiles, &count);
    let cache = dir.join("cache");
    let id = AssetId(regia_core::id::Guid::from_u128(0x5eed_0001));
    let (index, mapping, report) =
        cook_tile_set_heightfield(id, &dir.join(TILE_SET_FILE), &cache, 16, |_| {
            ControlFlow::Continue(())
        })
        .expect("tile-set cook");
    assert_eq!([index.width, index.height, index.tile_size], [cols, rows, 16]);
    assert_eq!(mapping, self::mapping(cols, rows));
    assert_eq!(report.cooked_tiles, 9);
    assert_eq!(report.cooked_tiles_recooked, 9);
    assert!(report.seam_samples_checked > 0, "shared borders were compared");
    let whole = Heightfield {
        width: cols,
        height: rows,
        samples: (0..rows)
            .flat_map(|r| (0..cols).map(move |c| count(c, r)))
            .collect(),
    };
    let expected = cook_tiled_heightfield(&whole, 16).expect("whole cook");
    let file = std::fs::read(cache.join(cooked_file_name(heightfield_asset_id(id)))).expect("fckd");
    let got = decode_cooked_tiled_heightfield(&file).expect("decode");
    assert_eq!(got.tiles.len(), expected.tiles.len());
    for (a, b) in got.tiles.iter().zip(&expected.tiles) {
        assert_eq!([a.x, a.y], [b.x, b.y]);
        assert_eq!([a.min, a.max], [b.min, b.max]);
        assert_eq!(
            a.samples, b.samples,
            "tile ({}, {}) incl. border padding",
            a.x, a.y
        );
    }
}

/// A disagreeing shared border, a gap, a misregistered tile, a too-wide overlap and a wrong tile
/// size are refused with their codes, and the previously cooked map is left byte for byte.
#[test]
fn disagreeing_overlap_gap_and_misregistered_tiles_fail_before_publication() {
    let (cols, rows) = (41, 30);
    let good = [tile(0, 0, 21, 30), tile(20, 0, 21, 30)];
    let root = regia_test_utils::unique_temp_dir("terrain-tiles-refuse");
    write_layer(&root);
    let dir = root.join(DIR);
    write_tile_set(&dir, cols, rows, &good, &count);
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // The published map: the index, the cooked heightfield and the tile evidence.
    let cache = cooked_cache_dir(&root, PLATFORM);
    let published = || {
        let mut files = snapshot(&cache.join(regia_asset_pipeline::terrain_cook::TILE_EVIDENCE_DIR));
        let (index, tiled) = cooked_bytes(&root);
        files.insert(PathBuf::from("index"), index);
        files.insert(PathBuf::from("heightfield"), tiled);
        files
    };
    let before = published();
    assert_eq!(before.len(), 3);

    let expect_refused = |code: &str| {
        let report = cook(&root);
        let text = format!("{:?}", report.errors);
        assert!(text.contains(code), "expected {code}, got {text}");
        assert_eq!(published(), before, "{code}: nothing published");
    };

    // Seam: the right tile disagrees with the left on one shared border sample (col 20).
    let right = good[1];
    let mut counts = tile_counts(&right, &count);
    counts[5 * right.size[0] as usize] ^= 1;
    write_tile(&dir, &right, counts, mapping(cols, rows).sample_world(20, 0));
    expect_refused("terrain_tiles_seam");
    write_tile_set(&dir, cols, rows, &good, &count);

    // Misregistered: the right tile's own placement is half a cell off its origin.
    let at = mapping(cols, rows).sample_world(20, 0);
    write_tile(&dir, &right, tile_counts(&right, &count), [at[0] + 0.25, at[1]]);
    expect_refused("terrain_tiles_misregistered");
    write_tile_set(&dir, cols, rows, &good, &count);

    // Gap: the right tile starts one column late, leaving column 21 in no tile.
    let late = [tile(0, 0, 21, 30), tile(22, 0, 19, 30)];
    write_tile_set(&dir, cols, rows, &late, &count);
    expect_refused("terrain_tiles_gap");

    // Overlap wider than the shared border.
    let wide = [tile(0, 0, 23, 30), tile(20, 0, 21, 30)];
    write_tile_set(&dir, cols, rows, &wide, &count);
    expect_refused("terrain_tiles_overlap");

    // A tile whose file is not the size the descriptor says.
    write_tile_set(&dir, cols, rows, &good, &count);
    let short = tile(20, 0, 21, 29);
    // Same origin, so the same file name: the right tile's file now holds 29 rows.
    write_tile(&dir, &short, tile_counts(&short, &count), at);
    expect_refused("terrain_tiles_dimensions");

    // The descriptor alone refuses structure errors before any tile is read.
    let gap = TileSetDescriptor::from_json(
        descriptor_json(cols, rows, &late).as_bytes(),
        ImportLimits::default(),
    )
    .expect_err("gap");
    assert_eq!(gap.code(), "terrain_tiles_gap");
    let outside = TileSetDescriptor::from_json(
        descriptor_json(cols, rows, &[tile(0, 0, 21, 30), tile(20, 0, 22, 30)]).as_bytes(),
        ImportLimits::default(),
    )
    .expect_err("outside");
    assert_eq!(outside.code(), "terrain_tiles_bounds");
    let future = descriptor_json(cols, rows, &good)
        .replace("\"terrain_source_tiles\":1", "\"terrain_source_tiles\":2");
    let future =
        TileSetDescriptor::from_json(future.as_bytes(), ImportLimits::default()).expect_err("future");
    assert_eq!(future.code(), "terrain_tiles_version");
}

/// ADR-8639 (review finding 2): a `heightfield.raster.json` beside `heightfield.tiles.json` is a
/// second description of the same heightfield (a tile set never applies its row order or
/// spacing), so the cook refuses it with `terrain_tiles_ambiguous`: on a fresh project no index
/// is written, and over a published map nothing changes. Removing the descriptor cooks again.
#[test]
fn a_raster_descriptor_beside_a_tile_set_is_refused() {
    let (cols, rows) = (41, 30);
    let good = [tile(0, 0, 21, 30), tile(20, 0, 21, 30)];
    let north_first =
        r#"{"raster_descriptor":1,"spacing_m":[0.5,0.5],"row0":"north","registration":"center"}"#;

    // A fresh project: refused, and no terrain index exists afterwards.
    let fresh = regia_test_utils::unique_temp_dir("terrain-tiles-and-raster-fresh");
    write_layer(&fresh);
    let dir = fresh.join(DIR);
    write_tile_set(&dir, cols, rows, &good, &count);
    std::fs::write(dir.join("heightfield.raster.json"), north_first).expect("raster");
    let report = cook(&fresh);
    let text = format!("{:?}", report.errors);
    assert!(text.contains("terrain_tiles_ambiguous"), "got {text}");
    assert!(
        text.contains("heightfield.raster.json"),
        "the message names the file: {text}"
    );
    let cache = cooked_cache_dir(&fresh, PLATFORM);
    let index = cooked_file_name(terrain_index_asset_id(map_id(&fresh)));
    assert!(
        !cache.join(&index).exists(),
        "no terrain index is written for a refused tile set"
    );

    // A published map: a descriptor added later is refused and the map stays byte for byte.
    let root = regia_test_utils::unique_temp_dir("terrain-tiles-and-raster-published");
    write_layer(&root);
    let dir = root.join(DIR);
    write_tile_set(&dir, cols, rows, &good, &count);
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let before = cooked_bytes(&root);
    std::fs::write(dir.join("heightfield.raster.json"), north_first).expect("raster");
    let report = cook(&root);
    let text = format!("{:?}", report.errors);
    assert!(text.contains("terrain_tiles_ambiguous"), "got {text}");
    assert_eq!(cooked_bytes(&root), before, "nothing published");

    std::fs::remove_file(dir.join("heightfield.raster.json")).expect("remove raster");
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(cooked_bytes(&root), before, "the same tiles cook to the same map");
}

/// Listing the tiles in another order gives the same cooked ids, bytes and evidence.
#[test]
fn shuffled_descriptor_order_gives_identical_output_ids_and_bytes() {
    let (cols, rows) = (41, 30);
    let mut png_tile = tile(20, 15, 21, 15);
    png_tile.png = true;
    let tiles = [tile(0, 0, 21, 30), tile(20, 0, 21, 16), png_tile];
    let shuffled = [png_tile, tile(0, 0, 21, 30), tile(20, 0, 21, 16)];
    let a = regia_test_utils::unique_temp_dir("terrain-tiles-order-a");
    let b = regia_test_utils::unique_temp_dir("terrain-tiles-order-b");
    write_layer(&a);
    write_tile_set(&a.join(DIR), cols, rows, &tiles, &count);
    let ra = cook(&a);
    write_layer(&b);
    write_tile_set(&b.join(DIR), cols, rows, &shuffled, &count);
    share_meta(&a, &b);
    let rb = cook(&b);
    assert!(
        ra.errors.is_empty() && rb.errors.is_empty(),
        "{:?} {:?}",
        ra.errors,
        rb.errors
    );
    assert_ne!(
        std::fs::read(a.join(DIR).join(TILE_SET_FILE)).expect("a"),
        std::fs::read(b.join(DIR).join(TILE_SET_FILE)).expect("b"),
        "the descriptors really differ"
    );
    assert_eq!(cooked_bytes(&a), cooked_bytes(&b));
    let evidence = |root: &Path| {
        let dir =
            cooked_cache_dir(root, PLATFORM).join(regia_asset_pipeline::terrain_cook::TILE_EVIDENCE_DIR);
        snapshot(&dir).into_values().collect::<Vec<_>>()
    };
    assert_eq!(evidence(&a), evidence(&b));
    assert!(!evidence(&a).is_empty());
}

/// Peak charged source-sample buffers are the same for a 2x2 and a 6x6 grid of 32x32 tiles:
/// they depend on the tile size, never on the world area.
#[test]
fn peak_charged_sample_buffers_depend_on_tile_size_not_world_area() {
    let mut peaks = Vec::new();
    for n in [2u32, 6] {
        // Tiles of 32 samples sharing one-sample borders: stride 31.
        let side = 31 * n + 1;
        let tiles: Vec<Tile> = (0..n)
            .flat_map(|ty| (0..n).map(move |tx| tile(tx * 31, ty * 31, 32, 32)))
            .collect();
        let dir = regia_test_utils::unique_temp_dir("terrain-tiles-peak");
        write_tile_set(&dir, side, side, &tiles, &count);
        let (_, _, report) = cook_tile_set_heightfield(
            AssetId(regia_core::id::Guid::from_u128(0x5eed_0002)),
            &dir.join(TILE_SET_FILE),
            &dir.join("cache"),
            16,
            |_| ControlFlow::Continue(()),
        )
        .expect("cook");
        assert_eq!(report.source_tiles, (n * n) as usize);
        assert!(
            report.peak_charged_samples <= 4 * 32 * 32,
            "at most the four tiles around one cooked tile: {}",
            report.peak_charged_samples
        );
        if n == 6 {
            // A 187x187 world: the peak is under an eighth of its samples.
            assert!((report.peak_charged_samples as u64) * 8 < u64::from(side) * u64::from(side));
        }
        peaks.push(report.peak_charged_samples);
    }
    assert_eq!(peaks[0], peaks[1], "same tile size, 9x the area, same peak");
}

/// A cancelled cook leaves the old `.fckd` and evidence; the recook after one tile changed
/// rebuilds only the cooked tiles that tile covers and equals a fresh full cook.
#[test]
fn cancel_leaves_the_old_map_and_recook_touches_only_changed_tiles() {
    let side = 31 * 3 + 1;
    let tiles: Vec<Tile> = (0..3u32)
        .flat_map(|ty| (0..3u32).map(move |tx| tile(tx * 31, ty * 31, 32, 32)))
        .collect();
    let dir = regia_test_utils::unique_temp_dir("terrain-tiles-recook");
    write_tile_set(&dir, side, side, &tiles, &count);
    let cache = dir.join("cache");
    let id = AssetId(regia_core::id::Guid::from_u128(0x5eed_0003));
    let descriptor = dir.join(TILE_SET_FILE);
    let go = |_: &SourceTileProgress| ControlFlow::Continue(());
    let (_, _, first) = cook_tile_set_heightfield(id, &descriptor, &cache, 16, go).expect("first cook");
    assert_eq!(first.source_tiles_changed, 9);
    assert_eq!(first.cooked_tiles_recooked, first.cooked_tiles);
    let before = snapshot(&cache);

    // Change one interior sample of the corner tile r0_c0 (not on a shared border).
    let edited = |c: u32, r: u32| {
        if (c, r) == (5, 6) {
            count(c, r) ^ 0x10
        } else {
            count(c, r)
        }
    };
    let corner = tiles[0];
    write_tile(
        &dir,
        &corner,
        tile_counts(&corner, &edited),
        mapping(side, side).sample_world(0, 0),
    );

    // Cancel at the first decode: an error, and the cache is untouched.
    let mut decodes = 0;
    let cancelled = cook_tile_set_heightfield(id, &descriptor, &cache, 16, |step| {
        if matches!(step, SourceTileProgress::Decoding { .. }) {
            decodes += 1;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .expect_err("cancelled");
    assert!(
        cancelled.to_string().contains("height_source_cancelled"),
        "{cancelled}"
    );
    assert_eq!(decodes, 1);
    assert_eq!(snapshot(&cache), before, "a cancelled cook publishes nothing");

    // Recook: one source tile changed; only the cooked tiles it covers are rebuilt.
    let (_, _, again) = cook_tile_set_heightfield(id, &descriptor, &cache, 16, go).expect("recook");
    assert_eq!(again.source_tiles_changed, 1);
    // r0_c0 covers samples 0..32 on both axes: cooked tiles 0..=1 on each axis (16-sample tiles).
    assert_eq!(again.cooked_tiles_recooked, 4, "{again:?}");
    assert!(again.cooked_tiles_recooked < again.cooked_tiles);
    assert!(again.source_tiles_decoded < first.source_tiles_decoded);
    let file = cooked_file_name(heightfield_asset_id(id));
    let recooked = std::fs::read(cache.join(&file)).expect("recooked");

    // A fresh cook of the edited set (no evidence) is byte-identical, and the untouched cooked
    // tiles are the previous cook's tiles.
    let fresh = dir.join("fresh-cache");
    cook_tile_set_heightfield(id, &descriptor, &fresh, 16, go).expect("fresh cook");
    assert_eq!(std::fs::read(fresh.join(&file)).expect("fresh"), recooked);
    let old = decode_cooked_tiled_heightfield(&before[&cache.join(&file)]).expect("old");
    let new = decode_cooked_tiled_heightfield(&recooked).expect("new");
    let changed: Vec<(u32, u32)> = old
        .tiles
        .iter()
        .zip(&new.tiles)
        .filter(|(a, b)| a.samples != b.samples)
        .map(|(a, _)| (a.x, a.y))
        .collect();
    assert_eq!(
        changed,
        vec![(0, 0)],
        "only the tile holding the edited sample differs"
    );

    // Nothing changed: the recook decodes nothing and rebuilds no cooked tile.
    let (_, _, idle) = cook_tile_set_heightfield(id, &descriptor, &cache, 16, go).expect("idle recook");
    assert_eq!(
        [idle.source_tiles_changed, idle.cooked_tiles_recooked],
        [0, 0],
        "{idle:?}"
    );
    assert_eq!(idle.source_tiles_decoded, 0);
}
