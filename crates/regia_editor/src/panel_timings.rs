// SPDX-License-Identifier: MPL-2.0
//! Per-panel draw times for soak scripts (ADR-7306, `book/agents/editor-panel-timings.md`).
//!
//! The dock times every panel's draw ([`PanelTimer`] around the tab viewer's `ui`) and keeps the
//! last [`KEEP`] draw times per panel in a small ring; `editor.panel_timings` returns them. A
//! large-project soak opens a level, scrolls or edits, and reads the rings to find the panel whose
//! draw time grows with the project.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// Draw times kept per panel.
pub const KEEP: usize = 240;

/// One panel's ring: the last draw times (milliseconds) and the total draws.
#[derive(Debug, Default)]
struct Ring {
    times: VecDeque<f32>,
    draws: u64,
}

fn rings() -> MutexGuard<'static, BTreeMap<&'static str, Ring>> {
    static RINGS: OnceLock<Mutex<BTreeMap<&'static str, Ring>>> = OnceLock::new();
    // A panic while the lock was held leaves plain numbers behind: keep using them.
    RINGS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records one draw of `panel` that took `elapsed`.
pub fn record(panel: &'static str, elapsed: Duration) {
    let mut rings = rings();
    let ring = rings.entry(panel).or_default();
    if ring.times.len() == KEEP {
        ring.times.pop_front();
    }
    ring.times.push_back(elapsed.as_secs_f32() * 1000.0);
    ring.draws += 1;
}

/// Times a panel draw: records the time from [`PanelTimer::start`] to the drop.
#[derive(Debug)]
pub struct PanelTimer {
    panel: &'static str,
    started: Instant,
}

impl PanelTimer {
    /// Starts timing `panel` (its dock title).
    #[must_use]
    pub fn start(panel: &'static str) -> Self {
        Self {
            panel,
            started: Instant::now(),
        }
    }
}

impl Drop for PanelTimer {
    fn drop(&mut self) {
        record(self.panel, self.started.elapsed());
    }
}

/// One panel's timings as `editor.panel_timings` returns them.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelTimings {
    /// The panel (its dock title).
    pub panel: String,
    /// Draws recorded since start (or the last clear).
    pub draws: u64,
    /// The last draw times in milliseconds, oldest first.
    pub last_ms: Vec<f32>,
}

impl PanelTimings {
    /// The largest of `last_ms` (0 when empty).
    #[must_use]
    pub fn max_ms(&self) -> f32 {
        self.last_ms.iter().copied().fold(0.0, f32::max)
    }

    /// The median of `last_ms` (0 when empty).
    #[must_use]
    pub fn median_ms(&self) -> f32 {
        let mut sorted = self.last_ms.clone();
        sorted.sort_by(f32::total_cmp);
        sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
    }
}

/// The last `last` draw times of every panel (or only `panel`), panels in name order.
#[must_use]
pub fn snapshot(panel: Option<&str>, last: usize) -> Vec<PanelTimings> {
    rings()
        .iter()
        .filter(|(name, _)| panel.is_none_or(|p| p == **name))
        .map(|(name, ring)| PanelTimings {
            panel: (*name).to_string(),
            draws: ring.draws,
            last_ms: ring
                .times
                .iter()
                .skip(ring.times.len().saturating_sub(last))
                .copied()
                .collect(),
        })
        .collect()
}

/// Forgets every recorded time (`editor.panel_timings { clear: true }`).
pub fn clear() {
    rings().clear();
}

fn panel_timings_method(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let last = match params.get("last") {
        None | Some(Value::Null) => 60,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=KEEP as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| RpcError::wrong_type("last", "an integer from 1 to 240"))?,
    };
    let panel = match params.get("panel") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.as_str()),
        Some(_) => return Err(RpcError::wrong_type("panel", "a string")),
    };
    let clear_after = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let panels = snapshot(panel, last);
    if clear_after {
        clear();
    }
    Ok(json!({
        "keep": KEEP,
        // Preview renders queued or running on the preview thread (ADR-8584): 0 once every
        // opened preview has its picture.
        "preview_jobs": crate::preview_worker::jobs_in_flight(),
        "panels": panels
            .iter()
            .map(|p| json!({
                "panel": p.panel,
                "draws": p.draws,
                "last_ms": p.last_ms,
                "median_ms": p.median_ms(),
                "max_ms": p.max_ms(),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The panel timings' agent surface (`editor.panel_timings`).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.panel_timings",
            tool: "panel_timings",
            description: "The last draw times of every editor panel (milliseconds, oldest first; the dock \
                times each panel's draw and keeps the last 240), with each panel's draw count, median \
                and max. For large-project soak scripts: draw the editor for a while, then read which \
                panel's time grows. `clear: true` forgets the times after reading them. `preview_jobs` \
                counts the preview renders queued or running off the UI thread (0 once every opened \
                preview has its picture).",
            params: &[
                ParamSchema {
                    name: "panel",
                    kind: "string",
                    required: false,
                    description: "Only this panel (its dock title, such as \"Outliner\" or \"Output Log\").",
                },
                ParamSchema {
                    name: "last",
                    kind: "integer",
                    required: false,
                    description: "How many of the newest draw times to return per panel (1 to 240, default 60).",
                },
                ParamSchema {
                    name: "clear",
                    kind: "boolean",
                    required: false,
                    description: "Forget every recorded time after reading. Default false.",
                },
            ],
            result: "{ keep, preview_jobs, panels: [{ panel, draws, last_ms: [number], median_ms, max_ms }] }",
            mutating: false,
            permission: Permission::Read,
        },
        panel_timings_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_times_and_counts_every_draw() {
        let panel = "panel_timings unit ring";
        for i in 0..(KEEP + 10) {
            record(panel, Duration::from_micros(i as u64 * 1000));
        }
        let timings = snapshot(Some(panel), KEEP);
        assert_eq!(timings.len(), 1);
        let ring = &timings[0];
        assert_eq!(ring.draws, (KEEP + 10) as u64);
        assert_eq!(ring.last_ms.len(), KEEP);
        assert!(
            (ring.last_ms[0] - 10.0).abs() < 1e-3,
            "the oldest kept is draw 10"
        );
        let last3 = snapshot(Some(panel), 3);
        assert_eq!(last3[0].last_ms.len(), 3);
        assert!((last3[0].max_ms() - (KEEP + 9) as f32).abs() < 1e-3);
    }

    #[test]
    fn a_timer_records_on_drop() {
        let panel = "panel_timings unit timer";
        {
            let _timer = PanelTimer::start(panel);
        }
        assert_eq!(snapshot(Some(panel), 10)[0].draws, 1);
    }
}
