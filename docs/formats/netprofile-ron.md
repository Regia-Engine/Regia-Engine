# `.netprofile.ron` (NetProfile, format_version 2)

Per-entity relevance, priority, send-rate and quantization settings of `regia_net` (spec 72 § 8), plus the proxy
interpolation fields owned by spec 74 § 10 (ADR-1662). Read with `regia_net::NetProfile::from_ron` (or
`NetProfiles::load_ron`), which fills every missing field with its default, refuses a newer `format_version`, and
validates the ranges below. JSON Schema: `docs/formats/netprofile.schema.json`.

```ron
NetProfile(
    format_version: 2,
    priority: 1.0,
    relevance: Spatial(radius: 150.0, hysteresis: 0.15),   // or World
    sent_to_all: false,
    owner_only: false,
    group_required: false,
    follow_parent: false,
    custom_filter: None,                // Some("line_of_sight")
    min_interval_ticks: 1,
    max_interval_ticks: None,
    linger_ticks: 30,
    owner_boost: 4.0,
    view_falloff: true,
    position_step_m: Some(0.001),
    // spec 74:
    interpolation: Linear,              // Hermite, None
    snap_distance_m: 5.0,
    max_extrapolate_ms: 100.0,
    appear_immediately: false,
    // spec 120 § 8.3 (v2):
    rate_tiers: None,                   // Some((near_m: 150.0, mid_m: 600.0, mid_every: 3, far_hz: 2.0))
    // spec 120 § 8.2 (v2):
    extrapolation_gate_m: None,         // Some(0.02)
)
```

| Field | Default | Rule | Meaning |
|---|---|---|---|
| `priority` | 1.0 | > 0 | Priority weight; only ratios between profiles matter |
| `relevance` | `Spatial(radius: 150.0, hysteresis: 0.15)` | radius > 0, 0 ≤ hysteresis ≤ 1 | Relevant near a viewpoint (stays until `radius × (1 + hysteresis)`), or `World` |
| `sent_to_all` | false | | Relevant to every connection (a grant) |
| `owner_only` | false | | Relevant to the owner only (beats `sent_to_all`; both set logs `net.profile_conflict`) |
| `group_required` | false | | Needs one of the entity's `NetGroups` allowed by the connection |
| `follow_parent` | false | | Relevant exactly when its parent or `NetOwnerLink` target is |
| `custom_filter` | None | registered name | A filter from `app.add_net_relevance_filter` |
| `min_interval_ticks` | 1 | ≥ 1 | At most one send per this many ticks |
| `max_interval_ticks` | None | ≥ `min_interval_ticks` | An entity with changes is sent at least this often |
| `linger_ticks` | 30 | | Ticks a copy stays after losing relevance |
| `owner_boost` | 4.0 | ≥ 1 | Priority factor for entities the connection owns |
| `view_falloff` | true | | Half priority outside every viewpoint's 120° cone |
| `position_step_m` | Some(0.001) | in [1e-5, 1.0] | Position quantization step: a replicated root's wire translation is snapped to it (ADR-2413 (4), ADR-2665); `None` sends raw `f32` |
| `interpolation` | Linear | | Spec 74: `Linear`, `Hermite` (replicated velocity) or `None` |
| `snap_distance_m` | 5.0 | ≥ 0 | Spec 74: a larger jump between two states snaps |
| `max_extrapolate_ms` | 100.0 | ≥ 0 | Spec 74: extrapolation past the newest state (0 disables) |
| `appear_immediately` | false | | Spec 74: show a spawned proxy at once |
| `rate_tiers` | None | `0 ≤ near_m ≤ mid_m`, `mid_every ≥ 1`, `far_hz > 0` | Spec 120 § 8.3 distance send-rate tiers: below `near_m` (default 150) every send, below `mid_m` (600) every `mid_every`-th send (3), beyond at `far_hz` (2.0, rounded up to whole send intervals); owned, forced and spawn records never wait. `Some(())` takes every default |
| `extrapolation_gate_m` | None | in (0, 10] | Spec 120 § 8.2 (v2, ADR-8522): the extrapolation gate. With a gate, the acknowledged state channel holds back a transform-only change while the client's extrapolation of every record it may be showing is within this many metres (and 0.5 deg); records are steady, so copies extrapolate them without the `max_extrapolate_ms` limit. The shown error is then up to the gate, not `position_step_m`. `None` is off |

An invalid profile fails to load with the field named (`ProfileError::Invalid { field, message }`, logged
`net.profile_invalid`); entities that use it fall back to the project default (`net.profile_fallback`, once).
Built-ins live under `engine://net/profiles/` (`default`, `player`, `projectile`, `prop`, `held`, `game_state`).

**Versioning.** `format_version: 2` (v2 adds `rate_tiers`, ADR-8031, and `extrapolation_gate_m`, ADR-8522: one
unreleased v2, ADR-8572; a v1 file loads unchanged, with no tiers and the gate off, and `regia upgrade-project` rewrites
its version number); additions come with serde defaults, so profiles written without them load.
Registered in `cargo xtask check-formats` and `regia upgrade-project` since ADR-2316 (spec 12 § 4a): any shape change, an added field with a serde default included, bumps `format_version` and ships a migrator (a version rewrite when older files still read). Profiles
are gameplay content: they belong in the content hash of spec 77 (a client and server with different quantization
cannot join); they are not part of the protocol hash.
