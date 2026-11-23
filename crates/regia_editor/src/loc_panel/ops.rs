// SPDX-License-Identifier: MPL-2.0
//! The Localization panel's file operations (ADR-3260): gather, find usages and the undoable write of
//! a set of `.ftl` files through the gather tool's atomic writer (`regia_ui::loc_gather::write_planned`, the
//! writer `regia loc gather` and `regia loc import` use).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_asset_pipeline::asset_graph::DependencyGraph;
use regia_ecs::prelude::*;
use regia_ui::loc_gather::{self, Gathered, PlannedFile};

use super::model::{FileChange, LOCALIZATION_DIR, LocProject, err};
use crate::command::EditorCommand;
use crate::data_panels::PanelError;

/// Writes `text` to the project-relative `path` with the CLI's atomic writer.
fn write(root: &Path, locale: &str, path: &str, text: &str) -> Result<(), PanelError> {
    let file = PlannedFile {
        locale: locale.to_owned(),
        path: path.to_owned(),
        added: Vec::new(),
        text: text.to_owned(),
    };
    loc_gather::write_planned(root, &file).map_err(|e| err("io_failed", format!("{path}: {e}")))
}

/// The current text of `path` (`None` when the file does not exist).
fn current(root: &Path, path: &str) -> Result<Option<String>, PanelError> {
    match std::fs::read_to_string(root.join(path)) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err("io_failed", format!("{path}: {e}"))),
    }
}

/// Moves every file of `changes` from its `from` text to its `to` text. A file whose current text is not
/// `from` is left alone (someone edited it since) and reported as `loc_panel_conflict`; the other files are
/// still written.
fn transition(root: &Path, changes: &[FileChange], forward: bool) -> Result<(), PanelError> {
    let mut conflicts = Vec::new();
    for change in changes {
        let (from, to) = if forward {
            (change.before.as_deref(), Some(change.after.as_str()))
        } else {
            (Some(change.after.as_str()), change.before.as_deref())
        };
        if current(root, &change.path)?.as_deref() != from {
            conflicts.push(change.path.clone());
            continue;
        }
        match to {
            Some(text) => write(root, &change.locale, &change.path, text)?,
            None => std::fs::remove_file(root.join(&change.path))
                .map_err(|e| err("io_failed", format!("{}: {e}", change.path)))?,
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(err(
            "conflict",
            format!(
                "changed on disk since the edit, left alone: {}",
                conflicts.join(", ")
            ),
        ))
    }
}

/// One undo step of the Localization panel: a translation set or a gather, as the files it rewrote.
/// Undo writes the bytes back (and removes a file the step created); redo writes the step's bytes again.
pub struct LocFilesEdit {
    root: PathBuf,
    changes: Vec<FileChange>,
    label: String,
}

impl LocFilesEdit {
    /// Writes `changes` under `root` and returns the step to push, or the write's error. Files are written
    /// in order; when one fails, the ones before it are restored.
    ///
    /// # Errors
    ///
    /// `loc_panel_conflict` when a file changed on disk since it was read, `loc_panel_io_failed` when a
    /// write fails.
    pub fn write(root: &Path, changes: Vec<FileChange>, label: String) -> Result<Self, PanelError> {
        for (i, change) in changes.iter().enumerate() {
            if let Err(e) = transition(root, std::slice::from_ref(change), true) {
                // Leave nothing half done.
                let _ = transition(root, &changes[..i], false);
                return Err(e);
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            changes,
            label,
        })
    }

    /// The files the step rewrote.
    #[must_use]
    pub fn changes(&self) -> &[FileChange] {
        &self.changes
    }

    fn run(&self, world: &mut World, forward: bool) {
        let result = transition(&self.root, &self.changes, forward);
        if let Err(e) = &result {
            tracing::warn!(target: "regia_editor", "localization {}: {} ({})", if forward { "redo" } else { "undo" }, e.message, e.code);
        }
        let mut panel = world.get_resource_or_insert_with(super::LocPanel::default);
        panel.reload = true;
        panel.error = result.err();
    }
}

impl EditorCommand for LocFilesEdit {
    fn apply(&mut self, world: &mut World) {
        self.run(world, true);
    }

    fn revert(&mut self, world: &mut World) {
        self.run(world, false);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        format!("Localization: {}", self.label)
    }
}

/// The keys `regia loc gather` finds: the text scan of `gather.roots` (`.ron` and `.rs`), the conversations'
/// text keys (spec 56 § 6) and the dialogue lines, subtitle cues and voice names (spec 36 § 11), with the
/// same notes the CLI writes above each new message.
///
/// # Errors
///
/// `loc_panel_io_failed` when a gather root cannot be listed, `loc_panel_gather_failed` when a conversation
/// does not parse.
pub fn gather(root: &Path, project: &LocProject) -> Result<Gathered, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_gather").entered();
    let mut gathered = loc_gather::gather_project(root, &project.config.gather)
        .map_err(|e| err("io_failed", format!("gather: {e}")))?;
    let assets = root.join("assets");
    if assets.join("conversations").is_dir() {
        let library = regia_framework::conversation::ConversationLibrary::load_assets(&assets)
            .map_err(|e| err("gather_failed", format!("conversations: {e}")))?;
        for asset in library.conversations.values() {
            let rel = format!("assets/conversations/{}.conv.ron", asset.id);
            let text = std::fs::read_to_string(root.join(&rel)).unwrap_or_default();
            gathered.files_scanned += 1;
            for key in
                regia_framework::conversation::gather::gather_keys(asset, &text, &library, &project.source)
            {
                gathered.add_key(&key.key, &rel, key.line_no, key.notes);
            }
        }
    }
    if assets.is_dir() {
        add_dialogue_keys(&assets, &mut gathered);
    }
    Ok(gathered)
}

/// The first `assets/`-relative path of a file named `name` under `assets` (sorted), else `name`.
fn find_file(assets: &Path, name: &str) -> String {
    let mut stack = vec![assets.to_path_buf()];
    let mut found: Vec<String> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name)
                && let Ok(rel) = path.strip_prefix(assets)
            {
                found.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    found.into_iter().next().unwrap_or_else(|| name.to_owned())
}

/// The dialogue keys, as `regia loc gather` adds them (`regia_cli` `loc_sheet::add_dialogue_keys`).
fn add_dialogue_keys(assets: &Path, gathered: &mut Gathered) {
    let library = regia_framework::dialogue::DialogueLibrary::load_dir(assets);
    let line_of = |rel: &str, needle: &str| -> u32 {
        std::fs::read_to_string(assets.join(rel))
            .ok()
            .and_then(|text| {
                text.lines()
                    .position(|l| l.contains(needle))
                    .and_then(|i| u32::try_from(i + 1).ok())
            })
            .unwrap_or(1)
    };
    for (name, dialogue) in &library.dialogues {
        let rel = find_file(assets, &format!("{name}.dialogue.ron"));
        let file = format!("assets/{rel}");
        gathered.files_scanned += 1;
        for line in &dialogue.lines {
            let at = line_of(&rel, &format!("\"{}\"", line.id));
            let mut notes = vec![format!("speaker: {}", line.speaker)];
            if !line.note.is_empty() {
                notes.push(format!("note: {}", line.note));
            }
            gathered.add_key(&line.text_key(name), &file, at, notes.clone());
            if let Some(display) = &line.display_key {
                gathered.add_key(display, &file, at, notes.clone());
            }
            for cue in line.variants.iter().filter_map(|v| v.cues.as_ref()).flatten() {
                if let Some(key) = &cue.text_key {
                    gathered.add_key(key, &file, at, Vec::new());
                }
            }
        }
    }
    for (rel, track) in &library.subtitles {
        gathered.files_scanned += 1;
        for cue in &track.cues {
            if let Some(key) = &cue.text_key {
                gathered.add_key(key, &format!("assets/{rel}"), line_of(rel, key), Vec::new());
            }
        }
    }
    for voice in library.voices.values() {
        let rel = find_file(assets, &format!("{}.voice.ron", voice.id));
        gathered.files_scanned += 1;
        gathered.add_key(&voice.name_key, &format!("assets/{rel}"), 1, Vec::new());
    }
}

/// What a gather would write: one change per locale `gathered.ftl` that gains keys
/// (`regia_ui::loc_gather::plan`, the CLI's planner).
#[must_use]
pub fn plan_gather(project: &LocProject, gathered: &Gathered) -> Vec<(FileChange, Vec<String>)> {
    let locales: Vec<_> = project
        .locales
        .iter()
        .map(|l| {
            (
                regia_ui::loc_check::parse_locale(&l.locale, &l.files),
                l.files.clone(),
            )
        })
        .collect();
    let mut existing = BTreeMap::new();
    for l in &project.locales {
        let rel = format!("{LOCALIZATION_DIR}/{}/{}", l.locale, loc_gather::GATHERED_FILE);
        if let Some((_, text)) = l.files.iter().find(|(p, _)| *p == rel) {
            existing.insert(l.locale.clone(), text.clone());
        }
    }
    loc_gather::plan(gathered, &locales, &project.source, LOCALIZATION_DIR, &existing)
        .into_iter()
        .map(|file| {
            let before = existing.get(&file.locale).cloned();
            (
                FileChange {
                    path: file.path,
                    locale: file.locale,
                    before,
                    after: file.text,
                },
                file.added,
            )
        })
        .collect()
}

/// What kind of file uses a key, from its name.
#[must_use]
pub fn usage_kind(file: &str) -> &'static str {
    let lower = file.to_ascii_lowercase();
    if lower.ends_with(".ui.ron") {
        "ui"
    } else if lower.ends_with(".conv.ron") {
        "conversation"
    } else if lower.ends_with(".dialogue.ron")
        || lower.ends_with(".subs.ron")
        || lower.ends_with(".voice.ron")
    {
        "dialogue"
    } else if lower.ends_with(".level.ron") || lower.ends_with(".scene.ron") || lower.ends_with(".prefab.ron")
    {
        "scene"
    } else if lower.ends_with(".rs") {
        "code"
    } else {
        "asset"
    }
}

/// One place that uses a key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Usage {
    /// Project-relative path.
    pub file: String,
    /// 1-based line (0 for a file that uses it through another asset).
    pub line: u32,
    /// [`usage_kind`] of the file.
    pub kind: &'static str,
    /// The file that names the key, when this one reaches it through the asset graph.
    pub via: Option<String>,
}

/// The places that use `key`: the gather's direct hits, then (with `graph`, the editor's asset registry)
/// every asset that references one of those files directly or indirectly (a level placing a prefab whose
/// UI names the key). Registry paths may be project-relative or `assets/`-relative; both are matched.
#[must_use]
pub fn usages(gathered: &Gathered, key: &str, graph: Option<&DependencyGraph>) -> Vec<Usage> {
    let mut out: Vec<Usage> = gathered
        .keys
        .get(key)
        .into_iter()
        .flatten()
        .map(|u| Usage {
            file: u.file.clone(),
            line: u.line,
            kind: usage_kind(&u.file),
            via: None,
        })
        .collect();
    let direct: BTreeSet<String> = out.iter().map(|u| u.file.clone()).collect();
    if let Some(graph) = graph {
        let mut seen = direct.clone();
        for file in &direct {
            let (path, prefixed) = if graph.contains(file) {
                (file.as_str(), false)
            } else {
                match file.strip_prefix("assets/") {
                    Some(p) if graph.contains(p) => (p, true),
                    _ => continue,
                }
            };
            for referrer in graph.transitive_referrers(path) {
                let full = if prefixed {
                    format!("assets/{referrer}")
                } else {
                    referrer
                };
                if seen.insert(full.clone()) {
                    out.push(Usage {
                        kind: usage_kind(&full),
                        file: full,
                        line: 0,
                        via: Some(file.clone()),
                    });
                }
            }
        }
    }
    out
}

/// The editor's asset graph, from its loaded registry (`None` when no registry is loaded).
#[must_use]
pub fn registry_graph(world: &World) -> Option<DependencyGraph> {
    let loaded = world.get_resource::<regia_asset::registry::LoadedAssetRegistry>()?;
    let set = regia_asset_pipeline::asset_graph::AssetSet::from_registry(&loaded.registry);
    Some(DependencyGraph::build(&set))
}

/// Sets pseudo-localization on the `Localization` of `world` when it has one; returns whether it has one.
pub fn sync_pseudo(world: &mut World, on: bool) -> bool {
    match world.get_resource_mut::<regia_ui::localization::Localization>() {
        Some(mut loc) => {
            if loc.pseudo() != on {
                loc.set_pseudo(on);
            }
            true
        }
        None => false,
    }
}
