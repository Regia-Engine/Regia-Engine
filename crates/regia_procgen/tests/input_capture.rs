// SPDX-License-Identifier: MPL-2.0
//! `in.mask` pixels and `in.ground` layers through the world host, the `threads` setting, and the road
//! profile's edge pieces (spec 63 § 3, spec 64 § 7.1, ADR-2731).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{compile, graph, node};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Vec2, Vec3};
use regia_procgen::data::{DataItem, GroundData, Payload};
use regia_procgen::exec::{MaskImage, StaticSnapshot};
use regia_procgen::road::{RoadPiece, RoadProfile, RoadSide};
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
    ProcMaskLibrary, ProcOutput, run_now,
};
use regia_procgen::{Bundle, CancelToken, Evaluator, ProcGraph, RunInputs};
use regia_scene::components::EntityGuid;
use regia_scene::spline::{Spline, SplineUp};
use regia_scene::{GlobalTransform, Transform};

/// Eight points in a row along X, at x = -3.5 .. 3.5, then what `mask` keeps of them.
fn mask_graph(channel: &str, filter: &str) -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(8.0, 1.0, 1.0)"), ("step_m", "(1.0, 1.0, 1.0)")],
                ),
            ),
            (
                2,
                node(
                    "in.mask",
                    &[
                        ("asset", "\"masks/left_off.png\""),
                        ("rect", "[-4.0, -1.0, 4.0, 1.0]"),
                        ("channel", &format!("\"{channel}\"")),
                        ("filter", &format!("\"{filter}\"")),
                    ],
                ),
            ),
            (3, node("region.intersect", &[])),
            (4, node("out.points", &[("name", "\"kept\"")])),
        ],
        &[(1, "out", 3, "in"), (2, "out", 3, "with"), (3, "out", 4, "in")],
    )
}

fn host(source: ProcGraphSource, settings: ProcGenSettings) -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source,
        ..settings
    });
    app.add_plugins(ProcGenPlugin);
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(40.0, 50.0, 40.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(0xabc)),
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    (app, e)
}

fn kept_x(app: &App, e: Entity) -> Vec<f32> {
    let out = app
        .world()
        .get::<ProcOutput>(e)
        .unwrap_or_else(|| panic!("output"));
    out.0.points["kept"].position.iter().map(|p| p[0]).collect()
}

/// A 2 × 1 PNG: left pixel off, right pixel on, in every channel `bytes` of a kind.
fn png_bytes(color: png::ColorType, depth: png::BitDepth, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, 2, 1);
    encoder.set_color(color);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().unwrap_or_else(|e| panic!("{e}"));
    writer.write_image_data(pixels).unwrap_or_else(|e| panic!("{e}"));
    drop(writer);
    out
}

#[test]
fn a_mask_fixture_filters_points_as_authored_through_the_project_file() {
    let dir = regia_test_utils::unique_temp_dir("procgen_mask");
    std::fs::create_dir_all(dir.join("masks")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("g.pgraph.ron"), mask_graph("r", "nearest").to_ron())
        .unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(
        dir.join("masks/left_off.png"),
        png_bytes(png::ColorType::Grayscale, png::BitDepth::Eight, &[0, 255]),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (mut app, e) = host(ProcGraphSource::Dir(dir.clone()), ProcGenSettings::default());
    assert!(run_now(app.world_mut(), e).is_some());
    assert_eq!(
        kept_x(&app, e),
        vec![0.5, 1.5, 2.5, 3.5],
        "only the right half of the mask is on"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_library_goes_first_and_channels_read_as_authored() {
    let files: BTreeMap<String, String> =
        [("g.pgraph.ron".to_string(), mask_graph("g", "nearest").to_ron())].into();
    let (mut app, e) = host(ProcGraphSource::Memory(files), ProcGenSettings::default());
    app.world_mut().insert_resource(ProcMaskLibrary(
        [(
            "masks/left_off.png".to_string(),
            MaskImage {
                width: 2,
                height: 1,
                // Red says left, green says right: the node reads green.
                rgba: vec![[1.0, 1.0, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0]],
            },
        )]
        .into(),
    ));
    assert!(run_now(app.world_mut(), e).is_some());
    assert_eq!(kept_x(&app, e), vec![-3.5, -2.5, -1.5, -0.5]);
}

#[test]
fn a_missing_or_broken_mask_warns_and_is_empty() {
    let dir = regia_test_utils::unique_temp_dir("procgen_mask_broken");
    std::fs::create_dir_all(dir.join("masks")).unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(dir.join("g.pgraph.ron"), mask_graph("r", "nearest").to_ron())
        .unwrap_or_else(|e| panic!("{e}"));
    let warned = |dir: &std::path::Path| {
        let (mut app, e) = host(
            ProcGraphSource::Dir(dir.to_path_buf()),
            ProcGenSettings::default(),
        );
        assert!(run_now(app.world_mut(), e).is_some());
        let st = app
            .world()
            .get::<ProcGenState>(e)
            .unwrap_or_else(|| panic!("state"));
        let report = st.report.as_ref().unwrap_or_else(|| panic!("report"));
        report.warning_codes().iter().any(|c| c == "mask_missing")
    };
    assert!(warned(&dir), "no file");
    std::fs::write(dir.join("masks/left_off.png"), b"not a png").unwrap_or_else(|e| panic!("{e}"));
    assert!(warned(&dir), "not a png");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_png_kind_decodes_to_the_stored_numbers() {
    use png::{BitDepth as D, ColorType as C};
    let two = |bytes: Vec<u8>| MaskImage::from_png(&bytes).unwrap_or_else(|e| panic!("{e}")).rgba;
    assert_eq!(
        two(png_bytes(C::Grayscale, D::Eight, &[0, 255]))[1],
        [1.0, 1.0, 1.0, 1.0]
    );
    assert_eq!(
        two(png_bytes(C::GrayscaleAlpha, D::Eight, &[0, 255, 255, 0]))[1],
        [1.0, 1.0, 1.0, 0.0]
    );
    assert_eq!(
        two(png_bytes(C::Rgb, D::Eight, &[0, 0, 0, 255, 0, 255]))[1],
        [1.0, 0.0, 1.0, 1.0]
    );
    let rgba = two(png_bytes(C::Rgba, D::Eight, &[0, 0, 0, 0, 51, 102, 153, 204]));
    assert_eq!(rgba[1], [0.2, 0.4, 0.6, 0.8], "no sRGB decoding");
    assert_eq!(
        two(png_bytes(C::Grayscale, D::Sixteen, &[0, 0, 255, 255]))[1],
        [1.0, 1.0, 1.0, 1.0]
    );
    assert!(MaskImage::from_png(b"nope").is_err());
    // 16-bit samples keep their precision: 1000 / 65535, not the 3 / 255 an 8-bit strip leaves.
    let [hi, lo] = 1000u16.to_be_bytes();
    let v = two(png_bytes(C::Grayscale, D::Sixteen, &[0, 0, hi, lo]))[1][0];
    assert_eq!(v, 1000.0 / 65535.0);
    let rgba = two(png_bytes(
        C::Rgba,
        D::Sixteen,
        &[0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x00, 0, 1, 0xff, 0xff, 0x40, 0x00],
    ));
    assert_eq!(
        rgba[1],
        [32768.0 / 65535.0, 1.0 / 65535.0, 1.0, 16384.0 / 65535.0]
    );
}

/// A tiny file that declares a huge image is refused before its pixels are allocated.
#[test]
fn a_mask_larger_than_the_cap_is_refused() {
    let header = |w: u32, h: u32| {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap_or_else(|e| panic!("{e}"));
        // A crafted file: the header, then one IDAT chunk holding the start of a zlib stream only.
        writer
            .write_chunk(png::chunk::IDAT, &[0x78, 0x9c, 0x01])
            .unwrap_or_else(|e| panic!("{e}"));
        drop(writer);
        out
    };
    let e = MaskImage::from_png(&header(30_000, 30_000)).expect_err("too large");
    assert!(e.contains("more than"), "{e}");
    let e = MaskImage::from_png(&header(4097, 4096)).expect_err("one column over");
    assert!(e.contains("more than"), "{e}");
    // At the cap the size is accepted; the missing data is what fails.
    let e = MaskImage::from_png(&header(4096, 4096)).expect_err("no data");
    assert!(!e.contains("more than"), "{e}");
}

/// A two-layer terrain: `rock` on slopes, `grass` on the flat. Flat at x < 0, a 45 degree ramp beyond.
fn layered_terrain() -> regia_terrain::Terrain {
    terrain_with_layer_names("grass", "rock")
}

/// [`layered_terrain`] with its flat and slope layers named `flat` and `slope`.
fn terrain_with_layer_names(flat: &str, slope: &str) -> regia_terrain::Terrain {
    use regia_terrain::{LayerDesc, TerrainLayers};
    let n = 33u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let c = (i % n) as f32 - 16.0;
            if c <= 0.0 { 0.0 } else { c * 2.0 }
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 64.0, 100.0, -50.0)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let layers = TerrainLayers {
        layers: vec![
            LayerDesc {
                name: flat.to_string(),
                slope_min: 0.0,
                slope_max: 0.3,
                ..LayerDesc::ground()
            },
            LayerDesc {
                name: slope.to_string(),
                slope_min: 0.5,
                slope_max: 1.6,
                ..LayerDesc::ground()
            },
        ],
        ..TerrainLayers::default()
    };
    regia_terrain::Terrain::new(
        &map.to_heightfield(),
        64.0,
        100.0,
        -50.0,
        regia_terrain::CdlodConfig::default(),
        3,
        layers,
    )
    .unwrap_or_else(|e| panic!("{e:?}"))
}

fn layer_graph(in_ground_layers: &str, layer: &str) -> ProcGraph {
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(8.0, 1.0, 1.0)"), ("step_m", "(4.0, 1.0, 1.0)")],
                ),
            ),
            (2, node("in.ground", &[("layers", in_ground_layers)])),
            (
                3,
                node(
                    "density.ground_layer",
                    &[("layer", &format!("\"{layer}\"")), ("mode", "\"set\"")],
                ),
            ),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"density\""), ("min", "0.5"), ("max", "1.0")],
                ),
            ),
            (5, node("out.points", &[("name", "\"kept\"")])),
        ],
        &[
            (1, "out", 3, "in"),
            (2, "out", 3, "ground"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
        ],
    )
}

fn layer_run(g: &ProcGraph) -> (App, Entity) {
    let files: BTreeMap<String, String> = [("g.pgraph.ron".to_string(), g.to_ron())].into();
    let (mut app, e) = host(ProcGraphSource::Memory(files), ProcGenSettings::default());
    app.world_mut().spawn(layered_terrain());
    (app, e)
}

#[test]
fn layer_filters_keep_the_points_on_their_layer() {
    let (mut app, e) = layer_run(&layer_graph("[\"grass\", \"rock\"]", "rock"));
    assert!(
        run_now(app.world_mut(), e).is_some(),
        "{:?}",
        app.world().get::<ProcGenState>(e)
    );
    let rock = kept_x(&app, e);
    assert!(
        !rock.is_empty() && rock.iter().all(|x| *x > 0.0),
        "rock is on the ramp: {rock:?}"
    );
    let (mut app, e) = layer_run(&layer_graph("[\"grass\"]", "grass"));
    assert!(run_now(app.world_mut(), e).is_some());
    let grass = kept_x(&app, e);
    assert!(
        !grass.is_empty() && grass.iter().all(|x| *x < 0.0),
        "grass is on the flat: {grass:?}"
    );
}

/// `density.ground_layer` names `density` as the last written attribute, like every other density
/// node, so a following `$prev` selector reads the layer weight and not an attribute written
/// before it (corr-procgen: `$prev` read `foo` here and kept the flat points too).
#[test]
fn ground_layer_density_is_what_prev_selects() {
    let g = graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[("count", "(8.0, 1.0, 1.0)"), ("step_m", "(4.0, 1.0, 1.0)")],
                ),
            ),
            (6, node("attr.set", &[("target", "\"foo\""), ("value", "0.75")])),
            (2, node("in.ground", &[("layers", "[\"rock\"]")])),
            (
                3,
                node(
                    "density.ground_layer",
                    &[("layer", "\"rock\""), ("mode", "\"set\"")],
                ),
            ),
            (
                4,
                node(
                    "filter.range",
                    &[("attr", "\"$prev\""), ("min", "0.5"), ("max", "1.0")],
                ),
            ),
            (5, node("out.points", &[("name", "\"kept\"")])),
        ],
        &[
            (1, "out", 6, "in"),
            (6, "out", 3, "in"),
            (2, "out", 3, "ground"),
            (3, "out", 4, "in"),
            (4, "inside", 5, "in"),
        ],
    );
    let (mut app, e) = layer_run(&g);
    assert!(
        run_now(app.world_mut(), e).is_some(),
        "{:?}",
        app.world().get::<ProcGenState>(e)
    );
    let kept = kept_x(&app, e);
    assert!(
        !kept.is_empty() && kept.iter().all(|x| *x > 0.0),
        "`$prev` must read the rock weight, so only ramp points stay: {kept:?}"
    );
}

/// With two terrains, `in.ground` layer weights come from the terrain its heights come from (the
/// lowest guid), not from whichever terrain a query meets first (corr-procgen: the layers were read
/// from the first-spawned terrain, which has no `rock` layer here).
#[test]
fn ground_layers_come_from_the_terrain_the_heights_come_from() {
    let g = layer_graph("[\"rock\"]", "rock");
    let files: BTreeMap<String, String> = [("g.pgraph.ron".to_string(), g.to_ron())].into();
    let (mut app, e) = host(ProcGraphSource::Memory(files), ProcGenSettings::default());
    // Spawned first, so a query meets it first; its guid is the higher one.
    app.world_mut()
        .spawn((terrain_with_layer_names("meadow", "scree"), EntityGuid(Guid(2))));
    app.world_mut().spawn((layered_terrain(), EntityGuid(Guid(1))));
    assert!(
        run_now(app.world_mut(), e).is_some(),
        "{:?}",
        app.world().get::<ProcGenState>(e)
    );
    let rock = kept_x(&app, e);
    assert!(
        !rock.is_empty() && rock.iter().all(|x| *x > 0.0),
        "rock is on the ramp of the lowest-guid terrain: {rock:?}"
    );
}

#[test]
fn unknown_and_unnamed_layers_answer_their_codes() {
    // `in.ground` warns about a layer the terrain does not have.
    let g = graph(vec![(1, node("in.ground", &[("layers", "[\"snow\"]")]))], &[]);
    let (mut app, e) = layer_run(&g);
    assert!(run_now(app.world_mut(), e).is_some());
    let st = app
        .world()
        .get::<ProcGenState>(e)
        .unwrap_or_else(|| panic!("state"));
    let report = st.report.as_ref().unwrap_or_else(|| panic!("report"));
    assert!(report.warning_codes().iter().any(|c| c == "layer_missing"));
    // A layer the ground item does not carry fails the reader.
    let (mut app, e) = layer_run(&layer_graph("[\"grass\"]", "rock"));
    let _ = run_now(app.world_mut(), e);
    let st = app
        .world()
        .get::<ProcGenState>(e)
        .unwrap_or_else(|| panic!("state"));
    assert_eq!(
        st.last_error.as_ref().map(|x| x.1.as_str()),
        Some("layer_missing")
    );
}

#[test]
fn a_capture_without_layers_keeps_its_hash() {
    let map = regia_terrain::TerrainMap::from_heights(9, 9, vec![1.0; 81], 16.0, 100.0, -50.0)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let plain = GroundData::capture(&map, [-8.0, -8.0, 8.0, 8.0], 2);
    let hash = |g: &GroundData| Bundle::one(DataItem::new(Payload::Ground(Arc::new(g.clone())))).hash();
    let reference = hash(&plain);
    let mut none = plain.clone();
    none.capture_layers(Vec::new(), |_, _, _, _| Some(1.0));
    assert_eq!(hash(&none), reference);
    let mut with = plain.clone();
    with.capture_layers(["a".to_string(), "b".to_string()], |n, _, _, _| {
        (n == "a").then_some(0.5)
    });
    assert_eq!(with.layers.len(), 1, "a layer the terrain lacks is left out");
    assert_eq!(with.layer_weight("a", 0.0, 0.0), Some(0.5));
    assert_eq!(with.layer_weight("b", 0.0, 0.0), None);
    assert_ne!(hash(&with), reference);
}

#[test]
fn the_threads_setting_picks_the_schedule_and_keeps_the_output() {
    let files: BTreeMap<String, String> = [(
        "g.pgraph.ron".to_string(),
        layer_graph("[\"rock\"]", "rock").to_ron(),
    )]
    .into();
    let mut hashes = Vec::new();
    for threads in [None, Some(0), Some(1), Some(2), Some(8)] {
        let (mut app, e) = host(
            ProcGraphSource::Memory(files.clone()),
            ProcGenSettings {
                threads,
                ..ProcGenSettings::default()
            },
        );
        app.world_mut().spawn(layered_terrain());
        assert!(run_now(app.world_mut(), e).is_some());
        let st = app
            .world()
            .get::<ProcGenState>(e)
            .unwrap_or_else(|| panic!("state"));
        hashes.push(st.output_hash.unwrap_or_else(|| panic!("hash")));
        if let Some(n) = threads {
            let report = st.report.as_ref().unwrap_or_else(|| panic!("report"));
            assert_eq!(report.threads, n.max(1), "the run used the asked schedule");
        }
    }
    assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:?}");
}

/// A closed 1 m × 0.2 m × 0.2 m box along X, its base on y = 0.
fn curb() -> regia_procgen::data::MeshData {
    let mut positions = Vec::new();
    for i in 0..8 {
        positions.push([
            if i & 1 == 0 { 0.0 } else { 1.0 },
            if i & 2 == 0 { 0.0 } else { 0.2 },
            if i & 4 == 0 { -0.1 } else { 0.1 },
        ]);
    }
    regia_procgen::data::MeshData {
        asset: "road/curb.glb".to_string(),
        positions,
        normals: Vec::new(),
        #[rustfmt::skip]
        indices: vec![
            0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6,
            0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7,
            0, 4, 2, 2, 4, 6, 1, 3, 5, 3, 7, 5,
        ],
    }
}

fn road_pieces(profile: RoadProfile, with_mesh: bool) -> regia_procgen::RunResult {
    let mut road = Spline::through([Vec3::new(0.0, 0.0, 0.0), Vec3::new(40.0, 0.0, 0.0)]);
    road.up = SplineUp::WorldY;
    road_pieces_on(profile, with_mesh, road)
}

fn road_pieces_on(profile: RoadProfile, with_mesh: bool, road: Spline) -> regia_procgen::RunResult {
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node(
                    "road.pieces",
                    &[("profile", "\"roads/p.road.ron\""), ("segments_per_m", "0.0")],
                ),
            ),
            (3, node("graph.output", &[("pin", "\"out\"")])),
        ],
        &[(1, "out", 2, "splines"), (2, "out", 3, "in")],
    );
    let mut snap = StaticSnapshot::default();
    snap.road_profiles.insert("roads/p.road.ron".to_string(), profile);
    if with_mesh {
        snap.meshes.insert("road/curb.glb".to_string(), Arc::new(curb()));
    }
    let frame = regia_procgen::exec::Frame {
        graph_inputs: [(
            "road".to_string(),
            Bundle::one(regia_procgen::nodes::spline_data_item(road)),
        )]
        .into(),
        ..regia_procgen::exec::Frame::default()
    };
    Evaluator::new()
        .run_frame(
            &compile(&g),
            &RunInputs::new(Arc::new(snap), 4),
            &CancelToken::new(),
            &frame,
        )
        .unwrap_or_else(|e| panic!("{e}"))
}

fn curb_profile() -> RoadProfile {
    RoadProfile {
        pieces: vec![
            RoadPiece {
                mesh: "road/curb.glb".to_string(),
                side: RoadSide::Left,
                offset_m: 3.5,
            },
            RoadPiece {
                mesh: "road/curb.glb".to_string(),
                side: RoadSide::Right,
                offset_m: 3.5,
            },
        ],
        ..RoadProfile::default()
    }
}

#[test]
fn edge_pieces_line_both_sides_of_the_road_on_its_surface() {
    let r = road_pieces(curb_profile(), true);
    let meshes = &r.output.meshes;
    assert_eq!(meshes.len(), 1, "one material");
    let m = &meshes[0];
    // 40 m / 1 m long curb: 40 copies a side, 8 vertices and 12 triangles each.
    assert_eq!(m.positions.len(), 2 * 40 * 8);
    assert_eq!(m.indices.len(), 2 * 40 * 36);
    let (mut left, mut right) = (0, 0);
    let profile = curb_profile();
    // The center stands high enough that the outer shoulder clears the lift; the crown falls from it.
    let slope = profile.cross_slope_pct * 0.01;
    let center = profile.surface_lift_m + (profile.width_m * 0.5 + profile.shoulders_m.0) * slope;
    let base = center - 3.5 * slope;
    for p in &m.positions {
        assert!((-1e-3..=40.001).contains(&p[0]), "along the road: {p:?}");
        let side = p[2].abs();
        assert!((3.399..=3.601).contains(&side), "at the offset: {p:?}");
        if p[2] < 0.0 {
            left += 1
        } else {
            right += 1
        }
        assert!(
            (p[1] - base).abs() <= 1e-3 || (p[1] - base - 0.2).abs() <= 1e-3,
            "on the surface at {base}: {p:?}"
        );
    }
    assert_eq!(left, right, "one line a side");
    // The Left side is the frame's -right: the same side `road.ribbon`'s negative offsets take.
    let again = road_pieces(curb_profile(), true);
    assert_eq!(r.output_hash, again.output_hash, "repeatable");
}

/// A spline's width scale (`scale.x`) moves a piece sideways by `offset_m * scale.x`, so its
/// base must follow the crowned surface at that scaled offset — sampling at the unscaled
/// offset floats the piece off the ribbon's edge column.
#[test]
fn edge_pieces_follow_the_surface_at_their_scaled_offset() {
    let mut road = Spline::through([Vec3::new(0.0, 0.0, 0.0), Vec3::new(40.0, 0.0, 0.0)]);
    road.up = SplineUp::WorldY;
    for p in &mut road.points {
        p.scale = Vec2::new(2.0, 1.0);
    }
    let r = road_pieces_on(curb_profile(), true, road);
    let m = &r.output.meshes[0];
    let profile = curb_profile();
    let slope = profile.cross_slope_pct * 0.01;
    let center = profile.surface_lift_m + (profile.width_m * 0.5 + profile.shoulders_m.0) * slope;
    // offset_m 3.5 with width scale 2: the piece center line is 7 m out.
    let base = center - 7.0 * slope;
    for p in &m.positions {
        let side = p[2].abs();
        assert!((6.799..=7.201).contains(&side), "at the scaled offset: {p:?}");
        assert!(
            (p[1] - base).abs() <= 1e-3 || (p[1] - base - 0.2).abs() <= 1e-3,
            "on the surface at {base}: {p:?}"
        );
    }
}

#[test]
fn a_road_without_pieces_or_meshes_builds_nothing() {
    let none = road_pieces(RoadProfile::default(), true);
    assert!(none.output.meshes.is_empty());
    let missing = road_pieces(curb_profile(), false);
    assert!(missing.output.meshes.is_empty());
    assert!(missing.report.warning_codes().iter().any(|c| c == "mesh_missing"));
}

/// On a hill, a curb's underside follows the surface `road.ribbon` builds, at the ribbon's own column
/// at that offset (the carriageway edge) and on the side the ribbon calls the same.
#[test]
fn edge_pieces_sit_on_the_ribbon_over_a_hill() {
    let n = 129u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let (c, r) = ((i % n) as f32 - 64.0, (i / n) as f32 - 64.0);
            12.0 - 0.0015 * (c * c + r * r) + 0.02 * c
        })
        .collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node("in.ground", &[("rect", "[-120.0, -120.0, 120.0, 120.0]")]),
            ),
            (3, node("road.ribbon", &[("profile", "\"roads/p.road.ron\"")])),
            (
                4,
                node(
                    "road.pieces",
                    &[
                        ("profile", "\"roads/p.road.ron\""),
                        ("segments_per_m", "0.0"),
                        ("material", "\"curb\""),
                    ],
                ),
            ),
        ],
        &[
            (1, "out", 3, "splines"),
            (2, "out", 3, "ground"),
            (1, "out", 4, "splines"),
            (2, "out", 4, "ground"),
        ],
    );
    let mut snap = StaticSnapshot {
        ground: Some(Arc::new(GroundData::capture(
            &map,
            [-120.0, -120.0, 120.0, 120.0],
            2,
        ))),
        ..StaticSnapshot::default()
    };
    snap.road_profiles.insert(
        "roads/p.road.ron".to_string(),
        RoadProfile {
            material: "asphalt".to_string(),
            ..curb_profile()
        },
    );
    snap.meshes.insert("road/curb.glb".to_string(), Arc::new(curb()));
    let mut road = Spline::through([Vec3::new(-80.0, 0.0, 0.0), Vec3::new(80.0, 0.0, 0.0)]);
    road.up = SplineUp::WorldY;
    let frame = regia_procgen::exec::Frame {
        graph_inputs: [(
            "road".to_string(),
            Bundle::one(regia_procgen::nodes::spline_data_item(road)),
        )]
        .into(),
        ..regia_procgen::exec::Frame::default()
    };
    let r = Evaluator::new()
        .run_frame(
            &compile(&g),
            &RunInputs::new(Arc::new(snap), 4),
            &CancelToken::new(),
            &frame,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let mesh = |m: &str| {
        r.output
            .meshes
            .iter()
            .find(|x| x.material == m)
            .unwrap_or_else(|| panic!("no {m} mesh"))
    };
    let ribbon = mesh("asphalt");
    let curbs = mesh("curb");
    let mut checked = 0;
    for side in [-1.0f32, 1.0] {
        let edge: Vec<[f32; 3]> = ribbon
            .positions
            .iter()
            .copied()
            .filter(|p| (p[2] - side * 3.5).abs() < 1e-3)
            .collect();
        assert!(edge.len() > 20, "ribbon rows at the edge: {}", edge.len());
        for p in curbs.positions.iter().filter(|p| p[2] * side > 0.0) {
            let nearest = edge
                .iter()
                .min_by(|a, b| (a[0] - p[0]).abs().total_cmp(&(b[0] - p[0]).abs()))
                .unwrap_or_else(|| panic!("edge"));
            // A curb's top is 0.2 m over its base; the hill rises under 0.2 m between two rows.
            let rise = p[1] - nearest[1];
            assert!(
                (-0.15..=0.35).contains(&rise),
                "{p:?} against the ribbon's {nearest:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked}");
}

/// A 90° roll must not tip a piece off the ribbon: the offset stays on flat right and the base
/// stays on the surface height (spec 64 addendum, ADR-2731).
#[test]
fn a_rolled_spline_keeps_a_piece_on_the_ribbon_offset() {
    let profile = RoadProfile {
        surface_lift_m: 0.05,
        cross_slope_pct: 0.0,
        pieces: vec![RoadPiece {
            mesh: "road/flat.glb".to_string(),
            side: RoadSide::Right,
            offset_m: 2.0,
        }],
        ..RoadProfile::default()
    };
    let g = graph(
        vec![
            (1, node("graph.input", &[("pin", "\"road\"")])),
            (
                2,
                node(
                    "road.pieces",
                    &[("profile", "\"roads/p.road.ron\""), ("segments_per_m", "0.0")],
                ),
            ),
        ],
        &[(1, "out", 2, "splines")],
    );
    let mut snap = StaticSnapshot::default();
    snap.road_profiles.insert("roads/p.road.ron".to_string(), profile);
    snap.meshes.insert(
        "road/flat.glb".to_string(),
        Arc::new(regia_procgen::data::MeshData {
            asset: "road/flat.glb".to_string(),
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
            normals: Vec::new(),
            indices: Vec::new(),
        }),
    );
    let mut road = Spline::through([Vec3::new(0.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 0.0)]);
    road.up = SplineUp::WorldY;
    for p in &mut road.points {
        p.roll_deg = 90.0;
    }
    let frame = regia_procgen::exec::Frame {
        graph_inputs: [(
            "road".to_string(),
            Bundle::one(regia_procgen::nodes::spline_data_item(road)),
        )]
        .into(),
        ..regia_procgen::exec::Frame::default()
    };
    let r = Evaluator::new()
        .run_frame(
            &compile(&g),
            &RunInputs::new(Arc::new(snap), 4),
            &CancelToken::new(),
            &frame,
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let mesh = r.output.meshes.first().unwrap_or_else(|| panic!("no piece"));
    assert!(!mesh.positions.is_empty(), "no vertices");
    for p in &mesh.positions {
        assert!(
            (p[1] - 0.05).abs() < 1e-3,
            "base left the surface height 0.05: {p:?}"
        );
        assert!(
            (p[2] - 2.0).abs() < 1e-3,
            "offset left the ribbon's flat right: {p:?}"
        );
    }
}

/// Snapshots cloned from one template share the content-hash memo. Each clone given a new ground
/// (or new self items) hashes as a fresh snapshot holding the same data does, even when the
/// allocator hands the new buffers the freed addresses of the previous clone's (corr-procgen: the
/// memo keyed on addresses and lengths and returned the previous clone's hash).
#[test]
fn cloned_snapshots_with_new_data_never_reuse_a_stale_content_hash() {
    use regia_procgen::{PointSet, WorldSnapshot};
    let ground = |h: f32| {
        Arc::new(GroundData {
            rect: [0.0, 0.0, 4.0, 4.0],
            map_size: [4, 4],
            world_width: 4.0,
            origin: [0, 0],
            size: [4, 4],
            heights: vec![h; 16],
            layers: Vec::new(),
        })
    };
    let template = StaticSnapshot::default();
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..16u8 {
        let mut s = template.clone();
        let g = ground(f32::from(i));
        s.ground = Some(Arc::clone(&g));
        let fresh = StaticSnapshot {
            ground: Some(g),
            ..StaticSnapshot::default()
        };
        let hash = s.content_hash();
        assert_eq!(hash, fresh.content_hash(), "ground {i}: a stale memo answered");
        assert!(seen.insert(hash), "ground {i} hashed like an earlier one");
    }
    for i in 0..16u8 {
        let mut s = template.clone();
        let mut points = PointSet::new();
        points.push(regia_procgen::Point {
            position: [f32::from(i), 0.0, 0.0],
            ..regia_procgen::Point::default()
        });
        s.self_items = vec![DataItem::new(Payload::Points(points))];
        let fresh = StaticSnapshot {
            self_items: s.self_items.clone(),
            ..StaticSnapshot::default()
        };
        assert_eq!(
            s.content_hash(),
            fresh.content_hash(),
            "self items {i}: a stale memo answered"
        );
    }
}
