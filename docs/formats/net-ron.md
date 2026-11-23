# `config/net.ron` (NetSettings, format_version 4)

Project settings for the whole `regia_net` replication layer (specs 70-77). Each spec owns one section; this page
lists the sections that are built. Parsed by `regia_net::NetSettings::from_ron` (the net layer does not read the file itself: a host reads it and inserts the resulting `NetSettings` resource before `NetPlugin`, `NetSessionPlugin` and `LobbyPlugin`, which copy their sections when it is present), which fills every missing field with
its default and refuses a newer `format_version`.

```ron
NetSettings(
    format_version: 4,
    messages: (                       // spec 70 § 7
        default_rate: (count: 120, per_ticks: 60),
        peer_cap_per_tick: 512,
        peer_bytes_per_second: 262144,
        slowed_recover_ticks: 300,
        disconnect_after_slowed_ticks: 600,
        stall_forgive_ms: 250,
        unresolved_wait_ticks: 32,
    ),
    replication: (                    // spec 71 § 11, spec 72 § 8
        sleep_enabled: true,
        max_pending_refs: 4096,
        netvec_max: 4096,
        send_every_ticks: 2,
        default_profile: "engine://net/profiles/default.netprofile.ron",
        client_rate_bytes: 49152,
        server_rate_bytes: 0,         // 0 = no total cap
        mtu_bytes: 1200,
        state_channel: Unreliable,
        spawns_per_send: 24,
        grid_cell_m: 64.0,
        android_listen_max_clients: 8,
    ),
    interpolation: (                  // spec 74 § 10
        delay_send_intervals: 2.0,
        min_delay_ms: 33.0,
        max_delay_ms: 250.0,
        raise_rate: 0.10,
        lower_rate: 0.02,
        resync_error_ms: 500.0,
        recover_half_life_ms: 60.0,
        buffer_states: 32,
    ),
    prediction: (                     // spec 73 § 10
        input_redundancy: 16,
        history_ticks: 64,
        max_resim_ticks: 32,
        margin_target_ticks: 2,
        margin_max_ticks: 8,
        dilation_window_ticks: 8,
        max_dilation: 0.05,
        smooth_half_life_ms: 40.0,
        snap_distance_m: 2.0,
        spawn_grace_ticks: 8,
        max_input_packets_per_tick: 4,
        trust_client_within_m: None,
        verify_resim: false,          // tests only
    ),
    lag_compensation: (               // spec 75 § 9, spec 90 § 3.1
        enabled: true,
        max_rewind_ms: 250.0,
        projectile_rewind_ticks: 6,
        claim_tolerance_m: 0.1,
        claim_origin_tolerance_m: 0.5,
        claim_view_tick_slack: 2.0,
        max_shapes_per_entity: 32,
        debug_echo: false,
    ),
    // session: spec 77
)
```

| Field | Default | Meaning |
|---|---|---|
| `messages.default_rate` | `(count: 120, per_ticks: 60)` | Flood-guard token bucket of a message type with no `RATE_LIMIT` |
| `messages.peer_cap_per_tick` | 512 | Inbound messages per peer per tick before a breach |
| `messages.peer_bytes_per_second` | 262144 | Inbound bytes per peer per second before a breach |
| `messages.slowed_recover_ticks` | 300 | Quiet ticks after which a slowed peer is clear |
| `messages.disconnect_after_slowed_ticks` | 600 | Breaching ticks while slowed before a `flood` disconnect |
| `messages.stall_forgive_ms` | 250 | A frame whose simulation step reached this many milliseconds forgives the next frame's burst; `0` disables it. Under `TimeUpdateStrategy::Automatic` the frame delta is clamped to 250 ms and a frame at the clamp reaches any threshold of 250 ms or more |
| `messages.unresolved_wait_ticks` | 32 | Ticks a message waits for its target to spawn |
| `replication.sleep_enabled` | true | `false` disables sleep everywhere (a diagnosis switch) |
| `replication.max_pending_refs` | 4096 | Pending entity references a client keeps (oldest dropped) |
| `replication.netvec_max` | 4096 | Reserved. A `NetVec` holds 4096 elements (`NETVEC_MAX`); both ends share that wire bound and this setting changes nothing |
| `replication.send_every_ticks` | 2 | Ticks between send ticks (spec 72 § 3): frames go on ticks that are a multiple of it; priorities and budgets count send ticks (ADR-1950) |
| `replication.state_channel` | `Unreliable` | `Unreliable`: deltas against acknowledged baselines on the unreliable channel for clients that speak protocol 2, the reliable tick stream for older ones. `Reliable`: the tick stream for every connection (spec 72 § 4, ADR-1950) |
| `replication.default_profile` | `engine://net/profiles/default.netprofile.ron` | The `.netprofile.ron` of entities that name none (spec 72) |
| `replication.client_rate_bytes` | 49152 | Per-connection send rate in bytes per second (spec 72 § 7) |
| `replication.server_rate_bytes` | 0 | Total outbound cap (0 = none); every connection's budget scales down by the same factor |
| `replication.mtu_bytes` | 1200 | Largest state packet with transport headers (spec 72 § 7); tick frames are fragmented by the transport |
| `replication.spawns_per_send` | 24 | Spawn records per connection per send tick |
| `replication.grid_cell_m` | 64.0 | Interest grid cell size in metres |
| `replication.android_listen_max_clients` | 8 | Clients an Android listen server accepts (spec 72 § 14; enforced, `0` reads as 8; dedicated servers and other platforms ignore it) |
| `interpolation.delay_send_intervals` | 2.0 | Target delay in send intervals, before jitter (spec 74 § 4) |
| `interpolation.min_delay_ms` / `max_delay_ms` | 33.0 / 250.0 | Clamp of the target delay |
| `interpolation.raise_rate` / `lower_rate` | 0.10 / 0.02 | Largest clock-speed change while growing / shrinking the delay |
| `interpolation.resync_error_ms` | 500.0 | A delay error (or frame gap) above this resyncs the clock (`net.interp_resync`) |
| `interpolation.recover_half_life_ms` | 60.0 | Half-life of the offset that hides a correction after extrapolating |
| `interpolation.buffer_states` | 32 | States buffered per proxy |
| `prediction.input_redundancy` | 16 | Unacknowledged commands resent with every input packet, newest first (spec 73 § 4) |
| `prediction.history_ticks` | 64 | Ticks of prediction history per predicted entity |
| `prediction.max_resim_ticks` | 32 | Most ticks re-run in one frame; a larger rollback snaps (`resets`) |
| `prediction.margin_target_ticks` | 2 | Target input buffer margin on the server, before jitter (one tick more per 4 ms) |
| `prediction.margin_max_ticks` | 8 | Upper clamp of the margin target |
| `prediction.dilation_window_ticks` | 8 | A margin error up to this is corrected by dilation; a larger one resyncs (`net.lead_resync`) |
| `prediction.max_dilation` | 0.05 | Largest tick-rate change while dilating (one tick inserted or skipped per `1 / max_dilation` steps at most) |
| `prediction.smooth_half_life_ms` | 40.0 | Half-life of the visual correction offset |
| `prediction.snap_distance_m` | 2.0 | A correction offset larger than this (or over 90°) snaps |
| `prediction.spawn_grace_ticks` | 8 | Acknowledged input ticks a predicted spawn waits for its server spawn |
| `prediction.max_input_packets_per_tick` | 4 | Input packets per player per server tick; more are dropped (`flood`) |
| `prediction.trust_client_within_m` | `None` | Opt-in: adopt the client's reported position within this many metres per second (`None` = off). The budget refills with the tick; a client cannot gain the whole distance every tick (spec 73 § 6.8) |
| `prediction.verify_resim` | false | Tests only: run every predicted tick twice and compare (`net.predict_nondeterministic`) |
| `lag_compensation.enabled` | true | `false` judges every shot at the present (a runtime switch; the history keeps recording) |
| `lag_compensation.max_rewind_ms` | 250.0 | How far back a shot is judged; the history keeps `ceil(max_rewind_ms / step_ms) + 2` ticks |
| `lag_compensation.projectile_rewind_ticks` | 6 | Ticks of flight a projectile is judged at the shooter's view (0 = never) |
| `lag_compensation.claim_tolerance_m` | 0.1 | How much a claimed shape grows when a hit claim is checked |
| `lag_compensation.claim_origin_tolerance_m` | 0.5 | Largest gap between a claim's ray origin and the server's muzzle point |
| `lag_compensation.claim_view_tick_slack` | 2.0 | Largest gap in ticks between a claim's view tick and the shooter's reported one |
| `lag_compensation.max_shapes_per_entity` | 32 | Hit shapes recorded per entity; more are ignored (`net.hitshape_invalid`) |
| `lag_compensation.debug_echo` | false | Dev builds, with `LagCompEchoPlugin`: send a shooter the rewound shapes of its hits |

**Versioning.** Every field added by specs 70-77 has a serde default, so older files load, and the
sections the runtime does not read yet (see the notes per key) have no effect. Registered in `cargo xtask check-formats` and `regia upgrade-project` since ADR-2316 (spec 12 § 4a): any shape change, an added field with a serde default included, bumps `format_version` and ships a migrator (a version rewrite when older files still read).

The wire protocol is versioned separately: `regia_net::NET_PROTOCOL_VERSION` (2 since spec 73's input frames, ADR-1793) plus the protocol hash of the
registered message types and replicated components. Neither is a content format; a mismatch refuses the join.

## `session` (spec 77 § 12, ADR-1663)

```ron
session: (
    timeout_ms: 20000,
    interrupt_ms: 1500,
    start_grace_ms: 30000,
    connect_timeout_s: 10.0,
    handshake_timeout_s: 10.0,
    loading_timeout_s: 120.0,
    joining_timeout_s: 30.0,
    auth_timeout_s: 10.0,
    max_handshaking: 32,
    reserved_slots: 0,
    reconnect_grace_s: 60.0,
    stream_share: 0.25,
    stream_idle_timeout_s: 30.0,
    max_stream_buffer_bytes: 536870912,
    dev_allow_content_mismatch: false,
    encryption: Auto,
    rate_control: (                   // spec 120 § 4.2, format 3 (ADR-8301, ADR-7724)
        enabled: true,
        floor_up_bytes_per_second: 16384,
        floor_down_bytes_per_second: 32768,
        start_bytes_per_second: 262144,
        circuit_breaker_ms: 10000,
        listen_shared_uplink: true,
        uplink_bytes_per_second: 0,
        first_flight: true,           // format 4 (ADR-10156, ADR-10200)
    ),
),
```

| Field | Default | Meaning |
|---|---|---|
| `session.timeout_ms` | 20000 | Authentic silence before removal; forwarded to transport |
| `session.interrupt_ms` | 1500 | Authentic silence before interruption |
| `session.start_grace_ms` | 30000 | Admission floor until game traffic starts |
| `session.timeout_s` | 20.0 | Legacy read only when the file has no `timeout_ms`; Rust override while `timeout_ms` keeps its default; omitted on write |
| `session.connect_timeout_s` | 10.0 | Transport connect timeout (`Connecting`) |
| `session.handshake_timeout_s` | 10.0 | Server: a link without a valid `Hello`. Client: `Handshaking` waits this plus `auth_timeout_s`. No start grace before admission (ADR-7950) |
| `session.loading_timeout_s` | 120.0 | `Loading` longer than this ends with `level_load_failed` |
| `session.joining_timeout_s` | 30.0 | `Joining` longer than this ends with `timeout` |
| `session.auth_timeout_s` | 10.0 | How long a pending authenticator may take (`auth_failed`) |
| `session.max_handshaking` | 32 | Links in `Handshaking` at once; further transports are closed |
| `session.reserved_slots` | 0 | Player slots only for clients the authenticator marks `reserved` |
| `session.reconnect_grace_s` | 60.0 | Grace after an unexpected drop from `InGame` (0 disables) |
| `session.stream_share` | 0.25 | Share of the per-tick connection budget streams may use |
| `session.stream_idle_timeout_s` | 30.0 | Incoming streams fail with `stream_timeout` when together they moved less than 256 bytes per second over this long (0 = never) |
| `session.max_stream_buffer_bytes` | 536870912 | Server: incoming stream bytes held across all peers; a chunk past it fails the sender's streams with `stream_buffer_full` (0 = no limit) |
| `session.dev_allow_content_mismatch` | false | Dev builds: a content mismatch is a warning, not a refusal |
| `session.encryption` | `Auto` | Transport encryption: `Off`, `Auto` (every non-loopback peer) or `Required` (ADR-1901) |
| `session.rate_control.enabled` | true | The transport's rate controller and pacer on both ends (spec 120 § 4.2); `false` sends every due packet at once |
| `session.rate_control.floor_up_bytes_per_second` | 16384 | The client's essential floor: inputs, acknowledgments and keepalives may always use it |
| `session.rate_control.floor_down_bytes_per_second` | 32768 | The server's essential floor toward each client (tick sync, input feedback, acknowledgments) |
| `session.rate_control.start_bytes_per_second` | 262144 | The start rate before the startup doubles it (at most 256 KiB/s) |
| `session.rate_control.circuit_breaker_ms` | 10000 | Less than the floor delivered at over 50 % loss for this long marks the peer interrupted |
| `session.rate_control.listen_shared_uplink` | true | A listen server shares its host's uplink among its clients (max-min fair shares) |
| `session.rate_control.uplink_bytes_per_second` | 0 | A fixed shared uplink for any server, bytes per second (0 = measured, listen servers only) |
| `session.rate_control.first_flight` | false | Format 4 (ADR-10156, ADR-10200, ADR-10231), opt-in: `true` holds a sender to the essential floor's rate and a first burst of eight packets until its path has answered once, and the startup begins at what the first round delivered; `false` (the default) sends the 64-packet initial window at the start rate before the path has answered |

Every `session` field has a serde default. Its shape is fingerprinted by `check-formats`.

## `lobby` (spec 89 § 6, ADR-1877)

```ron
lobby: (
    max_members: 8,
    visibility: Public,
    min_players: 2,
    match_content: true,
    auto_connect: true,
    joinable_in_game: false,
    rounds: [Close, Default, Far, Worldwide],
    results_per_round: 50,
    step_timeout: 600,
    create_if_none: true,
    steam_channel: 0,
),
```

| Field | Default | Meaning |
|---|---|---|
| `lobby.max_members` | 8 | Member limit of lobbies the engine creates (1..=250) |
| `lobby.visibility` | `Public` | `Public`, `FriendsOnly`, `Private` or `Invisible` |
| `lobby.min_players` | 2 | Members (owner included) needed before the owner may launch |
| `lobby.match_content` | true | Lists and matchmaking also require the same content hash, not only the protocol hash |
| `lobby.auto_connect` | true | Members connect their `NetSession` when the owner launches |
| `lobby.joinable_in_game` | false | Keep the lobby joinable after launch |
| `lobby.rounds` | `[Close, Default, Far, Worldwide]` | Matchmaking rounds, widening the distance filter |
| `lobby.results_per_round` | 50 | Results per list round (1..=50) |
| `lobby.step_timeout` | 600 | Frames to wait for a platform answer before the request counts as failed |
| `lobby.create_if_none` | true | Matchmaking creates a lobby when every round is empty |
| `lobby.steam_channel` | 0 | Steam sockets channel the session uses. The channel is a 16-bit value (`0..=65535`); a setting above 65535 is used as channel 0 |

Every `lobby` field has a serde default; files without it load with these defaults. Its shape
is fingerprinted by `check-formats`, with versioned migrations for later shape changes.


## Entropy settings and migration (ADR-4854)

Version 2 adds `replication.entropy`: `Off` (default) or `Range`. Version 1 files still load
with entropy off. `regia upgrade-project` uses `regia_net::settings::migrate_net_settings` to
rewrite older settings as the current version, retaining all values and filling defaults. This rewrite formats RON
and does not retain comments; already-current files remain byte-identical.

`NetSettings::to_ron` also stamps the current version when saving legacy settings.


## Version 3: silence policy (ADR-7950)

The `session` section adds `interrupt_ms: 1500`, `timeout_ms: 20000`, and
`start_grace_ms: 30000`. `timeout_s` is read for legacy compatibility but is not written.
`migrate_net_settings` and `regia upgrade-project --write` convert v1/v2 files to v3,
preserving an explicit seconds timeout as milliseconds. Newer versions are refused.
A `timeout_ms` the file names wins over legacy seconds at any value, its default included;
`timeout_s` is read only from files without `timeout_ms`. In Rust, `timeout_s` remains
compatible for existing callers while `timeout_ms` keeps its default. Other deadlines keep their seconds-based names.
Out-of-range values are clamped where the policy is resolved (`TimeoutPolicy::sanitized`):
`timeout_ms` at least 1000, `interrupt_ms` from 100 to below `timeout_ms`, `start_grace_ms` at
most 600000.

### Version 3, continued: rate control (ADR-8301)

The `session` section also has `rate_control` (`regia_net::cc::RateSettings`, spec 120 § 4.2). It was folded into the
unreleased version 3 as an additive section (ADR-7724: a release moves a format at most one step, v0.5.0 shipped v2);
every field has a default, so a v1, v2 or v3 file without it loads with the controller on, and `migrate_net_settings`
and `regia upgrade-project --write` stamp version 3 and keep every other setting. Version 4 and newer are refused. `NetSession` applies the section when it hosts
(`floor_down_bytes_per_second`, the shared uplink for a listen server) and when it joins (`floor_up_bytes_per_second`).

## Version 4: the first flight (ADR-10200, ADR-10231)

`session.rate_control` gains `first_flight` (`regia_net::cc::RateSettings::first_flight`, default `false`: the first
flight is opt-in in v0.7, the lead's revised ruling recorded in ADR-10231). Switched on, a connection's sender is held to the essential
floor's rate (32 KiB/s toward a client, 16 KiB/s toward the server) and a first burst of 300 ms of it (eight
1,200-byte packets) until a feedback round has acknowledged one of its packets; a first round the sender filled and
the path fell behind in starts the startup at what the path delivered, any other keeps the start rate. A v1, v2 or
v3 file has no such key and loads with it off, as it ran before; `migrate_net_settings` and `regia upgrade-project --write` stamp
version 4 and keep every other setting. Version 5 and newer are refused.
