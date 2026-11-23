// SPDX-License-Identifier: MPL-2.0
//! `json_layer`: one JSON object per line, `data` embedded as an object, lossy (drops and counts
//! when the queue is full, never blocks), size cap, `Normal` level and repeat suppression.

use std::io::{self, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_telemetry::{JsonLayerOptions, TelemetryLevel, json_layer, json_layer_to};
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;

/// A sink that blocks every write until the test releases it (a stalled disk).
struct Gate {
    open: Receiver<()>,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Write for Gate {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.open.recv();
        self.out.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

#[test]
fn full_queue_drops_and_counts_and_never_blocks() {
    let (release, open): (Sender<()>, Receiver<()>) = channel();
    let out = Arc::new(Mutex::new(Vec::new()));
    let (layer, guard) = json_layer_to(
        Box::new(Gate {
            open,
            out: out.clone(),
        }),
        JsonLayerOptions {
            queue_lines: 8,
            repeat_window: Duration::ZERO,
            ..JsonLayerOptions::default()
        },
    )
    .expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    let mut elapsed = Duration::ZERO;
    tracing::subscriber::with_default(subscriber, || {
        // A warning makes the writer flush; the flush blocks on the stalled disk.
        tracing::warn!(target: "do27::test", "first");
        std::thread::sleep(Duration::from_millis(200));
        let start = Instant::now();
        for i in 0..1000 {
            tracing::info!(target: "do27::test", i, "line {i}");
        }
        elapsed = start.elapsed();
    });
    assert!(
        elapsed < Duration::from_millis(500),
        "logging blocked for {elapsed:?}"
    );
    let stats = guard.stats();
    assert!(stats.dropped >= 900, "dropped {}", stats.dropped);
    // Let the writer go: what was queued is written, and the drop count is reported.
    for _ in 0..4096 {
        let _ = release.send(());
    }
    drop(guard);
    let written = lines(&out.lock().expect("lock"));
    assert!(
        !written.is_empty() && written.len() < 100,
        "{} lines",
        written.len()
    );
    assert!(
        written
            .iter()
            .any(|l| l["target"] == "regia::telemetry"
                && l["msg"].as_str().is_some_and(|m| m.contains("dropped"))),
        "no drop report"
    );
}

#[test]
fn lines_have_the_documented_shape_and_data_is_embedded() {
    let sink = Shared::default();
    let (layer, guard) =
        json_layer_to(Box::new(sink.clone()), JsonLayerOptions::default()).expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let _s = tracing::info_span!("level_load").entered();
        tracing::info!(target: "regia::frame_summary", frames = 600u64, data = %r#"{"frame_ms":{"p50":16.6}}"#, "frame summary");
        tracing::warn!(target: "do27::net", rtt_ms = 41.5, peer = "a\"b", "high rtt");
        tracing::info!(target: "do27::ui", data = %"{not json from a game", "game data field stays a string");
    });
    drop(guard);
    let l = lines(&sink.0.lock().expect("lock"));
    assert_eq!(l.len(), 3);
    for line in &l {
        for key in ["t_ms", "wall", "level", "target", "msg", "fields", "span"] {
            assert!(line.get(key).is_some(), "{key} missing in {line}");
        }
    }
    assert_eq!(l[0]["fields"]["data"]["frame_ms"]["p50"], 16.6);
    assert_eq!(l[0]["fields"]["frames"], 600);
    assert_eq!(l[0]["span"][0], "level_load");
    assert_eq!(l[1]["level"], "WARN");
    assert_eq!(l[1]["fields"]["peer"], "a\"b");
    assert_eq!(l[2]["fields"]["data"], "{not json from a game");
}

#[test]
fn size_cap_keeps_only_warnings_then_stops() {
    let sink = Shared::default();
    let (layer, guard) = json_layer_to(
        Box::new(sink.clone()),
        JsonLayerOptions {
            max_bytes: 2000,
            repeat_window: Duration::ZERO,
            ..JsonLayerOptions::default()
        },
    )
    .expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        for i in 0..200 {
            tracing::info!(target: "do27::spam", i, "info {i}");
            if i % 20 == 0 {
                tracing::error!(target: "do27::err", i, "error {i}");
            }
            // Keep the queue from overflowing in this test.
            std::thread::sleep(Duration::from_micros(200));
        }
    });
    drop(guard);
    let bytes = sink.0.lock().expect("lock").len() as u64;
    let l = lines(&sink.0.lock().expect("lock"));
    assert!(bytes <= 2 * 2000 + 400, "{bytes} bytes written");
    let cap_at = l
        .iter()
        .position(|x| x["msg"].as_str().is_some_and(|m| m.contains("size cap")))
        .expect("cap marker");
    assert!(
        l[cap_at + 1..]
            .iter()
            .all(|x| x["level"] == "ERROR" || x["level"] == "WARN"),
        "only warnings and errors after the cap"
    );
}

#[test]
fn normal_level_and_repeats() {
    let sink = Shared::default();
    let (layer, guard) = json_layer_to(
        Box::new(sink.clone()),
        JsonLayerOptions {
            level: TelemetryLevel::Normal,
            ..JsonLayerOptions::default()
        },
    )
    .expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "regia::hitch", "hitch: dropped at normal");
        tracing::debug!(target: "regia_render", "debug: dropped at normal");
        tracing::info!(target: "regia::session", "session: kept");
        tracing::info!(target: "regia::frame_summary", "summary: kept");
        for _ in 0..50 {
            tracing::warn!(target: "do27::audio", "underrun");
        }
    });
    drop(guard);
    let l = lines(&sink.0.lock().expect("lock"));
    let msgs: Vec<&str> = l.iter().filter_map(|x| x["msg"].as_str()).collect();
    assert_eq!(msgs, ["session: kept", "summary: kept", "underrun"], "{msgs:?}");
}

#[test]
fn json_layer_writes_a_file() {
    let dir = std::env::temp_dir().join(format!("regia-telemetry-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("session.jsonl");
    let (layer, guard) = json_layer(&path, JsonLayerOptions::default()).expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(target: "regia::gpu_error", kind = "validation", "wgpu validation error");
    });
    drop(guard);
    let l = lines(&std::fs::read(&path).expect("read jsonl"));
    assert_eq!(l[0]["fields"]["kind"], "validation");
    let _ = std::fs::remove_dir_all(&dir);
}

fn event_lines(out: &Shared, emit: impl FnOnce()) -> Vec<Value> {
    let (layer, guard) = json_layer_to(Box::new(out.clone()), JsonLayerOptions::default()).expect("layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, emit);
    drop(guard);
    lines(&out.0.lock().expect("lock"))
}

#[test]
fn data_is_a_nested_object_and_the_old_string_stays_as_data_json() {
    let out = Shared::default();
    let text = r#"{"phase":"match_start","name":"bore_twelve","n":[1,2]}"#;
    let rows = event_lines(&out, || {
        tracing::info!(target: "regia::phase", data = %text, "phase: match_start");
        tracing::info!(target: "do27::match", data = %text, "a host event too");
        tracing::info!(target: "do27::note", data = "not json {", "text that only looks like data");
    });
    assert_eq!(rows.len(), 3);
    for row in &rows[..2] {
        let f = &row["fields"];
        assert_eq!(f["data"]["name"], "bore_twelve", "{row}");
        assert_eq!(f["data"]["n"][1], 2);
        assert_eq!(f["data_json"], text, "the old string is kept for one release");
        assert_eq!(
            serde_json::from_str::<Value>(f["data_json"].as_str().unwrap_or("")).ok(),
            Some(f["data"].clone())
        );
    }
    assert_eq!(rows[2]["fields"]["data"], "not json {");
    assert!(rows[2]["fields"].get("data_json").is_none());
}

#[test]
fn the_large_periodic_events_skip_the_data_json_copy() {
    let out = Shared::default();
    let text = r#"{"frames":10}"#;
    let rows = event_lines(&out, || {
        tracing::info!(target: "regia::frame_summary", data = %text, "summary");
        tracing::info!(target: "regia::hitch", data = %text, "hitch");
    });
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row["fields"]["data"]["frames"], 10, "{row}");
        assert!(row["fields"].get("data_json").is_none(), "{row}");
    }
}

#[test]
fn the_home_directory_never_reaches_the_log() {
    let Some(home) = regia_core::brand::env_var("HOME")
        .ok()
        .filter(|h| h.len() > 3 && h.starts_with('/'))
    else {
        return; // Windows or no HOME: the scrubbing itself is unit tested.
    };
    let out = Shared::default();
    let rows = event_lines(&out, || {
        tracing::warn!(target: "regia::stream", path = %format!("{home}/x/y.pak"), data = %format!(r#"{{"file":"{home}/a.bin"}}"#), "cannot read {home}/data/map.pak");
    });
    let line = rows[0].to_string();
    assert!(!line.contains(&home), "the home path leaked: {line}");
    assert_eq!(rows[0]["msg"], "cannot read ~/data/map.pak");
    assert_eq!(rows[0]["fields"]["path"], "~/x/y.pak");
    assert_eq!(rows[0]["fields"]["data"]["file"], "~/a.bin");
}

/// ADR-8417: a Ship JSON layer keeps what Normal keeps except the boot stream and hitch records
/// (a severe hitch's warning included).
#[test]
fn ship_level_drops_boot_and_hitch_targets() {
    let sink = Shared::default();
    let (layer, guard) = json_layer_to(
        Box::new(sink.clone()),
        JsonLayerOptions {
            level: TelemetryLevel::Ship,
            ..JsonLayerOptions::default()
        },
    )
    .expect("json layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(target: "regia::hitch", "hitch: dropped at ship");
        tracing::info!(target: "regia::boot", "boot: dropped at ship");
        tracing::info!(target: "regia_render", "info: dropped at ship");
        tracing::info!(target: "regia::session", "session: kept");
        tracing::info!(target: "regia::frame_summary", "summary: kept");
        tracing::warn!(target: "do27::audio", "underrun");
    });
    drop(guard);
    let l = lines(&sink.0.lock().expect("lock"));
    let msgs: Vec<&str> = l.iter().filter_map(|x| x["msg"].as_str()).collect();
    assert_eq!(msgs, ["session: kept", "summary: kept", "underrun"], "{msgs:?}");
}
