// SPDX-License-Identifier: MPL-2.0
//! Lightmaps against the path tracer: the v0.3.0 definition-of-done evidence (`docs/master-plan.md`
//! § 3.3, `specs/27` § 2.2 GI rows, `docs/evidence/lightmaps-v030/`).
//!
//! Two scenes from the standard GI set (`regia_test_utils::gi_scenes`), every mesh a lightmap
//! receiver laid out by the shipped UV atlas generator (`build_atlas_layout`, the default
//! `regia bake-lightmaps` unwrap):
//!
//! - `lightmap_room`: the colour room (an interior open at the front, red and green walls, the sun
//!   through the opening) with a sphere on the floor, so the chart-seam metric has a smooth
//!   surface cut in UV2 to measure;
//! - `sealed_box`: the § 2.2 leak case, a closed box with the camera inside.
//!
//! Per scene the path tracer renders the reference (1024 spp, 6 bounces, its AOVs). The frame
//! renders at Desktop High with the lightmap baked by the CPU baker (and, with the
//! `lightmap-gpu` feature, by the GPU baker) on every mesh, and on the Mobile Low lite shader. The
//! scores are the `seeing.compare` ones (`regia_pt::compare::compare_images`: FLIP, SSIM, RMSE)
//! per AOV, PSNR included (`beauty`, `direct`, and the indirect light `indirect_diffuse + indirect_specular`
//! against the frame minus its direct-only frame, as `gi_validation` measures every GI
//! technique), and the masked § 2.2 numbers
//! (`regia_test_utils::gi_error::measure`). The flat ambient is scored beside them as the baseline.
//!
//! What is asserted (deterministic: fixed seeds, fixed samples, no clocks):
//!
//! - every receiver draws with the lightmap (`lightmap_forward_stats().drawn`);
//! - the lightmapped room is closer to the tracer than the flat ambient on the indirect light;
//! - the sealed box keeps the § 2.2 leak bound (≤ 0.5 % of the lit exterior ground);
//! - the sphere's chart seams are measured, and baked again at 2, 4 and 8 times the density they
//!   fall under the shipping default budget (at the room's density they do not: the finding of
//!   ADR-2315);
//! - with `lightmap-gpu`, the GPU bake is within `LIGHTMAP_GPU_PSNR_FLOOR_DB` of the CPU bake;
//! - a new bake (the CLI's defaults since ADR-2404: the denoiser, the crease placement and the seam
//!   stitch) has a lower room indirect FLIP than the v0.3.0 evidence's bake (the 3×3 filter, the
//!   `lightmap_cpu_base` row, 0.120) by at least [`FLIP_GAIN_NEW_BAKE`], the stitch lowers the
//!   sphere's seams, and the sphere laid out by the automatic density rule
//!   (`regia_pt::lightmap::density`) is under the shipping seam budget.
//!
//! The `lightmap_cpu_frame_ssao_off` row draws the new bake with the frame's SSAO off: a diagnosis
//! (ADR-2404), since SSAO darkens the creases on top of the occlusion the lightmap already holds.
//!
//! Receivers over 400 m² (the sealed box's exterior ground, never in view) stay on the ambient.
//! The lite rows subtract the full shader's direct-only frame, the realtime reference for direct
//! light: a lite receiver's sun and shadows are scored with its indirect light, so a direct-light
//! defect of the lite shader shows in its row (ADR-2403). The lite shader's own direct-only frame
//! (every mesh on the CPU colour loop, where the lite shader draws) is scored as `direct_lite_low`.
//! The interior is baked a second time at 1024 samples, to show the bake noise's share of the
//! error.
//!
//! The lite `LIGHTMAP` rules (ADR-2403), asserted on the room: at lightmap intensity 0 a lite
//! receiver adds no light over the lite shader's own direct-only frame (the full shader's rule:
//! at most `LITE_OWN_OTHER_LIGHT_SHARE` of the full shader's lightmap term) and little over the
//! full shader's (`LITE_OTHER_LIGHT_SHARE`), the lite row's indirect energy lands in
//! `LITE_ENERGY_BAND`, and the lite shader's direct light is the full shader's within
//! `LITE_DIRECT_ENERGY_TOLERANCE`.
//!
//! The room asserts the original indirect FLIP mean ≤ 0.08 and comparison p95 ≤ 0.20. With
//! `REGIA_LIGHTMAP_EVIDENCE_DIR` set the plates (PNG) and the numbers (`<scene>.json`, `<scene>.md`)
//! are written there. Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use regia_asset::lightmap::{DEFAULT_HARD_EDGE_DEG, Lightmap, LightmapLayout, SeamGeometry, seam_report};
use regia_asset_pipeline::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_atlas_layout};
use regia_pt::compare::{LoadedImage, compare_images, psnr_db};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::lightmap::{
    LightmapBakeReport, LightmapBakeSettings, LightmapComparison, LightmapScene, bake_lightmap,
    compare_lightmaps,
};
use regia_pt::{PathTracer, PtImage, PtScene, PtSettings};
use regia_render::compare::{DEFAULT_PPD, display_encode, flip_heatmap_rgba8};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapReceiverMesh, LightmapSettings, receiver_mesh};
use regia_render::{
    FrameQuality, FrameRenderer, FrameScene, GiMode, LabMesh, LabScene, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode, uv_sphere,
};
use regia_test_utils::gi_error::{
    GiError, GiErrorInput, add_planes, measure, shadow_mask, sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box};
use serde_json::{Value, json};

/// Where the plates and numbers go when set.
const EVIDENCE_ENV: &str = "REGIA_LIGHTMAP_EVIDENCE_DIR";
/// § 2.2 GI (diffuse) mean FLIP target.
const FLIP_TARGET: f32 = 0.08;
/// § 2.2 leak target: interior luminance over the lit exterior ground's.
const LEAK_TARGET: f32 = 0.005;
/// The shipping package's default seam budget (`[lightmaps] seam_budget`, `book/agents/lightmaps.md`).
const SHIPPING_SEAM_BUDGET: f32 = 0.25;
/// Frames per still capture (no technique here keeps a history).
const STILL_FRAMES: u32 = 2;
/// Hemisphere samples per texel of the evidence bakes (the CLI default).
const SAMPLES: u32 = 256;
/// The room's extra bake, to show how much of its error is bake noise.
const SAMPLES_HIGH: u32 = 1024;
/// The sphere's density multipliers of the seam sweep (`LightmapSurface::density_scale`).
const SEAM_SWEEP: [f32; 3] = [2.0, 4.0, 8.0];
/// Lightmap texel density of the evidence bakes, texels per metre (twice the CLI default: the
/// rooms are 2.4 m across).
const DENSITY: f32 = 16.0;
/// The room's indirect FLIP mean of the bake before ADR-2404 (the v0.3.0 evidence, ADR-2315).
const FLIP_BEFORE_ADR_2404: f32 = 0.120;
/// The least drop of the room's indirect FLIP mean a new bake must give against the v0.3.0 bake.
/// ADR-2404 measured 0.0048 with the frame's SSAO over the receivers; since ADR-2635 the SSAO
/// leaves a receiver's baked diffuse term alone, both bakes lose the SSAO's crease darkening, and the
/// gap is 0.0009 (0.1093 against 0.1102, lavapipe 2026-11-04): the denoiser mostly smoothed what
/// the SSAO doubled. [`FLIP_AFTER_ADR_2635`] holds the absolute figure instead.
const FLIP_GAIN_NEW_BAKE: f32 = 0.0005;
/// The room's indirect FLIP mean a new bake must reach since ADR-2635 (0.1093 measured; 0.1149
/// with the SSAO over the receivers).
const FLIP_AFTER_ADR_2635: f32 = 0.112;
/// How far the lightmapped frame may sit from the same frame with SSAO off (ADR-2635: the SSAO
/// leaves the baked diffuse term alone, so only the specular's occlusion differs; 0.0000 measured).
const SSAO_OFF_FLIP_TOLERANCE: f32 = 0.002;
/// The most light a lite receiver may add at lightmap intensity 0 over the lite shader's own
/// direct-only frame, as a share of the full shader's lightmap term: the full shader's rule (it adds
/// 0.0 over its own). Measured 0.0 (lavapipe, 2026-11-03); the allowance is float noise only.
const LITE_OWN_OTHER_LIGHT_SHARE: f64 = 0.005;
/// The same over the full shader's direct-only frame: what the lite sun (one hardware 2x2 tap, the
/// lite BRDF fit) adds over the full shader's filtered sun. Measured 53.5 cd/m², 3.2 % of the
/// room's 1651 cd/m² lightmap term; before ADR-2403 135 % (2229 cd/m²: the sun went unshadowed past
/// cascade 0).
const LITE_OTHER_LIGHT_SHARE: f64 = 0.05;
/// Where the lite row's indirect energy must land. The full shader's is 0.98; the lite lookup is
/// one bilinear fetch without directions (2.4 % above the full shader's bicubic, directional term
/// in the room) and the lite sun's edges move a few pixels, so ±0.1 around 1. Before ADR-2403: 2.33.
const LITE_ENERGY_BAND: (f32, f32) = (0.9, 1.1);
/// How far the lite shader's direct-only energy may sit from the full shader's (both against the
/// tracer's direct light). Measured 1.020 against 0.996 (the same 2.4 % as above); before
/// ADR-2403 the lite shader's own direct frame was about twice the tracer's.
const LITE_DIRECT_ENERGY_TOLERANCE: f32 = 0.04;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| {
        RenderDevice::headless().expect("the lightmap evidence needs a GPU; lavapipe is a GPU here")
    })
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// Desktop High as `gi_validation` runs it: native scale, no temporal upscaling, fog, clouds or
/// reflections, and the flat ambient as the GI (the lightmap replaces it on receivers).
fn base_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

/// The colour room with a sphere on its floor (a smooth receiver whose UV2 has seams).
fn lightmap_room() -> GiScene {
    let mut scene = color_room();
    scene.name = "lightmap_room";
    let (vertices, indices) = uv_sphere(0.35, 32, 16);
    scene.lab.meshes.push(LabMesh {
        vertices,
        indices,
        material: 0,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.45, 0.35, -0.45, 1.0],
        ],
        ..LabMesh::unskinned()
    });
    scene
}

fn reference(scene: &GiScene) -> (PtScene, PtImage) {
    reference_seeded(scene, 23)
}

/// The reference at another seed (the tracer's own noise: two seeds against each other).
fn reference_seeded(scene: &GiScene, seed: u32) -> (PtScene, PtImage) {
    let frame = scene.lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(&frame, &ExtractOptions::default());
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: 1024,
                max_bounces: 6,
                seed,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    assert_eq!((image.width, image.height), (GI_WIDTH, GI_HEIGHT));
    (pt, image)
}

/// The scene flattened for the baker and the atlas over every receiving instance (entry `e` is
/// instance `receivers[e]`, named `instance_<k>` as `regia bake-lightmaps` names them).
struct Laid {
    flat: LightmapScene,
    layout: LightmapLayout,
    receivers: Vec<usize>,
}

/// Receivers larger than this stay on the flat ambient: the sealed box's 60 m exterior ground,
/// which its camera never sees, would hold nine tenths of the texels.
const MAX_RECEIVER_AREA_M2: f64 = 400.0;

fn area(positions: &[[f32; 3]], indices: &[u32]) -> f64 {
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let p = |k: usize| regia_core::glam::Vec3::from_array(positions[t[k] as usize]);
            f64::from(0.5 * (p(1) - p(0)).cross(p(2) - p(0)).length())
        })
        .sum()
}

/// Lays out every receiver at [`DENSITY`], the instance `scaled.0` at `scaled.1` times it.
fn lay_out(pt: &PtScene, scaled: Option<(usize, f32)>) -> Laid {
    lay_out_with(pt, |k, _| match scaled {
        Some((i, f)) if i == k => f,
        _ => 1.0,
    })
}

/// Lays out every receiver at [`DENSITY`] with the automatic density rule of `regia bake-lightmaps`
/// (ADR-2404: the stitch on, the shipping seam budget).
fn lay_out_auto(pt: &PtScene) -> Laid {
    lay_out_with(pt, |_, inst| {
        regia_pt::lightmap::density::auto_density_scale(
            &inst.positions,
            &inst.indices,
            DENSITY,
            SHIPPING_SEAM_BUDGET,
            true,
        )
        .density_scale
    })
}

/// Lays out every receiver at [`DENSITY`] times `scale(instance, geometry)`.
fn lay_out_with(pt: &PtScene, scale: impl Fn(usize, &regia_pt::lightmap::InstanceGeometry) -> f32) -> Laid {
    let flat = LightmapScene::from_pt_scene(pt).expect("flatten the scene");
    let receivers: Vec<usize> = flat
        .instances()
        .iter()
        .enumerate()
        .filter(|(_, i)| {
            i.receiver && !i.indices.is_empty() && area(&i.positions, &i.indices) <= MAX_RECEIVER_AREA_M2
        })
        .map(|(k, _)| k)
        .collect();
    let names: Vec<String> = receivers.iter().map(|k| format!("instance_{k}")).collect();
    let inputs: Vec<UnwrapInput<'_>> = receivers
        .iter()
        .zip(&names)
        .map(|(&k, name)| {
            let inst = &flat.instances()[k];
            UnwrapInput {
                name,
                positions: &inst.positions,
                indices: &inst.indices,
                density_scale: scale(k, inst),
            }
        })
        .collect();
    let settings = LightmapUvSettings {
        texels_per_metre: DENSITY,
        padding: 2,
        max_atlas: 2048,
        ..LightmapUvSettings::default()
    };
    let (layout, _) = build_atlas_layout(&inputs, &settings, 2).expect("lay out the atlas");
    Laid {
        flat,
        layout,
        receivers,
    }
}

/// The evidence bake: a new bake's CLI defaults (sky and the sun's bounces; the sun stays live,
/// directions on, the à-trous denoiser of ADR-2314, the crease placement and the seam stitch of
/// ADR-2404) with the reference's path length.
fn bake_settings() -> LightmapBakeSettings {
    LightmapBakeSettings {
        crease_placement: true,
        stitch_seams: true,
        denoise: Some(regia_asset::lightmap::LightmapDenoiseInfo::default()),
        ..base_settings()
    }
}

/// The v0.3.0 evidence bake (ADR-2315): the 3×3 filter, no denoiser, no crease placement, no
/// stitch.
fn base_settings() -> LightmapBakeSettings {
    LightmapBakeSettings {
        samples: SAMPLES,
        bounces: 6,
        seed: 1,
        threads: 0,
        ..LightmapBakeSettings::default()
    }
}

fn bake_cpu(laid: &Laid, settings: &LightmapBakeSettings) -> (Lightmap, LightmapBakeReport) {
    bake_lightmap(
        &laid.flat,
        laid.layout.clone(),
        &laid.receivers,
        settings,
        |done, total| assert!(done <= total),
    )
    .expect("bake on the CPU")
}

#[cfg(feature = "lightmap-gpu")]
fn bake_gpu(laid: &Laid) -> (Lightmap, LightmapBakeReport) {
    regia_pt::lightmap::bake_lightmap_gpu(
        device(),
        &laid.flat,
        laid.layout.clone(),
        &laid.receivers,
        &bake_settings(),
        |_, _| {},
    )
    .expect("bake on the GPU")
}

/// The receiver geometry of every entry, from the frame's own meshes (instance `k` is frame
/// mesh `k`: the extraction keeps the order).
fn receiver_meshes(lab: &LabScene, map: &Lightmap, receivers: &[usize]) -> Vec<LightmapReceiverMesh> {
    receivers
        .iter()
        .enumerate()
        .map(|(e, &k)| {
            let m = &lab.meshes[k];
            receiver_mesh(map, e, &m.vertices, &m.indices)
                .unwrap_or_else(|err| panic!("entry {e} (mesh {k}): {err}"))
        })
        .collect()
}

/// `frame` with every receiver drawn from its entry's unwrap and UV2.
fn with_receivers<'a>(
    mut frame: FrameScene<'a>,
    receivers: &[usize],
    meshes: &'a [LightmapReceiverMesh],
) -> (FrameScene<'a>, Vec<(u64, u32)>) {
    let mut keys = Vec::new();
    for (e, (&k, rx)) in receivers.iter().zip(meshes).enumerate() {
        let fm = &mut frame.meshes[k];
        fm.vertices = &rx.vertices;
        fm.indices = &rx.indices;
        fm.uv1 = &rx.uv2;
        // Other vertices than the source mesh: another upload key.
        fm.mesh_key ^= 0x11A7_0000_0000_0000;
        keys.push((fm.object_key, e as u32));
    }
    (frame, keys)
}

/// The last of `STILL_FRAMES` frames of `frame` as linear HDR.
fn capture(renderer: &mut FrameRenderer, frame: &FrameScene<'_>) -> Vec<[f32; 3]> {
    for i in 0..STILL_FRAMES {
        if i + 1 == STILL_FRAMES {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device(), frame).expect("frame");
    }
    let hdr = renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (GI_WIDTH, GI_HEIGHT));
    hdr.rgb
}

/// The frame's direct light alone: the sun with its shadows, no sky, no GI, no AO, no lightmap,
/// on the full shader or the `lite` one. The lite shader draws from the CPU colour loop only (a
/// GPU-driven batch keeps the full shader), so the lite frame leaves the GPU-driven path, as the
/// Mobile profiles do; a lite receiver is on that loop anyway (ADR-2107).
fn realtime_direct(scene: &GiScene, lite: Option<ForwardLite>) -> Vec<[f32; 3]> {
    let mut lab = scene.lab.clone();
    lab.ambient_lux = 0.0;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        gpu_driven: lite.is_none() && base_quality().gpu_driven,
        ..base_quality()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(lite);
    capture(&mut renderer, &lab.as_frame_scene_with_quality(quality))
}

/// The full frame with the flat ambient (no lightmap).
fn realtime_ambient(scene: &GiScene) -> Vec<[f32; 3]> {
    capture(
        &mut FrameRenderer::new(),
        &scene.lab.as_frame_scene_with_quality(base_quality()),
    )
}

/// The full frame with `map` on every receiver, on the full shader or the `lite` one.
fn realtime_lightmapped(
    scene: &GiScene,
    map: &Arc<Lightmap>,
    receivers: &[usize],
    lite: Option<ForwardLite>,
) -> Vec<[f32; 3]> {
    realtime_lightmapped_at(scene, map, receivers, lite, 1.0)
}

/// [`realtime_lightmapped`] at lightmap `intensity` (0 keeps every other term of a receiver).
fn realtime_lightmapped_at(
    scene: &GiScene,
    map: &Arc<Lightmap>,
    receivers: &[usize],
    lite: Option<ForwardLite>,
    intensity: f32,
) -> Vec<[f32; 3]> {
    realtime_lightmapped_with(scene, map, receivers, lite, intensity, base_quality())
}

/// [`realtime_lightmapped_at`] at frame quality `quality`.
fn realtime_lightmapped_with(
    scene: &GiScene,
    map: &Arc<Lightmap>,
    receivers: &[usize],
    lite: Option<ForwardLite>,
    intensity: f32,
    quality: FrameQuality,
) -> Vec<[f32; 3]> {
    realtime_lightmapped_config(
        scene,
        map,
        receivers,
        lite,
        quality,
        LightmapSettings {
            enabled: true,
            intensity,
            ..LightmapSettings::default()
        },
    )
}

fn realtime_lightmapped_config(
    scene: &GiScene,
    map: &Arc<Lightmap>,
    receivers: &[usize],
    lite: Option<ForwardLite>,
    quality: FrameQuality,
    settings: LightmapSettings,
) -> Vec<[f32; 3]> {
    let meshes = receiver_meshes(&scene.lab, map, receivers);
    let (frame, keys) = with_receivers(scene.lab.as_frame_scene_with_quality(quality), receivers, &meshes);
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(lite);
    renderer.set_lightmap(Some(map.clone()), settings);
    renderer.set_lightmap_receivers(&keys);
    let rgb = capture(&mut renderer, &frame);
    let stats = renderer.lightmap_forward_stats();
    assert!(
        stats.active,
        "{}: the lightmap is installed: {stats:?}",
        scene.name
    );
    assert_eq!(
        stats.receivers,
        receivers.len(),
        "{}: every mesh is a receiver: {stats:?}",
        scene.name
    );
    assert_eq!(
        stats.fallback, 0,
        "{}: no receiver fell back to the probes: {stats:?}",
        scene.name
    );
    assert!(
        stats.drawn > 0 && stats.drawn <= receivers.len(),
        "{}: the visible receivers draw with the lightmap: {stats:?}",
        scene.name
    );
    rgb
}

fn linear_image(rgb: &[[f32; 3]]) -> LoadedImage {
    LoadedImage {
        width: GI_WIDTH,
        height: GI_HEIGHT,
        rgb: rgb.to_vec(),
        linear: true,
        denoiser: None,
    }
}

/// One AOV's `seeing.compare` scores (PSNR included) over the surface: outside `mask` (the sky,
/// silhouettes) the test reads the reference, as `gi_error::masked_flip` does, so only surface
/// pixels can differ. The means are over the whole frame.
fn compare_aov(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool], ev100: f32) -> Value {
    let test: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let test = test.as_slice();
    let (report, _) = compare_images(&linear_image(reference), &linear_image(test), ev100, DEFAULT_PPD)
        .expect("compare the AOV");
    json!({
        "flip_mean": report.flip_mean,
        "flip_p95": report.flip_p95,
        "ssim": report.ssim,
        "rel_rmse": if report.rel_rmse.is_finite() { json!(report.rel_rmse) } else { Value::Null },
        "mean_reference": report.mean_reference,
        "mean_test": report.mean_test,
        "psnr_db": psnr_db(&linear_image(reference), &linear_image(test), ev100),
    })
}

fn gi_json(e: &GiError) -> Value {
    json!({
        "pixels": e.pixels,
        "shadow_pixels": e.shadow_pixels,
        "flip_mean": e.flip_mean,
        "rel_rmse": e.rel_rmse,
        "energy_ratio": e.energy_ratio,
        "shadow_bias": e.shadow_bias,
        "mean_reference": e.mean_reference,
        "mean_test": e.mean_test,
        "scale": e.scale,
    })
}

/// sRGB-encodes `img` at `ev100` + 2 EV (indirect light is dim), as `gi_validation`'s review plates.
fn encode_indirect(img: &[[f32; 3]], ev100: f32) -> Vec<u8> {
    let exposure = 4.0 / (1.2 * 2f32.powf(ev100));
    let e = |v: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    img.iter().flat_map(|c| c.map(e)).collect()
}

/// The runtime's display transform as 8-bit RGB.
fn encode_display(img: &[[f32; 3]], ev100: f32) -> Vec<u8> {
    display_encode(img, ev100)
        .iter()
        .flat_map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8))
        .collect()
}

fn evidence_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(regia_core::brand::env_var_os(EVIDENCE_ENV)?);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("{EVIDENCE_ENV}: {e}");
            None
        }
    }
}

/// Writes RGB8 `planes` side by side (each `GI_WIDTH` × `GI_HEIGHT`) as `dir/name`.
fn write_strip(dir: &std::path::Path, name: &str, planes: &[Vec<u8>]) {
    let width = GI_WIDTH * planes.len() as u32;
    let row = (GI_WIDTH * 3) as usize;
    let mut rgb = Vec::with_capacity(row * planes.len() * GI_HEIGHT as usize);
    for y in 0..GI_HEIGHT as usize {
        for p in planes {
            rgb.extend_from_slice(&p[y * row..(y + 1) * row]);
        }
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, GI_HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    match encoder.write_header().and_then(|mut w| w.write_image_data(&rgb)) {
        Ok(()) => {
            if let Err(e) = regia_test_utils::write_atomic(&dir.join(name), &bytes) {
                eprintln!("{EVIDENCE_ENV}: {e}");
            }
        }
        Err(e) => eprintln!("{EVIDENCE_ENV}: {e}"),
    }
}

/// The FLIP map of `test` against `reference` (display-encoded) as RGB8.
fn flip_plate(reference: &[[f32; 3]], test: &[[f32; 3]], ev100: f32) -> Vec<u8> {
    let (_, flip) = compare_images(&linear_image(reference), &linear_image(test), ev100, DEFAULT_PPD)
        .expect("compare for the FLIP plate");
    flip_heatmap_rgba8(&flip.map)
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

/// Seam figures per entry (`regia bake-lightmaps`' `seams`), worst `p95` and `max` over the map.
fn seams_json(map: &Lightmap, laid: &Laid) -> (Value, f32, u32) {
    let mut entries = Vec::new();
    let (mut worst, mut edges) = (0.0f32, 0u32);
    for (e, &k) in laid.receivers.iter().enumerate() {
        let report = seam_report(
            map,
            e,
            SeamGeometry {
                positions: &laid.flat.instances()[k].positions,
            },
            DEFAULT_HARD_EDGE_DEG,
        );
        if let Some(s) = report.seams {
            worst = worst.max(s.p95);
            edges += s.edges;
        }
        entries.push(json!({
            "entry": map.entries[e].name,
            "charts": map.entries[e].charts,
            "hard_edges": report.hard_edges,
            "edges": report.seams.map_or(0, |s| s.edges),
            "mean": report.seams.map(|s| s.mean),
            "p95": report.seams.map(|s| s.p95),
            "max": report.seams.map(|s| s.max),
        }));
    }
    (json!(entries), worst, edges)
}

/// One scored frame: the technique, its full linear frame and its own shader's direct-only frame.
type Row<'a> = (&'static str, Vec<[f32; 3]>, &'a [[f32; 3]]);

/// Everything one scene produced.
struct Evidence {
    /// The masked § 2.2 indirect-light error per technique.
    indirect: Vec<(&'static str, GiError)>,
    /// The original surface-masked comparison's whole-frame 95th percentile, by technique.
    indirect_p95: std::collections::BTreeMap<&'static str, f64>,
    /// Worst seam `p95` and seam edges measured, over the CPU bake.
    seam_p95: f32,
    seam_edges: u32,
    /// The seam sweep: `(density multiplier of the smooth receiver, its worst p95)`, the bake's own
    /// density first; empty when no entry has seams.
    seam_sweep: Vec<(f32, f32)>,
    /// The GPU bake against the CPU bake (`None` without the `lightmap-gpu` feature).
    gpu_vs_cpu: Option<LightmapComparison>,
    /// Worst seam `p95` with every receiver laid out by the automatic density rule (`None` when no
    /// entry has seams).
    seam_p95_auto: Option<f32>,
    /// The worst seam `p95` of the bake before ADR-2404.
    seam_p95_base: f32,
    /// The lightmap term of the full shader on the surface (cd/m², `frame - frame_at_0`).
    full_term: f64,
    /// Light a lite receiver adds at lightmap intensity 0 over the full shader's direct-only frame
    /// and over the lite shader's own (cd/m²).
    lite_rest: (f64, f64),
    /// Direct-only energy against the tracer's direct light: the full shader, the lite shader.
    direct_energy: (Option<f32>, Option<f32>),
}

impl Evidence {
    fn row(&self, technique: &str) -> &GiError {
        &self
            .indirect
            .iter()
            .find(|(t, _)| *t == technique)
            .unwrap_or_else(|| panic!("no `{technique}` row"))
            .1
    }
}

fn run(scene: &GiScene) -> Evidence {
    let ev = scene.lab.exposure_ev;
    let (pt, image) = reference(scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);

    let laid = lay_out(&pt, None);
    let (cpu_map, cpu_report) = bake_cpu(&laid, &bake_settings());
    eprintln!("{}: CPU bake {cpu_report:?}", scene.name);
    let (seams, seam_p95, seam_edges) = seams_json(&cpu_map, &laid);
    // The same bake before ADR-2404 (no crease placement, no stitch).
    let (base_map, base_report) = bake_cpu(&laid, &base_settings());
    let (_, seam_p95_base, _) = seams_json(&base_map, &laid);
    eprintln!(
        "{}: CPU bake before ADR-2404 {base_report:?}; seam p95 {seam_p95_base:.4} before, {seam_p95:.4} with the stitch",
        scene.name
    );
    // The seam sweep: the entry with seams (the room's sphere) baked again at higher densities.
    let mut seam_sweep = Vec::new();
    let smooth = laid.receivers.iter().enumerate().find_map(|(e, &k)| {
        let geometry = SeamGeometry {
            positions: &laid.flat.instances()[k].positions,
        };
        seam_report(&base_map, e, geometry, DEFAULT_HARD_EDGE_DEG)
            .seams
            .map(|s| (k, s.p95))
    });
    let mut seam_p95_auto = None;
    if let Some((k, p95)) = smooth {
        let auto = lay_out_auto(&pt);
        let (map, _) = bake_cpu(&auto, &bake_settings());
        let (_, worst, _) = seams_json(&map, &auto);
        let scale = auto
            .layout
            .entries
            .iter()
            .map(|e| e.texels_per_metre / DENSITY)
            .fold(1.0f32, f32::max);
        eprintln!(
            "{}: the automatic density rule: largest scale {scale}, seam p95 {worst:.4}",
            scene.name
        );
        seam_p95_auto = Some(worst);
        seam_sweep.push((1.0, p95));
        for factor in SEAM_SWEEP {
            // The v0.3.0 bake, as ADR-2315 measured it: the density alone, without the stitch.
            let scaled = lay_out(&pt, Some((k, factor)));
            let (map, _) = bake_cpu(&scaled, &base_settings());
            let (_, worst, _) = seams_json(&map, &scaled);
            eprintln!(
                "{}: instance {k} at {factor}x density: seam p95 {worst:.4}",
                scene.name
            );
            seam_sweep.push((factor, worst));
        }
    }
    // The bake noise's share of the error: the same bake at more samples (the interior only).
    let high = scene.scale.is_none().then(|| {
        let settings = LightmapBakeSettings {
            samples: SAMPLES_HIGH,
            ..bake_settings()
        };
        Arc::new(bake_cpu(&laid, &settings).0)
    });
    let cpu = Arc::new(cpu_map);
    let base = Arc::new(base_map);
    #[cfg(feature = "lightmap-gpu")]
    let gpu = {
        let (map, report) = bake_gpu(&laid);
        eprintln!("{}: GPU bake {report:?}", scene.name);
        Some(Arc::new(map))
    };
    #[cfg(not(feature = "lightmap-gpu"))]
    let gpu: Option<Arc<Lightmap>> = None;

    let direct = realtime_direct(scene, None);
    let lite_direct = realtime_direct(scene, Some(ForwardLite::Low));
    // (technique, full frame, its shader's direct-only frame)
    let mut full: Vec<Row<'_>> = vec![
        ("ambient", realtime_ambient(scene), direct.as_slice()),
        (
            "lightmap_cpu",
            realtime_lightmapped(scene, &cpu, &laid.receivers, None),
            direct.as_slice(),
        ),
        (
            "lightmap_cpu_lite_low",
            realtime_lightmapped(scene, &cpu, &laid.receivers, Some(ForwardLite::Low)),
            direct.as_slice(),
        ),
        (
            "lightmap_cpu_base",
            realtime_lightmapped(scene, &base, &laid.receivers, None),
            direct.as_slice(),
        ),
        (
            "lightmap_cpu_frame_ssao_off",
            realtime_lightmapped_with(
                scene,
                &cpu,
                &laid.receivers,
                None,
                1.0,
                FrameQuality {
                    ssao: SsaoMode::Off,
                    ..base_quality()
                },
            ),
            direct.as_slice(),
        ),
    ];
    if let Some(high) = &high {
        full.push((
            "lightmap_cpu_1024spp",
            realtime_lightmapped(scene, high, &laid.receivers, None),
            direct.as_slice(),
        ));
    }
    if let Some(gpu) = &gpu {
        full.push((
            "lightmap_gpu",
            realtime_lightmapped(scene, gpu, &laid.receivers, None),
            direct.as_slice(),
        ));
    }

    let score = |reference: &[[f32; 3]], test: &[[f32; 3]]| {
        measure(&GiErrorInput {
            reference,
            test,
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: ev,
            scale: scene.scale,
        })
    };
    let mut indirect = Vec::new();
    let mut techniques = serde_json::Map::new();
    let mut indirect_p95 = std::collections::BTreeMap::new();
    for (name, frame, own_direct) in &full {
        let ind = sub_planes(frame, own_direct);
        let gi = score(&reference_indirect, &ind);
        let compared = compare_aov(&reference_indirect, &ind, &surface, ev);
        indirect_p95.insert(*name, compared["flip_p95"].as_f64().expect("comparison p95"));
        techniques.insert(
            (*name).to_owned(),
            json!({
                "indirect_masked": gi_json(&gi),
                "seeing_compare_on_surfaces": {
                    "beauty": compare_aov(&image.beauty, frame, &surface, ev),
                    "indirect": compared,
                },
            }),
        );
        indirect.push((*name, gi));
    }
    // Where the lite rows' light comes from: each shader at lightmap intensity 0 keeps every other
    // term of a receiver, so `frame - frame_at_0` is the lightmap's own term on that shader.
    let surface_mean = |a: &[[f32; 3]], b: &[[f32; 3]]| -> f64 {
        let (sum, n) = a.iter().zip(b).zip(&surface).filter(|(_, m)| **m).fold(
            (0.0f64, 0usize),
            |(s, n), ((x, y), _)| {
                let l = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
                (s + f64::from(l(*x) - l(*y)), n + 1)
            },
        );
        sum / n.max(1) as f64
    };
    let full_at_0 = realtime_lightmapped_at(scene, &cpu, &laid.receivers, None, 0.0);
    let lite_at_0 = realtime_lightmapped_at(scene, &cpu, &laid.receivers, Some(ForwardLite::Low), 0.0);
    let frame_of = |name: &str| {
        full.iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, f, _)| f.as_slice())
            .unwrap_or_else(|| panic!("no `{name}` frame"))
    };
    let full_term = surface_mean(frame_of("lightmap_cpu"), &full_at_0);
    let lite_term = surface_mean(frame_of("lightmap_cpu_lite_low"), &lite_at_0);
    let full_rest = surface_mean(&full_at_0, &direct);
    let lite_rest = surface_mean(&lite_at_0, &direct);
    let lite_rest_own = surface_mean(&lite_at_0, &lite_direct);
    // Preserve the legacy lite row above (it subtracts the full shader's direct frame).
    // This separate diagnostic isolates the lite shader's indirect light without changing
    // any acceptance input, row, assertion or threshold.
    let lite_own_indirect = sub_planes(frame_of("lightmap_cpu_lite_low"), &lite_direct);
    let lite_own_gi = score(&reference_indirect, &lite_own_indirect);
    eprintln!(
        "{}: lite own-direct diagnostic: indirect FLIP {:.6}, energy {:?}",
        scene.name, lite_own_gi.flip_mean, lite_own_gi.energy_ratio
    );
    eprintln!(
        "{}: lightmap term on the surface: full {full_term:.1}, lite {lite_term:.1} (ratio {:.3}); \
         other indirect on receivers: full {full_rest:.1}, lite {lite_rest:.1} (over its own \
         direct-only frame {lite_rest_own:.1})",
        scene.name,
        lite_term / full_term.max(1e-9)
    );
    let lite_diagnosis = json!({
        "lightmap_term_full": full_term,
        "lightmap_term_lite_low": lite_term,
        "ratio_lite_over_full": (full_term > 1e-6).then(|| lite_term / full_term),
        "other_indirect_full": full_rest,
        "other_indirect_lite_low": lite_rest,
        "other_indirect_lite_low_over_its_own_direct": lite_rest_own,
        "own_direct_indirect_masked": gi_json(&lite_own_gi),
        "own_direct_indirect_compare": compare_aov(&reference_indirect, &lite_own_indirect, &surface, ev),
    });
    let direct_gi = score(&image.direct, &direct);
    let lite_direct_gi = score(&image.direct, &lite_direct);
    let gpu_vs_cpu = gpu
        .as_ref()
        .map(|g| compare_lightmaps(&cpu, g).expect("compare the bakes"));
    let info = device().adapter.get_info();
    let adapter = format!("{} ({}; {})", info.name, info.driver, info.driver_info);

    for (name, gi) in &indirect {
        if let Some(scale) = scene.scale {
            let leak = gi.mean_test / scale;
            eprintln!(
                "{} / {name}: leak {:.4} % of the lit ground (§ 2.2 target {:.1} %: {})",
                scene.name,
                leak * 100.0,
                LEAK_TARGET * 100.0,
                if leak <= LEAK_TARGET { "met" } else { "not met" }
            );
        } else {
            eprintln!(
                "{} / {name}: indirect FLIP {:.3} rel RMSE {:.3} energy {:?} (§ 2.2 target {FLIP_TARGET}: {})",
                scene.name,
                gi.flip_mean,
                gi.rel_rmse,
                gi.energy_ratio,
                if gi.flip_mean <= FLIP_TARGET {
                    "met"
                } else {
                    "not met"
                }
            );
        }
    }
    eprintln!(
        "{}: seams {seam_edges} edges, worst p95 {seam_p95:.4}",
        scene.name
    );

    let numbers = json!({
        "scene": scene.name,
        "adapter": adapter,
        "size": [GI_WIDTH, GI_HEIGHT],
        "ev100": ev,
        "reference": { "spp": 1024, "max_bounces": 6, "seed": 23 },
        "bake": {
            "density": DENSITY,
            "samples": bake_settings().samples,
            "bounces": bake_settings().bounces,
            "atlas": [cpu.width, cpu.height],
            "entries": cpu.entries.len(),
            "texels_baked": cpu_report.texels_baked,
            "texels_inside": cpu_report.texels_inside,
            "texels_crease_inside": cpu_report.texels_crease_inside,
            "texels_relocated": cpu_report.texels_relocated,
            "seam_edges_stitched": cpu_report.seam_edges_stitched,
            "texels_stitched": cpu_report.texels_stitched,
            "noise": cpu_report.noise,
        },
        "bake_base": {
            "note": "before ADR-2404: no crease placement, no seam stitch (the lightmap_cpu_base row)",
            "texels_baked": base_report.texels_baked,
            "texels_inside": base_report.texels_inside,
            "seam_worst_p95": seam_p95_base,
        },
        "direct": {
            "masked": gi_json(&direct_gi),
            "seeing_compare_on_surfaces": compare_aov(&image.direct, &direct, &surface, ev),
        },
        "direct_lite_low": {
            "masked": gi_json(&lite_direct_gi),
            "seeing_compare_on_surfaces": compare_aov(&image.direct, &lite_direct, &surface, ev),
        },
        "techniques": techniques,
        "gpu_vs_cpu_lightmap": gpu_vs_cpu,
        "lite_diagnosis": lite_diagnosis,
        "seams": {
            "worst_p95": seam_p95,
            "worst_p95_auto_density": seam_p95_auto,
            "worst_p95_base": seam_p95_base,
            "edges": seam_edges,
            "entries": seams,
            "density_sweep": seam_sweep
                .iter()
                .map(|(f, p)| json!({ "density_scale": f, "texels_per_metre": DENSITY * f, "worst_p95": p }))
                .collect::<Vec<_>>(),
        },
        "targets": {
            "flip_mean": FLIP_TARGET,
            "leak": LEAK_TARGET,
            "seam_budget": SHIPPING_SEAM_BUDGET,
        },
    });
    if let Some(dir) = evidence_dir() {
        let text = serde_json::to_string_pretty(&numbers).unwrap_or_default();
        if let Err(e) =
            regia_test_utils::write_atomic(&dir.join(format!("{}.json", scene.name)), text.as_bytes())
        {
            eprintln!("{EVIDENCE_ENV}: {e}");
        }
        let mut indirect_planes = vec![encode_indirect(&reference_indirect, ev)];
        let mut beauty_planes = vec![encode_display(&image.beauty, ev)];
        for (_, frame, own_direct) in &full {
            indirect_planes.push(encode_indirect(&sub_planes(frame, own_direct), ev));
            beauty_planes.push(encode_display(frame, ev));
        }
        write_strip(&dir, &format!("{}_indirect.png", scene.name), &indirect_planes);
        write_strip(&dir, &format!("{}_beauty.png", scene.name), &beauty_planes);
        let flips: Vec<Vec<u8>> = full
            .iter()
            .map(|(_, frame, own_direct)| flip_plate(&reference_indirect, &sub_planes(frame, own_direct), ev))
            .collect();
        write_strip(&dir, &format!("{}_indirect_flip.png", scene.name), &flips);
        let names: Vec<&str> = full.iter().map(|(n, _, _)| *n).collect();
        let md = format!(
            "# {}\n\nPlates left to right: path tracer, {}.\n\n```json\n{text}\n```\n",
            scene.name,
            names.join(", ")
        );
        if let Err(e) = regia_test_utils::write_atomic(&dir.join(format!("{}.md", scene.name)), md.as_bytes())
        {
            eprintln!("{EVIDENCE_ENV}: {e}");
        }
    }
    Evidence {
        indirect,
        indirect_p95,
        seam_p95,
        seam_edges,
        seam_sweep,
        gpu_vs_cpu,
        seam_p95_auto,
        seam_p95_base,
        full_term,
        lite_rest: (lite_rest, lite_rest_own),
        direct_energy: (direct_gi.energy_ratio, lite_direct_gi.energy_ratio),
    }
}

#[test]
fn lightmapped_room_is_closer_to_the_path_tracer_than_the_ambient() {
    let scene = lightmap_room();
    let ev = run(&scene);
    let ambient = ev.row("ambient");
    let lightmap = ev.row("lightmap_cpu");
    assert!(ambient.pixels > 0 && ambient.pixels == lightmap.pixels);
    assert!(
        lightmap.rel_rmse < ambient.rel_rmse,
        "the lightmap's indirect light is closer to the tracer than the flat ambient: {lightmap:?} vs {ambient:?}"
    );
    assert!(
        lightmap.flip_mean < ambient.flip_mean,
        "and so is its FLIP: {lightmap:?} vs {ambient:?}"
    );
    // The sphere is the room's one smooth receiver: its UV2 cuts are measured. At the room's
    // density its charts are a few texels across and the seams show (p95 0.58 on lavapipe, over
    // the shipping default budget); at a hero density they fall under it (ADR-2315).
    assert!(ev.seam_edges > 0, "the sphere's seams are measured");
    assert_eq!(ev.seam_sweep.len(), SEAM_SWEEP.len() + 1, "{:?}", ev.seam_sweep);
    assert_eq!(ev.seam_sweep[0], (1.0, ev.seam_p95_base));
    let densest = ev.seam_sweep[SEAM_SWEEP.len()];
    assert!(
        densest.1 < SHIPPING_SEAM_BUDGET && densest.1 < ev.seam_p95_base,
        "at {}x density the sphere's seams are under the shipping default {SHIPPING_SEAM_BUDGET}: {:?}",
        densest.0,
        ev.seam_sweep
    );
    // ADR-2404: a new bake is closer to the tracer than the v0.3.0 bake, and below its figure.
    let base = ev.row("lightmap_cpu_base");
    assert!(
        lightmap.flip_mean <= base.flip_mean - FLIP_GAIN_NEW_BAKE
            && lightmap.flip_mean < FLIP_BEFORE_ADR_2404
            && lightmap.flip_mean <= FLIP_AFTER_ADR_2635,
        "indirect FLIP {:.4} for a new bake against {:.4} for the v0.3.0 bake (the gain must be at least \
         {FLIP_GAIN_NEW_BAKE}, the figure at most {FLIP_AFTER_ADR_2635})",
        lightmap.flip_mean,
        base.flip_mean
    );
    // ADR-2635: the frame's SSAO does not darken the baked diffuse term a second time, so the
    // lightmapped frame matches the one drawn with SSAO off.
    let ssao_off = ev.row("lightmap_cpu_frame_ssao_off");
    assert!(
        (lightmap.flip_mean - ssao_off.flip_mean).abs() <= SSAO_OFF_FLIP_TOLERANCE,
        "the SSAO over the receivers moves the indirect FLIP: {:.4} with it, {:.4} without",
        lightmap.flip_mean,
        ssao_off.flip_mean
    );
    assert!(
        ev.seam_p95 < ev.seam_p95_base,
        "the stitch lowers the sphere's seams: {} vs {}",
        ev.seam_p95,
        ev.seam_p95_base
    );
    let auto = ev.seam_p95_auto.expect("the sphere has seams");
    assert!(
        auto <= SHIPPING_SEAM_BUDGET,
        "with the automatic density rule the sphere's seams are under the shipping default: {auto}"
    );
    assert_eq!(
        ev.gpu_vs_cpu.is_some(),
        cfg!(feature = "lightmap-gpu"),
        "the GPU bake runs with the `lightmap-gpu` feature"
    );
    if let Some(c) = ev.gpu_vs_cpu {
        assert!(
            c.within(regia_pt::lightmap::LIGHTMAP_GPU_PSNR_FLOOR_DB),
            "the GPU bake is within the documented floor of the CPU bake: {c:?}"
        );
    }
    assert_lite_rules(&ev);
}

/// The lite `LIGHTMAP` rules of ADR-2403 on the room (see the module docs).
fn assert_lite_rules(ev: &Evidence) {
    assert!(
        ev.full_term > 100.0,
        "the room has a lightmap term: {}",
        ev.full_term
    );
    let (over_full, over_own) = ev.lite_rest;
    assert!(
        over_own.abs() <= LITE_OWN_OTHER_LIGHT_SHARE * ev.full_term,
        "a lite receiver at lightmap intensity 0 adds no light over the lite shader's direct-only \
         frame (the full shader adds 0.0 over its own): {over_own:.1} cd/m² against a lightmap term \
         of {:.1}",
        ev.full_term
    );
    assert!(
        over_full.abs() <= LITE_OTHER_LIGHT_SHARE * ev.full_term,
        "a lite receiver at lightmap intensity 0 adds little over the full shader's direct-only \
         frame: {over_full:.1} cd/m² against a lightmap term of {:.1} (at most {} %)",
        ev.full_term,
        LITE_OTHER_LIGHT_SHARE * 100.0
    );
    let lite = ev.row("lightmap_cpu_lite_low");
    let energy = lite.energy_ratio.unwrap_or(f32::NAN);
    assert!(
        (LITE_ENERGY_BAND.0..=LITE_ENERGY_BAND.1).contains(&energy),
        "the lite row's indirect energy {energy:.3} is in {LITE_ENERGY_BAND:?} (full shader {:?}): {lite:?}",
        ev.row("lightmap_cpu").energy_ratio
    );
    // The release bars are the original spec 27 targets, including the legacy Low row's
    // full-direct subtraction. Printing a miss is insufficient for release acceptance.
    for name in ["lightmap_cpu", "lightmap_cpu_lite_low"] {
        let row = ev.row(name);
        let p95 = ev.indirect_p95[name];
        assert!(
            row.flip_mean <= FLIP_TARGET && p95 <= 0.20,
            "{name}: original mean <= {FLIP_TARGET}, p95 <= 0.20; observed p95 {p95}: {row:?}"
        );
    }
    let (full, lite) = ev.direct_energy;
    let (full, lite) = (full.unwrap_or(f32::NAN), lite.unwrap_or(f32::NAN));
    assert!(
        (full - lite).abs() <= LITE_DIRECT_ENERGY_TOLERANCE,
        "the lite shader's direct light is the full shader's: energy {lite:.3} against {full:.3}"
    );
}

#[test]
fn lightmapped_sealed_box_keeps_the_leak_bound() {
    let scene = sealed_box();
    let ev = run(&scene);
    let scale = scene.scale.expect("the leak scene names its scale");
    for technique in [
        "lightmap_cpu",
        "lightmap_cpu_lite_low",
        "lightmap_cpu_base",
        "lightmap_cpu_frame_ssao_off",
    ] {
        let row = ev.row(technique);
        let leak = row.mean_test / scale;
        assert!(
            leak <= LEAK_TARGET,
            "{technique}: interior light {:.4} % of the lit ground passes the § 2.2 bound {:.1} %: {row:?}",
            leak * 100.0,
            LEAK_TARGET * 100.0
        );
    }
    #[cfg(feature = "lightmap-gpu")]
    {
        let row = ev.row("lightmap_gpu");
        assert!(row.mean_test / scale <= LEAK_TARGET, "lightmap_gpu: {row:?}");
    }
}

/// Hand-run (ADR-2404): the room's indirect FLIP for each bake variant, one trace for all, so the
/// share of each step is measured on the same reference. Prints one line per variant.
///
/// ```sh
/// VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json cargo test -p regia_pt --test lightmap_pt_evidence \
///   -- --ignored crease_stitch_variants --nocapture
/// ```
#[test]
#[ignore = "hand-run: measures the ADR-2404 variants (about as long as the room evidence)"]
fn crease_stitch_variants() {
    let scene = lightmap_room();
    let ev = scene.lab.exposure_ev;
    let (pt, image) = reference(&scene);
    let surface = surface_mask(&image.depth, GI_WIDTH, GI_HEIGHT, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let reference_indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    let direct = realtime_direct(&scene, None);
    let laid = lay_out(&pt, None);
    let denoised = Some(regia_asset::lightmap::LightmapDenoiseInfo::default());
    // The reference's own noise: the same trace at another seed, scored like a technique.
    let (_, other) = reference_seeded(&scene, 24);
    let other_indirect = add_planes(&other.indirect_diffuse, &other.indirect_specular);
    let floor = measure(&GiErrorInput {
        reference: &reference_indirect,
        test: &other_indirect,
        surface: &surface,
        shadow: &shadow,
        width: GI_WIDTH,
        height: GI_HEIGHT,
        ev100: ev,
        scale: scene.scale,
    });
    eprintln!(
        "variant {:28}: indirect FLIP {:.4} rel RMSE {:.4} energy {:?} (the 1024-spp reference at seed 24 against seed 23)",
        "reference noise floor", floor.flip_mean, floor.rel_rmse, floor.energy_ratio
    );
    if let Some(dir) = evidence_dir() {
        write_strip(
            &dir,
            "variant_reference_noise_flip.png",
            &[flip_plate(&reference_indirect, &other_indirect, ev)],
        );
    }
    let variants: Vec<(&str, LightmapBakeSettings)> = vec![
        ("base", base_settings()),
        (
            "crease",
            LightmapBakeSettings {
                crease_placement: true,
                ..base_settings()
            },
        ),
        (
            "stitch",
            LightmapBakeSettings {
                stitch_seams: true,
                ..base_settings()
            },
        ),
        ("crease+stitch", bake_settings()),
        (
            "crease+stitch, no filter",
            LightmapBakeSettings {
                filter: false,
                ..bake_settings()
            },
        ),
        (
            "base, no filter",
            LightmapBakeSettings {
                filter: false,
                ..base_settings()
            },
        ),
        (
            "crease+stitch, denoise",
            LightmapBakeSettings {
                denoise: denoised,
                ..bake_settings()
            },
        ),
        (
            "base, denoise",
            LightmapBakeSettings {
                denoise: denoised,
                ..base_settings()
            },
        ),
    ];
    let no_ssao = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    for (name, settings, ssao) in variants
        .into_iter()
        .flat_map(|(n, s)| [(n, s, true), (n, s, false)])
    {
        let (map, report) = bake_cpu(&laid, &settings);
        let map = Arc::new(map);
        let frame = if ssao {
            realtime_lightmapped(&scene, &map, &laid.receivers, None)
        } else {
            realtime_lightmapped_with(&scene, &map, &laid.receivers, None, 1.0, no_ssao)
        };
        let name = if ssao {
            name.to_string()
        } else {
            format!("{name}, frame SSAO off")
        };
        let ind = sub_planes(&frame, &direct);
        let gi = measure(&GiErrorInput {
            reference: &reference_indirect,
            test: &ind,
            surface: &surface,
            shadow: &shadow,
            width: GI_WIDTH,
            height: GI_HEIGHT,
            ev100: ev,
            scale: scene.scale,
        });
        let (_, seam, _) = seams_json(&map, &laid);
        eprintln!(
            "variant {name:28}: indirect FLIP {:.4} rel RMSE {:.4} energy {:?} seam p95 {seam:.4} \
             (inside {}, crease inside {}, relocated {}, stitched edges {}, texels {})",
            gi.flip_mean,
            gi.rel_rmse,
            gi.energy_ratio,
            report.texels_inside,
            report.texels_crease_inside,
            report.texels_relocated,
            report.seam_edges_stitched,
            report.texels_stitched
        );
        if let Some(dir) = evidence_dir() {
            let flip = flip_plate(&reference_indirect, &ind, ev);
            let plate = encode_indirect(&ind, ev);
            write_strip(
                &dir,
                &format!("variant_{}_flip.png", name.replace([' ', ',', '+'], "_")),
                &[flip, plate],
            );
        }
    }
}

#[path = "lightmap_pt_quality_study/mod.rs"]
mod quality_study;
