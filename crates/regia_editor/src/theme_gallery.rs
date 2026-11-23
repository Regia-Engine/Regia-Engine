// SPDX-License-Identifier: MPL-2.0
//! Runtime-rendered theme gallery and document preview pages (UI parity P-258).
//!
//! Samples use the runtime widget constructors, layout, theme resolver and offscreen renderer.
//! Forced state examples and pointer feedback share the same renderer. The focusable sample list
//! provides the same style selection as clicking the image; no content format is introduced.

use std::collections::BTreeMap;
use std::path::Path;

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_ui::authoring::{UiLibrary, UiNodeDef, WidgetDef};
use regia_ui::{Classes, ComputedNode, Dimension, NodeName, NodeStyle, Style, Text, UiFile, UiNode};
use serde_json::json;

use crate::theme_panel::{PanelError, STYLE_STATES, StyleSelection, ThemePanel};
use crate::ui_designer_gpu::ThemedFrame;
use crate::ui_designer_preview::{PreviewSettings, preview_app};

/// Default runtime widget kinds offered as gallery pages.
pub const WIDGET_PAGES: [&str; 35] = [
    "Button",
    "Toggle",
    "Radio",
    "SpinBox",
    "Slider",
    "TextInput",
    "Dropdown",
    "Tabs",
    "List",
    "Tree",
    "DataGrid",
    "ColorPicker",
    "Menu",
    "Tooltip",
    "Dialog",
    "Progress",
    "Scroll",
    "Split",
    "Foldable",
    "Breadcrumbs",
    "Toast",
    "Separator",
    "Panel",
    "Label",
    "Image",
    "Stick",
    "VirtualButton",
    "ContextMenu",
    "MenuBar",
    "LobbyList",
    "Scoreboard",
    "Radar",
    "Minimap",
    "MarkerLayer",
    "Focusable",
];

/// The theme gallery's view state and cached render, separate from the saved theme document.
#[derive(Resource, Default)]
pub struct ThemeGallery {
    page: String,
    documents: BTreeMap<String, UiFile>,
    path_buffer: String,
    keys: crate::canvas_keys::ListKeys,
    cache: Option<GalleryCache>,
    hover: Option<usize>,
    pressed: bool,
    last_frame: Option<u64>,
    error: Option<String>,
}

struct GalleryCache {
    revision: u64,
    density: regia_ui::theme_file::Density,
    page: String,
    style: String,
    hover: Option<usize>,
    pressed: bool,
    texture: egui::TextureHandle,
    picks: Vec<GalleryPick>,
}

/// One preview's pick target, in runtime output pixels.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GalleryPick {
    /// Accessible name (widget kind/state or document node).
    pub label: String,
    /// The runtime type/variation and state this preview styles.
    pub selection: StyleSelection,
    /// Rectangle `[x, y, width, height]` in the rendered page.
    pub rect: [f32; 4],
}

fn error(code: &'static str, message: impl Into<String>) -> PanelError {
    PanelError {
        code,
        message: message.into(),
    }
}

impl ThemeGallery {
    /// The built-in pages followed by user documents, in stable order.
    pub fn pages(&self) -> Vec<String> {
        WIDGET_PAGES
            .iter()
            .map(|p| (*p).into())
            .chain(self.documents.keys().cloned())
            .collect()
    }

    /// The active page. An untouched gallery starts on Button.
    pub fn page(&self) -> &str {
        if self.page.is_empty() {
            "Button"
        } else {
            &self.page
        }
    }

    /// Selects a built-in or previously added document page.
    pub fn select_page(&mut self, page: &str) -> Result<(), PanelError> {
        if !self.pages().iter().any(|p| p == page) {
            return Err(error("unknown_page", format!("unknown preview page `{page}`")));
        }
        if self.page() != page {
            self.cache = None;
            self.hover = None;
            self.keys = Default::default();
        }
        self.page = page.into();
        Ok(())
    }

    /// Adds/reloads a user `.ui.ron` preview page; parsing failures keep the previous page.
    pub fn add_document(&mut self, path: &Path) -> Result<(), PanelError> {
        let text = std::fs::read_to_string(path).map_err(|e| error("io_error", e.to_string()))?;
        let name = path.display().to_string();
        let file = regia_ui::parse_ui_ron(&text, &name, &regia_ui::UiMigrators::default())
            .map_err(|e| error("parse_error", e.to_string()))?;
        self.documents.insert(name.clone(), file);
        self.cache = None;
        self.hover = None;
        self.pressed = false;
        self.keys = Default::default();
        self.select_page(&name)
    }

    /// Renders the current page through the runtime renderer, also returning click-to-style targets.
    /// This is the same render path as the panel, usable without an egui window.
    pub fn snapshot(
        &self,
        panel: &ThemePanel,
        device: &regia_render::RenderDevice,
    ) -> Result<(ThemedFrame, Vec<GalleryPick>), PanelError> {
        render(panel, self, device)
    }

    /// Describes the preview pages and selection without requiring a GPU.
    pub fn state(&self) -> serde_json::Value {
        json!({"page":self.page(), "pages":self.pages(), "error":self.error,
            "picks":self.cache.as_ref().map(|c| &c.picks)})
    }
}

fn child(world: &mut World, parent: Entity, bundle: impl Bundle) -> Entity {
    let entity = world.spawn(bundle).id();
    world.entity_mut(parent).add_child(entity);
    entity
}

fn sample(world: &mut World, parent: Entity, kind: &str, checked: bool) -> Result<Entity, PanelError> {
    use regia_ui::default_widgets as defaults;
    use regia_ui::parity_widgets as parity;
    let special = match kind {
        "Radio" => Some(defaults::spawn_radio_button(
            &mut world.commands(),
            parent,
            "Radio",
            "preview",
            checked,
        )),
        "Foldable" => Some(defaults::spawn_foldable(&mut world.commands(), parent, "Details", None, true).0),
        "Split" => Some(
            defaults::spawn_split(
                &mut world.commands(),
                parent,
                defaults::SplitAxis::Horizontal,
                0.5,
            )
            .0,
        ),
        "Breadcrumbs" => Some(defaults::spawn_breadcrumbs(
            &mut world.commands(),
            parent,
            &["Home", "Items"],
            1,
        )),
        "Tree" => Some(
            parity::spawn_tree_view(
                &mut world.commands(),
                parent,
                &[regia_ui::tree::TreeSpec {
                    label: "Items".into(),
                    children: vec![regia_ui::tree::TreeSpec {
                        label: "Child".into(),
                        children: vec![],
                    }],
                }],
                120.0,
            )
            .map_err(|e| error("preview_failed", e.to_string()))?,
        ),
        "DataGrid" => Some(parity::spawn_data_grid(
            &mut world.commands(),
            parent,
            regia_ui::tree::DataGrid {
                columns: vec![regia_ui::tree::GridColumn {
                    title: "Name".into(),
                    width: 100.0,
                }],
                rows: vec![vec!["Item".into()]],
                ..Default::default()
            },
            120.0,
        )),
        "ColorPicker" => Some(parity::spawn_color_picker(
            &mut world.commands(),
            parent,
            regia_ui::color_picker::ColorPicker::default(),
            90.0,
        )),
        "Separator" => Some(parity::spawn_separator(&mut world.commands(), parent, false)),
        _ => None,
    };
    world.flush();
    if let Some(entity) = special {
        if let Some(mut style) = world.get_mut::<NodeStyle>(entity) {
            style.0.size = [
                Dimension::Points(240.0),
                Dimension::Points(match kind {
                    "Radio" | "Breadcrumbs" => 48.0,
                    "ColorPicker" => 180.0,
                    "Separator" => 4.0,
                    _ => 140.0,
                }),
            ];
        }
        return Ok(entity);
    }
    let preset = match kind {
        "Button" => "button",
        "Toggle" => "toggle",
        "Slider" => "slider",
        "TextInput" => "text_input",
        "Dropdown" => "dropdown",
        "Progress" => "progress",
        "List" => "list",
        "Scroll" => "scroll",
        "Label" | "Toast" => "text",
        "Image" => "image",
        "LobbyList" => "lobby_list",
        "Scoreboard" => "scoreboard",
        "Radar" => "radar",
        "Minimap" => "minimap",
        "MarkerLayer" => "marker_layer",
        _ => "panel",
    };
    let mut def =
        crate::ui_designer::preset(preset).ok_or_else(|| error("preview_failed", "missing widget preset"))?;
    def.style.size = [
        Dimension::Points(240.0),
        Dimension::Points(
            if matches!(
                kind,
                "List" | "LobbyList" | "Scoreboard" | "Radar" | "Minimap" | "MarkerLayer"
            ) {
                160.0
            } else {
                48.0
            },
        ),
    ];
    def.style.position = regia_ui::PositionType::Relative;
    def.interactable = true;
    match kind {
        "Toggle" => def.widget = Some(WidgetDef::Toggle(checked)),
        "List" => def.widget = Some(WidgetDef::List(3, 32.0)),
        "Tabs" => {
            def.widget = Some(WidgetDef::TabBar(
                vec!["First".into(), "Second".into()],
                usize::from(checked),
            ))
        }
        "Menu" => {
            def.widget = Some(WidgetDef::PopupMenu(vec![regia_ui::authoring::MenuEntryDef {
                label: "Action".into(),
                ..Default::default()
            }]))
        }
        "Tooltip" => {
            def.widget = Some(WidgetDef::Tooltip("Helpful text".into(), 0.0));
            def.text = crate::ui_designer::preset("text").and_then(|d| d.text);
        }
        "Dialog" => {
            def.widget = Some(WidgetDef::Modal(true, true));
            def.classes.push("panel".into());
        }
        "Stick" => def.widget = Some(WidgetDef::Stick("preview".into(), 20.0)),
        "VirtualButton" => def.widget = Some(WidgetDef::VButton("preview".into())),
        "Focusable" => def.widget = Some(WidgetDef::Focusable),
        "ContextMenu" => {
            def.widget = Some(WidgetDef::ContextMenu(vec![regia_ui::authoring::MenuEntryDef {
                label: "Action".into(),
                ..Default::default()
            }]))
        }
        "MenuBar" => {
            def.widget = Some(WidgetDef::MenuBar(vec![regia_ui::authoring::MenuEntryDef {
                label: "Actions".into(),
                submenu: vec![regia_ui::authoring::MenuEntryDef {
                    label: "Item".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }]))
        }
        "SpinBox" => def.classes.push("button".into()),
        "Panel" | "Toast" => def.classes.push("surface".into()),
        _ => {}
    }
    // Samples explicitly opt into the runtime styles they demonstrate. Several authoring
    // presets intentionally leave their classes empty for a game's own choice.
    match kind {
        "Toggle" => def.classes.push("Toggle".into()),
        "List" => def.classes.push("List.row".into()),
        "TextInput" | "Dropdown" | "VirtualButton" | "Tabs" | "Focusable" => {
            def.classes.push("Button".into());
        }
        "Scroll" | "Progress" | "Image" | "LobbyList" | "Scoreboard" | "Radar" | "Minimap"
        | "MarkerLayer" => def.classes.push("Panel".into()),
        _ => {}
    }
    let entity = regia_ui::authoring::spawn_node(world, parent, &def);
    if kind == "SpinBox" {
        world.entity_mut(entity).insert(defaults::SpinBox::default());
    }
    if kind == "Toast" {
        world
            .entity_mut(entity)
            .insert(defaults::Toast { remaining: 60.0 });
    }
    Ok(entity)
}

fn selected_class(world: &World, entity: Entity, theme: &regia_ui::theme_file::ResolvedTheme) -> String {
    if let Some(classes) = world.get::<Classes>(entity) {
        for class in classes.0.iter().rev() {
            if theme.types.contains_key(class) || theme.variations.contains_key(class) {
                return class.clone();
            }
            let target = regia_ui::theme_migrate::class_target(class).key;
            if theme.types.contains_key(&target) || theme.variations.contains_key(&target) {
                return target;
            }
        }
    }
    "Panel".into()
}

fn mark(world: &mut World, entity: Entity, state: &str) {
    let mut node = world.entity_mut(entity);
    match state {
        "hovered" => {
            node.insert(regia_ui::Hovered);
        }
        "pressed" => {
            node.insert((regia_ui::Hovered, regia_ui::Pressed));
        }
        "focused" => {
            node.insert(regia_ui::Focused);
        }
        "disabled" => {
            node.insert(regia_ui::Disabled);
        }
        "checked" => {
            node.insert(regia_ui::Selected);
        }
        "error" => {
            node.insert(regia_ui::Invalid);
        }
        _ => {}
    }
}

fn render(
    panel: &ThemePanel,
    gallery: &ThemeGallery,
    device: &regia_render::RenderDevice,
) -> Result<(ThemedFrame, Vec<GalleryPick>), PanelError> {
    let _span = tracing::info_span!("theme_gallery_render").entered();
    let settings = PreviewSettings {
        size: [960.0, 720.0],
        safe_area: [0.0; 4],
        ..Default::default()
    };
    let mut app = preview_app(&settings, None).map_err(|e| error(e.code, e.message))?;
    let theme = panel.resolved()?;
    let world = app.world_mut();
    world.insert_resource(panel.preview_themes());
    world.insert_resource(regia_ui::ActiveTheme {
        name: theme.name.clone(),
        density: Some(panel.density()),
        reduced_motion: true,
        ..Default::default()
    });
    let mut samples = Vec::new();
    if matches!(gallery.page(), "Radar" | "Minimap" | "MarkerLayer") {
        use regia_core::glam::{Affine3A, Vec3};
        use regia_scene::GlobalTransform;
        let accent = theme
            .color("color.accent")
            .ok_or_else(|| error("preview_failed", "theme has no color.accent"))?;
        world.spawn((regia_render::Camera::default(), GlobalTransform::default()));
        world.spawn((
            regia_ui::game_widgets::RadarBlip::dot(accent),
            regia_ui::game_widgets::WorldMarker::labeled("Target", accent),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 0.0, -10.0))),
        ));
    }
    if let Some(file) = gallery.documents.get(gallery.page()) {
        let mut file = file.clone();
        fn name_nodes(node: &mut UiNodeDef, path: String) {
            node.name = Some(path.clone());
            for (i, child) in node.children.iter_mut().enumerate() {
                name_nodes(child, format!("{path}/{i}"));
            }
        }
        name_nodes(&mut file.root, "document".into());
        world
            .resource_mut::<UiLibrary>()
            .templates
            .insert("theme-preview".into(), file);
        regia_ui::authoring::spawn_ui(world, "theme-preview")
            .map_err(|e| error("preview_failed", e.to_string()))?;
        let mut query = world.query::<(Entity, &NodeName)>();
        let mut nodes: Vec<_> = query.iter(world).map(|(e, n)| (e, n.0.clone())).collect();
        nodes.sort_by(|a, b| a.1.cmp(&b.1));
        for (entity, name) in nodes {
            samples.push((entity, name, "normal".to_owned()));
        }
    } else {
        let root = world
            .spawn((
                regia_ui::UiRoot {
                    scale_mode: regia_ui::ScaleMode::ConstantPixelSize,
                    ..Default::default()
                },
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    direction: regia_ui::style::FlexDirection::Row,
                    flex_wrap: true,
                    gap: [Dimension::Points(12.0); 2],
                    padding: regia_ui::Edges::px(12.0),
                    ..Default::default()
                }),
            ))
            .id();
        for state in STYLE_STATES {
            let cell = child(
                world,
                root,
                (
                    UiNode,
                    NodeStyle::new(Style {
                        size: [Dimension::Points(300.0), Dimension::Points(212.0)],
                        direction: regia_ui::style::FlexDirection::Column,
                        gap: [Dimension::Points(12.0); 2],
                        ..Default::default()
                    }),
                ),
            );
            child(
                world,
                cell,
                (UiNode, Text::new(format!("{} / {state}", gallery.page()))),
            );
            let entity = sample(world, cell, gallery.page(), state == "checked")?;
            samples.push((entity, format!("{} / {state}", gallery.page()), state.into()));
        }
    }
    for _ in 0..3 {
        app.update();
    }
    // Popup samples display the runtime popup panel itself, not just its opener. Keep each
    // popup on its own cell's anchor; ordinary app input still lays out and extracts the content.
    if !gallery.documents.contains_key(gallery.page())
        && matches!(gallery.page(), "Menu" | "ContextMenu" | "Tooltip")
    {
        let world = app.world_mut();
        for (entity, _, _) in &mut samples {
            let host = *entity;
            let handle = if gallery.page() == "Tooltip" {
                let handle = regia_ui::popup::open_popup(
                    world,
                    regia_ui::popup::PopupRequest {
                        kind: regia_ui::popup::PopupKind::Tooltip,
                        anchor: Some(host),
                        opener: Some(host),
                        interactive: false,
                        classes: vec!["Tooltip".into()],
                        ..Default::default()
                    },
                );
                child(world, handle.panel, (UiNode, Text::new("Helpful text")));
                handle
            } else {
                regia_ui::menu::open_menu(
                    world,
                    host,
                    if gallery.page() == "Menu" {
                        regia_ui::menu::MenuSource::Popup
                    } else {
                        regia_ui::menu::MenuSource::Context
                    },
                    Vec::new(),
                    regia_ui::menu::MenuAnchor::Node(host, regia_ui::popup::PopupPlacement::Below),
                    None,
                    Some(host),
                )
                .ok_or_else(|| error("preview_failed", "sample menu has no entries"))?
            };
            *entity = handle.panel;
        }
        for _ in 0..3 {
            app.update();
        }
    }
    let world = app.world_mut();
    // State examples are imposed after the input system's update, so disabled and hovered
    // samples stay visible even without a physical pointer over the offscreen window.
    let chosen_chain = panel.style_chain(&panel.selection().type_name)?;
    for (index, (entity, _, state)) in samples.iter().enumerate() {
        let base = selected_class(world, *entity, &theme);
        if !gallery.documents.contains_key(gallery.page()) {
            let mut classes = world.get::<Classes>(*entity).cloned().unwrap_or_default();
            // An unclassified constructor uses Panel in the gallery. Make that class explicit
            // so the property box chosen by picking always controls the sample that was drawn.
            if !classes.0.contains(&base) {
                classes.0.push(base.clone());
            }
            if chosen_chain.contains(&base) {
                classes.0.push(panel.selection().type_name.clone());
            }
            world.entity_mut(*entity).insert(classes);
        }
        let mut descendants = vec![*entity];
        let mut at = 0;
        while at < descendants.len() {
            if let Some(children) = world.get::<Children>(descendants[at]) {
                descendants.extend(children.iter());
            }
            at += 1;
        }
        for member in descendants {
            if member != *entity && world.get::<regia_ui::Interactable>(member).is_none() {
                continue;
            }
            mark(world, member, state);
            if gallery.hover == Some(index) && state != "disabled" {
                mark(world, member, if gallery.pressed { "pressed" } else { "hovered" });
            }
        }
    }
    world.insert_resource(regia_ui::focus::UiInputMode::Navigation);
    for _ in 0..2 {
        world
            .run_system_once(regia_ui::theme::apply_themes)
            .map_err(|e| error("preview_failed", e.to_string()))?;
    }
    regia_ui::begin_render_out(
        world,
        &regia_render::RenderOutSpec::new(960, 720, regia_render::HudLayout::Auto),
    );
    let picks = samples
        .into_iter()
        .filter_map(|(entity, label, state)| {
            let rect = world.get::<ComputedNode>(entity)?.rect;
            Some(GalleryPick {
                label,
                selection: StyleSelection {
                    type_name: selected_class(world, entity, &theme),
                    state,
                },
                rect,
            })
        })
        .collect();
    let shared = world.resource::<regia_ui::SharedUiRenderer>().clone();
    let target = regia_ui::OffscreenTarget::new(device, 960, 720);
    let backdrop = theme
        .color("color.bg")
        .ok_or_else(|| error("preview_failed", "theme has no color.bg"))?;
    target.clear(device, backdrop.0.map(f64::from));
    target.draw_ui(device, &mut shared.lock());
    let rgba = target
        .read(device)
        .map_err(|e| error("preview_failed", e.to_string()))?;
    Ok((
        ThemedFrame {
            width: 960,
            height: 720,
            rgba,
            theme: theme.name,
            theme_errors: Vec::new(),
        },
        picks,
    ))
}

/// Draws the gallery, with keyboard-equivalent sample selection and a document-page loader.
pub fn draw_gallery(ui: &mut egui::Ui, world: &mut World, panel: &mut ThemePanel) {
    use crate::agent_parity::{self as actions, action_button};
    let mut gallery = world.remove_resource::<ThemeGallery>().unwrap_or_default();
    let frame = ui.ctx().cumulative_frame_nr();
    let apply = gallery.last_frame != Some(frame);
    gallery.last_frame = Some(frame);
    ui.collapsing("Live widget gallery", |ui| {
        let current = gallery.page().to_owned();
        egui::ComboBox::from_id_salt("theme_gallery_page")
            .selected_text(&current)
            .show_ui(ui, |ui| {
                for page in gallery.pages() {
                    if ui.selectable_label(page == current, &page).clicked() && apply {
                        let _ = gallery.select_page(&page);
                    }
                }
            });
        ui.horizontal(|ui| {
            let response = ui.text_edit_singleline(&mut gallery.path_buffer);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::TextEdit,
                    ui.is_enabled(),
                    "Preview document path",
                )
            });
            if action_button(ui, &actions::THEME_PREVIEW_DOCUMENT).clicked() && apply {
                let path = gallery.path_buffer.clone();
                if let Err(e) = gallery.add_document(Path::new(&path)) {
                    gallery.error = Some(e.to_string());
                }
            }
        });
        let dirty = gallery.cache.as_ref().is_none_or(|c| {
            c.revision != panel.revision()
                || c.density != panel.density()
                || c.page != gallery.page()
                || c.style != panel.selection().type_name
                || c.hover != gallery.hover
                || c.pressed != gallery.pressed
        });
        if dirty && apply {
            match crate::ui_designer_gpu::preview_device(world)
                .map_err(|e| error("preview_failed", e.to_string()))
                .and_then(|device| render(panel, &gallery, &device))
            {
                Ok((frame, picks)) => {
                    gallery.cache = Some(GalleryCache {
                        revision: panel.revision(),
                        density: panel.density(),
                        page: gallery.page().into(),
                        style: panel.selection().type_name.clone(),
                        hover: gallery.hover,
                        pressed: gallery.pressed,
                        texture: ui.ctx().load_texture(
                            "theme-gallery",
                            frame.to_color_image(),
                            egui::TextureOptions::LINEAR,
                        ),
                        picks,
                    });
                    gallery.error = None;
                }
                Err(e) => {
                    gallery.error = Some(e.to_string());
                }
            }
        }
        if let Some(error) = &gallery.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(cache) = &gallery.cache {
            let width = ui.available_width().clamp(1.0, 960.0);
            let response = ui.add(
                egui::Image::new((cache.texture.id(), egui::vec2(width, width * 0.75)))
                    .sense(egui::Sense::click()),
            );
            crate::canvas_keys::name_canvas(
                ui.ctx(),
                response.id,
                "Theme gallery; use the sample list to select a style with the keyboard",
            );
            let hit = response.hover_pos().and_then(|pos| {
                let x = (pos.x - response.rect.left()) * 960.0 / width;
                let y = (pos.y - response.rect.top()) * 960.0 / width;
                cache
                    .picks
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, p)| {
                        x >= p.rect[0]
                            && y >= p.rect[1]
                            && x < p.rect[0] + p.rect[2]
                            && y < p.rect[1] + p.rect[3]
                    })
                    .map(|(i, _)| i)
            });
            if apply {
                gallery.hover = hit;
                gallery.pressed = response.is_pointer_button_down_on();
            }
            if response.clicked()
                && apply
                && let Some(index) = hit
            {
                let mut selection = cache.picks[index].selection.clone();
                if selection.state == "normal" {
                    selection.state = "hovered".into();
                }
                if let Err(e) = panel.select_style(selection) {
                    gallery.error = Some(e.to_string());
                }
            }
            let picks = cache.picks.clone();
            if let Err(e) = sample_list(ui, &mut gallery.keys, &picks, panel, apply) {
                gallery.error = Some(e.to_string());
            }
            if dirty || gallery.hover != cache.hover || gallery.pressed != cache.pressed {
                ui.ctx().request_repaint();
            }
        }
    });
    world.insert_resource(gallery);
}

fn sample_list(
    ui: &mut egui::Ui,
    keys: &mut crate::canvas_keys::ListKeys,
    picks: &[GalleryPick],
    panel: &mut ThemePanel,
    apply: bool,
) -> Result<(), PanelError> {
    let selected = picks.iter().position(|p| &p.selection == panel.selection());
    for key in crate::canvas_keys::list_rows(
        ui,
        "theme_gallery_samples",
        keys,
        picks.len(),
        140.0,
        selected,
        &|i| picks[i].label.clone(),
    ) {
        if let crate::canvas_keys::RowKey::Activate(index) = key
            && apply
        {
            panel.select_style(picks[index].selection.clone())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> ThemePanel {
        let mut panel = ThemePanel::default();
        panel.new_child("gallery-test", "regia-dark").expect("child");
        panel
    }

    #[test]
    fn every_widget_page_has_all_states_and_runtime_pixels() {
        let device = crate::render::headless_device().expect("lavapipe device");
        let panel = panel();
        let mut gallery = ThemeGallery::default();
        for kind in WIDGET_PAGES {
            gallery.select_page(kind).expect("page");
            let (frame, picks) = render(&panel, &gallery, &device).expect(kind);
            assert_eq!(picks.len(), STYLE_STATES.len(), "{kind}");
            assert!(frame.drawn_pixels() > 0, "{kind} draws real runtime widgets");
            if matches!(kind, "Menu" | "ContextMenu" | "Tooltip" | "Toggle") {
                let style = if kind == "ContextMenu" { "Menu" } else { kind };
                assert!(
                    picks.iter().all(|p| p.selection.type_name == style),
                    "{kind} picks its displayed style"
                );
            }
            for (pick, state) in picks.iter().zip(STYLE_STATES) {
                assert_eq!(pick.selection.state, state);
                assert!(
                    pick.rect[2] > 0.0 && pick.rect[3] > 0.0,
                    "{kind} {state}: {:?}",
                    pick.rect
                );
            }
        }
    }

    #[test]
    fn pointer_and_draft_changes_change_the_control_render_without_a_golden() {
        let device = crate::render::headless_device().expect("lavapipe device");
        let mut panel = panel();
        let mut gallery = ThemeGallery::default();
        let (control, _) = render(&panel, &gallery, &device).expect("control");
        gallery.hover = Some(0);
        let (hover, _) = render(&panel, &gallery, &device).expect("hover");
        assert_ne!(control.rgba, hover.rgba, "hover updates the runtime look");
        gallery.pressed = true;
        let (pressed, _) = render(&panel, &gallery, &device).expect("press");
        assert_ne!(pressed.rgba, hover.rgba, "press updates the runtime look");
        gallery.hover = None;
        gallery.pressed = false;
        panel
            .edit(
                &json!({"types":{"Button":{"props":{"background":"#ca1234"}}}}),
                None,
            )
            .expect("edit");
        let (edited, _) = render(&panel, &gallery, &device).expect("edited");
        assert_ne!(
            edited.rgba, control.rgba,
            "unsaved theme edits reach the renderer"
        );
    }

    #[test]
    fn keyboard_sample_selection_matches_click_to_style() {
        let mut panel = panel();
        let picks: Vec<_> = STYLE_STATES
            .iter()
            .map(|state| GalleryPick {
                label: format!("Button / {state}"),
                selection: StyleSelection {
                    type_name: "Button".into(),
                    state: (*state).into(),
                },
                rect: [0.0; 4],
            })
            .collect();
        let ctx = egui::Context::default();
        let mut keys = crate::canvas_keys::ListKeys::default();
        keys.focus(2);
        for events in [
            vec![],
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        ] {
            let _ = crate::canvas_keys::test_input::frame(&ctx, [800.0, 600.0], events, |ui| {
                sample_list(ui, &mut keys, &picks, &mut panel, true).expect("list");
            });
        }
        assert_eq!(panel.selection().state, "pressed");
    }

    #[test]
    fn user_documents_add_pages_and_invalid_reload_preserves_them() {
        let dir = regia_test_utils::unique_temp_dir("theme-gallery-doc");
        let path = dir.join("preview.ui.ron");
        std::fs::write(&path,"(version: 11, root: (children: [(widget: button, classes: [\"danger\"], style: (size: (160, 48)))]))").expect("write");
        let mut gallery = ThemeGallery::default();
        gallery.add_document(&path).expect("add");
        assert_eq!(gallery.pages().len(), WIDGET_PAGES.len() + 1);
        assert_eq!(gallery.page(), path.display().to_string());
        std::fs::write(&path, "broken").expect("write invalid");
        assert!(gallery.add_document(&path).is_err());
        assert_eq!(gallery.pages().len(), WIDGET_PAGES.len() + 1);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
