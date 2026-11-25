// SPDX-License-Identifier: MPL-2.0
//! Tests of the vendor seam (ADR-1150). No GPU, no SDK: the lifecycle runs on the CPU target with
//! a fake binding that logs every call.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::upscaler::plugin::{
    CpuTarget, CpuUpscaleContext, InputNeed, JitterSign, MotionDirection, UpscaleParams, VendorRuntimes,
};
use crate::upscaler::reference::CpuImage;

// ---------------------------------------------------------------- jitter sequence

#[test]
fn phase_counts_follow_the_ratio() {
    for display in [(2560, 1440), (3840, 2160), (1920, 1080), (1280, 800)] {
        let phases: Vec<u32> = UpscaleQuality::ALL
            .iter()
            .map(|mode| JitterSequence::for_quality(*mode, display).phase_count())
            .collect();
        assert_eq!(phases, vec![8, 18, 23, 32, 72], "{display:?}");
    }
    // Zero sizes read as 1; a ratio far beyond Ultra Performance is capped.
    assert_eq!(JitterSequence::for_sizes((0, 0), (0, 0)).phase_count(), 8);
    assert_eq!(
        JitterSequence::for_sizes((1, 1), (4096, 4096)).phase_count(),
        JITTER_MAX_PHASES
    );
    assert_eq!(
        JitterSequence::for_sizes((1, 1), (u32::MAX, u32::MAX)).phase_count(),
        JITTER_MAX_PHASES,
        "no overflow at the largest sizes"
    );
    // The axis with the larger ratio decides.
    assert_eq!(
        JitterSequence::for_sizes((1280, 1440), (2560, 1440)).phase_count(),
        32
    );
    assert_eq!(
        JitterSequence::for_sizes((2560, 720), (2560, 1440)).phase_count(),
        32
    );
    // A render larger than the display (supersampling) stays at the base.
    assert_eq!(
        JitterSequence::for_sizes((5120, 2880), (2560, 1440)).phase_count(),
        8
    );
}

#[test]
fn the_sequence_is_the_frames_jitter_for_one_cycle_then_wraps() {
    let display = (2560, 1440);
    let render = UpscaleQuality::Quality.render_size(display);
    let seq = JitterSequence::for_quality(UpscaleQuality::Quality, display);
    assert_eq!(seq.phase_count(), 18);
    // Frames 0 ..= phase_count: bit for bit what the frame computes today.
    for frame in 0..=u64::from(seq.phase_count()) {
        assert_eq!(
            seq.ndc(frame, render),
            crate::taau::halton_jitter(frame, render.0, render.1),
            "frame {frame}"
        );
    }
    assert_eq!(seq.halton_index(0), 1);
    assert_eq!(seq.halton_index(1), 1);
    assert_eq!(seq.halton_index(18), 18);
    assert_eq!(seq.halton_index(19), 1, "wraps after the cycle");
    assert_eq!(seq.ndc(19, render), seq.ndc(1, render));
    assert_eq!(seq.ndc(36, render), seq.ndc(18, render));
    let last = seq.halton_index(u64::MAX);
    assert!((1..=18).contains(&last), "{last}");
}

#[test]
fn render_pixels_are_the_ndc_jitter_in_the_vendor_convention() {
    for (render, display) in [
        ((1707, 960), (2560, 1440)),
        ((1280, 720), (2560, 1440)),
        ((853, 480), (2560, 1440)),
        ((33, 17), (99, 51)),
    ] {
        let seq = JitterSequence::for_sizes(render, display);
        for frame in 0..200 {
            let px = seq.render_pixels(frame);
            let converted = InputConventions::RENDER_PIXELS.jitter_from_regia(seq.ndc(frame, render), render);
            assert!(
                (px[0] - converted[0]).abs() < 1.0e-4 && (px[1] - converted[1]).abs() < 1.0e-4,
                "{render:?} frame {frame}: {px:?} vs {converted:?}"
            );
            assert!(px.iter().all(|axis| axis.abs() < 0.5), "{px:?}");
        }
    }
}

/// How many samples of one cycle land in each of the r × r display-pixel cells of a render
/// pixel, from the Halton values (x from base 2, y from base 3).
fn cell_counts(seq: JitterSequence, ratio: usize) -> Vec<usize> {
    let mut cells = vec![0usize; ratio * ratio];
    for frame in 1..=u64::from(seq.phase_count()) {
        let n = seq.halton_index(frame);
        let cell = |h: f32| ((h * ratio as f32).floor() as usize).min(ratio - 1);
        let (cx, cy) = (cell(crate::taau::halton(n, 2)), cell(crate::taau::halton(n, 3)));
        cells[cy * ratio + cx] += 1;
    }
    cells
}

#[test]
fn a_cycle_covers_every_display_pixel_evenly_and_is_centred() {
    let display = (2560, 1440);
    for (mode, ratio) in [
        (UpscaleQuality::Performance, 2usize),
        (UpscaleQuality::UltraPerformance, 3usize),
    ] {
        let seq = JitterSequence::for_quality(mode, display);
        let cells = cell_counts(seq, ratio);
        // 8 samples per display pixel, ± 1 (measured: 7, 8, 8, 9 and 7, 8 × 7, 9).
        assert!(
            cells.iter().all(|count| (7..=9).contains(count)),
            "{mode:?} {cells:?}"
        );
        assert_eq!(cells.iter().sum::<usize>(), seq.phase_count() as usize);
        let offsets: Vec<[f32; 2]> = (1..=u64::from(seq.phase_count()))
            .map(|frame| seq.render_pixels(frame))
            .collect();
        for (i, a) in offsets.iter().enumerate() {
            assert!(
                offsets[i + 1..].iter().all(|b| a != b),
                "{mode:?}: a repeated offset"
            );
        }
        let n = offsets.len() as f32;
        let mean = offsets
            .iter()
            .fold([0.0f32; 2], |m, o| [m[0] + o[0] / n, m[1] + o[1] / n]);
        // Measured −0.015 / 0.019 (Performance), −0.012 / 0.011 (Ultra Performance).
        assert!(
            mean[0].abs() < 0.025 && mean[1].abs() < 0.025,
            "{mode:?} {mean:?}"
        );
    }
    // Without the ratio scaling (8 phases at 3×) some display pixel gets no sample at all.
    let fixed = JitterSequence::for_quality(UpscaleQuality::Native, display);
    assert!(cell_counts(fixed, 3).contains(&0));
}

// ---------------------------------------------------------------- runtime library search

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// A fresh directory per call (pid + counter, like `regia_test_utils::unique_temp_dir`, which
/// this crate cannot depend on: that crate depends on this one).
fn temp_dir(tag: &str) -> std::io::Result<PathBuf> {
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-vendor-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn touch(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, b"not a library")?;
    Ok(path)
}

#[test]
fn library_names_and_variables_per_os() {
    let dlss_win = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Windows);
    assert_eq!(dlss_win.env_var, "REGIA_DLSS_PATH");
    assert_eq!(dlss_win.names, &[LibraryName::Exact("nvngx_dlss.dll")]);
    let dlss_linux = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Linux);
    assert_eq!(
        dlss_linux.names,
        &[LibraryName::Versioned("libnvidia-ngx-dlss.so")]
    );
    let xess_win = RuntimeLibrary::of(VendorUpscaler::Xess, LibraryOs::Windows);
    assert_eq!(xess_win.env_var, "REGIA_XESS_PATH");
    assert_eq!(xess_win.names, &[LibraryName::Exact("libxess.dll")]);
    for (vendor, os) in [
        (VendorUpscaler::Xess, LibraryOs::Linux),
        (VendorUpscaler::Dlss, LibraryOs::Other),
        (VendorUpscaler::Xess, LibraryOs::Other),
    ] {
        assert!(
            RuntimeLibrary::of(vendor, os).names.is_empty(),
            "{vendor:?} {os:?}"
        );
    }
    assert_eq!(
        RuntimeLibrary::current(VendorUpscaler::Dlss),
        RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::CURRENT)
    );
    if cfg!(target_os = "android") || cfg!(target_os = "macos") {
        assert_eq!(LibraryOs::CURRENT, LibraryOs::Other);
    }
}

#[test]
fn version_suffixes_are_numeric_only() {
    let stem = LibraryName::Versioned("libnvidia-ngx-dlss.so");
    assert_eq!(stem.version_of("libnvidia-ngx-dlss.so"), Some(vec![]));
    assert_eq!(
        stem.version_of("libnvidia-ngx-dlss.so.310.4.0"),
        Some(vec![310, 4, 0])
    );
    for not_it in [
        "libnvidia-ngx-dlss.so.",
        "libnvidia-ngx-dlss.so.1..2",
        "libnvidia-ngx-dlss.so.1a",
        "libnvidia-ngx-dlss.so.+1",
        "libnvidia-ngx-dlss.sox",
        "libnvidia-ngx-dlss.so.txt",
        "nvngx_dlss.dll",
    ] {
        assert_eq!(stem.version_of(not_it), None, "{not_it}");
    }
    let exact = LibraryName::Exact("nvngx_dlss.dll");
    assert_eq!(exact.version_of("nvngx_dlss.dll"), Some(vec![]));
    assert_eq!(exact.version_of("nvngx_dlss.dll.bak"), None);
    assert_eq!(exact.version_of("NVNGX_DLSS.DLL"), None);
}

#[test]
fn the_executable_directory_wins_over_the_variable() -> std::io::Result<()> {
    let library = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Windows);
    let exe = temp_dir("exe")?;
    let env = temp_dir("env")?;
    let in_env = touch(&env, "nvngx_dlss.dll")?;
    let search = library.locate(Some(&exe), Some(&env));
    assert_eq!(search.found.as_deref(), Some(in_env.as_path()));
    assert_eq!(search.searched, vec![exe.clone(), env.clone()]);
    assert_eq!(search.state(true), None, "found: the binding loads it");
    let in_exe = touch(&exe, "nvngx_dlss.dll")?;
    let search = library.locate(Some(&exe), Some(&env));
    assert_eq!(search.found.as_deref(), Some(in_exe.as_path()));
    assert_eq!(search.searched, vec![exe.clone()], "stops at the first hit");
    // Nothing anywhere: not found, and both directories were looked in.
    let empty = temp_dir("empty")?;
    let search = library.locate(Some(&empty), None);
    assert_eq!(search.found, None);
    assert_eq!(search.state(true), Some(VendorRuntime::NotFound));
    assert_eq!(search.state(false), Some(VendorRuntime::NotBundled));
    for dir in [exe, env, empty] {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[test]
fn a_relative_variable_is_refused_and_the_working_directory_never_searched() {
    let library = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Linux);
    let search = library.locate(None, Some(Path::new("vendor/dlss")));
    assert_eq!(search.found, None);
    assert!(search.searched.is_empty(), "{:?}", search.searched);
    assert_eq!(search.refused_relative, Some(PathBuf::from("vendor/dlss")));
    assert_eq!(library.locate(None, None), RuntimeSearch::default());
}

#[test]
fn the_highest_numeric_version_is_picked() -> std::io::Result<()> {
    let library = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Linux);
    let dir = temp_dir("versions")?;
    for name in [
        "libnvidia-ngx-dlss.so.310.1.0",
        "libnvidia-ngx-dlss.so.99",
        "libnvidia-ngx-dlss.so.txt",
        "libnvidia-ngx-dlss.so.1000.bak",
    ] {
        touch(&dir, name)?;
    }
    let best = touch(&dir, "libnvidia-ngx-dlss.so.310.10.0")?;
    // A directory with a higher version is not a library.
    std::fs::create_dir_all(dir.join("libnvidia-ngx-dlss.so.999"))?;
    let search = library.locate(Some(&dir), None);
    assert_eq!(search.found.as_deref(), Some(best.as_path()));
    // An OS without a runtime finds nothing even with files present.
    let other = RuntimeLibrary::of(VendorUpscaler::Dlss, LibraryOs::Other);
    assert_eq!(other.locate(Some(&dir), None).found, None);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

// ---------------------------------------------------------------- Vulkan extensions

fn request(instance: &[&str], device: &[&str]) -> VulkanExtensionRequest {
    VulkanExtensionRequest {
        instance: instance.iter().map(|s| (*s).to_string()).collect(),
        device: device.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn extension_requests_are_checked_against_what_was_enabled() {
    let dlss = request(&["VK_inst_a"], &["VK_dev_a", "VK_dev_b"]);
    let xess = request(&["VK_inst_a", "VK_inst_b"], &["VK_dev_b", "VK_dev_c"]);
    assert_eq!(
        dlss.missing(&[], &["VK_dev_b"]),
        vec!["VK_inst_a".to_string(), "VK_dev_a".to_string()]
    );
    assert!(dlss.is_satisfied(&["VK_inst_a", "VK_other"], &["VK_dev_b", "VK_dev_a"]));
    let both = dlss.union(&xess);
    assert_eq!(
        both,
        request(&["VK_inst_a", "VK_inst_b"], &["VK_dev_a", "VK_dev_b", "VK_dev_c"])
    );
    assert_eq!(both.union(&both), both, "idempotent");
    // With the union enabled both runtimes are satisfied; with DLSS's alone only DLSS is.
    let all_i = ["VK_inst_a", "VK_inst_b"];
    let all_d = ["VK_dev_a", "VK_dev_b", "VK_dev_c"];
    assert_eq!(
        features_from_extensions(Some(&dlss), Some(&xess), &all_i, &all_d),
        UpscalerFeatures {
            dlss_vulkan_extensions: true,
            xess_vulkan_extensions: true,
        }
    );
    assert_eq!(
        features_from_extensions(
            Some(&dlss),
            Some(&xess),
            &["VK_inst_a"],
            &["VK_dev_a", "VK_dev_b"]
        ),
        UpscalerFeatures {
            dlss_vulkan_extensions: true,
            xess_vulkan_extensions: false,
        }
    );
    // A runtime that did not answer counts as missing; one that needs nothing is satisfied.
    assert_eq!(
        features_from_extensions(None, Some(&VulkanExtensionRequest::default()), &[], &[]),
        UpscalerFeatures {
            dlss_vulkan_extensions: false,
            xess_vulkan_extensions: true,
        }
    );
}

// ---------------------------------------------------------------- stub bindings

#[test]
fn the_stub_bindings_report_not_bundled_and_refuse_every_call() {
    let display = (2560, 1440);
    let dlss = DlssUpscaler::new(DlssSdk, UpscaleQuality::Quality);
    let xess = XessUpscaler::new(XessSdk, UpscaleQuality::Quality);
    let backends: [&dyn Upscaler<GpuTarget>; 2] = [&dlss, &xess];
    let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
    assert_eq!(names, vec!["dlss", "xess"]);
    for backend in backends {
        assert_eq!(backend.contract(), InputContract::VENDOR);
        // No runtime to ask: ADR-324's rule.
        assert_eq!(backend.render_size(display, UpscaleQuality::Quality), (1707, 960));
    }
    let not_bundled = Some("runtime_unavailable");
    assert_eq!(dlss.sdk().vendor(), VendorUpscaler::Dlss);
    assert_eq!(xess.sdk().vendor(), VendorUpscaler::Xess);
    assert_eq!(
        dlss.sdk().vulkan_extensions().err().map(|e| e.code()),
        not_bundled
    );
    assert_eq!(
        xess.sdk().vulkan_extensions().err().map(|e| e.code()),
        not_bundled
    );
    assert_eq!(
        dlss.sdk()
            .optimal_settings(display, UpscaleQuality::Performance)
            .err(),
        Some(UpscaleError::RuntimeUnavailable {
            reason: reason::SDK_NOT_BUNDLED
        })
    );
    // The stubs' runtime state is what the probe assumes for this build.
    assert_eq!(
        VendorRuntimes {
            dlss: dlss.runtime(),
            xess: xess.runtime(),
        },
        VendorRuntimes::THIS_BUILD
    );
    assert_eq!(dlss.stats(), VendorStats::default());
    assert_eq!(dlss.live_feature(), None);
}

// ---------------------------------------------------------------- feature lifecycle

#[derive(Debug, Default)]
struct FakeLog {
    created: Vec<(FeatureHandle, FeatureDesc)>,
    released: Vec<FeatureHandle>,
    evals: Vec<(FeatureHandle, VendorEvalParams)>,
}

/// A binding that logs every call and writes a nearest-texel upscale.
#[derive(Debug)]
struct FakeSdk {
    runtime: VendorRuntime,
    contract: InputContract,
    log: Rc<RefCell<FakeLog>>,
    next_handle: u64,
    fail_creates: u32,
    fail_evaluates: u32,
    /// Answer the size query with half the display (a runtime with its own optimum).
    half_size: bool,
    /// Answer the size query with these settings as they are (a corrupt runtime).
    answer: Option<OptimalSettings>,
}

impl FakeSdk {
    fn new() -> (Self, Rc<RefCell<FakeLog>>) {
        let log = Rc::new(RefCell::new(FakeLog::default()));
        (
            Self {
                runtime: VendorRuntime::Ready,
                contract: InputContract::VENDOR,
                log: Rc::clone(&log),
                next_handle: 1,
                fail_creates: 0,
                fail_evaluates: 0,
                half_size: false,
                answer: None,
            },
            log,
        )
    }
}

impl VendorRuntimeApi for FakeSdk {
    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Dlss
    }

    fn runtime(&self) -> VendorRuntime {
        self.runtime
    }

    fn contract(&self) -> InputContract {
        self.contract
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        Ok(VulkanExtensionRequest::default())
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        if self.runtime != VendorRuntime::Ready {
            return Err(UpscaleError::RuntimeUnavailable {
                reason: reason::RUNTIME_NOT_FOUND,
            });
        }
        if let Some(answer) = self.answer {
            return Ok(answer);
        }
        if self.half_size {
            let half = (display.0 / 2, display.1 / 2);
            return Ok(OptimalSettings {
                render_size: half,
                render_min: half,
                render_max: half,
                sharpness: 0.0,
            });
        }
        Ok(OptimalSettings::from_rule(display, quality))
    }

    fn release_feature(&mut self, feature: FeatureHandle) {
        self.log.borrow_mut().released.push(feature);
    }
}

impl VendorEvaluate<CpuTarget> for FakeSdk {
    fn create_feature(
        &mut self,
        _ctx: &mut CpuUpscaleContext<'_>,
        desc: &FeatureDesc,
        _settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        if self.fail_creates > 0 {
            self.fail_creates -= 1;
            return Err(UpscaleError::Backend("create refused".into()));
        }
        let handle = FeatureHandle(self.next_handle);
        self.next_handle += 1;
        self.log.borrow_mut().created.push((handle, *desc));
        Ok(handle)
    }

    fn evaluate(
        &mut self,
        feature: FeatureHandle,
        ctx: &mut CpuUpscaleContext<'_>,
        frame: &UpscaleFrame<'_, CpuImage>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        if self.fail_evaluates > 0 {
            self.fail_evaluates -= 1;
            return Err(UpscaleError::Backend("evaluate failed".into()));
        }
        let (dw, dh) = frame.params.display_size;
        let (rw, rh) = eval.render_size;
        *ctx.output = CpuImage::from_fn(dw, dh, |x, y| frame.color.texel(x * rw / dw, y * rh / dh));
        self.log.borrow_mut().evals.push((feature, *eval));
        Ok(())
    }
}

const DISPLAY: (u32, u32) = (64, 32);

/// A Performance frame of the fake's inputs at `render` (all four images given).
fn run_frame(
    backend: &mut VendorBackend<FakeSdk>,
    render: (u32, u32),
    display: (u32, u32),
    params: impl FnOnce(&mut UpscaleParams),
) -> Result<CpuImage, UpscaleError> {
    let color = CpuImage::from_fn(render.0, render.1, |x, y| [x as f32, y as f32, 0.0, 1.0]);
    let depth = CpuImage::new(render.0, render.1);
    let motion = CpuImage::new(render.0, render.1);
    let mut p = UpscaleParams::new(render, display);
    params(&mut p);
    let frame = UpscaleFrame {
        color: &color,
        depth: Some(&depth),
        motion: Some(&motion),
        reactive: None,
        transparency: None,
        params: p,
    };
    let mut output = CpuImage::new(1, 1);
    let mut ctx = CpuUpscaleContext { output: &mut output };
    let upscaler: &mut dyn Upscaler<CpuTarget> = backend;
    upscaler.upscale(&mut ctx, &frame)?;
    Ok(output)
}

fn perf_render() -> (u32, u32) {
    UpscaleQuality::Performance.render_size(DISPLAY)
}

fn resets(log: &Rc<RefCell<FakeLog>>) -> Vec<bool> {
    log.borrow().evals.iter().map(|(_, e)| e.reset).collect()
}

/// Audit wave 6 (upscalers M4): a runtime whose size answer is out of order, zero or larger
/// than the display gets no feature created at those sizes; the frame gets an error (and so its
/// fallback), and the render size the frame is told is the mode's rule, not the bogus answer.
#[test]
fn a_bogus_size_answer_creates_no_feature_and_the_frame_falls_back() {
    let rule = OptimalSettings::from_rule(DISPLAY, UpscaleQuality::Performance);
    assert!(rule.is_sane_for(DISPLAY));
    let bogus = [
        OptimalSettings {
            render_max: (u32::MAX, u32::MAX),
            ..rule
        },
        OptimalSettings {
            render_size: (0, rule.render_size.1),
            render_min: (0, 0),
            ..rule
        },
        OptimalSettings {
            render_size: (rule.render_max.0 + 1, rule.render_size.1),
            ..rule
        },
        OptimalSettings {
            render_min: rule.render_max,
            render_max: rule.render_min,
            ..rule
        },
    ];
    for answer in bogus {
        assert!(!answer.is_sane_for(DISPLAY), "{answer:?}");
        let (mut sdk, log) = FakeSdk::new();
        sdk.answer = Some(answer);
        let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
        let upscaler: &dyn Upscaler<CpuTarget> = &backend;
        assert_eq!(
            upscaler.render_size(DISPLAY, UpscaleQuality::Performance),
            UpscaleQuality::Performance.render_size(DISPLAY),
            "{answer:?}"
        );
        let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).expect_err("refused");
        assert_eq!(error.code(), "backend_failed", "{answer:?}");
        assert!(log.borrow().created.is_empty(), "no feature at {answer:?}");
        assert_eq!(backend.stats().failures, 1);
    }
}

#[test]
fn a_feature_is_created_once_and_recreated_only_when_its_description_changes() -> Result<(), UpscaleError> {
    let (sdk, log) = FakeSdk::new();
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    for _ in 0..3 {
        let out = run_frame(&mut backend, perf_render(), DISPLAY, |_| {})?;
        assert_eq!(out.size(), DISPLAY, "the binding wrote the output");
    }
    assert_eq!(log.borrow().created.len(), 1);
    assert_eq!(
        resets(&log),
        vec![true, false, false],
        "a new feature starts without history"
    );
    assert_eq!(
        backend.live_feature(),
        Some(FeatureDesc {
            display_size: DISPLAY,
            quality: UpscaleQuality::Performance,
            flags: FeatureFlags::REGIA,
        })
    );
    // The frame's own reset passes through without a recreate.
    run_frame(&mut backend, perf_render(), DISPLAY, |p| p.reset = true)?;
    assert_eq!(log.borrow().created.len(), 1);
    assert_eq!(resets(&log).last(), Some(&true));

    // Display size, quality and flags each recreate it, releasing the old one first.
    let bigger = (128, 64);
    run_frame(
        &mut backend,
        UpscaleQuality::Performance.render_size(bigger),
        bigger,
        |_| {},
    )?;
    backend.set_quality(UpscaleQuality::Quality);
    run_frame(
        &mut backend,
        UpscaleQuality::Quality.render_size(bigger),
        bigger,
        |_| {},
    )?;
    backend.set_flags(FeatureFlags {
        auto_exposure: true,
        ..FeatureFlags::REGIA
    });
    run_frame(
        &mut backend,
        UpscaleQuality::Quality.render_size(bigger),
        bigger,
        |_| {},
    )?;
    // Setting the same quality again changes nothing.
    backend.set_quality(UpscaleQuality::Quality);
    run_frame(
        &mut backend,
        UpscaleQuality::Quality.render_size(bigger),
        bigger,
        |_| {},
    )?;
    {
        let log = log.borrow();
        assert_eq!(log.created.len(), 4);
        let released: Vec<FeatureHandle> = log.created.iter().take(3).map(|(h, _)| *h).collect();
        assert_eq!(log.released, released, "each old feature released, in order");
    }
    assert_eq!(
        resets(&log),
        vec![true, false, false, true, true, true, true, false]
    );
    let stats = backend.stats();
    assert_eq!(stats.features_created, 4);
    assert_eq!(stats.features_released, 3);
    assert_eq!(stats.live_features(), 1);
    assert_eq!(stats.evaluations, 8);
    assert_eq!(stats.failures, 0);
    // Dropping the backend releases the last one: nothing leaks.
    drop(backend);
    let log = log.borrow();
    assert_eq!(log.released.len(), log.created.len());
    Ok(())
}

#[test]
fn eval_params_are_in_the_vendor_convention() -> Result<(), UpscaleError> {
    let (sdk, log) = FakeSdk::new();
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    let render = perf_render();
    let seq = JitterSequence::for_sizes(render, DISPLAY);
    for frame in 1..=u64::from(seq.phase_count()) {
        run_frame(&mut backend, render, DISPLAY, |p| {
            p.jitter = seq.ndc(frame, render);
            p.exposure = 0.25;
            p.history_scale = 0.0;
            p.sharpness = 0.5;
            p.frame_time_ms = 8.0;
        })?;
        let eval = log.borrow().evals.last().map(|(_, e)| *e);
        let Some(eval) = eval else {
            panic!("no evaluate logged"); // invariant: test-only; the frame succeeded
        };
        // The runtime sees the same offset the projection used, in render pixels, y down.
        let px = seq.render_pixels(frame);
        assert!(
            (eval.jitter[0] - px[0]).abs() < 1.0e-4 && (eval.jitter[1] - px[1]).abs() < 1.0e-4,
            "frame {frame}: {:?} vs {px:?}",
            eval.jitter
        );
        assert_eq!(eval.motion_scale, [render.0 as f32, render.1 as f32]);
        assert_eq!(eval.render_size, render);
        assert_eq!(eval.exposure, 0.25);
        assert_eq!(eval.history_scale, 1.0, "0 reads as unchanged");
        assert_eq!(eval.sharpness, 0.5);
        assert_eq!(eval.frame_time_ms, 8.0);
    }

    // A vendor whose SDK wants the sample sign and current-minus-previous motion: one contract
    // value flips both, and nothing else changes.
    let (mut sdk, log) = FakeSdk::new();
    sdk.contract = InputContract {
        conventions: InputConventions {
            motion_direction: MotionDirection::ToCurrent,
            jitter_sign: JitterSign::Sample,
            ..InputConventions::RENDER_PIXELS
        },
        ..InputContract::VENDOR
    };
    let mut flipped = VendorBackend::new(sdk, UpscaleQuality::Performance);
    run_frame(&mut flipped, render, DISPLAY, |p| p.jitter = seq.ndc(5, render))?;
    let eval = log.borrow().evals.last().map(|(_, e)| *e);
    let px = seq.render_pixels(5);
    let Some(eval) = eval else {
        panic!("no evaluate logged"); // invariant: test-only; the frame succeeded
    };
    assert!((eval.jitter[0] + px[0]).abs() < 1.0e-4 && (eval.jitter[1] + px[1]).abs() < 1.0e-4);
    assert_eq!(eval.motion_scale, [-(render.0 as f32), -(render.1 as f32)]);
    Ok(())
}

#[test]
fn the_render_size_must_stay_in_the_features_range() -> Result<(), UpscaleError> {
    let (sdk, log) = FakeSdk::new();
    let display = (256, 144);
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Quality);
    // Quality at 256×144 accepts Balanced (151×85) up to Quality (171×96): dynamic resolution
    // moves inside that range without a recreate.
    run_frame(&mut backend, (171, 96), display, |_| {})?;
    run_frame(&mut backend, (160, 90), display, |_| {})?;
    run_frame(&mut backend, (151, 85), display, |_| {})?;
    let settings = backend.live_settings();
    assert_eq!(
        settings.map(|s| (s.render_min, s.render_max)),
        Some(((151, 85), (171, 96)))
    );
    let error = run_frame(&mut backend, (128, 72), display, |_| {}).err();
    assert_eq!(
        error,
        Some(UpscaleError::RenderSizeOutOfRange {
            render: (128, 72),
            min: (151, 85),
            max: (171, 96),
        })
    );
    // A caller error: not a runtime failure, and the feature is kept.
    assert_eq!(backend.stats().failures, 0);
    assert!(!backend.is_disabled());
    assert_eq!(log.borrow().created.len(), 1);
    assert_eq!(log.borrow().evals.len(), 3);
    run_frame(&mut backend, (171, 96), display, |_| {})?;
    assert_eq!(resets(&log), vec![true, false, false, false]);
    Ok(())
}

#[test]
fn failures_reset_the_history_and_three_in_a_row_disable_the_backend() -> Result<(), UpscaleError> {
    let (sdk, log) = FakeSdk::new();
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    run_frame(&mut backend, perf_render(), DISPLAY, |_| {})?;
    // Two failures, then a success: the success drops the history, nothing is disabled.
    backend.sdk_mut().fail_evaluates = 2;
    for _ in 0..2 {
        let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).err();
        assert_eq!(error.as_ref().map(UpscaleError::code), Some("backend_failed"));
    }
    run_frame(&mut backend, perf_render(), DISPLAY, |_| {})?;
    assert_eq!(resets(&log), vec![true, true]);
    assert!(!backend.is_disabled());
    assert_eq!(backend.stats().failures, 2);

    // Three in a row: disabled, the feature released, and the runtime is not called again.
    backend.sdk_mut().fail_evaluates = 3;
    for _ in 0..3 {
        assert!(run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).is_err());
    }
    assert!(backend.is_disabled());
    assert_eq!(backend.stats().live_features(), 0);
    assert_eq!(log.borrow().released.len(), 1);
    let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).err();
    assert_eq!(error, Some(UpscaleError::Disabled { failures: 3 }));
    assert_eq!(log.borrow().evals.len(), 2, "no call while disabled");
    assert_eq!(MAX_CONSECUTIVE_FAILURES, 3);

    // Re-enabled (after a reload), the next frame creates a new feature without history.
    backend.reenable();
    run_frame(&mut backend, perf_render(), DISPLAY, |_| {})?;
    assert_eq!(log.borrow().created.len(), 2);
    assert_eq!(resets(&log), vec![true, true, true]);
    Ok(())
}

#[test]
fn the_runtime_state_and_creation_failures_are_checked_before_any_evaluate() {
    let (mut sdk, log) = FakeSdk::new();
    sdk.runtime = VendorRuntime::NotFound;
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).err();
    assert_eq!(
        error,
        Some(UpscaleError::RuntimeUnavailable {
            reason: reason::RUNTIME_NOT_FOUND
        })
    );
    assert_eq!(
        backend.stats().failures,
        0,
        "a missing runtime is a state, not a failure"
    );
    assert!(log.borrow().created.is_empty());

    for (runtime, code) in [
        (VendorRuntime::NotBundled, reason::SDK_NOT_BUNDLED),
        (
            VendorRuntime::ReportsUnsupported,
            reason::RUNTIME_REPORTS_UNSUPPORTED,
        ),
        (VendorRuntime::NeedsNewerDriver, reason::NEEDS_NEWER_DRIVER),
    ] {
        backend.sdk_mut().runtime = runtime;
        let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).err();
        assert_eq!(error, Some(UpscaleError::RuntimeUnavailable { reason: code }));
    }

    backend.sdk_mut().runtime = VendorRuntime::Ready;
    backend.sdk_mut().fail_creates = 1;
    let error = run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).err();
    assert_eq!(error, Some(UpscaleError::Backend("create refused".into())));
    assert_eq!(backend.stats().failures, 1);
    assert_eq!(backend.stats().features_created, 0);
    assert!(run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).is_ok());
    assert_eq!(backend.stats().features_created, 1);
    assert_eq!(resets(&log), vec![true]);
}

#[test]
fn bad_frames_are_refused_before_the_runtime_sees_them() {
    let (sdk, log) = FakeSdk::new();
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    let render = perf_render();
    // Jitter given in pixels instead of NDC (0.4 NDC is 6.4 px at 32 wide), or not finite.
    for jitter in [[0.4, 0.0], [0.0, -0.2], [f32::NAN, 0.0], [0.0, f32::INFINITY]] {
        let error = run_frame(&mut backend, render, DISPLAY, |p| p.jitter = jitter).err();
        assert_eq!(error, Some(UpscaleError::InvalidJitter { render }), "{jitter:?}");
    }
    // Exactly half a pixel is the edge of the sequence's range and passes.
    let half = [1.0 / render.0 as f32, 1.0 / render.1 as f32];
    assert!(run_frame(&mut backend, render, DISPLAY, |p| p.jitter = half).is_ok());
    let created_after_good_frame = log.borrow().created.len();
    // 4× is beyond every vendor's range.
    let error = run_frame(&mut backend, (16, 8), DISPLAY, |_| {}).err();
    assert_eq!(error.as_ref().map(UpscaleError::code), Some("unsupported_ratio"));
    // Missing motion vectors.
    let color = CpuImage::new(render.0, render.1);
    let frame = UpscaleFrame {
        color: &color,
        depth: Some(&color),
        motion: None,
        reactive: None,
        transparency: None,
        params: UpscaleParams::new(render, DISPLAY),
    };
    let mut output = CpuImage::new(1, 1);
    let upscaler: &mut dyn Upscaler<CpuTarget> = &mut backend;
    let error = upscaler
        .upscale(&mut CpuUpscaleContext { output: &mut output }, &frame)
        .err();
    assert_eq!(error, Some(UpscaleError::MissingInput { input: "motion" }));
    assert_eq!(InputContract::VENDOR.motion, InputNeed::Required);
    assert_eq!(log.borrow().created.len(), created_after_good_frame);
    assert_eq!(
        log.borrow().evals.len(),
        1,
        "only the good frame reached the runtime"
    );
    assert_eq!(backend.stats().failures, 0);
}

#[test]
fn the_render_size_comes_from_the_runtime_when_it_answers() {
    let (mut sdk, _log) = FakeSdk::new();
    sdk.half_size = true;
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Quality);
    let upscaler: &dyn Upscaler<CpuTarget> = &backend;
    assert_eq!(
        upscaler.render_size((3840, 2160), UpscaleQuality::Quality),
        (1920, 1080)
    );
    assert_eq!(upscaler.name(), "dlss");
    backend.sdk_mut().runtime = VendorRuntime::NotFound;
    let upscaler: &dyn Upscaler<CpuTarget> = &backend;
    assert_eq!(
        upscaler.render_size((3840, 2160), UpscaleQuality::Quality),
        (2560, 1440),
        "no answer: ADR-324's rule"
    );
}

#[test]
fn every_new_code_is_snake_case_ascii() {
    let runtime_names = [
        VendorRuntime::CompiledIn,
        VendorRuntime::NotBundled,
        VendorRuntime::NotFound,
        VendorRuntime::Found,
        VendorRuntime::ReportsUnsupported,
        VendorRuntime::NeedsNewerDriver,
        VendorRuntime::Ready,
    ]
    .map(VendorRuntime::name);
    let error_codes = [
        UpscaleError::RuntimeUnavailable { reason: "x" },
        UpscaleError::RenderSizeOutOfRange {
            render: (1, 1),
            min: (1, 1),
            max: (1, 1),
        },
        UpscaleError::InvalidJitter { render: (1, 1) },
        UpscaleError::Disabled { failures: 1 },
    ]
    .map(|e| e.code());
    for code in runtime_names.into_iter().chain(error_codes) {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    assert_eq!(
        runtime_names,
        [
            "compiled_in",
            "not_bundled",
            "not_found",
            "found",
            "reports_unsupported",
            "needs_newer_driver",
            "ready"
        ]
    );
}

// ---------------------------------------------------------------- end to end on the CPU

/// A stand-in runtime that reconstructs by placing every render sample where the jitter it is
/// given says the sample was taken, then averaging per display pixel: about what a jitter-aware
/// runtime does with a still camera. `reads_sample_sign` makes it read the jitter with the
/// opposite sign from the one its contract declares (a contract that disagrees with the SDK).
#[derive(Debug)]
struct AccumulatingSdk {
    contract: InputContract,
    reads_sample_sign: bool,
    sum: Vec<f32>,
    count: Vec<u32>,
    min_count: u32,
}

impl AccumulatingSdk {
    fn new(contract: InputContract, reads_sample_sign: bool) -> Self {
        Self {
            contract,
            reads_sample_sign,
            sum: Vec::new(),
            count: Vec::new(),
            min_count: 0,
        }
    }
}

impl VendorRuntimeApi for AccumulatingSdk {
    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Xess
    }

    fn runtime(&self) -> VendorRuntime {
        VendorRuntime::Ready
    }

    fn contract(&self) -> InputContract {
        self.contract
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        Ok(VulkanExtensionRequest::default())
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        Ok(OptimalSettings::from_rule(display, quality))
    }

    fn release_feature(&mut self, _feature: FeatureHandle) {}
}

impl VendorEvaluate<CpuTarget> for AccumulatingSdk {
    fn create_feature(
        &mut self,
        _ctx: &mut CpuUpscaleContext<'_>,
        _desc: &FeatureDesc,
        _settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        Ok(FeatureHandle(1))
    }

    fn evaluate(
        &mut self,
        _feature: FeatureHandle,
        ctx: &mut CpuUpscaleContext<'_>,
        frame: &UpscaleFrame<'_, CpuImage>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        let (dw, dh) = frame.params.display_size;
        let (rw, rh) = eval.render_size;
        let pixels = dw as usize * dh as usize;
        if eval.reset || self.sum.len() != pixels {
            self.sum = vec![0.0; pixels];
            self.count = vec![0; pixels];
        }
        // The runtime's reading of its jitter, in render pixels, as the projection offset.
        let sign = if self.reads_sample_sign { -1.0 } else { 1.0 };
        let (jx, jy) = (sign * eval.jitter[0], sign * eval.jitter[1]);
        let (sx, sy) = (dw as f32 / rw as f32, dh as f32 / rh as f32);
        for j in 0..rh {
            for i in 0..rw {
                // The texel centre saw the scene at its position minus the projection offset.
                let px = (i as f32 + 0.5 - jx) * sx;
                let py = (j as f32 + 0.5 - jy) * sy;
                let x = (px.floor().max(0.0) as u32).min(dw - 1);
                let y = (py.floor().max(0.0) as u32).min(dh - 1);
                let at = y as usize * dw as usize + x as usize;
                self.sum[at] += frame.color.texel(i, j)[0];
                self.count[at] += 1;
            }
        }
        self.min_count = self.count.iter().copied().min().unwrap_or(0);
        let (sum, count) = (&self.sum, &self.count);
        *ctx.output = CpuImage::from_fn(dw, dh, |x, y| {
            let at = y as usize * dw as usize + x as usize;
            let v = if count[at] == 0 {
                0.0
            } else {
                sum[at] / count[at] as f32
            };
            [v, v, v, 1.0]
        });
        Ok(())
    }
}

/// Runs one jitter cycle of the check scene through a [`VendorBackend`] over `sdk` and returns
/// the RMSE against the supersampled truth and the fewest samples any display pixel got.
fn one_cycle(
    sdk: AccumulatingSdk,
    render: (u32, u32),
    display: (u32, u32),
) -> Result<(f32, u32), UpscaleError> {
    use crate::upscaler::reference::{check_scene, render_scene, rmse, scene_truth};
    let seq = JitterSequence::for_sizes(render, display);
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    // The mode's range must hold the explicit render size.
    backend.set_quality(
        UpscaleQuality::ALL
            .into_iter()
            .find(|mode| mode.render_size(display) == render)
            .unwrap_or(UpscaleQuality::Performance),
    );
    let mut output = CpuImage::new(1, 1);
    for frame in 1..=u64::from(seq.phase_count()) {
        let jitter = seq.ndc(frame, render);
        let color = render_scene(check_scene, render, jitter, [0.0, 0.0]);
        let depth = CpuImage::new(render.0, render.1);
        let motion = CpuImage::new(render.0, render.1);
        let frame = UpscaleFrame {
            color: &color,
            depth: Some(&depth),
            motion: Some(&motion),
            reactive: None,
            transparency: None,
            params: UpscaleParams {
                jitter,
                ..UpscaleParams::new(render, display)
            },
        };
        let upscaler: &mut dyn Upscaler<CpuTarget> = &mut backend;
        upscaler.upscale(&mut CpuUpscaleContext { output: &mut output }, &frame)?;
    }
    let truth = scene_truth(check_scene, display, [0.0, 0.0], 8);
    let error = rmse(&output, &truth).unwrap_or(f32::INFINITY);
    Ok((error, backend.sdk().min_count))
}

#[test]
fn the_runtime_puts_each_sample_where_the_projection_took_it() -> Result<(), UpscaleError> {
    // A Python mirror of this test (float64) measured 0.0157 RMSE at 2× (every display pixel
    // got at least 7 samples) and 0.0163 at 22 → 64 (68 phases, at least 4 samples: the ratio
    // is not an integer, so the cells are uneven); with the sign read the other way 0.143, 0.160
    // and 0.164. The thresholds keep a wide margin either side.
    for (render, display) in [((32, 32), (64, 64)), ((32, 16), (64, 32)), ((22, 22), (64, 64))] {
        let (good, min_count) = one_cycle(
            AccumulatingSdk::new(InputContract::VENDOR, false),
            render,
            display,
        )?;
        assert!(good < 0.03, "{render:?}: {good}");
        assert!(
            min_count >= 3,
            "{render:?}: a display pixel got {min_count} samples"
        );
        // The contract says projection sign, the runtime reads sample sign: the samples land
        // in the mirrored display pixel and the image is wrong, without any error.
        let (wrong, _) = one_cycle(AccumulatingSdk::new(InputContract::VENDOR, true), render, display)?;
        assert!(wrong > 0.10, "{render:?}: {wrong}");
        assert!(wrong > 4.0 * good, "{render:?}: {wrong} vs {good}");
        // Fixing the contract (one value) fixes it.
        let sample_contract = InputContract {
            conventions: InputConventions {
                jitter_sign: JitterSign::Sample,
                ..InputConventions::RENDER_PIXELS
            },
            ..InputContract::VENDOR
        };
        let (fixed, _) = one_cycle(AccumulatingSdk::new(sample_contract, true), render, display)?;
        assert!((fixed - good).abs() < 1.0e-6, "{render:?}: {fixed} vs {good}");
    }
    Ok(())
}

// ---------------------------------------------------------------- preferred range (ADR-2679)

/// A range unlike any built-in mode: preferred 77 %, accepted 61 % to 91 % of a 1000×500 display.
const ODD_RANGE: PreferredRange = PreferredRange {
    preferred: (770, 385),
    min: (610, 305),
    max: (910, 455),
};

const ODD_DISPLAY: (u32, u32) = (1000, 500);

#[test]
fn the_backend_answers_the_runtimes_range_and_no_built_in_has_one() {
    let (mut sdk, _log) = FakeSdk::new();
    sdk.answer = Some(OptimalSettings {
        render_size: ODD_RANGE.preferred,
        render_min: ODD_RANGE.min,
        render_max: ODD_RANGE.max,
        sharpness: 0.0,
    });
    let backend = VendorBackend::new(sdk, UpscaleQuality::Quality);
    let upscaler: &dyn Upscaler<CpuTarget> = &backend;
    assert_eq!(
        upscaler.preferred_range(ODD_DISPLAY, UpscaleQuality::Quality),
        Some(ODD_RANGE)
    );
    // The size query and the range agree.
    assert_eq!(
        upscaler.render_size(ODD_DISPLAY, UpscaleQuality::Quality),
        (770, 385)
    );
    // A display the answer does not fit: not usable, so no range.
    assert_eq!(
        upscaler.preferred_range((800, 400), UpscaleQuality::Quality),
        None
    );

    // Built-ins and the reference leave the frame alone.
    let reference = crate::upscaler::reference::ReferenceUpscaler::new(Default::default());
    let cpu: &dyn Upscaler<CpuTarget> = &reference;
    assert_eq!(cpu.preferred_range(ODD_DISPLAY, UpscaleQuality::Quality), None);

    // A runtime that is not ready, a stub binding or a disabled backend has no opinion.
    let (mut sdk, _log) = FakeSdk::new();
    sdk.runtime = VendorRuntime::NeedsNewerDriver;
    assert_eq!(
        VendorBackend::new(sdk, UpscaleQuality::Quality)
            .preferred_range(ODD_DISPLAY, UpscaleQuality::Quality),
        None
    );
    assert_eq!(
        DlssUpscaler::new(DlssSdk, UpscaleQuality::Quality)
            .preferred_range(ODD_DISPLAY, UpscaleQuality::Quality),
        None
    );
    assert_eq!(
        XessUpscaler::new(XessSdk, UpscaleQuality::Quality)
            .preferred_range(ODD_DISPLAY, UpscaleQuality::Quality),
        None
    );
    let (mut sdk, _log) = FakeSdk::new();
    sdk.fail_creates = MAX_CONSECUTIVE_FAILURES;
    let mut backend = VendorBackend::new(sdk, UpscaleQuality::Performance);
    assert!(
        backend
            .preferred_range(DISPLAY, UpscaleQuality::Performance)
            .is_some()
    );
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        assert!(run_frame(&mut backend, perf_render(), DISPLAY, |_| {}).is_err());
    }
    assert!(backend.is_disabled());
    assert_eq!(
        backend.preferred_range(DISPLAY, UpscaleQuality::Performance),
        None
    );
}

#[test]
fn a_frames_size_is_the_preferred_one_under_a_mode_and_stays_in_the_range() {
    let mode = Some(UpscaleQuality::Quality);
    let requested = (667, 334);
    assert_eq!(ODD_RANGE.frame_extent(requested, 667, mode), (770, 385));
    // Below and above the mode's permille it scales the preferred size, with no jump.
    assert_eq!(ODD_RANGE.frame_extent(requested, 650, mode), (750, 375));
    assert_eq!(ODD_RANGE.frame_extent(requested, 700, mode), (808, 404));
    // And stops at the range.
    assert_eq!(ODD_RANGE.frame_extent(requested, 1, mode), ODD_RANGE.min);
    assert_eq!(ODD_RANGE.frame_extent(requested, 5000, mode), ODD_RANGE.max);
    // No mode: the frame's own scale, inside the range.
    assert_eq!(ODD_RANGE.frame_extent((800, 400), 800, None), (800, 400));
    assert_eq!(ODD_RANGE.frame_extent((100, 50), 100, None), ODD_RANGE.min);
    assert_eq!(ODD_RANGE.frame_extent((1000, 500), 1000, None), ODD_RANGE.max);
    // The axes clamp apart.
    assert_eq!(ODD_RANGE.clamp((100, 500)), (610, 455));

    // A runtime that says min 0 (no floor) still renders something.
    let open = PreferredRange {
        preferred: (100, 50),
        min: (0, 0),
        max: (200, 100),
    };
    assert_eq!(open.clamp((0, 0)), (1, 1));
    assert!(!open.is_static());
    assert!(!ODD_RANGE.is_static());
}

#[test]
fn dynamic_resolution_stays_inside_the_runtimes_range() {
    use crate::{DynResConfig, DynResController};
    let mode = Some(UpscaleQuality::Quality);
    // Under the mode the range is relative to its 667 ‰, the runtime's preferred size.
    assert_eq!(ODD_RANGE.permille_range(ODD_DISPLAY, mode), (529, 788));
    // Without one it is a share of the display.
    assert_eq!(ODD_RANGE.permille_range(ODD_DISPLAY, None), (610, 910));
    let config = DynResConfig {
        cooldown_frames: 0,
        window_frames: 1,
        ..DynResConfig::default()
    }
    .within_preferred(&ODD_RANGE, ODD_DISPLAY, mode);
    assert_eq!((config.min_permille, config.max_permille), (529, 788));

    // Overload, then light load: the size stays inside on both axes and reaches both ends.
    let mut controller = DynResController::new(config, UpscaleQuality::Quality.permille());
    assert_eq!(controller.permille(), 667);
    let mut sizes = vec![ODD_RANGE.frame_extent(ODD_DISPLAY, controller.permille(), mode)];
    assert_eq!(sizes[0], ODD_RANGE.preferred);
    for _ in 0..12 {
        let permille = controller.update(40.0, 42.0);
        sizes.push(ODD_RANGE.frame_extent(ODD_DISPLAY, permille, mode));
    }
    // 529 ‰ is the lowest value whose size is in range on both axes; it rounds to 611 × 305.
    assert_eq!(controller.permille(), 529);
    assert_eq!(sizes.last().copied(), Some((611, 305)));
    for _ in 0..24 {
        let permille = controller.update(4.0, 40.0);
        sizes.push(ODD_RANGE.frame_extent(ODD_DISPLAY, permille, mode));
    }
    assert_eq!(controller.permille(), 788);
    assert_eq!(sizes.last().copied(), Some((910, 455)));
    for size in &sizes {
        assert_eq!(ODD_RANGE.clamp(*size), *size, "{size:?}");
    }

    // A static range holds one value under load and under light load.
    let fixed = PreferredRange {
        preferred: (770, 385),
        min: (770, 385),
        max: (770, 385),
    };
    assert!(fixed.is_static());
    let config = DynResConfig {
        cooldown_frames: 0,
        window_frames: 1,
        ..DynResConfig::default()
    }
    .within_preferred(&fixed, ODD_DISPLAY, mode);
    assert_eq!((config.min_permille, config.max_permille), (667, 667));
    let mut controller = DynResController::new(config, 1000);
    for (gpu, frame) in [(40.0, 42.0), (4.0, 40.0)] {
        for _ in 0..4 {
            assert_eq!(controller.update(gpu, frame), 667);
        }
    }
    // With no mode, per-axis rounding that leaves no common value collapses to the top.
    let awkward = PreferredRange {
        preferred: (500, 250),
        min: (500, 250),
        max: (500, 250),
    };
    assert_eq!(awkward.permille_range((1000, 501), None), (499, 499));
    // A preferred size smaller than the mode's: the top never goes above native.
    let small = PreferredRange {
        preferred: (300, 150),
        min: (300, 150),
        max: (1000, 500),
    };
    assert_eq!(small.permille_range(ODD_DISPLAY, mode), (667, 1000));
    // The built-in range rule is untouched: this is a separate call.
    assert_eq!(
        DynResConfig::default()
            .within_quality(UpscaleQuality::Quality)
            .max_permille,
        667
    );
}

// ---------------------------------------------------------------- conformance checklist (ADR-2679)

mod conformance {
    use super::super::scripted::{CHECKLIST, ExpectedFrame, RecordedEval, RecordedImage, check_eval};
    use super::*;

    const RENDER: (u32, u32) = (640, 360);
    const SHOWN: (u32, u32) = (960, 540);

    fn image(size: (u32, u32), format: wgpu::TextureFormat) -> RecordedImage {
        RecordedImage {
            size,
            format,
            samples: 1,
        }
    }

    fn expected() -> ExpectedFrame {
        ExpectedFrame {
            render: RENDER,
            display: SHOWN,
            jitter_ndc: [2.0 * 0.25 / 640.0, 2.0 * 0.125 / 360.0],
            exposure: 0.5,
            reset: false,
            reactive: true,
        }
    }

    /// What a correct frame hands a correct binding, in numbers written by hand: a quarter pixel
    /// right and an eighth up is +0.25 and −0.125 render pixels.
    fn good() -> RecordedEval {
        let e = expected();
        RecordedEval {
            feature: FeatureHandle(1),
            contract: InputContract::VENDOR,
            flags: FeatureFlags::REGIA,
            quality: UpscaleQuality::Quality,
            params: UpscaleParams {
                jitter: e.jitter_ndc,
                exposure: e.exposure,
                ..UpscaleParams::new(RENDER, SHOWN)
            },
            eval: VendorEvalParams {
                jitter: [0.25, -0.125],
                motion_scale: [640.0, 360.0],
                render_size: RENDER,
                reset: false,
                exposure: e.exposure,
                history_scale: 1.0,
                frame_time_ms: 16.667,
                sharpness: 0.0,
            },
            color: image(RENDER, wgpu::TextureFormat::Rgba16Float),
            depth: Some(image(RENDER, wgpu::TextureFormat::Depth32Float)),
            motion: Some(image(RENDER, wgpu::TextureFormat::Rg16Float)),
            reactive: Some(image(RENDER, wgpu::TextureFormat::R8Unorm)),
            transparency: None,
            output: image(SHOWN, wgpu::TextureFormat::Rgba16Float),
            ok: true,
        }
    }

    /// A checklist item and a way to break it.
    type Mistake = (&'static str, fn(&mut RecordedEval));

    /// Each list item fails on the mistake it exists for, and only that item fails.
    #[test]
    fn each_item_of_the_checklist_catches_its_own_mistake() {
        assert_eq!(check_eval(&good(), &expected()), vec![]);
        let cases: [Mistake; 16] = [
            ("render_size", |r| r.eval.render_size = (640, 359)),
            ("render_size", |r| r.color.size = (320, 180)),
            ("display_size", |r| r.params.display_size = (961, 540)),
            ("display_size", |r| r.output.size = (640, 360)),
            // NDC handed over as pixels.
            ("jitter_units", |r| r.eval.jitter = r.params.jitter),
            // The flipped sign on both axes.
            ("jitter_sign", |r| r.eval.jitter = [-0.25, 0.125]),
            // The y axis not flipped: the classic mistake.
            ("jitter_sign", |r| r.eval.jitter = [0.25, 0.125]),
            // UV motion handed over without a scale, or against the direction.
            ("motion_scale", |r| r.eval.motion_scale = [1.0, 1.0]),
            ("motion_scale", |r| r.eval.motion_scale = [-640.0, -360.0]),
            ("motion_scale", |r| r.flags.motion_vectors_jittered = true),
            ("exposure", |r| r.eval.exposure = 1.0),
            ("exposure", |r| r.flags.auto_exposure = true),
            ("reset", |r| r.eval.reset = true),
            ("depth_convention", |r| r.flags.depth_inverted = true),
            ("reactive_mask", |r| r.reactive = None),
            ("transparency_mask", |r| {
                r.transparency = Some(image(RENDER, wgpu::TextureFormat::R8Unorm));
            }),
        ];
        for (item, break_it) in cases {
            assert!(CHECKLIST.contains(&item), "{item}");
            let mut recorded = good();
            break_it(&mut recorded);
            let items: Vec<&str> = check_eval(&recorded, &expected())
                .iter()
                .map(|violation| violation.item)
                .collect();
            // A wrong jitter is wrong against the conversion and against the numbers by hand.
            let wanted: Vec<&str> = if item.starts_with("jitter") {
                vec!["jitter_units", "jitter_sign"]
            } else {
                vec![item]
            };
            assert_eq!(items, wanted, "{item}");
        }
        // The mask must be the render size, in the one format the runtimes read, and absent when
        // the frame has none.
        let mut small = good();
        small.reactive = Some(image((320, 180), wgpu::TextureFormat::R8Unorm));
        assert_eq!(check_eval(&small, &expected())[0].item, "reactive_mask");
        let mut wrong = good();
        wrong.reactive = Some(image(RENDER, wgpu::TextureFormat::Rgba8Unorm));
        assert_eq!(check_eval(&wrong, &expected())[0].item, "reactive_mask");
        let none = ExpectedFrame {
            reactive: false,
            ..expected()
        };
        assert_eq!(check_eval(&good(), &none)[0].item, "reactive_mask");
    }

    /// The recorded form hides the feature handle only: two slots compare on their inputs.
    #[test]
    fn inputs_compare_apart_from_the_feature_handle() {
        let a = good();
        let mut b = good();
        b.feature = FeatureHandle(9);
        assert_ne!(a, b);
        assert_eq!(
            super::super::scripted::inputs_of(&a),
            super::super::scripted::inputs_of(&b)
        );
        b.eval.reset = true;
        assert_ne!(
            super::super::scripted::inputs_of(&a),
            super::super::scripted::inputs_of(&b)
        );
    }
}
