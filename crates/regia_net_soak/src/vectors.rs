// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/steering_golden.rs, commit c06c17084), ADR-8012.
//! Per-system state vectors through the lockstep driver (ADR-8307).
//!
//! A game that hashes its simulation per system, one `u64` word per system and tick (the form
//! Direct Order 27's steering golden pins: 13 words a tick, `units physics weapons ...`), steps here
//! from [`LockstepFrame`]s that a real [`LockstepHost`] built from what real [`LockstepSeat`]s
//! sent, in memory, with no network between them. Every seat steps its own copy of the world
//! ([`VectorWorld`]), reports a chained hash of its rows to the quorum on every frame, and answers
//! the host's self-check and resync orders from checkpoint snapshots, so a planted divergence is
//! named, located and repaired the way a match would see it.
//!
//! - [`drive_vectors`]: the run; [`VectorRun`] holds each seat's rows, the verdicts, the host's
//!   first-diverged-tick probes and the per-system probes ([`VectorProbe`]).
//! - [`first_divergence`]: the first row and system two runs differ at (the bisect step for a
//!   golden mismatch).
//! - [`VectorText`]: the text form of a vector run (`# header`, then `<tick from 1> <16 hex
//!   digits> ...` per row), parsed and written byte for byte.
//!
//! The world decides what a frame means: it may step nothing for a frame (an input-delay gap at
//! the start) and return `None`. Each seat's rows are cut to [`VectorDrive::sim_ticks`] at the end.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use regia_net::lockstep::{
    DesyncVerdict, HostEvent, HostRole, LockstepConfig, LockstepError, LockstepFrame, LockstepHost,
    LockstepSeat, SeatEvent, SeatId, Stamp, Voter,
};

use crate::lockstep::{LockGame, LockInput};

/// A deterministic simulation that reports one word per system after each tick it steps.
///
/// `Clone` is the snapshot: the driver keeps clones at checkpoint ticks for the self-check
/// replay and for state transfers between seats.
pub trait VectorWorld: Clone {
    /// Steps `frame`. `Some(words)`: a simulation tick ran and these are its per-system words;
    /// `None`: the frame stepped nothing (for example the empty frames of the input delay).
    fn step(&mut self, frame: &LockstepFrame<LockInput>) -> Option<Vec<u64>>;
}

/// The shape of a [`drive_vectors`] run.
#[derive(Debug, Clone)]
pub struct VectorDrive {
    /// Seats, `0..seats`.
    pub seats: u16,
    /// Simulation ticks each seat must step (rows each seat must hold).
    pub sim_ticks: usize,
    /// The lockstep settings both ends use.
    pub config: LockstepConfig,
    /// Virtual milliseconds between host iterations (one tick at 60 Hz: 17).
    pub step_ms: u64,
    /// The host stops building frames at this tick even if a seat is short of rows.
    pub max_frames: u64,
}

impl VectorDrive {
    /// `seats` seats for `sim_ticks` simulation ticks at the default settings with the input
    /// delay pinned to its start value (4 ticks): the delay is never lowered, so no capture is
    /// [`Stamp::Skipped`] and capture `k` is stepped at frame `4 + k`.
    pub fn fixed_delay(seats: u16, sim_ticks: usize) -> Self {
        let mut config = LockstepConfig::default();
        config.min_delay_ticks = config.initial_delay_ticks;
        config.max_delay_ticks = config.initial_delay_ticks;
        let config = config.sanitized();
        Self {
            seats,
            sim_ticks,
            max_frames: sim_ticks as u64 * 2 + 64 + u64::from(config.max_delay_ticks),
            config,
            step_ms: 17,
        }
    }
}

/// One seat's side of a [`VectorRun`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatVectors {
    /// The seat.
    pub seat: SeatId,
    /// The per-system words of each simulation tick the seat stepped, cut to
    /// [`VectorDrive::sim_ticks`]. After a state transfer they are the source's rows up to the
    /// checkpoint, then the seat's own replay.
    pub rows: Vec<Vec<u64>>,
    /// Frames the seat stepped (replays not counted).
    pub frames: u64,
    /// Whether every frame the seat stepped followed the one before it.
    pub in_order: bool,
    /// Captures the seat's stamper skipped (a lowered delay).
    pub skipped: u64,
    /// Self-check replays the host asked of the seat.
    pub self_checks: u32,
    /// State transfers the seat took.
    pub resyncs: u32,
}

/// Where a seat's rows first differed from its transfer source's, found when the host ordered
/// the transfer (before the seat's state was replaced).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorProbe {
    /// The checkpoint of the transfer.
    pub checkpoint: u64,
    /// The seat that took the state.
    pub seat: SeatId,
    /// Whose state it took.
    pub source: Voter,
    /// Where the two runs first differ; `None` when their rows agree (the difference was not
    /// in the words).
    pub diff: Option<VectorDiff>,
}

/// The first place two vector runs differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorDiff {
    /// The row (simulation tick from 0).
    pub row: usize,
    /// The system index whose word differs; `None` when one run has no row there or the rows
    /// have different word counts.
    pub system: Option<usize>,
}

/// The outcome of [`drive_vectors`].
#[derive(Debug, Clone)]
pub struct VectorRun {
    /// Each seat, in seat order.
    pub seats: Vec<SeatVectors>,
    /// The host's verdicts, in order.
    pub verdicts: Vec<DesyncVerdict>,
    /// The host's probes: `(checkpoint, voter, first frame tick whose hash differs)`.
    pub located: Vec<(u64, Voter, u64)>,
    /// The per-system probe of each state transfer.
    pub probes: Vec<VectorProbe>,
    /// Seats the host removed (`max_resyncs` passed).
    pub removed: Vec<SeatId>,
    /// Frames the host built.
    pub frames_built: u64,
}

impl VectorRun {
    /// Whether every seat holds `sim_ticks` rows, stepped its frames in order and holds the
    /// same rows as seat 0.
    pub fn in_step(&self, sim_ticks: usize) -> bool {
        let Some(first) = self.seats.first() else {
            return false;
        };
        self.seats
            .iter()
            .all(|s| s.in_order && s.rows.len() == sim_ticks && s.rows == first.rows)
    }
}

/// The hash a seat reports after a frame: FNV-1a 64 over the previous hash and the row's words
/// (little-endian), chained over every row stepped so far. `None` (a frame that stepped nothing)
/// keeps the previous hash.
pub fn chain_hash(previous: u64, row: Option<&[u64]>) -> u64 {
    let Some(row) = row else {
        return previous;
    };
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for word in std::iter::once(previous).chain(row.iter().copied()) {
        for byte in word.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// The hash of a seat that stepped nothing yet.
pub const CHAIN_START: u64 = 0;

/// The first row and system at which `a` and `b` differ; `None` when they are equal.
pub fn first_divergence(a: &[Vec<u64>], b: &[Vec<u64>]) -> Option<VectorDiff> {
    for row in 0..a.len().max(b.len()) {
        match (a.get(row), b.get(row)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) if x.len() == y.len() => {
                return Some(VectorDiff {
                    row,
                    system: x.iter().zip(y).position(|(p, q)| p != q),
                });
            }
            _ => return Some(VectorDiff { row, system: None }),
        }
    }
    None
}

/// Checkpoint snapshots a seat keeps.
const SNAPSHOTS: usize = 16;
/// Per-frame hashes a seat keeps.
const HASHES: usize = 4_096;

#[derive(Clone)]
struct Snapshot<W> {
    world: W,
    rows: usize,
    chain: u64,
}

struct Node<W> {
    core: LockstepSeat<LockGame>,
    initial: W,
    world: W,
    rows: Vec<Vec<u64>>,
    chain: u64,
    snapshots: BTreeMap<u64, Snapshot<W>>,
    hashes: BTreeMap<u64, u64>,
    captures: u64,
    last_tick: Option<u64>,
    rec: SeatVectors,
}

impl<W: VectorWorld> Node<W> {
    /// Steps one frame on the live world and records the hash and, at a checkpoint, a snapshot.
    fn step(&mut self, frame: &LockstepFrame<LockInput>, hash_every: u32) -> u64 {
        let row = self.world.step(frame);
        self.chain = chain_hash(self.chain, row.as_deref());
        if let Some(row) = row {
            self.rows.push(row);
        }
        self.hashes.insert(frame.tick, self.chain);
        while self.hashes.len() > HASHES {
            self.hashes.pop_first();
        }
        if hash_every > 0 && frame.tick.is_multiple_of(u64::from(hash_every)) {
            self.snapshots.insert(
                frame.tick,
                Snapshot {
                    world: self.world.clone(),
                    rows: self.rows.len(),
                    chain: self.chain,
                },
            );
            while self.snapshots.len() > SNAPSHOTS {
                self.snapshots.pop_first();
            }
        }
        self.chain
    }

    /// Restarts from `snap` (`rows` is the row prefix to keep) and replays the frames after
    /// `tick` that this seat holds.
    fn restart(&mut self, tick: Option<u64>, snap: Snapshot<W>, rows: Vec<Vec<u64>>, hash_every: u32) {
        self.world = snap.world;
        self.chain = snap.chain;
        self.rows = rows;
        if let Some(tick) = tick {
            self.hashes.insert(tick, snap.chain);
            self.snapshots.insert(
                tick,
                Snapshot {
                    world: self.world.clone(),
                    rows: self.rows.len(),
                    chain: self.chain,
                },
            );
        }
        let frames: Vec<_> = self
            .core
            .frames_since(tick.map_or(0, |t| t + 1))
            .cloned()
            .collect();
        for frame in &frames {
            let hash = self.step(frame, hash_every);
            // The probe ring gets the replayed history, not the one the seat left (ADR-10147).
            self.core.report_replayed_hash(frame.tick, hash);
        }
    }

    fn submit<I: FnMut(SeatId, u64) -> LockInput>(&mut self, input: &mut I) -> Result<(), LockstepError> {
        let stamp = self.core.submit_input(&input(self.rec.seat, self.captures))?;
        if stamp == Stamp::Skipped {
            self.rec.skipped += 1;
        }
        self.captures += 1;
        Ok(())
    }
}

/// Runs `drive.seats` seats through a relay [`LockstepHost`] until every seat stepped
/// `drive.sim_ticks` simulation ticks (or the host reached `drive.max_frames`).
///
/// `input(seat, k)` is the seat's `k`-th capture; `world(seat)` builds the seat's world. Every
/// seat reports its chained hash ([`chain_hash`]) on every frame; a self-check replays from the
/// last agreed checkpoint snapshot; a resync takes the source seat's snapshot at the checkpoint
/// and replays the frames after it. A payload either end refuses ends the run with its error.
pub fn drive_vectors<W, I, F>(
    drive: &VectorDrive,
    mut input: I,
    mut world: F,
) -> Result<VectorRun, LockstepError>
where
    W: VectorWorld,
    I: FnMut(SeatId, u64) -> LockInput,
    F: FnMut(SeatId) -> W,
{
    let _span = tracing::info_span!("drive_vectors", seats = drive.seats, ticks = drive.sim_ticks).entered();
    let config = drive.config.clone();
    let hash_every = config.hash_every;
    let mut host = LockstepHost::<LockGame>::new(config.clone(), HostRole::Relay);
    let mut nodes: Vec<Node<W>> = (0..drive.seats)
        .map(|i| {
            let seat = SeatId(i);
            host.add_seat(seat);
            let w = world(seat);
            Node {
                core: LockstepSeat::new(config.clone(), seat),
                initial: w.clone(),
                world: w,
                rows: Vec::with_capacity(drive.sim_ticks),
                chain: CHAIN_START,
                snapshots: BTreeMap::new(),
                hashes: BTreeMap::new(),
                captures: 0,
                last_tick: None,
                rec: SeatVectors {
                    seat,
                    rows: Vec::new(),
                    frames: 0,
                    in_order: true,
                    skipped: 0,
                    self_checks: 0,
                    resyncs: 0,
                },
            }
        })
        .collect();
    let mut run = VectorRun {
        seats: Vec::new(),
        verdicts: Vec::new(),
        located: Vec::new(),
        probes: Vec::new(),
        removed: Vec::new(),
        frames_built: 0,
    };
    host.start(0);
    for node in &mut nodes {
        node.submit(&mut input)?;
    }
    let mut now = 0u64;
    let mut transfers: Vec<(SeatId, Voter, u64)> = Vec::new();
    while host.tick() < drive.max_frames && nodes.iter().any(|n| n.rows.len() < drive.sim_ticks) {
        for node in &mut nodes {
            for out in node.core.outgoing(now) {
                host.receive(node.rec.seat, out.lane, &out.bytes, now)?;
            }
        }
        while host.poll_frame(now).is_some() {
            run.frames_built += 1;
        }
        while let Some(event) = host.poll_event() {
            match event {
                HostEvent::Desync(verdict) => run.verdicts.push(verdict),
                HostEvent::DesyncLocated {
                    tick,
                    voter,
                    first_diverged_tick,
                } => run.located.push((tick, voter, first_diverged_tick)),
                HostEvent::SeatRemoved { seat, .. } => run.removed.push(seat),
                _ => {}
            }
        }
        for (to, out) in host.outgoing(now) {
            let Some(node) = nodes.get_mut(usize::from(to.0)) else {
                continue;
            };
            node.core.receive(out.lane, &out.bytes, now)?;
            while let Some(frame) = node.core.next_frame() {
                if node.last_tick.is_some_and(|t| frame.tick != t + 1)
                    || (node.last_tick.is_none() && frame.tick != 0)
                {
                    node.rec.in_order = false;
                }
                node.last_tick = Some(frame.tick);
                node.rec.frames += 1;
                let hash = node.step(&frame, hash_every);
                node.core.report_hash(frame.tick, hash);
                node.submit(&mut input)?;
            }
        }
        for node in &mut nodes {
            while let Some(event) = node.core.poll_event() {
                match event {
                    SeatEvent::SelfCheck { from, tick } => {
                        node.rec.self_checks += 1;
                        let snap = match from {
                            None => Some(Snapshot {
                                world: node.initial.clone(),
                                rows: 0,
                                chain: CHAIN_START,
                            }),
                            Some(t) => node.snapshots.get(&t).cloned(),
                        };
                        if let Some(snap) = snap {
                            let rows = node.rows[..snap.rows.min(node.rows.len())].to_vec();
                            node.restart(from, snap, rows, hash_every);
                            if let Some(hash) = node.hashes.get(&tick) {
                                node.core.revote(tick, *hash);
                            }
                        }
                    }
                    SeatEvent::Resync { tick, source } => transfers.push((node.rec.seat, source, tick)),
                    _ => {}
                }
            }
        }
        // The state transfers: the source seat's snapshot at the checkpoint, its rows up to
        // there, then the target's own frames after it.
        for (target, source, tick) in transfers.drain(..) {
            let Voter::Seat(from) = source else {
                continue;
            };
            let Some(src) = nodes.get(usize::from(from.0)) else {
                continue;
            };
            let Some(snap) = src.snapshots.get(&tick).cloned() else {
                continue;
            };
            let prefix = src.rows[..snap.rows.min(src.rows.len())].to_vec();
            let src_rows = src.rows.clone();
            let Some(node) = nodes.get_mut(usize::from(target.0)) else {
                continue;
            };
            run.probes.push(VectorProbe {
                checkpoint: tick,
                seat: target,
                source,
                diff: first_divergence(&src_rows, &node.rows),
            });
            node.rec.resyncs += 1;
            let chain = snap.chain;
            node.restart(Some(tick), snap, prefix, hash_every);
            node.core.resynced(tick, chain);
        }
        now += drive.step_ms;
    }
    run.seats = nodes
        .into_iter()
        .map(|mut n| {
            n.rows.truncate(drive.sim_ticks);
            n.rec.rows = n.rows;
            n.rec
        })
        .collect();
    Ok(run)
}

/// The text form of a vector run: one `#` header line, then one line per row,
/// `<row from 1> <word as 16 lowercase hex digits> ...`, each ending in `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorText {
    /// The header line, `#` included, without its line end.
    pub header: String,
    /// The rows.
    pub rows: Vec<Vec<u64>>,
}

/// Why a vector text does not parse. [`VectorTextError::code`] is the stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorTextError {
    /// The first line is not a `#` header.
    NoHeader,
    /// A row's number is missing, not a number, or not the next one.
    BadRowNumber {
        /// Line, from 1.
        line: usize,
        /// The number it should have.
        expected: usize,
    },
    /// A word is not 16 hex digits.
    BadWord {
        /// Line, from 1.
        line: usize,
    },
    /// A row has a different word count from the first row.
    Ragged {
        /// Line, from 1.
        line: usize,
        /// Words on the line.
        got: usize,
        /// Words on the first row.
        want: usize,
    },
}

impl std::fmt::Display for VectorTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHeader => f.write_str("the first line is not a `#` header"),
            Self::BadRowNumber { line, expected } => {
                write!(f, "line {line}: the row number is not {expected}")
            }
            Self::BadWord { line } => write!(f, "line {line}: a word is not 16 hex digits"),
            Self::Ragged { line, got, want } => {
                write!(f, "line {line}: {got} words, the first row has {want}")
            }
        }
    }
}

impl std::error::Error for VectorTextError {}

impl VectorTextError {
    /// The stable code: `vector_no_header`, `vector_bad_row_number`, `vector_bad_word`,
    /// `vector_ragged`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoHeader => "vector_no_header",
            Self::BadRowNumber { .. } => "vector_bad_row_number",
            Self::BadWord { .. } => "vector_bad_word",
            Self::Ragged { .. } => "vector_ragged",
        }
    }
}

impl VectorText {
    /// Parses `text`. Lines after the header that start with `#` are refused like any other
    /// malformed row; an empty last line (the final `\n`) is fine.
    pub fn parse(text: &str) -> Result<Self, VectorTextError> {
        let mut lines = text.lines();
        let header = match lines.next() {
            Some(h) if h.starts_with('#') => h.to_owned(),
            _ => return Err(VectorTextError::NoHeader),
        };
        let mut rows: Vec<Vec<u64>> = Vec::new();
        for (i, line) in lines.enumerate() {
            let line_no = i + 2;
            let expected = rows.len() + 1;
            let mut fields = line.split(' ');
            if fields.next().and_then(|n| n.parse::<usize>().ok()) != Some(expected) {
                return Err(VectorTextError::BadRowNumber {
                    line: line_no,
                    expected,
                });
            }
            let mut row = Vec::new();
            for word in fields {
                if word.len() != 16
                    || !word
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(VectorTextError::BadWord { line: line_no });
                }
                let value =
                    u64::from_str_radix(word, 16).map_err(|_| VectorTextError::BadWord { line: line_no })?;
                row.push(value);
            }
            if let Some(first) = rows.first()
                && first.len() != row.len()
            {
                return Err(VectorTextError::Ragged {
                    line: line_no,
                    got: row.len(),
                    want: first.len(),
                });
            }
            rows.push(row);
        }
        Ok(Self { header, rows })
    }

    /// The text, byte for byte the form [`VectorText::parse`] reads.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.header.len() + 1 + self.rows.len() * (6 + 17 * 13));
        out.push_str(&self.header);
        out.push('\n');
        for (i, row) in self.rows.iter().enumerate() {
            let _ = write!(out, "{}", i + 1);
            for word in row {
                let _ = write!(out, " {word:016x}");
            }
            out.push('\n');
        }
        out
    }
}

/// Lowercase hex SHA-256 (FIPS 180-4) of `bytes`: the form a golden's committed hash is written
/// in. The repository's own implementation, copied from `regia_pt::sha256` (one project; a soak
/// crate may not depend on the path tracer).
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// SHA-256 (FIPS 180-4 § 6.2), copied from `regia_pt::sha256`.
mod sha256 {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    const H0: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    /// Streaming SHA-256 (FIPS 180-4 § 6.2).
    #[derive(Debug, Clone)]
    pub(super) struct Sha256 {
        state: [u32; 8],
        block: [u8; 64],
        filled: usize,
        length: u64,
    }

    impl Default for Sha256 {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Sha256 {
        /// A fresh hasher.
        #[must_use]
        fn new() -> Self {
            Self {
                state: H0,
                block: [0; 64],
                filled: 0,
                length: 0,
            }
        }

        /// Feeds bytes.
        fn update(&mut self, mut data: &[u8]) {
            self.length = self.length.wrapping_add(data.len() as u64);
            while !data.is_empty() {
                let take = (64 - self.filled).min(data.len());
                self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
                self.filled += take;
                data = &data[take..];
                if self.filled == 64 {
                    let block = self.block;
                    self.compress(&block);
                    self.filled = 0;
                }
            }
        }

        /// The digest.
        #[must_use]
        fn finish(mut self) -> [u8; 32] {
            let bits = self.length.wrapping_mul(8);
            self.update(&[0x80]);
            while self.filled != 56 {
                self.update(&[0]);
            }
            self.update(&bits.to_be_bytes());
            let mut out = [0u8; 32];
            for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
                *chunk = word.to_be_bytes();
            }
            out
        }

        /// The digest of `data` in one call.
        #[must_use]
        pub(super) fn digest(data: &[u8]) -> [u8; 32] {
            let mut hasher = Self::new();
            hasher.update(data);
            hasher.finish()
        }

        fn compress(&mut self, block: &[u8; 64]) {
            let mut w = [0u32; 64];
            for (t, word) in block.as_chunks::<4>().0.iter().enumerate() {
                w[t] = u32::from_be_bytes(*word);
            }
            for t in 16..64 {
                let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
                let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
                w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
            for t in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[t])
                    .wrapping_add(w[t]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
                *slot = slot.wrapping_add(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_text_round_trips_and_names_its_errors() {
        let text = "# h a b\n1 0000000000000001 ffffffffffffffff\n2 0123456789abcdef 0000000000000000\n";
        let parsed = VectorText::parse(text).expect("parses");
        assert_eq!(
            parsed.rows,
            vec![vec![1, u64::MAX], vec![0x0123_4567_89ab_cdef, 0]]
        );
        assert_eq!(parsed.encode(), text);
        let code = |t: &str| VectorText::parse(t).map(|_| ()).map_err(|e| e.code());
        assert_eq!(code("1 0000000000000001\n"), Err("vector_no_header"));
        assert_eq!(code("# h\n2 0000000000000001\n"), Err("vector_bad_row_number"));
        assert_eq!(code("# h\n1 000000000000001\n"), Err("vector_bad_word"));
        assert_eq!(code("# h\n1 000000000000000G\n"), Err("vector_bad_word"));
        assert_eq!(code("# h\n1 00000000000000AA\n"), Err("vector_bad_word"));
        assert_eq!(
            code("# h\n1 0000000000000001\n2 0000000000000001 0000000000000002\n"),
            Err("vector_ragged")
        );
    }

    #[test]
    fn sha256_matches_the_fips_180_examples() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Two blocks of padding (55, 56 and 64 bytes cross the length boundary).
        assert_eq!(sha256_hex(&[b'a'; 55]).len(), 64);
        assert_ne!(sha256_hex(&[b'a'; 55]), sha256_hex(&[b'a'; 56]));
    }

    /// Counts the frames that carry inputs and remembers which capture each seat's input was.
    #[derive(Clone)]
    struct Captures(Vec<u64>);

    impl VectorWorld for Captures {
        fn step(&mut self, frame: &LockstepFrame<LockInput>) -> Option<Vec<u64>> {
            let first = frame.inputs.first()?.1.as_ref()?;
            self.0.push(u64::from(first.designate));
            Some(vec![u64::from(first.designate)])
        }
    }

    /// With the delay pinned, capture `k` is stepped as row `k` on every seat; with the default
    /// settings a calm in-memory match lowers the delay, and the capture at the lower has no tick
    /// of its own (`Stamp::Skipped`): a per-tick script then loses that tick, which is why a golden
    /// replay uses `VectorDrive::fixed_delay`.
    #[test]
    fn a_fixed_delay_keeps_every_capture_and_a_lowered_delay_skips_one() {
        let input = |_: SeatId, k: u64| LockInput {
            designate: k as u16,
            ..LockInput::default()
        };
        let drive = VectorDrive::fixed_delay(3, 1_200);
        let fixed = drive_vectors(&drive, input, |_| Captures(Vec::new())).expect("fixed run");
        assert!(fixed.in_step(1_200));
        assert!(fixed.verdicts.is_empty());
        for seat in &fixed.seats {
            assert_eq!(seat.skipped, 0);
            assert!(seat.rows.iter().enumerate().all(|(k, row)| row[0] == k as u64));
        }
        let lowering = VectorDrive {
            config: LockstepConfig::default(),
            ..drive
        };
        let free = drive_vectors(&lowering, input, |_| Captures(Vec::new())).expect("default run");
        assert!(free.in_step(1_200), "the seats still agree with each other");
        for seat in &free.seats {
            assert!(seat.skipped > 0, "seat {}: no capture was skipped", seat.seat.0);
            let gap = seat.rows.iter().enumerate().find(|(k, row)| row[0] != *k as u64);
            assert!(
                gap.is_some(),
                "seat {}: the rows do not show the lost capture",
                seat.seat.0
            );
        }
    }

    #[test]
    fn first_divergence_names_the_row_and_the_system() {
        let a = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let mut b = a.clone();
        assert_eq!(first_divergence(&a, &b), None);
        b[1][2] ^= 1;
        assert_eq!(
            first_divergence(&a, &b),
            Some(VectorDiff {
                row: 1,
                system: Some(2)
            })
        );
        assert_eq!(
            first_divergence(&a, &a[..1]),
            Some(VectorDiff { row: 1, system: None })
        );
    }

    #[test]
    fn the_chain_hash_keeps_its_value_over_an_empty_frame_and_folds_order() {
        let h = chain_hash(CHAIN_START, Some(&[1, 2]));
        assert_eq!(chain_hash(h, None), h);
        assert_ne!(chain_hash(CHAIN_START, Some(&[2, 1])), h);
        assert_ne!(chain_hash(h, Some(&[1, 2])), h);
    }
}
