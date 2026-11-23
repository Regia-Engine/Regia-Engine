// SPDX-License-Identifier: MPL-2.0
//! Motion presets (UI default design D14, ADR-522): enter, exit, hover and press animations built
//! on the tween system ([`crate::tween`]), timed by the theme's motion tokens, and the one
//! reduced-motion switch every UI animation reads.
//!
//! - [`UiMotion`] names a node's presets: a [`RevealPreset`] for enter and exit (fade, slide,
//!   zoom), a [`MotionPose`] for hover and press (lift, grow, sink, shrink), the [`MotionSpeed`]
//!   token enter and exit use, and what an exit does at its end ([`ExitAction`]). Named sets
//!   ([`UiMotion::preset`]: `button`, `card`, `popup`, `dialog`, `toast`, `page`) cover the
//!   spec's widgets.
//! - [`UiMotionState`] (added with [`UiMotion`]) holds the phase, the pose and requests:
//!   [`UiMotionState::request_enter`] and [`UiMotionState::request_exit`].
//! - [`run_ui_motion`] turns requests, the [`Hovered`] / [`Pressed`] markers (and [`Focused`] in
//!   navigation mode) into [`UiTween`]s. Nothing else drives the animation: the tween system
//!   samples the plans on the game clock.
//! - Timing comes from the active theme ([`MotionTokens`]): enter uses the speed token with
//!   `ease.enter`, exit 0.75 × that with `ease.exit`, hover and press `motion.fast` with
//!   `ease.standard` (`docs/specs/ui-default-design.md` § 2.6).
//! - Reduced motion ([`reduced_motion`]) is on when [`ActiveTheme::reduced_motion`] or the
//!   player's [`UiAccessibility::reduced_motion`] is. Presets then follow the spec rule:
//!   transforms jump, opacity fades are capped at `motion.fast`. Plain tweens jump to their end.
//!
//! See `book/agents/ui-motion.md`.

use regia_app::prelude::*;

use crate::focus::UiInputMode;
use crate::interaction::UiPointer;
use crate::node::{Disabled, Focused, Hovered, NodeStyle, Pressed};
use crate::scale::UiAccessibility;
use crate::style::Display;
use crate::theme_file::ResolvedTheme;
use crate::theme_resolve::{ActiveTheme, ThemeResolver};
use crate::theme_tokens::Easing;
use crate::tween::{Ease, Tween, TweenPlan, TweenProperty, TweenValue, UiTween, UiTweenFinished};

/// Whether UI animations run in reduced motion: the theme selection's flag or the player's
/// accessibility setting (either turns it on).
pub fn reduced_motion(theme: Option<&ActiveTheme>, access: Option<&UiAccessibility>) -> bool {
    theme.is_some_and(|t| t.reduced_motion) || access.is_some_and(|a| a.reduced_motion)
}

/// An exit lasts this fraction of the matching enter (spec § 2.6, `ease.exit`).
pub const EXIT_DURATION_FACTOR: f32 = 0.75;

// ---------------------------------------------------------------------------------------------
// Tokens and timing
// ---------------------------------------------------------------------------------------------

/// Which event a preset answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionTrigger {
    /// The node appears.
    Enter,
    /// The node leaves.
    Exit,
    /// The pointer is over the node (or it has focus in navigation mode).
    Hover,
    /// The node is held down.
    Press,
}

impl MotionTrigger {
    /// Every trigger, in documentation order.
    pub const ALL: [MotionTrigger; 4] = [Self::Enter, Self::Exit, Self::Hover, Self::Press];

    /// The snake_case name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Enter => "enter",
            Self::Exit => "exit",
            Self::Hover => "hover",
            Self::Press => "press",
        }
    }

    /// Looks a trigger up by name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// The motion duration token enter and exit use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MotionSpeed {
    /// `motion.fast` (80 ms by default).
    Fast,
    /// `motion.base` (160 ms).
    Base,
    /// `motion.slow` (240 ms): popups, dialogs, toasts.
    #[default]
    Slow,
    /// `motion.page` (320 ms): screen and page transitions.
    Page,
}

impl MotionSpeed {
    /// Every speed, fastest first.
    pub const ALL: [MotionSpeed; 4] = [Self::Fast, Self::Base, Self::Slow, Self::Page];

    /// The short name used by the agent protocol (`"fast"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Base => "base",
            Self::Slow => "slow",
            Self::Page => "page",
        }
    }

    /// Looks a speed up by its short name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The theme token (`"motion.slow"`).
    pub fn token(self) -> &'static str {
        match self {
            Self::Fast => "motion.fast",
            Self::Base => "motion.base",
            Self::Slow => "motion.slow",
            Self::Page => "motion.page",
        }
    }
}

/// A duration and an easing curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionTiming {
    /// Seconds.
    pub duration: f32,
    /// Curve.
    pub ease: Ease,
}

/// The theme's motion tokens in seconds and tween curves (spec § 2.6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionTokens {
    /// `motion.fast`.
    pub fast: f32,
    /// `motion.base`.
    pub base: f32,
    /// `motion.slow`.
    pub slow: f32,
    /// `motion.page`.
    pub page: f32,
    /// `ease.standard`.
    pub ease_standard: Ease,
    /// `ease.enter`.
    pub ease_enter: Ease,
    /// `ease.exit`.
    pub ease_exit: Ease,
}

impl Default for MotionTokens {
    fn default() -> Self {
        Self::SPEC
    }
}

impl MotionTokens {
    /// The values of `docs/specs/ui-default-design.md` § 2.6, used for any token a theme lacks.
    pub const SPEC: Self = Self {
        fast: 0.08,
        base: 0.16,
        slow: 0.24,
        page: 0.32,
        ease_standard: Ease::CubicBezier([0.2, 0.0, 0.0, 1.0]),
        ease_enter: Ease::CubicBezier([0.0, 0.0, 0.0, 1.0]),
        ease_exit: Ease::CubicBezier([0.3, 0.0, 1.0, 1.0]),
    };

    /// The tokens of a resolved theme; a missing or unusable token keeps its [`Self::SPEC`] value.
    pub fn from_theme(theme: &ResolvedTheme) -> Self {
        let d = Self::SPEC;
        let seconds = |key: &str, fallback: f32| {
            theme
                .duration(key)
                .filter(|s| s.is_finite() && *s >= 0.0)
                .unwrap_or(fallback)
        };
        let ease = |key: &str, fallback: Ease| theme.easing(key).map_or(fallback, ease_of_token);
        Self {
            fast: seconds("motion.fast", d.fast),
            base: seconds("motion.base", d.base),
            slow: seconds("motion.slow", d.slow),
            page: seconds("motion.page", d.page),
            ease_standard: ease("ease.standard", d.ease_standard),
            ease_enter: ease("ease.enter", d.ease_enter),
            ease_exit: ease("ease.exit", d.ease_exit),
        }
    }

    /// The tokens of the theme the resolver has active, or [`Self::SPEC`] before it resolved one.
    pub fn from_resolver(resolver: Option<&ThemeResolver>) -> Self {
        resolver
            .and_then(ThemeResolver::resolved_theme)
            .map_or(Self::SPEC, Self::from_theme)
    }

    /// Seconds of a speed token.
    pub fn duration(&self, speed: MotionSpeed) -> f32 {
        match speed {
            MotionSpeed::Fast => self.fast,
            MotionSpeed::Base => self.base,
            MotionSpeed::Slow => self.slow,
            MotionSpeed::Page => self.page,
        }
    }

    /// Timing of a trigger: enter `speed` with `ease.enter`, exit [`EXIT_DURATION_FACTOR`] ×
    /// `speed` with `ease.exit`, hover and press `motion.fast` with `ease.standard`.
    pub fn timing(&self, trigger: MotionTrigger, speed: MotionSpeed) -> MotionTiming {
        match trigger {
            MotionTrigger::Enter => MotionTiming {
                duration: self.duration(speed),
                ease: self.ease_enter,
            },
            MotionTrigger::Exit => MotionTiming {
                duration: self.duration(speed) * EXIT_DURATION_FACTOR,
                ease: self.ease_exit,
            },
            MotionTrigger::Hover | MotionTrigger::Press => MotionTiming {
                duration: self.fast,
                ease: self.ease_standard,
            },
        }
    }
}

/// The tween curve of a theme easing token.
pub fn ease_of_token(easing: Easing) -> Ease {
    match easing {
        Easing::Linear => Ease::Linear,
        Easing::CubicBezier(points) => Ease::CubicBezier(points),
    }
}

// ---------------------------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------------------------

/// How a node enters and leaves. Every reveal fades opacity between 0 and 1; slide and zoom also
/// move the node from (enter) or to (exit) its hidden end.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RevealPreset {
    /// Opacity only.
    Fade,
    /// Fade plus a translate; the hidden end is this offset in UI px.
    Slide([f32; 2]),
    /// Fade plus a scale about the centre; the hidden end is this scale (`0.96` = 4 % smaller).
    Zoom(f32),
}

impl RevealPreset {
    /// `"fade"`, `"slide"` or `"zoom"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Fade => "fade",
            Self::Slide(_) => "slide",
            Self::Zoom(_) => "zoom",
        }
    }
}

/// A visual pose relative to the node's rest: a translate in UI px and a uniform scale. Hover and
/// press presets move the node's [`crate::UiTransform`] to a pose; layout never sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionPose {
    /// Offset in UI px.
    pub translate: [f32; 2],
    /// Uniform scale about the centre.
    pub scale: f32,
}

impl Default for MotionPose {
    fn default() -> Self {
        Self::REST
    }
}

impl MotionPose {
    /// No offset, scale 1.
    pub const REST: Self = Self {
        translate: [0.0, 0.0],
        scale: 1.0,
    };
    /// Rises 2 UI px (hovered cards).
    pub const LIFT: Self = Self {
        translate: [0.0, -2.0],
        scale: 1.0,
    };
    /// Grows 4 % (hovered tiles and icons).
    pub const GROW: Self = Self {
        translate: [0.0, 0.0],
        scale: 1.04,
    };
    /// Sinks 1 UI px (pressed buttons, spec § 3).
    pub const SINK: Self = Self {
        translate: [0.0, 1.0],
        scale: 1.0,
    };
    /// Shrinks 3 % (pressed tiles and icons).
    pub const SHRINK: Self = Self {
        translate: [0.0, 0.0],
        scale: 0.97,
    };

    /// Every named pose.
    pub const NAMED: [(&'static str, MotionPose); 5] = [
        ("rest", Self::REST),
        ("lift", Self::LIFT),
        ("grow", Self::GROW),
        ("sink", Self::SINK),
        ("shrink", Self::SHRINK),
    ];

    /// Looks a pose up by name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::NAMED.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
    }

    /// The name of a named pose, or `"custom"`.
    pub fn name(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(_, p)| *p == self)
            .map_or("custom", |(n, _)| n)
    }
}

/// What an exit does when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ExitAction {
    /// Set `Display::None`; an enter brings the node back.
    #[default]
    Hide,
    /// Despawn the node and its subtree.
    Despawn,
}

impl ExitAction {
    /// `"hide"` or `"despawn"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hide => "hide",
            Self::Despawn => "despawn",
        }
    }

    /// Looks an action up by name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "hide" => Some(Self::Hide),
            "despawn" => Some(Self::Despawn),
            _ => None,
        }
    }
}

/// A node's motion presets. Adding it (with [`UiMotion::enter_on_add`]) plays the enter; game
/// code asks for an exit or another enter through [`UiMotionState`]; hover and press follow the
/// interaction markers. See the module docs.
#[derive(Component, Debug, Clone, PartialEq)]
#[require(UiMotionState)]
pub struct UiMotion {
    /// How the node appears (`None`: it appears at once).
    pub enter: Option<RevealPreset>,
    /// How the node leaves (`None`: the exit action happens at once).
    pub exit: Option<RevealPreset>,
    /// Pose while hovered, or focused in navigation mode (`None`: no hover motion).
    pub hover: Option<MotionPose>,
    /// Pose while pressed (`None`: no press motion; the hover pose stays).
    pub press: Option<MotionPose>,
    /// Duration token of enter and exit.
    pub speed: MotionSpeed,
    /// What an exit does at its end.
    pub exit_action: ExitAction,
    /// Play the enter when the component is added.
    pub enter_on_add: bool,
}

impl Default for UiMotion {
    fn default() -> Self {
        Self {
            enter: None,
            exit: None,
            hover: None,
            press: None,
            speed: MotionSpeed::Slow,
            exit_action: ExitAction::Hide,
            enter_on_add: true,
        }
    }
}

impl UiMotion {
    /// Names of the preset sets [`Self::preset`] knows.
    pub const PRESETS: [&'static str; 6] = ["button", "card", "popup", "dialog", "toast", "page"];

    /// No presets: the node does not animate.
    pub fn new() -> Self {
        Self::default()
    }

    /// A named preset set:
    ///
    /// | Name | Enter / exit | Hover | Press | Speed | Exit action |
    /// |---|---|---|---|---|---|
    /// | `button` | - | - | sink | - | - |
    /// | `card` | - | lift | sink | - | - |
    /// | `popup` | zoom 0.96 | - | - | slow | hide |
    /// | `dialog` | slide 12 px up | - | - | slow | hide |
    /// | `toast` | slide 24 px up | - | - | slow | despawn |
    /// | `page` | fade | - | - | page | hide |
    pub fn preset(name: &str) -> Option<Self> {
        let base = Self::new();
        Some(match name {
            "button" => base.with_press(MotionPose::SINK),
            "card" => base.with_hover(MotionPose::LIFT).with_press(MotionPose::SINK),
            "popup" => base.with_reveal(RevealPreset::Zoom(0.96)),
            "dialog" => base.with_reveal(RevealPreset::Slide([0.0, 12.0])),
            "toast" => base
                .with_reveal(RevealPreset::Slide([0.0, 24.0]))
                .with_exit_action(ExitAction::Despawn),
            "page" => base.with_reveal(RevealPreset::Fade).with_speed(MotionSpeed::Page),
            _ => return None,
        })
    }

    /// Sets the enter.
    pub fn with_enter(mut self, preset: RevealPreset) -> Self {
        self.enter = Some(preset);
        self
    }

    /// Sets the exit.
    pub fn with_exit(mut self, preset: RevealPreset) -> Self {
        self.exit = Some(preset);
        self
    }

    /// Sets enter and exit to the same reveal.
    pub fn with_reveal(self, preset: RevealPreset) -> Self {
        self.with_enter(preset).with_exit(preset)
    }

    /// Sets the hover pose.
    pub fn with_hover(mut self, pose: MotionPose) -> Self {
        self.hover = Some(pose);
        self
    }

    /// Sets the press pose.
    pub fn with_press(mut self, pose: MotionPose) -> Self {
        self.press = Some(pose);
        self
    }

    /// Sets the enter / exit speed token.
    pub fn with_speed(mut self, speed: MotionSpeed) -> Self {
        self.speed = speed;
        self
    }

    /// Sets what an exit does at its end.
    pub fn with_exit_action(mut self, action: ExitAction) -> Self {
        self.exit_action = action;
        self
    }

    /// Sets whether adding the component plays the enter.
    pub fn with_enter_on_add(mut self, on: bool) -> Self {
        self.enter_on_add = on;
        self
    }
}

/// Where a node with [`UiMotion`] is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MotionPhase {
    /// Visible, not entering or leaving.
    #[default]
    Shown,
    /// The enter is playing.
    Entering,
    /// The exit is playing.
    Exiting,
    /// An exit with [`ExitAction::Hide`] finished.
    Hidden,
}

impl MotionPhase {
    /// `"shown"`, `"entering"`, `"exiting"` or `"hidden"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::Entering => "entering",
            Self::Exiting => "exiting",
            Self::Hidden => "hidden",
        }
    }
}

/// Which interaction pose a node is in (or moving to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PoseKind {
    /// No hover or press.
    #[default]
    Rest,
    /// The hover pose.
    Hover,
    /// The press pose.
    Press,
}

impl PoseKind {
    /// `"rest"`, `"hover"` or `"press"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Hover => "hover",
            Self::Press => "press",
        }
    }
}

/// Runtime state of a [`UiMotion`] node (added with it).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiMotionState {
    /// Where the node is in its life.
    pub phase: MotionPhase,
    /// The interaction pose last started.
    pub pose: PoseKind,
    pending: Option<MotionTrigger>,
    running: Option<MotionTrigger>,
}

impl UiMotionState {
    /// Plays the enter on the next `Update` (from where the node is, if it is leaving).
    pub fn request_enter(&mut self) {
        self.pending = Some(MotionTrigger::Enter);
    }

    /// Plays the exit on the next `Update` (from where the node is).
    pub fn request_exit(&mut self) {
        self.pending = Some(MotionTrigger::Exit);
    }

    /// The enter or exit asked for and not started yet.
    pub fn pending(&self) -> Option<MotionTrigger> {
        self.pending
    }

    /// The preset whose tween is running, if one is.
    pub fn running(&self) -> Option<MotionTrigger> {
        self.running
    }
}

// ---------------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------------

/// Tokens plus the reduced-motion flag: everything a preset tween depends on besides the preset.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotionContext {
    /// The active theme's motion tokens.
    pub tokens: MotionTokens,
    /// Reduced motion is on ([`reduced_motion`]).
    pub reduced: bool,
}

impl MotionContext {
    /// Seconds of the opacity part and of the transform part of a preset that lasts `duration`:
    /// both `duration` normally; under reduced motion the transform jumps (0) and the fade is
    /// capped at `motion.fast` (spec § 2.6).
    pub fn part_durations(&self, duration: f32) -> (f32, f32) {
        if self.reduced {
            (duration.min(self.tokens.fast), 0.0)
        } else {
            (duration, duration)
        }
    }

    /// The enter of `preset` at `speed`. `from_hidden` starts at the hidden end (opacity 0, the
    /// offset or the zoom scale); otherwise it starts where the node is (an exit turned round).
    /// The tween takes the node out of `Display::None` first.
    pub fn enter_tween(&self, preset: RevealPreset, speed: MotionSpeed, from_hidden: bool) -> UiTween {
        let timing = self.tokens.timing(MotionTrigger::Enter, speed);
        let mut tween = self.tween(reveal_plan(
            preset,
            true,
            from_hidden,
            timing,
            self.part_durations(timing.duration),
        ));
        tween.show_on_start = true;
        tween
    }

    /// The exit of `preset` at `speed`, from where the node is. With [`ExitAction::Hide`] the
    /// tween sets `Display::None` at its end; [`run_ui_motion`] despawns for
    /// [`ExitAction::Despawn`].
    pub fn exit_tween(&self, preset: RevealPreset, speed: MotionSpeed, action: ExitAction) -> UiTween {
        let timing = self.tokens.timing(MotionTrigger::Exit, speed);
        let mut tween = self.tween(reveal_plan(
            preset,
            false,
            false,
            timing,
            self.part_durations(timing.duration),
        ));
        tween.hide_on_finish = action == ExitAction::Hide;
        tween
    }

    /// Moves the node's transform from where it is to `pose` (hover / press timing).
    pub fn pose_tween(&self, pose: MotionPose) -> UiTween {
        let timing = self.tokens.timing(MotionTrigger::Hover, MotionSpeed::Fast);
        let (_, seconds) = self.part_durations(timing.duration);
        let part = |property, to: f32| -> TweenPlan {
            Tween::to(property, TweenValue::Number(to), seconds)
                .ease(timing.ease)
                .into()
        };
        self.tween(TweenPlan::Parallel(vec![
            part(TweenProperty::TranslateX, finite_or(pose.translate[0], 0.0)),
            part(TweenProperty::TranslateY, finite_or(pose.translate[1], 0.0)),
            part(TweenProperty::Scale, finite_or(pose.scale, 1.0).max(0.0)),
        ]))
    }

    fn tween(&self, plan: TweenPlan) -> UiTween {
        let mut tween = UiTween::new(plan);
        tween.follows_reduced_motion = self.reduced;
        tween
    }
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

/// The parallel plan of a reveal. `enter` runs hidden → shown, otherwise shown → hidden;
/// `explicit_from` pins the start of an enter at the hidden end (an exit always starts where the
/// node is). `(fade, moves)` are the part durations ([`MotionContext::part_durations`]).
fn reveal_plan(
    preset: RevealPreset,
    enter: bool,
    explicit_from: bool,
    timing: MotionTiming,
    (fade, moves): (f32, f32),
) -> TweenPlan {
    let part = |property: TweenProperty, hidden: f32, shown: f32, seconds: f32| -> TweenPlan {
        let to = if enter { shown } else { hidden };
        let tween = Tween::to(property, TweenValue::Number(to), seconds).ease(timing.ease);
        if enter && explicit_from {
            tween.from(TweenValue::Number(hidden)).into()
        } else {
            tween.into()
        }
    };
    let mut parts = vec![part(TweenProperty::Opacity, 0.0, 1.0, fade)];
    match preset {
        RevealPreset::Fade => {}
        RevealPreset::Slide(offset) => {
            parts.push(part(
                TweenProperty::TranslateX,
                finite_or(offset[0], 0.0),
                0.0,
                moves,
            ));
            parts.push(part(
                TweenProperty::TranslateY,
                finite_or(offset[1], 0.0),
                0.0,
                moves,
            ));
        }
        RevealPreset::Zoom(scale) => {
            parts.push(part(
                TweenProperty::Scale,
                finite_or(scale, 1.0).max(0.0),
                1.0,
                moves,
            ));
        }
    }
    TweenPlan::Parallel(parts)
}

/// The pose a node should be in: none when disabled; the press pose while pressed (if it has
/// one); the hover pose while hovered (not in touch mode) or focused in navigation mode; else rest.
pub fn wanted_pose(
    motion: &UiMotion,
    hovered: bool,
    pressed: bool,
    focused_nav: bool,
    disabled: bool,
) -> PoseKind {
    if disabled {
        PoseKind::Rest
    } else if pressed && motion.press.is_some() {
        PoseKind::Press
    } else if (hovered || focused_nav) && motion.hover.is_some() {
        PoseKind::Hover
    } else {
        PoseKind::Rest
    }
}

// ---------------------------------------------------------------------------------------------
// The system
// ---------------------------------------------------------------------------------------------

/// `Update`, after the widget systems and before [`crate::tween::advance_tweens`]: settles
/// finished presets (exit actions), starts requested enters and exits, and moves nodes between
/// their rest, hover and press poses. A node whose foreign tween (game code, `ui.tween`) is
/// running keeps its pose until that tween ends; enter and exit always replace it.
pub fn run_ui_motion(
    theme: Option<Res<ActiveTheme>>,
    access: Option<Res<UiAccessibility>>,
    resolver: Option<Res<ThemeResolver>>,
    pointer: Option<Res<UiPointer>>,
    mode: Option<Res<UiInputMode>>,
    mut finished: MessageReader<UiTweenFinished>,
    mut done: Local<Vec<Entity>>,
    mut nodes: Query<(
        Entity,
        Ref<UiMotion>,
        &mut UiMotionState,
        Has<UiTween>,
        (Has<Hovered>, Has<Pressed>, Has<Focused>, Has<Disabled>),
        Option<&mut NodeStyle>,
    )>,
    mut commands: Commands,
) {
    done.clear();
    done.extend(finished.read().map(|m| m.entity));
    if nodes.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_motion").entered();
    let touch = pointer.is_some_and(|p| p.active_touch.is_some());
    let navigating = mode.is_some_and(|m| *m == UiInputMode::Navigation);
    let mut cached: Option<MotionContext> = None;
    let mut context = || {
        *cached.get_or_insert_with(|| MotionContext {
            tokens: MotionTokens::from_resolver(resolver.as_deref()),
            reduced: reduced_motion(theme.as_deref(), access.as_deref()),
        })
    };

    for (entity, motion, mut state, tweening, (hovered, pressed, focused, disabled), mut style) in &mut nodes
    {
        // 1. What ran last frame.
        if let Some(trigger) = state.running {
            if done.contains(&entity) {
                state.running = None;
                match trigger {
                    MotionTrigger::Enter => state.phase = MotionPhase::Shown,
                    MotionTrigger::Exit => match motion.exit_action {
                        ExitAction::Hide => state.phase = MotionPhase::Hidden,
                        ExitAction::Despawn => {
                            commands.entity(entity).despawn();
                            continue;
                        }
                    },
                    MotionTrigger::Hover | MotionTrigger::Press => {}
                }
            } else if !tweening {
                // Stopped from outside: the node stays where the tween left it.
                state.running = None;
                if matches!(state.phase, MotionPhase::Entering | MotionPhase::Exiting) {
                    state.phase = MotionPhase::Shown;
                }
            }
        }

        // 2. Enter and exit requests.
        let mut request = if state.pending.is_some() {
            state.pending.take()
        } else {
            None
        };
        if request.is_none() && motion.is_added() && motion.enter_on_add && motion.enter.is_some() {
            request = Some(MotionTrigger::Enter);
        }
        match request {
            Some(MotionTrigger::Enter) => {
                state.pose = PoseKind::Rest;
                match motion.enter {
                    Some(preset) => {
                        // A shown or hidden node replays from the hidden end; one that is moving
                        // turns round where it is.
                        let from_hidden = matches!(state.phase, MotionPhase::Shown | MotionPhase::Hidden);
                        commands.entity(entity).insert(context().enter_tween(
                            preset,
                            motion.speed,
                            from_hidden,
                        ));
                        state.running = Some(MotionTrigger::Enter);
                        state.phase = MotionPhase::Entering;
                    }
                    None => {
                        if let Some(style) = style.as_mut()
                            && style.0.display == Display::None
                        {
                            style.0.display = Display::Flex;
                        }
                        if state.running.is_some() {
                            commands.entity(entity).remove::<UiTween>();
                            state.running = None;
                        }
                        state.phase = MotionPhase::Shown;
                    }
                }
                continue;
            }
            Some(MotionTrigger::Exit) => {
                if state.phase == MotionPhase::Hidden {
                    continue;
                }
                match motion.exit {
                    Some(preset) => {
                        commands.entity(entity).insert(context().exit_tween(
                            preset,
                            motion.speed,
                            motion.exit_action,
                        ));
                        state.running = Some(MotionTrigger::Exit);
                        state.phase = MotionPhase::Exiting;
                    }
                    None => match motion.exit_action {
                        ExitAction::Hide => {
                            if let Some(style) = style.as_mut()
                                && style.0.display != Display::None
                            {
                                style.0.display = Display::None;
                            }
                            if state.running.is_some() {
                                commands.entity(entity).remove::<UiTween>();
                                state.running = None;
                            }
                            state.phase = MotionPhase::Hidden;
                        }
                        ExitAction::Despawn => {
                            commands.entity(entity).despawn();
                        }
                    },
                }
                continue;
            }
            Some(MotionTrigger::Hover | MotionTrigger::Press) | None => {}
        }

        // 3. Hover and press poses.
        if state.phase != MotionPhase::Shown || (tweening && state.running.is_none()) {
            continue;
        }
        let want = wanted_pose(
            &motion,
            hovered && !touch,
            pressed,
            focused && navigating,
            disabled,
        );
        if want != state.pose {
            let pose = match want {
                PoseKind::Rest => MotionPose::REST,
                PoseKind::Hover => motion.hover.unwrap_or(MotionPose::REST),
                PoseKind::Press => motion.press.unwrap_or(MotionPose::REST),
            };
            commands.entity(entity).insert(context().pose_tween(pose));
            state.running = Some(if want == PoseKind::Press {
                MotionTrigger::Press
            } else {
                MotionTrigger::Hover
            });
            state.pose = want;
        }
    }
}

/// Adds [`run_ui_motion`]. [`crate::UiPlugin`] adds it after the tween plugin.
pub struct UiMotionPlugin;

impl Plugin for UiMotionPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            run_ui_motion
                .after(crate::scroll::ui_scroll)
                .before(crate::tween::advance_tweens),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme_file::ThemeSet;

    const EPS: f32 = 1e-6;

    fn parts(tween: &UiTween) -> Vec<(TweenProperty, Option<f32>, f32, f32, Ease)> {
        tween
            .plan
            .tweens()
            .into_iter()
            .map(|t| {
                let number = |v: TweenValue| match v {
                    TweenValue::Number(n) => n,
                    TweenValue::Color(_) => f32::NAN,
                };
                (t.property, t.from.map(number), number(t.to), t.duration, t.ease)
            })
            .collect()
    }

    fn part(tween: &UiTween, property: TweenProperty) -> (Option<f32>, f32, f32, Ease) {
        let (_, from, to, duration, ease) = parts(tween)
            .into_iter()
            .find(|p| p.0 == property)
            .unwrap_or_else(|| panic!("no {property:?} part"));
        (from, to, duration, ease)
    }

    #[test]
    fn spec_tokens_match_the_built_in_themes() {
        for name in ["regia-dark", "regia-light", "regia-high-contrast"] {
            let theme = ThemeSet::with_builtins().resolve(name, None).expect(name);
            assert_eq!(MotionTokens::from_theme(&theme), MotionTokens::SPEC, "{name}");
        }
        assert_eq!(MotionTokens::from_resolver(None), MotionTokens::SPEC);
        assert_eq!(MotionTokens::default(), MotionTokens::SPEC);
    }

    #[test]
    fn a_child_theme_retimes_the_presets() {
        let mut set = ThemeSet::with_builtins();
        set.load_ron(
            r#"(format_version: 1, name: "slow-motion", extends: "regia-dark",
                tokens: {"motion.slow": "400ms", "motion.fast": "50ms", "ease.enter": "linear"})"#,
            "slow-motion",
        )
        .expect("loads");
        let tokens = MotionTokens::from_theme(&set.resolve("slow-motion", None).expect("resolves"));
        assert!((tokens.slow - 0.4).abs() < EPS);
        assert!((tokens.fast - 0.05).abs() < EPS);
        assert_eq!(tokens.ease_enter, Ease::Linear);
        assert_eq!(tokens.base, MotionTokens::SPEC.base, "inherited");
        assert_eq!(tokens.ease_exit, MotionTokens::SPEC.ease_exit, "inherited");
        let ctx = MotionContext {
            tokens,
            reduced: false,
        };
        let enter = ctx.enter_tween(RevealPreset::Fade, MotionSpeed::Slow, true);
        assert!((enter.duration() - 0.4).abs() < EPS);
        assert!((ctx.pose_tween(MotionPose::LIFT).duration() - 0.05).abs() < EPS);
    }

    #[test]
    fn timing_follows_the_spec_table() {
        let t = MotionTokens::SPEC;
        let enter = t.timing(MotionTrigger::Enter, MotionSpeed::Slow);
        assert_eq!((enter.duration, enter.ease), (0.24, t.ease_enter));
        let exit = t.timing(MotionTrigger::Exit, MotionSpeed::Slow);
        assert!((exit.duration - 0.18).abs() < EPS, "0.75 x motion.slow");
        assert_eq!(exit.ease, t.ease_exit);
        let page = t.timing(MotionTrigger::Exit, MotionSpeed::Page);
        assert!((page.duration - 0.24).abs() < EPS, "0.75 x motion.page");
        for trigger in [MotionTrigger::Hover, MotionTrigger::Press] {
            let pose = t.timing(trigger, MotionSpeed::Page);
            assert_eq!((pose.duration, pose.ease), (0.08, t.ease_standard), "{trigger:?}");
        }
        for speed in MotionSpeed::ALL {
            assert_eq!(MotionSpeed::parse(speed.name()), Some(speed));
            assert!(speed.token().ends_with(speed.name()));
        }
        for trigger in MotionTrigger::ALL {
            assert_eq!(MotionTrigger::parse(trigger.name()), Some(trigger));
        }
        assert_eq!(ease_of_token(Easing::Linear), Ease::Linear);
        assert_eq!(
            ease_of_token(Easing::CubicBezier([0.1, 0.2, 0.3, 0.4])),
            Ease::CubicBezier([0.1, 0.2, 0.3, 0.4])
        );
    }

    #[test]
    fn enter_plans_run_from_the_hidden_end_to_rest() {
        let ctx = MotionContext::default();
        let slide = ctx.enter_tween(RevealPreset::Slide([0.0, 24.0]), MotionSpeed::Slow, true);
        assert!(slide.show_on_start && !slide.hide_on_finish && !slide.follows_reduced_motion);
        assert_eq!(
            part(&slide, TweenProperty::Opacity),
            (Some(0.0), 1.0, 0.24, MotionTokens::SPEC.ease_enter)
        );
        assert_eq!(part(&slide, TweenProperty::TranslateY).0, Some(24.0));
        assert_eq!(part(&slide, TweenProperty::TranslateY).1, 0.0);
        assert_eq!(part(&slide, TweenProperty::TranslateX).1, 0.0);
        let zoom = ctx.enter_tween(RevealPreset::Zoom(0.96), MotionSpeed::Base, true);
        assert_eq!(
            part(&zoom, TweenProperty::Scale),
            (Some(0.96), 1.0, 0.16, MotionTokens::SPEC.ease_enter)
        );
        let fade = ctx.enter_tween(RevealPreset::Fade, MotionSpeed::Slow, true);
        assert_eq!(fade.plan.properties(), vec![TweenProperty::Opacity]);
        // Turning round mid-exit starts where the node is.
        let turn = ctx.enter_tween(RevealPreset::Zoom(0.9), MotionSpeed::Slow, false);
        assert!(parts(&turn).iter().all(|p| p.1.is_none()), "open starts");
    }

    #[test]
    fn exit_plans_start_where_the_node_is_and_carry_the_action() {
        let ctx = MotionContext::default();
        let hide = ctx.exit_tween(
            RevealPreset::Slide([0.0, 24.0]),
            MotionSpeed::Slow,
            ExitAction::Hide,
        );
        assert!(hide.hide_on_finish && !hide.show_on_start);
        assert!(parts(&hide).iter().all(|p| p.1.is_none()), "open starts");
        let (_, to, seconds, ease) = part(&hide, TweenProperty::Opacity);
        assert_eq!(to, 0.0);
        assert!((seconds - 0.18).abs() < EPS);
        assert_eq!(ease, MotionTokens::SPEC.ease_exit);
        assert_eq!(part(&hide, TweenProperty::TranslateY).1, 24.0);
        let despawn = ctx.exit_tween(RevealPreset::Zoom(0.5), MotionSpeed::Slow, ExitAction::Despawn);
        assert!(
            !despawn.hide_on_finish,
            "run_ui_motion despawns, the tween does not hide"
        );
        assert_eq!(part(&despawn, TweenProperty::Scale).1, 0.5);
    }

    #[test]
    fn reduced_motion_jumps_transforms_and_caps_fades_at_motion_fast() {
        let ctx = MotionContext {
            tokens: MotionTokens::SPEC,
            reduced: true,
        };
        assert_eq!(ctx.part_durations(0.24), (0.08, 0.0));
        assert_eq!(ctx.part_durations(0.05), (0.05, 0.0), "a short fade is kept");
        let enter = ctx.enter_tween(RevealPreset::Slide([0.0, 24.0]), MotionSpeed::Page, true);
        assert!(enter.follows_reduced_motion);
        assert!((part(&enter, TweenProperty::Opacity).2 - 0.08).abs() < EPS);
        assert_eq!(part(&enter, TweenProperty::TranslateY).2, 0.0);
        assert!((enter.duration() - 0.08).abs() < EPS);
        let exit = ctx.exit_tween(RevealPreset::Zoom(0.9), MotionSpeed::Slow, ExitAction::Hide);
        assert_eq!(part(&exit, TweenProperty::Scale).2, 0.0);
        let pose = ctx.pose_tween(MotionPose::SINK);
        assert_eq!(pose.duration(), 0.0, "hover and press poses jump");
        let full = MotionContext::default();
        assert_eq!(full.part_durations(0.24), (0.24, 0.24), "off: unchanged");
    }

    #[test]
    fn pose_tweens_move_the_transform_from_where_it_is() {
        let ctx = MotionContext::default();
        let lift = ctx.pose_tween(MotionPose::LIFT);
        assert!(parts(&lift).iter().all(|p| p.1.is_none()), "open starts");
        assert_eq!(
            lift.plan.properties(),
            vec![
                TweenProperty::TranslateX,
                TweenProperty::TranslateY,
                TweenProperty::Scale
            ]
        );
        assert_eq!(part(&lift, TweenProperty::TranslateY).1, -2.0);
        assert_eq!(part(&lift, TweenProperty::Scale).1, 1.0);
        assert_eq!(
            part(&lift, TweenProperty::Scale).3,
            MotionTokens::SPEC.ease_standard
        );
        let odd = ctx.pose_tween(MotionPose {
            translate: [f32::NAN, 3.0],
            scale: -2.0,
        });
        assert_eq!(part(&odd, TweenProperty::TranslateX).1, 0.0, "non-finite is rest");
        assert_eq!(part(&odd, TweenProperty::Scale).1, 0.0, "negative scale clamps");
    }

    #[test]
    fn wanted_pose_follows_the_state_order() {
        let card = UiMotion::preset("card").expect("card");
        let hover_only = UiMotion::new().with_hover(MotionPose::GROW);
        let none = UiMotion::new();
        // (hovered, pressed, focused in navigation, disabled)
        assert_eq!(wanted_pose(&card, false, false, false, false), PoseKind::Rest);
        assert_eq!(wanted_pose(&card, true, false, false, false), PoseKind::Hover);
        assert_eq!(wanted_pose(&card, false, false, true, false), PoseKind::Hover);
        assert_eq!(wanted_pose(&card, true, true, false, false), PoseKind::Press);
        assert_eq!(
            wanted_pose(&card, true, true, false, true),
            PoseKind::Rest,
            "disabled"
        );
        assert_eq!(
            wanted_pose(&hover_only, true, true, false, false),
            PoseKind::Hover,
            "no press pose keeps hover"
        );
        assert_eq!(wanted_pose(&none, true, true, true, false), PoseKind::Rest);
    }

    #[test]
    fn preset_sets_and_names_round_trip() {
        for name in UiMotion::PRESETS {
            assert!(UiMotion::preset(name).is_some(), "{name}");
        }
        assert_eq!(UiMotion::preset("wobble"), None);
        let toast = UiMotion::preset("toast").expect("toast");
        assert_eq!(toast.exit_action, ExitAction::Despawn);
        assert_eq!(toast.enter, toast.exit);
        assert_eq!(
            UiMotion::preset("button").expect("button").press,
            Some(MotionPose::SINK)
        );
        assert_eq!(UiMotion::preset("page").expect("page").speed, MotionSpeed::Page);
        for (name, pose) in MotionPose::NAMED {
            assert_eq!(MotionPose::parse(name), Some(pose));
            assert_eq!(pose.name(), name);
        }
        assert_eq!(
            MotionPose {
                translate: [1.0, 1.0],
                scale: 2.0
            }
            .name(),
            "custom"
        );
        for action in [ExitAction::Hide, ExitAction::Despawn] {
            assert_eq!(ExitAction::parse(action.name()), Some(action));
        }
        assert!(UiMotion::default().enter_on_add);
    }

    #[test]
    fn either_flag_turns_reduced_motion_on() {
        let on_theme = ActiveTheme {
            reduced_motion: true,
            ..ActiveTheme::default()
        };
        let on_access = UiAccessibility {
            reduced_motion: true,
            ..UiAccessibility::default()
        };
        assert!(!reduced_motion(None, None));
        assert!(!reduced_motion(
            Some(&ActiveTheme::default()),
            Some(&UiAccessibility::default())
        ));
        assert!(reduced_motion(Some(&on_theme), None));
        assert!(reduced_motion(None, Some(&on_access)));
        assert!(reduced_motion(Some(&on_theme), Some(&on_access)));
    }

    #[test]
    fn requests_are_recorded_until_the_system_takes_them() {
        let mut state = UiMotionState::default();
        assert_eq!(state.pending(), None);
        state.request_exit();
        assert_eq!(state.pending(), Some(MotionTrigger::Exit));
        state.request_enter();
        assert_eq!(
            state.pending(),
            Some(MotionTrigger::Enter),
            "the last request wins"
        );
        assert_eq!(state.running(), None);
        assert_eq!(state.phase, MotionPhase::Shown);
    }
}
