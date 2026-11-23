// SPDX-License-Identifier: MPL-2.0
//! `regia::phase` events (spec 28 § 3): one event per transition of the session's life, from the
//! engine (`boot_done`, `shaders_ready`, `scene_load_*`, `quit_*`) and from the host through
//! [`phase`]. They share a target so one tool reads both, and they tell the telemetry which map the
//! frames belong to (`summary.json`'s `frames_by_map`) and which phase a stall happened in.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde_json::{Map, Value};
use tracing::Level;

/// What the host's phase markers say about now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhaseNow {
    /// The newest phase's name (`""` before any).
    pub phase: String,
    /// The map the frames belong to: set by a named `scene_load_start`, `scene_load_end` or
    /// `match_start`, cleared by `match_end` and `menu_ready`.
    pub map: Option<String>,
    /// Every map named so far, in order of appearance.
    pub maps: Vec<String>,
}

static NOW: Mutex<PhaseNow> = Mutex::new(PhaseNow {
    phase: String::new(),
    map: None,
    maps: Vec::new(),
});

/// Bumped on every change of [`PhaseNow`], so the per-frame code reads one atomic.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Most distinct map names kept.
const MAX_MAPS: usize = 32;

pub(crate) fn epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

pub(crate) fn now() -> PhaseNow {
    NOW.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Applies the marker `name` (with its optional map `data.name`) to [`PhaseNow`].
fn note(name: &str, map: Option<&str>) {
    let mut now = NOW.lock().unwrap_or_else(PoisonError::into_inner);
    name.clone_into(&mut now.phase);
    match name {
        "scene_load_start" | "scene_load_end" | "match_start" => {
            if let Some(map) = map {
                if now.maps.iter().any(|m| m == map) {
                    now.map = Some(map.to_owned());
                } else if now.maps.len() < MAX_MAPS {
                    now.maps.push(map.to_owned());
                    now.map = Some(map.to_owned());
                } else {
                    // List full: a name the summary cannot list must not become current.
                    now.map = None;
                }
            }
        }
        "match_end" | "menu_ready" => now.map = None,
        _ => {}
    }
    drop(now);
    EPOCH.fetch_add(1, Ordering::Release);
}

/// Emits a `regia::phase` event (and a `phase:<name>` trace mark while a trace records marks):
/// the session entered phase `name`. `data` is a JSON object
/// whose fields (`name` for a map, `ms`, results) are written next to `phase` in the event's
/// `fields.data`; anything else is written as `value`. Hosts call it for the phases only they know
/// (`menu_ready`, `match_start`, `match_end`, the map `name` of a `scene_load_start`); the engine
/// emits the others itself. Cheap and thread-safe, but not for per-frame use.
pub fn phase(name: &str, data: Value) {
    emit(Level::INFO, name, data);
}

/// [`phase`] at `level` (a warning for `previous_session_unclean`).
pub(crate) fn emit(level: Level, name: &str, data: Value) {
    let mut fields = Map::new();
    fields.insert("phase".into(), Value::String(name.to_owned()));
    match data {
        Value::Object(map) => {
            for (k, v) in map {
                if k != "phase" {
                    fields.insert(k, v);
                }
            }
        }
        Value::Null => {}
        other => {
            fields.insert("value".into(), other);
        }
    }
    let map = fields.get("name").and_then(Value::as_str).map(str::to_owned);
    note(name, map.as_deref());
    // Spec 54 § 2: a phase is also a trace mark (a no-op load while no stream wants marks).
    crate::trace::recorder().mark(&format!("phase:{name}"), None);
    let data = Value::Object(fields).to_string();
    if level == Level::WARN {
        tracing::warn!(target: crate::target::PHASE, phase = name, data = %data, "phase: {name}");
    } else {
        tracing::info!(target: crate::target::PHASE, phase = name, data = %data, "phase: {name}");
    }
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *NOW.lock().unwrap_or_else(PoisonError::into_inner) = PhaseNow::default();
}
