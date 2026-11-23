// SPDX-License-Identifier: MPL-2.0
//! The project browser's tags, thumbnails, search and keyboard path (ADR-7856,
//! `book/agents/project-browser.md`). CPU only.
//!
//! - Tags are stored in a sidecar (`recent_projects_meta.ron`); the recent list itself is unchanged.
//! - A tag chip and the `tag:` search term filter the list; the agent answers the same filter.
//! - Each row shows its thumbnail source, path, last opened and engine version.
//! - Up, Down and Enter operate the list; every control has a spoken name; Remove from list
//!   leaves the project folder alone.

use std::path::{Path, PathBuf};

use egui::{Event, Key, Modifiers, Pos2, Rect, vec2};
use regia_ecs::prelude::World;
use regia_editor::project::{self, BrowserAction, ProjectBrowserState};
use serde_json::{Value, json};

fn call(method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(&mut World::new(), &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason").or_else(|| d.get("code")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn make_project(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("Regia.toml"), format!("[project]\nname = \"{name}\"\n")).expect("manifest");
    root
}

/// Three recent projects (alpha, beta, gamma; gamma newest) under a fresh recent file.
fn three_projects(label: &str) -> (PathBuf, Vec<PathBuf>) {
    let dir = regia_test_utils::unique_temp_dir(label);
    project::set_recent_file_for_tests(dir.join("config/recent_projects.txt"));
    let roots: Vec<PathBuf> = ["alpha", "beta", "gamma"]
        .iter()
        .map(|name| make_project(&dir, name))
        .collect();
    for root in &roots {
        project::remember_recent(root);
    }
    (dir, roots)
}

struct Harness {
    ctx: egui::Context,
    time: f64,
    state: ProjectBrowserState,
    texts: Vec<String>,
    rects: Vec<(String, Rect)>,
}

impl Harness {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            time: 0.0,
            state: ProjectBrowserState {
                parent: String::new(),
                ..Default::default()
            },
            texts: Vec::new(),
            rects: Vec::new(),
        }
    }

    fn frame(&mut self, events: Vec<Event>) -> Option<BrowserAction> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, Rect)>) {
            match shape {
                egui::Shape::Text(text) => out.push((
                    text.galley.text().to_owned(),
                    Rect::from_min_size(text.pos, text.galley.size()),
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        self.time += 0.05;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 2400.0))),
            events,
            time: Some(self.time),
            ..Default::default()
        };
        let mut action = None;
        let state = &mut self.state;
        let mut output = self.ctx.run_ui(raw, |ui| {
            if let Some(a) = project::browser_panel(ui, state) {
                action = Some(a);
            }
        });
        output.textures_delta.clear();
        let mut found = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut found);
        }
        self.rects = found.clone();
        self.texts = found.into_iter().map(|(t, _)| t).collect();
        action
    }

    fn drawn(&self, text: &str) -> bool {
        self.texts.iter().any(|t| t == text)
    }

    fn key(&mut self, key: Key) -> Option<BrowserAction> {
        let event = |pressed| Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let down = self.frame(vec![event(true)]);
        let up = self.frame(vec![event(false)]);
        down.or(up)
    }

    fn click(&mut self, text: &str) -> Option<BrowserAction> {
        self.frame(vec![]);
        self.frame(vec![]);
        let at = self
            .rects
            .iter()
            .find(|(t, _)| t == text)
            .map(|(_, r)| r.center())
            .unwrap_or_else(|| panic!("`{text}` is drawn; drawn: {:?}", self.texts));
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        self.frame(vec![Event::PointerMoved(at)]);
        let a = self.frame(vec![button(true)]);
        let b = self.frame(vec![button(false)]);
        let c = self.frame(vec![]);
        a.or(b).or(c)
    }
}

#[test]
fn tags_are_kept_in_a_sidecar_and_the_recent_list_is_unchanged() {
    let (dir, roots) = three_projects("pb-sidecar");
    let list_before = std::fs::read_to_string(project::recent_file()).expect("list");
    let set = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "tags": ["Mobile", " prototype ", "mobile", "two words"] }),
    )
    .expect("tags");
    assert_eq!(
        set["tags"],
        json!(["mobile", "prototype", "two-words"]),
        "normalized, deduplicated, sorted"
    );
    assert_eq!(
        std::fs::read_to_string(project::recent_file()).expect("list"),
        list_before,
        "recent_projects.txt keeps its shape"
    );
    let sidecar = std::fs::read_to_string(project::recent_file().with_file_name("recent_projects_meta.ron"))
        .expect("the sidecar");
    assert!(
        sidecar.contains("mobile") && sidecar.contains("version"),
        "{sidecar}"
    );
    let listed = call("editor.recent_projects", json!({})).expect("list");
    let entries = listed["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 3);
    let alpha = entries.iter().find(|e| e["name"] == "alpha").expect("alpha");
    assert_eq!(alpha["tags"], json!(["mobile", "prototype", "two-words"]));
    assert_eq!(
        alpha["engine_version"],
        regia_core::engine_version::CURRENT.to_string()
    );
    assert!(
        alpha["last_opened"].as_u64().is_some_and(|t| t > 1_700_000_000),
        "{alpha}"
    );
    assert_eq!(listed["tags"], json!(["mobile", "prototype", "two-words"]));
    // A newer sidecar is read as empty and never overwritten.
    let newer = "(version: 99, projects: {})";
    std::fs::write(
        project::recent_file().with_file_name("recent_projects_meta.ron"),
        newer,
    )
    .expect("newer");
    let err = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[1].display().to_string(), "tags": ["x"] }),
    )
    .expect_err("unsupported");
    assert_eq!(reason(&err), "recent_meta_unsupported");
    assert_eq!(
        std::fs::read_to_string(project::recent_file().with_file_name("recent_projects_meta.ron"))
            .expect("kept"),
        newer
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_agent_filters_by_tag_and_by_search_like_the_browser() {
    let (dir, roots) = three_projects("pb-filter");
    for (root, tags) in [
        (&roots[0], json!(["mobile"])),
        (&roots[2], json!(["mobile", "demo"])),
    ] {
        call(
            "editor.recent_projects_edit",
            json!({ "path": root.display().to_string(), "tags": tags }),
        )
        .expect("tag");
    }
    let names = |params: Value| -> Vec<String> {
        call("editor.recent_projects", params).expect("list")["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter_map(|e| e["name"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(names(json!({})), ["gamma", "beta", "alpha"], "newest first");
    assert_eq!(names(json!({ "tag": "mobile" })), ["gamma", "alpha"]);
    assert_eq!(names(json!({ "query": "tag:demo" })), ["gamma"]);
    assert_eq!(
        names(json!({ "query": "TAG:mobile alp" })),
        ["alpha"],
        "words and tag terms together"
    );
    assert_eq!(
        names(json!({ "query": "tag:demo", "tag": "mobile" })),
        ["gamma"],
        "both must hold"
    );
    assert_eq!(names(json!({ "query": "beta" })), ["beta"]);
    assert!(names(json!({ "tag": "nothing" })).is_empty());
    let bad = call("editor.recent_projects", json!({ "query": 3 })).expect_err("type");
    assert_eq!(bad.code, regia_agent::protocol::INVALID_PARAMS);
    let too_long = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[1].display().to_string(), "tags": ["x".repeat(40)] }),
    )
    .expect_err("long tag");
    assert_eq!(reason(&too_long), "recent_tag_invalid");
    let stranger = call(
        "editor.recent_projects_edit",
        json!({ "path": dir.join("elsewhere").display().to_string(), "tags": ["x"] }),
    )
    .expect_err("not listed");
    assert_eq!(reason(&stranger), "recent_not_found");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tag_chip_and_the_search_box_filter_the_drawn_list() {
    let (dir, roots) = three_projects("pb-chips");
    call(
        "editor.recent_projects_edit",
        json!({ "path": roots[1].display().to_string(), "tags": ["demo"] }),
    )
    .expect("tag");
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.frame(vec![]);
    for root in &roots {
        assert!(
            ui.drawn(&root.display().to_string()),
            "{root:?} listed: {:?}",
            ui.texts
        );
    }
    assert!(ui.drawn("#demo"), "the chip for the tag in use: {:?}", ui.texts);
    ui.click("#demo");
    ui.frame(vec![]);
    assert!(ui.drawn(&roots[1].display().to_string()));
    assert!(!ui.drawn(&roots[0].display().to_string()), "alpha lacks the tag");
    assert!(!ui.drawn(&roots[2].display().to_string()), "gamma lacks the tag");
    // Clicking the chip again clears the filter.
    ui.click("#demo");
    ui.frame(vec![]);
    assert!(ui.drawn(&roots[0].display().to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn up_down_enter_open_a_project_without_the_mouse() {
    let (dir, roots) = three_projects("pb-keys");
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.frame(vec![]);
    assert_eq!(
        ui.key(Key::Enter),
        None,
        "nothing highlighted yet: Enter does nothing"
    );
    ui.key(Key::ArrowDown);
    ui.key(Key::ArrowDown);
    // The list is newest first: gamma, beta, alpha. Two Downs highlight beta.
    assert_eq!(ui.key(Key::Enter), Some(BrowserAction::Open(roots[1].clone())));
    ui.key(Key::ArrowUp);
    assert_eq!(ui.key(Key::Enter), Some(BrowserAction::Open(roots[2].clone())));
    for _ in 0..5 {
        ui.key(Key::ArrowDown);
    }
    assert_eq!(
        ui.key(Key::Enter),
        Some(BrowserAction::Open(roots[0].clone())),
        "the highlight stops at the last row"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clicking_a_projects_path_still_opens_it() {
    let (dir, roots) = three_projects("pb-click");
    let mut ui = Harness::new();
    let action = ui.click(&roots[0].display().to_string());
    assert_eq!(action, Some(BrowserAction::Open(roots[0].clone())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_from_list_forgets_the_project_but_not_its_folder() {
    let (dir, roots) = three_projects("pb-remove");
    call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "tags": ["old"] }),
    )
    .expect("tag");
    let removed = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "remove": true }),
    )
    .expect("remove");
    assert_eq!(removed["removed"], true);
    let listed = call("editor.recent_projects", json!({})).expect("list");
    assert_eq!(listed["entries"].as_array().expect("entries").len(), 2);
    assert_eq!(listed["tags"], json!([]), "its tags went with it");
    assert!(roots[0].join("Regia.toml").is_file(), "the folder is untouched");
    let again = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "remove": true }),
    )
    .expect_err("already gone");
    assert_eq!(reason(&again), "recent_not_found");
    let both = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[1].display().to_string(), "remove": true, "tags": [] }),
    )
    .expect_err("both");
    assert_eq!(both.code, regia_agent::protocol::INVALID_PARAMS);

    // The same through the panel: the button removes the row.
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.frame(vec![]);
    assert!(ui.drawn(&roots[1].display().to_string()));
    // gamma is the first row, so the first button is its.
    ui.click("Remove from list");
    ui.frame(vec![]);
    assert!(
        !ui.drawn(&roots[2].display().to_string()),
        "gamma is off the list: {:?}",
        ui.texts
    );
    assert!(ui.drawn(&roots[1].display().to_string()));
    assert!(roots[2].join("Regia.toml").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tag_editor_adds_a_tag_from_the_keyboard() {
    let (dir, roots) = three_projects("pb-tag-edit");
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.frame(vec![]);
    // Highlight gamma (the first row): its tag field appears.
    ui.key(Key::ArrowDown);
    ui.frame(vec![]);
    assert!(
        ui.texts.iter().any(|t| t == "Add tag"),
        "the tag editor of the highlighted row: {:?}",
        ui.texts
    );
    let typed = Event::Text("shipping".into());
    ui.click("Add a tag");
    ui.frame(vec![typed]);
    ui.click("Add tag");
    ui.frame(vec![]);
    let listed = call("editor.recent_projects", json!({ "tag": "shipping" })).expect("list");
    assert_eq!(
        listed["projects"],
        json!([roots[2].display().to_string()]),
        "{listed}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_row_shows_its_thumbnail_source_date_and_engine_version() {
    let (dir, roots) = three_projects("pb-thumb");
    let entries = regia_editor::recent_meta::entries();
    assert!(entries.iter().all(|e| e.thumbnail.is_none()), "no picture yet");
    // The project icon wins; the manifest's `icon` key names it.
    let rgba: Vec<u8> = [200u8, 40, 40, 255].repeat(8 * 8);
    let mut png = Vec::new();
    {
        use image::ImageEncoder as _;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&rgba, 8, 8, image::ExtendedColorType::Rgba8)
            .expect("png");
    }
    std::fs::write(roots[0].join("icon.png"), &png).expect("icon");
    assert_eq!(
        regia_editor::recent_meta::thumbnail_source(&roots[0]),
        Some(roots[0].join("icon.png"))
    );
    // Without an icon the saved viewport capture is the picture.
    let saved =
        regia_editor::recent_meta::save_viewport_thumbnail(&roots[1], 256, 128, &[90u8; 256 * 128 * 4])
            .expect("save capture");
    assert!(saved.is_file());
    assert_eq!(
        regia_editor::recent_meta::thumbnail_source(&roots[1]),
        Some(saved.clone())
    );
    let size = image::image_dimensions(&saved).expect("dimensions");
    assert_eq!(size, (128, 64), "aspect kept inside the 128 px box");
    assert!(
        regia_editor::recent_meta::save_viewport_thumbnail(&roots[1], 4, 4, &[0; 3]).is_err(),
        "a picture whose bytes do not match its size is refused"
    );
    // An icon key may not leave the project.
    std::fs::write(
        roots[2].join("Regia.toml"),
        "[project]\nname = \"gamma\"\nicon = \"../beta/icon.png\"\n",
    )
    .expect("manifest");
    std::fs::write(roots[1].join("icon.png"), &png).expect("icon");
    assert_eq!(regia_editor::recent_meta::thumbnail_source(&roots[2]), None);
    // The panel draws the date and the version for every opened project.
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.frame(vec![]);
    let version = regia_core::engine_version::CURRENT.to_string();
    assert!(
        ui.texts
            .iter()
            .any(|t| t.starts_with("Last opened 20") && t.contains(&format!("Engine {version}"))),
        "{:?}",
        ui.texts
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_browser_widget_has_a_spoken_name() {
    let (dir, roots) = three_projects("pb-a11y");
    call(
        "editor.recent_projects_edit",
        json!({ "path": roots[2].display().to_string(), "tags": ["demo"] }),
    )
    .expect("tag");
    let mut ui = Harness::new();
    ui.frame(vec![]);
    ui.key(Key::ArrowDown);
    ui.ctx.enable_accesskit();
    let mut update = None;
    for _ in 0..3 {
        ui.time += 0.05;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 2400.0))),
            time: Some(ui.time),
            ..Default::default()
        };
        let state = &mut ui.state;
        let mut output = ui.ctx.run_ui(raw, |ui| {
            project::browser_panel(ui, state);
        });
        output.textures_delta.clear();
        update = output.platform_output.accesskit_update;
    }
    let update = update.expect("accesskit is on");
    let names = regia_editor::a11y_audit::widget_names(&update);
    assert_eq!(
        regia_editor::a11y_audit::unnamed_widgets(&update),
        Vec::<String>::new(),
        "{names:?}"
    );
    for want in [
        "Search recent projects",
        "Filter by tag demo",
        "Remove tag demo from gamma",
        "Show gamma in file manager",
        "Remove gamma from the recent list",
        "New tag for gamma",
    ] {
        assert!(names.iter().any(|(_, n)| n == want), "{want} in {names:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dates_and_file_manager_command_are_pure() {
    use regia_editor::recent_meta::{file_manager_program, format_date};
    assert_eq!(format_date(0), "1970-01-01");
    assert_eq!(format_date(951_782_400), "2000-02-29", "a leap day");
    assert_eq!(format_date(1_700_000_000), "2023-11-14");
    assert_eq!(format_date(4_102_444_799), "2099-12-31");
    assert!(["xdg-open", "explorer", "open"].contains(&file_manager_program()));
}

/// Review g17 finding 3: a torn or mistyped sidecar is never saved over. Opening a project,
/// a tag edit and Remove from list leave its bytes as they are (the writers refuse with
/// `recent_meta_unreadable`); the browser still lists the projects, without tags.
#[test]
fn a_corrupt_sidecar_is_never_overwritten() {
    let (dir, roots) = three_projects("pb-corrupt");
    call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "tags": ["mobile"] }),
    )
    .expect("tag");
    let sidecar = project::recent_file().with_file_name("recent_projects_meta.ron");
    let list = project::recent_file();
    let torn = "(version: 1, projects: {\n    \"/home/me/forest\": (tags: [\"demo\"";
    std::fs::write(&sidecar, torn).expect("torn sidecar");

    project::remember_recent(&roots[1]);
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        torn,
        "opening a project keeps the sidecar"
    );
    let list_before = std::fs::read(&list).expect("list");

    let err = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[0].display().to_string(), "tags": ["x"] }),
    )
    .expect_err("tags over an unreadable sidecar");
    assert_eq!(reason(&err), "recent_meta_unreadable", "{err:?}");
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        torn,
        "set_tags keeps it"
    );

    let err = call(
        "editor.recent_projects_edit",
        json!({ "path": roots[2].display().to_string(), "remove": true }),
    )
    .expect_err("remove over an unreadable sidecar");
    assert_eq!(reason(&err), "recent_meta_unreadable", "{err:?}");
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        torn,
        "remove keeps it"
    );
    assert_eq!(
        std::fs::read(&list).expect("list"),
        list_before,
        "a refused remove leaves the recent list too"
    );

    let listed = call("editor.recent_projects", json!({})).expect("the list still reads");
    assert_eq!(listed["entries"].as_array().map(Vec::len), Some(3), "{listed}");
    assert_eq!(listed["tags"], json!([]), "{listed}");
    let _ = std::fs::remove_dir_all(&dir);
}
