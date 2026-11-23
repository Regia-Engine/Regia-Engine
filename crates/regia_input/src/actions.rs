// SPDX-License-Identifier: MPL-2.0
//! The evaluated action state ([`Actions`]) and the `PreUpdate` system that produces it
//! (`docs/specs/11-platform-and-input.md` § 7).
//!
//! Every fixed step, [`evaluate_actions`] walks each player's enabled contexts (highest priority
//! first), evaluates every [`Binding`](crate::Binding) against the raw devices, applies
//! modifiers, hides inputs consumed by higher contexts, merges injected values
//! ([`crate::InputInjection`]) and finally runs each binding's [`Trigger`](crate::Trigger)s
//! against that binding's own value (an empty trigger list is `Down`) to produce one
//! [`ActionState`] per declared action. The evaluation runs in `FixedUpdate`, once
//! per fixed step with that step's delta, so triggers and timers advance in simulation time and
//! an edge is true for exactly one step; on a frame whose fixed loop runs no step,
//! [`evaluate_actions_unless_stepped`] runs the same pass in `PreUpdate` with a zero delta so
//! frame-quantized consumers (menus, one-frame injections) keep working — without consuming the
//! edge or advancing any step-carried state, which only the fixed steps may do.
//!
//! Actions read with unknown names return false / zero instead of panicking.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;

use crate::config::{
    ActionKind, BindingKey, DeviceClass, InputConfig, InputSource, ModifierFrame, apply_modifiers,
};
use crate::device::{Devices, Keyboard, Mouse};
use crate::gamepad::Gamepads;

/// The most distinct undeclared-binding names remembered for one-time warnings.
const MAX_WARNED_BINDINGS: usize = 256;

/// The phase of an action this frame (`docs/specs/11` § 7). Phases derive from actuation
/// (non-zero value) and the action's triggers; see [`crate::Trigger`] for the per-trigger
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActionPhase {
    /// Actuation began this frame.
    Started,
    /// Actuated and already actuated last frame.
    Ongoing,
    /// A trigger fired this frame (an edge, a hold threshold reached, a pulse tick...).
    Triggered,
    /// Actuation ended this frame, after the trigger work completed.
    Completed,
    /// Actuation ended this frame, without the trigger completing (a blown tap window, a hold
    /// released early).
    Canceled,
    /// Not actuated.
    #[default]
    Idle,
}

/// One action's evaluated state, internal bookkeeping included.
#[derive(Debug, Clone, Copy)]
pub struct ActionState {
    /// The action's declared kind.
    pub kind: ActionKind,
    /// The combined value this frame (after modifiers, injection and kind clamping).
    pub value: Vec3,
    /// This frame's phase.
    pub phase: ActionPhase,
    /// Seconds of continuous actuation (reset when actuation ends).
    pub elapsed: f32,
    /// Whether the combined raw value was non-zero this frame (trigger input, not phase-gated).
    raw_actuated: bool,
    /// Whether the action counted as "active" this frame (phase-gated: true when phase is
    /// Started, Ongoing or Triggered, false otherwise). Read with [`ActionState::actuated`].
    actuated: bool,
    /// Whether actuation was already *presented* by an evaluation the frame-level readers saw:
    /// the zero-step-frame fallback seeds its edge detection from this, so a press shows as
    /// `Started`/`Triggered` on the first zero-step frame after it lands and as held
    /// afterwards — while [`ActionState::raw_actuated`] (restored by the fallback) stays ready
    /// for the next fixed step to see the press as a fresh edge. Both evaluation paths update
    /// it to their derived actuation; only the fallback restores `raw_actuated`.
    seen_actuated: bool,
    /// The phase-gated active bit as the last *fixed step* left it (the `actuated` above is
    /// the frame-level view, which the zero-step-frame fallback also writes). The next fixed
    /// step's Chord and Started→Ongoing edges read this, so a fallback pass cannot eat them.
    step_active: bool,
    hold_fired: bool,
    tap_window_open: bool,
    pulse_timer: f32,
    /// A press edge (Started/Triggered) fired on some fixed step of the current frame; cleared
    /// at the start of every frame ([`clear_frame_edges`]). Step-level edges are read with
    /// [`ActionState::just_pressed`] (true on exactly one fixed step); this flag is the
    /// frame-level view for presentation-side readers (`Update` systems, quicksave, agents)
    /// that run once per frame regardless of how many fixed steps the frame ran.
    frame_fired: bool,
    /// The player's input assist bookkeeping (ADR-2646).
    assist: crate::assist::AssistState,
}

impl ActionState {
    fn zero(kind: ActionKind) -> Self {
        Self {
            kind,
            value: Vec3::ZERO,
            phase: ActionPhase::Idle,
            elapsed: 0.0,
            raw_actuated: false,
            actuated: false,
            seen_actuated: false,
            step_active: false,
            hold_fired: false,
            tap_window_open: false,
            pulse_timer: 0.0,
            frame_fired: false,
            assist: crate::assist::AssistState::default(),
        }
    }

    /// True while a [`crate::PressMode::Toggle`] assist holds this action on.
    pub fn toggled_on(&self) -> bool {
        self.assist.latched()
    }

    /// True while the action counts as active this frame: its raw value is non-zero and its phase is
    /// `Started`, `Ongoing` or `Triggered`. Edge triggers (`Pressed`, `Tap`, `Pulse`) are active
    /// only on their fire frame, although `value` can stay non-zero while the input is held.
    pub fn actuated(&self) -> bool {
        self.actuated
    }

    /// True on a frame whose phase is [`ActionPhase::Started`] or [`ActionPhase::Triggered`]
    /// (the "do the thing once" frames).
    pub fn just_pressed(&self) -> bool {
        self.phase == ActionPhase::Started || self.phase == ActionPhase::Triggered
    }

    /// True when a press edge fired on any fixed step of the current frame — the frame-level
    /// view of [`ActionState::just_pressed`] for readers that run once per frame (`Update`
    /// systems, quicksave, agent polling), where the step-level phase may already have been
    /// consumed by a later step of a multi-step frame. Unlike [`ActionState::just_pressed`]
    /// this stays true from the firing step until the end of the frame.
    pub fn just_pressed_this_frame(&self) -> bool {
        self.frame_fired
    }
}

/// Shared empty state returned for players that have never been evaluated.
static EMPTY_PLAYER_ACTIONS: PlayerActions = PlayerActions {
    states: BTreeMap::new(),
};

/// The evaluated state of every declared action, for every player that has device assignment
/// (`docs/specs/11` § 7). Player 0 is the default read target of the convenience accessors.
///
/// # Example
///
/// ```
/// use regia_input::Actions;
///
/// let actions = Actions::default();
/// // Reading an unknown action is false, not a panic.
/// assert!(!actions.pressed("Nope"));
/// ```
#[derive(Resource, Debug, Default)]
pub struct Actions {
    /// Per-player action states.
    pub players: BTreeMap<u8, PlayerActions>,
    /// Per-binding smoothing state for [`crate::Modifier::Smooth`] (carried between frames).
    smoothed: BTreeMap<BindingKey, Vec3>,
    /// Actuation fed to each binding's own triggers on the previous evaluation. An explicit
    /// trigger on one binding must not see another binding's hold as its edge.
    binding_actuated: BTreeMap<BindingKey, bool>,
    /// The per-binding actuation last *presented* (by either pass): the zero-step-frame
    /// fallback's per-binding edge seed. The fallback writes only this map, never
    /// `binding_actuated`, so the next fixed step still sees a binding's press edge.
    binding_seen: BTreeMap<BindingKey, bool>,
    /// How long each binding has been down (seconds of evaluation delta). `Hold` and `Tap` on a
    /// binding time that binding's own press, not the action's `elapsed`, which a sibling binding
    /// that was already held has advanced. Step state: only the fixed-step pass writes it.
    binding_elapsed: BTreeMap<BindingKey, f32>,
    /// Each binding's own `Tap` window: a binding released after a long press must not read
    /// the window a sibling's fresh press opened. Step state: only the fixed-step pass writes it.
    binding_tap_open: BTreeMap<BindingKey, bool>,
    /// Whether each binding's `Hold` has fired during its current press. Like the elapsed time
    /// and tap window, this is step state: a sibling cannot consume or rearm another hold.
    binding_hold_fired: BTreeMap<BindingKey, bool>,
}

/// One player's action states.
#[derive(Debug, Default)]
pub struct PlayerActions {
    /// The actions by name.
    pub states: BTreeMap<String, ActionState>,
}

impl Actions {
    /// All action states of one player (an empty set for players that have not been evaluated
    /// yet).
    pub fn player(&self, player: u8) -> &PlayerActions {
        self.players.get(&player).unwrap_or(&EMPTY_PLAYER_ACTIONS)
    }

    /// All action states of one player, mutable (internal: evaluation seeding).
    pub(crate) fn player_mut(&mut self, player: u8) -> &mut PlayerActions {
        self.players.entry(player).or_default()
    }

    /// True while the action (player 0) is active this frame: see [`ActionState::actuated`]
    /// (phase-gated, so a `Pressed` trigger is true only on its fire frame).
    pub fn pressed(&self, name: &str) -> bool {
        self.player(0).states.get(name).is_some_and(|s| s.actuated)
    }

    /// True on the frame the action (player 0) became active or a trigger fired.
    pub fn just_pressed(&self, name: &str) -> bool {
        self.player(0).states.get(name).is_some_and(|s| s.just_pressed())
    }

    /// The frame-level view of [`Actions::just_pressed`] (see
    /// [`ActionState::just_pressed_this_frame`]): true when a press edge fired on any fixed
    /// step of the current frame. This is what once-per-frame readers (UI, quicksave, agents)
    /// should use; [`Actions::just_pressed`] is the per-fixed-step view for simulation code.
    pub fn just_pressed_this_frame(&self, name: &str) -> bool {
        self.player(0)
            .states
            .get(name)
            .is_some_and(|s| s.just_pressed_this_frame())
    }

    /// The action's phase this frame; [`ActionPhase::Idle`] for unknown names.
    pub fn phase(&self, name: &str) -> ActionPhase {
        self.player(0)
            .states
            .get(name)
            .map_or(ActionPhase::Idle, |s| s.phase)
    }

    /// True on a frame whose phase is [`ActionPhase::Triggered`].
    pub fn triggered(&self, name: &str) -> bool {
        self.phase(name) == ActionPhase::Triggered
    }

    /// Seconds the action has been continuously actuated; 0 while idle.
    pub fn elapsed(&self, name: &str) -> f32 {
        self.player(0).states.get(name).map_or(0.0, |s| s.elapsed)
    }

    /// The action's declared kind, if it exists.
    pub fn kind(&self, name: &str) -> Option<ActionKind> {
        self.player(0).states.get(name).map(|s| s.kind)
    }

    /// The action's value (player 0) as a scalar (the x component): buttons are 0/1, axes their
    /// x value. A stick-Y axis lives on y, so this returns 0 for that source until
    /// [`crate::Swizzle::Yxz`]. Unknown names return `0.0`.
    pub fn axis1d(&self, name: &str) -> f32 {
        self.player(0).states.get(name).map_or(0.0, |s| s.value.x)
    }

    /// The action's value (player 0) as a 2D vector (x = right, y = forward): buttons are 0/1 on
    /// x, 1D axes are `(value, 0)`. Unknown names return zero.
    pub fn axis2d(&self, name: &str) -> regia_core::math::Vec2 {
        self.player(0)
            .states
            .get(name)
            .map(|s| s.value.truncate())
            .unwrap_or_default()
    }

    /// The action's full 3D value (player 0). Unknown names return zero.
    pub fn value(&self, name: &str) -> Vec3 {
        self.player(0).states.get(name).map_or(Vec3::ZERO, |s| s.value)
    }

    /// Drops the toggle latch ([`crate::PressMode::Toggle`]) of `player`'s action `name`, so it
    /// reads its inputs again from the next evaluation (a cutscene or a death that should stand the
    /// player up). Returns whether a latch was on.
    pub fn release_toggle(&mut self, player: u8, name: &str) -> bool {
        let Some(state) = self.players.get_mut(&player).and_then(|p| p.states.get_mut(name)) else {
            return false;
        };
        let was = state.assist.latched();
        state.assist.release();
        was
    }

    /// Drops every toggle latch of every player (see [`Actions::release_toggle`]).
    pub fn release_all_toggles(&mut self) {
        for player in self.players.values_mut() {
            for state in player.states.values_mut() {
                state.assist.release();
            }
        }
    }
}

/// Which devices one player's input is allowed to come from (`docs/specs/11` § 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeviceAssignment {
    /// Every device (the default for player 0).
    Any,
    /// Keyboard and mouse only.
    KeyboardMouse,
    /// One specific gamepad.
    Gamepad(u64),
    /// Touch only (action sources from touch arrive via injection, specs 11 § 7).
    Touch,
}

impl DeviceAssignment {
    /// Whether a binding reading `class` may feed this player.
    fn allows(self, class: DeviceClass) -> bool {
        match self {
            Self::Any => true,
            Self::KeyboardMouse => matches!(class, DeviceClass::Keyboard | DeviceClass::Mouse),
            Self::Gamepad(_) => class == DeviceClass::Gamepad,
            Self::Touch => false,
        }
    }
}

/// Per-player device assignments ([`DeviceAssignment`]); player 0 defaults to
/// [`DeviceAssignment::Any`].
#[derive(Resource, Debug, Clone, Default)]
pub struct PlayerInputs {
    /// Assignment by player index.
    pub players: BTreeMap<u8, DeviceAssignment>,
}

impl PlayerInputs {
    /// The assignment of `player` ([`DeviceAssignment::Any`] when unset).
    pub fn assignment(&self, player: u8) -> DeviceAssignment {
        self.players
            .get(&player)
            .copied()
            .unwrap_or(DeviceAssignment::Any)
    }

    /// Sets one player's assignment. Builder style.
    pub fn with(mut self, player: u8, assignment: DeviceAssignment) -> Self {
        self.players.insert(player, assignment);
        self
    }
}

/// The stack of enabled contexts per player (`docs/specs/11` § 7). Stack *order* is only
/// bookkeeping for push/pop; evaluation order comes from the contexts' priorities.
#[derive(Resource, Debug, Clone, Default)]
pub struct InputContexts {
    stacks: BTreeMap<u8, Vec<String>>,
}

impl InputContexts {
    /// The enabled context names for `player` (empty for unknown players).
    pub fn enabled(&self, player: u8) -> &[String] {
        self.stacks.get(&player).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Pushes a context onto `player`'s stack (no-op twice for the same name).
    pub fn push(&mut self, player: u8, context: impl Into<String>) {
        let stack = self.stacks.entry(player).or_default();
        let context = context.into();
        if !stack.contains(&context) {
            stack.push(context);
        }
    }

    /// Pops a context from `player`'s stack; returns whether it was enabled.
    pub fn pop(&mut self, player: u8, context: &str) -> bool {
        let Some(stack) = self.stacks.get_mut(&player) else {
            return false;
        };
        let before = stack.len();
        stack.retain(|name| name != context);
        let removed = stack.len() != before;
        if stack.is_empty() {
            // An empty stack is the same as none; do not keep evaluating the player.
            self.stacks.remove(&player);
        }
        removed
    }

    /// Forgets `player`'s stack entirely; the player is no longer evaluated each frame unless
    /// it is still assigned devices in `PlayerInputs`.
    pub fn remove_player(&mut self, player: u8) {
        self.stacks.remove(&player);
    }

    /// Replaces `player`'s enabled contexts.
    pub fn set(&mut self, player: u8, contexts: impl IntoIterator<Item = impl Into<String>>) {
        self.stacks
            .insert(player, contexts.into_iter().map(Into::into).collect::<Vec<_>>());
    }

    /// Whether `context` is enabled for `player`.
    pub fn is_enabled(&self, player: u8, context: &str) -> bool {
        self.enabled(player).iter().any(|name| name == context)
    }
}

/// Evaluates one trigger of an action given the actuation edges and previous state, returning
/// the phase this frame. `chord_other` is the chord partner's previous-frame actuation.
fn evaluate_trigger(
    trigger: &crate::Trigger,
    actuated: bool,
    was_actuated: bool,
    state: &mut ActionState,
    dt: f32,
    chord_other: bool,
) -> ActionPhase {
    use crate::Trigger;
    match trigger {
        Trigger::Down => {
            if actuated && !was_actuated {
                ActionPhase::Started
            } else if actuated {
                ActionPhase::Ongoing
            } else if was_actuated {
                ActionPhase::Completed
            } else {
                ActionPhase::Idle
            }
        }
        Trigger::Pressed => {
            if actuated && !was_actuated {
                ActionPhase::Triggered
            } else {
                ActionPhase::Idle
            }
        }
        Trigger::Released => {
            if !actuated && was_actuated {
                ActionPhase::Triggered
            } else {
                ActionPhase::Idle
            }
        }
        Trigger::Hold(secs) => {
            if !actuated {
                return if was_actuated {
                    if state.hold_fired {
                        state.hold_fired = false;
                        ActionPhase::Completed
                    } else {
                        ActionPhase::Canceled
                    }
                } else {
                    ActionPhase::Idle
                };
            }
            if state.elapsed >= *secs && !state.hold_fired {
                state.hold_fired = true;
                return ActionPhase::Triggered;
            }
            if state.hold_fired {
                ActionPhase::Ongoing
            } else if !was_actuated {
                ActionPhase::Started
            } else {
                ActionPhase::Ongoing
            }
        }
        Trigger::Tap(max_secs) => {
            if !actuated {
                let tapped = state.tap_window_open;
                state.tap_window_open = false;
                return if was_actuated {
                    if tapped {
                        ActionPhase::Triggered
                    } else {
                        ActionPhase::Canceled
                    }
                } else {
                    ActionPhase::Idle
                };
            }
            if state.elapsed > *max_secs {
                state.tap_window_open = false; // blew the window; keep actuating until release
                return ActionPhase::Ongoing;
            }
            state.tap_window_open = true;
            if !was_actuated {
                ActionPhase::Started
            } else {
                ActionPhase::Ongoing
            }
        }
        Trigger::Pulse(interval) => {
            if !actuated {
                return if was_actuated {
                    ActionPhase::Completed
                } else {
                    ActionPhase::Idle
                };
            }
            if !was_actuated {
                state.pulse_timer = interval.max(0.0);
                return ActionPhase::Triggered;
            }
            state.pulse_timer -= dt;
            if state.pulse_timer <= 0.0 {
                // Carry small lateness for an exact average rate, but never a hitch longer than
                // one interval: that would leave the timer negative and fire every frame.
                let interval = interval.max(f32::EPSILON);
                let carried = state.pulse_timer + interval;
                state.pulse_timer = if carried > 0.0 { carried } else { interval };
                return ActionPhase::Triggered;
            }
            ActionPhase::Ongoing
        }
        Trigger::Chord(_) => {
            if !chord_other {
                // The partner is not actuated: the chord is inactive this frame; actuation and
                // elapsed time reset as if the action was released.
                return ActionPhase::Idle;
            }
            if actuated && !was_actuated {
                ActionPhase::Started
            } else if actuated {
                ActionPhase::Ongoing
            } else if was_actuated {
                ActionPhase::Completed
            } else {
                ActionPhase::Idle
            }
        }
    }
}

/// Combines phases from several triggers or bindings: the "loudest" wins.
///
/// `Ongoing` outranks `Completed`. Bindings of one action are alternatives, so one
/// binding ending must not cancel another binding that is still held. One binding
/// never reports both on the same frame (a release is not still held). `Completed`
/// still outranks `Canceled`, so a finished hold is not reported as a failed tap.
fn combine_phase(a: ActionPhase, b: ActionPhase) -> ActionPhase {
    use ActionPhase::*;
    let rank = |p: ActionPhase| match p {
        Triggered => 0,
        Started => 1,
        Ongoing => 2,
        Completed => 3,
        Canceled => 4,
        Idle => 5,
    };
    if rank(a) <= rank(b) { a } else { b }
}

/// One binding's post-modifier value, kept so its triggers see that binding and not the
/// action's combined value.
struct BindingSample<'a> {
    action: &'a str,
    key: BindingKey,
    value: Vec3,
    triggers: &'a [crate::Trigger],
}

/// Runs one binding's triggers. An empty list is [`crate::Trigger::Down`]. A [`crate::Trigger::Chord`]
/// edges on whether the chord itself was active last evaluation (`chord_was_active`), not on the
/// raw key. Other triggers edge on `was_actuated`.
fn phase_from_triggers(
    triggers: &[crate::Trigger],
    actuated: bool,
    was_actuated: bool,
    chord_was_active: bool,
    state: &mut ActionState,
    dt: f32,
    partners: &BTreeMap<String, bool>,
) -> ActionPhase {
    if triggers.is_empty() {
        return evaluate_trigger(&crate::Trigger::Down, actuated, was_actuated, state, dt, true);
    }
    let mut phase = ActionPhase::Idle;
    for trigger in triggers {
        let (was, chord_other) = match trigger {
            crate::Trigger::Chord(other) => (chord_was_active, partners.get(other).copied().unwrap_or(false)),
            _ => (was_actuated, true),
        };
        let candidate = evaluate_trigger(trigger, actuated, was, state, dt, chord_other);
        phase = combine_phase(phase, candidate);
    }
    phase
}

/// `FixedUpdate` (set [`crate::InputSet::Evaluation`]): evaluates every binding of every enabled
/// context against the raw device state, merges injected values, runs triggers and writes the
/// [`Actions`] resource — **once per fixed step**, with that step as [`Time::delta`], so hold,
/// tap, pulse, smoothing and assist timers advance in simulation time and an edge (a press) is
/// true for exactly one fixed step no matter how many steps a frame runs. See the module docs
/// for the algorithm. On frames whose fixed loop runs no step (a fast display accumulating less
/// than one step, a paused app, no sim step), [`evaluate_actions_unless_stepped`] runs this
/// pass once in `PreUpdate` with a zero delta instead.
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn evaluate_actions(
    config: Res<InputConfig>,
    contexts: Res<InputContexts>,
    players: Res<PlayerInputs>,
    keyboard: Res<Keyboard>,
    mouse: Res<Mouse>,
    gamepads: Res<Gamepads>,
    mut injection: ResMut<crate::InputInjection>,
    time: Res<Time>,
    mut actions: ResMut<Actions>,
    mut warned: Local<BTreeSet<String>>,
    assists: Option<Res<crate::InputAssists>>,
    look: Option<Res<crate::config::LookSettings>>,
    gate: Option<Res<crate::UiPointerGate>>,
    claims: Option<Res<crate::PointerClaims>>,
) {
    // Buttons the UI claimed read as up for every binding (ADR-7976).
    let masked = claims.and_then(|claims| claims.masked(&mouse));
    evaluate_actions_impl(
        &config,
        &contexts,
        &players,
        &keyboard,
        masked.as_ref().unwrap_or(&*mouse),
        &gamepads,
        &mut injection,
        time.frame_count(),
        time.delta_secs(),
        &mut actions,
        &mut warned,
        assists.as_deref(),
        look.as_deref(),
        gate.as_deref(),
        StepPersistence::Full,
    );
}

/// `PreUpdate` (set [`crate::InputSet::Evaluation`]): the zero-step-frame fallback. Runs the
/// full evaluation with a **zero delta** on frames whose fixed loop will run no step
/// ([`FixedTime::would_run`] predicts it from the accumulator), so frame-quantized consumers
/// keep working exactly as they did when evaluation was per frame:
///
/// * `Update` readers (menus, quicksave) see this frame's edges and values even on a 144 Hz
///   display whose frames mostly bank into the accumulator instead of running a step;
/// * a one-frame [`crate::InputInjection`] applies to the frame it was scheduled for, not only
///   to frames that happen to run a step;
/// * with the simulation halted (paused, suspended), edges still refresh while the timing
///   state (elapsed, hold, tap, pulse, smoothing, assists) stays frozen.
///
/// The delta is zero, so nothing double-advances when the banked step runs later. The fallback
/// is also **step-state neutral**: it writes the frame-level view (value, phase, `actuated`,
/// [`ActionState::just_pressed_this_frame`]) but restores every step-carried field
/// (`raw_actuated`, `elapsed`, hold/tap/pulse timers, assist latches) afterwards, so a press
/// that lands on a zero-step frame is *not* consumed here — the first later fixed step still
/// sees the edge (a 144 Hz display banking a 60 Hz fixed loop must not eat presses, fixrev
/// review of the per-step evaluation). On frames the fixed loop runs, this system does nothing
/// (the evaluation happens per step in `FixedUpdate`).
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn evaluate_actions_unless_stepped(
    config: Res<InputConfig>,
    contexts: Res<InputContexts>,
    players: Res<PlayerInputs>,
    keyboard: Res<Keyboard>,
    mouse: Res<Mouse>,
    gamepads: Res<Gamepads>,
    mut injection: ResMut<crate::InputInjection>,
    time: Res<Time>,
    fixed: Option<Res<FixedTime>>,
    mut actions: ResMut<Actions>,
    mut warned: Local<BTreeSet<String>>,
    assists: Option<Res<crate::InputAssists>>,
    look: Option<Res<crate::config::LookSettings>>,
    gate: Option<Res<crate::UiPointerGate>>,
    claims: Option<Res<crate::PointerClaims>>,
) {
    if fixed.is_some_and(|fixed| fixed.would_run(time.sim_delta())) {
        return;
    }
    // Buttons the UI claimed read as up for every binding (ADR-7976).
    let masked = claims.and_then(|claims| claims.masked(&mouse));
    evaluate_actions_impl(
        &config,
        &contexts,
        &players,
        &keyboard,
        masked.as_ref().unwrap_or(&*mouse),
        &gamepads,
        &mut injection,
        time.frame_count(),
        0.0,
        &mut actions,
        &mut warned,
        assists.as_deref(),
        look.as_deref(),
        gate.as_deref(),
        StepPersistence::FrameOnly,
    );
}

/// Which evaluated state an evaluation pass persists into [`Actions`]. The fixed-step pass is
/// the simulation's view: everything it writes is carried to the next step. The zero-step-frame
/// fallback only refreshes the frame-level view (value, phase, `actuated`, `frame_fired`): the
/// step-carried fields it touched for the evaluation are restored, so the next fixed step sees
/// the world exactly as the last step left it (a press on a banked frame is not consumed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepPersistence {
    /// The fixed-step evaluation: all state persists.
    Full,
    /// The zero-step-frame fallback: the frame-level view persists, step state is restored.
    FrameOnly,
}

/// The shared evaluation (see [`evaluate_actions`] and [`evaluate_actions_unless_stepped`]):
/// plain references and an explicit `dt`, so both schedule entry points run the identical
/// algorithm — the fixed-step entry with the step, the fallback with zero.
#[allow(clippy::too_many_arguments)]
fn evaluate_actions_impl(
    config: &InputConfig,
    contexts: &InputContexts,
    players: &PlayerInputs,
    keyboard: &Keyboard,
    mouse: &Mouse,
    gamepads: &Gamepads,
    injection: &mut crate::InputInjection,
    frame: u64,
    dt: f32,
    actions: &mut Actions,
    warned: &mut BTreeSet<String>,
    assists: Option<&crate::InputAssists>,
    look: Option<&crate::config::LookSettings>,
    gate: Option<&crate::UiPointerGate>,
    persistence: StepPersistence,
) {
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

    // Everyone mentioned in either resource gets a full evaluation; player 0 always does.
    let mut player_ids: BTreeSet<u8> = players.players.keys().copied().collect();
    player_ids.extend(contexts.stacks.keys().copied());
    player_ids.insert(0);
    // Rebuilt every evaluation. A binding that disappeared must not keep a stale edge.
    let mut fresh_bindings: BTreeMap<BindingKey, bool> = BTreeMap::new();
    let mut fresh_elapsed: BTreeMap<BindingKey, f32> = BTreeMap::new();
    let mut fresh_tap: BTreeMap<BindingKey, bool> = BTreeMap::new();
    let mut fresh_hold: BTreeMap<BindingKey, bool> = BTreeMap::new();

    for &player in &player_ids {
        let assignment = players.assignment(player);
        let enabled: Vec<&str> = contexts.enabled(player).iter().map(String::as_str).collect();
        let devices = Devices {
            keyboard,
            mouse,
            gamepads,
            pad: match assignment {
                DeviceAssignment::Gamepad(id) => Some(id),
                _ => None,
            },
        };

        // Seed zero values, carrying last frame's timing/edge bookkeeping.
        let restore_step_state = persistence == StepPersistence::FrameOnly;
        let mut previous_states: BTreeMap<String, ActionState> = BTreeMap::new();
        let mut next: BTreeMap<String, (ActionState, bool, bool)> = config
            .actions
            .iter()
            .map(|(name, kind)| {
                let previous = actions
                    .player(player)
                    .states
                    .get(name)
                    .copied()
                    .unwrap_or_else(|| ActionState::zero(*kind));
                if restore_step_state {
                    previous_states.insert(name.clone(), previous);
                }
                let mut state = ActionState::zero(*kind);
                state.elapsed = previous.elapsed;
                state.hold_fired = previous.hold_fired;
                state.tap_window_open = previous.tap_window_open;
                state.pulse_timer = previous.pulse_timer;
                state.assist = previous.assist;
                state.frame_fired = previous.frame_fired;
                state.seen_actuated = previous.seen_actuated;
                state.step_active = previous.step_active;
                // `frame_fired` is frame-level: carried across this frame's steps (cleared in
                // `First`, see `clear_frame_edges`), unlike the step-level fields above.
                // The edge-detection seeds differ per pass: the fixed step detects edges
                // against the last step's raw actuation and active bit (the simulation must see
                // each press exactly once), the zero-step-frame fallback against what was
                // already presented to frame-level readers (an edge shows once, not on every
                // banked frame of a held press). `raw_actuated` is the post-assist value, so
                // Hold and Released still see a hold that had not reached its phase yet; the
                // active bit is the phase-gated one a Chord uses for its own rising edge.
                let (was_raw, was_active) = match persistence {
                    StepPersistence::Full => (previous.raw_actuated, previous.step_active),
                    StepPersistence::FrameOnly => (previous.seen_actuated, previous.actuated),
                };
                (name.clone(), (state, was_raw, was_active))
            })
            .collect();

        // Contexts evaluate highest priority first; ties break by name for determinism.
        let mut ordered: Vec<(&String, &crate::InputContext)> = config
            .contexts
            .iter()
            .filter(|(name, _)| enabled.contains(&name.as_str()))
            .collect();
        ordered.sort_by(|a, b| b.1.priority.cmp(&a.1.priority).then_with(|| a.0.cmp(b.0)));

        // Sources consumed by higher-priority contexts are hidden from lower ones.
        let mut consumed: HashSet<&InputSource> = HashSet::new();
        // Post-modifier value of every eligible-or-skipped binding. Triggers run on these,
        // not on the combined action value, so one binding's trigger list cannot replace
        // another's default Down and cannot eat another binding's press edge.
        let mut samples: Vec<BindingSample<'_>> = Vec::new();

        for (context_name, context) in &ordered {
            let mut marks: Vec<&InputSource> = Vec::new();
            for (index, binding) in context.bindings.iter().enumerate() {
                let Some((state, _, _)) = next.get_mut(&binding.action) else {
                    if warned.len() >= MAX_WARNED_BINDINGS {
                        warned.clear(); // bounded: generated bad names cannot grow it forever
                    }
                    if warned.insert(binding.action.clone()) {
                        tracing::warn!(
                            target: "regia_input",
                            "binding for undeclared action {:?}; ignored (declare it in InputConfig)",
                            binding.action
                        );
                    }
                    continue;
                };
                let kind = state.kind;
                let key = BindingKey {
                    player,
                    context: context_name.to_string(),
                    index,
                };
                // A mouse button the runtime UI took (a press on a button) is hidden from gameplay
                // contexts (`specs/18` § 4 input routing, ADR-7070).
                let eligible = assignment.allows(binding.input.class())
                    && !consumed.contains(&binding.input)
                    && !gate.is_some_and(|g| g.hides(context_name, &binding.input));
                let value = if eligible {
                    let raw = binding.input.raw(kind, &devices);
                    let mut modifier_frame = ModifierFrame {
                        delta_secs: dt,
                        binding: key.clone(),
                        smoothed: &mut actions.smoothed,
                    };
                    apply_modifiers(&binding.modifiers, raw, &mut modifier_frame)
                } else {
                    // Wrong device, or hidden by a higher context: this binding contributes
                    // nothing, and a level trigger that was held can complete.
                    Vec3::ZERO
                };
                if value != Vec3::ZERO {
                    if context.consume_lower {
                        marks.push(&binding.input);
                    }
                    state.value = kind.combine(state.value, value);
                }
                samples.push(BindingSample {
                    action: binding.action.as_str(),
                    key,
                    value,
                    triggers: binding.triggers.as_slice(),
                });
            }
            for mark in marks {
                consumed.insert(mark);
            }
        }

        // The player's look scale applies to named look actions only, and only to the device
        // value. Injection below still replaces the value outright (ADR-3464).
        if !config.look_actions.is_empty() {
            let look = look.copied().unwrap_or_default();
            for (name, (state, _, _)) in next.iter_mut() {
                if config.look_actions.iter().any(|action| action == name) {
                    state.value = look.apply(state.value);
                }
            }
        }

        // Chord partners read the *previous frame's* actuation, so evaluation order does not
        // matter; take it before overwriting `actions`.
        let mut chord_partners: Vec<String> = Vec::new();
        for sample in &samples {
            for trigger in sample.triggers {
                if let crate::Trigger::Chord(other) = trigger
                    && !chord_partners.contains(other)
                {
                    chord_partners.push(other.clone());
                }
            }
        }
        let partner_actuation: BTreeMap<String, bool> = chord_partners
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    actions
                        .player(player)
                        .states
                        .get(name)
                        .is_some_and(|s| s.actuated),
                )
            })
            .collect();

        for (name, (state, was_raw, was_active)) in next.iter_mut() {
            // Injection (agent / virtual controls) overrides the combined device value.
            let injected = injection.value_for(name, frame);
            if let Some(injected) = injected {
                state.value = injected;
            }
            let pre_assist_down = state.value != Vec3::ZERO;
            state.value = state.kind.clamp(state.value);
            // The player's assists (ADR-2646) turn the input into what the triggers see.
            // A per-player or per-gamepad entry overlays the default set (ADR-3464).
            let assist = assists.and_then(|a| {
                let pad = match assignment {
                    DeviceAssignment::Gamepad(id) => Some(id),
                    _ => None,
                };
                a.get_for(player, pad, name)
            });
            state.value = state.kind.clamp(crate::assist::assisted_value(
                assist,
                &mut state.assist,
                state.value,
                dt,
            ));
            // Post-assist actuation. Edge triggers on a single device binding use their own
            // previous bit (`binding_actuated`); this flag is the action-level hold that
            // Released, Hold and auto-repeat still read, including while a trigger phase is Idle.
            state.raw_actuated = state.value != Vec3::ZERO;
            // Both passes mark the actuation as presented for the next zero-step-frame
            // fallback; only `raw_actuated` is restored below, so the next fixed step still
            // detects the press as a fresh edge.
            state.seen_actuated = state.raw_actuated;
            let current_actuated = state.raw_actuated;
            // Injection replaces every device, and an assist that changes whether the action
            // is down (a latch, a minimum hold) is one value. Per-binding edges apply only
            // when the triggers are looking at the device bindings themselves.
            let action_level = injected.is_some() || pre_assist_down != current_actuated;

            let mut phase = ActionPhase::Idle;
            let mut saw_binding = false;
            // One pulse timer per action. The first Pulse binding that is down steps it;
            // a later one that is also down must not step it again this evaluation.
            let mut pulse_lock: Option<f32> = None;
            for sample in samples.iter().filter(|sample| sample.action == name.as_str()) {
                saw_binding = true;
                let device_down = sample.value != Vec3::ZERO;
                let edge_seed = match persistence {
                    StepPersistence::Full => &actions.binding_actuated,
                    StepPersistence::FrameOnly => &actions.binding_seen,
                };
                let was_binding = edge_seed.get(&sample.key).copied().unwrap_or(false);
                // Buttons are alternatives: each binding has its own edge. An axis binding
                // counts only while the combined value is live, so two axes that sum to zero
                // read as released.
                let actuated_bit = if action_level {
                    current_actuated
                } else if state.kind == ActionKind::Button {
                    device_down
                } else {
                    device_down && current_actuated
                };
                let was_bit = if action_level { *was_raw } else { was_binding };
                // `Hold` and `Tap` time this binding's own press. Under an action-level value
                // (an injection, an assist that changes whether the action is down) they keep
                // the action's `elapsed`, as every binding then sees one value.
                let binding_held = actions.binding_elapsed.get(&sample.key).copied().unwrap_or(0.0);
                let action_elapsed = state.elapsed;
                if !action_level {
                    state.elapsed = binding_held;
                }
                // The same for the `Tap` window: each binding keeps its own.
                let action_tap = state.tap_window_open;
                if !action_level {
                    state.tap_window_open = actions
                        .binding_tap_open
                        .get(&sample.key)
                        .copied()
                        .unwrap_or(false);
                }
                // A fired Hold belongs to this binding, just like its elapsed time. Retain
                // the action latch for injected/assisted values, which all bindings share.
                let action_hold = state.hold_fired;
                if !action_level {
                    state.hold_fired = actions
                        .binding_hold_fired
                        .get(&sample.key)
                        .copied()
                        .unwrap_or(false);
                }
                let saved_hold = state.hold_fired;
                let saved_tap = state.tap_window_open;
                let saved_pulse = state.pulse_timer;
                let pulses = sample
                    .triggers
                    .iter()
                    .any(|trigger| matches!(trigger, crate::Trigger::Pulse(_)));
                if pulses
                    && actuated_bit
                    && let Some(locked) = pulse_lock
                {
                    // Pulse subtracts `dt` when the binding was already down. Feeding
                    // `locked + dt` lands that subtract on the owner's post-step timer,
                    // so this binding cannot fire one step early. A fresh press ignores
                    // the timer and still fires immediately.
                    state.pulse_timer = locked + dt;
                }
                let candidate = phase_from_triggers(
                    sample.triggers,
                    actuated_bit,
                    was_bit,
                    *was_active,
                    state,
                    dt,
                    &partner_actuation,
                );
                state.elapsed = action_elapsed;
                if !actuated_bit && !was_bit {
                    // An alternative that is up, and was up, must not clear or advance the
                    // action's tap, hold, or pulse memory.
                    state.hold_fired = saved_hold;
                    state.tap_window_open = saved_tap;
                    state.pulse_timer = saved_pulse;
                } else if pulses && actuated_bit {
                    if let Some(locked) = pulse_lock {
                        state.pulse_timer = locked;
                    } else {
                        pulse_lock = Some(state.pulse_timer);
                    }
                }
                fresh_hold.insert(sample.key.clone(), state.hold_fired);
                if !action_level {
                    state.hold_fired = action_hold;
                }
                fresh_tap.insert(sample.key.clone(), state.tap_window_open);
                if !action_level {
                    state.tap_window_open = action_tap;
                }
                phase = combine_phase(phase, candidate);
                fresh_bindings.insert(sample.key.clone(), actuated_bit);
                fresh_elapsed.insert(
                    sample.key.clone(),
                    if actuated_bit { binding_held + dt } else { 0.0 },
                );
            }
            if !saw_binding {
                // No binding in an enabled context (injection-only, or every context off).
                phase = evaluate_trigger(&crate::Trigger::Down, current_actuated, *was_raw, state, dt, true);
            }
            // Started means the action became active this evaluation. A second Down binding
            // going down while the action is already held does not restart it. Triggered
            // still wins, so a Pressed binding fires beside a held Down binding, and a
            // Released binding still fires on the frame the value returns to zero.
            if *was_active && phase == ActionPhase::Started {
                phase = ActionPhase::Ongoing;
            }
            state.phase = phase;
            // Output `actuated` is gated by the phase: edge-fire triggers (Pressed, Tap, Pulse)
            // only count as active on their fire frame; triggers that maintain a held state
            // (Down, Hold, Released-tracking, Chord) count as active while ongoing.
            state.actuated = match state.phase {
                ActionPhase::Started | ActionPhase::Ongoing | ActionPhase::Triggered => current_actuated,
                _ => false,
            };
            // Auto-repeat (ADR-2646): a held action fires again; the fire frame counts as active.
            if crate::assist::repeat_fires(
                assist.and_then(|a| a.repeat.as_ref()),
                &mut state.assist,
                current_actuated,
                *was_raw,
                dt,
            ) {
                state.phase = ActionPhase::Triggered;
                state.actuated = true;
            }
            // The frame-level edge view: any step's Started/Triggered marks the frame.
            if state.phase == ActionPhase::Started || state.phase == ActionPhase::Triggered {
                state.frame_fired = true;
            }
            state.elapsed = if state.actuated { state.elapsed + dt } else { 0.0 };
            state.step_active = state.actuated;
        }

        if restore_step_state {
            // The fallback only re-renders the frame-level view; the step-carried fields go
            // back to what the last fixed step left, so the next stepped evaluation sees the
            // press edge fresh instead of finding `was_actuated` already true (and a
            // `ScaleByDelta` hold does not have its `elapsed` zeroed by the zero-delta pass).
            for (name, (state, _, _)) in next.iter_mut() {
                if let Some(previous) = previous_states.get(name) {
                    state.raw_actuated = previous.raw_actuated;
                    state.step_active = previous.step_active;
                    state.elapsed = previous.elapsed;
                    state.hold_fired = previous.hold_fired;
                    state.tap_window_open = previous.tap_window_open;
                    state.pulse_timer = previous.pulse_timer;
                    state.assist = previous.assist;
                }
            }
        }

        let player_actions = actions.player_mut(player);
        player_actions.states = next
            .into_iter()
            .map(|(name, (state, _, _))| (name, state))
            .collect();
    }
    if persistence == StepPersistence::Full {
        actions.binding_actuated = fresh_bindings.clone();
        // Per-binding hold time, latch and tap window are step state: the zero-step-frame fallback
        // restores them like the action's own timers (a release on a banked frame must not zero
        // the hold time the next step's `Tap`/`Hold` reads).
        actions.binding_elapsed = fresh_elapsed;
        actions.binding_tap_open = fresh_tap;
        actions.binding_hold_fired = fresh_hold;
    }
    actions.binding_seen = fresh_bindings;
    // A player no longer listed anywhere is gone: drop its states and smoothing so a held
    // button cannot read as down forever after the player leaves.
    actions.players.retain(|player, _| player_ids.contains(player));
    actions.smoothed.retain(|key, _| player_ids.contains(&key.player));
    injection.prune(frame);
}

/// `PreUpdate` (set [`crate::InputSet::Evaluation`], after the evaluation): zeroes
/// `Mouse::delta`, the per-frame motion accumulator, so the raw delta cannot be consumed twice
/// per frame. Actions already carry the evaluated value.
pub fn clear_mouse_delta(mut mouse: ResMut<Mouse>) {
    mouse.delta = regia_core::math::Vec2::ZERO;
}

/// Where this frame's mouse motion goes among the frame's fixed steps (ADR-5599).
///
/// `Mouse::delta` is per frame. A multi-step frame applies it once, on its **last** fixed step:
/// `FixedUpdate` readers still see the motion exactly once, and `Update` readers, which see the
/// last step's [`Actions`], see the whole frame's motion. Applied on the first step instead, an
/// `Update` mouse look read zero on every frame that ran two or more steps (every frame below
/// the fixed rate).
#[derive(Resource, Debug, Default)]
pub(crate) struct MouseDeltaSchedule {
    /// Fixed steps of this frame not yet evaluated, as predicted in `PreUpdate`.
    steps_left: u32,
    /// Motion held back from the frame's earlier steps.
    held: regia_core::math::Vec2,
}

/// `PreUpdate` (set [`crate::InputSet::Evaluation`], before the zero-step-frame fallback):
/// predicts how many fixed steps this frame runs, from a copy of [`FixedTime`] fed the frame's
/// simulation delta (the fixed loop does the same with the real one). Motion still held from a
/// frame whose loop ran fewer steps than predicted joins this frame's motion, so none is lost.
pub(crate) fn plan_mouse_delta(
    time: Res<Time>,
    fixed: Option<Res<FixedTime>>,
    mut mouse: ResMut<Mouse>,
    mut schedule: ResMut<MouseDeltaSchedule>,
) {
    let held = std::mem::take(&mut schedule.held);
    mouse.delta += held;
    // `App::update` inserts the default `FixedTime` when there is none.
    let mut probe = fixed.as_deref().cloned().unwrap_or_default();
    schedule.steps_left = probe.accumulate(time.sim_delta());
}

/// `FixedUpdate` (set [`crate::InputSet::Evaluation`], before [`evaluate_actions`]): holds the
/// frame's mouse motion back on every step but the last one the frame runs, and hands it to the
/// last (see `MouseDeltaSchedule`). A step that was not predicted (no `PreUpdate` this frame)
/// counts as the last.
pub(crate) fn stage_mouse_delta(mut mouse: ResMut<Mouse>, mut schedule: ResMut<MouseDeltaSchedule>) {
    let last = schedule.steps_left <= 1;
    schedule.steps_left = schedule.steps_left.saturating_sub(1);
    if last {
        let held = std::mem::take(&mut schedule.held);
        mouse.delta += held;
    } else {
        let motion = std::mem::take(&mut mouse.delta);
        schedule.held += motion;
    }
}

/// `First`: clears the frame-level press edges ([`ActionState::just_pressed_this_frame`]) so a
/// new frame starts clean: `PreUpdate` readers never see last frame's flag, and the first
/// evaluation of the frame (a fixed step, or the zero-step-frame fallback in `PreUpdate`) sets
/// it afresh. Step-level phases ([`ActionState::just_pressed`]) are recomputed by every
/// evaluation and need no clearing.
pub fn clear_frame_edges(mut actions: ResMut<Actions>) {
    for player in actions.players.values_mut() {
        for state in player.states.values_mut() {
            state.frame_fired = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_actions_read_zero_and_false() {
        let actions = Actions::default();
        assert!(!actions.pressed("Move"));
        assert!(!actions.just_pressed("Move"));
        assert_eq!(actions.axis1d("Move"), 0.0);
        assert_eq!(actions.axis2d("Move"), regia_core::math::Vec2::ZERO);
        assert_eq!(actions.value("Move"), Vec3::ZERO);
        assert_eq!(actions.phase("Move"), ActionPhase::Idle);
        assert_eq!(actions.kind("Move"), None);
        assert_eq!(actions.elapsed("Move"), 0.0);
    }

    #[test]
    fn emptied_and_removed_player_stacks_are_forgotten() {
        let mut contexts = InputContexts::default();
        contexts.push(3, "Gameplay");
        assert!(contexts.pop(3, "Gameplay"));
        assert!(!contexts.stacks.contains_key(&3), "empty stack dropped");
        contexts.push(4, "Menu");
        contexts.remove_player(4);
        assert!(contexts.stacks.is_empty());
        assert!(!contexts.is_enabled(4, "Menu"));
    }

    #[test]
    fn context_stack_push_pop_and_set() {
        let mut contexts = InputContexts::default();
        contexts.push(0, "Gameplay");
        contexts.push(0, "Menu");
        contexts.push(0, "Menu"); // duplicate is a no-op
        assert_eq!(contexts.enabled(0).len(), 2);
        assert!(contexts.is_enabled(0, "Menu"));
        assert!(contexts.pop(0, "Menu"));
        assert!(!contexts.is_enabled(0, "Menu"));
        assert!(!contexts.pop(0, "Menu"));
        contexts.set(1, ["Solo"]);
        assert_eq!(contexts.enabled(1), &["Solo".to_string()]);
    }

    #[test]
    fn pulse_does_not_machine_gun_after_a_long_frame() {
        let trigger = crate::Trigger::Pulse(0.1);
        let mut state = ActionState::zero(ActionKind::Button);
        assert_eq!(
            evaluate_trigger(&trigger, true, false, &mut state, 0.0, true),
            ActionPhase::Triggered
        );
        // A 500 ms stall fires once...
        assert_eq!(
            evaluate_trigger(&trigger, true, true, &mut state, 0.5, true),
            ActionPhase::Triggered
        );
        // ...and the next normal frames wait for a full interval again.
        assert_eq!(
            evaluate_trigger(&trigger, true, true, &mut state, 0.016, true),
            ActionPhase::Ongoing
        );
        assert_eq!(
            evaluate_trigger(&trigger, true, true, &mut state, 0.016, true),
            ActionPhase::Ongoing
        );
    }

    #[test]
    fn phase_ranking_prefers_triggered() {
        assert_eq!(
            combine_phase(ActionPhase::Ongoing, ActionPhase::Triggered),
            ActionPhase::Triggered
        );
        assert_eq!(
            combine_phase(ActionPhase::Started, ActionPhase::Ongoing),
            ActionPhase::Started
        );
        assert_eq!(
            combine_phase(ActionPhase::Idle, ActionPhase::Canceled),
            ActionPhase::Canceled
        );
        assert_eq!(
            combine_phase(ActionPhase::Canceled, ActionPhase::Completed),
            ActionPhase::Completed,
            "a finished trigger outranks one that canceled on the same frame"
        );
        assert_eq!(
            combine_phase(ActionPhase::Ongoing, ActionPhase::Completed),
            ActionPhase::Ongoing,
            "a binding that is still held outranks another binding that ended"
        );
    }
}
