// SPDX-License-Identifier: MPL-2.0
//! Procedural edits on the undo stack (spec 64 § 2.3 and § 7.3): a `spline.*` call and a terrain
//! flatten are one entry each.
//!
//! `spline.*` lives in `regia_agent` and generators publish in `regia_procgen`, neither of which
//! knows the editor, so both log their edits to a resource only the editor installs, and
//! `drain` turns the logs into entries. It runs every editor frame and before anything reads the
//! stack, so an `editor.undo` in the same frame as the edit reverts that edit.

use std::any::Any;

use regia_agent::{SplineEdit, SplineEditLog};
use regia_core::id::Guid;
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_procgen::terrain_edits::{ProcTerrainEditLog, TerrainEditChange, set_records};
use regia_procgen::world::ProcGenerator;
use regia_scene::prelude::EntityGuid;
use regia_scene::spline::Spline;

use crate::command::{EditorCommand, UndoStack, resolve_entity};

/// The entity with `guid` when it is a generator.
fn generator(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query_filtered::<(Entity, &EntityGuid), With<ProcGenerator>>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
}

/// One change of what a generator's terrain holds (`terrain.flatten_road`, restored bit for bit by
/// undo). A generator that no longer exists leaves the entry doing nothing.
pub struct TerrainEditUndo {
    change: TerrainEditChange,
}

impl TerrainEditUndo {
    fn write(&self, world: &mut World, before: bool) {
        let Some(e) = generator(world, self.change.generator) else {
            return;
        };
        let records = if before {
            &self.change.before
        } else {
            &self.change.after
        };
        if let Err((code, message)) = set_records(world, e, records) {
            tracing::warn!(target: "regia_editor", code = code.as_str(), "{message}");
        }
    }
}

impl EditorCommand for TerrainEditUndo {
    fn apply(&mut self, world: &mut World) {
        self.write(world, false);
    }

    fn revert(&mut self, world: &mut World) {
        self.write(world, true);
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        // Re-running one generator keeps one entry until the stack is sealed.
        let next = next as &dyn Any;
        match next.downcast_ref::<Self>() {
            Some(next) if next.change.generator == self.change.generator => {
                self.change.after = next.change.after.clone();
                true
            }
            _ => false,
        }
    }

    fn description(&self) -> String {
        "Generator terrain edit".to_string()
    }
}

/// One `spline.set` or `spline.edit`.
pub struct SplineUndo {
    entity: u64,
    before: Option<Spline>,
    after: Spline,
}

impl EditorCommand for SplineUndo {
    fn apply(&mut self, world: &mut World) {
        if let Some(e) = resolve_entity(world, self.entity) {
            world.entity_mut(e).insert(self.after.clone());
        }
    }

    fn revert(&mut self, world: &mut World) {
        let Some(e) = resolve_entity(world, self.entity) else {
            return;
        };
        match &self.before {
            Some(s) => {
                world.entity_mut(e).insert(s.clone());
            }
            None => {
                world.entity_mut(e).remove::<Spline>();
            }
        }
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Edit spline".to_string()
    }
}

/// Moves the logged edits onto the undo stack, in the order they happened: a spline edit, then the
/// run it caused.
pub(crate) fn drain(world: &mut World) {
    let splines: Vec<SplineEdit> = world
        .get_resource_mut::<SplineEditLog>()
        .map(|mut log| std::mem::take(&mut log.0))
        .unwrap_or_default();
    let terrain: Vec<TerrainEditChange> = world
        .get_resource_mut::<ProcTerrainEditLog>()
        .map(|mut log| std::mem::take(&mut log.0))
        .unwrap_or_default();
    if splines.is_empty() && terrain.is_empty() {
        return;
    }
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    for edit in splines {
        stack.seal();
        stack.push(Box::new(SplineUndo {
            entity: edit.entity.to_bits(),
            before: edit.before,
            after: edit.after,
        }));
        stack.seal();
    }
    for change in terrain {
        stack.push(Box::new(TerrainEditUndo { change }));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_procgen::exec::TerrainEditRecord;
    use regia_procgen::terrain_edits::ProcTerrainApplied;
    use regia_scene::spline::{Spline, TangentMode};

    use super::*;
    use crate::command::{redo_world, undo_world};

    fn line(x: f32) -> Spline {
        let mut s = Spline::through([
            regia_core::math::Vec3::new(0.0, 0.0, 0.0),
            regia_core::math::Vec3::new(x, 0.0, 0.0),
        ]);
        s.points.iter_mut().for_each(|p| p.mode = TangentMode::Linear);
        s
    }

    fn world() -> World {
        let mut w = World::new();
        w.init_resource::<SplineEditLog>();
        w.init_resource::<ProcTerrainEditLog>();
        w.insert_resource(UndoStack::new());
        w
    }

    #[test]
    fn a_spline_edit_is_one_entry_even_in_the_frame_it_happened() {
        let mut w = world();
        let e = w.spawn(line(1.0)).id();
        w.entity_mut(e).insert(line(2.0));
        w.resource_mut::<SplineEditLog>().0.push(SplineEdit {
            entity: e,
            before: Some(line(1.0)),
            after: line(2.0),
        });
        assert!(undo_world(&mut w), "undo drains the log first");
        assert_eq!(w.get::<Spline>(e), Some(&line(1.0)));
        assert!(redo_world(&mut w));
        assert_eq!(w.get::<Spline>(e), Some(&line(2.0)));
    }

    #[test]
    fn undoing_the_call_that_added_a_spline_removes_it() {
        let mut w = world();
        let e = w.spawn_empty().id();
        w.entity_mut(e).insert(line(2.0));
        w.resource_mut::<SplineEditLog>().0.push(SplineEdit {
            entity: e,
            before: None,
            after: line(2.0),
        });
        assert!(undo_world(&mut w));
        assert!(w.get::<Spline>(e).is_none());
    }

    #[test]
    fn terrain_entries_of_one_generator_merge_until_the_stack_seals() {
        let records = |v: f32| {
            Arc::new(vec![TerrainEditRecord {
                rect: [0, 0, 1, 1],
                samples: vec![v],
                original: vec![0.0],
            }])
        };
        let change = |before: f32, after: f32| TerrainEditChange {
            generator: Guid(7),
            before: records(before),
            after: records(after),
        };
        let mut w = world();
        let mut log = w.resource_mut::<ProcTerrainEditLog>();
        log.0.push(change(0.0, 1.0));
        log.0.push(change(1.0, 2.0));
        drain(&mut w);
        let stack = w.resource::<UndoStack>();
        assert_eq!(stack.undo_depth(), 1);
        assert_eq!(stack.next_undo().as_deref(), Some("Generator terrain edit"));
        // No such generator here: an entry that finds nothing changes nothing and does not panic.
        assert!(undo_world(&mut w));
        assert!(w.query::<&ProcTerrainApplied>().iter(&w).next().is_none());
    }
}
