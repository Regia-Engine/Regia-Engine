// SPDX-License-Identifier: MPL-2.0
//! The tunnel acceptance scene: one synthetic scene that exercises the tunnel features together.
//!
//! A terrain with an open-cut ramp hole (`HolesMask`), a tube under the ground with a lip around
//! the cut, an interior volume, a fog shaft, a headlight with dust, the procedural sky and auto
//! exposure, driven along a scripted camera path in and out of the mouth at 20 m/s. Per-frame
//! measurements (mean luminance, crack detector, terrain pixels inside the cut) run on lavapipe.
//! The tunnel features (exposure zone, reverb zone, probe volume, interior fog, light shafts,
//! world pick layers) plug into named slots (see README.md, `fills.rs`); one test per slot.
//!
//! By hand: `REGIA_DUMP_DIR=/tmp/out cargo test -p regia_terrain --test tunnel_scene --
//! --nocapture` writes the frames, `metrics.csv` and `contact_sheet.png` (old behaviour on the
//! top row, the tunnel set on the bottom row) to `/tmp/out/tunnel_scene/`. `REGIA_TUNNEL_FPS`
//! (default 10) sets the frame rate the 20 m/s path is sampled at.

mod dump;
mod fills;
mod measure;
mod oracle;
mod scene;

use std::sync::OnceLock;

use regia_core::glam::Vec3;
use scene::{CamFrame, Variant, camera_path, tunnel_scene};

const SPEED: f32 = 20.0;

fn fps() -> f32 {
    regia_core::brand::env_var("REGIA_TUNNEL_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0)
}

/// One rendered path.
struct Run {
    path: Vec<CamFrame>,
    frames: Vec<Vec<u8>>,
    luma: Vec<f32>,
    black: Vec<f32>,
    /// `(judged, terrain-like)` per frame.
    cut: Vec<(u32, u32)>,
    /// `(samples, cracked)` per frame.
    edge: Vec<(u32, u32)>,
}

fn run_path(variant: Variant) -> Run {
    let path = camera_path(SPEED, fps());
    let mut scene = tunnel_scene(variant);
    scene.dt = 1.0 / fps();
    let mut run = Run {
        path: path.clone(),
        frames: Vec::new(),
        luma: Vec::new(),
        black: Vec::new(),
        cut: Vec::new(),
        edge: Vec::new(),
    };
    scene.warm_up(&path[0]);
    for cam in &path {
        let px = scene.render(cam);
        run.luma.push(measure::mean_luma(&px));
        run.black.push(measure::black_fraction(&px));
        run.cut.push(measure::terrain_in_cut(&px, cam));
        run.edge.push(measure::edge_cracks(&px, cam));
        run.frames.push(px);
    }
    run
}

/// Both paths, rendered once for every test in this file; dumps when `REGIA_DUMP_DIR` is set.
fn runs() -> &'static (Run, Run) {
    static RUNS: OnceLock<(Run, Run)> = OnceLock::new();
    RUNS.get_or_init(|| {
        let old = run_path(Variant::Old);
        let on = run_path(Variant::On);
        if let Some(dir) = dump::dump_dir() {
            write_dump(&dir, &old, &on);
        }
        (old, on)
    })
}

fn write_dump(dir: &std::path::Path, old: &Run, on: &Run) {
    let mut csv = String::from(
        "variant,frame,t,x,y,z,inbound,mean_luma,black_fraction,cut_judged,cut_terrain,edge_samples,edge_cracked\n",
    );
    for (name, run) in [("old", old), ("on", on)] {
        for (i, cam) in run.path.iter().enumerate() {
            dump::write_png(
                &dir.join(name).join(format!("frame_{i:03}.png")),
                scene::W,
                scene::H,
                &run.frames[i],
            );
            csv.push_str(&format!(
                "{name},{i},{:.3},{:.2},{:.2},{:.2},{},{:.3},{:.5},{},{},{},{}\n",
                cam.time,
                cam.eye[0],
                cam.eye[1],
                cam.eye[2],
                cam.inbound,
                run.luma[i],
                run.black[i],
                run.cut[i].0,
                run.cut[i].1,
                run.edge[i].0,
                run.edge[i].1
            ));
        }
    }
    let _ = std::fs::write(dir.join("metrics.csv"), csv);
    // Every judged edge sample, and an overlay per frame (red: cracked, green: clean).
    let mut edges = String::from("variant,frame,x,y,worst,ground,lip,seam\n");
    let rgb = |c: [f32; 3]| format!("{:.0} {:.0} {:.0}", c[0], c[1], c[2]);
    for (name, run) in [("old", old), ("on", on)] {
        for (i, cam) in run.path.iter().enumerate() {
            let samples = measure::edge_samples(&run.frames[i], cam);
            if samples.is_empty() {
                continue;
            }
            let mut overlay = run.frames[i].clone();
            for s in &samples {
                let seam: Vec<String> = s.seam.iter().map(|&c| rgb(c)).collect();
                edges.push_str(&format!(
                    "{name},{i},{:.1},{:.1},{:.1},{},{},{}\n",
                    s.at[0],
                    s.at[1],
                    s.worst,
                    rgb(s.ground),
                    rgb(s.lip),
                    seam.join("|")
                ));
                let (x, y) = (s.at[0].floor() as i64, s.at[1].floor() as i64);
                if (0..i64::from(scene::W)).contains(&x) && (0..i64::from(scene::H)).contains(&y) {
                    let k = ((y as u32 * scene::W + x as u32) * 4) as usize;
                    let mark = if s.cracked { [255, 0, 0] } else { [0, 255, 0] };
                    overlay[k..k + 3].copy_from_slice(&mark);
                }
            }
            dump::write_png(
                &dir.join(name).join(format!("edges_{i:03}.png")),
                scene::W,
                scene::H,
                &overlay,
            );
        }
    }
    let _ = std::fs::write(dir.join("edges.csv"), edges);
    let columns = dump::sheet_indices(on.frames.len(), 8);
    let (w, h, sheet) = dump::contact_sheet(&[&old.frames, &on.frames], &columns);
    dump::write_png(&dir.join("contact_sheet.png"), w, h, &sheet);
    let listing: Vec<String> = columns
        .iter()
        .map(|&i| {
            format!(
                "{i} (z {:.1}, {})",
                on.path[i].eye[2],
                if on.path[i].inbound { "in" } else { "out" }
            )
        })
        .collect();
    let _ = std::fs::write(
        dir.join("contact_sheet.txt"),
        format!(
            "top row: old behaviour (interior lit as open sky); bottom row: tunnel set on\ncolumns, frame (camera z, direction): {}\n",
            listing.join(", ")
        ),
    );
    eprintln!(
        "tunnel_scene: dumped {} frames per variant to {}",
        on.frames.len(),
        dir.display()
    );
}

fn report(name: &str, run: &Run) {
    let max_step = run
        .luma
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    let (mn, mx) = run
        .luma
        .iter()
        .fold((f32::MAX, 0.0f32), |(a, b), &l| (a.min(l), b.max(l)));
    eprintln!(
        "{name}: {} frames, mean luma {mn:.1}..{mx:.1}, max frame-to-frame step {max_step:.1}, max black fraction {:.4}",
        run.luma.len(),
        run.black.iter().copied().fold(0.0, f32::max)
    );
}

#[test]
fn the_path_drives_in_and_out_at_twenty_metres_per_second() {
    let path = camera_path(SPEED, fps());
    assert!(path.len() > 20);
    let moving: Vec<_> = path
        .windows(2)
        .filter(|w| w[0].inbound == w[1].inbound && w[0].eye[2] != w[1].eye[2])
        .collect();
    for w in &moving {
        let speed = (w[1].eye[2] - w[0].eye[2]).abs() / (w[1].time - w[0].time);
        assert!((speed - SPEED).abs() < 0.01, "speed {speed}");
    }
    assert!(
        path.iter().any(|c| c.eye[2] < scene::RAMP_START_Z - 20.0),
        "starts on the approach"
    );
    assert!(path.iter().any(|c| c.eye[2] > 30.0), "reaches deep inside");
    assert!(
        path.last().expect("path").eye[2] < scene::RAMP_START_Z - 20.0,
        "ends outside"
    );
}

#[test]
fn the_collider_cut_matches_the_mask() {
    let mask = scene::mouth_mask();
    let (heights, cut) = scene::cut_collider_heights(&mask);
    assert!(
        cut > 100,
        "a 10 m × 20 m cut removes about 200 samples, got {cut}"
    );
    assert_eq!(heights.iter().filter(|h| h.is_nan()).count(), cut);
    assert!(
        mask.window_texel_m() < scene::LIP_OVERLAP,
        "the lip must reach past the drawn edge's raster error"
    );
}

#[test]
fn no_flash_and_no_black_frame() {
    let (old, on) = runs();
    report("old", old);
    report("on", on);
    for (name, run) in [("old", old), ("on", on)] {
        for (i, (l, b)) in run.luma.iter().zip(&run.black).enumerate() {
            assert!(*l > 3.0, "{name} frame {i} is black (mean luma {l:.2})");
            assert!(
                *b < 0.9,
                "{name} frame {i}: {:.0} % of pixels are black",
                b * 100.0
            );
        }
        let worst = run
            .luma
            .windows(2)
            .enumerate()
            .map(|(i, w)| ((w[1] - w[0]).abs(), i + 1))
            .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a });
        assert!(
            worst.0 <= FLASH_BOUND,
            "{name}: mean luma jumps {:.1} levels at frame {} (bound {FLASH_BOUND})",
            worst.0,
            worst.1
        );
    }
}

/// The largest mean-luma step between two frames (0–255) the scene allows today. Entering the
/// mouth is a real change of picture, so this is not zero; the exposure-zone lane tightens it.
/// Measured on lavapipe at 10 fps (2 m a frame): old 19.0 (back out past the cut), on 42.4
/// (passing under the roof at z 0, the tube lit by its interior floor only) and +35 driving out
/// into daylight with the eye still adapted to the tube. Before the interior-shade face fix the
/// on path flickered by up to 75 levels between neighbouring views inside the tube.
const FLASH_BOUND: f32 = 60.0;
/// What an exposure zone must reach: the step is the picture changing, not exposure adapting.
const FLASH_BOUND_WITH_ZONE: f32 = 25.0;

#[test]
fn no_terrain_inside_the_cut() {
    let (old, on) = runs();
    for (name, run) in [("old", old), ("on", on)] {
        let judged: u32 = run.cut.iter().map(|c| c.0).sum();
        let terrain: u32 = run.cut.iter().map(|c| c.1).sum();
        let best = run.cut.iter().map(|c| c.0).max().unwrap_or(0);
        eprintln!(
            "{name}: {judged} footprint pixels judged over the path (best frame {best}), terrain-like {terrain}"
        );
        assert!(
            best >= 150,
            "{name}: the path must see the cut well in at least one frame, best {best}"
        );
        assert_eq!(terrain, 0, "{name}: terrain pixels inside the hole footprint");
    }
}

#[test]
fn the_cut_detector_sees_terrain_in_an_uncut_ground() {
    // Negative control: without the hole the same camera sees terrain in the footprint.
    let (_, on) = runs();
    let (i, cam) = on
        .path
        .iter()
        .enumerate()
        .filter(|(_, c)| c.eye[1] > 1.0 && c.eye[2] > -34.0)
        .min_by_key(|(_, c)| c.index)
        .expect("approach frame");
    let mut uncut = scene::TunnelScene::new(Variant::On, scene::Slots::today(), 0.0, false);
    uncut.warm_up(cam);
    let px = uncut.render(cam);
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(&dir.join("control_uncut.png"), scene::W, scene::H, &px);
    }
    let (judged, terrain) = measure::terrain_in_cut(&px, cam);
    eprintln!("uncut ground, frame {i}: {terrain}/{judged} terrain-like pixels in the footprint");
    assert!(
        judged >= 150 && terrain * 10 >= judged * 9,
        "the detector must flag an uncut ground: {terrain}/{judged}"
    );
}

#[test]
fn the_lip_hides_the_cut_edge() {
    let (old, on) = runs();
    for (name, run) in [("old", old), ("on", on)] {
        let samples: u32 = run.edge.iter().map(|c| c.0).sum();
        let cracked: u32 = run.edge.iter().map(|c| c.1).sum();
        let frames: Vec<String> = run
            .edge
            .iter()
            .enumerate()
            .filter(|(_, c)| c.1 > 0)
            .map(|(i, c)| format!("frame {i} (z {:.1}): {}/{}", run.path[i].eye[2], c.1, c.0))
            .collect();
        eprintln!("{name}: {cracked} cracked of {samples} edge samples {frames:?}");
        assert!(
            samples >= 200,
            "{name}: the path must see the edge, {samples} samples"
        );
        assert_eq!(
            cracked, 0,
            "{name}: cracks along the lip, {cracked} of {samples} samples: {frames:?}"
        );
    }
}

#[test]
fn the_crack_detector_flags_a_lip_that_stops_short() {
    // Negative control: a lip that stops 0.5 m short of the cut leaves the trench showing.
    // The camera stands about 6 m before the cut: from there the detector judges both side edges
    // and the near edge (about 130 samples; from 14 m back only 3 m of each side edge are within
    // its 18 m and wide enough on screen, about 26 samples).
    let cam = camera_path(SPEED, fps())
        .into_iter()
        .find(|c| c.eye[2] >= scene::RAMP_START_Z - 6.5)
        .expect("approach frame");
    let mut gapped = scene::TunnelScene::new(Variant::Old, scene::Slots::none(), 0.5, true);
    gapped.warm_up(&cam);
    let px = gapped.render(&cam);
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(&dir.join("control_gapped_lip.png"), scene::W, scene::H, &px);
    }
    let (samples, cracked) = measure::edge_cracks(&px, &cam);
    eprintln!(
        "gapped lip, camera z {:.1}: {cracked}/{samples} cracked samples",
        cam.eye[2]
    );
    assert!(
        samples >= 50 && cracked * 4 >= samples,
        "gap must be detected: {cracked}/{samples} cracked samples at camera z {:.1}",
        cam.eye[2]
    );
}

#[test]
fn old_and_new_differ_inside_the_tube() {
    // The before/after exists: the interior shade changes the picture in the tube.
    let (old, on) = runs();
    let deep = on.path.iter().position(|c| c.eye[2] > 20.0).expect("deep frame");
    let diff: f32 = old.frames[deep]
        .iter()
        .zip(&on.frames[deep])
        .map(|(a, b)| (f32::from(*a) - f32::from(*b)).abs())
        .sum::<f32>()
        / old.frames[deep].len() as f32;
    eprintln!(
        "mean abs difference at frame {deep}: {diff:.2} levels (luma old {:.1}, on {:.1})",
        old.luma[deep], on.luma[deep]
    );
    assert!(diff > 1.0, "the tunnel set must change the picture");
}

// The tunnel features, one test per slot of `scene::Slots` (README.md). `Slots::today()` fills
// every slot (`fills.rs`); the on path renders with all of them, the old path with none.

#[test]
fn exposure_zone_removes_the_entry_flash() {
    // On the zone drive (`ZONE_DRIVE_TURN_Z`, ADR-8629). Ignored from ADR-3257 to ADR-8629: on the
    // shared path the best of about 50 zone tunings stepped 26.4 levels driving out of the mouth
    // (the eye chases a histogram two frames old), 27.5 at the 40 m turnaround after ADR-7975, and
    // 25.5 there with the cook's band (the band does not hold the eye 40 m in). With the cook's
    // band and the drive to the section darker than it, the worst step is 14.9 (entering).
    let slots = scene::Slots::today();
    assert!(slots.exposure_zone.is_filled(), "no exposure zone plugged in");
    // The zone covers the tube and fades in toward the mouth: weight 1 in its box, 0 on the
    // approach.
    let zone = fills::TubeExposureZone::new();
    let drives = zone_drives();
    let (luma, exposure) = &drives.with;
    for cam in &drives.path {
        let w = zone.weight(cam.eye);
        if zone.contains(cam.eye) {
            assert_eq!(w, 1.0, "inside the zone at z {:.1}", cam.eye[2]);
        }
        if zone.zone.signed_distance(&zone.at, cam.eye) >= zone.zone.blend_distance {
            assert_eq!(w, 0.0, "on the approach at z {:.1}", cam.eye[2]);
        }
    }
    let (worst, at) = luma
        .windows(2)
        .enumerate()
        .map(|(i, w)| ((w[1] - w[0]).abs(), i + 1))
        .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a });
    for (i, cam) in drives.path.iter().enumerate() {
        if let Some((ev, goal, meter, mode)) = exposure[i] {
            eprintln!(
                "on frame {i} z {:.1} {} luma {:.1} ev {ev:.2} goal {goal:.2} meter {meter:.2} {mode} zone weight {:.2}",
                cam.eye[2],
                if cam.inbound { "in" } else { "out" },
                luma[i],
                zone.weight(cam.eye)
            );
        }
    }
    assert!(
        worst <= FLASH_BOUND_WITH_ZONE,
        "step {worst:.1} at frame {at} > {FLASH_BOUND_WITH_ZONE}"
    );
}

/// How much the exposure zone must lower the worst step entering the tube against the same
/// drive without it, levels (0-255). Measured on lavapipe at 10 fps (ADR-3257, before ADR-7975's
/// baked interior shade): entering 15.4 with the zone, 20.1 without; leaving 26.4 and 28.3; the
/// whole drive 26.4 and 28.3. Since ADR-8629 (the cook's band, the drive to
/// [`ZONE_DRIVE_TURN_Z`]): entering 14.9 with, 20.2 without; leaving 13.3 and 11.8; the whole
/// drive 14.9 and 20.2.
const ZONE_ENTRY_GAIN: f32 = 3.0;

/// Where the zone drive turns around (z, metres): 60 m in, deeper than the shared path's
/// [`scene::PATH_TURN_Z`] (ADR-8629). Since the interior shade reads the tube's bake (ADR-7975)
/// the tube 40 m in meters about EV 7.7, within a stop of the zone's band, so the band did not
/// hold the eye there and the turnaround measured the plain meter's lag, not the zone (27.5
/// against 20.2 levels at the turnaround's first frame). From about 44 m in the tube meters
/// under the band (EV 7.2 at 60 m) and the zone holds the eye on it, the case a tunnel zone is
/// for (a dark tube under a sunlit mouth, ADR-257); the test asserts that it does.
/// `REGIA_TUNNEL_TURN_Z` overrides it (by hand, to compare depths).
const ZONE_DRIVE_TURN_Z: f32 = 60.0;

fn zone_drive_turn_z() -> f32 {
    regia_core::brand::env_var("REGIA_TUNNEL_TURN_Z")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ZONE_DRIVE_TURN_Z)
}

/// One drive of `slots` along `path`: mean luma and the exposure report `(ev, goal, meter, mode)`
/// per frame.
type ZoneDrive = (Vec<f32>, Vec<Option<(f32, f32, f32, &'static str)>>);

fn zone_drive(slots: scene::Slots, path: &[CamFrame]) -> ZoneDrive {
    let mut scene = scene::TunnelScene::new(Variant::On, slots, 0.0, true);
    scene.dt = 1.0 / fps();
    scene.warm_up(&path[0]);
    let mut luma = Vec::new();
    let mut exposure = Vec::new();
    for cam in path {
        luma.push(measure::mean_luma(&scene.render(cam)));
        exposure.push(
            scene
                .renderer()
                .exposure_zones()
                .report()
                .map(|r| (r.ev, r.goal, r.meter_ev, r.mode.name())),
        );
    }
    (luma, exposure)
}

/// The zone drive: the path to [`ZONE_DRIVE_TURN_Z`], the drive with every slot filled and the
/// same drive without the exposure zone; rendered once for the zone tests.
struct ZoneDrives {
    path: Vec<CamFrame>,
    with: ZoneDrive,
    without: ZoneDrive,
}

fn zone_drives() -> &'static ZoneDrives {
    static DRIVES: OnceLock<ZoneDrives> = OnceLock::new();
    DRIVES.get_or_init(|| {
        let path = scene::camera_path_to(SPEED, fps(), zone_drive_turn_z());
        let with = zone_drive(scene::Slots::today(), &path);
        let mut unzoned = scene::Slots::today();
        unzoned.exposure_zone.fill = None;
        let without = zone_drive(unzoned, &path);
        ZoneDrives { path, with, without }
    })
}

#[test]
fn exposure_zone_eases_the_mouth() {
    // Against the same drive with every slot filled but the exposure zone: without it the drive
    // flashes harder entering the tube, and no less over the whole drive. Both drives turn around
    // at `ZONE_DRIVE_TURN_Z`.
    let slots = scene::Slots::today();
    assert!(slots.exposure_zone.is_filled(), "no exposure zone plugged in");
    let drives = zone_drives();
    let path = &drives.path;
    let (with_luma, with_exposure) = &drives.with;
    let (luma, bare_exposure) = &drives.without;
    // Per frame, both drives (printed; `--nocapture` shows it): luma, the eye's EV, its goal and
    // the meter.
    let zone = fills::TubeExposureZone::new();
    let line = |e: Option<(f32, f32, f32, &str)>| {
        e.map(|(ev, goal, meter, mode)| format!("ev {ev:.2} goal {goal:.2} meter {meter:.2} {mode}"))
            .unwrap_or_default()
    };
    for (i, cam) in path.iter().enumerate() {
        eprintln!(
            "frame {i} z {:.1} {} weight {:.2}: with luma {:.1} {}; without luma {:.1} {}",
            cam.eye[2],
            if cam.inbound { "in" } else { "out" },
            zone.weight(cam.eye),
            with_luma[i],
            line(with_exposure[i]),
            luma[i],
            line(bare_exposure[i])
        );
    }
    // The drive reaches the section the zone is for: at the turnaround the plain meter sits under
    // the zone's band and the zone holds the eye on the band (if the scene's lighting moves the
    // meter into the band, the steps below stop measuring the zone; ADR-8629).
    let turn_z = path.iter().map(|c| c.eye[2]).fold(f32::MIN, f32::max);
    let (lo, hi) = (zone.zone.ev_min, zone.zone.ev_max);
    let tol = regia_render::exposure_zones::EXPOSURE_SETTLED_TOLERANCE_EV;
    let mut turned = 0;
    for (i, cam) in path.iter().enumerate() {
        if cam.eye[2] != turn_z || !zone.contains(cam.eye) {
            continue;
        }
        turned += 1;
        let ev = with_exposure[i]
            .map(|e| e.0)
            .expect("a zone frame reports its exposure");
        let bare = bare_exposure[i]
            .map(|e| e.0)
            .expect("a frame reports its exposure");
        assert!(
            (lo - tol..=hi + tol).contains(&ev),
            "frame {i} at the turnaround: the zone holds the eye at EV {ev:.2}, outside its band {lo}..{hi}"
        );
        assert!(
            bare < lo,
            "frame {i} at the turnaround: without the zone the eye is at EV {bare:.2}, not under the band {lo}..{hi}: \
             the drive no longer reaches a section darker than the zone asks"
        );
    }
    assert!(
        turned >= 8,
        "the turnaround must be inside the zone: {turned} frames"
    );
    // Worst step entering (inbound frames up to 10 m in) and leaving (outbound frames from 10 m
    // in), and over the whole drive.
    let worst = |l: &[f32], keep: &dyn Fn(&CamFrame) -> bool| {
        l.windows(2)
            .enumerate()
            .filter(|(i, _)| keep(&path[i + 1]))
            .map(|(i, w)| ((w[1] - w[0]).abs(), i + 1))
            .fold((0.0f32, 0), |a, b| if b.0 > a.0 { b } else { a })
    };
    let entering = |c: &CamFrame| c.inbound && c.eye[2] <= 10.0;
    let leaving = |c: &CamFrame| !c.inbound && c.eye[2] <= 10.0;
    let all = |_: &CamFrame| true;
    for (name, l) in [("with the zone", with_luma), ("without", luma)] {
        eprintln!(
            "exposure zone, {name}: worst step entering {:?}, leaving {:?}, whole drive {:?} (levels, frame)",
            worst(l, &entering),
            worst(l, &leaving),
            worst(l, &all)
        );
    }
    let (with, _) = worst(with_luma, &all);
    let (without, _) = worst(luma, &all);
    let (with_in, _) = worst(with_luma, &entering);
    let (without_in, _) = worst(luma, &entering);
    assert!(with <= FLASH_BOUND, "step {with:.1} > {FLASH_BOUND}");
    assert!(
        with_in + ZONE_ENTRY_GAIN <= without_in,
        "the zone must ease the entry: {with_in:.1} with, {without_in:.1} without"
    );
    assert!(
        with <= without,
        "the zone must not make the drive's worst step worse: {with:.1} with, {without:.1} without"
    );
}

#[test]
fn reverb_zone_covers_the_tube() {
    let slots = scene::Slots::today();
    assert!(slots.reverb_zone.is_filled(), "no reverb zone plugged in");
    // The zones as the audio sync places them (the audio lane asserts what the preset sounds
    // like; the scene places the zone).
    let mut world = scene::tunnel_world(&slots);
    let zones = regia_audio::placed_zones(&mut world);
    assert_eq!(zones.len(), 1, "one reverb zone over the tube");
    let placed = &zones[0].1;
    assert_eq!(placed.zone.preset, regia_audio::ReverbPreset::Tube);
    let interior = scene::interior_volume();
    let c = interior.center;
    let h = interior.half_extent;
    // Every corner of the tube (1 cm in), the middle of each face and the centre are inside.
    for sx in [-1.0f32, 0.0, 1.0] {
        for sy in [-1.0f32, 0.0, 1.0] {
            for sz in [-1.0f32, 0.0, 1.0] {
                let p = Vec3::new(
                    c[0] + sx * (h[0] - 0.01),
                    c[1] + sy * (h[1] - 0.01),
                    c[2] + sz * (h[2] - 0.01),
                );
                assert!(placed.contains(p), "{p} is in the tube but not in the zone");
                assert_eq!(placed.weight(p), 1.0);
            }
        }
    }
    // Along the drive: fully wet wherever the listener is in the tube, dry once it is further
    // out than the zone's fade.
    let fade = placed.zone.fade_distance;
    let (mut wet, mut dry) = (0, 0);
    for cam in camera_path(SPEED, fps()) {
        let eye = Vec3::from_array(cam.eye);
        let outside = (0..3)
            .map(|a| ((cam.eye[a] - c[a]).abs() - h[a]).max(0.0))
            .map(|d| d * d)
            .sum::<f32>()
            .sqrt();
        if interior.contains(cam.eye) {
            assert_eq!(
                placed.weight(eye),
                1.0,
                "listener in the tube at z {:.1}",
                cam.eye[2]
            );
            wet += 1;
        } else if outside > fade {
            assert_eq!(
                placed.weight(eye),
                0.0,
                "listener {outside:.1} m outside at z {:.1}",
                cam.eye[2]
            );
            dry += 1;
        }
    }
    assert!(
        wet >= 10 && dry >= 10,
        "the drive must cover both: {wet} wet, {dry} dry"
    );
    // The old scene places none.
    assert!(regia_audio::placed_zones(&mut scene::tunnel_world(&scene::Slots::none())).is_empty());
}

/// Exposure of the probe-volume plates (fixed, EV100): the tube lit by its interior floor and
/// bounce only.
const PROBE_EV: f32 = 5.0;

#[test]
fn probe_volume_lifts_the_tube_interior() {
    let slots = scene::Slots::today();
    assert!(slots.probe_volume.is_filled(), "no probe volume plugged in");
    // Deep in the tube, looking back toward the mouth; no headlight, no fog, fixed exposure.
    //
    // With the volume the tube is a baked tunnel: the bake holds what the sky and the sun's
    // bounce reach, so the interior shade is off (the forward pass's interior-shade branch adds
    // the legacy probe grid to its flat floor, not the baked volume: ADR-3257). Without it the
    // tube is today's interior shade: the flat interior floor and no GI.
    let cam = scene::CamFrame {
        index: 0,
        eye: [0.0, scene::FLOOR_Y + 1.6, 30.0],
        target: [0.0, scene::FLOOR_Y + 0.6, 16.0],
        time: 0.0,
        inbound: false,
    };
    let fixed = |_: &scene::TunnelGeometry, p: &mut scene::TunnelParts| {
        p.lab.local_lights.clear();
        p.outdoor.lights.clear();
        p.outdoor.volumes.clear();
        p.exposure_target = None;
        p.post.auto_exposure = false;
        p.post.exposure_ev = PROBE_EV;
        p.lab.exposure_ev = PROBE_EV;
    };
    let baked = |g: &scene::TunnelGeometry, p: &mut scene::TunnelParts| {
        fixed(g, p);
        p.outdoor.interiors.clear();
        p.quality.interior_ambient = false;
    };
    // Without: no GI beyond the interior's floor light (no volume, no DDGI, no SSGI).
    let bare = |g: &scene::TunnelGeometry, p: &mut scene::TunnelParts| {
        fixed(g, p);
        p.baked_gi = None;
        p.quality.baked_gi = false;
        p.quality.gi = regia_render::GiMode::Ambient;
        p.quality.gi_probes = false;
        p.quality.screen_space_gi = false;
    };
    let mut with = scene::TunnelScene::new(scene::Variant::On, scene::Slots::today(), 0.0, true);
    let mut without = scene::TunnelScene::new(scene::Variant::On, scene::Slots::today(), 0.0, true);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..PLATE_FRAMES {
        a = with.render_tweaked(&cam, &baked);
        b = without.render_tweaked(&cam, &bare);
    }
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(&dir.join("probe_volume_with.png"), scene::W, scene::H, &a);
        dump::write_png(&dir.join("probe_volume_without.png"), scene::W, scene::H, &b);
    }
    // The floor deeper than the mouth: past the sun's reach under the portal (z 4) and the
    // first probe planes, up to 14 m from the camera.
    let (z0, z1) = (6.0, 16.0);
    let (mut lit, mut bare_floor) = (Vec::new(), Vec::new());
    for y in 0..scene::H {
        for x in 0..scene::W {
            let (o, d) = oracle::pixel_ray(&cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let t = (scene::FLOOR_Y - o.y) / d.y;
            let p = o + d * t;
            if p.x.abs() < scene::HALF_WIDTH - 1.0 && (z0..z1).contains(&p.z) {
                lit.push(oracle::luma_at(&a, x, y));
                bare_floor.push(oracle::luma_at(&b, x, y));
            }
        }
    }
    let (with_luma, without_luma) = (oracle::mean(&lit), oracle::mean(&bare_floor));
    eprintln!(
        "probe volume: floor z {z0}..{z1} ({} px) mean luma {with_luma:.2} with the volume, {without_luma:.2} without",
        lit.len()
    );
    // The CPU mirror of the lookup: the volume covers the floor and carries bounce there.
    let renderer = with.renderer();
    let mut irradiance = Vec::new();
    for z in [2.0f32, 6.0, 10.0, 14.0, 18.0] {
        let s = renderer.sample_baked_gi([0.0, scene::FLOOR_Y + 0.05, z], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]);
        let l = 0.2126 * s.irradiance_over_pi[0]
            + 0.7152 * s.irradiance_over_pi[1]
            + 0.0722 * s.irradiance_over_pi[2];
        eprintln!(
            "probe volume: floor z {z}: coverage {:.2}, irradiance/pi {l:.3} {:?}",
            s.coverage, s.components
        );
        assert!(
            s.coverage > 0.99,
            "the volume must cover the floor at z {z}: {}",
            s.coverage
        );
        irradiance.push(l);
    }
    assert!(
        lit.len() >= 400,
        "the floor patch must be in view: {} px",
        lit.len()
    );
    assert!(
        irradiance.iter().all(|l| *l > 0.0),
        "the baked floor must carry bounce at every depth: {irradiance:?}"
    );
    assert!(
        with_luma >= without_luma + 3.0,
        "the volume must lift the deep floor: {with_luma:.2} with, {without_luma:.2} without"
    );
}

/// Frames each controlled plate renders before it is read (the fog history and exposure settle
/// on a still view).
const PLATE_FRAMES: usize = 6;
/// Exposure of the fog plates, EV100: the sunlit ground and the sky stay under white.
const FOG_EV: f32 = 13.0;
/// Pixels around the interior's screen box the froxel grid may blur fog into.
const FROXEL_MARGIN_PX: f32 = 24.0;

fn fog_plate(
    scene: &mut scene::TunnelScene,
    cam: &scene::CamFrame,
    volumes: &[regia_render::FogVolume],
    black: bool,
) -> Vec<u8> {
    let volumes = volumes.to_vec();
    let tweak = move |_: &scene::TunnelGeometry, p: &mut scene::TunnelParts| {
        p.lab.local_lights.clear();
        p.outdoor.lights.clear();
        p.outdoor.volumes = volumes.clone();
        p.exposure_target = None;
        p.post.auto_exposure = false;
        p.post.exposure_ev = FOG_EV;
        p.lab.exposure_ev = FOG_EV;
        if black {
            for m in &mut p.lab.materials {
                m.base_color = [0.0, 0.0, 0.0, 1.0];
                m.unlit = true;
            }
        }
    };
    let mut px = Vec::new();
    for _ in 0..PLATE_FRAMES {
        px = scene.render_tweaked(cam, &tweak);
    }
    px
}

#[test]
fn interior_fog_stays_in_the_tube() {
    let slots = scene::Slots::today();
    assert!(slots.interior_fog.is_filled(), "no interior fog plugged in");
    // What the filler adds, on its own.
    let mut probe = scene::tunnel_scene(scene::Variant::On);
    let cam = scene::CamFrame {
        index: 0,
        eye: [0.0, 1.6, -45.0],
        target: [0.0, -2.0, 0.0],
        time: 0.0,
        inbound: true,
    };
    let (geometry, mut parts) = probe.base_parts(&cam);
    parts.outdoor.volumes.clear();
    if let Some(fill) = &slots.interior_fog.fill {
        fill.apply(&geometry, &mut parts);
    }
    let fog = parts.outdoor.volumes;
    assert!(!fog.is_empty(), "the interior fog adds a volume");
    let interior = scene::interior_volume();
    let tris = oracle::tube_triangles();
    // Readback: the frame with the interior fog against the same frame without fog.
    let none = fog_plate(&mut probe, &cam, &[], false);
    let mut fogged_scene = scene::tunnel_scene(scene::Variant::On);
    let fogged = fog_plate(&mut fogged_scene, &cam, &fog, false);
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(&dir.join("interior_fog_off.png"), scene::W, scene::H, &none);
        dump::write_png(&dir.join("interior_fog_on.png"), scene::W, scene::H, &fogged);
    }
    // The interior's screen box (all eight corners are in front of this camera).
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for sx in [-1.0f32, 1.0] {
        for sy in [-1.0f32, 1.0] {
            for sz in [-1.0f32, 1.0] {
                let corner = Vec3::new(
                    interior.center[0] + sx * interior.half_extent[0],
                    interior.center[1] + sy * interior.half_extent[1],
                    interior.center[2] + sz * interior.half_extent[2],
                );
                let s = oracle::project(&cam, corner).expect("the tube is in front of the camera");
                for a in 0..2 {
                    lo[a] = lo[a].min(s[a] - FROXEL_MARGIN_PX);
                    hi[a] = hi[a].max(s[a] + FROXEL_MARGIN_PX);
                }
            }
        }
    }
    let mut outside = Vec::new();
    let mut worst_outside = 0.0f32;
    for y in 0..scene::H {
        for x in 0..scene::W {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if fx < lo[0] || fx > hi[0] || fy < lo[1] || fy > hi[1] {
                let diff = (oracle::luma_at(&fogged, x, y) - oracle::luma_at(&none, x, y)).abs();
                outside.push(diff);
                worst_outside = worst_outside.max(diff);
            }
        }
    }
    let mean_out = oracle::mean(&outside);
    // Inside, from the ramp 8 m before the portal: the interior's air the sun reaches under the
    // portal (the sky does not light air in an interior volume, and the headlight is off) must
    // show the fog.
    let ramp = camera_path(SPEED, fps())
        .into_iter()
        .find(|c| c.inbound && c.eye[2] >= -8.0)
        .expect("ramp frame");
    let mut ramp_none_scene = scene::tunnel_scene(scene::Variant::On);
    let ramp_none = fog_plate(&mut ramp_none_scene, &ramp, &[], false);
    let mut ramp_fog_scene = scene::tunnel_scene(scene::Variant::On);
    let ramp_fog = fog_plate(&mut ramp_fog_scene, &ramp, &fog, false);
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(
            &dir.join("interior_fog_ramp_off.png"),
            scene::W,
            scene::H,
            &ramp_none,
        );
        dump::write_png(
            &dir.join("interior_fog_ramp_on.png"),
            scene::W,
            scene::H,
            &ramp_fog,
        );
    }
    let to_sun = -Vec3::from_array(parts.lab.directional.direction).normalize();
    let rays = oracle::FogRays {
        tris: &tris,
        volumes: &fog,
        to_sun,
    };
    let mut inside = Vec::new();
    for y in 0..scene::H {
        for x in 0..scene::W {
            let (o, d) = oracle::pixel_ray(&ramp, x, y);
            let Some(t) = oracle::first_hit(&tris, o, d) else {
                continue;
            };
            let (lit, _) = rays.along(o, d, t, &|p| interior.contains(p.to_array()));
            if lit >= fills::INTERIOR_FOG_DENSITY * 1.5 {
                inside.push((oracle::luma_at(&ramp_fog, x, y) - oracle::luma_at(&ramp_none, x, y)).abs());
            }
        }
    }
    let mean_in = oracle::mean(&inside);
    eprintln!(
        "interior fog: outside the interior's screen box {} px, mean |diff| {mean_out:.3}, worst {worst_outside:.1}; \
         sunlit interior air seen from the ramp {} px, mean |diff| {mean_in:.2}",
        outside.len(),
        inside.len()
    );
    assert!(
        outside.len() > 20_000,
        "most of the frame is outside the tube: {}",
        outside.len()
    );
    assert!(
        worst_outside <= 2.0 && mean_out <= 0.05,
        "fog outside the interior: mean {mean_out:.3}, worst {worst_outside:.1} levels"
    );
    assert!(
        inside.len() >= 50,
        "the sunlit interior air must be in view: {} px",
        inside.len()
    );
    assert!(
        mean_in >= 1.5,
        "the interior must hold fog: mean |diff| {mean_in:.2} levels"
    );
}

#[test]
fn light_shafts_fall_through_the_cut() {
    let slots = scene::Slots::today();
    assert!(slots.light_shafts.is_filled(), "no light shafts plugged in");
    // On the ramp 8 m before the portal, looking down the road into the tube. Every tube
    // surface is black and the headlight is off, so what a pixel shows over the black is air.
    let cam = camera_path(SPEED, fps())
        .into_iter()
        .find(|c| c.inbound && c.eye[2] >= -8.0)
        .expect("ramp frame");
    let mut base = scene::tunnel_scene(scene::Variant::On);
    let (_, parts) = base.parts(&cam);
    let interior = scene::interior_volume();
    // The cut's air (the volumes outside the tube) and the tube's air (inside it).
    let (tube_air, cut_air): (Vec<_>, Vec<_>) = parts.outdoor.volumes.iter().copied().partition(|v| {
        let regia_render::FogVolume::Box { center, .. } = *v else {
            return false;
        };
        interior.contains(center)
    });
    assert!(!cut_air.is_empty() && !tube_air.is_empty());
    let all_air: Vec<_> = cut_air.iter().chain(&tube_air).copied().collect();
    let r0 = fog_plate(&mut base, &cam, &[], true);
    let mut cut_scene = scene::tunnel_scene(scene::Variant::On);
    let r_cut = fog_plate(&mut cut_scene, &cam, &cut_air, true);
    let mut all_scene = scene::tunnel_scene(scene::Variant::On);
    let r_all = fog_plate(&mut all_scene, &cam, &all_air, true);
    if let Some(dir) = dump::dump_dir() {
        dump::write_png(&dir.join("shaft_no_fog.png"), scene::W, scene::H, &r0);
        dump::write_png(&dir.join("shaft_cut_fog.png"), scene::W, scene::H, &r_cut);
        dump::write_png(&dir.join("shaft_all_fog.png"), scene::W, scene::H, &r_all);
    }
    let tris = oracle::tube_triangles();
    let to_sun = -Vec3::from_array(parts.lab.directional.direction).normalize();
    let cut_rays = oracle::FogRays {
        tris: &tris,
        volumes: &cut_air,
        to_sun,
    };
    let tube_rays = oracle::FogRays {
        tris: &tris,
        volumes: &tube_air,
        to_sun,
    };
    // Per pixel that ends on a (black) tube surface: the sunlit and shadowed air of the cut and
    // of the tube along the view ray (optical depths).
    let unit = fills::INTERIOR_FOG_DENSITY;
    let (mut cut_lit, mut wedge, mut shadow) = (Group::default(), Group::default(), Group::default());
    for y in (0..scene::H).step_by(2) {
        for x in (0..scene::W).step_by(2) {
            let (o, d) = oracle::pixel_ray(&cam, x, y);
            let Some(t) = oracle::first_hit(&tris, o, d) else {
                continue;
            };
            let in_tube = |p: Vec3| interior.contains(p.to_array());
            let (c_lit, c_shadow) = cut_rays.along(o, d, t, &|p| !in_tube(p));
            let (t_lit, t_shadow) = tube_rays.along(o, d, t, &in_tube);
            let gain_cut = oracle::luma_at(&r_cut, x, y) - oracle::luma_at(&r0, x, y);
            let gain_tube = oracle::luma_at(&r_all, x, y) - oracle::luma_at(&r_cut, x, y);
            // The cut's sunlit air glows over black: rays through 4 m or more of it, no shadow.
            if c_lit >= 0.02 * 4.0 && c_shadow <= 0.02 * 0.25 {
                cut_lit.push(gain_cut, c_lit, c_shadow);
            }
            // Under the roof: rays through the sunlit wedge below the portal, and rays through
            // long shadowed air (every ray into the tube crosses a little of the wedge).
            if t_lit >= unit * 1.5 && t_shadow <= t_lit {
                wedge.push(gain_tube, t_lit, t_shadow);
            } else if t_shadow >= unit * 12.0 && t_lit * 4.0 <= t_shadow {
                shadow.push(gain_tube, t_lit, t_shadow);
            }
        }
    }
    eprintln!(
        "light shafts ({:?}): sunlit cut air {cut_lit}; sunlit air under the portal {wedge}; shadowed tube air {shadow}",
        fills::shaft_rule()
    );
    assert!(
        cut_lit.n >= 100 && wedge.n >= 20 && shadow.n >= 20,
        "the frame must see all three: {} / {} / {}",
        cut_lit.n,
        wedge.n,
        shadow.n
    );
    assert!(
        cut_lit.gain() >= 4.0,
        "the sunlit air of the cut must glow over black: {:.2} levels",
        cut_lit.gain()
    );
    assert!(
        wedge.gain() >= 2.0,
        "the shaft must reach under the portal: {:.2} levels",
        wedge.gain()
    );
    // Scaled by the sunlit air each group crosses (from the wedge's gain per unit of sunlit
    // optical depth), the shadowed group may glow from its own sunlit air plus at most
    // `SHADOWED_AIR_GLOW` of its shadowed air; a fog that lit the air under the roof would glow
    // by all of it (printed).
    let per_lit = wedge.gain() / wedge.lit();
    let bound = per_lit * (shadow.lit() + SHADOWED_AIR_GLOW * shadow.shadowed());
    eprintln!(
        "light shafts: shadowed group gain {:.2}, bound {bound:.2}, a fully lit roof's air would read {:.2}",
        shadow.gain(),
        per_lit * (shadow.lit() + shadow.shadowed())
    );
    assert!(
        shadow.gain() <= bound,
        "air the roof shadows glows: {:.2} levels > {bound:.2}",
        shadow.gain()
    );
}

/// The share of the roof-shadowed air the shaft test lets glow. The froxel grid's depth slices
/// are metres deep at the portal, so a slice there mixes the sunlit wedge with shadowed air
/// behind it: measured on lavapipe, the `calibrated` sun-shadow rule (ADR-3122) glows by 9 % of
/// the shadowed air, the `legacy` rule by 35 % (it lets the sun into air under a roof).
const SHADOWED_AIR_GLOW: f32 = 0.15;

/// Pixels of one class in the shaft test: mean gain and mean optical depths.
#[derive(Default)]
struct Group {
    n: usize,
    gain: f32,
    lit: f32,
    shadowed: f32,
}

impl Group {
    fn push(&mut self, gain: f32, lit: f32, shadowed: f32) {
        self.n += 1;
        self.gain += gain;
        self.lit += lit;
        self.shadowed += shadowed;
    }

    fn mean(&self, sum: f32) -> f32 {
        if self.n == 0 { 0.0 } else { sum / self.n as f32 }
    }

    fn gain(&self) -> f32 {
        self.mean(self.gain)
    }

    fn lit(&self) -> f32 {
        self.mean(self.lit)
    }

    fn shadowed(&self) -> f32 {
        self.mean(self.shadowed)
    }
}

impl std::fmt::Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} px, gain {:.2} levels, sunlit depth {:.4}, shadowed depth {:.4}",
            self.n,
            self.gain(),
            self.lit(),
            self.shadowed()
        )
    }
}

#[test]
fn a_pick_ray_into_the_mouth_hits_the_tube() {
    let slots = scene::Slots::today();
    assert!(slots.world_pick.is_filled(), "no world pick layers plugged in");
    let world = scene::tunnel_world(&slots);
    let none = scene::tunnel_world(&scene::Slots::none());
    let tris = oracle::tube_triangles();
    let mask = scene::mouth_mask();
    // Rays through the pixels of an approach frame (the same view the cut detector judges).
    let cam = camera_path(SPEED, fps())
        .into_iter()
        .find(|c| c.eye[1] > 1.0 && c.eye[2] > -34.0)
        .expect("approach frame");
    let (mut into, mut ground) = (0, 0);
    for y in (0..scene::H).step_by(3) {
        for x in (0..scene::W).step_by(3) {
            let (o, d) = oracle::pixel_ray(&cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let g = o + d * (-o.y / d.y);
            if g.length() > 100.0 {
                continue;
            }
            // Inside the cut by half a metre, or outside it by half a metre.
            let deep_in = [[0.5f32, 0.0], [-0.5, 0.0], [0.0, 0.5], [0.0, -0.5]]
                .iter()
                .all(|e| mask.contains(g.x + e[0], g.z + e[1]));
            let clear_out = [[0.5f32, 0.0], [-0.5, 0.0], [0.0, 0.5], [0.0, -0.5]]
                .iter()
                .all(|e| !mask.contains(g.x + e[0], g.z + e[1]));
            let ray = regia_core::math::Ray::new(o, d);
            let pick = regia_pick::world_pick(&world, ray, &regia_pick::PickFilter::default());
            if deep_in {
                let pick = pick.unwrap_or_else(|| panic!("a ray into the mouth at {g} hit nothing"));
                assert_eq!(
                    pick.surface,
                    regia_pick::PickSurface::Mesh,
                    "into the mouth at {g}: {pick:?}"
                );
                assert_eq!(pick.layer, regia_pick::MapLayer(fills::TUBE_LAYER));
                assert!(pick.point.y < 0.0, "the hit is below the ground: {pick:?}");
                let t = oracle::first_hit(&tris, o, d).expect("the oracle sees the tube");
                assert!(
                    (pick.distance - t).abs() < 0.01,
                    "pick {:.3} m, triangle oracle {t:.3} m",
                    pick.distance
                );
                // On the terrain layer alone the ray falls through the hole.
                assert!(
                    regia_pick::world_pick(&world, ray, &regia_pick::PickFilter::on_layers(&[0])).is_none(),
                    "the terrain layer must be open at {g}"
                );
                // Without the tube's pick layer the mouth is empty.
                assert!(regia_pick::world_pick(&none, ray, &regia_pick::PickFilter::default()).is_none());
                into += 1;
            } else if clear_out && oracle::first_hit(&tris, o, d).is_none_or(|t| t > (g - o).length()) {
                let pick = pick.unwrap_or_else(|| panic!("a ray at the ground {g} hit nothing"));
                assert_eq!(
                    pick.surface,
                    regia_pick::PickSurface::Terrain,
                    "ground at {g}: {pick:?}"
                );
                assert_eq!(pick.layer, regia_pick::MapLayer(0));
                assert!(
                    pick.point.distance(g) < 0.05,
                    "terrain hit {} vs ground {g}",
                    pick.point
                );
                ground += 1;
            }
        }
    }
    eprintln!("world pick: {into} rays into the mouth hit the tube, {ground} rays hit the ground");
    assert!(
        into >= 100 && ground >= 100,
        "{into} into the mouth, {ground} at the ground"
    );
}
