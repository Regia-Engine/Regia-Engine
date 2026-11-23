// SPDX-License-Identifier: MPL-2.0
//! The debug echo (spec 75 § 11, `lag_compensation.debug_echo`): in dev builds the server sends a
//! shooter the rewound shapes of its hits, so a developer sees over the proxy they aimed at
//! whether the shown and the judged pose agree.
//!
//! Add [`LagCompEchoPlugin`] on every process (the message feeds the protocol hash). A release
//! build compiles the path but never queues an echo.

use std::collections::VecDeque;

use glam::{Quat, Vec3};
use regia_app::{App, FixedUpdate, Plugin};
use regia_ecs::prelude::*;
use serde::Serialize;

use super::{HitShapeKind, LagCompHistory, PosedShape, QueryRecord, ShapeReport};
use crate::message::{
    Channel, Direction, NetMessage, NetMessageAppExt, NetMessages, NetSender, ToOwnerMessage,
};
use crate::model::{NetMode, PeerId};
use crate::plugin::NetSet;

/// Echoes a client keeps in [`LagCompEchoLog`].
pub const ECHO_LOG_LEN: usize = 16;

/// One rewound shape in a [`LagCompEcho`].
#[derive(Clone, Debug, PartialEq, crate::Replicate)]
pub struct EchoShape {
    /// The shape's tag.
    pub tag: String,
    /// The shape and its size.
    pub kind: HitShapeKind,
    /// World-space center.
    pub position: Vec3,
    /// World-space rotation.
    pub rotation: Quat,
}

impl EchoShape {
    /// The shape as the server judged it.
    pub fn posed(&self) -> PosedShape {
        PosedShape {
            kind: self.kind,
            position: self.position,
            rotation: self.rotation,
        }
    }
}

/// The server's view of one of this client's hits: to the shooter, unreliable, dev builds.
#[derive(Clone, Debug, PartialEq, crate::Replicate)]
pub struct LagCompEcho {
    /// The server tick the query ran on.
    pub server_tick: u32,
    /// The tick the shapes were posed at.
    pub rewound_tick: f64,
    /// Ray origin (or overlap center).
    pub origin: Vec3,
    /// Ray direction (zero for overlaps).
    pub direction: Vec3,
    /// The hit entity's shapes at the rewound tick.
    pub shapes: Vec<EchoShape>,
}

impl NetMessage for LagCompEcho {
    const DIRECTION: Direction = Direction::ToOwner;
    const CHANNEL: Channel = Channel::Unreliable;
}
impl ToOwnerMessage for LagCompEcho {}

impl LagCompEcho {
    /// The echo of a logged query.
    pub fn of(record: &QueryRecord) -> Self {
        Self {
            server_tick: record.server_tick,
            rewound_tick: record.rewound_tick,
            origin: Vec3::from_array(record.origin),
            direction: Vec3::from_array(record.direction),
            shapes: record
                .shapes
                .iter()
                .filter_map(|s| {
                    let p = s.posed()?;
                    Some(EchoShape {
                        tag: s.tag.clone(),
                        kind: p.kind,
                        position: p.position,
                        rotation: p.rotation,
                    })
                })
                .collect(),
        }
    }
}

/// Client: the newest echoes, oldest first.
#[derive(Resource, Debug, Default)]
pub struct LagCompEchoLog {
    echoes: VecDeque<LagCompEcho>,
}

impl LagCompEchoLog {
    /// The echoes held, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &LagCompEcho> {
        self.echoes.iter()
    }

    /// How many are held.
    pub fn len(&self) -> usize {
        self.echoes.len()
    }

    /// Whether none are held.
    pub fn is_empty(&self) -> bool {
        self.echoes.is_empty()
    }

    /// Adds one, dropping the oldest past [`ECHO_LOG_LEN`].
    pub fn push(&mut self, echo: LagCompEcho) {
        if self.echoes.len() >= ECHO_LOG_LEN {
            self.echoes.pop_front();
        }
        self.echoes.push_back(echo);
    }
}

/// One echo in `lagcomp.echoes`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EchoReport {
    /// The server tick the query ran on.
    pub server_tick: u32,
    /// The tick the shapes were posed at.
    pub rewound_tick: f64,
    /// Ray origin (or overlap center).
    pub origin: [f32; 3],
    /// Ray direction (zero for overlaps).
    pub direction: [f32; 3],
    /// The hit entity's shapes at the rewound tick.
    pub shapes: Vec<ShapeReport>,
}

impl EchoReport {
    /// The report of an echo.
    pub fn of(echo: &LagCompEcho) -> Self {
        Self {
            server_tick: echo.server_tick,
            rewound_tick: echo.rewound_tick,
            origin: echo.origin.to_array(),
            direction: echo.direction.to_array(),
            shapes: echo
                .shapes
                .iter()
                .map(|s| ShapeReport::of(&s.posed(), &s.tag))
                .collect(),
        }
    }
}

/// Why `lagcomp.echoes` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoQueryError {
    /// The process is a server or offline: `net_not_client`.
    NotClient,
    /// [`LagCompEchoPlugin`] is not added: `lagcomp_not_installed`.
    NotInstalled,
}

impl EchoQueryError {
    /// The protocol error code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotClient => "net_not_client",
            Self::NotInstalled => "lagcomp_not_installed",
        }
    }
}

/// Data of `lagcomp.echoes` (client): the newest `limit` echoes, oldest first.
pub fn echoes_report(world: &World, limit: usize) -> Result<Vec<EchoReport>, EchoQueryError> {
    if world.get_resource::<NetMode>().copied().unwrap_or_default() != NetMode::Client {
        return Err(EchoQueryError::NotClient);
    }
    let log = world
        .get_resource::<LagCompEchoLog>()
        .ok_or(EchoQueryError::NotInstalled)?;
    let skip = log.len().saturating_sub(limit);
    Ok(log.iter().skip(skip).map(EchoReport::of).collect())
}

/// Server, after the record: sends each shooter the echoes of its hits since the last tick.
pub fn lagcomp_echo_send(history: Option<Res<LagCompHistory>>, mut sender: NetSender) {
    let _span = tracing::debug_span!("lagcomp_echo_send").entered();
    let Some(history) = history else {
        return;
    };
    for record in history.take_echoes() {
        let Some([peer, _]) = record.shooter else {
            continue;
        };
        if let Err(e) = sender.send_session_to_peer(PeerId(peer), LagCompEcho::of(&record)) {
            tracing::debug!(target: "regia_net", error = %e, peer, "lag compensation echo not sent");
        }
    }
}

/// Client: moves the tick's echoes into [`LagCompEchoLog`].
pub fn lagcomp_echo_receive(
    messages: Option<Res<NetMessages<LagCompEcho>>>,
    log: Option<ResMut<LagCompEchoLog>>,
) {
    let (Some(messages), Some(mut log)) = (messages, log) else {
        return;
    };
    for m in messages.iter() {
        log.push(m.msg.clone());
    }
}

/// The debug echo on a process: registers [`LagCompEcho`] (so add it on every process, in the same
/// order) and the send and receive systems. The server sends only with
/// `lag_compensation.debug_echo: true`, in dev builds.
#[derive(Debug, Clone, Copy, Default)]
pub struct LagCompEchoPlugin;

impl Plugin for LagCompEchoPlugin {
    fn build(&self, app: &mut App) {
        app.add_net_message::<LagCompEcho>();
        app.init_resource::<LagCompEchoLog>();
        app.add_systems(
            FixedUpdate,
            (
                lagcomp_echo_receive.after(NetSet::Apply).before(NetSet::Assign),
                lagcomp_echo_send
                    .in_set(NetSet::LagCompRecord)
                    .after(super::lagcomp_record),
            ),
        );
    }
}
