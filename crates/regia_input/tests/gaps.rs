// SPDX-License-Identifier: MPL-2.0
//! Test-gap closure for `regia_input`: modifier edge cases, config and override error paths,
//! override persistence through the VFS, injection ordering, action combination rules and
//! multi-finger touch bookkeeping. Everything here is deterministic (manual 60 Hz frames, fixed
//! inputs, per-process unique temp dirs); none of it depends on wall-clock time.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_input::prelude::*;
use regia_input::{
    BindingKey, DeadZoneMode, ModifierFrame, OVERRIDES_PATH, ResponseCurve, Swizzle, apply_modifiers,
};
use regia_platform::{
    ButtonState, DirMount, InputTimestamp, KeyboardInput, MountPoint, TouchInput, TouchPhase, Vfs, VfsPath,
};

const EPS: f32 = 1e-4;

fn input_app(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app
}

fn send_key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.into(),
        logical_key: name.into(),
        state,
        repeat: false,
        timestamp: InputTimestamp::ZERO,
    });
}

fn touch(app: &mut App, id: u64, phase: TouchPhase, position: (f64, f64)) {
    app.world_mut().write_message(TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position,
        timestamp: InputTimestamp::ZERO,
    });
}

/// A process-unique temp directory (pid + counter), removed by the caller.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-input-gaps-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn user_vfs(dir: &std::path::Path) -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.to_path_buf()));
    vfs
}

fn near(a: Vec3, b: Vec3) -> bool {
    (a - b).abs().max_element() < EPS
}

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

#[test]
fn response_curves_keep_the_sign_and_the_endpoints() {
    let cases = [
        (ResponseCurve::Linear, -0.5, -0.5),
        (ResponseCurve::Square, -0.5, -0.25),
        (ResponseCurve::Cubic, 0.5, 0.125),
        (ResponseCurve::SquareRoot, -0.25, -0.5),
        (ResponseCurve::SmoothStep, 0.5, 0.5),
    ];
    for (curve, input, expected) in cases {
        let got = curve.apply(input);
        assert!(
            (got - expected).abs() < EPS,
            "{curve:?}({input}) = {got}, want {expected}"
        );
    }
    for curve in [
        ResponseCurve::Linear,
        ResponseCurve::Square,
        ResponseCurve::Cubic,
        ResponseCurve::SquareRoot,
        ResponseCurve::SmoothStep,
    ] {
        assert!(curve.apply(0.0).abs() < EPS, "{curve:?} maps 0 to 0");
        assert!((curve.apply(1.0) - 1.0).abs() < EPS, "{curve:?} maps 1 to 1");
        assert!((curve.apply(-1.0) + 1.0).abs() < EPS, "{curve:?} maps -1 to -1");
    }
}

#[test]
fn swizzle_permutes_every_variant() {
    let v = Vec3::new(1.0, 2.0, 3.0);
    assert_eq!(Swizzle::Xyz.apply(v), Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(Swizzle::Yxz.apply(v), Vec3::new(2.0, 1.0, 3.0));
    assert_eq!(Swizzle::Zxy.apply(v), Vec3::new(3.0, 1.0, 2.0));
    assert_eq!(Swizzle::Xzy.apply(v), Vec3::new(1.0, 3.0, 2.0));
}

#[test]
fn dead_zone_constructors_pick_the_mode() {
    assert_eq!(DeadZone::radial(0.1, 1.0).mode, DeadZoneMode::Radial);
    assert_eq!(DeadZone::axial(0.1, 1.0).mode, DeadZoneMode::Axial);
    // Inside the dead zone both modes give exactly zero, for either sign.
    assert_eq!(
        DeadZone::axial(0.2, 1.0).apply(Vec3::new(-0.1, 0.1, 0.0)),
        Vec3::ZERO
    );
    assert_eq!(
        DeadZone::radial(0.2, 1.0).apply(Vec3::new(-0.1, 0.1, 0.0)),
        Vec3::ZERO
    );
}

#[test]
fn axial_dead_zone_keeps_the_sign_of_a_negative_component() {
    let out = DeadZone::axial(0.2, 1.0).apply(Vec3::new(-0.6, 0.0, 0.0));
    assert!(
        (out.x + 0.5).abs() < EPS,
        "(0.6 - 0.2) / 0.8 = 0.5, negated: {out:?}"
    );
}

#[test]
fn smooth_with_a_non_positive_rate_or_zero_delta_passes_through_and_keeps_no_state() {
    let mut state = BTreeMap::new();
    let mut frame = ModifierFrame {
        delta_secs: 1.0 / 60.0,
        binding: BindingKey::default(),
        smoothed: &mut state,
    };
    let v = Vec3::new(0.7, -0.2, 0.0);
    assert_eq!(Modifier::Smooth(0.0).apply(v, &mut frame), v);
    assert_eq!(Modifier::Smooth(-5.0).apply(v, &mut frame), v);
    frame.delta_secs = 0.0;
    assert_eq!(Modifier::Smooth(10.0).apply(v, &mut frame), v);
    assert!(
        state.is_empty(),
        "pass-through paths must not allocate smoothing state"
    );
}

#[test]
fn smooth_blend_is_bit_identical_across_targets() {
    // Audit finding 6 (ADR-4768): the exponential-smoothing blend must not depend on the
    // platform libm. The blend is pinned to the exact bit pattern of the deterministic exp
    // (multiplies only) — change the literal only when the deterministic exp changes
    // deliberately — and kept honest against the f64 reference.
    let mut state = BTreeMap::new();
    let mut step_toward = |raw: Vec3| {
        let mut frame = ModifierFrame {
            delta_secs: 1.0 / 60.0,
            binding: BindingKey::default(),
            smoothed: &mut state,
        };
        apply_modifiers(&[Modifier::Smooth(10.0)], raw, &mut frame)
    };
    // The first sample seeds the smoother (it comes back unchanged); the second reveals the
    // blend: previous = 1, raw = 0, so x = 1 - blend.
    assert_eq!(step_toward(Vec3::X), Vec3::X, "the seed passes through");
    let decayed = step_toward(Vec3::ZERO);
    assert_eq!(
        decayed.x.to_bits(),
        0x3f58b306,
        "1 - blend(10, 1/60) is one exact bit pattern on every target"
    );
    let truth = (-10.0_f64 / 60.0).exp() as f32;
    assert!(
        (decayed.x - truth).abs() < 1e-6,
        "and it still tracks the true value: {} vs {truth}",
        decayed.x
    );
}

#[test]
fn smooth_state_is_kept_per_binding() {
    let mut state = BTreeMap::new();
    let key_a = BindingKey {
        player: 0,
        context: "Gameplay".into(),
        index: 0,
    };
    let key_b = BindingKey {
        player: 0,
        context: "Gameplay".into(),
        index: 1,
    };
    let smooth = [Modifier::Smooth(10.0)];

    let mut step = |key: &BindingKey, value: Vec3| {
        let mut frame = ModifierFrame {
            delta_secs: 0.1,
            binding: key.clone(),
            smoothed: &mut state,
        };
        apply_modifiers(&smooth, value, &mut frame)
    };
    // The first sample seeds the smoother, so it comes back unchanged.
    assert!(near(step(&key_a, Vec3::X), Vec3::X));
    // Binding B's first sample is unaffected by binding A's history.
    assert!(near(step(&key_b, Vec3::ZERO), Vec3::ZERO));
    // A decays from 1 toward 0: 1 - (1 - e^-1) = e^-1.
    let a = step(&key_a, Vec3::ZERO);
    assert!((a.x - (-1.0f32).exp()).abs() < EPS, "{a:?}");
    // B stays at 0, not at A's smoothed value.
    assert!(near(step(&key_b, Vec3::ZERO), Vec3::ZERO));
    assert_eq!(state.len(), 2);
}

#[test]
fn scale_by_delta_and_modifier_conversions() {
    let mut state = BTreeMap::new();
    let mut frame = ModifierFrame {
        delta_secs: 0.5,
        binding: BindingKey::default(),
        smoothed: &mut state,
    };
    assert_eq!(
        Modifier::ScaleByDelta.apply(Vec3::new(4.0, -2.0, 0.0), &mut frame),
        Vec3::new(2.0, -1.0, 0.0)
    );
    assert_eq!(
        Modifier::from(Vec3::new(2.0, 3.0, 4.0)),
        Modifier::Scale(Vec3::new(2.0, 3.0, 4.0))
    );
    let dz = DeadZone::radial(0.1, 1.0);
    assert_eq!(Modifier::from(dz), Modifier::DeadZone(dz));
    // An empty chain is the identity.
    assert_eq!(apply_modifiers(&[], Vec3::ONE, &mut frame), Vec3::ONE);
}

// ---------------------------------------------------------------------------
// Config and overrides: error paths and builders
// ---------------------------------------------------------------------------

#[test]
fn from_ron_reports_garbage_with_a_prefixed_error() {
    let err = InputConfig::from_ron("this is ( not valid").expect_err("garbage must not parse");
    assert!(err.starts_with("input config RON"), "{err}");
    let err = InputOverrides::from_ron("{{{").expect_err("garbage must not parse");
    assert!(err.starts_with("input overrides RON"), "{err}");
    // An unknown ActionKind variant is a parse error, not a silent default.
    assert!(InputConfig::from_ron("(actions: {\"Jump\": Teleport})").is_err());
}

#[test]
fn empty_config_ron_parses_to_the_default() {
    assert_eq!(
        InputConfig::from_ron("()").expect("parses"),
        InputConfig::default()
    );
    assert_eq!(
        InputOverrides::from_ron("()").expect("parses"),
        InputOverrides::default()
    );
}

#[test]
fn config_builders_create_contexts_lazily() {
    let config = InputConfig::new();
    assert!(config.gameplay().is_empty(), "no Gameplay context yet");
    assert_eq!(config.kind("Missing"), None);

    let config = config
        .action("Menu", ActionKind::Button)
        .binding_in("Menu", Binding::new("Menu", InputSource::key("Escape")));
    let ctx = config
        .contexts
        .get("Menu")
        .expect("binding_in creates the context");
    assert_eq!(ctx.priority, 0, "implicit contexts get priority 0");
    assert!(!ctx.consume_lower);
    assert_eq!(ctx.bindings.len(), 1);
    assert!(
        config.gameplay().is_empty(),
        "other contexts do not leak into Gameplay"
    );

    let explicit = InputConfig::new().context("Menu", InputContext::consuming(5));
    assert!(explicit.contexts["Menu"].consume_lower);
    assert_eq!(explicit.contexts["Menu"].priority, 5);
}

#[test]
fn merge_overrides_ignores_unknown_contexts_and_appends_new_actions() {
    let base = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")));

    let mut overrides = InputOverrides::default();
    overrides.set("Nowhere", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
    let mut config = base.clone();
    assert_eq!(config.merge_overrides(&overrides), 0);
    assert_eq!(config, base, "an override for an unknown context changes nothing");

    // An override for an action with no existing binding is appended but does not count as a
    // replacement.
    let mut overrides = InputOverrides::default();
    overrides.set(
        "Gameplay",
        "Crouch",
        Binding::new("Crouch", InputSource::key("KeyC")),
    );
    let mut config = base.clone();
    assert_eq!(config.merge_overrides(&overrides), 0);
    assert_eq!(config.gameplay().len(), 2);

    // Merging the same overrides twice is idempotent: one binding per overridden action.
    let mut overrides = InputOverrides::default();
    overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
    let mut config = base;
    assert_eq!(config.merge_overrides(&overrides), 1);
    assert_eq!(config.merge_overrides(&overrides), 1);
    assert_eq!(config.gameplay().len(), 1);
    assert_eq!(config.gameplay()[0].input, InputSource::key("KeyJ"));
}

#[test]
fn overrides_set_replaces_and_clear_reports_whether_it_removed() {
    let mut overrides = InputOverrides::default();
    assert!(!overrides.clear("Gameplay", "Jump"), "unknown context");
    overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
    assert!(
        !overrides.clear("Gameplay", "Fire"),
        "known context, unknown action"
    );
    overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyK")));
    assert_eq!(
        overrides.contexts["Gameplay"]["Jump"].input,
        InputSource::key("KeyK"),
        "set replaces the earlier override"
    );
    assert!(overrides.clear("Gameplay", "Jump"));
    assert!(!overrides.clear("Gameplay", "Jump"), "already cleared");
}

#[test]
fn overrides_load_distinguishes_missing_from_corrupt() {
    let dir = temp_dir("overrides-load");
    let vfs = user_vfs(&dir);
    let path = VfsPath::parse(OVERRIDES_PATH).expect("valid path");

    // Missing file: an empty set, not an error.
    assert_eq!(InputOverrides::load(&vfs, &path), Ok(InputOverrides::default()));

    // Saved overrides round-trip.
    let mut overrides = InputOverrides::default();
    overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
    overrides.save(&vfs, &path).expect("save");
    assert_eq!(InputOverrides::load(&vfs, &path), Ok(overrides));

    // A corrupt file is reported, never silently treated as empty.
    vfs.write_atomic(&path, b"( contexts: oops")
        .expect("write corrupt file");
    let err = InputOverrides::load(&vfs, &path).expect_err("corrupt file is an error");
    assert!(err.contains("input overrides RON"), "{err}");

    // Invalid UTF-8 is replaced lossily and then fails to parse, still an error rather than a panic.
    vfs.write_atomic(&path, &[0xff, 0xfe, 0x00])
        .expect("write binary file");
    assert!(InputOverrides::load(&vfs, &path).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overrides_save_to_a_read_only_mount_is_an_error() {
    let dir = temp_dir("overrides-readonly");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::read_only(dir.clone()));
    let path = VfsPath::parse(OVERRIDES_PATH).expect("valid path");
    assert!(InputOverrides::default().save(&vfs, &path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Rebinder state machine
// ---------------------------------------------------------------------------

#[test]
fn rebinder_cancel_and_restart_clear_the_pending_capture() {
    let mut app = input_app(
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space"))),
    );
    app.update();
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("Gameplay", "Jump");
    app.world_mut().resource_mut::<InputRebinder>().cancel();

    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    let rebinder = app.world().resource::<InputRebinder>();
    assert!(rebinder.pending().is_none());
    assert!(rebinder.outcome.is_none(), "a cancelled rebind captures nothing");
    assert_eq!(
        app.world().resource::<InputConfig>().gameplay()[0].input,
        InputSource::key("Space"),
        "the live config is untouched"
    );
    assert!(app.world().resource::<InputOverrides>().contexts.is_empty());
}

#[test]
fn rebinder_without_a_pending_request_ignores_input() {
    let mut app = input_app(
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space"))),
    );
    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<InputRebinder>().outcome.is_none());
    assert!(app.world().resource::<InputOverrides>().contexts.is_empty());
}

#[test]
fn rebinding_to_a_source_used_by_another_action_reports_the_conflict_but_applies() {
    let mut app = input_app(
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Crouch", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new("Crouch", InputSource::key("KeyC"))),
    );
    app.update();
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("Gameplay", "Jump");
    send_key(&mut app, "KeyC", ButtonState::Pressed);
    app.update();

    let outcome = app
        .world()
        .resource::<InputRebinder>()
        .outcome
        .clone()
        .expect("rebind completed");
    assert_eq!(outcome.conflicts, vec!["Crouch".to_string()]);
    assert_eq!(outcome.input, InputSource::key("KeyC"));
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("Jump"), "the conflicting rebind is still applied");
    assert!(
        actions.pressed("Crouch"),
        "and the other action keeps its binding"
    );
}

#[test]
fn rebinding_an_undeclared_context_does_not_panic_and_leaves_the_config_alone() {
    let base = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")));
    let mut app = input_app(base.clone());
    app.update();
    app.world_mut()
        .resource_mut::<InputRebinder>()
        .start("NoSuchContext", "Jump");
    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    assert_eq!(*app.world().resource::<InputConfig>(), base);
    assert!(
        app.world().resource::<InputRebinder>().pending().is_none(),
        "the capture is consumed even though nothing could be applied"
    );
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

#[test]
fn injection_boundaries_prune_and_independent_actions() {
    let mut injection = InputInjection::default();
    injection.schedule("Move", Vec3::X, 10, 3);
    injection.schedule("Look", Vec3::Y, 11, 1);
    assert_eq!(injection.scheduled_count(), 2);

    assert_eq!(injection.value_for("Move", 9), None, "before the window");
    assert_eq!(
        injection.value_for("Move", 10),
        Some(Vec3::X),
        "start is inclusive"
    );
    assert_eq!(injection.value_for("Move", 12), Some(Vec3::X));
    assert_eq!(injection.value_for("Move", 13), None, "end is exclusive");
    assert_eq!(injection.value_for("Look", 11), Some(Vec3::Y));
    assert_eq!(injection.value_for("Look", 12), None);
    assert_eq!(
        injection.value_for("Never", 11),
        None,
        "unscheduled actions read devices"
    );

    injection.prune(12);
    assert_eq!(
        injection.scheduled_count(),
        1,
        "only the expired entry is dropped"
    );
    assert_eq!(injection.value_for("Move", 12), Some(Vec3::X));
    injection.prune(13);
    assert_eq!(injection.scheduled_count(), 0);
}

#[test]
fn releasing_an_action_that_was_never_held_is_harmless() {
    let mut injection = InputInjection::default();
    injection.release("Nothing");
    assert_eq!(injection.value_for("Nothing", 0), None);
    injection.hold("Move", Vec3::ZERO);
    assert_eq!(
        injection.value_for("Move", 0),
        Some(Vec3::ZERO),
        "a held zero is 'actively driven to zero', not absent"
    );
}

#[test]
fn injecting_an_undeclared_action_does_not_panic_or_create_it() {
    let mut app = input_app(InputConfig::new().action("Jump", ActionKind::Button));
    app.update();
    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold("Ghost", Vec3::X);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(!actions.pressed("Ghost"));
    assert_eq!(actions.kind("Ghost"), None);
}

// ---------------------------------------------------------------------------
// Combination rules
// ---------------------------------------------------------------------------

#[test]
fn button_bindings_or_together_and_axis_bindings_add() {
    let config = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .action("Throttle", ActionKind::Axis1D)
        .binding(Binding::new("Fire", InputSource::key("KeyF")))
        .binding(Binding::new("Fire", InputSource::key("KeyG")))
        .binding(Binding::new("Throttle", InputSource::key("KeyT")))
        .binding(Binding::new("Throttle", InputSource::key("KeyY")));
    let mut app = input_app(config);
    for key in ["KeyF", "KeyG", "KeyT", "KeyY"] {
        send_key(&mut app, key, ButtonState::Pressed);
    }
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("Fire"));
    assert_eq!(
        actions.value("Fire"),
        Vec3::X,
        "buttons clamp to exactly 1, not 2"
    );
    assert!(
        (actions.axis1d("Throttle") - 2.0).abs() < EPS,
        "two active axis bindings sum: {}",
        actions.axis1d("Throttle")
    );
}

#[test]
fn releasing_all_keys_returns_every_action_to_idle() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ));
    let mut app = input_app(config);
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    send_key(&mut app, "KeyD", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().axis2d("Move").length() > 0.9);
    send_key(&mut app, "KeyW", ButtonState::Released);
    send_key(&mut app, "KeyD", ButtonState::Released);
    app.update();
    assert_eq!(app.world().resource::<Actions>().axis2d("Move"), Vec2::ZERO);
}

#[test]
fn opposite_keys_cancel_on_an_axis() {
    let config = InputConfig::new()
        .action("Move", ActionKind::Axis2D)
        .binding(Binding::new(
            "Move",
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ));
    let mut app = input_app(config);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    send_key(&mut app, "KeyD", ButtonState::Pressed);
    app.update();
    assert_eq!(app.world().resource::<Actions>().axis2d("Move"), Vec2::ZERO);
}

// ---------------------------------------------------------------------------
// Touch bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn touch_centroid_is_the_mean_and_cancelled_touches_do_not_count() {
    let mut app = input_app(InputConfig::new());
    assert_eq!(
        app.world().resource::<Touches>().centroid(),
        None,
        "no fingers yet"
    );

    touch(&mut app, 1, TouchPhase::Started, (100.0, 100.0));
    touch(&mut app, 2, TouchPhase::Started, (200.0, 300.0));
    app.update();
    {
        let touches = app.world().resource::<Touches>();
        assert_eq!(touches.count(), 2);
        assert_eq!(touches.centroid(), Some(Vec2::new(150.0, 200.0)));
        assert_eq!(touches.iter().count(), 2);
    }

    touch(&mut app, 2, TouchPhase::Cancelled, (200.0, 300.0));
    app.update();
    let touches = app.world().resource::<Touches>();
    assert_eq!(touches.count(), 1, "a cancelled finger is not down");
    assert_eq!(touches.get(2).map(|p| p.phase), Some(TouchPhase::Cancelled));
}
