// SPDX-License-Identifier: MPL-2.0
//! Lockstep contact response for the disc bodies of [`crate::broadphase`] (ADR-8563).
//!
//! [`crate::broadphase`] finds the overlapping pairs, in the all-pairs loop's `(a, b)` order. This
//! module answers each one: it pushes the two discs apart, split by inverse mass, and exchanges an
//! impulse along the contact normal (with restitution) and along the tangent (capped by Coulomb
//! friction). A pair that closes faster than [`ContactConfig::ram_threshold_m_s`] is reported as a
//! [`Ram`] for the game's damage rules.
//!
//! Per body the solver reads a [`ContactBody`] (inverse mass, restitution, friction, the push cap),
//! a slice parallel to the broadphase bodies, and a velocity `[x, y, z]`. A body with
//! `inv_mass == 0` is static or kinematic: the solver never moves it or changes its velocity, and
//! the other body takes the whole push and the whole impulse.
//!
//! [`resolve_all`] drives [`DetBroadphase`] and [`resolve_all_n2`] drives the reference loop
//! [`for_each_contact_n2`]; both call [`resolve`] on the same contacts in the same order, so they
//! give the same bits and a host can switch between them without a checksum change. The floor rule
//! is the broadphase's ([`crate::broadphase::FloorRule`]): bodies on different floors never touch.
//!
//! The order rule of the reference loop holds: each pair sees the pushes and impulses of the pairs
//! before it. Velocities are not integrated here; the host moves its bodies before or after the pass.
//!
//! ```
//! use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
//! use regia_detsim::contact::{self, ContactBody, ContactConfig};
//!
//! let mut bodies = vec![
//!     BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//!     BroadBody::new([1.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//! ];
//! // A 1 kg body and a 3 kg body.
//! let props = [ContactBody::with_mass(1.0), ContactBody::with_mass(3.0)];
//! let mut vel = vec![[2.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
//! let mut grid = DetBroadphase::new(BroadphaseConfig::default());
//! let mut rams = Vec::new();
//! let stats = contact::resolve_all(
//!     &mut grid,
//!     &mut bodies,
//!     &props,
//!     &mut vel,
//!     &ContactConfig::default(),
//!     &mut rams,
//! )?;
//! assert_eq!(stats.pairs, 1);
//! // The 0.5 m overlap is split 3:1 (the light body moves three times as far).
//! assert!((bodies[0].pos[0] + 0.375).abs() < 1e-6);
//! assert!((bodies[1].pos[0] - 1.625).abs() < 1e-6);
//! // Restitution 0: the bodies leave the contact at the same speed, and momentum is kept.
//! assert!((vel[0][0] - vel[1][0]).abs() < 1e-6);
//! assert!((vel[0][0] * 1.0 + vel[1][0] * 3.0 - 2.0).abs() < 1e-5);
//! # Ok::<(), regia_detsim::contact::ContactError>(())
//! ```

use std::fmt;

use crate::broadphase::{BroadBody, Contact, DetBroadphase, FloorRule, for_each_contact_n2};
use crate::float::{det_clamp, det_max, det_min};
use crate::math;

mod pile;

pub use pile::ContactPile;

/// Below this centre distance (metres) a pair counts as coincident: the normal is the fixed
/// fallback axis `+X` from the lower slot to the higher one (see [`resolve`]).
pub const DEGENERATE_DIST_M: f32 = 1.0e-6;

/// The contact settings of one body, a slice parallel to the broadphase bodies. Settings, never
/// hard-coded constants: the host fills them per kind of body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactBody {
    /// `1 / mass` (1/kg). `0` makes the body static or kinematic: the solver never moves it or
    /// changes its velocity. Finite and `>= 0`.
    pub inv_mass: f32,
    /// Bounciness in `[0, 1]`; a pair uses the larger of its two values. `0` stops the closing
    /// motion, `1` reverses it at the same relative speed. Above `1` would add energy and is
    /// rejected by [`ContactBody::validate`].
    pub restitution: f32,
    /// Coulomb friction coefficient (`>= 0`); a pair uses `sqrt(fa * fb)`. The tangential impulse
    /// is at most `friction * normal impulse`.
    pub friction: f32,
    /// The most one contact may push this body in one pass (metres; `>= 0`, infinity for no cap).
    /// Each pair is visited once per pass, so this is the per-tick cap of one contact; it stops a
    /// deep overlap (a spawn on top of another body) from throwing the body across the map.
    pub max_push_m: f32,
}

impl Default for ContactBody {
    /// A 1 kg body, restitution 0, friction 0.5, push cap 0.5 m.
    fn default() -> Self {
        ContactBody {
            inv_mass: 1.0,
            restitution: 0.0,
            friction: 0.5,
            max_push_m: 0.5,
        }
    }
}

impl ContactBody {
    /// A static (or kinematic) body: never moved by contacts.
    pub const STATIC: ContactBody = ContactBody {
        inv_mass: 0.0,
        restitution: 0.0,
        friction: 0.5,
        max_push_m: 0.0,
    };

    /// The default body with mass `mass_kg`; a non-finite or non-positive mass makes it static.
    pub fn with_mass(mass_kg: f32) -> Self {
        let inv_mass = if mass_kg.is_finite() && mass_kg > 0.0 {
            1.0 / mass_kg
        } else {
            0.0
        };
        ContactBody {
            inv_mass,
            ..ContactBody::default()
        }
    }

    /// Whether the solver may move the body (`inv_mass > 0`).
    pub fn is_dynamic(&self) -> bool {
        self.inv_mass > 0.0
    }

    /// Check the ranges above. `slot` is echoed in the error.
    pub fn validate(&self, slot: usize) -> Result<(), ContactError> {
        let bad = |field| Err(ContactError::BadBody { slot, field });
        if !(self.inv_mass.is_finite() && self.inv_mass >= 0.0) {
            return bad("inv_mass");
        }
        if !(self.restitution >= 0.0 && self.restitution <= 1.0) {
            return bad("restitution");
        }
        if !(self.friction.is_finite() && self.friction >= 0.0) {
            return bad("friction");
        }
        if self.max_push_m.is_nan() || self.max_push_m < 0.0 {
            return bad("max_push_m");
        }
        Ok(())
    }
}

/// Settings shared by every contact of a pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactConfig {
    /// A pair closing faster than this (m/s, strictly) is reported as a [`Ram`]. `>= 0`; infinity
    /// reports none.
    pub ram_threshold_m_s: f32,
    /// Below or at this closing speed (m/s) a pair does not bounce (restitution 0), so a resting
    /// pile does not jitter. Finite and `>= 0`.
    pub bounce_min_speed_m_s: f32,
}

impl Default for ContactConfig {
    /// Rams above 6 m/s, no bounce at or below 0.5 m/s.
    fn default() -> Self {
        ContactConfig {
            ram_threshold_m_s: 6.0,
            bounce_min_speed_m_s: 0.5,
        }
    }
}

impl ContactConfig {
    /// Check the ranges above.
    pub fn validate(&self) -> Result<(), ContactError> {
        if self.ram_threshold_m_s.is_nan() || self.ram_threshold_m_s < 0.0 {
            return Err(ContactError::BadConfig {
                field: "ram_threshold_m_s",
            });
        }
        if !(self.bounce_min_speed_m_s.is_finite() && self.bounce_min_speed_m_s >= 0.0) {
            return Err(ContactError::BadConfig {
                field: "bounce_min_speed_m_s",
            });
        }
        Ok(())
    }
}

/// A pair that closed faster than [`ContactConfig::ram_threshold_m_s`], for a game's damage rules.
/// [`resolve_all`] appends them in visit order (ascending `(a, b)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ram {
    /// The lower slot.
    pub a: usize,
    /// The higher slot.
    pub b: usize,
    /// Unit contact normal from `a` to `b`, on XZ (`normal[1]` is 0).
    pub normal: [f32; 3],
    /// Closing speed along the normal before the response (m/s).
    pub closing_speed_m_s: f32,
    /// Normal impulse the solver applied (kg·m/s); 0 when both bodies are static or kinematic.
    pub impulse: f32,
}

/// Counters of a pass, the `sim.det_contact_stats` row a host exposes (no protocol method of its
/// own yet; the sim host forwards it when it lands).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContactStats {
    /// Contacts resolved.
    pub pairs: u64,
    /// Contacts that moved at least one body apart.
    pub pushes: u64,
    /// Contacts where a body's push was cut to its [`ContactBody::max_push_m`].
    pub capped_pushes: u64,
    /// Contacts reported as a [`Ram`].
    pub rams: u64,
}

impl ContactStats {
    /// Add `other`'s counters (wrapping).
    pub fn add(&mut self, other: &ContactStats) {
        self.pairs = self.pairs.wrapping_add(other.pairs);
        self.pushes = self.pushes.wrapping_add(other.pushes);
        self.capped_pushes = self.capped_pushes.wrapping_add(other.capped_pushes);
        self.rams = self.rams.wrapping_add(other.rams);
    }
}

/// Why a pass was refused (nothing was changed). [`ContactError::code`] is the protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactError {
    /// A [`ContactBody`] field is out of range (`contact_bad_body`).
    BadBody {
        /// The slot.
        slot: usize,
        /// The field.
        field: &'static str,
    },
    /// A [`ContactConfig`] field is out of range (`contact_bad_config`).
    BadConfig {
        /// The field.
        field: &'static str,
    },
    /// The bodies, settings and velocities differ in length (`contact_length_mismatch`).
    LengthMismatch {
        /// Broadphase bodies.
        bodies: usize,
        /// [`ContactBody`] slots.
        props: usize,
        /// Velocities.
        vel: usize,
    },
}

impl ContactError {
    /// The lowercase protocol code: `contact_bad_body`, `contact_bad_config` or
    /// `contact_length_mismatch`.
    pub fn code(&self) -> &'static str {
        match self {
            ContactError::BadBody { .. } => "contact_bad_body",
            ContactError::BadConfig { .. } => "contact_bad_config",
            ContactError::LengthMismatch { .. } => "contact_length_mismatch",
        }
    }
}

impl fmt::Display for ContactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContactError::BadBody { slot, field } => {
                write!(f, "{}: body {slot} has an out-of-range {field}", self.code())
            }
            ContactError::BadConfig { field } => {
                write!(f, "{}: contact config has an out-of-range {field}", self.code())
            }
            ContactError::LengthMismatch { bodies, props, vel } => write!(
                f,
                "{}: {bodies} bodies, {props} contact settings, {vel} velocities",
                self.code()
            ),
        }
    }
}

impl std::error::Error for ContactError {}

/// Check the lengths, every [`ContactBody`] and the config, as [`resolve_all`] does before a pass.
pub fn validate(
    bodies: &[BroadBody],
    props: &[ContactBody],
    vel: &[[f32; 3]],
    cfg: &ContactConfig,
) -> Result<(), ContactError> {
    if bodies.len() != props.len() || bodies.len() != vel.len() {
        return Err(ContactError::LengthMismatch {
            bodies: bodies.len(),
            props: props.len(),
            vel: vel.len(),
        });
    }
    cfg.validate()?;
    for (slot, p) in props.iter().enumerate() {
        p.validate(slot)?;
    }
    Ok(())
}

/// Resolve one contact from [`crate::broadphase::contact`]: push the two bodies apart and exchange
/// impulses. Returns a [`Ram`] when the pair closed faster than the threshold.
///
/// The steps, in this order (IEEE `f32`, source order, `sqrt` through [`crate::math`]):
///
/// 1. **Normal.** `dist = sqrt(dist_sq)`; `n = (dx, dz) / dist` from `a` to `b`. When `dist` is
///    below [`DEGENERATE_DIST_M`] or NaN (a coincident pair, or a body with a NaN position, which
///    the broadphase's NaN rule reports as touching), `n = (1, 0)`: the lower slot goes to `-X` and
///    the higher to `+X`, so the result never depends on a NaN's sign.
/// 2. **Push.** `overlap = min_dist - dist` (`dist` read as 0 when NaN). Body `a` moves
///    `overlap * wa / (wa + wb)` along `-n` and `b` moves `overlap * wb / (wa + wb)` along `n`
///    (`w` = inverse mass), each cut to its own `max_push_m`. A direct projection: no velocity
///    bias (Baumgarte-free). A non-finite overlap (an infinite or NaN radius) pushes nothing.
/// 3. **Normal impulse.** With `vn = (vel_b - vel_a) . n`, a closing pair (`vn < 0`) gets
///    `j = (1 + e) * -vn / (wa + wb)`, `e` the larger restitution (0 at or below
///    `bounce_min_speed_m_s`); `vel_a -= n * j * wa`, `vel_b += n * j * wb`.
/// 4. **Friction.** On the XZ tangent `t = (-nz, nx)`, `jt = clamp(-vt / (wa + wb), -mu * j,
///    mu * j)` with `mu = sqrt(fa * fb)` and `vt` measured after step 3. Vertical velocity is the
///    host's (floors and hover springs); the solver never changes `vel[1]`.
///
/// A body with `inv_mass == 0` is never written. A pair of two such bodies is not pushed and gets
/// no impulse, but may still be reported as a ram (`impulse` 0). A non-finite velocity on either
/// body skips steps 3 and 4. The result is finite whenever the inputs of the two bodies are.
///
/// The settings are not checked here; [`validate`] (which [`resolve_all`] runs) rejects
/// restitution above 1 and the other out-of-range values.
pub fn resolve(
    bodies: &mut [BroadBody],
    props: &[ContactBody],
    vel: &mut [[f32; 3]],
    c: Contact,
    cfg: &ContactConfig,
) -> Option<Ram> {
    resolve_counted(bodies, props, vel, c, cfg, &mut ContactStats::default())
}

/// [`resolve`] that also counts the contact into `stats`.
pub fn resolve_counted(
    bodies: &mut [BroadBody],
    props: &[ContactBody],
    vel: &mut [[f32; 3]],
    c: Contact,
    cfg: &ContactConfig,
    stats: &mut ContactStats,
) -> Option<Ram> {
    let (a, b) = (c.a, c.b);
    let (pa, pb) = (props[a], props[b]);
    let (wa, wb) = (pa.inv_mass, pb.inv_mass);
    let w = wa + wb;
    stats.pairs = stats.pairs.wrapping_add(1);

    // 1. Normal.
    let dist = math::sqrt(c.dist_sq);
    let (nx, nz) = if dist >= DEGENERATE_DIST_M {
        (c.dx / dist, c.dz / dist)
    } else {
        (1.0, 0.0)
    };

    // 2. Push.
    let overlap = c.min_dist - if dist.is_nan() { 0.0 } else { dist };
    if w > 0.0 && overlap > 0.0 && overlap.is_finite() {
        let mut capped = false;
        let mut moved = false;
        if wa > 0.0 {
            let want = overlap * wa / w;
            let push = det_min(want, pa.max_push_m);
            capped |= push < want;
            if push > 0.0 {
                bodies[a].pos[0] -= nx * push;
                bodies[a].pos[2] -= nz * push;
                moved = true;
            }
        }
        if wb > 0.0 {
            let want = overlap * wb / w;
            let push = det_min(want, pb.max_push_m);
            capped |= push < want;
            if push > 0.0 {
                bodies[b].pos[0] += nx * push;
                bodies[b].pos[2] += nz * push;
                moved = true;
            }
        }
        if moved {
            stats.pushes = stats.pushes.wrapping_add(1);
        }
        if capped {
            stats.capped_pushes = stats.capped_pushes.wrapping_add(1);
        }
    }

    // 3. Normal impulse.
    let (va, vb) = (vel[a], vel[b]);
    let vn = (vb[0] - va[0]) * nx + (vb[2] - va[2]) * nz;
    let closing = -vn;
    if !(closing > 0.0 && closing.is_finite()) {
        return None;
    }
    let mut j = 0.0;
    if w > 0.0 {
        let e = if closing > cfg.bounce_min_speed_m_s {
            det_max(pa.restitution, pb.restitution)
        } else {
            0.0
        };
        j = (1.0 + e) * closing / w;
        if wa > 0.0 {
            vel[a][0] -= nx * j * wa;
            vel[a][2] -= nz * j * wa;
        }
        if wb > 0.0 {
            vel[b][0] += nx * j * wb;
            vel[b][2] += nz * j * wb;
        }

        // 4. Friction.
        let mu = math::sqrt(pa.friction * pb.friction);
        let (tx, tz) = (-nz, nx);
        let vt = (vel[b][0] - vel[a][0]) * tx + (vel[b][2] - vel[a][2]) * tz;
        let jt_max = mu * j;
        let jt = det_clamp(-vt / w, -jt_max, jt_max);
        if jt != 0.0 {
            if wa > 0.0 {
                vel[a][0] -= tx * jt * wa;
                vel[a][2] -= tz * jt * wa;
            }
            if wb > 0.0 {
                vel[b][0] += tx * jt * wb;
                vel[b][2] += tz * jt * wb;
            }
        }
    }

    if closing > cfg.ram_threshold_m_s {
        stats.rams = stats.rams.wrapping_add(1);
        Some(Ram {
            a,
            b,
            normal: [nx, 0.0, nz],
            closing_speed_m_s: closing,
            impulse: j,
        })
    } else {
        None
    }
}

/// One contact pass through [`DetBroadphase`]: every contact of `bodies`, in the all-pairs loop's
/// order, through [`resolve`]. Rams are appended to `rams` (not cleared) in visit order.
///
/// Checks the lengths and settings first ([`validate`]) and changes nothing on an error. No
/// allocation once the broadphase's buffers and `rams` have grown to the pass's size.
pub fn resolve_all(
    broadphase: &mut DetBroadphase,
    bodies: &mut [BroadBody],
    props: &[ContactBody],
    vel: &mut [[f32; 3]],
    cfg: &ContactConfig,
    rams: &mut Vec<Ram>,
) -> Result<ContactStats, ContactError> {
    validate(bodies, props, vel, cfg)?;
    let _span = tracing::trace_span!("detsim_contacts", bodies = bodies.len()).entered();
    let mut stats = ContactStats::default();
    broadphase.for_each_contact(bodies, |bodies, c| {
        if let Some(ram) = resolve_counted(bodies, props, vel, c, cfg, &mut stats) {
            rams.push(ram);
        }
    });
    Ok(stats)
}

/// [`resolve_all`] through the reference all-pairs loop ([`for_each_contact_n2`]) with floor rule
/// `rule`: the oracle. Same contacts, same order, same bits as [`resolve_all`] with a broadphase of
/// the same rule.
pub fn resolve_all_n2(
    bodies: &mut [BroadBody],
    props: &[ContactBody],
    vel: &mut [[f32; 3]],
    cfg: &ContactConfig,
    rule: FloorRule,
    rams: &mut Vec<Ram>,
) -> Result<ContactStats, ContactError> {
    validate(bodies, props, vel, cfg)?;
    let mut stats = ContactStats::default();
    for_each_contact_n2(bodies, rule, |bodies, c| {
        if let Some(ram) = resolve_counted(bodies, props, vel, c, cfg, &mut stats) {
            rams.push(ram);
        }
    });
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(dx: f32) -> Vec<BroadBody> {
        vec![
            BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 1.0]),
            BroadBody::new([dx, 0.0, 0.0], 1.0, [-1.0, 1.0]),
        ]
    }

    #[test]
    fn validate_rejects_each_out_of_range_field() {
        let ok = ContactBody::default();
        assert_eq!(ok.validate(0), Ok(()));
        assert_eq!(ContactBody::STATIC.validate(0), Ok(()));
        let cases = [
            (ContactBody { inv_mass: -1.0, ..ok }, "inv_mass"),
            (
                ContactBody {
                    inv_mass: f32::INFINITY,
                    ..ok
                },
                "inv_mass",
            ),
            (
                ContactBody {
                    restitution: 1.0001,
                    ..ok
                },
                "restitution",
            ),
            (
                ContactBody {
                    restitution: f32::NAN,
                    ..ok
                },
                "restitution",
            ),
            (ContactBody { friction: -0.1, ..ok }, "friction"),
            (
                ContactBody {
                    max_push_m: f32::NAN,
                    ..ok
                },
                "max_push_m",
            ),
        ];
        for (body, field) in cases {
            let err = body.validate(7).expect_err(field);
            assert_eq!(err, ContactError::BadBody { slot: 7, field });
            assert_eq!(err.code(), "contact_bad_body");
        }
        assert_eq!(
            ContactBody {
                max_push_m: f32::INFINITY,
                ..ok
            }
            .validate(0),
            Ok(())
        );
        let cfg = ContactConfig {
            ram_threshold_m_s: f32::NAN,
            ..ContactConfig::default()
        };
        assert_eq!(cfg.validate().map_err(|e| e.code()), Err("contact_bad_config"));
    }

    #[test]
    fn a_length_mismatch_changes_nothing() {
        let mut bodies = pair(1.0);
        let before = bodies.clone();
        let mut vel = vec![[1.0, 0.0, 0.0]];
        let mut grid = DetBroadphase::default();
        let mut rams = Vec::new();
        let err = resolve_all(
            &mut grid,
            &mut bodies,
            &[ContactBody::default(); 2],
            &mut vel,
            &ContactConfig::default(),
            &mut rams,
        )
        .expect_err("one velocity for two bodies");
        assert_eq!(err.code(), "contact_length_mismatch");
        assert_eq!(bodies, before);
    }

    #[test]
    fn a_separating_pair_gets_no_impulse() {
        let mut bodies = pair(1.0);
        let mut vel = vec![[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let props = [ContactBody::default(); 2];
        let c = crate::broadphase::contact(&bodies, 0, 1, FloorRule::Bands).expect("overlap");
        assert_eq!(
            resolve(&mut bodies, &props, &mut vel, c, &ContactConfig::default()),
            None
        );
        assert_eq!(vel, vec![[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]]);
        assert_eq!(bodies[0].pos[0], -0.5);
        assert_eq!(bodies[1].pos[0], 1.5);
    }

    #[test]
    fn with_mass_maps_bad_masses_to_static() {
        assert_eq!(ContactBody::with_mass(4.0).inv_mass, 0.25);
        assert!(!ContactBody::with_mass(0.0).is_dynamic());
        assert!(!ContactBody::with_mass(f32::NAN).is_dynamic());
        assert!(!ContactBody::with_mass(-2.0).is_dynamic());
    }
}
