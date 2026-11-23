// SPDX-License-Identifier: MPL-2.0
//! Work that `regia_core::tasks` hands to the pools is visible to profilers (ADR-1378): every
//! `par_chunks` chunk opens a `par_chunk` span and every `spawn` / `spawn_io` future runs in an
//! `async_task` / `io_task` span, each a child of the caller's span that names it in `of`. A
//! system's own span covers only the thread it runs on, so without these the pool threads' time was
//! missing from the frame profiler and Tracy.
//!
//! The pool threads use the global subscriber, so this file installs one and holds a single test.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// The span names this test records.
const RECORDED: [&str; 3] = ["par_chunk", "async_task", "io_task"];

/// One recorded span: its name, its `of` field and its parent's name (empty for a root span). The
/// parent is explicit, so it is the caller's span on whichever thread the work runs.
#[derive(Debug, Clone)]
struct ChunkSpan {
    name: &'static str,
    of: String,
    parent: String,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<ChunkSpan>>>);

struct OfField(String);

impl Visit for OfField {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "of" {
            value.clone_into(&mut self.0);
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

impl<S> Layer<S> for Recorder
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        if !RECORDED.contains(&name) {
            return;
        }
        let mut of = OfField(String::new());
        attrs.record(&mut of);
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.name().to_owned())
            .unwrap_or_default();
        self.0.lock().expect("recorder lock").push(ChunkSpan {
            name,
            of: of.0,
            parent,
        });
    }
}

impl Recorder {
    fn take(&self) -> Vec<ChunkSpan> {
        std::mem::take(&mut *self.0.lock().expect("recorder lock"))
    }
}

#[test]
fn par_chunks_open_one_child_span_per_chunk_on_the_pool_threads() {
    let recorder = Recorder::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(recorder.clone()))
        .expect("the only subscriber in this test binary");

    let data: Vec<u32> = (0..1000).collect();
    let sums = {
        let _system = tracing::info_span!("cull_things").entered();
        regia_core::tasks::par_chunks(&data, 100, |_, chunk| chunk.iter().sum::<u32>())
    };
    assert_eq!(sums.iter().sum::<u32>(), data.iter().sum::<u32>());
    let spans = recorder.take();
    assert_eq!(spans.len(), 10, "one span per chunk: {spans:?}");
    assert!(
        spans
            .iter()
            .all(|s| s.of == "cull_things" && s.parent == "cull_things"),
        "{spans:?}"
    );

    let mut values = vec![0u8; 64];
    {
        let _system = tracing::info_span!("skin_things").entered();
        regia_core::tasks::par_chunks_mut(&mut values, 16, |_, chunk| chunk.fill(1));
    }
    assert!(values.iter().all(|&v| v == 1));
    let spans = recorder.take();
    assert_eq!(spans.len(), 4, "{spans:?}");
    assert!(
        spans
            .iter()
            .all(|s| s.of == "skin_things" && s.parent == "skin_things"),
        "{spans:?}"
    );

    // Called outside any span: the chunks are root spans with an empty `of`.
    let lens = regia_core::tasks::par_chunks(&data, 500, |_, chunk| chunk.len());
    assert_eq!(lens, vec![500, 500]);
    let spans = recorder.take();
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert!(
        spans.iter().all(|s| s.of.is_empty() && s.parent.is_empty()),
        "{spans:?}"
    );

    // Background futures: one span each, entered on every poll, a child of the caller's span.
    let (computed, read) = {
        let _system = tracing::info_span!("stream_things").entered();
        let computed = regia_core::tasks::spawn(async { 6 * 7 });
        let read = regia_core::tasks::spawn_io(async { "bytes" });
        (
            regia_core::tasks::block_on(computed),
            regia_core::tasks::block_on(read),
        )
    };
    assert_eq!((computed, read), (42, "bytes"));
    let mut spans = recorder.take();
    spans.sort_by_key(|s| s.name);
    let names: Vec<&str> = spans.iter().map(|s| s.name).collect();
    assert_eq!(names, ["async_task", "io_task"], "{spans:?}");
    assert!(
        spans
            .iter()
            .all(|s| s.of == "stream_things" && s.parent == "stream_things"),
        "{spans:?}"
    );
}
