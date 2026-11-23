// SPDX-License-Identifier: MPL-2.0
//! `specs/27` § 1.5: the path tracer's own acceptance. Runs the real kernels on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI) at small sizes.

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::cpu_brdf::{DfgTable, brdf_cos_vec, directional_albedo};
use regia_pt::radiosity::Radiosity;
use regia_pt::scenes::{self, cornell_room_quads};
use regia_pt::{
    Aov, PathTracer, PtCamera, PtHeightfield, PtImage, PtMaterial, PtMesh, PtScene, PtSettings, PtSky, PtSun,
    PtTexture,
};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn render(scene: &PtScene, spp: u32, bounces: u32, jitter: bool) -> PtImage {
    let settings = PtSettings {
        spp,
        max_bounces: bounces,
        seed: 7,
        rr_start: 3,
        jitter,
        samples_per_submit: 64,
    };
    tracer().render(scene, &settings).expect("render")
}

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

/// Camera ray through the centre of pixel (x, y), the same construction as `generate`.
fn pixel_ray(cam: &PtCamera, x: u32, y: u32) -> (Vec3, Vec3) {
    let (fwd, right, up) = cam.basis();
    let th = (0.5 * cam.fov_y).tan();
    let aspect = cam.width as f32 / cam.height as f32;
    let sx = ((x as f32 + 0.5) / cam.width as f32) * 2.0 - 1.0;
    let sy = 1.0 - ((y as f32 + 0.5) / cam.height as f32) * 2.0;
    let d = (fwd + right * (sx * th * aspect) + up * (sy * th)).normalize();
    (Vec3::from_array(cam.eye), d)
}

#[test]
fn furnace_sphere_matches_the_brdf_albedo() {
    let dfg = DfgTable::default();
    let mut configs = vec![
        ("ideal lambert", PtMaterial::lambert([1.0; 3])),
        ("white dielectric", PtMaterial::diffuse([1.0; 3])),
    ];
    for r in [0.05f32, 0.25, 0.5, 1.0] {
        configs.push((
            "metal",
            PtMaterial {
                base_color: [1.0; 4],
                metallic: 1.0,
                roughness: r,
                multi_scatter: true,
                ..PtMaterial::default()
            },
        ));
        configs.push((
            "dielectric",
            PtMaterial {
                base_color: [1.0; 4],
                metallic: 0.0,
                roughness: r,
                multi_scatter: true,
                ..PtMaterial::default()
            },
        ));
    }
    let size = 16;
    let mut report = Vec::new();
    for (name, m) in configs {
        let scene = scenes::furnace_sphere(m.clone(), size);
        let img = render(&scene, 1024, 4, false);
        // Expected: each pixel's directional albedo at its n·v (no inter-reflection on a sphere).
        let table: Vec<f64> = (0..=32)
            .map(|i| directional_albedo(&m, &dfg, (i as f32 / 32.0).max(0.02), 0, 512, 256))
            .collect();
        let albedo_at = |no_v: f32| {
            let f = no_v.clamp(0.0, 1.0) * 32.0;
            let i = (f.floor() as usize).min(31);
            let t = f64::from(f - i as f32);
            table[i] * (1.0 - t) + table[i + 1] * t
        };
        let (mut got, mut want, mut n) = (0.0f64, 0.0f64, 0usize);
        for y in 0..size {
            for x in 0..size {
                let (o, d) = pixel_ray(&scene.camera, x, y);
                // Ray–unit-sphere hit; the normal is the hit point.
                let b = o.dot(d);
                let c = o.dot(o) - 1.0;
                let disc = b * b - c;
                assert!(disc > 0.0, "every pixel sees the sphere");
                let t = -b - disc.sqrt();
                let nrm = (o + d * t).normalize();
                got += f64::from(img.at(Aov::Beauty, x, y)[0]);
                want += albedo_at(nrm.dot(-d));
                n += 1;
            }
        }
        let (got, want) = (got / n as f64, want / n as f64);
        let rel = (got / want - 1.0).abs();
        report.push(format!(
            "{name} r={} pt={got:.4} brdf={want:.4} rel={rel:.4}",
            m.roughness
        ));
        assert!(
            rel < 0.005,
            "furnace {name} roughness {}: pt {got} vs brdf albedo {want}",
            m.roughness
        );
        assert_eq!(img.metadata.rejected_samples, 0);
    }
    eprintln!("furnace:\n{}", report.join("\n"));
}

#[test]
fn closed_sphere_converges_to_the_bounce_series() {
    let albedo = 0.5;
    let (scene, fraction) = scenes::closed_sphere(albedo, 1.0, 0.5, 24);
    let img = render(&scene, 1024, 24, true);
    let expected = f64::from(albedo) * fraction / (1.0 - f64::from(albedo));
    let mut sum = 0.0;
    let mut n = 0usize;
    for (b, e) in img.beauty.iter().zip(&img.emissive) {
        if e[0] == 0.0 {
            sum += lum(*b);
            n += 1;
        }
    }
    assert!(n > 0);
    let got = sum / n as f64;
    let rel = (got / expected - 1.0).abs();
    eprintln!("closed sphere: pt {got:.5} analytic {expected:.5} rel {rel:.4}");
    assert!(rel < 0.01, "closed sphere: {got} vs {expected}");
}

fn plane_reference(scene: &PtScene, m: &PtMaterial, l: Vec3, x: u32, y: u32) -> Option<f64> {
    let (o, d) = pixel_ray(&scene.camera, x, y);
    if d.y >= 0.0 {
        return None;
    }
    let _hit = o + d * (-o.y / d.y);
    let dfg = DfgTable::default();
    Some(f64::from(brdf_cos_vec(
        m,
        &dfg,
        [0.0, 1.0, 0.0],
        (-d).to_array(),
        l.to_array(),
        0,
    )))
}

#[test]
fn plane_under_a_sun_reads_the_analytic_direct_term() {
    let m = PtMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        roughness: 0.6,
        ..PtMaterial::default()
    };
    let e = 1000.0f32;
    let el = 30f32.to_radians();
    for (radius, spp, tol) in [(0.0f32, 1u32, 1e-4f64), (0.004_65, 256, 0.005)] {
        let scene = scenes::sun_plane(m.clone(), el, e, radius, 16);
        let sun = scene.sun.expect("sun");
        let l = Vec3::from_array(sun.toward).normalize();
        let img = render(&scene, spp, 1, false);
        let (mut got, mut want) = (0.0f64, 0.0f64);
        for y in 0..16 {
            for x in 0..16 {
                if let Some(f) = plane_reference(&scene, &m, l, x, y) {
                    got += f64::from(img.at(Aov::Direct, x, y)[0]);
                    want += f * f64::from(e);
                }
            }
        }
        let rel = (got / want - 1.0).abs();
        eprintln!("sun plane radius {radius}: pt {got:.4} analytic {want:.4} rel {rel:.6}");
        assert!(rel < tol, "sun radius {radius}: {got} vs {want}");
        // Lambert part of the check: E·sinθ/π scaled by the BRDF's diffuse share stays the
        // dominant term, and the shadow AOV says the sun is visible everywhere.
        assert!(img.shadow.iter().all(|&s| s > 0.999));
    }
}

#[test]
fn plane_under_a_sphere_light_matches_the_form_factor() {
    let m = PtMaterial::lambert([0.8; 3]);
    let (r, h, radiance) = (0.1f32, 1.0f32, 100.0f32);
    let scene = scenes::sphere_light_plane(m.clone(), r, h, radiance, 16);
    let img = render(&scene, 1024, 1, false);
    let dfg = DfgTable::default();
    let c = Vec3::new(0.0, h, 0.0);
    let (mut got, mut want) = (0.0f64, 0.0f64);
    for y in 0..16 {
        for x in 0..16 {
            let (o, d) = pixel_ray(&scene.camera, x, y);
            let p = o + d * (-o.y / d.y);
            let to = c - p;
            let dist = to.length();
            let l = to / dist;
            // Irradiance from a sphere wholly above the horizon: π L (r/D)² cos θ.
            let irr = std::f32::consts::PI * radiance * (r / dist).powi(2) * l.y;
            let f = brdf_cos_vec(&m, &dfg, [0.0, 1.0, 0.0], (-d).to_array(), l.to_array(), 0) / l.y;
            got += f64::from(img.at(Aov::Direct, x, y)[0]);
            want += f64::from(f * irr);
        }
    }
    let rel = (got / want - 1.0).abs();
    eprintln!("sphere light: pt {got:.4} analytic {want:.4} rel {rel:.5}");
    assert!(rel < 0.005, "sphere light: {got} vs {want}");
}

#[test]
fn cornell_room_matches_an_independent_radiosity_solution() {
    let quads = cornell_room_quads();
    let size = 40;
    let scene = {
        let mut s = PtScene::new(scenes::cornell_camera(size, size));
        scenes::add_room(&mut s, &quads);
        s
    };
    let img = render(&scene, 4096, 16, false);
    let rad = Radiosity::solve(&quads, 0.03);
    let (mut err, mut n) = (0.0f64, 0usize);
    let (mut sum_pt, mut sum_ref) = (0.0f64, 0.0f64);
    for y in 0..size {
        for x in 0..size {
            let (o, d) = pixel_ray(&scene.camera, x, y);
            let Some((q, p)) = rad.hit(o.as_dvec3().to_array(), d.as_dvec3().to_array()) else {
                continue;
            };
            if quads[q].emission.iter().any(|&e| e > 0.0) {
                continue;
            }
            let r = rad.radiance_at(q, p);
            let reference = 0.2126 * r[0] + 0.7152 * r[1] + 0.0722 * r[2];
            let got = lum(img.at(Aov::Beauty, x, y));
            if reference <= 0.0 {
                continue;
            }
            err += (got - reference).abs() / reference;
            sum_pt += got;
            sum_ref += reference;
            n += 1;
        }
    }
    let mre = err / n as f64;
    eprintln!(
        "cornell room: {n} px, mean relative error {mre:.4}, mean pt {:.4} vs radiosity {:.4} ({} patches)",
        sum_pt / n as f64,
        sum_ref / n as f64,
        rad.patch_count()
    );
    assert!(mre < 0.02, "Cornell room mean relative error {mre}");
}

#[test]
fn same_seed_is_bit_identical() {
    let scene = scenes::cornell_box(24, 24, true);
    let a = render(&scene, 8, 8, true);
    let b = render(&scene, 8, 8, true);
    assert_eq!(a.beauty, b.beauty);
    assert_eq!(a.indirect_diffuse, b.indirect_diffuse);
    let settings = PtSettings {
        spp: 8,
        max_bounces: 8,
        seed: 8,
        ..PtSettings::default()
    };
    let c = tracer().render(&scene, &settings).expect("render");
    assert_ne!(a.beauty, c.beauty, "another seed gives another noise pattern");
}

#[test]
fn sky_aov_equals_the_runtime_sky_table() {
    let params = regia_render::AtmosphereParameters::default();
    let sun = Vec3::new(0.4, 0.5, -0.3).normalize().to_array();
    let luts = regia_render::build_atmosphere_luts_for(&params, sun, 2.0);
    let cam = PtCamera {
        eye: [0.0, 2.0, 0.0],
        target: [1.0, 2.2, 0.3],
        fov_y: 1.4,
        width: 32,
        height: 16,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let scene = scenes::empty_sky(PtSky::from_atmosphere(&luts), cam);
    let img = render(&scene, 1, 1, false);
    let mut worst = 0.0f64;
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (_, d) = pixel_ray(&cam, x, y);
            let want = regia_render::sample_sky_table(&luts, d.to_array());
            let got = img.at(Aov::Sky, x, y);
            let rel = (lum(got) / lum(want) - 1.0).abs();
            worst = worst.max(rel);
        }
    }
    eprintln!("sky table: worst relative error {worst:.6}");
    assert!(worst < 0.01, "sky AOV vs runtime table: {worst}");
}

fn bumpy_heights(n: u32) -> Vec<f32> {
    (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32, (i / n) as f32);
            (x * 0.7).sin() * 1.5 + (z * 0.45).cos() * 2.0 + (x * z * 0.05).sin()
        })
        .collect()
}

fn heightfield_mesh(t: &PtHeightfield) -> PtMesh {
    let mut m = PtMesh::default();
    for j in 0..t.depth {
        for i in 0..t.width {
            m.positions.push([
                t.origin[0] + i as f32 * t.spacing[0],
                t.height(i, j),
                t.origin[1] + j as f32 * t.spacing[1],
            ]);
            m.normals.push([0.0, 1.0, 0.0]);
        }
    }
    for j in 0..t.depth - 1 {
        for i in 0..t.width - 1 {
            let v00 = j * t.width + i;
            let v01 = (j + 1) * t.width + i;
            let (v10, v11) = (v00 + 1, v01 + 1);
            m.indices.extend_from_slice(&[v00, v01, v10, v10, v01, v11]);
        }
    }
    m
}

#[test]
fn heightfield_traversal_matches_the_same_triangles_as_a_mesh() {
    let n = 41;
    let cam = PtCamera {
        eye: [-25.0, 18.0, -22.0],
        target: [5.0, 0.0, 4.0],
        fov_y: 0.9,
        width: 48,
        height: 32,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let hf = PtHeightfield {
        width: n,
        depth: n,
        origin: [-20.0, -20.0],
        spacing: [1.0, 1.0],
        heights: bumpy_heights(n),
        material: 0,
    };
    let mut a = PtScene::new(cam);
    a.add_material(PtMaterial::diffuse([0.5; 3]));
    a.terrain = Some(hf.clone());
    let mut b = PtScene::new(cam);
    let mat = b.add_material(PtMaterial::diffuse([0.5; 3]));
    let mesh = b.add_mesh(heightfield_mesh(&hf));
    b.add_instance(mesh, mat, Mat4::IDENTITY);
    let ia = render(&a, 1, 1, false);
    let ib = render(&b, 1, 1, false);
    let mut hits = 0;
    for (da, db) in ia.depth.iter().zip(&ib.depth) {
        assert_eq!(*da > 0.0, *db > 0.0, "same coverage");
        if *da > 0.0 {
            hits += 1;
            assert!((da - db).abs() <= 1e-3 * db.abs().max(1.0), "depth {da} vs {db}");
        }
    }
    assert!(hits > 100, "the camera sees the terrain");
}

/// `PtViewAlbedo`: a camera ray's heightfield hit reads the raster albedo of its own pixel
/// (the projection lines up texel for texel with the tracer's pixel grid); uncovered texels
/// (alpha 0) and everything but the heightfield keep the material.
#[test]
fn heightfield_view_albedo_replaces_the_camera_hit_albedo_pixel_for_pixel() {
    let n = 41;
    let cam = PtCamera {
        eye: [-25.0, 18.0, -22.0],
        target: [5.0, 0.0, 4.0],
        fov_y: 0.9,
        width: 48,
        height: 32,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let mut scene = PtScene::new(cam);
    scene.add_material(PtMaterial::diffuse([0.5; 3]));
    scene.terrain = Some(PtHeightfield {
        width: n,
        depth: n,
        origin: [-20.0, -20.0],
        spacing: [1.0, 1.0],
        heights: bumpy_heights(n),
        material: 0,
    });
    let covered = |x: u32| x < cam.width * 3 / 4;
    let texel = |x: u32, y: u32| [(x * 5 % 256) as u8, (y * 7 % 256) as u8, 200u8];
    let mut rgba = Vec::new();
    for y in 0..cam.height {
        for x in 0..cam.width {
            let [r, g, b] = texel(x, y);
            rgba.extend_from_slice(&[r, g, b, if covered(x) { 255 } else { 0 }]);
        }
    }
    let aspect = cam.width as f32 / cam.height as f32;
    let view_proj = (regia_core::glam::camera::rh::proj::directx::perspective(cam.fov_y, aspect, 0.1, 500.0)
        * regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::from_array(cam.eye),
            Vec3::from_array(cam.target),
            Vec3::Y,
        ))
    .to_cols_array_2d();
    scene.set_terrain_view_albedo(
        view_proj,
        cam.eye,
        regia_pt::PtTexture {
            width: cam.width,
            height: cam.height,
            rgba: std::sync::Arc::from(rgba),
            srgb: false,
        },
        None,
    );
    let img = render(&scene, 1, 1, false);
    let (mut from_view, mut from_material) = (0, 0);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let i = (y * cam.width + x) as usize;
            if img.depth[i] <= 0.0 {
                continue;
            }
            let got = img.albedo[i];
            let want = if covered(x) {
                from_view += 1;
                texel(x, y).map(|c| 0.5 * f32::from(c) / 255.0)
            } else {
                from_material += 1;
                [0.5; 3]
            };
            for k in 0..3 {
                assert!(
                    (got[k] - want[k]).abs() < 2e-3,
                    "pixel ({x}, {y}) channel {k}: {} vs {}",
                    got[k],
                    want[k]
                );
            }
        }
    }
    assert!(
        from_view > 400 && from_material > 100,
        "{from_view} / {from_material}"
    );
}

/// `PtViewAlbedo::normal`: a camera ray's heightfield hit shades with the raster's normal of
/// its pixel (the first-hit normal AOV reads it back); uncovered texels keep the heightfield's.
#[test]
fn heightfield_view_normal_replaces_the_camera_hit_normal() {
    let cam = PtCamera {
        eye: [-25.0, 18.0, -22.0],
        target: [5.0, 0.0, 4.0],
        fov_y: 0.9,
        width: 48,
        height: 32,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let n = 41;
    let mut scene = PtScene::new(cam);
    scene.add_material(PtMaterial::diffuse([0.5; 3]));
    scene.terrain = Some(PtHeightfield {
        width: n,
        depth: n,
        origin: [-20.0, -20.0],
        spacing: [1.0, 1.0],
        heights: vec![0.0; (n * n) as usize],
        material: 0,
    });
    // Tilted toward the camera (−X, −Z side), so it faces the eye and stays above the plane.
    let tilt = Vec3::new(-0.3, 0.9, -0.25).normalize();
    let byte = |v: f32| (255.0 * (0.5 + 0.5 * v)).round() as u8;
    let covered = |x: u32| x < cam.width / 2;
    let mut albedo = Vec::new();
    let mut normal = Vec::new();
    for _ in 0..cam.height {
        for x in 0..cam.width {
            let a = if covered(x) { 255 } else { 0 };
            albedo.extend_from_slice(&[128, 128, 128, a]);
            normal.extend_from_slice(&[byte(tilt.x), byte(tilt.y), byte(tilt.z), a]);
        }
    }
    let aspect = cam.width as f32 / cam.height as f32;
    let view_proj = (regia_core::glam::camera::rh::proj::directx::perspective(cam.fov_y, aspect, 0.1, 500.0)
        * regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::from_array(cam.eye),
            Vec3::from_array(cam.target),
            Vec3::Y,
        ))
    .to_cols_array_2d();
    let texture = |rgba: Vec<u8>| regia_pt::PtTexture {
        width: cam.width,
        height: cam.height,
        rgba: std::sync::Arc::from(rgba),
        srgb: false,
    };
    scene.set_terrain_view_albedo(view_proj, cam.eye, texture(albedo), Some(texture(normal)));
    let img = render(&scene, 1, 1, false);
    let (mut tilted, mut flat) = (0, 0);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let i = (y * cam.width + x) as usize;
            if img.depth[i] <= 0.0 {
                continue;
            }
            let got = Vec3::from_array(img.normal[i]);
            let want = if covered(x) {
                tilted += 1;
                tilt
            } else {
                flat += 1;
                Vec3::Y
            };
            assert!(got.distance(want) < 0.01, "pixel ({x}, {y}): {got} vs {want}");
        }
    }
    assert!(tilted > 200 && flat > 200, "{tilted} / {flat}");
}

/// The shadow terminator on a coarse heightfield: on a convex cap every point whose (smooth)
/// normal faces the sun is lit, but at a grazing sun the cap's flat facets tilt away from it
/// where the shading normal still faces it, and neighbouring facets shadow each other. The
/// tracer starts heightfield shadow rays on the smooth surface and tests facing with its
/// normal (Hanika 2021), so the cap has no dark facets on its lit side.
#[test]
fn heightfield_facets_do_not_shadow_a_convex_cap_at_a_grazing_sun() {
    let n = 17;
    let spacing = 4.0;
    let half = (n - 1) as f32 * spacing * 0.5;
    let radius = 20.0f32;
    let heights = (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32 * spacing - half, (i / n) as f32 * spacing - half);
            (radius * radius - x * x - z * z).max(0.0).sqrt()
        })
        .collect();
    let mut scene = PtScene::new(PtCamera {
        eye: [10.0, 45.0, 35.0],
        target: [0.0, 10.0, 0.0],
        fov_y: 0.9,
        width: 64,
        height: 48,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    scene.add_material(PtMaterial::diffuse([0.5; 3]));
    scene.terrain = Some(PtHeightfield {
        width: n,
        depth: n,
        origin: [-half, -half],
        spacing: [spacing, spacing],
        heights,
        material: 0,
    });
    let el = 6f32.to_radians();
    let toward = Vec3::new(el.cos(), el.sin(), 0.0);
    scene.sun = Some(PtSun {
        toward: toward.to_array(),
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    let img = render(&scene, 1, 1, false);
    // The cap above its base (not the flat ground around it, which the cap shadows), where
    // its shading normal faces the sun. The depth AOV is along the view axis.
    let (fwd, _, _) = scene.camera.basis();
    let (mut lit_side, mut dark) = (0, 0);
    for (i, normal) in img.normal.iter().enumerate() {
        let (x, y) = (i as u32 % scene.camera.width, i as u32 / scene.camera.width);
        let (o, d) = pixel_ray(&scene.camera, x, y);
        let p = o + d * (img.depth[i] / d.dot(fwd));
        if img.depth[i] <= 0.0 || p.y < 1.0 || Vec3::from_array(*normal).dot(toward) <= 0.02 {
            continue;
        }
        lit_side += 1;
        if img.shadow[i] < 0.5 {
            dark += 1;
        }
    }
    eprintln!("convex cap: {dark} of {lit_side} sun-facing pixels in shadow");
    assert!(
        lit_side > 300,
        "the camera sees the cap's lit side ({lit_side} px)"
    );
    assert!(
        dark * 200 <= lit_side,
        "{dark} of {lit_side} sun-facing cap pixels in shadow"
    );
}

#[test]
fn instanced_tlas_matches_one_merged_mesh() {
    let cam = PtCamera {
        eye: [0.0, 6.0, -14.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        width: 40,
        height: 30,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let cube = {
        let mut m = PtMesh::default();
        let top = [
            [-0.4, 0.8, -0.4],
            [-0.4, 0.8, 0.4],
            [0.4, 0.8, 0.4],
            [0.4, 0.8, -0.4],
        ];
        scenes::merge(&mut m, &scenes::prism(top, 0.0));
        m
    };
    let mut a = PtScene::new(cam);
    let mat = a.add_material(PtMaterial::diffuse([0.6; 3]));
    let cm = a.add_mesh(cube.clone());
    let mut merged = PtMesh::default();
    for i in 0..12 {
        for j in 0..12 {
            let t = Mat4::from_translation(Vec3::new(i as f32 - 6.0, 0.0, j as f32 - 6.0))
                * Mat4::from_rotation_y((i * 7 + j) as f32 * 0.3)
                * Mat4::from_scale(Vec3::splat(0.5 + ((i + j) % 3) as f32 * 0.2));
            a.add_instance(cm, mat, t);
            let mut moved = cube.clone();
            for p in &mut moved.positions {
                *p = t.transform_point3(Vec3::from_array(*p)).to_array();
            }
            scenes::merge(&mut merged, &moved);
        }
    }
    let mut b = PtScene::new(cam);
    let mb = b.add_material(PtMaterial::diffuse([0.6; 3]));
    let mm = b.add_mesh(merged);
    b.add_instance(mm, mb, Mat4::IDENTITY);
    let ia = render(&a, 1, 1, false);
    let ib = render(&b, 1, 1, false);
    let mut diff = 0;
    for (da, db) in ia.depth.iter().zip(&ib.depth) {
        if (da - db).abs() > 1e-3 * db.abs().max(1.0) {
            diff += 1;
        }
    }
    // Edge pixels may round to different triangles; the interiors must agree.
    assert!(
        diff <= 2,
        "{diff} pixels differ between the TLAS and the merged mesh"
    );
}

#[test]
fn emissive_triangles_light_the_scene_with_mis() {
    // A floor under an emissive quad: NEE + BSDF hits on the emitter must agree with a
    // NEE-only reference (sphere light of equal power is not comparable, so check against the
    // quad's analytic form factor at the floor point straight below its centre instead).
    let mut scene = PtScene::new(PtCamera {
        eye: [0.0, 0.5, 0.0005],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.02,
        width: 4,
        height: 4,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    });
    let floor = scene.add_material(PtMaterial::lambert([1.0; 3]));
    let light = scene.add_material(PtMaterial::emitter([10.0; 3]));
    let fm = scene.add_mesh(scenes::quad([
        [-20.0, 0.0, -20.0],
        [-20.0, 0.0, 20.0],
        [20.0, 0.0, 20.0],
        [20.0, 0.0, -20.0],
    ]));
    let s = 0.5;
    let lm = scene.add_mesh(scenes::quad([
        [-s, 1.0, -s],
        [s, 1.0, -s],
        [s, 1.0, s],
        [-s, 1.0, s],
    ]));
    scene.add_instance(fm, floor, Mat4::IDENTITY);
    scene.add_instance(lm, light, Mat4::IDENTITY);
    let img = render(&scene, 4096, 2, false);
    let poly = [
        regia_core::glam::DVec3::new(-0.5, 1.0, -0.5),
        regia_core::glam::DVec3::new(0.5, 1.0, -0.5),
        regia_core::glam::DVec3::new(0.5, 1.0, 0.5),
        regia_core::glam::DVec3::new(-0.5, 1.0, 0.5),
    ];
    let ff = regia_pt::radiosity::point_to_polygon(
        regia_core::glam::DVec3::ZERO,
        regia_core::glam::DVec3::Y,
        &poly,
    );
    // Irradiance π L F on an ideal white Lambertian: radiance L·F.
    let want = 10.0 * ff;
    let got = img.mean(Aov::Direct)[0];
    let rel = (got / want - 1.0).abs();
    eprintln!("emissive quad: pt {got:.5} reference {want:.5} rel {rel:.4}");
    assert!(rel < 0.005, "emissive quad: {got} vs {want}");
}

/// Thin transmission (`KHR_materials_transmission`): an emitter seen head-on through a clear
/// pane reads its radiance times `1 − F(0)` (the forward's transmitted share; the black sky
/// adds nothing to the reflection), and the IOR sets F(0).
#[test]
fn clear_pane_transmits_one_minus_fresnel() {
    for (ior, f0) in [(1.5f32, 0.04f64), (1.8, (0.8f64 / 2.8).powi(2))] {
        let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 3.0], [0.0; 3], 0.2, 4, 4));
        let quad = |z: f32| scenes::quad([[-2.0, -2.0, z], [2.0, -2.0, z], [2.0, 2.0, z], [-2.0, 2.0, z]]);
        let wall = scene.add_mesh(quad(-1.0));
        let pane = scene.add_mesh(quad(1.0));
        let emitter = scene.add_material(PtMaterial::emitter([100.0; 3]));
        let glass = scene.add_material(PtMaterial {
            base_color: [1.0; 4],
            roughness: 0.05,
            transmission: 1.0,
            ior,
            ..PtMaterial::default()
        });
        scene.add_instance(wall, emitter, Mat4::IDENTITY);
        scene.add_instance(pane, glass, Mat4::IDENTITY);
        let img = render(&scene, 1024, 4, false);
        let mean = img.beauty.iter().map(|c| f64::from(c[1])).sum::<f64>() / img.beauty.len() as f64;
        // Head-on (N·V ≈ 1): F = F(0).
        let want = 100.0 * (1.0 - f0);
        eprintln!("clear pane ior {ior}: {mean:.2} vs {want:.2}");
        assert!((mean / want - 1.0).abs() < 0.03, "ior {ior}: {mean} vs {want}");
    }
}

/// Shadow rays pass thin transmissive surfaces: a glass pane between a floor and the sun casts
/// no shadow (the camera, below the pane, sees only the floor).
#[test]
fn glass_casts_no_shadow() {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 2.0, 4.0], [0.0; 3], 0.3, 4, 4));
    let floor = scene.add_mesh(scenes::quad([
        [-3.0, 0.0, -3.0],
        [-3.0, 0.0, 3.0],
        [3.0, 0.0, 3.0],
        [3.0, 0.0, -3.0],
    ]));
    let grey = scene.add_material(PtMaterial::lambert([0.5; 3]));
    scene.add_instance(floor, grey, Mat4::IDENTITY);
    scene.sun = Some(PtSun {
        toward: [0.0, 1.0, 0.0],
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    let bare = render(&scene, 16, 1, false);
    let pane = scene.add_mesh(scenes::quad([
        [-3.0, 3.0, -3.0],
        [-3.0, 3.0, 3.0],
        [3.0, 3.0, 3.0],
        [3.0, 3.0, -3.0],
    ]));
    let glass = scene.add_material(PtMaterial {
        base_color: [1.0; 4],
        transmission: 1.0,
        ..PtMaterial::default()
    });
    scene.add_instance(pane, glass, Mat4::IDENTITY);
    let glazed = render(&scene, 16, 1, false);
    let lit = |img: &PtImage| img.shadow.iter().map(|&s| f64::from(s)).sum::<f64>() / img.shadow.len() as f64;
    let mean =
        |img: &PtImage| img.beauty.iter().map(|c| f64::from(c[1])).sum::<f64>() / img.beauty.len() as f64;
    assert!(lit(&bare) > 0.99);
    assert!(lit(&glazed) > 0.99, "sun visibility under glass {}", lit(&glazed));
    let ratio = mean(&glazed) / mean(&bare);
    eprintln!("floor under glass: {ratio:.4} of bare");
    assert!((ratio - 1.0).abs() < 1e-3, "{ratio}");
    // An opaque pane does shadow it.
    scene.materials[glass].transmission = 0.0;
    assert!(lit(&render(&scene, 16, 1, false)) < 0.01);
}

/// Watertightness on every driver: camera rays along a quad's shared diagonal (the diagonal
/// pixels of a square view of a centred square) hit it. With fused multiply-adds both
/// triangles used to reject such rays on NVIDIA, leaving a black diagonal.
#[test]
fn rays_along_a_shared_edge_hit_the_surface() {
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 3.0], [0.0; 3], 0.2, 8, 8));
    let quad = scene.add_mesh(scenes::quad([
        [-2.0, -2.0, 0.0],
        [2.0, -2.0, 0.0],
        [2.0, 2.0, 0.0],
        [-2.0, 2.0, 0.0],
    ]));
    let emitter = scene.add_material(PtMaterial::emitter([100.0; 3]));
    scene.add_instance(quad, emitter, Mat4::IDENTITY);
    let img = render(&scene, 1, 1, false);
    let missed: Vec<(u32, u32)> = (0..64u32)
        .filter(|&i| img.at(Aov::Emissive, i % 8, i / 8)[0] < 99.9)
        .map(|i| (i % 8, i / 8))
        .collect();
    assert!(missed.is_empty(), "pixels with no hit: {missed:?}");
}

/// `PtViewAlbedo::all_bounces`: light bounced between the two walls of a valley the camera
/// sees carries the view albedo (0.8) instead of the material's (0.2), so the first hit's
/// one-bounce indirect light grows toward 4× (less where the bounce lands on terrain the
/// camera does not see, which keeps the material); camera-ray albedo is the same both ways.
#[test]
fn view_albedo_reaches_bounce_hits_on_terrain_the_camera_sees() {
    let n = 41u32;
    let cam = PtCamera {
        eye: [0.0, 14.0, -26.0],
        target: [0.0, 3.0, 0.0],
        fov_y: 1.0,
        width: 40,
        height: 30,
        aperture_radius: 0.0,
        focus_distance: 1.0,
    };
    let mut scene = PtScene::new(cam);
    let px = (cam.width * cam.height) as usize;
    let grey = |v: u8, w: u32, h: u32| PtTexture {
        width: w,
        height: h,
        rgba: std::sync::Arc::from([v, v, v, 255].repeat((w * h) as usize)),
        srgb: false,
    };
    // The map-wide texture (the bake's role) is 0.2 under a white base colour, as in pt_plates.
    scene.textures.push(grey(51, 4, 4));
    scene.add_material(PtMaterial {
        albedo: Some(0),
        ..PtMaterial::diffuse([1.0; 3])
    });
    scene.terrain = Some(PtHeightfield {
        width: n,
        depth: n,
        origin: [-20.0, -20.0],
        spacing: [1.0, 1.0],
        heights: (0..n * n).map(|k| 0.8 * ((k % n) as f32 - 20.0).abs()).collect(),
        material: 0,
    });
    scene.sun = Some(PtSun {
        toward: [0.25, 1.0, 0.1],
        color: [1.0; 3],
        illuminance: 1000.0,
        angular_radius: 0.0,
    });
    let aspect = cam.width as f32 / cam.height as f32;
    let view_proj = (regia_core::glam::camera::rh::proj::directx::perspective(cam.fov_y, aspect, 0.1, 500.0)
        * regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::from_array(cam.eye),
            Vec3::from_array(cam.target),
            Vec3::Y,
        ))
    .to_cols_array_2d();
    scene.set_terrain_view_albedo(view_proj, cam.eye, grey(204, cam.width, cam.height), None);
    let indirect = |scene: &PtScene| {
        let img = render(scene, 64, 2, false);
        let (mut sum, mut albedo, mut count) = (0.0f64, 0.0f64, 0);
        for i in 0..px {
            if img.depth[i] > 0.0 {
                sum += lum(img.indirect_diffuse[i]);
                albedo += lum(img.albedo[i]);
                count += 1;
            }
        }
        (sum / f64::from(count.max(1)), albedo / f64::from(count.max(1)))
    };
    let (all, albedo_all) = indirect(&scene);
    if let Some(v) = scene.terrain_view_albedo.as_mut() {
        v.all_bounces = false;
    }
    let (first, albedo_first) = indirect(&scene);
    eprintln!(
        "indirect all bounces {all:.4} camera rays only {first:.4} (x{:.2})",
        all / first
    );
    assert!((albedo_all - albedo_first).abs() < 1e-6 && (albedo_all - 0.8).abs() < 0.01);
    let ratio = all / first;
    assert!(ratio > 2.5 && ratio < 4.05, "bounce ratio {ratio}");
}
