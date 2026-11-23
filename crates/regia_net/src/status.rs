// SPDX-License-Identifier: MPL-2.0
//! Status, diagnostics overlay data and the agent bridge resource.
//!
//! Servers and clients describe themselves as a [`NetStatus`] plus one [`PeerRow`] per peer. A game
//! that runs `regia_net` inside an app publishes those into the [`NetAgentState`] resource each
//! frame (`NetServer::publish` / `NetClient::publish`); the agent methods `net.status`,
//! `net.peers`, `net.conditioner_set` and `net.rollback` read and write that resource, so the agent
//! server never needs the game's payload types.

use std::collections::BTreeMap;

#[cfg(feature = "ecs")]
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::conditioner::{ConditionerConfig, ConditionerStats};
use crate::connection::LinkStats;
use crate::crypto::{CryptoError, SealedKind};
use crate::rollback::RollbackCounters;

/// Endpoint-level counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointTotals {
    /// Datagrams received.
    pub datagrams_in: u64,
    /// Datagrams handed to the socket (after the conditioner).
    pub datagrams_out: u64,
    /// Datagrams that were not `regia_net` traffic.
    pub foreign: u64,
    /// Datagrams or messages that failed to decode.
    pub malformed: u64,
    /// Joins accepted.
    pub accepted: u64,
    /// Joins denied.
    pub denied: u64,
    /// Socket errors on send or receive.
    pub socket_errors: u64,
    /// Datagrams that never left: the socket refused them (its buffer was full, `WouldBlock`, or
    /// the send failed). They are not in `datagrams_out` (spec 120 § 4.3).
    #[serde(default)]
    pub socket_drops: u64,
    /// Reliable sends refused by the per-peer rate cap.
    pub rate_limited: u64,
    /// Inbound messages dropped by the per-peer cap of one receive.
    pub inbound_dropped: u64,
    /// Sealed datagrams sent.
    pub sealed_out: u64,
    /// Sealed datagrams that authenticated.
    pub opened_in: u64,
    /// Datagrams refused by the authentication tag, a wrong token, or as plain traffic on a sealed
    /// link.
    pub auth_failed: u64,
    /// Sealed datagrams refused by the replay window.
    pub replayed: u64,
    /// Key exchanges finished.
    pub handshakes: u64,
    /// Key exchanges refused for load (the table, the per-IP share, the per-second cap, a second
    /// share on one cookie), joins refused because a live sealed peer holds the address, and copies
    /// of a join answer that already made a peer from another port of its IP.
    pub handshakes_refused: u64,
    /// Server: pre-proof requests and unusable-cookie refreshes dropped without an answer by
    /// the answer shed ([`crate::server::ServerConfig::pre_proof_answers_per_s`], ADR-4936): over
    /// their source prefix's fair share or the server-wide cap (ADR-8718).
    #[serde(default)]
    pub pre_proof_shed: u64,
    /// Server: peers that moved to a new address (NAT rebinding, ADR-2416), each after the new
    /// path answered a probe (ADR-8299). Client: moves to a new server address.
    #[serde(default)]
    pub rebinds: u64,
    /// Server: records from an unknown address that carried a peer's token but were not let in:
    /// over that address's trial-open budget, from a banned address, older than the peer's newest
    /// record, or failing the key. Client: the same for records from a new server address.
    #[serde(default)]
    pub rebinds_refused: u64,
    /// Server and client: path probes sent to candidate addresses (ADR-8299). A probe is one
    /// acknowledgment-only packet sent only to the new address; the move waits for its
    /// acknowledgment from there.
    #[serde(default)]
    pub path_probes: u64,
    /// Server and client: data packets also sent to a candidate address while the validated path
    /// was silent (ADR-8422): a NAT rebinding's traffic reaches the far end while its new address
    /// validates, within three times the bytes the candidate sent.
    #[serde(default)]
    pub path_mirrored: u64,
    /// Client: input bundles that carried less of the remembered history than usual because the
    /// inputs did not all fit one datagram (ADR-7896). Each still carried the newest input.
    #[serde(default)]
    pub input_bundles_trimmed: u64,
    /// Inputs on the reliable input lane (too large for one datagram, ADR-7896): sent by a
    /// client, accepted by a server.
    #[serde(default)]
    pub reliable_inputs: u64,
    /// Server: input samples rejected for being at or beyond the future input horizon.
    #[serde(default)]
    pub inputs_too_far_ahead: u64,
    /// Server: players that timed out and went dormant, held for a resume (spec 120 § 10.3,
    /// ADR-8298).
    #[serde(default)]
    pub dormant: u64,
    /// Server: joins that came back as their old peer with a resume ticket, from dormancy or by
    /// taking over a connection the server still held.
    #[serde(default)]
    pub resumes: u64,
    /// Server: resumes that replaced a connection the server still held (live-token takeover).
    #[serde(default)]
    pub resume_takeovers: u64,
    /// Server: joins whose resume ticket matched no held peer (spent, expired or forged); they
    /// joined as new peers.
    #[serde(default)]
    pub resumes_refused: u64,
    /// Server: dormant peers whose grace ended before they came back.
    #[serde(default)]
    pub dormant_expired: u64,
    /// Server: datagrams and unreliable messages dropped by the per-peer inbound rate, a peer's
    /// per-receive slice, or the per-receive work budget for unattributed datagrams (ADR-8181).
    #[serde(default)]
    pub over_rate: u64,
    /// Server: peers removed with `DisconnectReason::Flood` (ADR-8181).
    #[serde(default)]
    pub flood_disconnects: u64,
    /// Server: resumes (counted in `resumes` too) that presented the ticket an earlier resume
    /// replaced before its client confirmed the accept: a retry after a lost accept (ADR-8535).
    #[serde(default)]
    pub resume_retries: u64,
    /// Server: joins whose ticket named the per-address slot held for a player that left on
    /// purpose (ADR-8733): new peers that the per-address cap did not count again.
    #[serde(default)]
    pub held_slot_returns: u64,
    /// Client: resume flights started (a connect that opened with a `ResumeInit`, wire 6,
    /// ADR-8858). Server: `ResumeInit` datagrams read.
    #[serde(default)]
    pub resume_inits: u64,
    /// Client: resume flights that ended in the full handshake (the server answered a challenge,
    /// or the reply's accept did not open). Server: `ResumeInit` datagrams refused to the full
    /// handshake (`NetServer::last_resume_refusal` names why).
    #[serde(default)]
    pub resume_fallbacks: u64,
    /// Server: resumes admitted in one round trip: a `ResumeInit` answered with a `ResumeReply`
    /// whose client then proved it holds the new keys (wire 6, ADR-8902); each is counted in
    /// `resumes` or `held_slot_returns` too. Client: connects that finished on a `ResumeReply`.
    #[serde(default)]
    pub resumed_one_rtt: u64,
    /// Server: `ResumeInit` datagrams that were a byte-identical copy of a pending flight and got
    /// its stored `ResumeReply` again, with no key work and no handshake charge (ADR-8858 A4).
    #[serde(default)]
    pub resume_replays: u64,
    /// Server: answered resume flights that left unconfirmed: no sealed record under their keys
    /// came within the pending lifetime, their ticket expired, or a newer flight of the same
    /// ticket took their place (ADR-8858 A1, A3). Nothing was admitted for them.
    #[serde(default)]
    pub resume_lapsed: u64,
}

impl EndpointTotals {
    /// Counts the result of opening a sealed record and passes on what authenticated.
    pub(crate) fn note_open(
        &mut self,
        opened: Result<(SealedKind, Vec<u8>), CryptoError>,
    ) -> Option<(SealedKind, Vec<u8>)> {
        match opened {
            Ok(record) => {
                self.opened_in += 1;
                Some(record)
            }
            Err(CryptoError::Replay) => {
                self.replayed += 1;
                None
            }
            Err(CryptoError::Malformed) => {
                self.malformed += 1;
                None
            }
            Err(_) => {
                self.auth_failed += 1;
                None
            }
        }
    }
}

/// What `net.status` says about an endpoint's encryption.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CryptoStatus {
    /// `off`, `auto` or `required`.
    pub mode: String,
    /// The Noise protocol name.
    pub suite: String,
    /// Server: its public key. Client: the key the server proved, once it did.
    pub server_key: Option<String>,
    /// Client: a pinned key was checked. Server: always false.
    pub pinned: bool,
    /// Client: the link is sealed. Server: every connected peer's link is sealed.
    pub sealed: bool,
    /// Connected peers on a sealed link (server) or 1/0 (client).
    pub sealed_peers: usize,
    /// Key switches so far on the live sealed links, both directions summed (ADR-2416).
    #[serde(default)]
    pub rekeys: u64,
}

/// One endpoint's state, as `net.status` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetStatus {
    /// Client: server interrupted; server: at least one peer interrupted.
    #[serde(default)]
    pub interrupted: bool,
    /// `"server"` or `"client"`.
    pub role: String,
    /// `listening` or `closed` for a server; `connecting`, `connected` or `disconnected:<reason>` for a client.
    pub state: String,
    /// The bound address.
    pub local_addr: Option<String>,
    /// The server address (clients only).
    pub remote_addr: Option<String>,
    /// This client's peer id once connected (clients only).
    pub peer_id: Option<u32>,
    /// Server: next tick to simulate. Client: estimated server tick.
    pub tick: u64,
    /// Authoritative tick rate.
    pub tick_rate_hz: u32,
    /// Connected peers (server) or 1/0 (client).
    pub peer_count: usize,
    /// Peer capacity (server) or 1 (client).
    pub max_peers: usize,
    /// Active conditioner configuration.
    pub conditioner: ConditionerConfig,
    /// Conditioner decision counters.
    pub conditioner_stats: ConditionerStats,
    /// Endpoint counters.
    pub totals: EndpointTotals,
    /// Link to the server (clients only).
    pub link: Option<LinkStats>,
    /// Encryption state.
    pub crypto: CryptoStatus,
    /// The last `protocol_mismatch` refusal and which side is older (spec 120 § 12.2, ADR-7985):
    /// a client's own refusal, or the last client a server refused. `None` until one happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_mismatch: Option<crate::version_probe::VersionMismatchReport>,
    /// Clients: input lead, clock resyncs and the server's last input feedback (ADR-8077).
    #[serde(default)]
    pub input_timing: Option<crate::feedback::InputTiming>,
    /// Client: reconnecting by itself after a timeout (spec 120 § 10.3, ADR-8352); `None`
    /// otherwise. Absent from the JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<ReconnectStatus>,
    /// Clients: the reliable streams of the link to the server (spec 120 § 5.1, ADR-8302).
    #[serde(default)]
    pub streams: Vec<crate::connection::StreamStatus>,
    /// The UDP socket's buffers as the operating system read them back and its DSCP mark (spec
    /// 120 § 4.4, ADR-8407); `None` for a transport without an OS socket of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<crate::socket::SocketReport>,
    /// Server: `ResumeInit` flights that were not answered with a `ResumeReply`, counted by the
    /// lowercase code of why (`ResumeRefusal::as_str`: `resume_epoch`, `resume_psk`, ...), and
    /// resumes refused at their key confirmation under the same codes (wire 6, ADR-8902). Codes
    /// that never happened are absent; the map is absent from the JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resume_refusals: BTreeMap<String, u64>,
    /// Clients: the connection quality of the link to the server while connected (spec 120
    /// § 13.1, ADR-8182). Servers list it per peer in [`PeerRow::quality`].
    #[serde(default)]
    pub quality: Option<crate::quality::PeerQuality>,
}

/// A client's automatic reconnect after a timeout, as `net.status` reports it (ADR-8352).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectStatus {
    /// Attempts started so far (0 before the first).
    pub attempt: u32,
    /// Milliseconds since the timeout started the reconnect.
    pub since_ms: u64,
    /// Milliseconds until the next attempt is due.
    pub next_in_ms: u64,
    /// Milliseconds left in the reconnect window (the server's resume grace).
    pub window_left_ms: u64,
}

/// One connected peer, as `net.peers` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRow {
    /// Silence passed the interruption interval; the seat remains connected.
    #[serde(default)]
    pub interrupted: bool,
    /// Peer id.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// `player` or `spectator`.
    pub role: String,
    /// Source address.
    pub addr: String,
    /// Smoothed round-trip time, milliseconds.
    pub rtt_ms: f64,
    /// Round-trip variation, milliseconds.
    pub jitter_ms: f64,
    /// Recent loss fraction.
    pub loss_fraction: f32,
    /// Milliseconds since the last packet.
    pub silent_ms: u64,
    /// Loss or latency above the health policy.
    pub flagged: bool,
    /// Ticks simulated without this peer's input.
    pub missed_input_ticks: u64,
    /// Input samples rejected for being at or beyond this peer's future input horizon.
    #[serde(default)]
    pub inputs_too_far_ahead: u64,
    /// The input feedback last sent to this player: arrival margins, late, missing (ADR-8077).
    #[serde(default)]
    pub input_feedback: Option<crate::feedback::LinkFeedback>,
    /// The connection grade, with hysteresis (spec 120 § 13.1, ADR-8182).
    #[serde(default)]
    pub grade: crate::quality::LinkGrade,
    /// The signals behind the grade (ADR-8182).
    #[serde(default)]
    pub quality: Option<crate::quality::PeerQuality>,
    /// Full link counters.
    pub link: LinkStats,
    /// The link is sealed.
    pub sealed: bool,
    /// Validated moves of this peer to a new address (ADR-8299).
    #[serde(default)]
    pub rebinds: u64,
    /// Records from a new address with this peer's token that were not let in (budget, ban,
    /// stale, key; ADR-8299).
    #[serde(default)]
    pub rebinds_refused: u64,
    /// The newest address waiting for path validation, if any.
    #[serde(default)]
    pub path_candidate: Option<String>,
    /// Path probes sent for this peer.
    #[serde(default)]
    pub path_probes: u64,
    /// Data packets also sent to this peer's candidate address while its validated path was
    /// silent (ADR-8422).
    #[serde(default)]
    pub path_mirrored: u64,
    /// Inbound policing state: over-rate drops, flood timer, malformed score (ADR-8181).
    #[serde(default)]
    pub inbound: crate::inbound::PeerInboundStatus,
    /// The reliable streams of this peer's link (spec 120 § 5.1, ADR-8302).
    #[serde(default)]
    pub streams: Vec<crate::connection::StreamStatus>,
}

/// Session-wide summary for a HUD or admin overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsOverlay {
    /// The endpoint status.
    pub status: NetStatus,
    /// Every peer.
    pub peers: Vec<PeerRow>,
    /// Worst round-trip time across peers.
    pub max_rtt_ms: f64,
    /// Mean loss fraction across peers.
    pub avg_loss_fraction: f32,
    /// Peers above the health thresholds.
    pub flagged_peers: usize,
    /// The worst connection grade: across the peers for a server, of the server link for a
    /// client (ADR-8182).
    #[serde(default)]
    pub worst_grade: crate::quality::LinkGrade,
}

impl DiagnosticsOverlay {
    /// Builds the overlay from a status and its peer rows.
    pub fn new(status: NetStatus, peers: Vec<PeerRow>) -> Self {
        let max_rtt_ms = peers.iter().map(|p| p.rtt_ms).fold(0.0, f64::max);
        let avg_loss_fraction = if peers.is_empty() {
            0.0
        } else {
            peers.iter().map(|p| p.loss_fraction).sum::<f32>() / peers.len() as f32
        };
        let flagged_peers = peers.iter().filter(|p| p.flagged).count();
        let worst_grade = peers
            .iter()
            .map(|p| p.grade)
            .chain(status.quality.map(|q| q.grade))
            .max()
            .unwrap_or_default();
        Self {
            status,
            peers,
            max_rtt_ms,
            avg_loss_fraction,
            flagged_peers,
            worst_grade,
        }
    }

    /// One text line per peer plus a header, for a debug overlay or a log.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{} {} tick {} @ {} Hz, {} peer(s), max rtt {:.0} ms, avg loss {:.1} %{}",
            self.status.role,
            if self.status.interrupted {
                format!("{} INTERRUPTED", self.status.state)
            } else {
                self.status.state.clone()
            },
            self.status.tick,
            self.status.tick_rate_hz,
            self.peers.len(),
            self.max_rtt_ms,
            self.avg_loss_fraction * 100.0,
            match self.status.quality {
                Some(q) => format!(
                    ", link {} (rtt p95 {:.0} ms, jitter {:.0} ms, loss out {:.1} % in {:.1} % recv {:.1} %)",
                    q.grade,
                    q.rtt_p95_ms,
                    q.jitter_ms,
                    q.loss_out * 100.0,
                    q.loss_in * 100.0,
                    q.loss_recv * 100.0
                ),
                None => String::new(),
            }
        )];
        for p in &self.peers {
            out.push(format!(
                "  #{} {} {} rtt {:.0}±{:.0} ms loss {:.1} % up {:.1} kbps down {:.1} kbps missed {}{}{}",
                p.id,
                p.name,
                p.grade,
                p.rtt_ms,
                p.jitter_ms,
                p.loss_fraction * 100.0,
                p.link.send_kbps,
                p.link.recv_kbps,
                p.missed_input_ticks,
                if p.flagged { " FLAGGED" } else { "" },
                if p.interrupted { " INTERRUPTED" } else { "" }
            ));
        }
        out
    }
}

/// What one endpoint last published.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointReport {
    /// Status.
    pub status: NetStatus,
    /// Peers.
    pub peers: Vec<PeerRow>,
    /// A server's refusals per peer, by code, peers that left included (KI-NT-025, ADR-10077;
    /// `net.rejections`). Empty on a client.
    #[serde(default)]
    pub rejections: Vec<PeerRejections>,
}

/// Peers whose refusals [`RejectionsByPeer`] keeps. Past this, the peer whose newest refusal is
/// the oldest is forgotten (connected or not), so a stream of new hostile peers cannot grow it.
pub const MAX_REJECTION_PEERS: usize = 256;

/// One peer's refusals by lowercase code (`RejectKind::as_str`), as `net.rejections` reports them
/// (KI-NT-025, ADR-10077).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRejections {
    /// The peer.
    pub peer: u32,
    /// Refusals of this peer, all codes.
    pub total: u64,
    /// Refusals of this peer by code.
    pub kinds: BTreeMap<String, u64>,
}

/// Refusal counts per peer and code, bounded at [`MAX_REJECTION_PEERS`] peers (KI-NT-025,
/// ADR-10077). A peer's row stays after it leaves (a peer removed for its refusals is the one an
/// operator asks about) until the bound evicts it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectionsByPeer {
    /// `peer -> (sequence of its newest refusal, row)`.
    rows: BTreeMap<u32, (u64, PeerRejections)>,
    /// Refusals counted, the sequence of the newest.
    seq: u64,
    /// Peer rows evicted by the bound.
    evicted: u64,
}

impl RejectionsByPeer {
    /// Counts one refusal of `peer` with lowercase code `code`.
    pub fn note(&mut self, peer: u32, code: &str) {
        self.seq += 1;
        if !self.rows.contains_key(&peer)
            && self.rows.len() >= MAX_REJECTION_PEERS
            && let Some(oldest) = self.rows.iter().min_by_key(|(_, (seq, _))| *seq).map(|(p, _)| *p)
        {
            self.rows.remove(&oldest);
            self.evicted += 1;
        }
        let (newest, row) = self.rows.entry(peer).or_insert_with(|| {
            (
                0,
                PeerRejections {
                    peer,
                    ..PeerRejections::default()
                },
            )
        });
        *newest = self.seq;
        row.total += 1;
        *row.kinds.entry(code.to_owned()).or_insert(0) += 1;
    }

    /// The rows, by peer id.
    pub fn rows(&self) -> Vec<PeerRejections> {
        self.rows.values().map(|(_, row)| row.clone()).collect()
    }

    /// One peer's row, if it has one.
    pub fn peer(&self, peer: u32) -> Option<&PeerRejections> {
        self.rows.get(&peer).map(|(_, row)| row)
    }

    /// Peer rows the bound evicted.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

/// The agent bridge: endpoints publish reports here; `net.conditioner_set` queues changes here.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "ecs", derive(Resource))]
pub struct NetAgentState {
    reports: BTreeMap<String, EndpointReport>,
    lockstep: BTreeMap<String, crate::lockstep::LockstepStatus>,
    relays: BTreeMap<String, crate::relay::RelayReport>,
    /// `net.desync` (ADR-8455): each lockstep host's or seat's desync diagnosis.
    desync: BTreeMap<String, crate::desync::DesyncStatus>,
    /// `net.desync_inject` (ADR-8455): injections queued per lockstep label, oldest first.
    desync_injects: BTreeMap<String, Vec<crate::desync::DesyncInject>>,
    hubs: BTreeMap<String, crate::lobby::hub::HubReport>,
    /// `net.replay_audit` (ADR-8513): the game's replay hooks.
    replay_hooks: Option<std::sync::Arc<crate::desync::ReplayRegistry>>,
    /// `net.replays` (ADR-8513): the project-relative folder the game records `.lsreplay` files into.
    replay_dir: Option<String>,
    /// `net.migration` (ADR-8853): each host journal's or successor's status.
    migration: BTreeMap<String, crate::migration::JournalStatus>,
    /// The `handoff` object of `net.migration` (ADR-8901): each host's or client's hand-off status.
    handoff: BTreeMap<String, crate::handoff::HandoffStatus>,
    requests: Vec<(u64, Option<String>, ConditionerConfig)>,
    applied: BTreeMap<String, u64>,
    generation: u64,
    rollback: RollbackCounters,
    rollback_published: bool,
}

impl NetAgentState {
    /// Stores the latest report of endpoint `label` (for example `"server"` or `"client"`).
    pub fn publish(&mut self, label: &str, report: EndpointReport) {
        self.applied.entry(label.to_owned()).or_insert(self.generation);
        self.reports.insert(label.to_owned(), report);
    }

    /// Forgets an endpoint (call when it shuts down).
    pub fn remove(&mut self, label: &str) {
        self.reports.remove(label);
        self.applied.remove(label);
    }

    /// Every published endpoint, by label.
    pub fn reports(&self) -> &BTreeMap<String, EndpointReport> {
        &self.reports
    }

    /// Stores the latest status of the lockstep host or seat `label` (`net.lockstep`, ADR-8146).
    pub fn publish_lockstep(&mut self, label: &str, status: crate::lockstep::LockstepStatus) {
        self.lockstep.insert(label.to_owned(), status);
    }

    /// Forgets a lockstep host or seat (call when its match ends).
    pub fn remove_lockstep(&mut self, label: &str) {
        self.lockstep.remove(label);
    }

    /// Every published lockstep host and seat, by label.
    pub fn lockstep(&self) -> &BTreeMap<String, crate::lockstep::LockstepStatus> {
        &self.lockstep
    }

    /// Stores the latest report of relay `label` (`net.relay_status`, ADR-8183): a relay this
    /// process runs, or a host's registration with one. At most 64 relays are kept.
    pub fn publish_relay(&mut self, label: &str, report: crate::relay::RelayReport) {
        if self.relays.len() >= 64 && !self.relays.contains_key(label) {
            return;
        }
        self.relays.insert(label.to_owned(), report);
    }

    /// Forgets a relay report.
    pub fn remove_relay(&mut self, label: &str) {
        self.relays.remove(label);
    }

    /// Stores the latest report of lobby hub `label` (`lobby.hub_status`, ADR-8559). At most 16 hubs
    /// are kept.
    pub fn publish_hub(&mut self, label: &str, report: crate::lobby::hub::HubReport) {
        if self.hubs.len() >= 16 && !self.hubs.contains_key(label) {
            return;
        }
        self.hubs.insert(label.to_owned(), report);
    }

    /// Forgets a hub report.
    pub fn remove_hub(&mut self, label: &str) {
        self.hubs.remove(label);
    }

    /// Every published hub report, by label.
    pub fn hubs(&self) -> &BTreeMap<String, crate::lobby::hub::HubReport> {
        &self.hubs
    }

    /// Every published relay report, by label.
    pub fn relays(&self) -> &BTreeMap<String, crate::relay::RelayReport> {
        &self.relays
    }

    /// Stores the latest status of the host journal or successor `label` (`net.migration`,
    /// ADR-8853). At most 64 labels are kept.
    pub fn publish_migration(&mut self, label: &str, status: crate::migration::JournalStatus) {
        if self.migration.len() >= 64 && !self.migration.contains_key(label) {
            return;
        }
        self.migration.insert(label.to_owned(), status);
    }

    /// Forgets a host journal or successor (call when its match ends).
    pub fn remove_migration(&mut self, label: &str) {
        self.migration.remove(label);
    }

    /// Every published host journal and successor, by label.
    pub fn migration(&self) -> &BTreeMap<String, crate::migration::JournalStatus> {
        &self.migration
    }

    /// Stores the latest hand-off status of the host or client `label` (the `handoff` object of
    /// `net.migration`, ADR-8901). At most 64 labels are kept.
    pub fn publish_handoff(&mut self, label: &str, status: crate::handoff::HandoffStatus) {
        if self.handoff.len() >= 64 && !self.handoff.contains_key(label) {
            return;
        }
        self.handoff.insert(label.to_owned(), status);
    }

    /// Forgets a hand-off status (call when its match ends).
    pub fn remove_handoff(&mut self, label: &str) {
        self.handoff.remove(label);
    }

    /// Every published hand-off status, by label.
    pub fn handoff(&self) -> &BTreeMap<String, crate::handoff::HandoffStatus> {
        &self.handoff
    }

    /// Stores the desync diagnosis of the lockstep host or seat `label` (`net.desync`, ADR-8455):
    /// publish it with the label used for `publish_lockstep`, after each probe report or bundle.
    pub fn publish_desync(&mut self, label: &str, status: crate::desync::DesyncStatus) {
        self.desync.insert(label.to_owned(), status);
    }

    /// Every published desync diagnosis, by label.
    pub fn desync(&self) -> &BTreeMap<String, crate::desync::DesyncStatus> {
        &self.desync
    }

    /// Queues a desync injection for the lockstep host or seat `target`, or for every published
    /// one when `None` (`net.desync_inject`, development builds; ADR-8455). Returns the labels it
    /// reaches; at most 16 wait per label (the oldest is dropped).
    pub fn request_desync_inject(
        &mut self,
        target: Option<&str>,
        inject: crate::desync::DesyncInject,
    ) -> Vec<String> {
        let labels: Vec<String> = self
            .lockstep
            .keys()
            .chain(self.desync.keys())
            .filter(|label| target.is_none_or(|t| t == label.as_str()))
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for label in &labels {
            let queue = self.desync_injects.entry(label.clone()).or_default();
            if queue.len() >= 16 {
                queue.remove(0);
            }
            queue.push(inject.clone());
        }
        labels
    }

    /// The oldest desync injection queued for `label`; the game applies it in its step
    /// ([`crate::desync::DesyncInject::flip`]).
    pub fn take_desync_inject(&mut self, label: &str) -> Option<crate::desync::DesyncInject> {
        let queue = self.desync_injects.get_mut(label)?;
        if queue.is_empty() {
            return None;
        }
        Some(queue.remove(0))
    }

    /// Registers the game's replay hooks for `net.replay_audit` (ADR-8513); the engine's sample
    /// hook answers for games not in `registry`.
    pub fn set_replay_hooks(&mut self, registry: crate::desync::ReplayRegistry) {
        self.replay_hooks = Some(std::sync::Arc::new(registry));
    }

    /// The game's replay hooks, when it registered them.
    pub fn replay_hooks(&self) -> Option<&crate::desync::ReplayRegistry> {
        self.replay_hooks.as_deref()
    }

    /// Publishes the project-relative folder the game keeps its `.lsreplay` library in
    /// (`net.replays`, ADR-8513).
    pub fn set_replay_dir(&mut self, dir: impl Into<String>) {
        self.replay_dir = Some(dir.into());
    }

    /// The published replay folder.
    pub fn replay_dir(&self) -> Option<&str> {
        self.replay_dir.as_deref()
    }

    /// The injections waiting for `label`, oldest first.
    pub fn pending_desync_injects(&self, label: &str) -> &[crate::desync::DesyncInject] {
        self.desync_injects.get(label).map_or(&[], Vec::as_slice)
    }

    /// Queues a conditioner change for endpoint `target`, or for every endpoint when `None`.
    /// Returns the labels it will reach.
    pub fn request_conditioner(&mut self, target: Option<&str>, config: ConditionerConfig) -> Vec<String> {
        self.generation += 1;
        self.requests
            .push((self.generation, target.map(str::to_owned), config));
        if self.requests.len() > 16 {
            self.requests.remove(0);
        }
        self.reports
            .keys()
            .filter(|label| target.is_none_or(|t| t == label.as_str()))
            .cloned()
            .collect()
    }

    /// The newest conditioner change for `label` it has not applied yet. Endpoints call this from
    /// `publish`.
    pub fn take_conditioner_for(&mut self, label: &str) -> Option<ConditionerConfig> {
        let applied = self.applied.get(label).copied().unwrap_or(0);
        let newest = self
            .requests
            .iter()
            .filter(|(generation, target, _)| {
                *generation > applied && target.as_deref().is_none_or(|t| t == label)
            })
            .max_by_key(|(generation, _, _)| *generation)
            .map(|(_, _, config)| config.clone());
        self.applied.insert(label.to_owned(), self.generation);
        newest
    }

    /// Stores the game-owned rollback counters that `net.rollback` reports.
    ///
    /// Publishing replaces the previous counters. The host reads them from its
    /// [`crate::RollbackLog`].
    pub fn publish_rollback(&mut self, counters: RollbackCounters) {
        self.rollback = counters;
        self.rollback_published = true;
    }

    /// The counters last published, or zeros when nothing has been published.
    #[must_use]
    pub fn rollback(&self) -> RollbackCounters {
        self.rollback
    }

    /// Whether [`Self::publish_rollback`] has been called.
    #[must_use]
    pub fn rollback_published(&self) -> bool {
        self.rollback_published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(role: &str) -> NetStatus {
        NetStatus {
            interrupted: false,
            role: role.into(),
            state: "listening".into(),
            local_addr: None,
            remote_addr: None,
            peer_id: None,
            tick: 0,
            tick_rate_hz: 30,
            peer_count: 0,
            max_peers: 4,
            conditioner: ConditionerConfig::default(),
            conditioner_stats: ConditionerStats::default(),
            totals: EndpointTotals::default(),
            link: None,
            crypto: CryptoStatus::default(),
            version_mismatch: None,
            input_timing: None,
            reconnect: None,
            streams: Vec::new(),
            socket: None,
            resume_refusals: BTreeMap::new(),
            quality: None,
        }
    }

    #[test]
    fn conditioner_requests_reach_each_target_once() {
        let mut s = NetAgentState::default();
        s.publish(
            "server",
            EndpointReport {
                status: status("server"),
                peers: vec![],
                rejections: vec![],
            },
        );
        s.publish(
            "client",
            EndpointReport {
                status: status("client"),
                peers: vec![],
                rejections: vec![],
            },
        );
        let wan = ConditionerConfig::from_profile("wan").expect("profile");
        assert_eq!(
            s.request_conditioner(Some("client"), wan.clone()),
            vec!["client".to_string()]
        );
        assert_eq!(s.take_conditioner_for("server"), None, "not for the server");
        assert_eq!(s.take_conditioner_for("client"), Some(wan.clone()));
        assert_eq!(s.take_conditioner_for("client"), None, "applied once");
        let off = ConditionerConfig::default();
        assert_eq!(s.request_conditioner(None, off.clone()).len(), 2);
        assert_eq!(s.take_conditioner_for("server"), Some(off.clone()));
        assert_eq!(s.take_conditioner_for("client"), Some(off));
        s.remove("client");
        assert_eq!(s.reports().len(), 1);
    }

    #[test]
    fn overlay_summarizes_peers() {
        let row = |id: u32, rtt: f64, loss: f32, flagged: bool| PeerRow {
            interrupted: false,
            id,
            name: format!("p{id}"),
            role: "player".into(),
            addr: "127.0.0.1:1".into(),
            rtt_ms: rtt,
            jitter_ms: 1.0,
            loss_fraction: loss,
            silent_ms: 0,
            flagged,
            missed_input_ticks: 0,
            inputs_too_far_ahead: 0,
            input_feedback: None,
            grade: if flagged {
                crate::quality::LinkGrade::Poor
            } else {
                crate::quality::LinkGrade::Good
            },
            quality: None,
            link: LinkStats::default(),
            sealed: false,
            rebinds: 0,
            rebinds_refused: 0,
            path_candidate: None,
            path_probes: 0,
            path_mirrored: 0,
            inbound: Default::default(),
            streams: Vec::new(),
        };
        let o = DiagnosticsOverlay::new(
            status("server"),
            vec![row(1, 20.0, 0.0, false), row(2, 300.0, 0.1, true)],
        );
        assert_eq!(o.max_rtt_ms, 300.0);
        assert!((o.avg_loss_fraction - 0.05).abs() < 1e-6);
        assert_eq!(o.flagged_peers, 1);
        assert_eq!(o.lines().len(), 3);
        assert!(o.lines()[2].contains("FLAGGED"));
        assert_eq!(o.worst_grade, crate::quality::LinkGrade::Poor);
        assert!(o.lines()[1].contains(" good rtt"), "{}", o.lines()[1]);
        assert!(o.lines()[2].contains(" poor rtt"), "{}", o.lines()[2]);
    }

    /// KI-NT-025 (ADR-10077): refusals are counted per peer and code, a peer's row stays after
    /// it leaves, and the bound evicts the peer whose newest refusal is the oldest.
    #[test]
    fn rejections_by_peer_count_per_code_and_stay_bounded() {
        let mut by = RejectionsByPeer::default();
        by.note(3, "message_malformed");
        by.note(3, "message_malformed");
        by.note(3, "input_too_long");
        by.note(1, "unknown_lane");
        let rows = by.rows();
        assert_eq!(rows.iter().map(|r| r.peer).collect::<Vec<_>>(), vec![1, 3]);
        let three = by.peer(3).expect("row");
        assert_eq!(three.total, 3);
        assert_eq!(three.kinds["message_malformed"], 2);
        assert_eq!(three.kinds["input_too_long"], 1);
        for peer in 100..100 + MAX_REJECTION_PEERS as u32 {
            by.note(3, "message_malformed");
            by.note(peer, "unknown_lane");
        }
        assert_eq!(by.rows().len(), MAX_REJECTION_PEERS);
        assert!(by.peer(1).is_none(), "the stalest peer went first");
        assert_eq!(by.peer(3).map(|r| r.total), Some(3 + MAX_REJECTION_PEERS as u64));
        assert!(by.peer(100).is_none());
        assert_eq!(by.evicted(), 2);
    }
}
