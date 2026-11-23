// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/difficulty.rs, commit c9513623f, tag regia-absorb-4;
// ir-sim/src/pilots.rs lines 87-117; ir-sim/src/ai/mod.rs lines 1396-1408), ADR-8012.
// Mark: adapt (difficulty.rs lines 1-45, 61-185 and 196-501, plan row S-11, ADR-8462). The profile, its masks and
// its Easy/Medium/Hard numbers are DO27's apart from rustfmt, doc comments and these named changes:
// - `PerceptionParams` (lines 46-60) is S-10's `regia_detsim::ai::PerceptionParams` (ADR-8395), imported here;
// - the commander pacing (`CommandPace`: build cadence, resource reserve, refits, retreat band, wave size) is
//   DO27's real-time-strategy commander, game code: it is not copied, and the `command` field is not on this
//   profile; DO27's tests of it (`each_level_changes_the_command_knobs`, the pacing half of
//   `normal_equals_todays_behavior`) stay in DO27;
// - `AiLevel` (DO27's pilots module, lines 87-117) moves here with `as_str` and `parse`;
// - `DifficultyProfile::allows` and `DifficultyProfile::score_options` name the repertoire and coordination
//   filter and the bias DO27's AI pipeline applies inline (mod.rs lines 1396-1408);
// - `combat_stats_are_identical_across_difficulties` read DO27's hull and weapon tables (game content); the port
//   checks the same invariant on the profile itself (`the_profile_carries_no_stat_field`);
// - `mask_bits_are_stable_and_distinct` uses a `BTreeSet` (lockstep crates do not iterate hash containers);
// - DO27's game names (hull, unit, mode and design-document names) are dropped from the comments.

//! Difficulty profiles: one source of truth for the Easy/Medium/Hard decision-quality parameters.
//!
//! Difficulty is **decision quality only**: the same hull, ammo and resources in the same fight play better on
//! Hard, never with better numbers. A profile tunes reaction time, awareness, aim error, which tactics a unit may
//! pick, which team abilities it has and how aggressive or cautious it is; it has no hull, ammunition or damage
//! field at all.

use serde::{Deserialize, Serialize};

use super::perception::PerceptionParams;
use super::scheduler::{score_options, score_options_idx};
use super::tactics::bias;
use super::utility::{AiOption, Inputs, OptionId};
use super::utility_catalogue::{InputValues, OptionIdx, OptionMask, UtilityCatalogue};

/// How sharp the AI is. `Medium` is "normal" (the default level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AiLevel {
    /// The forgiving level.
    Easy,
    /// The normal level.
    #[default]
    Medium,
    /// The sharpest level.
    Hard,
}

impl AiLevel {
    /// Every level, easiest first.
    pub const ALL: [AiLevel; 3] = [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard];

    /// Canonical wire string: "easy" | "medium" | "hard".
    pub fn as_str(self) -> &'static str {
        match self {
            AiLevel::Easy => "easy",
            AiLevel::Medium => "medium",
            AiLevel::Hard => "hard",
        }
    }

    /// Parse a lobby or settings level name (case-insensitive, trimmed). "normal" is a synonym for
    /// [`AiLevel::Medium`].
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "easy" => Some(AiLevel::Easy),
            "medium" | "normal" => Some(AiLevel::Medium),
            "hard" => Some(AiLevel::Hard),
            _ => None,
        }
    }
}

/// Bitmask over [`OptionId`]: which tactics options a level may pick (bit [`OptionId::bit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepertoireMask(pub u16);

impl RepertoireMask {
    /// No option.
    pub fn empty() -> Self {
        RepertoireMask(0)
    }

    /// Every option.
    pub fn all() -> Self {
        RepertoireMask(u16::MAX)
    }

    /// This mask with `id` added.
    pub fn with(mut self, id: OptionId) -> Self {
        self.0 |= 1 << id.bit();
        self
    }

    /// Whether the mask holds `id`.
    pub fn contains(self, id: OptionId) -> bool {
        self.0 & (1 << id.bit()) != 0
    }
}

/// Team-level coordination abilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CoordinationMask {
    /// The unit joins its team's focus target ([`OptionId::FocusFire`]).
    pub focus_fire: bool,
    /// The unit may flank ([`OptionId::Flank`]).
    pub flank: bool,
    /// The team staggers its attacks.
    pub stagger: bool,
}

/// Human-like aim error. Never a damage buff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AimParams {
    /// The lead-point miss, as a share of the ideal lead distance (the target's travel during the shot's flight).
    pub lead_error_scale: f32,
    /// Ticks before the reticle settles on a new target.
    pub tracking_lag_ticks: u32,
    /// The spread cone, milliradians (scaled by a random 0.5..1.5 per shot).
    pub spread_mrad: f32,
}

/// Score and behaviour thresholds: the knees of the tactics curves and the aggression/caution bias.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Allies-to-enemies ratio at which pushing becomes attractive.
    pub push_force_ratio: f32,
    /// Hull fraction below which retreating becomes attractive.
    pub retreat_hull_pct: f32,
    /// Enemies-to-allies ratio at which the unit counts itself outnumbered.
    pub outnumbered_ratio: f32,
    /// Multiplies the Push score.
    pub aggression: f32,
    /// Multiplies the Retreat and Defend scores.
    pub defence: f32,
}

/// Resource-collection parameters (the [`OptionId::CollectScrap`] option).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScrapParams {
    /// Route between pickups with the whole pool in mind (not just the nearest).
    pub routing: bool,
    /// How far the unit looks for a pickup, metres.
    pub search_radius_m: f32,
}

/// One profile object; no per-difficulty code paths, no stat divergence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DifficultyProfile {
    /// The level this profile is.
    pub level: AiLevel,
    /// Pure tick count (deterministic); about 600/350/200 ms at 60 Hz.
    pub reaction_delay_ticks: u32,
    /// Awareness: how long sightings are trusted and how often line of sight is re-checked.
    pub perception: PerceptionParams,
    /// Aim error.
    pub aim: AimParams,
    /// The tactics options the level may pick.
    pub repertoire: RepertoireMask,
    /// The team abilities the level has.
    pub coordination: CoordinationMask,
    /// Curve knees and the aggression/caution bias.
    pub thresholds: Thresholds,
    /// Resource collection.
    pub scrap: ScrapParams,
}

impl DifficultyProfile {
    /// Easy: tuned for new players. Reaction is the slow end of the 400-600 ms band (36 ticks at 60 Hz), accuracy
    /// is the worst of the three levels, there is no focus fire and no flanking, and units pull out early at 30 %
    /// hull so a new player's mistakes do not snowball.
    pub fn easy() -> Self {
        DifficultyProfile {
            level: AiLevel::Easy,
            reaction_delay_ticks: 36,
            perception: PerceptionParams {
                sighting_trust_ticks: 30,
                confidence_decay: 0.90,
                los_recheck_interval_ticks: 24,
                forget_confidence: 0.5,
            },
            aim: AimParams {
                lead_error_scale: 1.0,
                tracking_lag_ticks: 20,
                spread_mrad: 35.0,
            },
            repertoire: RepertoireMask::empty()
                .with(OptionId::Push)
                .with(OptionId::Hold)
                .with(OptionId::Regroup),
            coordination: CoordinationMask::default(),
            thresholds: Thresholds {
                push_force_ratio: 1.0,
                retreat_hull_pct: 0.30,
                outnumbered_ratio: 2.0,
                aggression: 1.1,
                defence: 0.8,
            },
            scrap: ScrapParams {
                routing: false,
                search_radius_m: 30.0,
            },
        }
    }

    /// Medium ("normal"). Its perception is [`PerceptionParams::default`].
    pub fn medium() -> Self {
        DifficultyProfile {
            level: AiLevel::Medium,
            reaction_delay_ticks: 21,
            perception: PerceptionParams {
                sighting_trust_ticks: 60,
                confidence_decay: 0.96,
                los_recheck_interval_ticks: 12,
                forget_confidence: 0.35,
            },
            aim: AimParams {
                lead_error_scale: 0.5,
                tracking_lag_ticks: 8,
                spread_mrad: 18.0,
            },
            repertoire: DifficultyProfile::easy()
                .repertoire
                .with(OptionId::Strafe)
                .with(OptionId::Kite)
                .with(OptionId::Retreat),
            coordination: CoordinationMask {
                focus_fire: true,
                flank: false,
                stagger: false,
            },
            thresholds: Thresholds {
                push_force_ratio: 1.3,
                retreat_hull_pct: 0.20,
                outnumbered_ratio: 2.0,
                aggression: 1.0,
                defence: 1.0,
            },
            scrap: ScrapParams {
                routing: false,
                search_radius_m: 45.0,
            },
        }
    }

    /// Hard: every option, every team ability, the fastest reaction and the smallest aim error.
    pub fn hard() -> Self {
        DifficultyProfile {
            level: AiLevel::Hard,
            reaction_delay_ticks: 12,
            perception: PerceptionParams {
                sighting_trust_ticks: 120,
                confidence_decay: 0.99,
                los_recheck_interval_ticks: 6,
                forget_confidence: 0.2,
            },
            aim: AimParams {
                lead_error_scale: 0.2,
                tracking_lag_ticks: 3,
                spread_mrad: 8.0,
            },
            repertoire: RepertoireMask::all(),
            coordination: CoordinationMask {
                focus_fire: true,
                flank: true,
                stagger: true,
            },
            thresholds: Thresholds {
                push_force_ratio: 1.5,
                // Above Easy's 0.30: the ordering tests pin Hard > Easy, so Hard stays the most careful about
                // saving its hulls (Medium sits at 0.20 and is not part of that pin).
                retreat_hull_pct: 0.35,
                outnumbered_ratio: 2.0,
                aggression: 0.9,
                defence: 1.2,
            },
            scrap: ScrapParams {
                routing: true,
                search_radius_m: 60.0,
            },
        }
    }

    /// The profile of `level`.
    pub fn for_level(level: AiLevel) -> Self {
        match level {
            AiLevel::Easy => Self::easy(),
            AiLevel::Medium => Self::medium(),
            AiLevel::Hard => Self::hard(),
        }
    }

    /// Whether this level may pick `id`: the option is in its repertoire, and [`OptionId::Flank`] and
    /// [`OptionId::FocusFire`] also need the matching coordination ability.
    pub fn allows(&self, id: OptionId) -> bool {
        self.repertoire.contains(id)
            && match id {
                OptionId::Flank => self.coordination.flank,
                OptionId::FocusFire => self.coordination.focus_fire,
                _ => true,
            }
    }

    /// Score `catalogue` for one unit at this level: the options it [`allows`](Self::allows), each scaled by the
    /// level's [`bias`] and clamped to 0..1, pushed to `out` in catalogue order ([`score_options`]).
    ///
    /// This is the scorer to call from [`super::AiCore::decide`]'s closure with the catalogue of
    /// [`super::catalogue`] for this profile's thresholds.
    pub fn score_options(&self, catalogue: &[AiOption], inputs: &Inputs, out: &mut Vec<(OptionId, f32)>) {
        score_options(
            catalogue,
            inputs,
            |id| self.allows(id),
            |id| bias(id, &self.thresholds),
            out,
        );
    }

    /// The repertoire as a mask over the DO27 preset's [`OptionIdx`] (the same bits as [`RepertoireMask`]).
    pub fn option_mask(&self) -> OptionMask {
        OptionMask::from(self.repertoire)
    }

    /// [`Self::allows`] by preset index: `false` for an index past the preset's twelve options.
    pub fn allows_idx(&self, idx: OptionIdx) -> bool {
        OptionId::from_idx(idx).is_some_and(|id| self.allows(id))
    }

    /// [`Self::score_options`] over the DO27 preset by index: `catalogue` is
    /// [`UtilityCatalogue::do27_preset`] for this profile's thresholds and `inputs` is [`Inputs::values`]; the
    /// scores equal [`Self::score_options`]'s bit for bit, keyed by [`OptionId::idx`]. An index past the
    /// preset's twelve is never allowed.
    pub fn score_options_idx(
        &self,
        catalogue: &UtilityCatalogue,
        inputs: &InputValues,
        out: &mut Vec<(OptionIdx, f32)>,
    ) {
        score_options_idx(
            catalogue,
            inputs,
            |idx| self.allows_idx(idx),
            |idx| OptionId::from_idx(idx).map_or(1.0, |id| bias(id, &self.thresholds)),
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_is_decision_quality_not_stats() {
        let easy = DifficultyProfile::easy();
        let hard = DifficultyProfile::hard();
        // Reaction time tightens with skill.
        assert!(hard.reaction_delay_ticks < easy.reaction_delay_ticks);
        // Awareness improves with skill, but is still the same *information*.
        assert!(hard.perception.confidence_decay > easy.perception.confidence_decay);
        assert!(hard.perception.sighting_trust_ticks > easy.perception.sighting_trust_ticks);
        // Aim error shrinks with skill (human-like, not a damage buff).
        assert!(hard.aim.lead_error_scale < easy.aim.lead_error_scale);
        assert!(hard.aim.spread_mrad < easy.aim.spread_mrad);
        // The repertoire grows; nothing about hull/ammo/scrap changes.
        assert!(hard.repertoire.contains(OptionId::Flank));
        assert!(!easy.repertoire.contains(OptionId::Flank));
        assert!(!easy.repertoire.contains(OptionId::Dogfight));
    }

    #[test]
    fn mask_bits_are_stable_and_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for id in OptionId::ALL {
            let bit = 1u16 << id.bit();
            assert!(seen.insert(bit), "bit for {id:?} collided");
            assert!(bit < u16::MAX / 2 + 1);
        }
    }

    /// No hull, ammo, resource or damage number changes between levels: the profile carries no such field (DO27
    /// checked its stat tables take no level; here the profile's own serialized fields are checked).
    #[test]
    fn the_profile_carries_no_stat_field() {
        for level in AiLevel::ALL {
            let wire = serde_json::to_value(DifficultyProfile::for_level(level)).unwrap_or_default();
            let mut keys = Vec::new();
            let mut stack = vec![&wire];
            while let Some(v) = stack.pop() {
                if let Some(map) = v.as_object() {
                    for (k, child) in map {
                        keys.push(k.clone());
                        stack.push(child);
                    }
                }
            }
            assert!(keys.len() > 20, "{keys:?}");
            for stat in [
                "hull_points",
                "hp",
                "max_hull",
                "ammo",
                "damage",
                "dps",
                "armor",
                "speed",
            ] {
                assert!(
                    !keys
                        .iter()
                        .any(|k| k == stat || k.starts_with(&format!("{stat}_"))),
                    "{level:?} carries the stat field `{stat}`: {keys:?}"
                );
            }
        }
    }

    /// Medium is "normal": its reaction delay and aim error keep DO27's Medium values, and its perception is the
    /// sighting board's default.
    #[test]
    fn normal_equals_todays_behavior() {
        let normal = DifficultyProfile::medium();
        assert_eq!(normal.reaction_delay_ticks, 21);
        assert_eq!(
            (
                normal.aim.lead_error_scale,
                normal.aim.tracking_lag_ticks,
                normal.aim.spread_mrad
            ),
            (0.5, 8, 18.0)
        );
        assert_eq!(normal.perception, PerceptionParams::default());
        assert_eq!(AiLevel::default(), AiLevel::Medium);
        assert_eq!(DifficultyProfile::for_level(AiLevel::default()), normal);
    }

    /// The table is plain data: it round-trips through serde unchanged, so what the lobby stores per seat is what
    /// the sim reads. Two reads of the same level are identical (no RNG, no clock in the table).
    #[test]
    fn difficulty_table_is_plain_deterministic_data() {
        for level in [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard] {
            let profile = DifficultyProfile::for_level(level);
            assert_eq!(profile, DifficultyProfile::for_level(level));
            let wire = serde_json::to_string(&profile);
            assert!(wire.is_ok(), "profile serializes");
            let back: Result<DifficultyProfile, _> = serde_json::from_str(&wire.unwrap_or_default());
            assert_eq!(back.ok(), Some(profile));
        }
    }

    /// Every decision-quality dimension is ordered Easy → Medium → Hard: reaction, awareness/memory, aim error,
    /// repertoire, coordination, aggression-vs-caution and economy. None of these touches stats.
    #[test]
    fn every_decision_knob_is_ordered_by_difficulty() {
        let easy = DifficultyProfile::easy();
        let medium = DifficultyProfile::medium();
        let hard = DifficultyProfile::hard();

        // Reaction time: Hard reacts faster.
        assert!(hard.reaction_delay_ticks < medium.reaction_delay_ticks);
        assert!(medium.reaction_delay_ticks < easy.reaction_delay_ticks);

        // Awareness / memory: Hard trusts longer, forgets slower, re-checks LOS more often.
        assert!(easy.perception.sighting_trust_ticks < medium.perception.sighting_trust_ticks);
        assert!(medium.perception.sighting_trust_ticks < hard.perception.sighting_trust_ticks);
        assert!(easy.perception.confidence_decay < medium.perception.confidence_decay);
        assert!(medium.perception.confidence_decay < hard.perception.confidence_decay);
        assert!(hard.perception.los_recheck_interval_ticks < medium.perception.los_recheck_interval_ticks);
        assert!(medium.perception.los_recheck_interval_ticks < easy.perception.los_recheck_interval_ticks);

        // Aim error: Hard misses less (smaller lead error and spread, less lag).
        assert!(hard.aim.lead_error_scale < medium.aim.lead_error_scale);
        assert!(medium.aim.lead_error_scale < easy.aim.lead_error_scale);
        assert!(hard.aim.spread_mrad < medium.aim.spread_mrad);
        assert!(medium.aim.spread_mrad < easy.aim.spread_mrad);
        assert!(hard.aim.tracking_lag_ticks < medium.aim.tracking_lag_ticks);

        // Repertoire grows with skill.
        let easy_n = OptionId::ALL
            .iter()
            .filter(|id| easy.repertoire.contains(**id))
            .count();
        let medium_n = OptionId::ALL
            .iter()
            .filter(|id| medium.repertoire.contains(**id))
            .count();
        let hard_n = OptionId::ALL
            .iter()
            .filter(|id| hard.repertoire.contains(**id))
            .count();
        assert!(
            easy_n < medium_n && medium_n < hard_n,
            "{easy_n} < {medium_n} < {hard_n}"
        );

        // Coordination: Easy feeds, Medium focuses fire, Hard flanks + staggers.
        assert!(!easy.coordination.focus_fire && !easy.coordination.flank && !easy.coordination.stagger);
        assert!(medium.coordination.focus_fire && !medium.coordination.flank);
        assert!(hard.coordination.focus_fire && hard.coordination.flank && hard.coordination.stagger);

        // Aggression vs caution: Hard pushes more cautiously, defends/retreats more readily.
        assert!(hard.thresholds.aggression < medium.thresholds.aggression);
        assert!(medium.thresholds.aggression < easy.thresholds.aggression);
        assert!(easy.thresholds.defence < medium.thresholds.defence);
        assert!(medium.thresholds.defence < hard.thresholds.defence);
        assert!(hard.thresholds.retreat_hull_pct > easy.thresholds.retreat_hull_pct);

        // Economy efficiency: only Hard routes pool-aware, and the search reach widens with skill.
        assert!(!easy.scrap.routing && !medium.scrap.routing && hard.scrap.routing);
        assert!(easy.scrap.search_radius_m < medium.scrap.search_radius_m);
        assert!(medium.scrap.search_radius_m < hard.scrap.search_radius_m);
    }

    #[test]
    fn level_names_round_trip() {
        for level in AiLevel::ALL {
            assert_eq!(AiLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(AiLevel::parse(" Normal "), Some(AiLevel::Medium));
        assert_eq!(AiLevel::parse("nightmare"), None);
    }
}
