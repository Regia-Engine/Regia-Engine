// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/utility.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1-354, plan row S-09). The code is DO27's byte for byte apart from rustfmt and five float
// `clamp`/`max` calls that go through `regia_detsim::float::det_*` (ADR-8085; integ-v06n); doc comments
// were added to every public item (`missing_docs`) and no longer cite DO27's design document; `crate::math`
// is `regia_detsim::math`, the same libm 0.2.16 bits DO27's sim runs.

//! Deterministic utility-AI framework.
//!
//! Infinite-Axis Utility System (IAUS) with a product-rule: considerations
//! flagged `multiplier` multiply in instead of adding, so a hard zero (dry,
//! near-dead) zeroes an option outright. Every curve is a pure function of a
//! normalised 0..1 input; no behaviour tree, no GOAP.

use serde::{Deserialize, Serialize};

use crate::float::{det_clamp, det_max};

/// The closed option set. `bit()` maps each to one bit of a repertoire mask
/// (a `u16` with bit `id.bit()` set for each option a profile may pick).
///
/// The set and its order are Direct Order 27's: the order is the bit index, so
/// a reorder is a sim-version change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptionId {
    /// Close on the enemy.
    Push,
    /// Hold the current position.
    Hold,
    /// Move sideways across the enemy's line of fire.
    Strafe,
    /// Turn fight at close range.
    Dogfight,
    /// Keep the range open while firing.
    Kite,
    /// Fall back.
    Retreat,
    /// Return to the leader or the group.
    Regroup,
    /// Guard a position or an ally.
    Defend,
    /// Gather a resource.
    CollectScrap,
    /// Accompany an ally.
    Escort,
    /// Attack from the side.
    Flank,
    /// Fire on the team's focus target.
    FocusFire,
}

impl OptionId {
    /// Every option, in bit order.
    pub const ALL: [OptionId; 12] = [
        OptionId::Push,
        OptionId::Hold,
        OptionId::Strafe,
        OptionId::Dogfight,
        OptionId::Kite,
        OptionId::Retreat,
        OptionId::Regroup,
        OptionId::Defend,
        OptionId::CollectScrap,
        OptionId::Escort,
        OptionId::Flank,
        OptionId::FocusFire,
    ];

    /// The option's bit index in a repertoire mask.
    pub fn bit(self) -> u32 {
        self as u32
    }
}

/// Curve shapes. `Band` is the bell for range windows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CurveKind {
    /// `m * x`.
    Linear,
    /// `x * x`.
    Quadratic,
    /// `1 - (1 - x)^2`.
    InverseQuadratic,
    /// `1 / (1 + e^(-m (x - k)))`.
    Logistic,
    /// `1 - logistic`.
    InverseLogistic,
    /// `x / k` below the knee `k`, 1 above it.
    Plateau,
    /// A Gaussian bell centred on `b` with half-width `c`.
    Band,
}

/// A response curve mapping a normalised 0..1 input to 0..1.
///
/// `m` = slope (logistic sharpness), `k` = knee (logistic centre), `b` = band
/// centre (fraction), `c` = band half-width (fraction).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResponseCurve {
    /// The curve shape.
    pub kind: CurveKind,
    /// Slope (linear) or sharpness (logistic).
    pub m: f32,
    /// Knee: the logistic centre or the plateau start.
    pub k: f32,
    /// Band centre (fraction).
    pub b: f32,
    /// Band half-width (fraction).
    pub c: f32,
}

impl ResponseCurve {
    /// A linear curve with slope `m`.
    pub fn linear(m: f32) -> Self {
        ResponseCurve {
            kind: CurveKind::Linear,
            m,
            k: 0.0,
            b: 0.0,
            c: 0.0,
        }
    }

    /// `x * x`.
    pub fn quadratic() -> Self {
        ResponseCurve {
            kind: CurveKind::Quadratic,
            m: 0.0,
            k: 0.0,
            b: 0.0,
            c: 0.0,
        }
    }

    /// `1 - (1 - x)^2`.
    pub fn inverse_quadratic() -> Self {
        ResponseCurve {
            kind: CurveKind::InverseQuadratic,
            m: 0.0,
            k: 0.0,
            b: 0.0,
            c: 0.0,
        }
    }

    /// A logistic step of sharpness `m` centred on `k`.
    pub fn logistic(m: f32, k: f32) -> Self {
        ResponseCurve {
            kind: CurveKind::Logistic,
            m,
            k,
            b: 0.0,
            c: 0.0,
        }
    }

    /// One minus a logistic step of sharpness `m` centred on `k`.
    pub fn inverse_logistic(m: f32, k: f32) -> Self {
        ResponseCurve {
            kind: CurveKind::InverseLogistic,
            m,
            k,
            b: 0.0,
            c: 0.0,
        }
    }

    /// A ramp from 0 to 1 that is flat from the knee `k` on.
    pub fn plateau(k: f32) -> Self {
        ResponseCurve {
            kind: CurveKind::Plateau,
            m: 0.0,
            k,
            b: 0.0,
            c: 0.0,
        }
    }

    /// A bell centred on `b` with half-width `c`.
    pub fn band(b: f32, c: f32) -> Self {
        ResponseCurve {
            kind: CurveKind::Band,
            m: 0.0,
            k: 0.0,
            b,
            c,
        }
    }

    /// `curve(x)` in 0..1, clamped; `x` in 0..1 after normalisation.
    /// Deterministic: `crate::math` transcendentals only.
    pub fn evaluate(&self, x: f32) -> f32 {
        let x = det_clamp(x, 0.0, 1.0);
        let v = match self.kind {
            CurveKind::Linear => x * self.m,
            CurveKind::Quadratic => x * x,
            CurveKind::InverseQuadratic => {
                let d = 1.0 - x;
                1.0 - d * d
            }
            CurveKind::Logistic => {
                let t = self.m * (x - self.k);
                1.0 / (1.0 + crate::math::exp(-t))
            }
            CurveKind::InverseLogistic => {
                let t = self.m * (x - self.k);
                1.0 - 1.0 / (1.0 + crate::math::exp(-t))
            }
            CurveKind::Plateau => {
                if x >= self.k {
                    1.0
                } else if self.k <= 0.0 {
                    0.0
                } else {
                    x / self.k
                }
            }
            CurveKind::Band => {
                let d = x - self.b;
                let c = det_max(self.c, 1.0e-4);
                crate::math::exp(-(d * d) / (2.0 * c * c))
            }
        };
        det_clamp(v, 0.0, 1.0)
    }
}

/// Normalised inputs a [`Consideration`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsiderationId {
    /// Health left, 0..1.
    HullPct,
    /// Ammunition left, 0..1.
    AmmoPct,
    /// How far into/out of the weapon band we are.
    RangeBandFrac,
    /// Local advantage, 0.5 = even.
    LocalForceRatio,
    /// Cover between the unit and the threat, 0..1.
    CoverAvailability,
    /// Distance to the leader, as a fraction of a range.
    DistanceToLeaderFrac,
    /// Distance to the nearest threat, as a fraction of a range.
    DistanceToThreatFrac,
    /// 0/1 (or smoothed): a threat has a firing solution on me.
    ThreatIncoming,
    /// How much the commander wants this, 0..1.
    CommanderPriority,
    /// Nearness of a resource to gather, 0..1.
    ScrapProximity,
    /// Distance to an objective point, as a fraction of a range.
    PointDistanceFrac,
    /// 1 if I am/was assigned the coordinator's focus id.
    LeaderTargetFocus,
    /// Movement "energy": speed vs turn rate.
    RelativeEnergyFrac,
    /// Turn rate, as a fraction of a reference rate.
    TurnRateFrac,
    /// Gatherer load fullness.
    HopperFrac,
}

/// One scored input with its curve and weight.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Consideration {
    /// The input read.
    pub input: ConsiderationId,
    /// The curve the input goes through.
    pub curve: ResponseCurve,
    /// The weight of an additive consideration (1 for a multiplier).
    pub weight: f32,
    /// Multiply in (product-rule) instead of adding.
    pub multiplier: bool,
}

impl Consideration {
    /// An additive consideration with weight `weight`.
    pub fn add(input: ConsiderationId, curve: ResponseCurve, weight: f32) -> Self {
        Consideration {
            input,
            curve,
            weight,
            multiplier: false,
        }
    }

    /// A multiplier consideration (the product rule).
    pub fn mult(input: ConsiderationId, curve: ResponseCurve) -> Self {
        Consideration {
            input,
            curve,
            weight: 1.0,
            multiplier: true,
        }
    }
}

/// A decision option: scored considerations plus a
/// `requires` mask (option-id bits it can be combined with; unused for now)
/// and a `min_score` floor below which it is never chosen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiOption {
    /// The option.
    pub id: OptionId,
    /// Its considerations, scored in order.
    pub considerations: Vec<Consideration>,
    /// Option-id bits it can be combined with (unused for now).
    pub requires: u16,
    /// The floor below which it is never chosen.
    pub min_score: f32,
}

impl AiOption {
    /// An option with no `requires` bits and a zero floor.
    pub fn new(id: OptionId, considerations: Vec<Consideration>) -> Self {
        AiOption {
            id,
            considerations,
            requires: 0,
            min_score: 0.0,
        }
    }

    /// The same option with the floor `floor`.
    pub fn with_min_score(mut self, floor: f32) -> Self {
        self.min_score = floor;
        self
    }
}

/// The normalised inputs handed to [`score`]. Each field is the input of the
/// [`ConsiderationId`] of the same name.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inputs {
    /// [`ConsiderationId::HullPct`].
    pub hull_pct: f32,
    /// [`ConsiderationId::AmmoPct`].
    pub ammo_pct: f32,
    /// [`ConsiderationId::RangeBandFrac`].
    pub range_band_frac: f32,
    /// [`ConsiderationId::LocalForceRatio`].
    pub local_force_ratio: f32,
    /// [`ConsiderationId::CoverAvailability`].
    pub cover_availability: f32,
    /// [`ConsiderationId::DistanceToLeaderFrac`].
    pub distance_to_leader_frac: f32,
    /// [`ConsiderationId::DistanceToThreatFrac`].
    pub distance_to_threat_frac: f32,
    /// [`ConsiderationId::ThreatIncoming`].
    pub threat_incoming: f32,
    /// [`ConsiderationId::CommanderPriority`].
    pub commander_priority: f32,
    /// [`ConsiderationId::ScrapProximity`].
    pub scrap_proximity: f32,
    /// [`ConsiderationId::PointDistanceFrac`].
    pub point_distance_frac: f32,
    /// [`ConsiderationId::LeaderTargetFocus`].
    pub leader_target_focus: f32,
    /// [`ConsiderationId::RelativeEnergyFrac`].
    pub relative_energy_frac: f32,
    /// [`ConsiderationId::TurnRateFrac`].
    pub turn_rate_frac: f32,
    /// [`ConsiderationId::HopperFrac`].
    pub hopper_frac: f32,
}

impl Inputs {
    /// The input `id` reads.
    pub fn get(&self, id: ConsiderationId) -> f32 {
        match id {
            ConsiderationId::HullPct => self.hull_pct,
            ConsiderationId::AmmoPct => self.ammo_pct,
            ConsiderationId::RangeBandFrac => self.range_band_frac,
            ConsiderationId::LocalForceRatio => self.local_force_ratio,
            ConsiderationId::CoverAvailability => self.cover_availability,
            ConsiderationId::DistanceToLeaderFrac => self.distance_to_leader_frac,
            ConsiderationId::DistanceToThreatFrac => self.distance_to_threat_frac,
            ConsiderationId::ThreatIncoming => self.threat_incoming,
            ConsiderationId::CommanderPriority => self.commander_priority,
            ConsiderationId::ScrapProximity => self.scrap_proximity,
            ConsiderationId::PointDistanceFrac => self.point_distance_frac,
            ConsiderationId::LeaderTargetFocus => self.leader_target_focus,
            ConsiderationId::RelativeEnergyFrac => self.relative_energy_frac,
            ConsiderationId::TurnRateFrac => self.turn_rate_frac,
            ConsiderationId::HopperFrac => self.hopper_frac,
        }
    }
}

/// `score(option) = Σ w_i · curve_i(norm(input_i))`, normalised to 0..1, with
/// the product-rule: multiplier considerations multiply in instead of adding.
/// Pure and deterministic.
pub fn score(option: &AiOption, inputs: &Inputs) -> f32 {
    let mut add = 0.0;
    let mut add_w = 0.0;
    let mut mult = 1.0;
    for c in &option.considerations {
        let v = c.curve.evaluate(inputs.get(c.input));
        if c.multiplier {
            mult *= det_max(v, 0.0);
        } else {
            add += c.weight * v;
            add_w += c.weight.abs();
        }
    }
    let norm = if add_w > 0.0 { add / add_w } else { 0.0 };
    det_clamp(norm * mult, 0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(id: OptionId, cons: Vec<Consideration>) -> AiOption {
        AiOption::new(id, cons)
    }

    #[test]
    fn linear_curve_maps_endpoints() {
        let c = ResponseCurve::linear(1.0);
        assert!((c.evaluate(0.0)).abs() < 1e-6);
        assert!((c.evaluate(1.0) - 1.0).abs() < 1e-6);
        assert!((c.evaluate(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn logistic_knee_is_at_k() {
        let c = ResponseCurve::logistic(12.0, 0.5);
        assert!((c.evaluate(0.5) - 0.5).abs() < 1e-4);
        assert!(c.evaluate(0.9) > 0.99);
        assert!(c.evaluate(0.1) < 0.01);
    }

    #[test]
    fn band_peaks_at_centre_and_falls_at_edges() {
        let c = ResponseCurve::band(0.5, 0.2);
        assert!((c.evaluate(0.5) - 1.0).abs() < 1e-5);
        assert!(c.evaluate(0.5 + 0.2) < 0.7);
        assert!(c.evaluate(0.0) < 0.1);
    }

    #[test]
    fn plateau_is_flat_above_the_knee() {
        let c = ResponseCurve::plateau(0.6);
        assert_eq!(c.evaluate(0.7), 1.0);
        assert_eq!(c.evaluate(1.0), 1.0);
        assert!((c.evaluate(0.3) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn multiplier_zero_zeroes_the_option() {
        let opt = opt(
            OptionId::Push,
            vec![
                Consideration::add(ConsiderationId::HullPct, ResponseCurve::linear(1.0), 1.0),
                Consideration::mult(ConsiderationId::AmmoPct, ResponseCurve::linear(1.0)),
            ],
        );
        let full = Inputs {
            hull_pct: 1.0,
            ammo_pct: 1.0,
            ..Inputs::default()
        };
        let dry = Inputs {
            hull_pct: 1.0,
            ammo_pct: 0.0,
            ..Inputs::default()
        };
        assert!((score(&opt, &full) - 1.0).abs() < 1e-5);
        assert_eq!(score(&opt, &dry), 0.0, "a dry unit never Pushes");
    }

    #[test]
    fn score_is_bounded_and_deterministic() {
        let opt = opt(
            OptionId::Retreat,
            vec![
                Consideration::add(
                    ConsiderationId::HullPct,
                    ResponseCurve::inverse_logistic(12.0, 0.3),
                    1.0,
                ),
                Consideration::add(
                    ConsiderationId::LocalForceRatio,
                    ResponseCurve::inverse_logistic(8.0, 0.5),
                    1.0,
                ),
            ],
        );
        for i in 0..=20 {
            let x = i as f32 / 20.0;
            let inputs = Inputs {
                hull_pct: x,
                local_force_ratio: 1.0 - x,
                ..Inputs::default()
            };
            let s = score(&opt, &inputs);
            assert!((0.0..=1.0).contains(&s), "score {s} out of range");
            assert_eq!(s, score(&opt, &inputs), "score must be pure");
        }
    }
}
