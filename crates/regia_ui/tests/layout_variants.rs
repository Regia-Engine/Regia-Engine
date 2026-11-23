// SPDX-License-Identifier: MPL-2.0
//! Layout variants (P-271, addendum § 13.6, ADR-7858): a `.ui.ron` document's `variants` override
//! base fields per window orientation and size; the first matching variant applies on top of the base
//! when the window changes, re-laid out once. Headless: fixed 16 ms steps, no GPU, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::layout_variants::{
    ActiveLayoutVariant, LayoutVariant, Orientation, VariantWhen, apply_variant, select_variant,
};
use regia_ui::prelude::*;
use regia_ui::{NodeName, UiMigrators, UiScales, UiTarget, parse_ui_ron, write_ui_ron};

/// A landscape menu whose portrait variant moves the Play button down and hides the logo.
const DOC: &str = r#"(
    version: 11,
    root: (
        style: (size: (Percent(100.0), Percent(100.0))),
        children: [
            (
                name: "play",
                interactable: true,
                style: (position: absolute, inset: (left: 40.0, top: 40.0), size: (200.0, 60.0)),
            ),
            (
                name: "logo",
                style: (position: absolute, inset: (left: 400.0, top: 40.0), size: (100.0, 100.0)),
            ),
        ],
    ),
    variants: [
        (
            name: "phone portrait",
            when: (orientation: portrait, max_width: 1200.0),
            overrides: {
                "play": { "style.inset": [40.0, 600.0, "Auto", "Auto"] },
                "logo": { "style.display": "none" },
            },
        ),
        (
            name: "tablet portrait",
            when: (orientation: portrait),
            overrides: {
                "play": { "style.inset": [40.0, 900.0, "Auto", "Auto"] },
            },
        ),
    ],
)
"#;

fn ui_app(size: [f32; 2]) -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.update();
    app.world_mut().insert_resource(UiScales {
        window_size: size,
        scale_factor: 1.0,
    });
    app
}

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

fn named(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    q.iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

/// Whether the node's top edge is at `want` UI px: the default scale mode scales with the window and
/// layout snaps to whole physical px, so within one physical px.
fn top_at(app: &mut App, name: &str, want: f32) -> bool {
    let e = named(app, name);
    let c = app.world().get::<ComputedNode>(e).unwrap();
    let ok = (c.rect[1] - want * c.scale).abs() <= 1.0;
    if !ok {
        eprintln!(
            "{name}: top {} px at scale {}, want {want} UI px",
            c.rect[1], c.scale
        );
    }
    ok
}

fn root_of(app: &mut App) -> Entity {
    let world = app.world_mut();
    let mut q = world.query_filtered::<Entity, (With<UiRoot>, With<regia_ui::authoring::UiTemplate>)>();
    let roots: Vec<Entity> = q.iter(world).collect();
    assert_eq!(roots.len(), 1, "one template root");
    roots[0]
}

fn spawn_doc(app: &mut App) {
    let file = parse_ui_ron(DOC, "menu.ui.ron", &UiMigrators::default()).unwrap();
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("menu".to_owned(), file);
    spawn_ui(app.world_mut(), "menu").unwrap();
    settle(app);
}

#[test]
fn the_document_parses_and_writes_back_with_its_variants() {
    let file = parse_ui_ron(DOC, "menu.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(file.version, 11, "variants fold into v11 (ADR-7827, ADR-7858)");
    assert_eq!(file.variants.len(), 2);
    let phone = &file.variants[0];
    assert_eq!(phone.name.as_deref(), Some("phone portrait"));
    assert_eq!(phone.when.orientation, Some(Orientation::Portrait));
    assert_eq!(phone.when.max_width, Some(1200.0));
    assert_eq!(
        phone.overrides["play"]["style.inset"],
        serde_json::json!([40.0, 600.0, "Auto", "Auto"])
    );
    let written = write_ui_ron(&file).unwrap();
    let again = parse_ui_ron(&written, "again.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(
        again, file,
        "write then parse gives the same document:\n{written}"
    );
    assert_eq!(write_ui_ron(&again).unwrap(), written, "the writer is stable");
    // A document without variants writes no `variants` key.
    let plain = regia_ui::UiFile {
        variants: Vec::new(),
        ..file
    };
    assert!(!write_ui_ron(&plain).unwrap().contains("variants"));
}

#[test]
fn the_first_matching_variant_wins() {
    let file = parse_ui_ron(DOC, "menu.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(
        select_variant(&file.variants, [1920.0, 1080.0]),
        None,
        "landscape: the base"
    );
    assert_eq!(
        select_variant(&file.variants, [1080.0, 2400.0]),
        Some(0),
        "phone portrait"
    );
    assert_eq!(
        select_variant(&file.variants, [1600.0, 2560.0]),
        Some(1),
        "wider than 1200: tablet"
    );
    // A square window is landscape (portrait means taller than wide).
    assert_eq!(select_variant(&file.variants, [1000.0, 1000.0]), None);
    let when = VariantWhen {
        min_aspect: Some(1.5),
        max_aspect: Some(2.0),
        min_width: Some(800.0),
        ..VariantWhen::default()
    };
    assert!(when.matches([1920.0, 1080.0]), "16:9 is inside 1.5..=2.0");
    assert!(!when.matches([1280.0, 1024.0]), "5:4 is below 1.5");
    assert!(!when.matches([700.0, 400.0]), "narrower than 800");
    assert!(
        VariantWhen::default().matches([1.0, 1.0]),
        "an empty `when` always matches"
    );
}

#[test]
fn apply_variant_overrides_named_fields_and_leaves_the_rest() {
    let file = parse_ui_ron(DOC, "menu.ui.ron", &UiMigrators::default()).unwrap();
    let (root, diagnostics) = apply_variant(&file.root, &file.variants[0]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let play = &root.children[0];
    assert_eq!(play.style.inset.top, Dimension::Points(600.0));
    assert_eq!(
        play.style.inset.left,
        Dimension::Points(40.0),
        "other fields stay"
    );
    assert_eq!(root.children[1].style.display, Display::None);
    assert_eq!(
        file.root.children[0].style.inset.top,
        Dimension::Points(40.0),
        "the base is untouched"
    );

    // Unknown nodes and bad values are reported, never fatal; the rest still applies.
    let bad = LayoutVariant {
        overrides: [
            (
                "nobody".to_owned(),
                [("opacity".to_owned(), serde_json::json!(0.5))].into(),
            ),
            (
                "play".to_owned(),
                [
                    ("opacity".to_owned(), serde_json::json!("very")),
                    ("z".to_owned(), serde_json::json!(4)),
                ]
                .into(),
            ),
        ]
        .into(),
        ..LayoutVariant::default()
    };
    let (root, diagnostics) = apply_variant(&file.root, &bad);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert_eq!(root.children[0].z, Some(4));
    // A node path addresses an unnamed node; `children` and `name` are not overridable.
    let by_path = LayoutVariant {
        overrides: [
            (
                "/1".to_owned(),
                [("opacity".to_owned(), serde_json::json!(0.25))].into(),
            ),
            (
                "/0".to_owned(),
                [("children".to_owned(), serde_json::json!([]))].into(),
            ),
        ]
        .into(),
        ..LayoutVariant::default()
    };
    let (root, diagnostics) = apply_variant(&file.root, &by_path);
    assert_eq!(root.children[1].opacity, Some(0.25));
    assert_eq!(diagnostics.len(), 1, "children refused: {diagnostics:?}");
}

#[test]
fn resizing_from_landscape_to_portrait_applies_the_variant_once() {
    let mut app = ui_app([1920.0, 1080.0]);
    spawn_doc(&mut app);
    assert!(top_at(&mut app, "play", 40.0), "landscape: the base");
    let root = root_of(&mut app);
    assert_eq!(app.world().get::<ActiveLayoutVariant>(root).unwrap().index, None);

    app.world_mut().resource_mut::<UiScales>().window_size = [1080.0, 2400.0];
    settle(&mut app);
    let root = root_of(&mut app);
    let active = app.world().get::<ActiveLayoutVariant>(root).unwrap().clone();
    assert_eq!(active.index, Some(0));
    assert_eq!(active.name.as_deref(), Some("phone portrait"));
    assert!(
        top_at(&mut app, "play", 600.0),
        "the portrait variant moved the button"
    );
    let logo = named(&mut app, "logo");
    assert_eq!(
        app.world().get::<NodeStyle>(logo).unwrap().0.display,
        Display::None
    );
    let changes = app
        .world()
        .resource::<regia_ui::layout_variants::LayoutVariantStats>()
        .respawns;
    assert_eq!(changes, 1, "one re-layout for one change");

    // Further frames at the same size change nothing.
    settle(&mut app);
    assert_eq!(
        app.world()
            .resource::<regia_ui::layout_variants::LayoutVariantStats>()
            .respawns,
        1
    );

    // A resize inside the same variant keeps the tree; back to landscape restores the base.
    app.world_mut().resource_mut::<UiScales>().window_size = [1000.0, 2200.0];
    settle(&mut app);
    assert_eq!(
        app.world()
            .resource::<regia_ui::layout_variants::LayoutVariantStats>()
            .respawns,
        1
    );
    app.world_mut().resource_mut::<UiScales>().window_size = [1920.0, 1080.0];
    settle(&mut app);
    assert!(top_at(&mut app, "play", 40.0));
    let root = root_of(&mut app);
    assert_eq!(app.world().get::<ActiveLayoutVariant>(root).unwrap().index, None);
}

#[test]
fn the_variant_reads_logical_window_px() {
    // 2160 x 4800 physical at OS scale 2 is 1080 x 2400 logical: the phone variant.
    let mut app = ui_app([2160.0, 4800.0]);
    app.world_mut().resource_mut::<UiScales>().scale_factor = 2.0;
    spawn_doc(&mut app);
    let root = root_of(&mut app);
    assert_eq!(
        app.world().get::<ActiveLayoutVariant>(root).unwrap().index,
        Some(0)
    );
}

#[test]
fn the_variant_is_applied_at_spawn_and_keeps_focus_across_a_change() {
    let mut app = ui_app([1080.0, 2400.0]);
    spawn_doc(&mut app);
    assert!(
        top_at(&mut app, "play", 600.0),
        "spawned straight into the variant"
    );
    let play = named(&mut app, "play");
    app.world_mut()
        .resource_mut::<regia_ui::focus::FocusState>()
        .focused = Some(play);
    app.world_mut().resource_mut::<UiScales>().window_size = [1920.0, 1080.0];
    settle(&mut app);
    let play = named(&mut app, "play");
    assert_eq!(
        app.world().resource::<regia_ui::focus::FocusState>().focused,
        Some(play),
        "focus follows the node across the re-layout"
    );
}

const FIXTURE: &str = include_str!("fixtures/ui.v11-variants.example.ui.ron");
const CORPUS: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/ui/v11-variants.ui.ron");

#[test]
fn the_variants_fixture_is_the_frozen_corpus_copy_and_every_variant_applies() {
    assert_eq!(FIXTURE, CORPUS, "the corpus file is a frozen copy of the fixture");
    let file = parse_ui_ron(FIXTURE, "v11-variants.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(file.variants.len(), 3);
    assert_eq!(select_variant(&file.variants, [1080.0, 2400.0]), Some(0));
    assert_eq!(
        select_variant(&file.variants, [3440.0, 1440.0]),
        Some(1),
        "21:9 is ultrawide"
    );
    assert_eq!(
        select_variant(&file.variants, [3840.0, 2160.0]),
        Some(2),
        "4K 16:9"
    );
    assert_eq!(select_variant(&file.variants, [1920.0, 1080.0]), None);
    for variant in &file.variants {
        let (_, problems) = apply_variant(&file.root, variant);
        assert!(problems.is_empty(), "{problems:?}");
    }
    let (root, _) = apply_variant(&file.root, &file.variants[1]);
    assert_eq!(
        root.children[0].style.inset.left,
        Dimension::Points(200.0),
        "by path"
    );
    let written = write_ui_ron(&file).unwrap();
    assert_eq!(
        parse_ui_ron(&written, "w.ui.ron", &UiMigrators::default()).unwrap(),
        file
    );
}

#[test]
fn ui_inspect_reports_the_variant_the_root_shows() {
    let mut app = ui_app([1920.0, 1080.0]);
    spawn_doc(&mut app);
    let play = named(&mut app, "play");
    let base = regia_ui::inspect::inspect(app.world_mut(), play);
    assert_eq!(base["layout_variant"]["template"], "menu");
    assert_eq!(
        base["layout_variant"]["index"],
        serde_json::Value::Null,
        "the base"
    );
    assert_eq!(base["layout_variant"]["variants"], 2);

    app.world_mut().resource_mut::<UiScales>().window_size = [1080.0, 2400.0];
    settle(&mut app);
    let play = named(&mut app, "play");
    let shown = regia_ui::inspect::inspect(app.world_mut(), play);
    let v = &shown["layout_variant"];
    assert_eq!(v["index"], 0);
    assert_eq!(v["name"], "phone portrait");
    assert_eq!(v["node_overridden"], true);
    assert_eq!(v["nodes"], serde_json::json!(["logo", "play"]));
    assert_eq!(v["problems"], serde_json::json!([]));
}

/// A window root and a world-space root share one template (review g17 row 11): a window resize
/// that picks another variant re-lays out the window root only. The world root keeps its entity and
/// the variant it spawned with (ADR-7858: camera-target and world roots keep theirs).
#[test]
fn a_window_resize_respawns_only_the_window_root_of_a_shared_template() {
    let mut app = ui_app([1920.0, 1080.0]);
    spawn_doc(&mut app);
    let window_root = root_of(&mut app);
    let world_root = spawn_ui(app.world_mut(), "menu").unwrap();
    app.world_mut().get_mut::<UiRoot>(world_root).unwrap().target = UiTarget::WorldSpace {
        size: [800.0, 600.0],
        ppm: 100.0,
    };
    settle(&mut app);
    let world_variant = app
        .world()
        .get::<ActiveLayoutVariant>(world_root)
        .unwrap()
        .clone();
    assert_eq!(
        world_variant.index, None,
        "spawned at a landscape window: the base"
    );

    app.world_mut().resource_mut::<UiScales>().window_size = [1080.0, 2400.0];
    settle(&mut app);
    assert!(
        app.world().get_entity(world_root).is_ok(),
        "the world root was not despawned"
    );
    assert_eq!(
        app.world().get::<UiRoot>(world_root).unwrap().target,
        UiTarget::WorldSpace {
            size: [800.0, 600.0],
            ppm: 100.0,
        }
    );
    assert_eq!(
        app.world().get::<ActiveLayoutVariant>(world_root).unwrap(),
        &world_variant,
        "the world root keeps the variant it spawned with"
    );
    assert!(
        app.world().get_entity(window_root).is_err(),
        "the window root was re-laid out (respawned)"
    );
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &UiRoot, &ActiveLayoutVariant)>();
    let windows: Vec<Option<usize>> = q
        .iter(world)
        .filter(|(e, r, _)| *e != world_root && matches!(r.target, UiTarget::Window(_)))
        .map(|(_, _, a)| a.index)
        .collect();
    assert_eq!(windows, [Some(0)], "one window root, now on the phone variant");
    assert_eq!(
        app.world()
            .resource::<regia_ui::layout_variants::LayoutVariantStats>()
            .respawns,
        1,
        "one root re-laid out"
    );
}
