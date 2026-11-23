// SPDX-License-Identifier: MPL-2.0
//! Replay acceptance (`docs/06-testing-and-ci.md` § 6, lane gameplay-coroutines): a recording made
//! from timestamped raw input carries the timestamps, and replaying it is frame-identical — the
//! stamps are analysis data and never change what a replay does.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::input_recording::{
    InputRecorder, InputRecording, InputRecordingPlugin, InputRecordingSet, RecorderMode,
};
use regia_input::prelude::*;
use regia_platform::{ButtonState, InputTimestamp, InputTiming, InputTimingPlugin, KeyboardInput};

const FRAMES: u64 = 90;

/// One trace entry per fixed step: (frame, height bits, jumps started).
type Trace = Vec<(u64, u32, u32)>;

/// A tiny deterministic "game": Jump raises the height, gravity pulls it back. One trace entry
/// per fixed step: (frame, height bits, jumps started).
#[derive(Resource, Default)]
struct Sim {
    height: f32,
    holding: bool,
    jumps: u32,
    trace: Trace,
}

fn simulate(time: Res<Time>, actions: Res<Actions>, mut sim: ResMut<Sim>) {
    let jump = actions.value("Jump").x > 0.5;
    if jump && !sim.holding {
        sim.jumps += 1;
    }
    sim.holding = jump;
    sim.height = if jump { sim.height + 0.25 } else { sim.height * 0.8 };
    let entry = (time.frame_count(), sim.height.to_bits(), sim.jumps);
    sim.trace.push(entry);
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((
        InputTimingPlugin,
        InputPlugin::new(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .binding(Binding::new("Jump", InputSource::key("Space"))),
        ),
        InputRecordingPlugin,
    ))
    .init_resource::<Sim>()
    .add_systems(FixedUpdate, simulate.after(InputRecordingSet));
    enable_fixed_step(&mut app, 60.0);
    app
}

fn key(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "Space".into(),
        logical_key: " ".into(),
        state,
        repeat: false,
        timestamp: InputTimestamp::now(),
    });
}

/// Plays the game from real (timestamped) key events while recording.
fn record() -> (InputRecording, Trace) {
    let mut app = app();
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .record_with(11, vec!["Jump".into()]);
    for frame in 0..FRAMES {
        match frame % 12 {
            0 => key(&mut app, ButtonState::Pressed),
            5 => key(&mut app, ButtonState::Released),
            _ => {}
        }
        // Spread the events over wall-clock time like a real session.
        std::thread::sleep(Duration::from_micros(200));
        app.update();
        if frame % 12 == 0 {
            let timing = app.world().resource::<InputTiming>();
            assert_eq!(timing.events.len(), 1, "the frame saw its key event's stamp");
            assert!(timing.max_latency().is_some());
        }
    }
    app.world_mut().resource_mut::<InputRecorder>().stop();
    let recording = app.world().resource::<InputRecorder>().recording.clone();
    let trace = std::mem::take(&mut app.world_mut().resource_mut::<Sim>().trace);
    (recording, trace)
}

/// Replays `recording` (no raw input at all) and returns the trace and each step's stamps.
fn replay(recording: &InputRecording) -> (Trace, Vec<Vec<u64>>) {
    let mut app = app();
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .play(recording.clone());
    let mut stamps = Vec::new();
    for _ in 0..FRAMES {
        app.update();
        stamps.push(
            app.world()
                .resource::<InputRecorder>()
                .playback_timestamps()
                .to_vec(),
        );
    }
    // Each recorded step plays on the same frame it was recorded on (ADR-537, ADR-867), so the replay is
    // over after as many frames as were recorded.
    assert_eq!(app.world().resource::<InputRecorder>().mode, RecorderMode::Idle);
    let trace = std::mem::take(&mut app.world_mut().resource_mut::<Sim>().trace);
    (trace, stamps)
}

#[test]
fn timestamped_recording_replays_frame_identically() {
    let (recording, recorded) = record();
    assert_eq!(recording.len(), FRAMES as usize);
    assert_eq!(
        recording.timestamps.len(),
        recording.frames.len(),
        "one stamp list per step"
    );

    // Every key event is in the recording, on its frame's step, in increasing time order.
    let stamped: Vec<(usize, u64)> = recording
        .timestamps
        .iter()
        .enumerate()
        .flat_map(|(step, stamps)| stamps.iter().map(move |t| (step, *t)))
        .collect();
    let expected_steps: Vec<usize> = (0..FRAMES as usize)
        .filter(|f| f % 12 == 0 || f % 12 == 5)
        .collect();
    assert_eq!(
        stamped.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        expected_steps
    );
    assert!(
        stamped.windows(2).all(|w| w[0].1 < w[1].1),
        "stamps increase: {stamped:?}"
    );
    assert!(
        recorded.last().is_some_and(|(_, _, jumps)| *jumps == 8),
        "{:?}",
        recorded.last()
    );

    // Two replays are frame-identical, step by step.
    let (first, stamps) = replay(&recording);
    let (second, _) = replay(&recording);
    assert_eq!(first, second, "replays are frame-identical");

    // The timestamps do not steer playback: the same recording without them, or with every stamp
    // shifted, replays to the same frames.
    let mut stripped = recording.clone();
    stripped.timestamps.clear();
    assert_eq!(replay(&stripped).0, first, "timestamps never change the replay");
    let mut shifted = recording.clone();
    for stamps in &mut shifted.timestamps {
        for stamp in stamps {
            *stamp += 1_000_000;
        }
    }
    assert_eq!(replay(&shifted).0, first);

    // The replay reproduces the recorded game on the same steps and frames: same frame numbers,
    // heights and jump counts (no playback latency, ADR-537, ADR-867).
    assert_eq!(first.len(), recorded.len());
    assert_eq!(
        first, recorded,
        "the replay reproduces every recorded step bit for bit, on the same frame"
    );

    // During playback each step exposes its recorded stamps.
    let exposed: Vec<u64> = stamps.iter().flatten().copied().collect();
    assert_eq!(exposed, stamped.iter().map(|(_, t)| *t).collect::<Vec<_>>());
}
