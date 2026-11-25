// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process: a [`HubHost`], its rate gate, an optional [`crate::relay::RelayServer`],
//! a graceful drain and the health body `lobby.hub_status` already reads (NL-06, ADR-8648;
//! the relay is NL-11, ADR-10397).

use super::config::HubFile;
use super::{HubHost, HubReport};
use crate::crypto::ServerIdentity;
use crate::handshake::DisconnectReason;
use crate::relay::RelayServer;
use crate::socket::DatagramSocket;
use crate::status::NetAgentState;

/// Label [`HubProcess::publish`] uses for `lobby.hub_status`.
pub const HUB_STATUS_LABEL: &str = "hub";

/// How long a drain keeps issued relay allocations, on the process clock (NL-11).
///
/// New allocations stop at [`HubProcess::begin_drain`]. Refresh of an allocation already issued
/// continues. [`HubProcess::finished_draining`] becomes true once those allocations are gone, or
/// once this many milliseconds have passed on the clock of the `update` calls after the drain
/// started. The clock is the `now_ms` argument, not the wall clock.
pub const HUB_RELAY_DRAIN_MS: u64 = 30_000;

/// A running hub. `S` is the datagram socket ([`crate::socket::UdpTransport`] in the process,
/// loopback in tests).
pub struct HubProcess<S: DatagramSocket> {
    host: HubHost<S>,
    relay: Option<RelayServer<S>>,
    draining: bool,
    /// `now_ms` of the first [`Self::update`] after [`Self::begin_drain`].
    drain_at_ms: Option<u64>,
    /// `now_ms` of the latest [`Self::update`].
    last_now_ms: u64,
}

impl<S: DatagramSocket> HubProcess<S> {
    /// A hub on `socket` with `identity` and the caps and rate limits in `file`.
    #[must_use]
    pub fn new(socket: S, identity: ServerIdentity, file: &HubFile) -> Self {
        let mut host = HubHost::new(socket, identity, file.settings());
        host.set_limits(file.limits);
        Self {
            host,
            relay: None,
            draining: false,
            drain_at_ms: None,
            last_now_ms: 0,
        }
    }

    /// Polls `relay` on the same [`Self::update`] as the hub. A second call replaces it.
    ///
    /// Call [`RelayServer::set_source_rate`](crate::relay::RelayServer::set_source_rate) before
    /// this when `hub.toml` sets `relay.per_address_per_sec`.
    pub fn attach_relay(&mut self, relay: RelayServer<S>) {
        self.relay = Some(relay);
    }

    /// The relay, when [`Self::attach_relay`] ran.
    #[must_use]
    pub fn relay(&self) -> Option<&RelayServer<S>> {
        self.relay.as_ref()
    }

    /// The host.
    #[must_use]
    pub fn host(&self) -> &HubHost<S> {
        &self.host
    }

    /// The host, mutable (tests and the drain).
    pub fn host_mut(&mut self) -> &mut HubHost<S> {
        &mut self.host
    }

    /// Receives, applies requests and flushes, then polls the relay on the same `now_ms`.
    ///
    /// The first call after [`Self::begin_drain`] records `now_ms` as the start of the relay
    /// drain window ([`HUB_RELAY_DRAIN_MS`]).
    pub fn update(&mut self, now_ms: u64) {
        if self.draining && self.drain_at_ms.is_none() {
            self.drain_at_ms = Some(now_ms);
        }
        self.last_now_ms = now_ms;
        self.host.update(now_ms);
        if let Some(relay) = &mut self.relay {
            relay.update(now_ms);
        }
    }

    /// Stops taking new sessions and new relay allocations, tells every peer the server is
    /// shutting down, and drops the hub sessions. A second call is a no-op.
    ///
    /// Issued relay allocations stay until they expire or [`HUB_RELAY_DRAIN_MS`] of the process
    /// clock have passed. Refresh of those allocations continues.
    pub fn begin_drain(&mut self) {
        if self.draining {
            return;
        }
        self.draining = true;
        self.host.begin_drain();
        if let Some(relay) = &mut self.relay {
            relay.stop_new_allocations();
        }
    }

    /// Applies one shutdown the way the binary does after SIGTERM or SIGINT.
    ///
    /// The first call starts the drain and returns false. A later call returns true so the
    /// process can exit at once, which is the second signal.
    pub fn note_shutdown(&mut self) -> bool {
        if self.is_draining() {
            return true;
        }
        self.begin_drain();
        false
    }

    /// True after [`Self::begin_drain`].
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining || self.host.is_draining()
    }

    /// True when the hub has dropped every session and the relay has either no allocation left
    /// or has had [`HUB_RELAY_DRAIN_MS`] since the first [`Self::update`] of the drain.
    ///
    /// With no relay attached this is true as soon as the hub sessions are gone, including
    /// before that update. With a relay and a live allocation it stays false until an update
    /// has recorded the drain clock and the window has elapsed.
    ///
    /// A link that connected during the drain and was refused holds no session. It is waited
    /// for (it may still leave on its own), but never past the same window: the window is the
    /// upper bound of the drain once the sessions are gone.
    #[must_use]
    pub fn finished_draining(&self) -> bool {
        if !self.is_draining() || !self.host.sessions_gone() {
            return false;
        }
        let window_passed = match self.drain_at_ms {
            Some(start) => self.last_now_ms.saturating_sub(start) >= HUB_RELAY_DRAIN_MS,
            None => false,
        };
        if !self.host.finished_draining() && !window_passed {
            return false;
        }
        let Some(relay) = &self.relay else {
            return true;
        };
        relay.core().allocation_count() == 0 || window_passed
    }

    /// The agent report. `relay_allocations` counts the attached relay. `draining` is this process.
    #[must_use]
    pub fn report(&self) -> HubReport {
        let mut report = self.host.report();
        report.relay_allocations = self
            .relay
            .as_ref()
            .map(|relay| relay.core().allocation_count())
            .unwrap_or(0);
        report.draining = self.is_draining();
        report
    }

    /// Publishes the report for `lobby.hub_status` under [`HUB_STATUS_LABEL`].
    pub fn publish(&self, state: &mut NetAgentState) {
        state.publish_hub(HUB_STATUS_LABEL, self.report());
    }

    /// The JSON body of `GET /health`. Numbers only, so a handle cannot break the object.
    ///
    /// `relay_allocations`, `servers_online` and `draining` are the same numbers
    /// [`Self::report`] publishes for `lobby.hub_status`.
    #[must_use]
    pub fn health_json(&self) -> String {
        let report = self.report();
        let refused: u64 = report.refused.values().sum();
        format!(
            "{{\"sessions\":{},\"lobbies\":{},\"listed\":{},\"state_bytes\":{},\"draining\":{},\"refused\":{},\"relay_allocations\":{},\"servers_online\":{}}}",
            report.sessions,
            report.lobbies,
            report.listed,
            report.state_bytes,
            report.draining,
            refused,
            report.relay_allocations,
            report.servers_online,
        )
    }
}

impl<S: DatagramSocket> HubProcess<S> {
    /// The disconnect reason a drain sends.
    #[must_use]
    pub fn drain_reason() -> DisconnectReason {
        DisconnectReason::ServerShutdown
    }
}

#[cfg(feature = "ecs")]
impl<S: DatagramSocket> HubProcess<S> {
    /// The snapshot the dedicated-server health endpoint serves (`200`, or `503` while draining).
    #[must_use]
    pub fn health_snapshot(&self) -> crate::server_host::health::HealthSnapshot {
        crate::server_host::health::HealthSnapshot {
            json: self.health_json(),
            draining: self.is_draining(),
        }
    }
}
