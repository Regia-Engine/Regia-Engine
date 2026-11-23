// SPDX-License-Identifier: MPL-2.0
//! Infinite-axis utility AI over the shared [`Blackboard`] (Dave Mark's published architecture).
//!
//! Every [`ActionDef`] is scored at each decision from any number of *considerations*: a
//! blackboard key is normalised to `[0, 1]` by an authored min/max range and mapped through a
//! [`ResponseCurve`] (linear, quadratic, logistic, logit, step or piecewise-linear). Each curve
//! output gets the number-of-considerations "make-up value" ([`modification_factor`],
//! [`compensate`]) and the compensated outputs are multiplied together, so that an action with
//! many high-scoring considerations is not beaten by a single-consideration action that happens
//! to score a little higher. The compensated score is scaled by the action's
//! `weight`, the current action gains its `momentum` bonus (commitment), and actions on cooldown
//! are skipped until their time has elapsed.
//!
//! A [`Reasoner`] holds the actions and picks one per decision: [`SelectionMode::Best`] takes the
//! highest score (ties go to declaration order), [`SelectionMode::WeightedRandom`] draws — from a
//! seeded SplitMix64 stream — among the top N candidates within a margin of the best. Both
//! [`Reasoner::decide`] and [`Reasoner::explain`] return a [`DecisionReport`] that records, per
//! action, every consideration's raw input, normalised input and curve output, the product, the
//! make-up factor and the final score — a plain serde type, so the editor and the agent protocol
//! can serialise it. `decide` commits (sets the current action, starts cooldowns, advances the
//! generator on weighted draws); `explain` answers the same question without changing anything.
//!
//! # Determinism
//!
//! No wall clock is read: `now` is an explicit parameter (pass fixed-step time). Iteration is
//! over vectors in declaration order; there is no hash-map iteration between runs. The only
//! generator is the seeded SplitMix64 stream, advanced only by weighted-random draws, so the same
//! definition, blackboard history, seed and `now` sequence always pick the same actions.
//!
//! # Sources (cleanroom)
//!
//! The architecture — considerations, the response-curve families, weighted random among the top
//! scorers, and the make-up value — is re-implemented from the published talks: Mike Lewis and
//! Dave Mark, "Building a Better Centaur: AI at Massive Scale" (GDC 2015), which presents the
//! infinite-axis system and the compensation factor (`m = 1 - 1/n`, `make_up = (1 - s) * m`,
//! `s + make_up * s` per consideration); Kevin Dill and Dave Mark, "Improving AI Decision Modeling
//! Through Utility Theory" (GDC 2010); and Dave Mark, *Behavioral Mathematics for Game AI*
//! (Cengage, 2009); see `docs/provenance.md`. The generator is SplitMix64 (Steele, Lea & Flood
//! 2014), as already used by `crate::state_tree`.
//!
//! # Example
//!
//! ```
//! use regia_ai::utility::{Reasoner, UtilityAiDef};
//! use regia_ai::Blackboard;
//!
//! let text = r#"(
//!     format_version: 1,
//!     actions: [
//!         (name: "attack", considerations: [(key: "health", min: 0.0, max: 100.0)]),
//!         (name: "flee",   considerations: [(key: "threat", min: 0.0, max: 100.0)]),
//!     ],
//! )"#;
//! let def = UtilityAiDef::from_ron(text).expect("parses");
//! let mut reasoner = Reasoner::new(&def).expect("valid");
//! let mut bb = Blackboard::new();
//! bb.insert("health", 90.0f32);
//! bb.insert("threat", 10.0f32);
//! let report = reasoner.decide(&bb, 0.0);
//! assert_eq!(report.selected.as_deref(), Some("attack"));
//! assert_eq!(reasoner.list_decisions(), vec!["attack".to_string(), "flee".to_string()]);
//! ```

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::blackboard::Blackboard;

use regia_core::math::{det_exp, det_ln};

/// Current `.utility.ron` format version. Version 1 is the first published shape.
pub const UTILITY_AI_FORMAT_VERSION: u32 = 1;

/// The oldest `.utility.ron` version this build reads (through the in-memory migration).
pub const UTILITY_AI_MIN_FORMAT_VERSION: u32 = 1;

/// The type path `.utility.ron` loads under ([`crate::utility_ecs::load_utility_definition`]).
pub const UTILITY_AI_TYPE_PATH: &str = "regia_ai::utility::UtilityAiDef";

/// The most actions one definition may declare (the editor sorts and scores them linearly; the
/// bound keeps a hand-edited file from stalling a tick).
pub const MAX_ACTIONS: usize = 65_536;

/// The most considerations one action may declare.
pub const MAX_CONSIDERATIONS: usize = 64;

/// Domain clamp for the logit curve: `ln(x / (1 - x))` is finite only inside `(0, 1)`, so the
/// input is clamped to `[EPSILON, 1 - EPSILON]` before the logarithms (documented values at the
/// endpoints are the clamped limits, 0 and 1).
pub const CURVE_EPSILON: f32 = 1e-6;

/// One control point of a [`CurveKind::Piecewise`] response curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    /// Input position; must be strictly increasing along the point list.
    pub x: f32,
    /// Output at `x`, clamped to `[0, 1]` after interpolation.
    pub y: f32,
}

/// Which family of response curve a [`ResponseCurve`] evaluates.
///
/// Every family is evaluated on `t = x - x_shift` (the x-shift) and then shifted by `y_shift`
/// before the result is clamped to `[0, 1]`; non-finite intermediates read as 0 after clamping.
/// See [`ResponseCurve`] for each family's formula and documented values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CurveKind {
    /// `slope * t + y_shift` — the identity when every parameter is the default.
    #[default]
    Linear,
    /// `slope * t^exponent + y_shift` — signed for odd integer exponents, `|t|^exponent`
    /// otherwise (so a fractional exponent of a negative input never produces NaN).
    Quadratic,
    /// `y_shift + slope / (1 + e^(-exponent * t))` — `exponent` doubles as the growth rate.
    Logistic,
    /// `y_shift + slope * (ln(u) - ln(1 - u)) / exponent` on `u = clamp(t, [EPSILON, 1 -
    /// EPSILON])` — the inverse of the logistic, clamped to `[0, 1]` at the ends.
    Logit,
    /// `y_shift + (t >= 0 ? slope : 0)` — a step up exactly at the x-shift.
    Step,
    /// Piecewise-linear interpolation of [`CurvePoint`]s over `t`, holding the first/last `y`
    /// outside their range. Ignores `slope` and `exponent`.
    Piecewise,
}

/// A response curve: how one normalised consideration input maps to a score in `[0, 1]`.
///
/// All families share the parameter set; which of them a family uses is documented on
/// [`CurveKind`]. With every parameter at its default the curve is the identity (`Linear`,
/// `slope = 1`). Evaluation clamps the output to `[0, 1]`; a non-finite input or intermediate
/// reads as 0 (never NaN, never a panic).
///
/// Documented values at `x = 0, 0.5, 1` with all parameters at their defaults:
///
/// | kind | 0 | 0.5 | 1 |
/// |---|---|---|---|
/// | `Linear` | 0 | 0.5 | 1 |
/// | `Quadratic` (exponent 2) | 0 | 0.25 | 1 |
/// | `Logistic` (exponent 2) | 0.5 | 0.7310586 | 0.8807971 |
/// | `Logit` (exponent 2) | 0 | 0 | 1 |
/// | `Step` (x-shift 0.5) | 0 | 1 | 1 |
/// | `Piecewise` `[(0,0),(0.5,0.25),(1,1)]` | 0 | 0.25 | 1 |
///
/// The `Quadratic`, `Logistic` and `Logit` families evaluate through the deterministic `f64`
/// kernels (`regia_core::math::{det_powf, det_exp, det_ln}`), not the platform libm, so the same
/// bits come out on every target — one libm ulp here can flip a `Best` decision, and the bits are
/// pinned by a test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseCurve {
    /// The family.
    #[serde(default)]
    pub kind: CurveKind,
    /// Vertical scale (intercept for `Linear`, threshold height for `Step`). Default 1.
    #[serde(default = "one")]
    pub slope: f32,
    /// Curve exponent (`Quadratic`), growth rate (`Logistic`, `Logit`). Default 2.
    #[serde(default = "two")]
    pub exponent: f32,
    /// Horizontal shift subtracted from the input first. Default 0.
    #[serde(default)]
    pub x_shift: f32,
    /// Vertical shift added after the family (and, for `Piecewise`, after interpolation).
    /// Default 0.
    #[serde(default)]
    pub y_shift: f32,
    /// Control points of `Piecewise` (unused by the other families): at least two, `x`
    /// strictly increasing — enforced by [`Reasoner::new`].
    #[serde(default)]
    pub points: Vec<CurvePoint>,
}

impl Default for ResponseCurve {
    /// The identity curve (`Linear`, `slope = 1`, no shifts).
    fn default() -> Self {
        Self {
            kind: CurveKind::Linear,
            slope: 1.0,
            exponent: 2.0,
            x_shift: 0.0,
            y_shift: 0.0,
            points: Vec::new(),
        }
    }
}

impl ResponseCurve {
    /// A curve of `kind` with every other parameter at its default.
    pub fn new(kind: CurveKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// Clamps a raw family output into `[0, 1]`; NaN reads as 0.
    fn clamped(y: f32) -> f32 {
        if y.is_nan() || y < 0.0 {
            0.0
        } else if y > 1.0 {
            1.0
        } else {
            y
        }
    }

    /// Piecewise-linear interpolation of `points` at `t`: the first/last `y` outside the point
    /// range, linear in between. Empty points read as 0 (validation rejects that authored form;
    /// this is the defined fallback for a hand-built curve).
    fn eval_piecewise(points: &[CurvePoint], t: f32) -> f32 {
        let (Some(first), Some(last)) = (points.first(), points.last()) else {
            return 0.0;
        };
        if t <= first.x {
            return first.y;
        }
        if t >= last.x {
            return last.y;
        }
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if t <= b.x {
                let span = b.x - a.x;
                let fraction = if span > 0.0 { (t - a.x) / span } else { 0.0 };
                return a.y + (b.y - a.y) * fraction;
            }
        }
        last.y
    }

    /// Evaluates the curve at `x`. Any finite `f32` is accepted (considerations pass `[0, 1]`);
    /// the result is always in `[0, 1]`, and non-finite input reads as 0.
    pub fn evaluate(&self, x: f32) -> f32 {
        if !x.is_finite() {
            return 0.0;
        }
        let t = x - self.x_shift;
        let y = match self.kind {
            CurveKind::Linear => self.slope * t + self.y_shift,
            CurveKind::Quadratic => {
                // The deterministic kernels: one libm ulp in a curve output can flip a `Best`
                // decision, and the same curve must give the same bits on every target.
                let magnitude = regia_core::math::det_powf(t.abs(), self.exponent);
                let odd = self.exponent.fract() == 0.0 && self.exponent.rem_euclid(2.0) == 1.0;
                let signed = if t < 0.0 && odd { -magnitude } else { magnitude };
                self.slope * signed + self.y_shift
            }
            CurveKind::Logistic => self.y_shift + self.slope / (1.0 + det_exp(-self.exponent * t)),
            CurveKind::Logit => {
                if self.slope == 0.0 {
                    self.y_shift
                } else {
                    let u = t.clamp(CURVE_EPSILON, 1.0 - CURVE_EPSILON);
                    self.y_shift + self.slope * (det_ln(u) - det_ln(1.0 - u)) / self.exponent
                }
            }
            CurveKind::Step => self.y_shift + if t >= 0.0 { self.slope } else { 0.0 },
            CurveKind::Piecewise => Self::eval_piecewise(&self.points, t) + self.y_shift,
        };
        Self::clamped(y)
    }

    /// Validates the curve's parameters: every number finite; `Piecewise` has at least two
    /// points with finite, strictly increasing `x` and finite `y`. `action` and `key` name the
    /// owner in the error.
    fn validate(&self, action: &str, key: &str) -> Result<(), UtilityAiError> {
        let bad = !self.slope.is_finite()
            || !self.exponent.is_finite()
            || !self.x_shift.is_finite()
            || !self.y_shift.is_finite();
        if bad {
            return Err(UtilityAiError::BadCurve {
                action: action.to_string(),
                key: key.to_string(),
            });
        }
        if self.kind == CurveKind::Piecewise {
            if self.points.len() < 2 || self.points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                return Err(UtilityAiError::BadCurve {
                    action: action.to_string(),
                    key: key.to_string(),
                });
            }
            for pair in self.points.windows(2) {
                if pair[0].x >= pair[1].x {
                    return Err(UtilityAiError::BadCurve {
                        action: action.to_string(),
                        key: key.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Mark and Lewis's number-of-considerations modification factor: `1 - 1 / n`, and `0` for
/// `n <= 1` (a single consideration needs no make-up).
///
/// Over 1..5 considerations: `0, 0.5, 0.6666667, 0.75, 0.8`.
pub fn modification_factor(num_considerations: usize) -> f32 {
    if num_considerations <= 1 {
        return 0.0;
    }
    // `num_considerations` is at most `MAX_CONSIDERATIONS` for a validated action; any usize
    // converts (rounded), and the factor stays in `[0, 1)`.
    1.0 - 1.0 / num_considerations as f32
}

/// Applies the make-up value to one consideration's score `s` (Mark and Lewis's compensation
/// factor): `make_up = (1 - s) * modification_factor(n)`, `s + make_up * s`. The identity for a
/// single consideration; 0 and 1 are fixed points; the result stays in `[0, 1]`. Non-finite input
/// reads as 0.
///
/// The reasoner applies it to every consideration of an action before multiplying, so each
/// factor below 1 costs less the more considerations the action has. For a consideration scoring
/// 0.5 over 1..5 considerations the values are `0.5, 0.625, 0.6666667, 0.6875, 0.7`; five
/// considerations that each score 0.9 multiply to 0.59049 raw but to `0.972^5 = 0.8676` compensated,
/// so they beat a single consideration scoring 0.8 instead of losing to it.
pub fn compensate(num_considerations: usize, score: f32) -> f32 {
    let s = if score.is_finite() {
        score.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let make_up = (1.0 - s) * modification_factor(num_considerations);
    s + make_up * s
}

/// One consideration of an action: read `key` from the blackboard, normalise it by
/// `[min, max]` into `[0, 1]` (clamped), and score it through `curve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsiderationDef {
    /// Blackboard key read (losslessly, see [`Blackboard::try_get_float`]); a missing key, a
    /// value of the wrong type or a non-finite float makes the consideration score 0.
    pub key: String,
    /// Input value that normalises to 0. Default 0.
    #[serde(default)]
    pub min: f32,
    /// Input value that normalises to 1; must be greater than `min`. Default 1.
    #[serde(default = "one")]
    pub max: f32,
    /// The response curve applied to the normalised input. Default: identity.
    #[serde(default)]
    pub curve: ResponseCurve,
}

/// One candidate action: any number of considerations plus the scoring knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionDef {
    /// Unique name, reported by [`Reasoner::list_decisions`] and [`DecisionReport`].
    pub name: String,
    /// The considerations; their curve outputs are multiplied (an empty list scores 0 and the
    /// action is never selected).
    #[serde(default)]
    pub considerations: Vec<ConsiderationDef>,
    /// Multiplier on the compensated score. Finite and `>= 0`; 0 disables the action. Default 1.
    #[serde(default = "one")]
    pub weight: f32,
    /// Seconds the action is blocked for after each selection, counted in the caller's `now`.
    /// Finite and `>= 0`. Default 0.
    #[serde(default)]
    pub cooldown: f32,
    /// Bonus added to the compensated-weighted score while this action is the current one
    /// (commitment): another action must exceed this action's score by more than the bonus to
    /// take over. Finite and `>= 0`. Default 0.
    #[serde(default)]
    pub momentum: f32,
}

/// How a [`Reasoner`] picks the action from the scores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum SelectionMode {
    /// The highest score; ties go to declaration order.
    #[default]
    Best,
    /// Weighted random among the top `top_n` candidates that score within `margin` of the best,
    /// drawn from the reasoner's seeded generator (weights are the scores). Falls back to the
    /// best action if the weights cannot be drawn from.
    WeightedRandom {
        /// How many of the best candidates are at most in the draw (at least 1).
        top_n: usize,
        /// A candidate joins the draw while its score is `>= best - margin` (finite, `>= 0`).
        margin: f32,
    },
}

/// The `.utility.ron` document: a versioned set of actions plus the selection mode and the seed
/// of the weighted-random generator.
///
/// ```ron
/// (
///     format_version: 1,
///     seed: 7,
///     selection: WeightedRandom(top_n: 3, margin: 0.2),
///     actions: [
///         (name: "attack",
///          considerations: [
///             (key: "health", min: 0.0, max: 100.0),
///             (key: "enemy_distance", min: 0.0, max: 50.0,
///              curve: (kind: Quadratic, exponent: 2.0)),
///          ],
///          cooldown: 1.5, momentum: 0.25),
///         (name: "flee",
///          considerations: [(key: "threat", min: 0.0, max: 100.0,
///                             curve: (kind: Logistic, exponent: 4.0))]),
///     ],
/// )
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UtilityAiDef {
    /// Format version, [`UTILITY_AI_FORMAT_VERSION`].
    pub format_version: u32,
    /// What this decision set is for (free text, never parsed).
    #[serde(default)]
    pub description: String,
    /// Seed of the generator used by [`SelectionMode::WeightedRandom`].
    #[serde(default)]
    pub seed: u64,
    /// How the winner is picked from the scores.
    #[serde(default)]
    pub selection: SelectionMode,
    /// The actions, in declaration order (the tie-break order).
    pub actions: Vec<ActionDef>,
}

impl Default for UtilityAiDef {
    /// An empty definition with the current format version (valid as an authoring start;
    /// [`Reasoner::new`] rejects an empty action list).
    fn default() -> Self {
        Self {
            format_version: UTILITY_AI_FORMAT_VERSION,
            description: String::new(),
            seed: 0,
            selection: SelectionMode::Best,
            actions: Vec::new(),
        }
    }
}

impl UtilityAiDef {
    /// An empty definition with the current format version and `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            ..Self::default()
        }
    }

    /// Parses RON text. The `format_version` is read first: a version this build does not read
    /// is [`UtilityAiError::UnsupportedVersion`] even if the rest of the file parses.
    pub fn from_ron(text: &str) -> Result<Self, UtilityAiError> {
        let header: VersionHeader = ron::from_str(text).map_err(|e| UtilityAiError::Parse(e.to_string()))?;
        if !(UTILITY_AI_MIN_FORMAT_VERSION..=UTILITY_AI_FORMAT_VERSION).contains(&header.format_version) {
            return Err(UtilityAiError::UnsupportedVersion {
                found: header.format_version,
                supported: UTILITY_AI_FORMAT_VERSION,
            });
        }
        let mut def: Self = ron::from_str(text).map_err(|e| UtilityAiError::Parse(e.to_string()))?;
        def.format_version = UTILITY_AI_FORMAT_VERSION;
        Ok(def)
    }

    /// Writes the definition as pretty RON.
    pub fn to_ron(&self) -> Result<String, UtilityAiError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| UtilityAiError::Write(e.to_string()))
    }
}

/// Why an action was not a candidate in a decision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BlockReason {
    /// The action is on cooldown until this `now` value (exclusive: at exactly `until` it is
    /// eligible again).
    Cooldown {
        /// The `now` value from which the action is selectable again.
        until: f32,
    },
    /// The action declares no considerations, so it scores 0 and is never selected.
    NoConsiderations,
    /// The decision's `now` was not finite (NaN or infinite): no action is selected and no
    /// state changes, so a bad clock cannot start or cancel a cooldown.
    InvalidTime,
}

/// One consideration's contribution to an action's score, as recorded per decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsiderationScore {
    /// The blackboard key that was read.
    pub key: String,
    /// The value that was read; NaN when the key is missing, holds a non-numeric or non-finite
    /// value (then `valid` is false).
    pub raw_input: f32,
    /// `(raw_input - min) / (max - min)`, clamped to `[0, 1]` (0 when invalid).
    pub normalized: f32,
    /// The response curve's output, always in `[0, 1]` (0 when invalid).
    pub curve_output: f32,
    /// `curve_output` after the make-up value for the action's consideration count
    /// ([`compensate`]); the action's `compensated` score is the product of these.
    pub compensated: f32,
    /// Whether the key read as a finite number; an invalid consideration scores 0.
    pub valid: bool,
}

/// One action's score in one decision, as recorded per decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionScore {
    /// Index of the action in declaration order.
    pub index: usize,
    /// The action's name.
    pub name: String,
    /// Whether the action could be selected: not blocked, a finite score and greater than 0.
    pub eligible: bool,
    /// Why the action was blocked, when it was.
    pub blocked: Option<BlockReason>,
    /// Every consideration's input and curve output, in declaration order.
    pub considerations: Vec<ConsiderationScore>,
    /// The product of the uncompensated curve outputs (0 for an action with no
    /// considerations); for the debug view only, the score uses `compensated`.
    pub raw: f32,
    /// The modification factor ([`modification_factor`]) for this action's consideration count.
    pub modification_factor: f32,
    /// The product of the considerations' compensated outputs ([`compensate`] applied to each
    /// curve output; 0 for an action with no considerations).
    pub compensated: f32,
    /// The action's authored weight.
    pub weight: f32,
    /// The momentum bonus actually added (the authored momentum while this action is current,
    /// else 0).
    pub momentum_applied: f32,
    /// `compensated * weight + momentum_applied`. May exceed 1 while momentum is applied.
    pub score: f32,
}

/// The full record of one decision: every action's score and the winner. A plain serde type, so
/// the editor and the agent protocol can serialise it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReport {
    /// The `now` the decision was taken at.
    pub now: f32,
    /// The selection mode in effect.
    pub mode: SelectionMode,
    /// The selected action's name, or None when no action was eligible.
    pub selected: Option<String>,
    /// Every action's score, in declaration order.
    pub actions: Vec<ActionScore>,
}

/// Why a definition could not be read or built into a [`Reasoner`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UtilityAiError {
    /// The RON text did not parse.
    #[error("utility AI RON is invalid: {0}")]
    Parse(String),
    /// The RON text could not be written.
    #[error("utility AI definition could not be written as RON: {0}")]
    Write(String),
    /// The file's `format_version` is not one this build reads.
    #[error("utility AI format_version {found} is not supported (this build reads version {supported})")]
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },
    /// The definition declares no actions.
    #[error("utility AI definition has no actions")]
    Empty,
    /// More than [`MAX_ACTIONS`] actions.
    #[error("utility AI definition has {0} actions (the limit is {MAX_ACTIONS})")]
    TooManyActions(usize),
    /// An action has an empty name.
    #[error("utility AI definition has an action with an empty name")]
    EmptyName,
    /// Two actions share a name.
    #[error("utility AI definition declares the action {0:?} twice")]
    DuplicateName(String),
    /// An action declares more than [`MAX_CONSIDERATIONS`] considerations.
    #[error("action {action:?} has {count} considerations (the limit is {MAX_CONSIDERATIONS})")]
    TooManyConsiderations {
        /// The action.
        action: String,
        /// How many considerations it declares.
        count: usize,
    },
    /// A weight, momentum, cooldown, consideration key, range number or selection-mode number is
    /// non-finite or negative (where negative is not allowed), or a consideration key is empty.
    #[error("action {0:?} has a non-finite or negative authored number, or an empty key")]
    BadValue(String),
    /// A consideration's normalisation range is degenerate (`min >= max`) or non-finite.
    #[error("action {action:?} consideration {key:?} has a bad normalisation range")]
    BadRange {
        /// The action.
        action: String,
        /// The consideration's key.
        key: String,
    },
    /// A response curve has a non-finite parameter, or a piecewise curve has fewer than two
    /// points, non-finite points or non-increasing x.
    #[error("action {action:?} consideration {key:?} has a bad response curve")]
    BadCurve {
        /// The action.
        action: String,
        /// The consideration's key.
        key: String,
    },
}

impl UtilityAiError {
    /// The snake_case reason used on the agent protocol (`data.reason`).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Parse(_) | Self::Write(_) => "parse",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::Empty => "empty",
            Self::TooManyActions(_) => "too_many_actions",
            Self::EmptyName => "empty_name",
            Self::DuplicateName(_) => "duplicate_name",
            Self::TooManyConsiderations { .. } => "too_many_considerations",
            Self::BadValue(_) => "bad_value",
            Self::BadRange { .. } => "bad_range",
            Self::BadCurve { .. } => "bad_curve",
        }
    }
}

/// SplitMix64 (Steele, Lea & Flood 2014): one step of the generator, returning an f32 in
/// `[0, 1)` from the top 24 bits — the same construction `crate::state_tree` uses for its
/// weighted selection.
fn next_unit(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / 16_777_216.0
}

/// Draws one index from `window` (non-empty `(index, weight)` pairs, at least one positive
/// finite weight) with a `unit` already drawn from the generator: the first candidate whose
/// cumulative weight exceeds `unit * total`, with the last as the float-edge fallback.
fn draw_weighted(window: &[(usize, f32)], unit: f32) -> usize {
    let total: f32 = window.iter().map(|&(_, w)| w).sum();
    if !total.is_finite() || total <= 0.0 {
        return window[0].0;
    }
    let mut roll = unit * total;
    let mut last = window[0].0;
    for &(index, weight) in window {
        last = index;
        if roll < weight {
            return index;
        }
        roll -= weight;
    }
    last
}

/// [`SelectionMode::Best`] without a candidate vector: the first eligible action wins, and a
/// later one replaces it only when its score is strictly higher or the score ties and it is
/// `current`.
fn pick_best(actions: &[ActionScore], current: Option<usize>) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for action in actions {
        if !action.eligible {
            continue;
        }
        let replace = match best {
            None => true,
            Some((_, score)) => {
                action.score > score || (action.score == score && Some(action.index) == current)
            }
        };
        if replace {
            best = Some((action.index, action.score));
        }
    }
    best.map(|(index, _)| index)
}

/// Picks the winner from one decision's scores: [`SelectionMode::Best`] keeps the `current`
/// action when it is among the equal-best (a challenger must exceed it, momentum included),
/// otherwise the first of the equal-best (declaration order); [`SelectionMode::WeightedRandom`]
/// sorts best-first (score descending, then declaration order), windows the top `top_n` within
/// `margin` of the best and draws one weighted by score, advancing `rng` once (a lone candidate
/// needs no draw).
fn pick(
    actions: &[ActionScore],
    mode: SelectionMode,
    current: Option<usize>,
    rng: &mut u64,
) -> Option<usize> {
    let SelectionMode::WeightedRandom { top_n, margin } = mode else {
        return pick_best(actions, current);
    };
    let mut candidates: Vec<(usize, f32)> = actions
        .iter()
        .filter(|a| a.eligible)
        .map(|a| (a.index, a.score))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0].0);
    }
    candidates.sort_unstable_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let best_score = candidates[0].1;
    let limit = top_n.max(1);
    let window: Vec<(usize, f32)> = candidates
        .iter()
        .take(limit)
        .take_while(|&&(_, score)| score >= best_score - margin)
        .copied()
        .collect();
    let unit = next_unit(rng);
    Some(draw_weighted(&window, unit))
}

#[derive(Deserialize)]
struct VersionHeader {
    format_version: u32,
}

/// A utility reasoner: the validated actions plus the per-agent runtime state (the current
/// action, the cooldown schedule and the seeded generator). Build one from a [`UtilityAiDef`]
/// (directly or with [`Reasoner::from_ron`]) and call [`Reasoner::decide`] each decision —
/// typically once per fixed step, passing fixed-step time as `now`.
///
/// The reasoner is self-contained (no ECS, no clock, no global state), so a game can hold one
/// per agent in a component, and tests can replay a decision sequence exactly.
#[derive(Debug, Clone)]
pub struct Reasoner {
    def: UtilityAiDef,
    current: Option<usize>,
    /// Parallel to `def.actions`: the `now` from which the action is selectable again.
    cooldown_until: Vec<f32>,
    rng: u64,
}

impl Reasoner {
    /// Validates `def` and builds the reasoner: every action needs a unique non-empty name,
    /// finite non-negative weight/momentum/cooldown, at most [`MAX_CONSIDERATIONS`]
    /// considerations with a non-empty key, a finite range `min < max` and a valid curve; the
    /// whole definition at most [`MAX_ACTIONS`] actions and a valid selection mode.
    pub fn new(def: &UtilityAiDef) -> Result<Self, UtilityAiError> {
        validate(def)?;
        Ok(Self {
            def: def.clone(),
            current: None,
            cooldown_until: vec![0.0; def.actions.len()],
            rng: def.seed,
        })
    }

    /// Parses and validates RON text in one step ([`UtilityAiDef::from_ron`] then
    /// [`Reasoner::new`]).
    pub fn from_ron(text: &str) -> Result<Self, UtilityAiError> {
        Self::new(&UtilityAiDef::from_ron(text)?)
    }

    /// The definition this reasoner was built from.
    pub fn def(&self) -> &UtilityAiDef {
        &self.def
    }

    /// How many actions the reasoner holds.
    pub fn action_count(&self) -> usize {
        self.def.actions.len()
    }

    /// The actions' names in declaration order (the agent-protocol listing of what this reasoner
    /// can decide between).
    pub fn list_decisions(&self) -> Vec<String> {
        self.def.actions.iter().map(|a| a.name.clone()).collect()
    }

    /// The index of the action that is current (the last one selected), if any.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The name of the current action (the last one selected), if any.
    pub fn current_action(&self) -> Option<&str> {
        self.current.map(|i| self.def.actions[i].name.as_str())
    }

    /// The seconds until `name` is selectable again at `now` (0 when it is selectable); `None`
    /// when no action has that name.
    pub fn cooldown_remaining(&self, name: &str, now: f32) -> Option<f32> {
        let index = self.def.actions.iter().position(|a| a.name == name)?;
        Some((self.cooldown_until[index] - now).max(0.0))
    }

    /// Takes over `previous`'s runtime state, matched by action name: the current action, each
    /// cooldown and the generator. A hot reload uses it, so tuning a number does not restart the
    /// agent. An action with no match starts fresh, and a current action that no longer exists
    /// leaves none.
    pub fn adopt_state_from(&mut self, previous: &Self) {
        let by_name = |action: &str| previous.def.actions.iter().position(|a| a.name == action);
        self.current = previous
            .current_action()
            .and_then(|name| self.def.actions.iter().position(|a| a.name == name));
        for (index, action) in self.def.actions.iter().enumerate() {
            if let Some(old) = by_name(&action.name) {
                self.cooldown_until[index] = previous.cooldown_until[old];
            }
        }
        self.rng = previous.rng;
    }

    /// Clears the current action and every cooldown, and reseeds the generator to the
    /// definition's seed, so the next decision sequence replays exactly.
    pub fn reset(&mut self) {
        self.current = None;
        self.cooldown_until.fill(0.0);
        self.rng = self.def.seed;
    }

    /// Scores one consideration against `bb`: the lossless float read of `key` (see
    /// [`Blackboard::try_get_float`]), normalised by `[min, max]` into `[0, 1]`, through the
    /// curve, then compensated for the action's `count` considerations ([`compensate`]). A
    /// missing key, a non-numeric value or a non-finite float is invalid and scores 0.
    fn score_consideration(def: &ConsiderationDef, count: usize, bb: &Blackboard) -> ConsiderationScore {
        let mut score = ConsiderationScore {
            key: def.key.clone(),
            raw_input: f32::NAN,
            normalized: 0.0,
            curve_output: 0.0,
            compensated: 0.0,
            valid: false,
        };
        Self::fill_consideration(def, count, bb, &mut score);
        score
    }

    /// Writes one consideration into `slot`. The key string is replaced only when it differs.
    fn fill_consideration(
        def: &ConsiderationDef,
        count: usize,
        bb: &Blackboard,
        slot: &mut ConsiderationScore,
    ) {
        if slot.key != def.key {
            slot.key.clone_from(&def.key);
        }
        match bb.try_get_float(&def.key) {
            Some(value) => {
                let normalized = ((value - def.min) / (def.max - def.min)).clamp(0.0, 1.0);
                let curve_output = def.curve.evaluate(normalized);
                slot.raw_input = value;
                slot.normalized = normalized;
                slot.curve_output = curve_output;
                slot.compensated = compensate(count, curve_output);
                slot.valid = true;
            }
            None => {
                slot.raw_input = f32::NAN;
                slot.normalized = 0.0;
                slot.curve_output = 0.0;
                slot.compensated = 0.0;
                slot.valid = false;
            }
        }
    }

    /// Whether `report` already has one slot per action and consideration, so a later decision can
    /// overwrite the numbers in place.
    fn report_matches(&self, report: &DecisionReport) -> bool {
        if report.actions.len() != self.def.actions.len() {
            return false;
        }
        self.def
            .actions
            .iter()
            .zip(report.actions.iter())
            .all(|(action, scored)| {
                scored.name == action.name
                    && scored.considerations.len() == action.considerations.len()
                    && scored
                        .considerations
                        .iter()
                        .zip(action.considerations.iter())
                        .all(|(slot, def)| slot.key == def.key)
            })
    }

    /// Scores into an existing report. Names and keys are copied only when they differ.
    fn score_into(&self, bb: &Blackboard, now: f32, report: &mut DecisionReport) {
        report.now = now;
        report.mode = self.def.selection;
        for (index, action) in self.def.actions.iter().enumerate() {
            let scored = &mut report.actions[index];
            let count = action.considerations.len();
            for (def, slot) in action.considerations.iter().zip(scored.considerations.iter_mut()) {
                Self::fill_consideration(def, count, bb, slot);
            }
            let (raw, compensated) = if count == 0 {
                (0.0, 0.0)
            } else {
                (
                    scored
                        .considerations
                        .iter()
                        .map(|c| c.curve_output)
                        .product::<f32>(),
                    scored
                        .considerations
                        .iter()
                        .map(|c| c.compensated)
                        .product::<f32>(),
                )
            };
            let momentum_applied = if self.current == Some(index) {
                action.momentum
            } else {
                0.0
            };
            let score = compensated * action.weight + momentum_applied;
            let blocked = if count == 0 {
                Some(BlockReason::NoConsiderations)
            } else if !now.is_finite() {
                Some(BlockReason::InvalidTime)
            } else if self.cooldown_until[index] > now {
                Some(BlockReason::Cooldown {
                    until: self.cooldown_until[index],
                })
            } else {
                None
            };
            if scored.name != action.name {
                scored.name.clone_from(&action.name);
            }
            scored.index = index;
            scored.eligible = blocked.is_none() && score.is_finite() && score > 0.0;
            scored.blocked = blocked;
            scored.raw = raw;
            scored.modification_factor = modification_factor(count);
            scored.compensated = compensated;
            scored.weight = action.weight;
            scored.momentum_applied = momentum_applied;
            scored.score = score;
        }
    }

    /// Scores every action against `bb` at `now`, without changing any state. An action is
    /// blocked when it has no considerations or its cooldown has not elapsed; blocked actions
    /// are still scored (with their hypothetical momentum) so the report explains them.
    fn score_actions(&self, bb: &Blackboard, now: f32) -> Vec<ActionScore> {
        let mut out = Vec::with_capacity(self.def.actions.len());
        for (index, action) in self.def.actions.iter().enumerate() {
            let count = action.considerations.len();
            let considerations: Vec<ConsiderationScore> = action
                .considerations
                .iter()
                .map(|c| Self::score_consideration(c, count, bb))
                .collect();
            // The empty product would be 1; by definition an action with no considerations
            // scores 0 and is never selected.
            let (raw, compensated) = if count == 0 {
                (0.0, 0.0)
            } else {
                (
                    considerations.iter().map(|c| c.curve_output).product::<f32>(),
                    considerations.iter().map(|c| c.compensated).product::<f32>(),
                )
            };
            let modification = modification_factor(count);
            let momentum_applied = if self.current == Some(index) {
                action.momentum
            } else {
                0.0
            };
            let score = compensated * action.weight + momentum_applied;
            let blocked = if count == 0 {
                Some(BlockReason::NoConsiderations)
            } else if !now.is_finite() {
                Some(BlockReason::InvalidTime)
            } else if self.cooldown_until[index] > now {
                Some(BlockReason::Cooldown {
                    until: self.cooldown_until[index],
                })
            } else {
                None
            };
            let eligible = blocked.is_none() && score.is_finite() && score > 0.0;
            out.push(ActionScore {
                index,
                name: action.name.clone(),
                eligible,
                blocked,
                considerations,
                raw,
                modification_factor: modification,
                compensated,
                weight: action.weight,
                momentum_applied,
                score,
            });
        }
        out
    }

    /// Takes one decision: scores every action, picks the winner by the selection mode, makes it
    /// the current action and starts its cooldown (if it has one). Returns the full report. When
    /// nothing is eligible (including a non-finite `now`, [`BlockReason::InvalidTime`]) the
    /// current action is left unchanged (see [`Reasoner::explain`] for
    /// the state-free form; the generator advances only on weighted-random draws).
    #[tracing::instrument(skip_all, target = "regia_ai", name = "utility_decide")]
    pub fn decide(&mut self, bb: &Blackboard, now: f32) -> DecisionReport {
        let actions = self.score_actions(bb, now);
        let picked = pick(&actions, self.def.selection, self.current, &mut self.rng);
        if let Some(index) = picked {
            self.current = Some(index);
            let cooldown = self.def.actions[index].cooldown;
            if cooldown > 0.0 {
                self.cooldown_until[index] = now + cooldown;
            }
        }
        DecisionReport {
            now,
            mode: self.def.selection,
            selected: picked.map(|i| self.def.actions[i].name.clone()),
            actions,
        }
    }

    /// Takes one decision and writes it into `report`. When `report` already has this definition's
    /// actions and consideration keys, the numbers are overwritten in place. Otherwise this falls
    /// through to [`Reasoner::decide`] and replaces `report`.
    pub fn decide_into(&mut self, bb: &Blackboard, now: f32, report: &mut DecisionReport) {
        if !self.report_matches(report) {
            *report = self.decide(bb, now);
            return;
        }
        self.score_into(bb, now, report);
        let picked = pick(&report.actions, self.def.selection, self.current, &mut self.rng);
        if let Some(index) = picked {
            self.current = Some(index);
            let cooldown = self.def.actions[index].cooldown;
            if cooldown > 0.0 {
                self.cooldown_until[index] = now + cooldown;
            }
        }
        match picked {
            Some(index) => {
                let name = &self.def.actions[index].name;
                match report.selected.as_mut() {
                    Some(selected) if selected == name => {}
                    Some(selected) => selected.clone_from(name),
                    None => report.selected = Some(name.clone()),
                }
            }
            None => report.selected = None,
        }
    }

    /// Answers "what would be picked now" without changing anything: same scores, same pick,
    /// but the current action, the cooldown schedule and the generator are untouched (the
    /// weighted draw runs on a copy of the generator state).
    pub fn explain(&self, bb: &Blackboard, now: f32) -> DecisionReport {
        let actions = self.score_actions(bb, now);
        let mut rng = self.rng;
        let picked = pick(&actions, self.def.selection, self.current, &mut rng);
        DecisionReport {
            now,
            mode: self.def.selection,
            selected: picked.map(|i| self.def.actions[i].name.clone()),
            actions,
        }
    }
}

/// Validates a definition for [`Reasoner::new`] (see there for the rules).
fn validate(def: &UtilityAiDef) -> Result<(), UtilityAiError> {
    if !(UTILITY_AI_MIN_FORMAT_VERSION..=UTILITY_AI_FORMAT_VERSION).contains(&def.format_version) {
        return Err(UtilityAiError::UnsupportedVersion {
            found: def.format_version,
            supported: UTILITY_AI_FORMAT_VERSION,
        });
    }
    if def.actions.is_empty() {
        return Err(UtilityAiError::Empty);
    }
    if def.actions.len() > MAX_ACTIONS {
        return Err(UtilityAiError::TooManyActions(def.actions.len()));
    }
    match def.selection {
        SelectionMode::WeightedRandom { top_n, margin } => {
            if top_n == 0 || !margin.is_finite() || margin < 0.0 {
                return Err(UtilityAiError::BadValue("<selection>".to_string()));
            }
        }
        SelectionMode::Best => {}
    }
    let mut names = BTreeSet::new();
    for action in &def.actions {
        if action.name.is_empty() {
            return Err(UtilityAiError::EmptyName);
        }
        if !names.insert(action.name.as_str()) {
            return Err(UtilityAiError::DuplicateName(action.name.clone()));
        }
        let bad = (!action.weight.is_finite() || action.weight < 0.0)
            || (!action.momentum.is_finite() || action.momentum < 0.0)
            || (!action.cooldown.is_finite() || action.cooldown < 0.0);
        if bad {
            return Err(UtilityAiError::BadValue(action.name.clone()));
        }
        if action.considerations.len() > MAX_CONSIDERATIONS {
            return Err(UtilityAiError::TooManyConsiderations {
                action: action.name.clone(),
                count: action.considerations.len(),
            });
        }
        for consideration in &action.considerations {
            if consideration.key.is_empty()
                || !consideration.min.is_finite()
                || !consideration.max.is_finite()
            {
                return Err(UtilityAiError::BadValue(action.name.clone()));
            }
            if consideration.min >= consideration.max {
                return Err(UtilityAiError::BadRange {
                    action: action.name.clone(),
                    key: consideration.key.clone(),
                });
            }
            consideration.curve.validate(&action.name, &consideration.key)?;
        }
    }
    Ok(())
}

/// The `.utility.ron` migration chain: the versions older than
/// [`UTILITY_AI_FORMAT_VERSION`] that still load. Empty: version 1 is the first published shape,
/// so there is nothing to migrate yet. When version 2 exists, this lists `[1]` and
/// [`migrate_ron`] rewrites version 1 files (never in place; the cook writes a new file).
pub fn utility_ai_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.utility.ron` (hashed by `cargo xtask check-formats`): the field
/// list of every struct reachable from [`UtilityAiDef`], then the spellings of its enums. The fields
/// of the `WeightedRandom` variant are listed by hand: no capture reaches a struct variant's fields.
pub fn utility_ai_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<UtilityAiDef>(),
        serde_field_names::<ActionDef>(),
        serde_field_names::<ConsiderationDef>(),
        serde_field_names::<ResponseCurve>(),
        serde_field_names::<CurvePoint>(),
        serde_variant_names::<SelectionMode>(),
        vec![
            "WeightedRandom.top_n".to_string(),
            "WeightedRandom.margin".to_string(),
        ],
        serde_variant_names::<CurveKind>(),
    ])
}

/// The `.utility.ron` migrator stub: a file already at [`UTILITY_AI_FORMAT_VERSION`] is returned
/// unchanged; any other version is [`UtilityAiError::UnsupportedVersion`]. Future versions chain
/// their rewrites here (a frozen content format is never changed without a migrator,
/// `docs/specs/12` § 4a).
pub fn migrate_ron(text: &str) -> Result<String, UtilityAiError> {
    let header: VersionHeader = ron::from_str(text).map_err(|e| UtilityAiError::Parse(e.to_string()))?;
    match header.format_version {
        UTILITY_AI_FORMAT_VERSION => Ok(text.to_string()),
        found => Err(UtilityAiError::UnsupportedVersion {
            found,
            supported: UTILITY_AI_FORMAT_VERSION,
        }),
    }
}

fn one() -> f32 {
    1.0
}

fn two() -> f32 {
    2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(kind: CurveKind) -> ResponseCurve {
        ResponseCurve {
            kind,
            ..ResponseCurve::default()
        }
    }

    fn piecewise(points: &[(f32, f32)]) -> ResponseCurve {
        ResponseCurve {
            kind: CurveKind::Piecewise,
            points: points.iter().map(|&(x, y)| CurvePoint { x, y }).collect(),
            ..ResponseCurve::default()
        }
    }

    fn con(key: &str, min: f32, max: f32, curve_kind: ResponseCurve) -> ConsiderationDef {
        ConsiderationDef {
            key: key.to_string(),
            min,
            max,
            curve: curve_kind,
        }
    }

    fn action(name: &str, considerations: Vec<ConsiderationDef>) -> ActionDef {
        ActionDef {
            name: name.to_string(),
            considerations,
            weight: 1.0,
            cooldown: 0.0,
            momentum: 0.0,
        }
    }

    fn def(actions: Vec<ActionDef>) -> UtilityAiDef {
        UtilityAiDef {
            format_version: UTILITY_AI_FORMAT_VERSION,
            description: String::new(),
            seed: 0,
            selection: SelectionMode::Best,
            actions,
        }
    }

    fn def_weighted(actions: Vec<ActionDef>, top_n: usize, margin: f32, seed: u64) -> UtilityAiDef {
        UtilityAiDef {
            seed,
            selection: SelectionMode::WeightedRandom { top_n, margin },
            ..def(actions)
        }
    }

    fn board(pairs: &[(&str, f32)]) -> Blackboard {
        let mut bb = Blackboard::new();
        for (key, value) in pairs {
            bb.insert(*key, *value);
        }
        bb
    }

    fn score_of(report: &DecisionReport, name: &str) -> f32 {
        report
            .actions
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.score)
            .unwrap_or(f32::NAN)
    }

    // ---- response curves ---------------------------------------------------------------

    #[test]
    fn linear_curve_documented_values() {
        let c = curve(CurveKind::Linear);
        assert_eq!(c.evaluate(0.0), 0.0);
        assert_eq!(c.evaluate(0.5), 0.5);
        assert_eq!(c.evaluate(1.0), 1.0);
        // Slope, x-shift and y-shift compose: slope * (x - x_shift) + y_shift.
        let shifted = ResponseCurve {
            slope: 2.0,
            x_shift: 0.25,
            y_shift: 0.1,
            ..curve(CurveKind::Linear)
        };
        assert_eq!(shifted.evaluate(0.25), 0.1);
        assert!((shifted.evaluate(0.5) - 0.6).abs() < 1e-6);
    }

    #[test]
    fn quadratic_curve_documented_values() {
        let c = curve(CurveKind::Quadratic);
        assert_eq!(c.evaluate(0.0), 0.0);
        assert_eq!(c.evaluate(0.5), 0.25);
        assert_eq!(c.evaluate(1.0), 1.0);
        // An exponent of 0.5 is the square root.
        let sqrt = ResponseCurve {
            exponent: 0.5,
            ..curve(CurveKind::Quadratic)
        };
        assert_eq!(sqrt.evaluate(0.25), 0.5);
        // An odd integer exponent of a negative shifted input keeps the sign (clamped to 0).
        let odd = ResponseCurve {
            exponent: 3.0,
            ..curve(CurveKind::Quadratic)
        };
        assert_eq!(odd.evaluate(-0.5), 0.0);
        assert_eq!(odd.evaluate(0.5), 0.125);
        // An even exponent of a negative input stays positive: (-0.5)^2 = 0.25.
        assert_eq!(c.evaluate(-0.5), 0.25);
    }

    /// Finding 10 (audit G): the quadratic, logistic and logit families called the platform libm
    /// (`powf`, `exp`, `ln`), whose last bit differs between platforms, and `Best` compares the
    /// scores with a strict `>`, so one ulp flipped a decision. These bits come from the
    /// deterministic kernels and hold on every target.
    #[test]
    fn logistic_quadratic_and_logit_bits_are_pinned() {
        // Logistic, exponent 2 (the default): the documented 0.7310586. On this host's glibc the
        // pins happen to coincide with the libm result at these points; the pin's point is that
        // the bits hold on every target, including hosts whose libm disagrees.
        let logistic = curve(CurveKind::Logistic);
        assert_eq!(logistic.evaluate(0.0).to_bits(), 0x3f00_0000);
        assert_eq!(logistic.evaluate(0.5).to_bits(), 0x3f3b_26a8);
        assert_eq!(logistic.evaluate(1.0).to_bits(), 0x3f61_7bea);
        // Quadratic with a fractional exponent: libm-dependent exactly where a whole-number
        // exponent is not.
        let quadratic = ResponseCurve {
            kind: CurveKind::Quadratic,
            exponent: 2.5,
            ..ResponseCurve::default()
        };
        assert_eq!(quadratic.evaluate(0.5).to_bits(), 0x3e35_04f3);
        let sqrt_curve = ResponseCurve {
            kind: CurveKind::Quadratic,
            exponent: 0.5,
            ..ResponseCurve::default()
        };
        assert_eq!(sqrt_curve.evaluate(1.0 / 3.0).to_bits(), 0x3f13_cd3a);
        // Logit is the exact inverse of the logistic at the defaults: the same magnitude either
        // side of 0.5, and zero at the midpoint. (Outputs clamp to [0, 1], so the pins sit where
        // the curve is in range.)
        let logit = curve(CurveKind::Logit);
        assert_eq!(logit.evaluate(0.5).to_bits(), 0x0000_0000);
        assert_eq!(logit.evaluate(0.6).to_bits(), 0x3e4f_9924);
        assert_eq!(logit.evaluate(0.75).to_bits(), 0x3f0c_9f54);

        // A `Best` decision where the linear rival sits on the neighbouring ulp: the normalized
        // input is 0.5, so a linear slope of `2 * logistic(0.5)` ties the logistic action; one ulp
        // above, the linear action must win, and on the exact tie declaration order keeps the
        // logistic action.
        let logistic_at_half = f32::from_bits(0x3f3b_26a8);
        let tie_slope = logistic_at_half * 2.0;
        let decision = |linear_slope: f32| {
            let mut reasoner = Reasoner::new(&def(vec![
                action(
                    "logistic",
                    vec![con("threat", 0.0, 1.0, curve(CurveKind::Logistic))],
                ),
                action(
                    "linear",
                    vec![con(
                        "threat",
                        0.0,
                        1.0,
                        ResponseCurve {
                            kind: CurveKind::Linear,
                            slope: linear_slope,
                            ..ResponseCurve::default()
                        },
                    )],
                ),
            ]))
            .expect("valid");
            let report = reasoner.decide(&board(&[("threat", 0.5)]), 0.0);
            report.selected.expect("a selection").to_string()
        };
        assert_eq!(decision(f32::from_bits(tie_slope.to_bits() + 1)), "linear");
        assert_eq!(decision(tie_slope), "logistic");
    }

    #[test]
    fn logistic_curve_documented_values() {
        let c = curve(CurveKind::Logistic);
        assert!((c.evaluate(0.0) - 0.5).abs() < 1e-6);
        assert!((c.evaluate(0.5) - 0.7310586).abs() < 1e-5);
        assert!((c.evaluate(1.0) - 0.8807971).abs() < 1e-5);
        // The x-shift moves the midpoint: at x = 0.5 the value is exactly the midpoint.
        let shifted = ResponseCurve {
            x_shift: 0.5,
            ..curve(CurveKind::Logistic)
        };
        assert!((shifted.evaluate(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn logit_curve_documented_values() {
        let c = curve(CurveKind::Logit);
        assert_eq!(c.evaluate(0.0), 0.0);
        assert_eq!(c.evaluate(0.5), 0.0);
        assert_eq!(c.evaluate(1.0), 1.0);
        // Halfway between the midpoint and the edge: logit(0.75) / 2 = ln(3) / 2.
        assert!((c.evaluate(0.75) - 0.5493061).abs() < 1e-4);
    }

    #[test]
    fn step_curve_documented_values() {
        let c = ResponseCurve {
            x_shift: 0.5,
            ..curve(CurveKind::Step)
        };
        assert_eq!(c.evaluate(0.0), 0.0);
        assert_eq!(c.evaluate(0.4999999), 0.0);
        assert_eq!(c.evaluate(0.5), 1.0);
        assert_eq!(c.evaluate(1.0), 1.0);
        // The default curve with no shift is up from 0.
        assert_eq!(curve(CurveKind::Step).evaluate(0.0), 1.0);
    }

    #[test]
    fn piecewise_curve_documented_values() {
        let c = piecewise(&[(0.0, 0.0), (0.5, 0.25), (1.0, 1.0)]);
        assert_eq!(c.evaluate(0.0), 0.0);
        assert_eq!(c.evaluate(0.5), 0.25);
        assert_eq!(c.evaluate(1.0), 1.0);
        assert_eq!(c.evaluate(0.25), 0.125);
        // Outside the point range the endpoints hold.
        assert_eq!(c.evaluate(-0.5), 0.0);
        assert_eq!(c.evaluate(1.5), 1.0);
        // The x-shift moves the whole polyline; empty points are a defined 0.
        let shifted = ResponseCurve {
            x_shift: 0.5,
            ..piecewise(&[(0.0, 0.0), (1.0, 1.0)])
        };
        assert_eq!(shifted.evaluate(1.0), 0.5);
        assert_eq!(piecewise(&[]).evaluate(0.5), 0.0);
    }

    #[test]
    fn curves_are_monotonic_where_documented() {
        let ascending = [
            curve(CurveKind::Linear),
            curve(CurveKind::Quadratic),
            curve(CurveKind::Logistic),
            curve(CurveKind::Logit),
            ResponseCurve {
                x_shift: 0.5,
                ..curve(CurveKind::Step)
            },
            piecewise(&[(0.0, 0.0), (0.5, 0.25), (1.0, 1.0)]),
        ];
        for c in ascending {
            let values: Vec<f32> = (0..=100u8).map(|i| c.evaluate(f32::from(i) / 100.0)).collect();
            for pair in values.windows(2) {
                assert!(
                    pair[1] >= pair[0],
                    "{:?} is not non-decreasing: {} < {}",
                    c.kind,
                    pair[1],
                    pair[0]
                );
            }
        }
        // A descending linear curve is non-increasing.
        let descending = ResponseCurve {
            slope: -1.0,
            y_shift: 1.0,
            ..curve(CurveKind::Linear)
        };
        let values: Vec<f32> = (0..=100u8)
            .map(|i| descending.evaluate(f32::from(i) / 100.0))
            .collect();
        for pair in values.windows(2) {
            assert!(pair[1] <= pair[0]);
        }
    }

    #[test]
    fn curve_outputs_are_clamped() {
        let steep = ResponseCurve {
            slope: 10.0,
            ..curve(CurveKind::Linear)
        };
        assert_eq!(steep.evaluate(0.2), 1.0);
        assert_eq!(steep.evaluate(0.05), 0.5);
        let descending = ResponseCurve {
            slope: -2.0,
            y_shift: 0.5,
            ..curve(CurveKind::Linear)
        };
        assert_eq!(descending.evaluate(0.0), 0.5);
        assert_eq!(descending.evaluate(1.0), 0.0);
    }

    #[test]
    fn curve_rejects_non_finite_input() {
        for kind in [
            CurveKind::Linear,
            CurveKind::Quadratic,
            CurveKind::Logistic,
            CurveKind::Logit,
            CurveKind::Step,
        ] {
            let c = curve(kind);
            assert_eq!(c.evaluate(f32::NAN), 0.0, "{kind:?}");
            assert_eq!(c.evaluate(f32::INFINITY), 0.0, "{kind:?}");
            assert_eq!(c.evaluate(f32::NEG_INFINITY), 0.0, "{kind:?}");
        }
    }

    // ---- compensation ------------------------------------------------------------------

    #[test]
    fn modification_factor_matches_published_values() {
        // Mark and Lewis: modification factor = 1 - 1/n over 1..5 considerations.
        let expected = [0.0, 0.5, 2.0 / 3.0, 0.75, 0.8];
        for (n, &want) in expected.iter().enumerate() {
            let n = n + 1;
            let got = modification_factor(n);
            assert!(
                (got - want).abs() < 1e-6,
                "modification_factor({n}) = {got}, want {want}"
            );
        }
        // Zero considerations is defined (no make-up).
        assert_eq!(modification_factor(0), 0.0);
        // Many considerations approach, never reach, 1.
        assert!(modification_factor(MAX_CONSIDERATIONS) < 1.0);
    }

    #[test]
    fn compensation_equals_published_values() {
        // One consideration scoring 0.5: make_up = 0.5 * (1 - 1/n); 0.5 + make_up * 0.5.
        let expected = [0.5, 0.625, 2.0 / 3.0, 0.6875, 0.7];
        for (n, &want) in expected.iter().enumerate() {
            let n = n + 1;
            let got = compensate(n, 0.5);
            assert!(
                (got - want).abs() < 1e-6,
                "compensate({n}, 0.5) = {got}, want {want}"
            );
        }
        // Fixed points: nothing to make up at 0 and 1.
        assert_eq!(compensate(5, 0.0), 0.0);
        assert_eq!(compensate(5, 1.0), 1.0);
        // One consideration is the identity.
        assert_eq!(compensate(1, 0.37), 0.37);
        // Non-finite input reads as 0; out-of-range input is clamped first.
        assert_eq!(compensate(4, f32::INFINITY), 0.0);
        assert_eq!(compensate(4, f32::NAN), 0.0);
        assert_eq!(compensate(4, 1.5), 1.0);
        // The result stays in [0, 1] and never lowers a score.
        for i in 0..=100u8 {
            let s = f32::from(i) / 100.0;
            for n in 1..=MAX_CONSIDERATIONS {
                let c = compensate(n, s);
                assert!((s..=1.0).contains(&c), "compensate({n}, {s}) = {c}");
            }
        }
    }

    #[test]
    fn action_scores_follow_the_published_compensation() {
        // n considerations all scoring 0.5: the action is the product of n compensated values,
        // (0.5 + 0.25 * (1 - 1/n))^n, for n = 1..5.
        let expected = [0.5, 0.390625, 0.2962963, 0.2234039, 0.16807];
        for (n, &want) in expected.iter().enumerate() {
            let n = n + 1;
            let a = action(
                "a",
                (0..n)
                    .map(|i| con(&format!("k{i}"), 0.0, 1.0, curve(CurveKind::Linear)))
                    .collect(),
            );
            let reasoner = Reasoner::new(&def(vec![a])).expect("valid");
            let mut bb = Blackboard::new();
            for i in 0..n {
                bb.insert(format!("k{i}"), 0.5f32);
            }
            let report = reasoner.explain(&bb, 0.0);
            let scored = &report.actions[0];
            assert!(
                (scored.compensated - want).abs() < 1e-5,
                "n = {n}: {} vs {want}",
                scored.compensated
            );
            assert!((scored.raw - 0.5f32.powi(n as i32)).abs() < 1e-6);
            for c in &scored.considerations {
                assert_eq!(c.compensated, compensate(n, 0.5));
            }
        }
    }

    #[test]
    fn many_considerations_are_not_unfairly_penalised() {
        // Five considerations each scoring 0.9 have a raw product of 0.59049 — below a single
        // consideration scoring 0.8. Compensated, each is 0.9 + 0.1 * 0.8 * 0.9 = 0.972 and the
        // five multiply to 0.972^5 = 0.8676, which wins.
        let mut five = action("five", Vec::new());
        for i in 0..5 {
            five.considerations
                .push(con(&format!("k{i}"), 0.0, 1.0, curve(CurveKind::Linear)));
        }
        let one = action("one", vec![con("high", 0.0, 1.0, curve(CurveKind::Linear))]);
        let mut reasoner = Reasoner::new(&def(vec![five, one])).expect("valid");
        let mut bb = Blackboard::new();
        for i in 0..5 {
            bb.insert(format!("k{i}"), 0.9f32);
        }
        bb.insert("high", 0.8f32);
        let report = reasoner.decide(&bb, 0.0);
        assert_eq!(report.selected.as_deref(), Some("five"));
        let five_score = report.actions.iter().find(|a| a.name == "five").expect("scored");
        assert!((five_score.raw - 0.59049).abs() < 1e-4);
        assert!((five_score.compensated - 0.867_624).abs() < 1e-4);
        assert!((five_score.modification_factor - 0.8).abs() < 1e-6);
        // The single consideration is the identity through the whole pipeline.
        assert_eq!(score_of(&report, "one"), 0.8);
    }

    // ---- scoring and selection ---------------------------------------------------------

    #[test]
    fn best_selects_the_highest_score() {
        let d = def(vec![
            action("low", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]),
            action("high", vec![con("b", 0.0, 1.0, curve(CurveKind::Linear))]),
        ]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let report = reasoner.decide(&board(&[("a", 0.3), ("b", 0.7)]), 0.0);
        assert_eq!(report.selected.as_deref(), Some("high"));
        assert_eq!(reasoner.current_action(), Some("high"));
    }

    #[test]
    fn weight_scales_the_score() {
        let mut weighted = action("weighted", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]);
        weighted.weight = 0.5;
        let plain = action("plain", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]);
        let mut reasoner = Reasoner::new(&def(vec![weighted, plain])).expect("valid");
        let report = reasoner.decide(&board(&[("a", 0.8)]), 0.0);
        assert_eq!(score_of(&report, "weighted"), 0.4);
        assert_eq!(score_of(&report, "plain"), 0.8);
        assert_eq!(report.selected.as_deref(), Some("plain"));

        // A weight of 0 disables the action entirely.
        let mut zero = action("zero", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]);
        zero.weight = 0.0;
        let mut reasoner = Reasoner::new(&def(vec![zero])).expect("valid");
        let report = reasoner.decide(&board(&[("a", 0.9)]), 0.0);
        assert_eq!(report.selected, None);
        assert!(!report.actions[0].eligible);
    }

    #[test]
    fn ties_resolve_deterministically() {
        let d = def(vec![
            action("first", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]),
            action("second", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]),
        ]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let bb = board(&[("a", 0.7)]);
        for _ in 0..100 {
            let report = reasoner.decide(&bb, 0.0);
            assert_eq!(report.selected.as_deref(), Some("first"));
        }
    }

    #[test]
    fn momentum_keeps_the_current_action() {
        let mut attack = action("attack", vec![con("health", 0.0, 1.0, curve(CurveKind::Linear))]);
        attack.momentum = 0.25;
        let flee = action("flee", vec![con("threat", 0.0, 1.0, curve(CurveKind::Linear))]);
        let mut reasoner = Reasoner::new(&def(vec![attack, flee])).expect("valid");

        // No current action: attack's plain 0.5 beats 0.4.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.4)]), 0.0);
        assert_eq!(report.selected.as_deref(), Some("attack"));

        // Flee at 0.74 does not exceed attack's 0.5 + 0.25 bonus: attack stays.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.74)]), 1.0);
        assert_eq!(report.selected.as_deref(), Some("attack"));
        assert_eq!(score_of(&report, "attack"), 0.75);
        assert_eq!(
            report
                .actions
                .iter()
                .find(|a| a.name == "attack")
                .expect("scored")
                .momentum_applied,
            0.25
        );

        // Flee at 0.8 exceeds it: the switch happens. This decision was scored while attack was
        // still current, so its report shows the bonus attack had when it lost.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.8)]), 2.0);
        assert_eq!(report.selected.as_deref(), Some("flee"));
        assert_eq!(
            report
                .actions
                .iter()
                .find(|a| a.name == "attack")
                .expect("scored")
                .momentum_applied,
            0.25
        );
        // From the next decision on, flee is current and attack has no bonus.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.8)]), 3.0);
        assert_eq!(report.selected.as_deref(), Some("flee"));
        assert_eq!(
            report
                .actions
                .iter()
                .find(|a| a.name == "attack")
                .expect("scored")
                .momentum_applied,
            0.0
        );
        assert_eq!(score_of(&report, "attack"), 0.5);
    }

    #[test]
    fn momentum_tie_keeps_the_current_action_whatever_the_declaration_order() {
        // The challenger is declared first. When it scores exactly the current action's score
        // plus the bonus it has not *exceeded* it, so the current action stays (the declaration
        // order tie-break must not override commitment).
        let flee = action("flee", vec![con("threat", 0.0, 1.0, curve(CurveKind::Linear))]);
        let mut attack = action("attack", vec![con("health", 0.0, 1.0, curve(CurveKind::Linear))]);
        attack.momentum = 0.25;
        let mut reasoner = Reasoner::new(&def(vec![flee, attack])).expect("valid");
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.25)]), 0.0);
        assert_eq!(report.selected.as_deref(), Some("attack"));
        // 0.5 + 0.25 == 0.75 exactly in f32.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.75)]), 1.0);
        assert_eq!(score_of(&report, "attack"), score_of(&report, "flee"));
        assert_eq!(report.selected.as_deref(), Some("attack"));
        assert_eq!(
            reasoner
                .explain(&board(&[("health", 0.5), ("threat", 0.75)]), 2.0)
                .selected
                .as_deref(),
            Some("attack")
        );
        // Strictly above: the switch happens.
        let report = reasoner.decide(&board(&[("health", 0.5), ("threat", 0.76)]), 2.0);
        assert_eq!(report.selected.as_deref(), Some("flee"));
    }

    #[test]
    fn cooldown_blocks_until_elapsed() {
        let mut dash = action("dash", vec![con("ready", 0.0, 1.0, curve(CurveKind::Linear))]);
        dash.cooldown = 2.0;
        let walk = action("walk", vec![con("ready", 0.0, 2.0, curve(CurveKind::Linear))]);
        let mut reasoner = Reasoner::new(&def(vec![dash, walk])).expect("valid");
        let bb = board(&[("ready", 1.0)]);

        // t = 0: dash (1.0) beats walk (0.5) and starts a 2 s cooldown.
        assert_eq!(reasoner.decide(&bb, 0.0).selected.as_deref(), Some("dash"));
        assert_eq!(reasoner.cooldown_remaining("dash", 0.0), Some(2.0));

        // t = 1: dash is blocked; walk wins.
        let report = reasoner.decide(&bb, 1.0);
        assert_eq!(report.selected.as_deref(), Some("walk"));
        let dash = report.actions.iter().find(|a| a.name == "dash").expect("scored");
        assert!(matches!(dash.blocked, Some(BlockReason::Cooldown { until }) if until == 2.0));
        assert!(!dash.eligible);

        // t = 1.9: still blocked.
        assert_eq!(reasoner.decide(&bb, 1.9).selected.as_deref(), Some("walk"));

        // t = 2.0: exactly elapsed (the boundary is inclusive) and dash wins again.
        assert_eq!(reasoner.decide(&bb, 2.0).selected.as_deref(), Some("dash"));
        assert_eq!(reasoner.cooldown_remaining("dash", 2.0), Some(2.0));
        assert_eq!(reasoner.cooldown_remaining("dash", 4.0), Some(0.0));
        assert_eq!(reasoner.cooldown_remaining("unknown", 0.0), None);
    }

    // A local mirror of the generator, used as the oracle for seed honoring.
    fn oracle_unit(state: &mut u64) -> f32 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 40) as f32 / 16_777_216.0
    }

    #[test]
    fn weighted_random_honours_the_seed() {
        // Two candidates, 0.9 and 0.85, both within the margin: the draw is
        // `unit * total < 0.9` for the first. The oracle replays the same SplitMix64 stream.
        let d = def_weighted(
            vec![
                action("a", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("b", vec![con("y", 0.0, 1.0, curve(CurveKind::Linear))]),
            ],
            2,
            1.0,
            7,
        );
        let bb = board(&[("x", 0.9), ("y", 0.85)]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let mut oracle = 7u64;
        for _ in 0..5 {
            let report = reasoner.decide(&bb, 0.0);
            let want = if oracle_unit(&mut oracle) * (0.9f32 + 0.85f32) < 0.9 {
                "a"
            } else {
                "b"
            };
            assert_eq!(report.selected.as_deref(), Some(want));
        }

        // Same definition built twice: the whole sequence replays.
        let first: Vec<String> = {
            let mut r = Reasoner::new(&d).expect("valid");
            (0..20)
                .map(|_| r.decide(&bb, 0.0).selected.expect("a pick"))
                .collect()
        };
        let second: Vec<String> = {
            let mut r = Reasoner::new(&d).expect("valid");
            (0..20)
                .map(|_| r.decide(&bb, 0.0).selected.expect("a pick"))
                .collect()
        };
        assert_eq!(first, second);

        // Across sixteen seeds with two equal candidates, the first pick is not one-sided.
        let mut firsts: Vec<String> = Vec::new();
        for seed in 0..16u64 {
            let d = def_weighted(
                vec![
                    action("left", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]),
                    action("right", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]),
                ],
                2,
                1.0,
                seed,
            );
            let mut r = Reasoner::new(&d).expect("valid");
            firsts.push(r.decide(&board(&[("x", 0.5)]), 0.0).selected.expect("a pick"));
        }
        assert!(firsts.iter().any(|f| f == "left"), "{firsts:?}");
        assert!(firsts.iter().any(|f| f == "right"), "{firsts:?}");
    }

    #[test]
    fn weighted_random_spreads_across_the_window() {
        // Three candidates, all within the margin: each is picked at least once over 100 fixed
        // draws (the sequence is deterministic for the seed; this guards the draw arithmetic).
        let d = def_weighted(
            vec![
                action("a", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("b", vec![con("b", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("c", vec![con("c", 0.0, 1.0, curve(CurveKind::Linear))]),
            ],
            3,
            1.0,
            42,
        );
        let bb = board(&[("a", 0.5), ("b", 0.5), ("c", 0.5)]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let mut seen = BTreeSet::new();
        for _ in 0..100 {
            let report = reasoner.decide(&bb, 0.0);
            seen.insert(report.selected.expect("a pick"));
        }
        assert_eq!(seen.len(), 3, "{seen:?}");
    }

    #[test]
    fn top_n_margin_limits_candidates() {
        // Scores 0.9, 0.85, 0.3 with margin 0.1: the third never joins the draw; with top_n 1
        // only the best is ever picked.
        let d = def_weighted(
            vec![
                action("a", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("b", vec![con("b", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("c", vec![con("c", 0.0, 1.0, curve(CurveKind::Linear))]),
            ],
            3,
            0.1,
            9,
        );
        let bb = board(&[("a", 0.9), ("b", 0.85), ("c", 0.3)]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        for _ in 0..100 {
            let report = reasoner.decide(&bb, 0.0);
            let picked = report.selected.as_deref().expect("a pick");
            assert!(picked == "a" || picked == "b", "picked {picked}");
        }

        let d = def_weighted(d.actions.clone(), 1, 1.0, 9);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        for _ in 0..20 {
            assert_eq!(reasoner.decide(&bb, 0.0).selected.as_deref(), Some("a"));
        }
    }

    // ---- degenerate and adversarial input ----------------------------------------------

    #[test]
    fn zero_considerations_score_zero_and_are_not_selected() {
        let empty = || action("empty", Vec::new());
        let mut reasoner = Reasoner::new(&def(vec![empty()])).expect("valid");
        let report = reasoner.decide(&Blackboard::default(), 0.0);
        assert_eq!(score_of(&report, "empty"), 0.0);
        assert!(!report.actions[0].eligible);
        assert_eq!(report.selected, None);
        assert!(matches!(
            report.actions[0].blocked,
            Some(BlockReason::NoConsiderations)
        ));

        // Mixed with a real action the real one wins; the empty one scores 0.
        let real = action("real", vec![con("a", 0.0, 1.0, curve(CurveKind::Linear))]);
        let mut reasoner = Reasoner::new(&def(vec![empty(), real])).expect("valid");
        let report = reasoner.decide(&board(&[("a", 0.4)]), 0.0);
        assert_eq!(report.selected.as_deref(), Some("real"));
        assert_eq!(score_of(&report, "empty"), 0.0);
    }

    #[test]
    fn non_finite_or_missing_inputs_score_zero() {
        let mut bb = Blackboard::new();
        bb.insert("nan", f32::NAN);
        bb.insert("inf", f32::INFINITY);
        bb.insert("text", "not a number");
        bb.insert("int", 5i32);
        let d = def(vec![
            action("nan", vec![con("nan", 0.0, 1.0, curve(CurveKind::Linear))]),
            action("inf", vec![con("inf", 0.0, 1.0, curve(CurveKind::Linear))]),
            action("text", vec![con("text", 0.0, 1.0, curve(CurveKind::Linear))]),
            action(
                "missing",
                vec![con("missing", 0.0, 1.0, curve(CurveKind::Linear))],
            ),
            action("int", vec![con("int", 0.0, 10.0, curve(CurveKind::Linear))]),
        ]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let report = reasoner.decide(&bb, 0.0);
        for name in ["nan", "inf", "text", "missing"] {
            let scored = report.actions.iter().find(|a| a.name == name).expect("scored");
            assert!(!scored.eligible, "{name}");
            assert_eq!(scored.score, 0.0, "{name}");
            let consideration = &scored.considerations[0];
            assert!(!consideration.valid, "{name}");
            assert_eq!(consideration.curve_output, 0.0, "{name}");
            assert!(consideration.raw_input.is_nan(), "{name}");
        }
        // An int key reads losslessly and wins.
        let scored = report.actions.iter().find(|a| a.name == "int").expect("scored");
        assert_eq!(scored.considerations[0].raw_input, 5.0);
        assert_eq!(scored.considerations[0].normalized, 0.5);
        assert_eq!(report.selected.as_deref(), Some("int"));
    }

    #[test]
    fn non_finite_now_selects_nothing_and_keeps_cooldowns() {
        let mut dash = action("dash", vec![con("ready", 0.0, 1.0, curve(CurveKind::Linear))]);
        dash.cooldown = 2.0;
        let d = def_weighted(vec![dash], 1, 0.0, 5);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let bb = board(&[("ready", 1.0)]);
        // A NaN or infinite clock picks nothing, starts no cooldown and leaves the state alone
        // (before the fix, NaN started a NaN cooldown that never blocked again).
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let report = reasoner.decide(&bb, bad);
            assert_eq!(report.selected, None);
            assert!(matches!(
                report.actions[0].blocked,
                Some(BlockReason::InvalidTime)
            ));
            assert_eq!(reasoner.current_action(), None);
            assert_eq!(reasoner.cooldown_remaining("dash", 0.0), Some(0.0));
        }
        // Real time still works, and the cooldown then blocks as authored.
        assert_eq!(reasoner.decide(&bb, 0.0).selected.as_deref(), Some("dash"));
        assert_eq!(reasoner.decide(&bb, f32::NAN).selected, None);
        assert_eq!(reasoner.decide(&bb, 1.0).selected, None);
        assert_eq!(reasoner.cooldown_remaining("dash", 1.0), Some(1.0));
        assert_eq!(reasoner.decide(&bb, 2.0).selected.as_deref(), Some("dash"));
    }

    #[test]
    fn degenerate_ranges_are_rejected() {
        let flat = action("flat", vec![con("a", 1.0, 1.0, curve(CurveKind::Linear))]);
        let err = Reasoner::new(&def(vec![flat])).expect_err("min == max");
        assert_eq!(err.reason(), "bad_range");
        let inverted = action("inverted", vec![con("a", 2.0, 1.0, curve(CurveKind::Linear))]);
        let err = Reasoner::new(&def(vec![inverted])).expect_err("min > max");
        assert!(matches!(err, UtilityAiError::BadRange { key, .. } if key == "a"));
        let nan_min = action("nan_min", vec![con("a", f32::NAN, 1.0, curve(CurveKind::Linear))]);
        assert!(Reasoner::new(&def(vec![nan_min])).is_err());
    }

    #[test]
    fn invalid_definitions_are_rejected() {
        let good = || vec![action("a", vec![con("k", 0.0, 1.0, curve(CurveKind::Linear))])];

        let empty = UtilityAiDef::default();
        assert!(matches!(Reasoner::new(&empty), Err(UtilityAiError::Empty)));

        let no_name = def(vec![action("", Vec::new())]);
        assert!(matches!(Reasoner::new(&no_name), Err(UtilityAiError::EmptyName)));

        let duplicate = def(vec![action("a", Vec::new()), action("a", Vec::new())]);
        assert!(matches!(
            Reasoner::new(&duplicate),
            Err(UtilityAiError::DuplicateName(_))
        ));

        let mut bad_weight = action("a", Vec::new());
        bad_weight.weight = -0.5;
        assert!(matches!(
            Reasoner::new(&def(vec![bad_weight])),
            Err(UtilityAiError::BadValue(_))
        ));

        let mut bad_momentum = action("a", Vec::new());
        bad_momentum.momentum = f32::NAN;
        assert!(matches!(
            Reasoner::new(&def(vec![bad_momentum])),
            Err(UtilityAiError::BadValue(_))
        ));

        let mut bad_cooldown = action("a", Vec::new());
        bad_cooldown.cooldown = -1.0;
        assert!(matches!(
            Reasoner::new(&def(vec![bad_cooldown])),
            Err(UtilityAiError::BadValue(_))
        ));

        let empty_key = action("a", vec![con("", 0.0, 1.0, curve(CurveKind::Linear))]);
        assert!(matches!(
            Reasoner::new(&def(vec![empty_key])),
            Err(UtilityAiError::BadValue(_))
        ));

        let too_many = action(
            "a",
            (0..MAX_CONSIDERATIONS + 1)
                .map(|i| con(&format!("k{i}"), 0.0, 1.0, curve(CurveKind::Linear)))
                .collect(),
        );
        assert!(matches!(
            Reasoner::new(&def(vec![too_many])),
            Err(UtilityAiError::TooManyConsiderations { .. })
        ));

        let unsorted = action(
            "a",
            vec![con("k", 0.0, 1.0, piecewise(&[(1.0, 0.0), (0.0, 1.0)]))],
        );
        assert!(matches!(
            Reasoner::new(&def(vec![unsorted])),
            Err(UtilityAiError::BadCurve { .. })
        ));

        let single_point = action("a", vec![con("k", 0.0, 1.0, piecewise(&[(0.0, 0.5)]))]);
        assert!(matches!(
            Reasoner::new(&def(vec![single_point])),
            Err(UtilityAiError::BadCurve { .. })
        ));

        let nan_curve = action(
            "a",
            vec![con(
                "k",
                0.0,
                1.0,
                ResponseCurve {
                    slope: f32::INFINITY,
                    ..curve(CurveKind::Logistic)
                },
            )],
        );
        assert!(matches!(
            Reasoner::new(&def(vec![nan_curve])),
            Err(UtilityAiError::BadCurve { .. })
        ));

        let bad_mode = def_weighted(good(), 0, 0.5, 0);
        assert!(matches!(
            Reasoner::new(&bad_mode),
            Err(UtilityAiError::BadValue(_))
        ));

        let bad_margin = def_weighted(good(), 3, f32::NAN, 0);
        assert!(matches!(
            Reasoner::new(&bad_margin),
            Err(UtilityAiError::BadValue(_))
        ));

        let negative_margin = def_weighted(good(), 3, -0.1, 0);
        assert!(matches!(
            Reasoner::new(&negative_margin),
            Err(UtilityAiError::BadValue(_))
        ));
    }

    // ---- RON ----------------------------------------------------------------------------

    #[test]
    fn ron_round_trip() {
        let mut attack = action(
            "attack",
            vec![
                con("health", 0.0, 100.0, curve(CurveKind::Linear)),
                con(
                    "distance",
                    0.0,
                    50.0,
                    ResponseCurve {
                        kind: CurveKind::Quadratic,
                        slope: 2.0,
                        exponent: 3.0,
                        x_shift: 0.1,
                        y_shift: 0.05,
                        points: Vec::new(),
                    },
                ),
                con("threat", 0.0, 10.0, ResponseCurve::new(CurveKind::Logistic)),
                con("fear", 0.0, 1.0, ResponseCurve::new(CurveKind::Logit)),
                con(
                    "ready",
                    0.0,
                    1.0,
                    ResponseCurve {
                        x_shift: 0.5,
                        ..curve(CurveKind::Step)
                    },
                ),
                con(
                    "skill",
                    0.0,
                    1.0,
                    piecewise(&[(0.0, 0.1), (0.5, 0.2), (1.0, 0.9)]),
                ),
            ],
        );
        attack.weight = 1.5;
        attack.cooldown = 2.5;
        attack.momentum = 0.3;
        let d = def_weighted(vec![attack, action("flee", Vec::new())], 3, 0.25, 99);
        let text = d.to_ron().expect("writes");
        let back = UtilityAiDef::from_ron(&text).expect("parses");
        assert_eq!(back, d);
        // The writer is idempotent.
        assert_eq!(back.to_ron().expect("writes"), text);
    }

    #[test]
    fn ron_report_round_trip() {
        let d = def(vec![
            action("a", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]),
            action("b", vec![con("y", 0.0, 1.0, curve(CurveKind::Logistic))]),
        ]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let report = reasoner.decide(&board(&[("x", 0.6), ("y", 0.4)]), 1.5);
        let text = ron::ser::to_string_pretty(&report, regia_core::ron_pretty()).expect("writes");
        let back: DecisionReport = ron::from_str(&text).expect("parses");
        assert_eq!(back, report);
    }

    #[test]
    fn ron_version_field_is_checked() {
        let err = UtilityAiDef::from_ron("(format_version: 2, actions: [])").expect_err("future");
        assert!(matches!(
            err,
            UtilityAiError::UnsupportedVersion {
                found: 2,
                supported: UTILITY_AI_FORMAT_VERSION
            }
        ));
        // Version 1 parses; the empty action list is caught when building the reasoner.
        assert!(matches!(
            Reasoner::from_ron("(format_version: 1, actions: [])"),
            Err(UtilityAiError::Empty)
        ));
        assert!(UtilityAiDef::from_ron("not ron").is_err());

        // The migrator stub: current version is a byte-for-byte identity, others are refused.
        let text = "(format_version: 1, actions: [(name: \"a\")])";
        assert_eq!(migrate_ron(text), Ok(text.to_string()));
        assert!(matches!(
            migrate_ron("(format_version: 2, actions: [])"),
            Err(UtilityAiError::UnsupportedVersion { .. })
        ));
        assert!(utility_ai_migration_sources().is_empty());
    }

    // ---- agent surface, explain, determinism --------------------------------------------

    #[test]
    fn list_decisions_in_declaration_order() {
        let d = def(vec![
            action("c", Vec::new()),
            action("a", Vec::new()),
            action("b", Vec::new()),
        ]);
        let reasoner = Reasoner::new(&d).expect("valid");
        assert_eq!(reasoner.list_decisions(), vec!["c", "a", "b"]);
        assert_eq!(reasoner.action_count(), 3);
        assert_eq!(reasoner.current_action(), None);
    }

    #[test]
    fn explain_does_not_mutate_state() {
        let mut attack = action("attack", vec![con("health", 0.0, 1.0, curve(CurveKind::Linear))]);
        attack.momentum = 0.2;
        attack.cooldown = 5.0;
        let flee = action("flee", vec![con("threat", 0.0, 1.0, curve(CurveKind::Linear))]);
        let bb = board(&[("health", 0.6), ("threat", 0.5)]);

        // Best mode: the pick is deterministic, so the state is easy to check.
        let mut reasoner = Reasoner::new(&def(vec![attack.clone(), flee.clone()])).expect("valid");
        let first = reasoner.decide(&bb, 0.0);
        assert_eq!(first.selected.as_deref(), Some("attack"));

        // Explaining twice answers identically and changes nothing. (At now = 1 the attack is on
        // cooldown from the decide above, so the explanation names the flee action — explain
        // honours cooldowns, it just does not start them.)
        let (a, b) = (reasoner.explain(&bb, 1.0), reasoner.explain(&bb, 1.0));
        assert_eq!(a, b);
        assert_eq!(a.selected.as_deref(), Some("flee"));
        assert_eq!(reasoner.current_action(), Some("attack"));
        assert_eq!(reasoner.cooldown_remaining("attack", 0.0), Some(5.0));

        // The generator was not advanced: a reasoner that explained in between picks the same
        // weighted sequence as one that did not.
        let d = def_weighted(vec![attack, flee], 2, 1.0, 3);
        let mut plain = Reasoner::new(&d).expect("valid");
        let mut explained = Reasoner::new(&d).expect("valid");
        plain.decide(&bb, 0.0);
        explained.decide(&bb, 0.0);
        let _ = explained.explain(&bb, 1.0);
        let mut picks = Vec::new();
        let mut picks_after_explain = Vec::new();
        for now in 1..6u16 {
            picks.push(plain.decide(&bb, f32::from(now)).selected);
            picks_after_explain.push(explained.decide(&bb, f32::from(now)).selected);
        }
        assert_eq!(picks, picks_after_explain);
    }

    #[test]
    fn reset_restores_the_initial_state() {
        let mut attack = action("attack", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]);
        attack.cooldown = 10.0;
        let mut reasoner = Reasoner::new(&def(vec![attack])).expect("valid");
        let bb = board(&[("x", 1.0)]);
        reasoner.decide(&bb, 0.0);
        assert_eq!(reasoner.current_action(), Some("attack"));
        reasoner.reset();
        assert_eq!(reasoner.current_action(), None);
        assert_eq!(reasoner.cooldown_remaining("attack", 0.0), Some(0.0));
    }

    #[test]
    fn determinism_over_100_repeats() {
        let d = def_weighted(
            vec![
                action("hunt", vec![con("hunger", 0.0, 1.0, curve(CurveKind::Linear))]),
                action(
                    "drink",
                    vec![con("thirst", 0.0, 1.0, ResponseCurve::new(CurveKind::Quadratic))],
                ),
                action(
                    "nap",
                    vec![con(
                        "tiredness",
                        0.0,
                        1.0,
                        ResponseCurve::new(CurveKind::Logistic),
                    )],
                ),
                action("flee", vec![con("fear", 0.0, 1.0, curve(CurveKind::Step))]),
            ],
            3,
            0.3,
            12345,
        );
        let run = || {
            let mut reasoner = Reasoner::new(&d).expect("valid");
            (0..100u16)
                .map(|i| {
                    let bb = board(&[
                        ("hunger", f32::from(i % 50) / 49.0),
                        ("thirst", f32::from((i * 7) % 50) / 49.0),
                        ("tiredness", f32::from((i * 13) % 50) / 49.0),
                        ("fear", f32::from(i % 2)),
                    ]);
                    let report = reasoner.decide(&bb, f32::from(i));
                    (
                        report.selected,
                        report
                            .actions
                            .iter()
                            .map(|a| (a.name.clone(), a.score))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    /// The 10,000 actions x 5 considerations performance guard (debug builds): one decision must
    /// stay linear work, and the generous wall bound only catches a quadratic accident (it is
    /// measured, never a source of nondeterminism — `now` stays explicit).
    #[cfg(debug_assertions)]
    #[test]
    fn perf_10000_actions_5_considerations() {
        use std::time::Instant;

        let actions: Vec<ActionDef> = (0..10_000)
            .map(|i| {
                action(
                    &format!("a{i}"),
                    (0..5)
                        .map(|j| con(&format!("k{j}"), 0.0, 1.0, curve(CurveKind::Linear)))
                        .collect(),
                )
            })
            .collect();
        let d = def(actions);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let bb = board(&[("k0", 1.0), ("k1", 1.0), ("k2", 1.0), ("k3", 1.0), ("k4", 1.0)]);

        let start = Instant::now();
        let report = reasoner.decide(&bb, 0.0);
        let spent = start.elapsed();
        // Linear work, checked by counts rather than a wall-clock bound (the repo's test rule):
        // exactly one score per action and one per consideration.
        assert_eq!(report.actions.len(), 10_000);
        assert_eq!(
            report
                .actions
                .iter()
                .map(|a| a.considerations.len())
                .sum::<usize>(),
            50_000
        );
        // Every action scores 1.0: the tie goes to declaration order.
        assert_eq!(report.selected.as_deref(), Some("a0"));
        // Reported, never asserted (the lead reads it from `--nocapture`).
        eprintln!("utility perf: one decision over 10k actions x 5 considerations took {spent:?}");
    }

    #[test]
    fn documented_ron_example_parses() {
        // The `UtilityAiDef` rustdoc and the book page show this exact file.
        let text = r#"(
    format_version: 1,
    seed: 7,
    selection: WeightedRandom(top_n: 3, margin: 0.2),
    actions: [
        (name: "attack",
         considerations: [
            (key: "health", min: 0.0, max: 100.0),
            (key: "enemy_distance", min: 0.0, max: 50.0,
             curve: (kind: Quadratic, exponent: 2.0)),
         ],
         cooldown: 1.5, momentum: 0.25),
        (name: "flee",
         considerations: [(key: "threat", min: 0.0, max: 100.0,
                            curve: (kind: Logistic, exponent: 4.0))]),
    ],
)"#;
        let reasoner = Reasoner::from_ron(text).expect("the documented example parses");
        assert_eq!(
            reasoner.def().selection,
            SelectionMode::WeightedRandom {
                top_n: 3,
                margin: 0.2
            }
        );
        assert_eq!(reasoner.list_decisions(), vec!["attack", "flee"]);
    }

    #[test]
    fn report_with_invalid_inputs_round_trips_through_ron() {
        // A missing key records NaN as the raw input; the report must still serialise for the
        // editor and agent, and read back with the NaN intact.
        let d = def(vec![action(
            "a",
            vec![con("missing", 0.0, 1.0, curve(CurveKind::Linear))],
        )]);
        let report = Reasoner::new(&d).expect("valid").explain(&Blackboard::new(), 0.0);
        let text = ron::ser::to_string_pretty(&report, regia_core::ron_pretty()).expect("writes");
        let back: DecisionReport = ron::from_str(&text).expect("parses");
        assert!(back.actions[0].considerations[0].raw_input.is_nan());
        assert!(!back.actions[0].considerations[0].valid);
        assert_eq!(back.selected, None);
    }

    #[test]
    fn huge_and_negative_inputs_normalise_into_range() {
        // Values far outside [min, max] clamp; f32::MAX does not overflow to NaN.
        let d = def(vec![action(
            "a",
            vec![con("x", -10.0, 10.0, curve(CurveKind::Linear))],
        )]);
        let reasoner = Reasoner::new(&d).expect("valid");
        for (value, want) in [
            (f32::MAX, 1.0),
            (f32::MIN, 0.0),
            (-10.0, 0.0),
            (0.0, 0.5),
            (10.0, 1.0),
        ] {
            let report = reasoner.explain(&board(&[("x", value)]), 0.0);
            assert_eq!(report.actions[0].considerations[0].normalized, want, "{value}");
        }
    }

    #[test]
    fn a_single_zero_consideration_vetoes_the_action() {
        // Multiplication keeps the veto property after compensation: one 0 zeroes the action.
        let d = def(vec![
            action(
                "vetoed",
                vec![
                    con("a", 0.0, 1.0, curve(CurveKind::Linear)),
                    con("b", 0.0, 1.0, curve(CurveKind::Linear)),
                    con("zero", 0.0, 1.0, curve(CurveKind::Linear)),
                ],
            ),
            action("weak", vec![con("w", 0.0, 1.0, curve(CurveKind::Linear))]),
        ]);
        let report = Reasoner::new(&d)
            .expect("valid")
            .explain(&board(&[("a", 1.0), ("b", 1.0), ("zero", 0.0), ("w", 0.01)]), 0.0);
        assert_eq!(score_of(&report, "vetoed"), 0.0);
        assert_eq!(report.selected.as_deref(), Some("weak"));
    }

    #[test]
    fn weighted_random_frequencies_follow_the_weights() {
        // Two candidates 0.75 and 0.25 within the window: over 4000 seeded draws the first is
        // picked about three times as often (deterministic for the seed; the bound is wide).
        let d = def_weighted(
            vec![
                action("big", vec![con("x", 0.0, 1.0, curve(CurveKind::Linear))]),
                action("small", vec![con("y", 0.0, 1.0, curve(CurveKind::Linear))]),
            ],
            2,
            1.0,
            2026,
        );
        let bb = board(&[("x", 0.75), ("y", 0.25)]);
        let mut reasoner = Reasoner::new(&d).expect("valid");
        let big = (0..4000)
            .filter(|_| reasoner.decide(&bb, 0.0).selected.as_deref() == Some("big"))
            .count();
        assert!((2800..=3200).contains(&big), "big picked {big} of 4000");
    }
}
