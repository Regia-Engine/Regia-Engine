# `.ambience.ron` v1

An ambience as an asset: looping beds whose levels follow the scene's wind and weather, and random
one-shots heard around the listener (`regia_audio::AmbienceDef`, `docs/specs/16-audio.md` § 3e,
ADR-2721). Frozen format (`docs/specs/12-assets.md` § 4a), registered with
`cargo xtask check-formats` as `ambience`. The cook validates every `.ambience.ron`
(`regia_asset_pipeline::audio_cook::cook_ambience`); a file that does not load fails the cook with
its `ambience_*` code.

The example below is `docs/formats/glade.ambience.ron`; tests and the fuzz target load it.

```ron
(
    version: 1,                  // required
    name: "glade",               // required, not empty
    seed: 20261105,              // default 0: every interval, variant, place and level is drawn from it
    bus: "Ambience",             // default "Ambience"
    volume_db: 0.0,              // default 0: offset on every bed and call
    calm_m_per_s: 0.3,           // default 0.3: under this wind the wind drives read 0
    attack_secs: 0.4,            // default 0.4: rising bed level time constant
    release_secs: 1.5,           // default 1.5: falling bed level time constant
    beds: [                      // default none
        (name: "air", sound: Some("amb/air_loop"), drive: Constant, curve: [(1.0, -42.0)]),
        (
            name: "wind",
            sound: Some("amb/wind_loop"),
            drive: Wind,
            curve: [(0.3, -54.0), (2.0, -36.0), (8.0, -20.0), (17.0, -10.0)],
        ),
        (name: "rain", sound: Some("amb/rain_loop"), drive: Rain, curve: [(0.1, -46.0), (10.0, -16.0)]),
    ],
    one_shots: [                 // default none
        (
            name: "birds",
            sounds: ["amb/bird_a", "amb/bird_b", "amb/bird_c"],
            interval_secs: (2.0, 7.0),   // required, 0.01 <= min <= max <= 3600
            distance_m: (8.0, 35.0),     // required, 0 <= min <= max <= 10000
            height_m: (2.0, 12.0),       // default (0, 0): above the listener
            volume_db: (-14.0, -6.0),    // default (0, 0), within -100..=24
            drive: Rain,                 // default Constant
            curve: [(0.0, 0.0), (0.5, -12.0), (2.0, -100.0)], // default none: no offset
        ),
        (
            name: "woodpecker",
            sounds: ["amb/woodpecker"],
            interval_secs: (9.0, 20.0),
            distance_m: (40.0, 90.0),
            height_m: (3.0, 10.0),
            volume_db: (-18.0, -12.0),
        ),
    ],
)
```

## Fields

- **Beds** are the `regia_audio::BedLayer` shape of the ambience bed (ADR-2442): `name` (unique in
  the file), `sound` (a `SoundAssets` name; `None` computes the level without playing), `drive`
  (`Constant`, `Wind`, `Gust`, `Rustle`, `Rain`, `Drips`, `Snow`) and `curve`, `(drive value, dB)`
  points interpolated linearly and clamped at both ends. A non-constant drive at 0 is silent. Each
  bed loops on `bus` and slews toward its level with the attack and release time constants.
- **One-shots**: `name` (unique among beds and one-shots), `sounds` (at least one; never the same
  one twice in a row), `interval_secs` (uniform between the two; min is at least 0.01 s — a
  tinier interval would make the player's per-step due-call loop unbounded on hostile input), `distance_m` (uniform over the
  ring between the two radii around the listener, `r = sqrt(u (max² − min²) + min²)`), `height_m`
  and `volume_db` (uniform), and an optional `drive` plus `curve` whose value is a dB offset on the
  call (a drive at 0 reads the curve too, so birds can sing in the dry and fall silent in rain). An
  offset at −100 dB or below skips the call; the timer keeps running. Calls play through the
  emitter pipeline at their place (distance gain, panning).
- Unknown fields are refused (`ambience_parse`); a NaN, infinite or reversed range, an empty name,
  a duplicate name or a one-shot with no sounds is `ambience_invalid`; a version other than 1 is
  `ambience_version` ("upgrade the engine").

## Determinism

`regia_audio::AmbiencePlayer` draws everything from one PCG32 generator seeded by `seed`, in a
fixed order per step (one-shots in file order, then interval, variant, angle, radius, height,
level), so the same file and the same sequence of steps give the same calls, and the offline
mixer renders the same samples (`crates/regia_audio/tests/ambience_file.rs`).

## Versions

| Version | Change | Migration |
|---|---|---|
| 1 | First version (ADR-2721). | None needed; `regia upgrade-project` leaves v1 files as they are. |
