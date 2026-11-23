// SPDX-License-Identifier: MPL-2.0
//! Montage commands over the network (spec 34 § 9, ADR-7216 (1), ADR-9370).
//!
//! `regia_animation` records every playback change of a [`MontagePlayer`] as a serializable
//! command and has no edge to `regia_net`; [`MontageNetPlugin`] is the transport. On the
//! authority (a dedicated or listen server) it drains each recording player of a replicated rig
//! and broadcasts the postcard bytes of every [`TimedMontageCommand`] as a reliable, ordered
//! [`MontageCommandMsg`] on that rig, stamped with the tick it goes out on and a per-player
//! sequence number. On a client, a `FixedUpdate` system applies each message to the rig's player
//! in the tick the net layer delivers it, which is the stamped tick (a message is delivered once
//! the server tick it was sent in is applied, spec 70 § 6.5), so the montage starts on the same
//! tick and plays the same sections at the same positions as on the server. A message whose
//! stamp is not newer than the last one applied (a duplicate, a late resend) is dropped and
//! counted, never applied twice
//! (`regia_animation::montage_command::MontageNetState`, on [`MontagePlayer::net_state`]).
//!
//! Timing: commands a gameplay system makes in `FixedUpdate` (before `NetSet::Predict`) go out in
//! that tick's frame and play on clients in the same tick. Commands made in `Update` go out with
//! the next tick and play one tick later on clients than on the server.
//!
//! `Play` carries the montage name: the client's player must hold that montage in its library
//! (`montage_command_unknown_montage` otherwise). Root-motion correction stays with the rig's own
//! replication. Opt-in: the plugin registers a net message, which is part of the session's
//! protocol, so add it on every process of a session, after `NetPlugin` and the
//! `AnimationPlugin`, in the same order relative to other net messages.

use regia_animation::montage_command::{
    MontageCommandRecorded, TimedMontageCommand, publish_montage_commands,
};
use regia_animation::montage_player::MontagePlayer;
use regia_animation::systems::evaluate_animators;
use regia_app::prelude::*;
use regia_net::model::NetMode;
use regia_net::runtime::NetNode;
use regia_net::{NetId, NetMessage, NetMessageAppExt, NetMessages, NetSender, NetSet};

/// Largest encoded command, bytes (a `Play` with long names and options is under 1 KiB).
pub const MAX_COMMAND_BYTES: usize = 4096;

/// Server to every client the rig is relevant to: one montage command for the target rig's
/// player (reliable, ordered).
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "regia.montage_command")]
pub struct MontageCommandMsg {
    /// The server tick the command was sent on.
    pub tick: u32,
    /// The player's sequence number of the command.
    pub seq: u32,
    /// Postcard bytes of the [`TimedMontageCommand`] ([`TimedMontageCommand::to_bytes`]).
    #[replicate(max_len = 4096)]
    pub bytes: Vec<u8>,
}

/// Carries montage commands from the authority to clients (see the module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct MontageNetPlugin;

impl Plugin for MontageNetPlugin {
    fn build(&self, app: &mut App) {
        // Registered by the `AnimationPlugin` too; a second registration is a no-op.
        app.add_message::<MontageCommandRecorded>();
        app.add_net_message::<MontageCommandMsg>()
            .add_systems(
                FixedUpdate,
                (receive_montage_commands, send_fixed_montage_commands)
                    .chain()
                    .after(NetSet::Gameplay)
                    .before(NetSet::Predict)
                    .run_if(resource_exists::<NetNode>),
            )
            .add_systems(
                Update,
                send_update_montage_commands
                    .after(evaluate_animators)
                    .before(publish_montage_commands)
                    .run_if(resource_exists::<NetNode>),
            );
    }
}

fn is_server(mode: Option<&NetMode>) -> bool {
    mode.copied().unwrap_or_default().is_server()
}

/// Drains every recording, replicated player and broadcasts its commands stamped with `tick`.
/// The drained commands are also written as [`MontageCommandRecorded`] for local readers.
fn send_recorded(
    sender: &mut NetSender,
    players: &mut Query<(Entity, &mut MontagePlayer), With<NetId>>,
    recorded: &mut MessageWriter<MontageCommandRecorded>,
) {
    let tick = sender.tick().0;
    for (entity, mut player) in players.iter_mut() {
        if !player.has_recorded_commands() {
            continue;
        }
        for command in player.take_commands() {
            match command.to_bytes() {
                Ok(bytes) if bytes.len() <= MAX_COMMAND_BYTES => {
                    let (tick, seq) = player.net_state_mut().stamp(tick);
                    if let Err(error) = sender.broadcast(entity, MontageCommandMsg { tick, seq, bytes }) {
                        tracing::warn!(
                            target: "regia::montage_net",
                            code = "montage_command_not_sent",
                            "{entity:?}: montage command {} not sent: {error}",
                            command.command.as_str()
                        );
                    }
                }
                Ok(bytes) => tracing::warn!(
                    target: "regia::montage_net",
                    code = "montage_command_too_large",
                    "{entity:?}: montage command {} is {} bytes (limit {MAX_COMMAND_BYTES})",
                    command.command.as_str(),
                    bytes.len()
                ),
                Err(error) => tracing::warn!(
                    target: "regia::montage_net",
                    code = error.code(),
                    "{entity:?}: {error}"
                ),
            }
            recorded.write(MontageCommandRecorded { entity, command });
        }
    }
}

/// `FixedUpdate`, after gameplay (authority): commands made this tick go out in this tick.
fn send_fixed_montage_commands(
    mode: Option<Res<NetMode>>,
    mut sender: NetSender,
    mut players: Query<(Entity, &mut MontagePlayer), With<NetId>>,
    mut recorded: MessageWriter<MontageCommandRecorded>,
) {
    if is_server(mode.as_deref()) {
        send_recorded(&mut sender, &mut players, &mut recorded);
    }
}

/// `Update`, after the evaluate system (authority): commands made outside `FixedUpdate` go out
/// with the next tick.
fn send_update_montage_commands(
    mode: Option<Res<NetMode>>,
    mut sender: NetSender,
    mut players: Query<(Entity, &mut MontagePlayer), With<NetId>>,
    mut recorded: MessageWriter<MontageCommandRecorded>,
) {
    if is_server(mode.as_deref()) {
        send_recorded(&mut sender, &mut players, &mut recorded);
    }
}

/// `FixedUpdate`, after gameplay (client): applies this tick's montage commands to their rigs'
/// players, dropping any whose stamp is not newer than the last one applied.
pub fn receive_montage_commands(
    mode: Option<Res<NetMode>>,
    messages: Option<Res<NetMessages<MontageCommandMsg>>>,
    mut players: Query<&mut MontagePlayer>,
) {
    // The authority made these commands; its own copy of the broadcast is not applied again.
    if is_server(mode.as_deref()) {
        return;
    }
    let Some(messages) = messages else {
        return;
    };
    for received in messages.iter() {
        let Some(entity) = received.target else {
            continue;
        };
        let Ok(mut player) = players.get_mut(entity) else {
            tracing::warn!(
                target: "regia::montage_net",
                code = "montage_command_no_player",
                "montage command for {entity:?}, which has no montage player"
            );
            continue;
        };
        let msg = &received.msg;
        let command = match TimedMontageCommand::from_bytes(&msg.bytes) {
            Ok(command) => command,
            Err(error) => {
                tracing::warn!(target: "regia::montage_net", code = error.code(), "{entity:?}: {error}");
                continue;
            }
        };
        if !player.net_state_mut().accept(msg.tick, msg.seq) {
            tracing::debug!(
                target: "regia::montage_net",
                code = "montage_command_dropped",
                tick = msg.tick,
                seq = msg.seq,
                "{entity:?}: montage command {} already applied or late",
                command.command.as_str()
            );
            continue;
        }
        if let Err(error) = player.apply_command(&command.command) {
            tracing::warn!(target: "regia::montage_net", code = error.code(), "{entity:?}: {error}");
        }
    }
}
