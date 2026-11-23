// SPDX-License-Identifier: MPL-2.0
//! Lane W-15 item 3 (ADR-8569, spec 26 § 2a.4 steps 5-6): the terrain draw draws each open
//! mouth's band (zipper strip, skirt, throat) in every pass it draws, placed on that pass's own
//! selection and morph eye, with the terrain's own pipelines.
//!
//! The scene: hilly ground, a named 12 × 20 m mouth cut on the lattice, an authored tube under it
//! (a loop 0.8 m inside the ring and 1 m down, a throat shelf, walls to a floor at -10 m) and an
//! unlit magenta plane at -60 m. Any magenta pixel is a view through the ground into nothing.
//! Lavapipe, no goldens, no timing.
//!
//! ADR-8824 adds the passes ADR-8569 left out: texture views (probe captures, mirrors, screens,
//! stereo eyes) and the precipitation occlusion tile, and a per-pass band draw count
//! ([`regia_terrain::TerrainDrawStats::band_draws`]) for every pass that draws terrain.

// Test fixtures panic on a broken fixture.
#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    AtmosphereParameters, AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, LabVertex, OutdoorParams, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SharedPassDraw, SurfaceWeatherFrame, TextureViewSettings,
};
use regia_terrain::{
    BandDrawCounts, CdlodConfig, HeightMips, HoleShape, HolesMask, LatticeHoleCut, LayerDesc, LayerMouth,
    LayerTextures, MaterialParams, MouthBands, NamedHole, Rgba8Image, TerrainDrawPasses, TerrainDrawStats,
    TerrainLayers, TerrainMap, TerrainMaterial, TerrainMeshLayers, TerrainSharedDraw, ZipperInput,
    ZipperParams, zipper,
};

const W: u32 = 320;
const H: u32 = 180;
const FOV_Y: f32 = 1.0;
const NEAR: f32 = 0.3;
const FAR: f32 = 1500.0;
const N: u32 = 257;
const WORLD: f32 = 256.0;
const ROOT: u32 = 3;
/// The mouth: x in [-6, 6], z in [-10, 10].
const HALF: [f32; 2] = [6.0, 10.0];
/// How far the authored loop sits inside the ring, metres (under `max_stitch_gap_m`).
const INSET: f32 = 0.8;
const FLOOR_Y: f32 = -10.0;
const VOID_Y: f32 = -60.0;
/// The throat shelf: x in [-4, -0.5], z in [-4, 4], 0.6 m under the ground at its centre.
const SHELF: [f32; 4] = [-4.0, -0.5, -4.0, 4.0];
/// The sun's travel: from -x, high.
const SUN: [f32; 3] = [0.45, -1.0, 0.0];

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

fn height(x: f32, z: f32) -> f32 {
    2.0 * (x * 0.05).sin() * (z * 0.07).cos() + 1.0 * ((x + z) * 0.04).sin()
}

/// Hilly ground, 1 m pitch, sample 0 at -128 m.
fn map() -> TerrainMap {
    let mut heights = Vec::with_capacity((N * N) as usize);
    for row in 0..N {
        for col in 0..N {
            let x = col as f32 - WORLD * 0.5;
            let z = row as f32 - WORLD * 0.5;
            heights.push(height(x, z));
        }
    }
    TerrainMap::from_heights(N, N, heights, WORLD, 1.0, 0.0).expect("map")
}

fn shape() -> HoleShape {
    HoleShape::Rect {
        center: [0.0, 0.0],
        half_extents: HALF,
        rotation_radians: 0.0,
    }
}

fn mask() -> HolesMask {
    let mut mask = HolesMask::default();
    mask.set_named(NamedHole {
        id: "mouth".into(),
        open: true,
        shape: shape(),
    })
    .expect("named hole");
    mask
}

/// Lattice triangle `(col, row, upper)`'s corners as sample coordinates (the engine's split).
fn tri_samples(col: u32, row: u32, upper: bool) -> [(u32, u32); 3] {
    if upper {
        [(col + 1, row), (col, row + 1), (col + 1, row + 1)]
    } else {
        [(col, row), (col, row + 1), (col + 1, row)]
    }
}

fn centroid_cut(map: &TerrainMap, col: u32, row: u32, upper: bool) -> bool {
    let pts = tri_samples(col, row, upper).map(|(c, r)| map.sample_to_world(c, r));
    let x = (pts[0][0] + pts[1][0] + pts[2][0]) / 3.0;
    let z = (pts[0][1] + pts[1][1] + pts[2][1]) / 3.0;
    shape().contains(x, z)
}

fn lattice(map: &TerrainMap) -> LatticeHoleCut {
    let mut cut = LatticeHoleCut::new(map.sample_to_world(0, 0), 1.0, [N - 1, N - 1]);
    let cell = |w: f32| (w + WORLD * 0.5) as u32;
    let (lo, hi) = (
        [cell(-HALF[0] - 2.0), cell(-HALF[1] - 2.0)],
        [cell(HALF[0] + 2.0), cell(HALF[1] + 2.0)],
    );
    cut.set_hole("mouth", lo, hi, |i, j, t| centroid_cut(map, i, j, t == 1));
    cut
}

/// The cut's boundary ring (lattice vertices, cook heights), from the cut triangles' directed edges.
fn ring(map: &TerrainMap) -> Vec<[f32; 3]> {
    let mut cut = BTreeSet::new();
    for row in 100..156 {
        for col in 100..156 {
            for upper in [false, true] {
                if centroid_cut(map, col, row, upper) {
                    cut.insert((col, row, upper));
                }
            }
        }
    }
    let mut edges = BTreeSet::new();
    for &(c, r, u) in &cut {
        let v = tri_samples(c, r, u);
        for e in 0..3 {
            edges.insert((v[e], v[(e + 1) % 3]));
        }
    }
    let mut next = BTreeMap::new();
    for &(a, b) in &edges {
        if !edges.contains(&(b, a)) {
            next.insert(a, b);
        }
    }
    let start = *next.keys().next().expect("a ring");
    let mut out = Vec::new();
    let mut at = start;
    while let Some(to) = next.remove(&at) {
        let [x, z] = map.sample_to_world(at.0, at.1);
        out.push([x, map.height_at_sample(i64::from(at.0), i64::from(at.1)), z]);
        at = to;
        if at == start {
            break;
        }
    }
    assert!(next.is_empty(), "one ring");
    out
}

fn point_in_ring(ring: &[[f32; 3]], x: f32, z: f32) -> bool {
    let mut inside = false;
    let n = ring.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, zi) = (ring[i][0], ring[i][2]);
        let (xj, zj) = (ring[j][0], ring[j][2]);
        if (zi > z) != (zj > z) && x < (xj - xi) * (z - zi) / (zj - zi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The authored tube: loops 0.8 m inside the ring at 1 m and 1.6 m under the ground and at the
/// floor, the floor fan, and the shelf. Returns positions and indices.
fn authored(map: &TerrainMap, ring: &[[f32; 3]]) -> (Vec<[f32; 3]>, Vec<u32>) {
    let inset: Vec<[f32; 2]> = ring
        .iter()
        .map(|p| {
            let (dx, dz) = (-p[0], -p[2]);
            let len = (dx * dx + dz * dz).sqrt().max(1.0e-6);
            [p[0] + dx / len * INSET, p[2] + dz / len * INSET]
        })
        .collect();
    let n = inset.len() as u32;
    let mut positions = Vec::new();
    for depth in [Some(1.0f32), Some(1.6), None] {
        for q in &inset {
            let y = depth.map_or(FLOOR_Y, |d| map.sample_bilinear(q[0], q[1]) - d);
            positions.push([q[0], y, q[1]]);
        }
    }
    let mut indices = Vec::new();
    for band in 0..2u32 {
        for i in 0..n {
            let (a, b) = (band * n + i, band * n + (i + 1) % n);
            let (c, d) = (a + n, b + n);
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }
    let centre = positions.len() as u32;
    positions.push([0.0, FLOOR_Y, 0.0]);
    for i in 0..n {
        indices.extend_from_slice(&[2 * n + i, 2 * n + (i + 1) % n, centre]);
    }
    let y = map.sample_bilinear((SHELF[0] + SHELF[1]) * 0.5, 0.0) - 0.6;
    let s = positions.len() as u32;
    positions.extend_from_slice(&[
        [SHELF[0], y, SHELF[2]],
        [SHELF[1], y, SHELF[2]],
        [SHELF[1], y, SHELF[3]],
        [SHELF[0], y, SHELF[3]],
    ]);
    indices.extend_from_slice(&[s, s + 1, s + 2, s, s + 2, s + 3]);
    (positions, indices)
}

struct Mouth {
    layers: TerrainMeshLayers,
    body: Vec<[[f32; 3]; 3]>,
}

fn mouth(map: &TerrainMap) -> &'static Mouth {
    static MOUTH: OnceLock<Mouth> = OnceLock::new();
    MOUTH.get_or_init(|| {
        let ring = ring(map);
        let (positions, indices) = authored(map, &ring);
        let ground = |x: f32, z: f32| map.sample_bilinear(x, z);
        let inside = |x: f32, z: f32| point_in_ring(&ring, x, z);
        let output = zipper(&ZipperInput {
            hole: "mouth",
            ring: &ring,
            positions: &positions,
            indices: &indices,
            params: ZipperParams::default(),
            ground_y: &ground,
            inside_cut: &inside,
            source_tris: &[],
        })
        .expect("the zipper stitches the mouth");
        let body = output.body.iter().map(|t| t.v).collect();
        let mut layers = TerrainMeshLayers::new();
        layers.push_mouth(LayerMouth::from_zipper("mouth", ring, output, Vec::new(), true));
        Mouth { layers, body }
    })
}

fn draw(map: &TerrainMap, bands: bool) -> (TerrainSharedDraw, TerrainDrawPasses) {
    let stack = TerrainLayers {
        layers: vec![LayerDesc {
            name: "ground".into(),
            albedo: [1.0, 1.0, 1.0],
            roughness: 1.0,
            slope_min: 0.0,
            slope_max: 1.6,
            altitude_min: -100.0,
            altitude_max: 100.0,
            ..LayerDesc::ground()
        }],
        ..Default::default()
    };
    let v = regia_terrain::linear_to_srgb8(0.4);
    let mut draw =
        TerrainSharedDraw::new(device(), map.clone(), &stack, CdlodConfig::default(), ROOT).expect("terrain");
    draw.set_material(
        &TerrainMaterial {
            layers: vec![Some(LayerTextures {
                albedo: Rgba8Image::solid(64, 64, [v, v, v, 255]),
                normal: Rgba8Image::neutral_normal(64),
                height: Rgba8Image::neutral_height(64),
                orm: Rgba8Image::solid(64, 64, [255, 255, 0, 255]),
                use_orm: false,
            })],
            macro_set: None,
            params: MaterialParams::default(),
        },
        &[],
    )
    .expect("material");
    draw.set_holes(Some(&mask()));
    draw.set_hole_lattice(Some(lattice(map)));
    if bands {
        let bands = MouthBands::from_layers(&mouth(map).layers).expect("an open mouth's band");
        draw.set_mouth_bands(Some(Arc::new(bands)));
    }
    let passes = draw.draw_passes_handle();
    (draw, passes)
}

fn tri_mesh(t: [[f32; 3]; 3], material: usize) -> LabMesh {
    let a = Vec3::from_array(t[1]) - Vec3::from_array(t[0]);
    let b = Vec3::from_array(t[2]) - Vec3::from_array(t[0]);
    let normal = a.cross(b).normalize_or_zero().to_array();
    LabMesh {
        vertices: t
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2],
        material,
        ..LabMesh::unskinned()
    }
}

fn meshes(map: &TerrainMap) -> Vec<LabMesh> {
    let mut meshes: Vec<LabMesh> = mouth(map).body.iter().map(|t| tri_mesh(*t, 0)).collect();
    let (r, y) = (200.0, VOID_Y);
    meshes.push(tri_mesh([[-r, y, -r], [-r, y, r], [r, y, r]], 1));
    meshes.push(tri_mesh([[-r, y, -r], [r, y, r], [r, y, -r]], 1));
    meshes
}

fn materials() -> Vec<LabMaterial> {
    vec![
        LabMaterial {
            base_color: [0.45, 0.30, 0.18, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        },
        LabMaterial {
            base_color: [1.0, 0.0, 1.0, 1.0],
            unlit: true,
            ..LabMaterial::default()
        },
    ]
}

/// The third frame of `eye` looking at `target` (cascades and history settled).
fn render(map: &TerrainMap, bands: bool, eye: [f32; 3], target: [f32; 3]) -> (Vec<u8>, TerrainDrawPasses) {
    render_path(map, bands, &[eye], target)
}

/// Three frames at `eyes[0]`, then one frame at each later eye, all looking at `target`.
fn render_path(
    map: &TerrainMap,
    bands: bool,
    eyes: &[[f32; 3]],
    target: [f32; 3],
) -> (Vec<u8>, TerrainDrawPasses) {
    let (terrain, passes) = draw(map, bands);
    let mut renderer = FrameRenderer::new();
    renderer.set_camera_cut_detection(None);
    renderer.set_shared_draw(Some(Box::new(terrain) as Box<dyn SharedPassDraw + Send + Sync>));
    // No time of day: a clock would replace the scene's directional light (the side sun).
    renderer.set_outdoor(OutdoorParams {
        planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
        ..OutdoorParams::default()
    });
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let mut lab = LabScene {
        width: W,
        height: H,
        eye: eyes[0],
        target,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new(SUN, [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: materials(),
        meshes: meshes(map),
    };
    let mut pixels = Vec::new();
    let path = std::iter::repeat_n(eyes[0], 2).chain(eyes.iter().copied());
    for eye in path {
        lab.eye = eye;
        let mut frame = lab.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        pixels = renderer.render_offscreen(device(), &frame).expect("frame").pixels;
    }
    (pixels, passes)
}

fn view_proj(eye: [f32; 3], target: [f32; 3]) -> Mat4 {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(eye),
        Vec3::from_array(target),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, NEAR, FAR);
    proj * view
}

fn project(vp: Mat4, p: [f32; 3]) -> Option<[f32; 2]> {
    let c = vp * Vec3::from_array(p).extend(1.0);
    if c.w <= 1.0e-4 {
        return None;
    }
    Some([
        (c.x / c.w * 0.5 + 0.5) * W as f32,
        (0.5 - c.y / c.w * 0.5) * H as f32,
    ])
}

fn is_void(px: &[u8], x: u32, y: u32) -> bool {
    let i = ((y * W + x) * 4) as usize;
    let (r, g, b) = (i32::from(px[i]), i32::from(px[i + 1]), i32::from(px[i + 2]));
    r > g + 60 && b > g + 60
}

/// Void pixels in the frame, and those within 3 px of the projected ring.
fn void_pixels(px: &[u8], vp: Mat4, ring: &[[f32; 3]]) -> (u32, u32) {
    let mut near_ring = BTreeSet::new();
    let n = ring.len();
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        for s in 0..8 {
            let t = s as f32 / 8.0;
            let p = [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ];
            let Some([sx, sy]) = project(vp, p) else {
                continue;
            };
            for dy in -3..=3 {
                for dx in -3..=3 {
                    let (x, y) = (sx as i32 + dx, sy as i32 + dy);
                    if x >= 0 && y >= 0 && x < W as i32 && y < H as i32 {
                        near_ring.insert((x as u32, y as u32));
                    }
                }
            }
        }
    }
    let mut all = 0;
    let mut along = 0;
    for y in 0..H {
        for x in 0..W {
            if is_void(px, x, y) {
                all += 1;
                if near_ring.contains(&(x, y)) {
                    along += 1;
                }
            }
        }
    }
    (all, along)
}

/// Views of the mouth from 14 m to 120 m, so the ring is drawn on several levels and morphs.
const VIEWS: [([f32; 3], [f32; 3]); 4] = [
    ([0.0, 12.0, -16.0], [0.0, -4.0, 2.0]),
    ([-14.0, 16.0, 22.0], [0.0, -3.0, -1.0]),
    ([30.0, 30.0, -40.0], [0.0, -2.0, 0.0]),
    ([0.0, 70.0, -100.0], [0.0, 0.0, 0.0]),
];

#[test]
fn no_void_pixel_along_the_mouth() {
    let map = map();
    let ring = mouth(&map).layers.mouths()[0].rings[0].clone();
    let mut control_total = 0;
    for (k, (eye, target)) in VIEWS.into_iter().enumerate() {
        let vp = view_proj(eye, target);
        let (with_band, _) = render(&map, true, eye, target);
        let (no_band, _) = render(&map, false, eye, target);
        dump(&format!("void_{k}_band"), &with_band);
        dump(&format!("void_{k}_no_band"), &no_band);
        let (all, along) = void_pixels(&with_band, vp, &ring);
        let (c_all, c_along) = void_pixels(&no_band, vp, &ring);
        println!(
            "view {k}: band void {all} (along the ring {along}); no band void {c_all} (along {c_along})"
        );
        assert_eq!(
            along, 0,
            "view {k}: {along} void pixels along the mouth with the band drawn"
        );
        assert_eq!(all, 0, "view {k}: {all} void pixels anywhere with the band drawn");
        control_total += c_along;
    }
    // The control: without the band the ground's cut edge opens onto the void.
    assert!(
        control_total > 20,
        "the detector sees the void without the band ({control_total} pixels)"
    );
}

/// Writes `px` as a binary PPM under `REGIA_BAND_DUMP` (a debugging aid; nothing without it).
fn dump(name: &str, px: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_BAND_DUMP") else {
        return;
    };
    let mut out = format!("P6\n{W} {H}\n255\n").into_bytes();
    for p in px.as_chunks::<4>().0 {
        out.extend_from_slice(&p[..3]);
    }
    let path = std::path::Path::new(&dir).join(format!("{name}.ppm"));
    std::fs::write(&path, out).expect("dump");
}

/// Mean BT.709 luma over a 5 × 5 box at pixel `p`.
fn luma_box(px: &[u8], p: [f32; 2]) -> f32 {
    let (cx, cy) = (p[0] as i32, p[1] as i32);
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in cy - 2..=cy + 2 {
        for x in cx - 2..=cx + 2 {
            let i = ((y as u32 * W + x as u32) * 4) as usize;
            sum += 0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

/// `None` when the bands are bit for bit equal; else the count, the first differing index and
/// both values there.
fn band_mismatch(drawn: &[[f32; 3]], expected: &[[f32; 3]]) -> Option<(usize, usize, [f32; 3], [f32; 3])> {
    if drawn.len() != expected.len() {
        return Some((usize::MAX, drawn.len(), [0.0; 3], [0.0; 3]));
    }
    let bits = |p: &[f32; 3]| p.map(f32::to_bits);
    let differ: Vec<usize> = (0..drawn.len())
        .filter(|&i| bits(&drawn[i]) != bits(&expected[i]))
        .collect();
    differ.first().map(|&i| (differ.len(), i, drawn[i], expected[i]))
}

#[test]
fn band_casts_with_the_cascade_selection() {
    let map = map();
    let (eye, target) = ([0.0, 35.0, -14.0], [1.0, FLOOR_Y, 1.0]);
    let vp = view_proj(eye, target);
    // The shelf (a throat triangle pair, drawn as band) shadows the floor at x 0.2..3.7 under
    // z -4..4; the floor at z 6..8 is lit.
    let shadowed = project(vp, [2.0, FLOOR_Y, 0.0]).expect("shadowed floor on screen");
    let lit = project(vp, [2.0, FLOOR_Y, 7.0]).expect("lit floor on screen");
    let (with_band, passes) = render(&map, true, eye, target);
    let (no_band, _) = render(&map, false, eye, target);
    dump("shadow_band", &with_band);
    dump("shadow_no_band", &no_band);
    println!("shadowed pixel {shadowed:?}, lit pixel {lit:?}");
    let ratio = luma_box(&with_band, shadowed) / luma_box(&with_band, lit).max(1.0);
    let control = luma_box(&no_band, shadowed) / luma_box(&no_band, lit).max(1.0);
    println!(
        "floor shadowed / lit: band {ratio:.3} ({:.1} / {:.1}), no band {control:.3}",
        luma_box(&with_band, shadowed),
        luma_box(&with_band, lit)
    );
    assert!(
        luma_box(&with_band, lit) > 20.0,
        "the lit floor is lit ({:.1})",
        luma_box(&with_band, lit)
    );
    assert!(ratio < 0.6, "the band's shelf shadows the floor ({ratio:.3})");
    assert!(
        control > 0.85,
        "without the band the floor is lit there ({control:.3})"
    );

    // Each cascade's band is the band placed on that cascade's own selection and morph eye.
    let bands = MouthBands::from_layers(&mouth(&map).layers).expect("bands");
    let mips = HeightMips::build(&map);
    // The draw raises the grid size and the root level (`TerrainSharedDraw::new`): place with its own.
    let (probe, _) = draw(&map, false);
    let (config, root) = (probe.config().clone(), probe.root_level());
    drop(probe);
    let near = check_passes(&passes, &bands, &map, &config, root, &mips);
    // From 230 m the ring sits in level 0's morph band (level 0 reaches 128 m on this map). The
    // camera then moves 6 m: its own selection and morph follow, while each cascade keeps the
    // selection and morph eye anchored at the first eye (a cascade re-anchors only after a node
    // size), so a cascade's band must differ from the camera's and follow its own.
    let d = 230.0f32;
    let eyes = [[0.0, 0.4 * d, -d], [0.0, 0.4 * d, 6.0 - d]];
    let (_, far_passes) = render_path(&map, true, &eyes, [0.0, 0.0, 0.0]);
    let (count, far_moved) = check_passes(&far_passes, &bands, &map, &config, root, &mips);
    println!(
        "view at {d} m: {count} cascades, largest ring height change from the camera's band {far_moved:.5} m"
    );
    describe(&far_passes, &map, &config, root, &mips, &bands);
    println!("near: {} cascades, change {:.5} m", near.0, near.1);
    assert!(near.0 > 0, "the frames drew sun cascades");
    assert!(
        far_moved > 1.0e-4,
        "a cascade's own selection moves the ring off the camera's ({far_moved})"
    );
}

/// Prints, per pass, the levels, morph range and coverage of the ring attach.
fn describe(
    passes: &TerrainDrawPasses,
    map: &TerrainMap,
    config: &CdlodConfig,
    root: u32,
    mips: &HeightMips,
    bands: &MouthBands,
) {
    let terrain = regia_terrain::Terrain {
        map: map.clone(),
        cdlod: config.clone(),
        root_level: root,
        minmax: regia_terrain::MinMaxPyramid::build(map, root).expect("pyramid"),
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    };
    for name in passes.names() {
        let pass = passes.pass(&name).expect("pass");
        let detail =
            regia_terrain::attach_ring_detail(&terrain, mips, &pass.selection, pass.eye, &bands.ring_points);
        let levels: BTreeSet<u32> = detail.iter().map(|a| a.node.level).collect();
        let t_max = detail.iter().flat_map(|a| a.morph_t).fold(0.0f32, f32::max);
        let uncovered = detail.iter().filter(|a| !a.covered).count();
        println!(
            "  {name}: eye {:?} levels {levels:?} morph max {t_max:.3} uncovered {uncovered}",
            pass.eye
        );
    }
}

/// Asserts the camera's and every cascade's band equal [`MouthBands::place`] on that pass's own
/// published selection and morph eye; returns the cascade count and the largest ring height
/// difference between a cascade's band and the camera's.
fn check_passes(
    passes: &TerrainDrawPasses,
    bands: &MouthBands,
    map: &TerrainMap,
    config: &CdlodConfig,
    root: u32,
    mips: &HeightMips,
) -> (u32, f32) {
    let main = passes.pass("main").expect("the camera pass");
    assert!(!main.band.is_empty(), "the camera pass drew the band");
    let expected = bands.place(map, config, root, mips, &main.selection, main.eye);
    assert_eq!(
        band_mismatch(&main.band, &expected),
        None,
        "the camera's band is placed on the camera's selection"
    );
    let mut cascades = 0;
    let mut moved = 0.0f32;
    for name in passes.names().into_iter().filter(|n| n.starts_with("cascade")) {
        let pass = passes.pass(&name).expect("pass");
        let expected = bands.place(map, config, root, mips, &pass.selection, pass.eye);
        assert_eq!(
            band_mismatch(&pass.band, &expected),
            None,
            "{name}: its band is placed on its own selection"
        );
        for (a, b) in pass.band.iter().zip(&main.band) {
            moved = moved.max((a[1] - b[1]).abs());
        }
        cascades += 1;
    }
    (cascades, moved)
}

/// A renderer over the mouth scene ([`draw`]) that keeps its terrain counters, for the passes
/// beyond the camera's colour image (ADR-8824).
struct Rig {
    renderer: FrameRenderer,
    stats: Arc<TerrainDrawStats>,
    quality: FrameQuality,
    post: PostSettings,
    lab: LabScene,
}

impl Rig {
    fn new(map: &TerrainMap, bands: bool, eye: [f32; 3], target: [f32; 3]) -> Self {
        let (terrain, _) = draw(map, bands);
        let stats = terrain.stats();
        let mut renderer = FrameRenderer::new();
        renderer.set_camera_cut_detection(None);
        renderer.set_shared_draw(Some(Box::new(terrain) as Box<dyn SharedPassDraw + Send + Sync>));
        renderer.set_outdoor(OutdoorParams {
            planet: AtmosphereParameters::from_preset(AtmospherePreset::Earth),
            ..OutdoorParams::default()
        });
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            ..PostSettings::golden_deterministic()
        };
        let lab = LabScene {
            width: W,
            height: H,
            eye,
            target,
            fov_y: FOV_Y,
            near: NEAR,
            far: FAR,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 0.0,
            double_sided: true,
            post: Some(post.clone()),
            directional: DirectionalLightUniform::new(SUN, [1.0, 1.0, 1.0], 100_000.0),
            local_lights: Vec::new(),
            materials: materials(),
            meshes: meshes(map),
        };
        Self {
            renderer,
            stats,
            quality,
            post,
            lab,
        }
    }

    /// One main-camera frame; its pixels.
    fn frame(&mut self) -> Vec<u8> {
        let mut frame = self.lab.as_frame_scene_with_quality(self.quality);
        frame.post = Some(&self.post);
        self.renderer
            .render_offscreen(device(), &frame)
            .expect("frame")
            .pixels
    }

    /// One frame of texture view `id` from `eye` (the view's own camera, the reduced feature set
    /// of [`TextureViewSettings::default`]); its pixels.
    fn view_frame(&mut self, id: u64, eye: [f32; 3], target: [f32; 3]) -> Vec<u8> {
        let mut lab = self.lab.clone();
        lab.eye = eye;
        lab.target = target;
        let mut frame = lab.as_frame_scene_with_quality(self.quality);
        frame.post = Some(&self.post);
        self.renderer
            .render_texture_view(device(), id, frame, &TextureViewSettings::default())
            .expect("texture view frame");
        self.renderer
            .read_texture_target(device(), id)
            .expect("texture view pixels")
            .pixels
    }
}

/// A texture view (a probe capture, mirror or screen camera) draws the band placed on its own
/// selection: no void along the mouth, from a view whose camera is not the main camera's.
/// Fail-before (ADR-8569): a texture view drew no band, so it saw the void like the control.
#[test]
fn no_void_pixel_along_the_mouth_in_a_texture_view() {
    let map = map();
    let ring = mouth(&map).layers.mouths()[0].rings[0].clone();
    // The main camera stays far and high; the views look into the mouth from their own eyes.
    let main = ([0.0, 140.0, -200.0], [0.0, 0.0, 0.0]);
    let mut control_total = 0;
    for (k, (eye, target)) in VIEWS.into_iter().take(3).enumerate() {
        let vp = view_proj(eye, target);
        let mut outcome = [(0u32, 0u32); 2];
        for (bands, slot) in [(true, 0usize), (false, 1)] {
            let mut rig = Rig::new(&map, bands, main.0, main.1);
            rig.frame();
            // The view's first prepare places its own bands; the second frame has its cascades.
            rig.view_frame(7, eye, target);
            let px = rig.view_frame(7, eye, target);
            dump(
                &format!("view_{k}_{}", if bands { "band" } else { "no_band" }),
                &px,
            );
            outcome[slot] = void_pixels(&px, vp, &ring);
            if bands {
                let views = rig.stats.view_band_draws();
                println!("view {k}: texture view band draws {views:?}");
                assert!(
                    views.color > 0,
                    "view {k}: the texture view's colour pass drew the band"
                );
            }
        }
        let [(all, along), (c_all, c_along)] = outcome;
        println!(
            "texture view {k}: band void {all} (along the ring {along}); no band void {c_all} (along {c_along})"
        );
        assert_eq!(along, 0, "texture view {k}: {along} void pixels along the mouth");
        assert_eq!(all, 0, "texture view {k}: {all} void pixels anywhere");
        control_total += c_along;
    }
    assert!(
        control_total > 20,
        "the detector sees the void in a texture view without the band ({control_total} pixels)"
    );
}

/// The precipitation occlusion tile stores the band's ground: a point under the zipper strip is
/// covered with the band and open through the cut without it. Fail-before (ADR-8569): the tile
/// drew no band, so rain fell through the strip between the ring and the authored loop.
#[test]
fn the_band_covers_the_precipitation_tile() {
    let map = map();
    // Under the zipper strip on the +x side (the ring near x 6, the authored loop 0.8 m inside it).
    let under_strip = [5.6, -2.0, 0.0];
    // Outside the cut, under the ground: covered either way (the tile holds the terrain).
    let under_ground = [12.0, height(12.0, 0.0) - 2.0, 0.0];
    let mut seen = Vec::new();
    for bands in [true, false] {
        let mut rig = Rig::new(&map, bands, [0.0, 24.0, -6.0], [0.0, 0.0, 0.0]);
        rig.renderer.set_surface_weather(SurfaceWeatherFrame {
            wetness: 1.0,
            ..SurfaceWeatherFrame::DRY
        });
        rig.renderer
            .set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
        rig.frame();
        rig.renderer.request_precip_occlusion_readback();
        rig.frame();
        let tile = rig
            .renderer
            .take_precip_occlusion_readback()
            .expect("the occlusion tile was copied");
        let strip = tile.exposure_at(under_strip);
        let ground = tile.exposure_at(under_ground);
        let counts = rig.stats.band_draws();
        println!(
            "bands {bands}: under the strip {strip:?}, under the ground {ground:?}, precip tile band draws {}",
            counts.precip_tile
        );
        assert!(
            ground.0 < 0.05,
            "bands {bands}: the ground covers a point under it ({ground:?})"
        );
        seen.push((strip, counts.precip_tile));
    }
    let [(with, drawn), (without, none)] = [seen[0], seen[1]];
    assert!(drawn > 0, "the tile drew the band");
    assert_eq!(none, 0, "no band, no band draw");
    assert!(
        without.0 > 0.95,
        "control: without the band the cut is open to the rain there ({without:?})"
    );
    assert!(
        with.0 < 0.05,
        "the band covers the point under its strip ({with:?})"
    );
    let cover = with.1.expect("an occluder above");
    assert!(
        cover > under_strip[1] && cover < height(6.0, 0.0) + 0.5,
        "the occluder is the strip, between the point and the ring ({cover})"
    );
}

/// Every pass that draws terrain draws the band, on the main camera and in a texture view, and a
/// draw without bands counts none (ADR-8824). The passes a frame runs here: depth prepass, colour,
/// velocity (temporal AA on, no depth-velocity), every sun cascade, and the texture view's depth,
/// colour and cascades.
#[test]
fn every_terrain_pass_draws_the_band() {
    let map = map();
    let (eye, target) = VIEWS[1];
    let mut runs = Vec::new();
    for bands in [true, false] {
        let mut rig = Rig::new(&map, bands, eye, target);
        rig.quality.taau = true;
        // The terrain's own velocity pass runs when motion is not reconstructed from depth.
        rig.quality.depth_velocity = false;
        for _ in 0..3 {
            rig.frame();
        }
        rig.view_frame(9, VIEWS[0].0, VIEWS[0].1);
        runs.push((rig.stats.band_draws(), rig.stats.view_band_draws()));
    }
    let [(main, view), (main_none, view_none)] = [runs[0], runs[1]];
    println!("main camera band draws {main:?}");
    println!("texture view band draws {view:?}");
    assert_eq!(
        main_none,
        BandDrawCounts::default(),
        "no band: the main camera counts none"
    );
    assert_eq!(
        view_none,
        BandDrawCounts::default(),
        "no band: the view counts none"
    );
    assert_eq!(main.color, 3, "one colour band draw a frame");
    assert_eq!(main.depth, 3, "one depth prepass band draw a frame");
    assert!(
        main.velocity > 0,
        "the velocity pass drew the band ({})",
        main.velocity
    );
    assert!(
        main.cascades.iter().filter(|&&n| n > 0).count() >= 2,
        "the sun cascades drew the band ({:?})",
        main.cascades
    );
    assert!(view.color > 0, "the texture view's colour pass drew the band");
    assert!(
        view.cascades.iter().any(|&n| n > 0),
        "the texture view's cascades drew the band ({:?})",
        view.cascades
    );
}
