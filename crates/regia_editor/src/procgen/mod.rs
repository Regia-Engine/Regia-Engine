// SPDX-License-Identifier: MPL-2.0
//! The procedural graph editor and placement tools (`docs/specs/67-procedural-graph-editor.md`):
//! a node canvas for `.pgraph.ron` files next to the viewport, live preview of a bound generator,
//! per-node visualization and data tables, and viewport tools (Paint, Box) that write the inputs
//! graphs read.
//!
//! Layout follows the anim graph editor: the open document is a world resource
//! ([`doc::ProcGraphDoc`]) that the panel ([`panel`]), the viewport overlays ([`debug_view`]) and
//! the agent protocol ([`agent`], `editor.procgen_*`) share; every document edit is one
//! [`doc::ProcGraphCommand`] on the editor's shared [`crate::command::UndoStack`]. Agent
//! `procgen.graph_edit` calls on the open graph land in the document too (through
//! [`regia_agent::ProcGraphDocRoute`]), so `editor.undo` reverts them. An open variant refuses
//! graph edits on the variant path and on the parent path with `procgen_read_only` before any
//! load or save.
//!
//! Desktop only; nothing here is linked into games (the runtime parts live in `regia_procgen`).

pub mod agent;
mod assets;
pub(crate) mod autosave;
pub mod badges;
pub mod biome_overlay;
pub mod debug_view;
pub mod doc;
pub mod inspect;
pub mod inspector;
pub mod panel;
pub mod params_tab;
pub mod plant_preview;
pub mod presets;
pub mod tools;
pub mod undo;
pub mod view;
pub mod widgets;

use regia_ecs::prelude::*;
use regia_procgen::ProcGraph;

pub use debug_view::{ProcEditorPrefs, ProcPreview};
pub use doc::{DocError, ProcGraphCommand, ProcGraphDoc};
pub use inspect::ProcInspect;
pub use tools::{HitFilter, ProcToolSession, ToolKind};

/// Ticks between checks of the open file for outside changes (hot reload).
const RELOAD_CHECK_TICKS: u32 = 30;

/// Tick counter for the hot-reload poll.
#[derive(Resource, Default)]
struct ReloadPoll(u32);

/// Installs the procedural editor's resources and routes agent graph edits of the open document
/// into it. Idempotent.
pub fn install(world: &mut World) {
    world.init_resource::<ProcGraphDoc>();
    world.init_resource::<ProcPreview>();
    world.init_resource::<ProcInspect>();
    world.init_resource::<ProcToolSession>();
    world.init_resource::<ReloadPoll>();
    world.init_resource::<regia_agent::SplineEditLog>();
    world.init_resource::<regia_procgen::terrain_edits::ProcTerrainEditLog>();
    if !world.contains_resource::<ProcEditorPrefs>() {
        world.init_resource::<ProcEditorPrefs>();
    }
    world.insert_resource(regia_agent::ProcGraphDocRoute {
        open: route_open,
        commit: route_commit,
    });
    if let Some(mut settings) = world.get_resource_mut::<regia_procgen::world::ProcGenSettings>() {
        settings.editor = true;
    }
}

/// [`install`] unless the document resource already exists (agent calls on a bare world).
pub(crate) fn install_if_missing(world: &mut World) {
    if !world.contains_resource::<ProcGraphDoc>() {
        install(world);
    }
}

/// Whether two project-relative graph paths name the same file: compared by their segments
/// (`./g.pgraph.ron`, `sub//g.pgraph.ron` and `sub\g.pgraph.ron` are the spellings the agent's
/// path check accepts), case-folded where the host filesystem folds (ADR-4824).
fn same_graph_path(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (
        regia_core::project_path::check_relative(a),
        regia_core::project_path::check_relative(b),
    ) {
        (Ok(x), Ok(y)) => crate::path_norm::strip_prefix_fold(&x, &y, crate::path_norm::FOLD_CASE)
            .is_some_and(|rest| rest.as_os_str().is_empty()),
        _ => false,
    }
}

fn route_open(world: &World, path: &str) -> Result<Option<ProcGraph>, regia_agent::ProcGraphRouteDeny> {
    let Some(doc) = world.get_resource::<ProcGraphDoc>() else {
        return Ok(None);
    };
    if doc.read_only() {
        let same = |other: &str| same_graph_path(other, path);
        let variant_path = doc.path.as_deref().is_some_and(same)
            || doc.variant.as_ref().is_some_and(|view| same(&view.path));
        // The parent, and every file further up the chain (the graph shown is the chain's end).
        let parent_path = doc
            .variant
            .as_ref()
            .is_some_and(|view| same(&view.variant.parent))
            || doc.variant_files.iter().any(|file| same(file));
        if variant_path || parent_path {
            return Err(regia_agent::ProcGraphRouteDeny {
                reason: "procgen_read_only",
                message: "a variant is open: the parent graph is read-only (edit the parameter overrides)",
            });
        }
        return Ok(None);
    }
    Ok((doc
        .path
        .as_deref()
        .is_some_and(|open| same_graph_path(open, path)))
    .then(|| doc.graph.clone()))
}

fn route_commit(world: &mut World, _path: &str, graph: ProcGraph, save: bool) -> Result<bool, String> {
    if world
        .get_resource::<ProcGraphDoc>()
        .is_some_and(ProcGraphDoc::read_only)
    {
        return Err(
            "procgen_read_only: a variant is open: the parent graph is read-only (edit the parameter overrides)"
                .to_string(),
        );
    }
    doc::edit(world, "agent graph edit", None, move |ctx| {
        *ctx.graph = graph;
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    world
        .get_resource_or_insert_with(crate::command::UndoStack::new)
        .seal();
    if save {
        doc::save(world).map_err(|e| e.to_string())?;
    }
    Ok(save)
}

/// Keeps a directory graph source rooted at the open project (the project browser can open
/// another project after startup).
fn follow_project_root(world: &mut World) {
    let Some(root) = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
    else {
        return;
    };
    if let Some(mut settings) = world.get_resource_mut::<regia_procgen::world::ProcGenSettings>()
        && let regia_procgen::world::ProcGraphSource::Dir(dir) = &settings.source
        && *dir != root
    {
        settings.source = regia_procgen::world::ProcGraphSource::Dir(root);
    }
}

/// One editor frame of the procedural editor: the hot-reload poll, the preview's quiet period and
/// visualization, and the tool session's re-runs.
#[tracing::instrument(target = "regia_editor", skip_all, name = "procgen_editor_tick")]
pub fn tick(world: &mut World) {
    view::tick(world);
    tools::sync_hit_filter_prefs(world);
    if !world.contains_resource::<ProcGraphDoc>() {
        return;
    }
    let poll = {
        let mut p = world.get_resource_or_insert_with(ReloadPoll::default);
        p.0 += 1;
        p.0.is_multiple_of(RELOAD_CHECK_TICKS)
    };
    if poll {
        follow_project_root(world);
        doc::check_external(world);
    }
    undo::drain(world);
    assets::follow_level(world);
    assets::fill_libraries(world);
    debug_view::follow_default_target(world);
    debug_view::tick(world);
    tools::tick(world);
    plant_preview::tick(world);
}

#[cfg(test)]
mod route_tests {
    use super::*;

    fn world_with_variant(files: &[&str]) -> World {
        let mut world = World::new();
        let mut doc = ProcGraphDoc {
            path: Some(files[0].to_string()),
            variant: Some(doc::VariantView {
                path: files[0].to_string(),
                variant: regia_procgen::ProcGraphVariant {
                    format_version: 1,
                    parent: files[1].to_string(),
                    overrides: std::collections::BTreeMap::new(),
                },
            }),
            ..ProcGraphDoc::default()
        };
        doc.variant_files = files.iter().map(|f| (*f).to_string()).collect();
        world.insert_resource(doc);
        world
    }

    fn denied(world: &World, path: &str) -> bool {
        matches!(route_open(world, path), Err(deny) if deny.reason == "procgen_read_only")
    }

    #[test]
    fn an_open_variant_refuses_other_spellings_of_its_files() {
        // The agent's path check accepts these spellings and the write normalises them, so an
        // exact string compare let a `./` path through to overwrite the read-only parent.
        let world = world_with_variant(&["v.pgvar.ron", "sub/g.pgraph.ron"]);
        for path in [
            "sub/g.pgraph.ron",
            "./sub/g.pgraph.ron",
            "sub//g.pgraph.ron",
            "sub\\g.pgraph.ron",
            "./v.pgvar.ron",
        ] {
            assert!(denied(&world, path), "{path} names a read-only file");
        }
        assert!(!denied(&world, "sub/other.pgraph.ron"));
        assert!(matches!(route_open(&world, "sub/other.pgraph.ron"), Ok(None)));
    }

    #[test]
    fn an_open_variant_chain_refuses_the_graph_at_its_end() {
        // v2 -> v1 -> g: the document shows g, so g is read-only too, not only v2 and v1.
        let world = world_with_variant(&["v2.pgvar.ron", "v1.pgvar.ron", "g.pgraph.ron"]);
        for path in ["v2.pgvar.ron", "v1.pgvar.ron", "g.pgraph.ron", "./g.pgraph.ron"] {
            assert!(denied(&world, path), "{path}");
        }
    }

    #[test]
    fn an_open_graph_routes_other_spellings_of_its_path() {
        let mut world = World::new();
        world.insert_resource(ProcGraphDoc {
            path: Some("sub/g.pgraph.ron".into()),
            ..ProcGraphDoc::default()
        });
        assert!(matches!(route_open(&world, "./sub/g.pgraph.ron"), Ok(Some(_))));
        assert!(matches!(route_open(&world, "sub/h.pgraph.ron"), Ok(None)));
        let fold = same_graph_path("Sub/G.pgraph.ron", "sub/g.pgraph.ron");
        assert_eq!(
            fold,
            crate::path_norm::FOLD_CASE,
            "case folds only where the filesystem does"
        );
    }
}
