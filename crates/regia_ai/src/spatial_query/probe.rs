// SPDX-License-Identifier: MPL-2.0
//! The spatial-query probe and debug gizmos (`docs/specs/33-spatial-queries.md` § 4.2).
//!
//! A [`SpatialQueryProbe`] entity re-runs its query in place (as its own asker) and keeps the last
//! result in [`SpatialQueryProbeState`] for the gizmos and the agent (`ai.query_probe`). The run
//! system exists only with the `spatial-query-probe` feature; a shipping build without it never runs
//! probes. The gizmos draw into the `"spatial_query"` group, off by default.

use std::collections::BTreeMap;

use regia_app::prelude::*;
#[cfg(feature = "debug_draw")]
use regia_core::math::Color;
use regia_core::math::Vec3;
#[cfg(feature = "debug_draw")]
use regia_render::gizmos::{GizmoConfig, GizmoConfigGroup, Gizmos};

use super::{ParamValue, Pick, SpatialQueryResult};

/// The `"spatial_query"` gizmo group: spheres at items colored red (worst) to green (best), the
/// winner larger with a ring, failed items grey. Off by default (`debug.set_gizmos`).
#[cfg(feature = "debug_draw")]
#[derive(Default, Debug, Clone, Copy)]
pub struct SpatialQueryGizmos;

#[cfg(feature = "debug_draw")]
impl GizmoConfigGroup for SpatialQueryGizmos {
    const NAME: &'static str = "spatial_query";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..GizmoConfig::default()
        }
    }
}

/// How the probe gizmos size items.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Default)]
pub enum ProbeHighlight {
    /// Sized by score, the winner larger.
    #[default]
    Scaled,
    /// Every item at one size.
    Uniform,
    /// Items with `score >= best - band * (best - worst)` at full size, the rest shrunk.
    Top(f32),
}

/// An editor-placeable preview of a spatial query, run with this entity as the asker.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct SpatialQueryProbe {
    /// The template name or path.
    pub template: String,
    /// Parameter overrides.
    pub params: BTreeMap<String, ParamValue>,
    /// The pick (`None`: the asset's default).
    pub pick: Option<Pick>,
    /// Re-run when the entity has moved at least 0.25 m since the last run (checked once per fixed
    /// tick).
    pub rerun_on_move: bool,
    /// Keep and draw the items that failed a filter.
    pub draw_failed: bool,
    /// Draw text labels (score, or the failing test id).
    pub labels: bool,
    /// Sizing mode.
    pub highlight: ProbeHighlight,
    /// Run only the tests up to this authoring index (draw the survivors after that test).
    pub only_through_test: Option<usize>,
    /// Stop computing past this many work units (0 = no limit).
    pub max_units: u64,
}

impl Default for SpatialQueryProbe {
    fn default() -> Self {
        Self {
            template: String::new(),
            params: BTreeMap::new(),
            pick: None,
            rerun_on_move: true,
            draw_failed: false,
            labels: false,
            highlight: ProbeHighlight::Scaled,
            only_through_test: None,
            max_units: 0,
        }
    }
}

impl SpatialQueryProbe {
    /// A probe for `template` with defaults.
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            ..Self::default()
        }
    }
}

/// The distance a probe must move before `rerun_on_move` runs it again.
pub const PROBE_RERUN_DISTANCE: f32 = 0.25;

/// A probe's last run (inserted by the probe system).
#[derive(Component, Debug, Clone, Default)]
pub struct SpatialQueryProbeState {
    /// Where the probe stood at its last run.
    pub last_position: Option<Vec3>,
    /// The settings of the last run (a change re-runs the probe).
    pub last_probe: Option<SpatialQueryProbe>,
    /// The last result.
    pub result: Option<SpatialQueryResult>,
    /// How many times the probe ran.
    pub runs: u64,
}

/// `FixedUpdate` (exclusive): re-runs every probe that has never run, changed settings, or moved
/// [`PROBE_RERUN_DISTANCE`] with `rerun_on_move`.
#[cfg(feature = "spatial-query-probe")]
pub(crate) fn run_probes(world: &mut World) {
    use super::{SpatialQueryManager, SpatialQueryRequest};
    let _span = tracing::debug_span!(target: "regia_ai", "spatial_query_probes").entered();
    if !world.contains_resource::<SpatialQueryManager>() {
        return;
    }
    let mut probes: Vec<(Entity, SpatialQueryProbe)> = world
        .query::<(Entity, &SpatialQueryProbe)>()
        .iter(world)
        .map(|(e, p)| (e, p.clone()))
        .collect();
    probes.sort_by_key(|(e, _)| (e.index_u32(), e.to_bits()));
    for (entity, probe) in probes {
        let Some(position) = super::exec::entity_position(world, entity) else {
            continue;
        };
        let state = world.get::<SpatialQueryProbeState>(entity);
        let due = match state {
            None => true,
            Some(state) => {
                state.last_probe.as_ref() != Some(&probe)
                    || state.last_position.is_none()
                    || (probe.rerun_on_move
                        && state
                            .last_position
                            .is_some_and(|last| (position - last).length() >= PROBE_RERUN_DISTANCE))
            }
        };
        if !due {
            continue;
        }
        let mut req = SpatialQueryRequest::new(probe.template.clone(), entity);
        req.params = probe.params.clone();
        req.pick = probe.pick;
        req.keep_failed = probe.draw_failed;
        let result = world.resource_scope(|world, mut manager: Mut<SpatialQueryManager>| {
            manager.run_limited(world, req, probe.only_through_test, probe.max_units)
        });
        let runs = world.get::<SpatialQueryProbeState>(entity).map_or(0, |s| s.runs) + 1;
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(SpatialQueryProbeState {
                last_position: Some(position),
                last_probe: Some(probe),
                result: Some(result),
                runs,
            });
        }
    }
}

#[cfg(feature = "debug_draw")]
const WORST: Color = Color::rgb(0.9, 0.15, 0.1);
#[cfg(feature = "debug_draw")]
const BEST: Color = Color::rgb(0.15, 0.9, 0.2);
#[cfg(feature = "debug_draw")]
const FAILED: Color = Color::rgb(0.45, 0.45, 0.45);

#[cfg(feature = "debug_draw")]
fn mix(a: Color, b: Color, t: f32) -> Color {
    let [ar, ag, ab, aa] = a.to_array();
    let [br, bg, bb, ba] = b.to_array();
    Color::new(
        ar + (br - ar) * t,
        ag + (bg - ag) * t,
        ab + (bb - ab) * t,
        aa + (ba - aa) * t,
    )
}

/// `PostUpdate` (only while the `"spatial_query"` group is enabled): draws each probe's last result.
#[cfg(feature = "debug_draw")]
pub(crate) fn draw_probes(
    probes: Query<(&SpatialQueryProbe, &SpatialQueryProbeState)>,
    mut gizmos: Gizmos<SpatialQueryGizmos>,
) {
    for (probe, state) in &probes {
        let Some(result) = &state.result else {
            continue;
        };
        let passed: Vec<f32> = result
            .items
            .iter()
            .filter(|i| i.passed)
            .map(|i| i.score)
            .collect();
        let lo = passed.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = passed.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let span = hi - lo;
        for (index, item) in result.items.iter().enumerate() {
            if !item.passed {
                if probe.draw_failed {
                    gizmos.sphere(item.position, 0.08, FAILED);
                    if probe.labels
                        && let Some(test) = &item.failed_test
                    {
                        gizmos.text(
                            item.position + Vec3::Y * 0.25,
                            test,
                            0.12,
                            Default::default(),
                            FAILED,
                        );
                    }
                }
                continue;
            }
            let t = if span > 0.0 { (item.score - lo) / span } else { 1.0 };
            let color = mix(WORST, BEST, t);
            let winner = index == 0;
            let radius = match probe.highlight {
                ProbeHighlight::Uniform => 0.12,
                ProbeHighlight::Scaled => {
                    if winner {
                        0.25
                    } else {
                        0.08 + 0.08 * t
                    }
                }
                ProbeHighlight::Top(band) => {
                    if item.score >= hi - band.clamp(0.0, 1.0) * span {
                        0.18
                    } else {
                        0.05
                    }
                }
            };
            gizmos.sphere(item.position, radius, color);
            if winner && probe.highlight == ProbeHighlight::Scaled {
                gizmos.sphere(item.position, radius * 1.6, BEST);
            }
            if probe.labels {
                gizmos.text(
                    item.position + Vec3::Y * (radius + 0.15),
                    &format!("{:.2}", item.score),
                    0.12,
                    Default::default(),
                    color,
                );
            }
        }
    }
}
