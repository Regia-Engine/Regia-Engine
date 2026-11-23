// SPDX-License-Identifier: MPL-2.0
//! [`AiCadence`]: the think cadence as host settings (audit gap AI-3, original code).
//!
//! A cadence is a list of named **channels** (`decide` and `sight` always come first; a game adds `target`,
//! `route`, `squad` or anything else), each with a period in ticks, a [`PhaseRule`] and an optional per-tick
//! cap, plus a list of importance **tiers** that multiply every channel's period. The schedule is a pure
//! function of the tick, the entity id and the tier the host passes, so every peer of a lockstep match agrees.
//!
//! **Phase rule.** In a channel of period `P`, a unit of tier multiplier `m` thinks when
//! `tick % (P * m) == base + P * (sub % m)`, where `base` (in `0..P`) and `sub` come from the entity id alone.
//! Every think tick of a unit lies on its base lattice (`tick % P == base`) in every tier, and when one
//! multiplier divides another (1x, 2x, 4x) the slower tier's ticks are a subset of the faster tier's. With
//! [`PhaseRule::IdModulo`] `base = id % P` and `sub = id / P`, which is `tick % (P * m) == id % (P * m)` (DO27's
//! rule at `m = 1`).
//!
//! **Caps.** A channel's `max_per_tick` limits how many units run on one tick; [`super::AiCore::due_units`]
//! queues the rest and runs the queue oldest first (wait descending, then entity id ascending). The bounds are
//! [`AiCadence::max_queue_wait`] and [`AiCadence::max_gap`].

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{AiRng, AiSchedule};

/// The name of the built-in decision channel (always [`ChannelId::DECIDE`]).
pub const DECIDE_CHANNEL: &str = "decide";
/// The name of the built-in sighting channel (always [`ChannelId::SIGHT`]).
pub const SIGHT_CHANNEL: &str = "sight";
/// The most channels one cadence holds (the two built-ins included).
pub const MAX_CADENCE_CHANNELS: usize = 64;
/// The most importance tiers one cadence holds.
pub const MAX_CADENCE_TIERS: usize = 16;

/// The 64-bit golden-ratio constant of Fibonacci hashing (`2^64 / phi`, odd).
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// A channel of an [`AiCadence`], by position. The built-ins are [`Self::DECIDE`] and [`Self::SIGHT`]; others
/// come from [`AiCadence::add_channel`] or [`AiCadence::channel_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChannelId(u16);

impl ChannelId {
    /// The `decide` channel ([`super::AiCore::decide`]).
    pub const DECIDE: ChannelId = ChannelId(0);
    /// The `sight` channel (perception refresh).
    pub const SIGHT: ChannelId = ChannelId(1);

    /// The channel at `index` of a cadence's channel list (it may not exist in a given cadence).
    pub const fn from_index(index: u16) -> Self {
        ChannelId(index)
    }

    /// The position in the cadence's channel list.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// How a channel turns an entity id into a phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhaseRule {
    /// `base = id % period`, `sub = id / period`: DO27's rule. Even for contiguous ids; ids that share a
    /// factor with the period (all multiples of 10 on a 10-tick channel) land on the same tick.
    #[default]
    IdModulo,
    /// Fibonacci (multiplicative) hashing: `w = id * 0x9E3779B97F4A7C15 + AiRng::mix(salt, 0)` (wrapping),
    /// `base = (w * period) >> 64`, `sub = (low 64 bits of w * period) >> 32`. Spreads ids in arithmetic
    /// progressions (clustered ids) close to evenly; a different salt per channel decorrelates channels.
    Hashed {
        /// Mixed into the hash; channels with different salts get unrelated phases.
        salt: u64,
    },
}

impl PhaseRule {
    /// `(base, sub)` of `entity_id` on a channel of `period` ticks (`period >= 1`); `base < period`.
    fn split(self, period: u32, entity_id: u32) -> (u64, u64) {
        let p = u64::from(period.max(1));
        match self {
            PhaseRule::IdModulo => {
                let id = u64::from(entity_id);
                (id % p, id / p)
            }
            PhaseRule::Hashed { salt } => {
                let w = u64::from(entity_id)
                    .wrapping_mul(GOLDEN)
                    .wrapping_add(AiRng::mix(salt, 0));
                let wide = u128::from(w) * u128::from(p);
                ((wide >> 64) as u64, (wide as u64) >> 32)
            }
        }
    }
}

/// One named channel: a period, a phase rule and an optional per-tick cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CadenceChannel {
    /// The channel's name, unique in its cadence and not empty.
    pub name: String,
    /// Ticks per window at tier multiplier 1 (0 is stored as 1).
    pub ticks: u32,
    /// How entity ids map to phases.
    pub phase: PhaseRule,
    /// At most this many units run per tick (`None`: no cap; 0 is stored as 1). Units over the cap wait in a
    /// queue, oldest first.
    pub max_per_tick: Option<u32>,
}

impl CadenceChannel {
    /// A channel with the id-modulo phase and no cap.
    pub fn new(name: impl Into<String>, ticks: u32) -> Self {
        CadenceChannel {
            name: name.into(),
            ticks,
            phase: PhaseRule::IdModulo,
            max_per_tick: None,
        }
    }

    /// The same channel with another phase rule.
    pub fn with_phase(mut self, phase: PhaseRule) -> Self {
        self.phase = phase;
        self
    }

    /// The same channel with another cap.
    pub fn with_max_per_tick(mut self, max_per_tick: Option<u32>) -> Self {
        self.max_per_tick = max_per_tick;
        self
    }

    fn normalized(mut self) -> Self {
        self.ticks = self.ticks.max(1);
        self.max_per_tick = self.max_per_tick.map(|c| c.max(1));
        self
    }

    fn period(&self) -> u32 {
        self.ticks.max(1)
    }
}

/// An importance tier: every channel's period is multiplied by `multiplier` for the units the host puts in
/// it ([`super::AiAgent::cadence_tier`]). Tiers come from sim state every peer shares (distance to the
/// nearest enemy, a squad role), never from a camera or the wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CadenceTier {
    /// The period multiplier (0 is stored as 1).
    pub multiplier: u32,
}

impl CadenceTier {
    /// A tier with this multiplier.
    pub fn new(multiplier: u32) -> Self {
        CadenceTier { multiplier }
    }
}

impl Default for CadenceTier {
    fn default() -> Self {
        CadenceTier { multiplier: 1 }
    }
}

/// Why cadence settings were refused. [`Self::code`] is the stable code for agent rows and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CadenceError {
    /// `ai_cadence_empty_name`: a channel name is empty.
    EmptyName,
    /// `ai_cadence_duplicate_name`: two channels share a name.
    DuplicateName {
        /// The name used twice.
        name: String,
    },
    /// `ai_cadence_builtin_channels`: a channel list does not start with `decide` and `sight`.
    BuiltinChannels,
    /// `ai_cadence_too_many_channels`: more than [`MAX_CADENCE_CHANNELS`] channels.
    TooManyChannels,
    /// `ai_cadence_too_many_tiers`: more than [`MAX_CADENCE_TIERS`] tiers.
    TooManyTiers,
    /// `ai_cadence_unknown_channel`: the channel id is not in this cadence.
    UnknownChannel {
        /// The channel index asked for.
        index: u16,
    },
}

impl CadenceError {
    /// Stable lowercase code for host agent rows and logs.
    pub fn code(&self) -> &'static str {
        match self {
            CadenceError::EmptyName => "ai_cadence_empty_name",
            CadenceError::DuplicateName { .. } => "ai_cadence_duplicate_name",
            CadenceError::BuiltinChannels => "ai_cadence_builtin_channels",
            CadenceError::TooManyChannels => "ai_cadence_too_many_channels",
            CadenceError::TooManyTiers => "ai_cadence_too_many_tiers",
            CadenceError::UnknownChannel { .. } => "ai_cadence_unknown_channel",
        }
    }
}

impl fmt::Display for CadenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            CadenceError::EmptyName => write!(f, "{code}: a channel name is empty"),
            CadenceError::DuplicateName { name } => {
                write!(f, "{code}: channel name \"{name}\" is used twice")
            }
            CadenceError::BuiltinChannels => {
                write!(
                    f,
                    "{code}: the channel list must start with \"decide\" and \"sight\""
                )
            }
            CadenceError::TooManyChannels => write!(f, "{code}: more than {MAX_CADENCE_CHANNELS} channels"),
            CadenceError::TooManyTiers => write!(f, "{code}: more than {MAX_CADENCE_TIERS} tiers"),
            CadenceError::UnknownChannel { index } => write!(f, "{code}: no channel at index {index}"),
        }
    }
}

impl std::error::Error for CadenceError {}

/// The time-sliced schedule as host settings: named channels (`decide` and `sight` first), importance tiers
/// and per-channel caps (see the module docs for the phase rule).
///
/// The default is [`AiSchedule`]'s cadences (decide every 10 ticks, sight every 6, id-modulo phase, no caps,
/// no tiers), which give DO27's slices bit for bit. A period of zero is stored as one (every unit every
/// tick).
///
/// Serialized, the cadence writes `decision_ticks` and `sight_ticks` (the legacy fields) next to `channels`
/// and `tiers`; text with only the legacy fields reads as before, and when `channels` is present it wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AiCadenceWire", into = "AiCadenceWire")]
pub struct AiCadence {
    /// `decide`, `sight`, then the host's channels; names unique and not empty; normalized.
    channels: Vec<CadenceChannel>,
    /// Normalized (multiplier >= 1); empty means every tier is 1x.
    tiers: Vec<CadenceTier>,
}

impl Default for AiCadence {
    fn default() -> Self {
        AiCadence::new(AiSchedule::DECISION_TICKS, AiSchedule::SIGHT_TICKS)
    }
}

impl AiCadence {
    /// A schedule that spreads decisions over `decision_ticks` ticks and sighting refreshes over
    /// `sight_ticks` ticks (each at least 1), id-modulo phase, no caps, no tiers.
    pub fn new(decision_ticks: u32, sight_ticks: u32) -> Self {
        AiCadence {
            channels: vec![
                CadenceChannel::new(DECIDE_CHANNEL, decision_ticks).normalized(),
                CadenceChannel::new(SIGHT_CHANNEL, sight_ticks).normalized(),
            ],
            tiers: Vec::new(),
        }
    }

    /// Append a channel (period and cap normalized); returns its id. Refused: an empty or taken name, or more
    /// than [`MAX_CADENCE_CHANNELS`] channels.
    pub fn add_channel(&mut self, channel: CadenceChannel) -> Result<ChannelId, CadenceError> {
        if channel.name.is_empty() {
            return Err(CadenceError::EmptyName);
        }
        if self.channels.iter().any(|c| c.name == channel.name) {
            return Err(CadenceError::DuplicateName { name: channel.name });
        }
        if self.channels.len() >= MAX_CADENCE_CHANNELS {
            return Err(CadenceError::TooManyChannels);
        }
        let id = ChannelId(self.channels.len() as u16);
        self.channels.push(channel.normalized());
        Ok(id)
    }

    /// [`Self::add_channel`] as a builder.
    pub fn with_channel(mut self, channel: CadenceChannel) -> Result<Self, CadenceError> {
        self.add_channel(channel)?;
        Ok(self)
    }

    /// Replace the tiers (multipliers normalized). Refused: more than [`MAX_CADENCE_TIERS`].
    pub fn set_tiers(&mut self, tiers: Vec<CadenceTier>) -> Result<(), CadenceError> {
        if tiers.len() > MAX_CADENCE_TIERS {
            return Err(CadenceError::TooManyTiers);
        }
        self.tiers = tiers
            .into_iter()
            .map(|t| CadenceTier::new(t.multiplier.max(1)))
            .collect();
        Ok(())
    }

    /// [`Self::set_tiers`] as a builder.
    pub fn with_tiers(mut self, tiers: Vec<CadenceTier>) -> Result<Self, CadenceError> {
        self.set_tiers(tiers)?;
        Ok(self)
    }

    fn channel_slot(&mut self, channel: ChannelId) -> Result<&mut CadenceChannel, CadenceError> {
        self.channels
            .get_mut(channel.index())
            .ok_or(CadenceError::UnknownChannel { index: channel.0 })
    }

    /// Change a channel's period (0 is stored as 1).
    pub fn set_ticks(&mut self, channel: ChannelId, ticks: u32) -> Result<(), CadenceError> {
        self.channel_slot(channel)?.ticks = ticks.max(1);
        Ok(())
    }

    /// Change a channel's phase rule.
    pub fn set_phase(&mut self, channel: ChannelId, phase: PhaseRule) -> Result<(), CadenceError> {
        self.channel_slot(channel)?.phase = phase;
        Ok(())
    }

    /// Change a channel's cap (`None`: no cap; 0 is stored as 1).
    pub fn set_max_per_tick(
        &mut self,
        channel: ChannelId,
        max_per_tick: Option<u32>,
    ) -> Result<(), CadenceError> {
        self.channel_slot(channel)?.max_per_tick = max_per_tick.map(|c| c.max(1));
        Ok(())
    }

    /// The channel named `name`.
    pub fn channel_id(&self, name: &str) -> Option<ChannelId> {
        self.channels
            .iter()
            .position(|c| c.name == name)
            .map(|i| ChannelId(i as u16))
    }

    /// One channel's settings.
    pub fn channel(&self, channel: ChannelId) -> Option<&CadenceChannel> {
        self.channels.get(channel.index())
    }

    /// Every channel, `decide` and `sight` first.
    pub fn channels(&self) -> &[CadenceChannel] {
        &self.channels
    }

    /// The tiers (empty: every tier is 1x).
    pub fn tiers(&self) -> &[CadenceTier] {
        &self.tiers
    }

    /// The period multiplier of `tier`: a tier past the list uses the last one; no tiers is 1x.
    pub fn multiplier(&self, tier: u8) -> u32 {
        self.tiers
            .get(usize::from(tier))
            .or(self.tiers.last())
            .map_or(1, |t| t.multiplier.max(1))
    }

    /// The least common multiple of every tier's multiplier (1 with no tiers), saturating.
    fn multiplier_lcm(&self) -> u64 {
        self.tiers.iter().fold(1u64, |acc, t| {
            let m = u64::from(t.multiplier.max(1));
            let (mut a, mut b) = (acc, m);
            while b != 0 {
                (a, b) = (b, a % b);
            }
            (acc / a).saturating_mul(m)
        })
    }

    /// A channel's period in ticks for a unit of `tier`: `ticks * multiplier`.
    pub fn period(&self, channel: ChannelId, tier: u8) -> Option<u64> {
        let ch = self.channel(channel)?;
        Some(u64::from(ch.period()) * u64::from(self.multiplier(tier)))
    }

    /// The tick (mod [`Self::period`]) on which `entity_id` in `tier` is due: `base + ticks * (sub % m)`.
    pub fn phase(&self, channel: ChannelId, tier: u8, entity_id: u32) -> Option<u64> {
        let ch = self.channel(channel)?;
        let p = u64::from(ch.period());
        let m = u64::from(self.multiplier(tier));
        let (base, sub) = ch.phase.split(ch.period(), entity_id);
        Some(base + p * (sub % m))
    }

    /// True on the ticks `entity_id` in tier 0 is due on `channel` (false for an unknown channel).
    pub fn due(&self, channel: ChannelId, tick: u64, entity_id: u32) -> bool {
        self.due_in_tier(channel, 0, tick, entity_id)
    }

    /// True on the ticks `entity_id` in `tier` is due on `channel` (false for an unknown channel).
    pub fn due_in_tier(&self, channel: ChannelId, tier: u8, tick: u64, entity_id: u32) -> bool {
        match (self.period(channel, tier), self.phase(channel, tier, entity_id)) {
            (Some(n), Some(phase)) => tick % n == phase,
            _ => false,
        }
    }

    /// The first tick at or after `tick` on which `entity_id` in tier 0 is due (`None`: unknown channel;
    /// saturates at `u64::MAX`).
    pub fn next_tick(&self, channel: ChannelId, tick: u64, entity_id: u32) -> Option<u64> {
        self.next_tick_in_tier(channel, 0, tick, entity_id)
    }

    /// The first tick at or after `tick` on which `entity_id` in `tier` is due, if it stays in `tier` (`None`:
    /// unknown channel; saturates at `u64::MAX`). After a tier change at tick `t`, this from `t` is the unit's
    /// next think: at most `period(channel, tier) - 1` ticks later.
    pub fn next_tick_in_tier(&self, channel: ChannelId, tier: u8, tick: u64, entity_id: u32) -> Option<u64> {
        let n = self.period(channel, tier)?;
        let phase = self.phase(channel, tier, entity_id)?;
        let r = tick % n;
        let base = tick - r;
        Some(if r <= phase {
            base.saturating_add(phase)
        } else {
            base.saturating_add(n).saturating_add(phase)
        })
    }

    /// The longest a unit waits in `channel`'s queue, from the tick it fell due to the tick it runs, when at
    /// most `units` units take part each tick: `ceil(units / cap) - 1`, or 0 without a cap. `None`: unknown
    /// channel. The proof is in ADR "think cadence channels" (oldest-first queue, one entry per unit).
    pub fn max_queue_wait(&self, channel: ChannelId, units: u32) -> Option<u64> {
        let ch = self.channel(channel)?;
        Some(match ch.max_per_tick {
            None => 0,
            Some(cap) => u64::from(units.div_ceil(cap.max(1)).saturating_sub(1)),
        })
    }

    /// The longest gap between two runs of a unit on `channel` that takes part every tick:
    /// `ticks * lcm(tier multipliers) + max_queue_wait`, whatever tiers the unit moves between. With one
    /// tier per unit for the whole gap it is `ticks * multiplier + max_queue_wait`. `None`: unknown channel.
    pub fn max_gap(&self, channel: ChannelId, units: u32) -> Option<u64> {
        let ch = self.channel(channel)?;
        let wait = self.max_queue_wait(channel, units)?;
        Some(
            u64::from(ch.period())
                .saturating_mul(self.multiplier_lcm())
                .saturating_add(wait),
        )
    }

    /// Ticks per decision window (the `decide` channel's period).
    pub fn decision_ticks(&self) -> u32 {
        self.channels[ChannelId::DECIDE.index()].period()
    }

    /// Ticks per sighting window (the `sight` channel's period).
    pub fn sight_ticks(&self) -> u32 {
        self.channels[ChannelId::SIGHT.index()].period()
    }

    /// The base phase (tick mod the decision period) on which `entity_id` re-scores.
    pub fn decision_phase(&self, entity_id: u32) -> u32 {
        let ch = &self.channels[ChannelId::DECIDE.index()];
        ch.phase.split(ch.period(), entity_id).0 as u32
    }

    /// The base phase (tick mod the sight period) on which `entity_id` refreshes its sightings.
    pub fn sight_phase(&self, entity_id: u32) -> u32 {
        let ch = &self.channels[ChannelId::SIGHT.index()];
        ch.phase.split(ch.period(), entity_id).0 as u32
    }

    /// True on the tick `entity_id` (tier 0) re-scores its options: `due(ChannelId::DECIDE, ..)`.
    pub fn due_to_decide(&self, tick: u64, entity_id: u32) -> bool {
        self.due(ChannelId::DECIDE, tick, entity_id)
    }

    /// True on the tick `entity_id` (tier 0) refreshes its sightings: `due(ChannelId::SIGHT, ..)`.
    pub fn due_to_sight(&self, tick: u64, entity_id: u32) -> bool {
        self.due(ChannelId::SIGHT, tick, entity_id)
    }

    /// The first tick at or after `tick` on which `entity_id` (tier 0) re-scores.
    pub fn next_decision_tick(&self, tick: u64, entity_id: u32) -> u64 {
        self.next_tick(ChannelId::DECIDE, tick, entity_id)
            .unwrap_or(u64::MAX)
    }

    /// Check settings built by hand or read from text: names, built-ins, limits. Normalizes periods, caps and
    /// multipliers.
    fn from_parts(channels: Vec<CadenceChannel>, tiers: Vec<CadenceTier>) -> Result<Self, CadenceError> {
        let mut it = channels.into_iter();
        let (Some(decide), Some(sight)) = (it.next(), it.next()) else {
            return Err(CadenceError::BuiltinChannels);
        };
        if decide.name != DECIDE_CHANNEL || sight.name != SIGHT_CHANNEL {
            return Err(CadenceError::BuiltinChannels);
        }
        let mut cadence = AiCadence {
            channels: vec![decide.normalized(), sight.normalized()],
            tiers: Vec::new(),
        };
        for channel in it {
            cadence.add_channel(channel)?;
        }
        cadence.set_tiers(tiers)?;
        Ok(cadence)
    }
}

fn default_decision_ticks() -> u32 {
    AiSchedule::DECISION_TICKS
}

fn default_sight_ticks() -> u32 {
    AiSchedule::SIGHT_TICKS
}

/// The serialized shape of [`AiCadence`]: the legacy two fields plus the full channel list and the tiers.
#[derive(Serialize, Deserialize)]
struct AiCadenceWire {
    #[serde(default = "default_decision_ticks")]
    decision_ticks: u32,
    #[serde(default = "default_sight_ticks")]
    sight_ticks: u32,
    #[serde(default)]
    channels: Vec<CadenceChannel>,
    #[serde(default)]
    tiers: Vec<CadenceTier>,
}

impl TryFrom<AiCadenceWire> for AiCadence {
    type Error = CadenceError;

    fn try_from(wire: AiCadenceWire) -> Result<Self, CadenceError> {
        if wire.channels.is_empty() {
            AiCadence::new(wire.decision_ticks, wire.sight_ticks).with_tiers(wire.tiers)
        } else {
            AiCadence::from_parts(wire.channels, wire.tiers)
        }
    }
}

impl From<AiCadence> for AiCadenceWire {
    fn from(cadence: AiCadence) -> Self {
        AiCadenceWire {
            decision_ticks: cadence.decision_ticks(),
            sight_ticks: cadence.sight_ticks(),
            channels: cadence.channels,
            tiers: cadence.tiers,
        }
    }
}

/// A unit waiting in a capped channel's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedUnit {
    /// The entity id.
    pub entity: u32,
    /// The tick the unit fell due (its wait is `tick - since`).
    pub since: u64,
}

/// What one channel did on one tick, as counters (no timing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CadenceReport {
    /// The channel.
    pub channel: ChannelId,
    /// The tick.
    pub tick: u64,
    /// Units whose phase fell on this tick (including ones already waiting).
    pub due: u32,
    /// Units that ran (in the due list).
    pub ran: u32,
    /// Units left waiting for a later tick.
    pub carried: u32,
    /// The longest wait (ticks from falling due to running) of a unit that ran.
    pub max_wait: u64,
}

/// A candidate of one tick: `(since, entity id, slice index)`.
type Candidate = (u64, u32, usize);

/// One channel's carry-over queue (sim state) plus its last report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ChannelQueue {
    /// Waiting units, ascending entity id, one entry per id.
    pub(super) pending: Vec<QueuedUnit>,
    #[serde(skip)]
    pub(super) last: Option<CadenceReport>,
}

/// Reused buffers for [`due_into`].
#[derive(Debug, Clone, Default)]
pub(super) struct DueScratch {
    candidates: Vec<Candidate>,
    pending: Vec<QueuedUnit>,
}

/// Fill `out` with the slice indices of the units that run on `channel` this tick, ascending entity id (equal
/// ids keep slice order), updating the channel's queue. Allocation-free once the buffers are warm.
pub(super) fn due_into<U: super::AiAgent>(
    cadence: &AiCadence,
    channel: ChannelId,
    queue: &mut ChannelQueue,
    scratch: &mut DueScratch,
    tick: u64,
    units: &[U],
    out: &mut Vec<usize>,
) -> Result<CadenceReport, CadenceError> {
    out.clear();
    let ch = cadence
        .channel(channel)
        .ok_or(CadenceError::UnknownChannel { index: channel.0 })?;
    let mut report = CadenceReport {
        channel,
        tick,
        due: 0,
        ran: 0,
        carried: 0,
        max_wait: 0,
    };
    // Every list below holds at most one entry per unit: reserving `units.len()` once makes later ticks
    // allocation-free (no-ops when the capacity is already there).
    out.reserve(units.len());
    if ch.max_per_tick.is_none() && queue.pending.is_empty() {
        // Uncapped with nothing waiting: the due slice ordered by id, equal ids in slice order (DO27's stable sort
        // by id, bit for bit; the index tie-break makes the unstable sort give that order without a buffer).
        out.extend(
            units
                .iter()
                .enumerate()
                .filter(|(_, u)| cadence.due_in_tier(channel, u.cadence_tier(), tick, u.entity_id()))
                .map(|(i, _)| i),
        );
        out.sort_unstable_by_key(|&i| (units[i].entity_id(), i));
        report.due = out.len() as u32;
        report.ran = report.due;
        queue.last = Some(report);
        return Ok(report);
    }
    let cap = ch.max_per_tick.map_or(usize::MAX, |c| c.max(1) as usize);
    let DueScratch { candidates, pending } = scratch;
    candidates.clear();
    candidates.reserve(units.len());
    for (i, u) in units.iter().enumerate() {
        let id = u.entity_id();
        let due = cadence.due_in_tier(channel, u.cadence_tier(), tick, id);
        if due {
            report.due = report.due.saturating_add(1);
        }
        let waiting = queue
            .pending
            .binary_search_by_key(&id, |q| q.entity)
            .ok()
            .map(|k| queue.pending[k].since);
        match waiting {
            // Already waiting: it keeps its older place; one run serves both.
            Some(since) => candidates.push((since.min(tick), id, i)),
            None if due => candidates.push((tick, id, i)),
            None => {}
        }
    }
    // Oldest first, then lowest id, then slice order: a total order, the same on every peer.
    candidates.sort_unstable();
    let run = candidates.len().min(cap);
    pending.clear();
    pending.reserve(units.len());
    pending.extend(
        candidates[run..]
            .iter()
            .map(|&(since, entity, _)| QueuedUnit { entity, since }),
    );
    pending.sort_unstable_by_key(|q| q.entity);
    pending.dedup_by_key(|q| q.entity);
    std::mem::swap(&mut queue.pending, pending);
    let ran = &mut candidates[..run];
    report.max_wait = ran.iter().map(|&(since, _, _)| tick - since).max().unwrap_or(0);
    ran.sort_unstable_by_key(|&(_, id, i)| (id, i));
    out.extend(ran.iter().map(|&(_, _, i)| i));
    report.ran = run as u32;
    report.carried = queue.pending.len() as u32;
    queue.last = Some(report);
    Ok(report)
}
