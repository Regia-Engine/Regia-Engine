// SPDX-License-Identifier: MPL-2.0
//! J24 foliage acceptance (v0.5 burn-down C06), lavapipe: swaying grass on the dynamic-caster
//! shadow class under a recorded flight. `grass_shadow_sway.rs` proves the class at one held
//! camera; here the camera and the sway clock move together, then the wind drops, so a swayed
//! shadow frozen into the static shadow cache, a frame where the moving cascades lose the swaying
//! boxes, or grass work that grows with the flight would show:
//!
//! - Calm air after a sway returns the frame to the rest-pose frame of a rig whose class was
//!   always off (no swayed blade is left in a shadow page), with the camera held and after a walk;
//!   a frozen clock holds one swayed shadow (spec 88 keeps the class on once the clock has moved).
//! - Along a flight the grass sways in every frame: one swaying cascade, its boxes are the
//!   class's boxes, pages are refreshed, and the frame differs from the rest-pose rig's.
//! - The grass work stays bounded along a flight from the ground to 150 m up: the selection never
//!   reaches the patch cap, candidates stay under the per-patch cap, a held pose walks the same
//!   work every frame.

use regia_render::{
    DirectionalLightUniform, DynamicCasterShadows, FrameQuality, FrameRenderer, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SharedPassDraw,
};
use regia_terrain::grass::MAX_GRASS_PATCHES;
use regia_terrain::grass::select::MAX_CANDIDATES_PER_FINE_PATCH;
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassQuality, GrassReport, GrassRequest, GrassSet,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw,
};

const W: u32 = 192;
const H: u32 = 108;

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [
            LayerDesc(name: "ground", albedo: (0.20, 0.16, 0.11), roughness: 0.92, tiling: 2.0,
                slope_min: 0.0, slope_max: 1.6, altitude_min: -1000.0, altitude_max: 5000.0,
                blend_sharpness: 0.15, triplanar: false),
        ])"#,
    )
    .unwrap_or_else(|err| panic!("layers: {err}"))
}

/// 512 m of gently rolling ground.
fn map() -> TerrainMap {
    let n = 257u32;
    let world = 512.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let z = (row as f32 / (n - 1) as f32 - 0.5) * world;
            let x = (col as f32 / (n - 1) as f32 - 0.5) * world;
            heights.push(0.8 * (x * 0.021).sin() * (z * 0.017).cos());
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).unwrap_or_else(|err| panic!("map: {err}"))
}

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"))
}

/// Where the camera is: its ground position, its height over the ground and the heading it looks
/// along (unit, xz).
#[derive(Debug, Clone, Copy)]
struct Pose {
    at: [f32; 2],
    height: f32,
    heading: [f32; 2],
}

const HELD: Pose = Pose {
    at: [12.0, 40.0],
    height: 1.2,
    heading: [0.0, -1.0],
};

/// The recorded walk: 4 m a frame along -z with a slow turn, so the cascades move under the grass.
fn walk(step: usize) -> Pose {
    let s = step as f32;
    let turn = 0.06 * s;
    Pose {
        at: [12.0 + 1.5 * s, 40.0 - 4.0 * s],
        height: 1.2 + 0.1 * s,
        heading: [turn.sin(), -turn.cos()],
    }
}

struct Rig {
    device: RenderDevice,
    renderer: FrameRenderer,
    handle: TerrainGrassHandle,
    map: TerrainMap,
    post: PostSettings,
    size: [u32; 2],
}

impl Rig {
    fn new(class: DynamicCasterShadows) -> Self {
        Self::sized(class, GrassQuality::Ultra, [W, H])
    }

    /// The rig on grass tier `quality`, rendering `size` pixels.
    fn sized(class: DynamicCasterShadows, quality: GrassQuality, size: [u32; 2]) -> Self {
        let device = device();
        let map = map();
        let layers = layers();
        let mut draw = TerrainSharedDraw::new(&device, map.clone(), &layers, CdlodConfig::default(), 4)
            .unwrap_or_else(|err| panic!("terrain: {err}"));
        let set = GrassSet::summer();
        let mask = GrassMask::build(
            &set,
            &GrassMaskInputs {
                layers: Some(&layers),
                texel_m: 1.0,
                ..GrassMaskInputs::new(&map)
            },
        )
        .unwrap_or_else(|err| panic!("mask: {err}"));
        let layer = GrassLayer::new(&device.device, &device.queue, set, mask)
            .unwrap_or_else(|err| panic!("grass layer: {err}"));
        let handle = layer.handle();
        handle.set_request(GrassRequest {
            quality: Some(quality),
            // A strong sway, so the moving shadow is well above lavapipe's noise.
            sway_scale: 4.0,
            ..GrassRequest::default()
        });
        draw.set_grass(Some(layer));
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        renderer.set_dynamic_caster_shadows(class);
        let post = PostSettings {
            auto_exposure: false,
            exposure_ev: 14.0,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        };
        Self {
            device,
            renderer,
            handle,
            map,
            post,
            size,
        }
    }

    /// One frame from `pose` at sway clock `time` (seconds).
    fn frame(&mut self, pose: Pose, time: f32) -> Vec<u8> {
        self.handle.set_time(time);
        let [x, z] = pose.at;
        let ahead = [x + 30.0 * pose.heading[0], z + 30.0 * pose.heading[1]];
        let ground = self.map.sample_bilinear(x, z);
        let target = self.map.sample_bilinear(ahead[0], ahead[1]);
        let lab = LabScene {
            width: self.size[0],
            height: self.size[1],
            eye: [x, ground + pose.height, z],
            target: [ahead[0], target + 0.2, ahead[1]],
            fov_y: 0.9,
            near: 0.05,
            far: 1000.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 9_000.0,
            double_sided: false,
            post: Some(self.post.clone()),
            // A low sun from the side: long blade shadows.
            directional: DirectionalLightUniform::new([0.75, -0.21, -0.62], [1.0, 0.93, 0.84], 60_000.0),
            local_lights: Vec::new(),
            materials: Vec::new(),
            meshes: Vec::new(),
        };
        let mut quality =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        quality.taau = false;
        quality.render_scale_permille = 1000;
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&self.post);
        self.renderer
            .render_offscreen(&self.device, &scene)
            .unwrap_or_else(|err| panic!("frame: {err}"))
            .pixels
    }

    fn report(&self) -> GrassReport {
        self.handle.report()
    }
}

const ON: DynamicCasterShadows = DynamicCasterShadows::On {
    cascades: 1,
    local_lights: false,
};

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Share of pixels whose channels differ by more than two levels.
fn changed_fraction(a: &[u8], b: &[u8]) -> f32 {
    let changed = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count();
    changed as f32 / (W * H) as f32
}

/// Frames of calm air (or a frozen clock) after the sway, before the comparison.
const STILL_FRAMES: usize = 3;

/// Share of the pixels a swayed shadow must change against the rest-pose frame for the
/// comparisons after it to mean anything (the swaying grass changes 16-18 % on a card, lavapipe
/// and Mali).
const MIN_SWAY: f32 = 0.01;

/// Sways the class-on rig and the class-off rig through the same poses and clock, then drops the
/// scene wind to calm air (`[0, 0]`, which stops the blades, spec 88) while the clock keeps
/// running at the last pose. Returns the last frames of both, the swaying difference (on vs off
/// while swaying) and the class-on rig's last report.
fn sway_then_calm(path: impl Fn(usize) -> Pose, sway_frames: usize) -> (Vec<u8>, Vec<u8>, f32, GrassReport) {
    let mut on = Rig::new(ON);
    let mut off = Rig::new(DynamicCasterShadows::Off);
    let mut swaying = 0.0f32;
    for step in 0..sway_frames {
        let t = 1.0 + step as f32 * 0.25;
        let a = on.frame(path(step), t);
        let b = off.frame(path(step), t);
        swaying = swaying.max(changed_fraction(&a, &b));
    }
    let report = on.report();
    assert_eq!(
        report.shadow_sway_cascades, 1,
        "the grass never swayed: {report:?}"
    );
    assert!(report.dynamic_boxes > 0, "{report:?}");
    let last = path(sway_frames - 1);
    on.renderer.set_foliage_wind(Some([0.0, 0.0]));
    off.renderer.set_foliage_wind(Some([0.0, 0.0]));
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for k in 0..STILL_FRAMES {
        let t = 1.0 + (sway_frames + k) as f32 * 0.25;
        a = on.frame(last, t);
        b = off.frame(last, t);
    }
    (a, b, swaying, on.report())
}

/// Held camera: once the wind drops to calm air, the class-on frame is the class-off (rest-pose)
/// frame: the swaying boxes leave the class and no swayed blade stays in a shadow page.
#[test]
fn calm_air_after_a_sway_leaves_no_swayed_shadow_behind() {
    let _gpu = serial();
    let (on, off, swaying, report) = sway_then_calm(|_| HELD, 5);
    let left = changed_fraction(&on, &off);
    eprintln!(
        "held: swaying differs on {:.2} %, calm differs on {:.3} %",
        swaying * 100.0,
        left * 100.0
    );
    assert!(
        swaying > MIN_SWAY,
        "the swayed shadow barely changed the frame ({swaying})"
    );
    assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
    assert_eq!(report.dynamic_boxes, 0, "{report:?}");
    assert!(
        on == off,
        "a swayed shadow stayed in calm air ({left} of the pixels)"
    );
}

/// The same after a walk: the cascades moved under the swaying grass, then the air goes calm;
/// the pages drawn during the walk must not keep a swayed blade.
#[test]
fn calm_air_after_a_walk_leaves_no_swayed_shadow_behind() {
    let _gpu = serial();
    let (on, off, swaying, report) = sway_then_calm(walk, 8);
    let left = changed_fraction(&on, &off);
    eprintln!(
        "walk: swaying differs on {:.2} %, calm differs on {:.3} %",
        swaying * 100.0,
        left * 100.0
    );
    assert!(
        swaying > MIN_SWAY,
        "the swayed shadow barely changed the frame ({swaying})"
    );
    assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
    assert_eq!(report.dynamic_boxes, 0, "{report:?}");
    assert!(
        on == off,
        "a swayed shadow stayed in calm air after the walk ({left} of the pixels)"
    );
}

/// A frozen clock after a sway (spec 88: the shadow sways once the clock has moved since the layer
/// was bound) keeps the frozen blades' shadow: the class keeps its boxes, and no pixel of two
/// frozen frames moves by more than two levels (no page flips between poses).
#[test]
fn a_frozen_clock_after_a_sway_holds_one_shadow() {
    let _gpu = serial();
    let mut on = Rig::new(ON);
    let mut off = Rig::new(DynamicCasterShadows::Off);
    for step in 0..4 {
        let t = 1.0 + step as f32 * 0.25;
        on.frame(HELD, t);
        off.frame(HELD, t);
    }
    let mut frames = Vec::new();
    let mut rest = Vec::new();
    for _ in 0..STILL_FRAMES {
        frames.push(on.frame(HELD, 2.0));
        rest = off.frame(HELD, 2.0);
    }
    let report = on.report();
    assert_eq!(report.shadow_sway_cascades, 1, "{report:?}");
    assert!(report.dynamic_boxes > 0, "{report:?}");
    let n = frames.len();
    // The held shadow is the frozen blades' (swayed), not the rest pose of the class-off rig.
    let swayed = changed_fraction(&frames[n - 1], &rest);
    // Consecutive frames are not byte-identical (per-frame screen-space noise moves pixels by a
    // level or two on a card and on lavapipe), but a shadow page flipping between two blade poses
    // moves whole blade shadows by far more (`swayed` is the size of such a change).
    let moved = changed_fraction(&frames[n - 1], &frames[n - 2]);
    eprintln!(
        "frozen: {swayed} of the pixels differ from the rest pose, consecutive frames differ by \
         more than 2 levels on {moved}"
    );
    assert!(swayed > MIN_SWAY, "the frozen shadow is the rest pose ({swayed})");
    assert!(moved == 0.0, "two frozen frames differ on {moved} of the pixels");
}

/// The walk step that holds the camera where the step before was.
const HELD_STEP: usize = 5;

/// Every frame of a walk sways: one swaying cascade with boxes, the class takes the boxes the
/// grass reported the frame before (spec 88: casters are planned before the shared draw prepares;
/// a moving camera redraws the grass's cascade anyway), their pages are redrawn (as dynamic pages
/// or with the whole cascade; at a held step inside the walk, as dynamic pages without a whole
/// redraw), and the frame is not the rest-pose frame. Two walks render the same bytes.
#[test]
fn a_walk_keeps_the_grass_shadow_swaying_every_frame() {
    let _gpu = serial();
    let run = || {
        let mut on = Rig::new(ON);
        let mut off = Rig::new(DynamicCasterShadows::Off);
        let mut last = Vec::new();
        let mut previous_boxes = 0u32;
        for step in 0..9 {
            let t = 1.0 + step as f32 * 0.2;
            // Step 5 holds the camera where step 4 was.
            let pose = walk(if step >= HELD_STEP { step - 1 } else { step });
            let a = on.frame(pose, t);
            let b = off.frame(pose, t);
            let report = on.report();
            let class = on.renderer.dynamic_caster_report();
            assert_eq!(
                class.shared_boxes, previous_boxes,
                "step {step}: the class did not take the last frame's boxes: {class:?}"
            );
            if previous_boxes > 0 {
                // A moving camera changes the cascade's matrix, so the whole cascade is redrawn
                // (no page counts as dynamic then); a held one refreshes the boxes' pages.
                let cache = on.renderer.shadow_cache_report();
                let whole = cache.cascade_full_redraw.first() == Some(&true);
                assert!(
                    class.dynamic_pages > 0 || whole,
                    "step {step}: the swaying grass's pages were not redrawn: {class:?}, {cache:?}"
                );
                if step == HELD_STEP {
                    assert!(
                        !whole && class.dynamic_pages > 0,
                        "held step: the boxes' pages were not refreshed on their own: {class:?}, {cache:?}"
                    );
                }
            }
            if step == 0 {
                // The clock has not moved since the layer was bound (spec 88): rest pose.
                assert_eq!(report.shadow_sway_cascades, 0, "{report:?}");
            } else {
                assert_eq!(report.shadow_sway_cascades, 1, "step {step}: {report:?}");
                assert!(report.dynamic_boxes > 0, "step {step}: {report:?}");
                let swayed = changed_fraction(&a, &b);
                assert!(
                    swayed > MIN_SWAY,
                    "step {step}: the walk froze the swayed shadow ({swayed})"
                );
            }
            previous_boxes = report.dynamic_boxes;
            last = a;
        }
        last
    };
    let a = run();
    let b = run();
    assert!(a == b, "two identical walks rendered different frames");
}

/// The grass work along a flight from the ground to 150 m up: the patch selection never reaches its
/// cap (the cap would silently drop the farthest ground), candidates stay under the per-patch cap,
/// one instance budget for the whole flight, and the same work every frame at a held pose.
#[test]
fn grass_work_stays_bounded_along_a_flight() {
    let _gpu = serial();
    let mut rig = Rig::new(ON);
    let heights = [1.2f32, 4.0, 15.0, 40.0, 80.0, 150.0];
    let mut ground: Option<GrassReport> = None;
    for (i, height) in heights.into_iter().enumerate() {
        let pose = Pose {
            at: [12.0 - 20.0 * i as f32, 40.0 + 10.0 * i as f32],
            height,
            heading: [0.6, -0.8],
        };
        let mut reports = Vec::new();
        for frame in 0..3 {
            rig.frame(pose, 2.0 + 0.1 * (3 * i + frame) as f32);
            reports.push(rig.report());
        }
        let r = &reports[2];
        eprintln!(
            "height {height} m: {} patches ({} camera, {} shadow), {} candidates, {} max instances",
            r.patches, r.camera_patches, r.shadow_patches, r.candidates, r.max_instances
        );
        assert!(r.drawing, "{r:?}");
        assert!(
            (r.patches as usize) < MAX_GRASS_PATCHES,
            "height {height}: the selection hit the patch cap: {r:?}"
        );
        assert_eq!(
            r.patches_per_level.iter().sum::<u32>(),
            r.patches,
            "height {height}: {r:?}"
        );
        assert!(
            r.candidates <= u64::from(r.patches) * u64::from(MAX_CANDIDATES_PER_FINE_PATCH),
            "height {height}: {r:?}"
        );
        assert!(r.max_instances > 0, "{r:?}");
        for other in &reports[1..] {
            assert_eq!(
                (other.patches, other.candidates, other.max_instances),
                (
                    reports[0].patches,
                    reports[0].candidates,
                    reports[0].max_instances
                ),
                "height {height}: the work changed at a held pose"
            );
        }
        match &ground {
            None => {
                assert!(r.patches > 0 && r.candidates > 0, "no grass on the ground: {r:?}");
                ground = Some(r.clone());
            }
            Some(g) => {
                assert_eq!(r.max_instances, g.max_instances, "the instance budget moved");
            }
        }
    }
}

/// GPU cost of the swaying grass per tier on a card, class on and off, along the walk at
/// 1920 x 1080 (GPU pass timestamps; the median of 24 frames after 8 warm ones). Prints one line
/// per row for the acceptance report; asserts only that the card times the passes. Run with a
/// pinned card:
/// `cargo test -p regia_terrain --test grass_flight -- --ignored --nocapture grass_tier_cost_bench`.
#[test]
#[ignore = "needs a GPU with TIMESTAMP_QUERY; prints numbers"]
fn grass_tier_cost_bench() {
    let _gpu = serial();
    for quality in [
        GrassQuality::Low,
        GrassQuality::Medium,
        GrassQuality::High,
        GrassQuality::Ultra,
    ] {
        for (name, class) in [("off", DynamicCasterShadows::Off), ("on", ON)] {
            let mut rig = Rig::sized(class, quality, [1920, 1080]);
            if !rig.device.capabilities.timestamp_query {
                println!("grass_tier_cost_bench: no TIMESTAMP_QUERY on this device, nothing measured");
                return;
            }
            rig.renderer.set_gpu_timing(&rig.device, true);
            let mut rows: Vec<[f32; 4]> = Vec::new();
            for step in 0..32usize {
                rig.frame(walk(step % 8), 1.0 + step as f32 / 60.0);
                if step >= 8 {
                    let t = rig.renderer.gpu_timings();
                    rows.push([t.frame_ms, t.shadows_ms, t.shared_prepass_ms, t.shared_forward_ms]);
                }
            }
            let median = |i: usize| {
                let mut v: Vec<f32> = rows.iter().map(|r| r[i]).collect();
                v.sort_by(f32::total_cmp);
                v[v.len() / 2]
            };
            let report = rig.report();
            let class_report = rig.renderer.dynamic_caster_report();
            println!(
                "grass_tier_cost_bench {quality:?} class {name}: frame {:.3} ms, shadows {:.3} ms, \
                 shared prepass {:.3} ms, shared forward {:.3} ms; {} patches, {} candidates, \
                 {} boxes, {} dynamic pages",
                median(0),
                median(1),
                median(2),
                median(3),
                report.patches,
                report.candidates,
                report.dynamic_boxes,
                class_report.dynamic_pages
            );
            assert!(median(0) > 0.0, "the card timed nothing");
        }
    }
}
