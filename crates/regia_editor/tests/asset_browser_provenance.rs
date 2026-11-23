// SPDX-License-Identifier: MPL-2.0
//! The Content Browser shows provenance and review state (ADR-2428): drawn headless in one egui frame,
//! the list rows carry each asset's maker and review badge, and a `config/provenance.ron` policy adds
//! its error count. No GPU and no clock.

use std::path::Path;

use regia_asset::{AssetId, MetaSidecar, Provenance, ProvenanceTool, ReviewerState, meta_path_for};
use regia_ecs::prelude::*;
use regia_editor::content::{ContentBrowserState, content_browser_panel};

/// Text drawn in one egui frame.
fn frame_text(ctx: &egui::Context, mut draw: impl FnMut(&mut egui::Ui)) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 900.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| draw(ui));
    });
    output.textures_delta.clear();
    let mut out = String::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

fn browser_world(root: &Path) -> (World, ContentBrowserState) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    let open = regia_editor::level_io::new_level(&mut world);
    world.insert_resource(open);
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let state = ContentBrowserState {
        project_root: Some(root.to_path_buf()),
        ..Default::default()
    };
    (world, state)
}

fn draw(world: &mut World, state: &mut ContentBrowserState, ctx: &egui::Context) -> String {
    let mut open = world
        .remove_resource::<regia_editor::level_io::OpenLevel>()
        .expect("open level");
    let text = frame_text(ctx, |ui| content_browser_panel(ui, world, state, &mut open));
    world.insert_resource(open);
    text
}

fn asset(root: &Path, rel: &str, provenance: Option<Provenance>) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    regia_test_utils::write_atomic(&path, b"(format_version: 1)").expect("source");
    let mut meta = MetaSidecar::new("material_ron", AssetId::new_random());
    meta.provenance = provenance;
    regia_test_utils::write_atomic(
        &meta_path_for(&path),
        meta.to_ron_string().expect("ron").as_bytes(),
    )
    .expect("meta");
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle} in:\n{text}"))
}

#[test]
fn rows_show_maker_review_state_and_policy_errors() {
    let root = regia_test_utils::unique_temp_dir("asset_browser_provenance");
    asset(
        &root,
        "assets/agent.material.ron",
        Some(Provenance {
            tool: ProvenanceTool::Agent,
            client: Some("claude".into()),
            ..Provenance::default()
        }),
    );
    asset(
        &root,
        "assets/reviewed.material.ron",
        Some(Provenance {
            tool: ProvenanceTool::Agent,
            reviewer: ReviewerState::Owner,
            origin: Some("dcc:blender".into()),
            ..Provenance::default()
        }),
    );
    asset(&root, "assets/plain.material.ron", None);
    let (mut world, mut state) = browser_world(&root);
    let ctx = egui::Context::default();
    let text = draw(&mut world, &mut state, &ctx);
    assert!(
        line_with(&text, "agent.material.ron").contains("agent \u{b7} review pending"),
        "{text}"
    );
    assert!(
        line_with(&text, "reviewed.material.ron").contains("agent \u{b7} \u{2713} owner"),
        "{text}"
    );
    assert!(
        line_with(&text, "plain.material.ron").ends_with("plain.material.ron"),
        "no provenance, no policy: no badge\n{text}"
    );
    assert!(
        !text.contains("\u{26a0} policy"),
        "no policy file: no policy badge\n{text}"
    );

    // A policy: every asset needs an origin, and agent work a review.
    std::fs::create_dir_all(root.join("config")).expect("config");
    regia_test_utils::write_atomic(
        &root.join("config/provenance.ron"),
        b"(kinds: { \"*\": (required: [\"origin\"]) }, review: (required_for: [agent]))",
    )
    .expect("policy");
    regia_editor::content_index::invalidate(&mut world);
    let text = draw(&mut world, &mut state, &ctx);
    assert!(
        line_with(&text, "agent.material.ron").contains("\u{26a0} policy 2"),
        "{text}"
    );
    assert!(
        line_with(&text, "plain.material.ron").contains("\u{26a0} policy 1"),
        "{text}"
    );
    assert!(
        !line_with(&text, "reviewed.material.ron").contains("policy"),
        "{text}"
    );
    let badges = world
        .get_resource::<regia_editor::content_provenance::ProvenanceBadges>()
        .expect("badges resource");
    assert!(badges.has_policy());

    // An invalid policy shows its error above the list instead of badges that cannot be trusted.
    regia_test_utils::write_atomic(&root.join("config/provenance.ron"), b"(kinds: 1)").expect("bad policy");
    regia_editor::content_index::invalidate(&mut world);
    let text = draw(&mut world, &mut state, &ctx);
    assert!(text.contains("config/provenance.ron is invalid"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}
