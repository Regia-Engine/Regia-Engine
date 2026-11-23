// SPDX-License-Identifier: MPL-2.0
//! Destructibles placed in a level (ADR-9750): a [`DestructibleSource`] on an entity names a
//! destructible asset, and [`define_destructible_sources`] places it in [`Destruction`] at the
//! entity's `Transform` the first frame the asset can be read.
//!
//! The component is what the editor's Fracture panel **Place** button spawns
//! (`editor.fracture_place`) and what a level file saves. `asset` is either a manifest path (the
//! cook's `<mesh>#fracture/destructible` sub-asset, whose chunks name the cooked chunk meshes in
//! `lods`) or a project-relative `.destructible.ron` the panel saved (read from the asset server's
//! project root; its `meshes` names the `.fracture.ron` beside it).
//!
//! The system runs in `PreUpdate`, outside the fixed tick, so it never moves a determinism pin of a
//! world without placed destructibles. Pending entities are defined in entity order, which is the
//! order a level file spawns them in, so a server and its clients that load the same level agree on
//! instance ids.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer};
use regia_scene::Transform;

use super::asset::DestructibleAsset;
use super::plugin::Destruction;
use super::sim::{DestructibleId, DestructiblePose};
use crate::world::PhysicsWorld;

/// A destructible placed in a level: the asset and the instance seed. Saved in levels and prefabs;
/// [`define_destructible_sources`] turns it into a [`Destruction`] instance at the entity's pose
/// (translation and rotation; scale is ignored, as for every destructible).
///
/// # Example
///
/// ```
/// use regia_physics::destruction::DestructibleSource;
///
/// let wall = DestructibleSource::new("assets/wall.destructible.ron");
/// assert_eq!(wall.seed, 0);
/// ```
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct DestructibleSource {
    /// A manifest path (`assets/wall.glb#fracture/destructible`), an asset id as text, or a
    /// `.destructible.ron` relative to the project root. The editor's asset filter takes the two
    /// path forms (an id is typed).
    #[reflect(@regia_ecs::attrs::AssetFilter("*.destructible.ron, *#fracture/destructible"))]
    pub asset: String,
    /// The instance seed (event seeds derive from it).
    pub seed: u64,
}

impl DestructibleSource {
    /// The asset at `asset`, seed 0.
    #[must_use]
    pub fn new(asset: impl Into<String>) -> Self {
        Self {
            asset: asset.into(),
            seed: 0,
        }
    }
}

/// What [`define_destructible_sources`] did with a [`DestructibleSource`] (inserted once per
/// entity; never saved).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum DestructibleSourceStatus {
    /// Placed as this instance.
    Defined(DestructibleId),
    /// Not placed.
    Failed {
        /// Stable lowercase code: `destructible_source_unknown` (no such asset or file),
        /// `destructible_source_read` (it could not be read) or a `destruction_*` code of the
        /// asset or the define.
        code: String,
        /// What went wrong.
        message: String,
    },
}

/// Reads the destructible asset `asset` names: a manifest entry first (a path or an asset id),
/// else a `.destructible.ron` file under the server's project root.
///
/// # Errors
/// A stable code (see [`DestructibleSourceStatus::Failed`]) and a message.
pub fn load_destructible_source(
    server: &mut AssetServer,
    asset: &str,
) -> Result<DestructibleAsset, (String, String)> {
    let path = asset.replace('\\', "/");
    let id = path.parse::<AssetId>().ok().or_else(|| server.id_for_path(&path));
    if let Some(id) = id {
        return DestructibleAsset::load(server, id).map_err(|e| (e.code().to_string(), e.to_string()));
    }
    let rel = std::path::Path::new(&path);
    if path.is_empty()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err((
            "destructible_source_unknown".to_string(),
            format!("`{path}` is neither a manifest entry nor a plain project path"),
        ));
    }
    let file = server.project_root().join(rel);
    if !file.is_file() {
        return Err((
            "destructible_source_unknown".to_string(),
            format!("no manifest entry or file for {path}"),
        ));
    }
    let text = std::fs::read_to_string(&file).map_err(|e| {
        (
            "destructible_source_read".to_string(),
            format!("{}: {e}", file.display()),
        )
    })?;
    DestructibleAsset::from_ron(&text).map_err(|e| (e.code().to_string(), e.to_string()))
}

type Pending = (Without<DestructibleSourceStatus>, With<DestructibleSource>);

/// Places every new [`DestructibleSource`] (`PreUpdate`, [`super::DestructionPlugin`]). Without an
/// [`AssetServer`] it waits; an entity that fails gets a [`DestructibleSourceStatus::Failed`] and
/// is not retried (remove the status to retry).
pub fn define_destructible_sources(
    pending: Query<(Entity, &DestructibleSource, Option<&Transform>), Pending>,
    server: Option<ResMut<AssetServer>>,
    mut destruction: ResMut<Destruction>,
    mut physics: Option<ResMut<PhysicsWorld>>,
    mut commands: Commands,
) {
    let Some(mut server) = server else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let _span = tracing::info_span!("define_destructible_sources").entered();
    let mut list: Vec<_> = pending.iter().collect();
    list.sort_by_key(|(entity, ..)| *entity);
    for (entity, source, transform) in list {
        let pose = transform.map_or_else(DestructiblePose::default, |t| DestructiblePose {
            translation: t.translation.to_array(),
            rotation: t.rotation.to_array(),
        });
        let defined = load_destructible_source(&mut server, &source.asset).and_then(|asset| {
            let asset = Arc::new(asset);
            let d = &mut *destruction;
            let result = match physics.as_deref_mut() {
                Some(physics) => d.define(physics, asset, pose, source.seed),
                None => d.world.define(asset, pose, source.seed),
            };
            result.map_err(|e| (e.code().to_string(), e.to_string()))
        });
        let status = match defined {
            Ok(id) => DestructibleSourceStatus::Defined(id),
            Err((code, message)) => {
                tracing::warn!(target: "regia_physics", asset = %source.asset, code = %code, "destructible source: {message}");
                DestructibleSourceStatus::Failed { code, message }
            }
        };
        commands.entity(entity).try_insert(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destruction::{BoxGrid, DestructionMaterial, DestructionPlugin, GridSupport};
    use regia_core::math::Vec3;

    fn app(root: &std::path::Path, server: bool) -> App {
        let mut app = App::new();
        app.add_plugins((crate::PhysicsPlugin, DestructionPlugin::default()));
        if server {
            let server = AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.to_path_buf(),
                platform: regia_asset::current_platform(),
                content_dir: None,
            })
            .expect("asset server");
            app.insert_resource(server);
        }
        app
    }

    fn write_grid(root: &std::path::Path) {
        let asset = DestructibleAsset::box_grid(&BoxGrid {
            name: "grid".to_string(),
            counts: [2, 2, 1],
            cell: [0.5, 0.5, 0.5],
            density: 2000.0,
            strength_per_area: 4000.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid");
        std::fs::create_dir_all(root.join("assets")).expect("dir");
        std::fs::write(
            root.join("assets/grid.destructible.ron"),
            asset.to_ron().expect("ron"),
        )
        .expect("write");
    }

    fn status(app: &App, entity: Entity) -> Option<DestructibleSourceStatus> {
        app.world().get::<DestructibleSourceStatus>(entity).cloned()
    }

    #[test]
    fn destruction_source_is_defined_at_the_entity_pose() {
        let root = regia_test_utils::unique_temp_dir("destructible-source");
        write_grid(&root);
        let mut app = app(&root, true);
        let placed = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
                DestructibleSource::new("assets/grid.destructible.ron"),
            ))
            .id();
        let missing = app
            .world_mut()
            .spawn(DestructibleSource::new("assets/none.destructible.ron"))
            .id();
        let outside = app
            .world_mut()
            .spawn(DestructibleSource::new("../grid.destructible.ron"))
            .id();
        app.update();
        let Some(DestructibleSourceStatus::Defined(id)) = status(&app, placed) else {
            panic!("defined: {:?}", status(&app, placed));
        };
        let d = app.world().resource::<Destruction>();
        let instance = d.world.instance(id).expect("instance");
        assert_eq!(instance.pose().translation, [1.0, 2.0, 3.0]);
        assert_eq!(instance.asset().chunks.len(), 4);
        for entity in [missing, outside] {
            assert!(
                matches!(
                    status(&app, entity),
                    Some(DestructibleSourceStatus::Failed { ref code, .. }) if code == "destructible_source_unknown"
                ),
                "{:?}",
                status(&app, entity)
            );
        }
        // Each entity is placed once.
        app.update();
        assert_eq!(app.world().resource::<Destruction>().world.instances().count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn destruction_source_waits_for_an_asset_server() {
        let root = regia_test_utils::unique_temp_dir("destructible-source-wait");
        let mut app = app(&root, false);
        let entity = app
            .world_mut()
            .spawn(DestructibleSource::new("assets/grid.destructible.ron"))
            .id();
        app.update();
        assert_eq!(status(&app, entity), None);
        assert_eq!(app.world().resource::<Destruction>().world.instances().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn destruction_source_reports_a_bad_asset_with_its_code() {
        let root = regia_test_utils::unique_temp_dir("destructible-source-bad");
        std::fs::create_dir_all(root.join("assets")).expect("dir");
        std::fs::write(root.join("assets/bad.destructible.ron"), "not ron").expect("write");
        let mut app = app(&root, true);
        let entity = app
            .world_mut()
            .spawn(DestructibleSource::new("assets/bad.destructible.ron"))
            .id();
        app.update();
        let Some(DestructibleSourceStatus::Failed { code, .. }) = status(&app, entity) else {
            panic!("failed: {:?}", status(&app, entity));
        };
        assert!(code.starts_with("destruction_"), "{code}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// rev-claude-59 inspector-forms finding 6: the asset filter names both path forms the doc
    /// lists, as suffix globs (`*.destructible.ron` and the manifest's `*#fracture/destructible`).
    #[test]
    fn the_asset_filter_names_both_path_forms() {
        use regia_ecs::reflect::{TypeInfo, Typed};
        let TypeInfo::Struct(info) = DestructibleSource::type_info() else {
            panic!("a struct");
        };
        let field = info.field("asset").expect("asset field");
        let filter = regia_ecs::attrs::EditorAttrs::from_custom(field.custom_attributes())
            .asset_filter
            .expect("tagged");
        let globs: Vec<&str> = filter.split([',', ';']).map(str::trim).collect();
        for path in [
            "assets/wall.destructible.ron",
            "assets/wall.glb#fracture/destructible",
        ] {
            assert!(
                globs
                    .iter()
                    .any(|g| g.strip_prefix('*').is_some_and(|suffix| path.ends_with(suffix))),
                "{filter} takes {path}"
            );
        }
    }
}
