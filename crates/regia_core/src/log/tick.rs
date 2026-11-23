// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-log/src/lib.rs, commit d6ed37677), ADR-8012.
// Adapted from DO27 ir-log/src/lib.rs:284-291, d6ed37677 (regia-absorb-1).
// Owner-authorized copy, 2026-11-16, absorb plan A-1; class P3.
//! Process tick stamps for tracing output. Peer sessions use their own independent tick.

use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::registry::LookupSpan;

static TICK: AtomicU64 = AtomicU64::new(0);

/// Sets the process's diagnostic simulation tick (zero before the first call).
/// This is a stamp, not a simulation clock. Multiple in-process peers use `PeerLog::set_tick`.
pub fn set_tick(tick: u64) {
    TICK.store(tick, Ordering::Relaxed);
}

/// Returns the process's diagnostic simulation tick.
pub fn tick() -> u64 {
    TICK.load(Ordering::Relaxed)
}

/// Wraps a tracing formatter, stamping every physical output line with the same event tick.
#[derive(Debug, Clone, Default)]
pub struct TickFormat<F>(
    /// Inner tracing formatter; its line content is preserved after the tick prefix.
    pub F,
);

impl<S, N, F> FormatEvent<S, N> for TickFormat<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let tick = tick();
        let mut text = String::new();
        self.0.format_event(ctx, Writer::new(&mut text), event)?;
        for line in text.lines() {
            writeln!(writer, "t={tick} {line}")?;
        }
        Ok(())
    }
}
