// SPDX-License-Identifier: MPL-2.0
//! The baker's reference scene end to end (ADR-1623): the quad-box floor baked by `regia_pt`
//! (sky and the sun's bounce, no direct sun), installed on the frame renderer and drawn as a
//! lightmap receiver next to its red box, with the sun live.
//!
//! - Without the lightmap the floor beside the box's sunlit red face is as grey as the far floor
//!   (the flat ambient carries no bounce).
//! - With it, that floor turns red (the baked bounce off the red face), the far floor does not,
//!   and the box itself (not a receiver) is unchanged.
//! - The golden `lightmap_forward_quad_box` is the lit frame, and
//!   `lightmap_forward_quad_box_lite_low` the same frame on the Mobile Low lite shader. A missing
//!   reference image fails the test (ADR-3211): the owner approves both images, and a test that
//!   compares with nothing proves nothing.

use std::sync::Arc;

use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_pt::lightmap::{
    LightmapBakeSettings, LightmapScene, MeshLightmapRequest, QuadBoxScene, bake_mesh_lightmap,
    quad_box_scene,
};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, HardSurfaceFactors,
    LabMaterial, LabTexture, LabVertex, RenderDevice, ShadowCast,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, goldens_skipped, guard_golden,
};

mod lightmap_lite_tier_study;

const W: u32 = 128;
const H: u32 = 96;
const EYE: [f32; 3] = [-1.0, 5.0, 3.5];
const TARGET: [f32; 3] = [-0.2, 0.0, 0.0];
const FOV_Y: f32 = 0.9;
/// The golden frames' camera: the floor fills the frame, so the box's bounce and its shadow are most
/// of what a reference image holds (the checks above keep the wider view with the floor's edges).
const GOLDEN_CAMERA: ([f32; 3], [f32; 3]) = ([-0.7, 2.6, 1.9], [-0.1, 0.0, 0.1]);
const FLOOR_KEY: u64 = 0x0F10_0001;
const BOX_KEY: u64 = 0x0F10_0002;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The scene's mesh `mesh` as frame vertices.
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

/// Screen pixel of world point `p` (the renderer's right-handed view, depth 0..1).
fn pixel(p: [f32; 3]) -> (u32, u32) {
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(FOV_Y, W as f32 / H as f32, 0.1, 50.0);
    let clip = proj * view * Vec4::new(p[0], p[1], p[2], 1.0);
    let ndc = clip.truncate() / clip.w;
    let x = ((ndc.x * 0.5 + 0.5) * W as f32) as u32;
    let y = ((0.5 - ndc.y * 0.5) * H as f32) as u32;
    (x.min(W - 1), y.min(H - 1))
}

/// Mean RGB over the 3 × 3 pixels around world point `p`.
fn rgb_at(img: &[u8], p: [f32; 3]) -> [f32; 3] {
    let (cx, cy) = pixel(p);
    let mut sum = [0.0f32; 3];
    for y in cy - 1..=cy + 1 {
        for x in cx - 1..=cx + 1 {
            let i = ((y * W + x) * 4) as usize;
            for (k, s) in sum.iter_mut().enumerate() {
                *s += f32::from(img[i + k]);
            }
        }
    }
    sum.map(|s| s / 9.0)
}

/// Red over green: 1 for grey, above 1 for a red tint.
fn redness(c: [f32; 3]) -> f32 {
    c[0] / c[1].max(1.0)
}

/// The frame's geometry: the floor drawn from its entry's unwrap (with or without the lightmap,
/// so the two frames differ only in the light) and the box.
struct Geometry {
    floor: regia_render::lightmap::LightmapReceiverMesh,
    box_vertices: Vec<LabVertex>,
    box_indices: Vec<u32>,
    grey: LabMaterial,
    red: LabMaterial,
}

fn geometry(map: &regia_asset::lightmap::Lightmap) -> Geometry {
    let qb = quad_box_scene();
    let (floor_v, floor_i) = lab_vertices(&qb, qb.scene.instances[qb.floor].mesh);
    let (box_vertices, box_indices) = lab_vertices(&qb, qb.scene.instances[qb.cube].mesh);
    let lambert = |c: [f32; 3]| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        metallic: 0.0,
        roughness: 1.0,
        ..LabMaterial::default()
    };
    Geometry {
        floor: receiver_mesh(map, 0, &floor_v, &floor_i).expect("the baked floor"),
        box_vertices,
        box_indices,
        grey: lambert([QuadBoxScene::FLOOR_ALBEDO; 3]),
        red: lambert(QuadBoxScene::BOX_ALBEDO),
    }
}

fn frame_scene(g: &Geometry) -> FrameScene<'_> {
    let mesh = |mesh_key, material_key, object_key, vertices, indices, material, uv1| FrameMesh {
        is_static: false,
        mesh_key,
        material_key,
        object_key,
        vertices,
        indices,
        material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::All,
        uv1,
        lod_dither: regia_render::LOD_DITHER_NONE,
    };
    FrameScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 3.0,
        // The white sky's irradiance on the open floor (π · L, L = 1).
        ambient_lux: std::f32::consts::PI * QuadBoxScene::SKY,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new(
            (-Vec3::from_array(QuadBoxScene::SUN_TOWARD)).to_array(),
            [1.0; 3],
            QuadBoxScene::SUN_LUX,
        ),
        local_lights: &[],
        quality: FrameQuality::default(),
        meshes: vec![
            mesh(
                0x0A11_0F01 << 3,
                1,
                FLOOR_KEY,
                &g.floor.vertices[..],
                &g.floor.indices[..],
                &g.grey,
                &g.floor.uv2[..],
            ),
            mesh(
                0x0A11_0F02 << 3,
                2,
                BOX_KEY,
                &g.box_vertices[..],
                &g.box_indices[..],
                &g.red,
                &[],
            ),
        ],
    }
}

/// Renders the floor (a receiver of entry 0 when `lit`) and the box.
fn render(device: &RenderDevice, map: &Arc<regia_asset::lightmap::Lightmap>, lit: bool) -> Vec<u8> {
    render_tier(device, map, lit, None, (EYE, TARGET))
}

/// [`render`] with the lite forward shader forced to `lite`.
fn render_tier(
    device: &RenderDevice,
    map: &Arc<regia_asset::lightmap::Lightmap>,
    lit: bool,
    lite: Option<ForwardLite>,
    view: ([f32; 3], [f32; 3]),
) -> Vec<u8> {
    render_tier_specular(device, map, lit, lite, view, true)
}

/// [`render_tier`] with the receiver's indirect specular from the lightmap or not (ADR-4762).
fn render_tier_specular(
    device: &RenderDevice,
    map: &Arc<regia_asset::lightmap::Lightmap>,
    lit: bool,
    lite: Option<ForwardLite>,
    (eye, target): ([f32; 3], [f32; 3]),
    specular: bool,
) -> Vec<u8> {
    let g = geometry(map);
    let scene = FrameScene {
        eye,
        target,
        ..frame_scene(&g)
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(lite);
    if lit {
        renderer.set_lightmap(
            Some(map.clone()),
            LightmapSettings {
                enabled: true,
                specular,
                ..LightmapSettings::default()
            },
        );
        renderer.set_lightmap_receivers(&[(FLOOR_KEY, 0)]);
    }
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(stats.drawn, usize::from(lit), "{stats:?}");
    pixels
}

/// One lit frame of a tier comparison (ADR-5945): the shader, the lightmap lookup, the frame's
/// quality, its size and the floor's material are the caller's.
struct TierFrame {
    /// The lite forward shader, or the full one.
    lite: Option<ForwardLite>,
    /// The lightmap lookup (enabled).
    settings: LightmapSettings,
    /// The frame's quality: its shadow filter and its lightmap tier are what a comparison turns.
    quality: FrameQuality,
    /// Width and height, pixels.
    size: (u32, u32),
    /// The receiver's material in place of the golden's grey floor, with its material key.
    floor: Option<(u64, LabMaterial)>,
}

impl TierFrame {
    /// The golden frame's configuration on shader `lite`.
    fn golden(lite: Option<ForwardLite>) -> Self {
        Self {
            lite,
            settings: LightmapSettings {
                enabled: true,
                ..LightmapSettings::default()
            },
            quality: FrameQuality::default(),
            size: (W, H),
            floor: None,
        }
    }

    /// The golden view with the floor shaded as `floor` (one of [`floors`]).
    fn with_floor(lite: Option<ForwardLite>, floor: &Floor) -> Self {
        Self {
            floor: Some((floor.key, floor.material.clone())),
            ..Self::golden(lite)
        }
    }
}

/// A receiver material of the tier comparison.
struct Floor {
    /// What it is.
    name: &'static str,
    /// Its material key in the frame (the golden's two materials are 1 and 2).
    key: u64,
    /// The material.
    material: LabMaterial,
}

impl Floor {
    /// A metal: its baked light is all reflection.
    fn is_metal(&self) -> bool {
        self.material.metallic > 0.5
    }

    /// The material has an occlusion map.
    fn is_occluded(&self) -> bool {
        self.material.occlusion_map.is_some()
    }

    /// ADR-5945 changed how the lite shader lights this receiver: a metal gained the
    /// multi-scatter share of its reflection, an occluded non-metal the multi-bounce form of its
    /// occlusion.
    fn changed_by_adr_5945(&self) -> bool {
        self.is_occluded() || (self.is_metal() && self.material.hard_surface.multi_scatter > 0.5)
    }

    /// Mobile Low is held to the full shader on this material. Low uploads no directions
    /// (ADR-2107), so its reflection reads the baked light as the same from everywhere
    /// (ADR-4762); the full shader reads the direction in the lobe, and on a smooth metal the two
    /// differ by the tier's own approximation (3.8 code values on the roughness 0.4 alloy).
    fn low_reads_the_full_field(&self) -> bool {
        !self.is_metal() || self.material.roughness >= 0.7
    }
}

/// The receiver materials the tiers are compared on, by the factors an authored `.material.ron`
/// carries: multi-scatter and the authored specular occlusion are on there by default
/// (`regia_asset::material::HardSurfaceParams`), and off on a bare [`LabMaterial`] such as the
/// golden's grey floor, which is the first entry. The last two have an occlusion map that is
/// [`FLOOR_OCCLUSION`] everywhere: a cavity as large as the floor, so every flat window measures
/// what the tiers do with it.
fn floors() -> Vec<Floor> {
    let authored = |name, key, base: [f32; 3], metallic, roughness| Floor {
        name,
        key,
        material: LabMaterial {
            base_color: [base[0], base[1], base[2], 1.0],
            metallic,
            roughness,
            hard_surface: HardSurfaceFactors {
                multi_scatter: 1.0,
                specular_occlusion: 1.0,
                ..HardSurfaceFactors::OFF
            },
            ..LabMaterial::default()
        },
    };
    let grey = [QuadBoxScene::FLOOR_ALBEDO; 3];
    // A light alloy and a warm one (linear F0).
    let alloy = [0.91, 0.92, 0.92];
    let copper = [0.95, 0.64, 0.54];
    vec![
        Floor {
            name: "bare grey floor (the golden's)",
            key: 1,
            material: LabMaterial {
                base_color: [grey[0], grey[1], grey[2], 1.0],
                metallic: 0.0,
                roughness: 1.0,
                ..LabMaterial::default()
            },
        },
        authored("authored grey floor, roughness 1", 11, grey, 0.0, 1.0),
        authored("authored grey floor, roughness 0.5", 12, grey, 0.0, 0.5),
        authored("authored alloy, roughness 1", 13, alloy, 1.0, 1.0),
        authored("authored alloy, roughness 0.7", 14, alloy, 1.0, 0.7),
        authored("authored alloy, roughness 0.4", 15, alloy, 1.0, 0.4),
        authored("authored copper, roughness 0.8", 16, copper, 1.0, 0.8),
        occluded(authored(
            "authored grey floor, roughness 1, occlusion map",
            17,
            grey,
            0.0,
            1.0,
        )),
        occluded(authored(
            "authored alloy, roughness 1, occlusion map",
            18,
            alloy,
            1.0,
            1.0,
        )),
    ]
}

/// The occlusion the occluded floors' map holds in every texel (128 of 255).
const FLOOR_OCCLUSION: u8 = 128;

/// `floor` with an occlusion map of [`FLOOR_OCCLUSION`] at full strength.
fn occluded(mut floor: Floor) -> Floor {
    floor.material.occlusion_map = Some(LabTexture {
        width: 2,
        height: 2,
        rgba: [FLOOR_OCCLUSION, FLOOR_OCCLUSION, FLOOR_OCCLUSION, 255].repeat(4),
    });
    floor.material.occlusion_strength = 1.0;
    floor
}

/// The lit golden view drawn as `frame` asks.
fn render_frame(
    device: &RenderDevice,
    map: &Arc<regia_asset::lightmap::Lightmap>,
    frame: &TierFrame,
) -> Vec<u8> {
    let g = geometry(map);
    let mut scene = FrameScene {
        width: frame.size.0,
        height: frame.size.1,
        eye: GOLDEN_CAMERA.0,
        target: GOLDEN_CAMERA.1,
        quality: frame.quality,
        ..frame_scene(&g)
    };
    if let Some((key, material)) = &frame.floor {
        scene.meshes[0].material_key = *key;
        scene.meshes[0].material = material;
    }
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(frame.lite);
    renderer.set_lightmap(Some(map.clone()), frame.settings);
    renderer.set_lightmap_receivers(&[(FLOOR_KEY, 0)]);
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let stats = renderer.lightmap_forward_stats();
    assert_eq!(stats.drawn, 1, "{stats:?}");
    pixels
}

/// The golden harness's windows (`image-compare`'s SSIM: 8 × 8 pixels, side by side).
const WINDOW: u32 = 8;
/// A window is flat when its display luma spans at most this many code values in both frames: no
/// silhouette, shadow edge or steep falloff crosses it.
const FLAT_RANGE: f32 = 4.0;

/// Display luma (BT.709, code values 0-255) of the RGBA8 pixel at byte `i`.
fn luma(img: &[u8], i: usize) -> f32 {
    0.2126 * f32::from(img[i]) + 0.7152 * f32::from(img[i + 1]) + 0.0722 * f32::from(img[i + 2])
}

/// How two frames of one view agree (ADR-5945).
#[derive(Debug, Clone, Copy)]
struct TierAgreement {
    /// Windows that are flat in both frames.
    flat: usize,
    /// All windows.
    windows: usize,
    /// Mean over the flat windows of the mean luma step, test minus reference, code values.
    mean_step: f32,
    /// The largest mean luma step of a flat window, either sign, code values.
    max_step: f32,
    /// The golden harness's score of the pair (`regia_test_utils::golden::ssim_rgba8`).
    ssim: f64,
}

/// Compares `test` with `reference` (RGBA8, `width` × `height`).
///
/// The harness's SSIM sums a window's covariance over its 64 pixels against a constant sized for
/// a mean, so on a flat floor it mostly scores where the 8-bit steps of a smooth falloff fall: a
/// frame that is one code value brighter everywhere scores about 0.94, the same as a visibly
/// different one. The flat-window step says which of the two it is.
fn tier_agreement(test: &[u8], reference: &[u8], width: u32, height: u32) -> TierAgreement {
    let (mut flat, mut windows) = (0usize, 0usize);
    let (mut sum_step, mut max_step) = (0.0f32, 0.0f32);
    for y0 in (0..height).step_by(WINDOW as usize) {
        for x0 in (0..width).step_by(WINDOW as usize) {
            windows += 1;
            // Each frame's (sum, min, max) of luma over the window.
            let mut stats = [(0.0f32, f32::MAX, f32::MIN); 2];
            let mut count = 0.0f32;
            for y in y0..(y0 + WINDOW).min(height) {
                for x in x0..(x0 + WINDOW).min(width) {
                    let i = ((y * width + x) * 4) as usize;
                    for (s, img) in stats.iter_mut().zip([test, reference]) {
                        let l = luma(img, i);
                        *s = (s.0 + l, s.1.min(l), s.2.max(l));
                    }
                    count += 1.0;
                }
            }
            if stats.iter().all(|s| s.2 - s.1 <= FLAT_RANGE) {
                let step = (stats[0].0 - stats[1].0) / count;
                flat += 1;
                sum_step += step;
                max_step = max_step.max(step.abs());
            }
        }
    }
    TierAgreement {
        flat,
        windows,
        mean_step: sum_step / flat.max(1) as f32,
        max_step,
        ssim: regia_test_utils::golden::ssim_rgba8(test, reference, width, height).unwrap_or(f64::NAN),
    }
}

/// The floor's lightmap: sky and the sun's bounce (the sun stays live), filtered.
fn bake() -> (
    regia_asset::lightmap::Lightmap,
    regia_pt::lightmap::LightmapBakeReport,
) {
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
        samples: 64,
        bounces: 3,
        seed: 7,
        direct_sun: false,
        threads: 4,
        ..LightmapBakeSettings::default()
    };
    bake_mesh_lightmap(&scene, &request, &settings, |done, total| assert!(done <= total))
        .expect("bake the floor")
}

/// With `REGIA_LIGHTMAP_EVIDENCE_DIR` set, writes `pixels` there as `<name>.png` (a look at the
/// frames by eye; not a golden).
fn evidence(name: &str, pixels: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_LIGHTMAP_EVIDENCE_DIR") else {
        return;
    };
    write_png(&std::path::Path::new(&dir).join(format!("{name}.png")), pixels);
}

fn write_png(path: &std::path::Path, pixels: &[u8]) {
    let file = std::fs::File::create(path).expect("png file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(pixels))
        .expect("write png");
}

/// Floor points: beside the box's sunlit face (x = 0, facing the sun), and far from the box.
const NEAR_FACE: [f32; 3] = [-0.2, 0.0, 0.0];
const FAR_FLOOR: [f32; 3] = [-1.6, 0.0, 1.4];
/// A point on the box's top (not a receiver).
const BOX_TOP: [f32; 3] = [0.5, 1.0, 0.0];

#[test]
fn the_baked_bounce_reaches_the_floor_and_the_box_is_unchanged() {
    let device = device();
    let map = Arc::new(bake().0);
    let plain = render(&device, &map, false);
    let lit = render(&device, &map, true);
    let (near0, far0) = (rgb_at(&plain, NEAR_FACE), rgb_at(&plain, FAR_FLOOR));
    let (near1, far1) = (rgb_at(&lit, NEAR_FACE), rgb_at(&lit, FAR_FLOOR));
    eprintln!("plain near {near0:?} far {far0:?}; lit near {near1:?} far {far1:?}");
    evidence("plain", &plain);
    evidence("lit", &lit);
    assert!(
        far0[1] > 20.0 && far1[1] > 20.0,
        "the floor is lit: {far0:?} {far1:?}"
    );
    assert!(
        (redness(near0) - redness(far0)).abs() < 0.08,
        "the flat ambient has no red bounce: {near0:?} vs {far0:?}"
    );
    assert!(
        redness(near1) > redness(far1) + 0.1,
        "the baked bounce turns the floor beside the red face red: {near1:?} vs {far1:?}"
    );
    assert!(redness(far1) < 1.1, "the far floor stays grey: {far1:?}");
    assert_eq!(
        rgb_at(&lit, BOX_TOP),
        rgb_at(&plain, BOX_TOP),
        "the box is not a receiver"
    );
}

/// The guards, then the comparison with the blessed image `name` (`crates/regia_test_utils/goldens/`).
/// A missing image is a failure, not a pass (ADR-3211); `REGIA_BLESS=1` writes it instead, on the
/// lavapipe reference, and the owner approves the image before it is committed. The real-GPU
/// non-golden pass (`REGIA_SKIP_GOLDENS=1`) compares nothing, so it needs no image either.
fn compare_golden(lit: &[u8], name: &str) {
    guard_golden(
        lit,
        W,
        H,
        name,
        GoldenGuard {
            min_lum: 20.0,
            max_lum: 220.0,
            max_white_pct: 5.0,
            max_black_pct: 40.0,
            min_object_pct: 30.0,
        },
    );
    let blessed = golden_path(name);
    let blessing = std::env::var_os("REGIA_BLESS").is_some_and(|v| v == "1");
    assert!(
        blessing || goldens_skipped() || blessed.exists(),
        "golden {name}: no reference image at {} (bless with REGIA_BLESS=1 on the lavapipe reference once the owner approves it)",
        blessed.display()
    );
    compare_rgba8_to_png(lit, W, H, &blessed, DEFAULT_SSIM_THRESHOLD)
        .unwrap_or_else(|err| panic!("golden {name}: {err}"));
}

/// The lit frame, on the full shader and on the Mobile Low lite shader (ADR-2107). Blessed with
/// `REGIA_BLESS=1 cargo test -p regia_pt --test lightmap_forward_quad_box` on the lavapipe reference.
#[test]
fn lightmap_forward_quad_box_golden() {
    let device = device();
    let map = Arc::new(bake().0);
    let lit = render_tier(&device, &map, true, None, GOLDEN_CAMERA);
    assert!(
        lit == render_tier(&device, &map, true, None, GOLDEN_CAMERA),
        "the lit frame is deterministic"
    );
    compare_golden(&lit, "lightmap_forward_quad_box");
    let lite = render_tier(&device, &map, true, Some(ForwardLite::Low), GOLDEN_CAMERA);
    assert!(lite != lit, "the lite frame draws the lite shader");
    compare_golden(&lite, "lightmap_forward_quad_box_lite_low");
}

/// A flat window's mean luma may differ by at most this many code values between the Mobile Low
/// lite frame and the full shader's frame. Measured on lavapipe: 0.11 at most (ADR-5945); the lite
/// shader before ADR-4930's energy correction drew up to 1.87.
const TIER_FLAT_STEP: f32 = 0.5;

/// ADR-5945: the Mobile Low lite frame against the full shader's frame of the same view, both
/// drawn here, so no reference image stands between the two tiers. The golden
/// `lightmap_forward_quad_box_lite_low` only says the lite frame is the frame someone approved; this
/// says the lite shader still draws the full shader's light. On this view they differ where the
/// profile says they may (the sun's shadow edge: one hardware tap against the 3 × 3 kernel) and
/// nowhere else:
///
/// - every flat 8 × 8 window has the same mean brightness within [`TIER_FLAT_STEP`] code values (the
///   floor's direct and baked light, the box's faces);
/// - the pair scores the golden bar on the harness's own measure (on a hardware GPU its sanity
///   floor, as a golden compare does there).
#[test]
fn the_lite_low_frame_agrees_with_the_full_frame() {
    let device = device();
    let map = Arc::new(bake().0);
    let full = render_frame(&device, &map, &TierFrame::golden(None));
    let lite = render_frame(&device, &map, &TierFrame::golden(Some(ForwardLite::Low)));
    assert!(lite != full, "the lite frame draws the lite shader");
    let agreement = tier_agreement(&lite, &full, W, H);
    eprintln!("Mobile Low lite against the full shader: {agreement:?}");
    assert!(
        agreement.flat * 2 >= agreement.windows,
        "most of the view is flat floor and box faces: {agreement:?}"
    );
    assert!(
        agreement.max_step <= TIER_FLAT_STEP,
        "the lite shader draws the full shader's light on every flat window: {agreement:?}"
    );
    let bar =
        regia_test_utils::golden::adapter_ssim_threshold(&device.adapter.get_info(), DEFAULT_SSIM_THRESHOLD);
    assert!(
        agreement.ssim >= bar,
        "the lite frame scores the golden bar {bar} against the full shader's: {agreement:?}"
    );
}

/// A flat window's mean luma may differ by at most this many code values between a lite tier and
/// the full shader on any receiver material of [`floors`]. Measured on lavapipe since ADR-5945:
/// 0.85 at most (the rough alloy at Mobile Low, whose reflection reads no direction). Before it
/// the metals were 5.6 to 11.2 darker (no multi-scatter share) and the occluded grey floor 4.1
/// (the occlusion as it is, where the full shader takes the multi-bounce form).
const TIER_MATERIAL_STEP: f32 = 1.5;

/// ADR-5945: each lite tier against the full shader on every receiver material of [`floors`], the
/// authored ones with the multi-scatter and specular occlusion an authored `.material.ron` turns
/// on by default. The lite shader's receiver term had no multi-scatter share (the full shader
/// reserves it from the diffuse share and lights it by the bake), so a metal receiver, whose baked
/// light is all reflection, drew as little as half of the full shader's; and it multiplied the
/// baked diffuse by the occlusion map as it is, where the full shader takes the multi-bounce form.
/// Every test and golden before this one used bare materials without an occlusion map, where
/// both are nothing.
#[test]
fn the_lite_tiers_draw_the_full_shaders_baked_light_on_every_material() {
    let device = device();
    let map = Arc::new(bake().0);
    let mut misses = Vec::new();
    for floor in floors() {
        let full = render_frame(&device, &map, &TierFrame::with_floor(None, &floor));
        for (tier, lite) in [("Low", ForwardLite::Low), ("High", ForwardLite::High)] {
            let frame = render_frame(&device, &map, &TierFrame::with_floor(Some(lite), &floor));
            let agreement = tier_agreement(&frame, &full, W, H);
            let held = lite == ForwardLite::High || floor.low_reads_the_full_field();
            eprintln!("{}, Mobile {tier} (held {held}): {agreement:?}", floor.name);
            if !held {
                continue;
            }
            // A smooth metal's sun highlight leaves about a third of the view flat; a quarter is
            // enough to say what the baked light does.
            if agreement.flat * 4 < agreement.windows || agreement.max_step > TIER_MATERIAL_STEP {
                misses.push(format!("{}, Mobile {tier}: {agreement:?}", floor.name));
            }
        }
    }
    assert!(
        misses.is_empty(),
        "a lite tier's flat windows are within {TIER_MATERIAL_STEP} code values of the full shader's on every receiver material: {misses:#?}"
    );
}

/// Where [`the_receiver_specular_only_adds_light_to_the_golden_frames`] writes its strips (golden |
/// specular off | on, binary PPM) when set: the before/after plates of the goldens this ADR moves.
const SPECULAR_PLATES_ENV: &str = "REGIA_LIGHTMAP_SPECULAR_PLATES";

/// ADR-4762 on the two golden frames, on whatever adapter runs it: the floor (the receiver) only gains
/// light from its indirect specular (no pixel darker than a rounding step, the mean brighter), the box
/// (not a receiver) is the same. Prints each frame's SSIM against the blessed golden, specular off and
/// on, so the lead sees how much of a golden's move is this ADR's; writes plates with
/// `REGIA_LIGHTMAP_SPECULAR_PLATES=<dir>`.
#[test]
fn the_receiver_specular_only_adds_light_to_the_golden_frames() {
    let device = device();
    let map = Arc::new(bake().0);
    for (lite, name) in [
        (None, "lightmap_forward_quad_box"),
        (Some(ForwardLite::Low), "lightmap_forward_quad_box_lite_low"),
    ] {
        let off = render_tier_specular(&device, &map, true, lite, GOLDEN_CAMERA, false);
        let on = render_tier_specular(&device, &map, true, lite, GOLDEN_CAMERA, true);
        let mut darker = 0usize;
        let (mut sum_off, mut sum_on) = (0u64, 0u64);
        for (a, b) in off.as_chunks::<4>().0.iter().zip(on.as_chunks::<4>().0) {
            for (&a, &b) in a[..3].iter().zip(&b[..3]) {
                darker += usize::from(b.saturating_add(1) < a);
                sum_off += u64::from(a);
                sum_on += u64::from(b);
            }
        }
        assert_eq!(darker, 0, "{name}: the specular only adds light");
        assert!(
            sum_on > sum_off,
            "{name}: the floor gains its specular ({sum_on} vs {sum_off})"
        );
        let golden = regia_test_utils::golden::read_png_rgba8(&golden_path(name))
            .filter(|(_, w, h)| (*w, *h) == (W, H))
            .map(|(rgba, _, _)| rgba);
        if let Some(golden) = &golden {
            let ssim =
                |frame: &[u8]| regia_test_utils::golden::ssim_rgba8(frame, golden, W, H).unwrap_or(f64::NAN);
            eprintln!(
                "{name}: SSIM against the golden, specular off {:.4}, on {:.4}",
                ssim(&off),
                ssim(&on)
            );
        }
        if let Some(dir) = regia_core::brand::env_var_os(SPECULAR_PLATES_ENV) {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).expect("create the plate directory");
            let blank = vec![0u8; on.len()];
            let planes = [
                golden.as_deref().unwrap_or(&blank[..]),
                off.as_slice(),
                on.as_slice(),
            ];
            let mut ppm = format!("P6\n{} {}\n255\n", W * 3, H).into_bytes();
            for y in 0..H as usize {
                for p in planes {
                    let row = &p[y * W as usize * 4..(y + 1) * W as usize * 4];
                    for px in row.as_chunks::<4>().0 {
                        ppm.extend_from_slice(&px[..3]);
                    }
                }
            }
            regia_test_utils::write_atomic(&dir.join(format!("{name}_specular.ppm")), &ppm)
                .expect("write the plate");
        }
    }
}
