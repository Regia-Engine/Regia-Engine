// SPDX-License-Identifier: MPL-2.0
//! `.rules.ron` readers for the shared settings types a rule set embeds (ADR-8799).
//!
//! `TeamDef`, `RoundSettings`, `SpawnWeights`, `TeamRespawn` and the tagged enums `TimeUp`,
//! `TeamSwitch`, `RespawnPolicy` and `ZoneMode` are also read from `MatchSession` snapshots and the
//! agent protocol's `settings`, which keep their lenient serde shape. A rules file must refuse an
//! unknown field anywhere (`docs/formats/rules-ron.md`), so the rule-set patches read those values
//! through the mirrors here: the same wire names and tags, plus `deny_unknown_fields`. Writing still
//! uses the shared types' `Serialize`.
//!
//! Each mirror converts both ways with an exhaustive destructure, so a field or variant added to a
//! shared type stops this file compiling until the mirror has it too. The tests here read every
//! variant the shared types write back through the mirrors, so a rename or tag change made on only
//! one side fails them.
//!
//! A payload-free variant of an internally tagged mirror is an empty struct variant (`End {}`),
//! not a unit variant (ADR-8879). serde's internally tagged unit-variant visitor skips every key
//! after the tag and never reads `deny_unknown_fields`, so `(kind: "end", ticks: 30)` loaded as
//! `End`. An empty struct variant takes the struct visitor, which does refuse the key. The wire
//! text is the same (`(kind: "end")`), and the schema probe reports both forms as `unit`, so the
//! `rules-ron` format hash does not move. [`VictoryConditionFile`] does the same for the rule
//! set's own `VictoryConditionDef`, which keeps its unit variants in Rust.

use serde::{Deserialize, Deserializer};

use super::VictoryConditionDef;
use crate::phases::{RoundSettings, TimeUp};
use crate::spawn::{RespawnPolicy, SpawnWeights, TeamRespawn};
use crate::teams::{TeamDef, TeamSwitch};
use crate::zones::ZoneMode;

/// A mirror that becomes its shared type.
trait Lift<T> {
    fn lift(self) -> T;
}

impl<M: Lift<T>, T> Lift<Option<T>> for Option<M> {
    fn lift(self) -> Option<T> {
        self.map(Lift::lift)
    }
}

impl<M: Lift<T>, T> Lift<Vec<T>> for Vec<M> {
    fn lift(self) -> Vec<T> {
        self.into_iter().map(Lift::lift).collect()
    }
}

/// Reads `M` and lifts it to `T`.
fn via<'de, D: Deserializer<'de>, M: Deserialize<'de> + Lift<T>, T>(d: D) -> Result<T, D::Error> {
    M::deserialize(d).map(Lift::lift)
}

/// `RuleSet::teams`.
pub(super) fn teams<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<TeamDef>>, D::Error> {
    via::<D, Option<Vec<TeamDefFile>>, _>(d)
}

/// `PhasePatch::on_time_up`.
pub(super) fn on_time_up<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TimeUp>, D::Error> {
    via::<D, Option<TimeUpFile>, _>(d)
}

/// `PhasePatch::rounds`.
pub(super) fn rounds<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<RoundSettings>>, D::Error> {
    via::<D, Option<Option<RoundSettingsFile>>, _>(d)
}

/// `TeamsPatch::switch`.
pub(super) fn switch<'de, D: Deserializer<'de>>(d: D) -> Result<Option<TeamSwitch>, D::Error> {
    via::<D, Option<TeamSwitchFile>, _>(d)
}

/// `RespawnPatch::default`.
pub(super) fn respawn_default<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RespawnPolicy>, D::Error> {
    via::<D, Option<RespawnPolicyFile>, _>(d)
}

/// `RespawnPatch::teams`.
pub(super) fn respawn_teams<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<TeamRespawn>>, D::Error> {
    via::<D, Option<Vec<TeamRespawnFile>>, _>(d)
}

/// `ZonesPatch::mode`.
pub(super) fn zone_mode<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ZoneMode>, D::Error> {
    via::<D, Option<ZoneModeFile>, _>(d)
}

/// `SpawnPolicyDef::Scored`.
pub(super) fn spawn_weights<'de, D: Deserializer<'de>>(d: D) -> Result<SpawnWeights, D::Error> {
    via::<D, SpawnWeightsFile, _>(d)
}

/// [`TeamDef`], unknown fields refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TeamDefFile {
    id: u8,
    name_key: String,
    color: [u8; 4],
    max_seats: u8,
}

impl Lift<TeamDef> for TeamDefFile {
    fn lift(self) -> TeamDef {
        let Self {
            id,
            name_key,
            color,
            max_seats,
        } = self;
        TeamDef {
            id,
            name_key,
            color,
            max_seats,
        }
    }
}

impl From<TeamDef> for TeamDefFile {
    fn from(value: TeamDef) -> Self {
        let TeamDef {
            id,
            name_key,
            color,
            max_seats,
        } = value;
        Self {
            id,
            name_key,
            color,
            max_seats,
        }
    }
}

/// [`RoundSettings`], unknown fields refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RoundSettingsFile {
    rounds_to_win: u32,
    max_rounds: u32,
    swap_sides_every: u32,
    round_end_ticks: u64,
}

impl Default for RoundSettingsFile {
    fn default() -> Self {
        RoundSettings::default().into()
    }
}

impl Lift<RoundSettings> for RoundSettingsFile {
    fn lift(self) -> RoundSettings {
        let Self {
            rounds_to_win,
            max_rounds,
            swap_sides_every,
            round_end_ticks,
        } = self;
        RoundSettings {
            rounds_to_win,
            max_rounds,
            swap_sides_every,
            round_end_ticks,
        }
    }
}

impl From<RoundSettings> for RoundSettingsFile {
    fn from(value: RoundSettings) -> Self {
        let RoundSettings {
            rounds_to_win,
            max_rounds,
            swap_sides_every,
            round_end_ticks,
        } = value;
        Self {
            rounds_to_win,
            max_rounds,
            swap_sides_every,
            round_end_ticks,
        }
    }
}

/// [`SpawnWeights`], unknown fields refused.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SpawnWeightsFile {
    enemy: i64,
    enemy_cap: f32,
    los: i64,
    los_range: f32,
    team: i64,
    team_radius: f32,
    recent: i64,
    recent_ticks: u64,
    priority: i64,
}

impl Default for SpawnWeightsFile {
    fn default() -> Self {
        SpawnWeights::default().into()
    }
}

impl Lift<SpawnWeights> for SpawnWeightsFile {
    fn lift(self) -> SpawnWeights {
        let Self {
            enemy,
            enemy_cap,
            los,
            los_range,
            team,
            team_radius,
            recent,
            recent_ticks,
            priority,
        } = self;
        SpawnWeights {
            enemy,
            enemy_cap,
            los,
            los_range,
            team,
            team_radius,
            recent,
            recent_ticks,
            priority,
        }
    }
}

impl From<SpawnWeights> for SpawnWeightsFile {
    fn from(value: SpawnWeights) -> Self {
        let SpawnWeights {
            enemy,
            enemy_cap,
            los,
            los_range,
            team,
            team_radius,
            recent,
            recent_ticks,
            priority,
        } = value;
        Self {
            enemy,
            enemy_cap,
            los,
            los_range,
            team,
            team_radius,
            recent,
            recent_ticks,
            priority,
        }
    }
}

/// [`TimeUp`], unknown fields refused (on the payload-free variants too: they are `{}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TimeUpFile {
    End {},
    Draw {},
    Overtime {
        #[serde(default)]
        ticks: Option<u64>,
    },
    SuddenDeath {},
}

impl Lift<TimeUp> for TimeUpFile {
    fn lift(self) -> TimeUp {
        match self {
            Self::End {} => TimeUp::End,
            Self::Draw {} => TimeUp::Draw,
            Self::Overtime { ticks } => TimeUp::Overtime { ticks },
            Self::SuddenDeath {} => TimeUp::SuddenDeath,
        }
    }
}

impl From<TimeUp> for TimeUpFile {
    fn from(value: TimeUp) -> Self {
        match value {
            TimeUp::End => Self::End {},
            TimeUp::Draw => Self::Draw {},
            TimeUp::Overtime { ticks } => Self::Overtime { ticks },
            TimeUp::SuddenDeath => Self::SuddenDeath {},
        }
    }
}

/// [`TeamSwitch`], unknown keys refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "policy", content = "n", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TeamSwitchFile {
    Never,
    BetweenMatches,
    WhenUnbalancedBy(u8),
}

impl Lift<TeamSwitch> for TeamSwitchFile {
    fn lift(self) -> TeamSwitch {
        match self {
            Self::Never => TeamSwitch::Never,
            Self::BetweenMatches => TeamSwitch::BetweenMatches,
            Self::WhenUnbalancedBy(n) => TeamSwitch::WhenUnbalancedBy(n),
        }
    }
}

impl From<TeamSwitch> for TeamSwitchFile {
    fn from(value: TeamSwitch) -> Self {
        match value {
            TeamSwitch::Never => Self::Never,
            TeamSwitch::BetweenMatches => Self::BetweenMatches,
            TeamSwitch::WhenUnbalancedBy(n) => Self::WhenUnbalancedBy(n),
        }
    }
}

/// [`RespawnPolicy`], unknown fields refused (on the payload-free variants too: they are `{}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RespawnPolicyFile {
    Instant {},
    Delay { ticks: u64 },
    Waves { period: u64 },
    Lives { lives: u32 },
    Never {},
}

impl Lift<RespawnPolicy> for RespawnPolicyFile {
    fn lift(self) -> RespawnPolicy {
        match self {
            Self::Instant {} => RespawnPolicy::Instant,
            Self::Delay { ticks } => RespawnPolicy::Delay { ticks },
            Self::Waves { period } => RespawnPolicy::Waves { period },
            Self::Lives { lives } => RespawnPolicy::Lives { lives },
            Self::Never {} => RespawnPolicy::Never,
        }
    }
}

impl From<RespawnPolicy> for RespawnPolicyFile {
    fn from(value: RespawnPolicy) -> Self {
        match value {
            RespawnPolicy::Instant => Self::Instant {},
            RespawnPolicy::Delay { ticks } => Self::Delay { ticks },
            RespawnPolicy::Waves { period } => Self::Waves { period },
            RespawnPolicy::Lives { lives } => Self::Lives { lives },
            RespawnPolicy::Never => Self::Never {},
        }
    }
}

/// [`TeamRespawn`], unknown fields refused (its policy too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TeamRespawnFile {
    team: u8,
    policy: RespawnPolicyFile,
}

impl Lift<TeamRespawn> for TeamRespawnFile {
    fn lift(self) -> TeamRespawn {
        let Self { team, policy } = self;
        TeamRespawn {
            team,
            policy: policy.lift(),
        }
    }
}

impl From<TeamRespawn> for TeamRespawnFile {
    fn from(value: TeamRespawn) -> Self {
        let TeamRespawn { team, policy } = value;
        Self {
            team,
            policy: policy.into(),
        }
    }
}

/// [`ZoneMode`], unknown fields refused (on the payload-free variant too: it is `{}`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ZoneModeFile {
    Domination {},
    Hardpoint {
        rotation: Vec<u32>,
        rotate_ticks: u64,
    },
    AttackDefend {
        attackers: u8,
        defenders: u8,
        order: Vec<u32>,
    },
}

impl Lift<ZoneMode> for ZoneModeFile {
    fn lift(self) -> ZoneMode {
        match self {
            Self::Domination {} => ZoneMode::Domination,
            Self::Hardpoint {
                rotation,
                rotate_ticks,
            } => ZoneMode::Hardpoint {
                rotation,
                rotate_ticks,
            },
            Self::AttackDefend {
                attackers,
                defenders,
                order,
            } => ZoneMode::AttackDefend {
                attackers,
                defenders,
                order,
            },
        }
    }
}

impl From<ZoneMode> for ZoneModeFile {
    fn from(value: ZoneMode) -> Self {
        match value {
            ZoneMode::Domination => Self::Domination {},
            ZoneMode::Hardpoint {
                rotation,
                rotate_ticks,
            } => Self::Hardpoint {
                rotation,
                rotate_ticks,
            },
            ZoneMode::AttackDefend {
                attackers,
                defenders,
                order,
            } => Self::AttackDefend {
                attackers,
                defenders,
                order,
            },
        }
    }
}

/// What `VictoryConditionDef` reads from a file: the same tag and wire names, with the
/// payload-free variants as empty struct variants so an unknown field beside them is refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum VictoryConditionFile {
    DestroyAllTagged { tag: String },
    HoldFor { objective: u32, ticks: u64 },
    ScoreThreshold { score: i64 },
    TeamElimination {},
    Surrender {},
}

impl From<VictoryConditionFile> for VictoryConditionDef {
    fn from(value: VictoryConditionFile) -> Self {
        match value {
            VictoryConditionFile::DestroyAllTagged { tag } => Self::DestroyAllTagged { tag },
            VictoryConditionFile::HoldFor { objective, ticks } => Self::HoldFor { objective, ticks },
            VictoryConditionFile::ScoreThreshold { score } => Self::ScoreThreshold { score },
            VictoryConditionFile::TeamElimination {} => Self::TeamElimination,
            VictoryConditionFile::Surrender {} => Self::Surrender,
        }
    }
}

impl From<VictoryConditionDef> for VictoryConditionFile {
    fn from(value: VictoryConditionDef) -> Self {
        match value {
            VictoryConditionDef::DestroyAllTagged { tag } => Self::DestroyAllTagged { tag },
            VictoryConditionDef::HoldFor { objective, ticks } => Self::HoldFor { objective, ticks },
            VictoryConditionDef::ScoreThreshold { score } => Self::ScoreThreshold { score },
            VictoryConditionDef::TeamElimination => Self::TeamElimination {},
            VictoryConditionDef::Surrender => Self::Surrender {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `value` with the shared type's `Serialize`, reads it back through the mirror, and
    /// round-trips the mirror conversion.
    fn back<S, M>(value: S) -> S
    where
        S: serde::Serialize + serde::de::DeserializeOwned + Clone + PartialEq + std::fmt::Debug,
        M: serde::de::DeserializeOwned + Lift<S> + From<S>,
    {
        let text = serde_json::to_string(&value).expect("serialize");
        let read: M = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(read.lift(), value, "{text}");
        M::from(value.clone()).lift()
    }

    /// The same text read by the shared type and by its mirror gives the same value (the defaults
    /// of absent fields agree).
    fn same<S, M>(text: &str)
    where
        S: serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        M: serde::de::DeserializeOwned + Lift<S>,
    {
        let shared: S = serde_json::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        let mirror: M = serde_json::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(mirror.lift(), shared, "{text}");
    }

    /// Every variant and field the shared types write reads back through the mirrors (the field
    /// sets are equal by construction: the conversions destructure exhaustively).
    #[test]
    fn shared_writes_read_back_through_the_mirrors() {
        let team = TeamDef {
            id: 2,
            name_key: "team.blue".into(),
            color: [1, 2, 3, 4],
            max_seats: 5,
        };
        assert_eq!(back::<_, TeamDefFile>(team.clone()), team);
        let rounds = RoundSettings {
            rounds_to_win: 4,
            max_rounds: 7,
            swap_sides_every: 2,
            round_end_ticks: 90,
        };
        assert_eq!(back::<_, RoundSettingsFile>(rounds), rounds);
        let weights = SpawnWeights {
            enemy: -3,
            enemy_cap: 12.5,
            los: 7,
            los_range: 30.0,
            team: 9,
            team_radius: 4.5,
            recent: 11,
            recent_ticks: 120,
            priority: 2,
        };
        assert_eq!(back::<_, SpawnWeightsFile>(weights), weights);
        for value in [
            TimeUp::End,
            TimeUp::Draw,
            TimeUp::Overtime { ticks: Some(30) },
            TimeUp::Overtime { ticks: None },
            TimeUp::SuddenDeath,
        ] {
            assert_eq!(back::<_, TimeUpFile>(value), value);
        }
        for value in [
            TeamSwitch::Never,
            TeamSwitch::BetweenMatches,
            TeamSwitch::WhenUnbalancedBy(2),
        ] {
            assert_eq!(back::<_, TeamSwitchFile>(value), value);
        }
        for value in [
            RespawnPolicy::Instant,
            RespawnPolicy::Delay { ticks: 10 },
            RespawnPolicy::Waves { period: 20 },
            RespawnPolicy::Lives { lives: 3 },
            RespawnPolicy::Never,
        ] {
            assert_eq!(back::<_, RespawnPolicyFile>(value), value);
            let row = TeamRespawn {
                team: 1,
                policy: value,
            };
            assert_eq!(back::<_, TeamRespawnFile>(row), row);
        }
        for value in [
            ZoneMode::Domination,
            ZoneMode::Hardpoint {
                rotation: vec![3, 1],
                rotate_ticks: 600,
            },
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![5, 6],
            },
        ] {
            assert_eq!(back::<_, ZoneModeFile>(value.clone()), value);
        }
        for value in [
            VictoryConditionDef::DestroyAllTagged { tag: "hq".into() },
            VictoryConditionDef::HoldFor {
                objective: 3,
                ticks: 6000,
            },
            VictoryConditionDef::ScoreThreshold { score: -4 },
            VictoryConditionDef::TeamElimination,
            VictoryConditionDef::Surrender,
        ] {
            let text = serde_json::to_string(&value).expect("serialize");
            let read: VictoryConditionFile =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(VictoryConditionDef::from(read), value, "{text}");
            assert_eq!(
                VictoryConditionDef::from(VictoryConditionFile::from(value.clone())),
                value
            );
            let def: VictoryConditionDef =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(def, value, "{text}");
        }
    }

    /// The payload-free variants still read from the bare tag, the shape every shipped file and
    /// every `Serialize` of the shared type writes (`{"kind":"end"}`; the RON form is covered by
    /// `regia_framework`'s shipped-defaults test).
    #[test]
    fn payload_free_variants_read_from_the_bare_tag() {
        same::<TimeUp, TimeUpFile>(r#"{"kind":"end"}"#);
        same::<TimeUp, TimeUpFile>(r#"{"kind":"draw"}"#);
        same::<TimeUp, TimeUpFile>(r#"{"kind":"sudden_death"}"#);
        same::<RespawnPolicy, RespawnPolicyFile>(r#"{"kind":"instant"}"#);
        same::<RespawnPolicy, RespawnPolicyFile>(r#"{"kind":"never"}"#);
        same::<ZoneMode, ZoneModeFile>(r#"{"kind":"domination"}"#);
        for (text, want) in [
            (
                r#"{"kind":"team_elimination"}"#,
                VictoryConditionDef::TeamElimination,
            ),
            (r#"{"kind":"surrender"}"#, VictoryConditionDef::Surrender),
        ] {
            let read: VictoryConditionDef =
                serde_json::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(read, want, "{text}");
        }
    }

    /// Absent fields default the same way in the mirror and the shared type.
    #[test]
    fn mirror_defaults_are_the_shared_defaults() {
        assert_eq!(RoundSettingsFile::default().lift(), RoundSettings::default());
        assert_eq!(SpawnWeightsFile::default().lift(), SpawnWeights::default());
        same::<RoundSettings, RoundSettingsFile>("{}");
        same::<RoundSettings, RoundSettingsFile>(r#"{"max_rounds":9}"#);
        same::<SpawnWeights, SpawnWeightsFile>("{}");
        same::<SpawnWeights, SpawnWeightsFile>(r#"{"los":1}"#);
        same::<TimeUp, TimeUpFile>(r#"{"kind":"overtime"}"#);
        same::<TeamSwitch, TeamSwitchFile>(r#"{"policy":"never"}"#);
    }

    /// The mirrors refuse what the shared types skip over.
    #[test]
    fn mirrors_refuse_unknown_fields() {
        fn refused<M: serde::de::DeserializeOwned + std::fmt::Debug>(text: &str) {
            let err = serde_json::from_str::<M>(text).expect_err(text);
            assert!(err.to_string().contains("extra"), "{text}: {err}");
        }
        refused::<TeamDefFile>(r#"{"id":1,"name_key":"a","color":[1,2,3,4],"max_seats":4,"extra":1}"#);
        refused::<RoundSettingsFile>(r#"{"extra":1}"#);
        refused::<SpawnWeightsFile>(r#"{"enemy":1,"extra":1}"#);
        refused::<TeamRespawnFile>(r#"{"team":1,"policy":{"kind":"instant"},"extra":1}"#);
        refused::<TeamRespawnFile>(r#"{"team":1,"policy":{"kind":"delay","ticks":1,"extra":1}}"#);
        refused::<TimeUpFile>(r#"{"kind":"overtime","ticks":1,"extra":1}"#);
        refused::<RespawnPolicyFile>(r#"{"kind":"waves","period":1,"extra":1}"#);
        refused::<ZoneModeFile>(
            r#"{"kind":"attack_defend","attackers":1,"defenders":2,"order":[],"extra":1}"#,
        );
        refused::<TeamSwitchFile>(r#"{"policy":"between_matches","extra":1}"#);
        // The payload-free variants (ADR-8879): a unit variant here would skip the key.
        refused::<TimeUpFile>(r#"{"kind":"end","extra":1}"#);
        refused::<TimeUpFile>(r#"{"kind":"draw","extra":1}"#);
        refused::<TimeUpFile>(r#"{"kind":"sudden_death","extra":1}"#);
        refused::<RespawnPolicyFile>(r#"{"kind":"instant","extra":1}"#);
        refused::<RespawnPolicyFile>(r#"{"kind":"never","extra":1}"#);
        refused::<TeamRespawnFile>(r#"{"team":1,"policy":{"kind":"never","extra":1}}"#);
        refused::<ZoneModeFile>(r#"{"kind":"domination","extra":1}"#);
        refused::<TeamSwitchFile>(r#"{"policy":"never","extra":1}"#);
        refused::<VictoryConditionDef>(r#"{"kind":"team_elimination","extra":1}"#);
        refused::<VictoryConditionDef>(r#"{"kind":"surrender","extra":1}"#);
        refused::<VictoryConditionDef>(r#"{"kind":"hold_for","objective":1,"ticks":2,"extra":1}"#);
        // An extra key in a nested payload-free value is still named.
        refused::<TeamRespawnFile>(r#"{"team":1,"policy":{"kind":"instant","extra":1}}"#);
        // The shared types keep reading them (snapshots, the agent protocol's `settings`).
        let lenient: TeamDef =
            serde_json::from_str(r#"{"id":1,"name_key":"a","color":[1,2,3,4],"max_seats":4,"extra":1}"#)
                .expect("shared type stays lenient");
        assert_eq!(lenient.id, 1);
    }
}
