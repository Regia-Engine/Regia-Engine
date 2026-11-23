// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::baked_gi::{ProbeAoSplit, probe_ao_keep};

const RADIUS: f32 = crate::ssao::SSAO_RADIUS_M;
const BIAS: f32 = crate::ssao::SSAO_BIAS;
const INTENSITY: f32 = crate::ssao::SSAO_INTENSITY;

/// The half-resolution AO target of a 2560×1440 frame, 60° vertical field of view.
const W: i32 = 1280;
const H: i32 = 720;
/// Desktop High's GTAO row.
const SAMPLES: usize = 12;

fn proj_y() -> f32 {
    1.0 / (std::f32::consts::FRAC_PI_6).tan()
}

fn proj_x() -> f32 {
    proj_y() * H as f32 / W as f32
}

/// Analytic scenes on a floor at y = 0, in world space.
#[derive(Clone, Copy)]
enum Scene {
    /// The floor alone.
    Floor,
    /// The floor in front of a wall at z = −5 facing +z.
    Wall,
}

impl Scene {
    /// Distance along `d` (world) from `o` to the first surface, or `None` (sky).
    fn hit(self, o: Vec3, d: Vec3) -> Option<f32> {
        let mut best: Option<f32> = None;
        let mut take = |t: f32| {
            if t > 0.0 && best.is_none_or(|b| t < b) {
                best = Some(t);
            }
        };
        if d.y < 0.0 {
            let t = -o.y / d.y;
            let p = o + d * t;
            if matches!(self, Scene::Floor) || p.z >= -5.0 {
                take(t);
            }
        }
        if matches!(self, Scene::Wall) && d.z < 0.0 {
            let t = (-5.0 - o.z) / d.z;
            if (o + d * t).y >= 0.0 {
                take(t);
            }
        }
        best
    }
}

/// A camera at (0, 1.6, 0) looking at `target`: world-to-view rotation rows and the eye.
struct Camera {
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    back: Vec3,
}

impl Camera {
    fn new(target: Vec3) -> Self {
        let eye = Vec3::new(0.0, 1.6, 0.0);
        let f = (target - eye).normalize();
        let right = f.cross(Vec3::Y).normalize();
        let up = right.cross(f);
        Self {
            eye,
            right,
            up,
            back: -f,
        }
    }

    fn to_view(&self, w: Vec3) -> Vec3 {
        Vec3::new(w.dot(self.right), w.dot(self.up), w.dot(self.back))
    }

    fn to_world(&self, v: Vec3) -> Vec3 {
        self.right * v.x + self.up * v.y + self.back * v.z
    }

    /// `ssao.wgsl` `view_position` of AO texel `c` (the depth buffer at the AO target's size),
    /// against the analytic scene; the sky reads as far away.
    fn view_position(&self, scene: Scene, c: (i32, i32)) -> Vec3 {
        let (x, y) = (c.0.clamp(0, W - 1), c.1.clamp(0, H - 1));
        let uv = ((x as f32 + 0.5) / W as f32, (y as f32 + 0.5) / H as f32);
        let ndc = (uv.0 * 2.0 - 1.0, 1.0 - uv.1 * 2.0);
        let dir_view = Vec3::new(ndc.0 / proj_x(), ndc.1 / proj_y(), -1.0);
        // Depth along −z is the ray parameter for a view direction with z = −1.
        let depth = scene.hit(self.eye, self.to_world(dir_view)).unwrap_or(1.0e4);
        dir_view * depth
    }

    /// AO texel of a world point.
    fn texel(&self, p: Vec3) -> (i32, i32) {
        let v = self.to_view(p - self.eye);
        let z = -v.z;
        let ndc = (v.x * proj_x() / z, v.y * proj_y() / z);
        (
            ((ndc.0 * 0.5 + 0.5) * W as f32) as i32,
            ((0.5 - ndc.1 * 0.5) * H as f32) as i32,
        )
    }
}

/// The kernel's samples at the AO texel of world point `receiver` (rotation 0), as `cs_main`
/// takes them, with `keep(height)` the share no probe holds; and the receiver's view normal for
/// world normal `normal`.
fn samples(
    scene: Scene,
    camera: &Camera,
    receiver: Vec3,
    normal: Vec3,
    keep: impl Fn(f32) -> f32,
) -> (Vec3, Vec<KernelSample>) {
    let g = camera.texel(receiver);
    let p = camera.view_position(scene, g);
    let n = camera.to_view(normal);
    let max_px = (H as f32 * 0.1).max(4.0);
    let radius_px = (RADIUS * proj_y() * 0.5 * H as f32 / (-p.z).max(1e-3)).clamp(1.5, max_px);
    let taken = (0..SAMPLES)
        .map(|i| {
            let t = (i as f32 + 0.5) / SAMPLES as f32;
            let angle = i as f32 * 2.399_963;
            let off = (angle.cos() * radius_px * t, angle.sin() * radius_px * t);
            let s = (
                (g.0 as f32 + 0.5 + off.0).floor() as i32,
                (g.1 as f32 + 0.5 + off.1).floor() as i32,
            );
            let v = camera.view_position(scene, s) - p;
            KernelSample {
                v,
                kept: keep(v.dot(n).max(0.0)),
            }
        })
        .collect();
    (n, taken)
}

fn degrees_between(a: Vec3, b: Vec3) -> f32 {
    a.normalize()
        .dot(b.normalize())
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

/// AO and the world-space lean at a world receiver (normal `normal`) in `scene`.
fn measure(scene: Scene, target: Vec3, receiver: Vec3, normal: Vec3) -> (f32, Vec3) {
    let camera = Camera::new(target);
    let (n, taken) = samples(scene, &camera, receiver, normal, |_| 1.0);
    let (ao, lean) = kernel_ao_and_lean(n, &taken, RADIUS, BIAS, INTENSITY);
    (ao, camera.to_world(lean))
}

#[test]
fn an_open_floor_keeps_its_normal_and_full_ao() {
    let (ao, lean) = measure(
        Scene::Floor,
        Vec3::new(0.0, 0.0, -5.0),
        Vec3::new(0.0, 0.0, -2.0),
        Vec3::Y,
    );
    assert_eq!(ao, 1.0);
    assert_eq!(lean, Vec3::ZERO);
}

#[test]
fn a_floor_beside_a_wall_leans_away_from_it() {
    let (ao, lean) = measure(
        Scene::Wall,
        Vec3::new(0.0, 0.0, -5.0),
        Vec3::new(0.0, 0.0, -4.9),
        Vec3::Y,
    );
    let angle = degrees_between(Vec3::Y + lean, Vec3::Y);
    eprintln!("floor 0.1 m from the wall: ao {ao:.3}, lean {lean:?}, {angle:.1}°");
    assert!(ao < 0.8, "the wall must occlude the floor at its base: ao {ao}");
    // Away from the wall (+z), and mostly so: the spiral's asymmetry leaves a small sideways part.
    assert!(lean.z > 0.15, "{lean:?}");
    assert!(
        lean.x.abs() < 0.5 * lean.z && lean.y.abs() < 0.5 * lean.z,
        "{lean:?}"
    );
    assert!((8.0..=40.0).contains(&angle), "{angle}°");
    // Further out the wall takes less of the hemisphere: a smaller lean.
    let (far_ao, far) = measure(
        Scene::Wall,
        Vec3::new(0.0, 0.0, -5.0),
        Vec3::new(0.0, 0.0, -4.6),
        Vec3::Y,
    );
    assert!(far_ao > ao && far.z < lean.z && far.z > 0.0, "{far_ao} {far:?}");
}

#[test]
fn a_wall_above_the_floor_leans_up() {
    let (ao, lean) = measure(
        Scene::Wall,
        Vec3::new(0.0, 0.5, -5.0),
        Vec3::new(0.0, 0.3, -5.0),
        Vec3::Z,
    );
    eprintln!("wall 0.3 m above the floor: ao {ao:.3}, lean {lean:?}");
    assert!(ao < 1.0);
    assert!(
        lean.y > 0.03 && lean.x.abs() < lean.y && lean.z.abs() < lean.y,
        "{lean:?}"
    );
}

#[test]
fn probes_that_hold_the_wall_do_not_lean_the_normal() {
    let camera = Camera::new(Vec3::new(0.0, 0.0, -5.0));
    let receiver = Vec3::new(0.0, 0.0, -4.9);
    let (n, whole) = samples(Scene::Wall, &camera, receiver, Vec3::Y, |_| 1.0);
    let (ao, lean) = kernel_ao_and_lean(n, &whole, RADIUS, BIAS, INTENSITY);
    // A brick covering the receiver whose probes resolve everything past 0.1 m: the wall above
    // that is in their light already.
    let split = ProbeAoSplit {
        brick_share: 1.0,
        terrain_share: 0.0,
        brick_rho: 0.1,
        terrain_rho: 0.0,
    };
    let keep = |h: f32| split.sky_share() + split.brick_share * probe_ao_keep(h, split.brick_rho);
    let (_, held) = samples(Scene::Wall, &camera, receiver, Vec3::Y, keep);
    let (held_ao, held_lean) = kernel_ao_and_lean(n, &held, RADIUS, BIAS, INTENSITY);
    eprintln!(
        "lean {:.3} -> {:.3} with the probes holding the wall",
        lean.length(),
        held_lean.length()
    );
    // The mirror's AO is the whole kernel's either way (the split AO is `ssao_visibility`'s).
    assert_eq!(ao, held_ao);
    assert!(
        held_lean.length() < 0.5 * lean.length(),
        "{held_lean:?} vs {lean:?}"
    );
    // A probe that holds everything leans nothing.
    let (_, none) = samples(Scene::Wall, &camera, receiver, Vec3::Y, |_| 0.0);
    assert_eq!(
        kernel_ao_and_lean(n, &none, RADIUS, BIAS, INTENSITY).1,
        Vec3::ZERO
    );
}

/// The open directions' mean over the cosine-weighted hemisphere of `n` = +y with every direction
/// toward +x blocked, by integration over a Fibonacci sphere: `(blocked vector, blocked share)`.
fn half_space_blocked() -> (Vec3, f32) {
    let count = 100_000;
    let d_omega = 4.0 * std::f32::consts::PI / count as f32;
    let (mut blocked, mut share) = (Vec3::ZERO, 0.0f32);
    for i in 0..count {
        let y = 1.0 - (i as f32 + 0.5) / count as f32 * 2.0;
        let r = (1.0 - y * y).max(0.0).sqrt();
        let phi = i as f32 * 2.399_963;
        let d = Vec3::new(r * phi.cos(), y, r * phi.sin());
        if d.y > 0.0 && d.x > 0.0 {
            let w = d.y * d_omega / std::f32::consts::PI;
            blocked += d * w;
            share += w;
        }
    }
    (blocked, share)
}

#[test]
fn the_tilt_turns_a_blocked_half_space_into_its_open_mean_direction() {
    let (blocked, share) = half_space_blocked();
    assert!((share - 0.5).abs() < 1e-3, "{share}");
    let lean = bent_normal_lean(Vec3::Y, blocked, share);
    let angle = degrees_between(Vec3::Y + lean, Vec3::Y);
    // atan(2/π): the open quarter-sphere's cosine-weighted mean direction.
    let exact = (2.0 / std::f32::consts::PI).atan().to_degrees();
    assert!((angle - exact).abs() < 0.5, "{angle}° vs {exact}°");
    assert!(lean.x < 0.0 && lean.z.abs() < 1e-3, "{lean:?}");
    // The lean keeps the bent normal unit length.
    assert!(((Vec3::Y + lean).length() - 1.0).abs() < 1e-5);
}

#[test]
fn the_lean_is_capped_at_sixty_degrees_and_a_clamped_ao_scales_back() {
    let n = Vec3::Y;
    let toward = Vec3::new(1.0, 0.2, 0.0).normalize();
    for share in [1.0, 2.0, 5.0] {
        let lean = bent_normal_lean(n, toward * share, share);
        let angle = degrees_between(n + lean, n);
        assert!(angle <= 60.0 + 1e-3, "{angle}°");
        assert!(lean.abs().max_element() <= 1.0 + 1e-6, "{lean:?}");
        // Past a fully blocked receiver the lean does not grow.
        let one = bent_normal_lean(n, toward, 1.0);
        assert!((lean - one).length() < 1e-5, "{lean:?} vs {one:?}");
    }
    // Straight above the receiver: the lean stays zero (nothing to lean toward).
    assert_eq!(bent_normal_lean(n, n * 0.3, 0.3), Vec3::ZERO);
}

#[test]
fn the_encoding_round_trips_and_zero_is_exact() {
    assert_eq!(encode_component(0.0), 128);
    assert_eq!(encode_component(-1.0), 1);
    assert_eq!(encode_component(1.0), 255);
    assert_eq!(encode_component(7.0), 255);
    // A driver that rounds the conversion down writes 127 for 0: still an exact zero.
    for code in [127, 128, 129] {
        assert_eq!(decode_component(code), 0.0);
    }
    assert_eq!(decode_component(1), -1.0);
    assert_eq!(decode_component(255), 1.0);
    for i in -100..=100 {
        let x = i as f32 / 100.0;
        let back = decode_component(encode_component(x));
        let err = (back - x).abs();
        assert!(err <= 1.5 / 127.0 + 1e-6, "{x} -> {back}");
    }
}

#[test]
fn the_bent_shading_normal_keeps_the_normal_maps_detail() {
    let mapped = Vec3::new(0.3, 0.9, 0.1).normalize();
    assert!((bent_shading_normal(mapped, Vec3::ZERO) - mapped).length() < 1e-6);
    let lean = Vec3::new(0.0, -0.13, 0.5);
    let bent = bent_shading_normal(mapped, lean);
    assert!((bent.length() - 1.0).abs() < 1e-5);
    assert!(bent.z > mapped.z);
    // A lean that would take it under the surface keeps the normal.
    assert_eq!(bent_shading_normal(mapped, -mapped * 1.5), mapped);
}

#[test]
fn the_specular_cone_widens_with_roughness() {
    let mut last = 0.0;
    for i in 0..=100 {
        let alpha = i as f32 / 100.0;
        let a = spec_cone_half_angle(alpha);
        assert!(a >= last - 1e-6, "{alpha}: {a} < {last}");
        assert!((SPEC_CONE_MIN_ANGLE..=std::f32::consts::FRAC_PI_2).contains(&a));
        last = a;
    }
    assert_eq!(spec_cone_half_angle(0.0), SPEC_CONE_MIN_ANGLE);
    assert_eq!(spec_cone_half_angle(0.1), SPEC_CONE_MIN_ANGLE);
    // Between the floor and the hemisphere the half-angle is twice the half-vector angle at which
    // GGX falls to half its peak: D(θ) / D(0) = α⁴ / (cos²θ (α² − 1) + 1)².
    for alpha in [0.25f32, 0.4, 0.6] {
        let half = 0.5 * spec_cone_half_angle(alpha);
        let c2 = half.cos() * half.cos();
        let a2 = alpha * alpha;
        let ratio = a2 * a2 / (c2 * (a2 - 1.0) + 1.0).powi(2);
        assert!((ratio - 0.5).abs() < 1e-3, "{alpha}: D ratio {ratio}");
    }
    // A lobe that never halves inside the hemisphere is the hemisphere.
    assert_eq!(spec_cone_half_angle(0.9), std::f32::consts::FRAC_PI_2);
    assert_eq!(spec_cone_half_angle(1.0), std::f32::consts::FRAC_PI_2);
}

#[test]
fn cap_intersection_limits() {
    let (a, b) = (0.4, 1.0);
    // One cap inside the other: the smaller's area.
    assert_eq!(cap_intersection(a, b, 0.3), cap_solid_angle(a));
    assert_eq!(cap_intersection(b, a, 0.5), cap_solid_angle(a));
    // Apart: nothing.
    assert_eq!(cap_intersection(a, b, 1.4), 0.0);
    // Continuous at both ends of the partial band.
    assert!((cap_intersection(a, b, 0.6 + 1e-4) - cap_solid_angle(a)).abs() < 1e-3);
    assert!(cap_intersection(a, b, 1.4 - 1e-4) < 1e-3);
    // Monotonic in between.
    let mut last = f32::INFINITY;
    for i in 0..=40 {
        let d = 0.6 + 0.8 * i as f32 / 40.0;
        let v = cap_intersection(a, b, d);
        assert!(v <= last + 1e-6);
        last = v;
    }
}

#[test]
fn cone_occlusion_is_one_on_open_surfaces_and_zero_when_closed() {
    let n = Vec3::Y;
    for alpha in [0.01, 0.1, 0.4, 0.8, 1.0] {
        for deg in [0.0f32, 30.0, 60.0, 85.0] {
            let r = Vec3::new(deg.to_radians().sin(), deg.to_radians().cos(), 0.0);
            assert_eq!(cone_specular_occlusion(n, 1.0, n, r, alpha), 1.0, "{alpha} {deg}");
            assert_eq!(cone_specular_occlusion(n, 0.0, n, r, alpha), 0.0, "{alpha} {deg}");
        }
    }
}

#[test]
fn cone_occlusion_falls_as_the_ao_closes_and_spares_the_open_side() {
    let n = Vec3::Y;
    let r = Vec3::new(0.5, 0.866, 0.0);
    let mut last = 1.0;
    for i in (0..=10).rev() {
        let ao = i as f32 / 10.0;
        let so = cone_specular_occlusion(n, ao, n, r, 0.3);
        assert!(so <= last + 1e-6, "ao {ao}: {so} > {last}");
        last = so;
    }
    // Bent toward +x: a reflection toward the open side keeps more than one toward the occluder.
    let bent = Vec3::new(0.34, 0.94, 0.0).normalize();
    let open_side = cone_specular_occlusion(bent, 0.6, n, r, 0.3);
    let blocked_side = cone_specular_occlusion(bent, 0.6, n, Vec3::new(-0.5, 0.866, 0.0), 0.3);
    assert!(open_side > blocked_side, "{open_side} vs {blocked_side}");
}

/// The share of the specular cone's directions above the horizon that fall inside the visibility
/// cone, counted over a Fibonacci sphere.
fn cone_reference(points: &[Vec3], bent: Vec3, ao: f32, n: Vec3, r: Vec3, alpha: f32) -> f32 {
    let spec = spec_cone_half_angle(alpha).cos();
    let vis = (1.0 - ao).clamp(0.0, 1.0).sqrt().acos().cos();
    let (mut open, mut seen) = (0u32, 0u32);
    for d in points {
        if d.dot(r) >= spec && d.dot(n) > 0.0 {
            open += 1;
            if d.dot(bent) >= vis {
                seen += 1;
            }
        }
    }
    if open == 0 { 1.0 } else { seen as f32 / open as f32 }
}

#[test]
fn cone_occlusion_follows_the_counted_cone_overlap() {
    let count = 50_000;
    let points: Vec<Vec3> = (0..count)
        .map(|i| {
            let y = 1.0 - (i as f32 + 0.5) / count as f32 * 2.0;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let phi = i as f32 * 2.399_963;
            Vec3::new(r * phi.cos(), y, r * phi.sin())
        })
        .collect();
    let n = Vec3::Y;
    let mut worst = 0.0f32;
    for ao in [0.9, 0.6, 0.3] {
        for tilt in [0.0f32, 20.0] {
            let bent = Vec3::new(tilt.to_radians().sin(), tilt.to_radians().cos(), 0.0);
            for deg in [0.0f32, 45.0, 75.0, -45.0, -75.0] {
                let r = Vec3::new(deg.to_radians().sin(), deg.to_radians().cos(), 0.0);
                for alpha in [0.1, 0.4, 0.8] {
                    let approx = cone_specular_occlusion(bent, ao, n, r, alpha);
                    let counted = cone_reference(&points, bent, ao, n, r, alpha);
                    worst = worst.max((approx - counted).abs());
                }
            }
        }
    }
    eprintln!("cone occlusion: worst error {worst:.3} against the counted overlap");
    // The smoothstep stands in for the exact lens area (Oat & Sander 2007): about 0.15 at worst.
    assert!(worst < 0.2, "{worst}");
}

/// The text of WGSL function `name` in `src` (from `fn name(` to the closing brace at column 0).
fn wgsl_fn<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{name} missing"));
    let len = src[start..]
        .find("\n}\n")
        .unwrap_or_else(|| panic!("{name} unclosed"));
    &src[start..start + len + 2]
}

/// The value of WGSL constant `const name: f32 = value;` in `src`.
fn wgsl_const(src: &str, name: &str) -> f32 {
    let key = format!("const {name}: f32 = ");
    let start = src.find(&key).unwrap_or_else(|| panic!("{name} missing")) + key.len();
    let end = start
        + src[start..]
            .find(';')
            .unwrap_or_else(|| panic!("{name} unterminated"));
    src[start..end].parse().unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn wgsl_copies_match() {
    let ssao = include_str!("../../../shaders/ssao.wgsl");
    // The forward pass's occlusion helpers live in its `regia::forward::occlusion` module
    // (ADR-1552 split of `lab_forward.wgsl`).
    let forward = include_str!("../../../shaders/forward_occlusion.wgsl");
    assert_eq!(wgsl_const(ssao, "BENT_TILT"), BENT_TILT);
    assert_eq!(wgsl_const(ssao, "BENT_MAX_ANGLE"), BENT_MAX_ANGLE);
    assert_eq!(wgsl_const(forward, "SPEC_CONE_MIN_ANGLE"), SPEC_CONE_MIN_ANGLE);
    // The forward pass decodes the lean exactly as the blur does.
    assert_eq!(wgsl_fn(ssao, "bent_decode"), wgsl_fn(forward, "bent_decode"));
    for name in ["bent_normal_lean", "bent_encode", "bent_decode", "store_texel"] {
        wgsl_fn(ssao, name);
    }
    for name in [
        "bent_shading_normal",
        "spec_cone_half_angle",
        "cap_solid_angle",
        "cap_intersection",
        "cone_specular_occlusion",
    ] {
        wgsl_fn(forward, name);
    }
}
