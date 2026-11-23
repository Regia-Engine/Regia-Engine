// SPDX-License-Identifier: MPL-2.0
//! The v0.5 materials and post look pack (burn-down rows A14, B12, B13, C04, C07, C08, C09): one
//! scene with a fixed camera, sun and exposure holds every shipped material feature side by side,
//! so the features are checked together, not only each alone, and the owner gets plates that
//! differ in one setting at a time.
//!
//! The scene: a ground, a striped back wall, an emissive lamp and six spheres in a row. Sphere 0 is
//! the plain reference; spheres 1 to 5 carry thin film, fabric sheen, subsurface scattering, rough
//! transmission and a blended (order-independent transparency) material. Each test switches one
//! thing and compares frames of this file with each other, so no golden image is involved
//! (lavapipe or any adapter).
//!
//! What it holds:
//! - each material feature moves only its own sphere's pixels and reports that it drew;
//! - subsurface scattering keeps the light it moves (its sphere's HDR sum);
//! - the whole pack is the same image in reverse draw order;
//! - two lenses of equal EV100 (aperture, shutter and ISO traded off) give the same frame, a
//!   manual lens at that EV100 gives it too, and a stop more exposure darkens the frame;
//! - a color table moves the frame's balance the way the table says;
//! - flare and dirt only add light;
//! - under every render preset, the forced lite shader and 4x MSAA, with the pack's knobs on, every
//!   feature draws or names why it did not, and the steady frames after the first build no pipeline
//!   and no pooled target.
//!
//! The ignored `write_the_look_pack_plates` writes the plates and the tier table for the by-eye
//! review into `REGIA_LOOK_PACK_DIR`.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use regia_core::glam::Vec3;
use regia_render::forward_lite::ForwardLite;
use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    ALPHA_BLEND, ALPHA_PREMULTIPLIED, AntiAliasing, CameraExposureMode, ColorLut, DirectionalLightUniform,
    FabricSheen, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene, LensEffects,
    MaterialShading, OitStats, PhysicalCamera, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SubsurfaceFactors, SubsurfaceProfile, SubsurfaceStats, ThinFilmFactors,
    ThinFilmStats, box_mesh, translate_mesh, uv_sphere,
};

/// Every test here renders on one device and reads the process-wide pipeline counter.
static SERIAL_GPU: Mutex<()> = Mutex::new(());

/// Size of the frames the assertions read.
const W: u32 = 256;
const H: u32 = 128;
/// Size of the plates for the by-eye review.
const PLATE_W: u32 = 960;
const PLATE_H: u32 = 480;

const EYE: [f32; 3] = [0.0, 0.9, 6.2];
const TARGET: [f32; 3] = [0.0, 0.45, 0.0];
const FOV_Y: f32 = 0.6;
const NEAR: f32 = 0.1;
const FAR: f32 = 60.0;
/// The pack's fixed exposure, set through a manual lens.
const EV100: f32 = 14.0;
const SUN_DIR: [f32; 3] = [0.35, -0.75, -0.55];
const SUN_LUX: f32 = 100_000.0;

const RADIUS: f32 = 0.38;
const SPHERE_Y: f32 = RADIUS;
const SPHERE_X: [f32; 6] = [-2.25, -1.35, -0.45, 0.45, 1.35, 2.25];
/// Pixels around a sphere's projected box that still count as its own (filter footprints).
const MARGIN: i32 = 3;

/// Material keys are fixed so the sheen side table can name sphere 2's material.
const MATERIAL_KEY_BASE: u64 = 7_000;
/// Index of the first sphere material in [`LabScene::materials`].
const FIRST_SPHERE_MATERIAL: usize = 5;

const FILM: ThinFilmFactors = ThinFilmFactors {
    factor: 1.0,
    ior: 1.33,
    thickness_min: 120.0,
    thickness_max: 420.0,
};
const SHEEN: FabricSheen = FabricSheen {
    color: [1.0, 0.6, 0.7],
    roughness: 0.4,
};

/// The material features of the pack, one per sphere after the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feature {
    ThinFilm,
    Sheen,
    Subsurface,
    RoughTransmission,
    Oit,
}

const FEATURES: [Feature; 5] = [
    Feature::ThinFilm,
    Feature::Sheen,
    Feature::Subsurface,
    Feature::RoughTransmission,
    Feature::Oit,
];

impl Feature {
    /// The sphere (0..6) that carries the feature.
    fn sphere(self) -> usize {
        match self {
            Self::ThinFilm => 1,
            Self::Sheen => 2,
            Self::Subsurface => 3,
            Self::RoughTransmission => 4,
            Self::Oit => 5,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ThinFilm => "thin_film",
            Self::Sheen => "sheen",
            Self::Subsurface => "subsurface",
            Self::RoughTransmission => "rough_transmission",
            Self::Oit => "oit",
        }
    }
}

/// Which features are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Toggles {
    thin_film: bool,
    sheen: bool,
    subsurface: bool,
    rough_transmission: bool,
    oit: bool,
}

impl Toggles {
    const NONE: Self = Self {
        thin_film: false,
        sheen: false,
        subsurface: false,
        rough_transmission: false,
        oit: false,
    };
    const ALL: Self = Self {
        thin_film: true,
        sheen: true,
        subsurface: true,
        rough_transmission: true,
        oit: true,
    };

    fn only(feature: Feature) -> Self {
        let mut t = Self::NONE;
        match feature {
            Feature::ThinFilm => t.thin_film = true,
            Feature::Sheen => t.sheen = true,
            Feature::Subsurface => t.subsurface = true,
            Feature::RoughTransmission => t.rough_transmission = true,
            Feature::Oit => t.oit = true,
        }
        t
    }
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The pack's post stack: deterministic (no bloom, no auto exposure), exposed by a manual lens.
fn pack_post() -> PostSettings {
    let mut post = PostSettings::golden_deterministic();
    PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: EV100 },
        ..PhysicalCamera::default()
    }
    .apply_exposure(&mut post);
    post
}

fn matte(color: [f32; 3], roughness: f32) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        roughness,
        ..LabMaterial::EMPTY
    }
}

/// The sphere materials for `t`, reference first.
fn sphere_materials(t: Toggles) -> [LabMaterial; 6] {
    let reference = matte([0.7, 0.3, 0.12], 0.45);
    let film = LabMaterial {
        thin_film: if t.thin_film { FILM } else { ThinFilmFactors::OFF },
        ..matte([0.02, 0.02, 0.03], 0.22)
    };
    // The sheen lives in the renderer's side table ([`shoot`]); the material is plain velvet.
    let velvet = matte([0.30, 0.04, 0.08], 0.85);
    let skin = LabMaterial {
        subsurface: if t.subsurface {
            SubsurfaceFactors::new(SubsurfaceProfile::Skin)
        } else {
            SubsurfaceFactors::OFF
        },
        ..matte([0.8, 0.55, 0.45], 0.5)
    };
    let glass = LabMaterial {
        alpha_mode: ALPHA_PREMULTIPLIED,
        transmission: 1.0,
        ior: 1.5,
        transmission_thickness: 0.5,
        transmission_roughness: if t.rough_transmission { 0.45 } else { 0.0 },
        ..matte([1.0, 1.0, 1.0], 0.05)
    };
    let tinted = LabMaterial {
        base_color: [0.15, 0.45, 0.9, 0.45],
        alpha_mode: ALPHA_BLEND,
        roughness: 0.3,
        ..LabMaterial::EMPTY
    };
    [reference, film, velvet, skin, glass, tinted]
}

/// The pack at `width` × `height` with the material features of `t` and the post stack `post`.
fn pack_lab(width: u32, height: u32, t: Toggles, post: PostSettings) -> LabScene {
    let (sv, si) = uv_sphere(RADIUS, 48, 24);
    let (gv, gi) = box_mesh([7.0, 0.1, 4.0]);
    let (wv, wi) = box_mesh([7.0, 0.8, 0.1]);
    let (bv, bi) = box_mesh([7.0, 0.25, 0.12]);
    let (lv, li) = uv_sphere(0.08, 16, 8);
    let mesh = |vertices, indices, material| LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    };
    let mut meshes = vec![
        mesh(translate_mesh(&gv, [0.0, -0.05, 0.0]), gi, 0),
        mesh(translate_mesh(&wv, [0.0, 0.8, -1.4]), wi.clone(), 1),
        // Two colored bands on the wall, so refraction and blending have edges to show.
        mesh(translate_mesh(&bv, [0.0, 0.55, -1.33]), bi.clone(), 2),
        mesh(translate_mesh(&bv, [0.0, 1.05, -1.33]), bi, 3),
        mesh(translate_mesh(&lv, [-1.8, 1.45, -0.9]), li, 4),
    ];
    for (i, x) in SPHERE_X.iter().enumerate() {
        meshes.push(mesh(
            translate_mesh(&sv, [*x, SPHERE_Y, 0.0]),
            si.clone(),
            FIRST_SPHERE_MATERIAL + i,
        ));
    }
    let mut materials = vec![
        matte([0.45, 0.45, 0.45], 0.8),
        matte([0.85, 0.85, 0.82], 0.7),
        matte([0.75, 0.12, 0.08], 0.6),
        matte([0.08, 0.15, 0.7], 0.6),
        LabMaterial {
            base_color: [0.0, 0.0, 0.0, 1.0],
            emissive: [1.0, 0.92, 0.8],
            emissive_intensity: 400_000.0,
            ..LabMaterial::EMPTY
        },
    ];
    materials.extend(sphere_materials(t));
    LabScene {
        width,
        height,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV100,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new(SUN_DIR, [1.0, 0.97, 0.92], SUN_LUX),
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// The lab quality with the pack's knobs: refraction always (the glass is glass in every frame),
/// scattering and order-independent transparency as `t` says.
fn pack_quality(t: Toggles) -> FrameQuality {
    FrameQuality {
        screen_space_refraction: true,
        subsurface_scattering: t.subsurface,
        order_independent_transparency: t.oit,
        ..FrameQuality::LAB
    }
}

/// The frame scene of `lab` under `quality`, with the pack's fixed material keys.
fn frame_scene(lab: &LabScene, quality: FrameQuality) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    assert_eq!(scene.meshes.len(), lab.meshes.len());
    for (mesh, lab_mesh) in scene.meshes.iter_mut().zip(&lab.meshes) {
        mesh.material_key = MATERIAL_KEY_BASE + lab_mesh.material as u64;
    }
    scene
}

fn sheen_key() -> u64 {
    MATERIAL_KEY_BASE + (FIRST_SPHERE_MATERIAL + Feature::Sheen.sphere()) as u64
}

/// One rendered frame and what the renderer said about it.
struct Shot {
    pixels: Vec<u8>,
    hdr: Option<Vec<[f32; 3]>>,
    oit: OitStats,
    subsurface: SubsurfaceStats,
    thin_film: ThinFilmStats,
    /// Pipelines and pooled targets created by the frames after the first `warm` frames.
    steady_pipelines: u64,
    steady_targets: u64,
}

/// Renders `scene` `frames` times on a new renderer (sheen in its side table when `sheen`, the lite
/// forward shader forced to `lite`) and returns the last frame. The first `warm` frames may build;
/// the later ones are counted.
fn shoot_scene(
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    sheen: bool,
    lite: Option<ForwardLite>,
    warm: usize,
    frames: usize,
) -> Shot {
    assert!(warm >= 1 && frames > warm);
    let mut renderer = FrameRenderer::new();
    // A tile GPU's choice, forced so a desktop adapter takes the lite shader too.
    renderer.set_forward_lite(lite);
    // As the runtime renderer does: the pass records only on views whose post settings ask for it.
    assert!(renderer.enable_lens_effects());
    if sheen {
        renderer.set_material_shading([(sheen_key(), MaterialShading::sheen(SHEEN))]);
    }
    let mut pipelines = 0;
    let mut targets = 0;
    for i in 0..frames - 1 {
        if i == warm {
            pipelines = pipelines_created();
            targets = renderer.graph_report().graph.transients.created;
        }
        renderer.render_offscreen(device, scene).expect("frame");
    }
    if frames - 1 == warm {
        pipelines = pipelines_created();
        targets = renderer.graph_report().graph.transients.created;
    }
    renderer.request_hdr_capture();
    let pixels = renderer.render_offscreen(device, scene).expect("frame").pixels;
    let steady_pipelines = pipelines_created() - pipelines;
    let steady_targets = renderer.graph_report().graph.transients.created - targets;
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("hdr readback")
        .map(|c| c.rgb);
    Shot {
        pixels,
        hdr,
        oit: renderer.oit_stats(),
        subsurface: renderer.subsurface_stats(),
        thin_film: renderer.thin_film_stats(),
        steady_pipelines,
        steady_targets,
    }
}

/// The pack with material features `t` and post `post` at the assertion size, lab quality.
fn shoot(device: &RenderDevice, t: Toggles, post: PostSettings) -> Shot {
    let lab = pack_lab(W, H, t, post);
    shoot_scene(device, &frame_scene(&lab, pack_quality(t)), t.sheen, None, 1, 2)
}

/// Screen box of sphere `i` at `width` × `height`: `[x0, y0, x1, y1]`, inclusive, with [`MARGIN`].
fn sphere_box(i: usize, width: u32, height: u32) -> [i32; 4] {
    // A right-handed camera looking from `EYE` at `TARGET`, y up, with the frame's vertical field of
    // view: view-space x and y over the depth, scaled to the frame's half extents.
    let eye = Vec3::from(EYE);
    let forward = (Vec3::from(TARGET) - eye).normalize();
    let right = forward.cross(Vec3::Y).normalize();
    let up = right.cross(forward);
    let half_h = (FOV_Y * 0.5).tan();
    let half_w = half_h * width as f32 / height as f32;
    let centre = Vec3::new(SPHERE_X[i], SPHERE_Y, 0.0);
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for corner in 0..8 {
        let offset = Vec3::new(
            if corner & 1 == 0 { -RADIUS } else { RADIUS },
            if corner & 2 == 0 { -RADIUS } else { RADIUS },
            if corner & 4 == 0 { -RADIUS } else { RADIUS },
        );
        let d = centre + offset - eye;
        let depth = d.dot(forward);
        assert!(depth > NEAR, "sphere {i} is in front of the camera");
        let ndc_x = d.dot(right) / (depth * half_w);
        let ndc_y = d.dot(up) / (depth * half_h);
        let px = (ndc_x * 0.5 + 0.5) * width as f32;
        let py = (0.5 - ndc_y * 0.5) * height as f32;
        x0 = x0.min(px);
        y0 = y0.min(py);
        x1 = x1.max(px);
        y1 = y1.max(py);
    }
    [
        x0.floor() as i32 - MARGIN,
        y0.floor() as i32 - MARGIN,
        x1.ceil() as i32 + MARGIN,
        y1.ceil() as i32 + MARGIN,
    ]
}

fn inside(b: [i32; 4], x: i32, y: i32) -> bool {
    (b[0]..=b[2]).contains(&x) && (b[1]..=b[3]).contains(&y)
}

/// Pixels that differ inside and outside box `b` (display bytes, `W` × `H`).
fn changed(a: &[u8], b_pixels: &[u8], b: [i32; 4]) -> (usize, usize) {
    let (mut inner, mut outer) = (0, 0);
    let (pa, _) = a.as_chunks::<4>();
    let (pb, _) = b_pixels.as_chunks::<4>();
    for (i, (p, q)) in pa.iter().zip(pb).enumerate() {
        if p != q {
            let (x, y) = ((i as u32 % W) as i32, (i as u32 / W) as i32);
            if inside(b, x, y) {
                inner += 1;
            } else {
                outer += 1;
            }
        }
    }
    (inner, outer)
}

fn luma(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

fn mean_channels(pixels: &[u8]) -> [f64; 3] {
    let (chunks, _) = pixels.as_chunks::<4>();
    let mut sum = [0.0; 3];
    for p in chunks {
        for (s, v) in sum.iter_mut().zip(p) {
            *s += f64::from(*v);
        }
    }
    sum.map(|s| s / chunks.len() as f64)
}

fn mean_luma(pixels: &[u8]) -> f64 {
    let [r, g, b] = mean_channels(pixels);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Each material feature switched on alone changes only its own sphere's pixels, changes them
/// (thin film, sheen, scattering, frosted glass), and is counted as drawn by the pass that owns it.
#[test]
fn each_feature_moves_only_its_own_sphere() {
    let _gpu = serial();
    let device = device();
    let base = shoot(&device, Toggles::NONE, pack_post());
    assert_eq!(base.oit.reason, Some("profile_off"), "{:?}", base.oit);
    assert_eq!(base.thin_film.drawn, 0, "{:?}", base.thin_film);
    assert_eq!(base.subsurface.drawn, 0, "{:?}", base.subsurface);
    for feature in FEATURES {
        let on = shoot(&device, Toggles::only(feature), pack_post());
        let b = sphere_box(feature.sphere(), W, H);
        let (inner, outer) = changed(&base.pixels, &on.pixels, b);
        eprintln!(
            "{}: {inner} pixels changed on its sphere, {outer} elsewhere",
            feature.label()
        );
        assert_eq!(
            outer,
            0,
            "{} changed {outer} pixels off its sphere",
            feature.label()
        );
        match feature {
            Feature::ThinFilm => {
                let s = on.thin_film;
                assert_eq!((s.drawn, s.fallback, s.unsupported), (1, 0, 0), "{s:?}");
            }
            Feature::Subsurface => {
                let s = on.subsurface;
                assert_eq!((s.reason, s.drawn, s.fallback), (None, 1, 0), "{s:?}");
            }
            Feature::Oit => {
                let s = on.oit;
                assert_eq!((s.reason, s.drawn, s.fallback), (None, 1, 0), "{s:?}");
            }
            Feature::Sheen | Feature::RoughTransmission => {}
        }
        // A single blended layer composites as sorted blending does (ADR-2065), so the OIT sphere
        // may keep its bytes; every other feature must show.
        if feature != Feature::Oit {
            assert!(
                inner > 30,
                "{} changed only {inner} pixels of its sphere",
                feature.label()
            );
        }
    }
}

/// Scattering moves the diffuse light inside the skin sphere and keeps it: the sphere's HDR sum
/// stays within 5 % of the unscattered frame's.
#[test]
fn scattering_keeps_the_light_of_its_sphere() {
    let _gpu = serial();
    let device = device();
    let off = shoot(&device, Toggles::NONE, pack_post());
    let on = shoot(&device, Toggles::only(Feature::Subsurface), pack_post());
    let (Some(off_hdr), Some(on_hdr)) = (off.hdr, on.hdr) else {
        panic!("the lab frame runs the post stack, so the HDR copy exists");
    };
    let b = sphere_box(Feature::Subsurface.sphere(), W, H);
    let sum = |hdr: &[[f32; 3]]| {
        hdr.iter()
            .enumerate()
            .filter(|(i, _)| inside(b, (*i as u32 % W) as i32, (*i as u32 / W) as i32))
            .map(|(_, c)| luma(*c))
            .sum::<f64>()
    };
    let (before, after) = (sum(&off_hdr), sum(&on_hdr));
    eprintln!("skin sphere HDR sum: {before:.1} unscattered, {after:.1} scattered");
    assert!(before > 0.0);
    assert!(
        ((after - before) / before).abs() < 0.05,
        "scattering changed its sphere's light by {:.2} %",
        100.0 * (after - before) / before
    );
}

/// With every feature on, the pack is the same image whatever order its meshes are submitted in:
/// opaque surfaces by depth, glass by its sorted run, the blended sphere by the composite.
#[test]
fn the_full_pack_does_not_depend_on_the_draw_order() {
    let _gpu = serial();
    let device = device();
    let lab = pack_lab(W, H, Toggles::ALL, pack_post());
    let forward = frame_scene(&lab, pack_quality(Toggles::ALL));
    let mut reversed = frame_scene(&lab, pack_quality(Toggles::ALL));
    reversed.meshes.reverse();
    let a = shoot_scene(&device, &forward, true, None, 1, 2);
    let b = shoot_scene(&device, &reversed, true, None, 1, 2);
    assert_eq!(a.oit.drawn, 1, "{:?}", a.oit);
    assert_eq!(b.oit.drawn, 1, "{:?}", b.oit);
    let worst = max_abs_diff(&a.pixels, &b.pixels);
    assert_eq!(
        worst, 0,
        "the reversed draw order moved a pixel by {worst} levels"
    );
}

fn lens_post(lens: PhysicalCamera) -> PostSettings {
    let mut post = PostSettings::golden_deterministic();
    lens.apply_exposure(&mut post);
    post
}

/// The exposure triangle on a frame (B13): f/2.8, 1/60 s, ISO 100; f/5.6 at ISO 400; and 1/30 s at
/// ISO 50 are the same EV100, so they are the same picture (to one level of rounding), and a manual
/// lens pinned to that EV100 is too. One stop more EV100 is a darker frame.
#[test]
fn equal_exposures_are_the_same_frame_and_a_stop_darkens_it() {
    let _gpu = serial();
    let device = device();
    let physical = |aperture_f_stops: f32, shutter_seconds: f32, iso: f32| PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        aperture_f_stops,
        shutter_seconds,
        iso,
        ..PhysicalCamera::default()
    };
    let a = physical(2.8, 1.0 / 60.0, 100.0);
    let lenses = [
        a,
        physical(5.6, 1.0 / 60.0, 400.0),
        physical(2.8, 1.0 / 30.0, 50.0),
    ];
    for lens in &lenses[1..] {
        assert!(
            (lens.ev100() - a.ev100()).abs() < 1.0e-4,
            "{} vs {}",
            lens.ev100(),
            a.ev100()
        );
    }
    let first = shoot(&device, Toggles::NONE, lens_post(a)).pixels;
    for lens in &lenses[1..] {
        let other = shoot(&device, Toggles::NONE, lens_post(*lens)).pixels;
        let worst = max_abs_diff(&first, &other);
        assert!(worst <= 1, "{lens:?}: {worst} levels from the f/2.8 frame");
    }
    let manual = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: a.ev100() },
        ..PhysicalCamera::default()
    };
    let pinned = shoot(&device, Toggles::NONE, lens_post(manual)).pixels;
    let worst = max_abs_diff(&first, &pinned);
    assert!(worst <= 1, "manual at the same EV100: {worst} levels");

    let mut last = f64::MAX;
    for ev100 in [13.0, 14.0, 15.0] {
        let lens = PhysicalCamera {
            exposure: CameraExposureMode::Manual { ev100 },
            ..PhysicalCamera::default()
        };
        let mean = mean_luma(&shoot(&device, Toggles::NONE, lens_post(lens)).pixels);
        eprintln!("EV100 {ev100}: mean display luma {mean:.2}");
        assert!(mean + 2.0 < last, "EV100 {ev100}: {mean} after {last}");
        last = mean;
    }
}

fn warm_lut() -> Arc<ColorLut> {
    Arc::new(ColorLut::from_fn(17, |c| {
        [(c[0] * 1.25).min(1.0), c[1], c[2] * 0.75]
    }))
}

fn cool_lut() -> Arc<ColorLut> {
    Arc::new(ColorLut::from_fn(17, |c| {
        [c[0] * 0.75, c[1], (c[2] * 1.25).min(1.0)]
    }))
}

fn with_lut(lut: Option<Arc<ColorLut>>) -> PostSettings {
    PostSettings { lut, ..pack_post() }
}

/// A warm table raises the frame's red against its blue and a cool one lowers it (C04).
#[test]
fn a_color_table_moves_the_balance_its_way() {
    let _gpu = serial();
    let device = device();
    let balance = |post| {
        let [r, _, b] = mean_channels(&shoot(&device, Toggles::ALL, post).pixels);
        r - b
    };
    let none = balance(with_lut(None));
    let warm = balance(with_lut(Some(warm_lut())));
    let cool = balance(with_lut(Some(cool_lut())));
    eprintln!("mean red minus blue: cool {cool:.2}, none {none:.2}, warm {warm:.2}");
    assert!(warm > none + 2.0 && none > cool + 2.0, "{cool} {none} {warm}");
}

const FLARE_AND_DIRT: LensEffects = LensEffects {
    flare_intensity: 1.0,
    flare_threshold: 1.0,
    coating: 1.0,
    dirt_intensity: 0.5,
    chromatic_aberration: 0.0,
    ..LensEffects::OFF
};

/// Flare and dirt over the full pack only add light (C07): no HDR pixel gets darker beyond the
/// half-float rounding of the sum, and the lamp casts something.
#[test]
fn flare_and_dirt_only_add_light_to_the_pack() {
    let _gpu = serial();
    let device = device();
    let off = shoot(&device, Toggles::ALL, pack_post());
    let on = shoot(
        &device,
        Toggles::ALL,
        PostSettings {
            lens_effects: FLARE_AND_DIRT,
            ..pack_post()
        },
    );
    let (Some(off), Some(on)) = (off.hdr, on.hdr) else {
        panic!("the lab frame runs the post stack, so the HDR copy exists");
    };
    let mut darker = 0;
    let mut added = 0.0;
    for (a, b) in off.iter().zip(&on) {
        let (a, b) = (luma(*a), luma(*b));
        if b < a * (1.0 - 1.0e-3) - 1.0e-6 {
            darker += 1;
        }
        added += (b - a).max(0.0);
    }
    eprintln!("lens: {added:.1} added, {darker} darker pixels");
    assert_eq!(darker, 0, "{darker} pixels got darker under flare and dirt");
    assert!(added > 0.0, "the lamp cast no flare");
}

/// One tier of the table: a preset with the pack's knobs on, optionally with the lite forward shader
/// forced (what a tile GPU takes for the Mobile presets) or with 4x MSAA.
#[derive(Clone, Copy)]
struct Tier {
    label: &'static str,
    preset: RenderProfilePreset,
    lite: Option<ForwardLite>,
    msaa: bool,
}

const fn tier(label: &'static str, preset: RenderProfilePreset) -> Tier {
    Tier {
        label,
        preset,
        lite: None,
        msaa: false,
    }
}

const TIERS: [Tier; 10] = [
    tier("mobile_low", RenderProfilePreset::MobileLow),
    tier("mobile_high", RenderProfilePreset::MobileHigh),
    tier("desktop_low", RenderProfilePreset::DesktopLow),
    tier("desktop_high", RenderProfilePreset::DesktopHigh),
    tier("desktop_ultra", RenderProfilePreset::DesktopUltra),
    tier("deck", RenderProfilePreset::Deck),
    tier("cinematic", RenderProfilePreset::Cinematic),
    Tier {
        lite: Some(ForwardLite::Low),
        ..tier("mobile_low_tile_gpu", RenderProfilePreset::MobileLow)
    },
    Tier {
        lite: Some(ForwardLite::High),
        ..tier("mobile_high_tile_gpu", RenderProfilePreset::MobileHigh)
    },
    Tier {
        msaa: true,
        ..tier("desktop_high_msaa4x", RenderProfilePreset::DesktopHigh)
    },
];

/// One row of the tier table: what each pass did with the pack under one tier.
struct TierRow {
    tier: Tier,
    shot: Shot,
}

/// The pack's knobs on top of `tier`'s preset.
fn tier_quality(tier: Tier) -> FrameQuality {
    let base = RenderProfile::from_preset(tier.preset);
    let profile = RenderProfile {
        subsurface_scattering: true,
        order_independent_transparency: true,
        screen_space_refraction: true,
        lens_effects: true,
        anti_aliasing: if tier.msaa {
            AntiAliasing::Msaa4x
        } else {
            base.anti_aliasing
        },
        ..base
    };
    FrameQuality::from_profile(&profile)
}

/// The full pack under each tier at `width` × `height`: 4 frames that may build, then 3 counted.
fn tier_rows(device: &RenderDevice, width: u32, height: u32) -> Vec<TierRow> {
    TIERS
        .iter()
        .map(|&tier| {
            let lab = pack_lab(width, height, Toggles::ALL, pack_post());
            let scene = frame_scene(&lab, tier_quality(tier));
            TierRow {
                tier,
                shot: shoot_scene(device, &scene, true, tier.lite, 4, 7),
            }
        })
        .collect()
}

/// The per-draw reasons scattering skipped, `code:count`, comma separated (`-` for none).
fn unsupported_codes(stats: &SubsurfaceStats) -> String {
    let codes: Vec<String> = stats
        .unsupported_reasons()
        .map(|(reason, count)| format!("{}:{count}", reason.code()))
        .collect();
    if codes.is_empty() {
        "-".to_owned()
    } else {
        codes.join(",")
    }
}

fn tier_table(rows: &[TierRow]) -> String {
    let mut out = String::from(
        "| tier | oit | subsurface | thin film | steady pipelines | steady targets | hdr finite |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for row in rows {
        let s = &row.shot;
        let finite = s.hdr.as_ref().map_or_else(
            || "no hdr copy".to_owned(),
            |h| h.iter().all(|c| c.iter().all(|v| v.is_finite())).to_string(),
        );
        let _ = writeln!(
            out,
            "| {} | drawn {} fallback {} sorted {} reason {} | drawn {} fallback {} unsupported {} ({}) reason {} | drawn {} fallback {} unsupported {} | {} | {} | {} |",
            row.tier.label,
            s.oit.drawn,
            s.oit.fallback,
            s.oit.sorted,
            s.oit.reason.unwrap_or("-"),
            s.subsurface.drawn,
            s.subsurface.fallback,
            s.subsurface.unsupported,
            unsupported_codes(&s.subsurface),
            s.subsurface.reason.unwrap_or("-"),
            s.thin_film.drawn,
            s.thin_film.fallback,
            s.thin_film.unsupported,
            s.steady_pipelines,
            s.steady_targets,
            finite,
        );
    }
    out
}

/// Under every tier with the pack's knobs on, each feature draws or names why not, nothing is still
/// building after the first frames, the HDR image is finite, and the steady frames build no
/// pipeline and no pooled target (C08, C09, A14 tier fallback and warmed-frame invariants).
#[test]
fn every_tier_draws_the_pack_or_names_what_it_skips() {
    let _gpu = serial();
    let device = device();
    let rows = tier_rows(&device, W, H);
    eprintln!("{}", tier_table(&rows));
    for row in &rows {
        let (p, s) = (row.tier.label, &row.shot);
        // Order-independent transparency: the blended sphere composites, or the frame names why it
        // drew it sorted; "still building" is not a reason on a steady frame.
        assert_ne!(s.oit.reason, Some("pipelines_building"), "{p}: {:?}", s.oit);
        assert!(
            (s.oit.drawn == 1 && s.oit.reason.is_none() && s.oit.fallback == 0)
                || (s.oit.drawn == 0 && s.oit.reason.is_some()),
            "{p}: {:?}",
            s.oit
        );
        // Scattering: the skin sphere scatters, or the frame names why it drew it plain.
        assert_ne!(
            s.subsurface.reason,
            Some("pipelines_building"),
            "{p}: {:?}",
            s.subsurface
        );
        assert!(
            (s.subsurface.drawn == 1 && s.subsurface.reason.is_none() && s.subsurface.fallback == 0)
                || (s.subsurface.drawn == 0
                    && (s.subsurface.reason.is_some() || s.subsurface.unsupported > 0)),
            "{p}: {:?}",
            s.subsurface
        );
        // Thin film: drawn with the film or counted as unsupported (the lite opaque path), never
        // left waiting for its variant.
        assert_eq!(s.thin_film.fallback, 0, "{p}: {:?}", s.thin_film);
        assert_eq!(
            s.thin_film.drawn + s.thin_film.unsupported,
            1,
            "{p}: {:?}",
            s.thin_film
        );
        if let Some(hdr) = &s.hdr {
            assert!(
                hdr.iter().all(|c| c.iter().all(|v| v.is_finite())),
                "{p}: a non-finite HDR pixel"
            );
        }
        assert_eq!(s.steady_pipelines, 0, "{p}: steady frames built pipelines");
        assert_eq!(s.steady_targets, 0, "{p}: steady frames created pooled targets");
    }
}

/// On a lite forward frame (a tile GPU under a Mobile preset) the skin sphere is counted unsupported
/// with the per-draw code `forward_lite`, and the frame names the tier as its reason, not
/// `no_material`: a scattering material is on screen (found by this pack; it read `no_material`).
#[test]
fn a_lite_frame_names_forward_lite_for_its_scattering_draw() {
    let _gpu = serial();
    let device = device();
    let lite = TIERS[7];
    assert_eq!(lite.lite, Some(ForwardLite::Low));
    let lab = pack_lab(W, H, Toggles::ALL, pack_post());
    let shot = shoot_scene(
        &device,
        &frame_scene(&lab, tier_quality(lite)),
        true,
        lite.lite,
        1,
        2,
    );
    assert_eq!(shot.subsurface.unsupported, 1, "{:?}", shot.subsurface);
    assert_eq!(
        shot.subsurface.reason,
        Some("forward_lite"),
        "{:?}",
        shot.subsurface
    );
}

/// Writes `rgba` as a PNG at `path` (atomically: a temporary file, then a rename).
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
        writer.write_image_data(rgba).map_err(std::io::Error::other)?;
        writer.finish().map_err(std::io::Error::other)?;
    }
    std::fs::rename(&tmp, path)
}

fn write_text(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The plates for the by-eye review: the same camera, sun and EV100 14 in every plate, one setting
/// changed at a time, plus the tier table. Set `REGIA_LOOK_PACK_DIR`.
#[test]
#[ignore = "writes plates for the by-eye review; needs REGIA_LOOK_PACK_DIR"]
fn write_the_look_pack_plates() {
    let Some(dir) = std::env::var_os("REGIA_LOOK_PACK_DIR") else {
        eprintln!("REGIA_LOOK_PACK_DIR is not set; nothing written");
        return;
    };
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).expect("create the plate directory");
    let _gpu = serial();
    let device = device();
    let plate = |t: Toggles, post: PostSettings| {
        let lab = pack_lab(PLATE_W, PLATE_H, t, post);
        shoot_scene(&device, &frame_scene(&lab, pack_quality(t)), t.sheen, None, 1, 2).pixels
    };
    let mut plates: Vec<(String, String, Vec<u8>)> = Vec::new();
    plates.push((
        "00_all_off".into(),
        "every material feature off".into(),
        plate(Toggles::NONE, pack_post()),
    ));
    for (n, feature) in FEATURES.iter().enumerate() {
        plates.push((
            format!("{:02}_{}_only", n + 1, feature.label()),
            format!("only {} on (sphere {})", feature.label(), feature.sphere()),
            plate(Toggles::only(*feature), pack_post()),
        ));
    }
    plates.push((
        "06_all_on".into(),
        "every material feature on".into(),
        plate(Toggles::ALL, pack_post()),
    ));
    plates.push((
        "07_lut_warm".into(),
        "all on, warm 17³ color table".into(),
        plate(Toggles::ALL, with_lut(Some(warm_lut()))),
    ));
    plates.push((
        "08_lut_cool".into(),
        "all on, cool 17³ color table".into(),
        plate(Toggles::ALL, with_lut(Some(cool_lut()))),
    ));
    plates.push((
        "09_lens_flare_dirt".into(),
        "all on, flare 1.0 threshold 1.0 coating 1.0 dirt 0.5".into(),
        plate(
            Toggles::ALL,
            PostSettings {
                lens_effects: FLARE_AND_DIRT,
                ..pack_post()
            },
        ),
    ));
    // The pack's lamp is small, so at strength 1 its ghosts barely read; this plate shows the
    // enabled look at strengths a small source needs (both inside their ranges).
    plates.push((
        "09b_lens_flare_dirt_strong".into(),
        "all on, flare 60 threshold 1.0 coating 1.0 dirt 2.0".into(),
        plate(
            Toggles::ALL,
            PostSettings {
                lens_effects: LensEffects {
                    flare_intensity: 60.0,
                    dirt_intensity: 2.0,
                    ..FLARE_AND_DIRT
                },
                ..pack_post()
            },
        ),
    ));
    plates.push((
        "10_lens_chromatic_aberration".into(),
        "all on, chromatic aberration 0.01".into(),
        plate(
            Toggles::ALL,
            PostSettings {
                lens_effects: LensEffects {
                    chromatic_aberration: 0.01,
                    ..LensEffects::OFF
                },
                ..pack_post()
            },
        ),
    ));
    for (n, ev100) in [(11, 13.0_f32), (12, 15.0)] {
        plates.push((
            format!("{n}_ev100_{ev100}"),
            format!("all on, manual EV100 {ev100} (the rest of the pack is 14)"),
            plate(
                Toggles::ALL,
                lens_post(PhysicalCamera {
                    exposure: CameraExposureMode::Manual { ev100 },
                    ..PhysicalCamera::default()
                }),
            ),
        ));
    }
    let mut index = format!(
        "# Materials look pack\n\nEvery plate: {PLATE_W}x{PLATE_H}, eye {EYE:?}, target {TARGET:?}, fov_y {FOV_Y} rad, \
         sun {SUN_LUX} lux along {SUN_DIR:?}, procedural sky IBL, AgX, no bloom, no auto exposure, manual EV100 {EV100} \
         unless the plate says otherwise. Spheres left to right: 0 reference, 1 thin film, 2 sheen, 3 skin subsurface, \
         4 glass (rough transmission 0.45 when on, clear when off), 5 blended blue (order-independent transparency when \
         on, sorted blending when off). Lab quality with screen-space refraction on.\n\n| plate | setting |\n|---|---|\n"
    );
    for (name, setting, pixels) in &plates {
        write_png(&dir.join(format!("{name}.png")), PLATE_W, PLATE_H, pixels).expect("write plate");
        let _ = writeln!(index, "| {name}.png | {setting} |");
    }
    let rows = tier_rows(&device, PLATE_W, PLATE_H);
    for row in &rows {
        let name = format!("tier_{}", row.tier.label);
        write_png(
            &dir.join(format!("{name}.png")),
            PLATE_W,
            PLATE_H,
            &row.shot.pixels,
        )
        .expect("write tier plate");
        let _ = writeln!(
            index,
            "| {name}.png | all on, {} tier with the pack's knobs on |",
            row.tier.label
        );
    }
    let _ = write!(index, "\n## Tier table (plate size)\n\n{}", tier_table(&rows));
    write_text(&dir.join("index.md"), &index).expect("write the index");
}
