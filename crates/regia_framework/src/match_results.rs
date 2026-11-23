// SPDX-License-Identifier: MPL-2.0
//! Finished-match records (`match-result` v1, `docs/formats/match-result.md`,
//! `book/agents/match-results.md`).
//!
//! When the hosted [`MatchSession`] ends (its [`MatchSession::match_end`] is set),
//! [`MatchResultsPlugin`] builds one [`MatchResult`]: the match id, the ruleset id and content hash,
//! the map, the start and end step and tick, the end reason, the winner (or a draw), one row per team
//! and one per seated player, the end-of-match ledger and the replay file name when one was recorded.
//!
//! - **Sinks.** The record goes through the ordered [`MatchResultSinks`] first: the built-in
//!   [`LeaderboardSink`] submits configured boards from result fields, `regia_steam`'s platform stats
//!   sink maps counters to platform stats, and a game adds its own (a web upload is a game's sink,
//!   never the engine's). A sink that fails logs `match_result_sink_failed`; the others still run.
//! - **File.** With [`MatchResultsSettings::root`] set, the host writes
//!   `<root>/<utc-date>/<match_id>.json` on the IO task pool, never on the tick. The write is atomic
//!   (a temp file of its own, fsync, rename, directory fsync: [`write_atomic`]), so a crash leaves a
//!   complete file or none. A failed write logs `match_result_write_failed` and is tried once more;
//!   a second failure drops it (`match_result_dropped`). After each write the oldest records beyond
//!   [`MatchResultsSettings::keep_count`] and the date folders older than
//!   [`MatchResultsSettings::keep_days`] are removed.
//! - **Privacy (ids only, ADR-8880).** A record holds no IP address and no display name: players are
//!   named by key only. A player key derived from a name (`name:<n>`, the `platform_or_name`
//!   identity rule) is written as a hash of the name instead. Names are an owner question (rules
//!   Q4); a record with a `name` on a player row is refused.
//! - **Determinism.** The record's content (everything but the date folder) comes from the session
//!   and the [`MatchPlayerKeys`] only, never the wall clock, so every lockstep peer that writes one
//!   writes the same bytes when they share the match id ([`MatchIdSource`]).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::tasks::Task;
use serde::{Deserialize, Serialize};

use crate::leaderboard::Leaderboard;
use crate::rules::MatchRules;
use crate::rules::end_stats::TimelineEvent;
use crate::rules::match_flow::SeatId;
use crate::rules::objective::EndReason;
use crate::rules::session::MatchSession;
use crate::save_game::write_atomic;

/// The `format` string every record carries.
pub const MATCH_RESULT_FORMAT: &str = "match-result";
/// The `version` this build writes and reads.
pub const MATCH_RESULT_FORMAT_VERSION: u32 = 1;
/// Largest record file read back (a crafted folder cannot make a read allocate more).
pub const MAX_RESULT_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Most team rows one record holds.
pub const MAX_RESULT_TEAMS: usize = 256;
/// Most player rows one record holds.
pub const MAX_RESULT_PLAYERS: usize = 1024;
/// Most timeline events one record holds (the oldest are kept).
pub const MAX_RESULT_TIMELINE: usize = 4096;
/// Most records a listing reads (a folder with more lists the newest that fit).
pub const MAX_LISTED_FILES: usize = 20_000;
/// Records waiting for the IO pool at once; a match ending while this many wait drops the oldest.
pub const MAX_PENDING_WRITES: usize = 16;

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// The ruleset a match ran: its id and a hash of its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesetRef {
    /// The ruleset id: the playlist row's mode string (`"ctf"`, a game's own mode, ...).
    pub id: String,
    /// BLAKE3 of the canonical JSON of the session settings and the row's mode and options, lowercase hex.
    pub hash: String,
}

/// A point of the match on both clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchMark {
    /// The lockstep step.
    pub step: u64,
    /// The match tick (frozen while paused).
    pub tick: u64,
}

/// One team's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamResult {
    /// Team id, from 1.
    pub team: u8,
    /// The team's board score at the end.
    pub score: u32,
    /// Rounds won (`0` with rounds off).
    pub round_wins: u32,
}

/// One seated player's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerResult {
    /// The stable player key: the session's identity (`steam:<id>`), a hashed name key
    /// (`name-hash:<hex>`), or `seat:<team>.<slot>` when the host bound none. Records hold ids
    /// only: there is no display name (ADR-8880).
    pub key: String,
    /// Team.
    pub team: u8,
    /// Slot on that team.
    pub slot: u8,
    /// Kills.
    pub kills: u32,
    /// Deaths.
    pub deaths: u32,
    /// Assists.
    pub assists: u32,
    /// Capture and hill points.
    pub objective_points: u32,
    /// Ranking total.
    pub score: i64,
    /// Named counters, in key order.
    #[serde(default)]
    pub counters: BTreeMap<String, i64>,
}

/// The end-of-match ledger (`regia_rules::end_stats`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerSummary {
    /// The match tick the ledger opened on.
    pub start_tick: u64,
    /// The match tick it was stamped over, if it was.
    pub over_tick: Option<u64>,
    /// Ticks per second of the ledger's lines.
    pub tick_hz: u32,
    /// Key moments, oldest first, at most [`MAX_RESULT_TIMELINE`]. Read through a strict mirror of
    /// `TimelineEvent`: an unknown key in a row is refused like one anywhere else in the record
    /// (ADR-10064).
    #[serde(deserialize_with = "strict_timeline")]
    pub timeline: Vec<TimelineEvent>,
}

/// The record's reader of one [`TimelineEvent`] row (ADR-10064): the same wire names as the shared
/// type, plus `deny_unknown_fields`. The shared type stays lenient for the session snapshot; a
/// record refuses an unknown key in a timeline row the way it refuses one in every other object.
/// The conversion destructures exhaustively, so a field added to the shared type stops this
/// compiling until the mirror has it too.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineEventFile {
    tick: u64,
    kind: crate::rules::end_stats::TimelineKind,
    team: u8,
    foe_team: u8,
    #[serde(default)]
    stem: String,
}

impl From<TimelineEventFile> for TimelineEvent {
    fn from(file: TimelineEventFile) -> Self {
        let TimelineEventFile {
            tick,
            kind,
            team,
            foe_team,
            stem,
        } = file;
        Self {
            tick,
            kind,
            team,
            foe_team,
            stem,
        }
    }
}

impl From<TimelineEvent> for TimelineEventFile {
    fn from(event: TimelineEvent) -> Self {
        let TimelineEvent {
            tick,
            kind,
            team,
            foe_team,
            stem,
        } = event;
        Self {
            tick,
            kind,
            team,
            foe_team,
            stem,
        }
    }
}

fn strict_timeline<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<TimelineEvent>, D::Error> {
    Vec::<TimelineEventFile>::deserialize(d).map(|rows| rows.into_iter().map(Into::into).collect())
}

/// One finished match (`match-result` v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchResult {
    /// Always [`MATCH_RESULT_FORMAT`].
    pub format: String,
    /// Always [`MATCH_RESULT_FORMAT_VERSION`] for a record this build writes.
    pub version: u32,
    /// The host's 128-bit match id.
    pub match_id: Guid,
    /// The ruleset.
    pub ruleset: RulesetRef,
    /// The map (level) id; empty when no playlist row ran.
    pub map_id: String,
    /// Where the match launched.
    pub start: MatchMark,
    /// Where it ended.
    pub end: MatchMark,
    /// Why it ended (`EndReason`, snake case).
    pub end_reason: EndReason,
    /// The winning team; `0` for none.
    pub winner: u8,
    /// True when the match ended level (reason `draw`, or no winner).
    pub draw: bool,
    /// One row per team, in team order.
    pub teams: Vec<TeamResult>,
    /// One row per seated, non-observer player at the end, sorted by team then slot.
    pub players: Vec<PlayerResult>,
    /// The end-of-match ledger.
    pub ledger: LedgerSummary,
    /// The replay file name, when the host recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
}

/// Why a record does not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchResultError {
    /// Larger than [`MAX_RESULT_FILE_BYTES`].
    #[error("record is {0} bytes, more than the {MAX_RESULT_FILE_BYTES}-byte limit")]
    TooLarge(u64),
    /// Not JSON of the record's shape.
    #[error("not a match-result record: {0}")]
    Malformed(String),
    /// The `format` field names another format.
    #[error("format is `{0}`, not `match-result`")]
    WrongFormat(String),
    /// Written by a newer engine.
    #[error("match-result version {0} is newer than this engine's {MATCH_RESULT_FORMAT_VERSION}")]
    Newer(u32),
    /// A list past its cap.
    #[error("{0} holds more rows than the format allows")]
    TooMany(&'static str),
}

impl MatchResultError {
    /// The stable lowercase code (`match_result_malformed`, ...).
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge(_) => "match_result_too_large",
            Self::Malformed(_) | Self::WrongFormat(_) | Self::TooMany(_) => "match_result_malformed",
            Self::Newer(_) => "match_result_newer",
        }
    }
}

impl MatchResult {
    /// The record as pretty JSON (the file's bytes, ending in a newline).
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Reads a record. Refuses other formats, newer versions and lists past their caps.
    ///
    /// # Errors
    /// [`MatchResultError`]: too large, malformed, another format, newer, or too many rows.
    pub fn from_json(bytes: &[u8]) -> Result<Self, MatchResultError> {
        if bytes.len() as u64 > MAX_RESULT_FILE_BYTES {
            return Err(MatchResultError::TooLarge(bytes.len() as u64));
        }
        #[derive(Deserialize)]
        struct Head {
            format: String,
            version: u32,
        }
        let head: Head =
            serde_json::from_slice(bytes).map_err(|e| MatchResultError::Malformed(e.to_string()))?;
        if head.format != MATCH_RESULT_FORMAT {
            return Err(MatchResultError::WrongFormat(head.format));
        }
        if head.version > MATCH_RESULT_FORMAT_VERSION {
            return Err(MatchResultError::Newer(head.version));
        }
        let result: Self =
            serde_json::from_slice(bytes).map_err(|e| MatchResultError::Malformed(e.to_string()))?;
        if result.teams.len() > MAX_RESULT_TEAMS {
            return Err(MatchResultError::TooMany("teams"));
        }
        if result.players.len() > MAX_RESULT_PLAYERS {
            return Err(MatchResultError::TooMany("players"));
        }
        if result.ledger.timeline.len() > MAX_RESULT_TIMELINE {
            return Err(MatchResultError::TooMany("ledger.timeline"));
        }
        Ok(result)
    }

    /// A small complete record: two teams, two players, one counter, one timeline event (docs,
    /// fuzz fixture, tests).
    pub fn example() -> Self {
        Self {
            format: MATCH_RESULT_FORMAT.to_owned(),
            version: MATCH_RESULT_FORMAT_VERSION,
            match_id: Guid::from_u128(0x6d61_7463_682d_7265_7375_6c74_0000_0001),
            ruleset: RulesetRef {
                id: "ctf".to_owned(),
                hash: "0".repeat(64),
            },
            map_id: "harbor".to_owned(),
            start: MatchMark { step: 90, tick: 0 },
            end: MatchMark {
                step: 1290,
                tick: 1200,
            },
            end_reason: EndReason::CaptureLimitReached,
            winner: 1,
            draw: false,
            teams: vec![
                TeamResult {
                    team: 1,
                    score: 3,
                    round_wins: 0,
                },
                TeamResult {
                    team: 2,
                    score: 1,
                    round_wins: 0,
                },
            ],
            players: vec![
                PlayerResult {
                    key: "steam:76561190000000001".to_owned(),
                    team: 1,
                    slot: 0,
                    kills: 4,
                    deaths: 1,
                    assists: 2,
                    objective_points: 3,
                    score: 7,
                    counters: BTreeMap::from([("revives".to_owned(), 2)]),
                },
                PlayerResult {
                    key: "seat:2.0".to_owned(),
                    team: 2,
                    slot: 0,
                    kills: 1,
                    deaths: 4,
                    assists: 0,
                    objective_points: 1,
                    score: 2,
                    counters: BTreeMap::new(),
                },
            ],
            ledger: LedgerSummary {
                start_tick: 0,
                over_tick: Some(1200),
                tick_hz: 60,
                timeline: vec![TimelineEvent {
                    tick: 300,
                    kind: crate::rules::end_stats::TimelineKind::FirstKill,
                    team: 1,
                    foe_team: 2,
                    stem: String::new(),
                }],
            },
            replay: Some("harbor-1.regiareplay".to_owned()),
        }
    }

    /// The row of player `key`.
    pub fn player(&self, key: &str) -> Option<&PlayerResult> {
        self.players.iter().find(|p| p.key == key)
    }

    /// The row of `team`.
    pub fn team(&self, team: u8) -> Option<&TeamResult> {
        self.teams.iter().find(|t| t.team == team)
    }
}

/// The version of a `match-result` file, `Ok(None)` for JSON of another kind (the
/// `regia upgrade-project` probe: other `.json` files are skipped).
///
/// # Errors
/// A record whose `format` is `match-result` but whose `version` is missing or not a number.
pub fn probe_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(None);
    };
    if value.get("format").and_then(serde_json::Value::as_str) != Some(MATCH_RESULT_FORMAT) {
        return Ok(None);
    }
    value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .map(Some)
        .ok_or_else(|| "match-result without a numeric version".to_owned())
}

/// The field and variant lists `cargo xtask check-formats` hashes for `match-result`.
pub fn match_result_schema_fingerprint() -> String {
    let mut reasons = regia_core::schema::serde_variant_names::<EndReason>();
    reasons.extend(regia_core::schema::serde_variant_shapes::<EndReason>());
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<MatchResult>(),
        regia_core::schema::serde_field_names::<RulesetRef>(),
        regia_core::schema::serde_field_names::<MatchMark>(),
        regia_core::schema::serde_field_names::<TeamResult>(),
        regia_core::schema::serde_field_names::<PlayerResult>(),
        regia_core::schema::serde_field_names::<LedgerSummary>(),
        regia_core::schema::serde_field_names::<TimelineEvent>(),
        regia_core::schema::serde_variant_names::<crate::rules::end_stats::TimelineKind>(),
        reasons,
    ])
}

/// `match-result` is at its first version: no migration steps.
pub fn match_result_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The `regia upgrade-project` migrator: v1 is current, so there is nothing older to migrate from.
///
/// # Errors
/// Always, for any `from` (no older version exists).
pub fn migrate_match_result(_bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    Err(format!(
        "no match-result migrator from v{from} (v1 is the first version)"
    ))
}

// ---------------------------------------------------------------------------
// Building a record
// ---------------------------------------------------------------------------

/// Where the match id comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchIdSource {
    /// A random version 4 id per match (a host writing alone).
    #[default]
    Random,
    /// This id (the host generated it and sent it to its peers with the match settings).
    Fixed(Guid),
    /// Derived from `seed` and the match's ruleset hash, map and launch step: every lockstep peer
    /// that shares the seed derives the same id.
    Derived {
        /// The shared seed.
        seed: u64,
    },
}

/// A seat's stable key, as the host bound it. Records hold ids only (ADR-8880): a display name is
/// never bound.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerKey {
    /// The stable key (`steam:<id>`, `name:<name>`, or a game's own). A `name:` key is written as
    /// a hash of the name.
    pub key: String,
}

/// Seat to player key, filled by the host as players take seats (see `bind_net_player` with the
/// `net` feature).
#[derive(Resource, Debug, Clone, Default)]
pub struct MatchPlayerKeys {
    /// Keys by seat.
    pub seats: BTreeMap<SeatId, PlayerKey>,
}

impl MatchPlayerKeys {
    /// Binds `seat` to `key`.
    pub fn bind(&mut self, seat: SeatId, key: impl Into<String>) {
        self.seats.insert(seat, PlayerKey { key: key.into() });
    }
}

/// Binds `seat` to the session's stable identity of `player` under `mode` (the server persistence
/// rule, `server_persistence::player_identity`). Returns false when the player has none.
#[cfg(feature = "net")]
pub fn bind_net_player(
    world: &mut World,
    seat: SeatId,
    player: regia_net::model::NetPlayerId,
    mode: regia_net::server_host::PlayerIdentityMode,
) -> bool {
    // The display name is dropped: records hold ids only (ADR-8880).
    let Some((key, _name)) = crate::server_persistence::player_identity(world, player, mode) else {
        return false;
    };
    world
        .get_resource_or_insert_with(MatchPlayerKeys::default)
        .bind(seat, key);
    true
}

/// What a record is built from besides the session.
#[derive(Debug, Clone, Default)]
pub struct ResultContext<'a> {
    /// The match id.
    pub match_id: Guid,
    /// Seat keys.
    pub keys: Option<&'a MatchPlayerKeys>,
    /// The replay file name.
    pub replay: Option<String>,
}

/// The ruleset id and content hash of `session`'s running match.
pub fn ruleset_of(session: &MatchSession) -> RulesetRef {
    let (id, options) = match &session.current {
        Some(row) => (row.entry.mode.clone(), row.entry.options.clone()),
        None => (session.board.mode.as_lobby_str().to_owned(), BTreeMap::new()),
    };
    let canonical = serde_json::json!({
        "settings": session.settings(),
        "mode": id,
        "options": options,
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    RulesetRef {
        id,
        hash: blake3::hash(&bytes).to_hex().to_string(),
    }
}

/// The id `source` gives the running match of `session`.
pub fn match_id_for(session: &MatchSession, source: MatchIdSource) -> Guid {
    match source {
        MatchIdSource::Random => Guid::new_random(),
        MatchIdSource::Fixed(id) => id,
        MatchIdSource::Derived { seed } => {
            let ruleset = ruleset_of(session);
            let map = session.current.as_ref().map_or("", |r| r.entry.map_id.as_str());
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"regia.match_id.v1");
            hasher.update(&seed.to_le_bytes());
            hasher.update(ruleset.hash.as_bytes());
            hasher.update(&(map.len() as u64).to_le_bytes());
            hasher.update(map.as_bytes());
            hasher.update(&session.launched_step.to_le_bytes());
            let mut raw = [0u8; 16];
            raw.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
            Guid::from_u128(u128::from_le_bytes(raw))
        }
    }
}

/// The written form of a bound key: a name-derived key becomes a hash of the name (ids only,
/// ADR-8880).
fn written_key(key: &str) -> String {
    match key.strip_prefix("name:") {
        Some(name) => {
            let hash = blake3::hash(name.as_bytes()).to_hex();
            format!("name-hash:{}", &hash.as_str()[..32])
        }
        _ => key.to_owned(),
    }
}

/// The record of `session`'s match, once it has ended ([`MatchSession::match_end`]).
pub fn build_result(session: &MatchSession, ctx: &ResultContext<'_>) -> Option<MatchResult> {
    let end = session.match_end?;
    let max_teams = session
        .board
        .team_scores
        .len()
        .saturating_sub(1)
        .min(MAX_RESULT_TEAMS);
    let round_wins = &session.flow.phases.round_wins;
    let teams = (1..=max_teams)
        .filter_map(|team| {
            Some(TeamResult {
                team: u8::try_from(team).ok()?,
                score: session.board.team_scores.get(team).copied().unwrap_or(0),
                round_wins: round_wins.get(team).copied().unwrap_or(0),
            })
        })
        .collect();
    // Only seats that play: a kill credited to an empty coordinate opens a ledger row nobody sits
    // in (teams-scores review, finding 2), and observers have no match of their own.
    let playing = session.flow.playing_seats();
    let players = session
        .ledgers
        .rows
        .iter()
        .filter(|row| playing.binary_search(&row.seat()).is_ok())
        .take(MAX_RESULT_PLAYERS)
        .map(|row| {
            let seat = row.seat();
            let bound = ctx.keys.and_then(|k| k.seats.get(&seat));
            let key = bound.map_or_else(
                || format!("seat:{}.{}", seat.team, seat.slot),
                |b| written_key(&b.key),
            );
            PlayerResult {
                key,
                team: row.team,
                slot: row.slot,
                kills: row.kills,
                deaths: row.deaths,
                assists: row.assists,
                objective_points: row.objective_points,
                score: row.score,
                counters: row.counters.clone(),
            }
        })
        .collect();
    let ledger = &session.ledger;
    Some(MatchResult {
        format: MATCH_RESULT_FORMAT.to_owned(),
        version: MATCH_RESULT_FORMAT_VERSION,
        match_id: ctx.match_id,
        ruleset: ruleset_of(session),
        map_id: session
            .current
            .as_ref()
            .map(|r| r.entry.map_id.clone())
            .unwrap_or_default(),
        start: MatchMark {
            step: session.launched_step,
            tick: ledger.start_tick,
        },
        end: MatchMark {
            step: end.step,
            tick: end.tick,
        },
        end_reason: end.reason,
        winner: end.winner,
        draw: end.winner == 0 || end.reason == EndReason::Draw,
        teams,
        players,
        ledger: LedgerSummary {
            start_tick: ledger.start_tick,
            over_tick: ledger.over_tick.or(Some(end.tick)),
            tick_hz: ledger.tick_hz,
            timeline: ledger
                .timeline
                .iter()
                .take(MAX_RESULT_TIMELINE)
                .cloned()
                .collect(),
        },
        replay: ctx.replay.clone(),
    })
}

// ---------------------------------------------------------------------------
// Dates and the folder
// ---------------------------------------------------------------------------

/// The civil UTC date of `unix_secs` as `YYYY-MM-DD` (proleptic Gregorian; days-to-civil from
/// H. Hinnant, "chrono-Compatible Low-Level Date Algorithms", public domain, original code).
pub fn utc_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` folder name (civil-to-days, same source), or `None`
/// when the name is not a date.
pub fn date_days(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let num = |s: &str| -> Option<i64> {
        if s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    let year = num(name.get(0..4)?)?;
    let month = num(name.get(5..7)?)?;
    let day = num(name.get(8..10)?)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The path of record `match_id` written on `date`.
pub fn result_path(root: &Path, date: &str, match_id: Guid) -> PathBuf {
    root.join(date).join(format!("{match_id}.json"))
}

/// Writes `result` under `root` for `date`, atomically, then rotates the folder.
///
/// # Errors
/// The IO error of the write (`match_result_write_failed`). Rotation failures are logged only.
pub fn write_result(
    root: &Path,
    date: &str,
    result: &MatchResult,
    keep_days: u32,
    keep_count: u32,
) -> Result<PathBuf, String> {
    let path = result_path(root, date, result.match_id);
    write_atomic(&path, result.to_json().as_bytes()).map_err(|e| e.to_string())?;
    if let Err(e) = rotate(root, date_days(date), keep_days, keep_count) {
        tracing::warn!(target: "regia_framework", code = "match_result_rotate_failed", "{e}");
    }
    Ok(path)
}

/// One complete record file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultFile {
    /// The date folder.
    pub date: String,
    /// The match id from the file name.
    pub match_id: Guid,
    /// The full path.
    pub path: PathBuf,
    /// Modification time in nanoseconds since the epoch (orders records within a day).
    pub modified_ns: u128,
}

/// Every complete record file under `root`, newest first: date folders descending, then
/// modification time, then file name. Temp files of an interrupted write (`*.json.tmp.*`) and
/// files whose name is not `<guid>.json` are not records.
///
/// # Errors
/// `root` cannot be read.
pub fn result_files(root: &Path) -> std::io::Result<Vec<ResultFile>> {
    let mut out = Vec::new();
    let mut dates: Vec<String> = std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| date_days(name).is_some())
        .collect();
    dates.sort_unstable_by(|a, b| b.cmp(a));
    for date in dates {
        let Ok(entries) = std::fs::read_dir(root.join(&date)) else {
            continue;
        };
        let mut day = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(match_id) = stem.parse::<Guid>() else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let modified_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            day.push(ResultFile {
                date: date.clone(),
                match_id,
                path: entry.path(),
                modified_ns,
            });
        }
        day.sort_unstable_by(|a, b| {
            b.modified_ns
                .cmp(&a.modified_ns)
                .then_with(|| b.match_id.cmp(&a.match_id))
        });
        out.extend(day);
        if out.len() >= MAX_LISTED_FILES {
            out.truncate(MAX_LISTED_FILES);
            break;
        }
    }
    Ok(out)
}

/// Removes date folders older than `today - keep_days` (`0` keeps every age) and the oldest
/// records beyond `keep_count` (`0` keeps every count). Empty date folders are removed. Returns how
/// many records were removed.
///
/// # Errors
/// `root` cannot be read.
pub fn rotate(root: &Path, today: Option<i64>, keep_days: u32, keep_count: u32) -> std::io::Result<usize> {
    let files = result_files(root)?;
    let mut removed = 0;
    let oldest_kept = today.map(|t| t - i64::from(keep_days));
    for (index, file) in files.iter().enumerate() {
        let too_old = keep_days > 0
            && matches!((oldest_kept, date_days(&file.date)), (Some(limit), Some(day)) if day < limit);
        let too_many = keep_count > 0 && index >= keep_count as usize;
        if (too_old || too_many) && std::fs::remove_file(&file.path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        for entry in std::fs::read_dir(root)?.filter_map(Result::ok) {
            let is_date = entry.file_name().to_str().is_some_and(|n| date_days(n).is_some());
            if is_date {
                // Fails while the folder still holds files; that is the point.
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    Ok(removed)
}

/// A record's listing row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultSummary {
    /// The match id.
    pub match_id: Guid,
    /// The date folder.
    pub date: String,
    /// The ruleset id.
    pub ruleset: String,
    /// The map id.
    pub map_id: String,
    /// Why it ended.
    pub end_reason: EndReason,
    /// The winner (`0` for none).
    pub winner: u8,
    /// A draw.
    pub draw: bool,
    /// Where it ended.
    pub end: MatchMark,
    /// Player rows.
    pub players: usize,
}

/// A page of records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResultPage {
    /// Newest first.
    pub results: Vec<ResultSummary>,
    /// Complete records on or after `since` (whether or not they fit the page).
    pub total: usize,
    /// Files with a record's name that did not read (torn by hand, another engine's, too large).
    pub skipped: usize,
}

/// Reads one record file, refusing files over the size cap before reading them.
///
/// # Errors
/// The IO error or the [`MatchResultError`], as text with its code first.
pub fn read_result_file(path: &Path) -> Result<MatchResult, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("match_result_read_failed: {e}"))?;
    if meta.len() > MAX_RESULT_FILE_BYTES {
        let err = MatchResultError::TooLarge(meta.len());
        return Err(format!("{}: {err}", err.code()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("match_result_read_failed: {e}"))?;
    MatchResult::from_json(&bytes).map_err(|e| format!("{}: {e}", e.code()))
}

/// The newest records under `root` dated `since` (`YYYY-MM-DD`) or later, at most `limit`.
///
/// # Errors
/// `root` cannot be read.
pub fn list_results(root: &Path, since: Option<i64>, limit: usize) -> std::io::Result<ResultPage> {
    let mut page = ResultPage {
        results: Vec::new(),
        total: 0,
        skipped: 0,
    };
    for file in result_files(root)? {
        if since.is_some_and(|s| date_days(&file.date).is_some_and(|d| d < s)) {
            continue;
        }
        if page.results.len() >= limit {
            // Count without reading: the page is full.
            page.total += 1;
            continue;
        }
        match read_result_file(&file.path) {
            Ok(r) if r.match_id == file.match_id => {
                page.total += 1;
                page.results.push(ResultSummary {
                    match_id: r.match_id,
                    date: file.date.clone(),
                    ruleset: r.ruleset.id.clone(),
                    map_id: r.map_id.clone(),
                    end_reason: r.end_reason,
                    winner: r.winner,
                    draw: r.draw,
                    end: r.end,
                    players: r.players.len(),
                });
            }
            _ => page.skipped += 1,
        }
    }
    Ok(page)
}

/// The record `match_id` under `root`, with its date folder.
///
/// # Errors
/// `match_result_not_found`, or the read error of the file.
pub fn find_result(root: &Path, match_id: Guid) -> Result<(String, MatchResult), String> {
    let file = result_files(root)
        .map_err(|e| format!("match_results_unavailable: {e}"))?
        .into_iter()
        .find(|f| f.match_id == match_id)
        .ok_or_else(|| format!("match_result_not_found: no record {match_id}"))?;
    let result = read_result_file(&file.path)?;
    Ok((file.date, result))
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// A destination for finished-match records, run in order on the main thread when a match ends.
pub trait MatchResultSink: Send + Sync + 'static {
    /// The sink's name in logs.
    fn name(&self) -> &str;

    /// Handles one record.
    ///
    /// # Errors
    /// Why the sink could not take it; logged as `match_result_sink_failed`.
    fn send(&mut self, world: &mut World, result: &MatchResult) -> Result<(), String>;
}

/// The ordered sinks.
#[derive(Resource, Default)]
pub struct MatchResultSinks {
    sinks: Vec<Box<dyn MatchResultSink>>,
}

impl MatchResultSinks {
    /// Appends a sink (runs after the ones before it).
    pub fn push(&mut self, sink: impl MatchResultSink) {
        self.sinks.push(Box::new(sink));
    }

    /// The sinks' names, in run order.
    pub fn names(&self) -> Vec<&str> {
        self.sinks.iter().map(|s| s.name()).collect()
    }
}

/// A value read from a record for one player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultValue {
    /// The player's kills.
    Kills,
    /// The player's deaths.
    Deaths,
    /// The player's assists.
    Assists,
    /// The player's objective points.
    ObjectivePoints,
    /// The player's ranking score.
    Score,
    /// A named counter of the player (`0` when absent).
    Counter(String),
    /// `1` when the player's team won, else `0`.
    Won,
    /// The player's team's board score.
    TeamScore,
    /// The player's team's round wins.
    RoundWins,
}

impl ResultValue {
    /// The value for player `key` in `result`, `None` when the record has no such player.
    pub fn read(&self, result: &MatchResult, key: &str) -> Option<i64> {
        let p = result.player(key)?;
        let team = result.team(p.team);
        Some(match self {
            Self::Kills => i64::from(p.kills),
            Self::Deaths => i64::from(p.deaths),
            Self::Assists => i64::from(p.assists),
            Self::ObjectivePoints => i64::from(p.objective_points),
            Self::Score => p.score,
            Self::Counter(name) => p.counters.get(name).copied().unwrap_or(0),
            Self::Won => i64::from(!result.draw && result.winner == p.team),
            Self::TeamScore => team.map_or(0, |t| i64::from(t.score)),
            Self::RoundWins => team.map_or(0, |t| i64::from(t.round_wins)),
        })
    }
}

/// Clamps a record value to a 32-bit platform score.
pub fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// One board the [`LeaderboardSink`] submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardSinkBoard {
    /// The board name.
    pub board: String,
    /// The value submitted.
    pub value: ResultValue,
}

/// Submits configured boards of the [`Leaderboard`] resource from a record: the row of
/// [`LeaderboardSink::player`] (the local user's key, as uploads are the local user's), clamped to
/// 32 bits, with the board's declared upload method.
#[derive(Debug, Clone, Default)]
pub struct LeaderboardSink {
    /// The local player's key in the record.
    pub player: String,
    /// Boards, in submit order.
    pub boards: Vec<LeaderboardSinkBoard>,
}

impl MatchResultSink for LeaderboardSink {
    fn name(&self) -> &str {
        "leaderboard"
    }

    fn send(&mut self, world: &mut World, result: &MatchResult) -> Result<(), String> {
        if result.player(&self.player).is_none() {
            // This peer did not play the match (a dedicated host): nothing to submit.
            return Ok(());
        }
        let mut boards = world
            .get_resource_mut::<Leaderboard>()
            .ok_or_else(|| "no Leaderboard resource".to_owned())?;
        let mut first_error = None;
        for entry in &self.boards {
            let Some(value) = entry.value.read(result, &self.player) else {
                continue;
            };
            if let Err(e) = boards.submit(&entry.board, clamp_i32(value), &[]) {
                first_error.get_or_insert_with(|| format!("board {}: {e}", entry.board));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// Settings of [`MatchResultsPlugin`] (`book/agents/match-results.md`).
#[derive(Resource, Debug, Clone)]
pub struct MatchResultsSettings {
    /// Record finished matches (sinks and file). Default on.
    pub enabled: bool,
    /// The folder records are written to (`<data dir>/matches`); `None` writes no file.
    pub root: Option<PathBuf>,
    /// Remove date folders older than this many days after a write; `0` keeps every age. Default 90.
    pub keep_days: u32,
    /// Keep at most this many records; `0` keeps every count. Default 1000.
    pub keep_count: u32,
    /// Where match ids come from. Default [`MatchIdSource::Random`].
    pub match_id: MatchIdSource,
    /// Write on the calling thread instead of the IO task pool (tools and tests). Default off.
    pub write_inline: bool,
}

impl Default for MatchResultsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            root: None,
            keep_days: 90,
            keep_count: 1000,
            match_id: MatchIdSource::Random,
            write_inline: false,
        }
    }
}

/// A record waiting for its write.
#[derive(Debug, Clone)]
struct PendingWrite {
    result: MatchResult,
    date: String,
    attempts: u32,
}

type WriteTask = Task<(PendingWrite, Result<PathBuf, String>)>;

/// State of [`MatchResultsPlugin`].
#[derive(Resource)]
pub struct MatchResults {
    /// Seconds since the epoch for the date folder (the wall clock unless a tool sets another).
    pub clock: fn() -> u64,
    /// The replay file name of the running match; taken by its record.
    pub replay: Option<String>,
    /// The newest record built.
    pub last: Option<MatchResult>,
    /// The path of the newest record written.
    pub last_path: Option<PathBuf>,
    /// The newest write error.
    pub last_error: Option<String>,
    /// Records written.
    pub written: u64,
    /// Writes that failed (each retry counts).
    pub failed: u64,
    /// Records dropped after their retry failed, or pushed out of a full queue.
    pub dropped: u64,
    seen: Option<(u64, u64, u64)>,
    queue: VecDeque<PendingWrite>,
    in_flight: Option<WriteTask>,
}

fn wall_clock_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Default for MatchResults {
    fn default() -> Self {
        Self {
            clock: wall_clock_secs,
            replay: None,
            last: None,
            last_path: None,
            last_error: None,
            written: 0,
            failed: 0,
            dropped: 0,
            seen: None,
            queue: VecDeque::new(),
            in_flight: None,
        }
    }
}

impl std::fmt::Debug for MatchResults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatchResults")
            .field("written", &self.written)
            .field("failed", &self.failed)
            .field("dropped", &self.dropped)
            .field("queued", &self.queue.len())
            .field("in_flight", &self.in_flight.is_some())
            .finish_non_exhaustive()
    }
}

impl MatchResults {
    /// Records waiting for (or in) a write.
    pub fn pending(&self) -> usize {
        self.queue.len() + usize::from(self.in_flight.is_some())
    }
}

/// Records each finished match of the [`MatchRules`] session (see the module docs). Adds the
/// settings, the state, the sinks and the player keys, and one exclusive system in `Last`.
pub struct MatchResultsPlugin;

impl Plugin for MatchResultsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MatchResultsSettings>()
            .init_resource::<MatchResults>()
            .init_resource::<MatchResultSinks>()
            .init_resource::<MatchPlayerKeys>()
            .add_systems(Last, match_results_system);
        #[cfg(feature = "net")]
        app.world_mut()
            .get_resource_or_insert_with(regia_net::server_host::HostGameHooks::default)
            .add_event_hook(host_event_hook);
    }
}

/// The folder name a dedicated server's records go to, under its server directory.
pub const SERVER_RESULTS_DIR: &str = "matches";

/// On a dedicated server's start, records go to `<server dir>/matches` unless the game chose a root.
#[cfg(feature = "net")]
fn host_event_hook(world: &mut World, event: &regia_net::server_host::HostEvent<'_>) -> Result<(), String> {
    if let regia_net::server_host::HostEvent::Started { base_dir, .. } = *event
        && let Some(mut settings) = world.get_resource_mut::<MatchResultsSettings>()
        && settings.root.is_none()
    {
        settings.root = Some(base_dir.join(SERVER_RESULTS_DIR));
    }
    Ok(())
}

/// The plugin's system: finishes writes, records a match that just ended, starts the next write.
pub fn match_results_system(world: &mut World) {
    let _span = tracing::trace_span!("match_results").entered();
    poll_write(world, false);
    let ended = world.get_resource::<MatchRules>().and_then(|rules| {
        let end = rules.session.match_end?;
        Some((rules.session.launched_step, end.step, end.tick))
    });
    if let Some(key) = ended {
        let seen = world.get_resource::<MatchResults>().and_then(|r| r.seen);
        if seen != Some(key)
            && let Some(session) = world.get_resource::<MatchRules>().map(|r| r.session.clone())
        {
            if let Some(mut state) = world.get_resource_mut::<MatchResults>() {
                state.seen = Some(key);
            }
            record_match(world, &session);
        }
    }
    start_write(world);
}

/// Builds the record of `session`'s ended match, runs the sinks and queues the write. For hosts that
/// keep their session outside [`MatchRules`]. Returns the record (`None` while the match plays or
/// with recording off).
pub fn record_match(world: &mut World, session: &MatchSession) -> Option<MatchResult> {
    let settings = world
        .get_resource::<MatchResultsSettings>()
        .cloned()
        .unwrap_or_default();
    if !settings.enabled {
        return None;
    }
    let replay = world
        .get_resource_mut::<MatchResults>()
        .and_then(|mut state| state.replay.take());
    let keys = world.get_resource::<MatchPlayerKeys>().cloned();
    let ctx = ResultContext {
        match_id: match_id_for(session, settings.match_id),
        keys: keys.as_ref(),
        replay,
    };
    let result = build_result(session, &ctx)?;
    run_sinks(world, &result);
    let clock = world
        .get_resource::<MatchResults>()
        .map_or(wall_clock_secs as fn() -> u64, |r| r.clock);
    if let Some(mut state) = world.get_resource_mut::<MatchResults>() {
        state.last = Some(result.clone());
        if settings.root.is_some() {
            if state.queue.len() >= MAX_PENDING_WRITES {
                state.queue.pop_front();
                state.dropped += 1;
                tracing::warn!(target: "regia_framework", code = "match_result_dropped", "the write queue is full");
            }
            state.queue.push_back(PendingWrite {
                result: result.clone(),
                date: utc_date(clock()),
                attempts: 0,
            });
        }
    }
    if settings.write_inline {
        start_write(world);
    }
    Some(result)
}

fn run_sinks(world: &mut World, result: &MatchResult) {
    let Some(mut sinks) = world.remove_resource::<MatchResultSinks>() else {
        return;
    };
    for sink in &mut sinks.sinks {
        if let Err(e) = sink.send(world, result) {
            tracing::warn!(
                target: "regia_framework",
                code = "match_result_sink_failed",
                sink = sink.name(),
                "{e}"
            );
        }
    }
    world.insert_resource(sinks);
}

fn finish(world: &mut World, mut pending: PendingWrite, outcome: Result<PathBuf, String>) {
    let Some(mut state) = world.get_resource_mut::<MatchResults>() else {
        return;
    };
    match outcome {
        Ok(path) => {
            state.written += 1;
            state.last_path = Some(path);
        }
        Err(e) => {
            state.failed += 1;
            tracing::warn!(
                target: "regia_framework",
                code = "match_result_write_failed",
                match_id = %pending.result.match_id,
                "{e}"
            );
            state.last_error = Some(e);
            pending.attempts += 1;
            if pending.attempts < 2 {
                // One in-memory retry, before anything queued after it.
                state.queue.push_front(pending);
            } else {
                state.dropped += 1;
                tracing::warn!(
                    target: "regia_framework",
                    code = "match_result_dropped",
                    match_id = %pending.result.match_id,
                    "the retry failed too"
                );
            }
        }
    }
}

/// Collects a finished write; with `block`, waits for the one in flight.
fn poll_write(world: &mut World, block: bool) {
    let Some(mut state) = world.get_resource_mut::<MatchResults>() else {
        return;
    };
    let ready = state.in_flight.as_ref().is_some_and(|t| block || t.is_finished());
    if !ready {
        return;
    }
    let Some(task) = state.in_flight.take() else {
        return;
    };
    let (pending, outcome) = regia_core::tasks::block_on(task);
    finish(world, pending, outcome);
}

fn start_write(world: &mut World) {
    let settings = world
        .get_resource::<MatchResultsSettings>()
        .cloned()
        .unwrap_or_default();
    let Some(root) = settings.root.clone() else {
        return;
    };
    loop {
        let Some(mut state) = world.get_resource_mut::<MatchResults>() else {
            return;
        };
        if state.in_flight.is_some() {
            return;
        }
        let Some(pending) = state.queue.pop_front() else {
            return;
        };
        let (days, count) = (settings.keep_days, settings.keep_count);
        if settings.write_inline {
            let outcome = write_result(&root, &pending.date, &pending.result, days, count);
            finish(world, pending, outcome);
            // A failed inline write is retried by the next loop turn (once).
            continue;
        }
        state.in_flight = Some(regia_core::tasks::spawn_io(async move {
            let outcome = write_result(&root, &pending.date, &pending.result, days, count);
            (pending, outcome)
        }));
        return;
    }
}

/// Waits for every queued and in-flight write (shutdown, tools, tests).
pub fn flush_match_results(world: &mut World) {
    for _ in 0..=MAX_PENDING_WRITES * 2 {
        poll_write(world, true);
        start_write(world);
        let idle = world
            .get_resource::<MatchResults>()
            .is_none_or(|r| r.pending() == 0);
        if idle {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates_round_trip_through_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_794_960_000), "2026-11-18");
        for days in [-1, 0, 59, 60, 365, 10_957, 11_016, 20_775, 47_482] {
            let secs = u64::try_from(days.max(0) * 86_400).unwrap_or(0);
            let name = utc_date(secs);
            assert_eq!(date_days(&name), Some(days.max(0)), "{name}");
        }
        assert_eq!(date_days("2026-13-01"), None);
        assert_eq!(date_days("20x6-01-01"), None);
        assert_eq!(date_days("2026-1-01"), None);
    }

    #[test]
    fn name_keys_are_always_hashed() {
        assert!(written_key("name:Ada").starts_with("name-hash:"));
        assert!(!written_key("name:Ada").contains("Ada"));
        assert_eq!(written_key("steam:7656"), "steam:7656");
    }

    #[test]
    fn probe_reads_only_match_results() {
        assert_eq!(
            probe_version(br#"{"format":"match-result","version":1}"#),
            Ok(Some(1))
        );
        assert_eq!(probe_version(br#"{"format":"viewer"}"#), Ok(None));
        assert_eq!(probe_version(b"not json"), Ok(None));
        assert!(probe_version(br#"{"format":"match-result"}"#).is_err());
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_dedicated_server_writes_under_its_server_dir_unless_the_game_chose() {
        use regia_net::server_host::{HostEvent, ServerSettings};
        let settings = ServerSettings::default();
        let started = HostEvent::Started {
            level: "arena",
            base_dir: Path::new("/srv/game"),
            settings: &settings,
        };
        let mut world = World::new();
        world.insert_resource(MatchResultsSettings::default());
        assert_eq!(host_event_hook(&mut world, &started), Ok(()));
        let root = world.resource::<MatchResultsSettings>().root.clone();
        assert_eq!(root, Some(Path::new("/srv/game").join(SERVER_RESULTS_DIR)));

        world.resource_mut::<MatchResultsSettings>().root = Some(PathBuf::from("/data/history"));
        assert_eq!(host_event_hook(&mut world, &started), Ok(()));
        let root = world.resource::<MatchResultsSettings>().root.clone();
        assert_eq!(
            root,
            Some(PathBuf::from("/data/history")),
            "the game's root stands"
        );
    }

    /// ADR-10064: every object of a record refuses an unknown key, the timeline rows too. Before,
    /// `TimelineEvent` (shared with the session snapshot) had no `deny_unknown_fields`, so a row's
    /// extra key was dropped while the same key on a team or player row was refused.
    #[test]
    fn timeline_rows_refuse_unknown_keys() {
        let v1 = include_str!("../tests/fixtures/match_results/v1.json");
        let record = MatchResult::from_json(v1.as_bytes()).expect("the v1 corpus record reads");
        assert_eq!(record.ledger.timeline.len(), 1);
        let with = |key: &str| v1.replacen("\"stem\": \"\"", &format!("\"stem\": \"\", {key}"), 1);
        for (extra, field) in [("\"weapon\": 3", "weapon"), ("\"foe\": 2", "foe")] {
            let text = with(extra);
            assert_ne!(text, v1, "the fixture has a timeline row to patch");
            let err = MatchResult::from_json(text.as_bytes()).expect_err(extra);
            assert_eq!(err.code(), "match_result_malformed", "{extra}");
            assert!(err.to_string().contains(field), "{extra}: {err}");
        }
        // The same key on a player row was already refused (the control).
        let player = v1.replacen("\"counters\": {}", "\"counters\": {}, \"weapon\": 3", 1);
        assert!(MatchResult::from_json(player.as_bytes()).is_err());
        // A row without `stem` still reads (it defaults to empty, as in the shared type).
        let no_stem = v1.replacen(",\n        \"stem\": \"\"", "", 1);
        assert_ne!(no_stem, v1);
        let read = MatchResult::from_json(no_stem.as_bytes()).expect("stem defaults");
        assert_eq!(read, record);
    }

    #[test]
    fn newer_and_foreign_records_are_refused() {
        let err = MatchResult::from_json(br#"{"format":"match-result","version":2}"#);
        assert_eq!(err.map_err(|e| e.code()), Err("match_result_newer"));
        let err = MatchResult::from_json(br#"{"format":"other","version":1}"#);
        assert_eq!(err.map_err(|e| e.code()), Err("match_result_malformed"));
    }
}
