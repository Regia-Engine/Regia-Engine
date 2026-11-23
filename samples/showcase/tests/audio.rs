// SPDX-License-Identifier: MIT-0
//! The headless showcase has a headless mixer (ADR-1561): the `Audio` resource exists, so
//! `agent.hello` lists the render-out audio markers and `render.out_start { audio }` is not refused
//! with `F0013`. Without any GPU adapter the tests print a note and pass, like the other render
//! tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{JobSpec, run};
use regia_render::RenderDevice;
use serde_json::json;
use showcase::app::{Options, build_headless_app};
use showcase::scene::load_scene;

#[test]
fn the_headless_app_has_a_headless_mixer() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase audio test: {err}");
        return;
    }
    let scene = Arc::new(
        load_scene(&Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")).expect("example scene"),
    );
    let app = build_headless_app(
        scene,
        Options {
            plain: true,
            ..Options::default()
        },
        None,
    )
    .expect("headless app");
    assert!(
        app.world().get_resource::<regia::audio::Audio>().is_some(),
        "the headless showcase has an Audio resource"
    );
    assert!(
        app.world().resource::<regia::audio::AudioSettings>().headless,
        "the mixer opens no output device"
    );
}

#[test]
fn agent_hello_lists_the_render_out_audio_markers() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase audio test: {err}");
        return;
    }
    let outcome = run(
        1,
        &JobSpec {
            frames: 1,
            after: vec![("agent.hello".to_owned(), json!({}))],
            ..JobSpec::default()
        },
    );
    let hello = outcome.reports.first().expect("agent.hello answer");
    let capabilities: Vec<&str> = hello["capabilities"]
        .as_array()
        .expect("capabilities")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    for marker in ["render_out_audio", "render_out_audio_stems"] {
        assert!(capabilities.contains(&marker), "{marker} in {capabilities:?}");
    }
}
