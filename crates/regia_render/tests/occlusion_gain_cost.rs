// SPDX-License-Identifier: MPL-2.0
//! Does either two-phase occlusion cull pay for itself on heavy casters (burn-down row A03)? The
//! cost cases of `shadow_occlusion.rs` (`shadow_occlusion_cost`) and `two_phase_occlusion.rs`
//! (`two_phase_prepass_cost`) draw cheap 12-triangle boxes, where both culls cost more GPU time
//! than they save. This file prices them on the visibility-buffer bench's heavy layout
//! (`visbuffer_dense_bench.rs`, scene `4096x2048`, `REGIA_VIS_BENCH_MASKED=1`,
//! `REGIA_VIS_BENCH_SWAY=1`): 4 096 instances of one ~2 048-triangle sphere over 64 masked,
//! double-sided materials with holed alpha, alone or swaying in a wind, with alpha-tested shadows.
//!
//! `heavy_caster_occlusion_cost` (`--ignored`, a GPU timestamp printout run on a card) renders
//! each scene with both culls off, with the prepass cull on and with the shadow cull on, in
//! interleaved rounds; it prints the means of 24 or more frames of GPU timestamps per run and the
//! medians over the rounds, and checks that every culled frame equals the unculled one byte for
//! byte. `swaying_casters_cull_on_their_declared_bound` and its two neighbors (lavapipe, in the
//! gate) pin how swaying casters take part: a vertex-stage graph that declares its largest offset (a wind sway) is
//! culled on its mesh bound grown by that offset (ADR-5389, as the colour pass's cull already
//! did); one that declares none has no safe bound, and both culls draw it in phase 1 (the only
//! behavior before ADR-5389, kept as the `sway_undeclared_hidden` class for comparison).
//!
//! ```sh
//! REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 cargo test --release -p regia_render \
//!     --test occlusion_gain_cost -- --ignored --nocapture heavy_caster_occlusion_cost
//! ```
//!
//! On a real card the byte comparison of `heavy_caster_occlusion_cost` can fail with both culls
//! exact: NVIDIA's 615 drivers run a pipeline's dispatch with one of two shader variants that
//! round differently, in short windows a few seconds into a renderer's life (ADR-5953).
//! `screen_lookup_repeats_alone` reproduces that with nothing but one dispatch repeated,
//! `culled_scene_repeats_run_to_run` shows it on whole frames of the unculled renderer, and
//! `heavy_casters_cull_exactly_where_they_draw` checks the culls where they act: scene depth and
//! shadow maps, which the drivers repeat exactly.
//!
//! Knobs: `REGIA_OCC_GAIN_SCENES` (comma list of `masked_hidden`, `masked_open`, `sway_hidden`,
//! `sway_undeclared_hidden`, `opaque_hidden`; default all five), `REGIA_OCC_GAIN_SIZE` (default `2560x1440`),
//! `REGIA_OCC_GAIN_ROUNDS` (default 3), `REGIA_OCC_GAIN_FRAMES` (default 24, at least 24),
//! `REGIA_OCC_GAIN_INSTANCES` (default 4096), `REGIA_OCC_GAIN_PAGED=1` (paged sun shadows; off by
//! default so every cascade is redrawn each frame, as in `shadow_occlusion_cost`).

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FogSettings, FoliageSway, FrameMesh, FrameQuality, FrameRenderer,
    FrameScene, IndirectStrategy, LabLocalLight, LabMaterial, LabSpotLight, LabTexture, LabVertex,
    PrepassOcclusionCounters, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    ShadowOcclusionCounters, box_mesh, uv_sphere,
};

/// The bench's checker (`visbuffer_dense_bench.rs`) with the alpha of every dark cell at 0.
fn holed_checker(size: u32, tint: u8) -> LabTexture {
    let mut rgba: Vec<u8> = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = (x / 4 + y / 4).is_multiple_of(2);
            rgba.extend_from_slice(&if on {
                [200, tint, 80, 255]
            } else {
                [40, 80, tint, 0]
            });
        }
    }
    LabTexture {
        width: size,
        height: size,
        rgba,
    }
}

/// What the casters are made of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Casters {
    /// The bench's masked materials (holed alpha, double-sided).
    Masked,
    /// The same, adopted by `FoliageSway` and blown by a wind with the clock running.
    Sway,
    /// The sway with its declared offset bound cleared (`max_vertex_offset` 0): a vertex-stage
    /// graph neither cull can bound.
    SwayUndeclared,
    /// The bench's opaque materials, for comparison.
    Opaque,
}

/// One scene class of the cost table.
#[derive(Clone, Copy, Debug)]
struct Class {
    name: &'static str,
    casters: Casters,
    /// A wall hides the field from the camera and a roof hides it from the sun and the spot.
    hidden: bool,
}

const CLASSES: [Class; 5] = [
    Class {
        name: "masked_hidden",
        casters: Casters::Masked,
        hidden: true,
    },
    Class {
        name: "masked_open",
        casters: Casters::Masked,
        hidden: false,
    },
    Class {
        name: "sway_hidden",
        casters: Casters::Sway,
        hidden: true,
    },
    Class {
        name: "sway_undeclared_hidden",
        casters: Casters::SwayUndeclared,
        hidden: true,
    },
    Class {
        name: "opaque_hidden",
        casters: Casters::Opaque,
        hidden: true,
    },
];

/// The meshes that are not casters of the field: the ground, the wall and the roof.
const OCCLUDERS: u32 = 3;

/// The field's meshes and materials, built once per class.
struct Field {
    caster: (Vec<LabVertex>, Vec<u32>),
    ground: (Vec<LabVertex>, Vec<u32>),
    wall: (Vec<LabVertex>, Vec<u32>),
    roof: (Vec<LabVertex>, Vec<u32>),
    materials: Vec<LabMaterial>,
    plain: LabMaterial,
    lights: Vec<LabLocalLight>,
    instances: u32,
    side: u32,
    hidden: bool,
}

impl Field {
    /// `instances` casters of about `triangles` triangles each over 64 materials (the bench's
    /// `dense_scene`), on a 1 m grid running away from the camera.
    fn new(casters: Casters, hidden: bool, instances: u32, triangles: u32, material_count: u32) -> Self {
        let segments = ((f64::from(triangles)).sqrt() * 1.4).max(4.0) as u32;
        let rings = (segments / 2).max(3);
        let materials = (0..material_count)
            .map(|m| {
                let opaque = LabMaterial {
                    base_color: [0.3 + (m % 7) as f32 * 0.1, 0.3 + (m % 5) as f32 * 0.1, 0.5, 1.0],
                    roughness: 0.3 + (m % 4) as f32 * 0.15,
                    ..LabMaterial::default()
                };
                let masked = LabMaterial {
                    albedo: Some(holed_checker(32 << (m % 3), 60 + (m * 3) as u8)),
                    alpha_mode: ALPHA_MASK,
                    alpha_cutoff: 0.5,
                    double_sided: true,
                    ..opaque.clone()
                };
                match casters {
                    Casters::Opaque => opaque,
                    Casters::Masked => masked,
                    Casters::Sway | Casters::SwayUndeclared => {
                        let mut sway = FoliageSway::default()
                            .adopt(&masked)
                            .expect("a plain material takes the sway");
                        if casters == Casters::SwayUndeclared
                            && let Some(graph) = sway.surface_graph.as_deref()
                        {
                            let mut graph = graph.clone();
                            graph.max_vertex_offset = 0.0;
                            sway.surface_graph = Some(std::sync::Arc::new(graph));
                        }
                        sway
                    }
                }
            })
            .collect();
        let side = f64::from(instances).sqrt().ceil() as u32;
        let half = side as f32 * 0.5;
        Self {
            caster: uv_sphere(0.45, segments, rings),
            ground: box_mesh([half + 4.0, 0.05, half + 4.0]),
            wall: box_mesh([half + 4.0, 8.0, 0.2]),
            roof: box_mesh([half + 4.0, 0.1, half + 4.0]),
            materials,
            plain: LabMaterial {
                base_color: [0.55, 0.5, 0.45, 1.0],
                roughness: 0.8,
                ..LabMaterial::default()
            },
            lights: vec![LabLocalLight::Spot(LabSpotLight {
                position: [0.0, 5.0, -half],
                direction: [0.0, -1.0, 0.0],
                color: [1.0, 0.9, 0.8],
                intensity: 40_000.0,
                range: 30.0,
                inner_angle: 0.6,
                outer_angle: 1.0,
                cast_shadows: true,
                source_radius: None,
            })],
            instances,
            side,
            hidden,
        }
    }

    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh: (u64, &'g (Vec<LabVertex>, Vec<u32>)),
        material: (u64, &'g LabMaterial),
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key: mesh.0,
            material_key: material.0,
            object_key: key,
            vertices: mesh.1.0.as_slice(),
            indices: mesh.1.1.as_slice(),
            material: material.1,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// The ground, the occluders (when `hidden`: a wall 3 m in front of the camera that covers
    /// the whole view, and a roof 3 m over the field that covers it from the sun and the spot),
    /// then the casters.
    fn meshes(&self) -> Vec<FrameMesh<'_>> {
        let half = self.side as f32 * 0.5;
        let mut out = vec![self.mesh(
            1,
            (1, &self.ground),
            (1, &self.plain),
            Vec3::new(0.0, -0.05, -half),
        )];
        if self.hidden {
            out.push(self.mesh(2, (2, &self.wall), (1, &self.plain), Vec3::new(0.0, 4.0, 3.0)));
            out.push(self.mesh(3, (3, &self.roof), (1, &self.plain), Vec3::new(0.0, 3.0, -half)));
        }
        for i in 0..self.instances {
            let material = (i as usize) % self.materials.len();
            let at = Vec3::new(
                (i % self.side) as f32 - half + 0.5,
                0.5,
                -((i / self.side) as f32) - 0.5,
            );
            out.push(self.mesh(
                100_000 + u64::from(i),
                (10, &self.caster),
                (7000 + material as u64, &self.materials[material]),
                at,
            ));
        }
        out
    }

    /// Casters among `meshes()`: everything after the ground and the occluders.
    fn caster_count(&self) -> u32 {
        self.instances
    }
}

fn scene<'a>(field: &'a Field, quality: FrameQuality, (width, height): (u32, u32)) -> FrameScene<'a> {
    let half = field.side as f32 * 0.5;
    FrameScene {
        width,
        height,
        eye: [0.0, 3.0, 6.0],
        target: [0.0, 1.0, -half],
        fov_y: 1.0,
        near: 0.1,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: None,
        fog: FogSettings::default(),
        // The bench's sun is slanted; this one is near overhead so the roof shades the whole field.
        directional: DirectionalLightUniform::new([0.1, -1.0, -0.15], [1.0; 3], 100_000.0),
        local_lights: &field.lights,
        quality,
        meshes: field.meshes(),
    }
}

/// `None` when the device cannot run the GPU-driven passes: neither cull runs there.
fn device() -> Option<RenderDevice> {
    let device = RenderDevice::headless().expect("headless device");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects (no multi-draw indirect with a first instance)");
        return None;
    }
    Some(device)
}

fn mismatched(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(x, y)| x != y).count()
}

/// PCI vendor id of NVIDIA.
const NVIDIA_VENDOR: u32 = 0x10de;

/// How far one RGBA8 channel of a culled frame may sit from the unculled frame on this adapter
/// (ADR-5993, from ADR-5953's repro): 0 (byte-exact) on lavapipe and every other adapter; 1 step
/// (1 ULP of the 8-bit word) on NVIDIA hardware, whose 615 drivers switch a live pipeline between
/// two shader variants whose results differ by 1 ULP with identical inputs, so two runs of one
/// renderer already differ there. Rasterized outputs stay exact on every adapter.
fn channel_tolerance(vendor: u32, device_type: wgpu::DeviceType) -> u8 {
    u8::from(vendor == NVIDIA_VENDOR && device_type != wgpu::DeviceType::Cpu)
}

/// Pixels of `a` and `b` (RGBA8) whose channels differ by more than `tolerance` (exact at 0).
fn mismatched_within(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.chunks(4)
        .zip(b.chunks(4))
        .filter(|(x, y)| x.iter().zip(y.iter()).any(|(p, q)| p.abs_diff(*q) > tolerance))
        .count()
}

/// The card tolerance of `heavy_caster_occlusion_cost` (ADR-5993): exact at 0, one step per
/// channel at 1 and never two, and 1 only for NVIDIA hardware.
#[test]
fn the_card_tolerance_is_one_step_per_channel_and_only_on_nvidia() {
    let a = [10u8, 20, 30, 255, 0, 0, 0, 255];
    let one = [11u8, 19, 30, 255, 0, 0, 1, 255];
    let two = [12u8, 20, 30, 255, 0, 0, 0, 255];
    assert_eq!(mismatched_within(&a, &a, 0), 0);
    assert_eq!(mismatched_within(&a, &one, 0), 2);
    assert_eq!(mismatched_within(&a, &one, 0), mismatched(&a, &one));
    assert_eq!(mismatched_within(&a, &one, 1), 0);
    assert_eq!(mismatched_within(&a, &two, 1), 1);
    assert_eq!(channel_tolerance(NVIDIA_VENDOR, wgpu::DeviceType::DiscreteGpu), 1);
    assert_eq!(
        channel_tolerance(0x10005, wgpu::DeviceType::Cpu),
        0,
        "lavapipe is exact"
    );
    assert_eq!(channel_tolerance(0x1002, wgpu::DeviceType::DiscreteGpu), 0);
    assert_eq!(channel_tolerance(0x8086, wgpu::DeviceType::IntegratedGpu), 0);
}

/// Which cull a run switches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cull {
    Off,
    Prepass,
    Shadow,
}

impl Cull {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Prepass => "prepass_on",
            Self::Shadow => "shadow_on",
        }
    }

    /// A renderer with exactly this cull on. The prepass cull is on in the Desktop High preset
    /// column; the renderer's own switch starts off, so both are set explicitly.
    fn renderer(self, device: &RenderDevice) -> FrameRenderer {
        let mut renderer = FrameRenderer::new();
        // `REGIA_OCC_TIMING=0` (experiment knob): no timestamp queries.
        renderer.set_gpu_timing(
            device,
            !regia_core::brand::env_var("REGIA_OCC_TIMING").is_ok_and(|v| v == "0"),
        );
        renderer.set_alpha_tested_shadows(true);
        renderer.set_foliage_wind(Some([6.0, 2.0]));
        renderer.set_two_phase_occlusion(self == Self::Prepass);
        renderer.set_shadow_occlusion(self == Self::Shadow);
        renderer
    }
}

/// The means of one run's timed frames.
#[derive(Clone, Copy, Default, Debug)]
struct Run {
    frame_ms: f64,
    shadows_ms: f64,
    prepass_ms: f64,
    cull_ms: f64,
    forward_ms: f64,
}

/// A fixed clock step, so every run sways the same frames.
fn tick(renderer: &mut FrameRenderer, frame: u32) {
    let step = 1.0 / 60.0;
    let now = (frame + 1) as f32 * step;
    renderer.set_material_clock(now, now - step);
}

struct Measured {
    run: Run,
    pixels: Vec<u8>,
    prepass: Option<PrepassOcclusionCounters>,
    shadow: Option<ShadowOcclusionCounters>,
    prepass_unbounded: u32,
    shadow_unbounded: u32,
}

fn measure(device: &RenderDevice, scene: &FrameScene<'_>, cull: Cull, frames: u32) -> Measured {
    const WARM: u32 = 8;
    let mut renderer = cull.renderer(device);
    let mut last = Vec::new();
    for frame in 0..WARM {
        tick(&mut renderer, frame);
        renderer.render_offscreen(device, scene).expect("warm-up frame");
    }
    let mut sum = Run::default();
    for frame in WARM..WARM + frames {
        tick(&mut renderer, frame);
        last = renderer.render_offscreen(device, scene).expect("frame").pixels;
        let t = renderer.gpu_timings();
        sum.frame_ms += f64::from(t.frame_ms);
        sum.shadows_ms += f64::from(t.shadows_ms);
        sum.prepass_ms += f64::from(t.prepass_ms);
        sum.cull_ms += f64::from(t.cull_ms);
        sum.forward_ms += f64::from(t.forward_ms);
    }
    let n = f64::from(frames);
    let prepass_stats = renderer.prepass_occlusion_stats();
    let shadow_stats = renderer.shadow_occlusion_stats();
    Measured {
        run: Run {
            frame_ms: sum.frame_ms / n,
            shadows_ms: sum.shadows_ms / n,
            prepass_ms: sum.prepass_ms / n,
            cull_ms: sum.cull_ms / n,
            forward_ms: sum.forward_ms / n,
        },
        pixels: last,
        prepass: (cull == Cull::Prepass)
            .then(|| renderer.prepass_occlusion_counters(device).ok().flatten())
            .flatten(),
        shadow: (cull == Cull::Shadow)
            .then(|| renderer.shadow_occlusion_counters(device).ok().flatten())
            .flatten(),
        prepass_unbounded: prepass_stats.unbounded,
        shadow_unbounded: shadow_stats.kinds.iter().map(|k| k.unbounded).sum(),
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    match values.len() {
        0 => 0.0,
        n if !n.is_multiple_of(2) => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2.0,
    }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(default)
}

fn quality(paged: bool) -> FrameQuality {
    let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    FrameQuality {
        csm_cascade_count: 4,
        csm_resolution: 2048,
        local_shadow_atlas_size: 2048,
        paged_sun_shadows: paged,
        // The forward path: the visibility buffer culls its clusters in two phases of its own.
        visibility_buffer: false,
        ..base
    }
}

/// The heavy-caster cost table (`--ignored`; prints, asserts only that the culled frames equal
/// the unculled ones). For each class, `rounds` interleaved rounds of three runs (off, prepass
/// cull on, shadow cull on; the order rotates every round so a card's clock moves every run
/// alike), each run a fresh renderer: 8 warm-up frames, then `frames` frames whose GPU
/// timestamps are averaged. Then the medians over the rounds and the on - off differences.
#[test]
#[ignore = "GPU timestamp printout; run on a card"]
fn heavy_caster_occlusion_cost() {
    let Some(device) = device() else {
        return;
    };
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    let size = regia_core::brand::env_var("REGIA_OCC_GAIN_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((2560u32, 1440u32));
    let rounds: u32 = env_or("REGIA_OCC_GAIN_ROUNDS", 3u32).max(1);
    let frames: u32 = env_or("REGIA_OCC_GAIN_FRAMES", 24u32).max(24);
    let instances: u32 = env_or("REGIA_OCC_GAIN_INSTANCES", 4096u32).max(1);
    let paged = regia_core::brand::env_var("REGIA_OCC_GAIN_PAGED").is_ok_and(|v| v == "1");
    let wanted: Vec<String> = regia_core::brand::env_var("REGIA_OCC_GAIN_SCENES")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    println!(
        "--- {}x{}, {instances} instances x ~2048 triangles, 64 materials, alpha-tested shadows, \
         4 x 2048^2 cascades + 2048^2 atlas, paged sun shadows {paged}, {rounds} rounds x {frames} frames",
        size.0, size.1
    );
    // Byte-exact on lavapipe and every deterministic adapter; at most 1 ULP per 8-bit word on
    // NVIDIA hardware, where one pipeline alternates between two results (ADR-5953, ADR-5993).
    let tolerance = channel_tolerance(info.vendor, info.device_type);
    let mut mismatches = Vec::new();
    let mut summary = Vec::new();
    for class in CLASSES {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == class.name) {
            continue;
        }
        let field = Field::new(class.casters, class.hidden, instances, 2048, 64);
        let s = scene(&field, quality(paged), size);
        let culls = [Cull::Off, Cull::Prepass, Cull::Shadow];
        let mut runs: [Vec<Run>; 3] = Default::default();
        for round in 0..rounds {
            let mut reference: Option<Vec<u8>> = None;
            let mut pending: Vec<(Cull, Vec<u8>)> = Vec::new();
            for k in 0..culls.len() {
                let cull = culls[(k + round as usize) % culls.len()];
                let m = measure(&device, &s, cull, frames);
                let r = m.run;
                println!(
                    "{} round {round} {:>10}: frame {:8.3} ms, shadows {:7.3}, prepass {:7.3}, colour cull {:6.3}, \
                     forward {:7.3} (mean of {frames}); prepass phases {:?}, shadow phases {:?}, \
                     unbounded prepass {} shadow {}",
                    class.name,
                    cull.name(),
                    r.frame_ms,
                    r.shadows_ms,
                    r.prepass_ms,
                    r.cull_ms,
                    r.forward_ms,
                    m.prepass,
                    m.shadow.map(|c| c.total),
                    m.prepass_unbounded,
                    m.shadow_unbounded,
                );
                let slot = culls.iter().position(|c| *c == cull).unwrap_or(0);
                runs[slot].push(r);
                if cull == Cull::Off {
                    reference = Some(m.pixels);
                } else {
                    pending.push((cull, m.pixels));
                }
            }
            if let Some(reference) = reference {
                for (cull, pixels) in pending {
                    let differ = mismatched_within(&reference, &pixels, tolerance);
                    if differ != 0 {
                        mismatches.push(format!(
                            "{} round {round} {}: {differ} pixels differ by more than {tolerance} per channel \
                             ({} driver {} {})",
                            class.name,
                            cull.name(),
                            info.name,
                            info.driver,
                            info.driver_info
                        ));
                    }
                }
            }
        }
        let med = |slot: usize, pick: fn(&Run) -> f64| {
            let mut values: Vec<f64> = runs[slot].iter().map(pick).collect();
            median(&mut values)
        };
        let (off_frame, pre_frame, sh_frame) = (
            med(0, |r| r.frame_ms),
            med(1, |r| r.frame_ms),
            med(2, |r| r.frame_ms),
        );
        let off_depth = med(0, |r| r.prepass_ms + r.cull_ms);
        let pre_depth = med(1, |r| r.prepass_ms + r.cull_ms);
        let (off_shadows, sh_shadows) = (med(0, |r| r.shadows_ms), med(2, |r| r.shadows_ms));
        let line = format!(
            "| {} | {} | {off_frame:.3} | {pre_frame:.3} ({:+.3}) | {sh_frame:.3} ({:+.3}) | \
             {off_depth:.3} -> {pre_depth:.3} ({:+.3}) | {off_shadows:.3} -> {sh_shadows:.3} ({:+.3}) |",
            class.name,
            field.caster_count(),
            pre_frame - off_frame,
            sh_frame - off_frame,
            pre_depth - off_depth,
            sh_shadows - off_shadows,
        );
        println!("median {line}");
        summary.push(line);
    }
    println!("=== medians over {rounds} rounds, GPU ms (on - off in brackets)");
    println!(
        "| Class | Casters | Frame off | Frame prepass cull | Frame shadow cull | Prepass + colour cull | Shadows |"
    );
    println!("|---|---|---|---|---|---|---|");
    for line in &summary {
        println!("{line}");
    }
    assert!(
        mismatches.is_empty(),
        "a culled frame differs from the unculled frame: {mismatches:?}"
    );
}

/// FNV-1a over a byte slice: a cheap per-frame fingerprint.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Switches named features off (experiment knob `REGIA_OCC_OFF`, a comma list).
fn features_off(mut q: FrameQuality, off: &[String]) -> FrameQuality {
    for name in off {
        match name.as_str() {
            "ssgi" => q.screen_space_gi = false,
            "gi_probes" => q.gi_probes = false,
            "gi" => {
                q.gi = regia_render::GiMode::Ambient;
                q.screen_space_gi = false;
                q.gi_probes = false;
            }
            "restir" => q.restir_gi = false,
            "ssr" => q.ssr = false,
            "taau" => q.taau = false,
            "contact" => q.contact_shadows = false,
            "blue_noise" => q.blue_noise = false,
            "horizon_ao" => q.horizon_ao = false,
            "ssao" => q.ssao = regia_render::SsaoMode::Off,
            "refl_occ" => q.reflection_occlusion = false,
            "bent" => q.bent_normals = false,
            "velocity" => q.depth_velocity = false,
            "many_light" => q.many_light_sampling = false,
            "spec_aa" => q.specular_aa = false,
            "smaa" => q.smaa = false,
            "streaming" => q.texture_streaming = regia_render::TextureStreaming::OFF,
            "fog" => q.volumetric_fog = false,
            "clouds" => q.volumetric_clouds = false,
            "atmosphere" => q.physical_atmosphere = false,
            "refl_probes" => q.reflection_probes = false,
            "gpu_driven" => q.gpu_driven = false,
            "" => {}
            other => eprintln!("REGIA_OCC_OFF: unknown feature {other}"),
        }
    }
    q
}

/// The calling thread's floating-point environment, probed by arithmetic: bit 0 = rounding is not
/// to-nearest (1 + 0.75 eps rounds down), bit 1 = flush-to-zero (a subnormal result becomes 0),
/// bit 2 = denormals-are-zero (a subnormal input reads as 0), bit 3 = rounding is upward
/// (1 + 0.25 eps rounds up). 0 is the default environment.
fn fp_env() -> u32 {
    use std::hint::black_box;
    let one = black_box(1.0f32);
    let mut env = 0;
    if one + black_box(f32::EPSILON * 0.75) == one {
        env |= 1;
    }
    if one + black_box(f32::EPSILON * 0.25) != one {
        env |= 8;
    }
    if black_box(f32::MIN_POSITIVE) * black_box(0.5f32) == 0.0 {
        env |= 2;
    }
    if black_box(f32::MIN_POSITIVE * 0.5) * black_box(2.0f32) == 0.0 {
        env |= 4;
    }
    env
}

/// A differing pixel: its position, run 0's RGBA and this run's.
type PixelDiff = ((u32, u32), [u8; 4], [u8; 4]);

/// One stress run's fingerprints.
struct RepeatRun {
    frame_hashes: Vec<u64>,
    map_hashes: Vec<Option<u64>>,
    maps: Vec<Option<Vec<f32>>>,
    pixels: Vec<u8>,
    /// Per frame (`REGIA_OCC_DDGI=1`): hashes of the probes' rays, probe words and screen lookup.
    ddgi: Vec<[u64; 3]>,
    /// Per frame (`REGIA_OCC_DEPTH=1`): hashes of the scene depth and the motion vectors.
    depth: Vec<[u64; 2]>,
    /// The DDGI screen lookup's texel words per frame (run 0 with `REGIA_OCC_KEEP=1`).
    lookups: Vec<Vec<u32>>,
    /// The scene depth per frame (run 0 with `REGIA_OCC_KEEP=1`).
    depths: Vec<Vec<f32>>,
    /// Per frame (`REGIA_OCC_DDGI=1`): the probes' uniform blocks, `[screen CPU, screen GPU, probes
    /// CPU, probes GPU]` (`FrameRenderer::ddgi_uniforms_read_back`).
    uniforms: Vec<[Vec<u8>; 4]>,
    /// Per frame (`REGIA_OCC_RERUN=<n>`): the lookup's hash after each of `n` reruns of the lookup
    /// alone (`FrameRenderer::ddgi_rerun_screen_lookup`).
    reruns: Vec<Vec<u64>>,
}

/// Hashes of the probes' last rays, their data words before the screen lookup, and the lookup;
/// with the lookup's texel words (8 per texel, `ddgi_screen.wgsl`) and its width.
fn ddgi_hashes(renderer: &FrameRenderer, device: &RenderDevice) -> ([u64; 3], Vec<u32>, u32) {
    let Ok(Some(r)) = renderer.ddgi_read_back(device) else {
        return ([0; 3], Vec::new(), 0);
    };
    let rays: Vec<u8> = r
        .rays
        .iter()
        .flat_map(|v| v.iter().flat_map(|f| f.to_bits().to_le_bytes()))
        .collect();
    let split = if r.screen[1] > 0 {
        (r.screen[0] as usize).min(r.data.len())
    } else {
        r.data.len()
    };
    let words = |w: &[u32]| -> u64 {
        let bytes: Vec<u8> = w.iter().flat_map(|v| v.to_le_bytes()).collect();
        fnv(&bytes)
    };
    let texels = (r.screen[1] as usize * r.screen[2] as usize * 8).min(r.data.len() - split);
    (
        [fnv(&rays), words(&r.data[..split]), words(&r.data[split..])],
        r.data[split..split + texels].to_vec(),
        r.screen[1],
    )
}

/// Writes an RGBA frame as a binary PPM (RGB) for a by-eye look at a mismatch.
fn write_ppm(path: &std::path::Path, (width, height): (u32, u32), rgba: &[u8]) {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    for px in rgba.chunks(4) {
        out.extend_from_slice(&px[..3]);
    }
    if let Err(err) = std::fs::write(path, out) {
        eprintln!("dump {}: {err}", path.display());
    }
}

/// A03 stress (`--ignored`, a card): renders one scene class `REGIA_OCC_REPEAT_RUNS` times (default 8),
/// each run a fresh renderer with the cull `REGIA_OCC_REPEAT_CULL` (`off` by default) and the cost
/// table's frame sequence (8 warm-up frames, then 24), and checks that every run repeats the first one:
/// every frame's pixels and the final shadow maps. Prints the first frame that differs, the differing
/// pixels' bounds and a few values; `REGIA_OCC_DUMP=<dir>` writes both frames as PPM.
#[test]
#[ignore = "A03 determinism stress; run on a card"]
fn culled_scene_repeats_run_to_run() {
    let Some(device) = device() else {
        return;
    };
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    let runs: u32 = env_or("REGIA_OCC_REPEAT_RUNS", 8u32).max(2);
    let frames: u32 = env_or("REGIA_OCC_GAIN_FRAMES", 24u32).max(1);
    let instances: u32 = env_or("REGIA_OCC_GAIN_INSTANCES", 4096u32).max(1);
    let class_name =
        regia_core::brand::env_var("REGIA_OCC_REPEAT_CLASS").unwrap_or_else(|_| "sway_hidden".into());
    let cull = match regia_core::brand::env_var("REGIA_OCC_REPEAT_CULL").as_deref() {
        Ok("prepass") => Cull::Prepass,
        Ok("shadow") => Cull::Shadow,
        _ => Cull::Off,
    };
    let off: Vec<String> = regia_core::brand::env_var("REGIA_OCC_OFF")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let every_frame_maps = regia_core::brand::env_var("REGIA_OCC_EVERY_FRAME_MAPS").is_ok_and(|v| v == "1");
    let dump = regia_core::brand::env_var("REGIA_OCC_DUMP")
        .ok()
        .map(std::path::PathBuf::from);
    let Some(class) = CLASSES.iter().find(|c| c.name == class_name) else {
        panic!("REGIA_OCC_REPEAT_CLASS {class_name}: no such class");
    };
    let size = (2560u32, 1440u32);
    let field = Field::new(class.casters, class.hidden, instances, 2048, 64);
    let s = scene(&field, features_off(quality(false), &off), size);
    println!(
        "--- {} {} x {runs} runs, {frames} frames, off {off:?}",
        class.name,
        cull.name()
    );
    const WARM: u32 = 8;
    let kinds = [0usize, 1, 2, 3, 4];
    let mut first: Option<RepeatRun> = None;
    let mut bad = Vec::new();
    // `REGIA_OCC_KEEP=1`: keep run 0's frames to list the first differing frame's pixels.
    let keep = regia_core::brand::env_var("REGIA_OCC_KEEP").is_ok_and(|v| v == "1");
    let ddgi = regia_core::brand::env_var("REGIA_OCC_DDGI").is_ok_and(|v| v == "1");
    let depth_hash = regia_core::brand::env_var("REGIA_OCC_DEPTH").is_ok_and(|v| v == "1");
    // `REGIA_OCC_RERUN=<n>` (with `REGIA_OCC_DDGI=1`): after every frame, run the lookup alone n more times.
    let reruns: u32 = env_or("REGIA_OCC_RERUN", 0u32);
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for run in 0..runs {
        let mut renderer = cull.renderer(&device);
        let mut out = RepeatRun {
            frame_hashes: Vec::new(),
            map_hashes: Vec::new(),
            maps: Vec::new(),
            pixels: Vec::new(),
            ddgi: Vec::new(),
            depth: Vec::new(),
            depths: Vec::new(),
            lookups: Vec::new(),
            uniforms: Vec::new(),
            reruns: Vec::new(),
        };
        let mut frame_map_hashes: Vec<u64> = Vec::new();
        let mut seen_first = false;
        let mut seen_depth = false;
        let mut seen_lookup = false;
        for frame in 0..WARM + frames {
            tick(&mut renderer, frame);
            // The test thread's floating-point environment (`fp_env`: 0 is the default one).
            let csr_before = fp_env();
            out.pixels = renderer.render_offscreen(&device, &s).expect("frame").pixels;
            let csr_after = fp_env();
            if csr_before != 0 || csr_after != 0 {
                println!(
                    "run {run}: frame {frame} floating-point environment before {csr_before:#x} after {csr_after:#x}"
                );
            }
            out.frame_hashes.push(fnv(&out.pixels));
            if ddgi {
                let (hashes, lookup, lookup_w) = ddgi_hashes(&renderer, &device);
                out.ddgi.push(hashes);
                out.uniforms.push(
                    renderer
                        .ddgi_uniforms_read_back(&device)
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                );
                // The CPU's bytes and the GPU buffer's must agree in every run, and both must repeat run 0.
                if let Some(u) = out.uniforms.last() {
                    const NAMES: [&str; 4] = ["screen CPU", "screen GPU", "probes CPU", "probes GPU"];
                    if u[0] != u[1] || u[2] != u[3] {
                        println!(
                            "run {run}: frame {frame} uniform CPU and GPU bytes differ: screen {} probes {}",
                            u[0] != u[1],
                            u[2] != u[3]
                        );
                    }
                    if let Some(expected) = first.as_ref().and_then(|r| r.uniforms.get(frame as usize)) {
                        for k in 0..4 {
                            if expected[k] != u[k] {
                                let words = |b: &[u8]| -> Vec<u32> {
                                    b.chunks(4)
                                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                        .collect()
                                };
                                let (a, b) = (words(&expected[k]), words(&u[k]));
                                let list: Vec<(usize, u32, u32)> = a
                                    .iter()
                                    .zip(&b)
                                    .enumerate()
                                    .filter(|(_, (x, y))| x != y)
                                    .take(24)
                                    .map(|(i, (x, y))| (i, *x, *y))
                                    .collect();
                                println!(
                                    "run {run}: frame {frame} uniform block {} differs from run 0 (word, run0, this) {list:x?}",
                                    NAMES[k]
                                );
                            }
                        }
                    }
                }
                if let Some(reference) = &first
                    && !seen_lookup
                    && reference.ddgi.get(frame as usize).map(|h| h[2]) != Some(hashes[2])
                    && let Some(expected) = reference.lookups.get(frame as usize)
                {
                    seen_lookup = true;
                    let mut per_word = [0usize; 8];
                    let mut list = Vec::new();
                    for (i, (a, b)) in expected.iter().zip(&lookup).enumerate() {
                        if a != b {
                            per_word[i % 8] += 1;
                            if list.len() < 24 {
                                let t = (i / 8) as u32;
                                list.push((t % lookup_w.max(1), t / lookup_w.max(1), i % 8, *a, *b));
                            }
                        }
                    }
                    println!(
                        "run {run}: frame {frame} lookup words differ per word slot {per_word:?}; (x, y, word, run0, this) {list:x?}"
                    );
                    if let Some(u) = out.uniforms.last() {
                        let hex = |b: &[u8]| -> String {
                            b.chunks(4)
                                .map(|c| format!("{:08x}", u32::from_le_bytes([c[0], c[1], c[2], c[3]])))
                                .collect::<Vec<_>>()
                                .join(" ")
                        };
                        println!("run {run}: frame {frame} screen params (CPU) {}", hex(&u[0]));
                        println!("run {run}: frame {frame} screen params (GPU) {}", hex(&u[1]));
                    }
                    // The depth under each listed texel (this run's, after the frame; run 0's when kept).
                    if let Ok(depth) = renderer.read_scene_depth(&device, size.0, size.1) {
                        let step = (size.0 / lookup_w.max(1)).max(1);
                        let kept = reference.depths.get(frame as usize);
                        let at: Vec<(u32, u32, u32, Option<u32>)> = list
                            .iter()
                            .filter(|t| t.2 == 6)
                            .map(|t| {
                                let i = ((t.1 * step) * size.0 + t.0 * step) as usize;
                                (
                                    t.0,
                                    t.1,
                                    depth.get(i).map_or(0, |d| d.to_bits()),
                                    kept.and_then(|k| k.get(i)).map(|d| d.to_bits()),
                                )
                            })
                            .collect();
                        println!(
                            "run {run}: frame {frame} depth bits under the texels (x, y, this, run0) {at:x?}"
                        );
                    }
                }
                if keep && first.is_none() {
                    out.lookups.push(lookup);
                }
                // The lookup again, alone in its own submits: the same pipeline, parameters and depth.
                let mut again = Vec::new();
                for _ in 0..reruns {
                    if renderer.ddgi_rerun_screen_lookup(&device).unwrap_or(false) {
                        again.push(ddgi_hashes(&renderer, &device).0[2]);
                    }
                }
                out.reruns.push(again);
            }
            if depth_hash {
                let depth = renderer
                    .read_scene_depth(&device, size.0, size.1)
                    .unwrap_or_default();
                let depth_bytes: Vec<u8> = depth.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect();
                let motion: Vec<u8> = renderer
                    .read_motion(&device)
                    .ok()
                    .flatten()
                    .map(|m| {
                        m.motion
                            .iter()
                            .flat_map(|v| v.iter().flat_map(|f| f.to_bits().to_le_bytes()))
                            .collect()
                    })
                    .unwrap_or_default();
                out.depth.push([fnv(&depth_bytes), fnv(&motion)]);
                if let Some(reference) = &first
                    && let Some(expected) = reference.depths.get(frame as usize)
                    && reference.depth.get(frame as usize).map(|h| h[0]) != Some(fnv(&depth_bytes))
                    && !seen_depth
                {
                    seen_depth = true;
                    let list: Vec<(u32, u32, f32, f32)> = expected
                        .iter()
                        .zip(&depth)
                        .enumerate()
                        .filter(|(_, (a, b))| a.to_bits() != b.to_bits())
                        .take(40)
                        .map(|(i, (a, b))| (i as u32 % size.0, i as u32 / size.0, *a, *b))
                        .collect();
                    println!("run {run}: frame {frame} depth first differs: {list:?}");
                }
                if keep && first.is_none() {
                    out.depths.push(depth);
                }
            }
            if keep && first.is_none() {
                kept.push(out.pixels.clone());
            }
            // The first frame that differs from run 0: every differing pixel, before the
            // temporal history spreads it.
            if let Some(reference) = &first
                && !seen_first
                && reference.frame_hashes.get(frame as usize) != out.frame_hashes.last()
                && let Some(expected) = kept.get(frame as usize)
            {
                seen_first = true;
                let (w, _) = size;
                let list: Vec<PixelDiff> = expected
                    .chunks(4)
                    .zip(out.pixels.chunks(4))
                    .enumerate()
                    .filter(|(_, (a, b))| a != b)
                    .take(400)
                    .map(|(i, (a, b))| {
                        (
                            (i as u32 % w, i as u32 / w),
                            [a[0], a[1], a[2], a[3]],
                            [b[0], b[1], b[2], b[3]],
                        )
                    })
                    .collect();
                println!(
                    "run {run}: frame {frame} first differs in {} px: {list:?}",
                    mismatched(expected, &out.pixels)
                );
                if let Some(dir) = &dump {
                    let _ = std::fs::create_dir_all(dir);
                    write_ppm(
                        &dir.join(format!("run{run}-frame{frame}-expected.ppm")),
                        size,
                        expected,
                    );
                    write_ppm(
                        &dir.join(format!("run{run}-frame{frame}-got.ppm")),
                        size,
                        &out.pixels,
                    );
                }
            }
            if every_frame_maps {
                let mut h = 0u64;
                for kind in kinds {
                    if let Ok(Some(map)) = renderer.shadow_map_depth(&device, kind) {
                        let bytes: Vec<u8> = map.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect();
                        h = h.rotate_left(7) ^ fnv(&bytes);
                    }
                }
                frame_map_hashes.push(h);
            }
        }
        for kind in kinds {
            let map = renderer.shadow_map_depth(&device, kind).ok().flatten();
            out.map_hashes.push(map.as_ref().map(|m| {
                let bytes: Vec<u8> = m.iter().flat_map(|v| v.to_bits().to_le_bytes()).collect();
                fnv(&bytes)
            }));
            out.maps.push(map);
        }
        // Each rerun against the SAME run's in-frame lookup (`.` same, `X` differs, `-` no rerun).
        for k in 0..reruns as usize {
            let line: String = out
                .reruns
                .iter()
                .zip(&out.ddgi)
                .map(|(again, inframe)| match again.get(k) {
                    Some(h) if *h == inframe[2] => '.',
                    Some(_) => 'X',
                    None => '-',
                })
                .collect();
            println!("run {run}: pattern rerun {k} vs own frame {line}");
        }
        let Some(reference) = &first else {
            println!(
                "run 0: final {:016x}, maps {:x?}",
                out.frame_hashes[out.frame_hashes.len() - 1],
                out.map_hashes
            );
            if every_frame_maps {
                println!("run 0: frame map hashes {frame_map_hashes:x?}");
            }
            first = Some(out);
            continue;
        };
        // Which frames differ from run 0 (`.` same, `X` differs): the image, then the screen lookup.
        let pattern = |same: &mut dyn Iterator<Item = bool>| -> String {
            same.map(|s| if s { '.' } else { 'X' }).collect()
        };
        println!(
            "run {run}: pattern image  {}",
            pattern(
                &mut reference
                    .frame_hashes
                    .iter()
                    .zip(&out.frame_hashes)
                    .map(|(a, b)| a == b)
            )
        );
        if ddgi {
            println!(
                "run {run}: pattern lookup {}",
                pattern(&mut reference.ddgi.iter().zip(&out.ddgi).map(|(a, b)| a[2] == b[2]))
            );
            // Each rerun against run 0's in-frame lookup of that frame.
            for k in 0..reruns as usize {
                let line: String = out
                    .reruns
                    .iter()
                    .zip(&reference.ddgi)
                    .map(|(again, run0)| match again.get(k) {
                        Some(h) if *h == run0[2] => '.',
                        Some(_) => 'X',
                        None => '-',
                    })
                    .collect();
                println!("run {run}: pattern rerun {k} vs run 0 frame {line}");
            }
        }
        if ddgi {
            let firsts: Vec<Option<usize>> = (0..3)
                .map(|k| {
                    reference
                        .ddgi
                        .iter()
                        .zip(&out.ddgi)
                        .position(|(a, b)| a[k] != b[k])
                })
                .collect();
            println!("run {run}: ddgi first differing frame (rays, probe words, screen lookup) {firsts:?}");
        }
        if depth_hash {
            let firsts: Vec<Option<usize>> = (0..2)
                .map(|k| {
                    reference
                        .depth
                        .iter()
                        .zip(&out.depth)
                        .position(|(a, b)| a[k] != b[k])
                })
                .collect();
            println!("run {run}: depth / motion first differing frame {firsts:?}");
        }
        let first_frame = reference
            .frame_hashes
            .iter()
            .zip(&out.frame_hashes)
            .position(|(a, b)| a != b);
        let map_diff: Vec<(usize, usize)> = kinds
            .iter()
            .enumerate()
            .filter_map(|(i, kind)| match (&reference.maps[i], &out.maps[i]) {
                (Some(a), Some(b)) => {
                    let n = a
                        .iter()
                        .zip(b)
                        .filter(|(x, y)| x.to_bits() != y.to_bits())
                        .count();
                    (n != 0).then_some((*kind, n))
                }
                (None, None) => None,
                _ => Some((*kind, usize::MAX)),
            })
            .collect();
        let differ = mismatched(&reference.pixels, &out.pixels);
        println!(
            "run {run}: first differing frame {first_frame:?}, final pixels differ {differ}, map texels differ {map_diff:?}"
        );
        if every_frame_maps {
            println!("run {run}: frame map hashes {frame_map_hashes:x?}");
        }
        if first_frame.is_some() || !map_diff.is_empty() {
            let (w, _) = size;
            let mut bounds = (u32::MAX, u32::MAX, 0u32, 0u32);
            let mut samples = Vec::new();
            for (i, (a, b)) in reference.pixels.chunks(4).zip(out.pixels.chunks(4)).enumerate() {
                if a != b {
                    let (x, y) = (i as u32 % w, i as u32 / w);
                    bounds = (bounds.0.min(x), bounds.1.min(y), bounds.2.max(x), bounds.3.max(y));
                    if samples.len() < 16 {
                        samples.push(((x, y), a.to_vec(), b.to_vec()));
                    }
                }
            }
            println!(
                "run {run}: differing pixels in x {}..={} y {}..={}; samples {samples:?}",
                bounds.0, bounds.2, bounds.1, bounds.3
            );
            for (i, kind) in kinds.iter().enumerate() {
                if let (Some(a), Some(b)) = (&reference.maps[i], &out.maps[i]) {
                    let side = (a.len() as f64).sqrt() as usize;
                    let texels: Vec<(usize, usize, f32, f32)> = a
                        .iter()
                        .zip(b)
                        .enumerate()
                        .filter(|(_, (x, y))| x.to_bits() != y.to_bits())
                        .take(8)
                        .map(|(t, (x, y))| (t % side.max(1), t / side.max(1), *x, *y))
                        .collect();
                    if !texels.is_empty() {
                        println!("run {run}: map {kind} texels (x, y, run0, this) {texels:?}");
                    }
                }
            }
            if let Some(dir) = &dump {
                let _ = std::fs::create_dir_all(dir);
                write_ppm(
                    &dir.join(format!("{}-run0.ppm", class.name)),
                    size,
                    &reference.pixels,
                );
                write_ppm(
                    &dir.join(format!("{}-run{run}.ppm", class.name)),
                    size,
                    &out.pixels,
                );
                let mask: Vec<u8> = reference
                    .pixels
                    .chunks(4)
                    .zip(out.pixels.chunks(4))
                    .flat_map(|(a, b)| {
                        if a == b {
                            [0u8, 0, 0, 255]
                        } else {
                            [255, 0, 255, 255]
                        }
                    })
                    .collect();
                write_ppm(
                    &dir.join(format!("{}-run{run}-diff.ppm", class.name)),
                    size,
                    &mask,
                );
            }
            bad.push(format!(
                "run {run}: frame {first_frame:?}, {differ} px, maps {map_diff:?}"
            ));
        }
    }
    assert!(bad.is_empty(), "runs differ from run 0: {bad:?}");
}

/// A hidden field of `casters` in both culls (on lavapipe, in the gate; one test per caster kind,
/// so they run in parallel). A swaying material is a vertex-stage graph, which moves its vertices
/// past the mesh bound. A wind sway declares its largest offset
/// (`SurfaceGraph::max_vertex_offset`), so both culls test it on the mesh bound grown by that
/// offset (ADR-5389), as the colour pass's cull does: behind the wall and under the roof most
/// swaying casters are culled, like the masked field that does not sway. A vertex graph that
/// declares no offset has no safe bound (ADR-1887, ADR-2072): every such caster is `unbounded`
/// and draws in phase 1. In every case, over frames whose sway moves, the culled frame equals the
/// unculled one byte for byte and the shadow maps are texel-equal.
fn hidden_field_culls_exactly(casters: Casters) {
    let Some(device) = device() else {
        return;
    };
    // 64 x 64 casters, ~30-triangle spheres: more than the prepass cull's 2 048-instance threshold
    // stay inside the frustum.
    let instances = 64 * 64;
    let small = FrameQuality {
        csm_cascade_count: 2,
        csm_resolution: 512,
        local_shadow_atlas_size: 512,
        taau: false,
        ..quality(false)
    };
    let field = Field::new(casters, true, instances, 16, 4);
    let s = scene(&field, small, (96, 64));
    let mut plain = Cull::Off.renderer(&device);
    let mut prepass = Cull::Prepass.renderer(&device);
    let mut shadow = Cull::Shadow.renderer(&device);
    shadow.set_shadow_occlusion_min_casters(Some(1));
    for frame in 0..4 {
        for renderer in [&mut plain, &mut prepass, &mut shadow] {
            tick(renderer, frame * 7);
        }
        let reference = plain.render_offscreen(&device, &s).expect("plain frame").pixels;
        let a = prepass
            .render_offscreen(&device, &s)
            .expect("prepass-culled frame")
            .pixels;
        let b = shadow
            .render_offscreen(&device, &s)
            .expect("shadow-culled frame")
            .pixels;
        assert_eq!(
            mismatched(&reference, &a),
            0,
            "{casters:?} frame {frame}: prepass cull"
        );
        assert_eq!(
            mismatched(&reference, &b),
            0,
            "{casters:?} frame {frame}: shadow cull"
        );
        for kind in [0, 1, 4] {
            let culled = shadow
                .shadow_map_depth(&device, kind)
                .expect("readback")
                .expect("map");
            let unculled = plain
                .shadow_map_depth(&device, kind)
                .expect("readback")
                .expect("map");
            let differ = culled
                .iter()
                .zip(&unculled)
                .filter(|(x, y)| x.to_bits() != y.to_bits())
                .count();
            assert_eq!(differ, 0, "{casters:?} frame {frame}: kind {kind} map");
        }
    }
    let p = prepass.prepass_occlusion_stats();
    assert!(p.active && p.history, "{casters:?}: {p:?}");
    let pc = prepass
        .prepass_occlusion_counters(&device)
        .expect("readback")
        .expect("a two-phase frame ran");
    let sh = shadow.shadow_occlusion_stats();
    assert!(sh.active, "{casters:?}: {sh:?}");
    let sc = shadow
        .shadow_occlusion_counters(&device)
        .expect("readback")
        .expect("a culled shadow frame ran");
    let shadow_unbounded: u32 = sh.kinds.iter().map(|k| k.unbounded).sum();
    let shadow_instances: u32 = sh.kinds.iter().filter(|k| k.active).map(|k| k.instances).sum();
    eprintln!("{casters:?}: prepass {p:?} {pc:?}; shadow {sh:?} {:?}", sc.total);
    match casters {
        Casters::SwayUndeclared => {
            // Every swaying caster in view is unbounded; only the ground, the wall and the
            // roof (`OCCLUDERS`) have a bound, so only they can be culled.
            assert!(
                p.unbounded > 0 && p.instances - p.unbounded <= OCCLUDERS,
                "every undeclared vertex graph is unbounded in the prepass: {p:?}"
            );
            assert!(
                pc.culled <= OCCLUDERS && pc.phase1 >= p.unbounded,
                "the prepass drew every unbounded caster in phase 1: {pc:?} {p:?}"
            );
            for (kind, k) in sh.kinds.iter().enumerate().filter(|(_, k)| k.active) {
                assert!(
                    k.unbounded > 0 && k.instances - k.unbounded <= OCCLUDERS,
                    "kind {kind}: every undeclared vertex graph is unbounded: {k:?}"
                );
            }
            let active = sh.kinds.iter().filter(|k| k.active).count() as u32;
            assert!(
                sc.total.culled <= OCCLUDERS * active,
                "no unbounded caster is culled from a shadow map: {:?}",
                sc.total
            );
        }
        Casters::Masked | Casters::Sway | Casters::Opaque => {
            assert_eq!(p.unbounded, 0, "{casters:?}: {p:?}");
            assert_eq!(shadow_unbounded, 0, "{casters:?}: {sh:?}");
            assert!(
                pc.culled * 2 > p.instances,
                "{casters:?}: the wall hides most of the field from the prepass: {pc:?} {p:?}"
            );
            assert!(
                sc.total.culled * 2 > shadow_instances,
                "{casters:?}: the roof hides most of the field from the shadow maps: {:?} of {shadow_instances}",
                sc.total
            );
        }
    }
}

#[test]
fn masked_casters_cull_behind_and_under_occluders() {
    hidden_field_culls_exactly(Casters::Masked);
}

#[test]
fn swaying_casters_cull_on_their_declared_bound() {
    hidden_field_culls_exactly(Casters::Sway);
}

#[test]
fn vertex_graphs_without_a_declared_offset_stay_unbounded() {
    hidden_field_culls_exactly(Casters::SwayUndeclared);
}

/// The DDGI screen lookup's position math (`ddgi_screen.wgsl` `world_at` and the distance word) on fixed
/// inputs: the experiment of A03's driver question.
const REPRO_WGSL: &str = r"
struct Params { m: mat4x4<f32>, eye: vec4<f32> }
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> samples: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> out: array<u32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= arrayLength(&samples)) {
        return;
    }
    let s = samples[i];
    let clip = vec4<f32>(s.x * 2.0 - 1.0, 1.0 - s.y * 2.0, s.z, 1.0);
    let w = params.m * clip;
    out[i] = bitcast<u32>(length(params.eye.xyz - w.xyz / w.w));
}
";

/// One dispatch of `pipeline` over the fixed inputs; the hash of the output bits.
fn repro_dispatch(
    device: &RenderDevice,
    pipeline: &wgpu::ComputePipeline,
    buffers: (&wgpu::Buffer, &wgpu::Buffer, &wgpu::Buffer, &wgpu::Buffer),
    count: u32,
) -> u64 {
    let (params, samples, out, readback) = buffers;
    let d = &device.device;
    let layout = pipeline.get_bind_group_layout(0);
    let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("a03 repro"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: samples.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: out.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("a03 repro"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("a03 repro"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(out, 0, readback, 0, u64::from(count) * 4);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let hash = fnv(&readback.slice(..).get_mapped_range().expect("range"));
    readback.unmap();
    hash
}

fn repro_pipeline(device: &RenderDevice, source: &str) -> wgpu::ComputePipeline {
    let module = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("a03 repro"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    device
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("a03 repro"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        })
}

/// A03 driver experiment (`--ignored`, a card): the lookup's position math on fixed inputs, dispatched
/// `REGIA_OCC_REPRO_ITERS` times (default 400) with one pipeline, and every tenth time with a pipeline
/// built anew from the same source, while `REGIA_OCC_REPRO_LOAD` threads (default 4) keep the driver
/// compiling other pipelines. Every dispatch must give the same bits.
#[test]
#[ignore = "A03 driver experiment; run on a card"]
fn lookup_math_repeats_bit_exact_under_compile_load() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(device) = device() else {
        return;
    };
    let iters: u32 = env_or("REGIA_OCC_REPRO_ITERS", 400u32);
    let load: u32 = env_or("REGIA_OCC_REPRO_LOAD", 4u32);
    let (w, h) = (1280u32, 720u32);
    let count = w * h;
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        regia_core::glam::Vec3::new(0.0, 3.0, 6.0),
        regia_core::glam::Vec3::new(0.0, 1.0, -32.0),
        regia_core::glam::Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.0, 16.0 / 9.0, 0.1, 400.0);
    let inv = (proj * view).inverse();
    let mut params: Vec<f32> = inv.to_cols_array().to_vec();
    params.extend_from_slice(&[0.0, 3.0, 6.0, 0.0]);
    let samples: Vec<f32> = (0..count)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let depth = 0.97 + 0.02 * ((x * 7 + y * 13) % 101) as f32 / 101.0;
            [
                (x as f32 * 2.0 + 0.5) / 2560.0,
                (y as f32 * 2.0 + 0.5) / 1440.0,
                depth,
                0.0,
            ]
        })
        .collect();
    let d = &device.device;
    let make = |label: &str, size: u64, usage: wgpu::BufferUsages| {
        d.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage,
            mapped_at_creation: false,
        })
    };
    let params_buf = make(
        "a03 params",
        80,
        wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    );
    let samples_buf = make(
        "a03 samples",
        u64::from(count) * 16,
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    );
    let out_buf = make(
        "a03 out",
        u64::from(count) * 4,
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    );
    let readback = make(
        "a03 readback",
        u64::from(count) * 4,
        wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
    );
    device
        .queue
        .write_buffer(&params_buf, 0, bytemuck::cast_slice(&params));
    device
        .queue
        .write_buffer(&samples_buf, 0, bytemuck::cast_slice(&samples));
    let buffers = (&params_buf, &samples_buf, &out_buf, &readback);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let loaders: Vec<_> = (0..load)
        .map(|t| {
            let stop = std::sync::Arc::clone(&stop);
            let device = device.clone();
            std::thread::spawn(move || {
                let mut n = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let source = format!(
                        "{REPRO_WGSL}\nconst LOAD_{t}_{n}: f32 = {n}.0;\n@compute @workgroup_size(64)\nfn load_{t}_{n}(@builtin(global_invocation_id) g: vec3<u32>) {{\n    out[g.x] = bitcast<u32>(sin(f32(g.x) * LOAD_{t}_{n}) * cos(params.eye.x + LOAD_{t}_{n}));\n}}\n"
                    );
                    let _ = repro_pipeline(&device, &source);
                    n += 1;
                }
                n
            })
        })
        .collect();
    let pipeline = repro_pipeline(&device, REPRO_WGSL);
    let first = repro_dispatch(&device, &pipeline, buffers, count);
    let mut changes = Vec::new();
    for iter in 1..iters {
        let same = repro_dispatch(&device, &pipeline, buffers, count);
        if same != first {
            changes.push((iter, "same pipeline", same));
        }
        if iter.is_multiple_of(10) {
            let fresh = repro_pipeline(&device, REPRO_WGSL);
            let hash = repro_dispatch(&device, &fresh, buffers, count);
            if hash != first {
                changes.push((iter, "fresh pipeline", hash));
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    let compiled: Vec<u32> = loaders.into_iter().filter_map(|t| t.join().ok()).collect();
    println!("first {first:016x}; load pipelines compiled {compiled:?}; changes {changes:x?}");
    assert!(
        changes.is_empty(),
        "the same inputs gave other bits: {changes:x?}"
    );
}

/// The screen lookup's position math (`ddgi_screen.wgsl` `world_at` and the distance word) over one
/// row of texels with every intermediate stored, 16 words per texel: uv, clip xy, the four
/// components of `inv_view_proj * clip`, the position, the distance and its square. Which
/// operation's result moves when the card repeats a dispatch differently (A03).
const DIAG_WGSL: &str = r"
struct Params { m: mat4x4<f32>, eye: vec4<f32>, size: vec4<u32>, depth: vec4<f32> }
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> out: array<u32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.size.z) {
        return;
    }
    let full = vec2<i32>(params.size.xy);
    let c = clamp(vec2<i32>(i32(i * 2u), 0), vec2<i32>(0), full - vec2<i32>(1));
    let uv = (vec2<f32>(c) + vec2<f32>(0.5)) / vec2<f32>(full);
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, params.depth.x, 1.0);
    let w = params.m * clip;
    let q = w.xyz / w.w;
    let v = params.eye.xyz - q;
    let b = i * 16u;
    out[b] = bitcast<u32>(uv.x);
    out[b + 1u] = bitcast<u32>(uv.y);
    out[b + 2u] = bitcast<u32>(clip.x);
    out[b + 3u] = bitcast<u32>(clip.y);
    out[b + 4u] = bitcast<u32>(w.x);
    out[b + 5u] = bitcast<u32>(w.y);
    out[b + 6u] = bitcast<u32>(w.z);
    out[b + 7u] = bitcast<u32>(w.w);
    out[b + 8u] = bitcast<u32>(q.x);
    out[b + 9u] = bitcast<u32>(q.y);
    out[b + 10u] = bitcast<u32>(q.z);
    out[b + 11u] = bitcast<u32>(length(v));
    out[b + 12u] = bitcast<u32>(dot(v, v));
    out[b + 13u] = 0u;
    out[b + 14u] = 0u;
    out[b + 15u] = 0u;
}
";

/// The names of [`DIAG_WGSL`]'s 16 word slots.
const DIAG_SLOTS: [&str; 16] = [
    "uv.x", "uv.y", "clip.x", "clip.y", "w.x", "w.y", "w.z", "w.w", "p.x", "p.y", "p.z", "length", "dot",
    "-", "-", "-",
];

/// Child processes killed and reaped when dropped.
struct Children(Vec<std::process::Child>);

impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The classes of a sequence of values in order of first appearance (`A`, `B`, ...), as run lengths:
/// `A120 B3 A2`.
fn run_lengths<T: PartialEq>(values: &[T]) -> String {
    let mut classes: Vec<&T> = Vec::new();
    let mut out = String::new();
    let mut last: Option<(usize, usize)> = None;
    for value in values {
        let class = classes.iter().position(|c| *c == value).unwrap_or_else(|| {
            classes.push(value);
            classes.len() - 1
        });
        match &mut last {
            Some((c, n)) if *c == class => *n += 1,
            _ => {
                if let Some((c, n)) = last {
                    out.push_str(&format!("{}{n} ", (b'A' + c.min(25) as u8) as char));
                }
                last = Some((class, 1));
            }
        }
    }
    if let Some((c, n)) = last {
        out.push_str(&format!("{}{n}", (b'A' + c.min(25) as u8) as char));
    }
    out
}

/// The summary line of the card repro: classes are lettered in order of first appearance and equal
/// neighbors are counted.
#[test]
fn run_lengths_letter_the_classes_in_order_of_first_appearance() {
    assert_eq!(run_lengths::<u32>(&[]), "");
    assert_eq!(run_lengths(&[7u32]), "A1");
    assert_eq!(run_lengths(&[7u32, 7, 7]), "A3");
    assert_eq!(run_lengths(&[7u32, 7, 9, 9, 9, 7, 3, 3]), "A2 B3 A1 C2");
    // The repro's shape on the RTX 5080: three windows of the alternate, then the frame's own output.
    let mut classes = vec![0usize; 8114];
    for (len, class) in [(264, 1), (1632, 0), (246, 1), (235, 0), (253, 1), (19256, 0)] {
        classes.extend(std::iter::repeat_n(class, len));
    }
    assert_eq!(run_lengths(&classes), "A8114 B264 A1632 B246 A235 B253 A19256");
}

/// A03 card repro (`--ignored`, a card; it needs no other process on it): the probes' screen lookup
/// of one rendered frame, dispatched again and again alone in its own submit, with nothing changed
/// between the dispatches (the same pipeline, bind groups, parameters and depth texture), must write
/// the same words every time. `REGIA_OCC_ALONE_ITERS` dispatches (default 40 000, a minute or two:
/// the first change came up to 13 s into the loop on an RTX 5080 and 18 s on an RTX 5090) after
/// `REGIA_OCC_ALONE_WARM` frames (default 12); `REGIA_OCC_ALONE_AGGRESSORS=<n>` starts n child
/// processes of this test binary that render the stress frames on the same card meanwhile
/// (`REGIA_OCC_ALONE_AGGRESSOR_TEST` names their test, `culled_scene_repeats_run_to_run` by
/// default). After every lookup dispatch a small shader with the lookup's position math on fixed
/// inputs runs too ([`DIAG_WGSL`]); when it changes, the slots that moved are printed.
#[test]
#[ignore = "A03 card repro; run on a card"]
fn screen_lookup_repeats_alone() {
    let Some(device) = device() else {
        return;
    };
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    let iters: u32 = env_or("REGIA_OCC_ALONE_ITERS", 40_000u32).max(2);
    let warm: u32 = env_or("REGIA_OCC_ALONE_WARM", 12u32).max(2);
    let aggressors: u32 = env_or("REGIA_OCC_ALONE_AGGRESSORS", 0u32);
    let aggressor_test = regia_core::brand::env_var("REGIA_OCC_ALONE_AGGRESSOR_TEST")
        .unwrap_or_else(|_| "culled_scene_repeats_run_to_run".into());
    let instances: u32 = env_or("REGIA_OCC_GAIN_INSTANCES", 4096u32).max(1);
    let size = (2560u32, 1440u32);
    // The first 64 rows of the lookup: the alternate moves about 8 % of every row's distance words.
    let texels = 1280u32 * 64;
    let field = Field::new(Casters::Sway, true, instances, 2048, 64);
    let s = scene(&field, quality(false), size);
    let mut renderer = Cull::Off.renderer(&device);
    for frame in 0..warm {
        tick(&mut renderer, frame);
        renderer.render_offscreen(&device, &s).expect("warm-up frame");
    }
    let read = |renderer: &FrameRenderer| -> Vec<u32> {
        renderer
            .ddgi_screen_words_read_back(&device, texels)
            .expect("lookup readback")
            .expect("the lookup is on")
    };
    let in_frame = read(&renderer);
    // The diagnostic shader on the inputs of a logged frame (frame 20 of the stress): its matrix,
    // the eye, the sizes and the wall's depth on the top row.
    let d = &device.device;
    let diag = repro_pipeline(&device, DIAG_WGSL);
    let mut params: Vec<u32> = vec![
        0x3f78_a0dc,
        0,
        0x8000_0000,
        0,
        0,
        0x3f0b_a902,
        0xbceb_380a,
        0xb2ae_c614,
        0x8000_0000,
        0xc1ef_f0dd,
        0xc26f_f0dd,
        0xc11f_f5e8,
        0xb8f8_a0dc,
        0x41ef_9511,
        0x426c_019f,
        0x4120_0025,
    ];
    params.extend_from_slice(&[0, 0x4040_0000, 0x40c0_0000, 0]);
    params.extend_from_slice(&[2560, 1440, 1280, 720]);
    params.extend_from_slice(&[0x3f76_ab41, 0, 0, 0]);
    let make = |label: &str, bytes: u64, usage: wgpu::BufferUsages| {
        d.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes,
            usage,
            mapped_at_creation: false,
        })
    };
    let diag_params = make(
        "a03 diag params",
        params.len() as u64 * 4,
        wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    );
    let diag_out = make(
        "a03 diag out",
        1280 * 16 * 4,
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
    );
    let diag_read = make(
        "a03 diag readback",
        1280 * 16 * 4,
        wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
    );
    device
        .queue
        .write_buffer(&diag_params, 0, bytemuck::cast_slice(&params));
    let diag_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("a03 diag"),
        layout: &diag.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: diag_params.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: diag_out.as_entire_binding(),
            },
        ],
    });
    let run_diag = || -> Vec<u32> {
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("a03 diag"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("a03 diag"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&diag);
            pass.set_bind_group(0, &diag_bg, &[]);
            pass.dispatch_workgroups(1280u32.div_ceil(64), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&diag_out, 0, &diag_read, 0, 1280 * 16 * 4);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        diag_read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        rx.recv().expect("map callback").expect("map");
        let words: Vec<u32> =
            bytemuck::pod_collect_to_vec(&diag_read.slice(..).get_mapped_range().expect("range"));
        diag_read.unmap();
        words
    };
    // The aggressors: other processes rendering on this card.
    let mut children = Children(Vec::new());
    if aggressors > 0 {
        let exe = std::env::current_exe().expect("the test binary's path");
        for _ in 0..aggressors {
            let child = std::process::Command::new(&exe)
                .args([
                    "--ignored",
                    "--exact",
                    "--test-threads=1",
                    aggressor_test.as_str(),
                ])
                .env("REGIA_OCC_ALONE_AGGRESSORS", "0")
                .env("REGIA_OCC_REPEAT_RUNS", "100000")
                .env("REGIA_OCC_ALONE_ITERS", "100000000")
                .env("REGIA_OCC_REPRO_ITERS", "100000000")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("an aggressor process");
            children.0.push(child);
        }
    }
    println!(
        "--- sway_hidden, {warm} frames, then {iters} lookup dispatches alone; {aggressors} aggressors ({aggressor_test})"
    );
    // Distinct outputs in order of first appearance, and each dispatch's class.
    let mut lookup_kinds: Vec<Vec<u32>> = vec![in_frame];
    let mut diag_kinds: Vec<Vec<u32>> = Vec::new();
    let mut lookups: Vec<usize> = Vec::with_capacity(iters as usize);
    let mut diags: Vec<usize> = Vec::with_capacity(iters as usize);
    let class = |kinds: &mut Vec<Vec<u32>>, words: Vec<u32>| -> usize {
        kinds.iter().position(|k| *k == words).unwrap_or_else(|| {
            kinds.push(words);
            kinds.len() - 1
        })
    };
    // When each change of the lookup's class happened (dispatch, Unix ms, class): for a look at what
    // the card and the other processes did then. Printed only, never compared.
    let unix_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_millis())
    };
    let started_ms = unix_ms();
    let mut changes: Vec<(u32, u128, usize)> = Vec::new();
    for i in 0..iters {
        assert!(
            renderer.ddgi_rerun_screen_lookup(&device).expect("the rerun"),
            "the lookup ran in the warm-up frames"
        );
        let kind = class(&mut lookup_kinds, read(&renderer));
        if lookups.last().is_some_and(|last| *last != kind) && changes.len() < 64 {
            changes.push((i, unix_ms(), kind));
        }
        lookups.push(kind);
        diags.push(class(&mut diag_kinds, run_diag()));
    }
    println!(
        "loop from Unix ms {started_ms} to {}; lookup class changes (dispatch, Unix ms, class) {changes:?}",
        unix_ms()
    );
    drop(children);
    println!(
        "lookup: {} distinct outputs (A = the frame's own); runs {}",
        lookup_kinds.len(),
        run_lengths(&lookups)
    );
    println!(
        "diag:   {} distinct outputs; runs {}",
        diag_kinds.len(),
        run_lengths(&diags)
    );
    // How the lookup's class and the diagnostic's go together.
    let mut pairs: Vec<((usize, usize), u32)> = Vec::new();
    for pair in lookups.iter().copied().zip(diags.iter().copied()) {
        match pairs.iter_mut().find(|(p, _)| *p == pair) {
            Some((_, n)) => *n += 1,
            None => pairs.push((pair, 1)),
        }
    }
    println!("(lookup class, diag class) counts {pairs:?}");
    if let [a, b, ..] = lookup_kinds.as_slice() {
        let mut per_slot = [0usize; 8];
        let mut list = Vec::new();
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            if x != y {
                per_slot[i % 8] += 1;
                if list.len() < 12 {
                    let t = (i / 8) as u32;
                    list.push((t % 1280, t / 1280, i % 8, *x, *y));
                }
            }
        }
        println!(
            "lookup A vs B: words differ per slot {per_slot:?} of {} texels; (x, y, slot, A, B) {list:x?}",
            a.len() / 8
        );
    }
    if let [a, b, ..] = diag_kinds.as_slice() {
        let mut per_slot = [0usize; 16];
        let mut list = Vec::new();
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            if x != y {
                per_slot[i % 16] += 1;
                if list.len() < 16 {
                    list.push((i / 16, DIAG_SLOTS[i % 16], *x, *y));
                }
            }
        }
        let named: Vec<(&str, usize)> = DIAG_SLOTS
            .iter()
            .copied()
            .zip(per_slot)
            .filter(|(_, n)| *n > 0)
            .collect();
        println!("diag A vs B: texels that differ per slot {named:?} of 1280; (texel, slot, A, B) {list:x?}");
    }
    assert!(
        lookup_kinds.len() == 1 && diag_kinds.len() == 1,
        "the card wrote other words for the same dispatch: lookup runs {}, diag runs {}",
        run_lengths(&lookups),
        run_lengths(&diags)
    );
}

/// Texels of two depth readbacks that differ in their bits (`usize::MAX` when the sizes differ).
fn depth_texels_differ(a: &[f32], b: &[f32]) -> usize {
    if a.len() != b.len() {
        return usize::MAX;
    }
    a.iter()
        .zip(b)
        .filter(|(x, y)| x.to_bits() != y.to_bits())
        .count()
}

/// A03 on a card (`--ignored`): the heavy layout's culls checked where they act. The prepass cull
/// only takes draws out of the depth prepass and the shadow cull only out of the shadow passes, so
/// with the renderers stepped together a culled renderer's depth and shadow maps must equal the
/// unculled renderer's texel for texel on every compared frame: asserted (the scene depth as the
/// depth passes left it and again at the end of the frame, every frame; the five shadow maps every
/// fourth frame and on the last). The depth is captured right after the depth passes because the
/// forward pass writes depth again and would put back a caster the prepass cull wrongly took. A
/// second unculled renderer runs as the control and must pass the same checks.
///
/// The final pixels are compared too, but only printed, one pattern per renderer (`.` equal to the
/// unculled frame, `X` not): on NVIDIA's 615 drivers a shaded pass can write other bits for the
/// same dispatch ([`screen_lookup_repeats_alone`], ADR-5953), so the control's pattern shows
/// whether the card repeated the unculled frame at all. Knobs: `REGIA_OCC_GAIN_SCENES`,
/// `REGIA_OCC_GAIN_SIZE`, `REGIA_OCC_GAIN_FRAMES` (default 24, after 8 warm-up frames),
/// `REGIA_OCC_GAIN_INSTANCES`.
#[test]
#[ignore = "A03 on a card: the heavy layout's culls leave depth and shadow maps texel-equal"]
fn heavy_casters_cull_exactly_where_they_draw() {
    let Some(device) = device() else {
        return;
    };
    let info = device.adapter.get_info();
    println!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.driver);
    let size = regia_core::brand::env_var("REGIA_OCC_GAIN_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((2560u32, 1440u32));
    let frames: u32 = env_or("REGIA_OCC_GAIN_FRAMES", 24u32).max(1);
    let instances: u32 = env_or("REGIA_OCC_GAIN_INSTANCES", 4096u32).max(1);
    let wanted: Vec<String> = regia_core::brand::env_var("REGIA_OCC_GAIN_SCENES")
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    const WARM: u32 = 8;
    const KINDS: [usize; 5] = [0, 1, 2, 3, 4];
    const NAMES: [&str; 3] = ["unculled control", "prepass cull", "shadow cull"];
    for class in CLASSES {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == class.name) {
            continue;
        }
        let field = Field::new(class.casters, class.hidden, instances, 2048, 64);
        let s = scene(&field, quality(false), size);
        let mut plain = Cull::Off.renderer(&device);
        let mut others = [
            Cull::Off.renderer(&device),
            Cull::Prepass.renderer(&device),
            Cull::Shadow.renderer(&device),
        ];
        let mut patterns = [String::new(), String::new(), String::new()];
        let mut map_frames = 0u32;
        plain.set_prepass_depth_capture(true);
        for renderer in &mut others {
            renderer.set_prepass_depth_capture(true);
        }
        for frame in 0..WARM + frames {
            tick(&mut plain, frame);
            let reference = plain
                .render_offscreen(&device, &s)
                .expect("unculled frame")
                .pixels;
            let prepass_depth = plain
                .read_prepass_depth(&device)
                .expect("unculled prepass depth")
                .expect("the frame captured its prepass depth");
            let depth = plain
                .read_scene_depth(&device, size.0, size.1)
                .expect("unculled scene depth");
            let with_maps = frame % 4 == 3 || frame + 1 == WARM + frames;
            let maps: Vec<Option<Vec<f32>>> = if with_maps {
                KINDS
                    .iter()
                    .map(|kind| {
                        plain
                            .shadow_map_depth(&device, *kind)
                            .expect("unculled shadow map")
                    })
                    .collect()
            } else {
                Vec::new()
            };
            map_frames += u32::from(with_maps);
            for (slot, renderer) in others.iter_mut().enumerate() {
                tick(renderer, frame);
                let pixels = renderer.render_offscreen(&device, &s).expect("frame").pixels;
                patterns[slot].push(if pixels == reference { '.' } else { 'X' });
                let other_prepass = renderer
                    .read_prepass_depth(&device)
                    .expect("prepass depth")
                    .expect("the frame captured its prepass depth");
                assert_eq!(
                    depth_texels_differ(&prepass_depth, &other_prepass),
                    0,
                    "{} frame {frame}: {}: depth texels after the depth passes differ from the unculled renderer's",
                    class.name,
                    NAMES[slot]
                );
                let other_depth = renderer
                    .read_scene_depth(&device, size.0, size.1)
                    .expect("scene depth");
                assert_eq!(
                    depth_texels_differ(&depth, &other_depth),
                    0,
                    "{} frame {frame}: {}: scene depth texels differ from the unculled renderer's",
                    class.name,
                    NAMES[slot]
                );
                for (kind, expected) in KINDS.iter().zip(&maps) {
                    let map = renderer.shadow_map_depth(&device, *kind).expect("shadow map");
                    let differ = match (expected, &map) {
                        (Some(a), Some(b)) => depth_texels_differ(a, b),
                        (None, None) => 0,
                        _ => usize::MAX,
                    };
                    assert_eq!(
                        differ, 0,
                        "{} frame {frame}: {}: shadow map {kind} texels differ from the unculled renderer's",
                        class.name, NAMES[slot]
                    );
                }
            }
        }
        // The culls ran: a hidden field with a bound loses casters to both.
        let p = others[1].prepass_occlusion_stats();
        let pc = others[1].prepass_occlusion_counters(&device).ok().flatten();
        let sh = others[2].shadow_occlusion_stats();
        let sc = others[2].shadow_occlusion_counters(&device).ok().flatten();
        println!(
            "{}: prepass depth and final depth equal on {} frames, shadow maps on {map_frames}; prepass cull \
             {pc:?} (active {}), shadow cull {:?} (active {})",
            class.name,
            WARM + frames,
            p.active,
            sc.map(|c| c.total),
            sh.active
        );
        for (name, pattern) in NAMES.iter().zip(&patterns) {
            println!(
                "{}: pixels vs the unculled frame, {name:>16}: {pattern}",
                class.name
            );
        }
        // An undeclared vertex graph has no bound: neither cull may take it (ADR-1887).
        if class.hidden && class.casters != Casters::SwayUndeclared {
            assert!(
                pc.is_some_and(|c| c.culled > 0),
                "{}: the prepass cull culled nothing: {pc:?} {p:?}",
                class.name
            );
            assert!(
                sc.is_some_and(|c| c.total.culled > 0),
                "{}: the shadow cull culled nothing: {:?} {sh:?}",
                class.name,
                sc.map(|c| c.total)
            );
        }
    }
}
