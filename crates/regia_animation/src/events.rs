// SPDX-License-Identifier: MPL-2.0
//! Animation ECS messages: notifies and state events.
//!
//! Specification: `docs/specs/17-animation.md` § 3. A clip notify is an [`AnimNotify`] message.
//! A state enter or exit is an [`AnimStateEvent`] message. Neither is a `ScriptEvent`.
//! `ScriptEvent` (`regia_framework` scripts) is collision and trigger volumes only. Footsteps
//! read [`AnimNotify`] directly.

use regia_ecs::prelude::*;

/// An animation notify fired while an animator played across a notify key.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct AnimNotify {
    /// The rig entity whose animator fired the notify.
    pub entity: Entity,
    /// Notify name (for example `"Footstep_L"`).
    pub name: String,
    /// Optional RON payload authored with the notify.
    pub payload: Option<String>,
}

/// A state-machine state change: fires on enter (including the initial state, `from: None`)
/// and on the frame a transition is decided (`from: Some(..)`); a completed transition emits
/// a second enter event with `from: None`.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct AnimStateEvent {
    /// The rig entity whose animator changed state.
    pub entity: Entity,
    /// State exited (`None` for enter-without-exit events).
    pub from: Option<String>,
    /// State entered.
    pub to: String,
}
