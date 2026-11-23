// SPDX-License-Identifier: MPL-2.0
//! The action manifest, the attach-once rule, per-sync action states, the `regia_input` bridge and
//! haptics (spec 58 § 2.7): the branches the in-crate tests leave open. CPU only.

use regia_core::glam::{Quat, Vec2, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{
    ControllerAction, ControllerState, Hand, NullXr, Pose, PoseSample, SimEvent, XrBackend, XrError,
};
use regia_xr::input::{controller_source, valid_name};
use regia_xr::{
    ActionDecl, ActionPoses, ActionSetDecl, ActionType, BindingDecl, DominantHand, HapticOutcome,
    ProfileBindings, XrActionManifest, XrActionSets, XrActionStates, XrActionValue,
};

fn action(name: &str, ty: ActionType, subs: &[&str]) -> ActionDecl {
    ActionDecl {
        name: name.into(),
        ty,
        label: String::new(),
        subs: subs.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn manifest_with(actions: Vec<ActionDecl>, bindings: Vec<BindingDecl>) -> XrActionManifest {
    XrActionManifest {
        format_version: 1,
        sets: vec![ActionSetDecl {
            name: "main".into(),
            priority: 0,
            actions,
        }],
        bindings: vec![ProfileBindings {
            profile: "p".into(),
            optional: false,
            map: bindings,
        }],
    }
}

fn bind(a: &str, sub: &str, path: &str) -> BindingDecl {
    BindingDecl(a.into(), sub.into(), path.into())
}

fn states_for(manifest: &XrActionManifest) -> XrActionStates {
    XrActionStates::new(&XrActionSets::from_manifest(manifest))
}

fn pose(x: f32) -> Pose {
    Pose::new(Vec3::new(x, 1.0, -0.5), Quat::IDENTITY)
}

fn live_controller(hand: Hand) -> ControllerState {
    let mut c = ControllerState::inactive(hand);
    c.active = true;
    c
}

/// Catches the default manifest drifting: its sets, per-hand shape and binding counts are what
/// games and the agent guide document.
#[test]
fn the_default_manifest_has_the_documented_shape() {
    let m = XrActionManifest::default();
    assert_eq!(m.format_version, 1);
    assert_eq!(m.sets.len(), 1);
    assert_eq!(m.sets[0].name, "gameplay");
    let expect = [
        ("move", ActionType::Vec2, vec!["left"]),
        ("turn", ActionType::Vec2, vec!["right"]),
        ("select", ActionType::Float, vec!["left", "right"]),
        ("grab", ActionType::Float, vec!["left", "right"]),
        ("primary", ActionType::Bool, vec!["left", "right"]),
        ("secondary", ActionType::Bool, vec!["left", "right"]),
        ("menu", ActionType::Bool, vec!["left"]),
        ("grip_pose", ActionType::Pose, vec!["left", "right"]),
        ("aim_pose", ActionType::Pose, vec!["left", "right"]),
        ("haptic", ActionType::Haptic, vec!["left", "right"]),
    ];
    let actions = &m.sets[0].actions;
    assert_eq!(actions.len(), expect.len());
    for (decl, (name, ty, subs)) in actions.iter().zip(expect) {
        assert_eq!((decl.name.as_str(), decl.ty), (name, ty));
        assert_eq!(decl.subs, subs, "{name}");
        assert_eq!(decl.label, format!("xr-action-{}", name.replace('_', "-")));
    }
    assert_eq!(m.bindings.len(), 2);
    assert_eq!(
        m.bindings[0].profile,
        "/interaction_profiles/khr/simple_controller"
    );
    assert_eq!(m.bindings[0].map.len(), 9);
    assert_eq!(m.bindings[1].profile, "generic_two_hand_controller");
    assert_eq!(m.bindings[1].map.len(), 17);
    let turn = m.bindings[1]
        .map
        .iter()
        .find(|b| b.action() == "turn")
        .expect("turn is bound");
    assert_eq!(
        (turn.sub(), turn.full_path().as_str()),
        ("right", "/user/hand/right/input/thumbstick")
    );
    assert!(m.bindings.iter().all(|p| !p.optional));
    assert_eq!(m.find("select").map(|d| d.ty), Some(ActionType::Float));
    assert!(m.find("nope").is_none());
}

/// Catches a swap that is not an involution, drops a binding, or leaves a binding pointing at a
/// hand the action no longer has.
#[test]
fn the_left_handed_swap_is_an_involution_and_stays_consistent() {
    let m = XrActionManifest::default();
    assert_eq!(
        m.with_dominant_hand(DominantHand::Right),
        m,
        "right-handed is the identity"
    );
    let swapped = m.with_dominant_hand(DominantHand::Left);
    assert_ne!(swapped, m);
    assert_eq!(
        swapped.with_dominant_hand(DominantHand::Left),
        m,
        "swapping twice restores"
    );
    swapped.validate().expect("still valid");
    // Single-hand actions follow their binding to the other hand; two-hand ones keep their order.
    let subs = |name: &str| swapped.find(name).expect("action").subs.clone();
    assert_eq!(subs("move"), ["right"]);
    assert_eq!(subs("turn"), ["left"]);
    assert_eq!(subs("menu"), ["right"]);
    assert_eq!(subs("select"), ["left", "right"]);
    // Every binding counts the same as before, and absolute paths name the binding's own hand.
    for (a, b) in swapped.bindings.iter().zip(&m.bindings) {
        assert_eq!(a.map.len(), b.map.len());
        for binding in &a.map {
            let path = binding.full_path();
            assert!(
                path.starts_with(&format!("/user/hand/{}/", binding.sub())),
                "{} {} -> {path}",
                binding.action(),
                binding.sub()
            );
        }
    }
    let turn = swapped.bindings[1]
        .map
        .iter()
        .find(|b| b.action() == "turn")
        .expect("turn");
    assert_eq!(
        (turn.sub(), turn.full_path().as_str()),
        ("left", "/user/hand/left/input/thumbstick")
    );
    let haptic_left = swapped.bindings[0]
        .map
        .iter()
        .find(|b| b.action() == "haptic" && b.sub() == "left")
        .expect("haptic left");
    assert_eq!(haptic_left.full_path(), "/user/hand/left/output/haptic");
    // Types and names are untouched.
    for (a, b) in swapped.sets[0].actions.iter().zip(&m.sets[0].actions) {
        assert_eq!((&a.name, a.ty), (&b.name, b.ty));
    }
}

/// Catches `valid_name` accepting what the grammar forbids, at the edges.
#[test]
fn name_grammar_edges() {
    for ok in ["a", "z9", "grip_pose", "a_", "a__b", "x0y1"] {
        assert!(valid_name(ok), "{ok}");
    }
    for bad in ["", "_a", "9a", "A", "aB", "a b", "a-b", "a.b", "é", "a\n", "ａ"] {
        assert!(!valid_name(bad), "{bad:?}");
    }
}

/// Catches validation holes: every structural error is refused, with the offender in the key.
#[test]
fn manifest_validation_names_the_offender() {
    let key = |m: &XrActionManifest| match m.validate() {
        Err(XrError::InvalidValue { key, .. }) => key,
        Err(XrError::ActionUnknown { name }) => format!("unknown:{name}"),
        other => panic!("expected an error, got {other:?}"),
    };
    let ok = manifest_with(
        vec![action("jump", ActionType::Bool, &[])],
        vec![bind("jump", "", "a/click")],
    );
    ok.validate()
        .expect("an action without sub-paths is bound with an empty sub-path");

    let mut m = ok.clone();
    m.sets[0].name = "Main".into();
    assert_eq!(key(&m), "set Main");

    let mut m = ok.clone();
    m.sets.push(m.sets[0].clone());
    assert_eq!(key(&m), "set main", "duplicate set name");

    let m = manifest_with(
        vec![
            action("jump", ActionType::Bool, &[]),
            action("jump", ActionType::Float, &[]),
        ],
        vec![],
    );
    assert_eq!(key(&m), "action jump");

    let m = manifest_with(vec![action("hold", ActionType::Float, &["left", "left"])], vec![]);
    assert_eq!(key(&m), "action hold sub left", "repeated sub-path");

    let m = manifest_with(vec![action("hold", ActionType::Float, &["head"])], vec![]);
    assert_eq!(key(&m), "action hold sub head");

    // Bindings: the action must exist and the sub-path must be one of its own.
    let m = manifest_with(
        vec![action("jump", ActionType::Bool, &[])],
        vec![bind("jump", "left", "a/click")],
    );
    assert_eq!(
        key(&m),
        "binding jump left",
        "a sub-path on an action without any"
    );
    let m = manifest_with(
        vec![action("grab", ActionType::Float, &["left", "right"])],
        vec![bind("grab", "", "squeeze/value")],
    );
    assert_eq!(key(&m), "binding grab ", "an empty sub-path on a per-hand action");
    let m = manifest_with(
        vec![action("grab", ActionType::Float, &["left"])],
        vec![bind("grab", "right", "x")],
    );
    assert_eq!(key(&m), "binding grab right");
    let m = manifest_with(vec![], vec![bind("ghost", "left", "x")]);
    assert!(key(&m).starts_with("unknown:ghost"), "{}", key(&m));

    let mut m = ok;
    m.format_version = 2;
    assert_eq!(key(&m), "format_version");
}

/// Catches `full_path` mishandling a relative or an absolute path.
#[test]
fn binding_paths_are_relative_to_the_hand_unless_absolute() {
    assert_eq!(
        bind("a", "left", "trigger/value").full_path(),
        "/user/hand/left/input/trigger/value"
    );
    assert_eq!(
        bind("a", "right", "/user/head/input/volume_up/click").full_path(),
        "/user/head/input/volume_up/click"
    );
    assert_eq!(
        (bind("x", "left", "p").action(), bind("x", "left", "p").sub()),
        ("x", "left")
    );
}

/// Catches defaults and strictness in the manifest text: omitted fields default, unknown ones are
/// errors, and the vendor-profile flag survives a round trip.
#[test]
fn manifest_text_defaults_unknown_fields_and_the_optional_flag() {
    let text = r#"(format_version: 1, sets: [(name: "s", actions: [(name: "a", ty: Bool)])])"#;
    let m = XrActionManifest::from_ron(text).expect("loads");
    assert_eq!(m.sets[0].priority, 0);
    assert_eq!(m.sets[0].actions[0].label, "");
    assert!(m.sets[0].actions[0].subs.is_empty());
    assert!(m.bindings.is_empty());
    for bad in [
        r#"(format_version: 1, sets: [(name: "s", actions: [(name: "a", ty: Bool, colour: 1)])])"#,
        r#"(format_version: 1, sets: [(name: "s", actions: [(name: "a", ty: Quaternion)])])"#,
        r#"(format_version: 1)"#,
        r#"(format_version: 1, sets: [(name: "s")])"#,
        r#"(format_version: 1, sets: [], extra: 1)"#,
        "",
    ] {
        let e = XrActionManifest::from_ron(bad).expect_err(bad);
        assert_eq!(e.code(), "xr_invalid_value", "{bad:?}: {e}");
    }
    let mut vendor = XrActionManifest::default();
    vendor.bindings[1].optional = true;
    vendor.sets[0].priority = 7;
    let again = XrActionManifest::from_ron(&vendor.to_ron().expect("ser")).expect("reload");
    assert_eq!(again, vendor);
    assert!(again.bindings[1].optional && again.sets[0].priority == 7);
}

/// Catches value helper tables going wrong: defaults, the `regia_input` encoding, type matching.
#[test]
fn action_value_helpers() {
    assert_eq!(
        XrActionValue::default_for(ActionType::Bool),
        Some(XrActionValue::Bool(false))
    );
    assert_eq!(
        XrActionValue::default_for(ActionType::Float),
        Some(XrActionValue::Float(0.0))
    );
    assert_eq!(
        XrActionValue::default_for(ActionType::Vec2),
        Some(XrActionValue::Vec2(Vec2::ZERO))
    );
    assert_eq!(
        XrActionValue::default_for(ActionType::Pose),
        Some(XrActionValue::Pose(Pose::IDENTITY))
    );
    assert_eq!(XrActionValue::default_for(ActionType::Haptic), None);

    assert_eq!(
        XrActionValue::Bool(true).as_input(),
        Some(Vec3::new(1.0, 0.0, 0.0))
    );
    assert_eq!(XrActionValue::Bool(false).as_input(), Some(Vec3::ZERO));
    assert_eq!(
        XrActionValue::Float(0.75).as_input(),
        Some(Vec3::new(0.75, 0.0, 0.0))
    );
    assert_eq!(
        XrActionValue::Vec2(Vec2::new(-0.5, 0.25)).as_input(),
        Some(Vec3::new(-0.5, 0.25, 0.0))
    );
    assert_eq!(XrActionValue::Pose(Pose::IDENTITY).as_input(), None);

    let samples = [
        (XrActionValue::Bool(true), ActionType::Bool),
        (XrActionValue::Float(1.0), ActionType::Float),
        (XrActionValue::Vec2(Vec2::ONE), ActionType::Vec2),
        (XrActionValue::Pose(Pose::IDENTITY), ActionType::Pose),
    ];
    let types = [
        ActionType::Bool,
        ActionType::Float,
        ActionType::Vec2,
        ActionType::Pose,
        ActionType::Haptic,
    ];
    for (value, own) in samples {
        for ty in types {
            assert_eq!(value.matches(ty), ty == own, "{value:?} vs {ty:?}");
        }
    }
}

/// Catches the name-to-controller-field table drifting.
#[test]
fn controller_sources_by_action_name() {
    assert_eq!(controller_source("select"), Some(ControllerAction::Trigger));
    assert_eq!(controller_source("grab"), Some(ControllerAction::Squeeze));
    assert_eq!(controller_source("move"), Some(ControllerAction::Thumbstick));
    assert_eq!(controller_source("turn"), Some(ControllerAction::Thumbstick));
    assert_eq!(
        controller_source("primary"),
        Some(ControllerAction::PrimaryButton)
    );
    assert_eq!(
        controller_source("secondary"),
        Some(ControllerAction::SecondaryButton)
    );
    assert_eq!(
        controller_source("thumbstick_click"),
        Some(ControllerAction::ThumbstickClick)
    );
    assert_eq!(controller_source("menu"), Some(ControllerAction::Menu));
    for other in ["", "Select", "grip_pose", "haptic", "jump"] {
        assert_eq!(controller_source(other), None, "{other:?}");
    }
}

/// Catches `create_action` accepting a bad name, and the frozen error not blocking the other
/// failure kinds' precedence.
#[test]
fn creating_actions_checks_names_and_the_frozen_state_first() {
    let mut sets = XrActionSets::from_manifest(&XrActionManifest::default());
    assert!(!sets.attached());
    let before = sets.sets()[0].actions.len();
    for bad in ["Jump", "", "9lives", "no-dash"] {
        let e = sets
            .create_action("gameplay", action(bad, ActionType::Bool, &[]))
            .expect_err(bad);
        assert_eq!(e.code(), "xr_invalid_value", "{bad:?}");
    }
    assert_eq!(sets.sets()[0].actions.len(), before, "nothing was added");
    sets.create_action("gameplay", action("jump", ActionType::Bool, &[]))
        .expect("ok");
    assert_eq!(sets.sets()[0].actions.len(), before + 1);
    sets.attach().expect("attach");
    assert!(sets.attached());
    // Frozen outranks both a bad name and an unknown set.
    assert_eq!(
        sets.create_action("nowhere", action("Bad", ActionType::Bool, &[]))
            .expect_err("frozen")
            .code(),
        "xr_actions_frozen"
    );
    assert_eq!(sets.sets()[0].actions.len(), before + 1);
    // A set rebuilt from the manifest starts detached again: attach is once per session.
    let mut fresh = XrActionSets::from_manifest(&XrActionManifest::default());
    fresh.attach().expect("a new session attaches again");
}

/// Catches states being built for haptics, or for the wrong keys.
#[test]
fn states_exist_for_every_non_haptic_action_and_sub_path() {
    let m = manifest_with(
        vec![
            action("jump", ActionType::Bool, &[]),
            action("grab", ActionType::Float, &["left", "right"]),
            action("buzz", ActionType::Haptic, &["left", "right"]),
            action("hand", ActionType::Pose, &["right"]),
        ],
        vec![],
    );
    let states = states_for(&m);
    let keys: Vec<_> = states
        .iter()
        .map(|(k, active, _)| (k.0.as_str(), k.1.as_str(), active))
        .collect();
    // Key order is the (action, sub-path) order; everything starts inactive.
    assert_eq!(
        keys,
        [
            ("grab", "left", false),
            ("grab", "right", false),
            ("hand", "right", false),
            ("jump", "", false)
        ]
    );
    assert_eq!(states.get("jump", ""), Some((false, XrActionValue::Bool(false))));
    assert_eq!(states.get("buzz", "left"), None, "haptics have no state");
    assert_eq!(states.get("jump", "left"), None);
    assert_eq!(states.syncs(), 0);
}

/// Catches `input_name` mixing up the per-hand rule.
#[test]
fn input_names_add_the_hand_only_for_per_hand_actions() {
    let m = manifest_with(
        vec![
            action("jump", ActionType::Bool, &[]),
            action("move", ActionType::Vec2, &["left"]),
            action("grab", ActionType::Float, &["left", "right"]),
        ],
        vec![],
    );
    let states = states_for(&m);
    assert_eq!(states.input_name("jump", ""), "jump");
    assert_eq!(
        states.input_name("move", "left"),
        "move",
        "one sub-path: the plain name"
    );
    assert_eq!(states.input_name("grab", "left"), "grab_left");
    assert_eq!(states.input_name("grab", "right"), "grab_right");
    assert_eq!(states.input_name("grab", ""), "grab");
    assert_eq!(states.input_name("unknown", "left"), "unknown");
}

/// Catches the controller-to-action read: each default action reads its own field of its own
/// hand, an inactive controller reads released, and out-of-range values are clamped.
#[test]
fn syncing_reads_each_action_from_its_own_controller_field() {
    let mut states = states_for(&XrActionManifest::default());
    let mut left = live_controller(Hand::Left);
    left.thumbstick = Vec2::new(0.25, 0.75);
    left.menu = true;
    left.primary = true;
    left.trigger = 1.5;
    left.grip = PoseSample::at_rest(pose(-0.3), 0);
    left.aim = PoseSample::at_rest(pose(-0.2), 0);
    let mut right = live_controller(Hand::Right);
    right.thumbstick = Vec2::new(-1.0, 0.5);
    right.squeeze = 0.3;
    right.secondary = true;
    right.grip = PoseSample::at_rest(pose(0.3), 0);
    right.aim = PoseSample::at_rest(pose(0.2), 0);
    states.sync(true, &[left, right]);
    let get = |a: &str, s: &str| states.get(a, s).expect("state");
    assert_eq!(
        get("move", "left"),
        (true, XrActionValue::Vec2(Vec2::new(0.25, 0.75)))
    );
    assert_eq!(
        get("turn", "right"),
        (true, XrActionValue::Vec2(Vec2::new(-1.0, 0.5)))
    );
    assert_eq!(
        get("select", "left"),
        (true, XrActionValue::Float(1.0)),
        "1.5 clamps to 1"
    );
    assert_eq!(
        get("select", "right"),
        (true, XrActionValue::Float(0.0)),
        "active but released"
    );
    assert_eq!(get("grab", "right"), (true, XrActionValue::Float(0.3)));
    assert_eq!(get("primary", "left"), (true, XrActionValue::Bool(true)));
    assert_eq!(get("primary", "right"), (true, XrActionValue::Bool(false)));
    assert_eq!(get("secondary", "right"), (true, XrActionValue::Bool(true)));
    assert_eq!(get("secondary", "left"), (true, XrActionValue::Bool(false)));
    assert_eq!(get("menu", "left"), (true, XrActionValue::Bool(true)));
    assert_eq!(get("grip_pose", "left"), (true, XrActionValue::Pose(pose(-0.3))));
    assert_eq!(get("aim_pose", "left"), (true, XrActionValue::Pose(pose(-0.2))));
    assert_eq!(get("grip_pose", "right"), (true, XrActionValue::Pose(pose(0.3))));
    assert_eq!(get("aim_pose", "right"), (true, XrActionValue::Pose(pose(0.2))));
    assert_eq!(states.syncs(), 1);

    // An untracked controller reads inactive and released, whatever its stale fields say.
    let mut stale_left = left;
    stale_left.active = false;
    states.sync(true, &[stale_left, right]);
    assert_eq!(
        states.get("select", "left"),
        Some((false, XrActionValue::Float(0.0)))
    );
    assert_eq!(
        states.get("move", "left"),
        Some((false, XrActionValue::Vec2(Vec2::ZERO)))
    );
    assert_eq!(
        states.get("grip_pose", "left"),
        Some((false, XrActionValue::Pose(Pose::IDENTITY)))
    );
    assert_eq!(
        states.get("grab", "right"),
        Some((true, XrActionValue::Float(0.3))),
        "the other hand is untouched"
    );
    assert_eq!(states.syncs(), 2);
}

/// Catches names that look like controller fields being fed anyway: wrong type, custom names,
/// and actions without a hand are only ever driven by injection.
#[test]
fn actions_the_controller_cannot_feed_stay_inactive() {
    let m = manifest_with(
        vec![
            action("select", ActionType::Bool, &["right"]),
            action("wave", ActionType::Bool, &["right"]),
            action("menu", ActionType::Bool, &[]),
            action("hand_pose", ActionType::Pose, &["right"]),
        ],
        vec![],
    );
    let mut states = states_for(&m);
    let mut right = live_controller(Hand::Right);
    right.trigger = 1.0;
    right.menu = true;
    right.primary = true;
    states.sync(true, &[ControllerState::inactive(Hand::Left), right]);
    assert_eq!(
        states.get("select", "right"),
        Some((false, XrActionValue::Bool(false))),
        "a float source on a bool action"
    );
    assert_eq!(
        states.get("wave", "right"),
        Some((false, XrActionValue::Bool(false)))
    );
    assert_eq!(states.get("menu", ""), Some((false, XrActionValue::Bool(false))));
    assert_eq!(
        states.get("hand_pose", "right"),
        Some((false, XrActionValue::Pose(Pose::IDENTITY)))
    );
    // Injection drives all of them, including the one without a hand.
    states
        .inject("wave", "right", XrActionValue::Bool(true))
        .expect("inject");
    states
        .inject("menu", "", XrActionValue::Bool(true))
        .expect("inject");
    states
        .inject("hand_pose", "right", XrActionValue::Pose(pose(2.0)))
        .expect("inject");
    states.sync(true, &[ControllerState::inactive(Hand::Left), right]);
    assert_eq!(
        states.get("wave", "right"),
        Some((true, XrActionValue::Bool(true)))
    );
    assert_eq!(states.get("menu", ""), Some((true, XrActionValue::Bool(true))));
    assert_eq!(
        states.get("hand_pose", "right"),
        Some((true, XrActionValue::Pose(pose(2.0))))
    );
}

/// Catches injection precedence and persistence: an injected value beats the controller, holds
/// until replaced, is hidden while input is not live, and returns when it is live again.
#[test]
fn injection_beats_the_controller_and_survives_a_focus_gap() {
    let mut states = states_for(&XrActionManifest::default());
    let mut right = live_controller(Hand::Right);
    right.trigger = 0.8;
    let pads = [ControllerState::inactive(Hand::Left), right];
    states
        .inject("select", "right", XrActionValue::Float(0.1))
        .expect("inject");
    states.sync(true, &pads);
    assert_eq!(
        states.get("select", "right"),
        Some((true, XrActionValue::Float(0.1)))
    );
    states.sync(false, &pads);
    assert_eq!(
        states.get("select", "right"),
        Some((false, XrActionValue::Float(0.0))),
        "not live"
    );
    states.sync(true, &pads);
    assert_eq!(
        states.get("select", "right"),
        Some((true, XrActionValue::Float(0.1))),
        "still held"
    );
    states
        .inject("select", "right", XrActionValue::Float(0.4))
        .expect("replace");
    states.sync(true, &pads);
    assert_eq!(
        states.get("select", "right"),
        Some((true, XrActionValue::Float(0.4)))
    );
}

/// Catches the injection error messages: the action/sub spelling, the type message, and haptics
/// and unknown sub-paths being refused.
#[test]
fn injection_errors_name_the_action() {
    let m = manifest_with(
        vec![
            action("jump", ActionType::Bool, &[]),
            action("buzz", ActionType::Haptic, &["left"]),
        ],
        vec![],
    );
    let mut states = states_for(&m);
    assert_eq!(
        states.inject("jump", "left", XrActionValue::Bool(true)),
        Err(XrError::ActionUnknown {
            name: "jump/left".into()
        })
    );
    assert_eq!(
        states.inject("ghost", "", XrActionValue::Bool(true)),
        Err(XrError::ActionUnknown { name: "ghost".into() })
    );
    assert_eq!(
        states
            .inject("buzz", "left", XrActionValue::Float(1.0))
            .map_err(|e| e.code()),
        Err("xr_action_unknown"),
        "haptics are outputs"
    );
    let wrong = states
        .inject("jump", "", XrActionValue::Float(1.0))
        .expect_err("type");
    assert_eq!(wrong.code(), "xr_invalid_value");
    assert!(wrong.to_string().contains("Bool"), "{wrong}");
    // Nothing was stored by the failures.
    states.sync(
        true,
        &[
            ControllerState::inactive(Hand::Left),
            ControllerState::inactive(Hand::Right),
        ],
    );
    assert_eq!(states.get("jump", ""), Some((false, XrActionValue::Bool(false))));
}

/// Catches publishing errors: an active false button is held at zero (not released), inactive
/// actions release, poses refresh each publish, and no injection resource is fine.
#[test]
fn publishing_holds_zero_for_an_active_release_and_refreshes_poses() {
    let mut states = states_for(&XrActionManifest::default());
    let mut injection = InputInjection::default();
    let mut poses = ActionPoses::default();
    let mut left = live_controller(Hand::Left);
    left.grip = PoseSample::at_rest(pose(0.1), 0);
    states.sync(true, &[left, ControllerState::inactive(Hand::Right)]);
    states.publish(Some(&mut injection), &mut poses);
    assert_eq!(
        injection.value_for("primary_left", 0),
        Some(Vec3::ZERO),
        "active but not pressed: driven to zero"
    );
    assert_eq!(
        injection.value_for("primary_right", 0),
        None,
        "the untracked hand is released"
    );
    assert_eq!(
        injection.value_for("menu", 0),
        Some(Vec3::ZERO),
        "a one-hand action keeps its plain name"
    );
    assert_eq!(poses.get("grip_pose", "left"), Some(pose(0.1)));
    assert_eq!(poses.get("grip_pose", "right"), None);
    assert_eq!(
        poses.poses.len(),
        4,
        "inactive poses are listed (inactive), not dropped"
    );
    assert_eq!(
        poses.poses[&("grip_pose".to_string(), "right".to_string())],
        (false, Pose::IDENTITY)
    );
    // The next publish replaces the pose table rather than adding to it.
    states.sync(false, &[left, ControllerState::inactive(Hand::Right)]);
    states.publish(None, &mut poses);
    assert_eq!(poses.get("grip_pose", "left"), None, "no longer active");
    assert_eq!(poses.poses.len(), 4);
    assert_eq!(
        injection.value_for("menu", 0),
        Some(Vec3::ZERO),
        "publish(None) leaves the injection alone"
    );
}

fn running_null() -> NullXr {
    let mut xr = NullXr::default();
    xr.simulate(SimEvent::RuntimeReady).expect("ready");
    xr.begin_session().expect("begin");
    xr
}

/// Catches haptic clamping and routing: negative durations floor at zero, non-finite amplitudes
/// are silent, the hand follows the sub-path, and the frequency is advisory only.
#[test]
fn haptic_sanitises_amplitude_and_duration_and_routes_by_sub_path() {
    let manifest = XrActionManifest::default();
    let mut xr = running_null();
    let r = regia_xr::input::haptic(
        &mut xr,
        &manifest,
        "haptic",
        "right",
        f32::NAN,
        -5,
        Some(f32::NAN),
    );
    assert_eq!(r, Ok(HapticOutcome::Played { amplitude: 0.0 }));
    let r = regia_xr::input::haptic(&mut xr, &manifest, "haptic", "left", 0.25, 2_000, Some(320.0));
    assert_eq!(r, Ok(HapticOutcome::Played { amplitude: 0.25 }));
    let r = regia_xr::input::haptic(&mut xr, &manifest, "haptic", "left", f32::INFINITY, 1, None);
    assert_eq!(r, Ok(HapticOutcome::Played { amplitude: 0.0 }));
    let pulses = xr.haptics();
    assert_eq!(pulses.len(), 3);
    assert_eq!(
        (pulses[0].hand, pulses[0].duration_ns, pulses[0].amplitude),
        (Hand::Right, 0, 0.0)
    );
    assert_eq!(
        (pulses[1].hand, pulses[1].duration_ns, pulses[1].amplitude),
        (Hand::Left, 2_000, 0.25)
    );
    assert_eq!(pulses[2].hand, Hand::Left);
}

/// Catches haptics on things that are not haptic outputs: a non-haptic action, an unknown
/// action, a hand the action lacks, and a non-hand sub-path are all `ActionUnknown`, and nothing is
/// sent.
#[test]
fn haptic_refuses_what_is_not_a_declared_haptic_output() {
    let m = manifest_with(
        vec![
            action("buzz", ActionType::Haptic, &["left"]),
            action("both", ActionType::Haptic, &["left", "right"]),
            action("select", ActionType::Float, &["left"]),
        ],
        vec![],
    );
    let mut xr = running_null();
    for (name, sub) in [
        ("buzz", "right"),
        ("buzz", ""),
        ("both", "head"),
        ("select", "left"),
        ("ghost", "left"),
    ] {
        let e = regia_xr::input::haptic(&mut xr, &m, name, sub, 1.0, 1, None).expect_err(name);
        assert_eq!(
            e,
            XrError::ActionUnknown {
                name: format!("{name}/{sub}")
            }
        );
    }
    assert!(xr.haptics().is_empty());
    regia_xr::input::haptic(&mut xr, &m, "buzz", "left", 1.0, 1, None).expect("the declared hand works");
    assert_eq!(xr.haptics().len(), 1);
}

/// Catches an error from the backend other than "unsupported" being swallowed: a session that is
/// not running reports its own error, not a quiet no-op.
#[test]
fn a_backend_error_other_than_unsupported_is_returned() {
    struct Failing;
    impl XrBackend for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn state(&self) -> regia_platform::xr::SessionState {
            regia_platform::xr::SessionState::Idle
        }
        fn poll_events(&mut self) -> Vec<regia_platform::xr::XrEvent> {
            Vec::new()
        }
        fn begin_session(&mut self) -> Result<(), XrError> {
            Ok(())
        }
        fn request_exit(&mut self) -> Result<(), XrError> {
            Ok(())
        }
        fn end_session(&mut self) -> Result<(), XrError> {
            Ok(())
        }
        fn wait_frame(&mut self) -> Result<regia_platform::xr::FrameTiming, XrError> {
            Err(XrError::RuntimeLost)
        }
        fn begin_frame(&mut self) -> Result<(), XrError> {
            Ok(())
        }
        fn locate_views(&mut self, _: i64) -> Result<[regia_platform::xr::View; 2], XrError> {
            Err(XrError::RuntimeLost)
        }
        fn head_sample(&self) -> PoseSample {
            PoseSample::default()
        }
        fn sync_actions(&mut self) -> Result<(), XrError> {
            Ok(())
        }
        fn controller(&self, hand: Hand) -> ControllerState {
            ControllerState::inactive(hand)
        }
        fn apply_haptic(&mut self, _: Hand, _: f32, _: i64) -> Result<(), XrError> {
            Err(XrError::RuntimeLost)
        }
        fn end_frame(&mut self, _: &regia_platform::xr::FrameSubmit) -> Result<(), XrError> {
            Ok(())
        }
    }
    let r = regia_xr::input::haptic(
        &mut Failing,
        &XrActionManifest::default(),
        "haptic",
        "left",
        1.0,
        1,
        None,
    );
    assert_eq!(r, Err(XrError::RuntimeLost));
}
