// SPDX-License-Identifier: MPL-2.0
//! Grounded placement of authored points on a heightfield (ADR-8036).
//!
//! A stamped point must sit on the height sampled at its horizontal position, inside the map,
//! and carry an id no other point in the same list uses. A spawn pad's ring must stay under a
//! rise limit. Tolerances, the ring radius and the rise ratio are settings on [`PlacementRules`].
//! Points with no stamped height are not float failures.

use std::collections::HashSet;

/// Default vertical slack between a stamped height and the heightfield, in metres.
pub const DEFAULT_GROUND_TOLERANCE_M: f32 = 0.05;

/// Default slack past the map half-extents, in metres.
pub const DEFAULT_EDGE_TOLERANCE_M: f32 = 0.05;

/// Default spawn-pad ring radius, in metres. `0` skips the ring check.
pub const DEFAULT_SPAWN_RING_M: f32 = 9.0;

/// Default rise limit as a run ratio: tan(15°) at `f32` precision, so the check does not call a
/// libm tangent. The longer spelling 0.26794919 is the same `f32`.
pub const DEFAULT_SPAWN_RISE_OVER_RUN: f32 = 0.267_949_2;

/// The stamped height is off the heightfield (or is not finite).
pub const ON_HEIGHTFIELD: &str = "on_heightfield";

/// The horizontal position is outside the map, or is not finite.
pub const INSIDE_MAP: &str = "inside_map";

/// Two points in one list share an id.
pub const UNIQUE_ID: &str = "unique_id";

/// A spawn pad's ring rises too far from its centre.
pub const SPAWN_FLAT: &str = "spawn_flat";

/// Eight unit offsets at 45° steps. The 45° components are `1/sqrt(2)`, stored so the ring
/// does not call sine or cosine.
const RING_OFFSETS: [(f32, f32); 8] = [
    (1.0, 0.0),
    (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2),
    (0.0, 1.0),
    (-std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2),
    (-1.0, 0.0),
    (-std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2),
    (0.0, -1.0),
    (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2),
];

/// One authored point. `ground_y: None` means the point was not stamped and is not a float failure.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedObject {
    /// Stable id inside this list.
    pub id: String,
    /// World x, metres.
    pub x: f32,
    /// World z, metres.
    pub z: f32,
    /// Stamped height, metres. Absent when the point was not stamped.
    pub ground_y: Option<f32>,
    /// When set, the spawn ring around `(x, z)` is checked.
    pub spawn_pad: bool,
}

/// Tolerances and map size for one check. Every field is a setting.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacementRules {
    /// Metres a stamped height may differ from the heightfield.
    pub ground_tolerance_m: f32,
    /// Metres a point may sit past a half-extent.
    pub edge_tolerance_m: f32,
    /// Half the map's x size, metres. The map runs from `-half` to `+half`.
    pub half_extent_x: f32,
    /// Half the map's z size, metres.
    pub half_extent_z: f32,
    /// Spawn-ring radius, metres. `0` skips the ring.
    pub spawn_ring_m: f32,
    /// Maximum `|height − centre| / radius` on the ring.
    pub spawn_rise_over_run: f32,
}

impl Default for PlacementRules {
    fn default() -> Self {
        Self {
            ground_tolerance_m: DEFAULT_GROUND_TOLERANCE_M,
            edge_tolerance_m: DEFAULT_EDGE_TOLERANCE_M,
            half_extent_x: 0.0,
            half_extent_z: 0.0,
            spawn_ring_m: DEFAULT_SPAWN_RING_M,
            spawn_rise_over_run: DEFAULT_SPAWN_RISE_OVER_RUN,
        }
    }
}

impl PlacementRules {
    /// Rules for a square map of the given half-extent, with the default tolerances and ring.
    #[must_use]
    pub fn square(half_extent_m: f32) -> Self {
        Self {
            half_extent_x: half_extent_m,
            half_extent_z: half_extent_m,
            ..Self::default()
        }
    }
}

/// One failed check. `code` is a stable lowercase token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementIssue {
    /// `on_heightfield`, `inside_map`, `unique_id` or `spawn_flat`.
    pub code: &'static str,
    /// The point's id. Empty when the id itself was missing.
    pub id: String,
    /// One sentence a person can read.
    pub detail: String,
}

/// Check `objects` against `height_at` (world x, z → metres). Issues follow the input order.
#[must_use]
pub fn layout_issues(
    objects: &[PlacedObject],
    rules: &PlacementRules,
    height_at: impl Fn(f32, f32) -> f32,
) -> Vec<PlacementIssue> {
    let mut seen = HashSet::new();
    let mut issues = Vec::new();
    for object in objects {
        if !seen.insert(object.id.clone()) {
            issues.push(PlacementIssue {
                code: UNIQUE_ID,
                id: object.id.clone(),
                detail: format!("id {} is already used", object.id),
            });
        }
        if !object.x.is_finite() || !object.z.is_finite() {
            issues.push(PlacementIssue {
                code: INSIDE_MAP,
                id: object.id.clone(),
                detail: format!("position of {} is not finite", object.id),
            });
            continue;
        }
        if !inside_map(object.x, object.z, rules) {
            issues.push(PlacementIssue {
                code: INSIDE_MAP,
                id: object.id.clone(),
                detail: format!("position of {} is outside the map", object.id),
            });
            continue;
        }
        if let Some(stamped) = object.ground_y {
            let sampled = height_at(object.x, object.z);
            let delta = (stamped - sampled).abs();
            if !stamped.is_finite() || !sampled.is_finite() || delta > rules.ground_tolerance_m {
                let shown = if delta.is_finite() { delta } else { f32::INFINITY };
                issues.push(PlacementIssue {
                    code: ON_HEIGHTFIELD,
                    id: object.id.clone(),
                    detail: format!("stamped y {stamped} m is {shown} m off the heightfield"),
                });
            }
        }
        if object.spawn_pad
            && rules.spawn_ring_m > 0.0
            && let Some(issue) = spawn_ring_issue(object, rules, &height_at)
        {
            issues.push(issue);
        }
    }
    issues
}

fn inside_map(x: f32, z: f32, rules: &PlacementRules) -> bool {
    x.abs() <= rules.half_extent_x + rules.edge_tolerance_m
        && z.abs() <= rules.half_extent_z + rules.edge_tolerance_m
}

fn spawn_ring_issue(
    object: &PlacedObject,
    rules: &PlacementRules,
    height_at: &impl Fn(f32, f32) -> f32,
) -> Option<PlacementIssue> {
    let centre = height_at(object.x, object.z);
    let radius = rules.spawn_ring_m;
    let mut rise: f32 = 0.0;
    for (ox, oz) in RING_OFFSETS {
        let sample = height_at(object.x + ox * radius, object.z + oz * radius);
        if !sample.is_finite() || !centre.is_finite() {
            return Some(PlacementIssue {
                code: SPAWN_FLAT,
                id: object.id.clone(),
                detail: format!("spawn ring of {} sampled a non-finite height", object.id),
            });
        }
        rise = rise.max((sample - centre).abs());
    }
    let limit = radius * rules.spawn_rise_over_run;
    if rise >= limit {
        return Some(PlacementIssue {
            code: SPAWN_FLAT,
            id: object.id.clone(),
            detail: format!(
                "spawn ring of {} rises {rise} m from the centre; the limit is {limit} m",
                object.id
            ),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_zero(_: f32, _: f32) -> f32 {
        0.0
    }

    fn object(id: &str, y: Option<f32>) -> PlacedObject {
        PlacedObject {
            id: id.to_owned(),
            x: 0.0,
            z: 0.0,
            ground_y: y,
            spawn_pad: false,
        }
    }

    #[test]
    fn floating_object_fails_placement() {
        let rules = PlacementRules {
            ground_tolerance_m: 0.05,
            ..PlacementRules::square(100.0)
        };
        let floating = layout_issues(&[object("crate", Some(1.0))], &rules, flat_zero);
        assert!(
            floating.iter().any(|issue| issue.code == ON_HEIGHTFIELD),
            "a point 1 m above a flat field must fail on_heightfield, got {floating:?}"
        );
        assert!(floating[0].detail.contains("stamped y"), "{}", floating[0].detail);

        let seated = layout_issues(&[object("crate", Some(0.04))], &rules, flat_zero);
        assert!(
            seated.is_empty(),
            "0.04 m is inside the 0.05 m tolerance, got {seated:?}"
        );
    }

    #[test]
    fn an_unstamped_point_is_not_a_float_failure() {
        let rules = PlacementRules::square(10.0);
        let issues = layout_issues(&[object("mark", None)], &rules, flat_zero);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_point_outside_the_map_or_with_a_repeated_id_fails() {
        let rules = PlacementRules::square(10.0);
        let mut far = object("edge", Some(0.0));
        far.x = 10.06;
        let issues = layout_issues(&[far], &rules, flat_zero);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].code, INSIDE_MAP);

        let mut nan = object("nan", Some(0.0));
        nan.z = f32::NAN;
        let issues = layout_issues(&[nan], &rules, flat_zero);
        assert_eq!(issues[0].code, INSIDE_MAP);
        assert!(issues[0].detail.contains("not finite"));

        let issues = layout_issues(
            &[object("same", Some(0.0)), object("same", Some(0.0))],
            &rules,
            flat_zero,
        );
        assert!(issues.iter().any(|issue| issue.code == UNIQUE_ID), "{issues:?}");
    }

    #[test]
    fn spawn_ring_rise_over_limit_fails() {
        let rules = PlacementRules {
            spawn_ring_m: 9.0,
            spawn_rise_over_run: DEFAULT_SPAWN_RISE_OVER_RUN,
            ..PlacementRules::square(100.0)
        };
        let limit = 9.0 * DEFAULT_SPAWN_RISE_OVER_RUN;
        let steep = |x: f32, z: f32| {
            let distance = (x * x + z * z).sqrt();
            if distance >= 8.0 { limit + 0.5 } else { 0.0 }
        };
        let mut pad = object("pad", Some(0.0));
        pad.spawn_pad = true;
        let issues = layout_issues(&[pad.clone()], &rules, steep);
        assert!(
            issues.iter().any(|issue| issue.code == SPAWN_FLAT),
            "rise over the limit must fail spawn_flat, got {issues:?}"
        );

        let gentle = |x: f32, z: f32| {
            let distance = (x * x + z * z).sqrt();
            if distance >= 8.0 { limit - 0.5 } else { 0.0 }
        };
        let issues = layout_issues(&[pad], &rules, gentle);
        assert!(
            issues.iter().all(|issue| issue.code != SPAWN_FLAT),
            "a rise under the limit must pass, got {issues:?}"
        );
    }
}
