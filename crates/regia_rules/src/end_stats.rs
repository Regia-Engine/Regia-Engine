// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/end_stats.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: match length is formatted at the ledger's tick_hz (default 60), not a
// constant 60; the first-kill field is first_kill and the kind is FirstKill; a primary structure
// loss is STRUCTURE LOST and a producer loss is PRODUCER LOST plus the stem, with no unit name in
// the API; gathered and spent replace the scrap columns; is_ai replaces the AI-seat flag; teams
// outside 1..=max_teams (default 4) never open the ledger; the sim's begin_end_stats is not copied.

//! End-of-match stats the match-end screen shows.
//!
//! The ledger records the match start, the over tick, the first kill and structure losses.
//! The summary math is pure: match length, per-player and per-team totals, and the MVP.
//! MVP order is most kills, then fewest deaths, then fewest ships lost, then the lowest team,
//! then the name. An AI seat is skipped while any player exists.

use serde::{Deserialize, Serialize};

/// Most timeline moments kept per match.
pub const MAX_TIMELINE_EVENTS: usize = 8;

/// The default fighting-team cap. Teams above it never open the ledger.
pub const DEFAULT_MAX_TEAMS: u8 = 4;

/// A key moment of the match, in tick order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TimelineKind {
    /// The first kill of the match.
    FirstKill = 0,
    /// A producer structure destroyed.
    ProducerLost = 1,
    /// A primary structure destroyed. It does not also record a producer loss.
    StructureLost = 2,
}

impl TimelineKind {
    /// The kind for a stored byte.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::FirstKill),
            1 => Some(Self::ProducerLost),
            2 => Some(Self::StructureLost),
            _ => None,
        }
    }

    /// A stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstKill => "first_kill",
            Self::ProducerLost => "producer_lost",
            Self::StructureLost => "structure_lost",
        }
    }

    /// Screen label.
    pub fn label(self) -> &'static str {
        match self {
            Self::FirstKill => "FIRST KILL",
            Self::ProducerLost => "PRODUCER LOST",
            Self::StructureLost => "STRUCTURE LOST",
        }
    }
}

/// One key moment. `tick` is the absolute sim tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// Sim tick of the moment.
    pub tick: u64,
    /// What happened.
    pub kind: TimelineKind,
    /// Team on the receiving end.
    pub team: u8,
    /// The other side, for a kill. 0 when there is none.
    pub foe_team: u8,
    /// Stem lost, for a producer moment. Empty otherwise.
    #[serde(default)]
    pub stem: String,
}

impl TimelineEvent {
    /// One timeline line. `tick_hz` is the match clock; 0 uses 60.
    pub fn line(&self, start_tick: u64, tick_hz: u32) -> String {
        let stamp = format_match_length(self.tick.saturating_sub(start_tick), tick_hz);
        let team = team_tag(self.team);
        match self.kind {
            TimelineKind::FirstKill => {
                let foe = team_tag(self.foe_team);
                format!("{stamp}  FIRST KILL  {team} OVER {foe}")
            }
            TimelineKind::ProducerLost => {
                let stem = self.stem.to_ascii_uppercase();
                format!("{stamp}  PRODUCER LOST  {team} {stem}")
            }
            TimelineKind::StructureLost => {
                format!("{stamp}  STRUCTURE LOST  {team}")
            }
        }
    }
}

/// Match tag on the timeline. Teams other than 1..=4 read as `--`.
fn team_tag(team: u8) -> &'static str {
    match team {
        1 => "T1",
        2 => "T2",
        3 => "T3",
        4 => "T4",
        _ => "--",
    }
}

fn default_tick_hz() -> u32 {
    regia_detsim::tick::TICK_RATE_HZ
}

fn default_max_teams() -> u8 {
    DEFAULT_MAX_TEAMS
}

/// The deterministic per-match ledger. One per match; [`MatchEndLedger::begin`] resets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchEndLedger {
    /// Sim tick the current match started on.
    #[serde(default)]
    pub start_tick: u64,
    /// Sim tick the match ended on, once it has.
    #[serde(default)]
    pub over_tick: Option<u64>,
    /// The first kill. Also the first timeline entry of that kind. `None` until one lands.
    #[serde(default)]
    pub first_kill: Option<TimelineEvent>,
    /// Key moments in tick order. Capped at [`MAX_TIMELINE_EVENTS`].
    #[serde(default)]
    pub timeline: Vec<TimelineEvent>,
    /// Tick rate used to format the length. The default is 60.
    #[serde(default = "default_tick_hz")]
    pub tick_hz: u32,
    /// Highest fighting team that may open the ledger. The default is 4.
    #[serde(default = "default_max_teams")]
    pub max_teams: u8,
}

impl Default for MatchEndLedger {
    fn default() -> Self {
        Self {
            start_tick: 0,
            over_tick: None,
            first_kill: None,
            timeline: Vec::new(),
            tick_hz: regia_detsim::tick::TICK_RATE_HZ,
            max_teams: DEFAULT_MAX_TEAMS,
        }
    }
}

impl MatchEndLedger {
    /// A new match starts on `tick`. Clears the previous match's moments. Keeps `tick_hz` and
    /// `max_teams`.
    pub fn begin(&mut self, tick: u64) {
        self.start_tick = tick;
        self.over_tick = None;
        self.first_kill = None;
        self.timeline.clear();
    }

    /// Record the tick the match ended on.
    pub fn note_over(&mut self, tick: u64) {
        self.over_tick = Some(tick);
    }

    /// A kill by `killer_team` on `victim_team`. Only the first one of the match is kept.
    /// Teams outside 1..=[`Self::max_teams`] never open the ledger.
    pub fn note_kill(&mut self, tick: u64, killer_team: u8, victim_team: u8) {
        if self.first_kill.is_some() {
            return;
        }
        if !self.team_ok(killer_team) || !self.team_ok(victim_team) {
            return;
        }
        let event = TimelineEvent {
            tick,
            kind: TimelineKind::FirstKill,
            team: victim_team,
            foe_team: killer_team,
            stem: String::new(),
        };
        self.first_kill = Some(event.clone());
        self.push(event);
    }

    /// A structure destroyed on `team`. A primary structure records only [`TimelineKind::StructureLost`].
    /// A producer that is not primary records [`TimelineKind::ProducerLost`]. Anything else is ignored.
    pub fn note_structure(&mut self, tick: u64, team: u8, stem: &str, is_producer: bool, is_primary: bool) {
        if !self.team_ok(team) {
            return;
        }
        let kind = if is_primary {
            TimelineKind::StructureLost
        } else if is_producer {
            TimelineKind::ProducerLost
        } else {
            return;
        };
        self.push(TimelineEvent {
            tick,
            kind,
            team,
            foe_team: 0,
            stem: if kind == TimelineKind::ProducerLost {
                stem.to_string()
            } else {
                String::new()
            },
        });
    }

    fn team_ok(&self, team: u8) -> bool {
        team >= 1 && team <= self.max_teams
    }

    fn push(&mut self, event: TimelineEvent) {
        if self.timeline.len() >= MAX_TIMELINE_EVENTS {
            return;
        }
        self.timeline.push(event);
    }

    /// Ticks of match played: the over tick, or `now_tick` while running, minus the start.
    /// Saturates instead of wrapping.
    pub fn match_length_ticks(&self, now_tick: u64, over_tick: Option<u64>) -> u64 {
        over_tick.unwrap_or(now_tick).saturating_sub(self.start_tick)
    }

    /// Length using the ledger's over tick when `now_tick` is the running clock.
    pub fn length_ticks(&self, now_tick: u64) -> u64 {
        self.match_length_ticks(now_tick, self.over_tick)
    }

    /// The short timeline, oldest first, stamped at [`Self::tick_hz`].
    pub fn timeline_lines(&self) -> Vec<String> {
        self.timeline
            .iter()
            .map(|event| event.line(self.start_tick, self.tick_hz))
            .collect()
    }
}

/// Match length on the end screen. `tick_hz` is the match clock (0 means 60).
/// `M:SS` under an hour, `H:MM:SS` past it.
pub fn format_match_length(ticks: u64, tick_hz: u32) -> String {
    let hz = if tick_hz == 0 {
        u64::from(regia_detsim::tick::TICK_RATE_HZ)
    } else {
        u64::from(tick_hz)
    };
    let secs = ticks / hz;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rest = secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{rest:02}")
    } else {
        format!("{mins}:{rest:02}")
    }
}

/// One player's end-of-match stats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerEndStats {
    /// Fighting team.
    pub team: u8,
    /// Display name.
    pub name: String,
    /// Kills.
    pub kills: u32,
    /// Deaths.
    pub deaths: u32,
    /// Hulls lost.
    pub ships_lost: u32,
    /// Opaque gathered total. The game decides what it counts.
    pub gathered: u32,
    /// Opaque spent total.
    pub spent: u32,
    /// Units made.
    pub units_made: u32,
    /// Buildings made.
    pub buildings_made: u32,
    /// An AI-run seat. Never the MVP while a player exists.
    pub is_ai: bool,
}

/// One team's end-of-match totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamEndStats {
    /// The team.
    pub team: u8,
    /// Player seats. AI seats are not counted here. Their other columns still sum.
    pub players: u32,
    /// Kills.
    pub kills: u32,
    /// Deaths.
    pub deaths: u32,
    /// Hulls lost.
    pub ships_lost: u32,
    /// Gathered total.
    pub gathered: u32,
    /// Spent total.
    pub spent: u32,
    /// Units made.
    pub units_made: u32,
    /// Buildings made.
    pub buildings_made: u32,
}

impl TeamEndStats {
    /// One footer line.
    pub fn line(&self) -> String {
        format!(
            "T{team}  {players} PLAYERS  {kills} K  {deaths} D  {ships} SHIPS  {gathered} GATHERED +{spent} SPENT {units} UNITS {buildings} BLDG",
            team = self.team,
            players = self.players,
            kills = self.kills,
            deaths = self.deaths,
            ships = self.ships_lost,
            gathered = self.gathered,
            spent = self.spent,
            units = self.units_made,
            buildings = self.buildings_made
        )
    }
}

/// Per-team totals in team order. Teams with no rows are skipped. Teams outside 1..=4 are skipped.
pub fn aggregate_players(players: &[PlayerEndStats]) -> Vec<TeamEndStats> {
    let mut out = Vec::new();
    for team in 1..=DEFAULT_MAX_TEAMS {
        let mut mine = Vec::new();
        for row in players {
            if row.team == team {
                mine.push(row);
            }
        }
        if mine.is_empty() {
            continue;
        }
        let mut players_n = 0u32;
        let mut kills = 0u32;
        let mut deaths = 0u32;
        let mut ships_lost = 0u32;
        let mut gathered = 0u32;
        let mut spent = 0u32;
        let mut units_made = 0u32;
        let mut buildings_made = 0u32;
        for row in &mine {
            if !row.is_ai {
                players_n = players_n.saturating_add(1);
            }
            kills = kills.saturating_add(row.kills);
            deaths = deaths.saturating_add(row.deaths);
            ships_lost = ships_lost.saturating_add(row.ships_lost);
            gathered = gathered.saturating_add(row.gathered);
            spent = spent.saturating_add(row.spent);
            units_made = units_made.saturating_add(row.units_made);
            buildings_made = buildings_made.saturating_add(row.buildings_made);
        }
        out.push(TeamEndStats {
            team,
            players: players_n,
            kills,
            deaths,
            ships_lost,
            gathered,
            spent,
            units_made,
            buildings_made,
        });
    }
    out
}

/// Index of the MVP. AI seats are skipped while a player exists. An all-AI board still names one.
pub fn mvp_index(players: &[PlayerEndStats]) -> Option<usize> {
    let mut pool = Vec::new();
    for (index, row) in players.iter().enumerate() {
        if !row.is_ai {
            pool.push(index);
        }
    }
    if pool.is_empty() {
        pool = (0..players.len()).collect();
    }
    pool.into_iter().min_by(|&a, &b| {
        let ra = &players[a];
        let rb = &players[b];
        rb.kills
            .cmp(&ra.kills)
            .then(ra.deaths.cmp(&rb.deaths))
            .then(ra.ships_lost.cmp(&rb.ships_lost))
            .then(ra.team.cmp(&rb.team))
            .then(ra.name.cmp(&rb.name))
    })
}

/// The MVP line: `MVP  ADA (T1)  7 KILLS`.
pub fn mvp_line(mvp: &PlayerEndStats) -> String {
    let name = mvp.name.to_ascii_uppercase();
    let team = mvp.team;
    let kills = mvp.kills;
    format!("MVP  {name} (T{team})  {kills} KILLS")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(team: u8, name: &str, kills: u32, deaths: u32, ai: bool) -> PlayerEndStats {
        PlayerEndStats {
            team,
            name: name.to_string(),
            kills,
            deaths,
            ships_lost: 1,
            gathered: 4,
            spent: 3,
            units_made: 2,
            buildings_made: 1,
            is_ai: ai,
        }
    }

    #[test]
    fn length_is_minutes_and_seconds_at_sixty_hz() {
        assert_eq!(format_match_length(0, 60), "0:00");
        assert_eq!(format_match_length(60, 60), "0:01");
        assert_eq!(format_match_length(60 * 75, 60), "1:15");
        assert_eq!(format_match_length(60 * 3599 + 59 * 60, 60), "1:00:58");
        assert_eq!(format_match_length(60 * 3600, 60), "1:00:00");
        assert_eq!(format_match_length(60 * 3723, 60), "1:02:03");
        assert_eq!(format_match_length(30, 30), "0:01", "the tick rate is a setting");
    }

    #[test]
    fn teams_sum_every_column_in_team_order() {
        let rows = vec![
            player(2, "Rex", 4, 2, false),
            player(1, "Ada", 5, 1, false),
            player(1, "Gib", 3, 0, true),
        ];
        let teams = aggregate_players(&rows);
        assert_eq!(teams.len(), 2);
        assert_eq!(teams[0].team, 1);
        assert_eq!(teams[0].players, 1, "the AI seat is not a player");
        assert_eq!(teams[0].kills, 8, "AI kills count for the team");
        assert_eq!(teams[0].deaths, 1);
        assert_eq!(teams[0].ships_lost, 2);
        assert_eq!(teams[0].gathered, 8);
        assert_eq!(teams[0].spent, 6);
        assert_eq!(teams[0].units_made, 4);
        assert_eq!(teams[0].buildings_made, 2);
        assert_eq!(teams[1].team, 2);
        assert_eq!(teams[1].kills, 4);
    }

    #[test]
    fn mvp_prefers_kills_then_fewest_deaths_then_lowest_team() {
        let rows = vec![
            player(2, "Rex", 5, 0, false),
            player(1, "Ada", 5, 1, false),
            player(1, "Zed", 7, 9, false),
        ];
        assert_eq!(mvp_index(&rows), Some(2), "most kills wins");
        let rows = vec![player(2, "Rex", 5, 0, false), player(1, "Ada", 5, 1, false)];
        assert_eq!(mvp_index(&rows), Some(0), "tied kills: fewer deaths wins");
        let rows = vec![player(2, "Rex", 5, 0, false), player(1, "Ada", 5, 0, false)];
        assert_eq!(mvp_index(&rows), Some(1), "fully tied: lower team wins");
        assert!(mvp_index(&[]).is_none());
    }

    #[test]
    fn mvp_skips_ai_seats_while_a_player_exists() {
        let rows = vec![player(1, "Gib", 9, 0, true), player(1, "Ada", 1, 6, false)];
        assert_eq!(mvp_index(&rows), Some(1));
        let solo = vec![player(1, "Gib", 9, 0, true)];
        assert_eq!(mvp_index(&solo), Some(0), "an all-AI board still names one");
    }

    #[test]
    fn mvp_line_names_the_player() {
        let row = player(1, "Ada", 7, 2, false);
        assert_eq!(mvp_line(&row), "MVP  ADA (T1)  7 KILLS");
    }

    #[test]
    fn first_kill_is_kept_once_and_stamps_from_the_start() {
        let mut ledger = MatchEndLedger::default();
        ledger.begin(100);
        ledger.note_kill(160, 1, 2);
        ledger.note_kill(200, 2, 1);
        assert_eq!(ledger.timeline.len(), 1);
        let first = ledger.first_kill.as_ref().expect("first kill");
        assert_eq!(first.foe_team, 1);
        assert_eq!(ledger.timeline_lines(), vec!["0:01  FIRST KILL  T2 OVER T1"]);
    }

    #[test]
    fn a_primary_structure_loss_is_one_moment_never_two() {
        let mut ledger = MatchEndLedger::default();
        ledger.begin(0);
        ledger.note_structure(600, 2, "forge", true, true);
        ledger.note_structure(900, 1, "forge", true, false);
        ledger.note_structure(1000, 1, "silo", false, false);
        assert_eq!(ledger.timeline.len(), 2);
        assert_eq!(
            ledger.timeline_lines(),
            vec![
                "0:10  STRUCTURE LOST  T2".to_string(),
                "0:15  PRODUCER LOST  T1 FORGE".to_string()
            ]
        );
    }

    #[test]
    fn the_timeline_is_capped_and_the_length_saturates() {
        let mut ledger = MatchEndLedger::default();
        ledger.begin(50);
        for tick in 0..32 {
            ledger.note_structure(100 + tick, 1, "forge", true, false);
        }
        assert_eq!(ledger.timeline.len(), MAX_TIMELINE_EVENTS);
        assert_eq!(ledger.match_length_ticks(9999, None), 9949);
        assert_eq!(ledger.match_length_ticks(9999, Some(5000)), 4950);
        assert_eq!(
            ledger.match_length_ticks(10, Some(5)),
            0,
            "a stale start saturates"
        );
    }

    #[test]
    fn a_new_match_clears_the_old_moments() {
        let mut ledger = MatchEndLedger::default();
        ledger.begin(0);
        ledger.note_kill(60, 1, 2);
        ledger.note_over(80);
        ledger.begin(5000);
        assert!(ledger.first_kill.is_none());
        assert!(ledger.timeline.is_empty());
        assert!(ledger.over_tick.is_none());
        assert_eq!(ledger.match_length_ticks(5060, None), 60);
    }

    #[test]
    fn unknown_teams_never_open_the_ledger() {
        let mut ledger = MatchEndLedger::default();
        ledger.begin(0);
        ledger.note_kill(60, 0, 2);
        ledger.note_structure(60, 9, "forge", true, false);
        assert!(ledger.timeline.is_empty());
    }
}
