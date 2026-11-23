// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/match_flow.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 78-81, 285-295 and 323-331: setting the playlist and taking the next row on a
// launch). Named issues: the playlist is its own value instead of a field of the game's campaign
// book, a row's game-specific plan (the sky) became free-form options, and looping is a setting.
// The game's own map list and its alpha rows are not carried.

//! The host's playlist: the maps and modes a session plays, one row per match.
//!
//! A playlist plays its rows in order by default (S-14). [`PlaylistOrder::Shuffle`] plays every row
//! once per cycle in an order drawn from a [`DetRng`] stream, and [`PlaylistOrder::Weighted`] plays
//! each row [`PlaylistEntry::weight`] times per cycle, so lockstep peers with the same seed agree
//! on every row. [`Playlist::take_next_for`] skips rows outside their player gate or with missing
//! content and reports each skip with a [`SkipReason`] (ADR-8897).

use std::collections::BTreeMap;

use regia_detsim::rng::DetRng;
use serde::{Deserialize, Serialize};

use super::mode::MatchMode;

/// The largest [`PlaylistEntry::weight`] a weighted cycle honors. A larger weight plays as this
/// many times per cycle, so a cycle holds at most `rows * MAX_ROW_WEIGHT` plays.
pub const MAX_ROW_WEIGHT: u32 = 100;

fn default_weight() -> u32 {
    1
}

fn is_default_weight(weight: &u32) -> bool {
    *weight == 1
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// One playlist row: a map and a mode, plus game options.
///
/// Unknown fields are refused (a `.playlist.ron` row, ADR-8553): a misspelled key is an error that
/// names it, never a silent default. `weight`, `min_players` and `max_players` (ADR-8897) are
/// optional: a row without them loads as before, plays once per cycle and has no player gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistEntry {
    /// The map (level) id the game loads.
    pub map_id: String,
    /// The mode string: a [`MatchMode`] lobby id (`"ctf"`, ...), a rule-set id or a game's own mode.
    pub mode: String,
    /// Game options for this row (time of day, limits, ...), in key order.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Plays per cycle under [`PlaylistOrder::Weighted`] (default 1; 0 leaves the row out of the
    /// weighted cycle; above [`MAX_ROW_WEIGHT`] plays as that). The other orders ignore it.
    #[serde(default = "default_weight", skip_serializing_if = "is_default_weight")]
    pub weight: u32,
    /// The row is skipped while fewer players are present. `None` reads the `min_players` option,
    /// and no option is no minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_players: Option<u32>,
    /// The row is skipped while more players are present. `None` reads the `max_players` option,
    /// and no option is no maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_players: Option<u32>,
}

impl Default for PlaylistEntry {
    fn default() -> Self {
        Self {
            map_id: String::new(),
            mode: String::new(),
            options: BTreeMap::new(),
            weight: 1,
            min_players: None,
            max_players: None,
        }
    }
}

impl PlaylistEntry {
    /// A row for `map_id` and `mode` without options, weight 1 and no player gate.
    pub fn new(map_id: impl Into<String>, mode: impl Into<String>) -> Self {
        Self {
            map_id: map_id.into(),
            mode: mode.into(),
            ..Self::default()
        }
    }

    /// The row's mode as a skeleton mode: a known lobby id, else [`MatchMode::Custom`].
    pub fn match_mode(&self) -> MatchMode {
        MatchMode::from_lobby_str(&self.mode).unwrap_or(MatchMode::Custom)
    }

    /// The row's minimum player count: the field, else the `min_players` option.
    pub fn min_players_bound(&self) -> Option<u32> {
        self.min_players.or_else(|| option_u32(self, "min_players"))
    }

    /// The row's maximum player count: the field, else the `max_players` option.
    pub fn max_players_bound(&self) -> Option<u32> {
        self.max_players.or_else(|| option_u32(self, "max_players"))
    }

    /// Why the row does not fit `players`, or `None` when it does (both bounds inclusive).
    pub fn player_gate(&self, players: u32) -> Option<SkipReason> {
        if self.min_players_bound().is_some_and(|min| players < min) {
            return Some(SkipReason::TooFewPlayers);
        }
        if self.max_players_bound().is_some_and(|max| players > max) {
            return Some(SkipReason::TooManyPlayers);
        }
        None
    }
}

/// How a playlist orders its rows (ADR-8897).
///
/// Every order draws only from [`DetRng`] streams of its seed, so two peers with the same rows,
/// settings and history take the same rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistOrder {
    /// The rows in list order (S-14's behavior, the default).
    #[default]
    InOrder,
    /// Every row once per cycle in a fresh order per cycle: cycle `n` is a Fisher-Yates shuffle by
    /// stream `n` of `DetRng::seed_from(seed)`. The last row played never opens the next cycle
    /// while another row fits (the repeat guard is at least 1).
    Shuffle {
        /// The generator seed every peer shares.
        seed: u64,
    },
    /// Each row [`PlaylistEntry::weight`] times per cycle, the cycle shuffled as for
    /// [`PlaylistOrder::Shuffle`]: over one cycle every row plays exactly its weight.
    Weighted {
        /// The generator seed every peer shares.
        seed: u64,
    },
}

impl PlaylistOrder {
    /// True for [`PlaylistOrder::InOrder`].
    pub fn is_in_order(&self) -> bool {
        matches!(self, Self::InOrder)
    }

    /// The order's name: `in_order`, `shuffle` or `weighted`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InOrder => "in_order",
            Self::Shuffle { .. } => "shuffle",
            Self::Weighted { .. } => "weighted",
        }
    }
}

/// Why the rotation passed over a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Fewer players than the row's minimum.
    TooFewPlayers,
    /// More players than the row's maximum.
    TooManyPlayers,
    /// The host reported the row's map or rule set missing from the installed content.
    MissingContent,
    /// The row is one of the last [`Playlist::no_repeat_last`] rows played (in-order lists only;
    /// a shuffled cycle moves such a row later instead of passing it).
    Recent,
}

impl SkipReason {
    /// The stable code: `too_few_players`, `too_many_players`, `missing_content` or `recent`.
    pub fn code(self) -> &'static str {
        match self {
            Self::TooFewPlayers => "too_few_players",
            Self::TooManyPlayers => "too_many_players",
            Self::MissingContent => "missing_content",
            Self::Recent => "recent",
        }
    }
}

/// One row the rotation passed over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSkip {
    /// The row's index in [`Playlist::entries`].
    pub index: u32,
    /// Why.
    pub reason: SkipReason,
}

/// What [`Playlist::take_next_for`] took.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RotationPick {
    /// `(row index, row)`, or `None` when no row fits (the playlist state is unchanged and the
    /// session reruns its current row).
    pub row: Option<(u32, PlaylistEntry)>,
    /// Rows passed over, each row once, in walk order. When no row fits they explain why.
    pub skipped: Vec<RowSkip>,
}

/// The rows, the order and the rotation's position.
///
/// The fields after `looping` are new in ADR-8897 and are not serialized while at their defaults,
/// so an in-order playlist keeps S-14's shape.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Playlist {
    /// The rows, in list order.
    pub entries: Vec<PlaylistEntry>,
    /// In order: the next row to start. Shuffled or weighted: the next position in
    /// [`Playlist::cycle`].
    pub index: u32,
    /// Start over after the last row (in order) or the last play of a cycle (a new cycle).
    #[serde(default)]
    pub looping: bool,
    /// How the rows are ordered. Default [`PlaylistOrder::InOrder`].
    #[serde(default, skip_serializing_if = "PlaylistOrder::is_in_order")]
    pub order: PlaylistOrder,
    /// The rotation avoids the last `n` rows played while another row fits (a soft guard: when
    /// only recent rows fit, the first of them plays). `0` (the default) is no guard; a shuffled
    /// order guards at least the last row.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub no_repeat_last: u8,
    /// The playlist's id, published as the `regia.playlist` lobby key. Empty: no key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Shuffled or weighted: the current cycle's plays (row indices). Empty until the first take.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cycle: Vec<u32>,
    /// Shuffled or weighted: the current cycle's number, the [`DetRng`] stream it was drawn from.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cycle_number: u32,
    /// The last rows taken, oldest first, as many as the repeat guard needs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<u32>,
}

/// One scan's answer: the chosen position in the scanned sequence and the rows passed over.
struct Scan {
    chosen: Option<usize>,
    skipped: Vec<(usize, RowSkip)>,
}

impl Playlist {
    /// Replace the rows and start from the first one (a fresh cycle, no recent rows).
    pub fn set(&mut self, entries: Vec<PlaylistEntry>) {
        self.entries = entries;
        self.index = 0;
        self.cycle.clear();
        self.cycle_number = 0;
        self.recent.clear();
    }

    /// Mark row `row` as playing now (a host that started on it): in order, the next take is the
    /// following row; shuffled or weighted, the first cycle starts fresh and the repeat guard
    /// knows the row.
    pub fn start_at(&mut self, row: u32) {
        self.cycle.clear();
        self.cycle_number = 0;
        self.recent.clear();
        if self.order.is_in_order() {
            self.index = row.saturating_add(1);
        } else {
            self.index = 0;
        }
        self.note_taken(row);
    }

    /// The row the next [`Playlist::take_next`] starts, if any.
    pub fn peek(&self) -> Option<&PlaylistEntry> {
        self.peek_index()
            .and_then(|index| self.entries.get(index as usize))
    }

    /// The index of the row the next [`Playlist::take_next`] starts, without taking it.
    pub fn peek_index(&self) -> Option<u32> {
        let mut copy = self.clone();
        copy.take(None, &mut |_| true).row.map(|(index, _)| index)
    }

    /// Take the next row for a launch with no player gate and every row's content present:
    /// `(row index, row)`, and move past it. `None` when no row is left (the state stays).
    pub fn take_next(&mut self) -> Option<(u32, PlaylistEntry)> {
        self.take(None, &mut |_| true).row
    }

    /// Take the next row that fits `players` and whose content `available` reports present.
    ///
    /// Rows outside their player gate ([`PlaylistEntry::player_gate`]) or refused by `available`
    /// are passed over and listed in [`RotationPick::skipped`]. In order, passed rows are behind
    /// the rotation until it comes around again. In a cycle, a passed row is dropped from that
    /// cycle. Rows among the last [`Playlist::no_repeat_last`] are avoided while another row fits.
    ///
    /// When no row fits, the pick holds no row, the playlist state is unchanged, and the skips
    /// say why: the session then reruns its current row, as at the end of a list that does not
    /// loop. A list that does not loop ends after its last row (in order) or its one cycle.
    pub fn take_next_for<F>(&mut self, players: u32, mut available: F) -> RotationPick
    where
        F: FnMut(&PlaylistEntry) -> bool,
    {
        self.take(Some(players), &mut available)
    }

    /// The index of the first row with `map_id` and `mode`.
    pub fn position(&self, map_id: &str, mode: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|row| row.map_id == map_id && row.mode == mode)
    }

    /// The next `n` rows eligible for `players`, in the order the rotation would reach them.
    ///
    /// A row is eligible when `players` is inside its player gate ([`PlaylistEntry::player_gate`];
    /// an absent bound is no limit), the same rule [`Playlist::take_next_for`] uses. A row index
    /// is never repeated, so a short or filtered list returns fewer than `n`. `players` of 0 keeps
    /// every row that sets no minimum. In order, the walk starts at [`Playlist::index`] and wraps
    /// at most once, and rows the repeat guard holds back ([`Playlist::no_repeat_last`]) come
    /// after the other rows, as a launch passes them (ADR-8881); shuffled or weighted, it is the
    /// walk [`Playlist::take_next_for`] takes on a copy of the playlist: the rest of the cycle,
    /// then (looping) the next cycle, with the repeat guard applied where the launches apply it,
    /// so at a cycle boundary the first option is the row the next launch plays. The map vote and
    /// the rotation share this rule (ADR-8897).
    pub fn vote_candidates(&self, n: usize, players: u32) -> Vec<PlaylistEntry> {
        self.vote_candidates_for(n, players, |_| true)
    }

    /// [`Playlist::vote_candidates`] that also leaves out rows `available` refuses.
    pub fn vote_candidates_for<F>(&self, n: usize, players: u32, mut available: F) -> Vec<PlaylistEntry>
    where
        F: FnMut(&PlaylistEntry) -> bool,
    {
        let len = self.entries.len();
        if n == 0 || len == 0 {
            return Vec::new();
        }
        if !self.order.is_in_order() {
            return self.vote_candidates_cycled(n, players, &mut available);
        }
        let start = if self.index as usize >= len {
            0
        } else {
            self.index as usize
        };
        let sequence: Vec<u32> = (0..len).map(|step| ((start + step) % len) as u32).collect();
        // The repeat guard the launches apply (ADR-8881): rows among the last `no_repeat_last`
        // played come after every other eligible row, each part in walk order, so the first
        // option is the row the next launch plays.
        let guard = self.repeat_guard();
        let mut seen: Vec<u32> = Vec::new();
        let mut fresh: Vec<PlaylistEntry> = Vec::new();
        let mut recent: Vec<PlaylistEntry> = Vec::new();
        for row in sequence {
            if fresh.len() >= n {
                break;
            }
            if let Err(at) = seen.binary_search(&row) {
                seen.insert(at, row);
                let Some(entry) = self.entries.get(row as usize) else {
                    continue;
                };
                if entry.player_gate(players).is_none() && available(entry) {
                    if self.is_recent(row, guard) {
                        recent.push(entry.clone());
                    } else {
                        fresh.push(entry.clone());
                    }
                }
            }
        }
        fresh.extend(recent);
        fresh.truncate(n);
        fresh
    }

    /// Shuffled or weighted ballot: take from a copy with [`Playlist::take_next_for`] until `n`
    /// distinct rows came up, the walk ends, or it has gone through the rest of this cycle and
    /// one whole next cycle (the cap that also ends a cycle of one row repeated by its weight).
    fn vote_candidates_cycled(
        &self,
        n: usize,
        players: u32,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> Vec<PlaylistEntry> {
        let remaining = if self.cycle.is_empty() {
            self.plays_per_cycle()
        } else {
            self.cycle.len().saturating_sub(self.index as usize)
        };
        let cap = remaining.saturating_add(self.plays_per_cycle()).saturating_add(1);
        let mut walk = self.clone();
        let mut seen: Vec<u32> = Vec::new();
        let mut out = Vec::new();
        for _ in 0..cap {
            if out.len() >= n {
                break;
            }
            let Some((row, entry)) = walk.take(Some(players), &mut *available).row else {
                break;
            };
            if let Err(at) = seen.binary_search(&row) {
                seen.insert(at, row);
                out.push(entry);
            }
        }
        out
    }

    /// Plays in one cycle of the current order: one per row for a shuffle, the capped weights for
    /// a weighted order, 0 in order. Equals `build_cycle(_).len()` without drawing it.
    fn plays_per_cycle(&self) -> usize {
        match self.order {
            PlaylistOrder::InOrder => 0,
            PlaylistOrder::Shuffle { .. } => self.entries.len(),
            PlaylistOrder::Weighted { .. } => self
                .entries
                .iter()
                .map(|row| row.weight.min(MAX_ROW_WEIGHT) as usize)
                .sum(),
        }
    }

    /// The repeat guard in effect: [`Playlist::no_repeat_last`], at least 1 for a shuffle.
    pub fn repeat_guard(&self) -> usize {
        let guard = usize::from(self.no_repeat_last);
        match self.order {
            PlaylistOrder::Shuffle { .. } => guard.max(1),
            _ => guard,
        }
    }

    /// Cycle `number`'s plays for the current order (empty in order).
    pub fn build_cycle(&self, number: u32) -> Vec<u32> {
        let (seed, weighted) = match self.order {
            PlaylistOrder::InOrder => return Vec::new(),
            PlaylistOrder::Shuffle { seed } => (seed, false),
            PlaylistOrder::Weighted { seed } => (seed, true),
        };
        let mut plays: Vec<u32> = Vec::new();
        for (index, row) in self.entries.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            let count = if weighted {
                row.weight.min(MAX_ROW_WEIGHT)
            } else {
                1
            };
            for _ in 0..count {
                plays.push(index);
            }
        }
        let mut rng = DetRng::seed_from(seed).stream(u64::from(number));
        rng.shuffle(&mut plays);
        plays
    }

    /// The rest of the current cycle (built first when none was drawn yet). Stale plays (rows
    /// since removed) are dropped. The next cycle is `build_cycle(cycle_number + 1)`, drawn only
    /// when the rest of this one has no row that fits.
    fn current_segment(&self) -> Vec<u32> {
        let len = self.entries.len() as u32;
        let current = if self.cycle.is_empty() {
            self.build_cycle(self.cycle_number)
        } else {
            let from = (self.index as usize).min(self.cycle.len());
            self.cycle[from..].to_vec()
        };
        current.into_iter().filter(|row| *row < len).collect()
    }

    fn is_recent(&self, row: u32, guard: usize) -> bool {
        guard > 0 && self.recent.iter().rev().take(guard).any(|recent| *recent == row)
    }

    fn note_taken(&mut self, row: u32) {
        let keep = self.repeat_guard();
        if keep == 0 {
            self.recent.clear();
            return;
        }
        self.recent.push(row);
        if self.recent.len() > keep {
            let extra = self.recent.len() - keep;
            self.recent.drain(..extra);
        }
    }

    /// Scan `sequence` for the first row that fits and is not recent under `guard`. Rows that do
    /// not fit are listed; with `report_recent`, so are recent rows passed over.
    fn scan(
        &self,
        sequence: &[u32],
        guard: usize,
        report_recent: bool,
        players: Option<u32>,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> Scan {
        let mut skipped = Vec::new();
        for (position, row) in sequence.iter().copied().enumerate() {
            let Some(entry) = self.entries.get(row as usize) else {
                continue;
            };
            let gate = players.and_then(|players| entry.player_gate(players));
            let reason = match gate {
                Some(reason) => Some(reason),
                None if !available(entry) => Some(SkipReason::MissingContent),
                None => None,
            };
            if let Some(reason) = reason {
                skipped.push((position, RowSkip { index: row, reason }));
                continue;
            }
            if self.is_recent(row, guard) {
                if report_recent {
                    skipped.push((
                        position,
                        RowSkip {
                            index: row,
                            reason: SkipReason::Recent,
                        },
                    ));
                }
                continue;
            }
            return Scan {
                chosen: Some(position),
                skipped,
            };
        }
        Scan {
            chosen: None,
            skipped,
        }
    }

    /// Scan with the guard, then without it. The rows passed over come from the scan that chose.
    fn scan_guarded(
        &self,
        sequence: &[u32],
        report_recent: bool,
        players: Option<u32>,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> Scan {
        let guard = self.repeat_guard();
        if guard > 0 {
            let guarded = self.scan(sequence, guard, report_recent, players, available);
            if guarded.chosen.is_some() {
                return guarded;
            }
        }
        self.scan(sequence, 0, false, players, available)
    }

    fn take(
        &mut self,
        players: Option<u32>,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> RotationPick {
        let _span = tracing::trace_span!("playlist_take").entered();
        if self.order.is_in_order() {
            self.take_in_order(players, available)
        } else {
            self.take_cycled(players, available)
        }
    }

    fn take_in_order(
        &mut self,
        players: Option<u32>,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> RotationPick {
        let len = self.entries.len();
        let mut start = self.index as usize;
        if self.looping && start >= len {
            start = 0;
        }
        if start >= len {
            return RotationPick::default();
        }
        let steps = if self.looping { len } else { len - start };
        let sequence: Vec<u32> = (0..steps).map(|step| ((start + step) % len) as u32).collect();
        let scan = self.scan_guarded(&sequence, true, players, available);
        let skipped = unique_skips(scan.skipped.into_iter().map(|(_, skip)| skip));
        let Some(position) = scan.chosen else {
            return RotationPick { row: None, skipped };
        };
        let row = sequence[position];
        self.index = row.saturating_add(1);
        self.note_taken(row);
        RotationPick {
            row: self.entries.get(row as usize).cloned().map(|entry| (row, entry)),
            skipped,
        }
    }

    fn take_cycled(
        &mut self,
        players: Option<u32>,
        available: &mut dyn FnMut(&PlaylistEntry) -> bool,
    ) -> RotationPick {
        let current = self.current_segment();
        let fresh = self.cycle.is_empty();
        let mut skipped: Vec<RowSkip> = Vec::new();
        let scan = self.scan_guarded(&current, false, players, available);
        if let Some(position) = scan.chosen {
            skipped.extend(scan.skipped.iter().map(|(_, skip)| skip.clone()));
            let tail = reorder_tail(&current, position, &scan.skipped);
            if fresh {
                self.cycle = tail;
                self.index = 0;
            } else {
                let from = (self.index as usize).min(self.cycle.len());
                self.cycle.truncate(from);
                self.cycle.extend(tail);
            }
            return self.commit_cycled(skipped);
        }
        skipped.extend(scan.skipped.into_iter().map(|(_, skip)| skip));
        if !self.looping {
            return RotationPick {
                row: None,
                skipped: unique_skips(skipped),
            };
        }
        let next = self.build_cycle(self.cycle_number.saturating_add(1));
        let scan = self.scan_guarded(&next, false, players, available);
        skipped.extend(scan.skipped.iter().map(|(_, skip)| skip.clone()));
        let Some(position) = scan.chosen else {
            return RotationPick {
                row: None,
                skipped: unique_skips(skipped),
            };
        };
        // Whether the current cycle was drawn just now or earlier, the next one is its successor.
        self.cycle_number = self.cycle_number.saturating_add(1);
        self.cycle = reorder_tail(&next, position, &scan.skipped);
        self.index = 0;
        self.commit_cycled(skipped)
    }

    /// Take the play at [`Playlist::index`] of the (reordered) cycle.
    fn commit_cycled(&mut self, skipped: Vec<RowSkip>) -> RotationPick {
        let Some(row) = self.cycle.get(self.index as usize).copied() else {
            return RotationPick {
                row: None,
                skipped: unique_skips(skipped),
            };
        };
        self.index = self.index.saturating_add(1);
        self.note_taken(row);
        RotationPick {
            row: self.entries.get(row as usize).cloned().map(|entry| (row, entry)),
            skipped: unique_skips(skipped),
        }
    }
}

/// The new cycle tail from `sequence`: the chosen play first, then the passed plays that fit
/// (recent rows, moved later), then the plays after the chosen one. Plays that did not fit are
/// dropped from the cycle.
fn reorder_tail(sequence: &[u32], chosen: usize, dropped: &[(usize, RowSkip)]) -> Vec<u32> {
    let mut tail = Vec::with_capacity(sequence.len());
    tail.push(sequence[chosen]);
    for (position, row) in sequence.iter().enumerate().take(chosen) {
        if !dropped.iter().any(|(at, _)| *at == position) {
            tail.push(*row);
        }
    }
    tail.extend_from_slice(&sequence[chosen + 1..]);
    tail
}

/// Each row once, the first reason kept, in walk order.
fn unique_skips(skips: impl IntoIterator<Item = RowSkip>) -> Vec<RowSkip> {
    let mut out: Vec<RowSkip> = Vec::new();
    for skip in skips {
        if !out.iter().any(|seen| seen.index == skip.index) {
            out.push(skip);
        }
    }
    out
}

fn option_u32(row: &PlaylistEntry, key: &str) -> Option<u32> {
    let text = row.options.get(key)?;
    text.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_play_in_order_and_stop_at_the_end() {
        let mut list = Playlist::default();
        list.set(vec![
            PlaylistEntry::new("cafe", "tdm"),
            PlaylistEntry::new("hq", "search"),
        ]);
        assert_eq!(list.peek().map(|r| r.map_id.as_str()), Some("cafe"));
        assert_eq!(
            list.take_next().map(|(i, r)| (i, r.map_id)),
            Some((0, "cafe".into()))
        );
        assert_eq!(
            list.take_next().map(|(i, r)| (i, r.mode)),
            Some((1, "search".into()))
        );
        assert_eq!(list.take_next(), None);
        assert_eq!(list.index, 2, "a missing row leaves the index");
        assert_eq!(list.position("hq", "search"), Some(1));
        assert_eq!(list.position("hq", "tdm"), None);
    }

    #[test]
    fn a_looping_playlist_starts_over() {
        let mut list = Playlist {
            looping: true,
            ..Playlist::default()
        };
        list.set(vec![
            PlaylistEntry::new("a", "ctf"),
            PlaylistEntry::new("b", "koth"),
        ]);
        let order: Vec<u32> = (0..5).filter_map(|_| list.take_next().map(|(i, _)| i)).collect();
        assert_eq!(order, [0, 1, 0, 1, 0]);
        assert_eq!(
            Playlist {
                looping: true,
                ..Playlist::default()
            }
            .take_next(),
            None
        );
    }

    #[test]
    fn a_row_maps_its_mode_string_onto_a_skeleton() {
        assert_eq!(
            PlaylistEntry::new("m", "CTF").match_mode(),
            MatchMode::CaptureTheFlag
        );
        assert_eq!(
            PlaylistEntry::new("m", "koth").match_mode(),
            MatchMode::KingOfTheHill
        );
        assert_eq!(PlaylistEntry::new("m", "siege").match_mode(), MatchMode::Custom);
    }

    #[test]
    fn vote_candidates_walk_once_and_honor_player_bounds() {
        let mut list = Playlist::default();
        let mut small = PlaylistEntry::new("mesa", "tdm");
        small.options.insert("max_players".into(), "4".into());
        let mut large = PlaylistEntry::new("canyon", "tdm");
        large.options.insert("min_players".into(), "8".into());
        list.set(vec![PlaylistEntry::new("ridge", "tdm"), small, large]);
        list.index = 1;
        let ids: Vec<String> = list
            .vote_candidates(3, 0)
            .into_iter()
            .map(|row| row.map_id)
            .collect();
        assert_eq!(
            ids,
            vec!["mesa".to_string(), "ridge".to_string()],
            "a minimum excludes players 0, and rows are not repeated"
        );
        let ids: Vec<String> = list
            .vote_candidates(3, 10)
            .into_iter()
            .map(|row| row.map_id)
            .collect();
        assert_eq!(ids, vec!["canyon".to_string(), "ridge".to_string()]);
        assert_eq!(list.vote_candidates(0, 10).len(), 0);
    }
}
