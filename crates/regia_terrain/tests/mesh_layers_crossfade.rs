// SPDX-License-Identifier: MPL-2.0
//! ADR-8842 (1): a mesh-layer chunk that changes LOD cross-fades instead of popping. The layer
//! stream keeps the old LOD's entity beside the new one for `fade_frames` frames with the
//! renderer's complementary dithers (`MeshLodFade(c)` on the incoming LOD, `MeshLodFade(-c)` on the
//! outgoing one; `regia_render`'s `mesh_lod_fade` test proves such a pair covers every pixel exactly
//! once). These tests run `drive_layer_stream` frame by frame over cooked chunk files and check,
//! every frame and every chunk, that the chunk is drawn by one whole entity or by exactly such a
//! pair: never both LODs whole, never a lone share, never nothing.
//!
//! Fixed steps and counters; no clock, no GPU. The chunk meshes are original single triangles.

// Test fixtures panic on a broken fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{
    AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CpuMesh, cooked_file_name,
    encode_cooked_file, pack_importer_version,
};
use regia_render::MeshLodFade;
use regia_scene::stream_plan::{SourceId, SourceView};
use regia_scene::streaming::StreamViews;
use regia_terrain::{
    CdlodConfig, CookedChunkLod, CookedLayer, CookedLayerChunk, DEFAULT_LAYER_FADE_FRAMES, LayerChunkGrid,
    TerrainLayerChunk, TerrainLayerStream, TerrainMeshLayers, TerrainMeshLayersCooked,
};

const SPAN: f32 = 512.0;
const ROOT: u32 = 3;
const LODS: u8 = 3;

/// One layer over an 8 x 8 grid of 64 m chunks, three LODs each, with every LOD mesh cooked into
/// `dir` (a legacy CPU mesh file per LOD: one triangle in the chunk).
fn cooked(dir: &Path) -> TerrainMeshLayersCooked {
    let config = CdlodConfig::default();
    let grid = LayerChunkGrid::new(
        SPAN,
        ROOT,
        config.grid_size,
        config.range_factor,
        [0.0, 0.0],
        64.0,
    );
    assert_eq!(grid.chunks_per_side(), 8);
    let root = AssetId::sub_asset(AssetId::default(), "adr-8842-crossfade");
    let n = grid.chunks_per_side();
    let chunks = (0..n)
        .flat_map(|z| (0..n).map(move |x| (x, z)))
        .map(|(x, z)| {
            let (mx, mz, size) = grid.chunk_rect(x, z);
            CookedLayerChunk {
                x,
                z,
                bounds: [[mx, 0.0, mz], [mx + size, 1.0, mz + size]],
                lods: (0..LODS)
                    .map(|lod| {
                        let id = AssetId::sub_asset(root, &format!("{x}_{z}/{lod}"));
                        write_mesh(dir, id, [mx, mz], size, lod);
                        CookedChunkLod {
                            mesh: id,
                            triangles: 1,
                            bytes: 100,
                            error_m: 0.0,
                        }
                    })
                    .collect(),
            }
        })
        .collect();
    TerrainMeshLayersCooked {
        version: regia_terrain::mesh_layer::PRESENTATION_VERSION,
        chunk_importer: 0,
        grid,
        layers: vec![CookedLayer {
            id: "rock".into(),
            map_layer: 0,
            cast_shadows: true,
            collider: false,
            material: String::new(),
            material_id: None,
            chunks,
        }],
    }
}

fn write_mesh(dir: &Path, id: AssetId, at: [f32; 2], size: f32, lod: u8) {
    let y = f32::from(lod);
    let mesh = CpuMesh {
        positions: vec![
            [at[0], y, at[1]],
            [at[0] + size, y, at[1]],
            [at[0], y, at[1] + size],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 3],
        uvs: vec![[0.0, 0.0]; 3],
        tangents: Vec::new(),
        indices: vec![0, 2, 1],
        ..Default::default()
    };
    let payload = mesh.to_postcard().expect("postcard");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: AssetTypeTag::mesh_legacy_cpu().0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    std::fs::write(
        dir.join(cooked_file_name(id)),
        encode_cooked_file(&header, &payload).expect("encode"),
    )
    .expect("write chunk mesh");
}

/// An app running only the layer stream, with the cooked layers on one entity and a view at
/// `eye`. Returns the app and the temp dir.
fn stream_app(tag: &str, stream: TerrainLayerStream, eye: [f32; 2]) -> (App, PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(tag);
    let cooked = cooked(&dir);
    let mut app = App::new();
    app.insert_resource(stream);
    app.insert_resource(views(eye));
    app.add_systems(Update, regia_terrain::mesh_layer::stream::drive_layer_stream);
    app.world_mut()
        .spawn(TerrainMeshLayers::new().with_presentation(cooked, Some(dir.clone())));
    (app, dir)
}

fn views(eye: [f32; 2]) -> StreamViews {
    StreamViews(vec![SourceView::at(
        SourceId::Pin(1),
        [f64::from(eye[0]), 0.0, f64::from(eye[1])],
    )])
}

/// One chunk's draw in one frame: `(lod, dither)` per entity, sorted by LOD.
type ChunkFrame = Vec<(u8, f32)>;

/// Every chunk's entities after one update, keyed `(z, x)`.
fn frame(app: &mut App, eye: [f32; 2]) -> BTreeMap<(u32, u32), ChunkFrame> {
    app.insert_resource(views(eye));
    app.update();
    let mut out: BTreeMap<(u32, u32), ChunkFrame> = BTreeMap::new();
    let mut q = app
        .world_mut()
        .query::<(&TerrainLayerChunk, Option<&MeshLodFade>)>();
    for (chunk, fade) in q.iter(app.world()) {
        out.entry((chunk.node.z, chunk.node.x))
            .or_default()
            .push((chunk.lod, fade.map_or(0.0, |f| f.0)));
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    }
    out
}

/// The coverage rule: one whole entity, or two LODs with dithers `c` and `-c`, `c` in `(0, 1)`.
/// Returns the incoming LOD and its share for a fade, `None` for a whole draw.
fn check_coverage(at: &str, key: (u32, u32), draw: &ChunkFrame) -> Option<(u8, f32)> {
    match draw.as_slice() {
        [(_, d)] => {
            assert_eq!(
                *d, 0.0,
                "{at}: chunk {key:?} has one entity with dither {d}: only a share of its pixels is drawn"
            );
            None
        }
        [(la, da), (lb, db)] => {
            assert_ne!(la, lb, "{at}: chunk {key:?} draws LOD {la} twice: {draw:?}");
            assert!(
                *da != 0.0 && *db != 0.0,
                "{at}: chunk {key:?} draws a LOD whole beside another: both LODs show ({draw:?})"
            );
            assert_eq!(
                *da, -*db,
                "{at}: chunk {key:?} dithers {da} and {db} do not cover every pixel exactly once"
            );
            let (lod, c) = if *da > 0.0 { (*la, *da) } else { (*lb, *db) };
            assert!(c > 0.0 && c < 1.0, "{at}: chunk {key:?} share {c} outside (0, 1)");
            Some((lod, c))
        }
        other => panic!("{at}: chunk {key:?} has {} entities: {other:?}", other.len()),
    }
}

/// The eye walks from 900 m west of the map to its center, 2 m a frame: every chunk steps
/// LOD 2 -> 1 -> 0 at the terrain's ranges. Every frame every chunk is covered exactly once, and
/// every LOD switch is a cross-fade of `fade_frames - 1` dithered frames with shares `1/N ..
/// (N-1)/N`, after which the new LOD draws whole.
#[test]
fn a_lod_switch_cross_fades_and_no_frame_shows_both_or_neither() {
    let stream = TerrainLayerStream::new(f32::INFINITY, u64::MAX);
    let n = stream.fade_frames;
    assert_eq!(n, DEFAULT_LAYER_FADE_FRAMES);
    assert!(n >= 2, "the default fades");
    let (mut app, dir) = stream_app("meshlayers-crossfade-walk", stream, [-900.0, 0.0]);
    let chunks = 64usize;
    // Per chunk: the LOD it last drew whole, and the shares of the fade in progress.
    let mut whole: BTreeMap<(u32, u32), u8> = BTreeMap::new();
    let mut shares: BTreeMap<(u32, u32), Vec<f32>> = BTreeMap::new();
    let (mut switches, mut faded, mut fade_frames_seen) = (0u32, 0u32, 0u32);
    for step in 0..=450u32 {
        let eye = [-900.0 + 2.0 * step as f32, 0.0];
        let draws = frame(&mut app, eye);
        let at = format!("frame {step} eye {eye:?}");
        assert_eq!(
            draws.len(),
            chunks,
            "{at}: every chunk is wanted and must be drawn (a chunk with no entity shows neither LOD)"
        );
        for (key, draw) in &draws {
            match check_coverage(&at, *key, draw) {
                Some((_, c)) => {
                    fade_frames_seen += 1;
                    shares.entry(*key).or_default().push(c);
                }
                None => {
                    let lod = draw[0].0;
                    let fade = shares.remove(key).unwrap_or_default();
                    if let Some(before) = whole.insert(*key, lod)
                        && before != lod
                    {
                        switches += 1;
                        let want: Vec<f32> = (1..n).map(|k| k as f32 / n as f32).collect();
                        if !fade.is_empty() {
                            faded += 1;
                        }
                        assert_eq!(
                            fade, want,
                            "{at}: chunk {key:?} LOD {before} -> {lod} dithered with shares {fade:?}, want {want:?} (a switch with no shares pops)"
                        );
                    }
                }
            }
        }
    }
    let stream = app.world().resource::<TerrainLayerStream>();
    println!(
        "ADR-8842 walk: {switches} LOD switches, {faded} cross-faded, {fade_frames_seen} chunk-frames dithered, fading now {}",
        stream.fading()
    );
    assert!(
        switches >= 64,
        "every chunk switched LOD at least once ({switches})"
    );
    assert_eq!(faded, switches);
    assert_eq!(stream.fading(), 0, "every fade ended");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The eye shakes across a chunk's LOD boundary faster than a fade: switches reverse mid-fade.
/// Coverage still holds every frame (never three entities, never a lone share), and once the eye
/// stops every chunk settles to one whole entity.
#[test]
fn a_switch_reversed_mid_fade_keeps_every_pixel_covered_once() {
    let stream = TerrainLayerStream::new(f32::INFINITY, u64::MAX);
    let n = stream.fade_frames;
    let (mut app, dir) = stream_app("meshlayers-crossfade-shake", stream, [-300.0, 0.0]);
    let grid = cooked(&dir).grid;
    // The westmost chunk row's LOD 0 / LOD 1 boundary: the eye at `range(0)` west of chunk x = 0.
    let edge = grid.chunk_rect(0, 3).0 - grid.range(0);
    let mut reversed = false;
    let mut last: BTreeMap<(u32, u32), ChunkFrame> = BTreeMap::new();
    for step in 0..120u32 {
        // Three frames on either side of the boundary.
        let side = if (step / 3).is_multiple_of(2) { -4.0 } else { 4.0 };
        let eye = [edge + side, 0.0];
        let draws = frame(&mut app, eye);
        let at = format!("frame {step} eye {eye:?}");
        assert_eq!(draws.len(), 64, "{at}: a chunk shows neither LOD");
        for (key, draw) in &draws {
            let now = check_coverage(&at, *key, draw);
            // A reversal: the LOD that was coming in is now going out.
            if let (Some((lod, _)), Some(prev)) = (now, last.get(key))
                && prev.len() == 2
                && let Some((was_in, _)) = check_coverage(&at, *key, prev)
                && was_in != lod
            {
                reversed = true;
            }
        }
        last = draws;
    }
    assert!(reversed, "the shake reversed a fade");
    // The eye stops: within one fade every chunk draws one whole LOD.
    let rest = [edge - 40.0, 0.0];
    for _ in 0..n {
        frame(&mut app, rest);
    }
    let draws = frame(&mut app, rest);
    for (key, draw) in &draws {
        assert_eq!(draw.len(), 1, "chunk {key:?} still fading: {draw:?}");
        assert_eq!(draw[0].1, 0.0);
    }
    assert_eq!(app.world().resource::<TerrainLayerStream>().fading(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `fade_frames` 0 keeps the old instant switch: one whole entity per chunk every frame and no
/// dither ever. A chunk that leaves the view radius mid-fade takes both its entities with it.
#[test]
fn no_fade_length_switches_at_once_and_a_leaving_chunk_takes_both_entities() {
    let mut stream = TerrainLayerStream::new(f32::INFINITY, u64::MAX);
    stream.fade_frames = 0;
    let (mut app, dir) = stream_app("meshlayers-crossfade-instant", stream, [-900.0, 0.0]);
    let mut seen = BTreeMap::new();
    for step in 0..=450u32 {
        let eye = [-900.0 + 2.0 * step as f32, 0.0];
        for (key, draw) in frame(&mut app, eye) {
            assert_eq!(draw.len(), 1, "instant: chunk {key:?} {draw:?}");
            assert_eq!(draw[0].1, 0.0, "instant: no dither");
            seen.entry(key).or_insert_with(Vec::new).push(draw[0].0);
        }
    }
    assert!(seen.values().any(|l| l.first() != l.last()), "LODs switched");
    let _ = std::fs::remove_dir_all(&dir);

    // Leaving mid-fade: a finite radius, a switch, then the eye jumps far away.
    let stream = TerrainLayerStream::new(300.0, u64::MAX);
    let (mut app, dir) = stream_app("meshlayers-crossfade-leave", stream, [-280.0, 0.0]);
    let grid = cooked(&dir).grid;
    let edge = grid.chunk_rect(0, 3).0 - grid.range(0);
    frame(&mut app, [edge - 4.0, 0.0]);
    let fading = frame(&mut app, [edge + 4.0, 0.0]);
    assert!(fading.values().any(|d| d.len() == 2), "a fade started");
    let gone = frame(&mut app, [-5000.0, 0.0]);
    assert!(
        gone.is_empty(),
        "chunks out of the radius left entities: {gone:?}"
    );
    assert_eq!(app.world().resource::<TerrainLayerStream>().fading(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}
