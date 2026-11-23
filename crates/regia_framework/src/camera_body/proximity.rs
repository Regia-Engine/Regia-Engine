// SPDX-License-Identifier: MPL-2.0
//! "What is within R of the camera path?" (ADR-1502).
//!
//! [`proximity`] samples a [`PathSampler`] at a fixed step over a time window. For every target
//! that comes within `radius` metres of the body's surface, it reports the closest approach
//! (time, gap, closing speed) and when the target enters and leaves the radius. The closest
//! approach is refined by a fixed number of golden-section steps and the entry and exit by
//! bisection, so the answer does not depend on the sampling step beyond its resolution. Targets
//! are culled against the bounding box of each 64-sample chunk of the path, inflated by the
//! radius, so a long path against thousands of scatter instances stays cheap.

use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use super::PathSampler;

/// Samples per culling chunk.
const CHUNK: usize = 64;
/// Golden-section refinement steps of the closest approach.
const GOLDEN_STEPS: u32 = 40;
/// Bisection steps of the entry and exit times.
const BISECT_STEPS: u32 = 30;
/// Most path samples one query takes.
pub const MAX_PROXIMITY_SAMPLES: usize = 1_000_000;

/// Something the path may pass near.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProximityTarget {
    /// The caller's name for it (an entity, a scatter instance, a bird).
    pub id: String,
    /// Centre, metres.
    pub position: [f32; 3],
    /// Radius of the target itself, metres (the gap is measured to its surface).
    #[serde(default)]
    pub radius_m: f32,
}

/// The query: a window of the path and a radius.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProximityQuery {
    /// Report targets whose surface comes within this many metres of the body's surface.
    pub radius_m: f32,
    /// Start of the window, seconds (clamped to the path).
    pub from_s: f64,
    /// End of the window, seconds (clamped to the path; infinite = the end).
    pub to_s: f64,
    /// Sampling step, seconds.
    pub step_s: f64,
    /// The body's own radius, metres.
    pub body_radius_m: f32,
    /// Velocity difference step, seconds.
    pub velocity_step_s: f64,
}

impl Default for ProximityQuery {
    fn default() -> Self {
        Self {
            radius_m: 10.0,
            from_s: 0.0,
            to_s: f64::INFINITY,
            step_s: 1.0 / 120.0,
            body_radius_m: 0.0,
            velocity_step_s: super::DEFAULT_VELOCITY_STEP_S,
        }
    }
}

/// One target the path passes within the radius of.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProximityHit {
    /// The target's id.
    pub id: String,
    /// Time of the closest approach, seconds.
    pub closest_s: f64,
    /// Gap between the body's and the target's surfaces at the closest approach, metres
    /// (negative: they overlap).
    pub gap_m: f32,
    /// Distance between the body's centre and the target's centre then, metres.
    pub distance_m: f32,
    /// The body's position then.
    pub body_position: [f32; 3],
    /// The body's speed then, m/s.
    pub speed_m_s: f32,
    /// When the gap first falls to the radius, seconds.
    pub enter_s: f64,
    /// When the gap rises past the radius again, seconds (the window's end if it never does).
    pub exit_s: f64,
    /// Closing speed (rate the centres approach, m/s) as the target enters the radius.
    pub closing_at_enter_m_s: f32,
}

/// Rate at which the body at `pos` moving at `vel` approaches `target`, m/s (negative: moving
/// away).
#[must_use]
pub fn closing_speed(pos: Vec3, vel: Vec3, target: Vec3) -> f32 {
    let to = target - pos;
    let d = to.length();
    if d <= 1.0e-6 { 0.0 } else { vel.dot(to / d) }
}

fn window(path: &dyn PathSampler, query: &ProximityQuery) -> Option<(f64, f64, f64, usize)> {
    let (start, end) = path.span();
    let from = query.from_s.max(start);
    let to = if query.to_s.is_finite() {
        query.to_s.min(end)
    } else {
        end
    };
    if from.is_nan() || to.is_nan() || to < from || !query.step_s.is_finite() || query.step_s <= 0.0 {
        return None;
    }
    let n = (((to - from) / query.step_s).ceil() as usize).min(MAX_PROXIMITY_SAMPLES);
    let step = if n == 0 { 0.0 } else { (to - from) / n as f64 };
    Some((from, to, step, n + 1))
}

/// Every target within `query.radius_m` of the path over the window, by closest approach time
/// (ties by id). Pure.
#[must_use]
pub fn proximity(
    path: &dyn PathSampler,
    targets: &[ProximityTarget],
    query: &ProximityQuery,
) -> Vec<ProximityHit> {
    let Some((from, to, step, count)) = window(path, query) else {
        return Vec::new();
    };
    let time_of = |i: usize| {
        if i + 1 == count {
            to
        } else {
            from + step * i as f64
        }
    };
    let samples: Vec<(f64, Vec3)> = (0..count)
        .filter_map(|i| {
            let t = time_of(i);
            path.position(t).map(|p| (t, p))
        })
        .collect();
    if samples.is_empty() {
        return Vec::new();
    }
    let reach = query.radius_m.max(0.0) + query.body_radius_m.max(0.0);
    let chunks: Vec<(usize, usize, Vec3, Vec3)> = samples
        .chunks(CHUNK)
        .enumerate()
        .map(|(c, chunk)| {
            // Neighbouring chunks share their end sample so a pass between them is not missed.
            let first = c * CHUNK;
            let last = (first + chunk.len()).min(samples.len() - 1);
            let mut lo = Vec3::splat(f32::INFINITY);
            let mut hi = Vec3::splat(f32::NEG_INFINITY);
            for (_, p) in &samples[first..=last] {
                lo = lo.min(*p);
                hi = hi.max(*p);
            }
            (first, last, lo, hi)
        })
        .collect();
    let at = |t: f64| path.position(t).unwrap_or(Vec3::ZERO);
    let mut hits = Vec::new();
    for target in targets {
        let centre = Vec3::from_array(target.position);
        let limit = reach + target.radius_m.max(0.0);
        let gap = |p: Vec3| p.distance(centre) - query.body_radius_m - target.radius_m;
        let mut best: Option<(usize, f32)> = None;
        let mut enter: Option<usize> = None;
        let mut exit: Option<usize> = None;
        for &(first, last, lo, hi) in &chunks {
            let outside = (centre - hi.min(centre).max(lo)).length() > limit;
            if outside {
                // Nothing in this chunk is within the radius; close a run that was open.
                if enter.is_some() && exit.is_none() {
                    exit = Some(first);
                }
                continue;
            }
            for (i, (_, p)) in samples.iter().enumerate().take(last + 1).skip(first) {
                let g = gap(*p);
                if best.is_none_or(|(_, b)| g < b) {
                    best = Some((i, g));
                }
                if g <= query.radius_m {
                    if enter.is_none() {
                        enter = Some(i);
                    }
                } else if enter.is_some() && exit.is_none() {
                    exit = Some(i);
                }
            }
        }
        let (Some((best_i, _)), Some(enter_i)) = (best, enter) else {
            continue;
        };
        // Golden-section refinement of the closest approach around the best sample.
        let (mut a, mut b) = (
            samples[best_i.saturating_sub(1)].0,
            samples[(best_i + 1).min(samples.len() - 1)].0,
        );
        let ratio = 0.5 * (5.0f64.sqrt() - 1.0);
        for _ in 0..GOLDEN_STEPS {
            let c = b - (b - a) * ratio;
            let d = a + (b - a) * ratio;
            if gap(at(c)) <= gap(at(d)) {
                b = d;
            } else {
                a = c;
            }
        }
        let closest = 0.5 * (a + b);
        let closest = if gap(at(closest)) <= gap(samples[best_i].1) {
            closest
        } else {
            samples[best_i].0
        };
        // Bisection of the entry (outside before, inside at `enter_i`) and the exit.
        let refine = |mut outside: f64, mut inside: f64| {
            for _ in 0..BISECT_STEPS {
                let mid = 0.5 * (outside + inside);
                if gap(at(mid)) <= query.radius_m {
                    inside = mid;
                } else {
                    outside = mid;
                }
            }
            inside
        };
        let enter_s = if enter_i == 0 {
            samples[0].0
        } else {
            refine(samples[enter_i - 1].0, samples[enter_i].0)
        };
        let exit_s = match exit {
            Some(i) if i > 0 => refine(samples[i].0, samples[i - 1].0),
            _ => samples[samples.len() - 1].0,
        };
        let body = path.body(closest, query.velocity_step_s).unwrap_or_default();
        let entry = path.body(enter_s, query.velocity_step_s).unwrap_or_default();
        hits.push(ProximityHit {
            id: target.id.clone(),
            closest_s: closest,
            gap_m: gap(body.position),
            distance_m: body.position.distance(centre),
            body_position: body.position.to_array(),
            speed_m_s: body.velocity.length(),
            enter_s,
            exit_s,
            closing_at_enter_m_s: closing_speed(entry.position, entry.velocity, centre),
        });
    }
    hits.sort_by(|a, b| a.closest_s.total_cmp(&b.closest_s).then_with(|| a.id.cmp(&b.id)));
    hits
}

#[cfg(test)]
mod tests {
    use super::super::FnPath;
    use super::*;

    fn line(speed: f32) -> FnPath<impl Fn(f64) -> Vec3> {
        FnPath {
            position: move |t: f64| Vec3::new(speed * t as f32 - 100.0 * speed / 10.0, 50.0, 0.0),
            span: (0.0, 20.0),
        }
    }

    fn target(id: &str, p: [f32; 3]) -> ProximityTarget {
        ProximityTarget {
            id: id.into(),
            position: p,
            radius_m: 0.0,
        }
    }

    #[test]
    fn closest_approach_entry_and_exit_of_a_straight_pass() {
        // 30 m/s along +X, starting at x = -300: passes x = 0 at t = 10 s.
        let path = line(30.0);
        let query = ProximityQuery {
            radius_m: 10.0,
            ..ProximityQuery::default()
        };
        let hits = proximity(
            &path,
            &[target("near", [0.0, 56.0, 0.0]), target("far", [0.0, 80.0, 0.0])],
            &query,
        );
        assert_eq!(hits.len(), 1, "{hits:?}");
        let h = &hits[0];
        assert_eq!(h.id, "near");
        assert!((h.closest_s - 10.0).abs() < 1.0e-4, "{}", h.closest_s);
        assert!((h.gap_m - 6.0).abs() < 1.0e-3);
        // Within 10 m while |x| ≤ 8 m: ±8/30 s around the pass.
        assert!((h.enter_s - (10.0 - 8.0 / 30.0)).abs() < 1.0e-4, "{}", h.enter_s);
        assert!((h.exit_s - (10.0 + 8.0 / 30.0)).abs() < 1.0e-4, "{}", h.exit_s);
        // Entering at x = -8, 6 m off: closing at 30 · 8/10.
        assert!(
            (h.closing_at_enter_m_s - 24.0).abs() < 0.05,
            "{}",
            h.closing_at_enter_m_s
        );
        assert!((h.speed_m_s - 30.0).abs() < 0.01);
    }

    #[test]
    fn the_answer_does_not_depend_on_the_step() {
        let path = line(750.0);
        let targets = [target("bird", [3.0, 52.0, 1.0])];
        let fine = proximity(
            &path,
            &targets,
            &ProximityQuery {
                radius_m: 8.0,
                step_s: 1.0 / 960.0,
                ..ProximityQuery::default()
            },
        );
        let coarse = proximity(
            &path,
            &targets,
            &ProximityQuery {
                radius_m: 8.0,
                step_s: 1.0 / 120.0,
                ..ProximityQuery::default()
            },
        );
        assert_eq!(fine.len(), 1);
        assert_eq!(coarse.len(), 1);
        assert!((fine[0].closest_s - coarse[0].closest_s).abs() < 1.0e-5);
        assert!((fine[0].enter_s - coarse[0].enter_s).abs() < 1.0e-5);
    }

    #[test]
    fn radii_and_an_empty_window() {
        let path = line(30.0);
        let mut big = target("tree", [0.0, 50.0, 20.0]);
        big.radius_m = 12.0;
        let query = ProximityQuery {
            radius_m: 10.0,
            body_radius_m: 1.5,
            ..ProximityQuery::default()
        };
        let hits = proximity(&path, std::slice::from_ref(&big), &query);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].gap_m - 6.5).abs() < 1.0e-3);
        let empty = ProximityQuery {
            from_s: 15.0,
            to_s: 12.0,
            ..query
        };
        assert!(proximity(&path, &[big], &empty).is_empty());
    }
}
