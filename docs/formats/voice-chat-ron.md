# `config/voice_chat.ron` — voice chat project defaults (format v1)

Loader: `regia_audio::voice_chat::VoiceChatSettings::from_ron`. Frozen like every content format
(`docs/specs/12-assets.md` § 4a): a file whose `format_version` is not 1 is refused with
`invalid_params`. A version 2 needs a migrator and `regia upgrade-project` support. The wire
`version` of voice packets (version 1) is separate and negotiated by taking the minimum both sides
announce at join. The file is copied into the pak as is.

```ron
(
    format_version: 1,
    quality: Standard,            // Narrow (16 kHz) | Standard (24 kHz) | High (48 kHz)
    mode: PushToTalk,             // Open | Vad | PushToTalk | Toggle
    frames_per_packet: 1,         // 1 or 2 (2: two frames per packet, `multi` flag, ADR-1911)
    max_mixed_speakers: 8,        // 1..=16
    jitter: (min_ms: 40, max_ms: 200, start_ms: 60),
    processing: (noise_suppression: Off, echo_cancel: false, agc: false),
    report_buffer_seconds: 0,     // 0..=30; 0 never keeps packets
    channels: [
        (id: 1, name: "team", kind: Team, priority: 3),
        (id: 2, name: "nearby", kind: Proximity, radius: 25.0, spatial: true, priority: 2),
        (id: 3, name: "party", kind: Party, priority: 1),
        (id: 4, name: "all", kind: Global, priority: 0),
    ],
)
```

Every field has a default, so a minimal file parses. Also accepted: `vad` (`on_db`, `off_db`,
`adaptive`), `complexity` (0..=10), `redundancy`, `deterministic`. Channel ids are stable for a
project's lifetime and part of the wire format. The per-user subset (`consent`, `device`, `mode`,
gains, `muted_accounts`, `blocked_accounts`, `deafen`) is `VoiceChatUserSettings`; consent is
`false` in a fresh profile.
