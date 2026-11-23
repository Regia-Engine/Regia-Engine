// SPDX-License-Identifier: MPL-2.0
//! Screen/page stack (gap W18, ADR-273): history of full-screen UI pages with transitions.
//!
//! A screen is any node carrying [`UiScreen`] (usually a full-window child of a
//! [`crate::node::UiRoot`]). The [`ScreenStack`] resource is the history, bottom first;
//! only the top screen is visible and receives input. Pushing hides the previous top
//! ([`Display::None`]) and gives the new top an [`InputLayer`];
//! popping restores the previous screen's visibility and input mode through
//! [`sync_layers`], then the control that had focus on that screen when it was covered,
//! if the control is still focusable, not disabled and still under the screen.
//!
//! Transitions are metadata ([`ScreenTransition`]): the switch itself is instant so layout
//! stays untouched, and games animate it with their own [`UiTween`](crate::tween::UiTween)
//! (for example a fade on [`Opacity`](crate::node::Opacity)). [`ScreenChanged`] tells
//! transition drivers what happened. Do not put [`Modal`](crate::widgets::Modal) on a
//! [`UiScreen`] node: dialogs belong inside screens.
//!
//! Agents drive it with `ui.screens` (`list`, `push`, `pop`, `show`; ADR-1563 l,
//! `book/agents/ui-screens.md`): [`screen_list`] is the read side, [`push_screen`] /
//! [`pop_screen`] / [`show_screen`] are the writes.

use regia_app::prelude::*;

use crate::focus::{FocusState, Focusable};
use crate::layers::{InputLayer, InputLayers, LayerInputMode, set_focus_world, sync_layers, within};
use crate::node::NodeStyle;
use crate::style::{Display, Style};

/// Default fade length for [`ScreenTransition::parse`] and docs examples, seconds.
pub const SCREEN_FADE_SECS: f32 = 0.2;

/// How a screen prefers to animate in and out. The stack applies switches instantly;
/// games read this to drive their own tween (reduced motion jumps to the end value).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ScreenTransition {
    /// Appear at once (the default: instant switches unless the game asks for more).
    #[default]
    Instant,
    /// Fade the incoming screen over `duration_secs`.
    Fade {
        /// Fade length in seconds.
        duration_secs: f32,
    },
    /// Slide the incoming screen in from `offset` (UI px) over `duration_secs`.
    Slide {
        /// Slide offset in UI px.
        offset: [f32; 2],
        /// Slide length in seconds.
        duration_secs: f32,
    },
}

impl ScreenTransition {
    /// The protocol name (`instant`, `fade`, `slide`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Instant => "instant",
            Self::Fade { .. } => "fade",
            Self::Slide { .. } => "slide",
        }
    }

    /// Parses a protocol name with default timings ([`SCREEN_FADE_SECS`], 30 px slide).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "instant" => Some(Self::Instant),
            "fade" => Some(Self::Fade {
                duration_secs: SCREEN_FADE_SECS,
            }),
            "slide" => Some(Self::Slide {
                offset: [0.0, 30.0],
                duration_secs: SCREEN_FADE_SECS,
            }),
            _ => None,
        }
    }
}

/// Marks a node as a full-screen page. Only the top of the [`ScreenStack`] is shown.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct UiScreen {
    /// Stable id agents and [`ScreenChanged`] readers use. Empty uses the entity bits.
    pub id: String,
    /// Input mode while this screen is on top.
    pub mode: LayerInputMode,
    /// Preferred enter/exit animation (metadata; the stack switches instantly).
    pub transition: ScreenTransition,
}

impl UiScreen {
    /// A screen with [`LayerInputMode::UiOnly`] and an instant transition.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            mode: LayerInputMode::UiOnly,
            transition: ScreenTransition::Instant,
        }
    }

    /// Sets the input mode while this screen is on top.
    #[must_use]
    pub fn with_mode(mut self, mode: LayerInputMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the preferred transition.
    #[must_use]
    pub fn with_transition(mut self, transition: ScreenTransition) -> Self {
        self.transition = transition;
        self
    }
}

/// One screen on the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenEntry {
    /// The screen's root node.
    pub root: Entity,
    /// The [`UiScreen::id`] at push time.
    pub id: String,
    /// Display to restore when this screen is on top (captured at push, updated while shown).
    pub shown: Display,
}

/// History of screens, bottom first. Only the last entry is visible.
#[derive(Resource, Debug, Default)]
pub struct ScreenStack {
    entries: Vec<ScreenEntry>,
    back_pop: bool,
}

impl ScreenStack {
    /// Enables back-to-pop by default for stacked screens. A [`ScreenBack`] component overrides it.
    pub fn set_back_pop(&mut self, enabled: bool) {
        self.back_pop = enabled;
    }

    /// Whether the stack's default back policy pops. Off until explicitly enabled.
    pub fn back_pop(&self) -> bool {
        self.back_pop
    }

    /// Entries, bottom first.
    pub fn entries(&self) -> &[ScreenEntry] {
        &self.entries
    }

    /// Number of screens on the stack.
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    /// The top screen, if any.
    pub fn top(&self) -> Option<Entity> {
        self.entries.last().map(|e| e.root)
    }

    /// The top screen's id, if any.
    pub fn top_id(&self) -> Option<&str> {
        self.entries.last().map(|e| e.id.as_str())
    }

    /// True when `root` is anywhere on the stack.
    pub fn contains(&self, root: Entity) -> bool {
        self.entries.iter().any(|e| e.root == root)
    }

    /// Ids, bottom first.
    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.id.clone()).collect()
    }
}

/// Why a screen operation failed. The `code` is the agent protocol's lowercase error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenError {
    /// The entity does not exist.
    UnknownEntity,
    /// The entity does not carry [`UiScreen`].
    NotAScreen,
    /// The screen is already on the stack.
    AlreadyOnStack,
    /// There is no screen to pop.
    EmptyStack,
    /// `pop` named a screen that is on the stack but not on top (use [`show_screen`]).
    NotOnTop,
}

impl ScreenError {
    /// The lowercase protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownEntity => "unknown_entity",
            Self::NotAScreen => "not_a_screen",
            Self::AlreadyOnStack => "already_on_stack",
            Self::EmptyStack => "empty_stack",
            Self::NotOnTop => "not_on_top",
        }
    }
}

/// A push, pop or prune changed the visible screen.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ScreenChanged {
    /// The screen just pushed or revealed, if any.
    pub pushed: Option<Entity>,
    /// The screen just hidden or removed, if any.
    pub popped: Option<Entity>,
    /// The visible screen now, if any.
    pub top: Option<Entity>,
}

/// One row of [`screen_list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenInfo {
    /// The screen's root node.
    pub root: Entity,
    /// The [`UiScreen::id`] (recorded at push).
    pub id: String,
    /// True for the top (visible) screen only.
    pub visible: bool,
}

/// Read side for agents and transition drivers: the stack, bottom first.
pub fn screen_list(world: &World) -> Vec<ScreenInfo> {
    let Some(stack) = world.get_resource::<ScreenStack>() else {
        return Vec::new();
    };
    let top = stack.top();
    stack
        .entries()
        .iter()
        .map(|e| ScreenInfo {
            root: e.root,
            id: e.id.clone(),
            visible: Some(e.root) == top,
        })
        .collect()
}

/// Pushes `root` onto the stack: hides the previous top, shows `root` and focuses its
/// default node. Fails when `root` is unknown, is not a [`UiScreen`] or is already stacked.
pub fn push_screen(world: &mut World, root: Entity) -> Result<(), ScreenError> {
    world.init_resource::<ScreenStack>();
    if world.get_entity(root).is_err() {
        return Err(ScreenError::UnknownEntity);
    }
    let Some(screen) = world.get::<UiScreen>(root).cloned() else {
        return Err(ScreenError::NotAScreen);
    };
    if world
        .get_resource::<ScreenStack>()
        .is_some_and(|s| s.contains(root))
    {
        return Err(ScreenError::AlreadyOnStack);
    }
    let shown = shown_display(world, root);
    let previous = world.get_resource::<ScreenStack>().and_then(|s| s.top());
    world.resource_mut::<ScreenStack>().entries.push(ScreenEntry {
        root,
        id: screen.id.clone(),
        shown,
    });
    if let Some(prev) = previous {
        remember_covered_focus(world, prev);
        hide_node(world, prev);
        set_screen_layer(world, prev, None);
    }
    show_node(world, root, shown);
    set_screen_layer(world, root, Some(screen.mode));
    world.write_message(ScreenChanged {
        pushed: Some(root),
        popped: None,
        top: Some(root),
    });
    sync_layers(world);
    Ok(())
}

/// Pops `root` (the top screen when `None`): hides it and shows the screen below, restoring
/// that screen's visibility, input mode and focus. Fails on an empty stack (a `None` pop),
/// an unknown entity, or a screen that is not on top (use [`show_screen`] to jump back).
pub fn pop_screen(world: &mut World, root: Option<Entity>) -> Result<Entity, ScreenError> {
    world.init_resource::<ScreenStack>();
    let target = match root {
        Some(root) => {
            if world.get_entity(root).is_err() {
                // The named screen is gone: prune it (and any other dead entries), then
                // reveal the new top like a successful pop — shown, layered and announced.
                // Returning before the reveal would leave the revealed screen hidden until
                // a later sync_screens, which shows it a frame late and emits no
                // ScreenChanged.
                let previous = world.get_resource::<ScreenStack>().and_then(|s| s.top());
                if prune_dead(world) {
                    reveal_top(world, previous);
                }
                return Err(ScreenError::UnknownEntity);
            }
            let top = world.get_resource::<ScreenStack>().and_then(|s| s.top());
            if Some(root) != top {
                return Err(ScreenError::NotOnTop);
            }
            root
        }
        None => world
            .get_resource::<ScreenStack>()
            .and_then(|s| s.top())
            .ok_or(ScreenError::EmptyStack)?,
    };
    remove_top(world, target);
    reveal_top(world, Some(target));
    Ok(target)
}

/// Shows the current top after a pop or prune and announces the change: restores the top
/// screen's visibility and input mode, sends [`ScreenChanged`] (with `previous_top` as the
/// popped screen) and re-syncs the input layers. A no-op reveal (the top did not change,
/// e.g. only a buried dead entry was pruned) sends nothing.
fn reveal_top(world: &mut World, previous_top: Option<Entity>) {
    let top = world.get_resource::<ScreenStack>().and_then(|s| s.top());
    if top != previous_top
        && let Some(next) = top
    {
        let (shown, mode) = restore_for(world, next);
        show_node(world, next, shown);
        set_screen_layer(world, next, Some(mode));
        remember_shown(world, next, shown);
    }
    if top != previous_top {
        world.write_message(ScreenChanged {
            pushed: None,
            popped: previous_top,
            top,
        });
    }
    sync_layers(world);
    if top != previous_top
        && let Some(next) = top
    {
        restore_covered_focus(world, next);
    }
}

/// Shows `root`: pushes it when absent, pops everything above it when present, and does
/// nothing when it is already on top. Returns true when the visible screen changed.
pub fn show_screen(world: &mut World, root: Entity) -> Result<bool, ScreenError> {
    world.init_resource::<ScreenStack>();
    if world.get_entity(root).is_err() {
        return Err(ScreenError::UnknownEntity);
    }
    if world.get::<UiScreen>(root).is_none() {
        return Err(ScreenError::NotAScreen);
    }
    let top = world.get_resource::<ScreenStack>().and_then(|s| s.top());
    if Some(root) == top {
        return Ok(false);
    }
    let stacked = world
        .get_resource::<ScreenStack>()
        .is_some_and(|s| s.contains(root));
    if !stacked {
        push_screen(world, root)?;
        return Ok(true);
    }
    let previous = top;
    while world
        .get_resource::<ScreenStack>()
        .and_then(|s| s.top())
        .is_some_and(|t| t != root)
    {
        let Some(top) = world.get_resource::<ScreenStack>().and_then(|s| s.top()) else {
            break;
        };
        remove_top(world, top);
    }
    let (shown, mode) = restore_for(world, root);
    show_node(world, root, shown);
    set_screen_layer(world, root, Some(mode));
    remember_shown(world, root, shown);
    world.write_message(ScreenChanged {
        pushed: None,
        popped: previous,
        top: Some(root),
    });
    sync_layers(world);
    restore_covered_focus(world, root);
    Ok(true)
}

/// Brings [`ScreenStack`] in step with the world: drops entries whose node despawned or
/// lost [`UiScreen`], then enforces that only the top is shown and layered. Runs before
/// [`sync_layers`] in the `Update` UI chain.
pub fn sync_screens(world: &mut World) {
    let _span = tracing::debug_span!("sync_screens").entered();
    world.init_resource::<ScreenStack>();
    let previous_top = world.get_resource::<ScreenStack>().and_then(|s| s.top());
    let pruned = prune_dead(world);
    let mut changed = pruned;
    let depth = world.get_resource::<ScreenStack>().map_or(0, |s| s.depth());
    for index in 0..depth {
        let Some(entry) = world
            .get_resource::<ScreenStack>()
            .and_then(|s| s.entries.get(index).cloned())
        else {
            continue;
        };
        let is_top = index + 1 == depth;
        if is_top {
            adopt_shown(world, entry.root);
            let fresh = world
                .get_resource::<ScreenStack>()
                .and_then(|s| s.entries.get(index).cloned());
            let Some(fresh) = fresh else { continue };
            if ensure_shown(world, fresh.root, fresh.shown) {
                changed = true;
            }
            let mode = world
                .get::<UiScreen>(fresh.root)
                .map(|s| s.mode)
                .unwrap_or(LayerInputMode::UiOnly);
            if ensure_layer(world, fresh.root, Some(mode)) {
                changed = true;
            }
        } else {
            if ensure_hidden(world, entry.root) {
                changed = true;
            }
            if ensure_layer(world, entry.root, None) {
                changed = true;
            }
        }
    }
    let top = world.get_resource::<ScreenStack>().and_then(|s| s.top());
    if pruned && top != previous_top {
        world.write_message(ScreenChanged {
            pushed: None,
            popped: previous_top,
            top,
        });
    }
    if changed {
        sync_layers(world);
    }
    if pruned
        && top != previous_top
        && let Some(next) = top
    {
        restore_covered_focus(world, next);
    }
}

/// Focus that was on a screen when a later screen covered it. Private: [`ScreenEntry`] stays
/// the three public fields, and this is cleared once the screen is shown again.
#[derive(Resource, Default)]
struct CoveredScreenFocus {
    saved: Vec<(Entity, Entity)>,
}

fn remember_covered_focus(world: &mut World, screen: Entity) {
    world.init_resource::<CoveredScreenFocus>();
    let Some(focused) = world.get_resource::<FocusState>().and_then(|f| f.focused) else {
        return;
    };
    if !within(world, screen, focused) {
        return;
    }
    let saved = &mut world.resource_mut::<CoveredScreenFocus>().saved;
    if let Some(slot) = saved.iter_mut().find(|(root, _)| *root == screen) {
        slot.1 = focused;
    } else {
        saved.push((screen, focused));
    }
}

fn restore_covered_focus(world: &mut World, screen: Entity) {
    let focused = {
        let Some(mut saved) = world.get_resource_mut::<CoveredScreenFocus>() else {
            return;
        };
        let Some(index) = saved.saved.iter().position(|(root, _)| *root == screen) else {
            return;
        };
        saved.saved.swap_remove(index).1
    };
    let usable = world.get::<Focusable>(focused).is_some()
        && !crate::node::is_disabled(world, focused)
        && within(world, screen, focused);
    if usable {
        set_focus_world(world, Some(focused));
    }
}

/// Display `root` should restore when shown: its current one, or flex when hidden/missing.
fn shown_display(world: &World, root: Entity) -> Display {
    world
        .get::<NodeStyle>(root)
        .map_or(Display::Flex, |n| match n.0.display {
            Display::None => Display::Flex,
            d => d,
        })
}

/// Display and mode to restore for a screen coming back on top.
fn restore_for(world: &World, root: Entity) -> (Display, LayerInputMode) {
    let shown = world
        .get_resource::<ScreenStack>()
        .and_then(|s| s.entries.iter().find(|e| e.root == root).map(|e| e.shown))
        .unwrap_or_else(|| shown_display(world, root));
    let mode = world
        .get::<UiScreen>(root)
        .map_or(LayerInputMode::UiOnly, |s| s.mode);
    (shown, mode)
}

/// Records the display a visible screen settled on, so intentional restyles survive.
fn remember_shown(world: &mut World, root: Entity, shown: Display) {
    if let Some(mut stack) = world.get_resource_mut::<ScreenStack>()
        && let Some(entry) = stack.entries.iter_mut().find(|e| e.root == root)
    {
        entry.shown = shown;
    }
}

/// Adopts a visible top screen's intentional restyle as its recorded display.
fn adopt_shown(world: &mut World, root: Entity) {
    let current = world.get::<NodeStyle>(root).map(|n| n.0.display);
    if let Some(current) = current
        && current != Display::None
        && let Some(mut stack) = world.get_resource_mut::<ScreenStack>()
        && let Some(entry) = stack.entries.iter_mut().find(|e| e.root == root)
        && entry.shown != current
    {
        entry.shown = current;
    }
}

/// Hides `root` when alive.
fn hide_node(world: &mut World, root: Entity) {
    if world.get_entity(root).is_err() {
        return;
    }
    if let Some(mut style) = world.get_mut::<NodeStyle>(root) {
        style.0.display = Display::None;
    } else {
        world.entity_mut(root).insert(NodeStyle::new(Style {
            display: Display::None,
            ..Default::default()
        }));
    }
}

/// Shows `root` with `display` when alive.
fn show_node(world: &mut World, root: Entity, display: Display) {
    if world.get_entity(root).is_err() {
        return;
    }
    if let Some(mut style) = world.get_mut::<NodeStyle>(root) {
        style.0.display = display;
    } else {
        world.entity_mut(root).insert(NodeStyle::new(Style {
            display,
            ..Default::default()
        }));
    }
}

/// Sets or clears the screen's input layer when alive.
fn set_screen_layer(world: &mut World, root: Entity, mode: Option<LayerInputMode>) {
    if world.get_entity(root).is_err() {
        return;
    }
    match mode {
        Some(mode) => {
            world.entity_mut(root).insert(InputLayer { mode });
        }
        None => {
            world.entity_mut(root).remove::<InputLayer>();
        }
    }
}

/// Removes the top entry and hides it. Caller shows the next screen.
fn remove_top(world: &mut World, top: Entity) {
    if let Some(mut stack) = world.get_resource_mut::<ScreenStack>()
        && let Some(pos) = stack.entries.iter().position(|e| e.root == top)
    {
        stack.entries.remove(pos);
    }
    hide_node(world, top);
    set_screen_layer(world, top, None);
}

/// Drops entries whose node despawned or lost [`UiScreen`]. Returns true when pruned.
fn prune_dead(world: &mut World) -> bool {
    let dead: Vec<Entity> = world.get_resource::<ScreenStack>().map_or(Vec::new(), |s| {
        s.entries
            .iter()
            .filter(|e| world.get_entity(e.root).is_err() || world.get::<UiScreen>(e.root).is_none())
            .map(|e| e.root)
            .collect()
    });
    if dead.is_empty() {
        return false;
    }
    if let Some(mut stack) = world.get_resource_mut::<ScreenStack>() {
        stack.entries.retain(|e| !dead.contains(&e.root));
    }
    for root in dead {
        set_screen_layer(world, root, None);
    }
    true
}

/// Ensures the top screen shows `shown`. Returns true when edited.
fn ensure_shown(world: &mut World, root: Entity, shown: Display) -> bool {
    if world.get_entity(root).is_err() {
        return false;
    }
    if world
        .get::<NodeStyle>(root)
        .is_some_and(|style| style.0.display == shown)
    {
        return false;
    }
    show_node(world, root, shown);
    true
}

/// Ensures a buried screen is hidden. Returns true when edited.
fn ensure_hidden(world: &mut World, root: Entity) -> bool {
    if world.get_entity(root).is_err() {
        return false;
    }
    if world
        .get::<NodeStyle>(root)
        .is_some_and(|style| style.0.display == Display::None)
    {
        return false;
    }
    hide_node(world, root);
    true
}

/// Ensures the screen's layer matches `mode`. Returns true when edited.
fn ensure_layer(world: &mut World, root: Entity, mode: Option<LayerInputMode>) -> bool {
    if world.get_entity(root).is_err() {
        return false;
    }
    match mode {
        Some(mode) => {
            if world.get::<InputLayer>(root).is_some_and(|l| l.mode == mode) {
                return false;
            }
            set_screen_layer(world, root, Some(mode));
            true
        }
        None => {
            if world.get::<InputLayer>(root).is_none() {
                return false;
            }
            set_screen_layer(world, root, None);
            true
        }
    }
}

/// Per-screen back policy; overrides the stack's default without changing the screen definition.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenBack {
    /// Leave back handling to game code.
    #[default]
    Ignore,
    /// Pop this screen on Escape or gamepad B while it owns the active input layer.
    Pop,
}

/// Handles one back action for the active scope. A modal or other layer above the screen
/// keeps the action; only an opted-in top screen can pop. Focus restore is the ordinary pop path.
/// Returns whether the action popped a screen.
pub fn handle_screen_back(world: &mut World, scope: Entity) -> bool {
    if world
        .get_resource::<regia_input::InputRebinder>()
        .is_some_and(|capture| capture.pending().is_some() || capture.click_guard())
    {
        return false;
    }
    let Some(stack) = world.get_resource::<ScreenStack>() else {
        return false;
    };
    if world
        .get_resource::<InputLayers>()
        .is_none_or(|layers| layers.top_root() != Some(scope) || layers.mode() == LayerInputMode::GameOnly)
    {
        return false;
    }
    if stack.top() != Some(scope) || crate::node::is_disabled(world, scope) {
        return false;
    }
    let pop = world
        .get::<ScreenBack>(scope)
        .map_or(stack.back_pop, |back| *back == ScreenBack::Pop);
    pop && pop_screen(world, Some(scope)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_names_round_trip() {
        for name in ["instant", "fade", "slide"] {
            let parsed = ScreenTransition::parse(name);
            assert!(parsed.is_some());
            assert_eq!(parsed.map(|t| t.name()), Some(name));
        }
        assert_eq!(ScreenTransition::parse("nope"), None);
        assert_eq!(ScreenTransition::default(), ScreenTransition::Instant);
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        for e in [
            ScreenError::UnknownEntity,
            ScreenError::NotAScreen,
            ScreenError::AlreadyOnStack,
            ScreenError::EmptyStack,
            ScreenError::NotOnTop,
        ] {
            assert!(e.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    #[test]
    fn screen_builders_keep_id_and_mode() {
        let screen = UiScreen::new("title")
            .with_mode(LayerInputMode::GameOnly)
            .with_transition(ScreenTransition::Fade { duration_secs: 0.5 });
        assert_eq!(screen.id, "title");
        assert_eq!(screen.mode, LayerInputMode::GameOnly);
        assert_eq!(screen.transition.name(), "fade");
        let plain = UiScreen::new("hud");
        assert_eq!(plain.mode, LayerInputMode::UiOnly);
    }

    #[test]
    fn pop_of_a_despawned_screen_reveals_the_next_with_screen_changed() {
        let mut world = World::new();
        world.init_resource::<Messages<ScreenChanged>>();
        let a = world.spawn(UiScreen::new("a")).id();
        let b = world.spawn(UiScreen::new("b")).id();
        push_screen(&mut world, a).unwrap();
        push_screen(&mut world, b).unwrap();
        assert!(
            world
                .get::<NodeStyle>(a)
                .is_some_and(|n| n.0.display == Display::None),
            "pushing b hides a"
        );
        world.despawn(b);
        let err = pop_screen(&mut world, Some(b)).unwrap_err();
        assert_eq!(err, ScreenError::UnknownEntity);
        // The prune must reveal the new top like a successful pop: shown, layered and
        // announced (sync_screens would only show it a frame late, without an event).
        assert!(
            world
                .get::<NodeStyle>(a)
                .is_some_and(|n| n.0.display != Display::None),
            "a is shown again"
        );
        assert!(world.get::<InputLayer>(a).is_some(), "a has its input layer back");
        assert_eq!(world.resource::<ScreenStack>().top(), Some(a));
        let changed: Vec<ScreenChanged> = world.resource_mut::<Messages<ScreenChanged>>().drain().collect();
        assert_eq!(
            changed.last(),
            Some(&ScreenChanged {
                pushed: None,
                popped: Some(b),
                top: Some(a),
            })
        );
    }

    #[test]
    fn empty_stack_reports_no_top() {
        let stack = ScreenStack::default();
        assert_eq!(stack.depth(), 0);
        assert!(stack.top().is_none());
        assert!(stack.top_id().is_none());
        assert!(stack.ids().is_empty());
    }
}
