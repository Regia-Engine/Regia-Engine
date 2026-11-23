// SPDX-License-Identifier: MPL-2.0
//! Saved searches (P-021, ADR-7766): a universal-search query kept under a name, either for the
//! user alone or shared with the project.
//!
//! - **Personal** searches live in the user config directory (`saved_searches.ron` next to
//!   `editor_prefs.ron`, `crate::editor_prefs::EditorConfigPaths`). The same file keeps the
//!   palette favorites (`crate::command_palette::PaletteFavorites`), which are personal too.
//! - **Project** searches live in `config/saved_searches.ron` under the project root, so a team
//!   shares them through revision control. The file holds no favorites.
//!
//! Both files are one new format, version 1 ([`SAVED_SEARCHES_FORMAT_VERSION`]):
//!
//! ```ron
//! (
//!     format_version: 1,
//!     searches: [
//!         (name: "Lights", query: "e: kind:light", providers: []),
//!     ],
//!     favorites: ["view.toggle_grid"],
//! )
//! ```
//!
//! A file from a newer editor is read but never written (the reason shows in the side bar); a
//! file that cannot be read or parsed is reported and left alone (saving into it answers
//! `saved_search_read_only`). Saving a name that exists replaces its query.
//! The universal search window lists both scopes in its side bar and re-runs one with a click;
//! the agent surface is `editor.saved_search_list`, `editor.saved_search_save` and
//! `editor.saved_search_delete` (`book/agents/universal-search.md`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The current version of `saved_searches.ron`.
pub const SAVED_SEARCHES_FORMAT_VERSION: u32 = 1;

/// The personal file's name, in the user config directory.
pub const PERSONAL_FILE: &str = "saved_searches.ron";

/// The project file, relative to the project root.
pub const PROJECT_FILE: &str = "config/saved_searches.ron";

/// The longest name a saved search may have, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// One saved search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSearch {
    /// The name shown in the side bar (unique within its scope).
    pub name: String,
    /// The query text, prefix tokens included (`e: kind:light`).
    pub query: String,
    /// The provider ids the search was limited to (`assets`, `entities`, ...); empty = the ones
    /// the query names, or all.
    #[serde(default)]
    pub providers: Vec<String>,
}

/// Version 1 of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedSearchesFileV1 {
    #[serde(default = "crate::editor_prefs::version_missing")]
    format_version: u32,
    #[serde(default)]
    searches: Vec<SavedSearch>,
    #[serde(default)]
    favorites: Vec<String>,
}

/// The serde shape of the file (field lists of every struct), for format checks.
#[must_use]
pub fn saved_searches_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SavedSearchesFileV1>(),
        regia_core::schema::serde_field_names::<SavedSearch>(),
    ])
}

/// The `from` versions of the registered migration steps (none: version 1 is the first).
#[must_use]
pub fn saved_searches_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Where a saved search lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The user config directory.
    Personal,
    /// `config/saved_searches.ron` of the project.
    Project,
}

impl Scope {
    /// Parses `personal` or `project`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "personal" => Some(Self::Personal),
            "project" => Some(Self::Project),
            _ => None,
        }
    }

    /// `personal` or `project`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
        }
    }
}

/// Why a saved-search action failed. [`Self::code`] is the protocol code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedSearchError {
    /// The name is blank or too long.
    BadName,
    /// No saved search has that name in that scope.
    NotFound(String),
    /// A project search needs an open project.
    NoProject,
    /// The file was written by a newer editor and is kept read-only.
    ReadOnly(String),
    /// The file does not parse.
    Parse(String),
    /// Reading or writing failed.
    Io(String),
}

impl SavedSearchError {
    /// The protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadName => "saved_search_bad_name",
            Self::NotFound(_) => "saved_search_not_found",
            Self::NoProject => "saved_search_no_project",
            Self::ReadOnly(_) => "saved_search_read_only",
            Self::Parse(_) => "saved_search_parse",
            Self::Io(_) => "saved_search_io",
        }
    }
}

impl std::fmt::Display for SavedSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName => write!(
                f,
                "a saved search needs a name of 1 to {MAX_NAME_CHARS} characters"
            ),
            Self::NotFound(name) => write!(f, "no saved search is called `{name}`"),
            Self::NoProject => f.write_str("project searches need an open project"),
            Self::ReadOnly(why) | Self::Parse(why) | Self::Io(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for SavedSearchError {}

/// What a saved-searches file holds: searches and (personal file only) favorites.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedSearchesRead {
    /// The searches, in file order (later duplicates of a name dropped).
    pub searches: Vec<SavedSearch>,
    /// Rows not shown (a name that is not valid, or a later duplicate of a name), kept to be
    /// written back unchanged so no save loses them (review sprint-w12-rev-g1 #7).
    pub kept: Vec<SavedSearch>,
    /// Pinned palette entry ids.
    pub favorites: Vec<String>,
}

/// Reads file text of any supported version.
///
/// # Errors
///
/// [`SavedSearchError::Parse`], or [`SavedSearchError::ReadOnly`] for a newer version.
pub fn parse(text: &str) -> Result<SavedSearchesRead, SavedSearchError> {
    let file = crate::editor_prefs::read_versioned(
        text,
        SAVED_SEARCHES_FORMAT_VERSION,
        |f: &SavedSearchesFileV1| f.format_version,
    )
    .map_err(|e| match e {
        crate::editor_prefs::VersionedError::Parse(m) => SavedSearchError::Parse(m),
        crate::editor_prefs::VersionedError::Newer(found) => SavedSearchError::ReadOnly(format!(
            "written by a newer editor (format_version {found}, this editor reads up to \
             {SAVED_SEARCHES_FORMAT_VERSION}); left untouched"
        )),
    })?;
    let mut searches: Vec<SavedSearch> = Vec::with_capacity(file.searches.len());
    let mut kept = Vec::new();
    for search in file.searches {
        if valid_name(&search.name) && !searches.iter().any(|s| s.name == search.name) {
            searches.push(search);
        } else {
            kept.push(search);
        }
    }
    Ok(SavedSearchesRead {
        searches,
        kept,
        favorites: file.favorites,
    })
}

/// The file text of `searches` and `favorites`, current version.
#[must_use]
pub fn to_text(searches: &[SavedSearch], favorites: &[String]) -> String {
    to_text_keeping(searches, &[], favorites)
}

/// The file text of `searches`, then the rows a read kept but did not show
/// ([`SavedSearchesRead::kept`]), and `favorites`, current version.
#[must_use]
pub fn to_text_keeping(searches: &[SavedSearch], kept: &[SavedSearch], favorites: &[String]) -> String {
    let file = SavedSearchesFileV1 {
        format_version: SAVED_SEARCHES_FORMAT_VERSION,
        searches: searches.iter().chain(kept).cloned().collect(),
        favorites: favorites.to_vec(),
    };
    ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
        .unwrap_or_else(|_| format!("(format_version: {SAVED_SEARCHES_FORMAT_VERSION}, searches: [])"))
}

fn valid_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_NAME_CHARS && trimmed == name
}

fn read_file(path: &Path) -> Result<Option<SavedSearchesRead>, SavedSearchError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SavedSearchError::Io(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// The saved searches of both scopes (a world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct SavedSearches {
    /// The user's searches.
    pub personal: Vec<SavedSearch>,
    /// The project's searches.
    pub project: Vec<SavedSearch>,
    /// What the last reads reported.
    pub problems: Vec<String>,
    personal_loaded: bool,
    project_loaded_for: Option<PathBuf>,
    personal_read_only: Option<String>,
    project_read_only: Option<String>,
    /// Favorites in the personal file that name no palette entry (kept and written back).
    unknown_favorites: Vec<String>,
    /// Rows of the personal and project files that are not shown (kept and written back).
    personal_kept: Vec<SavedSearch>,
    project_kept: Vec<SavedSearch>,
    /// Bumped by every personal change.
    personal_generation: u64,
    /// The personal generation and favorites generation last written (or read).
    personal_saved: (u64, u64),
}

impl SavedSearches {
    /// The searches of `scope`.
    #[must_use]
    pub fn of(&self, scope: Scope) -> &[SavedSearch] {
        match scope {
            Scope::Personal => &self.personal,
            Scope::Project => &self.project,
        }
    }

    /// The search called `name` in `scope`.
    #[must_use]
    pub fn find(&self, scope: Scope, name: &str) -> Option<&SavedSearch> {
        self.of(scope).iter().find(|s| s.name == name)
    }
}

/// The personal file, when the editor has a config directory.
fn personal_path(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|p| p.dir.as_ref().map(|d| d.join(PERSONAL_FILE)))
}

/// Reads the personal file once per session and the project file whenever the project changes.
/// Favorites in the personal file are added to [`crate::command_palette::PaletteFavorites`].
pub fn ensure_loaded(world: &mut World) {
    let mut saved = world.remove_resource::<SavedSearches>().unwrap_or_default();
    if !saved.personal_loaded {
        saved.personal_loaded = true;
        if let Some(path) = personal_path(world) {
            match read_file(&path) {
                Ok(Some(read)) => {
                    let mut favorites = world
                        .get_resource::<crate::command_palette::PaletteFavorites>()
                        .cloned()
                        .unwrap_or_default();
                    let session = favorites.ids.clone();
                    favorites.ids.clear();
                    for id in read.favorites {
                        match crate::command_palette::find(&id) {
                            Some(def) if !favorites.ids.contains(&def.id) => favorites.ids.push(def.id),
                            Some(_) => {}
                            None => saved.unknown_favorites.push(id),
                        }
                    }
                    for id in session {
                        if !favorites.ids.contains(&id) {
                            favorites.ids.push(id);
                        }
                    }
                    saved.personal_saved.1 = favorites.generation;
                    world.insert_resource(favorites);
                    // Session searches saved before the read win over the file's.
                    let session_searches = std::mem::take(&mut saved.personal);
                    saved.personal = read.searches;
                    saved.personal_kept = read.kept;
                    for search in session_searches {
                        upsert(&mut saved.personal, search);
                    }
                }
                Ok(None) => {}
                // A newer or unreadable file is reported and never overwritten.
                Err(err) => {
                    saved.problems.push(format!("{}: {err}", path.display()));
                    saved.personal_read_only = Some(format!("{}: {err}; left untouched", path.display()));
                }
            }
        }
        saved.personal_saved.0 = saved.personal_generation;
    }
    let root = crate::mesh_source::project_root(world);
    if saved.project_loaded_for != root {
        saved.project.clear();
        saved.project_kept.clear();
        saved.project_read_only = None;
        if let Some(root) = &root {
            let path = root.join(PROJECT_FILE);
            match read_file(&path) {
                Ok(Some(read)) => {
                    saved.project = read.searches;
                    saved.project_kept = read.kept;
                }
                Ok(None) => {}
                Err(err) => {
                    saved.problems.push(format!("{}: {err}", path.display()));
                    saved.project_read_only = Some(format!("{}: {err}; left untouched", path.display()));
                }
            }
        }
        saved.project_loaded_for = root;
    }
    world.insert_resource(saved);
}

fn upsert(list: &mut Vec<SavedSearch>, search: SavedSearch) {
    match list.iter_mut().find(|s| s.name == search.name) {
        Some(existing) => *existing = search,
        None => list.push(search),
    }
}

/// Saves `query` as `name` in `scope` (replacing a search of that name), and writes the file.
///
/// # Errors
///
/// A bad name, no project for [`Scope::Project`], a read-only file or a failed write.
pub fn save(
    world: &mut World,
    scope: Scope,
    name: &str,
    query: &str,
    providers: &[String],
) -> Result<SavedSearch, SavedSearchError> {
    if !valid_name(name) {
        return Err(SavedSearchError::BadName);
    }
    ensure_loaded(world);
    let search = SavedSearch {
        name: name.to_owned(),
        query: query.to_owned(),
        providers: providers.to_vec(),
    };
    edit(world, scope, |list| {
        upsert(list, search.clone());
        Ok(())
    })?;
    Ok(search)
}

/// Deletes the search `name` of `scope` and writes the file.
///
/// # Errors
///
/// [`SavedSearchError::NotFound`], no project, a read-only file or a failed write.
pub fn delete(world: &mut World, scope: Scope, name: &str) -> Result<(), SavedSearchError> {
    ensure_loaded(world);
    edit(world, scope, |list| {
        let before = list.len();
        list.retain(|s| s.name != name);
        if list.len() == before {
            Err(SavedSearchError::NotFound(name.to_owned()))
        } else {
            Ok(())
        }
    })
}

fn edit(
    world: &mut World,
    scope: Scope,
    change: impl FnOnce(&mut Vec<SavedSearch>) -> Result<(), SavedSearchError>,
) -> Result<(), SavedSearchError> {
    let mut saved = world.get_resource::<SavedSearches>().cloned().unwrap_or_default();
    match scope {
        Scope::Personal => {
            if let Some(why) = &saved.personal_read_only {
                return Err(SavedSearchError::ReadOnly(why.clone()));
            }
            change(&mut saved.personal)?;
            saved.personal_generation += 1;
            world.insert_resource(saved);
            persist_personal(world);
            Ok(())
        }
        Scope::Project => {
            let root = crate::mesh_source::project_root(world).ok_or(SavedSearchError::NoProject)?;
            if let Some(why) = &saved.project_read_only {
                return Err(SavedSearchError::ReadOnly(why.clone()));
            }
            let mut list = saved.project.clone();
            change(&mut list)?;
            let path = root.join(PROJECT_FILE);
            crate::level_io::write_atomic(&path, to_text_keeping(&list, &saved.project_kept, &[]).as_bytes())
                .map_err(SavedSearchError::Io)?;
            saved.project = list;
            world.insert_resource(saved);
            Ok(())
        }
    }
}

/// Writes the personal file when its searches or the palette favorites changed since the last
/// write (the palette calls it once per editor frame; one comparison when nothing changed).
/// Whether it wrote.
pub fn persist_personal(world: &mut World) -> bool {
    let favorites_generation = world
        .get_resource::<crate::command_palette::PaletteFavorites>()
        .map_or(0, |f| f.generation);
    let Some(saved) = world.get_resource::<SavedSearches>() else {
        // Favorites pinned before anything loaded: load (which keeps them), then write.
        if favorites_generation == 0 {
            return false;
        }
        ensure_loaded(world);
        return persist_personal(world);
    };
    if (saved.personal_generation, favorites_generation) == saved.personal_saved
        || saved.personal_read_only.is_some()
    {
        return false;
    }
    let Some(path) = personal_path(world) else {
        return false;
    };
    let mut favorites: Vec<String> = world
        .get_resource::<crate::command_palette::PaletteFavorites>()
        .map(|f| f.ids.iter().map(|id| (*id).to_owned()).collect())
        .unwrap_or_default();
    favorites.extend(saved.unknown_favorites.iter().cloned());
    let text = to_text_keeping(&saved.personal, &saved.personal_kept, &favorites);
    let generations = (saved.personal_generation, favorites_generation);
    let result = crate::level_io::write_atomic(&path, text.as_bytes());
    let mut saved = saved.clone();
    // A failed write is not retried every frame: the next change tries again.
    saved.personal_saved = generations;
    if let Err(err) = &result {
        tracing::warn!(target: "regia_editor", "saved searches not saved: {err}");
        saved.problems.push(format!("{}: {err}", path.display()));
    }
    world.insert_resource(saved);
    result.is_ok()
}

fn rpc(err: &SavedSearchError) -> RpcError {
    let code = if matches!(err, SavedSearchError::NotFound(_)) {
        NOT_FOUND
    } else {
        INVALID_PARAMS
    };
    RpcError::new(
        code,
        err.code(),
        err.to_string(),
        "list them with editor.saved_search_list",
    )
}

fn scope_param(params: &Value) -> Result<Scope, RpcError> {
    match params.get("scope") {
        None | Some(Value::Null) => Ok(Scope::Personal),
        Some(v) => v.as_str().and_then(Scope::parse).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "bad_scope",
                "`scope` must be \"personal\" or \"project\"",
                "personal is the default",
            )
        }),
    }
}

fn name_param(params: &Value) -> Result<String, RpcError> {
    params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| rpc(&SavedSearchError::BadName))
}

fn list_json(world: &World) -> Value {
    let saved = world.get_resource::<SavedSearches>().cloned().unwrap_or_default();
    json!({
        "personal": saved.personal,
        "project": saved.project,
        "personal_file": personal_path(world).map(|p| p.display().to_string()),
        "project_file": crate::mesh_source::project_root(world).map(|r| r.join(PROJECT_FILE).display().to_string()),
        "problems": saved.problems,
    })
}

/// `editor.saved_search_list`.
fn saved_search_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    ensure_loaded(world);
    Ok(list_json(world))
}

/// `editor.saved_search_save { name, query, scope?, providers? }`.
fn saved_search_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?;
    let query = params.get("query").and_then(Value::as_str).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "bad_query",
            "`query` must be a string",
            "the text typed into the search window",
        )
    })?;
    let scope = scope_param(params)?;
    let providers: Vec<String> = match params.get("providers") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => crate::universal_search::providers_param(v)?
            .into_iter()
            .map(|p| p.id().to_owned())
            .collect(),
    };
    let saved = save(world, scope, &name, query, &providers).map_err(|e| rpc(&e))?;
    let mut out = list_json(world);
    out["saved"] = json!({ "scope": scope.id(), "search": saved });
    Ok(out)
}

/// `editor.saved_search_delete { name, scope? }`.
fn saved_search_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?;
    let scope = scope_param(params)?;
    delete(world, scope, &name).map_err(|e| rpc(&e))?;
    let mut out = list_json(world);
    out["deleted"] = json!({ "scope": scope.id(), "name": name });
    Ok(out)
}

const P_SCOPE: ParamSchema = ParamSchema {
    name: "scope",
    kind: "string",
    required: false,
    description: "personal (the user config directory, default) or project (config/saved_searches.ron).",
};

/// The saved-search agent methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.saved_search_list",
                tool: "saved_search_list",
                description: "The universal search's saved searches: the user's (personal) and the project's \
                    (config/saved_searches.ron). Run one with editor.search { saved, scope }.",
                params: &[],
                result: "{ personal: [{ name, query, providers }], project: [...], personal_file, project_file, problems }",
                mutating: false,
                permission: Permission::Read,
            },
            saved_search_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.saved_search_save",
                tool: "saved_search_save",
                description: "Save a universal-search query under a name (replacing one of that name), for the \
                    user or shared with the project, as the search window's Save button does.",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "1 to 64 characters, no leading or trailing spaces.",
                    },
                    ParamSchema {
                        name: "query",
                        kind: "string",
                        required: true,
                        description: "The query text, prefix tokens included (\"e: kind:light\").",
                    },
                    P_SCOPE,
                    ParamSchema {
                        name: "providers",
                        kind: "array",
                        required: false,
                        description: "Provider ids the search is limited to (assets, entities, commands, settings, methods).",
                    },
                ],
                result: "{ saved: { scope, search }, personal, project, personal_file, project_file, problems }",
                mutating: true,
                permission: Permission::EditScene,
            },
            saved_search_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.saved_search_delete",
                tool: "saved_search_delete",
                description: "Delete a saved search by name, as its side-bar Delete button does.",
                params: &[
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: true,
                        description: "The saved search's name.",
                    },
                    P_SCOPE,
                ],
                result: "{ deleted: { scope, name }, personal, project, personal_file, project_file, problems }",
                mutating: true,
                permission: Permission::EditScene,
            },
            saved_search_delete as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn world_at(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::editor_prefs::EditorConfigPaths::at(dir.join("user")));
        world
    }

    fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods().into_iter().find(|(s, _)| s.name == name).unwrap();
        handler(world, &params)
    }

    fn code(e: RpcError) -> String {
        e.data.unwrap()["code"].as_str().unwrap().to_owned()
    }

    #[test]
    fn the_file_round_trips_and_drops_bad_rows() {
        let searches = vec![SavedSearch {
            name: "Lights".into(),
            query: "e: kind:light".into(),
            providers: vec!["entities".into()],
        }];
        let text = to_text(&searches, &["view.toggle_grid".into()]);
        let read = parse(&text).unwrap();
        assert_eq!(read.searches, searches);
        assert_eq!(read.favorites, ["view.toggle_grid"]);
        let read = parse(
            r#"(searches: [(name: "a", query: "x"), (name: "a", query: "y"), (name: " ", query: "z")])"#,
        )
        .unwrap();
        assert_eq!(
            read.searches.len(),
            1,
            "a duplicate name and a blank name are dropped"
        );
        assert_eq!(read.searches[0].query, "x");
        assert!(matches!(
            parse("(format_version: 9)"),
            Err(SavedSearchError::ReadOnly(_))
        ));
        assert!(matches!(
            parse("(format_version: 0)"),
            Err(SavedSearchError::Parse(_))
        ));
        assert!(matches!(parse("nonsense"), Err(SavedSearchError::Parse(_))));
        assert!(saved_searches_schema_fingerprint().contains("favorites"));
    }

    /// Review sprint-w12-rev-g1 #7: a row the read does not show (a name with a trailing space, a
    /// later duplicate) survives the next save of either file.
    #[test]
    fn rows_a_read_drops_are_written_back_by_the_next_save() {
        let dir = regia_test_utils::unique_temp_dir("saved-searches-kept");
        let text = r#"(format_version: 1, searches: [(name: "Lights ", query: "e: kind:light"), (name: "A", query: "x"), (name: "A", query: "y")])"#;
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::create_dir_all(dir.join("user")).unwrap();
        std::fs::write(root.join(PROJECT_FILE), text).unwrap();
        std::fs::write(dir.join("user").join(PERSONAL_FILE), text).unwrap();
        let mut world = world_at(&dir);
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        for scope in [Scope::Personal, Scope::Project] {
            save(&mut world, scope, "Props", "a: props/", &[]).unwrap();
        }
        for file in [dir.join("user").join(PERSONAL_FILE), root.join(PROJECT_FILE)] {
            let back = std::fs::read_to_string(&file).unwrap();
            let read = parse(&back).unwrap();
            let names: Vec<&str> = read.searches.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["A", "Props"], "{}", file.display());
            let kept: Vec<(&str, &str)> = read
                .kept
                .iter()
                .map(|s| (s.name.as_str(), s.query.as_str()))
                .collect();
            assert_eq!(
                kept,
                [("Lights ", "e: kind:light"), ("A", "y")],
                "{}: {back}",
                file.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn personal_searches_and_favorites_persist_in_the_user_directory() {
        let dir = regia_test_utils::unique_temp_dir("saved-searches-personal");
        let mut world = world_at(&dir);
        let out = call(
            &mut world,
            "editor.saved_search_save",
            json!({ "name": "Rocks", "query": "a: rock" }),
        )
        .unwrap();
        assert_eq!(out["saved"]["scope"], "personal");
        world
            .get_resource_or_insert_with(crate::command_palette::PaletteFavorites::default)
            .set("view.toggle_grid", true)
            .unwrap();
        assert!(persist_personal(&mut world));
        assert!(!persist_personal(&mut world), "nothing new to write");
        let text = std::fs::read_to_string(dir.join("user").join(PERSONAL_FILE)).unwrap();
        assert!(
            text.contains("Rocks") && text.contains("view.toggle_grid"),
            "{text}"
        );
        // A new session reads both back.
        let mut world = world_at(&dir);
        ensure_loaded(&mut world);
        assert_eq!(world.resource::<SavedSearches>().personal[0].query, "a: rock");
        assert_eq!(
            world.resource::<crate::command_palette::PaletteFavorites>().ids,
            ["view.toggle_grid"]
        );
        // Saving the name again replaces the query; delete removes it.
        save(&mut world, Scope::Personal, "Rocks", "a: rock -moss", &[]).unwrap();
        assert_eq!(world.resource::<SavedSearches>().personal.len(), 1);
        assert_eq!(
            world.resource::<SavedSearches>().personal[0].query,
            "a: rock -moss"
        );
        call(
            &mut world,
            "editor.saved_search_delete",
            json!({ "name": "Rocks" }),
        )
        .unwrap();
        assert!(world.resource::<SavedSearches>().personal.is_empty());
        assert_eq!(
            code(
                call(
                    &mut world,
                    "editor.saved_search_delete",
                    json!({ "name": "Rocks" })
                )
                .unwrap_err()
            ),
            "saved_search_not_found"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_searches_live_in_the_project_config() {
        let dir = regia_test_utils::unique_temp_dir("saved-searches-project");
        let mut world = world_at(&dir);
        assert_eq!(
            code(
                call(
                    &mut world,
                    "editor.saved_search_save",
                    json!({ "name": "Lights", "query": "e: kind:light", "scope": "project" })
                )
                .unwrap_err()
            ),
            "saved_search_no_project"
        );
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        call(
            &mut world,
            "editor.saved_search_save",
            json!({ "name": "Lights", "query": "e: kind:light", "scope": "project", "providers": ["entities"] }),
        )
        .unwrap();
        let text = std::fs::read_to_string(root.join(PROJECT_FILE)).unwrap();
        let read = parse(&text).unwrap();
        assert_eq!(read.searches[0].providers, ["entities"]);
        assert!(read.favorites.is_empty(), "favorites stay personal");
        let listed = call(&mut world, "editor.saved_search_list", json!({})).unwrap();
        assert_eq!(listed["project"][0]["name"], "Lights");
        assert_eq!(
            code(
                call(
                    &mut world,
                    "editor.saved_search_save",
                    json!({ "name": "", "query": "x" })
                )
                .unwrap_err()
            ),
            "saved_search_bad_name"
        );
        assert_eq!(
            code(
                call(
                    &mut world,
                    "editor.saved_search_save",
                    json!({ "name": "x", "query": "x", "scope": "team" })
                )
                .unwrap_err()
            ),
            "bad_scope"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_file_is_read_only() {
        let dir = regia_test_utils::unique_temp_dir("saved-searches-newer");
        std::fs::create_dir_all(dir.join("user")).unwrap();
        std::fs::write(
            dir.join("user").join(PERSONAL_FILE),
            "(format_version: 7, searches: [])",
        )
        .unwrap();
        let mut world = world_at(&dir);
        let err = save(&mut world, Scope::Personal, "x", "y", &[]).unwrap_err();
        assert_eq!(err.code(), "saved_search_read_only");
        let text = std::fs::read_to_string(dir.join("user").join(PERSONAL_FILE)).unwrap();
        assert!(text.contains("format_version: 7"), "left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every truncation of a rich file reads or fails with a message (never panics), and whatever
    /// reads writes back to bytes that read and write back identically.
    #[test]
    fn every_truncation_reads_or_errors_and_round_trips() {
        let text = to_text(
            &[
                SavedSearch {
                    name: "Lights \"quoted\"".into(),
                    query: "e: kind:light tag:lit".into(),
                    providers: vec!["entities".into(), "assets".into()],
                },
                SavedSearch {
                    name: "Rocks ▸ big".into(),
                    query: "a: type:mesh rock -moss".into(),
                    providers: Vec::new(),
                },
            ],
            &["view.toggle_grid".into(), "menu:File/Open level…".into()],
        );
        for end in (0..=text.len()).filter(|i| text.is_char_boundary(*i)) {
            match parse(&text[..end]) {
                Ok(read) => {
                    let again = to_text(&read.searches, &read.favorites);
                    let twice = parse(&again).unwrap();
                    assert_eq!(to_text(&twice.searches, &twice.favorites), again, "cut at {end}");
                }
                Err(err) => assert!(!err.to_string().is_empty(), "cut at {end}"),
            }
        }
    }
}
