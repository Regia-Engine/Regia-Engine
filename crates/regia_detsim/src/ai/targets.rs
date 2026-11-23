// SPDX-License-Identifier: MPL-2.0
//! Versioned target selection (audit gap AI-2): the engine owns the candidates, the cadence and the tie rule;
//! the game owns the score.
//!
//! A game that scores every known enemy for every unit every tick does `units x enemies` score calls per
//! tick (405,000 at 900 units on two teams). That loop cannot be pruned without changing results: an
//! out-of-range target still competes on value, and a tie goes to whichever enemy came first in the list.
//! So the cheaper rule is a *different* rule, and it is chosen by a version number the game bumps once:
//!
//! | Version | [`TargetRule`] | Candidates | When | Ties |
//! |---|---|---|---|---|
//! | 1 | [`TargetRule::Exhaustive`] | the caller's known-enemy list, in list order | every unit, every tick | the caller's ([`TargetHost::replaces`]) |
//! | 2 | [`TargetRule::Ringed`] | at most `max_candidates` eligible enemies of the innermost non-empty ring, ascending slot | on the unit's phase every `repick_ticks`, or at once when its target turns invalid | highest score, then lowest slot |
//!
//! `Exhaustive` exists so a game can move its pick into this module with its results (and its pinned
//! checksums) unchanged; `Ringed` is new behavior behind version 2 with its own pin.
//!
//! [`TargetSelector`] runs either rule over a [`DetSpatial`] index (det-core-01) and a [`TargetHost`] (the
//! game's score and eligibility). It keeps no per-unit state: each unit's current target is a slot the host
//! stores in its own sim state and passes in. Scratch buffers are the selector's, so a warm selector
//! allocates nothing. Everything is a pure function of the inputs: no clock, no threads, no hash containers.
//!
//! Original design (ADR-8517). Range-limited candidate sets and staggered re-evaluation of the target are
//! standard practice in game AI; the provenance row cites Mat Buckland, *Programming Game AI by Example*
//! (2005), whose example bot runs its target selection through an update regulator a few times a second
//! instead of every frame and chooses among the opponents in its sensory memory.

use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use super::AiCadence;
use crate::broadphase::{BroadBody, bodies_on_different_floors};
use crate::spatial::DetSpatial;

/// The rules version of [`TargetRule::Exhaustive`] (and of [`AiRules::default`]).
pub const EXHAUSTIVE_RULES_VERSION: u32 = 1;
/// The rules version of [`TargetRule::Ringed`].
pub const RINGED_RULES_VERSION: u32 = 2;
/// The newest rules version this build knows.
pub const LATEST_AI_RULES_VERSION: u32 = RINGED_RULES_VERSION;
/// Most rings a [`TargetRule::Ringed`] may list.
pub const MAX_TARGET_RINGS: usize = 8;

/// How units choose targets. Part of [`AiRules`]; a change of rule changes results, so it rides on the
/// rules version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TargetRule {
    /// Version 1: every unit scores every enemy of its known list, in list order, every tick, and the host's
    /// [`TargetHost::replaces`] decides between equal scores. Bit for bit the plain loop a game runs today.
    Exhaustive,
    /// Version 2: range rings, a candidate cap and a re-pick cadence.
    Ringed {
        /// Ring radii in metres (XZ, center to center), strictly ascending, finite and positive, at most
        /// [`MAX_TARGET_RINGS`]. Candidates come from the innermost ring that holds any; the outermost ring
        /// is also the range beyond which a held target is dropped.
        rings_m: Vec<f32>,
        /// Most candidates scored per pick (the lowest slots of the ring), at least 1.
        max_candidates: u16,
        /// A unit re-picks on ticks where `tick % repick_ticks == entity_id % repick_ticks` (at least 1),
        /// and keeps its target in between unless the target turns invalid.
        repick_ticks: u32,
    },
}

impl TargetRule {
    /// The `Ringed` rule of version 2: rings 80, 120 and 250 m, 8 candidates, a re-pick every 4 ticks.
    pub fn ringed_default() -> Self {
        TargetRule::Ringed {
            rings_m: vec![80.0, 120.0, 250.0],
            max_candidates: 8,
            repick_ticks: 4,
        }
    }

    /// The default rule of a rules version: 1 is [`TargetRule::Exhaustive`], 2 is
    /// [`TargetRule::ringed_default`]. Any other version is `ai_rules_unknown_version`.
    pub fn for_version(version: u32) -> Result<Self, AiRulesError> {
        match version {
            EXHAUSTIVE_RULES_VERSION => Ok(TargetRule::Exhaustive),
            RINGED_RULES_VERSION => Ok(TargetRule::ringed_default()),
            _ => Err(AiRulesError::UnknownVersion { version }),
        }
    }

    /// The lowest rules version that may run this rule.
    pub fn min_version(&self) -> u32 {
        match self {
            TargetRule::Exhaustive => EXHAUSTIVE_RULES_VERSION,
            TargetRule::Ringed { .. } => RINGED_RULES_VERSION,
        }
    }

    /// Check the rule's own settings (not the version).
    pub fn validate(&self) -> Result<(), AiRulesError> {
        let TargetRule::Ringed {
            rings_m,
            max_candidates,
            repick_ticks,
        } = self
        else {
            return Ok(());
        };
        let positive = rings_m.iter().all(|r| r.is_finite() && *r > 0.0);
        let ascending = rings_m.windows(2).all(|w| w[0] < w[1]);
        if rings_m.is_empty() || rings_m.len() > MAX_TARGET_RINGS || !positive || !ascending {
            return Err(AiRulesError::BadRings);
        }
        if *max_candidates == 0 {
            return Err(AiRulesError::BadMaxCandidates);
        }
        if *repick_ticks == 0 {
            return Err(AiRulesError::BadRepickTicks);
        }
        Ok(())
    }
}

/// The AI rules a match runs, with the version a lockstep host puts in its handshake.
///
/// `version` is the AI rules version (1 or 2 today), not the game's own sim version: a game maps its sim
/// version onto it, and moving from [`TargetRule::Exhaustive`] to [`TargetRule::Ringed`] is one bump of
/// both. Every peer must run the same rules; [`Self::check_peer`] refuses a peer whose build is older than
/// the rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiRules {
    /// The AI rules version.
    pub version: u32,
    /// The target rule.
    pub target: TargetRule,
}

impl Default for AiRules {
    /// Version 1 with [`TargetRule::Exhaustive`]: today's results.
    fn default() -> Self {
        AiRules {
            version: EXHAUSTIVE_RULES_VERSION,
            target: TargetRule::Exhaustive,
        }
    }
}

impl AiRules {
    /// The default rules of a version ([`TargetRule::for_version`]).
    pub fn for_version(version: u32) -> Result<Self, AiRulesError> {
        Ok(AiRules {
            version,
            target: TargetRule::for_version(version)?,
        })
    }

    /// Check the rules: a known version, a rule that version may run, and valid rule settings.
    pub fn validate(&self) -> Result<(), AiRulesError> {
        if self.version == 0 || self.version > LATEST_AI_RULES_VERSION {
            return Err(AiRulesError::UnknownVersion {
                version: self.version,
            });
        }
        let needs = self.target.min_version();
        if self.version < needs {
            return Err(AiRulesError::VersionTooOld {
                version: self.version,
                needs,
            });
        }
        self.target.validate()
    }

    /// Whether a peer whose build knows rules up to `peer_version` can run these rules. The host maps an
    /// error to its own handshake refusal; this crate has no net code.
    pub fn check_peer(&self, peer_version: u32) -> Result<(), AiRulesError> {
        let needs = self.version.max(self.target.min_version());
        if peer_version < needs {
            return Err(AiRulesError::PeerTooOld {
                peer: peer_version,
                needs,
            });
        }
        Ok(())
    }
}

/// Why [`AiRules`] were refused. Each variant has a stable lowercase code ([`Self::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRulesError {
    /// `ai_rules_unknown_version`: the version is 0 or newer than this build knows.
    UnknownVersion {
        /// The version asked for.
        version: u32,
    },
    /// `ai_rules_version_too_old`: the rules name a rule their version may not run (`Ringed` under 1).
    VersionTooOld {
        /// The rules' version.
        version: u32,
        /// The version the rule needs.
        needs: u32,
    },
    /// `ai_rules_peer_too_old`: a peer's build knows only older rules than the match runs.
    PeerTooOld {
        /// The peer's newest rules version.
        peer: u32,
        /// The version the match needs.
        needs: u32,
    },
    /// `ai_rules_bad_rings`: no rings, more than [`MAX_TARGET_RINGS`], or a radius that is not finite, not
    /// positive or not above the previous one.
    BadRings,
    /// `ai_rules_bad_max_candidates`: `max_candidates` is 0.
    BadMaxCandidates,
    /// `ai_rules_bad_repick_ticks`: `repick_ticks` is 0.
    BadRepickTicks,
}

impl AiRulesError {
    /// Stable lowercase code for host agent rows, logs and handshake refusals.
    pub fn code(self) -> &'static str {
        match self {
            AiRulesError::UnknownVersion { .. } => "ai_rules_unknown_version",
            AiRulesError::VersionTooOld { .. } => "ai_rules_version_too_old",
            AiRulesError::PeerTooOld { .. } => "ai_rules_peer_too_old",
            AiRulesError::BadRings => "ai_rules_bad_rings",
            AiRulesError::BadMaxCandidates => "ai_rules_bad_max_candidates",
            AiRulesError::BadRepickTicks => "ai_rules_bad_repick_ticks",
        }
    }
}

impl std::fmt::Display for AiRulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match *self {
            AiRulesError::UnknownVersion { version } => {
                write!(f, "{code}: AI rules version {version} is unknown to this build")
            }
            AiRulesError::VersionTooOld { version, needs } => {
                write!(
                    f,
                    "{code}: the target rule needs AI rules version {needs}, the rules say {version}"
                )
            }
            AiRulesError::PeerTooOld { peer, needs } => {
                write!(
                    f,
                    "{code}: the peer knows AI rules up to version {peer}, the match runs {needs}"
                )
            }
            AiRulesError::BadRings => write!(
                f,
                "{code}: rings must be 1 to {MAX_TARGET_RINGS} finite positive radii in ascending order"
            ),
            AiRulesError::BadMaxCandidates => write!(f, "{code}: max_candidates must be at least 1"),
            AiRulesError::BadRepickTicks => write!(f, "{code}: repick_ticks must be at least 1"),
        }
    }
}

impl std::error::Error for AiRulesError {}

/// The game's side of a pick. `unit` and `candidate` are slots of the host's row array (the slots of the
/// [`DetSpatial`] index).
///
/// Keep the methods pure functions of sim state: under [`TargetRule::Ringed`] the selector calls them for a
/// subset of candidates that depends on the rings, so a side effect would make results depend on the rule.
pub trait TargetHost {
    /// The value of `candidate` as `unit`'s target. Higher is better. A NaN score never wins.
    fn score(&mut self, unit: usize, candidate: usize) -> f32;

    /// Whether `candidate` may be `unit`'s target at all (alive, known to the unit). Called by
    /// [`TargetRule::Ringed`] for ring candidates and for a held target; `Exhaustive` trusts the known list.
    /// The selector itself already checks the enemy mask, the floor rule and the ring distance.
    fn eligible(&mut self, unit: usize, candidate: usize) -> bool {
        let _ = (unit, candidate);
        true
    }

    /// [`TargetRule::Exhaustive`]'s reduction: whether `challenger` (slot, score), met in list order,
    /// replaces the best so far. The default keeps the first of equal best scores (`>`) and never takes a
    /// NaN; a game whose loop keeps the last (`>=`) or breaks ties another way overrides it to stay bit for bit.
    fn replaces(&mut self, challenger: (usize, f32), best: Option<(usize, f32)>) -> bool {
        match best {
            None => !challenger.1.is_nan(),
            Some(best) => challenger.1 > best.1,
        }
    }
}

/// One unit's input to a pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetUnit {
    /// The unit's slot (its row in the host array and the [`DetSpatial`] index).
    pub slot: usize,
    /// The unit's entity id: it sets the re-pick phase, so it must be the same on every peer.
    pub entity_id: u32,
    /// The group bits of the teams the unit may target ([`super::team_group_mask`] of each enemy team).
    pub enemy_mask: u64,
}

/// What a pick did for one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickKind {
    /// `Exhaustive`: scored the known list (every tick).
    Exhaustive = 0,
    /// `Ringed`: the unit's phase was due and it picked.
    Scheduled = 1,
    /// `Ringed`: the held target turned invalid between picks, so it picked at once.
    Forced = 2,
    /// `Ringed`: kept a valid target between picks (no scores).
    Kept = 3,
    /// `Ringed`: no target and not due (no scores).
    Idle = 4,
}

/// The result of one pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPick {
    /// The unit's target after the pick (also written to the host's `current`).
    pub target: Option<usize>,
    /// What happened.
    pub kind: PickKind,
}

/// Counters of a [`TargetSelector`] (no timing), for a tick profiler row or an agent row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetPickReport {
    /// The last tick counted.
    pub tick: u64,
    /// Units handed to the selector.
    pub units: u64,
    /// Picks run (scored a candidate list): every unit under `Exhaustive`; scheduled plus forced under `Ringed`.
    pub picks: u64,
    /// [`TargetHost::score`] calls.
    pub scores_evaluated: u64,
    /// Candidates looked at before scoring: known-list entries (`Exhaustive`), or rows the spatial query
    /// delivered before the exact ring and eligibility tests (`Ringed`).
    pub candidates_seen: u64,
    /// `Ringed` picks forced by an invalid held target (dead, forgotten, beyond the outermost ring).
    pub forced_repicks: u64,
    /// `Ringed` units that kept their target between picks.
    pub kept_targets: u64,
}

impl TargetPickReport {
    fn add(&mut self, other: &TargetPickReport) {
        self.tick = other.tick;
        self.units += other.units;
        self.picks += other.picks;
        self.scores_evaluated += other.scores_evaluated;
        self.candidates_seen += other.candidates_seen;
        self.forced_repicks += other.forced_repicks;
        self.kept_targets += other.kept_targets;
    }
}

/// The highest score of `(slot, score)` pairs, ties to the lowest slot, NaN never; the same answer for any
/// order of the pairs. `-0.0` and `0.0` tie. This is [`TargetRule::Ringed`]'s reduction.
pub fn best_target(pairs: impl IntoIterator<Item = (usize, f32)>) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (slot, s) in pairs {
        if s.is_nan() {
            continue;
        }
        let better = match best {
            None => true,
            Some((b_slot, b)) => s > b || (s == b && slot < b_slot),
        };
        if better {
            best = Some((slot, s));
        }
    }
    best
}

/// Runs a [`TargetRule`] over the host's rows. One per sim; it holds the rules, scratch and counters, and no
/// per-unit state.
#[derive(Debug, Clone)]
pub struct TargetSelector {
    rules: AiRules,
    cadence: AiCadence,
    /// `Ringed`: `[radius, radius * radius]` per ring, innermost first.
    rings: Vec<[f32; 2]>,
    max_candidates: usize,
    /// Scratch: one pick's candidate slots, ascending.
    candidates: Vec<usize>,
    /// Counters since construction or [`Self::take_report`].
    report: TargetPickReport,
}

impl TargetSelector {
    /// A selector for validated `rules` ([`AiRules::validate`]).
    pub fn new(rules: AiRules) -> Result<Self, AiRulesError> {
        rules.validate()?;
        let (cadence, rings, max_candidates) = match &rules.target {
            TargetRule::Exhaustive => (AiCadence::new(1, 1), Vec::new(), 0),
            TargetRule::Ringed {
                rings_m,
                max_candidates,
                repick_ticks,
            } => (
                AiCadence::new(*repick_ticks, 1),
                rings_m.iter().map(|&r| [r, r * r]).collect(),
                usize::from(*max_candidates),
            ),
        };
        Ok(TargetSelector {
            rules,
            cadence,
            rings,
            max_candidates,
            candidates: Vec::with_capacity(max_candidates),
            report: TargetPickReport::default(),
        })
    }

    /// The rules.
    pub fn rules(&self) -> &AiRules {
        &self.rules
    }

    /// The counters since construction or the last [`Self::take_report`].
    pub fn report(&self) -> TargetPickReport {
        self.report
    }

    /// The counters, zeroing them.
    pub fn take_report(&mut self) -> TargetPickReport {
        std::mem::take(&mut self.report)
    }

    /// Whether `entity_id` re-picks on `tick` (always under `Exhaustive`).
    pub fn due(&self, tick: u64, entity_id: u32) -> bool {
        self.cadence.due_to_decide(tick, entity_id)
    }

    /// Pick one unit's target on `tick`. `current` is the unit's target slot as the host stores it (read,
    /// then overwritten with the result). `known` is the unit's known-enemy list (used by `Exhaustive` only;
    /// pass `&[]` under `Ringed`). `spatial` must be built from this tick's rows (used by `Ringed` only).
    pub fn pick<H: TargetHost + ?Sized>(
        &mut self,
        tick: u64,
        unit: TargetUnit,
        current: &mut Option<usize>,
        known: &[usize],
        spatial: &mut DetSpatial,
        host: &mut H,
    ) -> TargetPick {
        let mut counts = TargetPickReport {
            tick,
            units: 1,
            ..TargetPickReport::default()
        };
        let pick = if matches!(self.rules.target, TargetRule::Exhaustive) {
            exhaustive(unit, known, host, &mut counts)
        } else {
            self.ringed(tick, unit, *current, spatial, host, &mut counts)
        };
        *current = pick.target;
        self.report.add(&counts);
        pick
    }

    /// Pick for every unit of a tick, in the order given: `targets[i]` is `units[i]`'s current target (pairs
    /// past the shorter slice are skipped) and `known_of(slot)` its known-enemy list. Returns this call's
    /// counters (also added to [`Self::report`]).
    pub fn pick_tick<'k, H: TargetHost + ?Sized>(
        &mut self,
        tick: u64,
        units: &[TargetUnit],
        targets: &mut [Option<usize>],
        mut known_of: impl FnMut(usize) -> &'k [usize],
        spatial: &mut DetSpatial,
        host: &mut H,
    ) -> TargetPickReport {
        let _span =
            tracing::trace_span!("regia_detsim::ai::pick_targets", tick, units = units.len()).entered();
        let before = self.report;
        for (unit, current) in units.iter().zip(targets.iter_mut()) {
            let known = known_of(unit.slot);
            self.pick(tick, *unit, current, known, spatial, host);
        }
        let after = self.report;
        TargetPickReport {
            tick,
            units: after.units - before.units,
            picks: after.picks - before.picks,
            scores_evaluated: after.scores_evaluated - before.scores_evaluated,
            candidates_seen: after.candidates_seen - before.candidates_seen,
            forced_repicks: after.forced_repicks - before.forced_repicks,
            kept_targets: after.kept_targets - before.kept_targets,
        }
    }

    fn ringed<H: TargetHost + ?Sized>(
        &mut self,
        tick: u64,
        unit: TargetUnit,
        current: Option<usize>,
        spatial: &mut DetSpatial,
        host: &mut H,
        counts: &mut TargetPickReport,
    ) -> TargetPick {
        let due = self.cadence.due_to_decide(tick, unit.entity_id);
        let kind = if due {
            PickKind::Scheduled
        } else {
            let Some(held) = current else {
                return TargetPick {
                    target: None,
                    kind: PickKind::Idle,
                };
            };
            let outer = self.rings.last().map_or(0.0, |ring| ring[1]);
            if valid_target(spatial, unit, held, outer, host) {
                counts.kept_targets += 1;
                return TargetPick {
                    target: Some(held),
                    kind: PickKind::Kept,
                };
            }
            counts.forced_repicks += 1;
            PickKind::Forced
        };
        counts.picks += 1;
        self.gather(spatial, unit, host, counts);
        let target = best_target(self.candidates.iter().map(|&c| {
            counts.scores_evaluated += 1;
            (c, host.score(unit.slot, c))
        }))
        .map(|(slot, _)| slot);
        TargetPick { target, kind }
    }

    /// Fill `candidates` with at most `max_candidates` eligible enemies of the innermost non-empty ring, in
    /// ascending slot order.
    fn gather<H: TargetHost + ?Sized>(
        &mut self,
        spatial: &mut DetSpatial,
        unit: TargetUnit,
        host: &mut H,
        counts: &mut TargetPickReport,
    ) {
        self.candidates.clear();
        let Some(center) = spatial.row(unit.slot).map(|r| r.body) else {
            return;
        };
        let cap = self.max_candidates;
        let candidates = &mut self.candidates;
        for &[r, r_sq] in &self.rings {
            let mut seen = 0u64;
            let _ = spatial.within_mut(center, r, unit.enemy_mask, |sp, slot| {
                seen += 1;
                if slot == unit.slot {
                    return ControlFlow::Continue(());
                }
                // `within` is conservative: the exact center-distance test decides.
                let inside = sp
                    .row(slot)
                    .is_some_and(|row| dist_sq(&center, &row.body) <= r_sq);
                if inside && host.eligible(unit.slot, slot) {
                    candidates.push(slot);
                }
                if candidates.len() >= cap {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            counts.candidates_seen += seen;
            if !candidates.is_empty() {
                return;
            }
        }
    }
}

/// `Exhaustive`: the plain loop over the known list with the host's reduction.
fn exhaustive<H: TargetHost + ?Sized>(
    unit: TargetUnit,
    known: &[usize],
    host: &mut H,
    counts: &mut TargetPickReport,
) -> TargetPick {
    counts.picks += 1;
    counts.candidates_seen += known.len() as u64;
    counts.scores_evaluated += known.len() as u64;
    let mut best: Option<(usize, f32)> = None;
    for &candidate in known {
        let s = host.score(unit.slot, candidate);
        if host.replaces((candidate, s), best) {
            best = Some((candidate, s));
        }
    }
    TargetPick {
        target: best.map(|(slot, _)| slot),
        kind: PickKind::Exhaustive,
    }
}

/// Squared XZ center distance, the exact ring test: `dx * dx + dz * dz` in f32 (no fused multiply-add).
fn dist_sq(a: &BroadBody, b: &BroadBody) -> f32 {
    let dx = b.pos[0] - a.pos[0];
    let dz = b.pos[2] - a.pos[2];
    dx * dx + dz * dz
}

/// A held target is valid when it would be a candidate of the outermost ring: another row, in the enemy
/// mask, on the unit's floor, within the ring, and eligible to the host.
fn valid_target<H: TargetHost + ?Sized>(
    spatial: &DetSpatial,
    unit: TargetUnit,
    held: usize,
    outer_sq: f32,
    host: &mut H,
) -> bool {
    if held == unit.slot {
        return false;
    }
    let (Some(me), Some(it)) = (spatial.row(unit.slot), spatial.row(held)) else {
        return false;
    };
    let rule = spatial.config().floor_rule;
    it.group_mask & unit.enemy_mask != 0
        && !bodies_on_different_floors(rule, &me.body, &it.body)
        && dist_sq(&me.body, &it.body) <= outer_sq
        && host.eligible(unit.slot, held)
}
