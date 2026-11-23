// SPDX-License-Identifier: MPL-2.0
//! A level's load progress, 0..1, for loading screens and `level.status` (spec 14 § A4,
//! ADR-7203).
//!
//! A level that is `Active` is done: `1.0`. A level that is still `Loading` reports the
//! fraction of its load set, the [`regia_asset::LoadSet`] named [`level_load_set_name`]
//! (`level:<guid>`): whoever streams the level's assets defines that set and the subsystems mark
//! its items resident as they arrive (spec 12 § 5b). A loading level without such a set reports
//! `0.0`. A loading value never reaches `1.0`: the level becomes `Active` on the frame its
//! data spawns, and only then is it complete. A level the [`LevelManager`] does not track reports
//! `None`.

use regia_app::prelude::*;
use regia_asset::LoadSets;
use regia_scene::level::{LevelId, LevelManager, LevelState};

/// Largest `f32` below 1.0: a loading level never reads as finished.
const ONE_BELOW: f32 = f32::from_bits(0x3f7f_ffff);

/// The name of the load set that tracks `level`'s assets: `level:<guid>`.
#[must_use]
pub fn level_load_set_name(level: LevelId) -> String {
    format!("level:{}", level.0)
}

/// `level`'s load progress in `0.0..=1.0` (see the module docs), or `None` when the world's
/// [`LevelManager`] does not track it.
#[must_use]
pub fn level_load_progress(world: &World, level: LevelId) -> Option<f32> {
    let state = world.get_resource::<LevelManager>()?.state(level)?;
    Some(match state {
        LevelState::Active => 1.0,
        LevelState::Loading => world
            .get_resource::<LoadSets>()
            .and_then(|sets| sets.load_progress(&level_load_set_name(level)))
            .map_or(0.0, |progress| {
                if progress.fraction.is_finite() {
                    progress.fraction.clamp(0.0, ONE_BELOW)
                } else {
                    0.0
                }
            }),
    })
}

/// One level's state for a loading screen and the agent method `level.status`.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelStatus {
    /// The level.
    pub level: LevelId,
    /// `"loading"` or `"active"`.
    pub state: &'static str,
    /// [`level_load_progress`].
    pub progress: f32,
    /// Whether it is the manager's current (primary) level.
    pub current: bool,
    /// Items of its load set that failed (`kind:key: error`); empty without a set.
    pub failed: Vec<String>,
}

/// The status of `level`, or `None` when the [`LevelManager`] does not track it.
#[must_use]
pub fn level_status(world: &World, level: LevelId) -> Option<LevelStatus> {
    let manager = world.get_resource::<LevelManager>()?;
    let state = manager.state(level)?;
    let failed = world
        .get_resource::<LoadSets>()
        .and_then(|sets| sets.load_progress(&level_load_set_name(level)))
        .map(|progress| progress.failed)
        .unwrap_or_default();
    Some(LevelStatus {
        level,
        state: match state {
            LevelState::Loading => "loading",
            LevelState::Active => "active",
        },
        progress: level_load_progress(world, level).unwrap_or(0.0),
        current: manager.current() == Some(level),
        failed,
    })
}

/// Every tracked level's status, in level id order.
#[must_use]
pub fn level_statuses(world: &World) -> Vec<LevelStatus> {
    let Some(manager) = world.get_resource::<LevelManager>() else {
        return Vec::new();
    };
    let mut ids: Vec<LevelId> = manager.levels().map(|(id, _)| id).collect();
    ids.sort();
    ids.into_iter().filter_map(|id| level_status(world, id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{LoadKind, LoadSet, LoadSets};
    use regia_core::id::Guid;
    use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};

    #[test]
    fn progress_follows_the_level_load_set_and_reaches_one_when_active() {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let sets = LoadSets::default();
        app.insert_resource(sets.clone());
        let id = LevelId(Guid::from_u128(0x1e7e1));
        assert_eq!(level_load_progress(app.world(), id), None, "unknown level");

        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        assert_eq!(
            level_load_progress(app.world(), id),
            Some(0.0),
            "loading, no set yet"
        );

        let mut set = LoadSet::new(level_load_set_name(id));
        for key in ["a", "b", "c", "d"] {
            set.push(LoadKind::Texture, key, 0, 0);
        }
        sets.define(set);
        sets.mark_resident(LoadKind::Texture, "a");
        assert_eq!(level_load_progress(app.world(), id), Some(0.25));
        for key in ["b", "c", "d"] {
            sets.mark_resident(LoadKind::Texture, key);
        }
        let done = level_load_progress(app.world(), id).expect("loading");
        assert!(done < 1.0, "1.0 only once the level is active, got {done}");
        assert!(done > 0.99, "{done}");

        app.world_mut()
            .resource_mut::<LevelLibrary>()
            .insert(Level::from_scene_file(id, Default::default()));
        app.update();
        assert_eq!(level_load_progress(app.world(), id), Some(1.0));
        let status = level_status(app.world(), id).expect("status");
        assert_eq!(status.state, "active");
        assert_eq!(status.progress, 1.0);
    }
}
