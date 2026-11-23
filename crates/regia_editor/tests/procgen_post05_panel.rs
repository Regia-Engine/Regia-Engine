// SPDX-License-Identifier: MPL-2.0
//! Post-v0.5 procedural graph panel remainder (spec 67 § 2): the model behind the Parameters tab,
//! the typed setting widgets (curve editor, name lists, vectors, asset picker), the data table's
//! sort / filter / page controls, pin colors and node badges, promote to parameter with a name
//! prompt, the collapse boundary check and the shipped presets. The panel's widgets push these
//! same actions; the tests apply them directly, then draw the panel headless once. No GPU, no
//! wall clock, no golden images.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::procgen::badges::{self, Badge};
use regia_editor::procgen::debug_view;
use regia_editor::procgen::doc::{self, ProcGraphDoc};
use regia_editor::procgen::inspect::{self, ProcInspect, TableControl};
use regia_editor::procgen::panel;
use regia_editor::procgen::params_tab::{self, ParamAction};
use regia_editor::procgen::presets;
use regia_editor::procgen::tools::ToolKind;
use regia_editor::procgen::widgets::{self, WidgetKind};
use regia_editor::theme::ThemeTokens;
use regia_procgen::world::{
    ProcBounds, ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
};
use regia_procgen::{AttrType, DataKind, Edge, KindSet, NodeEntry, ProcGraph, SettingValue};
use regia_scene::components::EntityGuid;
use regia_scene::prelude::Transform;

fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    n.at = (at, 0.0);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

fn graph() -> ProcGraph {
    let text = r#"ProcGraph(
    format_version: 1,
    params: [
        (name: "spacing", ty: Float, default: 4.0, min: Some(0.5), max: Some(16.0), doc: "Cell size"),
        (name: "count", ty: Int, default: 3, min: None, max: None, doc: ""),
    ],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "sample.region", settings: {spacing_m: Bind("spacing")}, at: (200.0, 0.0)),
        3: (ty: "density.curve", settings: {}, at: (400.0, 0.0)),
        4: (ty: "out.points", settings: {}, at: (600.0, 0.0)),
    },
    edges: [
        (from: (1, "out"), to: (2, "region")),
        (from: (2, "out"), to: (3, "in")),
        (from: (3, "out"), to: (4, "in")),
    ],
)"#;
    ProcGraph::from_ron(text).expect("parses")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::from([("g.pgraph.ron".to_string(), graph().to_ron())])),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>();
    regia_editor::procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
    app
}

fn frame(app: &mut App) {
    regia_editor::procgen::tick(app.world_mut());
    app.update();
}

fn depth(app: &App) -> usize {
    app.world().resource::<UndoStack>().undo_depth()
}

fn g(app: &App) -> ProcGraph {
    app.world().resource::<ProcGraphDoc>().graph.clone()
}

// ---- Item 1: the Parameters tab ----

#[test]
fn each_parameters_tab_action_is_one_undo_entry() {
    let mut app = app();
    let steps = [
        ParamAction::Declare {
            name: "offset".into(),
            ty: AttrType::Vec3,
        },
        ParamAction::Rename {
            from: "spacing".into(),
            to: "cell".into(),
        },
        ParamAction::Move {
            name: "offset".into(),
            to: 0,
        },
        ParamAction::SetType {
            name: "count".into(),
            ty: AttrType::Float,
        },
        ParamAction::SetDefault {
            name: "count".into(),
            value: Some(SettingValue::Float(5.0)),
        },
        ParamAction::SetRange {
            name: "count".into(),
            min: Some(1.0),
            max: Some(9.0),
        },
        ParamAction::SetDoc {
            name: "count".into(),
            doc: "How many".into(),
        },
    ];
    for (k, step) in steps.iter().enumerate() {
        let before = depth(&app);
        params_tab::apply_param_action(app.world_mut(), step.clone())
            .unwrap_or_else(|e| panic!("{step:?}: {e}"));
        assert_eq!(depth(&app), before + 1, "step {k} {step:?} is one undo entry");
    }
    let graph = g(&app);
    let names: Vec<&str> = graph.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["offset", "cell", "count"]);
    assert_eq!(graph.nodes[&2].settings["spacing_m"].as_bind(), Some("cell"));
    let count = graph.param("count").expect("count");
    assert_eq!(
        (
            count.ty,
            count.default.clone(),
            count.min,
            count.max,
            count.doc.as_str()
        ),
        (
            AttrType::Float,
            SettingValue::Float(5.0),
            Some(1.0),
            Some(9.0),
            "How many"
        )
    );
    assert_eq!(
        graph.param("offset").map(|p| p.default.clone()),
        Some(SettingValue::Tuple(vec![SettingValue::Float(0.0); 3])),
        "a declared Vec3 defaults to zero"
    );
    // Undo walks back one step at a time.
    assert!(undo_world(app.world_mut()));
    assert_eq!(g(&app).param("count").map(|p| p.doc.clone()), Some(String::new()));
}

#[test]
fn refused_parameter_edits_write_nothing() {
    let mut app = app();
    let before = g(&app);
    let depth0 = depth(&app);
    let cases = [
        (
            ParamAction::Rename {
                from: "spacing".into(),
                to: "count".into(),
            },
            "procgen_param_name",
        ),
        (
            ParamAction::Rename {
                from: "nope".into(),
                to: "fine".into(),
            },
            "procgen_unknown_param",
        ),
        (
            ParamAction::SetRange {
                name: "spacing".into(),
                min: Some(5.0),
                max: Some(1.0),
            },
            "procgen_param_range",
        ),
        (
            ParamAction::Declare {
                name: "bad name".into(),
                ty: AttrType::Float,
            },
            "procgen_param_name",
        ),
        (
            ParamAction::SetDefault {
                name: "spacing".into(),
                value: Some(SettingValue::Str("text".into())),
            },
            "procgen_param_type",
        ),
    ];
    for (action, code) in cases {
        let e = params_tab::apply_param_action(app.world_mut(), action.clone()).expect_err("refused");
        assert_eq!(e.code, code, "{action:?}");
    }
    assert_eq!(g(&app), before);
    assert_eq!(depth(&app), depth0);
}

#[test]
fn a_type_change_keeps_a_convertible_default_and_resets_the_rest() {
    assert_eq!(
        params_tab::convert_default(&SettingValue::Int(3), AttrType::Float),
        SettingValue::Float(3.0)
    );
    assert_eq!(
        params_tab::convert_default(&SettingValue::Float(2.7), AttrType::Int),
        SettingValue::Int(3)
    );
    assert_eq!(
        params_tab::convert_default(&SettingValue::Float(2.0), AttrType::Bool),
        SettingValue::Bool(false)
    );
    assert_eq!(
        params_tab::convert_default(&SettingValue::Str("rock".into()), AttrType::Name),
        SettingValue::Str("rock".into())
    );
}

// ---- Item 2: typed setting widgets ----

#[test]
fn every_setting_type_maps_to_a_widget() {
    use regia_procgen::nodes::SettingType as T;
    let cases = [
        (T::Float, WidgetKind::Number { integer: false }),
        (T::Int, WidgetKind::Number { integer: true }),
        (T::Bool, WidgetKind::Toggle),
        (T::Vec3, WidgetKind::Vec3),
        (T::Range2, WidgetKind::Range2),
        (T::Vec3Range, WidgetKind::Vec3Range),
        (T::Enum(&["a", "b"]), WidgetKind::Enum),
        (T::Curve, WidgetKind::Curve),
        (T::Asset, WidgetKind::Asset),
        (T::Name, WidgetKind::Name),
        (T::NameList, WidgetKind::NameList),
        (T::Selector, WidgetKind::Name),
        (T::Any, WidgetKind::Text),
    ];
    for (ty, want) in cases {
        assert_eq!(widgets::widget_kind(ty), want, "{ty:?}");
    }
}

#[test]
fn the_curve_editor_keeps_keys_sorted_and_inside_the_neighbors() {
    let curve = SettingValue::parse("[(0.0, 0.0), (1.0, 1.0)]").expect("ron");
    let mut keys = widgets::curve_keys(&curve).expect("a curve");
    assert_eq!(keys, vec![[0.0, 0.0], [1.0, 1.0]]);
    let at = widgets::curve_insert(&mut keys, [0.5, 0.2]);
    assert_eq!(at, 1);
    assert_eq!(keys, vec![[0.0, 0.0], [0.5, 0.2], [1.0, 1.0]]);
    // Dragging a key past its neighbor stops at the neighbor.
    widgets::curve_move(&mut keys, 1, [2.0, 0.9]);
    assert_eq!(keys[1], [1.0, 0.9]);
    widgets::curve_remove(&mut keys, 1);
    assert_eq!(keys.len(), 2);
    // A curve keeps at least two keys.
    widgets::curve_remove(&mut keys, 0);
    assert_eq!(keys.len(), 2);
    assert_eq!(
        widgets::curve_value(&keys),
        SettingValue::List(vec![
            SettingValue::Tuple(vec![SettingValue::Float(0.0), SettingValue::Float(0.0)]),
            SettingValue::Tuple(vec![SettingValue::Float(1.0), SettingValue::Float(1.0)]),
        ])
    );
    // The value round-trips through the node: setting it validates.
    let mut app = app();
    let value = widgets::curve_value(&[[0.0, 1.0], [0.3, 0.5], [1.0, 0.0]]);
    widgets::check_setting(app.world(), 3, "curve", &value).expect("valid curve");
    doc::edit(app.world_mut(), "curve", None, |c| {
        doc::set_setting(c, 3, "curve", value.clone())
    })
    .expect("set");
    assert_eq!(g(&app).nodes[&3].settings["curve"], value);
}

#[test]
fn name_lists_and_vectors_write_typed_values() {
    let list = widgets::name_list_value(&["a".to_string(), "b".to_string()]);
    assert_eq!(
        list,
        SettingValue::List(vec![SettingValue::Str("a".into()), SettingValue::Str("b".into())])
    );
    assert_eq!(
        widgets::name_list(&list),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(
        widgets::vec3_value([1.0, 2.0, 3.0]),
        SettingValue::Tuple(vec![
            SettingValue::Float(1.0),
            SettingValue::Float(2.0),
            SettingValue::Float(3.0)
        ])
    );
    assert_eq!(
        widgets::pair_value([0.25, 0.75]),
        SettingValue::List(vec![SettingValue::Float(0.25), SettingValue::Float(0.75)])
    );
    // An out-of-range value is refused with the compiler's message.
    let app = app();
    let e = widgets::check_setting(app.world(), 2, "jitter", &SettingValue::Float(3.0)).expect_err("range");
    assert_eq!(e.code, "setting_out_of_range");
    assert!(!e.message.is_empty());
}

// ---- Item 3: table sort, filter and page controls ----

fn previewed(app: &mut App) -> Entity {
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(30.0, 2.0, 30.0),
                },
                ..ProcGenerator::new("g.pgraph.ron")
            },
            EntityGuid(Guid(0x00ab_cdef_0123_4567_89ab_cdef_0123_4567)),
            Transform::IDENTITY,
        ))
        .id();
    // 60 m square at 0.5 m cells: 14 400 rows, so the table has pages.
    params_tab::apply_param_action(
        app.world_mut(),
        ParamAction::SetDefault {
            name: "spacing".into(),
            value: Some(SettingValue::Float(0.5)),
        },
    )
    .expect("spacing");
    debug_view::set_preview(app.world_mut(), Some(e)).expect("preview");
    for _ in 0..14 {
        frame(app);
    }
    assert!(
        app.world()
            .get::<ProcGenState>(e)
            .and_then(|s| s.debug.as_ref())
            .is_some()
    );
    *app.world_mut().resource_mut::<ProcInspect>() = ProcInspect {
        node: Some(3),
        ..ProcInspect::default()
    };
    e
}

#[test]
fn table_controls_sort_filter_and_page_the_view() {
    let mut app = app();
    previewed(&mut app);
    let w = app.world_mut();
    let rows = inspect::build_table(w).expect("table").order.len();
    assert!(rows > 2 * 1000, "{rows} rows");
    // Header clicks: ascending, descending, off.
    inspect::apply_table_control(w, TableControl::ToggleSort("position.x".into())).expect("sort");
    assert_eq!(
        w.resource::<ProcInspect>().sort,
        Some(("position.x".into(), false))
    );
    inspect::apply_table_control(w, TableControl::ToggleSort("position.x".into())).expect("sort");
    assert_eq!(
        w.resource::<ProcInspect>().sort,
        Some(("position.x".into(), true))
    );
    let t = inspect::build_table(w).expect("table");
    let first = t.rows[t.order[0]][1].clone();
    let last = t.rows[*t.order.last().expect("rows")][1].clone();
    assert!(
        matches!((&first, &last), (inspect::Cell::Num(a), inspect::Cell::Num(b)) if a >= b),
        "descending"
    );
    inspect::apply_table_control(w, TableControl::ToggleSort("position.x".into())).expect("sort");
    assert_eq!(w.resource::<ProcInspect>().sort, None);
    // Paging: next, last, clamped past the end; back to the first.
    let pages = inspect::build_table(w).expect("table").pages();
    inspect::apply_table_control(w, TableControl::Page(1)).expect("page");
    assert_eq!(w.resource::<ProcInspect>().page, 1);
    inspect::apply_table_control(w, TableControl::Page(usize::MAX)).expect("page");
    assert_eq!(
        w.resource::<ProcInspect>().page,
        pages - 1,
        "clamped to the last page"
    );
    // A filter resets to the first page; a bad one is refused and the good one stays.
    inspect::apply_table_control(w, TableControl::Filter("position.x >= 0".into())).expect("filter");
    assert_eq!(w.resource::<ProcInspect>().page, 0);
    let kept = inspect::build_table(w).expect("table").order.len();
    assert!(kept < rows && kept > 0);
    let bad = inspect::apply_table_control(w, TableControl::Filter("position.x >=".into())).expect_err("bad");
    assert_eq!(bad.code, "procgen_filter_invalid");
    let unknown = inspect::apply_table_control(w, TableControl::Filter("nope > 1".into())).expect_err("bad");
    assert_eq!(unknown.code, "procgen_unknown_column");
    assert_eq!(
        w.resource::<ProcInspect>().filter.as_deref(),
        Some("position.x >= 0")
    );
    inspect::apply_table_control(w, TableControl::Filter(String::new())).expect("clear");
    assert_eq!(inspect::build_table(w).expect("table").order.len(), rows);
    // Clicking a row selects its element.
    inspect::apply_table_control(w, TableControl::Page(1)).expect("page");
    inspect::apply_table_control(w, TableControl::SelectRow(3)).expect("row");
    assert_eq!(w.resource::<ProcInspect>().selected, Some(1003));
    // A pin or item change starts at the first page.
    inspect::apply_table_control(w, TableControl::Item(0)).expect("item");
    assert_eq!(w.resource::<ProcInspect>().page, 0);
}

// ---- Item 4: pin colors and badges ----

#[test]
fn pin_colors_come_from_the_theme_and_multi_kind_pins_are_striped() {
    let mut tokens = ThemeTokens::regia_default();
    let points = KindSet::of(&[DataKind::Points]);
    let one = badges::pin_colors(&tokens, points);
    assert_eq!(one.len(), 1);
    tokens.data_kinds[DataKind::Points as usize] = egui::Color32::from_rgb(255, 0, 0);
    assert_eq!(
        badges::pin_colors(&tokens, points),
        vec![egui::Color32::from_rgb(255, 0, 0)]
    );
    let two = badges::pin_colors(&tokens, KindSet::of(&[DataKind::Points, DataKind::Spline]));
    assert_eq!(two.len(), 2, "one stripe per kind");
    assert_ne!(two[0], two[1]);
    let any = badges::pin_colors(&tokens, KindSet::of(&DataKind::ALL));
    assert_eq!(any.len(), 1, "a pin of every kind draws one neutral color");
    // The theme JSON names the kinds.
    let parsed = ThemeTokens::from_json(r##"{ "data_kinds": { "spline": "#00ff00" } }"##).expect("json");
    assert_eq!(
        parsed.data_kinds[DataKind::Spline as usize],
        egui::Color32::from_rgb(0, 255, 0)
    );
    assert_eq!(
        parsed.to_json()["data_kinds"]["spline"],
        serde_json::json!("#00ff00")
    );
    // The default set is the colorblind-safe one the panel had.
    assert_eq!(
        ThemeTokens::regia_default().data_kinds[DataKind::Points as usize],
        panel::kind_color(points)
    );
}

#[test]
fn badges_report_bypass_visualize_inspect_cache_warnings_tier_and_errors() {
    let mut app = app();
    let e = previewed(&mut app);
    doc::edit(app.world_mut(), "bypass", None, |c| {
        doc::set_enabled(c.graph, 3, false)
    })
    .expect("bypass");
    debug_view::visualize(
        app.world_mut(),
        2,
        Some(debug_view::VisualizeStyle {
            color_by: debug_view::ColorBy::Density,
            form: debug_view::VisForm::Points,
            color: debug_view::categorical(0),
        }),
    );
    // A run report with a warning and a cache hit on node 2.
    if let Some(mut st) = app.world_mut().get_mut::<ProcGenState>(e)
        && let Some(r) = st.report.as_mut()
        && let Some(n) = r.nodes.iter_mut().find(|n| n.id == 2)
    {
        n.cache_hit = true;
        n.warnings = vec![("w1".into(), "first".into()), ("w2".into(), "second".into())];
    }
    app.world_mut()
        .get_mut::<ProcGenerator>(e)
        .expect("generator")
        .tiles = Some(regia_procgen::tiles::TileSettings {
        cell_m: 64.0,
        ..regia_procgen::tiles::TileSettings::default()
    });
    let b = badges::node_badges(app.world(), None);
    let n2 = &b[&2];
    assert!(n2.list.contains(&Badge::Visualized));
    assert!(
        !n2.list.contains(&Badge::Inspected),
        "node 3 is the inspected one"
    );
    assert!(n2.list.contains(&Badge::CacheHit));
    assert_eq!(n2.warnings, 2);
    assert!(n2.list.contains(&Badge::Warnings(2)));
    assert_eq!(n2.tier.map(|t| t.cell_m), Some(Some(64.0)));
    assert!(b[&3].list.contains(&Badge::Bypassed));
    assert!(b[&3].list.contains(&Badge::Inspected));
    // A compile error lands on its node.
    doc::edit(app.world_mut(), "add", None, |c| {
        doc::add_node(c, "density.remap", (0.0, 300.0))
    })
    .expect("add");
    let b = badges::node_badges(app.world(), Some(&BTreeSet::from([5])));
    assert_eq!(b.len(), 1);
    assert!(
        b[&5].list.iter().any(|x| matches!(x, Badge::Error { .. })),
        "{:?}",
        b[&5]
    );
}

// ---- Item 5: promote with a name prompt ----

#[test]
fn promote_suggests_a_free_name_and_takes_the_typed_one() {
    let mut app = app();
    let graph = g(&app);
    assert_eq!(doc::suggest_param_name(&graph, "jitter"), "jitter");
    assert_eq!(doc::suggest_param_name(&graph, "spacing"), "spacing_2");
    assert_eq!(doc::suggest_param_name(&graph, "spacing_m"), "spacing_m");
    let before = depth(&app);
    let (name, created) =
        doc::promote_setting(app.world_mut(), 2, "jitter", Some("wiggle"), false).expect("promote");
    assert_eq!((name.as_str(), created), ("wiggle", true));
    assert_eq!(depth(&app), before + 1);
    assert_eq!(g(&app).nodes[&2].settings["jitter"].as_bind(), Some("wiggle"));
    let e = doc::promote_setting(app.world_mut(), 2, "y", Some("not ok"), false).expect_err("bad name");
    assert_eq!(e.code, "procgen_param_name");
    let e = doc::promote_setting(app.world_mut(), 2, "y", Some("count"), false).expect_err("taken");
    assert_eq!(e.code, "procgen_param_name");
    // Picking an existing parameter of another type is refused.
    let e = doc::promote_setting(app.world_mut(), 2, "height", Some("spacing"), true).expect_err("type");
    assert_eq!(e.code, "procgen_param_type");
    let (name, created) = doc::promote_setting(app.world_mut(), 2, "y", Some("spacing"), true).expect("pick");
    assert_eq!((name.as_str(), created), ("spacing", false));
    assert_eq!(depth(&app), before + 2);
}

// ---- Item 6: the collapse boundary check ----

#[test]
fn the_collapse_check_wants_one_connected_group_with_no_re_entering_path() {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", 0.0, &[]));
    g.nodes.insert(2, node("sample.region", 200.0, &[]));
    g.nodes.insert(3, node("density.curve", 400.0, &[]));
    g.nodes.insert(4, node("out.points", 600.0, &[]));
    g.nodes.insert(5, node("sample.region", 200.0, &[]));
    g.nodes.insert(6, node("note", 300.0, &[("text", "\"hi\"")]));
    g.edges = vec![
        Edge::new(1, "out", 2, "region"),
        Edge::new(2, "out", 3, "in"),
        Edge::new(3, "out", 4, "in"),
        Edge::new(1, "out", 5, "region"),
    ];
    let check = |ids: &[u32]| doc::collapse_check(&g, &ids.iter().copied().collect());
    check(&[2, 3]).expect("connected");
    check(&[3]).expect("one node");
    check(&[2, 3, 6]).expect("a note rides along");
    let split = check(&[3, 5]).expect_err("two groups");
    assert_eq!(split.code, "procgen_collapse_invalid");
    assert!(split.message.contains("connected"), "{}", split.message);
    let cycle = check(&[2, 4]).expect_err("re-entering");
    assert_eq!(cycle.code, "procgen_collapse_invalid");
    assert!(
        cycle.message.contains("3"),
        "names the outside node: {}",
        cycle.message
    );
    // collapse() itself refuses and leaves the graph unchanged.
    let registry = regia_procgen::ProcNodeRegistry::with_builtins();
    let mut probe = g.clone();
    doc::collapse(&mut probe, &registry, &BTreeSet::from([2, 4]), "x.pgraph.ron").expect_err("refused");
    assert_eq!(probe, g);
}

// ---- Item 7: presets ----

#[test]
fn the_fence_preset_ships_with_the_path_presets() {
    assert!(presets::all().iter().any(|(p, _)| *p == presets::TOOL_FENCE_PATH));
    assert_eq!(
        presets::for_kind(ToolKind::Path).collect::<Vec<_>>(),
        vec![presets::TOOL_ROAD_PATH, presets::TOOL_FENCE_PATH]
    );
    let g = ProcGraph::from_ron(presets::TOOL_FENCE).expect("parses");
    assert!(g.nodes.values().any(|n| n.ty == "spline.modules"));
}

// ---- The panel draws the new tabs headless ----

#[test]
fn the_panel_draws_the_parameters_tab_and_the_table_controls() {
    let mut app = app();
    previewed(&mut app);
    panel::set_tabs(app.world_mut(), true, true);
    app.world_mut().resource_mut::<ProcGraphDoc>().selection = BTreeSet::from([3]);
    let ctx = egui::Context::default();
    for _ in 0..2 {
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 900.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| panel::procgen_graph_panel(ui, app.world_mut()),
        );
        out.textures_delta.clear();
    }
    let (drawn, total) = panel::table_rows(app.world());
    assert!(total > 0 && drawn > 0, "the table drew rows: {drawn}/{total}");
    assert_eq!(panel::param_rows_drawn(app.world()), 2, "one row per parameter");
}
