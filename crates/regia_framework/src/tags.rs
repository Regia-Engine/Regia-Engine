// SPDX-License-Identifier: MPL-2.0
//! Gameplay tags: hierarchical names such as `Combat.Damage.Fire`, interned to cheap ids, held in
//! small sets on entities and matched with queries.
//!
//! Specification: `docs/decisions.md` ADR-1463; guide: `book/agents/gameplay-tags.md`.
//!
//! - [`Tag`]: an interned name. Comparing and hashing use a 64-bit FNV-1a id of the full name
//!   that is the same in every run, so ids may be recorded; saving uses the name.
//! - [`TagContainer`]: a sorted set of tags, a reflected component. `has` matches a held child
//!   (`Combat.Damage` matches a container holding `Combat.Damage.Fire`); `has_exact` does not.
//! - [`TagQuery`]: all-of, any-of and none-of over tags, nestable, with a RON form.
//! - [`TagTable`] / [`TagRegistry`]: the `*.tags.ron` list of declared tags and the resource built
//!   from it, with a validator that reports undeclared and invalid tags.
//! - [`TagPlugin`]: registers the types, owns the [`TagRegistry`] resource and raises a
//!   [`TagsChanged`] message when a [`TagContainer`] changes.
//!
//! ```
//! use regia_framework::tags::{TagContainer, TagQuery};
//!
//! let mut held = TagContainer::from_names(["Combat.Damage.Fire"]).expect("valid names");
//! let hurt_by_damage = TagQuery::from_ron(r#"Has("Combat.Damage")"#).expect("query");
//! assert!(held.matches(&hurt_by_damage));
//! held.clear();
//! assert!(!held.matches(&hurt_by_damage));
//! ```

mod container;
mod name;
mod query;
mod registry;

pub use container::TagContainer;
pub use name::{MAX_SEGMENT_LEN, MAX_TAG_DEPTH, MAX_TAG_NAME_LEN, Tag, TagError, fnv1a64, validate_tag_name};
pub use query::{MAX_QUERY_DEPTH, MAX_QUERY_NODES, TagLookup, TagQuery};
pub use registry::{
    TAG_TABLE_FORMAT_VERSION, TagDecl, TagDiagnostic, TagEntry, TagRegistry, TagTable,
    tag_table_migration_sources, tag_table_schema_fingerprint,
};

use regia_app::prelude::*;

/// What a [`TagContainer`] held when [`TagsChanged`] was last computed for it. Added to every
/// entity with a container; not reflected and not part of any save.
#[derive(Component, Debug, Clone, Default)]
pub struct TagSnapshot(Vec<Tag>);

/// Raised (as a message) after a [`TagContainer`] changes: which tags it gained and lost since
/// the last report. The first report for a new container lists all of its tags as `added`. A
/// container that is changed and changed back in one frame raises nothing. A despawned entity
/// raises nothing.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct TagsChanged {
    /// The entity whose container changed.
    pub entity: Entity,
    /// Explicit tags now held that were not before, sorted.
    pub added: Vec<Tag>,
    /// Explicit tags held before that are gone, sorted.
    pub removed: Vec<Tag>,
}

/// The [`PostUpdate`] step that raises [`TagsChanged`]. Order systems that react to tag changes
/// after it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct TagsSet;

/// Registers the tag types, the [`TagRegistry`] resource and the change message.
pub struct TagPlugin;

impl Plugin for TagPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Tag>()
            .register_type::<TagContainer>()
            .init_resource::<TagRegistry>()
            .add_message::<TagsChanged>()
            .add_systems(PostUpdate, report_tag_changes.in_set(TagsSet));
    }
}

/// `PostUpdate`: repairs an unsorted container loaded from a scene file and raises
/// [`TagsChanged`] for every container that changed.
#[tracing::instrument(skip_all, target = "regia_framework", name = "tags_report_changes")]
fn report_tag_changes(
    mut changed: MessageWriter<TagsChanged>,
    mut query: Query<(Entity, &mut TagContainer, &mut TagSnapshot)>,
) {
    for (entity, mut container, mut snapshot) in &mut query {
        if !container.is_changed() {
            continue;
        }
        if !container.is_normalized() {
            container.normalize();
        }
        let added: Vec<Tag> = container.iter().filter(|tag| !snapshot.0.contains(tag)).collect();
        let removed: Vec<Tag> = snapshot
            .0
            .iter()
            .copied()
            .filter(|tag| !container.has_exact(*tag))
            .collect();
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        snapshot.0 = container.iter().collect();
        changed.write(TagsChanged {
            entity,
            added,
            removed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::message::{MessageCursor, Messages};

    fn t(name: &str) -> Tag {
        Tag::new(name).expect("valid tag")
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TagPlugin));
        app
    }

    fn drain(app: &App, cursor: &mut MessageCursor<TagsChanged>) -> Vec<TagsChanged> {
        cursor
            .read(app.world().resource::<Messages<TagsChanged>>())
            .cloned()
            .collect()
    }

    #[test]
    fn spawn_then_edit_raises_one_message_per_change() {
        let mut app = app();
        let mut cursor = MessageCursor::default();
        let entity = app
            .world_mut()
            .spawn(TagContainer::from_names(["Ecs.A", "Ecs.B"]).expect("names"))
            .id();
        app.update();
        let first = drain(&app, &mut cursor);
        assert_eq!(
            first,
            vec![TagsChanged {
                entity,
                added: vec![t("Ecs.A"), t("Ecs.B")],
                removed: vec![]
            }]
        );

        // No edit, no message.
        app.update();
        assert!(drain(&app, &mut cursor).is_empty());

        {
            let mut world = app.world_mut().entity_mut(entity);
            let mut tags = world.get_mut::<TagContainer>().expect("container");
            tags.remove(t("Ecs.A"));
            tags.add(t("Ecs.C"));
        }
        app.update();
        assert_eq!(
            drain(&app, &mut cursor),
            vec![TagsChanged {
                entity,
                added: vec![t("Ecs.C")],
                removed: vec![t("Ecs.A")]
            }]
        );
    }

    #[test]
    fn a_no_op_edit_and_a_round_trip_edit_raise_nothing() {
        let mut app = app();
        let mut cursor = MessageCursor::default();
        let entity = app
            .world_mut()
            .spawn(TagContainer::from_names(["Noop.A"]).expect("names"))
            .id();
        app.update();
        drain(&app, &mut cursor);
        {
            let mut world = app.world_mut().entity_mut(entity);
            let mut tags = world.get_mut::<TagContainer>().expect("container");
            // Flags the component as changed but leaves the set as it was.
            tags.add(t("Noop.A"));
            tags.add(t("Noop.B"));
            tags.remove(t("Noop.B"));
        }
        app.update();
        assert!(drain(&app, &mut cursor).is_empty());
    }

    #[test]
    fn the_container_is_a_reflected_component_that_survives_a_reflect_round_trip() {
        let app = app();
        let registry = app.world().resource::<AppTypeRegistry>().read();
        let registration = registry
            .get(std::any::TypeId::of::<TagContainer>())
            .expect("TagContainer is registered");
        assert!(
            registration
                .data::<regia_ecs::reflect::ReflectComponent>()
                .is_some()
        );
        assert!(registry.get(std::any::TypeId::of::<Tag>()).is_some());

        // The reflect serializer writes names (the form a scene file holds), not ids.
        let container = TagContainer::from_names(["Reflect.Fire"]).expect("names");
        let serializer = regia_ecs::reflect::serde::TypedReflectSerializer::new(&container, &registry);
        let json = serde_json::to_string(&serializer).expect("serialize");
        assert!(json.contains("Reflect.Fire"), "{json}");
    }
}
