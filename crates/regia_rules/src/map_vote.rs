// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/map_vote.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: ballots are a BTreeMap so iteration order is the voter id (det-scan
// rejects HashMap); the rotation subset of server.toml is parsed here so the lockstep crate does
// not take a toml dependency; map ids are data; the ballot size and the open window are settings
// whose defaults are 3 options and 20 seconds at the 60 Hz tick.

//! End-of-match map vote.
//!
//! The host lists map ids in rotation order and turns the vote on. Off by default. When it is on,
//! the end of a match opens a ballot of the next rotation maps (default 3, wrapping, a short
//! rotation yields fewer). One ballot per voter; a later pick replaces the earlier one. Ties go
//! to rotation order, including a timeout with no ballots, which picks option 0. The window is
//! 20 seconds at [`regia_detsim::tick::TICK_RATE_HZ`] (1200 ticks) unless the host sets another
//! duration.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::playlist::{Playlist, PlaylistEntry};

/// How long an end-of-match vote stays open, in seconds. The default window.
pub const MAP_VOTE_DURATION_SECS: u64 = 20;

/// How many of the next rotation maps appear on the ballot, by default.
pub const MAP_VOTE_OPTIONS: usize = 3;

/// [`MAP_VOTE_DURATION_SECS`] in sim ticks at [`regia_detsim::tick::TICK_RATE_HZ`].
pub const MAP_VOTE_DURATION_TICKS: u64 = regia_detsim::tick::ticks_from_secs(20.0);

fn default_option_count() -> usize {
    MAP_VOTE_OPTIONS
}

fn default_duration_ticks() -> u64 {
    MAP_VOTE_DURATION_TICKS
}

/// Dedicated-server map list and the opt-in vote flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerMaps {
    /// Map ids in rotation order. Empty means no rotation is configured.
    #[serde(default)]
    pub rotation: Vec<String>,
    /// End-of-match vote among the next rotation maps. Off by default.
    #[serde(default)]
    pub map_vote: bool,
    /// How many maps the ballot offers. The default is [`MAP_VOTE_OPTIONS`].
    #[serde(default = "default_option_count")]
    pub option_count: usize,
    /// Ticks the ballot stays open. The default is [`MAP_VOTE_DURATION_TICKS`].
    #[serde(default = "default_duration_ticks")]
    pub duration_ticks: u64,
}

impl Default for ServerMaps {
    fn default() -> Self {
        Self {
            rotation: Vec::new(),
            map_vote: false,
            option_count: MAP_VOTE_OPTIONS,
            duration_ticks: MAP_VOTE_DURATION_TICKS,
        }
    }
}

impl ServerMaps {
    /// Parse a `server.toml` subset. Both top-level keys and a `[server]` table are accepted;
    /// the `[server]` table wins when both are present. Missing keys keep the defaults. A broken
    /// file is the default (empty rotation, vote off).
    pub fn parse(source: &str) -> Self {
        parse_server_text(source).unwrap_or_default()
    }

    /// Read the rotation and the vote flag from `path`. A missing or unreadable file is the default.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(source) => Self::parse(&source),
            Err(_) => Self::default(),
        }
    }

    /// The ballot size setting.
    pub fn option_count(&self) -> usize {
        self.option_count
    }

    /// The next rotation maps after `current`, in order, wrapping at most once.
    ///
    /// A map that is not on the rotation yields the head. A short rotation yields fewer options
    /// than [`Self::option_count`]. The walk is [`Playlist::vote_candidates`] with no player filter,
    /// so the playlist and the rotation share one eligibility rule.
    pub fn next_options(&self, current: &str) -> Vec<String> {
        if self.rotation.is_empty() || self.option_count == 0 {
            return Vec::new();
        }
        let entries: Vec<PlaylistEntry> = self
            .rotation
            .iter()
            .map(|id| PlaylistEntry::new(id.clone(), ""))
            .collect();
        let start = self
            .rotation
            .iter()
            .position(|map| map == current)
            .map_or(0, |index| index + 1);
        let list = Playlist {
            entries,
            index: u32::try_from(start).unwrap_or(0),
            looping: true,
            ..Playlist::default()
        };
        list.vote_candidates(self.option_count, 0)
            .into_iter()
            .map(|row| row.map_id)
            .collect()
    }

    /// Open the end-of-match vote after `current` when the flag is on and the rotation has at
    /// least one map. `None` while the vote is off or the ballot would be empty.
    pub fn open_vote(&self, current: &str, now_tick: u64) -> Option<MapVote> {
        if !self.map_vote {
            return None;
        }
        let options = self.next_options(current);
        if options.is_empty() {
            return None;
        }
        Some(MapVote::open(options, now_tick, self.duration_ticks))
    }

    fn retain_known_order(&mut self) {
        self.rotation.retain(|map| !map.trim().is_empty());
    }
}

/// Why a ballot was refused. A refused ballot changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteReject {
    /// The window has passed, or there is no open vote.
    Closed,
    /// Not one of the ballot options.
    BadOption,
}

/// One end-of-match vote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapVote {
    /// Ballot options in rotation order. Ties resolve to the lowest index.
    pub options: Vec<String>,
    /// Tick the vote opened.
    pub opened_tick: u64,
    /// First tick the vote is closed.
    pub close_tick: u64,
    /// One entry per voter. Casting again replaces the earlier pick. Key order is the voter id.
    #[serde(default)]
    votes: BTreeMap<u32, usize>,
}

impl MapVote {
    /// Open a vote over `options` at `now_tick` for the default window.
    pub fn new(options: Vec<String>, now_tick: u64) -> Self {
        Self::open(options, now_tick, MAP_VOTE_DURATION_TICKS)
    }

    /// Open a vote over `options` at `now_tick` for `duration_ticks`.
    pub fn open(options: Vec<String>, now_tick: u64, duration_ticks: u64) -> Self {
        let close_tick = now_tick.saturating_add(duration_ticks);
        Self {
            options,
            opened_tick: now_tick,
            close_tick,
            votes: BTreeMap::new(),
        }
    }

    /// True from the opening tick until the window passes.
    pub fn is_open(&self, now_tick: u64) -> bool {
        now_tick < self.close_tick
    }

    /// True once the window has passed.
    pub fn expired(&self, now_tick: u64) -> bool {
        !self.is_open(now_tick)
    }

    /// Cast or replace one voter's pick. Refused after the window or for an option that is not
    /// on the ballot. A refusal leaves the earlier pick in place.
    pub fn cast_vote(&mut self, voter: u32, option: usize, now_tick: u64) -> Result<(), VoteReject> {
        if !self.is_open(now_tick) {
            return Err(VoteReject::Closed);
        }
        if option >= self.options.len() {
            return Err(VoteReject::BadOption);
        }
        self.votes.insert(voter, option);
        Ok(())
    }

    /// Ballots currently held (one per voter).
    pub fn vote_count(&self) -> usize {
        self.votes.len()
    }

    /// Votes on one option.
    pub fn count(&self, option: usize) -> usize {
        let mut n = 0;
        for pick in self.votes.values() {
            if *pick == option {
                n += 1;
            }
        }
        n
    }

    /// Per-option counts in rotation order.
    pub fn tally(&self) -> Vec<usize> {
        (0..self.options.len()).map(|option| self.count(option)).collect()
    }

    /// Winning option index. Ties go to rotation order (lowest index), so a zero-vote timeout
    /// falls back to the first option. `None` only when the ballot itself is empty.
    pub fn winner(&self) -> Option<usize> {
        if self.options.is_empty() {
            return None;
        }
        let mut best = 0;
        let mut option = 1;
        while option < self.options.len() {
            if self.count(option) > self.count(best) {
                best = option;
            }
            option += 1;
        }
        Some(best)
    }

    /// Winning map id, with the same tiebreak as [`Self::winner`].
    pub fn winner_map(&self) -> Option<&str> {
        self.winner()
            .and_then(|index| self.options.get(index).map(String::as_str))
    }
}

/// Parse the rotation subset. `None` means the text is not a usable file.
fn parse_server_text(source: &str) -> Option<ServerMaps> {
    let mut top_vote = None;
    let mut top_rot = None;
    let mut srv_vote = None;
    let mut srv_rot = None;
    let mut section = Section::Top;
    for raw in source.lines() {
        let line = strip_hash_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') {
                return None;
            }
            let name = line[1..line.len() - 1].trim();
            section = if name == "server" {
                Section::Server
            } else {
                Section::Other
            };
            continue;
        }
        if section == Section::Other {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        let key = key.trim();
        let value = value.trim();
        if key == "map_vote" {
            let flag = match value {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            if section == Section::Server {
                srv_vote = Some(flag);
            } else {
                top_vote = Some(flag);
            }
        } else if key == "rotation" {
            let list = parse_string_array(value)?;
            if section == Section::Server {
                srv_rot = Some(list);
            } else {
                top_rot = Some(list);
            }
        }
    }
    let mut maps = ServerMaps::default();
    if let Some(flag) = top_vote {
        maps.map_vote = flag;
    }
    if let Some(list) = top_rot {
        maps.rotation = list;
    }
    if let Some(flag) = srv_vote {
        maps.map_vote = flag;
    }
    if let Some(list) = srv_rot {
        maps.rotation = list;
    }
    maps.retain_known_order();
    Some(maps)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Top,
    Server,
    Other,
}

fn strip_hash_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_string = !in_string;
        } else if bytes[i] == b'#' && !in_string {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn parse_string_array(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !value.starts_with('[') || !value.ends_with(']') {
        return None;
    }
    let inner = value[1..value.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let text = part.strip_prefix('"')?.strip_suffix('"')?;
        out.push(text.trim().to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotation() -> ServerMaps {
        ServerMaps::parse(
            r#"
[server]
rotation = ["mesa", "canyon", "ridge", "flats"]
map_vote = true
"#,
        )
    }

    #[test]
    fn tally_picks_the_most_voted_option() {
        let config = rotation();
        let mut vote = config.open_vote("mesa", 0).expect("vote is on");
        assert_eq!(vote.options, vec!["canyon", "ridge", "flats"]);
        vote.cast_vote(1, 2, 0).expect("ballot");
        vote.cast_vote(2, 1, 0).expect("ballot");
        vote.cast_vote(3, 2, 0).expect("ballot");
        assert_eq!(vote.vote_count(), 3);
        assert_eq!(vote.tally(), vec![0, 1, 2]);
        assert_eq!(vote.winner(), Some(2));
        assert_eq!(vote.winner_map(), Some("flats"));
    }

    #[test]
    fn ties_go_to_rotation_order() {
        let config = rotation();
        let mut vote = config.open_vote("mesa", 100).expect("vote is on");
        vote.cast_vote(1, 2, 100).expect("ballot");
        vote.cast_vote(2, 0, 100).expect("ballot");
        vote.cast_vote(3, 2, 101).expect("ballot");
        vote.cast_vote(4, 0, 101).expect("ballot");
        assert_eq!(vote.tally(), vec![2, 0, 2]);
        assert_eq!(vote.winner(), Some(0), "ties keep rotation order");
        assert_eq!(vote.winner_map(), Some("canyon"));
    }

    #[test]
    fn vote_closes_after_twenty_seconds_and_late_votes_are_refused() {
        assert_eq!(MAP_VOTE_DURATION_TICKS, 1200);
        let config = rotation();
        let mut vote = config.open_vote("mesa", 0).expect("vote is on");
        assert!(vote.is_open(0));
        assert!(vote.is_open(MAP_VOTE_DURATION_TICKS - 1));
        assert!(!vote.is_open(MAP_VOTE_DURATION_TICKS));
        assert!(vote.expired(MAP_VOTE_DURATION_TICKS));
        assert_eq!(
            vote.cast_vote(1, 0, MAP_VOTE_DURATION_TICKS),
            Err(VoteReject::Closed)
        );
        assert_eq!(vote.vote_count(), 0);
        assert_eq!(vote.winner(), Some(0));
        assert_eq!(vote.winner_map(), Some("canyon"));
    }

    #[test]
    fn map_vote_is_off_by_default() {
        assert!(!ServerMaps::default().map_vote);
        assert!(ServerMaps::default().rotation.is_empty());
        assert_eq!(ServerMaps::default().option_count, MAP_VOTE_OPTIONS);

        let missing_key = ServerMaps::parse("[server]\nrotation = [\"a\", \"b\"]\n");
        assert!(!missing_key.map_vote, "absent key stays off");
        assert!(missing_key.open_vote("a", 0).is_none(), "no vote while off");

        let empty = ServerMaps::parse("");
        assert!(!empty.map_vote);
        assert!(empty.open_vote("a", 0).is_none());

        let broken = ServerMaps::parse("rotation = [");
        assert!(!broken.map_vote);
        assert!(broken.rotation.is_empty());

        let on = rotation();
        assert!(on.map_vote);
        assert!(on.open_vote("mesa", 0).is_some());

        let both = ServerMaps::parse(
            "map_vote = false\nrotation = [\"mesa\"]\n[server]\nmap_vote = true\nrotation = [\"canyon\", \"ridge\"]\n",
        );
        assert!(both.map_vote);
        assert_eq!(both.rotation, vec!["canyon".to_string(), "ridge".to_string()]);
    }

    #[test]
    fn one_vote_per_player_with_the_latest_pick_counting() {
        let config = rotation();
        let mut vote = config.open_vote("mesa", 0).expect("vote is on");
        vote.cast_vote(7, 0, 0).expect("ballot");
        vote.cast_vote(7, 1, 1).expect("ballot");
        assert_eq!(vote.vote_count(), 1, "still one ballot for that player");
        assert_eq!(vote.tally(), vec![0, 1, 0]);
        assert_eq!(vote.cast_vote(8, 9, 1), Err(VoteReject::BadOption));
        assert_eq!(vote.vote_count(), 1, "a bad option adds nothing");
    }

    #[test]
    fn next_options_follow_rotation_order_and_wrap() {
        let config = rotation();
        assert_eq!(
            config.next_options("mesa"),
            vec!["canyon".to_string(), "ridge".to_string(), "flats".to_string()]
        );
        assert_eq!(
            config.next_options("ridge"),
            vec!["flats".to_string(), "mesa".to_string(), "canyon".to_string()]
        );
        assert_eq!(
            config.next_options("unknown_map"),
            vec!["mesa".to_string(), "canyon".to_string(), "ridge".to_string()]
        );

        let short = ServerMaps::parse("[server]\nrotation = [\"a\", \"b\"]\nmap_vote = true\n");
        assert_eq!(short.next_options("a"), vec!["b".to_string(), "a".to_string()]);

        let blanks =
            ServerMaps::parse("[server]\nrotation = [\"mesa\", \"\", \"canyon\"]\nmap_vote = true\n");
        assert_eq!(blanks.rotation, vec!["mesa".to_string(), "canyon".to_string()]);

        let empty = ServerMaps::default();
        assert!(empty.next_options("a").is_empty());
        assert!(
            empty.open_vote("a", 0).is_none(),
            "no maps means no vote even when on"
        );
        assert!(
            ServerMaps::load(Path::new("/no/such/regia-server-maps.toml"))
                .rotation
                .is_empty()
        );
    }
}
