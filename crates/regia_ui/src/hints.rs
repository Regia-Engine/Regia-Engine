// SPDX-License-Identifier: MPL-2.0
//! Tooltips and the hint system (`specs/ui-default-design.md` § 5, parity items W13 and I07).
//!
//! Three parts:
//!
//! 1. [`TooltipMachine`], a pure state machine with the delay and dismiss rules. It takes the
//!    elapsed time, the tooltip candidate of the frame and an optional dismiss reason, so every rule
//!    is testable with fixed steps and no clock.
//! 2. [`HintAction`], [`HintDevice`] and [`HintBar`]: the glyph a prompt shows for an abstract
//!    action on the last-used device, and the controller hint bar built from them.
//! 3. [`update_tooltips`], the system that feeds the machine from hover, focus, press, scroll and
//!    the navigation actions, and writes [`Tooltip::visible`].
//!
//! The rules:
//!
//! - A tooltip opens after its `delay` of hover (pointer) or after its `focus_delay` of focus
//!   (keyboard or gamepad navigation; 0.8 s by default).
//! - In [`UiInputMode::Navigation`] the focused node is the only candidate; in
//!   [`UiInputMode::Pointer`] the hovered node is.
//! - Warm-up: while a tooltip is open, or within [`TooltipPolicy::warm_window`] seconds after one
//!   closed, the next tooltip opens at once, so sweeping across a toolbar does not restart the wait.
//! - A press on the node, a submit or cancel action (Enter, Escape, gamepad A or B) and a scroll
//!   dismiss the tooltip and keep it closed until the pointer or focus leaves the node.
//! - Optionally the tooltip closes by itself after [`TooltipPolicy::auto_hide`] seconds.
//! - In navigation mode a [`TooltipNavText`] replaces the text, and `{activate}`-style placeholders
//!   in either text expand to the glyph of the last-used device.

use regia_app::prelude::*;

use crate::focus::UiInputMode;
use crate::interaction::UiPointer;
use crate::node::{Focused, Hovered};
use crate::widgets::Tooltip;

/// What opened a tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooltipTrigger {
    /// The pointer rests on the node.
    Hover,
    /// The node has keyboard or gamepad focus.
    Focus,
}

impl TooltipTrigger {
    /// Stable lowercase name (agent output).
    pub fn name(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Focus => "focus",
        }
    }
}

/// Why a tooltip was dismissed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooltipDismiss {
    /// The node was pressed or activated.
    Press,
    /// A cancel action (Escape, gamepad B).
    Cancel,
    /// A wheel or trackpad scroll.
    Scroll,
    /// [`TooltipPolicy::auto_hide`] elapsed.
    Timeout,
}

/// Tuning of the tooltip rules (a resource; the defaults follow `specs/ui-default-design.md` § 4).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct TooltipPolicy {
    /// Seconds after a tooltip closed during which the next one opens without waiting.
    pub warm_window: f32,
    /// Seconds an open tooltip stays before it closes by itself; `None` keeps it until a dismiss rule fires.
    pub auto_hide: Option<f32>,
}

impl Default for TooltipPolicy {
    fn default() -> Self {
        Self {
            warm_window: 0.4,
            auto_hide: None,
        }
    }
}

/// The node a tooltip is waiting on this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TooltipCandidate {
    /// The node that owns the tooltip.
    pub entity: Entity,
    /// What makes it a candidate.
    pub trigger: TooltipTrigger,
    /// Seconds of rest before it opens (already includes the focus extra delay).
    pub delay: f32,
}

/// The pure tooltip state machine: at most one tooltip is open at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct TooltipMachine {
    target: Option<(Entity, TooltipTrigger)>,
    waited: f32,
    shown: bool,
    shown_for: f32,
    idle: f32,
    suppressed: Option<Entity>,
    last_dismiss: Option<TooltipDismiss>,
}

impl Default for TooltipMachine {
    fn default() -> Self {
        Self {
            target: None,
            waited: 0.0,
            shown: false,
            shown_for: 0.0,
            // A fresh machine is cold: the first tooltip waits its full delay.
            idle: f32::INFINITY,
            suppressed: None,
            last_dismiss: None,
        }
    }
}

impl TooltipMachine {
    /// Advances by `dt` seconds with this frame's candidate and dismiss reason; returns whether a
    /// tooltip is open afterwards.
    pub fn step(
        &mut self,
        dt: f32,
        policy: &TooltipPolicy,
        candidate: Option<TooltipCandidate>,
        dismiss: Option<TooltipDismiss>,
    ) -> bool {
        // A NaN or negative step would leave `waited` NaN for as long as the same node stays the
        // candidate, and the tooltip would never open: it counts as no time passing.
        let dt = if dt.is_nan() || dt < 0.0 { 0.0 } else { dt };
        if !self.shown {
            self.idle += dt;
        }
        let candidate_entity = candidate.map(|c| c.entity);
        if self.target.map(|t| t.0) != candidate_entity {
            self.close();
            if self.suppressed != candidate_entity {
                self.suppressed = None;
            }
            self.waited = 0.0;
        }
        self.target = candidate.map(|c| (c.entity, c.trigger));
        if let (Some(reason), Some(c)) = (dismiss, candidate) {
            self.close();
            self.suppressed = Some(c.entity);
            self.waited = 0.0;
            self.last_dismiss = Some(reason);
        }
        if let Some(c) = candidate
            && self.suppressed != Some(c.entity)
        {
            if self.shown {
                self.shown_for += dt;
                if let Some(max) = policy.auto_hide
                    && self.shown_for >= max
                {
                    self.close();
                    self.suppressed = Some(c.entity);
                    self.last_dismiss = Some(TooltipDismiss::Timeout);
                }
            } else {
                self.waited += dt;
                let need = if self.idle <= policy.warm_window {
                    0.0
                } else {
                    c.delay
                };
                if self.waited >= need {
                    self.shown = true;
                    self.shown_for = 0.0;
                    self.last_dismiss = None;
                }
            }
        }
        self.shown
    }

    fn close(&mut self) {
        if self.shown {
            self.shown = false;
            self.idle = 0.0;
        }
        self.shown_for = 0.0;
    }

    /// The node whose tooltip is open.
    pub fn shown_entity(&self) -> Option<Entity> {
        if self.shown {
            self.target.map(|t| t.0)
        } else {
            None
        }
    }

    /// What opened the open tooltip.
    pub fn shown_trigger(&self) -> Option<TooltipTrigger> {
        if self.shown {
            self.target.map(|t| t.1)
        } else {
            None
        }
    }

    /// The node being waited on (or shown) and the seconds it has waited.
    pub fn waiting(&self) -> Option<(Entity, f32)> {
        self.target.map(|t| (t.0, self.waited))
    }

    /// Why the last tooltip was dismissed (cleared when the next one opens).
    pub fn last_dismiss(&self) -> Option<TooltipDismiss> {
        self.last_dismiss
    }
}

/// The tooltip state of the UI world: the machine plus the text on screen.
#[derive(Resource, Debug, Default, Clone)]
pub struct TooltipState {
    machine: TooltipMachine,
    pending_dismiss: Option<TooltipDismiss>,
    text: Option<String>,
}

impl TooltipState {
    /// Queues a dismiss for the next frame (the focus system calls this for submit and cancel).
    pub fn request_dismiss(&mut self, reason: TooltipDismiss) {
        self.pending_dismiss = Some(reason);
    }

    /// The node whose tooltip is open.
    pub fn shown_entity(&self) -> Option<Entity> {
        self.machine.shown_entity()
    }

    /// The text on screen (placeholders expanded), when a tooltip is open.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// What opened the open tooltip.
    pub fn trigger(&self) -> Option<TooltipTrigger> {
        self.machine.shown_trigger()
    }

    /// The machine (read-only).
    pub fn machine(&self) -> &TooltipMachine {
        &self.machine
    }
}

/// Text shown instead of [`Tooltip::text`] while the UI is in [`UiInputMode::Navigation`]
/// (a gamepad or keyboard prompt such as `"{activate} to buy"`).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TooltipNavText(pub String);

/// An abstract UI action a hint can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HintAction {
    /// Activate the focused node (A / Cross / Enter).
    Activate,
    /// Back or close (B / Circle / Escape).
    Back,
    /// Secondary action (Y / Triangle).
    Secondary,
    /// Pause or menu (Start).
    Menu,
    /// Previous tab (left shoulder).
    PrevTab,
    /// Next tab (right shoulder).
    NextTab,
    /// Page up in long lists (left trigger).
    PageUp,
    /// Page down in long lists (right trigger).
    PageDown,
}

impl HintAction {
    /// Every action, in table order.
    pub const ALL: [HintAction; 8] = [
        Self::Activate,
        Self::Back,
        Self::Secondary,
        Self::Menu,
        Self::PrevTab,
        Self::NextTab,
        Self::PageUp,
        Self::PageDown,
    ];

    /// Stable lowercase name, also the placeholder name in tooltip text (`{prev_tab}`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Back => "back",
            Self::Secondary => "secondary",
            Self::Menu => "menu",
            Self::PrevTab => "prev_tab",
            Self::NextTab => "next_tab",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
        }
    }

    /// Parses [`HintAction::name`].
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// The device whose glyphs prompts show (the host sets it from the last device used).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HintDevice {
    /// Keyboard and mouse.
    #[default]
    KeyboardMouse,
    /// Xbox-style gamepad.
    Xbox,
    /// PlayStation-style gamepad.
    PlayStation,
    /// Steam Deck controls.
    SteamDeck,
}

impl HintDevice {
    /// Stable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Self::KeyboardMouse => "keyboard_mouse",
            Self::Xbox => "xbox",
            Self::PlayStation => "playstation",
            Self::SteamDeck => "steam_deck",
        }
    }

    /// Parses [`HintDevice::name`] (ADR-7070: glyph tables and the agent name devices this way).
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::KeyboardMouse,
            Self::Xbox,
            Self::PlayStation,
            Self::SteamDeck,
        ]
        .into_iter()
        .find(|d| d.name() == name)
    }

    /// The label this device prints for `action`.
    pub fn glyph(self, action: HintAction) -> &'static str {
        use HintAction::*;
        match self {
            Self::KeyboardMouse => match action {
                Activate => "Enter",
                Back => "Esc",
                Secondary => "Shift+Enter",
                Menu => "F10",
                PrevTab => "Ctrl+PgUp",
                NextTab => "Ctrl+PgDn",
                PageUp => "PgUp",
                PageDown => "PgDn",
            },
            Self::Xbox => match action {
                Activate => "A",
                Back => "B",
                Secondary => "Y",
                Menu => "Menu",
                PrevTab => "LB",
                NextTab => "RB",
                PageUp => "LT",
                PageDown => "RT",
            },
            Self::PlayStation => match action {
                Activate => "Cross",
                Back => "Circle",
                Secondary => "Triangle",
                Menu => "Options",
                PrevTab => "L1",
                NextTab => "R1",
                PageUp => "L2",
                PageDown => "R2",
            },
            Self::SteamDeck => match action {
                Activate => "A",
                Back => "B",
                Secondary => "Y",
                Menu => "Menu",
                PrevTab => "L1",
                NextTab => "R1",
                PageUp => "L2",
                PageDown => "R2",
            },
        }
    }
}

/// Replaces `{action}` placeholders (see [`HintAction::name`]) with the glyph of `device`; unknown
/// names and unmatched braces stay as written.
pub fn expand_hints(text: &str, device: HintDevice) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        match tail.find('}') {
            Some(close) => {
                let name = &tail[1..close];
                match HintAction::parse(name) {
                    Some(action) => out.push_str(device.glyph(action)),
                    None => out.push_str(&tail[..=close]),
                }
                rest = &tail[close + 1..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// One entry of the hint bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintEntry {
    /// The action whose glyph leads the entry.
    pub action: HintAction,
    /// What the action does on this screen ("Select", "Back").
    pub label: String,
}

/// The controller hint bar: the actions the current screen offers, shown only in
/// [`UiInputMode::Navigation`]. A screen or layer sets it; the renderer or a HUD reads
/// [`HintBar::lines`].
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct HintBar {
    /// Entries in display order.
    pub entries: Vec<HintEntry>,
}

impl HintBar {
    /// Replaces the entries.
    pub fn set<S: Into<String>>(&mut self, entries: impl IntoIterator<Item = (HintAction, S)>) {
        self.entries = entries
            .into_iter()
            .map(|(action, label)| HintEntry {
                action,
                label: label.into(),
            })
            .collect();
    }

    /// Whether the bar shows in `mode` (never in pointer mode: hover and click need no prompt).
    pub fn visible(&self, mode: UiInputMode) -> bool {
        mode == UiInputMode::Navigation && !self.entries.is_empty()
    }

    /// One `"<glyph> <label>"` line per entry for `device`.
    pub fn lines(&self, device: HintDevice) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| format!("{} {}", device.glyph(e.action), e.label))
            .collect()
    }
}

/// `Update`: picks the tooltip candidate, steps the machine and publishes [`Tooltip::visible`].
pub fn update_tooltips(world: &mut World) {
    let _span = tracing::trace_span!("ui_tooltips").entered();
    let dt = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.delta().as_secs_f32());
    let mode = world.get_resource::<UiInputMode>().copied().unwrap_or_default();
    let device = world.get_resource::<HintDevice>().copied().unwrap_or_default();
    let policy = world.get_resource::<TooltipPolicy>().copied().unwrap_or_default();
    let (pressed, primary_down, wheel) = world
        .get_resource::<UiPointer>()
        .map_or((None, false, [0.0, 0.0]), |p| {
            (p.pressed, p.primary_down, p.wheel)
        });

    let mut candidate: Option<TooltipCandidate> = None;
    let mut query = world.query::<(Entity, &Tooltip, Has<Hovered>, Has<Focused>)>();
    for (entity, tooltip, hovered, focused) in query.iter(world) {
        let trigger = match mode {
            UiInputMode::Navigation if focused => TooltipTrigger::Focus,
            UiInputMode::Pointer if hovered => TooltipTrigger::Hover,
            _ => continue,
        };
        let delay = match trigger {
            TooltipTrigger::Hover => tooltip.delay,
            TooltipTrigger::Focus => tooltip.focus_delay,
        };
        // Several candidates cannot normally coexist; the lowest entity keeps the choice stable.
        if candidate.is_none_or(|c| entity.to_bits() < c.entity.to_bits()) {
            candidate = Some(TooltipCandidate {
                entity,
                trigger,
                delay,
            });
        }
    }

    let (shown, waiting) = {
        let mut state = world.get_resource_or_insert_with(TooltipState::default);
        let mut dismiss = state.pending_dismiss.take();
        if dismiss.is_none()
            && let Some(c) = candidate
        {
            if primary_down && pressed == Some(c.entity) {
                dismiss = Some(TooltipDismiss::Press);
            } else if wheel != [0.0, 0.0] {
                dismiss = Some(TooltipDismiss::Scroll);
            }
        }
        state.machine.step(dt, &policy, candidate, dismiss);
        (state.machine.shown_entity(), state.machine.waiting())
    };

    let mut raw_text: Option<String> = None;
    let mut tooltips = world.query::<(Entity, &mut Tooltip, Option<&TooltipNavText>)>();
    for (entity, mut tooltip, nav_text) in tooltips.iter_mut(world) {
        let visible = shown == Some(entity);
        if tooltip.visible != visible {
            tooltip.visible = visible;
        }
        let elapsed = waiting.filter(|w| w.0 == entity).map_or(0.0, |w| w.1);
        if tooltip.elapsed != elapsed {
            tooltip.elapsed = elapsed;
        }
        if visible {
            let raw = match nav_text {
                Some(nav) if mode == UiInputMode::Navigation => nav.0.as_str(),
                _ => tooltip.text.as_str(),
            };
            raw_text = Some(raw.to_owned());
        }
    }
    // The project's glyph table and action bindings (ADR-7070); without a table, the built-in
    // families as before.
    let text = raw_text.map(
        |raw| match world.get_resource::<crate::input_glyphs::InputGlyphs>() {
            Some(glyphs) if !glyphs.0.glyphs.is_empty() => crate::input_glyphs::expand_glyphs(
                &raw,
                device,
                glyphs,
                world.get_resource::<regia_input::InputConfig>(),
            ),
            _ => expand_hints(&raw, device),
        },
    );
    if let Some(mut state) = world.get_resource_mut::<TooltipState>()
        && state.text != text
    {
        state.text = text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly representable, so accumulated waits carry no rounding error.
    const DT: f32 = 0.125;

    fn ents() -> (Entity, Entity) {
        let mut world = World::new();
        (world.spawn_empty().id(), world.spawn_empty().id())
    }

    fn hover(entity: Entity) -> Option<TooltipCandidate> {
        Some(TooltipCandidate {
            entity,
            trigger: TooltipTrigger::Hover,
            delay: 0.5,
        })
    }

    fn focus(entity: Entity) -> Option<TooltipCandidate> {
        Some(TooltipCandidate {
            entity,
            trigger: TooltipTrigger::Focus,
            delay: 0.8,
        })
    }

    /// Steps `n` times and returns the 1-based step on which the tooltip first opened.
    fn steps_until_open(
        m: &mut TooltipMachine,
        p: &TooltipPolicy,
        c: Option<TooltipCandidate>,
        n: u32,
    ) -> Option<u32> {
        (1..=n).find(|_| m.step(DT, p, c, None))
    }

    #[test]
    fn hover_opens_after_its_delay_and_not_before() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        assert_eq!(steps_until_open(&mut m, &p, hover(a), 20), Some(4));
        assert_eq!(m.shown_entity(), Some(a));
        assert_eq!(m.shown_trigger(), Some(TooltipTrigger::Hover));
    }

    #[test]
    fn focus_waits_longer_than_hover() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        assert_eq!(steps_until_open(&mut m, &p, focus(a), 20), Some(7));
        assert_eq!(m.shown_trigger(), Some(TooltipTrigger::Focus));
    }

    #[test]
    fn leaving_the_node_resets_the_wait() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        for _ in 0..3 {
            assert!(!m.step(DT, &p, hover(a), None));
        }
        assert!(!m.step(DT, &p, None, None));
        assert_eq!(m.waiting(), None);
        // Three more steps are still short of the full delay: the wait restarted.
        for _ in 0..3 {
            assert!(!m.step(DT, &p, hover(a), None));
        }
        assert!(m.step(DT, &p, hover(a), None));
    }

    #[test]
    fn leaving_closes_an_open_tooltip() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        assert!(!m.step(DT, &p, None, None));
        assert_eq!(m.shown_entity(), None);
    }

    #[test]
    fn warm_window_opens_the_next_tooltip_at_once() {
        let (a, b) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        // Moving straight to another node: it opens in the same step.
        assert!(m.step(DT, &p, hover(b), None));
        assert_eq!(m.shown_entity(), Some(b));
    }

    #[test]
    fn warm_window_expires() {
        let (a, b) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        // Off every node for longer than the warm window (0.4 s is 4 steps).
        for _ in 0..6 {
            m.step(DT, &p, None, None);
        }
        assert_eq!(steps_until_open(&mut m, &p, hover(b), 20), Some(4));
    }

    #[test]
    fn press_dismisses_and_stays_closed_until_the_pointer_leaves() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        assert!(!m.step(DT, &p, hover(a), Some(TooltipDismiss::Press)));
        assert_eq!(m.last_dismiss(), Some(TooltipDismiss::Press));
        for _ in 0..30 {
            assert!(!m.step(DT, &p, hover(a), None));
        }
        // Leave and come back: it works again.
        m.step(DT, &p, None, None);
        assert_eq!(steps_until_open(&mut m, &p, hover(a), 20), Some(4));
        assert_eq!(m.last_dismiss(), None);
    }

    #[test]
    fn pressing_before_the_delay_keeps_the_tooltip_from_opening() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        m.step(DT, &p, hover(a), None);
        m.step(DT, &p, hover(a), Some(TooltipDismiss::Press));
        for _ in 0..30 {
            assert!(!m.step(DT, &p, hover(a), None));
        }
    }

    #[test]
    fn cancel_and_scroll_dismiss_a_focus_tooltip() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        for reason in [TooltipDismiss::Cancel, TooltipDismiss::Scroll] {
            let mut m = TooltipMachine::default();
            steps_until_open(&mut m, &p, focus(a), 20);
            assert!(!m.step(DT, &p, focus(a), Some(reason)));
            assert_eq!(m.last_dismiss(), Some(reason));
            for _ in 0..30 {
                assert!(!m.step(DT, &p, focus(a), None));
            }
        }
    }

    #[test]
    fn dismissing_with_no_candidate_is_a_no_op() {
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        assert!(!m.step(DT, &p, None, Some(TooltipDismiss::Cancel)));
        assert_eq!(m.last_dismiss(), None);
    }

    #[test]
    fn auto_hide_closes_and_suppresses() {
        let (a, _) = ents();
        let p = TooltipPolicy {
            auto_hide: Some(0.3),
            ..TooltipPolicy::default()
        };
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        assert!(m.step(DT, &p, hover(a), None));
        assert!(m.step(DT, &p, hover(a), None));
        assert!(!m.step(DT, &p, hover(a), None));
        assert_eq!(m.last_dismiss(), Some(TooltipDismiss::Timeout));
        for _ in 0..30 {
            assert!(!m.step(DT, &p, hover(a), None));
        }
    }

    #[test]
    fn without_auto_hide_the_tooltip_stays_open() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        steps_until_open(&mut m, &p, hover(a), 20);
        for _ in 0..200 {
            assert!(m.step(DT, &p, hover(a), None));
        }
    }

    #[test]
    fn hover_to_focus_on_the_same_node_keeps_the_wait() {
        let (a, _) = ents();
        let p = TooltipPolicy::default();
        let mut m = TooltipMachine::default();
        for _ in 0..3 {
            m.step(DT, &p, hover(a), None);
        }
        // The trigger changes but the node does not: the accumulated wait stays.
        let waited = m.waiting().map(|w| w.1);
        m.step(DT, &p, focus(a), None);
        assert!(m.waiting().map(|w| w.1) > waited);
    }

    #[test]
    fn glyphs_follow_the_device() {
        assert_eq!(HintDevice::KeyboardMouse.glyph(HintAction::Activate), "Enter");
        assert_eq!(HintDevice::Xbox.glyph(HintAction::Activate), "A");
        assert_eq!(HintDevice::PlayStation.glyph(HintAction::Activate), "Cross");
        assert_eq!(HintDevice::PlayStation.glyph(HintAction::Back), "Circle");
        assert_eq!(HintDevice::SteamDeck.glyph(HintAction::NextTab), "R1");
        assert_eq!(HintDevice::Xbox.glyph(HintAction::NextTab), "RB");
    }

    #[test]
    fn every_action_has_a_glyph_on_every_device_and_a_round_trip_name() {
        for device in [
            HintDevice::KeyboardMouse,
            HintDevice::Xbox,
            HintDevice::PlayStation,
            HintDevice::SteamDeck,
        ] {
            for action in HintAction::ALL {
                assert!(!device.glyph(action).is_empty());
                assert_eq!(HintAction::parse(action.name()), Some(action));
            }
        }
        assert_eq!(HintAction::parse("nope"), None);
    }

    #[test]
    fn placeholders_expand_and_unknown_text_survives() {
        assert_eq!(
            expand_hints("Press {activate} to buy, {back} to cancel", HintDevice::Xbox),
            "Press A to buy, B to cancel"
        );
        assert_eq!(
            expand_hints("Press {activate}", HintDevice::PlayStation),
            "Press Cross"
        );
        assert_eq!(
            expand_hints("{nope} {activate", HintDevice::Xbox),
            "{nope} {activate"
        );
        assert_eq!(expand_hints("plain", HintDevice::Xbox), "plain");
        assert_eq!(expand_hints("héllo {menu} ü", HintDevice::Xbox), "héllo Menu ü");
    }

    #[test]
    fn hint_bar_shows_only_in_navigation_mode() {
        let mut bar = HintBar::default();
        assert!(!bar.visible(UiInputMode::Navigation), "empty bar is hidden");
        bar.set([(HintAction::Activate, "Select"), (HintAction::Back, "Back")]);
        assert!(bar.visible(UiInputMode::Navigation));
        assert!(!bar.visible(UiInputMode::Pointer));
        assert_eq!(bar.lines(HintDevice::Xbox), vec!["A Select", "B Back"]);
        assert_eq!(
            bar.lines(HintDevice::PlayStation),
            vec!["Cross Select", "Circle Back"]
        );
    }
}
