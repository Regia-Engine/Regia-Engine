// SPDX-License-Identifier: MPL-2.0
//! The Theme panel's agent-protocol surface (`book/agents/ui-theme-panel.md`): open a UI theme or
//! a `.theme.ron` file as the draft, read its tokens, set and reset tokens, lint and save it.
//! The panel's model is [`ThemePanel`]; the same resource backs the egui panel, so a human and an
//! agent see one draft. Nothing here changes a running game's theme.

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::theme_file::Density;
use serde_json::{Map, Value, json};

use crate::theme_panel::{GROUPS, ImportItem, LintFinding, PanelError, StyleSelection, ThemePanel};
use crate::viewport_agent::Handler;

/// The `density` parameter shared by the open and set methods.
const DENSITY: ParamSchema = ParamSchema {
    name: "density",
    kind: "string",
    required: false,
    description: "comfortable or compact: the density rows and lint resolve for.",
};

/// Default cap on token rows in `editor.theme_panel_state`.
const DEFAULT_LIMIT: usize = 200;

fn rpc(e: PanelError) -> RpcError {
    let (code, hint) = match e.code {
        "no_draft" => (NOT_FOUND, "call editor.theme_panel_open first"),
        "unknown_theme" => (NOT_FOUND, "editor.theme_panel_state lists `themes`"),
        "io_error" => (INTERNAL_ERROR, "check the path and its permissions"),
        "builtin_name" => (
            INVALID_PARAMS,
            "rename the draft with editor.theme_panel_set { rename } before saving",
        ),
        _ => (
            INVALID_PARAMS,
            "the message names the token key path; nothing was changed",
        ),
    };
    RpcError::new(code, &format!("theme_panel_{}", e.code), e.message, hint)
}

fn density_param(params: &Value) -> Result<Option<Density>, RpcError> {
    match params.get("density").and_then(Value::as_str) {
        None => Ok(None),
        Some("comfortable") => Ok(Some(Density::Comfortable)),
        Some("compact") => Ok(Some(Density::Compact)),
        Some(other) => Err(RpcError::invalid_params(format!(
            "`density` is `comfortable` or `compact`, not `{other}`"
        ))),
    }
}

/// A path from the caller: relative to the project root when a project is open, never with `..`.
fn resolve_path(world: &World, text: &str) -> Result<PathBuf, RpcError> {
    // Absolute paths under the project or the temp dir only (ADR-7388); relative ones join the project
    // (ADR-1816: any absolute path used to be read and written).
    crate::agent_ext::document_request_path(world, "path", text)
}

fn finding_json(f: &LintFinding) -> Value {
    json!({ "code": f.code, "severity": f.severity.name(), "key": f.key, "message": f.message })
}

fn summary(panel: &ThemePanel) -> Value {
    let draft = panel.draft();
    json!({
        "open": draft.is_some(),
        "name": draft.map(|d| d.name.clone()),
        "extends": draft.and_then(|d| d.extends.clone()),
        "density": panel.density().key(),
        "dirty": panel.dirty(),
        "path": panel.path().map(|p| p.display().to_string()),
        "revision": panel.revision(),
        "error": panel.error,
        "themes": panel.theme_names(),
    })
}

/// `editor.theme_panel_state`.
fn theme_panel_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let filter = params.get("filter").and_then(Value::as_str).unwrap_or_default();
    let group = params.get("group").and_then(Value::as_str);
    if let Some(g) = group
        && !GROUPS.contains(&g)
    {
        return Err(RpcError::invalid_params(format!(
            "`group` is one of {}",
            GROUPS.join(", ")
        )));
    }
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |n| usize::try_from(n).unwrap_or(DEFAULT_LIMIT));
    let panel = world.get_resource_or_insert_with(ThemePanel::default);
    let mut out = summary(&panel);
    if panel.draft().is_some() {
        let rows = panel.rows(filter).map_err(rpc)?;
        let mut counts = Map::new();
        for g in GROUPS {
            let n = rows.iter().filter(|r| r.group == g).count();
            if n > 0 {
                counts.insert(g.to_owned(), json!(n));
            }
        }
        let matching: Vec<_> = rows
            .iter()
            .filter(|r| group.is_none_or(|g| r.group == g))
            .collect();
        let total = matching.len();
        let tokens: Vec<Value> = matching
            .into_iter()
            .take(limit)
            .map(|r| {
                json!({
                    "key": r.key,
                    "group": r.group,
                    "kind": r.kind,
                    "value": r.value,
                    "raw": r.raw,
                    "inherited": r.raw.is_none(),
                })
            })
            .collect();
        out["groups"] = Value::Object(counts);
        out["total"] = json!(total);
        out["truncated"] = json!(total > tokens.len());
        out["tokens"] = Value::Array(tokens);
        let resolved = panel.resolved().map_err(rpc)?;
        out["types"] = json!(resolved.types);
        out["variations"] = json!(resolved.variations);
        out["selection"] = json!(panel.selection());
        out["chain"] = json!(panel.style_chain(&panel.selection().type_name).map_err(rpc)?);
        out["properties"] = json!(panel.style_properties(panel.selection()).map_err(rpc)?);
    }
    let gallery = world.get_resource_or_insert_with(crate::theme_gallery::ThemeGallery::default);
    out["gallery"] = json!(gallery.pages());
    out["preview"] = gallery.state();
    if let Some(preview) = params.get("preview") {
        let preview = preview
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("preview is boolean"))?;
        if preview {
            let device = crate::ui_designer_gpu::preview_device(world).map_err(|e| {
                rpc(PanelError {
                    code: "preview_failed",
                    message: e.to_string(),
                })
            })?;
            let gallery = world.resource::<crate::theme_gallery::ThemeGallery>();
            let panel = world.resource::<ThemePanel>();
            let (frame, picks) = gallery.snapshot(panel, &device).map_err(rpc)?;
            out["preview"]["picks"] = json!(picks);
            out["preview"]["width"] = json!(frame.width);
            out["preview"]["height"] = json!(frame.height);
            out["preview"]["mime"] = json!("image/png");
            out["preview"]["png_base64"] = json!(
                crate::render::viewport_png_base64(&frame.to_color_image()).map_err(RpcError::internal)?
            );
        }
    }
    Ok(out)
}

/// `editor.theme_panel_open`.
fn theme_panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let density = density_param(params)?;
    let given = ["theme", "path", "new"]
        .iter()
        .filter(|k| params.get(**k).is_some())
        .count();
    if given != 1 {
        return Err(RpcError::invalid_params(
            "pass exactly one of `theme`, `path` and `new`",
        ));
    }
    let path = match params.get("path") {
        Some(v) => {
            let text = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`path` is a string"))?;
            Some(resolve_path(world, text)?)
        }
        None => None,
    };
    let mut panel = world.get_resource_or_insert_with(ThemePanel::default);
    if let Some(d) = density {
        panel.set_density(d);
    }
    if let Some(name) = params.get("theme").and_then(Value::as_str) {
        panel.open(name).map_err(rpc)?;
    } else if let Some(path) = path {
        panel.open_path(&path).map_err(rpc)?;
    } else if let Some(new) = params.get("new") {
        let name = new.get("name").and_then(Value::as_str);
        let parent = new.get("extends").and_then(Value::as_str);
        let (Some(name), Some(parent)) = (name, parent) else {
            return Err(RpcError::invalid_params("`new` is { name, extends }"));
        };
        panel.new_child(name, parent).map_err(rpc)?;
    } else {
        return Err(RpcError::invalid_params(
            "`theme` is a name, `path` a string and `new` an object",
        ));
    }
    Ok(summary(&panel))
}

/// A JSON token value as the text `ThemePanel::set_token` reads.
fn token_text(key: &str, value: &Value) -> Result<String, RpcError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(RpcError::invalid_params(format!(
            "`tokens.{key}` is a string, a number, or null to reset"
        ))),
    }
}

/// `editor.theme_panel_set`.
fn theme_panel_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(document) = params.get("preview_document") {
        if params.as_object().is_none_or(|p| p.len() != 1) {
            return Err(RpcError::invalid_params(
                "preview_document must be the only parameter",
            ));
        }
        let path = resolve_path(
            world,
            document
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("preview_document is a path string"))?,
        )?;
        world
            .get_resource_or_insert_with(crate::theme_gallery::ThemeGallery::default)
            .add_document(&path)
            .map_err(rpc)?;
        return theme_panel_state(world, &json!({}));
    }
    if let Some(page) = params.get("preview_page") {
        if params.as_object().is_none_or(|p| p.len() != 1) {
            return Err(RpcError::invalid_params(
                "preview_page must be the only parameter",
            ));
        }
        world
            .get_resource_or_insert_with(crate::theme_gallery::ThemeGallery::default)
            .select_page(
                page.as_str()
                    .ok_or_else(|| RpcError::invalid_params("preview_page is a string"))?,
            )
            .map_err(rpc)?;
        return theme_panel_state(world, &json!({}));
    }
    let density = density_param(params)?;
    let mut panel = world.get_resource_or_insert_with(ThemePanel::default);
    // Preserve the existing density-only behavior, including when there is no draft.
    if let Some(d) = density {
        panel.set_density(d);
    }
    if panel.draft().is_none() {
        return Err(rpc(PanelError {
            code: "no_draft",
            message: "open a theme first".into(),
        }));
    }
    let mut next = panel.clone();
    let mut changed = Vec::new();
    if let Some(history) = params.get("history") {
        if params.as_object().is_none_or(|p| p.len() != 1) {
            return Err(RpcError::invalid_params("history must be the only parameter"));
        }
        match history.as_str() {
            Some("undo") => next.history(false).map_err(rpc)?,
            Some("redo") => next.history(true).map_err(rpc)?,
            _ => return Err(RpcError::invalid_params("history is undo or redo")),
        }
        changed.push("history".into());
    } else {
        let mut patch = json!({});
        for field in ["tokens", "types", "variations"] {
            if let Some(value) = params.get(field) {
                if !value.is_object() {
                    return Err(RpcError::invalid_params(format!("{field} must be an object")));
                }
                patch[field] = value.clone();
                if field == "tokens" {
                    for (key, value) in value.as_object().into_iter().flatten() {
                        if value.is_null() && next.draft().is_none_or(|d| !d.tokens.contains_key(key)) {
                            return Err(rpc(PanelError {
                                code: "unknown_token",
                                message: format!("the draft does not set `{key}`"),
                            }));
                        }
                        if !value.is_null() {
                            token_text(key, value).map_err(|e| {
                                rpc(PanelError {
                                    code: "invalid_token",
                                    message: e.message,
                                })
                            })?;
                        }
                        changed.push(key.clone());
                    }
                } else {
                    changed.push(field.into());
                }
            }
        }
        let rename = params
            .get("rename")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| RpcError::invalid_params("rename is a string"))
            })
            .transpose()?;
        if patch.as_object().is_some_and(|p| !p.is_empty()) || rename.is_some() {
            next.edit(&patch, rename).map_err(rpc)?;
        }
        if let Some(selection) = params.get("selection") {
            let selection: StyleSelection = serde_json::from_value(selection.clone())
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
            next.select_style(selection).map_err(rpc)?;
            changed.push("selection".into());
        }
        if let Some(selection) = params.get("apply_all_states") {
            // Combining two draft operations would create two undo steps; require a separate call.
            if params.as_object().is_none_or(|p| p.len() != 1) {
                return Err(RpcError::invalid_params(
                    "apply_all_states must be the only parameter",
                ));
            }
            let selection: StyleSelection = serde_json::from_value(selection.clone())
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
            next.apply_all_states(&selection).map_err(rpc)?;
            changed.push("apply_all_states".into());
        }
        if changed.is_empty() && rename.is_none() && density.is_none() {
            return Err(RpcError::invalid_params(
                "pass tokens, types, variations, rename, density, selection, apply_all_states, or history",
            ));
        }
    }
    let findings = next.lint().map_err(rpc)?;
    *panel = next;
    let mut out = summary(&panel);
    out["changed"] = json!(changed);
    out["warnings"] = Value::Array(findings.iter().map(finding_json).collect());
    Ok(out)
}

/// `editor.theme_panel_import`: selectors are validated before any draft mutation.
fn theme_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from = params
        .get("from")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("from is a theme name, file path, or editor"))?;
    let items: Vec<ImportItem> = serde_json::from_value(
        params
            .get("items")
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("items is an array of selectors"))?,
    )
    .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let with_values = params
        .get("with_values")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("with_values is boolean"))
        })
        .transpose()?
        .unwrap_or(true);
    let editor =
        (from == "editor").then(|| crate::theme_panel::editor_import_source(crate::theme::tokens(world)));
    let known = world
        .get_resource_or_insert_with(ThemePanel::default)
        .theme_names()
        .iter()
        .any(|s| s == from);
    let source_path = if !known && editor.is_none() {
        Some(resolve_path(world, from)?)
    } else {
        None
    };
    let mut panel = world.resource_mut::<ThemePanel>();
    let source = match editor {
        Some(source) => source,
        None => panel
            .import_source(source_path.as_ref().and_then(|p| p.to_str()).unwrap_or(from))
            .map_err(rpc)?,
    };
    let copied = panel.import_items(&source, &items, with_values).map_err(rpc)?;
    let mut out = summary(&panel);
    out["copied"] = json!(copied);
    Ok(out)
}

/// `editor.theme_panel_lint`.
fn theme_panel_lint(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let panel = world.get_resource_or_insert_with(ThemePanel::default);
    let findings = panel.lint().map_err(rpc)?;
    Ok(json!({
        "name": panel.draft().map(|d| d.name.clone()),
        "density": panel.density().key(),
        "clean": findings.is_empty(),
        "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
    }))
}

/// `editor.theme_panel_save`.
fn theme_panel_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path").and_then(Value::as_str) {
        Some(text) => Some(resolve_path(world, text)?),
        None => None,
    };
    let mut panel = world.get_resource_or_insert_with(ThemePanel::default);
    let Some(target) = path.or_else(|| panel.path().map(Path::to_path_buf)) else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "theme_panel_no_path",
            "the draft has no file yet",
            "pass `path`, e.g. assets/ui/themes/my-game.theme.ron",
        ));
    };
    panel.save_to(&target).map_err(rpc)?;
    let mut out = summary(&panel);
    out["saved"] = json!(target.display().to_string());
    Ok(out)
}

/// The Theme panel methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.theme_panel_state",
                tool: "theme_panel_state",
                description: "Read the Theme panel: which UI theme is the draft, whether it has unsaved \
                    changes, the theme names that can be opened, and the draft's tokens (resolved value, \
                    own expression, inherited or not).",
                params: &[
                    ParamSchema {
                        name: "preview",
                        kind: "boolean",
                        required: false,
                        description: "Render the current gallery/document page as preview.png_base64 with picks; requires a render device.",
                    },
                    ParamSchema {
                        name: "filter",
                        kind: "string",
                        required: false,
                        description: "Only keys containing this text.",
                    },
                    ParamSchema {
                        name: "group",
                        kind: "string",
                        required: false,
                        description: "Color, Spacing, Shape, Type, Motion, Size or Other.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "number",
                        required: false,
                        description: "Most token rows returned (default 200); `truncated` says more exist.",
                    },
                ],
                result: "{ open, name, extends, density, dirty, path, revision, error, themes, groups, total, truncated, tokens: [{ key, group, kind, value, raw, inherited }] }",
                mutating: false,
                permission: Permission::Read,
            },
            theme_panel_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_panel_open",
                tool: "theme_panel_open",
                description: "Open a UI theme as the Theme panel's draft: a loaded theme by name, a \
                    `.theme.ron` file, or a new empty child of a theme. Replaces the current draft.",
                params: &[
                    ParamSchema {
                        name: "theme",
                        kind: "string",
                        required: false,
                        description: "A theme name, e.g. regia-dark.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "A `.theme.ron` file, relative to the project root.",
                    },
                    ParamSchema {
                        name: "new",
                        kind: "object",
                        required: false,
                        description: "{ name, extends }: a new child theme that inherits every token.",
                    },
                    DENSITY,
                ],
                result: "{ open, name, extends, density, dirty, path, revision, error, themes }",
                mutating: true,
                permission: Permission::Assets,
            },
            theme_panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_panel_set",
                tool: "theme_panel_set",
                description: "Edit the draft: set tokens (a number or an expression such as `#e8793a`, \
                    `80ms`, `@color.accent`, `lighten(@color.accent, 0.06)`), reset a token with null, \
                    rename the draft. All or nothing: a bad value changes nothing. Lint findings come back \
                    as `warnings`.",
                params: &[
                    ParamSchema {
                        name: "tokens",
                        kind: "object",
                        required: false,
                        description: "{ key: value | null }; null removes the draft's own value.",
                    },
                    ParamSchema {
                        name: "rename",
                        kind: "string",
                        required: false,
                        description: "A new theme name (needed before saving a copy of a built-in).",
                    },
                    ParamSchema {
                        name: "types",
                        kind: "object",
                        required: false,
                        description: "Type definitions: state/property patches; null resets a definition or property.",
                    },
                    ParamSchema {
                        name: "variations",
                        kind: "object",
                        required: false,
                        description: "Variation definitions including extends and state/property patches.",
                    },
                    ParamSchema {
                        name: "selection",
                        kind: "object",
                        required: false,
                        description: "{ type, state }: select a style without editing the file.",
                    },
                    ParamSchema {
                        name: "apply_all_states",
                        kind: "object",
                        required: false,
                        description: "{ type, state }: copy this effective style to all states, one undo step; sole parameter.",
                    },
                    ParamSchema {
                        name: "history",
                        kind: "string",
                        required: false,
                        description: "undo or redo; sole parameter.",
                    },
                    ParamSchema {
                        name: "preview_document",
                        kind: "string",
                        required: false,
                        description: "Add/reload a .ui.ron preview page; sole parameter.",
                    },
                    ParamSchema {
                        name: "preview_page",
                        kind: "string",
                        required: false,
                        description: "Select a gallery or loaded document page; sole parameter.",
                    },
                    DENSITY,
                ],
                result: "{ ...state summary, changed: [keys], warnings: [lint findings] }",
                mutating: true,
                permission: Permission::EditScene,
            },
            theme_panel_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_panel_import",
                tool: "theme_panel_import",
                description: "Import selected theme properties or tokens as one undo step. Expressions are resolved against the source; only selected keys are copied.",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "Loaded/built-in theme, theme file path, or editor for the editor palette.",
                    },
                    ParamSchema {
                        name: "items",
                        kind: "array",
                        required: true,
                        description: "[{ type, state?, kind? }]; type may also be tokens or fonts; kind defaults to all.",
                    },
                    ParamSchema {
                        name: "with_values",
                        kind: "boolean",
                        required: false,
                        description: "Default true. False retains destination values or uses built-in defaults; missing defaults reject the transaction.",
                    },
                ],
                result: "{ ...state summary, copied: [property paths] }",
                mutating: true,
                permission: Permission::Assets,
            },
            theme_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_panel_lint",
                tool: "theme_panel_lint",
                description: "Lint the draft: contrast pairs against their WCAG requirement, touch target \
                    size, tokens the built-in theme does not define, and resolve errors.",
                params: &[],
                result: "{ name, density, clean, findings: [{ code, severity, key, message }] }",
                mutating: false,
                permission: Permission::Read,
            },
            theme_panel_lint as Handler,
        ),
        (
            MethodSchema {
                name: "editor.theme_panel_save",
                tool: "theme_panel_save",
                description: "Write the draft to a `.theme.ron` file (temp file and rename). Built-in \
                    theme names are refused: rename the draft first.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Relative to the project root. Default: the file the draft came from.",
                }],
                result: "{ ...state summary, saved }",
                mutating: true,
                permission: Permission::Assets,
            },
            theme_panel_save as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> regia_app::App {
        regia_app::App::new()
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn parity_variation_panel_authors_runtime_layers() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({"new":{"name":"parity","extends":"regia-dark"}})).expect("open");
        theme_panel_set(
            w,
            &json!({
                "variations":{"Button.danger":{"extends":"Button","props":{"background":"#ca1234"}}}
            }),
        )
        .expect("author variation through panel");
        let panel = w.resource::<ThemePanel>();
        let draft = panel.draft().expect("draft");
        assert_eq!(
            draft.variations["Button.danger"].extends.as_deref(),
            Some("Button")
        );
        assert_eq!(
            draft.variations["Button.danger"].props["background"],
            regia_ui::theme_file::PropValue::Text("#ca1234".into())
        );
    }

    #[test]
    fn parity_import_copies_only_one_types_colors_and_undoes_once() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({"new":{"name":"imported","extends":"regia-dark"}})).expect("open");
        let before = w.resource::<ThemePanel>().to_ron().expect("before");
        let (_, import) = methods()
            .into_iter()
            .find(|(s, _)| s.name == "editor.theme_panel_import")
            .expect("editor.theme_panel_import is registered");
        import(
            w,
            &json!({"from":"regia-light","items":[{"type":"Button","state":"normal","kind":"colors"}]}),
        )
        .expect("import");
        let draft = w.resource::<ThemePanel>().draft().expect("draft");
        assert_eq!(
            draft.types.keys().map(String::as_str).collect::<Vec<_>>(),
            ["Button"]
        );
        assert_eq!(
            draft.types["Button"]
                .props
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["background", "border_color", "text_color"]
        );
        assert!(draft.tokens.is_empty());
        theme_panel_set(w, &json!({"history":"undo"})).expect("undo import");
        assert_eq!(w.resource::<ThemePanel>().to_ron().expect("after undo"), before);
    }

    #[test]
    fn parity_gallery_exposes_widget_state_selection() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({"theme":"regia-dark"})).expect("open");
        let state = theme_panel_state(w, &json!({})).expect("state");
        assert!(
            state["gallery"].is_array(),
            "live gallery pages have an agent surface"
        );
        theme_panel_set(w, &json!({"selection":{"type":"Button","state":"pressed"}})).expect("pick style");
        let state = theme_panel_state(w, &json!({})).expect("state");
        assert_eq!(state["selection"], json!({"type":"Button","state":"pressed"}));
    }

    #[test]
    fn parity_errors_are_stable_and_rejections_preserve_the_draft() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({"new":{"name":"errors","extends":"regia-dark"}})).expect("open");
        let before = w.resource::<ThemePanel>().to_ron().expect("before");
        for (params, expected) in [
            (
                json!({"selection":{"type":"Button","state":"unknown"}}),
                "theme_panel_invalid_state",
            ),
            (
                json!({"selection":{"type":"Missing","state":"normal"}}),
                "theme_panel_unknown_type",
            ),
            (json!({"history":"undo"}), "theme_panel_empty_history"),
            (json!({"preview_page":"Missing"}), "theme_panel_unknown_page"),
        ] {
            assert_eq!(code(&theme_panel_set(w, &params).expect_err("reject")), expected);
            assert_eq!(w.resource::<ThemePanel>().to_ron().expect("after"), before);
        }
        let error = theme_import(
            w,
            &json!({"from":"regia-dark","items":[{"type":"Button","kind":"unknown"}]}),
        )
        .expect_err("bad kind");
        assert_eq!(code(&error), "theme_panel_invalid_import");
        assert_eq!(w.resource::<ThemePanel>().to_ron().expect("after import"), before);
    }

    #[test]
    fn every_method_has_a_schema_and_valid_error_codes() {
        let methods = methods();
        assert_eq!(methods.len(), 6);
        for (schema, _) in &methods {
            assert!(schema.name.starts_with("editor.theme_panel_"));
            assert_eq!(schema.mutating, schema.permission != Permission::Read);
        }
        for c in [
            "theme_panel_no_draft",
            "theme_panel_unknown_theme",
            "theme_panel_invalid_token",
            "theme_panel_builtin_name",
            "theme_panel_io_error",
            "theme_panel_parse_error",
            "theme_panel_name_taken",
            "theme_panel_invalid_name",
            "theme_panel_unknown_token",
            "theme_panel_no_path",
        ] {
            assert!(
                c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{c}"
            );
        }
    }

    #[test]
    fn open_edit_lint_state_flow() {
        let mut app = world();
        let w = app.world_mut();
        let empty = theme_panel_state(w, &json!({})).expect("state");
        assert_eq!(empty["open"], false);
        assert!(
            empty["themes"]
                .as_array()
                .expect("themes")
                .iter()
                .any(|t| t == "regia-dark")
        );

        let opened = theme_panel_open(
            w,
            &json!({ "new": { "name": "my-game", "extends": "regia-dark" } }),
        )
        .expect("open");
        assert_eq!(opened["name"], "my-game");
        assert_eq!(opened["extends"], "regia-dark");

        let set = theme_panel_set(
            w,
            &json!({ "tokens": { "color.accent": "#e8793a", "radius.md": 3 } }),
        )
        .expect("set");
        assert_eq!(set["changed"], json!(["color.accent", "radius.md"]));
        assert_eq!(set["dirty"], true);

        let state = theme_panel_state(w, &json!({ "filter": "color.accent" })).expect("state");
        let accent = state["tokens"]
            .as_array()
            .expect("tokens")
            .iter()
            .find(|t| t["key"] == "color.accent")
            .expect("accent");
        assert_eq!(accent["value"], "#e8793a");
        assert_eq!(accent["inherited"], false);
        let hover = state["tokens"]
            .as_array()
            .expect("tokens")
            .iter()
            .find(|t| t["key"] == "color.accent-hover")
            .expect("hover");
        assert_eq!(hover["inherited"], true);

        let lint = theme_panel_lint(w, &json!({})).expect("lint");
        assert!(lint["findings"].is_array());
    }

    #[test]
    fn a_failed_batch_changes_nothing() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({ "new": { "name": "t", "extends": "regia-dark" } })).expect("open");
        let before = theme_panel_state(w, &json!({})).expect("state");
        let err = theme_panel_set(
            w,
            &json!({ "tokens": { "brand.a": 3, "color.accent": "@color.missing" } }),
        )
        .expect_err("dangling reference");
        assert_eq!(code(&err), "theme_panel_invalid_token");
        let after = theme_panel_state(w, &json!({})).expect("state");
        assert_eq!(before["revision"], after["revision"], "the revision is restored");
        assert_eq!(before["dirty"], after["dirty"]);
        let rows = after["tokens"].as_array().expect("tokens");
        assert!(
            rows.iter().all(|t| t["inherited"] == true),
            "the brand.a edit from the same batch was undone"
        );
    }

    #[test]
    fn errors_have_stable_codes() {
        let mut app = world();
        let w = app.world_mut();
        assert_eq!(
            code(&theme_panel_lint(w, &json!({})).expect_err("no draft")),
            "theme_panel_no_draft"
        );
        assert_eq!(
            code(&theme_panel_set(w, &json!({ "tokens": { "a": 1 } })).expect_err("no draft")),
            "theme_panel_no_draft"
        );
        assert_eq!(
            code(&theme_panel_open(w, &json!({ "theme": "nope" })).expect_err("unknown")),
            "theme_panel_unknown_theme"
        );
        assert!(
            theme_panel_open(w, &json!({})).is_err(),
            "needs one of theme, path, new"
        );
        assert!(theme_panel_open(w, &json!({ "theme": "regia-dark", "path": "x" })).is_err());
        assert!(theme_panel_open(w, &json!({ "theme": "regia-dark", "density": "huge" })).is_err());
        theme_panel_open(w, &json!({ "theme": "regia-dark" })).expect("open built-in");
        // An absolute path on every OS (`/tmp/...` has no drive on Windows, so it is relative there).
        let never = std::env::temp_dir().join("never-written.theme.ron");
        assert_eq!(
            code(&theme_panel_save(w, &json!({ "path": never.display().to_string() })).expect_err("builtin")),
            "theme_panel_builtin_name"
        );
        theme_panel_open(w, &json!({ "new": { "name": "c", "extends": "regia-dark" } })).expect("child");
        assert_eq!(
            code(&theme_panel_save(w, &json!({})).expect_err("no path")),
            "theme_panel_no_path"
        );
        assert!(
            theme_panel_save(w, &json!({ "path": "../escape.theme.ron" })).is_err(),
            "no `..`"
        );
        assert_eq!(
            code(&theme_panel_set(w, &json!({ "tokens": { "radius.md": true } })).expect_err("bool")),
            "theme_panel_invalid_token"
        );
        assert!(theme_panel_state(w, &json!({ "group": "Nope" })).is_err());
    }

    #[test]
    fn save_then_reopen_through_the_agent() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-agent");
        let path = dir.join("g.theme.ron");
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({ "new": { "name": "g", "extends": "regia-light" } })).expect("open");
        theme_panel_set(w, &json!({ "tokens": { "color.accent": "#e8793a" } })).expect("set");
        let saved = theme_panel_save(w, &json!({ "path": path.display().to_string() })).expect("save");
        assert_eq!(saved["dirty"], false);
        assert!(path.is_file());
        theme_panel_open(w, &json!({ "theme": "regia-dark" })).expect("switch");
        let reopened = theme_panel_open(w, &json!({ "path": path.display().to_string() })).expect("reopen");
        assert_eq!(reopened["name"], "g");
        assert_eq!(reopened["dirty"], false);
        let reset = theme_panel_set(w, &json!({ "tokens": { "color.accent": null } })).expect("reset");
        assert_eq!(reset["dirty"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_paging_reports_truncation() {
        let mut app = world();
        let w = app.world_mut();
        theme_panel_open(w, &json!({ "theme": "regia-dark" })).expect("open");
        let page = theme_panel_state(w, &json!({ "limit": 3 })).expect("state");
        assert_eq!(page["tokens"].as_array().expect("tokens").len(), 3);
        assert_eq!(page["truncated"], true);
        assert!(page["total"].as_u64().expect("total") > 3);
        let colors = theme_panel_state(w, &json!({ "group": "Color" })).expect("state");
        assert!(
            colors["tokens"]
                .as_array()
                .expect("tokens")
                .iter()
                .all(|t| t["group"] == "Color")
        );
    }
}

#[cfg(test)]
mod dispatcher_tests {
    use super::*;
    use regia_agent::protocol::Request;
    use regia_agent::server::Inbound;

    #[test]
    fn theme_panel_types_import_and_history_use_the_real_dispatcher() {
        let dir = regia_test_utils::unique_temp_dir("theme-panel-dispatch");
        let keep_evidence = std::env::var_os("REGIA_KEEP_THEME_EVIDENCE").is_some();
        let mut app = regia_app::App::new();
        app.add_plugins(regia_app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut id = 0;
        {
            let mut call = |method: &str, params: Value| {
                id += 1;
                let (reply, answer) = crossbeam_channel::bounded(1);
                inbox
                    .send(Inbound {
                        request: Request {
                            jsonrpc: "2.0".into(),
                            id: Some(json!(id)),
                            method: method.into(),
                            params,
                        },
                        reply,
                    })
                    .expect("send");
                app.update();
                let response = answer.try_recv().expect("one frame answers");
                assert!(response.error.is_none(), "{method}: {:?}", response.error);
                response.result.expect("result")
            };
            call(
                "editor.theme_panel_open",
                json!({"new":{"name":"dispatch","extends":"regia-dark"}}),
            );
            call(
                "editor.theme_panel_set",
                json!({"variations":{"Button.alert":{"extends":"Button","props":{"background":"#ca1234"}}},"selection":{"type":"Button.alert","state":"normal"}}),
            );
            let state = call("editor.theme_panel_state", json!({"preview":true}));
            assert_eq!(state["preview"]["mime"], "image/png");
            assert!(!state["preview"]["png_base64"].as_str().expect("PNG").is_empty());
            assert_eq!(
                state["preview"]["picks"].as_array().expect("picks").len(),
                crate::theme_panel::STYLE_STATES.len()
            );
            assert_eq!(state["chain"], json!(["Button", "Button.alert"]));
            assert_eq!(state["properties"]["background"], "#ca1234");
            if keep_evidence {
                let tmp = dir.join("preview.json.tmp");
                std::fs::write(
                    &tmp,
                    serde_json::to_vec_pretty(&state).expect("serialize response"),
                )
                .expect("write evidence");
                std::fs::rename(&tmp, dir.join("preview.json")).expect("publish evidence");
            }
            let imported = call(
                "editor.theme_panel_import",
                json!({"from":"regia-light","items":[{"type":"Button","state":"normal","kind":"colors"}]}),
            );
            assert_eq!(
                imported["copied"],
                json!([
                    "types.Button.props.background",
                    "types.Button.props.border_color",
                    "types.Button.props.text_color"
                ])
            );
            call("editor.theme_panel_set", json!({"history":"undo"}));
            let path = dir.join("dispatch.theme.ron");
            call("editor.theme_panel_save", json!({"path":path}));
            let bytes = std::fs::read(&path).expect("read");
            call("editor.theme_panel_open", json!({"path":path}));
            call("editor.theme_panel_save", json!({"path":path}));
            assert_eq!(bytes, std::fs::read(&path).expect("read"));
            call("editor.theme_panel_set", json!({"preview_page":"Toggle"}));
            let state = call("editor.theme_panel_state", json!({}));
            assert_eq!(state["preview"]["page"], "Toggle");
            let document = dir.join("own.ui.ron");
            let tmp = dir.join("own.ui.ron.tmp");
            std::fs::write(&tmp, r#"(version: 11, root: (children: [(widget: button, classes: ["Button.alert"], style: (size: (160, 48))), (classes: ["Button.alert"], style: (size: (160, 48)))]))"#).expect("write document");
            std::fs::rename(&tmp, &document).expect("publish document");
            call("editor.theme_panel_set", json!({"preview_document":document}));
            let own = call("editor.theme_panel_state", json!({"preview":true}));
            assert_eq!(own["preview"]["page"], document.display().to_string());
            // Runtime authoring puts the constructor's `button` class before the authored ones
            // and the resolver keeps the last class (ADR-8303), so the button widget with the
            // authored variation and the plain node both pick `Button.alert`.
            assert!(
                own["preview"]["picks"]
                    .as_array()
                    .expect("document picks")
                    .iter()
                    .filter(|pick| pick["selection"]["type"] == "Button.alert")
                    .count()
                    >= 2,
                "{}",
                own["preview"]["picks"]
            );
            assert!(
                !own["preview"]["png_base64"]
                    .as_str()
                    .expect("document PNG")
                    .is_empty()
            );
        }
        drop(app);
        if keep_evidence {
            println!("theme gallery evidence: {}", dir.display());
        } else {
            std::fs::remove_dir_all(dir).expect("cleanup");
        }
    }
}
