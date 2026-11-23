// SPDX-License-Identifier: MPL-2.0
//! UI components (`docs/specs/111-ui-components.md`, ADR-3147): one component used three times
//! with different parameters and bind roots, slots, cycle and depth errors, placeholders, and hot
//! reload of a component reaching every instance with focus and scroll kept. Headless: no window,
//! no GPU, fixed steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_ui::authoring::{UiLibrary, insert_ui_template, spawn_ui, ui_node_keys};
use regia_ui::node::{ComputedNode, NodeName};
use regia_ui::prelude::*;
use regia_ui::scroll::ScrollOffset;
use regia_ui::text::TextContent;
use regia_ui::ui_component::PLACEHOLDER_NAME;
use regia_ui::{FocusState, UiMigrators, ViewModelAppExt, parse_ui_ron};

#[derive(Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Default)]
struct Member {
    name: String,
    hp: f32,
}

#[derive(Resource, Reflect, Debug, Default, Clone, PartialEq)]
#[reflect(Resource, Default)]
struct Party {
    p1: Member,
    p2: Member,
    p3: Member,
}

const BAR: &str = r##"(
    version: 9,
    component: (
        params: [
            (name: "label", type: string, default: "HP", targets: [(node: "Caption", property: text)]),
            (name: "fill", type: color, default: "#30C060", targets: [(node: "Fill", property: background)]),
            (name: "width", type: number, default: 100, targets: [(node: "Fill", property: width)]),
        ],
        slots: ["badge"],
    ),
    root: (name: "Bar", style: (direction: row, gap: (4, 4)), children: [
        (name: "Caption", text: (spans: ["?"])),
        (name: "Who", text: (spans: ["-"]), bind: (source: "$.name")),
        (name: "Fill", style: (size: (100, 10))),
        (name: "Act", widget: button, children: [(text: (spans: ["Go"]))]),
        (name: "List", widget: scroll(0.0, false), style: (size: (60, 20)), clip: true, children: [
            (name: "Tall", style: (size: (60, 400))),
        ]),
        (name: "Badge", slot: "badge", children: [(name: "DefaultBadge", text: (spans: ["*"]))]),
    ]),
)"##;

const HUD: &str = r##"(version: 9, root: (
    style: (direction: column, size: (Percent(100), Percent(100))),
    children: [
        (use: "ui/bar.ui.ron", name: "P1", bind_root: "Party.p1", params: {"label": "One"}),
        (use: "ui/bar.ui.ron", name: "P2", bind_root: "Party.p2", params: {"fill": "#FF0000", "width": 150},
            slots: {"badge": [(name: "Star", text: (spans: ["!"]))]}),
        (use: "ui/bar.ui.ron", name: "P3", bind_root: "Party.p3", params: {"label": "Three", "width": 200}),
    ],
))"##;

/// The same HUD written out by hand: what the three instances must expand to.
const INLINE: &str = r##"(version: 9, root: (
    style: (direction: column, size: (Percent(100), Percent(100))),
    children: [
        (name: "P1", style: (direction: row, gap: (4, 4)), children: [
            (name: "Caption", text: (spans: ["One"])),
            (name: "Who", text: (spans: ["-"]), bind: (source: "Party.p1.name")),
            (name: "Fill", style: (size: (100, 10)), background: "#30C060"),
            (name: "Act", widget: button, children: [(text: (spans: ["Go"]))]),
            (name: "List", widget: scroll(0.0, false), style: (size: (60, 20)), clip: true, children: [
                (name: "Tall", style: (size: (60, 400))),
            ]),
            (name: "Badge", children: [(name: "DefaultBadge", text: (spans: ["*"]))]),
        ]),
        (name: "P2", style: (direction: row, gap: (4, 4)), children: [
            (name: "Caption", text: (spans: ["HP"])),
            (name: "Who", text: (spans: ["-"]), bind: (source: "Party.p2.name")),
            (name: "Fill", style: (size: (150, 10)), background: "#FF0000"),
            (name: "Act", widget: button, children: [(text: (spans: ["Go"]))]),
            (name: "List", widget: scroll(0.0, false), style: (size: (60, 20)), clip: true, children: [
                (name: "Tall", style: (size: (60, 400))),
            ]),
            (name: "Badge", children: [(name: "Star", text: (spans: ["!"]))]),
        ]),
        (name: "P3", style: (direction: row, gap: (4, 4)), children: [
            (name: "Caption", text: (spans: ["Three"])),
            (name: "Who", text: (spans: ["-"]), bind: (source: "Party.p3.name")),
            (name: "Fill", style: (size: (200, 10)), background: "#30C060"),
            (name: "Act", widget: button, children: [(text: (spans: ["Go"]))]),
            (name: "List", widget: scroll(0.0, false), style: (size: (60, 20)), clip: true, children: [
                (name: "Tall", style: (size: (60, 400))),
            ]),
            (name: "Badge", children: [(name: "DefaultBadge", text: (spans: ["*"]))]),
        ]),
    ],
))"##;

fn ui_app(project: Option<&Path>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    if let Some(root) = project {
        app.add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: root.to_path_buf(),
                platform: "linux".into(),
                content_dir: None,
            },
        });
    }
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.register_view_model::<Party>();
    app.update();
    app.world_mut().insert_resource(Party {
        p1: Member {
            name: "Ada".into(),
            hp: 1.0,
        },
        p2: Member {
            name: "Bo".into(),
            hp: 0.5,
        },
        p3: Member {
            name: "Cy".into(),
            hp: 0.25,
        },
    });
    app
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn project(files: &[(&str, &str)]) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("ui-components");
    for (rel, text) in files {
        write(&root.join("assets").join(rel), text);
    }
    root
}

fn load(
    app: &mut App,
    name: &str,
    rel: &str,
    text: &str,
) -> Vec<regia_ui::ui_component::ComponentDiagnostic> {
    let file = parse_ui_ron(text, rel, &UiMigrators::default()).expect("parses");
    insert_ui_template(app.world_mut(), name, rel, file)
}

fn named(app: &mut App, name: &str) -> Vec<Entity> {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    let mut out: Vec<Entity> = q
        .iter(world)
        .filter(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .collect();
    out.sort_by_key(|e| e.to_bits());
    out
}

fn text(app: &App, entity: Entity) -> String {
    match &app.world().get::<Text>(entity).expect("text").spans[0].content {
        TextContent::Lit(s) => s.clone(),
        TextContent::Loc { key, .. } => format!("loc:{key}"),
    }
}

fn key_map(app: &App, root: Entity) -> Vec<(String, Entity)> {
    ui_node_keys(app.world(), root)
}

fn entity_at(app: &App, root: Entity, key: &str) -> Entity {
    key_map(app, root)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, e)| e)
        .unwrap_or_else(|| panic!("no node {key}"))
}

/// Every node's key, rect and text, in tree order.
fn snapshot(app: &App, root: Entity) -> Vec<(String, [f32; 4], Option<String>)> {
    key_map(app, root)
        .into_iter()
        .map(|(k, e)| {
            let rect = app.world().get::<ComputedNode>(e).map_or([0.0; 4], |c| c.rect);
            let t = app.world().get::<Text>(e).map(|_| text(app, e));
            (k, rect, t)
        })
        .collect()
}

/// One component used three times with different parameters and bind roots expands to the tree
/// written by hand, binds each instance to its own member, and lays out identically.
#[test]
fn three_instances_match_the_hand_written_tree_and_layout() {
    let root = project(&[("ui/bar.ui.ron", BAR), ("ui/hud.ui.ron", HUD)]);
    let mut app = ui_app(Some(&root));
    let diags = load(&mut app, "hud", "ui/hud.ui.ron", HUD);
    assert!(diags.is_empty(), "{diags:?}");
    load(&mut app, "inline", "ui/inline.ui.ron", INLINE);
    {
        let library = app.world().resource::<UiLibrary>();
        assert_eq!(
            library.templates["hud"].root, library.templates["inline"].root,
            "expanded tree = the hand-written tree"
        );
        assert_eq!(library.users_of("ui/bar.ui.ron"), ["hud"]);
        let (_, source) = library.source("hud").expect("source kept");
        assert_eq!(
            source.root.children[0].use_component.as_deref(),
            Some("ui/bar.ui.ron"),
            "the source keeps instances as references"
        );
    }
    let hud = spawn_ui(app.world_mut(), "hud").expect("spawn hud");
    app.update();
    app.update();
    let who: Vec<String> = named(&mut app, "Who")
        .into_iter()
        .map(|e| text(&app, e))
        .collect();
    assert_eq!(who.len(), 3);
    for name in ["Ada", "Bo", "Cy"] {
        assert!(who.contains(&name.to_owned()), "{name} bound: {who:?}");
    }
    assert_eq!(text(&app, entity_at(&app, hud, "P1/Who")), "Ada");
    assert_eq!(text(&app, entity_at(&app, hud, "P3/Caption")), "Three");
    assert_eq!(text(&app, entity_at(&app, hud, "P2/Badge/Star")), "!");
    let fills: Vec<f32> = ["P1/Fill", "P2/Fill", "P3/Fill"]
        .iter()
        .map(|k| {
            let c = app
                .world()
                .get::<ComputedNode>(entity_at(&app, hud, k))
                .expect("laid out");
            c.rect[2] / c.scale
        })
        .collect();
    // Rects are snapped to physical pixels at a fractional scale, so each width is within one UI px
    // of the parameter; the exact check is the identity with the hand-written layout below.
    for (got, want) in fills.iter().zip([100.0_f32, 150.0, 200.0]) {
        assert!((got - want).abs() <= 1.0, "fill widths {fills:?}");
    }

    // The hand-written form in its own app: the same keys, rects and texts.
    let mut other = ui_app(Some(&root));
    load(&mut other, "inline", "ui/inline.ui.ron", INLINE);
    let inline = spawn_ui(other.world_mut(), "inline").expect("spawn inline");
    other.update();
    other.update();
    assert_eq!(snapshot(&app, hud), snapshot(&other, inline));
}

/// A slot's content lands in the outlet node; an instance without content keeps the default.
#[test]
fn slot_children_land_in_the_outlet() {
    let root = project(&[("ui/bar.ui.ron", BAR)]);
    let mut app = ui_app(Some(&root));
    load(&mut app, "hud", "ui/hud.ui.ron", HUD);
    let hud = spawn_ui(app.world_mut(), "hud").unwrap();
    let star = entity_at(&app, hud, "P2/Badge/Star");
    let parent = app.world().get::<ChildOf>(star).expect("parent").parent();
    assert_eq!(app.world().get::<NodeName>(parent).unwrap().0, "Badge");
    let grand = app.world().get::<ChildOf>(parent).unwrap().parent();
    assert_eq!(app.world().get::<NodeName>(grand).unwrap().0, "P2");
    assert_eq!(
        named(&mut app, "DefaultBadge").len(),
        2,
        "P1 and P3 keep the default content"
    );
    assert_eq!(named(&mut app, "Star").len(), 1);
}

/// A two-file cycle and a nine-deep chain give their documented codes and a visible placeholder;
/// a missing file and an unknown parameter too. Nothing panics.
#[test]
fn cycles_depth_and_missing_files_give_placeholders() {
    let a = r#"(version: 9, root: (name: "A", children: [(use: "ui/b.ui.ron")]))"#;
    let b = r#"(version: 9, root: (name: "B", children: [(use: "ui/a.ui.ron")]))"#;
    let mut files: Vec<(String, String)> =
        vec![("ui/a.ui.ron".into(), a.into()), ("ui/b.ui.ron".into(), b.into())];
    for i in 0..10 {
        let body = if i < 9 {
            format!(
                r#"(version: 9, root: (name: "C{i}", children: [(use: "ui/c{}.ui.ron")]))"#,
                i + 1
            )
        } else {
            r#"(version: 9, root: (name: "C9"))"#.to_owned()
        };
        files.push((format!("ui/c{i}.ui.ron"), body));
    }
    let list: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let root = project(&list);
    let mut app = ui_app(Some(&root));
    let cycle = load(&mut app, "a", "ui/a.ui.ron", a);
    assert_eq!(cycle.len(), 1, "{cycle:?}");
    assert_eq!(cycle[0].code, "ui_component_cycle");
    let deep = load(
        &mut app,
        "deep",
        "ui/deep.ui.ron",
        r#"(version: 9, root: (children: [(use: "ui/c0.ui.ron")]))"#,
    );
    assert_eq!(deep.len(), 1, "{deep:?}");
    assert_eq!(deep[0].code, "ui_component_depth");
    let missing = load(
        &mut app,
        "missing",
        "ui/missing.ui.ron",
        r#"(version: 9, root: (children: [(use: "ui/nope.ui.ron"), (use: "ui/c9.ui.ron", params: {"x": 1})]))"#,
    );
    let codes: Vec<&str> = missing.iter().map(|d| d.code).collect();
    assert_eq!(codes, ["ui_component_missing", "ui_component_unknown_param"]);
    assert_eq!(
        app.world()
            .resource::<UiLibrary>()
            .diagnostics_of("missing")
            .len(),
        2
    );
    for name in ["a", "deep", "missing"] {
        spawn_ui(app.world_mut(), name).expect("spawns despite errors");
    }
    app.update();
    let placeholders = named(&mut app, PLACEHOLDER_NAME);
    assert_eq!(
        placeholders.len(),
        4,
        "one visible placeholder per failing instance"
    );
    for e in placeholders {
        let t = text(&app, e);
        assert!(t.starts_with("ui_component_"), "{t}");
        assert!(
            app.world().get::<regia_ui::node::Background>(e).is_some(),
            "visible fill"
        );
    }
}

/// Editing the component file reaches every instance of every template that uses it; the
/// focused node and a scrolled view keep their state by key.
#[test]
fn a_component_edit_reaches_every_instance_with_focus_and_scroll_kept() {
    let root = project(&[("ui/bar.ui.ron", BAR), ("ui/hud.ui.ron", HUD)]);
    let mut app = ui_app(Some(&root));
    load(&mut app, "hud", "ui/hud.ui.ron", HUD);
    let hud = spawn_ui(app.world_mut(), "hud").unwrap();
    app.update();
    let act = entity_at(&app, hud, "P2/Act");
    app.world_mut().resource_mut::<FocusState>().focused = Some(act);
    let list = entity_at(&app, hud, "P3/List");
    app.world_mut().get_mut::<ScrollOffset>(list).unwrap().0 = [0.0, 30.0];

    write(
        &root.join("assets/ui/bar.ui.ron"),
        &BAR.replace(
            r#"(name: "Caption", text: (spans: ["?"])),"#,
            r#"(name: "Caption", text: (spans: ["?"])), (name: "Extra"),"#,
        ),
    );
    app.world_mut()
        .resource_mut::<regia_ui::authoring::PendingUiReloads>()
        .push(root.join("assets/ui/bar.ui.ron"));
    regia_ui::authoring::apply_ui_reloads(app.world_mut());

    let roots: Vec<Entity> = {
        let world = app.world_mut();
        let mut q = world.query_filtered::<Entity, With<regia_ui::UiTemplate>>();
        q.iter(world).collect()
    };
    assert_eq!(roots.len(), 1);
    let fresh = roots[0];
    assert_ne!(fresh, hud, "respawned");
    assert_eq!(
        named(&mut app, "Extra").len(),
        3,
        "every instance has the new node"
    );
    let focused = app.world().resource::<FocusState>().focused.expect("focus kept");
    assert_eq!(focused, entity_at(&app, fresh, "P2/Act"));
    let list = entity_at(&app, fresh, "P3/List");
    assert_eq!(app.world().get::<ScrollOffset>(list).unwrap().0, [0.0, 30.0]);
    // A frame later the tree still binds.
    app.update();
    assert_eq!(text(&app, entity_at(&app, fresh, "P2/Who")), "Bo");
}

/// A `use` node built in code (never through the library) still expands when spawned.
#[test]
fn an_unexpanded_instance_expands_at_spawn() {
    let root = project(&[("ui/bar.ui.ron", BAR)]);
    let mut app = ui_app(Some(&root));
    let world = app.world_mut();
    let parent = world.spawn((UiRoot::default(), NodeStyle::default())).id();
    let def = regia_ui::authoring::UiNodeDef {
        use_component: Some("ui/bar.ui.ron".into()),
        name: Some("Coded".into()),
        bind_root: Some("Party.p1".into()),
        ..Default::default()
    };
    regia_ui::authoring::spawn_node(world, parent, &def);
    assert_eq!(named(&mut app, "Coded").len(), 1);
    assert_eq!(named(&mut app, "Caption").len(), 1);
    assert!(named(&mut app, PLACEHOLDER_NAME).is_empty());
}

/// A component reached only through another component is a dependency too (§ 4.1): editing it
/// re-expands and respawns the template, and a focused node whose key the edit removed loses the
/// focus instead of pointing at a despawned entity.
#[test]
fn an_edit_reaches_templates_through_nested_components_and_drops_lost_keys() {
    let frame = r#"(version: 9, root: (name: "Frame", children: [
        (use: "ui/bar.ui.ron", name: "Inner", bind_root: "Party.p1"),
    ]))"#;
    let top = r#"(version: 9, root: (children: [(use: "ui/frame.ui.ron", name: "F")]))"#;
    let root = project(&[("ui/bar.ui.ron", BAR), ("ui/frame.ui.ron", frame)]);
    let mut app = ui_app(Some(&root));
    let diags = load(&mut app, "top", "ui/top.ui.ron", top);
    assert!(diags.is_empty(), "{diags:?}");
    {
        let library = app.world().resource::<UiLibrary>();
        assert_eq!(library.users_of("ui/frame.ui.ron"), ["top"]);
        assert_eq!(
            library.users_of("ui/bar.ui.ron"),
            ["top"],
            "an indirect use counts"
        );
    }
    let spawned = spawn_ui(app.world_mut(), "top").unwrap();
    app.update();
    let act = entity_at(&app, spawned, "F/Inner/Act");
    app.world_mut().resource_mut::<FocusState>().focused = Some(act);

    // The edit adds a node and removes the focused one.
    let edited = BAR
        .replace(
            r#"(name: "Caption", text: (spans: ["?"])),"#,
            r#"(name: "Caption", text: (spans: ["?"])), (name: "Extra"),"#,
        )
        .replace(
            r#"(name: "Act", widget: button, children: [(text: (spans: ["Go"]))]),"#,
            "",
        );
    assert_ne!(edited, BAR);
    write(&root.join("assets/ui/bar.ui.ron"), &edited);
    app.world_mut()
        .resource_mut::<regia_ui::authoring::PendingUiReloads>()
        .push(root.join("assets/ui/bar.ui.ron"));
    regia_ui::authoring::apply_ui_reloads(app.world_mut());

    assert_eq!(
        named(&mut app, "Extra").len(),
        1,
        "the nested instance re-expanded"
    );
    assert!(named(&mut app, "Act").is_empty());
    let fresh: Vec<Entity> = {
        let world = app.world_mut();
        let mut q = world.query_filtered::<Entity, With<regia_ui::UiTemplate>>();
        q.iter(world).collect()
    };
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0], spawned, "respawned");
    assert_eq!(
        app.world().resource::<FocusState>().focused,
        None,
        "a key that no longer exists is dropped"
    );
}
