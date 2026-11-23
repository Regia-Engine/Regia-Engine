// SPDX-License-Identifier: MPL-2.0
//! A selected vendor's render range sets the frame's render size (ADR-2679): the frame hookup in
//! `frame/steps/modes.rs`. A scripted binding asks for sizes unlike any built-in's, on any GPU.

use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_render::upscaler::plugin::VendorRuntime;
use regia_render::upscaler::vendor::dlss::GpuBinding;
use regia_render::upscaler::vendor::scripted::{
    BindingFactoryGuard, ExpectedFrame, Recorder, ScriptConfig, ScriptLog, ScriptedVendor, check_eval,
    install_binding_factory,
};
use regia_render::upscaler::vendor::{OptimalSettings, PreferredRange};
use regia_render::upscaler::{UpscaleQuality, VendorUpscaler};
use regia_render::{
    DirectionalLightUniform, DynResConfig, DynResController, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
    exposure_from_ev100, pre_exposure_scale,
};

const W: u32 = 200;
const H: u32 = 100;
const EV: f32 = 14.0;
/// `(preferred, min, max)` permille of the display: no built-in mode sits at any of them.
const ODD: (u32, u32, u32) = (770, 610, 910);

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> (MutexGuard<'static, ()>, RenderDevice) {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    (guard, RenderDevice::headless().expect("headless device"))
}

fn scene() -> LabScene {
    let n = [0.0, 0.0, 1.0];
    let plate = |half: f32, z: f32, material: usize| LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-half, -half, z], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, -half, z], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([half, half, z], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-half, half, z], n, [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 0.0],
        target: [0.0, 0.0, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
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
        ],
        meshes: vec![plate(60.0, -12.0, 0), plate(1.0, -6.0, 1)],
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

fn quality(mode: Option<UpscaleQuality>, permille: Option<u16>) -> FrameQuality {
    let mut profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    profile.upscale_quality = mode;
    let mut q = FrameQuality::from_profile(&profile);
    q.taau = true;
    if let Some(permille) = permille {
        q.render_scale_permille = permille;
    }
    q
}

fn render(device: &RenderDevice, renderer: &mut FrameRenderer, q: FrameQuality) -> Vec<u8> {
    let post = post();
    let lab = scene();
    let mut frame = lab.as_frame_scene_with_quality(q);
    frame.post = Some(&post);
    renderer.render_offscreen(device, &frame).expect("frame").pixels
}

fn scripted(log: &ScriptLog, config: ScriptConfig) -> BindingFactoryGuard {
    let log = log.clone();
    install_binding_factory(move |_vendor, _device| {
        let binding: Box<dyn GpuBinding> = Box::new(Recorder::new(ScriptedVendor::new(config), log.clone()));
        Some(binding)
    })
}

fn odd(vendor: VendorUpscaler) -> ScriptConfig {
    ScriptConfig {
        range_permille: Some(ODD),
        ..ScriptConfig::ready(vendor)
    }
}

fn sizes(log: &ScriptLog) -> Vec<(u32, u32)> {
    log.evals().iter().map(|e| e.eval.render_size).collect()
}

/// The frame renders at the runtime's preferred size from its first frame, under the mode, on both
/// vendor slots, and the binding is handed everything at that size.
#[test]
fn the_frame_renders_at_the_runtimes_preferred_size() {
    let (_gpu, device) = serial();
    let mode = Some(UpscaleQuality::Quality);
    for vendor in VendorUpscaler::ALL {
        let log = ScriptLog::new(false);
        let _factory = scripted(&log, odd(vendor));
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(vendor));
        for _ in 0..3 {
            render(&device, &mut renderer, quality(mode, None));
        }
        let report = renderer.vendor_upscaler_report();
        assert_eq!((report.frames, report.fallbacks), (3, 0), "{report:?}");
        assert_eq!(report.render_preferred, Some((154, 77)));
        assert_eq!(report.render_range, Some(((122, 61), (182, 91))));
        assert_eq!(sizes(&log), vec![(154, 77); 3], "{vendor:?}");
        let exposure = exposure_from_ev100(EV) / pre_exposure_scale(exposure_from_ev100(EV));
        for (index, recorded) in log.evals().iter().enumerate() {
            let violations = check_eval(
                recorded,
                &ExpectedFrame {
                    render: (154, 77),
                    display: (W, H),
                    jitter_ndc: recorded.params.jitter,
                    exposure,
                    reset: index == 0,
                    reactive: false,
                },
            );
            assert!(violations.is_empty(), "{vendor:?} frame {index}: {violations:#?}");
            assert_eq!(recorded.quality, UpscaleQuality::Quality);
        }
        // The jitter was made for that size: half a pixel of it, not of the mode's size.
        let jitter = log.evals()[2].eval.jitter;
        assert!(jitter.iter().all(|axis| axis.abs() <= 0.5), "{jitter:?}");
        assert_eq!(log.features().len(), 1);
    }
}

/// Dynamic resolution inside the runtime's range: the sizes stay in it, reach both ends and keep
/// one feature; the scale it moves is relative to the mode.
#[test]
fn dynamic_resolution_moves_the_frame_inside_the_runtimes_range() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(false);
    let _factory = scripted(&log, odd(VendorUpscaler::Xess));
    let mode = Some(UpscaleQuality::Quality);
    let answer = ScriptedVendorAnswer::of(odd(VendorUpscaler::Xess));
    let config = DynResConfig {
        cooldown_frames: 0,
        window_frames: 1,
        ..DynResConfig::default()
    }
    .within_preferred(&answer.0, (W, H), mode);
    let mut controller = DynResController::new(config, UpscaleQuality::Quality.permille());
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Xess));
    let mut permille = controller.permille();
    for (gpu_ms, frame_ms) in std::iter::repeat_n((40.0, 42.0), 5).chain(std::iter::repeat_n((4.0, 40.0), 14))
    {
        render(&device, &mut renderer, quality(mode, Some(permille as u16)));
        permille = controller.update(gpu_ms, frame_ms);
    }
    let seen = sizes(&log);
    assert_eq!(seen[0], (154, 77), "starts at the preferred size");
    for size in &seen {
        assert_eq!(answer.0.clamp(*size), *size, "{seen:?}");
    }
    assert!(seen.iter().any(|s| s.0 <= 123), "reached the floor: {seen:?}");
    assert!(seen.iter().any(|s| s.0 >= 181), "reached the top: {seen:?}");
    let report = renderer.vendor_upscaler_report();
    assert_eq!(report.fallbacks, 0, "{report:?}");
    assert_eq!(log.features().len(), 1);
    assert!(
        log.evals().iter().skip(1).all(|e| !e.eval.reset),
        "no resets on a scale step"
    );
}

/// The range as the seam reports it, for the test's dynamic-resolution setup.
struct ScriptedVendorAnswer(PreferredRange);

impl ScriptedVendorAnswer {
    fn of(config: ScriptConfig) -> Self {
        use regia_render::upscaler::vendor::VendorRuntimeApi as _;
        let settings: OptimalSettings = ScriptedVendor::new(config)
            .optimal_settings((W, H), UpscaleQuality::Quality)
            .expect("scripted answer");
        Self(PreferredRange::from_settings(&settings))
    }
}

/// A static range holds one size at every scale; a custom scale with no mode keeps its own size
/// inside the range.
#[test]
fn a_static_range_holds_and_a_custom_scale_keeps_its_size_in_range() {
    let (_gpu, device) = serial();
    let log = ScriptLog::new(false);
    let _factory = scripted(
        &log,
        ScriptConfig {
            range_permille: Some((770, 770, 770)),
            ..ScriptConfig::ready(VendorUpscaler::Dlss)
        },
    );
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    for permille in [667, 500, 900, 1000] {
        let q = quality(Some(UpscaleQuality::Quality), Some(permille));
        render(&device, &mut renderer, q);
    }
    assert_eq!(sizes(&log), vec![(154, 77); 4]);
    drop(_factory);

    let log = ScriptLog::new(false);
    let _factory = scripted(&log, odd(VendorUpscaler::Dlss));
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    for permille in [800, 500, 1000] {
        render(&device, &mut renderer, quality(None, Some(permille)));
    }
    assert_eq!(sizes(&log), vec![(160, 80), (122, 61), (182, 91)]);
    let report = renderer.vendor_upscaler_report();
    assert_eq!(report.fallbacks, 0, "{report:?}");
}

/// Everything that does not run the vendor renders exactly what it renders with none selected: no
/// selection, a runtime that cannot run, an MSAA frame.
#[test]
fn frames_the_vendor_does_not_run_do_not_move() {
    let (_gpu, device) = serial();
    let frames = 4;
    let q = quality(Some(UpscaleQuality::Quality), None);
    let mut plain = FrameRenderer::new();
    let mut built_in = Vec::new();
    for _ in 0..frames {
        built_in = render(&device, &mut plain, q);
    }
    for runtime in [
        VendorRuntime::NotBundled,
        VendorRuntime::ReportsUnsupported,
        VendorRuntime::NeedsNewerDriver,
        VendorRuntime::InitFailed,
    ] {
        let log = ScriptLog::new(false);
        let _factory = scripted(
            &log,
            ScriptConfig {
                runtime,
                ..odd(VendorUpscaler::Dlss)
            },
        );
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
        let mut pixels = Vec::new();
        for _ in 0..frames {
            pixels = render(&device, &mut renderer, q);
        }
        assert!(pixels == built_in, "{runtime:?} changed the frame");
        let report = renderer.vendor_upscaler_report();
        assert_eq!(report.render_preferred, None, "{runtime:?}");
        assert_eq!(report.runtime, Some(runtime));
        assert!(log.evals().is_empty(), "{runtime:?}");
    }
    // An MSAA frame and a frame with no TAA keep their sizes and never start the binding's work.
    let log = ScriptLog::new(false);
    let _factory = scripted(&log, odd(VendorUpscaler::Dlss));
    for change in [
        (|q: &mut FrameQuality| q.msaa_samples = 4) as fn(&mut FrameQuality),
        |q: &mut FrameQuality| q.taau = false,
    ] {
        let mut changed = q;
        change(&mut changed);
        let mut plain = FrameRenderer::new();
        let mut vendor = FrameRenderer::new();
        vendor.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
        for _ in 0..2 {
            assert!(render(&device, &mut plain, changed) == render(&device, &mut vendor, changed));
        }
        assert!(log.evals().is_empty());
    }
}
