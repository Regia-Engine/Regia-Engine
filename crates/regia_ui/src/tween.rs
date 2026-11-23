// SPDX-License-Identifier: MPL-2.0
//! Property tweens and the layout-neutral visual transform (ADR-273 Lane 3, ADR-343).
//!
//! Two independent pieces:
//!
//! - [`UiTween`] animates a node's [`Opacity`], background / border / text colour, `inset`, width and
//!   height, and its [`UiTransform`]. A tween is a [`TweenPlan`] tree (single [`Tween`]s, sequences,
//!   parallel groups). [`TweenPlan::sample`] is a pure function from a time to property values, like
//!   `ShotSequence::sample`; [`advance_tweens`] only feeds it the game clock (`Time`, so pause, time
//!   scale and a fixed step reproduce) and writes the result.
//! - [`UiTransform`] (translate, uniform scale, pivot ratio) is visual only: layout never sees it.
//!   [`apply_ui_transforms`] runs after layout and maps the already computed [`ComputedNode`] rects of
//!   the node and its subtree on the CPU, so the draw emission is unchanged and hit testing, focus
//!   navigation and the agent all see the transformed rect. [`UiLayoutBox`] remembers the untransformed
//!   rect, and [`UiTransform::hit_test`] can switch hit testing back to it.
//!
//! Reduced motion (`ActiveTheme::reduced_motion` or `UiAccessibility::reduced_motion`, see
//! [`crate::motion::reduced_motion`]) jumps every tween to its end value, except a tween whose
//! plan already follows the reduced-motion rule ([`UiTween::follows_reduced_motion`], the motion
//! presets of [`crate::motion`]).
//!
//! See `book/agents/ui-animation.md`.

use std::collections::HashSet;

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::node::{Background, BackgroundKind, Border, ComputedNode, NodeStyle, Opacity};
use crate::scale::UiAccessibility;
use crate::style::{Dimension, Display};
use crate::text::{ShapedText, Text};
use crate::theme_resolve::ActiveTheme;

// ---------------------------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------------------------

/// An easing curve: maps linear progress `0..=1` to eased progress (`0` at `0` and `1` at `1`; the
/// back and elastic curves overshoot in between). The in / out / in-out families are the textbook
/// Penner easing equations.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Ease {
    /// Constant speed.
    #[default]
    Linear,
    /// Quadratic, slow start.
    QuadIn,
    /// Quadratic, slow end.
    QuadOut,
    /// Quadratic, slow start and end.
    QuadInOut,
    /// Cubic, slow start.
    CubicIn,
    /// Cubic, slow end.
    CubicOut,
    /// Cubic, slow start and end.
    CubicInOut,
    /// Exponential, slow start.
    ExpoIn,
    /// Exponential, slow end.
    ExpoOut,
    /// Exponential, slow start and end.
    ExpoInOut,
    /// Pulls back before moving forward.
    BackIn,
    /// Overshoots, then settles.
    BackOut,
    /// Pulls back, overshoots, settles.
    BackInOut,
    /// Bounces into the start.
    BounceIn,
    /// Bounces at the end.
    BounceOut,
    /// Bounces at both ends.
    BounceInOut,
    /// Springs into the start.
    ElasticIn,
    /// Springs at the end.
    ElasticOut,
    /// Springs at both ends.
    ElasticInOut,
    /// CSS-style cubic bezier through `(0, 0)` and `(1, 1)` with control points `(x1, y1)` and
    /// `(x2, y2)`; the x values are clamped to `0..=1`.
    CubicBezier([f32; 4]),
}

impl Ease {
    /// Every named curve (everything but [`Ease::CubicBezier`]), in documentation order.
    pub const NAMED: [(&'static str, Ease); 19] = [
        ("linear", Ease::Linear),
        ("quad_in", Ease::QuadIn),
        ("quad_out", Ease::QuadOut),
        ("quad_in_out", Ease::QuadInOut),
        ("cubic_in", Ease::CubicIn),
        ("cubic_out", Ease::CubicOut),
        ("cubic_in_out", Ease::CubicInOut),
        ("expo_in", Ease::ExpoIn),
        ("expo_out", Ease::ExpoOut),
        ("expo_in_out", Ease::ExpoInOut),
        ("back_in", Ease::BackIn),
        ("back_out", Ease::BackOut),
        ("back_in_out", Ease::BackInOut),
        ("bounce_in", Ease::BounceIn),
        ("bounce_out", Ease::BounceOut),
        ("bounce_in_out", Ease::BounceInOut),
        ("elastic_in", Ease::ElasticIn),
        ("elastic_out", Ease::ElasticOut),
        ("elastic_in_out", Ease::ElasticInOut),
    ];

    /// Looks a curve up by its snake_case name (`"quad_in_out"`).
    pub fn parse(name: &str) -> Option<Self> {
        Self::NAMED.iter().find(|(n, _)| *n == name).map(|(_, e)| *e)
    }

    /// The snake_case name, or `"cubic_bezier"` for a bezier.
    pub fn name(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(_, e)| *e == self)
            .map_or("cubic_bezier", |(n, _)| n)
    }

    /// Eased progress for linear progress `t` (clamped to `0..=1`; NaN counts as `0`).
    pub fn apply(self, t: f32) -> f32 {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        match self {
            Self::Linear => t,
            Self::QuadIn => t * t,
            Self::QuadOut => 1.0 - (1.0 - t) * (1.0 - t),
            Self::QuadInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Self::CubicIn => t * t * t,
            Self::CubicOut => 1.0 - (1.0 - t).powi(3),
            Self::CubicInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Self::ExpoIn => {
                if t == 0.0 {
                    0.0
                } else {
                    2f32.powf(10.0 * t - 10.0)
                }
            }
            Self::ExpoOut => {
                if t == 1.0 {
                    1.0
                } else {
                    1.0 - 2f32.powf(-10.0 * t)
                }
            }
            Self::ExpoInOut => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    2f32.powf(20.0 * t - 10.0) / 2.0
                } else {
                    (2.0 - 2f32.powf(-20.0 * t + 10.0)) / 2.0
                }
            }
            Self::BackIn => {
                let c3 = BACK_OVERSHOOT + 1.0;
                c3 * t * t * t - BACK_OVERSHOOT * t * t
            }
            Self::BackOut => {
                let c3 = BACK_OVERSHOOT + 1.0;
                1.0 + c3 * (t - 1.0).powi(3) + BACK_OVERSHOOT * (t - 1.0).powi(2)
            }
            Self::BackInOut => {
                let c2 = BACK_OVERSHOOT * 1.525;
                if t < 0.5 {
                    ((2.0 * t).powi(2) * ((c2 + 1.0) * 2.0 * t - c2)) / 2.0
                } else {
                    ((2.0 * t - 2.0).powi(2) * ((c2 + 1.0) * (t * 2.0 - 2.0) + c2) + 2.0) / 2.0
                }
            }
            Self::BounceIn => 1.0 - bounce_out(1.0 - t),
            Self::BounceOut => bounce_out(t),
            Self::BounceInOut => {
                if t < 0.5 {
                    (1.0 - bounce_out(1.0 - 2.0 * t)) / 2.0
                } else {
                    (1.0 + bounce_out(2.0 * t - 1.0)) / 2.0
                }
            }
            Self::ElasticIn => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    -(2f32.powf(10.0 * t - 10.0)) * ((t * 10.0 - 10.75) * ELASTIC_C4).sin()
                }
            }
            Self::ElasticOut => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else {
                    2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * ELASTIC_C4).sin() + 1.0
                }
            }
            Self::ElasticInOut => {
                if t == 0.0 {
                    0.0
                } else if t == 1.0 {
                    1.0
                } else if t < 0.5 {
                    -(2f32.powf(20.0 * t - 10.0) * ((20.0 * t - 11.125) * ELASTIC_C5).sin()) / 2.0
                } else {
                    2f32.powf(-20.0 * t + 10.0) * ((20.0 * t - 11.125) * ELASTIC_C5).sin() / 2.0 + 1.0
                }
            }
            Self::CubicBezier([x1, y1, x2, y2]) => bezier_ease(x1, y1, x2, y2, t),
        }
    }
}

/// Overshoot constant of the back curves (the textbook value).
const BACK_OVERSHOOT: f32 = 1.70158;
/// Period constants of the elastic curves.
const ELASTIC_C4: f32 = std::f32::consts::TAU / 3.0;
const ELASTIC_C5: f32 = std::f32::consts::TAU / 4.5;

fn bounce_out(t: f32) -> f32 {
    const N1: f32 = 7.5625;
    const D1: f32 = 2.75;
    if t < 1.0 / D1 {
        N1 * t * t
    } else if t < 2.0 / D1 {
        let t = t - 1.5 / D1;
        N1 * t * t + 0.75
    } else if t < 2.5 / D1 {
        let t = t - 2.25 / D1;
        N1 * t * t + 0.9375
    } else {
        let t = t - 2.625 / D1;
        N1 * t * t + 0.984_375
    }
}

/// `y` of the cubic bezier `(0,0) (x1,y1) (x2,y2) (1,1)` at `x`, found by bisection on the curve
/// parameter (a fixed iteration count, so the result is deterministic).
fn bezier_ease(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let (x1, x2) = (x1.clamp(0.0, 1.0), x2.clamp(0.0, 1.0));
    let (y1, y2) = (
        if y1.is_finite() { y1 } else { 0.0 },
        if y2.is_finite() { y2 } else { 1.0 },
    );
    let at = |a: f32, b: f32, s: f32| {
        let u = 1.0 - s;
        3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..28 {
        let mid = (lo + hi) * 0.5;
        if at(x1, x2, mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    at(y1, y2, (lo + hi) * 0.5)
}

// ---------------------------------------------------------------------------------------------
// Properties and plans
// ---------------------------------------------------------------------------------------------

/// What a [`Tween`] animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TweenProperty {
    /// [`Opacity`] (number, `0..=1`).
    Opacity,
    /// Solid [`Background`] colour (colour).
    Background,
    /// [`Border`] colour (colour).
    BorderColor,
    /// Text colour (colour). Animates the shaped glyphs and sets every span at the end.
    TextColor,
    /// `Style::inset` left, in UI px (number).
    InsetLeft,
    /// `Style::inset` top, in UI px (number).
    InsetTop,
    /// `Style::inset` right, in UI px (number).
    InsetRight,
    /// `Style::inset` bottom, in UI px (number).
    InsetBottom,
    /// `Style::size` width, in UI px (number).
    Width,
    /// `Style::size` height, in UI px (number).
    Height,
    /// [`UiTransform::translate`] x, in UI px (number).
    TranslateX,
    /// [`UiTransform::translate`] y, in UI px (number).
    TranslateY,
    /// [`UiTransform::scale`] (number).
    Scale,
}

impl TweenProperty {
    /// Every property, in documentation order.
    pub const ALL: [TweenProperty; 13] = [
        Self::Opacity,
        Self::Background,
        Self::BorderColor,
        Self::TextColor,
        Self::InsetLeft,
        Self::InsetTop,
        Self::InsetRight,
        Self::InsetBottom,
        Self::Width,
        Self::Height,
        Self::TranslateX,
        Self::TranslateY,
        Self::Scale,
    ];

    /// The snake_case name used by the agent protocol.
    pub fn name(self) -> &'static str {
        match self {
            Self::Opacity => "opacity",
            Self::Background => "background",
            Self::BorderColor => "border_color",
            Self::TextColor => "text_color",
            Self::InsetLeft => "inset_left",
            Self::InsetTop => "inset_top",
            Self::InsetRight => "inset_right",
            Self::InsetBottom => "inset_bottom",
            Self::Width => "width",
            Self::Height => "height",
            Self::TranslateX => "translate_x",
            Self::TranslateY => "translate_y",
            Self::Scale => "scale",
        }
    }

    /// Looks a property up by its snake_case name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// Whether the property takes a colour (otherwise a number).
    pub fn is_color(self) -> bool {
        matches!(self, Self::Background | Self::BorderColor | Self::TextColor)
    }
}

/// A tweened value: a number or a colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TweenValue {
    /// A number (opacity, px, scale).
    Number(f32),
    /// A linear-space colour.
    Color(UiColor),
}

impl TweenValue {
    /// Interpolates towards `other`; kinds that differ snap to `other` once `t > 0`.
    pub fn lerp(self, other: Self, t: f32) -> Self {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => Self::Number(a + (b - a) * t),
            (Self::Color(a), Self::Color(b)) => Self::Color(a.lerp(b, t)),
            (_, b) => b,
        }
    }

    /// Whether this value fits `property` (colour for colour properties, number otherwise).
    pub fn fits(self, property: TweenProperty) -> bool {
        matches!(self, Self::Color(_)) == property.is_color()
    }
}

/// One property animation: `from` to `to` over `duration` seconds after `delay` seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Tween {
    /// What to animate.
    pub property: TweenProperty,
    /// Start value; `None` takes the value the property has when the tween starts (or the end of
    /// the previous step of a [`TweenPlan::Sequence`] on the same property).
    pub from: Option<TweenValue>,
    /// End value.
    pub to: TweenValue,
    /// Seconds from the end of the delay to the end value (`0` jumps).
    pub duration: f32,
    /// Seconds to wait first; the start value is held meanwhile.
    pub delay: f32,
    /// Easing curve.
    pub ease: Ease,
}

impl Tween {
    /// A tween from the property's current value to `to` (no delay, linear). Negative or
    /// non-finite durations count as `0`.
    pub fn to(property: TweenProperty, to: TweenValue, duration: f32) -> Self {
        Self {
            property,
            from: None,
            to,
            duration: sane_seconds(duration),
            delay: 0.0,
            ease: Ease::Linear,
        }
    }

    /// Sets the start value.
    pub fn from(mut self, from: TweenValue) -> Self {
        self.from = Some(from);
        self
    }

    /// Sets the delay in seconds.
    pub fn delay(mut self, delay: f32) -> Self {
        self.delay = sane_seconds(delay);
        self
    }

    /// Sets the easing curve.
    pub fn ease(mut self, ease: Ease) -> Self {
        self.ease = ease;
        self
    }

    fn total(&self) -> f32 {
        sane_seconds(self.delay) + sane_seconds(self.duration)
    }

    fn sample_at(&self, t: f32) -> TweenValue {
        let from = self.from.unwrap_or(self.to);
        let u = t - sane_seconds(self.delay);
        if u < 0.0 {
            return from;
        }
        let duration = sane_seconds(self.duration);
        if duration <= 0.0 || u >= duration {
            return self.to;
        }
        from.lerp(self.to, self.ease.apply(u / duration))
    }
}

fn sane_seconds(s: f32) -> f32 {
    if s.is_finite() { s.max(0.0) } else { 0.0 }
}

/// A tree of tweens: one tween, steps one after another, or branches at once.
#[derive(Debug, Clone, PartialEq)]
pub enum TweenPlan {
    /// A single property animation.
    Tween(Tween),
    /// Children one after another; a child with no `from` starts where the previous step of the
    /// same property ended.
    Sequence(Vec<TweenPlan>),
    /// Children at the same time; the last child wins when two animate the same property.
    Parallel(Vec<TweenPlan>),
}

/// The properties a plan has written so far, used while binding start values.
type Known = Vec<(TweenProperty, TweenValue)>;

fn set_known(known: &mut Known, property: TweenProperty, value: TweenValue) {
    match known.iter_mut().find(|(p, _)| *p == property) {
        Some(entry) => entry.1 = value,
        None => known.push((property, value)),
    }
}

impl TweenPlan {
    /// Total length in seconds.
    pub fn duration(&self) -> f32 {
        match self {
            Self::Tween(t) => t.total(),
            Self::Sequence(steps) => steps.iter().map(Self::duration).sum(),
            Self::Parallel(branches) => branches.iter().map(Self::duration).fold(0.0, f32::max),
        }
    }

    /// Every property the plan animates (each once, in first-use order).
    pub fn properties(&self) -> Vec<TweenProperty> {
        let mut out = Vec::new();
        self.collect_properties(&mut out);
        out
    }

    fn collect_properties(&self, out: &mut Vec<TweenProperty>) {
        match self {
            Self::Tween(t) => {
                if !out.contains(&t.property) {
                    out.push(t.property);
                }
            }
            Self::Sequence(children) | Self::Parallel(children) => {
                for c in children {
                    c.collect_properties(out);
                }
            }
        }
    }

    /// Fills every open `from` with the property's current value (`current`) or, in a sequence, the
    /// end of the previous step. [`advance_tweens`] calls this once when a tween starts.
    pub fn bind(&mut self, current: &dyn Fn(TweenProperty) -> TweenValue) {
        self.bind_into(&mut Vec::new(), current);
    }

    fn bind_into(&mut self, known: &mut Known, current: &dyn Fn(TweenProperty) -> TweenValue) {
        match self {
            Self::Tween(t) => {
                if t.from.is_none() {
                    let start = known
                        .iter()
                        .find(|(p, _)| *p == t.property)
                        .map_or_else(|| current(t.property), |(_, v)| *v);
                    t.from = Some(start);
                }
                set_known(known, t.property, t.to);
            }
            Self::Sequence(steps) => {
                for s in steps {
                    s.bind_into(known, current);
                }
            }
            Self::Parallel(branches) => {
                let start = known.clone();
                for b in branches {
                    let mut branch = start.clone();
                    b.bind_into(&mut branch, current);
                    for (p, v) in branch {
                        set_known(known, p, v);
                    }
                }
            }
        }
    }

    /// Appends the value of every property the plan has reached at time `t` (seconds since it
    /// started) to `out`. A property a branch has not reached yet is not written; when several
    /// entries name one property the last one is the answer. Pure: same `t`, same output.
    pub fn sample(&self, t: f32, out: &mut Vec<(TweenProperty, TweenValue)>) {
        if t < 0.0 {
            return;
        }
        match self {
            Self::Tween(tween) => {
                out.push((tween.property, tween.sample_at(t)));
            }
            Self::Sequence(steps) => {
                let mut offset = 0.0;
                for step in steps {
                    if t >= offset {
                        step.sample(t - offset, out);
                    }
                    offset += step.duration();
                }
            }
            Self::Parallel(branches) => {
                for b in branches {
                    b.sample(t, out);
                }
            }
        }
    }

    /// Every [`Tween`] of the plan, depth first.
    pub fn tweens(&self) -> Vec<&Tween> {
        let mut out = Vec::new();
        self.collect_tweens(&mut out);
        out
    }

    fn collect_tweens<'a>(&'a self, out: &mut Vec<&'a Tween>) {
        match self {
            Self::Tween(t) => out.push(t),
            Self::Sequence(c) | Self::Parallel(c) => {
                for child in c {
                    child.collect_tweens(out);
                }
            }
        }
    }
}

impl From<Tween> for TweenPlan {
    fn from(t: Tween) -> Self {
        Self::Tween(t)
    }
}

// ---------------------------------------------------------------------------------------------
// The component, show / hide helpers and the completion message
// ---------------------------------------------------------------------------------------------

/// How a node appears or disappears.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reveal {
    /// Opacity only.
    Fade,
    /// Opacity plus a translate from / to this offset in UI px.
    Slide([f32; 2]),
}

/// A running tween on a node. Insert it to start; it removes itself after it sends
/// [`UiTweenFinished`]. Inserting a new one replaces the old one (the node keeps whatever values
/// the old one had written). Needs no setup: missing [`Opacity`], [`UiTransform`], [`Background`]
/// or [`Border`] components are added with neutral values when a tween writes them.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct UiTween {
    /// What to animate.
    pub plan: TweenPlan,
    /// Seconds of game time since the tween started.
    pub elapsed: f32,
    /// Set `Display::None` on the node when the tween ends (a hide).
    pub hide_on_finish: bool,
    /// Take the node out of `Display::None` when the tween starts (a show).
    pub show_on_start: bool,
    /// The plan was built for reduced motion (transforms instant, fades capped at `motion.fast`,
    /// spec § 2.6), so [`advance_tweens`] plays it under reduced motion instead of jumping to its
    /// end. The motion presets set it (ADR-522).
    pub follows_reduced_motion: bool,
    /// Frame events on the plan's timeline, sorted by [`TweenEvent::at`] ([`Self::with_events`]).
    /// [`advance_tweens`] sends a [`UiAnimEventFired`] for each one exactly once, when the elapsed
    /// time first reaches it (ADR-8900).
    pub events: Vec<TweenEvent>,
    /// How many of `events` have fired.
    next_event: usize,
    bound: bool,
}

/// A frame event on a tween's timeline (a `.uianim.ron` v2 [`crate::anim_timeline::AnimEvent`]
/// placed by [`crate::anim_timeline::UiAnimDef::compile_events`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TweenEvent {
    /// Seconds from the tween's start.
    pub at: f32,
    /// The event name.
    pub name: String,
    /// The event payload.
    pub payload: String,
}

impl UiTween {
    /// Starts `plan` at time zero.
    pub fn new(plan: impl Into<TweenPlan>) -> Self {
        Self {
            plan: plan.into(),
            elapsed: 0.0,
            hide_on_finish: false,
            show_on_start: false,
            follows_reduced_motion: false,
            events: Vec::new(),
            next_event: 0,
            bound: false,
        }
    }

    /// The same tween with frame `events` (sorted here by time; equal times keep their order).
    #[must_use]
    pub fn with_events(mut self, mut events: Vec<TweenEvent>) -> Self {
        events.sort_by(|a, b| a.at.total_cmp(&b.at));
        self.events = events;
        self.next_event = 0;
        self
    }

    /// How many of [`Self::events`] have fired so far.
    pub fn events_fired(&self) -> usize {
        self.next_event
    }

    /// Advances the event cursor to `elapsed` (every event when `done`) and returns the range of
    /// [`Self::events`] that playback crossed since the last call: each event is in exactly one
    /// returned range, however far one step jumps.
    pub fn take_crossed_events(&mut self, elapsed: f32, done: bool) -> std::ops::Range<usize> {
        let start = self.next_event;
        let end = if done {
            self.events.len()
        } else {
            start + self.events[start..].partition_point(|e| e.at <= elapsed)
        };
        self.next_event = end;
        start..end
    }

    /// Fade or slide the node in: from transparent (and from the offset) to fully shown, and out of
    /// `Display::None` first.
    pub fn show(how: Reveal, duration: f32, ease: Ease) -> Self {
        let mut parts = vec![
            Tween::to(TweenProperty::Opacity, TweenValue::Number(1.0), duration)
                .from(TweenValue::Number(0.0))
                .ease(ease)
                .into(),
        ];
        if let Reveal::Slide(offset) = how {
            for (property, v) in [TweenProperty::TranslateX, TweenProperty::TranslateY]
                .into_iter()
                .zip(offset)
            {
                parts.push(
                    Tween::to(property, TweenValue::Number(0.0), duration)
                        .from(TweenValue::Number(v))
                        .ease(ease)
                        .into(),
                );
            }
        }
        let mut tween = Self::new(TweenPlan::Parallel(parts));
        tween.show_on_start = true;
        tween
    }

    /// Fade or slide the node out and set `Display::None` at the end.
    pub fn hide(how: Reveal, duration: f32, ease: Ease) -> Self {
        let mut parts = vec![
            Tween::to(TweenProperty::Opacity, TweenValue::Number(0.0), duration)
                .from(TweenValue::Number(1.0))
                .ease(ease)
                .into(),
        ];
        if let Reveal::Slide(offset) = how {
            for (property, v) in [TweenProperty::TranslateX, TweenProperty::TranslateY]
                .into_iter()
                .zip(offset)
            {
                parts.push(
                    Tween::to(property, TweenValue::Number(v), duration)
                        .from(TweenValue::Number(0.0))
                        .ease(ease)
                        .into(),
                );
            }
        }
        let mut tween = Self::new(TweenPlan::Parallel(parts));
        tween.hide_on_finish = true;
        tween
    }

    /// Total length in seconds.
    pub fn duration(&self) -> f32 {
        self.plan.duration()
    }

    /// Progress `0..=1` (`1` for a zero-length plan).
    pub fn progress(&self) -> f32 {
        let total = self.duration();
        if total <= 0.0 {
            1.0
        } else {
            (self.elapsed / total).clamp(0.0, 1.0)
        }
    }
}

/// Sent when a [`UiTween`] ran to its end (not when it is stopped or replaced).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiTweenFinished {
    /// The node that was tweened.
    pub entity: Entity,
}

/// Sent once for each frame event a playing [`UiTween`] crosses (`.uianim.ron` v2, ADR-8900), in
/// timeline order and before that tween's [`UiTweenFinished`]. An event is never sent twice for one
/// crossing and never skipped: a step that jumps over several events (a long frame, reduced motion
/// jumping to the end) sends all of them. A stopped or replaced tween sends no more events.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct UiAnimEventFired {
    /// The node the tween plays on.
    pub entity: Entity,
    /// The event name.
    pub name: String,
    /// The event payload.
    pub payload: String,
    /// The event's time on the tween's timeline, in seconds.
    pub at: f32,
}

/// The most recent [`UiAnimEventFired`] messages (at most [`UiAnimEventLog::CAPACITY`]) and how many
/// were sent in all, for the agent's `ui.play_anim { action: "events" }`.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct UiAnimEventLog {
    /// Oldest first.
    pub recent: std::collections::VecDeque<UiAnimEventFired>,
    /// Every event recorded since the log was made or cleared.
    pub total: u64,
}

impl UiAnimEventLog {
    /// Entries kept.
    pub const CAPACITY: usize = 64;

    /// Appends one event, dropping the oldest past [`Self::CAPACITY`].
    pub fn push(&mut self, event: UiAnimEventFired) {
        if self.recent.len() == Self::CAPACITY {
            self.recent.pop_front();
        }
        self.recent.push_back(event);
        self.total += 1;
    }
}

/// `Update`, after [`advance_tweens`]: copies the frame events into [`UiAnimEventLog`].
pub fn record_anim_events(mut fired: MessageReader<UiAnimEventFired>, mut log: ResMut<UiAnimEventLog>) {
    for event in fired.read() {
        log.push(event.clone());
    }
}

// ---------------------------------------------------------------------------------------------
// Transform
// ---------------------------------------------------------------------------------------------

/// Which rect pointer hit testing uses for a transformed node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitTestSpace {
    /// The transformed rect: you hit what you see.
    #[default]
    Transformed,
    /// The untransformed layout rect: a hovered button that moves away does not lose hover.
    Layout,
}

/// A visual-only transform of a node and its subtree. Layout never sees it: siblings do not move
/// and the layout rects stay put (see [`UiLayoutBox`]). Not rotated yet.
///
/// The scale applies to the boxes (backgrounds, images, children positions and sizes). Border
/// widths, corner radii, shadow blur and glyph size keep their size; they are drawn by the
/// overlay pass this lane does not change.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiTransform {
    /// Offset in UI px (multiplied by the node's UI scale).
    pub translate: [f32; 2],
    /// Uniform scale about the pivot (`1` = none; negative counts as `0`).
    pub scale: f32,
    /// Pivot as a fraction of the node's own layout rect (`[0.5, 0.5]` = its centre).
    pub pivot_ratio: [f32; 2],
    /// Rect used for pointer hit testing.
    pub hit_test: HitTestSpace,
}

impl Default for UiTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl UiTransform {
    /// No translate, scale 1, pivot at the centre.
    pub const IDENTITY: Self = Self {
        translate: [0.0; 2],
        scale: 1.0,
        pivot_ratio: [0.5; 2],
        hit_test: HitTestSpace::Transformed,
    };

    /// Whether the transform moves nothing.
    pub fn is_identity(&self) -> bool {
        self.translate == [0.0; 2] && self.scale == 1.0
    }
}

/// The untransformed layout result of a node under a [`UiTransform`] (its own or an ancestor's).
/// Kept so the transform is re-applied from the layout rect every time, never compounded.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiLayoutBox {
    /// What layout wrote.
    pub base: ComputedNode,
    /// What [`apply_ui_transforms`] wrote over it.
    pub applied: ComputedNode,
    /// The node's [`UiTransform::hit_test`] is [`HitTestSpace::Layout`].
    pub hit_layout: bool,
}

/// The rect and clip rect pointer hit testing should use for a node.
pub fn hit_rects(computed: &ComputedNode, layout: Option<&UiLayoutBox>) -> ([f32; 4], [f32; 4]) {
    match layout {
        Some(b) if b.hit_layout => (b.base.rect, b.base.clip_rect),
        _ => (computed.rect, computed.clip_rect),
    }
}

/// `p' = a * p + b` on both axes: what a chain of uniform scales and translations collapses to.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Affine {
    a: f32,
    b: [f32; 2],
}

const NO_CLIP: [f32; 4] = [f32::MIN / 4.0, f32::MIN / 4.0, f32::MAX, f32::MAX];

impl Affine {
    const IDENTITY: Self = Self { a: 1.0, b: [0.0; 2] };

    /// The map of `t` about its pivot on `base` (layout px).
    fn of(t: &UiTransform, base: &ComputedNode) -> Self {
        let s = if t.scale.is_finite() {
            t.scale.max(0.0)
        } else {
            1.0
        };
        let pivot = [
            base.rect[0] + t.pivot_ratio[0] * base.rect[2],
            base.rect[1] + t.pivot_ratio[1] * base.rect[3],
        ];
        let k = if base.scale.is_finite() && base.scale > 0.0 {
            base.scale
        } else {
            1.0
        };
        let tr = |v: f32| if v.is_finite() { v * k } else { 0.0 };
        Self {
            a: s,
            b: [
                pivot[0] * (1.0 - s) + tr(t.translate[0]),
                pivot[1] * (1.0 - s) + tr(t.translate[1]),
            ],
        }
    }

    /// `self` applied after `inner`.
    fn after(self, inner: Self) -> Self {
        Self {
            a: self.a * inner.a,
            b: [self.a * inner.b[0] + self.b[0], self.a * inner.b[1] + self.b[1]],
        }
    }

    fn is_identity(&self) -> bool {
        self.a == 1.0 && self.b == [0.0; 2]
    }

    fn rect(&self, r: [f32; 4]) -> [f32; 4] {
        [
            self.a * r[0] + self.b[0],
            self.a * r[1] + self.b[1],
            self.a * r[2],
            self.a * r[3],
        ]
    }

    /// Like [`Affine::rect`] but leaves the "no clip" sentinel (huge) alone.
    fn clip(&self, r: [f32; 4]) -> [f32; 4] {
        if self.is_identity() || r[2] >= f32::MAX / 2.0 {
            r
        } else {
            self.rect(r)
        }
    }
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// `PostUpdate`, after layout and before shaping: maps the [`ComputedNode`] of every node under a
/// [`UiTransform`] (see the module docs). Idle UIs pay one cheap query check; the work runs when a
/// transform changed, layout rewrote a rect, or a transform was removed.
pub fn apply_ui_transforms(
    transforms: Query<(Entity, Ref<UiTransform>)>,
    boxes: Query<(Entity, &UiLayoutBox)>,
    mut nodes: Query<&mut ComputedNode>,
    children: Query<&Children>,
    parents: Query<&ChildOf>,
    mut removed: RemovedComponents<UiTransform>,
    mut commands: Commands,
    mut counts: Option<ResMut<crate::layout::UiHotPathCounts>>,
) {
    let removed_any = removed.read().count() > 0;
    if transforms.is_empty() && boxes.is_empty() {
        return;
    }
    let dirty = removed_any || transforms.iter().any(|(_, t)| t.is_changed()) || {
        // The scan of every `ComputedNode` an idle frame pays while any transform exists.
        if let Some(counts) = counts.as_deref_mut() {
            counts.transform_node_scans += 1;
        }
        nodes.iter_mut().any(|c| c.is_changed())
    };
    if !dirty {
        return;
    }
    let _span = tracing::trace_span!("ui_apply_transforms").entered();

    let has_transform_ancestor = |entity: Entity| {
        let mut cur = parents.get(entity).ok().map(ChildOf::parent);
        while let Some(p) = cur {
            if transforms.contains(p) {
                return true;
            }
            cur = parents.get(p).ok().map(ChildOf::parent);
        }
        false
    };

    let mut visited: HashSet<Entity> = HashSet::new();
    let mut stack: Vec<(Entity, Affine, [f32; 4])> = Vec::new();
    for (entity, _) in &transforms {
        if has_transform_ancestor(entity) {
            continue;
        }
        let parent_clip = parents
            .get(entity)
            .ok()
            .and_then(|p| nodes.get(p.parent()).ok())
            .map_or(NO_CLIP, |c| c.clip_rect);
        stack.push((entity, Affine::IDENTITY, parent_clip));
    }

    while let Some((entity, ancestors, parent_clip)) = stack.pop() {
        visited.insert(entity);
        let own = transforms.get(entity).ok().map(|(_, t)| *t);
        let mut chain = ancestors;
        let mut child_parent_clip = parent_clip;
        if let Ok(mut computed) = nodes.get_mut(entity) {
            let existing = boxes.get(entity).ok().map(|(_, b)| *b);
            // Layout rewrote the rect (or never did) unless it still holds what we wrote.
            let base = match existing {
                Some(b) if *computed == b.applied => b.base,
                _ => *computed,
            };
            let total = match &own {
                Some(t) => ancestors.after(Affine::of(t, &base)),
                None => ancestors,
            };
            let rect = total.rect(base.rect);
            let clip = if base.clip_rect == parent_clip {
                ancestors.clip(parent_clip)
            } else {
                intersect(ancestors.clip(parent_clip), rect)
            };
            if total.is_identity() {
                if *computed != base {
                    *computed = base;
                }
                if existing.is_some() {
                    commands.entity(entity).remove::<UiLayoutBox>();
                }
            } else {
                let applied = ComputedNode {
                    rect,
                    clip_rect: clip,
                    ..base
                };
                if *computed != applied {
                    *computed = applied;
                }
                let layout_box = UiLayoutBox {
                    base,
                    applied,
                    hit_layout: own.is_some_and(|t| t.hit_test == HitTestSpace::Layout),
                };
                if existing != Some(layout_box) {
                    commands.entity(entity).insert(layout_box);
                }
            }
            chain = total;
            child_parent_clip = base.clip_rect;
        }
        if let Ok(kids) = children.get(entity) {
            for kid in kids.iter() {
                stack.push((kid, chain, child_parent_clip));
            }
        }
    }

    // Nodes that left every transform: give layout's rect back.
    for (entity, layout_box) in &boxes {
        if visited.contains(&entity) {
            continue;
        }
        if let Ok(mut computed) = nodes.get_mut(entity)
            && *computed == layout_box.applied
        {
            *computed = layout_box.base;
        }
        commands.entity(entity).remove::<UiLayoutBox>();
    }
}

// ---------------------------------------------------------------------------------------------
// Advancing tweens
// ---------------------------------------------------------------------------------------------

/// `Update`, after the widget systems: advances every [`UiTween`] by the game clock's delta and
/// writes the sampled values. Nodes without a tween are never visited. A property is only written
/// when its value changed, so a paused clock marks nothing.
pub fn advance_tweens(
    time: Res<Time>,
    theme: Option<Res<ActiveTheme>>,
    access: Option<Res<UiAccessibility>>,
    mut finished: MessageWriter<UiTweenFinished>,
    mut anim_events: MessageWriter<UiAnimEventFired>,
    mut commands: Commands,
    mut samples: Local<Vec<(TweenProperty, TweenValue)>>,
    mut tweens: Query<(
        Entity,
        &mut UiTween,
        Option<&mut Opacity>,
        Option<&mut Background>,
        Option<&mut Border>,
        Option<&mut NodeStyle>,
        Option<&mut UiTransform>,
        Option<&mut Text>,
        Option<&mut ShapedText>,
        Option<&ComputedNode>,
        Option<&UiLayoutBox>,
    )>,
) {
    if tweens.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_advance_tweens").entered();
    let dt = time.delta_secs();
    let reduced = crate::motion::reduced_motion(theme.as_deref(), access.as_deref());
    for (
        entity,
        mut tween,
        mut opacity,
        mut background,
        mut border,
        mut style,
        mut transform,
        mut text,
        mut shaped,
        computed,
        layout_box,
    ) in &mut tweens
    {
        if !tween.bound {
            if tween.show_on_start
                && let Some(style) = style.as_mut()
                && style.0.display == Display::None
            {
                style.0.display = Display::Flex;
            }
            let layout = layout_box.map_or(computed.copied(), |b| Some(b.base));
            let size = layout.map_or([0.0; 2], |c| {
                let k = if c.scale > 0.0 { c.scale } else { 1.0 };
                [c.rect[2] / k, c.rect[3] / k]
            });
            let current = |p: TweenProperty| {
                let dim = |d: Dimension, fallback: f32| match d {
                    Dimension::Points(v) => v,
                    _ => fallback,
                };
                let edges = style.as_ref().map(|s| s.0.inset);
                let number = |v: f32| TweenValue::Number(v);
                match p {
                    TweenProperty::Opacity => number(opacity.as_ref().map_or(1.0, |o| o.0)),
                    TweenProperty::Background => {
                        TweenValue::Color(match background.as_ref().map(|b| &b.kind) {
                            Some(BackgroundKind::Color(c)) => *c,
                            Some(BackgroundKind::Gradient { start, .. }) => *start,
                            _ => UiColor::TRANSPARENT,
                        })
                    }
                    TweenProperty::BorderColor => {
                        TweenValue::Color(border.as_ref().map_or(UiColor::TRANSPARENT, |b| b.color))
                    }
                    TweenProperty::TextColor => TweenValue::Color(
                        text.as_ref()
                            .and_then(|t| t.spans.first())
                            .map_or(UiColor::WHITE, |s| s.style.color),
                    ),
                    TweenProperty::InsetLeft => number(edges.map_or(0.0, |e| dim(e.left, 0.0))),
                    TweenProperty::InsetTop => number(edges.map_or(0.0, |e| dim(e.top, 0.0))),
                    TweenProperty::InsetRight => number(edges.map_or(0.0, |e| dim(e.right, 0.0))),
                    TweenProperty::InsetBottom => number(edges.map_or(0.0, |e| dim(e.bottom, 0.0))),
                    TweenProperty::Width => {
                        number(style.as_ref().map_or(size[0], |s| dim(s.0.size[0], size[0])))
                    }
                    TweenProperty::Height => {
                        number(style.as_ref().map_or(size[1], |s| dim(s.0.size[1], size[1])))
                    }
                    TweenProperty::TranslateX => number(transform.as_ref().map_or(0.0, |t| t.translate[0])),
                    TweenProperty::TranslateY => number(transform.as_ref().map_or(0.0, |t| t.translate[1])),
                    TweenProperty::Scale => number(transform.as_ref().map_or(1.0, |t| t.scale)),
                }
            };
            tween.plan.bind(&current);
            tween.bound = true;
        }

        let total = tween.plan.duration();
        tween.elapsed = if reduced && !tween.follows_reduced_motion {
            total
        } else {
            (tween.elapsed + dt).min(total)
        };
        let done = tween.elapsed >= total;
        if !tween.events.is_empty() {
            let elapsed = tween.elapsed;
            for i in tween.take_crossed_events(elapsed, done) {
                let e = &tween.events[i];
                anim_events.write(UiAnimEventFired {
                    entity,
                    name: e.name.clone(),
                    payload: e.payload.clone(),
                    at: e.at,
                });
            }
        }

        samples.clear();
        tween.plan.sample(tween.elapsed, &mut samples);

        let mut new_transform: Option<UiTransform> = None;
        for &(property, value) in samples.iter() {
            match (property, value) {
                (TweenProperty::Opacity, TweenValue::Number(v)) => {
                    let v = v.clamp(0.0, 1.0);
                    match opacity.as_mut() {
                        Some(o) => {
                            if o.0 != v {
                                o.0 = v;
                            }
                        }
                        None => {
                            commands.entity(entity).insert(Opacity(v));
                        }
                    }
                }
                (TweenProperty::Background, TweenValue::Color(c)) => match background.as_mut() {
                    Some(b) => {
                        if b.kind != BackgroundKind::Color(c) {
                            b.kind = BackgroundKind::Color(c);
                        }
                    }
                    None => {
                        commands.entity(entity).insert(Background {
                            kind: BackgroundKind::Color(c),
                        });
                    }
                },
                (TweenProperty::BorderColor, TweenValue::Color(c)) => match border.as_mut() {
                    Some(b) => {
                        if b.color != c {
                            b.color = c;
                        }
                    }
                    None => {
                        commands.entity(entity).insert(Border {
                            color: c,
                            ..Border::default()
                        });
                    }
                },
                (TweenProperty::TextColor, TweenValue::Color(c)) => {
                    if let Some(shaped) = shaped.as_mut()
                        && shaped.glyphs.iter().any(|g| g.color != c)
                    {
                        for g in &mut shaped.glyphs {
                            g.color = c;
                        }
                    }
                }
                (
                    TweenProperty::InsetLeft
                    | TweenProperty::InsetTop
                    | TweenProperty::InsetRight
                    | TweenProperty::InsetBottom
                    | TweenProperty::Width
                    | TweenProperty::Height,
                    TweenValue::Number(v),
                ) => {
                    let Some(style) = style.as_mut() else {
                        continue;
                    };
                    let value = Dimension::Points(v);
                    let slot: &Dimension = match property {
                        TweenProperty::InsetLeft => &style.0.inset.left,
                        TweenProperty::InsetTop => &style.0.inset.top,
                        TweenProperty::InsetRight => &style.0.inset.right,
                        TweenProperty::InsetBottom => &style.0.inset.bottom,
                        TweenProperty::Width => &style.0.size[0],
                        _ => &style.0.size[1],
                    };
                    if *slot != value {
                        match property {
                            TweenProperty::InsetLeft => style.0.inset.left = value,
                            TweenProperty::InsetTop => style.0.inset.top = value,
                            TweenProperty::InsetRight => style.0.inset.right = value,
                            TweenProperty::InsetBottom => style.0.inset.bottom = value,
                            TweenProperty::Width => style.0.size[0] = value,
                            _ => style.0.size[1] = value,
                        }
                    }
                }
                (
                    TweenProperty::TranslateX | TweenProperty::TranslateY | TweenProperty::Scale,
                    TweenValue::Number(v),
                ) => {
                    let apply = |t: &mut UiTransform| match property {
                        TweenProperty::TranslateX => t.translate[0] = v,
                        TweenProperty::TranslateY => t.translate[1] = v,
                        _ => t.scale = v,
                    };
                    let differs = |t: &UiTransform| match property {
                        TweenProperty::TranslateX => t.translate[0] != v,
                        TweenProperty::TranslateY => t.translate[1] != v,
                        _ => t.scale != v,
                    };
                    match transform.as_mut() {
                        Some(t) => {
                            if differs(t) {
                                apply(t);
                            }
                        }
                        None => apply(new_transform.get_or_insert_with(UiTransform::default)),
                    }
                }
                _ => {} // a value of the wrong kind for its property: ignored
            }
        }
        if let Some(t) = new_transform {
            if !done || t != UiTransform::IDENTITY {
                commands.entity(entity).insert(t);
            }
        } else if done && transform.as_deref().is_some_and(|t| *t == UiTransform::IDENTITY) {
            // A pose that settled on the rest transform must not keep the all-nodes scan alive.
            commands.entity(entity).remove::<UiTransform>();
        }

        if done {
            if tween.hide_on_finish
                && let Some(style) = style.as_mut()
                && style.0.display != Display::None
            {
                style.0.display = Display::None;
            }
            if let Some(color) = tween.plan.tweens().iter().rev().find_map(|t| {
                (t.property == TweenProperty::TextColor)
                    .then_some(t.to)
                    .and_then(|v| match v {
                        TweenValue::Color(c) => Some(c),
                        TweenValue::Number(_) => None,
                    })
            }) && let Some(text) = text.as_mut()
                && text.spans.iter().any(|s| s.style.color != color)
            {
                for span in &mut text.spans {
                    span.style.color = color;
                }
            }
            finished.write(UiTweenFinished { entity });
            commands.entity(entity).remove::<UiTween>();
        }
    }
}

/// Whether `entity` has a running [`UiTween`] in `world`.
pub fn is_tweening(world: &World, entity: Entity) -> bool {
    world.get::<UiTween>(entity).is_some()
}

/// Adds the tween message, the clock-driven advance system and the transform pass. [`crate::UiPlugin`]
/// adds it; the transform pass itself is ordered inside the layout chain there.
pub struct UiTweenPlugin;

impl Plugin for UiTweenPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<UiTweenFinished>()
            .add_message::<UiAnimEventFired>()
            .init_resource::<UiAnimEventLog>()
            .add_systems(
                Update,
                (
                    advance_tweens.after(crate::scroll::ui_scroll),
                    record_anim_events.after(advance_tweens),
                ),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    #[test]
    fn every_named_ease_hits_its_endpoints() {
        for (name, ease) in Ease::NAMED {
            assert!(ease.apply(0.0).abs() < EPS, "{name} at 0 = {}", ease.apply(0.0));
            assert!(
                (ease.apply(1.0) - 1.0).abs() < EPS,
                "{name} at 1 = {}",
                ease.apply(1.0)
            );
            assert_eq!(Ease::parse(name), Some(ease));
            assert_eq!(ease.name(), name);
        }
        assert_eq!(Ease::parse("wobble"), None);
        assert_eq!(Ease::CubicBezier([0.1, 0.2, 0.3, 0.4]).name(), "cubic_bezier");
    }

    #[test]
    fn ease_values_match_the_equations() {
        assert!((Ease::Linear.apply(0.3) - 0.3).abs() < EPS);
        assert!((Ease::QuadIn.apply(0.5) - 0.25).abs() < EPS);
        assert!((Ease::QuadOut.apply(0.5) - 0.75).abs() < EPS);
        assert!((Ease::QuadInOut.apply(0.25) - 0.125).abs() < EPS);
        assert!((Ease::QuadInOut.apply(0.75) - 0.875).abs() < EPS);
        assert!((Ease::CubicIn.apply(0.5) - 0.125).abs() < EPS);
        assert!((Ease::CubicOut.apply(0.5) - 0.875).abs() < EPS);
        assert!((Ease::CubicInOut.apply(0.25) - 0.0625).abs() < EPS);
        assert!((Ease::ExpoIn.apply(0.5) - 2f32.powf(-5.0)).abs() < EPS);
        assert!((Ease::ExpoOut.apply(0.5) - (1.0 - 2f32.powf(-5.0))).abs() < EPS);
        assert!((Ease::ExpoInOut.apply(0.5) - 0.5).abs() < EPS);
        // The first bounce segment ends at 1.0.
        assert!((Ease::BounceOut.apply(1.0 / 2.75) - 1.0).abs() < 1e-3);
        assert!((Ease::BounceOut.apply(0.5) - 0.765_625).abs() < EPS);
        assert!((Ease::BounceIn.apply(0.5) - 0.234_375).abs() < EPS);
        assert!((Ease::BounceInOut.apply(0.5) - 0.5).abs() < EPS);
        assert!((Ease::ElasticInOut.apply(0.5) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn back_and_elastic_overshoot_and_the_rest_stay_in_range() {
        assert!(Ease::BackOut.apply(0.7) > 1.0);
        assert!(Ease::BackIn.apply(0.2) < 0.0);
        assert!(Ease::ElasticOut.apply(0.2) > 1.0);
        for (name, ease) in Ease::NAMED {
            if name.starts_with("back") || name.starts_with("elastic") {
                continue;
            }
            for i in 0..=20 {
                let v = ease.apply(i as f32 / 20.0);
                assert!((-EPS..=1.0 + EPS).contains(&v), "{name} left 0..1: {v}");
            }
        }
    }

    #[test]
    fn monotone_curves_never_go_backwards() {
        for name in [
            "linear",
            "quad_in",
            "quad_out",
            "quad_in_out",
            "cubic_in",
            "cubic_out",
            "cubic_in_out",
            "expo_in",
            "expo_out",
            "expo_in_out",
        ] {
            let ease = Ease::parse(name).unwrap();
            let mut last = ease.apply(0.0);
            for i in 1..=50 {
                let v = ease.apply(i as f32 / 50.0);
                assert!(v >= last - 1e-6, "{name} dips at step {i}");
                last = v;
            }
        }
    }

    #[test]
    fn bezier_matches_the_diagonal_and_the_css_ease_shape() {
        let diagonal = Ease::CubicBezier([1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]);
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((diagonal.apply(t) - t).abs() < 1e-3, "diagonal at {t}");
        }
        let css_ease = Ease::CubicBezier([0.25, 0.1, 0.25, 1.0]);
        assert!(css_ease.apply(0.0).abs() < 1e-3);
        assert!((css_ease.apply(1.0) - 1.0).abs() < 1e-3);
        assert!(css_ease.apply(0.5) > 0.5, "ease starts fast");
        // Same input, same output.
        assert_eq!(css_ease.apply(0.37), css_ease.apply(0.37));
    }

    #[test]
    fn out_of_range_and_nan_progress_is_clamped() {
        assert_eq!(Ease::QuadIn.apply(-1.0), 0.0);
        assert_eq!(Ease::QuadIn.apply(2.0), 1.0);
        assert_eq!(Ease::QuadIn.apply(f32::NAN), 0.0);
    }

    fn opacity_to(to: f32, duration: f32) -> Tween {
        Tween::to(TweenProperty::Opacity, TweenValue::Number(to), duration)
    }

    fn sampled(plan: &TweenPlan, t: f32) -> Vec<(TweenProperty, TweenValue)> {
        let mut out = Vec::new();
        plan.sample(t, &mut out);
        out
    }

    fn last_number(out: &[(TweenProperty, TweenValue)], p: TweenProperty) -> Option<f32> {
        out.iter().rev().find_map(|(q, v)| match v {
            TweenValue::Number(n) if *q == p => Some(*n),
            _ => None,
        })
    }

    #[test]
    fn single_tween_samples_linearly_with_delay_and_hold() {
        let plan: TweenPlan = opacity_to(1.0, 2.0)
            .from(TweenValue::Number(0.0))
            .delay(1.0)
            .into();
        assert_eq!(plan.duration(), 3.0);
        let at = |t: f32| last_number(&sampled(&plan, t), TweenProperty::Opacity).unwrap();
        assert_eq!(at(0.0), 0.0);
        assert_eq!(at(1.0), 0.0, "held through the delay");
        assert!((at(2.0) - 0.5).abs() < EPS);
        assert_eq!(at(3.0), 1.0);
        assert_eq!(at(99.0), 1.0, "holds the end value");
        assert!(sampled(&plan, -0.1).is_empty());
    }

    #[test]
    fn zero_duration_jumps_and_bad_durations_are_sane() {
        let plan: TweenPlan = opacity_to(0.5, 0.0).from(TweenValue::Number(1.0)).into();
        assert_eq!(plan.duration(), 0.0);
        assert_eq!(
            last_number(&sampled(&plan, 0.0), TweenProperty::Opacity),
            Some(0.5)
        );
        assert_eq!(opacity_to(1.0, f32::NAN).duration, 0.0);
        assert_eq!(opacity_to(1.0, -3.0).duration, 0.0);
    }

    #[test]
    fn sequence_runs_steps_in_order_and_chains_open_starts() {
        let mut plan = TweenPlan::Sequence(vec![
            opacity_to(1.0, 1.0).from(TweenValue::Number(0.0)).into(),
            opacity_to(0.25, 1.0).into(), // open start: should begin at 1.0
        ]);
        plan.bind(&|_| TweenValue::Number(-7.0));
        assert_eq!(plan.duration(), 2.0);
        let at = |t: f32| last_number(&sampled(&plan, t), TweenProperty::Opacity).unwrap();
        assert!((at(0.5) - 0.5).abs() < EPS);
        assert!((at(1.0) - 1.0).abs() < EPS);
        assert!((at(1.5) - 0.625).abs() < EPS);
        assert!((at(2.0) - 0.25).abs() < EPS);
    }

    #[test]
    fn parallel_runs_branches_together_and_lasts_as_long_as_the_longest() {
        let plan = TweenPlan::Parallel(vec![
            opacity_to(1.0, 1.0).from(TweenValue::Number(0.0)).into(),
            Tween::to(TweenProperty::Width, TweenValue::Number(200.0), 2.0)
                .from(TweenValue::Number(100.0))
                .into(),
        ]);
        assert_eq!(plan.duration(), 2.0);
        let out = sampled(&plan, 1.0);
        assert_eq!(last_number(&out, TweenProperty::Opacity), Some(1.0));
        assert_eq!(last_number(&out, TweenProperty::Width), Some(150.0));
        assert_eq!(
            plan.properties(),
            vec![TweenProperty::Opacity, TweenProperty::Width]
        );
    }

    #[test]
    fn bind_fills_open_starts_with_the_current_value_once() {
        let mut plan: TweenPlan = opacity_to(1.0, 1.0).into();
        plan.bind(&|p| {
            assert_eq!(p, TweenProperty::Opacity);
            TweenValue::Number(0.2)
        });
        let TweenPlan::Tween(t) = &plan else {
            unreachable!()
        };
        assert_eq!(t.from, Some(TweenValue::Number(0.2)));
        // Binding again keeps the start.
        plan.bind(&|_| TweenValue::Number(0.9));
        let TweenPlan::Tween(t) = &plan else {
            unreachable!()
        };
        assert_eq!(t.from, Some(TweenValue::Number(0.2)));
    }

    #[test]
    fn sampling_is_pure() {
        let mut plan = TweenPlan::Parallel(vec![
            opacity_to(1.0, 1.0).ease(Ease::ElasticOut).into(),
            Tween::to(TweenProperty::Background, TweenValue::Color(UiColor::WHITE), 1.0).into(),
        ]);
        plan.bind(&|p| {
            if p.is_color() {
                TweenValue::Color(UiColor::BLACK)
            } else {
                TweenValue::Number(0.0)
            }
        });
        for i in 0..=10 {
            let t = i as f32 * 0.1;
            assert_eq!(sampled(&plan, t), sampled(&plan, t), "t = {t}");
        }
    }

    #[test]
    fn colours_blend_and_property_kinds_are_checked() {
        let c = TweenValue::Color(UiColor::BLACK).lerp(TweenValue::Color(UiColor::WHITE), 0.5);
        assert_eq!(c, TweenValue::Color(UiColor([0.5, 0.5, 0.5, 1.0])));
        assert!(TweenValue::Color(UiColor::WHITE).fits(TweenProperty::Background));
        assert!(!TweenValue::Number(1.0).fits(TweenProperty::Background));
        assert!(TweenValue::Number(1.0).fits(TweenProperty::Width));
        for p in TweenProperty::ALL {
            assert_eq!(TweenProperty::parse(p.name()), Some(p));
        }
        assert_eq!(TweenProperty::parse("nope"), None);
    }

    #[test]
    fn show_and_hide_helpers_build_the_expected_plans() {
        let show = UiTween::show(Reveal::Slide([0.0, 20.0]), 0.5, Ease::QuadOut);
        assert!(show.show_on_start && !show.hide_on_finish);
        assert_eq!(show.duration(), 0.5);
        assert_eq!(
            show.plan.properties(),
            vec![
                TweenProperty::Opacity,
                TweenProperty::TranslateX,
                TweenProperty::TranslateY
            ]
        );
        let hide = UiTween::hide(Reveal::Fade, 0.25, Ease::Linear);
        assert!(hide.hide_on_finish && !hide.show_on_start);
        assert_eq!(hide.plan.properties(), vec![TweenProperty::Opacity]);
        assert_eq!(hide.progress(), 0.0);
        assert_eq!(UiTween::new(opacity_to(1.0, 0.0)).progress(), 1.0);
    }

    fn base(rect: [f32; 4], scale: f32) -> ComputedNode {
        ComputedNode {
            rect,
            clip_rect: rect,
            scale,
            ..Default::default()
        }
    }

    #[test]
    fn affine_scale_keeps_the_pivot_fixed() {
        let b = base([100.0, 50.0, 80.0, 40.0], 1.0);
        let t = UiTransform {
            scale: 2.0,
            ..UiTransform::default()
        };
        let a = Affine::of(&t, &b);
        // Centre pivot at (140, 70): the rect doubles about it.
        assert_eq!(a.rect(b.rect), [60.0, 30.0, 160.0, 80.0]);
        let corner = UiTransform {
            scale: 0.5,
            pivot_ratio: [0.0, 0.0],
            ..UiTransform::default()
        };
        // Top-left pivot at (100, 50): that corner stays put while the size halves.
        assert_eq!(Affine::of(&corner, &b).rect(b.rect), [100.0, 50.0, 40.0, 20.0]);
    }

    #[test]
    fn affine_translate_uses_ui_px_times_the_node_scale() {
        let b = base([10.0, 10.0, 20.0, 20.0], 2.0);
        let t = UiTransform {
            translate: [5.0, -3.0],
            ..UiTransform::default()
        };
        assert_eq!(Affine::of(&t, &b).rect(b.rect), [20.0, 4.0, 20.0, 20.0]);
    }

    #[test]
    fn affine_composition_applies_the_inner_map_first() {
        let outer = Affine {
            a: 2.0,
            b: [10.0, 0.0],
        };
        let inner = Affine {
            a: 3.0,
            b: [1.0, 1.0],
        };
        let both = outer.after(inner);
        let p = 4.0;
        let expect = 2.0 * (3.0 * p + 1.0) + 10.0;
        assert_eq!(both.a * p + both.b[0], expect);
        assert!(Affine::IDENTITY.is_identity());
        assert!(!both.is_identity());
    }

    #[test]
    fn clip_sentinel_survives_a_scale() {
        let a = Affine {
            a: 2.0,
            b: [5.0, 5.0],
        };
        assert_eq!(a.clip(NO_CLIP), NO_CLIP);
        assert_eq!(a.clip([1.0, 1.0, 2.0, 2.0]), [7.0, 7.0, 4.0, 4.0]);
    }

    #[test]
    fn hit_rects_follow_the_flag() {
        let applied = base([5.0, 5.0, 10.0, 10.0], 1.0);
        let layout = base([0.0, 0.0, 10.0, 10.0], 1.0);
        let mut b = UiLayoutBox {
            base: layout,
            applied,
            hit_layout: false,
        };
        assert_eq!(hit_rects(&applied, Some(&b)).0, applied.rect);
        b.hit_layout = true;
        assert_eq!(hit_rects(&applied, Some(&b)).0, layout.rect);
        assert_eq!(hit_rects(&applied, None).0, applied.rect);
    }
}
