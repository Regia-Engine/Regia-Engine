// SPDX-License-Identifier: MPL-2.0
//! Radial menu (weapon wheel, unit HUD wheel) for gamepad stick, mouse and touch (DO27 ask 16).
//!
//! Three layers:
//!
//! 1. [`RadialGeometry`]: pure math. Maps a pointing vector to a sector with a dead zone and
//!    hysteresis (no flicker on a boundary), for 2 to 12 sectors.
//! 2. [`RadialMenu::step`]: the input state machine (open modes, confirm modes, selection,
//!    open/close animation). It takes a plain [`RadialInput`] and returns [`RadialEventKind`]s, so
//!    it is testable without an `App`.
//! 3. [`radial_menu_update`]: the ECS system. It gathers stick, mouse, touch and
//!    [`crate::focus::UiNavActions`] input, calls `step`, writes [`RadialEvent`] messages, and
//!    keeps one chip node per sector under the wheel's node, coloured from the active
//!    [`crate::theme::UiTheme`].
//!
//! The wheel draws with the existing quad kinds (rounded rectangles, text, images): each sector is a
//! chip placed on the ring, not an arc-shaped quad, so no renderer change is needed.
//!
//! Angles are radians measured clockwise from "up"; sector 0 is centred on the start angle
//! (default up).

use std::f32::consts::{PI, TAU};

use regia_app::prelude::*;
use regia_platform::{GamepadAxis, GamepadAxisChanged};

use crate::color::UiColor;
use crate::focus::UiNavActions;
use crate::interaction::UiPointer;
use crate::layout::LayoutDirty;
use crate::node::{Background, BackgroundKind, Border, ComputedNode, Opacity, UiImage, UiNode, ZOffset};
use crate::style::{Align, Dimension, Display, Edges, FlexDirection, JustifyContent, PositionType, Style};
use crate::text::{Text, TextSpan, TextStyle, WrapMode};
use crate::theme::UiThemes;
use crate::widgets::VirtualButtons;

/// Fewest sectors a wheel supports.
pub const MIN_SECTORS: usize = 2;
/// Most sectors a wheel supports (extra sectors are not shown).
pub const MAX_SECTORS: usize = 12;
/// While a sector is selected the stick may fall to this fraction of the dead zone before the
/// selection clears (dead-zone hysteresis).
const DEAD_ZONE_EXIT: f32 = 0.8;

/// Angle of a pointing vector `(x right, y up)`, radians clockwise from up, in `[0, TAU)`.
pub fn angle_from_up(x: f32, y: f32) -> f32 {
    let angle = x.atan2(y).rem_euclid(TAU);
    // A tiny negative angle plus a turn rounds to exactly a turn in f32, which is outside `[0, TAU)`.
    if angle >= TAU { 0.0 } else { angle }
}

/// Smallest signed difference `a - b` between two angles, in `[-PI, PI]`.
pub fn angle_delta(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(TAU);
    if d > PI { d - TAU } else { d }
}

/// The pure sector math of a wheel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadialGeometry {
    /// Number of sectors, clamped to [`MIN_SECTORS`]`..=`[`MAX_SECTORS`].
    pub sectors: usize,
    /// Angle of the centre of sector 0 (radians clockwise from up).
    pub start: f32,
    /// Pointing magnitude below which nothing is selected (`0..1`).
    pub dead_zone: f32,
    /// How far past a boundary the pointer must go before the selection changes, as a fraction of
    /// one sector's width (`0..=0.5`).
    pub hysteresis: f32,
}

impl Default for RadialGeometry {
    fn default() -> Self {
        Self {
            sectors: 8,
            start: 0.0,
            dead_zone: 0.35,
            hysteresis: 0.12,
        }
    }
}

impl RadialGeometry {
    /// Sector count after clamping.
    pub fn count(&self) -> usize {
        self.sectors.clamp(MIN_SECTORS, MAX_SECTORS)
    }

    /// Angular width of one sector.
    pub fn width(&self) -> f32 {
        TAU / self.count() as f32
    }

    /// Angle of the centre of sector `i`.
    pub fn centre_angle(&self, i: usize) -> f32 {
        (self.start + self.width() * i as f32).rem_euclid(TAU)
    }

    /// The sector an angle falls in, with no dead zone and no hysteresis.
    pub fn sector_at(&self, angle: f32) -> usize {
        let w = self.width();
        let a = (angle - self.start + w * 0.5).rem_euclid(TAU);
        ((a / w) as usize).min(self.count() - 1)
    }

    /// The sector selected by pointing vector `v` (`x` right, `y` up, length `0..=1` for a
    /// stick). `current` is the sector selected last frame: it is kept until the angle is more
    /// than [`hysteresis`](Self::hysteresis) of a sector past its boundary, and a selected sector
    /// survives the magnitude dipping to 80 % of the dead zone. `None` is the dead zone.
    pub fn select(&self, current: Option<usize>, v: [f32; 2]) -> Option<usize> {
        let mag = (v[0] * v[0] + v[1] * v[1]).sqrt();
        let dz = if current.is_some() {
            self.dead_zone * DEAD_ZONE_EXIT
        } else {
            self.dead_zone
        };
        // Non-finite input (NaN, or an infinite axis whose angle is NaN) selects nothing, and a
        // NaN dead zone must not disable the dead zone.
        if !v[0].is_finite() || !v[1].is_finite() || dz.is_nan() || mag <= dz || mag == 0.0 {
            return None;
        }
        let angle = angle_from_up(v[0], v[1]);
        if let Some(c) = current.filter(|c| *c < self.count()) {
            // Capped below half a turn: with two sectors and the maximum hysteresis the keep
            // arc would cover the whole circle and pointing could never change the sector.
            let half = (self.width() * (0.5 + self.hysteresis.clamp(0.0, 0.5))).min(PI - 0.01);
            if angle_delta(angle, self.centre_angle(c)).abs() <= half {
                return Some(c);
            }
        }
        Some(self.sector_at(angle))
    }
}

/// What a wheel has highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadialSelection {
    /// Nothing.
    #[default]
    None,
    /// The centre action.
    Center,
    /// Sector `n`.
    Sector(usize),
}

impl RadialSelection {
    /// The sector index, if a sector is selected.
    pub fn sector(self) -> Option<usize> {
        match self {
            Self::Sector(i) => Some(i),
            _ => None,
        }
    }
}

/// One entry of a wheel.
#[derive(Debug, Clone, PartialEq)]
pub struct RadialSector {
    /// Stable identifier the host maps to an action (`"rifle"`, `"order_hold"`). Sent in
    /// [`RadialEventKind::Select`].
    pub id: String,
    /// Label under the icon.
    pub label: String,
    /// Renderer image id (see [`crate::UiRenderer::add_image_rgba`]) drawn above the label.
    pub icon: Option<u64>,
    /// A disabled sector shows dimmed, can be hovered but not selected.
    pub enabled: bool,
}

impl RadialSector {
    /// An enabled sector without an icon.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            enabled: true,
        }
    }

    /// Adds an icon.
    pub fn with_icon(mut self, image: u64) -> Self {
        self.icon = Some(image);
        self
    }

    /// Sets whether the sector can be selected.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// How the wheel opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadialOpen {
    /// Open while the open control is held (`open_action` or [`RadialMenu::set_held`]).
    #[default]
    Hold,
    /// The open control toggles it.
    Toggle,
}

/// What confirms the highlighted sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadialConfirm {
    /// Releasing: the held open control (hold mode) or the mouse button or finger.
    #[default]
    Release,
    /// Pressing the mouse button or touching a sector; releasing the open control cancels.
    Press,
}

/// Which stick drives the highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadialStick {
    /// The right stick.
    #[default]
    Right,
    /// The left stick.
    Left,
    /// Whichever is deflected further.
    Either,
}

/// Colour overrides; unset colours come from the active theme.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RadialStyle {
    /// Sector fill (theme `surface_hi`).
    pub sector: Option<UiColor>,
    /// Highlighted sector fill (theme `primary`).
    pub selected: Option<UiColor>,
    /// Label colour (theme `text`).
    pub text: Option<UiColor>,
    /// Highlighted label colour (theme `on_primary`).
    pub selected_text: Option<UiColor>,
    /// Backdrop disc (theme `overlay`).
    pub backdrop: Option<UiColor>,
}

/// A radial menu. Put it on a [`UiNode`] sized `2 * outer_radius` (or use [`spawn_radial_menu`]);
/// the system keeps the sector chips as its children.
#[derive(Component, Debug, Clone)]
pub struct RadialMenu {
    /// The sectors, clockwise from the start angle. Two to twelve are shown.
    pub sectors: Vec<RadialSector>,
    /// An optional action in the middle (dead-zone pointing selects it).
    pub center: Option<RadialSector>,
    /// Hub radius in UI px: the pointer inside it means "centre".
    pub inner_radius: f32,
    /// Wheel radius in UI px.
    pub outer_radius: f32,
    /// Space between chips in UI px.
    pub gap: f32,
    /// Angle of sector 0's centre in degrees clockwise from up.
    pub start_angle_deg: f32,
    /// Stick magnitude below which nothing is selected.
    pub dead_zone: f32,
    /// Touch drag distance (in outer radii) below which nothing is selected.
    pub touch_dead_zone: f32,
    /// Boundary hysteresis as a fraction of a sector (`0..=0.5`).
    pub hysteresis: f32,
    /// Touch drag distance, in outer radii, that counts as full deflection.
    pub touch_range: f32,
    /// Hold or toggle.
    pub open_mode: RadialOpen,
    /// Release or press.
    pub confirm_mode: RadialConfirm,
    /// Which stick selects.
    pub stick: RadialStick,
    /// Name of a [`VirtualButtons`] action that is the open control (empty = the host calls
    /// [`set_held`](Self::set_held) / [`toggle`](Self::toggle) / [`open`](Self::open)).
    pub open_action: String,
    /// Open/close animation time in seconds (0 = instant).
    pub animation_secs: f32,
    /// Colour overrides.
    pub style: RadialStyle,
    /// Highlighted entry (read-only for hosts; written by the system).
    pub selection: RadialSelection,
    /// Whether the wheel is logically open (read-only for hosts).
    pub open: bool,
    /// Animation progress `0..=1` (read-only for hosts).
    pub progress: f32,
    held_api: bool,
    was_held: bool,
    pending_open: Option<bool>,
    pending_toggle: bool,
    pending_step: i32,
    contact: bool,
    touch: Option<(u64, [f32; 2])>,
    last_cursor: Option<[f32; 2]>,
    last_stick_active: bool,
    last_open_action: bool,
    signature: u64,
    chips: Vec<Entity>,
}

impl Default for RadialMenu {
    fn default() -> Self {
        Self {
            sectors: Vec::new(),
            center: None,
            inner_radius: 56.0,
            outer_radius: 180.0,
            gap: 6.0,
            start_angle_deg: 0.0,
            dead_zone: 0.35,
            touch_dead_zone: 0.2,
            hysteresis: 0.12,
            touch_range: 0.6,
            open_mode: RadialOpen::Hold,
            confirm_mode: RadialConfirm::Release,
            stick: RadialStick::Right,
            open_action: String::new(),
            animation_secs: 0.12,
            style: RadialStyle::default(),
            selection: RadialSelection::None,
            open: false,
            progress: 0.0,
            held_api: false,
            was_held: false,
            pending_open: None,
            pending_toggle: false,
            pending_step: 0,
            contact: false,
            touch: None,
            last_cursor: None,
            last_stick_active: false,
            last_open_action: false,
            signature: 0,
            chips: Vec::new(),
        }
    }
}

/// Animation phase of a wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadialPhase {
    /// Hidden.
    Closed,
    /// Opening animation running.
    Opening,
    /// Fully open.
    Open,
    /// Closing animation running.
    Closing,
}

/// Something that happened to a wheel.
#[derive(Debug, Clone, PartialEq)]
pub enum RadialEventKind {
    /// The wheel opened.
    Opened,
    /// The highlight moved (to [`RadialSelection::None`] when it left every sector).
    Hover(RadialSelection),
    /// An entry was confirmed (the wheel closes).
    Select {
        /// Which entry.
        selection: RadialSelection,
        /// The entry's [`RadialSector::id`].
        id: String,
    },
    /// The wheel was dismissed without a selection (the wheel closes).
    Cancel,
    /// The wheel finished closing logically (follows `Select` and `Cancel`).
    Closed,
}

/// A [`RadialEventKind`] with the wheel it came from, delivered as a message.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct RadialEvent {
    /// The wheel's node.
    pub target: Entity,
    /// What happened.
    pub kind: RadialEventKind,
}

/// One frame of input for [`RadialMenu::step`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RadialInput {
    /// The open control is held (hold mode).
    pub held: bool,
    /// The open control was pressed this frame (toggle mode).
    pub toggle: bool,
    /// Pointing vector, `x` right and `y` up, with `1.0` at the full-deflection ring. `None`
    /// keeps the current highlight (no pointing device moved).
    pub vector: Option<[f32; 2]>,
    /// Dead zone of `vector` (`0..1`).
    pub dead_zone: f32,
    /// Explicit confirm (gamepad A, Enter).
    pub confirm: bool,
    /// Mouse button or finger went down.
    pub contact_down: bool,
    /// Mouse button or finger came up.
    pub contact_up: bool,
    /// Explicit cancel (gamepad B, Escape).
    pub cancel: bool,
    /// Step the highlight by this many sectors (d-pad, shoulders).
    pub step: i32,
}

impl RadialMenu {
    /// A wheel with these sectors and default tuning.
    pub fn new(sectors: Vec<RadialSector>) -> Self {
        Self {
            sectors,
            ..Self::default()
        }
    }

    /// Sets the open mode.
    pub fn with_open(mut self, mode: RadialOpen) -> Self {
        self.open_mode = mode;
        self
    }

    /// Sets the confirm mode.
    pub fn with_confirm(mut self, mode: RadialConfirm) -> Self {
        self.confirm_mode = mode;
        self
    }

    /// Adds a centre action.
    pub fn with_center(mut self, center: RadialSector) -> Self {
        self.center = Some(center);
        self
    }

    /// Sets the hub and wheel radii in UI px.
    pub fn with_radii(mut self, inner: f32, outer: f32) -> Self {
        self.inner_radius = inner;
        self.outer_radius = outer;
        self
    }

    /// Names the [`VirtualButtons`] action that opens the wheel.
    pub fn with_open_action(mut self, action: impl Into<String>) -> Self {
        self.open_action = action.into();
        self
    }

    /// Opens the wheel on the next update.
    pub fn open(&mut self) {
        self.pending_open = Some(true);
    }

    /// Dismisses the wheel on the next update (emits `Cancel`).
    pub fn close(&mut self) {
        self.pending_open = Some(false);
    }

    /// Toggles the wheel on the next update (toggle mode).
    pub fn toggle(&mut self) {
        self.pending_toggle = true;
    }

    /// Tells a hold-mode wheel whether the open control is held. Releasing confirms the
    /// highlight (or cancels under [`RadialConfirm::Press`]).
    pub fn set_held(&mut self, held: bool) {
        self.held_api = held;
    }

    /// Steps the highlight `n` sectors clockwise (negative = counter-clockwise) on the next update.
    pub fn step_selection(&mut self, n: i32) {
        self.pending_step += n;
    }

    /// The pure sector math with this wheel's tuning, for `dead_zone`.
    pub fn geometry(&self, dead_zone: f32) -> RadialGeometry {
        RadialGeometry {
            sectors: self.sectors.len().clamp(MIN_SECTORS, MAX_SECTORS),
            start: self.start_angle_deg.to_radians(),
            dead_zone,
            hysteresis: self.hysteresis,
        }
    }

    /// Number of sectors shown.
    pub fn shown(&self) -> usize {
        self.sectors.len().min(MAX_SECTORS)
    }

    /// Animation phase.
    pub fn phase(&self) -> RadialPhase {
        match (self.open, self.progress) {
            (true, p) if p >= 1.0 => RadialPhase::Open,
            (true, _) => RadialPhase::Opening,
            (false, p) if p > 0.0 => RadialPhase::Closing,
            (false, _) => RadialPhase::Closed,
        }
    }

    /// The entry under `selection`.
    pub fn entry(&self, selection: RadialSelection) -> Option<&RadialSector> {
        match selection {
            RadialSelection::None => None,
            RadialSelection::Center => self.center.as_ref(),
            RadialSelection::Sector(i) => self.sectors.get(i).filter(|_| i < MAX_SECTORS),
        }
    }

    /// Clears per-open state and opens.
    fn do_open(&mut self, out: &mut Vec<RadialEventKind>) -> bool {
        if self.open {
            return false;
        }
        self.open = true;
        self.selection = RadialSelection::None;
        self.contact = false;
        self.touch = None;
        self.last_cursor = None;
        out.push(RadialEventKind::Opened);
        true
    }

    fn do_close(&mut self, out: &mut Vec<RadialEventKind>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.selection = RadialSelection::None;
        self.contact = false;
        self.touch = None;
        out.push(RadialEventKind::Closed);
    }

    fn cancel(&mut self, out: &mut Vec<RadialEventKind>) {
        out.push(RadialEventKind::Cancel);
        self.do_close(out);
    }

    /// Confirms the highlight; an empty or disabled highlight cancels when `cancel_if_empty`,
    /// otherwise it is ignored and the wheel stays open.
    fn confirm(&mut self, cancel_if_empty: bool, out: &mut Vec<RadialEventKind>) {
        match self.entry(self.selection).filter(|e| e.enabled) {
            Some(entry) => {
                out.push(RadialEventKind::Select {
                    selection: self.selection,
                    id: entry.id.clone(),
                });
                self.do_close(out);
            }
            None if cancel_if_empty => self.cancel(out),
            None => {}
        }
    }

    fn set_selection(&mut self, new: RadialSelection, out: &mut Vec<RadialEventKind>) {
        if new != self.selection {
            self.selection = new;
            out.push(RadialEventKind::Hover(new));
        }
    }

    /// Advances the state machine by one frame of `input` and `dt` seconds. Events are appended
    /// to `out` in the order they happen.
    pub fn step(&mut self, input: &RadialInput, dt: f32, out: &mut Vec<RadialEventKind>) {
        let held = input.held || self.held_api;
        let held_edge = (held != self.was_held).then_some(held);
        self.was_held = held;
        let pending = self.pending_open.take();
        let toggled = std::mem::take(&mut self.pending_toggle) || input.toggle;
        let mut just_opened = false;
        match self.open_mode {
            RadialOpen::Hold => match held_edge {
                Some(true) => just_opened |= self.do_open(out),
                Some(false) if self.open => match self.confirm_mode {
                    RadialConfirm::Release => self.confirm(true, out),
                    RadialConfirm::Press => self.cancel(out),
                },
                _ => {}
            },
            RadialOpen::Toggle => {
                if toggled {
                    if self.open {
                        self.cancel(out);
                    } else {
                        just_opened |= self.do_open(out);
                    }
                }
            }
        }
        match pending {
            Some(true) => just_opened |= self.do_open(out),
            Some(false) if self.open => self.cancel(out),
            _ => {}
        }
        let stepped = std::mem::take(&mut self.pending_step) + input.step;
        if self.open && !just_opened {
            if let Some(v) = input.vector {
                let geo = self.geometry(input.dead_zone);
                let current = self.selection.sector();
                let new = match geo.select(current, v).filter(|i| *i < self.shown()) {
                    Some(i) => RadialSelection::Sector(i),
                    None if self.center.is_some() => RadialSelection::Center,
                    None => RadialSelection::None,
                };
                self.set_selection(new, out);
            }
            if stepped != 0 {
                self.step_highlight(stepped, out);
            }
            if input.contact_down {
                self.contact = true;
                if self.confirm_mode == RadialConfirm::Press {
                    self.confirm(false, out);
                }
            }
            if self.open && input.contact_up {
                let had_contact = std::mem::take(&mut self.contact);
                if had_contact && self.confirm_mode == RadialConfirm::Release {
                    self.confirm(true, out);
                }
            }
            if self.open && input.confirm {
                self.confirm(false, out);
            }
            if self.open && input.cancel {
                self.cancel(out);
            }
        }
        let target = if self.open { 1.0 } else { 0.0 };
        if self.animation_secs <= 0.0 {
            self.progress = target;
        } else {
            let delta = dt.max(0.0) / self.animation_secs;
            self.progress = if target > self.progress {
                (self.progress + delta).min(1.0)
            } else {
                (self.progress - delta).max(0.0)
            };
        }
    }

    fn step_highlight(&mut self, n: i32, out: &mut Vec<RadialEventKind>) {
        let count = self.shown() as i32;
        if count == 0 {
            return;
        }
        let dir = n.signum();
        let mut idx = match self.selection {
            RadialSelection::Sector(i) => i as i32,
            _ if dir > 0 => -1,
            _ => count,
        };
        for _ in 0..n.unsigned_abs() {
            // Skip disabled sectors, at most one lap.
            for _ in 0..count {
                idx = (idx + dir).rem_euclid(count);
                if self.sectors[idx as usize].enabled {
                    break;
                }
            }
        }
        self.set_selection(RadialSelection::Sector(idx as usize), out);
    }
}

/// Stick positions, written from [`GamepadAxisChanged`] by [`radial_stick_input`]. A host that
/// reads its sticks through another layer can write them itself (`x` right, `y` up).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
pub struct RadialSticks {
    /// Left stick.
    pub left: [f32; 2],
    /// Right stick.
    pub right: [f32; 2],
}

/// `Update`: tracks the sticks.
pub fn radial_stick_input(mut axes: MessageReader<GamepadAxisChanged>, mut sticks: ResMut<RadialSticks>) {
    let _span = tracing::debug_span!("radial_stick_input").entered();
    for e in axes.read() {
        match e.axis {
            GamepadAxis::LeftStickX => sticks.left[0] = e.value,
            GamepadAxis::LeftStickY => sticks.left[1] = e.value,
            GamepadAxis::RightStickX => sticks.right[0] = e.value,
            GamepadAxis::RightStickY => sticks.right[1] = e.value,
            _ => {}
        }
    }
}

/// Marks a chip node as belonging to a wheel.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadialChip {
    /// The wheel.
    pub menu: Entity,
    /// What the chip shows.
    pub selection: RadialSelection,
    label: Option<Entity>,
}

/// Spawns a wheel centred in `parent` and returns its node. The wheel starts closed.
pub fn spawn_radial_menu(commands: &mut Commands, parent: Entity, menu: RadialMenu) -> Entity {
    let r = menu.outer_radius;
    let entity = commands
        .spawn((
            UiNode,
            crate::node::NodeStyle::new(Style {
                display: Display::None,
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Percent(50.0),
                    top: Dimension::Percent(50.0),
                    ..Edges::default()
                },
                margin: Edges {
                    left: Dimension::Points(-r),
                    top: Dimension::Points(-r),
                    ..Edges::default()
                },
                size: [Dimension::Points(2.0 * r), Dimension::Points(2.0 * r)],
                ..Default::default()
            }),
            ZOffset(100),
            Opacity(0.0),
            menu,
        ))
        .id();
    commands.entity(parent).add_child(entity);
    entity
}

fn signature(menu: &RadialMenu) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in menu.sectors.iter().take(MAX_SECTORS).chain(menu.center.iter()) {
        s.id.hash(&mut h);
        s.label.hash(&mut h);
        s.icon.hash(&mut h);
    }
    menu.sectors.len().min(MAX_SECTORS).hash(&mut h);
    menu.center.is_some().hash(&mut h);
    for f in [
        menu.inner_radius,
        menu.outer_radius,
        menu.gap,
        menu.start_angle_deg,
    ] {
        f.to_bits().hash(&mut h);
    }
    h.finish()
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn set_if_changed<C: Component + PartialEq>(world: &mut World, entity: Entity, value: C) {
    if world.get::<C>(entity).is_none_or(|c| *c != value) {
        world.entity_mut(entity).insert(value);
    }
}

/// `Update` (after `ui_gamepad_navigation`, before `ui_focus_navigation`): runs every wheel.
pub fn radial_menu_update(world: &mut World) {
    let _span = tracing::trace_span!("radial_menu").entered();
    let mut menus: Vec<Entity> = world
        .query_filtered::<Entity, With<RadialMenu>>()
        .iter(world)
        .collect();
    // Archetype order must not decide which wheel consumes the shared nav actions.
    menus.sort_by_key(|e| e.to_bits());
    if menus.is_empty() {
        return;
    }
    let dt = world
        .get_resource::<regia_app::RealTime>()
        .map_or(0.0, |t| t.delta().as_secs_f32());
    let pointer = world.get_resource::<UiPointer>().cloned().unwrap_or_default();
    let sticks = world.get_resource::<RadialSticks>().copied().unwrap_or_default();
    let open_actions = world.get_resource::<VirtualButtons>().map(|b| b.0.clone());
    let mut nav = world.get_resource::<UiNavActions>().copied().unwrap_or_default();
    let nav_before = nav;
    let colors = world
        .get_resource::<UiThemes>()
        .map(|t| (t.current().colors.clone(), t.current().font_px))
        .unwrap_or_default();
    let mut events = Vec::new();
    for entity in menus {
        let Some(mut menu) = world.entity_mut(entity).take::<RadialMenu>() else {
            continue;
        };
        let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
        let sig = signature(&menu);
        if sig != menu.signature {
            for chip in std::mem::take(&mut menu.chips) {
                if let Ok(e) = world.get_entity_mut(chip) {
                    e.despawn();
                }
            }
            menu.chips = build_chips(world, entity, &menu, &colors);
            menu.signature = sig;
        }
        let mut input = RadialInput::default();
        if let Some(buttons) = &open_actions
            && !menu.open_action.is_empty()
        {
            let down = buttons.get(&menu.open_action).copied().unwrap_or(false);
            input.held = down;
            input.toggle = down && !menu.last_open_action;
            menu.last_open_action = down;
        }
        let active = menu.open && computed.rect[2] > 0.0;
        if active {
            gather_pointing(&mut menu, &mut input, &computed, &pointer, &sticks);
            if nav.submit {
                input.confirm = true;
            }
            if nav.cancel {
                input.cancel = true;
            }
            if nav.left || nav.up || nav.prev {
                input.step -= 1;
            }
            if nav.right || nav.down || nav.next {
                input.step += 1;
            }
        }
        events.clear();
        menu.step(&input, dt, &mut events);
        if menu.phase() != RadialPhase::Closed || !events.is_empty() {
            // The wheel owns the controller while open: focus navigation must not also act.
            nav.submit = false;
            nav.cancel = false;
            nav.left = false;
            nav.right = false;
            nav.up = false;
            nav.down = false;
            nav.prev = false;
            nav.next = false;
            if menu.open
                && let Some(mut p) = world.get_resource_mut::<UiPointer>()
            {
                p.consumed_pointer = true;
            }
        }
        update_visuals(world, entity, &menu, &colors);
        for kind in events.drain(..) {
            world.write_message(RadialEvent { target: entity, kind });
        }
        world.entity_mut(entity).insert(menu);
    }
    if let Some(mut res) = world.get_resource_mut::<UiNavActions>() {
        let res = &mut *res;
        // Only clear what a wheel took; keep flags this system never touched.
        if nav_before.submit && !nav.submit {
            res.submit = false;
        }
        if nav_before.cancel && !nav.cancel {
            res.cancel = false;
        }
        for (before, now, flag) in [
            (nav_before.left, nav.left, &mut res.left),
            (nav_before.right, nav.right, &mut res.right),
            (nav_before.up, nav.up, &mut res.up),
            (nav_before.down, nav.down, &mut res.down),
            (nav_before.prev, nav.prev, &mut res.prev),
            (nav_before.next, nav.next, &mut res.next),
        ] {
            if before && !now {
                *flag = false;
            }
        }
    }
}

/// Fills the pointing part of `input` from stick, touch or mouse (in that priority).
fn gather_pointing(
    menu: &mut RadialMenu,
    input: &mut RadialInput,
    computed: &ComputedNode,
    pointer: &UiPointer,
    sticks: &RadialSticks,
) {
    let stick = match menu.stick {
        RadialStick::Right => sticks.right,
        RadialStick::Left => sticks.left,
        RadialStick::Either => {
            let len = |s: [f32; 2]| s[0] * s[0] + s[1] * s[1];
            if len(sticks.left) > len(sticks.right) {
                sticks.left
            } else {
                sticks.right
            }
        }
    };
    // Below the dead zone the stick is drift, not intent: the cursor keeps the wheel.
    let wake = menu.dead_zone
        * if menu.last_stick_active {
            DEAD_ZONE_EXIT
        } else {
            1.0
        };
    let stick_active = stick[0] * stick[0] + stick[1] * stick[1] > wake * wake;
    let outer = (menu.outer_radius * computed.scale.max(f32::EPSILON)).max(1.0);
    let centre = [
        computed.rect[0] + computed.rect[2] * 0.5,
        computed.rect[1] + computed.rect[3] * 0.5,
    ];
    // A finger: its drag from where it landed.
    let touch_pos = pointer
        .active_touch
        .and_then(|id| pointer.touches.get(&id).map(|p| (id, *p)));
    match (touch_pos, menu.touch) {
        (Some((id, p)), None) => {
            menu.touch = Some((id, p));
            input.contact_down = true;
        }
        (Some((id, p)), Some((tid, origin))) if id == tid => {
            let range = outer * menu.touch_range.max(0.05);
            input.vector = Some([(p[0] - origin[0]) / range, -(p[1] - origin[1]) / range]);
            input.dead_zone = menu.touch_dead_zone;
            return;
        }
        (None, Some(_)) => {
            menu.touch = None;
            input.contact_up = true;
            return;
        }
        _ => {}
    }
    if menu.touch.is_some() {
        return;
    }
    if stick_active || menu.last_stick_active {
        menu.last_stick_active = stick_active;
        input.vector = Some(stick);
        input.dead_zone = menu.dead_zone;
    } else if let Some(c) = pointer.cursor {
        // The mouse points at the wheel's centre; it only takes over once it moves.
        if menu.last_cursor.is_some_and(|l| l != c) {
            input.vector = Some([(c[0] - centre[0]) / outer, -(c[1] - centre[1]) / outer]);
            input.dead_zone = (menu.inner_radius / menu.outer_radius.max(1.0)).clamp(0.0, 0.9);
        }
        menu.last_cursor = Some(c);
    }
    // Mouse button edges (a touch drives its own).
    let down = pointer.primary_down;
    match (down, menu.contact) {
        (true, false) => input.contact_down = true,
        (false, true) if pointer.active_touch.is_none() => input.contact_up = true,
        _ => {}
    }
}

fn chip_geometry(
    menu: &RadialMenu,
    selection: RadialSelection,
    t: f32,
    highlighted: bool,
) -> ([f32; 2], f32) {
    let outer = menu.outer_radius;
    let inner = menu.inner_radius.min(outer * 0.9);
    let eased = smooth(t);
    let scale = 0.7 + 0.3 * eased;
    match selection {
        RadialSelection::Sector(i) => {
            let geo = menu.geometry(menu.dead_zone);
            let a = geo.centre_angle(i);
            let rm = (inner + outer) * 0.5 * scale;
            let ring = (outer - inner - menu.gap).max(16.0);
            let arc = (2.0 * rm * (geo.width() * 0.5).sin() - menu.gap).max(16.0);
            let size =
                ring.min(arc).clamp(16.0, 200.0) * (0.8 + 0.2 * eased) * if highlighted { 1.1 } else { 1.0 };
            (
                [
                    outer + a.sin() * rm - size * 0.5,
                    outer - a.cos() * rm - size * 0.5,
                ],
                size,
            )
        }
        _ => {
            let size = (inner * 2.0 - menu.gap).max(16.0)
                * (0.8 + 0.2 * eased)
                * if highlighted { 1.06 } else { 1.0 };
            ([outer - size * 0.5, outer - size * 0.5], size)
        }
    }
}

fn build_chips(
    world: &mut World,
    root: Entity,
    menu: &RadialMenu,
    colors: &(crate::theme::ThemeColors, f32),
) -> Vec<Entity> {
    let mut chips = Vec::new();
    let mut entries: Vec<(RadialSelection, &RadialSector)> = menu
        .sectors
        .iter()
        .take(MAX_SECTORS)
        .enumerate()
        .map(|(i, s)| (RadialSelection::Sector(i), s))
        .collect();
    if let Some(c) = &menu.center {
        entries.push((RadialSelection::Center, c));
    }
    for (selection, sector) in entries {
        let (pos, size) = chip_geometry(menu, selection, 1.0, false);
        let mut chip = world.spawn((
            UiNode,
            crate::node::NodeStyle::new(chip_style(pos, size)),
            Background {
                kind: BackgroundKind::Color(colors.0.surface_hi),
            },
            Border {
                color: colors.0.border,
                widths: [1.0; 4],
                radius: [size * 0.3; 4],
            },
        ));
        let chip_id = chip.id();
        chip.with_children(|c| {
            if let Some(icon) = sector.icon {
                c.spawn((
                    UiNode,
                    crate::node::NodeStyle::new(Style {
                        size: [Dimension::Points(size * 0.5), Dimension::Points(size * 0.5)],
                        ..Default::default()
                    }),
                    UiImage {
                        id: icon,
                        ..Default::default()
                    },
                ));
            }
        });
        let label = (!sector.label.is_empty()).then(|| {
            let text = Text {
                spans: vec![TextSpan::lit(sector.label.clone())],
                wrap: WrapMode::None,
                ..Default::default()
            }
            .with_style(TextStyle {
                font_px: (colors.1 * 0.75).max(10.0),
                color: colors.0.text,
                ..Default::default()
            });
            let id = world.spawn((UiNode, text)).id();
            world.entity_mut(chip_id).add_child(id);
            id
        });
        world.entity_mut(chip_id).insert(RadialChip {
            menu: root,
            selection,
            label,
        });
        world.entity_mut(root).add_child(chip_id);
        chips.push(chip_id);
    }
    world.resource_mut::<LayoutDirty>().mark();
    chips
}

fn chip_style(pos: [f32; 2], size: f32) -> Style {
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(pos[0]),
            top: Dimension::Points(pos[1]),
            ..Edges::default()
        },
        size: [Dimension::Points(size), Dimension::Points(size)],
        direction: FlexDirection::Column,
        justify_content: JustifyContent::Center,
        align_items: Align::Center,
        padding: Edges::px(2.0),
        ..Default::default()
    }
}

fn update_visuals(
    world: &mut World,
    root: Entity,
    menu: &RadialMenu,
    colors: &(crate::theme::ThemeColors, f32),
) {
    let visible = menu.progress > 0.0;
    let display = if visible { Display::Flex } else { Display::None };
    let mut root_style = world
        .get::<crate::node::NodeStyle>(root)
        .map(|s| s.0.clone())
        .unwrap_or_default();
    if root_style.display != display {
        root_style.display = display;
        set_if_changed(world, root, crate::node::NodeStyle(root_style));
    }
    set_if_changed(world, root, Opacity(smooth(menu.progress)));
    let outer = menu.outer_radius;
    let backdrop = menu.style.backdrop.unwrap_or(colors.0.overlay);
    set_if_changed(
        world,
        root,
        Background {
            kind: BackgroundKind::Color(backdrop.with_alpha(backdrop.0[3] * 0.6)),
        },
    );
    set_if_changed(
        world,
        root,
        Border {
            color: colors.0.border,
            widths: [1.0; 4],
            radius: [outer; 4],
        },
    );
    if !visible {
        return;
    }
    for &chip in &menu.chips {
        let Some(info) = world.get::<RadialChip>(chip).copied() else {
            continue;
        };
        let highlighted = menu.selection == info.selection;
        let enabled = menu.entry(info.selection).is_none_or(|e| e.enabled);
        let (pos, size) = chip_geometry(menu, info.selection, menu.progress, highlighted);
        let fill = if !enabled {
            colors.0.surface
        } else if highlighted {
            menu.style.selected.unwrap_or(colors.0.primary)
        } else {
            menu.style.sector.unwrap_or(colors.0.surface_hi)
        };
        let text = if !enabled {
            colors.0.text_dim
        } else if highlighted {
            menu.style.selected_text.unwrap_or(colors.0.on_primary)
        } else {
            menu.style.text.unwrap_or(colors.0.text)
        };
        set_if_changed(world, chip, crate::node::NodeStyle(chip_style(pos, size)));
        set_if_changed(
            world,
            chip,
            Background {
                kind: BackgroundKind::Color(fill),
            },
        );
        set_if_changed(
            world,
            chip,
            Border {
                color: if highlighted {
                    colors.0.primary
                } else {
                    colors.0.border
                },
                widths: [1.0; 4],
                radius: [size * 0.3; 4],
            },
        );
        if let Some(label) = info.label
            && let Some(mut t) = world.get_mut::<Text>(label)
            && t.spans.first().is_some_and(|s| s.style.color != text)
        {
            for span in &mut t.spans {
                span.style.color = text;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(n: usize) -> RadialGeometry {
        RadialGeometry {
            sectors: n,
            ..RadialGeometry::default()
        }
    }

    fn at(angle: f32, mag: f32) -> [f32; 2] {
        [angle.sin() * mag, angle.cos() * mag]
    }

    #[test]
    fn centres_select_their_own_sector_for_every_count() {
        for n in MIN_SECTORS..=MAX_SECTORS {
            let g = geo(n);
            for i in 0..n {
                let v = at(g.centre_angle(i), 1.0);
                assert_eq!(g.select(None, v), Some(i), "n={n} i={i}");
                assert_eq!(g.sector_at(g.centre_angle(i)), i);
            }
        }
    }

    #[test]
    fn non_finite_input_selects_nothing() {
        let g = geo(8);
        assert_eq!(g.select(None, [f32::INFINITY, f32::INFINITY]), None);
        assert_eq!(g.select(None, [f32::NEG_INFINITY, 0.0]), None);
        let nan_dz = RadialGeometry {
            dead_zone: f32::NAN,
            ..geo(8)
        };
        assert_eq!(nan_dz.select(None, [0.01, 0.01]), None);
    }

    #[test]
    fn two_sectors_with_maximum_hysteresis_can_still_switch() {
        let g = RadialGeometry {
            sectors: 2,
            hysteresis: 0.5,
            ..RadialGeometry::default()
        };
        let other = at(g.centre_angle(1), 1.0);
        assert_eq!(g.select(Some(0), other), Some(1));
        assert_eq!(g.select(Some(0), at(g.centre_angle(0), 1.0)), Some(0));
    }

    #[test]
    fn stick_drift_below_the_dead_zone_leaves_the_cursor_in_charge() {
        let mut m = wheel(8);
        let computed = ComputedNode {
            rect: [0.0, 0.0, 200.0, 200.0],
            scale: 1.0,
            ..ComputedNode::default()
        };
        let mut pointer = UiPointer::default();
        pointer.cursor = Some([100.0, 100.0]);
        let drift = RadialSticks {
            right: [0.05, -0.04],
            ..RadialSticks::default()
        };
        let mut input = RadialInput::default();
        gather_pointing(&mut m, &mut input, &computed, &pointer, &drift);
        // The cursor moves: with drift on the stick it still points.
        pointer.cursor = Some([180.0, 100.0]);
        let mut input = RadialInput::default();
        gather_pointing(&mut m, &mut input, &computed, &pointer, &drift);
        let v = input.vector.expect("the cursor points");
        assert!(v[0] > 0.1, "{v:?}");
        // A real push takes the pointer.
        let push = RadialSticks {
            right: [0.0, 0.9],
            ..RadialSticks::default()
        };
        let mut input = RadialInput::default();
        gather_pointing(&mut m, &mut input, &computed, &pointer, &push);
        assert_eq!(input.vector, Some([0.0, 0.9]));
    }

    #[test]
    fn sector_zero_is_up_and_clockwise() {
        let g = geo(4);
        assert_eq!(g.select(None, [0.0, 1.0]), Some(0));
        assert_eq!(g.select(None, [1.0, 0.0]), Some(1));
        assert_eq!(g.select(None, [0.0, -1.0]), Some(2));
        assert_eq!(g.select(None, [-1.0, 0.0]), Some(3));
    }

    #[test]
    fn wraps_around_at_the_seam() {
        for n in MIN_SECTORS..=MAX_SECTORS {
            let g = geo(n);
            let eps = g.width() * 0.25;
            // Just clockwise and just counter-clockwise of up are both inside sector 0.
            assert_eq!(g.select(None, at(eps, 1.0)), Some(0), "n={n}");
            assert_eq!(g.select(None, at(TAU - eps, 1.0)), Some(0), "n={n}");
            // Just past the last sector's far boundary lands in the last sector.
            let last = g.centre_angle(n - 1);
            assert_eq!(g.select(None, at(last + eps, 1.0)), Some(n - 1), "n={n}");
        }
    }

    #[test]
    fn dead_zone_selects_nothing() {
        let g = geo(8);
        assert_eq!(g.select(None, [0.0, 0.0]), None);
        assert_eq!(g.select(None, at(1.0, 0.3)), None);
        assert_eq!(g.select(None, at(1.0, 0.36)), Some(g.sector_at(1.0)));
        assert_eq!(g.select(None, [f32::NAN, 0.5]), None);
    }

    #[test]
    fn dead_zone_exit_has_hysteresis() {
        let g = geo(8);
        let s = Some(2);
        let a = g.centre_angle(2);
        // 0.30 is inside the entry dead zone (0.35) but above the exit threshold (0.28).
        assert_eq!(g.select(None, at(a, 0.30)), None);
        assert_eq!(g.select(s, at(a, 0.30)), s);
        assert_eq!(g.select(s, at(a, 0.2)), None);
    }

    #[test]
    fn boundary_does_not_flicker() {
        for n in MIN_SECTORS..=MAX_SECTORS {
            let g = geo(n);
            let boundary = g.centre_angle(0) + g.width() * 0.5;
            let jitter = g.width() * 0.03; // well inside the 12 % band
            let mut cur = g.select(None, at(boundary - jitter, 1.0));
            assert_eq!(cur, Some(0));
            let mut changes = 0;
            for k in 0..200 {
                let j = if k % 2 == 0 { jitter } else { -jitter };
                let next = g.select(cur, at(boundary + j, 1.0));
                changes += (next != cur) as usize;
                cur = next;
            }
            assert_eq!(changes, 0, "n={n} flickered");
            assert_eq!(cur, Some(0));
        }
    }

    #[test]
    fn crossing_the_band_switches_once_and_stays() {
        for n in 3..=MAX_SECTORS {
            let g = geo(n);
            let boundary = g.centre_angle(0) + g.width() * 0.5;
            let cur = g.select(None, at(boundary - 0.01, 1.0));
            assert_eq!(cur, Some(0));
            let past = boundary + g.width() * (g.hysteresis + 0.02);
            let next = g.select(cur, at(past, 1.0));
            assert_eq!(next, Some(1), "n={n}");
            // Coming back to just below the boundary does not flip back.
            assert_eq!(g.select(next, at(boundary - 0.01, 1.0)), Some(1), "n={n}");
        }
    }

    #[test]
    fn start_angle_rotates_the_wheel() {
        let g = RadialGeometry {
            sectors: 4,
            start: PI / 4.0,
            ..RadialGeometry::default()
        };
        // Sector 0 is centred up-right; straight up is on its counter-clockwise boundary.
        assert_eq!(g.select(None, at(PI / 4.0, 1.0)), Some(0));
        assert_eq!(g.select(None, at(PI / 4.0 + PI / 2.0, 1.0)), Some(1));
    }

    #[test]
    fn counts_clamp() {
        assert_eq!(geo(0).count(), 2);
        assert_eq!(geo(1).count(), 2);
        assert_eq!(geo(40).count(), 12);
    }

    #[test]
    fn angle_helpers() {
        assert!((angle_from_up(0.0, 1.0)).abs() < 1e-6);
        assert!((angle_from_up(1.0, 0.0) - PI / 2.0).abs() < 1e-6);
        assert!((angle_from_up(-1.0, 0.0) - 3.0 * PI / 2.0).abs() < 1e-6);
        assert!((angle_delta(0.1, TAU - 0.1) - 0.2).abs() < 1e-5);
        assert!((angle_delta(TAU - 0.1, 0.1) + 0.2).abs() < 1e-5);
    }

    fn wheel(n: usize) -> RadialMenu {
        let sectors = (0..n)
            .map(|i| RadialSector::new(format!("s{i}"), format!("S{i}")))
            .collect();
        RadialMenu {
            animation_secs: 0.0,
            ..RadialMenu::new(sectors)
        }
    }

    fn run(menu: &mut RadialMenu, inputs: &[RadialInput]) -> Vec<RadialEventKind> {
        let mut out = Vec::new();
        for i in inputs {
            menu.step(i, 0.016, &mut out);
        }
        out
    }

    fn point(angle: f32) -> RadialInput {
        RadialInput {
            held: true,
            vector: Some(at(angle, 1.0)),
            dead_zone: 0.35,
            ..Default::default()
        }
    }

    #[test]
    fn hold_release_selects_the_highlight() {
        let mut m = wheel(8);
        let g = m.geometry(0.35);
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                point(g.centre_angle(2)),
                point(g.centre_angle(2)),
                RadialInput::default(),
            ],
        );
        assert_eq!(
            out,
            vec![
                RadialEventKind::Opened,
                RadialEventKind::Hover(RadialSelection::Sector(2)),
                RadialEventKind::Select {
                    selection: RadialSelection::Sector(2),
                    id: "s2".into()
                },
                RadialEventKind::Closed,
            ]
        );
        assert!(!m.open);
    }

    #[test]
    fn hold_release_with_nothing_cancels() {
        let mut m = wheel(6);
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                RadialInput::default(),
            ],
        );
        assert_eq!(
            out,
            vec![
                RadialEventKind::Opened,
                RadialEventKind::Cancel,
                RadialEventKind::Closed
            ]
        );
    }

    #[test]
    fn hold_press_mode_cancels_on_release_and_confirms_on_contact() {
        let mut m = wheel(4).with_confirm(RadialConfirm::Press);
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                point(PI / 2.0),
                RadialInput {
                    held: true,
                    contact_down: true,
                    ..Default::default()
                },
            ],
        );
        assert!(out.contains(&RadialEventKind::Select {
            selection: RadialSelection::Sector(1),
            id: "s1".into()
        }));
        let mut m = wheel(4).with_confirm(RadialConfirm::Press);
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                point(PI / 2.0),
                RadialInput::default(),
            ],
        );
        assert_eq!(out.last(), Some(&RadialEventKind::Closed));
        assert!(out.contains(&RadialEventKind::Cancel));
        assert!(!out.iter().any(|e| matches!(e, RadialEventKind::Select { .. })));
    }

    #[test]
    fn toggle_opens_confirms_on_release_and_toggle_again_cancels() {
        let mut m = wheel(4).with_open(RadialOpen::Toggle);
        let out = run(
            &mut m,
            &[
                RadialInput {
                    toggle: true,
                    ..Default::default()
                },
                RadialInput {
                    vector: Some([0.0, -1.0]),
                    dead_zone: 0.35,
                    contact_down: true,
                    ..Default::default()
                },
                RadialInput {
                    contact_up: true,
                    ..Default::default()
                },
            ],
        );
        assert!(out.contains(&RadialEventKind::Select {
            selection: RadialSelection::Sector(2),
            id: "s2".into()
        }));
        let out = run(
            &mut m,
            &[
                RadialInput {
                    toggle: true,
                    ..Default::default()
                },
                RadialInput {
                    toggle: true,
                    ..Default::default()
                },
            ],
        );
        assert_eq!(
            out,
            vec![
                RadialEventKind::Opened,
                RadialEventKind::Cancel,
                RadialEventKind::Closed
            ]
        );
    }

    #[test]
    fn centre_action_selected_in_the_dead_zone() {
        let mut m = wheel(4).with_center(RadialSector::new("holster", "Holster"));
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                RadialInput {
                    held: true,
                    vector: Some([0.0, 0.0]),
                    dead_zone: 0.35,
                    ..Default::default()
                },
                RadialInput::default(),
            ],
        );
        assert!(out.contains(&RadialEventKind::Hover(RadialSelection::Center)));
        assert!(out.contains(&RadialEventKind::Select {
            selection: RadialSelection::Center,
            id: "holster".into()
        }));
    }

    #[test]
    fn disabled_sector_cannot_be_selected() {
        let mut m = wheel(4);
        m.sectors[1].enabled = false;
        let out = run(
            &mut m,
            &[
                RadialInput {
                    held: true,
                    ..Default::default()
                },
                point(PI / 2.0),
                RadialInput::default(),
            ],
        );
        assert!(!out.iter().any(|e| matches!(e, RadialEventKind::Select { .. })));
        assert!(out.contains(&RadialEventKind::Cancel));
    }

    #[test]
    fn stepping_wraps_and_skips_disabled() {
        let mut m = wheel(4);
        m.sectors[1].enabled = false;
        m.open();
        let mut out = Vec::new();
        m.step(&RadialInput::default(), 0.0, &mut out);
        m.step(
            &RadialInput {
                step: 1,
                ..Default::default()
            },
            0.0,
            &mut out,
        );
        assert_eq!(m.selection, RadialSelection::Sector(0));
        m.step(
            &RadialInput {
                step: 1,
                ..Default::default()
            },
            0.0,
            &mut out,
        );
        assert_eq!(m.selection, RadialSelection::Sector(2));
        m.step(
            &RadialInput {
                step: -2,
                ..Default::default()
            },
            0.0,
            &mut out,
        );
        assert_eq!(m.selection, RadialSelection::Sector(3));
    }

    #[test]
    fn animation_follows_dt() {
        let mut m = wheel(4);
        m.animation_secs = 0.2;
        m.open();
        let mut out = Vec::new();
        m.step(&RadialInput::default(), 0.1, &mut out);
        assert_eq!(m.phase(), RadialPhase::Opening);
        assert!((m.progress - 0.5).abs() < 1e-5);
        m.step(&RadialInput::default(), 0.2, &mut out);
        assert_eq!(m.phase(), RadialPhase::Open);
        m.close();
        m.step(&RadialInput::default(), 0.1, &mut out);
        assert_eq!(m.phase(), RadialPhase::Closing);
        m.step(&RadialInput::default(), 0.2, &mut out);
        assert_eq!(m.phase(), RadialPhase::Closed);
    }

    #[test]
    fn explicit_cancel_and_confirm() {
        let mut m = wheel(4);
        m.open();
        let g = m.geometry(0.35);
        let out = run(
            &mut m,
            &[
                RadialInput::default(),
                point(g.centre_angle(3)),
                RadialInput {
                    confirm: true,
                    ..Default::default()
                },
            ],
        );
        assert!(out.contains(&RadialEventKind::Select {
            selection: RadialSelection::Sector(3),
            id: "s3".into()
        }));
        m.open();
        let out = run(
            &mut m,
            &[
                RadialInput::default(),
                RadialInput {
                    cancel: true,
                    ..Default::default()
                },
            ],
        );
        assert!(out.contains(&RadialEventKind::Cancel));
    }
}
