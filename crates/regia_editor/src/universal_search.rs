// SPDX-License-Identifier: MPL-2.0
//! Universal search (P-019, P-020, ADR-7766): one window (Ctrl+T, rebindable as the
//! `search.everything` palette command, which is also the palette's first row) that searches
//! assets, scene entities, palette commands (menu items included), settings and agent methods.
//!
//! Results are grouped by [`Provider`], each group with its match count. Prefix tokens limit the
//! search to providers: `a:` assets, `e:` entities, `c:` commands, `s:` settings, `m:` agent
//! methods (`e: lamp`, or attached: `e:lamp`); the window's provider toggles do the same. Filter
//! tokens go to the providers that understand them: `type:`/`t:`, `ext:` and `-word` to assets
//! (the Content Browser's query, `crate::asset_search`), `kind:`, `tag:` and `type:<Component>` to
//! entities (the Outliner's query). Commands, settings and methods match plain words only, so a
//! query with filter tokens lists none of them.
//!
//! Moving through the results previews the item: an entity is selected and framed in the
//! viewport (no undo entry, and the selection history is paused), an asset shows its path, type
//! and size, a setting its description and current value. Enter commits (opens the asset, keeps
//! the entity selected, runs the command, opens the setting's window); Escape restores the
//! selection and camera from before the preview and closes the window. Tab completes the token
//! under the cursor (`crate::asset_search::complete_token`).
//!
//! Work is bounded per frame: a [`SearchRun`] examines at most [`PROVIDER_BUDGET`] candidates per
//! editor frame across its providers and keeps the best [`GROUP_KEEP`] hits per provider; each
//! examined candidate is one counted step ([`SearchRun::steps`]). A finished run costs nothing
//! per frame. The agent surface is `editor.search` (`book/agents/universal-search.md`); saved
//! searches are `crate::saved_searches`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::fuzzy::fuzzy_match;
use serde_json::{Value, json};

use crate::asset_search::{AssetQuery, search_key};
use crate::content_index::{ContentIndex, IndexedAsset};
use crate::kind_icons::SearchQuery;
use crate::selection::{Selection, SelectionHistory, SelectionSnapshot};
use crate::viewport::EditorCamera;
use crate::viewport_agent::Handler;

/// Candidates a search examines per editor frame, across its providers.
pub const PROVIDER_BUDGET: usize = 2048;
/// Best hits kept per provider (the count still covers every match).
pub const GROUP_KEEP: usize = 50;
/// Rows the window shows per provider.
pub const WINDOW_GROUP_ROWS: usize = 8;
/// Project settings keys read from `Regia.toml`, at most.
const MAX_PROJECT_KEYS: usize = 512;

/// One source of results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    /// Project assets (the Content Browser's index).
    Assets,
    /// Entities of the open level (name, kind, tag).
    Entities,
    /// Palette commands and menu items.
    Commands,
    /// Preferences and project settings keys.
    Settings,
    /// Editor agent methods.
    Methods,
}

impl Provider {
    /// Every provider, in result order.
    pub const ALL: [Self; 5] = [
        Self::Assets,
        Self::Entities,
        Self::Commands,
        Self::Settings,
        Self::Methods,
    ];

    /// The protocol id: `assets`, `entities`, `commands`, `settings`, `methods`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Assets => "assets",
            Self::Entities => "entities",
            Self::Commands => "commands",
            Self::Settings => "settings",
            Self::Methods => "methods",
        }
    }

    /// The group heading.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Assets => "Assets",
            Self::Entities => "Entities",
            Self::Commands => "Commands",
            Self::Settings => "Settings",
            Self::Methods => "Agent methods",
        }
    }

    /// The prefix token that limits a query to this provider.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Assets => "a:",
            Self::Entities => "e:",
            Self::Commands => "c:",
            Self::Settings => "s:",
            Self::Methods => "m:",
        }
    }

    /// Parses a protocol id.
    #[must_use]
    pub fn from_id(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == text)
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A query split into what each provider reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Providers named by prefix tokens (empty: no limit).
    pub only: Vec<Provider>,
    /// Plain words, lowercase.
    pub words: Vec<String>,
    /// The tokens the asset provider reads (plain words and asset filters).
    pub asset_text: String,
    /// The tokens the entity provider reads (plain words and entity filters).
    pub entity_text: String,
    /// The query has `t:`, `ext:` or `-word` tokens (assets only).
    pub asset_filters: bool,
    /// The query has `kind:` or `tag:` tokens (entities only).
    pub entity_filters: bool,
    /// The query has a `type:` token (assets read an asset type, entities a component).
    pub type_filter: bool,
}

impl ParsedQuery {
    /// Splits `text` (see the module docs).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut parsed = Self::default();
        let mut asset = Vec::new();
        let mut entity = Vec::new();
        for token in text.split_whitespace() {
            let lower = token.to_lowercase();
            let mut rest = lower.as_str();
            if let Some(provider) = Provider::ALL.into_iter().find(|p| lower.starts_with(p.prefix())) {
                if !parsed.only.contains(&provider) {
                    parsed.only.push(provider);
                }
                rest = &lower[provider.prefix().len()..];
                if rest.is_empty() {
                    continue;
                }
            }
            if rest.starts_with("type:") {
                parsed.type_filter = true;
                asset.push(rest.to_owned());
                entity.push(rest.to_owned());
            } else if rest.starts_with("t:")
                || rest.starts_with("ext:")
                || (rest.starts_with('-') && rest.len() > 1)
            {
                parsed.asset_filters = true;
                asset.push(rest.to_owned());
            } else if rest.starts_with("kind:") || rest.starts_with("tag:") {
                parsed.entity_filters = true;
                entity.push(rest.to_owned());
            } else {
                parsed.words.push(rest.to_owned());
                asset.push(rest.to_owned());
                entity.push(rest.to_owned());
            }
        }
        parsed.asset_text = asset.join(" ");
        parsed.entity_text = entity.join(" ");
        parsed
    }

    /// Whether the query has no words and no filters (prefix tokens alone list a provider).
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.words.is_empty() && !self.asset_filters && !self.entity_filters && !self.type_filter
    }

    fn has_filters(&self) -> bool {
        self.asset_filters || self.entity_filters || self.type_filter
    }
}

/// What a result points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchItem {
    /// A project-relative asset path.
    Asset(String),
    /// A level entity.
    Entity(Entity),
    /// A palette entry id (`crate::command_palette::entries`).
    Command(&'static str),
    /// A preference key, or a project settings key (`project`).
    Setting {
        /// The dotted key.
        key: String,
        /// Whether it is a `Regia.toml` key (else a preference).
        project: bool,
    },
    /// An agent method name.
    Method(&'static str),
}

impl SearchItem {
    /// The protocol object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Asset(path) => json!({ "type": "asset", "path": path }),
            Self::Entity(e) => json!({ "type": "entity", "entity": e.to_bits() }),
            Self::Command(id) => json!({ "type": "command", "id": id }),
            Self::Setting { key, project } => {
                json!({ "type": "setting", "key": key, "scope": if *project { "project" } else { "preferences" } })
            }
            Self::Method(name) => json!({ "type": "method", "name": name }),
        }
    }
}

/// One result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// What the row shows.
    pub label: String,
    /// A second line (path, menu path, key).
    pub detail: String,
    /// What it points at.
    pub item: SearchItem,
    /// Higher is better.
    pub score: i32,
}

/// The results of one provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderGroup {
    /// The provider.
    pub provider: Provider,
    /// The best hits, best first (at most [`GROUP_KEEP`] once done).
    pub hits: Vec<Hit>,
    /// Every match so far.
    pub total: usize,
    /// Whether every candidate was examined.
    pub done: bool,
    /// Why the provider gave nothing (a query error, no project), if it says.
    pub note: Option<String>,
}

/// The project settings keys of `Regia.toml` (dotted, sorted, at most 512).
#[must_use]
pub fn project_setting_keys(root: &Path) -> Vec<String> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<String>) {
        for (key, value) in table {
            if out.len() >= MAX_PROJECT_KEYS {
                return;
            }
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&path, inner, out),
                _ => out.push(path),
            }
        }
    }
    let Ok(text) = std::fs::read_to_string(root.join("Regia.toml")) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk("", &table, &mut out);
    out.sort();
    out
}

/// The editor's agent methods: `(name, description)`, sorted by name.
fn method_list() -> &'static [(&'static str, &'static str)] {
    static METHODS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    METHODS.get_or_init(|| {
        let mut all: Vec<(&'static str, &'static str)> = crate::agent_ext::all_methods()
            .iter()
            .map(|(schema, _)| (schema.name, schema.description))
            .collect();
        all.sort_unstable();
        all.dedup_by(|a, b| a.0 == b.0);
        all
    })
}

/// What a run searches, copied when it starts.
#[derive(Debug, Clone, Default)]
struct Sources {
    entities: Vec<Entity>,
    assets: Option<Arc<[IndexedAsset]>>,
    asset_query: Option<AssetQuery>,
    entity_query: SearchQuery,
    project_keys: Arc<Vec<String>>,
}

/// One search in progress: [`Self::step`] examines candidates under a budget.
#[derive(Debug, Clone)]
pub struct SearchRun {
    /// The query text.
    pub query: String,
    /// The provider toggles it ran with.
    pub enabled: [bool; 5],
    /// The split query.
    pub parsed: ParsedQuery,
    /// One group per provider the query reaches, in [`Provider::ALL`] order.
    pub groups: Vec<ProviderGroup>,
    /// Candidates examined so far (each is one step).
    pub steps: u64,
    cursors: Vec<usize>,
    sources: Sources,
}

impl SearchRun {
    /// Starts a search of `query` over the providers `enabled` allows (indexed like
    /// [`Provider::ALL`]). Copies the entity ids and takes the asset index's list (no component
    /// reads, no file reads); `project_keys` are the `Regia.toml` keys.
    #[must_use]
    pub fn start(world: &World, query: &str, enabled: [bool; 5], project_keys: Arc<Vec<String>>) -> Self {
        let parsed = ParsedQuery::parse(query);
        let reaches = |p: Provider| -> bool {
            if !enabled[p.index()] || (!parsed.only.is_empty() && !parsed.only.contains(&p)) {
                return false;
            }
            if parsed.is_blank() {
                return !parsed.only.is_empty();
            }
            match p {
                Provider::Assets => !parsed.entity_filters,
                Provider::Entities => !parsed.asset_filters,
                _ => !parsed.has_filters(),
            }
        };
        let mut sources = Sources {
            project_keys,
            ..Default::default()
        };
        let mut groups = Vec::new();
        for provider in Provider::ALL.into_iter().filter(|p| reaches(*p)) {
            let mut group = ProviderGroup {
                provider,
                hits: Vec::new(),
                total: 0,
                done: false,
                note: None,
            };
            match provider {
                Provider::Assets => match AssetQuery::parse(&parsed.asset_text) {
                    Ok(q) => {
                        sources.asset_query = Some(q);
                        sources.assets = world.get_resource::<ContentIndex>().map(ContentIndex::assets);
                        if sources.assets.is_none() {
                            group.note = Some("no asset index (open a project)".to_owned());
                        }
                    }
                    Err(err) => {
                        // `type:<Component>` is an entity filter: no note for it.
                        if !(parsed.type_filter && err.code == "search_bad_type") {
                            group.note = Some(err.message);
                        }
                    }
                },
                Provider::Entities => {
                    sources.entity_query = crate::kind_icons::parse_search(&parsed.entity_text);
                    sources.entities = world
                        .iter_entities()
                        .filter(|e| !e.contains::<regia_ecs::ecs::resource::IsResource>())
                        .map(|e| e.id())
                        .collect();
                }
                _ => {}
            }
            groups.push(group);
        }
        let cursors = vec![0; groups.len()];
        Self {
            query: query.to_owned(),
            enabled,
            parsed,
            groups,
            steps: 0,
            cursors,
            sources,
        }
    }

    /// Whether every provider is done.
    #[must_use]
    pub fn done(&self) -> bool {
        self.groups.iter().all(|g| g.done)
    }

    /// Examines up to `budget` candidates (providers in order); returns how many it examined.
    pub fn step(&mut self, world: &World, budget: usize) -> usize {
        let mut used = 0;
        let words = self.parsed.words.join(" ");
        for g in 0..self.groups.len() {
            while used < budget && !self.groups[g].done {
                let cursor = self.cursors[g];
                let examined = match self.groups[g].provider {
                    Provider::Assets => self.asset_step(g, cursor),
                    Provider::Entities => self.entity_step(world, g, cursor),
                    Provider::Commands => self.command_step(world, g, cursor, &words),
                    Provider::Settings => self.setting_step(g, cursor, &words),
                    Provider::Methods => self.method_step(g, cursor, &words),
                };
                if !examined {
                    let group = &mut self.groups[g];
                    group.done = true;
                    trim(&mut group.hits);
                    continue;
                }
                self.cursors[g] += 1;
                used += 1;
                if self.groups[g].hits.len() >= GROUP_KEEP * 4 {
                    trim(&mut self.groups[g].hits);
                }
            }
        }
        self.steps += used as u64;
        used
    }

    /// Runs to the end with `budget` per call; returns the number of calls (editor frames).
    pub fn finish(&mut self, world: &World, budget: usize) -> u64 {
        let mut frames = 0;
        while !self.done() {
            self.step(world, budget.max(1));
            frames += 1;
        }
        frames
    }

    fn push(&mut self, g: usize, hit: Hit) {
        let group = &mut self.groups[g];
        group.total += 1;
        group.hits.push(hit);
    }

    fn asset_step(&mut self, g: usize, cursor: usize) -> bool {
        let (Some(assets), Some(query)) = (&self.sources.assets, &self.sources.asset_query) else {
            return false;
        };
        let Some((path, _)) = assets.get(cursor) else {
            return false;
        };
        let (lower, name_at) = search_key(path);
        if let Some(rank) = query.rank(&lower, name_at) {
            let label = path.rsplit('/').next().unwrap_or(path).to_owned();
            let hit = Hit {
                label,
                detail: path.clone(),
                item: SearchItem::Asset(path.clone()),
                score: 100 - i32::from(rank) * 25,
            };
            self.push(g, hit);
        }
        true
    }

    fn entity_step(&mut self, world: &World, g: usize, cursor: usize) -> bool {
        let Some(&entity) = self.sources.entities.get(cursor) else {
            return false;
        };
        if world.get_entity(entity).is_err() || crate::visibility::is_hidden(world, entity) {
            return true;
        }
        let query = &self.sources.entity_query;
        let label = crate::outliner::display_name(world, entity);
        let lower = label.to_lowercase();
        if !lower.contains(&query.name) {
            return true;
        }
        if let Some(kind) = &query.kind
            && !kind.matches(&crate::kind_icons::row_icon(world, entity))
        {
            return true;
        }
        if !query.types.is_empty() || !query.tags.is_empty() {
            let (components, tags) = crate::kind_icons::row_terms(world, entity);
            if !query.types.iter().all(|t| components.contains(t))
                || !query.tags.iter().all(|t| tags.contains(t))
            {
                return true;
            }
        }
        let score = if query.name.is_empty() {
            0
        } else if lower.starts_with(&query.name) {
            100
        } else {
            50
        };
        self.push(
            g,
            Hit {
                label,
                detail: format!("entity {:x}", entity.to_bits()),
                item: SearchItem::Entity(entity),
                score,
            },
        );
        true
    }

    fn command_step(&mut self, world: &World, g: usize, cursor: usize, words: &str) -> bool {
        let Some(def) = crate::command_palette::entries().get(cursor) else {
            return false;
        };
        let text = format!("{}: {}", def.category, def.title);
        if let Some((score, _)) = fuzzy_match(words, &text) {
            let shortcut = world
                .get_resource::<crate::command_palette::CommandKeymap>()
                .map_or_else(|| def.shortcut.to_owned(), |k| k.shortcut_of(def));
            let mut detail = crate::command_palette::menu_path(def.id)
                .unwrap_or(def.id)
                .to_owned();
            if !shortcut.is_empty() {
                detail.push_str("    ");
                detail.push_str(&shortcut);
            }
            self.push(
                g,
                Hit {
                    label: text,
                    detail,
                    item: SearchItem::Command(def.id),
                    score,
                },
            );
        }
        true
    }

    fn setting_step(&mut self, g: usize, cursor: usize, words: &str) -> bool {
        let prefs = crate::editor_prefs::PREFS;
        if let Some(def) = prefs.get(cursor) {
            let text = format!("{}: {} ({})", def.category, def.label, def.key);
            if let Some((score, _)) = fuzzy_match(words, &text) {
                self.push(
                    g,
                    Hit {
                        label: format!("Preferences: {}", def.label),
                        detail: def.key.to_owned(),
                        item: SearchItem::Setting {
                            key: def.key.to_owned(),
                            project: false,
                        },
                        score,
                    },
                );
            }
            return true;
        }
        let Some(key) = self.sources.project_keys.get(cursor - prefs.len()).cloned() else {
            return false;
        };
        if let Some((score, _)) = fuzzy_match(words, &format!("Project settings: {key}")) {
            self.push(
                g,
                Hit {
                    label: format!("Project settings: {key}"),
                    detail: "Regia.toml".to_owned(),
                    item: SearchItem::Setting { key, project: true },
                    score,
                },
            );
        }
        true
    }

    fn method_step(&mut self, g: usize, cursor: usize, words: &str) -> bool {
        let Some(&(name, description)) = method_list().get(cursor) else {
            return false;
        };
        let score = fuzzy_match(words, name).map(|(s, _)| s).or_else(|| {
            let lower = description.to_lowercase();
            self.parsed
                .words
                .iter()
                .all(|w| lower.contains(w.as_str()))
                .then_some(0)
        });
        if let Some(score) = score {
            let short: String = description.chars().take(120).collect();
            self.push(
                g,
                Hit {
                    label: name.to_owned(),
                    detail: short,
                    item: SearchItem::Method(name),
                    score,
                },
            );
        }
        true
    }
}

/// Sorts best first and keeps [`GROUP_KEEP`]. Ties go by label, except between two commands:
/// they keep the palette's registry order (the hits arrive in `command_palette::entries` order and
/// the sort is stable), so Ctrl+T's commands rank one query the way the palette does.
fn trim(hits: &mut Vec<Hit>) {
    hits.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| match (&a.item, &b.item) {
            (SearchItem::Command(_), SearchItem::Command(_)) => std::cmp::Ordering::Equal,
            _ => a.label.cmp(&b.label),
        })
    });
    hits.truncate(GROUP_KEEP);
}

/// What the preview pane shows for a result.
#[must_use]
pub fn preview_text(world: &World, item: &SearchItem) -> String {
    match item {
        SearchItem::Asset(path) => {
            let size = crate::mesh_source::project_root(world)
                .and_then(|root| std::fs::metadata(root.join(path)).ok())
                .map_or_else(String::new, |m| format!(", {} bytes", m.len()));
            format!(
                "{path}\ntype {}{size}\nEnter: show it in the Content Browser and open it",
                crate::asset_search::asset_type(path)
            )
        }
        SearchItem::Entity(e) => {
            let (_, tags) = crate::kind_icons::row_terms(world, *e);
            let icon = crate::kind_icons::row_icon(world, *e);
            format!(
                "{}\nkind {}{}\nSelected and framed for the preview; Enter keeps it, Escape restores",
                crate::outliner::display_name(world, *e),
                icon.kind.as_deref().unwrap_or("none"),
                if tags.is_empty() {
                    String::new()
                } else {
                    format!(", tags {}", tags.join(", "))
                }
            )
        }
        SearchItem::Command(id) => {
            let def = crate::command_palette::find(id);
            format!(
                "{}\n{}\nEnter: run it",
                def.map_or("", |d| d.title),
                crate::command_palette::menu_path(id).unwrap_or(*id)
            )
        }
        SearchItem::Setting { key, project: false } => {
            let def = crate::editor_prefs::find_pref(key);
            let value = world
                .get_resource::<crate::editor_prefs::EditorPreferences>()
                .and_then(|p| p.get(key))
                .or_else(|| def.map(|d| d.default.value()))
                .map_or_else(String::new, |v| v.to_json().to_string());
            format!(
                "{key} = {value}\n{}\nEnter: open Preferences at this setting",
                def.map_or("", |d| d.description)
            )
        }
        SearchItem::Setting { key, project: true } => {
            format!("{key}\nA Regia.toml key\nEnter: open Project Settings")
        }
        SearchItem::Method(name) => {
            let description = method_list()
                .iter()
                .find(|(n, _)| n == name)
                .map_or("", |(_, d)| *d);
            format!("{name}\n{description}\nEnter: copy the method name")
        }
    }
}

/// The selection and camera from before an entity preview (Escape restores them).
#[derive(Resource, Debug, Clone)]
pub struct PreviewRestore {
    /// The selection before the first preview.
    pub selection: SelectionSnapshot,
    /// The editor camera before the first preview.
    pub camera: Option<EditorCamera>,
}

/// Previews `item`: an entity is selected and framed with no undo entry, and the selection
/// history is paused until [`commit`] or [`cancel_preview`]. Other items change nothing.
pub fn preview(world: &mut World, item: &SearchItem) {
    let SearchItem::Entity(entity) = item else {
        // Leaving an entity row for another kind of row puts the selection back.
        if let Some(restore) = world.get_resource::<PreviewRestore>().cloned() {
            world
                .get_resource_or_insert_with(Selection::default)
                .restore(&restore.selection);
            if let Some(camera) = restore.camera {
                world.insert_resource(camera);
            }
        }
        return;
    };
    if world.get_entity(*entity).is_err() {
        return;
    }
    if world.get_resource::<PreviewRestore>().is_none() {
        crate::selection::track(world);
        let selection = world
            .get_resource::<Selection>()
            .map(Selection::snapshot)
            .unwrap_or(SelectionSnapshot {
                entities: Vec::new(),
                primary: None,
            });
        let camera = world.get_resource::<EditorCamera>().cloned();
        world.insert_resource(PreviewRestore { selection, camera });
    }
    world
        .get_resource_or_insert_with(SelectionHistory::default)
        .set_paused(true);
    world
        .get_resource_or_insert_with(Selection::default)
        .select(*entity, crate::selection::SelectMode::Replace);
    crate::command_palette::frame_selection(world);
}

/// Escape: puts back the selection and camera from before the preview and resumes the history.
/// Whether there was a preview to undo.
pub fn cancel_preview(world: &mut World) -> bool {
    let Some(restore) = world.remove_resource::<PreviewRestore>() else {
        return false;
    };
    world
        .get_resource_or_insert_with(Selection::default)
        .restore(&restore.selection);
    if let Some(camera) = restore.camera {
        world.insert_resource(camera);
    }
    world
        .get_resource_or_insert_with(SelectionHistory::default)
        .set_paused(false);
    true
}

/// Enter: does what the result is for and returns a status line. An entity stays selected (and
/// is recorded in the selection history); an asset is shown selected in the Content Browser and
/// opened when the editor can open it from here (a level with no unsaved changes, a prefab, a
/// data asset); a command runs; a setting opens its window; a method name is reported (the window
/// copies it).
pub fn commit(world: &mut World, item: &SearchItem) -> String {
    // An entity preview is kept only when the committed item is that entity.
    let had_preview = if matches!(item, SearchItem::Entity(_)) {
        world.remove_resource::<PreviewRestore>().is_some()
    } else {
        cancel_preview(world)
    };
    world
        .get_resource_or_insert_with(SelectionHistory::default)
        .set_paused(false);
    use crate::editor_layout::{LayoutRequest, LayoutRequests};
    match item {
        SearchItem::Entity(entity) => {
            if world.get_entity(*entity).is_err() {
                return "That entity no longer exists".to_owned();
            }
            if !had_preview {
                world
                    .get_resource_or_insert_with(Selection::default)
                    .select(*entity, crate::selection::SelectMode::Replace);
                crate::command_palette::frame_selection(world);
            }
            crate::selection::track(world);
            format!("Selected {}", crate::outliner::display_name(world, *entity))
        }
        SearchItem::Asset(path) => {
            world
                .get_resource_or_insert_with(crate::asset_panels::AssetPanels::default)
                .select(path.clone());
            LayoutRequests::push(world, LayoutRequest::Open(crate::shell::Panel::Content));
            open_asset(world, path)
        }
        SearchItem::Command(id) => {
            crate::command_palette::run(world, id).unwrap_or_else(|| format!("{id}: unknown command"))
        }
        SearchItem::Setting { key, project } => {
            if *project {
                LayoutRequests::push(world, LayoutRequest::Open(crate::shell::Panel::ProjectSettings));
                format!("Project settings: {key}")
            } else {
                world
                    .get_resource_or_insert_with(crate::prefs_panel::PrefsPanelState::default)
                    .query
                    .clone_from(key);
                let mut panel =
                    world.get_resource_or_insert_with(crate::prefs_panel::PrefsPanelState::default);
                panel.keymap_tab = false;
                // Committed while Project Settings shows: the Preferences page comes up (rev-g1, 3205 with 3206).
                panel.project_tab = false;
                panel.theme_tab = false;
                LayoutRequests::push(world, LayoutRequest::Open(crate::shell::Panel::Preferences));
                format!("Preferences: {key}")
            }
        }
        SearchItem::Method(name) => format!("Agent method {name} (see book/agents)"),
    }
}

/// Opens an asset the editor can open without a panel's own state; else says where it is shown.
fn open_asset(world: &mut World, path: &str) -> String {
    let Some(root) = crate::mesh_source::project_root(world) else {
        return format!("{path}: no project open");
    };
    match crate::asset_search::asset_type(path) {
        "level" => {
            let unsaved = world
                .get_resource::<crate::command::UndoStack>()
                .is_some_and(crate::command::UndoStack::is_dirty);
            if unsaved {
                return format!(
                    "{path}: shown in the Content Browser (save the level first to open another)"
                );
            }
            match crate::level_io::open_level_file(world, &root.join(path)) {
                Ok(open) => {
                    world.insert_resource(open);
                    crate::level_io::reset_edit_state(world);
                    format!("Opened {path}")
                }
                Err(err) => format!("{path}: {err}"),
            }
        }
        "prefab" if path.ends_with(".prefab.ron") => {
            match crate::prefab_edit::enter_prefab_edit(world, &root.join(path)) {
                Ok(()) => format!("Editing prefab {path}"),
                Err(err) => format!("{path}: {err}"),
            }
        }
        _ if path.ends_with(&format!(".{}", crate::data_asset::DATA_ASSET_EXTENSION)) => {
            match crate::data_asset::open_data_asset(world, path) {
                Ok(_) => format!("Opened {path}"),
                Err(err) => format!("{path}: {}", err.message),
            }
        }
        _ => format!("{path}: shown in the Content Browser (Enter there opens it)"),
    }
}

// ---------------------------------------------------------------------------------------------
// The window.

/// The universal search window's state (a world resource).
#[derive(Resource, Debug, Clone)]
pub struct UniversalSearch {
    /// Whether the window shows.
    pub open: bool,
    /// The query text.
    pub query: String,
    /// Provider toggles, indexed like [`Provider::ALL`].
    pub enabled: [bool; 5],
    /// The highlighted row (flat index over the shown rows).
    pub selected: usize,
    /// The search for the current query.
    pub run: Option<SearchRun>,
    /// Editor frames in which the window did search work.
    pub frames_worked: u64,
    /// Candidates examined over all runs.
    pub steps_total: u64,
    /// The name typed for Save.
    pub save_name: String,
    /// Whether Save shares the search with the project.
    pub save_project: bool,
    /// The last status line in the window.
    pub status: String,
    /// The preview of the highlighted row.
    pub preview: String,
    previewed: Option<SearchItem>,
    focus_query: bool,
    last_frame: Option<u64>,
    project_keys: Option<(Option<PathBuf>, Arc<Vec<String>>)>,
    vocab: Option<crate::asset_search::CompletionVocab>,
}

impl Default for UniversalSearch {
    fn default() -> Self {
        Self {
            open: false,
            query: String::new(),
            enabled: [true; 5],
            selected: 0,
            run: None,
            frames_worked: 0,
            steps_total: 0,
            save_name: String::new(),
            save_project: false,
            status: String::new(),
            preview: String::new(),
            previewed: None,
            focus_query: false,
            last_frame: None,
            project_keys: None,
            vocab: None,
        }
    }
}

impl UniversalSearch {
    /// Opens the window with an empty query (the toggles are kept).
    pub fn open_fresh(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.run = None;
        self.previewed = None;
        self.preview.clear();
        self.focus_query = true;
        self.project_keys = None;
        self.vocab = None;
    }

    /// The rows the window shows: `(group, hit)` indices, [`WINDOW_GROUP_ROWS`] per group.
    #[must_use]
    pub fn rows(&self) -> Vec<(usize, usize)> {
        let Some(run) = &self.run else {
            return Vec::new();
        };
        run.groups
            .iter()
            .enumerate()
            .flat_map(|(g, group)| (0..group.hits.len().min(WINDOW_GROUP_ROWS)).map(move |h| (g, h)))
            .collect()
    }

    /// The highlighted result.
    #[must_use]
    pub fn selected_item(&self) -> Option<SearchItem> {
        let (g, h) = *self.rows().get(self.selected)?;
        Some(self.run.as_ref()?.groups.get(g)?.hits.get(h)?.item.clone())
    }

    /// Restarts the search when the query or the toggles changed, then examines up to `budget`
    /// candidates. Returns the candidates examined (0 once the search is done).
    pub fn work(&mut self, world: &World, budget: usize) -> usize {
        let root = crate::mesh_source::project_root(world);
        if self.project_keys.as_ref().is_none_or(|(r, _)| *r != root) {
            let keys = root.as_deref().map(project_setting_keys).unwrap_or_default();
            self.project_keys = Some((root, Arc::new(keys)));
        }
        let stale = self
            .run
            .as_ref()
            .is_none_or(|r| r.query != self.query || r.enabled != self.enabled);
        if stale {
            let keys = self
                .project_keys
                .as_ref()
                .map(|(_, k)| Arc::clone(k))
                .unwrap_or_default();
            self.run = Some(SearchRun::start(world, &self.query, self.enabled, keys));
            self.selected = 0;
        }
        let Some(run) = self.run.as_mut() else {
            return 0;
        };
        if run.done() {
            return 0;
        }
        let used = run.step(world, budget);
        if used > 0 {
            self.frames_worked += 1;
            self.steps_total += used as u64;
        }
        used
    }
}

/// The side of an asset result's thumbnail in the preview pane, in points.
pub const PREVIEW_THUMB: f32 = 96.0;

/// The Content Browser thumbnail of the project-relative asset `rel` for the preview pane: asked
/// of the shared thumbnail cache this frame; `None` while it is on its way, or for a kind with no
/// thumbnail.
pub fn asset_preview_texture(world: &mut World, ctx: &egui::Context, rel: &str) -> Option<egui::TextureId> {
    let mut thumbs = world
        .remove_resource::<crate::thumb_cache::ThumbCache>()
        .unwrap_or_default();
    let root = crate::mesh_source::project_root(world);
    thumbs.set_root(root.as_deref());
    let state = thumbs.request(rel);
    thumbs.pump();
    let texture = if state == crate::thumb_cache::ThumbState::Ready {
        thumbs.texture(ctx, rel)
    } else {
        None
    };
    world.insert_resource(thumbs);
    texture
}

/// Draws the window when open (Ctrl+T or `search.everything`) and returns a status line when an
/// item was committed. Not shown during play.
#[tracing::instrument(target = "regia_editor", skip_all, name = "universal_search")]
pub fn show(ctx: &egui::Context, world: &mut World, playing: bool) -> Option<String> {
    // Closed: nothing to do (no resource moves, no allocation).
    if !world.get_resource::<UniversalSearch>().is_some_and(|s| s.open) {
        return None;
    }
    let mut search = world.remove_resource::<UniversalSearch>()?;
    if playing {
        search.open = false;
        world.insert_resource(search);
        cancel_preview(world);
        return None;
    }
    crate::saved_searches::ensure_loaded(world);
    // egui may run a frame's UI twice: input, search work and previews happen once per frame.
    let frame = ctx.cumulative_frame_nr();
    let first_pass = search.last_frame != Some(frame);
    search.last_frame = Some(frame);
    let query_id = egui::Id::new("regia-universal-search-query");
    let (escape, down, up, enter) = if first_pass {
        ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        })
    } else {
        (false, false, false, false)
    };
    // Tab completes the last token while the query field has the focus and the token can be
    // completed; otherwise Tab moves the focus as usual (the field's focus lock below follows it).
    // The vocabulary is read once per opening.
    let vocab = search
        .vocab
        .take()
        .unwrap_or_else(|| crate::asset_search::CompletionVocab::of_world(world));
    if first_pass
        && ctx.memory(|m| m.has_focus(query_id))
        && let Some(done) = crate::asset_search::complete_token(&search.query, &vocab)
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
    {
        search.query = done;
        if let Some(mut state) = egui::TextEdit::load_state(ctx, query_id) {
            let end = egui::text::CCursor::new(search.query.chars().count());
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(end)));
            state.store(ctx, query_id);
        }
    }
    if first_pass {
        search.work(world, PROVIDER_BUDGET);
    }
    if search.run.as_ref().is_some_and(|r| !r.done()) {
        ctx.request_repaint();
    }
    let rows = search.rows();
    let before = search.selected;
    if !rows.is_empty() {
        if down {
            search.selected = (search.selected + 1) % rows.len();
        }
        if up {
            search.selected = (search.selected + rows.len() - 1) % rows.len();
        }
    }
    search.selected = search.selected.min(rows.len().saturating_sub(1));
    let mut status = None;
    let mut commit_item = if enter { search.selected_item() } else { None };
    let mut rerun: Option<crate::saved_searches::SavedSearch> = None;
    let mut save_clicked = false;
    let mut delete: Option<(crate::saved_searches::Scope, String)> = None;
    let saved = world
        .get_resource::<crate::saved_searches::SavedSearches>()
        .cloned()
        .unwrap_or_default();
    let mut clicked_row = None;
    // An asset row previews its Content Browser thumbnail (review sprint-w12-rev-g1 #5).
    let preview_texture = match search.selected_item() {
        Some(SearchItem::Asset(path)) => asset_preview_texture(world, ctx, &path),
        _ => None,
    };
    egui::Window::new("Search everything")
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width(160.0);
                    ui.strong("Saved searches");
                    for (scope, list) in [
                        (crate::saved_searches::Scope::Personal, &saved.personal),
                        (crate::saved_searches::Scope::Project, &saved.project),
                    ] {
                        if list.is_empty() {
                            continue;
                        }
                        ui.weak(match scope {
                            crate::saved_searches::Scope::Personal => "Mine",
                            crate::saved_searches::Scope::Project => "Project",
                        });
                        for entry in list {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(search.query == entry.query, entry.name.as_str())
                                    .on_hover_text(entry.query.as_str())
                                    .clicked()
                                {
                                    rerun = Some(entry.clone());
                                }
                                if ui
                                    .small_button("Delete")
                                    .on_hover_text(format!("Delete the saved search {}", entry.name))
                                    .clicked()
                                {
                                    delete = Some((scope, entry.name.clone()));
                                }
                            });
                        }
                    }
                    if saved.personal.is_empty() && saved.project.is_empty() {
                        ui.weak("None yet");
                    }
                    for problem in &saved.problems {
                        ui.colored_label(crate::theme::ui_tokens(ui.ctx()).warn, problem);
                    }
                    ui.separator();
                    ui.add(
                        egui::TextEdit::singleline(&mut search.save_name)
                            .hint_text("Name")
                            .desired_width(150.0),
                    );
                    crate::a11y_audit::bare_checkbox(ui, &mut search.save_project, "Share with the project");
                    if ui.button("Save search").clicked() {
                        save_clicked = true;
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut search.query)
                            .id(query_id)
                            .hint_text(
                                "Search assets, entities, commands, settings, methods (a: e: c: s: m:)",
                            )
                            .desired_width(440.0),
                    );
                    if search.focus_query {
                        edit.request_focus();
                        search.focus_query = false;
                    }
                    ui.horizontal_wrapped(|ui| {
                        for provider in Provider::ALL {
                            let mut on = search.enabled[provider.index()];
                            if ui
                                .checkbox(&mut on, provider.title())
                                .on_hover_text(format!("Prefix {} searches only these", provider.prefix()))
                                .changed()
                            {
                                search.enabled[provider.index()] = on;
                            }
                        }
                    });
                    ui.separator();
                    let Some(run) = &search.run else {
                        return;
                    };
                    if run.groups.is_empty() {
                        ui.weak("Type to search, or start with a: e: c: s: m: to list one provider");
                    }
                    let mut flat = 0;
                    for group in &run.groups {
                        let more = if group.done { "" } else { "+" };
                        ui.strong(format!("{} ({}{more})", group.provider.title(), group.total));
                        if let Some(note) = &group.note {
                            ui.weak(note);
                        }
                        for hit in group.hits.iter().take(WINDOW_GROUP_ROWS) {
                            let row = ui
                                .selectable_label(
                                    flat == search.selected,
                                    format!("{}    {}", hit.label, hit.detail),
                                )
                                .on_hover_text(hit.detail.as_str());
                            if row.clicked() {
                                clicked_row = Some(flat);
                            }
                            flat += 1;
                        }
                    }
                    ui.separator();
                    if let Some(texture) = preview_texture {
                        ui.image((texture, egui::vec2(PREVIEW_THUMB, PREVIEW_THUMB)));
                    }
                    if !search.preview.is_empty() {
                        ui.label(search.preview.as_str());
                    }
                    ui.weak("Up/Down: preview   Enter: open   Esc: close and restore   Tab: complete");
                    if !search.status.is_empty() {
                        ui.weak(search.status.as_str());
                    }
                });
            });
        });
    // The query field keeps Tab only while its last token can be completed.
    if ctx.memory(|m| m.has_focus(query_id)) {
        let completes = crate::asset_search::complete_token(&search.query, &vocab).is_some();
        ctx.memory_mut(|m| {
            m.set_focus_lock_filter(
                query_id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    tab: completes,
                    ..Default::default()
                },
            );
        });
    }
    search.vocab = Some(vocab);
    if let Some(row) = clicked_row {
        search.selected = row;
        commit_item = search.selected_item();
    }
    if let Some(entry) = rerun {
        search.query = entry.query.clone();
        if !entry.providers.is_empty() {
            for provider in Provider::ALL {
                search.enabled[provider.index()] = entry.providers.iter().any(|p| p == provider.id());
            }
        }
        search.focus_query = true;
    }
    if save_clicked {
        let scope = if search.save_project {
            crate::saved_searches::Scope::Project
        } else {
            crate::saved_searches::Scope::Personal
        };
        let providers: Vec<String> = if search.enabled.iter().all(|on| *on) {
            Vec::new()
        } else {
            Provider::ALL
                .into_iter()
                .filter(|p| search.enabled[p.index()])
                .map(|p| p.id().to_owned())
                .collect()
        };
        search.status =
            match crate::saved_searches::save(world, scope, &search.save_name, &search.query, &providers) {
                Ok(saved) => format!("Saved {} ({})", saved.name, scope.id()),
                Err(err) => err.to_string(),
            };
    }
    if let Some((scope, name)) = delete {
        search.status = match crate::saved_searches::delete(world, scope, &name) {
            Ok(()) => format!("Deleted {name}"),
            Err(err) => err.to_string(),
        };
    }
    // Moving the highlight previews the row once per change.
    let current = search.selected_item();
    if first_pass && (search.selected != before || current != search.previewed) && commit_item.is_none() {
        if let Some(item) = &current {
            preview(world, item);
            search.preview = preview_text(world, item);
        } else {
            search.preview.clear();
        }
        search.previewed = current;
    }
    if let Some(item) = commit_item {
        if let SearchItem::Method(name) = &item {
            ctx.copy_text((*name).to_owned());
        }
        let line = commit(world, &item);
        search.open = false;
        search.previewed = None;
        status = Some(line);
    } else if escape {
        cancel_preview(world);
        search.open = false;
        search.previewed = None;
    }
    world.insert_resource(search);
    status
}

// ---------------------------------------------------------------------------------------------
// The agent method.

/// Parses a `providers` parameter: an array of provider ids.
///
/// # Errors
///
/// `bad_providers` when it is not an array of known ids.
pub fn providers_param(value: &Value) -> Result<Vec<Provider>, RpcError> {
    let bad = || {
        RpcError::new(
            INVALID_PARAMS,
            "bad_providers",
            "`providers` must be an array of assets, entities, commands, settings, methods",
            "omit it to search every provider",
        )
    };
    value
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|v| v.as_str().and_then(Provider::from_id).ok_or_else(bad))
        .collect()
}

fn hit_param(params: &Value, name: &str) -> Result<Option<(Provider, usize)>, RpcError> {
    let Some(v) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = || {
        RpcError::new(
            INVALID_PARAMS,
            "bad_hit",
            format!("`{name}` must be {{ provider, index }} naming a returned hit"),
            "index counts from 0 within the provider's hits",
        )
    };
    let provider = v
        .get("provider")
        .and_then(Value::as_str)
        .and_then(Provider::from_id)
        .ok_or_else(bad)?;
    let index = v.get("index").and_then(Value::as_u64).ok_or_else(bad)? as usize;
    Ok(Some((provider, index)))
}

/// `editor.search { query?, providers?, limit?, saved?, scope?, preview?, commit?, cancel? }`.
fn editor_search(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |code: &str, message: &str, hint: &str| RpcError::new(INVALID_PARAMS, code, message, hint);
    let mut query = match params.get("query") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(bad(
                "bad_query",
                "`query` must be a string",
                "\"e: lamp\", \"a: type:mesh rock\"",
            ));
        }
    };
    let mut enabled = [true; 5];
    if let Some(v) = params.get("providers").filter(|v| !v.is_null()) {
        let chosen = providers_param(v)?;
        enabled = Provider::ALL.map(|p| chosen.contains(&p));
    }
    // `saved` runs a saved search by name.
    if let Some(name) = params.get("saved").filter(|v| !v.is_null()) {
        let name = name.as_str().ok_or_else(|| {
            bad(
                "bad_saved",
                "`saved` must be a saved search's name",
                "editor.saved_search_list",
            )
        })?;
        let scope = match params.get("scope").and_then(Value::as_str) {
            None => crate::saved_searches::Scope::Personal,
            Some(s) => crate::saved_searches::Scope::parse(s).ok_or_else(|| {
                bad(
                    "bad_scope",
                    "`scope` must be \"personal\" or \"project\"",
                    "personal is the default",
                )
            })?,
        };
        crate::saved_searches::ensure_loaded(world);
        let entry = world
            .get_resource::<crate::saved_searches::SavedSearches>()
            .and_then(|s| s.find(scope, name).cloned())
            .ok_or_else(|| {
                RpcError::new(
                    NOT_FOUND,
                    "saved_search_not_found",
                    "no saved search has that name in that scope",
                    "editor.saved_search_list",
                )
            })?;
        query = entry.query;
        if !entry.providers.is_empty() {
            enabled = Provider::ALL.map(|p| entry.providers.iter().any(|id| id == p.id()));
        }
    }
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 20,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=GROUP_KEEP as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                bad(
                    "bad_limit",
                    "`limit` must be a whole number from 1 to 50",
                    "hits per provider; 20 is the default",
                )
            })?,
    };
    let preview_at = hit_param(params, "preview")?;
    let commit_at = hit_param(params, "commit")?;
    if params.get("cancel").and_then(Value::as_bool) == Some(true) {
        let restored = cancel_preview(world);
        return Ok(json!({ "cancelled": restored }));
    }
    // The content index the asset provider reads (built from the project the first time).
    if world.get_resource::<ContentIndex>().is_none()
        && let Some(root) = crate::mesh_source::project_root(world)
    {
        let mut index = ContentIndex::default();
        index.ensure(Some(root.as_path()));
        world.insert_resource(index);
    }
    let keys = crate::mesh_source::project_root(world)
        .map(|root| project_setting_keys(&root))
        .unwrap_or_default();
    let mut run = SearchRun::start(world, &query, enabled, Arc::new(keys));
    let frames = run.finish(world, PROVIDER_BUDGET);
    let pick = |at: Option<(Provider, usize)>| -> Result<Option<SearchItem>, RpcError> {
        let Some((provider, index)) = at else {
            return Ok(None);
        };
        run.groups
            .iter()
            .find(|g| g.provider == provider)
            .and_then(|g| g.hits.get(index))
            .map(|h| Some(h.item.clone()))
            .ok_or_else(|| {
                RpcError::new(
                    NOT_FOUND,
                    "no_such_hit",
                    "no hit at that provider and index for this query",
                    "run the query first and use a returned index",
                )
            })
    };
    let preview_item = pick(preview_at)?;
    let commit_item = pick(commit_at)?;
    let mut previewed = Value::Null;
    if let Some(item) = &preview_item {
        preview(world, item);
        previewed = json!({ "item": item.to_json(), "text": preview_text(world, item) });
    }
    let mut committed = Value::Null;
    if let Some(item) = &commit_item {
        let message = commit(world, item);
        committed = json!({ "item": item.to_json(), "message": message });
    }
    let groups: Vec<Value> = run
        .groups
        .iter()
        .map(|g| {
            json!({
                "provider": g.provider.id(),
                "title": g.provider.title(),
                "count": g.total,
                "note": g.note,
                "hits": g.hits.iter().take(limit).enumerate().map(|(i, h)| json!({
                    "index": i,
                    "label": h.label,
                    "detail": h.detail,
                    "score": h.score,
                    "item": h.item.to_json(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "query": query,
        "only": run.parsed.only.iter().map(|p| p.id()).collect::<Vec<_>>(),
        "providers": groups,
        "steps": run.steps,
        "frames": frames,
        "budget": PROVIDER_BUDGET,
        "previewed": previewed,
        "committed": committed,
    }))
}

/// The universal search's agent method, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.search",
            tool: "search",
            description: "Universal search (the Ctrl+T window): one query over assets, level entities, palette commands \
                (menu items included), settings (preferences and Regia.toml keys) and agent methods, results grouped \
                by provider with counts. Prefix tokens a: e: c: s: m: limit the providers; type:/t:, ext: and -word \
                filter assets, kind:, tag: and type:<Component> filter entities. preview selects and frames an entity \
                without an undo entry (cancel restores); commit does what Enter does in the window.",
            params: &[
                ParamSchema {
                    name: "query",
                    kind: "string",
                    required: false,
                    description: "The query text (\"e: lamp\", \"a: type:mesh rock\", \"grid\").",
                },
                ParamSchema {
                    name: "providers",
                    kind: "array",
                    required: false,
                    description: "Provider ids to search (assets, entities, commands, settings, methods); default all.",
                },
                ParamSchema {
                    name: "limit",
                    kind: "integer",
                    required: false,
                    description: "Hits returned per provider, 1 to 50 (default 20); count covers every match.",
                },
                ParamSchema {
                    name: "saved",
                    kind: "string",
                    required: false,
                    description: "Run this saved search instead of query (editor.saved_search_list).",
                },
                ParamSchema {
                    name: "scope",
                    kind: "string",
                    required: false,
                    description: "The saved search's scope: personal (default) or project.",
                },
                ParamSchema {
                    name: "preview",
                    kind: "object",
                    required: false,
                    description: "{ provider, index }: preview that hit (an entity is selected and framed, no undo entry).",
                },
                ParamSchema {
                    name: "commit",
                    kind: "object",
                    required: false,
                    description: "{ provider, index }: commit that hit as Enter does (open, select, run, open the setting).",
                },
                ParamSchema {
                    name: "cancel",
                    kind: "boolean",
                    required: false,
                    description: "true: restore the selection and camera from before the previews (Escape).",
                },
            ],
            result: "{ query, only, providers: [{ provider, title, count, note, hits: [{ index, label, detail, score, item }] }], steps, frames, budget, previewed, committed } | { cancelled }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_search as Handler,
    )]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// rev-g1 (3205 with 3206): a setting committed while Project Settings shows brings the
    /// Preferences page up, not the project tab.
    #[test]
    fn a_committed_preference_leaves_the_project_settings_tab() {
        let mut world = World::new();
        world.insert_resource(crate::prefs_panel::PrefsPanelState {
            keymap_tab: true,
            project_tab: true,
            ..Default::default()
        });
        let item = SearchItem::Setting {
            key: "appearance.ui_scale".into(),
            project: false,
        };
        commit(&mut world, &item);
        let panel = world.resource::<crate::prefs_panel::PrefsPanelState>();
        assert!(!panel.keymap_tab && !panel.project_tab);
        assert_eq!(panel.query, "appearance.ui_scale");
    }

    /// Review sprint-w12-rev-g1 #5 (spec addendum § 2.1): an asset result previews the Content
    /// Browser thumbnail of a known image; a kind with no thumbnail gives none.
    #[test]
    fn an_image_asset_result_previews_its_thumbnail() {
        use image::ImageEncoder as _;
        let root = regia_test_utils::unique_temp_dir("universal-search-thumb");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let pixels = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 40, 40, 255]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(pixels.as_raw(), 8, 8, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(root.join("assets/red.png"), png).unwrap();
        std::fs::write(root.join("assets/notes.txt"), "x").unwrap();
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let ctx = egui::Context::default();
        assert_eq!(
            asset_preview_texture(&mut world, &ctx, "assets/red.png"),
            None,
            "queued first"
        );
        let mut thumbs = world.remove_resource::<crate::thumb_cache::ThumbCache>().unwrap();
        thumbs.finish();
        world.insert_resource(thumbs);
        assert!(
            asset_preview_texture(&mut world, &ctx, "assets/red.png").is_some(),
            "the picture is ready on a later frame"
        );
        assert_eq!(asset_preview_texture(&mut world, &ctx, "assets/notes.txt"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        editor_search(world, &params)
    }

    fn named(world: &mut World, name: &str) -> Entity {
        world.spawn(regia_ecs::ecs::name::Name::new(name.to_owned())).id()
    }

    fn group(out: &Value, provider: &str) -> Value {
        out["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["provider"] == provider)
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn prefix_tokens_limit_providers_and_filters_route() {
        let p = ParsedQuery::parse("e: Lamp kind:light");
        assert_eq!(p.only, [Provider::Entities]);
        assert_eq!(p.words, ["lamp"]);
        assert_eq!(p.entity_text, "lamp kind:light");
        assert!(p.entity_filters && !p.asset_filters);
        let p = ParsedQuery::parse("a:rock ext:glb -moss");
        assert_eq!(p.only, [Provider::Assets]);
        assert_eq!(p.asset_text, "rock ext:glb -moss");
        assert!(p.asset_filters);
        assert!(ParsedQuery::parse("c:").is_blank());
        assert!(
            ParsedQuery::parse("ext:png").only.is_empty(),
            "ext: is not the e: prefix"
        );
    }

    #[test]
    fn assets_provider_reads_the_content_index() {
        let mut world = World::new();
        world.insert_resource(ContentIndex::from_listing(
            None,
            [
                "assets/props/rock.glb",
                "assets/props/crate.prefab.ron",
                "assets/tex/rock.png",
            ]
            .map(String::from),
        ));
        let out = call(&mut world, json!({ "query": "rock" })).unwrap();
        let assets = group(&out, "assets");
        assert_eq!(assets["count"], 2);
        assert_eq!(assets["hits"][0]["item"]["type"], "asset");
        let out = call(&mut world, json!({ "query": "a: type:mesh" })).unwrap();
        assert_eq!(out["only"], json!(["assets"]));
        assert_eq!(
            group(&out, "assets")["hits"][0]["item"]["path"],
            "assets/props/rock.glb"
        );
        assert_eq!(out["providers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn entities_provider_matches_name_kind_and_tag() {
        let mut world = World::new();
        let lamp = named(&mut world, "Desk Lamp");
        let tagged = world
            .spawn((
                regia_ecs::ecs::name::Name::new("Crate".to_owned()),
                regia_scene::components::Tags::from_iter_str(["Loot"]),
            ))
            .id();
        named(&mut world, "Crate (plain)");
        let out = call(&mut world, json!({ "query": "e: lamp" })).unwrap();
        assert_eq!(
            group(&out, "entities")["hits"][0]["item"]["entity"],
            lamp.to_bits()
        );
        let out = call(&mut world, json!({ "query": "crate tag:loot" })).unwrap();
        let entities = group(&out, "entities");
        assert_eq!(entities["count"], 1);
        assert_eq!(entities["hits"][0]["item"]["entity"], tagged.to_bits());
        assert!(
            group(&out, "commands").is_null(),
            "a filter token leaves commands out"
        );
        let out = call(&mut world, json!({ "query": "e: kind:none lamp" })).unwrap();
        assert_eq!(group(&out, "entities")["count"], 1);
    }

    #[test]
    fn commands_provider_lists_palette_and_menu_entries() {
        let mut world = World::new();
        let out = call(&mut world, json!({ "query": "c: duplicate" })).unwrap();
        let hits = group(&out, "commands")["hits"].as_array().unwrap().clone();
        assert!(hits.iter().any(|h| h["item"]["id"] == "outliner.duplicate"));
        assert!(
            hits.iter()
                .any(|h| h["item"]["id"] == "menu:Edit/Duplicate (Ctrl+D)"),
            "a menu item is a palette entry: {hits:?}"
        );
        // Equal scores keep the palette's order: Ctrl+T and the palette rank a query alike (a
        // later command whose title also holds the word, gizmo.toggle_snap, ties on score).
        let palette: Vec<&str> = crate::command_palette::CommandPalette::default()
            .rank("grid")
            .iter()
            .map(|r| r.def.id)
            .take(GROUP_KEEP)
            .collect();
        let out = call(
            &mut world,
            json!({ "query": "grid", "providers": ["commands"], "limit": GROUP_KEEP }),
        )
        .unwrap();
        let ranked: Vec<String> = group(&out, "commands")["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["item"]["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ranked, palette, "the commands group ranks like the palette");
        let out = call(&mut world, json!({ "query": "grid", "providers": ["commands"], "commit": { "provider": "commands", "index": 0 } })).unwrap();
        assert_eq!(out["committed"]["item"]["id"], "view.toggle_grid");
        assert!(
            world
                .resource::<crate::command_palette::CommandPalette>()
                .recent
                .contains(&"view.toggle_grid")
        );
    }

    #[test]
    fn settings_provider_lists_preferences_and_opens_them() {
        let mut world = World::new();
        let out = call(&mut world, json!({ "query": "s: ui scale" })).unwrap();
        let settings = group(&out, "settings");
        assert_eq!(
            settings["hits"][0]["item"]["key"], "appearance.ui_scale",
            "{settings}"
        );
        let out = call(
            &mut world,
            json!({ "query": "s: ui scale", "commit": { "provider": "settings", "index": 0 } }),
        )
        .unwrap();
        assert_eq!(out["committed"]["message"], "Preferences: appearance.ui_scale");
        assert_eq!(
            world.resource::<crate::prefs_panel::PrefsPanelState>().query,
            "appearance.ui_scale"
        );
        let queued = &world.resource::<crate::editor_layout::LayoutRequests>().pending;
        assert_eq!(
            queued.last(),
            Some(&crate::editor_layout::LayoutRequest::Open(
                crate::shell::Panel::Preferences
            ))
        );
    }

    #[test]
    fn project_settings_keys_come_from_the_manifest() {
        let dir = regia_test_utils::unique_temp_dir("universal-search-project-keys");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Demo\"\n[content]\nbase_url = \"x\"\n",
        )
        .unwrap();
        assert_eq!(project_setting_keys(&dir), ["content.base_url", "project.name"]);
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        let out = call(&mut world, json!({ "query": "s: base url" })).unwrap();
        let hits = group(&out, "settings")["hits"].as_array().unwrap().clone();
        assert!(
            hits.iter()
                .any(|h| h["item"]["key"] == "content.base_url" && h["item"]["scope"] == "project")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn methods_provider_lists_agent_methods() {
        let mut world = World::new();
        let out = call(&mut world, json!({ "query": "m: editor.search" })).unwrap();
        let methods = group(&out, "methods");
        assert_eq!(methods["hits"][0]["item"]["name"], "editor.search");
        let out = call(
            &mut world,
            json!({ "query": "m: editor.search", "commit": { "provider": "methods", "index": 0 } }),
        )
        .unwrap();
        assert!(
            out["committed"]["message"]
                .as_str()
                .unwrap()
                .contains("editor.search")
        );
    }

    /// Acceptance: preview never creates an undo entry, and Escape (cancel) restores the
    /// selection and camera from before the preview.
    #[test]
    fn preview_has_no_undo_entry_and_cancel_restores() {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let before = named(&mut world, "Before");
        let target = named(&mut world, "Target Lamp");
        world
            .get_resource_or_insert_with(Selection::default)
            .select(before, crate::selection::SelectMode::Replace);
        let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
        let out = call(
            &mut world,
            json!({ "query": "e: target", "preview": { "provider": "entities", "index": 0 } }),
        )
        .unwrap();
        assert_eq!(out["previewed"]["item"]["entity"], target.to_bits());
        assert_eq!(world.resource::<Selection>().entities(), [target]);
        let undo = world.resource::<crate::command::UndoStack>();
        assert_eq!(undo.undo_depth(), 0, "a preview is not an edit");
        assert!(!undo.is_dirty());
        assert!(
            world.resource::<SelectionHistory>().paused(),
            "previews do not enter the history"
        );
        let out = call(&mut world, json!({ "cancel": true })).unwrap();
        assert_eq!(out["cancelled"], true);
        assert_eq!(world.resource::<Selection>().entities(), [before]);
        assert_eq!(*world.resource::<EditorCamera>(), camera, "the camera is back");
        assert!(!world.resource::<SelectionHistory>().paused());
        // Commit keeps the previewed entity and records it in the history.
        call(
            &mut world,
            json!({ "query": "e: target", "preview": { "provider": "entities", "index": 0 } }),
        )
        .unwrap();
        call(
            &mut world,
            json!({ "query": "e: target", "commit": { "provider": "entities", "index": 0 } }),
        )
        .unwrap();
        assert_eq!(world.resource::<Selection>().entities(), [target]);
        assert!(world.get_resource::<PreviewRestore>().is_none());
        let history = world.resource::<SelectionHistory>();
        assert_eq!(history.entries().last().unwrap().entities, [target]);
        assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 0);
    }

    /// Acceptance: a 20 000-entity level answers within a counted number of provider steps: each
    /// candidate is examined once, and at most `PROVIDER_BUDGET` per editor frame.
    #[test]
    fn a_large_level_answers_within_counted_steps() {
        let mut world = World::new();
        for i in 0..20_000 {
            named(&mut world, &format!("crate_{i}"));
        }
        let needle = named(&mut world, "needle_lamp");
        let candidates = world
            .iter_entities()
            .filter(|e| !e.contains::<regia_ecs::ecs::resource::IsResource>())
            .count();
        let mut run = SearchRun::start(&world, "e: needle", [true; 5], Arc::default());
        let frames = run.finish(&world, PROVIDER_BUDGET);
        assert_eq!(run.groups.len(), 1);
        assert_eq!(run.groups[0].hits[0].item, SearchItem::Entity(needle));
        assert_eq!(run.steps, candidates as u64, "each entity is examined once");
        // The frame that examines the last candidate also notices the end, unless the budget ran
        // out exactly there.
        assert_eq!(frames, candidates as u64 / PROVIDER_BUDGET as u64 + 1);
        // Per frame, the work is bounded by the budget; a finished run does nothing.
        let mut search = UniversalSearch {
            open: true,
            query: "e: crate_1".to_owned(),
            ..Default::default()
        };
        let mut steps_per_frame = Vec::new();
        loop {
            let used = search.work(&world, PROVIDER_BUDGET);
            if used == 0 {
                break;
            }
            steps_per_frame.push(used);
        }
        assert!(steps_per_frame.iter().all(|s| *s <= PROVIDER_BUDGET));
        assert_eq!(search.work(&world, PROVIDER_BUDGET), 0, "idle once done");
        let group = &search.run.as_ref().unwrap().groups[0];
        assert_eq!(group.hits.len(), GROUP_KEEP, "best hits kept");
        assert_eq!(
            group.total, 11_111,
            "crate_1, crate_10.., crate_100.., crate_1000..: every match counted"
        );
    }

    #[test]
    fn agent_errors_name_their_codes() {
        let mut world = World::new();
        let code = |e: RpcError| e.data.unwrap()["code"].as_str().unwrap().to_owned();
        assert_eq!(
            code(call(&mut world, json!({ "query": 3 })).unwrap_err()),
            "bad_query"
        );
        assert_eq!(
            code(call(&mut world, json!({ "providers": ["people"] })).unwrap_err()),
            "bad_providers"
        );
        assert_eq!(
            code(call(&mut world, json!({ "limit": 0 })).unwrap_err()),
            "bad_limit"
        );
        assert_eq!(
            code(
                call(
                    &mut world,
                    json!({ "query": "c: grid", "commit": { "provider": "assets", "index": 0 } })
                )
                .unwrap_err()
            ),
            "no_such_hit"
        );
        assert_eq!(
            code(call(&mut world, json!({ "commit": { "provider": "x" } })).unwrap_err()),
            "bad_hit"
        );
        assert_eq!(
            code(call(&mut world, json!({ "saved": "nope" })).unwrap_err()),
            "saved_search_not_found"
        );
        let (schema, _) = &methods()[0];
        assert_eq!(schema.name, "editor.search");
    }

    /// The keyboard path: Ctrl+T opens the window with the query focused; typing searches; Down
    /// previews; Escape restores and closes; Enter commits.
    #[test]
    fn keyboard_opens_previews_restores_and_commits() {
        use crate::canvas_keys::test_input::{key, raw, text};
        let ctx = egui::Context::default();
        let mut world = World::new();
        let first = named(&mut world, "Lamp A");
        let second = named(&mut world, "Lamp B");
        let other = named(&mut world, "Chair");
        world
            .get_resource_or_insert_with(Selection::default)
            .select(other, crate::selection::SelectMode::Replace);
        let frame = |world: &mut World, events: Vec<egui::Event>| {
            let mut status = None;
            let mut out = ctx.run_ui(raw([1200.0, 800.0], events), |ui| {
                status = crate::command_palette::show(ui.ctx(), world, false);
            });
            out.textures_delta.clear();
            status
        };
        frame(&mut world, vec![key(egui::Key::T, egui::Modifiers::COMMAND)]);
        assert!(
            world.resource::<UniversalSearch>().open,
            "Ctrl+T opens the window"
        );
        frame(&mut world, vec![]);
        frame(&mut world, vec![text("e: lamp")]);
        frame(&mut world, vec![]);
        assert_eq!(
            world.resource::<UniversalSearch>().query,
            "e: lamp",
            "the query field has the focus"
        );
        // The first row is previewed: selected and framed.
        let selected = world.resource::<Selection>().entities().to_vec();
        assert!(selected == [first] || selected == [second], "{selected:?}");
        frame(&mut world, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        frame(&mut world, vec![]);
        let now = world.resource::<Selection>().entities().to_vec();
        assert_ne!(now, selected, "Down previews the next row");
        frame(&mut world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert!(!world.resource::<UniversalSearch>().open);
        assert_eq!(
            world.resource::<Selection>().entities(),
            [other],
            "Escape restores"
        );
        // Again, and Enter commits.
        frame(&mut world, vec![key(egui::Key::T, egui::Modifiers::COMMAND)]);
        frame(&mut world, vec![]);
        frame(&mut world, vec![text("e: chair")]);
        frame(&mut world, vec![]);
        let status = frame(&mut world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert_eq!(status.as_deref(), Some("Selected Chair"));
        assert!(!world.resource::<UniversalSearch>().open);
        assert_eq!(world.resource::<Selection>().entities(), [other]);
    }

    #[test]
    fn tab_completes_in_the_query_field() {
        use crate::canvas_keys::test_input::{key, raw, text};
        let ctx = egui::Context::default();
        let mut world = World::new();
        let frame = |world: &mut World, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(raw([1200.0, 800.0], events), |ui| {
                let _ = crate::command_palette::show(ui.ctx(), world, false);
            });
            out.textures_delta.clear();
        };
        world
            .get_resource_or_insert_with(UniversalSearch::default)
            .open_fresh();
        frame(&mut world, vec![]);
        frame(&mut world, vec![text("a: type:te")]);
        frame(&mut world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
        frame(&mut world, vec![]);
        assert_eq!(world.resource::<UniversalSearch>().query, "a: type:texture");
    }
}
