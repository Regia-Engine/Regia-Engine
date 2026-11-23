// SPDX-License-Identifier: MPL-2.0
//! The Localization panel's Compile (ADR-7855, `book/agents/editor-localization.md`): validate the project's
//! Fluent files and write the shipping bundles (`loc/<locale>.ftl` and `loc/index.ron`) with the library call
//! `regia loc compile` and `regia package` run
//! (`regia_asset_pipeline::loc_exchange::compile_bundles`), so the bytes are the same. The findings come
//! back per file and line; the panel shows each as a link that opens the file at that line.

use std::path::PathBuf;

use regia_asset_pipeline::loc_exchange::{self, CompiledBundles};
use regia_ecs::prelude::*;
use regia_ui::loc_check::Finding;

use super::model::err;
use crate::data_panels::PanelError;

/// One validation finding of a compile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileFinding {
    /// The validator's code (`loc_parse_error`, `loc_variable_mismatch`, ...).
    pub code: String,
    /// `error`, `warning` or `info`.
    pub severity: &'static str,
    /// The locale.
    pub locale: String,
    /// The message key, when the finding is about one.
    pub key: Option<String>,
    /// The file, project-relative.
    pub file: Option<String>,
    /// The 1-based line in `file`.
    pub line: Option<u32>,
    /// A sentence for people.
    pub message: String,
}

impl CompileFinding {
    fn of(f: &Finding) -> Self {
        Self {
            code: f.code.to_owned(),
            severity: f.severity.as_str(),
            locale: f.locale.clone(),
            key: f.key.clone(),
            file: f.file.clone(),
            line: f.line,
            message: f.message.clone(),
        }
    }

    /// `file:line` (or just `file`), the text a link shows.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        Some(match self.line {
            Some(line) => format!("{file}:{line}"),
            None => file.clone(),
        })
    }
}

/// What a compile did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileOutcome {
    /// Validation passed and the bundles were written.
    pub ok: bool,
    /// The bundle directory (`<out>/loc`), when written.
    pub dir: Option<PathBuf>,
    /// The shipped locales, when written.
    pub locales: Vec<String>,
    /// Errors in the findings.
    pub errors: usize,
    /// Warnings in the findings.
    pub warnings: usize,
    /// Every finding, by locale, key and code.
    pub findings: Vec<CompileFinding>,
    /// Whether warnings failed the validation.
    pub strict: bool,
}

impl CompileOutcome {
    pub(super) fn of(done: &CompiledBundles, strict: bool) -> Self {
        use regia_ui::loc_check::Severity;
        Self {
            ok: done.written.is_some(),
            dir: done.written.as_ref().map(|(dir, _)| dir.clone()),
            locales: done.written.as_ref().map(|(_, l)| l.clone()).unwrap_or_default(),
            errors: done.report.count(Severity::Error),
            warnings: done.report.count(Severity::Warning),
            findings: done.report.findings.iter().map(CompileFinding::of).collect(),
            strict,
        }
    }
}

/// Validates the project's locales and, when they pass, writes the shipping bundles to `<out>/loc/`
/// (`out`: `<project>/build`, as `regia loc compile`). `locales` picks the locales to ship (empty: those of
/// `localization.ron`, else every locale); their fallback chains and the source locale ship too. `strict`
/// makes warnings fail the validation. A failed validation writes nothing and is a result, not an error.
///
/// # Errors
///
/// `loc_panel_no_project`, `loc_panel_compile_failed` (the files cannot be read, the source locale has no
/// files, or a bundle cannot be written).
pub fn compile_project(
    world: &mut World,
    locales: &[String],
    strict: bool,
    out: Option<PathBuf>,
) -> Result<CompileOutcome, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_compile").entered();
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let out = out.unwrap_or_else(|| root.join("build"));
    let result = loc_exchange::compile_bundles(&root, &out, locales, strict)
        .map(|done| CompileOutcome::of(&done, strict))
        .map_err(|e| err("compile_failed", format!("{e:#}")));
    let mut panel = world.remove_resource::<super::LocPanel>().unwrap_or_default();
    match &result {
        Ok(outcome) => {
            panel.last_compile = Some(outcome.clone());
            panel.error = None;
        }
        Err(e) => panel.error = Some(e.clone()),
    }
    world.insert_resource(panel);
    result
}
