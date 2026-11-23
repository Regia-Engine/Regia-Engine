// SPDX-License-Identifier: MPL-2.0
//! The editor as an extension host (`specs/57` § 2.8, § 3.4): resolves the project's extensions
//! with the `editor` audience, loads the dynamic ones behind the stamp, builds everything through
//! the runtime `ExtensionPlugin`, and swaps a library live for `plugin.reload`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::{self, RpcError};
use regia_app::prelude::*;
use regia_asset_pipeline::extension_import::ImportOutput;
use regia_framework::extension::discover::{Discovery, MANIFEST_FILE, hash_hex};
use regia_framework::extension::manifest::CodeEntry;
use regia_framework::extension::registry::DynamicRecord;
use regia_framework::extension::stamp::{BUILD_TARGET, ExtensionStamp};
use regia_framework::extension::{
    ExtensionPlugin, ExtensionRegistry, ExtensionsConfig, RegiaExtension, RegistryInputs, reload_in_world,
};
use serde_json::{Value, json};

use super::context::EditorExtensions;
#[cfg(test)]
use super::context::PanelDesc;
use super::load::{ExtensionLoadError, LoadedExtension, expected_stamp_for, load_extension};
use crate::hot_reload::dylib_extension;

/// One dynamic extension in this process.
struct LoadedLibrary {
    /// The extension; the old one of a reload stays mapped, and is dropped here only as a value.
    extension: Arc<dyn RegiaExtension>,
    /// The library the manifest points at (the source of the next reload).
    source: PathBuf,
}

/// Dynamic extensions of this editor.
#[derive(Resource)]
pub struct DynamicExtensions {
    loaded: BTreeMap<String, LoadedLibrary>,
    /// The target directory the engine dylib was linked in (the fingerprint's input).
    pub target_dir: PathBuf,
    /// The engine fingerprint at start, which stands for the engine this process has mapped.
    pub fingerprint: u64,
    /// Where unique library copies go (`.regia/hot/ext`).
    pub hot_dir: PathBuf,
    /// The engine directory whose `extensions/` is discovered after the project's (ADR-9850).
    pub engine_dir: Option<PathBuf>,
}

impl DynamicExtensions {
    /// Ids of the loaded dynamic extensions, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.loaded.keys().map(String::as_str)
    }
}

/// The cargo target directory of this executable: `<target>/<profile>/<exe>`.
#[must_use]
pub fn host_target_dir() -> PathBuf {
    regia_core::brand::env_var("REGIA_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            Some(exe.parent()?.parent()?.to_path_buf())
        })
        .unwrap_or_else(|| PathBuf::from("target"))
}

/// The library of a code entry: `library` from the manifest, else the file `regia plugin build`
/// writes (`bin/<target-triple>/<lib><crate>.<ext>`) when it exists.
#[must_use]
pub fn library_path(dir: &Path, entry: &CodeEntry) -> Option<PathBuf> {
    if let Some(library) = &entry.library {
        return Some(dir.join(library));
    }
    let name = entry.crate_name.as_deref()?;
    let prefix = if cfg!(windows) { "" } else { "lib" };
    let built = dir
        .join("bin")
        .join(BUILD_TARGET)
        .join(format!("{prefix}{name}.{}", dylib_extension()));
    built.is_file().then_some(built)
}

fn hot_dir(project: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project)
        .join("hot")
        .join("ext")
}

fn load_one(
    dir: &Path,
    id: &str,
    source: &Path,
    fingerprint: u64,
    hot: &Path,
    generation: u64,
) -> Result<LoadedExtension, ExtensionLoadError> {
    let manifest = std::fs::read(dir.join(MANIFEST_FILE))
        .map_err(|error| ExtensionLoadError::LoadFailed(format!("{MANIFEST_FILE}: {error}")))?;
    let expected: ExtensionStamp = expected_stamp_for(&manifest, fingerprint);
    load_extension(source, &expected, hot, id, generation)
}

fn record(loaded: &LoadedExtension) -> DynamicRecord {
    DynamicRecord {
        library: loaded.library.display().to_string(),
        stamp: loaded.stamp.describe(),
    }
}

/// The engine directory the editor discovers bundled extensions under (`<engine>/extensions/`,
/// ADR-9850 (a)): `REGIA_ENGINE_DIR` when set, else the engine checkout the editor runs from
/// (found at run time, ADR-10544) when it has an `extensions/` directory, else none.
#[must_use]
pub fn default_engine_dir() -> Option<PathBuf> {
    if let Ok(dir) = regia_core::brand::env_var("REGIA_ENGINE_DIR") {
        return Some(PathBuf::from(dir));
    }
    crate::scaffold::engine_checkout().filter(|root| root.join("extensions").is_dir())
}

/// Installs the extension registry for the editor: audience `editor`, dynamic libraries loaded.
pub struct EditorExtensionPlugin {
    /// The project directory; `None` until the project browser opens one ([`project_opened`]).
    pub project: Option<PathBuf>,
    /// The engine's target directory (default [`host_target_dir`]).
    pub target_dir: Option<PathBuf>,
    /// The engine directory: `<engine>/extensions/` is discovered after the project's roots, and a
    /// project extension with the same id overrides an engine one (ADR-9850 (a); the editor passes
    /// [`default_engine_dir`]).
    pub engine_dir: Option<PathBuf>,
    /// Extensions linked into this editor build (built like a dynamic one once resolved; tests and
    /// hosts that compile an extension in).
    pub linked: Vec<Arc<dyn RegiaExtension>>,
}

impl Plugin for EditorExtensionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EditorExtensions>();
        let target_dir = self.target_dir.clone().unwrap_or_else(host_target_dir);
        let hot = hot_dir(self.project.as_deref().unwrap_or(Path::new(".")));
        let mut plugin = host_plugin(self.project.as_deref(), self.engine_dir.clone());
        plugin.linked.extend(self.linked.iter().cloned());
        let fingerprint = crate::hot_reload::engine_fingerprint(&target_dir);

        let mut loaded = BTreeMap::new();
        let mut records = Vec::new();
        let mut failures = Vec::new();
        let preview = plugin.resolve_now();
        for resolved in &preview.resolved().order {
            let Some(found) = preview.discovered(&resolved.id) else {
                continue;
            };
            let dir = PathBuf::from(&found.path);
            let source = found
                .manifest
                .code
                .iter()
                .filter(|entry| entry.audience.iter().any(|a| a == "editor"))
                .find_map(|entry| library_path(&dir, entry));
            let Some(source) = source else { continue };
            match load_one(&dir, &resolved.id, &source, fingerprint, &hot, 0) {
                Ok(library) => {
                    records.push((resolved.id.clone(), record(&library)));
                    plugin.linked.push(Arc::clone(&library.extension));
                    loaded.insert(
                        resolved.id.clone(),
                        LoadedLibrary {
                            extension: library.extension,
                            source,
                        },
                    );
                }
                Err(error) => failures.push((resolved.id.clone(), error)),
            }
        }
        plugin.build(app);
        let world = app.world_mut();
        if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
            for (id, dynamic) in records {
                registry.record_dynamic(&id, dynamic);
            }
            for (id, error) in failures {
                registry.mark_failed(&id, error.code(), error.to_string());
            }
        }
        world.insert_resource(DynamicExtensions {
            loaded,
            target_dir,
            fingerprint,
            hot_dir: hot,
            engine_dir: self.engine_dir.clone(),
        });
        install_reload(world);
    }

    fn name(&self) -> &str {
        "regia_editor::extension::EditorExtensionPlugin"
    }
}

/// The runtime plugin as the editor configures it: audience `editor`, every protocol namespace
/// reserved. Without a project it resolves an empty set.
fn host_plugin(project: Option<&Path>, engine_dir: Option<PathBuf>) -> ExtensionPlugin {
    let mut plugin = match project {
        Some(project) => {
            ExtensionPlugin::for_project(project.to_path_buf(), Vec::new()).with_engine(engine_dir)
        }
        None => ExtensionPlugin::from_inputs(
            RegistryInputs::for_host(None, ExtensionsConfig::default(), Discovery::default()),
            Vec::new(),
        ),
    };
    plugin.audience = Some("editor".to_owned());
    plugin.reserved_ids = Some(regia_agent::plugins::reserved_namespaces());
    plugin
}

/// The project browser opened `root`: reads its extensions. Content mounts now; code loads at the
/// next start (`plugin.list` reports it under `restart_required`). Does nothing for the project
/// the editor started with, whose extensions are already built.
pub fn project_opened(world: &mut World, root: &Path) {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let current = world
        .get_resource::<ExtensionRegistry>()
        .and_then(|registry| registry.inputs().project_dir.as_deref().map(canonical));
    if !world.contains_resource::<ExtensionRegistry>()
        || current.as_deref() == Some(canonical(root).as_path())
    {
        return;
    }
    let engine_dir = world
        .get_resource::<DynamicExtensions>()
        .and_then(|dynamic| dynamic.engine_dir.clone());
    let mut registry = host_plugin(Some(root), engine_dir).resolve_now();
    registry.activate_without_code();
    world.insert_resource(registry);
    if let Some(mut dynamic) = world.get_resource_mut::<DynamicExtensions>() {
        dynamic.hot_dir = hot_dir(root);
    }
}

/// Gives `plugin.reload` to the editor's handler (the built-in row stays the schema).
fn install_reload(world: &mut World) {
    let Some(row) = regia_agent::schema::method("plugin.reload").cloned() else {
        return;
    };
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(row, reload);
    world.insert_resource(extension);
}

/// Runs importers extensions registered over `sources`, writing their output under
/// `assets/<folder>/`. Returns the sources no importer took, for the built-in import, and the
/// outcome of the rest. Built-in file types never reach an extension.
#[must_use]
pub fn run_importers(
    world: &World,
    project: &Path,
    folder: &str,
    sources: &[PathBuf],
) -> (Vec<PathBuf>, crate::content::ImportReport) {
    let mut report = crate::content::ImportReport::default();
    let Some(editor) = world.get_resource::<EditorExtensions>() else {
        return (sources.to_vec(), report);
    };
    let mut rest = Vec::new();
    for source in sources {
        let builtin = source.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            crate::content::importable_extensions().contains(&e.to_ascii_lowercase().as_str())
        });
        let Some((_, importer)) = editor.importers.for_file(source).filter(|_| !builtin) else {
            rest.push(source.clone());
            continue;
        };
        let mut out = ImportOutput::default();
        if let Err(error) = importer.import(source, &mut out) {
            report
                .errors
                .push((source.display().to_string(), error.to_string()));
            continue;
        }
        let dir = project.join("assets").join(folder.trim_matches('/'));
        for (name, bytes) in out.files {
            let written = regia_core::project_path::check_relative(&name)
                .map_err(|e| format!("{name}: {e}"))
                .and_then(|relative| {
                    let path = dir.join(relative);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
                    std::fs::write(&tmp, bytes)
                        .and_then(|()| std::fs::rename(&tmp, &path))
                        .map_err(|e| e.to_string())
                });
            match written {
                Ok(()) => report
                    .imported
                    .push(format!("assets/{}/{name}", folder.trim_matches('/')).replace("//", "/")),
                Err(message) => report.errors.push((source.display().to_string(), message)),
            }
        }
    }
    (rest, report)
}

/// Every file extension the Content panel's import offers: the built-in ones and those extension
/// importers registered.
#[must_use]
pub fn import_file_types(world: &World) -> Vec<String> {
    let mut types: Vec<String> = crate::content::importable_extensions()
        .iter()
        .map(|e| (*e).to_owned())
        .collect();
    if let Some(editor) = world.get_resource::<EditorExtensions>() {
        types.extend(editor.importers.file_extensions().map(str::to_owned));
    }
    types.sort();
    types.dedup();
    types
}

/// Imports `sources`: file types only an extension's importer takes go there first, the rest
/// through the built-in import.
///
/// # Errors
///
/// As [`crate::content::import_files`].
pub fn import_with_extensions(
    world: &World,
    project: &Path,
    folder: &str,
    sources: &[PathBuf],
    client: Option<&str>,
) -> Result<crate::content::ImportReport, String> {
    let (rest, from_extensions) = run_importers(world, project, folder, sources);
    let mut report = if rest.is_empty() {
        crate::content::ImportReport::default()
    } else {
        crate::content::import_files(project, folder, &rest, client)?
    };
    report.imported.extend(from_extensions.imported);
    report.errors.extend(from_extensions.errors);
    Ok(report)
}

fn refuse(code: i64, reason: &str, id: &str, detail: String) -> RpcError {
    RpcError {
        code,
        message: format!("{reason}: {detail}"),
        data: Some(
            json!({ "code": reason, "id": id, "detail": detail, "hint": "see book/agents/plugins.md" }),
        ),
    }
}

/// `plugin.reload`: loads a new copy behind the stamp, then swaps registrations. Nothing changes
/// until the new copy passed every check, so a refused library leaves the old one in place; a
/// failing `build` restores the old registrations.
///
/// # Errors
///
/// `abi_mismatch`, `not_an_extension`, `load_failed`, `reload_failed`, `restart_required` when the
/// manifest changed (its capabilities were checked at start and the set may resolve differently),
/// and the runtime `plugin.reload` answers for an extension that is not dynamic here.
pub fn reload(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("agent_plugin", method = "reload").entered();
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let hosted = world
        .get_resource::<DynamicExtensions>()
        .is_some_and(|host| host.loaded.contains_key(&id));
    if !hosted {
        return regia_agent::plugins::reload(world, params);
    }
    let generation = world
        .get_resource::<ExtensionRegistry>()
        .map_or(0, |registry| registry.generation(&id))
        + 1;
    let found = world
        .get_resource::<ExtensionRegistry>()
        .and_then(|registry| registry.discovered(&id))
        .map(|found| (PathBuf::from(&found.path), found.hash.clone()));
    let Some((dir, started_hash)) = found else {
        return regia_agent::plugins::reload(world, params);
    };
    if std::fs::read(dir.join(MANIFEST_FILE)).is_ok_and(|bytes| hash_hex(&bytes) != started_hash) {
        return Err(refuse(
            protocol::UNSUPPORTED,
            "restart_required",
            &id,
            format!("{MANIFEST_FILE} of `{id}` changed since start; restart to resolve it again"),
        ));
    }
    let (old, source, fingerprint, hot) = {
        let host = world.resource::<DynamicExtensions>();
        let Some(entry) = host.loaded.get(&id) else {
            return regia_agent::plugins::reload(world, params);
        };
        (
            Arc::clone(&entry.extension),
            entry.source.clone(),
            host.fingerprint,
            host.hot_dir.clone(),
        )
    };
    let new = load_one(&dir, &id, &source, fingerprint, &hot, generation).map_err(|error| {
        let code = if matches!(error, ExtensionLoadError::LoadFailed(_)) {
            protocol::INTERNAL_ERROR
        } else {
            protocol::INVALID_PARAMS
        };
        refuse(code, error.code(), &id, error.to_string())
    })?;
    let editor_snapshot = world
        .get_resource_mut::<EditorExtensions>()
        .map(|mut editor| editor.take_owner(&id));
    let report = match reload_in_world(world, old.as_ref(), new.extension.as_ref()) {
        Ok(report) => report,
        Err((code, detail)) => {
            if let (Some(snapshot), Some(mut editor)) =
                (editor_snapshot, world.get_resource_mut::<EditorExtensions>())
            {
                editor.restore(&id, snapshot);
            }
            return Err(refuse(protocol::INTERNAL_ERROR, code, &id, detail));
        }
    };
    if let Some(mut registry) = world.get_resource_mut::<ExtensionRegistry>() {
        registry.record_dynamic(&id, record(&new));
    }
    if let Some(entry) = world.resource_mut::<DynamicExtensions>().loaded.get_mut(&id) {
        entry.extension = new.extension;
    }
    // Replays across a reload are not reproducible (specs/57 § 7).
    tracing::warn!(target: "regia_editor", code = "nondeterministic_reload", id, generation = report.generation, "extension reloaded");
    Ok(json!({ "generation": report.generation, "removed": report.removed, "added": report.added }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset_pipeline::extension_import::{ExtensionImportError, Importer};

    fn project(tag: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let ext = dir.join("extensions");
        std::fs::create_dir_all(ext.join("art/content")).expect("dirs");
        std::fs::create_dir_all(ext.join("coded")).expect("dirs");
        std::fs::write(ext.join("art/content/a.txt"), "a").expect("write");
        std::fs::write(
            ext.join("art/regia-plugin.toml"),
            "format_version = 1\nid = \"art\"\nversion = \"1.0.0\"\n\n[[content]]\nroot = \"content\"\n",
        )
        .expect("write");
        std::fs::write(
            ext.join("coded/regia-plugin.toml"),
            "format_version = 1\nid = \"coded\"\nversion = \"1.0.0\"\n\n[[code]]\ncrate = \"coded\"\naudience = [\"editor\"]\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("Regia.toml"),
            "[extensions]\nenabled = [\"art\", \"coded\"]\n",
        )
        .expect("write");
        dir
    }

    #[test]
    fn a_project_opened_later_mounts_content_and_waits_for_a_restart() {
        let root = project("ext-opened-later");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(EditorExtensionPlugin {
            project: None,
            target_dir: Some(root.join("no-target")),
            engine_dir: None,
            linked: Vec::new(),
        });
        app.update();
        let list = app.world().resource::<ExtensionRegistry>().list_json(None);
        assert_eq!(list["extensions"].as_array().expect("list").len(), 0);

        project_opened(app.world_mut(), &root);
        let registry = app.world().resource::<ExtensionRegistry>();
        assert_eq!(registry.inputs().audience, "editor");
        assert!(registry.mounts().resolve("ext://art/a.txt", None).is_some());
        assert!(registry.restart_required().contains("coded"));
        assert!(!registry.restart_required().contains("art"));

        // Opening the same project again changes nothing.
        let hash = registry.resolved().resolved_hash.clone();
        app.world_mut()
            .resource_mut::<ExtensionRegistry>()
            .note_toggled("art", false);
        project_opened(app.world_mut(), &root);
        let registry = app.world().resource::<ExtensionRegistry>();
        assert_eq!(registry.resolved().resolved_hash, hash);
        assert_eq!(registry.mounts().list().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    struct Dialogue;

    impl Importer for Dialogue {
        fn file_extensions(&self) -> &[&str] {
            &["dlg", "png"]
        }

        fn import(&self, input: &Path, out: &mut ImportOutput) -> Result<(), ExtensionImportError> {
            let name = input.file_stem().and_then(|s| s.to_str()).unwrap_or("x");
            if name == "bad" {
                return Err(ExtensionImportError("no".into()));
            }
            let target = if name == "escape" {
                "../out.bin"
            } else {
                "talk.bin"
            };
            out.files.push((target.to_owned(), vec![1, 2]));
            Ok(())
        }
    }

    #[test]
    fn importers_take_what_the_engine_does_not() {
        let project = regia_test_utils::unique_temp_dir("ext-importers");
        let mut world = World::new();
        let mut editor = EditorExtensions::default();
        editor.importers.insert("dlg", "dialogue", Arc::new(Dialogue));
        editor.importers.insert("png", "dialogue", Arc::new(Dialogue));
        world.insert_resource(editor);
        let sources = [
            PathBuf::from("a/talk.dlg"),
            PathBuf::from("a/bad.dlg"),
            PathBuf::from("a/escape.dlg"),
            PathBuf::from("a/photo.png"),
            PathBuf::from("a/other.xyz"),
        ];
        let (rest, report) = run_importers(&world, &project, "dialogue", &sources);
        assert_eq!(rest, [PathBuf::from("a/photo.png"), PathBuf::from("a/other.xyz")]);
        assert_eq!(report.imported, ["assets/dialogue/talk.bin"]);
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert_eq!(
            std::fs::read(project.join("assets/dialogue/talk.bin")).expect("read"),
            [1, 2]
        );
        assert!(
            !project.join("assets/out.bin").exists(),
            "the output stays in the folder"
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn the_import_dialog_offers_what_importers_take() {
        let mut world = World::new();
        assert!(!import_file_types(&world).contains(&"dlg".to_owned()));
        let mut editor = EditorExtensions::default();
        editor.importers.insert("dlg", "dialogue", Arc::new(Dialogue));
        editor.importers.insert("png", "dialogue", Arc::new(Dialogue));
        world.insert_resource(editor);
        let types = import_file_types(&world);
        assert!(types.contains(&"dlg".to_owned()) && types.contains(&"glb".to_owned()));
        assert_eq!(
            types.iter().filter(|t| *t == "png").count(),
            1,
            "a type both take is offered once"
        );
    }

    #[test]
    fn a_failed_reload_gets_its_editor_registrations_back() {
        let mut editor = EditorExtensions::default();
        let panel = |id: &str, title: &str| PanelDesc {
            id: id.to_owned(),
            title_key: title.to_owned(),
            dock_region: super::super::context::DockRegion::Bottom,
            draw: Arc::new(|_, _| {}),
        };
        editor.panels.insert(
            "dialogue.main".into(),
            ("dialogue".into(), panel("dialogue.main", "old")),
        );
        editor.panels.insert(
            "other.main".into(),
            ("other".into(), panel("other.main", "other")),
        );
        editor.importers.insert("dlg", "dialogue", Arc::new(Dialogue));

        let snapshot = editor.take_owner("dialogue");
        assert!(editor.panel("dialogue.main").is_none());
        assert!(editor.importers.for_file(Path::new("a.dlg")).is_none());
        assert!(editor.panel("other.main").is_some(), "others keep theirs");

        // The new copy got part of the way before it failed.
        editor.panels.insert(
            "dialogue.main".into(),
            ("dialogue".into(), panel("dialogue.main", "new")),
        );
        editor.panels.insert(
            "dialogue.extra".into(),
            ("dialogue".into(), panel("dialogue.extra", "new")),
        );
        editor.restore("dialogue", snapshot);
        assert_eq!(editor.panel("dialogue.main").expect("back").title_key, "old");
        assert!(editor.panel("dialogue.extra").is_none());
        assert!(editor.importers.for_file(Path::new("a.dlg")).is_some());
    }

    #[test]
    fn a_built_library_is_found_at_its_default_path() {
        let dir = regia_test_utils::unique_temp_dir("ext-library-path");
        let entry = CodeEntry {
            crate_name: Some("dialogue".into()),
            audience: vec!["editor".into()],
            phase: regia_framework::extension::manifest::Phase::Default,
            library: None,
        };
        assert_eq!(library_path(&dir, &entry), None, "nothing built");
        let prefix = if cfg!(windows) { "" } else { "lib" };
        let built = dir
            .join("bin")
            .join(BUILD_TARGET)
            .join(format!("{prefix}dialogue.{}", dylib_extension()));
        std::fs::create_dir_all(built.parent().expect("parent")).expect("dir");
        std::fs::write(&built, b"x").expect("write");
        assert_eq!(library_path(&dir, &entry), Some(built));
        let named = CodeEntry {
            library: Some("bin/custom.so".into()),
            ..entry
        };
        assert_eq!(library_path(&dir, &named), Some(dir.join("bin/custom.so")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
