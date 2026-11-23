// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/fast_quit.rs, commit c9513623f), ADR-8012.
// Portions copied from Direct Order 27 (shell-regia/game/src/single_instance.rs, commit c9513623f), ADR-8012.
//! Process guards (ADR-8833, absorb plan P-07): the fast exit and the single-instance lock.
//!
//! - `fast_quit_hides_within_one_frame`: a fast exit requested in frame N hides the window in
//!   frame N (the hide hooks run inside the request), nothing is presented with the window
//!   visible after the request, and the flush is due at the end of frame N, so the runner never
//!   starts frame N+1.
//! - `fast_quit_ends_the_process_without_dropping_the_world` (child process): the flush runs its
//!   jobs in parallel, a hung job is cut at the budget, kept files are synced, and the process
//!   ends with the request's code without dropping the world.
//! - `second_instance_refused` (child process): a second process for the same lock is refused
//!   with [`EXIT_ALREADY_RUNNING`] and told the holder's pid; after the holder releases, a start
//!   succeeds.
//! - `stale_lock_from_a_dead_pid_is_taken_over` (child process): an instance that ended without
//!   releasing (a crash) never blocks the next start, which reports the dead pid.
//!
//! No wall clock: budgets only bound jobs that block on a channel, and outcomes are read from
//! the report, not timed.
//!
//! `cargo test -p regia_platform --test process_guards`

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use regia_app::prelude::*;
use regia_platform::{
    EXIT_ALREADY_RUNNING, ExitHooks, ExitMode, FastExitRequest, FastExitSettings, SingleInstanceError,
    SingleInstanceLock, SurfacesHidden, fast_exit_due, keep_file_on_fast_exit, poll_fast_exit,
    request_fast_exit, run_fast_exit_flush,
};
use regia_test_utils::temp::unique_temp_dir;

const CHILD_MODE: &str = "REGIA_PROCESS_GUARDS_CHILD";
const CHILD_PATH: &str = "REGIA_PROCESS_GUARDS_PATH";

/// Runs [`child_entry`] alone in a child process with `mode` and `path`.
fn run_child(mode: &str, path: &Path) -> Output {
    let exe = std::env::current_exe().expect("test executable");
    Command::new(exe)
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(CHILD_MODE, mode)
        .env(CHILD_PATH, path)
        .output()
        .expect("run the child")
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The child half of the process tests. In the parent run it does nothing.
#[test]
fn child_entry() {
    let (Ok(mode), Some(path)) = (std::env::var(CHILD_MODE), std::env::var_os(CHILD_PATH)) else {
        return;
    };
    let path = PathBuf::from(path);
    match mode.as_str() {
        "try_lock" => match SingleInstanceLock::acquire_at(&path, Duration::ZERO) {
            Ok(lock) => {
                println!("child: lock taken");
                drop(lock);
                std::process::exit(0)
            }
            Err(err @ SingleInstanceError::AlreadyRunning { .. }) => {
                println!("child: refused: {err}");
                std::process::exit(EXIT_ALREADY_RUNNING)
            }
            Err(err) => {
                println!("child: lock error: {err}");
                std::process::exit(2)
            }
        },
        "lock_and_die" => {
            let lock =
                SingleInstanceLock::acquire_at(&path, Duration::ZERO).expect("child takes the free lock");
            println!("child: holding as pid {}", std::process::id());
            // Ends without the release (no Drop): what a crash or a kill leaves behind.
            std::mem::forget(lock);
            std::process::exit(9)
        }
        "fast_exit" => child_fast_exit(&path),
        other => panic!("unknown child mode {other}"),
    }
}

type Frames = Arc<Mutex<Vec<u64>>>;

fn frames(list: &Frames) -> Vec<u64> {
    list.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// A window the host owns (an editor panel, an XR mirror): the fast exit's hide hook hides it.
#[derive(Resource)]
struct HostWindow {
    visible: bool,
}

const REQUEST_FRAME: u64 = 3;

/// In-process tests arm no hard deadline: it would end the test binary.
fn no_deadline() -> FastExitSettings {
    FastExitSettings {
        hard_deadline: None,
        ..FastExitSettings::default()
    }
}

#[test]
fn fast_quit_hides_within_one_frame() {
    let hidden_in: Frames = Arc::default();
    let presented_visible: Frames = Arc::default();
    let mut app = App::new();
    app.insert_resource(HostWindow { visible: true })
        .insert_resource(SurfacesHidden(false))
        .insert_resource(no_deadline());
    let mut hooks = ExitHooks::default();
    let hide_log = hidden_in.clone();
    hooks.on_fast_hide("host window", move |world| {
        world.resource_mut::<HostWindow>().visible = false;
        let frame = world.resource::<Time>().frame_count();
        hide_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(frame);
    });
    app.insert_resource(hooks);
    // The game's quit key, pressed in frame REQUEST_FRAME.
    app.add_systems(Update, |world: &mut World| {
        if world.resource::<Time>().frame_count() == REQUEST_FRAME {
            request_fast_exit(world, 7, "quit key", 0);
        }
    });
    // A stand-in for the present at the end of the frame.
    let present_log = presented_visible.clone();
    app.add_systems(
        Last,
        move |window: Res<HostWindow>, hidden: Res<SurfacesHidden>, time: Res<Time>| {
            if window.visible && !hidden.0 {
                present_log
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(time.frame_count());
            }
        },
    );

    // The runner's loop: one update, then the fast-exit check, as `poll_fast_exit` does.
    let mut last_frame = 0;
    for _ in 0..10 {
        app.update();
        last_frame = app.world().resource::<Time>().frame_count();
        if fast_exit_due(&mut app) {
            break;
        }
    }

    let request = app
        .world()
        .get_resource::<FastExitRequest>()
        .cloned()
        .expect("the request is recorded");
    assert_eq!(request.requested_frame, REQUEST_FRAME);
    assert_eq!(request.code, 7);
    assert_eq!(request.hide_hooks, 1);
    assert_eq!(
        frames(&hidden_in),
        [REQUEST_FRAME],
        "hidden inside the request frame"
    );
    assert!(
        frames(&presented_visible).iter().all(|f| *f < REQUEST_FRAME),
        "nothing presented with the window visible from the request on: {:?}",
        frames(&presented_visible)
    );
    assert_eq!(
        frames(&presented_visible).len() as u64,
        REQUEST_FRAME - 1,
        "every frame before the request presented"
    );
    assert!(
        app.world().resource::<SurfacesHidden>().0,
        "the renderer stops presenting"
    );
    assert_eq!(
        last_frame, REQUEST_FRAME,
        "the flush is due at the end of the request frame"
    );

    // The flush itself (a custom host calls it; the runner then ends the process).
    let report = run_fast_exit_flush(app.world_mut()).expect("a report");
    assert_eq!(report.request, request);
    assert!(
        report.flush.is_empty(),
        "no flush job registered: {:?}",
        report.flush
    );
}

#[test]
fn an_agent_quit_waits_its_delay_frames() {
    let mut app = App::new();
    app.insert_resource(no_deadline());
    app.update();
    let request = request_fast_exit(app.world_mut(), 0, "agent quit.request", 1);
    assert_eq!(request.flush_frame, request.requested_frame + 1);
    assert!(!fast_exit_due(&mut app), "not in the request frame");
    app.update();
    assert!(fast_exit_due(&mut app), "due one frame later");
    // A second request keeps the first.
    let again = request_fast_exit(app.world_mut(), 3, "second", 0);
    assert_eq!(again, request);
}

#[test]
fn app_exit_in_fast_mode_becomes_a_fast_exit() {
    let mut app = App::new();
    app.insert_resource(ExitMode::Fast).insert_resource(no_deadline());
    app.add_systems(Update, |world: &mut World| {
        if world.resource::<Time>().frame_count() == 2 {
            world.write_message(AppExit::Error(std::num::NonZeroU8::new(4).expect("non-zero")));
        }
    });
    app.update();
    assert!(!fast_exit_due(&mut app));
    app.update();
    assert!(
        fast_exit_due(&mut app),
        "the AppExit is a fast exit in ExitMode::Fast"
    );
    let request = app.world().resource::<FastExitRequest>();
    assert_eq!((request.code, request.requested_frame), (4, 2));

    // The default mode leaves AppExit to the clean exit.
    let mut clean = App::new();
    clean.add_systems(Update, |world: &mut World| {
        world.write_message(AppExit::Success);
    });
    clean.update();
    assert!(!fast_exit_due(&mut clean));
    assert!(clean.should_exit().is_some());
}

/// Dropped only if the world is dropped: the fast exit must never get there.
#[derive(Resource)]
struct WorldDropMarker(PathBuf);

impl Drop for WorldDropMarker {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("world-dropped"), b"1");
    }
}

fn child_fast_exit(dir: &Path) -> ! {
    let mut app = App::new();
    app.insert_resource(WorldDropMarker(dir.to_path_buf()))
        .insert_resource(FastExitSettings {
            flush_budget: Duration::from_millis(200),
            ..FastExitSettings::default()
        });
    let kept = dir.join("settings.ron");
    std::fs::write(&kept, b"()").expect("write the kept file");
    keep_file_on_fast_exit(&kept);
    let mut hooks = ExitHooks::default();
    let save = dir.join("save.bin");
    hooks.add_flush("save", move |_| {
        let save = save.clone();
        Some(Box::new(move || {
            let _ = std::fs::write(&save, b"saved");
        }))
    });
    hooks.add_flush("hung", |_| {
        Some(Box::new(|| {
            // A join or driver call that never returns: blocks on a channel nobody sends to.
            let (_tx, rx) = std::sync::mpsc::channel::<()>();
            let _keep = _tx;
            let _ = rx.recv();
        }))
    });
    hooks.add_flush("nothing", |_| None);
    app.insert_resource(hooks);
    app.update();
    request_fast_exit(app.world_mut(), 7, "child test", 0);
    poll_fast_exit(&mut app);
    panic!("poll_fast_exit returned on its flush frame");
}

#[test]
fn fast_quit_ends_the_process_without_dropping_the_world() {
    let dir = unique_temp_dir("fast-exit");
    let out = run_child("fast_exit", &dir);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(7), "the request's code: {all}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("regia fast exit: code=7"), "{all}");
    assert!(stdout.contains("save:ok:"), "{all}");
    assert!(
        stdout.contains("hung:timeout:200"),
        "a hung job is cut at the budget: {all}"
    );
    assert!(stdout.contains("fsync:ok:"), "{all}");
    assert!(
        !stdout.contains("nothing:"),
        "a hook that returns no job adds none: {all}"
    );
    assert!(dir.join("save.bin").is_file(), "the flush job wrote");
    assert!(
        !dir.join("world-dropped").exists(),
        "the world was dropped on the fast path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_instance_refused() {
    let dir = unique_temp_dir("single-instance");
    let path = dir.join("game.instance.lock");
    let first = SingleInstanceLock::acquire_at(&path, Duration::ZERO).expect("first instance");
    assert!(first.is_held());
    assert_eq!(first.took_over_from(), None);

    let refused = run_child("try_lock", &path);
    let all = text(&refused);
    assert_eq!(refused.status.code(), Some(EXIT_ALREADY_RUNNING), "{all}");
    assert!(
        String::from_utf8_lossy(&refused.stdout).contains(&format!("held by pid {}", std::process::id())),
        "the refusal names the holder: {all}"
    );
    // In one process a second open of the lock is refused too.
    match SingleInstanceLock::acquire_at(&path, Duration::ZERO) {
        Err(err) => assert_eq!(err.code(), "already_running"),
        Ok(_) => panic!("a second lock in the same process was granted"),
    }

    drop(first);
    let taken = run_child("try_lock", &path);
    assert_eq!(taken.status.code(), Some(0), "released: {}", text(&taken));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_lock_from_a_dead_pid_is_taken_over() {
    let dir = unique_temp_dir("single-instance-stale");
    let path = dir.join("game.instance.lock");
    let died = run_child("lock_and_die", &path);
    let all = text(&died);
    assert_eq!(died.status.code(), Some(9), "{all}");
    let stdout = String::from_utf8_lossy(&died.stdout);
    let pid: u32 = stdout
        .lines()
        .find_map(|l| l.split_once("child: holding as pid ").map(|(_, pid)| pid))
        // libtest prints `test child_entry ... ` on the same line first.
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("child pid missing: {all}"));

    let lock = SingleInstanceLock::acquire_at(&path, Duration::ZERO)
        .expect("the dead instance's lock is taken over");
    assert_eq!(lock.took_over_from(), Some(pid));
    assert_eq!(lock.state_name(), "locked");
    drop(lock);
    // A clean release leaves no record: the next start takes nothing over.
    let again = SingleInstanceLock::acquire_at(&path, Duration::ZERO).expect("free again");
    assert_eq!(again.took_over_from(), None);
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}
