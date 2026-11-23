// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process: a [`HubHost`], its rate gate, a graceful drain and the health body
//! `lobby.hub_status` already reads (NL-06, ADR-8648).

use super::config::HubFile;
use super::{HubHost, HubReport};
use crate::crypto::ServerIdentity;
use crate::handshake::DisconnectReason;
use crate::socket::DatagramSocket;
use crate::status::NetAgentState;

/// Label [`HubProcess::publish`] uses for `lobby.hub_status`.
pub const HUB_STATUS_LABEL: &str = "hub";

/// A running hub. `S` is the datagram socket ([`crate::socket::UdpTransport`] in the process,
/// loopback in tests).
pub struct HubProcess<S: DatagramSocket> {
    host: HubHost<S>,
    draining: bool,
}

impl<S: DatagramSocket> HubProcess<S> {
    /// A hub on `socket` with `identity` and the caps and rate limits in `file`.
    #[must_use]
    pub fn new(socket: S, identity: ServerIdentity, file: &HubFile) -> Self {
        let mut host = HubHost::new(socket, identity, file.settings());
        host.set_limits(file.limits);
        Self {
            host,
            draining: false,
        }
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

    /// Receives, applies requests and flushes. `now_ms` is the process clock.
    pub fn update(&mut self, now_ms: u64) {
        self.host.update(now_ms);
    }

    /// Stops taking new sessions, tells every peer the server is shutting down, and drops the
    /// hub sessions. A second call is a no-op. [`Self::finished_draining`] is true once no peer
    /// remains.
    pub fn begin_drain(&mut self) {
        self.draining = true;
        self.host.begin_drain();
    }

    /// True after [`Self::begin_drain`].
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining || self.host.is_draining()
    }

    /// True when the drain has dropped every session and every unconfirmed peer.
    #[must_use]
    pub fn finished_draining(&self) -> bool {
        self.is_draining() && self.host.finished_draining()
    }

    /// The agent report.
    #[must_use]
    pub fn report(&self) -> HubReport {
        self.host.report()
    }

    /// Publishes the report for `lobby.hub_status` under [`HUB_STATUS_LABEL`].
    pub fn publish(&self, state: &mut NetAgentState) {
        self.host.publish(HUB_STATUS_LABEL, state);
    }

    /// The JSON body of `GET /health`. Numbers only, so a handle cannot break the object.
    #[must_use]
    pub fn health_json(&self) -> String {
        let report = self.host.report();
        let refused: u64 = report.refused.values().sum();
        format!(
            "{{\"sessions\":{},\"lobbies\":{},\"listed\":{},\"state_bytes\":{},\"draining\":{},\"refused\":{}}}",
            report.sessions,
            report.lobbies,
            report.listed,
            report.state_bytes,
            self.is_draining(),
            refused,
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
