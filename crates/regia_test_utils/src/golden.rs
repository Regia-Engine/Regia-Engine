// SPDX-License-Identifier: MPL-2.0
//! Golden-image comparison (`docs/06-testing-and-ci.md` § 2).

use anyhow::{Context, Result};
use image::{ImageBuffer, Rgba};
use std::path::{Path, PathBuf};

/// Default SSIM threshold for renderer goldens.
pub const DEFAULT_SSIM_THRESHOLD: f64 = 0.98;

/// Scene classes whose goldens may use a looser SSIM threshold **on hardware GPUs**, as
/// decided in ADR-025 (`docs/06-testing-and-ci.md` § 2): exactly the scenes whose pixels are
/// dominated by vendor-variant rasterization or texture-filtering behaviour, not by engine
/// math. The lavapipe (and WARP) reference keeps [`DEFAULT_SSIM_THRESHOLD`] for every scene —
/// the software threshold is never lowered.
///
/// Measured deltas behind the entries (RTX 5080 vs the lavapipe-blessed goldens,
/// 2026-10-05): `alpha_modes` 0.9675 (alpha-test edge coverage on the checkerboard cutout),
/// `ibl_spheres` 0.9764 (hardware filtering of the CPU-built prefiltered mip chain).
pub const HARDWARE_TOLERANT_SCENES: &[(&str, f64)] = &[("alpha_modes", 0.95), ("ibl_spheres", 0.95)];

/// The SSIM threshold a golden compare uses for scene `name` on this GPU class.
///
/// `hardware_gpu` is `true` for discrete/integrated/vertical adapters and `false` for software
/// rasterizers (lavapipe, WARP — `wgpu::DeviceType::Cpu`), which bless and gate the reference
/// goldens. Software keeps the default threshold for every scene; hardware keeps it for every
/// scene except the [`HARDWARE_TOLERANT_SCENES`] entries.
#[must_use]
pub fn ssim_threshold_for(name: &str, hardware_gpu: bool) -> f64 {
    if hardware_gpu {
        HARDWARE_TOLERANT_SCENES
            .iter()
            .find(|(scene, _)| *scene == name)
            .map_or(DEFAULT_SSIM_THRESHOLD, |(_, threshold)| *threshold)
    } else {
        DEFAULT_SSIM_THRESHOLD
    }
}

/// Environment variable of the real-GPU **non-golden** pass (`fgate.sh`'s RTX 5090 pass,
/// ADR "farm gate goldens"): with `1`, golden compares against the lavapipe-blessed PNGs are
/// skipped with a note; guards and every other assertion of the test still run. Only a run whose
/// device selection is a strict hardware GPU sets it; CI and lavapipe runs never do.
pub const SKIP_GOLDENS_ENV: &str = "REGIA_SKIP_GOLDENS";

/// `true` when [`SKIP_GOLDENS_ENV`] is `1`.
#[must_use]
pub fn goldens_skipped() -> bool {
    regia_core::brand::env_var(SKIP_GOLDENS_ENV).as_deref() == Ok("1")
}

/// Environment variable naming a directory where every golden compare records the exact hash of
/// the frame it was given (ADR-1552, the renderer modularization's bit-identity proof). A run of
/// the golden suite before and after a refactor with this set gives two directories whose
/// contents must match byte for byte; SSIM thresholds cannot show that. Unset: nothing is written.
pub const FRAME_HASH_DIR_ENV: &str = "REGIA_FRAME_HASH_DIR";

/// FNV-1a 64 over the frame size and its RGBA8 bytes: the hash [`record_frame_hash`] writes.
#[must_use]
pub fn frame_hash(actual: &[u8], width: u32, height: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in width
        .to_le_bytes()
        .iter()
        .chain(height.to_le_bytes().iter())
        .chain(actual.iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// When [`FRAME_HASH_DIR_ENV`] names a directory, writes `<name>.hash` there (atomically) holding
/// `<name> <width>x<height> <hash>`; a later compare under the same name overwrites it. `name` is
/// sanitized to ascii letters, digits, `-` and `_`. Errors are printed, never returned: the hash
/// is evidence for a reviewer, not a test result.
pub fn record_frame_hash(name: &str, actual: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var(FRAME_HASH_DIR_ENV) else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let line = format!(
        "{safe} {width}x{height} {:016x}\n",
        frame_hash(actual, width, height)
    );
    let dir = PathBuf::from(dir);
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| crate::temp::write_atomic(&dir.join(format!("{safe}.hash")), line.as_bytes()));
    if let Err(err) = result {
        eprintln!("frame hash {safe}: not recorded in {}: {err}", dir.display());
    }
}

/// Environment variable naming a directory where every golden compare records a *plate*: the frame
/// as `<plate>.png` and its PSNR against the blessed image as `<plate>.plate` (ADR-2089). It works
/// under [`SKIP_GOLDENS_ENV`] too, so the real-GPU pass that skips the SSIM compare still measures
/// every golden. `cargo xtask plate-floors check` holds the results to the per-vendor floors in
/// `docs/ci/plate-floors.toml`. Unset: nothing is written and no golden is loaded for this.
pub const PLATE_DIR_ENV: &str = "REGIA_PLATE_DIR";

/// PSNR in dB over the RGB channels of two equal-sized RGBA8 frames against a peak of 255; alpha is
/// ignored, as a frame's alpha is not what a person sees. `f64::INFINITY` when RGB is identical,
/// `None` when the lengths differ, are not whole pixels, or the frames are empty. Definition:
/// `10 log10(255^2 / MSE)` (a textbook measure, `docs/provenance.md`).
#[must_use]
pub fn psnr_rgb8(actual: &[u8], golden: &[u8]) -> Option<f64> {
    if actual.len() != golden.len() || actual.is_empty() || !actual.len().is_multiple_of(4) {
        return None;
    }
    let mut sum: u64 = 0;
    for (a, g) in actual.as_chunks::<4>().0.iter().zip(golden.as_chunks::<4>().0) {
        for channel in 0..3 {
            let d = i64::from(a[channel]) - i64::from(g[channel]);
            sum += d.unsigned_abs() * d.unsigned_abs();
        }
    }
    if sum == 0 {
        return Some(f64::INFINITY);
    }
    let mse = sum as f64 / (actual.len() / 4 * 3) as f64;
    Some(10.0 * (255.0 * 255.0 / mse).log10())
}

/// The `<plate>.plate` line: `<plate> <width>x<height> <psnr dB or inf or size_differs> <golden>`.
/// `cargo xtask plate-floors` parses this line; change both together.
#[must_use]
pub fn plate_line(plate: &str, width: u32, height: u32, psnr: Option<f64>, golden: &str) -> String {
    let score = match psnr {
        Some(value) if value.is_infinite() => "inf".to_string(),
        Some(value) => format!("{value:.3}"),
        None => "size_differs".to_string(),
    };
    format!("{plate} {width}x{height} {score} {golden}\n")
}

/// Keeps ascii letters, digits, `-` and `_`; anything else becomes `_`.
fn safe_file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Writes one plate into `dir`: the `.plate` line, then the PNG. Both go through a temp name and a
/// rename, as test binaries share the directory.
fn write_plate(
    dir: &Path,
    plate: &str,
    actual: &[u8],
    (width, height): (u32, u32),
    psnr: Option<f64>,
    golden: &str,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{plate}.{}.tmp.png", std::process::id()));
    image::save_buffer(&tmp, actual, width, height, image::ColorType::Rgba8)?;
    std::fs::rename(&tmp, dir.join(format!("{plate}.png")))?;
    crate::temp::write_atomic(
        &dir.join(format!("{plate}.plate")),
        plate_line(plate, width, height, psnr, golden).as_bytes(),
    )?;
    Ok(())
}

/// The workspace-relative spelling of a blessed path for the `.plate` line. Spaces become `_` so
/// the line stays four fields.
fn golden_label(path: &Path) -> String {
    let text = path.to_string_lossy();
    let relative = ["/crates/", "/samples/", "/docs/"]
        .iter()
        .find_map(|root| text.rfind(root).map(|at| &text[at + 1..]))
        .unwrap_or(&text);
    relative.replace(' ', "_")
}

/// A golden under the temp directory is a helper's own unit test, not a plate.
fn is_plate_golden(blessed_path: &Path) -> bool {
    !blessed_path.starts_with(std::env::temp_dir())
}

/// When [`PLATE_DIR_ENV`] names a directory and `blessed_path` exists, records this frame's plate.
/// Errors are printed, never returned: the floor check notices a plate that is missing.
fn record_png_plate(actual: &[u8], width: u32, height: u32, blessed_path: &Path) {
    let Ok(dir) = regia_core::brand::env_var(PLATE_DIR_ENV) else {
        return;
    };
    if dir.is_empty() || !is_plate_golden(blessed_path) {
        return;
    }
    let plate = safe_file_stem(&golden_hash_name(blessed_path));
    let golden = match image::open(blessed_path) {
        Ok(image) => image.to_rgba8(),
        Err(err) => {
            eprintln!(
                "plate {plate}: golden {} unreadable: {err}",
                blessed_path.display()
            );
            return;
        }
    };
    let psnr = (golden.dimensions() == (width, height))
        .then(|| psnr_rgb8(actual, golden.as_raw()))
        .flatten();
    let written = write_plate(
        Path::new(&dir),
        &plate,
        actual,
        (width, height),
        psnr,
        &golden_label(blessed_path),
    );
    if let Err(err) = written {
        eprintln!("plate {plate}: not recorded in {dir}: {err}");
    }
}

/// [`record_png_plate`] for a pixel golden, whose test holds the golden's bytes.
fn record_pixel_plate(name: &str, actual: &[u8], golden: &[u8], width: u32, height: u32) {
    let Ok(dir) = regia_core::brand::env_var(PLATE_DIR_ENV) else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let plate = safe_file_stem(&format!("pixel-{name}"));
    let written = write_plate(
        Path::new(&dir),
        &plate,
        actual,
        (width, height),
        psnr_rgb8(actual, golden),
        &format!("pixel:{}", safe_file_stem(name)),
    );
    if let Err(err) = written {
        eprintln!("plate {plate}: not recorded in {dir}: {err}");
    }
}

/// The [`record_frame_hash`] name of a blessed PNG: its crate directory and file stem, so goldens
/// of the same stem in two crates do not overwrite each other.
fn golden_hash_name(blessed_path: &Path) -> String {
    let stem = blessed_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("golden");
    let owner = blessed_path
        .ancestors()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .find(|s| *s != "goldens" && Some(*s) != blessed_path.file_name().and_then(|f| f.to_str()))
        .unwrap_or("golden");
    format!("{owner}-{stem}")
}

/// Compares `actual` RGBA8 pixels with a blessed PNG on disk.
///
/// When `REGIA_BLESS=1`, writes `actual` to `blessed_path` instead of comparing. When
/// [`goldens_skipped`], returns `Ok` without comparing (the real-GPU non-golden pass). Every call
/// first records the frame's hash under [`FRAME_HASH_DIR_ENV`] and its plate under
/// [`PLATE_DIR_ENV`] when those are set.
pub fn compare_rgba8_to_png(
    actual: &[u8],
    width: u32,
    height: u32,
    blessed_path: &Path,
    ssim_threshold: f64,
) -> Result<()> {
    record_frame_hash(&golden_hash_name(blessed_path), actual, width, height);
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() != Ok("1") {
        record_png_plate(actual, width, height, blessed_path);
    }
    if goldens_skipped() && regia_core::brand::env_var("REGIA_BLESS").as_deref() != Ok("1") {
        eprintln!(
            "golden {} not compared ({SKIP_GOLDENS_ENV}=1: real-GPU non-golden pass)",
            blessed_path.display()
        );
        return Ok(());
    }
    if regia_core::brand::env_var("REGIA_BLESS").as_deref() == Ok("1") {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_raw(width, height, actual.to_vec()).context("invalid image dimensions")?;
        if let Some(parent) = blessed_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        img.save(blessed_path)
            .with_context(|| format!("write blessed {}", blessed_path.display()))?;
        return Ok(());
    }

    let expected = image::open(blessed_path)
        .with_context(|| format!("read blessed image {}", blessed_path.display()))?
        .to_rgba8();

    let actual_img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, actual.to_vec()).context("invalid image dimensions")?;

    if expected.dimensions() != actual_img.dimensions() {
        write_failure_artifacts(&actual_img, &expected, blessed_path)?;
        anyhow::bail!(
            "dimension mismatch: expected {}x{}, got {}x{}",
            expected.width(),
            expected.height(),
            width,
            height
        );
    }

    let result = image_compare::rgba_hybrid_compare(&expected, &actual_img)
        .map_err(|e| anyhow::anyhow!("compare failed: {e}"))?;

    if result.score < ssim_threshold {
        write_failure_artifacts(&actual_img, &expected, blessed_path)?;
        anyhow::bail!("SSIM {:.4} below threshold {:.4}", result.score, ssim_threshold);
    }
    Ok(())
}

/// SSIM floor for a lavapipe-blessed golden compared on a hardware GPU by
/// [`compare_rgba8_to_png_on`]. Measured hardware deltas (RTX 5090, 2026-10-20) are 0.89–0.98;
/// the floor sits below them and still fails a blank, wrong-scene or broken frame. The lavapipe
/// threshold is never lowered.
pub const HARDWARE_SANITY_SSIM: f64 = 0.75;

/// The SSIM threshold [`compare_rgba8_to_png_on`] applies for `info`: `threshold` unchanged on
/// software rasterizers, at most [`HARDWARE_SANITY_SSIM`] on hardware GPUs.
#[must_use]
pub fn adapter_ssim_threshold(info: &wgpu::AdapterInfo, threshold: f64) -> f64 {
    if golden_icd(info) == GoldenIcd::Hardware {
        threshold.min(HARDWARE_SANITY_SSIM)
    } else {
        threshold
    }
}

/// [`compare_rgba8_to_png`] that knows the adapter: lavapipe/WARP keep `ssim_threshold`
/// exactly; a hardware GPU is held to the [`HARDWARE_SANITY_SSIM`] floor instead (the goldens are
/// lavapipe-blessed, ADR-025), with the reason logged. The test's own look assertions still run.
pub fn compare_rgba8_to_png_on(
    info: &wgpu::AdapterInfo,
    actual: &[u8],
    width: u32,
    height: u32,
    blessed_path: &Path,
    ssim_threshold: f64,
) -> Result<()> {
    let threshold = adapter_ssim_threshold(info, ssim_threshold);
    if threshold < ssim_threshold && !goldens_skipped() {
        eprintln!(
            "golden {} on {} (hardware): lavapipe-blessed, compared at the SSIM sanity floor {threshold} instead of {ssim_threshold}",
            blessed_path.display(),
            info.name
        );
    }
    compare_rgba8_to_png(actual, width, height, blessed_path, threshold)
}

/// Writes `<stem>_actual.png` and `<stem>_diff.png` under `target/golden-failures/`.
fn write_failure_artifacts(
    actual: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    expected: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    blessed_path: &Path,
) -> Result<()> {
    let stem = blessed_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("golden");
    let dir = PathBuf::from("target/golden-failures");
    std::fs::create_dir_all(&dir)?;
    let actual_path = dir.join(format!("{stem}_actual.png"));
    let diff_path = dir.join(format!("{stem}_diff.png"));
    // Written through a per-process temp name and renamed, so concurrent test passes sharing this
    // directory never leave a truncated artifact.
    let pid = std::process::id();
    let actual_tmp = dir.join(format!("{stem}_actual.{pid}.tmp.png"));
    actual.save(&actual_tmp)?;
    std::fs::rename(&actual_tmp, &actual_path)?;
    if let Ok(result) = image_compare::rgba_hybrid_compare(expected, actual) {
        let diff_img = result.image.to_color_map();
        let diff_tmp = dir.join(format!("{stem}_diff.{pid}.tmp.png"));
        diff_img.save(&diff_tmp)?;
        std::fs::rename(&diff_tmp, &diff_path)?;
    }
    Ok(())
}

/// Guards against blessing a broken frame: too dark, blown out, or nearly empty.
///
/// A golden that fails the guard must not be blessed. The bands are per-scene
/// (see the golden tests): a black or empty frame can never pass them.
///
/// Measured and printed on every call:
/// - mean luminance (BT.709, 0–255) must be within `[min_lum, max_lum]`;
/// - near-white pixels (lum ≥ 245) must be `< max_white_pct` % of the frame;
/// - near-black pixels (lum ≤ 8) must be `< max_black_pct` % of the frame
///   (unless the scene is genuinely dark — then the band says so);
/// - object pixels (any channel > 5, i.e. not the 0.02-linear clear colour)
///   must cover `≥ min_object_pct` % of the frame.
pub fn guard_golden(pixels: &[u8], width: u32, height: u32, name: &str, guard: GoldenGuard) {
    let stats = measure_golden(pixels, width, height);
    eprintln!(
        "golden {name}: mean lum {:+.1} (band {}..{}), near-white {:.2}% (<{:.1}%), \
         near-black {:.2}% (<{:.1}%), object {:.2}% (>={:.1}%)",
        stats.mean_lum,
        guard.min_lum,
        guard.max_lum,
        stats.near_white_pct,
        guard.max_white_pct,
        stats.near_black_pct,
        guard.max_black_pct,
        stats.object_pct,
        guard.min_object_pct,
    );
    assert!(
        stats.mean_lum >= guard.min_lum && stats.mean_lum <= guard.max_lum,
        "golden {name}: mean luminance {:.1} outside [{}, {}] — frame is too dark or blown out",
        stats.mean_lum,
        guard.min_lum,
        guard.max_lum,
    );
    assert!(
        stats.near_white_pct < guard.max_white_pct,
        "golden {name}: {:.2}% near-white pixels (limit {:.1}%) — frame is blown out",
        stats.near_white_pct,
        guard.max_white_pct,
    );
    assert!(
        stats.near_black_pct < guard.max_black_pct,
        "golden {name}: {:.2}% near-black pixels (limit {:.1}%) — frame is mostly black",
        stats.near_black_pct,
        guard.max_black_pct,
    );
    assert!(
        stats.object_pct >= guard.min_object_pct,
        "golden {name}: only {:.2}% object pixels (need ≥ {:.1}%) — frame is mostly empty",
        stats.object_pct,
        guard.min_object_pct,
    );
}

/// Per-scene luminance bands for [`guard_golden`].
#[derive(Clone, Copy, Debug)]
pub struct GoldenGuard {
    /// Allowed mean-luminance band (0–255), e.g. 40..=200 for a lit scene.
    pub min_lum: f64,
    /// Upper end of the mean-luminance band.
    pub max_lum: f64,
    /// Maximum share of pixels with luminance ≥ 245 (blown out), in %.
    pub max_white_pct: f64,
    /// Maximum share of pixels with luminance ≤ 8 (black), in %.
    pub max_black_pct: f64,
    /// Minimum share of object pixels (any channel > 8), in %.
    pub min_object_pct: f64,
}

/// Measured luminance statistics of a rendered frame (BT.709, 0–255 scale).
#[derive(Clone, Copy, Debug)]
pub struct GoldenStats {
    /// Mean luminance of the frame, 0–255.
    pub mean_lum: f64,
    /// Share of pixels with luminance ≥ 245, in %.
    pub near_white_pct: f64,
    /// Share of pixels with luminance ≤ 8, in %.
    pub near_black_pct: f64,
    /// Share of pixels with any channel > 8 (not the clear colour), in %.
    pub object_pct: f64,
}

/// Measures a rendered RGBA8 frame.
///
/// Never describes an image it has not measured: the golden tests print these
/// numbers on every run and they go into the lane report.
#[must_use]
pub fn measure_golden(pixels: &[u8], width: u32, height: u32) -> GoldenStats {
    assert_eq!(
        pixels.len(),
        (width * height * 4) as usize,
        "measure_golden: wrong pixel count"
    );
    let count = (width * height) as usize;
    let mut total_lum = 0.0f64;
    let mut near_white = 0usize;
    let mut near_black = 0usize;
    let mut object = 0usize;
    for chunk in pixels.as_chunks::<4>().0 {
        let r = f64::from(chunk[0]);
        let g = f64::from(chunk[1]);
        let b = f64::from(chunk[2]);
        let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        total_lum += lum;
        if lum >= 245.0 {
            near_white += 1;
        }
        if lum <= 8.0 {
            near_black += 1;
        }
        // Above the lab clear colour (0.02 linear → (5, 5, 6) in sRGB): the
        // blue channel rounds to 6, so a threshold of 5 would count background.
        if chunk[0] > 8 || chunk[1] > 8 || chunk[2] > 8 {
            object += 1;
        }
    }
    let pct = |n: usize| n as f64 / count as f64 * 100.0;
    GoldenStats {
        mean_lum: total_lum / count as f64,
        near_white_pct: pct(near_white),
        near_black_pct: pct(near_black),
        object_pct: pct(object),
    }
}

/// Structural similarity of two tightly packed RGBA8 images (1 = identical).
pub fn ssim_rgba8(left: &[u8], right: &[u8], width: u32, height: u32) -> Result<f64> {
    let left_img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, left.to_vec()).context("invalid left image")?;
    let right_img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, right.to_vec()).context("invalid right image")?;
    let result = image_compare::rgba_hybrid_compare(&left_img, &right_img)
        .map_err(|err| anyhow::anyhow!("compare failed: {err}"))?;
    Ok(result.score)
}

/// LLVM major of the lavapipe build that blesses the pixel-exact goldens (the owner's the workstation:
/// Mesa 26.2.4, LLVM 23.1.1). Goldens stay single-reference (ADR-025); other lavapipe builds
/// compare against the same PNGs through [`LAVAPIPE_ICD_TOLERANCES`].
pub const REFERENCE_LAVAPIPE_LLVM_MAJOR: u32 = 23;

/// How far a pixel-exact lavapipe golden may drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelTolerance {
    /// Largest per-channel difference allowed on every pixel.
    pub max_channel_diff: u8,
    /// Pixels allowed beyond `max_channel_diff`, in parts per million of the frame (edge pixels
    /// whose coverage flips between LLVM code generators). The reference build allows none.
    pub outlier_ppm: u32,
}

impl PixelTolerance {
    /// Outlier pixels allowed in a `pixels`-pixel frame (rounded up).
    #[must_use]
    pub fn allowed_outliers(&self, pixels: usize) -> usize {
        (pixels as u64 * u64::from(self.outlier_ppm)).div_ceil(1_000_000) as usize
    }
}

/// Lavapipe builds other than the reference, keyed by LLVM major, with the outlier allowance
/// (ppm of the frame) their measured delta needs (ADR "farm gate goldens",
/// `docs/06-testing-and-ci.md` § 2). The per-channel bound stays the test's own; only the
/// outlier allowance grows.
///
/// - LLVM 19 (the build server farm, Mesa 26.1.6) against the LLVM 23 goldens, 2026-10-07 (main at
///   0f57be2): `terrain_fog_on_noon` 2 px of 360 000 beyond ±2 (ridge silhouette, max 20; 5.6 ppm),
///   `snow_setpiece` 7 px of 518 400 beyond ±1 (max 8; 13.5 ppm), `mars_setpiece` 4 px
///   (one crate-edge pixel at 50; 7.7 ppm); all other channels within ±1. Allowance 30 ppm
///   (about 2× the worst measured).
pub const LAVAPIPE_ICD_TOLERANCES: &[(u32, u32)] = &[(19, 30)];

/// Which golden reference an adapter can be held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoldenIcd {
    /// Mesa lavapipe (llvmpipe); `None` when the driver string carries no LLVM version.
    Lavapipe {
        /// LLVM major parsed from the driver info (`Mesa 26.2.4 (LLVM 23.1.1)`).
        llvm_major: Option<u32>,
    },
    /// Another software rasterizer (WARP).
    OtherSoftware,
    /// A hardware GPU (discrete, integrated, virtual).
    Hardware,
}

/// Classifies an adapter for golden compares.
#[must_use]
pub fn golden_icd(info: &wgpu::AdapterInfo) -> GoldenIcd {
    if info.device_type != wgpu::DeviceType::Cpu {
        return GoldenIcd::Hardware;
    }
    let lavapipe = info.name.contains("llvmpipe") || info.driver.contains("llvmpipe");
    if !lavapipe {
        return GoldenIcd::OtherSoftware;
    }
    GoldenIcd::Lavapipe {
        llvm_major: llvm_major(&info.driver_info).or_else(|| llvm_major(&info.name)),
    }
}

/// The major version after `LLVM ` in a Mesa driver string.
fn llvm_major(text: &str) -> Option<u32> {
    let rest = &text[text.find("LLVM ")? + 5..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// What a pixel-exact lavapipe golden compare does on this adapter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PixelGolden {
    /// Compare channel by channel with this tolerance.
    Pixels(PixelTolerance),
    /// Hardware GPU: the lavapipe pixel golden is held to the ADR-025 SSIM threshold instead.
    Ssim(f64),
}

/// The compare a pixel-exact lavapipe golden named `name` uses on `info`, given the
/// tolerance the test sets for the reference build.
///
/// The reference LLVM, WARP and unknown lavapipe builds use `reference` unchanged (an unknown
/// build fails until its delta is measured and added to [`LAVAPIPE_ICD_TOLERANCES`]); a listed
/// build keeps the per-channel bound and gets its outlier allowance; hardware GPUs use
/// [`ssim_threshold_for`].
#[must_use]
pub fn pixel_golden_for(info: &wgpu::AdapterInfo, name: &str, reference: PixelTolerance) -> PixelGolden {
    match golden_icd(info) {
        GoldenIcd::Hardware => PixelGolden::Ssim(ssim_threshold_for(name, true)),
        GoldenIcd::Lavapipe {
            llvm_major: Some(major),
        } if major != REFERENCE_LAVAPIPE_LLVM_MAJOR => LAVAPIPE_ICD_TOLERANCES
            .iter()
            .find(|(llvm, _)| *llvm == major)
            .map_or(PixelGolden::Pixels(reference), |(_, ppm)| {
                PixelGolden::Pixels(PixelTolerance {
                    outlier_ppm: reference.outlier_ppm.max(*ppm),
                    ..reference
                })
            }),
        _ => PixelGolden::Pixels(reference),
    }
}

/// Largest channel difference and the number of pixels beyond `max_channel_diff`
/// (RGBA8, alpha included).
#[must_use]
pub fn pixel_delta(actual: &[u8], golden: &[u8], max_channel_diff: u8) -> (u8, usize) {
    let mut max = 0;
    let mut outliers = 0;
    let (actual_px, _) = actual.as_chunks::<4>();
    let (golden_px, _) = golden.as_chunks::<4>();
    for (a, g) in actual_px.iter().zip(golden_px) {
        let worst = a.iter().zip(g).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(worst);
        if worst > max_channel_diff {
            outliers += 1;
        }
    }
    (max, outliers)
}

/// Compares `actual` with a lavapipe pixel golden under [`pixel_golden_for`] and returns a
/// one-line verdict; `Err` names the delta, the adapter and the tolerance that applied. On a
/// hardware GPU with [`goldens_skipped`] nothing is compared.
pub fn check_pixel_golden(
    info: &wgpu::AdapterInfo,
    name: &str,
    actual: &[u8],
    golden: &[u8],
    width: u32,
    height: u32,
    reference: PixelTolerance,
) -> Result<String, String> {
    record_frame_hash(&format!("pixel-{name}"), actual, width, height);
    record_pixel_plate(name, actual, golden, width, height);
    if actual.len() != golden.len() {
        return Err(format!(
            "{name}: size {} != golden {}",
            actual.len(),
            golden.len()
        ));
    }
    let adapter = format!("{} / {}", info.name, info.driver_info);
    if goldens_skipped() && golden_icd(info) == GoldenIcd::Hardware {
        return Ok(format!(
            "{name} on {adapter}: not compared ({SKIP_GOLDENS_ENV}=1: real-GPU non-golden pass)"
        ));
    }
    match pixel_golden_for(info, name, reference) {
        PixelGolden::Pixels(tol) => {
            let (max, outliers) = pixel_delta(actual, golden, tol.max_channel_diff);
            let allowed = tol.allowed_outliers(actual.len() / 4);
            let verdict = format!(
                "{name} on {adapter}: max channel diff {max}, {outliers} px beyond ±{} (allowed {allowed})",
                tol.max_channel_diff
            );
            if outliers <= allowed {
                Ok(verdict)
            } else {
                Err(verdict)
            }
        }
        PixelGolden::Ssim(threshold) => {
            let score = ssim_rgba8(actual, golden, width, height).map_err(|err| format!("{name}: {err}"))?;
            let verdict = format!(
                "{name} on {adapter} (hardware): SSIM {score:.4} vs lavapipe golden (min {threshold})"
            );
            if score >= threshold {
                Ok(verdict)
            } else {
                Err(verdict)
            }
        }
    }
}

/// Path to a golden PNG in this crate.
#[must_use]
pub fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{name}.png"))
}

/// The RGBA8 pixels and size of the PNG at `path` (a blessed golden), or `None` when it is missing or
/// does not decode. For tests that compare a frame with a golden themselves (ADR-4762).
#[must_use]
pub fn read_png_rgba8(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let image = image::open(path).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some((image.into_raw(), width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(device_type: wgpu::DeviceType, name: &str, driver_info: &str) -> wgpu::AdapterInfo {
        let mut info = wgpu::AdapterInfo::new(device_type, wgpu::Backend::Vulkan);
        info.name = name.to_string();
        info.driver = "llvmpipe".to_string();
        info.driver_info = driver_info.to_string();
        info
    }

    #[test]
    fn psnr_matches_a_hand_computed_case() {
        let golden = [10u8, 20, 30, 255, 40, 50, 60, 255];
        assert_eq!(psnr_rgb8(&golden, &golden), Some(f64::INFINITY));
        // One red channel off by 3 in six channels: MSE 9/6 = 1.5.
        let actual = [13u8, 20, 30, 255, 40, 50, 60, 255];
        let expected = 10.0 * (255.0f64 * 255.0 / 1.5).log10();
        let measured = psnr_rgb8(&actual, &golden).unwrap_or(f64::NAN);
        assert!((measured - expected).abs() < 1e-9, "{measured} vs {expected}");
    }

    #[test]
    fn psnr_ignores_alpha_and_refuses_mismatched_frames() {
        let golden = [10u8, 20, 30, 255];
        assert_eq!(psnr_rgb8(&[10, 20, 30, 0], &golden), Some(f64::INFINITY));
        assert_eq!(psnr_rgb8(&[10, 20, 30, 255, 1, 1, 1, 1], &golden), None);
        assert_eq!(psnr_rgb8(&[], &[]), None);
        assert_eq!(psnr_rgb8(&[1, 2, 3], &[1, 2, 3]), None);
    }

    #[test]
    fn plate_line_has_four_fields_for_every_score() {
        for (psnr, text) in [
            (Some(41.25), "41.250"),
            (Some(f64::INFINITY), "inf"),
            (None, "size_differs"),
        ] {
            let line = plate_line("regia_x-shot", 8, 4, psnr, "crates/x/goldens/shot.png");
            let fields: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(fields, ["regia_x-shot", "8x4", text, "crates/x/goldens/shot.png"]);
        }
    }

    #[test]
    fn golden_label_is_workspace_relative_and_keeps_the_line_parseable() {
        assert_eq!(
            golden_label(Path::new("/srv/my tree/crates/regia_x/goldens/a.png")),
            "crates/regia_x/goldens/a.png"
        );
        assert_eq!(golden_label(Path::new("rel dir/a.png")), "rel_dir/a.png");
    }

    #[test]
    fn temp_goldens_are_not_plates() {
        assert!(!is_plate_golden(&std::env::temp_dir().join("x/goldens/a.png")));
        assert!(is_plate_golden(Path::new(
            "/nowhere-near-temp/crates/x/goldens/a.png"
        )));
    }

    #[test]
    fn write_plate_leaves_only_the_plate_and_png() -> Result<()> {
        let dir = crate::temp::unique_temp_dir("plate_write");
        let pixels = [10u8, 20, 30, 255];
        write_plate(&dir, "p", &pixels, (1, 1), Some(50.0), "crates/x/goldens/p.png")?;
        let mut names: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        assert_eq!(names, ["p.plate", "p.png"]);
        let line = std::fs::read_to_string(dir.join("p.plate"))?;
        assert_eq!(line, "p 1x1 50.000 crates/x/goldens/p.png\n");
        assert_eq!(image::open(dir.join("p.png"))?.to_rgba8().as_raw(), &pixels);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn frame_hash_sees_every_byte_and_the_size() {
        let a = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut b = a;
        b[7] ^= 1;
        assert_eq!(frame_hash(&a, 2, 1), frame_hash(&a, 2, 1));
        assert_ne!(frame_hash(&a, 2, 1), frame_hash(&b, 2, 1));
        // Same bytes, another shape: a transposed frame must not hash the same.
        assert_ne!(frame_hash(&a, 2, 1), frame_hash(&a, 1, 2));
    }

    #[test]
    fn golden_hash_names_carry_the_owning_directory() {
        assert_eq!(
            golden_hash_name(Path::new("/x/crates/regia_test_utils/goldens/fog.png")),
            "regia_test_utils-fog"
        );
        assert_eq!(
            golden_hash_name(Path::new("/x/crates/regia_quick/goldens/lens/fog.png")),
            "lens-fog"
        );
    }

    #[test]
    fn adapter_threshold_is_exact_on_software_and_a_floor_on_hardware() {
        let lavapipe = adapter(
            wgpu::DeviceType::Cpu,
            "llvmpipe (LLVM 23.1.1, 256 bits)",
            "Mesa (LLVM 23.1.1)",
        );
        let nvidia = adapter(wgpu::DeviceType::DiscreteGpu, "NVIDIA GeForce RTX 5090", "615.71");
        assert_eq!(
            adapter_ssim_threshold(&lavapipe, DEFAULT_SSIM_THRESHOLD),
            DEFAULT_SSIM_THRESHOLD
        );
        assert_eq!(
            adapter_ssim_threshold(&nvidia, DEFAULT_SSIM_THRESHOLD),
            HARDWARE_SANITY_SSIM
        );
        // A threshold already below the floor is never raised.
        assert_eq!(adapter_ssim_threshold(&nvidia, 0.5), 0.5);
    }

    const STRICT: PixelTolerance = PixelTolerance {
        max_channel_diff: 2,
        outlier_ppm: 0,
    };

    #[test]
    fn pixel_golden_policy_per_icd() {
        let reference = adapter(
            wgpu::DeviceType::Cpu,
            "llvmpipe (LLVM 23.1.1, 256 bits)",
            "Mesa 26.2.4 (LLVM 23.1.1)",
        );
        let farm = adapter(
            wgpu::DeviceType::Cpu,
            "llvmpipe (LLVM 19.1.7, 256 bits)",
            "Mesa 26.1.6-1~bpo13+1 (LLVM 19.1.7)",
        );
        let unknown = adapter(
            wgpu::DeviceType::Cpu,
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            "Mesa 24.0 (LLVM 17.0.6)",
        );
        let nvidia = adapter(wgpu::DeviceType::DiscreteGpu, "NVIDIA GeForce RTX 5090", "615.71");
        assert_eq!(golden_icd(&farm), GoldenIcd::Lavapipe { llvm_major: Some(19) });
        assert_eq!(
            pixel_golden_for(&reference, "fog", STRICT),
            PixelGolden::Pixels(STRICT)
        );
        assert_eq!(
            pixel_golden_for(&unknown, "fog", STRICT),
            PixelGolden::Pixels(STRICT)
        );
        assert_eq!(
            pixel_golden_for(&farm, "fog", STRICT),
            PixelGolden::Pixels(PixelTolerance {
                max_channel_diff: 2,
                outlier_ppm: 30
            })
        );
        assert_eq!(
            pixel_golden_for(&nvidia, "fog", STRICT),
            PixelGolden::Ssim(DEFAULT_SSIM_THRESHOLD)
        );
        // A listed build never widens the per-channel bound.
        assert!(
            LAVAPIPE_ICD_TOLERANCES
                .iter()
                .all(|(llvm, _)| *llvm != REFERENCE_LAVAPIPE_LLVM_MAJOR)
        );
    }

    #[test]
    fn pixel_check_counts_outliers() {
        let golden = vec![100u8; 4 * 16];
        let mut actual = golden.clone();
        actual[1] = 102;
        assert!(
            check_pixel_golden(
                &adapter(wgpu::DeviceType::Cpu, "llvmpipe", "Mesa (LLVM 23.1.1)"),
                "t",
                &actual,
                &golden,
                4,
                4,
                STRICT
            )
            .is_ok()
        );
        actual[5] = 120;
        let farm = adapter(wgpu::DeviceType::Cpu, "llvmpipe", "Mesa (LLVM 19.1.7)");
        let reference = adapter(wgpu::DeviceType::Cpu, "llvmpipe", "Mesa (LLVM 23.1.1)");
        assert_eq!(pixel_delta(&actual, &golden, 2), (20, 1));
        let farm_tol = PixelTolerance {
            max_channel_diff: 2,
            outlier_ppm: 30,
        };
        assert_eq!(farm_tol.allowed_outliers(800 * 450), 11);
        assert_eq!(farm_tol.allowed_outliers(16), 1);
        assert!(check_pixel_golden(&reference, "t", &actual, &golden, 4, 4, STRICT).is_err());
        assert!(check_pixel_golden(&farm, "t", &actual, &golden, 4, 4, STRICT).is_ok());
        for px in 2..7 {
            actual[px * 4] = 0;
        }
        assert!(check_pixel_golden(&farm, "t", &actual, &golden, 4, 4, STRICT).is_err());
    }

    #[test]
    fn software_reference_keeps_the_default_threshold_everywhere() {
        for name in ["alpha_modes", "ibl_spheres", "sponza", "taau_thin"] {
            assert_eq!(ssim_threshold_for(name, false), DEFAULT_SSIM_THRESHOLD);
        }
        // The lavapipe threshold is never lowered, for any scene, by any table entry.
        assert!(
            HARDWARE_TOLERANT_SCENES
                .iter()
                .all(|(_, t)| *t < DEFAULT_SSIM_THRESHOLD)
        );
    }

    #[test]
    fn hardware_gpus_relax_exactly_the_tolerant_scenes() {
        assert_eq!(ssim_threshold_for("alpha_modes", true), 0.95);
        assert_eq!(ssim_threshold_for("ibl_spheres", true), 0.95);
        // Everything else on hardware keeps the default threshold.
        for name in ["sponza", "taau_thin", "clustered_lights", "light_shadows"] {
            assert_eq!(ssim_threshold_for(name, true), DEFAULT_SSIM_THRESHOLD);
        }
    }

    /// ADR-025's headroom check: the relaxed thresholds must sit below every real-GPU score
    /// measured for these scenes (0.9675 and 0.9764 on the RTX 5080) yet far above what a
    /// broken render scores, so a real regression still fails.
    #[test]
    fn relaxed_thresholds_have_measured_headroom() {
        for (name, measured) in [("alpha_modes", 0.9675), ("ibl_spheres", 0.9764)] {
            let threshold = ssim_threshold_for(name, true);
            assert!(
                threshold <= measured - 0.005,
                "{name}: threshold {threshold} leaves no headroom below the measured {measured}"
            );
        }
    }
}
