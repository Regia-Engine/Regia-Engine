// SPDX-License-Identifier: MIT-0
//! Pause/step/resume time accounting for the motion-matching course, driven over the real agent
//! protocol (`book/agents/mm-course-pause-step-contract.md`).
//!
//! One scripted agent pauses the headless sample; while paused, request-serving host updates must
//! not advance course time, distance, the hero's transform, matcher/player state or the evaluated
//! pose (`CourseLog.frames` counts those host updates, `CourseLog.simulation_frames` does not).
//! `play.step` with a fixed N must reproduce, bit for bit, a fresh app advanced by the same N
//! simulation steps from the same course prefix, and `play.resume` must continue exactly, with no
//! catch-up jump. A second scripted agent pauses one frame before a course-leg boundary and
//! changes the tag filter while paused: the setting is readable at once, but its effect on
//! playback may only appear at the next permitted simulation evaluation. No golden images; no
//! wall-clock assertions.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mm_locomotion::{Course, CourseLog, headless_app, hero};
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_animation::Pose;
use regia_animation::components::Animator;
use regia_animation::motion_matching::{
    MatchStep, MotionMatchPlayer, MotionMatcher, MotionMatching, SearchFilter,
};
use regia_scene::Transform;
use serde_json::{Value, json};

/// Hang guard only; nothing asserts on wall time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
/// `play.step` frames in the exact-step comparison.
const STEP_FRAMES: u32 = 8;
/// Frames counted after `play.resume` before the no-catch-up snapshot.
const RESUME_FRAMES: u32 = 4;

/// The tag whose frames cover a playing clip (the procedural set's tag families, spec 52 § 2.5).
fn family_tag(clip: &str) -> &'static str {
    match clip {
        "run" => "run",
        "walk" | "turn_left" | "turn_right" => "walk",
        "stop" => "stop",
        _ => "idle",
    }
}

/// Every clip a tag forbids: a forbidden tag excludes its whole clip family from the search.
fn family_clips(tag: &str) -> &[&str] {
    match tag {
        "walk" => &["walk", "turn_left", "turn_right"],
        "run" => &["run"],
        "stop" => &["stop"],
        _ => &["idle"],
    }
}

/// Everything the contract checks, recorded from the world: the agent protocol cannot read the
/// sample's own resources, so the test thread (which owns the app) records them.
#[derive(Clone, Debug)]
struct Snapshot {
    /// `Course.elapsed`, seconds (simulation time; frozen while paused).
    course_elapsed: f32,
    /// The course leg at `course_elapsed`.
    leg: String,
    /// The course log: host updates, simulation frames, distance, clips.
    log: CourseLog,
    /// The hero's transform.
    transform: Transform,
    /// The runtime matcher: playback, counters, filter, ground track.
    matcher: MotionMatcher,
    /// The matcher's last step result (what the player consumed this frame).
    last_step: Option<MatchStep>,
    /// The player's output pose.
    player_pose: Pose,
    /// Whether the player's jump blend is running.
    player_blending: bool,
    /// Jumps the player has blended.
    player_jumps: u64,
    /// The latest blend duration, seconds (bits), when blending.
    last_blend_bits: Option<u32>,
    /// The animator's evaluated pose.
    evaluated_pose: Pose,
    /// `Course.elapsed` after one more simulation update of the app under test (the driver fills
    /// it from that app's fixed clock; `None` on the fresh app and before a step was measured).
    next_course_elapsed: Option<f32>,
}

fn snap(app: &mut regia_app::App) -> Snapshot {
    let world = app.world_mut();
    let course = world.resource::<Course>();
    let leg = course.leg().name.to_string();
    let course_elapsed = course.elapsed;
    let log = world.resource::<CourseLog>().clone();
    let entity = hero(world).expect("the hero");
    let transform = *world.get::<Transform>(entity).expect("transform");
    let matching = world.get::<MotionMatching>(entity).expect("matcher");
    let matcher = matching.matcher.clone();
    let last_step = matching.last_step;
    let player_pose = world
        .get::<MotionMatchPlayer>(entity)
        .expect("player")
        .pose()
        .clone();
    let player = world.get::<MotionMatchPlayer>(entity).expect("player");
    let player_blending = player.is_blending();
    let player_jumps = player.jumps_seen();
    let last_blend_bits = player.last_blend().map(|(duration, _method)| duration.to_bits());
    let evaluated_pose = world
        .get::<Animator>(entity)
        .expect("animator")
        .evaluated_pose()
        .clone();
    Snapshot {
        course_elapsed,
        leg,
        log,
        transform,
        matcher,
        last_step,
        player_pose,
        player_blending,
        player_jumps,
        last_blend_bits,
        evaluated_pose,
        next_course_elapsed: None,
    }
}

/// `Course.elapsed` after `updates` more simulation updates of `frame` each, computed with the
/// additions the app performs: the course clock runs in `FixedUpdate`, so each update adds one
/// fixed step per sub-step the fixed accumulator releases (two `1/60` steps for a `1/30` frame at
/// the default rate, with the banked remainder carried), not one frame delta. A clone of the app's
/// `FixedTime` replays the accumulator, so the remainder banked so far counts.
fn course_after_updates(app: &regia_app::App, frame: Duration, updates: u32) -> f32 {
    let world = app.world();
    let mut fixed = world.resource::<regia_app::FixedTime>().clone();
    let mut elapsed = world.resource::<Course>().elapsed;
    for _ in 0..updates {
        let steps = fixed.accumulate(frame);
        for _ in 0..steps {
            // `drive_course` adds `Time::delta_secs()`, which is the fixed step in `FixedUpdate`.
            elapsed += fixed.step().as_secs_f32();
        }
    }
    elapsed
}

fn pose_bits(pose: &Pose) -> Vec<u32> {
    pose.translations
        .iter()
        .flat_map(|v| v.to_array())
        .chain(pose.rotations.iter().flat_map(|q| q.to_array()))
        .chain(pose.scales.iter().flat_map(|v| v.to_array()))
        .map(f32::to_bits)
        .collect()
}

fn transform_bits(transform: &Transform) -> Vec<u32> {
    transform
        .translation
        .to_array()
        .into_iter()
        .chain(transform.rotation.to_array())
        .chain(transform.scale.to_array())
        .map(f32::to_bits)
        .collect()
}

/// The full simulation-state equality the pause/step contract promises. `CourseLog.frames` (host
/// updates) is deliberately excluded: it may grow while paused, everything else may not. The
/// player and evaluated poses are compared bit for bit in every state, a running jump blend
/// included: the blending exemption this check used to carry is gone because an active blend's
/// decay no longer depends on the pause pattern before the jump (`tests/blend_pause_pattern.rs`
/// pins that, measured on x86_64 at every jump a blend overlaps in 1500 steps).
fn assert_same_simulation(a: &Snapshot, b: &Snapshot, ctx: &str) {
    assert_eq!(
        a.course_elapsed.to_bits(),
        b.course_elapsed.to_bits(),
        "{ctx}: course elapsed"
    );
    assert_eq!(a.leg, b.leg, "{ctx}: course leg");
    assert_eq!(
        a.log.simulation_frames, b.log.simulation_frames,
        "{ctx}: simulation frames"
    );
    assert_eq!(
        a.log.distance.to_bits(),
        b.log.distance.to_bits(),
        "{ctx}: distance"
    );
    assert_eq!(
        transform_bits(&a.transform),
        transform_bits(&b.transform),
        "{ctx}: transform"
    );
    assert_eq!(a.matcher, b.matcher, "{ctx}: matcher state");
    // `last_step` is rewritten by every host update, including zero-delta ones (which record a
    // no-search step); the player has already consumed the real step, so only its playback is
    // simulation-relevant across apps.
    assert_eq!(
        a.last_step.map(|step| step.playback),
        b.last_step.map(|step| step.playback),
        "{ctx}: last step playback"
    );
    assert_eq!(a.player_blending, b.player_blending, "{ctx}: player blending");
    assert_eq!(a.player_jumps, b.player_jumps, "{ctx}: player jumps");
    assert_eq!(a.last_blend_bits, b.last_blend_bits, "{ctx}: last blend duration");
    assert_eq!(
        pose_bits(&a.player_pose),
        pose_bits(&b.player_pose),
        "{ctx}: player pose"
    );
    assert_eq!(
        pose_bits(&a.evaluated_pose),
        pose_bits(&b.evaluated_pose),
        "{ctx}: evaluated pose"
    );
}

/// Equality of everything a paused filter change must not move: time, distance, transforms,
/// poses and the matcher's *evaluation* state (counters and playback). The filter field, the
/// search clocks and the ground track are excluded: the filter changes by intent, the clocks
/// advance only when a step is permitted.
fn assert_same_evaluation(a: &Snapshot, b: &Snapshot, ctx: &str) {
    assert_eq!(
        a.course_elapsed.to_bits(),
        b.course_elapsed.to_bits(),
        "{ctx}: course elapsed"
    );
    assert_eq!(a.leg, b.leg, "{ctx}: course leg");
    assert_eq!(
        a.log.simulation_frames, b.log.simulation_frames,
        "{ctx}: simulation frames"
    );
    assert_eq!(
        a.log.distance.to_bits(),
        b.log.distance.to_bits(),
        "{ctx}: distance"
    );
    assert_eq!(
        transform_bits(&a.transform),
        transform_bits(&b.transform),
        "{ctx}: transform"
    );
    assert_eq!(
        a.matcher.searches(),
        b.matcher.searches(),
        "{ctx}: search counter"
    );
    assert_eq!(a.matcher.jumps(), b.matcher.jumps(), "{ctx}: jump counter");
    assert_eq!(a.matcher.playback(), b.matcher.playback(), "{ctx}: playback");
    assert_eq!(
        pose_bits(&a.player_pose),
        pose_bits(&b.player_pose),
        "{ctx}: player pose"
    );
    assert_eq!(
        pose_bits(&a.evaluated_pose),
        pose_bits(&b.evaluated_pose),
        "{ctx}: evaluated pose"
    );
}

/// Script → driver commands.
enum Cmd {
    /// Snapshot the app now; answer with [`Msg::Snap`].
    Snap(u64),
    /// Build (once) the fresh comparison app and advance it to `1` total simulation frames;
    /// answer with its snapshot.
    FreshTo(u64, u64),
    /// Advance the fresh app by `n` updates; answer with its snapshot.
    FreshAdvance(u64, u64),
    /// On the fresh app, forbid `1` (a tag name) exactly as `anim.motion_match_set` does (a
    /// runtime filter write, no search request).
    FreshForbidTag(String),
    /// Compute the `play.step` count that lands exactly one update before the next course-leg
    /// boundary; answer with `(id, steps, elapsed_bits_at_that_point, elapsed_bits_one_update_later)`.
    PlanBoundary(u64),
    /// Watch for the app reaching `1` total simulation frames (the resume target). The script
    /// sends this while the clock is still paused, before `play.resume`: the simulation frame
    /// counter stays frozen while paused, so the driver holds the target before the first resumed
    /// frame runs, however late the script thread is scheduled. Sent after the resume call
    /// returned, the command raced the resumed frames: a driver that ran past the target before
    /// draining it snapshotted one or more frames late.
    ArmResume(u64),
}

/// Driver → script answers. Snapshots are boxed: the enum rides channels, and `Snapshot` is
/// far larger than the other variants.
enum Msg {
    Snap(u64, Box<Snapshot>),
    Fresh(u64, Box<Snapshot>),
    Boundary(u64, u32, u32, u32),
    /// `(app snapshot, fresh snapshot)` after [`RESUME_FRAMES`] full-rate frames post-resume.
    ResumeCheck(Box<Snapshot>, Box<Snapshot>),
}

fn msg_name(msg: &Msg) -> &'static str {
    match msg {
        Msg::Snap(..) => "snap",
        Msg::Fresh(..) => "fresh",
        Msg::Boundary(..) => "boundary",
        Msg::ResumeCheck(..) => "resume_check",
    }
}

fn send(msg_tx: &Sender<Msg>, msg: Msg) {
    let _ = msg_tx.send(msg);
}

/// The steps of `play.step` that land exactly one update before the next leg boundary, computed
/// by replaying the course clock with the same `f32` additions the app performs
/// ([`course_after_updates`]). Answers `(steps, elapsed bits there, elapsed bits one update
/// later)`: the second is where one more permitted step lands, across the boundary.
fn plan_boundary(app: &regia_app::App, one_step: Option<Duration>) -> Result<(u32, u32, u32), String> {
    let Some(frame) = one_step else {
        return Err("the per-update step was never measured".into());
    };
    let course = app.world().resource::<Course>();
    let start_leg = course.leg().name;
    let leg_at = |t: f32| {
        let probe = Course {
            legs: course.legs.clone(),
            elapsed: t,
            heading: course.heading,
        };
        probe.leg().name
    };
    let mut steps = 0u32;
    let mut elapsed = course.elapsed;
    let mut next = course_after_updates(app, frame, 1);
    while leg_at(next) == start_leg {
        steps += 1;
        if steps > 1_000 {
            return Err("no leg boundary within 1000 steps".into());
        }
        elapsed = next;
        next = course_after_updates(app, frame, steps + 1);
    }
    Ok((steps, elapsed.to_bits(), next.to_bits()))
}

/// Build the fresh comparison app with the same plugin set as the app under test. The agent
/// plugin matters: this ECS stores resources as entities, so the hero's entity index (and the
/// matcher's `stagger_slot = index mod 8`, spec 52 § 2.8) depends on which plugins are installed.
/// Without a client the agent systems are inert, so the comparison stays simulation-pure.
fn fresh_app(project: &Path) -> regia_app::App {
    let mut app = headless_app();
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    })
    .add_plugins(regia_agent::AgentPlugin);
    app.finish_plugins();
    app
}

/// The script thread's connections, handed to the driver.
struct Links {
    cmd_rx: Receiver<Cmd>,
    msg_tx: Sender<Msg>,
    done: Arc<AtomicBool>,
    script: JoinHandle<()>,
}

/// The main-thread side: run the app, answer the script's commands, and watch for the resume
/// edge. Assertions live in the script (or the test body after the join); only internal
/// impossibilities are recorded here.
fn drive(
    app: &mut regia_app::App,
    fresh_project: &Path,
    links: &Links,
    failures: &mut Vec<String>,
    trace: &mut Vec<String>,
) {
    let Links {
        cmd_rx,
        msg_tx,
        done,
        script,
    } = links;
    let mut was_paused = false;
    // The simulation delta of one unpaused update (`Time::delta`; this sample sets no `SimStep`).
    let mut one_step: Option<Duration> = None;
    let mut fresh: Option<regia_app::App> = None;
    // Once set, the app is expected to resume and then run unpaused until the course log reaches
    // this simulation frame count (the no-catch-up anchor). The flag is whether the clock has run
    // since the target was armed: it is armed while paused, before the script resumes.
    let mut resume_target: Option<(u64, bool)> = None;
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Cmd::Snap(id) => {
                    let mut snapshot = snap(app);
                    snapshot.next_course_elapsed = one_step.map(|frame| course_after_updates(app, frame, 1));
                    send(msg_tx, Msg::Snap(id, Box::new(snapshot)));
                }
                Cmd::FreshTo(id, frames) => {
                    let f = fresh.get_or_insert_with(|| fresh_app(fresh_project));
                    let have = f.world().resource::<CourseLog>().simulation_frames;
                    for _ in have..frames {
                        f.update();
                    }
                    send(msg_tx, Msg::Fresh(id, Box::new(snap(f))));
                }
                Cmd::FreshAdvance(id, n) => match fresh.as_mut() {
                    Some(f) => {
                        for _ in 0..n {
                            f.update();
                        }
                        send(msg_tx, Msg::Fresh(id, Box::new(snap(f))));
                    }
                    None => failures.push("FreshAdvance before FreshTo".into()),
                },
                Cmd::FreshForbidTag(tag) => match fresh.as_mut() {
                    Some(f) => {
                        let world = f.world_mut();
                        let entity = hero(world).expect("fresh hero");
                        let mut matching = world.get_mut::<MotionMatching>(entity).expect("fresh matcher");
                        let mask = matching
                            .database
                            .tag_table()
                            .mask(&[tag.as_str()])
                            .expect("a known tag");
                        matching.matcher.filter = SearchFilter {
                            require: 0,
                            forbid: mask,
                        };
                    }
                    None => failures.push("FreshForbidTag before FreshTo".into()),
                },
                Cmd::PlanBoundary(id) => match plan_boundary(app, one_step) {
                    Ok((steps, elapsed_bits, one_bits)) => {
                        send(msg_tx, Msg::Boundary(id, steps, elapsed_bits, one_bits));
                    }
                    Err(error) => failures.push(format!("PlanBoundary: {error}")),
                },
                Cmd::ArmResume(target) => resume_target = Some((target, false)),
            }
        }
        app.update();
        let paused = app.world().resource::<regia_app::Time>().paused;
        if paused != was_paused {
            trace.push(format!(
                "host update {}: paused {was_paused}->{paused} at {} simulation frames",
                app.world().resource::<CourseLog>().frames,
                app.world().resource::<CourseLog>().simulation_frames,
            ));
        }
        if !paused && one_step.is_none() {
            one_step = Some(app.world().resource::<regia_app::Time>().delta());
        }
        // The resume anchor: the target is armed while the clock is still paused, so the driver
        // holds it through the paused updates before the release and checks the counter after
        // every update from the release on. The simulation frame counter stays frozen while
        // paused, so reaching the target means exactly the requested number of full-rate frames
        // ran since the checkpoint; the release frame itself runs a zero gameplay delta (the pause
        // releases in PreUpdate, after First took the delta) and does not advance the counter.
        // A clock that pauses again after it ran, before the target, is a failure.
        if let Some((target, ran)) = resume_target {
            if paused {
                if ran {
                    resume_target = None;
                    failures.push("the clock paused before the resume target".into());
                }
            } else if app.world().resource::<CourseLog>().simulation_frames >= target {
                resume_target = None;
                let after = snap(app);
                match fresh.as_mut() {
                    Some(f) => {
                        for _ in 0..RESUME_FRAMES {
                            f.update();
                        }
                        let fresh_snap = snap(f);
                        send(msg_tx, Msg::ResumeCheck(Box::new(after), Box::new(fresh_snap)));
                    }
                    None => failures.push("resume check without a fresh app".into()),
                }
            } else if !ran {
                resume_target = Some((target, true));
            }
        }
        was_paused = paused;
    }
}

/// One agent call. The protocol budgets mutating calls (`F0007`); this script can outrun it on a
/// fast host, so it waits and retries. Nothing asserts on time.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    for _ in 0..100 {
        match client.call(method, params.clone()) {
            Ok(value) => return value,
            Err(err) if format!("{err:?}").contains("F0007") => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => panic!("{method} failed: {err:?}"),
        }
    }
    panic!("{method}: still rate limited after 100 tries");
}

fn connect(agent_dir: &Path) -> Client {
    for _ in 0..3000 {
        if let Ok((advertised, token)) = endpoint::read(agent_dir)
            && let Ok(connected) = Client::connect(&advertised.url, &token)
        {
            return connected;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no endpoint at {}", agent_dir.display());
}

/// The hero entity once the database registry lists it.
fn wait_for_hero(client: &mut Client) -> u64 {
    for _ in 0..500 {
        let list = call(client, "anim.motion_match_databases", json!({}));
        if let Some(entity) = list["databases"][0]["entities"][0].as_u64() {
            return entity;
        }
    }
    panic!("the database registry never listed the hero");
}

fn wait_snap(rx: &Receiver<Msg>, id: u64, what: &str) -> Snapshot {
    let msg = rx
        .recv_timeout(SCRIPT_TIMEOUT)
        .unwrap_or_else(|err| panic!("no reply for {what}: {err}"));
    match msg {
        Msg::Snap(got, snapshot) if got == id => *snapshot,
        other => panic!("expected snapshot {id} for {what}, got {}", msg_name(&other)),
    }
}

fn wait_fresh(rx: &Receiver<Msg>, id: u64, what: &str) -> Snapshot {
    let msg = rx
        .recv_timeout(SCRIPT_TIMEOUT)
        .unwrap_or_else(|err| panic!("no reply for {what}: {err}"));
    match msg {
        Msg::Fresh(got, snapshot) if got == id => *snapshot,
        other => panic!(
            "expected fresh snapshot {id} for {what}, got {}",
            msg_name(&other)
        ),
    }
}

fn wait_boundary(rx: &Receiver<Msg>, id: u64) -> (u32, u32, u32) {
    let msg = rx
        .recv_timeout(SCRIPT_TIMEOUT)
        .unwrap_or_else(|err| panic!("no boundary plan: {err}"));
    match msg {
        Msg::Boundary(got, steps, elapsed_bits, crossed_bits) if got == id => {
            (steps, elapsed_bits, crossed_bits)
        }
        other => panic!("expected boundary {id}, got {}", msg_name(&other)),
    }
}

fn wait_resume_check(rx: &Receiver<Msg>) -> (Snapshot, Snapshot) {
    let msg = rx
        .recv_timeout(SCRIPT_TIMEOUT)
        .unwrap_or_else(|err| panic!("no resume check: {err}"));
    match msg {
        Msg::ResumeCheck(after, fresh) => (*after, *fresh),
        other => panic!("expected a resume check, got {}", msg_name(&other)),
    }
}

/// Spawn the agent script thread and drive the app until the script sets `done`.
fn run_script(
    name: &str,
    script: impl FnOnce(Client, Sender<Cmd>, Receiver<Msg>, Arc<AtomicBool>) + Send + 'static,
) {
    let project = regia_test_utils::temp::unique_temp_dir(name);
    let fresh_project = regia_test_utils::temp::unique_temp_dir(&format!("{name}-fresh"));
    let mut app = headless_app();
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);
    let agent_dir = endpoint::agent_dir(&project);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (msg_tx, msg_rx) = channel::<Msg>();
    let script_dir = agent_dir.clone();
    let script = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let client = connect(&script_dir);
            script(client, cmd_tx, msg_rx, flag);
        })
        .expect("script thread");

    let mut failures: Vec<String> = Vec::new();
    let mut trace: Vec<String> = Vec::new();
    app.finish_plugins();
    let links = Links {
        cmd_rx,
        msg_tx,
        done: done.clone(),
        script,
    };
    drive(&mut app, &fresh_project, &links, &mut failures, &mut trace);
    if let Err(payload) = links.script.join() {
        panic!("the script panicked: {payload:?}\ndriver trace: {trace:?}\ndriver failures: {failures:?}");
    }
    assert!(
        done.load(Ordering::Relaxed),
        "the agent script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    assert!(failures.is_empty(), "driver failures: {failures:?}");
    let _ = std::fs::remove_dir_all(&project);
    let _ = std::fs::remove_dir_all(&fresh_project);
}

/// The driver's course-clock model is the app's clock, bit for bit, from any frame. The boundary
/// test predicts course time from wherever the pause landed, which depends on how fast the agent
/// connected (host load); a model that adds one frame delta per update instead of the fixed
/// sub-steps matched only when the plan was zero steps (the v047 gate: pause at simulation frame 3,
/// 26 planned steps, `0.96666706` predicted, `0.9666664` run). No agent and no threads here, so
/// every start frame is checked, not just the one a run happens to pause at.
#[test]
fn the_course_clock_model_matches_the_app_from_every_start_frame() {
    const STARTS: u32 = 40;
    const AHEAD: [u32; 3] = [1, 2, 26];
    // `ran[k]`: course time after `k + 1` updates of an identical app.
    let mut probe = headless_app();
    probe.finish_plugins();
    let ran: Vec<f32> = (0..STARTS + AHEAD[2])
        .map(|_| {
            probe.update();
            probe.world().resource::<Course>().elapsed
        })
        .collect();
    let mut app = headless_app();
    app.finish_plugins();
    app.update();
    let frame = app.world().resource::<regia_app::Time>().delta();
    assert!(!frame.is_zero(), "the first update ran simulation time");
    for start in 1..STARTS {
        for ahead in AHEAD {
            let predicted = course_after_updates(&app, frame, ahead);
            let ran = ran[(start + ahead - 1) as usize];
            assert_eq!(
                ran.to_bits(),
                predicted.to_bits(),
                "from simulation frame {start}, {ahead} updates: predicted {predicted}, ran {ran}"
            );
        }
        app.update();
    }
}

#[test]
fn pause_step_resume_matches_a_fresh_run() {
    run_script("mm-course-pause-step", |mut client, cmd_tx, msg_rx, done| {
        let hero = wait_for_hero(&mut client);
        // Pause through the protocol; every host update from here is zero-delta until stepped.
        let paused = call(&mut client, "play.pause", json!({}));
        assert_eq!(paused["paused"], true, "{paused}");

        cmd_tx.send(Cmd::Snap(0)).expect("driver alive");
        let s0 = wait_snap(&msg_rx, 0, "the checkpoint");

        // Service several request-serving host updates while paused.
        for _ in 0..3 {
            let read = call(&mut client, "anim.motion_match", json!({ "entity": hero }));
            assert!(read["playback"].is_object(), "{read}");
        }
        cmd_tx.send(Cmd::Snap(1)).expect("driver alive");
        let s0b = wait_snap(&msg_rx, 1, "paused reads");

        // The host-update counter grew; nothing simulation-driven moved.
        assert!(
            s0b.log.frames > s0.log.frames,
            "host updates ran while paused: {} -> {}",
            s0.log.frames,
            s0b.log.frames
        );
        assert_eq!(
            s0b.log.simulation_frames, s0.log.simulation_frames,
            "paused host updates are not simulation frames"
        );
        assert_same_simulation(&s0, &s0b, "paused host updates");

        // The checkpoint prefix must equal a fresh app advanced to the same simulation count.
        cmd_tx
            .send(Cmd::FreshTo(2, s0.log.simulation_frames))
            .expect("driver alive");
        let sb0 = wait_fresh(&msg_rx, 2, "the checkpoint prefix");
        assert_same_simulation(&s0, &sb0, "checkpoint prefix");

        // play.step with a fixed N must equal N fresh simulation steps from that prefix.
        let stepped = call(&mut client, "play.step", json!({ "frames": STEP_FRAMES }));
        assert_eq!(stepped["paused"], true, "step re-pauses: {stepped}");
        cmd_tx.send(Cmd::Snap(3)).expect("driver alive");
        let s1 = wait_snap(&msg_rx, 3, "the stepped state");
        assert_eq!(
            s1.log.simulation_frames - s0.log.simulation_frames,
            u64::from(STEP_FRAMES),
            "exactly {STEP_FRAMES} simulation frames ran"
        );
        cmd_tx
            .send(Cmd::FreshAdvance(4, u64::from(STEP_FRAMES)))
            .expect("driver alive");
        let sb1 = wait_fresh(&msg_rx, 4, "the fresh stepped state");
        assert_same_simulation(&s1, &sb1, "play.step equals fresh steps");

        // Resume: the next frames continue exactly, with no catch-up jump. The target is armed
        // first, while the clock is still paused, so the driver holds it before the first resumed
        // frame however late this thread runs after the resume call returns.
        cmd_tx
            .send(Cmd::ArmResume(
                s1.log.simulation_frames + u64::from(RESUME_FRAMES),
            ))
            .expect("driver alive");
        let resumed = call(&mut client, "play.resume", json!({}));
        assert_eq!(resumed["paused"], false, "{resumed}");
        let (s2, sb2) = wait_resume_check(&msg_rx);
        assert_eq!(
            s2.log.simulation_frames,
            s1.log.simulation_frames + u64::from(RESUME_FRAMES),
            "resume ran exactly {RESUME_FRAMES} simulation frames, no catch-up"
        );
        assert_same_simulation(&s2, &sb2, "resume continues exactly");

        done.store(true, Ordering::Relaxed);
    });
}

#[test]
fn a_paused_boundary_holds_and_a_filter_waits_for_a_stepped_evaluation() {
    run_script("mm-course-boundary-filter", |mut client, cmd_tx, msg_rx, done| {
        let hero = wait_for_hero(&mut client);
        call(&mut client, "play.pause", json!({}));
        cmd_tx.send(Cmd::Snap(0)).expect("driver alive");
        let s0 = wait_snap(&msg_rx, 0, "the early checkpoint");

        // Land exactly one update before the next course-leg boundary, from whichever leg the
        // pause caught (the app free-runs while the agent connects, so the leg is not fixed).
        // The plan may be zero steps: the pause can already sit one frame before a boundary.
        cmd_tx.send(Cmd::PlanBoundary(1)).expect("driver alive");
        let (steps, elapsed_bits, crossed_bits) = wait_boundary(&msg_rx, 1);
        if steps > 0 {
            let stepped = call(&mut client, "play.step", json!({ "frames": steps }));
            assert_eq!(stepped["paused"], true, "{stepped}");
        }
        cmd_tx.send(Cmd::Snap(2)).expect("driver alive");
        let s1 = wait_snap(&msg_rx, 2, "one frame before the boundary");
        assert_eq!(s1.leg, s0.leg, "the course holds on the prior leg");
        assert_eq!(
            s1.course_elapsed.to_bits(),
            elapsed_bits,
            "the course clock advanced exactly {steps} steps"
        );
        assert_eq!(
            s1.log.simulation_frames - s0.log.simulation_frames,
            u64::from(steps),
            "exactly {steps} simulation frames ran"
        );

        // Host updates at the boundary must not cross it.
        for _ in 0..3 {
            call(&mut client, "anim.motion_match", json!({ "entity": hero }));
        }
        cmd_tx.send(Cmd::Snap(3)).expect("driver alive");
        let s2 = wait_snap(&msg_rx, 3, "boundary reads");
        assert!(
            s2.log.frames > s1.log.frames,
            "host updates ran at the boundary: {} -> {}",
            s1.log.frames,
            s2.log.frames
        );
        assert_eq!(
            s2.log.simulation_frames, s1.log.simulation_frames,
            "the boundary is not crossed by host updates"
        );
        assert_same_simulation(&s1, &s2, "the boundary holds while paused");

        // One permitted step crosses it, by exactly one frame of course time.
        call(&mut client, "play.step", json!({ "frames": 1 }));
        cmd_tx.send(Cmd::Snap(4)).expect("driver alive");
        let s3 = wait_snap(&msg_rx, 4, "the crossed boundary");
        assert_ne!(s3.leg, s0.leg, "one permitted step crossed the boundary");
        assert_eq!(
            s3.course_elapsed.to_bits(),
            crossed_bits,
            "the boundary crossing advanced exactly one step"
        );

        // Step until a search has just run: from that anchor the search clock is exactly known
        // (since_search resets to zero on a search), so the interval reasoning below is exact.
        // Whatever clip plays defines the tag family to forbid.
        let read = call(&mut client, "anim.motion_match", json!({ "entity": hero }));
        let mut searches = read["searches"].as_u64().unwrap_or(u64::MAX);
        let mut clip = String::new();
        for _ in 0..24 {
            call(&mut client, "play.step", json!({ "frames": 1 }));
            let read = call(&mut client, "anim.motion_match", json!({ "entity": hero }));
            let now = read["searches"].as_u64().unwrap_or(u64::MAX);
            if now != searches {
                clip = read["playback"]["clip_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                searches = now;
                break;
            }
        }
        assert!(
            !clip.is_empty(),
            "a search ran within the interval window: {searches} searches"
        );
        let forbidden = family_tag(&clip);
        cmd_tx.send(Cmd::Snap(5)).expect("driver alive");
        let s4 = wait_snap(&msg_rx, 5, "the playing clip");

        // Change the filter while paused, without asking for a search.
        let set = call(
            &mut client,
            "anim.motion_match_set",
            json!({ "entity": hero, "filter": { "forbid": [forbidden] } }),
        );
        assert_eq!(set["forbid"], json!([forbidden]), "{set}");
        let read = call(&mut client, "anim.motion_match", json!({ "entity": hero }));
        assert_eq!(read["tags"]["forbid"], json!([forbidden]), "{read}");
        cmd_tx.send(Cmd::Snap(6)).expect("driver alive");
        let s5 = wait_snap(&msg_rx, 6, "the paused filter change");
        assert_ne!(s5.matcher.filter, s4.matcher.filter, "the filter setting changed");
        assert_eq!(
            s5.matcher.searches(),
            s4.matcher.searches(),
            "a paused filter change runs no search"
        );
        assert_same_evaluation(&s4, &s5, "the paused filter change");

        // Host updates while paused: the filter is still not evaluated.
        for _ in 0..3 {
            call(&mut client, "anim.motion_match", json!({ "entity": hero }));
        }
        cmd_tx.send(Cmd::Snap(7)).expect("driver alive");
        let s6 = wait_snap(&msg_rx, 7, "paused filter reads");
        assert!(
            s6.log.frames > s5.log.frames,
            "host updates ran after the filter change: {} -> {}",
            s5.log.frames,
            s6.log.frames
        );
        assert_eq!(
            s6.log.simulation_frames, s5.log.simulation_frames,
            "no simulation time accumulated while paused"
        );
        assert_same_evaluation(&s5, &s6, "no evaluation while paused");

        // One step is inside the 0.1 s search interval: no evaluation, while the course itself
        // advances by exactly that one step (time and playback time move, the clip and the
        // search counters do not).
        call(&mut client, "play.step", json!({ "frames": 1 }));
        cmd_tx.send(Cmd::Snap(8)).expect("driver alive");
        let s7 = wait_snap(&msg_rx, 8, "one step inside the interval");
        assert_eq!(
            s7.matcher.searches(),
            s6.matcher.searches(),
            "one 1/30 s step does not reach the 0.1 s search interval"
        );
        assert_eq!(
            s7.matcher.playback().map(|p| p.clip),
            s6.matcher.playback().map(|p| p.clip),
            "no jump inside the interval"
        );
        let stepped = s6
            .next_course_elapsed
            .expect("the driver measured the update step before the pause");
        assert_eq!(
            s7.course_elapsed.to_bits(),
            stepped.to_bits(),
            "the step advanced the course clock by exactly one step"
        );
        assert_eq!(
            s7.log.simulation_frames,
            s6.log.simulation_frames + 1,
            "exactly one simulation frame ran"
        );

        // Three more steps pass the interval: the filter is evaluated at the search. The search's
        // best frame must leave the forbidden family; playback itself may hold the well-matching
        // continuation per the jump rule (spec 52 § 2.8), which the fresh-app comparison below
        // pins down exactly.
        call(&mut client, "play.step", json!({ "frames": 3 }));
        let read = call(&mut client, "anim.motion_match", json!({ "entity": hero }));
        assert!(
            read["searches"].as_u64().unwrap_or(0) > s7.matcher.searches(),
            "the search interval passed: {read}"
        );
        let best = read["last_search"]["best"]["clip_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            !family_clips(forbidden).contains(&best.as_str()),
            "the filter shaped the search result: best {best}"
        );
        cmd_tx.send(Cmd::Snap(9)).expect("driver alive");
        let s8 = wait_snap(&msg_rx, 9, "the evaluated filter");

        // A fresh app with the same prefix, the same filter write and the same four steps
        // must end in exactly the same state. The intermediate check after one step isolates
        // the first diverging frame if the run ever breaks the contract.
        cmd_tx
            .send(Cmd::FreshTo(10, s4.log.simulation_frames))
            .expect("driver alive");
        let sc4 = wait_fresh(&msg_rx, 10, "the fresh prefix");
        assert_same_simulation(&s4, &sc4, "the fresh prefix");
        cmd_tx
            .send(Cmd::FreshForbidTag(forbidden.to_string()))
            .expect("driver alive");
        cmd_tx.send(Cmd::FreshAdvance(11, 1)).expect("driver alive");
        let sc7 = wait_fresh(&msg_rx, 11, "the fresh one-step state");
        assert_same_simulation(&s7, &sc7, "the fresh one-step state");
        cmd_tx.send(Cmd::FreshAdvance(12, 3)).expect("driver alive");
        let sc8 = wait_fresh(&msg_rx, 12, "the fresh filter evaluation");
        assert_same_simulation(&s8, &sc8, "paused filter equals fresh evaluation");

        // Leave the app running again; the exact resume contract is the other test's.
        call(&mut client, "play.resume", json!({}));
        done.store(true, Ordering::Relaxed);
    });
}
