// SPDX-License-Identifier: MPL-2.0
//! A level switch with cancel: one level loading at a time, the old one freed (ADR-8027).
//!
//! [`LevelSwitch::request`] reads and parses the next level's source on a worker
//! ([`regia_core::load_switch::LoadSwitch`]). Requesting another level before it lands cancels the
//! read in flight; [`LevelSwitch::cancel`] cancels it and keeps the open level. When a read lands,
//! [`level_switch_poll`] (`PreUpdate`, [`LevelSwitchPlugin`]) fills the [`LevelLibrary`] with it
//! (the definitions it binds and the levels it places, as [`crate::level_assets::load_level_source`]
//! does), opens it with [`LoadMode::Replace`], and takes the replaced level's parsed data out of
//! the library and drops it on a retire worker. The next read waits for that drop, so the parsed
//! data of at most one level is being built or freed at a time.
//!
//! A host with a per-level bank of its own (CPU mesh copies in a
//! `regia_render::host_memory::HostMeshStore`, a decoded audio bank) hands the old bank to
//! [`LevelSwitch::retire`] when the switch lands; the next request waits for it too.
//!
//! The agent surface is `level.switch` and `level.switch_status` (book/agents/level-switch.md).

use std::path::PathBuf;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::load_switch::{CancelToken, LoadSwitch, LoadSwitchStats, SwitchEvent};
use regia_scene::level::{Level, LevelId, LevelLibrary, LevelManager, LoadMode, open_level};
use regia_scene::level_dir::read_level_as_stored;

use crate::level_assets::{level_id_of, load_level_source_parsed};

/// A level source read and parsed on the switch's worker.
pub struct ParsedLevel {
    /// The source it was read from.
    pub path: PathBuf,
    /// The parsed level.
    pub level: Level,
}

/// How the last switch ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LevelSwitchOutcome {
    /// The level opened and is current.
    Opened {
        /// The source it was read from.
        path: PathBuf,
        /// Its id.
        level: LevelId,
        /// The level it replaced (its parsed data was retired), if any.
        replaced: Option<LevelId>,
    },
    /// The read or the open failed; the level that was open stays open.
    Failed {
        /// The source asked for.
        path: String,
        /// Why (the reader's or the open's error text).
        error: String,
    },
    /// [`LevelSwitch::cancel`] stopped it; the level that was open stays open.
    Cancelled {
        /// The source asked for.
        path: String,
    },
}

/// The level switch: at most one level read in flight, the replaced level freed on a worker.
#[derive(Resource, Default)]
pub struct LevelSwitch {
    loads: LoadSwitch<ParsedLevel>,
    last: Option<LevelSwitchOutcome>,
    requests: u64,
}

impl std::fmt::Debug for LevelSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelSwitch")
            .field("loading", &self.loading())
            .field("last", &self.last)
            .field("stats", &self.stats())
            .finish()
    }
}

impl LevelSwitch {
    /// Starts reading the level at `path` (`.level.ron` or `.level.d`) on a worker. A read already
    /// in flight is cancelled. The open level stays until the new one lands.
    pub fn request(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        let name = path.display().to_string();
        self.requests += 1;
        tracing::info!(target: "regia_framework", level = %name, "level switch: requested");
        self.loads
            .switch_to(name, move |token: &CancelToken| read_parsed(path, token));
    }

    /// Cancels the read in flight, if any; the open level stays. True when one was cancelled.
    pub fn cancel(&mut self) -> bool {
        let path = self.loads.loading().map(str::to_owned);
        let cancelled = self.loads.cancel();
        if let (true, Some(path)) = (cancelled, path) {
            self.last = Some(LevelSwitchOutcome::Cancelled { path });
        }
        cancelled
    }

    /// Frees `value` on a retire worker; the next read waits for it (a host's bank of the level it
    /// switched away from).
    pub fn retire<V: Send + 'static>(&mut self, value: V) {
        self.loads.retire(value);
    }

    /// The source being read, if any.
    #[must_use]
    pub fn loading(&self) -> Option<&str> {
        self.loads.loading()
    }

    /// How the last switch ended.
    #[must_use]
    pub fn last(&self) -> Option<&LevelSwitchOutcome> {
        self.last.as_ref()
    }

    /// Requests since start.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Retire workers still running.
    #[must_use]
    pub fn retiring(&self) -> usize {
        self.loads.retiring()
    }

    /// Blocks until every retire worker has finished, or `timeout` passes (tools and tests).
    #[must_use]
    pub fn wait_retired(&self, timeout: Duration) -> bool {
        self.loads.wait_retired(timeout)
    }

    /// The worker counters (reads started, skipped, cancelled, landed, failed; retires).
    #[must_use]
    pub fn stats(&self) -> LoadSwitchStats {
        self.loads.stats()
    }

    /// Blocks up to `timeout` for the read in flight, then finishes it like [`level_switch_poll`]
    /// (tools and tests; a frame loop lets the system poll). `None` when nothing landed.
    pub fn wait_and_open(world: &mut World, timeout: Duration) -> Option<LevelSwitchOutcome> {
        let mut switch = world.remove_resource::<LevelSwitch>()?;
        let event = switch.loads.wait(timeout);
        let outcome = event.map(|event| switch.finish(world, event));
        world.insert_resource(switch);
        outcome
    }

    fn finish(&mut self, world: &mut World, event: SwitchEvent) -> LevelSwitchOutcome {
        let outcome = match event {
            SwitchEvent::Loaded { name } => match self.loads.take_current() {
                Some((_, parsed)) => self.open(world, parsed),
                None => LevelSwitchOutcome::Failed {
                    path: name,
                    error: "the landed level was not kept".into(),
                },
            },
            SwitchEvent::Failed { name, error } => LevelSwitchOutcome::Failed { path: name, error },
        };
        if let LevelSwitchOutcome::Failed { path, error } = &outcome {
            tracing::warn!(target: "regia_framework", level = %path, "level switch failed: {error}");
        }
        self.last = Some(outcome.clone());
        outcome
    }

    fn open(&mut self, world: &mut World, parsed: ParsedLevel) -> LevelSwitchOutcome {
        let _span = tracing::info_span!("level_switch_open").entered();
        let ParsedLevel { path, level } = parsed;
        let id = level.id;
        let failed = |error: String| LevelSwitchOutcome::Failed {
            path: path.display().to_string(),
            error,
        };
        let previous = world
            .get_resource::<LevelManager>()
            .and_then(LevelManager::current);
        if let Err(err) = load_level_source_parsed(world, &path, level) {
            return failed(err.to_string());
        }
        if let Err(err) = open_level(world, id, LoadMode::Replace) {
            return failed(err.to_string());
        }
        let replaced = previous.filter(|previous| *previous != id);
        if let Some(old) = replaced {
            // The replaced level's parsed data leaves the library (a later switch reads it again)
            // and drops on a worker, never on this frame.
            if let Some(data) = world
                .get_resource_mut::<LevelLibrary>()
                .and_then(|mut library| library.remove(old))
            {
                self.loads.retire(data);
            }
        }
        tracing::info!(target: "regia_framework", level = %path.display(), "level switch: opened");
        LevelSwitchOutcome::Opened {
            path,
            level: id,
            replaced,
        }
    }
}

/// The worker half: the level's id and its parsed source, with cancel checks around the read.
fn read_parsed(path: PathBuf, token: &CancelToken) -> Result<ParsedLevel, String> {
    token.check()?;
    let id = level_id_of(&path);
    let scene = read_level_as_stored(&path).map_err(|err| format!("{}: {err}", err.reason()))?;
    token.check()?;
    Ok(ParsedLevel {
        level: Level::from_scene_file(id, scene),
        path,
    })
}

/// `PreUpdate`: opens a level whose read landed (see the module docs).
pub fn level_switch_poll(world: &mut World) {
    let Some(mut switch) = world.remove_resource::<LevelSwitch>() else {
        return;
    };
    if let Some(event) = switch.loads.poll() {
        switch.finish(world, event);
    }
    world.insert_resource(switch);
}

/// Installs [`LevelSwitch`] and [`level_switch_poll`].
pub struct LevelSwitchPlugin;

impl Plugin for LevelSwitchPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LevelSwitch>()
            .add_systems(PreUpdate, level_switch_poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::level::LevelState;

    use regia_core::id::Guid;
    use regia_scene::ScenePlugin;
    use regia_scene::format::SCENE_FORMAT_VERSION;
    use regia_test_utils::unique_temp_dir;
    use std::path::Path;
    use std::time::Instant;

    const DEADLINE: Duration = Duration::from_secs(60);

    /// A level file of `entities` transformed entities with guids from `seed` up.
    fn write_level(dir: &Path, name: &str, seed: u128, entities: u128) -> PathBuf {
        let records: String = (0..entities)
            .map(|index| {
                format!(
                    r#"(guid: "{}", components: {{ "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)) }}),"#,
                    Guid::from_u128(seed + index)
                )
            })
            .collect();
        let text = format!(
            "SceneFile(format_version: {SCENE_FORMAT_VERSION}, entities: [{records}], resources: {{}})"
        );
        let path = dir.join(format!("{name}.level.ron"));
        regia_test_utils::write_atomic(&path, text.as_bytes()).expect("write level");
        path
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, ScenePlugin, LevelSwitchPlugin));
        app
    }

    fn open(app: &mut App) -> LevelSwitchOutcome {
        LevelSwitch::wait_and_open(app.world_mut(), DEADLINE).expect("a switch landed")
    }

    #[test]
    fn a_switch_replaces_the_level_and_frees_the_old_one() {
        let dir = unique_temp_dir("level_switch_replace");
        let a = write_level(&dir, "canyon", 0x100, 3);
        let b = write_level(&dir, "bore", 0x200, 2);
        let mut app = app();
        app.world_mut().resource_mut::<LevelSwitch>().request(&a);
        let LevelSwitchOutcome::Opened {
            level: first,
            replaced,
            ..
        } = open(&mut app)
        else {
            panic!("canyon did not open");
        };
        assert_eq!(replaced, None);
        app.world_mut().resource_mut::<LevelSwitch>().request(&b);
        let LevelSwitchOutcome::Opened {
            level: second,
            replaced,
            ..
        } = open(&mut app)
        else {
            panic!("bore did not open");
        };
        assert_eq!(replaced, Some(first));
        let world = app.world();
        let manager = world.resource::<LevelManager>();
        assert_eq!(manager.current(), Some(second));
        assert_eq!(manager.state(first), None, "the old level is still tracked");
        assert_eq!(manager.state(second), Some(LevelState::Active));
        let library = world.resource::<LevelLibrary>();
        assert!(!library.contains(first), "the old level's data stayed resident");
        assert!(library.contains(second));
        let switch = world.resource::<LevelSwitch>();
        assert!(switch.wait_retired(DEADLINE));
        assert_eq!(switch.stats().retires, 1);
        assert_eq!(switch.stats().loads_landed, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_request_cancels_the_first_and_cancel_keeps_the_open_level() {
        let dir = unique_temp_dir("level_switch_cancel");
        let a = write_level(&dir, "canyon", 0x100, 1);
        let b = write_level(&dir, "bore", 0x200, 1);
        let c = write_level(&dir, "ridge", 0x300, 1);
        let mut app = app();
        app.world_mut().resource_mut::<LevelSwitch>().request(&a);
        let LevelSwitchOutcome::Opened { level: canyon, .. } = open(&mut app) else {
            panic!("canyon did not open");
        };
        {
            let mut switch = app.world_mut().resource_mut::<LevelSwitch>();
            switch.request(&b);
            switch.request(&c);
            assert_eq!(switch.loading(), Some(c.display().to_string().as_str()));
            assert!(switch.cancel());
            assert!(!switch.cancel());
            assert!(matches!(
                switch.last(),
                Some(LevelSwitchOutcome::Cancelled { .. })
            ));
        }
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(canyon));
        let stats = app.world().resource::<LevelSwitch>().stats();
        assert_eq!(stats.loads_cancelled, 2, "{stats:?}");
        assert_eq!(stats.loads_landed, 1, "{stats:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_source_fails_and_keeps_the_open_level() {
        let dir = unique_temp_dir("level_switch_missing");
        let a = write_level(&dir, "canyon", 0x100, 1);
        let mut app = app();
        app.world_mut().resource_mut::<LevelSwitch>().request(&a);
        let LevelSwitchOutcome::Opened { level: canyon, .. } = open(&mut app) else {
            panic!("canyon did not open");
        };
        app.world_mut()
            .resource_mut::<LevelSwitch>()
            .request(dir.join("nowhere.level.ron"));
        let outcome = open(&mut app);
        let LevelSwitchOutcome::Failed { path, .. } = &outcome else {
            panic!("expected a failure, got {outcome:?}");
        };
        assert!(path.ends_with("nowhere.level.ron"), "{path}");
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(canyon));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_frame_system_opens_a_landed_level() {
        let dir = unique_temp_dir("level_switch_poll");
        let a = write_level(&dir, "canyon", 0x100, 2);
        let mut app = app();
        app.world_mut().resource_mut::<LevelSwitch>().request(&a);
        // Frames run until the frame's system opened the landed read (a deadline, not a budget).
        let deadline = Instant::now() + DEADLINE;
        while app.world().resource::<LevelSwitch>().last().is_none() && Instant::now() < deadline {
            app.update();
            std::thread::yield_now();
        }
        assert!(matches!(
            app.world().resource::<LevelSwitch>().last(),
            Some(LevelSwitchOutcome::Opened { .. })
        ));
        assert!(app.world().resource::<LevelManager>().current().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
