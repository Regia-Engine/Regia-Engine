// SPDX-License-Identifier: MPL-2.0
//! Desync detection, diagnosis and the injection hook (spec 120 § 7.6; ADR-8211).
//!
//! A lockstep match stays correct only while every simulation steps to the same state. This module
//! is the game-facing half of finding out when one does not:
//!
//! * [`DesyncHash`]: the game hashes its deterministic state, once as a whole (`world_hash`, the
//!   vote every [`LockstepConfig::hash_every`](crate::lockstep::LockstepConfig::hash_every) ticks)
//!   and per named subsystem (`subsystem_hashes`, at most [`MAX_SUBSYSTEMS`]).
//! * [`SubsystemRing`]: every participant keeps the subsystem rows of its last [`ROW_RING_TICKS`]
//!   ticks. They never travel unless the host probes after a split.
//! * The **probe**: after a verdict with a diverged voter the host first finds the first tick its
//!   world hash differs from the reference's (per-tick world hashes), then pulls the subsystem rows
//!   at that tick and the tick before it from every diverged voter and from the reference voter (an
//!   agreeing seat, or the host itself), and names the first diverged subsystem and the last
//!   agreed tick ([`locate`], [`DesyncReport`]).
//! * [`DesyncInject`]: the test and agent-protocol hook that flips one subsystem at a chosen tick.
//!
//! The verdict itself (who is in the minority, who is resynced, removed, or a host fault) is the
//! quorum of [`crate::lockstep`] (ADR-8146): one forged hash is one vote, so a single hostile seat is
//! resynced and then removed with [`DESYNC`] while the match goes on for everyone else.
//!
//! ```
//! use regia_net::desync::{DesyncHash, SubsystemRing, locate};
//!
//! struct Sim { units: u64, economy: u64 }
//! impl DesyncHash for Sim {
//!     fn world_hash(&self, _tick: u64) -> u64 { self.units.rotate_left(17) ^ self.economy }
//!     fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
//!         out.clear();
//!         out.push(("units", self.units));
//!         out.push(("economy", self.economy));
//!     }
//! }
//!
//! let (mut mine, mut theirs) = (SubsystemRing::new(), SubsystemRing::new());
//! for tick in 0..10 {
//!     let economy = if tick >= 7 { tick + 1000 } else { tick };
//!     mine.record(tick, &Sim { units: tick * 3, economy });
//!     theirs.record(tick, &Sim { units: tick * 3, economy: tick });
//! }
//! let found = locate(&mine.window(0, 9), &theirs.window(0, 9));
//! assert_eq!(found.first_diverged_tick, Some(7));
//! assert_eq!(found.last_agreed_tick, Some(6));
//! assert_eq!(found.subsystem.as_deref(), Some("economy"));
//! ```

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::lockstep::Voter;

/// Most named subsystems a [`DesyncHash`] reports; more are ignored.
pub const MAX_SUBSYSTEMS: usize = 64;
/// Ticks of subsystem rows a [`SubsystemRing`] keeps.
pub const ROW_RING_TICKS: usize = 240;
/// Longest subsystem name kept, bytes (a longer one is cut at a character boundary).
pub const MAX_SUBSYSTEM_NAME_BYTES: usize = 64;
/// Reason code of a seat removed for repeated divergence.
pub const DESYNC: &str = "desync";
/// Reason code of a match whose host simulation is outvoted past `max_resyncs` (a host fault);
/// until host migration (spec 120 § 10.5) the match ends with it.
pub const HOST_DESYNC: &str = "host_desync";
/// Reason code of a match whose votes cannot be decided (a tie past the tie rule).
pub const DESYNC_UNRESOLVED: &str = "desync_unresolved";

/// Game-supplied hashing of its deterministic state (spec 120 § 7.6).
///
/// The same shape as the detsim kit's trait (ADR-8047); when both are in one build this one becomes
/// a re-export of that one, and an implementation of either compiles against both.
pub trait DesyncHash {
    /// One 64-bit hash of the whole state after stepping `tick` (cheap; sent every `hash_every`
    /// ticks).
    fn world_hash(&self, tick: u64) -> u64;
    /// Named per-subsystem hashes for the probe (at most [`MAX_SUBSYSTEMS`]), folded after every
    /// step into a ring. Clear `out` first; list the subsystems in the same order every tick.
    fn subsystem_hashes(&self, tick: u64, out: &mut Vec<(&'static str, u64)>);
}

/// One tick of subsystem hashes, in the ring's name order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemRow {
    /// The tick the state was hashed after.
    pub tick: u64,
    /// The world hash at the tick.
    pub world: u64,
    /// One hash per name of the ring, in its order.
    pub hashes: Vec<u64>,
}

/// Subsystem rows of a window of ticks with their names: what a probe reply carries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProbeRows {
    /// Subsystem names, in row order.
    pub names: Vec<String>,
    /// Rows, ascending by tick.
    pub rows: Vec<SubsystemRow>,
}

/// The last [`ROW_RING_TICKS`] ticks of a simulation's subsystem hashes (spec 120 § 7.6, a DO27 design).
///
/// Recording a tick at or before the newest one (a replay, a state transfer) first drops the rows
/// from that tick on, so the ring always holds the history the simulation has now. Once full it
/// reuses its rows: recording allocates nothing.
#[derive(Debug, Clone, Default)]
pub struct SubsystemRing {
    names: Vec<&'static str>,
    rows: VecDeque<SubsystemRow>,
    scratch: Vec<(&'static str, u64)>,
    spare: Vec<SubsystemRow>,
}

impl SubsystemRing {
    /// An empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hashes `state` after `tick` and records the row; returns the world hash (the value to vote).
    pub fn record<H: DesyncHash + ?Sized>(&mut self, tick: u64, state: &H) -> u64 {
        let world = state.world_hash(tick);
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        state.subsystem_hashes(tick, &mut scratch);
        self.push(tick, world, &scratch);
        self.scratch = scratch;
        world
    }

    /// Records a row made elsewhere: `world` and `(name, hash)` pairs (at most [`MAX_SUBSYSTEMS`]
    /// are kept). A name list that differs from the ring's starts the ring again with the new names.
    pub fn push(&mut self, tick: u64, world: u64, subsystems: &[(&'static str, u64)]) {
        let subsystems = &subsystems[..subsystems.len().min(MAX_SUBSYSTEMS)];
        let same_names = self.names.len() == subsystems.len()
            && self.names.iter().zip(subsystems).all(|(a, (b, _))| a == b);
        if !same_names {
            self.clear();
            self.names.extend(subsystems.iter().map(|(name, _)| *name));
        }
        self.rewind(tick);
        let mut row = if self.rows.len() >= ROW_RING_TICKS {
            self.rows.pop_front()
        } else {
            self.spare.pop()
        }
        .unwrap_or_else(|| SubsystemRow {
            tick,
            world,
            hashes: Vec::with_capacity(MAX_SUBSYSTEMS),
        });
        row.tick = tick;
        row.world = world;
        row.hashes.clear();
        row.hashes.extend(subsystems.iter().map(|(_, hash)| *hash));
        self.rows.push_back(row);
    }

    /// Drops the rows from `tick` on (the simulation is about to step them again).
    pub fn rewind(&mut self, tick: u64) {
        while self.rows.back().is_some_and(|row| row.tick >= tick) {
            if let Some(row) = self.rows.pop_back() {
                self.spare.push(row);
            }
        }
    }

    /// Forgets every row and name.
    pub fn clear(&mut self) {
        self.names.clear();
        self.spare.extend(self.rows.drain(..));
    }

    /// The subsystem names, in row order.
    pub fn names(&self) -> &[&'static str] {
        &self.names
    }

    /// The row of `tick`, when the ring still holds it.
    pub fn row(&self, tick: u64) -> Option<&SubsystemRow> {
        let first = self.rows.front()?.tick;
        let at = usize::try_from(tick.checked_sub(first)?).ok()?;
        match self.rows.get(at) {
            Some(row) if row.tick == tick => Some(row),
            // Ticks recorded with gaps: fall back to a search.
            _ => self.rows.iter().find(|row| row.tick == tick),
        }
    }

    /// Rows held, ticks.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the ring holds no row.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The oldest and newest ticks held.
    pub fn span(&self) -> Option<(u64, u64)> {
        Some((self.rows.front()?.tick, self.rows.back()?.tick))
    }

    /// The rows of `from..=to` the ring holds, with the names: a probe reply.
    pub fn window(&self, from: u64, to: u64) -> ProbeRows {
        ProbeRows {
            names: self.names.iter().map(|n| (*n).to_owned()).collect(),
            rows: self
                .rows
                .iter()
                .filter(|row| (from..=to).contains(&row.tick))
                .cloned()
                .collect(),
        }
    }
}

/// Where two simulations' rows first differ ([`locate`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Location {
    /// The first tick both sides hold whose world hash or any subsystem hash differs.
    pub first_diverged_tick: Option<u64>,
    /// The newest tick before it both sides hold with every hash equal.
    pub last_agreed_tick: Option<u64>,
    /// The first subsystem (in the diverged side's order) that differs at `first_diverged_tick`;
    /// `None` when only the world hash differs (state no subsystem covers) or nothing differs.
    pub subsystem: Option<String>,
    /// Every subsystem that differs at `first_diverged_tick`, in the same order.
    pub subsystems: Vec<String>,
}

/// Compares the rows of a diverged simulation with a reference's, tick by tick in ascending order,
/// subsystems by name. A name only one side lists counts as differing.
pub fn locate(diverged: &ProbeRows, reference: &ProbeRows) -> Location {
    let mut out = Location::default();
    let mut theirs = reference.rows.iter().peekable();
    for row in &diverged.rows {
        while theirs.peek().is_some_and(|r| r.tick < row.tick) {
            theirs.next();
        }
        let Some(other) = theirs.peek().filter(|r| r.tick == row.tick) else {
            continue;
        };
        let differing = differing_subsystems(diverged, row, reference, other);
        if differing.is_empty() && row.world == other.world {
            out.last_agreed_tick = Some(row.tick);
            continue;
        }
        out.first_diverged_tick = Some(row.tick);
        out.subsystem = differing.first().cloned();
        out.subsystems = differing;
        break;
    }
    out
}

fn differing_subsystems(
    mine: &ProbeRows,
    my_row: &SubsystemRow,
    theirs: &ProbeRows,
    their_row: &SubsystemRow,
) -> Vec<String> {
    let value = |rows: &ProbeRows, row: &SubsystemRow, name: &str| {
        rows.names
            .iter()
            .position(|n| n == name)
            .and_then(|i| row.hashes.get(i).copied())
    };
    let mut out: Vec<String> = mine
        .names
        .iter()
        .filter(|name| value(mine, my_row, name) != value(theirs, their_row, name))
        .cloned()
        .collect();
    out.extend(
        theirs
            .names
            .iter()
            .filter(|name| !mine.names.contains(name) && value(theirs, their_row, name).is_some())
            .cloned(),
    );
    out
}

/// What the probe found for one diverged voter (spec 120 § 7.6): `HostEvent::DesyncProbed` and the
/// `last_desync` field of `net.lockstep`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesyncReport {
    /// The checkpoint whose verdict started the probe.
    pub checkpoint: u64,
    /// The diverged voter.
    pub voter: Voter,
    /// The voter whose rows it was compared with (an agreeing seat, or the host's own simulation).
    pub reference: Voter,
    /// The first tick a hash differs at; `None` when the rows around the split held no difference
    /// (the rows were gone, or the voter's state was transferred before it replied).
    pub first_diverged_tick: Option<u64>,
    /// The newest tick before it with every hash equal (from the rows), else the last agreed
    /// checkpoint.
    pub last_agreed_tick: Option<u64>,
    /// The first diverged subsystem; `None` when only the world hash differs or no row differs.
    pub subsystem: Option<String>,
    /// Every subsystem that differs at `first_diverged_tick`.
    pub subsystems: Vec<String>,
}

/// The test and agent-protocol hook that flips one subsystem at a chosen tick (spec 120 § 7.6).
///
/// The game consults it in its step: `if inject.hits(tick, "economy") { state.gold ^= inject.mask }`.
/// A real divergence is deterministic on the machine that has it, so a replay of the tick (a
/// self-check) flips it again; a state transfer from a later checkpoint skips the tick and repairs
/// it. Never enable it in a shipped match: it exists so a test or an agent can prove that a desync
/// is detected by the next hash tick and named by the probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesyncInject {
    /// The subsystem to flip.
    pub subsystem: String,
    /// The tick whose step flips it.
    pub tick: u64,
    /// The bits to flip (never zero).
    pub mask: u64,
}

impl DesyncInject {
    /// Flips `subsystem` at `tick` with the default mask.
    pub fn new(subsystem: impl Into<String>, tick: u64) -> Self {
        Self {
            subsystem: subsystem.into(),
            tick,
            mask: 0x5a5a_c3c3_0f0f_9696,
        }
    }

    /// Whether the step of `tick` must flip `subsystem`.
    pub fn hits(&self, tick: u64, subsystem: &str) -> bool {
        tick == self.tick && subsystem == self.subsystem
    }

    /// `value` with the mask applied when [`Self::hits`].
    pub fn flip(&self, tick: u64, subsystem: &str, value: u64) -> u64 {
        if self.hits(tick, subsystem) {
            value ^ self.mask.max(1)
        } else {
            value
        }
    }
}

/// The host's second probe stage: subsystem rows from each located diverged voter and from one
/// reference, around that voter's first diverged tick.
#[derive(Debug, Clone)]
pub(crate) struct SubsystemProbe {
    checkpoint: u64,
    last_agreed_checkpoint: Option<u64>,
    pub(crate) reference: Voter,
    targets: Vec<ProbeTarget>,
    reference_names: Option<Vec<String>>,
    reference_rows: BTreeMap<u64, SubsystemRow>,
    /// Windows asked of the reference that it has not answered.
    reference_pending: usize,
}

#[derive(Debug, Clone)]
struct ProbeTarget {
    voter: Voter,
    from: u64,
    to: u64,
    rows: Option<ProbeRows>,
    reported: bool,
}

impl SubsystemProbe {
    /// Diverged voters a probe asks at most.
    pub(crate) const MAX_TARGETS: usize = 8;

    pub(crate) fn new(checkpoint: u64, last_agreed_checkpoint: Option<u64>, reference: Voter) -> Self {
        Self {
            checkpoint,
            last_agreed_checkpoint,
            reference,
            targets: Vec::new(),
            reference_names: None,
            reference_rows: BTreeMap::new(),
            reference_pending: 0,
        }
    }

    /// Adds `voter`, whose world hash first differs at `split`; returns the window to ask it and
    /// the reference for (the split tick and the tick before it), `None` when it is already asked.
    pub(crate) fn add_target(&mut self, voter: Voter, split: u64) -> Option<(u64, u64)> {
        if voter == self.reference
            || self.targets.len() >= Self::MAX_TARGETS
            || self.targets.iter().any(|t| t.voter == voter)
        {
            return None;
        }
        let (from, to) = (split.saturating_sub(1), split);
        self.targets.push(ProbeTarget {
            voter,
            from,
            to,
            rows: None,
            reported: false,
        });
        self.reference_pending += 1;
        Some((from, to))
    }

    /// Takes `voter`'s reply; returns whether it was one this probe asked for.
    pub(crate) fn offer(&mut self, voter: Voter, rows: ProbeRows) -> bool {
        if voter == self.reference {
            if self.reference_pending == 0 {
                return false;
            }
            self.reference_pending -= 1;
            let names = self.reference_names.get_or_insert_with(|| rows.names.clone());
            if *names == rows.names {
                for row in rows.rows {
                    self.reference_rows.entry(row.tick).or_insert(row);
                }
            }
            return true;
        }
        let Some(target) = self
            .targets
            .iter_mut()
            .find(|t| t.voter == voter && t.rows.is_none())
        else {
            return false;
        };
        let (from, to) = (target.from, target.to);
        target.rows = Some(ProbeRows {
            names: rows.names,
            rows: rows
                .rows
                .into_iter()
                .filter(|row| (from..=to).contains(&row.tick))
                .collect(),
        });
        true
    }

    /// Every row collected, per voter ascending: the targets' rows and the reference's.
    pub(crate) fn peer_rows(&self) -> Vec<(Voter, ProbeRows)> {
        let mut out: Vec<(Voter, ProbeRows)> = self
            .targets
            .iter()
            .filter_map(|t| t.rows.clone().map(|rows| (t.voter, rows)))
            .collect();
        if let Some(names) = &self.reference_names {
            out.push((
                self.reference,
                ProbeRows {
                    names: names.clone(),
                    rows: self.reference_rows.values().cloned().collect(),
                },
            ));
        }
        out.sort_by_key(|(voter, _)| *voter);
        out
    }

    /// The reports that can be made now: each target once, when its rows are in and the reference
    /// holds its window (or has answered every window asked).
    pub(crate) fn reports(&mut self) -> Vec<DesyncReport> {
        let Some(names) = &self.reference_names else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for target in &mut self.targets {
            let Some(rows) = target.rows.as_ref().filter(|_| !target.reported) else {
                continue;
            };
            let covered = (target.from..=target.to).all(|t| self.reference_rows.contains_key(&t));
            if !covered && self.reference_pending > 0 {
                continue;
            }
            target.reported = true;
            let reference = ProbeRows {
                names: names.clone(),
                rows: self
                    .reference_rows
                    .range(target.from..=target.to)
                    .map(|(_, row)| row.clone())
                    .collect(),
            };
            let found = locate(rows, &reference);
            out.push(DesyncReport {
                checkpoint: self.checkpoint,
                voter: target.voter,
                reference: self.reference,
                first_diverged_tick: found.first_diverged_tick,
                last_agreed_tick: found.last_agreed_tick.or(self.last_agreed_checkpoint),
                subsystem: found.subsystem,
                subsystems: found.subsystems,
            });
        }
        out
    }
}

/// What `net.desync` shows for one lockstep host or seat (spec 120 § 13.3; ADR-8455). The game
/// publishes it with `NetAgentState::publish_desync` after each probe report or bundle.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DesyncStatus {
    /// Probe reports so far.
    pub reports: u64,
    /// The newest probe report: the live verdict.
    pub last: Option<DesyncReport>,
    /// The newest probe's subsystem rows, ascending by voter.
    pub rows: Vec<BundlePeer>,
    /// The path of the newest bundle written, when the game wrote one.
    pub bundle: Option<String>,
    /// The newest offline replay verdict, when the game ran one.
    pub replay: Option<ReplayVerdict>,
}

impl DesyncStatus {
    /// The host's diagnosis: its report count, newest report and the newest probe's rows.
    pub fn of_host<G: crate::payload::NetGame>(host: &crate::lockstep::LockstepHost<G>) -> Self {
        let (reports, last) = host.desync_reports();
        Self {
            reports,
            last: last.cloned(),
            rows: host
                .desync_probe_rows()
                .into_iter()
                .map(|(voter, rows)| BundlePeer { voter, rows })
                .collect(),
            bundle: None,
            replay: None,
        }
    }

    /// The same, with the path of the bundle just written.
    pub fn with_bundle(mut self, path: &std::path::Path) -> Self {
        self.bundle = Some(path.display().to_string());
        self
    }
}

/// Cuts a subsystem name to [`MAX_SUBSYSTEM_NAME_BYTES`] at a character boundary.
pub(crate) fn clip_name(name: &str) -> &str {
    if name.len() <= MAX_SUBSYSTEM_NAME_BYTES {
        return name;
    }
    let mut end = MAX_SUBSYSTEM_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

pub mod bundle;
pub mod replay;
pub mod sample;

pub use bundle::{
    BundleError, BundleFrame, BundleGame, BundleInput, BundleMembership, BundlePeer, BundleSnapshot,
    BundleWriter, DESYNC_BUNDLE_FORMAT_VERSION, DESYNC_BUNDLE_SUFFIX, DesyncBundle, HexBytes,
};
pub use replay::{DesyncReplay, GameIdentity, PeerReplay, ReplayRegistry, ReplayVerdict, replay};

#[cfg(test)]
mod tests;
