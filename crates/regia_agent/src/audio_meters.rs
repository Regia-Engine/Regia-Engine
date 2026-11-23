// SPDX-License-Identifier: MPL-2.0
//! `audio.bus_meters` (ADR-2530, `book/agents/audio-mixer.md`): the level of every bus (sample peak,
//! RMS and short-term loudness, post-fader), read lock-free from the meters the mixer thread publishes.
//!
//! A world without [`regia_audio::Audio`] returns `UNSUPPORTED` (`F0013`).

use regia_app::prelude::*;
use regia_audio::Audio;
use regia_audio::bus_meter::{METER_FLOOR_DB, METER_HOP_SECS};
use serde_json::{Value, json};

use crate::audio_mix::{boolean, fields, require_audio};
use crate::protocol::RpcError;

/// `audio.bus_meters`. With `floor: false` silence reads `null` instead of the floor value.
pub fn bus_meters(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    let fields = fields(params)?;
    let floor = boolean(&fields, "floor")?.unwrap_or(true);
    let audio = world.resource::<Audio>();
    let level = |db: f32| -> Value {
        if floor {
            json!(db.max(METER_FLOOR_DB))
        } else if db.is_finite() {
            json!(db)
        } else {
            Value::Null
        }
    };
    let buses: Vec<Value> = audio
        .bus_meters()
        .into_iter()
        .map(|(name, r)| {
            json!({
                "bus": name,
                "peak_db": level(r.peak_db),
                "rms_db": level(r.rms_db),
                "short_term_lufs": level(r.short_term_lufs),
            })
        })
        .collect();
    Ok(json!({
        "buses": buses,
        "floor_db": METER_FLOOR_DB,
        "hop_ms": METER_HOP_SECS * 1000.0,
        "headless": audio.is_headless(),
    }))
}
