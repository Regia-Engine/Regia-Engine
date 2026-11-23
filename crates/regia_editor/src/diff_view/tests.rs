// SPDX-License-Identifier: MPL-2.0
//! Tests of the diff window: the keyboard path (accessibility bar), the shared view, the layout,
//! the tree rows and the revision picker over a temp git repo.

use std::path::Path;

use egui::{Key, Modifiers};

use super::*;
use crate::asset_diff::{AssetKind, DiffSide, diff_texts};
use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};

const MAT_OLD: &str = r#"(
    format_version: 3,
    name: "fixture",
    nodes: {
        "base": Float(0.5),
        "tint": Vec3((1.0, 0.5, 0.25)),
        "spare": Float(2.0),
        "mix": Mul("tint", "base"),
    },
    output: (base_color: Some("mix")),
)"#;

const MAT_NEW: &str = r#"(
    format_version: 3,
    name: "fixture",
    nodes: {
        "base": Float(0.8),
        "tint": Vec3((1.0, 0.5, 0.25)),
        "mix": Mul("tint", "base"),
        "gloss": Float(0.3),
    },
    output: (base_color: Some("mix")),
)"#;

const SIZE: [f32; 2] = [1400.0, 900.0];

fn side(rev: Option<&str>) -> DiffSide {
    DiffSide {
        rev: rev.map(str::to_string),
        hash: None,
        path: "assets/m.matgraph.ron".into(),
    }
}

fn material_window() -> DiffWindow {
    let body = diff_texts(AssetKind::MaterialGraph, MAT_OLD, None, MAT_NEW, None).expect("diff");
    DiffWindow::for_result(
        Path::new("/nonexistent-project"),
        AssetDiff {
            path: "assets/m.matgraph.ron".into(),
            kind: AssetKind::MaterialGraph,
            old: side(Some("HEAD")),
            new: side(None),
            body,
        },
    )
}

fn tree_window() -> DiffWindow {
    let body = diff_texts(
        AssetKind::Ron,
        "(a: (b: 1, c: 2), d: [1, 2])",
        None,
        "(a: (b: 1, c: 3), d: [1, 2])",
        None,
    )
    .expect("diff");
    DiffWindow::for_result(
        Path::new("/nonexistent-project"),
        AssetDiff {
            path: "assets/x.ron".into(),
            kind: AssetKind::Ron,
            old: side(Some("HEAD")),
            new: side(None),
            body,
        },
    )
}

fn names(w: &DiffWindow) -> Vec<String> {
    let d = w.diff().expect("diff");
    d.body.entries().iter().map(|e| d.body.describe(e)).collect()
}

/// Keyboard path: Tab reaches the change list, Down moves, Enter selects the change and
/// centers its node in both canvases (one shared view).
#[test]
fn change_list_is_keyboard_operable() {
    let ctx = egui::Context::default();
    let mut w = material_window();
    let rows = names(&w);
    assert_eq!(rows.len(), 3, "{rows:?}");
    let first = rows[0].clone();
    tab_to(&ctx, SIZE, &first, false, 30, |ui| window_ui(ui, &mut w));
    frame(&ctx, SIZE, vec![key(Key::ArrowDown, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    let update = frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    assert_eq!(focused_name(&update).as_deref(), Some(rows[1].as_str()));
    frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    assert_eq!(w.selected, Some(1));
    let DiffBody::Graph(g) = &w.diff().expect("diff").body else {
        panic!("graph");
    };
    let ChangeEntry::Node(n) = w.entries()[1] else {
        panic!("a node entry");
    };
    let p = Layout::of(g).node_pos(n, Side::New).expect("placed");
    assert_eq!(
        w.view.center,
        [p[0] + NODE_SIZE[0] * 0.5, p[1] + NODE_SIZE[1] * 0.5],
        "Enter centers the node"
    );
}

/// The zoom buttons and both canvases are reachable by Tab; a focused canvas pans with the
/// arrows and zooms with `+`, and the two canvases share the view.
#[test]
fn zoom_and_canvas_keys() {
    let ctx = egui::Context::default();
    let mut w = material_window();
    // Settle the first-frame fit.
    frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    let zoom = w.view.zoom;
    tab_to(&ctx, SIZE, "Zoom in", false, 40, |ui| window_ui(ui, &mut w));
    frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    assert!(w.view.zoom > zoom, "Zoom in: {} -> {}", zoom, w.view.zoom);
    let update = frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    // The tree update lists nodes in no particular order.
    let mut canvases = labels_of(&update, egui::accesskit::Role::Canvas);
    canvases.sort();
    assert_eq!(
        canvases,
        vec![
            "New graph (working copy), 4 nodes".to_string(),
            "Old graph (HEAD), 4 nodes".to_string(),
        ]
    );
    let old_canvas = canvases[1].clone();
    tab_to(&ctx, SIZE, &old_canvas, false, 40, |ui| window_ui(ui, &mut w));
    let center = w.view.center;
    frame(&ctx, SIZE, vec![key(Key::ArrowRight, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    assert!(
        w.view.center[0] > center[0],
        "Right pans the view: {center:?} -> {:?}",
        w.view.center
    );
    let zoom = w.view.zoom;
    frame(&ctx, SIZE, vec![key(Key::Plus, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    assert!(w.view.zoom > zoom);
}

#[test]
fn view_zoom_is_clamped_and_fit_covers_the_points() {
    let mut v = GraphView::default();
    v.zoom_by(100.0);
    assert_eq!(v.zoom, ZOOM_RANGE.1);
    v.zoom_by(0.0001);
    assert_eq!(v.zoom, ZOOM_RANGE.0);
    v.fit(&[[0.0, 0.0], [1000.0, 400.0]], egui::vec2(500.0, 400.0));
    assert_eq!(v.center, [500.0, 200.0]);
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 400.0));
    for p in [[0.0, 0.0], [1000.0, 400.0]] {
        assert!(rect.contains(v.to_screen(rect, p)), "{p:?} is on screen");
    }
}

/// Unmoved nodes sit in the same place on both sides; consumers sit right of their inputs.
#[test]
fn layout_is_shared_and_follows_wires() {
    let w = material_window();
    let DiffBody::Graph(g) = &w.diff().expect("diff").body else {
        panic!("graph");
    };
    let layout = Layout::of(g);
    let at = |id: &str, side: Side| {
        let i = g.nodes.iter().position(|n| n.id() == id).expect(id);
        layout.node_pos(i, side).expect("placed")
    };
    assert_eq!(at("tint", Side::Old), at("tint", Side::New));
    assert!(at("mix", Side::New)[0] > at("tint", Side::New)[0]);
    assert!(layout.endpoint(g, "output", Side::New).expect("output")[0] > at("mix", Side::New)[0]);
}

#[test]
fn tree_rows_show_changes_or_everything() {
    let w = tree_window();
    let DiffBody::Tree { changes, old, new } = &w.diff().expect("diff").body else {
        panic!("tree");
    };
    let only = tree_rows(changes, old, new, false);
    assert_eq!(only.len(), 1);
    assert_eq!(
        (only[0].path.as_str(), only[0].old.as_str(), only[0].new.as_str()),
        ("a.c", "2", "3")
    );
    let all = tree_rows(changes, old, new, true);
    let paths: Vec<&str> = all.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["a.b", "a.c", "d[0]", "d[1]"]);
    assert_eq!(all[1].status, ChangeKind::Changed);
    assert!(
        all.iter()
            .filter(|r| r.path != "a.c")
            .all(|r| r.status == ChangeKind::Unchanged)
    );
}

/// The tree view's toggle and change rows are keyboard operable too.
#[test]
fn tree_view_keyboard_path() {
    let ctx = egui::Context::default();
    let mut w = tree_window();
    let rows = names(&w);
    assert_eq!(rows, vec!["Changed a.c".to_string()]);
    tab_to(&ctx, SIZE, &rows[0], false, 30, |ui| window_ui(ui, &mut w));
    frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    assert_eq!(w.selected, Some(0));
    tab_to(&ctx, SIZE, "Show unchanged fields", false, 30, |ui| {
        window_ui(ui, &mut w)
    });
    frame(&ctx, SIZE, vec![key(Key::Space, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
    assert!(w.show_unchanged);
}

/// Status colors come from theme tokens (a custom theme recolors them).
#[test]
fn status_colors_are_theme_tokens() {
    let mut tokens = crate::theme::ThemeTokens::regia_default();
    tokens.accents.ok = egui::Color32::from_rgb(1, 2, 3);
    tokens.error = egui::Color32::from_rgb(4, 5, 6);
    tokens.warn = egui::Color32::from_rgb(7, 8, 9);
    tokens.muted = egui::Color32::from_rgb(10, 11, 12);
    assert_eq!(status_color(&tokens, ChangeKind::Added), tokens.accents.ok);
    assert_eq!(status_color(&tokens, ChangeKind::Removed), tokens.error);
    assert_eq!(status_color(&tokens, ChangeKind::Changed), tokens.warn);
    assert_eq!(status_color(&tokens, ChangeKind::Unchanged), tokens.muted);
}

/// A project outside git shows "No revision control" in the window.
#[test]
fn no_revision_control_message() {
    let dir = regia_test_utils::TempDir::new("diff-view-nogit");
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    std::fs::write(dir.join("assets/a.ron"), "(a: 1)").expect("write");
    let w = DiffWindow::for_diff(&dir, "assets/a.ron", None);
    // A temp dir is outside any work tree on the test hosts; if one ever is not, the diff runs.
    if let DiffContent::Error(e) = &w.content {
        assert_eq!(e.code, "no_revision_control");
        let ctx = egui::Context::default();
        let mut w = w;
        let update = frame(&ctx, SIZE, vec![], |ui| window_ui(ui, &mut w));
        // egui gives a label's text as the node's value.
        let shown: Vec<String> = update
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == egui::accesskit::Role::Label)
            .filter_map(|(_, n)| n.value().map(str::to_string))
            .collect();
        assert!(
            shown.iter().any(|l| l.starts_with("No revision control")),
            "{shown:?}"
        );
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Regia Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap_or(dir))
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Diff against revision…: the picker lists the history; Enter on an entry shows the diff.
#[test]
fn picker_lists_history_and_enter_diffs() {
    let dir = regia_test_utils::TempDir::new("diff-view-picker");
    git(&dir, &["init", "-q"]);
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    std::fs::write(dir.join("assets/m.matgraph.ron"), MAT_OLD).expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "first graph"]);
    std::fs::write(dir.join("assets/m.matgraph.ron"), MAT_NEW).expect("write");
    let mut w = DiffWindow::for_picker(&dir, "assets/m.matgraph.ron");
    let DiffContent::Picker(list) = &w.content else {
        panic!("picker: {:?}", w.content);
    };
    assert_eq!(list.len(), 1);
    let row = format!("{} {} Regia Test: first graph", list[0].short, list[0].date);
    let ctx = egui::Context::default();
    tab_to(&ctx, SIZE, &row, false, 20, |ui| window_ui(ui, &mut w));
    frame(&ctx, SIZE, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        window_ui(ui, &mut w)
    });
    let diff = w.diff().expect("Enter diffs against the picked revision");
    assert_eq!(diff.body.entries().len(), 3);
    assert_eq!(diff.new.rev, None);
}

/// Review sprint-w12-rev-g7 #3: a pan or zoom source applies once per egui frame, so the
/// discarded second pass of a frame does not move the shared view again; the two canvases and the
/// toolbar each have their own gate.
#[test]
fn view_input_applies_once_per_egui_frame_and_source() {
    let mut w = DiffWindow::new(Path::new("."), "assets/a.material.ron");
    assert!(w.view_input_once(ViewSource::Canvas(Side::Old), 7));
    assert!(
        !w.view_input_once(ViewSource::Canvas(Side::Old), 7),
        "the second pass of frame 7"
    );
    assert!(
        w.view_input_once(ViewSource::Canvas(Side::New), 7),
        "the other canvas has its own gate"
    );
    assert!(w.view_input_once(ViewSource::Toolbar, 7));
    assert!(!w.view_input_once(ViewSource::Toolbar, 7));
    assert!(
        w.view_input_once(ViewSource::Canvas(Side::Old), 8),
        "the next frame"
    );
}
