// SPDX-License-Identifier: MPL-2.0
//! Probe baking (`regia_pt::probe`, baked lighting phase 1): the probe path against analytic
//! answers and against the path tracer's own pixel estimate, separable components, validity,
//! relocation, the terrain layer, the asset round trip, and determinism per seed. Runs the
//! real kernels on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::f32::consts::PI;
use std::sync::OnceLock;

use regia_asset::baked_gi::{
    BakedGiVolume, GiComponent, ProbeFlags, SH_Y00, SH_Y1, ShRgb, VolumeLayout, irradiance,
};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::probe::{
    GiBakeSettings, ProbeTrace, ProbeTraceSettings, bake_gi_volume, detail_brick, terrain_layer,
};
use regia_pt::scenes::{self, quad, uv_sphere};
use regia_pt::{
    PathTracer, PtCamera, PtHeightfield, PtMaterial, PtScene, PtSettings, PtSky, PtSkyBody, PtSun,
};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings(rays: u32, samples: u32, bounces: u32) -> ProbeTraceSettings {
    ProbeTraceSettings {
        rays_per_probe: rays,
        samples,
        max_bounces: bounces,
        seed: 5,
        samples_per_submit: 16,
        ..ProbeTraceSettings::default()
    }
}

fn trace(scene: &PtScene, positions: &[[f32; 3]], s: &ProbeTraceSettings) -> Vec<ProbeTrace> {
    tracer()
        .trace_probes(scene, positions, s, |_, _| true)
        .expect("trace probes")
}

fn camera() -> PtCamera {
    PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4)
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn comp(t: &ProbeTrace, c: GiComponent) -> &ShRgb {
    &t.sh[c as usize]
}

fn assert_close(what: &str, got: f32, want: f32, tol: f32) {
    assert!(
        (got - want).abs() <= tol,
        "{what}: got {got}, want {want} (± {tol})"
    );
}

#[test]
fn furnace_probes_see_the_uniform_sky_and_nothing_else() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([1.0, 0.5, 0.25]);
    let positions = [[0.0, 0.0, 0.0], [10.0, -3.0, 2.0], [-1.0, 100.0, 7.0]];
    for t in trace(&scene, &positions, &settings(128, 8, 2)) {
        assert_eq!(t.rejected, 0);
        assert_eq!(t.rays, 128 * 8);
        assert_eq!(t.hits, 0);
        let sky = comp(&t, GiComponent::Sky);
        for (ch, want) in [1.0f32, 0.5, 0.25].iter().enumerate() {
            assert_close("L00", sky[ch][0], 4.0 * PI * SH_Y00 * want, 1e-3 * want);
            for m in 1..4 {
                assert!(sky[ch][m].abs() < 0.02 * want, "L1 {:?}", sky[ch]);
            }
        }
        for n in [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]] {
            assert_close("E/π", irradiance(sky, n)[0] / PI, 1.0, 0.01);
        }
        for c in [GiComponent::SunBounce, GiComponent::Fill, GiComponent::Emissive] {
            assert!(comp(&t, c).iter().flatten().all(|v| *v == 0.0), "{c:?}");
        }
        // Escaped rays count as the clamp distance.
        assert!(t.moments.iter().all(|m| (m[0] - 32.0).abs() < 1e-3));
    }
}

#[test]
fn integrating_sphere_centre_probe_matches_the_analytic_radiance() {
    // Inside a closed sphere of albedo a with an emitting cap (radiance e, area fraction f) the
    // walls carry a·f·e/(1 − a) everywhere; at the centre the cap subtends the solid-angle
    // fraction f, so the mean incident radiance is f·e/(1 − a), and the cap's first moment is
    // e·π·(1 − c²) along +Y.
    let (a, e, c) = (0.5f32, 1.0f32, 0.5f32);
    let (scene, f) = scenes::closed_sphere(a, e, c, 4);
    let t = &trace(&scene, &[[0.0; 3]], &settings(256, 64, 64))[0];
    assert_eq!(t.rejected, 0);
    let em = comp(t, GiComponent::Emissive);
    let mean = f as f32 * e / (1.0 - a);
    let l1y = SH_Y1 * e * PI * (1.0 - c * c);
    eprintln!(
        "integrating sphere: L00 {} want {}, L1y {} want {}",
        em[0][0],
        4.0 * PI * SH_Y00 * mean,
        em[0][2],
        l1y
    );
    assert_close(
        "L00",
        em[0][0],
        4.0 * PI * SH_Y00 * mean,
        0.02 * 4.0 * PI * SH_Y00 * mean,
    );
    assert_close("L1y", em[0][2], l1y, 0.03 * l1y);
    assert!(
        em[0][1].abs() < 0.03 * l1y && em[0][3].abs() < 0.03 * l1y,
        "{em:?}"
    );
    // Every ray hits the inside of the sphere: front faces at radius 1.
    assert_eq!(t.hits, t.rays);
    assert_eq!(t.back_faces, 0);
    assert!(
        t.moments.iter().all(|m| (m[0] - 1.0).abs() < 0.01),
        "{:?}",
        t.moments
    );
}

/// Irradiance at `p` on a tiny ideal-Lambertian (albedo 1) patch facing `n`, from the path
/// tracer's own camera estimate: the patch's radiance is `E/π`.
fn pixel_irradiance(base: &PtScene, p: [f32; 3], n: [f32; 3], bounces: u32) -> f32 {
    let mut scene = base.clone();
    let (p, n) = (Vec3::from_array(p), Vec3::from_array(n));
    let a = if n.y.abs() < 0.9 { Vec3::Y } else { Vec3::Z };
    let u = a.cross(n).normalize();
    let v = n.cross(u);
    let h = 0.004;
    let mesh = scene.add_mesh(quad([
        (p - u * h - v * h).to_array(),
        (p + u * h - v * h).to_array(),
        (p + u * h + v * h).to_array(),
        (p - u * h + v * h).to_array(),
    ]));
    let white = scene.add_material(PtMaterial::lambert([1.0; 3]));
    scene.add_instance(mesh, white, Mat4::IDENTITY);
    let eye = p + n * 0.05 + u * 1e-4;
    scene.camera = PtCamera::pinhole(eye.to_array(), p.to_array(), 0.08, 4, 4);
    let image = tracer()
        .render(
            &scene,
            &PtSettings {
                spp: 4096,
                max_bounces: bounces,
                seed: 11,
                rr_start: 64,
                jitter: false,
                samples_per_submit: 128,
            },
        )
        .expect("render");
    let count = image.beauty.len() as f32;
    PI * image.beauty.iter().map(|c| lum(*c)).sum::<f32>() / count
}

#[test]
fn cornell_probe_irradiance_matches_the_path_tracers_pixel_estimate() {
    let scene = scenes::cornell_box(4, 4, true);
    let p = [0.278, 0.42, 0.28];
    let bounces = 6;
    let probe_settings = ProbeTraceSettings {
        rr_start: 64,
        ..settings(256, 128, bounces)
    };
    let t = &trace(&scene, &[p], &probe_settings)[0];
    assert_eq!(t.rejected, 0);
    let em = comp(t, GiComponent::Emissive);
    let mut row = [0.0f32; 4];
    for (m, v) in row.iter_mut().enumerate() {
        *v = 0.2126 * em[0][m] + 0.7152 * em[1][m] + 0.0722 * em[2][m];
    }
    let lum_sh: ShRgb = [row; 3];
    for (axis, n) in [("Y", [0.0f32, 1.0, 0.0]), ("X", [1.0, 0.0, 0.0])] {
        let neg = [-n[0], -n[1], -n[2]];
        // The camera path has one more vertex (the patch).
        let pt_pos = pixel_irradiance(&scene, p, n, bounces + 1);
        let pt_neg = pixel_irradiance(&scene, p, neg, bounces + 1);
        // E(n) − E(−n) = ∫ L (n·ω) dω is exactly the linear band, 2·(2π/3)·Y1·(L1·n): no L1
        // truncation, only noise.
        let linear = 2.0 * (2.0 * PI / 3.0) * SH_Y1 * (row[1] * n[0] + row[2] * n[1] + row[3] * n[2]);
        let probe_pos = irradiance(&lum_sh, n)[0];
        let probe_neg = irradiance(&lum_sh, neg)[0];
        eprintln!(
            "cornell {axis}: path tracer E+ {pt_pos} E- {pt_neg} (diff {}); probe diff {linear}, \
             L1 E+ {probe_pos} E- {probe_neg}",
            pt_pos - pt_neg
        );
        assert_close(
            &format!("E+ − E- along {axis}"),
            linear,
            pt_pos - pt_neg,
            0.04 * (pt_pos + pt_neg),
        );
        if axis == "Y" {
            // E(n) itself carries L1's truncation: the light 13 cm overhead reads ~25 % low
            // (a point source overhead reads exactly 75 %; Ramamoorthi & Hanrahan 2001), and the
            // floor side rings below zero and clamps to 0 (reported, not asserted).
            assert_close("E↑ (L1)", probe_pos, pt_pos, 0.3 * pt_pos);
        }
    }
}

#[test]
fn sun_bounce_excludes_the_direct_sun() {
    let rho = 0.5;
    let e_sun = 1000.0;
    let elevation = 1.0f32;
    let mut scene = scenes::sun_plane(PtMaterial::lambert([rho; 3]), elevation, e_sun, 0.0, 4);
    scene.sky = PtSky::None;
    let t = &trace(&scene, &[[0.0, 1.0, 0.0]], &settings(256, 32, 1))[0];
    let sun = comp(t, GiComponent::SunBounce);
    // The ground is a uniform hemisphere of radiance ρ·E·sin(elev)/π below; the sun above is
    // live light. A uniform hemisphere is exact in L1 for ±Y.
    let lp = rho * e_sun * elevation.sin() / PI;
    let down = irradiance(sun, [0.0, -1.0, 0.0])[0];
    let up = irradiance(sun, [0.0, 1.0, 0.0])[0];
    eprintln!("sun bounce: E↓ {down} want {}, E↑ {up} want 0", PI * lp);
    assert_close("E↓", down, PI * lp, 0.03 * PI * lp);
    assert!(
        up < 0.03 * PI * lp,
        "the direct sun leaked into the bake: E↑ {up}"
    );
    for c in [GiComponent::Sky, GiComponent::Fill, GiComponent::Emissive] {
        assert!(comp(t, c).iter().flatten().all(|v| *v == 0.0), "{c:?}");
    }
}

#[test]
fn a_sky_body_lands_in_the_fill_component() {
    let dir = [0.6f32, 0.8, 0.0];
    let (r, radiance) = (0.6f32, 2.0f32);
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([0.1; 3]);
    scene.sky_bodies.push(PtSkyBody::uniform(dir, r, [radiance; 3]));
    let t = &trace(&scene, &[[0.0; 3]], &settings(256, 64, 1))[0];
    let fill = comp(t, GiComponent::Fill);
    let l00 = SH_Y00 * radiance * 2.0 * PI * (1.0 - r.cos());
    let l1 = SH_Y1 * radiance * PI * r.sin() * r.sin();
    let along = fill[0][1] * dir[0] + fill[0][2] * dir[1] + fill[0][3] * dir[2];
    eprintln!("fill: L00 {} want {l00}, L1·d {along} want {l1}", fill[0][0]);
    assert_close("fill L00", fill[0][0], l00, 0.03 * l00);
    assert_close("fill L1", along, l1, 0.03 * l1);
    let sky = comp(t, GiComponent::Sky);
    assert_close("sky L00", sky[0][0], 4.0 * PI * SH_Y00 * 0.1, 1e-3);
}

#[test]
fn probes_inside_geometry_are_invalid_and_probes_near_a_wall_are_relocated() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([1.0; 3]);
    let ball = scene.add_mesh(uv_sphere(0.5, 32, 16, false));
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    scene.add_instance(ball, grey, Mat4::IDENTITY);
    // A 3×1×1 brick, spacing 0.53: probe 0 at the ball's centre, probe 1 just outside its
    // surface (0.03 m), probe 2 in the open.
    let mut brick = detail_brick([0.0, 0.0, 0.0], [1.06, 0.0, 0.0], 0.53);
    brick.counts = [3, 1, 1];
    brick.spacing = [0.53, 0.53, 0.53];
    let layout = VolumeLayout {
        terrain: None,
        bricks: vec![brick],
        moment_resolution: 4,
        max_distance: 4.0,
    };
    let bake_settings = GiBakeSettings {
        trace: settings(64, 4, 2),
        min_front_face_fraction: 0.2,
        ..GiBakeSettings::default()
    };
    let bake = bake_gi_volume(tracer(), &scene, &layout, &bake_settings, |_, _, _| true).expect("bake");
    let v = &bake.volume;
    let m0 = v.probe_meta(0);
    assert!(
        !m0.is_valid() && m0.flags & ProbeFlags::INSIDE_GEOMETRY != 0,
        "{m0:?}"
    );
    let m1 = v.probe_meta(1);
    assert!(m1.is_valid() && m1.flags & ProbeFlags::RELOCATED != 0, "{m1:?}");
    assert!(m1.offset[0] > 0.05, "pushed away from the ball: {:?}", m1.offset);
    let m2 = v.probe_meta(2);
    assert!(m2.is_valid() && m2.flags & ProbeFlags::RELOCATED == 0, "{m2:?}");
    assert_eq!(bake.report.inside_geometry, 1);
    assert_eq!(bake.report.valid, 2);
}

fn flat_terrain(scene: &mut PtScene, size: u32, spacing: f32, height: impl Fn(f32, f32) -> f32) {
    let half = (size - 1) as f32 * spacing * 0.5;
    let mut heights = Vec::new();
    for j in 0..size {
        for i in 0..size {
            heights.push(height(i as f32 * spacing - half, j as f32 * spacing - half));
        }
    }
    let ground = scene.add_material(PtMaterial::lambert([0.3; 3]));
    scene.terrain = Some(PtHeightfield {
        width: size,
        depth: size,
        origin: [-half, -half],
        spacing: [spacing, spacing],
        heights,
        material: ground,
    });
}

#[test]
fn probes_under_the_terrain_are_invalid() {
    let mut scene = PtScene::new(camera());
    scene.sky = PtSky::Uniform([1.0; 3]);
    flat_terrain(&mut scene, 33, 1.0, |_, _| 0.0);
    let mut brick = detail_brick([0.0, -1.0, 0.0], [0.0, 1.0, 0.0], 2.0);
    brick.counts = [1, 2, 1];
    let layout = VolumeLayout {
        terrain: None,
        bricks: vec![brick],
        moment_resolution: 4,
        max_distance: 8.0,
    };
    let bake = bake_gi_volume(
        tracer(),
        &scene,
        &layout,
        &GiBakeSettings {
            trace: settings(32, 2, 1),
            ..GiBakeSettings::default()
        },
        |_, _, _| true,
    )
    .expect("bake");
    let below = bake.volume.probe_meta(0);
    assert!(
        !below.is_valid() && below.flags & ProbeFlags::UNDER_TERRAIN != 0,
        "{below:?}"
    );
    assert!(bake.volume.probe_meta(1).is_valid());
    assert_eq!(bake.report.under_terrain, 1);
}

#[test]
fn terrain_layer_bake_round_trips_and_sees_the_sky_above_flat_ground() {
    let mut scene = PtScene::new(camera());
    let sky = 2.0f32;
    scene.sky = PtSky::Uniform([sky; 3]);
    scene.sun = Some(PtSun {
        toward: [0.3, 0.9, 0.3],
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    // 512 m of flat ground so the horizon gap under the interior probes stays below 1.5°.
    flat_terrain(&mut scene, 513, 1.0, |_, _| 3.0);
    let hf = scene.terrain.clone().expect("terrain");
    let layer = terrain_layer(&hf, [128.0, 128.0], &[1.0, 2.0, 4.0]).expect("layer");
    assert_eq!(layer.counts, [4, 4]);
    let layout = VolumeLayout {
        terrain: Some(layer),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    let bake = bake_gi_volume(
        tracer(),
        &scene,
        &layout,
        &GiBakeSettings {
            trace: settings(128, 8, 2),
            ..GiBakeSettings::default()
        },
        |_, _, _| true,
    )
    .expect("bake");
    let v = &bake.volume;
    assert_eq!(bake.report.valid, 48);
    assert_eq!(bake.report.rejected_rays, 0);
    let bytes = v.to_bytes().expect("encode");
    let back = BakedGiVolume::from_bytes(&bytes).expect("decode");
    assert_eq!(&back, v);
    assert_eq!(back.lights.sun_illuminance, 1000.0);
    assert!((back.lights.sky_scale - sky).abs() < 1e-5);
    let t = back.terrain.as_ref().expect("layer");
    for j in 1..3 {
        for i in 1..3 {
            for l in 0..3 {
                let idx = t.index(i, j, l);
                let p = back.probe_position(idx);
                assert!((p[1] - 3.0 - [1.0, 2.0, 4.0][l as usize]).abs() < 1e-3, "{p:?}");
                // Open sky above flat ground: E↑ = π·L exactly (a hemisphere split is exact in
                // L1 for ±Y), up to the encoding and the filter.
                let up = back.irradiance(idx, GiComponent::Sky, [0.0, 1.0, 0.0])[0];
                assert_close("sky E↑", up, PI * sky, 0.04 * PI * sky);
                // Sun bounce reaches the probe from the lit ground below only.
                let sb_down = back.irradiance(idx, GiComponent::SunBounce, [0.0, -1.0, 0.0])[0];
                let lp = 0.3 * 1000.0 * (0.9f32 / (0.3f32 * 0.3 + 0.81 + 0.09).sqrt()) / PI;
                assert_close("sun bounce E↓", sb_down, PI * lp, 0.06 * PI * lp);
                // The ground blocks a point 2 m under a level-1 probe; the sky is open.
                if l == 0 {
                    assert!(back.visibility(idx, [0.0, -1.0, 0.0], 2.0) < 0.2);
                }
                assert_eq!(back.visibility(idx, [0.0, 1.0, 0.0], 10.0), 1.0);
            }
        }
    }
}

#[test]
fn a_bake_is_deterministic_per_seed_and_independent_of_batching() {
    let scene = scenes::cornell_box(4, 4, true);
    let positions = [
        [0.1, 0.1, 0.1],
        [0.278, 0.274, 0.28],
        [0.45, 0.4, 0.45],
        [0.2, 0.5, 0.3],
    ];
    let s = settings(64, 4, 3);
    let a = trace(&scene, &positions, &s);
    let b = trace(
        &scene,
        &positions,
        &ProbeTraceSettings {
            probes_per_batch: 1,
            samples_per_submit: 3,
            ..s
        },
    );
    assert_eq!(a, b, "same seed, different batching");
    let c = trace(&scene, &positions, &ProbeTraceSettings { seed: 6, ..s });
    assert_ne!(a[1].sh, c[1].sh, "a different seed must change the noise");
    // Whole-volume bakes are byte-identical too.
    let layout = VolumeLayout {
        terrain: None,
        bricks: vec![detail_brick([0.1, 0.1, 0.1], [0.45, 0.45, 0.45], 0.2)],
        moment_resolution: 4,
        max_distance: 1.0,
    };
    let bake = |seed| {
        bake_gi_volume(
            tracer(),
            &scene,
            &layout,
            &GiBakeSettings {
                trace: ProbeTraceSettings { seed, ..s },
                ..GiBakeSettings::default()
            },
            |_, _, _| true,
        )
        .expect("bake")
        .volume
        .to_bytes()
        .expect("bytes")
    };
    assert_eq!(bake(9), bake(9));
}
