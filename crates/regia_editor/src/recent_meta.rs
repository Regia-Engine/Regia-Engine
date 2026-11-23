// SPDX-License-Identifier: MPL-2.0
//! What the project browser knows about each recent project beyond its path: user-assigned tags,
//! when it was last opened, which engine version opened it, and a thumbnail
//! (`book/agents/project-browser.md`).
//!
//! `recent_projects.txt` is a plain list of paths with no room for more, so this lives in a
//! sidecar next to it: `recent_projects_meta.ron`, version 1, keyed by project path. A missing,
//! unreadable or newer-than-known sidecar reads as empty; a newer one is never overwritten. The
//! browser filters the list by a text search (`tag:name` terms match tags) and by chosen tags.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::World;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::project::{load_recents, recent_file};

/// File name of the sidecar, next to `recent_projects.txt`.
pub const META_FILE: &str = "recent_projects_meta.ron";

/// Current version of the sidecar.
pub const META_VERSION: u32 = 1;

/// Most tags on one project.
pub const MAX_TAGS: usize = 16;

/// Longest tag, in characters.
pub const MAX_TAG_LEN: usize = 32;

/// What the sidecar keeps for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectMeta {
    /// User-assigned tags: lowercase, no spaces or commas, sorted.
    pub tags: Vec<String>,
    /// When the editor last opened the project, Unix seconds (0 = unknown).
    pub last_opened: u64,
    /// The engine version of the editor that last opened it (empty = unknown).
    pub engine_version: String,
}

/// The sidecar file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentMeta {
    /// Format version ([`META_VERSION`]).
    pub version: u32,
    /// Per project, keyed by the project path as written in `recent_projects.txt`.
    pub projects: BTreeMap<String, ProjectMeta>,
}

impl Default for RecentMeta {
    fn default() -> Self {
        Self {
            version: META_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

/// A refused edit: a lowercase code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaError {
    /// The agent error code (`recent_not_found`, `recent_tag_invalid`, ...).
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl MetaError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn rpc(&self) -> RpcError {
        let status = if self.code == "recent_not_found" {
            NOT_FOUND
        } else {
            INVALID_PARAMS
        };
        RpcError::new(
            status,
            self.code,
            self.message.clone(),
            "list the recent projects with editor.recent_projects",
        )
    }
}

/// Where the sidecar lives: beside the recent-projects file.
#[must_use]
pub fn meta_file() -> PathBuf {
    recent_file().with_file_name(META_FILE)
}

/// Reads the sidecar.
///
/// # Errors
///
/// `recent_meta_unsupported` when the file is a newer version than this editor knows, and
/// `recent_meta_unreadable` when it exists but cannot be read or does not parse (torn, or edited
/// by hand). Either way a writer leaves the file alone, so the tags in it are never lost. A
/// missing file is not an error: it reads as empty. Readers that only show the list use
/// `unwrap_or_default` and show no tags.
pub fn load_meta() -> Result<RecentMeta, MetaError> {
    let file = meta_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(RecentMeta::default()),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{}: {err}", file.display());
            return Err(MetaError::new(
                "recent_meta_unreadable",
                format!("cannot read {}: {err}", file.display()),
            ));
        }
    };
    match ron::from_str::<RecentMeta>(&text) {
        Ok(meta) if meta.version > META_VERSION => Err(MetaError::new(
            "recent_meta_unsupported",
            format!(
                "{} is version {}; this editor reads up to {META_VERSION}",
                file.display(),
                meta.version
            ),
        )),
        Ok(meta) => Ok(meta),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{}: {err}", file.display());
            Err(MetaError::new(
                "recent_meta_unreadable",
                format!(
                    "{} does not parse ({err}); fix or delete it to edit tags again",
                    file.display()
                ),
            ))
        }
    }
}

fn save_meta(meta: &RecentMeta) -> Result<(), MetaError> {
    let file = meta_file();
    let text = ron::ser::to_string_pretty(meta, regia_core::ron_pretty())
        .map_err(|err| MetaError::new("recent_write_failed", err.to_string()))?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| MetaError::new("recent_write_failed", format!("{}: {err}", parent.display())))?;
    }
    crate::atomic_write::write_atomic(&file, text.as_bytes())
        .map_err(|err| MetaError::new("recent_write_failed", format!("{}: {err}", file.display())))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Records that the editor just opened `root`: the time and the engine version, tags kept. Entries
/// for projects no longer in the recent list are dropped. Best effort: a failure is logged.
pub fn touch_opened(root: &Path) {
    let mut meta = match load_meta() {
        Ok(meta) => meta,
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{}", err.message);
            return;
        }
    };
    let keep: Vec<String> = load_recents().iter().map(|p| p.display().to_string()).collect();
    meta.projects.retain(|path, _| keep.contains(path));
    let entry = meta.projects.entry(root.display().to_string()).or_default();
    entry.last_opened = now_secs();
    entry.engine_version = regia_core::engine_version::CURRENT.to_string();
    if let Err(err) = save_meta(&meta) {
        tracing::warn!(target: "regia_editor", "{}", err.message);
    }
}

/// Normalizes one tag: trimmed, lowercase, runs of whitespace become `-`. `None` for an empty one.
///
/// # Errors
///
/// `recent_tag_invalid` for a tag over [`MAX_TAG_LEN`] characters or holding a comma, a colon or
/// a control character.
pub fn normalize_tag(raw: &str) -> Result<Option<String>, MetaError> {
    let joined = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if joined.is_empty() {
        return Ok(None);
    }
    if joined.chars().count() > MAX_TAG_LEN {
        return Err(MetaError::new(
            "recent_tag_invalid",
            format!("a tag is at most {MAX_TAG_LEN} characters"),
        ));
    }
    if joined.chars().any(|c| c == ',' || c == ':' || c.is_control()) {
        return Err(MetaError::new(
            "recent_tag_invalid",
            "a tag cannot hold a comma, a colon or a control character",
        ));
    }
    Ok(Some(joined))
}

/// Normalizes a tag list: each tag through [`normalize_tag`], duplicates and empties dropped,
/// sorted.
///
/// # Errors
///
/// A bad tag, or more than [`MAX_TAGS`] of them (`recent_tag_invalid`).
pub fn normalize_tags(raw: &[String]) -> Result<Vec<String>, MetaError> {
    let mut tags = std::collections::BTreeSet::new();
    for tag in raw {
        if let Some(tag) = normalize_tag(tag)? {
            tags.insert(tag);
        }
    }
    if tags.len() > MAX_TAGS {
        return Err(MetaError::new(
            "recent_tag_invalid",
            format!("a project holds at most {MAX_TAGS} tags"),
        ));
    }
    Ok(tags.into_iter().collect())
}

/// The listed recent project at `path` (as written, or canonicalized).
fn find_recent(path: &Path) -> Option<PathBuf> {
    let recents = load_recents();
    let canonical = path.canonicalize().ok();
    recents
        .into_iter()
        .find(|recent| recent == path || canonical.as_ref().is_some_and(|c| c == recent))
}

/// Replaces the tags of the recent project `path`.
///
/// # Errors
///
/// `recent_not_found` (not in the recent list), `recent_tag_invalid`, `recent_meta_unsupported`,
/// `recent_meta_unreadable` or `recent_write_failed`.
pub fn set_tags(path: &Path, tags: &[String]) -> Result<Vec<String>, MetaError> {
    let recent = find_recent(path).ok_or_else(|| {
        MetaError::new(
            "recent_not_found",
            format!("{} is not a recent project", path.display()),
        )
    })?;
    let tags = normalize_tags(tags)?;
    let mut meta = load_meta()?;
    meta.projects
        .entry(recent.display().to_string())
        .or_default()
        .tags = tags.clone();
    save_meta(&meta)?;
    Ok(tags)
}

/// Takes `path` off the recent list (the project folder is untouched) and forgets its metadata.
/// Returns whether it was listed.
///
/// # Errors
///
/// `recent_meta_unsupported`, `recent_meta_unreadable` (the list is left as it was too) or
/// `recent_write_failed`.
pub fn remove_recent(path: &Path) -> Result<bool, MetaError> {
    let Some(recent) = find_recent(path) else {
        return Ok(false);
    };
    let mut meta = load_meta()?;
    meta.projects.remove(&recent.display().to_string());
    let list: Vec<PathBuf> = load_recents().into_iter().filter(|p| *p != recent).collect();
    crate::project::write_recents(&list).map_err(|err| MetaError::new("recent_write_failed", err))?;
    save_meta(&meta)?;
    Ok(true)
}

/// One row of the browser's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentEntry {
    /// The project root.
    pub path: PathBuf,
    /// `[project] name` from the manifest, else the folder name.
    pub name: String,
    /// The user's tags.
    pub tags: Vec<String>,
    /// Last opened, Unix seconds.
    pub last_opened: Option<u64>,
    /// The engine version that last opened it.
    pub engine_version: Option<String>,
    /// The picture the row shows ([`thumbnail_source`]).
    pub thumbnail: Option<PathBuf>,
}

/// `[project] name` of a project's manifest.
fn manifest_name(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(regia_core::brand::project_manifest(root)?).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("project")?
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The recent projects with their metadata, newest first.
#[must_use]
pub fn entries() -> Vec<RecentEntry> {
    let meta = load_meta().unwrap_or_default();
    load_recents()
        .into_iter()
        .map(|path| {
            let kept = meta.projects.get(&path.display().to_string());
            let name = manifest_name(&path).unwrap_or_else(|| {
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            });
            RecentEntry {
                name,
                tags: kept.map(|m| m.tags.clone()).unwrap_or_default(),
                last_opened: kept.map(|m| m.last_opened).filter(|t| *t > 0),
                engine_version: kept.map(|m| m.engine_version.clone()).filter(|v| !v.is_empty()),
                thumbnail: thumbnail_source(&path),
                path,
            }
        })
        .collect()
}

/// Every tag in use, sorted.
#[must_use]
pub fn all_tags(entries: &[RecentEntry]) -> Vec<String> {
    let set: std::collections::BTreeSet<&String> = entries.iter().flat_map(|e| e.tags.iter()).collect();
    set.into_iter().cloned().collect()
}

/// A parsed search: words that must appear in the name or path, and tags the project must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Lowercase words; each must be a substring of the name or the path.
    pub words: Vec<String>,
    /// Lowercase tags; the project must hold every one.
    pub tags: Vec<String>,
}

impl Filter {
    /// Parses a search box: whitespace-separated terms; `tag:name` requires a tag, any other term
    /// is a word. `extra_tags` are the tags chosen by the chips.
    #[must_use]
    pub fn parse(query: &str, extra_tags: &[String]) -> Self {
        let mut filter = Self::default();
        for term in query.split_whitespace() {
            let lower = term.to_lowercase();
            if let Some(tag) = lower.strip_prefix("tag:") {
                if !tag.is_empty() {
                    filter.tags.push(tag.to_owned());
                }
            } else {
                filter.words.push(lower);
            }
        }
        filter.tags.extend(extra_tags.iter().map(|t| t.to_lowercase()));
        filter
    }

    /// Whether `entry` passes.
    #[must_use]
    pub fn matches(&self, entry: &RecentEntry) -> bool {
        let haystack = format!("{} {}", entry.name, entry.path.display()).to_lowercase();
        self.words.iter().all(|word| haystack.contains(word))
            && self.tags.iter().all(|tag| entry.tags.iter().any(|t| t == tag))
    }
}

/// The entries that pass `filter`, in order.
#[must_use]
pub fn filtered(entries: &[RecentEntry], filter: &Filter) -> Vec<RecentEntry> {
    entries.iter().filter(|e| filter.matches(e)).cloned().collect()
}

// ---------------------------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------------------------

/// File name of the viewport capture saved when a project closes, under the project state
/// directory.
pub const VIEWPORT_THUMBNAIL: &str = "project_thumbnail.png";

/// The picture a project's row shows: the manifest's `[project] icon` (a path under the project),
/// else `icon.png` at the root, else the last viewport capture the editor saved at close
/// ([`save_viewport_thumbnail`]). `None` when none exists.
#[must_use]
pub fn thumbnail_source(root: &Path) -> Option<PathBuf> {
    let icon = regia_core::brand::project_manifest(root)
        .and_then(|manifest| std::fs::read_to_string(manifest).ok())
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| value.get("project")?.get("icon")?.as_str().map(str::to_owned));
    let named = icon.filter(|rel| {
        !Path::new(rel)
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    });
    [
        named.map(|rel| root.join(rel)),
        Some(root.join("icon.png")),
        Some(regia_core::brand::project_state_dir(root).join(VIEWPORT_THUMBNAIL)),
    ]
    .into_iter()
    .flatten()
    .find(|path| path.is_file())
}

/// Side of the stored viewport capture, pixels.
pub const THUMBNAIL_PX: u32 = 128;

/// Saves a viewport capture as the project's thumbnail (a [`THUMBNAIL_PX`] square, aspect kept,
/// atomic write). The shell calls it with the last viewport frame when a project closes.
///
/// # Errors
///
/// The picture is empty or the file cannot be written.
pub fn save_viewport_thumbnail(root: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<PathBuf, String> {
    use image::ImageEncoder as _;
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4));
    if width == 0 || height == 0 || expected != Some(rgba.len()) {
        return Err(format!(
            "a {width}x{height} RGBA picture needs {expected:?} bytes"
        ));
    }
    let source = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "the picture does not fit its size".to_owned())?;
    let (fit_w, fit_h) = crate::thumb_cache::fit_size(width, height, THUMBNAIL_PX);
    let small = image::imageops::thumbnail(&source, fit_w, fit_h);
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(small.as_raw(), fit_w, fit_h, image::ExtendedColorType::Rgba8)
        .map_err(|err| format!("png encode: {err}"))?;
    let file = regia_core::brand::project_state_dir(root).join(VIEWPORT_THUMBNAIL);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    crate::atomic_write::write_atomic(&file, &bytes)?;
    Ok(file)
}

// ---------------------------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------------------------

/// `YYYY-MM-DD` (UTC) of Unix seconds. Civil-from-days conversion of the proleptic Gregorian
/// calendar (public-domain algorithm, `docs/provenance.md`).
#[must_use]
pub fn format_date(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

// ---------------------------------------------------------------------------------------------
// Show in file manager
// ---------------------------------------------------------------------------------------------

/// The program that shows a folder in the OS file manager.
#[must_use]
pub fn file_manager_program() -> &'static str {
    if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Shows `path` in the OS file manager. Failure is logged, not an error.
pub fn show_in_file_manager(path: &Path) {
    if std::process::Command::new(file_manager_program())
        .arg(path)
        .spawn()
        .is_err()
    {
        tracing::warn!(target: "regia_editor", "cannot open a file manager at {}", path.display());
    }
}

// ---------------------------------------------------------------------------------------------
// Browser list UI
// ---------------------------------------------------------------------------------------------

/// The list's per-window state, kept in egui's memory so the browser's public state is unchanged.
#[derive(Debug, Clone, Default)]
struct ListUi {
    query: String,
    tags: Vec<String>,
    selected: Option<PathBuf>,
    tag_input: String,
    /// The frame whose click on a tag chip was already applied (egui can run a panel twice a frame).
    toggled_frame: u64,
    /// The frame whose Show in file manager click already ran.
    revealed_frame: u64,
    status: String,
}

const THUMB_POINTS: f32 = 56.0;

/// A decoded thumbnail kept in egui's memory; `None` is a picture that did not decode.
#[derive(Clone)]
struct ThumbTexture(Option<egui::TextureHandle>);

fn thumbnail_texture(ctx: &egui::Context, source: &Path) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("project-thumb", source));
    if let Some(ThumbTexture(texture)) = ctx.data(|d| d.get_temp::<ThumbTexture>(id)) {
        return texture;
    }
    let texture = crate::thumb_cache::make_texture_thumb(source, 128)
        .ok()
        .map(|rgba| {
            let image = egui::ColorImage::from_rgba_unmultiplied([128, 128], &rgba);
            ctx.load_texture(
                format!("project-thumb:{}", source.display()),
                image,
                egui::TextureOptions::LINEAR,
            )
        });
    ctx.data_mut(|d| d.insert_temp(id, ThumbTexture(texture.clone())));
    texture
}

/// The browser's Recent section: search box, tag chips and one row per project (thumbnail, name,
/// path, last opened and engine version, tags, Show in file manager, Remove from list). Returns the
/// project to open. Keys: Up and Down move the highlighted row, Enter opens it (from the search box
/// or when nothing else has focus); every control is also reachable with Tab.
pub fn recent_section(ui: &mut egui::Ui) -> Option<PathBuf> {
    let id = egui::Id::new("project-browser-list");
    let mut state = ui.ctx().data(|d| d.get_temp::<ListUi>(id)).unwrap_or_default();
    let frame = ui.ctx().cumulative_frame_nr();
    let all = entries();
    let mut open = None;
    if all.is_empty() {
        ui.weak("No recent projects.");
        return None;
    }
    ui.label("Recent");
    let search =
        ui.add(egui::TextEdit::singleline(&mut state.query).hint_text("Search projects, or tag:name"));
    crate::a11y_audit::name(
        search.clone(),
        egui::WidgetType::TextEdit,
        "Search recent projects",
    );
    let tags = all_tags(&all);
    if !tags.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Tags");
            for tag in &tags {
                let on = state.tags.contains(tag);
                let chip = ui.selectable_label(on, format!("#{tag}"));
                let chip =
                    crate::a11y_audit::name(chip, egui::WidgetType::Button, &format!("Filter by tag {tag}"));
                if chip.clicked() && state.toggled_frame != frame {
                    state.toggled_frame = frame;
                    if on {
                        state.tags.retain(|t| t != tag);
                    } else {
                        state.tags.push(tag.clone());
                    }
                }
            }
        });
        // A chosen tag whose last project lost it must not filter everything away.
        state.tags.retain(|t| tags.contains(t));
    }
    let filter = Filter::parse(&state.query, &state.tags);
    let shown = filtered(&all, &filter);
    if shown.is_empty() {
        ui.weak("No project matches.");
    }
    // Keyboard: Up and Down move the highlight; Enter opens it.
    let nothing_focused = ui.memory(|m| m.focused().is_none());
    let (steps, enter) = ui.input_mut(|input| {
        (
            crate::level_templates::arrow_steps(input),
            input.key_pressed(egui::Key::Enter),
        )
    });
    let at = shown
        .iter()
        .position(|entry| state.selected.as_ref() == Some(&entry.path));
    if steps != 0 && !shown.is_empty() {
        let last = shown.len() as i64 - 1;
        let from = at.map_or(if steps > 0 { -1 } else { last + 1 }, |i| i as i64);
        let next = (from + i64::from(steps)).clamp(0, last) as usize;
        state.selected = Some(shown[next].path.clone());
    }
    if enter
        && (search.has_focus() || nothing_focused)
        && let Some(entry) = shown
            .iter()
            .find(|entry| state.selected.as_ref() == Some(&entry.path))
    {
        open = Some(entry.path.clone());
    }
    let mut remove = None;
    let mut retag: Option<(PathBuf, Vec<String>)> = None;
    for entry in &shown {
        let selected = state.selected.as_ref() == Some(&entry.path);
        let mut frame_style = egui::Frame::group(ui.style());
        if selected {
            frame_style = frame_style.stroke(ui.visuals().selection.stroke);
        }
        frame_style.show(ui, |ui| {
            ui.horizontal(|ui| {
                match entry
                    .thumbnail
                    .as_deref()
                    .and_then(|src| thumbnail_texture(ui.ctx(), src))
                {
                    Some(texture) => {
                        ui.add(
                            egui::Image::new(&texture)
                                .fit_to_exact_size(egui::vec2(THUMB_POINTS, THUMB_POINTS))
                                .alt_text(format!("Thumbnail of {}", entry.name)),
                        );
                    }
                    None => {
                        let initial = entry
                            .name
                            .chars()
                            .next()
                            .unwrap_or('?')
                            .to_uppercase()
                            .to_string();
                        ui.add_sized(
                            [THUMB_POINTS, THUMB_POINTS],
                            egui::Label::new(egui::RichText::new(initial).heading().weak()),
                        );
                    }
                }
                ui.vertical(|ui| {
                    if ui.selectable_label(selected, &entry.name).clicked() {
                        state.selected = Some(entry.path.clone());
                    }
                    if ui.button(entry.path.display().to_string()).clicked() {
                        open = Some(entry.path.clone());
                    }
                    let mut facts = Vec::new();
                    if let Some(when) = entry.last_opened {
                        facts.push(format!("Last opened {}", format_date(when)));
                    }
                    if let Some(version) = &entry.engine_version {
                        facts.push(format!("Engine {version}"));
                    }
                    if !facts.is_empty() {
                        ui.weak(facts.join(" · "));
                    }
                    ui.horizontal_wrapped(|ui| {
                        for tag in &entry.tags {
                            ui.label(format!("#{tag}"));
                            let drop = crate::a11y_audit::glyph_button(
                                ui,
                                "\u{1f5d9}",
                                &format!("Remove tag {tag} from {}", entry.name),
                            );
                            if drop.clicked() {
                                let rest = entry.tags.iter().filter(|t| *t != tag).cloned().collect();
                                retag = Some((entry.path.clone(), rest));
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        let reveal = ui.button("Show in file manager");
                        let reveal = crate::a11y_audit::name(
                            reveal,
                            egui::WidgetType::Button,
                            &format!("Show {} in file manager", entry.name),
                        );
                        if reveal.clicked() && state.revealed_frame != frame {
                            state.revealed_frame = frame;
                            show_in_file_manager(&entry.path);
                        }
                        let gone = ui.button("Remove from list");
                        let gone = crate::a11y_audit::name(
                            gone,
                            egui::WidgetType::Button,
                            &format!("Remove {} from the recent list", entry.name),
                        );
                        if gone.clicked() {
                            remove = Some(entry.path.clone());
                        }
                    });
                    if selected {
                        ui.horizontal(|ui| {
                            let field = ui
                                .add(egui::TextEdit::singleline(&mut state.tag_input).hint_text("Add a tag"));
                            crate::a11y_audit::name(
                                field.clone(),
                                egui::WidgetType::TextEdit,
                                &format!("New tag for {}", entry.name),
                            );
                            let add = ui.button("Add tag").clicked()
                                || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                            if add && !state.tag_input.trim().is_empty() {
                                let mut next = entry.tags.clone();
                                next.push(std::mem::take(&mut state.tag_input));
                                retag = Some((entry.path.clone(), next));
                            }
                        });
                    }
                });
            });
        });
    }
    if let Some((path, tags)) = retag {
        match set_tags(&path, &tags) {
            Ok(_) => state.status.clear(),
            Err(err) => state.status = err.message,
        }
    }
    if let Some(path) = remove {
        match remove_recent(&path) {
            Ok(_) => {
                if state.selected.as_ref() == Some(&path) {
                    state.selected = None;
                }
                state.status.clear();
            }
            Err(err) => state.status = err.message,
        }
    }
    if !state.status.is_empty() {
        ui.colored_label(ui.visuals().error_fg_color, &state.status);
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, state));
    open
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn entry_json(entry: &RecentEntry) -> Value {
    json!({
        "path": entry.path.display().to_string(),
        "name": entry.name,
        "tags": entry.tags,
        "last_opened": entry.last_opened,
        "engine_version": entry.engine_version,
        "thumbnail": entry.thumbnail.as_ref().map(|p| p.display().to_string()),
    })
}

/// `editor.recent_projects { query?, tag? }`: the recent projects, filtered like the browser.
///
/// # Errors
///
/// `query` or `tag` that is not a string.
pub fn recent_projects_json(params: &Value) -> Result<Value, RpcError> {
    let text = |key: &str| -> Result<Option<String>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a string"))),
        }
    };
    let query = text("query")?.unwrap_or_default();
    let tags: Vec<String> = text("tag")?.into_iter().collect();
    let all = entries();
    let shown = filtered(&all, &Filter::parse(&query, &tags));
    Ok(json!({
        "projects": shown.iter().map(|e| e.path.display().to_string()).collect::<Vec<_>>(),
        "entries": shown.iter().map(entry_json).collect::<Vec<_>>(),
        "tags": all_tags(&all),
    }))
}

/// `editor.recent_projects_edit { path, tags?, remove? }`.
fn editor_recent_projects_edit(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (a recent project) is required"))?;
    let remove = match params.get("remove") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(RpcError::invalid_params("`remove` must be a boolean")),
    };
    let tags = match params.get("tags") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| RpcError::invalid_params("`tags` holds strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(RpcError::invalid_params("`tags` must be an array of strings")),
    };
    if remove && tags.is_some() {
        return Err(RpcError::invalid_params("pass `tags` or `remove`, not both"));
    }
    let path = Path::new(path);
    if remove {
        let removed = remove_recent(path).map_err(|e| e.rpc())?;
        if !removed {
            return Err(MetaError::new(
                "recent_not_found",
                format!("{} is not a recent project", path.display()),
            )
            .rpc());
        }
        return Ok(json!({ "removed": true }));
    }
    let tags = tags.ok_or_else(|| RpcError::invalid_params("pass `tags` or `remove: true`"))?;
    let tags = set_tags(path, &tags).map_err(|e| e.rpc())?;
    Ok(json!({ "tags": tags }))
}

/// The recent-project agent methods other than `editor.recent_projects` (which is in
/// `agent_ext`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.recent_projects_edit",
            tool: "recent_projects_edit",
            description: "Set the tags of a recent project, or take it off the recent list (the folder is untouched), as the project browser does.",
            params: &[
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "A project directory from editor.recent_projects.",
                },
                ParamSchema {
                    name: "tags",
                    kind: "array",
                    required: false,
                    description: "The project's whole tag list (lowercase, at most 16 tags of 32 characters); replaces the old list.",
                },
                ParamSchema {
                    name: "remove",
                    kind: "boolean",
                    required: false,
                    description: "true takes the project off the list (not with tags).",
                },
            ],
            result: "{ tags } or { removed }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_recent_projects_edit as Handler,
    )]
}
