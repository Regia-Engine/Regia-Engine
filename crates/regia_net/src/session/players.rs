// SPDX-License-Identifier: MPL-2.0
//! Remote players in the world (spec 77 § 3.2, § 9, § 10.2; ADR-2413): the join and leave hooks other
//! crates attach to the session, the hand-off of a `Gone` peer's entities, per-player ping and loss,
//! and the switch of the in-game stats overlay.
//!
//! - **Hooks.** [`NetPlayerHooks`] holds plain function pointers that
//!   [`net_session_system`](crate::session::net_session::net_session_system) calls on the server: a
//!   `joined` hook when a remote player reaches `InGame` (every time it does: after a travel and a
//!   resume too, so a hook that must run once keeps its own mark), a `left` hook when it leaves,
//!   before its entity is despawned. `regia_framework` installs its `GameRules` bridge here, which is
//!   how `GameRules::on_player_joined` / `on_player_left` reach sessions without `regia_net`
//!   knowing about `GameRules`.
//! - **Hand-off.** When a peer reaches `Gone`, every entity whose own [`NetOwner`] names it is handed
//!   on after the leave hooks ran: [`SessionRules::owned_hand_off`](crate::session::SessionRules::owned_hand_off)
//!   decides, and its default ([`default_hand_off`]) reads the entity's [`NetSuccessor`]: a successor
//!   peer that is still in the session (any state but `Gone`) takes it over, anything else (no
//!   successor, a successor that is gone or unknown, or `PeerId::SERVER`) gives it to the server
//!   (the [`NetOwner`] is removed), except that an entity marked [`NetDespawnWithOwner`] is
//!   despawned (KI-GP-033, ADR-10070: the character the game rules spawned for a remote player
//!   leaves with that player). [`OwnedHandOff::Despawn`] despawns it. Entities that borrow
//!   ownership (`NetOwnerLink`, a transform parent) follow their owner and are not visited.
//! - **Stats.** [`NetPlayerStats`] holds one [`PlayerLinkStats`] per remote player (server) or the
//!   link to the server (client), refreshed every [`NetPlayerStats::refresh_ms`] from the transport's
//!   own estimates (`LinkStats` for datagram peers, the loopback meters for loopback and simulated
//!   clients). Nothing new is measured.
//! - **Overlay.** [`NetStatsOverlay`] is the on/off switch of the in-game stats overlay
//!   (`net.stats_overlay`); `regia_framework::net_stats_overlay` draws it.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use serde::Serialize;

use crate::connection::LinkStats;
use crate::model::{NetOwner, NetPlayerId, PeerId};
use crate::session::meter::LinkMeter;

/// A join or leave hook: the world, the remote player's entity, its id.
pub type PlayerHook = fn(&mut World, Entity, NetPlayerId);

/// Join and leave hooks of other crates (server side). Install with [`NetPlayerHooks::add_joined`]
/// and [`NetPlayerHooks::add_left`]; adding the same function twice keeps one copy, so a plugin
/// built twice does not run its hook twice.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetPlayerHooks {
    welcomed: Vec<PlayerHook>,
    joined: Vec<PlayerHook>,
    left: Vec<PlayerHook>,
}

impl NetPlayerHooks {
    /// Adds a hook run once when a remote player's `NetPlayer` entity is spawned at its peer's
    /// welcome (before `InGame`): `regia_framework` makes it a `Player` there (spec 70 § 4).
    pub fn add_welcomed(&mut self, hook: PlayerHook) {
        if !self.welcomed.iter().any(|h| std::ptr::fn_addr_eq(*h, hook)) {
            self.welcomed.push(hook);
        }
    }

    /// The welcome hooks, in install order.
    pub fn welcomed(&self) -> &[PlayerHook] {
        &self.welcomed
    }

    /// Adds a hook run when a remote player reaches `InGame`.
    pub fn add_joined(&mut self, hook: PlayerHook) {
        if !self.joined.iter().any(|h| std::ptr::fn_addr_eq(*h, hook)) {
            self.joined.push(hook);
        }
    }

    /// Adds a hook run when a remote player leaves, before its entity is despawned.
    pub fn add_left(&mut self, hook: PlayerHook) {
        if !self.left.iter().any(|h| std::ptr::fn_addr_eq(*h, hook)) {
            self.left.push(hook);
        }
    }

    /// The join hooks, in install order.
    pub fn joined(&self) -> &[PlayerHook] {
        &self.joined
    }

    /// The leave hooks, in install order.
    pub fn left(&self) -> &[PlayerHook] {
        &self.left
    }
}

/// Server side: the peer that takes over this entity when its owner reaches `Gone` (spec 77 § 3.2,
/// ADR-2413). Read by [`default_hand_off`]; `PeerId::SERVER` names the server.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
#[reflect(Component)]
pub struct NetSuccessor(pub PeerId);

/// Server side: despawn this entity when its owner reaches `Gone` instead of giving it to the
/// server (KI-GP-033, ADR-10070). Read by [`default_hand_off`]; a live [`NetSuccessor`] still takes
/// the entity over first. `regia_framework` puts it on the character the game rules spawn for a
/// remote player, so a leaving player's character leaves with them; a rules type that wants the
/// character kept removes it in `GameRules::on_player_left`, which runs before the hand-off.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component)]
pub struct NetDespawnWithOwner;

/// What happens to an entity whose owner reached `Gone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "to", content = "peer", rename_all = "snake_case")]
pub enum OwnedHandOff {
    /// The server owns it from now on (its `NetOwner` is removed).
    Server,
    /// This peer owns it from now on.
    Peer(PeerId),
    /// It is despawned.
    Despawn,
}

impl OwnedHandOff {
    /// Lowercase name for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Peer(_) => "peer",
            Self::Despawn => "despawn",
        }
    }
}

/// The default hand-off rule (ADR-2413): the entity's [`NetSuccessor`] when that peer is still in
/// the session; else [`OwnedHandOff::Despawn`] for an entity marked [`NetDespawnWithOwner`]
/// (KI-GP-033, ADR-10070); else the server. `live` answers whether a peer is still in the session.
pub fn default_hand_off(world: &World, entity: Entity, live: &dyn Fn(PeerId) -> bool) -> OwnedHandOff {
    match world.get::<NetSuccessor>(entity) {
        Some(NetSuccessor(peer)) if *peer != PeerId::SERVER && live(*peer) => OwnedHandOff::Peer(*peer),
        _ if world.get::<NetDespawnWithOwner>(entity).is_some() => OwnedHandOff::Despawn,
        _ => OwnedHandOff::Server,
    }
}

/// One hand-off the session performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HandOffRecord {
    /// The entity, as `Entity::to_bits`.
    pub entity: u64,
    /// The peer that left.
    pub from: PeerId,
    /// What happened.
    pub to: OwnedHandOff,
}

/// Hand-offs kept in [`NetHandOffs`]; the oldest goes first.
pub const MAX_HAND_OFF_RECORDS: usize = 256;

/// The most recent hand-offs (newest last), for the agent's `net.handoffs` and tests.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetHandOffs {
    /// The records, newest last, at most [`MAX_HAND_OFF_RECORDS`].
    pub records: std::collections::VecDeque<HandOffRecord>,
    /// Every hand-off since the session started.
    pub total: u64,
}

impl NetHandOffs {
    fn push(&mut self, r: HandOffRecord) {
        if self.records.len() >= MAX_HAND_OFF_RECORDS {
            self.records.pop_front();
        }
        self.records.push_back(r);
        self.total += 1;
    }
}

/// The entities whose own [`NetOwner`] names `peer`, in entity order (deterministic).
pub fn owned_by(world: &mut World, peer: PeerId) -> Vec<Entity> {
    let mut q = world.query::<(Entity, &NetOwner)>();
    let mut out: Vec<Entity> = q
        .iter(world)
        .filter(|(_, o)| o.0 == peer)
        .map(|(e, _)| e)
        .collect();
    out.sort_by_key(|e| e.to_bits());
    out
}

/// Applies one hand-off decision and records it.
pub(crate) fn apply_hand_off(world: &mut World, entity: Entity, from: PeerId, to: OwnedHandOff) {
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return;
    };
    match to {
        OwnedHandOff::Server => {
            e.remove::<NetOwner>();
        }
        OwnedHandOff::Peer(p) => {
            e.insert(NetOwner(p));
        }
        OwnedHandOff::Despawn => e.despawn(),
    }
    tracing::info!(target: "net", entity = entity.to_bits(), %from, to = to.as_str(), "net.owned_hand_off");
    world
        .get_resource_or_insert_with(NetHandOffs::default)
        .push(HandOffRecord {
            entity: entity.to_bits(),
            from,
            to,
        });
}

/// Ping and loss of one link, from the transport's own estimates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct PlayerLinkStats {
    /// Smoothed round-trip time, milliseconds (0 until the first sample).
    pub ping_ms: f64,
    /// Round-trip variation, milliseconds.
    pub jitter_ms: f64,
    /// Fraction of server → client messages lost (0..=1).
    pub loss_to_player: f32,
    /// Fraction of client → server messages lost (0..=1); `None` where the server cannot see it
    /// (a datagram link counts the loss of what its own end sent).
    pub loss_from_player: Option<f32>,
    /// True for a simulated client (`net.sim_clients`).
    pub simulated: bool,
}

impl PlayerLinkStats {
    /// From a datagram connection's estimates (`loss_fraction` counts what this end sent).
    pub fn from_link(link: &LinkStats) -> Self {
        Self {
            ping_ms: link.rtt_ms,
            jitter_ms: link.jitter_ms,
            loss_to_player: link.loss_fraction,
            loss_from_player: None,
            simulated: false,
        }
    }

    /// From a loopback client's meter (both directions are known there).
    pub fn from_meter(meter: &LinkMeter, simulated: bool) -> Self {
        Self {
            ping_ms: meter.rtt_ms(),
            jitter_ms: meter.jitter_ms(),
            loss_to_player: meter.down.loss_fraction(),
            loss_from_player: Some(meter.up.loss_fraction()),
            simulated,
        }
    }
}

/// Default refresh interval of [`NetPlayerStats`], milliseconds (4 Hz, the overlay's text rate).
pub const PLAYER_STATS_REFRESH_MS: u64 = 250;

/// Per-player ping and loss (spec 77 § 10.2), refreshed by the session system.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct NetPlayerStats {
    /// Server: one row per remote player (players of one peer share their link's numbers).
    pub players: BTreeMap<NetPlayerId, PlayerLinkStats>,
    /// Client: the link to the server.
    pub server: Option<PlayerLinkStats>,
    /// Session clock of the last refresh, milliseconds.
    pub refreshed_ms: Option<u64>,
    /// Refresh interval, milliseconds (0 refreshes every frame).
    pub refresh_ms: u64,
}

impl Default for NetPlayerStats {
    fn default() -> Self {
        Self {
            players: BTreeMap::new(),
            server: None,
            refreshed_ms: None,
            refresh_ms: PLAYER_STATS_REFRESH_MS,
        }
    }
}

impl NetPlayerStats {
    /// The row of one player.
    pub fn get(&self, player: NetPlayerId) -> Option<&PlayerLinkStats> {
        self.players.get(&player)
    }

    /// Whether a refresh is due at `now_ms`.
    pub fn due(&self, now_ms: u64) -> bool {
        self.refreshed_ms
            .is_none_or(|t| now_ms < t || now_ms - t >= self.refresh_ms)
    }
}

/// The in-game stats overlay switch (spec 77 § 10.2): off by default, toggled by the agent's
/// `net.stats_overlay` or by the game (bind a key to [`NetStatsOverlay::toggle`]).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct NetStatsOverlay {
    /// Shown.
    pub visible: bool,
}

impl NetStatsOverlay {
    /// Flips the switch and returns the new state.
    pub fn toggle(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }
}

fn overlay_row(label: &str, state: &str, s: &PlayerLinkStats) -> String {
    let from = s
        .loss_from_player
        .map_or_else(|| "-".to_owned(), |l| format!("{:.1}", l * 100.0));
    format!(
        "{label:<8} {state:<9} ping {:>4.0}±{:<3.0} ms  loss {:>4.1}/{from} %{}",
        s.ping_ms,
        s.jitter_ms,
        s.loss_to_player * 100.0,
        if s.simulated { "  sim" } else { "" }
    )
}

/// The stats overlay's text (spec 77 § 10.2): the session role, then one line per remote player
/// (server) or for the link to the server (client) with state, ping, jitter and loss (to the
/// player / from the player, `-` where the transport cannot see it).
pub fn overlay_lines(world: &World) -> Vec<String> {
    use crate::session::lifecycle::PeerState;
    use crate::session::net_session::NetSession;
    let session = world.get_resource::<NetSession>();
    let stats = world.get_resource::<NetPlayerStats>();
    let role = session.map_or("no session", NetSession::role_name);
    let mut lines = Vec::new();
    match (session, stats) {
        (Some(session), Some(stats)) if session.server().is_some() => {
            lines.push(format!("net: {role}, {} players", stats.players.len()));
            for (player, s) in &stats.players {
                let state = session.peer_state(player.peer).map_or("-", PeerState::as_str);
                let label = format!("p{}.{}", player.peer.0, player.local);
                lines.push(overlay_row(&label, state, s));
            }
        }
        (Some(session), Some(stats)) if session.client().is_some() => {
            let state = session.state();
            lines.push(format!("net: {role}, {}", state.as_str()));
            if let Some(s) = &stats.server {
                lines.push(overlay_row("server", state.as_str(), s));
            }
        }
        _ => lines.push(format!("net: {role}")),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_kept_once() {
        // Distinct bodies: two identical functions may share one address (the Windows linker
        // folds them), and the hooks are kept once by address (ADR-4924).
        fn a(_: &mut World, _: Entity, _: NetPlayerId) {
            std::hint::black_box(1u32);
        }
        fn b(_: &mut World, _: Entity, _: NetPlayerId) {
            std::hint::black_box(2u32);
        }
        let mut hooks = NetPlayerHooks::default();
        hooks.add_joined(a);
        hooks.add_joined(a);
        hooks.add_joined(b);
        hooks.add_left(a);
        hooks.add_left(a);
        assert_eq!(hooks.joined().len(), 2);
        assert_eq!(hooks.left().len(), 1);
    }

    #[test]
    fn default_rule_prefers_a_live_successor() {
        let mut world = World::new();
        let none = world.spawn(NetOwner(PeerId(1))).id();
        let live = world.spawn((NetOwner(PeerId(1)), NetSuccessor(PeerId(2)))).id();
        let dead = world.spawn((NetOwner(PeerId(1)), NetSuccessor(PeerId(3)))).id();
        let server = world
            .spawn((NetOwner(PeerId(1)), NetSuccessor(PeerId::SERVER)))
            .id();
        let is_live = |p: PeerId| p == PeerId(2);
        assert_eq!(default_hand_off(&world, none, &is_live), OwnedHandOff::Server);
        assert_eq!(
            default_hand_off(&world, live, &is_live),
            OwnedHandOff::Peer(PeerId(2))
        );
        assert_eq!(default_hand_off(&world, dead, &is_live), OwnedHandOff::Server);
        assert_eq!(default_hand_off(&world, server, &is_live), OwnedHandOff::Server);
    }

    /// KI-GP-033 (ADR-10070): a marked entity is despawned with its owner, unless a live successor
    /// takes it over.
    #[test]
    fn default_rule_despawns_a_marked_entity_with_its_owner() {
        let mut world = World::new();
        let marked = world.spawn((NetOwner(PeerId(1)), NetDespawnWithOwner)).id();
        let marked_live = world
            .spawn((NetOwner(PeerId(1)), NetDespawnWithOwner, NetSuccessor(PeerId(2))))
            .id();
        let marked_dead = world
            .spawn((NetOwner(PeerId(1)), NetDespawnWithOwner, NetSuccessor(PeerId(3))))
            .id();
        let is_live = |p: PeerId| p == PeerId(2);
        assert_eq!(default_hand_off(&world, marked, &is_live), OwnedHandOff::Despawn);
        assert_eq!(
            default_hand_off(&world, marked_live, &is_live),
            OwnedHandOff::Peer(PeerId(2))
        );
        assert_eq!(
            default_hand_off(&world, marked_dead, &is_live),
            OwnedHandOff::Despawn
        );
    }

    #[test]
    fn hand_off_records_are_bounded() {
        let mut log = NetHandOffs::default();
        for i in 0..(MAX_HAND_OFF_RECORDS as u64 + 10) {
            log.push(HandOffRecord {
                entity: i,
                from: PeerId(1),
                to: OwnedHandOff::Server,
            });
        }
        assert_eq!(log.records.len(), MAX_HAND_OFF_RECORDS);
        assert_eq!(log.total, MAX_HAND_OFF_RECORDS as u64 + 10);
        assert_eq!(log.records.front().map(|r| r.entity), Some(10));
    }

    #[test]
    fn stats_refresh_interval() {
        let mut s = NetPlayerStats::default();
        assert!(s.due(0));
        s.refreshed_ms = Some(1000);
        assert!(!s.due(1100));
        assert!(s.due(1250));
        assert!(s.due(10), "a clock that went back refreshes");
        s.refresh_ms = 0;
        assert!(s.due(1000));
    }
}
