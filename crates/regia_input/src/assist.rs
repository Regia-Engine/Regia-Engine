// SPDX-License-Identifier: MPL-2.0
//! Per-action input assists for players (ADR-2646, `book/agents/input-assists.md`): press-to-toggle
//! instead of hold, a minimum hold time before a press counts, and auto-repeat while held.
//!
//! These are the player's choices, not the designer's: the designer's [`crate::Trigger`]s stay as
//! authored and run on the assisted value. [`evaluate_actions`](crate::evaluate_actions) applies an
//! action's [`ActionAssist`] to the combined value of its bindings (after injection, so virtual
//! controls and the agent's `input.inject` get it too):
//!
//! 1. **Minimum hold** (`min_hold_secs`): a press counts only while it is still down that long after
//!    the frame it started on (the start frame is time 0, each later frame adds its delta). A press
//!    released earlier never reaches the action. This filters out short accidental presses (a
//!    tremor, a bumped key).
//! 2. **Mode**: [`PressMode::Hold`] passes the counted value through. [`PressMode::Toggle`] flips a
//!    latch on each counted press; while latched the action reads the value it had on the press that
//!    latched it (1 for a button) whether or not anything is held, and the next counted press
//!    unlatches it. Replaces holding a button down for a long time.
//! 3. **Repeat** (`repeat`): while the assisted value stays non-zero the action reports
//!    [`crate::ActionPhase::Triggered`] (so `just_pressed` is true) once after `delay_secs` and then
//!    every `interval_secs`. Holding replaces mashing. A frame longer than an interval fires once,
//!    never a burst.
//! 4. **Cool-down** (`cooldown_steps`): after a press is accepted, a later press that *starts*
//!    before this many evaluation steps have passed is ignored for its whole hold. `0` accepts
//!    every press. The count is steps, not wall time, so a recording replays the same way.
//!
//! [`InputAssists::actions`] is the default for every player. [`InputAssists::players`] and
//! [`InputAssists::gamepads`] overlay it for one action: the player's own entry wins, else the
//! entry for the gamepad assigned to that player, else the default. A player with no entry uses
//! the default set unchanged.
//!
//! The default assist (hold, no minimum, no repeat, no cool-down) changes nothing: the action reads
//! exactly as without one. Everything is driven by evaluation deltas and evaluation counts, never a clock read,
//! so a recorded event stream replays to the same phases.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

/// How a counted press drives the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PressMode {
    /// The action is active while the input is held (the authored behavior).
    #[default]
    Hold,
    /// Each press flips the action on or off; nothing needs to stay held.
    Toggle,
}

impl PressMode {
    /// `hold` or `toggle`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Toggle => "toggle",
        }
    }

    /// Parses [`PressMode::as_str`] (ignoring ASCII case).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "hold" => Some(Self::Hold),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
    }
}

/// Auto-repeat while held: one extra fire after `delay_secs`, then one every `interval_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RepeatRate {
    /// Seconds from the press to the first repeat ([`ActionAssist::MAX_SECS`] at most).
    pub delay_secs: f32,
    /// Seconds between repeats ([`RepeatRate::MIN_INTERVAL_SECS`] to [`ActionAssist::MAX_SECS`]).
    pub interval_secs: f32,
}

impl RepeatRate {
    /// The shortest repeat interval accepted: 20 repeats a second.
    pub const MIN_INTERVAL_SECS: f32 = 0.05;

    /// A common key-repeat feel: first repeat after half a second, then ten a second.
    pub const DEFAULT: Self = Self {
        delay_secs: 0.5,
        interval_secs: 0.1,
    };

    /// The rate with both fields in range (NaN takes [`RepeatRate::DEFAULT`]'s value).
    #[must_use]
    pub fn clamped(self) -> Self {
        let clamp = |v: f32, lo: f32, fallback: f32| {
            if v.is_nan() {
                fallback
            } else {
                v.clamp(lo, ActionAssist::MAX_SECS)
            }
        };
        Self {
            delay_secs: clamp(self.delay_secs, 0.0, Self::DEFAULT.delay_secs),
            interval_secs: clamp(
                self.interval_secs,
                Self::MIN_INTERVAL_SECS,
                Self::DEFAULT.interval_secs,
            ),
        }
    }
}

impl Default for RepeatRate {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One action's assists. The default changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ActionAssist {
    /// Hold (as authored) or toggle.
    #[serde(default)]
    pub mode: PressMode,
    /// Seconds a press must stay down before it counts (`0` = at once; at most
    /// [`ActionAssist::MAX_SECS`]).
    #[serde(default)]
    pub min_hold_secs: f32,
    /// Auto-repeat while the action is active; `None` = off.
    #[serde(default)]
    pub repeat: Option<RepeatRate>,
    /// Evaluation steps that must pass after an accepted press before the next press is accepted.
    /// `0` (the default, and older saves) accepts every press. At most [`Self::MAX_COOLDOWN_STEPS`].
    #[serde(default)]
    pub cooldown_steps: u32,
}

impl ActionAssist {
    /// The longest minimum hold, repeat delay or repeat interval accepted, seconds.
    pub const MAX_SECS: f32 = 5.0;
    /// The longest cool-down accepted, in evaluation steps (ten seconds at 60 Hz).
    pub const MAX_COOLDOWN_STEPS: u32 = 600;

    /// Whether this assist leaves the action exactly as authored.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.mode == PressMode::Hold
            && self.min_hold_secs <= 0.0
            && self.repeat.is_none()
            && self.cooldown_steps == 0
    }

    /// The assist with every field in range (a NaN minimum hold reads as 0).
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            mode: self.mode,
            min_hold_secs: if self.min_hold_secs.is_nan() {
                0.0
            } else {
                self.min_hold_secs.clamp(0.0, Self::MAX_SECS)
            },
            repeat: self.repeat.map(RepeatRate::clamped),
            cooldown_steps: self.cooldown_steps.min(Self::MAX_COOLDOWN_STEPS),
        }
    }
}

/// The player's input assists by action name (`book/agents/input-assists.md`).
/// [`Self::actions`] is the default for every player; [`Self::players`] and [`Self::gamepads`]
/// overlay it. Empty by default. The framework copies
/// `GameSettings.accessibility.input_assists` (and the per-player maps) here when the settings change.
///
/// # Example
///
/// ```
/// use regia_input::{ActionAssist, InputAssists, PressMode};
///
/// let mut assists = InputAssists::default();
/// assists.set("Crouch", ActionAssist { mode: PressMode::Toggle, ..Default::default() });
/// assert_eq!(assists.get("Crouch").map(|a| a.mode), Some(PressMode::Toggle));
/// ```
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct InputAssists {
    /// The default assists, by action name, for every player without their own entry. Neutral
    /// entries are not stored.
    pub actions: BTreeMap<String, ActionAssist>,
    /// Assists that overlay [`Self::actions`] for one local player index.
    pub players: BTreeMap<u8, BTreeMap<String, ActionAssist>>,
    /// Assists that overlay [`Self::actions`] for one gamepad device id, used when that pad is
    /// the player's assignment and the player has no entry of their own for the action.
    pub gamepads: BTreeMap<u64, BTreeMap<String, ActionAssist>>,
}

impl InputAssists {
    /// The default assist of `action`, when it has one.
    #[must_use]
    pub fn get(&self, action: &str) -> Option<&ActionAssist> {
        self.actions.get(action)
    }

    /// The assist `player` uses for `action`: the player's own entry, else `gamepad`'s entry,
    /// else the default set.
    #[must_use]
    pub fn get_for(&self, player: u8, gamepad: Option<u64>, action: &str) -> Option<&ActionAssist> {
        if let Some(found) = self.players.get(&player).and_then(|map| map.get(action)) {
            return Some(found);
        }
        if let Some(id) = gamepad
            && let Some(found) = self.gamepads.get(&id).and_then(|map| map.get(action))
        {
            return Some(found);
        }
        self.actions.get(action)
    }

    /// Sets the default assist of `action` (clamped); a neutral one removes the entry.
    pub fn set(&mut self, action: impl Into<String>, assist: ActionAssist) {
        Self::set_in(&mut self.actions, action.into(), assist);
    }

    /// Sets `player`'s overlay for `action` (clamped). A neutral assist removes the entry, and an
    /// empty player map is dropped.
    pub fn set_player(&mut self, player: u8, action: impl Into<String>, assist: ActionAssist) {
        let map = self.players.entry(player).or_default();
        Self::set_in(map, action.into(), assist);
        if map.is_empty() {
            self.players.remove(&player);
        }
    }

    /// Sets the overlay for gamepad `device` (clamped). A neutral assist removes the entry, and an
    /// empty pad map is dropped.
    pub fn set_gamepad(&mut self, device: u64, action: impl Into<String>, assist: ActionAssist) {
        let map = self.gamepads.entry(device).or_default();
        Self::set_in(map, action.into(), assist);
        if map.is_empty() {
            self.gamepads.remove(&device);
        }
    }

    /// Replaces the default assists with `actions` (clamped, neutral entries dropped).
    pub fn replace(&mut self, actions: &BTreeMap<String, ActionAssist>) {
        self.actions.clear();
        for (name, assist) in actions {
            self.set(name.clone(), *assist);
        }
    }

    /// Replaces every per-player overlay (clamped, neutral entries and empty maps dropped).
    pub fn replace_players(&mut self, players: &BTreeMap<u8, BTreeMap<String, ActionAssist>>) {
        self.players.clear();
        for (player, actions) in players {
            for (name, assist) in actions {
                self.set_player(*player, name.clone(), *assist);
            }
        }
    }

    /// Replaces every per-gamepad overlay (clamped, neutral entries and empty maps dropped).
    pub fn replace_gamepads(&mut self, gamepads: &BTreeMap<u64, BTreeMap<String, ActionAssist>>) {
        self.gamepads.clear();
        for (device, actions) in gamepads {
            for (name, assist) in actions {
                self.set_gamepad(*device, name.clone(), *assist);
            }
        }
    }

    /// Removes one player's overlay. The player then uses the default set.
    pub fn clear_player(&mut self, player: u8) {
        self.players.remove(&player);
    }

    /// Removes one gamepad's overlay.
    pub fn clear_gamepad(&mut self, device: u64) {
        self.gamepads.remove(&device);
    }

    fn set_in(map: &mut BTreeMap<String, ActionAssist>, action: String, assist: ActionAssist) {
        let assist = assist.clamped();
        if assist.is_neutral() {
            map.remove(&action);
        } else {
            map.insert(action, assist);
        }
    }
}

/// The per-action bookkeeping the assists carry between frames (inside
/// [`crate::ActionState`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct AssistState {
    /// The input was down last frame.
    physical: bool,
    /// Seconds the current press has been down (0 on its start frame).
    held_secs: f32,
    /// The current press passed the minimum hold.
    counted: bool,
    /// Toggle mode: the action is latched on.
    latched: bool,
    /// The value the latch holds.
    latched_value: Vec3,
    /// Seconds to the next repeat.
    repeat_timer: f32,
    /// An accepted press has happened, so the cool-down is running.
    had_accept: bool,
    /// Evaluation steps since the last accepted press.
    steps_since_accept: u32,
    /// This physical press started inside the cool-down and is ignored until it is released.
    cooled: bool,
}

impl AssistState {
    /// Whether a toggle is latched on.
    pub(crate) fn latched(&self) -> bool {
        self.latched
    }

    /// Drops a latched toggle (the action reads its inputs again from the next frame).
    pub(crate) fn release(&mut self) {
        self.latched = false;
        self.latched_value = Vec3::ZERO;
    }
}

/// Runs the minimum hold and the mode on this frame's combined input `physical`; returns the value
/// the action's triggers see. `None` (no assist) passes `physical` through and forgets any latch.
pub(crate) fn assisted_value(
    assist: Option<&ActionAssist>,
    state: &mut AssistState,
    physical: Vec3,
    dt: f32,
) -> Vec3 {
    let down = physical != Vec3::ZERO;
    let rising = down && !state.physical;
    // Cool-down counts simulation evaluations. A zero delta is the no-step frame fallback
    // (or a pause): assist timing stays frozen, the same contract as hold and tap.
    if state.had_accept && dt.is_finite() && dt > 0.0 {
        state.steps_since_accept = state.steps_since_accept.saturating_add(1);
    }
    let cooldown = assist.map(|a| a.cooldown_steps).unwrap_or(0);
    if cooldown == 0 {
        state.cooled = false;
    } else if rising {
        state.cooled = state.had_accept && state.steps_since_accept < cooldown;
    }
    if !down {
        state.cooled = false;
    }
    state.held_secs = match (down, state.physical) {
        (true, true) => state.held_secs + dt.max(0.0),
        _ => 0.0,
    };
    state.physical = down;
    let Some(assist) = assist else {
        state.counted = down;
        state.release();
        state.cooled = false;
        state.had_accept = false;
        state.steps_since_accept = 0;
        return physical;
    };
    let was_counted = state.counted;
    state.counted = down && !state.cooled && (was_counted || state.held_secs >= assist.min_hold_secs);
    if state.counted && !was_counted {
        state.had_accept = true;
        state.steps_since_accept = 0;
    }
    match assist.mode {
        PressMode::Hold => {
            state.release();
            if state.counted { physical } else { Vec3::ZERO }
        }
        PressMode::Toggle => {
            if state.counted && !was_counted {
                if state.latched {
                    state.release();
                } else {
                    state.latched = true;
                    state.latched_value = physical;
                }
            }
            if state.latched {
                state.latched_value
            } else {
                Vec3::ZERO
            }
        }
    }
}

/// Repeat: given whether the assisted value is non-zero this frame and was last frame, returns
/// whether a repeat fires this frame.
pub(crate) fn repeat_fires(
    repeat: Option<&RepeatRate>,
    state: &mut AssistState,
    active: bool,
    was_active: bool,
    dt: f32,
) -> bool {
    let Some(repeat) = repeat else {
        state.repeat_timer = 0.0;
        return false;
    };
    if !active {
        state.repeat_timer = 0.0;
        return false;
    }
    if !was_active {
        state.repeat_timer = repeat.delay_secs.max(0.0);
        return false;
    }
    state.repeat_timer -= dt.max(0.0);
    if state.repeat_timer > 0.0 {
        return false;
    }
    // Carry the small lateness for an even rate, but never more than one interval: a long frame
    // fires once and waits a full interval again (the `Pulse` rule).
    let interval = repeat.interval_secs.max(RepeatRate::MIN_INTERVAL_SECS);
    let carried = state.repeat_timer + interval;
    state.repeat_timer = if carried > 0.0 { carried } else { interval };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames of 1/8 s: exact in binary, so hold thresholds land on known frames.
    const DT: f32 = 0.125;
    const ON: Vec3 = Vec3::X;

    /// Runs `stream` (true = held) through the assist; returns the value seen each frame.
    fn run(assist: Option<&ActionAssist>, stream: &[bool]) -> Vec<bool> {
        let mut state = AssistState::default();
        stream
            .iter()
            .map(|&down| {
                let physical = if down { ON } else { Vec3::ZERO };
                assisted_value(assist, &mut state, physical, DT) != Vec3::ZERO
            })
            .collect()
    }

    fn bits(text: &str) -> Vec<bool> {
        text.chars().map(|c| c == '1').collect()
    }

    #[test]
    fn no_assist_and_the_default_assist_pass_every_stream_through() {
        let stream = bits("0110111001011111000101");
        assert_eq!(run(None, &stream), stream);
        let neutral = ActionAssist::default();
        assert!(neutral.is_neutral());
        assert_eq!(run(Some(&neutral), &stream), stream);
        // Analog values pass through unchanged too.
        let mut state = AssistState::default();
        for v in [0.0, 0.3, 0.7, -0.2, 0.0] {
            let input = Vec3::new(v, v * 0.5, 0.0);
            assert_eq!(assisted_value(Some(&neutral), &mut state, input, DT), input);
        }
    }

    #[test]
    fn toggle_latches_on_one_press_and_off_on_the_next() {
        let toggle = ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        };
        // press, release, idle, press (unlatch) held, release, press again.
        let stream = bits("1100011100110");
        let seen = run(Some(&toggle), &stream);
        assert_eq!(seen, bits("1111100000111"));
    }

    #[test]
    fn a_toggle_latch_holds_an_axis_value_from_the_press() {
        let toggle = ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        };
        let mut state = AssistState::default();
        let push = Vec3::new(0.6, 0.0, 0.0);
        assert_eq!(assisted_value(Some(&toggle), &mut state, push, DT), push);
        // Released, then pushed harder: still the latched value until the next press.
        assert_eq!(assisted_value(Some(&toggle), &mut state, Vec3::ZERO, DT), push);
        assert_eq!(assisted_value(Some(&toggle), &mut state, Vec3::ZERO, DT), push);
        assert_eq!(assisted_value(Some(&toggle), &mut state, Vec3::X, DT), Vec3::ZERO);
        assert!(!state.latched());
    }

    #[test]
    fn minimum_hold_drops_short_presses_and_delays_long_ones() {
        // 0.25 s = two frames after the start frame.
        let slow = ActionAssist {
            min_hold_secs: 0.25,
            ..Default::default()
        };
        // A 1-frame and a 2-frame press never count; a 5-frame press counts from its third frame.
        let stream = bits("10110011111000");
        assert_eq!(run(Some(&slow), &stream), bits("00000000111000"));
    }

    #[test]
    fn minimum_hold_and_toggle_combine() {
        let assist = ActionAssist {
            mode: PressMode::Toggle,
            min_hold_secs: 0.125,
            repeat: None,
            cooldown_steps: 0,
        };
        // A bump (one frame) does nothing; a two-frame press latches on its second frame; another
        // bump is ignored; a two-frame press unlatches on its second frame.
        let stream = bits("1001100100110");
        assert_eq!(run(Some(&assist), &stream), bits("0000111111100"));
    }

    #[test]
    fn removing_the_assist_forgets_a_latch() {
        let toggle = ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        };
        let mut state = AssistState::default();
        assert_eq!(assisted_value(Some(&toggle), &mut state, ON, DT), ON);
        assert_eq!(assisted_value(Some(&toggle), &mut state, Vec3::ZERO, DT), ON);
        assert_eq!(assisted_value(None, &mut state, Vec3::ZERO, DT), Vec3::ZERO);
        assert!(!state.latched());
    }

    #[test]
    fn repeat_fires_after_the_delay_then_every_interval() {
        let rate = RepeatRate {
            delay_secs: 0.25,
            interval_secs: 0.125,
        };
        let mut state = AssistState::default();
        let mut fired = Vec::new();
        let mut was = false;
        for down in bits("111111100111") {
            fired.push(repeat_fires(Some(&rate), &mut state, down, was, DT));
            was = down;
        }
        // Press frame 0: no repeat (the press itself is the fire); frames 2..6 repeat after the
        // 0.25 s delay at 0.125 s; release resets; the second press starts the delay again.
        assert_eq!(
            fired,
            vec![
                false, false, true, true, true, true, true, false, false, false, false, true
            ]
        );
    }

    #[test]
    fn a_long_frame_fires_one_repeat_not_a_burst() {
        let rate = RepeatRate {
            delay_secs: 0.1,
            interval_secs: 0.1,
        };
        let mut state = AssistState::default();
        assert!(!repeat_fires(Some(&rate), &mut state, true, false, 0.016));
        assert!(
            repeat_fires(Some(&rate), &mut state, true, true, 1.0),
            "one fire for a 1 s hitch"
        );
        assert!(!repeat_fires(Some(&rate), &mut state, true, true, 0.016));
        assert!(!repeat_fires(Some(&rate), &mut state, true, true, 0.016));
    }

    #[test]
    fn clamping_keeps_fields_in_range() {
        let wild = ActionAssist {
            mode: PressMode::Hold,
            min_hold_secs: f32::NAN,
            repeat: Some(RepeatRate {
                delay_secs: -3.0,
                interval_secs: 0.0,
            }),
            cooldown_steps: 10_000,
        }
        .clamped();
        assert_eq!(wild.min_hold_secs, 0.0);
        let repeat = wild.repeat.expect("kept");
        assert_eq!(repeat.delay_secs, 0.0);
        assert_eq!(repeat.interval_secs, RepeatRate::MIN_INTERVAL_SECS);
        let long = ActionAssist {
            min_hold_secs: 60.0,
            ..Default::default()
        }
        .clamped();
        assert_eq!(long.min_hold_secs, ActionAssist::MAX_SECS);
        assert_eq!(wild.cooldown_steps, ActionAssist::MAX_COOLDOWN_STEPS);
    }

    #[test]
    fn neutral_assists_are_not_stored() {
        let mut assists = InputAssists::default();
        assists.set(
            "Jump",
            ActionAssist {
                min_hold_secs: 0.2,
                ..Default::default()
            },
        );
        assert!(assists.get("Jump").is_some());
        assists.set("Jump", ActionAssist::default());
        assert!(assists.actions.is_empty());
    }

    #[test]
    fn the_settings_shape_round_trips_and_fills_missing_fields() {
        let assist = ActionAssist {
            mode: PressMode::Toggle,
            min_hold_secs: 0.3,
            repeat: Some(RepeatRate::DEFAULT),
            cooldown_steps: 4,
        };
        let text = serde_json::to_string(&assist).expect("serializes");
        let back: ActionAssist = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, assist);
        let partial: ActionAssist = serde_json::from_str(r#"{"mode":"toggle"}"#).expect("parses");
        assert_eq!(partial.mode, PressMode::Toggle);
        assert_eq!(partial.min_hold_secs, 0.0);
        assert_eq!(partial.cooldown_steps, 0);
        assert_eq!(PressMode::parse("TOGGLE"), Some(PressMode::Toggle));
        assert_eq!(PressMode::parse("tap"), None);
    }

    #[test]
    fn cooldown_steps_drop_a_press_that_starts_inside_the_gap() {
        let assist = ActionAssist {
            cooldown_steps: 4,
            ..Default::default()
        };
        // Single-frame presses. The accept frame is step 0; a press is accepted again once
        // four evaluation steps have passed (the fourth frame after the accept).
        let stream = bits("101010101");
        assert_eq!(run(Some(&assist), &stream), bits("100010001"));
        // A hold that is already accepted is not cut short. A new press that starts inside the
        // gap is ignored for its whole hold, even after the gap elapses.
        let held = bits("11011");
        assert_eq!(run(Some(&assist), &held), bits("11000"));
        // Zero is the authored behavior.
        assert_eq!(run(Some(&ActionAssist::default()), &stream), stream);
    }

    /// Zero-delta evaluations (the no-step frame fallback, a pause) must not burn cool-down
    /// steps. Those steps are simulation evaluations, and the fallback's contract is that
    /// assist timing stays frozen.
    #[test]
    fn audfix_cooldown_does_not_advance_on_zero_delta() {
        let assist = ActionAssist {
            cooldown_steps: 4,
            ..Default::default()
        };
        let mut state = AssistState::default();
        assert_ne!(assisted_value(Some(&assist), &mut state, ON, DT), Vec3::ZERO);
        assert_eq!(
            assisted_value(Some(&assist), &mut state, Vec3::ZERO, DT),
            Vec3::ZERO
        );
        for _ in 0..10 {
            assert_eq!(
                assisted_value(Some(&assist), &mut state, Vec3::ZERO, 0.0),
                Vec3::ZERO
            );
        }
        assert_eq!(
            assisted_value(Some(&assist), &mut state, ON, DT),
            Vec3::ZERO,
            "ten zero-delta frames must leave the 4-step cool-down in force"
        );
    }

    #[test]
    fn player_and_gamepad_maps_overlay_the_default() {
        let toggle = ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        };
        let slow = ActionAssist {
            min_hold_secs: 0.2,
            ..Default::default()
        };
        let mut assists = InputAssists::default();
        assists.set("Crouch", slow);
        assists.set_player(1, "Crouch", toggle);
        assists.set_gamepad(
            9,
            "Crouch",
            ActionAssist {
                cooldown_steps: 3,
                ..Default::default()
            },
        );
        assert_eq!(assists.get_for(0, None, "Crouch"), Some(&slow));
        assert_eq!(
            assists.get_for(1, Some(9), "Crouch").map(|a| a.mode),
            Some(PressMode::Toggle)
        );
        assert_eq!(
            assists.get_for(2, Some(9), "Crouch").map(|a| a.cooldown_steps),
            Some(3)
        );
        assert_eq!(assists.get_for(2, Some(4), "Crouch"), Some(&slow));
        assists.set_player(1, "Crouch", ActionAssist::default());
        assert!(assists.players.is_empty(), "a neutral overlay is dropped");
        assert_eq!(assists.get_for(1, None, "Crouch"), Some(&slow));
    }
}
