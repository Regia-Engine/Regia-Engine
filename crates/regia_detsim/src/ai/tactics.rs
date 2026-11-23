// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/tactics.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-331, plan row S-11, ADR-8462). The catalogue, the bias and the hysteresis loop are DO27's
// code apart from rustfmt, doc comments and these named changes:
// - the two float `max` calls (`force_ratio_norm`, the outnumbered knee) go through `regia_detsim::float::det_max`
//   (ADR-8085: the same bits away from a zero or NaN result; lockstep crates deny std `max`);
// - `decide`'s best-score pick uses `f32::total_cmp` (DO27's `math::total_cmp` is the same order), as S-09's
//   `ArgMax` does;
// - `Hysteresis` implements S-09's `DecisionPolicy` (DO27's pipeline calls `decide` with `invalidated = false`,
//   which is what `commit` does), and `Hysteresis::cadence` names the schedule its decision interval is;
// - DO27's design-document section numbers are dropped from the comments.

//! Tactics: the option catalogue and the hysteresis decision loop.
//!
//! The closed option set is shared across difficulty levels; a level's [`super::RepertoireMask`] limits *which*
//! it may pick. The curves are parameterised by the profile's thresholds, so difficulty is the same curves with
//! different knees and slopes — never different stats.

use serde::{Deserialize, Serialize};

use super::difficulty::Thresholds;
use super::scheduler::DecisionPolicy;
use super::utility::{AiOption, Consideration, ConsiderationId, OptionId, ResponseCurve};
use super::{AiCadence, AiSchedule};
use crate::float::det_max;

/// Normalise a raw force ratio (allies ÷ enemies) into 0..1: 0.5 = even, 1.0 = dominant, 0.0 = outnumbered.
#[inline]
pub fn force_ratio_norm(ratio: f32) -> f32 {
    let r = det_max(ratio, 0.0);
    r / (r + 1.0)
}

/// The difficulty's aggression/defence score scale: `aggression` multiplies the Push score, `defence` multiplies
/// Retreat/Defend. A cautious (Hard) profile pushes less and withdraws more; a reckless (Easy) profile does the
/// opposite. Decision quality, never stats.
#[inline]
pub fn bias(id: OptionId, t: &Thresholds) -> f32 {
    match id {
        OptionId::Push => t.aggression,
        OptionId::Retreat | OptionId::Defend => t.defence,
        _ => 1.0,
    }
}

/// The 12 options, curves parameterised by `t`, in [`OptionId::ALL`] order.
pub fn catalogue(t: &Thresholds) -> Vec<AiOption> {
    let push_knee = force_ratio_norm(t.push_force_ratio);
    let outnumbered_knee = force_ratio_norm(1.0 / det_max(t.outnumbered_ratio, 1.0));
    vec![
        // 1. Push / engage.
        AiOption::new(
            OptionId::Push,
            vec![
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::logistic(12.0, push_knee),
                    1.0,
                ),
                Consideration::add(ConsiderationId::HullPct, ResponseCurve::plateau(0.6), 0.8),
                Consideration::mult(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0)),
            ],
        ),
        // 2. Hold / snipe.
        AiOption::new(
            OptionId::Hold,
            vec![
                Consideration::add(ConsiderationId::RangeBandFrac, ResponseCurve::band(0.5, 0.3), 1.0),
                Consideration::add(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0), 0.3),
                Consideration::add(
                    ConsiderationId::CoverAvailability,
                    ResponseCurve::linear(1.0),
                    0.4,
                ),
            ],
        ),
        // 3. Strafe / orbit.
        AiOption::new(
            OptionId::Strafe,
            vec![
                Consideration::add(ConsiderationId::RangeBandFrac, ResponseCurve::band(0.5, 0.3), 1.0),
                Consideration::add(ConsiderationId::LocalForceRatio, ResponseCurve::linear(1.0), 0.4),
                Consideration::mult(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0)),
            ],
        ),
        // 4. Dogfight.
        AiOption::new(
            OptionId::Dogfight,
            vec![
                Consideration::add(
                    ConsiderationId::RelativeEnergyFrac,
                    ResponseCurve::logistic(10.0, 0.5),
                    1.0,
                ),
                Consideration::add(ConsiderationId::TurnRateFrac, ResponseCurve::linear(1.0), 0.6),
                Consideration::add(ConsiderationId::HullPct, ResponseCurve::linear(1.0), 0.4),
                Consideration::mult(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0)),
            ],
        ),
        // 5. Kite.
        AiOption::new(
            OptionId::Kite,
            vec![
                Consideration::add(ConsiderationId::RangeBandFrac, ResponseCurve::band(0.7, 0.2), 1.0),
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::inverse_logistic(8.0, outnumbered_knee),
                    0.6,
                ),
                Consideration::add(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0), 0.5),
            ],
        ),
        // 6. Retreat / disengage.
        AiOption::new(
            OptionId::Retreat,
            vec![
                Consideration::add(
                    ConsiderationId::HullPct,
                    ResponseCurve::inverse_logistic(12.0, t.retreat_hull_pct),
                    1.0,
                ),
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::inverse_logistic(8.0, outnumbered_knee),
                    0.8,
                ),
                Consideration::add(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0), 0.3),
            ],
        ),
        // 7. Regroup.
        AiOption::new(
            OptionId::Regroup,
            vec![
                Consideration::add(
                    ConsiderationId::DistanceToLeaderFrac,
                    ResponseCurve::linear(1.0),
                    1.0,
                ),
                Consideration::mult(ConsiderationId::CommanderPriority, ResponseCurve::linear(1.0)),
                Consideration::add(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0), 0.2),
            ],
        ),
        // 8. Defend point.
        AiOption::new(
            OptionId::Defend,
            vec![
                Consideration::mult(ConsiderationId::CommanderPriority, ResponseCurve::linear(1.0)),
                Consideration::add(
                    ConsiderationId::PointDistanceFrac,
                    ResponseCurve::inverse_quadratic(),
                    1.0,
                ),
                Consideration::add(ConsiderationId::LocalForceRatio, ResponseCurve::linear(1.0), 0.4),
            ],
        ),
        // 9. Collect a resource.
        AiOption::new(
            OptionId::CollectScrap,
            vec![
                Consideration::add(
                    ConsiderationId::ScrapProximity,
                    ResponseCurve::inverse_logistic(10.0, 0.3),
                    1.0,
                ),
                Consideration::add(ConsiderationId::HopperFrac, ResponseCurve::band(0.5, 0.3), 0.5),
                Consideration::mult(
                    ConsiderationId::ThreatIncoming,
                    ResponseCurve::inverse_logistic(8.0, 0.5),
                ),
            ],
        ),
        // 10. Escort leader.
        AiOption::new(
            OptionId::Escort,
            vec![
                Consideration::add(
                    ConsiderationId::DistanceToLeaderFrac,
                    ResponseCurve::band(0.3, 0.3),
                    1.0,
                ),
                Consideration::mult(ConsiderationId::CommanderPriority, ResponseCurve::linear(1.0)),
                Consideration::add(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0), 0.2),
            ],
        ),
        // 11. Flank.
        AiOption::new(
            OptionId::Flank,
            vec![
                Consideration::add(
                    ConsiderationId::CoverAvailability,
                    ResponseCurve::linear(1.0),
                    1.0,
                ),
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::logistic(8.0, push_knee),
                    0.6,
                ),
                Consideration::mult(ConsiderationId::ThreatIncoming, ResponseCurve::linear(1.0)),
            ],
        ),
        // 12. Focus fire.
        AiOption::new(
            OptionId::FocusFire,
            vec![
                Consideration::mult(ConsiderationId::LeaderTargetFocus, ResponseCurve::linear(1.0)),
                Consideration::add(ConsiderationId::RangeBandFrac, ResponseCurve::band(0.5, 0.3), 0.8),
                Consideration::add(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0), 0.3),
            ],
        ),
    ]
}

/// Anti-flip-flop parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Hysteresis {
    /// 10 ticks ≈ 167 ms; per-unit phase = entity_id % interval.
    pub decision_interval_ticks: u32,
    /// A challenger must beat the incumbent by this much.
    pub switch_margin: f32,
    /// ...continuously for this long.
    pub min_hold_ticks: u32,
    /// Re-evaluate even if nothing changes (reaction decay).
    pub max_hold_ticks: u32,
}

impl Default for Hysteresis {
    fn default() -> Self {
        Hysteresis {
            decision_interval_ticks: 10,
            switch_margin: 0.15,
            min_hold_ticks: 5,
            max_hold_ticks: 40,
        }
    }
}

impl Hysteresis {
    /// The AI core schedule this loop's decision interval is: decisions every `decision_interval_ticks`, sighting
    /// refreshes on [`AiSchedule::SIGHT_TICKS`]. The default is [`AiCadence::default`].
    pub fn cadence(&self) -> AiCadence {
        AiCadence::new(self.decision_interval_ticks, AiSchedule::SIGHT_TICKS)
    }
}

/// A unit's committed decision. A committed action is a visible consequence: the unit holds its option so the
/// player can read it.
///
/// `held_ticks` and `challenger_ticks` count decisions (calls of [`decide`]), which are ticks when the unit
/// decides every tick and decision windows on a time-sliced schedule.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Decision {
    /// The committed option (`None` before the first decision).
    pub current: Option<OptionId>,
    /// Decisions the current option has been held.
    pub held_ticks: u32,
    /// The option leading the current one by the switch margin, if any.
    pub challenger: Option<OptionId>,
    /// Decisions in a row the challenger has led by the margin.
    pub challenger_ticks: u32,
}

fn score_of(scores: &[(OptionId, f32)], id: OptionId) -> f32 {
    scores
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, s)| *s)
        .unwrap_or(0.0)
}

/// The hysteresis decision loop. Returns the committed [`OptionId`].
///
/// Keep the current option unless (a) a challenger's score ≥ current + margin continuously for
/// `min_hold_ticks`, or (b) the option is invalidated, or (c) it has been held `max_hold_ticks`. The best score
/// is the last of equal maxima; no scores commit [`OptionId::Hold`].
pub fn decide(
    hyst: &Hysteresis,
    dec: &mut Decision,
    scores: &[(OptionId, f32)],
    invalidated: bool,
) -> OptionId {
    let best = scores
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| *id)
        .unwrap_or(OptionId::Hold);
    let Some(current) = dec.current else {
        dec.current = Some(best);
        dec.held_ticks = 0;
        dec.challenger = None;
        dec.challenger_ticks = 0;
        return best;
    };

    if invalidated || current == best {
        dec.current = Some(best);
        dec.held_ticks = dec.held_ticks.saturating_add(1);
        dec.challenger = None;
        dec.challenger_ticks = 0;
        return best;
    }

    let current_score = score_of(scores, current);
    let best_score = score_of(scores, best);
    if best_score >= current_score + hyst.switch_margin {
        dec.challenger = Some(best);
        dec.challenger_ticks = dec.challenger_ticks.saturating_add(1);
        if dec.challenger_ticks >= hyst.min_hold_ticks {
            dec.current = Some(best);
            dec.held_ticks = 0;
            dec.challenger = None;
            dec.challenger_ticks = 0;
            return best;
        }
    } else {
        dec.challenger = None;
        dec.challenger_ticks = 0;
    }

    if dec.held_ticks >= hyst.max_hold_ticks {
        dec.current = Some(best);
        dec.held_ticks = 0;
        dec.challenger = None;
        dec.challenger_ticks = 0;
        return best;
    }

    dec.held_ticks = dec.held_ticks.saturating_add(1);
    current
}

/// The hysteresis loop as the AI core's decision policy: each scheduled decision runs [`decide`] with
/// `invalidated = false`. A host that invalidates an option (its target died, its order was cancelled) calls
/// [`decide`] with `invalidated = true` on [`super::AiCore::decision_entry`].
impl DecisionPolicy for Hysteresis {
    type Decision = Decision;

    fn commit(&self, decision: &mut Decision, scores: &[(OptionId, f32)]) {
        decide(self, decision, scores, false);
    }

    fn current(decision: &Decision) -> Option<OptionId> {
        decision.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_all_twelve_options() {
        let opts = catalogue(&Thresholds {
            push_force_ratio: 1.3,
            retreat_hull_pct: 0.2,
            outnumbered_ratio: 2.0,
            aggression: 1.0,
            defence: 1.0,
        });
        assert_eq!(opts.len(), 12);
        let mut ids: Vec<OptionId> = opts.iter().map(|o| o.id).collect();
        ids.sort_by_key(|id| id.bit());
        ids.dedup();
        assert_eq!(ids.len(), 12, "every option id unique");
        // Push and Retreat are present and use the threshold knees.
        assert!(opts.iter().any(|o| o.id == OptionId::Push));
        assert!(opts.iter().any(|o| o.id == OptionId::Retreat));
    }

    #[test]
    fn force_ratio_norm_maps_even_to_half() {
        assert!((force_ratio_norm(1.0) - 0.5).abs() < 1e-6);
        assert!(force_ratio_norm(3.0) > 0.5);
        assert!(force_ratio_norm(0.5) < 0.5);
        assert_eq!(force_ratio_norm(-1.0), 0.0);
    }

    #[test]
    fn decide_commits_the_best_option_on_first_tick() {
        let hyst = Hysteresis::default();
        let mut dec = Decision::default();
        let scores = [
            (OptionId::Push, 0.8),
            (OptionId::Hold, 0.4),
            (OptionId::Retreat, 0.9),
        ];
        let chosen = decide(&hyst, &mut dec, &scores, false);
        assert_eq!(chosen, OptionId::Retreat);
        assert_eq!(dec.current, Some(OptionId::Retreat));
    }

    #[test]
    fn decide_holds_through_a_knife_edge() {
        let hyst = Hysteresis::default();
        let mut dec = Decision {
            current: Some(OptionId::Hold),
            ..Decision::default()
        };
        // A challenger leads by less than the margin: hold.
        let scores = [(OptionId::Hold, 0.5), (OptionId::Push, 0.55)];
        assert_eq!(decide(&hyst, &mut dec, &scores, false), OptionId::Hold);
        // Keep it leading for long enough: switch after min_hold_ticks.
        let scores2 = [(OptionId::Hold, 0.5), (OptionId::Push, 0.7)];
        let mut chosen = OptionId::Hold;
        for _ in 0..hyst.min_hold_ticks {
            chosen = decide(&hyst, &mut dec, &scores2, false);
        }
        assert_eq!(chosen, OptionId::Push, "a sustained lead flips the option");
        assert_eq!(dec.current, Some(OptionId::Push));
    }

    #[test]
    fn decide_switches_immediately_on_invalidation() {
        let hyst = Hysteresis::default();
        let mut dec = Decision {
            current: Some(OptionId::FocusFire),
            ..Decision::default()
        };
        let scores = [(OptionId::FocusFire, 0.2), (OptionId::Retreat, 0.9)];
        assert_eq!(decide(&hyst, &mut dec, &scores, true), OptionId::Retreat);
    }

    #[test]
    fn decide_reevaluates_after_max_hold() {
        let hyst = Hysteresis::default();
        let mut dec = Decision {
            current: Some(OptionId::Hold),
            held_ticks: hyst.max_hold_ticks,
            ..Decision::default()
        };
        let scores = [(OptionId::Hold, 0.5), (OptionId::Push, 0.6)];
        // Even without the margin being held, max_hold forces a re-evaluate.
        assert_eq!(decide(&hyst, &mut dec, &scores, false), OptionId::Push);
    }

    #[test]
    fn the_default_interval_is_the_default_cadence() {
        assert_eq!(Hysteresis::default().cadence(), AiCadence::default());
        assert_eq!(
            Hysteresis {
                decision_interval_ticks: 0,
                ..Hysteresis::default()
            }
            .cadence()
            .decision_ticks(),
            1
        );
    }
}
