// SPDX-License-Identifier: MPL-2.0
//! Input layers (ADR-273 lane 4, ADR-293): a stack of layer roots where only the top layer
//! receives pointer and navigation input.
//!
//! A layer is any node carrying [`InputLayer`]; an open [`crate::widgets::Modal`] gets one
//! automatically. The stack lives in [`InputLayers`] and is kept in step with the world by
//! [`sync_layers`] once per frame, before the pointer and navigation systems run. Each layer
//! declares a [`LayerInputMode`]; the effective mode is the top layer's, so popping a layer
//! restores the mode of the one below. Pushing focuses the layer's default node, popping puts
//! focus back on the element that had it before the push.

use regia_app::prelude::*;

use crate::focus::{DefaultFocus, FocusState, Focusable};
use crate::interaction::{UiEvent, UiEventKind, UiInputState};
use crate::node::{ComputedNode, Disabled, Focused};
use crate::widgets::Modal;

/// Who gets input while a layer is the top layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerInputMode {
    /// Only the UI: gameplay should not act on input (a pause menu).
    UiOnly,
    /// The layer's UI and the game both receive input (a modal dialog over live gameplay, and
    /// the mode when no layer is active).
    #[default]
    UiAndGame,
    /// Only the game: the UI ignores the pointer and navigation (a HUD-only state).
    GameOnly,
}

impl LayerInputMode {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::UiOnly => "ui_only",
            Self::UiAndGame => "ui_and_game",
            Self::GameOnly => "game_only",
        }
    }

    /// Parses a protocol name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "ui_only" => Some(Self::UiOnly),
            "ui_and_game" => Some(Self::UiAndGame),
            "game_only" => Some(Self::GameOnly),
            _ => None,
        }
    }
}

/// Marks a node as the root of an input layer. Only the top layer's subtree receives pointer
/// and navigation input. Add it to push, remove it (or despawn the node) to pop.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputLayer {
    /// The input mode while this layer is on top.
    pub mode: LayerInputMode,
}

/// Marks an [`InputLayer`] that [`sync_layers`] added for an open modal (removed again when the
/// modal closes; a layer added by hand is never removed by the modal logic).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModalLayer;

/// One layer on the stack.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerEntry {
    /// The layer's root node.
    pub root: Entity,
    /// The mode while this layer is on top.
    pub mode: LayerInputMode,
    /// The effective mode when this layer was pushed (what returns when it is popped).
    pub previous_mode: LayerInputMode,
    /// The focused node when this layer was pushed (focus returns there on pop).
    pub restore_focus: Option<Entity>,
    pending_default: bool,
}

/// The stack of input layers, bottom first.
#[derive(Resource, Debug, Default)]
pub struct InputLayers {
    entries: Vec<LayerEntry>,
}

impl InputLayers {
    /// The layers, bottom first.
    pub fn entries(&self) -> &[LayerEntry] {
        &self.entries
    }

    /// Number of layers.
    pub fn depth(&self) -> usize {
        self.entries.len()
    }

    /// The top layer, if any.
    pub fn top(&self) -> Option<&LayerEntry> {
        self.entries.last()
    }

    /// The top layer's root, if any.
    pub fn top_root(&self) -> Option<Entity> {
        self.entries.last().map(|e| e.root)
    }

    /// The effective input mode: the top layer's, [`LayerInputMode::UiAndGame`] with no layer.
    pub fn mode(&self) -> LayerInputMode {
        self.entries.last().map_or(LayerInputMode::UiAndGame, |e| e.mode)
    }
}

/// Why a layer operation failed. The `code` is the agent protocol's lowercase error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerError {
    /// The entity does not exist.
    UnknownEntity,
    /// The entity is already a layer root.
    AlreadyLayer,
    /// The entity is not a layer root.
    NotALayer,
    /// There is no layer to pop.
    NoLayers,
}

impl LayerError {
    /// The lowercase protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownEntity => "unknown_entity",
            Self::AlreadyLayer => "already_layer",
            Self::NotALayer => "not_a_layer",
            Self::NoLayers => "no_layers",
        }
    }
}

/// Makes `root` an input layer with `mode` and updates the stack at once.
pub fn push_layer(world: &mut World, root: Entity, mode: LayerInputMode) -> Result<(), LayerError> {
    if world.get_entity(root).is_err() {
        return Err(LayerError::UnknownEntity);
    }
    if world.get::<InputLayer>(root).is_some() {
        return Err(LayerError::AlreadyLayer);
    }
    world.entity_mut(root).insert(InputLayer { mode });
    sync_layers(world);
    Ok(())
}

/// Pops `root` (the top layer when `None`): an open modal is closed, the layer component
/// removed, and the stack updated at once. Returns the popped root.
pub fn pop_layer(world: &mut World, root: Option<Entity>) -> Result<Entity, LayerError> {
    let target = match root {
        Some(root) => root,
        None => world
            .get_resource::<InputLayers>()
            .and_then(InputLayers::top_root)
            .ok_or(LayerError::NoLayers)?,
    };
    if world.get_entity(target).is_err() {
        return Err(LayerError::UnknownEntity);
    }
    if world.get::<InputLayer>(target).is_none() {
        return Err(LayerError::NotALayer);
    }
    if let Some(mut modal) = world.get_mut::<Modal>(target) {
        modal.open = false;
    }
    world.entity_mut(target).remove::<(InputLayer, ModalLayer)>();
    sync_layers(world);
    Ok(target)
}

/// Whether `entity` is `root` or one of its descendants.
pub(crate) fn within(world: &World, root: Entity, entity: Entity) -> bool {
    // Hierarchies are trees; the bound (the one `a11y::ancestors_or_self` uses) only stops a
    // corrupt `ChildOf` cycle from spinning the frame (audit wave 6, accesskit H2). A chain that
    // long is treated as outside the layer.
    let mut current = entity;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if current == root {
            return true;
        }
        match world.get::<ChildOf>(current) {
            Some(parent) => current = parent.0,
            None => return false,
        }
    }
    false
}

/// The most `ChildOf` links [`within`] follows.
const MAX_ANCESTOR_STEPS: usize = 4096;

/// The node to focus when a layer appears: a [`DefaultFocus`] inside it, else its first
/// focusable in reading order.
fn default_focus(world: &mut World, root: Entity) -> Option<Entity> {
    let mut best: Option<(bool, [f32; 2], u64, Entity)> = None;
    let candidates: Vec<(Entity, bool, [f32; 2])> = world
        .query_filtered::<(Entity, Option<&DefaultFocus>, Option<&ComputedNode>), (With<Focusable>, Without<Disabled>)>()
        .iter(world)
        .map(|(e, d, c)| {
            let at = c
                .filter(|c| c.rect[2] > 0.0 && c.rect[3] > 0.0)
                .map_or([f32::MAX, f32::MAX], |c| [c.rect[1], c.rect[0]]);
            (e, d.is_some(), at)
        })
        .collect();
    for (entity, is_default, at) in candidates {
        if !within(world, root, entity) || crate::node::is_disabled(world, entity) {
            continue;
        }
        let key = (!is_default, at, entity.to_bits(), entity);
        let better = best.is_none_or(|b| {
            (b.0, b.1[0], b.1[1], b.2)
                .partial_cmp(&(key.0, key.1[0], key.1[1], key.2))
                .is_some_and(std::cmp::Ordering::is_gt)
        });
        if better {
            best = Some(key);
        }
    }
    best.map(|b| b.3)
}

/// Moves focus (writes `Focused` and the `FocusChanged` events), the world-access twin of
/// [`crate::focus::set_focus`].
pub(crate) fn set_focus_world(world: &mut World, next: Option<Entity>) {
    let prev = world.resource::<FocusState>().focused;
    if prev == next {
        return;
    }
    if let Some(p) = prev
        && let Ok(mut entity) = world.get_entity_mut(p)
    {
        entity.remove::<Focused>();
    }
    if let Some(n) = next
        && let Ok(mut entity) = world.get_entity_mut(n)
    {
        entity.insert(Focused);
    }
    if let Some(p) = prev {
        world.write_message(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    if let Some(n) = next {
        world.write_message(UiEvent::new(n, UiEventKind::FocusChanged(true)));
    }
    world.resource_mut::<FocusState>().focused = next;
}

/// Gives each open modal a layer and takes it away again when it closes.
fn sync_modal_layers(world: &mut World) {
    let modals: Vec<(Entity, bool, bool, bool)> = world
        .query::<(Entity, &Modal, Has<InputLayer>, Has<ModalLayer>)>()
        .iter(world)
        .map(|(e, m, layer, auto)| (e, m.open, layer, auto))
        .collect();
    for (entity, open, has_layer, auto) in modals {
        if open && !has_layer {
            world
                .entity_mut(entity)
                .insert((InputLayer::default(), ModalLayer));
        } else if !open && auto {
            world.entity_mut(entity).remove::<(InputLayer, ModalLayer)>();
        }
    }
}

fn focus_target_valid(world: &World, entity: Entity) -> bool {
    world.get::<Focusable>(entity).is_some() && !crate::node::is_disabled(world, entity)
}

/// Brings [`InputLayers`] in step with the world: pops layers whose node lost [`InputLayer`] or
/// despawned (restoring focus), pushes new ones (focusing their default), follows mode changes
/// and publishes the effective mode in [`UiInputState::layer_mode`]. Runs at the start of the
/// `Update` UI chain; also called directly by the agent's `ui.layers`.
pub fn sync_layers(world: &mut World) {
    let _span = tracing::trace_span!("ui_layers").entered();
    world.init_resource::<InputLayers>();
    world.init_resource::<FocusState>();
    sync_modal_layers(world);
    let mut live: Vec<(Entity, LayerInputMode)> = world
        .query::<(Entity, &InputLayer)>()
        .iter(world)
        .map(|(e, l)| (e, l.mode))
        .collect();
    live.sort_by_key(|(e, _)| e.to_bits());
    // Read the stack through a shared reference. `Mut` field access marks the resource changed,
    // so a quiet frame that only compared the stack still looked like a focus-layer edit.
    let (stack_matches, stack_empty, mode) = {
        let layers = world.resource::<InputLayers>();
        let stack_matches = layers.entries.len() == live.len()
            && layers.entries.iter().zip(&live).all(|(entry, (root, mode))| {
                entry.root == *root && entry.mode == *mode && !entry.pending_default
            });
        (stack_matches, layers.entries.is_empty(), layers.mode())
    };
    let navigation = world
        .get_resource::<crate::focus::UiInputMode>()
        .is_some_and(|mode| *mode == crate::focus::UiInputMode::Navigation);
    let refill = !stack_empty
        && mode != LayerInputMode::GameOnly
        && navigation
        && world.resource::<FocusState>().focused.is_none();
    if stack_matches && !refill {
        let needs_mode = world
            .get_resource::<UiInputState>()
            .is_some_and(|input| input.layer_mode != mode);
        if needs_mode && let Some(mut input) = world.get_resource_mut::<UiInputState>() {
            input.layer_mode = mode;
        }
        return;
    }
    world.resource_scope(|world, mut layers: Mut<InputLayers>| {
        // Pops: any gone layer leaves the stack. The lowest gone layer's saved focus passes to the
        // next surviving layer above it, or, when nothing survives above it, is restored.
        let mut restore: Option<Option<Entity>> = None;
        let mut kept = Vec::with_capacity(layers.entries.len());
        for mut entry in std::mem::take(&mut layers.entries) {
            if live.iter().any(|(e, _)| *e == entry.root) {
                if let Some(saved) = restore.take() {
                    entry.restore_focus = saved;
                }
                kept.push(entry);
            } else if restore.is_none() {
                restore = Some(entry.restore_focus);
            }
        }
        // A gone layer with a survivor above it was handed over; only a popped top restores focus.
        let popped_top = restore;
        layers.entries = kept;
        let mut restore = popped_top;
        // Pushes, in entity order (spawn order for entities created in the same frame).
        let mut pushed = false;
        for (entity, mode) in &live {
            if layers.entries.iter().any(|entry| entry.root == *entity) {
                continue;
            }
            let previous_mode = layers.mode();
            // A layer pushed in the same sync as a pop remembers the focus the pop would restore.
            let restore_focus = popped_top.unwrap_or(world.resource::<FocusState>().focused);
            layers.entries.push(LayerEntry {
                root: *entity,
                mode: *mode,
                previous_mode,
                restore_focus,
                pending_default: true,
            });
            pushed = true;
        }
        for entry in &mut layers.entries {
            if let Some((_, mode)) = live.iter().find(|(e, _)| *e == entry.root) {
                entry.mode = *mode;
            }
        }
        if pushed {
            restore = None;
        }
        // Focus: restore after a pop, else resolve the pending default of the top layer.
        if let Some(previous) = restore {
            let target = previous.filter(|e| focus_target_valid(world, *e));
            let target = match (target, layers.top_root()) {
                (None, Some(root)) => default_focus(world, root),
                (target, _) => target,
            };
            set_focus_world(world, target);
        }
        let top = layers.top_root();
        if let Some(root) = top {
            world.resource_mut::<FocusState>().scope = Some(root);
        }
        let resolve = layers.entries.last().is_some_and(|entry| entry.pending_default);
        if resolve && let Some(root) = top {
            let focused_inside = world
                .resource::<FocusState>()
                .focused
                .is_some_and(|f| within(world, root, f));
            if focused_inside {
                if let Some(entry) = layers.entries.last_mut() {
                    entry.pending_default = false;
                }
            } else if let Some(target) = default_focus(world, root) {
                set_focus_world(world, Some(target));
                if let Some(entry) = layers.entries.last_mut() {
                    entry.pending_default = false;
                }
            } else {
                // Nothing focusable yet (children still spawning): clear focus outside the layer.
                set_focus_world(world, None);
            }
        }
        // Refill an emptied top layer (navigation mode only): repair cleared the focus with no
        // recovery candidate left (every focusable child went away) and `pending_default` was
        // already consumed when the layer opened. Focus the layer's default again so a restored
        // child takes focus without waiting for a navigation press; while no candidate exists
        // the documented empty state stays. Pointer mode is untouched (focus stays cleared
        // until the next press, as repair decided). A world without `UiInputMode` (agents and
        // tests driving `sync_layers` directly) never refills.
        let navigation = world
            .get_resource::<crate::focus::UiInputMode>()
            .is_some_and(|mode| *mode == crate::focus::UiInputMode::Navigation);
        let refill = layers.entries.last().is_some()
            && layers.mode() != LayerInputMode::GameOnly
            && navigation
            && world.resource::<FocusState>().focused.is_none();
        if refill
            && let Some(root) = layers.top_root()
            && let Some(target) = default_focus(world, root)
        {
            set_focus_world(world, Some(target));
        }
        if let Some(mut input) = world.get_resource_mut::<UiInputState>() {
            input.layer_mode = layers.mode();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        for mode in [
            LayerInputMode::UiOnly,
            LayerInputMode::UiAndGame,
            LayerInputMode::GameOnly,
        ] {
            assert_eq!(LayerInputMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(LayerInputMode::parse("nope"), None);
    }

    /// Audit wave 6 (accesskit H2): a `ChildOf` cycle that does not reach the layer root used to
    /// spin `within` forever on every frame an input layer was open. It ends and answers false.
    #[test]
    fn a_child_of_cycle_is_outside_the_layer_and_ends() {
        let mut world = World::new();
        let root = world.spawn_empty().id();
        let a = world.spawn_empty().id();
        let b = world.spawn(ChildOf(a)).id();
        world.entity_mut(a).insert(ChildOf(b));
        assert_eq!(
            world.get::<ChildOf>(a).map(|p| p.0),
            Some(b),
            "the cycle is in place"
        );
        assert!(!within(&world, root, a));
        assert!(within(&world, b, a), "a node inside reaches its ancestor");
        let child = world.spawn(ChildOf(root)).id();
        assert!(within(&world, root, child));
    }

    #[test]
    fn empty_stack_is_ui_and_game() {
        let layers = InputLayers::default();
        assert_eq!(layers.mode(), LayerInputMode::UiAndGame);
        assert_eq!(layers.depth(), 0);
        assert!(layers.top_root().is_none());
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        for e in [
            LayerError::UnknownEntity,
            LayerError::AlreadyLayer,
            LayerError::NotALayer,
            LayerError::NoLayers,
        ] {
            assert!(e.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }
}
