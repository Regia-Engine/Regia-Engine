// SPDX-License-Identifier: MPL-2.0
//! The vendor seam's conformance checklist on real frames (ADR-2679).
//!
//! A scripted binding stands in the frame's vendor slot and records every input it receives
//! (`regia_render::upscaler::vendor::scripted`). It runs on any GPU, lavapipe included, so what the
//! frame hands a vendor is checked with no vendor runtime: jitter sign and units, motion-vector
//! scale, exposure, reset on a camera cut, depth convention, render and display size, reactive mask.
//!
//! This is the checklist a new binding runs. `REGIA_VENDOR_CONFORMANCE=dlss` (a `dlss` build with
//! the SDK, on an RTX card) or `=xess` (a Windows build with the XeSS binding) records the real
//! binding of that name instead of the scripted one in
//! `the_frame_hands_every_vendor_slot_the_same_inputs`; a vendor that cannot start fails it:
//!
//! ```text
//! REGIA_VENDOR_CONFORMANCE=dlss DLSS_SDK=/abs/DLSS REGIA_DLSS_PATH=/abs/DLSS/lib/Linux_x86_64/rel \
//!   REGIA_GPU_UUID=GPU-... cargo test -p regia_render --features dlss --test vendor_conformance -- --nocapture
//! ```

use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_render::oit::reactive_from_revealage;
use regia_render::upscaler::plugin::VendorRuntime;
use regia_render::upscaler::vendor::VendorRuntimeApi;
use regia_render::upscaler::vendor::dlss::{DlssNgx, GpuBinding};
use regia_render::upscaler::vendor::scripted::{
    BindingFactoryGuard, CHECKLIST, ExpectedFrame, RecordedEval, Recorder, ScriptConfig, ScriptLog,
    ScriptedVendor, check_eval, inputs_of, install_binding_factory,
};
use regia_render::upscaler::{UpscaleQuality, VendorUpscaler};
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, exposure_from_ev100,
    pre_exposure_scale,
};

const W: u32 = 192;
const H: u32 = 108;
const FOV_Y: f32 = 0.9;
const NEAR: f32 = 0.1;
const FAR: f32 = 500.0;
const EV: f32 = 14.0;
/// The slab's and the backdrop's distance from the eye.
const SLAB_DISTANCE: f32 = 6.0;
const BACKDROP_DISTANCE: f32 = 12.0;

/// Device creation, teardown and the process-wide binding factory, one test at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> (MutexGuard<'static, ()>, RenderDevice) {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    (guard, RenderDevice::headless().expect("headless device"))
}

fn plate(x: [f32; 2], y: [f32; 2], z: f32, material: usize) -> LabMesh {
    let n = [0.0, 0.0, 1.0];
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x[0], y[0], z], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], y[0], z], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x[1], y[1], z], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x[0], y[1], z], n, [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A bright slab in the middle of a dark backdrop, with nothing moving. Material 2 is a translucent
/// plate for the reactive mask's test.
fn scene() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -6.0],
        fov_y: FOV_Y,
        near: NEAR,
        far: FAR,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: EV,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.5, -0.8], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.03, 0.03, 0.03, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.9, 0.3, 0.1, 0.5],
                alpha_mode: ALPHA_BLEND,
                unlit: true,
                double_sided: true,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            plate([-40.0, 40.0], [-30.0, 30.0], -BACKDROP_DISTANCE, 0),
            plate([-1.0, 1.0], [-0.8, 0.8], -SLAB_DISTANCE, 1),
        ],
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: EV,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

fn quality(scale: f32) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = true;
    q.render_scale_permille = (scale * 1000.0).round() as u16;
    q
}

fn render(device: &RenderDevice, renderer: &mut FrameRenderer, lab: &LabScene, q: FrameQuality) -> Vec<u8> {
    let post = post();
    let mut frame = lab.as_frame_scene_with_quality(q);
    frame.post = Some(&post);
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

/// The exposure the frame must hand over: the post stack's over the target's pre-exposure.
fn expected_exposure() -> f32 {
    let post = exposure_from_ev100(EV);
    post / pre_exposure_scale(post)
}

/// A factory that gives every vendor a scripted binding, logging into `log`.
fn scripted(
    log: &ScriptLog,
    config: impl Fn(VendorUpscaler) -> ScriptConfig + Send + Sync + 'static,
) -> BindingFactoryGuard {
    let log = log.clone();
    install_binding_factory(move |vendor, _device| {
        let binding: Box<dyn GpuBinding> =
            Box::new(Recorder::new(ScriptedVendor::new(config(vendor)), log.clone()));
        Some(binding)
    })
}

fn assert_conforms(recorded: &RecordedEval, expected: &ExpectedFrame, frame: usize) {
    let violations = check_eval(recorded, expected);
    assert!(
        violations.is_empty(),
        "frame {frame}: {violations:#?}\nrecorded {recorded:#?}"
    );
}

/// The vendor under test: the scripted one, or the real binding `REGIA_VENDOR_CONFORMANCE` names.
fn real_vendor() -> Option<VendorUpscaler> {
    match std::env::var("REGIA_VENDOR_CONFORMANCE").as_deref() {
        Ok("dlss") => Some(VendorUpscaler::Dlss),
        Ok("xess") => Some(VendorUpscaler::Xess),
        _ => None,
    }
}

/// Runs the checklist's script through one vendor slot: a run of frames, a camera cut, a run of
/// frames, a new display size. Returns what the binding received.
fn run_the_script(device: &RenderDevice, vendor: VendorUpscaler, log: &ScriptLog) -> Vec<RecordedEval> {
    let q = quality(0.667);
    let mut lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(vendor));
    let frames = 4;
    for _ in 0..frames {
        render(device, &mut renderer, &lab, q);
    }
    renderer.camera_cut();
    for _ in 0..frames {
        render(device, &mut renderer, &lab, q);
    }
    lab.width = W * 2;
    lab.height = H * 2;
    for _ in 0..frames {
        render(device, &mut renderer, &lab, q);
    }
    let report = renderer.vendor_upscaler_report();
    assert!(report.active, "{vendor:?} did not run: {report:?}");
    assert_eq!(report.frames, 3 * frames as u64, "{report:?}");
    assert_eq!(report.fallbacks, 0, "{report:?}");
    assert_eq!(report.runtime, Some(VendorRuntime::Ready));
    let evals = log.evals();
    assert_eq!(evals.len(), 3 * frames, "{vendor:?}");

    // What the frame must have handed over, worked out apart from the seam.
    let render_at = |display: (u32, u32)| q.scaled_extent(display.0, display.1);
    let sizes = [(W, H), (W, H), (W * 2, H * 2)];
    let mut previous_jitter = None;
    for (index, recorded) in evals.iter().enumerate() {
        let (run, in_run) = (index / frames, index % frames);
        let display = sizes[run];
        let render = render_at(display);
        // Run 0 starts the renderer, run 1 at the cut and run 2 at a new display size.
        let reset = in_run == 0;
        // Frame jitter is what the projection moved by. The jitter sequence is ADR-1150's: the first
        // frame after a cut is its frame 1, Halton index 1, at any size.
        let sequence = regia_render::upscaler::vendor::JitterSequence::for_sizes(render, display);
        let jitter_ndc = recorded.params.jitter;
        if run == 1 {
            assert_eq!(
                jitter_ndc,
                sequence.ndc(in_run as u64 + 1, render),
                "after a cut the sequence starts over, frame {in_run}"
            );
        }
        if in_run > 0 && run != 2 {
            assert_ne!(
                Some(jitter_ndc),
                previous_jitter,
                "the jitter moves, frame {index}"
            );
        }
        previous_jitter = Some(jitter_ndc);
        let expected = ExpectedFrame {
            render,
            display,
            jitter_ndc,
            exposure: expected_exposure(),
            reset,
            reactive: false,
        };
        assert_conforms(recorded, &expected, index);
    }
    // The frame's jitter in render pixels is the sequence's, sign and all, spelled out once: the
    // first frame after a cut is (1/2, 1/3) in Halton, so x 0 and y −(1/3 − 1/2) = +1/6.
    let after_cut = &evals[frames];
    assert!(
        after_cut.eval.jitter[0].abs() < 1.0e-4,
        "{:?}",
        after_cut.eval.jitter
    );
    assert!(
        (after_cut.eval.jitter[1] - 1.0 / 6.0).abs() < 1.0e-4,
        "{:?}",
        after_cut.eval.jitter
    );
    // One feature per display size, the old one released, every frame in its mode.
    let features = log.features();
    assert_eq!(features.len(), 2, "{features:?}");
    assert_eq!(features[0].desc.display_size, (W, H));
    assert_eq!(features[1].desc.display_size, (W * 2, H * 2));
    assert_eq!(features[0].desc.quality, UpscaleQuality::Quality);
    assert_eq!(log.released(), vec![features[0].handle]);
    evals
}

/// Item 1 of ADR-2679: every vendor slot receives what the checklist says, and the same thing.
#[test]
fn the_frame_hands_every_vendor_slot_the_same_inputs() {
    let (_gpu, device) = serial();
    assert_eq!(CHECKLIST.len(), 10);
    if let Some(vendor) = real_vendor() {
        // A real binding: the Recorder wraps it, and a binding that cannot start fails the run.
        let log = ScriptLog::new(false);
        let recording = log.clone();
        let _factory = install_binding_factory(move |asked, device| {
            (asked == vendor).then(|| {
                let binding = real_binding(asked, device);
                eprintln!("real binding: {:?}", binding.runtime());
                let recorded: Box<dyn GpuBinding> = Box::new(Recorder::new(binding, recording.clone()));
                recorded
            })
        });
        let evals = run_the_script(&device, vendor, &log);
        eprintln!("{vendor:?} passes the checklist over {} evaluates", evals.len());
        return;
    }
    let mut runs = Vec::new();
    for vendor in VendorUpscaler::ALL {
        let log = ScriptLog::new(false);
        let _factory = scripted(&log, ScriptConfig::ready);
        runs.push(run_the_script(&device, vendor, &log));
    }
    let [dlss, xess] = &runs[..] else {
        panic!("two vendors"); // invariant: test-only; VendorUpscaler::ALL has two
    };
    assert_eq!(dlss.len(), xess.len());
    for (a, b) in dlss.iter().zip(xess) {
        assert_eq!(inputs_of(a), inputs_of(b));
    }
}

/// The real binding of `vendor`, for `REGIA_VENDOR_CONFORMANCE`.
fn real_binding(vendor: VendorUpscaler, device: &wgpu::Device) -> Box<dyn GpuBinding> {
    match vendor {
        VendorUpscaler::Dlss => Box::new(DlssNgx::for_device(device)),
        VendorUpscaler::Xess => Box::new(regia_render::upscaler::vendor::xess::Xess::for_device(device)),
        _ => panic!("this vendor has no conformance binding"),
    }
}

/// Item 1: the binding's image is the known filter of its inputs, and the frame shows that image.
#[test]
fn the_vendor_image_is_the_known_filter_and_the_frame_shows_it() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(true);
    let _factory = scripted(&log, ScriptConfig::ready);
    let q = quality(0.667);
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Xess));
    let shown = render(&device, &mut renderer, &lab, q);
    let frame = log.read_capture(&device.device, 0).expect("capture");
    assert_eq!(frame.display, (W, H));
    assert_eq!(frame.output.len(), (W * H) as usize);
    assert_eq!(frame.texels.len(), (frame.render.0 * frame.render.1) as usize);
    // Nearest: every display pixel is the render texel under it, to the half-float's precision.
    for y in 0..H {
        for x in 0..W {
            let source = frame.at(x * frame.render.0 / W, y * frame.render.1 / H).color;
            let written = frame.output[(y * W + x) as usize];
            for channel in 0..3 {
                let (a, b) = (source[channel], written[channel]);
                assert!(
                    (a - b).abs() <= a.abs().max(1.0e-3) * 2.0e-3,
                    "({x}, {y}) channel {channel}: {a} became {b}"
                );
            }
        }
    }
    // The frame carries it on: the slab is bright in what the frame shows.
    let green = |pixels: &[u8], x: u32, y: u32| pixels[((y * W + x) * 4 + 1) as usize];
    assert!(
        green(&shown, W / 2, H / 2) > 40,
        "{}",
        green(&shown, W / 2, H / 2)
    );
    drop(_factory);

    // Gain 0: a black vendor image makes a black frame, so nothing else reached the output.
    let log = ScriptLog::new(false);
    let _factory = scripted(&log, |vendor| ScriptConfig {
        gain: 0.0,
        ..ScriptConfig::ready(vendor)
    });
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    let black = render(&device, &mut renderer, &lab, q);
    assert!(renderer.vendor_upscaler_report().active);
    let brightest = black.as_chunks::<4>().0.iter().map(|p| p[1]).max();
    assert!(brightest.is_some_and(|b| b <= 1), "{brightest:?}");
}

/// Item 1: the motion texture holds UV toward the previous frame, and the scale makes it render
/// pixels. The camera pans right, so the scene moves left and the surface was to the right.
#[test]
fn motion_reaches_the_binding_toward_the_previous_frame_and_scales_to_render_pixels() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(true);
    let _factory = scripted(&log, ScriptConfig::ready);
    let q = quality(0.667);
    let mut lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    render(&device, &mut renderer, &lab, q);
    render(&device, &mut renderer, &lab, q);
    let step = 0.25;
    lab.eye[0] += step;
    lab.target[0] += step;
    render(&device, &mut renderer, &lab, q);
    let evals = log.evals();
    assert!(!evals[2].eval.reset, "a small pan is not a cut");
    let frame = log.read_capture(&device.device, 2).expect("capture");
    let (rw, rh) = frame.render;
    // A backdrop pixel, away from the slab.
    let backdrop = frame.at(rw / 8, rh / 8);
    let aspect = W as f32 / H as f32;
    let focal = 1.0 / (FOV_Y * 0.5).tan();
    let expected_uv = step * focal / aspect / BACKDROP_DISTANCE * 0.5;
    assert!(
        backdrop.motion[0] > 0.0,
        "toward the previous position: {backdrop:?}"
    );
    assert!(
        (backdrop.motion[0] - expected_uv).abs() < expected_uv * 0.05,
        "UV {} against {expected_uv}",
        backdrop.motion[0]
    );
    assert!(backdrop.motion[1].abs() < expected_uv * 0.05, "{backdrop:?}");
    // The scale takes it to render pixels.
    let in_pixels = backdrop.motion[0] * evals[2].eval.motion_scale[0];
    let expected_pixels = expected_uv * rw as f32;
    assert!(
        (in_pixels - expected_pixels).abs() < expected_pixels * 0.05,
        "{in_pixels} render pixels against {expected_pixels}"
    );
    // The nearer slab moved further, by the depth ratio: motion is per pixel, not global.
    let slab = frame.at(rw / 2, rh / 2);
    let ratio = slab.motion[0] / backdrop.motion[0];
    assert!((ratio - BACKDROP_DISTANCE / SLAB_DISTANCE).abs() < 0.1, "{ratio}");
    // No jitter in the motion: a still camera has none.
    let still = log.read_capture(&device.device, 1).expect("capture");
    let at_rest = still.at(rw / 8, rh / 8).motion;
    assert!(
        at_rest[0].abs() < 1.0e-5 && at_rest[1].abs() < 1.0e-5,
        "{at_rest:?}"
    );
}

/// Item 1: the depth a binding reads is what its creation flags say. The frame's depth is standard:
/// near is smaller. The flag said reverse-Z until this test read the texture (ADR-2679).
#[test]
fn depth_reaches_the_binding_as_its_flags_say() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(true);
    let _factory = scripted(&log, ScriptConfig::ready);
    let q = quality(0.667);
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Xess));
    render(&device, &mut renderer, &lab, q);
    assert!(!log.evals()[0].flags.depth_inverted);
    let frame = log.read_capture(&device.device, 0).expect("capture");
    let (rw, rh) = frame.render;
    let slab = frame.at(rw / 2, rh / 2).depth;
    let backdrop = frame.at(rw / 8, rh / 8).depth;
    assert!(
        slab < backdrop,
        "near is smaller: slab {slab}, backdrop {backdrop}"
    );
    // The projection's own depth at each distance.
    for (depth, distance) in [(slab, SLAB_DISTANCE), (backdrop, BACKDROP_DISTANCE)] {
        let expected = FAR / (FAR - NEAR) * (1.0 - NEAR / distance);
        assert!(
            (depth - expected).abs() < 1.0e-4,
            "{depth} against {expected} at {distance}"
        );
    }
}

/// Item 3: with order-independent transparency on, the mask reaches every vendor slot at the render
/// size with the covered share in it; with it off, no mask.
#[test]
fn the_oit_mask_reaches_the_vendor_when_oit_is_on() {
    let (_gpu, device) = serial();
    let mut lab = scene();
    lab.meshes
        .push(plate([-0.5, 0.5], [-0.4, 0.4], -SLAB_DISTANCE + 2.0, 2));
    for vendor in VendorUpscaler::ALL {
        let log = ScriptLog::new(true);
        let _factory = scripted(&log, ScriptConfig::ready);
        let mut q = quality(0.667);
        q.order_independent_transparency = true;
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(vendor));
        render(&device, &mut renderer, &lab, q);
        let evals = log.evals();
        let render_size = q.scaled_extent(W, H);
        let expected = ExpectedFrame {
            render: render_size,
            display: (W, H),
            jitter_ndc: evals[0].params.jitter,
            exposure: expected_exposure(),
            reset: true,
            reactive: true,
        };
        assert_conforms(&evals[0], &expected, 0);
        // What the frame wrote is the mask the frame's own host reads: same size, same contents.
        let mask = renderer.oit_reactive_mask().expect("the frame wrote a mask");
        assert_eq!(
            (mask.texture().width(), mask.texture().height()),
            render_size,
            "{vendor:?}"
        );
        let frame = log.read_capture(&device.device, 0).expect("capture");
        let (rw, rh) = frame.render;
        let center = frame.at(rw / 2, rh / 2).reactive;
        let share = reactive_from_revealage(0.5);
        assert!(
            (center - share).abs() < 2.0 / 255.0,
            "{vendor:?}: {center} against {share}"
        );
        assert_eq!(frame.at(2, 2).reactive, 0.0, "{vendor:?}: outside the plate");

        // Off: no mask, and the frame says the same to every slot.
        q.order_independent_transparency = false;
        render(&device, &mut renderer, &lab, q);
        let off = log.evals();
        assert_eq!(off[1].reactive, None, "{vendor:?}");
        assert_conforms(
            &off[1],
            &ExpectedFrame {
                jitter_ndc: off[1].params.jitter,
                reset: false,
                reactive: false,
                ..expected
            },
            1,
        );
    }
}

/// Item 4: a runtime in any state that cannot run leaves the frame byte-identical to the built-in's,
/// is never asked to evaluate, and the report names the reason.
#[test]
fn a_runtime_that_cannot_run_falls_back_to_the_built_in_with_the_reason_visible() {
    let (_gpu, device) = serial();
    let q = quality(0.667);
    let lab = scene();
    let frames = 4;
    let mut plain = FrameRenderer::new();
    let mut built_in = Vec::new();
    for _ in 0..frames {
        built_in = render(&device, &mut plain, &lab, q);
    }
    for (runtime, reason) in [
        (VendorRuntime::NotBundled, "sdk_not_bundled"),
        (VendorRuntime::NotFound, "runtime_not_found"),
        (VendorRuntime::Found, "runtime_not_initialized"),
        (VendorRuntime::ReportsUnsupported, "runtime_reports_unsupported"),
        (VendorRuntime::NeedsNewerDriver, "needs_newer_driver"),
        (VendorRuntime::InitFailed, "runtime_init_failed"),
    ] {
        let log = ScriptLog::new(false);
        let _factory = scripted(&log, move |vendor| ScriptConfig {
            runtime,
            ..ScriptConfig::ready(vendor)
        });
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
        let mut pixels = Vec::new();
        for _ in 0..frames {
            pixels = render(&device, &mut renderer, &lab, q);
        }
        let report = renderer.vendor_upscaler_report();
        assert!(!report.active, "{runtime:?}");
        assert_eq!(report.frames, 0, "{runtime:?}");
        assert_eq!(report.fallbacks, frames as u64, "{runtime:?}");
        assert_eq!(report.fallback, Some("runtime_unavailable"), "{runtime:?}");
        assert_eq!(report.runtime, Some(runtime));
        assert_eq!(report.runtime.and_then(VendorRuntime::blocker), Some(reason));
        assert!(log.evals().is_empty() && log.features().is_empty(), "{runtime:?}");
        assert!(pixels == built_in, "{runtime:?} changed the frame");
    }
}

/// Item 4: a runtime that fails while running falls back on each failed frame and gives up after
/// three in a row, with the frame the built-in's all along.
#[test]
fn a_binding_that_keeps_failing_falls_back_and_turns_itself_off() {
    let (_gpu, device) = serial();
    let q = quality(0.667);
    let lab = scene();
    let frames = 5;
    let mut plain = FrameRenderer::new();
    let mut built_in = Vec::new();
    for _ in 0..frames {
        built_in.push(render(&device, &mut plain, &lab, q));
    }
    for (fail_creates, fail_evaluates, what) in [(9, 0, "create"), (0, 9, "evaluate")] {
        let log = ScriptLog::new(false);
        let _factory = scripted(&log, move |vendor| ScriptConfig {
            fail_creates,
            fail_evaluates,
            ..ScriptConfig::ready(vendor)
        });
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(VendorUpscaler::Xess));
        let mut codes = Vec::new();
        for image in &built_in {
            let pixels = render(&device, &mut renderer, &lab, q);
            assert!(pixels == *image, "{what}: a failed vendor changed the frame");
            codes.push(renderer.vendor_upscaler_report().fallback);
        }
        assert_eq!(
            codes,
            vec![
                Some("backend_failed"),
                Some("backend_failed"),
                Some("backend_failed"),
                Some("backend_disabled"),
                Some("backend_disabled"),
            ],
            "{what}"
        );
        let report = renderer.vendor_upscaler_report();
        assert_eq!((report.frames, report.fallbacks), (0, frames as u64), "{what}");
        // Evaluate failures are logged; nothing leaks a feature past the give-up.
        let evals = log.evals();
        assert!(evals.iter().all(|e| !e.ok), "{what}");
        assert_eq!(log.features().len(), log.released().len(), "{what}");
    }
}

/// A dynamic-resolution step across the middle of two modes keeps the live feature and its history:
/// it used to recreate the feature whenever the nearest mode changed.
#[test]
fn a_scale_change_inside_the_features_range_keeps_the_feature() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(false);
    let _factory = scripted(&log, |vendor| ScriptConfig {
        range_permille: Some((667, 400, 1000)),
        ..ScriptConfig::ready(vendor)
    });
    let lab = scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    // Nearest mode by size: Quality, Balanced, Performance, Ultra Performance's side of 2.0.
    for permille in [667u16, 600, 520, 450, 667] {
        let mut q = quality(1.0);
        q.render_scale_permille = permille;
        render(&device, &mut renderer, &lab, q);
    }
    let report = renderer.vendor_upscaler_report();
    assert_eq!((report.frames, report.fallbacks), (5, 0), "{report:?}");
    assert_eq!(log.features().len(), 1, "{:?}", log.features());
    assert!(log.released().is_empty());
    let resets: Vec<bool> = log.evals().iter().map(|e| e.eval.reset).collect();
    assert_eq!(resets, vec![true, false, false, false, false]);
    let widths: Vec<u32> = log.evals().iter().map(|e| e.eval.render_size.0).collect();
    assert_eq!(widths, vec![128, 115, 100, 86, 128]);
}
