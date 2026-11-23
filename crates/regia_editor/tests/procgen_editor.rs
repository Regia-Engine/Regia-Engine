// SPDX-License-Identifier: MPL-2.0
//! The procedural graph editor (`docs/specs/67-procedural-graph-editor.md` § 7): document edits
//! through the shared undo stack, collapse/expand, agent graph edits landing in the document, hot
//! reload, the debounced preview, the data table, visualization caps, placement tools, the hit
//! filter and painted layers. Headless: no GPU, no wall clock (editor ticks are counted).

use std::collections::{BTreeMap, BTreeSet};

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{UndoStack, redo_world, undo_world};
use regia_editor::procgen::debug_view::{self, ColorBy, ProcPreview, VisForm, VisualizeStyle};
use regia_editor::procgen::doc::{self, ProcGraphDoc};
use regia_editor::procgen::inspect::{self, ProcInspect};
use regia_editor::procgen::panel::{self, PanelAction};
use regia_editor::procgen::tools::{self, Brush, HitFilter, ToolGesture, ToolKind};
use regia_procgen::graph::ParamDecl;
use regia_procgen::painted::PaintedPoints;
use regia_procgen::world::{
    ProcGenLog, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
};
use regia_procgen::{AttrType, Edge, NodeEntry, ProcGraph, ProcNodeRegistry, SettingValue};
use regia_scene::components::{EntityGuid, Generated, Name};
use regia_scene::prelude::{Transform, snapshot_scene};
use regia_scene::spawn::SnapshotFilter;
use serde_json::{Value, json};

fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    n.at = (at, 0.0);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

/// in.self → sample.region → points.jitter → out.points, with a `spacing` parameter.
fn grid_graph() -> ProcGraph {
    let mut g = ProcGraph::default();
    g.params.push(ParamDecl {
        name: "spacing".into(),
        ty: AttrType::Float,
        default: SettingValue::Float(4.0),
        min: Some(0.5),
        max: Some(64.0),
        doc: String::new(),
    });
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes.insert(
        2,
        node("sample.region", 200.0, &[("spacing_m", "Bind(\"spacing\")")]),
    );
    g.nodes.insert(
        3,
        node(
            "points.jitter",
            400.0,
            &[("offset", "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]")],
        ),
    );
    g.nodes.insert(4, node("out.points", 600.0, &[]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
    ];
    g
}

fn tool_graph(tool: &str) -> String {
    let mut g = ProcGraph::default();
    g.flags.tool = Some(tool.to_string());
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("items.by_tag", 200.0, &[("tags", "[\"painted\"]")]));
    g.nodes.insert(
        3,
        node(
            "filter.range",
            400.0,
            &[("attr", "\"layer\""), ("min", "2.0"), ("max", "2.0")],
        ),
    );
    g.nodes
        .insert(4, node("out.points", 600.0, &[("name", "\"layer2\"")]));
    g.edges = vec![
        Edge::new(1, "out", 2, "in"),
        Edge::new(2, "inside", 3, "in"),
        Edge::new(3, "inside", 4, "in"),
    ];
    g.to_ron()
}

fn files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("g.pgraph.ron".to_string(), grid_graph().to_ron()),
        ("tool_painted_props.pgraph.ron".to_string(), tool_graph("paint")),
        ("tool_rubble_box.pgraph.ron".to_string(), {
            let mut g = grid_graph();
            g.flags.tool = Some("box".into());
            g.to_ron()
        }),
    ])
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>();
    regia_editor::procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    app
}

/// One editor frame: the procedural editor tick, then the app update (generator lifecycle).
fn frame(app: &mut App) {
    regia_editor::procgen::tick(app.world_mut());
    app.update();
}

fn generator(app: &mut App, graph: &str) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new(graph)
            },
            EntityGuid(Guid(0x00ab_cdef_0123_4567_89ab_cdef_0123_4567)),
            Transform::IDENTITY,
        ))
        .id()
}

fn run_to_ready(app: &mut App, e: Entity) -> [u8; 32] {
    regia_procgen::world::request_run(app.world_mut(), e);
    for _ in 0..8 {
        app.update();
        if let Some(st) = app.world().get::<ProcGenState>(e)
            && st.state == regia_procgen::world::ProcGenRunState::Ready
        {
            return st.output_hash.expect("hash");
        }
    }
    let st = app.world().get::<ProcGenState>(e).expect("state");
    panic!("not ready: {:?} {:?}", st.state, st.last_error);
}

fn doc_text(app: &App) -> String {
    app.world().resource::<ProcGraphDoc>().graph.to_ron()
}

fn registry() -> ProcNodeRegistry {
    ProcNodeRegistry::with_builtins()
}

// ---- Test 1 and 2: every edit applies, undoes and redoes byte-identically ----

#[test]
fn every_edit_round_trips_through_the_undo_stack_and_slider_drags_coalesce() {
    let mut app = app();
    let w = app.world_mut();
    doc::open(w, "g.pgraph.ron").expect("open");
    let mut states = vec![doc_text(&app)];
    let w = app.world_mut();
    let added = doc::edit(w, "add", None, |c| {
        doc::add_node(c, "points.keep_random", (700.0, 0.0))
    })
    .expect("add");
    states.push(doc_text(&app));
    let w = app.world_mut();
    doc::edit(w, "disconnect", None, |c| {
        doc::disconnect(c.graph, (3, "out"), (4, "in"));
        Ok(())
    })
    .expect("disconnect");
    states.push(doc_text(&app));
    let w = app.world_mut();
    doc::edit(w, "connect", None, |c| doc::connect(c, (3, "out"), (added, "in"))).expect("connect");
    states.push(doc_text(&app));
    // A drag: three moves, one entry.
    for x in [710.0, 720.0, 730.0] {
        doc::edit(app.world_mut(), "move", Some(format!("move:{added}")), |c| {
            doc::move_node(c.graph, added, (x, 5.0))
        })
        .expect("move");
    }
    states.push(doc_text(&app));
    app.world_mut().resource_mut::<UndoStack>().seal();
    // A slider scrub: three values, one entry.
    for r in [0.2, 0.3, 0.4] {
        doc::edit(
            app.world_mut(),
            "set ratio",
            Some(format!("set:{added}:ratio")),
            |c| doc::set_setting(c, added, "ratio", SettingValue::Float(r)),
        )
        .expect("set");
    }
    states.push(doc_text(&app));
    app.world_mut().resource_mut::<UndoStack>().seal();
    // Invalid values are not written.
    let bad = doc::edit(app.world_mut(), "set ratio", None, |c| {
        doc::set_setting(c, added, "ratio", SettingValue::Float(7.0))
    })
    .expect_err("out of range");
    assert_eq!(bad.code, "setting_out_of_range", "{bad}");
    let w = app.world_mut();
    doc::edit(w, "bypass", None, |c| doc::set_enabled(c.graph, 3, false)).expect("bypass");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "add param", None, |c| {
        doc::add_param(
            c.graph,
            ParamDecl {
                name: "ratio".into(),
                ty: AttrType::Float,
                default: SettingValue::Float(0.5),
                min: None,
                max: None,
                doc: String::new(),
            },
        )
    })
    .expect("param");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "bind", None, |c| {
        doc::bind_setting(c, added, "ratio", "ratio", None)
    })
    .expect("bind");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "unbind", None, |c| {
        doc::unbind_setting(c.graph, added, "ratio")
    })
    .expect("unbind");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "remove param", None, |c| {
        doc::remove_param(c.graph, "ratio")
    })
    .expect("remove");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "note", None, |c| {
        Ok(doc::add_note(c.graph, "rocks here", (0.0, 200.0)))
    })
    .expect("note");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "reroute", None, |c| {
        Ok(doc::add_reroute(c.graph, (500.0, 100.0)))
    })
    .expect("reroute");
    states.push(doc_text(&app));
    let frag = doc::copy_fragment(
        &app.world().resource::<ProcGraphDoc>().graph,
        &BTreeSet::from([2, 3]),
    );
    doc::edit(app.world_mut(), "paste", None, |c| {
        doc::paste_fragment(c.graph, &frag, (40.0, 40.0)).map(|_| ())
    })
    .expect("paste");
    states.push(doc_text(&app));
    doc::edit(app.world_mut(), "remove", None, |c| {
        doc::remove_nodes(c.graph, &BTreeSet::from([added]));
        Ok(())
    })
    .expect("remove nodes");
    states.push(doc_text(&app));

    let entries = states.len() - 1;
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        entries,
        "one entry per edit"
    );
    for k in (0..entries).rev() {
        assert!(undo_world(app.world_mut()));
        assert_eq!(doc_text(&app), states[k], "undo to state {k}");
    }
    for (k, expected) in states.iter().enumerate().skip(1) {
        assert!(redo_world(app.world_mut()));
        assert_eq!(doc_text(&app), *expected, "redo to state {k}");
    }
}

#[test]
fn renaming_a_parameter_updates_every_bind_in_one_entry() {
    let mut app = app();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    doc::edit(app.world_mut(), "bind jitter", None, |c| {
        doc::set_setting(
            c,
            3,
            "offset",
            SettingValue::parse("[(0.0, 0.0, 0.0), (0.0, 0.0, 0.0)]").expect("ron"),
        )
    })
    .expect("set");
    let depth = app.world().resource::<UndoStack>().undo_depth();
    let n = doc::edit(app.world_mut(), "rename", None, |c| {
        doc::rename_param(c.graph, "spacing", "cell")
    })
    .expect("rename");
    assert_eq!(n, 1);
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    let d = app.world().resource::<ProcGraphDoc>();
    assert_eq!(d.graph.nodes[&2].settings["spacing_m"].as_bind(), Some("cell"));
    assert!(d.errors.is_empty(), "{:?}", d.errors);
    assert!(undo_world(app.world_mut()));
    let d = app.world().resource::<ProcGraphDoc>();
    assert_eq!(d.graph.nodes[&2].settings["spacing_m"].as_bind(), Some("spacing"));
}

// ---- Test 3: collapse and expand ----

#[test]
fn collapse_keeps_the_output_hash_and_expand_restores_the_graph_hash() {
    let mut app = app();
    let e = generator(&mut app, "g.pgraph.ron");
    let before = run_to_ready(&mut app, e);
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    let original = app
        .world()
        .resource::<ProcGraphDoc>()
        .graph
        .graph_hash(&registry());
    let call = doc::collapse_selection(app.world_mut(), &BTreeSet::from([2, 3]), "sub.pgraph.ron")
        .expect("collapse");
    assert_eq!(call, 2, "the call node takes the smallest selected id");
    let d = app.world().resource::<ProcGraphDoc>();
    assert!(d.errors.is_empty(), "collapsed graph compiles: {:?}", d.errors);
    assert_eq!(d.graph.nodes[&2].ty, "graph.call");
    let after = run_to_ready(&mut app, e);
    assert_eq!(
        after, before,
        "the generator's output hash is unchanged by collapse"
    );

    let nodes = doc::expand_call(app.world_mut(), call, false).expect("expand");
    assert_eq!(nodes, vec![2, 3]);
    let d = app.world().resource::<ProcGraphDoc>();
    assert_eq!(
        d.graph.graph_hash(&registry()),
        original,
        "expand gives the original graph hash"
    );
    // A graph called from another file is not expanded without copy.
    let shared = grid_graph();
    let mut caller = ProcGraph::default();
    caller
        .nodes
        .insert(1, node("graph.call", 0.0, &[("graph", "\"sub.pgraph.ron\"")]));
    doc::write_source(app.world_mut(), "other.pgraph.ron", &caller.to_ron()).expect("write");
    let _ = shared;
    let again = doc::collapse_selection(app.world_mut(), &BTreeSet::from([2, 3]), "sub2.pgraph.ron")
        .expect("collapse");
    let mut caller2 = ProcGraph::default();
    caller2
        .nodes
        .insert(1, node("graph.call", 0.0, &[("graph", "\"sub2.pgraph.ron\"")]));
    doc::write_source(app.world_mut(), "other2.pgraph.ron", &caller2.to_ron()).expect("write");
    let refused = doc::expand_call(app.world_mut(), again, false).expect_err("shared");
    assert_eq!(refused.code, "procgen_expand_shared");
    doc::expand_call(app.world_mut(), again, true).expect("copy expands");
    // Invalid collapse selections.
    let bad = doc::collapse_selection(app.world_mut(), &BTreeSet::new(), "x.pgraph.ron").expect_err("empty");
    assert_eq!(bad.code, "procgen_collapse_invalid");
}

// ---- Test 5: hot reload ----

#[test]
fn outside_changes_reload_when_clean_and_ask_when_dirty() {
    let mut app = app();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    let mut changed = grid_graph();
    changed.description = "edited outside".into();
    doc::write_source(app.world_mut(), "g.pgraph.ron", &changed.to_ron()).expect("write");
    assert!(doc::check_external(app.world_mut()), "clean document reloads");
    assert_eq!(
        app.world().resource::<ProcGraphDoc>().graph.description,
        "edited outside"
    );

    doc::edit(app.world_mut(), "move", None, |c| {
        doc::move_node(c.graph, 1, (5.0, 5.0))
    })
    .expect("edit");
    let mut theirs = grid_graph();
    theirs.description = "theirs".into();
    doc::write_source(app.world_mut(), "g.pgraph.ron", &theirs.to_ron()).expect("write");
    assert!(
        !doc::check_external(app.world_mut()),
        "dirty document does not reload"
    );
    let d = app.world().resource::<ProcGraphDoc>();
    assert!(
        d.conflict.is_some(),
        "the keep-mine / take-theirs choice is pending"
    );
    assert_eq!(d.graph.nodes[&1].at, (5.0, 5.0), "edits kept");
    let on_disk = doc::source_text(app.world(), "g.pgraph.ron");
    assert_eq!(
        on_disk.as_deref(),
        Some(theirs.to_ron().as_str()),
        "the file was not overwritten"
    );
    doc::resolve_conflict(app.world_mut(), true).expect("take theirs");
    assert_eq!(app.world().resource::<ProcGraphDoc>().graph.description, "theirs");
}

#[test]
fn a_variant_shows_its_parent_read_only_and_edits_overrides() {
    let mut app = app();
    let variant = regia_procgen::ProcGraphVariant {
        format_version: 1,
        parent: "g.pgraph.ron".into(),
        overrides: BTreeMap::new(),
    };
    doc::write_source(app.world_mut(), "g_dense.pgvar.ron", &variant.to_ron()).expect("write");
    doc::open(app.world_mut(), "g_dense.pgvar.ron").expect("open variant");
    let d = app.world().resource::<ProcGraphDoc>();
    assert!(d.read_only() && !d.dirty());
    assert_eq!(d.graph.nodes.len(), 4, "the parent graph is shown");
    let refused = doc::edit(app.world_mut(), "move", None, |c| {
        doc::move_node(c.graph, 1, (9.0, 9.0))
    })
    .expect_err("ro");
    assert_eq!(refused.code, "procgen_read_only");
    doc::edit(app.world_mut(), "override", None, |c| {
        doc::set_param_value(c, "spacing", Some(SettingValue::Float(2.0)))
    })
    .expect("override");
    let d = app.world().resource::<ProcGraphDoc>();
    assert!(d.dirty());
    assert_eq!(
        d.variant
            .as_ref()
            .and_then(|v| v.variant.overrides.get("spacing")),
        Some(&SettingValue::Float(2.0))
    );
    doc::save(app.world_mut()).expect("save");
    assert!(!app.world().resource::<ProcGraphDoc>().dirty());
    let saved = doc::source_text(app.world(), "g_dense.pgvar.ron").expect("saved");
    assert!(saved.contains("spacing"), "{saved}");
    doc::edit(app.world_mut(), "reset", None, |c| {
        doc::set_param_value(c, "spacing", None)
    })
    .expect("reset");
    assert!(
        app.world()
            .resource::<ProcGraphDoc>()
            .variant
            .as_ref()
            .is_some_and(|v| v.variant.overrides.is_empty())
    );
    assert!(undo_world(app.world_mut()));
    assert!(
        !app.world().resource::<ProcGraphDoc>().dirty(),
        "undo returns to the saved override"
    );
}

#[test]
fn graph_edit_on_an_open_variant_refuses_the_variant_and_its_parent() {
    let mut a = Agent::new();
    let variant = regia_procgen::ProcGraphVariant {
        format_version: 1,
        parent: "g.pgraph.ron".into(),
        overrides: BTreeMap::new(),
    };
    doc::write_source(a.app.world_mut(), "g_dense.pgvar.ron", &variant.to_ron()).expect("write");
    let parent_before = doc::source_text(a.app.world(), "g.pgraph.ron").expect("parent");
    let variant_before = doc::source_text(a.app.world(), "g_dense.pgvar.ron").expect("variant");
    a.call("editor.procgen_open", json!({ "path": "g_dense.pgvar.ron" }));
    let description = a.app.world().resource::<ProcGraphDoc>().graph.description.clone();
    let undo_before = a.app.world().resource::<UndoStack>().undo_depth();
    let overlay_parent = a
        .app
        .world()
        .resource::<regia_procgen::world::ProcGraphOverlay>()
        .0
        .get("g.pgraph.ron")
        .cloned();
    let overlay_variant = a
        .app
        .world()
        .resource::<regia_procgen::world::ProcGraphOverlay>()
        .0
        .get("g_dense.pgvar.ron")
        .cloned();

    let on_variant = a
        .try_call(
            "procgen.graph_edit",
            json!({ "path": "g_dense.pgvar.ron", "ops": [{ "op": "set_description", "text": "nope" }], "save": true }),
        )
        .expect_err("variant path");
    assert_eq!(reason(&on_variant), "procgen_read_only");
    let on_parent = a
        .try_call(
            "procgen.graph_edit",
            json!({ "path": "g.pgraph.ron", "ops": [{ "op": "set_description", "text": "nope" }], "save": true }),
        )
        .expect_err("parent path");
    assert_eq!(reason(&on_parent), "procgen_read_only");

    assert_eq!(
        doc::source_text(a.app.world(), "g.pgraph.ron").as_deref(),
        Some(parent_before.as_str())
    );
    assert_eq!(
        doc::source_text(a.app.world(), "g_dense.pgvar.ron").as_deref(),
        Some(variant_before.as_str())
    );
    assert_eq!(
        a.app.world().resource::<ProcGraphDoc>().graph.description,
        description
    );
    assert_eq!(a.app.world().resource::<UndoStack>().undo_depth(), undo_before);
    assert_eq!(
        a.app
            .world()
            .resource::<regia_procgen::world::ProcGraphOverlay>()
            .0
            .get("g.pgraph.ron")
            .cloned(),
        overlay_parent
    );
    assert_eq!(
        a.app
            .world()
            .resource::<regia_procgen::world::ProcGraphOverlay>()
            .0
            .get("g_dense.pgvar.ron")
            .cloned(),
        overlay_variant
    );
}

// ---- Test 6: preview debounce ----

#[test]
fn preview_reruns_once_after_the_quiet_period_and_cancels_an_in_flight_run() {
    let mut app = app();
    let e = generator(&mut app, "g.pgraph.ron");
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    debug_view::set_preview(app.world_mut(), Some(e)).expect("preview");
    let quiet = app
        .world()
        .resource::<debug_view::ProcEditorPrefs>()
        .refresh_ticks();
    assert_eq!(quiet, 9, "150 ms at 60 ticks per second");
    for _ in 0..quiet + 3 {
        frame(&mut app);
    }
    let runs0 = app.world().resource::<ProcPreview>().runs;
    assert_eq!(runs0, 1, "binding runs the target once");
    let started = |app: &App| {
        app.world()
            .resource::<ProcGenLog>()
            .0
            .iter()
            .filter(|(x, what, _)| *x == e && *what == "started")
            .count()
    };
    let started0 = started(&app);

    // One change: one run after the quiet period, none before.
    doc::edit(app.world_mut(), "jitter", Some("s".into()), |c| {
        doc::set_setting(
            c,
            3,
            "offset",
            SettingValue::parse("[(-2.0, 0.0, -2.0), (2.0, 0.0, 2.0)]").expect("ron"),
        )
    })
    .expect("edit");
    for _ in 0..quiet - 1 {
        frame(&mut app);
    }
    assert_eq!(
        app.world().resource::<ProcPreview>().runs,
        runs0,
        "nothing during the quiet period"
    );
    for _ in 0..4 {
        frame(&mut app);
    }
    assert_eq!(app.world().resource::<ProcPreview>().runs, runs0 + 1);

    // Three quick changes: one run.
    for x in [1.0, 2.0, 3.0] {
        doc::edit(app.world_mut(), "move", None, |c| {
            doc::move_node(c.graph, 1, (x, 0.0))
        })
        .expect("move");
        doc::edit(app.world_mut(), "jitter", None, |c| {
            doc::set_setting(
                c,
                3,
                "offset",
                SettingValue::parse(&format!("[(-{x}, 0.0, -{x}), ({x}, 0.0, {x})]")).expect("ron"),
            )
        })
        .expect("edit");
        frame(&mut app);
    }
    for _ in 0..quiet + 3 {
        frame(&mut app);
    }
    assert_eq!(
        app.world().resource::<ProcPreview>().runs,
        runs0 + 2,
        "debounced to one run"
    );

    // A change while a run is queued cancels it.
    regia_procgen::world::request_run(app.world_mut(), e);
    let cancelled = app.world().resource::<ProcPreview>().cancelled;
    doc::edit(app.world_mut(), "jitter", None, |c| {
        doc::set_setting(
            c,
            3,
            "offset",
            SettingValue::parse("[(0.0, 0.0, 0.0), (0.5, 0.0, 0.5)]").expect("ron"),
        )
    })
    .expect("edit");
    assert_eq!(
        app.world().resource::<ProcPreview>().cancelled,
        cancelled + 1,
        "in-flight run cancelled"
    );
    for _ in 0..quiet + 3 {
        frame(&mut app);
    }
    assert!(started(&app) > started0);
    // Invalid graphs do not run; the last good output stays.
    let runs = app.world().resource::<ProcPreview>().runs;
    let hash = app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    doc::edit(app.world_mut(), "break", None, |c| {
        doc::disconnect(c.graph, (1, "out"), (2, "region"));
        Ok(())
    })
    .expect("edit");
    assert!(!app.world().resource::<ProcGraphDoc>().errors.is_empty());
    for _ in 0..quiet + 3 {
        frame(&mut app);
    }
    assert_eq!(
        app.world().resource::<ProcPreview>().runs,
        runs,
        "invalid graph does not run"
    );
    assert_eq!(
        app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash),
        hash
    );
}

fn previewed(app: &mut App) -> Entity {
    let e = generator(app, "g.pgraph.ron");
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    debug_view::set_preview(app.world_mut(), Some(e)).expect("preview");
    for _ in 0..14 {
        frame(app);
    }
    assert!(
        app.world()
            .get::<ProcGenState>(e)
            .and_then(|s| s.debug.as_ref())
            .is_some(),
        "debug kept"
    );
    e
}

// ---- Test 7: inspect ----

#[test]
fn the_data_table_shows_rows_sorts_stably_and_selects_points() {
    let mut app = app();
    previewed(&mut app);
    *app.world_mut().resource_mut::<ProcInspect>() = ProcInspect {
        node: Some(3),
        ..ProcInspect::default()
    };
    let t = inspect::build_table(app.world()).expect("table");
    assert_eq!(t.kind, "Points");
    assert_eq!(t.rows.len(), 16, "4 m cells over a 16 m square");
    assert!(t.columns.iter().any(|c| c == "density"));
    // Every density is 1: a stable sort keeps element order.
    app.world_mut().resource_mut::<ProcInspect>().sort = Some(("density".into(), false));
    let sorted = inspect::build_table(app.world()).expect("table");
    assert_eq!(sorted.order, (0..16).collect::<Vec<_>>(), "stable for equal keys");
    app.world_mut().resource_mut::<ProcInspect>().sort = Some(("position.x".into(), true));
    let by_x = inspect::build_table(app.world()).expect("table");
    let xs: Vec<f64> = by_x
        .order
        .iter()
        .map(|e| match &by_x.rows[*e][1] {
            inspect::Cell::Num(v) => *v,
            inspect::Cell::Text(_) => f64::NAN,
        })
        .collect();
    assert!(xs.windows(2).all(|w| w[0] >= w[1]), "descending x");
    // Clicking row 0 selects its element; with no sort, selection index equals row index.
    app.world_mut().resource_mut::<ProcInspect>().sort = None;
    for row in [0, 5, 15] {
        let element = inspect::select_row(app.world_mut(), row).expect("select");
        assert_eq!(element, row);
    }
    // A viewport click near a point selects its row.
    let t = inspect::build_table(app.world()).expect("table");
    let pos = |e: usize| match (&t.rows[e][1], &t.rows[e][2], &t.rows[e][3]) {
        (inspect::Cell::Num(x), inspect::Cell::Num(y), inspect::Cell::Num(z)) => {
            [*x as f32, *y as f32, *z as f32]
        }
        _ => [0.0; 3],
    };
    let p = pos(7);
    let row = inspect::select_at(app.world_mut(), [p[0] + 0.05, p[1], p[2]], 0.5).expect("pick");
    assert_eq!(row, 7);
    // Filters.
    app.world_mut().resource_mut::<ProcInspect>().filter = Some("position.x > 0".into());
    let f = inspect::build_table(app.world()).expect("filtered");
    assert!(!f.order.is_empty() && f.order.len() < 16);
    app.world_mut().resource_mut::<ProcInspect>().filter = Some("nope > 0".into());
    assert_eq!(
        inspect::build_table(app.world()).expect_err("column").code,
        "procgen_unknown_column"
    );
}

// ---- Test 8: visualize ----

#[test]
fn visualized_nodes_register_their_draws_under_the_cap() {
    let mut app = app();
    previewed(&mut app);
    let style = VisualizeStyle {
        color_by: ColorBy::Density,
        form: VisForm::Points,
        color: [1.0; 4],
    };
    let ids = debug_view::visualize(app.world_mut(), 3, Some(style));
    assert_eq!(ids, vec![3]);
    let p = app.world().resource::<ProcPreview>();
    assert_eq!(p.drawn.len(), 16, "one draw per visible point");
    assert_eq!(p.total_points, 16);
    assert!(!p.subsampled);
    // Beyond the cap: a uniform subsample of exactly the cap, spread over the input.
    let all: Vec<usize> = (0..450_000).collect();
    let sub = debug_view::subsample(all, debug_view::MAX_DRAWN_POINTS);
    assert_eq!(sub.len(), debug_view::MAX_DRAWN_POINTS);
    assert_eq!(sub[0], 0);
    assert!(
        sub.windows(2).all(|w| w[1] > w[0] && w[1] - w[0] <= 3),
        "even spread"
    );
    // Quiet ticks reuse the drawn lists; a new run rebuilds them once.
    let rebuilds = app.world().resource::<ProcPreview>().rebuilds;
    for _ in 0..5 {
        frame(&mut app);
    }
    assert_eq!(
        app.world().resource::<ProcPreview>().rebuilds,
        rebuilds,
        "no rebuild without a change"
    );
    doc::edit(app.world_mut(), "jitter", None, |c| {
        doc::set_setting(
            c,
            3,
            "offset",
            SettingValue::parse("[(-2.0, 0.0, -2.0), (2.0, 0.0, 2.0)]").expect("ron"),
        )
    })
    .expect("edit");
    for _ in 0..14 {
        frame(&mut app);
    }
    let after = app.world().resource::<ProcPreview>().rebuilds;
    assert!(
        after > rebuilds && after <= rebuilds + 5,
        "rebuilt for the edit and the new runs only: {rebuilds} → {after}"
    );
    debug_view::visualize(app.world_mut(), 3, None);
    assert!(app.world().resource::<ProcPreview>().drawn.is_empty());
}

#[test]
fn the_debug_cap_drops_the_oldest_nodes_first() {
    use regia_procgen::{Bundle, DataItem, PointSet};
    let mut big = PointSet::new();
    for i in 0..1000 {
        big.push(regia_procgen::Point {
            position: [i as f32, 0.0, 0.0],
            ..regia_procgen::Point::default()
        });
    }
    let debug: regia_procgen::world::DebugOutputs = (1..=4)
        .map(|id| (id, vec![Bundle::one(DataItem::points(big.clone()))]))
        .collect();
    let one = {
        let (_, dropped) = debug_view::cap_debug(&debug, &[1, 2, 3, 4], usize::MAX);
        assert!(dropped.is_empty());
        let (kept, _) = debug_view::cap_debug(&debug, &[1, 2, 3, 4], 0);
        assert!(kept.is_empty());
        let (kept, dropped) = debug_view::cap_debug(&debug, &[4, 3, 2, 1], 0);
        assert_eq!(dropped, vec![4, 3, 2, 1]);
        kept.len()
    };
    assert_eq!(one, 0);
    // A cap that fits two nodes keeps the two evaluated last.
    let (kept_all, _) = debug_view::cap_debug(&debug, &[1, 2, 3, 4], usize::MAX);
    assert_eq!(kept_all.len(), 4);
    let per_node = 1000 * 92;
    let (kept, dropped) = debug_view::cap_debug(&debug, &[1, 2, 3, 4], 2 * per_node);
    assert_eq!(dropped, vec![1, 2]);
    assert_eq!(kept.keys().copied().collect::<Vec<_>>(), vec![3, 4]);
}

#[test]
fn isolate_runs_up_to_a_node_without_replacing_the_output() {
    let mut app = app();
    let e = previewed(&mut app);
    let hash = app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    let iso = debug_view::isolate(app.world_mut(), Some(2))
        .expect("isolate")
        .expect("on");
    assert!(iso.error.is_none(), "{:?}", iso.error);
    assert_eq!(
        iso.debug.keys().copied().collect::<Vec<_>>(),
        vec![1, 2],
        "the node and its ancestors"
    );
    assert_eq!(
        app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash),
        hash,
        "output kept"
    );
    assert_eq!(
        app.world().resource::<ProcPreview>().drawn.len(),
        16,
        "the isolated node is visualized"
    );
    assert!(debug_view::isolate(app.world_mut(), None).expect("off").is_none());
}

// ---- Tests 9 to 11: tools ----

fn ground(app: &mut App) -> Entity {
    use std::sync::Arc;
    let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    let material = Arc::new(regia_render::LabMaterial::default());
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            regia_render::MeshRenderer::new(mesh, material),
            Name::new("ground"),
            EntityGuid(Guid(77)),
        ))
        .id()
}

fn scene_text(app: &mut App) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let mut file = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.entities.sort_by_key(|e| e.guid);
    file.to_ron_string().expect("ron")
}

#[test]
fn paint_and_box_tools_apply_undo_and_cancel() {
    let mut app = app();
    ground(&mut app);
    app.update();
    let before = scene_text(&mut app);
    let brush = Brush {
        radius_m: 1.0,
        spacing_m: 1.0,
        layer: 2,
        ..Brush::default()
    };
    let out = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_painted_props.pgraph.ron",
        None,
        &ToolGesture {
            points: vec![Vec3::new(0.0, 5.0, 0.0)],
            brush: Some(brush),
            ..ToolGesture::default()
        },
        false,
    )
    .expect("paint");
    assert!(out.created && !out.applied);
    assert_eq!(
        out.added, 5,
        "a 1 m disc on a 1 m grid: center and four neighbors"
    );
    let painted = app
        .world()
        .get::<PaintedPoints>(out.entity)
        .expect("painted")
        .clone();
    assert!(
        painted
            .points
            .iter()
            .all(|p| p.layer == 2 && p.position.y.abs() < 1e-4)
    );
    assert_eq!(
        app.world()
            .get::<Name>(out.entity)
            .map(|n| n.as_str().to_string())
            .as_deref(),
        Some("tool_painted_props_01")
    );
    // Cancel leaves the level byte-identical.
    assert!(tools::finish(app.world_mut(), false).expect("cancel"));
    app.update();
    assert_eq!(scene_text(&mut app), before, "cancel restores the level");

    // Apply: one entry; undo removes the entity; redo brings it back.
    let depth = app.world().resource::<UndoStack>().undo_depth();
    let out = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_painted_props.pgraph.ron",
        None,
        &ToolGesture {
            points: vec![Vec3::new(0.0, 5.0, 0.0), Vec3::new(4.0, 5.0, 0.0)],
            brush: Some(brush),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("paint");
    assert_eq!(out.painted, 10);
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    let guid = app.world().get::<EntityGuid>(out.entity).expect("guid").0;
    // The graph selects exactly the layer-2 points (zero based: digit 3).
    let hash = run_to_ready(&mut app, out.entity);
    let _ = hash;
    let output = app
        .world()
        .get::<regia_procgen::world::ProcOutput>(out.entity)
        .expect("output");
    assert_eq!(
        output.0.points.get("layer2").map(regia_procgen::PointSet::len),
        Some(10)
    );
    // Erase under the brush (second stroke's points).
    let erased = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_painted_props.pgraph.ron",
        Some(out.entity),
        &ToolGesture {
            points: vec![Vec3::new(4.0, 5.0, 0.0)],
            brush: Some(Brush { erase: true, ..brush }),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("erase");
    assert_eq!((erased.created, erased.removed, erased.painted), (false, 5, 5));
    assert!(undo_world(app.world_mut()), "undo erase");
    assert_eq!(
        app.world()
            .get::<PaintedPoints>(out.entity)
            .map(|p| p.points.len()),
        Some(10)
    );
    assert!(undo_world(app.world_mut()), "undo paint");
    let alive = app
        .world_mut()
        .query::<&EntityGuid>()
        .iter(app.world())
        .any(|g| g.0 == guid);
    assert!(!alive, "undo removes the created entity");
    assert!(redo_world(app.world_mut()));
    let painted_again = app
        .world_mut()
        .query::<(&EntityGuid, &PaintedPoints)>()
        .iter(app.world())
        .find(|(g, _)| g.0 == guid)
        .map(|(_, p)| p.points.len());
    assert_eq!(painted_again, Some(10), "redo brings it back");

    // Box: the generator's bounds match the given min/max.
    let b = tools::run_tool(
        app.world_mut(),
        ToolKind::Box,
        "tool_rubble_box.pgraph.ron",
        None,
        &ToolGesture {
            box_extents: Some((Vec3::new(-2.0, 0.0, 1.0), Vec3::new(4.0, 3.0, 5.0))),
            ..ToolGesture::default()
        },
        true,
    )
    .expect("box");
    app.update();
    let shape = regia_procgen::world::generator_bounds(app.world(), b.entity);
    let (lo, hi) = shape.world_aabb();
    for (got, want) in lo.iter().chain(hi.iter()).zip([-2.0, 0.0, 1.0, 4.0, 3.0, 5.0]) {
        assert!((got - want).abs() < 1e-4, "box {lo:?} {hi:?}");
    }
    // A graph that is not a path tool graph is refused for Path.
    let path = tools::run_tool(
        app.world_mut(),
        ToolKind::Path,
        "g.pgraph.ron",
        None,
        &ToolGesture::default(),
        true,
    )
    .expect_err("path");
    assert_eq!(path.code, "procgen_tool_preset_mismatch");
    // A preset of another kind is refused.
    let wrong = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_rubble_box.pgraph.ron",
        None,
        &ToolGesture::default(),
        true,
    )
    .expect_err("kind");
    assert_eq!(wrong.code, "procgen_tool_preset_mismatch");
}

#[test]
fn unfit_targets_and_misses_are_reported() {
    let mut app = app();
    ground(&mut app);
    let plain = app
        .world_mut()
        .spawn((Name::new("plain"), Transform::IDENTITY))
        .id();
    app.world_mut()
        .resource_mut::<debug_view::ProcEditorPrefs>()
        .edit_selected_only = true;
    let unfit = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_painted_props.pgraph.ron",
        Some(plain),
        &ToolGesture::default(),
        true,
    )
    .expect_err("unfit");
    assert_eq!(unfit.code, "procgen_tool_target_unfit");
    app.world_mut()
        .resource_mut::<debug_view::ProcEditorPrefs>()
        .edit_selected_only = false;
    let miss = tools::run_tool(
        app.world_mut(),
        ToolKind::Paint,
        "tool_painted_props.pgraph.ron",
        None,
        &ToolGesture {
            points: vec![Vec3::new(500.0, 5.0, 500.0)],
            brush: Some(Brush::default()),
            ..ToolGesture::default()
        },
        false,
    )
    .expect_err("miss");
    assert_eq!(miss.code, "procgen_tool_no_hit");
    tools::finish(app.world_mut(), false).expect("cancel");
}

#[test]
fn the_hit_filter_skips_generated_entities() {
    use std::sync::Arc;
    let mut app = app();
    ground(&mut app);
    let (vertices, indices) = regia_render::box_mesh([1.0, 0.25, 1.0]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
        regia_render::MeshRenderer::new(mesh, Arc::new(regia_render::LabMaterial::default())),
        Generated {
            generator: Guid(1),
            run_id: 1,
        },
    ));
    app.update();
    let on = HitFilter::default();
    let hit = tools::project_down(app.world(), Vec3::new(0.0, 10.0, 0.0), &on).expect("hit");
    assert!(hit.position.y.abs() < 1e-4, "skips the generated box: {hit:?}");
    let off = HitFilter {
        skip_generated: false,
        ..HitFilter::default()
    };
    let hit = tools::project_down(app.world(), Vec3::new(0.0, 10.0, 0.0), &off).expect("hit");
    assert!(
        (hit.position.y - 2.25).abs() < 1e-4,
        "hits the generated box: {hit:?}"
    );
}

// ---- Test 4 and the acceptance flow through the real dispatcher ----

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    /// The project directory (removed on drop).
    dir: std::path::PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Agent {
    fn new() -> Self {
        let dir = regia_test_utils::unique_temp_dir("procgen-editor");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(files()),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<regia_editor::selection::Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = regia_editor::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        permissions.config.heavy_rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
            dir,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        regia_editor::procgen::tick(self.app.world_mut());
        self.app.update();
        let response = answer.try_recv().expect("answered within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(v) => v,
            Err(e) => panic!("{method} {params}: {e:?}"),
        }
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            regia_editor::procgen::tick(self.app.world_mut());
            self.app.update();
        }
    }
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[test]
fn agent_graph_edits_on_the_open_document_land_in_its_undo_stack() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let edit = a.call(
        "procgen.graph_edit",
        json!({ "path": "g.pgraph.ron", "ops": [{ "op": "move", "id": 1, "at": [33.0, 44.0] }] }),
    );
    assert_eq!(edit["document"], true);
    let world = a.app.world();
    assert_eq!(world.resource::<ProcGraphDoc>().graph.nodes[&1].at, (33.0, 44.0));
    assert_eq!(
        world.resource::<UndoStack>().next_undo().as_deref(),
        Some("Procgen: agent graph edit")
    );
    a.call("editor.undo", json!({}));
    assert_eq!(
        a.app.world().resource::<ProcGraphDoc>().graph.nodes[&1].at,
        (0.0, 0.0),
        "one undo reverts it"
    );
    // save: true saves the panel's document.
    a.call(
        "procgen.graph_edit",
        json!({ "path": "g.pgraph.ron", "ops": [{ "op": "set_description", "text": "saved" }], "save": true }),
    );
    let state = a.call("editor.procgen_state", json!({}));
    assert_eq!(state["dirty"], false);
    // Errors carry reasons.
    let none = a
        .try_call("editor.procgen_new", json!({ "path": "g.pgraph.ron" }))
        .expect_err("exists");
    assert_eq!(reason(&none), "procgen_file_exists");
    let bad = a
        .try_call("editor.procgen_preview", json!({ "generator": 12345 }))
        .expect_err("no generator");
    assert_eq!(reason(&bad), "procgen_not_generator");
}

#[test]
fn acceptance_build_preview_visualize_inspect_and_paint_through_the_agent() {
    let mut a = Agent::new();
    a.call("editor.procgen_new", json!({ "path": "rocks.pgraph.ron" }));
    // Blank: in.self (1) → sample.region (2) → out.points (3). Add a jitter between 2 and 3.
    let edit = a.call(
        "procgen.graph_edit",
        json!({ "path": "rocks.pgraph.ron", "ops": [
            { "op": "add_node", "ty": "points.jitter", "at": [400.0, 0.0],
              "settings": { "offset": { "ron": "[(-1.0, 0.0, -1.0), (1.0, 0.0, 1.0)]" } } },
            { "op": "disconnect", "from": [2, "out"], "to": [3, "in"] },
            { "op": "connect", "from": [2, "out"], "to": [4, "in"] },
            { "op": "connect", "from": [4, "out"], "to": [3, "in"] },
            { "op": "set_setting", "id": 2, "name": "spacing_m", "value": 4.0 },
        ] }),
    );
    assert_eq!(edit["document"], true);
    let e = a
        .app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new("rocks.pgraph.ron")
            },
            EntityGuid(Guid(5)),
            Transform::IDENTITY,
        ))
        .id();
    a.call("editor.procgen_preview", json!({ "generator": e.to_bits() }));
    a.frames(14);
    let state = a.call("editor.procgen_state", json!({}));
    assert_eq!(state["preview"]["state"], "ready", "{state}");
    let vis = a.call("editor.procgen_visualize", json!({ "node": 4, "on": true }));
    assert_eq!(vis["drawn_points"], 16);
    let table = a.call(
        "editor.procgen_table",
        json!({ "node": 4, "sort": "density", "select_row": 3 }),
    );
    assert_eq!(table["total"], 16);
    assert_eq!(table["selected"], 3);
    let iso = a.call("editor.procgen_isolate", json!({ "node": 2 }));
    assert_eq!(iso["state"], "isolated");
    let coll = a.call(
        "editor.procgen_collapse",
        json!({ "nodes": [2, 4], "path": "rocks_core.pgraph.ron" }),
    );
    assert_eq!(coll["call_node"], 2);
    let exp = a.call("editor.procgen_expand", json!({ "node": 2 }));
    assert_eq!(exp["nodes"], json!([2, 4]));
    // A box tool session through the agent, then cancel.
    let tool = a.call(
        "editor.procgen_tool",
        json!({ "tool": "box", "preset": "tool_rubble_box.pgraph.ron", "box": { "min": [0, 0, 0], "max": [2, 2, 2] }, "apply": false }),
    );
    assert_eq!(tool["created"], true);
    let cancel = a.call("editor.procgen_tool_cancel", json!({}));
    assert_eq!(cancel["cancelled"], true);
    let again = a.call("editor.procgen_tool_cancel", json!({}));
    assert_eq!(again["cancelled"], false);
    let path = a
        .try_call(
            "editor.procgen_tool",
            json!({ "tool": "path", "preset": "g.pgraph.ron", "apply": true }),
        )
        .expect_err("path");
    assert_eq!(reason(&path), "procgen_tool_preset_mismatch");
}

#[test]
fn the_palette_filters_by_search_and_by_a_dragged_output_pin() {
    let mut app = app();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    let all = doc::palette(&registry(), "", None);
    assert!(all.len() >= 90, "every registered type: {}", all.len());
    let jitter = doc::palette(&registry(), "JITTER", None);
    assert!(jitter.iter().any(|e| e.ty == "points.jitter"));
    // From sample.region's Points output: only types with an input accepting points.
    let kinds = doc::output_kinds(app.world(), 2, "out").expect("kinds");
    let from = doc::palette(&registry(), "", Some(kinds));
    assert!(!from.is_empty() && from.len() < all.len());
    assert!(from.iter().all(|e| e.pin.is_some()));
    assert!(
        from.iter()
            .any(|e| e.ty == "points.jitter" && e.pin.as_deref() == Some("in"))
    );
    assert!(
        !from.iter().any(|e| e.ty == "in.self"),
        "a type with no inputs is not offered"
    );
}

#[test]
fn entering_a_called_graph_keeps_a_breadcrumb_back() {
    let mut app = app();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    let call = doc::collapse_selection(app.world_mut(), &BTreeSet::from([2, 3]), "core.pgraph.ron")
        .expect("collapse");
    let entered = doc::enter_call(app.world_mut(), call).expect("enter");
    assert_eq!(entered, "core.pgraph.ron");
    let d = app.world().resource::<ProcGraphDoc>();
    assert_eq!(d.path.as_deref(), Some("core.pgraph.ron"));
    assert_eq!(d.breadcrumb, vec!["g.pgraph.ron".to_string()]);
    let up = doc::back(app.world_mut()).expect("back");
    assert_eq!(up.as_deref(), Some("g.pgraph.ron"));
    assert!(app.world().resource::<ProcGraphDoc>().breadcrumb.is_empty());
    assert_eq!(doc::back(app.world_mut()).expect("top"), None);
    assert_eq!(
        doc::enter_call(app.world_mut(), 1).expect_err("not a call").code,
        "procgen_not_call"
    );
}

#[test]
fn seed_actions_are_undoable_and_change_the_output() {
    let mut a = Agent::new();
    let e = a
        .app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(9)),
            Transform::IDENTITY,
        ))
        .id();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    a.call(
        "editor.procgen_preview",
        json!({ "generator": e.to_bits(), "tier": "medium" }),
    );
    assert_eq!(
        a.app.world().resource::<ProcGenSettings>().running_tier,
        regia_render::quality::QualityTier::Medium
    );
    a.frames(14);
    let h0 = a.app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    let pinned = a.call("editor.procgen_seed", json!({ "action": "pin" }));
    let seed0 = pinned["seed"].as_u64().expect("seed");
    assert_eq!(pinned["pinned"], seed0);
    let new = a.call("editor.procgen_seed", json!({ "action": "new" }));
    assert_ne!(new["seed"], pinned["seed"]);
    a.frames(4);
    let h1 = a.app.world().get::<ProcGenState>(e).and_then(|s| s.output_hash);
    assert_ne!(h0, h1, "a new seed changes the jittered output");
    a.call("editor.undo", json!({}));
    assert_eq!(
        a.app.world().get::<ProcGenerator>(e).and_then(|g| g.seed),
        Some(u32::try_from(seed0).expect("u32"))
    );
    let bad = a
        .try_call("editor.procgen_seed", json!({ "action": "dance" }))
        .expect_err("bad");
    assert_eq!(reason(&bad), "procgen_bad_seed");
    let palette = a.call("editor.procgen_palette", json!({ "from": [2, "out"] }));
    assert!(palette["count"].as_u64().unwrap_or(0) > 0);
    let entered = a.call("editor.procgen_open", json!({ "back": true }));
    assert_eq!(entered["breadcrumb"], json!([]));
}

/// Spec 67 § 7 test 12: the canvas capture of a graph is produced (non-empty PNG of the asked
/// size). No golden image (lead TODO if one is wanted). Needs a Vulkan adapter (lavapipe works).
#[test]
fn the_canvas_capture_is_a_png_of_the_requested_size() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut app = app();
    previewed(&mut app);
    for view in [
        regia_editor::procgen::panel::CaptureView::Canvas,
        regia_editor::procgen::panel::CaptureView::Table,
        regia_editor::procgen::panel::CaptureView::Report,
    ] {
        let image = regia_editor::procgen::panel::capture_panel(&device, app.world_mut(), view, [800, 480])
            .expect("capture");
        assert_eq!((image.width(), image.height()), (800, 480));
        let first = image.pixels[0];
        assert!(
            image.pixels.iter().any(|p| *p != first),
            "{view:?}: the panel drew something"
        );
        let png = regia_editor::ui_capture::png_bytes(&image).expect("png");
        assert!(png.len() > 1000 && png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}

/// Spec 67 § 9 probe (hand-run): one panel frame over a 300-node graph, drawn headless without a GPU.
/// `cargo test -p regia_editor --release --test procgen_editor -- --ignored --nocapture panel_frame_cost`
#[test]
#[ignore = "perf probe, hand-run"]
fn panel_frame_cost_with_300_nodes() {
    let mut app = app();
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes.insert(2, node("sample.region", 200.0, &[]));
    g.edges.push(Edge::new(1, "out", 2, "region"));
    for i in 3..=300u32 {
        let mut n = node("points.jitter", f64::from(i % 20) * 220.0, &[]);
        n.at.1 = f64::from(i / 20) * 140.0;
        g.nodes.insert(i, n);
        g.edges.push(Edge::new(i - 1, "out", i, "in"));
    }
    doc::write_source(app.world_mut(), "big.pgraph.ron", &g.to_ron()).expect("write");
    doc::open(app.world_mut(), "big.pgraph.ron").expect("open");
    let ctx = egui::Context::default();
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 900.0),
        )),
        ..Default::default()
    };
    let world = app.world_mut();
    // Warm-up frame (snarl build), then timed frames.
    let mut out = ctx.run_ui(input(), |ui| {
        regia_editor::procgen::panel::procgen_graph_panel(ui, world)
    });
    // No renderer here: drop the texture uploads on purpose.
    out.textures_delta.clear();
    let frames = 30;
    let started = std::time::Instant::now();
    for _ in 0..frames {
        let mut out = ctx.run_ui(input(), |ui| {
            regia_editor::procgen::panel::procgen_graph_panel(ui, world)
        });
        out.textures_delta.clear();
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
    println!("procgen panel frame, 300 nodes: {ms:.3} ms (budget 2 ms on the desktop reference)");
}

/// Spec 67 § 4: the generator inspector's override column and "Open graph", through the agent.
#[test]
fn generator_overrides_are_undoable_and_open_graph_binds_the_preview() {
    let mut a = Agent::new();
    let e = a
        .app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(11)),
            Transform::IDENTITY,
        ))
        .id();
    let opened = a.call("editor.procgen_open", json!({ "generator": e.to_bits() }));
    assert_eq!(opened["path"], "g.pgraph.ron");
    a.frames(14);
    let state = a.call("editor.procgen_state", json!({}));
    assert_eq!(state["preview"]["generator"], e.to_bits());
    assert_eq!(state["preview"]["state"], "ready");
    let before = a.call("editor.procgen_table", json!({ "node": 2 }))["total"].clone();
    assert_eq!(before, 16);
    let set = a.call(
        "editor.procgen_param",
        json!({ "generator": e.to_bits(), "name": "spacing", "value": 8.0 }),
    );
    assert_eq!(set["params"][0]["source"], "generator");
    assert_eq!(set["params"][0]["generator"], "8.0");
    a.frames(4);
    let after = a.call("editor.procgen_table", json!({ "node": 2 }))["total"].clone();
    assert_eq!(after, 4, "8 m cells over 16 m");
    a.call("editor.undo", json!({}));
    assert!(
        a.app
            .world()
            .get::<ProcGenerator>(e)
            .is_some_and(|g| g.params.is_empty()),
        "undo removes the override"
    );
    let unknown = a
        .try_call(
            "editor.procgen_param",
            json!({ "generator": e.to_bits(), "name": "nope", "value": 1 }),
        )
        .expect_err("unknown");
    assert_eq!(reason(&unknown), "procgen_unknown_param");
    let rows = regia_editor::procgen::inspector::param_rows(a.app.world(), e).expect("rows");
    assert_eq!(rows[0].effective, ("4.0".to_string(), "default".to_string()));
}

/// Spec 67 § 2.3: the starter picker lists `flags.starter` graphs (and is empty when disabled in
/// preferences); "Make starter" is one undo entry; a new graph copied from a starter is not one.
#[test]
fn the_starter_picker_lists_starters_and_make_starter_is_undoable() {
    let mut a = Agent::new();
    let none = a.call("editor.procgen_starters", json!({}));
    assert_eq!(none["enabled"], true);
    assert_eq!(none["starters"], json!([]));
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let made = a.call("editor.procgen_starters", json!({ "starter": true }));
    assert_eq!(made["starters"], json!([]), "listed once saved");
    a.call(
        "procgen.graph_edit",
        json!({ "path": "g.pgraph.ron", "ops": [], "save": true }),
    );
    let listed = a.call("editor.procgen_starters", json!({}));
    assert_eq!(listed["starters"][0]["path"], "g.pgraph.ron");
    a.call(
        "editor.procgen_new",
        json!({ "path": "copy.pgraph.ron", "starter": "g.pgraph.ron" }),
    );
    let state = a.call("editor.procgen_state", json!({}));
    assert_eq!(state["path"], "copy.pgraph.ron");
    let copy = doc::source_text(a.app.world(), "copy.pgraph.ron").expect("written");
    assert!(ProcGraph::from_ron(&copy).is_ok_and(|g| !g.flags.starter && g.nodes.len() == 4));
    let refused = a
        .try_call(
            "editor.procgen_new",
            json!({ "path": "x.pgraph.ron", "starter": "tool_fence_missing.pgraph.ron" }),
        )
        .expect_err("missing starter");
    assert_eq!(reason(&refused), "procgen_unknown_graph");
    a.app
        .world_mut()
        .resource_mut::<debug_view::ProcEditorPrefs>()
        .starter_picker = false;
    let off = a.call("editor.procgen_starters", json!({}));
    assert_eq!(
        (off["enabled"].clone(), off["starters"].clone()),
        (json!(false), json!([]))
    );
    // Make starter undoes in one step.
    a.call("editor.procgen_open", json!({ "path": "copy.pgraph.ron" }));
    a.call("editor.procgen_starters", json!({ "starter": true }));
    assert!(a.app.world().resource::<ProcGraphDoc>().graph.flags.starter);
    a.call("editor.undo", json!({}));
    assert!(!a.app.world().resource::<ProcGraphDoc>().graph.flags.starter);
}

/// Spec 67 § 2.1 "show as pin": an override pin is shown on request, wired through
/// `procgen.graph_edit`, drawn on the canvas, and hiding it removes its edge in one undo entry.
#[test]
fn override_pins_show_connect_and_hide_with_undo() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let shown = a.call(
        "editor.procgen_pin",
        json!({ "node": 2, "setting": "spacing_m", "show": true }),
    );
    assert_eq!(shown["shown"], json!(["spacing_m"]));
    let pins = regia_editor::procgen::panel::graph_pins(
        a.app.world(),
        &a.app.world().resource::<ProcGraphDoc>().graph,
    );
    assert!(
        pins[&2].input("@spacing_m").is_some(),
        "a shown pin is drawn before it is wired"
    );
    let bad = a
        .try_call(
            "editor.procgen_pin",
            json!({ "node": 2, "setting": "nope", "show": true }),
        )
        .expect_err("unknown");
    assert_eq!(reason(&bad), "unknown_setting");
    a.call(
        "procgen.graph_edit",
        json!({ "path": "g.pgraph.ron", "ops": [
            { "op": "add_node", "ty": "in.table", "settings": { "asset": "cells.table.ron" } },
            { "op": "connect", "from": [5, "out"], "to": [2, "@spacing_m"] },
        ] }),
    );
    let d = a.app.world().resource::<ProcGraphDoc>();
    assert!(d.errors.is_empty(), "an override edge compiles: {:?}", d.errors);
    assert!(
        d.graph
            .edges
            .iter()
            .any(|e| e.to == (2, "@spacing_m".to_string()))
    );
    a.call(
        "editor.procgen_pin",
        json!({ "node": 2, "setting": "spacing_m", "show": false }),
    );
    assert!(
        !a.app
            .world()
            .resource::<ProcGraphDoc>()
            .graph
            .edges
            .iter()
            .any(|e| e.to.1 == "@spacing_m"),
        "hiding removes the edge"
    );
    a.call("editor.undo", json!({}));
    assert!(
        a.app
            .world()
            .resource::<ProcGraphDoc>()
            .graph
            .edges
            .iter()
            .any(|e| e.to.1 == "@spacing_m"),
        "one undo restores it"
    );
}

/// Spec 67 § 9 probe (hand-run): rebuilding the visualization of a 202 500-point node (CPU side of
/// the 200 000-point budget; the GPU side needs the reference card).
/// `cargo test -p regia_editor --test procgen_editor -- --ignored --nocapture visualization_rebuild_cost`
#[test]
#[ignore = "perf probe, hand-run"]
fn visualization_rebuild_cost_200k_points() {
    let mut app = app();
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes
        .insert(2, node("sample.region", 200.0, &[("spacing_m", "1.0")]));
    g.edges.push(Edge::new(1, "out", 2, "region"));
    doc::write_source(app.world_mut(), "dense.pgraph.ron", &g.to_ron()).expect("write");
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(225.0, 2.0, 225.0),
                },
                limits: Some(regia_procgen::world::ProcLimits {
                    points_per_item: Some(1_000_000),
                    total_elements: Some(4_000_000),
                    memory_bytes: Some(1 << 30),
                    prefabs_per_generator: None,
                }),
                ..ProcGenerator::new("dense.pgraph.ron")
            },
            EntityGuid(Guid(3)),
            Transform::IDENTITY,
        ))
        .id();
    doc::open(app.world_mut(), "dense.pgraph.ron").expect("open");
    debug_view::set_preview(app.world_mut(), Some(e)).expect("preview");
    for _ in 0..14 {
        frame(&mut app);
    }
    let style = VisualizeStyle {
        color_by: ColorBy::Density,
        form: VisForm::Points,
        color: [1.0; 4],
    };
    debug_view::visualize(app.world_mut(), 2, Some(style));
    let p = app.world().resource::<ProcPreview>();
    println!(
        "visualized {} of {} points (subsampled: {})",
        p.drawn.len(),
        p.total_points,
        p.subsampled
    );
    let runs = 10;
    let started = std::time::Instant::now();
    for _ in 0..runs {
        debug_view::rebuild_visualization(app.world_mut());
    }
    let ms = started.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
    println!(
        "procgen visualization rebuild, {} points: {ms:.2} ms",
        p_total(&app)
    );
}

fn p_total(app: &App) -> usize {
    app.world().resource::<ProcPreview>().total_points
}

/// `editor.procgen_select`, `editor.procgen_reload` and the parameter checks of
/// `editor.procgen_capture` (its PNG needs the GPU editor tests): selecting a missing node
/// is `procgen_unknown_node` and leaves the selection; a reload without an outside change
/// reloads nothing; an unknown capture panel is `procgen_bad_panel` before any device is used.
#[test]
fn select_reload_and_capture_parameters_through_the_agent() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let sel = a.call("editor.procgen_select", json!({ "nodes": [2] }));
    assert_eq!(sel["selection"], json!([2]));
    let missing = a
        .try_call("editor.procgen_select", json!({ "nodes": [2, 999] }))
        .expect_err("unknown node");
    assert_eq!(reason(&missing), "procgen_unknown_node");
    let state = a.call("editor.procgen_state", json!({}));
    assert_eq!(state["selection"], json!([2]), "{state}");
    let reload = a.call("editor.procgen_reload", json!({}));
    assert_eq!(reload, json!({ "reloaded": false, "conflict": false }));
    let capture = a
        .app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.procgen_capture")
        .map(|(_, handler)| handler)
        .expect("registered");
    let bad = capture(a.app.world_mut(), &json!({ "panel": "timeline" })).expect_err("bad panel");
    assert_eq!(reason(&bad), "procgen_bad_panel");
}

// ---- Path and Area tools (spec 67 § 5, spec 64) ----

const ROAD: &str = "starter/tool_road.pgraph.ron";
const FOREST: &str = "starter/tool_forest_area.pgraph.ron";

fn pts(v: &[[f32; 3]]) -> Vec<Vec3> {
    v.iter().map(|p| Vec3::new(p[0], p[1], p[2])).collect()
}

fn tool_call(
    app: &mut App,
    kind: ToolKind,
    preset: &str,
    target: Option<Entity>,
    g: ToolGesture,
    apply: bool,
) -> tools::ToolOutcome {
    tools::run_tool(app.world_mut(), kind, preset, target, &g, apply).expect("tool call")
}

/// A target with a fixed guid (the run seed derives from it, so two worlds agree) and an empty spline.
fn spline_target(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            EntityGuid(Guid(4242)),
            Name::new("scripted"),
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            regia_scene::spline::Spline::default(),
        ))
        .id()
}

/// The scripted road session: two calls (open, extend) then apply. Returns the entity's output hash
/// and the scene text.
fn road_session(app: &mut App) -> (Entity, [u8; 32], String) {
    ground(app);
    let target = spline_target(app);
    app.update();
    let a = tool_call(
        app,
        ToolKind::Path,
        ROAD,
        Some(target),
        ToolGesture {
            points: pts(&[[-10.0, 5.0, 0.0], [0.0, 5.0, 4.0]]),
            ..ToolGesture::default()
        },
        false,
    );
    assert!(!a.created && !a.applied);
    assert_eq!(a.spline_points, 2);
    let b = tool_call(
        app,
        ToolKind::Path,
        ROAD,
        Some(target),
        ToolGesture {
            points: pts(&[[10.0, 5.0, 0.0]]),
            ..ToolGesture::default()
        },
        true,
    );
    assert!(b.applied);
    assert_eq!((b.entity, b.spline_points), (a.entity, 3));
    let hash = run_to_ready(app, a.entity);
    (a.entity, hash, scene_text(app))
}

#[test]
fn the_path_tool_builds_a_road_deterministically() {
    let mut one = app();
    let (e, hash, scene) = road_session(&mut one);
    let w = one.world();
    let spline = w.get::<regia_scene::spline::Spline>(e).expect("spline");
    assert_eq!(spline.points.len(), 3);
    assert!(!spline.closed);
    // Local space of the (fixed) target.
    let tf = w.get::<Transform>(e).expect("tf");
    let world_x: Vec<f32> = spline
        .points
        .iter()
        .map(|p| p.position.x + tf.translation.x)
        .collect();
    assert!(
        world_x
            .iter()
            .zip([-10.0, 0.0, 10.0])
            .all(|(a, b)| (a - b).abs() < 1e-3),
        "{world_x:?}"
    );
    assert!(
        spline.points.iter().all(|p| p.position.y.abs() < 1e-3),
        "points are on the ground"
    );
    let gen_ = w.get::<ProcGenerator>(e).expect("generator");
    assert_eq!(gen_.graph, ROAD);
    let regia_procgen::world::ProcBounds::Box { half_extents } = gen_.bounds else {
        panic!("box bounds");
    };
    assert!(
        half_extents.x >= 10.0 + tools::PATH_MARGIN_M - 1.0,
        "bounds cover the road: {half_extents}"
    );
    // The starter was written into the project.
    assert!(regia_procgen::world::read_graph_text(w, ROAD).is_some());
    // A second process of the same script gives the same bytes.
    let mut two = app();
    let (_, hash2, scene2) = road_session(&mut two);
    assert_eq!(hash, hash2, "output hash");
    assert_eq!(scene, scene2, "scene text");
}

#[test]
fn the_area_tool_builds_a_forest_deterministically() {
    let run = |app: &mut App| {
        ground(app);
        let target = spline_target(app);
        app.update();
        let out = tool_call(
            app,
            ToolKind::Area,
            FOREST,
            Some(target),
            ToolGesture {
                points: pts(&[
                    [-8.0, 5.0, -8.0],
                    [8.0, 5.0, -8.0],
                    [8.0, 5.0, 8.0],
                    [-8.0, 5.0, 8.0],
                ]),
                ..ToolGesture::default()
            },
            true,
        );
        let hash = run_to_ready(app, out.entity);
        (out.entity, hash, scene_text(app))
    };
    let mut one = app();
    let (e, hash, scene) = run(&mut one);
    let w = one.world();
    let spline = w.get::<regia_scene::spline::Spline>(e).expect("spline");
    assert!(spline.closed && spline.points.len() == 4);
    assert!(
        matches!(
            w.get::<regia_scene::shape::Shape>(e).map(|s| &s.kind),
            Some(regia_scene::shape::ShapeKind::Hull { points }) if points.len() == 4
        ),
        "the outline's hull rides along as a Shape"
    );
    let mut two = app();
    let (_, hash2, scene2) = run(&mut two);
    assert_eq!((hash, scene), (hash2, scene2));
}

#[test]
fn path_gestures_move_remove_clear_undo_and_validate() {
    let mut app = app();
    ground(&mut app);
    app.update();
    let before = scene_text(&mut app);
    let start = tool_call(
        &mut app,
        ToolKind::Path,
        ROAD,
        None,
        ToolGesture {
            points: pts(&[[0.0, 5.0, 0.0], [4.0, 5.0, 0.0], [8.0, 5.0, 0.0]]),
            ..ToolGesture::default()
        },
        false,
    );
    let e = start.entity;
    let mv = tool_call(
        &mut app,
        ToolKind::Path,
        ROAD,
        None,
        ToolGesture {
            move_point: Some((1, Vec3::new(4.0, 5.0, 3.0))),
            remove_point: Some(0),
            ..ToolGesture::default()
        },
        false,
    );
    assert_eq!(mv.spline_points, 2);
    let tf = *app.world().get::<Transform>(e).expect("tf");
    let s = app.world().get::<regia_scene::spline::Spline>(e).expect("spline");
    assert!((s.points[0].position.z + tf.translation.z - 3.0).abs() < 1e-3);
    // A bad index is refused and changes nothing.
    let bad = tools::run_tool(
        app.world_mut(),
        ToolKind::Path,
        ROAD,
        None,
        &ToolGesture {
            remove_point: Some(9),
            ..ToolGesture::default()
        },
        false,
    )
    .expect_err("index");
    assert_eq!(bad.code, "procgen_tool_bad_gesture");
    // A miss is reported.
    let miss = tools::run_tool(
        app.world_mut(),
        ToolKind::Path,
        ROAD,
        None,
        &ToolGesture {
            points: pts(&[[900.0, 5.0, 900.0]]),
            ..ToolGesture::default()
        },
        false,
    )
    .expect_err("miss");
    assert_eq!(miss.code, "procgen_tool_no_hit");
    // Clear then apply with one point: too few.
    let few = tools::run_tool(
        app.world_mut(),
        ToolKind::Path,
        ROAD,
        None,
        &ToolGesture {
            clear: true,
            points: pts(&[[1.0, 5.0, 1.0]]),
            ..ToolGesture::default()
        },
        true,
    )
    .expect_err("few");
    assert_eq!(few.code, "procgen_tool_bad_gesture");
    // The open session is still there; cancel restores the level byte for byte.
    assert!(tools::finish(app.world_mut(), false).expect("cancel"));
    app.update();
    assert_eq!(scene_text(&mut app), before);
    // Apply then undo/redo through the shared stack.
    let ok = tool_call(
        &mut app,
        ToolKind::Path,
        ROAD,
        None,
        ToolGesture {
            points: pts(&[[0.0, 5.0, 0.0], [6.0, 5.0, 0.0]]),
            ..ToolGesture::default()
        },
        true,
    );
    let guid = app.world().get::<EntityGuid>(ok.entity).expect("guid").0;
    assert!(undo_world(app.world_mut()));
    let alive = app
        .world_mut()
        .query::<&EntityGuid>()
        .iter(app.world())
        .any(|g| g.0 == guid);
    assert!(!alive);
    assert!(redo_world(app.world_mut()));
    let n = app
        .world_mut()
        .query::<(&EntityGuid, &regia_scene::spline::Spline)>()
        .iter(app.world())
        .find(|(g, _)| g.0 == guid)
        .map(|(_, s)| s.points.len());
    assert_eq!(n, Some(2));
}

#[test]
fn path_and_area_sessions_run_through_the_agent() {
    let mut a = Agent::new();
    ground(&mut a.app);
    a.app.update();
    let open = a.call(
        "editor.procgen_tool",
        json!({ "tool": "path", "preset": ROAD, "points": [[0, 5, 0], [5, 5, 0], [9, 5, 2]], "apply": false }),
    );
    assert_eq!(open["spline_points"], 3);
    let moved = a.call(
        "editor.procgen_tool",
        json!({ "tool": "path", "preset": ROAD, "move": { "index": 2, "to": [9, 5, 5] }, "remove": 0, "apply": true }),
    );
    assert_eq!(
        (moved["spline_points"].clone(), moved["applied"].clone()),
        (json!(2), json!(true))
    );
    let bad = a
        .try_call(
            "editor.procgen_tool",
            json!({ "tool": "area", "preset": FOREST, "move": { "index": 0 }, "apply": false }),
        )
        .expect_err("move shape");
    assert_eq!(reason(&bad), "procgen_tool_bad_gesture");
    let area = a.call(
        "editor.procgen_tool",
        json!({ "tool": "area", "preset": FOREST, "points": [[-4, 5, -4], [4, 5, -4], [0, 5, 4]], "apply": true }),
    );
    assert_eq!(area["spline_points"], 3);
}

// ---- Permissions of editing forms (spec 67 § 6, ADR-5646) ----

fn regia_code(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// A read-only profile (`read_only = true`) may read the panel but not edit through it:
/// `editor.procgen_pin` (always an undoable graph edit) is refused by the dispatcher, and the
/// editing forms of read methods (`procgen_starters { starter }`, `procgen_reload { take_theirs }`,
/// `procgen_capture { out }`) are refused by their handlers, all with `F0012`, before anything
/// changes or any capture is made. Their reading forms still answer.
#[test]
fn a_read_only_profile_cannot_edit_through_read_methods() {
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let graph_before = a.app.world().resource::<ProcGraphDoc>().graph.to_ron();
    let flags_before = a.app.world().resource::<ProcGraphDoc>().graph.flags.clone();
    let depth_before = a.app.world().resource::<UndoStack>().undo_depth();
    a.app
        .world_mut()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .read_only = true;
    let denied = |a: &mut Agent, method: &str, params: Value| {
        let e = match a.try_call(method, params.clone()) {
            Ok(v) => panic!("{method} {params} must be refused, answered {v}"),
            Err(e) => e,
        };
        assert_eq!(regia_code(&e), "F0012", "{method} {params}: {e:?}");
    };
    denied(
        &mut a,
        "editor.procgen_pin",
        json!({ "node": 2, "setting": "spacing_m", "show": true }),
    );
    denied(&mut a, "editor.procgen_starters", json!({ "starter": true }));
    denied(&mut a, "editor.procgen_reload", json!({ "take_theirs": true }));
    denied(
        &mut a,
        "editor.procgen_capture",
        json!({ "panel": "canvas", "out": "shots/canvas.png" }),
    );
    // Nothing changed: no undo entry, the same document state.
    assert_eq!(a.app.world().resource::<UndoStack>().undo_depth(), depth_before);
    let d = a.app.world().resource::<ProcGraphDoc>();
    assert_eq!(d.graph.to_ron(), graph_before);
    assert_eq!(d.graph.flags, flags_before);
    assert!(
        !d.shown_pins.contains(&(2, "spacing_m".to_string())),
        "{:?}",
        d.shown_pins
    );
    // The reading forms answer.
    let listed = a.call("editor.procgen_starters", json!({}));
    assert!(listed.get("starters").is_some(), "{listed}");
    let reload = a.call("editor.procgen_reload", json!({}));
    assert_eq!(reload, json!({ "reloaded": false, "conflict": false }));
}

// ---- Error codes and pure edits without a test of their own (spec 67 § 6, ADR-5646) ----

/// Spec 67 § 6 errors through the dispatcher: `procgen_not_open` before a document is open,
/// `procgen_no_preview` without a preview target, `procgen_file_exists` for a collapse onto an
/// existing file, plus the parameter errors listed on the book page (`procgen_bad_path`,
/// `procgen_not_starter`, `procgen_no_conflict`, `procgen_bad_tier`, `procgen_bad_visualize`).
#[test]
fn spec67_error_codes_answer_through_the_agent() {
    let mut a = Agent::new();
    let refused = |a: &mut Agent, method: &str, params: Value, code: &str| {
        let e = match a.try_call(method, params.clone()) {
            Ok(v) => panic!("{method} {params} must fail with {code}, answered {v}"),
            Err(e) => e,
        };
        assert_eq!(reason(&e), code, "{method} {params}: {e:?}");
    };
    refused(
        &mut a,
        "editor.procgen_select",
        json!({ "nodes": [2] }),
        "procgen_not_open",
    );
    refused(
        &mut a,
        "editor.procgen_preview",
        json!({ "tier": "potato" }),
        "procgen_bad_tier",
    );
    refused(
        &mut a,
        "editor.procgen_seed",
        json!({ "action": "pin" }),
        "procgen_no_preview",
    );
    refused(
        &mut a,
        "editor.procgen_new",
        json!({ "path": "x.txt" }),
        "procgen_bad_path",
    );
    refused(
        &mut a,
        "editor.procgen_new",
        json!({ "path": "n.pgraph.ron", "starter": "g.pgraph.ron" }),
        "procgen_not_starter",
    );
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    // Isolate checks for an open graph first, then for a preview target (a new level has no
    // generator, so opening binds none).
    refused(
        &mut a,
        "editor.procgen_isolate",
        json!({ "node": 2 }),
        "procgen_no_preview",
    );
    refused(
        &mut a,
        "editor.procgen_collapse",
        json!({ "nodes": [2], "path": "g.pgraph.ron" }),
        "procgen_file_exists",
    );
    refused(
        &mut a,
        "editor.procgen_reload",
        json!({ "take_theirs": true }),
        "procgen_no_conflict",
    );
    refused(
        &mut a,
        "editor.procgen_visualize",
        json!({ "node": 2, "on": true, "color_by": "rainbow" }),
        "procgen_bad_visualize",
    );
    refused(
        &mut a,
        "editor.procgen_visualize",
        json!({ "node": 2, "on": true, "color_by": "attr" }),
        "procgen_bad_visualize",
    );
}

/// Spec 67 § 3: the data table pages by 1 000 rows; an empty table still has one (empty) page.
#[test]
fn spec67_data_table_pages_by_a_thousand_rows() {
    let table = |n: usize| inspect::DataTable {
        pins: 1,
        items: 1,
        kind: "points".into(),
        tags: Vec::new(),
        columns: Vec::new(),
        rows: Vec::new(),
        order: (0..n).rev().collect(),
        cell: None,
    };
    let t = table(2500);
    assert_eq!(t.pages(), 3);
    assert_eq!(t.page(0).len(), 1000);
    assert_eq!(t.page(0)[0], 2499, "pages follow the sorted order");
    assert_eq!(t.page(2).len(), 500);
    assert!(t.page(9).is_empty());
    assert_eq!(table(1000).pages(), 1);
    assert_eq!(table(1001).pages(), 2);
    let empty = table(0);
    assert_eq!(empty.pages(), 1);
    assert!(empty.page(0).is_empty());
}

/// Spec 67 § 2.1 / § 2.4 document edits: align snaps to the first node; reorder clamps to the
/// end; a bad paste, a bad or duplicate parameter name, an unknown parameter and unbinding an
/// unbound setting answer their codes and change nothing.
#[test]
fn spec67_pure_document_edits_and_their_errors() {
    let mut g = grid_graph();
    let ids: BTreeSet<u32> = g.nodes.keys().copied().take(2).collect();
    assert_eq!(ids.len(), 2, "the test graph has two nodes to align");
    let first: Vec<u32> = ids.iter().copied().collect();
    if let Some(n) = g.nodes.get_mut(&first[0]) {
        n.at = (10.0, 20.0);
    }
    if let Some(n) = g.nodes.get_mut(&first[1]) {
        n.at = (300.0, 400.0);
    }
    doc::align(&mut g, &ids, true);
    assert_eq!(g.nodes[&first[1]].at, (10.0, 400.0), "vertical: same x");
    doc::align(&mut g, &ids, false);
    assert_eq!(g.nodes[&first[1]].at, (10.0, 20.0), "horizontal: same y");

    let decl = |name: &str| ParamDecl {
        name: name.to_string(),
        ty: AttrType::Float,
        default: SettingValue::Float(1.0),
        min: None,
        max: None,
        doc: String::new(),
    };
    let mut g = grid_graph();
    g.params.clear();
    for name in ["a", "b", "c"] {
        doc::add_param(&mut g, decl(name)).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }
    doc::reorder_param(&mut g, "a", 99).expect("clamped to the end");
    let names: Vec<&str> = g.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["b", "c", "a"]);
    let before = g.to_ron();
    let code = |r: Result<(), doc::DocError>| r.map_err(|e| e.code);
    assert_eq!(
        code(doc::reorder_param(&mut g, "zz", 0)),
        Err("procgen_unknown_param".into())
    );
    assert_eq!(
        code(doc::add_param(&mut g, decl("b"))),
        Err("procgen_param_name".into())
    );
    assert_eq!(
        code(doc::add_param(&mut g, decl("not a name"))),
        Err("procgen_param_name".into())
    );
    assert_eq!(
        doc::rename_param(&mut g, "a", "c").map_err(|e| e.code),
        Err("procgen_param_name".into())
    );
    assert_eq!(
        doc::paste_fragment(&mut g, "this is not RON", (0.0, 0.0)).map_err(|e| e.code),
        Err("procgen_paste_invalid".into())
    );
    let node = *g.nodes.keys().next().expect("a node");
    let setting = g.nodes[&node]
        .settings
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "spacing_m".into());
    assert_eq!(
        code(doc::unbind_setting(&mut g, node, &setting)),
        Err("procgen_not_bound".into())
    );
    assert_eq!(g.to_ron(), before, "refused edits change nothing");
}

/// Spec 67 § 7 test 10 and § 5.1: the Path tool with an entity that has no spline as its target and
/// "edit selected only" on answers `procgen_tool_target_unfit`, opens no session and touches nothing;
/// with the preference off the unfit target is passed over and the session creates `tool_road_01`
/// with a spline and the preset's generator instead.
#[test]
fn spec67_path_tool_refuses_an_unfit_target_only_with_edit_selected_only() {
    let mut app = app();
    ground(&mut app);
    let plain = app
        .world_mut()
        .spawn((Name::new("plain"), Transform::IDENTITY))
        .id();
    app.update();
    let gesture = || ToolGesture {
        points: pts(&[[-10.0, 5.0, 0.0], [0.0, 5.0, 4.0]]),
        ..ToolGesture::default()
    };
    app.world_mut()
        .resource_mut::<debug_view::ProcEditorPrefs>()
        .edit_selected_only = true;
    let unfit = tools::run_tool(
        app.world_mut(),
        ToolKind::Path,
        ROAD,
        Some(plain),
        &gesture(),
        false,
    )
    .expect_err("an entity without a spline is unfit for the Path tool");
    assert_eq!(unfit.code, "procgen_tool_target_unfit");
    assert!(unfit.message.contains("Spline"), "{}", unfit.message);
    let w = app.world();
    assert!(w.get::<regia_scene::spline::Spline>(plain).is_none());
    assert!(w.get::<ProcGenerator>(plain).is_none());
    assert!(
        !tools::finish(app.world_mut(), false).expect("no session"),
        "a refused call opens no session"
    );

    app.world_mut()
        .resource_mut::<debug_view::ProcEditorPrefs>()
        .edit_selected_only = false;
    let out = tool_call(&mut app, ToolKind::Path, ROAD, Some(plain), gesture(), false);
    assert!(out.created, "a fresh generator, not the unfit target");
    assert_ne!(out.entity, plain);
    let w = app.world();
    assert_eq!(
        w.get::<Name>(out.entity).map(|n| n.as_str().to_string()),
        Some("tool_road_01".to_string())
    );
    assert!(w.get::<regia_scene::spline::Spline>(out.entity).is_some());
    assert_eq!(
        w.get::<ProcGenerator>(out.entity).map(|g| g.graph.clone()),
        Some(ROAD.to_string())
    );
    assert!(w.get::<regia_scene::spline::Spline>(plain).is_none());
    assert!(w.get::<ProcGenerator>(plain).is_none());
    tools::finish(app.world_mut(), false).expect("cancel");
}

/// Spec 65 § 7 biome overlay over the agent dispatcher (lane post05-spline-biome-perf): the four
/// view flags round-trip through `editor.procgen_view_set`; `editor.procgen_biome_overlay` counts
/// the claim raster per biome and the blend-band cells whether or not a view draws; without a
/// preview run there are no footprints, filtered marks or hover; a bad `at` is `procgen_bad_view`.
#[test]
fn biome_overlay_views_and_counts_through_the_dispatcher() {
    use regia_procgen::biome::ecs::BiomeArea;
    let mut a = Agent::new();
    let def = |name: &str, priority: i32, color: &str| {
        format!(
            "BiomeDef(format_version: 1, name: \"{name}\", color: {color}, priority: {priority}, layers: [])"
        )
    };
    a.app
        .world_mut()
        .insert_resource(regia_procgen::world::ProcGraphOverlay(BTreeMap::from([
            (
                "forest.biome.ron".to_string(),
                def("Forest", 20, "(0.1, 0.6, 0.1)"),
            ),
            (
                "meadow.biome.ron".to_string(),
                def("Meadow", 10, "(0.9, 0.8, 0.2)"),
            ),
        ])));
    let mut forest = BiomeArea::new("forest.biome.ron");
    forest.blend_m = 10.0;
    let mut meadow = BiomeArea::new("meadow.biome.ron");
    meadow.blend_m = 0.0;
    a.app.world_mut().spawn((
        forest,
        Transform {
            translation: Vec3::new(50.0, 0.0, 0.0),
            scale: Vec3::new(100.0, 1.0, 40.0),
            ..Transform::IDENTITY
        },
    ));
    a.app.world_mut().spawn((
        meadow,
        Transform {
            scale: Vec3::new(200.0, 1.0, 40.0),
            ..Transform::IDENTITY
        },
    ));
    let read = a.call("editor.procgen_view_set", json!({}));
    assert_eq!(read["biome_tint"], false, "views start off");
    let set = a.call(
        "editor.procgen_view_set",
        json!({ "biome_tint": true, "biome_bands": true, "biome_filtered": true }),
    );
    assert_eq!(
        (
            set["biome_tint"].clone(),
            set["biome_bands"].clone(),
            set["biome_footprints"].clone()
        ),
        (json!(true), json!(true), json!(false))
    );
    let o = a.call("editor.procgen_biome_overlay", json!({ "at": [5.0, 0.0] }));
    assert_eq!(o["views"]["tint"], true);
    assert_eq!(o["views"]["footprints"], false);
    let cells = o["claim_cells"].as_u64().expect("claim_cells");
    let forest_cells = o["biomes"]["Forest"].as_u64().expect("forest cells");
    let meadow_cells = o["biomes"]["Meadow"].as_u64().expect("meadow cells");
    assert_eq!(cells, forest_cells + meadow_cells, "{o}");
    assert!(forest_cells > 0 && meadow_cells > 0, "{o}");
    let bands = o["band_cells"].as_u64().expect("band_cells");
    assert!(
        bands > 0 && bands < forest_cells,
        "the band is the forest's 10 m rim: {o}"
    );
    assert_eq!(o["footprints"], 0);
    assert_eq!(o["filtered"], 0);
    assert!(o["hover"].is_null(), "no preview run, nothing to hover");
    // The viewport draws the views that are on.
    let lines = regia_editor::procgen::biome_overlay::biome_view_lines(a.app.world_mut());
    assert_eq!(
        lines.len() as u64,
        2 * cells + 4 * bands,
        "tint crosses plus band outlines"
    );
    let err = a
        .try_call("editor.procgen_biome_overlay", json!({ "at": "here" }))
        .expect_err("bad at");
    assert_eq!(reason(&err), "procgen_bad_view");
}

/// post05w4 item 3c (the spline-biome-perf lane's known issue "tint and bands are drawn at the
/// owning area's height, not draped on the terrain"): over a terrain that slopes 0.25 m per meter
/// along +X, every claim cell of the tint view sits on the ground under it (plus the 5 cm lift),
/// and the drawn lines follow the slope.
#[test]
fn the_biome_tint_drapes_on_the_terrain_under_each_cell() {
    use regia_procgen::biome::ecs::BiomeArea;
    use regia_terrain::{CdlodConfig, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap};
    let mut a = Agent::new();
    a.app
        .world_mut()
        .insert_resource(regia_procgen::world::ProcGraphOverlay(BTreeMap::from([(
            "forest.biome.ron".to_string(),
            "BiomeDef(format_version: 1, name: \"Forest\", color: (0.1, 0.6, 0.1), priority: 20, layers: [])"
                .to_string(),
        )])));
    let mut forest = BiomeArea::new("forest.biome.ron");
    forest.blend_m = 0.0;
    a.app.world_mut().spawn((
        forest,
        Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            scale: Vec3::new(40.0, 1.0, 20.0),
            ..Transform::IDENTITY
        },
    ));
    // A 256 m square terrain: height = 0.25 · (x + 128), so 32 m at x = 0 and 42 m at x = 40.
    let n = 65;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| {
            let col = (i % n) as f32;
            0.25 * col * 256.0 / (n - 1) as f32
        })
        .collect();
    let map = TerrainMap::from_heights(n as u32, n as u32, heights, 256.0, 1.0, 0.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 0).expect("pyramid");
    let probe = (map.sample_bilinear(0.0, 0.0), map.sample_bilinear(20.0, 0.0));
    let ground = |x: f32| 0.25 * (x + 128.0);
    a.app.world_mut().spawn(Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: 0,
        minmax,
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    });
    assert!(
        (probe.1 - probe.0 - 5.0).abs() < 0.05,
        "the slope is 0.25: {probe:?}"
    );
    let views = regia_editor::procgen::biome_overlay::BiomeViews {
        tint: true,
        ..Default::default()
    };
    let state = regia_editor::procgen::biome_overlay::gather(a.app.world_mut(), views);
    assert!(
        state.cells.len() > 16,
        "the forest is rasterized: {}",
        state.cells.len()
    );
    for cell in state.cells.iter() {
        let [x, y, z] = cell.center;
        let want = ground(x) + 0.05;
        assert!(
            (y - want).abs() < 0.01,
            "cell at ({x}, {z}) is drawn at y {y}, the ground is {want}"
        );
    }
    // The agent counts the draped cells (every cell lies on the terrain here).
    let o = a.call("editor.procgen_biome_overlay", json!({}));
    assert_eq!(o["draped_cells"], o["claim_cells"], "{o}");
    // The lines rise with the ground: their highest point is at least 8 m above their lowest
    // (the area spans 40 m of a 0.25 slope, 10 m of rise). The viewport draws the views that are on.
    a.call("editor.procgen_view_set", json!({ "biome_tint": true }));
    let lines = regia_editor::procgen::biome_overlay::biome_view_lines(a.app.world_mut());
    assert!(!lines.is_empty(), "the tint draws");
    let ys: Vec<f32> = lines.iter().flat_map(|(p, q, _)| [p[1], q[1]]).collect();
    let (lo, hi) = ys
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), y| (l.min(*y), h.max(*y)));
    assert!(hi - lo > 8.0, "the tint follows the slope: {lo}..{hi}");
}

fn spawn_gen(app: &mut App, guid: u64) -> Entity {
    app.world_mut()
        .spawn((
            ProcGenerator {
                bounds: regia_procgen::world::ProcBounds::Box {
                    half_extents: Vec3::new(8.0, 2.0, 8.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(u128::from(guid))),
            Transform::IDENTITY,
        ))
        .id()
}

fn graph_ron(app: &App) -> String {
    app.world().resource::<ProcGraphDoc>().graph.to_ron()
}

#[test]
fn panel_actions_match_the_agent_methods() {
    let mut agent = Agent::new();
    let mut panel_app = app();
    agent.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    doc::open(panel_app.world_mut(), "g.pgraph.ron").expect("open");
    let a_gen = spawn_gen(&mut agent.app, 9);
    let a_other = spawn_gen(&mut agent.app, 10);
    let p_gen = spawn_gen(&mut panel_app, 9);
    let p_other = spawn_gen(&mut panel_app, 10);

    let coll = agent.call(
        "editor.procgen_collapse",
        json!({ "nodes": [2, 3], "path": "lane_core.pgraph.ron" }),
    );
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Collapse {
            nodes: BTreeSet::from([2, 3]),
            path: "lane_core.pgraph.ron".into(),
        },
    )
    .expect("panel collapse");
    assert_eq!(graph_ron(&agent.app), graph_ron(&panel_app), "collapse");
    let call = u32::try_from(coll["call_node"].as_u64().expect("call")).expect("u32");

    agent.call("editor.procgen_expand", json!({ "node": call }));
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Expand {
            node: call,
            copy: false,
        },
    )
    .expect("panel expand");
    assert_eq!(graph_ron(&agent.app), graph_ron(&panel_app), "expand");

    agent.call("editor.procgen_preview", json!({ "generator": a_gen.to_bits() }));
    debug_view::set_preview(panel_app.world_mut(), Some(p_gen)).expect("preview");
    agent.call("editor.procgen_isolate", json!({ "node": 2 }));
    panel::apply_panel_action(panel_app.world_mut(), PanelAction::Isolate(Some(2))).expect("isolate");
    let a_iso = agent
        .app
        .world()
        .resource::<ProcPreview>()
        .isolate
        .clone()
        .expect("agent isolate");
    let p_iso = panel_app
        .world()
        .resource::<ProcPreview>()
        .isolate
        .clone()
        .expect("panel isolate");
    assert_eq!(a_iso.node, p_iso.node);
    assert_eq!(a_iso.error.is_none(), p_iso.error.is_none());
    agent.call("editor.procgen_isolate", json!({ "node": null }));
    panel::apply_panel_action(panel_app.world_mut(), PanelAction::Isolate(None)).expect("end");
    assert!(agent.app.world().resource::<ProcPreview>().isolate.is_none());
    assert!(panel_app.world().resource::<ProcPreview>().isolate.is_none());

    let pin = agent.call(
        "editor.procgen_seed",
        json!({ "action": "pin", "generator": a_gen.to_bits() }),
    );
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Seed {
            entity: p_gen,
            action: doc::SeedAction::Pin,
        },
    )
    .expect("pin");
    let new = agent.call(
        "editor.procgen_seed",
        json!({ "action": "new", "generator": a_gen.to_bits() }),
    );
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Seed {
            entity: p_gen,
            action: doc::SeedAction::New,
        },
    )
    .expect("new");
    assert_ne!(new["seed"], pin["seed"]);
    let same = agent.call(
        "editor.procgen_seed",
        json!({ "action": "same_as", "generator": a_gen.to_bits(), "from": a_other.to_bits() }),
    );
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Seed {
            entity: p_gen,
            action: doc::SeedAction::SameAs(p_other),
        },
    )
    .expect("same");
    let a_seed = agent.app.world().get::<ProcGenerator>(a_gen).and_then(|g| g.seed);
    let p_seed = panel_app.world().get::<ProcGenerator>(p_gen).and_then(|g| g.seed);
    assert_eq!(a_seed, p_seed);
    assert_eq!(same["seed"].as_u64(), p_seed.map(u64::from));

    agent.call(
        "editor.procgen_preview",
        json!({ "generator": a_gen.to_bits(), "tier": "medium" }),
    );
    panel::apply_panel_action(
        panel_app.world_mut(),
        PanelAction::Tier(Some(regia_render::quality::QualityTier::Medium)),
    )
    .expect("tier");
    assert_eq!(
        agent.app.world().resource::<ProcPreview>().tier,
        panel_app.world().resource::<ProcPreview>().tier
    );
    assert_eq!(
        agent.app.world().resource::<ProcGenSettings>().running_tier,
        panel_app.world().resource::<ProcGenSettings>().running_tier
    );
    agent.call(
        "editor.procgen_preview",
        json!({ "generator": a_gen.to_bits(), "tier": null }),
    );
    panel::apply_panel_action(panel_app.world_mut(), PanelAction::Tier(None)).expect("running");
    assert_eq!(agent.app.world().resource::<ProcPreview>().tier, None);
    assert_eq!(panel_app.world().resource::<ProcPreview>().tier, None);
}

// ---- FU-5 (ADR-7383): the Nodes list, keyboard only ----

/// One headless panel frame with `events` (AccessKit on); returns the tree update.
fn panel_keys_frame(
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1600.0, 900.0),
        )),
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        regia_editor::procgen::panel::procgen_graph_panel(ui, world)
    });
    out.textures_delta.clear();
    out.platform_output.accesskit_update.expect("accesskit is on")
}

fn press(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn focused(update: &egui::accesskit::TreeUpdate) -> Option<String> {
    update
        .nodes
        .iter()
        .find(|(id, _)| *id == update.focus)
        .and_then(|(_, n)| n.label().map(str::to_string))
}

/// The canvas is named with its node count, the panel has no unnamed widget, and the Nodes list
/// adds `points.jitter` wired after node 3 (Ctrl+Shift+A, filter, Enter) and moves it one grid
/// step down (Alt+Down, one undo entry) with keys only: the graph equals the one
/// `procgen.graph_edit` builds.
#[test]
fn the_nodes_list_builds_by_keyboard_what_graph_edit_builds() {
    use egui::{Key, Modifiers};
    let mut a = Agent::new();
    a.call("editor.procgen_open", json!({ "path": "g.pgraph.ron" }));
    let added = a.call(
        "procgen.graph_edit",
        json!({ "path": "g.pgraph.ron", "ops": [
            { "op": "add_node", "ty": "points.jitter", "at": [620.0, 0.0] },
            { "op": "connect", "from": [3, "out"], "to": [5, "in"] },
            { "op": "move", "id": 5, "at": [620.0, 16.0] },
        ] }),
    );
    assert_eq!(added["document"], true, "{added}");
    let agent_graph = a.app.world().resource::<ProcGraphDoc>().graph.clone();

    let mut app = app();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    let world = app.world_mut();
    let ctx = egui::Context::default();
    panel_keys_frame(&ctx, world, vec![]);
    let first = panel_keys_frame(&ctx, world, vec![]);
    assert_eq!(
        regia_editor::a11y_audit::unnamed_widgets(&first),
        Vec::<String>::new(),
        "{:?}",
        regia_editor::a11y_audit::widget_names(&first)
    );
    assert!(
        first
            .nodes
            .iter()
            .any(|(_, n)| n.role() == egui::accesskit::Role::Canvas
                && n.label() == Some("Procedural graph canvas, 4 nodes")),
        "the canvas is named"
    );
    let mut seen = Vec::new();
    let reached = (0..400).any(|_| {
        let update = panel_keys_frame(&ctx, world, vec![press(Key::Tab, Modifiers::NONE)]);
        let name = focused(&update);
        let hit = name.as_deref() == Some("points.jitter #3");
        seen.push(name.unwrap_or_default());
        hit
    });
    assert!(reached, "Tab reaches row 3: {seen:?}");
    panel_keys_frame(
        &ctx,
        world,
        vec![press(Key::A, Modifiers::COMMAND | Modifiers::SHIFT)],
    );
    panel_keys_frame(&ctx, world, vec![]);
    panel_keys_frame(&ctx, world, vec![egui::Event::Text("points.jitter".into())]);
    let toast = world.resource::<ProcGraphDoc>().toast.clone();
    let depth = world.resource::<UndoStack>().undo_depth();
    panel_keys_frame(&ctx, world, vec![press(Key::Enter, Modifiers::NONE)]);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "add and wire is one undo entry"
    );
    panel_keys_frame(&ctx, world, vec![]);
    let back = panel_keys_frame(&ctx, world, vec![]);
    assert_eq!(
        focused(&back).as_deref(),
        Some("points.jitter #3"),
        "the focus comes back to the row the node was added after"
    );
    for _ in 0..2 {
        panel_keys_frame(&ctx, world, vec![press(Key::ArrowDown, Modifiers::NONE)]);
    }
    let down = panel_keys_frame(&ctx, world, vec![]);
    assert_eq!(focused(&down).as_deref(), Some("points.jitter #5"));
    panel_keys_frame(&ctx, world, vec![press(Key::ArrowDown, Modifiers::ALT)]);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 2);
    let doc = world.resource::<ProcGraphDoc>();
    assert_eq!(
        doc.toast, toast,
        "no edit was refused (a refusal replaces the toast)"
    );
    assert_eq!(doc.graph.to_ron(), agent_graph.to_ron());
    let (drawn, total) = regia_editor::procgen::panel::node_rows(world);
    assert_eq!((drawn, total), (5, 5));
}

/// A 5 000-node graph's Nodes list lays out only the rows in view.
#[test]
fn a_5000_node_list_lays_out_only_the_rows_in_view() {
    let mut app = app();
    let mut g = ProcGraph::default();
    for i in 1..=5_000u32 {
        g.nodes
            .insert(i, node("points.jitter", f64::from(i % 50) * 220.0, &[]));
    }
    doc::write_source(app.world_mut(), "big.pgraph.ron", &g.to_ron()).expect("write");
    doc::open(app.world_mut(), "big.pgraph.ron").expect("open");
    let ctx = egui::Context::default();
    for _ in 0..2 {
        panel_keys_frame(&ctx, app.world_mut(), vec![]);
    }
    let (drawn, total) = regia_editor::procgen::panel::node_rows(app.world());
    assert_eq!(total, 5_000);
    assert!(drawn > 0 && drawn < 60, "{drawn} rows laid out");
}
