// SPDX-License-Identifier: MPL-2.0
//! Transforms, hierarchy, visibility, levels, prefabs and scene serialization.
//!
//! Specification: `docs/specs/14-scene-and-framework.md` Part A (A1–A4) and Part C where scene
//! work needs it. Format reference: `docs/formats/scene-files.md` (current version 13,
//! [`format::SCENE_FORMAT_VERSION`]).
//!
//! - [`transform`]: local `Transform`, derived `GlobalTransform` / `PreviousGlobalTransform`.
//! - [`components`]: `Name`, `EntityGuid`, `Tags`, `Layers`, `Static`, visibility components.
//! - [`hierarchy`]: `set_parent` / `remove_parent` / keep-world-transform reparenting.
//! - [`propagate`]: the dirty-subtree propagation system.
//! - the `format` module: the `.level.ron` / `.prefab.ron` file format (current version 13) and its migrators.
//! - [`spawn`]: loading a scene file into the world and snapshotting a world back.
//! - [`prefab`]: prefab templates, instancing, overrides, nested prefabs.
//! - [`level`]: `LevelManager`, `LevelSettings`, level load/unload lifecycle messages.
//! - [`kind`]: entity kinds (a declared label and contract per game type), derived roles, the kind
//!   lint and the project map (spec 101).
//! - [`cooked`]: the cooked postcard form (string table + hashed type paths) for fast loads.
//! - [`spline`], [`shape`]: the general spline and region-volume components (spec 64 § 2).
//! - [`stream_grid`], [`stream_plan`], [`hlod`], [`floating_origin`]: world streaming and HLOD data
//!   model (a cell grid, load/unload planning, the HLOD tree, large-world coordinates); pure logic.
//!
//! - [`streaming`]: world streaming authoring types, messages, readiness gates and the frame's system
//!   sets, shared by the `regia_world` runtime and every subsystem that hooks into it.
//!
//! Add [`ScenePlugin`] to an app to register the types, resources, messages and systems.
//!
//! # How the pieces fit
//!
//! The authored value is a local [`Transform`]; the [`propagate`] system derives
//! [`GlobalTransform`] (and [`PreviousGlobalTransform`]) for it every frame in
//! `PostUpdate/TransformPropagate`, and render/physics/audio read the global form. On top of that
//! sits the scene file: [`format`](mod@format) defines the frozen `.level.ron` / `.prefab.ron`
//! shape, [`spawn`] materialises it into entities, and [`level`] ([`level::LevelManager`]) drives
//! the load/unload lifecycle — messages ([`level::LevelLoading`], [`level::LevelLoaded`],
//! [`level::LevelUnloading`]) plus the `OnLevelLoaded`/`OnLevelUnloading` schedules. [`prefab`]
//! reuses subtrees with overrides and nested instances; [`cooked`] is the fast postcard form used
//! for shipping builds.
//!
//! See `book/user-guide/scenes.md` (levels and prefabs) and `book/agents/scene.md` (the agent
//! surface).
//!
//! # Agent surface (`docs/specs/23-agent-native.md`)
//! The agent protocol lane owns `regia_agent::dispatch`; the calls it can forward here are:
//! `regia_agent::dispatch` does not register `scene.spawn_prefab` or `level.load` today (switching
//! levels is not an agent method; see the scenes user guide). The Rust entry points an embedder
//! can call are [`prefab::PrefabWorldExt::spawn_prefab`] (returns the instance root; a
//! not-yet-loaded template makes the instance pending) and [`level::open_level`]; both operate
//! synchronously on the world.

pub mod bake_query;
pub mod components;
pub mod cooked;
pub mod floating_origin;
pub mod format;
pub mod hierarchy;
pub mod hlod;
// Entity kinds, derived roles, the kind lint and the project map (spec 101, ADR-2488).
pub mod kind;
pub mod layers;
pub mod level;
pub mod level_dir;
pub mod placed;
pub mod prefab;
pub mod prefab_variant;
pub mod propagate;
pub mod shape;
pub mod spawn;
pub mod spline;
pub mod stream_grid;
pub mod stream_plan;
pub mod streaming;
mod text_split;
pub mod transform;
pub mod weather;
// Deleting a layer from a definition asset: `layer_in_use` (spec 37 § 4.4, ADR-7203).
pub mod layer_delete;
// The unique-name marker and its runtime lookup by name (ADR-7772).
pub mod unique_name;
// A layer stored in its own file next to the level (`<level>.<layer>.layer.ron`, ADR-7792).
pub mod layer_file;
// Level variants: a level as a base level plus differences (ADR-7823).
pub mod level_variant;

// Root paths for the transform types: several crates (framework, samples, editor) use
// `regia_scene::Transform` directly and predate the module split.
pub use crate::transform::{GlobalTransform, PreviousGlobalTransform, Transform};

use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet, PreUpdate};

/// Everything a game normally needs from the scene crate.
pub mod prelude {
    pub use crate::ScenePlugin;
    pub use crate::components::{
        EntityGuid, FeedAttach, FeedOwned, Generated, GiMobility, InheritedVisibility, Layers,
        LightmapSurface, MapLayer, Name, Static, Tags, ViewVisibility, Visibility,
    };
    pub use crate::format::{
        PrefabInstanceData, PrefabOverride, SCENE_FORMAT_VERSION, SceneEntity, SceneFile, SceneFormatError,
    };
    pub use crate::hierarchy::HierarchyCommands;
    pub use crate::kind::{EntityKind, KindAppExt, KindMeta, KindRegistry, KindTag};
    pub use crate::level::{
        Level, LevelId, LevelLibrary, LevelLoaded, LevelLoading, LevelManager, LevelMember, LevelSettings,
        LevelUnloading, LoadMode, Persistent,
    };
    pub use crate::prefab::{
        PendingPrefab, PrefabCommands, PrefabInstance, PrefabLibrary, PrefabLink, PrefabWorldExt,
    };
    pub use crate::propagate::propagate_transforms;
    pub use crate::spawn::{SpawnOptions, snapshot_scene, spawn_scene};
    pub use crate::transform::{GlobalTransform, PreviousGlobalTransform, Transform};
}

/// Scene, hierarchy and level lifecycle: types, resources, messages and the propagation systems.
#[derive(Default)]
pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        // Types used by scene data: primitives (component fields, override values), scene
        // components, then the plugin's own resources and messages.
        {
            let registry = app.world().resource::<regia_ecs::reflect::AppTypeRegistry>();
            let mut registry = registry.write();
            components::register_primitives(&mut registry);
        }
        app.register_type::<transform::Transform>()
            .register_type::<transform::GlobalTransform>()
            .register_type::<transform::PreviousGlobalTransform>()
            .register_type::<components::Name>()
            .register_type::<components::EntityGuid>()
            .register_type::<components::Tags>()
            .register_type::<components::EditorFolder>()
            .register_type::<unique_name::UniqueName>()
            .register_type::<components::Layers>()
            .register_type::<components::MapLayer>()
            .register_type::<components::VisCell>()
            .register_type::<components::FeedAttach>()
            .register_type::<components::Static>()
            .register_type::<components::GiMobility>()
            .register_type::<components::LightmapSurface>()
            .register_type::<components::Visibility>()
            .register_type::<components::InheritedVisibility>()
            .register_type::<components::ViewVisibility>()
            .register_type::<prefab::PrefabInstance>()
            .register_type::<prefab::PrefabLink>()
            .register_type::<prefab::PendingPrefab>()
            .register_type::<level::LevelSettings>()
            .register_type::<level::LevelMember>()
            .register_type::<level::Persistent>()
            .register_type::<layers::LayerMembership>()
            .register_type::<layers::LevelLayers>()
            .register_type::<layers::LayerEntry>()
            .register_type::<layers::LayerState>()
            .register_type::<layers::LayerPeers>()
            .register_type::<layers::LayerKind>()
            .register_type::<layers::LayerDef>()
            .register_type::<layers::LayerDefs>()
            .register_type::<Vec<layers::LayerDef>>()
            .register_type::<Vec<layers::LayerDefs>>()
            .register_type::<(f32, f32, f32)>()
            .register_type::<Option<String>>()
            .register_type::<Vec<layers::LayerEntry>>()
            .register_type::<placed::PlacedLevel>()
            .register_type::<placed::PlacedMode>()
            .register_type::<placed::PlacedFilter>()
            .register_type::<placed::PlacedLayerRule>()
            .register_type::<Vec<placed::PlacedLayerRule>>()
            .register_type::<Vec<String>>()
            .register_type::<spline::Spline>()
            .register_type::<spline::SplinePoint>()
            .register_type::<spline::SplineUp>()
            .register_type::<spline::TangentMode>()
            .register_type::<spline::SplineAttrDecl>()
            .register_type::<spline::SplineAttrType>()
            .register_type::<spline::SplineValue>()
            .register_type::<shape::Shape>()
            .register_type::<shape::ShapeKind>()
            .register_type::<weather::SceneWind>()
            .register_type::<weather::SceneWindMode>()
            .register_type::<weather::SurfaceWeather>()
            .register_type::<weather::SurfaceWeatherMode>()
            .register_type::<weather::WeatherAir>();
        kind::add_to_app(app);

        app.init_resource::<prefab::PrefabLibrary>()
            .init_resource::<level::LevelLibrary>()
            .init_resource::<level::LevelManager>()
            .init_resource::<propagate::PropagationCache>()
            .init_resource::<layers::LayerTable>()
            .init_resource::<layers::LayerDefsLibrary>()
            .init_resource::<placed::PlacedRegistry>()
            .insert_resource(spawn::SceneResourceHooks::with_engine_hooks());

        app.add_message::<level::LevelLoading>()
            .add_message::<level::LevelLoaded>()
            .add_message::<level::LevelUnloading>()
            .add_message::<layers::LayerChanged>()
            .add_message::<placed::PlacedLevelLoaded>();
        streaming::add_to_app(app);

        // Level lifecycle first (it may spawn prefabs), then fill pending prefab instances.
        app.add_systems(
            PreUpdate,
            (
                level::level_lifecycle,
                placed::spawn_placed_levels,
                prefab::resolve_pending_prefabs,
            )
                .chain(),
        );
        app.add_systems(
            PostUpdate,
            propagate::propagate_transforms.in_set(PostUpdateSet::TransformPropagate),
        );
        // Layer switches apply at one fixed point, before transforms propagate (spec 37 § 4.2).
        app.configure_sets(
            PostUpdate,
            layers::LayerSwitch.before(PostUpdateSet::TransformPropagate),
        );
        app.add_systems(
            PostUpdate,
            (
                layers::apply_layer_switches,
                layers::follow_parked_parents.run_if(layers::any_parked),
            )
                .chain()
                .in_set(layers::LayerSwitch),
        );
    }
}
