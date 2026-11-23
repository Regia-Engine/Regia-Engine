// SPDX-License-Identifier: MPL-2.0
//! Rule sets: a match mode as data (`.rules.ron` v1, ADR-8694).
//!
//! A [`RuleSet`] names a skeleton ([`MatchMode`] lobby id), may extend another rule set, and overrides
//! any session setting field by field ([`SessionPatch`]: every field is optional, an absent field
//! keeps the parent's value). It also lists its team definitions, its win conditions and a typed
//! option schema ([`OptionDef`]) that playlist rows fill in. [`RuleSetLibrary::resolve`] walks the
//! `extends` chain (at most [`MAX_EXTENDS_DEPTH`] parents, a cycle is refused), applies the patches
//! from the root down over the Rust defaults, validates the result and the row's options, and returns
//! a [`ResolvedRules`]: plain settings for [`crate::session::MatchSession::new`] plus the resolved
//! option values.
//!
//! Resolution happens before the match, never inside a step. Floats are parsed once by the loader
//! straight to `f32` (the rule set's own types keep every float out of serde's buffered enum path,
//! which would read it as `f64` first), and option values are parsed with `str::parse::<f32>`.
//! [`ResolvedRules::content_hash`] hashes the canonical bytes of the resolved settings, bit for bit,
//! so peers whose rules differ refuse the session at the content handshake.
//!
//! This module is plain data: the file reader (`regia_framework::ruleset_file`) parses the text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::elimination::{Alliance, EliminationRules};
use crate::match_flow::{FlowSettings, MatchClock};
use crate::mode::MatchMode;
use crate::objective::{HillVolume, ObjectiveSettings};
use crate::phases::{PhaseSettings, RoundSettings, TimeUp};
use crate::playlist::PlaylistEntry;
use crate::race::{Checkpoint, RaceSettings};
use crate::session::{MatchSession, SessionSettings};
use crate::spawn::{RespawnPolicy, RespawnRules, SpawnPolicy, SpawnWeights, TeamRespawn};
use crate::teams::{TeamDef, TeamRules, TeamSwitch, default_team_defs};
use crate::victory::{SurrenderRules, VictoryCondition, VictoryRules};
use crate::waves::{CoopSuccess, DifficultyRow, WaveGroup, WaveRow, WaveSettings, WaveStart};
use crate::win_condition::{StockRule, WinRuleState, clamp_lives};
use crate::zone::{ZonePhase, ZoneSchedule};
use crate::zones::{ZoneMode, ZoneRules};

mod strict;

/// The `.rules.ron` version this build reads and writes.
pub const RULES_RON_FORMAT_VERSION: u32 = 1;

/// A rule set may have at most this many ancestors (`extends` links).
pub const MAX_EXTENDS_DEPTH: usize = 4;

/// At most this many options in one resolved schema.
pub const MAX_OPTIONS: usize = 64;

/// At most this many win conditions in one rule set.
pub const MAX_WIN_CONDITIONS: usize = 8;

/// At most this many team definitions in one rule set.
pub const MAX_TEAM_DEFS: usize = 64;

/// At most this many strategy victory conditions in one rule set (`session.victory.conditions`).
pub const MAX_VICTORY_CONDITIONS: usize = 16;

/// At most this many alliances in one rule set (`session.elimination.alliances`).
pub const MAX_ALLIANCES: usize = 255;

/// Longest rule-set id or option key, in bytes.
pub const MAX_ID_BYTES: usize = 64;

/// Longest option text value (a `string` option, a default, a choice), in bytes.
pub const MAX_OPTION_TEXT_BYTES: usize = 256;

/// The largest `.rules.ron` text a loader accepts, in bytes (mods and downloaded content are
/// untrusted).
pub const MAX_RULES_RON_BYTES: usize = 256 * 1024;

/// Playlist row option keys that belong to the playlist itself (the vote's player bounds), never to
/// a rule set's schema: they pass through without validation.
pub const RESERVED_ROW_KEYS: [&str; 2] = ["min_players", "max_players"];

/// Error codes of rule-set loading and resolution (agent protocol, `regia rules check`).
pub mod codes {
    /// The text is not RON of the `.rules.ron` shape (an unknown field included).
    pub const PARSE: &str = "ruleset_parse";
    /// A `format_version` this build does not read.
    pub const VERSION: &str = "ruleset_version";
    /// The text is larger than `MAX_RULES_RON_BYTES`.
    pub const TOO_LARGE: &str = "ruleset_too_large";
    /// A value of the rule set itself cannot run (bad id, unknown skeleton, a setting out of range).
    pub const INVALID: &str = "ruleset_invalid";
    /// Two files declare the same id.
    pub const DUPLICATE: &str = "ruleset_duplicate";
    /// No rule set (and no skeleton) has this id, or `extends` names a missing parent.
    pub const UNKNOWN: &str = "ruleset_unknown";
    /// The `extends` chain comes back to a rule set already on it.
    pub const CYCLE: &str = "ruleset_cycle";
    /// The `extends` chain is longer than `MAX_EXTENDS_DEPTH`.
    pub const DEPTH: &str = "ruleset_depth";
    /// A row option the schema does not declare.
    pub const OPTION_UNKNOWN: &str = "ruleset_option_unknown";
    /// An option value outside its `min`/`max`.
    pub const OPTION_RANGE: &str = "ruleset_option_range";
    /// An option value that does not parse as its kind (or an enum value outside `choices`).
    pub const OPTION_KIND: &str = "ruleset_option_kind";

    /// Every code, sorted.
    pub const ALL: [&str; 11] = [
        CYCLE,
        DEPTH,
        DUPLICATE,
        INVALID,
        OPTION_KIND,
        OPTION_RANGE,
        OPTION_UNKNOWN,
        PARSE,
        TOO_LARGE,
        UNKNOWN,
        VERSION,
    ];
}

/// Why a rule set did not load or resolve: a stable code, the field path and a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSetError {
    /// One of [`codes::ALL`].
    pub code: String,
    /// The field the error is about (`options.capture_limit`, `session.objectives.flag_touch_radius`,
    /// `extends`), prefixed with the rule set's id when one is known (`ctf_long: extends`).
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl RuleSetError {
    /// An error with `code` at `path`.
    pub fn new(code: &str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            path: path.into(),
            message: message.into(),
        }
    }

    fn at(mut self, id: &str) -> Self {
        if !id.is_empty() && !self.path.starts_with(&format!("{id}:")) {
            self.path = if self.path.is_empty() {
                format!("{id}:")
            } else {
                format!("{id}: {}", self.path)
            };
        }
        self
    }
}

impl std::fmt::Display for RuleSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {}: {}", self.code, self.path, self.message)
    }
}

impl std::error::Error for RuleSetError {}

/// Declares an all-optional patch of a settings struct: each `Some` field replaces the base value.
macro_rules! patch {
    (
        $(#[$meta:meta])*
        $name:ident => $base:ty { $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $name {
            $( $(#[$fmeta])* pub $field: Option<$ty>, )*
        }

        impl $name {
            /// The field names this patch covers (the base struct's serde names, in order).
            pub const FIELDS: &'static [&'static str] = &[$(stringify!($field)),*];

            /// Replace every field of `base` this patch sets.
            pub fn apply_to(&self, base: &mut $base) {
                $( if let Some(value) = &self.$field { base.$field = value.clone(); } )*
            }

            /// True when the patch sets nothing.
            pub fn is_empty(&self) -> bool {
                true $( && self.$field.is_none() )*
            }
        }
    };
}

patch! {
    /// [`FlowSettings`] fields a rule set may set (the phases are [`SessionPatch::phases`]).
    FlowPatch => FlowSettings {
        /// [`FlowSettings::max_teams`].
        max_teams: u8,
        /// [`FlowSettings::seats_per_team`].
        seats_per_team: u8,
        /// [`FlowSettings::seat_slot_limit`].
        seat_slot_limit: Option<u8>,
        /// [`FlowSettings::friendly_unpause_ticks`].
        friendly_unpause_ticks: u64,
        /// [`FlowSettings::start_delay_ticks`].
        start_delay_ticks: u64,
        /// [`FlowSettings::pause_name_max`].
        pause_name_max: u8,
        /// [`FlowSettings::max_join_asks`].
        max_join_asks: u8,
        /// [`FlowSettings::max_pauses_per_seat`].
        max_pauses_per_seat: Option<u8>,
        /// [`FlowSettings::max_pause_ticks_per_seat`].
        max_pause_ticks_per_seat: Option<u64>,
        /// [`FlowSettings::match_clock`].
        match_clock: MatchClock,
    }
}

patch! {
    /// [`PhaseSettings`] fields a rule set may set.
    PhasePatch => PhaseSettings {
        /// [`PhaseSettings::warmup_ticks`].
        warmup_ticks: Option<u64>,
        /// [`PhaseSettings::warmup_min_seats`].
        warmup_min_seats: Option<u8>,
        /// [`PhaseSettings::countdown_ticks`].
        countdown_ticks: u64,
        /// [`PhaseSettings::time_limit_ticks`].
        time_limit_ticks: Option<u64>,
        /// [`PhaseSettings::on_time_up`].
        #[serde(deserialize_with = "strict::on_time_up")]
        on_time_up: TimeUp,
        /// [`PhaseSettings::rounds`].
        #[serde(deserialize_with = "strict::rounds")]
        rounds: Option<RoundSettings>,
        /// [`PhaseSettings::post_match_ticks`].
        post_match_ticks: Option<u64>,
        /// [`PhaseSettings::forfeit_after_ticks`].
        forfeit_after_ticks: Option<u64>,
    }
}

patch! {
    /// [`ObjectiveSettings`] fields a rule set may set.
    ObjectivePatch => ObjectiveSettings {
        /// [`ObjectiveSettings::max_teams`].
        max_teams: u8,
        /// [`ObjectiveSettings::max_objectives`].
        max_objectives: u16,
        /// [`ObjectiveSettings::flag_touch_radius`].
        flag_touch_radius: f32,
        /// [`ObjectiveSettings::touch_height`].
        touch_height: Option<f32>,
        /// [`ObjectiveSettings::flag_return_ticks`].
        flag_return_ticks: Option<u64>,
        /// [`ObjectiveSettings::capture_needs_home_flag`].
        capture_needs_home_flag: bool,
        /// [`ObjectiveSettings::capture_limit`].
        capture_limit: u32,
        /// [`ObjectiveSettings::hill_score_limit`].
        hill_score_limit: u32,
        /// [`ObjectiveSettings::hill_accrual_ticks`].
        hill_accrual_ticks: u32,
        /// [`ObjectiveSettings::max_pending_events`].
        max_pending_events: u32,
    }
}

patch! {
    /// [`TeamRules`] fields a rule set may set (the team list is [`RuleSet::teams`]).
    TeamsPatch => TeamRules {
        /// [`TeamRules::switch`].
        #[serde(deserialize_with = "strict::switch")]
        switch: TeamSwitch,
        /// [`TeamRules::rebalance_between_rounds`].
        rebalance_between_rounds: bool,
        /// [`TeamRules::assist_window_ticks`].
        assist_window_ticks: u64,
    }
}

/// How spawn points are chosen, as a rule set writes it (an externally tagged [`SpawnPolicy`]:
/// `First`, `RoundRobin`, `Random` or `Scored(enemy: 10, enemy_cap: 40.0, ...)`). The weights are
/// read field by field, straight to `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SpawnPolicyDef {
    /// [`SpawnPolicy::First`].
    First,
    /// [`SpawnPolicy::RoundRobin`].
    RoundRobin,
    /// [`SpawnPolicy::Random`].
    Random,
    /// [`SpawnPolicy::Scored`]; absent weights keep [`SpawnWeights::default`].
    Scored(#[serde(deserialize_with = "strict::spawn_weights")] SpawnWeights),
}

impl SpawnPolicyDef {
    /// The session's policy.
    pub fn policy(self) -> SpawnPolicy {
        match self {
            Self::First => SpawnPolicy::First,
            Self::RoundRobin => SpawnPolicy::RoundRobin,
            Self::Random => SpawnPolicy::Random,
            Self::Scored(weights) => SpawnPolicy::Scored(weights),
        }
    }
}

/// [`RespawnRules`] fields a rule set may set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RespawnPatch {
    /// `RespawnRules::default` (the policy of a team without its own row).
    #[serde(deserialize_with = "strict::respawn_default")]
    pub default: Option<RespawnPolicy>,
    /// [`RespawnRules::teams`] (the whole list).
    #[serde(deserialize_with = "strict::respawn_teams")]
    pub teams: Option<Vec<TeamRespawn>>,
    /// [`RespawnRules::spawn`].
    pub spawn: Option<SpawnPolicyDef>,
    /// [`RespawnRules::seed`].
    pub seed: Option<u64>,
}

impl RespawnPatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] = &["default", "teams", "spawn", "seed"];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut RespawnRules) {
        if let Some(value) = self.default {
            base.default = value;
        }
        if let Some(value) = &self.teams {
            base.teams = value.clone();
        }
        if let Some(value) = self.spawn {
            base.spawn = value.policy();
        }
        if let Some(value) = self.seed {
            base.seed = value;
        }
    }
}

/// The capture-zone rules a rule set starts the board with (`ZoneRules`: the mode and the zone
/// score limit). Hardpoint and attack/defend name zone ids, which are map data: the host applies them
/// with `ObjectiveBoard::set_zone_rules` after it installs the zones.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZonesPatch {
    /// [`ZoneRules::mode`].
    #[serde(deserialize_with = "strict::zone_mode")]
    pub mode: Option<ZoneMode>,
    /// [`ZoneRules::score_limit`].
    pub score_limit: Option<Option<u32>>,
}

/// When a wave starts, as a rule set writes it ([`WaveStart`], the same `kind` words; unknown fields
/// refused): `(kind: "after_cleared", delay_ticks: 30)` (the delay may be left out) or
/// `(kind: "at_tick", tick: 600)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaveStartDef {
    /// [`WaveStart::AfterCleared`].
    AfterCleared {
        /// Ticks to wait after the previous wave is cleared.
        #[serde(default)]
        delay_ticks: u64,
    },
    /// [`WaveStart::AtTick`].
    AtTick {
        /// The match tick.
        tick: u64,
    },
}

impl Default for WaveStartDef {
    fn default() -> Self {
        Self::AfterCleared { delay_ticks: 0 }
    }
}

impl WaveStartDef {
    /// The session's start rule.
    pub fn start(self) -> WaveStart {
        match self {
            Self::AfterCleared { delay_ticks } => WaveStart::AfterCleared { delay_ticks },
            Self::AtTick { tick } => WaveStart::AtTick { tick },
        }
    }
}

/// One group of a wave ([`WaveGroup`]). `archetype` and `count` are required; `spawn_tag` may be
/// left out (empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaveGroupDef {
    /// [`WaveGroup::archetype`].
    pub archetype: String,
    /// [`WaveGroup::count`] (at least 1).
    pub count: u32,
    /// [`WaveGroup::spawn_tag`].
    #[serde(default)]
    pub spawn_tag: String,
}

/// One row of the wave table ([`WaveRow`]). `groups` is required; `at` defaults to
/// `after_cleared` with no delay and `alive_cap` to 0 (no cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaveRowDef {
    /// [`WaveRow::at`].
    #[serde(default)]
    pub at: WaveStartDef,
    /// [`WaveRow::groups`].
    pub groups: Vec<WaveGroupDef>,
    /// [`WaveRow::alive_cap`].
    #[serde(default)]
    pub alive_cap: u32,
}

impl WaveRowDef {
    /// The session's wave row.
    pub fn row(&self) -> WaveRow {
        WaveRow {
            at: self.at.start(),
            groups: self
                .groups
                .iter()
                .map(|g| WaveGroup {
                    archetype: g.archetype.clone(),
                    count: g.count,
                    spawn_tag: g.spawn_tag.clone(),
                })
                .collect(),
            alive_cap: self.alive_cap,
        }
    }
}

/// One difficulty row ([`DifficultyRow`]); every field is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DifficultyRowDef {
    /// [`DifficultyRow::players`].
    pub players: u32,
    /// [`DifficultyRow::count_percent`].
    pub count_percent: u32,
    /// [`DifficultyRow::cap_percent`].
    pub cap_percent: u32,
}

/// [`WaveSettings`] fields a rule set may set (co-op waves). A wave table makes the mode co-op.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WavesPatch {
    /// [`WaveSettings::waves`] (the whole table).
    pub waves: Option<Vec<WaveRowDef>>,
    /// [`WaveSettings::difficulty`] (the whole list).
    pub difficulty: Option<Vec<DifficultyRowDef>>,
    /// [`WaveSettings::players_team`].
    pub players_team: Option<u8>,
    /// [`WaveSettings::shared_lives`].
    pub shared_lives: Option<Option<u32>>,
    /// [`WaveSettings::fail_on_team_wipe`].
    pub fail_on_team_wipe: Option<bool>,
    /// [`WaveSettings::success`] (`last_wave_cleared`, `objective_reached` or `either`).
    pub success: Option<CoopSuccess>,
}

impl WavesPatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] = &[
        "waves",
        "difficulty",
        "players_team",
        "shared_lives",
        "fail_on_team_wipe",
        "success",
    ];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut WaveSettings) {
        if let Some(rows) = &self.waves {
            base.waves = rows.iter().map(WaveRowDef::row).collect();
        }
        if let Some(rows) = &self.difficulty {
            base.difficulty = rows
                .iter()
                .map(|r| DifficultyRow {
                    players: r.players,
                    count_percent: r.count_percent,
                    cap_percent: r.cap_percent,
                })
                .collect();
        }
        if let Some(team) = self.players_team {
            base.players_team = team;
        }
        if let Some(lives) = self.shared_lives {
            base.shared_lives = lives;
        }
        if let Some(fail) = self.fail_on_team_wipe {
            base.fail_on_team_wipe = fail;
        }
        if let Some(success) = self.success {
            base.success = success;
        }
    }
}

/// An axis-aligned gate volume ([`HillVolume`]): `min` and `max` corners, `(x, y, z)`, read
/// straight to `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeDef {
    /// [`HillVolume::min`].
    pub min: [f32; 3],
    /// [`HillVolume::max`].
    pub max: [f32; 3],
}

/// One checkpoint gate ([`Checkpoint`]); every field is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDef {
    /// [`Checkpoint::id`].
    pub id: u32,
    /// [`Checkpoint::volume`].
    pub volume: VolumeDef,
    /// [`Checkpoint::next`].
    pub next: Vec<u32>,
}

/// [`RaceSettings`] fields a rule set may set. A course makes the mode a race.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RacePatch {
    /// [`RaceSettings::checkpoints`] (the whole course; the first is the start and finish).
    pub checkpoints: Option<Vec<CheckpointDef>>,
    /// [`RaceSettings::laps`].
    pub laps: Option<u32>,
    /// [`RaceSettings::finish_window_ticks`].
    pub finish_window_ticks: Option<u64>,
    /// [`RaceSettings::wrong_way_ticks`].
    pub wrong_way_ticks: Option<u64>,
}

impl RacePatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] =
        &["checkpoints", "laps", "finish_window_ticks", "wrong_way_ticks"];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut RaceSettings) {
        if let Some(course) = &self.checkpoints {
            base.checkpoints = course
                .iter()
                .map(|c| Checkpoint {
                    id: c.id,
                    volume: HillVolume {
                        min: c.volume.min,
                        max: c.volume.max,
                    },
                    next: c.next.clone(),
                })
                .collect();
        }
        if let Some(laps) = self.laps {
            base.laps = laps;
        }
        if let Some(ticks) = self.finish_window_ticks {
            base.finish_window_ticks = ticks;
        }
        if let Some(ticks) = self.wrong_way_ticks {
            base.wrong_way_ticks = ticks;
        }
    }
}

/// One phase of a shrinking zone ([`ZonePhase`]); every field is required.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZonePhaseDef {
    /// [`ZonePhase::wait_ticks`].
    pub wait_ticks: u64,
    /// [`ZonePhase::shrink_ticks`].
    pub shrink_ticks: u64,
    /// [`ZonePhase::end_radius`].
    pub end_radius: f32,
    /// [`ZonePhase::damage_per_tick`].
    pub damage_per_tick: u32,
}

/// [`ZoneSchedule`] fields a rule set may set (battle royale). Over a parent without a zone the
/// patch starts from [`ZoneSchedule::default`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZonePatch {
    /// [`ZoneSchedule::center`] (`(x, z)`).
    pub center: Option<[f32; 2]>,
    /// [`ZoneSchedule::radius`].
    pub radius: Option<f32>,
    /// [`ZoneSchedule::phases`] (the whole list).
    pub phases: Option<Vec<ZonePhaseDef>>,
    /// [`ZoneSchedule::seed`].
    pub seed: Option<u64>,
}

impl ZonePatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] = &["center", "radius", "phases", "seed"];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut ZoneSchedule) {
        if let Some(center) = self.center {
            base.center = center;
        }
        if let Some(radius) = self.radius {
            base.radius = radius;
        }
        if let Some(phases) = &self.phases {
            base.phases = phases
                .iter()
                .map(|p| ZonePhase {
                    wait_ticks: p.wait_ticks,
                    shrink_ticks: p.shrink_ticks,
                    end_radius: p.end_radius,
                    damage_per_tick: p.damage_per_tick,
                })
                .collect();
        }
        if let Some(seed) = self.seed {
            base.seed = seed;
        }
    }
}

/// Teams that win together ([`Alliance`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllianceDef {
    /// [`Alliance::teams`].
    pub teams: Vec<u8>,
}

/// [`EliminationRules`] fields a rule set may set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EliminationPatch {
    /// [`EliminationRules::last_standing`].
    pub last_standing: Option<bool>,
    /// [`EliminationRules::alliances`] (the whole list).
    pub alliances: Option<Vec<AllianceDef>>,
}

impl EliminationPatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] = &["last_standing", "alliances"];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut EliminationRules) {
        if let Some(on) = self.last_standing {
            base.last_standing = on;
        }
        if let Some(list) = &self.alliances {
            base.alliances = list
                .iter()
                .map(|a| Alliance {
                    teams: a.teams.clone(),
                })
                .collect();
        }
    }
}

/// One strategy victory condition ([`VictoryCondition`], the same `kind` words; unknown fields
/// refused): `(kind: "destroy_all_tagged", tag: "capital")`, `(kind: "hold_for", objective: 3,
/// ticks: 6000)`, `(kind: "score_threshold", score: 500)`, `(kind: "team_elimination")` or
/// `(kind: "surrender")`.
///
/// Reading goes through a private strict mirror whose payload-free variants are empty struct
/// variants, so `(kind: "surrender", extra: 1)` is refused like an unknown field on the other
/// variants (ADR-8879; serde skips the rest of the map after an internally tagged unit variant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    from = "strict::VictoryConditionFile"
)]
pub enum VictoryConditionDef {
    /// [`VictoryCondition::DestroyAllTagged`].
    DestroyAllTagged {
        /// The tag the host counts.
        tag: String,
    },
    /// [`VictoryCondition::HoldFor`].
    HoldFor {
        /// The objective id.
        objective: u32,
        /// Match ticks to hold (at least 1).
        ticks: u64,
    },
    /// [`VictoryCondition::ScoreThreshold`].
    ScoreThreshold {
        /// The score to reach.
        score: i64,
    },
    /// [`VictoryCondition::TeamElimination`].
    TeamElimination,
    /// [`VictoryCondition::Surrender`].
    Surrender,
}

impl VictoryConditionDef {
    /// The session's condition.
    pub fn condition(&self) -> VictoryCondition {
        match self {
            Self::DestroyAllTagged { tag } => VictoryCondition::DestroyAllTagged { tag: tag.clone() },
            Self::HoldFor { objective, ticks } => VictoryCondition::HoldFor {
                objective: *objective,
                ticks: *ticks,
            },
            Self::ScoreThreshold { score } => VictoryCondition::ScoreThreshold { score: *score },
            Self::TeamElimination => VictoryCondition::TeamElimination,
            Self::Surrender => VictoryCondition::Surrender,
        }
    }
}

patch! {
    /// [`SurrenderRules`] fields a rule set may set.
    SurrenderPatch => SurrenderRules {
        /// [`SurrenderRules::threshold_percent`] (`1..=100`).
        threshold_percent: u8,
        /// [`SurrenderRules::window_ticks`] (at least 1).
        window_ticks: u64,
        /// [`SurrenderRules::cooldown_ticks`].
        cooldown_ticks: u64,
    }
}

/// [`VictoryRules`] fields a rule set may set (strategy modes).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VictoryPatch {
    /// [`VictoryRules::conditions`] (the whole list, at most [`MAX_VICTORY_CONDITIONS`]).
    pub conditions: Option<Vec<VictoryConditionDef>>,
    /// [`VictoryRules::surrender`], field by field.
    pub surrender: SurrenderPatch,
}

impl VictoryPatch {
    /// The field names this patch covers.
    pub const FIELDS: &'static [&'static str] = &["conditions", "surrender"];

    /// Replace every field of `base` this patch sets.
    pub fn apply_to(&self, base: &mut VictoryRules) {
        if let Some(list) = &self.conditions {
            base.conditions = list.iter().map(VictoryConditionDef::condition).collect();
        }
        self.surrender.apply_to(&mut base.surrender);
    }
}

/// Every session setting a rule set may override, section by section. An absent section or field
/// keeps the parent's value (the Rust default at the root).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionPatch {
    /// Pause, ready-up and seat settings.
    pub flow: FlowPatch,
    /// Warm-up, time limit, rounds and the post-match phase.
    pub phases: PhasePatch,
    /// Objective, score-limit and team-cap settings.
    pub objectives: ObjectivePatch,
    /// Team switch policy, rebalance and the assist window.
    pub teams: TeamsPatch,
    /// Respawn policies and the spawn-point policy.
    pub respawn: RespawnPatch,
    /// Capture-zone mode and zone score limit.
    pub zones: ZonesPatch,
    /// Co-op waves (ADR-8748).
    pub waves: WavesPatch,
    /// The race course (ADR-8748).
    pub race: RacePatch,
    /// The shrinking zone (ADR-8748). Absent keeps the parent's; `Some(None)` turns it off; a
    /// patch turns it on, starting from the parent's schedule or [`ZoneSchedule::default`].
    pub zone: Option<Option<ZonePatch>>,
    /// Team elimination and alliances (ADR-8748).
    pub elimination: EliminationPatch,
    /// Strategy victory conditions and the surrender vote (ADR-8748).
    pub victory: VictoryPatch,
}

/// One win condition: a stock rule's lobby word and its parameters (S-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WinConditionDef {
    /// `lives` or `structures` (the aliases `clones` and `production` parse too).
    pub rule: String,
    /// Starting lives (`1..=10`); absent keeps 3.
    #[serde(default)]
    pub lives: Option<u32>,
    /// Structure stems the structures rule counts.
    #[serde(default)]
    pub stems: Vec<String>,
}

/// The value kind of an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    /// A whole number (`i64`).
    Int,
    /// An `f32`, parsed straight from the text.
    Float,
    /// `true` or `false`.
    Bool,
    /// One of `choices`.
    Enum,
    /// Any text up to [`MAX_OPTION_TEXT_BYTES`].
    String,
}

impl OptionKind {
    /// The lowercase word (`int`, `float`, `bool`, `enum`, `string`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Enum => "enum",
            Self::String => "string",
        }
    }
}

/// One option a playlist row may set. Values are written as text (`default: "3"`), exactly as a
/// playlist row's `options` map holds them, and parsed once by [`OptionKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionDef {
    /// The row option key (`[a-z0-9_]`, 1 to 64 bytes).
    pub key: String,
    /// Its kind.
    pub kind: OptionKind,
    /// The value when a row does not set it.
    pub default: String,
    /// Lowest value (`int` and `float`).
    #[serde(default)]
    pub min: Option<String>,
    /// Highest value (`int` and `float`).
    #[serde(default)]
    pub max: Option<String>,
    /// The allowed values (`enum`).
    #[serde(default)]
    pub choices: Vec<String>,
    /// The session field this option sets (ADR-8881), one of [`OPTION_BINDS`]
    /// (`objectives.capture_limit`, `phases.time_limit_ticks`, ...), of the option's kind. The
    /// value (the row's, else the default) replaces the rule set's own value before the settings
    /// are checked; on an optional field (`warmup_ticks`, `time_limit_ticks`, `flag_return_ticks`,
    /// `zones.score_limit`, ...) 0 turns it off. Absent: the game reads the value
    /// ([`ResolvedRules::option`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
}

/// Every session field an option may bind to ([`OptionDef::bind`]): the path and the option kind
/// it takes. Int fields refuse a negative value or one too large for the field
/// (`ruleset_option_range`).
pub const OPTION_BINDS: [(&str, OptionKind); 19] = [
    ("flow.start_delay_ticks", OptionKind::Int),
    ("phases.warmup_ticks", OptionKind::Int),
    ("phases.warmup_min_seats", OptionKind::Int),
    ("phases.countdown_ticks", OptionKind::Int),
    ("phases.time_limit_ticks", OptionKind::Int),
    ("phases.post_match_ticks", OptionKind::Int),
    ("phases.forfeit_after_ticks", OptionKind::Int),
    ("objectives.flag_touch_radius", OptionKind::Float),
    ("objectives.flag_return_ticks", OptionKind::Int),
    ("objectives.capture_needs_home_flag", OptionKind::Bool),
    ("objectives.capture_limit", OptionKind::Int),
    ("objectives.hill_score_limit", OptionKind::Int),
    ("objectives.hill_accrual_ticks", OptionKind::Int),
    ("teams.rebalance_between_rounds", OptionKind::Bool),
    ("teams.assist_window_ticks", OptionKind::Int),
    ("respawn.seed", OptionKind::Int),
    ("zones.score_limit", OptionKind::Int),
    ("race.laps", OptionKind::Int),
    ("victory.surrender.window_ticks", OptionKind::Int),
];

/// A resolved option value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionValue {
    /// An `int` option.
    Int(i64),
    /// A `float` option.
    Float(f32),
    /// A `bool` option.
    Bool(bool),
    /// An `enum` option (one of its choices).
    Enum(String),
    /// A `string` option.
    String(String),
}

/// One `.rules.ron` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    /// [`RULES_RON_FORMAT_VERSION`].
    pub format_version: u32,
    /// The id a playlist row's `mode` names (`[a-z0-9_-]`, 1 to 64 bytes).
    pub id: String,
    /// Localization key of the mode's name.
    #[serde(default)]
    pub name_key: String,
    /// The [`MatchMode`] lobby id the mode runs on (`tdm`, `ffa`, `ctf`, `koth`, `dom`, `custom`).
    /// Required unless a parent sets it.
    #[serde(default)]
    pub skeleton: Option<String>,
    /// The parent rule set's id.
    #[serde(default)]
    pub extends: Option<String>,
    /// The fighting teams. Absent keeps the parent's (S-14's four unnamed teams at the root).
    #[serde(default, deserialize_with = "strict::teams")]
    pub teams: Option<Vec<TeamDef>>,
    /// Session setting overrides.
    #[serde(default)]
    pub session: SessionPatch,
    /// Win conditions. Absent keeps the parent's; an empty list is the default lives rule.
    #[serde(default)]
    pub win_conditions: Option<Vec<WinConditionDef>>,
    /// Option schema. A key a parent declared is replaced; new keys are added.
    #[serde(default)]
    pub options: Vec<OptionDef>,
}

impl RuleSet {
    /// A v1 rule set with this id and skeleton and nothing else set.
    pub fn new(id: impl Into<String>, skeleton: MatchMode) -> Self {
        Self {
            format_version: RULES_RON_FORMAT_VERSION,
            id: id.into(),
            name_key: String::new(),
            skeleton: Some(skeleton.as_lobby_str().to_owned()),
            extends: None,
            teams: None,
            session: SessionPatch::default(),
            win_conditions: None,
            options: Vec::new(),
        }
    }

    /// Checks what one file can check alone: the version, the id, the skeleton word, the option
    /// schema and the win conditions. The chain and the merged settings are checked by
    /// [`RuleSetLibrary::resolve`].
    ///
    /// # Errors
    ///
    /// The first problem, with its field path.
    pub fn check(&self) -> Result<(), RuleSetError> {
        self.check_inner().map_err(|e| e.at(&self.id))
    }

    fn check_inner(&self) -> Result<(), RuleSetError> {
        if self.format_version != RULES_RON_FORMAT_VERSION {
            return Err(RuleSetError::new(
                codes::VERSION,
                "format_version",
                format!("no migrator from rules.ron v{}", self.format_version),
            ));
        }
        check_id(&self.id, "id")?;
        if let Some(parent) = &self.extends {
            check_id(parent, "extends")?;
        }
        if let Some(word) = &self.skeleton {
            skeleton_of(word)?;
        } else if self.extends.is_none() {
            return Err(RuleSetError::new(
                codes::INVALID,
                "skeleton",
                "a rule set without `extends` names its skeleton",
            ));
        }
        if let Some(teams) = &self.teams {
            check_teams(teams)?;
        }
        if let Some(wins) = &self.win_conditions {
            win_states(wins)?;
        }
        if self.options.len() > MAX_OPTIONS {
            return Err(RuleSetError::new(
                codes::INVALID,
                "options",
                format!("at most {MAX_OPTIONS} options"),
            ));
        }
        let mut keys: Vec<&str> = Vec::new();
        for (index, def) in self.options.iter().enumerate() {
            check_option_def(def, &format!("options[{index}]"))?;
            if keys.contains(&def.key.as_str()) {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    format!("options[{index}].key"),
                    format!("option `{}` is declared twice", def.key),
                ));
            }
            keys.push(&def.key);
        }
        check_binds(
            self.options
                .iter()
                .enumerate()
                .map(|(i, def)| (format!("options[{i}]"), def)),
        )?;
        Ok(())
    }
}

/// The settings a rule set resolves to, ready for a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRules {
    /// The rule set's id (the bare skeleton id for a skeleton fallback).
    pub id: String,
    /// The rule set's name key.
    pub name_key: String,
    /// The ids from the root ancestor down to this rule set.
    pub chain: Vec<String>,
    /// The skeleton mode.
    pub mode: MatchMode,
    /// The session settings.
    pub settings: SessionSettings,
    /// The board's starting zone rules (mode and score limit).
    pub zones: ZoneRules,
    /// The win conditions, in file order. Empty is the default lives rule.
    pub win_conditions: Vec<WinRuleState>,
    /// The option schema, by key.
    pub schema: BTreeMap<String, OptionDef>,
    /// Every option's value: the row's where it set one, else the default.
    pub options: BTreeMap<String, OptionValue>,
}

/// What [`ResolvedRules::content_hash`] covers: the rules, never the names.
#[derive(Serialize)]
struct HashedRules<'a> {
    format: &'static str,
    mode: MatchMode,
    settings: &'a SessionSettings,
    zones: &'a ZoneRules,
    win_conditions: &'a [WinRuleState],
    options: &'a BTreeMap<String, OptionValue>,
}

impl ResolvedRules {
    /// The Rust defaults of `mode`: what a bare skeleton id (a row whose mode names no rule set)
    /// runs, and what the shipped default rule sets reproduce bit for bit.
    pub fn skeleton(mode: MatchMode) -> Self {
        Self {
            id: mode.as_lobby_str().to_owned(),
            name_key: String::new(),
            chain: Vec::new(),
            mode,
            settings: SessionSettings::default(),
            zones: ZoneRules::default(),
            win_conditions: Vec::new(),
            schema: BTreeMap::new(),
            options: BTreeMap::new(),
        }
    }

    /// The canonical bytes [`ResolvedRules::content_hash`] hashes: the mode, the settings (every
    /// `f32` as its bits), the zone rules, the win conditions and the option values. The id, the
    /// name key and the chain are not part of them, so two ids with the same rules hash the same.
    ///
    /// # Errors
    ///
    /// Never for these types; the result keeps the serializer's signature.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        crate::canon::to_bytes(&HashedRules {
            format: "regia rules v1",
            mode: self.mode,
            settings: &self.settings,
            zones: &self.zones,
            win_conditions: &self.win_conditions,
            options: &self.options,
        })
    }

    /// FNV-1a 64 of [`ResolvedRules::canonical_bytes`]: the rules entry of the content handshake.
    /// `0` only when the bytes cannot be made (never for these types).
    pub fn content_hash(&self) -> u64 {
        self.canonical_bytes()
            .map(|bytes| crate::canon::fnv1a64(&bytes))
            .unwrap_or(0)
    }

    /// A session running these rules: the settings, the first win condition and the zone rules.
    /// The host still places the map's objectives; hardpoint and attack/defend zone rules name zone
    /// ids, so the host sets them again after installing the zones.
    pub fn session(&self) -> MatchSession {
        let mut session = MatchSession::new(self.settings.clone());
        self.apply_to(&mut session);
        session
    }

    /// Set what the session takes from the rules besides its settings: the board mode, the win
    /// rule and the zone rules.
    pub fn apply_to(&self, session: &mut MatchSession) {
        session.board.mode = self.mode;
        if let Some(rule) = self.win_conditions.first() {
            session.win_rule = rule.clone();
        }
        session.board.zone_rules = self.zones.clone();
    }

    /// The value of option `key`.
    pub fn option(&self, key: &str) -> Option<&OptionValue> {
        self.options.get(key)
    }
}

/// The content hash of several resolved rule sets (every rule set a playlist names), in id order.
pub fn combined_hash(rules: &[&ResolvedRules]) -> u64 {
    let mut entries: Vec<(&str, u64)> = rules
        .iter()
        .map(|rule| (rule.id.as_str(), rule.content_hash()))
        .collect();
    entries.sort_unstable();
    let mut bytes = Vec::new();
    for (id, hash) in entries {
        bytes.extend_from_slice(&u64::try_from(id.len()).unwrap_or(u64::MAX).to_le_bytes());
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&hash.to_le_bytes());
    }
    crate::canon::fnv1a64(&bytes)
}

/// What [`RuleSetLibrary::playlist_rules`] found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlaylistRules {
    /// The combined content hash of the distinct rule sets the rows resolve to; `None` when no
    /// row names a mode.
    pub hash: Option<u64>,
    /// Rows that do not resolve: `(playlist index, error)`, in row order.
    pub refused: Vec<(u32, RuleSetError)>,
}

/// Rule sets by id. Resolution walks `extends` through the library.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleSetLibrary {
    sets: BTreeMap<String, RuleSet>,
}

impl RuleSetLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `set` after [`RuleSet::check`]. An id already present is refused (`ruleset_duplicate`);
    /// use [`RuleSetLibrary::replace`] to override one.
    ///
    /// # Errors
    ///
    /// The check's error, or `ruleset_duplicate`.
    pub fn insert(&mut self, set: RuleSet) -> Result<(), RuleSetError> {
        set.check()?;
        if self.sets.contains_key(&set.id) {
            return Err(RuleSetError::new(
                codes::DUPLICATE,
                format!("{}: id", set.id),
                format!("rule set `{}` is already loaded", set.id),
            ));
        }
        self.sets.insert(set.id.clone(), set);
        Ok(())
    }

    /// Add or replace `set` after [`RuleSet::check`] (a project file overriding a shipped default).
    ///
    /// # Errors
    ///
    /// The check's error.
    pub fn replace(&mut self, set: RuleSet) -> Result<Option<RuleSet>, RuleSetError> {
        set.check()?;
        Ok(self.sets.insert(set.id.clone(), set))
    }

    /// The rule set `id`.
    pub fn get(&self, id: &str) -> Option<&RuleSet> {
        self.sets.get(id)
    }

    /// Every rule set, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &RuleSet> {
        self.sets.values()
    }

    /// How many rule sets the library holds.
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    /// True when the library holds none.
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// The ids from the root ancestor down to `id`.
    ///
    /// # Errors
    ///
    /// `ruleset_unknown` (no such id, or a missing parent), `ruleset_cycle`, `ruleset_depth`.
    pub fn chain(&self, id: &str) -> Result<Vec<&RuleSet>, RuleSetError> {
        let mut chain: Vec<&RuleSet> = Vec::new();
        let mut at = self.sets.get(id).ok_or_else(|| {
            RuleSetError::new(codes::UNKNOWN, "mode", format!("no rule set or skeleton `{id}`"))
        })?;
        loop {
            chain.push(at);
            let Some(parent) = &at.extends else { break };
            if chain.iter().any(|set| &set.id == parent) {
                let mut ids: Vec<&str> = chain.iter().map(|set| set.id.as_str()).collect();
                ids.push(parent);
                return Err(RuleSetError::new(
                    codes::CYCLE,
                    format!("{}: extends", at.id),
                    format!("`extends` comes back: {}", ids.join(" -> ")),
                ));
            }
            if chain.len() > MAX_EXTENDS_DEPTH {
                return Err(RuleSetError::new(
                    codes::DEPTH,
                    format!("{id}: extends"),
                    format!("more than {MAX_EXTENDS_DEPTH} ancestors"),
                ));
            }
            at = self.sets.get(parent).ok_or_else(|| {
                RuleSetError::new(
                    codes::UNKNOWN,
                    format!("{}: extends", at.id),
                    format!("parent rule set `{parent}` is not loaded"),
                )
            })?;
        }
        chain.reverse();
        Ok(chain)
    }

    /// Resolve rule set `id` with a playlist row's options. A bare skeleton id that names no rule
    /// set resolves to [`ResolvedRules::skeleton`] and its options pass through unchecked (S-14).
    ///
    /// # Errors
    ///
    /// Any [`codes`] error, with the field path.
    pub fn resolve(
        &self,
        id: &str,
        row_options: &BTreeMap<String, String>,
    ) -> Result<ResolvedRules, RuleSetError> {
        if !self.sets.contains_key(id)
            && let Some(mode) = MatchMode::from_lobby_str(id)
        {
            return Ok(ResolvedRules::skeleton(mode));
        }
        let chain = self.chain(id)?;
        let mut settings = SessionSettings::default();
        let mut zones = ZoneRules::default();
        let mut teams = default_team_defs();
        let mut wins: Vec<WinConditionDef> = Vec::new();
        let mut schema: BTreeMap<String, OptionDef> = BTreeMap::new();
        let mut mode = MatchMode::Custom;
        let mut name_key = String::new();
        for set in &chain {
            if let Some(word) = &set.skeleton {
                mode = skeleton_of(word).map_err(|e| e.at(&set.id))?;
            }
            if !set.name_key.is_empty() {
                name_key.clone_from(&set.name_key);
            }
            if let Some(defs) = &set.teams {
                teams.clone_from(defs);
            }
            let patch = &set.session;
            patch.flow.apply_to(&mut settings.flow);
            patch.phases.apply_to(&mut settings.flow.phases);
            patch.objectives.apply_to(&mut settings.objectives);
            patch.teams.apply_to(&mut settings.teams);
            patch.respawn.apply_to(&mut settings.respawn);
            if let Some(zone_mode) = &patch.zones.mode {
                zones.mode = zone_mode.clone();
            }
            if let Some(limit) = patch.zones.score_limit {
                zones.score_limit = limit;
            }
            patch.waves.apply_to(&mut settings.waves);
            patch.race.apply_to(&mut settings.race);
            match &patch.zone {
                None => {}
                Some(None) => settings.zone = None,
                Some(Some(zone)) => zone.apply_to(settings.zone.get_or_insert_with(ZoneSchedule::default)),
            }
            patch.elimination.apply_to(&mut settings.elimination);
            patch.victory.apply_to(&mut settings.victory);
            if let Some(list) = &set.win_conditions {
                wins.clone_from(list);
            }
            for def in &set.options {
                schema.insert(def.key.clone(), def.clone());
            }
        }
        settings.teams.defs = teams;
        let leaf = chain.last().map_or(id, |set| set.id.as_str());
        // Bound options write their values first, so the settings check covers them (ADR-8881).
        let options = resolve_options(&schema, row_options)?;
        bind_options(&schema, &options, &mut settings, &mut zones)?;
        check_settings(&settings, &zones).map_err(|e| e.at(leaf))?;
        if schema.len() > MAX_OPTIONS {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("{leaf}: options"),
                format!("at most {MAX_OPTIONS} options after `extends`"),
            ));
        }
        let win_conditions = win_states(&wins).map_err(|e| e.at(leaf))?;
        Ok(ResolvedRules {
            id: leaf.to_owned(),
            name_key,
            chain: chain.iter().map(|set| set.id.clone()).collect(),
            mode,
            settings,
            zones,
            win_conditions,
            schema,
            options,
        })
    }

    /// Resolve the rule set a playlist row names in its `mode`.
    ///
    /// # Errors
    ///
    /// As [`RuleSetLibrary::resolve`].
    pub fn resolve_row(&self, row: &PlaylistEntry) -> Result<ResolvedRules, RuleSetError> {
        self.resolve(&row.mode, &row.options)
    }

    /// How a dedicated host resolves a playlist row (ADR-8881). `None` when the row's `mode`
    /// names no rule set in this library (empty: the level's own rules; a game's registered rules
    /// name; a bare skeleton id with no rule set): the row runs as before and adds nothing to the
    /// rules hash. Otherwise the rule set resolved with the row's options. A row key the rule set
    /// does not declare is left to the game, as S-14 rows carry options a game reads itself: it is
    /// not checked and does not enter the hash. A declared option with a bad value, and a rule set
    /// that does not resolve, are the error.
    pub fn resolve_hosted_row(&self, row: &PlaylistEntry) -> Option<Result<ResolvedRules, RuleSetError>> {
        if !self.sets.contains_key(&row.mode) {
            return None;
        }
        let mut options = row.options.clone();
        // Each pass drops one undeclared key, so this ends after at most one pass per key.
        for _ in 0..=row.options.len() {
            match self.resolve(&row.mode, &options) {
                Err(error) if error.code == codes::OPTION_UNKNOWN => {
                    let key = error.path.strip_prefix("options.").unwrap_or_default().to_owned();
                    if options.remove(&key).is_none() {
                        return Some(Err(error));
                    }
                }
                other => return Some(other),
            }
        }
        Some(self.resolve(&row.mode, &options))
    }

    /// The rules a dedicated host's playlist runs (ADR-8881): the [`combined_hash`] of every
    /// distinct rule set its rows resolve to ([`RuleSetLibrary::resolve_hosted_row`]; `None` when
    /// no row names a rule set of this library), and every row that does not resolve, by playlist
    /// index, with its error. The host refuses those rows and puts the hash in its session
    /// manifest (`NetSession::set_rules_hash`), so a client whose rule sets differ is refused at
    /// the handshake. A client computes the same hash from the same playlist and rule sets.
    pub fn playlist_rules(&self, rows: &[PlaylistEntry]) -> PlaylistRules {
        let mut resolved: Vec<ResolvedRules> = Vec::new();
        let mut refused = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let Some(result) = self.resolve_hosted_row(row) else {
                continue;
            };
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            match result {
                Ok(rules) => {
                    let hash = rules.content_hash();
                    if !resolved
                        .iter()
                        .any(|have| have.id == rules.id && have.content_hash() == hash)
                    {
                        resolved.push(rules);
                    }
                }
                Err(error) => refused.push((index, error)),
            }
        }
        let refs: Vec<&ResolvedRules> = resolved.iter().collect();
        PlaylistRules {
            hash: (!refs.is_empty()).then(|| combined_hash(&refs)),
            refused,
        }
    }

    /// The skeleton mode of every rule-set id these rows name (for
    /// [`MatchSession::rule_modes`]); a bare skeleton id needs no entry.
    ///
    /// # Errors
    ///
    /// The first row that does not resolve, with `rows[<n>]` in its path.
    pub fn rule_modes(&self, rows: &[PlaylistEntry]) -> Result<BTreeMap<String, MatchMode>, RuleSetError> {
        let mut modes = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            if !self.sets.contains_key(&row.mode) {
                continue;
            }
            let resolved = self.resolve_row(row).map_err(|mut e| {
                e.path = format!("rows[{index}].{}", e.path);
                e
            })?;
            modes.insert(row.mode.clone(), resolved.mode);
        }
        Ok(modes)
    }
}

fn skeleton_of(word: &str) -> Result<MatchMode, RuleSetError> {
    MatchMode::from_lobby_str(word).ok_or_else(|| {
        RuleSetError::new(
            codes::INVALID,
            "skeleton",
            format!("`{word}` is not a skeleton (tdm, ffa, ctf, koth, dom, custom)"),
        )
    })
}

fn check_id(id: &str, path: &str) -> Result<(), RuleSetError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(RuleSetError::new(
            codes::INVALID,
            path,
            format!("`{id}` is not an id ([a-z0-9_-], 1 to {MAX_ID_BYTES} bytes)"),
        ))
    }
}

fn check_teams(teams: &[TeamDef]) -> Result<(), RuleSetError> {
    if teams.is_empty() || teams.len() > MAX_TEAM_DEFS {
        return Err(RuleSetError::new(
            codes::INVALID,
            "teams",
            format!("1 to {MAX_TEAM_DEFS} teams"),
        ));
    }
    for (index, team) in teams.iter().enumerate() {
        if team.id == 0 {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("teams[{index}].id"),
                "team 0 is neutral, not a fighting team",
            ));
        }
        if teams[..index].iter().any(|other| other.id == team.id) {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("teams[{index}].id"),
                format!("team {} is defined twice", team.id),
            ));
        }
    }
    Ok(())
}

fn finite(value: f32, path: &str) -> Result<(), RuleSetError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(RuleSetError::new(codes::INVALID, path, "not a finite number"))
    }
}

fn check_settings(settings: &SessionSettings, zones: &ZoneRules) -> Result<(), RuleSetError> {
    let objectives = &settings.objectives;
    finite(
        objectives.flag_touch_radius,
        "session.objectives.flag_touch_radius",
    )?;
    if objectives.flag_touch_radius < 0.0 {
        return Err(RuleSetError::new(
            codes::INVALID,
            "session.objectives.flag_touch_radius",
            "a negative radius",
        ));
    }
    if let Some(height) = objectives.touch_height {
        finite(height, "session.objectives.touch_height")?;
    }
    if let SpawnPolicy::Scored(weights) = settings.respawn.spawn {
        finite(weights.enemy_cap, "session.respawn.spawn.enemy_cap")?;
        finite(weights.los_range, "session.respawn.spawn.los_range")?;
        finite(weights.team_radius, "session.respawn.spawn.team_radius")?;
    }
    settings.flow.phases.validate().map_err(|error| {
        let field = match &error {
            crate::RulesError::PhaseSettings { field } => *field,
            _ => "",
        };
        RuleSetError::new(
            codes::INVALID,
            format!("session.phases.{field}"),
            error.to_string(),
        )
    })?;
    check_teams(&settings.teams.defs)?;
    // A repeated id would lock the zone the mode means to open (capture-zones review, finding 2), and
    // a list longer than the objective cap cannot be placed.
    let ids: Option<(&[u32], &str)> = match &zones.mode {
        ZoneMode::Hardpoint { rotation, .. } => Some((rotation, "session.zones.mode.rotation")),
        ZoneMode::AttackDefend { order, .. } => Some((order, "session.zones.mode.order")),
        _ => None,
    };
    if let Some((list, path)) = ids {
        if list.len() > usize::from(objectives.max_objectives) {
            return Err(RuleSetError::new(
                codes::INVALID,
                path,
                format!("more zones than max_objectives ({})", objectives.max_objectives),
            ));
        }
        if let Some(index) = (1..list.len()).find(|&i| list[..i].contains(&list[i])) {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("{path}[{index}]"),
                format!("zone {} is listed twice", list[index]),
            ));
        }
    }
    match &zones.mode {
        ZoneMode::Hardpoint {
            rotation,
            rotate_ticks,
        } => {
            if *rotate_ticks == 0 {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    "session.zones.mode.rotate_ticks",
                    "a hardpoint rotation of 0 ticks",
                ));
            }
            if rotation.is_empty() {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    "session.zones.mode.rotation",
                    "an empty hardpoint rotation",
                ));
            }
        }
        ZoneMode::AttackDefend {
            attackers,
            defenders,
            order,
        } => {
            let fighting = |team: u8| (1..=objectives.max_teams).contains(&team);
            if !fighting(*attackers) {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    "session.zones.mode.attackers",
                    "not a fighting team",
                ));
            }
            if !fighting(*defenders) || attackers == defenders {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    "session.zones.mode.defenders",
                    "not a fighting team other than the attackers",
                ));
            }
            if order.is_empty() {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    "session.zones.mode.order",
                    "an empty zone order",
                ));
            }
        }
        _ => {}
    }
    check_waves(&settings.waves, objectives.max_teams)?;
    check_race(&settings.race)?;
    if let Some(zone) = &settings.zone {
        check_zone(zone)?;
    }
    check_elimination(&settings.elimination, objectives.max_teams)?;
    check_victory(&settings.victory)?;
    Ok(())
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> RuleSetError {
    RuleSetError::new(codes::INVALID, path, message)
}

/// The core check after the indexed ones: anything they missed still names its section.
fn backstop(result: Result<(), crate::RulesError>, section: &str) -> Result<(), RuleSetError> {
    result.map_err(|error| invalid(format!("session.{section}"), error.to_string()))
}

fn check_waves(waves: &WaveSettings, max_teams: u8) -> Result<(), RuleSetError> {
    use crate::waves::{
        MAX_DIFFICULTY_ROWS, MAX_GROUPS, MAX_ID_BYTES as WAVE_ID_BYTES, MAX_PERCENT, MAX_WAVES,
    };
    if waves.waves.len() > MAX_WAVES {
        return Err(invalid(
            "session.waves.waves",
            format!("at most {MAX_WAVES} waves"),
        ));
    }
    if !waves.is_off() && !(1..=max_teams).contains(&waves.players_team) {
        return Err(invalid(
            "session.waves.players_team",
            format!("not a fighting team (1 to max_teams, {max_teams})"),
        ));
    }
    for (w, row) in waves.waves.iter().enumerate() {
        let at = format!("session.waves.waves[{w}]");
        if row.groups.is_empty() || row.groups.len() > MAX_GROUPS {
            return Err(invalid(
                format!("{at}.groups"),
                format!("a wave has 1 to {MAX_GROUPS} groups"),
            ));
        }
        for (g, group) in row.groups.iter().enumerate() {
            let at = format!("{at}.groups[{g}]");
            if group.count == 0 {
                return Err(invalid(format!("{at}.count"), "a group of 0 enemies"));
            }
            if group.archetype.is_empty() || group.archetype.len() > WAVE_ID_BYTES {
                return Err(invalid(
                    format!("{at}.archetype"),
                    format!("an archetype id is 1 to {WAVE_ID_BYTES} bytes"),
                ));
            }
            if group.spawn_tag.len() > WAVE_ID_BYTES {
                return Err(invalid(
                    format!("{at}.spawn_tag"),
                    format!("a spawn tag is at most {WAVE_ID_BYTES} bytes"),
                ));
            }
        }
    }
    if waves.difficulty.len() > MAX_DIFFICULTY_ROWS {
        return Err(invalid(
            "session.waves.difficulty",
            format!("at most {MAX_DIFFICULTY_ROWS} difficulty rows"),
        ));
    }
    for (index, row) in waves.difficulty.iter().enumerate() {
        let at = format!("session.waves.difficulty[{index}]");
        if index > 0 && row.players <= waves.difficulty[index - 1].players {
            return Err(invalid(
                format!("{at}.players"),
                "difficulty rows ascend by players",
            ));
        }
        for (name, value) in [
            ("count_percent", row.count_percent),
            ("cap_percent", row.cap_percent),
        ] {
            if !(1..=MAX_PERCENT).contains(&value) {
                return Err(invalid(
                    format!("{at}.{name}"),
                    format!("a percentage from 1 to {MAX_PERCENT}"),
                ));
            }
        }
    }
    backstop(waves.validate(), "waves")
}

fn check_race(race: &RaceSettings) -> Result<(), RuleSetError> {
    use crate::race::{MAX_CHECKPOINTS, MAX_LAPS, MAX_NEXT};
    if race.is_off() {
        return Ok(());
    }
    let course = &race.checkpoints;
    if course.len() < 2 || course.len() > MAX_CHECKPOINTS {
        return Err(invalid(
            "session.race.checkpoints",
            format!("a course has 2 to {MAX_CHECKPOINTS} checkpoints"),
        ));
    }
    if race.laps == 0 || race.laps > MAX_LAPS {
        return Err(invalid("session.race.laps", format!("1 to {MAX_LAPS} laps")));
    }
    for (index, checkpoint) in course.iter().enumerate() {
        let at = format!("session.race.checkpoints[{index}]");
        if course[..index].iter().any(|c| c.id == checkpoint.id) {
            return Err(invalid(
                format!("{at}.id"),
                format!("checkpoint {} is defined twice", checkpoint.id),
            ));
        }
        if checkpoint.next.is_empty() || checkpoint.next.len() > MAX_NEXT {
            return Err(invalid(
                format!("{at}.next"),
                format!("a checkpoint names 1 to {MAX_NEXT} next checkpoints"),
            ));
        }
        for (n, next) in checkpoint.next.iter().enumerate() {
            if *next == checkpoint.id {
                return Err(invalid(format!("{at}.next[{n}]"), "a checkpoint follows itself"));
            }
            if !course.iter().any(|c| c.id == *next) {
                return Err(invalid(
                    format!("{at}.next[{n}]"),
                    format!("no checkpoint {next} on the course"),
                ));
            }
        }
        let volume = &checkpoint.volume;
        for (lo, hi) in volume.min.iter().zip(volume.max.iter()) {
            finite(*lo, &format!("{at}.volume.min"))?;
            finite(*hi, &format!("{at}.volume.max"))?;
            if lo > hi {
                return Err(invalid(format!("{at}.volume"), "min is above max"));
            }
        }
    }
    backstop(race.validate(), "race")
}

fn check_zone(zone: &ZoneSchedule) -> Result<(), RuleSetError> {
    use crate::zone::{MAX_ZONE_PHASES, MAX_ZONE_RADIUS};
    let radius_ok = |r: f32| r.is_finite() && (0.0..=MAX_ZONE_RADIUS).contains(&r);
    finite(zone.center[0], "session.zone.center")?;
    finite(zone.center[1], "session.zone.center")?;
    if !radius_ok(zone.radius) {
        return Err(invalid(
            "session.zone.radius",
            format!("a radius from 0 to {MAX_ZONE_RADIUS}"),
        ));
    }
    if zone.phases.len() > MAX_ZONE_PHASES {
        return Err(invalid(
            "session.zone.phases",
            format!("at most {MAX_ZONE_PHASES} phases"),
        ));
    }
    let mut before = zone.radius;
    for (index, phase) in zone.phases.iter().enumerate() {
        if !radius_ok(phase.end_radius) || phase.end_radius > before {
            return Err(invalid(
                format!("session.zone.phases[{index}].end_radius"),
                "a finite radius from 0 to the radius before the phase",
            ));
        }
        before = phase.end_radius;
    }
    backstop(zone.validate(), "zone")
}

fn check_elimination(rules: &EliminationRules, max_teams: u8) -> Result<(), RuleSetError> {
    if rules.alliances.len() > MAX_ALLIANCES {
        return Err(invalid(
            "session.elimination.alliances",
            format!("at most {MAX_ALLIANCES} alliances"),
        ));
    }
    let mut seen: Vec<u8> = Vec::new();
    for (a, alliance) in rules.alliances.iter().enumerate() {
        let at = format!("session.elimination.alliances[{a}].teams");
        if alliance.teams.is_empty() {
            return Err(invalid(at, "an alliance lists at least one team"));
        }
        for (t, team) in alliance.teams.iter().enumerate() {
            if !(1..=max_teams).contains(team) {
                return Err(invalid(
                    format!("{at}[{t}]"),
                    format!("not a fighting team (1 to max_teams, {max_teams})"),
                ));
            }
            if seen.contains(team) {
                return Err(invalid(
                    format!("{at}[{t}]"),
                    format!("team {team} is in two alliances or listed twice"),
                ));
            }
            seen.push(*team);
        }
    }
    Ok(())
}

fn check_victory(rules: &VictoryRules) -> Result<(), RuleSetError> {
    use crate::victory::MAX_TAG_LEN;
    if rules.conditions.len() > MAX_VICTORY_CONDITIONS {
        return Err(invalid(
            "session.victory.conditions",
            format!("at most {MAX_VICTORY_CONDITIONS} conditions"),
        ));
    }
    for (index, condition) in rules.conditions.iter().enumerate() {
        let at = format!("session.victory.conditions[{index}]");
        if rules.conditions[..index].contains(condition) {
            return Err(invalid(at, "the condition is listed twice"));
        }
        match condition {
            VictoryCondition::DestroyAllTagged { tag } if tag.is_empty() || tag.len() > MAX_TAG_LEN => {
                return Err(invalid(
                    format!("{at}.tag"),
                    format!("a tag is 1 to {MAX_TAG_LEN} bytes"),
                ));
            }
            VictoryCondition::HoldFor { ticks: 0, .. } => {
                return Err(invalid(format!("{at}.ticks"), "a hold of 0 ticks"));
            }
            _ => {}
        }
    }
    let surrender = &rules.surrender;
    if !(1..=100).contains(&surrender.threshold_percent) {
        return Err(invalid(
            "session.victory.surrender.threshold_percent",
            "a percentage from 1 to 100",
        ));
    }
    if surrender.window_ticks == 0 {
        return Err(invalid(
            "session.victory.surrender.window_ticks",
            "a ballot open for 0 ticks",
        ));
    }
    backstop(rules.validate(), "victory")
}

fn win_states(wins: &[WinConditionDef]) -> Result<Vec<WinRuleState>, RuleSetError> {
    if wins.len() > MAX_WIN_CONDITIONS {
        return Err(RuleSetError::new(
            codes::INVALID,
            "win_conditions",
            format!("at most {MAX_WIN_CONDITIONS} win conditions"),
        ));
    }
    let mut out = Vec::with_capacity(wins.len());
    for (index, win) in wins.iter().enumerate() {
        let rule = StockRule::from_lobby_str(&win.rule).ok_or_else(|| {
            RuleSetError::new(
                codes::INVALID,
                format!("win_conditions[{index}].rule"),
                format!("`{}` is not a win condition (lives, structures)", win.rule),
            )
        })?;
        let mut state = WinRuleState {
            rule,
            stems: win.stems.clone(),
            ..WinRuleState::default()
        };
        if let Some(lives) = win.lives {
            state.lives = clamp_lives(lives).map_err(|error| {
                RuleSetError::new(
                    codes::INVALID,
                    format!("win_conditions[{index}].lives"),
                    error.to_string(),
                )
            })?;
        }
        out.push(state);
    }
    Ok(out)
}

fn check_option_def(def: &OptionDef, path: &str) -> Result<(), RuleSetError> {
    let key_ok = !def.key.is_empty()
        && def.key.len() <= MAX_ID_BYTES
        && def
            .key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !key_ok {
        return Err(RuleSetError::new(
            codes::INVALID,
            format!("{path}.key"),
            format!(
                "`{}` is not an option key ([a-z0-9_], 1 to {MAX_ID_BYTES} bytes)",
                def.key
            ),
        ));
    }
    if RESERVED_ROW_KEYS.contains(&def.key.as_str()) {
        return Err(RuleSetError::new(
            codes::INVALID,
            format!("{path}.key"),
            format!("`{}` is a playlist row key, not a rule-set option", def.key),
        ));
    }
    let bounded = matches!(def.kind, OptionKind::Int | OptionKind::Float);
    if !bounded && (def.min.is_some() || def.max.is_some()) {
        return Err(RuleSetError::new(
            codes::INVALID,
            format!("{path}.min"),
            format!("a {} option has no min or max", def.kind.as_str()),
        ));
    }
    if def.kind == OptionKind::Enum {
        if def.choices.is_empty() || def.choices.len() > MAX_OPTIONS {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("{path}.choices"),
                format!("an enum option lists 1 to {MAX_OPTIONS} choices"),
            ));
        }
        for (index, choice) in def.choices.iter().enumerate() {
            if choice.is_empty()
                || choice.len() > MAX_OPTION_TEXT_BYTES
                || def.choices[..index].contains(choice)
            {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    format!("{path}.choices[{index}]"),
                    "an empty, too long or repeated choice",
                ));
            }
        }
    } else if !def.choices.is_empty() {
        return Err(RuleSetError::new(
            codes::INVALID,
            format!("{path}.choices"),
            "only an enum option lists choices",
        ));
    }
    let unbounded = OptionDef {
        min: None,
        max: None,
        ..def.clone()
    };
    let lo = def
        .min
        .as_deref()
        .map(|text| parse_value(&unbounded, text, &format!("{path}.min")))
        .transpose()?;
    let hi = def
        .max
        .as_deref()
        .map(|text| parse_value(&unbounded, text, &format!("{path}.max")))
        .transpose()?;
    let inverted = match (&lo, &hi) {
        (Some(OptionValue::Int(lo)), Some(OptionValue::Int(hi))) => lo > hi,
        (Some(OptionValue::Float(lo)), Some(OptionValue::Float(hi))) => lo > hi,
        _ => false,
    };
    if inverted {
        return Err(RuleSetError::new(
            codes::INVALID,
            format!("{path}.max"),
            "max is below min",
        ));
    }
    parse_value(def, &def.default, &format!("{path}.default"))?;
    if let Some(bind) = &def.bind {
        match OPTION_BINDS.iter().find(|(name, _)| name == bind) {
            None => {
                let names: Vec<&str> = OPTION_BINDS.iter().map(|(name, _)| *name).collect();
                return Err(RuleSetError::new(
                    codes::INVALID,
                    format!("{path}.bind"),
                    format!("`{bind}` is not a bindable session field ({})", names.join(", ")),
                ));
            }
            Some((_, kind)) if *kind != def.kind => {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    format!("{path}.bind"),
                    format!(
                        "`{bind}` takes a {} option, not a {} one",
                        kind.as_str(),
                        def.kind.as_str()
                    ),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Refuses two options that bind the same session field (`<path>.bind` of the second).
fn check_binds<'a>(defs: impl Iterator<Item = (String, &'a OptionDef)>) -> Result<(), RuleSetError> {
    let mut seen: Vec<&str> = Vec::new();
    for (path, def) in defs {
        let Some(bind) = def.bind.as_deref() else {
            continue;
        };
        if seen.contains(&bind) {
            return Err(RuleSetError::new(
                codes::INVALID,
                format!("{path}.bind"),
                format!("`{bind}` is bound by two options"),
            ));
        }
        seen.push(bind);
    }
    Ok(())
}

/// Writes every bound option's value into its session field ([`OptionDef::bind`], ADR-8881).
fn bind_options(
    schema: &BTreeMap<String, OptionDef>,
    values: &BTreeMap<String, OptionValue>,
    settings: &mut SessionSettings,
    zones: &mut ZoneRules,
) -> Result<(), RuleSetError> {
    check_binds(schema.iter().map(|(key, def)| (format!("options.{key}"), def)))?;
    for (key, def) in schema {
        let (Some(bind), Some(value)) = (def.bind.as_deref(), values.get(key)) else {
            continue;
        };
        let path = format!("options.{key}");
        let range = |what: &str| {
            RuleSetError::new(
                codes::OPTION_RANGE,
                path.as_str(),
                format!("`{bind}` cannot hold {what}"),
            )
        };
        let int = |value: &OptionValue| match value {
            OptionValue::Int(v) => u64::try_from(*v).map_err(|_| range(&v.to_string())),
            _ => Err(range("this kind")),
        };
        let u32_of = |value: &OptionValue| {
            int(value).and_then(|v| u32::try_from(v).map_err(|_| range(&v.to_string())))
        };
        let u8_of =
            |value: &OptionValue| int(value).and_then(|v| u8::try_from(v).map_err(|_| range(&v.to_string())));
        // On an optional field, 0 is off.
        let opt = |v: u64| (v != 0).then_some(v);
        let flag = |value: &OptionValue| match value {
            OptionValue::Bool(v) => Ok(*v),
            _ => Err(range("this kind")),
        };
        let phases = &mut settings.flow.phases;
        let objectives = &mut settings.objectives;
        match bind {
            "flow.start_delay_ticks" => settings.flow.start_delay_ticks = int(value)?,
            "phases.warmup_ticks" => phases.warmup_ticks = opt(int(value)?),
            "phases.warmup_min_seats" => phases.warmup_min_seats = Some(u8_of(value)?).filter(|v| *v != 0),
            "phases.countdown_ticks" => phases.countdown_ticks = int(value)?,
            "phases.time_limit_ticks" => phases.time_limit_ticks = opt(int(value)?),
            "phases.post_match_ticks" => phases.post_match_ticks = opt(int(value)?),
            "phases.forfeit_after_ticks" => phases.forfeit_after_ticks = opt(int(value)?),
            "objectives.flag_touch_radius" => match value {
                OptionValue::Float(v) => objectives.flag_touch_radius = *v,
                _ => return Err(range("this kind")),
            },
            "objectives.flag_return_ticks" => objectives.flag_return_ticks = opt(int(value)?),
            "objectives.capture_needs_home_flag" => objectives.capture_needs_home_flag = flag(value)?,
            "objectives.capture_limit" => objectives.capture_limit = u32_of(value)?,
            "objectives.hill_score_limit" => objectives.hill_score_limit = u32_of(value)?,
            "objectives.hill_accrual_ticks" => objectives.hill_accrual_ticks = u32_of(value)?,
            "teams.rebalance_between_rounds" => settings.teams.rebalance_between_rounds = flag(value)?,
            "teams.assist_window_ticks" => settings.teams.assist_window_ticks = int(value)?,
            "respawn.seed" => settings.respawn.seed = int(value)?,
            "zones.score_limit" => zones.score_limit = Some(u32_of(value)?).filter(|v| *v != 0),
            "race.laps" => settings.race.laps = u32_of(value)?,
            "victory.surrender.window_ticks" => settings.victory.surrender.window_ticks = int(value)?,
            other => {
                return Err(RuleSetError::new(
                    codes::INVALID,
                    format!("options.{key}.bind"),
                    format!("`{other}` is not a bindable session field"),
                ));
            }
        }
    }
    Ok(())
}

/// Parse `text` as `def`'s kind and check its bounds. Floats parse straight to `f32`.
fn parse_value(def: &OptionDef, text: &str, path: &str) -> Result<OptionValue, RuleSetError> {
    let kind_error = || {
        RuleSetError::new(
            codes::OPTION_KIND,
            path,
            format!(
                "`{text}` is not a{} {} value",
                if def.kind == OptionKind::Int { "n" } else { "" },
                def.kind.as_str()
            ),
        )
    };
    if text.len() > MAX_OPTION_TEXT_BYTES {
        return Err(RuleSetError::new(
            codes::OPTION_RANGE,
            path,
            format!("longer than {MAX_OPTION_TEXT_BYTES} bytes"),
        ));
    }
    let value = match def.kind {
        OptionKind::Int => OptionValue::Int(text.trim().parse::<i64>().map_err(|_| kind_error())?),
        OptionKind::Float => {
            let value = text.trim().parse::<f32>().map_err(|_| kind_error())?;
            if !value.is_finite() {
                return Err(kind_error());
            }
            OptionValue::Float(value)
        }
        OptionKind::Bool => match text.trim() {
            "true" => OptionValue::Bool(true),
            "false" => OptionValue::Bool(false),
            _ => return Err(kind_error()),
        },
        OptionKind::Enum => {
            if !def.choices.iter().any(|choice| choice == text) {
                return Err(RuleSetError::new(
                    codes::OPTION_KIND,
                    path,
                    format!("`{text}` is not one of {}", def.choices.join(", ")),
                ));
            }
            OptionValue::Enum(text.to_owned())
        }
        OptionKind::String => OptionValue::String(text.to_owned()),
    };
    let bound = |text: &Option<String>| -> Option<OptionValue> {
        let unbounded = OptionDef {
            min: None,
            max: None,
            ..def.clone()
        };
        text.as_deref()
            .and_then(|t| parse_value(&unbounded, t, path).ok())
    };
    let out_of_range = match (&value, bound(&def.min), bound(&def.max)) {
        (OptionValue::Int(v), lo, hi) => {
            matches!(lo, Some(OptionValue::Int(lo)) if *v < lo)
                || matches!(hi, Some(OptionValue::Int(hi)) if *v > hi)
        }
        (OptionValue::Float(v), lo, hi) => {
            matches!(lo, Some(OptionValue::Float(lo)) if *v < lo)
                || matches!(hi, Some(OptionValue::Float(hi)) if *v > hi)
        }
        _ => false,
    };
    if out_of_range {
        return Err(RuleSetError::new(
            codes::OPTION_RANGE,
            path,
            format!(
                "`{text}` is outside {}..={}",
                def.min.as_deref().unwrap_or(""),
                def.max.as_deref().unwrap_or("")
            ),
        ));
    }
    Ok(value)
}

fn resolve_options(
    schema: &BTreeMap<String, OptionDef>,
    row: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, OptionValue>, RuleSetError> {
    for key in row.keys() {
        if !schema.contains_key(key) && !RESERVED_ROW_KEYS.contains(&key.as_str()) {
            return Err(RuleSetError::new(
                codes::OPTION_UNKNOWN,
                format!("options.{key}"),
                format!(
                    "the rule set has no option `{key}` (it has: {})",
                    schema.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }
    let mut out = BTreeMap::new();
    for (key, def) in schema {
        let text = row.get(key).unwrap_or(&def.default);
        out.insert(key.clone(), parse_value(def, text, &format!("options.{key}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn int_option(key: &str, default: &str, min: &str, max: &str) -> OptionDef {
        OptionDef {
            key: key.into(),
            kind: OptionKind::Int,
            default: default.into(),
            min: Some(min.into()),
            max: Some(max.into()),
            choices: Vec::new(),
            bind: None,
        }
    }

    #[test]
    fn patch_fields_cover_every_base_field() {
        fn keys<T: Serialize>(value: &T) -> Vec<String> {
            match serde_json::to_value(value).expect("json") {
                serde_json::Value::Object(map) => map.keys().cloned().collect(),
                other => panic!("not an object: {other}"),
            }
        }
        let mut flow = keys(&FlowSettings::default());
        flow.retain(|k| k != "phases");
        let mut have: Vec<String> = FlowPatch::FIELDS.iter().map(|s| (*s).to_owned()).collect();
        flow.sort();
        have.sort();
        assert_eq!(flow, have, "FlowPatch");
        for (name, mut base, fields) in [
            ("PhasePatch", keys(&PhaseSettings::default()), PhasePatch::FIELDS),
            (
                "ObjectivePatch",
                keys(&ObjectiveSettings::default()),
                ObjectivePatch::FIELDS,
            ),
            (
                "RespawnPatch",
                keys(&RespawnRules::default()),
                RespawnPatch::FIELDS,
            ),
        ] {
            let mut have: Vec<String> = fields.iter().map(|s| (*s).to_owned()).collect();
            base.sort();
            have.sort();
            assert_eq!(base, have, "{name}");
        }
        let mut teams = keys(&TeamRules::default());
        teams.retain(|k| k != "defs");
        let mut have: Vec<String> = TeamsPatch::FIELDS.iter().map(|s| (*s).to_owned()).collect();
        teams.sort();
        have.sort();
        assert_eq!(teams, have, "TeamsPatch");
        // The sections the v0.7 roll-up merged after the first `rules-ron` v1 shape (co-op waves,
        // races, the shrinking zone, elimination, victory; ADR-8731) are patchable since ADR-8748:
        // every `SessionSettings` section has a patch of the same name. `phases` patches
        // `flow.phases`, `zones` the board's zone rules.
        let mut session = keys(&SessionSettings::default());
        let mut sections = keys(&SessionPatch::default());
        sections.retain(|k| k != "phases" && k != "zones");
        session.sort();
        sections.sort();
        assert_eq!(session, sections, "SessionPatch sections");
        for (name, mut base, fields) in [
            ("WavesPatch", keys(&WaveSettings::default()), WavesPatch::FIELDS),
            ("RacePatch", keys(&RaceSettings::default()), RacePatch::FIELDS),
            ("ZonePatch", keys(&ZoneSchedule::default()), ZonePatch::FIELDS),
            (
                "EliminationPatch",
                keys(&EliminationRules::default()),
                EliminationPatch::FIELDS,
            ),
            (
                "VictoryPatch",
                keys(&VictoryRules::default()),
                VictoryPatch::FIELDS,
            ),
            (
                "SurrenderPatch",
                keys(&SurrenderRules::default()),
                SurrenderPatch::FIELDS,
            ),
        ] {
            let mut have: Vec<String> = fields.iter().map(|s| (*s).to_owned()).collect();
            base.sort();
            have.sort();
            assert_eq!(base, have, "{name}");
        }
        // Each row type of the file serializes with exactly the field names of its session type.
        let group = WaveGroupDef {
            archetype: "a".into(),
            count: 1,
            spawn_tag: String::new(),
        };
        let checkpoint = CheckpointDef {
            id: 1,
            volume: VolumeDef {
                min: [0.0; 3],
                max: [1.0; 3],
            },
            next: vec![2],
        };
        let phase = ZonePhaseDef {
            wait_ticks: 1,
            shrink_ticks: 1,
            end_radius: 1.0,
            damage_per_tick: 1,
        };
        let row = WaveRowDef {
            at: WaveStartDef::default(),
            groups: vec![group.clone()],
            alive_cap: 0,
        };
        let difficulty = DifficultyRowDef {
            players: 1,
            count_percent: 100,
            cap_percent: 100,
        };
        let mut waves = WaveSettings::default();
        WavesPatch {
            waves: Some(vec![row.clone()]),
            difficulty: Some(vec![difficulty]),
            ..WavesPatch::default()
        }
        .apply_to(&mut waves);
        let mut race = RaceSettings::default();
        RacePatch {
            checkpoints: Some(vec![checkpoint.clone()]),
            ..RacePatch::default()
        }
        .apply_to(&mut race);
        let mut zone = ZoneSchedule::default();
        ZonePatch {
            phases: Some(vec![phase]),
            ..ZonePatch::default()
        }
        .apply_to(&mut zone);
        for (name, def, core) in [
            ("WaveRowDef", keys(&row), keys(&waves.waves[0])),
            ("WaveGroupDef", keys(&group), keys(&waves.waves[0].groups[0])),
            ("DifficultyRowDef", keys(&difficulty), keys(&waves.difficulty[0])),
            ("CheckpointDef", keys(&checkpoint), keys(&race.checkpoints[0])),
            (
                "VolumeDef",
                keys(&checkpoint.volume),
                keys(&race.checkpoints[0].volume),
            ),
            ("ZonePhaseDef", keys(&phase), keys(&zone.phases[0])),
            (
                "AllianceDef",
                keys(&AllianceDef { teams: vec![1] }),
                keys(&Alliance { teams: vec![1] }),
            ),
        ] {
            let (mut def, mut core) = (def, core);
            def.sort();
            core.sort();
            assert_eq!(def, core, "{name}");
        }
        // The enums keep the session's `kind` words and fields.
        for (def, core) in [
            (
                WaveStartDef::AfterCleared { delay_ticks: 3 },
                WaveStartDef::AfterCleared { delay_ticks: 3 }.start(),
            ),
            (
                WaveStartDef::AtTick { tick: 9 },
                WaveStartDef::AtTick { tick: 9 }.start(),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(def).expect("json"),
                serde_json::to_value(core).expect("json")
            );
        }
        for def in [
            VictoryConditionDef::DestroyAllTagged { tag: "hq".into() },
            VictoryConditionDef::HoldFor {
                objective: 2,
                ticks: 5,
            },
            VictoryConditionDef::ScoreThreshold { score: -4 },
            VictoryConditionDef::TeamElimination,
            VictoryConditionDef::Surrender,
        ] {
            assert_eq!(
                serde_json::to_value(&def).expect("json"),
                serde_json::to_value(def.condition()).expect("json")
            );
        }
    }

    #[test]
    fn a_bare_skeleton_id_is_the_rust_defaults() {
        let library = RuleSetLibrary::new();
        for mode in MatchMode::ALL {
            let resolved = library
                .resolve(mode.as_lobby_str(), &row(&[("anything", "x")]))
                .expect("skeleton");
            assert_eq!(resolved, ResolvedRules::skeleton(mode));
        }
        let err = library
            .resolve("ctf_long", &BTreeMap::new())
            .expect_err("unknown");
        assert_eq!(err.code, codes::UNKNOWN);
    }

    #[test]
    fn extends_overrides_only_the_named_fields_and_a_cycle_is_refused() {
        let mut library = RuleSetLibrary::new();
        let mut base = RuleSet::new("ctf_base", MatchMode::CaptureTheFlag);
        base.session.objectives.capture_limit = Some(5);
        base.session.objectives.flag_touch_radius = Some(2.5);
        base.options.push(int_option("score", "10", "1", "50"));
        library.insert(base).expect("base");
        let mut child = RuleSet {
            skeleton: None,
            extends: Some("ctf_base".into()),
            ..RuleSet::new("ctf_long", MatchMode::Custom)
        };
        child.session.objectives.capture_limit = Some(7);
        child.session.flow.start_delay_ticks = Some(9);
        library.insert(child).expect("child");
        let resolved = library.resolve("ctf_long", &BTreeMap::new()).expect("resolves");
        let mut expected = SessionSettings::default();
        expected.objectives.capture_limit = 7;
        expected.objectives.flag_touch_radius = 2.5;
        expected.flow.start_delay_ticks = 9;
        assert_eq!(resolved.settings, expected);
        assert_eq!(
            resolved.mode,
            MatchMode::CaptureTheFlag,
            "the skeleton is inherited"
        );
        assert_eq!(resolved.chain, ["ctf_base", "ctf_long"]);
        assert_eq!(
            resolved.options["score"],
            OptionValue::Int(10),
            "the option is inherited"
        );

        // a -> b -> a.
        let mut cyclic = RuleSetLibrary::new();
        for (id, parent) in [("a", "b"), ("b", "a")] {
            cyclic
                .insert(RuleSet {
                    extends: Some(parent.into()),
                    ..RuleSet::new(id, MatchMode::TeamDeathmatch)
                })
                .expect("insert");
        }
        let err = cyclic.resolve("a", &BTreeMap::new()).expect_err("cycle");
        assert_eq!(err.code, codes::CYCLE);
        assert!(err.message.contains("a -> b -> a"), "{err}");
        assert!(err.path.ends_with("extends"), "{err}");

        // A missing parent names the child's extends.
        let mut orphan = RuleSetLibrary::new();
        orphan
            .insert(RuleSet {
                extends: Some("gone".into()),
                ..RuleSet::new("kid", MatchMode::TeamDeathmatch)
            })
            .expect("insert");
        let err = orphan
            .resolve("kid", &BTreeMap::new())
            .expect_err("missing parent");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::UNKNOWN, "kid: extends")
        );
    }

    #[test]
    fn the_depth_cap_is_four_ancestors() {
        let mut library = RuleSetLibrary::new();
        library
            .insert(RuleSet::new("l0", MatchMode::KingOfTheHill))
            .expect("root");
        for n in 1..=5 {
            library
                .insert(RuleSet {
                    skeleton: None,
                    extends: Some(format!("l{}", n - 1)),
                    ..RuleSet::new(format!("l{n}"), MatchMode::Custom)
                })
                .expect("insert");
        }
        assert_eq!(
            library
                .resolve("l4", &BTreeMap::new())
                .expect("4 ancestors")
                .chain
                .len(),
            5
        );
        let err = library.resolve("l5", &BTreeMap::new()).expect_err("5 ancestors");
        assert_eq!(err.code, codes::DEPTH);
    }

    #[test]
    fn row_options_out_of_range_are_refused_with_the_field_path() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("ctf_custom", MatchMode::CaptureTheFlag);
        set.options = vec![
            int_option("score_limit", "3", "1", "10"),
            OptionDef {
                key: "wind".into(),
                kind: OptionKind::Float,
                default: "0.1".into(),
                min: Some("0".into()),
                max: Some("1.5".into()),
                choices: Vec::new(),
                bind: None,
            },
            OptionDef {
                key: "sky".into(),
                kind: OptionKind::Enum,
                default: "day".into(),
                min: None,
                max: None,
                choices: vec!["day".into(), "night".into()],
                bind: None,
            },
        ];
        library.insert(set).expect("insert");
        let check = |pairs: &[(&str, &str)]| library.resolve("ctf_custom", &row(pairs));
        let err = check(&[("score_limit", "11")]).expect_err("range");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_RANGE, "options.score_limit")
        );
        let err = check(&[("wind", "2.0")]).expect_err("float range");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_RANGE, "options.wind")
        );
        let err = check(&[("score_limit", "three")]).expect_err("kind");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_KIND, "options.score_limit")
        );
        let err = check(&[("sky", "dusk")]).expect_err("choice");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_KIND, "options.sky")
        );
        let err = check(&[("tickets", "100")]).expect_err("unknown");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_UNKNOWN, "options.tickets")
        );
        // In range: the row wins over the default, the float is the f32 the text names, and the
        // playlist's own keys pass through.
        let ok = check(&[("score_limit", "10"), ("wind", "0.3"), ("min_players", "2")]).expect("in range");
        assert_eq!(ok.options["score_limit"], OptionValue::Int(10));
        assert_eq!(ok.options["wind"], OptionValue::Float(0.3_f32));
        assert_eq!(ok.options["sky"], OptionValue::Enum("day".into()));
        assert!(!ok.options.contains_key("min_players"));
    }

    #[test]
    fn a_bad_schema_is_refused_when_inserted() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("bad", MatchMode::TeamDeathmatch);
        set.options = vec![int_option("kills", "100", "1", "50")];
        let err = library.insert(set.clone()).expect_err("default out of range");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_RANGE, "bad: options[0].default")
        );
        set.options = vec![
            int_option("kills", "10", "1", "50"),
            int_option("kills", "10", "1", "50"),
        ];
        assert_eq!(
            library.insert(set.clone()).expect_err("twice").code,
            codes::INVALID
        );
        set.options.clear();
        set.skeleton = Some("tug".into());
        let err = library.insert(set.clone()).expect_err("skeleton");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::INVALID, "bad: skeleton")
        );
        set.skeleton = Some("tdm".into());
        set.format_version = 2;
        assert_eq!(
            library.insert(set.clone()).expect_err("version").code,
            codes::VERSION
        );
        set.format_version = 1;
        library.insert(set.clone()).expect("good");
        assert_eq!(library.insert(set).expect_err("dup").code, codes::DUPLICATE);
    }

    #[test]
    fn bad_merged_settings_name_their_field() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("nan", MatchMode::CaptureTheFlag);
        set.session.objectives.flag_touch_radius = Some(f32::NAN);
        library.insert(set).expect("insert");
        let err = library.resolve("nan", &BTreeMap::new()).expect_err("nan");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::INVALID, "nan: session.objectives.flag_touch_radius")
        );
        let mut set = RuleSet::new("warm", MatchMode::CaptureTheFlag);
        set.session.phases.warmup_ticks = Some(Some(0));
        library.insert(set).expect("insert");
        let err = library.resolve("warm", &BTreeMap::new()).expect_err("phase");
        assert_eq!(err.path, "warm: session.phases.warmup_ticks");
    }

    #[test]
    fn a_row_naming_a_rule_set_launches_on_its_skeleton() {
        let mut library = RuleSetLibrary::new();
        library
            .insert(RuleSet::new("ctf_long", MatchMode::CaptureTheFlag))
            .expect("insert");
        let rows = vec![
            PlaylistEntry::new("harbor", "ctf_long"),
            PlaylistEntry::new("quarry", "koth"),
        ];
        let modes = library.rule_modes(&rows).expect("modes");
        assert_eq!(modes.len(), 1, "a bare skeleton id needs no entry");
        let launch = |modes: BTreeMap<String, MatchMode>| {
            let mut session = MatchSession {
                rule_modes: modes,
                ..MatchSession::default()
            };
            session.playlist.set(rows.clone());
            session.start_now();
            let first = session.board.mode;
            session.start_now();
            (first, session.board.mode)
        };
        assert_eq!(
            launch(modes),
            (MatchMode::CaptureTheFlag, MatchMode::KingOfTheHill)
        );
        // Without the map a rule-set id is S-14's unknown mode string.
        assert_eq!(
            launch(BTreeMap::new()),
            (MatchMode::Custom, MatchMode::KingOfTheHill)
        );
        // A row naming a rule set with a bad option is refused with the row in the path.
        let mut bad = rows.clone();
        bad[0].options.insert("tickets".into(), "5".into());
        let err = library.rule_modes(&bad).expect_err("unknown option");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_UNKNOWN, "rows[0].options.tickets")
        );
    }

    #[test]
    fn a_zone_listed_twice_is_refused() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("hp", MatchMode::Domination);
        set.session.zones.mode = Some(ZoneMode::Hardpoint {
            rotation: vec![7, 8, 7],
            rotate_ticks: 600,
        });
        library.insert(set).expect("insert");
        let err = library.resolve("hp", &BTreeMap::new()).expect_err("repeat");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::INVALID, "hp: session.zones.mode.rotation[2]")
        );
        let mut set = RuleSet::new("ad", MatchMode::Domination);
        set.session.zones.mode = Some(ZoneMode::AttackDefend {
            attackers: 1,
            defenders: 2,
            order: (0..9).collect(),
        });
        library.insert(set).expect("insert");
        let err = library.resolve("ad", &BTreeMap::new()).expect_err("too many");
        assert_eq!(err.path, "ad: session.zones.mode.order");
    }

    #[test]
    fn the_hash_covers_rules_not_names_and_moves_with_one_bit() {
        let a = ResolvedRules::skeleton(MatchMode::CaptureTheFlag);
        let mut renamed = a.clone();
        renamed.id = "other".into();
        renamed.name_key = "mode.other".into();
        assert_eq!(a.content_hash(), renamed.content_hash());
        let mut nudged = a.clone();
        nudged.settings.objectives.flag_touch_radius =
            f32::from_bits(a.settings.objectives.flag_touch_radius.to_bits() + 1);
        assert_ne!(a.content_hash(), nudged.content_hash());
        assert_ne!(
            a.content_hash(),
            ResolvedRules::skeleton(MatchMode::KingOfTheHill).content_hash()
        );
        assert_ne!(
            combined_hash(&[&a]),
            combined_hash(&[&renamed]),
            "ids are part of a playlist hash"
        );
    }

    #[test]
    fn resolved_rules_start_a_session() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("lives", MatchMode::TeamDeathmatch);
        set.win_conditions = Some(vec![WinConditionDef {
            rule: "lives".into(),
            lives: Some(5),
            stems: Vec::new(),
        }]);
        set.session.flow.start_delay_ticks = Some(4);
        library.insert(set).expect("insert");
        let resolved = library.resolve("lives", &BTreeMap::new()).expect("resolve");
        let session = resolved.session();
        assert_eq!(session.win_rule.lives, 5);
        assert_eq!(session.flow.settings.start_delay_ticks, 4);
        assert_eq!(session.board.mode, MatchMode::TeamDeathmatch);
        let mut bad = RuleSet::new("bad_lives", MatchMode::TeamDeathmatch);
        bad.win_conditions = Some(vec![WinConditionDef {
            rule: "lives".into(),
            lives: Some(11),
            stems: Vec::new(),
        }]);
        assert_eq!(
            library.insert(bad).expect_err("lives").path,
            "bad_lives: win_conditions[0].lives"
        );
    }

    fn one_wave() -> WavesPatch {
        WavesPatch {
            waves: Some(vec![WaveRowDef {
                at: WaveStartDef::AtTick { tick: 30 },
                groups: vec![WaveGroupDef {
                    archetype: "grunt".into(),
                    count: 4,
                    spawn_tag: "east".into(),
                }],
                alive_cap: 3,
            }]),
            shared_lives: Some(Some(5)),
            ..WavesPatch::default()
        }
    }

    #[test]
    fn the_genre_sections_resolve_through_extends() {
        let mut library = RuleSetLibrary::new();
        let mut base = RuleSet::new("br_base", MatchMode::Custom);
        base.session.zone = Some(Some(ZonePatch {
            radius: Some(200.0),
            phases: Some(vec![ZonePhaseDef {
                wait_ticks: 10,
                shrink_ticks: 50,
                end_radius: 20.0,
                damage_per_tick: 2,
            }]),
            seed: Some(7),
            ..ZonePatch::default()
        }));
        base.session.elimination.last_standing = Some(true);
        base.session.waves = one_wave();
        base.session.victory.conditions = Some(vec![VictoryConditionDef::Surrender]);
        base.session.victory.surrender.threshold_percent = Some(50);
        library.insert(base).expect("base");
        // The child moves the center only, keeps the parent's phases and seed, and adds a course.
        let mut child = RuleSet {
            skeleton: None,
            extends: Some("br_base".into()),
            ..RuleSet::new("br_child", MatchMode::Custom)
        };
        child.session.zone = Some(Some(ZonePatch {
            center: Some([5.0, -5.0]),
            ..ZonePatch::default()
        }));
        child.session.race.checkpoints = Some(vec![
            CheckpointDef {
                id: 1,
                volume: VolumeDef {
                    min: [-1.0; 3],
                    max: [1.0; 3],
                },
                next: vec![2],
            },
            CheckpointDef {
                id: 2,
                volume: VolumeDef {
                    min: [9.0, -1.0, -1.0],
                    max: [11.0, 1.0, 1.0],
                },
                next: vec![1],
            },
        ]);
        child.session.race.laps = Some(2);
        library.insert(child).expect("child");
        let resolved = library.resolve("br_child", &BTreeMap::new()).expect("resolves");
        let zone = resolved.settings.zone.clone().expect("zone on");
        assert_eq!(zone.center, [5.0, -5.0]);
        assert_eq!((zone.radius, zone.seed, zone.phases.len()), (200.0, 7, 1));
        assert!(resolved.settings.elimination.last_standing);
        assert_eq!(
            resolved.settings.waves.waves[0].at,
            WaveStart::AtTick { tick: 30 }
        );
        assert_eq!(resolved.settings.waves.shared_lives, Some(5));
        assert_eq!(resolved.settings.race.laps, 2);
        assert_eq!(resolved.settings.race.checkpoints.len(), 2);
        assert_eq!(
            resolved.settings.victory.conditions,
            [VictoryCondition::Surrender]
        );
        assert_eq!(resolved.settings.victory.surrender.threshold_percent, 50);
        assert_eq!(
            resolved.settings.victory.surrender.window_ticks,
            SurrenderRules::default().window_ticks,
            "an unset surrender field keeps the default"
        );
        // The hash covers the new sections: one bit of the zone radius moves it.
        let mut nudged = resolved.clone();
        if let Some(zone) = nudged.settings.zone.as_mut() {
            zone.radius = f32::from_bits(zone.radius.to_bits() + 1);
        }
        assert_ne!(resolved.content_hash(), nudged.content_hash());

        // `Some(None)` turns the parent's zone off; an empty patch keeps every section.
        let mut off = RuleSet {
            skeleton: None,
            extends: Some("br_base".into()),
            ..RuleSet::new("no_zone", MatchMode::Custom)
        };
        off.session.zone = Some(None);
        library.insert(off).expect("off");
        let resolved = library.resolve("no_zone", &BTreeMap::new()).expect("resolves");
        assert_eq!(resolved.settings.zone, None);
        assert!(resolved.settings.elimination.last_standing, "inherited");
    }

    #[test]
    fn bad_genre_sections_name_their_field() {
        let check = |edit: &dyn Fn(&mut SessionPatch)| -> String {
            let mut library = RuleSetLibrary::new();
            let mut set = RuleSet::new("g", MatchMode::Custom);
            edit(&mut set.session);
            library.insert(set).expect("insert");
            let err = library.resolve("g", &BTreeMap::new()).expect_err("refused");
            assert_eq!(err.code, codes::INVALID, "{err}");
            err.path
        };
        assert_eq!(
            check(&|p| {
                p.waves = one_wave();
                if let Some(rows) = p.waves.waves.as_mut() {
                    rows[0].groups[0].count = 0;
                }
            }),
            "g: session.waves.waves[0].groups[0].count"
        );
        assert_eq!(
            check(&|p| {
                p.waves = one_wave();
                p.waves.players_team = Some(9);
            }),
            "g: session.waves.players_team"
        );
        assert_eq!(
            check(&|p| {
                p.waves.difficulty = Some(vec![
                    DifficultyRowDef {
                        players: 2,
                        count_percent: 100,
                        cap_percent: 100,
                    },
                    DifficultyRowDef {
                        players: 2,
                        count_percent: 150,
                        cap_percent: 100,
                    },
                ]);
            }),
            "g: session.waves.difficulty[1].players"
        );
        let gate = |id: u32, next: Vec<u32>| CheckpointDef {
            id,
            volume: VolumeDef {
                min: [0.0; 3],
                max: [1.0; 3],
            },
            next,
        };
        assert_eq!(
            check(&|p| p.race.checkpoints = Some(vec![gate(1, vec![2]), gate(2, vec![3])])),
            "g: session.race.checkpoints[1].next[0]"
        );
        assert_eq!(
            check(&|p| {
                let mut far = gate(2, vec![1]);
                far.volume.min[1] = f32::NAN;
                p.race.checkpoints = Some(vec![gate(1, vec![2]), far]);
            }),
            "g: session.race.checkpoints[1].volume.min"
        );
        assert_eq!(
            check(&|p| {
                p.race.checkpoints = Some(vec![gate(1, vec![2]), gate(2, vec![1])]);
                p.race.laps = Some(0);
            }),
            "g: session.race.laps"
        );
        assert_eq!(
            check(&|p| {
                p.zone = Some(Some(ZonePatch {
                    radius: Some(50.0),
                    phases: Some(vec![ZonePhaseDef {
                        wait_ticks: 1,
                        shrink_ticks: 1,
                        end_radius: 60.0,
                        damage_per_tick: 1,
                    }]),
                    ..ZonePatch::default()
                }));
            }),
            "g: session.zone.phases[0].end_radius"
        );
        assert_eq!(
            check(&|p| p.zone = Some(Some(ZonePatch {
                radius: Some(f32::INFINITY),
                ..ZonePatch::default()
            }))),
            "g: session.zone.radius"
        );
        assert_eq!(
            check(&|p| p.elimination.alliances = Some(vec![
                AllianceDef { teams: vec![1, 2] },
                AllianceDef { teams: vec![2, 3] },
            ])),
            "g: session.elimination.alliances[1].teams[0]"
        );
        assert_eq!(
            check(&|p| p.elimination.alliances = Some(vec![AllianceDef { teams: vec![0] }])),
            "g: session.elimination.alliances[0].teams[0]"
        );
        assert_eq!(
            check(&|p| p.victory.conditions =
                Some(vec![VictoryConditionDef::DestroyAllTagged { tag: String::new() }])),
            "g: session.victory.conditions[0].tag"
        );
        assert_eq!(
            check(
                &|p| p.victory.conditions = Some(vec![VictoryConditionDef::HoldFor {
                    objective: 1,
                    ticks: 0,
                }])
            ),
            "g: session.victory.conditions[0].ticks"
        );
        assert_eq!(
            check(&|p| p.victory.conditions = Some(vec![
                VictoryConditionDef::Surrender,
                VictoryConditionDef::Surrender,
            ])),
            "g: session.victory.conditions[1]"
        );
        assert_eq!(
            check(&|p| p.victory.surrender.threshold_percent = Some(0)),
            "g: session.victory.surrender.threshold_percent"
        );
        assert_eq!(
            check(&|p| p.victory.surrender.window_ticks = Some(0)),
            "g: session.victory.surrender.window_ticks"
        );
    }

    /// R-04 (6), ADR-8881: an option with `bind` writes its value (the row's, else the default)
    /// into that session field before the settings are checked, so a playlist row's option
    /// changes the match without game code. An optional field takes 0 as off. A bind outside the
    /// table, of the wrong kind, bound twice, or a value the field cannot hold is refused.
    #[test]
    fn a_bound_option_sets_its_session_field() {
        let mut library = RuleSetLibrary::new();
        let mut set = RuleSet::new("ctf_bound", MatchMode::CaptureTheFlag);
        let mut limit = int_option("caps", "3", "1", "10");
        limit.bind = Some("objectives.capture_limit".into());
        let mut time = int_option("minutes", "0", "0", "60000");
        time.bind = Some("phases.time_limit_ticks".into());
        let home = OptionDef {
            key: "home".into(),
            kind: OptionKind::Bool,
            default: "true".into(),
            min: None,
            max: None,
            choices: Vec::new(),
            bind: Some("objectives.capture_needs_home_flag".into()),
        };
        set.options = vec![limit, time, home];
        set.check().expect("binds check");
        library.insert(set).expect("insert");
        let resolved = library.resolve("ctf_bound", &row(&[])).expect("defaults");
        assert_eq!(resolved.settings.objectives.capture_limit, 3);
        assert_eq!(resolved.settings.flow.phases.time_limit_ticks, None, "0 is off");
        assert!(resolved.settings.objectives.capture_needs_home_flag);
        let resolved = library
            .resolve(
                "ctf_bound",
                &row(&[("caps", "7"), ("minutes", "3600"), ("home", "false")]),
            )
            .expect("row values");
        assert_eq!(resolved.settings.objectives.capture_limit, 7);
        assert_eq!(resolved.settings.flow.phases.time_limit_ticks, Some(3600));
        assert!(!resolved.settings.objectives.capture_needs_home_flag);
        assert_eq!(resolved.session().board.settings.capture_limit, 7);
        let unbound = library
            .resolve("ctf_bound", &row(&[("caps", "3")]))
            .expect("same");
        assert_ne!(
            resolved.content_hash(),
            unbound.content_hash(),
            "the bound field is in the hash"
        );

        let refused = |def: OptionDef| {
            let mut set = RuleSet::new("bad", MatchMode::CaptureTheFlag);
            set.options = vec![def];
            let err = set.check().expect_err("refused");
            (err.code, err.path)
        };
        let mut unknown = int_option("x", "1", "0", "5");
        unknown.bind = Some("objectives.no_such_field".into());
        assert_eq!(
            refused(unknown),
            (codes::INVALID.to_owned(), "bad: options[0].bind".to_owned())
        );
        let mut wrong_kind = int_option("x", "1", "0", "5");
        wrong_kind.bind = Some("objectives.capture_needs_home_flag".into());
        assert_eq!(
            refused(wrong_kind),
            (codes::INVALID.to_owned(), "bad: options[0].bind".to_owned())
        );
        let mut twice = RuleSet::new("twice", MatchMode::CaptureTheFlag);
        let mut a = int_option("a", "1", "1", "5");
        a.bind = Some("objectives.capture_limit".into());
        let mut b = int_option("b", "1", "1", "5");
        b.bind = Some("objectives.capture_limit".into());
        twice.options = vec![a, b];
        let err = twice.check().expect_err("bound twice");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::INVALID, "twice: options[1].bind")
        );
        // A value the field cannot hold: a negative tick count.
        let mut lib = RuleSetLibrary::new();
        let mut set = RuleSet::new("neg", MatchMode::CaptureTheFlag);
        let mut neg = int_option("caps", "1", "-5", "5");
        neg.bind = Some("objectives.capture_limit".into());
        set.options = vec![neg];
        lib.insert(set).expect("insert");
        let err = lib.resolve("neg", &row(&[("caps", "-1")])).expect_err("negative");
        assert_eq!(
            (err.code.as_str(), err.path.as_str()),
            (codes::OPTION_RANGE, "options.caps")
        );
    }

    /// R-04 (5), ADR-8881: a playlist's rules are the combined hash of the distinct rule sets its
    /// rows resolve to. Rows whose mode names no rule set (the level's own rules, a game's rules
    /// name) add nothing; a row key the rule set does not declare is left to the game; a declared
    /// option with a bad value is refused; a row's bound option moves the hash.
    #[test]
    fn playlist_rules_hash_the_rows_and_list_the_refused() {
        let mut library = RuleSetLibrary::new();
        library
            .insert(RuleSet::new("ctf", MatchMode::CaptureTheFlag))
            .expect("ctf");
        let mut set = RuleSet::new("ctf_custom", MatchMode::CaptureTheFlag);
        let mut caps = int_option("caps", "3", "1", "10");
        caps.bind = Some("objectives.capture_limit".into());
        set.options = vec![caps];
        library.insert(set).expect("insert");
        let own = PlaylistEntry::new("levels/a", "");
        let game_rules = PlaylistEntry::new("levels/a", "SiegeRules");
        assert_eq!(
            library.playlist_rules(&[own.clone(), game_rules.clone()]),
            PlaylistRules::default(),
            "rows naming no rule set add nothing and are not refused"
        );
        let ctf = PlaylistEntry::new("levels/a", "ctf");
        let mut ctf_game_option = ctf.clone();
        ctf_game_option.options.insert("limit".into(), "10".into());
        let custom = PlaylistEntry::new("levels/b", "ctf_custom");
        let mut custom_seven = custom.clone();
        custom_seven.options.insert("caps".into(), "7".into());
        let mut custom_bad = custom.clone();
        custom_bad.options.insert("caps".into(), "99".into());
        let rules = library.playlist_rules(&[
            own,
            ctf.clone(),
            custom.clone(),
            custom_bad,
            game_rules,
            ctf_game_option.clone(),
        ]);
        let ctf_rules = library.resolve_row(&ctf).expect("ctf");
        let custom_rules = library.resolve_row(&custom).expect("custom");
        assert_eq!(
            rules.hash,
            Some(combined_hash(&[&ctf_rules, &custom_rules])),
            "a repeated rule set counts once; a game's own row key does not enter the hash"
        );
        assert_eq!(rules.refused.len(), 1);
        assert_eq!(rules.refused[0].0, 3);
        assert_eq!(rules.refused[0].1.code, codes::OPTION_RANGE);
        assert!(
            library
                .resolve_hosted_row(&ctf_game_option)
                .is_some_and(|r| r.is_ok()),
            "an undeclared key is left to the game"
        );
        let seven = library.playlist_rules(&[ctf, custom_seven]);
        assert_ne!(seven.hash, rules.hash, "a row's bound option moves the hash");
    }
}
