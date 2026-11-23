// SPDX-License-Identifier: MPL-2.0
//! Races (audit gap GR-11.2): an ordered checkpoint course with branches, laps, per-racer
//! progress, a live position ranking, wrong-way and missed-checkpoint detection, the finish order
//! with its tie breaks, a DNF window, and best laps.
//!
//! Positions arrive as `f32` meters (the session's [`Contestant`]s). The race converts each one to
//! whole millimeters once (`as i64`, which saturates and maps NaN to 0) and does the rest in
//! integers: distance along a segment is a dot product, and two fractions compare by cross
//! multiplication in `i128`. No square root, no division of positions, sorted iteration only.
//!
//! A course with no checkpoints (the default) is off: the session never runs this module.

use serde::{Deserialize, Serialize};

use crate::RulesError;
use crate::match_flow::SeatId;
use crate::objective::{Contestant, HillVolume, RulesEvent};
use crate::win_condition::Outcome;

/// Most checkpoints a course holds.
pub const MAX_CHECKPOINTS: usize = 256;

/// Most `next` links one checkpoint holds.
pub const MAX_NEXT: usize = 8;

/// Most laps a race may run.
pub const MAX_LAPS: u32 = 1000;

/// Most racers one race tracks.
pub const MAX_RACERS: usize = 256;

/// The ledger counter that holds each racer's best lap, in match ticks.
pub const BEST_LAP_COUNTER: &str = "best_lap_ticks";

/// One checkpoint gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Course-unique id.
    pub id: u32,
    /// The gate volume. A racer passes the checkpoint on the step it enters the volume.
    pub volume: HillVolume,
    /// Ids that may follow this one. More than one is a branch; the course start closes a lap.
    pub next: Vec<u32>,
}

/// Race settings (`SessionSettings::race`). An empty course is off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RaceSettings {
    /// The course. The first checkpoint is the start and finish line. List branch alternates in
    /// the order the ranking should prefer (the ranking compares list indexes).
    pub checkpoints: Vec<Checkpoint>,
    /// Laps to finish. At least 1.
    pub laps: u32,
    /// Once the first racer finishes, every racer still running this many match ticks later is
    /// DNF. `0` waits for every racer.
    pub finish_window_ticks: u64,
    /// Consecutive steps a racer must lose ground on its segment before it is flagged wrong-way.
    /// `0` turns the check off.
    pub wrong_way_ticks: u64,
}

impl Default for RaceSettings {
    fn default() -> Self {
        Self {
            checkpoints: Vec::new(),
            laps: 1,
            finish_window_ticks: 0,
            wrong_way_ticks: 30,
        }
    }
}

fn bad(reason: &'static str) -> RulesError {
    RulesError::RaceSettings { reason }
}

impl RaceSettings {
    /// True when the course is empty.
    pub fn is_off(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// Refuse a course the race cannot run ([`RulesError::RaceSettings`]).
    pub fn validate(&self) -> Result<(), RulesError> {
        if self.is_off() {
            return Ok(());
        }
        if self.checkpoints.len() < 2 {
            return Err(bad("too_few_checkpoints"));
        }
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            return Err(bad("too_many_checkpoints"));
        }
        if self.laps == 0 || self.laps > MAX_LAPS {
            return Err(bad("laps_range"));
        }
        let mut ids: Vec<u32> = self.checkpoints.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(bad("duplicate_id"));
        }
        for checkpoint in &self.checkpoints {
            if checkpoint.next.is_empty() || checkpoint.next.len() > MAX_NEXT {
                return Err(bad("next_count"));
            }
            if checkpoint.next.iter().any(|id| ids.binary_search(id).is_err()) {
                return Err(bad("next_unknown"));
            }
            if checkpoint.next.contains(&checkpoint.id) {
                return Err(bad("next_self"));
            }
            let v = &checkpoint.volume;
            if v.min
                .iter()
                .zip(v.max.iter())
                .any(|(lo, hi)| !lo.is_finite() || !hi.is_finite() || lo > hi)
            {
                return Err(bad("volume_bad"));
            }
        }
        Ok(())
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.checkpoints.iter().position(|c| c.id == id)
    }

    /// The id of the start and finish checkpoint.
    pub fn start_id(&self) -> Option<u32> {
        self.checkpoints.first().map(|c| c.id)
    }
}

/// A non-negative fraction `num / den` with `den > 0`, compared exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fraction {
    /// Numerator.
    pub num: i64,
    /// Denominator, above 0.
    pub den: i64,
}

impl Default for Fraction {
    fn default() -> Self {
        Self { num: 0, den: 1 }
    }
}

impl Fraction {
    fn new(num: i128, den: i128) -> Self {
        if den <= 0 {
            return Self::default();
        }
        // Keep the value; scale both down together when either leaves i64 (a course larger than
        // about 1700 km). Halving both keeps the comparison exact to within one part in 2^62.
        let (mut num, mut den) = (num, den);
        while num.abs() > i128::from(i64::MAX) || den > i128::from(i64::MAX) {
            num /= 2;
            den /= 2;
        }
        Self {
            num: i64::try_from(num).unwrap_or(0),
            den: i64::try_from(den).unwrap_or(1).max(1),
        }
    }

    /// Exact order: `self` against `other`.
    pub fn cmp_exact(&self, other: &Self) -> std::cmp::Ordering {
        let left = i128::from(self.num) * i128::from(other.den);
        let right = i128::from(other.num) * i128::from(self.den);
        left.cmp(&right)
    }
}

/// One racer's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RacerProgress {
    /// The racer's seat.
    pub seat: SeatId,
    /// The unit id whose position the race reads.
    pub unit: u32,
    /// Laps completed.
    pub lap: u32,
    /// Id of the last checkpoint passed (the start before the first one).
    pub checkpoint: u32,
    /// Its index in the course list (the ranking key).
    pub checkpoint_index: u32,
    /// Match tick the current lap started.
    pub lap_started_at: u64,
    /// Fraction of the current segment still to run to the nearest next checkpoint (0 at the gate).
    pub remaining: Fraction,
    /// The checkpoint `remaining` measures to.
    pub target: u32,
    /// Signed projection on the segment last step (millimeters squared, not clamped to the
    /// segment, saturated to `i64`), for the wrong-way check.
    pub along: i64,
    /// Consecutive steps the racer lost ground.
    pub backwards: u64,
    /// Flagged wrong-way (cleared when it gains ground again).
    pub wrong_way: bool,
    /// The checkpoint volume the racer is inside, for edge detection.
    pub inside: Option<u32>,
    /// Laps run so far, in match ticks, oldest first.
    pub lap_times: Vec<u64>,
    /// Best lap in match ticks.
    pub best_lap_ticks: Option<u64>,
    /// Match tick it finished.
    pub finished_at: Option<u64>,
    /// How far past the finish gate it was on its finish step, in millimeters along its last
    /// segment (larger finished earlier).
    pub overshoot: Fraction,
    /// Finish place, from 1.
    pub place: Option<u32>,
    /// Did not finish inside the window.
    pub dnf: bool,
}

/// One row of the live standings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Standing {
    /// Position, from 1.
    pub position: u32,
    /// The racer's seat.
    pub seat: SeatId,
    /// Laps completed.
    pub lap: u32,
    /// Index of the last checkpoint passed.
    pub checkpoint_index: u32,
    /// Fraction of the segment still to run.
    pub remaining: Fraction,
    /// Finish place, when finished.
    pub place: Option<u32>,
    /// Did not finish.
    pub dnf: bool,
    /// Flagged wrong-way.
    pub wrong_way: bool,
    /// Best lap in match ticks.
    pub best_lap_ticks: Option<u64>,
}

/// The race state of the running match. A launch (and a round reset) clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RaceState {
    /// Match tick of the first step the race ran in this match (lap 1 starts here).
    pub opened_at: Option<u64>,
    /// Racers, sorted by seat.
    pub racers: Vec<RacerProgress>,
    /// Places handed out so far.
    pub finished: u32,
    /// Match tick the first racer finished.
    pub first_finish_at: Option<u64>,
    /// Match tick every racer had finished or was DNF.
    pub ended_at: Option<u64>,
}

/// Race settings and state, as the session holds them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Race {
    /// The settings.
    pub settings: RaceSettings,
    /// The running match's state.
    pub state: RaceState,
}

/// Coordinates are clamped to this many millimeters (a million kilometers) either way, so every
/// dot product of two differences fits in `i128` whatever position a host sends.
pub const MAX_MM: i64 = 1_000_000_000_000;

fn mm(meters: f32) -> i64 {
    let raw = (meters * 1000.0) as i64;
    raw.clamp(-MAX_MM, MAX_MM)
}

fn mm3(p: [f32; 3]) -> [i64; 3] {
    [mm(p[0]), mm(p[1]), mm(p[2])]
}

fn center_mm(volume: &HillVolume) -> [i64; 3] {
    let a = mm3(volume.min);
    let b = mm3(volume.max);
    [
        (a[0] + b[0]).div_euclid(2),
        (a[1] + b[1]).div_euclid(2),
        (a[2] + b[2]).div_euclid(2),
    ]
}

fn sub(a: [i64; 3], b: [i64; 3]) -> [i128; 3] {
    [
        i128::from(a[0]) - i128::from(b[0]),
        i128::from(a[1]) - i128::from(b[1]),
        i128::from(a[2]) - i128::from(b[2]),
    ]
}

fn dot(a: [i128; 3], b: [i128; 3]) -> i128 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Remaining fraction of segment `from -> to` at `p`, clamped to `0..=1`, and the signed
/// projection on the segment (millimeters squared), not clamped: it keeps falling behind the
/// segment origin and keeps rising past its end, so the wrong-way check sees every backward step
/// (rules-genre-pve-racing-review finding 2).
fn remaining_on(from: [i64; 3], to: [i64; 3], p: [i64; 3]) -> (Fraction, i128) {
    let seg = sub(to, from);
    let len2 = dot(seg, seg);
    if len2 == 0 {
        return (Fraction::default(), 0);
    }
    let along = dot(sub(p, from), seg);
    (Fraction::new(len2 - along.clamp(0, len2), len2), along)
}

/// How far `p` is beyond the gate at `gate` along the segment `from -> gate`, in millimeters
/// (negative while the racer is still short of the gate's center): the scalar projection
/// `dot(p - gate, seg) / |seg|`, kept as an exact fraction over the integer square root of
/// `|seg|^2`. Finishers on different last segments compare by distance, not by the share of their
/// own segment (rules-genre-pve-racing-review finding 3).
fn past_gate(from: [i64; 3], gate: [i64; 3], p: [i64; 3]) -> Fraction {
    let seg = sub(gate, from);
    let len2 = dot(seg, seg);
    if len2 <= 0 {
        return Fraction::default();
    }
    let len = i128::try_from(len2.unsigned_abs().isqrt()).unwrap_or(i128::MAX);
    Fraction::new(dot(sub(p, gate), seg), len.max(1))
}

/// `value` saturated into `i64`.
fn saturate_i64(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX })
}

impl Race {
    /// A race with `settings` and a fresh state.
    pub fn new(settings: RaceSettings) -> Self {
        Self {
            settings,
            state: RaceState::default(),
        }
    }

    /// True when the course is empty.
    pub fn is_off(&self) -> bool {
        self.settings.is_off()
    }

    /// Forget the running match.
    pub fn reset(&mut self) {
        self.state = RaceState::default();
    }

    /// Enter `seat` driving `unit` at the start line. A seat already racing keeps its progress
    /// and takes the new unit. Past [`MAX_RACERS`] the entry is ignored.
    pub fn enter(&mut self, seat: SeatId, unit: u32) {
        let Some(start) = self.settings.start_id() else {
            return;
        };
        match self.state.racers.binary_search_by_key(&seat, |r| r.seat) {
            Ok(at) => self.state.racers[at].unit = unit,
            Err(at) => {
                if self.state.racers.len() >= MAX_RACERS || self.state.ended_at.is_some() {
                    return;
                }
                let first_next = self
                    .settings
                    .checkpoints
                    .first()
                    .and_then(|c| c.next.first().copied())
                    .unwrap_or(start);
                self.state.racers.insert(
                    at,
                    RacerProgress {
                        seat,
                        unit,
                        lap: 0,
                        checkpoint: start,
                        checkpoint_index: 0,
                        lap_started_at: self.state.opened_at.unwrap_or(0),
                        remaining: Fraction { num: 1, den: 1 },
                        target: first_next,
                        along: 0,
                        backwards: 0,
                        wrong_way: false,
                        inside: None,
                        lap_times: Vec::new(),
                        best_lap_ticks: None,
                        finished_at: None,
                        overshoot: Fraction::default(),
                        place: None,
                        dnf: false,
                    },
                );
            }
        }
    }

    /// `seat` left the match at match tick `tick` (rules-genre-pve-racing-review finding 1): a row
    /// still running is DNF now, with one [`RulesEvent::RacerDnf`], so it never takes a place and
    /// never holds the race open. A finished row keeps its place. The race ends on the next step
    /// once every row is finished or DNF, with or without a first finisher.
    pub fn retire(&mut self, seat: SeatId, tick: u64, push: &mut dyn FnMut(RulesEvent)) {
        if self.state.ended_at.is_some() {
            return;
        }
        let Ok(at) = self.state.racers.binary_search_by_key(&seat, |r| r.seat) else {
            return;
        };
        let racer = &mut self.state.racers[at];
        if racer.finished_at.is_some() || racer.dnf {
            return;
        }
        racer.dnf = true;
        push(RulesEvent::RacerDnf {
            tick,
            team: seat.team,
            slot: seat.slot,
        });
    }

    /// A new player took `seat`: a row a former occupant left there without a place (DNF, or
    /// still running) is dropped, so the newcomer enters at the start line on its own. A row with
    /// a place is kept: places are never taken back.
    pub fn forget_unplaced(&mut self, seat: SeatId) {
        if let Ok(at) = self.state.racers.binary_search_by_key(&seat, |r| r.seat)
            && self.state.racers[at].place.is_none()
        {
            self.state.racers.remove(at);
        }
    }

    /// Seats moved in one batch (`(from, to)`, as the ledger rekeys them): each mover's row
    /// follows it to the new seat with its progress (rules-genre-pve-racing-review finding 1), so
    /// the old seat stops racing and the new one is not entered again at lap 0. A row at a
    /// destination that is not itself moving away is replaced unless it holds a place: places are
    /// never taken back, so a mover onto a seat whose former occupant finished drops its own row
    /// (nothing is left racing at the old seat) and the finished row stays where it is.
    pub fn carry(&mut self, moves: &[(SeatId, SeatId)]) {
        let moves: Vec<(SeatId, SeatId)> = moves.iter().copied().filter(|(from, to)| from != to).collect();
        if moves.is_empty() || self.state.racers.is_empty() {
            return;
        }
        let mut taken: Vec<(SeatId, RacerProgress)> = Vec::new();
        for (from, to) in &moves {
            if let Ok(at) = self.state.racers.binary_search_by_key(from, |r| r.seat) {
                taken.push((*to, self.state.racers.remove(at)));
            }
        }
        let mut landed: Vec<SeatId> = Vec::new();
        for (to, mut row) in taken {
            let found = self.state.racers.binary_search_by_key(&to, |r| r.seat);
            let placed_there = found.is_ok_and(|at| self.state.racers[at].place.is_some());
            if placed_there || landed.contains(&to) {
                continue;
            }
            row.seat = to;
            match found {
                Ok(at) => self.state.racers[at] = row,
                Err(at) => self.state.racers.insert(at, row),
            }
            landed.push(to);
        }
    }

    /// Run one step at match tick `tick` over `units` (the host's deterministic order; a unit id
    /// listed twice uses the first). Events go to `push`, racers in seat order.
    pub fn step(&mut self, tick: u64, units: &[Contestant], push: &mut dyn FnMut(RulesEvent)) {
        if self.is_off() || self.state.ended_at.is_some() {
            return;
        }
        let opened = *self.state.opened_at.get_or_insert(tick);
        let settings = &self.settings;
        let Some(start) = settings.start_id() else {
            return;
        };
        let centers: Vec<[i64; 3]> = settings
            .checkpoints
            .iter()
            .map(|c| center_mm(&c.volume))
            .collect();
        let mut finishers: Vec<usize> = Vec::new();
        for (racer_index, racer) in self.state.racers.iter_mut().enumerate() {
            if racer.finished_at.is_some() || racer.dnf {
                continue;
            }
            if racer.lap == 0 && racer.lap_times.is_empty() && racer.lap_started_at < opened {
                racer.lap_started_at = opened;
            }
            let Some(unit) = units.iter().find(|u| u.id == racer.unit) else {
                continue;
            };
            let p = mm3(unit.position);
            let current = settings.index_of(racer.checkpoint).unwrap_or(0);
            let expected = &settings.checkpoints[current].next;
            // The volume the racer is in: an expected next first, then any other in course order.
            let here = expected
                .iter()
                .copied()
                .find(|id| {
                    settings
                        .index_of(*id)
                        .is_some_and(|i| settings.checkpoints[i].volume.contains(unit.position))
                })
                .or_else(|| {
                    settings
                        .checkpoints
                        .iter()
                        .find(|c| c.volume.contains(unit.position))
                        .map(|c| c.id)
                });
            if here != racer.inside {
                racer.inside = here;
                if let Some(id) = here.filter(|id| *id != racer.checkpoint) {
                    if expected.contains(&id) {
                        let before = current;
                        let index = settings.index_of(id).unwrap_or(0);
                        racer.checkpoint = id;
                        racer.checkpoint_index = u32::try_from(index).unwrap_or(u32::MAX);
                        racer.backwards = 0;
                        racer.wrong_way = false;
                        if id == start {
                            let lap_ticks = tick.saturating_sub(racer.lap_started_at);
                            racer.lap = racer.lap.saturating_add(1);
                            racer.lap_times.push(lap_ticks);
                            let best = racer.best_lap_ticks.is_none_or(|b| lap_ticks < b);
                            if best {
                                racer.best_lap_ticks = Some(lap_ticks);
                            }
                            racer.lap_started_at = tick;
                            push(RulesEvent::RacerLap {
                                tick,
                                team: racer.seat.team,
                                slot: racer.seat.slot,
                                lap: racer.lap,
                                lap_ticks,
                                best,
                            });
                            if racer.lap >= settings.laps {
                                racer.overshoot = past_gate(centers[before], centers[index], p);
                                racer.finished_at = Some(tick);
                                finishers.push(racer_index);
                            }
                        } else {
                            push(RulesEvent::RacerCheckpoint {
                                tick,
                                team: racer.seat.team,
                                slot: racer.seat.slot,
                                checkpoint: id,
                                lap: racer.lap,
                            });
                        }
                    } else {
                        push(RulesEvent::RacerMissedCheckpoint {
                            tick,
                            team: racer.seat.team,
                            slot: racer.seat.slot,
                            checkpoint: id,
                            expected: expected.first().copied().unwrap_or(start),
                        });
                    }
                }
            }
            if racer.finished_at.is_some() {
                continue;
            }
            // Distance to the nearest next gate along its segment, and the wrong-way check.
            let current = settings.index_of(racer.checkpoint).unwrap_or(0);
            let from = centers[current];
            let mut best: Option<(Fraction, i128, u32)> = None;
            for id in &settings.checkpoints[current].next {
                let Some(i) = settings.index_of(*id) else {
                    continue;
                };
                let (rem, along) = remaining_on(from, centers[i], p);
                if best.is_none_or(|(b, _, _)| rem.cmp_exact(&b).is_lt()) {
                    best = Some((rem, along, *id));
                }
            }
            if let Some((rem, along, target)) = best {
                let along = saturate_i64(along);
                if target == racer.target && settings.wrong_way_ticks > 0 {
                    if along < racer.along {
                        racer.backwards = racer.backwards.saturating_add(1);
                        if racer.backwards == settings.wrong_way_ticks && !racer.wrong_way {
                            racer.wrong_way = true;
                            push(RulesEvent::RacerWrongWay {
                                tick,
                                team: racer.seat.team,
                                slot: racer.seat.slot,
                            });
                        }
                    } else if along > racer.along {
                        racer.backwards = 0;
                        racer.wrong_way = false;
                    }
                } else {
                    racer.backwards = 0;
                }
                racer.remaining = rem;
                racer.along = along;
                racer.target = target;
            }
        }
        // This step's finishers: farther past the gate first, then seat.
        finishers.sort_by(|a, b| {
            let (ra, rb) = (&self.state.racers[*a], &self.state.racers[*b]);
            rb.overshoot.cmp_exact(&ra.overshoot).then(ra.seat.cmp(&rb.seat))
        });
        for index in finishers {
            self.state.finished = self.state.finished.saturating_add(1);
            self.state.first_finish_at.get_or_insert(tick);
            let place = self.state.finished;
            let racer = &mut self.state.racers[index];
            racer.place = Some(place);
            push(RulesEvent::RacerFinished {
                tick,
                team: racer.seat.team,
                slot: racer.seat.slot,
                place,
            });
        }
        let window = self.settings.finish_window_ticks;
        if let Some(first) = self.state.first_finish_at
            && window > 0
            && tick >= first.saturating_add(window)
        {
            for racer in &mut self.state.racers {
                if racer.finished_at.is_none() && !racer.dnf {
                    racer.dnf = true;
                    push(RulesEvent::RacerDnf {
                        tick,
                        team: racer.seat.team,
                        slot: racer.seat.slot,
                    });
                }
            }
        }
        if !self.state.racers.is_empty() && self.state.racers.iter().all(|r| r.finished_at.is_some() || r.dnf)
        {
            self.state.ended_at = Some(tick);
        }
    }

    /// The live standings: finished racers by place, then running racers by laps (most first),
    /// checkpoint index (highest first), remaining fraction (least first) and seat, then DNF
    /// racers by seat.
    pub fn standings(&self) -> Vec<Standing> {
        let mut order: Vec<&RacerProgress> = self.state.racers.iter().collect();
        order.sort_by(|a, b| {
            let group = |r: &RacerProgress| -> u8 {
                if r.place.is_some() {
                    0
                } else if r.dnf {
                    2
                } else {
                    1
                }
            };
            group(a)
                .cmp(&group(b))
                .then_with(|| a.place.cmp(&b.place))
                .then_with(|| {
                    if group(a) == 1 {
                        b.lap
                            .cmp(&a.lap)
                            .then(b.checkpoint_index.cmp(&a.checkpoint_index))
                            .then(a.remaining.cmp_exact(&b.remaining))
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then(a.seat.cmp(&b.seat))
        });
        order
            .into_iter()
            .enumerate()
            .map(|(i, r)| Standing {
                position: u32::try_from(i + 1).unwrap_or(u32::MAX),
                seat: r.seat,
                lap: r.lap,
                checkpoint_index: r.checkpoint_index,
                remaining: r.remaining,
                place: r.place,
                dnf: r.dnf,
                wrong_way: r.wrong_way,
                best_lap_ticks: r.best_lap_ticks,
            })
            .collect()
    }

    /// The match outcome once every racer finished or is DNF: the winner's team (`0` when nobody
    /// finished). Rule word `"race"`.
    pub fn outcome(&self) -> Option<Outcome> {
        self.state.ended_at?;
        let winner = self
            .state
            .racers
            .iter()
            .find(|r| r.place == Some(1))
            .map_or(0, |r| r.seat.team);
        Some(Outcome { winner, rule: "race" })
    }
}

/// The race result as a [`crate::win_condition::WinCondition`] (reads the race, not the view).
#[derive(Debug, Clone, Copy)]
pub struct RaceWin<'a>(pub &'a Race);

impl crate::win_condition::WinCondition for RaceWin<'_> {
    fn evaluate(&self, _view: &dyn crate::win_condition::MatchView) -> Option<Outcome> {
        self.0.outcome()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_and_non_finite_positions_do_not_overflow() {
        let mut race = Race::new(RaceSettings {
            checkpoints: vec![
                Checkpoint {
                    id: 1,
                    volume: HillVolume {
                        min: [-1.0, -1.0, -1.0],
                        max: [1.0, 1.0, 1.0],
                    },
                    next: vec![2],
                },
                Checkpoint {
                    id: 2,
                    volume: HillVolume {
                        min: [9.0, -1.0, -1.0],
                        max: [11.0, 1.0, 1.0],
                    },
                    next: vec![1],
                },
            ],
            ..RaceSettings::default()
        });
        race.enter(SeatId::new(1, 0), 7);
        for (tick, position) in [
            [f32::MAX, -f32::MAX, 1.0e30],
            [f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
            [-1.0e38, 1.0e38, 0.0],
        ]
        .into_iter()
        .enumerate()
        {
            let unit = Contestant {
                id: 7,
                team: 1,
                position,
                alive: true,
            };
            race.step(tick as u64, &[unit], &mut |_| {});
        }
        assert_eq!(race.state.racers[0].lap, 0);
        assert_eq!(mm(f32::MAX), MAX_MM);
        assert_eq!(mm(f32::NAN), 0);
    }

    fn two_gates() -> RaceSettings {
        let gate = |id: u32, x: f32, next: u32| Checkpoint {
            id,
            volume: HillVolume {
                min: [x - 1.0, -1.0, -1.0],
                max: [x + 1.0, 1.0, 1.0],
            },
            next: vec![next],
        };
        RaceSettings {
            checkpoints: vec![gate(1, 0.0, 2), gate(2, 10.0, 1)],
            ..RaceSettings::default()
        }
    }

    fn at(id: u32, x: f32) -> Contestant {
        Contestant {
            id,
            team: 1,
            position: [x, 0.0, 0.0],
            alive: true,
        }
    }

    /// ADR-8802: a swap in one batch moves both rows with their progress; a placed row at a
    /// destination is kept and the mover's row is dropped, so nothing is left racing behind.
    #[test]
    fn carry_moves_rows_with_progress_and_never_takes_a_place_back() {
        let (a, b) = (SeatId::new(1, 0), SeatId::new(1, 1));
        let mut race = Race::new(two_gates());
        race.enter(a, 1);
        race.enter(b, 2);
        race.step(1, &[at(1, 0.0), at(2, 0.0)], &mut |_| {});
        race.step(2, &[at(1, 10.0), at(2, 5.0)], &mut |_| {});
        assert_eq!(race.state.racers[0].checkpoint, 2, "a passed the second gate");
        race.carry(&[(a, b), (b, a)]);
        let seats: Vec<(SeatId, u32, u32)> = race
            .state
            .racers
            .iter()
            .map(|r| (r.seat, r.unit, r.checkpoint))
            .collect();
        assert_eq!(seats, vec![(a, 2, 1), (b, 1, 2)]);
        // b's row (unit 1) finishes; then a moves onto b's coordinate after b's player leaves.
        race.step(3, &[at(1, 0.0), at(2, 5.0)], &mut |_| {});
        assert_eq!(race.state.racers[1].place, Some(1));
        race.carry(&[(a, b)]);
        assert_eq!(race.state.racers.len(), 1, "the mover's row is dropped");
        assert_eq!(race.state.racers[0].seat, b);
        assert_eq!(race.state.racers[0].place, Some(1), "the place stays");
    }

    /// ADR-8802: a seat that leaves is DNF at once (one event) and ends a race nobody finished;
    /// a finished row keeps its place and sends nothing.
    #[test]
    fn retire_marks_a_running_row_dnf_once_and_keeps_a_finished_place() {
        let (a, b) = (SeatId::new(1, 0), SeatId::new(1, 1));
        let mut race = Race::new(two_gates());
        race.enter(a, 1);
        race.enter(b, 2);
        race.step(1, &[at(1, 0.0), at(2, 0.0)], &mut |_| {});
        race.step(2, &[at(1, 10.0), at(2, 10.0)], &mut |_| {});
        race.step(3, &[at(1, 0.0), at(2, 10.0)], &mut |_| {});
        let mut events = Vec::new();
        race.retire(a, 3, &mut |e| events.push(e));
        race.retire(b, 3, &mut |e| events.push(e));
        race.retire(b, 3, &mut |e| events.push(e));
        assert_eq!(
            events,
            vec![RulesEvent::RacerDnf {
                tick: 3,
                team: 1,
                slot: 1
            }]
        );
        race.step(4, &[], &mut |_| {});
        assert_eq!(race.state.ended_at, Some(4));
        assert_eq!(race.outcome().map(|o| o.winner), Some(1));
        race.forget_unplaced(a);
        assert_eq!(race.state.racers.len(), 2, "a placed row is never forgotten");
        let mut solo = Race::new(two_gates());
        solo.enter(a, 1);
        solo.step(1, &[at(1, 5.0)], &mut |_| {});
        solo.retire(a, 1, &mut |_| {});
        solo.step(2, &[], &mut |_| {});
        assert_eq!(solo.outcome().map(|o| o.winner), Some(0), "nobody finished");
        solo.forget_unplaced(a);
        assert!(solo.state.racers.is_empty());
    }

    /// ADR-8802: the finish overshoot is millimeters along the last segment, whatever its length.
    #[test]
    fn past_gate_is_meters_along_the_segment() {
        let short = past_gate([-30_000, 0, 0], [0, 0, 0], [2_000, 0, 0]);
        let long = past_gate([-500_000, 0, 0], [0, 0, 0], [24_000, 0, 0]);
        assert!(short.cmp_exact(&Fraction { num: 2_000, den: 1 }).is_eq());
        assert!(long.cmp_exact(&Fraction { num: 24_000, den: 1 }).is_eq());
        let short_of = past_gate([0, 0, 0], [10_000, 0, 0], [9_000, 0, 0]);
        assert!(short_of.cmp_exact(&Fraction { num: -1_000, den: 1 }).is_eq());
        // The projection is not clamped behind the segment origin.
        let (_, behind) = remaining_on([0, 0, 0], [10_000, 0, 0], [-4_000, 0, 0]);
        assert_eq!(behind, -40_000_000);
        assert_eq!(saturate_i64(i128::MIN), i64::MIN);
        let far = past_gate([-MAX_MM, -MAX_MM, -MAX_MM], [MAX_MM, MAX_MM, MAX_MM], [MAX_MM; 3]);
        assert_eq!(far.num, 0);
    }

    #[test]
    fn fractions_compare_exactly_across_denominators() {
        let a = Fraction { num: 1, den: 3 };
        let b = Fraction { num: 2, den: 6 };
        let c = Fraction { num: 1, den: 2 };
        assert!(a.cmp_exact(&b).is_eq());
        assert!(a.cmp_exact(&c).is_lt());
        assert_eq!(Fraction::new(5, 0), Fraction::default());
    }

    #[test]
    fn remaining_is_clamped_to_the_segment() {
        let from = [0, 0, 0];
        let to = [10_000, 0, 0];
        let (half, _) = remaining_on(from, to, [5_000, 0, 3_000]);
        assert!(half.cmp_exact(&Fraction { num: 1, den: 2 }).is_eq());
        let (behind, _) = remaining_on(from, to, [-4_000, 0, 0]);
        assert!(behind.cmp_exact(&Fraction { num: 1, den: 1 }).is_eq());
        let (past, _) = remaining_on(from, to, [12_000, 0, 0]);
        assert_eq!(past.num, 0);
    }

    #[test]
    fn validate_refuses_unknown_next_and_duplicate_ids() {
        let gate = |id: u32, x: f32, next: Vec<u32>| Checkpoint {
            id,
            volume: HillVolume {
                min: [x - 1.0, -1.0, -5.0],
                max: [x + 1.0, 3.0, 5.0],
            },
            next,
        };
        let mut settings = RaceSettings {
            checkpoints: vec![gate(1, 0.0, vec![2]), gate(2, 50.0, vec![1])],
            ..RaceSettings::default()
        };
        assert_eq!(settings.validate(), Ok(()));
        settings.checkpoints[1].next = vec![9];
        assert_eq!(settings.validate(), Err(bad("next_unknown")));
        settings.checkpoints[1] = gate(1, 50.0, vec![1]);
        assert_eq!(settings.validate(), Err(bad("duplicate_id")));
        settings.checkpoints = vec![gate(1, 0.0, vec![2]), gate(2, 50.0, vec![1])];
        settings.laps = 0;
        assert_eq!(settings.validate(), Err(bad("laps_range")));
    }
}
