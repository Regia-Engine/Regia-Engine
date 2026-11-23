// SPDX-License-Identifier: MPL-2.0
//! Installs a level's local reflection capture bake (`LevelLook::captures`, scene format v13,
//! ADR-2860; `book/agents/reflection-captures.md`).
//!
//! [`LocalCaptureLevelPlugin`] watches the open level's [`LevelSettings`]. When the level names a
//! `.fcap` it reads the file through the [`AssetServer`] (the cooked asset, from a pak or the dev
//! cache; an uncooked `assets/<path>` file in a dev run) and publishes every capture's map in
//! [`LocalCaptureBakes`]. `regia_render::sync_local_captures` then places each saved
//! [`regia_render::LocalCapture`] with the map baked at its position, so the level opens with its
//! reflections and no capture face is rendered; a capture moved since the bake (or one the file
//! lacks) bakes as before.
//!
//! A new file on disk, a hot reload of the cooked asset and a changed path reinstall it; closing
//! the level empties the bakes it published. A level that names no bake changes nothing.
//!
//! A missing file or a damaged one logs one warning and sets [`LevelCapturesState::error`] with
//! its code (`captures_not_found`, or the reader's `fcap_io`, `fcap_bad_magic`, `fcap_version`,
//! `fcap_truncated`, `fcap_crc`, `fcap_layout`, `fcap_value`); the captures then bake by
//! rendering, as without a file.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::local_capture_bake::FcapFile;
use regia_asset::{AssetServer, decode_cooked_file};
use regia_render::{LocalCaptureBake, LocalCaptureBakes};
use regia_scene::level::{LevelCaptures, LevelManager, LevelSettings};

use crate::baked_gi_level::{Source, Stamp, resolve, stamp_of};

/// The code of a bake the level names that is neither cooked nor on disk.
pub const CAPTURES_NOT_FOUND: &str = "captures_not_found";

/// What the level's capture bake install did, for the agent protocol
/// (`render.get_capture_bake` `level`) and host UIs.
#[derive(Resource, Default)]
pub struct LevelCapturesState {
    /// The `.fcap` path the open level names, as written in the level; `None` for none.
    pub file: Option<String>,
    /// The level's bakes are published in [`LocalCaptureBakes`].
    pub installed: bool,
    /// Captures in the installed file.
    pub captures: usize,
    /// Why the file could not be read; `None` when it loaded or none is named.
    pub error: Option<String>,
    /// A stable code for `error` (see the module docs).
    pub error_code: Option<&'static str>,
    /// Files read so far (a test counter: a frame where nothing changed reads none).
    pub reads: u32,
    wanted: Option<LevelCaptures>,
    stamp: Option<Stamp>,
}

/// [`LevelCapturesState`] as the agent protocol reports it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LevelCapturesReport {
    /// [`LevelCapturesState::file`].
    pub file: Option<String>,
    /// [`LevelCapturesState::installed`].
    pub installed: bool,
    /// [`LevelCapturesState::captures`].
    pub captures: usize,
    /// [`LevelCapturesState::error`].
    pub error: Option<String>,
    /// [`LevelCapturesState::error_code`].
    pub error_code: Option<&'static str>,
}

impl LevelCapturesState {
    /// The status as the agent protocol reports it.
    #[must_use]
    pub fn report(&self) -> LevelCapturesReport {
        LevelCapturesReport {
            file: self.file.clone(),
            installed: self.installed,
            captures: self.captures,
            error: self.error.clone(),
            error_code: self.error_code,
        }
    }
}

/// Publishes the open level's capture bake ([`sync_level_captures`]).
#[derive(Default)]
pub struct LocalCaptureLevelPlugin;

impl Plugin for LocalCaptureLevelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LevelCapturesState>()
            .init_resource::<LocalCaptureBakes>()
            .add_systems(Update, sync_level_captures);
    }
}

/// The renderer's bakes of a read file, by capture id.
#[must_use]
pub fn bakes_of(file: &FcapFile) -> Vec<(u64, [f32; 3], Arc<LocalCaptureBake>)> {
    file.captures
        .iter()
        .map(|c| {
            (
                c.id,
                c.center,
                Arc::new(LocalCaptureBake {
                    width: file.width,
                    coverage: c.coverage,
                    mips: c.mips.clone(),
                }),
            )
        })
        .collect()
}

fn read(
    path: &str,
    source: &Source,
    assets: Option<&mut AssetServer>,
) -> Result<FcapFile, (&'static str, String)> {
    let _span = tracing::info_span!("local_capture_level_read").entered();
    match source {
        Source::Cooked(id) => {
            let Some(server) = assets else {
                return Err(("fcap_io", format!("{path}: no asset server")));
            };
            let (bytes, _) = server
                .read_cooked(*id)
                .map_err(|e| ("fcap_io", format!("{path}: {e}")))?;
            let (_, payload) = decode_cooked_file(&bytes).map_err(|e| ("fcap_io", format!("{path}: {e}")))?;
            FcapFile::from_bytes(payload).map_err(|e| (e.code(), format!("{path}: {e}")))
        }
        Source::Raw(file) => FcapFile::read(file).map_err(|e| (e.code(), format!("{path}: {e}"))),
        Source::Missing => Err((CAPTURES_NOT_FOUND, format!("{path}: file not found"))),
    }
}

/// `Update`: publishes the open level's `look.captures` bake in [`LocalCaptureBakes`]. Reads the
/// file only when the path, the file or the cooked asset changed.
pub fn sync_level_captures(
    level: Option<Res<LevelSettings>>,
    manager: Option<Res<LevelManager>>,
    mut assets: Option<ResMut<AssetServer>>,
    mut bakes: ResMut<LocalCaptureBakes>,
    mut state: ResMut<LevelCapturesState>,
) {
    // The settings resource outlives the level that wrote it: a level-less world names no bake.
    let open = manager.is_none_or(|m| m.current().is_some());
    let wanted = level
        .as_deref()
        .filter(|_| open)
        .and_then(|l| l.look.captures.clone())
        .filter(|c| !c.file.trim().is_empty());
    let Some(want) = wanted else {
        if state.wanted.take().is_some() {
            // The level closed or stopped naming a bake: its bakes go.
            if state.installed {
                bakes.by_id.clear();
                bakes.generation += 1;
            }
            *state = LevelCapturesState {
                reads: state.reads,
                ..LevelCapturesState::default()
            };
        }
        return;
    };
    let source = resolve(&want.file, assets.as_deref_mut());
    let stamp = stamp_of(&source, assets.as_deref());
    if state.wanted.as_ref() == Some(&want) && state.stamp.as_ref() == Some(&stamp) {
        return;
    }
    state.wanted = Some(want.clone());
    state.stamp = Some(stamp);
    state.file = Some(want.file.clone());
    state.reads += 1;
    let had = state.installed;
    match read(&want.file, &source, assets.as_deref_mut()) {
        Ok(file) => {
            bakes.by_id = bakes_of(&file)
                .into_iter()
                .map(|(id, at, b)| (id, (at, b)))
                .collect();
            bakes.generation += 1;
            state.installed = true;
            state.captures = file.captures.len();
            state.error = None;
            state.error_code = None;
        }
        Err((code, message)) => {
            tracing::warn!(target: "regia_framework", code, "{message}");
            if had {
                bakes.by_id.clear();
                bakes.generation += 1;
            }
            state.installed = false;
            state.captures = 0;
            state.error = Some(message);
            state.error_code = Some(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::local_capture_bake::{FcapCapture, fcap_mip_bytes};

    fn bake_file(width: u32) -> FcapFile {
        FcapFile {
            width,
            captures: vec![FcapCapture {
                id: 4,
                center: [0.0, 1.0, 0.0],
                half_extent: [3.0, 2.0, 3.0],
                weight: 1.0,
                fade: 0.5,
                coverage: 0.75,
                mips: (0..3).map(|m| vec![7; fcap_mip_bytes(width, m)]).collect(),
            }],
        }
    }

    fn app_with_level(root: &std::path::Path, file: &str) -> App {
        let mut app = App::new();
        app.add_plugins(LocalCaptureLevelPlugin);
        let server = AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open asset server");
        app.insert_resource(server);
        let mut settings = LevelSettings::default();
        settings.look.captures = Some(LevelCaptures { file: file.into() });
        app.insert_resource(settings);
        app
    }

    #[test]
    fn a_level_publishes_its_bake_once_and_withdraws_it_when_it_stops_naming_it() {
        let root = regia_test_utils::unique_temp_dir("captures-level");
        std::fs::create_dir_all(root.join("assets/maps")).expect("dir");
        bake_file(16)
            .write(&root.join("assets/maps/hall.fcap"))
            .expect("write");
        let mut app = app_with_level(&root, "maps/hall.fcap");
        app.update();
        {
            let state = app.world().resource::<LevelCapturesState>();
            assert!(state.installed, "{:?}", state.error);
            assert_eq!(state.captures, 1);
            assert_eq!(state.reads, 1);
            let bakes = app.world().resource::<LocalCaptureBakes>();
            let bake = bakes.matching(4, [0.0, 1.0, 0.0]).expect("baked here");
            assert_eq!(bake.coverage, 0.75);
            // A capture moved since the bake gets none.
            assert!(bakes.matching(4, [0.5, 1.0, 0.0]).is_none());
        }
        // Frames where nothing changed read nothing.
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(app.world().resource::<LevelCapturesState>().reads, 1);
        let generation = app.world().resource::<LocalCaptureBakes>().generation;
        app.world_mut().resource_mut::<LevelSettings>().look.captures = None;
        app.update();
        let bakes = app.world().resource::<LocalCaptureBakes>();
        assert!(bakes.by_id.is_empty());
        assert!(bakes.generation > generation);
        assert!(!app.world().resource::<LevelCapturesState>().installed);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_damaged_or_missing_bake_is_refused_by_name() {
        let root = regia_test_utils::unique_temp_dir("captures-level-bad");
        std::fs::create_dir_all(root.join("assets")).expect("dir");
        let mut bytes = bake_file(8).to_bytes().expect("bytes");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(root.join("assets/bad.fcap"), &bytes).expect("write");
        let mut app = app_with_level(&root, "bad.fcap");
        app.update();
        let state = app.world().resource::<LevelCapturesState>();
        assert!(!state.installed);
        assert_eq!(state.error_code, Some("fcap_crc"));
        assert!(app.world().resource::<LocalCaptureBakes>().by_id.is_empty());
        let mut app = app_with_level(&root, "none.fcap");
        app.update();
        let report = app.world().resource::<LevelCapturesState>().report();
        assert_eq!(report.error_code, Some(CAPTURES_NOT_FOUND));
        assert_eq!(report.file.as_deref(), Some("none.fcap"));
        std::fs::remove_dir_all(&root).ok();
    }

    const HALL: &str = "00000000-0000-0000-0000-0000000000c1";

    /// A v13 level: one saved capture at (0, 1, 0) and the bake it names.
    fn level_text(look: &str) -> String {
        format!(
            r#"SceneFile(
    format_version: 13,
    entities: [
        (guid: "{HALL}", name: "hall", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            "regia_render::local_capture::LocalCapture": (id: 4, half_extent: (3.0, 2.0, 3.0), weight: 1.0, fade: 0.5),
        }}),
    ],
    resources: {{
        "regia_scene::level::LevelSettings": (look: ({look})),
    }},
)"#
        )
    }

    fn renderer_bake() -> FcapFile {
        let width = regia_render::LOCAL_CAPTURE_WIDTH;
        let chain = (0..regia_render::local_capture_mip_count())
            .map(|m| vec![3; fcap_mip_bytes(width, m)])
            .collect::<Vec<_>>();
        FcapFile {
            width,
            captures: vec![FcapCapture {
                id: 4,
                center: [0.0, 1.0, 0.0],
                half_extent: [3.0, 2.0, 3.0],
                weight: 1.0,
                fade: 0.5,
                coverage: 0.75,
                mips: chain,
            }],
        }
    }

    /// ADR-2860 acceptance: a level saved with a capture and naming its bake opens with the
    /// capture placed and its map installed from the file: no bake is queued, so no face renders.
    /// Without the bake the same level queues one.
    #[test]
    fn a_level_opens_with_its_saved_capture_and_installs_the_bake_without_rendering() {
        use regia_core::id::Guid;
        use regia_render::RuntimeRenderer;
        use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
        use regia_scene::prelude::ScenePlugin;

        let run = |look: &str| {
            let root = regia_test_utils::unique_temp_dir("captures-level-open");
            std::fs::create_dir_all(root.join("assets/maps")).expect("dir");
            renderer_bake()
                .write(&root.join("assets/maps/hall.fcap"))
                .expect("write");
            let mut app = App::new();
            app.add_plugins((ScenePlugin, LocalCaptureLevelPlugin));
            regia_render::register_render_types(&mut app);
            app.add_systems(Last, regia_render::sync_local_captures);
            let server = AssetServer::open(regia_asset::AssetServerSettings {
                project_root: root.clone(),
                platform: "linux".to_string(),
                content_dir: None,
            })
            .expect("open asset server");
            app.insert_resource(server);
            app.insert_resource(RuntimeRenderer::default());
            let id = LevelId(Guid::new_random());
            let level = Level::from_ron_str(id, &level_text(look)).expect("the v13 level parses");
            app.world_mut().resource_mut::<LevelLibrary>().insert(level);
            open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
            for _ in 0..3 {
                app.update();
            }
            let infos = app.world().resource::<RuntimeRenderer>().0.local_captures();
            std::fs::remove_dir_all(&root).ok();
            infos
        };
        let infos = run(r#"captures: Some((file: "maps/hall.fcap"))"#);
        assert_eq!(infos.len(), 1, "the saved capture is placed");
        let info = &infos[0];
        assert_eq!(info.desc.id, 4);
        assert_eq!(info.desc.center, [0.0, 1.0, 0.0]);
        assert_eq!(info.desc.half_extent, [3.0, 2.0, 3.0]);
        assert!(info.baked, "the bake came from the file");
        assert!(!info.pending, "no face render was queued");
        assert_eq!(info.coverage, 0.75);
        let without = run("");
        assert_eq!(without.len(), 1);
        assert!(!without[0].baked);
        assert!(without[0].pending, "without a bake the capture queues a render");
    }
}
