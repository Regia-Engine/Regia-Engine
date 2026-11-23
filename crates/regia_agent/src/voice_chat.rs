// SPDX-License-Identifier: MPL-2.0
//! Agent methods for voice chat (`docs/specs/59-voice-chat.md` § 5, `book/agents/voice-chat.md`).
//!
//! Every method works in a headless build with synthetic sources and no audio device. Consent is
//! never granted here: only a person sets it in the user settings. `F0013` means
//! `VoiceChatPlugin` is missing; every other refusal is `F0002` with `data.reason` set to one of
//! the lowercase codes of `regia_audio::voice_chat::ERROR_CODES` (a plain `F0002` for
//! `invalid_params`).

use regia_app::prelude::*;
use regia_audio::voice_chat::{
    CpalInputOpener, EchoReference, FakeMicrophone, NoiseSuppression, STAGE_NAMES, SimulatorSettings,
    TestSource, VoiceChatEmitter, VoiceChatError, VoiceChatOutput, VoiceChatResource, VoiceChatSession,
    VoiceChatTalk, VoiceMode, VoiceQuality,
};
use regia_net::voice::VoiceRelay;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

fn missing_plugin() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0013",
        "no voice chat in this app",
        "add regia_audio::voice_chat::VoiceChatPlugin",
    )
}

/// `F0002` with `data.reason` (plain `F0002` for `invalid_params`).
fn refused(error: &VoiceChatError) -> RpcError {
    if let VoiceChatError::InvalidParams(message) = error {
        return RpcError::invalid_params(message.clone());
    }
    let mut out = RpcError::new(
        protocol::INVALID_PARAMS,
        "F0002",
        error.to_string(),
        "see the reason codes in book/agents/voice-chat.md",
    );
    if let Some(Value::Object(data)) = out.data.as_mut() {
        data.insert("reason".into(), json!(error.code()));
    }
    out
}

fn with_session(
    world: &World,
    f: impl FnOnce(&mut VoiceChatSession) -> Result<Value, VoiceChatError>,
) -> Result<Value, RpcError> {
    let Some(resource) = world.get_resource::<VoiceChatResource>() else {
        return Err(missing_plugin());
    };
    let mut session = resource
        .0
        .lock()
        .map_err(|_| RpcError::internal("the voice chat session lock is poisoned"))?;
    f(&mut session).map_err(|e| refused(&e))
}

fn fields(params: &Value) -> Result<Map<String, Value>, VoiceChatError> {
    match params {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(VoiceChatError::InvalidParams("params must be an object".into())),
    }
}

fn opt_bool(f: &Map<String, Value>, name: &str) -> Result<Option<bool>, VoiceChatError> {
    match f.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| VoiceChatError::InvalidParams(format!("`{name}` must be true or false"))),
    }
}

fn opt_number(f: &Map<String, Value>, name: &str, lo: f64, hi: f64) -> Result<Option<f64>, VoiceChatError> {
    match f.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_f64()
                .filter(|n| n.is_finite() && *n >= lo && *n <= hi)
                .ok_or_else(|| {
                    VoiceChatError::InvalidParams(format!("`{name}` must be a number in {lo}..={hi}"))
                })?;
            Ok(Some(n))
        }
    }
}

fn opt_str<'a>(f: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>, VoiceChatError> {
    match f.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| VoiceChatError::InvalidParams(format!("`{name}` must be a string"))),
    }
}

fn required_u16(f: &Map<String, Value>, name: &str) -> Result<u16, VoiceChatError> {
    opt_number(f, name, 0.0, 65_535.0)?
        .map(|n| n as u16)
        .ok_or_else(|| VoiceChatError::InvalidParams(format!("`{name}` is required")))
}

fn stats_json(s: &VoiceChatSession) -> Value {
    let t = s.stats();
    json!({
        "frames_encoded": t.frames_encoded,
        "packets_sent": t.packets_sent,
        "bytes_sent": t.bytes_sent,
        "packets_received": t.packets_received,
        "bytes_received": t.bytes_received,
        "bad_packets": t.bad_packets,
        "wrong_channel": t.wrong_channel,
        "muted_drops": t.muted_drops,
        "deafened_drops": t.deafened_drops,
        "overflow_drops": t.overflow_drops,
        "capture_overruns": t.capture_overruns,
        "encode_errors": t.encode_errors,
        "oversize_frames": t.oversize_frames,
    })
}

/// The entity of each speaker's `VoiceChatEmitter`, by sender; the lowest entity when several
/// carry the same sender, so the answer does not depend on query order.
fn emitter_entities(world: &mut World) -> std::collections::BTreeMap<u16, u64> {
    let mut out = std::collections::BTreeMap::new();
    let mut query = world.query::<(Entity, &VoiceChatEmitter)>();
    for (entity, emitter) in query.iter(world) {
        let bits = entity.to_bits();
        out.entry(emitter.sender)
            .and_modify(|seen: &mut u64| *seen = (*seen).min(bits))
            .or_insert(bits);
    }
    out
}

/// `voice_chat.status`: the state, consent, mode, channels, speakers (with their emitter entity)
/// test-mode flags and the error codes the running session met.
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let entities = emitter_entities(world);
    with_session(world, |s| {
        let st = s.status();
        Ok(json!({
            "state": st.state,
            "error": st.error,
            "consent": st.consent,
            "can_prompt": st.can_prompt,
            "microphone": st.microphone,
            "mode": st.mode,
            "quality": st.quality,
            "codec": st.codec,
            "bitrate_bps": st.bitrate_bps,
            "device": st.device.clone().or_else(|| s.user().device.clone()),
            "rate": st.rate,
            "channels": st.channels.iter().map(|(id, name, kind, joined, active)| {
                json!({ "id": id, "name": name, "kind": kind, "joined": joined, "active": active })
            }).collect::<Vec<_>>(),
            "speakers": st.speakers.iter().map(|sp| {
                json!({
                    "sender": sp.sender,
                    "state": sp.state,
                    "entity": entities.get(&sp.sender),
                    "muted": sp.muted,
                    "blocked": sp.blocked,
                    "level": sp.level_db,
                })
            }).collect::<Vec<_>>(),
            "effective_processing": st.effective_processing,
            "loopback": st.loopback,
            "test_mode": st.test_mode,
            "errors": st.errors.iter().map(|e| {
                json!({ "code": e.code, "count": e.count, "last_frame": e.last_frame })
            }).collect::<Vec<_>>(),
        }))
    })
}

/// `voice_chat.set_channel`: join or leave a channel, make it the transmit channel, set a radius.
pub fn set_channel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        let id = required_u16(&f, "channel")?;
        let join = opt_bool(&f, "join")?;
        let active = opt_bool(&f, "active")?;
        let radius = opt_number(&f, "radius", 0.0, 1.0e6)?.map(|r| r as f32);
        let (joined, is_active) = s.set_channel(id, join, active, radius)?;
        let def = s
            .settings()
            .channels
            .iter()
            .find(|c| c.id == id)
            .ok_or(VoiceChatError::UnknownChannel(id))?;
        Ok(json!({
            "id": def.id,
            "name": def.name,
            "kind": def.kind.name(),
            "radius": def.radius,
            "spatial": def.spatial,
            "joined": joined,
            "active": is_active,
        }))
    })
}

/// `voice_chat.mute`: self mute, deafen, a sender slot or an account (persisted).
pub fn mute(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        let muted = opt_bool(&f, "muted")?
            .ok_or_else(|| VoiceChatError::InvalidParams("`muted` is required".into()))?;
        let block = opt_bool(&f, "block")?.unwrap_or(false);
        let persist = opt_bool(&f, "persist")?.unwrap_or(true);
        let volume = opt_number(&f, "volume_pct", 0.0, 200.0)?;
        let target = f
            .get("target")
            .ok_or_else(|| VoiceChatError::InvalidParams("`target` is required".into()))?;
        match target {
            Value::String(t) if t == "self" => {
                s.set_self_muted(muted);
                Ok(json!({ "applied": true, "persisted": false }))
            }
            Value::String(t) if t == "deafen" => {
                s.set_deafened(muted);
                Ok(json!({ "applied": true, "persisted": false }))
            }
            Value::Object(o) if o.contains_key("sender") => {
                let sender = required_u16(o, "sender")?;
                if block {
                    // Spec 59 § 2.13: a block is per account and enforced by the server.
                    return Err(VoiceChatError::InvalidParams(
                        "a block is per account: pass `target: { account }`, not a sender slot".into(),
                    ));
                }
                if let Some(pct) = volume {
                    s.set_speaker_volume(sender, pct as f32)?;
                }
                s.mute_sender(sender, muted);
                Ok(json!({ "applied": true, "persisted": false, "block": false }))
            }
            Value::Object(o) if o.contains_key("account") => {
                let account = opt_str(o, "account")?
                    .filter(|a| !a.is_empty() && a.len() <= 256)
                    .ok_or_else(|| {
                        VoiceChatError::InvalidParams("`account` must be 1..=256 characters".into())
                    })?;
                if !persist {
                    return Err(VoiceChatError::Unsupported(
                        "account mutes are always stored in the user settings; omit `persist` or pass true"
                            .into(),
                    ));
                }
                let note = opt_str(&f, "note")?.unwrap_or("");
                if block {
                    s.block_account(account, note, muted);
                } else {
                    s.mute_account(account, note, muted);
                }
                Ok(json!({ "applied": true, "persisted": true, "block": block }))
            }
            _ => Err(VoiceChatError::InvalidParams(
                "`target` must be \"self\", \"deafen\", { sender } or { account }".into(),
            )),
        }
    })
}

/// `voice_chat.loopback`: the full-chain loopback of the local source (never touches a network).
pub fn loopback(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        let enabled = opt_bool(&f, "enabled")?
            .ok_or_else(|| VoiceChatError::InvalidParams("`enabled` is required".into()))?;
        let source = opt_str(&f, "source")?.unwrap_or("sine");
        if !enabled {
            s.set_loopback(false, false)?;
            return Ok(json!({ "enabled": false, "source": source }));
        }
        match source {
            "mic" => s.set_loopback(true, true)?,
            "sine" | "noise" => {
                if !s.test_mode_allowed() {
                    return Err(VoiceChatError::TestModeOnly);
                }
                let src = if source == "sine" {
                    TestSource::sine(440.0, -12.0)
                } else {
                    TestSource::noise(1, -20.0)
                };
                s.set_test_source(Some(src));
                s.set_loopback(true, false)?;
            }
            other => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`source` must be \"mic\", \"sine\" or \"noise\", not \"{other}\""
                )));
            }
        }
        Ok(json!({ "enabled": true, "source": source }))
    })
}

/// `voice_chat.mic`: enables or disables the microphone. Never grants consent: without it the
/// answer is `consent_required`. `fake: true` installs a scripted microphone (the developer test
/// mode), so the whole capture path runs with no device.
pub fn mic(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        let enabled = opt_bool(&f, "enabled")?
            .ok_or_else(|| VoiceChatError::InvalidParams("`enabled` is required".into()))?;
        if !enabled {
            s.disable_microphone();
        } else {
            let fake = opt_bool(&f, "fake")?.unwrap_or(false);
            if fake {
                if !s.test_mode_allowed() {
                    return Err(VoiceChatError::TestModeOnly);
                }
                let rate = opt_number(&f, "rate", 8_000.0, 192_000.0)?.unwrap_or(48_000.0) as u32;
                let channels = opt_number(&f, "channels", 1.0, 8.0)?.unwrap_or(1.0) as usize;
                let freq = opt_number(&f, "freq_hz", 20.0, 20_000.0)?.unwrap_or(440.0);
                let level = opt_number(&f, "level_db", -120.0, 0.0)?.unwrap_or(-12.0) as f32;
                let mic = FakeMicrophone::new(rate, channels);
                mic.set_tone(Some(TestSource::sine(freq, level)));
                s.set_microphone(Box::new(mic));
            } else if !s.has_microphone() {
                s.set_microphone(Box::new(CpalInputOpener::new()));
            }
            s.enable_microphone()?;
        }
        let st = s.status();
        Ok(json!({
            "enabled": enabled,
            "microphone": st.microphone,
            "state": st.state,
            "error": st.error,
            "can_prompt": st.can_prompt,
            "device": st.device,
        }))
    })
}

fn settings_json(s: &VoiceChatSession) -> Value {
    let u = s.user();
    json!({
        "consent": u.consent,
        "mode": u.mode.name(),
        "quality": s.quality().name(),
        "device": u.device,
        "input_gain_db": u.input_gain_db,
        "output_gain_db": u.output_gain_db,
        "deafen": u.deafen,
        "muted_accounts": u.muted_accounts.0.len(),
        "blocked_accounts": u.blocked_accounts.0.len(),
    })
}

/// `voice_chat.settings`: reads the user settings and sets the transmit mode, gains, device and
/// profile. Consent is not a parameter: only a person sets it.
pub fn settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        if f.contains_key("consent") {
            return Err(VoiceChatError::InvalidParams(
                "consent is never set by the agent protocol".into(),
            ));
        }
        let mode = match opt_str(&f, "mode")? {
            None => None,
            Some("open") => Some(VoiceMode::Open),
            Some("vad") => Some(VoiceMode::Vad),
            Some("push_to_talk") => Some(VoiceMode::PushToTalk),
            Some("toggle") => Some(VoiceMode::Toggle),
            Some(other) => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`mode` must be open, vad, push_to_talk or toggle, not \"{other}\""
                )));
            }
        };
        let quality = match opt_str(&f, "quality")? {
            None => None,
            Some("narrow") => Some(VoiceQuality::Narrow),
            Some("standard") => Some(VoiceQuality::Standard),
            Some("high") => Some(VoiceQuality::High),
            Some(other) => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`quality` must be narrow, standard or high, not \"{other}\""
                )));
            }
        };
        let input = opt_number(&f, "input_gain_db", -60.0, 24.0)?;
        let output = opt_number(&f, "output_gain_db", -60.0, 24.0)?;
        let device = opt_str(&f, "device")?;
        if let Some(mode) = mode {
            s.set_mode(mode);
        }
        if let Some(quality) = quality {
            s.set_quality(quality);
        }
        if let Some(db) = input {
            s.user_mut().input_gain_db = db as f32;
        }
        if let Some(db) = output {
            s.user_mut().output_gain_db = db as f32;
        }
        if let Some(name) = device {
            s.user_mut().device = (!name.is_empty()).then(|| name.to_string());
        }
        Ok(settings_json(s))
    })
}

/// `voice_chat.talk`: presses or releases the push-to-talk action, as the player's binding would.
pub fn talk(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |_| Ok(Value::Null))?;
    let pressed = fields(params)
        .and_then(|f| {
            opt_bool(&f, "pressed")?
                .ok_or_else(|| VoiceChatError::InvalidParams("`pressed` is required".into()))
        })
        .map_err(|e| refused(&e))?;
    world.get_resource_or_insert_with(VoiceChatTalk::default).0 = pressed;
    Ok(json!({ "pressed": pressed }))
}

/// `voice_chat.relay`: the server's voice relay: seated peers, channel members and counters.
pub fn relay(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(relay) = world.get_resource::<VoiceRelay>() else {
        return Err(RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "no voice relay in this app",
            "add regia_net::voice::NetVoicePlugin on the server",
        ));
    };
    let t = relay.stats();
    let peers: Vec<Value> = relay
        .peers()
        .map(|(peer, slot)| json!({ "peer": peer.0, "slot": slot }))
        .collect();
    Ok(json!({
        "peers": peers,
        "packets_in": t.packets_in,
        "bytes_in": t.bytes_in,
        "forwarded": t.forwarded,
        "bytes_out": t.bytes_out,
        "bad_packets": t.bad_packets,
        "rate_limited": t.rate_limited,
        "not_in_channel": t.not_in_channel,
        "server_muted": t.server_muted,
        "listener_cap_drops": t.listener_cap_drops,
        "joins_refused": t.joins_refused,
    }))
}

fn output_json(world: &World) -> Value {
    world.get_resource::<VoiceChatOutput>().map_or(Value::Null, |o| {
        let t = o.stats();
        let speakers: Vec<Value> = o
            .senders()
            .into_iter()
            .map(|sender| {
                json!({
                    "sender": sender,
                    "placed": o.is_placed(sender),
                    "gains": o.gains(sender),
                })
            })
            .collect();
        json!({
            "frames": t.frames,
            "dropped_frames": t.dropped_frames,
            "failed_frames": t.failed_frames,
            "streams_opened": t.streams_opened,
            "speakers": speakers,
        })
    })
}

/// `voice_chat.stats`: session and per-speaker counters; `reset: true` zeroes the session ones.
pub fn stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let output = output_json(world);
    with_session(world, |s| {
        let f = fields(params)?;
        let reset = opt_bool(&f, "reset")?.unwrap_or(false);
        let totals = stats_json(s);
        let speakers: Vec<Value> = s
            .speaker_stats()
            .iter()
            .map(|(id, st)| {
                let j = &st.jitter;
                json!({
                    "sender": id,
                    "received": j.received,
                    "late": j.late,
                    "duplicates": j.duplicates,
                    "played": j.played,
                    "recovered": j.recovered,
                    "spare_filled": j.spare_filled,
                    "concealed": j.concealed,
                    "silenced": j.silenced,
                    "underruns": j.underruns,
                    "jitter_ms": j.jitter_ms,
                    "decode_errors": st.decode_errors,
                    "timeouts": st.timeouts,
                    "unsupported": st.unsupported,
                    "shortened": st.shortened,
                    "lengthened": st.lengthened,
                    "drift_ppm": st.drift_ppm,
                })
            })
            .collect();
        if reset {
            s.reset_stats();
        }
        Ok(json!({ "totals": totals, "speakers": speakers, "output": output, "reset": reset }))
    })
}

/// `voice_chat.test_source`: a synthetic source in place of the microphone, or as a fake remote
/// speaker when `as_sender` is set.
pub fn test_source(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        if !s.test_mode_allowed() {
            return Err(VoiceChatError::TestModeOnly);
        }
        let kind =
            opt_str(&f, "kind")?.ok_or_else(|| VoiceChatError::InvalidParams("`kind` is required".into()))?;
        let freq = opt_number(&f, "freq_hz", 20.0, 20_000.0)?.unwrap_or(440.0);
        let level = opt_number(&f, "level_db", -120.0, 0.0)?.unwrap_or(-12.0) as f32;
        let seed = opt_number(&f, "seed", 0.0, 9.0e15)?.unwrap_or(1.0) as u64;
        let as_sender = match f.get("as_sender") {
            None | Some(Value::Null) => None,
            Some(_) => Some(required_u16(&f, "as_sender")?),
        };
        if as_sender == Some(regia_audio::voice_chat::LOOPBACK_SENDER) {
            return Err(VoiceChatError::InvalidParams(
                "`as_sender` 65535 is reserved for loopback".into(),
            ));
        }
        let enabled = opt_bool(&f, "enabled")?.unwrap_or(true);
        let source = match kind {
            "sine" => TestSource::sine(freq, level),
            "noise" => TestSource::noise(seed, level),
            "silence" => TestSource::Silence,
            other => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`kind` must be \"sine\", \"noise\" or \"silence\", not \"{other}\""
                )));
            }
        };
        let source = enabled.then_some(source);
        match as_sender {
            Some(sender) => s.set_fake_speaker(sender, source),
            None => s.set_test_source(source),
        }
        Ok(json!({ "enabled": enabled }))
    })
}

/// `voice_chat.simulate_network`: loss, burst, reorder, duplication, latency and jitter for the
/// loopback and fake-speaker paths.
pub fn simulate_network(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        if !s.test_mode_allowed() {
            return Err(VoiceChatError::TestModeOnly);
        }
        let settings = SimulatorSettings {
            loss_pct: opt_number(&f, "loss_pct", 0.0, 95.0)?
                .ok_or_else(|| VoiceChatError::InvalidParams("`loss_pct` is required".into()))?
                as f32,
            burst_len: opt_number(&f, "burst_len", 0.0, 1000.0)?.unwrap_or(1.0) as f32,
            reorder_pct: opt_number(&f, "reorder_pct", 0.0, 100.0)?.unwrap_or(0.0) as f32,
            dup_pct: opt_number(&f, "dup_pct", 0.0, 100.0)?.unwrap_or(0.0) as f32,
            latency_frames: (opt_number(&f, "latency_ms", 0.0, 5_000.0)?
                .ok_or_else(|| VoiceChatError::InvalidParams("`latency_ms` is required".into()))?
                / 20.0)
                .round() as u32,
            jitter_frames: (opt_number(&f, "jitter_ms", 0.0, 5_000.0)?.unwrap_or(0.0) / 20.0).round() as u32,
            seed: opt_number(&f, "seed", 0.0, 9.0e15)?
                .ok_or_else(|| VoiceChatError::InvalidParams("`seed` is required".into()))?
                as u64,
        };
        s.simulate_network(settings)?;
        Ok(json!({ "applied": true }))
    })
}

fn noise_suppression_name(n: NoiseSuppression) -> &'static str {
    match n {
        NoiseSuppression::Off => "off",
        NoiseSuppression::Light => "light",
        NoiseSuppression::Strong => "strong",
    }
}

fn processing_json(s: &VoiceChatSession, reset: bool) -> Value {
    let chain = s.processing();
    let requested = chain.settings();
    let st = s.processing_stats();
    let stages: Vec<Value> = STAGE_NAMES
        .iter()
        .zip(&st.stages)
        .map(|(name, t)| {
            json!({
                "name": name,
                "frames": t.frames,
                "bypassed": t.bypassed,
                "mean_us": t.mean_us(),
                "max_us": t.max_ns as f64 / 1000.0,
            })
        })
        .collect();
    json!({
        "requested": {
            "noise_suppression": noise_suppression_name(requested.noise_suppression),
            "echo_cancel": requested.echo_cancel,
            "agc": requested.agc,
        },
        "effective": s.status().effective_processing,
        "strong_available": false,
        "echo_reference": match chain.reference_source() {
            EchoReference::Playout => "playout",
            EchoReference::External => "external",
        },
        "reference_attached": chain.reference_attached(),
        "rate": chain.sample_rate(),
        "nonfinite_samples": st.nonfinite_samples,
        "frame_mismatches": st.frame_mismatches,
        "stages": stages,
        "aec": {
            "blocks": st.aec.blocks,
            "adapted_blocks": st.aec.adapted_blocks,
            "double_talk_blocks": st.aec.double_talk_blocks,
            "transfers": st.aec.transfers,
            "resets": st.aec.resets,
            "erle_db": st.aec.erle_db,
            "ffts": st.aec.ffts,
            "complex_macs": st.aec.complex_macs,
            "reference_frames": st.reference_frames,
            "reference_underruns": st.reference_underruns,
            "reference_dropped": st.reference_dropped,
        },
        "noise_suppression": {
            "hops": st.noise.hops,
            "ffts": st.noise.ffts,
            "noise_floor_db": st.noise.noise_floor_db,
            "mean_gain_db": st.noise.mean_gain_db,
        },
        "agc": {
            "frames": st.agc.frames,
            "gated_frames": st.agc.gated_frames,
            "limited_frames": st.agc.limited_frames,
            "gain_db": st.agc.gain_db,
            "applied_db": st.agc.applied_db,
            "noise_floor_db": st.agc.noise_floor_db,
            "gate_open": st.agc.gate_open,
        },
        "reset": reset,
    })
}

/// `voice_chat.processing`: reads and sets the capture processing stages (echo cancellation,
/// noise suppression, automatic gain; ADR-2608) and returns per-stage counters and measured cost.
/// `strong` noise suppression is accepted and runs as `light` (the network is not built).
/// `reset_stats: true` zeroes the counters after reading them.
pub fn processing(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_session(world, |s| {
        let f = fields(params)?;
        let noise = match opt_str(&f, "noise_suppression")? {
            None => None,
            Some("off") => Some(NoiseSuppression::Off),
            Some("light") => Some(NoiseSuppression::Light),
            Some("strong") => Some(NoiseSuppression::Strong),
            Some(other) => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`noise_suppression` must be off, light or strong, not \"{other}\""
                )));
            }
        };
        let echo = opt_bool(&f, "echo_cancel")?;
        let agc = opt_bool(&f, "agc")?;
        let reference = match opt_str(&f, "echo_reference")? {
            None => None,
            Some("playout") => Some(EchoReference::Playout),
            Some("external") => Some(EchoReference::External),
            Some(other) => {
                return Err(VoiceChatError::InvalidParams(format!(
                    "`echo_reference` must be playout or external, not \"{other}\""
                )));
            }
        };
        let reset = opt_bool(&f, "reset_stats")?.unwrap_or(false);
        let mut wanted = s.processing().settings();
        if let Some(n) = noise {
            wanted.noise_suppression = n;
        }
        if let Some(e) = echo {
            wanted.echo_cancel = e;
        }
        if let Some(a) = agc {
            wanted.agc = a;
        }
        if wanted != s.processing().settings() {
            s.set_processing(wanted);
        }
        if let Some(r) = reference
            && r != s.processing().reference_source()
        {
            s.set_echo_reference(r);
        }
        let out = processing_json(s, reset);
        if reset {
            s.reset_processing_stats();
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_audio::voice_chat::{VoiceChatSettings, VoiceChatUserSettings, VoiceQuality};
    use std::sync::Mutex;

    fn world() -> World {
        let settings = VoiceChatSettings {
            deterministic: true,
            quality: VoiceQuality::Narrow,
            ..VoiceChatSettings::default()
        };
        let session = VoiceChatSession::new(settings, VoiceChatUserSettings::default()).expect("session");
        let mut world = World::new();
        world.insert_resource(VoiceChatResource(Mutex::new(session)));
        world
    }

    fn tick(world: &World, frames: usize) {
        let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
        for _ in 0..frames {
            session.tick(false);
        }
    }

    fn reason(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn code(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .map(|d| d["code"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    #[test]
    fn every_method_needs_the_plugin() {
        let mut empty = World::new();
        let p = json!({});
        for result in [
            status(&mut empty, &p),
            set_channel(&mut empty, &p),
            mute(&mut empty, &p),
            loopback(&mut empty, &p),
            stats(&mut empty, &p),
            test_source(&mut empty, &p),
            simulate_network(&mut empty, &p),
            mic(&mut empty, &p),
            settings(&mut empty, &p),
            talk(&mut empty, &p),
            relay(&mut empty, &p),
            processing(&mut empty, &p),
        ] {
            let error = result.expect_err("no plugin");
            assert_eq!(
                (error.code, code(&error).as_str()),
                (protocol::UNSUPPORTED, "F0013")
            );
        }
    }

    #[test]
    fn status_starts_off_with_consent_false_and_push_to_talk() {
        let mut world = world();
        let out = status(&mut world, &json!({})).expect("status");
        assert_eq!(out["state"], "off");
        assert_eq!(out["consent"], false);
        assert_eq!(out["mode"], "push_to_talk");
        assert_eq!(out["channels"].as_array().map(Vec::len), Some(4));
        assert_eq!(out["speakers"], json!([]));
        assert_eq!(out["loopback"], false);
        assert_eq!(out["test_mode"], false);
    }

    #[test]
    fn a_fake_speaker_is_heard_and_counted() {
        let mut world = world();
        let joined = set_channel(
            &mut world,
            &json!({ "channel": 2, "join": true, "active": true, "radius": 30 }),
        )
        .expect("set_channel");
        assert_eq!(
            (
                joined["joined"].clone(),
                joined["active"].clone(),
                joined["radius"].clone()
            ),
            (json!(true), json!(true), json!(30.0))
        );
        let on = test_source(
            &mut world,
            &json!({ "kind": "sine", "freq_hz": 440, "as_sender": 7 }),
        )
        .expect("source");
        assert_eq!(on["enabled"], true);
        tick(&world, 20);
        let seen = status(&mut world, &json!({})).expect("status");
        assert_eq!(seen["speakers"][0]["sender"], 7);
        assert_eq!(seen["speakers"][0]["state"], "speaking");
        assert_eq!(seen["test_mode"], true);
        let counters = stats(&mut world, &json!({ "reset": true })).expect("stats");
        assert!(counters["totals"]["packets_received"].as_u64().unwrap_or(0) >= 18);
        assert_eq!(counters["speakers"][0]["sender"], 7);
        crate::contract::assert_result_named("voice_chat.stats", &counters);
        let after = stats(&mut world, &json!({})).expect("stats");
        assert_eq!(
            after["totals"]["packets_received"], 0,
            "reset zeroed the session counters"
        );
        test_source(
            &mut world,
            &json!({ "kind": "sine", "as_sender": 7, "enabled": false }),
        )
        .expect("remove");
    }

    #[test]
    fn simulated_loss_makes_the_receiver_conceal() {
        let mut world = world();
        set_channel(&mut world, &json!({ "channel": 4, "join": true, "active": true })).expect("join");
        let applied = simulate_network(
            &mut world,
            &json!({ "loss_pct": 20, "burst_len": 3, "latency_ms": 40, "jitter_ms": 20, "seed": 3 }),
        )
        .expect("simulate");
        assert_eq!(applied["applied"], true);
        test_source(&mut world, &json!({ "kind": "sine", "as_sender": 9 })).expect("source");
        tick(&world, 300);
        let counters = stats(&mut world, &json!({})).expect("stats");
        let s = &counters["speakers"][0];
        assert!(
            s["concealed"].as_u64().unwrap_or(0) + s["recovered"].as_u64().unwrap_or(0) > 0,
            "{counters}"
        );
    }

    #[test]
    fn loopback_runs_without_consent_for_synthetic_sources_only() {
        let mut world = world();
        set_channel(&mut world, &json!({ "channel": 1, "join": true, "active": true })).expect("join");
        let error =
            loopback(&mut world, &json!({ "enabled": true, "source": "mic" })).expect_err("no consent");
        assert_eq!(reason(&error).as_deref(), Some("consent_required"));
        let on = loopback(&mut world, &json!({ "enabled": true, "source": "sine" })).expect("loopback");
        assert_eq!(on["enabled"], true);
        {
            let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
            for _ in 0..20 {
                session.tick(true);
            }
        }
        let seen = status(&mut world, &json!({})).expect("status");
        assert_eq!(seen["loopback"], true);
        assert!(
            seen["speakers"]
                .as_array()
                .is_some_and(|s| s.iter().any(|sp| sp["sender"] == 65_535))
        );
        let off = loopback(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["enabled"], false);
    }

    #[test]
    fn mute_covers_self_deafen_sender_and_account() {
        let mut world = world();
        let own = mute(&mut world, &json!({ "target": "self", "muted": true })).expect("self");
        assert_eq!(
            (own["applied"].clone(), own["persisted"].clone()),
            (json!(true), json!(false))
        );
        mute(&mut world, &json!({ "target": "deafen", "muted": true })).expect("deafen");
        let account = mute(
            &mut world,
            &json!({ "target": { "account": "steam:1" }, "muted": true, "persist": true, "note": "loud" }),
        )
        .expect("account");
        assert_eq!(account["persisted"], true);
        {
            let session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
            assert!(session.user().muted_accounts.contains("steam:1"));
            assert!(session.user().deafen);
        }
        mute(
            &mut world,
            &json!({ "target": { "account": "steam:2" }, "muted": true, "block": true }),
        )
        .expect("block");
        {
            let session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
            assert!(session.user().blocked_accounts.contains("steam:2"));
        }
        let sender = mute(&mut world, &json!({ "target": { "sender": 7 }, "muted": true })).expect("sender");
        assert_eq!(sender["persisted"], false);
    }

    /// Spec 59 § 2.13: blocks are per account. A sender-slot block used to be ignored while the
    /// reply echoed `block: true`; now it is refused as invalid params and nothing changes.
    #[test]
    fn a_sender_slot_block_is_refused_not_silently_ignored() {
        let mut world = world();
        let error = mute(
            &mut world,
            &json!({ "target": { "sender": 7 }, "muted": true, "block": true }),
        )
        .expect_err("a slot cannot be blocked");
        assert_eq!(error.code, RpcError::invalid_params("x").code);
        assert_eq!(reason(&error), None, "plain F0002 for invalid params");
        let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
        assert!(!session.is_sender_silenced(7), "a refused call changes nothing");
        assert!(session.take_control().is_empty(), "no hint was queued");
    }

    #[test]
    fn the_microphone_needs_consent_the_protocol_cannot_give() {
        let mut world = world();
        let error = mic(&mut world, &json!({ "enabled": true, "fake": true })).expect_err("no consent");
        assert_eq!(reason(&error).as_deref(), Some("consent_required"));
        let refused = settings(&mut world, &json!({ "consent": true })).expect_err("never");
        assert_eq!(code(&refused), "F0002");
        assert_eq!(settings(&mut world, &json!({})).expect("read")["consent"], false);
    }

    #[test]
    fn a_fake_microphone_runs_the_capture_path_end_to_end() {
        let mut world = world();
        world
            .resource::<VoiceChatResource>()
            .0
            .lock()
            .expect("lock")
            .user_mut()
            .consent = true;
        set_channel(&mut world, &json!({ "channel": 4, "join": true, "active": true })).expect("join");
        settings(&mut world, &json!({ "mode": "open" })).expect("mode");
        let on = mic(
            &mut world,
            &json!({ "enabled": true, "fake": true, "rate": 44_100, "channels": 2 }),
        )
        .expect("mic");
        assert_eq!(
            (on["microphone"].clone(), on["state"].clone()),
            (json!("open"), json!("idle"))
        );
        tick(&world, 20);
        let counters = stats(&mut world, &json!({})).expect("stats");
        assert!(
            counters["totals"]["frames_encoded"].as_u64().unwrap_or(0) >= 15,
            "{counters}"
        );
        assert_eq!(
            status(&mut world, &json!({})).expect("status")["state"],
            "transmitting"
        );
        let off = mic(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["microphone"], "off");
    }

    #[test]
    fn settings_change_mode_gains_and_profile() {
        let mut world = world();
        let out = settings(
            &mut world,
            &json!({ "mode": "vad", "input_gain_db": -3, "output_gain_db": 6, "device": "usb" }),
        )
        .expect("settings");
        assert_eq!(out["mode"], "vad");
        assert_eq!(out["input_gain_db"], -3.0);
        assert_eq!(out["output_gain_db"], 6.0);
        assert_eq!(out["device"], "usb");
        settings(&mut world, &json!({ "quality": "high", "device": "" })).expect("quality");
        tick(&world, 1);
        let read = settings(&mut world, &json!({})).expect("read");
        assert_eq!(
            (read["quality"].clone(), read["device"].clone()),
            (json!("high"), Value::Null)
        );
        for bad in [
            json!({ "mode": "loud" }),
            json!({ "quality": "best" }),
            json!({ "input_gain_db": 99 }),
        ] {
            assert_eq!(code(&settings(&mut world, &bad).expect_err("bad")), "F0002");
        }
    }

    #[test]
    fn talk_presses_the_action() {
        let mut world = world();
        assert_eq!(
            talk(&mut world, &json!({ "pressed": true })).expect("talk")["pressed"],
            true
        );
        assert!(world.resource::<VoiceChatTalk>().0);
        assert_eq!(code(&talk(&mut world, &json!({})).expect_err("bad")), "F0002");
    }

    #[test]
    fn the_relay_reports_peers_and_counters() {
        use regia_net::model::PeerId;
        use regia_net::voice::VoiceRelaySettings;
        let mut world = world();
        let mut relay_core = VoiceRelay::new(VoiceRelaySettings::default(), &VoiceChatSettings::default());
        relay_core.add_peer(PeerId(3));
        world.insert_resource(relay_core);
        let out = relay(&mut world, &json!({})).expect("relay");
        assert_eq!(out["peers"][0]["peer"], 3);
        assert_eq!(out["forwarded"], 0);
    }

    #[test]
    fn refusals_carry_the_documented_reasons() {
        let mut world = world();
        let reason_of = |r: Result<Value, RpcError>| {
            let e = r.expect_err("refused");
            assert_eq!(code(&e), "F0002");
            reason(&e)
        };
        assert_eq!(
            reason_of(set_channel(&mut world, &json!({ "channel": 99, "join": true }))).as_deref(),
            Some("unknown_channel")
        );
        assert_eq!(
            reason_of(set_channel(&mut world, &json!({ "channel": 1, "active": true }))).as_deref(),
            Some("not_in_channel")
        );
        assert_eq!(
            reason_of(mute(
                &mut world,
                &json!({ "target": { "account": "a" }, "muted": true, "persist": false })
            ))
            .as_deref(),
            Some("unsupported")
        );
        assert_eq!(
            reason_of(mute(
                &mut world,
                &json!({ "target": { "sender": 5 }, "muted": true, "volume_pct": 50 })
            ))
            .as_deref(),
            Some("unknown_speaker")
        );
        // Plain F0002, no reason, for bad parameters.
        for result in [
            set_channel(&mut world, &json!({})),
            set_channel(&mut world, &json!({ "channel": 1, "join": "yes" })),
            mute(&mut world, &json!({ "target": "nobody", "muted": true })),
            mute(&mut world, &json!({ "target": "self" })),
            loopback(&mut world, &json!({ "enabled": true, "source": "tape" })),
            test_source(&mut world, &json!({ "kind": "saw" })),
            test_source(&mut world, &json!({ "kind": "sine", "as_sender": 65_535 })),
            test_source(&mut world, &json!({ "kind": "sine", "freq_hz": 5 })),
            simulate_network(&mut world, &json!({ "loss_pct": 99, "latency_ms": 0, "seed": 1 })),
            simulate_network(&mut world, &json!({ "loss_pct": 1 })),
            stats(&mut world, &json!([])),
        ] {
            let e = result.expect_err("invalid");
            assert_eq!(
                (e.code, code(&e).as_str(), reason(&e)),
                (protocol::INVALID_PARAMS, "F0002", None)
            );
        }
        // A shipping game turns the developer test mode off.
        world
            .resource::<VoiceChatResource>()
            .0
            .lock()
            .expect("lock")
            .set_test_mode_allowed(false);
        for result in [
            test_source(&mut world, &json!({ "kind": "sine" })),
            simulate_network(&mut world, &json!({ "loss_pct": 1, "latency_ms": 0, "seed": 1 })),
            loopback(&mut world, &json!({ "enabled": true, "source": "noise" })),
            mic(&mut world, &json!({ "enabled": true, "fake": true })),
        ] {
            assert_eq!(
                reason(&result.expect_err("closed")).as_deref(),
                Some("test_mode_only")
            );
        }
    }

    /// `voice_chat.processing` starts with every optional stage off, sets stages, reports
    /// `strong` as light, counts frames per stage, resets its counters, and refuses bad values
    /// with a plain `F0002`.
    #[test]
    fn processing_get_set_and_stats() {
        let mut world = world();
        let out = processing(&mut world, &json!({})).expect("read");
        assert_eq!(out["requested"]["noise_suppression"], "off");
        assert_eq!(out["requested"]["echo_cancel"], false);
        assert_eq!(out["requested"]["agc"], false);
        // Deterministic test mode bypasses even the high-pass.
        assert_eq!(out["effective"], json!([]));
        assert_eq!(out["stages"].as_array().map(Vec::len), Some(4));

        let out = processing(
            &mut world,
            &json!({ "noise_suppression": "strong", "agc": true, "echo_cancel": true }),
        )
        .expect("set");
        assert_eq!(out["requested"]["noise_suppression"], "strong");
        // No playback output is attached, so echo cancellation stays off; strong runs as light.
        assert_eq!(out["effective"], json!(["noise_suppression_light", "agc"]));
        assert_eq!(out["reference_attached"], false);
        assert_eq!(out["strong_available"], false);

        test_source(
            &mut world,
            &json!({ "kind": "noise", "level_db": -30.0, "seed": 3 }),
        )
        .expect("source");
        tick(&world, 10);
        let out = processing(&mut world, &json!({ "reset_stats": true })).expect("stats");
        let stage = |name: &str| {
            out["stages"]
                .as_array()
                .and_then(|a| a.iter().find(|s| s["name"] == name))
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(stage("noise_suppression_light")["frames"], 10);
        assert_eq!(stage("agc")["frames"], 10);
        assert_eq!(stage("echo_cancel")["frames"], 0);
        assert_eq!(out["noise_suppression"]["hops"], 20);
        assert_eq!(out["reset"], true);
        let out = processing(&mut world, &json!({})).expect("after reset");
        assert_eq!(out["agc"]["frames"], 0);

        // An external reference attaches once the host pushes a frame.
        let out = processing(&mut world, &json!({ "echo_reference": "external" })).expect("external");
        assert_eq!(out["echo_reference"], "external");
        assert_eq!(out["reference_attached"], false);
        world
            .resource::<VoiceChatResource>()
            .0
            .lock()
            .expect("lock")
            .push_echo_reference(&[0.0; 320]);
        let out = processing(&mut world, &json!({})).expect("attached");
        assert_eq!(out["reference_attached"], true);
        assert_eq!(
            out["effective"],
            json!(["echo_cancel", "noise_suppression_light", "agc"])
        );

        // The project settings follow, so `voice_chat.status` and a saved config agree.
        let out = status(&mut world, &json!({})).expect("status");
        assert_eq!(
            out["effective_processing"],
            json!(["echo_cancel", "noise_suppression_light", "agc"])
        );

        for bad in [
            json!({ "noise_suppression": "heavy" }),
            json!({ "agc": "yes" }),
            json!({ "echo_reference": "mic" }),
            json!([]),
        ] {
            let e = processing(&mut world, &bad).expect_err("invalid");
            assert_eq!(
                (e.code, code(&e).as_str(), reason(&e)),
                (protocol::INVALID_PARAMS, "F0002", None)
            );
        }
    }

    /// Spec 59 § 5 through the dispatcher: `voice_chat.status` names the `VoiceChatEmitter` entity
    /// of each speaker (`entity`, the entity bits; `null` without one).
    #[test]
    fn status_names_the_emitter_entity_of_each_speaker() {
        let mut world = world();
        set_channel(&mut world, &json!({ "channel": 2, "join": true, "active": true })).expect("join");
        test_source(&mut world, &json!({ "kind": "sine", "as_sender": 7 })).expect("seven");
        test_source(&mut world, &json!({ "kind": "sine", "as_sender": 9 })).expect("nine");
        let emitter = world
            .spawn(regia_audio::voice_chat::VoiceChatEmitter {
                sender: 7,
                ..Default::default()
            })
            .id();
        tick(&world, 20);
        let out = crate::dispatch::call_in_process(&mut world, "voice_chat.status", json!({}))
            .expect("synchronous")
            .expect("status");
        let speakers = out["speakers"].as_array().cloned().unwrap_or_default();
        let entity_of = |sender: u64| {
            speakers
                .iter()
                .find(|s| s["sender"] == sender)
                .map(|s| s["entity"].clone())
        };
        assert_eq!(entity_of(7), Some(json!(emitter.to_bits())), "{out}");
        assert_eq!(entity_of(9), Some(Value::Null), "no emitter: null, {out}");
    }

    /// Spec 59 § 5 through the dispatcher: the codes a running session meets without an agent call
    /// to refuse (`bad_packet`, `codec_error`) are reported by `voice_chat.status` in `errors`, one
    /// record per code with its count and last frame, and `voice_chat.stats { reset }` clears them.
    #[test]
    fn stream_error_codes_reach_the_agent_through_status() {
        let mut world = world();
        set_channel(&mut world, &json!({ "channel": 2, "join": true })).expect("join");
        {
            let mut session = world.resource::<VoiceChatResource>().0.lock().expect("lock");
            session.receive_packet(&[0xFF, 0, 1]);
            for seq in 0..4u16 {
                let mut wire = Vec::new();
                regia_audio::voice_chat::VoicePacket {
                    flags: regia_audio::voice_chat::VoiceFlags::default(),
                    sender: 8,
                    channel: 2,
                    seq,
                    timestamp: u32::from(seq).wrapping_mul(regia_audio::voice_chat::FRAME_TIMESTAMP_STEP),
                    quality: VoiceQuality::Narrow,
                    codec: regia_audio::voice_chat::VoiceCodecId::Pcm,
                    payload: &[0x7F, 0, 0],
                }
                .write(&mut wire);
                session.receive_packet(&wire);
            }
        }
        tick(&world, 8);
        let out = crate::dispatch::call_in_process(&mut world, "voice_chat.status", json!({}))
            .expect("synchronous")
            .expect("status");
        let codes: Vec<&str> = out["errors"]
            .as_array()
            .map(|a| a.iter().filter_map(|e| e["code"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(codes, ["bad_packet", "codec_error"], "{out}");
        assert_eq!(out["errors"][0]["count"], 1);
        assert!(out["errors"][1]["last_frame"].as_u64().is_some());
        for code in &codes {
            assert!(regia_audio::voice_chat::ERROR_CODES.contains(code));
        }
        crate::contract::assert_result_named("voice_chat.status", &out);
        stats(&mut world, &json!({ "reset": true })).expect("reset");
        let out = status(&mut world, &json!({})).expect("status");
        assert_eq!(out["errors"], json!([]));
    }
}
