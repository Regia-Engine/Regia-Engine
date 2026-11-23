// SPDX-License-Identifier: MPL-2.0
//! Project browser (`docs/specs/19-editor.md` § 6).
//!
//! Launching the editor without a project shows recent projects, **New project** with a template
//! picker (Blank, First Person, Third Person, Top Down; ADR-7234) and **Open folder**. Creating a
//! project calls `scaffold::create`, the editor's copy of `regia new` (the two Tools crates cannot
//! depend on each other).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::World;

thread_local! {
    /// Per-thread override so tests do not write the user's recent-projects file.
    static RECENT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Stores the recent-projects list at `path` for this thread.
///
/// The editor process leaves this unset and uses the user config directory. Tests set it so
/// opening a project does not touch `$HOME/.config`.
#[doc(hidden)]
pub fn set_recent_file_for_tests(path: PathBuf) {
    RECENT_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(path));
}

use crate::agent_ext::EditorProject;
use crate::autosave;
use crate::command::UndoStack;
use crate::level_io;

/// Form state for the project browser.
#[derive(Debug, Clone)]
pub struct ProjectBrowserState {
    /// Display name of the project to create.
    pub name: String,
    /// Directory the project folder is created in.
    pub parent: String,
    /// The last error or warning, shown under the form.
    pub status: String,
    /// The template id New project creates from (one of [`BROWSER_TEMPLATES`]).
    pub template: String,
}

/// The templates the project browser offers: `(id, label)`, the `regia new` ids minus `quick` and
/// `net_game` (those need the command line's options).
pub const BROWSER_TEMPLATES: &[(&str, &str)] = &[
    ("blank", "Blank"),
    ("first_person", "First Person"),
    ("third_person", "Third Person"),
    ("top_down", "Top Down"),
];

impl Default for ProjectBrowserState {
    fn default() -> Self {
        let parent = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        Self {
            name: "My Game".into(),
            parent,
            status: String::new(),
            template: "blank".into(),
        }
    }
}

/// What the browser's buttons asked for. The shell applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Open this directory (it must contain `Regia.toml`).
    Open(PathBuf),
    /// Create a project from [`ProjectBrowserState`]'s name, parent and template.
    Create,
}

/// A project the editor just opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedProject {
    /// The project root.
    pub root: PathBuf,
    /// Set when the project opened but its startup level did not.
    pub warning: Option<String>,
}

/// Draws the browser. Returns the button the user pressed this frame.
pub fn browser_panel(ui: &mut egui::Ui, state: &mut ProjectBrowserState) -> Option<BrowserAction> {
    let mut action = None;
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("Projects");
        ui.label("Open a project, or create one from a template.");
        ui.separator();
        // Thumbnails, tags, search and per-project actions (`recent_meta`).
        if let Some(path) = crate::recent_meta::recent_section(ui) {
            action = Some(BrowserAction::Open(path));
        }
        ui.separator();
        ui.label("New project");
        ui.horizontal(|ui| {
            ui.label("Name");
            let field = ui.text_edit_singleline(&mut state.name);
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Project name");
        });
        ui.horizontal(|ui| {
            ui.label("Folder");
            let field = ui.text_edit_singleline(&mut state.parent);
            crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Project folder");
            if ui.button("Browse…").clicked()
                && let Some(dir) = crate::file_dialog::FileDialog::new().pick_folder()
            {
                state.parent = dir.display().to_string();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Template");
            for (id, label) in BROWSER_TEMPLATES {
                if ui.selectable_label(state.template == *id, *label).clicked() {
                    state.template = (*id).to_owned();
                }
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Create project").clicked() {
                action = Some(BrowserAction::Create);
            }
            if ui.button("Open folder…").clicked()
                && let Some(dir) = crate::file_dialog::FileDialog::new().pick_folder()
            {
                action = Some(BrowserAction::Open(dir));
            }
        });
        if !state.status.is_empty() {
            ui.colored_label(ui.visuals().error_fg_color, &state.status);
        }
    });
    action
}

/// Opens `root` as the editor project and, when `Regia.toml` names one, its startup level.
///
/// # Errors
///
/// When `root` has no `Regia.toml`. A startup level that fails to open is a warning on the
/// [`OpenedProject`], not an error: the project itself is open.
pub fn open_project(world: &mut World, root: &Path) -> Result<OpenedProject, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    if !regia_core::brand::is_project_root(root) {
        return Err(format!(
            "{} is not a Regia Engine project (no Regia.toml)",
            root.display()
        ));
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    if !crate::safe_mode::enabled(world) {
        crate::extension::host::project_opened(world, &root);
    }
    // The project's `.kind.ron` declarations (spec 101 § 2.3, ADR-5609): the outliner shows data
    // kinds with their icon and color; the previous project's data kinds go away.
    let kinds = regia_scene::kind::load_project_kinds(world, &root);
    for (path, code, message) in &kinds.errors {
        tracing::warn!(target: "regia_editor", "{path}: {code}: {message}");
    }
    if let Some(mut settings) = world.get_resource_mut::<regia_procgen::world::ProcGenSettings>() {
        settings.terrain_edits = procgen_terrain_edits(&root);
    }
    // The previous project's level, undo history and selection do not carry over: start from an
    // empty level, then let the startup level (when there is one) replace it.
    level_io::reset_edit_state(world);
    if world.contains_resource::<regia_scene::prelude::LevelManager>() {
        let fresh = level_io::new_level(world);
        world.insert_resource(fresh);
    }
    let startup = if crate::safe_mode::enabled(world) {
        None
    } else {
        startup_level(&root)
    };
    let warning = match startup {
        Some(level) => match level_io::open_level_file(world, &level) {
            Ok(open) => {
                world.insert_resource(open);
                world
                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                    .mark_saved();
                None
            }
            Err(err) => Some(err),
        },
        None => None,
    };
    remember_recent(&root);
    if world
        .get_resource::<crate::asset_watch::LiveAssetReload>()
        .is_some_and(|live| live.enabled)
    {
        // The project's cooked cache becomes the edit world's asset server.
        if let Ok(server) = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: regia_asset::current_platform(),
            content_dir: None,
        }) {
            world.insert_resource(server);
        }
        if let Err(err) = crate::asset_watch::start_asset_watch(world, &root) {
            tracing::warn!(target: "regia_editor", "{err}");
        }
    }
    tracing::info!(target: "regia_editor", "opened project {}", crate::save_all_dialog::user_path(&root));
    Ok(OpenedProject { root, warning })
}

/// Creates a project from `template` under `parent` and opens it. Same scaffolding as `regia new`.
///
/// # Errors
///
/// The name, the template write, or opening the new project failed.
pub fn create_and_open(
    world: &mut World,
    name: &str,
    parent: &Path,
    template: &str,
) -> Result<OpenedProject, String> {
    let parent = if parent.as_os_str().is_empty() {
        std::env::current_dir().map_err(|err| format!("cannot read the current directory: {err}"))?
    } else {
        parent.to_path_buf()
    };
    let created = crate::scaffold::create(&crate::scaffold::NewProjectRequest {
        name: name.to_string(),
        template: template.to_string(),
        parent,
    })
    .map_err(|err| err.to_string())?;
    open_project(world, &created.root)
}

/// `[procgen] terrain_edits` from `Regia.toml` (specs/64 § 7.3): whether graphs may change the
/// heightfield at bake time. Default false.
#[must_use]
pub fn procgen_terrain_edits(root: &Path) -> bool {
    regia_core::brand::project_manifest(root)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| value.get("procgen")?.get("terrain_edits")?.as_bool())
        .unwrap_or(false)
}

/// `assets/<startup_level>` from `Regia.toml`, when that file exists.
#[must_use]
pub fn startup_level(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(regia_core::brand::project_manifest(root)?).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let rel = value.get("project")?.get("startup_level")?.as_str()?;
    // Stay under `assets/`: no absolute paths, drive prefixes or `..` segments.
    let escapes = Path::new(rel).components().any(|part| {
        !matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if escapes {
        tracing::warn!(target: "regia_editor", "ignoring startup_level `{rel}`: it leaves assets/");
        return None;
    }
    let path = root.join("assets").join(rel);
    path.is_file().then_some(path)
}

/// Recent project roots that still contain a `Regia.toml`, newest first.
#[must_use]
pub fn load_recents() -> Vec<PathBuf> {
    let file = recent_file();
    // Until this editor writes its own list, read the one kept before the rename (ADR-1429) or
    // before the recents file joined the shared config resolution; the next `remember_recent`
    // writes it to the new location.
    let Some(text) = read_recents_text(&file, |key| regia_core::brand::env_var_os(key), cfg!(windows)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| regia_core::brand::is_project_root(path))
        .take(12)
        .collect()
}

/// Reads the recent-projects list, falling back across the locations an older build wrote:
/// the pre-rename directory name ([`legacy_recent_file`]), then the pre-ADR-4825 Windows
/// launch-dependent bases ([`legacy_launch_recent_files`]). `None` when no candidate reads.
fn read_recents_text(
    file: &Path,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    windows: bool,
) -> Option<String> {
    std::fs::read_to_string(file)
        .ok()
        .or_else(|| legacy_recent_file(file).and_then(|legacy| std::fs::read_to_string(legacy).ok()))
        .or_else(|| {
            legacy_launch_recent_files(file, env, windows)
                .into_iter()
                .find_map(|legacy| std::fs::read_to_string(legacy).ok())
        })
}

/// Read-miss candidates from before the recents file joined the shared per-OS config
/// resolution (ADR-4825): on Windows an editor launched from Explorer kept its list at
/// `%USERPROFILE%\<main crate>\recent_projects.txt`, and one launched from Git Bash kept it
/// at `%HOME%\.config\<main crate>\recent_projects.txt`, each also under the legacy crate
/// name before the rename. The migration reads each candidate once instead of dropping the
/// list. Empty off Windows, when the environment names no base, and for candidates that
/// resolve to `file` itself.
fn legacy_launch_recent_files(
    file: &Path,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    windows: bool,
) -> Vec<PathBuf> {
    if !windows {
        return Vec::new();
    }
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(userprofile) = env("USERPROFILE") {
        bases.push(PathBuf::from(userprofile));
    }
    if let Some(home) = env("HOME") {
        bases.push(PathBuf::from(home).join(".config"));
    }
    let Some(name) = file.file_name() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for base in bases {
        for dir in [
            regia_core::brand::MAIN_CRATE,
            regia_core::brand::LEGACY_MAIN_CRATE,
        ] {
            let candidate = base.join(dir).join(name);
            if candidate != file && !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// Remembers `root` at the front of the recent-projects list (at most 12, dead entries dropped).
pub fn remember_recent(root: &Path) {
    let mut list = load_recents();
    list.retain(|path| path != root);
    list.insert(0, root.to_path_buf());
    list.truncate(12);
    let file = recent_file();
    if let Some(parent) = file.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(target: "regia_editor", "cannot create {}: {err}", parent.display());
        return;
    }
    let body: String = list.iter().map(|path| format!("{}\n", path.display())).collect();
    if let Err(err) = std::fs::write(&file, body) {
        tracing::warn!(target: "regia_editor", "cannot write {}: {err}", file.display());
    }
    crate::recent_meta::touch_opened(root);
}

/// Writes `list` as the recent-projects file (the browser's Remove from list).
///
/// # Errors
///
/// The directory or the file cannot be written.
pub(crate) fn write_recents(list: &[PathBuf]) -> Result<(), String> {
    let file = recent_file();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let body: String = list.iter().map(|path| format!("{}\n", path.display())).collect();
    std::fs::write(&file, body).map_err(|err| format!("cannot write {}: {err}", file.display()))
}

/// Where recent projects are stored: `recent_projects.txt` in the per-user editor config
/// directory ([`crate::editor_prefs::user_config_dir`]), the same directory the prefs, keymap
/// and layout resolve, so the list cannot drift to a launch-dependent location.
#[must_use]
pub fn recent_file() -> PathBuf {
    recent_file_from(
        RECENT_OVERRIDE.with(|slot| slot.borrow().clone()),
        regia_core::brand::env_var_os("REGIA_RECENT_PROJECTS"),
        |key| regia_core::brand::env_var_os(key),
        cfg!(windows),
    )
}

/// Pure resolution of [`recent_file`]: the per-thread test override, then
/// `$REGIA_RECENT_PROJECTS`, then the shared per-OS user-config resolver.
fn recent_file_from(
    overridden: Option<PathBuf>,
    recent_env: Option<std::ffi::OsString>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    windows: bool,
) -> PathBuf {
    if let Some(path) = overridden {
        return path;
    }
    if let Some(path) = recent_env {
        return PathBuf::from(path);
    }
    crate::editor_prefs::user_config_dir_from(env, windows).join("recent_projects.txt")
}

/// The recent-projects list the editor kept before the rename to Regia Engine: the same file under
/// the legacy directory name (`<config>/<legacy main crate>/recent_projects.txt`). `None` when
/// `file` is not in a directory named after the engine, and before the rename.
fn legacy_recent_file(file: &Path) -> Option<PathBuf> {
    let dir = file.parent()?;
    if dir.file_name()? != regia_core::brand::MAIN_CRATE {
        return None;
    }
    let legacy = dir
        .parent()?
        .join(regia_core::brand::LEGACY_MAIN_CRATE)
        .join(file.file_name()?);
    (legacy != file).then_some(legacy)
}

/// Reloads crash-recovery candidates for a project the shell just adopted.
#[must_use]
pub fn recovery_for(root: &Path) -> Vec<autosave::RecoveryCandidate> {
    autosave::recovery_candidates(root)
        .into_iter()
        .filter(autosave::should_offer_recovery)
        .collect()
}

/// Directories a project copy leaves out at the project root: build output, packaged builds, the
/// version-control store and the editor state (cooked cache, asset database, autosaves, version
/// history). Anywhere below the root, a directory holding a `CACHEDIR.TAG` (a cargo `target/`)
/// and any `.regia/` or `.ferrite/` is left out too.
pub const COPY_SKIP_ROOT_DIRS: &[&str] = &["target", "build", "dist", ".git", ".regia", ".ferrite"];

/// Why a project copy was refused or failed: an agent error code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyError {
    /// `not_a_project`, `duplicate_dest_exists`, `duplicate_dest_inside` or `duplicate_failed`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

/// What a project copy wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// The new project's root.
    pub root: PathBuf,
    /// Files copied.
    pub files: u64,
    /// Bytes copied.
    pub bytes: u64,
    /// What was left out, project-relative (`/`-separated): skipped directories and symbolic links.
    pub skipped: Vec<String>,
}

/// Whether the copy leaves the directory `rel` (project-relative) out.
fn skip_dir(dir: &Path, rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let at_root = rel.components().count() == 1;
    (at_root && COPY_SKIP_ROOT_DIRS.contains(&name))
        || name == regia_core::brand::PROJECT_STATE_DIR
        || name == regia_core::brand::LEGACY_PROJECT_STATE_DIR
        || dir.join("CACHEDIR.TAG").is_file()
}

/// Copies the project's source files from `source` into `dest` (created), leaving out
/// [`COPY_SKIP_ROOT_DIRS`] and caches. Symbolic links are not followed (they are listed as
/// skipped). Returns what was copied.
///
/// # Errors
///
/// A directory could not be read or a file could not be copied.
pub fn copy_project_files(source: &Path, dest: &Path) -> std::io::Result<CopyReport> {
    let mut report = CopyReport {
        root: dest.to_path_buf(),
        ..CopyReport::default()
    };
    std::fs::create_dir_all(dest)?;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut entries: Vec<std::fs::DirEntry> =
            std::fs::read_dir(source.join(&rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let child = rel.join(entry.file_name());
            let shown = child.to_string_lossy().replace('\\', "/");
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                report.skipped.push(shown);
            } else if kind.is_dir() {
                if skip_dir(&entry.path(), &child) {
                    report.skipped.push(format!("{shown}/"));
                } else {
                    std::fs::create_dir_all(dest.join(&child))?;
                    stack.push(child);
                }
            } else {
                report.bytes += std::fs::copy(entry.path(), dest.join(&child))?;
                report.files += 1;
            }
        }
    }
    report.skipped.sort();
    Ok(report)
}

/// File ▸ Duplicate project…: copies the project at `source` to the new folder `dest` (source files
/// only, see [`copy_project_files`]). The copy goes to a staging folder beside `dest` and is renamed
/// into place, so a failed copy leaves no half project behind. The open project stays open.
///
/// # Errors
///
/// `source` is not a project, `dest` exists (and is not an empty folder) or lies inside `source`,
/// or the copy failed.
pub fn duplicate_project(source: &Path, dest: &Path) -> Result<CopyReport, CopyError> {
    let fail = |code, message: String| CopyError { code, message };
    if !regia_core::brand::is_project_root(source) {
        return Err(fail(
            "not_a_project",
            format!(
                "{} is not a Regia Engine project (no Regia.toml)",
                source.display()
            ),
        ));
    }
    let source = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
    let empty_dir = std::fs::read_dir(dest).is_ok_and(|mut d| d.next().is_none());
    if dest.exists() && !empty_dir {
        return Err(fail(
            "duplicate_dest_exists",
            format!("{} already exists; choose a new folder", dest.display()),
        ));
    }
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent).map_err(|e| {
        fail(
            "duplicate_failed",
            format!("cannot create {}: {e}", parent.display()),
        )
    })?;
    let parent = parent.canonicalize().unwrap_or(parent);
    if parent.starts_with(&source) {
        return Err(fail(
            "duplicate_dest_inside",
            format!(
                "{} is inside the project; choose a folder outside it",
                dest.display()
            ),
        ));
    }
    let name = dest
        .file_name()
        .map_or_else(|| "project".into(), |n| n.to_string_lossy().into_owned());
    let staging = parent.join(format!(".{name}.copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let copied = copy_project_files(&source, &staging).and_then(|mut report| {
        if empty_dir {
            std::fs::remove_dir(dest)?;
        }
        std::fs::rename(&staging, dest)?;
        report.root = dest.to_path_buf();
        Ok(report)
    });
    copied.map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        fail(
            "duplicate_failed",
            format!("copying {} to {}: {e}", source.display(), dest.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;
    use regia_scene::prelude::Name;

    fn unique_dir(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("regia-project-{label}-{n}-{}", std::process::id()))
    }

    fn temp_project(label: &str, manifest_extra: &str) -> PathBuf {
        let dir = unique_dir(label);
        std::fs::create_dir_all(dir.join("assets/levels")).expect("dirs");
        std::fs::write(
            dir.join("Regia.toml"),
            format!("[project]\nname = \"t\"\n{manifest_extra}\n"),
        )
        .expect("manifest");
        dir
    }

    fn recent_file_windows_env() -> impl Fn(&str) -> Option<std::ffi::OsString> {
        |key| match key {
            "APPDATA" => Some(std::ffi::OsString::from(r"C:\Users\dev\AppData\Roaming")),
            "HOME" => Some(std::ffi::OsString::from(r"C:\Users\dev")),
            "XDG_CONFIG_HOME" => Some(std::ffi::OsString::from(r"C:\Users\dev\.config")),
            _ => None,
        }
    }

    fn recent_file_xdg_env() -> impl Fn(&str) -> Option<std::ffi::OsString> {
        |key| match key {
            "XDG_CONFIG_HOME" => Some(std::ffi::OsString::from("/home/dev/.xdg")),
            "HOME" => Some(std::ffi::OsString::from("/home/dev")),
            _ => None,
        }
    }

    #[test]
    fn recent_file_uses_the_same_per_os_resolver_as_the_prefs() {
        // Regression (audit fix-editor5-platform, Med): the old recent_file chain resolved
        // XDG_CONFIG_HOME/HOME first and had no APPDATA at all, so a Windows editor started
        // from Git Bash kept its recents next to that shell's HOME while prefs moved to
        // APPDATA. Now both resolve through editor_prefs::user_config_dir.
        assert_eq!(
            recent_file_from(None, None, recent_file_windows_env(), true),
            PathBuf::from(r"C:\Users\dev\AppData\Roaming")
                .join("regia")
                .join("recent_projects.txt")
        );
        // The same env elsewhere keeps the XDG order and agrees with user_config_dir.
        let recent = recent_file_from(None, None, recent_file_xdg_env(), false);
        assert_eq!(recent, PathBuf::from("/home/dev/.xdg/regia/recent_projects.txt"));
        assert_eq!(
            recent,
            crate::editor_prefs::user_config_dir_from(recent_file_xdg_env(), false)
                .join("recent_projects.txt")
        );
        // The per-thread test override and REGIA_RECENT_PROJECTS still win.
        assert_eq!(
            recent_file_from(
                Some(PathBuf::from("/tmp/override.txt")),
                None,
                |_: &str| None,
                false
            ),
            PathBuf::from("/tmp/override.txt")
        );
        assert_eq!(
            recent_file_from(
                None,
                Some(std::ffi::OsString::from("/tmp/recent.txt")),
                |_: &str| None,
                false,
            ),
            PathBuf::from("/tmp/recent.txt")
        );
    }

    #[test]
    fn legacy_launch_miss_reads_the_old_windows_locations() {
        // Regression (rev-kimi-16, fixrev-09-k Low 5): a pre-ADR-4825 Windows build launched
        // from Git Bash kept its recents at %HOME%\.config\regia\recent_projects.txt (from
        // Explorer at %USERPROFILE%\regia\recent_projects.txt); the read now falls back there
        // once so the list is not lost. The old load_recents returned an empty list here.
        let dir = unique_dir("recents-legacy-launch");
        let current = dir.join("appdata").join("regia").join("recent_projects.txt");
        std::fs::create_dir_all(current.parent().expect("parent")).expect("dirs");
        let git_bash_old = dir.join("home").join(".config").join("regia");
        std::fs::create_dir_all(&git_bash_old).expect("dirs");
        std::fs::write(git_bash_old.join("recent_projects.txt"), "/old/project\n").expect("write");
        let env = |key: &str| match key {
            "USERPROFILE" => Some(dir.join("profile").into_os_string()),
            "HOME" => Some(dir.join("home").into_os_string()),
            _ => None,
        };
        // (Windows behavior exercised on every host: the candidates themselves are pure, and
        // the fs fallback just reads them in order.)
        assert_eq!(
            read_recents_text(&current, env, true),
            Some("/old/project\n".to_string())
        );
    }

    #[test]
    fn legacy_launch_candidates_cover_both_old_bases_and_names() {
        let dir = unique_dir("recents-legacy-candidates");
        let current = dir.join("appdata").join("regia").join("recent_projects.txt");
        let env = |key: &str| match key {
            "USERPROFILE" => Some(dir.join("profile").into_os_string()),
            "HOME" => Some(dir.join("home").into_os_string()),
            _ => None,
        };
        let recent = "recent_projects.txt";
        assert_eq!(
            legacy_launch_recent_files(&current, env, true),
            vec![
                dir.join("profile").join("regia").join(recent),
                dir.join("profile").join("ferrite").join(recent), // rename-keep: legacy config dir
                dir.join("home").join(".config").join("regia").join(recent),
                dir.join("home").join(".config").join("ferrite").join(recent), // rename-keep: legacy config dir
            ]
        );
        // Off Windows the migration is a no-op, even with the variables set.
        assert!(legacy_launch_recent_files(&current, env, false).is_empty());
        // A candidate that resolves to the current file is dropped.
        let profile_current = dir.join("profile").join("regia").join(recent);
        let env = |key: &str| (key == "USERPROFILE").then(|| dir.join("profile").into_os_string());
        assert_eq!(
            legacy_launch_recent_files(&profile_current, env, true),
            vec![dir.join("profile").join("ferrite").join(recent)] // rename-keep: legacy config dir
        );
    }

    #[test]
    fn startup_level_stays_under_assets() {
        let dir = temp_project("startup-level", "startup_level = \"levels/main.level.ron\"");
        std::fs::write(
            dir.join("assets/levels/main.level.ron"),
            "( format_version: 1, entities: [] )",
        )
        .expect("level");
        assert_eq!(
            startup_level(&dir),
            Some(dir.join("assets").join("levels/main.level.ron"))
        );
        let outside = unique_dir("outside").with_extension("level.ron");
        std::fs::write(&outside, "x").expect("outside");
        for bad in [outside.display().to_string(), "../outside.level.ron".to_string()] {
            std::fs::write(
                dir.join("Regia.toml"),
                format!("[project]\nname = \"t\"\nstartup_level = {bad:?}\n"),
            )
            .expect("manifest");
            assert_eq!(startup_level(&dir), None, "{bad}");
        }
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_recent_list_kept_before_the_rename_is_read_then_rewritten() {
        let config = unique_dir("recent-legacy");
        let file = config
            .join(regia_core::brand::MAIN_CRATE)
            .join("recent_projects.txt");
        let legacy = config
            .join(regia_core::brand::LEGACY_MAIN_CRATE)
            .join("recent_projects.txt");
        set_recent_file_for_tests(file.clone());
        let project = temp_project("recent-legacy-project", "");
        if file == legacy {
            // Before the rename both names are the same file.
            assert_eq!(legacy_recent_file(&file), None);
            return;
        }
        std::fs::create_dir_all(legacy.parent().expect("parent")).expect("legacy dir");
        std::fs::write(&legacy, format!("{}\n", project.display())).expect("legacy list");
        assert_eq!(legacy_recent_file(&file), Some(legacy.clone()));
        assert_eq!(load_recents(), vec![project.clone()], "the old list is read");
        let other = temp_project("recent-legacy-other", "");
        remember_recent(&other);
        assert_eq!(
            std::fs::read_to_string(&file).expect("new list written"),
            format!("{}\n{}\n", other.display(), project.display()),
            "the old entries carry over to the new file"
        );
        assert!(legacy.is_file(), "the old list is left for older editors");
        assert_eq!(legacy_recent_file(Path::new("/tmp/elsewhere/recent.txt")), None);
        for dir in [config, project, other] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn opening_a_project_drops_the_previous_level_and_its_undo_stack() {
        set_recent_file_for_tests(unique_dir("recent").join("recent.txt"));
        let dir = temp_project("open-fresh", "");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut().insert_resource(UndoStack::new());
        let mut cmd = crate::command::SpawnEntities::new_entity(
            app.world(),
            regia_core::id::Guid::new_random(),
            "Old",
            regia_scene::prelude::Transform::default(),
        )
        .expect("spawn");
        crate::command::EditorCommand::apply(&mut cmd, app.world_mut());
        app.world_mut().resource_mut::<UndoStack>().push(Box::new(cmd));

        let opened = open_project(app.world_mut(), &dir).expect("open");
        assert!(opened.warning.is_none());
        let world = app.world_mut();
        let names: Vec<String> = world
            .query::<&Name>()
            .iter(world)
            .map(|name| name.as_str().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == "Old"), "{names:?}");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
        assert!(world.resource::<level_io::OpenLevel>().path.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 101 § 4.1, ADR-5609: opening a project loads its `.kind.ron` declarations, so a tagged
    /// entity's outliner row shows the kind's icon and color, not the "not declared" generic icon.
    #[test]
    fn opening_a_project_loads_its_data_kinds_for_the_outliner() {
        set_recent_file_for_tests(unique_dir("recent-kinds").join("recent.txt"));
        let dir = temp_project("open-kinds", "");
        std::fs::create_dir_all(dir.join("assets/kinds")).expect("kinds dir");
        std::fs::write(
            dir.join("assets/kinds/pickup.kind.ron"),
            r##"(format_version: 1, name: "Pickup", icon: "coin", color: "#30c050", description: "Something to pick up", category: Some("Items"))"##,
        )
        .expect("kind");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        open_project(app.world_mut(), &dir).expect("open");
        let tagged = app
            .world_mut()
            .spawn(regia_scene::kind::KindTag("Pickup".into()))
            .id();
        let icon = crate::kind_icons::row_icon(app.world(), tagged);
        assert_eq!(icon.kind.as_deref(), Some("Pickup"));
        assert_eq!(icon.glyph, crate::kind_icons::glyph("coin"));
        assert_eq!(icon.color, Some([0x30, 0xc0, 0x50]));
        assert_eq!(icon.category, "Items");
        assert!(
            icon.tooltip.starts_with("kind: Pickup\nSomething to pick up"),
            "{}",
            icon.tooltip
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tree(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, file.as_bytes()).expect("file");
        }
    }

    fn listing(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read").flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push(
                        entry
                            .path()
                            .strip_prefix(root)
                            .expect("inside")
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        out.sort();
        out
    }

    /// P-327: File ▸ Duplicate project… copies the source files and leaves out build output,
    /// version control, editor state and caches.
    #[test]
    fn duplicate_copies_sources_and_skips_targets_and_caches() {
        let source = unique_dir("dup-source");
        tree(
            &source,
            &[
                "Regia.toml",
                "assets/maps/main.level.ron",
                "assets/build/kept.txt",
                "src/lib.rs",
                "target/debug/game.so",
                "build/android/app.apk",
                "dist/game.pak",
                ".git/HEAD",
                ".regia/history/x/target.ron",
                ".regia/cache/blob",
                "assets/.regia/stray",
                "tools/helper/target/CACHEDIR.TAG",
                "tools/helper/target/out.o",
                "tools/helper/src/main.rs",
            ],
        );
        let dest = unique_dir("dup-dest");
        let report = duplicate_project(&source, &dest).expect("duplicate");
        assert_eq!(
            listing(&dest),
            vec![
                "Regia.toml",
                "assets/build/kept.txt",
                "assets/maps/main.level.ron",
                "src/lib.rs",
                "tools/helper/src/main.rs",
            ],
            "{report:?}"
        );
        assert_eq!(report.files, 5);
        assert!(report.skipped.contains(&"target/".to_string()), "{report:?}");
        assert!(
            report.skipped.contains(&"tools/helper/target/".to_string()),
            "{report:?}"
        );
        assert!(report.skipped.contains(&".regia/".to_string()), "{report:?}");
        assert_eq!(
            std::fs::read_to_string(dest.join("src/lib.rs")).expect("copied"),
            "src/lib.rs"
        );
        // No staging folder is left beside the copy.
        let parent = dest.parent().expect("parent");
        let name = dest.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(
            !parent
                .join(format!(".{name}.copy-{}", std::process::id()))
                .exists()
        );

        // An existing folder with files, a folder inside the project and a non-project are refused.
        assert_eq!(
            duplicate_project(&source, &dest).expect_err("exists").code,
            "duplicate_dest_exists"
        );
        assert_eq!(
            duplicate_project(&source, &source.join("assets/copy"))
                .expect_err("inside")
                .code,
            "duplicate_dest_inside"
        );
        assert_eq!(
            duplicate_project(&source.join("assets"), &unique_dir("dup-other"))
                .expect_err("not a project")
                .code,
            "not_a_project"
        );
        // An empty folder is fine.
        let empty = unique_dir("dup-empty");
        std::fs::create_dir_all(&empty).expect("empty");
        assert_eq!(duplicate_project(&source, &empty).expect("into empty").files, 5);
        for dir in [&source, &dest, &empty] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
