// SPDX-License-Identifier: MPL-2.0
//! The frame's vendor upscaler slot (ADR-2002).
//!
//! - Where the vendor cannot run (no `dlss` feature, no NGX SDK linked, no runtime library, a
//!   GPU without NGX's extensions, lavapipe), selecting it changes nothing: every frame is
//!   byte-identical to the built-in upscaler's and the report says why it fell back.
//! - With `REGIA_DLSS_SMOKE=1` on an NVIDIA GPU, a `dlss` build with the SDK linked and the DLSS
//!   runtime in `REGIA_DLSS_PATH`, DLSS upscales the frame and the result stays close to the
//!   native-resolution frame:
//!
//! ```text
//! DLSS_SDK=/abs/DLSS REGIA_DLSS_SMOKE=1 REGIA_DLSS_PATH=/abs/DLSS/lib/Linux_x86_64/rel \
//!   REGIA_GPU_UUID=GPU-... cargo test -p regia_render --features dlss --test vendor_upscaler_frame -- --nocapture
//! ```

use regia_render::upscaler::VendorUpscaler;
use regia_render::upscaler::plugin::VendorRuntime;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 320;
const H: u32 = 180;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// Slanted bright slabs over a dark backdrop: edges at several angles, nothing moving.
fn scene() -> LabScene {
    let slab = |cx: f32, cy: f32, angle: f32, hw: f32, hh: f32| {
        let (s, c) = angle.sin_cos();
        let rot = |x: f32, y: f32| [cx + x * c - y * s, cy + x * s + y * c, -6.0];
        quad(
            [rot(-hw, -hh), rot(hw, -hh), rot(hw, hh), rot(-hw, hh)],
            [0.0, 0.0, 1.0],
            1,
        )
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, 0.0],
        target: [0.0, 1.5, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.5, -0.8], [1.0, 1.0, 1.0], 100_000.0),
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
        meshes: vec![
            quad(
                [
                    [-40.0, -20.0, -12.0],
                    [40.0, -20.0, -12.0],
                    [40.0, 25.0, -12.0],
                    [-40.0, 25.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
            slab(-1.6, 1.5, 0.14, 1.2, 0.8),
            slab(1.4, 2.2, -0.6, 0.9, 0.25),
            slab(1.2, 0.6, 0.9, 0.7, 0.08),
        ],
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
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

fn render(device: &RenderDevice, renderer: &mut FrameRenderer, q: FrameQuality, frames: usize) -> Vec<u8> {
    let post = post();
    let s = scene();
    let mut frame = s.as_frame_scene_with_quality(q);
    frame.post = Some(&post);
    let mut last = Vec::new();
    for _ in 0..frames {
        last = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    last
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    }
}

/// Mean absolute difference of the green channel, in 8-bit levels.
fn mean_abs(a: &[u8], b: &[u8]) -> f32 {
    let (a, _) = a.as_chunks::<4>();
    let (b, _) = b.as_chunks::<4>();
    let sum: f32 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f32::from(x[1]) - f32::from(y[1])).abs())
        .sum();
    sum / a.len().max(1) as f32
}

/// Device creation and teardown one at a time (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn selecting_a_vendor_that_cannot_run_leaves_the_frame_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Explicitly select the unavailable binding even in an SDK-enabled Windows test build.
    // The pixel-identical fallback assertion remains unconditional on every GPU.
    let _factory = regia_render::upscaler::vendor::scripted::install_binding_factory(|vendor, _| {
        (vendor == VendorUpscaler::Xess).then(|| {
            Box::new(regia_render::upscaler::vendor::XessSdk)
                as Box<dyn regia_render::upscaler::vendor::dlss::GpuBinding>
        })
    });
    let device = device();
    let q = quality(0.67);
    let mut plain = FrameRenderer::new();
    let mut vendor = FrameRenderer::new();
    vendor.set_vendor_upscaler(Some(VendorUpscaler::Xess));
    let a = render(&device, &mut plain, q, 6);
    let b = render(&device, &mut vendor, q, 6);
    let report = vendor.vendor_upscaler_report();
    assert_eq!(report.selected, Some(VendorUpscaler::Xess));
    assert!(!report.active);
    assert_eq!(report.frames, 0);
    assert_eq!(
        report.fallbacks, 6,
        "every TAA frame tried the vendor and fell back"
    );
    assert_eq!(report.fallback, Some("runtime_unavailable"));
    assert!(a == b, "a vendor that cannot run changed the frame");
    // DLSS where it cannot run (no NVIDIA device with NGX's extensions here): the same.
    let mut dlss = FrameRenderer::new();
    dlss.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    let c = render(&device, &mut dlss, q, 6);
    let report = dlss.vendor_upscaler_report();
    if report.active {
        eprintln!("DLSS runs on this device; the fallback half is covered by the XeSS selection");
        return;
    }
    assert_eq!(report.fallbacks, 6);
    assert_eq!(report.fallback, Some("runtime_unavailable"));
    assert_ne!(report.runtime, Some(VendorRuntime::Ready));
    assert!(a == c, "DLSS that cannot run changed the frame");
    // Turning the selection off clears the report and keeps the built-in path.
    dlss.set_vendor_upscaler(None);
    let d = render(&device, &mut dlss, q, 1);
    assert_eq!(dlss.vendor_upscaler_report().fallbacks, 0);
    assert_eq!(d.len(), a.len());
}

/// What the frame learnt about DLSS is what the process reports (ADR-4937). The agent's
/// `render.get_upscalers` row reads the process-wide status, so after a frame fell back its runtime
/// state must be the frame's, and on an NVIDIA Vulkan adapter the row's `unavailable_code` must be
/// that state's reason. A GTX 1080 with the SDK linked and the runtime present is refused by NGX at
/// the device hook (`feature_not_supported`): the frame fell back as `reports_unsupported` while the
/// process still said `found`, so the row read `runtime_not_initialized` instead of
/// `runtime_reports_unsupported` (`book/agents/upscalers.md`). Holds on every adapter: lavapipe and
/// a build without the SDK are `not_bundled` on both sides; where DLSS runs there is nothing to check.
#[test]
fn the_process_reports_the_dlss_state_the_frame_fell_back_on() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    render(&device, &mut renderer, quality(0.67), 2);
    let report = renderer.vendor_upscaler_report();
    if report.active {
        eprintln!("DLSS runs on this device; nothing fell back");
        return;
    }
    let status = regia_render::upscaler::vendor::dlss::dlss_status();
    eprintln!("report {report:?}; status {status:?}");
    let runtime = report
        .runtime
        .expect("a fallen-back DLSS frame names its runtime state");
    assert_eq!(
        status.runtime, runtime,
        "the process status disagrees with the frame (code {:?})",
        status.code
    );
    let info = device.adapter.get_info();
    if info.vendor != 0x10DE || info.backend != wgpu::Backend::Vulkan {
        return;
    }
    let features = regia_render::upscaler::UpscalerFeatures {
        dlss_vulkan_extensions: !status.device_extensions.is_empty(),
        xess_vulkan_extensions: false,
    };
    let row = regia_render::upscaler::available(&info, info.backend, &features)
        .into_iter()
        .find(|s| s.kind == regia_render::upscaler::UpscalerKind::Vendor(VendorUpscaler::Dlss))
        .expect("a DLSS row");
    assert_eq!(
        row.unavailable_code(),
        runtime.blocker(),
        "the DLSS row's code on {}: {:?}",
        info.name,
        row.blockers
    );
}

#[test]
fn a_frame_without_taa_never_asks_the_vendor() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut q = quality(1.0);
    q.taau = false;
    let mut plain = FrameRenderer::new();
    let mut vendor = FrameRenderer::new();
    vendor.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    let a = render(&device, &mut plain, q, 2);
    let b = render(&device, &mut vendor, q, 2);
    assert!(a == b);
    let report = vendor.vendor_upscaler_report();
    assert_eq!((report.frames, report.fallbacks), (0, 0));
}

#[test]
fn dlss_upscales_the_frame_close_to_native_on_an_nvidia_gpu() {
    if std::env::var("REGIA_DLSS_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set REGIA_DLSS_SMOKE=1 (see the module docs)");
        return;
    }
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let status = regia_render::upscaler::vendor::dlss::dlss_status();
    eprintln!("DLSS status before the frame: {status:?}");
    let frames = 24;
    let native = render(&device, &mut FrameRenderer::new(), quality(1.0), frames);
    let taau = render(&device, &mut FrameRenderer::new(), quality(0.67), frames);
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
    let dlss = render(&device, &mut renderer, quality(0.67), frames);
    let report = renderer.vendor_upscaler_report();
    eprintln!(
        "report {report:?}; status after {:?}",
        regia_render::upscaler::vendor::dlss::dlss_status()
    );
    assert!(report.active, "DLSS did not run: {:?}", report.fallback);
    assert_eq!(report.frames, frames as u64);
    assert_eq!(report.runtime, Some(VendorRuntime::Ready));
    let (e_dlss, e_taau) = (mean_abs(&dlss, &native), mean_abs(&taau, &native));
    eprintln!("mean |Δ green| vs native: DLSS {e_dlss:.2} levels, TAAU {e_taau:.2} levels");
    assert!(dlss != taau, "the DLSS frame equals the TAAU frame");
    assert!(e_dlss < 6.0, "DLSS is {e_dlss:.2} levels from the native frame");
}

/// Optional evidence capture, never a golden. Files are published atomically after PNG encoding.
fn proof_png(dir: &std::path::Path, name: &str, frame: &[u8], size: (u32, u32)) {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, size.0, size.1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("PNG header")
            .write_image_data(frame)
            .expect("PNG pixels");
    }
    regia_test_utils::write_atomic(&dir.join(name), &bytes).expect("publish evidence PNG");
}

/// Residual on pixels that were bright eight frames ago and are dark now. The native
/// sequence defines the same disocclusion mask for both resolves; zero samples is a failure.
fn trail_error(old: &[u8], now: &[u8], resolved: &[u8]) -> (f32, usize) {
    let mut sum = 0.0;
    let mut count = 0;
    for ((old, now), resolved) in old
        .as_chunks::<4>()
        .0
        .iter()
        .zip(now.as_chunks::<4>().0)
        .zip(resolved.as_chunks::<4>().0)
    {
        // A 32-level contrast excludes native edge quantization and the static backdrop.
        if i16::from(old[1]) - i16::from(now[1]) > 32 {
            sum +=
                (f32::from(resolved[1]) - f32::from(now[1])).abs() / (f32::from(old[1]) - f32::from(now[1]));
            count += 1;
        }
    }
    (sum / count.max(1) as f32, count)
}

#[test]
fn proof_trail_mask_measures_only_revealed_pixels() {
    let old = [0, 200, 0, 255, 0, 20, 0, 255];
    let now = [0, 20, 0, 255, 0, 200, 0, 255];
    let resolved = [0, 65, 0, 255, 0, 0, 0, 255];
    assert_eq!(trail_error(&old, &now, &resolved), (0.25, 1));
    assert_eq!(trail_error(&old, &now, &now), (0.0, 1));
    assert_eq!(trail_error(&now, &now, &resolved), (0.0, 0));
}

/// Real SDK acceptance: every mode, still FLIP, synchronous frame latency and a moving slab.
/// Explicitly ignored so a normal SDK-free run cannot count a skipped hardware probe as proof.
/// Run with REGIA_DLSS_SMOKE=1 and --ignored --exact; REGIA_DLSS_PROOF_DIR optionally saves PNGs
/// and JSON outside the source tree. Timing includes CPU work and readback; it is not a GPU benchmark.
#[test]
#[ignore = "requires the real NGX SDK and an exclusive NVIDIA Vulkan card session"]
fn dlss_all_modes_native_flip_motion_and_timing() {
    use regia_render::compare::{DEFAULT_PPD, compare_srgb8};
    use regia_render::upscaler::UpscaleQuality;
    use regia_render::upscaler::vendor::dlss::dlss_status;
    use std::time::Instant;

    assert_eq!(std::env::var("REGIA_DLSS_SMOKE").as_deref(), Ok("1"));
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    assert!(dlss_status().binding_built, "the real SDK must be linked");
    let size = (960, 540);
    let warmup = 80; // At least the 72-phase Ultra Performance cycle.
    let motion_frames = 48;
    let output = std::env::var_os("REGIA_DLSS_PROOF_DIR").map(std::path::PathBuf::from);
    if let Some(dir) = &output {
        assert!(dir.is_absolute(), "evidence directory must be absolute");
        std::fs::create_dir_all(dir).expect("evidence directory");
    }
    let mut s = scene();
    s.width = size.0;
    s.height = size.1;
    let post = post();
    let render_one = |renderer: &mut FrameRenderer, q, s: &LabScene| {
        let mut frame = s.as_frame_scene_with_quality(q);
        frame.post = Some(&post);
        renderer
            .render_offscreen(&device, &frame)
            .expect("proof frame")
            .pixels
    };
    let mut native_quality = quality(1.0);
    native_quality.taau = false;
    let mut native_renderer = FrameRenderer::new();
    native_renderer.set_gpu_timing(&device, true);
    let mut native = Vec::new();
    let mut native_latency = 0.0;
    let mut native_gpu_ms = 0.0;
    for frame in 0..warmup {
        let start = Instant::now();
        native = render_one(&mut native_renderer, native_quality, &s);
        if frame >= warmup - 16 {
            native_latency += start.elapsed().as_secs_f64() * 1000.0 / 16.0;
            native_gpu_ms += f64::from(native_renderer.gpu_timings().frame_ms) / 16.0;
        }
    }
    if let Some(dir) = &output {
        proof_png(dir, "native-still.png", &native, size);
    }
    // Pan the slabs, not the backdrop; per-mesh transforms generate real object motion vectors.
    let move_scene = |frame: usize| {
        let mut moving = s.clone();
        for mesh in moving.meshes.iter_mut().skip(1) {
            mesh.model[3][0] = frame as f32 * 0.03;
        }
        moving
    };
    let mut native_renderer = FrameRenderer::new();
    let native_motion: Vec<_> = (0..motion_frames)
        .map(|frame| {
            let pixels = render_one(&mut native_renderer, native_quality, &move_scene(frame));
            if let Some(dir) = &output {
                proof_png(dir, &format!("native-motion-{frame:03}.png"), &pixels, size);
            }
            pixels
        })
        .collect();
    let mut rows = Vec::new();
    let mut regressions = Vec::new();
    for mode in UpscaleQuality::ALL {
        let mut q = quality(mode.permille() as f32 / 1000.0);
        q.upscale_quality = Some(mode);
        let mut dlss = FrameRenderer::new();
        dlss.set_vendor_upscaler(Some(VendorUpscaler::Dlss));
        let mut tsr = FrameRenderer::new();
        tsr.set_temporal_upscaler(regia_render::TemporalUpscaler::Tsr);
        dlss.set_gpu_timing(&device, true);
        tsr.set_gpu_timing(&device, true);
        let mut still = [Vec::new(), Vec::new()];
        let mut latency = [0.0f64; 2];
        let mut gpu_ms = [0.0f64; 2];
        for (index, renderer) in [&mut dlss, &mut tsr].into_iter().enumerate() {
            for frame in 0..warmup {
                let start = Instant::now();
                still[index] = render_one(renderer, q, &s);
                if frame >= warmup - 16 {
                    latency[index] += start.elapsed().as_secs_f64() * 1000.0 / 16.0;
                    gpu_ms[index] += f64::from(renderer.gpu_timings().frame_ms) / 16.0;
                }
            }
        }
        let report = dlss.vendor_upscaler_report();
        assert!(report.active, "{mode:?}: {report:?}; {:?}", dlss_status());
        assert_eq!(report.frames, warmup as u64);
        assert_eq!(report.fallbacks, 0);
        assert_eq!(report.runtime, Some(VendorRuntime::Ready));
        let dlss_flip = compare_srgb8(&native, &still[0], size.0, size.1, DEFAULT_PPD).0;
        let tsr_flip = compare_srgb8(&native, &still[1], size.0, size.1, DEFAULT_PPD).0;
        assert!(dlss_flip.flip_mean.is_finite() && tsr_flip.flip_mean.is_finite());
        if let Some(dir) = &output {
            proof_png(dir, &format!("{}-dlss-still.png", mode.name()), &still[0], size);
            proof_png(dir, &format!("{}-tsr-still.png", mode.name()), &still[1], size);
        }
        let mut worst = [0.0f32; 2];
        for frame in 0..motion_frames {
            let moving = move_scene(frame);
            for (index, renderer) in [&mut dlss, &mut tsr].into_iter().enumerate() {
                let pixels = render_one(renderer, q, &moving);
                if let Some(dir) = &output {
                    let backend = ["dlss", "tsr"][index];
                    proof_png(
                        dir,
                        &format!("{}-{backend}-motion-{frame:03}.png", mode.name()),
                        &pixels,
                        size,
                    );
                }
                if frame >= 8 {
                    let (error, count) =
                        trail_error(&native_motion[frame - 8], &native_motion[frame], &pixels);
                    assert!(count > 0, "motion must reveal pixels at frame {frame}");
                    worst[index] = worst[index].max(error);
                }
            }
            let report = dlss.vendor_upscaler_report();
            assert!(report.active && report.fallbacks == 0, "{mode:?}: {report:?}");
        }
        // One 8-bit level over the mask's minimum 32-level contrast is the quantization margin.
        let no_regression = worst[0] <= worst[1] + 1.0 / 32.0;
        if !no_regression {
            regressions.push(mode.name());
        }
        let row = serde_json::json!({
            "mode": mode.name(), "dlaa": mode == UpscaleQuality::Native,
            "extent": format!("{:?}", dlss.last_extent()),
            "dlss_flip_mean": dlss_flip.flip_mean, "dlss_flip_p95": dlss_flip.flip_p95,
            "tsr_flip_mean": tsr_flip.flip_mean,
            "mean_sync_frame_ms_dlss": latency[0], "mean_sync_frame_ms_tsr": latency[1],
            "mean_gpu_frame_ms_dlss": (gpu_ms[0] > 0.0).then_some(gpu_ms[0]),
            "mean_gpu_frame_ms_tsr": (gpu_ms[1] > 0.0).then_some(gpu_ms[1]),
            "worst_trail_residual_dlss": worst[0], "worst_trail_residual_tsr": worst[1],
            "no_ghosting_regression": no_regression,
            "evaluated_frames": dlss.vendor_upscaler_report().frames,
            "fallbacks": dlss.vendor_upscaler_report().fallbacks,
        });
        eprintln!("DLSS_PROOF {row}");
        rows.push(row);
    }
    if let Some(dir) = &output {
        let evidence = serde_json::json!({
            "sdk": dlss_status().sdk_version, "display": [size.0, size.1],
            "adapter": format!("{:?}", device.adapter.get_info()),
            "source_revision": std::env::var("REGIA_GIT_HASH").unwrap_or_else(|_| "unrecorded".into()),
            "warmup_frames": warmup, "motion_frames": motion_frames,
            "mean_sync_frame_ms_native": native_latency,
            "mean_gpu_frame_ms_native": (native_gpu_ms > 0.0).then_some(native_gpu_ms),
            "timing": "synchronous CPU+GPU+readback latency; final 16 held frames; no timing assertions",
            "rows": rows,
        });
        regia_test_utils::write_atomic(
            &dir.join("results.json"),
            &serde_json::to_vec_pretty(&evidence).expect("JSON"),
        )
        .expect("publish proof results");
    }
    assert!(
        regressions.is_empty(),
        "ghosting regression against TSR: {regressions:?}"
    );
}

/// ADR-7820: in a DLSS or XeSS build with the runtimes present, a `REGIA_GPU_INDEX` /
/// `REGIA_GPU_NAME` selection of a DX12 adapter opens DX12 (the vendors' Vulkan-only creation
/// hooks step aside), and both vendors then fall back on that device with the built-in
/// upscaler's frame. Opt-in: `REGIA_VENDOR_DX12=1` plus the selection, e.g.
/// `REGIA_GPU_INDEX=1 REGIA_GPU_STRICT=1` on a one-GPU NVIDIA Windows host.
#[test]
fn a_dx12_selection_opens_dx12_and_the_vendors_fall_back() {
    if std::env::var("REGIA_VENDOR_DX12").as_deref() != Ok("1") {
        eprintln!("skipped: set REGIA_VENDOR_DX12=1 with REGIA_GPU_INDEX of a DX12 adapter");
        return;
    }
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let info = device.adapter.get_info();
    eprintln!("adapter {info:?}");
    assert_eq!(
        info.backend,
        wgpu::Backend::Dx12,
        "the selection did not open DX12"
    );
    let q = quality(0.67);
    let frames = 4;
    let plain = render(&device, &mut FrameRenderer::new(), q, frames);
    for vendor in [VendorUpscaler::Dlss, VendorUpscaler::Xess] {
        let mut renderer = FrameRenderer::new();
        renderer.set_vendor_upscaler(Some(vendor));
        let frame = render(&device, &mut renderer, q, frames);
        let report = renderer.vendor_upscaler_report();
        eprintln!("{vendor:?} on DX12: {report:?}");
        assert!(!report.active, "{vendor:?} ran on a DX12 device");
        assert_eq!(report.fallbacks, frames as u64);
        assert!(frame == plain, "{vendor:?}'s fallback changed the frame");
    }
}

#[test]
fn xess_upscales_the_frame_close_to_native_on_a_windows_gpu() {
    if std::env::var("REGIA_XESS_SMOKE").as_deref() != Ok("1") {
        eprintln!("skipped: set REGIA_XESS_SMOKE=1 on Windows with the external SDK");
        return;
    }
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let status = regia_render::upscaler::vendor::xess::xess_status();
    eprintln!(
        "XeSS status before frame: {status:?}; adapter {:?}",
        device.adapter.get_info()
    );
    let frames = 24;
    let mut native_renderer = FrameRenderer::new();
    let _ = render(&device, &mut native_renderer, quality(1.0), 4);
    let start = std::time::Instant::now();
    let native = render(&device, &mut native_renderer, quality(1.0), frames);
    let native_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let mut renderer = FrameRenderer::new();
    renderer.set_vendor_upscaler(Some(VendorUpscaler::Xess));
    let _ = render(&device, &mut renderer, quality(0.67), 4);
    let start = std::time::Instant::now();
    let xess = render(&device, &mut renderer, quality(0.67), frames);
    let xess_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let report = renderer.vendor_upscaler_report();
    assert!(report.active, "XeSS did not run: {report:?}");
    assert_eq!(report.runtime, Some(VendorRuntime::Ready));
    assert_eq!(report.fallbacks, 0);
    let error = mean_abs(&xess, &native);
    eprintln!(
        "XeSS frame report {report:?}; mean |delta green| {error:.3}; offscreen frame+readback ms: native {native_ms:.3}, XeSS {xess_ms:.3}"
    );
    save_xess_evidence("native", &native);
    save_xess_evidence("xess", &xess);
    assert!(error < 6.0, "XeSS frame differs by {error:.3} levels from native");
    assert_ne!(
        xess, native,
        "XeSS output unexpectedly equals the native frame byte for byte"
    );
}

fn save_xess_evidence(name: &str, pixels: &[u8]) {
    let Some(directory) = std::env::var_os("REGIA_XESS_EVIDENCE").map(std::path::PathBuf::from) else {
        return;
    };
    assert!(directory.is_absolute(), "evidence directory must be absolute");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = source
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    assert!(
        !directory.starts_with(root),
        "tests never write into the source tree"
    );
    std::fs::create_dir_all(&directory).expect("evidence directory");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, W, H);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header");
        writer.write_image_data(pixels).expect("PNG pixels");
    }
    std::fs::write(directory.join(format!("{name}.png")), bytes).expect("write evidence image");
}
