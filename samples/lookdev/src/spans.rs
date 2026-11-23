// SPDX-License-Identifier: MIT-0
//! Hitch-hunting span tools for `do27_chase` (`--slow-spans MS`, `--census`).
//!
//! A `tracing` layer that times every span on the frame thread. Each top-level span (one
//! `render_into` per frame) collects the time of its descendants down to five levels; a frame
//! longer than the threshold prints its ten most expensive descendants. The census prints every
//! `Device::create_*` made inside a frame — with the `wgpu-spans` feature these are wgpu-core's
//! own spans, so a create the engine's `TrackedDevice` counters cannot see (a crate holding a raw
//! `wgpu::Device`) still shows, by call chain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::span::Id;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Levels below the top-level span a slow frame's breakdown keeps (`render_into` < `encode` <
/// phase < pass < its parts, e.g. the terrain's selection inside the shared draw's prepare).
const SPAN_DEPTH: usize = 7;

/// Descendant time (ms) by call chain under one top-level span.
#[derive(Default)]
struct Breakdown(HashMap<String, f64>);

/// Span levels the per-system totals keep ([`install_systems`]): a root span (`render_into`,
/// an encode worker's segment) and seven levels below it (`render_into>encode>shadow_plan>…`, down
/// to the terrain's staging inside the shared draw's prepare).
const SYSTEM_DEPTH: usize = 8;

/// The layer ([`install`]).
pub struct SpanTools {
    slow: Option<Duration>,
    census: bool,
    frames: AtomicU64,
    systems: Option<SystemSpans>,
}

/// CPU time by span chain since the last [`SystemSpans::take_frame`], on every thread
/// (the frame thread and the renderer's encode workers): `do27_bench`'s per-system table.
#[derive(Clone, Default)]
pub struct SystemSpans(Arc<Mutex<HashMap<String, f64>>>);

impl SystemSpans {
    /// The totals (ms, keyed `root>child>grandchild`) gathered since the last call; a
    /// bench calls it once per frame, after the frame's spans closed.
    #[must_use]
    pub fn take_frame(&self) -> HashMap<String, f64> {
        self.0
            .lock()
            .map(|mut totals| std::mem::take(&mut *totals))
            .unwrap_or_default()
    }

    fn add(&self, key: String, ms: f64) {
        if let Ok(mut totals) = self.0.lock() {
            *totals.entry(key).or_insert(0.0) += ms;
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for SpanTools
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Instant::now());
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let Some(elapsed) = span.extensions().get::<Instant>().map(Instant::elapsed) else {
            return;
        };
        let Some(root) = span.scope().last() else { return };
        let chain: Vec<&str> = span.scope().map(|s| s.name()).collect();
        if let Some(systems) = self.systems.as_ref()
            && chain.len() <= SYSTEM_DEPTH
        {
            let key: Vec<&str> = chain.iter().rev().copied().collect();
            systems.add(key.join(">"), elapsed.as_secs_f64() * 1e3);
        }
        if root.id() == span.id() {
            let frame = if span.name() == "render_into" {
                self.frames.fetch_add(1, Ordering::Relaxed)
            } else {
                self.frames.load(Ordering::Relaxed)
            };
            let breakdown = span.extensions_mut().remove::<Breakdown>().unwrap_or_default();
            if self.slow.is_some_and(|slow| elapsed > slow) {
                let mut parts: Vec<(String, f64)> = breakdown.0.into_iter().collect();
                parts.sort_by(|a, b| b.1.total_cmp(&a.1));
                let top: Vec<String> = parts
                    .iter()
                    .take(10)
                    .map(|(k, ms)| format!("{k}={ms:.2}"))
                    .collect();
                eprintln!(
                    "SLOW frame {frame} {:.2} ms {} :: {}",
                    elapsed.as_secs_f64() * 1e3,
                    span.name(),
                    top.join(" | ")
                );
            }
            return;
        }
        if self.census && span.name().starts_with("Device::create_") && chain.contains(&"render_into") {
            eprintln!(
                "CREATE frame {} {}",
                self.frames.load(Ordering::Relaxed),
                chain.join(" < ")
            );
        }
        if self.slow.is_some() && chain.len() <= SPAN_DEPTH {
            let key = chain[..chain.len() - 1].join("<");
            let mut ext = root.extensions_mut();
            if ext.get_mut::<Breakdown>().is_none() {
                ext.insert(Breakdown::default());
            }
            if let Some(breakdown) = ext.get_mut::<Breakdown>() {
                *breakdown.0.entry(key).or_insert(0.0) += elapsed.as_secs_f64() * 1e3;
            }
        }
    }
}

/// Installs the layer as the global subscriber: frames over `slow_ms` print their breakdown,
/// `census` prints frame-path creates (`CREATE frame <n> <chain>`, `n` counting every
/// `render_into` of the process, warm-up included). `false` when a subscriber was already set.
pub fn install(slow_ms: Option<f64>, census: bool) -> bool {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    tracing_subscriber::registry()
        .with(SpanTools {
            slow: slow_ms.map(|ms| Duration::from_secs_f64(ms.max(0.0) / 1000.0)),
            census,
            frames: AtomicU64::new(0),
            systems: None,
        })
        .try_init()
        .is_ok()
}

/// Installs the layer with per-system totals ([`SystemSpans`]) and nothing printed. `None`
/// when a subscriber was already set.
#[must_use]
pub fn install_systems() -> Option<SystemSpans> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let systems = SystemSpans::default();
    tracing_subscriber::registry()
        .with(SpanTools {
            slow: None,
            census: false,
            frames: AtomicU64::new(0),
            systems: Some(systems.clone()),
        })
        .try_init()
        .ok()
        .map(|()| systems)
}

/// [`install_systems`] plus what a game's telemetry adds: the frame profiler (hitch events read
/// their top spans from it) and a JSONL sink at `jsonl` (`do27_bench` under `REGIA_TELEMETRY`).
pub fn install_systems_telemetry(
    jsonl: &std::path::Path,
) -> Option<(
    SystemSpans,
    regia_core::profiling::FrameProfiler,
    regia_telemetry::JsonLayerGuard,
)> {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let systems = SystemSpans::default();
    let profiler = regia_core::profiling::FrameProfiler::new();
    // `REGIA_TELEMETRY_SPANS=0`: no frame-profiler layer (hitch events then carry no CPU spans).
    let spans = std::env::var("REGIA_TELEMETRY_SPANS").map_or(true, |v| v != "0");
    let (json, guard) =
        regia_telemetry::json_layer(jsonl, regia_telemetry::JsonLayerOptions::default()).ok()?;
    let filter = tracing_subscriber::EnvFilter::new(regia_telemetry::RECOMMENDED_FILTER);
    tracing_subscriber::registry()
        .with(SpanTools {
            slow: None,
            census: false,
            frames: AtomicU64::new(0),
            systems: Some(systems.clone()),
        })
        .with(spans.then(|| profiler.clone()))
        .with(json.with_filter(filter))
        .try_init()
        .ok()?;
    Some((systems, profiler, guard))
}

/// Span totals for `do27_chase`'s warm probe (`CHASE_WARM_SPANS=1`), set once by the binary.
pub static WARM_SPANS: std::sync::OnceLock<SystemSpans> = std::sync::OnceLock::new();

/// Clears the warm probe's span totals (before the phase it measures).
pub fn reset_warm_spans() {
    if let Some(spans) = WARM_SPANS.get() {
        drop(spans.take_frame());
    }
}

/// Prints the warm probe's span totals since [`reset_warm_spans`], largest first (top 40).
pub fn print_warm_spans(label: &str) {
    let Some(spans) = WARM_SPANS.get() else { return };
    let mut totals: Vec<(String, f64)> = spans.take_frame().into_iter().collect();
    totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("span totals ({label}), ms summed over all threads:");
    for (chain, ms) in totals.iter().take(40) {
        println!("  {ms:10.1}  {chain}");
    }
}
