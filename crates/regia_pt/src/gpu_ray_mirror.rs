// SPDX-License-Identifier: MPL-2.0
//! A CPU mirror of `pt.wgsl`'s ray/triangle arithmetic (`wt_ray`, `ray_tri`, `offset_ray`), op
//! for op in `f32`, so its precision is tested without a GPU. Keep it in step with the shader.
//!
//! What it guards (ADR-1411): a ray leaving a point on a very large triangle (the 2 km ground of
//! the `bake_light_inputs` emitter test) at a shallow angle. A BVH leaf's triangles are tested
//! with no box test (the two-triangle ground is one leaf), so nothing culls them. The sheared
//! depth `t` is a weighted sum of the vertices' depths, which sit a kilometre away on both sides
//! of the ray origin, so its rounding error (about 1e-4 m) is larger than the 1/65536 m
//! `offset_ray` lift of a point near a coordinate plane. Without a lower bound on `t` the ray
//! "hits" the surface it left, just behind its origin: bounce rays and shadow rays from the
//! ground never reached the emitter, and the bake's ground-bounced light was about 14 % low.
//! `ray_tri` now rejects a hit closer than the computation's own error bound (Pharr, Jakob and
//! Humphreys, Physically Based Rendering, 3rd ed., § 3.9.6, "Conservative ray-triangle t").

use regia_core::glam::{DVec3, Vec3};

const INF: f32 = 1e30;

/// Unit roundoff of `f32` (half the machine epsilon).
const U: f32 = f32::EPSILON * 0.5;

/// `γ(n) = n·u / (1 − n·u)`, the relative error bound of `n` rounded operations (PBRT § 3.9.1).
const fn gamma(n: f32) -> f32 {
    n * U / (1.0 - n * U)
}

/// `pt.wgsl` `WtRay`: the ray origin, the axis permutation and the shear.
struct WtRay {
    o: Vec3,
    k: [usize; 3],
    s: [f32; 3],
}

/// `pt.wgsl` `wt_ray`.
fn wt_ray(o: Vec3, d: Vec3) -> WtRay {
    let a = d.abs();
    let mut kz = 2;
    if a.x > a.y && a.x > a.z {
        kz = 0;
    } else if a.y > a.z {
        kz = 1;
    }
    let mut kx = (kz + 1) % 3;
    let mut ky = (kx + 1) % 3;
    if d[kz] < 0.0 {
        std::mem::swap(&mut kx, &mut ky);
    }
    let sz = 1.0 / d[kz];
    WtRay {
        o,
        k: [kx, ky, kz],
        s: [d[kx] * sz, d[ky] * sz, sz],
    }
}

/// `pt.wgsl` `ray_tri`: `(t, u, v)` or `None` on a miss. `bounded: false` is the shader before
/// ADR-1411 (no lower bound on `t`), kept to measure what the bound removes.
fn ray_tri(r: &WtRay, p0: Vec3, p1: Vec3, p2: Vec3, tmax: f32, bounded: bool) -> Option<[f32; 3]> {
    let [kx, ky, kz] = r.k;
    let a = p0 - r.o;
    let b = p1 - r.o;
    let c = p2 - r.o;
    let ax = a[kx] - r.s[0] * a[kz];
    let ay = a[ky] - r.s[1] * a[kz];
    let bx = b[kx] - r.s[0] * b[kz];
    let by = b[ky] - r.s[1] * b[kz];
    let cx = c[kx] - r.s[0] * c[kz];
    let cy = c[ky] - r.s[1] * c[kz];
    let eu = cx * by - cy * bx;
    let ev = ax * cy - ay * cx;
    let ew = bx * ay - by * ax;
    let tu = 4e-7 * ((cx * by).abs() + (cy * bx).abs());
    let tv = 4e-7 * ((ax * cy).abs() + (ay * cx).abs());
    let tw = 4e-7 * ((bx * ay).abs() + (by * ax).abs());
    if (eu < -tu || ev < -tv || ew < -tw) && (eu > tu || ev > tv || ew > tw) {
        return None;
    }
    let det = eu + ev + ew;
    if det == 0.0 {
        return None;
    }
    let az = r.s[2] * a[kz];
    let bz = r.s[2] * b[kz];
    let cz = r.s[2] * c[kz];
    let t = (eu * az + ev * bz + ew * cz) / det;
    if t.is_nan() || t <= 0.0 || t >= tmax {
        return None;
    }
    if bounded {
        let max_zt = az.abs().max(bz.abs()).max(cz.abs());
        let max_xt = ax.abs().max(bx.abs()).max(cx.abs());
        let max_yt = ay.abs().max(by.abs()).max(cy.abs());
        let delta_z = gamma(3.0) * max_zt;
        let delta_x = gamma(5.0) * (max_xt + max_zt);
        let delta_y = gamma(5.0) * (max_yt + max_zt);
        let delta_e = 2.0 * (gamma(2.0) * max_xt * max_yt + delta_y * max_xt + delta_x * max_yt);
        let max_e = eu.abs().max(ev.abs()).max(ew.abs());
        let delta_t = 3.0 * (gamma(3.0) * max_e * max_zt + delta_e * max_zt + delta_z * max_e) / det.abs();
        if t <= delta_t {
            return None;
        }
    }
    let u = (ev / det).clamp(0.0, 1.0);
    let v = (ew / det).clamp(0.0, 1.0 - u);
    Some([t, u, v])
}

/// `pt.wgsl` `offset_ray` (Wächter and Binder 2019).
fn offset_ray(p: Vec3, n: Vec3) -> Vec3 {
    const ORIGIN: f32 = 1.0 / 32.0;
    const FLOAT_SCALE: f32 = 1.0 / 65536.0;
    const INT_SCALE: f32 = 256.0;
    let of_i = (n * INT_SCALE).as_ivec3();
    let nudge = |c: f32, o: i32, nc: f32| {
        if c.abs() < ORIGIN {
            c + FLOAT_SCALE * nc
        } else {
            let o = if c < 0.0 { -o } else { o };
            f32::from_bits((c.to_bits() as i32).wrapping_add(o) as u32)
        }
    };
    Vec3::new(
        nudge(p.x, of_i.x, n.x),
        nudge(p.y, of_i.y, n.y),
        nudge(p.z, of_i.z, n.z),
    )
}

/// The two triangles of an axis-aligned quad, wound like `scenes::quad`.
fn quad_tris(p: [Vec3; 4]) -> [[Vec3; 3]; 2] {
    [[p[0], p[1], p[2]], [p[0], p[2], p[3]]]
}

/// The emitter test's 2 km ground at y = 0.
fn ground() -> [[Vec3; 3]; 2] {
    quad_tris([
        Vec3::new(-1000.0, 0.0, -1000.0),
        Vec3::new(-1000.0, 0.0, 1000.0),
        Vec3::new(1000.0, 0.0, 1000.0),
        Vec3::new(1000.0, 0.0, -1000.0),
    ])
}

/// The emitter test's 400 m panel 20 m up.
fn panel() -> [[Vec3; 3]; 2] {
    quad_tris([
        Vec3::new(-200.0, 20.0, -200.0),
        Vec3::new(-200.0, 20.0, 200.0),
        Vec3::new(200.0, 20.0, 200.0),
        Vec3::new(200.0, 20.0, -200.0),
    ])
}

fn closest(tris: &[[Vec3; 3]], o: Vec3, d: Vec3, bounded: bool) -> Option<f32> {
    let r = wt_ray(o, d);
    tris.iter()
        .filter_map(|t| ray_tri(&r, t[0], t[1], t[2], INF, bounded))
        .map(|h| h[0])
        .reduce(f32::min)
}

/// Ground points around the origin (where the emitter test's probe sees the ground), as
/// `surface_at` interpolates them (y exactly 0), lifted by `offset_ray`.
fn ground_origins() -> impl Iterator<Item = Vec3> {
    (0..48).flat_map(|i| {
        (0..48).map(move |j| {
            let p = Vec3::new(
                -60.0 + 120.0 * (i as f32 + 0.37) / 48.0,
                0.0,
                -60.0 + 120.0 * (j as f32 + 0.61) / 48.0,
            );
            offset_ray(p, Vec3::Y)
        })
    })
}

/// Upward directions from 2° to 88° above the horizon, all around.
fn upward_dirs() -> impl Iterator<Item = Vec3> {
    (0..44).flat_map(|e| {
        let elevation = (2.0 + 2.0 * e as f32).to_radians();
        (0..24).map(move |a| {
            let azimuth = (a as f32 + 0.5) * std::f32::consts::TAU / 24.0;
            Vec3::new(
                elevation.cos() * azimuth.cos(),
                elevation.sin(),
                elevation.cos() * azimuth.sin(),
            )
        })
    })
}

#[test]
fn a_ray_leaving_the_2_km_ground_never_hits_the_ground_again() {
    let ground = ground();
    let mut rays = 0u32;
    let mut unbounded = 0u32;
    let mut bounded = 0u32;
    for o in ground_origins() {
        for d in upward_dirs() {
            rays += 1;
            unbounded += u32::from(closest(&ground, o, d, false).is_some());
            bounded += u32::from(closest(&ground, o, d, true).is_some());
        }
    }
    eprintln!("ground self-hits: {unbounded} of {rays} rays without the t bound, {bounded} with it");
    assert_eq!(bounded, 0, "a ray leaving the ground hit it again");
}

#[test]
fn the_t_bound_keeps_every_hit_on_the_panel_and_the_ground_from_the_probe() {
    // Rays from the ground up to the 400 m panel: each one the exact (f64) plane test puts
    // inside the square, away from its border, must hit at the f64 distance.
    let panel = panel();
    let (mut checked, mut missed) = (0u32, 0u32);
    for o in ground_origins() {
        for d in upward_dirs() {
            let (od, dd) = (o.as_dvec3(), d.as_dvec3());
            let t = (20.0 - od.y) / dd.y;
            let at: DVec3 = od + dd * t;
            if at.x.abs() > 199.0 || at.z.abs() > 199.0 {
                continue;
            }
            checked += 1;
            match closest(&panel, o, d, true) {
                Some(got) if (f64::from(got) - t).abs() <= 1e-4 * t => {}
                _ => missed += 1,
            }
        }
    }
    assert!(checked > 10_000, "{checked}");
    assert_eq!(missed, 0, "{missed} of {checked} panel hits lost");

    // The probe 1 m above the ground, looking down at every angle steeper than 2 degrees.
    let ground = ground();
    let probe = Vec3::new(0.0, 1.0, 0.0);
    for d in upward_dirs() {
        let down = Vec3::new(d.x, -d.y, d.z);
        let want = 1.0 / f64::from(d.y);
        let got = closest(&ground, probe, down, true).map(f64::from);
        assert!(
            got.is_some_and(|t| (t - want).abs() <= 1e-4 * want),
            "probe ray {down:?}: {got:?}, want {want}"
        );
    }
}

#[test]
fn a_ray_along_the_shared_diagonal_still_hits_one_of_the_two_triangles() {
    // The bound only drops hits closer than their own rounding error; the watertight inside
    // test is unchanged, so rays aimed exactly at the ground quad's diagonal still hit.
    let ground = ground();
    for k in 0..200 {
        let s = -900.0 + 9.0 * k as f32;
        let target = Vec3::new(s, 0.0, s);
        let o = Vec3::new(s * 0.25 + 3.0, 7.5, s * 0.25 - 11.0);
        let d = (target - o).normalize();
        assert!(
            closest(&ground, o, d, true).is_some(),
            "diagonal ray to {target:?}"
        );
    }
}
