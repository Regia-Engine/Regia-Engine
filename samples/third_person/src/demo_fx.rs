// SPDX-License-Identifier: MIT-0
//! Demo recording and scrubbing for the sample: `F6` records a run (`saved/demos/run.frec` under the
//! data directory when it stops), `F7` plays the last recording and pauses or resumes it,
//! `BracketLeft` / `BracketRight` scrub one second back or forward, and `F8` opens the scrub bar.
//! The same operations are the agent methods `demo.record_start`, `demo.record_stop`, `demo.play`,
//! `demo.pause`, `demo.seek` and `demo.scrub_bar` (`book/agents/demo.md`).
//!
//! What a demo restores is the tracked state: each player's character (its reflected components)
//! carries [`DemoTracked`]. The character's input is part of that state, so a run recorded with
//! the same keys held (a walk) replays exactly; the simulation runs in `FixedUpdate`, as a seek
//! requires.

use std::path::PathBuf;

use regia::prelude::*;
use regia_framework::demo::DemoHeader;
use regia_framework::demo_world::{
    DemoDriver, DemoMode, DemoRequest, DemoTracked, DemoWorldPlugin, demo_status,
};

/// The keyframe interval of demos the keys record (fixed steps; one second at 60 Hz).
pub const KEYFRAME_INTERVAL: u32 = 60;

/// The tick rate stored in demos the keys record.
pub const TICK_RATE_HZ: u32 = 60;

/// Adds the demo driver and the scrub bar when the host did not.
pub fn add_plugins(app: &mut App) {
    if !app.world().contains_resource::<DemoDriver>() {
        app.add_plugins(DemoWorldPlugin);
    }
    if !app
        .world()
        .contains_resource::<regia_framework::demo_scrub::DemoScrubBar>()
    {
        app.add_plugins(regia_framework::demo_scrub::DemoScrubPlugin);
    }
}

/// Marks `character` as demo state and names it (a demo keys entities by name, never by id).
pub fn track_character(world: &mut World, character: Entity, player: u8) {
    let name = format!("player-{player}");
    world.entity_mut(character).insert((
        DemoTracked,
        Name::new(name.clone()),
        regia_scene::components::Name::new(name),
    ));
}

/// Where the keys save a demo.
fn demo_path(world: &World) -> PathBuf {
    world
        .get_resource::<crate::SampleDataDir>()
        .map(|dir| dir.0.clone())
        .unwrap_or_default()
        .join("demos")
        .join("run.frec")
}

/// `F6`: starts a recording, or stops the running one and saves it.
pub fn toggle_record(world: &mut World) {
    let path = demo_path(world);
    let Some(mut driver) = world.get_resource_mut::<DemoDriver>() else {
        return;
    };
    if driver.mode() == DemoMode::Recording {
        // `save_frec` writes into the file's directory and does not create it: a fresh data
        // dir has no `demos/`, and the recording would be lost with only `last_error` left.
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(target: "third_person", "demo: cannot create {}: {err}", parent.display());
        }
        driver.request(DemoRequest::StopRecording { path: Some(path) });
    } else {
        driver.request(DemoRequest::Record {
            header: DemoHeader::new("third_person", 0, TICK_RATE_HZ, KEYFRAME_INTERVAL),
            actions: None,
        });
    }
}

/// `F7`: plays the last recording, or pauses and resumes the one playing.
pub fn toggle_play(world: &mut World) {
    let Some(status) = demo_status(world) else {
        return;
    };
    let Some(mut driver) = world.get_resource_mut::<DemoDriver>() else {
        return;
    };
    match status.mode {
        DemoMode::Playing => driver.request(DemoRequest::Pause(true)),
        DemoMode::Paused => driver.request(DemoRequest::Pause(false)),
        DemoMode::Idle | DemoMode::Finished => {
            if let Some(demo) = driver.recording().cloned() {
                driver.request(DemoRequest::Play(Box::new(demo)));
            }
        }
        DemoMode::Recording => {}
    }
}

/// `BracketLeft` / `BracketRight`: scrubs `seconds` (negative: back) through the loaded demo.
pub fn scrub(world: &mut World, seconds: i64) {
    let Some(status) = demo_status(world) else {
        return;
    };
    if !matches!(
        status.mode,
        DemoMode::Playing | DemoMode::Paused | DemoMode::Finished
    ) {
        return;
    }
    let rate = i64::from(status.tick_rate_hz.max(1));
    let target = i64::try_from(status.tick).unwrap_or(0) + seconds * rate;
    let target = u64::try_from(target.clamp(0, i64::try_from(status.steps).unwrap_or(i64::MAX))).unwrap_or(0);
    if let Some(mut driver) = world.get_resource_mut::<DemoDriver>() {
        driver.request(DemoRequest::Seek(target));
    }
}

/// `F8`: shows or hides the scrub bar.
pub fn toggle_scrub_bar(world: &mut World) {
    if let Some(mut bar) = world.get_resource_mut::<regia_framework::demo_scrub::DemoScrubBar>() {
        bar.open = !bar.open;
    }
}
