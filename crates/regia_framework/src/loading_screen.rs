// SPDX-License-Identifier: MPL-2.0
//! The project's loading-screen prefab (spec 14 § A4, ADR-7263).
//!
//! `Regia.toml` names it under `[project]`:
//!
//! ```toml
//! [project]
//! loading_screen = "ui/loading.prefab.ron"   # a path under assets/, or "asset:<guid>"
//! ```
//!
//! A path is a `.prefab.ron` under `assets/` (no absolute path, no `..`): when the app's plugins
//! finish (before the first frame's `Startup`, so before a game opens its first level)
//! [`LoadingScreenPlugin`] bakes it into the [`PrefabLibrary`] under its `.meta` id (a packaged
//! build without sidecars uses an id derived from the path) and sets
//! [`LevelManager::loading_screen`]; an app driven by `update` without `run` gets the same step
//! as a `Startup` system. `asset:<guid>` (or a bare guid) names a prefab or variant the
//! game registers itself. A game that set `loading_screen` itself keeps its own.
//!
//! While a `Replace` open waits for its data, the [`LevelManager`] keeps one instance of the
//! prefab alive, its root marked [`LoadingScreen`], and despawns it when the load completes,
//! fails or is unloaded. Each `PreUpdate`, after the level lifecycle, [`update_loading_screen`]
//! writes the level's load progress ([`crate::level_progress::level_load_progress`]) into
//! [`LoadingScreen::progress`]; the shown value never goes down for one level (a load set that
//! grows does not move a progress bar back). The prefab's own systems or scripts read it, and
//! the agent method `level.status` names the entity.

use std::path::{Component, Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::level::{LevelManager, LoadingScreen};
use regia_scene::prefab::{PrefabLibrary, PrefabTemplate};

/// The `[project]` key in `Regia.toml` naming the loading-screen prefab.
pub const LOADING_SCREEN_KEY: &str = "loading_screen";

/// Where the loading-screen prefab comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadingScreenSource {
    /// A `.prefab.ron` under the project's `assets/`, registered at startup.
    File(PathBuf),
    /// A prefab (or variant) the game registers in the [`PrefabLibrary`] itself.
    Asset(Guid),
}

/// The loading screen this run's `Regia.toml` names (inserted by [`install_loading_screen`]).
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct LoadingScreenSetting {
    /// The prefab.
    pub source: LoadingScreenSource,
}

/// Reads `Regia.toml`'s `[project] loading_screen` and keeps the loading screen's progress up to
/// date (part of `regia::DefaultPlugins`). Without the key it only adds the progress system,
/// which is idle while no loading screen is up.
#[derive(Debug, Default)]
pub struct LoadingScreenPlugin;

impl Plugin for LoadingScreenPlugin {
    fn build(&self, app: &mut App) {
        let root = regia_asset::detect_project_root();
        install_loading_screen(app, &root);
    }

    fn finish(&self, app: &mut App) {
        // Every plugin is built (the scene types are registered): register the prefab before
        // any `Startup` system can open a level.
        apply_loading_screen_setting(app.world_mut());
    }
}

/// What [`LoadingScreenPlugin`] does with the project at `project_root` (tests and tools call it
/// directly): adds [`update_loading_screen`] and, when the manifest names a loading screen, the
/// [`LoadingScreenSetting`] and a `Startup` [`apply_loading_screen_setting`] (a no-op once the
/// plugin's `finish` applied it).
pub fn install_loading_screen(app: &mut App, project_root: &Path) {
    app.add_systems(
        PreUpdate,
        update_loading_screen.after(regia_scene::level::level_lifecycle),
    );
    let Some(source) = manifest_loading_screen(project_root) else {
        return;
    };
    tracing::info!(target: "regia_framework", ?source, "loading screen from Regia.toml");
    app.insert_resource(LoadingScreenSetting { source })
        .add_systems(Startup, apply_loading_screen_setting);
}

/// The loading screen `<project_root>/Regia.toml` names, if any: `asset:<guid>` or a bare guid
/// is [`LoadingScreenSource::Asset`]; anything else is a path under `assets/`
/// ([`LoadingScreenSource::File`]), ignored (with a warning) when it is absolute or uses `..`.
#[must_use]
pub fn manifest_loading_screen(project_root: &Path) -> Option<LoadingScreenSource> {
    let manifest = regia_core::brand::project_manifest(project_root)?;
    let text = std::fs::read_to_string(&manifest).ok()?;
    let value: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "{}: {err}; no loading screen", manifest.display());
            return None;
        }
    };
    let text = value.get("project")?.get(LOADING_SCREEN_KEY)?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(id) = text.strip_prefix("asset:").unwrap_or(text).parse::<Guid>() {
        return Some(LoadingScreenSource::Asset(id));
    }
    let rel = Path::new(text);
    let escapes = rel.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if escapes {
        tracing::warn!(target: "regia_framework", "ignoring loading_screen `{text}`: it leaves assets/");
        return None;
    }
    Some(LoadingScreenSource::File(project_root.join("assets").join(rel)))
}

/// The id of the prefab file at `path`: its `.meta` sidecar's, else one derived from the path
/// below `assets/` (a packaged build ships no sidecars; the id only has to agree with itself).
fn prefab_id_of(path: &Path) -> Guid {
    if let Ok(bytes) = std::fs::read(regia_asset::meta_path_for(path))
        && let Ok(meta) = regia_asset::MetaSidecar::from_ron(&bytes)
    {
        return meta.id.0;
    }
    let text = path.to_string_lossy().replace('\\', "/");
    let key = text.rfind("assets/").map_or(text.as_str(), |at| &text[at..]);
    let hash = blake3::hash(key.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Guid::from_u128(u128::from_le_bytes(bytes).max(1))
}

/// Reads, bakes and registers the loading-screen prefab file; `None` (logged) when it fails.
fn register_prefab_file(world: &mut World, path: &Path) -> Option<Guid> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "loading screen {}: {err}", path.display());
            return None;
        }
    };
    let file = match regia_scene::format::SceneFile::from_ron_str(&text) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "loading screen {}: {err}", path.display());
            return None;
        }
    };
    let id = prefab_id_of(path);
    let registry = world
        .get_resource::<regia_ecs::reflect::AppTypeRegistry>()?
        .0
        .clone();
    let template = match PrefabTemplate::from_scene_file(id, &file, &registry.read()) {
        Ok(template) => template,
        Err(err) => {
            tracing::warn!(target: "regia_framework", "loading screen {}: bake: {err}", path.display());
            return None;
        }
    };
    for warning in &template.warnings {
        tracing::warn!(target: "regia_framework", "loading screen {}: {warning}", path.display());
    }
    world
        .get_resource_or_insert_with::<PrefabLibrary>(PrefabLibrary::default)
        .insert(template);
    Some(id)
}

/// Registers the [`LoadingScreenSetting`]'s prefab and sets [`LevelManager::loading_screen`],
/// unless it is already set (by an earlier call or by the game). Idle without the setting.
pub fn apply_loading_screen_setting(world: &mut World) {
    let Some(setting) = world.get_resource::<LoadingScreenSetting>().cloned() else {
        return;
    };
    if !world.contains_resource::<LevelManager>() {
        tracing::warn!(target: "regia_framework", "a loading screen needs regia_scene's ScenePlugin (no LevelManager)");
        return;
    }
    if world.resource::<LevelManager>().loading_screen.is_some() {
        return;
    }
    let id = match &setting.source {
        LoadingScreenSource::Asset(id) => Some(*id),
        LoadingScreenSource::File(path) => register_prefab_file(world, path),
    };
    if let Some(id) = id {
        world.resource_mut::<LevelManager>().loading_screen = Some(id);
    }
}

/// `PreUpdate`, after the level lifecycle: writes the shown level's load progress into the live
/// [`LoadingScreen`]. The value only goes up while the level stays the same; idle without a
/// loading screen.
pub fn update_loading_screen(world: &mut World) {
    let Some(entity) = world
        .get_resource::<LevelManager>()
        .and_then(LevelManager::loading_screen_entity)
    else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_framework", "loading_screen_progress").entered();
    let Some(screen) = world.get::<LoadingScreen>(entity).copied() else {
        return;
    };
    let Some(progress) = crate::level_progress::level_load_progress(world, screen.level) else {
        return;
    };
    if progress > screen.progress
        && let Some(mut live) = world.get_mut::<LoadingScreen>(entity)
    {
        live.progress = progress;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level_progress::level_load_set_name;
    use regia_asset::{LoadKind, LoadSet, LoadSets, MetaSidecar};
    use regia_core::id::Guid;
    use regia_scene::ScenePlugin;
    use regia_scene::level::{
        Level, LevelId, LevelLibrary, LevelManager, LoadMode, LoadingScreen, open_level,
    };
    use regia_scene::prefab::PrefabInstance;
    use std::path::{Path, PathBuf};

    const SCREEN: Guid = Guid::from_u128(0x8b6f_2a11_0000_4000_8000_0000_0000_105c);

    const PREFAB: &str = r#"(format_version: 1, entities: [
        (guid: "8b6f2a11-0000-4000-8000-0000000010a1", name: "LoadingRoot", components: {
            "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
        }),
    ])"#;

    const LEVEL: &str = r#"(format_version: 1, entities: [
        (guid: "8b6f2a11-0000-4000-8000-0000000010b1", name: "in_level", components: {}),
    ])"#;

    /// A project whose `Regia.toml` names `ui/loading.prefab.ron` as its loading screen.
    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets/ui")).expect("dir");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"g\"\nloading_screen = \"ui/loading.prefab.ron\"\n",
        )
        .expect("manifest");
        let prefab = root.join("assets/ui/loading.prefab.ron");
        std::fs::write(&prefab, PREFAB).expect("prefab");
        write_meta(&prefab, SCREEN);
        root
    }

    fn write_meta(path: &Path, id: Guid) {
        let sidecar = MetaSidecar::new("prefab_ron", regia_asset::AssetId(id));
        std::fs::write(
            regia_asset::meta_path_for(path),
            sidecar.to_ron_string().expect("meta"),
        )
        .expect("write meta");
    }

    fn app(root: &Path) -> (App, LoadSets) {
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        let sets = LoadSets::default();
        app.insert_resource(sets.clone());
        install_loading_screen(&mut app, root);
        app.update();
        (app, sets)
    }

    /// Entities carrying [`LoadingScreen`], with their progress.
    fn screens(app: &mut App) -> Vec<(Entity, LoadingScreen)> {
        let mut query = app.world_mut().query::<(Entity, &LoadingScreen)>();
        let mut out: Vec<(Entity, LoadingScreen)> = query.iter(app.world()).map(|(e, s)| (e, *s)).collect();
        out.sort_by_key(|(entity, _)| *entity);
        out
    }

    #[test]
    fn the_manifest_names_the_loading_screen_prefab_and_its_template_is_registered() {
        let root = project("loading_screen_manifest");
        assert_eq!(
            manifest_loading_screen(&root),
            Some(LoadingScreenSource::File(
                root.join("assets/ui/loading.prefab.ron")
            ))
        );
        let (app, _) = app(&root);
        assert_eq!(
            app.world().resource::<LevelManager>().loading_screen,
            Some(SCREEN)
        );
        assert!(
            app.world()
                .resource::<regia_scene::prefab::PrefabLibrary>()
                .get(SCREEN)
                .is_some(),
            "the prefab is baked into the library at startup"
        );
        // An asset id instead of a path, and no key at all.
        let id_root = regia_test_utils::unique_temp_dir("loading_screen_manifest_id");
        std::fs::write(
            id_root.join("Regia.toml"),
            format!("[project]\nloading_screen = \"asset:{SCREEN}\"\n"),
        )
        .expect("manifest");
        assert_eq!(
            manifest_loading_screen(&id_root),
            Some(LoadingScreenSource::Asset(SCREEN))
        );
        std::fs::write(id_root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
        assert_eq!(manifest_loading_screen(&id_root), None);
        // A path that leaves assets/ is ignored.
        std::fs::write(
            id_root.join("Regia.toml"),
            "[project]\nloading_screen = \"../x.prefab.ron\"\n",
        )
        .expect("manifest");
        assert_eq!(manifest_loading_screen(&id_root), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&id_root);
    }

    /// Acceptance (spec 14 § A4): an `open_level` with a loading-screen prefab has the prefab
    /// entity on every load frame, its progress never goes down, and none is left after.
    #[test]
    fn a_loading_screen_shows_on_every_load_frame_with_monotonic_progress_and_goes_after() {
        let root = project("loading_screen_frames");
        let (mut app, sets) = app(&root);
        let id = LevelId(Guid::from_u128(0x1e7e_105c));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");

        // The frame of the call already has it.
        let first = screens(&mut app);
        assert_eq!(first.len(), 1, "one loading screen: {first:?}");
        let screen = first[0].0;
        assert_eq!(first[0].1.level, id);
        assert_eq!(
            app.world()
                .get::<PrefabInstance>(screen)
                .map(PrefabInstance::prefab_guid),
            Some(SCREEN),
            "the loading screen is an instance of the project's prefab"
        );
        assert_eq!(
            app.world().resource::<LevelManager>().loading_screen_entity(),
            Some(screen)
        );

        let mut set = LoadSet::new(level_load_set_name(id));
        for key in ["a", "b", "c", "d"] {
            set.push(LoadKind::Texture, key, 0, 0);
        }
        sets.define(set);
        let mut seen = vec![0.0f32];
        for key in ["a", "b", "c", "d"] {
            sets.mark_resident(LoadKind::Texture, key);
            app.update();
            let now = screens(&mut app);
            assert_eq!(now.len(), 1, "the screen stays while the level loads");
            assert_eq!(now[0].0, screen, "the same entity every frame");
            seen.push(now[0].1.progress);
        }
        // A redefined, larger set must not move the shown progress back.
        let mut bigger = LoadSet::new(level_load_set_name(id));
        for key in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            bigger.push(LoadKind::Texture, key, 0, 0);
        }
        sets.define(bigger);
        app.update();
        seen.push(screens(&mut app)[0].1.progress);
        assert!(
            seen.windows(2).all(|pair| pair[1] >= pair[0]),
            "progress went down: {seen:?}"
        );
        assert_eq!(seen[1], 0.25, "{seen:?}");
        assert!(*seen.last().expect("seen") < 1.0, "{seen:?}");

        let mut level = Level::from_scene_file(id, Default::default());
        level.scene = regia_scene::format::SceneFile::from_ron_str(LEVEL).expect("level");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.update();
        assert!(
            screens(&mut app).is_empty(),
            "no loading screen once the level is active"
        );
        assert!(
            app.world().get_entity(screen).is_err(),
            "the prefab instance is despawned"
        );
        assert_eq!(
            app.world().resource::<LevelManager>().loading_screen_entity(),
            None
        );
        assert_eq!(
            crate::level_progress::level_load_progress(app.world(), id),
            Some(1.0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two `Replace` opens waiting at once share one screen, which shows the newer level and
    /// stays until neither waits.
    #[test]
    fn two_waiting_replace_opens_share_one_screen_until_both_are_done() {
        let root = project("loading_screen_two");
        let (mut app, _) = app(&root);
        let first = LevelId(Guid::from_u128(0x1e7e_0001));
        let second = LevelId(Guid::from_u128(0x1e7e_0002));
        open_level(app.world_mut(), first, LoadMode::Replace).expect("open first");
        let screen = screens(&mut app)[0].0;
        open_level(app.world_mut(), second, LoadMode::Replace).expect("open second");
        let now = screens(&mut app);
        assert_eq!(now.len(), 1, "one screen for both: {now:?}");
        assert_eq!(now[0].0, screen, "the same instance");
        assert_eq!(now[0].1.level, second, "it shows the newer open");

        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(first, Default::default()));
        app.update();
        assert_eq!(screens(&mut app).len(), 1, "the second open still waits");
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(second, Default::default()));
        app.update();
        assert!(screens(&mut app).is_empty());
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(second));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Acceptance: a load that fails (its file does not validate) also removes the screen, and
    /// a load whose data is already present shows none; an additive load shows none.
    #[test]
    fn a_failed_load_removes_the_loading_screen() {
        let root = project("loading_screen_failed");
        let (mut app, _) = app(&root);
        let id = LevelId(Guid::from_u128(0x01e7_ebad));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let screen = screens(&mut app)[0].0;
        app.update();
        assert_eq!(screens(&mut app).len(), 1);

        let mut level = Level::from_scene_file(id, Default::default());
        // Parsing validates, so the duplicate guid is added after: the load's own validation
        // refuses it.
        let mut scene = regia_scene::format::SceneFile::from_ron_str(
            r#"(format_version: 1, entities: [
            (guid: "8b6f2a11-0000-4000-8000-0000000010c1", name: "a", components: {}),
        ])"#,
        )
        .expect("parses");
        let twin = scene.entities[0].clone();
        scene.entities.push(twin);
        assert!(scene.validate().is_err(), "the level does not validate");
        level.scene = scene;
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.update();
        assert_eq!(
            app.world().resource::<LevelManager>().state(id),
            None,
            "the failed level is not tracked"
        );
        assert!(
            screens(&mut app).is_empty(),
            "a failed load removes the loading screen"
        );
        assert!(app.world().get_entity(screen).is_err());

        // Unloading a level that is still loading (a cancel) removes it too.
        let other = LevelId(Guid::from_u128(0x1e7e_ca9c));
        open_level(app.world_mut(), other, LoadMode::Replace).expect("open");
        assert_eq!(screens(&mut app).len(), 1);
        regia_scene::level::unload_level(app.world_mut(), other).expect("unload");
        assert!(
            screens(&mut app).is_empty(),
            "a cancelled load removes the loading screen"
        );

        // Data already present: the load completes in the call, no screen; additive: none.
        let ready = LevelId(Guid::from_u128(0x1e7e_00ad));
        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(ready, Default::default()));
        open_level(app.world_mut(), ready, LoadMode::Replace).expect("open");
        assert!(screens(&mut app).is_empty());
        let streamed = LevelId(Guid::from_u128(0x1e7e_0add));
        open_level(app.world_mut(), streamed, LoadMode::Additive).expect("open");
        assert!(
            screens(&mut app).is_empty(),
            "an additive load shows no loading screen"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
