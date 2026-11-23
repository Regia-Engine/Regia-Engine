// SPDX-License-Identifier: MPL-2.0
//! One undo step for an Abilities inspector operation in the edit world (spec 53 § 6 parity):
//! adding an attribute set or applying an effect changes an entity's ability components, so the
//! step keeps them as they were before and after and puts back whichever side is asked for.

use regia_ecs::prelude::*;
use regia_framework::abilities::{AbilityBook, AppliedEffects, Attributes, TagCounts};

use crate::command::{EditorCommand, UndoStack, resolve_entity};

/// The ability components of one entity (each `None` when it has none).
#[derive(Clone, Default)]
pub(super) struct AbilityComponents {
    attributes: Option<Attributes>,
    effects: Option<AppliedEffects>,
    book: Option<AbilityBook>,
    tags: Option<TagCounts>,
}

impl AbilityComponents {
    /// `e`'s ability components now.
    pub(super) fn of(world: &World, e: Entity) -> Self {
        Self {
            attributes: world.get::<Attributes>(e).cloned(),
            effects: world.get::<AppliedEffects>(e).cloned(),
            book: world.get::<AbilityBook>(e).cloned(),
            tags: world.get::<TagCounts>(e).cloned(),
        }
    }

    fn restore(&self, world: &mut World, e: Entity) {
        let Ok(mut entity) = world.get_entity_mut(e) else {
            return;
        };
        fn put<T: Component + Clone>(entity: &mut EntityWorldMut<'_>, value: Option<&T>) {
            match value {
                Some(value) => {
                    entity.insert(value.clone());
                }
                None => {
                    entity.remove::<T>();
                }
            }
        }
        put(&mut entity, self.attributes.as_ref());
        put(&mut entity, self.effects.as_ref());
        put(&mut entity, self.book.as_ref());
        put(&mut entity, self.tags.as_ref());
    }
}

/// An Abilities inspector operation that already ran on the edit world, as one undo entry.
pub(super) struct AbilityEdit {
    entity: u64,
    label: String,
    before: AbilityComponents,
    after: AbilityComponents,
}

impl EditorCommand for AbilityEdit {
    fn apply(&mut self, world: &mut World) {
        if let Some(e) = resolve_entity(world, self.entity) {
            self.after.restore(world, e);
        }
    }

    fn revert(&mut self, world: &mut World) {
        if let Some(e) = resolve_entity(world, self.entity) {
            self.before.restore(world, e);
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// Pushes the operation that turned `before` into `e`'s current ability components onto the edit
/// world's undo stack as one sealed step (nothing when the world keeps no undo stack).
pub(super) fn record(world: &mut World, e: Entity, label: String, before: AbilityComponents) {
    if !world.contains_resource::<UndoStack>() {
        return;
    }
    let after = AbilityComponents::of(world, e);
    let mut stack = world.resource_mut::<UndoStack>();
    stack.seal();
    stack.push(Box::new(AbilityEdit {
        entity: e.to_bits(),
        label,
        before,
        after,
    }));
    stack.seal();
}
