// SPDX-License-Identifier: MPL-2.0
//! Localization panel tests (ADR-3260): a fixture project with the source locale `en` and the targets `fr`
//! and `de`. Model level and agent level; one headless draw. No display, no GPU.

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_asset_pipeline::asset_graph::{AssetEntry, AssetKind, AssetSet, DependencyGraph};
use regia_ui::loc_po::source_hash;
use serde_json::{Value, json};

use super::model::{CellState, Filter, Query};
use super::*;
use crate::viewport_agent::Handler;

const CONFIG: &str = "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\", \"de\"])\n";

const EN: &str = "# Main menu\nmenu-start = Start game\nmenu-quit = Quit\ngreet = Hello, { $name }!\nlong-one = OK\nchanged = New source text\n";

const EN_OTHER: &str = "credits = Credits\n";

fn fr_main() -> String {
    format!(
        "## Main menu, French\n\nmenu-start = Démarrer\n    .title = Démarrer le jeu\n\n# regia-source: {}\nchanged = Ancien texte\n\n# Keep it short!\nlong-one = D'accord, très bien alors\n",
        source_hash("Old source text")
    )
}

const FR_GATHERED: &str =
    "\n# source: assets/ui/menu.ui.ron:3\n# TODO translate\ngreet = Hello, { $name }!\n";

fn de_main() -> String {
    format!(
        "menu-start = Starten\nmenu-quit = Ende\ngreet = Hallo, {{ $name }}!\nlong-one = Gut\n# regia-source: {}\nchanged = Neuer Text\n",
        source_hash("New source text")
    )
}

const UI: &str = "(\n  root: (children: [\n    (text: Loc(\"menu-start\")),\n    (text: \"loc:greet\"),\n    (text: Loc(\"menu-new\")),\n  ]),\n)\n";

const LEVEL: &str = "(\n  entities: [\n    (name: \"sign\", label: \"loc:menu-start\"),\n  ],\n)\n";

const CODE: &str = "fn main() {\n    let _ = regia_ui::loc!(\"menu-start\");\n}\n";

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

fn read(dir: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(rel)).ok()
}

fn fixture(name: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    write(&dir, "assets/localization/localization.ron", CONFIG);
    write(&dir, "assets/localization/en/main.ftl", EN);
    write(&dir, "assets/localization/en/other.ftl", EN_OTHER);
    write(&dir, "assets/localization/fr/main.ftl", &fr_main());
    write(&dir, "assets/localization/fr/gathered.ftl", FR_GATHERED);
    write(&dir, "assets/localization/de/main.ftl", &de_main());
    dir
}

fn with_content(dir: &Path) {
    write(dir, "assets/ui/menu.ui.ron", UI);
    write(dir, "assets/levels/hub.level.ron", LEVEL);
    write(dir, "src/main.rs", CODE);
}

fn world_with(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    method(name)(world, &params)
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn keys(table: &model::LocTable, query: &Query) -> Vec<String> {
    table.select(query).iter().map(|r| r.key.clone()).collect()
}

fn query(filter: Filter, locale: Option<&str>) -> Query {
    Query {
        locale: locale.map(str::to_owned),
        filter,
        ..Query::default()
    }
}

#[test]
fn filters_return_the_expected_keys() {
    let dir = fixture("loc-panel-filters");
    let project = model::read_project(&dir).expect("reads");
    assert_eq!(project.source, "en");
    assert_eq!(project.tags(), ["de", "en", "fr"]);
    let table = model::build_table(&project, model::DEFAULT_LONG_RATIO);
    assert_eq!(table.locales, ["en", "de", "fr"], "source first");
    assert_eq!(
        keys(&table, &query(Filter::All, None)),
        [
            "changed",
            "credits",
            "greet",
            "long-one",
            "menu-quit",
            "menu-start"
        ]
    );
    assert_eq!(
        keys(&table, &query(Filter::Missing, None)),
        ["credits", "menu-quit"]
    );
    assert_eq!(
        keys(&table, &query(Filter::Untranslated, None)),
        ["credits", "greet", "menu-quit"]
    );
    assert_eq!(keys(&table, &query(Filter::Stale, None)), ["changed"]);
    assert_eq!(keys(&table, &query(Filter::Long, None)), ["long-one"]);
    // `de` alone: only `credits` (in en/other.ftl) is missing; nothing is stale or long.
    assert_eq!(keys(&table, &query(Filter::Missing, Some("de"))), ["credits"]);
    assert!(keys(&table, &query(Filter::Stale, Some("de"))).is_empty());
    assert!(keys(&table, &query(Filter::Long, Some("de"))).is_empty());
    // A lower ratio flags `de`'s "Gut" (3 chars) against "OK" (2).
    let low = model::build_table(&project, 1.2);
    assert_eq!(
        keys(
            &low,
            &Query {
                long_ratio: 1.2,
                ..query(Filter::Long, Some("de"))
            }
        ),
        ["long-one"]
    );
    // Search reads keys, source text and translations, ignoring case.
    let search = |s: &str| {
        keys(
            &table,
            &Query {
                search: s.to_owned(),
                ..Query::default()
            },
        )
    };
    assert_eq!(search("QUIT"), ["menu-quit"]);
    assert_eq!(search("démarrer"), ["menu-start"]);
    assert_eq!(search("ende"), ["menu-quit"]);
    // Cell states and where each cell lives.
    let row = |k: &str| table.rows.iter().find(|r| r.key == k).expect("row");
    assert_eq!(row("menu-start").cells["en"].state, CellState::Source);
    assert_eq!(row("menu-start").cells["fr"].state, CellState::Translated);
    assert_eq!(
        row("menu-start").cells["fr"].file.as_deref(),
        Some("assets/localization/fr/main.ftl")
    );
    assert_eq!(row("menu-start").cells["fr"].line, 3);
    assert_eq!(row("greet").cells["fr"].state, CellState::Placeholder);
    assert_eq!(row("changed").cells["fr"].state, CellState::Stale);
    assert_eq!(row("changed").cells["de"].state, CellState::Translated);
    assert_eq!(row("menu-quit").cells["fr"].state, CellState::Missing);
    // Progress: the same numbers as `localization.coverage`.
    let fr = table.coverage.iter().find(|c| c.locale == "fr").expect("fr");
    assert_eq!(
        (fr.messages, fr.translated, fr.placeholders, fr.stale),
        (6, 3, 1, 1)
    );
    let de = table.coverage.iter().find(|c| c.locale == "de").expect("de");
    assert_eq!((de.messages, de.translated, de.stale), (6, 5, 0));
    assert!(table.issues.is_empty(), "{:?}", table.issues);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_edit_round_trips_byte_exact_except_the_edited_entry_and_undo_restores_the_bytes() {
    let dir = fixture("loc-panel-edit");
    let mut world = world_with(&dir);
    let path = "assets/localization/fr/main.ftl";
    let original = read(&dir, path).expect("fr");
    let out = set_translation(&mut world, "menu-start", "fr", "Lancer").expect("sets");
    assert_eq!(out.file, path);
    assert!(out.changed && !out.created);
    let edited = read(&dir, path).expect("fr");
    let expected = original.replace(
        "menu-start = Démarrer\n",
        &format!(
            "# regia-source: {}\nmenu-start = Lancer\n",
            source_hash("Start game")
        ),
    );
    assert_eq!(
        edited, expected,
        "only the edited entry changed; its attribute stayed"
    );
    // The other locales' files are untouched.
    assert_eq!(read(&dir, "assets/localization/de/main.ftl"), Some(de_main()));
    assert_eq!(
        read(&dir, "assets/localization/fr/gathered.ftl").as_deref(),
        Some(FR_GATHERED)
    );
    // Undo and redo through the editor's stack.
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(read(&dir, path).expect("fr").as_bytes(), original.as_bytes());
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(read(&dir, path).expect("fr"), edited);
    assert!(crate::command::undo_world(&mut world));

    // A stale translation: the hash line is updated in place and the cell is translated again.
    set_translation(&mut world, "changed", "fr", "Nouveau texte").expect("sets");
    let text = read(&dir, path).expect("fr");
    assert!(
        text.contains(&format!(
            "# regia-source: {}\nchanged = Nouveau texte\n",
            source_hash("New source text")
        )),
        "{text}"
    );
    assert!(!text.contains(&source_hash("Old source text")));
    // A placeholder in gathered.ftl: the TODO marker goes, the `# source:` comment stays.
    set_translation(&mut world, "greet", "fr", "Bonjour, { $name } !").expect("sets");
    let gathered = read(&dir, "assets/localization/fr/gathered.ftl").expect("gathered");
    assert_eq!(
        gathered,
        format!(
            "\n# source: assets/ui/menu.ui.ron:3\n# regia-source: {}\ngreet = Bonjour, {{ $name }} !\n",
            source_hash("Hello, { $name }!")
        )
    );
    let state = call(
        &mut world,
        "editor.loc_table",
        json!({ "locale": "fr", "filter": "stale" }),
    )
    .expect("table");
    assert_eq!(state["total"], 0, "{state}");
    // A missing key is appended to the file named like the source's; a key from en/other.ftl creates
    // fr/other.ftl, and undo removes it again.
    set_translation(&mut world, "menu-quit", "fr", "Quitter").expect("sets");
    let text = read(&dir, path).expect("fr");
    assert!(
        text.ends_with(&format!(
            "\n\n# regia-source: {}\nmenu-quit = Quitter\n",
            source_hash("Quit")
        )),
        "{text}"
    );
    let out = set_translation(&mut world, "credits", "fr", "Générique").expect("sets");
    assert!(out.created);
    assert_eq!(out.file, "assets/localization/fr/other.ftl");
    assert!(crate::command::undo_world(&mut world));
    assert!(
        read(&dir, "assets/localization/fr/other.ftl").is_none(),
        "undo removed the created file"
    );
    // Undo everything: every file has its first bytes again.
    while crate::command::undo_world(&mut world) {}
    assert_eq!(read(&dir, path).expect("fr"), original);
    assert_eq!(
        read(&dir, "assets/localization/fr/gathered.ftl").as_deref(),
        Some(FR_GATHERED)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn undo_leaves_a_file_changed_on_disk_alone() {
    let dir = fixture("loc-panel-conflict");
    let mut world = world_with(&dir);
    set_translation(&mut world, "menu-start", "de", "Los").expect("sets");
    let path = "assets/localization/de/main.ftl";
    let theirs = "menu-start = Edited elsewhere\n";
    write(&dir, path, theirs);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(read(&dir, path).as_deref(), Some(theirs));
    let panel = world.resource::<LocPanel>();
    assert_eq!(
        panel.error.as_ref().map(|e| e.code.as_str()),
        Some("loc_panel_conflict")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_edits_are_refused_with_codes_and_write_nothing() {
    let dir = fixture("loc-panel-refuse");
    let mut world = world_with(&dir);
    let before = read(&dir, "assets/localization/fr/main.ftl");
    for (params, code) in [
        (
            json!({ "key": "menu-start", "locale": "xx", "text": "a" }),
            "loc_panel_locale_not_found",
        ),
        (
            json!({ "key": "nope", "locale": "fr", "text": "a" }),
            "loc_panel_key_not_found",
        ),
        (
            json!({ "key": "1bad", "locale": "fr", "text": "a" }),
            "loc_panel_bad_key",
        ),
        (
            json!({ "key": "menu-start", "locale": "fr", "text": "  " }),
            "loc_panel_empty_text",
        ),
        (
            json!({ "key": "menu-start", "locale": "fr", "text": "{ unclosed" }),
            "loc_panel_bad_text",
        ),
        (
            json!({ "key": "menu-start", "locale": "fr", "text": "A\n.title = sneaky" }),
            "loc_panel_bad_text",
        ),
    ] {
        let e = call(&mut world, "editor.loc_set", params.clone()).expect_err(&params.to_string());
        assert_eq!(reason(&e), code, "{params}");
    }
    assert_eq!(read(&dir, "assets/localization/fr/main.ftl"), before);
    assert_eq!(
        world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth),
        0
    );
    // A dropped variable is written with a warning.
    let out = call(
        &mut world,
        "editor.loc_set",
        json!({ "key": "greet", "locale": "de", "text": "Hallo!" }),
    )
    .expect("sets");
    assert_eq!(out["warnings"][0]["code"], "loc_variable_mismatch", "{out}");
    assert_eq!(out["undo_depth"], 1);
    // The same text again changes nothing and pushes nothing.
    let again = call(
        &mut world,
        "editor.loc_set",
        json!({ "key": "greet", "locale": "de", "text": "Hallo!" }),
    )
    .expect("sets");
    assert_eq!(
        (again["changed"].clone(), again["undo_depth"].clone()),
        (json!(false), json!(1))
    );
    // Table errors.
    for (params, code) in [
        (json!({ "filter": "weird" }), "loc_panel_bad_filter"),
        (json!({ "locale": "xx" }), "loc_panel_locale_not_found"),
        (json!({ "page": 7 }), "loc_panel_bad_page"),
        (json!({ "page_size": 0 }), "loc_panel_bad_page"),
        (json!({ "long_ratio": 0.5 }), "loc_panel_bad_ratio"),
    ] {
        let e = call(&mut world, "editor.loc_table", params.clone()).expect_err(&params.to_string());
        assert_eq!(reason(&e), code, "{params}");
    }
    let mut empty = World::new();
    let e = call(&mut empty, "editor.loc_table", json!({})).expect_err("no project");
    assert_eq!(reason(&e), "loc_panel_no_project");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gather_picks_up_a_new_key_from_a_ui_file_and_undo_restores_the_files() {
    let dir = fixture("loc-panel-gather");
    with_content(&dir);
    let mut world = world_with(&dir);
    let dry = call(&mut world, "editor.loc_gather", json!({ "dry_run": true })).expect("dry run");
    assert_eq!(dry["written"], false);
    assert!(read(&dir, "assets/localization/en/gathered.ftl").is_none());
    let out = call(&mut world, "editor.loc_gather", json!({})).expect("gathers");
    assert_eq!(out["written"], true, "{out}");
    let files = out["files"].as_array().expect("files");
    assert_eq!(files.len(), 3, "every locale gains menu-new: {out}");
    assert!(files.iter().all(|f| f["added"] == json!(["menu-new"])), "{out}");
    let en = read(&dir, "assets/localization/en/gathered.ftl").expect("en gathered");
    assert_eq!(en, "\n# source: assets/ui/menu.ui.ron:5\nmenu-new = menu-new\n");
    let fr = read(&dir, "assets/localization/fr/gathered.ftl").expect("fr gathered");
    assert!(fr.starts_with(FR_GATHERED), "existing text kept: {fr}");
    assert!(fr.ends_with("# TODO translate\nmenu-new = menu-new\n"), "{fr}");
    let table = call(&mut world, "editor.loc_table", json!({ "search": "menu-new" })).expect("table");
    assert_eq!(table["total"], 1, "{table}");
    assert_eq!(table["rows"][0]["cells"]["fr"]["state"], "placeholder");
    assert!(crate::command::undo_world(&mut world));
    assert!(read(&dir, "assets/localization/en/gathered.ftl").is_none());
    assert_eq!(
        read(&dir, "assets/localization/fr/gathered.ftl").as_deref(),
        Some(FR_GATHERED)
    );
    // Nothing new: a second gather writes nothing.
    call(&mut world, "editor.loc_gather", json!({})).expect("gathers");
    let again = call(&mut world, "editor.loc_gather", json!({})).expect("gathers");
    assert_eq!(again["files"], json!([]));
    assert_eq!(again["written"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn usages_list_the_files_that_reference_a_key() {
    let dir = fixture("loc-panel-usages");
    with_content(&dir);
    let mut world = world_with(&dir);
    let out = call(&mut world, "editor.loc_usages", json!({ "key": "menu-start" })).expect("usages");
    let found: Vec<(String, u64, String)> = out["usages"]
        .as_array()
        .expect("usages")
        .iter()
        .map(|u| {
            (
                u["file"].as_str().unwrap_or_default().to_owned(),
                u["line"].as_u64().unwrap_or_default(),
                u["kind"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("assets/levels/hub.level.ron".to_owned(), 3, "scene".to_owned()),
            ("assets/ui/menu.ui.ron".to_owned(), 3, "ui".to_owned()),
            ("src/main.rs".to_owned(), 2, "code".to_owned()),
        ]
    );
    assert_eq!(out["graph"], "none");
    assert_eq!(out["defined_in"].as_array().map(Vec::len), Some(3), "{out}");
    let e = call(&mut world, "editor.loc_usages", json!({ "key": "not a key" })).expect_err("bad key");
    assert_eq!(reason(&e), "loc_panel_bad_key");
    // Through the asset graph: a prefab placing the UI and a level placing the prefab use the key too.
    let project = model::read_project(&dir).expect("reads");
    let gathered = ops::gather(&dir, &project).expect("gathers");
    let mut set = AssetSet::new();
    set.push(AssetEntry::new("ui/menu.ui.ron", AssetKind::Other));
    set.push(
        AssetEntry::new("prefabs/kiosk.prefab.ron", AssetKind::Other).with_references(["ui/menu.ui.ron"]),
    );
    set.push(
        AssetEntry::new("levels/town.level.ron", AssetKind::Scene)
            .with_references(["prefabs/kiosk.prefab.ron"]),
    );
    let graph = DependencyGraph::build(&set);
    let via: Vec<(String, Option<String>)> = ops::usages(&gathered, "menu-start", Some(&graph))
        .into_iter()
        .filter(|u| u.via.is_some())
        .map(|u| (u.file, u.via))
        .collect();
    assert_eq!(
        via,
        [
            (
                "assets/levels/town.level.ron".to_owned(),
                Some("assets/ui/menu.ui.ron".to_owned())
            ),
            (
                "assets/prefabs/kiosk.prefab.ron".to_owned(),
                Some("assets/ui/menu.ui.ron".to_owned())
            ),
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pseudo_toggle_reaches_the_runtime_localization_of_the_viewport() {
    let dir = fixture("loc-panel-pseudo");
    let mut world = world_with(&dir);
    let out = call(&mut world, "editor.loc_pseudo", json!({ "on": true })).expect("toggles");
    assert_eq!(out, json!({ "pseudo": true, "viewport_localization": false }));
    let loc = regia_ui::localization::Localization::new(
        vec![("en".to_owned(), "menu-start = Start\n".to_owned())],
        Some("en".to_owned()),
    )
    .expect("loads");
    world.insert_resource(loc);
    let out = call(&mut world, "editor.loc_pseudo", json!({ "on": true })).expect("toggles");
    assert_eq!(out["viewport_localization"], true);
    assert!(world.resource::<regia_ui::localization::Localization>().pseudo());
    // A play world started later follows the toggle when the panel draws.
    let mut play = World::new();
    play.insert_resource(
        regia_ui::localization::Localization::new(
            vec![("en".to_owned(), "a = A\n".to_owned())],
            Some("en".to_owned()),
        )
        .expect("loads"),
    );
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        loc_panel(ui, &mut world, Some(&mut play))
    });
    output.textures_delta.clear();
    assert!(play.resource::<regia_ui::localization::Localization>().pseudo());
    call(&mut world, "editor.loc_pseudo", json!({ "on": false })).expect("toggles");
    assert!(!world.resource::<regia_ui::localization::Localization>().pseudo());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_panel_draws_the_table_with_state_colors() {
    let dir = fixture("loc-panel-draw");
    let mut world = world_with(&dir);
    let shapes =
        crate::data_panels::test_ui::draw_frames(&mut world, 3, |ui, world| loc_panel(ui, world, None));
    for fill in [TRANSLATED_FILL, STALE_FILL, PLACEHOLDER_FILL, MISSING_FILL] {
        assert!(
            crate::data_panels::test_ui::has_color(&shapes, fill),
            "{fill:?} drawn"
        );
    }
    let panel = world.resource::<LocPanel>();
    assert!(panel.error.is_none(), "{:?}", panel.error);
    assert_eq!(panel.table().map(|t| t.rows.len()), Some(6));
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ exchange (ADR-7677)

/// Records a widget's rect so the exchange tests can click it (egui has no widget lookup).
pub(super) fn record(ui: &egui::Ui, key: &str, rect: egui::Rect) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("loc-panel-test-rect", key)), rect));
}

/// A headless Localization panel driven by egui input events.
struct Headless {
    ctx: egui::Context,
    world: World,
}

impl Headless {
    fn new(dir: &Path) -> Self {
        let mut h = Self {
            ctx: egui::Context::default(),
            world: world_with(dir),
        };
        h.frame(Vec::new());
        h.frame(Vec::new());
        h
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 1000.0),
            )),
            events,
            ..Default::default()
        };
        let world = &mut self.world;
        let mut output = self.ctx.run_ui(raw, |ui| loc_panel(ui, world, None));
        output.textures_delta.clear();
    }

    fn rect(&self, key: &str) -> egui::Rect {
        let id = egui::Id::new(("loc-panel-test-rect", key));
        self.ctx
            .data_mut(|d| d.get_temp::<egui::Rect>(id))
            .unwrap_or_else(|| panic!("no rect for {key}"))
    }

    fn click(&mut self, key: &str) {
        let pos = self.rect(key).center();
        self.frame(vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
        self.frame(Vec::new());
        self.frame(Vec::new());
    }

    /// Clicks the text field `key`, replaces its text and presses Enter.
    fn type_into(&mut self, key: &str, text: &str) {
        self.click(key);
        let press = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        self.frame(vec![press(egui::Key::A)]);
        self.frame(vec![egui::Event::Text(text.to_owned())]);
        self.frame(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        self.frame(Vec::new());
    }

    fn panel(&self) -> &LocPanel {
        self.world.resource::<LocPanel>()
    }
}

/// The exchange file `text` (PO or XLIFF) with `msgstr` of each `(key, translation)` filled, in the same
/// format.
fn translate_exchange(text: &str, edits: &[(&str, &str)]) -> String {
    use regia_asset_pipeline::loc_exchange::ExchangeFile;
    let file = ExchangeFile::parse(text, "export").expect("parses");
    let (mut po, xliff) = match file {
        ExchangeFile::Po(po) => (po, false),
        ExchangeFile::Xliff(x) => (x.po, true),
    };
    for (key, translation) in edits {
        let entry = po
            .entries
            .iter_mut()
            .find(|e| e.ctxt == *key)
            .unwrap_or_else(|| panic!("no entry {key}"));
        (*translation).clone_into(&mut entry.msgstr);
    }
    if xliff {
        regia_ui::loc_xliff::write_xliff(&po).expect("xliff")
    } else {
        regia_ui::loc_po::write_po(&po)
    }
}

/// What `regia loc import` writes for `file` into `dir`: the CLI's own steps (`plan_import` with the file's
/// plan, then `write_import`), which `regia_cli`'s `import_with` calls.
fn cli_import(dir: &Path, file: &Path) {
    use regia_asset_pipeline::loc_exchange::{self as exchange, ExchangeFile};
    let text = std::fs::read_to_string(file).expect("file");
    let parsed = ExchangeFile::parse(&text, "file").expect("parses");
    let planned = exchange::plan_import(dir, parsed.language(), None, |ctx| parsed.plan(ctx)).expect("plans");
    exchange::write_import(dir, &planned).expect("writes");
}

const FR_IMPORTED: &str = "assets/localization/fr/imported.ftl";

/// Export through the panel's controls writes the library's (`regia loc export`'s) bytes; a translated file
/// imported through Import… then Apply shows the key report first, writes nothing before Apply, then writes
/// the same `imported.ftl` bytes as the CLI's import on a twin project, as one undo step. PO and XLIFF.
#[test]
fn export_and_import_through_the_panel_write_the_cli_bytes() {
    use regia_asset_pipeline::loc_exchange::{self as exchange, ExchangeFormat};
    for (format, ext) in [(ExchangeFormat::Po, "po"), (ExchangeFormat::Xliff, "xliff")] {
        let dir = fixture(&format!("loc-panel-exchange-{ext}"));
        let twin = fixture(&format!("loc-panel-exchange-twin-{ext}"));
        let mut h = Headless::new(&dir);
        assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
        // Locale, format and file, then Export.
        h.click("exchange-locale");
        h.click("exchange-locale:fr");
        assert_eq!(h.panel().exchange_locale.as_deref(), Some("fr"));
        h.click("exchange-format");
        h.click(&format!("exchange-format:{ext}"));
        assert_eq!(h.panel().exchange_format, format, "{ext}");
        let name = format!("fr.{}", format.extension());
        h.type_into("exchange-export-path", &name);
        assert_eq!(h.panel().export_path, name);
        assert!(
            h.panel().import_path.is_empty(),
            "Export's path is not Import's (ADR-7716)"
        );
        h.click("exchange-export");
        assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
        let exported = std::fs::read_to_string(dir.join(&name)).expect("the panel wrote the file");
        let library =
            exchange::export(&dir, "fr", format, regia_ui::loc_po::ExportFilter::All).expect("exports");
        assert_eq!(
            exported, library.text,
            "{ext}: the panel writes regia loc export's bytes"
        );
        assert_eq!(
            h.panel().last_export.as_ref().map(|e| e.entries),
            Some(library.entries)
        );

        // A translator fills menu-quit; Import… plans and reports, writing nothing.
        std::fs::write(
            dir.join(&name),
            translate_exchange(&exported, &[("menu-quit", "Quitter")]),
        )
        .expect("fill");
        std::fs::copy(dir.join(&name), twin.join(&name)).expect("same file for the twin");
        h.type_into("exchange-import-path", &name);
        h.click("exchange-import");
        assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
        let preview = h.panel().pending_import.clone().expect("a planned import");
        assert_eq!(preview.locale, "fr");
        assert_eq!(preview.format, format);
        assert_eq!(preview.added, ["menu-quit"]);
        assert!(preview.changed.is_empty(), "{:?}", preview.changed);
        assert_eq!(preview.missing, ["credits", "greet"]);
        assert!(read(&dir, FR_IMPORTED).is_none(), "nothing written before Apply");

        // Apply: one undo step, the CLI's bytes.
        let depth = h
            .world
            .get_resource::<UndoStack>()
            .map_or(0, UndoStack::undo_depth);
        h.click("exchange-apply");
        assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
        assert!(h.panel().pending_import.is_none());
        assert_eq!(
            h.world.resource::<UndoStack>().undo_depth(),
            depth + 1,
            "one undo step"
        );
        cli_import(&twin, &twin.join(&name));
        let ours = read(&dir, FR_IMPORTED).expect("imported.ftl");
        assert_eq!(
            Some(ours.clone()),
            read(&twin, FR_IMPORTED),
            "{ext}: regia loc import's bytes"
        );
        assert!(ours.contains("menu-quit = Quitter"), "{ours}");
        assert_eq!(
            read(&dir, "assets/localization/fr/gathered.ftl"),
            read(&twin, "assets/localization/fr/gathered.ftl")
        );

        // A second file changes the imported key.
        std::fs::write(
            dir.join(&name),
            translate_exchange(&exported, &[("menu-quit", "Sortir")]),
        )
        .expect("fill");
        h.click("exchange-import");
        let preview = h.panel().pending_import.clone().expect("a planned import");
        assert!(preview.added.is_empty(), "{:?}", preview.added);
        assert_eq!(preview.changed, ["menu-quit"]);
        h.click("exchange-apply");
        assert!(
            read(&dir, FR_IMPORTED)
                .expect("imported")
                .contains("menu-quit = Sortir")
        );

        // Undo walks back to the first import, then to no file.
        assert!(crate::command::undo_world(&mut h.world));
        assert_eq!(read(&dir, FR_IMPORTED), Some(ours));
        assert!(crate::command::undo_world(&mut h.world));
        assert!(read(&dir, FR_IMPORTED).is_none(), "undo removed the created file");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&twin);
    }
}

/// ADR-7716: Export and Import have their own paths. Picking a returned translation file for Import
/// leaves Export's path alone, and an Export after it writes to its own path, not over the returned
/// file. Both directions round-trip: the exported file, translated and returned under another name,
/// imports with the key report.
#[test]
fn export_and_import_paths_are_separate() {
    let dir = fixture("loc-panel-separate-paths");
    let mut h = Headless::new(&dir);
    assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
    h.click("exchange-locale");
    h.click("exchange-locale:fr");
    h.type_into("exchange-export-path", "out/fr.po");
    h.click("exchange-export");
    assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
    let exported = std::fs::read_to_string(dir.join("out/fr.po")).expect("exported");

    // The translator's file comes back under another name; Import reads it from its own box.
    let returned = translate_exchange(&exported, &[("menu-quit", "Quitter")]);
    write(&dir, "returned/fr.po", &returned);
    h.type_into("exchange-import-path", "returned/fr.po");
    assert_eq!(
        h.panel().export_path,
        "out/fr.po",
        "picking an import file leaves Export's path"
    );
    h.click("exchange-import");
    assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
    let preview = h.panel().pending_import.clone().expect("a planned import");
    assert_eq!(preview.added, ["menu-quit"]);
    assert!(preview.file.ends_with("returned/fr.po"), "{:?}", preview.file);

    // An Export now must not touch the returned file.
    h.click("exchange-export");
    assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
    assert_eq!(
        read(&dir, "returned/fr.po").as_deref(),
        Some(returned.as_str()),
        "Export wrote to its own path, not over the returned file"
    );
    assert_eq!(read(&dir, "out/fr.po").as_deref(), Some(exported.as_str()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent methods call the panel's functions: `editor.loc_export` writes the panel's bytes,
/// `editor.loc_import` with `dry_run` reports and writes nothing, without it writes the CLI's bytes as one
/// undo step; bad input answers with the documented codes.
#[test]
fn agent_export_and_import_match_the_panel() {
    use regia_asset_pipeline::loc_exchange::{self as exchange, ExchangeFormat};
    let dir = fixture("loc-agent-exchange");
    let twin = fixture("loc-agent-exchange-twin");
    let mut world = world_with(&dir);
    let out = call(
        &mut world,
        "editor.loc_export",
        json!({ "locale": "fr", "format": "xliff", "path": "out/fr.xlf" }),
    )
    .expect("exports");
    assert_eq!(out["format"], "xliff");
    assert_eq!(out["only"], "all");
    let text = std::fs::read_to_string(dir.join("out/fr.xlf")).expect("written");
    let library = exchange::export(
        &dir,
        "fr",
        ExchangeFormat::Xliff,
        regia_ui::loc_po::ExportFilter::All,
    )
    .expect("exports");
    assert_eq!(text, library.text);
    assert_eq!(out["entries"], library.entries);
    // Default path: .regia/loc/fr.po.
    let out = call(&mut world, "editor.loc_export", json!({ "locale": "fr" })).expect("exports");
    assert!(dir.join(".regia/loc/fr.po").is_file(), "{out}");

    std::fs::write(
        dir.join("out/fr.xlf"),
        translate_exchange(&text, &[("menu-quit", "Quitter")]),
    )
    .expect("fill");
    std::fs::create_dir_all(twin.join("out")).expect("out");
    std::fs::copy(dir.join("out/fr.xlf"), twin.join("out/fr.xlf")).expect("copy");
    let dry = call(
        &mut world,
        "editor.loc_import",
        json!({ "path": "out/fr.xlf", "dry_run": true }),
    )
    .expect("plans");
    assert_eq!(dry["added"], json!(["menu-quit"]));
    assert_eq!(dry["missing"], json!(["credits", "greet"]));
    assert_eq!(dry["written"], false);
    assert_eq!(dry["files"], json!([FR_IMPORTED]));
    assert!(read(&dir, FR_IMPORTED).is_none());
    let done = call(&mut world, "editor.loc_import", json!({ "path": "out/fr.xlf" })).expect("imports");
    assert_eq!(done["written"], true);
    // An `all` export carries the locale's existing translations too: they import again, unchanged.
    assert_eq!(done["imported"], dry["imported"]);
    assert_eq!(done["added"], json!(["menu-quit"]));
    assert_eq!(done["undo_depth"], 1);
    cli_import(&twin, &twin.join("out/fr.xlf"));
    assert_eq!(read(&dir, FR_IMPORTED), read(&twin, FR_IMPORTED));

    // Codes.
    let e = call(&mut world, "editor.loc_export", json!({ "locale": "en" })).expect_err("source locale");
    assert_eq!(reason(&e), "loc_panel_export_failed", "{e:?}");
    let e = call(
        &mut world,
        "editor.loc_export",
        json!({ "locale": "fr", "format": "csv" }),
    )
    .expect_err("format");
    assert_eq!(reason(&e), "loc_panel_bad_format");
    let e = call(
        &mut world,
        "editor.loc_export",
        json!({ "locale": "fr", "only": "some" }),
    )
    .expect_err("filter");
    assert_eq!(reason(&e), "loc_panel_bad_filter");
    let e = call(&mut world, "editor.loc_export", json!({ "locale": "../x" })).expect_err("tag");
    assert_eq!(reason(&e), "loc_panel_bad_locale");
    std::fs::write(dir.join("out/junk.po"), "msgid \"unterminated\n").expect("junk");
    let e = call(&mut world, "editor.loc_import", json!({ "path": "out/junk.po" })).expect_err("junk");
    assert_eq!(reason(&e), "loc_panel_import_failed", "{e:?}");
    // A file for another locale is refused and writes nothing.
    let before = read(&dir, FR_IMPORTED);
    let e = call(
        &mut world,
        "editor.loc_import",
        json!({ "path": "out/fr.xlf", "locale": "de" }),
    )
    .expect_err("refused");
    assert_eq!(reason(&e), "loc_panel_import_refused", "{e:?}");
    assert_eq!(read(&dir, FR_IMPORTED), before);
    assert!(read(&dir, "assets/localization/de/imported.ftl").is_none());
    let e = super::exchange::apply_import(&mut world).expect_err("nothing planned");
    assert_eq!(e.code, "loc_panel_no_import");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&twin);
}

// ------------------------------------------------------------------ compile, context, preview (ADR-7855)

/// Records a widget's id, so a test can give it keyboard focus.
pub(super) fn record_id(ui: &egui::Ui, key: &str, id: egui::Id) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("loc-panel-test-id", key)), id));
}

impl Headless {
    /// Gives the widget `key` keyboard focus and presses `key_name` (Enter or Space): no pointer.
    fn press_on_focus(&mut self, key: &str, key_name: egui::Key) {
        let id = self
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("loc-panel-test-id", key))))
            .unwrap_or_else(|| panic!("no id for {key}"));
        self.ctx.memory_mut(|m| m.request_focus(id));
        self.frame(Vec::new());
        for pressed in [true, false] {
            self.frame(vec![egui::Event::Key {
                key: key_name,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
        self.frame(Vec::new());
        self.frame(Vec::new());
    }
}

fn table_row(world: &mut World, key: &str) -> Value {
    let table = call(world, "editor.loc_table", json!({ "page_size": 100 })).expect("table");
    table["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["key"] == key))
        .cloned()
        .unwrap_or_else(|| panic!("no row {key}"))
}

/// The context is the comment above the message (tool markers aside), shown in `editor.loc_table`; setting
/// it rewrites only those lines of the source file as `# note:` lines, is one undo step, and the exports carry
/// it (PO `#.`, XLIFF note). Empty text removes it.
#[test]
fn a_keys_context_is_read_edited_undone_and_exported() {
    let dir = fixture("loc-panel-context");
    let mut world = world_with(&dir);
    assert_eq!(
        table_row(&mut world, "menu-start")["context"],
        json!(["Main menu"])
    );
    assert_eq!(table_row(&mut world, "menu-quit")["context"], json!([]));
    let before = read(&dir, "assets/localization/en/main.ftl").expect("en");

    let set = call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "menu-quit", "text": "Button in the pause menu\nKeep it short" }),
    )
    .expect("sets");
    assert_eq!(set["changed"], true);
    assert_eq!(set["file"], "assets/localization/en/main.ftl");
    assert_eq!(
        set["context"],
        json!(["Button in the pause menu", "Keep it short"])
    );
    assert_eq!(
        read(&dir, "assets/localization/en/main.ftl").expect("en"),
        before.replace(
            "menu-quit = Quit",
            "# note: Button in the pause menu\n# note: Keep it short\nmenu-quit = Quit"
        ),
        "only the comment lines above the message change"
    );
    // The same text again changes nothing and adds no undo step.
    let again = call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "menu-quit", "text": "Button in the pause menu\nKeep it short" }),
    )
    .expect("again");
    assert_eq!(again["changed"], false);
    assert_eq!(again["undo_depth"], set["undo_depth"]);
    // An existing comment is replaced, not doubled.
    call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "menu-start", "text": "The first entry" }),
    )
    .expect("replaces");
    let en = read(&dir, "assets/localization/en/main.ftl").expect("en");
    assert!(
        en.contains("# note: The first entry\nmenu-start = Start game"),
        "{en}"
    );
    assert!(!en.contains("# Main menu"), "{en}");

    // Exports carry it.
    let po = call(
        &mut world,
        "editor.loc_export",
        json!({ "locale": "fr", "path": "fr.po" }),
    )
    .expect("po");
    let po_text = std::fs::read_to_string(po["file"].as_str().expect("file")).expect("po file");
    assert!(
        po_text.contains("#. note: Button in the pause menu") && po_text.contains("#. note: Keep it short"),
        "{po_text}"
    );
    let xliff = call(
        &mut world,
        "editor.loc_export",
        json!({ "locale": "fr", "format": "xliff", "path": "fr.xlf" }),
    )
    .expect("xliff");
    let xliff_text = std::fs::read_to_string(xliff["file"].as_str().expect("file")).expect("xliff file");
    assert!(
        xliff_text.contains("<note category=\"regia\">note: Button in the pause menu</note>"),
        "{xliff_text}"
    );

    // One undo step per set; removing the context is a set too.
    call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "menu-quit", "text": "" }),
    )
    .expect("clears");
    assert_eq!(table_row(&mut world, "menu-quit")["context"], json!([]));
    assert!(crate::command::undo_world(&mut world), "undo the clear");
    assert!(crate::command::undo_world(&mut world), "undo the replace");
    assert!(crate::command::undo_world(&mut world), "undo the first set");
    assert_eq!(
        read(&dir, "assets/localization/en/main.ftl").expect("en"),
        before,
        "three sets, three undo steps, the bytes are back"
    );

    // Errors: a key only a target defines, and a bad key.
    let e = call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "nope", "text": "x" }),
    )
    .expect_err("not in the source");
    assert_eq!(reason(&e), "loc_panel_key_not_found");
    let e = call(
        &mut world,
        "editor.loc_set_comment",
        json!({ "key": "Not A Key", "text": "x" }),
    )
    .expect_err("bad key");
    assert_eq!(reason(&e), "loc_panel_bad_key");
}

/// The panel's context column and edit box: the keyboard reaches Apply context and writes the comment.
#[test]
fn the_context_edit_box_writes_the_comment_from_the_keyboard() {
    let dir = fixture("loc-panel-context-ui");
    let mut h = Headless::new(&dir);
    h.click("context:menu-quit");
    assert_eq!(
        h.panel().selected,
        Some(("menu-quit".to_owned(), "en".to_owned())),
        "the context cell selects its key"
    );
    h.click("context-edit");
    let command = |key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    };
    h.frame(vec![command(egui::Key::A)]);
    h.frame(vec![egui::Event::Text("Pause menu button".to_owned())]);
    h.frame(Vec::new());
    assert_eq!(h.panel().context_text, "Pause menu button");
    h.press_on_focus("context-apply", egui::Key::Enter);
    assert!(h.panel().error.is_none(), "{:?}", h.panel().error);
    let en = read(&dir, "assets/localization/en/main.ftl").expect("en");
    assert!(en.contains("# note: Pause menu button\nmenu-quit = Quit"), "{en}");
}

/// What the runtime loads from a locale's files in development: its `.ftl` files joined in order.
fn dev_localization(dir: &Path) -> regia_ui::localization::Localization {
    let project = model::read_project(dir).expect("reads");
    let sources = project
        .locales
        .iter()
        .map(|l| {
            let text: Vec<&str> = l.files.iter().map(|(_, t)| t.as_str()).collect();
            (l.locale.clone(), text.join("\n"))
        })
        .collect();
    regia_ui::localization::Localization::new(sources, Some("en".to_owned())).expect("dev runtime loads")
}

/// Compile writes the bundles `regia loc compile` writes, and the runtime that loads them shows the same
/// text for every key in every shipped locale as the runtime that loads the `.ftl` files directly.
#[test]
fn compile_writes_the_shipping_bundles_and_the_runtime_loads_the_same_texts() {
    let dir = fixture("loc-panel-compile");
    let mut world = world_with(&dir);
    let out = call(&mut world, "editor.loc_compile", json!({})).expect("compiles");
    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["locales"], json!(["de", "en", "fr"]));
    let bundle_dir = dir.join("build/loc");
    assert_eq!(out["dir"], bundle_dir.display().to_string());
    // The same bytes as the CLI's library call into a second directory.
    let twin = dir.join("twin");
    regia_asset_pipeline::loc_exchange::compile_bundles(&dir, &twin, &[], false).expect("library compile");
    for name in ["de.ftl", "en.ftl", "fr.ftl", "index.ron"] {
        assert_eq!(
            std::fs::read(bundle_dir.join(name)).expect("panel file"),
            std::fs::read(twin.join("loc").join(name)).expect("library file"),
            "{name}"
        );
    }
    // The runtime loading the bundles equals the runtime loading the files.
    let index_text = std::fs::read_to_string(bundle_dir.join("index.ron")).expect("index");
    let index = regia_ui::loc_bundle::BundleIndex::from_ron(&index_text).expect("index parses");
    let mut compiled = regia_ui::localization::Localization::load_bundles(&index, |file| {
        std::fs::read(dir.join("build").join(file)).map_err(|e| e.to_string())
    })
    .expect("bundles load");
    let mut dev = dev_localization(&dir);
    let args = [("name".to_owned(), "Ada".to_owned())];
    let mut compared = 0;
    for locale in ["en", "fr", "de"] {
        assert!(compiled.set_locale(locale) || compiled.locale() == locale);
        assert!(dev.set_locale(locale) || dev.locale() == locale);
        for key in [
            "menu-start",
            "menu-quit",
            "greet",
            "long-one",
            "changed",
            "credits",
        ] {
            assert_eq!(
                compiled.resolve(key, &args),
                dev.resolve(key, &args),
                "{locale}/{key}: compiled bundles and loose files show the same text"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, 18);
    // The panel keeps the result for its report.
    let panel = world.resource::<LocPanel>();
    assert_eq!(panel.last_compile.as_ref().map(|c| c.ok), Some(true));
}

/// A validation error stops the compile before anything is written and comes back as a finding with its
/// file and line, which the panel shows as a link.
#[test]
fn a_failed_compile_reports_the_file_and_line_and_writes_nothing() {
    let dir = fixture("loc-panel-compile-fail");
    write(
        &dir,
        "assets/localization/fr/broken.ftl",
        "ok = fine\n\nbad = { $x\n",
    );
    let mut world = world_with(&dir);
    let out = call(&mut world, "editor.loc_compile", json!({})).expect("a failed validation is a result");
    assert_eq!(out["ok"], false, "{out}");
    assert!(out["errors"].as_u64().unwrap_or(0) >= 1, "{out}");
    let finding = out["findings"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["severity"] == "error"))
        .cloned()
        .expect("an error finding");
    assert_eq!(finding["file"], "assets/localization/fr/broken.ftl", "{finding}");
    assert!(finding["line"].as_u64().is_some(), "{finding}");
    assert!(!dir.join("build/loc").exists(), "nothing is written");
    // Through the panel: the finding is a link that is reachable by keyboard.
    let mut h = Headless::new(&dir);
    h.press_on_focus("compile", egui::Key::Enter);
    let compile = h.panel().last_compile.clone().expect("the button compiled");
    assert!(!compile.ok);
    let location = compile
        .findings
        .iter()
        .find(|f| f.severity == "error")
        .and_then(compile::CompileFinding::location)
        .expect("a location");
    assert!(
        location.starts_with("assets/localization/fr/broken.ftl:"),
        "{location}"
    );
}

/// The preview language: `ui.set_locale` on the edit world now and on a play world that exists or starts
/// later; Source resets; an unknown locale is refused; the Designer's lock flag follows.
#[test]
fn the_preview_locale_follows_into_both_worlds_and_resets_to_source() {
    use regia_ui::localization::Localization;
    let dir = fixture("loc-panel-preview");
    let mut world = world_with(&dir);
    let runtime = || {
        Localization::new(
            vec![
                ("en".to_owned(), "hello = Hello".to_owned()),
                ("fr".to_owned(), "hello = Bonjour".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("runtime")
    };
    world.insert_resource(runtime());
    let mut play = World::new();
    play.insert_resource(runtime());
    let ctx = egui::Context::default();

    let out = preview::set_preview_locale(&mut world, Some(&mut play), Some("fr")).expect("previews");
    assert_eq!(out.locale.as_deref(), Some("fr"));
    assert!(out.edit_world && out.play_world);
    assert_eq!(world.resource::<Localization>().locale(), "fr");
    assert_eq!(play.resource::<Localization>().locale(), "fr");
    assert_eq!(
        world.resource::<Localization>().resolve("hello", &[]),
        "Bonjour",
        "the edit viewport's UI shows the translation"
    );
    preview::sync_frame(&ctx, &mut world, Some(&mut play));
    assert!(
        preview::is_previewing(&ctx),
        "a translation previews: localized text locks"
    );

    // A play session that starts later follows on the next frame.
    let mut later = World::new();
    later.insert_resource(runtime());
    preview::sync_frame(&ctx, &mut world, Some(&mut later));
    assert_eq!(later.resource::<Localization>().locale(), "fr");

    // Source resets both worlds and unlocks.
    let out = preview::set_preview_locale(&mut world, Some(&mut play), None).expect("resets");
    assert_eq!(out.locale, None);
    assert_eq!(world.resource::<Localization>().locale(), "en");
    assert_eq!(play.resource::<Localization>().locale(), "en");
    preview::sync_frame(&ctx, &mut world, Some(&mut play));
    assert!(!preview::is_previewing(&ctx));

    let e = preview::set_preview_locale(&mut world, None, Some("zz")).expect_err("unknown");
    assert_eq!(e.code, "loc_panel_locale_not_found");

    // The agent route: the same call, and `source` resets.
    let set = call(&mut world, "editor.preview_locale", json!({ "locale": "fr" })).expect("agent sets");
    assert_eq!(set["locale"], "fr");
    assert_eq!(set["shown"], "fr");
    assert!(
        set["locales"]
            .as_array()
            .is_some_and(|l| l.iter().any(|v| v == "de"))
    );
    let read = call(&mut world, "editor.preview_locale", json!({})).expect("agent reads");
    assert_eq!(read["locale"], "fr");
    let reset = call(&mut world, "editor.preview_locale", json!({ "locale": "source" })).expect("resets");
    assert_eq!(reset["locale"], Value::Null);
    assert_eq!(reset["shown"], "en");
    let e = call(&mut world, "editor.preview_locale", json!({ "locale": "zz" })).expect_err("unknown");
    assert_eq!(reason(&e), "loc_panel_locale_not_found");
}

/// The toolbar picker is a combo box with a text label: the keyboard opens it, and the pick sets the preview.
#[test]
fn the_toolbar_picker_is_operable_from_the_keyboard() {
    use regia_ui::localization::Localization;
    let dir = fixture("loc-panel-preview-ui");
    let mut world = world_with(&dir);
    world.insert_resource(
        Localization::new(
            vec![
                ("en".to_owned(), "hello = Hello".to_owned()),
                ("fr".to_owned(), "hello = Bonjour".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("runtime"),
    );
    let ctx = egui::Context::default();
    let frame = |world: &mut World, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| preview::preview_locale_picker(ui, world));
        output.textures_delta.clear();
    };
    frame(&mut world, Vec::new());
    frame(&mut world, Vec::new());
    let key = |key, pressed| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let focus = |key: &str, ctx: &egui::Context| {
        let id = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("preview-test-id", key))))
            .unwrap_or_else(|| panic!("no id for {key}"));
        ctx.memory_mut(|m| m.request_focus(id));
    };
    let press = |world: &mut World, name: egui::Key| {
        frame(world, vec![key(name, true), key(name, false)]);
        frame(world, Vec::new());
    };
    // Keyboard focus on the combo box, Enter opens it, focus on the French item, Enter picks it.
    focus("combo", &ctx);
    frame(&mut world, Vec::new());
    press(&mut world, egui::Key::Enter);
    frame(&mut world, Vec::new());
    focus("item:fr", &ctx);
    frame(&mut world, Vec::new());
    press(&mut world, egui::Key::Enter);
    frame(&mut world, Vec::new());
    assert!(
        preview::current(&world).is_some(),
        "the keyboard alone picked a preview locale: {:?}",
        preview::current(&world)
    );
    assert_eq!(preview::current(&world).as_deref(), Some("fr"));
}

/// Review g17 row 1: the toolbar picker and `editor.preview_locale` never see the play world, so a pick of Source
/// must still put a running play session back on the source locale (once, on the next frame), and later frames
/// on Source must leave the game's own locale choice alone. A play session that ended before the reset is not
/// touched in the next one.
#[test]
fn source_from_the_picker_or_the_agent_resets_a_running_play_session() {
    use regia_ui::localization::Localization;
    let dir = fixture("loc-panel-preview-play");
    let mut world = world_with(&dir);
    let runtime = || {
        Localization::new(
            vec![
                ("en".to_owned(), "hello = Hello".to_owned()),
                ("fr".to_owned(), "hello = Bonjour".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("runtime")
    };
    world.insert_resource(runtime());
    let mut play = World::new();
    play.insert_resource(runtime());
    let ctx = egui::Context::default();
    // One editor frame the way the shell draws it: sync into the play world, then the toolbar picker.
    let frame = |world: &mut World, play: &mut World, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            preview::sync_frame(ui.ctx(), world, Some(play));
            preview::preview_locale_picker(ui, world);
        });
        output.textures_delta.clear();
    };
    let key = |key, pressed| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let focus = |key: &str, ctx: &egui::Context| {
        let id = ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("preview-test-id", key))))
            .unwrap_or_else(|| panic!("no id for {key}"));
        ctx.memory_mut(|m| m.request_focus(id));
    };
    // A keyboard pick leaves the drop-down open (egui closes a combo popup on a pointer click or Escape), so
    // each pick starts with Escape; otherwise the next Enter on the combo would close it instead of opening it.
    let pick = |world: &mut World, play: &mut World, item: &str| {
        frame(
            world,
            play,
            vec![key(egui::Key::Escape, true), key(egui::Key::Escape, false)],
        );
        frame(world, play, Vec::new());
        focus("combo", &ctx);
        frame(world, play, Vec::new());
        frame(
            world,
            play,
            vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)],
        );
        frame(world, play, Vec::new());
        focus(item, &ctx);
        frame(world, play, Vec::new());
        frame(
            world,
            play,
            vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)],
        );
        frame(world, play, Vec::new());
        frame(world, play, Vec::new());
    };

    // The picker: French previews in play, then Source puts play back on the source locale.
    pick(&mut world, &mut play, "item:fr");
    assert_eq!(preview::current(&world).as_deref(), Some("fr"));
    assert_eq!(play.resource::<Localization>().locale(), "fr");
    pick(&mut world, &mut play, "item:source");
    assert_eq!(preview::current(&world), None, "the picker chose Source");
    assert_eq!(world.resource::<Localization>().locale(), "en");
    assert_eq!(
        play.resource::<Localization>().locale(),
        "en",
        "Source from the toolbar resets the running play session"
    );
    // While Source stays selected the game chooses its own locale again: later frames leave it alone.
    play.resource_mut::<Localization>().set_locale("fr");
    frame(&mut world, &mut play, Vec::new());
    frame(&mut world, &mut play, Vec::new());
    assert_eq!(
        play.resource::<Localization>().locale(),
        "fr",
        "the game's own choice holds"
    );
    play.resource_mut::<Localization>().set_locale("en");

    // The agent route: `editor.preview_locale` never has the play world either.
    call(&mut world, "editor.preview_locale", json!({ "locale": "fr" })).expect("agent sets");
    preview::sync_frame(&ctx, &mut world, Some(&mut play));
    assert_eq!(play.resource::<Localization>().locale(), "fr");
    call(&mut world, "editor.preview_locale", json!({ "locale": "source" })).expect("agent resets");
    preview::sync_frame(&ctx, &mut world, Some(&mut play));
    assert_eq!(
        play.resource::<Localization>().locale(),
        "en",
        "Source from the agent resets the running play session"
    );
    play.resource_mut::<Localization>().set_locale("fr");
    preview::sync_frame(&ctx, &mut world, Some(&mut play));
    assert_eq!(
        play.resource::<Localization>().locale(),
        "fr",
        "the game's own choice holds"
    );

    // A session that ended before the reset: the next session keeps the game's own choice.
    let mut first = World::new();
    first.insert_resource(runtime());
    call(&mut world, "editor.preview_locale", json!({ "locale": "fr" })).expect("agent sets");
    preview::sync_frame(&ctx, &mut world, Some(&mut first));
    assert_eq!(first.resource::<Localization>().locale(), "fr");
    preview::sync_frame(&ctx, &mut world, None);
    call(&mut world, "editor.preview_locale", json!({ "locale": "source" })).expect("agent resets");
    let mut next = World::new();
    next.insert_resource(runtime());
    next.resource_mut::<Localization>().set_locale("fr");
    preview::sync_frame(&ctx, &mut world, Some(&mut next));
    assert_eq!(
        next.resource::<Localization>().locale(),
        "fr",
        "a new session after the reset keeps the game's own choice"
    );
}
