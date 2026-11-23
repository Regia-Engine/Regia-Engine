// SPDX-License-Identifier: MPL-2.0
//! [`json_layer`]: a ready-made `tracing` layer that writes one JSON object per line to a file
//! through a bounded queue on its own thread. Lossy by design: a full queue drops the line and
//! counts it, it never blocks the calling thread.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::TelemetryLevel;
use crate::rate::{RateLimiter, key};

/// Options of [`json_layer`].
#[derive(Clone, Debug)]
pub struct JsonLayerOptions {
    /// Bytes written before the writer keeps only warnings and errors (default 64 MiB). At
    /// twice this it stops writing altogether.
    pub max_bytes: u64,
    /// Lines the queue holds before new ones are dropped (default 4096).
    pub queue_lines: usize,
    /// `Verbose` writes everything the host's filter passes; `Normal` keeps warnings, errors
    /// and the targets in [`Self::always_targets`] (the session header and summaries); `Ship`
    /// keeps what `Normal` does except the `regia::boot` and `regia::hitch` targets.
    pub level: TelemetryLevel,
    /// Targets written at `Normal` whatever their level.
    pub always_targets: &'static [&'static str],
    /// The most verbose level written (default `DEBUG`: `TRACE` events are dropped before they
    /// are formatted, whatever the host's filter passes).
    pub max_level: Level,
    /// The writer flushes at most this often, and right away (at most once per this interval
    /// with an fsync) after a warning or error. Default 1 s.
    pub flush_interval: Duration,
    /// The same target and message within this window are written once; the next line after
    /// the window carries `"repeated": N`. Default 5 s; zero turns it off.
    pub repeat_window: Duration,
}

impl Default for JsonLayerOptions {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            queue_lines: 4096,
            level: TelemetryLevel::Verbose,
            always_targets: &[
                crate::target::SESSION,
                crate::target::FRAME_SUMMARY,
                crate::target::BOOT,
                crate::target::PHASE,
                crate::target::EXIT,
            ],
            max_level: Level::DEBUG,
            flush_interval: Duration::from_secs(1),
            repeat_window: Duration::from_secs(5),
        }
    }
}

/// Targets a `Ship` JSON layer never writes (ADR-8417): the boot stream and hitch records.
const SHIP_DROPPED_TARGETS: [&str; 2] = [crate::target::BOOT, crate::target::HITCH];

#[derive(Default)]
struct Stats {
    dropped: AtomicU64,
    written_lines: AtomicU64,
    written_bytes: AtomicU64,
    capped: AtomicBool,
    failed: AtomicBool,
}

enum Msg {
    Line { text: String, severe: bool },
    Flush,
}

/// The layer half of [`json_layer`]. Add it to the host's subscriber.
pub struct JsonLayer {
    tx: SyncSender<Msg>,
    stats: Arc<Stats>,
    opts: JsonLayerOptions,
    start: Instant,
    repeats: Mutex<RateLimiter>,
}

/// The guard half of [`json_layer`]: keep it alive for the session. Dropping it flushes what is
/// queued and stops the writer (waiting at most 2 s).
pub struct JsonLayerGuard {
    tx: Option<SyncSender<Msg>>,
    stats: Arc<Stats>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Counters of a [`json_layer`] writer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JsonLayerStats {
    /// Lines dropped because the queue was full.
    pub dropped: u64,
    /// Lines written.
    pub written_lines: u64,
    /// Bytes written.
    pub written_bytes: u64,
    /// The size cap was reached (only warnings and errors are written).
    pub capped: bool,
    /// Writing failed (disk full, permissions); the sink is off.
    pub failed: bool,
}

impl JsonLayerGuard {
    /// The writer's counters now.
    #[must_use]
    pub fn stats(&self) -> JsonLayerStats {
        snapshot(&self.stats)
    }

    /// Asks the writer to flush (non-blocking; skipped when the queue is full).
    pub fn flush(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(Msg::Flush);
        }
    }
}

impl JsonLayer {
    /// The writer's counters now.
    #[must_use]
    pub fn stats(&self) -> JsonLayerStats {
        snapshot(&self.stats)
    }
}

fn snapshot(stats: &Stats) -> JsonLayerStats {
    JsonLayerStats {
        dropped: stats.dropped.load(Ordering::Relaxed),
        written_lines: stats.written_lines.load(Ordering::Relaxed),
        written_bytes: stats.written_bytes.load(Ordering::Relaxed),
        capped: stats.capped.load(Ordering::Relaxed),
        failed: stats.failed.load(Ordering::Relaxed),
    }
}

impl Drop for JsonLayerGuard {
    fn drop(&mut self) {
        // Closing the channel ends the writer after it drains the queue.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// A non-blocking, bounded, lossy JSON-lines writer layer for `path` (created or truncated).
///
/// Each line is `{"t_ms", "wall", "level", "target", "msg", "fields": {…}, "span": […]}`; the
/// `data` field of a `regia::*` telemetry event is embedded as a JSON object. Wire it in one
/// line: `let (json, _guard) = regia_telemetry::json_layer(path, Default::default())?;` then
/// `registry().with(filter).with(json)`. See `book/user-guide/telemetry.md`.
pub fn json_layer(path: impl AsRef<Path>, opts: JsonLayerOptions) -> io::Result<(JsonLayer, JsonLayerGuard)> {
    let file = File::create(path.as_ref())?;
    *JSONL_PATH.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.as_ref().to_path_buf());
    let sync = file.try_clone().ok();
    spawn(Box::new(file), sync, opts)
}

static JSONL_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// The file the process's [`json_layer`] writes to (the newest one created): the default place
/// for `summary.json` is next to it, with the same stem.
#[must_use]
pub fn jsonl_path() -> Option<std::path::PathBuf> {
    JSONL_PATH.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// [`json_layer`] over any writer (tests, pipes). The writer runs on its own thread.
pub fn json_layer_to(
    sink: Box<dyn Write + Send>,
    opts: JsonLayerOptions,
) -> io::Result<(JsonLayer, JsonLayerGuard)> {
    spawn(sink, None, opts)
}

fn spawn(
    sink: Box<dyn Write + Send>,
    sync_file: Option<File>,
    opts: JsonLayerOptions,
) -> io::Result<(JsonLayer, JsonLayerGuard)> {
    let (tx, rx) = std::sync::mpsc::sync_channel(opts.queue_lines.max(1));
    let stats = Arc::new(Stats::default());
    let start = Instant::now();
    let thread = {
        let stats = stats.clone();
        let opts = opts.clone();
        std::thread::Builder::new()
            .name("regia-telemetry-json".into())
            .spawn(move || writer(rx, sink, sync_file.as_ref(), &stats, &opts, start))?
    };
    Ok((
        JsonLayer {
            tx: tx.clone(),
            stats: stats.clone(),
            repeats: Mutex::new(RateLimiter::new(opts.repeat_window)),
            opts,
            start,
        },
        JsonLayerGuard {
            tx: Some(tx),
            stats,
            thread: Some(thread),
        },
    ))
}

fn writer(
    rx: Receiver<Msg>,
    sink: Box<dyn Write + Send>,
    sync_file: Option<&File>,
    stats: &Stats,
    opts: &JsonLayerOptions,
    start: Instant,
) {
    let mut out = BufWriter::with_capacity(64 * 1024, sink);
    let mut bytes = 0u64;
    let mut last_flush = Instant::now();
    let mut last_sync: Option<Instant> = None;
    let mut dirty = false;
    let mut want_sync = false;
    let mut reported_drops = 0u64;
    let mut stopped = false;
    let mut failed = false;
    loop {
        let msg = rx.recv_timeout(opts.flush_interval);
        if failed {
            // The sink is off: keep draining so senders never see a full queue for long.
            if matches!(msg, Err(RecvTimeoutError::Disconnected)) {
                return;
            }
            continue;
        }
        let mut lines: [Option<String>; 2] = [None, None];
        match msg {
            Ok(Msg::Line { text, severe }) => {
                if !stopped && (bytes < opts.max_bytes || severe) {
                    if bytes >= opts.max_bytes && !stats.capped.swap(true, Ordering::Relaxed) {
                        lines[0] = Some(meta_line(
                            start,
                            "WARN",
                            "log size cap reached; only warnings and errors from here",
                            opts.max_bytes,
                        ));
                    }
                    if bytes >= opts.max_bytes.saturating_mul(2) {
                        stopped = true;
                        lines[1] = Some(meta_line(
                            start,
                            "WARN",
                            "log size hard cap reached; writing stopped",
                            bytes,
                        ));
                    } else {
                        lines[1] = Some(text);
                    }
                    want_sync |= severe;
                }
            }
            Ok(Msg::Flush) => {
                last_flush = Instant::now() - opts.flush_interval;
                want_sync = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let dropped = stats.dropped.load(Ordering::Relaxed);
                if dropped != reported_drops && !stopped {
                    let line = meta_line(start, "WARN", "dropped lines (queue full)", dropped);
                    let _ = write_line(&mut out, &line, &mut bytes, stats);
                }
                let _ = out.flush();
                return;
            }
        }
        let dropped = stats.dropped.load(Ordering::Relaxed);
        if dropped != reported_drops && last_flush.elapsed() >= opts.flush_interval && !stopped {
            let line = meta_line(start, "WARN", "dropped lines (queue full)", dropped);
            reported_drops = dropped;
            if write_line(&mut out, &line, &mut bytes, stats).is_err() {
                failed = true;
            }
        }
        for line in lines.iter().flatten() {
            if !failed && write_line(&mut out, line, &mut bytes, stats).is_err() {
                failed = true;
            }
            dirty = true;
        }
        if !failed && dirty && (last_flush.elapsed() >= opts.flush_interval || want_sync) {
            let sync_due = want_sync && last_sync.is_none_or(|t| t.elapsed() >= opts.flush_interval);
            if out.flush().is_err() || (sync_due && sync_file.is_some_and(|f| f.sync_data().is_err())) {
                failed = true;
            }
            if sync_due {
                last_sync = Some(Instant::now());
                want_sync = false;
            }
            last_flush = Instant::now();
            dirty = false;
        }
        if failed {
            stats.failed.store(true, Ordering::Relaxed);
            // Reported once; the subscriber may be this layer, so never `tracing` from here.
            eprintln!("regia_telemetry: the JSON log writer failed; the sink is off");
        }
    }
}

fn write_line(out: &mut impl Write, line: &str, bytes: &mut u64, stats: &Stats) -> io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    let n = line.len() as u64 + 1;
    *bytes += n;
    stats.written_lines.fetch_add(1, Ordering::Relaxed);
    stats.written_bytes.fetch_add(n, Ordering::Relaxed);
    Ok(())
}

fn meta_line(start: Instant, level: &str, msg: &str, value: u64) -> String {
    let mut s = String::with_capacity(200);
    let _ = write!(
        s,
        "{{\"t_ms\":{:.1},\"wall\":\"{}\",\"level\":\"{level}\",\"target\":\"{}\",\"msg\":",
        start.elapsed().as_secs_f64() * 1000.0,
        wall_clock(SystemTime::now()),
        crate::target::TELEMETRY,
    );
    escape_into(&mut s, msg);
    let _ = write!(s, ",\"fields\":{{\"value\":{value}}}}}");
    s
}

/// What makes two events "the same" for repeat suppression: target and message, plus the fields
/// for the one-off engine events (`regia::phase`, `regia::device_lost`) whose message is
/// constant but whose `data` is the point (two `scene_load_end`s with different results).
fn repeat_key(target: &str, message: &str, fields: &str) -> u64 {
    if target == crate::target::PHASE || target == crate::target::DEVICE_LOST {
        key(&[target.as_bytes(), message.as_bytes(), fields.as_bytes()])
    } else {
        key(&[target.as_bytes(), message.as_bytes()])
    }
}

impl<S> Layer<S> for JsonLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = *meta.level();
        let severe = level <= Level::WARN;
        let target = meta.target();
        if self.opts.level != TelemetryLevel::Verbose
            && !severe
            && !self.opts.always_targets.contains(&target)
        {
            return;
        }
        // Ship (ADR-8417) never writes the boot stream or hitch records, whatever emits them.
        if self.opts.level == TelemetryLevel::Ship && SHIP_DROPPED_TARGETS.contains(&target) {
            return;
        }
        if level > self.opts.max_level {
            return;
        }
        // Past the size cap only warnings and errors are written: skip formatting the rest.
        if self.stats.failed.load(Ordering::Relaxed) || (!severe && self.stats.capped.load(Ordering::Relaxed))
        {
            return;
        }
        let mut line = String::with_capacity(256);
        let _ = write!(
            line,
            "{{\"t_ms\":{:.1},\"wall\":\"{}\",\"level\":\"{}\",\"target\":",
            self.start.elapsed().as_secs_f64() * 1000.0,
            wall_clock(SystemTime::now()),
            level.as_str()
        );
        escape_into(&mut line, target);
        let mut visitor = JsonVisitor {
            fields: String::with_capacity(128),
            message: String::new(),
            // The large periodic engine events carry `data` as an object only: nothing legacy
            // reads their string, and writing both would double the log's growth.
            keep_data_json: target != crate::target::FRAME_SUMMARY && target != crate::target::HITCH,
        };
        event.record(&mut visitor);
        if !self.opts.repeat_window.is_zero() {
            let k = repeat_key(target, &visitor.message, &visitor.fields);
            let mut repeats = self.repeats.lock().unwrap_or_else(PoisonError::into_inner);
            match repeats.check(k, Instant::now()) {
                None => return,
                Some(0) => {}
                Some(n) => {
                    visitor.sep();
                    let _ = write!(visitor.fields, "\"repeated\":{n}");
                }
            }
        }
        line.push_str(",\"msg\":");
        escape_into(&mut line, &visitor.message);
        line.push_str(",\"fields\":{");
        line.push_str(&visitor.fields);
        line.push_str("},\"span\":[");
        if let Some(scope) = ctx.event_scope(event) {
            for (i, span) in scope.from_root().enumerate() {
                if i > 0 {
                    line.push(',');
                }
                escape_into(&mut line, span.name());
            }
        }
        line.push_str("]}");
        match self.tx.try_send(Msg::Line { text: line, severe }) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct JsonVisitor {
    fields: String,
    message: String,
    /// Also write a nested `data` object's text as `data_json` (v0.1.9 compatibility).
    keep_data_json: bool,
}

impl JsonVisitor {
    fn sep(&mut self) {
        if !self.fields.is_empty() {
            self.fields.push(',');
        }
    }

    fn name(&mut self, field: &Field) {
        self.sep();
        escape_into(&mut self.fields, field.name());
        self.fields.push(':');
    }
}

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        // The user's home directory never reaches the log (spec 28 § 5.8).
        let value = regia_core::privacy::scrub_home(value);
        if field.name() == "message" {
            value.into_owned().clone_into(&mut self.message);
            return;
        }
        self.name(field);
        if field.name() == "data" && is_json_document(&value) {
            // `data` is a nested object; the text it came from stays as `data_json` for one
            // release (v0.1.9) for parsers that still read the string.
            self.fields.push_str(&value);
            if self.keep_data_json {
                self.fields.push_str(",\"data_json\":");
                escape_into(&mut self.fields, &value);
            }
        } else {
            escape_into(&mut self.fields, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        self.record_str(field, &text);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.name(field);
        if value.is_finite() {
            let _ = write!(self.fields, "{value}");
        } else {
            self.fields.push_str("null");
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.name(field);
        let _ = write!(self.fields, "{value}");
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.name(field);
        let _ = write!(self.fields, "{value}");
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.name(field);
        self.fields.push_str(if value { "true" } else { "false" });
    }
}

/// A `data` value that is a JSON object or array (telemetry builds it with `serde_json`; a host
/// may use any serializer) is embedded as is; anything else, including text that only looks like
/// JSON, is written as a string.
fn is_json_document(value: &str) -> bool {
    let v = value.trim();
    ((v.starts_with('{') && v.ends_with('}')) || (v.starts_with('[') && v.ends_with(']')))
        && serde_json::from_str::<serde::de::IgnoredAny>(v).is_ok()
}

/// Appends `s` as a JSON string literal.
pub(crate) fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`. Civil-from-days after Howard Hinnant's public-domain
/// `chrono`-compatible date algorithms (docs/provenance.md).
pub(crate) fn wall_clock(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let ms = since.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_matches_known_dates() {
        let t = UNIX_EPOCH + Duration::from_millis(1_791_547_200_123); // 2026-10-09T12:00:00.123Z
        assert_eq!(wall_clock(t), "2026-10-09T12:00:00.123Z");
        assert_eq!(wall_clock(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&wall_clock(leap)[..10], "2000-02-29");
    }

    #[test]
    fn phase_events_with_different_data_are_not_repeats() {
        let phase = crate::target::PHASE;
        assert_ne!(
            repeat_key(phase, "phase: scene_load_end", "\"data\":\"a\""),
            repeat_key(phase, "phase: scene_load_end", "\"data\":\"b\"")
        );
        assert_eq!(
            repeat_key(phase, "phase: scene_load_end", "\"data\":\"a\""),
            repeat_key(phase, "phase: scene_load_end", "\"data\":\"a\"")
        );
        // Other targets keep the message-only key.
        assert_eq!(repeat_key("x", "m", "1"), repeat_key("x", "m", "2"));
    }

    #[test]
    fn escapes_control_characters() {
        let mut s = String::new();
        escape_into(&mut s, "a\"b\\c\nd\u{1}");
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert!(serde_json::from_str::<String>(&s).is_ok());
    }
}
