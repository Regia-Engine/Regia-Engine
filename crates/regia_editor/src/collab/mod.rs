// SPDX-License-Identifier: MPL-2.0
//! Optional LAN collaboration presence and camera jumps (ADR-7842).
//!
//! This editor-only session carries replaceable camera/selection snapshots, never edits.
//! It opens no socket by default. Host admission is explicit for each endpoint/nonce pair.
//! The main GUI and headless loops call [`tick`]; all limits apply before accepting peer state.

mod agent;
mod indicators;
mod transport;
mod ui;

pub use agent::methods;
pub use indicators::{outliner_indicator, paint};
pub use ui::{show_window, toggle_window};

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_scene::prelude::EntityGuid;
use serde::{Deserialize, Serialize};

use crate::selection::Selection;
use crate::viewport::EditorCamera;
use crate::views::{ViewKind, ViewportLayout};
use transport::{MAX_PACKET, Transport};

const INTERVAL: Duration = Duration::from_millis(100);
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PARTICIPANTS: usize = 8;
const MAX_SELECTION: usize = 64;
const MAX_RECEIVE: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct CameraPose {
    target: [f32; 3],
    distance: f32,
    yaw: f32,
    pitch: f32,
    fov_deg: f32,
    kind: String,
}

impl CameraPose {
    fn from_camera(camera: &EditorCamera, kind: ViewKind) -> Self {
        let (yaw, pitch) = kind.ortho_angles().unwrap_or((camera.yaw, camera.pitch));
        Self {
            target: camera.target.to_array(),
            distance: camera.distance,
            yaw,
            pitch,
            fov_deg: camera.fov_deg,
            kind: kind.name().into(),
        }
    }

    fn valid(&self) -> bool {
        self.target.iter().all(|v| v.is_finite() && v.abs() <= 1.0e8)
            && self.distance.is_finite()
            && (0.001..=1.0e8).contains(&self.distance)
            && self.yaw.is_finite()
            && self.yaw.abs() <= 1.0e6
            && self.pitch.is_finite()
            && self.pitch.abs() <= std::f32::consts::FRAC_PI_2
            && self.fov_deg.is_finite()
            && (1.0..=179.0).contains(&self.fov_deg)
            && ViewKind::from_name(&self.kind).is_some()
    }

    fn camera(&self, speed: f32) -> EditorCamera {
        EditorCamera {
            target: self.target.into(),
            distance: self.distance,
            yaw: self.yaw,
            pitch: self.pitch,
            fov_deg: self.fov_deg,
            speed,
            // Clip planes are session state (ADR-7755): not in the presence pose.
            ..EditorCamera::default()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Presence {
    camera: CameraPose,
    selection: Vec<Guid>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            camera: CameraPose::from_camera(&EditorCamera::default(), ViewKind::Perspective),
            selection: vec![],
        }
    }
}

impl Presence {
    fn capture(world: &World) -> Self {
        let layout = world
            .get_resource::<ViewportLayout>()
            .cloned()
            .unwrap_or_default();
        let (kind, camera) = layout.pane_camera(world, layout.active);
        let selection = world
            .get_resource::<Selection>()
            .map(|s| {
                s.entities()
                    .iter()
                    .filter_map(|e| world.get::<EntityGuid>(*e).map(|g| g.0))
                    .filter(|guid| !guid.is_nil())
                    .take(MAX_SELECTION)
                    .collect()
            })
            .unwrap_or_default();
        let presence = Self {
            camera: CameraPose::from_camera(&camera, kind),
            selection,
        };
        if presence.valid() {
            presence
        } else {
            Self::default()
        }
    }

    fn valid(&self) -> bool {
        self.camera.valid()
            && self.selection.len() <= MAX_SELECTION
            && self.selection.iter().all(|guid| !guid.is_nil())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Participant {
    id: u64,
    name: String,
    color: u8,
    presence: Presence,
}

impl Participant {
    fn valid(&self) -> bool {
        self.id > 0
            && valid_name(&self.name)
            && usize::from(self.color) < MAX_PARTICIPANTS
            && self.presence.valid()
    }
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    message: Message,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Message {
    Hello {
        nonce: Guid,
        name: String,
    },
    State {
        nonce: Guid,
        epoch: Guid,
        seq: u64,
        presence: Presence,
    },
    Snapshot {
        nonce: Guid,
        epoch: Guid,
        seq: u64,
        you: u64,
        participants: Vec<Participant>,
    },
    Pending {
        nonce: Guid,
    },
    Rejected {
        nonce: Guid,
    },
    Leave {
        nonce: Guid,
        epoch: Option<Guid>,
    },
    Closed {
        nonce: Guid,
        epoch: Guid,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Pending,
    Accepted,
    Rejected,
}

struct Peer {
    nonce: Guid,
    participant: Participant,
    admission: Admission,
    last_seen: Duration,
    sequence: u64,
}

impl Peer {
    /// A fresh join request with the next host-assigned id.
    fn pending(nonce: Guid, next_id: &mut u64, name: String, now: Duration) -> Self {
        let id = *next_id;
        *next_id = next_id.saturating_add(1);
        Self {
            nonce,
            participant: Participant {
                id,
                name,
                color: 0,
                presence: Presence::default(),
            },
            admission: Admission::Pending,
            last_seen: now,
            sequence: 0,
        }
    }
}

struct Host {
    epoch: Guid,
    own: Participant,
    next_id: u64,
    peers: BTreeMap<SocketAddr, Peer>,
}

struct Client {
    host: SocketAddr,
    nonce: Guid,
    name: String,
    epoch: Option<Guid>,
    id: Option<u64>,
    last_seen: Duration,
    roster_sequence: u64,
    roster: Vec<Participant>,
}

enum Role {
    Off,
    Host(Host),
    Client(Client),
}

#[derive(Resource)]
struct Session {
    role: Role,
    transport: Option<Transport>,
    clock: Instant,
    last_tick: Option<Duration>,
    last_frame: Option<u64>,
    sequence: u64,
    sent: u64,
    received: u64,
    invalid: u64,
    error: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            role: Role::Off,
            transport: None,
            clock: Instant::now(),
            last_tick: None,
            last_frame: None,
            sequence: 0,
            sent: 0,
            received: 0,
            invalid: 0,
            error: None,
        }
    }
}

impl Session {
    fn host(transport: Transport, name: String) -> Self {
        Self {
            role: Role::Host(Host {
                epoch: Guid::new_random(),
                own: Participant {
                    id: 1,
                    name,
                    color: 0,
                    presence: Presence::default(),
                },
                next_id: 2,
                peers: BTreeMap::new(),
            }),
            transport: Some(transport),
            ..Self::default()
        }
    }

    fn join(transport: Transport, host: SocketAddr, name: String) -> Self {
        Self {
            role: Role::Client(Client {
                host,
                nonce: Guid::new_random(),
                name,
                epoch: None,
                id: None,
                last_seen: Duration::ZERO,
                roster_sequence: 0,
                roster: vec![],
            }),
            transport: Some(transport),
            ..Self::default()
        }
    }

    fn self_id(&self) -> Option<u64> {
        match &self.role {
            Role::Off => None,
            Role::Host(_) => Some(1),
            Role::Client(c) => c.id,
        }
    }

    fn participants(&self) -> Vec<Participant> {
        match &self.role {
            Role::Off => vec![],
            Role::Host(h) => std::iter::once(h.own.clone())
                .chain(
                    h.peers
                        .values()
                        .filter(|p| p.admission == Admission::Accepted)
                        .map(|p| p.participant.clone()),
                )
                .collect(),
            Role::Client(c) => c.roster.clone(),
        }
    }

    fn remote_participants(&self) -> Vec<Participant> {
        self.participants()
            .into_iter()
            .filter(|p| Some(p.id) != self.self_id())
            .collect()
    }

    fn pending(&self) -> Vec<serde_json::Value> {
        match &self.role {
            Role::Host(h) => h.peers.iter().filter(|(_, p)| p.admission == Admission::Pending)
                .map(|(address, p)| serde_json::json!({"id":p.participant.id,"name":p.participant.name,"address":address.to_string()})).collect(),
            _ => vec![],
        }
    }

    fn send(&mut self, to: SocketAddr, message: Message) {
        let Some(transport) = &self.transport else { return };
        let Ok(bytes) = serde_json::to_vec(&Envelope { version: 1, message }) else {
            return;
        };
        match transport.send(&bytes, to) {
            Ok(()) => self.sent += 1,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => self.error = Some("collab_io".into()),
        }
    }

    fn close(&mut self, reason: Option<&str>) {
        self.role = Role::Off;
        self.transport = None;
        self.error = reason.map(str::to_owned);
    }

    fn leave(&mut self) {
        let messages: Vec<_> = match &self.role {
            Role::Off => vec![],
            Role::Host(h) => h
                .peers
                .iter()
                .map(|(address, p)| {
                    (
                        *address,
                        Message::Closed {
                            nonce: p.nonce,
                            epoch: h.epoch,
                        },
                    )
                })
                .collect(),
            Role::Client(c) => vec![(
                c.host,
                Message::Leave {
                    nonce: c.nonce,
                    epoch: c.epoch,
                },
            )],
        };
        for (address, message) in messages {
            self.send(address, message);
        }
        self.close(None);
    }

    fn receive(&mut self, now: Duration) {
        // Fit the entire UDP datagram, then reject oversized envelopes on every platform.
        let mut bytes = vec![0; 65_536];
        for _ in 0..MAX_RECEIVE {
            let Some(transport) = &self.transport else { break };
            let (len, from) = match transport.receive(&mut bytes) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                // UDP reset/refusal can describe one earlier datagram's unreachable peer
                // (not a broken host socket). Retrying still spends this update's bounded work.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue;
                }
                Err(_) => {
                    self.close(Some("collab_io"));
                    break;
                }
            };
            self.received += 1;
            if len > MAX_PACKET || !transport::lan_ip(from.ip()) {
                self.invalid += 1;
                continue;
            }
            match serde_json::from_slice::<Envelope>(&bytes[..len]) {
                Ok(Envelope { version: 1, message }) => self.handle(from, message, now),
                _ => self.invalid += 1,
            }
        }
    }

    fn handle(&mut self, from: SocketAddr, message: Message, now: Duration) {
        match &mut self.role {
            Role::Host(h) => match message {
                Message::Hello { nonce, name } if !nonce.is_nil() && valid_name(&name) => {
                    if let Some(peer) = h.peers.get_mut(&from) {
                        if peer.nonce == nonce {
                            if peer.participant.name == name {
                                peer.last_seen = now;
                            }
                        } else if peer.admission != Admission::Accepted {
                            // A new join from the same socket replaces a stale pending or
                            // rejected request (new id, explicit acceptance again). The
                            // count of unaccepted entries does not change.
                            *peer = Peer::pending(nonce, &mut h.next_id, name, now);
                        }
                    } else if h
                        .peers
                        .values()
                        .filter(|p| p.admission != Admission::Accepted)
                        .count()
                        < 7
                    {
                        let peer = Peer::pending(nonce, &mut h.next_id, name, now);
                        h.peers.insert(from, peer);
                    }
                }
                Message::State {
                    nonce,
                    epoch,
                    seq,
                    presence,
                } if epoch == h.epoch && presence.valid() => {
                    if let Some(peer) = h.peers.get_mut(&from)
                        && peer.nonce == nonce
                        && peer.admission == Admission::Accepted
                        && seq > peer.sequence
                    {
                        peer.sequence = seq;
                        peer.participant.presence = presence;
                        peer.last_seen = now;
                    }
                }
                Message::Leave { nonce, epoch } => {
                    if h.peers.get(&from).is_some_and(|p| {
                        p.nonce == nonce
                            && (p.admission != Admission::Accepted
                                || (epoch.is_none() || epoch == Some(h.epoch)))
                    }) {
                        h.peers.remove(&from);
                    }
                }
                _ => self.invalid += 1,
            },
            Role::Client(c) if from == c.host => match message {
                Message::Pending { nonce } if nonce == c.nonce && c.id.is_none() => c.last_seen = now,
                Message::Rejected { nonce } if nonce == c.nonce => self.close(Some("collab_rejected")),
                Message::Closed { nonce, epoch }
                    if nonce == c.nonce && c.epoch.is_none_or(|e| e == epoch) =>
                {
                    self.close(Some("collab_host_left"));
                }
                Message::Snapshot {
                    nonce,
                    epoch,
                    seq,
                    you,
                    participants,
                } if nonce == c.nonce
                    && !epoch.is_nil()
                    && c.epoch.is_none_or(|e| e == epoch)
                    && seq > c.roster_sequence
                    && valid_roster(&participants, you) =>
                {
                    c.epoch = Some(epoch);
                    c.id = Some(you);
                    c.roster_sequence = seq;
                    c.roster = participants;
                    c.last_seen = now;
                }
                _ => self.invalid += 1,
            },
            _ => self.invalid += 1,
        }
    }

    fn update(&mut self, presence: Presence, now: Duration) {
        if self.transport.is_none()
            || self
                .last_tick
                .is_some_and(|last| now.saturating_sub(last) < INTERVAL)
        {
            return;
        }
        self.last_tick = Some(now);
        self.sequence = self.sequence.saturating_add(1);
        // Read before expiring: a local hitch longer than the timeout leaves fresh datagrams
        // queued, and judging silence first would drop a live session. Only a strictly newer
        // valid packet refreshes `last_seen`, and nothing recreates an expired peer.
        self.receive(now);
        match &mut self.role {
            Role::Host(h) => h.peers.retain(|_, p| now.saturating_sub(p.last_seen) < TIMEOUT),
            Role::Client(c) if now.saturating_sub(c.last_seen) >= TIMEOUT => {
                self.close(Some("collab_timeout"));
                return;
            }
            _ => {}
        }
        let messages = match &mut self.role {
            Role::Off => vec![],
            Role::Host(h) => {
                h.own.presence = presence;
                let participants: Vec<_> = std::iter::once(h.own.clone())
                    .chain(
                        h.peers
                            .values()
                            .filter(|p| p.admission == Admission::Accepted)
                            .map(|p| p.participant.clone()),
                    )
                    .collect();
                h.peers
                    .iter()
                    .map(|(address, p)| {
                        (
                            *address,
                            match p.admission {
                                Admission::Pending => Message::Pending { nonce: p.nonce },
                                Admission::Rejected => Message::Rejected { nonce: p.nonce },
                                Admission::Accepted => Message::Snapshot {
                                    nonce: p.nonce,
                                    epoch: h.epoch,
                                    seq: self.sequence,
                                    you: p.participant.id,
                                    participants: participants.clone(),
                                },
                            },
                        )
                    })
                    .collect()
            }
            Role::Client(c) => vec![(
                c.host,
                match c.epoch {
                    Some(epoch) => Message::State {
                        nonce: c.nonce,
                        epoch,
                        seq: self.sequence,
                        presence,
                    },
                    None => Message::Hello {
                        nonce: c.nonce,
                        name: c.name.clone(),
                    },
                },
            )],
        };
        for (address, message) in messages {
            self.send(address, message);
        }
    }
}

fn valid_roster(participants: &[Participant], you: u64) -> bool {
    (2..=MAX_PARTICIPANTS).contains(&participants.len())
        && you != 1
        && participants.iter().all(Participant::valid)
        && participants.iter().any(|p| p.id == 1)
        && participants.iter().any(|p| p.id == you)
        && participants.iter().enumerate().all(|(i, p)| {
            participants[..i]
                .iter()
                .all(|old| old.id != p.id && old.color != p.color)
        })
}

/// Polls an enabled editor session without blocking. Presence is sent at most ten times/second.
/// Call from the edit world's frame loop, including in headless mode; reads never pump I/O.
pub fn tick(world: &mut World) {
    let Some(session) = world.get_resource::<Session>() else {
        return;
    };
    let now = session.clock.elapsed();
    tick_at(world, now);
}

/// Polls presence once per GUI frame, even when egui repeats its layout pass.
pub fn frame_tick(world: &mut World, ctx: &egui::Context) {
    let frame = ctx.cumulative_frame_nr();
    let Some(mut session) = world.get_resource_mut::<Session>() else {
        return;
    };
    if session.transport.is_none() || session.last_frame == Some(frame) {
        return;
    }
    session.last_frame = Some(frame);
    if session.transport.is_some() {
        ctx.request_repaint_after(INTERVAL);
    }
    tick(world);
}

fn tick_at(world: &mut World, now: Duration) {
    if !world.get_resource::<Session>().is_some_and(|session| {
        session.transport.is_some()
            && session
                .last_tick
                .is_none_or(|last| now.saturating_sub(last) >= INTERVAL)
    }) {
        return;
    }
    let Some(mut session) = world.remove_resource::<Session>() else {
        return;
    };
    if session.transport.is_some() {
        let _span = tracing::info_span!(target: "regia_editor", "collab_presence").entered();
        session.update(Presence::capture(world), now);
    }
    world.insert_resource(session);
}

fn selected_by(world: &World, entity: Entity) -> Vec<Participant> {
    let Some(guid) = world.get::<EntityGuid>(entity) else {
        return vec![];
    };
    world
        .get_resource::<Session>()
        .map(|s| {
            s.remote_participants()
                .into_iter()
                .filter(|p| p.presence.selection.contains(&guid.0))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a remote selection needs the viewport's entity-id readback even with no local selection.
#[must_use]
pub fn needs_ids(world: &World) -> bool {
    world.get_resource::<Session>().is_some_and(|s| {
        s.remote_participants()
            .iter()
            .any(|p| !p.presence.selection.is_empty())
    })
}

#[cfg(test)]
mod tests;
