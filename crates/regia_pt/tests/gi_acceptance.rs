// SPDX-License-Identifier: MPL-2.0
//! Dynamic GI acceptance measurement for v0.5 (J06, `specs/79` § 9, `specs/27` § 2.2 GI rows and § 2.3
//! stability, ADR-4948): one harness that turns the GI acceptance into a numbered table of metric, bar,
//! measured value and verdict, on whatever adapter `RenderDevice::headless` picks (lavapipe, or a card
//! pinned with `REGIA_GPU_UUID` + `REGIA_GPU_STRICT=1`).
//!
//! It measures, it does not gate: the GPU tests are `#[ignore]` and assert only that the harness ran
//! (finite numbers, the probes lit the frame). The regression gates stay `gi_validation` (the recorded
//! errors), `gi_radiance_cache` and `regia_render`'s `dynamic_gi`; this file reports where the renderer
//! stands against the bars and why it misses.
//!
//! - `acceptance_scenes`: the standard plates (`regia_test_utils::gi_scenes`) plus two interiors built
//!   here, a room lit through a door and a tunnel open at both ends. Each scene against the path
//!   tracer's indirect light at four probe configurations: the plate's fine layout and the shipped Low
//!   and High tiers, settled with the static-screen cache off, and the fine layout with the cache on (the
//!   held view a standing player sees). Rows: mean and p95 FLIP, energy, GI in sun shadow over the
//!   tracer's, the leak on the sealed box, the probes' coverage of the view, frame-to-frame FLIP after
//!   convergence with a no-GI control, and the drift from N to 2N frames.
//! - `energy_decomposition`: the indirect light split by light (sun only, sky only) and the tracer's
//!   bounce depth (1, 2 and 6 bounces), at the fine layout and the High tier, so a deficit points at the
//!   probes' sun bounce, their sky term or their multi-bounce.
//! - `tier_leak`: the sealed box at every shipped tier.
//! - `tier_cost`: the probe update's GPU time per tier at 2560 × 1440 and 1280 × 800, and the frame's
//!   create-to-first-frame CPU time (printed; no time is asserted).
//! - `owner_plates`: review plates (beauty and indirect light), path tracer | High tier | fine layout |
//!   FLIP, `REGIA_GI_PLATE_SCALE` times the GI frame size.
//!
//! Output: printed, and with `REGIA_GI_ACCEPT_OUT=<dir>` written as `<test>.md` (and PNG plates).
//! `cargo test -p regia_pt --test gi_acceptance -- --ignored --nocapture --test-threads=1`

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings};
use regia_render::compare::{DEFAULT_PPD, compare_linear, compare_srgb8, display_encode, flip_heatmap_rgba8};
use regia_render::{
    DdgiSettings, DirectionalLightUniform, DynamicGiTier, FrameQuality, FrameRenderer, GiMode, LabMaterial,
    LabMesh, LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, box_mesh,
};
use regia_test_utils::gi_error::{
    GiError, GiErrorInput, add_planes, luminance, measure, shadow_mask, sub_planes, surface_mask,
};
use regia_test_utils::gi_scenes::{GI_HEIGHT, GI_WIDTH, GiScene, color_room, sealed_box, trench};

/// § 2.2 GI (diffuse) mean FLIP target.
const FLIP_TARGET: f32 = 0.08;
/// § 2.2 GI (diffuse) p95 FLIP target (ADR-2951).
const FLIP_P95_TARGET: f32 = 0.20;
/// § 2.2 leak target: interior luminance over the lit exterior ground's.
const LEAK_TARGET: f32 = 0.005;
/// § 2.3 stability: static-scene frame-to-frame FLIP after convergence.
const STABILITY_TARGET: f32 = 0.01;
/// Least mean probe coverage of the scored pixels (`gi_validation`
/// `the_plate_layouts_hold_what_the_camera_sees`): under it the frame mixes in the flat sky term.
const COVERAGE_TARGET: f32 = 0.98;
/// `specs/79` § 7: Desktop High probes at 1440p on the RX 7900 and the RTX 5080, ms.
const HIGH_COST_MS: f32 = 0.6;
/// `specs/79` § 7: the Deck tier at 800p on the Deck, ms (★ first guess).
const DECK_COST_MS: f32 = 1.5;

/// Frames of the plates' fine layouts (every probe re-traced every frame at hysteresis 0.9; `gi_validation`).
const PLATE_FRAMES: u32 = 64;
/// Frames of a shipped tier with every probe re-traced each frame (`update_divisor` 1) at the tier's own
/// hysteresis (0.97: 0.97^192 leaves 0.3 % of the start).
const TIER_FRAMES: u32 = 192;
/// Frames of a technique without history.
const STILL_FRAMES: u32 = 2;
/// Consecutive frames scored for frame-to-frame stability after convergence.
const STABILITY_FRAMES: u32 = 8;
/// Path tracer samples per pixel of the references.
const REFERENCE_SPP: u32 = 1024;
/// Path tracer bounces of the acceptance references (ADR-5290). The probes carry every bounce (each update reads
/// the field it builds); 6 bounces held 90 % of the door room's converged indirect light and 95 % of the tunnel's,
/// so the probes read over a short reference (door room fine layout 1.20 × at 6 bounces, 1.09 × at 32). 12 bounces
/// hold within 0.6 % of 32 on every scene; Russian roulette from the third vertex keeps the long paths cheap.
const REFERENCE_BOUNCES: u32 = 32;
/// Path tracer samples per pixel of the decomposition's references.
const DECOMPOSITION_SPP: u32 = 512;

/// Writes the tables and plates there when set.
const OUT_ENV: &str = "REGIA_GI_ACCEPT_OUT";
/// Plate size multiplier (of 160 × 96).
const PLATE_SCALE_ENV: &str = "REGIA_GI_PLATE_SCALE";
/// Plate path tracer samples per pixel.
const PLATE_SPP_ENV: &str = "REGIA_GI_PLATE_SPP";
/// Comma list of scene names to run (default: all).
const SCENES_ENV: &str = "REGIA_GI_SCENES";
/// Comma list of configuration names to run (default: all).
const CONFIGS_ENV: &str = "REGIA_GI_CONFIGS";
/// Frames of the shipped tiers (default [`TIER_FRAMES`]).
const TIER_FRAMES_ENV: &str = "REGIA_GI_TIER_FRAMES";
/// Measurement (ADR-5909): metres every scene is moved by, `x,y,z` (meshes, eye and target alike; the lights keep
/// their directions). The probes stand on a world lattice, so this moves the lattice against the scene's walls: what
/// a layout's score owes to where the walls fall between its probes.
const SHIFT_ENV: &str = "REGIA_GI_SHIFT";
/// Measurement (ADR-5909): `0` scores the frames without the screen-space gather (the probes alone).
const SSGI_ENV: &str = "REGIA_GI_SSGI";
/// Measurement (ADR-5909): `0` scores the frames without ambient occlusion.
const SSAO_ENV: &str = "REGIA_GI_SSAO";
/// Measurement (ADR-5909): the `x` configuration, a tier's layout with fields replaced, as `key=value` pairs
/// separated by `;` ([`x_config`] lists the keys; `tier=low|medium|high|ultra` comes first when given, Low without
/// it). One binary then measures any layout.
const X_ENV: &str = "REGIA_GI_X";

// ---------------------------------------------------------------------------------------------
// The table (pure, unit-tested below).
// ---------------------------------------------------------------------------------------------

/// What a row is held to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Bar {
    /// Passes at or below the value.
    AtMost(f32),
    /// Passes at or above the value.
    AtLeast(f32),
    /// Reported only: no bar in the specs (or none on this adapter).
    Report,
}

impl Bar {
    /// `Some(pass)` against the bar; `None` for a report row. A non-finite measurement fails.
    fn verdict(self, measured: f32) -> Option<bool> {
        match self {
            Self::AtMost(b) => Some(measured.is_finite() && measured <= b),
            Self::AtLeast(b) => Some(measured.is_finite() && measured >= b),
            Self::Report => None,
        }
    }

    fn text(self, unit: &str) -> String {
        match self {
            Self::AtMost(b) => format!("≤ {}{unit}", fmt_value(b)),
            Self::AtLeast(b) => format!("≥ {}{unit}", fmt_value(b)),
            Self::Report => "report".to_owned(),
        }
    }
}

/// One measured row.
#[derive(Clone, Debug)]
struct Row {
    metric: String,
    bar: Bar,
    measured: f32,
    unit: &'static str,
    /// Likely cause of a miss, or a note.
    note: String,
}

impl Row {
    fn new(metric: impl Into<String>, bar: Bar, measured: f32, unit: &'static str) -> Self {
        Self {
            metric: metric.into(),
            bar,
            measured,
            unit,
            note: String::new(),
        }
    }

    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
}

fn fmt_value(v: f32) -> String {
    if !v.is_finite() {
        return format!("{v}");
    }
    let a = v.abs();
    if a != 0.0 && !(0.001..10_000.0).contains(&a) {
        format!("{v:.3e}")
    } else if a >= 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.4}")
    }
}

/// The numbered markdown table: `#`, metric, bar, measured, verdict, likely cause / note.
fn render_rows(first: usize, rows: &[Row]) -> String {
    let mut s = String::from(
        "| # | Metric | Bar | Measured | Verdict | Likely cause / note |\n|---|---|---|---|---|---|\n",
    );
    for (i, r) in rows.iter().enumerate() {
        let verdict = match r.bar.verdict(r.measured) {
            Some(true) => "pass",
            Some(false) => "**FAIL**",
            None => "report",
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {}{} | {verdict} | {} |",
            first + i,
            r.metric,
            r.bar.text(r.unit),
            fmt_value(r.measured),
            r.unit,
            r.note.replace('|', "/")
        );
    }
    s
}

/// Least even probe count per axis whose finest cascade's full-weight window, at `spacing` with the eye
/// at `eye`, holds `[lo, hi]`: the window runs from `e − lift − d/2 + 1.75` to `e − lift + d/2 − 3.75`
/// cells (`e` the eye in cells, `lift` 0.1 d in y only; `gi_scenes`' `ddgi`). Capped at 64.
fn plate_dims(eye: [f32; 3], spacing: f32, lo: [f32; 3], hi: [f32; 3]) -> [u32; 3] {
    let mut dims = [64; 3];
    for (axis, out) in dims.iter_mut().enumerate() {
        let e = eye[axis] / spacing;
        let (l, h) = (lo[axis] / spacing, hi[axis] / spacing);
        for d in (4..=64u32).step_by(2) {
            let df = d as f32;
            let lift = if axis == 1 { 0.1 * df } else { 0.0 };
            if e - lift - df / 2.0 + 1.75 <= l && h <= e - lift + df / 2.0 - 3.75 {
                *out = d;
                break;
            }
        }
    }
    dims
}

// ---------------------------------------------------------------------------------------------
// Scenes built here: two interiors with an opening.
// ---------------------------------------------------------------------------------------------

fn block(centre: [f32; 3], half: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material,
        model: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [centre[0], centre[1], centre[2], 1.0],
        ],
        ..LabMesh::unskinned()
    }
}

fn lambert(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        metallic: 0.0,
        roughness: 0.95,
        ..LabMaterial::default()
    }
}

fn plate_ddgi(spacing: f32, dims: [u32; 3]) -> DdgiSettings {
    DdgiSettings {
        cascades: 2,
        dims,
        spacing,
        spacing_ratio: 3.0,
        update_divisor: 1,
        hysteresis: 0.9,
        min_proxy_radius: 0.2,
        bake_on_caller: true,
        ..DdgiSettings::default()
    }
}

fn lab(
    eye: [f32; 3],
    target: [f32; 3],
    exposure_ev: f32,
    sky_lux: f32,
    sun: DirectionalLightUniform,
    materials: Vec<LabMaterial>,
    meshes: Vec<LabMesh>,
) -> LabScene {
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = exposure_ev;
    LabScene {
        width: GI_WIDTH,
        height: GI_HEIGHT,
        eye,
        target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev,
        ambient_lux: sky_lux,
        double_sided: false,
        post: Some(post),
        directional: sun,
        local_lights: Vec::new(),
        materials,
        meshes,
    }
}

/// Eye of [`door_room`].
const DOOR_EYE: [f32; 3] = [1.2, 1.6, -2.2];

/// A 6 × 3 × 6 m room (20 cm walls) with a 1.6 × 2.2 m door in its +z wall, on a large ground, the
/// camera inside looking at the door. A 100 klx sun 40° high travels in through the door onto the
/// floor; a 10 klx sky. A warm left wall (−x) and white walls otherwise: everything the camera sees
/// inside except the sun patch is indirect light. The interior plate the owner reviews.
fn door_room() -> GiScene {
    let t = 0.1;
    let (h, door_w, door_h) = (3.0f32, 1.6f32, 2.2f32);
    let wh = h / 2.0;
    let side = (6.0 - door_w) / 4.0; // half width of each wall beside the door
    let meshes = vec![
        block([0.0, -0.25, 0.0], [30.0, 0.25, 30.0], 0),
        block([3.0 + t, wh, 0.0], [t, wh, 3.0 + 2.0 * t], 1),
        block([-3.0 - t, wh, 0.0], [t, wh, 3.0 + 2.0 * t], 2),
        block([0.0, wh, -3.0 - t], [3.0, wh, t], 1),
        block([-3.0 + side, wh, 3.0 + t], [side, wh, t], 1),
        block([3.0 - side, wh, 3.0 + t], [side, wh, t], 1),
        // Lintel over the door.
        block(
            [0.0, door_h + (h - door_h) / 2.0, 3.0 + t],
            [door_w / 2.0, (h - door_h) / 2.0, t],
            1,
        ),
        block([0.0, h + t, 0.0], [3.0 + 2.0 * t, t, 3.0 + 2.0 * t], 1),
    ];
    let spacing = 0.5;
    let dims = plate_dims(DOOR_EYE, spacing, [-3.0, 0.0, -3.0], [3.0, 3.0, 3.0]);
    GiScene {
        name: "door_room",
        lab: lab(
            DOOR_EYE,
            [-0.4, 0.8, 3.0],
            11.5,
            10_000.0,
            DirectionalLightUniform::new([-0.15, -0.643, -0.75], [1.0, 0.97, 0.92], 100_000.0),
            vec![
                lambert([0.45; 3]),
                lambert([0.75, 0.75, 0.72]),
                lambert([0.8, 0.45, 0.25]),
            ],
            meshes,
        ),
        bake_bounds: ([-2.8, 0.2, -2.8], [2.8, 2.8, 2.8]),
        bake_spacing: 0.5,
        ddgi: plate_ddgi(spacing, dims),
        scale: None,
    }
}

/// Eye of [`tunnel`].
const TUNNEL_EYE: [f32; 3] = [-2.0, 1.5, 0.6];

/// A tunnel 16 m long in x, 3 m wide and 3 m tall inside (20 cm walls and roof), open at both ends,
/// on a large ground, the camera inside looking at the +x mouth. A 100 klx sun 55° high lights the
/// ground outside both mouths and a strip just inside them; an 8 klx sky. Inside, away from the
/// mouths, every bit of light is indirect: the lab analog of the `specs/27` § 2.0 tunnel_interior plate
/// (GI in sun shadow read 2.56 × the path tracer there).
fn tunnel() -> GiScene {
    let t = 0.1;
    let (len, half_w, h) = (8.0f32, 1.5f32, 3.0f32);
    let meshes = vec![
        block([0.0, -0.25, 0.0], [40.0, 0.25, 40.0], 0),
        block([0.0, h / 2.0, half_w + t], [len, h / 2.0, t], 1),
        block([0.0, h / 2.0, -half_w - t], [len, h / 2.0, t], 1),
        block([0.0, h + t, 0.0], [len, t, half_w + 2.0 * t], 1),
    ];
    let spacing = 0.75;
    let dims = plate_dims(TUNNEL_EYE, spacing, [-len, 0.0, -half_w], [len, h, half_w]);
    GiScene {
        name: "tunnel",
        lab: lab(
            TUNNEL_EYE,
            [6.0, 1.0, -0.3],
            11.5,
            8_000.0,
            DirectionalLightUniform::new([-0.45, -0.819, -0.355], [1.0, 0.97, 0.92], 100_000.0),
            vec![lambert([0.4; 3]), lambert([0.55, 0.52, 0.48])],
            meshes,
        ),
        bake_bounds: ([-7.5, 0.2, -1.3], [7.5, 2.8, 1.3]),
        bake_spacing: 0.75,
        ddgi: plate_ddgi(spacing, dims),
        scale: None,
    }
}

/// Eye of [`shaft`].
const SHAFT_EYE: [f32; 3] = [0.8, -6.5, 0.8];

/// A square shaft 3 × 3 m and 8 m deep (20 cm walls and floor) through a large ground, the camera near its
/// bottom looking up at a wall. A 100 klx sun 60° high lights the −z wall from the top down to about 5 m;
/// an 8 klx sky. The bottom is lit only by light bounced down the shaft: the lab analog of the `specs/27`
/// § 2.0 set_piece_borehole plate (GI in sun shadow read 1.63 × the path tracer there).
fn shaft() -> GiScene {
    let (t, half, depth) = (0.1f32, 1.5f32, 8.0f32);
    let meshes = vec![
        // The ground around the 3 × 3 m hole, its top at y = 0.
        block([0.0, -0.25, 16.5], [30.0, 0.25, 15.0], 0),
        block([0.0, -0.25, -16.5], [30.0, 0.25, 15.0], 0),
        block([16.5, -0.25, 0.0], [15.0, 0.25, half], 0),
        block([-16.5, -0.25, 0.0], [15.0, 0.25, half], 0),
        // Walls and floor, overlapping at the corners.
        block([half + t, -depth / 2.0, 0.0], [t, depth / 2.0, half + 2.0 * t], 1),
        block(
            [-half - t, -depth / 2.0, 0.0],
            [t, depth / 2.0, half + 2.0 * t],
            1,
        ),
        block([0.0, -depth / 2.0, half + t], [half, depth / 2.0, t], 1),
        block([0.0, -depth / 2.0, -half - t], [half, depth / 2.0, t], 1),
        block([0.0, -depth - t, 0.0], [half + 2.0 * t, t, half + 2.0 * t], 1),
    ];
    let spacing = 0.75;
    let dims = plate_dims(SHAFT_EYE, spacing, [-half, -depth, -half], [half, 0.0, half]);
    GiScene {
        name: "shaft",
        lab: lab(
            SHAFT_EYE,
            [-1.2, -3.5, -1.5],
            11.0,
            8_000.0,
            DirectionalLightUniform::new([-0.2, -0.866, -0.458], [1.0, 0.97, 0.92], 100_000.0),
            vec![lambert([0.4; 3]), lambert([0.5, 0.42, 0.34])],
            meshes,
        ),
        bake_bounds: ([-1.3, -7.8, -1.3], [1.3, -0.2, 1.3]),
        bake_spacing: 0.75,
        ddgi: plate_ddgi(spacing, dims),
        scale: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------------------------

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("GI acceptance needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

fn adapter_label() -> String {
    let info = device().adapter.get_info();
    format!("{} ({}; {})", info.name, info.driver, info.driver_info)
}

/// Desktop High as `gi_validation` runs it: native scale, no fog, clouds or reflections, every GI
/// technique off. `held`: keep the static-screen cache (on in Desktop High); off, the frame takes the
/// cascaded sun shadows instead of the paged ones (`FrameQuality::reuse_static_screen`).
fn base_quality(held: bool) -> FrameQuality {
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
    q.paged_sun_shadows = held;
    q
}

/// The Desktop High GI technique: probes and the screen-space pass.
fn gi_quality(held: bool) -> FrameQuality {
    let mut q = FrameQuality {
        gi: GiMode::Probes,
        gi_probes: true,
        screen_space_gi: true,
        ..base_quality(held)
    };
    assert_eq!(
        q.reuse_static_screen(),
        held,
        "the static-screen cache follows `held`"
    );
    // Measurement switches ([`SSGI_ENV`], [`SSAO_ENV`]): unset, the frame is Desktop High's.
    if env_u32(SSGI_ENV, 1) == 0 {
        q.screen_space_gi = false;
    }
    if env_u32(SSAO_ENV, 1) == 0 {
        q.ssao = SsaoMode::Off;
    }
    q
}

/// A probe configuration: its name, the probes, the frames before the capture, the cache state.
#[derive(Clone, Debug)]
struct Config {
    name: &'static str,
    ddgi: DdgiSettings,
    frames: u32,
    held: bool,
    /// Score the drift from N to 2N frames (off for the long shipped-divisor runs).
    drift: bool,
}

/// The High tier's layout before ADR-5001 (every probe re-traced each frame): `DdgiSettings::default`'s 2 m finest
/// cascade, × 3, without ADR-2605's options. The control for the High tier's finer layout.
fn high_before() -> DdgiSettings {
    DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        update_divisor: 1,
        ..DdgiSettings::default()
    }
}

/// Frames of an unchanged scene after which the renderer stops a layout's probe update
/// (`DdgiSettings::freeze_frames`, ADR-5001): 512 updates of a finest probe at the layout's divisor, at least 600
/// frames.
fn freeze_frames(s: &DdgiSettings) -> u32 {
    512u32
        .saturating_mul(s.update_divisor.clamp(1, 8192))
        .max(600)
        .max(s.young_settle_frames())
}

/// Frames of a shipped tier at its own update divisor: past the renderer's probe freeze ([`freeze_frames`]), so
/// the capture is the field a player standing still keeps.
fn shipped_frames(s: &DdgiSettings) -> u32 {
    freeze_frames(s) + 64
}

/// A shipped tier exactly as it ships (its own update divisor), bricks baked on the caller.
fn shipped_ddgi(tier: DynamicGiTier) -> DdgiSettings {
    let mut s = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    tier.apply(&mut s);
    s
}

/// A shipped tier's probe layout with every probe re-traced each frame (as `dynamic_gi`'s
/// `tier_settings`), so a fixed frame count converges; the hysteresis stays the tier's.
fn tier_ddgi(tier: DynamicGiTier) -> DdgiSettings {
    let mut s = DdgiSettings {
        bake_on_caller: true,
        min_proxy_radius: 0.2,
        ..DdgiSettings::default()
    };
    tier.apply(&mut s);
    s.update_divisor = 1;
    s
}

/// The configurations of ADR-5938's and ADR-7870's measurements ([`configs`] runs them only when
/// `REGIA_GI_CONFIGS` lists them).
const LOW_DENSE_CONFIGS: [&str; 8] = [
    "low_dense_shipped",
    "low_dense_300",
    "low_dense_1200",
    "low_shipped_300",
    "low_shipped_1200",
    "low_dense_young_shipped",
    "low_dense_young_300",
    "low_dense_young_1200",
];

/// The young-probe budget of the `low_dense_young_*` configurations (ADR-7870): `REGIA_GI_YOUNG_BUDGET`, default
/// [`LOW_DENSE_YOUNG_BUDGET`].
const YOUNG_BUDGET_ENV: &str = "REGIA_GI_YOUNG_BUDGET";
/// Default of [`YOUNG_BUDGET_ENV`]: young probes a frame (of the candidate's 19 200). The largest budget measured
/// whose every start-up frame stays under the shipped Low tier's on the Steam Deck (ADR-7870: 22.5 ms worst).
const LOW_DENSE_YOUNG_BUDGET: u32 = 3072;

/// [`low_dense_ddgi`] with a young-probe budget (ADR-7870, `DdgiSettings::young_budget`): a start-up or a light cut
/// traces at most that many young probes a frame.
fn low_dense_young_ddgi() -> DdgiSettings {
    DdgiSettings {
        young_budget: env_u32(YOUNG_BUDGET_ENV, LOW_DENSE_YOUNG_BUDGET),
        ..low_dense_ddgi()
    }
}

/// Update divisor of [`low_dense_ddgi`]: an eighth of High's update rate.
const LOW_DENSE_DIVISOR: u32 = 64;

/// A measured candidate for the Low tier (ADR-5938; not shipped): High's probe grid (1 m, × 3.8, 20 × 12 × 20) with
/// Low's brick budget and screen lookup region, each settled probe re-traced an eighth as often as High's
/// ([`LOW_DENSE_DIVISOR`]). That traces 9 000 rays a settled frame where Low's 2 m grid traces 8 640, in 60.1 MB of
/// probe buffers for Low's 14.4 MB.
fn low_dense_ddgi() -> DdgiSettings {
    let low = shipped_ddgi(DynamicGiTier::Low);
    DdgiSettings {
        update_divisor: LOW_DENSE_DIVISOR,
        max_instances_per_cascade: low.max_instances_per_cascade,
        sdf_pool_bytes: low.sdf_pool_bytes,
        screen_max: low.screen_max,
        ..shipped_ddgi(DynamicGiTier::High)
    }
}

/// Whether `name` is in the comma list of `env` (every name when unset or empty).
fn selected(env: &str, name: &str) -> bool {
    match regia_core::brand::env_var_os(env).and_then(|v| v.into_string().ok()) {
        Some(list) if !list.trim().is_empty() => list.split(',').any(|n| n.trim() == name),
        _ => true,
    }
}

fn tier_frames() -> u32 {
    env_u32(TIER_FRAMES_ENV, TIER_FRAMES).max(STABILITY_FRAMES + 2)
}

/// The `x` configuration ([`X_ENV`]); `None` when the variable is unset. Keys: `tier`, `spacing`, `ratio`, `dims`
/// (`12x8x12`), `cascades`, `headroom`, `edge`, `front` (0 or 1), `divisor`, `hysteresis`, `normal_bias`,
/// `view_bias`, `min_frontface`, `visibility_limit`, `seed`, `recent`, `young` (the young-probe budget,
/// ADR-7870), `frames`.
fn x_config(tier_frames: u32) -> Option<Config> {
    let text = regia_core::brand::env_var_os(X_ENV).and_then(|v| v.into_string().ok())?;
    let mut ddgi = tier_ddgi(DynamicGiTier::Low);
    let mut frames = tier_frames;
    for pair in text.split(';').filter(|p| !p.trim().is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (key.trim(), value.trim());
        let num = value.parse::<f32>().unwrap_or(f32::NAN);
        match key {
            "tier" => {
                let tier =
                    DynamicGiTier::parse(value).unwrap_or_else(|| panic!("{X_ENV}: unknown tier {value:?}"));
                ddgi = tier_ddgi(tier);
            }
            "spacing" => ddgi.spacing = num,
            "ratio" => ddgi.spacing_ratio = num,
            "dims" => {
                let mut axes = value.split('x').filter_map(|v| v.trim().parse::<u32>().ok());
                match (axes.next(), axes.next(), axes.next()) {
                    (Some(x), Some(y), Some(z)) => ddgi.dims = [x, y, z],
                    _ => panic!("{X_ENV}: dims takes AxBxC, got {value:?}"),
                }
            }
            "cascades" => ddgi.cascades = num as u32,
            "headroom" => ddgi.finest_headroom_m = num,
            "edge" => ddgi.edge_visibility = num != 0.0,
            "front" => ddgi.front_weights = num != 0.0,
            "divisor" => ddgi.update_divisor = num as u32,
            "hysteresis" => ddgi.hysteresis = num,
            "normal_bias" => ddgi.normal_bias = num,
            "view_bias" => ddgi.view_bias = num,
            "min_frontface" => ddgi.min_frontface = num,
            "visibility_limit" => ddgi.visibility_limit = num,
            "seed" => ddgi.seed_updates = num as u32,
            "recent" => ddgi.recent_weight = num as u32,
            "young" => ddgi.young_budget = num as u32,
            "frames" => frames = num as u32,
            other => panic!("{X_ENV}: unknown key {other:?}"),
        }
    }
    Some(Config {
        name: "x",
        ddgi,
        frames,
        held: false,
        drift: true,
    })
}

fn configs(scene: &GiScene) -> Vec<Config> {
    let tier_frames = tier_frames();
    let mut all = vec![
        Config {
            name: "fine",
            ddgi: scene.ddgi.clone(),
            frames: PLATE_FRAMES,
            held: false,
            drift: true,
        },
        // The plate's layout with the tiers' lookup (ADR-5374: probes weighted at the surface point; it rides
        // `visibility_blend`). The plate layouts themselves keep the defaults `gi_validation` records.
        Config {
            name: "fine_surface",
            ddgi: DdgiSettings {
                visibility_blend: true,
                ..scene.ddgi.clone()
            },
            frames: PLATE_FRAMES,
            held: false,
            drift: true,
        },
        Config {
            name: "fine_held",
            ddgi: scene.ddgi.clone(),
            frames: PLATE_FRAMES,
            held: true,
            drift: false,
        },
        Config {
            name: "low",
            ddgi: tier_ddgi(DynamicGiTier::Low),
            frames: tier_frames,
            held: false,
            drift: true,
        },
        // Measurement (ADR-5566, ADR-5716): each tier without its front weights and edge visibility (the tier
        // before those ADRs, the control of the `low`, `medium` and `high` rows), and Medium as it ships.
        Config {
            name: "low_plain",
            ddgi: DdgiSettings {
                front_weights: false,
                edge_visibility: false,
                ..tier_ddgi(DynamicGiTier::Low)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        // Measurement (ADR-5796): what a finer finest cascade would buy the Low tier. `low_15m` keeps Low's probe
        // count, rays and bytes (12 × 8 × 12) at 1.5 m, × 3.3 (1.5, 4.95, 16.3 and 53.9 m: the same reach), so its
        // finest window is a quarter shorter in metres; `low_15m_wide` adds probes to hold the window (14 × 8 × 14,
        // 36 % more); `low_1m` is High's spacing on Low's grid, the far end of the trade.
        Config {
            name: "low_15m",
            ddgi: DdgiSettings {
                spacing: 1.5,
                spacing_ratio: 3.3,
                ..tier_ddgi(DynamicGiTier::Low)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "low_15m_wide",
            ddgi: DdgiSettings {
                spacing: 1.5,
                spacing_ratio: 3.3,
                dims: [14, 8, 14],
                ..tier_ddgi(DynamicGiTier::Low)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "low_1m",
            ddgi: DdgiSettings {
                spacing: 1.0,
                spacing_ratio: 3.8,
                ..tier_ddgi(DynamicGiTier::Low)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        // The control for `low_15m`: 1.5 m probes stand on the walls of the 3 m and 6 m scenes (door room, tunnel,
        // shaft), 1.7 m probes (× 3.17: 1.7, 5.4, 17.1 and 54.2 m) fall where the walls are not, as 2 m probes do.
        Config {
            name: "low_17m",
            ddgi: DdgiSettings {
                spacing: 1.7,
                spacing_ratio: 3.17,
                ..tier_ddgi(DynamicGiTier::Low)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "medium",
            ddgi: tier_ddgi(DynamicGiTier::Medium),
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "medium_plain",
            ddgi: DdgiSettings {
                front_weights: false,
                edge_visibility: false,
                ..tier_ddgi(DynamicGiTier::Medium)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "high_plain",
            ddgi: DdgiSettings {
                front_weights: false,
                edge_visibility: false,
                ..tier_ddgi(DynamicGiTier::High)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "high",
            ddgi: tier_ddgi(DynamicGiTier::High),
            frames: tier_frames,
            held: false,
            drift: true,
        },
        // Measurement (ADR-5457): High with the finest window's spare height over the eye (4.7 m over, 1.8 m under),
        // what a view up a shaft would read if the window followed the view.
        Config {
            name: "high_head47",
            ddgi: DdgiSettings {
                finest_headroom_m: 4.7,
                ..tier_ddgi(DynamicGiTier::High)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "high_15m",
            ddgi: DdgiSettings {
                spacing: 1.5,
                spacing_ratio: 3.3,
                ..tier_ddgi(DynamicGiTier::High)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        // ADR-5290 candidates (not shipped): finer finest cascades with the same window over and under the eye and
        // the same 549 m reach, to size what a finer High layout would recover.
        Config {
            name: "high_075m",
            ddgi: DdgiSettings {
                spacing: 0.75,
                spacing_ratio: 4.18,
                dims: [20, 14, 20],
                ..tier_ddgi(DynamicGiTier::High)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "high_05m",
            ddgi: DdgiSettings {
                spacing: 0.5,
                spacing_ratio: 4.79,
                dims: [20, 18, 20],
                ..tier_ddgi(DynamicGiTier::High)
            },
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "high_2m",
            ddgi: high_before(),
            frames: tier_frames,
            held: false,
            drift: true,
        },
        Config {
            name: "low_shipped",
            ddgi: shipped_ddgi(DynamicGiTier::Low),
            frames: shipped_frames(&shipped_ddgi(DynamicGiTier::Low)),
            held: false,
            drift: false,
        },
        Config {
            name: "high_shipped",
            ddgi: shipped_ddgi(DynamicGiTier::High),
            frames: shipped_frames(&shipped_ddgi(DynamicGiTier::High)),
            held: false,
            drift: false,
        },
        // Measurement (ADR-5938): the Low tier on High's grid at Low's ray budget, frozen at its own divisor, and
        // the same field 300 and 1 200 frames after a cold start (7.5 and 30 s at the Deck's 40 frames a second)
        // beside Low's own layout at the same age.
        Config {
            name: "low_dense_shipped",
            ddgi: low_dense_ddgi(),
            frames: shipped_frames(&low_dense_ddgi()),
            held: false,
            drift: false,
        },
        Config {
            name: "low_dense_300",
            ddgi: low_dense_ddgi(),
            frames: 300,
            held: false,
            drift: false,
        },
        Config {
            name: "low_dense_1200",
            ddgi: low_dense_ddgi(),
            frames: 1200,
            held: false,
            drift: false,
        },
        Config {
            name: "low_shipped_300",
            ddgi: shipped_ddgi(DynamicGiTier::Low),
            frames: 300,
            held: false,
            drift: false,
        },
        Config {
            name: "low_shipped_1200",
            ddgi: shipped_ddgi(DynamicGiTier::Low),
            frames: 1200,
            held: false,
            drift: false,
        },
        // Measurement (ADR-7870): the same candidate with a young-probe budget, frozen and at the same ages.
        Config {
            name: "low_dense_young_shipped",
            ddgi: low_dense_young_ddgi(),
            frames: shipped_frames(&low_dense_young_ddgi()),
            held: false,
            drift: false,
        },
        Config {
            name: "low_dense_young_300",
            ddgi: low_dense_young_ddgi(),
            frames: 300,
            held: false,
            drift: false,
        },
        Config {
            name: "low_dense_young_1200",
            ddgi: low_dense_young_ddgi(),
            frames: 1200,
            held: false,
            drift: false,
        },
    ];
    all.extend(x_config(tier_frames));
    // ADR-5938's rows run only when `REGIA_GI_CONFIGS` names them: `low_dense_shipped` renders 32 832 frames a
    // scene, which a card does in seconds and lavapipe does not.
    let listed = regia_core::brand::env_var_os(CONFIGS_ENV).is_some_and(|v| !v.is_empty());
    all.into_iter()
        .filter(|c| selected(CONFIGS_ENV, c.name) && (listed || !LOW_DENSE_CONFIGS.contains(&c.name)))
        .collect()
}

fn renderer(ddgi: &DdgiSettings, probes: bool) -> FrameRenderer {
    let mut r = FrameRenderer::new();
    r.set_ddgi_settings(ddgi.clone());
    r.set_ddgi_enabled(Some(probes));
    r
}

/// Renders `frames` more frames and returns the last one's linear HDR and its display pixels (RGBA8).
fn capture(
    r: &mut FrameRenderer,
    lab: &LabScene,
    quality: FrameQuality,
    frames: u32,
) -> (Vec<[f32; 3]>, Vec<u8>) {
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for i in 0..frames {
        if i + 1 == frames {
            r.request_hdr_capture();
        }
        pixels = r.render_offscreen(device(), &frame).expect("frame").pixels;
    }
    let hdr = r
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (lab.width, lab.height));
    (hdr.rgb, pixels)
}

/// [`capture`] with the sun's illuminance nudged by a negligible alternating amount each frame (±0.01 lux of
/// 100 klx), so the probes' scene key changes every frame and the field never reaches the renderer's freeze
/// ([`freeze_frames`] unchanged frames): the control that tells a frozen field from a converged one.
fn capture_awake(r: &mut FrameRenderer, lab: &LabScene, quality: FrameQuality, frames: u32) -> Vec<[f32; 3]> {
    let base = lab.directional.illuminance;
    for i in 0..frames {
        let mut l = lab.clone();
        l.directional.illuminance = base + if i % 2 == 0 { 0.01 } else { -0.01 };
        let frame = l.as_frame_scene_with_quality(quality);
        if i + 1 == frames {
            r.request_hdr_capture();
        }
        r.render_offscreen(device(), &frame).expect("frame");
    }
    r.take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy")
        .rgb
}

/// The frame's direct light alone (sun with shadows; no sky, GI or AO), at the cache state's shadows.
fn realtime_direct(lab: &LabScene, held: bool) -> Vec<[f32; 3]> {
    let mut lab = lab.clone();
    lab.ambient_lux = 0.0;
    let q = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality(held)
    };
    capture(
        &mut renderer(&DdgiSettings::default(), false),
        &lab,
        q,
        STILL_FRAMES,
    )
    .0
}

fn reference(lab: &LabScene, spp: u32, max_bounces: u32) -> PtImage {
    reference_seeded(lab, spp, max_bounces, 23)
}

/// [`reference`] with its own sample seed (two seeds measure the reference's own noise).
fn reference_seeded(lab: &LabScene, spp: u32, max_bounces: u32, seed: u32) -> PtImage {
    let frame = lab.as_frame_scene_with_quality(base_quality(true));
    let pt = from_frame(&frame, &ExtractOptions::default());
    tracer()
        .render(
            &pt,
            &PtSettings {
                spp,
                max_bounces,
                seed,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference")
}

/// Mean probe coverage over `mask` (the screen lookup's nearest texel; half resolution).
fn mean_coverage(r: &mut FrameRenderer, lab: &LabScene, mask: &[bool]) -> Option<f32> {
    let rb = r.ddgi_read_back(device()).ok()??;
    let (tw, th, texels) = rb.screen_irradiance()?;
    let (w, h) = (lab.width as usize, lab.height as usize);
    let (tw, th) = (tw as usize, th as usize);
    let (mut sum, mut n) = (0.0f64, 0usize);
    for (i, _) in mask.iter().enumerate().filter(|(_, m)| **m) {
        let (x, y) = (i % w, i / w);
        sum += f64::from(
            texels[(y * th / h).min(th - 1) * tw + (x * tw / w).min(tw - 1)]
                .coverage
                .clamp(0.0, 1.0),
        );
        n += 1;
    }
    (n > 0).then(|| (sum / n as f64) as f32)
}

/// Mean and largest frame-to-frame FLIP of the display frames over `frames` more frames.
fn stability(r: &mut FrameRenderer, lab: &LabScene, quality: FrameQuality, frames: u32) -> (f32, f32) {
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut last = r.render_offscreen(device(), &frame).expect("frame").pixels;
    let (mut sum, mut max) = (0.0f32, 0.0f32);
    for _ in 0..frames {
        let next = r.render_offscreen(device(), &frame).expect("frame").pixels;
        let (report, _) = compare_srgb8(&last, &next, lab.width, lab.height, DEFAULT_PPD);
        sum += report.flip_mean;
        max = max.max(report.flip_mean);
        last = next;
    }
    (sum / frames.max(1) as f32, max)
}

/// The masks and the reference indirect light of a scene.
struct Truth {
    surface: Vec<bool>,
    shadow: Vec<bool>,
    indirect: Vec<[f32; 3]>,
    image: PtImage,
}

fn truth(lab: &LabScene, spp: u32, max_bounces: u32) -> Truth {
    let image = reference(lab, spp, max_bounces);
    let surface = surface_mask(&image.depth, lab.width, lab.height, 0.05);
    let shadow = shadow_mask(&surface, &image.shadow, &image.direct, 0.05);
    let indirect = add_planes(&image.indirect_diffuse, &image.indirect_specular);
    Truth {
        surface,
        shadow,
        indirect,
        image,
    }
}

fn score(scene: &GiScene, truth: &Truth, reference: &[[f32; 3]], test: &[[f32; 3]]) -> GiError {
    measure(&GiErrorInput {
        reference,
        test,
        surface: &truth.surface,
        shadow: &truth.shadow,
        width: scene.lab.width,
        height: scene.lab.height,
        ev100: scene.lab.exposure_ev,
        scale: scene.scale,
    })
}

fn out_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(regia_core::brand::env_var_os(OUT_ENV)?);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            eprintln!("{OUT_ENV}: {e}");
            None
        }
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    regia_core::brand::env_var_os(name)
        .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
        .unwrap_or(default)
}

/// Prints the section and writes it to `<out>/<file>.md`.
fn publish(file: &str, title: &str, body: &str) {
    let text = format!("## {title}\n\nAdapter: {}\n\n{body}\n", adapter_label());
    eprintln!("{text}");
    if let Some(dir) = out_dir()
        && let Err(e) = regia_test_utils::write_atomic(&dir.join(format!("{file}.md")), text.as_bytes())
    {
        eprintln!("{OUT_ENV}: {e}");
    }
}

fn is_cost_card() -> bool {
    let name = device().adapter.get_info().name.to_ascii_lowercase();
    name.contains("5080") || name.contains("7900")
}

// ---------------------------------------------------------------------------------------------
// The measurements.
// ---------------------------------------------------------------------------------------------

/// The scene moved by `d` metres ([`SHIFT_ENV`]): every mesh, the eye and its target.
fn shifted(mut scene: GiScene, d: [f32; 3]) -> GiScene {
    for mesh in &mut scene.lab.meshes {
        for (axis, delta) in d.iter().enumerate() {
            mesh.model[3][axis] += delta;
        }
    }
    for (axis, delta) in d.iter().enumerate() {
        scene.lab.eye[axis] += delta;
        scene.lab.target[axis] += delta;
    }
    scene
}

/// [`SHIFT_ENV`] in metres; zero when unset, and for a component that does not parse.
fn shift_m() -> [f32; 3] {
    let mut d = [0.0f32; 3];
    if let Some(text) = regia_core::brand::env_var_os(SHIFT_ENV).and_then(|v| v.into_string().ok()) {
        for (out, part) in d.iter_mut().zip(text.split(',')) {
            *out = part.trim().parse().unwrap_or(0.0);
        }
    }
    d
}

fn acceptance_scene_list() -> Vec<GiScene> {
    let shift = shift_m();
    vec![
        trench(),
        color_room(),
        sealed_box(),
        door_room(),
        tunnel(),
        shaft(),
    ]
    .into_iter()
    .filter(|s| selected(SCENES_ENV, s.name))
    .map(|s| shifted(s, shift))
    .collect()
}

/// The likely cause text of a FLIP or energy miss, from the row's signed numbers.
fn flip_cause(e: &GiError, config: &str) -> String {
    let energy = e.energy_ratio.unwrap_or(f32::NAN);
    let shadow = e.shadow_bias.map_or(f32::NAN, |b| 1.0 + b);
    let mut s = format!("energy {energy:.3}, in shadow {shadow:.3}×");
    if config == "fine_held" {
        s.push_str("; held view (early light, static-screen cache)");
    }
    if energy.is_finite() && energy < 0.95 {
        s.push_str("; under-lit: probe irradiance short (sun bounce, see decomposition)");
    } else if energy.is_finite() && energy > 1.05 {
        s.push_str("; over-lit: probe irradiance over the tracer (leak or coarse cascade)");
    } else {
        s.push_str("; energy near the tracer: error is spatial (probe spacing/screen gather noise)");
    }
    s
}

#[test]
#[ignore = "measurement: the J06 acceptance table on this adapter, asserts only that it ran"]
fn acceptance_scenes() {
    let mut rows: Vec<Row> = Vec::new();
    let mut detail = String::new();
    for scene in acceptance_scene_list() {
        let lab = &scene.lab;
        let truth = truth(lab, REFERENCE_SPP, REFERENCE_BOUNCES);
        let surface_px = truth.surface.iter().filter(|s| **s).count();
        assert!(
            surface_px * 10 >= truth.surface.len() * 3,
            "{}: 30 % of the frame is scored",
            scene.name
        );
        let direct_held = realtime_direct(lab, true);
        let direct_settled = realtime_direct(lab, false);
        let _ = writeln!(
            detail,
            "\n### {} ({} × {}, fine layout {:?} at {} m)\n\n| config | frames | mean FLIP | p95 | energy | in shadow × | rel RMSE | coverage | ftf FLIP mean / max | drift N→2N |\n|---|---|---|---|---|---|---|---|---|---|",
            scene.name, lab.width, lab.height, scene.ddgi.dims, scene.ddgi.spacing
        );
        // The no-GI control's stability: the frame without probes or the screen pass.
        let (ctl_mean, ctl_max) = {
            let mut r = renderer(&DdgiSettings::default(), false);
            let q = base_quality(false);
            capture(&mut r, lab, q, STILL_FRAMES);
            stability(&mut r, lab, q, STABILITY_FRAMES)
        };
        for config in configs(&scene) {
            let q = gi_quality(config.held);
            let mut r = renderer(&config.ddgi, true);
            let (full, _) = capture(&mut r, lab, q, config.frames);
            assert!(
                r.ddgi_active(),
                "{} / {}: the probes lit the frame",
                scene.name,
                config.name
            );
            let direct = if config.held {
                &direct_held
            } else {
                &direct_settled
            };
            let test = sub_planes(&full, direct);
            let e = score(&scene, &truth, &truth.indirect, &test);
            assert!(
                e.flip_mean.is_finite() && e.pixels == surface_px,
                "{}: {e:?}",
                scene.name
            );
            let coverage = mean_coverage(&mut r, lab, &truth.surface).unwrap_or(f32::NAN);
            let (ftf_mean, ftf_max, drift) = if config.held {
                (f32::NAN, f32::NAN, f32::NAN)
            } else if !config.drift {
                let (m, x) = stability(&mut r, lab, q, STABILITY_FRAMES);
                (m, x, f32::NAN)
            } else {
                let (m, x) = stability(&mut r, lab, q, STABILITY_FRAMES);
                // Drift: continue to twice the frames (the stability frames count) and capture again.
                let more = config.frames.saturating_sub(STABILITY_FRAMES + 1).max(1);
                let (later, _) = capture(&mut r, lab, q, more);
                let later = sub_planes(&later, direct);
                let e2 = score(&scene, &truth, &truth.indirect, &later);
                (m, x, e2.mean_test / e.mean_test.max(1e-6) - 1.0)
            };
            let _ = writeln!(
                detail,
                "| {} | {} | {:.4} | {:.4} | {} | {} | {:.4} | {coverage:.3} | {} / {} | {} |",
                config.name,
                config.frames,
                e.flip_mean,
                e.flip_p95,
                e.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
                e.shadow_bias
                    .map_or("-".to_owned(), |b| format!("{:.4}", 1.0 + b)),
                e.rel_rmse,
                fmt_value(ftf_mean),
                fmt_value(ftf_max),
                fmt_value(drift),
            );
            let settled = r.ddgi_settled_frames();
            let tag = if settled >= freeze_frames(&config.ddgi) && settled != u32::MAX {
                format!("{} / {} (probes frozen)", scene.name, config.name)
            } else {
                format!("{} / {}", scene.name, config.name)
            };
            if let Some(ground) = scene.scale {
                let leak = e.mean_test / ground;
                rows.push(
                    Row::new(
                        format!("{tag}: leak, interior / lit ground"),
                        Bar::AtMost(LEAK_TARGET * 100.0),
                        leak * 100.0,
                        " %",
                    )
                    .note(if leak <= LEAK_TARGET {
                        "closed box stays dark".to_owned()
                    } else {
                        if drift < -0.2 {
                            format!(
                                "still decaying at N ({:.0} % by 2N): a transient, see `tier_leak`'s curve",
                                drift * 100.0
                            )
                        } else {
                            "light inside a closed box: probe visibility / cascade reach".to_owned()
                        }
                    }),
                );
            } else {
                let cause = flip_cause(&e, config.name);
                rows.push(
                    Row::new(
                        format!("{tag}: mean FLIP"),
                        Bar::AtMost(FLIP_TARGET),
                        e.flip_mean,
                        "",
                    )
                    .note(cause.clone()),
                );
                rows.push(
                    Row::new(
                        format!("{tag}: p95 FLIP"),
                        Bar::AtMost(FLIP_P95_TARGET),
                        e.flip_p95,
                        "",
                    )
                    .note(cause),
                );
                rows.push(
                    Row::new(
                        format!("{tag}: GI in sun shadow / tracer"),
                        Bar::Report,
                        e.shadow_bias.map_or(f32::NAN, |b| 1.0 + b),
                        "×",
                    )
                    .note("specs/27 § 2.0 metric (tunnel_interior 2.56×, borehole 1.63× on DO27 maps)"),
                );
            }
            rows.push(
                Row::new(
                    format!("{tag}: probe coverage of the view"),
                    Bar::AtLeast(COVERAGE_TARGET),
                    coverage,
                    "",
                )
                .note(if coverage >= COVERAGE_TARGET {
                    String::new()
                } else {
                    "pixels outside the finest full-weight window read the flat sky term".to_owned()
                }),
            );
            if !config.held {
                let note = format!(
                    "no-GI control {} (max {}); {}",
                    fmt_value(ctl_mean),
                    fmt_value(ctl_max),
                    if ftf_mean > STABILITY_TARGET && ctl_mean <= STABILITY_TARGET {
                        "GI adds the flicker (per-frame ray rotation / screen gather)"
                    } else if ftf_mean > STABILITY_TARGET {
                        "the frame without GI flickers too: not GI"
                    } else {
                        "stable"
                    }
                );
                rows.push(
                    Row::new(
                        format!("{tag}: frame-to-frame FLIP after convergence"),
                        Bar::AtMost(STABILITY_TARGET),
                        ftf_mean,
                        "",
                    )
                    .note(note),
                );
                if config.drift {
                    rows.push(
                        Row::new(
                            format!("{tag}: indirect drift N→2N frames"),
                            Bar::Report,
                            drift * 100.0,
                            " %",
                        )
                        .note("not converged at N when large"),
                    );
                }
            }
        }
    }
    let body = format!(
        "Bars: `specs/27` § 2.2 (FLIP {FLIP_TARGET}, p95 {FLIP_P95_TARGET}, leak {} %), § 2.3 stability {STABILITY_TARGET}, \
         coverage {COVERAGE_TARGET}. Reference: path tracer {REFERENCE_SPP} spp, {REFERENCE_BOUNCES} bounces, indirect diffuse + specular AOVs. \
         Configs: `fine` = the plate's own layout ({PLATE_FRAMES} frames, hysteresis 0.9); `fine_surface` = the same with \
         the tiers' lookup (ADR-5374, `visibility_blend`); `fine_held` = the plate's layout with the \
         static-screen cache on (paged sun shadows, the view a standing player sees); `low` / `high` = the shipped tier \
         layouts, every probe re-traced each frame, {} frames; `low_plain` / `medium_plain` / `high_plain` = the tier without its front \
         weights and edge visibility (the tier before ADR-5566 and ADR-5716); `medium` = the Medium tier, which has \
         Low's 2 m probes; `low_15m` / `low_15m_wide` / `low_1m` / `low_17m` = ADR-5796's measurement of a finer Low layout (1.5 m × 3.3 \
         on Low's own 12 × 8 × 12 grid, the same on 14 × 8 × 14, 1 m × 3.8 and 1.7 m × 3.17 on Low's grid; not shipped); `high_2m` = High's layout before ADR-5001 (2 m finest cascade, × 3, without \
         ADR-2605's options), every probe re-traced each frame; `high_15m` = High with a 1.5 m finest cascade (× 3.3), \
         the rejected ADR-5001 candidate that keeps a 6 m room dark; `high_075m` / `high_05m` = ADR-5290 candidates with \
         0.75 m (× 4.18, 14 layers) and 0.5 m (× 4.79, 18 layers) finest cascades, the same window and reach; `low_shipped` / `high_shipped` = the tiers at their own update divisors (16, \
         8), each to 64 frames past its probe freeze (512 updates a finest probe, ADR-5001): the field a player \
         standing still keeps; `low_dense_shipped` = ADR-5938's candidate for Low (High's grid at divisor 64, Low's ray budget; \
         not shipped) past its freeze, `low_dense_300` / `low_dense_1200` and `low_shipped_300` / `low_shipped_1200` = that \
         layout and Low's own 300 and 1 200 frames after a cold start, each at its own divisor (these five run only \
         when `REGIA_GI_CONFIGS` names them). Settled configs use cascaded sun shadows (the cache \
         needs paged ones); each config's direct frame uses the same shadows.\n\n{}\n### Per scene\n{detail}",
        LEAK_TARGET * 100.0,
        tier_frames(),
        render_rows(1, &rows)
    );
    publish("acceptance_scenes", "GI acceptance: scenes", &body);
}

/// Mean luminance over `mask`.
fn mean_lum(plane: &[[f32; 3]], mask: &[bool]) -> f32 {
    let (s, n) = plane
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| {
            (s + f64::from(luminance(*c)), n + 1)
        });
    (s / n.max(1) as f64) as f32
}

#[test]
#[ignore = "measurement: indirect light by light and bounce depth, asserts only that it ran"]
fn energy_decomposition() {
    let mut rows = Vec::new();
    let mut detail = String::from(
        "| scene | light | config | probes surface | PT6 surface | PT2 | PT1 | probes/PT6 | probes/PT2 | probes/PT1 | probes shadow | PT6 shadow | shadow ratio |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for scene in [trench(), color_room(), door_room(), tunnel(), shaft()]
        .into_iter()
        .filter(|s| selected(SCENES_ENV, s.name))
    {
        for light in ["sun", "sky"] {
            let mut lab = scene.lab.clone();
            if light == "sun" {
                lab.ambient_lux = 0.0;
            } else {
                lab.directional.illuminance = 0.0;
            }
            let t6 = truth(&lab, DECOMPOSITION_SPP, 6);
            let t2 = reference(&lab, DECOMPOSITION_SPP, 2);
            let t1 = reference(&lab, DECOMPOSITION_SPP, 1);
            let pt2 = add_planes(&t2.indirect_diffuse, &t2.indirect_specular);
            let pt1 = add_planes(&t1.indirect_diffuse, &t1.indirect_specular);
            // The direct frame: the sun alone, or nothing (sky only has no direct light; the frame's
            // sky term is indirect here, as in the tracer's AOVs).
            let direct = realtime_direct(&lab, false);
            for (cname, ddgi, frames) in [
                ("fine", scene.ddgi.clone(), PLATE_FRAMES),
                ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
            ] {
                if !selected(CONFIGS_ENV, cname) {
                    continue;
                }
                let mut r = renderer(&ddgi, true);
                let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
                assert!(r.ddgi_active());
                let test = sub_planes(&full, &direct);
                let m = [
                    mean_lum(&test, &t6.surface),
                    mean_lum(&t6.indirect, &t6.surface),
                    mean_lum(&pt2, &t6.surface),
                    mean_lum(&pt1, &t6.surface),
                    mean_lum(&test, &t6.shadow),
                    mean_lum(&t6.indirect, &t6.shadow),
                ];
                assert!(m.iter().all(|v| v.is_finite()));
                let ratio = |a: f32, b: f32| if b > 1e-3 { a / b } else { f32::NAN };
                let _ = writeln!(
                    detail,
                    "| {} | {light} | {cname} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3} | {:.3} | {:.3} | {:.1} | {:.1} | {:.3} |",
                    scene.name,
                    m[0],
                    m[1],
                    m[2],
                    m[3],
                    ratio(m[0], m[1]),
                    ratio(m[0], m[2]),
                    ratio(m[0], m[3]),
                    m[4],
                    m[5],
                    ratio(m[4], m[5])
                );
                let multi = ratio(m[1] - m[2], m[1]);
                rows.push(
                    Row::new(
                        format!(
                            "{} / {light} only / {cname}: indirect energy / tracer",
                            scene.name
                        ),
                        Bar::Report,
                        ratio(m[0], m[1]),
                        "×",
                    )
                    .note(format!(
                        "in shadow {:.3}×; tracer's light past `max_bounces` 2 (two or more bounces) {:.1} % of its indirect",
                        ratio(m[4], m[5]),
                        multi * 100.0
                    )),
                );
            }
        }
    }
    let body = format!(
        "The frame's indirect light (full − direct, settled, cache off) against the tracer at {DECOMPOSITION_SPP} spp with \
         1, 2 and 6 path bounces (`max_bounces`: PT1 holds no sun bounce, the sun's indirect light needs two; PT2 holds one bounce, PT6 the full chain). A probe deficit that matches PT1/PT2 better than PT6 is \
         missing multi-bounce energy; a sun-only deficit with a sky-only match is the probes' sun bounce (their trace's \
         direct light at hits); both short is a shared term (albedo, normalization).\n\n{}\n### Means (cd/m²)\n\n{detail}",
        render_rows(1, &rows)
    );
    publish(
        "energy_decomposition",
        "GI acceptance: energy decomposition",
        &body,
    );
}

/// The probes' ray hits shade as Lambertian (albedo × irradiance), the tracer's surfaces as the rough dielectric the
/// materials are (diffuse × (1 − F) plus a GGX lobe, F0 0.04). At grazing angles, down a long tunnel or a shaft, the
/// two differ most. This traces every scene's reference both ways (32 bounces) and scores the probes' indirect energy
/// against each (ADR-5338's tunnel lead): a deficit that closes against the Lambertian reference is the material
/// model, not the probes' transport.
#[test]
#[ignore = "measurement: the probes against a Lambertian and the material reference, asserts only that it ran"]
fn material_model() {
    let mut rows = Vec::new();
    for scene in [door_room(), tunnel(), shaft(), trench(), color_room()]
        .into_iter()
        .filter(|s| selected(SCENES_ENV, s.name))
    {
        let lab = scene.lab.clone();
        let t = truth(&lab, DECOMPOSITION_SPP, REFERENCE_BOUNCES);
        let frame = lab.as_frame_scene_with_quality(base_quality(true));
        let mut pt = from_frame(&frame, &ExtractOptions::default());
        for m in &mut pt.materials {
            m.lambert = true;
        }
        let lam = tracer()
            .render(
                &pt,
                &PtSettings {
                    spp: DECOMPOSITION_SPP,
                    max_bounces: REFERENCE_BOUNCES,
                    seed: 23,
                    jitter: false,
                    samples_per_submit: 8,
                    ..PtSettings::default()
                },
            )
            .expect("trace the Lambertian reference");
        let lam_indirect = add_planes(&lam.indirect_diffuse, &lam.indirect_specular);
        let pbr = mean_lum(&t.indirect, &t.surface);
        let lambert = mean_lum(&lam_indirect, &t.surface);
        assert!(pbr.is_finite() && lambert.is_finite());
        rows.push(
            Row::new(
                format!(
                    "{} / Lambertian tracer / material tracer: indirect energy",
                    scene.name
                ),
                Bar::Report,
                lambert / pbr.max(1e-3),
                "×",
            )
            .note(format!("{lambert:.1} against {pbr:.1} cd/m² on the surfaces")),
        );
        let direct = realtime_direct(&lab, false);
        for (cname, ddgi, frames) in [
            ("fine", scene.ddgi.clone(), PLATE_FRAMES),
            ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
        ] {
            let mut r = renderer(&ddgi, true);
            let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
            assert!(r.ddgi_active());
            let probes = mean_lum(&sub_planes(&full, &direct), &t.surface);
            rows.push(
                Row::new(
                    format!("{} / {cname}: indirect energy / Lambertian tracer", scene.name),
                    Bar::Report,
                    probes / lambert.max(1e-3),
                    "×",
                )
                .note(format!(
                    "against the material tracer {:.3}×",
                    probes / pbr.max(1e-3)
                )),
            );
        }
    }
    publish(
        "material_model",
        "GI acceptance: the probes against a Lambertian and the material reference",
        &format!(
            "Full light, settled, static-screen cache off; the tracer at {DECOMPOSITION_SPP} spp, {REFERENCE_BOUNCES} \
             bounces, once with the scene's materials and once with every material Lambertian (`PtMaterial::lambert`). \
             The frame side is unchanged.\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

/// Frame counts at which `tier_leak` reads the sealed box: the leak's convergence curve.
const LEAK_CHECKPOINTS: [u32; 6] = [96, 192, 384, 768, 1536, 4160];

#[test]
#[ignore = "measurement: the sealed box at every shipped tier over time, asserts only that it ran"]
fn tier_leak() {
    let scene = sealed_box();
    let ground = scene.scale.expect("the leak scene names its scale");
    let truth = truth(&scene.lab, 256, 6);
    let direct = realtime_direct(&scene.lab, false);
    let mut rows = Vec::new();
    let mut curve = String::from("| tier | divisor |");
    for c in LEAK_CHECKPOINTS {
        let _ = write!(curve, " {c} frames |");
    }
    curve.push_str("\n|---|---|");
    curve.push_str(&"---|".repeat(LEAK_CHECKPOINTS.len()));
    curve.push('\n');
    for tier in DynamicGiTier::ALL {
        let shipped = {
            let mut s = DdgiSettings::default();
            tier.apply(&mut s);
            s.update_divisor
        };
        let divisors = if shipped == 1 { vec![1] } else { vec![1, shipped] };
        // High and Ultra changed layout in ADR-5001; one more row keeps their layout from before (2 m, × 3, no
        // ADR-2605 options) as the control (divisor 0 in the list marks it).
        let opt_in = matches!(tier, DynamicGiTier::High | DynamicGiTier::Ultra);
        // Rows: every divisor; 0 = the layout before ADR-5001; `awake` = the shipped divisor kept from the freeze.
        let mut cases: Vec<(u32, bool)> = divisors.into_iter().map(|d| (d, false)).collect();
        if opt_in {
            cases.push((0, false));
        }
        if shipped > 1 {
            cases.push((shipped, true));
        }
        for (divisor, awake) in cases {
            let mut ddgi = DdgiSettings {
                update_divisor: divisor.max(1),
                ..tier_ddgi(tier)
            };
            if divisor == 0 {
                ddgi = DdgiSettings {
                    max_instances_per_cascade: ddgi.max_instances_per_cascade,
                    sdf_pool_bytes: ddgi.sdf_pool_bytes,
                    ..high_before()
                };
            }
            let mut r = renderer(&ddgi, true);
            let mut done = 0;
            let mut leaks = Vec::new();
            let mut settled = Vec::new();
            for at in LEAK_CHECKPOINTS {
                let full = if awake {
                    capture_awake(&mut r, &scene.lab, gi_quality(false), at - done)
                } else {
                    capture(&mut r, &scene.lab, gi_quality(false), at - done).0
                };
                done = at;
                assert!(r.ddgi_active());
                settled.push(r.ddgi_settled_frames());
                let leak = mean_lum(&sub_planes(&full, &direct), &truth.surface) / ground;
                assert!(leak.is_finite());
                leaks.push(leak);
            }
            let label = if divisor == 0 {
                format!("{} 2 m layout (before ADR-5001)", tier.name())
            } else if awake {
                format!("{} kept awake", tier.name())
            } else {
                tier.name().to_owned()
            };
            let _ = write!(curve, "| {label} | {} |", divisor.max(1));
            for (l, st) in leaks.iter().zip(&settled) {
                let _ = write!(curve, " {:.4} % ({st}) |", l * 100.0);
            }
            curve.push('\n');
            let last = leaks.last().copied().unwrap_or(f32::NAN);
            let first = leaks.first().copied().unwrap_or(f32::NAN);
            let freeze = freeze_frames(&ddgi);
            let frozen = settled.last().is_some_and(|s| *s >= freeze);
            let note = if last > LEAK_TARGET && frozen {
                format!(
                    "frozen field: unchanged {} frames (renderer freeze at {freeze}) after {} updates a probe; the \
                     transient leak is held",
                    settled.last().copied().unwrap_or(0),
                    freeze / divisor.max(1)
                )
            } else if last <= LEAK_TARGET && first > LEAK_TARGET {
                format!(
                    "transient: {:.3} % at {} frames, under the bar by the last read",
                    first * 100.0,
                    LEAK_CHECKPOINTS[0]
                )
            } else if last > LEAK_TARGET {
                "steady leak: probe visibility / a cascade wider than the box".to_owned()
            } else {
                String::new()
            };
            rows.push(
                Row::new(
                    format!(
                        "sealed_box / {label} (divisor {}): leak after {} frames",
                        divisor.max(1),
                        LEAK_CHECKPOINTS[LEAK_CHECKPOINTS.len() - 1]
                    ),
                    Bar::AtMost(LEAK_TARGET * 100.0),
                    last * 100.0,
                    " %",
                )
                .note(note),
            );
        }
    }
    publish(
        "tier_leak",
        "GI acceptance: sealed box per tier",
        &format!(
            "Mean interior indirect luminance over the lit exterior ground ({ground:.0} cd/m²), static-screen cache off, \
             the 2 m box with 0.1 m walls of `gi_scenes::sealed_box` (smaller than one 2 m tier cell). Each tier with \
             every probe re-traced each frame (divisor 1), at its shipped divisor, and at its shipped divisor kept awake \
             (`capture_awake`: a ±0.01 lux sun nudge each frame keeps the probes' scene key changing, so the renderer's \
             probe freeze, 512 updates a finest probe since ADR-5001, never stops the update). Curve cells: leak (frames the probes' scene was unchanged).\n\n{}\n### Convergence\n\n{curve}",
            render_rows(1, &rows)
        ),
    );
}

/// The display frame's frame-to-frame FLIP split by technique: probes + screen pass, probes alone, the
/// screen pass alone, so a stability miss names its term.
#[test]
#[ignore = "measurement: frame-to-frame stability split by GI technique, asserts only that it ran"]
fn stability_split() {
    let mut rows = Vec::new();
    for scene in [door_room(), tunnel(), sealed_box(), trench()]
        .into_iter()
        .filter(|s| selected(SCENES_ENV, s.name))
    {
        let lab = &scene.lab;
        for (cname, ddgi, frames) in [
            ("fine", scene.ddgi.clone(), PLATE_FRAMES),
            ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
        ] {
            if !selected(CONFIGS_ENV, cname) {
                continue;
            }
            let both = gi_quality(false);
            let probes_only = FrameQuality {
                screen_space_gi: false,
                ..both
            };
            let screen_only = FrameQuality {
                gi: GiMode::ScreenSpace,
                gi_probes: false,
                ..both
            };
            let mut out = Vec::new();
            for (vname, q, probes) in [
                ("probes + screen", both, true),
                ("probes only", probes_only, true),
                ("screen only", screen_only, false),
            ] {
                let mut r = renderer(&ddgi, probes);
                capture(&mut r, lab, q, frames);
                let (mean, max) = stability(&mut r, lab, q, STABILITY_FRAMES);
                assert!(mean.is_finite());
                out.push((vname, mean, max));
            }
            let worst = out
                .iter()
                .skip(1)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or("", |w| w.0);
            for (vname, mean, max) in &out {
                rows.push(
                    Row::new(
                        format!("{} / {cname} / {vname}: frame-to-frame FLIP", scene.name),
                        Bar::AtMost(STABILITY_TARGET),
                        *mean,
                        "",
                    )
                    .note(if *vname == "probes + screen" {
                        format!("max {}; larger single term: {worst}", fmt_value(*max))
                    } else {
                        format!("max {}", fmt_value(*max))
                    }),
                );
            }
        }
    }
    publish(
        "stability_split",
        "GI acceptance: stability by technique",
        &format!(
            "Mean frame-to-frame FLIP of the display frame over {STABILITY_FRAMES} frames after convergence (`fine` \
             {PLATE_FRAMES} frames, `high` {} frames), static-screen cache off.\n\n{}",
            tier_frames(),
            render_rows(1, &rows)
        ),
    );
}

#[test]
#[ignore = "measurement: per-tier GPU cost at 1440p and 800p, asserts no time"]
fn tier_cost() {
    let frames = env_u32("DYNAMIC_GI_COST_FRAMES", 48);
    // Frames before the timed ones (default 32: past the probes' first sixteen updates, which trace every probe
    // each frame). With 0 and a short `DYNAMIC_GI_COST_FRAMES` the timed frames are that start-up load, the one a
    // light cut or a new volume also costs.
    let settle = env_u32("DYNAMIC_GI_COST_SETTLE", 32);
    // ADR-7870: `low_dense_young<B>` rows, ADR-5938's candidate with a young-probe budget of B, one per value of
    // `DYNAMIC_GI_COST_YOUNG_BUDGET` (a comma list; default 3072). `DYNAMIC_GI_COST_LAYOUTS` (a comma list of row
    // names) and `DYNAMIC_GI_COST_SIZES` (`1440p`, `800p`) keep a run on a slow device short.
    let young_budgets: Vec<u32> = regia_core::brand::env_var_os("DYNAMIC_GI_COST_YOUNG_BUDGET")
        .and_then(|v| v.into_string().ok())
        .map(|list| list.split(',').filter_map(|b| b.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![LOW_DENSE_YOUNG_BUDGET]);
    let mut rows = Vec::new();
    let mut detail = String::from(
        "| size | tier | divisor | young budget | gi ms | worst frame gi ms | trace | relocate | irradiance | distance | lookup | ssgi ms | frame GPU ms | create→first frame ms |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let card = is_cost_card();
    for (label, w, h) in [("1440p", 2560u32, 1440u32), ("800p", 1280, 800)] {
        if !selected("DYNAMIC_GI_COST_SIZES", label) {
            continue;
        }
        let mut scene = door_room();
        scene.lab.width = w;
        scene.lab.height = h;
        let q = gi_quality(false);
        // Every tier at its own divisor, then the `x` layout ([`X_ENV`]) at the divisor it names.
        let mut layouts: Vec<(String, Option<DynamicGiTier>, DdgiSettings)> = DynamicGiTier::ALL
            .into_iter()
            .map(|tier| {
                let mut ddgi = DdgiSettings {
                    bake_on_caller: true,
                    ..DdgiSettings::default()
                };
                tier.apply(&mut ddgi);
                (tier.name().to_owned(), Some(tier), ddgi)
            })
            .collect();
        // ADR-5938's candidate for Low (`bake_on_caller` as the tiers above set it).
        layouts.push(("low_dense".to_owned(), None, low_dense_ddgi()));
        for &budget in &young_budgets {
            layouts.push((
                format!("low_dense_young{budget}"),
                None,
                DdgiSettings {
                    young_budget: budget,
                    ..low_dense_ddgi()
                },
            ));
        }
        if let Some(x) = x_config(tier_frames()) {
            layouts.push(("x".to_owned(), None, x.ddgi));
        }
        for (name, tier, ddgi) in layouts {
            if !selected("DYNAMIC_GI_COST_LAYOUTS", &name) {
                continue;
            }
            let start = std::time::Instant::now();
            let mut r = renderer(&ddgi, true);
            r.set_gpu_timing(device(), true);
            let frame = scene.lab.as_frame_scene_with_quality(q);
            r.render_offscreen(device(), &frame).expect("frame");
            let first_ms = start.elapsed().as_secs_f32() * 1000.0;
            for _ in 0..settle {
                r.render_offscreen(device(), &frame).expect("frame");
            }
            let (mut gi, mut stages, mut ssgi, mut total) = (0.0f32, [0.0f32; 5], 0.0f32, 0.0f32);
            let mut worst = 0.0f32;
            for _ in 0..frames {
                r.render_offscreen(device(), &frame).expect("frame");
                let t = r.gpu_timings();
                gi += t.gi_ms;
                worst = worst.max(t.gi_ms);
                for (a, b) in stages.iter_mut().zip(t.gi_stages_ms) {
                    *a += b;
                }
                ssgi += t.ssgi_ms;
                total += t.frame_ms;
            }
            let n = frames.max(1) as f32;
            let (gi, ssgi, total) = (gi / n, ssgi / n, total / n);
            let stages = stages.map(|s| s / n);
            let _ = writeln!(
                detail,
                "| {label} | {name} | {} | {} | {gi:.3} | {worst:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {ssgi:.3} | {total:.3} | {first_ms:.0} |",
                ddgi.update_divisor, ddgi.young_budget, stages[0], stages[1], stages[2], stages[3], stages[4]
            );
            let bar = match (label, tier) {
                ("1440p", Some(DynamicGiTier::High)) if card => Bar::AtMost(HIGH_COST_MS),
                _ => Bar::Report,
            };
            let note = match (label, tier) {
                ("1440p", Some(DynamicGiTier::High)) => {
                    if card {
                        "specs/79 § 7 Desktop High bar (RX 7900 / RTX 5080)".to_owned()
                    } else {
                        format!(
                            "bar {HIGH_COST_MS} ms holds on the RX 7900 / RTX 5080 only; not extrapolated"
                        )
                    }
                }
                ("800p", Some(DynamicGiTier::Low)) => {
                    format!("Deck bar {DECK_COST_MS} ms holds on the Deck only; not run there")
                }
                (_, None) if name == "low_dense" => {
                    "ADR-5938's candidate for Low: High's grid at a 64th of its probes a frame".to_owned()
                }
                (_, None) if name.starts_with("low_dense_young") => format!(
                    "ADR-7870: the candidate with at most {} young probes traced a frame (worst frame {worst:.3} ms)",
                    ddgi.young_budget
                ),
                (_, None) => format!("the `{X_ENV}` layout at its own divisor"),
                _ => String::new(),
            };
            if !note.is_empty() {
                rows.push(
                    Row::new(format!("{label} / {name}: probe update GPU time"), bar, gi, " ms").note(note),
                );
            }
        }
    }
    publish(
        "tier_cost",
        "GI acceptance: per-tier cost",
        &format!(
            "Door room, Desktop High GI (probes + screen pass; static-screen cache off, so the screen pass runs every frame as it does under a moving camera), each tier at its own update divisor; {settle} frames to \
             settle, then the mean of {frames}. `create→first frame` is the CPU wall time from a new renderer to its first \
             frame (pipelines, probe buffers, bricks baked on the caller), printed only.\n\n{}\n{detail}",
            render_rows(1, &rows)
        ),
    );
}

/// The sun bounce's energy under other probe layouts: whether a deficit follows the probe spacing, the
/// cascade count or the history, at the same scene and frames.
#[test]
#[ignore = "measurement: sun-only indirect energy across probe layouts, asserts only that it ran"]
fn layout_sweep() {
    let mut rows = Vec::new();
    for scene in [door_room(), tunnel(), shaft(), trench()]
        .into_iter()
        .filter(|s| selected(SCENES_ENV, s.name))
    {
        let mut lab = scene.lab.clone();
        lab.ambient_lux = 0.0;
        let t = truth(&lab, DECOMPOSITION_SPP, 6);
        let reference = mean_lum(&t.indirect, &t.surface);
        let direct = realtime_direct(&lab, false);
        let (lo, hi) = ([-8.0f32, -8.0, -8.0], [8.0f32, 4.0, 8.0]);
        let mut layouts: Vec<(String, DdgiSettings, u32)> = Vec::new();
        for spacing in [0.5f32, 1.0, 2.0] {
            let dims = plate_dims(lab.eye, spacing, lo, hi);
            layouts.push((
                format!("{spacing} m × 2 cascades {dims:?}"),
                plate_ddgi(spacing, dims),
                PLATE_FRAMES,
            ));
        }
        let fine4 = DdgiSettings {
            cascades: 4,
            ..scene.ddgi.clone()
        };
        layouts.push(("plate layout, 4 cascades".to_owned(), fine4, PLATE_FRAMES));
        let slow = DdgiSettings {
            hysteresis: 0.97,
            ..scene.ddgi.clone()
        };
        layouts.push(("plate layout, hysteresis 0.97".to_owned(), slow, tier_frames()));
        layouts.push(("plate layout".to_owned(), scene.ddgi.clone(), PLATE_FRAMES));
        layouts.push((
            "High tier".to_owned(),
            tier_ddgi(DynamicGiTier::High),
            tier_frames(),
        ));
        for (name, ddgi, frames) in layouts {
            let mut r = renderer(&ddgi, true);
            let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
            assert!(r.ddgi_active());
            let ratio = mean_lum(&sub_planes(&full, &direct), &t.surface) / reference.max(1e-3);
            assert!(ratio.is_finite());
            rows.push(Row::new(
                format!("{} / sun only / {name}: indirect energy / tracer", scene.name),
                Bar::Report,
                ratio,
                "×",
            ));
        }
    }
    publish(
        "layout_sweep",
        "GI acceptance: sun bounce across probe layouts",
        &format!(
            "Sun only (sky off), settled, static-screen cache off; the tracer at {DECOMPOSITION_SPP} spp, 6 bounces. \
             Layouts sized by `plate_dims` to hold the box from (−8, −8, −8) to (8, 4, 8) m.\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

/// The scene with its ground (mesh 0, a large thin slab whose top is at y = 0) replaced: `half` is the new
/// half extent in x and z, `thick` the new thickness, `tiles` per axis (1 = one slab).
fn with_ground(scene: &GiScene, half: f32, thick: f32, tiles: u32) -> GiScene {
    let mut s = scene.clone();
    let material = s.lab.meshes[0].material;
    let top = s.lab.meshes[0].model[3][1] + ground_half_height(scene);
    let step = 2.0 * half / tiles as f32;
    let mut ground = Vec::new();
    for i in 0..tiles {
        for k in 0..tiles {
            let cx = -half + step * (i as f32 + 0.5);
            let cz = -half + step * (k as f32 + 0.5);
            ground.push(block(
                [cx, top - thick / 2.0, cz],
                [step / 2.0, thick / 2.0, step / 2.0],
                material,
            ));
        }
    }
    s.lab.meshes.splice(0..1, ground);
    s
}

/// Half height of mesh 0's box (its vertices' y extent / 2).
fn ground_half_height(scene: &GiScene) -> f32 {
    let ys = scene.lab.meshes[0].vertices.iter().map(|v| v.position[1]);
    let (lo, hi) = ys.fold((f32::MAX, f32::MIN), |(l, h), y| (l.min(y), h.max(y)));
    (hi - lo) / 2.0
}

/// Whether the probes miss the sunlit ground a large thin slab makes (its brick voxels are the slab's long
/// extent over 40): the sun bounce against the tracer with the ground as built, smaller, thicker and tiled.
#[test]
#[ignore = "measurement: sun bounce across ground geometry, asserts only that it ran"]
fn ground_sweep() {
    let mut rows = Vec::new();
    for (scene, half) in [(tunnel(), 40.0f32), (trench(), 16.0)]
        .into_iter()
        .filter(|(s, _)| selected(SCENES_ENV, s.name))
    {
        for (vname, variant) in [
            ("as built", scene.clone()),
            ("ground 24 m wide", with_ground(&scene, 12.0, 0.5, 1)),
            ("ground 2 m thick", with_ground(&scene, half, 2.0, 1)),
            ("ground in 4 × 4 tiles", with_ground(&scene, half, 0.5, 4)),
        ] {
            let mut lab = variant.lab.clone();
            lab.ambient_lux = 0.0;
            let t = truth(&lab, DECOMPOSITION_SPP, 6);
            let reference = mean_lum(&t.indirect, &t.surface);
            let direct = realtime_direct(&lab, false);
            for (cname, ddgi, frames) in [
                ("fine", scene.ddgi.clone(), PLATE_FRAMES),
                ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
            ] {
                let mut r = renderer(&ddgi, true);
                let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
                assert!(r.ddgi_active());
                let ratio = mean_lum(&sub_planes(&full, &direct), &t.surface) / reference.max(1e-3);
                assert!(ratio.is_finite());
                rows.push(
                    Row::new(
                        format!(
                            "{} / sun only / {cname} / {vname}: indirect energy / tracer",
                            scene.name
                        ),
                        Bar::Report,
                        ratio,
                        "×",
                    )
                    .note(format!("tracer {reference:.1} cd/m²")),
                );
            }
        }
    }
    publish(
        "ground_sweep",
        "GI acceptance: sun bounce across ground geometry",
        &format!(
            "Sun only, settled, static-screen cache off; each variant against its own trace ({DECOMPOSITION_SPP} \
             spp, 6 bounces). A ratio that rises when the ground is smaller, thicker or tiled says the probes' \
             traced proxy of the large slab loses the sunlit ground.\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

#[test]
fn a_replaced_ground_keeps_its_top_and_covers_its_extent() {
    let s = tunnel();
    for (half, thick, tiles) in [(12.0f32, 0.5f32, 1u32), (40.0, 2.0, 1), (40.0, 0.5, 4)] {
        let g = with_ground(&s, half, thick, tiles);
        assert_eq!(
            g.lab.meshes.len(),
            s.lab.meshes.len() - 1 + (tiles * tiles) as usize
        );
        let n = (tiles * tiles) as usize;
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for m in &g.lab.meshes[..n] {
            for v in &m.vertices {
                for (a, (l, h)) in lo.iter_mut().zip(hi.iter_mut()).enumerate() {
                    let p = v.position[a] + m.model[3][a];
                    *l = l.min(p);
                    *h = h.max(p);
                }
            }
        }
        assert!(hi[1].abs() < 1e-4, "top at y = 0: {hi:?}");
        assert!((hi[1] - lo[1] - thick).abs() < 1e-4);
        assert!((hi[0] - half).abs() < 1e-3 && (lo[2] + half).abs() < 1e-3);
    }
}

/// Sunlit-surface radiance of a Lambertian of `albedo` under `lab`'s sun alone: ρ / π · E · sin(elevation).
fn sunlit_radiance(lab: &LabScene, albedo: f32) -> f32 {
    let d = lab.directional.direction;
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    albedo / std::f32::consts::PI * lab.directional.illuminance * (-d[1] / len).max(0.0)
}

/// What the probes' rays carry, direction-free: with the sun alone, a ray that lands on the sunlit ground
/// (or the door room's sun patch) should carry about that surface's analytic radiance. The census counts
/// hits, misses and back faces over every traced probe ray of the last update and the share of hits at
/// half the sunlit radiance or more; none at all means the probes' sun at a hit is lost (its shadow test or
/// the hit's albedo), not their sampling.
#[test]
#[ignore = "measurement: probe ray radiance census under the sun alone, asserts only that it ran"]
fn ray_census() {
    let mut rows = Vec::new();
    for (scene, albedo) in [
        (tunnel(), 0.4f32),
        (door_room(), 0.45),
        (trench(), 0.45),
        (shaft(), 0.4),
    ]
    .into_iter()
    .filter(|(s, _)| selected(SCENES_ENV, s.name))
    {
        let mut lab = scene.lab.clone();
        lab.ambient_lux = 0.0;
        let expected = sunlit_radiance(&lab, albedo);
        for (cname, ddgi, frames) in [
            ("fine", scene.ddgi.clone(), PLATE_FRAMES),
            ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
        ] {
            let mut r = renderer(&ddgi, true);
            capture(&mut r, &lab, gi_quality(false), frames);
            let Some(rb) = r.ddgi_read_back(device()).expect("read back") else {
                panic!("{}: the probes ran", scene.name);
            };
            let (mut hits, mut misses, mut back, mut bright, mut max) =
                (0usize, 0usize, 0usize, 0usize, 0.0f32);
            for ray in &rb.rays {
                let lum = luminance([ray[0], ray[1], ray[2]]);
                if !lum.is_finite() || (ray[3] == 0.0 && lum == 0.0) {
                    continue; // an untraced slot
                }
                if ray[3] >= 1e8 {
                    misses += 1;
                } else if ray[3] < 0.0 {
                    back += 1;
                } else {
                    hits += 1;
                    max = max.max(lum);
                    if lum >= 0.5 * expected {
                        bright += 1;
                    }
                }
            }
            let share = bright as f32 / hits.max(1) as f32;
            rows.push(
                Row::new(
                    format!("{} / sun only / {cname}: probe ray hits at ≥ ½ sunlit radiance", scene.name),
                    Bar::Report,
                    share * 100.0,
                    " %",
                )
                .note(format!(
                    "{hits} hits, {misses} misses, {back} back faces; brightest hit {max:.0} cd/m² against the sunlit \
                     surface's {expected:.0} (ratio {:.3})",
                    max / expected.max(1e-3)
                )),
            );
        }
    }
    publish(
        "ray_census",
        "GI acceptance: probe ray census (sun only)",
        &format!(
            "Every probe ray of the last update (`DdgiReadback::rays`: radiance, hit distance; ≥ 1e8 a miss, < 0 a back \
             face), sun only. The sunlit radiance is ρ / π · E · sin(elevation) for the named surface (the ground, or the \
             door room's floor patch).\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

/// `lab`'s sun turned to `elevation_deg` above the horizon, its azimuth kept.
fn with_sun_elevation(lab: &LabScene, elevation_deg: f32) -> LabScene {
    let mut l = lab.clone();
    let d = l.directional.direction;
    let h = (d[0] * d[0] + d[2] * d[2]).sqrt().max(1e-6);
    let e = elevation_deg.to_radians();
    l.directional.direction = [d[0] / h * e.cos(), -e.sin(), d[2] / h * e.cos()];
    l
}

/// Whether the sun-bounce deficit grows as the sun gets lower: a sun shadow ray from a ray hit that grazes
/// the surface it starts on (the heightfield precedent `DdgiSettings::hf_shadow_cone` fixed for terrain)
/// would self-shadow the sunlit mesh surfaces more at a low sun.
#[test]
#[ignore = "measurement: sun bounce across sun elevations, asserts only that it ran"]
fn sun_elevation_sweep() {
    let mut rows = Vec::new();
    for scene in [tunnel(), trench(), door_room()]
        .into_iter()
        .filter(|s| selected(SCENES_ENV, s.name))
    {
        for elevation in [20.0f32, 35.0, 50.0, 65.0, 80.0] {
            let mut lab = with_sun_elevation(&scene.lab, elevation);
            lab.ambient_lux = 0.0;
            let t = truth(&lab, DECOMPOSITION_SPP, 6);
            let reference = mean_lum(&t.indirect, &t.surface);
            let direct = realtime_direct(&lab, false);
            for (cname, ddgi, frames) in [
                ("fine", scene.ddgi.clone(), PLATE_FRAMES),
                ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
            ] {
                if !selected(CONFIGS_ENV, cname) {
                    continue;
                }
                let mut r = renderer(&ddgi, true);
                let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
                assert!(r.ddgi_active());
                let ratio = mean_lum(&sub_planes(&full, &direct), &t.surface) / reference.max(1e-3);
                assert!(ratio.is_finite());
                rows.push(
                    Row::new(
                        format!(
                            "{} / sun {elevation}° / {cname}: sun-only indirect energy / tracer",
                            scene.name
                        ),
                        Bar::Report,
                        ratio,
                        "×",
                    )
                    .note(format!("tracer {reference:.1} cd/m²")),
                );
            }
        }
    }
    publish(
        "sun_elevation_sweep",
        "GI acceptance: sun bounce across sun elevations",
        &format!(
            "Sun only, the scene's sun azimuth, settled, static-screen cache off; each elevation against its own trace \
             ({DECOMPOSITION_SPP} spp, 6 bounces).\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

#[test]
fn a_turned_sun_keeps_its_azimuth_and_unit_length() {
    let lab = tunnel().lab;
    let d0 = lab.directional.direction;
    for e in [20.0f32, 55.0, 80.0] {
        let d = with_sun_elevation(&lab, e).directional.direction;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "{len}");
        assert!(((-d[1]).asin().to_degrees() - e).abs() < 1e-3);
        assert!(
            (d[0] * d0[2] - d[2] * d0[0]).abs() < 1e-5,
            "azimuth kept: {d:?} {d0:?}"
        );
    }
}

/// The probe trace's hit epsilon for a box mesh of `extent` (`ddgi_bake::brick_grid`: the longest axis gets 40
/// interior voxels, the others in proportion but at least 4; `eps` = half the smallest voxel, `ddgi.rs`).
fn brick_eps(extent: [f32; 3]) -> f32 {
    let longest = extent.iter().copied().fold(0.0f32, f32::max).max(1e-3);
    extent
        .iter()
        .map(|e| {
            let interior = ((e / longest * 40.0).ceil()).clamp(4.0, 40.0);
            e / interior
        })
        .fold(f32::MAX, f32::min)
        * 0.5
}

/// The self-shadow test of the sun bounce: `ddgi_trace.wgsl` starts a hit's sun shadow ray `0.02 ·
/// spacing` off the hit, and `sdf_trace` reports a hit wherever a sample is nearer than the instance's
/// `eps`. A ground plate whose `eps` (thickness / 8 for a thin plate) exceeds that offset shadows its own
/// sunlit hits, so the sun bounce should flip from short to right as the ground gets thinner, at about
/// thickness = 8 · 0.02 · spacing.
#[test]
#[ignore = "measurement: sun bounce across ground thickness (the hit self-shadow test), asserts only that it ran"]
fn ground_thickness_sweep() {
    let mut rows = Vec::new();
    for (scene, half) in [(tunnel(), 40.0f32), (door_room(), 30.0)]
        .into_iter()
        .filter(|(s, _)| selected(SCENES_ENV, s.name))
    {
        for thick in [0.05f32, 0.1, 0.2, 0.3, 0.5, 1.0] {
            let variant = with_ground(&scene, half, thick, 1);
            let mut lab = variant.lab.clone();
            lab.ambient_lux = 0.0;
            let t = truth(&lab, DECOMPOSITION_SPP, 6);
            let reference = mean_lum(&t.indirect, &t.surface);
            let direct = realtime_direct(&lab, false);
            let eps = brick_eps([2.0 * half, thick, 2.0 * half]);
            for (cname, ddgi, frames) in [
                ("fine", scene.ddgi.clone(), PLATE_FRAMES),
                ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
            ] {
                if !selected(CONFIGS_ENV, cname) {
                    continue;
                }
                let offset = 0.02 * ddgi.spacing;
                let mut r = renderer(&ddgi, true);
                let (full, _) = capture(&mut r, &lab, gi_quality(false), frames);
                assert!(r.ddgi_active());
                let ratio = mean_lum(&sub_planes(&full, &direct), &t.surface) / reference.max(1e-3);
                assert!(ratio.is_finite());
                rows.push(
                    Row::new(
                        format!(
                            "{} / sun only / {cname} / ground {thick} m: indirect energy / tracer",
                            scene.name
                        ),
                        Bar::Report,
                        ratio,
                        "×",
                    )
                    .note(format!(
                        "ground eps {eps:.4} m vs shadow-ray offset {offset:.3} m: {}",
                        if eps > offset {
                            "self-shadow predicted"
                        } else {
                            "clear"
                        }
                    )),
                );
            }
        }
    }
    publish(
        "ground_thickness_sweep",
        "GI acceptance: sun bounce across ground thickness",
        &format!(
            "Sun only, settled, static-screen cache off; the ground's top stays at y = 0; each variant against its own \
             trace ({DECOMPOSITION_SPP} spp, 6 bounces). `eps` is the probe trace's hit epsilon of the ground's brick, \
             the offset the sun shadow ray's start above a hit (`ddgi_trace.wgsl` `trace`).\n\n{}",
            render_rows(1, &rows)
        ),
    );
}

#[test]
fn the_brick_eps_follows_the_baked_grid() {
    // A 0.5 m plate 80 m wide: 4 interior voxels in y (the floor of `MIN_INTERIOR_DIM`), 0.125 m: eps 0.0625 m.
    assert!((brick_eps([80.0, 0.5, 80.0]) - 0.0625).abs() < 1e-6);
    // A cube: 40 voxels a side.
    assert!((brick_eps([4.0, 4.0, 4.0]) - 0.05).abs() < 1e-6);
    // A 2 m thick plate: 0.5 m voxels in y, eps 0.25 m.
    assert!((brick_eps([80.0, 2.0, 80.0]) - 0.25).abs() < 1e-6);
}

/// Sun-only indirect energy over the tracer's for `scene` with its ground `thick` metres thick, plate layout.
fn thick_ground_sun_ratio(scene: &GiScene, half: f32, thick: f32) -> f32 {
    let variant = with_ground(scene, half, thick, 1);
    let mut lab = variant.lab.clone();
    lab.ambient_lux = 0.0;
    let t = truth(&lab, DECOMPOSITION_SPP, 6);
    let reference = mean_lum(&t.indirect, &t.surface);
    let direct = realtime_direct(&lab, false);
    let mut r = renderer(&scene.ddgi, true);
    let (full, _) = capture(&mut r, &lab, gi_quality(false), PLATE_FRAMES);
    assert!(r.ddgi_active());
    mean_lum(&sub_planes(&full, &direct), &t.surface) / reference.max(1e-3)
}

/// Target for the GI fix lane (finding C, ADR-4948): a probe ray's hit on a thick floor must keep its sun. Before
/// ADR-5001 the sun shadow ray started inside the floor brick's own epsilon band and the 0.5 m floor read 0.57 of
/// what a 0.1 m one reads on the tunnel's plate layout (RTX 5080, 2026-11-09); `sdf_trace` now skips the shadow
/// ray's own band, and the measured experiment read 0.95 of it.
#[test]
#[ignore = "GPU target: the probe sun shadow ray clears its own floor (ADR-4948 finding C, ADR-5001)"]
fn target_a_thick_floor_keeps_its_sun_bounce() {
    let scene = tunnel();
    let thin = thick_ground_sun_ratio(&scene, 40.0, 0.1);
    let thick = thick_ground_sun_ratio(&scene, 40.0, 0.5);
    eprintln!("tunnel sun bounce / tracer: 0.1 m ground {thin:.4}, 0.5 m ground {thick:.4}");
    assert!(thin.is_finite() && thick.is_finite());
    assert!(
        thick >= 0.9 * thin,
        "a 0.5 m floor reads {thick:.3} of the tracer's sun bounce, a 0.1 m one {thin:.3}: the probes' shadow ray \
         self-shadows the thicker floor"
    );
}

/// Target for the frame lane (finding A, ADR-4948): a shipped tier must converge before its probe update freezes.
/// Before ADR-5001 the update stopped after 600 unchanged frames, 75 updates a probe at High's divisor 8, and the
/// sealed box held 7.49 % of the lit ground (RTX 5080, 2026-11-09). 4096 frames is 512 updates a probe, past the
/// 384 at which the every-frame run reads 0.51 %; the freeze now waits for those 512.
#[test]
#[ignore = "GPU target, long (4096 frames): the probe freeze counts updates a probe (ADR-4948 finding A, ADR-5001)"]
fn target_the_shipped_high_tier_converges_before_it_freezes() {
    const FRAMES: u32 = 4096;
    let scene = sealed_box();
    let ground = scene.scale.expect("the leak scene names its scale");
    let truth = truth(&scene.lab, 256, 6);
    let direct = realtime_direct(&scene.lab, false);
    let mut r = renderer(&shipped_ddgi(DynamicGiTier::High), true);
    let (full, _) = capture(&mut r, &scene.lab, gi_quality(false), FRAMES);
    assert!(r.ddgi_active());
    let leak = mean_lum(&sub_planes(&full, &direct), &truth.surface) / ground;
    eprintln!(
        "sealed box, High at divisor {}: leak {:.4} % after {FRAMES} frames (scene unchanged {} frames)",
        shipped_ddgi(DynamicGiTier::High).update_divisor,
        leak * 100.0,
        r.ddgi_settled_frames()
    );
    assert!(
        leak.is_finite() && leak <= LEAK_TARGET,
        "the shipped High tier holds {:.3} % in a closed box after {FRAMES} frames (bar {} %)",
        leak * 100.0,
        LEAK_TARGET * 100.0
    );
}

/// sRGB-encodes the runtime's display transform of `img` at `ev100` + `boost` EV as RGB8.
fn display_rgb8(img: &[[f32; 3]], ev100: f32, boost: f32) -> Vec<u8> {
    display_encode(img, ev100 - boost)
        .iter()
        .flat_map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8))
        .collect()
}

/// Writes the panels side by side as one RGB8 PNG.
fn write_strip(path: &std::path::Path, w: u32, h: u32, panels: &[Vec<u8>]) {
    let row = (w * 3) as usize;
    let mut rgb = Vec::with_capacity(row * panels.len() * h as usize);
    for y in 0..h as usize {
        for p in panels {
            rgb.extend_from_slice(&p[y * row..(y + 1) * row]);
        }
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, w * panels.len() as u32, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    match encoder
        .write_header()
        .and_then(|mut wr| wr.write_image_data(&rgb))
    {
        Ok(()) => {
            if let Err(e) = regia_test_utils::write_atomic(path, &bytes) {
                eprintln!("{}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("{}: {e}", path.display()),
    }
}

#[test]
#[ignore = "measurement: owner review plates (PNG with REGIA_GI_ACCEPT_OUT), asserts only that they rendered"]
fn owner_plates() {
    let scale = env_u32(PLATE_SCALE_ENV, 1).clamp(1, 12);
    let spp = env_u32(PLATE_SPP_ENV, REFERENCE_SPP);
    let dir = out_dir();
    let mut table = String::from(
        "| scene | size | beauty FLIP High | beauty FLIP fine | indirect FLIP High | indirect FLIP fine | indirect FLIP High probes only | indirect FLIP screen only | files |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for scene in [
        door_room(),
        color_room(),
        tunnel(),
        shaft(),
        trench(),
        sealed_box(),
    ]
    .into_iter()
    .filter(|s| selected(SCENES_ENV, s.name))
    {
        let mut lab = scene.lab.clone();
        lab.width = GI_WIDTH * scale;
        lab.height = GI_HEIGHT * scale;
        let (w, h) = (lab.width, lab.height);
        let t = truth(&lab, spp, REFERENCE_BOUNCES);
        let direct = realtime_direct(&lab, false);
        let mut beauty = Vec::new();
        let mut indirect = Vec::new();
        let both = gi_quality(false);
        let probes_only = FrameQuality {
            screen_space_gi: false,
            ..both
        };
        let screen_only = FrameQuality {
            gi: GiMode::ScreenSpace,
            gi_probes: false,
            ..both
        };
        // High tier, fine layout (both techniques), then the High tier's terms: probes alone, screen alone.
        for (ddgi, frames, q, probes) in [
            (tier_ddgi(DynamicGiTier::High), tier_frames(), both, true),
            // The plate's layout with the tiers' lookup (`fine_surface`, ADR-5374).
            (
                DdgiSettings {
                    visibility_blend: true,
                    ..scene.ddgi.clone()
                },
                PLATE_FRAMES,
                both,
                true,
            ),
            (tier_ddgi(DynamicGiTier::High), tier_frames(), probes_only, true),
            (
                tier_ddgi(DynamicGiTier::High),
                STILL_FRAMES + 4,
                screen_only,
                false,
            ),
        ] {
            let mut r = renderer(&ddgi, probes);
            let (full, _) = capture(&mut r, &lab, q, frames);
            assert_eq!(r.ddgi_active(), probes);
            indirect.push(sub_planes(&full, &direct));
            beauty.push(full);
        }
        let ev = lab.exposure_ev;
        // Surfaces only: the tracer's uniform sky and the frame's atmosphere differ behind every opening, and
        // that is not GI.
        let mask = &t.surface;
        let scored = |a: &[[f32; 3]], b: &[[f32; 3]], boost: f32| {
            regia_test_utils::gi_error::masked_flip_stats(a, b, mask, w, h, ev - boost)
        };
        let (bh, _) = scored(&t.image.beauty, &beauty[0], 0.0);
        let (bf, _) = scored(&t.image.beauty, &beauty[1], 0.0);
        let (ih, ih95) = scored(&t.indirect, &indirect[0], 2.0);
        let (ifine, _) = scored(&t.indirect, &indirect[1], 2.0);
        let (ip, _) = scored(&t.indirect, &indirect[2], 2.0);
        let (is, _) = scored(&t.indirect, &indirect[3], 2.0);
        let mut files = String::new();
        if let Some(dir) = &dir {
            let heat = |a: &[[f32; 3]], b: &[[f32; 3]], boost: f32| -> Vec<u8> {
                let (_, f) = compare_linear(a, b, w, h, ev - boost, DEFAULT_PPD);
                let map: Vec<f32> = f
                    .map
                    .iter()
                    .zip(mask)
                    .map(|(v, m)| if *m { *v } else { 0.0 })
                    .collect();
                flip_heatmap_rgba8(&map)
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2]])
                    .collect()
            };
            let b = format!("{}_beauty.png", scene.name);
            write_strip(
                &dir.join(&b),
                w,
                h,
                &[
                    display_rgb8(&t.image.beauty, ev, 0.0),
                    display_rgb8(&beauty[0], ev, 0.0),
                    display_rgb8(&beauty[1], ev, 0.0),
                    heat(&t.image.beauty, &beauty[0], 0.0),
                ],
            );
            let i = format!("{}_indirect.png", scene.name);
            write_strip(
                &dir.join(&i),
                w,
                h,
                &[
                    display_rgb8(&t.indirect, ev, 2.0),
                    display_rgb8(&indirect[0], ev, 2.0),
                    display_rgb8(&indirect[1], ev, 2.0),
                    heat(&t.indirect, &indirect[0], 2.0),
                ],
            );
            let k = format!("{}_terms.png", scene.name);
            write_strip(
                &dir.join(&k),
                w,
                h,
                &[
                    display_rgb8(&t.indirect, ev, 2.0),
                    display_rgb8(&indirect[0], ev, 2.0),
                    display_rgb8(&indirect[2], ev, 2.0),
                    display_rgb8(&indirect[3], ev, 2.0),
                ],
            );
            files = format!("`{b}`, `{i}`, `{k}`");
        }
        let _ = writeln!(
            table,
            "| {} | {w} × {h} | {bh:.4} | {bf:.4} | {ih:.4} (p95 {ih95:.3}) | {ifine:.4} | {ip:.4} | {is:.4} | {files} |",
            scene.name
        );
        assert!(bh.is_finite() && ih.is_finite());
    }
    publish(
        "owner_plates",
        "GI acceptance: owner review plates",
        &format!(
            "`_beauty` and `_indirect`, left to right: path tracer ({spp} spp, {REFERENCE_BOUNCES} bounces) | Desktop High tier probes + \
             screen pass | the plate's fine probe layout with the tiers' lookup (`fine_surface`) | FLIP map of the High tier against the tracer on surfaces (black \
             none, white most; the sky is masked: the tracer's uniform sky and the frame's atmosphere differ behind every \
             opening, and that is not GI). `_terms`, indirect light: path tracer | High probes + screen pass | High probes \
             only | screen pass only (probes off). Beauty through the runtime's AgX at the scene's exposure (no bloom, so \
             both sides match); indirect light (full − direct) at +2 EV. Settled ({} / {PLATE_FRAMES} frames), \
             static-screen cache off. FLIP numbers are over the surface pixels.\n\n{table}",
            tier_frames()
        ),
    );
}

// ---------------------------------------------------------------------------------------------
// CPU tests of the harness itself (run in every pass).
// ---------------------------------------------------------------------------------------------

#[test]
fn a_bar_passes_at_its_bound_and_fails_past_it_or_on_nan() {
    assert_eq!(Bar::AtMost(0.08).verdict(0.08), Some(true));
    assert_eq!(Bar::AtMost(0.08).verdict(0.0801), Some(false));
    assert_eq!(Bar::AtLeast(0.98).verdict(0.98), Some(true));
    assert_eq!(Bar::AtLeast(0.98).verdict(0.979), Some(false));
    assert_eq!(Bar::AtMost(1.0).verdict(f32::NAN), Some(false));
    assert_eq!(Bar::AtLeast(1.0).verdict(f32::INFINITY), Some(false));
    assert_eq!(Bar::Report.verdict(5.0), None);
}

#[test]
fn the_table_numbers_its_rows_and_marks_each_verdict() {
    let rows = [
        Row::new("a", Bar::AtMost(0.08), 0.05, ""),
        Row::new("b", Bar::AtMost(0.08), 0.11, "").note("over | under"),
        Row::new("c", Bar::Report, 1.5, "×"),
    ];
    let t = render_rows(4, &rows);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 5);
    assert!(
        lines[2].starts_with("| 4 | a | ≤ 0.0800 | 0.0500 | pass |"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("| 5 | b |") && lines[3].contains("**FAIL**"),
        "{}",
        lines[3]
    );
    assert!(
        lines[3].contains("over / under"),
        "a pipe in a note must not split the row"
    );
    assert!(
        lines[4].contains("report") && lines[4].contains("1.5000×"),
        "{}",
        lines[4]
    );
}

/// ADR-5938's candidate is what its measurement says it is: High's probe grid and finest window, Low's brick pool
/// and screen lookup region, and within a twentieth of the rays Low traces a settled frame. It is no shipped
/// tier, and its long configurations stay out of an unfiltered run.
#[test]
fn the_low_dense_candidate_is_highs_grid_at_lows_ray_budget() {
    use regia_render::dynamic_gi::budget_of;
    let dense = low_dense_ddgi();
    let (low, high) = (
        shipped_ddgi(DynamicGiTier::Low),
        shipped_ddgi(DynamicGiTier::High),
    );
    assert_eq!(
        (dense.dims, dense.cascades, dense.spacing, dense.spacing_ratio),
        (high.dims, high.cascades, high.spacing, high.spacing_ratio)
    );
    assert_eq!(dense.update_divisor, 8 * high.update_divisor);
    assert_eq!(
        (
            dense.finest_headroom_m,
            dense.front_weights,
            dense.edge_visibility
        ),
        (high.finest_headroom_m, high.front_weights, high.edge_visibility)
    );
    let (b, bl, bh) = (budget_of(&dense), budget_of(&low), budget_of(&high));
    assert_eq!((b.probes, b.probe_bytes), (bh.probes, bh.probe_bytes));
    assert_eq!(b.rays_per_frame, 9_000);
    assert_eq!(bl.rays_per_frame, 8_640);
    assert!(
        b.rays_per_frame * 20 <= bl.rays_per_frame * 21,
        "{} rays a frame against Low's {}",
        b.rays_per_frame,
        bl.rays_per_frame
    );
    assert_eq!(
        (b.sdf_pool_bytes, b.screen_bytes),
        (bl.sdf_pool_bytes, bl.screen_bytes)
    );
    assert_eq!(
        (b.finest_above_m, b.finest_below_m),
        (bh.finest_above_m, bh.finest_below_m)
    );
    assert!(
        DynamicGiTier::ALL.iter().all(|t| !t.matches(&dense)),
        "the candidate is not a shipped tier"
    );
    for name in LOW_DENSE_CONFIGS {
        assert!(
            name.starts_with("low_dense_") || name.starts_with("low_shipped_"),
            "{name}"
        );
    }
}

#[test]
fn plate_dims_hold_the_box_in_the_finest_window() {
    for (eye, s, lo, hi) in [
        (DOOR_EYE, 0.5, [-3.0, 0.0, -3.0], [3.0, 3.0, 3.0]),
        (TUNNEL_EYE, 0.75, [-8.0, 0.0, -1.5], [8.0, 3.0, 1.5]),
        (SHAFT_EYE, 0.75, [-1.5, -8.0, -1.5], [1.5, 0.0, 1.5]),
    ] {
        let d = plate_dims(eye, s, lo, hi);
        for (a, &da) in d.iter().enumerate() {
            assert!(da < 64 && da.is_multiple_of(2), "axis {a}: {d:?}");
            let (e, df) = (eye[a] / s, da as f32);
            let lift = if a == 1 { 0.1 * df } else { 0.0 };
            assert!(e - lift - df / 2.0 + 1.75 <= lo[a] / s, "axis {a} low: {d:?}");
            assert!(hi[a] / s <= e - lift + df / 2.0 - 3.75, "axis {a} high: {d:?}");
            // Least: two fewer probes no longer hold it.
            if da > 4 {
                let df = df - 2.0;
                let lift = if a == 1 { 0.1 * df } else { 0.0 };
                assert!(
                    !(e - lift - df / 2.0 + 1.75 <= lo[a] / s && hi[a] / s <= e - lift + df / 2.0 - 3.75),
                    "axis {a}: {d:?} is not the least"
                );
            }
        }
    }
}

#[test]
fn the_interior_scenes_have_the_camera_inside_and_rough_dielectrics() {
    for s in [door_room(), tunnel()] {
        assert_eq!((s.lab.width, s.lab.height), (GI_WIDTH, GI_HEIGHT));
        assert!(!s.lab.ibl && s.lab.ambient_lux > 0.0 && s.lab.local_lights.is_empty());
        assert!(
            s.lab
                .materials
                .iter()
                .all(|m| m.metallic == 0.0 && m.roughness >= 0.9)
        );
        assert!(s.lab.meshes.iter().all(|m| m.material < s.lab.materials.len()));
        assert!(s.scale.is_none() && s.ddgi.bake_on_caller && s.ddgi.update_divisor == 1);
        let e = s.lab.eye;
        assert!(e[1] > 0.0 && e[1] < 3.0, "{}: the eye is under the roof", s.name);
        assert!(
            e[0].abs() < 3.0 && e[2].abs() < 1.5 + 2.0,
            "{}: the eye is inside",
            s.name
        );
    }
    // The shaft: the eye is inside the shaft near its bottom, the sun 60° high.
    let sh = shaft();
    assert!(
        sh.lab
            .materials
            .iter()
            .all(|m| m.metallic == 0.0 && m.roughness >= 0.9)
    );
    assert!(sh.lab.meshes.iter().all(|m| m.material < sh.lab.materials.len()));
    let e = sh.lab.eye;
    assert!(
        e[0].abs() < 1.5 && e[2].abs() < 1.5 && e[1] > -8.0 && e[1] < -4.0,
        "{e:?}"
    );
    let sd = sh.lab.directional.direction;
    let sl = (sd[0] * sd[0] + sd[1] * sd[1] + sd[2] * sd[2]).sqrt();
    assert!(((-sd[1] / sl).asin().to_degrees() - 60.0).abs() < 1.0);
    // The door room's sun travels in through the door (−z), down at 40°.
    let d = door_room().lab.directional;
    let dir = [d.direction[0], d.direction[1], d.direction[2]];
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    assert!(dir[2] < 0.0, "{dir:?}");
    let elevation = (-dir[1] / len).asin().to_degrees();
    assert!((elevation - 40.0).abs() < 1.0, "{elevation}");
    // The decomposition's lights are separable: zeroing one keeps the other.
    let mut sky = door_room().lab;
    sky.directional.illuminance = 0.0;
    assert!(sky.ambient_lux > 0.0);
}

/// Measurement (ADR-5290): where each scene's remaining mean FLIP comes from. Per scene: the path tracer's
/// own noise (a second reference with another seed against the first), then the fine layout and the
/// High tier (every probe each frame) as scored by `acceptance_scenes`, and each with one term changed:
/// SSAO off, the screen-space GI pass off, the screen lookup's exact irradiance for the depth normal (no
/// ambient-cube rebuild), and the mean of 16 consecutive frames (the probes' update noise averaged out).
/// A term whose change lowers the FLIP is a share of the error; none of these is a shipping setting.
#[test]
#[ignore = "measurement: the remaining FLIP by term per scene, asserts only that it ran"]
fn error_attribution() {
    let mut detail = String::new();
    let _ = writeln!(
        detail,
        "| scene | layout | variant | mean FLIP | p95 | energy | in shadow × |\n|---|---|---|---|---|---|---|"
    );
    for scene in acceptance_scene_list() {
        if scene.scale.is_some() {
            continue;
        }
        let lab = &scene.lab;
        let truth = truth(lab, REFERENCE_SPP, REFERENCE_BOUNCES);
        let second = reference_seeded(lab, REFERENCE_SPP, REFERENCE_BOUNCES, 91);
        let second = add_planes(&second.indirect_diffuse, &second.indirect_specular);
        let floor = score(&scene, &truth, &truth.indirect, &second);
        let _ = writeln!(
            detail,
            "| {} | path tracer | seed 91 vs 23 ({REFERENCE_SPP} spp, {REFERENCE_BOUNCES} bounces) | {:.4} | {:.4} | {} | - |",
            scene.name,
            floor.flip_mean,
            floor.flip_p95,
            floor.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
        );
        let direct = realtime_direct(lab, false);
        let layouts = [
            ("fine", scene.ddgi.clone(), PLATE_FRAMES),
            ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
        ];
        for (layout, ddgi, frames) in layouts {
            if !selected(CONFIGS_ENV, layout) {
                continue;
            }
            let base = gi_quality(false);
            let variants: [(&str, DdgiSettings, FrameQuality, u32); 6] = [
                ("as scored", ddgi.clone(), base, 1),
                (
                    "ssao off",
                    ddgi.clone(),
                    FrameQuality {
                        ssao: SsaoMode::Off,
                        ..base
                    },
                    1,
                ),
                (
                    "screen-space GI off",
                    ddgi.clone(),
                    FrameQuality {
                        screen_space_gi: false,
                        ..base
                    },
                    1,
                ),
                (
                    "exact lookup (no ambient cube)",
                    DdgiSettings {
                        screen_exact_debug: true,
                        ..ddgi.clone()
                    },
                    base,
                    1,
                ),
                ("mean of 16 frames", ddgi.clone(), base, 16),
                (
                    "ssao + screen-space GI off, 16 frames",
                    ddgi.clone(),
                    FrameQuality {
                        ssao: SsaoMode::Off,
                        screen_space_gi: false,
                        ..base
                    },
                    16,
                ),
            ];
            for (name, settings, q, average) in variants {
                let mut r = renderer(&settings, true);
                let (mut sum, _) = capture(&mut r, lab, q, frames);
                for _ in 1..average {
                    let (next, _) = capture(&mut r, lab, q, 1);
                    for (s, n) in sum.iter_mut().zip(&next) {
                        for (a, b) in s.iter_mut().zip(n) {
                            *a += b;
                        }
                    }
                }
                let inv = 1.0 / average as f32;
                for s in &mut sum {
                    for v in s.iter_mut() {
                        *v *= inv;
                    }
                }
                let test = sub_planes(&sum, &direct);
                let e = score(&scene, &truth, &truth.indirect, &test);
                let _ = writeln!(
                    detail,
                    "| {} | {layout} | {name} | {:.4} | {:.4} | {} | {} |",
                    scene.name,
                    e.flip_mean,
                    e.flip_p95,
                    e.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
                    e.shadow_bias
                        .map_or("-".to_owned(), |b| format!("{:.4}", 1.0 + b)),
                );
            }
        }
    }
    let body = format!(
        "Mean FLIP of the indirect light (frame minus its direct light, as `acceptance_scenes`) against the path \
         tracer at {REFERENCE_SPP} spp, {REFERENCE_BOUNCES} bounces; bar {FLIP_TARGET}. Variants change one term each.\n\n{detail}"
    );
    publish(
        "error_attribution",
        "GI acceptance: the remaining error by term",
        &body,
    );
}

/// Measurement (ADR-5290): whether the 6-bounce reference holds the converged indirect light. The probes carry
/// every bounce (each update reads the field it is building); the tracer stops at `max_bounces` (Russian roulette
/// from the third vertex keeps it unbiased below that). Per scene: the reference's indirect energy at 12 and 32
/// bounces over 6, and the fine layout and the High tier scored against the 6- and the 32-bounce reference.
#[test]
#[ignore = "measurement: the reference's bounce depth per scene, asserts only that it ran"]
fn reference_depth() {
    let mut detail = String::new();
    let _ = writeln!(
        detail,
        "| scene | indirect 12 / 6 bounces | 32 / 6 | layout | mean FLIP vs 6 | vs 32 | p95 vs 6 | vs 32 | energy vs 6 | vs 32 |\n|---|---|---|---|---|---|---|---|---|---|"
    );
    for scene in acceptance_scene_list() {
        if scene.scale.is_some() {
            continue;
        }
        let lab = &scene.lab;
        let six = truth(lab, REFERENCE_SPP, 6);
        let twelve = truth(lab, REFERENCE_SPP, 12);
        let deep = truth(lab, REFERENCE_SPP, 32);
        let e6 = mean_lum(&six.indirect, &six.surface);
        let e12 = mean_lum(&twelve.indirect, &six.surface) / e6.max(1e-9);
        let e32 = mean_lum(&deep.indirect, &six.surface) / e6.max(1e-9);
        let direct = realtime_direct(lab, false);
        for (layout, ddgi, frames) in [
            ("fine", scene.ddgi.clone(), PLATE_FRAMES),
            ("high", tier_ddgi(DynamicGiTier::High), tier_frames()),
        ] {
            let mut r = renderer(&ddgi, true);
            let (full, _) = capture(&mut r, lab, gi_quality(false), frames);
            let test = sub_planes(&full, &direct);
            let a = score(&scene, &six, &six.indirect, &test);
            let b = score(&scene, &six, &deep.indirect, &test);
            let _ = writeln!(
                detail,
                "| {} | {e12:.4} | {e32:.4} | {layout} | {:.4} | {:.4} | {:.4} | {:.4} | {} | {} |",
                scene.name,
                a.flip_mean,
                b.flip_mean,
                a.flip_p95,
                b.flip_p95,
                a.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
                b.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
            );
        }
    }
    let body = format!(
        "Path tracer {REFERENCE_SPP} spp at 6, 12 and 32 bounces (Russian roulette from vertex 3); the masks are the \
         6-bounce reference's. Bar {FLIP_TARGET}.\n\n{detail}"
    );
    publish(
        "reference_depth",
        "GI acceptance: the reference's bounce depth",
        &body,
    );
}

/// Reference samples per pixel of [`term_table`] (default [`REFERENCE_SPP`]).
const REF_SPP_ENV: &str = "REGIA_GI_REF_SPP";
/// Frame size multiplier of [`term_table`] (of 160 × 96; default 1).
const SCALE_ENV: &str = "REGIA_GI_SCALE";
/// Comma list of the lights [`term_table`] scores: `both`, `sun`, `sky` (default `both`).
const LIGHTS_ENV: &str = "REGIA_GI_LIGHTS";
/// With `1`, [`term_table`] also scores a second reference (another seed) against the first: the reference's own
/// noise at the chosen samples and size.
const NOISE_ENV: &str = "REGIA_GI_NOISE";

/// With `1`, [`term_table`] writes one PNG strip per row into `REGIA_GI_ACCEPT_OUT`: the tracer's indirect light, the
/// probes', their signed ratio ([`signed_ratio_rgb8`]) and the FLIP map.
const DUMP_ENV: &str = "REGIA_GI_DUMP";

/// The luminance ratio test / reference per surface pixel as RGB8: white where they agree, toward red where the test
/// is brighter and toward blue where it is darker, full color at a factor of √2 either way; black off the mask.
fn signed_ratio_rgb8(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool]) -> Vec<u8> {
    let lum = |c: &[f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let n = mask.iter().filter(|m| **m).count().max(1) as f32;
    let mean = reference
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .map(|(c, _)| lum(c))
        .sum::<f32>()
        / n;
    let eps = (0.01 * mean).max(1e-6);
    reference
        .iter()
        .zip(test)
        .zip(mask)
        .flat_map(|((r, t), m)| {
            if !*m {
                return [0u8; 3];
            }
            let v = (((lum(t).max(0.0) + eps) / (lum(r).max(0.0) + eps)).log2() / 0.5).clamp(-1.0, 1.0);
            let fade = ((1.0 - v.abs()) * 255.0 + 0.5) as u8;
            if v >= 0.0 {
                [255, fade, fade]
            } else {
                [fade, fade, 255]
            }
        })
        .collect()
}

/// Measurement (ADR-5374): one compact row per scene, light and probe configuration (mean and p95 FLIP, energy, GI
/// in sun shadow over the tracer's), at a chosen frame size and reference sample count. The acceptance table scores
/// 160 × 96 frames against a 1024-sample reference; this tells how much of a row is the frame size (the half
/// resolution screen lookup holds 80 × 48 texels there) and how much the reference's noise, and it splits a row by
/// light. Scenes and configurations as `acceptance_scenes` (`REGIA_GI_SCENES`, `REGIA_GI_CONFIGS`).
#[test]
#[ignore = "measurement: FLIP and energy per scene, light and layout at a chosen size and reference, asserts only that it ran"]
fn term_table() {
    let spp = env_u32(REF_SPP_ENV, REFERENCE_SPP).max(1);
    let scale = env_u32(SCALE_ENV, 1).clamp(1, 16);
    let noise = env_u32(NOISE_ENV, 0) == 1;
    let mut detail = String::from(
        "| scene | light | config | frames | mean FLIP | p95 | energy | in shadow × | probes cd/m² | tracer cd/m² | frame-to-frame FLIP |\n|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for scene in acceptance_scene_list() {
        for light in ["both", "sun", "sky"] {
            let listed = regia_core::brand::env_var_os(LIGHTS_ENV).is_some_and(|v| !v.is_empty());
            if !(if listed {
                selected(LIGHTS_ENV, light)
            } else {
                light == "both"
            }) {
                continue;
            }
            let mut lab = scene.lab.clone();
            lab.width *= scale;
            lab.height *= scale;
            match light {
                "sun" => lab.ambient_lux = 0.0,
                "sky" => lab.directional.illuminance = 0.0,
                _ => {}
            }
            let scaled = GiScene {
                lab: lab.clone(),
                ..scene.clone()
            };
            let truth = truth(&lab, spp, REFERENCE_BOUNCES);
            if noise {
                let second = reference_seeded(&lab, spp, REFERENCE_BOUNCES, 91);
                let second = add_planes(&second.indirect_diffuse, &second.indirect_specular);
                let floor = score(&scaled, &truth, &truth.indirect, &second);
                let _ = writeln!(
                    detail,
                    "| {} | {light} | tracer seed 91 vs 23 | - | {:.4} | {:.4} | {} | - | {:.1} | {:.1} | - |",
                    scene.name,
                    floor.flip_mean,
                    floor.flip_p95,
                    floor.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
                    floor.mean_test,
                    floor.mean_reference,
                );
            }
            let direct = realtime_direct(&lab, false);
            for config in configs(&scene) {
                if config.held {
                    continue;
                }
                let mut r = renderer(&config.ddgi, true);
                let (full, _) = capture(&mut r, &lab, gi_quality(false), config.frames);
                assert!(
                    r.ddgi_active(),
                    "{} / {}: the probes lit the frame",
                    scene.name,
                    config.name
                );
                let test = sub_planes(&full, &direct);
                let e = score(&scaled, &truth, &truth.indirect, &test);
                assert!(e.flip_mean.is_finite(), "{}: {e:?}", scene.name);
                if env_u32(DUMP_ENV, 0) == 1
                    && let Some(dir) = out_dir()
                {
                    // tracer | probes | signed ratio | FLIP, indirect light at +1 EV.
                    let (w, h, ev) = (lab.width, lab.height, lab.exposure_ev);
                    let (_, f) = compare_linear(&truth.indirect, &test, w, h, ev, DEFAULT_PPD);
                    let map: Vec<f32> = f
                        .map
                        .iter()
                        .zip(&truth.surface)
                        .map(|(v, m)| if *m { *v } else { 0.0 })
                        .collect();
                    let heat: Vec<u8> = flip_heatmap_rgba8(&map)
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .flat_map(|p| [p[0], p[1], p[2]])
                        .collect();
                    write_strip(
                        &dir.join(format!("{}_{light}_{}.png", scene.name, config.name)),
                        w,
                        h,
                        &[
                            display_rgb8(&truth.indirect, ev, 1.0),
                            display_rgb8(&test, ev, 1.0),
                            signed_ratio_rgb8(&truth.indirect, &test, &truth.surface),
                            heat,
                        ],
                    );
                }
                // The probes' update noise: display frames against their predecessors while every probe updates
                // (`acceptance_scenes`' stability row, bar 0.01).
                let (noise, _) = stability(&mut r, &lab, gi_quality(false), STABILITY_FRAMES);
                let _ = writeln!(
                    detail,
                    "| {} | {light} | {} | {} | {:.4} | {:.4} | {} | {} | {:.1} | {:.1} | {noise:.4} |",
                    scene.name,
                    config.name,
                    config.frames,
                    e.flip_mean,
                    e.flip_p95,
                    e.energy_ratio.map_or("-".to_owned(), |v| format!("{v:.4}")),
                    e.shadow_bias
                        .map_or("-".to_owned(), |b| format!("{:.4}", 1.0 + b)),
                    e.mean_test,
                    e.mean_reference,
                );
            }
        }
    }
    let body = format!(
        "Frames {} × {}, reference {spp} spp, {REFERENCE_BOUNCES} bounces; settled, static-screen cache off. Bars: mean \
         FLIP {FLIP_TARGET}, p95 {FLIP_P95_TARGET}.\n\n{detail}",
        GI_WIDTH * scale,
        GI_HEIGHT * scale,
    );
    publish(
        "term_table",
        "GI acceptance: terms by size, reference and light",
        &body,
    );
}
