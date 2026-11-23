// SPDX-License-Identifier: MPL-2.0
//! Extension workspaces register through the real editor extension host.
use regia_app::prelude::*;
use regia_editor::extension::context::WorkspaceDesc;
use regia_editor::extension::{EditorExtensionPlugin, ToolExtensionContext};
use regia_editor::shell::Panel;
use regia_editor::workspaces::{DockViews, EditorMode, ViewRequest};
use regia_framework::extension::{ExtensionContext, ExtensionError, RegiaExtension};
use serde_json::json;
use std::sync::Arc;

struct WorkspaceExtension;
impl RegiaExtension for WorkspaceExtension {
    fn id(&self) -> &str {
        "workspace_test"
    }
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        ToolExtensionContext::new(cx)?.register_workspace(WorkspaceDesc {
            id: "workspace_test.terrain".into(),
            title: "Terrain review".into(),
            layout: regia_editor::editor_layout::LayoutDoc::from_dock(&egui_dock::DockState::new(vec![
                Panel::Viewport,
                Panel::Terrain,
            ])),
            mode: Some(EditorMode::Terrain),
        })
    }
}

fn dispatch(
    app: &mut App,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    let (inbox, requests) = crossbeam_channel::unbounded();
    let (reply, answer) = crossbeam_channel::bounded(1);
    app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
    inbox
        .send(regia_agent::server::Inbound {
            request: regia_agent::protocol::Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("inbox open");
    app.update();
    let response = answer.try_recv().expect("dispatcher answers within the frame");
    match (response.result, response.error) {
        (_, Some(error)) => Err(error),
        (Some(value), None) => Ok(value),
        (None, None) => panic!("dispatcher returned no result or error"),
    }
}

#[test]
fn an_extension_workspace_appears_and_applies() {
    let root = regia_test_utils::unique_temp_dir("extension-workspace");
    std::fs::create_dir_all(root.join("extensions/workspace_test")).expect("project");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"Workspace test\"\n[extensions]\nenabled = [\"workspace_test\"]\n[extensions.grants]\nworkspace_test = [\"editor_panel\"]\n").expect("project config");
    std::fs::write(root.join("extensions/workspace_test/regia-plugin.toml"), "format_version = 1\nid = \"workspace_test\"\nname = \"Workspace test\"\nversion = \"0.1.0\"\ncapabilities = [\"editor_panel\"]\n[[code]]\ncrate = \"workspace_test\"\naudience = [\"editor\"]\n").expect("extension config");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.add_plugins(EditorExtensionPlugin {
        project: Some(root.clone()),
        target_dir: Some(root.join("no-target")),
        engine_dir: None,
        linked: vec![Arc::new(WorkspaceExtension)],
    });
    regia_editor::agent_ext::install(app.world_mut());
    app.finish_plugins();
    app.update();
    app.world_mut()
        .insert_resource(regia_agent::AgentPermissions::default());
    let list = dispatch(&mut app, "editor.workspace_list", json!({})).expect("workspaces");
    assert!(
        list["workspaces"]
            .as_array()
            .expect("array")
            .iter()
            .any(|w| w["id"] == "extension:workspace_test.terrain"
                && w["extension"] == "workspace_test"
                && w["mode"] == "terrain"),
        "{list}"
    );
    let mut views = DockViews::default();
    let mut dock = egui_dock::DockState::new(vec![Panel::Viewport]);
    dispatch(
        &mut app,
        "editor.workspace_set",
        json!({"id": "extension:workspace_test.terrain"}),
    )
    .expect("apply through dispatcher");
    views.apply_requests(&mut dock, app.world_mut());
    views.publish(app.world_mut());
    assert!(dock.find_tab(&Panel::Terrain).is_some());
    let state = dispatch(&mut app, "editor.workspace_list", json!({})).expect("state");
    assert_eq!(state["mode"], "terrain");
    // Reload rollback restores editor descriptors, and registry removal hides stale descriptors.
    let snapshot = app
        .world_mut()
        .resource_mut::<regia_editor::extension::EditorExtensions>()
        .take_owner("workspace_test");
    let hidden = dispatch(&mut app, "editor.workspace_list", json!({})).expect("hidden");
    assert!(
        !hidden["workspaces"]
            .as_array()
            .expect("array")
            .iter()
            .any(|w| w["extension"] == "workspace_test")
    );
    app.world_mut()
        .resource_mut::<regia_editor::extension::EditorExtensions>()
        .restore("workspace_test", snapshot);
    let restored = dispatch(&mut app, "editor.workspace_list", json!({})).expect("restored");
    assert!(
        restored["workspaces"]
            .as_array()
            .expect("array")
            .iter()
            .any(|w| w["extension"] == "workspace_test")
    );
    let registrations = app
        .world_mut()
        .resource_mut::<regia_framework::extension::ExtensionRegistry>()
        .take_registrations("workspace_test");
    let denied = dispatch(
        &mut app,
        "editor.workspace_set",
        json!({"id": "extension:workspace_test.terrain"}),
    )
    .expect_err("disabled registration is unavailable");
    assert_eq!(
        denied
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str),
        Some("unknown_workspace")
    );
    assert!(
        views
            .apply(
                &mut dock,
                app.world_mut(),
                ViewRequest::SetWorkspace("extension:workspace_test.terrain".into())
            )
            .is_err(),
        "a parked but disabled workspace cannot apply"
    );
    app.world_mut()
        .resource_mut::<regia_framework::extension::ExtensionRegistry>()
        .restore_registrations(registrations);
    assert!(
        views
            .apply(
                &mut dock,
                app.world_mut(),
                ViewRequest::SetWorkspace("extension:workspace_test.terrain".into())
            )
            .is_ok()
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

fn ui_frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    mut draw: impl FnMut(&mut egui::Ui),
) -> egui::accesskit::TreeUpdate {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 700.0),
        )),
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| draw(ui));
    });
    out.textures_delta.clear();
    out.platform_output
        .accesskit_update
        .expect("accessibility enabled")
}

fn keyboard(key: egui::Key) -> Vec<egui::Event> {
    [true, false]
        .into_iter()
        .map(|pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        })
        .collect()
}

fn focused_name(update: &egui::accesskit::TreeUpdate) -> &str {
    update
        .nodes
        .iter()
        .find(|(id, _)| *id == update.focus)
        .and_then(|(_, node)| node.label())
        .unwrap_or_default()
}

fn focus_named(ctx: &egui::Context, name: &str, mut draw: impl FnMut(&mut egui::Ui)) {
    let mut seen = std::collections::BTreeSet::new();
    let mut last_names = Vec::new();
    for _ in 0..48 {
        ui_frame(ctx, keyboard(egui::Key::Tab), &mut draw);
        let update = ui_frame(ctx, Vec::new(), &mut draw);
        seen.insert(focused_name(&update).to_owned());
        last_names = regia_editor::a11y_audit::widget_names(&update);
        if focused_name(&update) == name {
            return;
        }
    }
    panic!("Tab did not reach {name}; focused={seen:?}; widgets={last_names:?}");
}

#[test]
fn the_mode_toolbar_and_selector_are_keyboard_operable() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    ctx.set_zoom_factor(1.5);
    let mut world = World::new();
    let mut draw = |ui: &mut egui::Ui| regia_editor::workspaces::mode_toolbar(ui, &mut world);
    ui_frame(&ctx, Vec::new(), &mut draw);
    focus_named(&ctx, "Editor mode", &mut draw);
    ui_frame(&ctx, keyboard(egui::Key::Space), &mut draw);
    focus_named(&ctx, "Terrain", &mut draw);
    ui_frame(&ctx, keyboard(egui::Key::Enter), &mut draw);
    assert_eq!(
        world.resource::<regia_editor::workspaces::ViewRequests>().pending,
        vec![ViewRequest::SetWorkspaceMode {
            id: "level".into(),
            mode: EditorMode::Terrain
        }]
    );
    let mut views = DockViews::default();
    let mut dock = regia_editor::editor_layout::default_dock();
    views.apply_requests(&mut dock, &mut world);
    views.publish(&mut world);
    let mut draw = |ui: &mut egui::Ui| regia_editor::workspaces::mode_toolbar(ui, &mut world);
    ui_frame(&ctx, Vec::new(), &mut draw);
    focus_named(&ctx, Panel::Layers.title(), &mut draw);
    let update = ui_frame(&ctx, keyboard(egui::Key::Enter), &mut draw);
    assert!(regia_editor::a11y_audit::unnamed_widgets(&update).is_empty());
    assert_eq!(
        world
            .resource::<regia_editor::editor_layout::LayoutRequests>()
            .pending,
        vec![regia_editor::editor_layout::LayoutRequest::Open(Panel::Layers)]
    );
}

#[test]
fn add_panel_and_area_menus_are_keyboard_operable() {
    struct Viewer;
    impl egui_dock::TabViewer for Viewer {
        type Tab = Panel;
        fn id(&mut self, tab: &mut Panel) -> egui::Id {
            egui::Id::new(tab.id())
        }
        fn title(&mut self, tab: &mut Panel) -> egui::WidgetText {
            tab.title().into()
        }
        fn ui(&mut self, _ui: &mut egui::Ui, _tab: &mut Panel) {}
    }
    for (menu, request) in [
        (
            "Join with neighbor",
            ViewRequest::Join {
                from: Panel::Viewport,
                into: Panel::Inspector,
            },
        ),
        (
            "Swap with",
            ViewRequest::Swap {
                from: Panel::Viewport,
                other: Panel::Inspector,
            },
        ),
    ] {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut world = World::new();
        let mut dock = egui_dock::DockState::new(vec![Panel::Viewport]);
        dock.main_surface_mut()
            .split_right(egui_dock::NodeIndex::root(), 0.5, vec![Panel::Inspector]);
        regia_editor::workspaces::DockGroups::refresh(&mut world, &dock);
        let mut draw = |ui: &mut egui::Ui| {
            let mut style = egui_dock::Style::from_egui(ui.style());
            regia_editor::workspaces::reserve_tab_buttons(&mut style);
            let height = style.tab_bar.height;
            egui_dock::DockArea::new(&mut dock)
                .style(style)
                .show_inside(ui, &mut Viewer);
            regia_editor::workspaces::tab_overflow_buttons(ui, &mut world, &dock, height);
        };
        ui_frame(&ctx, Vec::new(), &mut draw);
        focus_named(&ctx, "Add panel", &mut draw);
        ui_frame(&ctx, keyboard(egui::Key::Space), &mut draw);
        // egui includes the submenu arrow in its accessible button label.
        focus_named(&ctx, &format!("{menu} ⏵"), &mut draw);
        ui_frame(&ctx, keyboard(egui::Key::Space), &mut draw);
        focus_named(&ctx, Panel::Inspector.title(), &mut draw);
        let update = ui_frame(&ctx, keyboard(egui::Key::Enter), &mut draw);
        assert!(regia_editor::a11y_audit::unnamed_widgets(&update).is_empty());
        assert_eq!(
            world.resource::<regia_editor::workspaces::ViewRequests>().pending,
            vec![request]
        );
    }
}
