// SPDX-License-Identifier: MPL-2.0
//! The World Settings panel (`docs/specs/19-editor.md` § 2, ADR-2696): the `LevelSettings` of the
//! current level, through the reflection inspector.
//!
//! The settings are the world's [`LevelSettings`] resource, which every level save writes, so an
//! edit here saves with the level and marks it dirty through the undo stack. Each edit is one
//! [`SetLevelSettings`]; a slider scrub on one field coalesces into one entry, as in the Inspector.
//!
//! Live effect: what reads the resource every frame follows at once (gravity in play, the look and
//! the atmosphere in the renderer, though the viewport's `editor` quality preset keeps its own sky and
//! exposure); what a level load applies (the game rules, the baked GI volume, the lightmap, ...) takes
//! effect the next time the level opens.
//!
//! The agent surface is `editor.world_settings_get`, `editor.world_settings_set` and
//! `editor.world_settings_panel` (`book/agents/viewport-tools.md`).

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect};
use regia_scene::level::{LEVEL_SETTINGS_PATH, LevelSettings, current_settings};
use serde_json::{Map, Value, json};

use crate::command::{EditorCommand, UndoStack, downcast_to};
use crate::inspector::{InspectorContext, InspectorUi};
use crate::level_io::OpenLevel;
use crate::viewport_agent::Handler;

/// One edit of the level's settings: the whole settings before and after.
pub struct SetLevelSettings {
    /// The settings before.
    pub old: LevelSettings,
    /// The settings after.
    pub new: LevelSettings,
    /// The edited field's reflect path: edits of one field coalesce.
    pub field: String,
    /// A continuous edit (a slider scrub) that merges with the next one of the same field.
    pub coalesce: bool,
}

impl SetLevelSettings {
    /// Captures the world's current settings as the edit's "before".
    #[must_use]
    pub fn capture(world: &World, new: LevelSettings, field: impl Into<String>, coalesce: bool) -> Self {
        Self {
            old: current_settings(world),
            new,
            field: field.into(),
            coalesce,
        }
    }
}

impl EditorCommand for SetLevelSettings {
    fn apply(&mut self, world: &mut World) {
        world.insert_resource(self.new.clone());
    }

    fn revert(&mut self, world: &mut World) {
        world.insert_resource(self.old.clone());
    }

    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = downcast_to::<SetLevelSettings>(next) else {
            return false;
        };
        if self.coalesce && next.coalesce && self.field == next.field {
            self.new = next.new.clone();
            true
        } else {
            false
        }
    }

    fn description(&self) -> String {
        if self.field.is_empty() {
            "World settings".to_owned()
        } else {
            format!("World settings: {}", self.field)
        }
    }
}

/// Applies `new` as one undoable edit. Returns whether anything changed.
pub fn apply_settings(world: &mut World, new: LevelSettings, field: &str, coalesce: bool) -> bool {
    if current_settings(world) == new {
        return false;
    }
    let mut command = SetLevelSettings::capture(world, new, field, coalesce);
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    true
}

/// What a value of the settings must satisfy: finite numbers and the documented ranges. Returns the
/// first problem.
///
/// # Errors
///
/// A message naming the field.
pub fn validate(settings: &LevelSettings) -> Result<(), String> {
    if !settings.gravity.is_finite() {
        return Err("gravity must be finite".to_owned());
    }
    let atmosphere = &settings.atmosphere;
    if !(0.0..=1.0).contains(&atmosphere.cloud_coverage) {
        return Err("atmosphere.cloud_coverage must be from 0 to 1".to_owned());
    }
    if let Some(time) = &atmosphere.time_of_day {
        if !(0.0..=24.0).contains(&time.hour) {
            return Err("atmosphere.time_of_day.hour must be from 0 to 24".to_owned());
        }
        if !(1..=365).contains(&time.day_of_year) {
            return Err("atmosphere.time_of_day.day_of_year must be from 1 to 365".to_owned());
        }
        if !(-90.0..=90.0).contains(&time.latitude_deg) {
            return Err("atmosphere.time_of_day.latitude_deg must be from -90 to 90".to_owned());
        }
    }
    if !settings.look.exposure_bias_ev.is_finite() {
        return Err("look.exposure_bias_ev must be finite".to_owned());
    }
    Ok(())
}

/// The panel's own state.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldSettingsPanel {
    /// What the panel last had to say.
    pub status: String,
}

/// The World Settings row labels (ADR-8586): one per field name of [`LevelSettings`] and the structs
/// inside it. Rows show these; level files, undo entries and the `editor.world_settings_*` params
/// keep the field ids.
pub const FIELD_LABELS: &[(&str, &str)] = &[
    ("gravity", "Gravity"),
    ("environment", "Environment"),
    ("game_rules", "Game Rules"),
    ("ambient_audio", "Ambient Audio"),
    ("streaming_enabled", "Streaming"),
    ("atmosphere", "Atmosphere"),
    ("look", "Look"),
    ("audio_medium", "Audio Medium"),
    ("terrain", "Terrain"),
    ("layers", "Layers"),
    ("map", "Map"),
    ("world_width_m", "World Width (m)"),
    ("height_scale_m", "Height Scale (m)"),
    ("height_offset_m", "Height Offset (m)"),
    ("sample_layout", "Sample Layout"),
    ("airborne", "Airborne"),
    ("speed_of_sound_mps", "Speed of Sound (m/s)"),
    ("level_offset_db", "Level Offset (dB)"),
    ("absorption_db_per_m_khz2", "Absorption (dB/m/kHz²)"),
    ("ground", "Ground"),
    ("hour", "Hour"),
    ("day_of_year", "Day of Year"),
    ("latitude_deg", "Latitude (°)"),
    ("exposure_bias_ev", "Exposure Bias (EV)"),
    ("min_ev100", "Min EV100"),
    ("max_ev100", "Max EV100"),
    ("meter", "Meter"),
    ("contrast", "Contrast"),
    ("saturation", "Saturation"),
    ("grade", "Grade"),
    ("lut", "LUT"),
    ("local_exposure", "Local Exposure"),
    ("baked_gi", "Baked GI"),
    ("lightmap", "Lightmap"),
    ("captures", "Captures"),
    ("file", "File"),
    ("enabled", "Enabled"),
    ("intensity", "Intensity"),
    ("volume", "Volume"),
    ("settings", "Settings"),
    ("sky_scale", "Sky Scale"),
    ("emissive_scale", "Emissive Scale"),
    ("replace_live_fill", "Replace Live Fill"),
    ("shadow_contrast", "Shadow Contrast"),
    ("highlight_contrast", "Highlight Contrast"),
    ("detail_strength", "Detail Strength"),
    ("low_percentile", "Low Percentile"),
    ("high_percentile", "High Percentile"),
    ("center_weight", "Center Weight"),
    ("bright_threshold_cd_m2", "Bright Threshold (cd/m²)"),
    ("bright_bias_ev", "Bright Bias (EV)"),
    ("white_balance_kelvin", "White Balance (K)"),
    ("tint", "Tint"),
    ("lift", "Lift"),
    ("gamma", "Gamma"),
    ("gain", "Gain"),
    ("density", "Density"),
    ("scale_height_m", "Scale Height (m)"),
    ("base_height_m", "Base Height (m)"),
    ("preset", "Preset"),
    ("time_of_day", "Time of Day"),
    ("sun_illuminance_lux", "Sun Illuminance (lux)"),
    ("cloud_coverage", "Cloud Coverage"),
    ("cloud_type", "Cloud Type"),
    ("sky_body", "Sky Body"),
    ("moons", "Moons"),
    ("haze", "Haze"),
    ("aerosol_turbidity", "Aerosol Turbidity"),
    ("angular_diameter_deg", "Angular Diameter (°)"),
    ("bands", "Bands"),
    ("direction", "Direction"),
    ("fill_lux", "Fill (lux)"),
];

/// The row label of the settings field at reflect `path` (`look.exposure_bias_ev` -> `Exposure Bias
/// (EV)`): its last named segment looked up in [`FIELD_LABELS`].
#[must_use]
pub fn field_label(path: &str) -> Option<&'static str> {
    let name = path
        .rsplit('.')
        .map(|segment| segment.split('[').next().unwrap_or(segment))
        .find(|segment| !segment.is_empty() && !segment.bytes().all(|b| b.is_ascii_digit()))?;
    FIELD_LABELS
        .iter()
        .find(|(id, _)| *id == name)
        .map(|(_, label)| *label)
}

/// Draws the World Settings panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "world_settings_panel")]
pub fn world_settings_panel(ui: &mut egui::Ui, world: &mut World) {
    let level = world.get_resource::<OpenLevel>().map(|open| {
        open.path
            .as_ref()
            .map_or_else(|| "(unsaved level)".to_owned(), |p| p.display().to_string())
    });
    ui.horizontal(|ui| {
        ui.strong("World Settings");
        ui.weak(level.as_deref().unwrap_or("(no level)"));
    });
    if world.get_resource::<OpenLevel>().is_none() {
        ui.weak("Open a level to edit its settings.");
        return;
    }
    let current = current_settings(world);
    let mut status = world
        .get_resource::<WorldSettingsPanel>()
        .map(|panel| panel.status.clone())
        .unwrap_or_default();
    ui.horizontal(|ui| {
        if ui
            .button("Reset to defaults")
            .on_hover_text("Every setting of this level back to the engine default (undoable)")
            .clicked()
            && apply_settings(world, LevelSettings::default(), "", false)
        {
            status = "Settings reset to the defaults".to_owned();
        }
        ui.weak(&status);
    });
    ui.separator();
    // Game Rules as a picker of the registered names and the project's rule sets (ADR-8738); the
    // reflected text row below keeps a custom name.
    crate::match_rules_panel::game_rules_row(ui, world);
    ui.separator();
    let Some(registry) = world.get_resource::<AppTypeRegistry>().cloned() else {
        ui.weak("The type registry is not available.");
        return;
    };
    let mut edited = current.clone();
    let mut context = InspectorContext::new(LEVEL_SETTINGS_PATH).with_field_labels(field_label);
    egui::ScrollArea::vertical().show(ui, |ui| {
        let registry = registry.read();
        InspectorUi::new(&registry, &mut context).ui_for_reflect_with_options(
            edited.as_partial_reflect_mut(),
            ui,
            egui::Id::new("world-settings"),
            &(),
        );
    });
    let changes = context.take_changes();
    if !changes.is_empty() && edited != current {
        match validate(&edited) {
            Ok(()) => {
                // One frame edits one field; a structural change is its own entry.
                let (field, coalesce) = changes
                    .first()
                    .map_or((String::new(), false), |c| (c.path.clone(), !c.discrete));
                apply_settings(world, edited, &field, coalesce);
                status.clear();
            }
            Err(problem) => status = problem,
        }
    }
    world.insert_resource(WorldSettingsPanel { status });
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn refused(reason: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, reason, message, hint)
}

fn settings_json(settings: &LevelSettings) -> Result<Value, RpcError> {
    serde_json::to_value(settings).map_err(|e| RpcError::internal(format!("settings as JSON: {e}")))
}

fn reply(world: &mut World, changed: Option<bool>) -> Result<Value, RpcError> {
    // The Game Rules picker's choices and the name's status (ADR-8738).
    let game_rules = crate::match_rules_panel::game_rules_json(world);
    let open = world.get_resource::<OpenLevel>();
    let mut out = json!({
        "settings": settings_json(&current_settings(world))?,
        "level": open.and_then(|o| o.path.as_ref()).map(|p| p.display().to_string()),
        "dirty": world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty),
        "game_rules": game_rules,
    });
    if let Some(changed) = changed {
        out["changed"] = json!(changed);
    }
    Ok(out)
}

/// `patch` merged into `base`: objects field by field, everything else replaced.
fn merge_json(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (slot, value) => *slot = value.clone(),
    }
}

/// The first field of `patch` the settings do not hold after a read: an unknown name (serde ignores
/// those) or a number the type cannot hold. Nulls clear optional fields, which may then not be written.
fn missing_field(path: &str, patch: &Map<String, Value>, read: &Map<String, Value>) -> Option<String> {
    for (key, value) in patch {
        let here = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if value.is_null() {
            continue;
        }
        match (value, read.get(key)) {
            (_, None | Some(Value::Null)) => return Some(here),
            (Value::Object(patch), Some(Value::Object(read))) => {
                if let Some(found) = missing_field(&here, patch, read) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// `editor.world_settings_get`.
fn world_settings_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    reply(world, None)
}

/// `editor.world_settings_set`.
fn world_settings_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<OpenLevel>().is_none() {
        return Err(refused(
            "world_settings_no_level",
            "no level is open",
            "open one with editor.open_level",
        ));
    }
    if params.get("reset").and_then(Value::as_bool).unwrap_or(false) {
        let changed = apply_settings(world, LevelSettings::default(), "", false);
        return reply(world, Some(changed));
    }
    let Some(patch) = params.get("settings").and_then(Value::as_object) else {
        return Err(refused(
            "world_settings_bad_value",
            "`settings` must be an object of the fields to change",
            "{ \"gravity\": [0, -1.62, 0] }; see editor.world_settings_get",
        ));
    };
    let mut merged = settings_json(&current_settings(world))?;
    merge_json(&mut merged, &Value::Object(patch.clone()));
    let next: LevelSettings = serde_json::from_value(merged).map_err(|e| {
        refused(
            "world_settings_bad_value",
            format!("the settings do not read back: {e}"),
            "see editor.world_settings_get for the shape",
        )
    })?;
    if let Value::Object(read) = settings_json(&next)?
        && let Some(field) = missing_field("", patch, &read)
    {
        return Err(refused(
            "world_settings_unknown_field",
            format!("{field} is not a level setting, or its value is out of range for its type"),
            "see editor.world_settings_get for the fields",
        ));
    }
    validate(&next).map_err(|problem| {
        refused(
            "world_settings_bad_value",
            problem,
            "see book/agents/viewport-tools.md",
        )
    })?;
    let changed = apply_settings(world, next, "", false);
    reply(world, Some(changed))
}

/// `editor.world_settings_panel`.
fn world_settings_panel_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::WorldSettings),
    );
    Ok(json!({ "opened": "world_settings" }))
}

/// The World Settings agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.world_settings_get",
                tool: "world_settings_get",
                description: "The current level's LevelSettings (gravity, environment, game rules, ambient audio, streaming, atmosphere, look, audio medium) as the World Settings panel edits them, plus game_rules: the Game Rules picker's registered names and rule sets and the current name's status (custom = game_rules_unregistered).",
                params: &[],
                result: "{ settings, level, dirty, game_rules: { current, status, warning, registered, rule_sets } }",
                mutating: false,
                permission: Permission::Read,
            },
            world_settings_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.world_settings_set",
                tool: "world_settings_set",
                description: "Changes the current level's LevelSettings as one undo step: the fields named in settings merge over the current ones (null clears an optional field), or reset returns them all to the defaults. The next level save writes them.",
                params: &[
                    ParamSchema {
                        name: "settings",
                        kind: "object",
                        required: false,
                        description: "The fields to change, e.g. { \"gravity\": [0, -1.62, 0], \"atmosphere\": { \"preset\": \"Mars\" } }.",
                    },
                    ParamSchema {
                        name: "reset",
                        kind: "boolean",
                        required: false,
                        description: "Return every setting to the engine default.",
                    },
                ],
                result: "{ settings, level, dirty, game_rules, changed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            world_settings_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.world_settings_panel",
                tool: "world_settings_panel",
                description: "Brings the World Settings tab forward (adding it when closed) on the editor's next frame.",
                params: &[],
                result: "{ opened }",
                mutating: false,
                permission: Permission::Read,
            },
            world_settings_panel_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrub_of_one_field_is_one_entry_and_another_field_is_not() {
        let mut world = World::new();
        let step = |world: &mut World, gravity: f32, field: &str| {
            let mut next = current_settings(world);
            next.gravity.y = gravity;
            apply_settings(world, next, field, true);
        };
        step(&mut world, -1.0, "gravity.y");
        step(&mut world, -2.0, "gravity.y");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        step(&mut world, -3.0, "gravity.x");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
        // An unchanged value is no entry.
        let same = current_settings(&world);
        assert!(!apply_settings(&mut world, same, "gravity.y", true));
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    }

    #[test]
    fn validate_names_the_field() {
        let mut settings = LevelSettings::default();
        assert_eq!(validate(&settings), Ok(()));
        settings.gravity.x = f32::NAN;
        assert!(validate(&settings).expect_err("refused").contains("gravity"));
        let mut settings = LevelSettings::default();
        settings.atmosphere.cloud_coverage = 2.0;
        assert!(
            validate(&settings)
                .expect_err("refused")
                .contains("cloud_coverage")
        );
    }

    /// Every field name of the level settings (and the regia_scene structs inside them).
    fn settings_field_names(
        info: &'static regia_ecs::reflect::TypeInfo,
        out: &mut std::collections::BTreeSet<&'static str>,
    ) {
        use regia_ecs::reflect::TypeInfo;
        use regia_ecs::reflect::enums::VariantInfo;
        let ours = |i: &TypeInfo| {
            i.type_path().starts_with("regia_scene::")
                || i.type_path().starts_with("core::option::Option<regia_scene::")
                || i.type_path().starts_with("alloc::vec::Vec<regia_scene::")
        };
        if !ours(info) {
            return;
        }
        match info {
            TypeInfo::Struct(s) => {
                for field in s.iter() {
                    out.insert(field.name());
                    if let Some(inner) = field.type_info() {
                        settings_field_names(inner, out);
                    }
                }
            }
            TypeInfo::Enum(e) => {
                for variant in e.iter() {
                    match variant {
                        VariantInfo::Struct(v) => {
                            for field in v.iter() {
                                out.insert(field.name());
                                if let Some(inner) = field.type_info() {
                                    settings_field_names(inner, out);
                                }
                            }
                        }
                        VariantInfo::Tuple(v) => {
                            for field in v.iter() {
                                if let Some(inner) = field.type_info() {
                                    settings_field_names(inner, out);
                                }
                            }
                        }
                        VariantInfo::Unit(_) => {}
                    }
                }
            }
            TypeInfo::List(l) => {
                if let Some(inner) = l.item_info() {
                    settings_field_names(inner, out);
                }
            }
            _ => {}
        }
    }

    /// Bug 22 (ADR-8586): every settings field has a title-case row label; the ids stay snake_case.
    #[test]
    fn every_settings_field_has_a_row_label() {
        use regia_ecs::reflect::Typed;
        let mut names = std::collections::BTreeSet::new();
        settings_field_names(LevelSettings::type_info(), &mut names);
        assert!(
            names.contains("game_rules") && names.contains("ambient_audio"),
            "{names:?}"
        );
        let missing: Vec<_> = names.iter().filter(|n| field_label(n).is_none()).collect();
        assert!(
            missing.is_empty(),
            "World Settings fields without a label in FIELD_LABELS: {missing:?}"
        );
        for (id, label) in FIELD_LABELS {
            assert!(
                !label.contains('_') && label.starts_with(|c: char| c.is_ascii_uppercase()),
                "{id}: {label}"
            );
        }
        assert_eq!(field_label("game_rules"), Some("Game Rules"));
        assert_eq!(field_label("ambient_audio"), Some("Ambient Audio"));
        assert_eq!(field_label("look.exposure_bias_ev"), Some("Exposure Bias (EV)"));
        assert_eq!(
            field_label("atmosphere.moons[0].x"),
            None,
            "a vector lane keeps its own label"
        );
        assert_eq!(field_label("unknown_field"), None);
    }

    #[test]
    fn merge_replaces_leaves_and_recurses_into_objects() {
        let mut base = json!({ "a": { "b": 1, "c": 2 }, "d": [1, 2] });
        merge_json(&mut base, &json!({ "a": { "b": 5 }, "d": [3] }));
        assert_eq!(base, json!({ "a": { "b": 5, "c": 2 }, "d": [3] }));
    }
}
