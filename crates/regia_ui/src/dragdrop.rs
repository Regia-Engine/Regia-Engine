// SPDX-License-Identifier: MPL-2.0
//! Drag and drop between widgets (`docs/specs/ui-parity-roadmap.md` W16, ADR-517).
//!
//! A node with [`DragSource`] starts a drag operation when the interaction system reports
//! [`crate::UiEventKind::DragStart`] on it. The operation carries a typed [`DragPayload`] and the
//! grab offset (where in the node the pointer took hold, so a ghost can be drawn at the right
//! place). While it runs, the topmost interactable node under the pointer is resolved to the
//! nearest ancestor-or-self [`DropTarget`]; that target is *hovered*, and it either accepts the
//! payload kind or rejects it. Releasing over an accepting target drops; releasing anywhere else,
//! pressing Escape, losing the source, or an input layer that hides the source cancels. Nothing is
//! moved by the engine: the game reacts to [`DragDropEvent`]s, so a cancel needs no undo.
//!
//! All state is plain data and the system reads only pointer, keyboard and [`crate::UiEvent`]
//! messages, so a headless test drives it with fixed steps.

use regia_app::prelude::*;

use crate::focus::in_scope;
use crate::interaction::{HitCandidate, UiEvent, UiEventKind, UiPointer, UiValue, pick_topmost};
use crate::layers::InputLayers;
use crate::node::{ComputedNode, Disabled, Interactable};

/// What a drag carries: a type tag the targets filter on and a value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DragPayload {
    /// The payload type, e.g. `"item"` or `"tab"`. A [`DropTarget`] lists the kinds it accepts.
    pub kind: String,
    /// The value (an item id, an index, text...).
    pub data: UiValue,
}

impl DragPayload {
    /// A payload of `kind` carrying `data`.
    pub fn new(kind: impl Into<String>, data: UiValue) -> Self {
        Self {
            kind: kind.into(),
            data,
        }
    }
}

/// Opt-in: dragging this node (past the interaction system's drag threshold) starts a drag and
/// drop operation carrying `payload`. The node must be [`Interactable`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct DragSource {
    /// The payload the operation carries.
    pub payload: DragPayload,
}

impl DragSource {
    /// A source that carries `payload`.
    pub fn new(payload: DragPayload) -> Self {
        Self { payload }
    }
}

/// Opt-in: the node (or any [`Interactable`] descendant of it) can receive drops. The node
/// should be [`Interactable`] itself unless a descendant covers it fully.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct DropTarget {
    /// The payload kinds this target accepts; empty accepts every kind.
    pub accepts: Vec<String>,
}

impl DropTarget {
    /// A target that accepts exactly the listed kinds.
    pub fn accepting<I, S>(kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            accepts: kinds.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether a payload of `kind` may be dropped here.
    pub fn accepts_kind(&self, kind: &str) -> bool {
        self.accepts.is_empty() || self.accepts.iter().any(|k| k == kind)
    }
}

/// Marker on the source node while its drag operation runs.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DragActive;

/// Marker on the [`DropTarget`] node under the pointer while a drag runs.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropHover {
    /// Whether the dragged payload kind would be accepted here (`false`: show a "no drop" cue).
    pub accepted: bool,
}

/// Why an operation ended without a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragCancelReason {
    /// Escape was pressed.
    Escape,
    /// The button was released over no drop target.
    NoTarget,
    /// The button was released over a target that does not accept the payload kind.
    Rejected,
    /// The source node was despawned or lost its layout.
    SourceGone,
    /// An input layer opened that excludes the source.
    LayerChanged,
    /// [`cancel_drag`] was called (the agent's `cancel` action, or game code).
    Requested,
}

impl DragCancelReason {
    /// The protocol name (lowercase snake_case).
    pub fn name(self) -> &'static str {
        match self {
            Self::Escape => "escape",
            Self::NoTarget => "no_target",
            Self::Rejected => "rejected",
            Self::SourceGone => "source_gone",
            Self::LayerChanged => "layer_changed",
            Self::Requested => "requested",
        }
    }
}

/// Drag and drop events, delivered as ECS messages.
#[derive(Message, Debug, Clone, PartialEq)]
pub enum DragDropEvent {
    /// An operation began on `source`.
    Started {
        /// The dragged node.
        source: Entity,
        /// What it carries.
        payload: DragPayload,
    },
    /// The pointer entered `target` during an operation.
    HoverEnter {
        /// The dragged node.
        source: Entity,
        /// The drop target now under the pointer.
        target: Entity,
        /// Whether the target accepts the payload kind.
        accepted: bool,
    },
    /// The pointer left `target` during an operation.
    HoverLeave {
        /// The dragged node.
        source: Entity,
        /// The drop target the pointer left.
        target: Entity,
    },
    /// The payload was dropped on an accepting target.
    Dropped {
        /// The dragged node.
        source: Entity,
        /// The target that received the drop.
        target: Entity,
        /// What was dropped.
        payload: DragPayload,
    },
    /// The operation ended without a drop.
    Cancelled {
        /// The dragged node.
        source: Entity,
        /// Why.
        reason: DragCancelReason,
    },
}

/// A running operation.
#[derive(Debug, Clone, PartialEq)]
pub struct DragOperation {
    /// The dragged node.
    pub source: Entity,
    /// What it carries.
    pub payload: DragPayload,
    /// Where in the source's rect the pointer took hold (physical px from the top-left corner).
    pub grab_offset: [f32; 2],
    /// Current pointer position (physical px); `None` while the pointer is outside the window.
    pub position: Option<[f32; 2]>,
    /// The drop target under the pointer and whether it accepts the payload.
    pub hover: Option<(Entity, bool)>,
}

impl DragOperation {
    /// Where a ghost of a `size` (w, h) node belongs so it stays where it was grabbed: the
    /// pointer position minus the grab offset. `None` while the pointer is outside the window.
    pub fn ghost_origin(&self) -> Option<[f32; 2]> {
        self.position
            .map(|p| [p[0] - self.grab_offset[0], p[1] - self.grab_offset[1]])
    }
}

/// The drag and drop state: at most one operation runs at a time.
#[derive(Resource, Debug, Default)]
pub struct DragDrop {
    active: Option<DragOperation>,
    cancel_requested: bool,
}

impl DragDrop {
    /// The running operation, if any.
    pub fn active(&self) -> Option<&DragOperation> {
        self.active.as_ref()
    }
}

/// Asks the running operation (if any) to cancel with [`DragCancelReason::Requested`] at the next
/// `ui_drag_drop` run. Returns whether an operation was running.
pub fn cancel_drag(state: &mut DragDrop) -> bool {
    if state.active.is_some() {
        state.cancel_requested = true;
        true
    } else {
        false
    }
}

/// The nearest [`DropTarget`] at or above `start`, by walking `parent_of`.
pub fn nearest_target(
    start: Entity,
    parent_of: impl Fn(Entity) -> Option<Entity>,
    is_target: impl Fn(Entity) -> bool,
) -> Option<Entity> {
    let mut current = start;
    // A hierarchy cannot loop, but a bounded walk costs nothing and never hangs on bad data.
    for _ in 0..4096 {
        if is_target(current) {
            return Some(current);
        }
        current = parent_of(current)?;
    }
    None
}

fn end_markers(commands: &mut Commands, op: &DragOperation) {
    if let Ok(mut source) = commands.get_entity(op.source) {
        source.remove::<DragActive>();
    }
    if let Some((target, _)) = op.hover
        && let Ok(mut target) = commands.get_entity(target)
    {
        target.remove::<DropHover>();
    }
}

/// `Update`, after [`crate::interaction::ui_interaction`]: runs the drag and drop state machine.
pub fn ui_drag_drop(
    pointer: Res<UiPointer>,
    mut ui_events: MessageReader<UiEvent>,
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut state: ResMut<DragDrop>,
    mut out: MessageWriter<DragDropEvent>,
    sources: Query<(&DragSource, &ComputedNode, Option<&Disabled>)>,
    computed: Query<&ComputedNode>,
    nodes: Query<(Entity, &ComputedNode, &Interactable, Option<&Disabled>)>,
    targets: Query<&DropTarget>,
    parents: Query<&ChildOf>,
    disabled_subtrees: Query<(), With<crate::node::DisabledSubtree>>,
    layers: Res<InputLayers>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_drag_drop").entered();
    let mut escape = false;
    for key in keys.read() {
        escape |= key.state == regia_platform::ButtonState::Pressed && key.logical_key == "Escape";
    }
    let mut ended = false;
    for event in ui_events.read() {
        match &event.kind {
            UiEventKind::DragStart if state.active.is_none() => {
                let Ok((source, node, disabled)) = sources.get(event.target) else {
                    continue;
                };
                if disabled.is_some()
                    || crate::node::subtree_disabled(event.target, &parents, &disabled_subtrees)
                {
                    continue;
                }
                let position = pointer.position();
                let grab_offset = position.map_or([0.0, 0.0], |p| [p[0] - node.rect[0], p[1] - node.rect[1]]);
                commands.entity(event.target).insert(DragActive);
                out.write(DragDropEvent::Started {
                    source: event.target,
                    payload: source.payload.clone(),
                });
                state.active = Some(DragOperation {
                    source: event.target,
                    payload: source.payload.clone(),
                    grab_offset,
                    position,
                    hover: None,
                });
                state.cancel_requested = false;
            }
            UiEventKind::DragEnd if state.active.as_ref().is_some_and(|op| op.source == event.target) => {
                ended = true;
            }
            _ => {}
        }
    }
    let Some(mut op) = state.active.take() else {
        return;
    };

    // Conditions that cancel before any hover work.
    let top_layer = layers.top_root();
    let cancel = if escape {
        Some(DragCancelReason::Escape)
    } else if state.cancel_requested || crate::node::subtree_disabled(op.source, &parents, &disabled_subtrees)
    {
        Some(DragCancelReason::Requested)
    } else if computed.get(op.source).is_err() {
        Some(DragCancelReason::SourceGone)
    } else if top_layer.is_some_and(|root| !in_scope(&parents, root, op.source)) {
        Some(DragCancelReason::LayerChanged)
    } else {
        None
    };
    if let Some(reason) = cancel {
        end_markers(&mut commands, &op);
        if let Some((target, _)) = op.hover {
            out.write(DragDropEvent::HoverLeave {
                source: op.source,
                target,
            });
        }
        out.write(DragDropEvent::Cancelled {
            source: op.source,
            reason,
        });
        state.cancel_requested = false;
        return;
    }

    // Hover: topmost interactable outside the source's own subtree, resolved to its target.
    op.position = pointer.position();
    let new_hover = op.position.and_then(|p| {
        let candidates = nodes
            .iter()
            .filter(|(entity, ..)| {
                top_layer.is_none_or(|root| in_scope(&parents, root, *entity))
                    && !in_scope(&parents, op.source, *entity)
            })
            .map(|(entity, node, interactable, disabled)| HitCandidate {
                entity,
                rect: node.rect,
                clip: node.clip_rect,
                pick_through: interactable.pick_through,
                disabled: disabled.is_some()
                    || crate::node::subtree_disabled(entity, &parents, &disabled_subtrees),
                z: node.z,
                paint_order: node.paint_order,
            });
        let hit = pick_topmost(candidates, p)?;
        let target = nearest_target(
            hit,
            |e| parents.get(e).ok().map(|parent| parent.0),
            |e| targets.contains(e),
        )?;
        let accepted = targets
            .get(target)
            .is_ok_and(|t| t.accepts_kind(&op.payload.kind));
        Some((target, accepted))
    });
    if new_hover != op.hover {
        if let Some((old, _)) = op.hover {
            out.write(DragDropEvent::HoverLeave {
                source: op.source,
                target: old,
            });
            if let Ok(mut old) = commands.get_entity(old) {
                old.remove::<DropHover>();
            }
        }
        if let Some((target, accepted)) = new_hover {
            out.write(DragDropEvent::HoverEnter {
                source: op.source,
                target,
                accepted,
            });
            commands.entity(target).insert(DropHover { accepted });
        }
        op.hover = new_hover;
    }

    if ended {
        end_markers(&mut commands, &op);
        match op.hover {
            Some((target, true)) => {
                out.write(DragDropEvent::Dropped {
                    source: op.source,
                    target,
                    payload: op.payload.clone(),
                });
            }
            hover => {
                if let Some((target, _)) = hover {
                    out.write(DragDropEvent::HoverLeave {
                        source: op.source,
                        target,
                    });
                }
                out.write(DragDropEvent::Cancelled {
                    source: op.source,
                    reason: if hover.is_some() {
                        DragCancelReason::Rejected
                    } else {
                        DragCancelReason::NoTarget
                    },
                });
            }
        }
        return;
    }
    state.active = Some(op);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_accept_list_takes_every_kind() {
        let any = DropTarget::default();
        assert!(any.accepts_kind("item") && any.accepts_kind(""));
        let only = DropTarget::accepting(["item", "tab"]);
        assert!(only.accepts_kind("tab"));
        assert!(!only.accepts_kind("file"));
    }

    #[test]
    fn nearest_target_walks_up_and_stops_at_the_root() {
        let a = Entity::from_bits(1);
        let b = Entity::from_bits(2);
        let c = Entity::from_bits(3);
        let parent = |e: Entity| match e.to_bits() {
            3 => Some(b),
            2 => Some(a),
            _ => None,
        };
        assert_eq!(nearest_target(c, parent, |e| e == b), Some(b), "ancestor");
        assert_eq!(nearest_target(c, parent, |e| e == c), Some(c), "self first");
        assert_eq!(nearest_target(c, parent, |_| false), None, "no target above");
    }

    #[test]
    fn ghost_follows_the_grab_point() {
        let op = DragOperation {
            source: Entity::from_bits(1),
            payload: DragPayload::default(),
            grab_offset: [10.0, 4.0],
            position: Some([110.0, 54.0]),
            hover: None,
        };
        assert_eq!(op.ghost_origin(), Some([100.0, 50.0]));
        let outside = DragOperation { position: None, ..op };
        assert_eq!(outside.ghost_origin(), None);
    }

    #[test]
    fn cancel_names_are_protocol_snake_case() {
        for reason in [
            DragCancelReason::Escape,
            DragCancelReason::NoTarget,
            DragCancelReason::Rejected,
            DragCancelReason::SourceGone,
            DragCancelReason::LayerChanged,
            DragCancelReason::Requested,
        ] {
            assert!(
                reason
                    .name()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }
}
