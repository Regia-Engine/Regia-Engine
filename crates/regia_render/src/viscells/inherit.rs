// SPDX-License-Identifier: MPL-2.0
//! Inherited cells (stage 2): an entity without [`VisCell`] takes its nearest ancestor's, as
//! `MapLayer` does (`docs/specs/25-sim-bridge-and-do27.md` § 7.3). DO27's bake tags a building
//! root and spawns the parts as its children; without inheritance every part drew everywhere.
//!
//! The resolution is cached, not walked per draw: [`InheritedCells`] maps every untagged
//! descendant of a tagged entity to the cell it inherits. It is rebuilt only on a frame where a
//! `VisCell` or a `ChildOf` was added, changed or removed ([`VisCellMembers::take_edited`]): one
//! top-down pass from each tagged entity, stopping at descendants that carry their own tag (they
//! start their own pass). A steady frame costs two change-tick scans and allocates nothing; a
//! rebuild reuses the map's and the stack's capacity.

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemParam;

use super::VisCell;

/// A `VisCell` or a `ChildOf` added or changed since the cull last ran.
type HierarchyEdited = Or<(Changed<VisCell>, Changed<ChildOf>)>;

/// The `VisCell` lookups of the cell cull: the entity's own tag, else the nearest tagged
/// ancestor's (stage 2), and what can make the cached inheritance stale.
#[derive(SystemParam)]
pub struct VisCellMembers<'w, 's> {
    own: Query<'w, 's, &'static VisCell>,
    tagged: Query<'w, 's, (Entity, &'static VisCell)>,
    children: Query<'w, 's, &'static Children>,
    edited: Query<'w, 's, (), HierarchyEdited>,
    untagged: RemovedComponents<'w, 's, VisCell>,
    unparented: RemovedComponents<'w, 's, ChildOf>,
}

impl VisCellMembers<'_, '_> {
    /// Whether a tag or a parent link was added, changed or removed since the last call (the
    /// removal readers are drained either way).
    pub(super) fn take_edited(&mut self) -> bool {
        let removed = self.untagged.read().count() + self.unparented.read().count();
        removed > 0 || !self.edited.is_empty()
    }

    /// The cell `entity` draws in: its own [`VisCell`], else the cached inherited one, else
    /// `None` (no tag up the chain: always drawn).
    pub(super) fn cell_of(&self, entity: Entity, inherited: &InheritedCells) -> Option<u16> {
        match self.own.get(entity) {
            Ok(cell) => Some(cell.0),
            Err(_) => inherited.get(entity),
        }
    }
}

/// The cells untagged entities inherit from their nearest tagged ancestor (module docs).
#[derive(Debug, Default)]
pub struct InheritedCells {
    by_entity: HashMap<Entity, u16>,
    stack: Vec<(Entity, u16)>,
    built: bool,
}

impl InheritedCells {
    /// The cell `entity` inherits, if a tagged ancestor gives it one.
    #[must_use]
    pub fn get(&self, entity: Entity) -> Option<u16> {
        self.by_entity.get(&entity).copied()
    }

    /// Untagged entities that inherit a cell.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_entity.len()
    }

    /// No entity inherits a cell.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_entity.is_empty()
    }

    /// Marks the map stale when `members` saw an edit since the last call. Call it every frame,
    /// culling or not, so the removal readers stay drained.
    pub(super) fn note_edits(&mut self, members: &mut VisCellMembers) {
        if members.take_edited() {
            self.built = false;
        }
    }

    /// Rebuilds the map if it is stale (or was never built). Returns whether it rebuilt.
    pub(super) fn refresh(&mut self, members: &VisCellMembers) -> bool {
        if self.built {
            return false;
        }
        let _span = tracing::info_span!("viscells_inherit").entered();
        self.built = true;
        self.by_entity.clear();
        self.stack.clear();
        for (root, cell) in &members.tagged {
            self.push_untagged_children(members, root, cell.0);
            while let Some((entity, cell)) = self.stack.pop() {
                // A `ChildOf` cycle (bevy does not prevent one) revisits an entity: stop there.
                if self.by_entity.insert(entity, cell).is_some() {
                    continue;
                }
                self.push_untagged_children(members, entity, cell);
            }
        }
        true
    }

    fn push_untagged_children(&mut self, members: &VisCellMembers, parent: Entity, cell: u16) {
        let Ok(children) = members.children.get(parent) else {
            return;
        };
        let children: &[Entity] = children;
        for &child in children {
            // A tagged descendant keeps its own cell and passes it on from its own pass.
            if !members.own.contains(child) {
                self.stack.push((child, cell));
            }
        }
    }
}
