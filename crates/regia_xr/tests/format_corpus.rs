// SPDX-License-Identifier: MPL-2.0
//! The three XR text formats against the old-bytes corpus (ADR-2743): `xr-config`, `xr-actions` and `xrsim` in
//! `crates/regia_agent/tests/fixtures/format-corpus/`. `regia_xr` is optional there (`COVERED_ELSEWHERE`), so the four
//! checks run here.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_xr::{
    MANIFEST_FORMAT_VERSION, SIM_FORMAT_VERSION, XR_CONFIG_FORMAT_VERSION, XrActionManifest, XrConfig,
    XrSimScript,
};

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

fn text(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|e| e.to_string())
}

fn config(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let c = XrConfig::from_ron(text(bytes)?).map_err(|e| e.to_string())?;
    c.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

fn actions(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let m = XrActionManifest::from_ron(text(bytes)?).map_err(|e| e.to_string())?;
    m.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

fn sim(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let s = XrSimScript::from_ron(text(bytes)?).map_err(|e| e.to_string())?;
    s.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    let src =
        "frozen copy of the fixture text of crates/regia_xr/tests/fuzz.rs (the example on the format page)";
    format_corpus_support::bless_current(
        "xr-config",
        XR_CONFIG_FORMAT_VERSION,
        "ron",
        CONFIG.as_bytes(),
        src,
    );
    format_corpus_support::bless_current(
        "xr-actions",
        MANIFEST_FORMAT_VERSION,
        "ron",
        ACTIONS.as_bytes(),
        src,
    );
    format_corpus_support::bless_current("xrsim", SIM_FORMAT_VERSION, "ron", SIM.as_bytes(), src);
}

#[test]
fn every_xr_fixture_loads_round_trips_and_refuses_the_future() {
    let mut problems = format_corpus_support::check_format("xr-config", XR_CONFIG_FORMAT_VERSION, &config);
    problems.extend(format_corpus_support::check_format(
        "xr-actions",
        MANIFEST_FORMAT_VERSION,
        &actions,
    ));
    problems.extend(format_corpus_support::check_format(
        "xrsim",
        SIM_FORMAT_VERSION,
        &sim,
    ));
    assert!(problems.is_empty(), "{problems:#?}");
}
