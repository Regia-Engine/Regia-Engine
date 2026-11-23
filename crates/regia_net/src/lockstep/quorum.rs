// SPDX-License-Identifier: MPL-2.0
//! Hash authority by quorum (ADR-8146; spec 120 § 7.6 "corroborated verdict").
//!
//! Every simulation of a match votes its state hash at checkpoint ticks: each member seat, and the
//! host's own simulation when the host is a dedicated observer. A listen host votes as its seat,
//! once, like everyone else: **the host's hash is one vote, not the reference**.
//!
//! The rules, in order:
//!
//! 1. A checkpoint closes when every expected voter has voted or `vote_deadline_ticks` after it was
//!    built. A silent voter abstains; it never blocks a tally.
//! 2. All votes equal: agreed.
//! 3. Otherwise, with `self_check` on, the voters outside the strict majority (every voter when
//!    there is none) are asked to replay from the last agreed checkpoint and vote again. A voter
//!    whose state was hit by a one-off fault repairs itself this way, with no state transfer and
//!    nobody's word taken.
//! 4. **Strict majority** (more than half of the votes cast): its hash is the reference. Every
//!    other voter is diverged and is ordered to take the state of a majority voter; that includes
//!    the host. A seat is removed only after `max_resyncs` transfers; a host past the limit is a
//!    host fault (the seam for host migration), and no seat is removed for it.
//! 5. **No strict majority** (1 against 1 in a two-voter match, 2 against 2, three ways): nobody
//!    is diverged and nobody is removed. [`TiePolicy::HostGroup`] takes the state of the group the
//!    host votes in (the lowest voter's group when the host has no vote) so the match goes on;
//!    it is counted as a tie, never against a seat. After `max_tie_resyncs`, or at once under
//!    [`TiePolicy::EndMatch`], the match is unresolved and ends without blame.
//!
//! A diverged voter's later votes are ignored until one of them matches the reference again.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::SeatId;

/// One simulation with a vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Voter {
    /// The host's own simulation when the host has no seat (a dedicated observer process).
    Observer,
    /// A seat's simulation. A listen host votes as its seat.
    Seat(SeatId),
}

impl std::fmt::Display for Voter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observer => f.write_str("observer"),
            Self::Seat(seat) => write!(f, "{seat}"),
        }
    }
}

/// What decides a tally without a strict majority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TiePolicy {
    /// Everyone takes the state of the group the host votes in (the lowest voter's group when the
    /// host has no vote). Counted as a tie, never against a seat.
    #[default]
    HostGroup,
    /// The match ends as unresolved, without blame (competitive play without an observer).
    EndMatch,
}

/// How a checkpoint with differing hashes ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    /// The hashes differed, and after the self-check every voter agrees.
    SelfHealed,
    /// A strict majority agrees; the voters in `diverged` take its state.
    Diverged,
    /// No strict majority: the tie rule picked the group whose state everyone takes.
    Split,
    /// No strict majority and the tie rule gives up: the match ends without blame.
    Unresolved,
}

/// The outcome of a checkpoint whose votes differed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesyncVerdict {
    /// The checkpoint tick the hashes differed at.
    pub tick: u64,
    /// The checkpoint before it that was agreed; the divergence lies after it.
    pub last_agreed: Option<u64>,
    /// How it ended.
    pub kind: VerdictKind,
    /// The reference hash: the majority's, or the tie rule's group's; `None` when unresolved.
    pub hash: Option<u64>,
    /// Voters whose final vote is the reference.
    pub agreeing: Vec<Voter>,
    /// Voters whose final vote differs from the reference. Under [`VerdictKind::Split`] they are
    /// the other side of a tie, not at fault.
    pub diverged: Vec<Voter>,
    /// Voters whose vote changed to the reference after the self-check.
    pub self_healed: Vec<Voter>,
    /// Expected voters that did not vote in time.
    pub abstained: Vec<Voter>,
    /// The host's own vote is among `diverged`.
    pub host_diverged: bool,
    /// The voter whose state the diverged voters take.
    pub source: Option<Voter>,
}

/// What the host has to do after a tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuorumAction {
    /// Ask `voters` to replay from `from` and vote again on `tick`.
    Recheck {
        tick: u64,
        from: Option<u64>,
        voters: Vec<Voter>,
    },
    /// A checkpoint with differing votes closed.
    Verdict(DesyncVerdict),
    /// `target` takes `source`'s state at `tick`. `counted` is false for a tie.
    Resync {
        tick: u64,
        target: Voter,
        source: Voter,
        counted: bool,
        resyncs: u32,
    },
    /// `seat` passed `max_resyncs`: remove it with `desync`.
    Remove { tick: u64, seat: SeatId },
    /// The host's own simulation passed `max_resyncs`.
    HostFault { tick: u64, resyncs: u32 },
    /// The match cannot be decided.
    Unresolved { tick: u64 },
}

#[derive(Debug, Clone, Copy)]
struct Vote {
    hash: u64,
    first_hash: u64,
    /// 0: the live vote; 1 and up: voted again after a self-check or a state transfer.
    round: u8,
}

#[derive(Debug, Clone)]
enum Phase {
    Voting,
    Rechecking { asked: BTreeSet<Voter>, deadline: u64 },
}

#[derive(Debug, Clone)]
struct Checkpoint {
    expected: BTreeSet<Voter>,
    votes: BTreeMap<Voter, Vote>,
    phase: Phase,
    deadline: u64,
}

/// One voter's record.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VoterRecord {
    /// Counted state transfers ordered for it.
    pub(crate) resyncs: u32,
    /// Self-check repairs.
    pub(crate) self_heals: u32,
    /// `Some(deadline tick)` while its votes are ignored; past the deadline the transfer is
    /// ordered again.
    pub(crate) resyncing: Option<u64>,
    /// The pending transfer is a tie's (not counted against it).
    tie: bool,
    /// The checkpoint of the pending transfer: only a vote on it or on a later checkpoint says
    /// anything about the state the voter took.
    resync_from: u64,
}

/// The tally state.
#[derive(Debug, Clone)]
pub(crate) struct Quorum {
    vote_deadline: u64,
    self_check: bool,
    max_resyncs: u32,
    tie_policy: TiePolicy,
    max_tie_resyncs: u32,
    resync_deadline: u64,
    host_voter: Option<Voter>,
    checkpoints: BTreeMap<u64, Checkpoint>,
    voters: BTreeMap<Voter, VoterRecord>,
    removed: BTreeSet<Voter>,
    last_agreed: Option<(u64, u64)>,
    /// The reference hash of the newest closed checkpoints, by tick ([`MAX_REFERENCES`] at most):
    /// what a vote that arrives after its checkpoint closed is compared with.
    references: BTreeMap<u64, u64>,
    /// The voters whose vote was the reference at the last agreed checkpoint: the only ones known
    /// to hold the reference state.
    reference_group: BTreeSet<Voter>,
    closed_through: Option<u64>,
    pub(crate) agreed: u64,
    pub(crate) desyncs: u64,
    pub(crate) ties: u32,
    pub(crate) resyncs: u64,
    pub(crate) late_votes: u64,
    unresolved: bool,
}

/// Open checkpoints kept. When this many wait for votes, a new checkpoint is not opened: the
/// oldest keeps its place and is tallied at its deadline.
const MAX_OPEN_CHECKPOINTS: usize = 64;
/// Closed references kept. A transfer's deadline is at most 96 checkpoint intervals (four vote
/// deadlines of 24 intervals each), so the reference of a transfer's checkpoint is still here when
/// the voter's vote arrives, however short the interval.
const MAX_REFERENCES: usize = 128;

impl Quorum {
    pub(crate) fn new(config: &super::LockstepConfig, host_voter: Option<Voter>) -> Self {
        let every = u64::from(config.hash_every.max(1));
        Self {
            vote_deadline: u64::from(config.vote_deadline_ticks),
            self_check: config.self_check,
            max_resyncs: config.max_resyncs,
            tie_policy: config.tie_policy,
            max_tie_resyncs: config.max_tie_resyncs,
            resync_deadline: (every * 20).max(u64::from(config.vote_deadline_ticks) * 4),
            host_voter,
            checkpoints: BTreeMap::new(),
            voters: BTreeMap::new(),
            removed: BTreeSet::new(),
            last_agreed: None,
            references: BTreeMap::new(),
            reference_group: BTreeSet::new(),
            closed_through: None,
            agreed: 0,
            desyncs: 0,
            ties: 0,
            resyncs: 0,
            late_votes: 0,
            unresolved: false,
        }
    }

    /// The last checkpoint with a reference hash.
    pub(crate) fn last_agreed(&self) -> Option<u64> {
        self.last_agreed.map(|(tick, _)| tick)
    }

    /// One voter's record.
    pub(crate) fn record(&self, voter: Voter) -> VoterRecord {
        self.voters.get(&voter).copied().unwrap_or_default()
    }

    /// Whether the match was reported unresolved.
    pub(crate) fn is_unresolved(&self) -> bool {
        self.unresolved
    }

    /// A voter left the match: it is expected nowhere any more.
    pub(crate) fn forget(&mut self, voter: Voter) {
        self.removed.insert(voter);
        for c in self.checkpoints.values_mut() {
            c.expected.remove(&voter);
            c.votes.remove(&voter);
            if let Phase::Rechecking { asked, .. } = &mut c.phase {
                asked.remove(&voter);
            }
        }
    }

    /// A voter (re)joins: its record starts clean.
    pub(crate) fn admit(&mut self, voter: Voter) {
        self.removed.remove(&voter);
        self.voters.insert(voter, VoterRecord::default());
    }

    /// Opens the checkpoint at `tick`, built at `build_tick`, for `members` (and the host's
    /// observer vote). Voters under a state transfer are not expected.
    pub(crate) fn open(&mut self, tick: u64, members: impl Iterator<Item = Voter>, build_tick: u64) {
        if self.unresolved || self.closed_through.is_some_and(|c| tick <= c) {
            return;
        }
        // Full: dropping the oldest instead would let one silent voter keep every checkpoint
        // from being tallied. `LockstepConfig::sanitized` keeps the settings clear of this.
        if self.checkpoints.len() >= MAX_OPEN_CHECKPOINTS && !self.checkpoints.contains_key(&tick) {
            return;
        }
        let mut expected: BTreeSet<Voter> = members
            .filter(|v| !self.removed.contains(v) && self.record(*v).resyncing.is_none())
            .collect();
        // An observer that left the quorum (it fell behind, ADR-8558) is not expected either.
        if let Some(Voter::Observer) = self.host_voter
            && !self.removed.contains(&Voter::Observer)
            && self.record(Voter::Observer).resyncing.is_none()
        {
            expected.insert(Voter::Observer);
        }
        self.checkpoints.insert(
            tick,
            Checkpoint {
                expected,
                votes: BTreeMap::new(),
                phase: Phase::Voting,
                deadline: build_tick.saturating_add(self.vote_deadline),
            },
        );
    }

    /// Takes `voter`'s hash for the checkpoint at `tick`. `round` 0 is the live vote; a later
    /// round replaces it (after a self-check or a state transfer). Returns whether it was stored.
    pub(crate) fn vote(&mut self, voter: Voter, tick: u64, round: u8, hash: u64) -> bool {
        if self.removed.contains(&voter) {
            return false;
        }
        let Some(c) = self.checkpoints.get_mut(&tick) else {
            if self.closed_through.is_some_and(|c| tick <= c) {
                self.late_votes += 1;
            }
            return false;
        };
        match c.votes.get_mut(&voter) {
            // A repeated live vote (datagram redundancy) changes nothing.
            Some(v) if round <= v.round => v.hash == hash,
            Some(v) => {
                v.hash = hash;
                v.round = round;
                true
            }
            None => {
                c.votes.insert(
                    voter,
                    Vote {
                        hash,
                        first_hash: hash,
                        round,
                    },
                );
                true
            }
        }
    }

    /// Closes every checkpoint that can close at `build_tick`, oldest first, and returns what the
    /// host has to do. A checkpoint never closes before an older one.
    pub(crate) fn advance(&mut self, build_tick: u64) -> Vec<QuorumAction> {
        let mut out = Vec::new();
        while let Some((&tick, c)) = self.checkpoints.iter().next() {
            if self.unresolved {
                self.checkpoints.clear();
                break;
            }
            let voted = |v: &Voter| c.votes.contains_key(v);
            match &c.phase {
                Phase::Voting => {
                    let complete = c.expected.iter().all(voted);
                    if !complete && build_tick < c.deadline {
                        break;
                    }
                }
                Phase::Rechecking { asked, deadline } => {
                    let done = asked
                        .iter()
                        .all(|v| c.votes.get(v).is_some_and(|vote| vote.round >= 1));
                    if !done && build_tick < *deadline {
                        break;
                    }
                }
            }
            if !self.tally(tick, build_tick, &mut out) {
                break;
            }
        }
        self.expire_transfers(build_tick, &mut out);
        out
    }

    /// Tallies the oldest checkpoint. Returns `false` when it stays open (a self-check started).
    fn tally(&mut self, tick: u64, build_tick: u64, out: &mut Vec<QuorumAction>) -> bool {
        let before = self.last_agreed();
        let Some(c) = self.checkpoints.get(&tick) else {
            return true;
        };
        let cast: BTreeMap<Voter, Vote> = c
            .votes
            .iter()
            .filter(|(v, _)| c.expected.contains(v))
            .map(|(v, vote)| (*v, *vote))
            .collect();
        let abstained: Vec<Voter> = c
            .expected
            .iter()
            .filter(|v| !cast.contains_key(v))
            .copied()
            .collect();
        let mut groups: BTreeMap<u64, Vec<Voter>> = BTreeMap::new();
        for (voter, vote) in &cast {
            groups.entry(vote.hash).or_default().push(*voter);
        }
        let rechecked = matches!(c.phase, Phase::Rechecking { .. });
        let healed = |reference: u64| -> Vec<Voter> {
            cast.iter()
                .filter(|(_, v)| v.hash == reference && v.first_hash != reference)
                .map(|(voter, _)| *voter)
                .collect()
        };
        if groups.len() <= 1 {
            let reference = groups.keys().next().copied();
            let others: Vec<(Voter, u64)> = c
                .votes
                .iter()
                .filter(|(v, _)| !c.expected.contains(v))
                .map(|(v, vote)| (*v, vote.hash))
                .collect();
            if let Some(hash) = reference {
                self.agree(tick, hash, cast.keys().copied());
                self.agreed += 1;
                if rechecked {
                    let self_healed = healed(hash);
                    for v in &self_healed {
                        self.voters.entry(*v).or_default().self_heals += 1;
                    }
                    self.desyncs += 1;
                    out.push(QuorumAction::Verdict(DesyncVerdict {
                        tick,
                        last_agreed: before,
                        kind: VerdictKind::SelfHealed,
                        hash: Some(hash),
                        agreeing: cast.keys().copied().collect(),
                        diverged: Vec::new(),
                        self_healed,
                        abstained,
                        host_diverged: false,
                        source: None,
                    }));
                }
                // Recovery: a voter under a transfer whose vote here is the reference is back.
                for (voter, voted) in others {
                    if voted == hash
                        && let Some(rec) = self.voters.get_mut(&voter)
                    {
                        rec.resyncing = None;
                        rec.tie = false;
                    }
                }
            }
            self.close(tick);
            return true;
        }
        let n = cast.len();
        let (top_hash, top_len) = groups
            .iter()
            .map(|(hash, voters)| (*hash, voters.len()))
            .max_by_key(|(hash, len)| (*len, std::cmp::Reverse(*hash)))
            .unwrap_or((0, 0));
        let majority = top_len * 2 > n;
        if self.self_check && !rechecked {
            let asked: BTreeSet<Voter> = cast
                .iter()
                .filter(|(_, v)| !majority || v.hash != top_hash)
                .map(|(voter, _)| *voter)
                .collect();
            let from = self.last_agreed();
            if let Some(c) = self.checkpoints.get_mut(&tick) {
                c.phase = Phase::Rechecking {
                    asked: asked.clone(),
                    deadline: build_tick.saturating_add(self.vote_deadline),
                };
            }
            out.push(QuorumAction::Recheck {
                tick,
                from,
                voters: asked.into_iter().collect(),
            });
            return false;
        }
        let last_agreed = before;
        self.desyncs += 1;
        let host = self.host_voter;
        if majority {
            let agreeing: Vec<Voter> = groups.get(&top_hash).cloned().unwrap_or_default();
            let diverged: Vec<Voter> = cast
                .iter()
                .filter(|(_, v)| v.hash != top_hash)
                .map(|(voter, _)| *voter)
                .collect();
            let source = host
                .filter(|h| agreeing.contains(h))
                .or_else(|| agreeing.iter().min().copied());
            let self_healed = healed(top_hash);
            for v in &self_healed {
                self.voters.entry(*v).or_default().self_heals += 1;
            }
            self.agree(tick, top_hash, agreeing.iter().copied());
            out.push(QuorumAction::Verdict(DesyncVerdict {
                tick,
                last_agreed,
                kind: VerdictKind::Diverged,
                hash: Some(top_hash),
                agreeing,
                diverged: diverged.clone(),
                self_healed,
                abstained,
                host_diverged: host.is_some_and(|h| diverged.contains(&h)),
                source,
            }));
            if let Some(source) = source {
                for target in diverged {
                    self.order_transfer(tick, build_tick, target, source, true, out);
                }
            }
        } else {
            self.ties += 1;
            let give_up = self.tie_policy == TiePolicy::EndMatch || self.ties > self.max_tie_resyncs;
            let source_hash = host
                .and_then(|h| cast.get(&h))
                .or_else(|| cast.values().next())
                .map(|v| v.hash);
            let (agreeing, diverged): (Vec<Voter>, Vec<Voter>) = cast
                .keys()
                .copied()
                .partition(|voter| cast.get(voter).map(|v| v.hash) == source_hash);
            let source = host
                .filter(|h| agreeing.contains(h))
                .or_else(|| agreeing.iter().min().copied());
            if give_up {
                self.unresolved = true;
                out.push(QuorumAction::Verdict(DesyncVerdict {
                    tick,
                    last_agreed,
                    kind: VerdictKind::Unresolved,
                    hash: None,
                    agreeing: Vec::new(),
                    diverged: Vec::new(),
                    self_healed: Vec::new(),
                    abstained,
                    host_diverged: false,
                    source: None,
                }));
                out.push(QuorumAction::Unresolved { tick });
            } else {
                if let Some(hash) = source_hash {
                    self.agree(tick, hash, agreeing.iter().copied());
                }
                out.push(QuorumAction::Verdict(DesyncVerdict {
                    tick,
                    last_agreed,
                    kind: VerdictKind::Split,
                    hash: source_hash,
                    agreeing,
                    diverged: diverged.clone(),
                    self_healed: Vec::new(),
                    abstained,
                    host_diverged: false,
                    source,
                }));
                if let Some(source) = source {
                    for target in diverged {
                        self.order_transfer(tick, build_tick, target, source, false, out);
                    }
                }
            }
        }
        self.close(tick);
        true
    }

    /// The checkpoint at `tick` has the reference `hash`, voted by `group`.
    fn agree(&mut self, tick: u64, hash: u64, group: impl IntoIterator<Item = Voter>) {
        self.last_agreed = Some((tick, hash));
        self.reference_group = group.into_iter().collect();
        self.references.insert(tick, hash);
        while self.references.len() > MAX_REFERENCES {
            self.references.pop_first();
        }
    }

    fn close(&mut self, tick: u64) {
        self.checkpoints.remove(&tick);
        self.closed_through = Some(self.closed_through.map_or(tick, |c| c.max(tick)));
    }

    fn order_transfer(
        &mut self,
        tick: u64,
        build_tick: u64,
        target: Voter,
        source: Voter,
        counted: bool,
        out: &mut Vec<QuorumAction>,
    ) {
        let deadline = build_tick.saturating_add(self.resync_deadline);
        let rec = self.voters.entry(target).or_default();
        if counted {
            rec.resyncs += 1;
            if rec.resyncs > self.max_resyncs {
                match target {
                    Voter::Seat(seat) if Some(target) != self.host_voter => {
                        out.push(QuorumAction::Remove { tick, seat });
                        self.forget(target);
                        return;
                    }
                    _ => out.push(QuorumAction::HostFault {
                        tick,
                        resyncs: rec.resyncs,
                    }),
                }
            }
        }
        let rec = self.voters.entry(target).or_default();
        rec.resyncing = Some(deadline);
        rec.tie = !counted;
        rec.resync_from = tick;
        self.resyncs += 1;
        let resyncs = rec.resyncs;
        // Its votes on the checkpoints still open were made from the diverged state.
        for c in self.checkpoints.values_mut() {
            if c.expected.remove(&target) {
                c.votes.remove(&target);
            }
        }
        out.push(QuorumAction::Resync {
            tick,
            target,
            source,
            counted,
            resyncs,
        });
    }

    /// A transfer that did not bring its voter back by its deadline is ordered again (and counted
    /// again), from a voter of the reference's group: the host's own vote when it is one of them,
    /// else the lowest. A voter that abstained at the last agreed checkpoint, or joined after it,
    /// is not known to hold the reference state and is never the source; with nobody of the group
    /// left the transfer waits for the next agreed checkpoint.
    fn expire_transfers(&mut self, build_tick: u64, out: &mut Vec<QuorumAction>) {
        let Some((tick, _)) = self.last_agreed else {
            return;
        };
        let expired: Vec<(Voter, bool)> = self
            .voters
            .iter()
            .filter(|(v, rec)| !self.removed.contains(v) && rec.resyncing.is_some_and(|d| build_tick >= d))
            .map(|(v, rec)| (*v, rec.tie))
            .collect();
        for (target, tie) in expired {
            let holds_reference = |v: &Voter| {
                *v != target
                    && self.reference_group.contains(v)
                    && self.record(*v).resyncing.is_none()
                    && !self.removed.contains(v)
            };
            let source = self
                .host_voter
                .filter(holds_reference)
                .or_else(|| self.reference_group.iter().copied().find(holds_reference));
            let Some(source) = source else {
                continue;
            };
            if tie {
                self.ties += 1;
                if self.ties > self.max_tie_resyncs {
                    self.unresolved = true;
                    out.push(QuorumAction::Unresolved { tick });
                    return;
                }
            }
            self.order_transfer(tick, build_tick, target, source, !tie, out);
        }
    }

    /// Votes of voters under a transfer are checked when they arrive after their checkpoint
    /// closed: `hash` equal to the reference at `tick` brings `voter` back, when `tick` is the
    /// checkpoint of its transfer or a later one. The reference need not be the newest: with a
    /// short checkpoint interval the newest has moved on by the time the vote arrives.
    pub(crate) fn recovered(&mut self, voter: Voter, tick: u64, hash: u64) -> bool {
        if self.references.get(&tick) == Some(&hash)
            && let Some(rec) = self.voters.get_mut(&voter)
            && rec.resyncing.is_some()
            && tick >= rec.resync_from
        {
            rec.resyncing = None;
            rec.tie = false;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockstep::LockstepConfig;

    fn seat(n: u16) -> Voter {
        Voter::Seat(SeatId(n))
    }

    fn quorum(self_check: bool, host: Option<Voter>) -> Quorum {
        let config = LockstepConfig {
            self_check,
            ..LockstepConfig::default()
        };
        Quorum::new(&config, host)
    }

    fn verdicts(actions: &[QuorumAction]) -> Vec<&DesyncVerdict> {
        actions
            .iter()
            .filter_map(|a| match a {
                QuorumAction::Verdict(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn open(q: &mut Quorum, tick: u64, seats: &[u16]) {
        q.open(tick, seats.iter().map(|s| seat(*s)), tick);
        for s in seats {
            q.voters.entry(seat(*s)).or_default();
        }
    }

    #[test]
    fn equal_votes_agree_without_a_verdict() {
        let mut q = quorum(true, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2]);
        for s in 0..3 {
            assert!(q.vote(seat(s), 30, 0, 7));
        }
        assert!(q.advance(31).is_empty());
        assert_eq!((q.agreed, q.desyncs, q.last_agreed()), (1, 0, Some(30)));
    }

    #[test]
    fn a_silent_voter_abstains_at_the_deadline_and_never_blocks() {
        let mut q = quorum(true, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2]);
        q.vote(seat(0), 30, 0, 7);
        q.vote(seat(1), 30, 0, 7);
        assert!(q.advance(100).is_empty(), "still inside the vote deadline");
        assert_eq!(q.agreed, 0);
        assert!(q.advance(30 + 120).is_empty());
        assert_eq!(q.agreed, 1, "closed with two votes");
        assert!(!q.vote(seat(2), 30, 0, 9), "a vote after the close is late");
        assert_eq!(q.late_votes, 1);
    }

    #[test]
    fn a_minority_seat_is_diverged_and_the_majority_is_the_source() {
        let mut q = quorum(false, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2, 3]);
        for s in [0, 1, 3] {
            q.vote(seat(s), 30, 0, 7);
        }
        q.vote(seat(2), 30, 0, 99);
        let actions = q.advance(31);
        let v = verdicts(&actions);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, VerdictKind::Diverged);
        assert_eq!(v[0].diverged, [seat(2)]);
        assert_eq!(v[0].source, Some(seat(0)), "the host votes with the majority");
        assert!(!v[0].host_diverged);
        assert!(actions.contains(&QuorumAction::Resync {
            tick: 30,
            target: seat(2),
            source: seat(0),
            counted: true,
            resyncs: 1,
        }));
        assert!(q.record(seat(2)).resyncing.is_some());
        assert!(q.record(seat(1)).resyncing.is_none());
    }

    #[test]
    fn a_host_in_the_minority_is_the_one_resynced() {
        let mut q = quorum(false, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2, 3]);
        q.vote(seat(0), 30, 0, 99);
        for s in 1..4 {
            q.vote(seat(s), 30, 0, 7);
        }
        let actions = q.advance(31);
        let v = verdicts(&actions);
        assert_eq!(v[0].kind, VerdictKind::Diverged);
        assert!(v[0].host_diverged);
        assert_eq!(v[0].diverged, [seat(0)], "only the host");
        assert_eq!(v[0].source, Some(seat(1)), "the lowest majority seat");
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, QuorumAction::Resync { .. }))
                .count(),
            1
        );
        assert!(!actions.iter().any(|a| matches!(a, QuorumAction::Remove { .. })));
    }

    #[test]
    fn a_two_voter_split_is_a_tie_and_convicts_nobody() {
        let mut q = quorum(false, Some(seat(0)));
        open(&mut q, 30, &[0, 1]);
        q.vote(seat(0), 30, 0, 7);
        q.vote(seat(1), 30, 0, 99);
        let actions = q.advance(31);
        let v = verdicts(&actions);
        assert_eq!(v[0].kind, VerdictKind::Split);
        assert!(!v[0].host_diverged);
        assert_eq!(v[0].source, Some(seat(0)), "the host's group supplies the state");
        assert!(actions.contains(&QuorumAction::Resync {
            tick: 30,
            target: seat(1),
            source: seat(0),
            counted: false,
            resyncs: 0,
        }));
        assert_eq!(
            q.record(seat(1)).resyncs,
            0,
            "a tie is never counted against a seat"
        );
        assert_eq!(q.ties, 1);
    }

    #[test]
    fn ties_past_the_limit_end_unresolved_and_never_remove_a_seat() {
        let mut q = quorum(false, Some(seat(0)));
        let mut all = Vec::new();
        for (i, tick) in [30u64, 60, 90].into_iter().enumerate() {
            // The joiner comes back between ties (its vote matches the reference).
            q.voters.entry(seat(1)).or_default().resyncing = None;
            open(&mut q, tick, &[0, 1]);
            q.vote(seat(0), tick, 0, 7);
            q.vote(seat(1), tick, 0, 99 + i as u64);
            all.extend(q.advance(tick + 1));
        }
        assert!(
            all.contains(&QuorumAction::Unresolved { tick: 90 }),
            "the third tie"
        );
        assert!(!all.iter().any(|a| matches!(a, QuorumAction::Remove { .. })));
        assert!(q.is_unresolved());
        let mut q = Quorum::new(
            &LockstepConfig {
                self_check: false,
                tie_policy: TiePolicy::EndMatch,
                ..LockstepConfig::default()
            },
            Some(seat(0)),
        );
        open(&mut q, 30, &[0, 1]);
        q.vote(seat(0), 30, 0, 7);
        q.vote(seat(1), 30, 0, 99);
        assert!(q.advance(31).contains(&QuorumAction::Unresolved { tick: 30 }));
    }

    #[test]
    fn the_self_check_repairs_a_one_off_fault_without_a_transfer() {
        let mut q = quorum(true, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2]);
        q.vote(seat(0), 30, 0, 99); // the host took a bit flip
        q.vote(seat(1), 30, 0, 7);
        q.vote(seat(2), 30, 0, 7);
        let actions = q.advance(31);
        assert_eq!(
            actions,
            [QuorumAction::Recheck {
                tick: 30,
                from: None,
                voters: vec![seat(0)],
            }]
        );
        assert!(q.advance(40).is_empty(), "waiting for the revote");
        assert!(q.vote(seat(0), 30, 1, 7), "the replay gives the majority's hash");
        let actions = q.advance(41);
        let v = verdicts(&actions);
        assert_eq!(v[0].kind, VerdictKind::SelfHealed);
        assert_eq!(v[0].self_healed, [seat(0)]);
        assert_eq!(actions.len(), 1, "no transfer, no removal");
        assert_eq!(q.record(seat(0)).self_heals, 1);
        assert_eq!(q.last_agreed(), Some(30));
    }

    #[test]
    fn a_seat_past_the_resync_limit_is_removed_and_only_that_seat() {
        let mut q = quorum(false, Some(seat(0)));
        let mut removed = Vec::new();
        for tick in [30u64, 60, 90] {
            q.voters.entry(seat(2)).or_default().resyncing = None;
            open(&mut q, tick, &[0, 1, 2, 3]);
            for s in [0, 1, 3] {
                q.vote(seat(s), tick, 0, 7);
            }
            q.vote(seat(2), tick, 0, 1000 + tick);
            for a in q.advance(tick + 1) {
                if let QuorumAction::Remove { seat, .. } = a {
                    removed.push((tick, seat));
                }
            }
        }
        assert_eq!(
            removed,
            [(90, SeatId(2))],
            "two transfers, then removal on the third"
        );
        for s in [0, 1, 3] {
            assert_eq!(q.record(seat(s)).resyncs, 0);
        }
    }

    #[test]
    fn a_host_past_the_limit_is_a_host_fault_and_no_seat_is_removed() {
        let mut q = quorum(false, Some(seat(0)));
        let mut faults = 0;
        for tick in [30u64, 60, 90] {
            q.voters.entry(seat(0)).or_default().resyncing = None;
            open(&mut q, tick, &[0, 1, 2]);
            q.vote(seat(0), tick, 0, 1000 + tick);
            q.vote(seat(1), tick, 0, 7);
            q.vote(seat(2), tick, 0, 7);
            for a in q.advance(tick + 1) {
                assert!(!matches!(a, QuorumAction::Remove { .. }));
                if matches!(a, QuorumAction::HostFault { .. }) {
                    faults += 1;
                }
            }
        }
        assert_eq!(faults, 1);
    }

    #[test]
    fn an_observer_breaks_a_two_seat_tie() {
        let mut q = quorum(false, Some(Voter::Observer));
        open(&mut q, 30, &[0, 1]);
        q.vote(seat(0), 30, 0, 99);
        q.vote(seat(1), 30, 0, 7);
        q.vote(Voter::Observer, 30, 0, 7);
        let actions = q.advance(31);
        let v = verdicts(&actions);
        assert_eq!(v[0].kind, VerdictKind::Diverged);
        assert_eq!(v[0].diverged, [seat(0)]);
        assert_eq!(v[0].source, Some(Voter::Observer));
    }

    /// An observer out of the quorum (ADR-8558) is not expected and its vote does not count; back
    /// in, it is expected again from the next checkpoint opened.
    #[test]
    fn an_observer_out_of_the_quorum_is_neither_expected_nor_counted() {
        let mut q = quorum(false, Some(Voter::Observer));
        q.forget(Voter::Observer);
        open(&mut q, 30, &[0, 1]);
        q.vote(seat(0), 30, 0, 7);
        q.vote(seat(1), 30, 0, 7);
        // Both seats voted: the checkpoint closes at once, without waiting for the observer.
        assert!(verdicts(&q.advance(31)).is_empty());
        assert_eq!(q.agreed, 1);
        assert!(!q.vote(Voter::Observer, 30, 0, 99), "out: not stored");
        q.admit(Voter::Observer);
        open(&mut q, 60, &[0, 1]);
        q.vote(seat(0), 60, 0, 8);
        q.vote(seat(1), 60, 0, 8);
        // Back in: the checkpoint waits for the observer until its deadline.
        assert!(q.advance(61).is_empty());
        assert!(q.vote(Voter::Observer, 60, 0, 8));
        assert!(verdicts(&q.advance(62)).is_empty());
        assert_eq!(q.agreed, 2);
    }

    #[test]
    fn a_diverged_voter_is_ignored_until_its_vote_matches_again() {
        let mut q = quorum(false, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2]);
        q.vote(seat(0), 30, 0, 7);
        q.vote(seat(1), 30, 0, 7);
        q.vote(seat(2), 30, 0, 99);
        assert_eq!(verdicts(&q.advance(31)).len(), 1);
        // Seat 2 is not expected at the next checkpoint: its stale vote makes no new verdict.
        open(&mut q, 60, &[0, 1, 2]);
        q.vote(seat(0), 60, 0, 8);
        q.vote(seat(1), 60, 0, 8);
        q.vote(seat(2), 60, 0, 100);
        assert!(q.advance(61).is_empty());
        assert_eq!(q.desyncs, 1);
        assert!(q.record(seat(2)).resyncing.is_some());
        // After the transfer its vote is the reference: it is back.
        open(&mut q, 90, &[0, 1, 2]);
        q.vote(seat(0), 90, 0, 9);
        q.vote(seat(1), 90, 0, 9);
        q.vote(seat(2), 90, 0, 9);
        assert!(q.advance(91).is_empty());
        assert!(q.record(seat(2)).resyncing.is_none());
    }

    /// With more checkpoints waiting for votes than the quorum keeps open, the new ones are not
    /// opened; the oldest keeps its place and is tallied at its deadline. (Evicting the oldest
    /// instead meant that one silent voter could keep every checkpoint from ever being tallied.)
    #[test]
    fn checkpoints_past_the_cap_are_not_opened_and_the_oldest_is_still_tallied() {
        let config = LockstepConfig {
            self_check: false,
            hash_every: 1,
            vote_deadline_ticks: 1_000,
            ..LockstepConfig::default()
        };
        let mut q = Quorum::new(&config, Some(seat(0)));
        for s in 0..3 {
            q.voters.entry(seat(s)).or_default();
        }
        // Seat 2 never votes: every checkpoint waits for its deadline.
        for tick in 1..=(MAX_OPEN_CHECKPOINTS as u64 + 20) {
            q.open(tick, [seat(0), seat(1), seat(2)].into_iter(), tick);
            q.vote(seat(0), tick, 0, 7);
            q.vote(seat(1), tick, 0, 7);
            assert!(
                q.advance(tick).is_empty(),
                "nothing is due before the first deadline"
            );
        }
        assert_eq!(q.checkpoints.len(), MAX_OPEN_CHECKPOINTS);
        assert_eq!(
            q.checkpoints.keys().next(),
            Some(&1),
            "the oldest checkpoint is kept"
        );
        assert!(q.advance(1_001).is_empty(), "two equal votes of three agree");
        assert_eq!(q.last_agreed(), Some(1));
        assert_eq!(q.agreed, 1);
        // Room again: the next checkpoint is opened.
        q.open(2_000, [seat(0), seat(1), seat(2)].into_iter(), 2_000);
        assert!(q.checkpoints.contains_key(&2_000));
    }
    fn resyncs(actions: &[QuorumAction]) -> Vec<(Voter, Voter)> {
        actions
            .iter()
            .filter_map(|a| match a {
                QuorumAction::Resync { target, source, .. } => Some((*target, *source)),
                _ => None,
            })
            .collect()
    }

    /// With a checkpoint every tick or two the newest reference moves on before a resynced
    /// voter's vote arrives. A vote that equals the reference of the transfer's checkpoint, or of
    /// a later one, takes the voter back; a checkpoint before the transfer says nothing.
    #[test]
    fn a_resynced_voter_is_taken_back_by_a_vote_on_a_reference_that_is_no_longer_the_newest() {
        let config = LockstepConfig {
            hash_every: 1,
            self_check: false,
            ..LockstepConfig::default()
        }
        .sanitized();
        let mut q = Quorum::new(&config, None);
        for tick in 1..=4u64 {
            open(&mut q, tick, &[0, 1, 2]);
            for s in 0..3 {
                q.vote(seat(s), tick, 0, 100 + tick);
            }
            assert!(q.advance(tick + 1).is_empty());
        }
        open(&mut q, 5, &[0, 1, 2]);
        q.vote(seat(0), 5, 0, 105);
        q.vote(seat(1), 5, 0, 105);
        q.vote(seat(2), 5, 0, 999);
        assert_eq!(resyncs(&q.advance(6)), [(seat(2), seat(0))]);
        // Seven more checkpoints close without the voter under transfer.
        for tick in 6..=12u64 {
            open(&mut q, tick, &[0, 1, 2]);
            q.vote(seat(0), tick, 0, 100 + tick);
            q.vote(seat(1), tick, 0, 100 + tick);
            assert!(q.advance(tick + 1).is_empty());
        }
        assert_eq!(q.last_agreed(), Some(12));
        assert!(
            !q.recovered(seat(2), 4, 104),
            "a checkpoint before the transfer says nothing about the state after it"
        );
        assert!(!q.recovered(seat(2), 7, 999), "a hash that is not the reference");
        assert!(q.record(seat(2)).resyncing.is_some());
        assert!(
            q.recovered(seat(2), 5, 105),
            "its vote on the transfer's checkpoint equals that checkpoint's reference"
        );
        assert!(q.record(seat(2)).resyncing.is_none());
        // It is expected again, and nothing is ordered at what was the transfer's deadline.
        let mut later = Vec::new();
        for tick in 13..=400u64 {
            open(&mut q, tick, &[0, 1, 2]);
            for s in 0..3 {
                q.vote(seat(s), tick, 0, 100 + tick);
            }
            later.extend(q.advance(tick + 1));
        }
        assert!(later.is_empty(), "{later:?}");
        assert_eq!(q.record(seat(2)).resyncs, 1);
        assert_eq!(q.agreed, 4 + 7 + 388);
    }

    /// A transfer ordered again after its deadline takes its source from the voters that agreed
    /// with the reference, not from a host that abstained (its state is not known to be the
    /// reference's).
    #[test]
    fn a_transfer_ordered_again_takes_its_source_from_the_reference_group() {
        let mut q = quorum(false, Some(seat(0)));
        open(&mut q, 30, &[0, 1, 2, 3]);
        // The host's own vote never comes; seats 1 and 2 agree, seat 3 differs.
        q.vote(seat(1), 30, 0, 7);
        q.vote(seat(2), 30, 0, 7);
        q.vote(seat(3), 30, 0, 9);
        let actions = q.advance(30 + 120);
        let v = verdicts(&actions);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].kind, v[0].source), (VerdictKind::Diverged, Some(seat(1))));
        assert_eq!(v[0].abstained, [seat(0)]);
        assert_eq!(resyncs(&actions), [(seat(3), seat(1))]);
        // Seat 3 never comes back: at the deadline the transfer is ordered again.
        let deadline = q.record(seat(3)).resyncing.expect("under transfer");
        assert!(q.advance(deadline - 1).is_empty());
        assert_eq!(
            resyncs(&q.advance(deadline)),
            [(seat(3), seat(1))],
            "from a member of the reference group, not from the host that abstained"
        );
        assert_eq!(q.record(seat(3)).resyncs, 2);
    }
}
