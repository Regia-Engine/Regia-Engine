// SPDX-License-Identifier: MIT-0
//! `input_lab` (M7, `docs/specs/11-platform-and-input.md` § 8): exercises the full action-mapping
//! pipeline — keyboard, mouse, gamepads, touch, modifiers, triggers, contexts and rebinding — with
//! device input simulated through the platform message layer.
//!
//! Running: `cargo run -p input_lab`
//!
//! `input_lab --list-gamepads [SECONDS]` instead polls the real gamepad backend (gilrs) for SECONDS
//! (default 10): it prints each pad as it is found, every button press and release and every stick,
//! trigger or d-pad axis that crosses half travel, then a summary line. On a Steam Deck run it over SSH
//! and press the built-in controls meanwhile (the Deck pass in `book/user-guide/steam-deck.md`).
//!
//! The binary logs every action state change every few simulated frames; injection at the action
//! level and the rebinding flow are exercised in `src/main.rs` below too. This sample does **not**
//! open a window; it uses the headless test harness so it works identically on every platform and in
//! CI without a display server.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_input::prelude::*;
use regia_input::{
    Actions, GestureEvent, InputInjection, InputRebinder, Mouse, OVERRIDES_PATH, Pad, Touches,
};
use regia_platform::{
    ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged, GamepadConnected,
    KeyboardInput, MouseButtonInput, MouseMotion, MouseWheel, TouchInput, TouchPhase, WheelUnit,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--list-gamepads") {
        match parse_seconds(args.get(1).map(String::as_str)) {
            Ok(seconds) => list_gamepads(seconds),
            Err(err) => {
                eprintln!("input_lab: {err}");
                std::process::exit(2);
            }
        }
        return;
    }
    println!("=== regia_input lab (M7) ===");
    println!();

    keyboard_mouse_demo();
    gamepad_demo();
    touch_demo();
    modifiers_triggers_demo();
    context_priority_demo();
    injection_demo();
    rebinding_demo();

    println!();
    println!("all demos passed.");
}

/// Keyboard and mouse: Move (WASD, scaled) + Jump (Space) + Fire (LMB).
fn keyboard_mouse_demo() {
    println!("-- keyboard / mouse --");

    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Look", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .action("Fire", ActionKind::Button)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(
            Binding::new("Look", InputSource::MouseDelta)
                .with_modifier(Modifier::Scale(Vec3::new(0.1, 0.1, 0.0))),
        )
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new(
            "Fire",
            InputSource::mouse_button(regia_platform::MouseButton::Left),
        ));

    let mut app = headless_app(config);

    // Press W (forward).
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "KeyW".into(),
        logical_key: "KeyW".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        let moved = actions.axis2d("Move");
        assert!((moved.y - 1.0).abs() < 0.01, "W → forward ({moved:?})");
        assert!(actions.pressed("Move"));
        println!(
            "  W held  → Move  = ({:.2}, {:.2})  pressed: {}",
            moved.x,
            moved.y,
            actions.pressed("Move")
        );
    }

    // Mouse delta (look).
    app.world_mut().write_message(MouseMotion {
        delta: (100.0, -50.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        let look = actions.axis2d("Look");
        assert!(
            (look.x - 10.0).abs() < 0.01 && (look.y + 5.0).abs() < 0.01,
            "{look:?}"
        );
        println!(
            "  mouse Δ  → Look  = ({:.2}, {:.2})  pressed: {}",
            look.x,
            look.y,
            actions.pressed("Look")
        );
    }

    // Mouse button.
    app.world_mut().write_message(MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: regia_platform::MouseButton::Left,
        state: ButtonState::Pressed,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Fire") && actions.just_pressed("Fire"));
        println!(
            "  LMB down → Fire  = pressed  just_pressed: {}",
            actions.just_pressed("Fire")
        );
    }

    // Wheel normalisation.
    app.world_mut().write_message(MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (0.0, 3.0),
        unit: WheelUnit::Line,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.world_mut().write_message(MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (-80.0, 0.0),
        unit: WheelUnit::Pixel,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    {
        let mouse = app.world().resource::<Mouse>();
        assert!((mouse.wheel().x + 2.0).abs() < 0.01 && (mouse.wheel().y - 3.0).abs() < 0.01);
        println!(
            "  wheel   → wheel = ({:.2}, {:.2})",
            mouse.wheel().x,
            mouse.wheel().y
        );
    }

    println!();
}

/// Two gamepads, stick dead zone, analog trigger, button edge.
fn gamepad_demo() {
    println!("-- gamepads (2 pads) --");

    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .action("Jump", ActionKind::Button)
        .binding(
            Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                .with_modifier(DeadZone::radial(0.15, 0.95)),
        )
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(GamepadButton::South),
        ));
    let mut app = headless_app(config);

    // Connect two pads.
    for (id, name) in [(1, "Pad A"), (2, "Pad B")] {
        app.world_mut().write_message(GamepadConnected {
            device: id,
            name: name.into(),
        });
    }
    app.update();
    {
        let gamepads = app.world().resource::<Gamepads>();
        assert_eq!(gamepads.connected(), 2);
        println!("  connected {}", gamepads.connected());
    }

    // Pad 1 Left Stick with dead zone.
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::LeftStickX,
        value: 0.8,
    });
    app.world_mut().write_message(GamepadAxisChanged {
        device: 1,
        axis: GamepadAxis::LeftStickY,
        value: 0.4,
    });
    // Pad 1 South button.
    app.world_mut().write_message(GamepadButtonChanged {
        device: 1,
        button: GamepadButton::South,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        let moved = actions.axis2d("Move");
        let expected: Vec2 = DeadZone::radial(0.15, 0.95)
            .apply(Vec3::new(0.8, 0.4, 0.0))
            .truncate();
        assert!(
            (moved.distance(expected)).abs() < 0.02,
            "dead-zone output {moved:?} ≈ {expected:?}"
        );
        println!(
            "  stick({:.1},{:.1}) + deadzone → Move = ({:.3}, {:.3})",
            0.8, 0.4, moved.x, moved.y
        );
        assert!(actions.pressed("Jump") && actions.just_pressed("Jump"));
        println!("  South  → Jump = pressed");
    }

    // Pad 2: just verify its presence in the resource.
    {
        let gamepads = app.world().resource::<Gamepads>();
        assert!(gamepads.name(2).is_some());
    }

    println!();
}

/// Touch lifecycle and gesture: tap, long press, swipe.
fn touch_demo() {
    println!("-- touch --");

    let config = InputConfig::new()
        .action("TapAction", ActionKind::Button)
        .binding(Binding::new("TapAction", InputSource::key("Space"))); // dummy config
    let mut app = headless_app(config);

    // Start a touch and end it quickly → tap.
    app.world_mut().write_message(TouchInput {
        window: Entity::PLACEHOLDER,
        id: 1,
        phase: TouchPhase::Started,
        position: (100.0, 200.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    app.world_mut().write_message(TouchInput {
        window: Entity::PLACEHOLDER,
        id: 1,
        phase: TouchPhase::Ended,
        position: (102.0, 203.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();

    let gestures: Vec<_> = app
        .world_mut()
        .resource_mut::<Messages<GestureEvent>>()
        .drain()
        .collect();
    let tapped = gestures.iter().any(|g| matches!(g, GestureEvent::Tap { .. }));
    assert!(tapped, "quick touch → Tap gesture (gestures: {gestures:?})");
    println!("  touch down+up quickly → GestureEvent::Tap");

    // Touch count reflects active fingers.
    let touches = app.world().resource::<Touches>();
    assert_eq!(touches.count(), 0, "finger removed");
    println!("  active touches: {}", touches.count());

    println!();
}

/// Modifiers (dead zone, response curve, smooth) and triggers (Pressed, Hold, Pulse).
fn modifiers_triggers_demo() {
    println!("-- modifiers & triggers --");

    let config = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .action("PulseFire", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("Space")).with_trigger(Trigger::Hold(0.3)))
        .binding(Binding::new("PulseFire", InputSource::key("Space")).with_trigger(Trigger::Pulse(0.1)));
    let mut app = headless_app(config);

    // Press Space; Hold(0.3) fires 18 frames later at 60 Hz.
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "Space".into(),
        logical_key: "Space".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();

    // Pulse fires immediately.
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.triggered("PulseFire"), "Pulse fires on the first frame");
        println!(
            "  Pulse(0.1s) → fires on frame 1: triggered={}",
            actions.triggered("PulseFire")
        );
        assert_eq!(actions.phase("Charge"), ActionPhase::Started);
        println!("  Hold(0.3s)  → frame 1 phase: {:?}", actions.phase("Charge"));
    }

    // Advance to frame 19 (hold threshold ≈ 0.3s = 18 frames of elapsed time).
    // We already did 1 frame, so 18 more. Elapsed entering frame 19 = 18/60 = 0.3s ≥ 0.3s.
    for _ in 0..18 {
        app.update();
    }
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.triggered("Charge"),
            "Hold fires after the threshold (elapsed {}s)",
            actions.elapsed("Charge")
        );
        println!(
            "  Hold(0.3s)  → fires frame 18: triggered={} elapsed={:.3}",
            actions.triggered("Charge"),
            actions.elapsed("Charge")
        );
    }

    // Pulse should have fired a few more times.
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.elapsed("PulseFire") > 0.0);
        println!(
            "  Pulse(0.1s) → elapsed {:.3}s (firing during the hold)",
            actions.elapsed("PulseFire")
        );
    }

    println!();
}

/// Higher-priority consuming context hides lower bindings.
fn context_priority_demo() {
    println!("-- context priority & consumption --");

    let config = InputConfig::new()
        .action("Lower", ActionKind::Button)
        .action("Upper", ActionKind::Button)
        .context(
            "Lower",
            InputContext::new(0).with_binding(Binding::new("Lower", InputSource::key("Escape"))),
        )
        .context(
            "Upper",
            InputContext::consuming(10).with_binding(Binding::new("Upper", InputSource::key("Escape"))),
        );
    let mut app = headless_app(config);

    // Both contexts enabled; Upper consumes Lower.
    app.world_mut()
        .resource_mut::<InputContexts>()
        .set(0, ["Lower", "Upper"]);
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "Escape".into(),
        logical_key: "Escape".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Upper"), "upper context fires");
        assert!(!actions.pressed("Lower"), "consume_lower hides Lower");
        println!("  Upper(consume_lower) + Lower on Escape → Upper=pressed, Lower=idle  ✓");
    }

    // Remove Upper; Lower fires again.
    app.world_mut().resource_mut::<InputContexts>().set(0, ["Lower"]);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Lower"));
    println!("  Lower alone on Escape → Lower=pressed  ✓");

    println!();
}

/// Action injection through the agent-protocol / virtual-control interface.
fn injection_demo() {
    println!("-- injection --");

    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Move", ActionKind::Axis2D);
    let mut app = headless_app(config);

    // Schedule a three-frame injection of Jump.
    let frame = app.world().resource::<Time>().frame_count();
    app.world_mut()
        .resource_mut::<InputInjection>()
        .schedule("Jump", Vec3::X, frame, 3);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Jump"));
    println!("  schedule(Jump, frame={frame}, 3 frames) → pressed");

    // Hold Move (virtual stick).
    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold("Move", Vec3::new(0.0, 1.0, 0.0));
    app.update();
    {
        let moved = app.world().resource::<Actions>().axis2d("Move");
        assert!((moved.y - 1.0).abs() < 0.01, "{moved:?}");
        println!("  hold(Move, (0,1)) → Move.y = {:.2}", moved.y);
    }

    // Release.
    app.world_mut().resource_mut::<InputInjection>().release("Move");
    app.update();
    assert!(!app.world().resource::<Actions>().pressed("Move"));
    println!("  release(Move) → Move.pressed = false");

    println!();
}

/// Rebinding: capture a key, replace the old binding, persist to a Vfs.
fn rebinding_demo() {
    use regia_platform::{DirMount, MountPoint, Vfs, VfsPath};

    println!("-- rebinding & persistence --");

    let user_dir = std::env::temp_dir().join(format!("regia-input-lab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&user_dir);
    std::fs::create_dir_all(&user_dir).expect("temp dir");
    let path = VfsPath::parse("user://input_overrides.ron").expect("valid path");

    // Session 1: rebind Jump from Space to KeyJ.
    {
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(user_dir.clone()));
        let config = InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")));
        let mut app = App::new();
        app.insert_resource(vfs.clone());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(InputPlugin::new(config));
        app.update();

        app.world_mut()
            .resource_mut::<InputRebinder>()
            .start("Gameplay", "Jump");
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "KeyJ".into(),
            logical_key: "KeyJ".into(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();

        let outcome = app.world().resource::<InputRebinder>().outcome.clone();
        assert!(outcome.is_some_and(|o| o.input == InputSource::key("KeyJ")));
        assert!(vfs.exists(&path), "overrides written to disk");
        println!("  rebind Jump → KeyJ  saved to {}", OVERRIDES_PATH);
    }

    // Session 2: restart; the override is merged on load.
    {
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(user_dir.clone()));
        let config = InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")));
        let mut app = App::new();
        app.insert_resource(vfs.clone());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(InputPlugin::new(config));

        // Space should NOT jump (it was replaced).
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "Space".into(),
            logical_key: "Space".into(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        assert!(!app.world().resource::<Actions>().pressed("Jump"));
        println!("  after restart: Space → Jump = false (original replaced)");

        // KeyJ jumps.
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "Space".into(),
            logical_key: "Space".into(),
            state: ButtonState::Released,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "KeyJ".into(),
            logical_key: "KeyJ".into(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        assert!(app.world().resource::<Actions>().pressed("Jump"));
        println!("  after restart: KeyJ → Jump = true (override persists)  ✓");
    }

    let _ = std::fs::remove_dir_all(&user_dir);
    println!("  persistence across restart: verified");
    println!();
}

fn headless_app(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app
}

/// The polling window of `--list-gamepads`, seconds (default 10, at most an hour).
fn parse_seconds(arg: Option<&str>) -> Result<u32, String> {
    match arg {
        None => Ok(10),
        Some(text) => match text.parse::<u32>() {
            Ok(seconds) if seconds <= 3600 => Ok(seconds),
            _ => Err(format!(
                "--list-gamepads takes whole seconds in 0..=3600, got {text}"
            )),
        },
    }
}

/// What `--list-gamepads` last reported for one pad: the buttons held and the axes past half travel
/// (axis and direction, -1 or 1).
#[derive(Debug, Default, Clone, PartialEq)]
struct PadSnapshot {
    pressed: BTreeSet<GamepadButton>,
    deflected: BTreeSet<(GamepadAxis, i8)>,
}

impl PadSnapshot {
    fn of(pad: &Pad) -> Self {
        let pressed = pad.buttons.keys().copied().filter(|b| pad.pressed(*b)).collect();
        let deflected = pad
            .axes
            .iter()
            .filter(|(_, v)| v.abs() >= 0.5)
            .map(|(axis, v)| (*axis, if *v < 0.0 { -1 } else { 1 }))
            .collect();
        Self { pressed, deflected }
    }

    /// The lines describing what changed from `self` to `now`, in a stable order.
    fn changes(&self, now: &Self) -> Vec<String> {
        let mut lines = Vec::new();
        for button in now.pressed.difference(&self.pressed) {
            lines.push(format!("{button:?} pressed"));
        }
        for button in self.pressed.difference(&now.pressed) {
            lines.push(format!("{button:?} released"));
        }
        for (axis, dir) in now.deflected.difference(&self.deflected) {
            lines.push(format!("{axis:?} {}", if *dir < 0 { "-1" } else { "+1" }));
        }
        lines
    }
}

/// `--list-gamepads`: polls the real backend at 60 Hz for `seconds` and prints what it sees.
fn list_gamepads(seconds: u32) {
    let mut app = App::new();
    app.add_plugins(InputPlugin::new(InputConfig::new()));
    let frames = u64::from(seconds) * 60;
    let mut seen: BTreeMap<u64, (String, PadSnapshot)> = BTreeMap::new();
    let mut buttons_seen: BTreeSet<GamepadButton> = BTreeSet::new();
    let mut axes_seen: BTreeSet<GamepadAxis> = BTreeSet::new();
    println!("input_lab: polling gamepads for {seconds} s");
    for frame in 0..=frames {
        app.update();
        let gamepads = app.world().resource::<Gamepads>();
        for (id, pad) in gamepads.pads() {
            let now = PadSnapshot::of(pad);
            let entry = seen.entry(id).or_insert_with(|| {
                println!("pad {id}: {}", pad.name);
                (pad.name.clone(), PadSnapshot::default())
            });
            for line in entry.1.changes(&now) {
                println!("pad {id} at {:.2} s: {line}", frame as f64 / 60.0);
            }
            buttons_seen.extend(now.pressed.iter().copied());
            axes_seen.extend(now.deflected.iter().map(|(axis, _)| *axis));
            entry.1 = now;
        }
        if frame < frames {
            std::thread::sleep(Duration::from_micros(16_667));
        }
    }
    let names: Vec<&str> = seen.values().map(|(name, _)| name.as_str()).collect();
    println!(
        "gamepads: {} found {names:?}; buttons pressed {buttons_seen:?}; axes moved {axes_seen:?}",
        seen.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(buttons: &[(GamepadButton, f32)], axes: &[(GamepadAxis, f32)]) -> Pad {
        let mut pad = Pad::default();
        for (button, value) in buttons {
            pad.buttons.insert(
                *button,
                regia_input::PadButton {
                    value: *value,
                    ..Default::default()
                },
            );
        }
        pad.axes.extend(axes.iter().copied());
        pad
    }

    #[test]
    fn a_snapshot_keeps_held_buttons_and_half_travel_axes() {
        let snap = PadSnapshot::of(&pad(
            &[(GamepadButton::South, 1.0), (GamepadButton::LeftTrigger, 0.3)],
            &[
                (GamepadAxis::LeftStickX, -0.8),
                (GamepadAxis::LeftStickY, 0.49),
                (GamepadAxis::RightTrigger, 0.5),
            ],
        ));
        assert_eq!(snap.pressed, BTreeSet::from([GamepadButton::South]));
        assert_eq!(
            snap.deflected,
            BTreeSet::from([(GamepadAxis::LeftStickX, -1), (GamepadAxis::RightTrigger, 1)])
        );
    }

    #[test]
    fn changes_list_presses_releases_and_new_deflections() {
        let before = PadSnapshot::of(&pad(&[(GamepadButton::East, 1.0)], &[(GamepadAxis::DPadX, 1.0)]));
        let after = PadSnapshot::of(&pad(
            &[(GamepadButton::South, 1.0), (GamepadButton::Start, 1.0)],
            &[(GamepadAxis::DPadX, 1.0), (GamepadAxis::RightStickY, -0.9)],
        ));
        assert_eq!(
            before.changes(&after),
            [
                "South pressed",
                "Start pressed",
                "East released",
                "RightStickY -1"
            ]
        );
        assert!(after.changes(&after).is_empty());
    }

    #[test]
    fn seconds_default_to_ten_and_are_bounded() {
        assert_eq!(parse_seconds(None), Ok(10));
        assert_eq!(parse_seconds(Some("0")), Ok(0));
        assert_eq!(parse_seconds(Some("3600")), Ok(3600));
        for bad in ["3601", "-1", "2.5", "x"] {
            assert!(parse_seconds(Some(bad)).is_err(), "{bad} must be refused");
        }
    }
}
