// SPDX-License-Identifier: MPL-2.0
//! Per-shooter rewind windows and view-tick checks (spec 120 § 9.6, ADR-8315).
//!
//! A shot is judged at the view tick its command carried. The server does not take that tick on
//! trust: when it consumes a command it works out how old the shooter's view can honestly be,
//!
//! ```text
//! expected age = RTT + input lead + interpolation delay + 1 tick
//! window       = min(expected age + allowance, max_rewind_ms)
//! allowance    = VIEW_SLACK_MS + VIEW_JITTER_FACTOR × RTT variation
//! ```
//!
//! from numbers it measured itself: the transport's smoothed round trip and its variation for the
//! shooter's peer, the averaged arrival margin of the shooter's commands (the lead), and an
//! interpolation delay no larger than the shooter reported nor than the server's own model of the
//! shooter's interpolation clock allows. The carried view tick must lie in
//! `[now − window, now]` and may not run backward by more than the allowance from the shooter's
//! previous view; a violation is clamped to the nearest allowed tick and counted per shooter
//! (`lagcomp.stats` `windows`). The checked tick is what [`NetViewTick`](crate::NetViewTick)
//! holds, so every rewound query and hit claim uses it.
//!
//! A peer whose round trip the server has not measured (no transport report and no
//! [`set_shooter_rtt`]) gets the project maximum as its window; the backward and future checks
//! still apply.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use serde::Serialize;

use crate::model::{NetMode, NetPlayer, NetPlayerId, PeerId};
use crate::predict::ServerPrediction;
use crate::runtime::NetNode;
use crate::settings::NetSettings;
use crate::status::NetAgentState;

/// The largest `lag_compensation.max_rewind_ms` a project may set (spec 120 § 9.6): a larger
/// value acts as this one.
pub const MAX_REWIND_CEILING_MS: f64 = 1_000.0;

/// The fixed part of a shooter's view allowance, milliseconds: how far a carried view tick may sit
/// past the shooter's expected age, or behind its previous view, before it is clamped.
pub const VIEW_SLACK_MS: f64 = 50.0;

/// The allowance grows by this many times the shooter's measured round-trip variation.
pub const VIEW_JITTER_FACTOR: f64 = 2.0;

/// The server's model of a client's interpolation target takes this many times the measured
/// round-trip variation as the client's arrival jitter (a client targets twice its jitter).
pub const INTERP_JITTER_FACTOR: f64 = 4.0;

/// One shooter's measured window and the checks of its carried view ticks (`lagcomp.stats`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ShooterWindow {
    /// The shooter's peer.
    pub peer: u32,
    /// Its local-player slot.
    pub local: u8,
    /// The server has a round-trip measurement for the peer.
    pub measured: bool,
    /// The measured smoothed round trip, milliseconds (0 when not measured).
    pub rtt_ms: f64,
    /// The measured round-trip variation, milliseconds.
    pub jitter_ms: f64,
    /// The input lead: the averaged arrival margin of its commands, milliseconds.
    pub lead_ms: f64,
    /// The interpolation delay counted: the reported one, at most `interp_bound_ms`.
    pub interp_ms: f64,
    /// The most the server believes the shooter's interpolation delay can be, milliseconds.
    pub interp_bound_ms: f64,
    /// `rtt_ms + lead_ms + interp_ms + 1 tick`.
    pub expected_age_ms: f64,
    /// The allowance past the expected age and behind the previous view, milliseconds.
    pub allowance_ms: f64,
    /// The rewind window: `expected_age_ms + allowance_ms`, at most the project maximum (the
    /// project maximum when not measured), milliseconds.
    pub window_ms: f64,
    /// The newest checked view tick.
    pub last_view_tick: f64,
    /// The server tick it was checked at.
    pub last_server_tick: u32,
    /// View ticks taken as carried.
    pub accepted: u64,
    /// View ticks older than the window, clamped to its edge.
    pub clamped_old: u64,
    /// View ticks newer than the present, clamped to it.
    pub clamped_future: u64,
    /// View ticks that ran backward past the allowance, clamped to it.
    pub clamped_backward: u64,
}

impl ShooterWindow {
    /// Every clamped view tick.
    pub fn clamped(&self) -> u64 {
        self.clamped_old + self.clamped_future + self.clamped_backward
    }
}

/// Server: every shooter's [`ShooterWindow`], and round trips a host measured outside the
/// transport reports ([`set_shooter_rtt`]). Kept by [`crate::interp::set_view_tick`].
#[derive(Resource, Debug, Default, Clone)]
pub struct LagCompWindows {
    shooters: BTreeMap<NetPlayerId, ShooterWindow>,
    rtt: BTreeMap<PeerId, (f64, f64)>,
}

impl LagCompWindows {
    /// The window of `shooter`, once one of its commands carried a view tick.
    pub fn get(&self, shooter: NetPlayerId) -> Option<&ShooterWindow> {
        self.shooters.get(&shooter)
    }

    /// Every shooter's window, by player id.
    pub fn all(&self) -> Vec<ShooterWindow> {
        self.shooters.values().copied().collect()
    }

    /// Forgets a peer (it left).
    pub fn remove_peer(&mut self, peer: PeerId) {
        self.shooters.retain(|p, _| p.peer != peer);
        self.rtt.remove(&peer);
    }
}

/// Server: a peer's round trip and its variation, milliseconds, measured by a host whose
/// transport does not publish endpoint reports (a non-positive or non-finite `rtt_ms` clears it).
/// Takes precedence over the transport reports.
pub fn set_shooter_rtt(world: &mut World, peer: PeerId, rtt_ms: f64, jitter_ms: f64) {
    let mut w = world.get_resource_or_insert_with(LagCompWindows::default);
    if rtt_ms.is_finite() && rtt_ms > 0.0 {
        let jitter = if jitter_ms.is_finite() {
            jitter_ms.max(0.0)
        } else {
            0.0
        };
        w.rtt.insert(peer, (rtt_ms, jitter));
    } else {
        w.rtt.remove(&peer);
    }
}

/// The measured `(rtt_ms, jitter_ms)` of `peer`: [`set_shooter_rtt`], else the first endpoint
/// report that lists the peer with a sampled round trip.
fn measured_rtt(world: &World, peer: PeerId) -> Option<(f64, f64)> {
    if let Some(r) = world
        .get_resource::<LagCompWindows>()
        .and_then(|w| w.rtt.get(&peer))
    {
        return Some(*r);
    }
    world.get_resource::<NetAgentState>().and_then(|s| {
        s.reports().values().find_map(|r| {
            r.peers
                .iter()
                .find(|p| p.id == peer.0 && p.rtt_ms.is_finite() && p.rtt_ms > 0.0)
                .map(|p| {
                    (
                        p.rtt_ms,
                        if p.jitter_ms.is_finite() {
                            p.jitter_ms.max(0.0)
                        } else {
                            0.0
                        },
                    )
                })
        })
    })
}

/// The project maximum in milliseconds: `max_rewind_ms`, in `[0, MAX_REWIND_CEILING_MS]`.
pub(crate) fn cap_ms(max_rewind_ms: f64) -> f64 {
    if max_rewind_ms.is_finite() {
        max_rewind_ms.clamp(0.0, MAX_REWIND_CEILING_MS)
    } else {
        0.0
    }
}

/// Server, when a player's command for the current tick is consumed: checks the carried view tick
/// `raw` (module docs) and returns the tick to keep. Returns `raw` unchanged off a server, for the
/// server's own players, and for an entity that is not a `NetPlayer`.
pub(crate) fn check_view_tick(world: &mut World, player: Entity, raw: f64, delay_ms: f64) -> f64 {
    let is_server = world.get_resource::<NetMode>().is_some_and(|m| m.is_server());
    let Some(now) = world.get_resource::<NetNode>().map(|n| n.tick.0) else {
        return raw;
    };
    let Some(id) = world.get::<NetPlayer>(player).map(|p| p.id) else {
        return raw;
    };
    if !is_server || id.peer == PeerId::SERVER || !raw.is_finite() {
        return raw;
    }
    let settings = world.get_resource::<NetSettings>().cloned().unwrap_or_default();
    let hz = super::tick_hz(world);
    let step_ms = 1000.0 / hz;
    let cap = cap_ms(settings.lag_compensation.max_rewind_ms);
    let measured = measured_rtt(world, id.peer);
    let (rtt_ms, jitter_ms) = measured.unwrap_or((0.0, 0.0));
    let margin_max = f64::from(
        settings
            .prediction
            .margin_max_ticks
            .max(settings.prediction.margin_target_ticks),
    );
    let lead_ticks = world
        .get_resource::<ServerPrediction>()
        .and_then(|sp| sp.players.get(&id))
        .map_or(0.0, |p| f64::from(p.margin))
        .clamp(0.0, margin_max);
    let lead_ms = lead_ticks * step_ms;
    let interp = &settings.interpolation;
    let (min_delay, max_delay) = (
        interp.min_delay_ms.max(0.0),
        interp.max_delay_ms.max(interp.min_delay_ms).max(0.0),
    );
    let send_ms = f64::from(settings.replication.send_every_ticks.max(1)) * step_ms;
    let target = (send_ms * interp.delay_send_intervals.max(0.0) + INTERP_JITTER_FACTOR * jitter_ms)
        .clamp(min_delay, max_delay);
    let nf = f64::from(now);
    let mut windows = world.get_resource_or_insert_with(LagCompWindows::default);
    let prev = windows.shooters.get(&id).copied();
    // The client's interpolation clock lowers its delay by at most `lower_rate` of real time, so
    // the bound decays no faster than that from where it was; it starts at the most a client may
    // use, which also covers the delay a client starts with before its jitter is known.
    let bound = match prev {
        Some(p) => {
            let elapsed_ms = f64::from(now.saturating_sub(p.last_server_tick)) * step_ms;
            target.max(p.interp_bound_ms - interp.lower_rate.max(0.0) * elapsed_ms)
        }
        None => max_delay,
    };
    let reported = if delay_ms.is_finite() {
        delay_ms.max(0.0)
    } else {
        0.0
    };
    let interp_ms = reported.min(bound);
    let expected = rtt_ms + lead_ms + interp_ms + step_ms;
    let allowance = VIEW_SLACK_MS + VIEW_JITTER_FACTOR * jitter_ms;
    let window = if measured.is_some() {
        (expected + allowance).min(cap)
    } else {
        cap
    };
    let floor = nf - window / step_ms;
    let back_floor = prev.map(|p| p.last_view_tick - allowance / step_ms);
    let mut w = prev.unwrap_or(ShooterWindow {
        peer: id.peer.0,
        local: id.local,
        measured: false,
        rtt_ms: 0.0,
        jitter_ms: 0.0,
        lead_ms: 0.0,
        interp_ms: 0.0,
        interp_bound_ms: max_delay,
        expected_age_ms: 0.0,
        allowance_ms: 0.0,
        window_ms: 0.0,
        last_view_tick: raw,
        last_server_tick: now,
        accepted: 0,
        clamped_old: 0,
        clamped_future: 0,
        clamped_backward: 0,
    });
    let tick = if raw > nf {
        w.clamped_future += 1;
        nf
    } else if raw < floor {
        w.clamped_old += 1;
        floor.max(back_floor.unwrap_or(floor)).min(nf)
    } else if let Some(bf) = back_floor.filter(|bf| raw < *bf) {
        w.clamped_backward += 1;
        bf.min(nf)
    } else {
        w.accepted += 1;
        raw
    };
    w.measured = measured.is_some();
    w.rtt_ms = rtt_ms;
    w.jitter_ms = jitter_ms;
    w.lead_ms = lead_ms;
    w.interp_ms = interp_ms;
    w.interp_bound_ms = bound;
    w.expected_age_ms = expected;
    w.allowance_ms = allowance;
    w.window_ms = window;
    w.last_view_tick = tick;
    w.last_server_tick = now;
    windows.shooters.insert(id, w);
    tick
}
