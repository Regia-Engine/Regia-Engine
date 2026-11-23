// SPDX-License-Identifier: MIT-0
//! `hello_window` (M1, `docs/specs/11-platform-and-input.md` § 8): clears the window to a colour that
//! cycles with elapsed time and logs every input and window event.

use regia::prelude::*;

/// The sample's plugin.
pub struct HelloWindow;

/// Exit after this many frames (automated smoke runs).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ExitAfterFrames(pub u64);

impl Plugin for HelloWindow {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, keep_screen_on).add_systems(
            Update,
            (
                animate_clear_color,
                log_input,
                log_window_events,
                exit_after_frames,
                buzz_on_touch,
            ),
        );
    }
}

/// Seconds for one full trip around the colour wheel.
pub const CYCLE_SECONDS: f32 = 6.0;

/// The clear colour at `t` seconds: a hue cycle at moderate saturation and value, linear RGBA.
pub fn clear_color_at(t: f32) -> [f64; 4] {
    let hue = (t / CYCLE_SECONDS).rem_euclid(1.0) * 6.0;
    let (s, v) = (0.6f32, 0.8f32);
    let c = v * s;
    let x = c * (1.0 - ((hue % 2.0) - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [f64::from(r + m), f64::from(g + m), f64::from(b + m), 1.0]
}

fn animate_clear_color(time: Res<Time>, mut clear: ResMut<ClearColor>) {
    clear.0 = clear_color_at(time.elapsed_secs());
}

#[allow(clippy::too_many_arguments)]
fn log_input(
    mut keys: MessageReader<KeyboardInput>,
    mut buttons: MessageReader<MouseButtonInput>,
    mut cursor: MessageReader<CursorMoved>,
    mut motion: MessageReader<MouseMotion>,
    mut wheel: MessageReader<MouseWheel>,
    mut touch: MessageReader<TouchInput>,
    mut ime: MessageReader<ImeEvent>,
    mut back: MessageReader<BackPressed>,
) {
    for e in keys.read() {
        info!(
            "KeyboardInput {} ({}) {:?}{}",
            e.physical_key,
            e.logical_key,
            e.state,
            if e.repeat { " repeat" } else { "" }
        );
    }
    for e in buttons.read() {
        info!("MouseButtonInput {:?} {:?}", e.button, e.state);
    }
    for e in cursor.read() {
        info!("CursorMoved {:.1},{:.1}", e.position.0, e.position.1);
    }
    for e in motion.read() {
        info!("MouseMotion {:.1},{:.1}", e.delta.0, e.delta.1);
    }
    for e in wheel.read() {
        info!("MouseWheel {:.2},{:.2} {:?}", e.delta.0, e.delta.1, e.unit);
    }
    for e in touch.read() {
        info!(
            "TouchInput #{} {:?} {:.1},{:.1}",
            e.id, e.phase, e.position.0, e.position.1
        );
    }
    for e in ime.read() {
        info!("ImeEvent {e:?}");
    }
    for _ in back.read() {
        info!("BackPressed");
    }
}

fn log_window_events(
    mut resized: MessageReader<WindowResized>,
    mut scale: MessageReader<WindowScaleFactorChanged>,
    mut focus: MessageReader<WindowFocused>,
    mut close: MessageReader<WindowCloseRequested>,
    mut dropped: MessageReader<FileDropped>,
) {
    for e in resized.read() {
        info!("WindowResized {}x{}", e.width, e.height);
    }
    for e in scale.read() {
        info!("WindowScaleFactorChanged {:.2}", e.scale_factor);
    }
    for e in focus.read() {
        info!("WindowFocused {}", e.focused);
    }
    for _ in close.read() {
        info!("WindowCloseRequested");
    }
    for e in dropped.read() {
        info!("FileDropped {}", e.path.display());
    }
}

/// Keep the display awake while the sample runs (exercises `KeepScreenOn` on Android).
fn keep_screen_on(mut keep: ResMut<KeepScreenOn>) {
    keep.0 = true;
}

/// A short vibration on every new touch (exercises `Haptics` on Android; nothing on desktop).
fn buzz_on_touch(mut touch: MessageReader<TouchInput>, haptics: Res<Haptics>) {
    for e in touch.read() {
        if e.phase == TouchPhase::Started {
            match haptics.vibrate(std::time::Duration::from_millis(25), 0.6) {
                Ok(()) => info!("Haptics vibrate 25 ms"),
                Err(err) => warn!("Haptics vibrate failed: {err}"),
            }
        }
    }
}

fn exit_after_frames(limit: Option<Res<ExitAfterFrames>>, time: Res<Time>, mut exit: MessageWriter<AppExit>) {
    if let Some(limit) = limit
        && time.frame_count() >= limit.0
    {
        info!("hello_window: {} frames done, exiting", time.frame_count());
        exit.write(AppExit::Success);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn colour_cycles_with_time() {
        let a = clear_color_at(0.0);
        let b = clear_color_at(CYCLE_SECONDS / 3.0);
        assert_ne!(a, b, "color changes over time");
        let (p, q) = (clear_color_at(1.25), clear_color_at(1.25 + CYCLE_SECONDS));
        assert!(
            p.iter().zip(q).all(|(a, b)| (a - b).abs() < 1e-5),
            "periodic: {p:?} vs {q:?}"
        );
        for i in 0..600 {
            let c = clear_color_at(i as f32 * 0.01);
            assert!(
                c.iter().all(|v| (0.0..=1.0).contains(v)),
                "in range at {i}: {c:?}"
            );
            assert_eq!(c[3], 1.0);
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut v) = self.0.lock() {
                v.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_every_input_message() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        // Systems run on worker threads, so the capture must be the global subscriber (only this test
        // in this binary sets one).
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            panic!("another global tracing subscriber is already installed");
        }
        {
            let mut app = App::new();
            app.add_plugins(regia::platform::PlatformPlugin)
                .init_resource::<ClearColor>()
                .add_plugins(HelloWindow);
            let window = app.world_mut().spawn(Window::default()).id();
            let world = app.world_mut();
            world.write_message(KeyboardInput {
                window,
                physical_key: "KeyW".into(),
                logical_key: "w".into(),
                state: ButtonState::Pressed,
                repeat: false,
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(MouseButtonInput {
                window,
                button: MouseButton::Left,
                state: ButtonState::Released,
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(CursorMoved {
                window,
                position: (10.0, 20.0),
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(MouseMotion {
                delta: (1.0, -1.0),
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(MouseWheel {
                window,
                delta: (0.0, 1.0),
                unit: regia::platform::WheelUnit::Line,
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(TouchInput {
                window,
                id: 7,
                phase: TouchPhase::Started,
                position: (1.0, 2.0),
                timestamp: regia::platform::InputTimestamp::ZERO,
            });
            world.write_message(ImeEvent::Commit(window, "é".into()));
            world.write_message(BackPressed);
            world.write_message(WindowResized {
                window,
                width: 800,
                height: 600,
            });
            app.update();
        }
        let text = capture
            .0
            .lock()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default();
        for needle in [
            "KeyboardInput KeyW (w) Pressed",
            "MouseButtonInput Left Released",
            "CursorMoved 10.0,20.0",
            "MouseMotion 1.0,-1.0",
            "MouseWheel 0.00,1.00 Line",
            "TouchInput #7 Started",
            "ImeEvent Commit",
            "BackPressed",
            "WindowResized 800x600",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in log:\n{text}");
        }
    }
}
