// SPDX-License-Identifier: MPL-2.0
//! In-process session clients over a simulated link (spec 77 § 10.3, § 13, § 15).
//!
//! [`LoopbackClients`] runs client cores against a [`SessionServer`] with simulated time: messages
//! travel through a seeded `LinkShaper` per direction in dev builds (unshaped in builds without
//! `debug_assertions`, which compile the shaper out; spec 77 § 10.1) (reliable messages are never lost, only
//! delayed by resends, and keep their order; unreliable ones may be lost, duplicated or reordered),
//! levels "load" after a set number of milliseconds, and simulated clients play idle, random or
//! recorded inputs as game session messages. It backs both the session tests and
//! `NetSession::add_simulated_clients`. [`SessionHub`] bundles a server core with one.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::connection::Channel;
#[cfg(debug_assertions)]
use crate::session::conditioner::LinkShaper;
#[cfg(not(debug_assertions))]
use crate::session::conditioner::PassThroughShaper as LinkShaper;
use crate::session::conditioner::{SessionConditioner, ShaperSettings};
use crate::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, LinkId, Outgoing, SessionClient, SessionServer,
    SessionSettings,
};
use crate::session::manifest::ContentManifest;
use crate::session::meter::{ConnectionStats, DirMeter, LinkMeter};
use crate::session::proto::{DisconnectReason, Hello, kind_name};
use crate::testing::SplitMix64;

/// Links of loopback clients start here, so they never collide with a real transport's ids.
pub const LOOPBACK_LINK_BASE: u32 = 0x4000_0000;
/// Most simulated clients one session runs.
pub const MAX_SIMULATED_CLIENTS: usize = 256;
/// The `format_version` of a simulated-input recording (`*.siminput.ron`).
pub const SIM_RECORDING_FORMAT_VERSION: u32 = 1;

/// What a simulated client plays (spec 77 § 10.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimInput {
    /// Sends nothing but keepalives.
    Idle,
    /// One random 8-byte game message per tick from a seeded generator.
    Random {
        /// Seed (each client adds its index).
        seed: u64,
    },
    /// Plays these game messages, one per tick, looping.
    Recording(Vec<Vec<u8>>),
}

/// A recorded input file for simulated clients (`*.siminput.ron`, dev only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename = "SimRecording")]
pub struct SimRecording {
    /// Layout version, [`SIM_RECORDING_FORMAT_VERSION`].
    pub format_version: u32,
    /// One game message per tick.
    pub frames: Vec<Vec<u8>>,
}

impl SimRecording {
    /// Parses the file text; refuses other versions.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        let r: SimRecording = ron::from_str(text).map_err(|e| e.to_string())?;
        if r.format_version != SIM_RECORDING_FORMAT_VERSION {
            return Err(format!(
                "format_version {} is not {SIM_RECORDING_FORMAT_VERSION}",
                r.format_version
            ));
        }
        Ok(r)
    }
}

#[derive(Debug, Clone)]
enum Payload {
    Data(Channel, Vec<u8>),
    Close,
}

#[derive(Debug, Clone)]
struct InFlight {
    at: u64,
    order: u64,
    payload: Payload,
}

/// One loopback client.
#[derive(Debug)]
pub struct LoopbackClient {
    /// Its link on the server.
    pub link: LinkId,
    /// Its session core.
    pub core: SessionClient,
    /// Every event it produced, in order.
    pub log: Vec<ClientSessionEvent>,
    /// Milliseconds a level takes to load (`None` = never finishes).
    pub load_ms: Option<u64>,
    /// What it plays once in game.
    pub input: SimInput,
    /// True for clients made by `add_simulated_clients`.
    pub simulated: bool,
    addr: SocketAddr,
    connect_at: Option<u64>,
    up: bool,
    to_server: Vec<InFlight>,
    to_client: Vec<InFlight>,
    last_reliable_up: u64,
    last_reliable_down: u64,
    shaper_up: LinkShaper,
    shaper_down: LinkShaper,
    load_due: Option<(u32, u64)>,
    rng: SplitMix64,
    frame: usize,
    hello: Hello,
    meter: LinkMeter,
}

impl LoopbackClient {
    /// Traffic counters of this link (spec 77 § 10.2).
    pub fn meter(&self) -> &LinkMeter {
        &self.meter
    }

    /// The state.
    pub fn state(&self) -> &ClientSessionState {
        self.core.state()
    }

    /// True when the client reached `InGame`.
    pub fn in_game(&self) -> bool {
        *self.core.state() == ClientSessionState::InGame
    }

    /// The reason of the last disconnect, if disconnected.
    pub fn disconnect_reason(&self) -> Option<&DisconnectReason> {
        match self.core.state() {
            ClientSessionState::Disconnected(r) => r.as_ref(),
            _ => None,
        }
    }

    /// Reconnects with the same `Hello`, carrying the reconnect token of the last session.
    pub fn reconnect(&mut self, now_ms: u64, latency_ms: u64) {
        let mut hello = self.hello.clone();
        hello.reconnect_token = self.core.reconnect_token();
        self.core.connect(hello, now_ms);
        self.connect_at = Some(now_ms + latency_ms);
        self.up = false;
        self.to_server.clear();
        self.to_client.clear();
    }

    /// Simulates the transport dying under the client (cable pulled): neither side is told; the
    /// server notices by silence.
    pub fn cut(&mut self) {
        self.up = false;
        self.to_server.clear();
        self.to_client.clear();
    }
}

/// Loopback clients of one server, with simulated time.
#[derive(Debug)]
pub struct LoopbackClients {
    clients: Vec<LoopbackClient>,
    next_link: u32,
    conditioner: SessionConditioner,
    settings: SessionSettings,
    order: u64,
}

impl LoopbackClients {
    /// No clients yet.
    pub fn new(settings: SessionSettings) -> Self {
        Self {
            clients: Vec::new(),
            next_link: LOOPBACK_LINK_BASE,
            conditioner: SessionConditioner::default(),
            settings,
            order: 0,
        }
    }

    /// The conditioner applied to clients added from now on (outbound = client to server).
    pub fn set_conditioner(&mut self, conditioner: SessionConditioner) {
        self.conditioner = conditioner;
    }

    /// The conditioner.
    pub fn conditioner(&self) -> &SessionConditioner {
        &self.conditioner
    }

    /// Adds a client that connects with `hello` (the transport comes up after one latency).
    pub fn add(&mut self, hello: Hello, now_ms: u64, input: SimInput, simulated: bool) -> usize {
        self.add_delayed(hello, now_ms, 0, input, simulated)
    }

    /// Like [`Self::add`], with the transport connect starting `delay_ms` later (spreads a burst
    /// of simulated joins under the server's `max_handshaking`).
    pub fn add_delayed(
        &mut self,
        hello: Hello,
        now_ms: u64,
        delay_ms: u64,
        input: SimInput,
        simulated: bool,
    ) -> usize {
        let link = LinkId(self.next_link);
        self.next_link += 1;
        let index = self.clients.len();
        let seed = self
            .conditioner
            .seed
            .wrapping_add(u64::from(link.0).wrapping_mul(0x9e37_79b9));
        let mut core = SessionClient::new(self.settings.clone());
        core.connect(hello.clone(), now_ms);
        let up = self.conditioner.outbound;
        let down = self.conditioner.inbound;
        let latency = u64::from(up.latency_ms.max(1));
        let port = 20_000u16.wrapping_add((link.0 - LOOPBACK_LINK_BASE) as u16);
        self.clients.push(LoopbackClient {
            link,
            core,
            log: Vec::new(),
            load_ms: Some(0),
            input,
            simulated,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            connect_at: Some(now_ms + delay_ms + latency),
            up: false,
            to_server: Vec::new(),
            to_client: Vec::new(),
            last_reliable_up: 0,
            last_reliable_down: 0,
            shaper_up: LinkShaper::new(up, seed),
            shaper_down: LinkShaper::new(down, seed ^ 0xd00d),
            load_due: None,
            rng: SplitMix64::new(seed),
            frame: 0,
            hello,
            meter: LinkMeter::default(),
        });
        index
    }

    /// Changes one client's address (for address-ban tests). Before it connects.
    pub fn set_addr(&mut self, index: usize, addr: SocketAddr) {
        if let Some(c) = self.clients.get_mut(index) {
            c.addr = addr;
        }
    }

    /// The clients, in the order added.
    pub fn clients(&self) -> &[LoopbackClient] {
        &self.clients
    }

    /// One client.
    pub fn client(&self, index: usize) -> &LoopbackClient {
        &self.clients[index]
    }

    /// One client, mutably.
    pub fn client_mut(&mut self, index: usize) -> &mut LoopbackClient {
        &mut self.clients[index]
    }

    /// One stats-overlay row per client, in join order (spec 77 § 10.2).
    pub fn stats(&self, now_ms: u64) -> Vec<ConnectionStats> {
        let mut real = 0;
        let mut sim = 0;
        self.clients
            .iter()
            .map(|c| {
                let label = if c.simulated {
                    sim += 1;
                    format!("sim-{sim}")
                } else {
                    real += 1;
                    format!("client-{real}")
                };
                ConnectionStats::from_meter(
                    &label,
                    c.core.peer_id().map(|p| p.0),
                    c.core.state().as_str(),
                    c.simulated,
                    &c.meter,
                    now_ms,
                )
            })
            .collect()
    }

    /// Simulated clients still connected or connecting.
    pub fn simulated_live(&self) -> usize {
        self.clients
            .iter()
            .filter(|c| c.simulated && !matches!(c.core.state(), ClientSessionState::Disconnected(_)))
            .count()
    }

    /// Disconnects the newest `n` live simulated clients (`client_quit`). Returns how many.
    pub fn remove_simulated(&mut self, n: usize, now_ms: u64) -> usize {
        let mut removed = 0;
        for c in self.clients.iter_mut().rev() {
            if removed == n {
                break;
            }
            if c.simulated
                && !matches!(
                    c.core.state(),
                    ClientSessionState::Disconnected(_) | ClientSessionState::Disconnecting
                )
            {
                c.core.disconnect(DisconnectReason::ClientQuit, now_ms);
                removed += 1;
            }
        }
        removed
    }

    /// Forgets clients that are disconnected and have nothing in flight; returns them. Suspended
    /// clients stay: they may resume with their reconnect token.
    pub fn prune(&mut self) -> Vec<LoopbackClient> {
        let (gone, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.clients).into_iter().partition(|c| {
            matches!(c.core.state(), ClientSessionState::Disconnected(r) if r.as_ref() != Some(&DisconnectReason::Suspended))
                && c.to_server.is_empty()
                && c.to_client.is_empty()
        });
        self.clients = keep;
        gone
    }

    /// Cuts a client's transport silently (see [`LoopbackClient::cut`]); the server notices by
    /// silence.
    pub fn cut(&mut self, index: usize) {
        if let Some(c) = self.clients.get_mut(index) {
            c.cut();
        }
    }

    fn schedule(
        shaper: &mut LinkShaper,
        meter: &mut DirMeter,
        queue: &mut Vec<InFlight>,
        last_reliable: &mut u64,
        order: &mut u64,
        payload: Payload,
        now_ms: u64,
    ) {
        let (reliable, len) = match &payload {
            Payload::Data(channel, b) => (channel.is_reliable(), b.len()),
            Payload::Close => (true, 0),
        };
        let times = shaper.shape(len, now_ms, !reliable);
        if !matches!(payload, Payload::Close) {
            let kind = match &payload {
                Payload::Data(_, b) => kind_name(b),
                Payload::Close => "close",
            };
            let delay = times.iter().min().map(|t| t.saturating_sub(now_ms) as f64);
            meter.record(kind, len, now_ms, delay);
        }
        for mut at in times {
            if reliable {
                // Reliable-ordered: never before an earlier reliable message.
                at = at.max(*last_reliable);
                *last_reliable = at;
            }
            *order += 1;
            queue.push(InFlight {
                at,
                order: *order,
                payload: payload.clone(),
            });
        }
    }

    fn take_due(queue: &mut Vec<InFlight>, now_ms: u64) -> Vec<Payload> {
        let mut due: Vec<InFlight> = Vec::new();
        queue.retain(|m| {
            if m.at <= now_ms {
                due.push(m.clone());
                false
            } else {
                true
            }
        });
        due.sort_by_key(|m| (m.at, m.order));
        due.into_iter().map(|m| m.payload).collect()
    }

    /// One tick: delivers due messages both ways, runs level loads and inputs, updates the server
    /// and every client, and schedules what they sent. Server output for links that are not
    /// loopback clients (a real transport's) is returned: `(messages, closes)`.
    pub fn step(&mut self, server: &mut SessionServer, now_ms: u64) -> (Vec<Outgoing>, Vec<LinkId>) {
        let mut other_out = Vec::new();
        let mut other_closes = Vec::new();
        // Transports that come up.
        for c in &mut self.clients {
            if let Some(at) = c.connect_at
                && at <= now_ms
            {
                c.connect_at = None;
                c.up = true;
                c.last_reliable_up = 0;
                c.last_reliable_down = 0;
                server.link_up(c.link, c.addr, now_ms);
                c.core.link_up(now_ms);
            }
        }
        // Deliveries.
        for c in &mut self.clients {
            for p in Self::take_due(&mut c.to_server, now_ms) {
                match p {
                    Payload::Data(_, bytes) => server.receive(c.link, &bytes, now_ms),
                    Payload::Close => server.link_down(c.link, DisconnectReason::ClientQuit, now_ms),
                }
            }
            for p in Self::take_due(&mut c.to_client, now_ms) {
                match p {
                    Payload::Data(_, bytes) => c.core.receive(&bytes, now_ms),
                    Payload::Close => {
                        c.up = false;
                        c.core.link_down(
                            DisconnectReason::TransportError("closed by the server".into()),
                            now_ms,
                        );
                    }
                }
            }
        }
        // Level loads and inputs.
        for c in &mut self.clients {
            while let Some(e) = c.core.poll_event() {
                if let ClientSessionEvent::LoadLevel { travel_id, .. } = &e {
                    c.load_due = c.load_ms.map(|ms| (*travel_id, now_ms + ms));
                }
                c.log.push(e);
            }
            if let Some((travel, at)) = c.load_due
                && at <= now_ms
            {
                c.load_due = None;
                c.core.level_loaded(travel, now_ms);
            }
            if c.in_game() {
                match &c.input {
                    SimInput::Idle => {}
                    SimInput::Random { .. } => {
                        let v = c.rng.next_u64();
                        c.core.send_game(v.to_le_bytes().to_vec());
                    }
                    SimInput::Recording(frames) => {
                        if !frames.is_empty() {
                            let f = frames[c.frame % frames.len()].clone();
                            c.frame += 1;
                            c.core.send_game(f);
                        }
                    }
                }
            }
        }
        server.update(now_ms);
        for c in &mut self.clients {
            c.core.update(now_ms);
            while let Some(e) = c.core.poll_event() {
                c.log.push(e);
            }
        }
        // Route outgoing.
        let out = server.take_outgoing();
        for o in out {
            if let Some(c) = self.clients.iter_mut().find(|c| c.link == o.link) {
                if !c.up {
                    continue;
                }
                Self::schedule(
                    &mut c.shaper_down,
                    &mut c.meter.down,
                    &mut c.to_client,
                    &mut c.last_reliable_down,
                    &mut self.order,
                    Payload::Data(o.channel, o.bytes),
                    now_ms,
                );
            } else if o.link.0 < LOOPBACK_LINK_BASE {
                other_out.push(o);
            }
        }
        for link in server.take_closes() {
            if link.0 < LOOPBACK_LINK_BASE {
                other_closes.push(link);
            }
            if let Some(c) = self.clients.iter_mut().find(|c| c.link == link)
                && c.up
            {
                Self::schedule(
                    &mut c.shaper_down,
                    &mut c.meter.down,
                    &mut c.to_client,
                    &mut c.last_reliable_down,
                    &mut self.order,
                    Payload::Close,
                    now_ms,
                );
            }
        }
        for c in &mut self.clients {
            let out = c.core.take_outgoing();
            if c.up {
                for (channel, bytes) in out {
                    Self::schedule(
                        &mut c.shaper_up,
                        &mut c.meter.up,
                        &mut c.to_server,
                        &mut c.last_reliable_up,
                        &mut self.order,
                        Payload::Data(channel, bytes),
                        now_ms,
                    );
                }
            }
            if c.core.take_close() && c.up {
                c.up = false;
                Self::schedule(
                    &mut c.shaper_up,
                    &mut c.meter.up,
                    &mut c.to_server,
                    &mut c.last_reliable_up,
                    &mut self.order,
                    Payload::Close,
                    now_ms,
                );
            }
        }
        (other_out, other_closes)
    }
}

/// A server core and its loopback clients with a simulated clock: the session test bench.
#[derive(Debug)]
pub struct SessionHub {
    /// The server.
    pub server: SessionServer,
    /// The clients.
    pub clients: LoopbackClients,
    /// Simulated time, milliseconds.
    pub now_ms: u64,
    /// Milliseconds per step.
    pub step_ms: u64,
    /// Unix-epoch second `now_ms: 0` stands for, so the server's injected ban clock tracks the
    /// simulated time from a realistic base (ADR-4772).
    unix_base_s: u64,
}

impl SessionHub {
    /// A hub at time 0 with a fixed secret (deterministic ids and tokens).
    pub fn new(settings: SessionSettings, opts: HostOptions, manifest: ContentManifest) -> Self {
        let step_ms = (1000 / u64::from(opts.tick_hz.max(1))).max(1);
        Self {
            server: SessionServer::new(settings.clone(), opts, manifest, [7; 32]),
            clients: LoopbackClients::new(settings),
            now_ms: 0,
            step_ms,
            unix_base_s: crate::session::unix_now_s(),
        }
    }

    /// Sets the conditioner for clients added from now on.
    pub fn set_conditioner(&mut self, outbound: ShaperSettings, inbound: ShaperSettings, seed: u64) {
        self.clients.set_conditioner(SessionConditioner {
            outbound,
            inbound,
            seed,
        });
    }

    /// Adds a client.
    pub fn add_client(&mut self, hello: Hello) -> usize {
        self.clients.add(hello, self.now_ms, SimInput::Idle, false)
    }

    /// Replaces the unix base `now_ms: 0` stands for (tests, replays), so a recorded session's
    /// bans expire at the recorded times instead of the wall second the hub was constructed at.
    /// The default base is the wall second at construction.
    pub fn set_unix_base_s(&mut self, unix_s: u64) {
        self.unix_base_s = unix_s;
    }

    /// One step.
    pub fn step(&mut self) {
        self.now_ms += self.step_ms;
        self.server
            .set_unix_now_s(self.unix_base_s.saturating_add(self.now_ms / 1000));
        let _ = self.clients.step(&mut self.server, self.now_ms);
    }

    /// Steps `n` times.
    pub fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.step();
        }
    }

    /// Steps for `seconds` of simulated time (rounded up to whole steps).
    pub fn run_secs(&mut self, seconds: f64) {
        let steps = ((seconds * 1000.0) / self.step_ms as f64).ceil() as u64;
        self.run(steps);
    }

    /// Steps until `done` holds or `max` steps passed; returns whether it held.
    pub fn run_until(&mut self, max: u64, mut done: impl FnMut(&SessionHub) -> bool) -> bool {
        for _ in 0..max {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit rev-kimi-17 fixrev-15-k finding 8 (ADR-5444's M2 setter had no test): the base
    /// `set_unix_base_s` supplies is the unix second `now_ms: 0` stands for on the server's ban
    /// clock, so a recorded session's bans expire at the recorded times on every host.
    #[test]
    fn set_unix_base_s_drives_the_server_ban_clock() {
        let manifest = ContentManifest::build("0.0.0", "0.0.0", 1, 0, &[]);
        let mut hub = SessionHub::new(SessionSettings::default(), HostOptions::default(), manifest);
        hub.set_unix_base_s(1_000_000);
        hub.step_ms = 500;
        hub.run(5);
        assert_eq!(hub.now_ms, 2_500);
        assert_eq!(
            hub.server.now_unix_s(),
            1_000_002,
            "the recorded base plus the simulated whole seconds"
        );
    }
}
