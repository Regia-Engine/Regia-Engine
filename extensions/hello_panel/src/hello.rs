// SPDX-License-Identifier: MPL-2.0
//! The engine's example editor extension (ADR-9850 (5)): what an editor extension registers and how.
//!
//! - a panel `hello_panel.greeting` (docked at the bottom) that greets with the manifest's display
//!   name, so a project copy of the extension (which overrides this one) shows its own name;
//! - an action `hello_panel.say_hello` in the Extensions menu (submenu "Hello") with a toolbar
//!   button, counting how often it ran in [`HelloCount`];
//! - an asset tool `hello_panel.line_count` for `.ron` files, answering `{ lines }`.
//!
//! Everything goes through `ToolExtensionContext`, which needs the `editor_panel` capability
//! (requested in `regia-plugin.toml`, granted by the project).

use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_editor::extension::{
    ActionDesc, AssetToolDesc, AssetToolInput, DockRegion, PanelDesc, ToolExtensionContext,
};
use regia_framework::extension::{ExtensionContext, ExtensionError, ExtensionRegistry, RegiaExtension};
use serde_json::json;

/// The extension's id (`regia-plugin.toml`).
pub const ID: &str = "hello_panel";

/// How often `hello_panel.say_hello` ran in this editor session.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloCount(pub u32);

/// The extension.
#[derive(Default)]
pub struct HelloPanel;

/// The resolved manifest's display name (the project copy's when one overrides the engine's).
fn display_name(world: &World) -> String {
    world
        .get_resource::<ExtensionRegistry>()
        .and_then(|registry| registry.discovered(ID))
        .map_or_else(|| ID.to_owned(), |found| found.manifest.name.clone())
}

fn say_hello(world: &mut World) -> Result<String, String> {
    let name = display_name(world);
    let mut count = world.get_resource_or_init::<HelloCount>();
    count.0 += 1;
    Ok(format!("Hello #{} from {name}", count.0))
}

fn line_count(_world: &mut World, input: &AssetToolInput) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(&input.file).map_err(|error| format!("{}: {error}", input.path))?;
    Ok(json!({ "lines": text.lines().count() }))
}

impl RegiaExtension for HelloPanel {
    fn id(&self) -> &str {
        ID
    }

    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError> {
        let mut editor = ToolExtensionContext::new(cx)?;
        editor.register_editor_panel(PanelDesc {
            id: format!("{ID}.greeting"),
            title_key: "Hello".into(),
            dock_region: DockRegion::Bottom,
            draw: Arc::new(|ui: &mut egui::Ui, world: &mut World| {
                ui.heading(format!("Hello from {}", display_name(world)));
                let count = world.get_resource::<HelloCount>().map_or(0, |count| count.0);
                ui.label(format!("Said hello {count} times"));
            }),
        })?;
        editor.register_editor_action(ActionDesc {
            menu: "Hello".into(),
            toolbar: true,
            category: "Hello".into(),
            ..ActionDesc::new(format!("{ID}.say_hello"), "Say hello", Arc::new(say_hello))
        })?;
        editor.register_asset_tool(AssetToolDesc {
            id: format!("{ID}.line_count"),
            title: "Count lines".into(),
            suffixes: vec![".ron".into()],
            run: Arc::new(line_count),
        })
    }
}
