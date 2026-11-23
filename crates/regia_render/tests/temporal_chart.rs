// SPDX-License-Identifier: MPL-2.0
//! Spec 27 § 2.3 through the temporal upscalers at a render scale (ADR-2638).
//!
//! `taa_motion.rs` scores native TAA on its own chart; these tests run the shared § 2.3 chart
//! (`regia_render::temporal_chart`: line pairs 1–8 px, a 1 px lattice in front of a far
//! wall, a box moving over a striped ground) through TSR at 0.67 and 0.5 with TAAU and the
//! FSR 3.1 port at the same scales as baselines, panning 2 px/frame and orbiting 1°/frame for
//! 120 frames. `cargo test -p regia_render --test temporal_chart -- --ignored --nocapture
//! chart_table` prints the whole table.

use regia_render::temporal_chart::{ChartReport, ChartRequest, run};

/// Rows of the chart's 1 px line-pair band (`temporal_chart`'s first band, rows 4–22) without its
/// 3-row margins, as the MTF profile reads it.
const BAND_1PX_ROWS: (u32, u32) = (7, 19);
use regia_render::{RenderDevice, TemporalUpscaler};

/// Serialises the GPU work of this binary's tests (one shared device, ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    }
}

fn row(label: &str, r: &ChartReport) -> String {
    format!(
        "| {label} | {} | {:.2} | {:.2} | {:.3} ({:.3}) | {:.1} | {:.4} ({:.2}) |",
        r.request.motion.name(),
        r.motion.motion_clarity,
        r.motion.thin_geometry,
        r.ghost.ghosting,
        r.ghost.ghost_residual,
        r.ghost.disocclusion_px,
        r.stability.flip,
        r.stability.luma_delta,
    )
}

fn chart(request: ChartRequest) -> ChartReport {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match run(&device(), &request) {
        Ok(report) => report,
        Err(err) => panic!("chart run failed: {err}"),
    }
}

fn name(upscaler: TemporalUpscaler) -> &'static str {
    match upscaler {
        TemporalUpscaler::Taau => "TAAU",
        TemporalUpscaler::Tsr => "TSR",
        TemporalUpscaler::Fsr3 => "FSR 3.1",
    }
}

fn motion(request: ChartRequest) -> regia_render::temporal_chart::MotionNumbers {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match regia_render::temporal_chart::measure_motion(&device(), &request) {
        Ok((numbers, _, _)) => numbers,
        Err(err) => panic!("chart run failed: {err}"),
    }
}

/// TSR at 0.67 panning 2 px/frame for 120 frames: the five § 2.3 numbers, at their targets
/// (spec 27 § 2.3, ADR-2638). The smear read 4 px before a restarting pixel kept to one layer.
#[test]
fn tsr_067_pan_section_2_3() {
    let r = chart(ChartRequest::pan(TemporalUpscaler::Tsr, 0.67));
    println!("{}", row("TSR 0.67", &r));
    assert!(
        r.motion.motion_clarity >= 0.85,
        "motion clarity {:.2} (≥ 0.85)",
        r.motion.motion_clarity
    );
    assert!(
        r.motion.thin_geometry >= 0.90,
        "thin geometry {:.2} (≥ 0.90)",
        r.motion.thin_geometry
    );
    assert!(
        r.ghost.ghosting <= 0.02,
        "ghosting {:.3} (≤ 0.02)",
        r.ghost.ghosting
    );
    assert!(
        r.ghost.disocclusion_px <= 2.0,
        "disocclusion smear {:.1} px (≤ 2)",
        r.ghost.disocclusion_px
    );
    assert!(
        r.stability.flip <= 0.01,
        "stability FLIP {:.4} (≤ 0.01)",
        r.stability.flip
    );
}

/// TSR at 0.67 orbiting 1°/frame: motion clarity and the lattice, at the § 2.3 targets. Before
/// ADR-2638 about one bar in seven was lost (a bar every sample of the frame missed took the
/// wall's motion and restarted as wall; thin geometry 0.86).
#[test]
fn tsr_067_orbit_keeps_clarity_and_the_lattice() {
    let m = motion(ChartRequest::orbit(TemporalUpscaler::Tsr, 0.67));
    println!(
        "TSR 0.67 orbit: clarity {:.2}, thin {:.2}",
        m.motion_clarity, m.thin_geometry
    );
    assert!(
        m.motion_clarity >= 0.85,
        "orbit motion clarity {:.2} (≥ 0.85)",
        m.motion_clarity
    );
    assert!(
        m.thin_geometry >= 0.90,
        "orbit thin geometry {:.2} (≥ 0.90)",
        m.thin_geometry
    );
}

/// TSR at 0.5: a held chart no longer flickers between the phases of detail the render
/// resolution aliases (0.100 FLIP, 10.8 levels per frame before ADR-2638; 0.0115 before
/// ADR-4859), the pan keeps the lattice, and the pan's motion clarity and the held stability meet
/// § 2.3 (clarity 0.69 before ADR-4859: the 1 px band was lost, see
/// `tsr_050_pan_resolves_the_1px_line_pairs`).
#[test]
fn tsr_050_held_chart_is_stable_and_the_pan_keeps_the_lattice() {
    let request = ChartRequest::pan(TemporalUpscaler::Tsr, 0.5);
    let stability = {
        let _gpu = SERIAL_GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match regia_render::temporal_chart::measure_stability(&device(), &request) {
            Ok(s) => s,
            Err(err) => panic!("chart run failed: {err}"),
        }
    };
    let m = motion(request);
    println!(
        "TSR 0.5: stability FLIP {:.4} (ΔL {:.2}), pan clarity {:.2}, thin {:.2}",
        stability.flip, stability.luma_delta, m.motion_clarity, m.thin_geometry
    );
    assert!(stability.flip <= 0.01, "held FLIP {:.4} (≤ 0.01)", stability.flip);
    assert!(
        stability.luma_delta <= 1.0,
        "held ΔL {:.2} levels (floor 1)",
        stability.luma_delta
    );
    assert!(
        m.thin_geometry >= 0.9,
        "pan thin geometry {:.2} (≥ 0.9)",
        m.thin_geometry
    );
    assert!(
        m.motion_clarity >= 0.85,
        "pan clarity {:.2} (≥ 0.85)",
        m.motion_clarity
    );
}

/// Mean |Δ| of 8-bit luma between neighbouring columns of `rows`, row-averaged, 8 px margins:
/// the 1 px band's real resolved contrast. Unlike a percentile contrast it does not read moiré
/// (a few dark columns among grey) as resolved bars.
fn column_contrast(image: &regia_render::ForwardResult, rows: (u32, u32)) -> f32 {
    let luma = |x: u32, y: u32| {
        let i = ((y * image.width + x) * 4) as usize;
        match image.pixels.get(i..i + 3) {
            Some(p) => 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]),
            None => 0.0,
        }
    };
    let n = (rows.1 - rows.0).max(1) as f32;
    let profile: Vec<f32> = (8..image.width.saturating_sub(8))
        .map(|x| (rows.0..rows.1).map(|y| luma(x, y)).sum::<f32>() / n)
        .collect();
    let steps: Vec<f32> = profile.windows(2).map(|w| (w[0] - w[1]).abs()).collect();
    steps.iter().sum::<f32>() / steps.len().max(1) as f32
}

/// TSR at 0.5 panning 2 px/frame resolves the 1 px line pairs (ADR-4859): the moving frame keeps
/// at least 85 % of the native still's column-to-column contrast in that band. Before ADR-4859 it
/// kept 0.2 %: every texel shares the jitter, all 3×3 samples sat on one phase of the bars, and
/// each output pixel no sample landed in was clipped to its neighbour's colour (a flat grey band).
#[test]
fn tsr_050_pan_resolves_the_1px_line_pairs() {
    let (_, moving, native) = {
        let _gpu = SERIAL_GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match regia_render::temporal_chart::measure_motion(
            &device(),
            &ChartRequest::pan(TemporalUpscaler::Tsr, 0.5),
        ) {
            Ok(v) => v,
            Err(err) => panic!("chart run failed: {err}"),
        }
    };
    let (got, want) = (
        column_contrast(&moving, BAND_1PX_ROWS),
        column_contrast(&native, BAND_1PX_ROWS),
    );
    println!("TSR 0.5 pan, 1 px band column contrast {got:.1} (native {want:.1})");
    assert!(want > 50.0, "the native still resolves the band ({want:.1})");
    assert!(
        got >= 0.85 * want,
        "1 px band column contrast {got:.1} (≥ 0.85 × native {want:.1})"
    );
}

/// TSR at 0.5 ghosting at the § 2.3 target (≤ 0.02; ADR-4934). It read 0.028 before: each frame
/// the box uncovers a strip of ground whose first pixel has no sample of its own at 0.5, so it
/// restarted from the nearest sample (often the neighboring stripe's) at that sample's full kernel
/// weight, and the off-pixel samples that followed re-converged it over 3–8 frames: the trail was
/// that ground at its stripe edges, not the box's color. A pixel that restarts with no sample of
/// its own now holds that value as a placeholder until one lands in it.
#[test]
fn tsr_050_ghosting_section_2_3() {
    let ghost = {
        let _gpu = SERIAL_GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match regia_render::temporal_chart::measure_ghost(
            &device(),
            &ChartRequest::pan(TemporalUpscaler::Tsr, 0.5),
        ) {
            Ok(g) => g,
            Err(err) => panic!("chart run failed: {err}"),
        }
    };
    println!(
        "TSR 0.5: ghosting {:.4} (residual {:.4}), smear {:.1} px",
        ghost.ghosting, ghost.ghost_residual, ghost.disocclusion_px
    );
    assert!(ghost.ghosting <= 0.02, "ghosting {:.4} (≤ 0.02)", ghost.ghosting);
    assert!(
        ghost.disocclusion_px <= 2.0,
        "disocclusion smear {:.1} px (≤ 2)",
        ghost.disocclusion_px
    );
}

/// TSR at 0.5 orbit thin geometry at the § 2.3 target (≥ 0.90). Read 0.683 on lavapipe and the
/// RTX 5090 before ADR-7694: at 0.5 a 1 px member goes unsampled for 4–8 frames in a row, and a
/// missed member was claimed a whole output pixel at a time, so under the orbit's sub-pixel motion
/// it ran ahead of its true pixel, which restarted as wall. Tracked locks (ADR-7694) read 0.905 on
/// both (57 of 63 bars; the mean over run lengths 104–136 is 0.879).
#[test]
fn tsr_050_orbit_thin_section_2_3() {
    let m = motion(ChartRequest::orbit(TemporalUpscaler::Tsr, 0.5));
    println!("TSR 0.5: orbit thin {:.3}", m.thin_geometry);
    assert!(
        m.thin_geometry >= 0.90,
        "orbit thin geometry {:.2} (≥ 0.90)",
        m.thin_geometry
    );
}

/// Largest channel difference a pixel may show against the fresh renderer after a cut (as
/// `camera_cut.rs`).
const CUT_MAX_CHANNEL_DIFF: u8 = 2;
/// Pixels allowed beyond it, per million (edge pixels of a nondeterministic GPU; `camera_cut.rs`).
const CUT_OUTLIER_PPM: usize = 1_000;

/// Pixels of `a` that differ from `b` by more than [`CUT_MAX_CHANNEL_DIFF`] on any channel.
fn cut_outliers(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len());
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(pa, pb)| {
            pa.iter()
                .zip(pb.iter())
                .any(|(x, y)| x.abs_diff(*y) > CUT_MAX_CHANNEL_DIFF)
        })
        .count()
}

/// The chart through TSR at a render scale after a camera cut (ADR-4859): 24 frames of the 2
/// px/frame pan, a cut, then 8 frames of the chart from a pose 900 px away. Every frame after
/// the cut matches a fresh renderer's frame of that pose frame for frame, at 0.5 and 0.67:
/// nothing of the pan's history, locks, remembered thin surfaces or own-motion labels survives
/// the cut, and history restarts and accumulates as from a first frame.
#[test]
fn tsr_cut_at_scale_restarts_like_a_fresh_renderer() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (w, h) = regia_render::temporal_chart::CHART_SIZE;
    let d = regia_render::temporal_chart::chart_distance();
    let mut scene = regia_render::temporal_chart::chart_scene(-1400.0, 1400.0);
    let post = regia_render::PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..regia_render::PostSettings::golden_deterministic()
    };
    for scale in [0.5f32, 0.67] {
        let mut quality = regia_render::FrameQuality::LAB;
        quality.taau = true;
        quality.depth_velocity = true;
        quality.render_scale_permille = (scale * 1000.0).round() as u16;
        let render = |renderer: &mut regia_render::FrameRenderer, scene: &regia_render::LabScene| {
            let mut fs = scene.as_frame_scene_with_quality(quality);
            fs.post = Some(&post);
            match renderer.render_offscreen(&device, &fs) {
                Ok(image) => image.pixels,
                Err(err) => panic!("TSR {scale}: {err}"),
            }
        };
        let fresh_renderer = || {
            let mut r = regia_render::FrameRenderer::new();
            r.set_temporal_upscaler(TemporalUpscaler::Tsr);
            r.set_camera_cut_detection(None);
            r
        };
        let mut cut = fresh_renderer();
        let mut leak = fresh_renderer();
        for frame in 0..24 {
            let x = 2.0 * frame as f32;
            scene.eye = [x, 0.0, d];
            scene.target = [x, 0.0, 0.0];
            render(&mut cut, &scene);
            render(&mut leak, &scene);
        }
        cut.camera_cut();
        scene.eye = [900.0, 0.0, d];
        scene.target = [900.0, 0.0, 0.0];
        let mut fresh = fresh_renderer();
        let allowed = (w * h) as usize * CUT_OUTLIER_PPM / 1_000_000;
        let mut leaked_max = 0;
        for frame in 1..=8 {
            let want = render(&mut fresh, &scene);
            let got = render(&mut cut, &scene);
            let leaked = render(&mut leak, &scene);
            let outliers = cut_outliers(&got, &want);
            leaked_max = leaked_max.max(cut_outliers(&leaked, &want));
            assert!(
                outliers <= allowed,
                "TSR {scale}, frame {frame} after the cut: {outliers} px differ from a fresh renderer by more \
                 than {CUT_MAX_CHANNEL_DIFF} (allowed {allowed}); temporal state leaked across the cut"
            );
        }
        // The finding, for the record: the same jump without a cut (the history's own rejection).
        eprintln!(
            "TSR {scale} without a cut: up to {leaked_max} px beyond ±{CUT_MAX_CHANNEL_DIFF} in 8 frames"
        );
    }
}

/// The baselines at 0.67 (spec 27 § 4.2 stage 4): TSR keeps more of the moving chart than TAAU
/// and more lattice bars than the FSR 3.1 port.
#[test]
fn tsr_067_beats_the_taau_and_fsr3_baselines_while_panning() {
    let tsr = motion(ChartRequest::pan(TemporalUpscaler::Tsr, 0.67));
    let taau = motion(ChartRequest::pan(TemporalUpscaler::Taau, 0.67));
    let fsr3 = motion(ChartRequest::pan(TemporalUpscaler::Fsr3, 0.67));
    println!(
        "0.67 pan clarity / thin: TSR {:.2} / {:.2}, TAAU {:.2} / {:.2}, FSR 3.1 {:.2} / {:.2}",
        tsr.motion_clarity,
        tsr.thin_geometry,
        taau.motion_clarity,
        taau.thin_geometry,
        fsr3.motion_clarity,
        fsr3.thin_geometry
    );
    assert!(tsr.motion_clarity >= taau.motion_clarity + 0.3);
    assert!(tsr.thin_geometry >= taau.thin_geometry + 0.3);
    assert!(tsr.thin_geometry >= fsr3.thin_geometry + 0.2);
}

/// The whole table (TSR, TAAU, FSR 3.1 × 0.67, 0.5 × pan, orbit; native TAA for scale).
#[test]
#[ignore = "about 4 000 lavapipe frames; run by hand for the report table"]
fn chart_table() {
    println!(
        "| resolve | motion | clarity (≥ 0.85) | thin (≥ 0.90) | ghost bias (residual) (≤ 0.02) | \
         smear px (≤ 2) | stability FLIP (ΔL) (≤ 0.01) |"
    );
    println!("|---|---|---|---|---|---|---|");
    let native = [(TemporalUpscaler::Taau, 1.0)];
    let scaled = [
        TemporalUpscaler::Tsr,
        TemporalUpscaler::Taau,
        TemporalUpscaler::Fsr3,
    ]
    .into_iter()
    .flat_map(|u| [(u, 0.67), (u, 0.5)]);
    // `TEMPORAL_CHART_ONLY=<substring>` keeps the rows whose label contains it ("TSR", "0.5").
    let only = regia_core::brand::env_var_os("TEMPORAL_CHART_ONLY").map(|v| v.to_string_lossy().into_owned());
    for (upscaler, scale) in native.into_iter().chain(scaled) {
        let label = if scale >= 1.0 {
            "native TAA".to_string()
        } else {
            format!("{} {scale}", name(upscaler))
        };
        if only.as_ref().is_some_and(|o| !label.contains(o.as_str())) {
            continue;
        }
        for request in [
            ChartRequest::pan(upscaler, scale),
            ChartRequest::orbit(upscaler, scale),
        ] {
            println!("{}", row(&label, &chart(request)));
        }
    }
}

fn write_ppm(path: &std::path::Path, width: u32, height: u32, rgb: impl Iterator<Item = [u8; 3]>) {
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for p in rgb {
        ppm.extend_from_slice(&p);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, ppm).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

/// Debug view of the disocclusion number: the FLIP profile behind the box's trailing edge on
/// the centre row, per resolve. `TEMPORAL_CHART_DUMP=<dir>` also writes the frames and maps.
#[test]
#[ignore = "debugging aid; run by hand"]
fn smear_profile() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dump = regia_core::brand::env_var_os("TEMPORAL_CHART_DUMP").map(std::path::PathBuf::from);
    let device = device();
    let (w, h) = regia_render::temporal_chart::CHART_SIZE;
    for (upscaler, scale) in [
        (TemporalUpscaler::Taau, 1.0),
        (TemporalUpscaler::Tsr, 0.67),
        (TemporalUpscaler::Tsr, 0.5),
        (TemporalUpscaler::Taau, 0.67),
        (TemporalUpscaler::Fsr3, 0.67),
    ] {
        let request = ChartRequest::pan(upscaler, scale);
        let frame = regia_core::brand::env_var_os("TEMPORAL_CHART_FRAME")
            .and_then(|v| v.to_string_lossy().parse().ok())
            .unwrap_or(32);
        let (a, b, map, edge) = match regia_render::temporal_chart::ghost_sample(&device, &request, frame) {
            Ok(v) => v,
            Err(err) => panic!("{err}"),
        };
        let cy = h / 2;
        let profile: Vec<String> = (1..=16)
            .map(|d| format!("{:.2}", map[(cy * w) as usize + (edge - d) as usize]))
            .collect();
        let luma = |img: &regia_render::ForwardResult, x: i32| {
            let i = ((cy * w) as usize + x as usize) * 4;
            img.pixels[i + 1]
        };
        let la: Vec<u8> = (1..=16).map(|d| luma(&a, edge - d)).collect();
        let lb: Vec<u8> = (1..=16).map(|d| luma(&b, edge - d)).collect();
        println!(
            "{} {scale}: FLIP behind edge {profile:?}\n  with {la:?}\n  w/o  {lb:?}",
            name(upscaler)
        );
        if let Some(dir) = dump.as_ref() {
            let _ = std::fs::create_dir_all(dir);
            let tag = format!(
                "{}_{:03}",
                name(upscaler).replace([' ', '.'], ""),
                (scale * 100.0) as u32
            );
            for (img, kind) in [(&a, "with"), (&b, "without")] {
                write_ppm(
                    &dir.join(format!("{tag}_{kind}.ppm")),
                    w,
                    h,
                    img.pixels.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]),
                );
            }
            write_ppm(
                &dir.join(format!("{tag}_flip.ppm")),
                w,
                h,
                map.iter().map(|v| [(v.clamp(0.0, 1.0) * 255.0) as u8; 3]),
            );
        }
    }
}

/// Tuning sweep for the held-view stability (debugging aid): TSR at 0.67 and 0.5 with the
/// default tuning and variants, stability only (48 held frames each).
#[test]
#[ignore = "tuning aid; run by hand"]
fn tsr_stability_sweep() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for scale in [0.67f32, 0.5] {
        let base = regia_render::TsrSettings::for_scale(scale);
        let variants: [(&str, Option<regia_render::TsrSettings>); 5] = [
            ("renderer default", None),
            ("for_scale", Some(base)),
            (
                "max_frames x1.5",
                Some(regia_render::TsrSettings {
                    max_frames: base.max_frames * 1.5,
                    ..base
                }),
            ),
            (
                "max_frames x2",
                Some(regia_render::TsrSettings {
                    max_frames: base.max_frames * 2.0,
                    ..base
                }),
            ),
            (
                "sharpness 0.25",
                Some(regia_render::TsrSettings {
                    sharpness: 0.25,
                    ..base
                }),
            ),
        ];
        for (label, tsr) in variants {
            let request = ChartRequest {
                tsr,
                ..ChartRequest::pan(TemporalUpscaler::Tsr, scale)
            };
            let s = match regia_render::temporal_chart::measure_stability(&device, &request) {
                Ok(s) => s,
                Err(err) => panic!("{err}"),
            };
            println!(
                "TSR {scale} {label}: stability FLIP {:.4}, ΔL {:.3}",
                s.flip, s.luma_delta
            );
        }
    }
}

/// Writes frames for eyeballing (debugging aid): `TEMPORAL_CHART_DUMP=<dir>` (required).
/// Held chart, last two frames, TSR at 0.5 and 0.67; orbit end frame and native, TSR 0.67.
#[test]
#[ignore = "debugging aid; run by hand"]
fn dump_frames() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(dir) = regia_core::brand::env_var_os("TEMPORAL_CHART_DUMP").map(std::path::PathBuf::from) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let device = device();
    let (w, h) = regia_render::temporal_chart::CHART_SIZE;
    let save = |img: &regia_render::ForwardResult, name: &str| {
        write_ppm(
            &dir.join(format!("{name}.ppm")),
            w,
            h,
            img.pixels.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]),
        );
    };
    for scale in [0.5f32, 0.67] {
        let request = ChartRequest::pan(TemporalUpscaler::Tsr, scale);
        let frames = match regia_render::temporal_chart::held_frames(&device, &request, 48) {
            Ok(f) => f,
            Err(err) => panic!("{err}"),
        };
        let tag = (scale * 100.0) as u32;
        save(&frames[46], &format!("held_tsr{tag}_46"));
        save(&frames[47], &format!("held_tsr{tag}_47"));
    }
    for view in [0u32, 1, 2, 3] {
        let request = ChartRequest {
            tsr: (view > 0).then(|| regia_render::TsrSettings {
                debug_view: view,
                ..regia_render::TsrSettings::for_scale(0.67)
            }),
            ..ChartRequest::orbit(TemporalUpscaler::Tsr, 0.67)
        };
        let (numbers, moving, native) = match regia_render::temporal_chart::measure_motion(&device, &request)
        {
            Ok(v) => v,
            Err(err) => panic!("{err}"),
        };
        println!("orbit TSR 0.67 view {view}: {numbers:?}");
        save(&moving, &format!("orbit_tsr67_moving_view{view}"));
        save(&native, "orbit_native");
    }
}

/// Per-bar lattice contrast of the orbit's last frame over the native still's median (debugging
/// aid): which bars a change keeps or loses. `TEMPORAL_CHART_VIEWS=1` adds the TSR debug views;
/// `TEMPORAL_CHART_DUMP=<dir>` writes the frames.
#[test]
#[ignore = "debugging aid; run by hand"]
fn orbit_bars() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let dump = regia_core::brand::env_var_os("TEMPORAL_CHART_DUMP").map(std::path::PathBuf::from);
    let scale = regia_core::brand::env_var_os("TEMPORAL_CHART_SCALE")
        .and_then(|v| v.to_string_lossy().parse().ok())
        .unwrap_or(0.67f32);
    let views: &[u32] = if regia_core::brand::env_var_os("TEMPORAL_CHART_VIEWS").is_some() {
        &[0, 1, 2]
    } else {
        &[0]
    };
    let (w, h) = regia_render::temporal_chart::CHART_SIZE;
    for &view in views {
        let request = ChartRequest {
            tsr: (view > 0).then(|| regia_render::TsrSettings {
                debug_view: view,
                ..regia_render::TsrSettings::for_scale(scale)
            }),
            ..ChartRequest::orbit(TemporalUpscaler::Tsr, scale)
        };
        let (numbers, moving, native) = match regia_render::temporal_chart::measure_motion(&device, &request)
        {
            Ok(v) => v,
            Err(err) => panic!("{err}"),
        };
        if let Some(dir) = dump.as_ref() {
            let _ = std::fs::create_dir_all(dir);
            for (img, tag) in [
                (&moving, format!("orbit_view{view}")),
                (&native, "orbit_native".into()),
            ] {
                write_ppm(
                    &dir.join(format!("{tag}.ppm")),
                    w,
                    h,
                    img.pixels.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]),
                );
            }
        }
        if view > 0 {
            continue;
        }
        let bars = regia_render::temporal_chart::lattice_bars(&moving, 0.0);
        let native_bars = regia_render::temporal_chart::lattice_bars(&native, 0.0);
        let mut sorted: Vec<f32> = native_bars.iter().map(|b| b.1).collect();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2].max(1.0);
        println!(
            "orbit {scale}: clarity {:.3}, thin {:.3}, native median contrast {median:.1}",
            numbers.motion_clarity, numbers.thin_geometry
        );
        let line: Vec<String> = bars
            .iter()
            .map(|(col, c)| format!("{col}:{:.2}", c / median))
            .collect();
        println!("{}", line.join(" "));
    }
}

/// Ghosting and disocclusion only, for the baselines and native TAA (debugging aid).
#[test]
#[ignore = "about 900 lavapipe frames; run by hand"]
fn ghost_table() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    for (upscaler, scale) in [
        (TemporalUpscaler::Taau, 1.0),
        (TemporalUpscaler::Taau, 0.67),
        (TemporalUpscaler::Taau, 0.5),
        (TemporalUpscaler::Fsr3, 0.67),
        (TemporalUpscaler::Fsr3, 0.5),
    ] {
        let request = ChartRequest::pan(upscaler, scale);
        match regia_render::temporal_chart::measure_ghost(&device, &request) {
            Ok(g) => println!(
                "{} {scale}: ghost {:.3} (residual {:.3}), smear {:.1} px",
                name(upscaler),
                g.ghosting,
                g.ghost_residual,
                g.disocclusion_px
            ),
            Err(err) => panic!("{err}"),
        }
    }
}

/// GPU time of TSR at 0.5 and 0.67 and of native TAA on the orbiting chart (timing aid,
/// ADR-7694): one renderer per mode, interleaved frame by frame on one device so the modes share
/// the card's clock and thermal state (a card-to-card spread is larger than the differences).
/// Output size `TEMPORAL_CHART_TIMING_SIZE` (default `1920x1080`), `TEMPORAL_CHART_TIMING_FRAMES`
/// frames per mode (default 240, the first 40 discarded); prints the median and 90th percentile
/// of the upscale pass (`taau_ms`) and of the whole frame (`frame_ms`). `TSR_SHADER_DIR` stages a
/// resolve variant as in `half_scale_probe`.
#[test]
#[ignore = "timing aid; run by hand on a card"]
fn tsr_scale_timings() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = |k: &str| regia_core::brand::env_var_os(k).map(|v| v.to_string_lossy().into_owned());
    if let Some(dir) = env("TSR_SHADER_DIR") {
        let path = std::path::Path::new(&dir).join("tsr_resolve.wgsl");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match regia_render::shader_reload::stage_shader_edit("tsr resolve", &text) {
                Ok(_) => println!("staged {}", path.display()),
                Err(err) => panic!("{}: {err}", path.display()),
            }
        }
    }
    let (width, height) = env("TEMPORAL_CHART_TIMING_SIZE")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((1920u32, 1080u32));
    let frames: u32 = env("TEMPORAL_CHART_TIMING_FRAMES")
        .and_then(|v| v.parse().ok())
        .unwrap_or(240)
        .max(50);
    let device = device();
    if !device.capabilities.timestamp_query {
        println!(
            "TIMING no timestamp queries on {}",
            device.capabilities.adapter_name
        );
        return;
    }
    let (cw, _) = regia_render::temporal_chart::CHART_SIZE;
    let mut scene = regia_render::temporal_chart::chart_scene(-3.0 * cw as f32, 3.0 * cw as f32);
    scene.width = width;
    scene.height = height;
    let d = regia_render::temporal_chart::chart_distance();
    let modes: [(&str, Option<f32>); 3] = [
        ("tsr 0.5", Some(0.5)),
        ("tsr 0.67", Some(0.67)),
        ("native taa", None),
    ];
    let mut renderers: Vec<regia_render::FrameRenderer> = modes
        .iter()
        .map(|(_, scale)| {
            let mut r = regia_render::FrameRenderer::new();
            if scale.is_some() {
                r.set_temporal_upscaler(TemporalUpscaler::Tsr);
            }
            r.set_gpu_timing(&device, true);
            r
        })
        .collect();
    let mut samples: Vec<(Vec<f32>, Vec<f32>)> = modes.iter().map(|_| (Vec::new(), Vec::new())).collect();
    for frame in 0..frames {
        let theta = (frame as f32 * 0.25).to_radians().sin() * 0.5;
        scene.eye = [d * theta.sin(), 0.0, d * theta.cos()];
        for (((_, scale), renderer), (taau, total)) in modes.iter().zip(&mut renderers).zip(&mut samples) {
            let post = regia_render::PostSettings {
                auto_exposure: false,
                exposure_ev: 14.5,
                bloom: false,
                taa: scale.is_none(),
                ..regia_render::PostSettings::golden_deterministic()
            };
            let mut quality = regia_render::FrameQuality::LAB;
            quality.depth_velocity = true;
            quality.taau = scale.is_some();
            quality.render_scale_permille = (scale.unwrap_or(1.0) * 1000.0).round() as u16;
            let mut fs = scene.as_frame_scene_with_quality(quality);
            fs.post = Some(&post);
            if let Err(err) = renderer.render_offscreen(&device, &fs) {
                panic!("{err}");
            }
            let gpu = renderer.gpu_timings();
            if frame >= 40 && gpu.frame_ms > 0.0 {
                taau.push(gpu.taau_ms);
                total.push(gpu.frame_ms);
            }
        }
    }
    let stat = |v: &mut Vec<f32>| {
        v.sort_by(f32::total_cmp);
        let at = |p: f32| {
            v.get(((v.len().max(1) - 1) as f32 * p).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        (at(0.5), at(0.9), v.len())
    };
    println!("TIMING {} {width}x{height}", device.capabilities.adapter_name);
    for ((label, _), (taau, total)) in modes.iter().zip(&mut samples) {
        let (t50, t90, n) = stat(taau);
        let (f50, f90, _) = stat(total);
        println!(
            "TIMING {label}: upscale median {t50:.3} ms p90 {t90:.3}; frame median {f50:.3} ms p90 {f90:.3} ({n} frames)"
        );
    }
}

/// Parses `TSR_SET="field=value,..."` over `base` (tuning aid). Unknown fields panic.
fn tsr_overrides(mut s: regia_render::TsrSettings, spec: &str) -> regia_render::TsrSettings {
    for pair in spec.split(',').filter(|p| !p.trim().is_empty()) {
        let Some((key, value)) = pair.split_once('=') else {
            panic!("TSR_SET entry {pair:?} is not field=value");
        };
        let v: f32 = match value.trim().parse() {
            Ok(v) => v,
            Err(err) => panic!("TSR_SET {key}: {err}"),
        };
        match key.trim() {
            "sharpness" => s.sharpness = v,
            "max_frames" => s.max_frames = v,
            "static_gamma" => s.static_gamma = v,
            "moving_gamma" => s.moving_gamma = v,
            "ridge_threshold" => s.ridge_threshold = v,
            "depth_tolerance" => s.depth_tolerance = v,
            "lock_decay" => s.lock_decay = v,
            "kernel_sharpness" => s.kernel_sharpness = v,
            "texture_bias_offset" => s.texture_bias_offset = v,
            "motion_sharpness" => s.motion_sharpness = v,
            "debug_view" => s.debug_view = v as u32,
            other => panic!("TSR_SET: unknown field {other:?}"),
        }
    }
    s
}

/// The 0.5 investigation probe (tuning aid, ADR-3116). Per scale in `TEMPORAL_CHART_SCALES`
/// (default `0.5`) and part in `TEMPORAL_CHART_PARTS` (default `pan,orbit,ghost,held`): the
/// numbers with the per-band modulation of the moving frame and of the native still. Part
/// `trail` prints the ghost trail per scored frame (`TEMPORAL_CHART_FRAMES`, default the ghost
/// number's): its signed bias and the per-column luma error behind the box's trailing edge.
/// Part `orbitn` prints the orbit's thin geometry for several run lengths
/// (`TEMPORAL_CHART_ORBIT_FRAMES`) and their mean, because one 120-frame number moves by several
/// bars with the jitter phase it ends on (ADR-4934). Part `orbittrack` scores each of the orbit's
/// last `TEMPORAL_CHART_TAIL` frames against a native render of the same pose and classes each lost
/// bar as shifted or faded (ADR-7694; off the head-on pose native point-samples a member that
/// straddles two pixels, so only the last frame compares like for like); it also prints each bar under 0.75 of the
/// native median (`orbitnear`) and, with `TEMPORAL_CHART_DUMP`, writes each scored frame's TSR output
/// before post as raw f32 RGB (`hdr<scale>_v<debug view>_f<frame>_<w>x<h>.f32`, ADR-7876). Part `lastview` runs the orbit (or the pan,
/// `TEMPORAL_CHART_MOTION=pan`) with TSR debug view `TEMPORAL_CHART_LASTVIEW` on its last frame
/// only and dumps it: with a staged resolve that writes a path code into the debug channel it
/// maps which resolve path each pixel took (ADR-4934's diagnosis).
/// `TSR_SHADER_DIR=<dir>` stages `tsr_prepare.wgsl` / `tsr_resolve.wgsl` / `tsr_sharpen.wgsl`
/// from it over the embedded TSR shaders (one process per variant: the device caches the TSR
/// pipelines); `TSR_SET="kernel_sharpness=5,..."` overrides the tuning (over
/// `TsrSettings::for_scale(scale)` with `TSR_BASE=for_scale`, else over the renderer default);
/// `TEMPORAL_CHART_DUMP=<dir>` writes the last moving frames and the native stills.
#[test]
#[ignore = "tuning aid; run by hand"]
fn half_scale_probe() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let env = |k: &str| regia_core::brand::env_var_os(k).map(|v| v.to_string_lossy().into_owned());
    if let Some(dir) = env("TSR_SHADER_DIR") {
        for (file, label) in [
            ("tsr_prepare.wgsl", "tsr prepare"),
            ("tsr_resolve.wgsl", "tsr resolve"),
            ("tsr_sharpen.wgsl", "tsr sharpen"),
        ] {
            let path = std::path::Path::new(&dir).join(file);
            if let Ok(text) = std::fs::read_to_string(&path) {
                match regia_render::shader_reload::stage_shader_edit(label, &text) {
                    Ok(_) => println!("staged {}", path.display()),
                    Err(err) => panic!("{}: {err}", path.display()),
                }
            }
        }
    }
    let scales: Vec<f32> = env("TEMPORAL_CHART_SCALES")
        .unwrap_or_else(|| "0.5".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let parts = env("TEMPORAL_CHART_PARTS").unwrap_or_else(|| "pan,orbit,ghost,held".into());
    let dump = env("TEMPORAL_CHART_DUMP").map(std::path::PathBuf::from);
    let device = device();
    let (w, h) = regia_render::temporal_chart::CHART_SIZE;
    let save = |img: &regia_render::ForwardResult, name: &str| {
        if let Some(dir) = dump.as_ref() {
            let _ = std::fs::create_dir_all(dir);
            write_ppm(
                &dir.join(format!("{name}.ppm")),
                w,
                h,
                img.pixels.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]),
            );
        }
    };
    for scale in scales {
        let tsr = env("TSR_SET").map(|spec| {
            let base = if env("TSR_BASE").as_deref() == Some("for_scale") {
                regia_render::TsrSettings::for_scale(scale)
            } else {
                regia_render::TsrSettings::default()
            };
            tsr_overrides(base, &spec)
        });
        let tag = (scale * 100.0).round() as u32;
        for part in parts.split(',') {
            let pan = ChartRequest {
                tsr,
                ..ChartRequest::pan(TemporalUpscaler::Tsr, scale)
            };
            match part.trim() {
                "pan" | "orbit" => {
                    let request = if part.trim() == "pan" {
                        pan
                    } else {
                        ChartRequest {
                            tsr,
                            ..ChartRequest::orbit(TemporalUpscaler::Tsr, scale)
                        }
                    };
                    let (m, moving, native) =
                        match regia_render::temporal_chart::measure_motion(&device, &request) {
                            Ok(v) => v,
                            Err(err) => panic!("{err}"),
                        };
                    println!(
                        "PROBE {scale} {part}: clarity {:.3} thin {:.3} mtf50 {:.3}/{:.3} mod {:?} native {:?}",
                        m.motion_clarity,
                        m.thin_geometry,
                        m.moving.mtf50,
                        m.native.mtf50,
                        m.moving.modulation.map(|v| (v * 100.0).round() / 100.0),
                        m.native.modulation.map(|v| (v * 100.0).round() / 100.0),
                    );
                    save(&moving, &format!("{part}_tsr{tag}"));
                    save(&native, &format!("{part}_native"));
                    if part.trim() == "orbit" {
                        let bars = regia_render::temporal_chart::lattice_bars(&moving, 0.0);
                        let native_bars = regia_render::temporal_chart::lattice_bars(&native, 0.0);
                        let mut sorted: Vec<f32> = native_bars.iter().map(|b| b.1).collect();
                        sorted.sort_by(f32::total_cmp);
                        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1.0).max(1.0);
                        let lost: Vec<i32> =
                            bars.iter().filter(|b| b.1 < 0.5 * median).map(|b| b.0).collect();
                        println!("PROBE {scale} orbit lost bars at columns {lost:?}");
                    }
                }
                "ghost" => match regia_render::temporal_chart::measure_ghost(&device, &pan) {
                    Ok(g) => println!(
                        "PROBE {scale} ghost: bias {:.4} residual {:.4} smear {:.1}",
                        g.ghosting, g.ghost_residual, g.disocclusion_px
                    ),
                    Err(err) => panic!("{err}"),
                },
                "held" => match regia_render::temporal_chart::measure_stability(&device, &pan) {
                    Ok(s) => println!("PROBE {scale} held: flip {:.4} dL {:.3}", s.flip, s.luma_delta),
                    Err(err) => panic!("{err}"),
                },
                "orbitn" => {
                    // Orbit thin geometry over several run lengths (`TEMPORAL_CHART_ORBIT_FRAMES`,
                    // default 104–136 by 8): each ends head-on with another jitter phase, so the
                    // mean tells a change from the one-frame number's noise.
                    let lengths =
                        env("TEMPORAL_CHART_ORBIT_FRAMES").unwrap_or_else(|| "104,112,120,128,136".into());
                    let mut thins = Vec::new();
                    for n in lengths.split(',').filter_map(|v| v.trim().parse::<u32>().ok()) {
                        let request = ChartRequest {
                            tsr,
                            frames: n,
                            ..ChartRequest::orbit(TemporalUpscaler::Tsr, scale)
                        };
                        match regia_render::temporal_chart::measure_motion(&device, &request) {
                            Ok((m, _, _)) => thins.push(m.thin_geometry),
                            Err(err) => panic!("{err}"),
                        }
                    }
                    let mean = thins.iter().sum::<f32>() / thins.len().max(1) as f32;
                    let min = thins.iter().copied().fold(f32::INFINITY, f32::min);
                    println!("PROBE {scale} orbitn: thin mean {mean:.3} min {min:.3} {thins:.3?}");
                }
                "orbittrack" => {
                    // The orbit's lattice per frame over its last `TEMPORAL_CHART_TAIL` frames
                    // (default 8; ADR-7694): each frame is scored against a native render of the
                    // same pose, at the bars that native render shows (members move up to 2 px per
                    // frame off the head-on pose), and each lost bar is classed as shifted (the
                    // moving frame holds it a column off) or faded. Run length
                    // `TEMPORAL_CHART_ORBIT_FRAMES` (first entry, default 120).
                    let frames: u32 = env("TEMPORAL_CHART_ORBIT_FRAMES")
                        .and_then(|v| v.split(',').next().and_then(|s| s.trim().parse().ok()))
                        .unwrap_or(120);
                    let tail: u32 = env("TEMPORAL_CHART_TAIL")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(8)
                        .min(frames);
                    let d = regia_render::temporal_chart::chart_distance();
                    let mut scene =
                        regia_render::temporal_chart::chart_scene(-3.0 * w as f32, 3.0 * w as f32);
                    let post = regia_render::PostSettings {
                        auto_exposure: false,
                        exposure_ev: 14.5,
                        bloom: false,
                        taa: false,
                        ..regia_render::PostSettings::golden_deterministic()
                    };
                    let mut quality = regia_render::FrameQuality::LAB;
                    quality.depth_velocity = true;
                    let native_quality = quality;
                    quality.taau = true;
                    quality.render_scale_permille = (scale * 1000.0).round() as u16;
                    let mut renderer = regia_render::FrameRenderer::new();
                    renderer.set_temporal_upscaler(TemporalUpscaler::Tsr);
                    renderer.set_tsr_settings(tsr.unwrap_or_default());
                    // Rows of the lattice band clear of the horizontal members (`temporal_chart`'s
                    // LATTICE_ROWS 138–174, pitch 6, as its bar contrast reads them).
                    let rows: Vec<u32> = (140..172)
                        .filter(|r| (r + 6 - 140) % 6 > 1 && (r + 6 - 140) % 6 < 5)
                        .collect();
                    let column = |img: &regia_render::ForwardResult, x: i32| -> f32 {
                        let x = x.clamp(0, w as i32 - 1) as u32;
                        rows.iter()
                            .map(|&y| {
                                let i = ((y * w + x) * 4) as usize;
                                img.pixels.get(i..i + 3).map_or(0.0, |p| {
                                    0.2126 * f32::from(p[0])
                                        + 0.7152 * f32::from(p[1])
                                        + 0.0722 * f32::from(p[2])
                                })
                            })
                            .sum::<f32>()
                            / rows.len().max(1) as f32
                    };
                    let contrast = |img: &regia_render::ForwardResult, x: i32| {
                        column(img, x) - 0.5 * (column(img, x - 2) + column(img, x + 2))
                    };
                    let a = regia_render::temporal_chart::ORBIT_AMPLITUDE_DEG;
                    let (mut sum_lost, mut sum_bars) = (0usize, 0usize);
                    for frame in 0..frames {
                        let p = frame as f32 - (frames as f32 - 1.0);
                        let q = (p + a).rem_euclid(4.0 * a);
                        let theta = if q < 2.0 * a { q - a } else { 3.0 * a - q }.to_radians();
                        scene.eye = [d * theta.sin(), 0.0, d * theta.cos()];
                        scene.target = [0.0, 0.0, 0.0];
                        let mut fs = scene.as_frame_scene_with_quality(quality);
                        fs.post = Some(&post);
                        // With a dump directory, each scored frame's TSR output before post is written
                        // raw (f32 RGB): with a staged sharpen shader that writes resolve state into a
                        // debug view, it reads locks, offsets and weights per pixel (ADR-7876).
                        let hdr_dump = dump.is_some() && frame + tail >= frames;
                        if hdr_dump {
                            renderer.request_hdr_capture();
                        }
                        let moving = match renderer.render_offscreen(&device, &fs) {
                            Ok(image) => image,
                            Err(err) => panic!("{err}"),
                        };
                        if hdr_dump
                            && let Ok(Some(cap)) = renderer.take_hdr_capture(&device)
                            && let Some(dir) = dump.as_ref()
                        {
                            let view = tsr.map_or(0, |t| t.debug_view);
                            let bytes: Vec<u8> =
                                cap.rgb.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
                            let _ = std::fs::create_dir_all(dir);
                            let _ = std::fs::write(
                                dir.join(format!(
                                    "hdr{tag}_v{view}_f{frame}_{}x{}.f32",
                                    cap.width, cap.height
                                )),
                                bytes,
                            );
                        }
                        if frame + tail < frames {
                            continue;
                        }
                        let mut nfs = scene.as_frame_scene_with_quality(native_quality);
                        nfs.post = Some(&post);
                        let native = match regia_render::FrameRenderer::new().render_offscreen(&device, &nfs)
                        {
                            Ok(image) => image,
                            Err(err) => panic!("{err}"),
                        };
                        // Native bars: local maxima of the column contrast, 4 px from the edges.
                        let bars: Vec<(i32, f32)> = (4..w as i32 - 4)
                            .map(|x| (x, contrast(&native, x)))
                            .filter(|&(x, c)| {
                                c > 20.0 && c >= contrast(&native, x - 1) && c > contrast(&native, x + 1)
                            })
                            .collect();
                        let mut sorted: Vec<f32> = bars.iter().map(|b| b.1).collect();
                        sorted.sort_by(f32::total_cmp);
                        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(1.0).max(1.0);
                        let (mut shifted, mut faded) = (Vec::new(), Vec::new());
                        for &(x, _) in &bars {
                            if contrast(&moving, x) >= 0.5 * median {
                                continue;
                            }
                            if contrast(&moving, x - 1).max(contrast(&moving, x + 1)) >= 0.5 * median {
                                shifted.push(x);
                            } else {
                                faded.push(x);
                            }
                        }
                        // Every bar near the threshold (contrast over the native median under 0.75):
                        // the margin, and which bar a backend that differs slightly would lose (ADR-7876).
                        let near: Vec<String> = bars
                            .iter()
                            .map(|&(x, _)| (x, contrast(&moving, x) / median))
                            .filter(|&(_, r)| r < 0.75)
                            .map(|(x, r)| format!("{x}:{r:.2}"))
                            .collect();
                        println!(
                            "PROBE {scale} orbitnear f{frame} median {median:.1}: {}",
                            near.join(" ")
                        );
                        let lost = shifted.len() + faded.len();
                        sum_lost += lost;
                        sum_bars += bars.len();
                        println!(
                            "PROBE {scale} orbittrack f{frame} theta {:.1}: thin {:.3} ({} bars) shifted {shifted:?} faded {faded:?}",
                            theta.to_degrees(),
                            1.0 - lost as f32 / bars.len().max(1) as f32,
                            bars.len()
                        );
                        save(&moving, &format!("orbittrack_tsr{tag}_f{frame}"));
                        save(&native, &format!("orbittrack_native_f{frame}"));
                    }
                    println!(
                        "PROBE {scale} orbittrack: thin mean {:.3} over the last {tail} frames",
                        1.0 - sum_lost as f32 / sum_bars.max(1) as f32
                    );
                }
                "lastview" => {
                    // The orbit (`TEMPORAL_CHART_MOTION=pan` for the pan) with TSR debug view
                    // `TEMPORAL_CHART_LASTVIEW` (default 2, locks) on its last frame only, so the
                    // view does not change the history the run builds; dumps that frame.
                    let view: u32 = env("TEMPORAL_CHART_LASTVIEW")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(2);
                    let orbit = env("TEMPORAL_CHART_MOTION").as_deref() != Some("pan");
                    let frames = 120u32;
                    let d = regia_render::temporal_chart::chart_distance();
                    let mut scene =
                        regia_render::temporal_chart::chart_scene(-3.0 * w as f32, 3.0 * w as f32);
                    let post = regia_render::PostSettings {
                        auto_exposure: false,
                        exposure_ev: 14.5,
                        bloom: false,
                        taa: false,
                        ..regia_render::PostSettings::golden_deterministic()
                    };
                    let mut quality = regia_render::FrameQuality::LAB;
                    quality.taau = true;
                    quality.depth_velocity = true;
                    quality.render_scale_permille = (scale * 1000.0).round() as u16;
                    let settings = tsr.unwrap_or_default();
                    let mut renderer = regia_render::FrameRenderer::new();
                    renderer.set_temporal_upscaler(TemporalUpscaler::Tsr);
                    renderer.set_tsr_settings(settings);
                    let a = regia_render::temporal_chart::ORBIT_AMPLITUDE_DEG;
                    for frame in 0..frames {
                        if orbit {
                            // `temporal_chart::orbit_angle` at 1°/frame.
                            let p = frame as f32 - (frames as f32 - 1.0);
                            let q = (p + a).rem_euclid(4.0 * a);
                            let theta = if q < 2.0 * a { q - a } else { 3.0 * a - q }.to_radians();
                            scene.eye = [d * theta.sin(), 0.0, d * theta.cos()];
                            scene.target = [0.0, 0.0, 0.0];
                        } else {
                            let x = 2.0 * frame as f32 - 2.0 * (frames - 1) as f32;
                            scene.eye = [x, 0.0, d];
                            scene.target = [x, 0.0, 0.0];
                        }
                        if frame + 1 == frames {
                            renderer.set_tsr_settings(regia_render::TsrSettings {
                                debug_view: view,
                                ..settings
                            });
                        }
                        let mut fs = scene.as_frame_scene_with_quality(quality);
                        fs.post = Some(&post);
                        let image = match renderer.render_offscreen(&device, &fs) {
                            Ok(image) => image,
                            Err(err) => panic!("{err}"),
                        };
                        if frame + 2 >= frames {
                            save(&image, &format!("lastview{view}_tsr{tag}_f{frame}"));
                        }
                    }
                    println!("PROBE {scale} lastview {view}: dumped");
                }
                "trail" => {
                    // The ghost trail per scored frame: the signed bias the ghost number reads
                    // (over the box/ground contrast), and the per-column with − without luma
                    // over the center rows, behind the trailing edge (column 1 = just behind).
                    let frames =
                        env("TEMPORAL_CHART_FRAMES").unwrap_or_else(|| "16,21,26,31,36,41,46".into());
                    for frame in frames.split(',').filter_map(|f| f.trim().parse::<u32>().ok()) {
                        let (a, b, _, edge) =
                            match regia_render::temporal_chart::ghost_sample(&device, &pan, frame) {
                                Ok(v) => v,
                                Err(err) => panic!("{err}"),
                            };
                        let luma = |img: &regia_render::ForwardResult, x: i32, y: i32| {
                            let i = ((y * w as i32 + x) * 4) as usize;
                            match img.pixels.get(i..i + 3) {
                                Some(p) => {
                                    0.2126 * f32::from(p[0])
                                        + 0.7152 * f32::from(p[1])
                                        + 0.0722 * f32::from(p[2])
                                }
                                None => 0.0,
                            }
                        };
                        let cy = (h / 2) as i32;
                        let now = edge + 12;
                        let old = now - 32;
                        let column = |x: i32| -> f32 {
                            ((cy - 10)..=(cy + 10))
                                .map(|y| luma(&a, x, y) - luma(&b, x, y))
                                .sum::<f32>()
                                / 21.0
                        };
                        let (mut obj, mut gnd) = (0.0, 0.0);
                        for y in (cy - 6)..=(cy + 6) {
                            for x in (now - 6)..=(now + 6) {
                                obj += luma(&a, x, y);
                                gnd += luma(&b, x, y);
                            }
                        }
                        let contrast = ((obj - gnd) / 169.0).abs().max(1.0);
                        let region: Vec<i32> = ((old - 10)..=(old + 10))
                            .filter(|x| (x - now).abs() >= 14 && *x >= 0)
                            .collect();
                        let bias =
                            region.iter().map(|&x| column(x)).sum::<f32>() / region.len().max(1) as f32;
                        let behind: Vec<String> =
                            (1..=32).map(|d| format!("{:+.0}", column(edge - d))).collect();
                        println!(
                            "PROBE {scale} trail f{frame}: bias {:.4} ({bias:+.2} levels / {contrast:.0}); behind edge {}",
                            bias / contrast,
                            behind.join(" ")
                        );
                        save(&a, &format!("trail_tsr{tag}_f{frame}_with"));
                        save(&b, &format!("trail_tsr{tag}_f{frame}_without"));
                    }
                }
                other => panic!("TEMPORAL_CHART_PARTS: unknown part {other:?}"),
            }
        }
    }
}

/// FSR 3.1 at 0.67 ghosting at the § 2.3 target (≤ 0.02; known issue A05, ADR-7678), with the
/// chart's own ghost scene and measure (`temporal_chart::measure_ghost`). It read 0.029 on every
/// adapter after ADR-5890 moved the ground stripe off an 8-bit rounding boundary (0.018 before),
/// and no test held FSR to the target.
#[test]
fn fsr3_067_ghosting_meets_spec27() {
    let ghost = {
        let _gpu = SERIAL_GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match regia_render::temporal_chart::measure_ghost(
            &device(),
            &ChartRequest::pan(TemporalUpscaler::Fsr3, 0.67),
        ) {
            Ok(g) => g,
            Err(err) => panic!("chart run failed: {err}"),
        }
    };
    println!(
        "FSR 3.1 0.67: ghosting {:.4} (residual {:.4}), smear {:.1} px",
        ghost.ghosting, ghost.ghost_residual, ghost.disocclusion_px
    );
    assert!(ghost.ghosting <= 0.02, "ghosting {:.4} (≤ 0.02)", ghost.ghosting);
}
