// SPDX-License-Identifier: MPL-2.0
//! Plan row W-24 (ADR-8384): one exposure zone, one reverb zone and one interior volume per cooked section,
//! the exposure ease on the frame clock, and the depth blend on by default.

use regia_detsim::float::{det_clamp, det_max};
use regia_detsim::math;
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::layout::SampledPassage;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedSection, CookedZone, CookedZoneShape, WorldGeomCooked};
use regia_world_geom::passages::PassagesDocument;
use regia_world_geom::sections::open_skylight_shafts;
use regia_world_geom::zones::{
    EaseSettings, ExposureEase, EyeSample, band_exposure, band_target, interior_exposure_target,
    section_volumes, smoothstep, under_rock_weight,
};

const STEP_M: f32 = 2.0;
const DT_S: f32 = 0.25;
const OUTDOOR_EV: f32 = 11.5;
const INTERIOR_EV: f32 = 9.0;
const EYE_M: f32 = 1.65;
const GROUND_Y: f32 = -256.0;

fn canyon_terrain() -> TerrainSampler {
    let Some(grid) = HeightGrid::square(4, vec![8192; 16]) else {
        panic!("height grid");
    };
    TerrainSampler::new(grid, 1280.0).with_zero_counts(16384)
}

fn canyon_ground() -> GroundInput {
    GroundInput {
        lattice_n: 4,
        width_m: 1280.0,
        offset_m: -512.0,
        step_m: 1.0 / 32.0,
        counts: vec![8192; 16],
    }
}

fn cook_switchbacks() -> WorldGeomCooked {
    let Some(layout) = for_map("switchbacks") else {
        panic!("switchbacks fixture missing");
    };
    let passages = match PassagesDocument::from_layout(layout).to_ron() {
        Ok(text) => text,
        Err(err) => panic!("passages ron: {err:?}"),
    };
    let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"map.passages.ron\"), )])";
    let read = |p: &str| {
        if p == "map.passages.ron" {
            Ok(passages.as_bytes().to_vec())
        } else {
            Err(format!("{p}: not found"))
        }
    };
    let terrain = canyon_terrain();
    let ground = canyon_ground();
    let run = |document: &str| {
        cook(&CookInputs {
            document: document.as_bytes(),
            ground: &ground,
            terrain: Some(&terrain),
            read_file: &read,
        })
    };
    // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
    let out = match run(doc) {
        Ok(out) => out,
        Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
            match waive_ramp_facets(doc, &report).map(|waived| run(&waived)) {
                Ok(Ok(out)) => out,
                Ok(Err(err)) => panic!("cook: {err}"),
                Err(err) => panic!("ramp-facet waiver: {err}"),
            }
        }
        Err(err) => panic!("cook: {err}"),
    };
    out.file.canonical
}

fn eye_sample(floor: [f32; 3], covered: bool) -> EyeSample {
    let position = [floor[0], floor[1] + EYE_M, floor[2]];
    EyeSample {
        position,
        depth_m: GROUND_Y - position[1],
        open_cut: !covered,
        under_lid: covered,
    }
}

fn segment_len(a: [f32; 3], b: [f32; 3]) -> (f32, [f32; 3]) {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let len = math::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
    (len, d)
}

/// Surface to tube, one sample every 2 m, plus the tube end. Each sample carries its segment's cover.
fn resample_inward(passage: &SampledPassage) -> Vec<EyeSample> {
    let n = passage.samples.len();
    if n < 2 || passage.covered.is_empty() {
        return Vec::new();
    }
    let mut out = vec![eye_sample(passage.samples[n - 1].p, passage.covered[n - 2])];
    let mut since = 0.0_f32;
    for k in 0..(n - 1) {
        let from_i = n - 1 - k;
        let to_i = n - 2 - k;
        let a = passage.samples[from_i].p;
        let b = passage.samples[to_i].p;
        let covered = passage.covered[to_i];
        let (len, d) = segment_len(a, b);
        if len <= 1.0e-6 {
            continue;
        }
        let mut walked = 0.0_f32;
        while since + (len - walked) >= STEP_M - 1.0e-4 {
            let need = STEP_M - since;
            walked += need;
            since = 0.0;
            let t = det_clamp(walked / len, 0.0, 1.0);
            let p = [a[0] + d[0] * t, a[1] + d[1] * t, a[2] + d[2] * t];
            out.push(eye_sample(p, covered));
        }
        since += len - walked;
    }
    let end = passage.samples[0].p;
    let last = out[out.len() - 1].position;
    let floor_last = [last[0], last[1] - EYE_M, last[2]];
    let (gap, _) = segment_len(floor_last, end);
    if gap > 1.0e-3 {
        out.push(eye_sample(end, passage.covered[0]));
    }
    out
}

/// The shell's interior band, kept only in this test: weight 0 at or below 2 m, 1 under a lid or
/// outside an open cut, otherwise the 3-22 m smoothstep. A settled weight (0 or 1) or a floor that
/// moved by more than 0.1 EV is written in full, with no frame-clock ease.
fn shell_weight(sample: &EyeSample) -> f32 {
    if !sample.depth_m.is_finite() || sample.depth_m <= 2.0 {
        return 0.0;
    }
    if sample.under_lid || !sample.open_cut {
        return 1.0;
    }
    smoothstep(3.0, 22.0, sample.depth_m)
}

fn shell_write(prev: &mut Option<f32>, floor: f32, weight: f32) -> f32 {
    let Some(prior) = *prev else {
        *prev = Some(floor);
        return floor;
    };
    let settle = weight == 0.0 || weight == 1.0;
    if (settle && floor != prior) || (floor - prior).abs() > 0.1 {
        *prev = Some(floor);
        floor
    } else {
        prior
    }
}

fn max_step(values: &[f32]) -> f32 {
    values
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0_f32, det_max)
}

fn section_stub(id: u16, bounds: [[i32; 3]; 2]) -> CookedSection {
    CookedSection {
        id,
        source: format!("s{id}"),
        covered: true,
        bounds_mm: bounds,
        triangles: Vec::new(),
        portals: Vec::new(),
        blocker: None,
    }
}

/// Plan row W-24 (`drive_through_every_mouth_steps_le_half_ev`): a scripted eye walks into and back out of
/// every Switchbacks mouth at 2 m and 0.25 s. The product exposure moves at most 0.5 EV per sample. The
/// shell's band, which writes the interior floor in one sample when the lid closes, moves by more than
/// 0.5 EV on this same path. The plan's 11.5 and 9.0 floors are this test's arguments, not engine defaults.
#[test]
fn drive_through_every_mouth_steps_le_half_ev() {
    let Some(layout) = for_map("switchbacks") else {
        panic!("switchbacks fixture missing");
    };
    let terrain = canyon_terrain();
    let mut sampled = layout.sample(Some(&terrain));
    open_skylight_shafts(&mut sampled, &layout.openings);
    assert_eq!(layout.openings.len(), 4, "the fixture has four mouths");
    let settings = EaseSettings::default();
    let mut shell_worst = 0.0_f32;
    for opening in &layout.openings {
        let Some(passage) = sampled.passages.iter().find(|p| p.id == opening.ramp) else {
            panic!("ramp {} missing", opening.ramp);
        };
        let inward = resample_inward(passage);
        assert!(
            inward.len() > 4,
            "{} produced {} samples",
            opening.id,
            inward.len()
        );
        assert!(
            inward.iter().any(|s| s.under_lid) && inward.iter().any(|s| !s.under_lid),
            "{} does not cross a lid",
            opening.id
        );
        let mut path = inward.clone();
        let back = inward.iter().rev().skip(1).copied();
        path.extend(back);
        let mut ease = ExposureEase::new(settings);
        let mut product = Vec::with_capacity(path.len());
        let mut shell_prev = None;
        let mut shell = Vec::with_capacity(path.len());
        for sample in &path {
            let target = band_target(sample, OUTDOOR_EV, INTERIOR_EV, &settings);
            product.push(ease.step(target, DT_S));
            let weight = shell_weight(sample);
            let floor = band_exposure(OUTDOOR_EV, INTERIOR_EV, weight);
            shell.push(shell_write(&mut shell_prev, floor, weight));
        }
        let product_max = max_step(&product);
        let shell_max = max_step(&shell);
        assert!(
            product.iter().all(|ev| ev.is_finite()),
            "{} produced a non-finite exposure",
            opening.id
        );
        assert!(
            product_max <= 0.5,
            "{id}: product step {product_max} EV over 0.5 (shell {shell_max}, {} samples)",
            path.len(),
            id = opening.id
        );
        eprintln!(
            "{id}: product {product_max} EV, shell {shell_max} EV, {} samples",
            path.len(),
            id = opening.id
        );
        shell_worst = det_max(shell_worst, shell_max);
    }
    assert!(
        shell_worst > 0.5,
        "the shell band stayed within 0.5 EV (worst {shell_worst}); the fixture no longer shows the step"
    );
}

/// The ease advances only by the `dt` it is given. A fixed sequence matches run to run. A frozen frame
/// clock (`dt` 0) holds, including after the target changes. One 1/30 s step toward a floor 2.5 EV away
/// moves by less than 0.5 EV and does not snap.
#[test]
fn ease_uses_frame_time_not_wall_time() {
    let seq = [0.0_f32, 1.0 / 30.0, 0.1, 0.25, 1.0, 2.0];
    let run = || {
        let mut ease = ExposureEase::new(EaseSettings::default());
        let mut out = Vec::with_capacity(seq.len());
        for (i, dt) in seq.iter().enumerate() {
            let target = if i < 3 { OUTDOOR_EV } else { INTERIOR_EV };
            out.push(ease.step(target, *dt));
        }
        out
    };
    let first = run();
    let second = run();
    assert_eq!(first, second, "the same dt sequence must match bit for bit");
    assert_eq!(
        first[0], OUTDOOR_EV,
        "the first sample takes the target even at dt 0"
    );

    let mut frozen = ExposureEase::new(EaseSettings::default());
    assert_eq!(frozen.step(OUTDOOR_EV, 0.0), OUTDOOR_EV);
    assert_eq!(
        frozen.step(INTERIOR_EV, 0.0),
        OUTDOOR_EV,
        "dt 0 holds across a target change"
    );
    assert_eq!(
        frozen.step(INTERIOR_EV, f32::NAN),
        OUTDOOR_EV,
        "a non-finite dt holds"
    );
    assert_eq!(frozen.value(), Some(OUTDOOR_EV));

    let mut frame = ExposureEase::new(EaseSettings::default());
    assert_eq!(frame.step(OUTDOOR_EV, 0.0), OUTDOOR_EV);
    let next = frame.step(INTERIOR_EV, 1.0 / 30.0);
    let moved = OUTDOOR_EV - next;
    assert!(moved > 0.0 && moved <= 0.5, "one frame moved {moved} EV");
    assert!(
        next > INTERIOR_EV,
        "one frame snapped to the interior floor ({next})"
    );

    let mut snap = ExposureEase::new(EaseSettings {
        half_life_s: 0.0,
        ..EaseSettings::default()
    });
    assert_eq!(snap.step(OUTDOOR_EV, 0.0), OUTDOOR_EV);
    assert_eq!(
        snap.step(INTERIOR_EV, 0.1),
        INTERIOR_EV,
        "a non-positive half-life snaps"
    );
}

/// Plan row W-24 (`thirty_five_sections_give_thirty_five_volumes`, the plan's name kept, as
/// `switchbacks_cuts_35_sections` keeps it): the Switchbacks cook's section count is 37 (spec 121 § 5.6),
/// and each section produces one exposure band, one reverb preset and one interior volume.
#[test]
fn thirty_five_sections_give_thirty_five_volumes() {
    let cooked = cook_switchbacks();
    let volumes = section_volumes(&cooked.sections, &cooked.zones);
    assert_eq!(cooked.sections.len(), 37, "W-08's Switchbacks count");
    assert_eq!(volumes.len(), cooked.sections.len());
    assert!(volumes.len() > 4, "the bake emitted only four volumes");
    assert_eq!(cooked.zones.len(), cooked.sections.len());
    for (section, volume) in cooked.sections.iter().zip(&volumes) {
        assert_eq!(volume.section, section.id);
        let Some(zone) = cooked.zones.iter().find(|zone| zone.section == section.id) else {
            panic!("section {} has no zone", section.id);
        };
        assert_eq!(volume.exposure_ev, [zone.exposure[0], zone.exposure[1]]);
        assert_eq!(volume.blend_m, zone.exposure[2]);
        assert_eq!(volume.reverb_preset, zone.reverb_preset);
        assert!(
            volume.reverb_size_m > 0.0,
            "section {} has no reverb size",
            section.id
        );
        assert!(
            volume.interior_half_m.iter().all(|half| *half > 0.0),
            "section {} has an empty interior",
            section.id
        );
    }
}

/// Eight sections, listed with their zones out of order and one zone missing, still produce eight volumes.
#[test]
fn no_four_volume_cap() {
    let bounds = [[0, 0, 0], [2_000, 2_000, 2_000]];
    let sections: Vec<CookedSection> = (0..8).map(|id| section_stub(id, bounds)).collect();
    let mut zones = Vec::new();
    for id in [7_u16, 6, 5, 4, 3, 2, 0] {
        let shape = if id == 7 {
            CookedZoneShape::Capsule {
                a_mm: [0, 0, 0],
                b_mm: [4_000, 0, 0],
                radius_mm: 1_000,
            }
        } else if id == 6 {
            CookedZoneShape::OrientedBox {
                center_mm: [0, 0, 0],
                half_mm: [1_000, 2_000, 3_000],
                yaw: std::f32::consts::FRAC_PI_2,
            }
        } else {
            CookedZoneShape::OrientedBox {
                center_mm: [i32::from(id) * 1_000, 0, 0],
                half_mm: [500, 500, 500],
                yaw: 0.0,
            }
        };
        zones.push(CookedZone {
            section: id,
            shape,
            exposure: [8.5, 10.5, 25.0],
            reverb_preset: "tube".to_string(),
            reverb_size_m: 12.0,
        });
    }
    let volumes = section_volumes(&sections, &zones);
    assert_eq!(volumes.len(), 8);
    assert!(volumes.len() > 4);
    assert_eq!(
        volumes.iter().map(|volume| volume.section).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5, 6, 7]
    );
    let missing = &volumes[1];
    assert_eq!(missing.reverb_preset, "");
    assert_eq!(missing.exposure_ev, [0.0, 0.0]);
    assert!(
        (missing.interior_center_m[0] - 1.0).abs() < 1.0e-4,
        "bounds centre {}",
        missing.interior_center_m[0]
    );
    let yawed = &volumes[6];
    assert!(
        (yawed.interior_half_m[0] - 3.0).abs() < 1.0e-4,
        "x {}",
        yawed.interior_half_m[0]
    );
    assert!(
        (yawed.interior_half_m[1] - 2.0).abs() < 1.0e-4,
        "y {}",
        yawed.interior_half_m[1]
    );
    assert!(
        (yawed.interior_half_m[2] - 1.0).abs() < 1.0e-4,
        "z {}",
        yawed.interior_half_m[2]
    );
    let capsule = &volumes[7];
    assert!((capsule.interior_center_m[0] - 2.0).abs() < 1.0e-4);
    assert!((capsule.interior_half_m[0] - 3.0).abs() < 1.0e-4);
    assert!((capsule.interior_half_m[1] - 1.0).abs() < 1.0e-4);
    assert!((capsule.interior_half_m[2] - 1.0).abs() < 1.0e-4);
}

/// Depth blend defaults on, so a lid at 10 m of overburden is a partial weight, not a step to 1.
#[test]
fn depth_blend_is_on_by_default() {
    let on = EaseSettings::default();
    assert!(on.depth_blend);
    let partial = under_rock_weight(10.0, false, true, &on);
    assert!(
        partial > 0.0 && partial < 1.0,
        "default weight jumped to {partial}"
    );
    let off = EaseSettings {
        depth_blend: false,
        ..EaseSettings::default()
    };
    assert_eq!(under_rock_weight(10.0, false, true, &off), 1.0);
    assert_eq!(under_rock_weight(10.0, true, false, &off), 0.0);
    assert_eq!(under_rock_weight(1.0, true, false, &on), 0.0);
    let deep_open = interior_exposure_target(OUTDOOR_EV, INTERIOR_EV, 30.0, true, &on);
    assert!((deep_open - (OUTDOOR_EV - 1.0)).abs() < 1.0e-4, "{deep_open}");
    let outdoors = interior_exposure_target(OUTDOOR_EV, INTERIOR_EV, 0.0, false, &on);
    assert_eq!(outdoors, OUTDOOR_EV);
}
