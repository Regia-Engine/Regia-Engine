// SPDX-License-Identifier: MPL-2.0
//! The three XR text formats under seeded mutation (ADR-1917, ADR-2043): each reader returns a value or an error,
//! never a panic or a hang. `regia_xr` is an optional dependency of `regia_agent`, so the registry test
//! (`regia_agent/tests/fuzz_targets.rs`) names these tests for `docs/formats/xr-*-ron.md` and `xrsim-ron.md`.
//! The fixtures are the examples on those pages.

use regia_test_utils::fuzz::fuzz_text_loader;
use regia_xr::{XrActionManifest, XrConfig, XrSimScript};

const CONFIG: &str = "(format_version: 1)";

const ACTIONS: &str = r#"(format_version: 1,
 sets: [(name: "gameplay", priority: 0, actions: [
   (name: "move", ty: Vec2, label: "xr-action-move", subs: ["left"]),
   (name: "grab", ty: Float, label: "xr-action-grab", subs: ["left", "right"]),
   (name: "grip_pose", ty: Pose, subs: ["left", "right"]),
   (name: "buzz", ty: Haptic, subs: ["left", "right"])])],
 bindings: [(profile: "simple_controller", map: [("grab", "left", "select/click")])])"#;

const SIM: &str = r#"(format_version: 1, fov_deg: 100.0, ipd_m: 0.063, refresh_hz: 90.0,
 ticks: [
  (tick: 0, head: (position: (0.0, 1.7, 0.0)), state: Some(Focused)),
  (tick: 10, head: (position: (0.0, 1.7, -0.2)), right: Some((position: (0.3, 1.2, -0.3))),
   inputs: [("grab", "right", Float(1.0))]),
 ])"#;

#[test]
fn xr_config_never_panics() {
    assert!(
        XrConfig::from_ron(CONFIG).is_ok(),
        "the documented example parses"
    );
    fuzz_text_loader(CONFIG, 0x5852_0001, 2000, |s| {
        XrConfig::from_ron(s).and_then(|config| config.to_ron())
    });
}

#[test]
fn xr_actions_never_panic() {
    assert!(
        XrActionManifest::from_ron(ACTIONS).is_ok(),
        "the documented example parses"
    );
    fuzz_text_loader(ACTIONS, 0x5852_0002, 2000, |s| {
        XrActionManifest::from_ron(s).and_then(|manifest| manifest.to_ron())
    });
}

#[test]
fn xrsim_never_panics() {
    assert!(
        XrSimScript::from_ron(SIM).is_ok(),
        "the documented example parses"
    );
    fuzz_text_loader(SIM, 0x5852_0003, 2000, |s| {
        XrSimScript::from_ron(s).and_then(|script| script.to_ron())
    });
}
