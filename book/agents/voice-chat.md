# Voice chat

Drive and verify voice chat (specs/59) without a microphone or a sound device: packets, the codec,
channels, moderation and the packet-loss simulator. The older capture and playback baseline stays
under `audio.*` ([Voice capture and loopback](voice.md)); `voice_chat.loopback` below is the
full-chain loopback (encoder, packets, simulated network, jitter buffer, playout) and is different
from `audio.voice_loopback_start`, which only replays frames the game pushes.

Every method but `voice_chat.relay` needs `regia_audio::voice_chat::VoiceChatPlugin`; without it they
return `UNSUPPORTED` (`F0013`). `voice_chat.relay` needs `regia_net::voice::NetVoicePlugin` on the
server. Every other refusal is `F0002` with `data.reason` set to one of the codes below (a plain
`F0002` for `invalid_params`).

Consent is **reported, never set**, by the agent protocol: `consent` becomes true only when a
person changes it in the user settings. `test_source` and a synthetic `loopback` never open the
device, so they work without consent. A microphone `loopback` or `mic` without consent fails with
`consent_required`, and `voice_chat.settings` refuses a `consent` parameter.

## Methods

| Method | Class | Params | Result |
|---|---|---|---|
| `voice_chat.status` | read | none | `{ state, error, consent, can_prompt, microphone, mode, quality, codec, bitrate_bps, device, rate, channels: [{ id, name, kind, joined, active }], speakers: [{ sender, state, entity, muted, blocked, level }], effective_processing, loopback, test_mode, errors: [{ code, count, last_frame }] }` |
| `voice_chat.set_channel` | play | `{ channel, join?, active?, radius? }` | `{ id, name, kind, radius, spatial, joined, active }` |
| `voice_chat.mute` | play | `{ target: "self" \| "deafen" \| { sender } \| { account }, muted, block?, persist?, volume_pct?, note? }` | `{ applied, persisted }` |
| `voice_chat.loopback` | play | `{ enabled, source?: "mic" \| "sine" \| "noise" }` | `{ enabled, source }` |
| `voice_chat.stats` | play | `{ reset? }` | `{ totals, speakers: [...], output, reset }` |
| `voice_chat.test_source` | play | `{ kind: "sine" \| "noise" \| "silence", freq_hz?, level_db?, seed?, as_sender?, enabled? }` | `{ enabled }` |
| `voice_chat.simulate_network` | play | `{ loss_pct, latency_ms, seed, burst_len?, reorder_pct?, dup_pct?, jitter_ms? }` | `{ applied }` |
| `voice_chat.mic` | play | `{ enabled, fake?, rate?, channels?, freq_hz?, level_db? }` | `{ enabled, microphone, state, error, can_prompt, device }` |
| `voice_chat.settings` | play | `{ mode?, quality?, input_gain_db?, output_gain_db?, device? }` | `{ consent, mode, quality, device, input_gain_db, output_gain_db, deafen, muted_accounts, blocked_accounts }` |
| `voice_chat.talk` | play | `{ pressed }` | `{ pressed }` |
| `voice_chat.relay` | read | none | `{ peers: [{ peer, slot }], packets_in, bytes_in, forwarded, bytes_out, bad_packets, rate_limited, not_in_channel, server_muted, listener_cap_drops, joins_refused }` |
| `voice_chat.processing` | play | `{ noise_suppression?: "off" \| "light" \| "strong", echo_cancel?, agc?, echo_reference?: "playout" \| "external", reset_stats? }` | `{ requested, effective, strong_available, echo_reference, reference_attached, rate, nonfinite_samples, frame_mismatches, stages: [{ name, frames, bypassed, mean_us, max_us }], aec, noise_suppression, agc, reset }` |

- `state` is `off`, `idle`, `transmitting`, `hangover`, `muted` or `error` (then `error` holds the
  code). A speaker's `state` is `silent`, `buffering`, `speaking` or `draining`.
- `active: true` in `set_channel` makes the channel the transmit channel; it must be joined.
- `mute` with `{ account }` always writes the persisted user settings (capped at 512 entries,
  oldest evicted), so `persist: false` answers `unsupported`. `{ sender }` is session-scoped;
  `volume_pct` (0..=200) applies to a sender that has been heard (`unknown_speaker` otherwise).
  A block is per account: `block: true` with `{ sender }` is refused as invalid params (plain
  `F0002`) and changes nothing. Muting an account also sends a `mute_hint` to the server for
  every slot bound to that account, so the server stops forwarding it (ADR-5642).
- `test_source` with `as_sender` creates a fake remote speaker that transmits on the active
  channel; without it the source replaces the microphone. `enabled: false` removes it.
  `as_sender: 65535` is reserved for loopback. Times in `simulate_network` are milliseconds,
  rounded to 20 ms voice frames.
- `loopback` turns itself off after 3000 voice frames (60 s).
- `microphone` is `off`, `awaiting_permission`, `open`, `lost` (retried every 2 s) or `suspended`
  (the app is paused). `codec` is `pcm` or `opus`; `bitrate_bps` moves between 60 % and 100 % of the
  profile's target with the uplink loss the server reports.
- `mic` with `fake: true` installs a scripted microphone with a sine source, so capture, the
  resampler, framing and the encoder run with no device; it still needs consent, as the real path
  does. On Android, enabling a real microphone shows the `RECORD_AUDIO` prompt and returns at once;
  poll `status.microphone`. After "don't ask again" `can_prompt` is false and only the system
  settings can change it.
- `settings` with `quality` waits for the end of a talk spurt; `device: ""` selects the system
  default.
- `frames_per_packet: 2` in `config/voice_chat.ron` sends 40 ms per packet. `redundancy: true` adds a
  spare copy of the previous frame while the reported loss is above 10 %; `spare_filled` counts the
  holes it filled. The two settings are mutually exclusive — a two-frame packet has no slot for a
  spare, so `validate` rejects `frames_per_packet: 2` combined with `redundancy: true` and the
  project config fails to load until one is turned off. An open microphone in silence sends one
  comfort packet per 400 ms.
- `shortened` and `lengthened` count pitch-period corrections of the jitter buffer's depth, and
  `drift_ppm` is the long-run clock difference to the sender. All three stay 0 in the deterministic
  mode.
- `stats.output` is the playback stage: frames sent to the `Voice` bus, frames dropped by clock drift,
  and each speaker's `{ sender, placed, gains: [left, right] }`.

## Microphone processing

`voice_chat.processing` turns the optional stages on and off and reads what they did. The chain runs
on every captured 20 ms frame in this order: `high_pass` (90 Hz, always on outside the deterministic
test mode), `echo_cancel`, `noise_suppression_light`, `agc`, then voice activity and the encoder.
Every optional stage is off by default; `effective` (and `status.effective_processing`) lists what
really runs.

- **Echo cancellation** removes what the speakers play from what the microphone hears: a
  frequency-domain adaptive filter covering 128 ms of room echo, guarded against double talk (the
  local player talking over the far end). It needs a playback reference: with
  `echo_reference: "playout"` (the default) that is the session's own voice playout, available once
  an audio output is attached; with `"external"` the game pushes the final mix with
  `VoiceChatSession::push_echo_reference`, one 20 ms frame per tick. Without a reference the stage
  stays off and logs a warning once. `aec.erle_db` is the measured echo reduction (about 35 dB once
  converged on the test fixtures), `aec.double_talk_blocks` counts 10 ms blocks where the detector
  slowed adaptation, `aec.transfers` and `aec.resets` count the two-filter guard's copies.
- **Noise suppression** `light` is a spectral gain with minimum-statistics noise tracking: steady
  noise drops by up to 12 dB, speech keeps its level within about 1 dB, and the stage adds 10 ms of
  delay. `strong` needs a trained network that is not built (`strong_available: false`); it runs as
  `light` and is reported as `noise_suppression_light`.
- **Automatic gain** levels speech toward -20 dBFS (at most +18 dB of boost, -18 dB of cut; down 2 dB
  per frame, up 0.15 dB per frame), with a noise gate that follows the noise floor so silence and
  steady room noise are not boosted, and a limiter at -1 dBFS. A room that suddenly gets much louder
  (a fan switching on) reads as speech for about 4 s, until the floor catches up. The gain holds
  while the far end talks alone, so it does not boost left-over echo.
- `stages[].mean_us` and `max_us` are measured time per frame, for a cost readout; tests check the
  deterministic counters (`aec.ffts`, `aec.complex_macs`, `noise_suppression.ffts`) instead.
  `reset_stats: true` zeroes the counters after reading them.
- At most two frames of reference are queued (older samples are dropped, `aec.reference_dropped`),
  so the reference never falls behind the microphone. Non-finite microphone samples become silence
  before the chain (`nonfinite_samples`); a frame of the wrong length passes through unprocessed
  (`frame_mismatches`).

```json
{ "method": "voice_chat.processing", "params": { "echo_cancel": true, "noise_suppression": "light", "agc": true } }
{ "method": "voice_chat.processing", "params": { "reset_stats": true } }
```

## Limits on what a server sends

Sender slots come from the server, and a listen server is another player's machine. A client decodes and mixes at
most 256 remote speakers (`MAX_REMOTE_SPEAKERS`); a packet from a new sender past that is dropped and counted in
`voice_chat.stats` `totals.overflow_drops`. At most 1,024 sender slots are bound to an account (`MAX_BOUND_SENDERS`);
a new binding past that is ignored. On the server, a mute hint names a connected slot or is ignored (ADR-5870).

## Error codes

`F0002` `data.reason`: `bad_packet`, `codec_error`, `consent_required`, `device_busy`,
`invalid_params`, `no_input_device`, `not_in_channel`, `permission_denied`, `rate_limited`,
`server_muted`, `test_mode_only`, `unknown_channel`, `unknown_speaker`, `unsupported`,
`unsupported_format`.

The agent methods raise `consent_required`, `device_busy`, `invalid_params`, `no_input_device`,
`not_in_channel`, `permission_denied`, `test_mode_only` (a shipping game turned the developer test
mode off), `unknown_channel`, `unknown_speaker`, `unsupported` and `unsupported_format`. The rest happen to a
running session, not to a call, so no method refuses with them; they are reported as data (ADR-7237):

- `bad_packet` (a packet that does not parse), `codec_error` (the codec refused a payload; the frame
  was concealed) and `server_muted` (the server forced a mute on the local sender) appear in
  `voice_chat.status.errors`: one record per code, `{ code, count, last_frame }`, sorted by code.
  Device codes the microphone path meets (`no_input_device`, `device_busy`, ...) are recorded there
  too. `voice_chat.stats { "reset": true }` clears the record. A game sees the same as
  `VoiceChatEvent::Error`.
- `rate_limited` happens on the server: the relay drops the flood and counts it in
  `voice_chat.relay.rate_limited` (the relay never tells the sender). `voice_chat.relay` also counts
  `bad_packets` and `server_muted` drops.

`speakers[].entity` is the entity bits of the `VoiceChatEmitter` that carries the speaker's slot (the
lowest one when several do), or `null` when the speaker has none.

## Verify without a microphone

```json
{ "method": "voice_chat.set_channel", "params": { "channel": 2, "join": true, "active": true } }
{ "method": "voice_chat.test_source", "params": { "kind": "sine", "freq_hz": 440, "as_sender": 7 } }
```

Advance fixed steps, then `expect` that `voice_chat.status.speakers[0].state == "speaking"` and
read `voice_chat.stats`. To see the concealment work, call `voice_chat.simulate_network` with
`{ "loss_pct": 20, "burst_len": 3, "latency_ms": 40, "seed": 3 }` and watch `concealed` and
`recovered` grow while `underruns` stays 0.

## Use it in a game

```rust,ignore
app.add_plugins((VoiceChatPlugin::default(), regia::media::voice_opus::VoiceOpusPlugin));
app.add_plugins((NetPlugin { mode }, regia::net::voice::NetVoicePlugin::default()));
```

Features: `voice-opus` for the codec and `voice-net` for the transport. Opus packets from a peer must be
one mono 20 ms SILK or hybrid frame, as this encoder makes them; anything else counts as a decode error
and is concealed. Without `voice-opus` a session
speaks PCM, which is for tests: a `high` frame is too big for one datagram. On the server, call
`VoiceRelay::set_account` with an opaque per-account tag, and let moderation decide teams with
`VoiceRelay::set_moderation`.

The microphone takes three explicit steps: the player sets `consent` in the settings screen, the game
installs `CpalInputOpener` with `set_microphone`, and an enable button calls `enable_microphone`.
Compiling the feature in or opening a settings screen never shows a prompt. On Android the project's
`Regia.toml` must list the permission:

```toml
[platforms.android]
permissions = ["android.permission.RECORD_AUDIO"]
```

Push-to-talk is the `voice_chat_talk` input action in the player's input contexts, so rebinding, gamepads
and touch buttons work like any action. To place a speaker, put `VoiceChatEmitter { sender, .. }` on
their character: on a spatial channel the voice comes from there (2 m minimum distance, the channel's
radius as the maximum, 6 dB per doubling, occlusion on). A speaker with no such entity plays flat at
-6 dB until one appears, then crossfades over 100 ms. `VoiceChatSpeaking` sits on the entity while they
talk.

`VoiceChatEvent::SpeakerActivity` (from `VoiceChatSession::take_events`) is debounced to 10 Hz: one
sender's events are at least five voice frames (100 ms) apart, a stop and restart inside that window
sends nothing, and the events always alternate, starting with "started" (`ACTIVITY_DEBOUNCE_FRAMES`).
For captioning or a waveform, `VoiceChatSession::set_emit_decoded_frames(true)` adds a
`VoiceChatEvent::DecodedFrame { sender, channel, rate, samples }` per heard speaker per 20 ms: mono
decoded samples before the output gain. Drain the events each frame: at most
`MAX_PENDING_DECODED_FRAMES` (256) frames wait. After five concealed frames (100 ms) the next frame
fades the concealment out to zero, then silence follows until real audio returns (20 ms fade in).

## What is built, what is not

Built: the packet format (version 1) with two-frame packets and spare frames, `PcmCodec`, Opus with
in-band FEC and concealment (`regia_media`, feature `voice-opus`), the adaptive jitter buffer with
pitch-period time scaling and a drift estimate, the seeded loss simulator, the
cpal microphone with permission, device-loss retry and pause handling, a scripted microphone for tests,
capture conversion (downmix, windowed-sinc resampler, 20 ms framing, high-pass), the transmit and
speaker state machines, voice activity, push-to-talk and toggle, the server relay and client transport
(`regia_net`, feature `voice`), adaptive bit rate from reported uplink loss, the quality cap, channel
routing, mute, block, deafen, forced server mute, the report ring, playback on the `Voice` bus, and the
spatial chat emitter.

Also built (ADR-2608): echo cancellation, `light` noise suppression and automatic gain, and the
unknown-key round trip of the `voice_chat` user settings (keys a newer build wrote are kept on save).

Not built yet: `strong` noise suppression (runs as `light`), HRTF for chat voices (spec 59 § 11 leaves
it out; chat emitters use the stereo spatial path), echo-path delay estimation (the reference must be
fed one frame per tick) and the editor panel. `status.effective_processing` lists what is really in
effect. Hardware checks (a real microphone on each platform, the Android prompt on a device)
are listed in the report of ADR-1911.
