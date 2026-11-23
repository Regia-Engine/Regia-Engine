// SPDX-License-Identifier: MPL-2.0
//! The engine's reference desync simulation (ADR-8455): a small deterministic step over four
//! named subsystems and one-byte inputs, with a [`DesyncReplay`] hook. `regia net desync-replay`
//! registers it as the built-in hook (game [`SAMPLE_GAME_ID`]), the bundle fuzz target and the
//! format corpus use [`sample_bundle`], and the book's walkthrough replays one.

use super::bundle::{
    BundleFrame, BundleGame, BundleInput, BundlePeer, BundleSnapshot, BundleWriter,
    DESYNC_BUNDLE_FORMAT_VERSION, DesyncBundle, HexBytes,
};
use super::replay::{DesyncReplay, GameIdentity};
use super::{DesyncHash, DesyncInject, DesyncReport, SubsystemRing, locate};
use crate::lockstep::{SeatId, Voter};
use crate::payload::NetGame;

/// The sample's game id.
pub const SAMPLE_GAME_ID: &str = "regia.desync_sample";
/// The sample's subsystem names, in row order.
pub const SAMPLE_SUBSYSTEMS: [&str; 4] = ["units", "economy", "orders", "rng"];

/// The sample's [`NetGame`] (one-byte inputs).
#[derive(Debug, Clone, Copy)]
pub struct SampleGame;

impl NetGame for SampleGame {
    type Message = Vec<u8>;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = SAMPLE_GAME_ID;
    const PROTOCOL_VERSION: u32 = 1;
}

fn mix(mut h: u64, v: u64) -> u64 {
    h ^= v
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(h << 6)
        .wrapping_add(h >> 2);
    h.wrapping_mul(0x0000_0100_0000_01b3)
}

/// The sample simulation: four 64-bit subsystem states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SampleSim {
    /// The subsystem states, in [`SAMPLE_SUBSYSTEMS`] order.
    pub state: [u64; 4],
    /// A divergence to plant (tests and the agent protocol only).
    pub inject: Option<DesyncInject>,
}

impl SampleSim {
    /// Steps `tick` with each seat's input (`None`: absent), seats ascending.
    pub fn step_inputs(&mut self, tick: u64, inputs: impl IntoIterator<Item = (u16, Option<u8>)>) {
        for (seat, input) in inputs {
            let v = input.map_or(0xab, u64::from);
            self.state[0] = mix(self.state[0], u64::from(seat) ^ v);
            self.state[2] = mix(self.state[2], v.wrapping_mul(3) ^ tick);
        }
        self.state[1] = mix(self.state[1], tick.wrapping_mul(7) ^ self.state[0]);
        self.state[3] = mix(self.state[3], tick);
        if let Some(inject) = &self.inject {
            for (i, name) in SAMPLE_SUBSYSTEMS.iter().enumerate() {
                self.state[i] = inject.flip(tick, name, self.state[i]);
            }
        }
    }

    /// The state as snapshot bytes (32, little endian).
    pub fn snapshot(&self) -> Vec<u8> {
        self.state.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// The state from [`Self::snapshot`] bytes.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), String> {
        let (words, rest) = bytes.as_chunks::<8>();
        if words.len() != 4 || !rest.is_empty() {
            return Err(format!("a sample snapshot is 32 bytes, not {}", bytes.len()));
        }
        for (slot, word) in self.state.iter_mut().zip(words) {
            *slot = u64::from_le_bytes(*word);
        }
        Ok(())
    }
}

impl DesyncHash for SampleSim {
    fn world_hash(&self, _tick: u64) -> u64 {
        self.state.iter().fold(0x00c0_ffee, |h, s| mix(h, *s))
    }

    fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
        out.clear();
        out.extend(SAMPLE_SUBSYSTEMS.iter().copied().zip(self.state));
    }
}

impl DesyncReplay for SampleSim {
    fn identity(&self) -> GameIdentity {
        GameIdentity::of::<SampleGame>()
    }

    fn load(&mut self, snapshot: &BundleSnapshot) -> Result<(), String> {
        self.inject = None;
        match snapshot.tick {
            None if snapshot.data.0.is_empty() => {
                self.state = [0; 4];
                Ok(())
            }
            _ => self.restore(&snapshot.data.0),
        }
    }

    fn step(&mut self, frame: &BundleFrame) -> Result<(), String> {
        let inputs = frame.decode::<u8>().map_err(|e| e.to_string())?;
        self.step_inputs(frame.tick, inputs);
        Ok(())
    }
}

/// The input seat `seat` sends for `tick` in the sample match.
pub fn sample_input(seat: u16, tick: u64) -> u8 {
    (tick / 9 % 5) as u8 ^ (seat as u8)
}

/// A host bundle of a sample match without a network: `seats` seats (at least 2) step `ticks`
/// ticks from the start, `inject` flips one subsystem on seat `victim` (when `Some`), and the
/// bundle holds every seat's rows of the newest 240 ticks with a live report of seat 0 against the
/// victim. Deterministic: the format corpus and the fuzz target use it.
pub fn sample_bundle(seats: u16, ticks: u64, victim: u16, inject: Option<DesyncInject>) -> DesyncBundle {
    let seats = seats.max(2);
    let mut sims: Vec<SampleSim> = (0..seats).map(|_| SampleSim::default()).collect();
    if let Some(sim) = sims.get_mut(usize::from(victim)) {
        sim.inject = inject;
    }
    let mut rings: Vec<SubsystemRing> = (0..seats).map(|_| SubsystemRing::new()).collect();
    let mut frames = Vec::new();
    for tick in 0..ticks {
        let inputs: Vec<(u16, Option<u8>)> = (0..seats)
            .map(|seat| {
                (
                    seat,
                    (!(tick + u64::from(seat)).is_multiple_of(13)).then(|| sample_input(seat, tick)),
                )
            })
            .collect();
        for (sim, ring) in sims.iter_mut().zip(&mut rings) {
            sim.step_inputs(tick, inputs.iter().copied());
            ring.record(tick, sim);
        }
        frames.push(BundleFrame {
            tick,
            inputs: inputs
                .iter()
                .map(|(seat, input)| BundleInput {
                    seat: *seat,
                    input: input.map(|b| HexBytes(vec![b])),
                })
                .collect(),
            membership: Vec::new(),
        });
    }
    let window = |ring: &SubsystemRing| ring.window(0, u64::MAX);
    let peers: Vec<BundlePeer> = rings
        .iter()
        .enumerate()
        .map(|(i, ring)| BundlePeer {
            voter: Voter::Seat(SeatId(i as u16)),
            rows: window(ring),
        })
        .collect();
    let verdict = rings
        .get(usize::from(victim))
        .filter(|_| victim != 0)
        .map(|ring| {
            let found = locate(&window(ring), &window(&rings[0]));
            DesyncReport {
                checkpoint: ticks.saturating_sub(1),
                voter: Voter::Seat(SeatId(victim)),
                reference: Voter::Seat(SeatId(0)),
                first_diverged_tick: found.first_diverged_tick,
                last_agreed_tick: found.last_agreed_tick,
                subsystem: found.subsystem,
                subsystems: found.subsystems,
            }
        });
    DesyncBundle {
        format_version: DESYNC_BUNDLE_FORMAT_VERSION,
        game: BundleGame::of::<SampleGame>(),
        writer: BundleWriter::Host,
        checkpoint: ticks.saturating_sub(1),
        snapshot: BundleSnapshot::start(),
        frames,
        peers,
        verdict,
    }
}
