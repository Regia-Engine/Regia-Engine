// SPDX-License-Identifier: MPL-2.0
//! Player-facing choice widgets: a vertical list and a radial wheel
//! (`docs/specs/56-dialogue-and-subtitles.md` § 12, ADR-3234).
//!
//! [`ChoiceWidget`] is a pure state machine: it takes [`ChoiceInput`]s (keyboard, gamepad, pointer,
//! touch) and fixed simulation steps ([`ChoiceWidget::tick`]) and returns [`ChoiceEvent`]s. It
//! reads no clock, so every rule is testable without an `App`. [`ChoiceWidget::layout`] returns
//! the rectangles, theme classes and accessible names of the parts; the ECS side
//! ([`ChoicePlugin`]) feeds it from [`crate::focus::UiNavActions`] and writes a [`ChoicePicked`]
//! message a game binds to.
//!
//! Rules:
//! - Focus lands on the default option (or the first enabled one) when the widget opens.
//! - Keyboard and gamepad navigation skips disabled options and wraps; a disabled option can be
//!   hovered (its reason shows) but never focused or picked.
//! - An optional timer counts fixed steps and picks the default option when it reaches zero.
//! - In a right-to-left locale the list is mirrored (marker and text align to the right) and the
//!   wheel's left/right keys turn the other way.

use std::f32::consts::TAU;

use regia_app::prelude::*;

use crate::captions::Rect;
use crate::focus::UiNavActions;
use crate::radial::{RadialGeometry, angle_from_up};

/// Theme class of the widget root.
pub const CLASS_ROOT: &str = "choice";
/// Theme class of the prompt text.
pub const CLASS_PROMPT: &str = "choice-prompt";
/// Theme class of one option.
pub const CLASS_OPTION: &str = "choice-option";
/// Added to the focused option.
pub const CLASS_FOCUSED: &str = "choice-option-focused";
/// Added to a disabled option.
pub const CLASS_DISABLED: &str = "choice-option-disabled";
/// Theme class of the reason text of a disabled option.
pub const CLASS_REASON: &str = "choice-reason";
/// Theme class of the countdown bar.
pub const CLASS_TIMER: &str = "choice-timer";
/// Theme class of a wheel sector chip (instead of [`CLASS_OPTION`]).
pub const CLASS_SECTOR: &str = "choice-sector";

/// List or wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChoiceLayout {
    /// A vertical list.
    #[default]
    List,
    /// A radial wheel.
    Wheel,
}

/// One option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    /// The text shown.
    pub label: String,
    /// Why the option cannot be picked; `Some` disables it.
    pub disabled_reason: Option<String>,
}

impl ChoiceOption {
    /// An enabled option.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disabled_reason: None,
        }
    }

    /// A disabled option with a reason.
    pub fn disabled(label: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disabled_reason: Some(reason.into()),
        }
    }

    /// True when the option can be picked.
    pub fn enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

/// An input, already reduced from device events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChoiceInput {
    /// Up arrow, d-pad up.
    Up,
    /// Down arrow, d-pad down.
    Down,
    /// Left arrow, d-pad left.
    Left,
    /// Right arrow, d-pad right.
    Right,
    /// Tab, shoulder button.
    Next,
    /// Shift-Tab, shoulder button.
    Prev,
    /// Enter, gamepad A.
    Submit,
    /// Escape, gamepad B.
    Cancel,
    /// A stick or d-pad vector (`x` right, `y` up, length `0..=1`): wheels select by angle.
    Stick([f32; 2]),
    /// The pointer moved to this pixel.
    PointerMove([f32; 2]),
    /// A pointer press at this pixel.
    PointerPress([f32; 2]),
    /// A touch tap at this pixel.
    Touch([f32; 2]),
    /// A number key: option `n` (1 based).
    Digit(usize),
}

/// Why a pick happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickSource {
    /// The player picked it.
    Player,
    /// The timer ran out and the default was picked.
    Timer,
}

/// What the widget reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceEvent {
    /// The focus moved to an option.
    Focused(usize),
    /// An option was picked; the widget is closed.
    Picked {
        /// The option.
        index: usize,
        /// Player or timer.
        source: PickSource,
    },
    /// The player cancelled; the widget is closed.
    Cancelled,
    /// A disabled option was pressed or submitted.
    Rejected {
        /// The option.
        index: usize,
        /// Its reason.
        reason: String,
    },
}

/// A countdown on fixed simulation steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChoiceTimer {
    /// Steps left; the pick fires on the step this reaches zero.
    pub remaining: u32,
    /// Steps at open.
    pub total: u32,
    /// The option picked at zero (a disabled or missing default falls back to the first enabled).
    pub default_index: usize,
}

/// Geometry parameters, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChoiceMetrics {
    /// The area the widget may use.
    pub area: Rect,
    /// List: height of one row.
    pub row_height: f32,
    /// List: gap between rows.
    pub gap: f32,
    /// Wheel: ring radius of the chip centers.
    pub radius: f32,
    /// Wheel: chip width and height.
    pub chip: [f32; 2],
    /// Wheel: dead zone of the stick (`0..1`).
    pub dead_zone: f32,
}

impl Default for ChoiceMetrics {
    fn default() -> Self {
        Self {
            area: Rect {
                x: 0.0,
                y: 0.0,
                w: 960.0,
                h: 540.0,
            },
            row_height: 48.0,
            gap: 8.0,
            radius: 160.0,
            chip: [180.0, 56.0],
            dead_zone: 0.35,
        }
    }
}

/// One laid-out option.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoicePart {
    /// The option index.
    pub index: usize,
    /// The option rectangle.
    pub rect: Rect,
    /// List only: the marker (arrow or number) rectangle, at the start edge (right in RTL).
    pub marker: Option<Rect>,
    /// True when the text aligns to the right (RTL list).
    pub align_end: bool,
    /// Theme classes: the part class plus state classes.
    pub classes: Vec<&'static str>,
    /// The accessible name.
    pub a11y_name: String,
}

/// The layout of the whole widget.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceLayoutResult {
    /// One per option, in option order.
    pub parts: Vec<ChoicePart>,
    /// The countdown bar, when a timer runs; its width is the remaining fraction.
    pub timer_bar: Option<Rect>,
    /// The accessible name of the whole widget.
    pub a11y_name: String,
}

/// The widget state machine.
#[derive(Debug, Clone)]
pub struct ChoiceWidget {
    layout: ChoiceLayout,
    prompt: Option<String>,
    options: Vec<ChoiceOption>,
    focus: Option<usize>,
    hover: Option<usize>,
    rtl: bool,
    timer: Option<ChoiceTimer>,
    metrics: ChoiceMetrics,
    open: bool,
    /// True until the first event is polled: the host sets the UI focus onto the widget.
    focus_on_open: bool,
}

impl ChoiceWidget {
    /// Opens a widget. Focus lands on the first enabled option; with none enabled it stays unset.
    pub fn open(layout: ChoiceLayout, prompt: Option<String>, options: Vec<ChoiceOption>, rtl: bool) -> Self {
        let focus = options.iter().position(ChoiceOption::enabled);
        Self {
            layout,
            prompt,
            options,
            focus,
            hover: None,
            rtl,
            timer: None,
            metrics: ChoiceMetrics::default(),
            open: true,
            focus_on_open: true,
        }
    }

    /// Sets the geometry.
    pub fn with_metrics(mut self, metrics: ChoiceMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Adds a countdown of `steps` fixed steps that picks `default_index` (clamped to a valid,
    /// enabled option) at zero. A zero count is ignored.
    pub fn with_timer(mut self, steps: u32, default_index: usize) -> Self {
        if steps > 0 {
            let default_index = if self.options.get(default_index).is_some_and(ChoiceOption::enabled) {
                default_index
            } else {
                self.options.iter().position(ChoiceOption::enabled).unwrap_or(0)
            };
            self.timer = Some(ChoiceTimer {
                remaining: steps,
                total: steps,
                default_index,
            });
            // The default is where focus starts, so a player sees what the timer will pick.
            if self.options.get(default_index).is_some_and(ChoiceOption::enabled) {
                self.focus = Some(default_index);
            }
        }
        self
    }

    /// The prompt.
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    /// The options.
    pub fn options(&self) -> &[ChoiceOption] {
        &self.options
    }

    /// The layout kind.
    pub fn kind(&self) -> ChoiceLayout {
        self.layout
    }

    /// The focused option.
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// The option under the pointer (may be disabled: its reason shows).
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// The reason text to show now: the hovered or focused disabled option's reason.
    pub fn reason_shown(&self) -> Option<&str> {
        self.hover
            .or(self.focus)
            .and_then(|i| self.options.get(i))
            .and_then(|o| o.disabled_reason.as_deref())
    }

    /// The countdown.
    pub fn timer(&self) -> Option<ChoiceTimer> {
        self.timer
    }

    /// False once a pick or cancel closed the widget.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// True once after opening so the host can move the UI focus onto the widget.
    pub fn take_focus_on_open(&mut self) -> bool {
        std::mem::take(&mut self.focus_on_open)
    }

    /// True for a right-to-left locale.
    pub fn rtl(&self) -> bool {
        self.rtl
    }

    fn enabled_count(&self) -> usize {
        self.options.iter().filter(|o| o.enabled()).count()
    }

    /// The next enabled option from `from` in `dir` (+1 / -1), wrapping; `None` when none.
    fn step_enabled(&self, from: Option<usize>, dir: i32) -> Option<usize> {
        let n = self.options.len();
        if n == 0 || self.enabled_count() == 0 {
            return None;
        }
        let mut i = match from {
            Some(i) => i as i64,
            None if dir > 0 => -1,
            None => n as i64,
        };
        for _ in 0..n {
            i = (i + i64::from(dir)).rem_euclid(n as i64);
            if self.options[i as usize].enabled() {
                return Some(i as usize);
            }
        }
        None
    }

    fn move_focus(&mut self, dir: i32, out: &mut Vec<ChoiceEvent>) {
        if let Some(i) = self.step_enabled(self.focus, dir) {
            self.focus_to(i, out);
        }
    }

    fn focus_to(&mut self, i: usize, out: &mut Vec<ChoiceEvent>) {
        if self.focus != Some(i) {
            self.focus = Some(i);
            out.push(ChoiceEvent::Focused(i));
        }
    }

    fn pick(&mut self, index: usize, source: PickSource, out: &mut Vec<ChoiceEvent>) {
        match self.options.get(index).and_then(|o| o.disabled_reason.clone()) {
            Some(reason) => out.push(ChoiceEvent::Rejected { index, reason }),
            None if index < self.options.len() => {
                self.open = false;
                out.push(ChoiceEvent::Picked { index, source });
            }
            None => {}
        }
    }

    fn wheel_geometry(&self) -> RadialGeometry {
        RadialGeometry {
            sectors: self.options.len(),
            dead_zone: self.metrics.dead_zone,
            ..RadialGeometry::default()
        }
    }

    /// The option under a pixel, in the current layout.
    pub fn hit_test(&self, px: [f32; 2]) -> Option<usize> {
        let layout = self.layout();
        layout
            .parts
            .iter()
            .find(|p| contains(p.rect, px))
            .map(|p| p.index)
    }

    /// Applies one input and returns the events it caused. A closed widget ignores input.
    pub fn input(&mut self, input: ChoiceInput) -> Vec<ChoiceEvent> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        let wheel = self.layout == ChoiceLayout::Wheel;
        // Clockwise is "next" on a wheel; right turns clockwise unless the locale is mirrored.
        let right_dir = if self.rtl { -1 } else { 1 };
        match input {
            ChoiceInput::Up | ChoiceInput::Prev => self.move_focus(-1, &mut out),
            ChoiceInput::Down | ChoiceInput::Next => self.move_focus(1, &mut out),
            ChoiceInput::Left if wheel => self.move_focus(-right_dir, &mut out),
            ChoiceInput::Right if wheel => self.move_focus(right_dir, &mut out),
            ChoiceInput::Left | ChoiceInput::Right => {}
            ChoiceInput::Submit => {
                if let Some(i) = self.focus {
                    self.pick(i, PickSource::Player, &mut out);
                }
            }
            ChoiceInput::Cancel => {
                self.open = false;
                out.push(ChoiceEvent::Cancelled);
            }
            ChoiceInput::Digit(n) => {
                if n >= 1 && n <= self.options.len() {
                    self.pick(n - 1, PickSource::Player, &mut out);
                }
            }
            ChoiceInput::Stick(v) => {
                if wheel {
                    let sel = self.wheel_geometry().select(self.focus.or(self.hover), v);
                    self.hover = sel;
                    if let Some(i) = sel.filter(|i| self.options.get(*i).is_some_and(ChoiceOption::enabled)) {
                        self.focus_to(i, &mut out);
                    }
                } else if v[1].abs() > self.metrics.dead_zone.max(0.5) && v[1].abs() >= v[0].abs() {
                    // A stick on a list: one step per tilt is the caller's job (edge detection);
                    // a vertical tilt moves the focus.
                    self.move_focus(if v[1] > 0.0 { -1 } else { 1 }, &mut out);
                }
            }
            ChoiceInput::PointerMove(px) => {
                self.hover = self.hit_test(px);
                if let Some(i) = self.hover.filter(|i| self.options[*i].enabled()) {
                    self.focus_to(i, &mut out);
                }
            }
            ChoiceInput::PointerPress(px) | ChoiceInput::Touch(px) => {
                if let Some(i) = self.hit_test(px) {
                    self.hover = Some(i);
                    self.pick(i, PickSource::Player, &mut out);
                }
            }
        }
        out
    }

    /// Advances the timer by one fixed step. At zero the default option is picked.
    pub fn tick(&mut self) -> Vec<ChoiceEvent> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        if let Some(t) = &mut self.timer {
            t.remaining = t.remaining.saturating_sub(1);
            if t.remaining == 0 {
                let index = t.default_index;
                if self.options.get(index).is_some_and(ChoiceOption::enabled) {
                    self.pick(index, PickSource::Timer, &mut out);
                }
                if self.open {
                    // Nothing could be picked (no enabled option): close without a pick.
                    self.open = false;
                    out.push(ChoiceEvent::Cancelled);
                }
            }
        }
        out
    }

    /// The rectangles, classes and accessible names of the parts.
    pub fn layout(&self) -> ChoiceLayoutResult {
        let m = &self.metrics;
        let n = self.options.len();
        let mut parts = Vec::with_capacity(n);
        let wheel = self.layout == ChoiceLayout::Wheel;
        let geo = self.wheel_geometry();
        let cx = m.area.x + m.area.w * 0.5;
        let cy = m.area.y + m.area.h * 0.5;
        for (i, o) in self.options.iter().enumerate() {
            let (rect, marker, align_end);
            let mut classes = vec![if wheel { CLASS_SECTOR } else { CLASS_OPTION }];
            if self.focus == Some(i) {
                classes.push(CLASS_FOCUSED);
            }
            if !o.enabled() {
                classes.push(CLASS_DISABLED);
            }
            if wheel {
                let a = if n == 1 { 0.0 } else { geo.centre_angle(i) };
                // Angles run clockwise from up; the ring is mirrored in RTL so that "right" of the
                // wheel stays the direction the right key turns.
                let (s, c) = a.sin_cos();
                let sx = if self.rtl { -s } else { s };
                let x = cx + sx * m.radius;
                let y = cy - c * m.radius;
                rect = Rect {
                    x: x - m.chip[0] * 0.5,
                    y: y - m.chip[1] * 0.5,
                    w: m.chip[0],
                    h: m.chip[1],
                };
                marker = None;
                align_end = false;
            } else {
                let total = n as f32 * m.row_height + n.saturating_sub(1) as f32 * m.gap;
                let top = m.area.y + (m.area.h - total).max(0.0) * 0.5;
                let y = top + i as f32 * (m.row_height + m.gap);
                rect = Rect {
                    x: m.area.x,
                    y,
                    w: m.area.w,
                    h: m.row_height,
                };
                let mw = m.row_height;
                marker = Some(Rect {
                    x: if self.rtl { rect.x + rect.w - mw } else { rect.x },
                    y,
                    w: mw,
                    h: m.row_height,
                });
                align_end = self.rtl;
            }
            let a11y_name = match &o.disabled_reason {
                Some(r) => format!("{}, {} of {}, unavailable: {}", o.label, i + 1, n, r),
                None => format!("{}, {} of {}", o.label, i + 1, n),
            };
            parts.push(ChoicePart {
                index: i,
                rect,
                marker,
                align_end,
                classes,
                a11y_name,
            });
        }
        let timer_bar = self.timer.map(|t| Rect {
            x: m.area.x,
            y: m.area.y + m.area.h,
            w: m.area.w * (t.remaining as f32 / t.total.max(1) as f32),
            h: 4.0,
        });
        let a11y_name = match &self.prompt {
            Some(p) => format!("{p}, {n} options"),
            None => format!("{n} options"),
        };
        ChoiceLayoutResult {
            parts,
            timer_bar,
            a11y_name,
        }
    }

    /// Wheel only: the pixel a stick vector `v` at full tilt maps to, for tests and prompts.
    pub fn wheel_center(&self) -> [f32; 2] {
        [
            self.metrics.area.x + self.metrics.area.w * 0.5,
            self.metrics.area.y + self.metrics.area.h * 0.5,
        ]
    }

    /// The stick vector that selects option `i` on a wheel (mirrors in RTL).
    pub fn stick_for(&self, i: usize) -> [f32; 2] {
        let a = self.wheel_geometry().centre_angle(i);
        let (s, c) = a.sin_cos();
        let sx = if self.rtl { -s } else { s };
        // Keep the angle in range for `angle_from_up`.
        debug_assert!((0.0..TAU).contains(&angle_from_up(sx, c)));
        [sx, c]
    }
}

fn contains(r: Rect, p: [f32; 2]) -> bool {
    p[0] >= r.x && p[0] < r.x + r.w && p[1] >= r.y && p[1] < r.y + r.h
}

/// A pick, for a game to bind to.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ChoicePicked {
    /// The widget id from [`ChoiceBoard::show`].
    pub widget: u64,
    /// The picked option; `None` when the player cancelled.
    pub index: Option<usize>,
    /// Player or timer; `Player` for a cancel.
    pub source: PickSource,
}

/// The open choice widgets.
#[derive(Resource, Debug, Default)]
pub struct ChoiceBoard {
    next_id: u64,
    widgets: Vec<(u64, ChoiceWidget)>,
    /// Inputs queued by the agent protocol or the host.
    queued: Vec<(u64, ChoiceInput)>,
}

impl ChoiceBoard {
    /// Shows a widget and returns its id.
    pub fn show(&mut self, widget: ChoiceWidget) -> u64 {
        self.next_id += 1;
        self.widgets.push((self.next_id, widget));
        self.next_id
    }

    /// The widget `id`, if open.
    pub fn get(&self, id: u64) -> Option<&ChoiceWidget> {
        self.widgets.iter().find(|(i, _)| *i == id).map(|(_, w)| w)
    }

    /// The open widgets.
    pub fn open_ids(&self) -> Vec<u64> {
        self.widgets.iter().map(|(i, _)| *i).collect()
    }

    /// Queues an input for widget `id` (applied on the next [`choice_input_system`] run).
    pub fn queue_input(&mut self, id: u64, input: ChoiceInput) {
        self.queued.push((id, input));
    }

    /// Applies the queued inputs and the navigation actions to the newest widget (frame rate).
    pub fn apply_inputs(&mut self, nav: Option<&UiNavActions>) -> Vec<ChoicePicked> {
        let mut picks = Vec::new();
        let mut inputs = std::mem::take(&mut self.queued);
        if let (Some(nav), Some((id, _))) = (nav, self.widgets.last()) {
            let id = *id;
            for (flag, input) in [
                (nav.up, ChoiceInput::Up),
                (nav.down, ChoiceInput::Down),
                (nav.left, ChoiceInput::Left),
                (nav.right, ChoiceInput::Right),
                (nav.next, ChoiceInput::Next),
                (nav.prev, ChoiceInput::Prev),
                (nav.submit, ChoiceInput::Submit),
                (nav.cancel, ChoiceInput::Cancel),
            ] {
                if flag {
                    inputs.push((id, input));
                }
            }
        }
        for (id, input) in inputs {
            if let Some((_, w)) = self.widgets.iter_mut().find(|(i, _)| *i == id) {
                collect(id, w.input(input), &mut picks);
            }
        }
        self.widgets.retain(|(_, w)| w.is_open());
        picks
    }

    /// Advances every timer by one fixed step (fixed rate).
    pub fn tick(&mut self) -> Vec<ChoicePicked> {
        let mut picks = Vec::new();
        for (id, w) in &mut self.widgets {
            collect(*id, w.tick(), &mut picks);
        }
        self.widgets.retain(|(_, w)| w.is_open());
        picks
    }

    /// Both in order: inputs, then one step. For tests and the agent.
    pub fn step(&mut self, nav: Option<&UiNavActions>) -> Vec<ChoicePicked> {
        let mut picks = self.apply_inputs(nav);
        picks.extend(self.tick());
        picks
    }
}

fn collect(widget: u64, events: Vec<ChoiceEvent>, picks: &mut Vec<ChoicePicked>) {
    for e in events {
        match e {
            ChoiceEvent::Picked { index, source } => picks.push(ChoicePicked {
                widget,
                index: Some(index),
                source,
            }),
            ChoiceEvent::Cancelled => picks.push(ChoicePicked {
                widget,
                index: None,
                source: PickSource::Player,
            }),
            ChoiceEvent::Focused(_) | ChoiceEvent::Rejected { .. } => {}
        }
    }
}

/// `Update`, before the focus navigation clears the actions: applies input.
pub fn choice_input_system(
    mut board: ResMut<ChoiceBoard>,
    nav: Option<Res<UiNavActions>>,
    mut picked: MessageWriter<ChoicePicked>,
) {
    let _span = tracing::trace_span!("choice_input").entered();
    if board.widgets.is_empty() && board.queued.is_empty() {
        return;
    }
    for p in board.apply_inputs(nav.as_deref()) {
        picked.write(p);
    }
}

/// `FixedUpdate`: advances the timers one step.
pub fn choice_timer_system(mut board: ResMut<ChoiceBoard>, mut picked: MessageWriter<ChoicePicked>) {
    if board.widgets.is_empty() {
        return;
    }
    for p in board.tick() {
        picked.write(p);
    }
}

/// Registers [`ChoiceBoard`], [`ChoicePicked`] and the systems.
pub struct ChoicePlugin;

impl Plugin for ChoicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChoiceBoard>();
        app.add_message::<ChoicePicked>();
        app.add_systems(
            Update,
            choice_input_system.before(crate::focus::ui_focus_navigation),
        );
        app.add_systems(FixedUpdate, choice_timer_system);
    }
}

#[cfg(test)]
mod tests;
