// SPDX-License-Identifier: MPL-2.0
//! Path tracer v2 acceptance (`specs/27` § 1.5, v2 rows): participating media, sky bodies,
//! quad area lights and the thin lens, each against a closed-form reference. Runs on whatever
//! adapter `RenderDevice::headless` picks (lavapipe in CI) at small sizes.

#![allow(clippy::needless_range_loop)]

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Vec3};
use regia_pt::scenes;
use regia_pt::{
    Aov, PathTracer, PtCamera, PtImage, PtLight, PtMaterial, PtMedia, PtMediumLayer, PtPhase, PtScene,
    PtSettings, PtSky, PtSkyBody,
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
        seed: 11,
        rr_start: 3,
        jitter,
        samples_per_submit: 64,
    };
    tracer().render(scene, &settings).expect("render")
}

fn lum(c: [f32; 3]) -> f64 {
    lum64(c.map(f64::from))
}

fn lum64(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Camera ray through the centre of pixel (x, y) (the `generate` kernel's pinhole).
fn pixel_ray(cam: &PtCamera, x: u32, y: u32) -> (Vec3, Vec3) {
    let (fwd, right, up) = cam.basis();
    let th = (0.5 * cam.fov_y).tan();
    let aspect = cam.width as f32 / cam.height as f32;
    let sx = ((x as f32 + 0.5) / cam.width as f32) * 2.0 - 1.0;
    let sy = 1.0 - ((y as f32 + 0.5) / cam.height as f32) * 2.0;
    let d = (fwd + right * (sx * th * aspect) + up * (sy * th)).normalize();
    (Vec3::from_array(cam.eye), d)
}

fn slab(scattering: [f32; 3], absorption: [f32; 3], y0: f32, y1: f32, phase: PtPhase) -> PtMediumLayer {
    PtMediumLayer {
        scattering,
        absorption,
        reference_height: y0,
        falloff: 0.0,
        min_height: y0,
        max_height: y1,
        phase,
        aerial: false,
    }
}

/// Camera at the origin looking up into a uniform sky through absorbing layers: the sky AOV
/// is the sky radiance × the analytic transmittance of each pixel's ray.
#[test]
fn media_transmittance_through_a_slab_is_analytic() {
    let cam = PtCamera::pinhole([0.0, 0.0, 0.0], [0.0, 10.0, 0.01], 0.9, 24, 24);
    let sky = 100.0f32;
    let sigma = [0.2f32, 0.4, 0.8];
    let (y0, y1) = (2.0f32, 5.0f32);
    // A homogeneous slab, and an exponential layer (k = 0.3/m from y = 1 to 4) on top of it.
    let k = 0.3f32;
    let expo = PtMediumLayer {
        scattering: [0.0; 3],
        absorption: [0.3, 0.1, 0.05],
        reference_height: 1.0,
        falloff: k,
        min_height: 1.0,
        max_height: 4.0,
        phase: PtPhase::HenyeyGreenstein([0.0; 3]),
        aerial: false,
    };
    let mut scene = scenes::empty_sky(PtSky::Uniform([sky; 3]), cam);
    scene.media = PtMedia {
        layers: vec![slab([0.0; 3], sigma, y0, y1, PtPhase::Rayleigh), expo],
        escape_distance: 0.0,
    };
    let img = render(&scene, 512, 2, false);
    let mut worst = 0.0f64;
    let mut sums = [0.0f64; 2];
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (_, d) = pixel_ray(&cam, x, y);
            let len_slab = (y1 - y0) / d.y;
            // ∫ e^(−k (y − 1)) over y ∈ [1, 4] along the ray: (1 − e^(−3k)) / (k · d.y).
            let col = (1.0 - (-3.0 * k).exp()) / (k * d.y);
            let i = (y * cam.width + x) as usize;
            for c in 0..3 {
                let expect =
                    f64::from(sky) * f64::from((-(sigma[c] * len_slab) - expo.absorption[c] * col).exp());
                let got = f64::from(img.sky[i][c]);
                worst = worst.max((got / expect - 1.0).abs());
                sums[0] += got;
                sums[1] += expect;
            }
        }
    }
    let mean = sums[0] / sums[1] - 1.0;
    eprintln!("slab transmittance: mean error {mean:.5}, worst pixel {worst:.4}");
    assert!(mean.abs() < 0.01, "mean transmittance error {mean}");
    // Absorption is applied analytically: no per-pixel noise.
    assert!(worst < 0.002, "worst pixel {worst}");
    assert_eq!(img.metadata.rejected_samples, 0);

    // The same slab as the planet's air (aerial): escaping rays already carry it in the sky
    // table, so the sky reads exactly the sky radiance.
    let mut aerial = scene.clone();
    aerial.media.layers = vec![PtMediumLayer {
        aerial: true,
        ..slab([0.0; 3], sigma, y0, y1, PtPhase::Rayleigh)
    }];
    let img = render(&aerial, 4, 2, false);
    assert!(img.sky.iter().all(|c| c.iter().all(|v| (v - sky).abs() < 1e-3)));
}

/// A conservative (albedo 1) chromatic slab in a uniform sky: radiance is the sky's in every
/// direction, from inside the slab, after any number of scattering events.
#[test]
fn a_scattering_slab_in_a_uniform_sky_is_a_furnace() {
    let cam = PtCamera::pinhole([0.0, 2.5, 0.0], [3.0, 3.5, 4.0], 1.4, 24, 16);
    let mut scene = scenes::empty_sky(PtSky::Uniform([1.0; 3]), cam);
    scene.media = PtMedia {
        layers: vec![
            slab(
                [0.15, 0.2, 0.3],
                [0.0; 3],
                1.0,
                4.0,
                PtPhase::HenyeyGreenstein([0.7, 0.5, 0.2]),
            ),
            PtMediumLayer {
                falloff: 0.2,
                reference_height: 1.0,
                ..slab([0.05, 0.1, 0.15], [0.0; 3], 1.0, 4.0, PtPhase::Rayleigh)
            },
        ],
        escape_distance: 0.0,
    };
    let img = render(&scene, 256, 64, true);
    let n = img.beauty.len() as f64;
    let mut mean = [0.0f64; 3];
    for c in &img.beauty {
        for k in 0..3 {
            mean[k] += f64::from(c[k]) / n;
        }
    }
    let vol = img.mean(Aov::Volumetric);
    eprintln!("medium furnace: beauty {mean:?}, volumetric share {vol:?}");
    for k in 0..3 {
        assert!((mean[k] - 1.0).abs() < 0.015, "channel {k}: {}", mean[k]);
    }
    // Most of that light was scattered in the slab (the volumetric AOV), not seen through it.
    assert!(vol[0] > 0.3, "volumetric AOV {vol:?}");
}

/// Lambert plane under a uniform sky body: irradiance `π·L·sin²r·cos θ`, so the plane reads
/// `ρ·L·sin²r·cos θ`; the body's light scale multiplies it.
#[test]
fn a_sky_body_delivers_the_disc_irradiance() {
    let r = 5f32.to_radians();
    let theta = 40f32.to_radians();
    let l = 1000.0f32;
    let rho = 0.5f32;
    let body = PtSkyBody::uniform([theta.sin(), theta.cos(), 0.0], r, [l; 3]);
    let analytic = std::f32::consts::PI * l * r.sin().powi(2);
    let tabulated = body.normal_irradiance();
    assert!(
        (tabulated / analytic - 1.0).abs() < 0.005,
        "table integral {tabulated} vs {analytic}"
    );
    let cam = PtCamera::pinhole([0.0, 3.0, 0.0], [0.0, 0.0, 0.01], 0.6, 16, 16);
    let mut scene = PtScene::new(cam);
    let plane = scene.add_mesh(scenes::quad([
        [-50.0, 0.0, -50.0],
        [50.0, 0.0, -50.0],
        [50.0, 0.0, 50.0],
        [-50.0, 0.0, 50.0],
    ]));
    let mat = scene.add_material(PtMaterial::lambert([rho; 3]));
    scene.add_instance(plane, mat, Mat4::IDENTITY);
    scene.sky_bodies = vec![body.clone()];
    let expect = f64::from(rho * l * r.sin().powi(2) * theta.cos());
    for scale in [1.0f32, 2.0] {
        scene.sky_bodies[0].light_scale = scale;
        let img = render(&scene, 256, 1, false);
        let got = lum64(img.mean(Aov::Beauty));
        eprintln!(
            "sky body ×{scale}: plane {got:.4} vs {:.4}",
            expect * f64::from(scale)
        );
        assert!(
            (got / (expect * f64::from(scale)) - 1.0).abs() < 0.01,
            "plane {got} vs {}",
            expect * f64::from(scale)
        );
    }
    // Seen directly, the disc is the table's radiance (unscaled).
    let mut look = PtScene::new(PtCamera::pinhole(
        [0.0, 0.0, 0.0],
        [theta.sin(), theta.cos(), 0.0],
        0.02,
        4,
        4,
    ));
    look.sky_bodies = vec![body];
    let img = render(&look, 1, 1, false);
    assert!(img.sky.iter().all(|c| (c[1] - l).abs() < 1.0), "{:?}", img.sky[0]);
}

/// Irradiance at `p` (normal `n`) from a Lambertian polygon of radiance 1 (Lambert's formula:
/// `½ Σ Θ_i n·Γ̂_i` over the edges).
fn polygon_irradiance(p: Vec3, n: Vec3, poly: &[Vec3]) -> f64 {
    let mut sum = 0.0f64;
    for i in 0..poly.len() {
        let a = (poly[i] - p).normalize();
        let b = (poly[(i + 1) % poly.len()] - p).normalize();
        let gamma = a.cross(b);
        let len = gamma.length();
        if len < 1e-9 {
            continue;
        }
        let angle = f64::from(a.dot(b).clamp(-1.0, 1.0)).acos();
        sum += angle * f64::from(n.dot(gamma / len));
    }
    (0.5 * sum).abs()
}

/// A one-sided quad light over a Lambert plane: each pixel reads `ρ/π · E` from Lambert's
/// polygon formula; flipped to face away, it lights nothing.
#[test]
fn quad_light_matches_lamberts_polygon_irradiance() {
    let cam = PtCamera::pinhole([0.0, 1.5, 3.0], [0.0, 0.0, 0.0], 0.9, 24, 16);
    let rho = 0.6f32;
    let radiance = 50.0f32;
    let corner = [-0.5f32, 1.0, -0.4];
    let (u, v) = ([1.0f32, 0.0, 0.0], [0.0f32, 0.0, 0.8]);
    let build = |facing_down: bool| {
        let mut scene = PtScene::new(cam);
        let plane = scene.add_mesh(scenes::quad([
            [-20.0, 0.0, -20.0],
            [20.0, 0.0, -20.0],
            [20.0, 0.0, 20.0],
            [-20.0, 0.0, 20.0],
        ]));
        let mat = scene.add_material(PtMaterial::lambert([rho; 3]));
        scene.add_instance(plane, mat, Mat4::IDENTITY);
        // u × v = (0, −0.8, 0): facing down; swapped, facing up.
        let (eu, ev) = if facing_down { (u, v) } else { (v, u) };
        scene.lights.push(PtLight::Quad {
            corner,
            edge_u: eu,
            edge_v: ev,
            radiance: [radiance; 3],
            two_sided: false,
        });
        scene
    };
    let img = render(&build(true), 1024, 1, false);
    let poly = [
        Vec3::from_array(corner),
        Vec3::from_array(corner) + Vec3::from_array(u),
        Vec3::from_array(corner) + Vec3::from_array(u) + Vec3::from_array(v),
        Vec3::from_array(corner) + Vec3::from_array(v),
    ];
    let (mut err, mut bias, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (o, d) = pixel_ray(&cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let p = o + d * (-o.y / d.y);
            let i = (y * cam.width + x) as usize;
            // Only pixels that see the plane (not the light's black back face).
            let (fwd, _, _) = cam.basis();
            let plane_depth = (p - o).dot(fwd);
            if (img.depth[i] - plane_depth).abs() > 1e-3 * plane_depth {
                continue;
            }
            let e = polygon_irradiance(p, Vec3::Y, &poly) * f64::from(radiance);
            let expect = f64::from(rho) / std::f64::consts::PI * e;
            if expect < 0.05 {
                continue;
            }
            let rel = f64::from(img.direct[i][1]) / expect - 1.0;
            err += rel.abs();
            bias += rel;
            n += 1;
        }
    }
    let mre = err / n.max(1) as f64;
    let bias = bias / n.max(1) as f64;
    eprintln!("quad light: {n} px, mean relative error {mre:.5} (noise), bias {bias:.5}");
    assert!(n > 100, "{n} pixels");
    assert!(bias.abs() < 0.004, "bias {bias}");
    assert!(mre < 0.02, "mean relative error {mre}");
    let dark = render(&build(false), 16, 1, false);
    assert!(
        lum64(dark.mean(Aov::Direct)) < 1e-6,
        "a quad facing away lights the plane"
    );
}

/// Second moment of the image's luminance about its centroid → radius of a uniform disc.
fn blur_radius(img: &PtImage) -> f64 {
    let (mut w, mut cx, mut cy) = (0.0f64, 0.0f64, 0.0f64);
    for y in 0..img.height {
        for x in 0..img.width {
            let v = lum(img.beauty[(y * img.width + x) as usize]);
            w += v;
            cx += v * (f64::from(x) + 0.5);
            cy += v * (f64::from(y) + 0.5);
        }
    }
    let (cx, cy) = (cx / w, cy / w);
    let mut m2 = 0.0f64;
    for y in 0..img.height {
        for x in 0..img.width {
            let v = lum(img.beauty[(y * img.width + x) as usize]);
            let (dx, dy) = (f64::from(x) + 0.5 - cx, f64::from(y) + 0.5 - cy);
            m2 += v * (dx * dx + dy * dy);
        }
    }
    (2.0 * m2 / w).sqrt()
}

/// A point emitter off the focus plane images as a disc of radius `A·|d − d_f|/d` on the
/// focus plane (thin lens); in focus, and with a pinhole, it stays a point.
#[test]
fn thin_lens_blur_matches_the_circle_of_confusion() {
    let (w, h, fov) = (64u32, 64u32, 0.5f32);
    let d_obj = 4.0f32;
    let shot = |aperture: f32, focus: f32| {
        let cam = PtCamera {
            aperture_radius: aperture,
            focus_distance: focus,
            ..PtCamera::pinhole([0.0, 0.0, 0.0], [0.0, 0.0, -1.0], fov, w, h)
        };
        let mut scene = PtScene::new(cam);
        let ball = scene.add_mesh(scenes::uv_sphere(0.01, 16, 8, false));
        let mat = scene.add_material(PtMaterial::emitter([1000.0; 3]));
        scene.add_instance(ball, mat, Mat4::from_translation(Vec3::new(0.0, 0.0, -d_obj)));
        render(&scene, 1024, 1, true)
    };
    let px_per_m_at = |dist: f32| f64::from(h) * 0.5 / (f64::from(dist) * f64::from((0.5 * fov).tan()));
    let (aperture, focus) = (0.15f32, 2.0f32);
    let expect = f64::from(aperture * (d_obj - focus).abs() / d_obj) * px_per_m_at(focus);
    let blurred = blur_radius(&shot(aperture, focus));
    let sharp = blur_radius(&shot(aperture, d_obj));
    let pinhole = blur_radius(&shot(0.0, focus));
    eprintln!(
        "thin lens: blur {blurred:.2} px (expect {expect:.2}), in focus {sharp:.2}, pinhole {pinhole:.2}"
    );
    assert!(
        (blurred / expect - 1.0).abs() < 0.08,
        "blur {blurred} vs {expect}"
    );
    assert!(
        sharp < 1.5 && pinhole < 1.5,
        "in focus {sharp}, pinhole {pinhole}"
    );
}

/// `∫ f cos θ_l dω` of the forward pass's anisotropic metal specular (`d_ggx_anisotropic`,
/// `v_smith_ggx_anisotropic`, Schlick with F0 = base) for the view `v` in the frame (t, b, n),
/// by a fine midpoint rule in (cos θ, φ).
fn aniso_albedo(v: Vec3, f0: f32, perceptual: f32, aniso: f32, rot: f32) -> f64 {
    let a = (perceptual * perceptual).max(0.002_025);
    let at = (a * (1.0 + aniso)).max(1e-4);
    let ab = (a * (1.0 - aniso)).max(1e-4);
    let (t0, b0) = (Vec3::X, Vec3::Z * -1.0);
    // The forward frame: b = n × t (n = +Y, t = +X gives −Z), then rotated by `rot`.
    let n = Vec3::Y;
    let t = t0 * rot.cos() + b0 * rot.sin();
    let b = b0 * rot.cos() - t0 * rot.sin();
    let (nu, np) = (1200usize, 2400usize);
    let mut sum = 0.0f64;
    for i in 0..nu {
        let ct = (i as f32 + 0.5) / nu as f32;
        let st = (1.0 - ct * ct).max(0.0).sqrt();
        for j in 0..np {
            let phi = (j as f32 + 0.5) / np as f32 * std::f32::consts::TAU;
            let l = Vec3::new(st * phi.cos(), ct, st * phi.sin());
            let h = (l + v).normalize();
            let (nl, nv, nh, lh) = (n.dot(l), n.dot(v), n.dot(h), l.dot(h));
            let a2 = at * ab;
            let fv = Vec3::new(ab * t.dot(h), at * b.dot(h), a2 * nh);
            let w2 = a2 / fv.dot(fv).max(1e-8);
            let d = a2 * w2 * w2 / std::f32::consts::PI;
            let gv = nl * Vec3::new(at * t.dot(v), ab * b.dot(v), nv).length();
            let gl = nv * Vec3::new(at * t.dot(l), ab * b.dot(l), nl).length();
            let vis = 0.5 / (gv + gl).max(1e-7);
            let f = f0 + (1.0 - f0) * (1.0 - lh.max(0.0)).powi(5);
            sum += f64::from(d * vis * f * nl);
        }
    }
    sum * std::f64::consts::TAU / (nu * np) as f64
}

/// An anisotropic metal plane under a uniform white sky (NEE + BSDF sampling with MIS, two
/// path vertices) reads the BRDF's directional albedo: the anisotropic VNDF sampling and its
/// pdf agree.
#[test]
fn anisotropic_sampling_matches_the_brdf_albedo() {
    let v = Vec3::new(0.0, 0.6, 0.8);
    let cam = PtCamera::pinhole((v * 20.0).to_array(), [0.0; 3], 0.01, 8, 8);
    let base = [1.0f32, 0.8, 0.5];
    let (rough, aniso, rot) = (0.45f32, 0.7f32, 0.3f32);
    let mut scene = scenes::empty_sky(PtSky::Uniform([1.0; 3]), cam);
    let mut plane = scenes::quad([
        [-5.0, 0.0, -5.0],
        [5.0, 0.0, -5.0],
        [5.0, 0.0, 5.0],
        [-5.0, 0.0, 5.0],
    ]);
    plane.tangents = vec![[1.0, 0.0, 0.0, 1.0]; plane.positions.len()];
    let mesh = scene.add_mesh(plane);
    let mat = scene.add_material(PtMaterial {
        base_color: [base[0], base[1], base[2], 1.0],
        metallic: 1.0,
        roughness: rough,
        anisotropy: aniso,
        anisotropy_rotation: rot,
        ..PtMaterial::default()
    });
    scene.add_instance(mesh, mat, Mat4::IDENTITY);
    let img = render(&scene, 4096, 2, true);
    let got = img.mean(Aov::Beauty);
    for c in 0..3 {
        let want = aniso_albedo(v, base[c], rough, aniso, rot);
        eprintln!("anisotropic albedo channel {c}: PT {:.5} vs {want:.5}", got[c]);
        assert!(
            (got[c] / want - 1.0).abs() < 0.01,
            "channel {c}: {} vs {want}",
            got[c]
        );
    }
}
