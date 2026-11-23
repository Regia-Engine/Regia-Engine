// SPDX-License-Identifier: MPL-2.0
//! A `regia::phase` call is also recorded as a `phase:<name>` trace mark (spec 54 § 2).
//! Own test binary: the process recorder is global state.

use regia_telemetry::trace::{TraceConfig, configure, read_trace, recorder};
use regia_test_utils::unique_temp_dir;
use serde_json::json;

#[test]
fn a_phase_during_a_running_trace_is_recorded_as_a_mark() {
    let dir = unique_temp_dir("phase_mark");
    configure(TraceConfig {
        dir: dir.clone(),
        ..TraceConfig::default()
    });
    recorder().start(Some("p"), None).expect("start");
    regia_telemetry::phase("match_start", json!({}));
    recorder().stop().expect("stop");
    let trace = read_trace(&dir.join("p.regiatrace")).expect("reads");
    let text = format!("{:?}", trace.events);
    assert!(text.contains("phase:match_start"), "{text}");
}
