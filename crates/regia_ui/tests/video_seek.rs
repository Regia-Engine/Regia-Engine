// SPDX-License-Identifier: MPL-2.0
//! Seek slider correctness (audit fix-ui3-correctness-k, finding 2): while a drag (or an
//! unapplied queued seek) is in flight, the transport system must leave the slider at the value
//! the widget set; copying the stale playback position back in `PreUpdate` used to reset the
//! slider before the next drag delta, so two 0.4 drags queued a 0.4 seek instead of 0.8.
//! Headless: no window, no GPU, a mock `VideoFrameSource`, real pointer input through the
//! interaction systems, fixed 100 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_render::{
    DynamicTexture, VideoControl, VideoControlError, VideoFrameInfo, VideoFrameSource, VideoFrames,
    VideoPlayback,
};
use regia_ui::prelude::*;
use regia_ui::video::{UiVideo, UiVideoAction, UiVideoControl, UiVideoPlugin};

const PLAYER: u64 = 1;
const DURATION: f64 = 10.0;

#[derive(Default)]
struct MockState {
    position: f64,
    seeks: Vec<f64>,
}

#[derive(Default)]
struct MockSource {
    state: Mutex<MockState>,
}

impl VideoFrameSource for MockSource {
    fn frame(&self, player: u64) -> Option<VideoFrameInfo> {
        if player != PLAYER {
            return None;
        }
        let s = self.state.lock().unwrap();
        Some(VideoFrameInfo {
            width: 1,
            height: 1,
            aspect: 1.0,
            serial: 1,
            pts_ns: Some(0),
            fallback: false,
            playback: VideoPlayback::Playing,
            position: s.position,
            duration: Some(DURATION),
            looping: false,
            audio_clock: false,
        })
    }

    fn texture(&self, _player: u64) -> Option<DynamicTexture> {
        None
    }

    fn rgba(&self, _player: u64) -> Option<(u64, u32, u32, Arc<[u8]>)> {
        None
    }

    fn control(&self, _player: u64, control: VideoControl) -> Result<(), VideoControlError> {
        if let VideoControl::Seek(t) = control {
            self.state.lock().unwrap().seeks.push(t);
        }
        Ok(())
    }

    fn players(&self) -> Vec<u64> {
        vec![PLAYER]
    }
}

fn app() -> (App, Arc<MockSource>) {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_plugins(UiVideoPlugin);
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    let mock = Arc::new(MockSource::default());
    app.insert_resource(VideoFrames(Some(mock.clone())));
    (app, mock)
}

fn seek_slider(app: &mut App) -> Entity {
    let video = app.world_mut().spawn(UiVideo::new(PLAYER)).id();
    app.world_mut()
        .spawn((
            Interactable::default(),
            ComputedNode {
                rect: [0.0, 0.0, 100.0, 20.0],
                clip_rect: [0.0, 0.0, 100.0, 20.0],
                scale: 1.0,
                ..Default::default()
            },
            Slider {
                value: 0.0,
                min: 0.0,
                max: 1.0,
                step: 0.0,
            },
            UiVideoControl {
                video,
                action: UiVideoAction::SeekSlider,
            },
        ))
        .id()
}

fn move_to(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
}

fn left(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
}

fn value(app: &App, slider: Entity) -> f32 {
    app.world().get::<Slider>(slider).unwrap().value
}

/// Presses on the slider and drags by two 40 px steps on the 100 px track.
fn grab_and_drag_twice(app: &mut App) -> Entity {
    let slider = seek_slider(app);
    move_to(app, 50.0, 10.0);
    left(app, ButtonState::Pressed);
    move_to(app, 90.0, 10.0);
    move_to(app, 130.0, 10.0);
    slider
}

#[test]
fn drag_seeks_queue_the_dragged_value_not_the_stale_position() {
    let (mut app, mock) = app();
    let slider = grab_and_drag_twice(&mut app);
    // The two 40 px deltas sum to 0.8 of the track; the source keeps reporting position 0
    // (the seek lands later), and the release at (130, 10) is outside the track: no click.
    assert!(
        (value(&app, slider) - 0.8).abs() < 1e-5,
        "the slider keeps the value the widget set: {}",
        value(&app, slider)
    );
    app.update();
    let seeks = mock.state.lock().unwrap().seeks.clone();
    assert_eq!(seeks.len(), 2, "each drag queues one seek: {seeks:?}");
    for (seek, want) in seeks.iter().zip([0.4 * DURATION, 0.8 * DURATION]) {
        assert!(
            (seek - want).abs() < 1e-3,
            "each seek lands at its dragged time: {seeks:?}"
        );
    }
}

#[test]
fn playback_sync_resumes_once_the_queued_seek_is_applied() {
    let (mut app, mock) = app();
    let slider = seek_slider(&mut app);
    move_to(&mut app, 50.0, 10.0);
    left(&mut app, ButtonState::Pressed);
    move_to(&mut app, 90.0, 10.0);
    // One drag of 40 px: the delta math adds 0.4 to the widget-driven value (0, the stale
    // playback position).
    assert!(
        (value(&app, slider) - 0.4).abs() < 1e-5,
        "{}",
        value(&app, slider)
    );
    // Releasing over the track is a click that jumps to the release point (0.9).
    left(&mut app, ButtonState::Released);
    assert!(
        (value(&app, slider) - 0.9).abs() < 1e-5,
        "{}",
        value(&app, slider)
    );
    // The source now reports the applied seek target; syncing resumes.
    mock.state.lock().unwrap().position = 0.9 * DURATION;
    app.update();
    assert!(
        (value(&app, slider) - 0.9).abs() < 1e-5,
        "the slider follows the playback position again: {}",
        value(&app, slider)
    );
    mock.state.lock().unwrap().position = 0.7 * DURATION;
    app.update();
    assert!(
        (value(&app, slider) - 0.7).abs() < 1e-5,
        "sync keeps working on later frames: {}",
        value(&app, slider)
    );
}
