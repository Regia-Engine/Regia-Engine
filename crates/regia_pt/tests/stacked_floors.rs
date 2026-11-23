// SPDX-License-Identifier: MPL-2.0
//! Stacked floors (DO27 ask 11, `specs/25` § 7.11 "Stacked floors"): light on one storey of a closed
//! building must not reach the storeys under it, on any light path the engine ships.
//!
//! The scene, built in code like the thin-wall tests: a three-storey block on open ground, 6 × 6 m
//! inside, 0.2 m outer walls, 0.3 m floor slabs on a 3 m storey pitch (2.7 m clear), a 0.3 m roof,
//! one 1.2 × 1.2 m window per storey in the south wall. A 100 klx sun 55° high stands in the north,
//! so no sun ray enters a window (the windows face away from it) and the lower storeys see only the
//! 10 klx sky through their windows. The light switched on the top storey is one of:
//!
//! - **panel**: a 2 × 2 m emissive ceiling panel (20 000 cd/m²). The probes (dynamic and baked),
//!   the screen-space pass and the forward pass's emissive area lights all carry it.
//! - **point**: a shadowed 200 klm point light 1.2 m under the roof, radius 12 m (it reaches the
//!   ground storey). The forward pass's local shadows carry it; the probes do not trace local lights.
//! - **sun**: for the sun cascades, the sun itself switched on and off with no sky and no top-storey
//!   light, scored inside every storey against a view of the sunlit ground.
//!
//! **The measurement.** Each path renders a lower storey's view with the light on and with it off,
//! from two renderers that never switch, at the same frame number (the memory note "compare
//! switch-off at the same frame": a switched renderer still converges, so only a never-switched control
//! at the same frame tells the switch from the drift). The leak of a storey is its mean linear
//! luminance difference over the difference the light makes where it is (the top storey's view; for
//! the sun, the sunlit ground):
//!
//! `leak = (lower_on − lower_off) / (top_on − top_off)`.
//!
//! **The bar, from the path tracer** (the reference of the same scene from `LabScene`, every light
//! and bounce; direct light only for the sun row). Through closed 0.3 m slabs the true transport is
//! zero; what the tracer still finds on a lower storey is light that leaves the top window, bounces
//! off the ground or the facade, and comes back in through a lower window: real, and tiny. Each
//! storey's reference leak is traced with two seeds, `a` and `b`. Its mean `r = (a + b) / 2` is the
//! true leak to within the noise; one estimate's standard deviation is `σ = |a − b| / √2` (the
//! difference of two independent estimates has variance 2σ²). The bar is
//!
//! `bar = max(r, 0) + 3σ + 0.005`,
//!
//! the reference plus three standard deviations of its noise plus spec 27 § 2.2's sealed-room bound
//! (0.5 % of the light that should be absent) as the margin. A path passes when its leak is at or
//! under the bar.
//!
//! Every path scores storeys 0 and 1 (2 and above for the sun). Rows print as `stacked-floors ...`.
//! Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI). No golden.
//! `cargo test -p regia_pt --test stacked_floors -- --nocapture --test-threads=1`

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use regia_asset::baked_gi::{BakedGiVolume, VolumeLayout};
use regia_core::glam::Mat4;
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::probe::{GiBakeSettings, ProbeTraceSettings, bake_gi_volume, detail_brick};
use regia_pt::{PathTracer, PtSettings};
use regia_render::{
    AERIAL_INTERIOR_SLOTS, DdgiSettings, DirectionalLightUniform, DynamicGiTier, FrameQuality, FrameRenderer,
    GiMode, InteriorVolume, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, box_mesh, interior_spans_on_ray,
};

const W: u32 = 128;
const H: u32 = 80;
/// Floor slab and roof thickness (DO27 ask 11).
const SLAB: f32 = 0.3;
/// Storey pitch: slab top to slab top.
const STOREY: f32 = 3.0;
/// Clear height of a storey.
const CLEAR: f32 = STOREY - SLAB;
/// Outer wall thickness.
const WALL: f32 = 0.2;
/// Inside size of the block in x and z.
const SIZE: f32 = 6.0;
/// Storeys.
const STOREYS: usize = 3;
/// The window: x from `WIN_X0` to `WIN_X1`, sill and head above each storey's floor.
const WIN_X0: f32 = 2.4;
const WIN_X1: f32 = 3.6;
const SILL: f32 = 0.9;
const HEAD: f32 = 2.1;
/// The panel's radiance, cd/m².
const PANEL_NITS: f32 = 20_000.0;
/// The point light's power, lm.
const POINT_LM: f32 = 200_000.0;
/// Sun and sky.
const SUN_LUX: f32 = 100_000.0;
const SKY_LUX: f32 = 10_000.0;
/// Spec 27 § 2.2: the sealed-room leak bound, the margin over the reference.
const MARGIN: f32 = 0.005;
/// Frames of a probe path (every probe re-traced each frame at hysteresis 0.9: 0.9^64 leaves 0.1 % of the start).
const PROBE_FRAMES: u32 = 64;
/// Frames of a path without probe history.
const STILL_FRAMES: u32 = 2;
/// Path tracer samples per pixel (`REGIA_STACKED_SPP` overrides).
const REFERENCE_SPP: u32 = 128;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("stacked floors need a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// Serializes the tests of this binary: each renders many frames on one device.
static SERIAL: Mutex<()> = Mutex::new(());

fn block(lo: [f32; 3], hi: [f32; 3], material: usize) -> LabMesh {
    let centre = [
        0.5 * (lo[0] + hi[0]),
        0.5 * (lo[1] + hi[1]),
        0.5 * (lo[2] + hi[2]),
    ];
    let half = [
        0.5 * (hi[0] - lo[0]),
        0.5 * (hi[1] - lo[1]),
        0.5 * (hi[2] - lo[2]),
    ];
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// What the switch turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// The emissive ceiling panel on the top storey.
    Panel,
    /// The shadowed point light on the top storey.
    Point,
    /// The sun (no sky, no top-storey light).
    Sun,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Point => "point",
            Self::Sun => "sun",
        }
    }

    /// The storeys scored as "lower" (should stay dark).
    fn lower(self) -> &'static [usize] {
        match self {
            Self::Panel | Self::Point => &[0, 1],
            // The roof is a slab too: under it the top storey is as closed as the others.
            Self::Sun => &[0, 1, 2],
        }
    }

    /// The view where the light is: the top storey, or the sunlit ground for the sun.
    fn lit_view(self) -> View {
        match self {
            Self::Panel | Self::Point => View::Storey(STOREYS - 1),
            Self::Sun => View::Ground,
        }
    }

    /// Path tracer vertices: every bounce, or the direct light alone for the sun cascades.
    fn reference_bounces(self) -> u32 {
        match self {
            Self::Panel | Self::Point => 8,
            Self::Sun => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// Inside storey `k`, from beside the window looking at the far corner (the window behind).
    Storey(usize),
    /// Outside, north of the block, looking down at the sunlit ground.
    Ground,
}

impl View {
    fn name(self) -> String {
        match self {
            Self::Storey(k) => format!("storey{k}"),
            Self::Ground => "ground".into(),
        }
    }

    fn eye_target(self) -> ([f32; 3], [f32; 3]) {
        match self {
            Self::Storey(k) => {
                let y = k as f32 * STOREY;
                ([4.6, y + 1.6, 0.5], [1.2, y + 0.4, 5.2])
            }
            Self::Ground => ([3.0, 3.0, 12.0], [3.0, 0.0, 18.0]),
        }
    }
}

/// The block's meshes. Material 0 is the grey shell; material 1 is the panel (its emission is
/// switched in the material, so the geometry is the same with the light on and off).
fn meshes() -> Vec<LabMesh> {
    let (lo, hi) = (-WALL, SIZE + WALL);
    let mut m = vec![
        // Open ground, top at y = 0: the ground storey's floor.
        block([-40.0, -1.0, -40.0], [46.0, 0.0, 46.0], 0),
        // Roof, 0.3 m, on top of the last storey.
        block(
            [lo, STOREYS as f32 * STOREY - SLAB, lo],
            [hi, STOREYS as f32 * STOREY, hi],
            0,
        ),
    ];
    for k in 0..STOREYS {
        let y0 = k as f32 * STOREY;
        let y1 = y0 + CLEAR;
        if k > 0 {
            // The floor slab under storey k runs through the outer walls.
            m.push(block([lo, y0 - SLAB, lo], [hi, y0, hi], 0));
        }
        // North (+z), east (+x) and west (−x) walls, closed.
        m.push(block([lo, y0, SIZE], [hi, y1, hi], 0));
        m.push(block([SIZE, y0, 0.0], [hi, y1, SIZE], 0));
        m.push(block([lo, y0, 0.0], [0.0, y1, SIZE], 0));
        // South (−z) wall around the window.
        m.push(block([lo, y0, lo], [WIN_X0, y1, 0.0], 0));
        m.push(block([WIN_X1, y0, lo], [hi, y1, 0.0], 0));
        m.push(block([WIN_X0, y0, lo], [WIN_X1, y0 + SILL, 0.0], 0));
        m.push(block([WIN_X0, y0 + HEAD, lo], [WIN_X1, y1, 0.0], 0));
    }
    // The panel, 2 × 2 m, 4 cm thick, under the roof of the top storey.
    let top = (STOREYS - 1) as f32 * STOREY + CLEAR;
    m.push(block([2.0, top - 0.04, 2.0], [4.0, top, 4.0], 1));
    m
}

fn lab(source: Source, on: bool, view: View) -> LabScene {
    let (eye, target) = view.eye_target();
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = 11.0;
    let grey = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.95,
        ..LabMaterial::default()
    };
    let panel_on = source == Source::Panel && on;
    let panel = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        roughness: 0.95,
        emissive: if panel_on { [1.0; 3] } else { [0.0; 3] },
        emissive_intensity: if panel_on { PANEL_NITS } else { 0.0 },
        ..LabMaterial::default()
    };
    let mut local_lights = Vec::new();
    if source == Source::Point && on {
        local_lights.push(LabLocalLight::Point(LabPointLight {
            position: [3.0, (STOREYS - 1) as f32 * STOREY + CLEAR - 1.2, 3.0],
            color: [1.0; 3],
            intensity: POINT_LM,
            radius: 12.0,
            cast_shadows: true,
            source_radius: None,
        }));
    }
    let sun_lux = match source {
        Source::Sun if !on => 0.0,
        _ => SUN_LUX,
    };
    // North sun, 55° high: the light travels south (−z) and down.
    let e = 55f32.to_radians();
    let dir = [0.25 * e.cos(), -e.sin(), -0.97 * e.cos()];
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 1.1,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 11.0,
        ambient_lux: if source == Source::Sun { 0.0 } else { SKY_LUX },
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new(dir, [1.0; 3], sun_lux),
        local_lights,
        materials: vec![grey, panel],
        meshes: meshes(),
    }
}

/// Desktop High at native scale without fog, clouds or reflections; every GI technique off (the
/// paths below turn theirs on). The static-screen cache is off (cascaded sun shadows), so a fixed
/// frame count measures where the light settles, not an early frame the cache holds.
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
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    q
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn mean_luminance(rgb: &[[f32; 3]]) -> f32 {
    let sum: f64 = rgb.iter().map(|c| f64::from(luminance(*c))).sum();
    (sum / rgb.len().max(1) as f64) as f32
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Mean path-traced luminance of a view.
fn traced(source: Source, on: bool, view: View, seed: u32) -> f32 {
    let scene = lab(source, on, view);
    let pt = from_frame(
        &scene.as_frame_scene_with_quality(base_quality()),
        &ExtractOptions::default(),
    );
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: env_u32("REGIA_STACKED_SPP", REFERENCE_SPP),
                max_bounces: source.reference_bounces(),
                seed,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    mean_luminance(&image.beauty)
}

/// One lower storey's reference leak and its bar.
#[derive(Clone, Copy, Debug)]
struct Bar {
    storey: usize,
    /// The reference leak, mean of the two seeds.
    reference: f32,
    /// One estimate's standard deviation.
    sigma: f32,
    /// `max(reference, 0) + 3 sigma + MARGIN`.
    bar: f32,
}

/// The path tracer's leak per lower storey and the bars derived from it (module docs).
fn bars(source: Source) -> Vec<Bar> {
    static PANEL: OnceLock<Vec<Bar>> = OnceLock::new();
    static POINT: OnceLock<Vec<Bar>> = OnceLock::new();
    static SUN: OnceLock<Vec<Bar>> = OnceLock::new();
    let cell = match source {
        Source::Panel => &PANEL,
        Source::Point => &POINT,
        Source::Sun => &SUN,
    };
    cell.get_or_init(|| {
        let (seed_a, seed_b) = (23, 71);
        let lit = source.lit_view();
        let lit_delta = traced(source, true, lit, seed_a) - traced(source, false, lit, seed_a);
        eprintln!(
            "stacked-floors reference source={} view={} delta={lit_delta:.4}",
            source.name(),
            lit.name()
        );
        assert!(
            lit_delta.is_finite() && lit_delta > 0.0,
            "the switched light lights its own view in the reference ({lit_delta})"
        );
        source
            .lower()
            .iter()
            .map(|&k| {
                let view = View::Storey(k);
                let leak =
                    |seed| (traced(source, true, view, seed) - traced(source, false, view, seed)) / lit_delta;
                let (a, b) = (leak(seed_a), leak(seed_b));
                let reference = 0.5 * (a + b);
                let sigma = (a - b).abs() / std::f32::consts::SQRT_2;
                let bar = reference.max(0.0) + 3.0 * sigma + MARGIN;
                eprintln!(
                    "stacked-floors reference source={} storey={k} leak_a={a:.6} leak_b={b:.6} \
                     reference={reference:.6} sigma={sigma:.6} bar={bar:.6}",
                    source.name()
                );
                Bar {
                    storey: k,
                    reference,
                    sigma,
                    bar,
                }
            })
            .collect()
    })
    .clone()
}

/// A renderer path: how it is set up and how many frames it renders before the capture.
struct Path {
    name: String,
    quality: FrameQuality,
    frames: u32,
    setup: Box<dyn Fn(&mut FrameRenderer)>,
}

/// Mean linear luminance (cd/m²) of `view` after `path.frames` frames from a fresh renderer.
fn rendered(path: &Path, source: Source, on: bool, view: View) -> f32 {
    let scene = lab(source, on, view);
    let mut r = FrameRenderer::new();
    (path.setup)(&mut r);
    let frame = scene.as_frame_scene_with_quality(path.quality);
    for i in 0..path.frames {
        if i + 1 == path.frames {
            r.request_hdr_capture();
        }
        r.render_offscreen(device(), &frame).expect("frame");
    }
    let hdr = r
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (W, H));
    mean_luminance(&hdr.rgb)
}

/// Scores `path` against the reference bars: prints a row per storey and returns the failures.
fn score(path: &Path, source: Source) -> Vec<String> {
    let bars = bars(source);
    let lit = source.lit_view();
    let lit_on = rendered(path, source, true, lit);
    let lit_off = rendered(path, source, false, lit);
    let lit_delta = lit_on - lit_off;
    eprintln!(
        "stacked-floors path={} source={} view={} on={lit_on:.4} off={lit_off:.4} delta={lit_delta:.4}",
        path.name,
        source.name(),
        lit.name()
    );
    assert!(
        lit_delta.is_finite() && lit_delta > 0.0,
        "{}: the switched light lights its own view ({lit_on} on, {lit_off} off)",
        path.name
    );
    let mut failures = Vec::new();
    for b in &bars {
        let view = View::Storey(b.storey);
        let on = rendered(path, source, true, view);
        let off = rendered(path, source, false, view);
        let leak = (on - off) / lit_delta;
        let pass = leak.is_finite() && leak <= b.bar;
        eprintln!(
            "stacked-floors path={} source={} storey={} on={on:.5} off={off:.5} leak={leak:.6} \
             reference={:.6} sigma={:.6} bar={:.6} {}",
            path.name,
            source.name(),
            b.storey,
            b.reference,
            b.sigma,
            b.bar,
            if pass { "pass" } else { "FAIL" }
        );
        if !pass {
            failures.push(format!(
                "{} ({}): storey {} leak {leak:.5} over the bar {:.5} (reference {:.5}, sigma {:.5}; on {on:.4}, off {off:.4})",
                path.name,
                source.name(),
                b.storey,
                b.bar,
                b.reference,
                b.sigma
            ));
        }
    }
    failures
}

/// The dynamic probes at a shipped tier's layout, every probe re-traced each frame at hysteresis 0.9
/// so [`PROBE_FRAMES`] converge (the tier's visibility, spacing and cascades are unchanged), with the
/// screen-space pass Desktop High runs beside them.
fn ddgi_path(tier: DynamicGiTier) -> Path {
    let mut s = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    tier.apply(&mut s);
    s.update_divisor = 1;
    s.hysteresis = 0.9;
    let quality = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: true,
        ..base_quality()
    };
    Path {
        name: format!("ddgi_{tier:?}").to_lowercase(),
        quality,
        frames: PROBE_FRAMES,
        setup: Box::new(move |r| {
            r.set_ddgi_settings(s.clone());
            r.set_ddgi_enabled(Some(true));
        }),
    }
}

/// The frame without any probe: direct light (sun cascades, local shadows, the emissive area lights)
/// and the flat sky term.
fn direct_path() -> Path {
    Path {
        name: "direct".into(),
        quality: FrameQuality {
            ssao: SsaoMode::Off,
            ..base_quality()
        },
        frames: STILL_FRAMES,
        setup: Box::new(|r| {
            r.set_ddgi_enabled(Some(false));
            r.set_baked_gi_enabled(Some(false));
        }),
    }
}

fn failures_report(failures: &[String]) -> String {
    failures.join("\n")
}

#[test]
fn the_path_traced_reference_keeps_the_light_upstairs() {
    let _gpu = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    // The bar's own sanity: through closed slabs the reference leak is near zero for every source.
    for source in [Source::Panel, Source::Point, Source::Sun] {
        for b in bars(source) {
            assert!(
                b.reference.abs() < MARGIN,
                "{}: the reference itself carries {:.5} of the light to storey {}",
                source.name(),
                b.reference,
                b.storey
            );
        }
    }
}

#[test]
fn the_dynamic_probes_keep_the_top_floor_light_upstairs() {
    let _gpu = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let mut failures = Vec::new();
    for tier in [DynamicGiTier::Low, DynamicGiTier::High] {
        failures.extend(score(&ddgi_path(tier), Source::Panel));
    }
    assert!(failures.is_empty(), "{}", failures_report(&failures));
}

#[test]
fn the_baked_probes_keep_the_top_floor_light_upstairs() {
    let _gpu = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let on = baked_volume(true);
    let off = baked_volume(false);
    // The two volumes differ only in the panel's light; each frame reads the volume baked for its
    // own state. A bake is the scene's: the panel off has no light to bake.
    let quality = FrameQuality {
        baked_gi: true,
        screen_space_gi: true,
        ..base_quality()
    };
    let path = |volume: Arc<BakedGiVolume>| Path {
        name: "baked".into(),
        quality,
        frames: STILL_FRAMES,
        setup: Box::new(move |r| {
            r.set_ddgi_enabled(Some(false));
            r.set_baked_gi(Some(volume.clone()));
            r.set_baked_gi_enabled(Some(true));
        }),
    };
    let (path_on, path_off) = (path(on), path(off));
    let source = Source::Panel;
    let bars = bars(source);
    let lit = source.lit_view();
    let lit_delta = rendered(&path_on, source, true, lit) - rendered(&path_off, source, false, lit);
    eprintln!(
        "stacked-floors path=baked source=panel view={} delta={lit_delta:.4}",
        lit.name()
    );
    assert!(
        lit_delta > 0.0,
        "the baked panel lights the top storey ({lit_delta})"
    );
    let mut failures = Vec::new();
    for b in &bars {
        let view = View::Storey(b.storey);
        let on = rendered(&path_on, source, true, view);
        let off = rendered(&path_off, source, false, view);
        let leak = (on - off) / lit_delta;
        let pass = leak.is_finite() && leak <= b.bar;
        eprintln!(
            "stacked-floors path=baked source=panel storey={} on={on:.5} off={off:.5} leak={leak:.6} \
             reference={:.6} sigma={:.6} bar={:.6} {}",
            b.storey,
            b.reference,
            b.sigma,
            b.bar,
            if pass { "pass" } else { "FAIL" }
        );
        if !pass {
            failures.push(format!(
                "baked (panel): storey {} leak {leak:.5} over the bar {:.5} (on {on:.4}, off {off:.4})",
                b.storey, b.bar
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures_report(&failures));
}

#[test]
fn the_emissive_area_light_and_the_point_light_shadow_stop_at_the_slab() {
    let _gpu = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let mut failures = score(&direct_path(), Source::Panel);
    failures.extend(score(&direct_path(), Source::Point));
    assert!(failures.is_empty(), "{}", failures_report(&failures));
}

#[test]
fn the_sun_cascades_leave_every_storey_dark() {
    let _gpu = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let failures = score(&direct_path(), Source::Sun);
    assert!(failures.is_empty(), "{}", failures_report(&failures));
}

/// The interior brick a DO27-style bake places over the whole block: one brick for all three
/// storeys at 1 m (the default interior spacing), so probes stand above and below every slab and the
/// lookup has to keep a storey's taps on its own side.
fn layout() -> VolumeLayout {
    let lo = [0.0, 0.0, 0.0];
    let hi = [SIZE, STOREYS as f32 * STOREY - SLAB, SIZE];
    let mut layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick(lo, hi, 1.0)],
        moment_resolution: 8,
        max_distance: 8.0,
    };
    assert_eq!(layout.flag_interior_bricks(&[(lo, hi)]), 1);
    layout.assign_brick_offsets();
    layout
}

/// The block baked with the panel on or off (the sky and sun as in every frame).
fn baked_volume(on: bool) -> Arc<BakedGiVolume> {
    static ON: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    static OFF: OnceLock<Arc<BakedGiVolume>> = OnceLock::new();
    let cell = if on { &ON } else { &OFF };
    cell.get_or_init(|| {
        let scene = lab(Source::Panel, on, View::Storey(0));
        let pt = from_frame(
            &scene.as_frame_scene_with_quality(base_quality()),
            &ExtractOptions::default(),
        );
        let settings = GiBakeSettings {
            trace: ProbeTraceSettings {
                rays_per_probe: 128,
                samples: 4,
                max_bounces: 6,
                seed: 5,
                samples_per_submit: 4,
                ..ProbeTraceSettings::default()
            },
            ..GiBakeSettings::default()
        };
        let bake = bake_gi_volume(tracer(), &pt, &layout(), &settings, |_, _, _| true).expect("bake");
        eprintln!("stacked-floors baked on={on} {:?}", bake.report);
        Arc::new(bake.volume)
    })
    .clone()
}

/// One `InteriorVolume` per storey (the contract's rule for a stacked building) on two blocks side
/// by side: six volumes, over the composite's four air slots. A camera on the second block's top
/// storey looks along its room: the air there is under a roof and takes no sky in-scatter, so the
/// whole 2 m ray stretch must be inside a bound volume (`interior_spans_on_ray`, the CPU twin of the
/// fog's and the aerial perspective's slots). Before ADR-7661 the first four volumes in list order
/// were bound, and the camera's own storey (the sixth) was not.
#[test]
fn the_storey_the_camera_stands_in_keeps_its_air_out_of_the_sky_light() {
    let half = [0.5 * SIZE, 0.5 * CLEAR, 0.5 * SIZE];
    let volumes: Vec<InteriorVolume> = (0..2)
        .flat_map(|b| {
            (0..STOREYS).map(move |k| {
                InteriorVolume::with_floor(
                    [b as f32 * 20.0 + half[0], k as f32 * STOREY + half[1], half[2]],
                    half,
                )
            })
        })
        .collect();
    assert!(volumes.len() > AERIAL_INTERIOR_SLOTS);
    let eye = [20.0 + 1.0, (STOREYS - 1) as f32 * STOREY + 1.6, 3.0];
    assert!(volumes[volumes.len() - 1].contains(eye));
    let spans = interior_spans_on_ray(eye, [1.0, 0.0, 0.0], 2.0, &volumes);
    eprintln!(
        "stacked-floors interiors={} eye={eye:?} inside_m={:.3} spans={:?}",
        volumes.len(),
        spans.inside_m,
        spans.spans
    );
    assert!(
        (spans.inside_m - 2.0).abs() < 1.0e-4,
        "the camera's own storey is not among the bound interior volumes: {:.3} m of 2 m inside",
        spans.inside_m
    );
}
