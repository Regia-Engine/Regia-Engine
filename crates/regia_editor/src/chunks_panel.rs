// SPDX-License-Identifier: MPL-2.0
//! The Content Chunks panel (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 6, ADR-1876):
//! every chunk of the project's cooked assets with a size bar (disk or memory), the direct assets of
//! the selected chunk with the rule selector that picked each, and "why is this asset here" for any
//! asset (also from the Content Browser's context menu).
//!
//! The view is computed from the project's `Regia.toml` chunk rules and the cooked registry with the
//! same pipeline functions the cook and `regia assets chunks` use ([`assign_with_roots`],
//! [`summarize_chunks`], [`why_table`]), so the panel shows what the next cook writes. Agents read and drive it with
//! `editor.content_chunks_state` and `editor.content_chunks_why`
//! (`book/agents/content-chunks-panel.md`). Rules are edited in Project Settings or with
//! `editor.content_chunks_set_rule`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset::AssetId;
use regia_asset::content::{ChunkWhy, WhyAnswer};
use regia_asset::registry::{AssetRegistry, cache_registry_path};
use regia_asset_pipeline::chunks::{
    ChunkRules, ChunkSummary, ContentMode, Severity, assign_with_roots, check_references_with,
    optional_references, startup_roots, summarize_chunks, why_table,
};
use regia_asset_pipeline::closure_roots::RootReport;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Most direct assets the panel and `editor.content_chunks_state` list for one chunk.
pub const MAX_DIRECT_ASSETS: usize = 100;

/// The computed view of a project's chunks.
#[derive(Clone, Debug, Default)]
pub struct ChunksView {
    /// The cooked platform the view is for.
    pub platform: String,
    /// Every chunk, in mount order.
    pub chunks: Vec<ChunkSummary>,
    /// The reason table (as the cook would write `chunk-why.bin`).
    pub why: ChunkWhy,
    /// Reference-check problems (`<reason>: <message>`); errors fail the next cook.
    pub problems: Vec<String>,
    /// The cooked registry the view was computed from.
    pub registry: AssetRegistry,
    /// Each chunk's assets, largest first (ties by path).
    pub size_maps: BTreeMap<String, Vec<AssetSize>>,
    /// `[cook.chunk_options] content`: every asset, or the closure of the entry points.
    pub content: ContentMode,
    /// `assets/...` paths of the cooked assets that ship in no chunk (`content = "closure"` and no
    /// entry point reaches them, ADR-8092), sorted.
    pub unshipped: Vec<String>,
    /// Why each root of the cook exists and how its raw files were scanned (the resolver named
    /// build profiles share, CT-25, ADR-8477).
    pub roots: RootReport,
}

/// One row of a chunk's size map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetSize {
    /// `assets/...` path.
    pub path: String,
    /// The type path's last segment.
    pub type_name: String,
    /// Cooked bytes.
    pub bytes: u64,
}

/// One end of a reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRef {
    /// `assets/...` path.
    pub path: String,
    /// The chunk that holds it (empty when the cook does not know it).
    pub chunk: String,
}

/// What an asset references and what references it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetRefs {
    /// `assets/...` path.
    pub path: String,
    /// Its chunk.
    pub chunk: String,
    /// Assets it references.
    pub dependencies: Vec<AssetRef>,
    /// Assets that reference it.
    pub referrers: Vec<AssetRef>,
}

impl ChunksView {
    fn asset_ref(&self, id: AssetId) -> Option<AssetRef> {
        let entry = self.registry.by_id(id)?;
        Some(AssetRef {
            path: entry.path.clone(),
            chunk: self.why.explain(id).map(|a| a.chunk).unwrap_or_default(),
        })
    }

    /// The references of `asset` (a path, with or without `assets/`, or an id), each with its chunk.
    #[must_use]
    /// An unshipped asset (no chunk) resolves through the registry, so its referrers can be
    /// inspected too; its `chunk` is empty.
    pub fn references(&self, asset: &str) -> Option<AssetRefs> {
        let id = self
            .why
            .resolve(asset)
            .or_else(|| self.registry.resolve(asset).map(|e| e.id))?;
        let entry = self.registry.by_id(id)?;
        let mut dependencies: Vec<AssetRef> = entry
            .dependencies
            .iter()
            .filter_map(|d| self.asset_ref(*d))
            .collect();
        dependencies.sort_by(|a, b| a.path.cmp(&b.path));
        let mut referrers: Vec<AssetRef> = self
            .registry
            .entries()
            .filter(|e| e.dependencies.contains(&id))
            .filter_map(|e| self.asset_ref(e.id))
            .collect();
        referrers.sort_by(|a, b| a.path.cmp(&b.path));
        Some(AssetRefs {
            path: entry.path.clone(),
            chunk: self.why.explain(id).map(|a| a.chunk).unwrap_or_default(),
            dependencies,
            referrers,
        })
    }
}

/// Panel state (a resource; the shell and the agent methods share it).
#[derive(Resource, Clone, Debug, Default)]
pub struct ContentChunksPanel {
    /// Platform to read the cooked cache of (empty = the host platform).
    pub platform: String,
    /// Size bars show memory (decompressed cooked) bytes instead of disk (pak) bytes.
    pub show_memory: bool,
    /// The selected chunk.
    pub selected: Option<String>,
    /// The asset the "why" box explains.
    pub why_query: String,
    /// The last "why" answer.
    pub why_answer: Option<WhyAnswer>,
    /// The references of the last "why" asset.
    pub why_refs: Option<AssetRefs>,
    /// Why the last "why" failed.
    pub why_error: Option<String>,
    /// The computed view; `None` until the first refresh.
    pub view: Option<Arc<ChunksView>>,
    /// Why the last refresh failed.
    pub error: Option<String>,
    /// Stable code of the last refresh failure, preserved for the agent report.
    pub error_code: Option<&'static str>,
    /// The Unshipped list's filter: a substring, or a glob under `assets/` when it holds `*`/`?`.
    pub unshipped_filter: String,
    /// The cursors of the Unshipped pages before the shown one (empty: the first page).
    pub unshipped_cursors: Vec<String>,
    /// [`crate::closure_settings::manifest_revision`] when the view was computed: an undo or a
    /// settings save that rewrote `Regia.toml` makes the panel recompute.
    pub manifest_revision: u64,
    /// The last refresh found no cooked assets: a normal state before the first cook, drawn as a
    /// plain sentence rather than an error (bug 25, ADR-8586).
    pub not_cooked: bool,
}

/// A panel failure with its agent error code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunksPanelError {
    /// `content_chunks_no_project`, `content_chunks_no_cook`, `content_registry_invalid`, `content_chunks_rules`,
    /// `content_chunks_bad_chunk`, `content_chunks_asset_not_found`, `content_chunks_unshipped` or
    /// `content_chunks_bad_platform`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

pub(crate) fn refuse<T>(code: &'static str, message: impl Into<String>) -> Result<T, ChunksPanelError> {
    Err(ChunksPanelError {
        code,
        message: message.into(),
    })
}

/// Each chunk's assets, largest first, with the size source `summarize_chunks` uses.
fn size_maps(registry: &AssetRegistry, why: &ChunkWhy, cache_dir: &Path) -> BTreeMap<String, Vec<AssetSize>> {
    let mut maps: BTreeMap<String, Vec<AssetSize>> = BTreeMap::new();
    for record in &why.records {
        let Some(entry) = registry.by_id(record.id) else {
            continue;
        };
        let Some(chunk) = usize::try_from(record.chunk).ok().and_then(|i| why.chunks.get(i)) else {
            continue;
        };
        let bytes = if entry.size > 0 {
            entry.size
        } else {
            std::fs::metadata(cache_dir.join(regia_asset::cooked_file_name(entry.id))).map_or(0, |m| m.len())
        };
        maps.entry(chunk.clone()).or_default().push(AssetSize {
            path: entry.path.clone(),
            type_name: entry
                .type_path
                .rsplit("::")
                .next()
                .unwrap_or(&entry.type_path)
                .to_owned(),
            bytes,
        });
    }
    for rows in maps.values_mut() {
        rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    }
    maps
}

/// The cooked registry of `project` for `platform` (empty = host): the resolved platform tag, the
/// cooked cache directory and the registry. Shared by [`load_view`] and the closure preview.
///
/// # Errors
/// `content_chunks_bad_platform`, `content_registry_invalid` or `content_chunks_no_cook`.
pub(crate) fn read_registry(
    project: &Path,
    platform: &str,
) -> Result<(String, PathBuf, AssetRegistry), ChunksPanelError> {
    if !platform.is_empty() && !valid_platform_tag(platform) {
        return refuse(
            "content_chunks_bad_platform",
            format!("{platform:?} is not a platform tag (1-32 lowercase letters, digits, `_` or `-`)"),
        );
    }
    let platform = if platform.is_empty() {
        regia_asset_pipeline::host_platform().to_string()
    } else {
        platform.to_owned()
    };
    let cache_dir = regia_asset::cooked_cache_dir(project, &platform);
    let registry = match AssetRegistry::read_from(&cache_registry_path(&cache_dir)) {
        Ok(Some(registry)) if !registry.is_empty() => registry,
        Err(e) => {
            return refuse(
                "content_registry_invalid",
                format!("{}: {e}", cache_registry_path(&cache_dir).display()),
            );
        }
        _ => {
            return refuse(
                "content_chunks_no_cook",
                format!(
                    "no cooked asset registry for {platform} in {} (cook the project first)",
                    cache_dir.display()
                ),
            );
        }
    };
    Ok((platform, cache_dir, registry))
}

/// Computes the chunk view of `project` for `platform` (empty = host) from `Regia.toml` and the
/// cooked registry.
///
/// # Errors
/// `content_chunks_no_cook` without cooked assets, `content_registry_invalid` for an unreadable
/// registry, `content_chunks_rules` for invalid rules,
/// `content_chunks_bad_platform` for a tag that is not a plain platform name (it is joined into
/// the cache and pak paths, so `../x` must never get there).
pub fn load_view(project: &Path, platform: &str) -> Result<ChunksView, ChunksPanelError> {
    let (platform, cache_dir, registry) = read_registry(project, platform)?;
    let rules = match std::fs::read_to_string(project.join("Regia.toml")) {
        Ok(text) => ChunkRules::from_manifest_toml(&text)
            .or_else(|e| refuse("content_chunks_rules", e.to_string()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ChunkRules::default(),
        Err(e) => return refuse("content_chunks_rules", format!("reading Regia.toml: {e}")),
    };
    // The startup level's boot closure is pinned in base and, with `content = "closure"`, every
    // root is an entry point: the same evidence the cook uses (ADR-8092, ADR-8477).
    let roots = startup_roots(project, &registry, &rules).or_else(|e| refuse(e.code(), e.to_string()))?;
    let assignment = assign_with_roots(&registry, &rules, &roots)
        .or_else(|e| refuse("content_chunks_rules", e.to_string()))?;
    let path_of = |id: AssetId| {
        registry
            .by_id(id)
            .map_or_else(|| id.to_string(), |e| e.path.clone())
    };
    let roots = roots.report(&path_of);
    let pak_dir = project.join("build").join("paks").join(&platform);
    let chunks = summarize_chunks(&registry, &assignment, &rules, &cache_dir, &pak_dir)
        .or_else(|e| refuse("content_chunks_rules", e.to_string()))?;
    let problems = check_references_with(&registry, &assignment, &rules, &optional_references(&registry))
        .into_iter()
        .map(|p| {
            format!(
                "{}{}: {}",
                if p.severity == Severity::Warning {
                    "warning "
                } else {
                    ""
                },
                p.reason,
                p.message
            )
        })
        .collect();
    let why = why_table(&registry, &assignment, &rules);
    let mut unshipped: Vec<String> = assignment
        .unshipped
        .iter()
        .filter_map(|id| registry.by_id(*id).map(|e| e.path.clone()))
        .collect();
    unshipped.sort();
    Ok(ChunksView {
        size_maps: size_maps(&registry, &why, &cache_dir),
        content: rules.content,
        unshipped,
        roots,
        platform,
        why,
        chunks,
        problems,
        registry,
    })
}

impl ContentChunksPanel {
    /// Recomputes the view for `project`.
    pub fn refresh(&mut self, project: Option<&Path>) {
        let result = match project {
            Some(root) => load_view(root, &self.platform),
            None => refuse("content_chunks_no_project", "no project is open"),
        };
        match result {
            Ok(view) => {
                self.manifest_revision = crate::closure_settings::manifest_revision();
                self.unshipped_cursors.clear();
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|s| !view.chunks.iter().any(|c| &c.name == s))
                {
                    self.selected = None;
                }
                self.view = Some(Arc::new(view));
                self.error = None;
                self.error_code = None;
                self.not_cooked = false;
            }
            Err(e) => {
                self.view = None;
                self.not_cooked = e.code == "content_chunks_no_cook";
                self.error = Some(e.message);
                self.error_code = Some(e.code);
            }
        }
    }

    /// Explains `asset` (a path, with or without `assets/`, or an id) and keeps the answer.
    ///
    /// # Errors
    /// `content_chunks_asset_not_found`, or no view yet.
    pub fn ask_why(&mut self, asset: &str) -> Result<&WhyAnswer, ChunksPanelError> {
        self.why_query = asset.to_owned();
        self.why_answer = None;
        self.why_refs = None;
        let Some(view) = &self.view else {
            return refuse(
                self.error_code.unwrap_or("content_chunks_no_cook"),
                self.error
                    .clone()
                    .unwrap_or_else(|| "the panel has no view yet".into()),
            );
        };
        let Some(answer) = view.why.resolve(asset).and_then(|id| view.why.explain(id)) else {
            if let Some(entry) = view.registry.resolve(asset)
                && view.unshipped.binary_search(&entry.path).is_ok()
            {
                return refuse(
                    "content_chunks_unshipped",
                    format!(
                        "{} ships in no chunk: content = \"closure\" and no entry point reaches it",
                        entry.path
                    ),
                );
            }
            return refuse(
                "content_chunks_asset_not_found",
                format!("{asset} is not a cooked asset of this project"),
            );
        };
        self.why_refs = view.references(asset);
        Ok(self.why_answer.insert(answer))
    }

    /// The bar value of a chunk (disk or memory bytes).
    #[must_use]
    pub fn bar_bytes(&self, chunk: &ChunkSummary) -> u64 {
        if self.show_memory {
            chunk.memory_bytes
        } else {
            chunk.disk_bytes
        }
    }

    /// Direct assets of the selected chunk (path, selector), at most [`MAX_DIRECT_ASSETS`], and the
    /// total count.
    #[must_use]
    pub fn direct_assets(&self) -> (Vec<(String, String)>, usize) {
        match (&self.view, &self.selected) {
            (Some(view), Some(chunk)) => {
                let mut all = view.why.direct_assets(chunk);
                let total = all.len();
                all.truncate(MAX_DIRECT_ASSETS);
                (all, total)
            }
            _ => (Vec::new(), 0),
        }
    }
}

pub(crate) fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

/// Draws the panel.
pub fn content_chunks_panel(ui: &mut egui::Ui, world: &mut World) {
    let root = project_root(world);
    let mut panel = world.remove_resource::<ContentChunksPanel>().unwrap_or_default();
    if (panel.view.is_none() && panel.error.is_none())
        || (panel.view.is_some() && panel.manifest_revision != crate::closure_settings::manifest_revision())
    {
        panel.refresh(root.as_deref());
    }
    draw(ui, &mut panel, root.as_deref());
    world.insert_resource(panel);
}

/// What the panel says before the project is cooked for the platform (bug 25, ADR-8586).
pub const NOT_COOKED_TEXT: &str = "Not cooked yet for this platform. Cook the project to see its chunks.";

fn draw(ui: &mut egui::Ui, panel: &mut ContentChunksPanel, root: Option<&Path>) {
    ui.horizontal(|ui| {
        ui.label("Platform");
        ui.add(
            egui::TextEdit::singleline(&mut panel.platform)
                .hint_text(regia_asset_pipeline::host_platform().to_string())
                .desired_width(80.0),
        );
        if ui.button("Refresh").clicked() {
            panel.refresh(root);
        }
        ui.separator();
        ui.selectable_value(&mut panel.show_memory, false, "Disk");
        ui.selectable_value(&mut panel.show_memory, true, "Memory");
    });
    if let Some(error) = &panel.error {
        let error = crate::save_all_dialog::user_path_text(error);
        if panel.not_cooked {
            // Not cooked yet is a normal state: one plain sentence, the details on hover.
            ui.weak(NOT_COOKED_TEXT).on_hover_text(error);
        } else {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        return;
    }
    let Some(view) = panel.view.clone() else {
        return;
    };
    let max = view
        .chunks
        .iter()
        .map(|c| panel.bar_bytes(c))
        .max()
        .unwrap_or(0)
        .max(1);
    egui::Grid::new("content_chunks_grid")
        .striped(true)
        .num_columns(5)
        .show(ui, |ui| {
            ui.strong("Chunk");
            ui.strong("Delivery");
            ui.strong("Assets");
            ui.strong(if panel.show_memory { "Memory" } else { "Disk" });
            ui.strong("Requires");
            ui.end_row();
            for chunk in &view.chunks {
                let selected = panel.selected.as_deref() == Some(chunk.name.as_str());
                if ui.selectable_label(selected, chunk.name.as_str()).clicked() {
                    panel.selected = Some(chunk.name.clone());
                }
                ui.label(chunk.delivery.name());
                ui.label(chunk.assets.to_string());
                let bytes = panel.bar_bytes(chunk);
                ui.add(
                    egui::ProgressBar::new(bytes as f32 / max as f32)
                        .desired_width(160.0)
                        .text(format_bytes(bytes)),
                );
                ui.label(chunk.requires.join(", "));
                ui.end_row();
            }
        });
    if !view.unshipped.is_empty() {
        // `content = "closure"` (ADR-8092): what the roots leave out of every chunk, paged and
        // filtered like `editor.content_chunks_unshipped` (CT-26).
        unshipped_section(ui, panel, &view);
    }
    for problem in &view.problems {
        ui.colored_label(ui.visuals().warn_fg_color, problem.as_str());
    }
    ui.separator();
    if let Some(chunk) = panel.selected.clone() {
        let (assets, total) = panel.direct_assets();
        ui.label(format!(
            "Direct assets of {chunk} ({total}; selected by its rule)"
        ));
        egui::ScrollArea::vertical()
            .id_salt("content_chunks_direct")
            .max_height(160.0)
            .show(ui, |ui| {
                for (path, selector) in &assets {
                    ui.horizontal(|ui| {
                        if ui.link(path.as_str()).clicked() {
                            let _ = panel.ask_why(path);
                        }
                        ui.weak(selector.as_str());
                    });
                }
            });
        size_map(ui, panel, &view, &chunk);
        ui.separator();
    }
    ui.horizontal(|ui| {
        ui.label("Why is this asset here?");
        let edit = ui.add(egui::TextEdit::singleline(&mut panel.why_query).hint_text("assets/... or an id"));
        let go = ui.button("Explain").clicked()
            || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if go {
            let query = panel.why_query.clone();
            panel.why_error = panel.ask_why(&query).err().map(|e| e.message);
        }
    });
    if let Some(error) = &panel.why_error {
        ui.label(error.as_str());
    }
    if let Some(answer) = &panel.why_answer {
        ui.label(format!(
            "{} is in chunk {} ({})",
            answer.path, answer.chunk, answer.reason
        ));
        if let Some((chunk, index, selector)) = &answer.rule {
            ui.label(format!(
                "rule: [cook.chunks.{chunk}] selector {index}: {selector}"
            ));
        }
        if answer.via.len() > 1 {
            ui.label(format!("via {}", answer.via.join(" <- ")));
        }
        if !answer.duplicated_in.is_empty() {
            ui.label(format!("copied into {}", answer.duplicated_in.join(", ")));
        }
    }
    if let Some(refs) = panel.why_refs.clone() {
        references(ui, panel, &refs);
    }
}

/// The Unshipped list: a filter box and keyset pages of [`MAX_DIRECT_ASSETS`] paths; a path opens
/// its references (the existing why/references view).
fn unshipped_section(ui: &mut egui::Ui, panel: &mut ContentChunksPanel, view: &ChunksView) {
    let cursor = panel.unshipped_cursors.last().cloned();
    let page = crate::closure_settings::unshipped_page(
        &view.unshipped,
        &panel.unshipped_filter,
        cursor.as_deref(),
        MAX_DIRECT_ASSETS,
    );
    egui::CollapsingHeader::new(format!(
        "Unshipped: {} asset(s) no root reaches",
        view.unshipped.len()
    ))
    .id_salt("content_chunks_unshipped")
    .show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Filter");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut panel.unshipped_filter)
                        .hint_text("substring or glob, e.g. dlc/**")
                        .desired_width(180.0),
                )
                .changed()
            {
                panel.unshipped_cursors.clear();
            }
            ui.weak(format!(
                "{} of {} match; page {}",
                page.matched,
                page.total,
                panel.unshipped_cursors.len() + 1
            ));
            let back = ui
                .add_enabled(!panel.unshipped_cursors.is_empty(), egui::Button::new("Previous"))
                .clicked();
            let next = ui
                .add_enabled(page.next_cursor.is_some(), egui::Button::new("Next"))
                .clicked();
            if back {
                panel.unshipped_cursors.pop();
            } else if next && let Some(cursor) = page.next_cursor.clone() {
                panel.unshipped_cursors.push(cursor);
            }
        });
        for path in &page.paths {
            if ui.link(path.as_str()).clicked() {
                panel.why_query.clone_from(path);
                panel.why_error = panel.ask_why(path).err().map(|e| e.message);
                panel.why_refs = view.references(path);
            }
        }
    });
}

/// The selected chunk's assets, largest first, with bars relative to the largest.
fn size_map(ui: &mut egui::Ui, panel: &mut ContentChunksPanel, view: &ChunksView, chunk: &str) {
    let rows = view.size_maps.get(chunk).map_or(&[][..], Vec::as_slice);
    egui::CollapsingHeader::new(format!("Size map of {chunk} ({} assets)", rows.len()))
        .id_salt("content_chunks_size_map")
        .show(ui, |ui| {
            let largest = rows.first().map_or(1, |r| r.bytes.max(1));
            egui::ScrollArea::vertical()
                .id_salt("content_chunks_size_rows")
                .max_height(200.0)
                .show(ui, |ui| {
                    for row in rows.iter().take(MAX_DIRECT_ASSETS) {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::ProgressBar::new(row.bytes as f32 / largest as f32)
                                    .desired_width(120.0)
                                    .text(format_bytes(row.bytes)),
                            );
                            if ui.link(row.path.as_str()).clicked() {
                                let _ = panel.ask_why(&row.path);
                            }
                            ui.weak(row.type_name.as_str());
                        });
                    }
                });
        });
}

/// What the explained asset references and what references it; an entry in another chunk names it.
fn references(ui: &mut egui::Ui, panel: &mut ContentChunksPanel, refs: &AssetRefs) {
    egui::CollapsingHeader::new(format!(
        "References ({} out, {} in)",
        refs.dependencies.len(),
        refs.referrers.len()
    ))
    .id_salt("content_chunks_references")
    .default_open(true)
    .show(ui, |ui| {
        for (title, list) in [
            ("References", &refs.dependencies),
            ("Referenced by", &refs.referrers),
        ] {
            if list.is_empty() {
                continue;
            }
            ui.strong(title);
            // Capped like the direct assets (ADR-7306): a shared texture can have thousands of
            // referrers; `editor.content_chunks_why` returns them all.
            for item in list.iter().take(MAX_DIRECT_ASSETS) {
                ui.horizontal(|ui| {
                    if ui.link(item.path.as_str()).clicked() {
                        let _ = panel.ask_why(&item.path);
                    }
                    if item.chunk != refs.chunk {
                        ui.weak(format!("in {}", item.chunk));
                    }
                });
            }
            if list.len() > MAX_DIRECT_ASSETS {
                ui.weak(format!("… and {} more", list.len() - MAX_DIRECT_ASSETS));
            }
        }
    });
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn rpc(e: ChunksPanelError) -> RpcError {
    let hint = match e.code {
        "content_chunks_no_project" => "start the editor with --project <dir>",
        "content_chunks_no_cook" => "cook the project (`regia cook`, or the editor's cook) first",
        "content_registry_invalid" => "rebuild the cooked registry and inspect the cook errors",
        "content_chunks_rules" => "fix the [cook.chunks.*] tables in Regia.toml",
        "content_chunks_bad_chunk" => "editor.content_chunks_state lists the chunk names",
        "content_chunks_entry_unmatched" => {
            "fix or remove the entry glob; editor.content_chunks_preview lists each root with `matched`"
        }
        "content_chunks_no_entry" => {
            "add an entry root or a [project] startup_level, or set content = \"all\""
        }
        "content_chunks_preview_stale" => "an input changed while the preview computed; request it again",
        "content_chunks_preview_failed" => "request the preview again; the editor log has the cause",
        "content_chunks_unshipped" => {
            "add an `entry` that reaches it to a [cook.chunks.*] rule, or set [cook.chunk_options] content = \"all\""
        }
        _ => "use an assets/... path or an id the cooked registry knows",
    };
    let code = match e.code {
        "content_chunks_rules"
        | "content_chunks_bad_chunk"
        | "content_chunks_entry_unmatched"
        | "content_chunks_no_entry" => regia_agent::protocol::INVALID_PARAMS,
        _ => regia_agent::protocol::NOT_FOUND,
    };
    RpcError::new(code, e.code, e.message, hint)
}

fn why_json(answer: &WhyAnswer) -> Value {
    let mut value = json!({
        "asset": answer.path,
        "id": answer.id.to_string(),
        "chunk": answer.chunk,
        "reason": answer.reason,
        "via": answer.via,
    });
    if let Some((chunk, index, selector)) = &answer.rule {
        value["rule"] = json!({ "chunk": chunk, "index": index, "selector": selector });
    }
    if !answer.duplicated_in.is_empty() {
        value["duplicated_in"] = json!(answer.duplicated_in);
    }
    value
}

fn state_json(panel: &ContentChunksPanel) -> Value {
    let (direct, total) = panel.direct_assets();
    let chunks: Vec<Value> = panel
        .view
        .as_ref()
        .map(|v| {
            v.chunks
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "delivery": c.delivery.name(),
                        "assets": c.assets,
                        "disk_bytes": c.disk_bytes,
                        "memory_bytes": c.memory_bytes,
                        "bar_bytes": panel.bar_bytes(c),
                        "requires": c.requires,
                        "mount_order": c.mount_order,
                        "by_type": c.by_type,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "loaded": panel.view.is_some(),
        "platform": panel.view.as_ref().map(|v| v.platform.clone()),
        "show": if panel.show_memory { "memory" } else { "disk" },
        "chunks": chunks,
        "selected": panel.selected,
        "direct_assets": direct.iter().map(|(path, selector)| json!({ "path": path, "selector": selector })).collect::<Vec<_>>(),
        "direct_assets_total": total,
        "problems": panel.view.as_ref().map(|v| v.problems.clone()).unwrap_or_default(),
        "content": panel.view.as_ref().map(|v| v.content.name()),
        "unshipped": panel
            .view
            .as_ref()
            .map(|v| v.unshipped.iter().take(MAX_DIRECT_ASSETS).cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        "unshipped_total": panel.view.as_ref().map_or(0, |v| v.unshipped.len()),
        "unshipped_next_cursor": panel
            .view
            .as_ref()
            .and_then(|v| crate::closure_settings::unshipped_page(&v.unshipped, "", None, MAX_DIRECT_ASSETS).next_cursor),
        "roots": panel
            .view
            .as_ref()
            .map(|v| v.roots.records.iter().take(MAX_DIRECT_ASSETS).cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        "roots_total": panel.view.as_ref().map_or(0, |v| v.roots.records.len()),
        "root_coverage": panel.view.as_ref().map(|v| v.roots.coverage.clone()),
        "why": panel.why_answer.as_ref().map(why_json),
        "error": panel.error,
        "error_code": panel.error_code,
    })
}

/// `editor.content_chunks_state { platform?, refresh?, chunk?, show? }`.
fn content_chunks_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world);
    let mut panel = world.remove_resource::<ContentChunksPanel>().unwrap_or_default();
    let result = drive_state(&mut panel, root.as_deref(), params);
    world.insert_resource(panel);
    result
}

/// The agent-controlled `platform` tag must be a plain host tag before it is joined into cache
/// and pak paths (audit G finding 6): anything holding a separator, `..` or a drive colon
/// would make the cooked-cache read stat files outside the project (ADR-1816 confines every
/// other agent path the same way).
pub(crate) fn valid_platform_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

pub(crate) fn drive_state(
    panel: &mut ContentChunksPanel,
    root: Option<&Path>,
    params: &Value,
) -> Result<Value, RpcError> {
    if let Some(platform) = params.get("platform").and_then(Value::as_str) {
        if !platform.is_empty() && !valid_platform_tag(platform) {
            return Err(RpcError::invalid_params(format!(
                "`platform` must be empty or a host tag of 1-32 lowercase letters, digits, `_` or `-`, not {platform:?}"
            )));
        }
        if platform != panel.platform {
            panel.platform = platform.to_owned();
            panel.view = None;
        }
    }
    match params.get("show").and_then(Value::as_str) {
        None => {}
        Some("disk") => panel.show_memory = false,
        Some("memory") => panel.show_memory = true,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`show` must be disk or memory, not {other:?}"
            )));
        }
    }
    let refresh = params.get("refresh").and_then(Value::as_bool).unwrap_or(false);
    // A settings save or an undo that rewrote Regia.toml since the view was computed recomputes it.
    let rewritten =
        panel.view.is_some() && panel.manifest_revision != crate::closure_settings::manifest_revision();
    if refresh || rewritten || panel.view.is_none() {
        panel.refresh(root);
    }
    if panel.view.is_none() {
        let code = panel.error_code.unwrap_or(if root.is_none() {
            "content_chunks_no_project"
        } else {
            "content_chunks_no_cook"
        });
        return Err(rpc(ChunksPanelError {
            code,
            message: panel.error.clone().unwrap_or_default(),
        }));
    }
    if let Some(chunk) = params.get("chunk").and_then(Value::as_str) {
        let known = panel
            .view
            .as_ref()
            .is_some_and(|v| v.chunks.iter().any(|c| c.name == chunk));
        if !known {
            return Err(rpc(ChunksPanelError {
                code: "content_chunks_bad_chunk",
                message: format!("no chunk named {chunk:?}"),
            }));
        }
        panel.selected = Some(chunk.to_owned());
    }
    Ok(state_json(panel))
}

/// `editor.content_chunks_why { asset }`.
fn content_chunks_why(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(asset) = params
        .get("asset")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(RpcError::invalid_params("`asset` (a path or an id) is required"));
    };
    let root = project_root(world);
    let mut panel = world.remove_resource::<ContentChunksPanel>().unwrap_or_default();
    if panel.view.is_none() {
        panel.refresh(root.as_deref());
    }
    let result = panel.ask_why(asset).map(why_json).map_err(rpc);
    world.insert_resource(panel);
    result
}

fn refs_json(refs: &AssetRefs) -> Value {
    let list = |items: &[AssetRef]| -> Vec<Value> {
        items
            .iter()
            .map(|i| json!({ "path": i.path, "chunk": i.chunk }))
            .collect()
    };
    json!({
        "asset": refs.path,
        "chunk": refs.chunk,
        "dependencies": list(&refs.dependencies),
        "referrers": list(&refs.referrers),
    })
}

/// Runs `body` on the panel with its view computed.
fn with_view<T>(
    world: &mut World,
    body: impl FnOnce(&mut ContentChunksPanel) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    let root = project_root(world);
    let mut panel = world.remove_resource::<ContentChunksPanel>().unwrap_or_default();
    if panel.view.is_none() {
        panel.refresh(root.as_deref());
    }
    let result = if panel.view.is_some() {
        body(&mut panel)
    } else {
        Err(rpc(ChunksPanelError {
            code: panel.error_code.unwrap_or(if root.is_none() {
                "content_chunks_no_project"
            } else {
                "content_chunks_no_cook"
            }),
            message: panel.error.clone().unwrap_or_default(),
        }))
    };
    world.insert_resource(panel);
    result
}

/// `editor.content_chunks_size_map { chunk, limit? }`.
fn content_chunks_size_map(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(chunk) = params
        .get("chunk")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(RpcError::invalid_params("`chunk` is required"));
    };
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(MAX_DIRECT_ASSETS);
    with_view(world, |panel| {
        let view = panel
            .view
            .as_ref()
            .ok_or_else(|| RpcError::invalid_params("no view"))?;
        if !view.chunks.iter().any(|c| c.name == chunk) {
            return Err(rpc(ChunksPanelError {
                code: "content_chunks_bad_chunk",
                message: format!("no chunk named {chunk:?}"),
            }));
        }
        let rows = view.size_maps.get(chunk).map_or(&[][..], Vec::as_slice);
        let total: u64 = rows.iter().map(|r| r.bytes).sum();
        let assets: Vec<Value> = rows
            .iter()
            .take(limit)
            .map(|r| json!({ "path": r.path, "type": r.type_name, "bytes": r.bytes }))
            .collect();
        Ok(json!({ "chunk": chunk, "count": rows.len(), "total_bytes": total, "assets": assets }))
    })
}

/// `editor.content_chunks_references { asset }`.
fn content_chunks_references(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(asset) = params
        .get("asset")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Err(RpcError::invalid_params("`asset` (a path or an id) is required"));
    };
    with_view(world, |panel| {
        panel
            .view
            .as_ref()
            .and_then(|view| view.references(asset))
            .map(|refs| refs_json(&refs))
            .ok_or_else(|| {
                rpc(ChunksPanelError {
                    code: "content_chunks_asset_not_found",
                    message: format!("{asset} is not a cooked asset of this project"),
                })
            })
    })
}

pub(crate) fn string_list(params: &Value, field: &str) -> Result<Option<Vec<String>>, RpcError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an array of strings")))?;
    items
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an array of strings")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `editor.content_chunks_set_rule { chunk?, content?, include?, entry?, tags?, requires?, delivery?,
/// priority?, remove?, platform? }`: edits one rule (and/or `[cook.chunk_options] content`) in
/// `Regia.toml`, validated as the cook does (with closure mode or `entry` roots, every root must
/// resolve against the cooked registry), then records an undo entry and recomputes the panel.
fn content_chunks_set_rule(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(root) = project_root(world) else {
        return Err(rpc(ChunksPanelError {
            code: "content_chunks_no_project",
            message: "no project is open".into(),
        }));
    };
    let platform = params.get("platform").and_then(Value::as_str).unwrap_or("");
    let mut settings = crate::project_settings::ProjectSettingsState::default();
    settings.load(&root).map_err(|message| {
        rpc(ChunksPanelError {
            code: "content_chunks_rules",
            message,
        })
    })?;
    let (chunk, removed) = crate::closure_settings::apply_rule_params(&mut settings, params)?;
    let saved =
        crate::closure_settings::save_checked(&mut settings, &root, platform, "agent").map_err(rpc)?;
    let preview = saved
        .preview
        .as_ref()
        .map(|p| crate::closure_settings::preview_json(p, crate::closure_settings::PREVIEW_ROWS));
    crate::closure_settings::push_manifest_undo(world, saved.undo);
    let mut panel = world.remove_resource::<ContentChunksPanel>().unwrap_or_default();
    panel.refresh(Some(&root));
    let error = panel.error.clone();
    world.insert_resource(panel);
    Ok(json!({
        "chunk": chunk,
        "removed": removed,
        "saved": true,
        "verified": saved.verified,
        "content": settings.content_mode().name(),
        "preview": preview,
        "error": error,
    }))
}

const PLATFORM: ParamSchema = ParamSchema {
    name: "platform",
    kind: "string",
    required: false,
    description: "Cooked platform to read (default: the host).",
};

/// The panel's agent methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.content_chunks_state",
                tool: "content_chunks_state",
                description: "Read the Content Chunks panel: every chunk of the project's cooked assets under its \
                    Regia.toml chunk rules (delivery, asset count, disk and memory bytes, requires, mount order, size \
                    by type), the selected chunk's direct assets with the rule selector that picked each, reference \
                    problems and the last why answer. Optionally selects a chunk, switches the size bars or recomputes.",
                params: &[
                    PLATFORM,
                    ParamSchema {
                        name: "chunk",
                        kind: "string",
                        required: false,
                        description: "Select this chunk.",
                    },
                    ParamSchema {
                        name: "show",
                        kind: "string",
                        required: false,
                        description: "disk or memory: what the size bars show.",
                    },
                    ParamSchema {
                        name: "refresh",
                        kind: "boolean",
                        required: false,
                        description: "Recompute from Regia.toml and the cooked registry.",
                    },
                ],
                result: "{ loaded, platform, show, chunks: [{ name, delivery, assets, disk_bytes, memory_bytes, bar_bytes, requires, mount_order, by_type }], selected, direct_assets: [{ path, selector }], direct_assets_total, problems, content, unshipped, unshipped_total, unshipped_next_cursor, roots, roots_total, root_coverage, why, error, error_code }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_chunks_why",
                tool: "content_chunks_why",
                description: "Why an asset is in its chunk, as the panel's why box shows it: the chunk, the reason \
                    (rule, follows, shared, pinned, default), the rule selector, and the asset path chain back to a \
                    direct asset.",
                params: &[ParamSchema {
                    name: "asset",
                    kind: "string",
                    required: true,
                    description: "An assets/... path (the prefix may be left out) or an asset id.",
                }],
                result: "{ asset, id, chunk, reason, rule?: { chunk, index, selector }, via: [path], duplicated_in? }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_why as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_chunks_size_map",
                tool: "content_chunks_size_map",
                description: "The size map of one chunk, as the panel shows it: its assets, largest first, with \
                    type and cooked bytes, and the chunk's total.",
                params: &[
                    ParamSchema {
                        name: "chunk",
                        kind: "string",
                        required: true,
                        description: "The chunk.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Most rows to return (default 100).",
                    },
                ],
                result: "{ chunk, count, total_bytes, assets: [{ path, type, bytes }] }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_size_map as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_chunks_references",
                tool: "content_chunks_references",
                description: "What an asset references and what references it, each with the chunk that holds it, \
                    as the panel's References list shows. A reference into another chunk is what a `requires` entry \
                    or a rule change has to cover.",
                params: &[ParamSchema {
                    name: "asset",
                    kind: "string",
                    required: true,
                    description: "An assets/... path (the prefix may be left out) or an asset id.",
                }],
                result: "{ asset, chunk, dependencies: [{ path, chunk }], referrers: [{ path, chunk }] }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_references as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_chunks_set_rule",
                tool: "content_chunks_set_rule",
                description: "Edit one [cook.chunks.<name>] rule (include, entry roots, tags, requires, delivery, \
                    priority) and/or [cook.chunk_options] content in Regia.toml, as Project Settings does: fields given \
                    replace the rule's, other keys (pin, follow, overrides, unknown keys) stay, `remove: true` deletes \
                    the rule. The rules are validated as the cook validates them before anything is written; with \
                    content = closure or any entry root, every root must resolve against the cooked registry \
                    (content_chunks_entry_unmatched otherwise). The write is one undo entry and the panel recomputes. \
                    editor.content_chunks_preview shows the same edit without saving.",
                params: crate::closure_settings::SET_RULE_PARAMS,
                result: "{ chunk, removed, saved, verified, content, preview?: { valid, shipped, unshipped_total, moves_total, moves, roots, unmatched, ... }, error }",
                mutating: true,
                permission: Permission::EditScene,
            },
            content_chunks_set_rule as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::registry::RegistryEntry;
    use regia_core::id::Guid;

    fn id(n: u128) -> AssetId {
        AssetId(Guid::from_u128(n))
    }

    /// A project with a cooked registry (no cooked files) and an `arena` chunk rule.
    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        let platform = regia_asset_pipeline::host_platform().to_string();
        let cache = regia_asset::cooked_cache_dir(&root, &platform);
        std::fs::create_dir_all(&cache).expect("mkdir");
        let rec = |n, path: &str, deps: &[u128], size| RegistryEntry {
            id: id(n),
            path: format!("assets/{path}"),
            type_path: "regia_asset::CpuMesh".into(),
            dependencies: deps.iter().map(|d| id(*d)).collect(),
            tags: Default::default(),
            size,
        };
        let registry = AssetRegistry::from_entries([
            rec(1, "levels/arena/main.level.ron", &[2], 100),
            rec(2, "arena/rock.mesh", &[], 4_000),
            rec(3, "ui/hud.png", &[], 50),
        ]);
        registry.write_to(&cache_registry_path(&cache)).expect("registry");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"t\"\nid = \"com.example.t\"\nversion = \"1.0.0\"\n\n\
             [cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\ndelivery = \"on_demand\"\n",
        )
        .expect("toml");
        root
    }

    fn world_with(root: Option<PathBuf>) -> World {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject { root });
        world
    }

    #[test]
    fn state_lists_chunks_selects_and_switches_bars() {
        let root = project("editor-chunks-state");
        let mut world = world_with(Some(root.clone()));
        let state = content_chunks_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["loaded"], true);
        let names: Vec<&str> = state["chunks"]
            .as_array()
            .expect("chunks")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(names, ["base", "arena"]);
        assert_eq!(state["chunks"][1]["delivery"], "on_demand");
        assert_eq!(state["chunks"][1]["memory_bytes"], 4_100);
        assert_eq!(state["chunks"][1]["disk_bytes"], 0, "no paks written yet");
        assert_eq!(state["show"], "disk");

        let state =
            content_chunks_state(&mut world, &json!({ "chunk": "arena", "show": "memory" })).expect("select");
        assert_eq!(state["selected"], "arena");
        assert_eq!(state["chunks"][1]["bar_bytes"], 4_100);
        assert_eq!(
            state["direct_assets"],
            json!([{ "path": "assets/levels/arena/main.level.ron", "selector": "levels/arena/**" }])
        );
        let err = content_chunks_state(&mut world, &json!({ "chunk": "nope" })).expect_err("bad chunk");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_bad_chunk")
        );
        let err = content_chunks_state(&mut world, &json!({ "show": "both" })).expect_err("bad show");
        assert_eq!(err.code, regia_agent::protocol::INVALID_PARAMS);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit G finding 6: the agent-controlled `platform` string is joined into cache and pak
    /// paths; it must stay a plain host tag (`^[a-z0-9_-]{1,32}$`) so `..`/`\`/`/`-bearing
    /// values are refused like every other agent path (ADR-1816 confinement).
    #[test]
    fn load_view_rejects_platform_with_separators() {
        // Corr editor-a finding L5: the panel's own platform field calls `load_view` directly;
        // the tag check used to run only on the agent path, so `../x` reached the cache path.
        let root = project("editor-chunks-load-view-platform");
        for bad in ["../x", "a/b", "..\\x", "x:y"] {
            let err = load_view(&root, bad).expect_err("refused");
            assert_eq!(
                err.code, "content_chunks_bad_platform",
                "{bad:?}: {}",
                err.message
            );
        }
    }

    #[test]
    fn content_chunks_state_rejects_platform_with_separators() {
        // Valid tags refresh the view, so the panel needs a project; bad tags are refused
        // before the root is touched.
        let root = project("editor-chunks-platform");
        let mut panel = ContentChunksPanel::default();
        for bad in [
            "../x",
            "..\\x",
            "a/b",
            "a\\b",
            "x:y",
            "UPPER",
            "has space",
            &"a".repeat(33),
        ] {
            let result = drive_state(&mut panel, None, &json!({ "platform": bad }));
            assert!(
                result.is_err(),
                "{bad:?} must be refused before it reaches a filesystem path"
            );
            assert_eq!(
                result.expect_err("refused").code,
                regia_agent::protocol::INVALID_PARAMS
            );
        }
        // The fixture only has the host platform's cook, so a valid tag is the host tag.
        let host = regia_asset_pipeline::host_platform().to_string();
        assert!(drive_state(&mut panel, Some(&root), &json!({ "platform": host })).is_ok());
        assert_eq!(panel.platform, host);
        assert!(drive_state(&mut panel, Some(&root), &json!({ "platform": "" })).is_ok());
        assert_eq!(panel.platform, "", "empty keeps the host-platform default");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn why_explains_the_chain_and_refuses_unknown_assets() {
        let root = project("editor-chunks-why");
        let mut world = world_with(Some(root.clone()));
        let answer = content_chunks_why(&mut world, &json!({ "asset": "arena/rock.mesh" })).expect("why");
        assert_eq!(answer["chunk"], "arena");
        assert_eq!(answer["reason"], "follows");
        assert_eq!(
            answer["via"],
            json!(["assets/arena/rock.mesh", "assets/levels/arena/main.level.ron"])
        );
        assert_eq!(answer["rule"]["selector"], "levels/arena/**");
        // The panel keeps the answer for its why box.
        let state = content_chunks_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["why"]["chunk"], "arena");
        let err =
            content_chunks_why(&mut world, &json!({ "asset": "assets/nope.png" })).expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_asset_not_found")
        );
        assert_eq!(
            content_chunks_why(&mut world, &json!({}))
                .expect_err("no asset")
                .code,
            regia_agent::protocol::INVALID_PARAMS
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Row C-03 (ADR-8092): with `content = "closure"` the panel uses the cook's startup list as an
    /// entry point, lists what ships in no chunk, and its why answer names an unshipped asset.
    #[test]
    fn closure_mode_lists_unshipped_assets_and_explains_them() {
        let root = project("editor-chunks-closure");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"t\"\nid = \"com.example.t\"\nversion = \"1.0.0\"\n\
             startup_level = \"levels/arena/main.level.ron\"\n\n\
             [cook.chunk_options]\ncontent = \"closure\"\n\n\
             [cook.chunks.arena]\ninclude = [\"levels/arena/**\"]\ndelivery = \"on_demand\"\n",
        )
        .expect("toml");
        let mut world = world_with(Some(root.clone()));
        let state = content_chunks_state(&mut world, &json!({})).expect("state");
        assert_eq!(state["content"], "closure");
        assert_eq!(state["unshipped"], json!(["assets/ui/hud.png"]));
        assert_eq!(state["unshipped_total"], 1);
        let rock = content_chunks_why(&mut world, &json!({ "asset": "arena/rock.mesh" })).expect("why");
        assert_eq!(
            (rock["chunk"].as_str(), rock["reason"].as_str()),
            (Some("base"), Some("pinned")),
            "the startup closure ships, pinned in base"
        );
        let err = content_chunks_why(&mut world, &json!({ "asset": "ui/hud.png" })).expect_err("unshipped");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_unshipped")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn size_map_and_references_open_on_a_chunk() {
        let root = project("editor-chunks-refs");
        let mut world = world_with(Some(root.clone()));
        let map = content_chunks_size_map(&mut world, &json!({ "chunk": "arena" })).expect("map");
        assert_eq!(map["count"], 2);
        assert_eq!(map["total_bytes"], 4_100);
        assert_eq!(
            map["assets"][0]["path"], "assets/arena/rock.mesh",
            "largest first"
        );
        assert_eq!(map["assets"][0]["type"], "CpuMesh");
        let one = content_chunks_size_map(&mut world, &json!({ "chunk": "arena", "limit": 1 })).expect("map");
        assert_eq!(one["assets"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["count"], 2, "the count is the chunk's, not the page's");
        let err = content_chunks_size_map(&mut world, &json!({ "chunk": "nope" })).expect_err("bad");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_bad_chunk")
        );

        let refs =
            content_chunks_references(&mut world, &json!({ "asset": "arena/rock.mesh" })).expect("refs");
        assert_eq!(refs["chunk"], "arena");
        assert_eq!(refs["dependencies"], json!([]));
        assert_eq!(
            refs["referrers"],
            json!([{ "path": "assets/levels/arena/main.level.ron", "chunk": "arena" }])
        );
        let err = content_chunks_references(&mut world, &json!({ "asset": "nope" })).expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_asset_not_found")
        );
        // Asking why fills the panel's references too.
        content_chunks_why(&mut world, &json!({ "asset": "levels/arena/main.level.ron" })).expect("why");
        let panel = world.resource::<ContentChunksPanel>();
        assert_eq!(panel.why_refs.as_ref().map(|r| r.dependencies.len()), Some(1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_rule_writes_regia_toml_validates_and_recomputes() {
        let root = project("editor-chunks-set-rule");
        let mut world = world_with(Some(root.clone()));
        let before = std::fs::read_to_string(root.join("Regia.toml")).expect("toml");
        let err = content_chunks_set_rule(&mut world, &json!({ "chunk": "arena", "delivery": "by_pigeon" }))
            .expect_err("bad delivery");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_rules")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Regia.toml")).expect("toml"),
            before,
            "nothing written"
        );

        let out = content_chunks_set_rule(
            &mut world,
            &json!({ "chunk": "hud", "include": ["ui/**"], "delivery": "fast_follow" }),
        )
        .expect("add");
        assert_eq!(out["saved"], true);
        let state = content_chunks_state(&mut world, &json!({})).expect("state");
        let names: Vec<&str> = state["chunks"]
            .as_array()
            .expect("chunks")
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(names.contains(&"hud"), "{names:?}");
        let map = content_chunks_size_map(&mut world, &json!({ "chunk": "hud" })).expect("map");
        assert_eq!(map["assets"][0]["path"], "assets/ui/hud.png");

        content_chunks_set_rule(&mut world, &json!({ "chunk": "hud", "remove": true })).expect("remove");
        let err = content_chunks_set_rule(&mut world, &json!({ "chunk": "hud", "remove": true }))
            .expect_err("gone");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_bad_chunk")
        );
        assert_eq!(
            content_chunks_set_rule(&mut world, &json!({ "chunk": "arena", "include": "x" }))
                .expect_err("type")
                .code,
            regia_agent::protocol::INVALID_PARAMS
        );
        let mut no_project = world_with(None);
        let err = content_chunks_set_rule(&mut no_project, &json!({ "chunk": "a" })).expect_err("none");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_no_project")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_project_or_a_cook_the_panel_says_so() {
        let mut world = world_with(None);
        let err = content_chunks_state(&mut world, &json!({})).expect_err("no project");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_no_project")
        );
        let empty = regia_test_utils::unique_temp_dir("editor-chunks-empty");
        let mut world = world_with(Some(empty.clone()));
        let err = content_chunks_state(&mut world, &json!({})).expect_err("no cook");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("content_chunks_no_cook")
        );
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// Bug 25 (ADR-8586): before the first cook the panel shows one plain sentence in the weak text
    /// color, not the agent error with its absolute cache path.
    #[test]
    fn an_uncooked_project_draws_a_plain_sentence() {
        let empty = regia_test_utils::unique_temp_dir("editor-chunks-not-cooked");
        let mut panel = ContentChunksPanel::default();
        panel.refresh(Some(empty.as_path()));
        assert!(panel.not_cooked, "{:?}", panel.error);
        let ctx = egui::Context::default();
        let mut texts = Vec::new();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 300.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| draw(ui, &mut panel, Some(empty.as_path())));
            });
            output.textures_delta.clear();
            texts.clear();
            for clipped in &output.shapes {
                if let egui::epaint::Shape::Text(text) = &clipped.shape {
                    let color = text
                        .galley
                        .job
                        .sections
                        .first()
                        .map_or(text.fallback_color, |s| s.format.color);
                    texts.push((text.galley.text().to_owned(), color));
                }
            }
        }
        let weak = ctx.global_style().visuals.weak_text_color();
        let line = texts.iter().find(|(t, _)| t == NOT_COOKED_TEXT);
        assert!(line.is_some(), "{texts:?}");
        assert!(
            !texts
                .iter()
                .any(|(t, _)| t.contains(&empty.display().to_string())),
            "no absolute path in the panel: {texts:?}"
        );
        assert_eq!(
            line.map(|(_, c)| *c),
            Some(weak),
            "a neutral color for a normal state"
        );
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// The names an agent calls reach these handlers: the tests above call the functions directly.
    #[test]
    fn every_method_name_reaches_its_handler() {
        let methods = methods();
        let by_name = |name: &str| {
            methods
                .iter()
                .find(|(schema, _)| schema.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"))
        };
        let root = project("names");
        let mut world = world_with(Some(root.clone()));
        let call = |world: &mut World, name: &str, params: Value| (by_name(name).1)(world, &params);
        let state = call(&mut world, "editor.content_chunks_state", json!({})).expect("state");
        let chunk = state["chunks"][0]["name"].as_str().expect("a chunk").to_string();
        call(
            &mut world,
            "editor.content_chunks_size_map",
            json!({ "chunk": chunk }),
        )
        .expect("size map");
        for name in ["editor.content_chunks_why", "editor.content_chunks_references"] {
            let err = call(&mut world, name, json!({ "asset": "no/such/asset" })).expect_err(name);
            assert!(
                err.data.as_ref().and_then(|d| d["code"].as_str()).is_some(),
                "{name}: {err:?}"
            );
        }
        let err = call(&mut world, "editor.content_chunks_set_rule", json!({})).expect_err("no chunk");
        assert!(
            err.data.as_ref().and_then(|d| d["code"].as_str()).is_some(),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_registry_keeps_its_code_through_the_agent_report() {
        let root = project("editor-chunks-integrity-registry");
        let cache = regia_asset::cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
        let registry = cache_registry_path(&cache);
        let tmp = registry.with_extension("tmp");
        std::fs::write(&tmp, b"broken registry").expect("fixture");
        std::fs::rename(tmp, &registry).expect("publish fixture");
        let mut world = world_with(Some(root.clone()));
        for params in [
            json!({"refresh": true}),
            json!({"refresh": true, "chunk": "base"}),
        ] {
            let error = content_chunks_state(&mut world, &params).expect_err("corrupt registry");
            assert_eq!(
                error.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("content_registry_invalid")
            );
            assert!(error.message.contains("asset_registry.bin"), "{error:?}");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
