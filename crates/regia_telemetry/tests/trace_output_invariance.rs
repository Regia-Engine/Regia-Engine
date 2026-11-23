// SPDX-License-Identifier: MPL-2.0
//! Spec 54 § 7 and test 9 (GPU; lavapipe counts): breadcrumbs *and* a running trace recording
//! (`marks`, `frames`, `passes`) never change the rendered output. Ten frames of the light and
//! shadows lab scene with both on, then ten with the writer off and the recorder idle: every
//! readback is byte-identical (two runs compared, no golden). Skips without an adapter.
//!
//! Its own test binary: the breadcrumb writer and the process recorder are global state.

use regia_render::RenderDevice;
use regia_render::breadcrumbs::{self, BreadcrumbConfig};
use regia_telemetry::trace::{EventKind, RecorderState, TraceConfig, configure, read_trace, recorder};
use regia_test_utils::scenes::render_light_shadows;
use regia_test_utils::unique_temp_dir;

const FRAMES: usize = 10;

#[test]
fn breadcrumbs_and_recording_never_change_the_output() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping: no adapter ({e})");
            return;
        }
    };
    // Warm-up so lazily built state is the same for every compared frame.
    let _ = render_light_shadows(&device).expect("warm-up");

    let dir = unique_temp_dir("trace_output_invariance");
    configure(TraceConfig {
        dir: dir.clone(),
        ..TraceConfig::default()
    });
    breadcrumbs::configure(BreadcrumbConfig::default());
    recorder().start(Some("invariance"), None).expect("start");
    assert_eq!(recorder().state(), RecorderState::Writing);
    let on: Vec<Vec<u8>> = (0..FRAMES)
        .map(|_| render_light_shadows(&device).expect("render"))
        .collect();
    recorder().stop().expect("stop");

    breadcrumbs::configure(BreadcrumbConfig {
        enabled: false,
        ..BreadcrumbConfig::default()
    });
    assert_ne!(recorder().state(), RecorderState::Writing);
    let off: Vec<Vec<u8>> = (0..FRAMES)
        .map(|_| render_light_shadows(&device).expect("render"))
        .collect();
    breadcrumbs::configure(BreadcrumbConfig::default());

    for (i, frame) in on.iter().chain(&off).enumerate() {
        assert!(*frame == on[0], "frame {i} differs from the first recorded frame");
    }
    // The recording really ran: the file holds the frames' breadcrumb passes.
    let trace = read_trace(&dir.join("invariance.regiatrace")).expect("reads");
    assert!(
        trace
            .events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::PassOpen { name, .. } if &**name == "opaque")),
        "the passes stream recorded the opaque pass"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
