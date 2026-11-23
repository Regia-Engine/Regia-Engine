// SPDX-License-Identifier: MPL-2.0
//! Effect bounds (P-225; `.vfx.ron` v8 fold ADR-8105 row (h); ADR-8355).
//!
//! Three kinds of bounds, one rule for each:
//!
//! - **Fixed** ([`FixedBoundsDef`], `system.bounds`): an authored box in the system's local space.
//!   [`fixed_world_aabb`] transforms it by an instance (origin, axes, scale) the way the kill volume
//!   is transformed ([`crate::kill_volume::resolve`]). When set it is the instance's culling volume
//!   ([`culling_volume`], used by [`crate::significance`]) and the editor's bounds readout.
//! - **Measured** ([`measured_bounds`]): the CPU reference ([`crate::probe`]) run with every motion
//!   module of the GPU update kernel (gravity, drag, wind, curl noise, vortex, attractor, kill
//!   plane and volume), each live particle grown by its drawn extent ([`particle_extent`]).
//! - **GPU** ([`crate::gpu::VfxGpu::read_particle_bounds`]): a reduction over the live pool that
//!   applies the same extent rule on the GPU, read back on request (the editor asks at a bounded
//!   rate, opt-in).
//!
//! The extent of a particle is a radius around its centre that holds every vertex the renderer
//! can draw for it, view independent: a camera-facing quad of width `s` rotated any angle reaches
//! `s·√2/2`; a stretched quad reaches half its diagonal (`max(velocity_scale·speed, min_length)` by
//! `s`, the view-depth clamp ignored); a mesh particle (the built-in cube of edge `s`) reaches
//! `s·√3/2`. Not included: the screen-size floor (a per-view enlargement of distant particles) and
//! a registered render mesh larger than the unit cube (`render.mesh`).

use regia_core::math::{Aabb, Vec3};

use crate::format::{EmitterDef, FixedBoundsDef, RenderMode, VfxSystemDef};
use crate::probe::ProbeSettings;

/// `√2 / 2`: the corner of a camera-facing quad of width 1, any rotation.
pub const QUAD_CORNER: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// `√3 / 2`: the corner of the built-in mesh-particle cube of edge 1.
pub const CUBE_CORNER: f32 = 0.866_025_4;

/// The render-mode code of the GPU emitter record (`render.x`): 0 billboard, 1 stretched
/// billboard, 2 mesh, 3 ribbon.
#[must_use]
pub fn render_mode_code(mode: RenderMode) -> u32 {
    match mode {
        RenderMode::Billboard => 0,
        RenderMode::Stretched => 1,
        RenderMode::Mesh => 2,
        RenderMode::Ribbon => 3,
    }
}

/// The radius around a particle's centre that holds everything drawn for it (module docs).
/// `mode` is [`render_mode_code`], `size` the current size (base size × size over life), `speed`
/// the particle's speed and `stretch` `(velocity_scale, min_length)` of the emitter. The GPU
/// reduction's `particle_extent` is this function, operation for operation.
#[must_use]
pub fn particle_extent(mode: u32, size: f32, speed: f32, stretch: [f32; 2]) -> f32 {
    let size = size.abs();
    match mode {
        1 => {
            let min_len = if stretch[1] < 0.0 {
                -stretch[1] * size
            } else {
                stretch[1]
            };
            let along = (stretch[0] * speed).max(min_len).max(0.0);
            (0.25 * (along * along + size * size)).sqrt()
        }
        2 => size * CUBE_CORNER,
        _ => size * QUAD_CORNER,
    }
}

/// [`particle_extent`] for one emitter of a definition.
#[must_use]
pub fn emitter_extent(emitter: &EmitterDef, size: f32, speed: f32) -> f32 {
    let s = &emitter.render.stretch;
    particle_extent(
        render_mode_code(emitter.render.mode),
        size,
        speed,
        [s.velocity_scale, s.min_length],
    )
}

/// Local `v` in world space for an instance with axes `(right, up, forward)`.
fn to_world(axes: (Vec3, Vec3, Vec3), v: Vec3) -> Vec3 {
    axes.0 * v.x + axes.1 * v.y + axes.2 * v.z
}

/// The world AABB of `bounds` on an instance at `origin` with axes `(right, up, forward)` and
/// spatial `scale`: the transformed box's centre plus its rotated half extents projected on the
/// world axes (exact for the rotated box). Computed in `f64` and saturated to the `f32` range, so
/// every valid box (even one reaching `f32::MAX`) gives a finite box.
#[must_use]
pub fn fixed_world_aabb(bounds: &FixedBoundsDef, origin: Vec3, axes: (Vec3, Vec3, Vec3), scale: f32) -> Aabb {
    let scale = f64::from(if scale.is_finite() { scale.abs() } else { 1.0 });
    let (r, u, f) = (axes.0.as_dvec3(), axes.1.as_dvec3(), axes.2.as_dvec3());
    let lo = Vec3::from_array(bounds.min).as_dvec3();
    let hi = Vec3::from_array(bounds.max).as_dvec3();
    let c = (lo + hi) * 0.5;
    let h = (hi - lo) * 0.5 * scale;
    let centre = origin.as_dvec3() + (r * c.x + u * c.y + f * c.z) * scale;
    let half = r.abs() * h.x + u.abs() * h.y + f.abs() * h.z;
    Aabb::new(saturate(centre - half), saturate(centre + half))
}

/// `v` in the `f32` range: values past `±f32::MAX` saturate.
fn saturate(v: regia_core::glam::DVec3) -> Vec3 {
    let max = f64::from(f32::MAX);
    v.clamp(
        regia_core::glam::DVec3::splat(-max),
        regia_core::glam::DVec3::splat(max),
    )
    .as_vec3()
}

/// What an instance is culled by ([`culling_volume`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CullingVolume {
    /// The computed bound: a sphere at the instance origin with
    /// [`crate::significance::system_radius`] times the instance scale.
    Computed {
        /// Sphere centre (the instance origin).
        center: Vec3,
        /// Sphere radius, metres.
        radius: f32,
    },
    /// The authored fixed bounds, in world space.
    Fixed(Aabb),
}

impl CullingVolume {
    /// The centre the significance distance is measured to.
    #[must_use]
    pub fn center(&self) -> Vec3 {
        match self {
            Self::Computed { center, .. } => *center,
            Self::Fixed(b) => b.min * 0.5 + b.max * 0.5,
        }
    }

    /// The radius of a sphere holding the volume (the projected size of the score).
    #[must_use]
    pub fn radius(&self) -> f32 {
        match self {
            Self::Computed { radius, .. } => *radius,
            // Half the diagonal, in f64 so a box near `f32::MAX` saturates instead of overflowing.
            Self::Fixed(b) => {
                let half = (b.max.as_dvec3() - b.min.as_dvec3()) * 0.5;
                half.length().min(f64::from(f32::MAX)) as f32
            }
        }
    }
}

/// The culling volume of an instance of `def` at `origin` with `axes` and `scale`: the fixed
/// bounds when the definition has them, else the computed sphere.
#[must_use]
pub fn culling_volume(
    def: &VfxSystemDef,
    origin: Vec3,
    axes: (Vec3, Vec3, Vec3),
    scale: f32,
) -> CullingVolume {
    match &def.system.bounds {
        Some(b) if b.is_valid() => CullingVolume::Fixed(fixed_world_aabb(b, origin, axes, scale)),
        _ => CullingVolume::Computed {
            center: origin,
            radius: crate::significance::system_radius(def) * scale,
        },
    }
}

/// Per-emitter measured bounds after a run ([`measured_bounds`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MeasuredBounds {
    /// One entry per emitter of the definition: the world AABB of its live particles grown by
    /// their extent, `None` when it has none alive.
    pub emitters: Vec<Option<Aabb>>,
    /// Live particles measured (all emitters).
    pub particles: u32,
}

impl MeasuredBounds {
    /// The union over every emitter, `None` when nothing is alive.
    #[must_use]
    pub fn system(&self) -> Option<Aabb> {
        self.emitters.iter().flatten().copied().reduce(|a, b| a.union(&b))
    }
}

/// Measured bounds of `def` fired at `origin` with local +Z along `forward`, after exactly
/// `steps` steps of the CPU reference with every motion module on (module docs). Particles with
/// a non-finite position, or whose grown box overflows, are skipped.
#[must_use]
pub fn measured_bounds(
    def: &VfxSystemDef,
    origin: Vec3,
    forward: Vec3,
    settings: ProbeSettings,
    steps: u32,
) -> MeasuredBounds {
    let _span = tracing::info_span!("vfx_measured_bounds").entered();
    let mut out = MeasuredBounds {
        emitters: vec![None; def.emitters.len()],
        particles: 0,
    };
    for p in crate::probe::probe_steps_drawn(def, origin, forward, settings, steps) {
        let Some(emitter) = def.emitters.get(p.emitter) else {
            continue;
        };
        if !p.position.is_finite() {
            continue;
        }
        let r = emitter_extent(emitter, p.size, p.velocity.length());
        let r = if r.is_finite() { r } else { 0.0 };
        let b = Aabb::new(p.position - Vec3::splat(r), p.position + Vec3::splat(r));
        // A box that overflows (a centre near f32::MAX) is skipped, as the GPU reduction does.
        if !(b.min.is_finite() && b.max.is_finite()) {
            continue;
        }
        if let Some(slot) = out.emitters.get_mut(p.emitter) {
            *slot = Some(slot.map_or(b, |have| have.union(&b)));
            out.particles += 1;
        }
    }
    out
}

/// A fixed bounds box in local space that holds a world AABB measured on an instance at
/// `origin` with orthonormal axes `(right, up, forward)` and spatial `scale` (the editor preview's
/// instance): the inverse of [`fixed_world_aabb`], exact when the axes are world axes (in any
/// order or sign), conservative otherwise. `None` for an empty or non-finite box or a zero scale.
#[must_use]
pub fn fixed_from_measured(
    measured: Aabb,
    origin: Vec3,
    axes: (Vec3, Vec3, Vec3),
    scale: f32,
) -> Option<FixedBoundsDef> {
    if measured.is_empty() || !measured.min.is_finite() || !measured.max.is_finite() {
        return None;
    }
    let scale = scale.abs();
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let c = (measured.center() - origin) / scale;
    let h = measured.half_extents() / scale;
    let (r, u, f) = axes;
    let local_c = Vec3::new(r.dot(c), u.dot(c), f.dot(c));
    let local_h = Vec3::new(r.abs().dot(h), u.abs().dot(h), f.abs().dot(h));
    let b = FixedBoundsDef {
        min: (local_c - local_h).to_array(),
        max: (local_c + local_h).to_array(),
    };
    b.is_valid().then_some(b)
}

// ---------------------------------------------------------------------------
// Motion forces of the update kernel (curl, vortex, attractor) for the CPU reference
// ---------------------------------------------------------------------------

/// One emitter's vortex, attractor and curl as the GPU emitter record carries them for an
/// instance ([`crate::scene`]'s `emitter_record`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct MotionForces {
    /// Normalised vortex axis and tangential strength (`w`).
    vortex: [f32; 4],
    /// Extra radial acceleration of the vortex.
    vortex_radial: f32,
    /// The instance origin (a point on the vortex axis).
    origin: Vec3,
    /// World attractor position and strength (`w`).
    attractor: [f32; 4],
    /// Curl `(amount, frequency, speed)`.
    curl: [f32; 3],
}

impl MotionForces {
    /// The forces of `emitter` on an instance at `origin` with `axes` (as `emitter_record`).
    pub(crate) fn resolve(emitter: &EmitterDef, origin: Vec3, axes: (Vec3, Vec3, Vec3)) -> Self {
        let u = &emitter.update;
        let vortex = u.vortex.as_ref().map_or([0.0; 4], |v| {
            let axis = crate::format::dir_from_tuple(v.axis);
            [axis.x, axis.y, axis.z, v.strength]
        });
        let attractor = u.attractor.as_ref().map_or([0.0; 4], |a| {
            let p = origin + to_world(axes, Vec3::new(a.position.0, a.position.1, a.position.2));
            [p.x, p.y, p.z, a.strength]
        });
        Self {
            vortex,
            vortex_radial: u.vortex.as_ref().map_or(0.0, |v| v.radial),
            origin,
            attractor,
            curl: u.curl.map_or([0.0; 3], |(a, f, s)| [a, f, s]),
        }
    }

    /// The kernel's velocity change for a particle at `pos` with velocity `v` over `dt`, applied
    /// after the gravity, wind and drag step (`gpu/shaders.rs`, `update`, in that order).
    pub(crate) fn apply(&self, pos: Vec3, mut v: Vec3, dt: f32) -> Vec3 {
        let [ax, ay, az, strength] = self.vortex;
        if strength.abs() > 0.0 || self.vortex_radial.abs() > 0.0 {
            let rel = pos - self.origin;
            let axis = (Vec3::new(ax, ay, az) + Vec3::splat(1e-6)).normalize_or_zero();
            let tangential = axis.cross(rel);
            let radial = rel - axis * rel.dot(axis);
            let rt = tangential.length() + 1e-5;
            let rr = radial.length() + 1e-5;
            v += (tangential / rt * strength + radial / rr * (strength * 0.25 + self.vortex_radial)) * dt;
        }
        let [px, py, pz, pull] = self.attractor;
        if pull.abs() > 0.0 {
            let d = Vec3::new(px, py, pz) - pos;
            let dist2 = d.dot(d) + 1.0;
            v += d * (1.0 / dist2.sqrt()) * (pull / dist2) * dt;
        }
        let [amount, frequency, speed] = self.curl;
        if amount > 0.0 {
            v += curl_noise(pos * frequency, speed) * amount * dt;
        }
        v
    }
}

/// The shaders' `hash_u32` (PCG output permutation).
fn hash_u32(seed: u32) -> u32 {
    let state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// The shaders' `corner_hash`.
fn corner_hash(ii: [u32; 3], off: [u32; 3], channel: u32) -> f32 {
    let h = hash_u32(
        ii[0].wrapping_add(off[0]).wrapping_mul(197)
            ^ ii[1].wrapping_add(off[1]).wrapping_mul(60_493)
            ^ ii[2].wrapping_add(off[2]).wrapping_mul(1759)
            ^ channel,
    );
    (h & 0x00FF_FFFF) as f32 / 16_777_215.0
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1.0 - t) + b * t
}

/// The shaders' trilinear lattice `value_noise` in `[0, 1]`.
fn value_noise(p: Vec3, channel: u32) -> f32 {
    let ip = p.floor();
    let f = p - ip;
    let w = f * f * (Vec3::splat(3.0) - 2.0 * f);
    // The signed lattice index bit-cast to u32, as the shader does.
    let ii = [ip.x as i32 as u32, ip.y as i32 as u32, ip.z as i32 as u32];
    let c = |x: u32, y: u32, z: u32| corner_hash(ii, [x, y, z], channel);
    let x00 = mix(c(0, 0, 0), c(1, 0, 0), w.x);
    let x10 = mix(c(0, 1, 0), c(1, 1, 0), w.x);
    let x01 = mix(c(0, 0, 1), c(1, 0, 1), w.x);
    let x11 = mix(c(0, 1, 1), c(1, 1, 1), w.x);
    mix(mix(x00, x10, w.y), mix(x01, x11, w.y), w.z)
}

fn potential(p: Vec3) -> Vec3 {
    Vec3::new(value_noise(p, 1), value_noise(p, 2), value_noise(p, 3))
}

/// The shaders' `curl_noise`: the curl of a 3-channel potential by central differences
/// (Bridson, Hourihan and Nordenstam, "Curl-Noise for Procedural Fluid Flow", 2007).
fn curl_noise(p: Vec3, scale: f32) -> Vec3 {
    let e = 0.75;
    let dx = (potential(p + Vec3::new(e, 0.0, 0.0)) - potential(p - Vec3::new(e, 0.0, 0.0))) / (2.0 * e);
    let dy = (potential(p + Vec3::new(0.0, e, 0.0)) - potential(p - Vec3::new(0.0, e, 0.0))) / (2.0 * e);
    let dz = (potential(p + Vec3::new(0.0, 0.0, e)) - potential(p - Vec3::new(0.0, 0.0, e))) / (2.0 * e);
    Vec3::new(dy.z - dz.y, dz.x - dx.z, dx.y - dy.x) * scale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(text: &str) -> VfxSystemDef {
        VfxSystemDef::from_ron(text).expect("definition")
    }

    /// One still billboard particle of size 2 at the origin (no velocity, no forces).
    const STILL: &str = r#"(version: 8, name: "still", emitters: [(name: "e", spawn: (mode: Burst(1),
        lifetime: (min: 5.0, max: 5.0)), init: (shape: Point, velocity: (mode: Radial(speed: (min: 0.0, max: 0.0))),
        size: (min: 2.0, max: 2.0)), render: (mode: Billboard))])"#;

    /// Acceptance (P-225 (2)): a still particle's measured bounds are its centre grown by the
    /// drawn quad's corner radius, not the bare centre the w12 readout reported.
    #[test]
    fn measured_bounds_include_particle_extent() {
        let d = def(STILL);
        let settings = ProbeSettings {
            ground: None,
            ..ProbeSettings::default()
        };
        let m = measured_bounds(&d, Vec3::ZERO, Vec3::Z, settings, 10);
        assert_eq!(m.particles, 1);
        let b = m.system().expect("one live particle");
        let r = 2.0 * QUAD_CORNER;
        for (got, want) in b.min.to_array().iter().zip([-r; 3]) {
            assert!((got - want).abs() < 1e-4, "{b:?}");
        }
        for (got, want) in b.max.to_array().iter().zip([r; 3]) {
            assert!((got - want).abs() < 1e-4, "{b:?}");
        }
    }

    /// Every motion module moves the measured bounds: an attractor pulls the cloud toward its
    /// centre, a vortex swings it around the axis and curl noise moves it (the w12 readout left
    /// all three out).
    #[test]
    fn measured_bounds_follow_every_motion_module() {
        let base = r#"(version: 8, name: "m", emitters: [(name: "e", spawn: (mode: Burst(16), lifetime: (min: 4.0, max: 4.0)),
            init: (shape: Point, velocity: (mode: Directional(dir: (1.0, 0.0, 0.0), spread: 0.0, speed: (min: 1.0, max: 1.0))),
            size: (min: 0.1, max: 0.1)), update: (UPDATE), render: (mode: Billboard))])"#;
        let settings = ProbeSettings {
            ground: None,
            ..ProbeSettings::default()
        };
        let run = |update: &str| {
            measured_bounds(
                &def(&base.replace("UPDATE", update)),
                Vec3::ZERO,
                Vec3::Z,
                settings,
                120,
            )
            .system()
            .expect("alive")
        };
        let free = run("");
        let attract = run("attractor: Some((position: (0.0, 10.0, 0.0), strength: 40.0))");
        assert!(
            attract.max.y > free.max.y + 0.5,
            "attractor: {attract:?} vs {free:?}"
        );
        let vortex = run("vortex: Some((axis: (0.0, 1.0, 0.0), strength: 3.0))");
        assert!(
            (vortex.center() - free.center()).length() > 0.2,
            "vortex: {vortex:?} vs {free:?}"
        );
        let curl = run("curl: Some((8.0, 1.3, 1.0))");
        assert!(curl != free, "curl moved nothing: {curl:?}");
    }

    /// A stretched particle reaches half its streak; a mesh particle its cube corner.
    #[test]
    fn extent_rules_per_render_mode() {
        assert!((particle_extent(0, 1.0, 9.0, [0.05, 0.02]) - QUAD_CORNER).abs() < 1e-6);
        assert!((particle_extent(2, 1.0, 0.0, [0.0, 0.0]) - CUBE_CORNER).abs() < 1e-6);
        // velocity_scale 0.5 at 8 m/s: a 4 m streak, width 1 → half diagonal √(4 + 0.25).
        let r = particle_extent(1, 1.0, 8.0, [0.5, 0.02]);
        assert!((r - (4.0f32 + 0.25).sqrt()).abs() < 1e-5, "{r}");
        // A negative minimum is a multiple of the size.
        let r = particle_extent(1, 2.0, 0.0, [0.0, -1.5]);
        assert!((r - (0.25f32 * (9.0 + 4.0)).sqrt()).abs() < 1e-5, "{r}");
    }

    /// The fixed box follows the instance's rotation and scale; set from measured, it is the
    /// measured box relative to the origin.
    #[test]
    fn fixed_box_transforms_with_the_instance() {
        let b = FixedBoundsDef {
            min: [-1.0, 0.0, -1.0],
            max: [1.0, 4.0, 1.0],
        };
        // Rotated so local +Y points along world +X, scaled 2, at (10, 0, 0).
        let axes = (Vec3::new(0.0, -1.0, 0.0), Vec3::X, Vec3::Z);
        let w = fixed_world_aabb(&b, Vec3::new(10.0, 0.0, 0.0), axes, 2.0);
        assert!((w.min - Vec3::new(10.0, -2.0, -2.0)).length() < 1e-5, "{w:?}");
        assert!((w.max - Vec3::new(18.0, 2.0, 2.0)).length() < 1e-5, "{w:?}");
        let identity = (Vec3::X, Vec3::Y, Vec3::Z);
        let measured = Aabb::new(Vec3::new(4.0, 5.0, 6.0), Vec3::new(5.0, 7.0, 9.0));
        let f = fixed_from_measured(measured, Vec3::new(4.0, 5.0, 6.0), identity, 1.0).expect("valid");
        assert_eq!(f.min, [0.0, 0.0, 0.0]);
        assert_eq!(f.max, [1.0, 2.0, 3.0]);
        assert!(fixed_from_measured(Aabb::EMPTY, Vec3::ZERO, identity, 1.0).is_none());
        // Back through the rotated, scaled instance: the same world box.
        let local = fixed_from_measured(w, Vec3::new(10.0, 0.0, 0.0), axes, 2.0).expect("valid");
        for (got, want) in local.min.iter().chain(&local.max).zip(b.min.iter().chain(&b.max)) {
            assert!((got - want).abs() < 1e-5, "{local:?} vs {b:?}");
        }
    }

    /// The `vfx-bounds` fuzz finding: a valid box reaching `f32::MAX` overflowed the culling
    /// radius to infinity. Every reading of such a box is finite now.
    #[test]
    fn a_box_at_the_float_limit_gives_finite_culling_readings() {
        let b = FixedBoundsDef {
            min: [0.0, -0.5, -f32::MAX],
            max: [f32::MAX, 6.0, f32::MAX],
        };
        assert!(b.is_valid());
        assert!(b.center().is_finite() && b.half_extents().is_finite());
        let tilted = (Vec3::new(0.6, 0.8, 0.0), Vec3::new(-0.8, 0.6, 0.0), Vec3::Z);
        for (axes, scale) in [((Vec3::X, Vec3::Y, Vec3::Z), 1.0), (tilted, 3.0)] {
            let w = fixed_world_aabb(&b, Vec3::new(5.0, 0.0, 0.0), axes, scale);
            assert!(w.min.is_finite() && w.max.is_finite(), "{w:?}");
            assert!(w.min.cmple(w.max).all(), "{w:?}");
            let v = CullingVolume::Fixed(w);
            assert!(v.center().is_finite() && v.radius().is_finite(), "{v:?}");
        }
    }

    /// The CPU curl mirror is the shader's field: finite, bounded and not constant.
    #[test]
    fn curl_mirror_is_finite_and_varies() {
        let a = curl_noise(Vec3::new(0.3, -2.7, 5.1), 1.0);
        let b = curl_noise(Vec3::new(-4.2, 1.9, 0.6), 1.0);
        assert!(a.is_finite() && b.is_finite());
        assert!(a != b);
        assert!(a.length() < 10.0 && b.length() < 10.0);
    }
}
