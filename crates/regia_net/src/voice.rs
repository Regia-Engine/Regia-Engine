// SPDX-License-Identifier: MPL-2.0
//! Voice chat over `regia_net` (`docs/specs/59-voice-chat.md` §§ 2.4, 2.10, 2.13; feature `voice`,
//! ADR-1911).
//!
//! Server-relayed: every client sends its packets to the server on the unreliable channel, and the
//! server forwards the unmodified payload to each eligible listener. The server never decodes, so it
//! links no codec. It does overwrite the sender field with the slot it assigned, enforce channel
//! membership, proximity, mutual blocks, mute hints, the per-listener cap, a transmit rate limit and
//! forced mutes, so a client can neither eavesdrop nor impersonate.
//!
//! | Piece | What |
//! |---|---|
//! | [`VoiceRelay`] | The server's routing core, plain Rust over a time in milliseconds |
//! | [`NetVoiceLink`] | The client's [`VoiceChatTransport`] |
//! | [`voice_server_system`], [`voice_client_system`] | Move packets and control between the two over net messages |
//! | [`NetVoicePlugin`] | Registers the messages and the systems |
//!
//! ```
//! use regia_net::model::PeerId;
//! use regia_net::voice::{VoiceRelay, VoiceRelaySettings};
//! use regia_audio::voice_chat::{VoiceChatSettings, VoiceControl};
//!
//! let mut relay = VoiceRelay::new(VoiceRelaySettings::default(), &VoiceChatSettings::default());
//! let a = relay.add_peer(PeerId(1)).expect("slot");
//! relay.control(PeerId(1), &VoiceControl::Join(4));
//! assert_eq!(relay.members_of(4), vec![a]);
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use regia_app::{App, FixedTime, FixedUpdate, Plugin};
use regia_audio::voice_chat::{
    Candidate, LISTENER_CAP_EXTRA, LOOPBACK_SENDER, Member, MembershipTable, RANK_STAY_IN_MARGIN_DB,
    VoiceChatResource, VoiceChatSettings, VoiceChatTransport, VoiceControl, VoiceFlags, VoiceLinkQuality,
    VoicePacket, VoiceQuality, parse_multi, pump_transport, rank_speakers_sticky, seq_diff,
};
use regia_ecs::ecs::system::SystemParam;
use regia_ecs::prelude::*;
use regia_macros::Replicate;
use regia_scene::GlobalTransform;

use crate::message::{
    Channel, Direction, NetMessage, NetMessages, NetSender, ToOwnerMessage, ToServerMessage,
    register_net_message,
};
use crate::model::{LocalPeer, NetMode, NetOwner, NetTick, PeerId};
use crate::plugin::NetSet;
use crate::send::DEFAULT_TICK_HZ;
use crate::settings::RateLimit;

/// Packets a sender may average per second (spec § 2.4).
pub const VOICE_RATE_PER_SECOND: f64 = 60.0;
/// Packets a sender may burst above the average (spec § 2.4).
pub const VOICE_BURST: f64 = 10.0;
/// Milliseconds a sender stays in a listener's active set after its last packet.
pub const ACTIVE_SENDER_MS: u64 = 1000;
/// How long a listener's audible set is kept before it is ranked again (spec 59 § 2.4: 10 frames).
const RANK_HOLD_MS: u64 = 200;
/// Packets between uplink loss reports to a sender (about 2 s of speech, spec § 2.8).
pub const LINK_REPORT_PACKETS: u32 = 100;
/// Most voice packets one peer's message carries in a tick.
pub const MAX_PACKETS_PER_MESSAGE: usize = 32;
/// Longest account tag accepted from a client.
pub const MAX_ACCOUNT_BYTES: usize = 256;
/// Most accounts one peer may block.
pub const MAX_BLOCKS: usize = 512;

const CTRL_JOIN: u8 = 1;
const CTRL_LEAVE: u8 = 2;
const CTRL_MUTE_HINT: u8 = 3;
const CTRL_BLOCK: u8 = 4;

const NOTICE_WELCOME: u8 = 1;
const NOTICE_ROSTER: u8 = 2;
const NOTICE_LEFT: u8 = 3;
const NOTICE_SERVER_MUTED: u8 = 4;
const NOTICE_QUALITY_CAP: u8 = 5;
const NOTICE_LINK: u8 = 6;

/// Voice packets from a client to the server, unreliable.
#[derive(Clone, Debug, PartialEq, Replicate)]
pub struct VoiceUp {
    /// Serialized voice packets.
    pub packets: Vec<Vec<u8>>,
}

impl NetMessage for VoiceUp {
    const DIRECTION: Direction = Direction::ToServer;
    const CHANNEL: Channel = Channel::Unreliable;
    // One message per tick at most; the per-packet limit is the relay's.
    const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
        count: 240,
        per_ticks: 60,
    });
}
impl ToServerMessage for VoiceUp {}

/// Voice packets from the server to one listener, unreliable.
#[derive(Clone, Debug, PartialEq, Replicate)]
pub struct VoiceDown {
    /// Serialized voice packets with the authenticated sender.
    pub packets: Vec<Vec<u8>>,
}

impl NetMessage for VoiceDown {
    const DIRECTION: Direction = Direction::ToOwner;
    const CHANNEL: Channel = Channel::Unreliable;
}
impl ToOwnerMessage for VoiceDown {}

/// A membership or moderation change from a client (spec § 2.10), reliable.
#[derive(Clone, Debug, PartialEq, Replicate)]
pub struct VoiceCtrl {
    /// What changed.
    pub kind: u8,
    /// Channel or sender.
    pub id: u16,
    /// On or off.
    pub on: bool,
    /// Account tag for blocks.
    pub account: String,
}

impl NetMessage for VoiceCtrl {
    const DIRECTION: Direction = Direction::ToServer;
    const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
        count: 64,
        per_ticks: 60,
    });
}
impl ToServerMessage for VoiceCtrl {}

impl VoiceCtrl {
    fn encode_control(control: &VoiceControl) -> Self {
        let (kind, id, on, account) = match control {
            VoiceControl::Join(c) => (CTRL_JOIN, *c, true, String::new()),
            VoiceControl::Leave(c) => (CTRL_LEAVE, *c, false, String::new()),
            VoiceControl::MuteHint { sender, muted } => (CTRL_MUTE_HINT, *sender, *muted, String::new()),
            VoiceControl::Block { account, blocked } => (CTRL_BLOCK, 0, *blocked, account.clone()),
        };
        Self {
            kind,
            id,
            on,
            account,
        }
    }

    fn decode_control(&self) -> Option<VoiceControl> {
        match self.kind {
            CTRL_JOIN => Some(VoiceControl::Join(self.id)),
            CTRL_LEAVE => Some(VoiceControl::Leave(self.id)),
            CTRL_MUTE_HINT => Some(VoiceControl::MuteHint {
                sender: self.id,
                muted: self.on,
            }),
            CTRL_BLOCK if self.account.len() <= MAX_ACCOUNT_BYTES => Some(VoiceControl::Block {
                account: self.account.clone(),
                blocked: self.on,
            }),
            _ => None,
        }
    }
}

/// News from the server to one client, reliable.
#[derive(Clone, Debug, PartialEq, Replicate)]
pub struct VoiceNotice {
    /// What happened.
    pub kind: u8,
    /// A slot, a quality code or a loss in percent.
    pub value: u32,
    /// The tick a forced mute ends.
    pub until: u32,
    /// The roster's account tag.
    pub account: String,
}

impl NetMessage for VoiceNotice {
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for VoiceNotice {}

/// Server-side decisions a game plugs in (spec § 2.13). Every method has a permissive default.
pub trait VoiceChatModeration: Send + Sync {
    /// May this peer join this channel? The server asks on every join, so a game enforces teams
    /// here.
    fn allow_join(&mut self, _peer: PeerId, _channel: u16) -> bool {
        true
    }
    /// May this peer transmit into this channel? Asked at most once per second per sender.
    fn allow_transmit(&mut self, _peer: PeerId, _channel: u16) -> bool {
        true
    }
}

struct PermissiveModeration;
impl VoiceChatModeration for PermissiveModeration {}

/// The relay's limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceRelaySettings {
    /// Packets per second a sender may average.
    pub rate_per_second: f64,
    /// Packets a sender may burst.
    pub burst: f64,
    /// A listener's `max_mixed_speakers`; the relay forwards this many plus
    /// [`LISTENER_CAP_EXTRA`] senders at once.
    pub max_mixed_speakers: usize,
}

impl Default for VoiceRelaySettings {
    fn default() -> Self {
        Self {
            rate_per_second: VOICE_RATE_PER_SECOND,
            burst: VOICE_BURST,
            max_mixed_speakers: 8,
        }
    }
}

/// Counters of the relay (`voice_chat.relay`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceRelayStats {
    /// Packets received from clients.
    pub packets_in: u64,
    /// Bytes received from clients.
    pub bytes_in: u64,
    /// Packet copies forwarded to listeners.
    pub forwarded: u64,
    /// Bytes forwarded to listeners.
    pub bytes_out: u64,
    /// Packets that did not parse. Each spent a token of its sender's rate bucket first and is
    /// charged to the sender ([`VoiceRelay::take_rejected`], ADR-7984).
    pub bad_packets: u64,
    /// Packets over the sender's rate limit.
    pub rate_limited: u64,
    /// Packets for a channel the sender has not joined.
    pub not_in_channel: u64,
    /// Packets from a server-muted sender or one the moderation hook refused.
    pub server_muted: u64,
    /// Packet copies dropped by a listener's cap.
    pub listener_cap_drops: u64,
    /// Joins the moderation hook refused.
    pub joins_refused: u64,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

impl Bucket {
    fn admit(&mut self, now_ms: u64, rate: f64, burst: f64) -> bool {
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * rate).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Uplink {
    next_seq: Option<u16>,
    /// Packets since the last report (the report cadence).
    packets: u32,
    /// Frames (sequence numbers) received since the last report.
    received: u32,
    /// Frames (sequence numbers) missed since the last report.
    lost: u32,
    /// Sequence numbers counted lost that a late packet may still fill (bounded).
    holes: Vec<u16>,
}

/// Most missing sequence numbers an uplink remembers for late fills.
const MAX_HOLES: usize = 64;

impl Uplink {
    /// Counts one packet that carries `step` sequence numbers; every [`LINK_REPORT_PACKETS`]
    /// packets returns the loss in percent of frames since the last report.
    fn note(&mut self, seq: u16, step: u16) -> Option<u32> {
        if let Some(next) = self.next_seq {
            let gap = seq_diff(seq, next);
            // A small forward gap is loss; a talk-spurt jump says nothing.
            if (1..=16).contains(&gap) {
                self.lost += gap as u32;
                for i in 0..gap as u16 {
                    if self.holes.len() >= MAX_HOLES {
                        self.holes.remove(0);
                    }
                    self.holes.push(next.wrapping_add(i));
                }
            }
            if gap < 0 {
                // A late packet fills only the holes it covers; a duplicate of a frame already
                // counted fills nothing and changes no count.
                for i in 0..step {
                    let q = seq.wrapping_add(i);
                    if let Some(pos) = self.holes.iter().position(|h| *h == q) {
                        self.holes.swap_remove(pos);
                        self.lost = self.lost.saturating_sub(1);
                        self.received += 1;
                    }
                }
                return None;
            }
        }
        self.next_seq = Some(seq.wrapping_add(step));
        self.received += u32::from(step);
        self.packets += 1;
        if self.packets < LINK_REPORT_PACKETS {
            return None;
        }
        let total = self.received + self.lost;
        let percent = self.lost * 100 / total.max(1);
        self.packets = 0;
        self.received = 0;
        self.lost = 0;
        Some(percent)
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveSender {
    last_ms: u64,
    priority: u8,
    bytes: usize,
}

/// The audible senders last chosen for one listener.
struct RankedSet {
    at_ms: u64,
    audible: BTreeSet<u16>,
}

/// A message the relay wants sent.
#[derive(Clone, Debug, PartialEq)]
pub enum RelayNotice {
    /// A peer got its slot.
    Welcome {
        /// The peer.
        peer: PeerId,
        /// Its slot.
        slot: u16,
    },
    /// Tell `to` which account `slot` is.
    Roster {
        /// Receiver.
        to: PeerId,
        /// Slot.
        slot: u16,
        /// Account tag.
        account: String,
    },
    /// A slot was freed.
    Left {
        /// Receiver.
        to: PeerId,
        /// Slot.
        slot: u16,
    },
    /// The server muted a peer until a tick (0 = lifted).
    ServerMuted {
        /// Receiver.
        to: PeerId,
        /// End tick.
        until_tick: u32,
    },
    /// Every sender must not exceed this profile.
    QualityCap {
        /// Receiver.
        to: PeerId,
        /// The cap.
        quality: VoiceQuality,
    },
    /// The sender's uplink loss.
    Link {
        /// Receiver.
        to: PeerId,
        /// Loss in percent.
        loss_pct: u32,
    },
}

/// The server's routing core. Plain Rust: the ECS systems feed it packets and carry its output.
#[derive(Resource)]
pub struct VoiceRelay {
    settings: VoiceRelaySettings,
    table: MembershipTable,
    slots: BTreeMap<PeerId, u16>,
    peers: BTreeMap<u16, PeerId>,
    accounts: BTreeMap<u16, String>,
    blocks: BTreeMap<u16, BTreeSet<String>>,
    buckets: BTreeMap<u16, Bucket>,
    uplinks: BTreeMap<u16, Uplink>,
    muted_until: BTreeMap<u16, u32>,
    allow_checks: BTreeMap<(u16, u16), (u64, bool)>,
    active: BTreeMap<u16, BTreeMap<u16, ActiveSender>>,
    /// Audible set per listener, reused for `RANK_HOLD_MS`.
    ranked: BTreeMap<u16, RankedSet>,
    moderation: Box<dyn VoiceChatModeration>,
    out: BTreeMap<u16, Vec<Vec<u8>>>,
    notices: VecDeque<RelayNotice>,
    quality_cap: Option<VoiceQuality>,
    tick: u32,
    stats: VoiceRelayStats,
    /// Senders of packets that did not parse, one entry per packet, oldest first; at most
    /// [`MAX_KEPT_VOICE_REJECTS`] (the oldest go first, counted in `rejected_dropped`).
    rejected: VecDeque<PeerId>,
    /// Entries `rejected` dropped because nobody took them in time (ADR-8438).
    rejected_dropped: u64,
}

/// Malformed-packet entries a [`VoiceRelay`] keeps for [`VoiceRelay::take_rejected`] (ADR-8438).
/// [`voice_server_system`] takes them every tick; a host that calls [`VoiceRelay::route`] itself
/// must call `take_rejected` as often, or the oldest entries are dropped (and counted in
/// [`VoiceRelay::rejected_dropped`]) once this many wait.
pub const MAX_KEPT_VOICE_REJECTS: usize = 4096;

impl VoiceRelay {
    /// A relay for the channels of `voice`.
    #[must_use]
    pub fn new(settings: VoiceRelaySettings, voice: &VoiceChatSettings) -> Self {
        let mut table = MembershipTable::default();
        table.channels = voice.channels.iter().map(|c| (c.id, c.clone())).collect();
        Self {
            settings,
            table,
            slots: BTreeMap::new(),
            peers: BTreeMap::new(),
            accounts: BTreeMap::new(),
            blocks: BTreeMap::new(),
            buckets: BTreeMap::new(),
            uplinks: BTreeMap::new(),
            muted_until: BTreeMap::new(),
            allow_checks: BTreeMap::new(),
            active: BTreeMap::new(),
            ranked: BTreeMap::new(),
            moderation: Box::new(PermissiveModeration),
            out: BTreeMap::new(),
            notices: VecDeque::new(),
            quality_cap: None,
            tick: 0,
            stats: VoiceRelayStats::default(),
            rejected: VecDeque::new(),
            rejected_dropped: 0,
        }
    }

    /// Installs the game's moderation hooks.
    pub fn set_moderation(&mut self, moderation: impl VoiceChatModeration + 'static) {
        self.moderation = Box::new(moderation);
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> VoiceRelayStats {
        self.stats
    }

    /// The slot of a peer.
    #[must_use]
    pub fn slot(&self, peer: PeerId) -> Option<u16> {
        self.slots.get(&peer).copied()
    }

    /// Every connected `(peer, slot)`.
    pub fn peers(&self) -> impl Iterator<Item = (PeerId, u16)> + '_ {
        self.slots.iter().map(|(p, s)| (*p, *s))
    }

    /// The slots in a channel, ascending.
    #[must_use]
    pub fn members_of(&self, channel: u16) -> Vec<u16> {
        self.table
            .members
            .iter()
            .filter(|(_, m)| m.channels.contains(&channel))
            .map(|(s, _)| *s)
            .collect()
    }

    /// Assigns a slot (the lowest free one; `0xFFFF` is loopback's). `None` when none is free.
    pub fn add_peer(&mut self, peer: PeerId) -> Option<u16> {
        if let Some(slot) = self.slots.get(&peer) {
            return Some(*slot);
        }
        let slot = (0..LOOPBACK_SENDER).find(|s| !self.peers.contains_key(s))?;
        self.slots.insert(peer, slot);
        self.peers.insert(slot, peer);
        self.table.members.insert(slot, Member::default());
        self.notices.push_back(RelayNotice::Welcome { peer, slot });
        let existing: Vec<(u16, PeerId)> = self.peers.iter().map(|(s, p)| (*s, *p)).collect();
        for (other_slot, other) in existing {
            if other == peer {
                continue;
            }
            if let Some(account) = self.accounts.get(&other_slot) {
                self.notices.push_back(RelayNotice::Roster {
                    to: peer,
                    slot: other_slot,
                    account: account.clone(),
                });
            }
        }
        if let Some(quality) = self.quality_cap {
            self.notices
                .push_back(RelayNotice::QualityCap { to: peer, quality });
        }
        Some(slot)
    }

    /// Frees a peer's slot and tells the others.
    pub fn remove_peer(&mut self, peer: PeerId) {
        let Some(slot) = self.slots.remove(&peer) else {
            return;
        };
        self.peers.remove(&slot);
        self.table.members.remove(&slot);
        self.accounts.remove(&slot);
        self.blocks.remove(&slot);
        self.buckets.remove(&slot);
        self.uplinks.remove(&slot);
        self.muted_until.remove(&slot);
        self.active.remove(&slot);
        self.ranked.remove(&slot);
        for set in self.ranked.values_mut() {
            set.audible.remove(&slot);
        }
        self.out.remove(&slot);
        self.allow_checks.retain(|(s, _), _| *s != slot);
        for member in self.table.members.values_mut() {
            member.muted.remove(&slot);
            member.blocked.remove(&slot);
        }
        for senders in self.active.values_mut() {
            senders.remove(&slot);
        }
        let others: Vec<PeerId> = self.peers.values().copied().collect();
        for to in others {
            self.notices.push_back(RelayNotice::Left { to, slot });
        }
    }

    /// Records a peer's account tag and announces it. Pass an opaque tag, never a raw platform id:
    /// clients see it (their mute lists key on it), and spec § 2.15 keeps account ids from players.
    pub fn set_account(&mut self, peer: PeerId, account: &str) {
        let Some(slot) = self.slot(peer) else {
            return;
        };
        self.accounts.insert(slot, account.to_string());
        let others: Vec<PeerId> = self.peers.values().copied().filter(|p| *p != peer).collect();
        for to in others {
            self.notices.push_back(RelayNotice::Roster {
                to,
                slot,
                account: account.to_string(),
            });
        }
        self.apply_blocks();
    }

    /// Sets a peer's replicated position (proximity routing).
    pub fn set_position(&mut self, peer: PeerId, position: [f32; 3]) {
        if let Some(member) = self.slot(peer).and_then(|s| self.table.members.get_mut(&s)) {
            member.position = position;
        }
    }

    /// The game's relevancy hook: an excluded peer is out of proximity channels (other level, dead).
    pub fn set_excluded(&mut self, peer: PeerId, excluded: bool) {
        if let Some(member) = self.slot(peer).and_then(|s| self.table.members.get_mut(&s)) {
            member.excluded = excluded;
        }
    }

    /// Mutes a peer at the server until `until_tick` (0 lifts it) and notifies them.
    pub fn server_mute(&mut self, peer: PeerId, until_tick: u32) {
        let Some(slot) = self.slot(peer) else {
            return;
        };
        if until_tick == 0 {
            self.muted_until.remove(&slot);
        } else {
            self.muted_until.insert(slot, until_tick);
        }
        self.notices
            .push_back(RelayNotice::ServerMuted { to: peer, until_tick });
    }

    /// Caps every sender at `quality` (the configured downlink ceiling, spec § 2.11).
    pub fn set_quality_cap(&mut self, quality: Option<VoiceQuality>) {
        self.quality_cap = quality;
        if let Some(quality) = quality {
            let peers: Vec<PeerId> = self.peers.values().copied().collect();
            for to in peers {
                self.notices.push_back(RelayNotice::QualityCap { to, quality });
            }
        }
    }

    /// Applies a control message from `peer`.
    pub fn control(&mut self, peer: PeerId, control: &VoiceControl) {
        let Some(slot) = self.slot(peer) else {
            return;
        };
        match control {
            VoiceControl::Join(channel) => {
                if !self.table.channels.contains_key(channel) {
                    return;
                }
                if !self.moderation.allow_join(peer, *channel) {
                    self.stats.joins_refused += 1;
                    return;
                }
                if let Some(member) = self.table.members.get_mut(&slot) {
                    member.channels.insert(*channel);
                }
            }
            VoiceControl::Leave(channel) => {
                if let Some(member) = self.table.members.get_mut(&slot) {
                    member.channels.remove(channel);
                }
            }
            VoiceControl::MuteHint { sender, muted } => {
                // Only a connected slot: any u16 was kept, up to 65,536 entries per member
                // (ADR-5870). `remove_peer` clears a slot from every set when it frees.
                if *muted && !self.table.members.contains_key(sender) {
                    return;
                }
                if let Some(member) = self.table.members.get_mut(&slot) {
                    if *muted {
                        member.muted.insert(*sender);
                    } else {
                        member.muted.remove(sender);
                    }
                }
            }
            VoiceControl::Block { account, blocked } => {
                let list = self.blocks.entry(slot).or_default();
                if *blocked {
                    if list.len() < MAX_BLOCKS {
                        list.insert(account.clone());
                    }
                } else {
                    list.remove(account);
                }
                // Only the sender's own list changed: re-resolving every member on each block
                // made one client's control messages cost members x accounts (ADR-5870).
                self.apply_blocks_of(slot);
            }
        }
    }

    /// Resolves the account block lists into the slot sets the routing table reads.
    fn apply_blocks(&mut self) {
        let slots: Vec<u16> = self.table.members.keys().copied().collect();
        for slot in slots {
            self.apply_blocks_of(slot);
        }
    }

    /// [`Self::apply_blocks`] for one member.
    fn apply_blocks_of(&mut self, slot: u16) {
        let accounts = &self.accounts;
        let blocked = self.blocks.get(&slot).map_or_else(BTreeSet::new, |list| {
            accounts
                .iter()
                .filter(|(_, a)| list.contains(*a))
                .map(|(s, _)| *s)
                .collect()
        });
        if let Some(member) = self.table.members.get_mut(&slot) {
            member.blocked = blocked;
        }
    }

    fn may_transmit(&mut self, peer: PeerId, slot: u16, channel: u16, now_ms: u64) -> bool {
        if self
            .muted_until
            .get(&slot)
            .is_some_and(|until| *until > self.tick)
        {
            return false;
        }
        let key = (slot, channel);
        if let Some((at, allowed)) = self.allow_checks.get(&key)
            && now_ms.saturating_sub(*at) < 1000
        {
            return *allowed;
        }
        let allowed = self.moderation.allow_transmit(peer, channel);
        self.allow_checks.insert(key, (now_ms, allowed));
        allowed
    }

    /// Whether `listener` may hear `sender` under its cap (spec § 2.10 rule 4, ranking of § 2.4).
    /// The relay has no audio, so recent packet size stands in for energy. The audible set is
    /// ranked again every `RANK_HOLD_MS` with a 3-unit stay-in margin.
    fn within_cap(&mut self, listener: u16, sender: u16, priority: u8, bytes: usize, now_ms: u64) -> bool {
        let cap = self.settings.max_mixed_speakers + LISTENER_CAP_EXTRA;
        let under_cap = {
            let senders = self.active.entry(listener).or_default();
            senders.retain(|_, a| now_ms.saturating_sub(a.last_ms) < ACTIVE_SENDER_MS);
            senders.insert(
                sender,
                ActiveSender {
                    last_ms: now_ms,
                    priority,
                    bytes,
                },
            );
            senders.len() <= cap
        };
        if under_cap {
            self.ranked.remove(&listener);
            return true;
        }
        if let Some(held) = self.ranked.get(&listener)
            && now_ms.saturating_sub(held.at_ms) < RANK_HOLD_MS
        {
            return held.audible.contains(&sender);
        }
        let candidates: Vec<Candidate> = self
            .active
            .get(&listener)
            .map(|senders| {
                senders
                    .iter()
                    .map(|(id, a)| Candidate {
                        sender: *id,
                        priority: a.priority,
                        energy_db: a.bytes as f32,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let current: Vec<u16> = self
            .ranked
            .get(&listener)
            .map(|held| held.audible.iter().copied().collect())
            .unwrap_or_default();
        let audible = rank_speakers_sticky(&candidates, cap, &current, RANK_STAY_IN_MARGIN_DB);
        let allowed = audible.contains(&sender);
        self.ranked.insert(
            listener,
            RankedSet {
                at_ms: now_ms,
                audible: audible.into_iter().collect(),
            },
        );
        allowed
    }

    /// Routes one packet from `peer`, queueing a copy with the authenticated sender for each
    /// eligible listener.
    pub fn route(&mut self, peer: PeerId, bytes: &[u8], now_ms: u64) {
        let _span = tracing::trace_span!("voice_chat_route").entered();
        let Some(slot) = self.slot(peer) else {
            return;
        };
        self.stats.packets_in += 1;
        self.stats.bytes_in += bytes.len() as u64;
        // The bucket comes before the parse (spec 120 § 11.2, ADR-7984): a packet that does not
        // parse still costs its sender a token, so garbage is never free, and only packets inside
        // the bucket are parsed at all.
        let (rate, burst) = (self.settings.rate_per_second, self.settings.burst);
        let bucket = self.buckets.entry(slot).or_insert(Bucket {
            tokens: burst,
            last_ms: now_ms,
        });
        if !bucket.admit(now_ms, rate, burst) {
            self.stats.rate_limited += 1;
            return;
        }
        let Ok(packet) = VoicePacket::parse(bytes) else {
            self.stats.bad_packets += 1;
            if peer != PeerId::SERVER {
                if self.rejected.len() >= MAX_KEPT_VOICE_REJECTS {
                    self.rejected.pop_front();
                    self.rejected_dropped += 1;
                }
                self.rejected.push_back(peer);
            }
            return;
        };
        let (channel, seq) = (packet.channel, packet.seq);
        // A two-frame packet covers two sequence numbers; a spare frame's packet advances by one.
        let step = match parse_multi(packet.payload) {
            Ok(frames) if packet.flags.contains(VoiceFlags::MULTI) && !frames.redundant => 2,
            _ => 1,
        };
        if let Some(loss_pct) = self.uplinks.entry(slot).or_default().note(seq, step) {
            self.notices.push_back(RelayNotice::Link { to: peer, loss_pct });
        }
        if !self
            .table
            .members
            .get(&slot)
            .is_some_and(|m| m.channels.contains(&channel))
        {
            self.stats.not_in_channel += 1;
            return;
        }
        if !self.may_transmit(peer, slot, channel, now_ms) {
            self.stats.server_muted += 1;
            return;
        }
        let priority = self.table.channels.get(&channel).map_or(0, |c| c.priority);
        let size = packet.payload.len();
        let mut stamped = bytes.to_vec();
        if let Some(field) = stamped.get_mut(1..3) {
            field.copy_from_slice(&slot.to_le_bytes());
        }
        for listener in self.table.recipients_at(slot, channel, now_ms) {
            if !self.within_cap(listener, slot, priority, size, now_ms) {
                self.stats.listener_cap_drops += 1;
                continue;
            }
            self.stats.forwarded += 1;
            self.stats.bytes_out += stamped.len() as u64;
            self.out.entry(listener).or_default().push(stamped.clone());
        }
    }

    /// Takes the queued `(listener peer, packets)`, ascending by slot.
    pub fn take_outgoing(&mut self) -> Vec<(PeerId, Vec<Vec<u8>>)> {
        let out = std::mem::take(&mut self.out);
        out.into_iter()
            .filter_map(|(slot, packets)| self.peers.get(&slot).map(|p| (*p, packets)))
            .collect()
    }

    /// Takes the queued notices, oldest first.
    pub fn take_notices(&mut self) -> Vec<RelayNotice> {
        self.notices.drain(..).collect()
    }

    /// Takes the senders of the packets that did not parse since the last call, one entry per
    /// packet, oldest first (the listen host's own player is never listed). [`voice_server_system`]
    /// charges each to its peer as `voice_malformed` (ADR-7984).
    ///
    /// A host that calls [`VoiceRelay::route`] without [`voice_server_system`] must take them as
    /// often: at most [`MAX_KEPT_VOICE_REJECTS`] wait, and older ones are dropped
    /// ([`VoiceRelay::rejected_dropped`]).
    pub fn take_rejected(&mut self) -> Vec<PeerId> {
        self.rejected.drain(..).collect()
    }

    /// Malformed-packet entries dropped because more than [`MAX_KEPT_VOICE_REJECTS`] waited for
    /// [`VoiceRelay::take_rejected`] (ADR-8438). Every dropped packet is still in `bad_packets`.
    pub fn rejected_dropped(&self) -> u64 {
        self.rejected_dropped
    }
}

/// The client's transport: what `pump_transport` talks to. [`voice_client_system`] fills the inbox
/// from [`VoiceDown`] and sends the outbox as [`VoiceUp`].
#[derive(Resource, Default)]
pub struct NetVoiceLink {
    inbox: VecDeque<Vec<u8>>,
    outbox: Vec<Vec<u8>>,
    control: Vec<VoiceControl>,
    link: Option<VoiceLinkQuality>,
    slot: Option<u16>,
    server_muted_until: u32,
}

impl NetVoiceLink {
    /// The slot the server assigned.
    #[must_use]
    pub fn slot(&self) -> Option<u16> {
        self.slot
    }

    /// The tick a forced mute ends (0 = none).
    #[must_use]
    pub fn server_muted_until(&self) -> u32 {
        self.server_muted_until
    }
}

impl VoiceChatTransport for NetVoiceLink {
    fn send(&mut self, packet: &[u8]) {
        self.outbox.push(packet.to_vec());
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        self.inbox.pop_front()
    }

    fn link_quality(&self) -> Option<VoiceLinkQuality> {
        self.link
    }
}

/// Wall milliseconds of `tick` at `tick_hz` ticks per second (the rate limiter and the active
/// speaker window are in wall time, whatever the fixed step).
fn relay_ms(tick: NetTick, tick_hz: f64) -> u64 {
    let hz = if tick_hz.is_finite() && tick_hz > 0.0 {
        tick_hz
    } else {
        DEFAULT_TICK_HZ
    };
    (f64::from(tick.0) * 1000.0 / hz) as u64
}

fn notice_message(notice: &RelayNotice) -> (PeerId, VoiceNotice) {
    let build = |kind, value, until, account: &str| VoiceNotice {
        kind,
        value,
        until,
        account: account.to_string(),
    };
    match notice {
        RelayNotice::Welcome { peer, slot } => (*peer, build(NOTICE_WELCOME, u32::from(*slot), 0, "")),
        RelayNotice::Roster { to, slot, account } => {
            (*to, build(NOTICE_ROSTER, u32::from(*slot), 0, account))
        }
        RelayNotice::Left { to, slot } => (*to, build(NOTICE_LEFT, u32::from(*slot), 0, "")),
        RelayNotice::ServerMuted { to, until_tick } => (*to, build(NOTICE_SERVER_MUTED, 0, *until_tick, "")),
        RelayNotice::QualityCap { to, quality } => {
            (*to, build(NOTICE_QUALITY_CAP, u32::from(quality.code()), 0, ""))
        }
        RelayNotice::Link { to, loss_pct } => (*to, build(NOTICE_LINK, *loss_pct, 0, "")),
    }
}

/// Applies one notice to a local session and link. The listen server's own player and a client go
/// through the same path.
fn apply_notice(session: Option<&VoiceChatResource>, link: &mut NetVoiceLink, notice: &VoiceNotice) {
    let Some(session) = session else {
        return;
    };
    let Ok(mut session) = session.0.lock() else {
        return;
    };
    match notice.kind {
        NOTICE_WELCOME => {
            let slot = notice.value as u16;
            link.slot = Some(slot);
            session.set_local_sender(slot);
            // The server starts from nothing after a (re)connect: repeat the membership and blocks.
            session.take_control();
            for channel in session.joined_channels() {
                link.control.push(VoiceControl::Join(channel));
            }
            let blocked: Vec<String> = session.user().blocked_accounts.accounts().collect();
            for account in blocked {
                link.control.push(VoiceControl::Block {
                    account,
                    blocked: true,
                });
            }
        }
        NOTICE_ROSTER => session.bind_sender(notice.value as u16, &notice.account),
        NOTICE_LEFT => session.unbind_sender(notice.value as u16),
        NOTICE_SERVER_MUTED => {
            link.server_muted_until = notice.until;
            // Spec 59 § 5: the muted client's session reports `server_muted` (event and record).
            session.note_server_muted(notice.until);
        }
        NOTICE_QUALITY_CAP => {
            if let Some(cap) = VoiceQuality::from_code(notice.value as u8)
                && cap.sample_rate() < session.quality().sample_rate()
            {
                session.set_quality(cap);
            }
        }
        NOTICE_LINK => {
            link.link = Some(VoiceLinkQuality {
                loss_pct: notice.value as f32,
                rtt_ms: 0.0,
            });
        }
        _ => {}
    }
}

/// The messages clients sent this tick.
#[derive(SystemParam)]
pub struct VoiceInbox<'w> {
    up: Res<'w, NetMessages<VoiceUp>>,
    ctrl: Res<'w, NetMessages<VoiceCtrl>>,
    fixed: Option<Res<'w, FixedTime>>,
}

/// Server side, once per fixed tick: seats new peers, routes the tick's packets, forwards them.
pub fn voice_server_system(
    mut relay: ResMut<VoiceRelay>,
    inbox: VoiceInbox,
    mode: Option<Res<NetMode>>,
    session: Option<Res<VoiceChatResource>>,
    mut link: ResMut<NetVoiceLink>,
    mut sender: NetSender,
    speakers: Query<(&NetOwner, &GlobalTransform)>,
) {
    let _span = tracing::trace_span!("voice_chat_relay").entered();
    let mode = mode.as_deref().copied().unwrap_or_default();
    if !mode.is_server() {
        return;
    }
    let tick = sender.tick();
    let tick_hz = inbox
        .fixed
        .as_ref()
        .map_or(DEFAULT_TICK_HZ, |f| 1.0 / f.step().as_secs_f64());
    let now_ms = relay_ms(tick, tick_hz);
    relay.tick = tick.0;
    let connected: BTreeSet<PeerId> = sender
        .peers()
        .into_iter()
        .chain(mode.has_local_players().then_some(PeerId::SERVER))
        .collect();
    let seated: Vec<PeerId> = relay.peers().map(|(p, _)| p).collect();
    for peer in &seated {
        if !connected.contains(peer) {
            relay.remove_peer(*peer);
        }
    }
    for peer in &connected {
        relay.add_peer(*peer);
    }
    for (owner, global) in &speakers {
        relay.set_position(owner.0, global.translation().to_array());
    }
    for message in inbox.ctrl.iter() {
        match message.msg.decode_control() {
            Some(control) => relay.control(message.from, &control),
            // An unknown kind or an over-long block account: malformed, so charged and named like
            // a voice packet that does not parse (spec 120 § 11.2, ADR-8438), never ignored.
            None => {
                sender.reject_peer(
                    message.from,
                    crate::server::RejectKind::VoiceMalformed,
                    "voice control",
                );
            }
        }
    }
    for message in inbox.up.iter() {
        for packet in message.msg.packets.iter().take(MAX_PACKETS_PER_MESSAGE) {
            relay.route(message.from, packet, now_ms);
        }
    }
    // Every packet that did not parse is charged to its sender and named (spec 120 § 11.2).
    for peer in relay.take_rejected() {
        sender.reject_peer(peer, crate::server::RejectKind::VoiceMalformed, "voice packet");
    }
    if let Some(session) = session.as_deref()
        && mode.has_local_players()
        && let Ok(mut session) = session.0.lock()
    {
        for control in session.take_control() {
            relay.control(PeerId::SERVER, &control);
        }
        for packet in session.take_outgoing() {
            relay.route(PeerId::SERVER, &packet, now_ms);
        }
    }
    for notice in relay.take_notices() {
        let (to, message) = notice_message(&notice);
        if to == PeerId::SERVER {
            apply_notice(session.as_deref(), &mut link, &message);
        } else if let Err(error) = sender.send_session_to_peer(to, message) {
            tracing::debug!(target: "regia_net", ?to, "voice notice dropped: {error}");
        }
    }
    for control in std::mem::take(&mut link.control) {
        relay.control(PeerId::SERVER, &control);
    }
    for (peer, packets) in relay.take_outgoing() {
        if peer == PeerId::SERVER {
            if let Some(session) = session.as_deref()
                && let Ok(mut session) = session.0.lock()
            {
                for packet in &packets {
                    session.receive_packet(packet);
                }
            }
        } else if let Err(error) = sender.send_session_to_peer(peer, VoiceDown { packets }) {
            tracing::debug!(target: "regia_net", ?peer, "voice packets dropped: {error}");
        }
    }
}

/// Client side, once per fixed tick: hands received packets to the session and sends its own.
pub fn voice_client_system(
    session: Option<Res<VoiceChatResource>>,
    down: Res<NetMessages<VoiceDown>>,
    notices: Res<NetMessages<VoiceNotice>>,
    mut link: ResMut<NetVoiceLink>,
    mode: Option<Res<NetMode>>,
    local: Option<Res<LocalPeer>>,
    mut sender: NetSender,
) {
    let _span = tracing::trace_span!("voice_chat_link").entered();
    if !mode.as_deref().copied().unwrap_or_default().is_client() || local.is_none_or(|l| l.0.is_none()) {
        return;
    }
    let link = &mut *link;
    for notice in notices.iter() {
        apply_notice(session.as_deref(), link, &notice.msg);
    }
    for message in down.iter() {
        for packet in message.msg.packets.iter().take(MAX_PACKETS_PER_MESSAGE) {
            link.inbox.push_back(packet.clone());
        }
    }
    let Some(session) = session.as_deref() else {
        link.inbox.clear();
        return;
    };
    if let Ok(mut session) = session.0.lock() {
        let welcomed = link.slot.is_some();
        pump_transport(&mut session, link);
        if !welcomed {
            // Not seated yet: control waits in the session, packets have no sender.
            link.outbox.clear();
            return;
        }
        for control in session.take_control() {
            link.control.push(control);
        }
    }
    for control in link.control.drain(..) {
        if let Err(error) = sender.send_session_to_server(VoiceCtrl::encode_control(&control)) {
            tracing::debug!(target: "regia_net", "voice control dropped: {error}");
        }
    }
    if !link.outbox.is_empty() {
        let packets = std::mem::take(&mut link.outbox);
        if let Err(error) = sender.send_session_to_server(VoiceUp { packets }) {
            tracing::debug!(target: "regia_net", "voice packets dropped: {error}");
        }
    }
}

/// Registers the voice messages, in a fixed order, on a world (every process does this the same way;
/// it feeds the protocol hash).
pub fn register_voice_messages(world: &mut World) {
    register_net_message::<VoiceUp>(world);
    register_net_message::<VoiceDown>(world);
    register_net_message::<VoiceCtrl>(world);
    register_net_message::<VoiceNotice>(world);
    world.init_resource::<NetVoiceLink>();
}

/// Carries voice chat over the net runtime. Add it with `NetPlugin` on every process, and
/// `regia_audio`'s `VoiceChatPlugin` on every process with a local player.
#[derive(Clone, Copy, Debug, Default)]
pub struct NetVoicePlugin {
    /// The relay's limits.
    pub relay: VoiceRelaySettings,
}

impl Plugin for NetVoicePlugin {
    fn build(&self, app: &mut App) {
        register_voice_messages(app.world_mut());
        let voice = app
            .world()
            .get_resource::<VoiceChatSettings>()
            .cloned()
            .unwrap_or_default();
        app.insert_resource(VoiceRelay::new(self.relay, &voice));
        app.add_systems(
            FixedUpdate,
            (voice_server_system, voice_client_system)
                .chain()
                .after(NetSet::Apply)
                .before(NetSet::Assign),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sec review (ADR-5870 § 8): a mute hint for any `u16` was kept, so one client could grow
    /// its member's set to 65,536 entries; only connected slots are kept now, and a block
    /// resolves the sender's own list.
    #[test]
    fn mute_hints_name_connected_slots_only_and_blocks_resolve_per_member() {
        let settings = VoiceChatSettings {
            deterministic: true,
            ..VoiceChatSettings::default()
        };
        let mut relay = VoiceRelay::new(VoiceRelaySettings::default(), &settings);
        let a = relay.add_peer(PeerId(1)).expect("slot");
        let b = relay.add_peer(PeerId(2)).expect("slot");
        relay.set_account(PeerId(1), "acct-a");
        relay.set_account(PeerId(2), "acct-b");
        for sender in 0..=u16::MAX {
            relay.control(PeerId(1), &VoiceControl::MuteHint { sender, muted: true });
        }
        let muted = &relay.table.members[&a].muted;
        assert_eq!(muted.iter().copied().collect::<Vec<_>>(), [a, b]);
        relay.control(
            PeerId(1),
            &VoiceControl::Block {
                account: "acct-b".into(),
                blocked: true,
            },
        );
        assert!(relay.table.members[&a].blocked.contains(&b));
        assert!(relay.table.members[&b].blocked.is_empty());
        relay.control(
            PeerId(1),
            &VoiceControl::Block {
                account: "acct-b".into(),
                blocked: false,
            },
        );
        assert!(relay.table.members[&a].blocked.is_empty());
    }

    /// The relay clock is wall time at the session's rate, not 60 Hz ticks.
    #[test]
    fn relay_time_follows_the_session_tick_rate() {
        assert_eq!(relay_ms(NetTick(60), 60.0), 1000);
        assert_eq!(relay_ms(NetTick(30), 30.0), 1000);
        assert_eq!(relay_ms(NetTick(128), 128.0), 1000);
        assert_eq!(relay_ms(NetTick(60), 0.0), 1000, "a bad rate falls back to 60 Hz");
        assert_eq!(relay_ms(NetTick(60), f64::NAN), 1000);
    }

    /// One lost two-frame packet is two lost frames out of four, not 2 lost of 1 received.
    #[test]
    fn uplink_loss_counts_frames_on_both_sides() {
        let mut u = Uplink::default();
        // 98 single-frame packets, then a two-frame packet (seq 98..99) that is lost, then
        // single frames to the report.
        let mut seq = 0u16;
        let mut report = None;
        for _ in 0..98 {
            report = report.or(u.note(seq, 1));
            seq = seq.wrapping_add(1);
        }
        seq = seq.wrapping_add(2); // the lost two-frame packet
        for _ in 0..2 {
            report = report.or(u.note(seq, 1));
            seq = seq.wrapping_add(1);
        }
        // 100 packets: 100 frames received, 2 lost.
        assert_eq!(report, Some(1), "2 / 102 is 1 percent");
    }

    /// A late packet fills its hole once; a duplicate of it changes nothing.
    #[test]
    fn a_duplicate_late_packet_does_not_erase_a_real_loss() {
        let mut u = Uplink::default();
        assert_eq!(u.note(0, 1), None);
        assert_eq!(u.note(1, 1), None);
        // Seq 2..3 (one two-frame packet) is missed; 4 arrives.
        assert_eq!(u.note(4, 1), None);
        assert_eq!((u.received, u.lost), (3, 2));
        // The late two-frame packet arrives: both holes fill.
        assert_eq!(u.note(2, 2), None);
        assert_eq!((u.received, u.lost), (5, 0));
        // Its duplicate, and a duplicate of an old packet, change nothing.
        assert_eq!(u.note(2, 2), None);
        assert_eq!(u.note(0, 1), None);
        assert_eq!((u.received, u.lost), (5, 0));
        // A real loss is not erased by a duplicate of an already consumed frame.
        assert_eq!(u.note(7, 1), None);
        assert_eq!(u.lost, 2);
        assert_eq!(u.note(4, 1), None);
        assert_eq!((u.received, u.lost), (6, 2));
    }
}
