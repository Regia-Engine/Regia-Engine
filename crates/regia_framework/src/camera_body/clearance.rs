// SPDX-License-Identifier: MPL-2.0
//! Clearance guard (ADR-1502): sweeps a planned camera path against the ground and against
//! tree trunks and crowns, and reports where the body comes closer than the clearance.
//!
//! It moves nothing. [`check_clearance`] only reports. [`lift_keys`] proposes raised path keys
//! from a report, and [`lift_shot`] repeats check-and-lift on one shot of a sequence until it is
//! clear (or the iteration limit). The caller decides whether to load the result.
//!
//! The ground is checked twice: below every sample (`ground_height`) and along each segment
//! between samples (`raycast`, when the world offers one), so a ridge between two samples is
//! still caught. Trees are a vertical trunk capsule plus a crown sphere ([`Obstacle`]). Runs of
//! consecutive violating samples against the same thing are merged into one [`Violation`], which
//! carries the worst gap.

use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use super::PathSampler;
use crate::shots::{PathKey, ShotSequence};

/// Most path samples one check takes.
pub const MAX_CLEARANCE_SAMPLES: usize = 1_000_000;

/// A tree (or any upright obstacle): a trunk capsule from `base` up `trunk_height_m`, and a
/// crown sphere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Obstacle {
    /// The caller's name for it.
    pub id: String,
    /// Foot of the trunk, metres.
    pub base: [f32; 3],
    /// Trunk radius, metres (0: no trunk).
    pub trunk_radius_m: f32,
    /// Trunk height above `base`, metres.
    pub trunk_height_m: f32,
    /// Crown centre, metres.
    pub crown_center: [f32; 3],
    /// Crown radius, metres (0: no crown).
    pub crown_radius_m: f32,
}

impl Obstacle {
    /// Distance from `p` to the trunk's surface and to the crown's surface, metres (negative
    /// inside; infinite where the part is absent).
    #[must_use]
    pub fn surface_distances(&self, p: Vec3) -> (f32, f32) {
        let base = Vec3::from_array(self.base);
        let trunk = if self.trunk_radius_m > 0.0 {
            let y = (p.y - base.y).clamp(0.0, self.trunk_height_m.max(0.0));
            p.distance(Vec3::new(base.x, base.y + y, base.z)) - self.trunk_radius_m
        } else {
            f32::INFINITY
        };
        let crown = if self.crown_radius_m > 0.0 {
            p.distance(Vec3::from_array(self.crown_center)) - self.crown_radius_m
        } else {
            f32::INFINITY
        };
        (trunk, crown)
    }

    /// The obstacle's bounding box.
    #[must_use]
    pub fn bounds(&self) -> (Vec3, Vec3) {
        let base = Vec3::from_array(self.base);
        let crown = Vec3::from_array(self.crown_center);
        let t = Vec3::new(self.trunk_radius_m, 0.0, self.trunk_radius_m);
        let top = base + Vec3::Y * self.trunk_height_m.max(0.0);
        let c = Vec3::splat(self.crown_radius_m.max(0.0));
        ((base - t).min(crown - c), (top + t).max(crown + c))
    }
}

/// The world the guard checks against. Hosts implement it over their terrain and scatter.
pub trait ClearanceWorld {
    /// Ground height at `(x, z)`, or `None` off the ground.
    fn ground_height(&self, x: f32, z: f32) -> Option<f32>;

    /// Distance along the unit `direction` from `origin` to the ground, if hit within
    /// `max_distance`. The default has no raycast (samples only).
    fn raycast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<f32> {
        let _ = (origin, direction, max_distance);
        None
    }

    /// Appends the obstacles overlapping the box `lo..hi` to `out`. The default has none.
    fn obstacles(&self, lo: Vec3, hi: Vec3, out: &mut Vec<Obstacle>) {
        let _ = (lo, hi, out);
    }
}

/// What the guard checks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClearanceSpec {
    /// Required gap between the body's surface and anything, metres.
    pub clearance_m: f32,
    /// The body's radius, metres.
    pub body_radius_m: f32,
    /// Sampling step, seconds.
    pub step_s: f64,
    /// Start of the window, seconds (clamped to the path).
    pub from_s: f64,
    /// End of the window, seconds (infinite = the end of the path).
    pub to_s: f64,
    /// Check the ground.
    pub ground: bool,
    /// Check obstacles (trunks and crowns).
    pub obstacles: bool,
}

impl Default for ClearanceSpec {
    fn default() -> Self {
        Self {
            clearance_m: 5.0,
            body_radius_m: 1.5,
            step_s: 1.0 / 120.0,
            from_s: 0.0,
            to_s: f64::INFINITY,
            ground: true,
            obstacles: true,
        }
    }
}

/// What the body came too close to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// The ground (below a sample or along a segment).
    Ground,
    /// A tree trunk.
    Trunk,
    /// A tree crown.
    Crown,
}

/// One run of samples closer than the clearance to one thing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Violation {
    /// What was too close.
    pub kind: ViolationKind,
    /// The obstacle's id (`None` for the ground).
    pub id: Option<String>,
    /// First violating sample, seconds.
    pub from_s: f64,
    /// Last violating sample, seconds.
    pub to_s: f64,
    /// The worst sample, seconds.
    pub worst_s: f64,
    /// The body's position at the worst sample.
    pub position: [f32; 3],
    /// The gap there between the body's surface and the thing, metres (negative: inside it).
    pub gap_m: f32,
    /// How far the path would have to rise there to clear it, metres.
    pub lift_m: f32,
}

/// The guard's answer.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ClearanceReport {
    /// Samples checked.
    pub samples: usize,
    /// Smallest gap to the ground, metres (infinite: no ground under the path).
    pub min_ground_gap_m: f32,
    /// When it happened, seconds.
    pub min_ground_s: f64,
    /// Smallest gap to an obstacle, metres (infinite: none near).
    pub min_obstacle_gap_m: f32,
    /// Obstacles considered.
    pub obstacles_checked: usize,
    /// The violations, by time.
    pub violations: Vec<Violation>,
    /// Every violating sample `(time, lift needed)`, for [`lift_keys`].
    #[serde(skip)]
    pub lift_samples: Vec<(f64, f32)>,
}

impl ClearanceReport {
    /// Whether the path is clear.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.violations.is_empty()
    }
}

/// The lift that clears an obstacle part whose surface is `gap` away at `p`: for a crown, rise
/// until the sphere is cleared from above; for a trunk, rise over its top.
fn obstacle_lift(o: &Obstacle, kind: ViolationKind, p: Vec3, need: f32) -> f32 {
    match kind {
        ViolationKind::Crown => {
            let c = Vec3::from_array(o.crown_center);
            let r = o.crown_radius_m + need;
            let dx = Vec3::new(p.x - c.x, 0.0, p.z - c.z).length();
            if dx >= r {
                0.0
            } else {
                (c.y + (r * r - dx * dx).sqrt() - p.y).max(0.0)
            }
        }
        ViolationKind::Trunk => (o.base[1] + o.trunk_height_m + need - p.y).max(0.0),
        ViolationKind::Ground => 0.0,
    }
}

/// Sweeps `path` against `world` (see the module docs). Pure given the world.
#[must_use]
pub fn check_clearance(
    path: &dyn PathSampler,
    world: &dyn ClearanceWorld,
    spec: &ClearanceSpec,
) -> ClearanceReport {
    let mut report = ClearanceReport {
        min_ground_gap_m: f32::INFINITY,
        min_obstacle_gap_m: f32::INFINITY,
        ..ClearanceReport::default()
    };
    let (start, end) = path.span();
    let from = spec.from_s.max(start);
    let to = if spec.to_s.is_finite() {
        spec.to_s.min(end)
    } else {
        end
    };
    if from.is_nan() || to.is_nan() || to < from || !spec.step_s.is_finite() || spec.step_s <= 0.0 {
        return report;
    }
    let n = (((to - from) / spec.step_s).ceil() as usize).min(MAX_CLEARANCE_SAMPLES);
    let step = if n == 0 { 0.0 } else { (to - from) / n as f64 };
    let samples: Vec<(f64, Vec3)> = (0..=n)
        .filter_map(|i| {
            let t = if i == n { to } else { from + step * i as f64 };
            path.position(t).map(|p| (t, p))
        })
        .collect();
    report.samples = samples.len();
    let need = spec.clearance_m.max(0.0) + spec.body_radius_m.max(0.0);

    let mut obstacles = Vec::new();
    if spec.obstacles && !samples.is_empty() {
        let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        for (_, p) in &samples {
            lo = lo.min(*p);
            hi = hi.max(*p);
        }
        world.obstacles(lo - Vec3::splat(need), hi + Vec3::splat(need), &mut obstacles);
        obstacles.sort_by(|a, b| a.id.cmp(&b.id));
    }
    report.obstacles_checked = obstacles.len();

    // One open run per (kind, obstacle index); index usize::MAX is the ground.
    let mut open: Vec<(ViolationKind, usize, Violation)> = Vec::new();
    let mut hits: Vec<(ViolationKind, usize, f32, f32)> = Vec::new();
    for (i, &(t, p)) in samples.iter().enumerate() {
        hits.clear();
        if spec.ground {
            let below = world
                .ground_height(p.x, p.z)
                .map(|g| p.y - g - spec.body_radius_m);
            let mut gap = below.unwrap_or(f32::INFINITY);
            if let Some(&(_, next)) = samples.get(i + 1) {
                let seg = next - p;
                let len = seg.length();
                if len > 1.0e-4
                    && let Some(d) = world.raycast(p, seg / len, len)
                {
                    // The segment dives into the ground: the gap is how far the hit point lies
                    // below the ground's top there (a wall reads as its full height).
                    let hit = p + seg / len * d;
                    let top = world.ground_height(hit.x, hit.z).unwrap_or(hit.y);
                    gap = gap.min(hit.y.min(top) - top - spec.body_radius_m);
                }
            }
            if gap < report.min_ground_gap_m {
                report.min_ground_gap_m = gap;
                report.min_ground_s = t;
            }
            if gap < spec.clearance_m {
                hits.push((ViolationKind::Ground, usize::MAX, gap, spec.clearance_m - gap));
            }
        }
        for (k, o) in obstacles.iter().enumerate() {
            let (trunk, crown) = o.surface_distances(p);
            for (kind, d) in [(ViolationKind::Trunk, trunk), (ViolationKind::Crown, crown)] {
                let gap = d - spec.body_radius_m;
                report.min_obstacle_gap_m = report.min_obstacle_gap_m.min(gap);
                if gap < spec.clearance_m {
                    hits.push((kind, k, gap, obstacle_lift(o, kind, p, need)));
                }
            }
        }
        // Close runs that this sample does not continue.
        let mut j = 0;
        while j < open.len() {
            let (kind, k, _) = &open[j];
            if hits.iter().any(|h| h.0 == *kind && h.1 == *k) {
                j += 1;
            } else {
                report.violations.push(open.remove(j).2);
            }
        }
        let mut lift_here = 0.0f32;
        for &(kind, k, gap, lift) in &hits {
            lift_here = lift_here.max(lift);
            match open.iter_mut().find(|(ok, oi, _)| *ok == kind && *oi == k) {
                Some((_, _, v)) => {
                    v.to_s = t;
                    if gap < v.gap_m {
                        v.gap_m = gap;
                        v.worst_s = t;
                        v.position = p.to_array();
                    }
                    v.lift_m = v.lift_m.max(lift);
                }
                None => open.push((
                    kind,
                    k,
                    Violation {
                        kind,
                        id: obstacles.get(k).map(|o| o.id.clone()),
                        from_s: t,
                        to_s: t,
                        worst_s: t,
                        position: p.to_array(),
                        gap_m: gap,
                        lift_m: lift,
                    },
                )),
            }
        }
        if !hits.is_empty() {
            report.lift_samples.push((t, lift_here));
        }
    }
    report.violations.extend(open.into_iter().map(|(_, _, v)| v));
    report.violations.sort_by(|a, b| {
        a.from_s
            .total_cmp(&b.from_s)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| (a.kind as u8).cmp(&(b.kind as u8)))
    });
    report
}

/// Keys raised to clear `report`'s violations: every key whose neighbourhood (the span to the
/// keys before and after it) holds a violating sample rises by the largest lift needed there,
/// plus `margin_m`. `time_offset` converts the keys' times (shot-local) to the report's.
#[must_use]
pub fn lift_keys(
    keys: &[PathKey],
    time_offset: f64,
    report: &ClearanceReport,
    margin_m: f32,
) -> Vec<PathKey> {
    let mut out = keys.to_vec();
    if report.lift_samples.is_empty() {
        return out;
    }
    for (i, key) in out.iter_mut().enumerate() {
        let lo = keys
            .get(i.wrapping_sub(1))
            .map_or(f64::NEG_INFINITY, |k| f64::from(k.time) + time_offset);
        let hi = keys
            .get(i + 1)
            .map_or(f64::INFINITY, |k| f64::from(k.time) + time_offset);
        let lift = report
            .lift_samples
            .iter()
            .filter(|(t, _)| *t > lo && *t < hi)
            .map(|(_, l)| *l)
            .fold(0.0f32, f32::max);
        if lift > 0.0 {
            key.position[1] += lift + margin_m.max(0.0);
        }
    }
    out
}

/// Check-and-lift of shot `index` of `sequence` until it clears or `iterations` run out. Returns
/// the lifted sequence and the last report (the caller loads it only if it wants to).
#[must_use]
pub fn lift_shot(
    sequence: &ShotSequence,
    index: usize,
    world: &dyn ClearanceWorld,
    spec: &ClearanceSpec,
    margin_m: f32,
    iterations: u32,
) -> (ShotSequence, ClearanceReport) {
    let mut seq = sequence.clone();
    let offset = seq.shot_start(index);
    let span = ClearanceSpec {
        from_s: offset,
        to_s: offset + seq.shots.get(index).map_or(0.0, |s| f64::from(s.duration)),
        ..*spec
    };
    let mut report = check_clearance(&seq, world, &span);
    for _ in 0..iterations {
        if report.is_clear() {
            break;
        }
        let Some(shot) = seq.shots.get_mut(index) else {
            break;
        };
        shot.camera.path.keys = lift_keys(&shot.camera.path.keys, offset, &report, margin_m);
        report = check_clearance(&seq, world, &span);
    }
    (seq, report)
}

#[cfg(test)]
mod tests {
    use super::super::FnPath;
    use super::*;

    /// Flat ground at y = 0 with a 30 m ridge at x in 0..10, and one tree at x = 100.
    struct Hills;

    impl ClearanceWorld for Hills {
        fn ground_height(&self, x: f32, _z: f32) -> Option<f32> {
            Some(if (0.0..10.0).contains(&x) { 30.0 } else { 0.0 })
        }

        fn raycast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<f32> {
            let mut d = 0.0;
            while d <= max_distance {
                let p = origin + direction * d;
                if p.y <= self.ground_height(p.x, p.z).unwrap_or(f32::NEG_INFINITY) {
                    return Some(d);
                }
                d += 0.05;
            }
            None
        }

        fn obstacles(&self, lo: Vec3, hi: Vec3, out: &mut Vec<Obstacle>) {
            let tree = Obstacle {
                id: "oak".into(),
                base: [100.0, 0.0, 0.0],
                trunk_radius_m: 0.4,
                trunk_height_m: 6.0,
                crown_center: [100.0, 10.0, 0.0],
                crown_radius_m: 5.0,
            };
            let (a, b) = tree.bounds();
            if a.cmple(hi).all() && b.cmpge(lo).all() {
                out.push(tree);
            }
        }
    }

    fn level(height: f32) -> FnPath<impl Fn(f64) -> Vec3> {
        FnPath {
            position: move |t: f64| Vec3::new(-50.0 + 100.0 * t as f32, height, 0.0),
            span: (0.0, 3.0),
        }
    }

    #[test]
    fn ridge_and_crown_are_reported_with_their_lift() {
        let report = check_clearance(&level(12.0), &Hills, &ClearanceSpec::default());
        assert!(!report.is_clear());
        let kinds: Vec<ViolationKind> = report.violations.iter().map(|v| v.kind).collect();
        assert!(kinds.contains(&ViolationKind::Ground), "{kinds:?}");
        assert!(kinds.contains(&ViolationKind::Crown), "{kinds:?}");
        let ground = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::Ground)
            .expect("a ground violation");
        assert!(
            ground.gap_m < 0.0,
            "the path dives into the ridge: {}",
            ground.gap_m
        );
        let crown = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::Crown)
            .expect("a crown violation");
        assert_eq!(crown.id.as_deref(), Some("oak"));
        // Over the crown top (15 m) with the clearance (5) and the body (1.5): y ≥ 21.5.
        assert!((crown.lift_m - (21.5 - 12.0)).abs() < 0.2, "{}", crown.lift_m);
        assert_eq!(report.obstacles_checked, 1);
    }

    #[test]
    fn a_high_path_is_clear() {
        let report = check_clearance(&level(60.0), &Hills, &ClearanceSpec::default());
        assert!(report.is_clear(), "{:?}", report.violations);
        assert!((report.min_ground_gap_m - (60.0 - 30.0 - 1.5)).abs() < 1.0e-3);
    }

    #[test]
    fn the_segment_raycast_catches_a_ridge_between_samples() {
        // 1 s steps: samples at x = -50, 50, 150 straddle the ridge without one above it.
        let spec = ClearanceSpec {
            step_s: 1.0,
            obstacles: false,
            ..ClearanceSpec::default()
        };
        let report = check_clearance(&level(20.0), &Hills, &spec);
        assert!(report.violations.iter().any(|v| v.kind == ViolationKind::Ground));
    }

    #[test]
    fn lifting_a_shot_clears_it() {
        use crate::shots::{CameraPath, LookAt, PathInterp, Shot, ShotCamera, Track};
        let shot = Shot {
            name: "pass".into(),
            duration: 4.0,
            camera: ShotCamera {
                path: CameraPath {
                    interp: PathInterp::Linear,
                    keys: (0..=8)
                        .map(|i| PathKey {
                            time: i as f32 * 0.5,
                            position: [-100.0 + 50.0 * i as f32, 12.0, 0.0],
                        })
                        .collect(),
                    look_at: LookAt::Heading,
                },
                fov: Track::default(),
                roll: Track::default(),
                dof: None,
                rotation: Vec::new(),
            },
            fade_in: 0.0,
            fade_out: 0.0,
            fade_color: [0.0; 3],
        };
        let sequence = ShotSequence {
            shots: vec![shot],
            ..ShotSequence::default()
        };
        let before = check_clearance(&sequence, &Hills, &ClearanceSpec::default());
        assert!(!before.is_clear());
        let (lifted, after) = lift_shot(&sequence, 0, &Hills, &ClearanceSpec::default(), 1.0, 4);
        assert!(after.is_clear(), "{:?}", after.violations);
        // Only keys near trouble rose: the first two are far from both (the ridge starts at the
        // third key, x = 0), the third and fourth bracket the ridge.
        let keys = &lifted.shots[0].camera.path.keys;
        assert_eq!(keys[0].position[1], 12.0);
        assert_eq!(keys[1].position[1], 12.0);
        assert!(keys[2].position[1] >= 30.0 + 5.0 + 1.5, "{}", keys[2].position[1]);
        assert!(keys[3].position[1] > 12.0);
    }
}
