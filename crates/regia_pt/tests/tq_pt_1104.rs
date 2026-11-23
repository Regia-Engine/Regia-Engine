// SPDX-License-Identifier: MPL-2.0
//! Test-gap sweep of `regia_pt` (sprint lane testgaps-render-pt, 2026-11-05): public functions that
//! no test named before, ranked by risk (math, encodings, hashes and cache keys, schedules,
//! parsers). Pure CPU, fixed inputs and seeds, no goldens, no device.

use regia_asset::baked_gi::VolumeLayout;
use regia_core::glam::{Mat4, Quat, Vec3};
use regia_pt::bake_incremental::{GlobalHashes, IncrementalSettings, global_hashes};
use regia_pt::bake_sizing::{SyntheticLevel, bake_gpu_bytes};
use regia_pt::bvh::Aabb;
use regia_pt::cloud::{
    CLOUD_ASYMMETRY, CloudLight, PtCloudLayer, VALIDATION_GROUND_HALF_M, VALIDATION_SKY_NIT,
    reference_ground_ratio, renderer_sky_ratio, renderer_sun_ratio, slab_transport_reference,
    suggested_max_bounces, validation_scene,
};
use regia_pt::env_map::{EnvMap, equirect_uv};
use regia_pt::exr::{ExrChannel, decode as exr_decode, encode as exr_encode};
use regia_pt::hdr::{rgb_to_rgbe, rgbe_to_rgb};
use regia_pt::probe::{GiBakeSettings, detail_brick};
use regia_pt::scene::{PtHeightfield, PtSkyBody};
use regia_pt::{MAX_PATH_VERTICES, PtCamera, PtImage, PtMaterial, PtMesh, PtScene, PtSky, PtSun};

fn unit_box_mesh() -> PtMesh {
    let c = |i: usize| {
        [
            if i & 1 == 0 { 0.0 } else { 1.0 },
            if i & 2 == 0 { 0.0 } else { 1.0 },
            if i & 4 == 0 { 0.0 } else { 1.0 },
        ]
    };
    let positions: Vec<[f32; 3]> = (0..8).map(c).collect();
    let faces: [[u32; 4]; 6] = [
        [0, 4, 6, 2],
        [1, 3, 7, 5],
        [0, 1, 5, 4],
        [2, 6, 7, 3],
        [0, 2, 3, 1],
        [4, 5, 7, 6],
    ];
    let indices = faces
        .iter()
        .flat_map(|f| [f[0], f[1], f[2], f[0], f[2], f[3]])
        .collect();
    PtMesh {
        normals: vec![[0.0; 3]; positions.len()],
        positions,
        indices,
        ..PtMesh::default()
    }
}

/// A floor and a box under a sun and a uniform sky.
fn small_scene() -> PtScene {
    let mut s = PtScene::new(PtCamera::pinhole([0.0, 5.0, 10.0], [0.0; 3], 1.0, 8, 8));
    s.sky = PtSky::Uniform([0.5, 0.6, 0.8]);
    s.sun = Some(PtSun {
        toward: [0.5, 0.8, 0.3],
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    let grey = s.add_material(PtMaterial::lambert([0.5; 3]));
    let floor = s.add_mesh(PtMesh {
        positions: vec![
            [-10.0, 0.0, -10.0],
            [10.0, 0.0, -10.0],
            [-10.0, 0.0, 10.0],
            [10.0, 0.0, 10.0],
        ],
        normals: vec![[0.0, 1.0, 0.0]; 4],
        indices: vec![0, 2, 1, 1, 2, 3],
        ..PtMesh::default()
    });
    s.add_instance(floor, grey, Mat4::IDENTITY);
    let cube = s.add_mesh(unit_box_mesh());
    s.add_instance(cube, grey, Mat4::IDENTITY);
    s
}

fn two_brick_layout() -> VolumeLayout {
    let mut l = VolumeLayout {
        terrain: None,
        bricks: [[-1.5, 0.5, 0.0], [4.0, 0.5, 0.0]]
            .iter()
            .map(|o: &[f32; 3]| detail_brick(*o, [o[0] + 1.0, o[1] + 1.0, o[2] + 1.0], 1.0))
            .collect(),
        moment_resolution: 4,
        max_distance: 8.0,
    };
    l.assign_brick_offsets();
    l
}

fn inc(device: &str) -> IncrementalSettings {
    IncrementalSettings {
        influence_margin: Some(4.0),
        device: device.into(),
        ..IncrementalSettings::default()
    }
}

fn globals(scene: &PtScene, settings: &GiBakeSettings, device: &str, backend: &str) -> GlobalHashes {
    global_hashes(scene, &two_brick_layout(), settings, &inc(device), backend)
}

// ---------------------------------------------------------------------------------------------
// Incremental GI bake: the global hashes are a cache key (a wrong key reuses stale probes).
// ---------------------------------------------------------------------------------------------

/// Catches a global cache key that is not a pure function of its inputs (a pointer, a map order
/// or a clock folded in), which would make every incremental rebake a full bake.
#[test]
fn global_hashes_are_stable_for_the_same_inputs() {
    let s = small_scene();
    let set = GiBakeSettings::default();
    let a = globals(&s, &set, "cpu (test)", "cpu");
    let b = globals(&small_scene(), &set, "cpu (test)", "cpu");
    assert_eq!(a, b);
    assert_eq!(a.first_difference(&b), None);
}

/// Catches a batching knob leaking into the settings key: batch size and samples per submit do not
/// change a probe, so changing them must not invalidate the stored state.
#[test]
fn batching_does_not_change_the_settings_hash() {
    let s = small_scene();
    let set = GiBakeSettings::default();
    let mut batched = set;
    batched.trace.probes_per_batch = set.trace.probes_per_batch.wrapping_add(17).max(1);
    batched.trace.samples_per_submit = set.trace.samples_per_submit.wrapping_add(3).max(1);
    assert_eq!(globals(&s, &set, "d", "cpu"), globals(&s, &batched, "d", "cpu"));
}

/// Catches an edit that lands in the wrong hash (or none): each change must flip exactly the hash
/// that covers it, so `first_difference` names the right reason for a full rebake.
#[test]
fn each_global_change_flips_its_own_hash() {
    let s = small_scene();
    let set = GiBakeSettings::default();
    let base = globals(&s, &set, "d", "cpu");

    let mut rays = set;
    rays.trace.rays_per_probe += 1;
    assert_eq!(
        base.first_difference(&globals(&s, &rays, "d", "cpu")),
        Some("settings")
    );

    let mut sunny = s.clone();
    if let Some(sun) = sunny.sun.as_mut() {
        sun.illuminance *= 2.0;
    }
    let g = globals(&sunny, &set, "d", "cpu");
    assert_eq!(base.first_difference(&g), Some("sky"));
    assert_eq!(
        (g.tracer, g.settings, g.terrain, g.camera),
        (base.tracer, base.settings, base.terrain, base.camera)
    );

    let mut moved = s.clone();
    moved.camera.eye[0] += 1.0;
    let g = globals(&moved, &set, "d", "cpu");
    assert_eq!(base.first_difference(&g), Some("camera"));
    assert_eq!(
        (g.tracer, g.settings, g.sky, g.terrain),
        (base.tracer, base.settings, base.sky, base.terrain)
    );

    assert_eq!(
        base.first_difference(&globals(&s, &set, "other gpu", "cpu")),
        Some("tracer")
    );
    assert_eq!(
        base.first_difference(&globals(&s, &set, "d", "gpu")),
        Some("tracer")
    );
}

/// Catches `first_difference` reporting a later hash when several differ, or comparing the wrong
/// pair of fields: it must walk `NAMES` in order.
#[test]
fn first_difference_reports_hashes_in_name_order() {
    let a = GlobalHashes::default();
    let mut b = a;
    b.camera[0] = 1;
    b.sky[31] = 1;
    assert_eq!(a.first_difference(&b), Some("sky"));
    b.tracer[5] = 9;
    assert_eq!(a.first_difference(&b), Some("tracer"));
    for (k, name) in GlobalHashes::NAMES.iter().enumerate() {
        let mut c = a;
        match k {
            0 => c.tracer[0] = 1,
            1 => c.settings[0] = 1,
            2 => c.sky[0] = 1,
            3 => c.terrain[0] = 1,
            _ => c.camera[0] = 1,
        }
        assert_eq!(a.first_difference(&c), Some(*name));
        assert_eq!(c.first_difference(&a), Some(*name));
    }
}

// ---------------------------------------------------------------------------------------------
// Bake sizing: GPU byte estimates gate whether a bake starts.
// ---------------------------------------------------------------------------------------------

fn job() -> regia_pt::bake_desc::GiBakeJob {
    regia_pt::bake_desc::GiBakeJob {
        scene: small_scene(),
        layout: two_brick_layout(),
        settings: GiBakeSettings::default(),
        plan: regia_pt::bake_desc::BakePlan::default(),
    }
}

/// Catches the batch not being capped by the live probe count (a tiny volume sized as a full
/// batch) and a zero live count turning into a zero-sized (or underflowed) batch.
#[test]
fn bake_gpu_bytes_caps_the_batch_by_the_live_probes() {
    let mut j = job();
    j.settings.trace.probes_per_batch = 64;
    j.settings.trace.rays_per_probe = 32;
    let few = bake_gpu_bytes(&j, 5).expect("packs");
    assert_eq!(few.batch_probes, 5);
    assert_eq!(few.path_slots, 5 * 32);
    let many = bake_gpu_bytes(&j, 10_000).expect("packs");
    assert_eq!(many.batch_probes, 64);
    let none = bake_gpu_bytes(&j, 0).expect("packs");
    assert_eq!(none.batch_probes, 1);
    assert!(none.total > 0);
    // The scene bytes hold one 16-byte probe record per batch slot (ADR-862); nothing else in
    // them depends on the batch.
    assert_eq!(many.scene - few.scene, (64 - 5) * 16);
    assert!(many.total > few.total);
}

/// Catches an unclamped ray count (0 rays sized as an empty buffer, a huge count overflowing the
/// path buffer) and a total that drops or double counts a buffer.
#[test]
fn bake_gpu_bytes_clamps_rays_and_sums_its_parts() {
    let mut j = job();
    j.settings.trace.probes_per_batch = 4;
    j.settings.trace.rays_per_probe = 0;
    let zero = bake_gpu_bytes(&j, 4).expect("packs");
    assert_eq!(zero.path_slots, 4);
    j.settings.trace.rays_per_probe = u32::MAX;
    let huge = bake_gpu_bytes(&j, 4).expect("packs");
    assert_eq!(huge.path_slots, 4 * 4096);
    for b in [zero, huge] {
        assert!(b.total >= b.scene + b.paths + b.hits + 2 * b.accum, "{b:?}");
        assert!(b.largest_binding <= b.total);
        assert!(b.largest_binding >= b.paths.max(b.hits).max(b.accum));
    }
}

/// Catches a backdrop ring whose rotation mirrors the slabs (inward-facing triangles the tracer
/// culls as back faces) or puts slabs off the ring.
#[test]
fn backdrop_slabs_face_outward_on_the_ring() {
    let level = SyntheticLevel::small();
    let m = level.backdrop_mesh();
    assert!(!m.indices.is_empty());
    assert_eq!(m.positions.len(), m.normals.len());
    assert_eq!(m.positions.len() % 36, 0, "36 equal slabs");
    assert!(m.indices.len().is_multiple_of(3));
    let r = level.half_extent() * 0.9375;
    for p in &m.positions {
        let d = (p[0] * p[0] + p[2] * p[2]).sqrt();
        assert!(
            (d - r).abs() < 31.0,
            "vertex {p:?} is {d} m from the centre, ring {r}"
        );
    }
    let mut agree = 0usize;
    for tri in m.indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| Vec3::from_array(m.positions[i as usize]));
        let face = (b - a).cross(c - a);
        let n: Vec3 = tri.iter().map(|&i| Vec3::from_array(m.normals[i as usize])).sum();
        assert!(face.length() > 0.0, "degenerate triangle");
        if face.dot(n) > 0.0 {
            agree += 1;
        }
    }
    assert_eq!(
        agree,
        m.indices.len() / 3,
        "every triangle winds toward its normals"
    );
    for n in &m.normals {
        let l = Vec3::from_array(*n).length();
        assert!((l - 1.0).abs() < 1e-4, "normal {n:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Bake meshes: mesh conversion, bounds and winding.
// ---------------------------------------------------------------------------------------------

/// Catches a cooked mesh with short normal or UV arrays being passed to the tracer as is (an
/// out-of-bounds vertex attribute read on the GPU).
#[test]
fn pt_mesh_from_cpu_drops_mismatched_attributes() {
    let cpu = regia_asset::CpuMesh {
        positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]],
        uvs: vec![[0.0; 2]; 2],
        indices: vec![0, 1, 2],
        ..Default::default()
    };
    let m = regia_pt::bake_meshes::pt_mesh_from_cpu(&cpu);
    assert_eq!(m.normals, vec![[0.0; 3]; 3]);
    assert!(m.uvs.is_empty());
    assert_eq!(m.indices, cpu.indices);
    let whole = regia_asset::CpuMesh {
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.5; 2]; 3],
        ..cpu
    };
    let m = regia_pt::bake_meshes::pt_mesh_from_cpu(&whole);
    assert_eq!(m.normals, whole.normals);
    assert_eq!(m.uvs, whole.uvs);
}

/// Catches world bounds that transform only the two corners (a rotated box clipped) or swap min
/// and max under a mirroring scale.
#[test]
fn transform_bounds_covers_rotations_and_mirrors() {
    use regia_pt::bake_meshes::transform_bounds;
    let rot = Mat4::from_rotation_y(std::f32::consts::FRAC_PI_4);
    let (lo, hi) = transform_bounds(&rot, [-1.0; 3], [1.0; 3]);
    let s = std::f32::consts::SQRT_2;
    for a in [0, 2] {
        assert!(
            (lo[a] + s).abs() < 1e-5 && (hi[a] - s).abs() < 1e-5,
            "{lo:?} {hi:?}"
        );
    }
    assert!((lo[1] + 1.0).abs() < 1e-6 && (hi[1] - 1.0).abs() < 1e-6);
    let mirror =
        Mat4::from_scale_rotation_translation(Vec3::new(-2.0, 1.0, 1.0), Quat::IDENTITY, Vec3::X * 10.0);
    let (lo, hi) = transform_bounds(&mirror, [0.0; 3], [1.0; 3]);
    assert_eq!(lo, [8.0, 0.0, 0.0]);
    assert_eq!(hi, [10.0, 1.0, 1.0]);
}

/// Catches a winding flip that touches a trailing partial triangle or is not its own inverse.
#[test]
fn flip_winding_is_an_involution_and_keeps_normals() {
    use regia_pt::bake_meshes::flip_winding;
    let mut m = unit_box_mesh();
    m.indices.extend([0, 1]); // a malformed tail
    let f = flip_winding(&m);
    assert_eq!(&f.indices[..3], &[m.indices[0], m.indices[2], m.indices[1]]);
    assert_eq!(&f.indices[f.indices.len() - 2..], &[0, 1]);
    assert_eq!(f.normals, m.normals);
    assert_eq!(f.positions, m.positions);
    assert_eq!(flip_winding(&f).indices, m.indices);
}

// ---------------------------------------------------------------------------------------------
// BVH, environment maps, cameras, sky bodies, heightfields.
// ---------------------------------------------------------------------------------------------

/// Catches a NaN point poisoning the box (NaN bounds make every ray miss the whole BLAS) and an
/// empty point list reading as a zero box at the origin.
#[test]
fn aabb_from_points_ignores_nan_and_empty_stays_empty() {
    assert!(Aabb::from_points(&[]).is_empty());
    let b = Aabb::from_points(&[[1.0, 2.0, 3.0], [f32::NAN, -1.0, 0.0], [-4.0, f32::NAN, 5.0]]);
    assert_eq!(b.min, [-4.0, -1.0, 0.0]);
    assert_eq!(b.max, [1.0, 2.0, 5.0]);
    assert!(!b.is_empty());
    let one = Aabb::from_points(&[[7.0, 7.0, 7.0]]);
    assert!(!one.is_empty());
    assert_eq!(one.centroid(), [7.0; 3]);
}

/// Catches an off-by-half-texel or flipped-axis `texel_dir`: the direction of a texel's centre must
/// look up that same texel, and the texels' solid angles must sum to the sphere.
#[test]
fn texel_dir_inverts_equirect_uv_on_every_texel() {
    let (w, h) = (16u32, 8u32);
    let map = EnvMap::constant(w, h, [1.0; 3]);
    let mut sphere = 0.0f64;
    for j in 0..h {
        sphere += f64::from(map.texel_solid_angle(j)) * f64::from(w);
        for i in 0..w {
            let d = map.texel_dir(i, j);
            let len = Vec3::from_array(d).length();
            assert!((len - 1.0).abs() < 1e-5, "({i}, {j}): |d| = {len}");
            let (u, v) = equirect_uv(d, 0.0);
            assert_eq!(((u * w as f32) as u32, (v * h as f32) as u32), (i, j), "{d:?}");
        }
    }
    assert!((sphere - 4.0 * std::f64::consts::PI).abs() < 1e-4, "{sphere}");
}

/// Catches a thin-lens aperture that goes negative or infinite for a zero or negative f-number or
/// focal length, or a zero focus distance (a division by zero in the lens sample).
#[test]
fn with_lens_clamps_its_inputs() {
    let cam = PtCamera::pinhole([0.0; 3], [0.0, 0.0, -1.0], 1.0, 4, 4);
    let lens = cam.with_lens(50.0, 2.0, 3.0);
    assert!((lens.aperture_radius - 0.0125).abs() < 1e-7);
    assert_eq!(lens.focus_distance, 3.0);
    let zero_f = cam.with_lens(50.0, 0.0, 0.0);
    assert!(
        (zero_f.aperture_radius - 0.25).abs() < 1e-6,
        "f-number floored at 0.1"
    );
    assert!(zero_f.focus_distance > 0.0);
    let negative = cam.with_lens(-50.0, 2.0, -5.0);
    assert_eq!(negative.aperture_radius, 0.0);
    assert!(negative.focus_distance > 0.0);
    let nan = cam.with_lens(f32::NAN, 2.0, f32::NAN);
    assert_eq!(nan.aperture_radius, 0.0, "a NaN focal length is a pinhole");
    assert!(nan.focus_distance.is_finite() && nan.focus_distance > 0.0);
    assert_eq!(
        (lens.eye, lens.target, lens.width),
        (cam.eye, cam.target, cam.width)
    );
}

/// Catches a sky-body table that samples outside the disc (the rim darkened or the body grown) or
/// divides by zero for a zero radius.
#[test]
fn sky_body_tabulate_stays_inside_the_disc() {
    let c = Vec3::new(0.3, 0.8, -0.5).normalize();
    let body = PtSkyBody {
        direction: c.to_array(),
        angular_radius: 0.2,
        radiance: Vec::new(),
        size: 9,
        light_scale: 1.0,
    };
    let dirs = body.tabulate(|d| d.to_array());
    assert_eq!(dirs.len(), 81);
    let cos_r = 0.2f32.cos();
    for d in &dirs {
        let v = Vec3::from_array(*d);
        assert!((v.length() - 1.0).abs() < 1e-5);
        assert!(v.dot(c) >= cos_r - 1e-5, "{d:?} lies outside the disc");
    }
    // The centre texel looks at the centre.
    assert!(Vec3::from_array(dirs[40]).dot(c) > 0.99999);
    let point = PtSkyBody {
        angular_radius: 0.0,
        size: 0,
        ..body
    };
    let dirs = point.tabulate(|d| d.to_array());
    assert_eq!(dirs.len(), 1, "size 0 tabulates one texel");
    assert!(Vec3::from_array(dirs[0]).dot(c) > 0.99999);
    assert!(dirs[0].iter().all(|v| v.is_finite()));
}

/// Catches a heightfield whose origin or spacing disagrees with the runtime map (the traced
/// ground offset from the drawn one), including a non-square map.
#[test]
fn from_terrain_map_matches_the_runtime_samples() {
    let (w, h) = (5u32, 3u32);
    let heights: Vec<f32> = (0..w * h).map(|i| i as f32 * 0.25).collect();
    let map = regia_terrain::TerrainMap::from_heights(w, h, heights.clone(), 8.0, 10.0, 0.0).expect("map");
    let hf = PtHeightfield::from_terrain_map(&map, 3);
    assert_eq!((hf.width, hf.depth, hf.material), (w, h, 3));
    assert_eq!(hf.heights, heights);
    for j in 0..h {
        for i in 0..w {
            let world = map.sample_to_world(i, j);
            let x = hf.origin[0] + i as f32 * hf.spacing[0];
            let z = hf.origin[1] + j as f32 * hf.spacing[1];
            assert!(
                (x - world[0]).abs() < 1e-5 && (z - world[1]).abs() < 1e-5,
                "({i}, {j})"
            );
            let traced = hf.height_at(world[0], world[1]).expect("inside");
            assert!((traced - map.height_at_sample(i64::from(i), i64::from(j))).abs() < 1e-4);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cloud validation: the ratios and the reference.
// ---------------------------------------------------------------------------------------------

/// Catches an inverted or NaN slab reporting a negative thickness, which turns into a negative
/// optical depth and a transmittance above 1.
#[test]
fn thickness_is_never_negative() {
    let mut l = PtCloudLayer::overcast(1000.0, 2000.0, 4.0);
    assert_eq!(l.thickness_m(), 1000.0);
    l.top_m = 500.0;
    assert_eq!(l.thickness_m(), 0.0);
    assert_eq!(l.optical_depth(), 0.0);
    l.top_m = f32::NAN;
    assert_eq!(l.thickness_m(), 0.0);
}

/// Catches a bounce budget that is NaN-cast to garbage, exceeds the tracer's vertex cap, or does
/// not grow with the slab's effective depth.
#[test]
fn suggested_bounces_are_bounded_and_monotonic() {
    let thin = PtCloudLayer::overcast(1000.0, 2000.0, 0.0);
    assert_eq!(suggested_max_bounces(&thin), 32);
    let mid = suggested_max_bounces(&PtCloudLayer::overcast(1000.0, 2000.0, 10.0));
    let thick = suggested_max_bounces(&PtCloudLayer::overcast(1000.0, 2000.0, 50.0));
    assert!(
        32 < mid && mid <= thick && thick <= MAX_PATH_VERTICES,
        "{mid} {thick}"
    );
    let huge = suggested_max_bounces(&PtCloudLayer::overcast(1000.0, 2000.0, 1.0e30));
    assert_eq!(huge, MAX_PATH_VERTICES);
    let mut nan = thin;
    nan.extinction = f32::NAN;
    let b = suggested_max_bounces(&nan);
    assert!((32..=MAX_PATH_VERTICES).contains(&b));
    // g = 1 scatters straight through: no walk.
    let mut forward = PtCloudLayer::overcast(1000.0, 2000.0, 50.0);
    forward.asymmetry = 1.0;
    assert_eq!(suggested_max_bounces(&forward), 32);
    assert_eq!(PtCloudLayer::overcast(0.0, 1.0, 1.0).asymmetry, CLOUD_ASYMMETRY);
}

/// Catches renderer ratios that brighten under a cloud (a ratio above 1), go negative, or are not
/// 1 for an empty slab.
#[test]
fn renderer_ratios_stay_in_unit_range() {
    let clear = PtCloudLayer::overcast(1500.0, 3000.0, 0.0);
    assert!((renderer_sky_ratio(&clear) - 1.0).abs() < 1e-6);
    assert!((renderer_sun_ratio(&clear, 0.7) - 1.0).abs() < 1e-4);
    let mut last_sky = 1.0;
    let mut last_sun = 1.0;
    for tau in [0.5f32, 2.0, 8.0, 32.0] {
        let l = PtCloudLayer::overcast(1500.0, 3000.0, tau);
        let sky = renderer_sky_ratio(&l);
        let sun = renderer_sun_ratio(&l, 0.7);
        assert!(
            (0.0..=1.0).contains(&sky) && sky <= last_sky,
            "tau {tau}: sky {sky}"
        );
        assert!(
            (0.0..=1.0).contains(&sun) && sun <= last_sun + 1e-6,
            "tau {tau}: sun {sun}"
        );
        last_sky = sky;
        last_sun = sun;
        for sun_y in [-1.0f32, 0.0, 2.0] {
            let r = renderer_sun_ratio(&l, sun_y);
            assert!(r.is_finite() && (0.0..=1.0).contains(&r), "sun_y {sun_y}: {r}");
        }
    }
}

/// Catches the transport reference hanging or returning garbage for a slab with no extinction
/// (zero, negative or NaN): such a slab passes all light.
#[test]
fn slab_reference_of_an_empty_slab_is_one() {
    let mut l = PtCloudLayer::overcast(1000.0, 2000.0, 0.0);
    assert_eq!(slab_transport_reference(&l, Some(0.5), 0.5, 100, 1), 1.0);
    l.extinction = -1.0;
    assert_eq!(slab_transport_reference(&l, None, 0.5, 100, 1), 1.0);
    l.extinction = f32::NAN;
    assert_eq!(slab_transport_reference(&l, Some(0.5), 0.5, 100, 1), 1.0);
    // Same seed, same answer; the estimate stays a share of the packets (plus ground bounces).
    let thick = PtCloudLayer::overcast(1000.0, 2000.0, 4.0);
    let a = slab_transport_reference(&thick, Some(0.6), 0.5, 500, 9);
    assert_eq!(a, slab_transport_reference(&thick, Some(0.6), 0.5, 500, 9));
    assert!(a > 0.0 && a < 2.0, "{a}");
}

/// Catches a reference ground ratio that divides by zero when there is no light at all.
#[test]
fn reference_ground_ratio_without_light_is_one() {
    let l = PtCloudLayer::overcast(1000.0, 2000.0, 4.0);
    assert_eq!(reference_ground_ratio(&l, 0.5, 0.0, 0.0), 1.0);
    assert_eq!(reference_ground_ratio(&l, -0.5, 1000.0, 0.0), 1.0);
}

/// Catches a validation scene with the wrong lights for the measurement it serves (a sky in the
/// sun-only trace, a sun in the sky-only one), a small ground, or a NaN sun from a NaN elevation.
#[test]
fn validation_scene_lights_follow_the_selection() {
    let both = validation_scene(0.5, CloudLight::Both);
    assert!(matches!(both.sky, PtSky::Uniform(c) if c == [VALIDATION_SKY_NIT; 3]));
    let sun = both.sun.expect("sun");
    assert!((sun.toward[1] - 0.5).abs() < 1e-5);
    let half = both.meshes[0]
        .positions
        .iter()
        .map(|p| p[0].abs().max(p[2].abs()))
        .fold(0.0f32, f32::max);
    assert_eq!(half, VALIDATION_GROUND_HALF_M);

    let sun_only = validation_scene(0.5, CloudLight::SunOnly);
    assert!(matches!(sun_only.sky, PtSky::None));
    assert!(sun_only.sun.is_some());
    let sky_only = validation_scene(0.5, CloudLight::SkyOnly);
    assert!(sky_only.sun.is_none());
    assert!(matches!(sky_only.sky, PtSky::Uniform(_)));

    for (sun_y, want) in [(-1.0f32, 0.02f32), (0.0, 0.02), (2.0, 1.0), (f32::NAN, 0.02)] {
        let s = validation_scene(sun_y, CloudLight::SunOnly).sun.expect("sun");
        assert!(
            s.toward.iter().all(|v| v.is_finite()),
            "sun_y {sun_y}: {:?}",
            s.toward
        );
        assert!((s.toward[1] - want).abs() < 1e-5, "sun_y {sun_y}: {:?}", s.toward);
    }
}

// ---------------------------------------------------------------------------------------------
// Cloud comparison lights.
// ---------------------------------------------------------------------------------------------

/// Catches the single-light variants of the comparison leaking the other light (a sun-only term
/// with sky ambient, a sky-only term with sun) or losing the shared geometry.
#[test]
fn compare_light_variants_isolate_one_light() {
    let case = regia_pt::cloud_compare::CloudCase {
        name: "tq",
        settings: regia_render::CloudSettings::default(),
        sun_elevation_deg: 30.0,
        sun_azimuth_deg: 20.0,
        view_elevation_deg: 10.0,
        fov_y_deg: 40.0,
    };
    let light = case.light();
    assert!(light.sun.illuminance > 0.0);
    let sun = light.sun_only();
    assert!(matches!(sun.sky, PtSky::None));
    assert_eq!(sun.ambient, ([0.0; 3], [0.0; 3]));
    assert_eq!(sun.sun, light.sun);
    assert_eq!(sun.to_sun, light.to_sun);
    let sky = light.sky_only();
    assert_eq!(sky.sun.illuminance, 0.0);
    assert_eq!(sky.sun.toward, light.sun.toward);
    assert_eq!(sky.ambient, light.ambient);
    assert_eq!(sky.planet_radius_m, light.planet_radius_m);
    assert!(!matches!(sky.sky, PtSky::None));
    let up = Vec3::from_array(case.to_sun());
    assert!((up.length() - 1.0).abs() < 1e-5 && (up.y - 0.5).abs() < 1e-5);
}

// ---------------------------------------------------------------------------------------------
// Bake inputs, analytic references, frame extraction.
// ---------------------------------------------------------------------------------------------

/// Catches a sky-atmosphere description with a NaN, infinite, zero or negative height or scale
/// slipping past validation into the LUT build.
#[test]
fn validate_sky_atmosphere_refuses_non_finite_and_negative_values() {
    use regia_pt::bake_inputs::{PlanetPreset, SkyAtmosphereDesc, validate_sky_atmosphere};
    let ok = SkyAtmosphereDesc {
        planet: PlanetPreset::Earth,
        height: 2.0,
        scale: None,
    };
    assert!(validate_sky_atmosphere(&ok).is_ok());
    for scale in [Some(0.0), Some(-0.0), Some(3.0)] {
        assert!(
            validate_sky_atmosphere(&SkyAtmosphereDesc { scale, ..ok }).is_ok(),
            "{scale:?}"
        );
    }
    for height in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let e = validate_sky_atmosphere(&SkyAtmosphereDesc { height, ..ok }).expect_err("height");
        assert_eq!(e.code, "bake_desc_invalid");
        assert_eq!(e.path.as_deref(), Some("sky_atmosphere.height"), "{height}");
    }
    for scale in [-1.0, f32::NAN, f32::INFINITY] {
        let e = validate_sky_atmosphere(&SkyAtmosphereDesc {
            scale: Some(scale),
            ..ok
        })
        .expect_err("scale");
        assert_eq!(e.path.as_deref(), Some("sky_atmosphere.scale"), "{scale}");
    }
}

/// Catches a Lambert reference off by the 1/π (radiance quoted as exitance) or mixing channels.
#[test]
fn lambert_radiance_is_albedo_times_irradiance_over_pi() {
    use regia_pt::analytic::lambert_radiance;
    let l = lambert_radiance(
        [0.5, 0.25, 1.0],
        [std::f32::consts::PI, 2.0 * std::f32::consts::PI, 0.0],
    );
    assert!(
        (l[0] - 0.5).abs() < 1e-6 && (l[1] - 0.5).abs() < 1e-6 && l[2] == 0.0,
        "{l:?}"
    );
    assert_eq!(lambert_radiance([0.0; 3], [1e9; 3]), [0.0; 3]);
}

/// Catches the traced sun drifting from the frame's: without a sun clock the authored light is
/// used as is; with one, the light travels away from the sun's position and a below-ground eye is
/// treated as 1 m.
#[test]
fn effective_sun_follows_the_clock_or_the_authored_light() {
    use regia_pt::extract::effective_sun;
    use regia_render::{DirectionalLightUniform, OutdoorParams, TimeOfDay, sun_position};
    let authored = DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 0.9, 0.8], 12_345.0);
    let mut outdoor = OutdoorParams::default();
    let same = effective_sun(&outdoor, authored, 2.0);
    assert_eq!(
        (same.direction, same.color, same.illuminance),
        (authored.direction, authored.color, authored.illuminance)
    );
    let time = TimeOfDay::default();
    outdoor.time = Some(time);
    let sun = effective_sun(&outdoor, authored, 2.0);
    let toward = Vec3::from_array(sun_position(&time).direction).normalize();
    let travel = Vec3::from_array(sun.direction);
    assert!(travel.dot(-toward) > 0.9999, "{:?}", sun.direction);
    assert!(sun.illuminance.is_finite() && sun.illuminance > 0.0);
    assert!(sun.color.iter().all(|c| c.is_finite() && *c >= 0.0));
    let below = effective_sun(&outdoor, authored, -50.0);
    let one = effective_sun(&outdoor, authored, 1.0);
    assert_eq!(
        (below.direction, below.color, below.illuminance),
        (one.direction, one.color, one.illuminance)
    );
}

// ---------------------------------------------------------------------------------------------
// HDR and EXR encodings.
// ---------------------------------------------------------------------------------------------

/// Catches an RGBE encoder that loses more than a mantissa step anywhere in its exponent range, or
/// writes a non-zero pixel for NaN, infinite or negative input.
#[test]
fn rgbe_round_trips_across_magnitudes_and_zeroes_bad_input() {
    for e in -30..=37 {
        for m in [1.0f32, 1.7, 3.3, 9.9] {
            let v = m * 10f32.powi(e);
            let c = [v, v * 0.5, v * 0.01];
            let back = rgbe_to_rgb(rgb_to_rgbe(c));
            let rel = (back[0] - v).abs() / v;
            assert!(rel <= 1.0 / 128.0 + 1e-6, "{v}: {back:?} (rel {rel})");
            assert!(back[1] <= back[0] && back[2] <= back[1]);
        }
    }
    for bad in [
        [f32::NAN; 3],
        [f32::INFINITY, 1.0, 1.0],
        [-1.0; 3],
        [0.0; 3],
        [1e-40; 3],
    ] {
        assert_eq!(rgb_to_rgbe(bad), [0; 4], "{bad:?}");
    }
    // One NaN channel next to valid ones decodes as the format's smallest step there, never NaN.
    let back = rgbe_to_rgb(rgb_to_rgbe([f32::NAN, 2.0, 1.0]));
    assert!(back.iter().all(|v| v.is_finite()), "{back:?}");
    assert!(back[0] < 0.01 && (back[1] - 2.0).abs() < 2.0 / 128.0, "{back:?}");
}

/// Catches an EXR encoder that refuses or corrupts non-finite samples (a firefly or a miss written
/// as inf must survive the file bit for bit) or accepts a zero-sized image.
#[test]
fn exr_keeps_non_finite_samples_and_refuses_empty_images() {
    let data = vec![
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.0,
        -0.0,
        f32::MAX,
        f32::MIN_POSITIVE,
    ];
    let ch = ExrChannel {
        name: "Y".into(),
        data: data.clone(),
    };
    let bytes = exr_encode(3, 2, std::slice::from_ref(&ch), &[]).expect("encode");
    let img = exr_decode(&bytes).expect("decode");
    let got = img.channel("Y").expect("Y");
    assert_eq!(
        got.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        data.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let nan = ExrChannel {
        name: "Y".into(),
        data: vec![f32::NAN; 6],
    };
    let img = exr_decode(&exr_encode(3, 2, &[nan], &[]).expect("encode")).expect("decode");
    assert!(img.channel("Y").expect("Y").iter().all(|v| v.is_nan()));
    assert!(exr_encode(0, 2, &[], &[]).is_err());
    assert!(exr_encode(3, 0, &[], &[]).is_err());
    assert!(
        exr_encode(
            3,
            2,
            std::slice::from_ref(&ExrChannel {
                name: "Y".into(),
                data: vec![0.0; 5]
            }),
            &[]
        )
        .is_err()
    );
}

/// Catches `layers()` listing a layer twice when another layer's channels sort between its own
/// (`a.A`, `a.B.x`, `a.R`): a caller that loads "the first layer with RGB" would try it twice.
#[test]
fn exr_layers_are_unique() {
    let plane = |name: &str| ExrChannel {
        name: name.into(),
        data: vec![0.0; 4],
    };
    let channels = [plane("a.A"), plane("a.B.x"), plane("a.R"), plane("R")];
    let img = exr_decode(&exr_encode(2, 2, &channels, &[]).expect("encode")).expect("decode");
    let layers = img.layers();
    let mut unique = layers.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(layers.len(), unique.len(), "{layers:?}");
    for l in ["", "a", "a.B"] {
        assert!(layers.iter().any(|x| x == l), "{l:?} missing from {layers:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Output sidecar.
// ---------------------------------------------------------------------------------------------

fn temp_dir(tag: &str) -> std::path::PathBuf {
    regia_test_utils::unique_temp_dir(tag)
}

/// Catches a metadata sidecar that does not round trip (a renamed key breaks the comparison
/// tooling), writes the empty denoiser fields, or panics on a NaN wall time.
#[test]
fn metadata_json_round_trips_and_omits_empty_denoise_fields() {
    let dir = temp_dir("tq_pt_meta");
    std::fs::create_dir_all(&dir).expect("dir");
    let mut img = PtImage {
        width: 1,
        height: 1,
        beauty: vec![[0.0; 3]],
        albedo: vec![[0.0; 3]],
        normal: vec![[0.0; 3]],
        depth: vec![0.0],
        direct: vec![[0.0; 3]],
        indirect_diffuse: vec![[0.0; 3]],
        indirect_specular: vec![[0.0; 3]],
        emissive: vec![[0.0; 3]],
        volumetric: vec![[0.0; 3]],
        sky: vec![[0.0; 3]],
        shadow: vec![0.0],
        roughness: vec![0.0],
        variance: vec![[0.0; 3]],
        spp: 4,
        spp_map: Vec::new(),
        metadata: regia_pt::PtMetadata {
            scene_hash: "00ff".into(),
            seed: 7,
            spp: 4,
            max_bounces: 3,
            device: "cpu".into(),
            git_tag: String::new(),
            wall_seconds: 1.5,
            rejected_samples: 0,
            triangles: 12,
            denoiser: String::new(),
            denoise_seconds: 0.0,
            surface_graphs: Default::default(),
        },
    };
    let path = dir.join("meta.json");
    img.write_metadata_json(&path).expect("write");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        !text.contains("denoiser") && !text.contains("denoise_seconds"),
        "{text}"
    );
    let back: regia_pt::PtMetadata = serde_json::from_str(&text).expect("parse");
    assert_eq!(back, img.metadata);

    img.metadata.wall_seconds = f64::NAN;
    img.metadata.denoiser = "oidn".into();
    img.metadata.denoise_seconds = 0.25;
    img.write_metadata_json(&path)
        .expect("a NaN wall time still writes");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.contains("\"denoiser\": \"oidn\""), "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert!(value["wall_seconds"].is_null());

    let missing = dir.join("no_such_dir").join("meta.json");
    assert!(img.write_metadata_json(&missing).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
