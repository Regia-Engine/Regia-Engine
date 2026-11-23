// SPDX-License-Identifier: MPL-2.0
//! UI designer acceptance (v0.5 ledger N09): a layout built from an empty document over the real
//! editor socket. It covers the hierarchy (add, move, duplicate, remove, with every step undone
//! and redone), properties (checked through the layout they produce), bindings and a converter
//! (checked through sample values), lint, and the multi-size, multi-locale preview. The saved file
//! is reopened by a fresh editor process and then laid out by the runtime `spawn_ui` outside the
//! editor: every named rect must equal the designer's preview. No GPU, no golden image, no timing.

use super::*;
use regia_test_utils::write_atomic;

const MENU: &str = "assets/ui/menu.ui.ron";
const EN: &str = "menu-title = Main menu\nmenu-play = Play\nmenu-quit = Quit\n";
const FR: &str = "menu-title = Menu principal\nmenu-play = Jouer\nmenu-quit = Quitter\n";
/// Preset names and one custom size, with the insets the presets carry (left, top, right, bottom).
const SIZES: [(&str, [f64; 2], [f64; 4]); 5] = [
    ("1920x1080", [1920.0, 1080.0], [0.0; 4]),
    ("1280x720", [1280.0, 720.0], [0.0; 4]),
    ("2400x1080", [2400.0, 1080.0], [110.0, 0.0, 60.0, 0.0]),
    ("1080x2400", [1080.0, 2400.0], [0.0, 110.0, 0.0, 60.0]),
    ("custom", [800.0, 600.0], [0.0; 4]),
];

fn designer_fixture() -> Fixture {
    let f = Fixture::new("designer-acceptance");
    for (locale, text) in [("en", EN), ("fr", FR)] {
        let path = f.0.join(format!("assets/localization/{locale}/main.ftl"));
        std::fs::create_dir_all(path.parent().expect("locale dir")).expect("locale dir");
        write_atomic(&path, text.as_bytes()).expect("ftl");
    }
    f
}

fn start_designer(f: &Fixture) -> (Editor, Client) {
    let editor = Editor::start(f);
    editor.event("ready", f);
    let client = connect(f);
    (editor, client)
}

fn shutdown(mut editor: Editor, mut client: Client, f: &Fixture) {
    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    editor.event("exit", f);
    assert_eq!(editor.wait().code(), Some(0), "{}", f.log());
}

/// The hierarchy as (path, name, "kind bindings command") rows.
fn tree(client: &mut Client) -> Tree {
    let state = call(client, "editor.ui_designer_state", json!({}));
    state["tree"]
        .as_array()
        .expect("designer tree")
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap_or_default().to_owned(),
                row["name"].as_str().unwrap_or_default().to_owned(),
                // Kind, bindings and command: binding and command edits show in the walk too.
                format!("{} {} {}", row["kind"], row["bindings"], row["command"]),
            )
        })
        .collect()
}

fn undo_depth(client: &mut Client) -> u64 {
    call(client, "editor.ui_designer_state", json!({}))["undo"]
        .as_u64()
        .expect("undo count")
}

fn edit(client: &mut Client, params: Value) -> Value {
    call(client, "editor.ui_designer_edit", params)
}

fn set(client: &mut Client, node: &str, key: &str, value: Value) {
    edit(
        client,
        json!({"op": "set", "node": node, "key": key, "value": value}),
    );
}

type Tree = Vec<(String, String, String)>;

/// Keeps the hierarchy after an edit and checks the edit was exactly one undo step.
fn record(client: &mut Client, steps: &mut Vec<Tree>) {
    steps.push(tree(client));
    assert_eq!(
        undo_depth(client),
        (steps.len() - 1) as u64,
        "every edit is one undo step"
    );
}

/// Adds a preset under `parent` and names it (two recorded steps); returns its path.
fn add(client: &mut Client, steps: &mut Vec<Tree>, parent: &str, preset: &str, name: &str) -> String {
    let added = edit(client, json!({"op": "add", "parent": parent, "preset": preset}));
    record(client, steps);
    let path = added["selection"]
        .as_str()
        .unwrap_or_else(|| panic!("the added node is selected: {added}"))
        .to_owned();
    set(client, &path, "name", json!(name));
    record(client, steps);
    path
}

fn lint_codes(client: &mut Client) -> (u64, Vec<String>) {
    let lint = call(client, "editor.ui_designer_lint", json!({}));
    let codes = lint["findings"]
        .as_array()
        .expect("lint findings")
        .iter()
        .map(|f| f["code"].as_str().unwrap_or_default().to_owned())
        .collect();
    (lint["errors"].as_u64().expect("lint error count"), codes)
}

/// Builds the menu step by step. Returns the hierarchy after every step (index 0: the empty
/// document).
fn author_menu(client: &mut Client) -> Vec<Tree> {
    let opened = call(
        client,
        "editor.ui_designer_open",
        json!({"new": true, "path": MENU}),
    );
    assert_eq!(opened["open"], true, "{opened}");
    let mut steps = vec![tree(client)];
    assert_eq!(steps[0].len(), 1, "a new document is the root alone");
    set(client, "/", "style.direction", json!("column"));
    record(client, &mut steps);
    set(
        client,
        "/",
        "style.size",
        json!([{"Percent": 100}, {"Percent": 100}]),
    );
    record(client, &mut steps);

    // Header: a full-width bar with the localized title.
    let header = add(client, &mut steps, "/", "panel", "header");
    assert_eq!(header, "/0");
    set(client, "#header", "style.direction", json!("column"));
    record(client, &mut steps);
    set(client, "#header", "style.size", json!([{"Percent": 100}, 80]));
    record(client, &mut steps);
    let title = add(client, &mut steps, "#header", "text", "title");
    assert_eq!(title, "/0/0");
    set(
        client,
        "#title",
        "text",
        json!({"spans": [{"Loc": "menu-title"}]}),
    );
    record(client, &mut steps);
    set(client, "#title", "style.size", json!([{"Percent": 100}, 40]));
    record(client, &mut steps);

    // Bound values: a formatted score and a grouped number through a converter.
    let score = add(client, &mut steps, "/", "text", "score");
    assert_eq!(score, "/1");
    set(client, "#score", "text", json!({"spans": ["0"]}));
    record(client, &mut steps);
    set(client, "#score", "style.size", json!([120, 32]));
    record(client, &mut steps);
    set(
        client,
        "#score",
        "bindings",
        json!([{"source": "HudVm.score", "target": "text", "format": "{:03}"}]),
    );
    record(client, &mut steps);
    add(client, &mut steps, "/", "text", "coins");
    set(client, "#coins", "style.size", json!([200, 32]));
    record(client, &mut steps);
    set(
        client,
        "#coins",
        "bindings",
        json!([{"source": "HudVm.coins", "target": "text"}]),
    );
    record(client, &mut steps);
    edit(
        client,
        json!({"op": "converter", "node": "#coins",
        "converter": {"number": {"precision": 2, "group": ","}}}),
    );
    record(client, &mut steps);
    add(client, &mut steps, "/", "progress", "health");
    set(client, "#health", "style.size", json!([300, 20]));
    record(client, &mut steps);
    set(
        client,
        "#health",
        "bindings",
        json!([{"source": "HudVm.health", "target": "progress"}]),
    );
    record(client, &mut steps);

    // Footer row: two auto-width buttons whose widths come from their localized labels.
    let footer = add(client, &mut steps, "/", "panel", "footer");
    set(client, "#footer", "style.direction", json!("row"));
    record(client, &mut steps);
    set(client, "#footer", "style.size", json!([{"Percent": 100}, 48]));
    record(client, &mut steps);
    for (name, key) in [("play", "menu-play"), ("quit", "menu-quit")] {
        add(client, &mut steps, &footer, "button", name);
        set(
            client,
            &format!("#{name}"),
            "text",
            json!({"spans": [{"Loc": key}]}),
        );
        record(client, &mut steps);
        set(client, &format!("#{name}"), "style.size", json!(["Auto", "Auto"]));
        record(client, &mut steps);
    }
    set(client, "#play", "command", json!("menu.play"));
    record(client, &mut steps);

    // Hierarchy edits: move, duplicate (a duplicate name the lint reports), remove.
    let names = |rows: &[(String, String, String)]| -> Vec<String> {
        rows.iter().map(|r| format!("{} {}", r.0, r.1)).collect()
    };
    edit(
        client,
        json!({"op": "move", "node": "#health", "parent": "/", "index": 1}),
    );
    record(client, &mut steps);
    let moved = names(steps.last().expect("tree"));
    assert!(moved.contains(&"/1 health".to_owned()), "{moved:?}");
    assert!(moved.contains(&"/2 score".to_owned()), "{moved:?}");
    let (errors, codes) = lint_codes(client);
    assert_eq!((errors, codes.clone()), (0, Vec::<String>::new()), "{codes:?}");
    edit(client, json!({"op": "duplicate", "node": "#quit"}));
    record(client, &mut steps);
    let footer_path = steps
        .last()
        .expect("tree")
        .iter()
        .find(|r| r.1 == "footer")
        .expect("footer row")
        .0
        .clone();
    let duplicated = steps
        .last()
        .expect("tree")
        .iter()
        .filter(|r| r.1 == "quit")
        .count();
    assert_eq!(duplicated, 2);
    let (_, codes) = lint_codes(client);
    assert!(codes.contains(&"duplicate_name".to_owned()), "{codes:?}");
    edit(
        client,
        json!({"op": "remove", "node": format!("{footer_path}/2")}),
    );
    record(client, &mut steps);
    assert_eq!(
        steps.last(),
        steps.get(steps.len() - 3),
        "the remove undid the duplicate"
    );

    // A wrong binding target is a lint error and undoes cleanly.
    set(
        client,
        "#health",
        "bindings",
        json!([{"source": "HudVm.health", "target": "value"}]),
    );
    record(client, &mut steps);
    let (errors, codes) = lint_codes(client);
    assert!(errors >= 1, "{codes:?}");
    assert!(codes.contains(&"binding_target_mismatch".to_owned()), "{codes:?}");
    // Fixed by a new edit, not an undo, so the redo history stays empty for the walk below.
    set(
        client,
        "#health",
        "bindings",
        json!([{"source": "HudVm.health", "target": "progress"}]),
    );
    record(client, &mut steps);
    assert_eq!(lint_codes(client), (0, vec![]));

    // A failed op changes nothing and adds no history.
    let depth = undo_depth(client);
    let before = tree(client);
    assert_eq!(
        error_code(client.call(
            "editor.ui_designer_edit",
            json!({"op": "move", "node": "#header", "parent": "#title"})
        )),
        "ui_designer_bad_move"
    );
    assert_eq!(
        error_code(client.call(
            "editor.ui_designer_edit",
            json!({"op": "set", "node": "#nobody", "key": "name", "value": "x"})
        )),
        "ui_designer_bad_node"
    );
    assert_eq!(
        error_code(client.call("editor.ui_designer_preview", json!({"size": "12x34"}))),
        "ui_designer_bad_size"
    );
    assert_eq!(undo_depth(client), depth);
    assert_eq!(tree(client), before);
    steps
}

/// Undoes every step back to the empty document, then redoes them, comparing each hierarchy.
fn walk_history(client: &mut Client, steps: &[Tree]) {
    for expected in steps.iter().rev().skip(1) {
        edit(client, json!({"op": "undo"}));
        assert_eq!(&tree(client), expected);
    }
    assert_eq!(
        error_code(client.call("editor.ui_designer_edit", json!({"op": "undo"}))),
        "ui_designer_nothing_to_undo"
    );
    for expected in steps.iter().skip(1) {
        edit(client, json!({"op": "redo"}));
        assert_eq!(&tree(client), expected);
    }
    assert_eq!(
        error_code(client.call("editor.ui_designer_edit", json!({"op": "redo"}))),
        "ui_designer_nothing_to_redo"
    );
}

fn preview_at(
    client: &mut Client,
    size: &str,
    dims: [f64; 2],
    safe: [f64; 4],
    locale: &str,
    pseudo: bool,
) -> Value {
    let size = if size == "custom" {
        json!(dims)
    } else {
        json!(size)
    };
    // Settings stick to the document, so every call names its insets too.
    call(
        client,
        "editor.ui_designer_preview",
        json!({"size": size, "safe_area": safe, "locale": locale, "pseudo": pseudo,
        "samples": {"HudVm.score": "7", "HudVm.coins": "1234.5", "HudVm.health": "0.25"}}),
    )
}

fn named<'a>(preview: &'a Value, name: &str) -> &'a Value {
    preview["nodes"]
        .as_array()
        .expect("preview nodes")
        .iter()
        .find(|n| n["name"] == name)
        .unwrap_or_else(|| panic!("{name} in the preview: {preview}"))
}

fn rect(preview: &Value, name: &str) -> [f64; 4] {
    let r = named(preview, name)["rect"].as_array().expect("rect").clone();
    let v: Vec<f64> = r.iter().map(|x| x.as_f64().expect("rect number")).collect();
    [v[0], v[1], v[2], v[3]]
}

fn near(a: f64, b: f64, what: &str) {
    // Rects snap to whole physical pixels.
    assert!((a - b).abs() <= 1.0, "{what}: {a} vs {b}");
}

/// Every size and locale: layout from the properties, text from the locale and the bindings.
/// Returns the previews in order, for the reopen comparison.
fn check_previews(client: &mut Client) -> Vec<Value> {
    let mut all = Vec::new();
    for (size, dims, safe) in SIZES {
        let en = preview_at(client, size, dims, safe, "en", false);
        let fr = preview_at(client, size, dims, safe, "fr", false);
        let pseudo = preview_at(client, size, dims, safe, "en", true);
        for p in [&en, &fr, &pseudo] {
            let shown: Vec<f64> = p["size"]
                .as_array()
                .expect("size")
                .iter()
                .map(|v| v.as_f64().expect("size number"))
                .collect();
            assert_eq!(shown, dims.to_vec(), "{size}");
            let scale = p["scale"].as_f64().expect("scale");
            assert!(scale > 0.0, "{size}");
            let header = rect(p, "header");
            near(header[2], dims[0], &format!("{size}: full-width header"));
            near(header[0], 0.0, &format!("{size}: header x"));
            near(
                rect(p, "title")[2],
                header[2],
                &format!("{size}: title fills the header"),
            );
            near(
                rect(p, "title")[3],
                40.0 * scale,
                &format!("{size}: title height"),
            );
            let score = rect(p, "score");
            near(score[2], 120.0 * scale, &format!("{size}: score width"));
            near(score[3], 32.0 * scale, &format!("{size}: score height"));
            near(rect(p, "health")[2], 300.0 * scale, &format!("{size}: bar width"));
            // Column order follows the hierarchy: header, health (moved up), score, coins, footer.
            let order: Vec<f64> = ["header", "health", "score", "coins", "footer"]
                .iter()
                .map(|n| rect(p, n)[1])
                .collect();
            assert!(order.windows(2).all(|w| w[0] < w[1]), "{size}: {order:?}");
            let (play, quit) = (rect(p, "play"), rect(p, "quit"));
            assert!(
                quit[0] >= play[0] + play[2] - 1.0,
                "{size}: quit follows play: {play:?} {quit:?}"
            );
            let crosses = safe[0] > 0.0 || safe[1] > 0.0;
            assert_eq!(
                named(p, "header")["outside_safe_area"],
                crosses,
                "{size}: the header crosses only a left or top inset"
            );
            assert_eq!(named(p, "score")["text"], "007", "{size}");
            assert_eq!(named(p, "coins")["text"], "1,234.50", "{size}");
        }
        assert_eq!(named(&en, "title")["text"], "Main menu", "{size}");
        assert_eq!(named(&en, "play")["text"], "Play", "{size}");
        assert_eq!(named(&fr, "title")["text"], "Menu principal", "{size}");
        assert_eq!(named(&fr, "quit")["text"], "Quitter", "{size}");
        let pseudo_title = named(&pseudo, "title")["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !pseudo_title.is_empty() && pseudo_title != "Main menu",
            "{size}: {pseudo_title}"
        );
        // Label widths follow the locale: the longer French and padded pseudo labels widen the
        // auto-sized buttons.
        assert!(rect(&fr, "quit")[2] > rect(&en, "quit")[2], "{size}");
        assert!(rect(&pseudo, "play")[2] > rect(&en, "play")[2], "{size}");
        all.extend([en, fr, pseudo]);
    }
    all
}

/// The saved file spawned the way a game spawns it (no designer code), at one size and locale.
fn runtime_rects(text: &str, dims: [f64; 2], safe: [f64; 4], ftl: &str) -> Vec<(String, [f64; 4])> {
    use regia_app::{App, TimeUpdateStrategy};
    let file = regia_ui::parse_ui_ron(text, "menu.ui.ron", &regia_ui::UiMigrators::default())
        .expect("the saved file loads in the runtime");
    let mut app = App::new();
    app.add_plugins(regia_ui::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.update();
    let loc = regia_ui::Localization::new(vec![("en".into(), ftl.into())], None).expect("bundle");
    let world = app.world_mut();
    world.insert_resource(loc);
    world.insert_resource(regia_ui::UiScales {
        window_size: [dims[0] as f32, dims[1] as f32],
        scale_factor: 1.0,
    });
    world.insert_resource(regia_platform::SafeArea {
        left: safe[0] as u32,
        top: safe[1] as u32,
        right: safe[2] as u32,
        bottom: safe[3] as u32,
    });
    world
        .resource_mut::<regia_ui::authoring::UiLibrary>()
        .templates
        .insert("game".to_owned(), file);
    regia_ui::authoring::spawn_ui(world, "game").expect("spawn the saved menu");
    app.update();
    app.update();
    let world = app.world_mut();
    let mut names = world.query::<(&regia_ui::NodeName, &regia_ui::ComputedNode)>();
    let mut rects: Vec<(String, [f64; 4])> = names
        .iter(world)
        .map(|(name, computed)| (name.0.clone(), computed.rect.map(f64::from)))
        .collect();
    rects.sort_by(|a, b| a.0.cmp(&b.0));
    rects
}

#[test]
fn tools_designer_hierarchy_properties_bindings_multisize_locale() {
    let f = designer_fixture();
    let (editor, mut client) = start_designer(&f);
    let steps = author_menu(&mut client);
    walk_history(&mut client, &steps);
    let authored = steps.last().expect("final tree").clone();
    let previews = check_previews(&mut client);
    let saved = call(&mut client, "editor.ui_designer_save", json!({}));
    assert_eq!(saved["dirty"], false, "{saved}");
    shutdown(editor, client, &f);

    let text = level_text(&f, MENU);
    assert!(
        text.contains(&format!("version: {}", regia_ui::authoring::UI_RON_VERSION)),
        "written at the current version: {text}"
    );
    let (editor, mut client) = start_designer(&f);
    let reopened = call(&mut client, "editor.ui_designer_open", json!({"path": MENU}));
    assert_eq!(reopened["dirty"], false, "{reopened}");
    assert_eq!(tree(&mut client), authored, "the reopened hierarchy");
    assert_eq!(
        call(&mut client, "editor.ui_designer_state", json!({"node": "#play"}))["node"]["fields"]["command"],
        "menu.play"
    );
    assert_eq!(lint_codes(&mut client), (0, vec![]));
    let again = check_previews(&mut client);
    assert_eq!(again.len(), previews.len());
    for (a, b) in again.iter().zip(&previews) {
        assert_eq!(a["nodes"], b["nodes"], "the reopened preview is the authored one");
    }

    // The runtime lays the saved file out exactly as the designer previewed it.
    for (index, (size, dims, safe)) in SIZES.iter().enumerate() {
        let en = &again[index * 3];
        let runtime = runtime_rects(&text, *dims, *safe, EN);
        let mut compared = 0;
        for (name, runtime_rect) in &runtime {
            let Some(node) = en["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .find(|n| n["name"] == name.as_str())
            else {
                continue;
            };
            let shown = rect(en, name);
            for (a, b) in shown.iter().zip(runtime_rect) {
                assert!(
                    (a - b).abs() < 1e-3,
                    "{size}: {name}: designer {shown:?}, runtime {runtime_rect:?}"
                );
            }
            assert!(node["path"].is_string());
            compared += 1;
        }
        assert_eq!(
            compared, 8,
            "{size}: every authored node is in the runtime layout: {runtime:?}"
        );
    }
    shutdown(editor, client, &f);
}

#[test]
fn tools_designer_anchor_runtime_parity_and_project_scale() {
    let f = designer_fixture();
    // This fixture checks geometry, not admission speed; never depend on the socket's pacing.
    write_atomic(&f.0.join(".regia/agent.toml"), b"permissions = [\"read\",\"edit_scene\",\"assets\",\"play\",\"build\"]\nrate_limit_per_second = 10000\nheavy_rate_limit_per_second = 10000\n").expect("test permissions");
    let (editor, mut client) = start_designer(&f);
    call(&mut client, "editor.ui_designer_open", json!({"new":true}));
    edit(&mut client, json!({"op":"add","parent":"/","preset":"button"}));
    set(&mut client, "/0", "name", json!("anchored"));
    set(&mut client, "/0", "style.size", json!([120, 40]));
    for size in [[1280.0, 720.0], [1920.0, 1080.0], [800.0, 1280.0]] {
        for preset in regia_ui::anchors::AnchorPreset::ALL {
            let before = call(&mut client, "editor.ui_designer_preview", json!({"size":size}));
            edit(
                &mut client,
                json!({"op":"anchor_preset","node":"/0","preset":preset.name(),"keep_position":true}),
            );
            let kept = call(&mut client, "editor.ui_designer_preview", json!({}));
            for (a, b) in rect(&before, "anchored").into_iter().zip(rect(&kept, "anchored")) {
                near(a, b, "keep position");
            }
            edit(&mut client, json!({"op":"undo"}));
            edit(
                &mut client,
                json!({"op":"anchor_preset","node":"/0","preset":preset.name(),"keep_position":false}),
            );
            let snapped = call(&mut client, "editor.ui_designer_preview", json!({}));
            call(&mut client, "editor.ui_designer_save", json!({"path":MENU}));
            let text = std::fs::read_to_string(f.0.join(MENU)).expect("saved UI");
            let runtime = runtime_rects(&text, size, [0.0; 4], EN);
            let actual = runtime
                .iter()
                .find(|(name, _)| name == "anchored")
                .expect("runtime button")
                .1;
            for (a, b) in rect(&snapped, "anchored").into_iter().zip(actual) {
                assert!(
                    (a - b).abs() < 1e-3,
                    "{}: designer {a}, runtime {b}",
                    preset.name()
                );
            }
            edit(&mut client, json!({"op":"undo"}));
        }
    }
    set(&mut client, "/0", "style.aspect_ratio", json!(3.0));
    let effects = json!([
        {"color":[0,0,0,1],"width":1,"offset":[0,0],"blur":0},
        {"color":[0,0,0,0.5],"width":0,"offset":[2,3],"blur":2}
    ]);
    set(&mut client, "/0", "text.style.effects", effects);
    call(&mut client, "editor.ui_designer_save", json!({"path":MENU}));
    let scale = call(
        &mut client,
        "editor.ui_scale_settings",
        json!({"action":"set","dpi_curve":[[96,1],[192,2.5]]}),
    );
    assert_eq!(scale["applied"], true);
    let scale_get = call(&mut client, "editor.ui_scale_settings", json!({"action":"get"}));
    assert_eq!(scale_get["dpi_curve"], json!([[96.0, 1.0], [192.0, 2.5]]));
    let preview = call(
        &mut client,
        "editor.ui_designer_preview",
        json!({"scale":null,"scale_mode":"ConstantPhysicalSize","dpi":144,"box_model":true}),
    );
    assert_eq!(preview["scale"], 1.75);
    assert_eq!(preview["box_model"], true);
    let preview = call(&mut client, "editor.ui_designer_preview", json!({"scale":1.5}));
    assert_eq!(preview["scale"], 1.5);
    assert_eq!(
        call(&mut client, "editor.ui_designer_state", json!({}))["dirty"],
        false
    );
    shutdown(editor, client, &f);
}

/// ADR-7811: the real editor dispatcher drives an isolated sample model, exposes the runtime
/// cascade and applies an explicit live patch through the designer's undoable save path.
#[test]
fn designer_acceptance_interactive_samples_style_sources_and_copy_dispatch() {
    let f = designer_fixture();
    let (editor, mut client) = start_designer(&f);
    call(
        &mut client,
        "editor.ui_designer_open",
        json!({"new":true,"path":MENU}),
    );
    edit(&mut client, json!({"op":"add","parent":"/","preset":"button"}));
    set(&mut client, "/0", "name", json!("accept"));
    set(&mut client, "/0", "command", json!("accept"));
    edit(
        &mut client,
        json!({"op":"add","parent":"/","preset":"text_input"}),
    );
    set(&mut client, "/1", "name", json!("name"));
    set(
        &mut client,
        "/1",
        "bindings",
        json!([{"source":"Sample.name","target":"input","mode":"two_way"}]),
    );
    call(&mut client, "editor.ui_designer_save", json!({}));
    let before = std::fs::read(f.0.join(MENU)).expect("saved document");
    let play = call(
        &mut client,
        "editor.ui_designer_preview",
        json!({"interactive":true,"samples":{"Sample.name":"Ada"}}),
    );
    assert_eq!(play["interactive"], true);
    let clicked = call(
        &mut client,
        "editor.ui_designer_preview",
        json!({"input":{"action":"click","node":"#accept"}}),
    );
    assert!(
        clicked["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|event| event["value"] == "accept")
    );
    let typed = call(
        &mut client,
        "editor.ui_designer_preview",
        json!({"input":{"action":"type","node":"#name","text":"Grace"}}),
    );
    assert_eq!(typed["samples"]["Sample.name"], "Grace");
    let sources = call(
        &mut client,
        "editor.ui_designer_style_sources",
        json!({"node":"/0","open_token":"radius.md"}),
    );
    assert_eq!(sources["path"], "/0");
    assert!(
        sources["properties"]
            .as_array()
            .expect("properties")
            .iter()
            .any(|property| property["name"] == "radius" && property["sources"].is_array())
    );
    call(
        &mut client,
        "editor.ui_designer_preview",
        json!({"interactive":false}),
    );
    assert_eq!(std::fs::read(f.0.join(MENU)).expect("unchanged document"), before);
    let file = regia_ui::parse_ui_ron(
        std::str::from_utf8(&before).expect("utf8"),
        MENU,
        &regia_ui::UiMigrators::default(),
    )
    .expect("file");
    let node = &file.root.children[0];
    let origin = json!({"path":"ui/menu.ui.ron","node":"/0","expected":{"style":node.style,"text":node.text,"name":node.name,"widget":node.widget}});
    let depth = undo_depth(&mut client);
    let copied = call(
        &mut client,
        "editor.ui_inspector_copy_to_document",
        json!({"document":origin,"edits":{"style.flex_grow":2.0}}),
    );
    assert_eq!(copied["copied"], 1);
    assert_eq!(undo_depth(&mut client), depth + 1);
    let changed = std::fs::read_to_string(f.0.join(MENU)).expect("copied document");
    let changed =
        regia_ui::parse_ui_ron(&changed, MENU, &regia_ui::UiMigrators::default()).expect("copied file");
    assert_eq!(changed.root.children[0].style.flex_grow, 2.0);
    edit(&mut client, json!({"op":"undo"}));
    call(&mut client, "editor.ui_designer_save", json!({}));
    assert_eq!(std::fs::read(f.0.join(MENU)).expect("undo saved"), before);
    shutdown(editor, client, &f);
}
