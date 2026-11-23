// SPDX-License-Identifier: MPL-2.0
//! Import presets (w12 UI parity addendum § 8.10, P-150, ADR-7904): named importer settings per
//! importer kind, shared by the project in `config/import_presets.ron` (format `import-presets`
//! v1, [`regia_asset::import_presets`]).
//!
//! - **Save**: the Import settings panel's form (or a settings text an agent passes) becomes a
//!   named preset of the form's importer. The per-file `clips` key (animation names belong to
//!   one file) is never stored in a preset.
//! - **Apply**: a preset's settings replace the form of every source the panel edits (the
//!   panel's multi-source editing, ADR-7744), keeping each source's own `clips`; nothing is
//!   written until the panel's Apply (or `write: true`), which writes every `.meta` and cooks
//!   once. The import dialog applies a preset to the file it shows (and with Apply to all, to
//!   every remaining file of that importer).
//! - **Delete** removes the preset from the file.
//!
//! Every change reads the file again and writes it atomically, so presets another user committed
//! meanwhile are kept; a file that does not parse is reported (`import_presets_unreadable`) and is
//! never overwritten.
//!
//! Agent surface: `editor.import_presets { action: list | save | apply | delete }`
//! (`book/agents/import-settings.md`).

use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::import_presets::{
    IMPORT_PRESETS_PATH, ImportPreset, ImportPresetsFile, preset_name_problem,
};
use regia_asset::import_rules::SettingsObject;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_ext::EditorProject;
use crate::asset_ops_agent::reasoned;
use crate::import_settings_panel::{ImportSettingsPanel, ImporterKind, validate_settings};
use crate::viewport_agent::Handler;

/// The settings key a preset never stores or replaces: per-file clip options (ADR-7904).
pub const PER_FILE_KEYS: &[&str] = &["clips"];

/// The project's presets, or the default (empty) file when there is none yet.
///
/// # Errors
///
/// `import_presets_unreadable` when the file exists and does not read or parse.
pub fn load(root: &Path) -> Result<ImportPresetsFile, RpcError> {
    let path = root.join(IMPORT_PRESETS_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => ImportPresetsFile::from_ron(&text).map_err(|e| {
            reasoned(
                "import_presets_unreadable",
                format!("{IMPORT_PRESETS_PATH} does not parse ({e}); fix it or remove it"),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ImportPresetsFile::default()),
        Err(e) => Err(reasoned(
            "import_presets_unreadable",
            format!("cannot read {IMPORT_PRESETS_PATH}: {e}"),
        )),
    }
}

fn store(root: &Path, file: &ImportPresetsFile) -> Result<(), RpcError> {
    let text = file
        .to_ron_string()
        .map_err(|e| reasoned("import_presets_io", format!("cannot serialize the presets: {e}")))?;
    crate::atomic_write::write_atomic(&root.join(IMPORT_PRESETS_PATH), text.as_bytes()).map_err(|e| {
        reasoned(
            "import_presets_io",
            format!("cannot write {IMPORT_PRESETS_PATH}: {e}"),
        )
    })
}

/// `object` without the per-file keys ([`PER_FILE_KEYS`]).
#[must_use]
pub fn shareable(object: &SettingsObject) -> SettingsObject {
    SettingsObject {
        fields: object
            .fields
            .iter()
            .filter(|(key, _)| !PER_FILE_KEYS.contains(&key.as_str()))
            .cloned()
            .collect(),
    }
}

fn settings_text(object: &SettingsObject) -> String {
    if object.fields.is_empty() {
        "()".to_string()
    } else {
        object.to_ron()
    }
}

/// Saves `settings` as the `kind` preset `name` (replacing the settings of a preset with that
/// name, ignoring case). Answers whether one was replaced.
///
/// # Errors
///
/// `import_preset_name_invalid`, `asset_settings_invalid` (a value the importer refuses),
/// `import_presets_unreadable` or `import_presets_io`.
pub fn save(
    root: &Path,
    kind: ImporterKind,
    name: &str,
    settings: &SettingsObject,
) -> Result<bool, RpcError> {
    if let Some(problem) = preset_name_problem(name) {
        return Err(reasoned(
            "import_preset_name_invalid",
            format!("the preset name {name:?} {problem}"),
        ));
    }
    let text = settings_text(&shareable(settings));
    validate_settings(kind.sample_path(), &text)
        .map_err(|message| reasoned("asset_settings_invalid", message))?;
    let mut file = load(root)?;
    let replaced = file
        .upsert(ImportPreset {
            name: name.trim().to_string(),
            importer: kind.name().to_string(),
            settings: text,
        })
        .map_err(|e| reasoned("import_preset_name_invalid", e.0))?;
    store(root, &file)?;
    Ok(replaced)
}

/// Removes the `kind` preset `name`.
///
/// # Errors
///
/// `import_preset_not_found`, `import_presets_unreadable` or `import_presets_io`.
pub fn delete(root: &Path, kind: ImporterKind, name: &str) -> Result<(), RpcError> {
    let mut file = load(root)?;
    if !file.remove(kind.name(), name) {
        return Err(not_found(kind, name));
    }
    store(root, &file)
}

fn not_found(kind: ImporterKind, name: &str) -> RpcError {
    reasoned(
        "import_preset_not_found",
        format!(
            "no {} preset is named {name:?} (editor.import_presets {{ action: \"list\" }})",
            kind.name()
        ),
    )
}

/// The settings `form` takes when the preset `preset` is applied: the preset's fields, plus the
/// form's own per-file keys ([`PER_FILE_KEYS`]).
///
/// # Errors
///
/// The preset's settings do not parse (`asset_settings_invalid`).
pub fn applied(form: &SettingsObject, preset: &ImportPreset) -> Result<SettingsObject, RpcError> {
    let mut next = SettingsObject::parse(&preset.settings).map_err(|e| {
        reasoned(
            "asset_settings_invalid",
            format!("preset {:?}: {}", preset.name, e.message),
        )
    })?;
    next.fields
        .retain(|(key, _)| !PER_FILE_KEYS.contains(&key.as_str()));
    for (key, value) in &form.fields {
        if PER_FILE_KEYS.contains(&key.as_str()) {
            next.fields.push((key.clone(), value.clone()));
        }
    }
    Ok(next)
}

/// Applies the preset `name` of the panel's importer to every source the panel edits (not
/// written until Apply).
///
/// # Errors
///
/// `import_settings_not_open`, `import_preset_not_found`, `asset_settings_invalid` (the preset
/// holds a value this importer refuses) or `import_presets_unreadable`.
pub fn apply_to_panel(panel: &mut ImportSettingsPanel, name: &str) -> Result<(), RpcError> {
    let (Some(root), Some(kind), Some(path)) = (panel.root.clone(), panel.kind, panel.path.clone()) else {
        return Err(reasoned(
            "import_settings_not_open",
            "no source is open in the Import settings panel: call editor.import_settings_open first",
        ));
    };
    let file = load(&root)?;
    let preset = file
        .find(kind.name(), name)
        .ok_or_else(|| not_found(kind, name))?;
    validate_settings(&path, &preset.settings)
        .map_err(|message| reasoned("asset_settings_invalid", message))?;
    panel.form = applied(&panel.form, preset)?;
    for other in &mut panel.others {
        other.form = applied(&other.form, preset)?;
    }
    panel.buffers.clear();
    panel.invalid.clear();
    panel.error = None;
    panel.preset_choice = Some(preset.name.clone());
    Ok(())
}

/// The presets as the agent sees them (`kind` filters by importer).
#[must_use]
pub fn list_json(file: &ImportPresetsFile, kind: Option<ImporterKind>) -> Value {
    let presets: Vec<Value> = file
        .presets
        .iter()
        .filter(|p| kind.is_none_or(|k| p.importer == k.name()))
        .map(|p| json!({ "name": p.name, "importer": p.importer, "settings": p.settings }))
        .collect();
    json!({ "path": IMPORT_PRESETS_PATH, "presets": presets })
}

/// The preset names of `kind`, in file order (the panel's and the dialog's preset combo).
#[must_use]
pub fn names(root: &Path, kind: ImporterKind) -> Vec<String> {
    load(root)
        .map(|file| {
            file.presets
                .iter()
                .filter(|p| p.importer == kind.name())
                .map(|p| p.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// UI (inside the Import settings window)
// ---------------------------------------------------------------------------------------------

/// The Presets row of the Import settings window: a preset combo with Apply preset and Delete, and
/// a name field with Save as preset. The preset names are read when the panel opens a source and
/// after each change (`panel.preset_names`), never every frame.
pub fn presets_ui(ui: &mut egui::Ui, panel: &mut ImportSettingsPanel) {
    let (Some(root), Some(kind)) = (panel.root.clone(), panel.kind) else {
        return;
    };
    let names = panel
        .preset_names
        .get_or_insert_with(|| names(&root, kind))
        .clone();
    ui.horizontal_wrapped(|ui| {
        let label = ui.label("Preset");
        let shown = panel.preset_choice.clone().unwrap_or_else(|| {
            if names.is_empty() {
                "none saved".into()
            } else {
                "pick one".into()
            }
        });
        egui::ComboBox::from_id_salt("import-settings-preset")
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for name in &names {
                    ui.selectable_value(&mut panel.preset_choice, Some(name.clone()), name.as_str());
                }
            })
            .response
            .labelled_by(label.id);
        let chosen = panel.preset_choice.clone().filter(|c| names.contains(c));
        if crate::agent_parity::action_enabled(
            ui,
            chosen.is_some(),
            &crate::agent_parity::IMPORT_PRESET_APPLY,
            None,
        )
        .on_hover_text("Replace the form with the preset's settings (each file keeps its own clips)")
        .clicked()
            && let Some(name) = &chosen
            && let Err(error) = apply_to_panel(panel, name)
        {
            panel.error = Some((code_of(&error), error.message));
        }
        if crate::agent_parity::action_enabled(
            ui,
            chosen.is_some(),
            &crate::agent_parity::IMPORT_PRESET_DELETE,
            None,
        )
        .clicked()
            && let Some(name) = &chosen
        {
            match delete(&root, kind, name) {
                Ok(()) => {
                    panel.preset_choice = None;
                    panel.preset_names = None;
                }
                Err(error) => panel.error = Some((code_of(&error), error.message)),
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        let label = ui.label("Save as");
        ui.add(
            egui::TextEdit::singleline(&mut panel.preset_name)
                .hint_text("preset name")
                .desired_width(160.0),
        )
        .labelled_by(label.id);
        let name = panel.preset_name.trim().to_string();
        if crate::agent_parity::action_enabled(
            ui,
            preset_name_problem(&name).is_none() && panel.invalid.is_empty(),
            &crate::agent_parity::IMPORT_PRESET_SAVE,
            None,
        )
        .on_hover_text("Save the form as a project preset (config/import_presets.ron)")
        .clicked()
        {
            match save(&root, kind, &name, &panel.form) {
                Ok(_) => {
                    panel.preset_choice = Some(name);
                    panel.preset_name.clear();
                    panel.preset_names = None;
                }
                Err(error) => panel.error = Some((code_of(&error), error.message)),
            }
        }
    });
}

fn code_of(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("import_presets_failed")
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// Agent method
// ---------------------------------------------------------------------------------------------

fn project_root(world: &World) -> Result<std::path::PathBuf, RpcError> {
    world
        .get_resource::<EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                "no project is open",
                "start the editor with --project <dir> (or open one in the project browser)",
            )
        })
}

fn importer_param(params: &Value) -> Result<Option<ImporterKind>, RpcError> {
    match params.get("importer").and_then(Value::as_str) {
        None => Ok(None),
        Some(name) => ImporterKind::from_name(name).map(Some).ok_or_else(|| {
            reasoned(
                "import_preset_importer_unknown",
                format!("importer {name:?} is not texture, gltf, obj, fbx or audio"),
            )
        }),
    }
}

fn name_param(params: &Value) -> Result<&str, RpcError> {
    params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`name` (the preset name) is required"))
}

/// `editor.import_presets`.
fn import_presets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or("list");
    let root = project_root(world)?;
    let importer = importer_param(params)?;
    let mut panel = world.remove_resource::<ImportSettingsPanel>().unwrap_or_default();
    let out = run_action(&root, &mut panel, action, importer, params);
    world.insert_resource(panel);
    let out = out?;
    if action == "apply" && params.get("write").and_then(Value::as_bool) == Some(true) {
        // The panel's Apply as the method runs it: writes every edited `.meta`, cooks once.
        let apply_method = crate::import_settings_panel::methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.import_settings_apply")
            .map(|(_, handler)| handler)
            .ok_or_else(|| RpcError::internal("editor.import_settings_apply is not registered"))?;
        let result = apply_method(world, &json!({}))?;
        return Ok(json!({ "applied": out, "result": result["result"], "state": result["state"] }));
    }
    Ok(out)
}

fn run_action(
    root: &Path,
    panel: &mut ImportSettingsPanel,
    action: &str,
    importer: Option<ImporterKind>,
    params: &Value,
) -> Result<Value, RpcError> {
    let panel_kind = || {
        panel.kind.filter(|_| panel.path.is_some()).ok_or_else(|| {
            reasoned(
                "import_settings_not_open",
                "no source is open in the Import settings panel: pass `importer`, or open one with editor.import_settings_open",
            )
        })
    };
    match action {
        "list" => Ok(list_json(&load(root)?, importer)),
        "save" => {
            let name = name_param(params)?;
            let (kind, settings) = match params.get("settings").and_then(Value::as_str) {
                Some(text) => {
                    let kind = importer
                        .ok_or_else(|| RpcError::invalid_params("`importer` is required with `settings`"))?;
                    let object = SettingsObject::parse(text)
                        .map_err(|e| reasoned("asset_settings_invalid", e.message))?;
                    (kind, object)
                }
                None => {
                    let kind = panel_kind()?;
                    if let Some(key) = panel.invalid.iter().next() {
                        return Err(reasoned(
                            "asset_settings_invalid",
                            format!("`{key}` holds a value that was refused: fix it before saving a preset"),
                        ));
                    }
                    (kind, panel.form.clone())
                }
            };
            let replaced = save(root, kind, name, &settings)?;
            panel.preset_names = None;
            let mut out = list_json(&load(root)?, Some(kind));
            if let Some(object) = out.as_object_mut() {
                object.insert("saved".into(), json!(name.trim()));
                object.insert("replaced".into(), json!(replaced));
            }
            Ok(out)
        }
        "apply" => {
            let name = name_param(params)?;
            if let Some(Value::Array(list)) = params.get("paths") {
                let paths: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                if paths.len() != list.len() || paths.is_empty() {
                    return Err(RpcError::invalid_params("`paths` holds source path strings"));
                }
                panel.open_paths(root, &paths)?;
            }
            apply_to_panel(panel, name)?;
            Ok(panel.state_json())
        }
        "delete" => {
            let name = name_param(params)?;
            let kind = match importer {
                Some(kind) => kind,
                None => panel_kind()?,
            };
            delete(root, kind, name)?;
            panel.preset_names = None;
            if panel
                .preset_choice
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(name))
            {
                panel.preset_choice = None;
            }
            Ok(list_json(&load(root)?, Some(kind)))
        }
        other => Err(RpcError::invalid_params(format!(
            "`action` {other:?} is not list, save, apply or delete"
        ))),
    }
}

/// The presets' agent method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.import_presets",
            tool: "import_presets",
            description: "Named import presets per importer (config/import_presets.ron, ADR-7904). \
                action list (optionally one importer), save (the open Import settings form, or \
                settings + importer, as preset name; the per-file `clips` key is never stored), \
                apply (the preset's settings replace the form of every source the panel edits, \
                each keeping its clips; paths opens those sources first; write: true then runs \
                the panel's Apply: every .meta written, one cook) or delete.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: false,
                    description: "list (default), save, apply or delete.",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "The preset name (save, apply, delete); unique per importer ignoring case.",
                },
                ParamSchema {
                    name: "importer",
                    kind: "string",
                    required: false,
                    description: "texture, gltf, obj, fbx or audio: filters list; required by save with settings; delete defaults to the open panel's importer.",
                },
                ParamSchema {
                    name: "settings",
                    kind: "string",
                    required: false,
                    description: "save: a RON settings object to store instead of the panel's form.",
                },
                ParamSchema {
                    name: "paths",
                    kind: "array",
                    required: false,
                    description: "apply: open these sources (one importer) in the panel first.",
                },
                ParamSchema {
                    name: "write",
                    kind: "boolean",
                    required: false,
                    description: "apply: then write every edited .meta and cook once (editor.import_settings_apply).",
                },
            ],
            result: "list/save/delete: { path, presets: [{ name, importer, settings }], saved?, replaced? }; apply: the Import settings panel state (write: { applied, result, state })",
            mutating: true,
            permission: Permission::Assets,
        },
        import_presets as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .expect("png encode");
        bytes
    }

    /// A scratch project with two imported PNGs (`assets/a.png`, `assets/b.png`).
    fn project(label: &str) -> (World, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(label);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let sources: Vec<std::path::PathBuf> = ["a.png", "b.png"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::write(&path, tiny_png()).expect("png");
                path
            })
            .collect();
        crate::content::import_files(&root, "", &sources, Some("test-agent")).expect("import");
        let mut world = World::new();
        world.insert_resource(EditorProject {
            root: Some(root.clone()),
        });
        (world, root)
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut extension = regia_agent::extension::AgentExtension::default();
        for (schema, handler) in methods()
            .into_iter()
            .chain(crate::import_settings_panel::methods())
            .chain(crate::asset_ops_agent::methods())
        {
            extension.register(schema, handler);
        }
        let (_, handler) = extension
            .method(method)
            .unwrap_or_else(|| panic!("{method} is not registered"));
        handler(world, &params)
    }

    fn reason(result: Result<Value, RpcError>) -> String {
        match result {
            Ok(value) => panic!("expected an error, got {value}"),
            Err(error) => code_of(&error),
        }
    }

    fn meta_settings(root: &Path, rel: &str) -> Option<String> {
        let bytes = std::fs::read(root.join(rel)).expect("meta");
        regia_asset::MetaSidecar::from_ron(&bytes)
            .expect("parse")
            .settings
    }

    /// P-150: save the panel's form as a named preset, apply it to a selection of two sources,
    /// write once; list and delete. On the base none of this existed (no preset file, no method).
    #[test]
    fn save_apply_to_a_selection_and_delete_a_preset() {
        let (mut world, root) = project("import-presets-flow");
        call(
            &mut world,
            "editor.import_settings_open",
            json!({ "path": "assets/a.png" }),
        )
        .expect("open");
        call(
            &mut world,
            "editor.import_settings_edit",
            json!({ "field": "max_size", "value": 64 }),
        )
        .expect("edit");
        let saved = call(
            &mut world,
            "editor.import_presets",
            json!({ "action": "save", "name": "Tiny" }),
        )
        .expect("save");
        assert_eq!(saved["saved"], "Tiny");
        assert_eq!(saved["replaced"], false);
        assert_eq!(saved["presets"][0]["settings"], "(max_size: Some(64))");
        let file = std::fs::read_to_string(root.join(IMPORT_PRESETS_PATH)).expect("presets file");
        ImportPresetsFile::from_ron(&file).expect("the written file parses");

        // Apply to both sources and write once.
        let out = call(
            &mut world,
            "editor.import_presets",
            json!({ "action": "apply", "name": "tiny", "paths": ["assets/a.png", "assets/b.png"], "write": true }),
        )
        .expect("apply");
        assert_eq!(out["state"]["dirty"], false, "{out}");
        assert_eq!(out["state"]["recooks"], 1, "one write cooks once: {out}");
        for rel in ["assets/a.png.meta", "assets/b.png.meta"] {
            assert_eq!(
                meta_settings(&root, rel).as_deref(),
                Some("(max_size: Some(64))"),
                "{rel}"
            );
        }

        // List filters by importer; an unknown preset and importer are refused by code.
        let list = call(&mut world, "editor.import_presets", json!({ "importer": "gltf" })).expect("list");
        assert_eq!(list["presets"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_presets",
                json!({ "action": "apply", "name": "nope" })
            )),
            "import_preset_not_found"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_presets",
                json!({ "importer": "mesh" })
            )),
            "import_preset_importer_unknown"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_presets",
                json!({ "action": "save", "name": "  " })
            )),
            "import_preset_name_invalid"
        );
        assert_eq!(
            reason(call(
                &mut world,
                "editor.import_presets",
                json!({ "action": "save", "name": "Huge", "importer": "texture", "settings": "(max_size: Some(0))" })
            )),
            "asset_settings_invalid"
        );
        call(
            &mut world,
            "editor.import_presets",
            json!({ "action": "delete", "name": "TINY", "importer": "texture" }),
        )
        .expect("delete");
        let list = call(&mut world, "editor.import_presets", json!({})).expect("list");
        assert_eq!(list["presets"].as_array().map(Vec::len), Some(0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_preset_keeps_each_files_clips_and_never_stores_them() {
        let form =
            SettingsObject::parse("(scale: 2.0, clips: [(animation: \"a\", skip: true)])").expect("form");
        assert_eq!(shareable(&form).to_ron(), "(scale: 2.0)");
        let preset = ImportPreset {
            name: "p".into(),
            importer: "gltf".into(),
            settings: "(meshlets: true, clips: [(animation: \"other\")])".into(),
        };
        let next = applied(&form, &preset).expect("apply");
        assert_eq!(
            next.to_ron(),
            "(meshlets: true, clips: [(animation: \"a\", skip: true)])"
        );
    }

    #[test]
    fn an_unreadable_presets_file_is_reported_and_never_overwritten() {
        let root = regia_test_utils::unique_temp_dir("import-presets-unreadable");
        std::fs::create_dir_all(root.join("config")).expect("config");
        std::fs::write(root.join(IMPORT_PRESETS_PATH), "(format_version: 1, presets: [").expect("bad file");
        let code =
            save(&root, ImporterKind::Texture, "x", &SettingsObject::default()).map_err(|e| code_of(&e));
        assert_eq!(code, Err("import_presets_unreadable".into()));
        assert_eq!(
            std::fs::read_to_string(root.join(IMPORT_PRESETS_PATH)).expect("file"),
            "(format_version: 1, presets: ["
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The keyboard path (accessibility bar): Tab reaches Save as preset (named by its label) and
    /// Enter presses it, writing the preset.
    #[test]
    fn the_presets_row_saves_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let (_world, root) = project("import-presets-keys");
        let mut panel = ImportSettingsPanel::default();
        panel.open_path(&root, "assets/a.png").expect("open");
        panel.edit("max_size", &json!(32)).expect("edit");
        panel.preset_name = "Keys".into();
        let ctx = egui::Context::default();
        let size = [800.0, 600.0];
        {
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| presets_ui(ui, &mut panel));
            };
            frame(&ctx, size, vec![], &mut draw);
            tab_to(&ctx, size, "Save as preset", false, 20, &mut draw);
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                &mut draw,
            );
            frame(&ctx, size, vec![], &mut draw);
        }
        assert!(
            load(&root).expect("load").find("texture", "Keys").is_some(),
            "Enter on Save as preset saves it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
