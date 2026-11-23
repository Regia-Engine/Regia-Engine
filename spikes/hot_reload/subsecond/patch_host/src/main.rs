//! subsecond hot-patching probe. Run under `dx serve --hot-patch` (see ../../README.md), then edit
//! the body of `game_system` (the `MARKER` line and the increment): the running process picks up
//! the new function body without restarting and without losing `Game`'s state.
//!
//! Prints `PATCHPROBE <unix ms> frame=<n> value=<v> marker=<m>` ten times per second so a driver
//! script can measure save → visible latency.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regia::prelude::*;
use regia_app::App;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Game state that must survive a patch.
#[derive(Resource, Default)]
struct Game {
    frame: u64,
    value: i64,
}

fn game_system(mut g: ResMut<Game>) {
    const MARKER: &str = "v1";
    g.frame += 1;
    g.value += 1;
    if g.frame % 6 == 0 {
        println!(
            "PATCHPROBE {} frame={} value={} marker={}",
            now_ms(),
            g.frame,
            g.value,
            MARKER
        );
    }
}

#[cfg(feature = "hotpatching")]
fn setup_hotpatch(app: &mut App) {
    use bevy_ecs::change_detection::DetectChangesMut;
    let flag = Arc::new(AtomicBool::new(false));
    let set = flag.clone();
    dioxus_devtools::connect_subsecond();
    dioxus_devtools::subsecond::register_handler(Arc::new(move || set.store(true, Ordering::Release)));
    app.init_resource::<bevy_ecs::HotPatchChanges>();
    app.add_message::<bevy_ecs::HotPatched>();
    app.add_systems(
        First,
        move |mut changes: ResMut<bevy_ecs::HotPatchChanges>,
              mut patched: MessageWriter<bevy_ecs::HotPatched>| {
            if flag.swap(false, Ordering::AcqRel) {
                changes.set_changed();
                patched.write(bevy_ecs::HotPatched);
                println!("PATCHPROBE {} patch-applied", now_ms());
            }
        },
    );
}

#[cfg(not(feature = "hotpatching"))]
fn setup_hotpatch(_app: &mut App) {}

fn main() {
    let seconds: u64 = std::env::var("PATCH_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let mut app = App::new();
    app.init_resource::<Game>();
    app.init_resource::<patch_game::LibGame>();
    app.add_systems(Update, (game_system, patch_game::lib_system));
    setup_hotpatch(&mut app);
    #[cfg(feature = "full")]
    println!(
        "PATCHPROBE linked full engine: {}",
        spike_engine::heavy::exercise(0)
    );
    println!("PATCHPROBE {} start pid={}", now_ms(), std::process::id());
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        app.update();
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("PATCHPROBE {} exit", now_ms());
}
