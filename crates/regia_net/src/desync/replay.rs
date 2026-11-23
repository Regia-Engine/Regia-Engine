// SPDX-License-Identifier: MPL-2.0
//! Offline desync replay (spec 120 § 7.6, a DO27 design; ADR-8455): `regia net desync-replay`.
//!
//! A game registers a [`DesyncReplay`] hook (its deterministic step, headless) in a
//! [`ReplayRegistry`]. [`replay`] loads the bundle's snapshot into it, steps every frame in tick
//! order with each frame's seats in ascending order, hashes after every step, and compares each
//! peer's recorded rows with the replay's, peers in ascending voter order. The replay is the
//! reference: the earliest tick at which a peer's rows differ from it is the first diverging tick,
//! and the subsystem differing there is named. The live path walks seats and peers in the same
//! order (the DO27 study found its offline replay walked a hash map while the live path sorted).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::bundle::{BundleError, BundleFrame, BundleSnapshot, DesyncBundle};
use super::{DesyncHash, DesyncReport, ProbeRows, SubsystemRow, locate};
use crate::lockstep::Voter;
use crate::payload::{NetGame, protocol_id};

/// The game a replay hook steps: must match the bundle's [`super::BundleGame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameIdentity {
    /// `NetGame::GAME_ID`.
    pub game_id: String,
    /// `NetGame::PROTOCOL_VERSION`.
    pub protocol_version: u32,
    /// [`protocol_id`] of the game.
    pub protocol_id: u64,
}

impl GameIdentity {
    /// The identity of game `G` in this build.
    pub fn of<G: NetGame>() -> Self {
        Self {
            game_id: G::GAME_ID.to_owned(),
            protocol_version: G::PROTOCOL_VERSION,
            protocol_id: protocol_id::<G>(),
        }
    }
}

/// A game's deterministic step, run headless by [`replay`] (spec 120 § 7.6).
///
/// The same step the live match runs, with the same hashing ([`DesyncHash`]). Inputs arrive as the
/// bytes the bundle holds; decode them with the game's input codec ([`BundleFrame::decode`]).
pub trait DesyncReplay: DesyncHash {
    /// The game and version this hook steps.
    fn identity(&self) -> GameIdentity;
    /// Loads the state the replay starts from (`snapshot.tick` `None`: the match start).
    fn load(&mut self, snapshot: &BundleSnapshot) -> Result<(), String>;
    /// Steps one frame (seats ascending).
    fn step(&mut self, frame: &BundleFrame) -> Result<(), String>;
}

type Factory = Box<dyn Fn() -> Box<dyn DesyncReplay> + Send + Sync>;

/// Replay hooks by game id. `regia net desync-replay` uses the registry its binary built; a
/// game's own tool binary registers its hook and calls the same command code.
#[derive(Default)]
pub struct ReplayRegistry {
    hooks: BTreeMap<String, Factory>,
}

impl std::fmt::Debug for ReplayRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayRegistry")
            .field("games", &self.hooks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ReplayRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry with the engine's built-in hook: the reference simulation of
    /// [`super::sample`] (game `regia.desync_sample`).
    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry.register(super::sample::SAMPLE_GAME_ID, || {
            Box::new(super::sample::SampleSim::default())
        });
        registry
    }

    /// Registers (or replaces) the hook of `game_id`; `factory` makes a fresh simulation.
    pub fn register(
        &mut self,
        game_id: &str,
        factory: impl Fn() -> Box<dyn DesyncReplay> + Send + Sync + 'static,
    ) {
        self.hooks.insert(game_id.to_owned(), Box::new(factory));
    }

    /// The registered game ids, ascending.
    pub fn games(&self) -> Vec<&str> {
        self.hooks.keys().map(String::as_str).collect()
    }

    /// A fresh hook for `game_id`.
    pub fn hook(&self, game_id: &str) -> Option<Box<dyn DesyncReplay>> {
        self.hooks.get(game_id).map(|factory| factory())
    }

    /// Replays `bundle` through the hook registered for its game.
    pub fn replay(&self, bundle: &DesyncBundle) -> Result<ReplayVerdict, BundleError> {
        let mut hook = self
            .hook(&bundle.game.game_id)
            .ok_or_else(|| BundleError::NoHook {
                game: bundle.game.game_id.clone(),
                registered: if self.hooks.is_empty() {
                    "none".to_owned()
                } else {
                    self.games().join(", ")
                },
            })?;
        replay(bundle, hook.as_mut())
    }
}

/// What the replay found for one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReplay {
    /// The peer.
    pub voter: Voter,
    /// Its rows at ticks the replay also hashed.
    pub rows_compared: u64,
    /// The first tick its rows differ from the replay's.
    pub first_diverged_tick: Option<u64>,
    /// The newest tick before that with every hash equal.
    pub last_agreed_tick: Option<u64>,
    /// The first subsystem that differs (the peer's name order).
    pub subsystem: Option<String>,
    /// Every subsystem that differs at `first_diverged_tick`.
    pub subsystems: Vec<String>,
}

/// The offline verdict of [`replay`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayVerdict {
    /// The game replayed.
    pub game_id: String,
    /// The first and last tick stepped (`None` when the bundle has no frame).
    pub ticks: Option<(u64, u64)>,
    /// The earliest tick at which any peer differs from the replay.
    pub first_diverging_tick: Option<u64>,
    /// The peer that differs there (the lowest voter on a tie).
    pub voter: Option<Voter>,
    /// The first subsystem that differs there.
    pub subsystem: Option<String>,
    /// Every subsystem that differs there.
    pub subsystems: Vec<String>,
    /// That peer's newest tick before it with every hash equal.
    pub last_agreed_tick: Option<u64>,
    /// Every peer, ascending by voter.
    pub peers: Vec<PeerReplay>,
    /// The live probe's report from the bundle.
    pub live: Option<DesyncReport>,
    /// Whether the replay names the same voter, tick and subsystem as the live probe (`None`
    /// without a live report).
    pub matches_live: Option<bool>,
}

impl ReplayVerdict {
    /// The verdict as lines for a person (what `regia net desync-replay` prints).
    pub fn summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        let ticks = self
            .ticks
            .map_or_else(|| "no frame".to_owned(), |(a, b)| format!("ticks {a}..={b}"));
        out.push(format!("game {}: replayed {ticks}", self.game_id));
        match (self.voter, self.first_diverging_tick) {
            (Some(voter), Some(tick)) => {
                out.push(format!(
                    "first diverging tick {tick}: {voter}, subsystem {}{}",
                    self.subsystem.as_deref().unwrap_or("(world hash only)"),
                    self.last_agreed_tick
                        .map_or_else(String::new, |t| format!(", last agreed tick {t}"))
                ));
                if self.subsystems.len() > 1 {
                    out.push(format!(
                        "  every subsystem differing there: {}",
                        self.subsystems.join(", ")
                    ));
                }
            }
            _ => out.push("no divergence: every peer's rows match the replay".to_owned()),
        }
        for peer in &self.peers {
            let state = match (peer.first_diverged_tick, &peer.subsystem) {
                (Some(t), Some(s)) => format!("diverged at tick {t} ({s})"),
                (Some(t), None) => format!("diverged at tick {t} (world hash only)"),
                (None, _) => "matches".to_owned(),
            };
            out.push(format!(
                "  {}: {} rows compared, {state}",
                peer.voter, peer.rows_compared
            ));
        }
        match (&self.live, self.matches_live) {
            (Some(live), Some(same)) => out.push(format!(
                "live probe: {} at tick {} ({}): {}",
                live.voter,
                live.first_diverged_tick
                    .map_or_else(|| "?".to_owned(), |t| t.to_string()),
                live.subsystem.as_deref().unwrap_or("world hash only"),
                if same {
                    "the replay agrees"
                } else {
                    "the replay DISAGREES"
                }
            )),
            _ => out.push("live probe: none in the bundle".to_owned()),
        }
        out
    }
}

/// Replays `bundle` through `hook`: checks the game identity, loads the snapshot, steps every
/// frame and compares each peer's rows with the replay's (module docs).
pub fn replay(bundle: &DesyncBundle, hook: &mut dyn DesyncReplay) -> Result<ReplayVerdict, BundleError> {
    let _span = tracing::info_span!("regia_net::desync_replay", frames = bundle.frames.len()).entered();
    let id = hook.identity();
    if id.game_id != bundle.game.game_id {
        return Err(BundleError::GameMismatch {
            bundle: bundle.game.game_id.clone(),
            hook: id.game_id,
        });
    }
    if id.protocol_version != bundle.game.protocol_version || id.protocol_id != bundle.game.protocol_id {
        return Err(BundleError::GameVersion {
            game: id.game_id,
            bundle_version: bundle.game.protocol_version,
            bundle_id: bundle.game.protocol_id,
            engine: bundle.game.engine_version.clone(),
            hook_version: id.protocol_version,
            hook_id: id.protocol_id,
        });
    }
    // The bundle may be a struct built in memory: hold it to the same limits as a decoded one.
    let mut checked = bundle.clone();
    checked.validate()?;
    let bundle = &checked;

    hook.load(&bundle.snapshot)
        .map_err(|reason| BundleError::Replay {
            tick: bundle.snapshot.tick.unwrap_or(0),
            reason,
        })?;
    let mut ours = ReplayRows::default();
    if let Some(tick) = bundle.snapshot.tick {
        ours.record(tick, hook)?;
    }
    for frame in &bundle.frames {
        hook.step(frame).map_err(|reason| BundleError::Replay {
            tick: frame.tick,
            reason,
        })?;
        ours.record(frame.tick, hook)?;
    }
    let reference = ours.into_rows();

    let peers: Vec<PeerReplay> = bundle
        .peers
        .iter()
        .map(|peer| {
            let compared = peer
                .rows
                .rows
                .iter()
                .filter(|r| reference.rows.binary_search_by_key(&r.tick, |o| o.tick).is_ok())
                .count() as u64;
            let found = locate(&peer.rows, &reference);
            PeerReplay {
                voter: peer.voter,
                rows_compared: compared,
                first_diverged_tick: found.first_diverged_tick,
                last_agreed_tick: found.last_agreed_tick,
                subsystem: found.subsystem,
                subsystems: found.subsystems,
            }
        })
        .collect();
    // Peers are ascending by voter: the first with the smallest tick wins a tie.
    let first = peers
        .iter()
        .filter_map(|p| p.first_diverged_tick.map(|t| (t, p)))
        .min_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    let live = bundle.verdict.clone();
    let matches_live = live.as_ref().map(|r| {
        first.is_some_and(|p| {
            p.voter == r.voter && p.first_diverged_tick == r.first_diverged_tick && p.subsystem == r.subsystem
        })
    });
    Ok(ReplayVerdict {
        game_id: bundle.game.game_id.clone(),
        ticks: bundle
            .frames
            .first()
            .zip(bundle.frames.last())
            .map(|(a, b)| (a.tick, b.tick)),
        first_diverging_tick: first.and_then(|p| p.first_diverged_tick),
        voter: first.map(|p| p.voter),
        subsystem: first.and_then(|p| p.subsystem.clone()),
        subsystems: first.map(|p| p.subsystems.clone()).unwrap_or_default(),
        last_agreed_tick: first.and_then(|p| p.last_agreed_tick),
        peers,
        live,
        matches_live,
    })
}

/// The replay's own rows, every tick.
#[derive(Default)]
struct ReplayRows {
    names: Option<Vec<&'static str>>,
    rows: Vec<SubsystemRow>,
    scratch: Vec<(&'static str, u64)>,
}

impl ReplayRows {
    fn record(&mut self, tick: u64, hook: &dyn DesyncReplay) -> Result<(), BundleError> {
        let world = hook.world_hash(tick);
        self.scratch.clear();
        hook.subsystem_hashes(tick, &mut self.scratch);
        self.scratch.truncate(super::MAX_SUBSYSTEMS);
        let names = self
            .names
            .get_or_insert_with(|| self.scratch.iter().map(|(n, _)| *n).collect());
        if names.len() != self.scratch.len() || names.iter().zip(&self.scratch).any(|(a, (b, _))| a != b) {
            return Err(BundleError::Replay {
                tick,
                reason: "the hook's subsystem names changed between ticks".to_owned(),
            });
        }
        self.rows.push(SubsystemRow {
            tick,
            world,
            hashes: self.scratch.iter().map(|(_, h)| *h).collect(),
        });
        Ok(())
    }

    fn into_rows(self) -> ProbeRows {
        ProbeRows {
            names: self
                .names
                .unwrap_or_default()
                .into_iter()
                .map(|n| super::clip_name(n).to_owned())
                .collect(),
            rows: self.rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desync::sample::{SampleSim, sample_bundle};
    use crate::desync::{BundleWriter, DesyncInject};
    use crate::lockstep::SeatId;

    fn injected() -> DesyncBundle {
        sample_bundle(4, 120, 2, Some(DesyncInject::new("orders", 77)))
    }

    /// The built-in hook replays the sample to the planted seat, tick and subsystem, and agrees
    /// with the bundle's live report.
    #[test]
    fn the_builtin_hook_names_the_planted_divergence() {
        let verdict = ReplayRegistry::with_builtin()
            .replay(&injected())
            .expect("replay");
        assert_eq!(
            (
                verdict.voter,
                verdict.first_diverging_tick,
                verdict.subsystem.as_deref()
            ),
            (Some(Voter::Seat(SeatId(2))), Some(77), Some("orders")),
            "{verdict:?}"
        );
        assert_eq!(verdict.last_agreed_tick, Some(76));
        assert_eq!(verdict.matches_live, Some(true));
        assert_eq!(verdict.ticks, Some((0, 119)));
        let others: Vec<_> = verdict
            .peers
            .iter()
            .filter(|p| p.voter != Voter::Seat(SeatId(2)))
            .collect();
        assert_eq!(others.len(), 3);
        assert!(
            others
                .iter()
                .all(|p| p.first_diverged_tick.is_none() && p.rows_compared == 120)
        );
    }

    /// Acceptance (N227): a bundle from a different game version is refused with an error that
    /// names both versions and says what to do; another game and a missing hook are refused too.
    #[test]
    fn a_bundle_from_a_different_game_version_is_refused_with_a_readable_error() {
        let mut older = injected();
        older.game.protocol_version = 7;
        older.game.protocol_id ^= 0x55;
        older.game.engine_version = "0.4.2".into();
        let err = replay(&older, &mut SampleSim::default()).expect_err("another version");
        assert_eq!(err.code(), "desync_bundle_game_version");
        let text = err.to_string();
        for needle in [
            "regia.desync_sample",
            "protocol version 7",
            "protocol version 1",
            "engine 0.4.2",
            "replay it with the build that wrote it",
        ] {
            assert!(text.contains(needle), "`{needle}` missing from: {text}");
        }
        // Only the protocol id differs (the content hash or the wire version moved): refused too.
        let mut content = injected();
        content.game.protocol_id ^= 1;
        assert_eq!(
            replay(&content, &mut SampleSim::default()).map_err(|e| e.code()),
            Err("desync_bundle_game_version")
        );
        let mut other = injected();
        other.game.game_id = "studio.other".into();
        let err = replay(&other, &mut SampleSim::default()).expect_err("another game");
        assert_eq!(err.code(), "desync_bundle_game_mismatch");
        assert!(err.to_string().contains("studio.other"), "{err}");
        let err = ReplayRegistry::with_builtin()
            .replay(&other)
            .expect_err("no hook");
        assert_eq!(err.code(), "desync_replay_no_hook");
        assert!(
            err.to_string().contains("registered: regia.desync_sample"),
            "{err}"
        );
        let err = ReplayRegistry::new().replay(&other).expect_err("empty registry");
        assert!(err.to_string().contains("registered: none"), "{err}");
    }

    /// Seats step in ascending order on both paths: the sample's step is order-sensitive, a
    /// bundle whose frames list seats and peers in another order replays to the same verdict
    /// (the reader sorts), and stepping the same inputs unsorted would give another state.
    #[test]
    fn seats_and_peers_are_walked_in_sorted_order_on_both_paths() {
        let bundle = injected();
        let sorted = ReplayRegistry::with_builtin().replay(&bundle).expect("replay");
        let mut shuffled = bundle.clone();
        for frame in &mut shuffled.frames {
            frame.inputs.reverse();
        }
        shuffled.peers.reverse();
        let text = shuffled.to_ron().expect("ron");
        let read = DesyncBundle::from_ron(&text).expect("read");
        assert_eq!(read, {
            let mut b = bundle.clone();
            b.validate().expect("valid");
            b
        });
        assert_eq!(
            ReplayRegistry::with_builtin()
                .replay(&shuffled)
                .expect("in memory"),
            sorted
        );
        // The order matters to the step: unsorted inputs give another state.
        let frame = &bundle.frames[5];
        let inputs = frame.decode::<u8>().expect("inputs");
        let (mut a, mut b) = (SampleSim::default(), SampleSim::default());
        a.step_inputs(frame.tick, inputs.iter().copied());
        b.step_inputs(frame.tick, inputs.iter().rev().copied());
        assert_ne!(a.world_hash(frame.tick), b.world_hash(frame.tick));
    }

    /// A hook whose step fails or whose snapshot does not load is reported with the tick.
    #[test]
    fn hook_failures_name_the_tick() {
        let mut bad = injected();
        bad.snapshot = BundleSnapshot::at(u64::MAX - 1, vec![1, 2, 3]);
        bad.frames.clear();
        let err = replay(&bad, &mut SampleSim::default()).expect_err("bad snapshot");
        assert_eq!(err.code(), "desync_replay_failed");
        let mut seat = injected();
        seat.writer = BundleWriter::Seat(2);
        seat.frames[3].inputs[0].input = Some(crate::desync::bundle::HexBytes(vec![1, 2]));
        let err = replay(&seat, &mut SampleSim::default()).expect_err("an input that is not a u8");
        assert_eq!(err.code(), "desync_replay_failed");
        assert!(err.to_string().contains("tick 3"), "{err}");
    }
}
