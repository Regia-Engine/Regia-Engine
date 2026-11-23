// SPDX-License-Identifier: MPL-2.0
//! The montage replication hook (`docs/specs/34-montage-extensions.md` § 9, ADR-7216).
//!
//! Every runtime change a gameplay system makes to a [`MontagePlayer`] is one small serializable
//! [`MontageCommand`]: the player methods (`play`, `queue`, `stop`, `jump_to_section`, ...) build
//! the command for their call, and [`MontagePlayer::apply_command`] runs a command through the same
//! method, so applying a recorded stream to a fresh player gives the same instance ids, instances
//! and events.
//!
//! A player records nothing until [`MontagePlayer::set_recording`] turns it on; each recorded
//! command carries the player's animator clock (seconds of animation time the evaluate system has
//! advanced it by) at the moment of the call. Through the plugin, [`MontageCommandRecorded`]
//! messages carry the recorded commands out (after the evaluate system), and
//! [`ApplyMontageCommand`] messages carry commands in: each waits in the player's inbox until its
//! clock is reached, then applies before that frame's evaluation, the same point in the frame
//! sequence the recording player saw it.
//!
//! The transport is not here: `regia_animation` has no edge to `regia_net`. The umbrella crate's
//! net wiring (`regia::montage_net`, ADR-9370) drains a recording player's commands on the
//! authority, sends the postcard bytes of [`TimedMontageCommand`]
//! ([`TimedMontageCommand::to_bytes`]) stamped with the tick and a sequence number, and applies
//! them on clients at that tick; [`MontageNetState`] on the player keeps the counts and stamps.
//! Root-motion correction on a client is the networking layer's job.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use crate::montage::SectionNext;
use crate::montage_player::{BranchAction, MontageId, MontagePlayer, PlayOptions};
use crate::warp::WarpTarget;

/// One runtime change to a [`MontagePlayer`], as its method call made it (spec 34 § 9).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MontageCommand {
    /// [`MontagePlayer::play`] (or [`MontagePlayer::queue`] when `queued`) of a library montage
    /// by name. The receiving player must hold a montage of that name.
    Play {
        /// The library montage's name.
        montage: String,
        /// The play options.
        options: PlayOptions,
        /// `queue` instead of `play`.
        queued: bool,
    },
    /// [`MontagePlayer::stop`].
    Stop {
        /// The instance (`None` = every montage).
        id: Option<MontageId>,
        /// Blend-out seconds (`None` = the montage's own).
        blend: Option<f32>,
    },
    /// [`MontagePlayer::cancel_all`].
    CancelAll,
    /// [`MontagePlayer::jump_to_section`].
    JumpToSection {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// The section.
        section: String,
    },
    /// [`MontagePlayer::set_next_section`].
    SetNextSection {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// The section whose link changes.
        from: String,
        /// Its new link.
        next: SectionNext,
    },
    /// [`MontagePlayer::on_branch`].
    OnBranch {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// The branching point's name.
        branch: String,
        /// What happens there.
        action: BranchAction,
    },
    /// [`MontagePlayer::set_rate`].
    SetRate {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// The play rate.
        rate: f32,
    },
    /// [`MontagePlayer::set_reverse`].
    SetReverse {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// Play backward.
        reverse: bool,
    },
    /// [`MontagePlayer::stretch_to`].
    StretchTo {
        /// The instance (`None` = the newest active one).
        id: Option<MontageId>,
        /// Wall seconds to the end.
        seconds: f32,
    },
    /// [`MontagePlayer::set_warp_target`].
    SetWarpTarget {
        /// The warp target's name.
        name: String,
        /// Where it is.
        target: WarpTarget,
    },
    /// [`MontagePlayer::clear_warp_target`].
    ClearWarpTarget {
        /// The warp target's name.
        name: String,
    },
    /// [`MontagePlayer::shift_warp_targets`] (a floating-origin shift).
    ShiftWarpTargets {
        /// The shift.
        shift: Vec3,
    },
}

impl MontageCommand {
    /// Lowercase name (agent protocol, logs).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            MontageCommand::Play { queued: false, .. } => "play",
            MontageCommand::Play { queued: true, .. } => "queue",
            MontageCommand::Stop { .. } => "stop",
            MontageCommand::CancelAll => "cancel_all",
            MontageCommand::JumpToSection { .. } => "jump_to_section",
            MontageCommand::SetNextSection { .. } => "set_next_section",
            MontageCommand::OnBranch { .. } => "on_branch",
            MontageCommand::SetRate { .. } => "set_rate",
            MontageCommand::SetReverse { .. } => "set_reverse",
            MontageCommand::StretchTo { .. } => "stretch_to",
            MontageCommand::SetWarpTarget { .. } => "set_warp_target",
            MontageCommand::ClearWarpTarget { .. } => "clear_warp_target",
            MontageCommand::ShiftWarpTargets { .. } => "shift_warp_targets",
        }
    }
}

/// A command and the player clock it applies at (seconds of animation time the evaluate system
/// had advanced the player by when the call was made).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimedMontageCommand {
    /// Player clock, seconds.
    pub at: f64,
    /// The command.
    pub command: MontageCommand,
}

impl TimedMontageCommand {
    /// The command's wire form: postcard bytes (the network layer sends these, ADR-9370).
    ///
    /// # Errors
    ///
    /// [`CommandBytesError`] when the encoder fails (never for a command a player recorded).
    pub fn to_bytes(&self) -> Result<Vec<u8>, CommandBytesError> {
        postcard::to_allocvec(self).map_err(|e| CommandBytesError(e.to_string()))
    }

    /// Decodes [`TimedMontageCommand::to_bytes`] output; bytes after the command are refused.
    ///
    /// # Errors
    ///
    /// [`CommandBytesError`] for damaged, truncated or overlong bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CommandBytesError> {
        let (command, rest) =
            postcard::take_from_bytes::<Self>(bytes).map_err(|e| CommandBytesError(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CommandBytesError(format!(
                "{} bytes after the command",
                rest.len()
            )));
        }
        Ok(command)
    }
}

/// Command bytes that do not encode or decode (`montage_command_bad_bytes`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("montage_command_bad_bytes: {0}")]
pub struct CommandBytesError(pub String);

impl CommandBytesError {
    /// Stable lowercase code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "montage_command_bad_bytes"
    }
}

/// The network bookkeeping of one player's command stream (ADR-9370), kept on the
/// [`MontagePlayer`] ([`MontagePlayer::net_state`]). The authority stamps every command it sends
/// with the tick it goes out on and a per-player sequence number; a receiver applies a command
/// only when its stamp is newer than the last one it applied, so a duplicated or late message is
/// dropped by its stamp and never applied twice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MontageNetState {
    /// Commands this player sent (authority).
    pub sent: u64,
    /// Commands this player received and applied (receiver).
    pub received: u64,
    /// Commands dropped as duplicates or late (receiver).
    pub dropped: u64,
    /// The sequence number of the next command sent.
    pub next_seq: u32,
    /// The `(tick, seq)` stamp of the last command applied.
    pub last_applied: Option<(u32, u32)>,
}

impl MontageNetState {
    /// Authority: the stamp of the next command, sent on `tick`; counts it as sent.
    pub fn stamp(&mut self, tick: u32) -> (u32, u32) {
        let stamp = (tick, self.next_seq);
        self.next_seq = self.next_seq.wrapping_add(1);
        self.sent += 1;
        stamp
    }

    /// Receiver: whether a command stamped `(tick, seq)` is newer than the last applied one. A
    /// new stamp becomes the last applied and counts as received; an older or equal one counts as
    /// dropped.
    pub fn accept(&mut self, tick: u32, seq: u32) -> bool {
        if self.last_applied.is_some_and(|last| (tick, seq) <= last) {
            self.dropped += 1;
            return false;
        }
        self.last_applied = Some((tick, seq));
        self.received += 1;
        true
    }
}

/// Why [`MontagePlayer::apply_command`] refused a command.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// A `Play` names a montage the player's library lacks (`montage_command_unknown_montage`).
    #[error("montage_command_unknown_montage: no montage named {0:?} in the player's library")]
    UnknownMontage(String),
}

impl CommandError {
    /// Stable lowercase code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CommandError::UnknownMontage(_) => "montage_command_unknown_montage",
        }
    }
}

/// A command a player with recording on made (written after the evaluate system).
#[derive(Message, Clone, Debug, PartialEq)]
pub struct MontageCommandRecorded {
    /// The rig entity.
    pub entity: Entity,
    /// The command.
    pub command: TimedMontageCommand,
}

/// A command for a player (from a replay or a network layer): it waits in the player's inbox
/// until the player's clock reaches `command.at`, then applies before that frame's evaluation.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct ApplyMontageCommand {
    /// The rig entity.
    pub entity: Entity,
    /// The command.
    pub command: TimedMontageCommand,
}

/// `Update`, before the evaluate system: move [`ApplyMontageCommand`] messages into their
/// players' inboxes and apply every command whose time has come. A refused command is logged
/// (`montage_command_unknown_montage`) and dropped.
pub fn apply_montage_commands(
    mut incoming: MessageReader<ApplyMontageCommand>,
    mut players: Query<(Entity, &mut MontagePlayer)>,
) {
    for message in incoming.read() {
        match players.get_mut(message.entity) {
            Ok((_, mut player)) => player.receive_command(message.command.clone()),
            Err(_) => tracing::warn!(
                target: "regia_animation",
                code = "montage_command_no_player",
                "montage command {} for {:?}, which has no montage player",
                message.command.command.as_str(),
                message.entity
            ),
        }
    }
    for (entity, mut player) in &mut players {
        if !player.has_inbox() {
            continue;
        }
        for error in player.apply_due_commands() {
            tracing::warn!(target: "regia_animation", code = error.code(), "{entity:?}: {error}");
        }
    }
}

/// `Update`, after the evaluate system: write the commands recording players made this frame as
/// [`MontageCommandRecorded`] messages.
pub fn publish_montage_commands(
    mut out: MessageWriter<MontageCommandRecorded>,
    mut players: Query<(Entity, &mut MontagePlayer)>,
) {
    for (entity, mut player) in &mut players {
        if !player.has_recorded_commands() {
            continue;
        }
        for command in player.take_commands() {
            out.write(MontageCommandRecorded { entity, command });
        }
    }
}
