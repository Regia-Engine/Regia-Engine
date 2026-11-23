// SPDX-License-Identifier: MPL-2.0
//! Interior chase camera (ADR-8336). Behavior only: no native-query hashes and no pinned `push_disc` centers.
//!
//! At 306c4950 these symbols did not exist, so each test failed to compile on the base.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]

// The shared support module exports helpers this test does not call.
#[allow(dead_code)]
mod native_support;

use regia_detsim::float::{det_clamp, det_max};
use regia_detsim::math;
use regia_world_geom::compat::fixtures::switchbacks;
use regia_world_geom::compat::layout::Sample;
use regia_world_geom::cook::{CookInputs, GroundInput, cook};
use regia_world_geom::cooked::{CookedTile, CookedTriangle, TriKind, encode_canonical};
use regia_world_geom::interior::{
    CamZone, CameraInteriorParams, ChaseFrame, InteriorProfile, RampSample, RampSpan, boom_for, camera_floor,
    in_camera_space, mouth_blend, shaped_boom, single_ray_clip,
};
use regia_world_geom::{Band, WorldGeom};

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    math::sqrt(d0 * d0 + d1 * d1 + d2 * d2)
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    d0 * d0 + d1 * d1 + d2 * d2
}

fn tri(index: u32, kind: TriKind, v: [[i32; 3]; 3]) -> CookedTriangle {
    CookedTriangle {
        v,
        kind,
        flags: 0,
        layer: 0,
        index,
        section: None,
        material: 0,
    }
}

fn slab(index: u32, kind: TriKind, y_mm: i32) -> Vec<CookedTriangle> {
    vec![
        tri(
            index,
            kind,
            [
                [-10_000, y_mm, -10_000],
                [10_000, y_mm, -10_000],
                [-10_000, y_mm, 10_000],
            ],
        ),
        tri(
            index + 1,
            kind,
            [
                [-10_000, y_mm, 10_000],
                [10_000, y_mm, -10_000],
                [10_000, y_mm, 10_000],
            ],
        ),
    ]
}

fn wall_quad(index: u32, x_mm: i32, z0: i32, z1: i32, y0: i32, y1: i32) -> Vec<CookedTriangle> {
    vec![
        tri(
            index,
            TriKind::Wall,
            [[x_mm, y0, z0], [x_mm, y1, z0], [x_mm, y0, z1]],
        ),
        tri(
            index + 1,
            TriKind::Wall,
            [[x_mm, y1, z0], [x_mm, y1, z1], [x_mm, y0, z1]],
        ),
    ]
}

fn flat(triangles: Vec<CookedTriangle>) -> WorldGeom {
    let input = GroundInput {
        lattice_n: 17,
        width_m: 32.0,
        offset_m: 0.0,
        step_m: 0.03125,
        counts: vec![0; 17 * 17],
    };
    let mut cooked = cook(&CookInputs {
        document: b"(version: 1, ground_tile_cells: 16, layers: [])",
        ground: &input,
        terrain: None,
        read_file: &|_| Err("no sources".into()),
    })
    .unwrap()
    .file
    .canonical;
    if !triangles.is_empty() {
        cooked.tiles = vec![CookedTile {
            origin_mm: [0, 0],
            triangles,
        }];
    }
    WorldGeom::from_canonical_bytes(&encode_canonical(&cooked).unwrap()).unwrap()
}

fn no_hole(_: f32, _: f32) -> bool {
    false
}

fn always_hole(_: f32, _: f32) -> bool {
    true
}

/// Per-frame world bound at `speed` m/s.
///
/// The pivot moves `speed * dt`. Heading eases 20% toward a point 12 samples ahead, which swings a
/// boom of `arm` metres by about `0.2 * arm`. The boom length itself moves at most
/// `(arm - interior).max(1) * dt / mouth_blend_s + 0.9` (the mouth step, plus the slack a pull-in
/// may keep past the hard clip). A memoryless ray that clears a fin moves the whole remaining boom
/// and fails this bound.
fn world_bound(speed: f32, dt: f32, arm: f32, params: &CameraInteriorParams) -> (f32, f32) {
    let gap = arm - params.interior_boom_m;
    let gap = if gap > 1.0 { gap } else { 1.0 };
    let length_step = gap * (dt / params.mouth_blend_s) + 0.9;
    (speed * dt + arm * 0.2 + length_step, length_step)
}

fn place_frame(
    profile: &mut InteriorProfile,
    geom: &WorldGeom,
    state: &regia_world_geom::GeomState,
    frame: &ChaseFrame<'_>,
    params: &CameraInteriorParams,
) -> regia_world_geom::interior::ChasePose {
    profile.place(geom, state, frame, params)
}

fn sphere_clear(geom: &WorldGeom, state: &regia_world_geom::GeomState, eye: [f32; 3], radius: f32) -> bool {
    let band = Band {
        lo: eye[1] - radius,
        hi: eye[1] + radius,
    };
    geom.push_disc(state, eye[0], eye[2], radius, band).contacts == 0
}

/// The fin is the snap the single-ray camera has and the eased boom does not.
fn fin_snap(params: &CameraInteriorParams) {
    let mut tris = slab(0, TriKind::Floor, 0);
    tris.extend(slab(2, TriKind::Roof, 4_000));
    tris.extend(wall_quad(4, 0, 0, 4_000, 0, 4_000));
    let geom = flat(tris);
    let state = geom.new_state();
    let dt = 1.0 / 60.0;
    let speed = 8.0;
    let arm = 6.0;
    let (bound, _) = world_bound(speed, dt, arm, params);
    let mut profile = InteriorProfile::new();
    let mut prev_eye = None;
    let mut prev_ray = None;
    let mut eased_max = 0.0_f32;
    let mut ray_max = 0.0_f32;
    let mut ray_far = false;
    let mut ray_near = false;
    let ramps: &[RampSpan] = &[];
    for frame in 0..50 {
        let z = -2.0 + speed * dt * frame as f32;
        let pivot = [-2.0, 1.7, z];
        let desired = [pivot[0] + arm, 1.7, z];
        let pose = place_frame(
            &mut profile,
            &geom,
            &state,
            &ChaseFrame {
                feet: [-2.0, 0.0, z],
                pivot,
                desired,
                exterior: desired,
                dt_s: dt,
                hole_at: &no_hole,
                ramps,
            },
            params,
        );
        let ray = single_ray_clip(&geom, &state, pivot, desired, params.cam_margin_m);
        if ray[0] > 3.0 {
            ray_far = true;
        }
        if ray[0] < 0.0 {
            ray_near = true;
        }
        assert!(
            sphere_clear(&geom, &state, pose.eye, params.interior_sphere_m),
            "eased eye {:?} intersects a wall",
            pose.eye
        );
        assert!(
            in_camera_space(&geom, &state, pose.eye, params),
            "eased eye {:?} left camera space",
            pose.eye
        );
        if frame > 0 {
            let step = dist(pose.eye, prev_eye.unwrap());
            eased_max = det_max(eased_max, step);
            let ray_step = dist(ray, prev_ray.unwrap());
            ray_max = det_max(ray_max, ray_step);
            assert!(
                step <= bound,
                "eased eye moved {step} m, bound {bound} (ray step {ray_step})"
            );
        }
        prev_eye = Some(pose.eye);
        prev_ray = Some(ray);
    }
    assert!(ray_far && ray_near, "the single ray never crossed the fin");
    assert!(
        ray_max > bound,
        "single ray max step {ray_max} did not exceed bound {bound}"
    );
    assert!(eased_max <= bound, "eased max step {eased_max}, bound {bound}");
    assert!(
        eased_max < ray_max,
        "eased {eased_max} was not smoother than the ray {ray_max}"
    );
}

fn index_near(samples: &[Sample], at: [f32; 3]) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::MAX;
    for (i, sample) in samples.iter().enumerate() {
        let d = dist2(sample.p, at);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

fn points_between(samples: &[Sample], from_s: f32, to_s: f32) -> Vec<[f32; 3]> {
    let lo = if from_s < to_s { from_s } else { to_s };
    let hi = if from_s < to_s { to_s } else { from_s };
    let mut pts: Vec<[f32; 3]> = samples
        .iter()
        .filter(|sample| sample.s + 1.0e-3 >= lo && sample.s <= hi + 1.0e-3)
        .map(|sample| sample.p)
        .collect();
    if from_s > to_s {
        pts.reverse();
    }
    pts
}

fn resample(pts: &[[f32; 3]], step: f32) -> Vec<[f32; 3]> {
    if pts.len() < 2 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    let mut acc = 0.0_f32;
    let mut next = step;
    for pair in pts.windows(2) {
        let seg = dist(pair[0], pair[1]);
        if seg < 1.0e-6 {
            continue;
        }
        while acc + seg + 1.0e-4 >= next && out.len() < 8_000 {
            let t = det_clamp((next - acc) / seg, 0.0, 1.0);
            out.push([
                pair[0][0] + (pair[1][0] - pair[0][0]) * t,
                pair[0][1] + (pair[1][1] - pair[0][1]) * t,
                pair[0][2] + (pair[1][2] - pair[0][2]) * t,
            ]);
            next += step;
        }
        acc += seg;
    }
    out
}

fn junction_polyline(
    samples_in: &[Sample],
    samples_out: &[Sample],
    at: [f32; 3],
    step: f32,
) -> Vec<[f32; 3]> {
    let into = index_near(samples_in, at);
    let out_at = index_near(samples_out, at);
    let s_in = samples_in[into].s;
    let mut approach = points_between(samples_in, s_in - 30.0, s_in);
    let s_out = samples_out[out_at].s;
    let end = samples_out.last().map(|sample| sample.s).unwrap_or(s_out);
    let depart = if end - s_out >= 20.0 {
        points_between(samples_out, s_out, s_out + 20.0)
    } else {
        points_between(samples_out, s_out, s_out - 20.0)
    };
    if depart
        .first()
        .is_some_and(|p| approach.last().is_some_and(|q| dist2(*p, *q) < 1.0e-4))
    {
        approach.extend(depart.iter().skip(1).copied());
    } else {
        approach.extend(depart);
    }
    resample(&approach, step)
}

fn norm_xz(x: f32, z: f32) -> (f32, f32) {
    let n = math::sqrt(x * x + z * z);
    if n < 1.0e-6 { (0.0, 1.0) } else { (x / n, z / n) }
}

fn chase_junction(
    geom: &WorldGeom,
    state: &regia_world_geom::GeomState,
    path: &[[f32; 3]],
    junction: [f32; 3],
) {
    let params = CameraInteriorParams::default();
    let dt = 1.0 / 60.0;
    let speed = 8.0;
    let arm = params.exterior_boom_m;
    let (bound, length_step) = world_bound(speed, dt, arm, &params);
    assert!(path.len() > 40, "junction path has {} samples", path.len());
    let floor = camera_floor(
        geom,
        state,
        junction[0],
        junction[2],
        junction[1],
        params.floor_reach_m,
    );
    assert!(floor.is_some(), "no tube floor at {junction:?}");
    let mut profile = InteriorProfile::new();
    let mut heading = {
        let d0 = path[1][0] - path[0][0];
        let d2 = path[1][2] - path[0][2];
        norm_xz(d0, d2)
    };
    let mut prev_eye: Option<[f32; 3]> = None;
    let mut prev_boom: Option<f32> = None;
    let mut nearest = f32::MAX;
    let mut saw_near = false;
    let ramps: &[RampSpan] = &[];
    for (frame, feet) in path.iter().enumerate() {
        let ahead = path[(frame + 12).min(path.len() - 1)];
        let to = norm_xz(ahead[0] - feet[0], ahead[2] - feet[2]);
        heading = norm_xz(heading.0 * 0.8 + to.0 * 0.2, heading.1 * 0.8 + to.1 * 0.2);
        let pivot = [feet[0], feet[1] + 1.7, feet[2]];
        let desired = [pivot[0] - heading.0 * arm, pivot[1], pivot[2] - heading.1 * arm];
        let pose = place_frame(
            &mut profile,
            geom,
            state,
            &ChaseFrame {
                feet: *feet,
                pivot,
                desired,
                exterior: desired,
                dt_s: dt,
                hole_at: &no_hole,
                ramps,
            },
            &params,
        );
        let dx = feet[0] - junction[0];
        let dz = feet[2] - junction[2];
        let flat_d = math::sqrt(dx * dx + dz * dz);
        if flat_d < nearest {
            nearest = flat_d;
        }
        if flat_d < 8.0 {
            saw_near = true;
            assert_ne!(
                pose.zone,
                CamZone::Exterior,
                "exterior within 8 m of {junction:?}"
            );
            assert!(
                pose.boom_m < 4.0,
                "boom {} within 8 m of the junction",
                pose.boom_m
            );
        }
        assert!(
            sphere_clear(geom, state, pose.eye, params.interior_sphere_m),
            "eye {:?} intersects rock near {junction:?}",
            pose.eye
        );
        assert!(
            in_camera_space(geom, state, pose.eye, &params),
            "eye {:?} left the tube near {junction:?}",
            pose.eye
        );
        if frame > 0 {
            let step = dist(pose.eye, prev_eye.unwrap());
            assert!(step <= bound, "eye moved {step} m at {feet:?}, bound {bound}");
            let boom_step = f32::abs(pose.boom_m - prev_boom.unwrap());
            assert!(
                boom_step <= length_step,
                "boom changed {boom_step} m, length step {length_step}"
            );
        }
        prev_eye = Some(pose.eye);
        prev_boom = Some(pose.boom_m);
    }
    assert!(saw_near, "never came within 8 m of {junction:?}");
    assert!(nearest < 1.0, "nearest approach {nearest} m at {junction:?}");
}

#[test]
fn junction_pass_has_no_one_frame_snap() {
    let params = CameraInteriorParams::default();
    fin_snap(&params);

    let file = native_support::read("switchbacks_mesh");
    let geom = native_support::native(&file, "switchbacks_mesh", 16.0);
    let state = geom.new_state();
    let layout = switchbacks().sample(None);
    let tube = layout.passage("tube").unwrap();
    let gallery = layout.passage("gallery").unwrap();
    let dt = 1.0 / 60.0;
    let step = 8.0 * dt;
    let west = [-120.0, -286.0, 40.0];
    let east = [120.0, -286.0, 40.0];
    let west_path = junction_polyline(&tube.samples, &gallery.samples, west, step);
    chase_junction(&geom, &state, &west_path, west);
    let east_path = junction_polyline(&tube.samples, &gallery.samples, east, step);
    chase_junction(&geom, &state, &east_path, east);
}

#[test]
fn cam_rel_eye_under_a_ceiling_stays_in_the_tube() {
    let mut tris = slab(0, TriKind::Floor, 0);
    tris.extend(slab(2, TriKind::Roof, 4_000));
    tris.extend(wall_quad(4, 1_000, -2_000, 2_000, 0, 4_000));
    let geom = flat(tris);
    let state = geom.new_state();
    let params = CameraInteriorParams::default();
    let ramps: &[RampSpan] = &[];
    let mut profile = InteriorProfile::new();
    let pivot = [0.0, 1.7, 0.0];
    let up = place_frame(
        &mut profile,
        &geom,
        &state,
        &ChaseFrame {
            feet: [0.0, 0.0, 0.0],
            pivot,
            desired: [0.0, 7.7, 0.0],
            exterior: [0.0, 7.7, 0.0],
            dt_s: 1.0 / 60.0,
            hole_at: &no_hole,
            ramps,
        },
        &params,
    );
    let bare = single_ray_clip(&geom, &state, pivot, [0.0, 7.7, 0.0], params.cam_margin_m);
    assert!(up.eye[1] < 4.0, "eye rose through the lid: {:?}", up.eye);
    assert!(
        up.eye[1] + params.interior_sphere_m < 4.0,
        "sphere top crossed the lid: {:?}",
        up.eye
    );
    assert!(
        up.eye[1] < bare[1],
        "swept eye {:?} was not shorter than the ray {bare:?}",
        up.eye
    );
    assert!(sphere_clear(&geom, &state, up.eye, params.interior_sphere_m));
    assert!(in_camera_space(&geom, &state, up.eye, &params));

    let mut profile = InteriorProfile::new();
    let side = place_frame(
        &mut profile,
        &geom,
        &state,
        &ChaseFrame {
            feet: [0.0, 0.0, 0.0],
            pivot,
            desired: [8.0, 1.7, 0.0],
            exterior: [8.0, 1.7, 0.0],
            dt_s: 1.0 / 60.0,
            hole_at: &no_hole,
            ramps,
        },
        &params,
    );
    assert!(
        side.eye[0] + params.interior_sphere_m < 1.0,
        "eye crossed the wall: {:?}",
        side.eye
    );
    assert!(sphere_clear(&geom, &state, side.eye, params.interior_sphere_m));

    let mut profile = InteriorProfile::new();
    let clear = place_frame(
        &mut profile,
        &geom,
        &state,
        &ChaseFrame {
            feet: [0.0, 0.0, 0.0],
            pivot,
            desired: [0.0, 2.2, 0.0],
            exterior: [0.0, 2.2, 0.0],
            dt_s: 1.0 / 60.0,
            hole_at: &no_hole,
            ramps,
        },
        &params,
    );
    assert!(
        (clear.eye[1] - 2.2).abs() < 0.05,
        "clear boom moved: {:?}",
        clear.eye
    );
    assert!(sphere_clear(&geom, &state, clear.eye, params.interior_sphere_m));
}

#[test]
fn mouth_blend_takes_half_a_second() {
    let params = CameraInteriorParams::default();
    assert_eq!(mouth_blend(0.25, params.mouth_blend_s), 0.5);
    assert_eq!(mouth_blend(0.5, params.mouth_blend_s), 1.0);
    assert_eq!(mouth_blend(4.0, params.mouth_blend_s), 1.0);
    assert_eq!(mouth_blend(0.0, params.mouth_blend_s), 0.0);
    assert_eq!(mouth_blend(-1.0, params.mouth_blend_s), 0.0);
    assert_eq!(boom_for(0.0, &params), params.exterior_boom_m);
    assert_eq!(boom_for(1.0, &params), params.interior_boom_m);
    assert_eq!(boom_for(0.5, &params), 3.5);
    let (look, eye) = shaped_boom([0.0, 0.0, 0.0], [6.0, 0.0, 0.0], 1.0, &params);
    assert_eq!(look, [0.0, 0.0, 0.0]);
    assert!((eye[0] - params.interior_boom_m).abs() < 1.0e-4, "{eye:?}");

    let dt = 1.0 / 60.0;
    let mut profile = InteriorProfile::new();
    let seed = profile.step(dt, false, false, params.exterior_boom_m, &params);
    assert!(!seed.seed);
    assert_eq!(seed.blend, 0.0);
    assert_eq!(seed.zone, CamZone::Exterior);
    let mut blend = 0.0;
    for frame in 1..=30 {
        let snap = profile.step(dt, false, true, params.exterior_boom_m, &params);
        blend = snap.blend;
        assert_eq!(snap.zone, CamZone::Mouth);
        if frame == 15 {
            assert!((blend - 0.5).abs() < 1.0e-4, "blend {blend} after 15 frames");
        }
        if frame == 29 {
            assert!(blend < 1.0, "blend reached 1 before half a second: {blend}");
        }
    }
    assert_eq!(blend, 1.0);

    let mut profile = InteriorProfile::new();
    profile.step(dt, false, false, params.exterior_boom_m, &params);
    let one = profile.step(0.5, false, true, params.exterior_boom_m, &params);
    assert_eq!(one.blend, 1.0);
    assert_eq!(one.zone, CamZone::Mouth);

    let mut profile = InteriorProfile::new();
    let entered = profile.step(dt, true, false, params.exterior_boom_m, &params);
    assert_eq!(entered.zone, CamZone::Interior);
    let mut open = profile.step(dt, false, false, params.exterior_boom_m, &params);
    assert_eq!(open.zone, CamZone::Mouth);
    for _ in 0..28 {
        open = profile.step(dt, false, false, params.exterior_boom_m, &params);
        assert_eq!(open.zone, CamZone::Mouth);
    }
    open = profile.step(dt, false, false, params.exterior_boom_m, &params);
    assert_eq!(open.zone, CamZone::Exterior);

    let mut profile = InteriorProfile::new();
    for _ in 0..40 {
        let snap = profile.step(dt, false, true, params.exterior_boom_m, &params);
        assert_eq!(snap.zone, CamZone::Mouth);
    }
}

#[test]
fn camera_floor_reach_is_the_callers_tolerance() {
    let geom = flat(slab(0, TriKind::Floor, -30_000));
    let state = geom.new_state();
    assert!(camera_floor(&geom, &state, 1.0, 0.0, 0.0, 12.0).is_none());
    let near = camera_floor(&geom, &state, 1.0, 0.0, -20.0, 12.0).unwrap();
    assert!((near - (-30.0)).abs() < 0.05, "{near}");
    let long = camera_floor(&geom, &state, 1.0, 0.0, 0.0, 40.0).unwrap();
    assert!((long - (-30.0)).abs() < 0.05, "{long}");
    assert!(camera_floor(&geom, &state, 1.0, 0.0, 0.0, -1.0).is_none());
    assert!(camera_floor(&geom, &state, 1.0, 0.0, 0.0, f32::NAN).is_none());

    let file = native_support::read("switchbacks_mesh");
    let cooked = native_support::native(&file, "switchbacks_mesh", 16.0);
    let cooked_state = cooked.new_state();
    let plain = camera_floor(&cooked, &cooked_state, -120.0, 40.0, -256.0, 12.0);
    if let Some(fy) = plain {
        assert!(
            (fy - (-286.0)).abs() > 10.0,
            "plain query picked the tube at {fy}"
        );
    }
    let tube = camera_floor(&cooked, &cooked_state, -120.0, 40.0, -275.0, 12.0)
        .expect("tube floor within 12 m of y=-275");
    assert!((tube - (-286.0)).abs() < 2.0, "tube floor {tube}");
}

#[test]
fn open_cut_at_the_feet_does_not_clamp_to_the_heightfield() {
    let geom = flat(Vec::new());
    let state = geom.new_state();
    let params = CameraInteriorParams::default();
    let samples = [
        RampSample {
            p: [-18.0, 0.0, 0.0],
            s: 0.0,
        },
        RampSample {
            p: [0.0, 0.0, 0.0],
            s: 18.0,
        },
    ];
    let ramps = [RampSpan {
        width_m: 20.0,
        samples: &samples,
    }];
    let pivot = [0.0, 0.2, 0.0];
    let desired = [0.5, -3.0, 0.0];
    let mut held = InteriorProfile::new();
    let on_ground = place_frame(
        &mut held,
        &geom,
        &state,
        &ChaseFrame {
            feet: pivot,
            pivot,
            desired,
            exterior: desired,
            dt_s: 1.0 / 60.0,
            hole_at: &no_hole,
            ramps: &ramps,
        },
        &params,
    );
    let mut cut = InteriorProfile::new();
    let in_hole = place_frame(
        &mut cut,
        &geom,
        &state,
        &ChaseFrame {
            feet: pivot,
            pivot,
            desired,
            exterior: desired,
            dt_s: 1.0 / 60.0,
            hole_at: &always_hole,
            ramps: &ramps,
        },
        &params,
    );
    assert!(
        on_ground.eye[1] >= 0.0,
        "heightfield let the eye through: {:?}",
        on_ground.eye
    );
    assert!(in_hole.eye[1] < 0.0, "open cut still clamped: {:?}", in_hole.eye);
    assert_eq!(on_ground.zone, CamZone::Mouth);
    assert_eq!(in_hole.zone, CamZone::Mouth);
}
