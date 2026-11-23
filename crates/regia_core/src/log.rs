// SPDX-License-Identifier: MPL-2.0
//! Logging (`docs/specs/10-core.md` § 1.3): `tracing` with an env filter, console output, an optional
//! file sink keeping the last N session logs, logcat on Android, and a panic hook that logs and flushes.
//!
//! Records from crates that use the `log` facade (wgpu, android-activity) are forwarded into `tracing`.
//!
//! Logging never blocks the frame thread on the console: when stderr is not a terminal (a pipe a
//! launcher may never read), console lines go through [`console::NonBlockingConsole`], a bounded
//! lossy queue with its own writer thread.

pub mod console;
pub mod devenv;
mod file;
#[cfg(target_os = "android")]
mod logcat;
pub mod ring;
pub mod session;
mod tick;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

pub use console::NonBlockingConsole;
pub use file::{FileSink, civil_date_time};
pub use tick::{TickFormat, set_tick, tick};

use crate::profiling::FrameProfiler;

/// Environment variable that overrides [`LogConfig::filter`].
pub const FILTER_ENV: &str = "REGIA_LOG";

/// Environment variable naming a JSON-lines log file. Empty means no JSON log. Honored by
/// `regia::run` before the subscriber is installed (ADR-3968).
pub const JSON_LOG_ENV: &str = "REGIA_JSON_LOG";

/// An extra `tracing` layer installed on the root registry, before the filter and the other sinks.
pub type ExtraLogLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Boxes `layer` as an [`ExtraLogLayer`].
#[must_use]
pub fn box_extra_layer<L>(layer: L) -> ExtraLogLayer
where
    L: Layer<Registry> + Send + Sync + 'static,
{
    Box::new(layer)
}

/// Default filter: engine at `info`, chatty GPU crates at `warn`, the font database at `error`
/// (v0.5.1: its per-file "malformed font" warnings name a user's font paths; `regia_ui` counts
/// them into one `debug` line per launch instead).
pub const DEFAULT_FILTER: &str = "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,fontdb=error";

/// Whether the process subscriber keeps a callsite (ADR-4933).
///
/// `bevy_ecs` opens helper spans on every run: `schedule` and `multithreaded executor` per schedule
/// run, `check_conditions` per system per run on the single-threaded executor (with the system's
/// name copied into a new `String`), and a `par_for_each` span per new `QueryState` (with the query's
/// type names). [`FrameProfiler`] ignores them and the console and file layers would only format
/// their fields, so they cost a registry slot, an allocation and a field format each while nothing
/// reads them. On a phone that was 8 % of the main thread. They are dropped for every layer; the
/// per-system `system` span and `system_commands` (created once per system, timed by the profiler,
/// the context of every log line a system writes) are kept, as are all events and every other
/// crate's spans. With the `tracy` feature nothing is dropped, so Tracy still shows schedules.
#[must_use]
pub fn keeps_callsite(metadata: &tracing::Metadata<'_>) -> bool {
    if cfg!(feature = "tracy") || !metadata.is_span() {
        return true;
    }
    let target = metadata.target();
    let bevy_ecs = target == "bevy_ecs" || target.starts_with("bevy_ecs::");
    !bevy_ecs || matches!(metadata.name(), "system" | "system_commands")
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// `tracing` filter directives, e.g. `info,regia_render=debug`. Overridden by `REGIA_LOG`.
    pub filter: String,
    /// Directory for session log files (`user://logs` once the VFS is up). `None` disables the file sink.
    pub file_dir: Option<PathBuf>,
    /// How many session log files to keep in `file_dir`.
    pub max_files: usize,
    /// Install the panic hook that logs the panic with a backtrace and flushes the sinks.
    pub panic_hook: bool,
    /// Frame profiler to install as a layer, so every span is timed (`docs/specs/10-core.md` § 1.3).
    /// Keep a clone to read the results.
    pub profiler: Option<FrameProfiler>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: DEFAULT_FILTER.into(),
            file_dir: None,
            max_files: 10,
            panic_hook: true,
            profiler: None,
        }
    }
}

/// Errors from [`init`].
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The filter string could not be parsed.
    #[error("invalid log filter {filter:?}: {source}")]
    Filter {
        /// The rejected filter.
        filter: String,
        /// Parser error.
        source: tracing_subscriber::filter::ParseError,
    },
    /// The log directory or file could not be created.
    #[error("cannot open log file in {dir}: {source}")]
    File {
        /// Directory that was used.
        dir: PathBuf,
        /// IO error.
        source: std::io::Error,
    },
}

/// Longest [`LogGuard::flush`] waits for queued console lines to reach stderr (a stuck pipe must
/// not hang the exit or the panic hook).
pub const CONSOLE_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// Keeps the file sink alive; flushes it when dropped. Hold it for the lifetime of the app.
#[derive(Debug, Clone)]
pub struct LogGuard {
    file: Option<FileSink>,
    console: Option<NonBlockingConsole>,
}

impl LogGuard {
    /// Flushes buffered log output to disk, and waits (at most [`CONSOLE_FLUSH_TIMEOUT`]) for
    /// queued console lines to be written.
    pub fn flush(&self) {
        if let Some(console) = &self.console {
            console.flush_timeout(CONSOLE_FLUSH_TIMEOUT);
        }
        if let Some(file) = &self.file {
            file.flush();
        }
    }

    /// Console lines dropped because stderr was not being read (always 0 when stderr is a
    /// terminal or on Android).
    #[must_use]
    pub fn console_dropped(&self) -> u64 {
        self.console.as_ref().map_or(0, NonBlockingConsole::dropped)
    }

    /// Path of this session's log file, if the file sink is enabled.
    pub fn file_path(&self) -> Option<&std::path::Path> {
        self.file.as_ref().map(FileSink::path)
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.flush();
    }
}

static INSTALLED: OnceLock<LogGuard> = OnceLock::new();

/// Serialises `init` so two overlapping first calls cannot both prune logs and install the panic hook.
static INIT_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Initialises logging once per process. Later calls (Android may run `android_main` again after an
/// activity restart) return the guard from the first call.
pub fn init(config: LogConfig) -> Result<LogGuard, LogError> {
    init_with_layer(config, None)
}

/// [`init`] plus an optional extra layer on the root registry (a JSON log, for example).
///
/// The extra layer is attached before the filter and the console and file sinks. A later call in
/// the same process returns the first guard and does not install `extra`.
pub fn init_with_layer(config: LogConfig, extra: Option<ExtraLogLayer>) -> Result<LogGuard, LogError> {
    if let Some(guard) = INSTALLED.get() {
        return Ok(guard.clone());
    }
    let _init = INIT_LOCK.lock();
    if let Some(guard) = INSTALLED.get() {
        return Ok(guard.clone());
    }
    let filter_text = crate::brand::env_var(FILTER_ENV).unwrap_or_else(|_| config.filter.clone());
    // Directives naming engine crates by their names from before the rename keep matching
    // (ADR-1429).
    let filter_text = crate::brand::current_log_filter(&filter_text).into_owned();
    let filter = EnvFilter::builder()
        .parse(&filter_text)
        .map_err(|source| LogError::Filter {
            filter: filter_text.clone(),
            source,
        })?;

    let file = match &config.file_dir {
        Some(dir) => Some(
            FileSink::create(dir, config.max_files).map_err(|source| LogError::File {
                dir: dir.clone(),
                source,
            })?,
        ),
        None => None,
    };

    let file_layer = file.clone().map(|sink| {
        fmt::layer()
            .with_ansi(false)
            .event_format(TickFormat(fmt::format().with_ansi(false)))
            .with_writer(sink)
            .boxed()
    });
    #[cfg(not(target_os = "android"))]
    let stderr_is_terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());
    // Not a terminal: possibly a pipe nobody drains. Never let a full pipe block the frame.
    #[cfg(not(target_os = "android"))]
    let console = if stderr_is_terminal {
        None
    } else {
        NonBlockingConsole::spawn(std::io::stderr(), console::CONSOLE_QUEUE_LINES).ok()
    };
    #[cfg(target_os = "android")]
    let console: Option<NonBlockingConsole> = None;
    #[cfg(not(target_os = "android"))]
    let console_layer = match &console {
        Some(writer) => fmt::layer()
            .with_ansi(false)
            .event_format(TickFormat(fmt::format().with_ansi(false)))
            .with_writer(writer.clone())
            .boxed(),
        None => fmt::layer()
            .with_ansi(stderr_is_terminal)
            .event_format(TickFormat(fmt::format().with_ansi(stderr_is_terminal)))
            .with_writer(std::io::stderr)
            .boxed(),
    };
    #[cfg(target_os = "android")]
    let console_layer = fmt::layer()
        .with_ansi(false)
        .event_format(TickFormat(
            fmt::format()
                .with_ansi(false)
                .without_time()
                .with_level(false)
                .with_target(false),
        ))
        .with_writer(logcat::Logcat)
        .boxed();

    #[cfg(feature = "tracy")]
    let tracy_layer = Some(tracing_tracy::TracyLayer::default());
    #[cfg(not(feature = "tracy"))]
    let tracy_layer: Option<tracing_subscriber::layer::Identity> = None;

    // `try_init` also installs the `log` -> `tracing` bridge. It fails only if another subscriber is
    // already set (for example by a test harness); logging then goes there, which is fine.
    let _ = tracing_subscriber::registry()
        .with(extra)
        .with(filter)
        .with(tracing_subscriber::filter::filter_fn(keeps_callsite))
        .with(ring::RingLayer)
        .with(console_layer)
        .with(file_layer)
        .with(config.profiler.clone())
        .with(tracy_layer)
        .try_init();

    let guard = LogGuard { file, console };
    if config.panic_hook {
        install_panic_hook(guard.clone());
    }
    let guard = INSTALLED.get_or_init(|| guard).clone();
    tracing::info!(target: "regia_core", "logging started (filter: {filter_text})");
    Ok(guard)
}

static PANIC_CALLBACK: OnceLock<fn()> = OnceLock::new();

/// Registers a function the panic hook calls after it logged the panic and before it flushes
/// (telemetry writes `summary.json` with it). Once per process; returns `false` when one is
/// already set. The function runs inside the panic hook: it must not panic or block for long.
pub fn set_panic_callback(callback: fn()) -> bool {
    PANIC_CALLBACK.set(callback).is_ok()
}

fn install_panic_hook(guard: LogGuard) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::telemetry::add(crate::telemetry::Counter::Panics, 1);
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(target: "panic", "{info}\nbacktrace:\n{backtrace}");
        if let Some(callback) = PANIC_CALLBACK.get() {
            callback();
        }
        guard.flush();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names of the spans a layer behind [`keeps_callsite`] sees created.
    #[derive(Clone, Default)]
    struct SeenSpans(std::sync::Arc<parking_lot::Mutex<Vec<&'static str>>>);

    impl<S: tracing::Subscriber> Layer<S> for SeenSpans {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0.lock().push(attrs.metadata().name());
        }
    }

    #[test]
    fn per_run_bevy_ecs_helper_spans_are_dropped_and_the_rest_kept() {
        let seen = SeenSpans::default();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::filter_fn(keeps_callsite))
                .with(seen.clone()),
        );
        let built = std::cell::Cell::new(0);
        tracing::dispatcher::with_default(&dispatch, || {
            let _a = tracing::info_span!(target: "bevy_ecs::schedule::executor::single_threaded", "check_conditions",
                name = { built.set(built.get() + 1); "x".to_string() })
            .entered();
            let _b = tracing::info_span!(target: "bevy_ecs::schedule::schedule", "schedule").entered();
            let _c = tracing::info_span!(target: "bevy_ecs::query::state", "par_for_each").entered();
            let _d = tracing::info_span!(target: "bevy_ecs::system::function_system", "system", name = "s")
                .entered();
            let _e =
                tracing::info_span!(target: "bevy_ecs::system::function_system", "system_commands").entered();
            let _f = tracing::info_span!(target: "bevy_ecsx", "schedule").entered();
            let _g = tracing::info_span!(target: "regia_ai", "state_tree_tick").entered();
        });
        let seen = seen.0.lock().clone();
        if cfg!(feature = "tracy") {
            assert_eq!(seen.len(), 7, "{seen:?}");
        } else {
            assert_eq!(seen, ["system", "system_commands", "schedule", "state_tree_tick"]);
            // A dropped callsite is disabled before its fields are evaluated: no `String` is built.
            assert_eq!(built.get(), 0);
        }
    }

    fn noop_callback() {}

    #[test]
    fn panic_callback_registers_once_per_process() {
        // No other test in this crate registers one; the callback is harmless if a panic test runs it.
        assert!(
            set_panic_callback(noop_callback),
            "the first registration is accepted"
        );
        assert!(
            !set_panic_callback(noop_callback),
            "later registrations are refused"
        );
        assert!(PANIC_CALLBACK.get().is_some());
    }

    #[test]
    fn default_filter_parses() {
        assert!(EnvFilter::builder().parse(DEFAULT_FILTER).is_ok());
    }

    /// v0.5.1: the default filter drops the font database's warnings and keeps its errors and
    /// everything else at `info`.
    #[test]
    fn default_filter_keeps_fontdb_errors_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing_subscriber::layer::SubscriberExt;
        #[derive(Clone, Default)]
        struct Count(std::sync::Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> Layer<S> for Count {
            fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Count::default();
        let filter = EnvFilter::builder()
            .parse(DEFAULT_FILTER)
            .expect("default filter");
        let subscriber = tracing_subscriber::registry().with(count.clone().with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "fontdb", "malformed font");
            tracing::info!(target: "fontdb", "loaded");
        });
        assert_eq!(
            count.0.load(Ordering::Relaxed),
            0,
            "fontdb warn and info are dropped"
        );
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(
                count.clone().with_filter(
                    EnvFilter::builder()
                        .parse(DEFAULT_FILTER)
                        .expect("default filter"),
                ),
            ),
            || {
                tracing::error!(target: "fontdb", "kept");
                tracing::info!(target: "regia_ui", "kept");
            },
        );
        assert_eq!(
            count.0.load(Ordering::Relaxed),
            2,
            "fontdb errors and engine info are kept"
        );
    }

    #[test]
    fn bad_filter_is_an_error() {
        let config = LogConfig {
            filter: "info,[".into(),
            ..LogConfig::default()
        };
        // Only meaningful when nothing initialised logging earlier in this test process.
        if INSTALLED.get().is_none() && crate::brand::env_var(FILTER_ENV).is_err() {
            assert!(matches!(init(config), Err(LogError::Filter { .. })));
        }
    }
}
