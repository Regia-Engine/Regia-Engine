// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pilot_collide.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 774-930 (`slide_move`, `resolve_feet`) and 216 (`accept_support`), mark: requirements (Stage B, ADR-8323).
// Edits: the rules are re-expressed over `push_disc`, `clamp_capsule` and `support`; no source arithmetic is kept.
//! The character mover (spec 121 § 5.4; lane W-30, ADR-8323): a deterministic disc mover for hulls
//! and a capsule mover for walkers, in height bands, over the native queries.
//!
//! The host's integrator owns speed, gravity and input; the mover turns one tick's horizontal
//! displacement into the body's new position:
//!
//! - [`move_disc`]: one [`WorldGeom::push_disc`] over the hull's band and the floor
//!   [`WorldGeom::support`] gives at the hull's reach (T3's hull rule; the caller's band).
//! - [`move_capsule`]: the walker. The step is cut into substeps no longer than
//!   [`CapsuleParams::substep_m`]. Each substep pushes the disc out of the walls over one step
//!   (walls under the step are stepped, as the on-foot grid steps a curb), stops at a floor that
//!   stands in the body band above one step anywhere inside the radius (a drawn rock, a ledge), takes
//!   the floor `support` answers within one step up, sticks down at most
//!   [`CapsuleParams::step_down_m`] (otherwise the body is airborne and keeps its height for the
//!   host's fall), then runs the capsule's vertical rule ([`WorldGeom::clamp_capsule`]) and stops when
//!   the head does not fit under a roof.
//!
//! Every query is a pure function of its arguments and the state; the mover keeps no history.
//! Arithmetic is IEEE `f32` in source order, `regia_detsim::math::sqrt` and `det_*` extrema
//! (spec 121 § 2, § 14); nothing allocates.

use crate::WorldGeom;
use crate::query::{Band, GeomState, SpanBuf, Support};
use regia_detsim::float::det_max;
use regia_detsim::math;

/// Most substeps one [`move_capsule`] call runs; a longer step is cut into this many.
pub const MAX_SUBSTEPS: i32 = 64;

/// Slack over one step that still counts as a step (the on-foot rule's 0.02 m).
pub const STEP_SLACK_M: f32 = 0.02;

/// A walker capsule's sizes and step rules. There is no default: the host passes its values (DO27's
/// human-scale caps are settings, ADR-8012 A-1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapsuleParams {
    /// Capsule radius, metres.
    pub radius_m: f32,
    /// Feet to the top of the head, metres.
    pub height_m: f32,
    /// Highest floor the feet step up onto in one move, metres.
    pub step_up_m: f32,
    /// Lowest floor the feet stick down to in one move, metres; a larger drop leaves the body airborne.
    pub step_down_m: f32,
    /// Longest substep, metres (the on-foot grid's sweep samples every 0.2 m).
    pub substep_m: f32,
}

/// A hull disc's sizes and band rule (T3's hull band: `floor + band_lo_m` to
/// `max(y, floor) + band_hi_m`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiscParams {
    /// Disc radius, metres.
    pub radius_m: f32,
    /// Height over the origin of the point `support` reads (DO27: the waist, 0.9 m).
    pub reach_m: f32,
    /// Band bottom over the floor, metres (DO27: 0.5).
    pub band_lo_m: f32,
    /// Band top over the higher of the origin and the floor, metres (DO27: 2.0).
    pub band_hi_m: f32,
}

/// Why a capsule move stopped short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveStop {
    /// It did not: the whole step was taken (walls may still have slid it).
    None,
    /// A floor in the body band above one step inside the radius (a ledge or a drawn rock).
    Ledge,
    /// The floor under the axis is above one step (the axis would be inside rock).
    Rock,
    /// The head does not fit under a roof.
    Roof,
}

/// One capsule move's answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapsuleMove {
    /// The new feet position (the capsule's lowest point).
    pub feet: [f32; 3],
    /// The capsule's band after the vertical rule (`lo` is `feet[1]`).
    pub band: Band,
    /// The span `support` answers at the new axis, one step over the feet.
    pub support: Support,
    /// Wall contacts summed over the substeps.
    pub contacts: u16,
    /// The feet are over a drop larger than the stick-down limit, or the move did not stick.
    pub airborne: bool,
    /// Why the move stopped short.
    pub stop: MoveStop,
}

/// One disc move's answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiscMove {
    /// The pushed centre XZ.
    pub xz: [f32; 2],
    /// The span `support` answers at the pushed centre and the origin's reach.
    pub support: Support,
    /// The band the push used.
    pub band: Band,
    /// Walls that moved the centre.
    pub contacts: u16,
}

/// The hull rule: the floor under the origin at its reach, the band from it, one disc push to the
/// target, and the floor again at the pushed centre. A non-finite input returns the origin unmoved.
#[must_use]
pub fn move_disc(
    geom: &WorldGeom,
    state: &GeomState,
    params: &DiscParams,
    origin: [f32; 3],
    delta_xz: [f32; 2],
) -> DiscMove {
    let [x, y, z] = origin;
    let start = geom.support(state, x, z, y + params.reach_m);
    let floor = start.floor.y;
    let band = Band {
        lo: floor + params.band_lo_m,
        hi: det_max(y, floor) + params.band_hi_m,
    };
    if ![x, y, z, delta_xz[0], delta_xz[1]].iter().all(|v| v.is_finite()) {
        return DiscMove {
            xz: [x, z],
            support: start,
            band,
            contacts: 0,
        };
    }
    let push = geom.push_disc(state, x + delta_xz[0], z + delta_xz[1], params.radius_m, band);
    DiscMove {
        xz: push.xz,
        support: geom.support(state, push.xz[0], push.xz[1], y + params.reach_m),
        band,
        contacts: push.contacts,
    }
}

/// The capsule's rim samples, as multiples of the radius: eight points counter-clockwise from +X at
/// 0.9 of the radius (inside the pushed disc, so a sample never lands on the wall line itself).
const RIM: [[f32; 2]; 8] = [
    [0.9, 0.0],
    [0.636_396_1, 0.636_396_1],
    [0.0, 0.9],
    [-0.636_396_1, 0.636_396_1],
    [-0.9, 0.0],
    [-0.636_396_1, -0.636_396_1],
    [0.0, -0.9],
    [0.636_396_1, -0.636_396_1],
];

/// The body-band test at the axis and the rim: a floor in `(step, head)` is a ledge, a roof in
/// `(feet, head)` is a roof the body does not fit under. Samples run in a fixed order, the axis first.
#[expect(
    clippy::too_many_arguments,
    reason = "the band and the sample point, all scalars"
)]
fn body_band_hit(
    geom: &WorldGeom,
    state: &GeomState,
    x: f32,
    z: f32,
    r: f32,
    feet: f32,
    step: f32,
    head: f32,
) -> Option<MoveStop> {
    let mut spans = SpanBuf::default();
    let mut at = |sx: f32, sz: f32| {
        geom.floors_at(state, sx, sz, &mut spans);
        for s in spans.spans() {
            if s.floor.y > step && s.floor.y < head {
                return Some(MoveStop::Ledge);
            }
            if s.ceiling.is_some_and(|c| c.y > feet && c.y < head) {
                return Some(MoveStop::Roof);
            }
        }
        None
    };
    if let Some(stop) = at(x, z) {
        return Some(stop);
    }
    RIM.iter().find_map(|[ox, oz]| at(x + ox * r, z + oz * r))
}

/// Moves a walker capsule with its feet at `feet` by `delta_xz` (see the module docs for the rule).
/// `stick` is false while the host has the body in the air (a jump): the feet keep their height and
/// only walls stop the move. A non-finite input returns the feet unmoved.
#[must_use]
pub fn move_capsule(
    geom: &WorldGeom,
    state: &GeomState,
    params: &CapsuleParams,
    feet: [f32; 3],
    delta_xz: [f32; 2],
    stick: bool,
) -> CapsuleMove {
    let [mut x, mut y, mut z] = feet;
    let r = params.radius_m;
    let reach = params.step_up_m + STEP_SLACK_M;
    let mut out = CapsuleMove {
        feet,
        band: Band {
            lo: y,
            hi: y + params.height_m,
        },
        support: geom.support(state, x, z, y + reach),
        contacts: 0,
        airborne: false,
        stop: MoveStop::None,
    };
    let finite = [
        x,
        y,
        z,
        delta_xz[0],
        delta_xz[1],
        r,
        params.height_m,
        params.substep_m,
    ]
    .iter()
    .all(|v| v.is_finite());
    if !finite || r <= 0.0 || params.substep_m <= 0.0 {
        return out;
    }
    let [dx, dz] = delta_xz;
    let len = math::sqrt(dx * dx + dz * dz);
    // (Two lines: the integer `max` on its own line, so the det-scan float hint does not read it as `f32::max`.)
    let steps = (len / params.substep_m).ceil() as i32;
    let steps = steps.clamp(1, MAX_SUBSTEPS);
    let (sx, sz) = (dx / steps as f32, dz / steps as f32);
    for _ in 0..steps {
        let push = geom.push_disc(
            state,
            x + sx,
            z + sz,
            r,
            Band {
                lo: y + params.step_up_m,
                hi: y + params.height_m,
            },
        );
        let [nx, nz] = push.xz;
        if let Some(stop) = body_band_hit(geom, state, nx, nz, r, y, y + reach, y + params.height_m) {
            out.stop = stop;
            break;
        }
        let support = geom.support(state, nx, nz, y + reach);
        let floor = support.floor.y;
        if floor > y + reach {
            out.stop = MoveStop::Rock;
            break;
        }
        let mut ny = y;
        let mut airborne = !stick;
        if stick {
            if floor >= y - params.step_down_m {
                ny = floor;
            } else {
                airborne = true;
            }
        }
        let band = geom.clamp_capsule(
            state,
            nx,
            nz,
            r,
            Band {
                lo: ny,
                hi: ny + params.height_m,
            },
        );
        if band.hi < ny + params.height_m {
            out.stop = MoveStop::Roof;
            break;
        }
        if stick && !airborne {
            ny = band.lo;
        }
        x = nx;
        z = nz;
        y = ny;
        out.contacts = out.contacts.saturating_add(push.contacts);
        out.airborne = airborne;
        out.support = support;
        out.band = Band {
            lo: ny,
            hi: ny + params.height_m,
        };
    }
    out.feet = [x, y, z];
    out
}

/// A drawn triangle whose centroid lies this close to a canonical surface is the canonical shell
/// (the tunnel lining the art draws over the sim triangles), not dressing, metres (the on-foot
/// grid's own drawn-floor match, `compat::onfoot::FLOOR_MATCH_M`).
pub const SHELL_MATCH_M: f32 = 0.30;

/// True when a drawn triangle (world space) is the canonical shell: its centroid is within
/// [`SHELL_MATCH_M`] of a floor, the ground or a roof in the column there, or of a wall (a disc of
/// that radius at the centroid's height touches one). Load time only.
#[must_use]
pub fn is_canonical_shell(
    geom: &WorldGeom,
    state: &GeomState,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
) -> bool {
    let g = [
        (a[0] + b[0] + c[0]) / 3.0,
        (a[1] + b[1] + c[1]) / 3.0,
        (a[2] + b[2] + c[2]) / 3.0,
    ];
    if !g.iter().all(|v| v.is_finite()) {
        return false;
    }
    let near = |y: f32| (y - g[1]).abs() <= SHELL_MATCH_M;
    let mut spans = SpanBuf::default();
    geom.floors_at(state, g[0], g[2], &mut spans);
    if spans
        .spans()
        .iter()
        .any(|s| near(s.floor.y) || s.ceiling.is_some_and(|lid| near(lid.y)))
    {
        return true;
    }
    let band = Band {
        lo: g[1] - 0.05,
        hi: g[1] + 0.05,
    };
    geom.push_disc(state, g[0], g[2], SHELL_MATCH_M, band).contacts > 0
}

/// The Stage B dressing grid (plan row W-30): the on-foot grid of one world-space glTF set piece,
/// built by the Stage A loader with every canonical-shell triangle ([`is_canonical_shell`]) dropped,
/// so tunnel shells never reach it. The grid keeps loose dressing only; a walker's floors and walls
/// in the canonical geometry come from [`move_capsule`]. Load time only.
///
/// # Errors
///
/// The loader's message for an unreadable or malformed glTF.
pub fn dressing_grid(
    params: &crate::compat::onfoot::OnFootParams,
    source: &dyn crate::compat::onfoot::mesh::GltfGridSource,
    terrain: Option<&crate::compat::sampler::TerrainSampler>,
    geom: &WorldGeom,
    state: &GeomState,
) -> Result<crate::compat::onfoot::PilotGrid, String> {
    let keep = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| !is_canonical_shell(geom, state, a, b, c);
    crate::compat::onfoot::mesh::load_gltf_grid_keep::<()>(params, source, true, terrain, None, &keep)
}

/// A body's band and support at its feet, for tools and the agent surface (plan row W-30: "an agent
/// read of a body's band and support"): what the mover would answer for a capsule standing still.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyProbe {
    /// The span `support` answers one step over the feet.
    pub support: Support,
    /// The capsule's band after the vertical rule ([`WorldGeom::clamp_capsule`]).
    pub band: Band,
    /// A lid covers the body (the support span has a ceiling over the feet).
    pub covered: bool,
    /// The capsule fits: its head was not lowered and no floor or lid stands in its body band.
    pub fits: bool,
}

/// Reads a standing capsule's band and support at `feet` (no move, no history). A non-finite input
/// gives the band at the feet and the map-edge ground, `fits == false`.
#[must_use]
pub fn body_probe(geom: &WorldGeom, state: &GeomState, params: &CapsuleParams, feet: [f32; 3]) -> BodyProbe {
    let [x, y, z] = feet;
    let reach = params.step_up_m + STEP_SLACK_M;
    let support = geom.support(state, x, z, y + reach);
    let raw = Band {
        lo: y,
        hi: y + params.height_m,
    };
    if !feet.iter().all(|v| v.is_finite()) || !params.radius_m.is_finite() || params.radius_m <= 0.0 {
        return BodyProbe {
            support,
            band: raw,
            covered: false,
            fits: false,
        };
    }
    let band = geom.clamp_capsule(state, x, z, params.radius_m, raw);
    let blocked = body_band_hit(
        geom,
        state,
        x,
        z,
        params.radius_m,
        y,
        y + reach,
        y + params.height_m,
    );
    BodyProbe {
        support,
        band,
        covered: support.ceiling.is_some_and(|c| c.y > y),
        fits: band.hi >= raw.hi && blocked.is_none(),
    }
}
