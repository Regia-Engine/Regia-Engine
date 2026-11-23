// SPDX-License-Identifier: MPL-2.0
//! Rich tooltips (ADR-7767, `docs/specs/19-editor.md` § 2): one builder every panel uses, so a
//! menu item, a palette entry, a preference and an Inspector field all show the same parts in the
//! same order: the title, what it does, its default value, its shortcut and the agent route that
//! does the same (an `editor.command` id, an agent method, or the `editor.set_property` path).
//!
//! [`RichTooltip::show`] draws it inside `on_hover_ui`; [`RichTooltip::text`] is the same content
//! as plain lines (tests, the agent's `editor.help_context`). Inspector field rows call
//! [`RichTooltip::for_field`] with [`field_doc`] and [`field_default`] (the field rows of
//! `inspector/reflect_ui.rs` belong to job 3216, which wires the call).

use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_ecs::reflect::{TypeInfo, TypeRegistration};

use crate::agent_parity::{ActionPath, EditorAction};
use crate::command_palette::{CommandDef, CommandKeymap, find};
use crate::editor_keymap::ShortcutScope;
use crate::editor_prefs::PrefDef;

/// Longest default value shown, characters (a long list is cut with `…`).
const MAX_VALUE_CHARS: usize = 80;

/// The parts of one tooltip; empty parts are not drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichTooltip {
    /// The control's name.
    pub title: String,
    /// What it does.
    pub description: String,
    /// The value it has with nothing set.
    pub default_value: Option<String>,
    /// Its keyboard shortcut (with the scope when not global).
    pub shortcut: Option<String>,
    /// What an agent calls to do the same.
    pub agent: Option<String>,
}

fn shortcut_text(keymap: &CommandKeymap, def: &CommandDef) -> Option<String> {
    let keys = keymap.shortcut_of(def);
    if keys.is_empty() {
        return None;
    }
    let scope = if keymap.bindings.contains_key(def.id) {
        keymap.scope_of(def.id)
    } else {
        crate::editor_keymap::builtin_scope(def.id)
    };
    Some(if scope == ShortcutScope::Global {
        keys
    } else {
        format!("{keys} (in the {})", scope.label())
    })
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_VALUE_CHARS {
        return text;
    }
    let mut out: String = text.chars().take(MAX_VALUE_CHARS - 1).collect();
    out.push('…');
    out
}

impl RichTooltip {
    /// A palette command: its title, category, shortcut and `editor.command { run }`.
    #[must_use]
    pub fn for_command(def: &CommandDef, keymap: &CommandKeymap) -> Self {
        Self {
            title: def.title.to_owned(),
            description: format!("{} command", def.category),
            default_value: None,
            shortcut: shortcut_text(keymap, def),
            agent: Some(format!("editor.command {{ run: \"{}\" }}", def.id)),
        }
    }

    /// A registered menu or toolbar action with its description; the shortcut comes from the
    /// palette command it runs, when it runs one.
    #[must_use]
    pub fn for_action(action: &EditorAction, description: &str, keymap: Option<&CommandKeymap>) -> Self {
        if let ActionPath::Command(id) = action.path
            && let Some(def) = find(id)
        {
            let mut tip = Self::for_command(&def, keymap.unwrap_or(&CommandKeymap::default()));
            tip.title = action.label.trim_end_matches('…').to_owned();
            if !description.is_empty() {
                tip.description = description.to_owned();
            }
            return tip;
        }
        let agent = match action.path {
            ActionPath::Method(m) => Some(m.to_owned()),
            ActionPath::Command(id) => Some(format!("editor.command {{ run: \"{id}\" }}")),
            ActionPath::OsDialog(m) => Some(format!("{m} {{ path }}")),
            ActionPath::Confirm(m) => Some(format!("{m} (states the choice)")),
            ActionPath::ViewOnly => None,
        };
        let shortcut = match (action.path, keymap) {
            (ActionPath::Command(id), Some(k)) => find(id).and_then(|def| shortcut_text(k, &def)),
            _ => None,
        };
        Self {
            title: action.label.trim_end_matches('…').to_owned(),
            description: if description.is_empty() {
                match action.path {
                    ActionPath::OsDialog(_) => "Choose a local file for this action.".to_owned(),
                    ActionPath::Confirm(_) => "Review and confirm this action.".to_owned(),
                    _ => format!("{} in {}.", action.label.trim_end_matches('…'), action.panel),
                }
            } else {
                description.to_owned()
            },
            default_value: None,
            shortcut,
            agent,
        }
    }

    /// An editor preference: its description, default and `editor.prefs_set`.
    #[must_use]
    pub fn for_pref(def: &PrefDef) -> Self {
        let default = def.default.value().to_json();
        Self {
            title: def.label.to_owned(),
            description: def.description.to_owned(),
            default_value: Some(clip(default.to_string())),
            shortcut: None,
            agent: Some(format!("editor.prefs_set {{ values: {{ \"{}\": … }} }}", def.key)),
        }
    }

    /// An Inspector field: `doc` (the field's reflect docs or its Tooltip attribute), its default
    /// value and the `editor.set_property` path that sets it.
    #[must_use]
    pub fn for_field(component: &str, path: &str, doc: Option<&str>, default_value: Option<String>) -> Self {
        Self {
            title: path.rsplit('.').next().unwrap_or(path).to_owned(),
            description: doc.map(str::trim).unwrap_or_default().to_owned(),
            default_value: default_value.map(clip),
            shortcut: None,
            agent: Some(format!(
                "editor.set_property {{ entity, component: \"{component}\", path: \"{path}\", value }}"
            )),
        }
    }

    /// The tooltip as plain lines, in drawing order.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![self.title.clone()];
        if !self.description.is_empty() {
            out.push(self.description.clone());
        }
        if let Some(v) = &self.default_value {
            out.push(format!("Default: {v}"));
        }
        if let Some(k) = &self.shortcut {
            out.push(format!("Shortcut: {k}"));
        }
        if let Some(a) = &self.agent {
            out.push(format!("Agent: {a}"));
        }
        out
    }

    /// [`Self::lines`] joined with newlines.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// Draws the tooltip (inside `on_hover_ui`): the title strong, the description, then the
    /// default, shortcut and agent route weak. Theme text colors only; it scales with the UI.
    pub fn show(&self, ui: &mut egui::Ui) {
        ui.set_max_width(ui.spacing().tooltip_width);
        ui.strong(&self.title);
        if !self.description.is_empty() {
            ui.label(&self.description);
        }
        if let Some(v) = &self.default_value {
            ui.weak(format!("Default: {v}"));
        }
        if let Some(k) = &self.shortcut {
            ui.weak(format!("Shortcut: {k}"));
        }
        if let Some(a) = &self.agent {
            ui.weak(format!("Agent: {a}"));
        }
    }
}

/// Shares the current keymap with menu widgets, which do not borrow the editor world.
#[tracing::instrument(target = "regia_editor", skip_all, name = "tooltip_keymap_sync")]
pub(crate) fn sync_keymap(ctx: &egui::Context, keymap: &CommandKeymap) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("rich_tooltip_keymap"), Arc::new(keymap.clone())));
}

/// The same rich tooltip builder as the palette, using the current user keymap when available.
pub(crate) fn menu_tooltip(ui: &egui::Ui, action: &EditorAction) -> RichTooltip {
    let keymap = ui
        .data_mut(|data| data.get_temp::<Arc<CommandKeymap>>(egui::Id::new("rich_tooltip_keymap")))
        .unwrap_or_default();
    RichTooltip::for_action(action, "", Some(&keymap))
}

/// Adds `tip` to `response` as its hover tooltip.
pub fn attach(response: egui::Response, tip: &RichTooltip) -> egui::Response {
    response
        .on_hover_ui(|ui| tip.show(ui))
        .on_disabled_hover_ui(|ui| tip.show(ui))
}

/// The registration of component `component` (short or full type path).
fn registration<'a>(registry: &'a TypeRegistry, component: &str) -> Option<&'a TypeRegistration> {
    registry
        .get_with_short_type_path(component)
        .or_else(|| registry.get_with_type_path(component))
}

/// The doc comment of field `field` (a top-level field name) of `component`, when the type was
/// compiled with reflect documentation.
#[must_use]
pub fn field_doc(registry: &TypeRegistry, component: &str, field: &str) -> Option<&'static str> {
    match registration(registry, component)?.type_info() {
        TypeInfo::Struct(info) => info.field(field)?.docs(),
        _ => None,
    }
    .map(str::trim)
    .filter(|d| !d.is_empty())
}

/// The default value of field `path` (reflect path syntax) of `component`, from its reflected
/// `Default`, as debug text (`None` without a reflected default or for a path that does not
/// resolve).
#[must_use]
pub fn field_default(registry: &TypeRegistry, component: &str, path: &str) -> Option<String> {
    let default = registration(registry, component)?
        .data::<ReflectDefault>()?
        .default();
    let value: &dyn Reflect = default.as_ref();
    let field = if path.is_empty() {
        value.as_partial_reflect()
    } else {
        value.reflect_path(path).ok()?
    };
    Some(clip(format!("{field:?}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A component with documented fields.
    #[derive(Component, Reflect, Default)]
    #[reflect(Component, Default)]
    struct Lamp {
        /// Light output in lumens.
        brightness: f32,
        radius: f32,
    }

    #[test]
    fn a_field_tooltip_has_its_doc_default_and_agent_path() {
        let mut registry = TypeRegistry::default();
        registry.register::<Lamp>();
        assert_eq!(
            field_doc(&registry, "Lamp", "brightness"),
            Some("Light output in lumens.")
        );
        assert_eq!(field_doc(&registry, "Lamp", "radius"), None, "no doc comment");
        assert_eq!(
            field_default(&registry, "Lamp", "brightness").as_deref(),
            Some("0.0")
        );
        assert_eq!(field_default(&registry, "Lamp", "nope"), None);
        let tip = RichTooltip::for_field(
            "Lamp",
            "brightness",
            field_doc(&registry, "Lamp", "brightness"),
            field_default(&registry, "Lamp", "brightness"),
        );
        assert_eq!(
            tip.lines(),
            [
                "brightness",
                "Light output in lumens.",
                "Default: 0.0",
                "Agent: editor.set_property { entity, component: \"Lamp\", path: \"brightness\", value }",
            ]
        );
    }

    #[test]
    fn command_action_and_pref_tooltips_name_shortcut_and_agent_route() {
        let mut keymap = CommandKeymap::default();
        let undo = find("edit.undo").unwrap();
        let tip = RichTooltip::for_command(&undo, &keymap);
        assert_eq!(tip.shortcut.as_deref(), Some("Ctrl+Z"));
        assert_eq!(
            tip.agent.as_deref(),
            Some("editor.command { run: \"edit.undo\" }")
        );
        // A scoped user binding names its scope.
        keymap
            .bind_in(
                "view.toggle_grid",
                crate::command_palette::KeyChord::parse("X").unwrap(),
                ShortcutScope::Viewport,
            )
            .unwrap();
        let grid = find("view.toggle_grid").unwrap();
        assert_eq!(
            RichTooltip::for_command(&grid, &keymap).shortcut.as_deref(),
            Some("X (in the Viewport)")
        );
        let action = RichTooltip::for_action(&crate::agent_parity::HELP_ABOUT, "Version and license", None);
        assert!(action.agent.is_some(), "{action:?}");
        assert_eq!(action.description, "Version and license");
        let pref = RichTooltip::for_pref(crate::editor_prefs::find_pref("appearance.ui_scale").unwrap());
        assert_eq!(pref.default_value.as_deref(), Some("1.0"));
        assert!(pref.text().contains("editor.prefs_set"), "{}", pref.text());
    }

    #[test]
    fn every_registered_action_has_a_description_and_its_registered_route() {
        let ctx = egui::Context::default();
        let mut keymap = CommandKeymap::default();
        keymap
            .bind(
                "edit.undo",
                crate::command_palette::KeyChord::parse("Alt+Z").unwrap(),
            )
            .unwrap();
        sync_keymap(&ctx, &keymap);
        ctx.run_ui(egui::RawInput::default(), |ui| {
            for action in crate::agent_parity::ACTIONS {
                let tip = menu_tooltip(ui, action);
                assert!(!tip.title.is_empty(), "{}", action.id);
                assert!(!tip.description.is_empty(), "{}", action.id);
                if let Some(route) = action.path.via() {
                    assert!(
                        tip.agent.as_ref().is_some_and(|text| text.contains(route)),
                        "{}: {tip:?}",
                        action.id
                    );
                }
            }
            assert_eq!(
                menu_tooltip(ui, &crate::agent_parity::EDIT_UNDO)
                    .shortcut
                    .as_deref(),
                Some("Alt+Z")
            );
        })
        .textures_delta
        .clear();
    }

    #[test]
    fn menu_items_carry_the_agent_path_in_their_tooltip() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.interaction.tooltip_delay = 0.0);
        let mut rect = egui::Rect::NOTHING;
        let mut text = String::new();
        for frame in 0..5 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    time: Some(f64::from(frame)),
                    events: if frame == 0 {
                        vec![]
                    } else {
                        vec![egui::Event::PointerMoved(rect.center())]
                    },
                    ..Default::default()
                },
                |ui| {
                    rect = crate::agent_parity::action_menu_item(ui, &crate::agent_parity::EDIT_UNDO).rect;
                },
            );
            output.textures_delta.clear();
            for shape in output.shapes {
                if let egui::epaint::Shape::Text(shape) = shape.shape {
                    text.push_str(shape.galley.text());
                }
            }
        }
        assert!(text.contains("Agent: editor.command"), "{text}");
        assert!(text.contains("edit.undo"), "{text}");
        assert!(text.contains("Shortcut: Ctrl+Z"), "{text}");
    }

    #[test]
    fn long_defaults_are_clipped() {
        let tip = RichTooltip::for_field("C", "list", None, Some("x".repeat(500)));
        assert_eq!(tip.default_value.unwrap().chars().count(), MAX_VALUE_CHARS);
    }
}
