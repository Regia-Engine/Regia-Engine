# Spec 59: Voice chat

Status: proposed (ADR-1579). Target rung: v0.5.0, with multiplayer (`specs/22` § T2.3). The capture and playback baseline already exists (`specs/16` § 3b); the codec, packetization, jitter handling, channels, moderation and agent surface are new.

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_audio::voice` (capture, VAD, `VoiceStreamData`), `AudioUserSettings` in `regia_audio::settings`; on `integ/ext2` (ADR-1563 f) the agent methods `audio.voice`, `audio.capture_start/stop`, `audio.set_talking`, `audio.voice_loopback_start/stop` (`book/agents/voice.md`). Codec, packetizer, jitter buffer, channels and moderation are not built anywhere.

Source area: runtime-online/runtime-online-overview, runtime-online/runtime-online-backends, runtime-audio/runtime-audio-mixing, runtime-audio/runtime-audio-overview, runtime-audio/runtime-audio-spatial.

## 1. Purpose and scope

Players in a multiplayer session need to talk to each other: a team radio, nearby speech that gets quieter with distance, a party line that follows a group between sessions. This spec defines microphone capture, the voice codec, the packet format, loss handling, channel routing, moderation and privacy for that, and how each piece is driven and verified by an agent without a human speaking.

It is not a third-party voice SDK adapter, not speech-to-text or text-to-speech, not a platform party/overlay system (those arrive through `regia_steam` and friends and may feed the same channel API), and not a recorder.

Naming: in `regia_audio`, "voice" has two older meanings. `voices.rs` manages *mixer voices* (playing sounds, virtualization, the voice budget), and `voice.rs` holds the capture and playback baseline. To avoid a clash, this feature is always called **`voice_chat`** in code and config, types are `VoiceChat*`, agent methods use the prefix `voice_chat.*` (they never touch mixer voices; mixer-voice methods and the built capture methods stay under `audio.*`). Prose below says "mixer voice" for the other meaning.

**Decision (ADR-1585): supersedes the "game's job" sentence.** `specs/16` § 3b and the `opus` row of `docs/03` § 7 say voice encoding and transport belong to the game. This spec moves the codec, packet format, jitter buffer, channels and moderation into the engine, with the network glue left to a transport trait (§ 3). The builder amends that sentence in `specs/16` § 3b and the `docs/03` row in the same change as the ADR. A game that prefers its own voice core keeps working: it uses only the baseline and ignores `VoiceChatPlugin`.

Baseline versus new:

| Piece | Baseline (exists, `regia_audio::voice`) | New in this spec |
|---|---|---|
| Capture | `AudioCapture`: device pick, mono f32 frames over a bounded channel, level meter, `dropped_frames` | 20 ms framing, 16/24/48 kHz output, permission flow, consent gate, suppressor hooks |
| Activity | `VadSettings` / `VoiceActivity` energy detector with hysteresis; push-to-talk flag | Mode selection (open, VAD, push-to-talk, toggle), DTX end-of-talk signal |
| Playback | `VoiceStreamData` with prebuffer jitter and linear resample | Adaptive jitter buffer, concealment, per-speaker mixing, spatial emitter |
| Codec, transport, channels, moderation | none | all of it |

## 2. Regia terms and model

### 2.1 Nouns

- **Voice chat session** (`VoiceChatSession`, resource): the local participant's state: consent, capture state, selected channels, mute lists, statistics. One per local player slot that has a microphone (split-screen: only slot 0 by default).
- **Speaker** (`VoiceSpeaker`): a remote participant with a sender id (the 16-bit session slot the server assigned), a display name hook, an optional world entity, a jitter buffer, a decoder and a gain.
- **Channel** (`VoiceChannelId`, `u16`): a named audience. Kinds:

| Kind | Audience | Routing | Spatial |
|---|---|---|---|
| `team` | players on the same team id | server membership | no (flat stereo, centered) |
| `proximity` | players within `radius` of the speaker | server relevancy by distance | yes: attenuation, pan, optional occlusion and medium effects through the existing spatial path |
| `party` | explicit member list (friends, squad) | server membership, survives level change | no |
| `global` | every player in the session | server | no |
| `custom` | game-defined, created and destroyed at runtime | server membership | per channel flag |

A speaker transmits into exactly one **transmit channel** at a time (the active one), and listens to all channels the player has joined. A packet carries one channel id; a game wanting simultaneous team and proximity speech sends one packet per target channel only if the budget allows (default: no, the active channel wins).

- **Voice frame**: 20 ms of mono audio (320 samples at 16 kHz, 480 at 24 kHz, 960 at 48 kHz). The frame is the unit of encoding, loss and playout.
- **Bandwidth profile** (`VoiceQuality`): `narrow` (16 kHz, target 12 kbps), `standard` (24 kHz, target 20 kbps), `high` (48 kHz, target 32 kbps). All mono, constant frame size, variable bit rate with a cap.

### 2.2 Local transmit state machine

States: `Off` (no consent or no device), `Idle` (device open, not transmitting), `Transmitting`, `Hangover` (activity ended, tail still sent), `Muted` (self mute; device stays open for the meter but nothing is encoded), `Error`.

| From | Event | To | Notes |
|---|---|---|---|
| Off | consent granted and device opened | Idle | permission prompt happens here on Android |
| Off | permission denied / no device | Error | code `permission_denied` / `no_input_device`; retry only on explicit user action |
| Idle | open-mic mode, or VAD active, or PTT/toggle on | Transmitting | |
| Transmitting | VAD inactive (or PTT released) | Hangover | hangover default 200 ms, i.e. 10 frames |
| Hangover | activity returns | Transmitting | |
| Hangover | tail elapsed | Idle | the last packet carries `end_of_talk` |
| any live state | self mute on | Muted | `end_of_talk` sent if mid-speech |
| Muted | self mute off | Idle | |
| any | device removed | Error, then auto-retry to Idle when a device appears | at most once per 2 s counted in simulation ticks |

During silence in VAD mode, nothing is sent (discontinuous transmission); the receiver derives silence from the `end_of_talk` flag and from a gap in the timestamp. In open-mic mode silence frames are sent as 1-byte comfort frames at most one per 400 ms so liveness is visible.

### 2.3 Remote speaker state machine

`Silent` -> `Buffering` (first packet seen, jitter buffer prefilling) -> `Speaking` -> `Draining` (end_of_talk received, buffer playing out) -> `Silent`. A speaker with no packet for 1 s without an `end_of_talk` is forced to `Silent` ("timed out", counted in stats). A speaker whose sender id is reused by a different account is reset (decoder and buffer discarded) when the session announces the rebinding.

### 2.4 Limits and edge cases

- At most 8 speakers are decoded and mixed per listener at once (`max_mixed_speakers`, default 8, range 1..=16). Overflow ranks speakers by `priority class` (team above proximity above party above global is the default, configurable), then by recent energy, then by lower sender id; ranking is re-evaluated every 10 frames with hysteresis so the audible set does not flap. Dropped speakers are neither decoded nor buffered.
- Maximum transmit rate per sender enforced by the server: 60 packets/s average, bursts of 10. Excess is dropped and counted.
- Maximum packet payload 400 bytes; larger encoder output is a bug (the encoder is capped, see § 2.6).
- Sequence numbers wrap at 65536; timestamps wrap at 2^32; comparison is serial-number arithmetic.
- A packet for a channel the listener has not joined is dropped at the client and counted (`wrong_channel`); the server also filters, so this indicates a membership race only.
- Sample rate or channel count changes mid-session (device hot-swap) re-open capture and restart the encoder; the next packet carries a `reset` flag, listeners flush their decoder.
- Self-hearing: a player never receives their own packets from the server (except in loopback, § 2.12).

### 2.5 Capture and format conversion

- Input uses the cpal-class input stream baseline. Whatever the device delivers (any rate, 1 or more channels, i16/f32) is converted on the capture side: channels are averaged to mono; sample rate is converted to the profile rate with a windowed-sinc polyphase resampler (fixed ratio per device, state kept across callbacks); everything lands on the game thread through the bounded lock-free channel from the baseline. Nothing in the audio callback allocates or blocks; overruns increment `dropped_frames`.
- Re-framing: an accumulator emits exact 20 ms frames; leftover samples carry over; on stream restart the accumulator is cleared and the first frame is zero-padded rather than stretched.
- Device selection by stable device name string with a "system default" entry; a saved name that no longer exists falls back to default and logs once.
- Permission prompts: on Android the first transition out of `Off` calls the runtime permission request for microphone recording through the platform seam (`regia_platform`), once per app launch unless the user chose "don't ask again", in which case the state goes to `Error(permission_denied)` and `voice_chat.status` carries `can_prompt: false` so the UI can deep-link to system settings. Desktop OSes with a privacy gate (macOS, sandboxed packages) surface the same states. No prompt is shown by merely compiling the feature in or by opening a settings screen; only by an explicit enable action.

### 2.6 Voice activity, push-to-talk, toggle

- Modes: `open` (always transmit), `vad`, `push_to_talk`, `toggle`. Default `push_to_talk` on desktop and Android alike (lowest accidental-leak risk), changeable in settings.
- VAD baseline: short-term RMS with on/off thresholds, hysteresis and hangover (the ITU-T P.56 style active-speech idea, already implemented). New: an adaptive noise floor (slow minimum tracking over 5 s) shifts both thresholds by a fixed margin when `vad_adaptive` is on; fixed thresholds otherwise. Optionally the encoder's own DTX/activity hint is OR-ed in; the energy detector alone is authoritative for tests.
- Push-to-talk and toggle are driven by an input action `voice_chat_talk` in the player's input contexts (`specs/11`), so rebinding, gamepad and touch buttons work like any other action. Release has a short grace (default 150 ms) to avoid clipped word endings.

### 2.7 Optional processing (no machine learning required)

A processing chain runs between conversion and encoding, each stage disabled by default except the high-pass.

1. **High-pass** at 80-100 Hz (2-pole), removes rumble and DC. On.
2. **Noise suppression** (`noise_suppression: off | light | strong`). `light` is a classic spectral-subtraction/Wiener-style gate with minimum-statistics noise tracking (public algorithm family: Martin 2001). `strong` may use a small recurrent network of the RNNoise class (BSD-licensed, public paper; see § 9); it is a **proposed optional feature flag** `voice-denoise-rnn`, off in default builds, and nothing in this spec requires it. If the feature is not compiled in, `strong` silently behaves as `light` and `voice_chat.status` reports the effective mode.
3. **Echo cancellation** (`echo_cancel: off | on`): a frequency-domain adaptive filter in the style of the public WebRTC-family AEC (partitioned block frequency-domain NLMS plus a residual suppressor and delay estimation). Needs a reference signal: the final mixed output of the game, tapped after the bus tree on the audio thread into a ring buffer with the output latency recorded. It is only useful with open speakers; the default stays off, and headset users do not need it. A missing reference (no output device) turns it off with a warning.
4. **Automatic gain** (`agc: off | on`): slow digital gain toward a target of about -20 dBFS RMS, max +18 dB, with a hard limiter, applied only while speech is detected.

*Implementation status (ADR-2608):* all four steps run. The chain order is high-pass, echo cancellation, noise suppression, automatic gain (echo cancellation before the non-linear suppressor), then voice activity and the encoder; the numbered list above is not the order. `strong` noise suppression runs as `light` (the network is not built) and `effective_processing` reports `noise_suppression_light`. Echo cancellation takes its reference from the session's own voice playout while an output is attached, or from frames the host pushes (`EchoReference::External`, the bus-tree tap); the bus-tree tap itself and delay estimation are follow-ups, so an external reference must be pushed one frame per tick. The automatic gain also holds during far-end single talk so it never boosts residual echo. `voice_chat.processing` sets the stages and reads per-stage counters.

All stages operate on 20 ms frames, are allocation-free after construction and are skipped (bypass, not silence) on any internal error. Their combined cost has a budget in § 10.

### 2.8 Codec and packetization

- **Codec**: Opus (BSD-3-Clause), voice application mode, mono, 20 ms frames, in-band forward error correction on, DTX off in the codec (silence handled by § 2.2), VBR with a constrained maximum bit rate per profile, complexity 5 by default (range 0..=10, lower on min-spec Android, see § 10). With `PcmCodec` the bit rates of § 2.1 do not apply (about 3x), FEC and PLC are provided by the generic jitter-buffer repeat-and-fade concealment instead.
- **Packet** (little-endian, sent on the `regia_net` *unreliable* channel; never fragmented):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | `version` (1) and `flags` in the high bits: `end_of_talk`, `reset`, `priority`, `multi` |
| 1 | 2 | `sender` (session slot, server overwrites it with the authenticated slot) |
| 3 | 2 | `channel` |
| 5 | 2 | `seq` (+1 per frame, frames not sent in silence still consume a number when the gap is under 1 s, otherwise a talk-spurt start flag resets timing) |
| 7 | 4 | `timestamp` in 48 kHz sample units regardless of profile |
| 11 | 1 | low nibble `quality` (profile), high nibble codec id (0 PCM, 1 Opus) |
| 12 | n | Opus payload |

  Header 12 bytes. Note: the frame-count field of earlier drafts is dropped; a two-frame packet
  (`frames_per_packet = 2`, or one frame plus `redundancy` — the two settings are mutually exclusive,
  see the Redundancy paragraph below) sets flag bit `multi` and begins the payload with one count byte, so single-frame
  packets stay at 12 bytes of header. The packet sits inside the transport's own authenticated encryption; voice adds no cipher. Clients ignore unknown flag bits and drop `version` greater than they know (counted).
- **Redundancy**: besides Opus in-band FEC, the sender may append the previous frame's payload when
  `redundancy: true` and the measured loss is above 10 % (second frame in the same packet, `multi`
  flag set, count byte = 2). Default `false`; loss above 10 % is rare and doubles bit rate.
  Redundancy requires `frames_per_packet: 1`: a two-frame packet has no slot for the spare, so
  `validate` rejects the combination.
- **Adaptive bit rate**: every 2 s the sender may step the target bit rate between the profile's floor (60 % of target) and ceiling (the target) using the loss and RTT reported by the transport. No change in channel kind or sample rate happens mid-talk.

### 2.9 Receive path: jitter buffer and concealment

- One **adaptive jitter buffer** per speaker, keyed by `seq`. Target depth starts at 60 ms (3 frames), is bounded 40..=200 ms, and follows the RFC 3550 interarrival-jitter estimate: `target = clamp(2.5 * jitter + one frame, min, max)`, updated at talk-spurt boundaries (never mid-word, except to grow quickly on an underrun).
- Playout is driven by the audio render clock in 20 ms frames, resampled to the device rate by the baseline resampler. A late packet (older than the playout cursor) is dropped and counted; duplicates are dropped and counted; reordered packets inside the window are slotted.
- **Loss handling per missing frame**: (1) if the next packet is already buffered, use its in-band FEC data to recover the missing frame; (2) else run the decoder's packet-loss concealment for that frame (with `unsafe-libopus` 0.2.0 that is a Rust pitch repeat in `regia_media::voice_opus`: the library's own is broken, ADR-1911); (3) after 5 consecutive concealed frames (100 ms), fade out over the next frame and output silence until real data returns, with a 20 ms crossfade on resume.
- **Time scaling**: when the buffer exceeds target by two frames, drop one pitch period of audio (WSOLA-style overlap-add) rather than a whole frame; when under target by one frame, stretch. Both off in the deterministic mode (§ 7).
- **Clock drift** between capture and playout is absorbed by the same time scaling; the long-run drift estimate appears in stats (`drift_ppm`).

### 2.10 Channels, topology and why server-relayed

Default topology is **server-relayed over the `regia_net` unreliable channel**: every client sends to the server, the server forwards the unmodified Opus packet to each eligible listener. The server never decodes (no mixing, no codec dependency on dedicated servers; cost is a copy per listener).

Justification: (1) peers behind NAT would each need traversal and a mesh of N-1 uplinks, whereas one uplink per client keeps upload at one stream even in a 16-player match; (2) the server already holds authoritative membership, team ids, positions (for proximity) and mutes, so routing is enforced centrally and a client cannot eavesdrop on a channel it is not in or fake a sender id; (3) the transport already provides encryption, connection tokens and congestion feedback; (4) the server can rate-limit abuse; (5) no client reveals its IP address to other players. Cost: server downlink fan-out, which § 2.11 bounds.

**Peer topology** is a later opt-in transport (`VoiceTransport::Peer`, follow-up): for tiny LAN or two-player sessions where no server exists. The packet format is identical; only the routing layer changes. Out of scope for the first delivery.

Server routing rules per incoming packet from sender S on channel C, per candidate listener L:

1. L is joined to C (membership table).
2. Neither side has blocked the other (§ 2.13). L's *mute* of S is local and does not affect routing (but saves downlink if L reports it: the client sends a `mute_hint` so the server stops forwarding S to L).
3. For `proximity`: distance between S's and L's replicated positions is below `radius + slack` (slack 5 m to absorb movement); the server evaluates once per 100 ms and caches. Listeners on other levels or dead (game-defined flag) are excluded when the game sets the `VoiceChatRelevancy` hook.
4. L's per-listener cap: at most `max_mixed_speakers` + 2 senders are forwarded concurrently; surplus senders by the ranking of § 2.4 are dropped at the server so a crowded proximity area does not flood a client.

Channel membership changes are a small reliable message (join/leave/set-active); voice packets themselves are never reliable.

### 2.11 Bandwidth budgets

Per speaker, wire cost including a 12-byte voice header, 16-byte transport authentication tag and 28 bytes IP/UDP, at 50 packets/s while talking:

| Profile | Payload at target | Bytes per packet | Kbit/s on the wire |
|---|---|---|---|
| narrow | 12 kbps = 30 B | ~86 | ~34 |
| standard | 20 kbps = 50 B | ~106 | ~42 |
| high | 32 kbps = 80 B | ~136 | ~54 |

The header and framing overhead dominates narrow profiles; this is why 20 ms frames (not 10) are used and why two-frame packets (40 ms) are an optional `frames_per_packet = 2` setting for mobile data (halves the overhead, adds 20 ms latency). Budgets: client uplink while talking <= 56 kbps on `high`, <= 36 kbps on `narrow`; client downlink <= `(max_mixed_speakers + 2) x` per-speaker figure, i.e. about 420 kbps worst case at `standard`, typical 2-3 simultaneous speakers = 130 kbps. Server downlink = sum over listeners; a 16-player session with 3 concurrent speakers and no filtering is 16 x 3 x 42 = ~2 Mbit/s. The server publishes the sums in stats and may force the `narrow` profile for all senders when a configured downlink ceiling is exceeded (`quality_cap` in the channel membership message).

### 2.12 Proximity speech through the spatial path

For `proximity` (and any custom channel with `spatial: true`), each speaker with a world entity gets a **chat emitter**. The speaker's decoded audio is placed by `regia_audio::spatial::spatialize`: distance attenuation (logarithmic rolloff, `db_per_doubling` default 6, min distance 2 m, max distance the channel radius, default 25 m), pan, and a cone that is off (inner and outer angles 360°, outer gain 1). Doppler factor is 0 and the emitter velocity passed in is zero, so speech is not pitch-shifted. The optional `VoiceChatEmitter.air_absorption` flag (default false) is that spatialize low-pass, which stays at 20 kHz until the min distance and then ramps linearly to 2 kHz at the max distance. It is not the level propagation medium. Occlusion applies when an occlusion provider is present and the emitter's `occlusion` flag is set (default true): a smoothed gain and cutoff. Proximity chat does not use the level propagation medium: there is no path delay, no medium-dependent level offset, and no silence in an airless medium. Output goes to the `Voice` bus (exempt from announcer ducking per `specs/16` § 3a). A speaker without a resolved entity (not yet replicated) plays flat at -6 dB until the entity appears; the switch crossfades over 100 ms. Channels that are not spatial play through a per-channel pan-center mixer node to the `Voice` bus.

**Listener**: the existing `AudioListener`. In split-screen the nearest-listener rule from `specs/16` § 3 decides per emitter.

**Loopback** (`voice_chat.loopback on`): local capture passes through processing, the real encoder and decoder, the packet serializer and parser, the simulated network (§ 7, loss/jitter settings apply) and the receive path, then plays as a speaker with a reserved sender id (`0xFFFF`) in no spatial channel. It never touches `regia_net`. It is the end-to-end test of the whole chain and is the only way the player can hear their own voice; it auto-disables after 60 s (counted in ticks) and is off at launch.

### 2.13 Moderation, mute and block

- **Local mute**: per account id (not per session slot, so it survives reconnects). Silences S for this listener only: packets dropped before decode, a `mute_hint` sent to the server. Stored in the persistent user settings (`specs/16` settings) under `voice_chat.muted_accounts` (list of account id strings with an optional display-name note), cap 512 entries, oldest evicted. A muted account that speaks produces no audio and no caption.
- **Block**: stronger than mute; mutual. Blocked accounts are neither heard nor hear this player; enforced at the server for the lifetime of the session using the lists the clients register on join, and persisted with the same cap in `voice_chat.blocked_accounts`. When the platform/online service provides its own block list it is merged in (union).
- **Per-speaker volume**: 0..=200 % gain, session-scoped, not persisted by default.
- **Self mute / deafen**: mute stops transmit; deafen (`deafen`) additionally silences all playback and implies mute.
- **Report hook**: `VoiceChatEvent::ReportRequested { sender, channel, seconds }` is raised by the game's UI. Regia never records audio on its own. If the project opts in with `report_buffer_seconds > 0` (default 0, hard cap 30), the *client* keeps a ring buffer of the **last N seconds of that speaker's already-encoded packets** in memory, and only on a report action hands the bytes to a game-supplied sink (`VoiceReportSink` trait) which the game must route to its moderation backend with the user's knowledge. The buffer is cleared on session end and never written to disk by the engine.
- **Server moderation hooks**: `VoiceChatModeration` trait on the server: `allow_transmit(sender, channel) -> bool` (called on membership changes and at most once per second per sender), `on_report(report)`, and a forced server mute (`ServerMuted { sender, until_tick }`) that the server enforces by dropping packets and notifies the client of.

### 2.14 Accessibility and caption hook

Speech-to-text is out of scope. The hook: `VoiceChatEvent::SpeakerActivity { sender, channel, active, level }` (talk-spurt start/end, level 0..1, debounced to 10 Hz) and `VoiceChatEvent::DecodedFrame` (opt-in `emit_decoded_frames`, delivers `Arc<[f32]>` mono PCM per 20 ms to a game-provided listener) let a game attach its own captioning, visualization or "speaking" icon. Every speaking state is also exposed as a visual indicator component (`VoiceChatSpeaking` on the speaker entity) so the UI never has to rely on audio alone. Text size and contrast of any built-in HUD follow the UI scale settings.

### 2.15 Privacy defaults

- Microphone **off until the user opts in**: `consent` is `false` in a fresh profile; the device is never opened (not even for a meter) before consent. Consent is a persisted user setting and a per-session confirmation is a game choice. The agent protocol cannot grant consent on behalf of the user in a shipping build (§ 5).
- Transmission defaults to push-to-talk; the transmit indicator is always shown when the game uses the built-in HUD (game-built HUDs are expected to follow the same rule).
- **Never record to disk by default.** No call in this spec writes voice audio to a file. The only audio retention is the opt-in in-memory report ring (§ 2.13) and the developer-only `voice_chat.loopback_capture` of the test mode (synthetic sources only, § 7).
- Account identifiers and IP addresses are never shown to other players by this feature; the server relay hides addresses.
- Logs never contain audio; they contain counters and sender slots.

## 3. API shape in Rust

Crates and layers (`docs/02-architecture.md`): everything that does not touch the network lives in **`regia_audio::voice_chat`** (layer 2, no new crate). `regia_net` is built (layer 2, ADR-1600); its `voice` module sits behind the `voice` feature (ADR-1911).

**Decision (ADR-1585): layering.** `regia_audio` never depends on a network crate. The seam is the `VoiceChatTransport` trait below, implemented (a) by `VoiceChatSimulator`/loopback inside `regia_audio` for tests, (b) by the game, or by `regia_steam` for a platform relay, today, and (c) by a `voice` module of `regia_net` (layer 3, gameplay, depends on `regia_audio` and `regia_framework`) when that crate is built. No separate `regia_voice` crate. The server-side routing function (§ 2.10) is pure logic over a membership table and also lives in `regia_audio::voice_chat::routing`, so it is testable now and reusable by any server.

**Decision (ADR-1585, amended by ADR-1911): codec dependency.** The codec sits behind a `VoiceCodec` trait. Two implementations: `PcmCodec` (always built; 16-bit little-endian PCM with a 1-byte mode header, about 3x the Opus bit rate; used by tests, loopback and as a fallback when negotiated) and `OpusVoiceCodec` in `regia_media` behind its off-by-default cargo feature `voice-opus` (ADR-1593 keeps every media codec there); it reaches a session through `VoiceCodecProvider`. Opus is BSD-3-Clause and royalty free. `unsafe_code = "deny"` is unaffected (unsafe is inside the dependency). Dependency rules: the binding must build with `cc` only (no cmake, no system libopus at runtime) and cross-compile to Android aarch64; a binding that needs cmake or a system library is rejected. Candidates, in order: a pure-Rust or transpiled libopus crate with a permissive licence if it passes the conformance vectors of RFC 6716; otherwise a binding that vendors libopus and builds it with `cc`. The builder picks one that passes `cargo deny check` (all features), adds the `docs/03` row (replacing the "game-side" `opus 0.4` row) and records the crate and version in an ADR. If none qualifies, the feature stays unbuilt and shipping builds use `PcmCodec`, which keeps every other part of this spec testable. The same `VoiceCodec` seam lets a game supply its own codec.

```rust
// regia_audio::voice_chat
pub struct VoiceChatPlugin;                    // adds systems in PreUpdate (drain), PostUpdate (play out)
pub struct VoiceChatSettings { /* serde + Reflect: mode, quality, processing, device, jitter bounds,
                                 max_mixed_speakers, frames_per_packet, redundancy, ... */ }
pub struct VoiceChatSession { /* resource */ }
pub struct VoiceChannel { pub id: VoiceChannelId, pub kind: VoiceChannelKind, pub name: String,
                          pub radius: f32, pub spatial: bool, pub priority: u8 }
pub enum VoiceChannelKind { Team, Proximity, Party, Global, Custom }
pub enum VoiceQuality { Narrow, Standard, High }
pub enum VoiceMode { Open, Vad, PushToTalk, Toggle }

pub trait VoiceCodec {                         // one encoder+decoder state per stream; PcmCodec always, OpusCodec with `voice-opus`
    fn encode(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, VoiceChatError>;
    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<usize, VoiceChatError>; // None = conceal
}
pub fn new_codec(id: VoiceCodecId, q: VoiceQuality, complexity: u8) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError>;
pub enum VoiceCodecId { Pcm, Opus }            // carried in the packet `quality` byte's high nibble
pub struct VoicePacket<'a> { pub flags: VoiceFlags, pub sender: u16, pub channel: u16, pub seq: u16,
                             pub timestamp: u32, pub quality: VoiceQuality, pub payload: &'a [u8] }
impl VoicePacket<'_> { pub fn parse(bytes: &[u8]) -> Result<VoicePacket<'_>, VoiceChatError>;
                       pub fn write(&self, out: &mut Vec<u8>); }
pub struct JitterBuffer;                       // pure state machine; time comes from the caller's tick counter
pub trait VoiceChatTransport {                 // implemented in regia_net::voice, or by the test simulator
    fn send(&mut self, packet: &[u8]);
    fn poll(&mut self) -> Option<Vec<u8>>;
}
pub trait VoiceReportSink { fn submit(&mut self, report: VoiceReport); }
pub struct VoiceChatSimulator { /* § 7 */ }

pub enum VoiceChatEvent { StateChanged, SpeakerActivity {..}, DecodedFrame {..},
                          ReportRequested {..}, PermissionResult {..}, Error(VoiceChatError) }

#[derive(thiserror::Error)] pub enum VoiceChatError { /* § 5 codes */ }
```

All settings and channel definitions derive `serde` and `Reflect`; the encoder, decoder and jitter buffer are plain Rust types usable without a `World` so tests and tools can run them directly. `VoiceChatSession` has no `Reflect` on the runtime fields (device handles); the persisted subset (`VoiceChatUserSettings`) does.

Systems and ordering: `PreUpdate`: drain capture, run VAD/PTT, run processing, encode, hand to transport. `PreUpdate` after the network poll: parse, route into jitter buffers. `PostUpdate/AudioSync`: update chat emitters from `GlobalTransform`. The audio render path pulls decoded frames from jitter buffers at the device clock; it never calls the transport.

## 4. Files and formats

- **`config/voice_chat.ron`** (project defaults, `format_version: 1`): `(format_version: 1, quality: Standard, mode: PushToTalk, frames_per_packet: 1, max_mixed_speakers: 8, jitter: (min_ms: 40, max_ms: 200, start_ms: 60), processing: (noise_suppression: Off, echo_cancel: false, agc: false), report_buffer_seconds: 0, channels: [(id: 1, name: "team", kind: Team, priority: 3), (id: 2, name: "nearby", kind: Proximity, radius: 25.0, spatial: true, priority: 2), (id: 3, name: "party", kind: Party, priority: 1), (id: 4, name: "all", kind: Global, priority: 0)])`. Channel ids are stable across a project's lifetime and part of the wire format.
- **User settings** `voice_chat` section in the persisted user settings: `consent`, `device`, `mode`, `input_gain_db`, `output_gain_db`, `muted_accounts`, `blocked_accounts`, `deafen`. Missing keys take defaults; unknown keys are kept (round-trip) so a newer build's file survives an older build.
- **Wire**: § 2.8, `version` 1.
- **Migration**: a v2 of any of these gets a migrator registered with the asset/config migration path (`specs/12` § 4a) and `regia upgrade-project` rewrites `config/voice_chat.ron` in place with a backup; the wire `version` field is separate and negotiated by taking the minimum both sides announce at join.
- No cook output: the config is copied into the pak as-is.

## 5. Agent-protocol surface

Access classes follow `specs/23`: *read* observes, *play* acts in a running game. All methods work in a headless dev build with synthetic sources and no audio device. Error codes are lowercase ascii/digits/underscore.

| Method | Class | Params | Result |
|---|---|---|---|
| `voice_chat.status` | read | none | `{ state, consent, can_prompt, mode, quality, device, rate, channels: [{id, name, kind, joined, active}], speakers: [{sender, state, entity?, muted, blocked, level}], effective_processing, loopback, test_mode }` |
| `voice_chat.set_channel` | play | `{ channel, join?: bool, active?: bool, radius?: f32 }` | updated channel record; `active: true` makes it the transmit channel |
| `voice_chat.mute` | play | `{ target: "self" \| "deafen" \| {sender} \| {account}, muted: bool, block?: bool, persist?: bool }` | `{ applied, persisted }` |
| `voice_chat.loopback` | play | `{ enabled: bool, source?: "mic" \| "sine" \| "noise" }` | `{ enabled, source }` |
| `voice_chat.stats` | read | `{ reset?: bool }` | counters of § 10 and below per speaker and totals |
| `voice_chat.test_source` | play | `{ kind: "sine" \| "noise" \| "silence", freq_hz?, level_db?, seed?, as_sender? }` | `{ enabled }`; injects synthetic frames in place of the microphone, or as a fake remote speaker when `as_sender` is set |
| `voice_chat.simulate_network` | play | `{ loss_pct, burst_len?, reorder_pct?, dup_pct?, latency_ms, jitter_ms, seed }` | `{ applied }` |

Consent: `voice_chat.status` reports it; no method sets it to true on a real microphone. In builds with the dev agent feature, `voice_chat.test_source` and loopback with a synthetic source work without consent because they never open the device; selecting `source: "mic"` without consent fails with `consent_required`.

Error shape: `F0013` when `VoiceChatPlugin` is missing; every other error is `F0002` with `data.reason` set to one of these lowercase codes (plain `F0002` for `invalid_params`): `consent_required`, `permission_denied`, `no_input_device`, `device_busy`, `unsupported_format`, `codec_error`, `bad_packet`, `unknown_channel`, `not_in_channel`, `unknown_speaker`, `invalid_params`, `unsupported` (optional stage or codec not compiled in), `server_muted`, `rate_limited`, `test_mode_only`. The book page lists every code and links the built `audio.voice*` page.

Agent verification recipe: start a session with `voice_chat.test_source { kind: "sine", freq_hz: 440, as_sender: 7 }`, `voice_chat.set_channel` onto a proximity channel, advance fixed steps, `expect` on `voice_chat.status.speakers[0].state == "speaking"` and on `voice_chat.stats` counters, render the mix through `audio.render_check` and check the tone level and left/right balance for the spatial case.

Guide page: `book/agents/voice-chat.md`. COVERAGE.md row text: `Voice chat | voice_chat.status, voice_chat.set_channel, voice_chat.mute, voice_chat.loopback, voice_chat.stats, voice_chat.test_source, voice_chat.simulate_network | Microphone capture, Opus coding, channels, moderation and the packet-loss simulator without a microphone | agents/voice-chat.md`.

## 6. Editor surface

Brief; the agent protocol is primary. A "Voice chat" panel (desktop editor, PIE): channel list with join/active toggles and radius gizmo in the viewport for proximity channels, a speaker list with level bars and mute/block buttons, a stats readout (jitter depth, loss, bitrate), and the test-source/loopback/network-simulator controls. The inspector edits `config/voice_chat.ron` through reflection. The panel never opens the microphone in the editor unless consent is set in the editor's own profile.

## 7. Determinism notes

- **Test mode** (`VoiceChatSettings::deterministic = true`, implied by `--deterministic`): the clock is the simulation tick counter (one frame = one 20 ms voice frame by definition, independent of render rate; the voice pump runs a fixed number of voice frames per tick from a rational accumulator), time scaling in the jitter buffer is off, and the processing chain is bypassed unless a test names a stage. No wall-clock time, no OS audio device.
- **Synthetic sources**: `sine(freq, level)` is computed from a sample index (phase = `2 pi f n / rate`, f64 accumulate), and `noise(seed)` uses a seeded PCG-class generator; both are bit-identical on every platform and the same input to the encoder.
- **Opus** output is deterministic for a given library build, version and complexity on a given platform, but not guaranteed across libopus builds or CPU feature paths (SIMD). Tests therefore assert *packet counts, sizes within a range, headers, sequence, and decoded signal properties* (RMS within a tolerance, dominant frequency within 1 bin of an FFT, correlation above a threshold), and use bit-exact checks only for header parse/write, the jitter buffer (which operates on opaque payload bytes), the resampler and the simulator.
- **Packet-loss simulator** (`VoiceChatSimulator`): a seeded model with independent loss, Gilbert-Elliott burst loss (`loss_pct` target, `burst_len` mean), reorder, duplication, base latency and jitter, all expressed in voice frames; given a seed and an input packet list it yields an identical output list. It sits between the encoder and the receive path in loopback and in tests, and also inside `regia_net`'s conditions simulator when that exists.
- Ordering: speakers are processed in ascending sender id; the mix sums in that order in f32, so the mixed output is reproducible for a fixed set of decoded frames. Speaker ranking ties break by lower sender id.

## 8. Tests and acceptance

No test below needs a microphone or sound device unless marked (device). None asserts wall-clock time; they use frame/tick counters. Golden images: none are produced by this feature (a lead TODO only if a HUD indicator golden is wanted).

1. Packet header round trip: every flag and field extreme (seq 65535, timestamp near 2^32); `parse` rejects short buffers, unknown version, payload > 400 bytes; fuzz-style test over random bytes never panics.
2. Serial-number comparison: sequences wrap correctly across 65535 -> 0 in the jitter buffer; duplicates dropped; reorder inside the window is slotted; a late packet is counted and dropped.
3. Encoder/decoder (`PcmCodec` always; the `OpusCodec` variant runs only with `--features voice-opus`): for each profile, 1 s of a 440 Hz sine round-trips; decoded RMS within 2 dB of input (PCM: bit-exact within 16-bit quantization) after the codec delay, FFT peak at 440 Hz +/- 1 bin; bytes per packet stay under the profile's cap.
4. Framing: feeding odd-sized chunks (e.g. 441, 17, 1000 samples at 44.1 kHz) from a fake capture yields exactly `floor(total / frame)` frames at the profile rate with no sample gain or loss beyond the resampler delay.
5. Loss concealment at 0, 5, 10, 20 and 40 % seeded loss (bursty and independent): zero panics; the fraction of frames output as concealed or recovered by FEC matches the lost fraction within 1 frame; after 5 consecutive missing frames the output is silent; output has no sample above 1.0 in magnitude and no NaN.
6. Jitter buffer: with simulated jitter up to 60 ms the depth settles within bounds 40..=200 ms and underruns are zero after the first talk-spurt; a sudden 150 ms stall causes at most one underrun and a recovery without growth beyond the max.
7. State machines: all transitions of § 2.2 and § 2.3 covered, including device removed during Transmitting, end_of_talk on mute, and a 1 s timeout of a silent speaker (tick counted).
8. VAD and push-to-talk: a synthetic burst (noise at -30 dBFS then silence) produces Transmitting for the burst plus hangover, then Idle; PTT release grace honored; adaptive threshold shifts with a raised noise floor.
9. Routing (server side, pure function over a membership table): team, party, global, proximity radius with slack, mutual block, mute hint, per-listener cap with deterministic ranking.
10. Proximity spatial: two speakers at 2 m left and 20 m right of the listener render with the louder, left-panned first, level difference matching the configured rolloff within 1 dB; moving the emitter out of the radius silences it; an entity-less speaker plays flat at -6 dB then crossfades.
11. Moderation: local mute drops packets before decode and sends a hint; block is mutual in the server routing; lists persist through a settings round trip, unknown keys survive; cap 512 evicts the oldest; the report ring holds only the last N seconds of encoded packets, is empty when `report_buffer_seconds = 0`, and is cleared at session end; assert no file under the user directory is created during any test.
12. Privacy: with `consent = false` no capture stream is requested (mock device records zero opens); the default settings value of every mode is PTT; loopback auto-disables after 3000 frames.
13. Agent protocol: each method round-trips against a test world with and without the plugin; `voice_chat.mute` with `persist` changes the settings; error codes in § 5 each reachable; `agent-coverage` and `check-layers` pass.
14. Bandwidth: a 10 s test-source session at each profile stays within 10 % of the § 2.11 figures (counted bytes / frames, not time).
15. (device) Capture opens the default input on Linux, Windows and an Android aarch64 device, delivers frames, and `dropped_frames` stays 0 during a 30 s loopback; the Android permission prompt appears once and a denial leaves `can_prompt` correct.
16. (device) End-to-end loopback with a real microphone: round-trip latency (capture to render) measured by the baseline's timestamp mechanism is reported and is below the § 10 target on each platform.

## 9. Platform notes

- **Linux**: capture through cpal's ALSA backend, which also reaches PipeWire and PulseAudio via their ALSA compatibility plugins; prefer the `default` device. Sandboxed packages need the audio-input portal permission, surfaced as `permission_denied`.
- **Windows**: WASAPI shared mode; a 10 ms default period gives 20 ms frames without extra buffering; honor the system microphone privacy setting (denial arrives as an open error, mapped to `permission_denied`).
- **Android aarch64**: Oboe/AAudio low-latency input (`PerformanceMode::LowLatency`, `Usage::VoiceCommunication` or equivalent so the system AEC/NS effects are available through the platform; if the platform's own effects are active the built-in AEC/NS stages are disabled automatically and reported in `effective_processing`). Runtime microphone permission as in § 2.5; the stream must be closed on pause and re-opened on resume together with the audio output (surface/audio lifecycle rule). Expected input latency 20-40 ms on AAudio, up to 100 ms on devices that fall back to OpenSL ES; use `frames_per_packet = 1` regardless, and mobile data users may select 2.
- **macOS** (testing only): CoreAudio through cpal; the microphone privacy prompt is requested by the OS on first open, and a missing usage-description entry makes the open fail, mapped to `permission_denied`.
- **Dependencies (decided, ADR-1585; each needs a `docs/03` row and a passing `cargo deny check`)**:
  - Opus: feature `voice-opus`, rules and fallback in § 3 (Decision: codec dependency). libopus is C: the one native dependency this feature may add, built with `cc` only, never on by default.
  - Recurrent-network noise suppression of the RNNoise class: feature `voice-denoise-rnn`, off by default; a Rust implementation with a BSD/MIT licence only, otherwise the feature stays unbuilt and `strong` behaves as `light`. Not part of the first delivery.
  - AEC: implement the public algorithm in Rust (partitioned-block frequency-domain adaptive filter); do not link a large third-party media stack.
  - Resampling: the baseline linear resampler stays for playout; capture conversion uses a small windowed-sinc polyphase resampler written in `regia_audio::voice_chat` (no new dependency).
  - Frames (FFT for tests and AEC): reuse an FFT already in the workspace lockfile if one exists, else a local radix-2 in `voice_chat::dsp`; no new dependency for this.

## 10. Performance budget

Min-spec desktop class (4 cores, 2 GHz-class x86) and mid Android aarch64 (big cores). Tracing spans in brackets.

| Item | Budget |
|---|---|
| Capture conversion + framing + processing (high-pass, light suppression) per 20 ms frame | <= 0.15 ms desktop, <= 0.4 ms Android [`voice_chat_capture`] |
| Encode one frame, complexity 5 | <= 0.25 ms desktop, <= 0.6 ms Android; complexity 0-3 on very low-end [`voice_chat_encode`] |
| Decode one frame (or FEC/PLC) | <= 0.1 ms desktop, <= 0.25 ms Android [`voice_chat_decode`] |
| Whole receive path per speaker per frame (parse, buffer, decode, spatialize, mix) | <= 0.2 ms; 8 speakers <= 1.6 ms per 20 ms, i.e. under 8 % of one core [`voice_chat_playout`] |
| Echo cancel (if on) per frame | <= 0.6 ms desktop, <= 1.5 ms Android [`voice_chat_aec`] |
| Strong suppression (if compiled in) per frame | <= 1.0 ms desktop, off by default on Android [`voice_chat_denoise`] |
| Server routing per packet (16 players) | <= 20 microseconds, no allocation after warm-up [`voice_chat_route`] |
| End-to-end loopback latency, headset, local | target <= 80 ms desktop, <= 140 ms Android (capture ~20-40, frame 20, jitter 60) |
| Memory per local session | <= 256 KB (encoder state, processing, rings) |
| Memory per remote speaker | <= 96 KB (decoder, jitter buffer for 200 ms at 48 kHz f32, resampler) |
| Total with 8 speakers | <= 1.1 MB |

No per-frame heap allocation on the capture and audio-callback paths; all buffers are sized at session creation. Spans are per-frame aggregates, not per sample.

## 11. Out of scope / follow-ups

- Third-party voice SDKs and platform voice services (a platform adapter may implement `VoiceChatTransport` or replace the codec later; not designed here).
- Speech-to-text, text-to-speech, voice changers, spatial HRTF for chat beyond what the spatial path already offers, recording or replay of voice, server-side mixing, multi-track transcript export.
- Peer-to-peer topology (format compatible, § 2.10) and a relay cluster.
- Text chat (separate feature; shares the channel ids).
- Automatic language detection or profanity filtering; the moderation hooks only forward reports.
- Console and iOS platforms.

## 12. Provenance and review decisions

| Area | Algorithm/behavior | Public source (paper/standard/doc URL) |
|---|---|---|
| Codec | Opus voice coding, FEC, PLC, frame sizes | IETF RFC 6716, Opus Codec Definition (https://www.rfc-editor.org/rfc/rfc6716); RFC 7587 (RTP payload) for timestamp conventions |
| Jitter estimate | Interarrival jitter and serial-number arithmetic | IETF RFC 3550 section 6.4.1 and Appendix A.1 (https://www.rfc-editor.org/rfc/rfc3550) |
| Voice activity | Energy-based speech detection with hysteresis | ITU-T Recommendation P.56 (active speech level); existing baseline `regia_audio::voice` |
| Noise suppression | Minimum-statistics noise tracking; recurrent-network suppression | R. Martin, "Noise Power Spectral Density Estimation Based on Optimal Smoothing and Minimum Statistics", IEEE TSAP 2001; J.-M. Valin, "A Hybrid DSP/Deep Learning Approach to Real-Time Full-Band Speech Enhancement" (RNNoise), MMSP 2018, https://arxiv.org/abs/1709.08243 |
| Echo cancellation | Partitioned-block frequency-domain adaptive filter | J.-S. Soo and K. K. Pang, "Multidelay block frequency domain adaptive filter", IEEE TASSP 1990; public WebRTC AEC design notes (https://webrtc.googlesource.com/src/+/refs/heads/main/modules/audio_processing/) -- design level only |
| Time scaling | Waveform-similarity overlap-add | W. Verhelst and M. Roelands, "An overlap-add technique based on waveform similarity (WSOLA)", ICASSP 1993 |
| Network model | Burst loss | E. N. Gilbert, "Capacity of a burst-noise channel", Bell System Technical Journal 1960 |

Decisions made in review (ADR-1585):

- Layering and the `regia_net` seam, codec dependency and `PcmCodec` fallback: § 3.
- Method prefix `voice_chat.*`; the built `audio.*` voice methods stay as they are and are the baseline for capture and loopback of the local microphone. `voice_chat.loopback` is the full-chain loopback of § 2.12 and is distinct from `audio.voice_loopback_start/stop` (which only replays pushed frames). The book page `voice-chat.md` links both.
- Error shape follows the neighboring audio methods: `F0013` for "plugin missing" (`unsupported`), and `F0002` with `data.reason` set to the lowercase codes of § 5 for everything else (`invalid_params` is plain `F0002`).
- Bandwidth figures in § 2.11 are estimates; the builder re-measures with the real encoder and updates the table (acceptance 14 allows 10 %). Re-measured in ADR-1911: `wire_payloads_match_the_bandwidth_table` pins the mean payload to the table within 10 %.
- Default push-to-talk everywhere and the 8-speaker cap stay as the spec's defaults; both are config values, not owner questions.
- Persisted settings go in `AudioUserSettings` (`regia_audio::settings`) as a `voice_chat` section; it is an existing user-settings shape, so adding a defaulted field with unknown-key round trip needs no version bump, but the builder checks `format_version` handling there.
- Formats to register: `config/voice_chat.ron` v1, documented in `docs/formats/voice-chat-ron.md`; the lead runs `cargo xtask check-formats --update` and `cargo xtask manual`.

As built (ADR-5642): a local account mute also queues a `mute_hint` for every slot bound to that account (once per change);
`voice_chat.mute` refuses `block: true` with a `{ sender }` target (blocks are per account); `voice_chat.stats` is a *play*
method because `reset` mutates counters; `voice_chat_denoise` is opened by the light suppressor (strong runs as light,
ADR-2608) and `voice_chat_decode` by both codecs. Not built at v0.5 (follow-ups): the `DecodedFrame` event and
`emit_decoded_frames`, the 10 Hz debounce of `SpeakerActivity` (edges are sent as they happen), `VoiceChatModeration::on_report`,
skipping buffer and decode for speakers outside the audible set (they are reset every 10 frames and, while the set is full, not mixed), the fade over
the frame after five concealed frames (output steps to silence), `speakers[].entity` in `voice_chat.status`, and the
"logs once" guard on the device fallback.

As built after v0.5 (ADR-7237): `SpeakerActivity` is debounced per sender to one event per 5 voice frames
(`ACTIVITY_DEBOUNCE_FRAMES`); an edge inside the window is sent at its end only if the state differs from the last
event, so events alternate and a stop/restart inside 100 ms sends nothing; `end_session` sends the pending "stopped"
events. `DecodedFrame { sender, channel, rate, samples: Arc<[f32]> }` is opt-in through
`VoiceChatSession::set_emit_decoded_frames` (a runtime switch, not a `config/voice_chat.ron` field, so the format is
unchanged); it carries each heard speaker's decoded frame before the output gain, and at most
`MAX_PENDING_DECODED_FRAMES` (256) wait for `take_events`. The frame after the fifth concealed frame is the decoder's
concealment faded linearly from 1 to 0 over the frame; silence follows. `voice_chat.status` lists `speakers[].entity`
(the bits of the lowest `VoiceChatEmitter` entity carrying the slot, or `null`). Acceptance 13 is narrowed for the
four stream codes: `bad_packet`, `codec_error` and `server_muted` are reached through `voice_chat.status.errors`
(`{ code, count, last_frame }` per code; `voice_chat.stats { reset }` clears it; the net client turns a `ServerMuted`
notice into `VoiceChatEvent::Error(ServerMuted)`), and `rate_limited`, a server-side drop the sender is never told
about, through `voice_chat.relay.rate_limited`. No agent method refuses with these four codes. Still not built:
`VoiceChatModeration::on_report`, skipping decode for speakers outside a full audible set, and the "logs once" guard.

### Owner decisions

None.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
