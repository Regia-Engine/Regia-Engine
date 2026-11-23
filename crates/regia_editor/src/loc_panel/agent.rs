// SPDX-License-Identifier: MPL-2.0
//! The Localization panel's agent methods (`book/agents/editor-localization.md`, ADR-3260):
//! `editor.loc_table`, `editor.loc_set`, `editor.loc_gather`, `editor.loc_usages`, `editor.loc_pseudo`,
//! `editor.loc_export` and `editor.loc_import` (ADR-7677), `editor.loc_compile`, `editor.loc_set_comment` and
//! `editor.preview_locale` (ADR-7855). Each calls the function the panel's buttons call.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::model::{self, Cell, Filter, Query, err};
use super::{LocPanel, with_panel};
use crate::command::UndoStack;
use crate::data_panels::{opt_bool, opt_f32, str_param};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/editor-localization.md";

fn rpc(e: &crate::data_panels::PanelError) -> RpcError {
    e.rpc(HINT)
}

fn opt_usize(params: &Value, key: &str) -> Result<Option<usize>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| rpc(&err("bad_page", format!("`{key}` must be a whole number ≥ 0")))),
    }
}

fn cell_json(cell: &Cell) -> Value {
    json!({
        "state": cell.state.as_str(),
        "text": cell.text,
        "file": cell.file,
        "line": cell.line,
        "long": cell.long,
    })
}

/// `editor.loc_table`.
fn loc_table(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let filter = match params.get("filter") {
        None | Some(Value::Null) => Filter::All,
        Some(Value::String(name)) => Filter::parse(name).map_err(|e| rpc(&e))?,
        Some(_) => return Err(rpc(&err("bad_filter", "`filter` must be a string"))),
    };
    let locale = match params.get("locale") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(RpcError::invalid_params("`locale` must be a string")),
    };
    let search = match params.get("search") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(RpcError::invalid_params("`search` must be a string")),
    };
    let long_ratio = opt_f32(params, "long_ratio")?.unwrap_or(model::DEFAULT_LONG_RATIO);
    if long_ratio < 1.0 {
        return Err(rpc(&err("bad_ratio", "`long_ratio` must be at least 1.0")));
    }
    let page = opt_usize(params, "page")?.unwrap_or(0);
    let page_size = opt_usize(params, "page_size")?.unwrap_or(model::DEFAULT_PAGE_SIZE);
    if page_size == 0 || page_size > model::MAX_PAGE_SIZE {
        return Err(rpc(&err(
            "bad_page",
            format!("`page_size` must be 1..={}", model::MAX_PAGE_SIZE),
        )));
    }
    let query = Query {
        locale: locale.clone(),
        filter,
        search,
        long_ratio,
    };
    with_panel(world, |panel, _| {
        // The panel shows what the agent asked for.
        panel.query = query.clone();
        panel.page = page;
        panel.page_size = page_size;
        Ok(())
    })
    .map_err(|e| rpc(&e))?;
    // A ratio change is applied by the next refresh.
    with_panel(world, |panel, _| {
        let table = panel
            .table()
            .ok_or_else(|| err("io_failed", "the localization files were not read"))?;
        if let Some(l) = &locale
            && !table.locales.contains(l)
        {
            return Err(err(
                "locale_not_found",
                format!("no locale `{l}`; locales: {}", table.locales.join(", ")),
            ));
        }
        let rows = table.select(&query);
        let total = rows.len();
        let pages = total.div_ceil(page_size).max(1);
        if page >= pages {
            return Err(err("bad_page", format!("page {page} is past the last page ({})", pages - 1)));
        }
        let shown: Vec<&String> = table
            .locales
            .iter()
            .filter(|l| **l == table.source || locale.as_ref().is_none_or(|q| q == *l))
            .collect();
        let rows: Vec<Value> = rows
            .iter()
            .skip(page * page_size)
            .take(page_size)
            .map(|row| {
                let cells: serde_json::Map<String, Value> = shown
                    .iter()
                    .filter_map(|l| row.cells.get(*l).map(|c| ((*l).clone(), cell_json(c))))
                    .collect();
                json!({ "key": row.key, "in_source": row.in_source, "context": row.context, "cells": cells })
            })
            .collect();
        let coverage: Vec<Value> = table
            .coverage
            .iter()
            .filter(|c| locale.as_ref().is_none_or(|l| *l == c.locale))
            .map(|c| {
                json!({
                    "locale": c.locale,
                    "messages": c.messages,
                    "translated": c.translated,
                    "untranslated": c.untranslated,
                    "placeholders": c.placeholders,
                    "stale": c.stale,
                    "percent": c.percent,
                })
            })
            .collect();
        let issues: Vec<Value> = table
            .issues
            .iter()
            .map(|i| json!({ "code": i.code, "locale": i.locale, "file": i.file, "line": i.line, "message": i.message }))
            .collect();
        Ok(json!({
            "source": table.source,
            "locales": table.locales,
            "filter": filter.as_str(),
            "locale": locale,
            "search": query.search,
            "long_ratio": table.long_ratio,
            "total": total,
            "page": page,
            "pages": pages,
            "page_size": page_size,
            "rows": rows,
            "coverage": coverage,
            "issues": issues,
        }))
    })
    .map_err(|e| rpc(&e))
}

/// `editor.loc_set`.
fn loc_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = str_param(params, "key")?.to_owned();
    let locale = str_param(params, "locale")?.to_owned();
    let text = str_param(params, "text")?.to_owned();
    let out = super::set_translation(world, &key, &locale, &text).map_err(|e| rpc(&e))?;
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    Ok(json!({
        "key": key,
        "locale": locale,
        "file": out.file,
        "created": out.created,
        "changed": out.changed,
        "warnings": out.warnings.iter().map(|(c, m)| json!({ "code": c, "message": m })).collect::<Vec<_>>(),
        "undo_depth": undo_depth,
    }))
}

/// `editor.loc_gather`.
fn loc_gather(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dry_run = opt_bool(params, "dry_run")?.unwrap_or(false);
    let out = super::run_gather(world, dry_run).map_err(|e| rpc(&e))?;
    Ok(json!({
        "files_scanned": out.files_scanned,
        "keys": out.keys,
        "bad_keys": out.bad_keys.iter().map(|(f, l, t)| json!({ "file": f, "line": l, "text": t })).collect::<Vec<_>>(),
        "files": out.files.iter().map(|(locale, path, added)| json!({ "locale": locale, "path": path, "added": added })).collect::<Vec<_>>(),
        "written": out.written,
    }))
}

/// `editor.loc_usages`.
fn loc_usages(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = str_param(params, "key")?.to_owned();
    let (usages, graph) = super::find_usages(world, &key).map_err(|e| rpc(&e))?;
    let defined_in: Vec<Value> = world
        .get_resource::<LocPanel>()
        .and_then(LocPanel::table)
        .and_then(|t| t.rows.iter().find(|r| r.key == key))
        .map(|row| {
            row.cells
                .iter()
                .filter_map(|(l, c)| {
                    c.file
                        .as_ref()
                        .map(|f| json!({ "locale": l, "file": f, "line": c.line }))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "key": key,
        "defined_in": defined_in,
        "usages": usages.iter().map(|u| json!({ "file": u.file, "line": u.line, "kind": u.kind, "via": u.via })).collect::<Vec<_>>(),
        "graph": if graph { "registry" } else { "none" },
    }))
}

/// `editor.loc_pseudo`.
fn loc_pseudo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = match opt_bool(params, "on")? {
        Some(on) => on,
        None => world.get_resource::<LocPanel>().is_some_and(|p| p.pseudo),
    };
    let has = super::set_pseudo(world, None, on);
    Ok(json!({ "pseudo": on, "viewport_localization": has }))
}

/// `editor.loc_export`.
fn loc_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let locale = str_param(params, "locale")?.to_owned();
    let format = match params.get("format") {
        None | Some(Value::Null) => regia_asset_pipeline::loc_exchange::ExchangeFormat::Po,
        Some(Value::String(name)) => super::exchange::parse_format(name).map_err(|e| rpc(&e))?,
        Some(_) => return Err(rpc(&err("bad_format", "`format` must be po or xliff"))),
    };
    let only = match params.get("only") {
        None | Some(Value::Null) => regia_ui::loc_po::ExportFilter::All,
        Some(Value::String(name)) => super::exchange::parse_filter(name).map_err(|e| rpc(&e))?,
        Some(_) => {
            return Err(rpc(&err(
                "bad_filter",
                "`only` must be all, untranslated or stale",
            )));
        }
    };
    let out = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(crate::agent_ext::document_request_path(world, "path", text)?),
        Some(_) => return Err(RpcError::invalid_params("`path` must be a string")),
    };
    let e = super::exchange::export_locale(world, &locale, format, only, out).map_err(|e| rpc(&e))?;
    Ok(json!({
        "file": e.file.display().to_string(),
        "locale": e.locale,
        "format": super::exchange::format_name(e.format),
        "only": super::exchange::filter_name(only),
        "entries": e.entries,
        "skipped_attributes": e.skipped_attributes,
        "skipped_terms": e.skipped_terms,
        "tm_pre_filled": e.tm_pre_filled,
        "tm_near": e.tm_near,
    }))
}

/// `editor.loc_import`.
fn loc_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = str_param(params, "path")?.to_owned();
    let file = crate::agent_ext::document_request_path(world, "path", &text)?;
    let locale = match params.get("locale") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(RpcError::invalid_params("`locale` must be a string")),
    };
    let dry_run = opt_bool(params, "dry_run")?.unwrap_or(false);
    let preview = super::exchange::preview_import(world, &file, locale).map_err(|e| rpc(&e))?;
    let written = if dry_run {
        false
    } else {
        let outcome = super::exchange::apply_import(world).map_err(|e| rpc(&e))?;
        let written = outcome.written;
        world.get_resource_or_insert_with(LocPanel::default).last_import = Some(outcome);
        written
    };
    let notes = |ns: &[super::exchange::ImportNote]| {
        ns.iter()
            .map(|(code, key, message)| json!({ "code": code, "key": key, "message": message }))
            .collect::<Vec<_>>()
    };
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    Ok(json!({
        "file": preview.file.display().to_string(),
        "format": super::exchange::format_name(preview.format),
        "locale": preview.locale,
        "imported": preview.imported,
        "added": preview.added,
        "changed": preview.changed,
        "missing": preview.missing,
        "skipped": notes(&preview.skipped),
        "warnings": notes(&preview.warnings),
        "refused": preview.refused.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
        "files": preview.changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
        "written": written,
        "undo_depth": undo_depth,
    }))
}

/// `editor.loc_set_comment`.
fn loc_set_comment(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = str_param(params, "key")?.to_owned();
    let text = str_param(params, "text")?.to_owned();
    let out = super::set_comment(world, &key, &text).map_err(|e| rpc(&e))?;
    // Read the file again so the answer shows what is on disk now.
    let _ = with_panel(world, |_, _| Ok(()));
    let context = world
        .get_resource::<LocPanel>()
        .and_then(LocPanel::table)
        .and_then(|t| t.rows.iter().find(|r| r.key == key))
        .map(|r| r.context.clone())
        .unwrap_or_default();
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    Ok(json!({
        "key": key,
        "file": out.file,
        "changed": out.changed,
        "context": context,
        "undo_depth": undo_depth,
    }))
}

/// `editor.loc_compile`.
fn loc_compile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let strict = opt_bool(params, "strict")?.unwrap_or(false);
    let locales: Vec<String> = match params.get("locales") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::invalid_params("`locales` must be an array of strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(RpcError::invalid_params("`locales` must be an array of strings")),
    };
    let out = match params.get("out") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(crate::agent_ext::document_request_path(world, "out", text)?),
        Some(_) => return Err(RpcError::invalid_params("`out` must be a string")),
    };
    let c = super::compile::compile_project(world, &locales, strict, out).map_err(|e| rpc(&e))?;
    Ok(json!({
        "ok": c.ok,
        "strict": c.strict,
        "dir": c.dir.as_ref().map(|d| d.display().to_string()),
        "locales": c.locales,
        "errors": c.errors,
        "warnings": c.warnings,
        "findings": c.findings.iter().map(|f| json!({
            "code": f.code, "severity": f.severity, "locale": f.locale, "key": f.key,
            "file": f.file, "line": f.line, "message": f.message,
        })).collect::<Vec<_>>(),
    }))
}

/// `editor.preview_locale`.
fn preview_locale(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let requested = match params.get("locale") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(RpcError::invalid_params("`locale` must be a string")),
    };
    // Make sure the project's locales are known before the pick is checked.
    let _ = with_panel(world, |_, _| Ok(()));
    let out = match requested.as_deref() {
        None => {
            let current = super::preview::current(world);
            super::preview::PreviewOutcome {
                locale: current,
                source: super::preview::source_locale(world),
                locales: super::preview::available_locales(world),
                edit_world: world.contains_resource::<regia_ui::localization::Localization>(),
                play_world: false,
            }
        }
        Some("" | "source") => super::preview::set_preview_locale(world, None, None).map_err(|e| rpc(&e))?,
        Some(locale) => super::preview::set_preview_locale(world, None, Some(locale)).map_err(|e| rpc(&e))?,
    };
    let shown = world
        .get_resource::<regia_ui::localization::Localization>()
        .map(|l| l.locale().to_owned());
    Ok(json!({
        "locale": out.locale,
        "source": out.source,
        "locales": out.locales,
        "edit_world": out.edit_world,
        "shown": shown,
    }))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const TABLE_PARAMS: &[ParamSchema] = &[
    param(
        "locale",
        "string",
        false,
        "Only this locale's column (and its coverage); default every locale.",
    ),
    param(
        "filter",
        "string",
        false,
        "all (default) | missing | untranslated | stale | long: rows where a shown target locale matches.",
    ),
    param(
        "search",
        "string",
        false,
        "Case-insensitive text in the key, the source text or a shown translation.",
    ),
    param(
        "long_ratio",
        "number",
        false,
        "The `long` filter's ratio of translation to source length (default 1.5, at least 1).",
    ),
    param("page", "number", false, "0-based page (default 0)."),
    param(
        "page_size",
        "number",
        false,
        "Rows per page, 1..=500 (default 50).",
    ),
];

const SET_PARAMS: &[ParamSchema] = &[
    param("key", "string", true, "The message key."),
    param(
        "locale",
        "string",
        true,
        "The locale (the source locale edits the source text).",
    ),
    param(
        "text",
        "string",
        true,
        "The new value as a Fluent pattern; several lines for a multi-line value.",
    ),
];

const GATHER_PARAMS: &[ParamSchema] = &[param(
    "dry_run",
    "boolean",
    false,
    "Report what would be written; write nothing.",
)];
const USAGES_PARAMS: &[ParamSchema] = &[param("key", "string", true, "The message key.")];
const PSEUDO_PARAMS: &[ParamSchema] = &[param(
    "on",
    "boolean",
    false,
    "On or off; omitted: report the current state.",
)];

const EXPORT_PARAMS: &[ParamSchema] = &[
    param(
        "locale",
        "string",
        true,
        "The target locale to export (not the source locale).",
    ),
    param("format", "string", false, "po (default) | xliff."),
    param("only", "string", false, "all (default) | untranslated | stale."),
    param(
        "path",
        "string",
        false,
        "The file to write, under the project (default .regia/loc/<locale>.po or .xlf).",
    ),
];

const IMPORT_PARAMS: &[ParamSchema] = &[
    param(
        "path",
        "string",
        true,
        "The returned PO or XLIFF file (the format is read from its content).",
    ),
    param(
        "locale",
        "string",
        false,
        "The locale (default: the file's Language).",
    ),
    param(
        "dry_run",
        "boolean",
        false,
        "Report what the import would add, change and still miss; write nothing.",
    ),
];

const SET_COMMENT_PARAMS: &[ParamSchema] = &[
    param(
        "key",
        "string",
        true,
        "The message key (the source locale must define it).",
    ),
    param(
        "text",
        "string",
        true,
        "The translator context, one line per line; empty removes it. Written as `# note:` lines above the message.",
    ),
];

const COMPILE_PARAMS: &[ParamSchema] = &[
    param(
        "locales",
        "array",
        false,
        "Locales to ship (default: those of localization.ron, else every locale); fallbacks and the source ship too.",
    ),
    param("strict", "boolean", false, "Warnings fail the validation too."),
    param(
        "out",
        "string",
        false,
        "Output directory under the project; bundles go to <out>/loc (default <project>/build).",
    ),
];

const PREVIEW_PARAMS: &[ParamSchema] = &[param(
    "locale",
    "string",
    false,
    "The locale to preview in the edit viewport and play; `source` (or empty) resets; omitted: report.",
)];

/// The Localization panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.loc_table",
                tool: "loc_table",
                description: "The Localization panel's key-by-locale table: each cell's state (source, translated, stale, \
                    placeholder, missing), text, file and line; per-locale coverage; filters, search and paging.",
                params: TABLE_PARAMS,
                result: "{ source, locales, filter, locale, search, long_ratio, total, page, pages, page_size, rows: [{ key, \
                    in_source, context: [line], cells: { <locale>: { state, text, file, line, long } } }], coverage: [{ locale, messages, \
                    translated, untranslated, placeholders, stale, percent }], issues: [{ code, locale, file, line, message }] }",
                mutating: false,
                permission: Permission::Read,
            },
            loc_table as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_set",
                tool: "loc_set",
                description: "Set one translation: the message's value is rewritten in the file that defines it (or \
                    appended to the locale's file named like the source's), atomically, every other byte kept; one \
                    editor undo step (editor.undo restores the bytes).",
                params: SET_PARAMS,
                result: "{ key, locale, file, created, changed, warnings: [{ code, message }], undo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_gather",
                tool: "loc_gather",
                description: "Run the gather pipeline of `regia loc gather` (UI and asset text scan, code, conversations, \
                    dialogues) and add the missing keys to each locale's gathered.ftl; one undo step.",
                params: GATHER_PARAMS,
                result: "{ files_scanned, keys, bad_keys: [{ file, line, text }], files: [{ locale, path, added }], written }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_gather as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_usages",
                tool: "loc_usages",
                description: "Where a key is used: the files that name it (UI layouts, scenes, conversations, dialogues, \
                    code) and, with a loaded asset registry, the assets that reach them through the asset graph.",
                params: USAGES_PARAMS,
                result: "{ key, defined_in: [{ locale, file, line }], usages: [{ file, line, kind, via }], graph }",
                mutating: false,
                permission: Permission::Read,
            },
            loc_usages as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_pseudo",
                tool: "loc_pseudo",
                description: "Turn pseudo-localization of the viewport's runtime Localization on or off (edit world now, \
                    the play world while the panel draws).",
                params: PSEUDO_PARAMS,
                result: "{ pseudo, viewport_localization }",
                mutating: true,
                permission: Permission::EditScene,
            },
            loc_pseudo as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_export",
                tool: "loc_export",
                description: "Write a locale's translator file, gettext PO or XLIFF 2.0, with the same library call (and \
                    the same bytes) as `regia loc export`: every source message the filter keeps, with source \
                    references and translation-memory pre-fills.",
                params: EXPORT_PARAMS,
                result: "{ file, locale, format, only, entries, skipped_attributes, skipped_terms, tm_pre_filled, tm_near }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_export as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_import",
                tool: "loc_import",
                description: "Read a returned PO or XLIFF file into the locale's imported.ftl as `regia loc import` does \
                    (same checks, same bytes), reporting first which keys it adds, changes and still misses; one \
                    editor undo step (editor.undo restores the files). A refused file writes nothing.",
                params: IMPORT_PARAMS,
                result: "{ file, format, locale, imported, added, changed, missing, skipped: [{ code, key, message }], \
                    warnings: [{ code, key, message }], refused: { code, message }, files, written, undo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_set_comment",
                tool: "loc_set_comment",
                description: "Set a key's translator context: the comment above its message in the source locale's \
                    file becomes `# note:` lines (tool markers and every other byte kept), one editor undo step. \
                    Exports carry it as a note (PO `#.`, XLIFF note). Empty text removes the context.",
                params: SET_COMMENT_PARAMS,
                result: "{ key, file, changed, context: [line], undo_depth }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_set_comment as Handler,
        ),
        (
            MethodSchema {
                name: "editor.loc_compile",
                tool: "loc_compile",
                description: "Validate the project's locales and write the shipping bundles (loc/<locale>.ftl, \
                    loc/index.ron) with the library call of `regia loc compile`: same bytes. Findings come back per \
                    file and line; a failed validation writes nothing and is a result (ok false), not an error.",
                params: COMPILE_PARAMS,
                result: "{ ok, strict, dir, locales, errors, warnings, findings: [{ code, severity, locale, key, file, \
                    line, message }] }",
                mutating: true,
                permission: Permission::Assets,
            },
            loc_compile as Handler,
        ),
        (
            MethodSchema {
                name: "editor.preview_locale",
                tool: "preview_locale",
                description: "Set (or read) the preview language of the edit viewport's UI and of play: ui.set_locale \
                    on both worlds, kept in step each frame; `source` resets. Changes no file.",
                params: PREVIEW_PARAMS,
                result: "{ locale, source, locales, edit_world, shown }",
                mutating: true,
                permission: Permission::EditScene,
            },
            preview_locale as Handler,
        ),
    ]
}
