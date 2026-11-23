// SPDX-License-Identifier: MPL-2.0
//! ORCA local collision avoidance (`docs/specs/22-tier2.md` § T2.2): Reciprocal Velocity
//! Obstacles in 2D (the XZ plane), so a crowd of agents steers around each other without any
//! central coordination.
//!
//! Provenance: van den Berg, Guy, Lin, Manocha, "Reciprocal n-Body Collision Avoidance",
//! ISRR 2011 — the ORCA half-plane construction and its two-variable linear program are
//! re-derived from the paper's description. Only the agent-agent case is used (no static
//! obstacle lines). `linearProgram3` is still needed: agent lines alone can have an empty
//! intersection with the max-speed disc (a dense crowd), and then the velocity that violates the
//! lines least is used instead of giving up on every line after the first infeasible one (audit
//! v033b). Agents that already overlap resolve over one time step, as in the paper
//! ([`orca_velocity_with_step`]).

use regia_core::math::{Vec2, Vec3};

#[cfg(test)]
thread_local! {
    /// ORCA half-planes actually inserted on this thread since `reset_lines_built`. A
    /// neighbour that produces no line (non-finite, or exactly co-located) is not counted. The
    /// agent step test uses this to prove the neighbour cap; per thread so tests running in
    /// parallel never count each other's agents (ADR-5517).
    static LINES_BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Zeroes `lines_built` on this thread.
#[cfg(test)]
pub(crate) fn reset_lines_built() {
    LINES_BUILT.with(|c| c.set(0));
}

/// Half-planes built on this thread since the last `reset_lines_built`.
#[cfg(test)]
pub(crate) fn lines_built() -> u64 {
    LINES_BUILT.with(std::cell::Cell::get)
}

/// Reused half-plane buffers for one ORCA solve. The agent step keeps one in a `Local` so a step
/// does not allocate a line list per agent.
#[derive(Default)]
pub(crate) struct OrcaScratch {
    lines: Vec<Line>,
    projected: Vec<Line>,
}

/// A half-plane constraint `(v - point) · direction ≥ 0` on the agent's new velocity.
#[derive(Debug, Clone, Copy)]
struct Line {
    /// A point on the boundary line.
    point: Vec2,
    /// The outward normal of the half-plane.
    direction: Vec2,
}

/// The 2D determinant `det(a, b) = a.x*b.y - a.y*b.x` (positive = b is counter-clockwise of a).
fn det(a: Vec2, b: Vec2) -> f32 {
    a.perp_dot(b)
}

/// Computes the ORCA velocity for one agent given its neighbours (positions, velocities, radii).
///
/// `position`/`velocity`/`preferred` are world-space (the XZ components are used); the result is
/// the closest velocity to `preferred` (bounded by `max_speed`) that satisfies every neighbour's
/// ORCA half-plane.
///
/// Agents that already overlap are pushed apart over `time_horizon`; the crowd system uses
/// [`orca_velocity_with_step`], which resolves an overlap over one simulation step like the paper.
pub fn orca_velocity(
    position: Vec3,
    velocity: Vec3,
    preferred: Vec3,
    radius: f32,
    max_speed: f32,
    time_horizon: f32,
    neighbors: &[(Vec3, Vec3, f32)],
) -> Vec3 {
    orca_velocity_with_step(
        position,
        velocity,
        preferred,
        radius,
        max_speed,
        time_horizon,
        time_horizon,
        neighbors,
    )
}

/// [`orca_velocity`] with the simulation step `time_step` (seconds): a neighbour that already
/// overlaps this agent gives a half-plane that separates the two within one step (the paper's
/// colliding case), rather than over the whole `time_horizon` (audit v033b).
#[allow(clippy::too_many_arguments)]
pub fn orca_velocity_with_step(
    position: Vec3,
    velocity: Vec3,
    preferred: Vec3,
    radius: f32,
    max_speed: f32,
    time_horizon: f32,
    time_step: f32,
    neighbors: &[(Vec3, Vec3, f32)],
) -> Vec3 {
    let mut scratch = OrcaScratch::default();
    orca_velocity_with_scratch(
        &mut scratch,
        position,
        velocity,
        preferred,
        radius,
        max_speed,
        time_horizon,
        time_step,
        neighbors,
    )
}

/// [`orca_velocity_with_step`] writing its half-planes into `scratch` (cleared, then reused).
#[allow(clippy::too_many_arguments)]
pub(crate) fn orca_velocity_with_scratch(
    scratch: &mut OrcaScratch,
    position: Vec3,
    velocity: Vec3,
    preferred: Vec3,
    radius: f32,
    max_speed: f32,
    time_horizon: f32,
    time_step: f32,
    neighbors: &[(Vec3, Vec3, f32)],
) -> Vec3 {
    // A non-finite agent state or limit has no meaningful answer: stand still rather than
    // spread NaN into the agent's transform.
    if !(position.is_finite()
        && velocity.is_finite()
        && preferred.is_finite()
        && radius.is_finite()
        && max_speed.is_finite()
        && time_horizon.is_finite()
        && time_step.is_finite())
    {
        return Vec3::ZERO;
    }
    let max_speed = max_speed.max(0.0);
    // A negative radius shrinks or flips the combined radius, and agents steered into each
    // other (audit-v035-numerics L1).
    let radius = radius.max(0.0);
    let position2 = Vec2::new(position.x, position.z);
    let velocity2 = Vec2::new(velocity.x, velocity.z);
    let preferred2 = Vec2::new(preferred.x, preferred.z);
    let inv_time_horizon = 1.0 / time_horizon.max(1e-3);
    let inv_time_step = 1.0 / time_step.max(1e-4);

    scratch.lines.clear();
    for &(npos, nvel, nradius) in neighbors {
        if !(npos.is_finite() && nvel.is_finite() && nradius.is_finite()) {
            continue;
        }
        let Some(line) = orca_line(
            position2,
            velocity2,
            npos,
            nvel,
            radius,
            nradius.max(0.0),
            inv_time_horizon,
            inv_time_step,
        ) else {
            continue;
        };
        // Exactly co-located, motionless agents give a 0/0 direction: no usable constraint.
        if !(line.point.is_finite() && line.direction.is_finite()) {
            continue;
        }
        scratch.lines.push(line);
        #[cfg(test)]
        LINES_BUILT.with(|c| c.set(c.get() + 1));
    }

    let mut result = Vec2::ZERO;
    let failed = linear_program2(&scratch.lines, max_speed, preferred2, false, &mut result);
    if failed < scratch.lines.len() {
        linear_program3(
            &scratch.lines,
            failed,
            max_speed,
            &mut result,
            &mut scratch.projected,
        );
    }
    Vec3::new(result.x, 0.0, result.y)
}

/// Builds the ORCA half-plane `Line` induced by one neighbour, or `None` when no constraint
/// applies (the neighbour is irrelevant this frame).
#[allow(clippy::too_many_arguments)]
fn orca_line(
    position: Vec2,
    velocity: Vec2,
    other_position: Vec3,
    other_velocity: Vec3,
    radius: f32,
    other_radius: f32,
    inv_time_horizon: f32,
    inv_time_step: f32,
) -> Option<Line> {
    let other_pos = Vec2::new(other_position.x, other_position.z);
    let other_vel = Vec2::new(other_velocity.x, other_velocity.z);

    let relative_position = other_pos - position;
    let relative_velocity = velocity - other_vel;
    let combined_radius = radius + other_radius;
    let dist_sq = relative_position.length_squared();
    let combined_sq = combined_radius * combined_radius;

    let mut line = Line {
        point: Vec2::ZERO,
        direction: Vec2::ZERO,
    };

    let u = if dist_sq > combined_sq {
        // Not colliding: project onto the velocity-obstacle cone (or its truncated legs).
        let w = relative_velocity - relative_position * inv_time_horizon;
        let w_len_sq = w.length_squared();
        let dot = w.dot(relative_position);

        if dot < 0.0 && dot * dot > combined_sq * w_len_sq {
            // Project onto the cut-off circle.
            let w_len = w_len_sq.sqrt();
            let unit_w = w / w_len;
            line.direction = Vec2::new(unit_w.y, -unit_w.x);
            unit_w * (combined_radius * inv_time_horizon - w_len)
        } else {
            // Project onto one of the two legs.
            let leg = (dist_sq - combined_sq).sqrt();
            if det(relative_position, w) > 0.0 {
                // Left leg.
                line.direction = Vec2::new(
                    relative_position.x * leg - relative_position.y * combined_radius,
                    relative_position.x * combined_radius + relative_position.y * leg,
                ) / dist_sq;
            } else {
                // Right leg.
                line.direction = -Vec2::new(
                    relative_position.x * leg + relative_position.y * combined_radius,
                    -relative_position.x * combined_radius + relative_position.y * leg,
                ) / dist_sq;
            }
            let dot_product2 = relative_velocity.dot(line.direction);
            line.direction * dot_product2 - relative_velocity
        }
    } else {
        // Currently colliding: the horizon shrinks to one time step, so the pair is asked to
        // separate within the next step (the paper's colliding case).
        let w = relative_velocity - relative_position * inv_time_step;
        let w_len = w.length();
        if w_len < 1e-6 {
            return None; // exactly on top of each other; no well-defined direction
        }
        let unit_w = w / w_len;
        line.direction = Vec2::new(unit_w.y, -unit_w.x);
        unit_w * (combined_radius * inv_time_step - w_len)
    };

    line.point = velocity + u * 0.5;
    Some(line)
}

/// Finds the closest point to `opt_velocity` inside the half-planes `lines[0..line_no]`, bounded
/// by `radius`, using `line_no` as the last constraint (the paper's `linearProgram1`).
fn linear_program1(
    lines: &[Line],
    line_no: usize,
    radius: f32,
    opt_velocity: Vec2,
    direction_opt: bool,
    result: &mut Vec2,
) -> bool {
    let line = &lines[line_no];
    let dot_product = line.point.dot(line.direction);
    let discriminant = dot_product * dot_product + radius * radius - line.point.length_squared();
    if discriminant < 0.0 {
        return false;
    }
    let sqrt_disc = discriminant.sqrt();
    let mut t_left = -dot_product - sqrt_disc;
    let mut t_right = -dot_product + sqrt_disc;

    for other in &lines[..line_no] {
        let denominator = det(line.direction, other.direction);
        let numerator = det(other.direction, line.point - other.point);
        if denominator.abs() <= 1e-5 {
            // Parallel: the constraint must already hold, otherwise the region is empty.
            if numerator < 0.0 {
                return false;
            }
            continue;
        }
        let t = numerator / denominator;
        if denominator >= 0.0 {
            t_right = t_right.min(t);
        } else {
            t_left = t_left.max(t);
        }
        if t_left > t_right {
            return false;
        }
    }

    if direction_opt {
        if opt_velocity.dot(line.direction) > 0.0 {
            *result = line.point + line.direction * t_right;
        } else {
            *result = line.point + line.direction * t_left;
        }
    } else {
        let t = line.direction.dot(opt_velocity - line.point);
        *result = if t < t_left {
            line.point + line.direction * t_left
        } else if t > t_right {
            line.point + line.direction * t_right
        } else {
            line.point + line.direction * t
        };
    }
    true
}

/// The paper's `linearProgram2`: finds the velocity closest to `opt_velocity` (clamped to
/// `radius`) inside all `lines`. Returns the index of the first line it could not satisfy
/// (`lines.len()` on success); `result` then holds the best velocity for the lines before it.
fn linear_program2(
    lines: &[Line],
    radius: f32,
    opt_velocity: Vec2,
    direction_opt: bool,
    result: &mut Vec2,
) -> usize {
    if direction_opt {
        *result = opt_velocity * radius;
    } else if opt_velocity.length_squared() > radius * radius {
        *result = opt_velocity.normalize_or_zero() * radius;
    } else {
        *result = opt_velocity;
    }

    for i in 0..lines.len() {
        if det(lines[i].direction, lines[i].point - *result) > 0.0 {
            let temp = *result;
            if !linear_program1(lines, i, radius, opt_velocity, direction_opt, result) {
                *result = temp;
                return i;
            }
        }
    }
    lines.len()
}

/// The paper's `linearProgram3`: the lines from `begin` on have no common point inside the
/// speed disc, so find the velocity that minimizes the largest violation of any of them. Line by
/// line, a violated line's neighbours are projected onto it and a direction-optimized
/// `linearProgram2` moves the result as far into it as the earlier lines allow.
fn linear_program3(lines: &[Line], begin: usize, radius: f32, result: &mut Vec2, projected: &mut Vec<Line>) {
    let mut distance = 0.0_f32;
    for i in begin..lines.len() {
        if det(lines[i].direction, lines[i].point - *result) <= distance {
            continue;
        }
        // The earlier lines, each turned into a half-plane on line i's side of equal violation.
        projected.clear();
        for j in 0..i {
            let determinant = det(lines[i].direction, lines[j].direction);
            let point = if determinant.abs() <= 1e-5 {
                if lines[i].direction.dot(lines[j].direction) > 0.0 {
                    // Same direction: line j adds nothing beyond line i.
                    continue;
                }
                // Opposite directions: the point halfway between them.
                (lines[i].point + lines[j].point) * 0.5
            } else {
                lines[i].point
                    + lines[i].direction
                        * (det(lines[j].direction, lines[i].point - lines[j].point) / determinant)
            };
            let direction = (lines[j].direction - lines[i].direction).normalize_or_zero();
            projected.push(Line { point, direction });
        }
        let previous = *result;
        // Optimize straight into line i's half-plane.
        let into = Vec2::new(-lines[i].direction.y, lines[i].direction.x);
        if linear_program2(projected, radius, into, true, result) < projected.len() {
            // Cannot fail in exact arithmetic; with rounding keep the previous result.
            *result = previous;
        }
        distance = det(lines[i].direction, lines[i].point - *result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest violation of `lines` by velocity `v` (0 when every half-plane holds).
    fn worst_violation(lines: &[Line], v: Vec2) -> f32 {
        lines
            .iter()
            .map(|line| det(line.direction, line.point - v))
            .fold(0.0, f32::max)
    }

    /// Audit v033b (Med): when the agent lines had no common velocity inside the speed disc,
    /// `linearProgram2` gave up at the first infeasible line and returned a velocity that broke it
    /// (and every later line was never looked at). `linearProgram3` returns the velocity whose
    /// worst violation is smallest: no velocity in the disc does clearly better.
    #[test]
    fn an_infeasible_crowd_gets_the_least_violating_velocity() {
        let (radius, max_speed, horizon) = (0.5, 1.0, 2.0);
        let neighbors = [
            (Vec3::new(1.2, 0.0, 0.0), Vec3::new(-3.0, 0.0, 0.0), 0.5),
            (Vec3::new(-1.2, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0), 0.5),
            (Vec3::new(0.0, 0.0, 1.2), Vec3::new(0.0, 0.0, -3.0), 0.5),
            (Vec3::new(0.0, 0.0, -1.2), Vec3::new(0.0, 0.0, 3.0), 0.5),
        ];
        let preferred = Vec3::new(1.0, 0.0, 0.0);
        let v = orca_velocity(
            Vec3::ZERO,
            Vec3::ZERO,
            preferred,
            radius,
            max_speed,
            horizon,
            &neighbors,
        );
        let lines: Vec<Line> = neighbors
            .iter()
            .filter_map(|&(p, nv, r)| {
                orca_line(
                    Vec2::ZERO,
                    Vec2::ZERO,
                    p,
                    nv,
                    radius,
                    r,
                    1.0 / horizon,
                    1.0 / horizon,
                )
            })
            .collect();
        let got = worst_violation(&lines, Vec2::new(v.x, v.z));
        // Brute force over the disc.
        let mut best = f32::MAX;
        for ix in -40..=40 {
            for iz in -40..=40 {
                let c = Vec2::new(ix as f32, iz as f32) * (max_speed / 40.0);
                if c.length() <= max_speed {
                    best = best.min(worst_violation(&lines, c));
                }
            }
        }
        assert!(best > 0.0, "the scene must be infeasible: {best}");
        assert!(Vec2::new(v.x, v.z).length() <= max_speed + 1e-4, "{v:?}");
        assert!(
            got <= best + 0.05,
            "worst violation {got}, the disc allows {best}: {v:?}"
        );
    }

    /// Audit v033b (Low): an overlapping pair was asked to separate over the whole time horizon
    /// (about 0.17 m/s for two default agents 0.2 m apart). With the step, the overlap is resolved
    /// as fast as the speed limit allows, as in the paper's colliding case.
    #[test]
    fn overlapping_agents_separate_within_the_step() {
        let neighbor = [(Vec3::new(0.2, 0.0, 0.0), Vec3::ZERO, 0.35)];
        let slow = orca_velocity(Vec3::ZERO, Vec3::ZERO, Vec3::ZERO, 0.35, 3.0, 1.5, &neighbor);
        assert!(slow.x < 0.0 && slow.x > -0.5, "over the horizon: {slow:?}");
        let fast = orca_velocity_with_step(
            Vec3::ZERO,
            Vec3::ZERO,
            Vec3::ZERO,
            0.35,
            3.0,
            1.5,
            1.0 / 60.0,
            &neighbor,
        );
        assert!(fast.x < -2.9, "away at the speed limit: {fast:?}");
        assert!(fast.z.abs() < 1e-3, "{fast:?}");
    }

    #[test]
    fn a_lone_agent_moves_at_its_preferred_velocity() {
        let v = orca_velocity(
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::ZERO,
            Vec3::new(2.0, 0.0, 0.0),
            0.5,
            3.0,
            1.0,
            &[],
        );
        assert!(
            (v.x - 2.0).abs() < 1e-3 && v.z.abs() < 1e-3,
            "preferred kept: {v:?}"
        );
    }

    #[test]
    fn head_on_agents_deflect() {
        // A at origin moving +X, B at x=2 moving -X; they must not both keep going straight.
        let va = orca_velocity(
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            0.5,
            3.0,
            1.0,
            &[(Vec3::new(2.0, 0.0, 0.0), Vec3::new(-1.0, 0.0, 0.0), 0.5)],
        );
        assert!(va.z.abs() > 0.05, "A must gain a lateral component: {va:?}");
        // B's symmetric view of the same encounter.
        let vb = orca_velocity(
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(-1.0, 0.0, 0.0),
            0.5,
            3.0,
            1.0,
            &[(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0), 0.5)],
        );
        assert!(va.length() <= 3.0 + 1e-3 && vb.length() <= 3.0 + 1e-3);
        // Both agents following their returned velocities keep the radii apart over the horizon.
        let pa = Vec3::new(0.0, 0.0, 0.0);
        let pb = Vec3::new(2.0, 0.0, 0.0);
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let gap = ((pb + vb * t) - (pa + va * t)).length();
            assert!(
                gap >= 1.0 - 1e-2,
                "agents overlap at t={t}: gap {gap}, va={va:?} vb={vb:?}"
            );
        }
    }

    #[test]
    fn max_speed_is_respected() {
        let v = orca_velocity(
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::ZERO,
            Vec3::new(100.0, 0.0, 0.0),
            0.5,
            3.0,
            1.0,
            &[],
        );
        assert!(
            (v.x - 3.0).abs() < 1e-3 && v.z.abs() < 1e-3,
            "clamped along the preferred direction: {v:?}"
        );
    }

    #[test]
    fn co_located_agents_do_not_nan() {
        let v = orca_velocity(
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::ZERO,
            Vec3::new(1.0, 0.0, 0.0),
            0.5,
            3.0,
            1.0,
            &[(Vec3::new(0.0, 0.0, 0.0), Vec3::ZERO, 0.5)],
        );
        assert!(v.x.is_finite() && v.z.is_finite(), "{v:?}");
    }
}

/// audit-v035-numerics L1 (fix-audit-wave12, ADR-2395).
#[cfg(test)]
mod audit_v035_tests {
    use super::*;

    /// A negative radius (own or a neighbour's) shrank or flipped the combined radius; it now
    /// avoids exactly like a zero radius.
    #[test]
    fn a_negative_radius_avoids_like_a_zero_radius() {
        let neighbors = |r: f32| [(Vec3::new(1.0, 0.0, 0.2), Vec3::new(-1.0, 0.0, 0.0), r)];
        let preferred = Vec3::new(2.0, 0.0, 0.0);
        let solve = |own: f32, other: f32| {
            orca_velocity(
                Vec3::ZERO,
                Vec3::ZERO,
                preferred,
                own,
                2.0,
                2.0,
                &neighbors(other),
            )
        };
        assert_eq!(solve(-1.0, 0.35), solve(0.0, 0.35), "own radius");
        assert_eq!(solve(0.35, -1.0), solve(0.35, 0.0), "neighbour radius");
    }
}
