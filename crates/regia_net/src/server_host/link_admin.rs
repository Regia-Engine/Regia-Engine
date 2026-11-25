// SPDX-License-Identifier: MPL-2.0
//! Kick and ban for peers on the game's own port (ADR-2665 (4)).
//!
//! A replication game under the host ([`HostedLink`], ADR-2440) owns the game
//! port: its players are peers of the app's [`NetLink`], not of the host's local
//! [`NetSession`]. These functions give the console `kick` / `ban` / `unban` and the agent's
//! `server.kick` / `server.ban` / `server.unban` the same behavior for those peers:
//!
//! - **Kick** removes the link peer with `kicked`.
//! - **Ban** adds an `Address` entry for the peer's IP block to the **session's ban list** (an
//!   IPv6 address is written with an explicit `/64`, so it persists in `bans.ron` as the block
//!   ban it is), makes the link refuse handshakes from that block for the entry's duration, and
//!   removes the peer with `banned`.
//! - **Unban** removes the entry and lifts every link refusal it caused.
//! - [`enforce_link_bans`] runs on every host tick: a link peer whose address matches an active
//!   `Address` entry (an exact IP or a CIDR range, also entries added by editing `bans.ron` and
//!   `reload`) is removed with `banned`, and its IP is refused for that entry's duration: a
//!   `/128` entry (ADR-4936) refuses exactly that address, any other entry refuses the address's
//!   block (its IPv4 address or IPv6 /64).
//!   A ban whose save fails reports `ban_store_save_failed`: the entry is memory-only for the run,
//!   but the peer still left.
//!
//! **Clocks (ADR-5973).** The ban list's `until` is Unix seconds on the session clock
//! ([`NetSession::now_unix_s`]: the wall second when the session was made plus its monotonic
//! clock), the clock the session's own join check reads; these functions stamp and judge entries
//! by it, never by the system clock. The link runs its own clock (the system's), so a link
//! refusal carries the entry's remaining time on the session clock, not its absolute stamp; the
//! list stays the authority, and an entry that ends on the session clock lifts its refusal at the
//! next [`enforce_link_bans`] whatever the link's clock reads.
//!
//! Player ids are the link's peer ids (`7`, or `7.0` as `server.status` writes them); the local
//! part is ignored, since a link peer is one player. On a shared port (`--shared-port`) the link
//! and the session number their players on their own, so one id can name a player on each:
//! [`player_layer`] refuses that id (`player_ambiguous`) until it says which, `link:7` or
//! `session:7.0`. A kick or a ban of the wrong player is not undone by a retry.

use std::net::{IpAddr, SocketAddr};

use regia_ecs::prelude::*;

use super::ecs_host::{op_err, resolve_target};
use super::{HostOpError, HostedLink, parse_player_id, unban_entry};
use crate::model::PeerId;
use crate::session::NetSession;
use crate::session::bans::{BanId, BanKind, BanSubject};
use crate::{NetLink, session::unix_now_s};

/// The link refusals each ban entry caused, so an unban lifts exactly those.
#[derive(Resource, Debug, Default, Clone)]
pub struct LinkBanMirror {
    /// `(refused ip, the entry that caused it)`.
    pub refused: Vec<(IpAddr, BanId)>,
    /// What the last full check of [`enforce_link_bans`] saw.
    checked: Option<EnforceKey>,
}

/// Nothing a ban check reads has changed while this stays equal (an expiry or an edit of the list
/// shows within the second).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnforceKey {
    now_s: u64,
    entries: usize,
    peers: Vec<(PeerId, SocketAddr)>,
}

/// The layer of a shared port a player id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerLayer {
    /// A peer of the game's link.
    Link,
    /// A player of the host's session.
    Session,
}

/// Which layer `player` names, and the id without its `link:` / `session:` prefix. A bare id on
/// the link only is the link's; any other bare id is the session's, whose lookup reports an
/// unknown one.
///
/// # Errors
///
/// `player_ambiguous`: a bare id that names a player on both layers.
pub fn player_layer<'a>(
    player: &'a str,
    link_peers: &[PeerId],
    session: Option<&NetSession>,
) -> Result<(PlayerLayer, &'a str), HostOpError> {
    if let Some(id) = player.strip_prefix("link:") {
        return Ok((PlayerLayer::Link, id));
    }
    if let Some(id) = player.strip_prefix("session:") {
        return Ok((PlayerLayer::Session, id));
    }
    let on_link = parse_player_id(player).is_some_and(|(peer, _)| link_peers.contains(&peer));
    if !on_link {
        return Ok((PlayerLayer::Session, player));
    }
    if session.is_some_and(|s| resolve_target(s, player).is_ok()) {
        return Err(op_err(
            "player_ambiguous",
            format!(
                "`{player}` is a player on both layers of this port: say link:{player} or session:{player}"
            ),
        ));
    }
    Ok((PlayerLayer::Link, player))
}

/// [`player_layer`] for a world that hosts the game port.
///
/// # Errors
///
/// As [`player_layer`].
pub fn player_layer_in_world<'a>(
    world: &World,
    player: &'a str,
) -> Result<(PlayerLayer, &'a str), HostOpError> {
    let peers: Vec<PeerId> = world
        .get_resource::<NetLink>()
        .map(NetLink::peer_addrs)
        .unwrap_or_default()
        .into_iter()
        .map(|(peer, _)| peer)
        .collect();
    player_layer(player, &peers, world.get_resource::<NetSession>())
}

/// True when the world hosts the game port with its own link: a [`HostedLink`] and a
/// [`NetLink`] running as a server.
pub fn link_hosts_game_port(world: &World) -> bool {
    world.contains_resource::<HostedLink>()
        && world
            .get_resource::<NetLink>()
            .is_some_and(|l| l.role() == "server")
}

fn link_peer(world: &World, player: &str) -> Result<(PeerId, SocketAddr), HostOpError> {
    let (peer, _) = parse_player_id(player).ok_or_else(|| {
        op_err(
            "player_unknown",
            format!("bad player id `{player}` (peer or peer.local)"),
        )
    })?;
    world
        .get_resource::<NetLink>()
        .map(NetLink::peer_addrs)
        .unwrap_or_default()
        .into_iter()
        .find(|(p, _)| *p == peer)
        .ok_or_else(|| op_err("player_unknown", player.to_owned()))
}

/// Kicks a peer of the game port (`kicked`).
///
/// # Errors
///
/// `player_unknown`: no link peer has that id.
pub fn kick_link_peer(world: &mut World, player: &str, reason: &str) -> Result<(), HostOpError> {
    let (peer, _) = link_peer(world, player)?;
    if !world.resource_mut::<NetLink>().kick(peer) {
        return Err(op_err("player_unknown", player.to_owned()));
    }
    tracing::info!(%player, %reason, "server host: kick (game port)");
    Ok(())
}

/// True when an `Address` entry names exactly one address — an explicit `/128` (ADR-4936) — and
/// not its block. Every other entry (a bare or ranged IPv4, a bare IPv6, a shorter prefix) is a
/// block ban on the transport.
fn entry_is_exact(entry: &crate::session::bans::BanListEntry) -> bool {
    entry.kind == BanKind::Address && entry.value.ends_with("/128")
}

/// The session clock the ban list's `until` is stamped on (ADR-5973); `None` without a session.
fn session_now_s(world: &World) -> Option<u64> {
    world.get_resource::<NetSession>().map(NetSession::now_unix_s)
}

/// A ban entry's expiry (`until`, on the session clock read as `now_s`) as a refusal expiry on the
/// link's clock, the system clock: the entry's remaining time, at least one second. 0 = permanent
/// (ADR-5973).
fn link_expiry(until: Option<u64>, now_s: u64) -> u64 {
    until.map_or(0, |u| unix_now_s().saturating_add(u.saturating_sub(now_s).max(1)))
}

/// Refuses `ip` on the link for the entry `id` until `until` (session clock, read as `now_s`).
fn refuse(world: &mut World, ip: IpAddr, entry: (BanId, &str, Option<u64>), exact: bool, now_s: u64) {
    let (id, reason, until) = entry;
    let ip = ip.to_canonical();
    let expires = link_expiry(until, now_s);
    let mut link = world.resource_mut::<NetLink>();
    if exact {
        link.refuse_ip_exact(ip, reason, expires);
    } else {
        link.refuse_ip(ip, reason, expires);
    }
    let mut mirror = world.get_resource_or_insert_with(LinkBanMirror::default);
    if !mirror.refused.contains(&(ip, id)) {
        mirror.refused.push((ip, id));
    }
}

/// Bans a peer of the game port by address for `duration_s` (permanent when `None`): the entry
/// goes to the session's ban list (saved with it), the link refuses the address, and the peer is
/// removed with `banned`. On a shared port the session's peers on that address leave too: its
/// list is only read at a join.
///
/// # Errors
///
/// `player_unknown`, `net_not_server` (no hosted session to keep the list), or the ban list's
/// own code.
pub fn ban_link_peer(
    world: &mut World,
    player: &str,
    duration_s: Option<u64>,
    reason: &str,
) -> Result<BanId, HostOpError> {
    let (peer, addr) = link_peer(world, player)?;
    let ip = addr.ip().to_canonical();
    // The list's `until` is on the session clock, the clock its join check reads (ADR-5973).
    let now = session_now_s(world).ok_or_else(|| op_err("net_not_server", "the app has no NetSession"))?;
    let until = duration_s.map(|d| now.saturating_add(d));
    // A block ban is intended (the peer stays out even if it rotates addresses inside its /64), so
    // an IPv6 address is written with an explicit `/64` rather than left bare (ADR-4936).
    let value = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("{v6}/64"),
    };
    let (id, save_err): (BanId, Option<String>) = {
        let mut session = world
            .get_resource_mut::<NetSession>()
            .ok_or_else(|| op_err("net_not_server", "the app has no NetSession"))?;
        if session.server().is_none() {
            return Err(op_err("net_not_server", "this process is not hosting"));
        }
        let on_ip: Vec<PeerId> = session.server().map_or_else(Vec::new, |server| {
            server
                .peer_ids()
                .into_iter()
                .filter(|p| {
                    server
                        .peer_addr(*p)
                        .is_some_and(|a| crate::addr_block::same_block(a.ip(), ip))
                })
                .collect()
        });
        match session.ban(BanKind::Address, &value, until, reason) {
            Ok(id) => {
                for session_peer in &on_ip {
                    let _ = session.kick_peer(*session_peer, &format!("banned: {reason}"));
                }
                (id, None)
            }
            Err(crate::session::SessionError::BanStoreSaveFailed(e)) => {
                // The entry is in memory for this run; the peers on that address leave anyway,
                // and the operator gets the save error (see `ban_player`). Mirror the refusal
                // under the entry's real id so a later unban still lifts it.
                let real_id = session
                    .server()
                    .and_then(|s| {
                        s.bans().list().find(
                            &BanSubject {
                                platform_id: None,
                                addr: Some(ip),
                                names: &[],
                            },
                            now,
                        )
                    })
                    .map_or(BanId(0), |entry| entry.id);
                for session_peer in &on_ip {
                    let _ = session.kick_peer(*session_peer, &format!("banned: {reason}"));
                }
                (real_id, Some(e))
            }
            Err(e) => return Err(op_err(e.code(), e.to_string())),
        }
    };
    refuse(world, ip, (id, reason, until), false, now);
    // `refuse_ip` already removed the peers on that address; this covers a peer it missed.
    let _ = world.resource_mut::<NetLink>().remove_banned(peer);
    if let Some(e) = save_err {
        return Err(op_err("ban_store_save_failed", e));
    }
    tracing::info!(%player, %ip, ?until, "server host: ban (game port)");
    Ok(id)
}

/// Removes a ban entry from the session's list and lifts the link refusals it caused.
///
/// # Errors
///
/// `ban_entry_unknown`, `net_not_server`.
pub fn unban_link_entry(world: &mut World, id: u64) -> Result<(), HostOpError> {
    {
        let mut session = world
            .get_resource_mut::<NetSession>()
            .ok_or_else(|| op_err("net_not_server", "the app has no NetSession"))?;
        unban_entry(&mut session, id)?;
    }
    reconcile_link_refusals(world);
    Ok(())
}

/// Decides every refusal whose entry is gone or expired again from the live list, whatever removed
/// the entry (an unban, an edit of `bans.ron` and `reload`, time): a refusal another active entry
/// still covers moves to that entry, with its reason and duration; any other is lifted. The list
/// decides, so a stale row cannot keep an address refused when nothing bans it.
fn reconcile_link_refusals(world: &mut World) {
    let Some(rows) = world
        .get_resource::<LinkBanMirror>()
        .filter(|m| !m.refused.is_empty())
        .map(|m| m.refused.clone())
    else {
        return;
    };
    let Some(session) = world.get_resource::<NetSession>() else {
        return;
    };
    let now = session.now_unix_s();
    let Some(server) = session.server() else {
        return;
    };
    let list = server.bans().list();
    let active = |id: BanId| list.entries.iter().any(|e| e.id == id && e.active(now));
    if rows.iter().all(|(_, id)| active(*id)) {
        return;
    }
    let mut kept: Vec<(IpAddr, BanId)> = Vec::with_capacity(rows.len());
    let mut moved: Vec<(IpAddr, String, Option<u64>, bool)> = Vec::new();
    let mut lifted: Vec<IpAddr> = Vec::new();
    for (ip, id) in rows {
        let row = if active(id) {
            (ip, id)
        } else {
            let who = BanSubject {
                platform_id: None,
                addr: Some(ip),
                names: &[],
            };
            let Some(entry) = list.find(&who, now) else {
                lifted.push(ip);
                continue;
            };
            moved.push((ip, entry.reason.clone(), entry.until, entry_is_exact(entry)));
            (ip, entry.id)
        };
        if !kept.contains(&row) {
            kept.push(row);
        }
    }
    // A refusal that moves to an exact (`/128`) entry may have been a block refusal: the link
    // keeps a block refusal next to an exact one, so the move lifts the address's refusals
    // first. Lifting works per block, so every kept row in a touched block is applied again
    // from its live entry (corr-low, ADR-5789).
    let mut touched: Vec<IpAddr> = lifted;
    touched.extend(moved.iter().filter(|(_, _, _, exact)| *exact).map(|(ip, ..)| *ip));
    let reapply: Vec<(IpAddr, String, Option<u64>, bool)> = kept
        .iter()
        .filter(|(ip, _)| touched.iter().any(|t| crate::addr_block::same_block(*t, *ip)))
        .filter_map(|(ip, id)| {
            list.entries
                .iter()
                .find(|e| e.id == *id)
                .map(|e| (*ip, e.reason.clone(), e.until, entry_is_exact(e)))
        })
        .collect();
    world.resource_mut::<LinkBanMirror>().refused = kept;
    let Some(mut link) = world.get_resource_mut::<NetLink>() else {
        return;
    };
    for ip in &touched {
        link.allow_ip(*ip);
    }
    for (ip, reason, until, exact) in moved.into_iter().chain(reapply) {
        let expires = link_expiry(until, now);
        if exact {
            link.refuse_ip_exact(ip, &reason, expires);
        } else {
            link.refuse_ip(ip, &reason, expires);
        }
    }
}

/// Removes every game-port peer whose address an active `Address` entry of the session's ban
/// list matches (exact IP or CIDR range), with `banned`, and refuses its IP for the entry's
/// duration, and lifts the refusals nothing bans any more. Returns how many peers it removed. Runs
/// on each host tick while the game owns the port; a world without a hosting link or session does
/// nothing. The check itself runs when the peers or the list's length changed, and at most once a
/// second otherwise, since its cost grows with peers times entries.
pub fn enforce_link_bans(world: &mut World) -> usize {
    if !link_hosts_game_port(world) {
        return 0;
    }
    // Entries are judged on the session clock they are stamped on (ADR-5973).
    let Some((entries, now)) = world.get_resource::<NetSession>().and_then(|s| {
        s.server()
            .map(|server| (server.bans().list().entries.len(), s.now_unix_s()))
    }) else {
        return 0;
    };
    let peers = world.resource::<NetLink>().peer_addrs();
    {
        let mut mirror = world.get_resource_or_insert_with(LinkBanMirror::default);
        let unchanged = mirror
            .checked
            .as_ref()
            .is_some_and(|k| k.now_s == now && k.entries == entries && k.peers == peers);
        if unchanged {
            return 0;
        }
        mirror.checked = Some(EnforceKey {
            now_s: now,
            entries,
            peers: peers.clone(),
        });
    }
    let _span = tracing::trace_span!("net_enforce_link_bans").entered();
    reconcile_link_refusals(world);
    if peers.is_empty() {
        return 0;
    }
    let hits: Vec<(PeerId, IpAddr, BanId, String, Option<u64>, bool)> = {
        let Some(server) = world.get_resource::<NetSession>().and_then(|s| s.server()) else {
            return 0;
        };
        let list = server.bans().list();
        if list.entries.is_empty() {
            return 0;
        }
        peers
            .iter()
            .filter_map(|(peer, addr)| {
                let who = BanSubject {
                    platform_id: None,
                    addr: Some(addr.ip()),
                    names: &[],
                };
                list.find(&who, now).map(|e| {
                    let exact = entry_is_exact(e);
                    (*peer, addr.ip(), e.id, e.reason.clone(), e.until, exact)
                })
            })
            .collect()
    };
    let removed = hits.len();
    for (peer, ip, id, reason, until, exact) in hits {
        refuse(world, ip, (id, &reason, until), exact, now);
        let _ = world.resource_mut::<NetLink>().remove_banned(peer);
        tracing::info!(peer = peer.0, %ip, entry = id.0, exact, "server host: banned peer removed (game port)");
    }
    removed
}
