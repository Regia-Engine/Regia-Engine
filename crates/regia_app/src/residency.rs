// SPDX-License-Identifier: MPL-2.0
//! [`ResidencyPlugin`]: runs a [`ResidencyKeeper`] for the app's lifetime, so data a frame may
//! read at any moment (the terrain heights where something spawns) is not paged out between
//! uses on a machine short of memory and a frame never sleeps reading it back (see
//! [`regia_core::residency`]).
//!
//! `REGIA_KEEP_RESIDENT=0` turns it off (an A/B, or a box where the sweep is unwanted);
//! `REGIA_KEEP_RESIDENT=code` also keeps the process's code resident.

use regia_core::residency::{ResidencyKeeper, ResidencySettings, ResidencyStats};
use regia_ecs::prelude::*;

use crate::{App, First, Plugin, Time};

/// The running keeper; [`Residency::stats`] for the agent protocol and diagnostics.
#[derive(Resource)]
pub struct Residency {
    keeper: Option<ResidencyKeeper>,
    settings: ResidencySettings,
    /// Stats of the keepers stopped so far (a suspend stops the sweep thread).
    stopped: ResidencyStats,
}

impl Residency {
    /// What the keeper has done so far: the cumulative totals (`sweeps`, `restored_pages`,
    /// `max_sweep_us`) of every keeper stopped by a suspend plus the live keeper's, while one
    /// runs; the last-sweep gauges (`data_bytes`, `code_bytes`, `pages`, `last_sweep_us`)
    /// describe the most recent sweep either way.
    #[must_use]
    pub fn stats(&self) -> ResidencyStats {
        let Some(keeper) = &self.keeper else {
            return self.stopped;
        };
        let mut live = keeper.stats();
        live.sweeps = self.stopped.sweeps.saturating_add(live.sweeps);
        live.restored_pages = self.stopped.restored_pages.saturating_add(live.restored_pages);
        live.max_sweep_us = self.stopped.max_sweep_us.max(live.max_sweep_us);
        live
    }

    /// True while the sweep thread runs (it does not while the app is suspended).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.keeper.is_some()
    }
}

/// `First` system: stops the sweep thread while the app is suspended (a backgrounded phone must
/// not keep faulting pages in, which would defeat the OS reclaiming them) and starts it again on
/// resume.
fn follow_suspend(time: Option<Res<Time>>, mut residency: ResMut<Residency>) {
    let suspended = time.is_some_and(|t| t.is_suspended());
    if suspended {
        if let Some(keeper) = residency.keeper.take() {
            merge_stopped(&mut residency.stopped, keeper.stats());
        }
    } else if residency.keeper.is_none() {
        let settings = residency.settings;
        residency.keeper = ResidencyKeeper::start(settings);
    }
}

/// Adds a stopped keeper's cumulative totals into the saved ones: `sweeps` and
/// `restored_pages` add, `max_sweep_us` keeps the peak, and the last-sweep gauges become the
/// stopped keeper's last values (what `Residency::stats` reports while no keeper runs).
fn merge_stopped(stopped: &mut ResidencyStats, live: ResidencyStats) {
    stopped.sweeps = stopped.sweeps.saturating_add(live.sweeps);
    stopped.restored_pages = stopped.restored_pages.saturating_add(live.restored_pages);
    stopped.max_sweep_us = stopped.max_sweep_us.max(live.max_sweep_us);
    stopped.data_bytes = live.data_bytes;
    stopped.code_bytes = live.code_bytes;
    stopped.pages = live.pages;
    stopped.last_sweep_us = live.last_sweep_us;
}

/// Starts the keeper when the app is built (Linux and Android; elsewhere it adds nothing).
#[derive(Debug, Clone, Copy, Default)]
pub struct ResidencyPlugin {
    /// Sweep settings (`None`: the defaults, adjusted by `REGIA_KEEP_RESIDENT`).
    pub settings: Option<ResidencySettings>,
}

impl Plugin for ResidencyPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<Residency>() {
            return;
        }
        let env = env_token(regia_core::brand::env_var_os("REGIA_KEEP_RESIDENT"));
        let Some(settings) = settings_for(self.settings, env.as_deref()) else {
            return;
        };
        if let Some(keeper) = ResidencyKeeper::start(settings) {
            app.insert_resource(Residency {
                keeper: Some(keeper),
                settings,
                stopped: ResidencyStats::default(),
            })
            .add_systems(First, follow_suspend);
        }
    }
}

/// The variable's text. A value that is not valid UTF-8 is read lossily, and one that starts with
/// `0` still means off (the bytes after it are garbage the owner cannot see in a shell).
fn env_token(raw: Option<std::ffi::OsString>) -> Option<String> {
    let raw = raw?;
    match raw.into_string() {
        Ok(text) => Some(text),
        Err(raw) => {
            let lossy = raw.to_string_lossy();
            Some(if lossy.trim_start().starts_with('0') {
                "0".to_owned()
            } else {
                lossy.into_owned()
            })
        }
    }
}

/// The settings `REGIA_KEEP_RESIDENT` asks for: `0` none, `code` with code, else `base`.
fn settings_for(base: Option<ResidencySettings>, env: Option<&str>) -> Option<ResidencySettings> {
    let mut settings = base.unwrap_or_default();
    match env.map(str::trim) {
        Some("0") => return None,
        Some(off) if ["false", "off", "no"].iter().any(|v| off.eq_ignore_ascii_case(v)) => return None,
        Some("code") => settings.code = true,
        _ => {}
    }
    Some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_switches_the_keeper() {
        assert_eq!(settings_for(None, Some("0")), None);
        for off in ["false", "off", "No", " OFF "] {
            assert_eq!(settings_for(None, Some(off)), None, "{off:?}");
        }
        assert_eq!(settings_for(None, None), Some(ResidencySettings::default()));
        assert_eq!(settings_for(None, Some("code")).map(|s| s.code), Some(true));
        assert_eq!(settings_for(None, Some("1")).map(|s| s.code), Some(false));
    }

    #[test]
    fn a_non_utf8_value_starting_with_zero_is_off() {
        use std::ffi::OsString;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(vec![b'0', 0xFF]);
            let token = env_token(Some(raw));
            assert_eq!(settings_for(None, token.as_deref()), None);
            let other = env_token(Some(OsString::from_vec(vec![b'x', 0xFF])));
            assert!(settings_for(None, other.as_deref()).is_some());
        }
        assert_eq!(env_token(None), None);
        assert_eq!(env_token(Some(OsString::from("code"))).as_deref(), Some("code"));
    }

    /// A suspend stops the sweep thread and a resume restarts it.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn suspend_stops_the_keeper_and_resume_restarts_it() {
        let off = regia_core::brand::env_var("REGIA_KEEP_RESIDENT").is_ok_and(|v| v.trim() == "0");
        if off {
            return;
        }
        let mut app = App::new();
        app.add_plugins(ResidencyPlugin::default());
        app.init_resource::<Time>();
        app.world_mut().run_schedule(First);
        assert!(app.world().resource::<Residency>().is_running());
        app.world_mut().resource_mut::<Time>().set_suspended(true);
        app.world_mut().run_schedule(First);
        let r = app.world().resource::<Residency>();
        assert!(!r.is_running());
        assert!(r.stats().sweeps >= 1, "the numbers survive the stop");
        app.world_mut().resource_mut::<Time>().set_suspended(false);
        app.world_mut().run_schedule(First);
        assert!(app.world().resource::<Residency>().is_running());
    }

    /// A suspend/resume cycle (finding 3 of the app-correctness audit) must not reset the
    /// cumulative keeper totals the agent hitch report subtracts: the sweeps and restored pages
    /// of every keeper add up, across stops, and `stats` reports `stopped + live` while a keeper
    /// runs.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn suspend_resume_accumulates_keeper_totals_across_keepers() {
        let off = regia_core::brand::env_var("REGIA_KEEP_RESIDENT").is_ok_and(|v| v.trim() == "0");
        if off {
            return;
        }
        let mut app = App::new();
        app.add_plugins(ResidencyPlugin::default());
        app.init_resource::<Time>();
        // A deterministic stand-in for a keeper that already worked: the fresh keeper's counts
        // must add to these, not replace them.
        app.insert_resource(Residency {
            keeper: None,
            settings: ResidencySettings::default(),
            stopped: ResidencyStats {
                sweeps: 100,
                restored_pages: 10,
                max_sweep_us: 500,
                ..ResidencyStats::default()
            },
        });
        app.world_mut().run_schedule(First); // resume: starts a fresh keeper
        let stats = app.world().resource::<Residency>().stats();
        assert!(
            stats.sweeps >= 100 && stats.restored_pages >= 10 && stats.max_sweep_us >= 500,
            "the live keeper's counts add to the stopped ones: {stats:?}"
        );
        app.world_mut().resource_mut::<Time>().set_suspended(true);
        app.world_mut().run_schedule(First); // suspend: merges the live keeper into `stopped`
        let stopped = app.world().resource::<Residency>().stats();
        assert!(
            stopped.sweeps >= 100 && stopped.restored_pages >= 10 && stopped.max_sweep_us >= 500,
            "the totals survive the stop: {stopped:?}"
        );
        app.world_mut().resource_mut::<Time>().set_suspended(false);
        app.world_mut().run_schedule(First); // a second keeper still reports the earlier totals
        let stats = app.world().resource::<Residency>().stats();
        assert!(
            stats.sweeps >= 100 && stats.restored_pages >= 10 && stats.max_sweep_us >= 500,
            "a second keeper does not reset the running totals: {stats:?}"
        );
    }

    #[test]
    fn stopping_a_keeper_merges_its_totals_into_the_stopped_ones() {
        // The merge on its own, without threads: cumulative fields add (max for the sweep peak),
        // the last-sweep gauges come from the newer sample.
        let mut stopped = ResidencyStats {
            sweeps: 100,
            restored_pages: 10,
            max_sweep_us: 500,
            data_bytes: 4096,
            ..ResidencyStats::default()
        };
        let live = ResidencyStats {
            sweeps: 3,
            restored_pages: 2,
            max_sweep_us: 900,
            data_bytes: 8192,
            pages: 64,
            last_sweep_us: 900,
            code_bytes: 0,
        };
        merge_stopped(&mut stopped, live);
        assert_eq!(stopped.sweeps, 103);
        assert_eq!(stopped.restored_pages, 12);
        assert_eq!(stopped.max_sweep_us, 900, "the peak is a maximum, not a sum");
        assert_eq!(stopped.data_bytes, 8192, "the gauges come from the newer sample");
        assert_eq!(stopped.last_sweep_us, 900);
    }

    #[test]
    fn plugin_installs_the_keeper_where_supported() {
        let mut app = App::new();
        app.add_plugins(ResidencyPlugin::default());
        let installed = app.world().contains_resource::<Residency>();
        let off = regia_core::brand::env_var("REGIA_KEEP_RESIDENT").is_ok_and(|v| v.trim() == "0");
        if cfg!(any(target_os = "linux", target_os = "android")) && !off {
            assert!(installed);
            assert!(app.world().resource::<Residency>().stats().sweeps >= 1);
        } else {
            assert!(!installed);
        }
    }
}
