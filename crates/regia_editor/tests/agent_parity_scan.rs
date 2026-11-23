// SPDX-License-Identifier: MPL-2.0
//! Editor agent parity (ADR-7307): every button, menu item, toggle and checkbox of non-test editor
//! code is drawn through an `agent_parity::action_*` helper or listed, classified, in
//! `crates/regia_editor/agent-parity-allow.ron`.
//!
//! `REGIA_AGENT_PARITY_UPDATE=1 cargo test -p regia_editor --test agent_parity_scan` rewrites the
//! allowlist: known sites keep their class, new sites are added as `gap`, vanished ones dropped.
//!
//! A `gap` row (a control that changes state with no agent path) fails the scan unless
//! [`KNOWN_GAPS`] names it with its follow-up (ADR-7452), so a new raw button cannot land silently:
//! classify the refreshed row (`method`/`command` with the real `via`, `view_only`, `confirm`,
//! `os_dialog`), or add the agent path, or list it here with the follow-up that will.
//!
//! Value editors (drag values, combo boxes, selectable values, sliders, text edits, angle and color
//! edits; ADR-7747) are scanned too and listed in the allowlist's last section. A value-editor row
//! of class `view_only` or `gap` must carry a `note` (why no route is needed, or what is missing and
//! the follow-up that adds it) instead of a `KNOWN_GAPS` entry.

use std::path::PathBuf;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::agent_parity::{
    self, ACTIONS, ActionPath, AllowEntry, SiteClass, allowlist_text, is_value_call, parity_report,
    parse_allowlist, refreshed_allowlist, scan_sites,
};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_allowlist() -> Vec<AllowEntry> {
    let path = crate_dir().join(agent_parity::ALLOWLIST_FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    parse_allowlist(&text).unwrap_or_else(|err| panic!("{err}"))
}

fn edit_world() -> World {
    let mut world = World::new();
    regia_editor::agent_ext::install(&mut world);
    regia_editor::headless::install(&mut world);
    world
}

/// A `gap` row the scan accepts for now: the allowlist key (`file`, `text`) and the follow-up that
/// will give it an agent path.
struct KnownGap {
    /// Source file, relative to `crates/regia_editor/src`.
    file: &'static str,
    /// The allowlist row's `text` (the trimmed source line).
    text: &'static str,
    /// The follow-up item (report and number) that adds the agent path.
    follow_up: &'static str,
}

/// The `gap` rows waiting for a follow-up (ADR-7452). Keep this short: every entry names the lane
/// report item that adds its agent path. An entry whose row is no longer `gap` fails the scan too.
///
/// Empty since the fracture support plane's clear landed (`editor.fracture_panel_set` takes
/// `{ below_y: null }`): the scan has no gap rows.
const KNOWN_GAPS: &[KnownGap] = &[];

/// The problems the button `gap` rows of `allow` raise against `known`: a `gap` row no entry names,
/// and an entry that names no `gap` row (stale). Value-editor `gap` rows carry their follow-up in
/// their `note` instead (the scan's `unreasoned` check, ADR-7747).
fn gap_problems(allow: &[AllowEntry], known: &[KnownGap]) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in allow
        .iter()
        .filter(|e| e.class == SiteClass::Gap && !is_value_call(&e.call))
    {
        if !known.iter().any(|k| k.file == entry.file && k.text == entry.text) {
            problems.push(format!(
                "gap row {}:{} `{}` is not in KNOWN_GAPS: classify it (method/command with the real via, view_only, confirm, os_dialog), add its agent path, or list it with its follow-up",
                entry.file, entry.line, entry.text
            ));
        }
    }
    for k in known {
        let live = allow
            .iter()
            .any(|e| e.class == SiteClass::Gap && e.file == k.file && e.text == k.text);
        if !live {
            problems.push(format!(
                "stale KNOWN_GAPS entry {} `{}` ({}) names no gap row: drop it",
                k.file, k.text, k.follow_up
            ));
        }
    }
    problems
}

/// Every `gap` row is a listed known gap, and every known gap is still a `gap` row.
#[test]
fn every_gap_row_is_a_known_gap() {
    let problems = gap_problems(&read_allowlist(), KNOWN_GAPS);
    assert!(
        problems.is_empty(),
        "{} problem(s):\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The acceptance fixture for the gap rule: one new unlisted `gap` row fails, the same row listed
/// passes, a classified row passes, and a listed entry with no `gap` row is stale.
#[test]
fn an_unlisted_gap_row_fails() {
    let row = |text: &str, class: &str| {
        format!(
            "(file:\"panel.rs\",line:2,call:\"button\",text:\"{text}\",class:{class},via:\"{}\")",
            if class == "method" { "editor.play" } else { "" }
        )
    };
    let fixture =
        |rows: &[String]| parse_allowlist(&format!("[{}]", rows.join(","))).expect("fixture parses");
    let classified = fixture(&[row("ui.button(\\\"Play\\\")", "method")]);
    assert!(gap_problems(&classified, &[]).is_empty());
    let one_gap = fixture(&[
        row("ui.button(\\\"Play\\\")", "method"),
        row("ui.button(\\\"Do it\\\")", "gap"),
    ]);
    let problems = gap_problems(&one_gap, &[]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("panel.rs:2") && problems[0].contains("Do it"),
        "{problems:?}"
    );
    let listed = [KnownGap {
        file: "panel.rs",
        text: "ui.button(\"Do it\")",
        follow_up: "fixture follow-up 1",
    }];
    assert!(gap_problems(&one_gap, &listed).is_empty());
    let problems = gap_problems(&classified, &listed);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("stale KNOWN_GAPS entry"), "{problems:?}");
}

#[test]
fn every_editor_ui_site_is_a_helper_or_classified() {
    let src = crate_dir().join("src");
    let sites = scan_sites(&src).expect("scan the editor sources");
    assert!(
        sites.len() > 100,
        "the scan finds the editor's controls: {}",
        sites.len()
    );
    let allow = read_allowlist();
    if std::env::var_os("REGIA_AGENT_PARITY_UPDATE").is_some() {
        let refreshed = refreshed_allowlist(&sites, &allow);
        let path = crate_dir().join(agent_parity::ALLOWLIST_FILE);
        let tmp = path.with_extension("ron.tmp");
        std::fs::write(&tmp, allowlist_text(&refreshed)).expect("write the allowlist");
        std::fs::rename(&tmp, &path).expect("replace the allowlist");
        return;
    }
    let report = parity_report(&sites, &allow);
    let mut problems = Vec::new();
    for site in &report.unlisted {
        problems.push(format!(
            "unlisted UI site crates/regia_editor/src/{}:{} ({}): `{}` — draw it with an agent_parity::action_* helper, or add it to agent-parity-allow.ron with its class",
            site.file, site.line, site.call, site.text
        ));
    }
    for entry in &report.stale {
        problems.push(format!(
            "stale allowlist entry {}:{}: `{}` matches no site",
            entry.file, entry.line, entry.text
        ));
    }
    for entry in &report.malformed {
        problems.push(format!(
            "allowlist entry {}:{} class {} {} a `via`",
            entry.file,
            entry.line,
            entry.class.name(),
            if entry.class.needs_via() {
                "needs"
            } else {
                "must not have"
            }
        ));
    }
    for entry in &report.unreasoned {
        problems.push(format!(
            "value-editor allowlist entry {}:{} (`{}`) is class {} with no `note`: say why the value needs no agent path (view_only) or what is missing and the follow-up that adds it (gap)",
            entry.file,
            entry.line,
            entry.text,
            entry.class.name()
        ));
    }
    assert!(
        problems.is_empty(),
        "{} problem(s); refresh with REGIA_AGENT_PARITY_UPDATE=1 cargo test -p regia_editor --test agent_parity_scan, then classify the new `gap` rows:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The acceptance fixture: a raw new button in non-test code fails the scan; the same button
/// through a helper, in a `#[cfg(test)]` item, or listed, does not.
#[test]
fn a_raw_new_button_fails_the_scan() {
    let dir = regia_test_utils::unique_temp_dir("agent-parity-fixture");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("dir");
    let write = |body: &str| std::fs::write(src.join("panel.rs"), body).expect("write");
    write(
        "fn draw(ui: &mut egui::Ui) {\n    agent_parity::action_button(ui, &agent_parity::FILE_NEW_LEVEL);\n}\n#[cfg(test)]\nmod tests { fn t(ui: &mut egui::Ui) { ui.button(\"In a test\"); } }\n",
    );
    let clean = scan_sites(&src).expect("scan");
    assert!(clean.is_empty(), "{clean:?}");
    write("fn draw(ui: &mut egui::Ui) {\n    if ui.button(\"Do it\").clicked() {}\n}\n");
    let sites = scan_sites(&src).expect("scan");
    let report = parity_report(&sites, &[]);
    assert_eq!(report.unlisted.len(), 1, "{report:?}");
    assert_eq!(report.unlisted[0].file, "panel.rs");
    assert_eq!(report.unlisted[0].line, 2);
    assert_eq!(report.unlisted[0].call, "button");
    let listed = refreshed_allowlist(&sites, &[]);
    assert_eq!(listed[0].class, SiteClass::Gap);
    assert!(parity_report(&sites, &listed).is_clean());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_radio_or_checkbox_widget_is_a_scanned_site() {
    // Review sprint-w11-rev-e-k #3: `ui.radio(` and `egui::Checkbox::new(` are controls too.
    let dir = regia_test_utils::unique_temp_dir("agent-parity-radio");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("dir");
    std::fs::write(
        src.join("panel.rs"),
        "fn draw(ui: &mut egui::Ui, on: &mut bool) {\n    if ui.radio(*on, \"Pivot\").clicked() {}\n    ui.add(egui::Checkbox::new(on, \"Windowed\"));\n    ui.radio_value(on, true, \"Yes\");\n}\n",
    )
    .expect("write");
    let sites = scan_sites(&src).expect("scan");
    let calls: Vec<(usize, &str)> = sites.iter().map(|s| (s.line, s.call.as_str())).collect();
    assert_eq!(
        calls,
        [(2, "radio"), (3, "Checkbox::new"), (4, "radio_value")],
        "{sites:?}"
    );
    let report = parity_report(&sites, &[]);
    assert_eq!(
        report.unlisted.len(),
        3,
        "unclassified, the scan fails: {report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The value-editor acceptance fixture (ADR-7747): a new unclassified `DragValue` in non-test code
/// fails the scan; refreshed, it is a `gap` row that still fails until it carries a `note`; with a
/// `method` route it passes. Every value-editor call kind is found, and the refresh keeps the
/// value-editor rows after the button rows.
#[test]
fn a_new_unclassified_drag_value_fails_the_scan() {
    let dir = regia_test_utils::unique_temp_dir("agent-parity-values");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("dir");
    let write = |body: &str| std::fs::write(src.join("panel.rs"), body).expect("write");
    write(
        "fn draw(ui: &mut egui::Ui, speed: &mut f32) {\n    if ui.button(\"Play\").clicked() {}\n    ui.add(egui::DragValue::new(speed).speed(0.1));\n}\n#[cfg(test)]\nmod tests { fn t(ui: &mut egui::Ui, v: &mut f32) { ui.add(egui::DragValue::new(v)); } }\n",
    );
    let sites = scan_sites(&src).expect("scan");
    let calls: Vec<(usize, &str)> = sites.iter().map(|s| (s.line, s.call.as_str())).collect();
    assert_eq!(calls, [(2, "button"), (3, "DragValue::new")], "{sites:?}");
    let button = refreshed_allowlist(&sites[..1], &[]);
    let mut button_row = button[0].clone();
    button_row.class = SiteClass::Method;
    button_row.via = "editor.play".into();
    let allow = vec![button_row.clone()];
    let report = parity_report(&sites, &allow);
    assert_eq!(report.unlisted.len(), 1, "{report:?}");
    assert_eq!(report.unlisted[0].call, "DragValue::new");
    assert_eq!(report.unlisted[0].line, 3);
    // Refreshed: a `gap` row with no note, which the scan still refuses.
    let refreshed = refreshed_allowlist(&sites, &allow);
    assert_eq!(refreshed.len(), 2);
    assert_eq!(refreshed[1].class, SiteClass::Gap);
    let report = parity_report(&sites, &refreshed);
    assert!(report.unlisted.is_empty());
    assert_eq!(report.unreasoned.len(), 1, "{report:?}");
    assert!(!report.is_clean());
    // A reasoned gap passes the source check; a route passes outright.
    let mut reasoned = refreshed.clone();
    reasoned[1].note = "no route sets the speed yet; follow-up: fixture job".into();
    assert!(parity_report(&sites, &reasoned).is_clean());
    let mut routed = refreshed;
    routed[1].class = SiteClass::Method;
    routed[1].via = "editor.set_property".into();
    assert!(parity_report(&sites, &routed).is_clean());
    // The file form puts the value rows in their own section after the buttons and round-trips.
    let text = allowlist_text(&routed);
    let section = text.find("// Value editors").expect("section header");
    assert!(text.find("editor.play").expect("button row") < section, "{text}");
    assert!(section < text.find("DragValue").expect("value row"), "{text}");
    assert_eq!(parse_allowlist(&text).expect("round trip"), routed);
    // Every value-editor kind is a site.
    write(
        "fn draw(ui: &mut egui::Ui, v: &mut f32, s: &mut String, k: &mut u8, a: &mut f32, c: &mut [u8; 3]) {\n\
         \x20   ui.add(egui::Slider::new(v, 0.0..=1.0));\n\
         \x20   egui::ComboBox::from_id_salt(\"k\").show_ui(ui, |ui| { ui.selectable_value(k, 1, \"One\"); });\n\
         \x20   ui.add(egui::TextEdit::singleline(s));\n\
         \x20   ui.add(egui::TextEdit::multiline(s));\n\
         \x20   ui.text_edit_singleline(s);\n\
         \x20   ui.text_edit_multiline(s);\n\
         \x20   ui.drag_angle(a);\n\
         \x20   ui.color_edit_button_srgb(c);\n\
         }\n",
    );
    let sites = scan_sites(&src).expect("scan");
    let mut calls: Vec<&str> = sites.iter().map(|s| s.call.as_str()).collect();
    calls.sort_unstable();
    let mut want: Vec<&str> = agent_parity::VALUE_CALLS
        .iter()
        .copied()
        .filter(|call| *call != "DragValue::new")
        .collect();
    want.sort_unstable();
    assert_eq!(calls, want, "{sites:?}");
    assert!(sites.iter().all(|s| is_value_call(&s.call)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `confirm` row names the method whose parameters state the choice: an empty `via` is malformed
/// (ADR-7747).
#[test]
fn a_confirm_row_needs_a_via() {
    let row = |via: &str| {
        parse_allowlist(&format!(
            "[(file:\"panel.rs\",line:2,call:\"button\",text:\"ui.button(\\\"Keep mine\\\")\",class:confirm,via:\"{via}\")]"
        ))
        .expect("fixture parses")
    };
    let site = agent_parity::Site {
        file: "panel.rs".into(),
        line: 2,
        call: "button".into(),
        text: "ui.button(\"Keep mine\")".into(),
    };
    let empty = parity_report(std::slice::from_ref(&site), &row(""));
    assert_eq!(empty.malformed.len(), 1, "{empty:?}");
    let named = parity_report(std::slice::from_ref(&site), &row("editor.level_reload_resolve"));
    assert!(named.is_clean(), "{named:?}");
}

/// The `a11y_audit` helpers' call sites are controls the scan finds (ADR-7452); the helpers' own
/// definitions are not.
#[test]
fn a_named_glyph_helper_call_is_a_site() {
    let dir = regia_test_utils::unique_temp_dir("agent-parity-helpers");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("dir");
    std::fs::write(
        src.join("panel.rs"),
        "pub fn glyph_button(ui: &mut egui::Ui, g: &str, s: &str) {}\n\
         fn bare_checkbox(ui: &mut egui::Ui, b: &mut bool, s: &str) {}\n\
         fn draw(ui: &mut egui::Ui, on: &mut bool) {\n\
         \x20   if crate::a11y_audit::glyph_button(ui, \"x\", \"Remove the binding\").clicked() {}\n\
         \x20   crate::a11y_audit::bare_checkbox(ui, on, \"Snap\");\n\
         }\n",
    )
    .expect("write");
    let sites = scan_sites(&src).expect("scan");
    let found: Vec<(usize, &str)> = sites.iter().map(|s| (s.line, s.call.as_str())).collect();
    assert_eq!(found, [(4, "glyph_button"), (5, "bare_checkbox")], "{sites:?}");
    assert_eq!(parity_report(&sites, &[]).unlisted.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every method and command the registry and the allowlist name exists.
#[test]
fn every_named_agent_path_exists() {
    let world = edit_world();
    let extension = world.resource::<AgentExtension>();
    let names = extension.method_names();
    // Host extension methods other crates register (`lua.*` by regia_script_lua's plugin) count
    // when the book's extension table lists them (`book-coverage` checks that table against the code).
    let coverage = std::fs::read_to_string(crate_dir().join("../../book/COVERAGE.md")).expect("COVERAGE.md");
    let method_exists = |name: &str| {
        names.contains(&name)
            || regia_agent::schema::METHODS.iter().any(|m| m.name == name)
            || coverage.contains(&format!("| `{name}` |"))
    };
    let command_exists = |id: &str| regia_editor::command_palette::find(id).is_some();
    let mut missing = Vec::new();
    for action in ACTIONS {
        match action.path {
            ActionPath::Method(m) | ActionPath::OsDialog(m) | ActionPath::Confirm(m) if !method_exists(m) => {
                missing.push(format!("action {}: method {m}", action.id));
            }
            ActionPath::Command(c) if !command_exists(c) => {
                missing.push(format!("action {}: command {c}", action.id))
            }
            _ => {}
        }
    }
    for entry in read_allowlist() {
        let ok = match entry.class {
            SiteClass::Command => command_exists(&entry.via),
            SiteClass::Method | SiteClass::OsDialog => method_exists(&entry.via),
            SiteClass::Confirm => method_exists(&entry.via),
            SiteClass::ViewOnly | SiteClass::Gap => true,
        };
        if !ok {
            missing.push(format!("{}:{} via {}", entry.file, entry.line, entry.via));
        }
    }
    assert!(missing.is_empty(), "unknown agent paths:\n{}", missing.join("\n"));
}

/// `editor.actions` lists the registry and the allowlist, filters, and refuses bad filters.
#[test]
fn editor_actions_lists_and_filters() {
    let mut world = edit_world();
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method("editor.actions")
        .expect("editor.actions is registered");
    let all = handler(&mut world, &serde_json::json!({})).expect("list");
    assert_eq!(all["actions"].as_array().map(Vec::len), Some(ACTIONS.len()));
    let file = handler(&mut world, &serde_json::json!({ "panel": "shell.file" })).expect("panel");
    let rows = file["actions"].as_array().expect("actions");
    assert!(rows.iter().all(|row| row["panel"] == "shell.file"), "{rows:?}");
    assert!(
        rows.iter()
            .any(|row| row["id"] == "file.new_level" && row["command"] == "file.new_level"),
        "{rows:?}"
    );
    let play = handler(
        &mut world,
        &serde_json::json!({ "panel": "shell", "class": "method" }),
    )
    .expect("class");
    let rows = play["actions"].as_array().expect("actions");
    assert!(rows.iter().any(|row| row["method"] == "editor.play"), "{rows:?}");
    assert!(rows.iter().all(|row| row["class"] == "method"));
    let gaps = handler(&mut world, &serde_json::json!({ "class": "gap" })).expect("gaps");
    assert!(
        gaps["sites"]
            .as_array()
            .expect("sites")
            .iter()
            .all(|row| row["class"] == "gap"),
        "{gaps}"
    );
    for (params, want) in [
        (serde_json::json!({ "panel": 3 }), "bad_panel"),
        (serde_json::json!({ "class": "nope" }), "bad_class"),
    ] {
        let err = handler(&mut world, &params).expect_err("refused");
        let got = err
            .data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default();
        assert_eq!(got, want, "{params}");
    }
}

/// The allowlist file and the one compiled into the build agree (the build embeds it).
#[test]
fn compiled_allowlist_matches_the_file() {
    assert_eq!(agent_parity::allowlist().expect("compiled"), read_allowlist());
}
