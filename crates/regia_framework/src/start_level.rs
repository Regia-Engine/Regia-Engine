// SPDX-License-Identifier: MPL-2.0
//! `regia run --level L` (`docs/specs/21-tooling-and-packaging.md` § 1): the game starts in
//! level `L` instead of the one its own code opens.
//!
//! The CLI resolves `L` and passes the absolute path in [`START_LEVEL_ENV`]. [`StartLevelPlugin`]
//! (part of `regia::DefaultPlugins`) reads it once; on the first `Update`, after the game's own
//! `Startup` systems, the level (a `.level.ron` file or a `.level.d` directory, with the layer
//! definitions it binds and the levels it places, [`crate::level_assets`]) is read and opened with
//! [`LoadMode::Replace`], so it replaces any level the game opened. In a `--dev` run a save of it
//! ([`regia_asset::SourceAssetChanged`]) re-opens it (live reload; entities marked
//! [`regia_scene::level::Persistent`], such as a game's camera rig, stay). A file that does not
//! read or parse is logged and the current level stays.
//!
//! Without the variable the project's `Regia.toml` `[project] startup_level` is the start level
//! (ADR-7203), opened the same way unless the game's own `Startup` already opened a level.
//! Without either the plugin adds nothing. The world needs `regia_scene`'s
//! `ScenePlugin` (games add it, usually through their gameplay plugins).

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_scene::level::{LevelId, LoadMode, open_level};
use regia_scene::level_dir::containing_level_dir;
use regia_scene::streaming::{StreamedLevelOpener, StreamedOpen};

/// Environment variable carrying the absolute path of the start level (`regia run --level`).
pub const START_LEVEL_ENV: &str = "REGIA_START_LEVEL";

/// The start level requested for this run.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct StartLevel {
    /// The `.level.ron` file or `.level.d` directory.
    pub path: PathBuf,
    /// The id it was opened under (after the first update).
    pub opened: Option<LevelId>,
    /// Re-open on the next update (set at start and when the file changes).
    pub dirty: bool,
    /// Open only when no level is loaded or loading yet: the manifest's `startup_level` does not
    /// replace a level the game's own `Startup` opened (`regia run --level` does).
    pub if_no_level: bool,
}

impl StartLevel {
    /// A request for `path`, opened at the next chance.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            opened: None,
            dirty: true,
            if_no_level: false,
        }
    }
}

/// Opens the [`START_LEVEL_ENV`] level after the game's startup and re-opens it when it changes.
#[derive(Debug, Default)]
pub struct StartLevelPlugin;

impl Plugin for StartLevelPlugin {
    fn build(&self, app: &mut App) {
        let flag = regia_core::brand::env_var_os(START_LEVEL_ENV);
        let root = regia_asset::detect_project_root();
        // A dev launch on a device (ADR-8472) names its start level in the intent extra.
        let dev = crate::dev_launch::from_launch_intent();
        install_dev_launch(app, dev, flag, &root);
    }
}

/// [`StartLevelPlugin`] with the dev launch options read: inserts `dev` as a resource (its
/// arguments and agent transport), and its start level (a path under `assets/`, read from the VFS
/// when it is not a file, [`crate::dev_launch::materialize_level`]) stands in for an empty `flag`.
pub fn install_dev_launch(
    app: &mut App,
    dev: Option<crate::dev_launch::DevLaunch>,
    flag: Option<std::ffi::OsString>,
    project_root: &Path,
) {
    let level = dev.as_ref().and_then(|d| d.start_level.clone());
    if let Some(dev) = dev {
        app.insert_resource(dev);
    }
    let flag = flag
        .filter(|p| !p.is_empty())
        .or_else(|| level.map(std::ffi::OsString::from));
    install_start_level(app, flag, project_root);
}

/// Picks this run's start level and installs it (what [`StartLevelPlugin`] does with
/// [`START_LEVEL_ENV`] and the detected project root; tests call it directly).
///
/// A non-empty `flag` (`regia run --level`) wins. Otherwise the project's `Regia.toml`
/// `[project] startup_level` (a path under `assets/`) is the start level, opened only when the
/// game's own `Startup` opened no level ([`StartLevel::if_no_level`], ADR-7203). Without either,
/// nothing is installed.
pub fn install_start_level(app: &mut App, flag: Option<std::ffi::OsString>, project_root: &Path) {
    if let Some(path) = flag.filter(|p| !p.is_empty()) {
        tracing::info!(target: "regia_framework", path = %Path::new(&path).display(), "start level requested");
        add_start_level(app, StartLevel::new(PathBuf::from(path)));
        return;
    }
    let Some(path) = manifest_startup_level(project_root) else {
        return;
    };
    tracing::info!(target: "regia_framework", path = %path.display(), "start level from Regia.toml");
    let mut start = StartLevel::new(path);
    start.if_no_level = true;
    add_start_level(app, start);
}

/// `assets/<startup_level>` from `<project_root>/Regia.toml`, when the manifest names one that
/// stays under `assets/` (no absolute path, no `..`). A missing or unreadable manifest is `None`.
#[must_use]
pub fn manifest_startup_level(project_root: &Path) -> Option<PathBuf> {
    let manifest = regia_core::brand::project_manifest(project_root)?;
    let text = std::fs::read_to_string(&manifest).ok()?;
    let value: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "{}: {err}; no startup level", manifest.display());
            return None;
        }
    };
    let rel = value.get("project")?.get("startup_level")?.as_str()?;
    if rel.is_empty() {
        return None;
    }
    let rel_path = Path::new(rel);
    let escapes = rel_path.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)
        )
    });
    if escapes {
        tracing::warn!(target: "regia_framework", "ignoring startup_level `{rel}`: it leaves assets/");
        return None;
    }
    Some(project_root.join("assets").join(rel_path))
}

/// Installs `start` and the systems that open and live-reload it (what [`StartLevelPlugin`]
/// does for the environment variable; tests and tools call it directly).
pub fn add_start_level(app: &mut App, start: StartLevel) {
    app.insert_resource(start)
        .add_message::<regia_asset::SourceAssetChanged>()
        .add_systems(Update, (watch_start_level, open_start_level).chain());
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// `Update`: marks the start level dirty when its file changed on disk (in either spelling of the
/// level: a change inside `town.level.d` reloads a start level named `town.level.ron`).
pub fn watch_start_level(
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
    mut start: ResMut<StartLevel>,
) {
    for change in changes.read() {
        // A change in a level directory arrives as the entity file that changed.
        let level = containing_level_dir(&change.path).unwrap_or_else(|| change.path.clone());
        // Either spelling names the level (`town.level.ron` or `town.level.d`, one key), as for
        // the level-source reload that leaves the start level to this watcher.
        if change.kind == regia_asset::SourceKind::Level
            && (same_file(&level, &start.path)
                || crate::level_assets::level_key(&level) == crate::level_assets::level_key(&start.path))
        {
            start.dirty = true;
        }
    }
}

/// Parses the start level's file and opens it (`Replace`) when [`StartLevel::dirty`].
pub fn open_start_level(world: &mut World) {
    let Some(start) = world.get_resource::<StartLevel>().cloned() else {
        return;
    };
    if !start.dirty {
        return;
    }
    if let Some(mut res) = world.get_resource_mut::<StartLevel>() {
        res.dirty = false;
    }
    // A dev launch's level is named under `assets/` (ADR-8472): on a device it is in the APK, so
    // it is copied out of the VFS once and opened (and watched) from there.
    let mut start = start;
    if start.path.is_relative()
        && !start.path.exists()
        && let Some(host) = crate::dev_launch::materialize_level(world, &start.path)
    {
        start.path = host.clone();
        if let Some(mut res) = world.get_resource_mut::<StartLevel>() {
            res.path = host;
        }
    }
    // The manifest's level yields to a level the game opened itself; once it is open, its own
    // live reloads go through.
    if start.if_no_level
        && start.opened.is_none()
        && world
            .get_resource::<regia_scene::level::LevelManager>()
            .is_some_and(|manager| manager.levels().next().is_some())
    {
        tracing::info!(target: "regia_framework", path = %start.path.display(), "the game opened its own level; startup_level not opened");
        return;
    }
    let _span = tracing::info_span!("start_level_open").entered();
    let started = std::time::Instant::now();
    // A level cooked for streaming opens through the runtime, never as one whole file (spec 78 § 6.5).
    if let Some(opener) = world.get_resource::<StreamedLevelOpener>().copied() {
        let id = crate::level_assets::level_id_of(&start.path);
        match (opener.0)(world, id, LoadMode::Replace) {
            StreamedOpen::NotCooked => {}
            opened @ (StreamedOpen::Opened | StreamedOpen::AlwaysLoaded) => {
                let always_only = matches!(opened, StreamedOpen::AlwaysLoaded);
                if let Some(mut res) = world.get_resource_mut::<StartLevel>() {
                    res.opened = Some(id);
                }
                tracing::info!(
                    target: "regia_framework",
                    path = %start.path.display(),
                    ms = started.elapsed().as_millis() as u64,
                    always_only,
                    "start level opened (streamed)"
                );
                return;
            }
            StreamedOpen::Failed(err) => {
                tracing::error!(target: "regia_framework", path = %start.path.display(), "start level not opened: {err}");
                return;
            }
        }
    }
    let id: LevelId = match crate::level_assets::load_level_source(world, &start.path) {
        Ok(report) => report
            .level
            .unwrap_or_else(|| crate::level_assets::level_id_of(&start.path)),
        Err(err) => {
            tracing::error!(target: "regia_framework", path = %start.path.display(), "start level not opened: {err}");
            return;
        }
    };
    if let Err(err) = open_level(world, id, LoadMode::Replace) {
        tracing::error!(target: "regia_framework", path = %start.path.display(), "start level not opened: {err}");
        return;
    }
    if let Some(mut res) = world.get_resource_mut::<StartLevel>() {
        res.opened = Some(id);
    }
    tracing::info!(
        target: "regia_framework",
        path = %start.path.display(),
        ms = started.elapsed().as_millis() as u64,
        "start level opened"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::ScenePlugin;
    use regia_scene::components::Name;
    use regia_scene::level::LevelManager;

    const LEVEL_A: &str = r#"(
    format_version: 1,
    entities: [
        (
            guid: "8b6f2a11-0000-4000-8000-00000000000a",
            name: "from_a",
            components: {},
        ),
    ],
)"#;

    fn names(app: &mut App) -> Vec<String> {
        let mut q = app.world_mut().query::<&Name>();
        let mut out: Vec<String> = q.iter(app.world()).map(|n| n.as_str().to_string()).collect();
        out.sort();
        out
    }

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia_start_level_{test}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn app_with(path: &Path) -> App {
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        add_start_level(&mut app, StartLevel::new(path));
        app
    }

    #[test]
    fn opens_the_start_level_after_startup_and_reopens_it_when_it_changes() {
        let dir = scratch("reopen");
        let path = dir.join("a.level.ron");
        std::fs::write(&path, LEVEL_A).expect("write");
        let mut app = app_with(&path);
        app.update();
        let first = app.world().resource::<StartLevel>().opened.expect("opened");
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(first));
        assert_eq!(names(&mut app), vec!["from_a".to_string()]);
        let generation = app.world().resource::<LevelManager>().load_generation(first);

        std::fs::write(&path, LEVEL_A.replace("from_a", "edited")).expect("edit");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(path.clone()));
        app.update();
        let second = app.world().resource::<StartLevel>().opened.expect("reopened");
        // One id across reloads (ADR-1953), so a reopen counts as a load of the same level.
        assert_eq!(first, second);
        assert!(app.world().resource::<LevelManager>().load_generation(second) > generation);
        assert_eq!(
            names(&mut app),
            vec!["edited".to_string()],
            "the old level was replaced"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 78 § 6.5: a level with a cooked world index opens through the runtime and the source
    /// file is not read; a level without one opens the ordinary way.
    #[test]
    fn a_level_cooked_for_streaming_opens_through_the_runtime() {
        use regia_scene::streaming::{StreamedLevelOpener, StreamedOpen};
        let dir = scratch("streamed");
        // No such file: an ordinary open would log an error and leave `opened` empty.
        let path = dir.join("cooked.level.ron");
        let mut app = app_with(&path);
        app.insert_resource(StreamedLevelOpener(|_, _, _| StreamedOpen::Opened));
        app.update();
        let expected = crate::level_assets::level_id_of(&path);
        assert_eq!(app.world().resource::<StartLevel>().opened, Some(expected));

        // The ordinary way when the runtime has no cooked index.
        let path = dir.join("a.level.ron");
        std::fs::write(&path, LEVEL_A).expect("write");
        let mut app = app_with(&path);
        app.insert_resource(StreamedLevelOpener(|_, _, _| StreamedOpen::NotCooked));
        app.update();
        assert!(app.world().resource::<StartLevel>().opened.is_some());
        assert_eq!(names(&mut app), vec!["from_a".to_string()]);

        // A refused index is an error, not a whole-file open.
        let mut app = app_with(&path);
        app.insert_resource(StreamedLevelOpener(|_, _, _| {
            StreamedOpen::Failed("corrupt".into())
        }));
        app.update();
        assert_eq!(app.world().resource::<StartLevel>().opened, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every file under `dir` (recursively) whose text contains `needle`.
    fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).expect("read dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(needle)) {
                    out.push(path);
                }
            }
        }
        out
    }

    /// `regia run --level town.level.ron` for a level stored as `town.level.d`: an edit of an
    /// entity file inside the directory re-opens the level (audit framework5 #9).
    #[test]
    fn an_edit_in_the_directory_form_reopens_a_start_level_named_by_its_file_form() {
        use regia_scene::format::SCENE_FORMAT_VERSION;
        let dir = regia_test_utils::unique_temp_dir("start_level_spelling");
        let file = dir.join("town.level.ron");
        let text = format!(
            r#"SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [(guid: "8b6f2a11-0000-4000-8000-00000000000a", name: "from_a", components: {{}})], resources: {{}})"#
        );
        std::fs::write(&file, text).expect("write");
        let level_dir = dir.join("town.level.d");
        regia_scene::level_dir::split_level_file(&file, &level_dir).expect("split");
        std::fs::remove_file(&file).expect("only the directory form stays");
        let mut app = app_with(&file);
        app.update();
        let first = app.world().resource::<StartLevel>().opened.expect("opened");
        assert_eq!(names(&mut app), vec!["from_a".to_string()]);
        let generation = app.world().resource::<LevelManager>().load_generation(first);

        let edited = files_containing(&level_dir, "from_a");
        assert_eq!(edited.len(), 1, "one entity file names it: {edited:?}");
        let body = std::fs::read_to_string(&edited[0]).expect("read");
        std::fs::write(&edited[0], body.replace("from_a", "edited")).expect("edit");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(edited[0].clone()));
        app.update();
        assert!(
            app.world().resource::<LevelManager>().load_generation(first) > generation,
            "the start level was opened again"
        );
        assert_eq!(names(&mut app), vec!["edited".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 14 § A4 (ADR-7203): without `regia run --level`, the runtime opens the project's
    /// `[project] startup_level`; a game that already opened its own level keeps it.
    #[test]
    fn a_start_without_level_opens_the_manifest_startup_level() {
        let root = regia_test_utils::unique_temp_dir("start_level_manifest");
        std::fs::create_dir_all(root.join("assets/levels")).expect("dir");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"g\"\nstartup_level = \"levels/a.level.ron\"\n",
        )
        .expect("manifest");
        std::fs::write(root.join("assets/levels/a.level.ron"), LEVEL_A).expect("level");

        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        install_start_level(&mut app, None, &root);
        app.update();
        assert_eq!(names(&mut app), vec!["from_a".to_string()]);
        assert!(app.world().resource::<StartLevel>().opened.is_some());

        // `--level` wins over the manifest.
        let other = root.join("other.level.ron");
        std::fs::write(&other, LEVEL_A.replace("from_a", "from_flag")).expect("other");
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        install_start_level(&mut app, Some(other.into_os_string()), &root);
        app.update();
        assert_eq!(names(&mut app), vec!["from_flag".to_string()]);

        // A game whose own Startup opened a level keeps it.
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        app.add_systems(Startup, |world: &mut World| {
            let id = regia_scene::level::LevelId(regia_core::id::Guid::from_u128(0x6a3e));
            let mut level = regia_scene::level::Level::from_scene_file(id, Default::default());
            level.scene =
                regia_scene::format::SceneFile::from_ron_str(&LEVEL_A.replace("from_a", "game_own"))
                    .expect("scene");
            world
                .resource_mut::<regia_scene::level::LevelLibrary>()
                .insert(level);
            open_level(world, id, LoadMode::Replace).expect("open");
        });
        install_start_level(&mut app, None, &root);
        app.update();
        assert_eq!(names(&mut app), vec!["game_own".to_string()]);

        // No manifest level, no start level.
        let bare = regia_test_utils::unique_temp_dir("start_level_bare");
        std::fs::write(bare.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        install_start_level(&mut app, None, &bare);
        assert!(app.world().get_resource::<StartLevel>().is_none());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    /// ADR-8472: a dev launch on a device names its start level under `assets/`; the level comes
    /// out of the VFS (the APK's assets there) and opens, and the launch's options stay readable.
    #[test]
    fn a_dev_launch_start_level_opens_from_the_vfs_assets() {
        use regia_platform::{DirMount, MountPoint, Vfs};
        let root = regia_test_utils::unique_temp_dir("start_level_dev_launch");
        let assets = root.join("apk_assets");
        std::fs::create_dir_all(assets.join("levels")).expect("dir");
        std::fs::write(
            assets.join("levels/b.level.ron"),
            LEVEL_A.replace("from_a", "from_extra"),
        )
        .expect("level");
        std::fs::create_dir_all(root.join("cache")).expect("cache");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Assets, DirMount::read_only(assets));
        vfs.mount(MountPoint::Cache, DirMount::writable(root.join("cache")));
        let text = "v1.l6c6576656c732f622e6c6576656c2e726f6e.a2d2d73656564.a37";
        let dev = crate::dev_launch::decode_logged(text).expect("decodes");
        assert_eq!(dev.start_level.as_deref(), Some("levels/b.level.ron"));

        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        app.insert_resource(vfs);
        // No project manifest here: without the extra nothing would open.
        install_dev_launch(&mut app, Some(dev), None, &root.join("no_project"));
        app.update();
        assert_eq!(names(&mut app), vec!["from_extra".to_string()]);
        let start = app.world().resource::<StartLevel>();
        assert!(start.opened.is_some());
        assert_eq!(start.path, root.join("cache/dev_launch/levels/b.level.ron"));
        assert_eq!(
            crate::dev_launch::launch_arguments(app.world()),
            vec!["--seed".to_string(), "7".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bad_file_keeps_the_current_level() {
        let dir = scratch("bad");
        let path = dir.join("a.level.ron");
        std::fs::write(&path, LEVEL_A).expect("write");
        let mut app = app_with(&path);
        app.update();
        let first = app.world().resource::<StartLevel>().opened;
        std::fs::write(&path, "(not a level").expect("break");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(path.clone()));
        app.update();
        assert_eq!(app.world().resource::<StartLevel>().opened, first);
        assert_eq!(names(&mut app), vec!["from_a".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
