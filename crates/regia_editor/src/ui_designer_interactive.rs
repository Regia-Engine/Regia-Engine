// SPDX-License-Identifier: MPL-2.0
//! Interactive designer preview (ADR-7811): an isolated runtime app with a reflected sample
//! model. All widget behavior, binding conversion, hit testing and commands use `regia_ui`.
//! Only the scratch document's binding paths change; authored bytes are never mutated.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_ui::authoring::{BindTargetDef, UiLibrary, UiNodeDef};
use regia_ui::{ComputedNode, NodeName, UiFile, ViewModelAppExt};
use serde_json::{Value, json};

use crate::ui_designer::{DesignerError, UiDesign, node_kind};
use crate::ui_designer_gpu::{ThemeChoice, ThemedFrame};
use crate::ui_designer_preview::{
    PreviewFrame, PreviewNode, PreviewSettings, interactive_document, preview_app,
};

#[derive(Reflect, Clone, Default)]
#[reflect(Default)]
struct SampleValue {
    text: String,
    number: f64,
    flag: bool,
}

#[derive(Resource, Reflect, Clone, Default)]
#[reflect(Resource, Default)]
struct DesignerSampleModel {
    values: Vec<SampleValue>,
}

#[derive(Clone)]
struct SampleSlot {
    index: usize,
    field: &'static str,
}

/// Preview state shared by the panel and agent methods. Stored as a non-send editor resource:
/// its private `App` never leaves the editor thread, just like the play world.
#[derive(Default)]
pub struct DesignerRuntime {
    /// Whether canvas input belongs to the preview runtime.
    pub interactive: bool,
    /// The scratch runtime, cached by document identity, revision and preview settings.
    pub session: Option<InteractivePreview>,
    key: Option<(u64, u64, PreviewSettings)>,
    choice: Option<ThemeChoice>,
    /// Changes when a scratch app is rebuilt, including a theme-only change.
    pub(crate) generation: u64,
}

impl DesignerRuntime {
    /// Builds or refreshes the scratch tree only when its authored inputs change.
    pub fn ensure(
        &mut self,
        doc: &UiDesign,
        choice: &ThemeChoice,
        localization: impl FnOnce() -> Option<regia_ui::Localization>,
    ) -> Result<(), DesignerError> {
        let key = (doc.serial(), doc.revision(), doc.preview.clone());
        let theme_changed = self.choice.as_ref().is_none_or(|old| {
            old.themes != choice.themes
                || old.active != choice.active
                || old.files != choice.files
                || old.theme != choice.theme
                || old.project != choice.project
        });
        if self.key.as_ref() != Some(&key) || theme_changed {
            let session = InteractivePreview::new(doc.file(), &doc.preview, choice, localization())?;
            self.session = Some(session);
            self.generation = self.generation.saturating_add(1);
            self.key = Some(key);
            self.choice = Some(choice.clone());
        }
        Ok(())
    }

    /// Stops playback and discards transient values; the next play starts from authored samples.
    pub fn stop(&mut self) {
        self.interactive = false;
        self.session = None;
        self.key = None;
        self.choice = None;
    }
}

/// A live UI app and the map from its entities to authored node paths and sample fields.
pub struct InteractivePreview {
    app: App,
    settings: PreviewSettings,
    nodes: Vec<(Entity, PreviewNode)>,
    slots: BTreeMap<String, SampleSlot>,
    samples: BTreeMap<String, String>,
    truncated: bool,
    theme: String,
    theme_errors: Vec<String>,
}

impl InteractivePreview {
    /// Spawns a document in a private runtime with sample-only bindings and the chosen theme.
    pub fn new(
        file: &UiFile,
        settings: &PreviewSettings,
        choice: &ThemeChoice,
        localization: Option<regia_ui::Localization>,
    ) -> Result<Self, DesignerError> {
        let _span = tracing::info_span!("ui_designer_interactive_spawn").entered();
        settings.validate()?;
        let mut expanded = interactive_document(file, settings);
        let mut sample_types = BTreeMap::new();
        collect_sample_types(&expanded.file.root, &mut sample_types);
        let mut model = DesignerSampleModel::default();
        let mut slots = BTreeMap::new();
        for (source, (target, initial)) in sample_types {
            let raw = sample(settings, &source).unwrap_or(&initial);
            let field = match target {
                BindTargetDef::Checked | BindTargetDef::Visible | BindTargetDef::Enabled => "flag",
                BindTargetDef::Progress
                | BindTargetDef::Value
                | BindTargetDef::Selected
                | BindTargetDef::Opacity => "number",
                _ => "text",
            };
            slots.insert(
                source,
                SampleSlot {
                    index: model.values.len(),
                    field,
                },
            );
            model.values.push(SampleValue {
                text: raw.to_owned(),
                number: raw.parse().unwrap_or(0.0),
                flag: raw == "true",
            });
        }
        let mut metadata = Vec::new();
        prepare_nodes(&mut expanded.file.root, &slots, &mut metadata);
        let mut app = preview_app(settings, localization)?;
        app.register_view_model::<DesignerSampleModel>();
        app.insert_resource(model);
        let world = app.world_mut();
        if let Some(themes) = &choice.themes {
            world.insert_resource(themes.clone());
        }
        let mut pending: Vec<_> = choice.files.iter().collect();
        let mut theme_errors = Vec::new();
        for round in 0..2 {
            pending.retain(|path| match regia_ui::theme_agent::load_theme_file(world, path) {
                Ok(_) => false,
                Err(issue) => {
                    if round == 1 {
                        theme_errors.push(format!("{}: {}", path.display(), issue.message));
                    }
                    true
                }
            });
        }
        let mut active = choice.active.clone().unwrap_or_default();
        if let Some(name) = choice.theme.as_ref().or(settings.theme.as_ref()) {
            if world.resource::<regia_ui::UiThemes>().set.get(name).is_none() {
                return Err(DesignerError::new(
                    "unknown_theme",
                    format!("unknown preview theme {name}"),
                ));
            }
            active.name.clone_from(name);
        }
        let theme = active.name.clone();
        world.insert_resource(active);
        // Source images use the same source-picture substitution as the still preview.
        let mut cache = crate::ui_designer_images::PictureCache::default();
        let uploads = match choice.project.as_deref() {
            Some(project) => crate::ui_designer_images::substitute_pictures(
                &mut expanded.file,
                Some((project, &mut cache)),
            ),
            None => {
                crate::ui_designer_images::substitute_pictures_without_project(&mut expanded.file);
                Vec::new()
            }
        };
        if let Some(shared) = world.get_resource::<regia_ui::SharedUiRenderer>() {
            regia_ui::ui_image_assets::queue_builtin_pictures(shared);
            for (id, width, height, rgba) in uploads {
                shared.lock().queue_image_rgba(id, width, height, rgba);
            }
        }
        const TEMPLATE: &str = "__designer_interactive";
        world
            .resource_mut::<UiLibrary>()
            .templates
            .insert(TEMPLATE.to_owned(), expanded.file);
        let root = regia_ui::authoring::spawn_ui(world, TEMPLATE)
            .map_err(|e| DesignerError::new("parse", e.to_string()))?;
        let tagged: BTreeMap<usize, Entity> = world
            .query::<(Entity, &NodeName)>()
            .iter(world)
            .filter_map(|(e, n)| Some((n.0.strip_prefix(TAG)?.parse().ok()?, e)))
            .collect();
        let mut nodes = Vec::new();
        for (index, ((name, kind), (path, item))) in metadata.into_iter().zip(expanded.origins).enumerate() {
            let entity = if index == 0 {
                root
            } else {
                *tagged.get(&index).ok_or_else(|| {
                    DesignerError::new("parse", format!("preview node {path} did not spawn"))
                })?
            };
            if let Some(name) = &name {
                world.entity_mut(entity).insert(NodeName(name.clone()));
            } else {
                world.entity_mut(entity).remove::<NodeName>();
            }
            nodes.push((
                entity,
                PreviewNode {
                    path,
                    item,
                    name,
                    kind,
                    rect: [0.0; 4],
                    clip: [0.0; 4],
                    visible: true,
                    rtl: false,
                    box_model_bands: None,
                    text: None,
                    outside_safe_area: false,
                },
            ));
            // Authored list nodes carry their rows but no scroll container. Supply the runtime
            // container in this scratch world so their sample rows can be scrolled interactively.
            if world.get::<regia_ui::ListView>(entity).is_some()
                && world.get::<regia_ui::ScrollView>(entity).is_none()
            {
                world.entity_mut(entity).insert((
                    regia_ui::ScrollView::vertical(),
                    regia_ui::ClipChildren,
                    regia_ui::Interactable::default(),
                ));
            }
        }
        // Expanded sample rows use the runtime list's external-row contract. The existing sample
        // expansion is bounded; the list owns scrolling, focus and selection, not another pool.
        for (host, ranges) in expanded.lists {
            let list = nodes[host].0;
            world.entity_mut(list).insert((
                regia_ui::list::RepeatRows,
                regia_ui::scroll::ItemCount(ranges.len()),
            ));
            for (index, range) in ranges.into_iter().enumerate() {
                for (entity, _) in &nodes[range] {
                    if world.get::<ChildOf>(*entity).is_some_and(|p| p.parent() == list) {
                        world.entity_mut(*entity).insert((
                            regia_ui::ListRow { list, index },
                            regia_ui::Interactable::default(),
                            regia_ui::Focusable::default(),
                        ));
                    }
                }
            }
        }
        regia_ui::game_widgets::fill_preview_samples(app.world_mut());
        let mut preview = Self {
            app,
            settings: settings.clone(),
            nodes,
            slots,
            samples: settings.samples.clone(),
            truncated: expanded.truncated,
            theme,
            theme_errors,
        };
        for _ in 0..3 {
            preview.step();
        }
        Ok(preview)
    }

    /// Advances exactly one runtime frame; no document or editor world is reachable here.
    pub fn step(&mut self) {
        let _span = tracing::trace_span!("ui_designer_interactive_step").entered();
        self.app.update();
    }

    /// Resolves an authored path or name to its first sample instance.
    pub fn entity(&self, node: &str) -> Result<Entity, DesignerError> {
        self.nodes
            .iter()
            .find(|(_, n)| {
                n.path == node
                    || node
                        .strip_prefix('#')
                        .is_some_and(|name| n.name.as_deref() == Some(name))
            })
            .map(|(e, _)| *e)
            .ok_or_else(|| DesignerError::new("bad_node", format!("no preview node {node}")))
    }

    /// The current runtime inspection for one authored node.
    pub fn inspect(&mut self, node: &str) -> Result<Value, DesignerError> {
        let entity = self.entity(node)?;
        let mut out = regia_ui::inspect::inspect(self.app.world_mut(), entity);
        out["path"] = self
            .nodes
            .iter()
            .find(|(e, _)| *e == entity)
            .map_or(Value::Null, |(_, n)| json!(n.path));
        Ok(out)
    }

    /// Runtime agent input on the scratch tree. `type` replaces the selection with submitted text;
    /// `click`, `focus`, `set_value`, `list` and `scroll_to` share their runtime method behavior.
    pub fn input(&mut self, input: &Value) -> Result<(), DesignerError> {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let method = match action {
            "click" => "ui.click",
            "focus" => "ui.focus",
            "type" => "ui.type",
            "set_value" => "ui.set_value",
            "list" => "ui.list",
            "scroll_to" => "ui.scroll_to",
            _ => {
                return Err(DesignerError::new(
                    "bad_value",
                    "input action is click, focus, type, set_value, list or scroll_to",
                ));
            }
        };
        let node = input
            .get("node")
            .and_then(Value::as_str)
            .ok_or_else(|| DesignerError::new("bad_node", "input needs an authored node path or #name"))?;
        let entity = self.entity(node)?;
        let mut params = input.clone();
        params["node"] = json!(entity.to_bits());
        if action == "type" {
            if input.get("text").and_then(Value::as_str).is_none() {
                return Err(DesignerError::new("bad_value", "type needs a text string"));
            }
            let text = self
                .app
                .world()
                .get::<regia_ui::TextInput>(entity)
                .ok_or_else(|| DesignerError::new("bad_value", "type needs a text input node"))?;
            if text.read_only {
                return Err(DesignerError::new("bad_value", "the text input is read-only"));
            }
            regia_ui::agent_bridge::call(
                self.app.world_mut(),
                "ui.focus",
                &json!({"node":entity.to_bits()}),
            )
            .map_err(|e| DesignerError::new("bad_value", e.message))?;
            self.step();
            // Select-all is the widget's own operation; typing and submit still go through IME
            // and keyboard messages, including validation and two-way conversion.
            if let Some(mut text) = self.app.world_mut().get_mut::<regia_ui::TextInput>(entity) {
                text.select_all();
            }
            // Keys are processed before IME in a runtime frame; clear the selected text first.
            regia_ui::agent_bridge::call(
                self.app.world_mut(),
                "ui.type",
                &json!({"node":entity.to_bits(), "keys":["Backspace"]}),
            )
            .map_err(|e| DesignerError::new("bad_value", e.message))?;
            self.step();
            params["keys"] = json!([]);
        }
        match action {
            "list" => {
                let index = input
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| DesignerError::new("bad_value", "list input needs an index"))?;
                let old = self
                    .app
                    .world()
                    .get::<regia_ui::list::ListSelection>(entity)
                    .cloned();
                if let Some(mode) = input.get("mode") {
                    let mode = mode
                        .as_str()
                        .and_then(regia_ui::list::SelectionMode::parse)
                        .ok_or_else(|| {
                            DesignerError::new("bad_value", "list mode is none, single or multi")
                        })?;
                    let mut selection = old.clone().unwrap_or_default();
                    selection.mode = mode;
                    self.app.world_mut().entity_mut(entity).insert(selection);
                }
                let selected = regia_ui::list::select_items(
                    self.app.world_mut(),
                    entity,
                    regia_ui::list::SelectOp::Set,
                    &[index],
                );
                if let Err(e) = selected {
                    match old {
                        Some(selection) => {
                            self.app.world_mut().entity_mut(entity).insert(selection);
                        }
                        None => {
                            self.app
                                .world_mut()
                                .entity_mut(entity)
                                .remove::<regia_ui::list::ListSelection>();
                        }
                    }
                    return Err(DesignerError::new("bad_value", format!("{e:?}")));
                }
            }
            "scroll_to" => {
                let index = input
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| DesignerError::new("bad_value", "scroll_to input needs an index"))?;
                regia_ui::list::request_scroll_to_item(
                    self.app.world_mut(),
                    entity,
                    index,
                    regia_ui::list::ScrollAlign::Nearest,
                )
                .map_err(|e| DesignerError::new("bad_value", format!("{e:?}")))?;
            }
            "type" if input.get("text").and_then(Value::as_str) == Some("") => {}
            _ => {
                regia_ui::agent_bridge::call(self.app.world_mut(), method, &params)
                    .map_err(|e| DesignerError::new("bad_value", format!("{}: {}", e.reason, e.message)))?;
            }
        }
        // A click uses press and release frames; the extra frame resolves its new style.
        for _ in 0..3 {
            self.step();
        }
        if action == "type" {
            // Primary+Enter submits both single- and multi-line inputs without adding a newline.
            send_modifiers(
                self.app.world_mut(),
                egui::Modifiers {
                    ctrl: true,
                    command: true,
                    ..Default::default()
                },
            );
            regia_ui::agent_bridge::call(
                self.app.world_mut(),
                "ui.type",
                &json!({"node":entity.to_bits(), "keys":["Enter"]}),
            )
            .map_err(|e| DesignerError::new("bad_value", e.message))?;
            self.step();
            send_modifiers(self.app.world_mut(), egui::Modifiers::default());
            self.step();
        }
        Ok(())
    }

    /// The sample model after the most recent runtime frame, keyed by the original binding path.
    #[must_use]
    pub fn samples(&self) -> BTreeMap<String, String> {
        let mut out = self.samples.clone();
        let model = self.app.world().resource::<DesignerSampleModel>();
        for (path, slot) in &self.slots {
            let value = &model.values[slot.index];
            out.insert(
                path.clone(),
                match slot.field {
                    "flag" => value.flag.to_string(),
                    "number" => value.number.to_string(),
                    _ => value.text.clone(),
                },
            );
        }
        out
    }

    /// Recorded commands, in event sequence order, from the bounded runtime event log.
    pub fn commands(&self) -> Vec<Value> {
        let _span = tracing::trace_span!("ui_designer_interactive_commands").entered();
        self.app
            .world()
            .get_resource::<regia_ui::inspect::UiEventLog>()
            .map(regia_ui::inspect::UiEventLog::commands)
            .unwrap_or_default()
    }

    /// The current computed frame, including the runtime state of sample widgets.
    pub fn frame(&mut self) -> PreviewFrame {
        let _span = tracing::trace_span!("ui_designer_interactive_snapshot").entered();
        let world = self.app.world_mut();
        let mut frame = PreviewFrame {
            size: self.settings.size,
            safe_area: self.settings.safe_area,
            scale: 1.0,
            nodes: Vec::new(),
            truncated: self.truncated,
            lists: Vec::new(),
        };
        for (entity, original) in &self.nodes {
            let mut node = original.clone();
            if let Some(computed) = world.get::<ComputedNode>(*entity) {
                node.rect = computed.rect;
                node.clip = computed.clip_rect;
                frame.scale = computed.scale;
                node.visible =
                    node.rect[2] > 0.0 && node.rect[3] > 0.0 && node.clip[2] > 0.0 && node.clip[3] > 0.0;
            }
            node.text = world.get::<regia_ui::Text>(*entity).map(|text| {
                text.spans
                    .iter()
                    .map(|span| match &span.content {
                        regia_ui::text::TextContent::Lit(text) => text.clone(),
                        regia_ui::text::TextContent::Loc { key, args } => world
                            .get_resource::<regia_ui::Localization>()
                            .map_or_else(|| key.clone(), |loc| loc.resolve(key, args)),
                    })
                    .collect()
            });
            if let Some(input) = world.get::<regia_ui::TextInput>(*entity) {
                node.text = Some(input.display_text());
            }
            if let Some(toggle) = world.get::<regia_ui::Toggle>(*entity) {
                node.text = Some(if toggle.checked { "On" } else { "Off" }.to_owned());
            }
            if let Some(list) = world.get::<regia_ui::ListView>(*entity)
                && let Some(info) = regia_ui::list::list_info(world, *entity)
            {
                frame.lists.push(crate::ui_designer_preview::PreviewList {
                    path: node.path.clone(),
                    items: info.count,
                    row_height: list.item_height,
                    materialized: info.live_rows,
                    first_visible_key: (info.range.start < info.count).then(|| info.range.start.to_string()),
                });
            }
            let [left, top, right, bottom] = self.settings.safe_area;
            let [x, y, w, h] = node.rect;
            node.outside_safe_area = node.path != "/"
                && node.visible
                && (x < left - 0.5
                    || y < top - 0.5
                    || x + w > self.settings.size[0] - right + 0.5
                    || y + h > self.settings.size[1] - bottom + 0.5);
            frame.nodes.push(node);
        }
        frame
    }

    /// Renders this app's current UI, using the same runtime renderer as the still preview.
    pub fn render(&mut self, device: &regia_render::RenderDevice) -> Result<ThemedFrame, DesignerError> {
        let _span = tracing::info_span!("ui_designer_interactive_render").entered();
        let width = self.settings.size[0].round().max(1.0) as u32;
        let height = self.settings.size[1].round().max(1.0) as u32;
        let world = self.app.world_mut();
        world
            .get_resource_or_insert_with(regia_ui::UiOutput::default)
            .safe_area = Some(self.settings.safe_area_insets());
        let spec = regia_render::RenderOutSpec::new(width, height, regia_render::HudLayout::Auto);
        regia_ui::begin_render_out(world, &spec);
        let backdrop = world
            .get_resource::<regia_ui::ThemeResolver>()
            .and_then(|r| r.resolved_theme())
            .and_then(|t| t.color("color.bg"))
            .map_or([0.0; 4], |c| c.0.map(f64::from));
        let target = regia_ui::OffscreenTarget::new(device, width, height);
        target.clear(device, backdrop);
        target.draw_ui(device, &mut world.resource::<regia_ui::SharedUiRenderer>().lock());
        let pixels = target.read(device);
        regia_ui::end_render_out(world, &spec);
        let rgba = pixels.map_err(|e| DesignerError::new("render_failed", e.to_string()))?;
        Ok(ThemedFrame {
            width,
            height,
            rgba,
            theme: self.theme.clone(),
            theme_errors: self.theme_errors.clone(),
        })
    }

    /// Forwards one egui event into the runtime's platform input messages. The caller maps
    /// pointer positions from canvas points to preview pixels and gates input by canvas focus.
    pub fn egui_event(&mut self, event: &egui::Event, origin: egui::Pos2, scale: f32) {
        use regia_platform::*;
        let world = self.app.world_mut();
        let window = Entity::PLACEHOLDER;
        let timestamp = InputTimestamp::ZERO;
        match event {
            egui::Event::PointerMoved(p) => {
                let at = (*p - origin) / scale;
                world.write_message(CursorMoved {
                    window,
                    position: (f64::from(at.x), f64::from(at.y)),
                    timestamp,
                });
            }
            egui::Event::PointerGone => {
                world.write_message(CursorMoved {
                    window,
                    position: (-1.0, -1.0),
                    timestamp,
                });
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            } => {
                send_modifiers(world, *modifiers);
                let at = (*pos - origin) / scale;
                world.write_message(CursorMoved {
                    window,
                    position: (f64::from(at.x), f64::from(at.y)),
                    timestamp,
                });
                let button = match button {
                    egui::PointerButton::Primary => MouseButton::Left,
                    egui::PointerButton::Secondary => MouseButton::Right,
                    egui::PointerButton::Middle => MouseButton::Middle,
                    _ => return,
                };
                world.write_message(MouseButtonInput {
                    window,
                    button,
                    state: if *pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    timestamp,
                });
            }
            egui::Event::MouseWheel { unit, delta, .. } => {
                world.write_message(MouseWheel {
                    window,
                    delta: if *unit == egui::MouseWheelUnit::Line {
                        (delta.x, delta.y)
                    } else {
                        (delta.x / scale, delta.y / scale)
                    },
                    unit: if *unit == egui::MouseWheelUnit::Line {
                        WheelUnit::Line
                    } else {
                        WheelUnit::Pixel
                    },
                    timestamp,
                });
            }
            egui::Event::Key {
                key,
                pressed,
                repeat,
                modifiers,
                ..
            } => {
                send_modifiers(world, *modifiers);
                let name = key_name(*key);
                // Text comes from egui Text/IME, preventing duplicate characters. Letter keys
                // still travel when a command modifier is held (select all, copy, undo).
                if name.chars().count() != 1 || modifiers.command || modifiers.ctrl {
                    world.write_message(KeyboardInput {
                        window,
                        physical_key: name.clone(),
                        logical_key: name,
                        state: if *pressed {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        },
                        repeat: *repeat,
                        timestamp,
                    });
                }
            }
            egui::Event::ModifiersChanged(modifiers) => send_modifiers(world, *modifiers),
            egui::Event::Copy | egui::Event::Cut => {
                send_modifiers(
                    world,
                    egui::Modifiers {
                        ctrl: true,
                        ..Default::default()
                    },
                );
                let key = if matches!(event, egui::Event::Copy) {
                    "C"
                } else {
                    "X"
                };
                world.write_message(KeyboardInput {
                    window,
                    physical_key: key.into(),
                    logical_key: key.into(),
                    state: ButtonState::Pressed,
                    repeat: false,
                    timestamp,
                });
                send_modifiers(world, egui::Modifiers::default());
            }
            egui::Event::Text(text) | egui::Event::Paste(text) => {
                world.write_message(ImeEvent::Commit(window, text.clone()));
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                world.write_message(ImeEvent::Commit(window, text.clone()));
            }
            egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                world.write_message(ImeEvent::Preedit(window, text.clone()));
            }
            _ => {}
        }
    }
}

fn send_modifiers(world: &mut World, modifiers: egui::Modifiers) {
    use regia_platform::{ButtonState, InputTimestamp, KeyboardInput};
    for (name, down) in [
        ("Control", modifiers.ctrl),
        ("Shift", modifiers.shift),
        ("Alt", modifiers.alt),
        ("Super", modifiers.mac_cmd),
    ] {
        world.write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.into(),
            logical_key: name.into(),
            state: if down {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
            repeat: false,
            timestamp: InputTimestamp::ZERO,
        });
    }
}

fn key_name(key: egui::Key) -> String {
    match key {
        egui::Key::ArrowUp => "ArrowUp",
        egui::Key::ArrowDown => "ArrowDown",
        egui::Key::ArrowLeft => "ArrowLeft",
        egui::Key::ArrowRight => "ArrowRight",
        egui::Key::Space => "Space",
        _ => key.name(),
    }
    .to_owned()
}

const TAG: &str = "__designer_live_node_";

fn collect_sample_types(node: &UiNodeDef, out: &mut BTreeMap<String, (BindTargetDef, String)>) {
    use regia_ui::authoring::{TextSpanDef, WidgetDef};
    for bind in node.bind.iter().chain(&node.bindings) {
        let initial = match (bind.target, node.widget.as_ref()) {
            (BindTargetDef::Checked, Some(WidgetDef::Toggle(value))) => value.to_string(),
            (BindTargetDef::Value, Some(WidgetDef::Slider(value, ..)))
            | (BindTargetDef::Progress, Some(WidgetDef::Progress(value))) => value.to_string(),
            (BindTargetDef::Selected, Some(WidgetDef::Dropdown(_, value))) => value.unwrap_or(0).to_string(),
            (BindTargetDef::Selected, Some(WidgetDef::TabBar(_, value))) => value.to_string(),
            (BindTargetDef::Opacity, _) => node.opacity.unwrap_or(1.0).to_string(),
            (BindTargetDef::Visible, _) => (node.style.display != regia_ui::Display::None).to_string(),
            (BindTargetDef::Enabled, _) => (!node.disabled).to_string(),
            _ => node
                .text
                .as_ref()
                .map(|text| {
                    text.spans
                        .iter()
                        .filter_map(|span| match span {
                            TextSpanDef::Lit(text) | TextSpanDef::Full { text, .. } => Some(text.as_str()),
                            TextSpanDef::Loc(_) => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        let entry = out
            .entry(bind.source.clone())
            .or_insert((bind.target, initial.clone()));
        if matches!(entry.0, BindTargetDef::Text) {
            *entry = (bind.target, initial);
        }
    }
    for child in &node.children {
        collect_sample_types(child, out);
    }
}

fn prepare_nodes(
    node: &mut UiNodeDef,
    slots: &BTreeMap<String, SampleSlot>,
    metadata: &mut Vec<(Option<String>, &'static str)>,
) {
    let index = metadata.len();
    metadata.push((node.name.clone(), node_kind(node)));
    node.name = Some(format!("{TAG}{index}"));
    for bind in node.bind.iter_mut().chain(&mut node.bindings) {
        if let Some(slot) = slots.get(&bind.source) {
            bind.source = format!("DesignerSampleModel.values.{}.{}", slot.index, slot.field);
        }
    }
    for child in &mut node.children {
        prepare_nodes(child, slots, metadata);
    }
}

fn sample<'a>(settings: &'a PreviewSettings, path: &str) -> Option<&'a str> {
    if let Some(raw) = settings.samples.get(path) {
        return Some(raw);
    }
    let mut any = String::new();
    let mut inside = false;
    for c in path.chars() {
        match c {
            '[' => {
                inside = true;
                any.push(c);
            }
            ']' => {
                inside = false;
                any.push(c);
            }
            _ if !inside => any.push(c),
            _ => {}
        }
    }
    settings.samples.get(&any).map(String::as_str)
}

impl InteractivePreview {
    /// Runtime frame number for change-driven inspector caches.
    #[must_use]
    pub fn frame_number(&self) -> u64 {
        self.app.world().resource::<regia_app::Time>().frame_count()
    }
}

/// Opens the Theme panel at a token named by the actual attribution result.
pub(crate) fn open_theme_source(
    world: &mut World,
    choice: &ThemeChoice,
    theme: &str,
    token: &str,
) -> Result<(), DesignerError> {
    let mut panel = world.get_resource_or_insert_with(crate::theme_panel::ThemePanel::default);
    let current = panel.draft().map(|d| d.name.as_str());
    if current != Some(theme) {
        if panel.dirty() {
            return Err(DesignerError::new(
                "bad_value",
                "save the Theme panel's draft before opening another theme",
            ));
        }
        let custom = choice.files.iter().find(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| regia_ui::parse_theme_ron(&text, &path.display().to_string()).ok())
                .is_some_and(|file| file.name == theme)
        });
        match custom {
            Some(path) => panel.open_path(path),
            None => panel.open(theme),
        }
        .map_err(|e| DesignerError::new("bad_value", e.to_string()))?;
    }
    panel.filter = token.to_owned();
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::Theme),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (UiFile, PreviewSettings) {
        let mut root = UiNodeDef::default();
        root.style.direction = regia_ui::style::FlexDirection::Column;
        for (preset, name) in [
            ("button", "button"),
            ("toggle", "toggle"),
            ("text_input", "input"),
            ("list", "list"),
        ] {
            let mut node = crate::ui_designer::preset(preset).expect("preset");
            node.name = Some(name.into());
            node.style.size = [
                regia_ui::Dimension::Points(240.0),
                regia_ui::Dimension::Points(48.0),
            ];
            if name == "button" {
                node.command = Some("activate".into());
            }
            if name == "toggle" {
                node.bindings = serde_json::from_value(
                    json!([{"source":"Sample.checked","target":"checked","mode":"two_way"}]),
                )
                .expect("binding");
            }
            if name == "list" {
                node.widget = Some(regia_ui::authoring::WidgetDef::List(20, 24.0));
            }
            root.children.push(node);
        }
        let mut settings = PreviewSettings::default();
        settings.samples.insert("Sample.checked".into(), "false".into());
        (
            UiFile {
                version: regia_ui::UI_RON_VERSION,
                component: None,
                variants: Vec::new(),
                root,
                back: None,
            },
            settings,
        )
    }

    /// `.theme.ron` v2's keyboard-only `focus_visible` layer (ADR-8105 (e)) in the designer's live
    /// preview: Tab shows it, a pointer click on the focused control hides it and keeps the focus.
    #[test]
    fn interactive_focus_visible_shows_on_tab_not_on_click() {
        let (mut file, settings) = fixture();
        file.root.children.truncate(1);
        let mut themes = regia_ui::UiThemes::default();
        let name = themes
            .set
            .load_ron(
                r#"(format_version: 2, name: "fv", extends: Some("regia-dark"),
                    types: { "Button": (focused: { "border_width": 3 }, focus_visible: { "border_width": 7 }) })"#,
                "fv.theme.ron",
            )
            .expect("theme");
        let choice = ThemeChoice {
            themes: Some(themes),
            active: Some(regia_ui::ActiveTheme::named(&name)),
            ..ThemeChoice::default()
        };
        let mut live = InteractivePreview::new(&file, &settings, &choice, None).expect("runtime");
        let button = live.entity("#button").expect("button");
        let key = |live: &mut InteractivePreview, pressed| {
            live.egui_event(
                &egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Pos2::ZERO,
                1.0,
            );
            live.step();
        };
        key(&mut live, true);
        key(&mut live, false);
        for _ in 0..4 {
            live.step();
        }
        assert_eq!(
            live.app.world().resource::<regia_ui::focus::FocusState>().focused,
            Some(button),
            "Tab focuses the button"
        );
        let themed = *live.app.world().get::<regia_ui::Themed>(button).expect("themed");
        assert!(themed.focus_visible, "keyboard focus is visible");
        assert_eq!(themed.border_width, Some(7.0));

        let rect = live
            .app
            .world()
            .get::<ComputedNode>(button)
            .expect("computed")
            .rect;
        let at = egui::pos2(rect[0] + 4.0, rect[1] + 4.0);
        live.egui_event(&egui::Event::PointerMoved(at), egui::Pos2::ZERO, 1.0);
        live.step();
        for pressed in [true, false] {
            live.egui_event(
                &egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Pos2::ZERO,
                1.0,
            );
            live.step();
        }
        for _ in 0..4 {
            live.step();
        }
        assert_eq!(
            live.app.world().resource::<regia_ui::focus::FocusState>().focused,
            Some(button)
        );
        let themed = *live.app.world().get::<regia_ui::Themed>(button).expect("themed");
        assert!(
            themed.focused && !themed.focus_visible,
            "a click hides visible focus"
        );
        assert_eq!(themed.border_width, Some(3.0), "the focused layer only");
    }

    #[test]
    fn interactive_pixels_match_the_control_and_change_on_runtime_hover() {
        let (mut file, mut settings) = fixture();
        file.root.children.truncate(1);
        settings.size = [320.0, 120.0];
        settings.safe_area = [0.0; 4];
        let choice = ThemeChoice {
            active: Some(regia_ui::ActiveTheme {
                reduced_motion: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let device = crate::render::headless_device().expect("lavapipe preview device");
        let control = crate::ui_designer_gpu::themed_preview(&file, &settings, None, &choice, &device)
            .expect("control render");
        let mut live = InteractivePreview::new(&file, &settings, &choice, None).expect("runtime");
        let resting = live.render(&device).expect("live render");
        assert_eq!(resting.rgba.len(), control.rgba.len());
        let differing = resting
            .rgba
            .iter()
            .zip(&control.rgba)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 0, "resting runtime equals the still control");
        let button = live.entity("#button").expect("button");
        let rect = live.app.world().get::<ComputedNode>(button).expect("layout").rect;
        live.egui_event(
            &egui::Event::PointerMoved(egui::pos2(rect[0] + 4.0, rect[1] + 4.0)),
            egui::Pos2::ZERO,
            1.0,
        );
        live.step();
        // Theme resolution runs in PreUpdate, before Update establishes Hovered. Resolve that
        // state on the next deterministic frame, just as the running canvas does.
        live.step();
        let hovered = live.render(&device).expect("hover render");
        let differing = hovered
            .rgba
            .iter()
            .zip(&resting.rgba)
            .filter(|(a, b)| a != b)
            .count();
        assert!(differing > 0, "render captures live interaction state");
    }

    #[test]
    fn interactive_clipboard_events_use_the_runtime_text_widget() {
        let (file, settings) = fixture();
        let mut live =
            InteractivePreview::new(&file, &settings, &ThemeChoice::default(), None).expect("runtime");
        let clipboard = regia_ui::clipboard::MemoryClipboard::default();
        live.app
            .insert_resource(regia_ui::clipboard::UiClipboard::new(clipboard.clone()));
        live.input(&json!({"action":"type","node":"#input","text":"Sample text"}))
            .expect("type");
        let input = live.entity("#input").expect("input");
        live.app
            .world_mut()
            .get_mut::<regia_ui::TextInput>(input)
            .expect("text")
            .select_all();
        live.egui_event(&egui::Event::Copy, egui::Pos2::ZERO, 1.0);
        live.step();
        assert_eq!(clipboard.contents().as_deref(), Some("Sample text"));
        live.egui_event(&egui::Event::Cut, egui::Pos2::ZERO, 1.0);
        live.step();
        assert!(
            live.app
                .world()
                .get::<regia_ui::TextInput>(input)
                .expect("text")
                .text
                .is_empty()
        );
        live.egui_event(&egui::Event::Paste("Sample text".into()), egui::Pos2::ZERO, 1.0);
        live.step();
        assert_eq!(
            live.app
                .world()
                .get::<regia_ui::TextInput>(input)
                .expect("text")
                .text,
            "Sample text"
        );
    }

    #[test]
    fn interactive_repeat_text_writes_only_its_exact_sample_slot() {
        let mut input = crate::ui_designer::preset("text_input").expect("input");
        input.bindings =
            serde_json::from_value(json!([{"source":".name","target":"input","mode":"two_way"}]))
                .expect("bindings");
        let mut list = crate::ui_designer::preset("list").expect("list");
        list.repeat = Some(regia_ui::authoring::RepeatDef {
            source: "Vm.rows".into(),
            key: None,
            ..Default::default()
        });
        list.children.push(input);
        let file = UiFile {
            version: regia_ui::UI_RON_VERSION,
            component: None,
            variants: Vec::new(),
            root: UiNodeDef {
                children: vec![list],
                ..Default::default()
            },
            back: None,
        };
        let mut settings = PreviewSettings::default();
        settings.samples.insert("Vm.rows[].name".into(), "All".into());
        settings.samples.insert("Vm.rows[1].name".into(), "Second".into());
        let mut live =
            InteractivePreview::new(&file, &settings, &ThemeChoice::default(), None).expect("runtime");
        let second = live
            .nodes
            .iter()
            .find(|(_, node)| node.path == "/0/0" && node.item == Some(1))
            .expect("second row")
            .0;
        regia_ui::agent_bridge::call(
            live.app.world_mut(),
            "ui.focus",
            &json!({"node":second.to_bits()}),
        )
        .expect("focus");
        live.step();
        live.app
            .world_mut()
            .get_mut::<regia_ui::TextInput>(second)
            .expect("input")
            .select_all();
        live.egui_event(&egui::Event::Text("Changed".into()), egui::Pos2::ZERO, 1.0);
        live.step();
        live.egui_event(
            &egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Pos2::ZERO,
            1.0,
        );
        live.step();
        assert_eq!(live.samples()["Vm.rows[1].name"], "Changed");
        assert_eq!(live.samples()["Vm.rows[0].name"], "All");
        assert_eq!(settings.samples["Vm.rows[1].name"], "Second");
    }

    #[test]
    fn interactive_runtime_caches_idle_localization_and_refreshes_theme_context() {
        let mut designer = crate::ui_designer::UiDesigner::default();
        designer.new_document();
        let doc = designer.doc().expect("doc");
        let reads = std::cell::Cell::new(0);
        let localization = || {
            reads.set(reads.get() + 1);
            None
        };
        let mut runtime = DesignerRuntime::default();
        let mut choice = ThemeChoice::default();
        runtime.ensure(doc, &choice, localization).expect("first");
        runtime.ensure(doc, &choice, localization).expect("cached");
        assert_eq!(reads.get(), 1);
        choice.active = Some(regia_ui::ActiveTheme::named("regia-light"));
        runtime.ensure(doc, &choice, localization).expect("new theme");
        assert_eq!(reads.get(), 2);
        assert_eq!(runtime.session.as_ref().expect("session").theme, "regia-light");
    }

    #[test]
    fn interactive_runtime_uses_pointer_focus_toggle_scroll_and_sample_bindings() {
        let (file, settings) = fixture();
        let before = regia_ui::write_ui_ron(&file).expect("ron");
        let mut live =
            InteractivePreview::new(&file, &settings, &ThemeChoice::default(), None).expect("runtime");
        let button = live.entity("#button").expect("button");
        let rect = live
            .app
            .world()
            .get::<ComputedNode>(button)
            .expect("computed")
            .rect;
        live.egui_event(
            &egui::Event::PointerMoved(egui::pos2(rect[0] + 4.0, rect[1] + 4.0)),
            egui::Pos2::ZERO,
            1.0,
        );
        live.step();
        assert!(live.app.world().get::<regia_ui::Hovered>(button).is_some());
        live.input(&json!({"action":"click","node":"#toggle"}))
            .expect("toggle");
        assert_eq!(live.samples()["Sample.checked"], "true");
        live.input(&json!({"action":"focus","node":"#button"}))
            .expect("focus");
        assert_eq!(
            live.app.world().resource::<regia_ui::focus::FocusState>().focused,
            Some(button)
        );
        live.egui_event(
            &egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Pos2::ZERO,
            1.0,
        );
        live.step();
        assert!(live.commands().iter().any(|event| event["value"] == "activate"));
        live.input(&json!({"action":"scroll_to","node":"#list","index":19}))
            .expect("scroll");
        let list = live.entity("#list").expect("list");
        assert!(
            regia_ui::list::list_info(live.app.world(), list)
                .expect("list info")
                .offset
                > 0.0
        );
        live.input(&json!({"action":"list","node":"#list","index":19,"mode":"single"}))
            .expect("selection");
        assert_eq!(
            regia_ui::list::list_info(live.app.world(), list)
                .expect("list info")
                .selected,
            vec![19]
        );
        assert_eq!(regia_ui::write_ui_ron(&file).expect("unchanged"), before);
        assert_eq!(settings.samples["Sample.checked"], "false");
    }
}
