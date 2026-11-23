// SPDX-License-Identifier: MPL-2.0
//! The lightmap debug views on the baker's reference scene (ADR-2656, `specs/lightmaps.md` § 8 phase
//! 4): the quad-box floor baked by `regia_pt`, drawn through the app's own systems
//! (`apply_lightmap_receivers`, `extract_frame`, `apply_debug_view`) on lavapipe. Each texel centre of
//! the floor is projected to its pixel and read back. No golden image.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::prelude::*;
use regia_asset::lightmap::{Coverage, Lightmap};
use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec2, Vec3, Vec4};
use regia_pt::lightmap::{
    LightmapBakeSettings, LightmapScene, MeshLightmapRequest, QuadBoxScene, bake_mesh_lightmap,
    quad_box_scene,
};
use regia_render::debug_view::{DebugView, apply_debug_view};
use regia_render::lightmap::{
    InstalledLightmap, LightmapReceiver, LightmapReceiverReport, LightmapSettings, apply_lightmap_receivers,
    receiver_mesh,
};
use regia_render::lightmap_debug::{
    DENSITY_PALETTE, INVALID_TEXEL, NON_RECEIVER_COLOR, density_color, entry_density_scale,
    lightmap_debug_atlas,
};
use regia_render::{
    Camera, CpuMesh, DebugViewMode, DirectionalLight, ExtractedFrame, LabMaterial, LabVertex, MeshRenderer,
    PostSettings, RenderDevice, RenderSettings, RuntimeRenderer, SsaoMode, extract_frame,
    horizontal_plus_fov_y,
};
use regia_scene::{GlobalTransform, Transform};

const W: u32 = 256;
const H: u32 = 256;
const EYE: [f32; 3] = [-0.8, 3.0, 1.2];
const TARGET: [f32; 3] = [-0.4, 0.0, 0.0];
const EXPOSURE_EV: f32 = 3.0;
const FOV_Y: f32 = 0.9;
/// The vertical FOV the last frame drew, as `f32` bits: a horizontal-plus profile widens `FOV_Y` below
/// 16:9, and this square frame is narrower. 0 before the first frame.
static DRAWN_FOV_Y: AtomicU32 = AtomicU32::new(0);
/// How far a debug colour's bytes may sit from the texture's: 8-bit rounding and a cell's filtered edge.
const COLOR_TOLERANCE: i32 = 10;
/// One test at a time: the frame path's traces count the whole process.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lab_vertices(qb: &QuadBoxScene, mesh: usize) -> (Vec<LabVertex>, Vec<u32>) {
    let m = &qb.scene.meshes[mesh];
    let vertices = m
        .positions
        .iter()
        .zip(&m.normals)
        .enumerate()
        .map(|(i, (p, n))| LabVertex::from_pos_normal_uv(*p, *n, m.uvs.get(i).copied().unwrap_or([0.0, 0.0])))
        .collect();
    (vertices, m.indices.clone())
}

fn bake() -> Lightmap {
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten the quad+box scene");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [36, 36],
        padding: 2,
    };
    let settings = LightmapBakeSettings {
        samples: 16,
        bounces: 2,
        seed: 7,
        direct_sun: false,
        threads: 4,
        ..LightmapBakeSettings::default()
    };
    bake_mesh_lightmap(&scene, &request, &settings, |done, total| assert!(done <= total))
        .expect("bake the floor")
        .0
}

#[derive(Resource, Default)]
struct Drawn(Vec<u8>);

fn draw(
    device: Res<RenderDevice>,
    view: Res<DebugView>,
    extracted: Res<ExtractedFrame>,
    mut renderer: ResMut<RuntimeRenderer>,
    mut drawn: ResMut<Drawn>,
) {
    // The fixture's light is a few lux (a sky of radiance 1): the quad-box frames' exposure. The surface
    // views are unlit, and with no post stack their bytes are the texture's.
    let post = PostSettings {
        exposure_ev: EXPOSURE_EV,
        ..PostSettings::golden_deterministic()
    };
    let mode = view.mode;
    let mut scene = extracted.frame_scene(0, W, H);
    scene.post = (mode == DebugViewMode::LightmapOnly).then_some(&post);
    scene.exposure_ev = EXPOSURE_EV;
    scene.quality.taau = false;
    scene.quality.render_scale_permille = 1000;
    scene.quality.ssao = SsaoMode::Off;
    scene.fov_y = FOV_Y;
    let drawn_fov = if scene.quality.horizontal_plus {
        horizontal_plus_fov_y(FOV_Y, W as f32 / H as f32)
    } else {
        FOV_Y
    };
    DRAWN_FOV_Y.store(drawn_fov.to_bits(), Ordering::Relaxed);
    scene.eye = EYE;
    scene.target = TARGET;
    scene.near = 0.1;
    scene.far = 50.0;
    drawn.0 = renderer
        .0
        .render_offscreen(&device, &scene)
        .unwrap_or_else(|e| panic!("frame: {e}"))
        .pixels;
}

/// What a frame draws.
#[derive(Clone)]
struct Setup {
    mode: DebugViewMode,
    floor_color: [f32; 3],
    with_box: bool,
    sun_lux: f32,
    intensity: f32,
    enabled: bool,
}

impl Setup {
    fn new(mode: DebugViewMode) -> Self {
        Self {
            mode,
            floor_color: [QuadBoxScene::FLOOR_ALBEDO; 3],
            with_box: true,
            sun_lux: 0.0,
            intensity: 1.0,
            enabled: true,
        }
    }
}

/// The floor (a receiver of `floor`) and the box, through the app's systems. Runs until the lightmap
/// path's pipelines are built, so the frame does not hold a fallback.
fn frame(device: &RenderDevice, map: &Lightmap, s: &Setup) -> Vec<u8> {
    let qb = quad_box_scene();
    let (floor_v, floor_i) = lab_vertices(&qb, qb.scene.instances[qb.floor].mesh);
    let (box_v, box_i) = lab_vertices(&qb, qb.scene.instances[qb.cube].mesh);
    let lambert = |c: [f32; 3]| {
        Arc::new(LabMaterial {
            base_color: [c[0], c[1], c[2], 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        })
    };
    let mut app = App::new();
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<Drawn>();
    app.init_resource::<LightmapReceiverReport>();
    app.insert_resource(RenderSettings::default());
    app.insert_resource(DebugView {
        mode: s.mode,
        ..DebugView::default()
    });
    app.insert_resource(LightmapSettings {
        enabled: s.enabled,
        intensity: s.intensity,
        ..LightmapSettings::default()
    });
    let mut installed = InstalledLightmap::default();
    installed.install("quad_box", map.clone());
    app.insert_resource(installed);
    let at = Transform::default();
    app.world_mut().spawn((
        at,
        GlobalTransform(at.compute_affine()),
        MeshRenderer::new(
            Arc::new(CpuMesh {
                vertices: floor_v,
                indices: floor_i,
            }),
            lambert(s.floor_color),
        ),
        LightmapReceiver {
            entry: "floor".into(),
        },
    ));
    if s.with_box {
        app.world_mut().spawn((
            at,
            GlobalTransform(at.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: box_v,
                    indices: box_i,
                }),
                lambert(QuadBoxScene::BOX_ALBEDO),
            ),
        ));
    }
    if s.sun_lux > 0.0 {
        app.world_mut().spawn((
            at,
            GlobalTransform(at.compute_affine()),
            DirectionalLight {
                direction: (-Vec3::from_array(QuadBoxScene::SUN_TOWARD)).to_array(),
                color: [1.0; 3],
                illuminance: s.sun_lux,
                cast_shadows: false,
                static_shadow: false,
            },
        ));
    }
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y).inverse();
    let (_, rotation, translation) = view.to_scale_rotation_translation();
    let eye = Transform {
        translation,
        rotation,
        scale: Vec3::ONE,
    };
    app.world_mut()
        .spawn((eye, GlobalTransform(eye.compute_affine()), Camera::default()));
    app.add_systems(
        Render,
        (apply_lightmap_receivers, extract_frame, apply_debug_view, draw).chain(),
    );
    for _ in 0..3 {
        app.update();
    }
    if s.mode == DebugViewMode::LightmapOnly && s.enabled {
        let stats = app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .lightmap_forward_stats();
        assert_eq!((stats.drawn, stats.fallback), (1, 0), "{stats:?}");
    }
    let report = app.world().resource::<LightmapReceiverReport>();
    assert_eq!(report.applied, 1, "{report:?}");
    let pixels = app.world().resource::<Drawn>().0.clone();
    assert_eq!(pixels.len(), (W * H * 4) as usize);
    evidence(s, &pixels);
    pixels
}

/// With `REGIA_LIGHTMAP_EVIDENCE_DIR` set, writes the frame there as a PNG, to read by eye (not a
/// golden).
fn evidence(s: &Setup, pixels: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_LIGHTMAP_EVIDENCE_DIR") else {
        return;
    };
    let name = format!(
        "debug_{:?}_box{}_sun{}_i{}_on{}.png",
        s.mode, s.with_box, s.sun_lux, s.intensity, s.enabled
    );
    let file = std::fs::File::create(std::path::Path::new(&dir).join(name)).expect("png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(pixels))
        .expect("write png");
}

/// The vertical FOV the frames drew (`DRAWN_FOV_Y`); a frame must have been drawn first.
fn drawn_fov_y() -> f32 {
    let bits = DRAWN_FOV_Y.load(Ordering::Relaxed);
    assert_ne!(bits, 0, "draw a frame before projecting into it");
    f32::from_bits(bits)
}

/// Pixel of world point `p` (the frame's right-handed view, depth 0..1).
fn pixel(p: Vec3) -> Option<(u32, u32)> {
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj: Mat4 = perspective(drawn_fov_y(), W as f32 / H as f32, 0.1, 50.0);
    let clip = proj * view * Vec4::new(p.x, p.y, p.z, 1.0);
    let ndc = clip.truncate() / clip.w;
    let (x, y) = ((ndc.x * 0.5 + 0.5) * W as f32, (0.5 - ndc.y * 0.5) * H as f32);
    (x >= 1.0 && y >= 1.0 && x < (W - 1) as f32 && y < (H - 1) as f32).then_some((x as u32, y as u32))
}

fn rgb(img: &[u8], (x, y): (u32, u32)) -> [i32; 3] {
    let i = ((y * W + x) * 4) as usize;
    [i32::from(img[i]), i32::from(img[i + 1]), i32::from(img[i + 2])]
}

/// A frame pixel whose centre lies on the floor: the atlas texel under that centre (found by casting
/// the pixel's own ray at the floor's triangles and reading their UV2, so no projection is rounded),
/// the world point, and how far the point sits from its cell's nearest edge, in texels.
struct FloorPixel {
    at: (u32, u32),
    texel: (u32, u32),
    world: Vec3,
    edge: f32,
}

impl FloorPixel {
    /// Away from a cell's edge, where the texture's filter does not mix in the neighbouring cell.
    fn inside_cell(&self) -> bool {
        self.edge >= CELL_EDGE
    }
}

/// How far from a cell's edge a pixel must sit to show the cell's own colour: the debug texture holds
/// a texel as 4 × 4 pixels, and its bilinear filter mixes neighbours within half of one of them.
const CELL_EDGE: f32 = 0.2;

/// Every frame pixel whose centre's ray hits the floor (nearest hit over the floor's triangles).
fn floor_pixels(map: &Lightmap) -> Vec<FloorPixel> {
    let qb = quad_box_scene();
    let (v, i) = lab_vertices(&qb, qb.scene.instances[qb.floor].mesh);
    let built = receiver_mesh(map, 0, &v, &i).expect("the baked floor");
    let so = map.entries[0].scale_offset;
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj: Mat4 = perspective(drawn_fov_y(), W as f32 / H as f32, 0.1, 50.0);
    let inv = (proj * view).inverse();
    let unproject = |ndc: Vec2, z: f32| {
        let p = inv * Vec4::new(ndc.x, ndc.y, z, 1.0);
        p.truncate() / p.w
    };
    let tris: Vec<[usize; 3]> = built
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| t.map(|k| k as usize))
        .collect();
    let pos = |k: usize| Vec3::from_array(built.vertices[k].position);
    let mut out = Vec::new();
    for y in 0..H {
        for x in 0..W {
            let ndc = Vec2::new(
                (x as f32 + 0.5) / W as f32 * 2.0 - 1.0,
                1.0 - (y as f32 + 0.5) / H as f32 * 2.0,
            );
            let origin = unproject(ndc, 0.0);
            let dir = unproject(ndc, 1.0) - origin;
            // Moller-Trumbore over the floor's triangles, nearest hit.
            let mut hit: Option<(f32, [usize; 3], f32, f32)> = None;
            for &[a, b, c] in &tris {
                let (e1, e2) = (pos(b) - pos(a), pos(c) - pos(a));
                let q = dir.cross(e2);
                let det = e1.dot(q);
                if det.abs() < 1e-12 {
                    continue;
                }
                let s = origin - pos(a);
                let u = s.dot(q) / det;
                let r = s.cross(e1);
                let w = dir.dot(r) / det;
                let t = e2.dot(r) / det;
                if u >= 0.0 && w >= 0.0 && u + w <= 1.0 && t > 0.0 && hit.is_none_or(|h| t < h.0) {
                    hit = Some((t, [a, b, c], u, w));
                }
            }
            let Some((t, [a, b, c], u, w)) = hit else {
                continue;
            };
            let uv = |k: usize| Vec2::from_array(built.uv2[k]);
            let entry_uv = uv(a) * (1.0 - u - w) + uv(b) * u + uv(c) * w;
            let atlas = Vec2::new(
                (entry_uv.x * so[0] + so[2]) * map.width as f32,
                (entry_uv.y * so[1] + so[3]) * map.height as f32,
            );
            let cell = atlas.floor();
            let f = atlas - cell;
            out.push(FloorPixel {
                at: (x, y),
                texel: (cell.x as u32, cell.y as u32),
                world: origin + dir * t,
                edge: f.x.min(1.0 - f.x).min(f.y).min(1.0 - f.y),
            });
        }
    }
    out
}

fn close(got: [i32; 3], want: [u8; 3]) -> bool {
    (0..3).all(|k| (got[k] - i32::from(want[k])).abs() <= COLOR_TOLERANCE)
}

/// What the box does not hide from this camera.
fn beside_the_box(p: Vec3) -> bool {
    p.x < -0.05
}

/// One cell per texel, coloured by `k`; a map whose entry got twice the density it asked turns the
/// floor from green to yellow; the box, no receiver, draws flat.
#[test]
fn the_density_view_is_a_checker_per_texel_coloured_by_k() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU: {e}"));
    let mut map = bake();
    // The bake's own k, whatever it is: 1 here.
    map.density = map.entries[0].texels_per_metre;
    assert_eq!(entry_density_scale(&map, 0), 1.0);
    let img = frame(&device, &map, &Setup::new(DebugViewMode::LightmapDensity));
    let pixels = floor_pixels(&map);
    let atlas = lightmap_debug_atlas(&map, DebugViewMode::LightmapDensity);
    let (mut checked, mut seen) = (0, std::collections::BTreeSet::new());
    let mut worst = (0, [0; 3], [0; 3], (0, 0));
    for p in pixels
        .iter()
        .filter(|p| beside_the_box(p.world) && p.inside_cell())
    {
        let want = atlas[(p.texel.1 * map.width + p.texel.0) as usize];
        let got = rgb(&img, p.at);
        let err = (0..3)
            .map(|k| (got[k] - i32::from(want[k])).abs())
            .max()
            .unwrap_or(0);
        if err > worst.0 {
            worst = (err, got, [want[0].into(), want[1].into(), want[2].into()], p.at);
        }
        checked += 1;
        seen.insert((p.texel, want == density_color(1.0)));
    }
    let light = seen.iter().filter(|(_, light)| *light).count();
    let dark = seen.len() - light;
    eprintln!(
        "density: {checked} pixels over {} texels, {light} light, {dark} dark; worst {worst:?}",
        seen.len()
    );
    assert!(seen.len() > 200, "{} floor texels in frame", seen.len());
    assert!(light > 50 && dark > 50, "both cells show: {light} {dark}");
    assert!(
        worst.0 <= COLOR_TOLERANCE,
        "pixel {:?}'s cell: got {:?}, want {:?}",
        worst.3,
        worst.1,
        worst.2
    );
    // The box's top.
    let top = pixel(Vec3::new(0.5, 1.0, 0.0)).expect("the box is in frame");
    let flat = NON_RECEIVER_COLOR.map(|c| (c * 255.0 + 0.5) as u8);
    assert!(close(rgb(&img, top), flat), "{:?}", rgb(&img, top));

    // Twice the density it asked for: k = 2.
    map.density /= 2.0;
    assert_eq!(entry_density_scale(&map, 0), 2.0);
    let img = frame(&device, &map, &Setup::new(DebugViewMode::LightmapDensity));
    let yellow = DENSITY_PALETTE[3];
    let at = pixels
        .iter()
        .find(|p| beside_the_box(p.world) && p.edge > 0.3 && (p.texel.0 + p.texel.1).is_multiple_of(2))
        .map(|p| p.at)
        .expect("a light cell in frame");
    assert!(close(rgb(&img, at), yellow), "{:?}", rgb(&img, at));
}

/// The texels the bake filled show magenta, at the place the file says, and only there. The floor
/// under the box is inside geometry: the frame leaves the box out to see them.
#[test]
fn the_invalid_view_marks_the_texels_the_bake_filled() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU: {e}"));
    let map = bake();
    let img = frame(
        &device,
        &map,
        &Setup {
            with_box: false,
            ..Setup::new(DebugViewMode::LightmapInvalid)
        },
    );
    let pixels = floor_pixels(&map);
    let atlas = lightmap_debug_atlas(&map, DebugViewMode::LightmapInvalid);
    let (mut invalid, mut baked) = (
        std::collections::BTreeSet::new(),
        std::collections::BTreeSet::new(),
    );
    // A cell's own colour only away from its edge: the texture is filtered.
    for p in pixels.iter().filter(|p| p.inside_cell()) {
        let k = (p.texel.1 * map.width + p.texel.0) as usize;
        let want = atlas[k];
        let got = rgb(&img, p.at);
        assert!(
            close(got, want),
            "pixel {:?} on texel {:?}: got {got:?}, want {want:?}",
            p.at,
            p.texel
        );
        if map.coverage[k] == Coverage::DILATED {
            assert_eq!(want, INVALID_TEXEL);
            invalid.insert(p.texel);
        } else {
            baked.insert(p.texel);
        }
    }
    let (invalid, baked) = (invalid.len(), baked.len());
    eprintln!("invalid view: {invalid} filled texels, {baked} baked");
    assert!(invalid > 10, "{invalid}: the floor under the box was filled");
    assert!(baked > 100, "{baked}");
    // The file agrees: those texels lie under the box.
    assert!(map.coverage_counts()[2] >= invalid);
}

/// Lightmap only shows the baked light and nothing else: it does not follow the material, the sun or
/// the box's own colour, it follows the lightmap's intensity, the baked red bounce shows beside the
/// box, and with lightmaps off the floor is flat.
#[test]
fn the_lightmap_only_view_shows_the_baked_light_alone() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU: {e}"));
    let map = bake();
    let only = Setup::new(DebugViewMode::LightmapOnly);
    let base = frame(&device, &map, &only);
    let red_floor = frame(
        &device,
        &map,
        &Setup {
            floor_color: [0.9, 0.1, 0.1],
            ..only.clone()
        },
    );
    assert_eq!(base, red_floor, "the view does not read the material");
    let with_sun = frame(
        &device,
        &map,
        &Setup {
            sun_lux: QuadBoxScene::SUN_LUX,
            ..only.clone()
        },
    );
    assert_eq!(base, with_sun, "the view has no live sun");

    let brighter = frame(
        &device,
        &map,
        &Setup {
            intensity: 1.5,
            ..only.clone()
        },
    );
    let far = pixel(Vec3::new(-1.3, 0.0, 0.9)).expect("the far floor is in frame");
    let near_box = pixel(Vec3::new(-0.2, 0.0, 0.0)).expect("the floor by the box is in frame");
    let luma = |c: [i32; 3]| c[0] + c[1] + c[2];
    assert!(
        luma(rgb(&brighter, far)) > luma(rgb(&base, far)) + 6,
        "the view follows the lightmap's intensity: {:?} {:?}",
        rgb(&brighter, far),
        rgb(&base, far)
    );
    let (near_rgb, far_rgb) = (rgb(&base, near_box), rgb(&base, far));
    eprintln!("lightmap only: by the box {near_rgb:?}, far floor {far_rgb:?}");
    let red_share = |c: [i32; 3]| c[0] as f32 / c[1].max(1) as f32;
    assert!(
        red_share(near_rgb) > red_share(far_rgb) + 0.05,
        "the baked bounce off the red box shows: {near_rgb:?} against {far_rgb:?}"
    );
    let flat = NON_RECEIVER_COLOR.map(|c| (c * 255.0 + 0.5) as u8);
    let top = pixel(Vec3::new(0.5, 1.0, 0.0)).expect("the box is in frame");
    assert!(close(rgb(&base, top), flat), "{:?}", rgb(&base, top));

    // No light but the bake: at intensity 0 the floor is black. The frame's air (the atmosphere's
    // aerial perspective and the distance fog) is daylight in-scatter, and at this exposure it
    // painted the floor sky-blue with no baked light at all.
    let unlit = frame(
        &device,
        &map,
        &Setup {
            intensity: 0.0,
            ..only.clone()
        },
    );
    for at in [far, near_box] {
        let c = rgb(&unlit, at);
        assert!(
            c.iter().all(|&v| v <= 4),
            "no light but the bake at {at:?}: {c:?}"
        );
    }
    assert_eq!(
        rgb(&unlit, top),
        rgb(&base, top),
        "the flat box does not follow the lightmap"
    );

    let off = frame(
        &device,
        &map,
        &Setup {
            enabled: false,
            ..only
        },
    );
    assert!(close(rgb(&off, far), flat), "{:?}", rgb(&off, far));
}

/// Another mode leaves the frame as `Lit` draws it: the views cost nothing while off.
#[test]
fn the_lit_view_is_untouched() {
    let _gpu = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU: {e}"));
    let map = bake();
    let lit = frame(&device, &map, &Setup::new(DebugViewMode::Lit));
    let view = frame(&device, &map, &Setup::new(DebugViewMode::LightmapDensity));
    assert_ne!(lit, view, "the density view changes the frame");
    assert_eq!(lit, frame(&device, &map, &Setup::new(DebugViewMode::Lit)));
}
