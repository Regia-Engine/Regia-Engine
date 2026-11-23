# .gait.ron: gait settings (v1)

Gap G7. Code: `regia_animation::gait_asset` (`GaitFile`, `GaitAsset`, `GaitAssets`). Guide:
`book/agents/procedural-gait.md` ("Gait files: `.gait.ron`") and `book/user-guide/animation.md`
("Legged units: settings in a `.gait.ron` file"). Registry name `gait`. Version 1 is the first version:
the migrator registry is empty and `regia upgrade-project` passes a v1 file through unchanged (the
version is registered so a version 2 can migrate). A `format_version` other than 1 is refused as
`gait_version` before any other field is read.

A `.gait.ron` holds everything the procedural gait stepper (`regia_animation::gait`, ADR-8060, ADR-8792,
ADR-8653, ADR-8706) and the engine-side walker (`GaitRig`, ADR-8707) need to walk a legged rig: the
patterns and the speeds that switch between them, the speed limits, the step, the ground and foothold
settings, the body posture and the legs by joint name. It is presentation data: no simulation checksum
reads it.

The file is gameplay data read at runtime (`SourceKind::GameplayData`): it ships as text, the dev watcher
forwards a change as `SourceAssetChanged`, and `GaitAssets` reads and validates it again.

## Schema

```
GaitFile {
    format_version: u32,                  // 1
    patterns: [GaitPatternSpec],          // 1 to 4, slowest first, all for the same leg count
    switch_speeds: [f32] = [],            // m/s between neighboring patterns: patterns.len() - 1 of them
    hysteresis: f32 = 0.0,                // m/s half-width of each switch band
    speeds: GaitSpeeds,
    step_length: f32,                     // m of body travel per stance, > 0
    swing_lift: f32,                      // m, >= 0
    clearance: f32,                       // m of body height above the stance floor, >= 0
    max_slope: f32,                       // rise over run, >= 0
    max_reach: Option<f32> = None,        // m, every leg; None = no limit
    settle: GaitSettle,
    turn: GaitTurn,
    ground_band: Option<f32> = None,      // m half-height around the expected floor; None = any height
    foothold: GaitFoothold,
    hang_drop: f32,                       // m, >= 0
    posture: Option<GaitPostureSpec> = None,  // None = the upright body (GaitPosture::UPRIGHT)
    body: Option<String> = None,          // joint GaitRig lifts to the stance height
    legs: [GaitLegSpec],                  // in pattern order, exactly the patterns' leg count
}
GaitPatternSpec =
      Tripod                              // 6 legs, duty 0.5
    | Ripple(u8)                          // legs: even, 4 to 16
    | Wave(u8)                            // legs: even, 2 to 16
    | QuadrupedWalk | Trot | Pace | Bound // 4 legs
    | BipedWalk                           // 2 legs
    | Explicit(offsets: [f32], duty: f32) // one offset per leg in [0, 1), duty in (0, 1)
GaitSpeeds { forward: f32, reverse: f32, strafe: f32, acceleration: f32, idle: f32, yaw_rate: f32 }
GaitSettle { distance: f32, period: f32 }
GaitTurn { step_distance: f32, step_angle: f32 }      // step_angle in radians, [0, pi]
GaitFoothold { candidates: u32, spacing: f32 }       // candidates 0 turns the search off, at most 64
GaitPostureSpec {
    swing_weight: f32, height_tau: f32, height_slew: Option<f32> = None,  // None = no limit
    attitude_weight: f32, max_pitch: f32, max_roll: f32, attitude_tau: f32,
}
GaitLegSpec {
    root: String,                         // chain root joint (hip, coxa), not empty
    tip: String,                          // chain tip joint (foot), not empty
    hip_offset: Option<[f32; 2]> = None,  // the stepper's hip_offsets entry; None = measured (GaitRig) or 0
    max_reach: Option<f32> = None,        // overrides the file's max_reach for this leg
}
```

Unknown fields are refused (`gait_parse`), so a typo never silently drops a setting. Only fields whose
absence means "off" have defaults: no pattern switching, no reach limit, any ground height, the upright
body, no body joint, no per-leg override. Every tuning value is required; there are no game tuning
defaults. Angles are radians and lengths meters, as in `GaitParams`; each field's range is the one
documented on the matching `GaitParams` or `GaitPosture` field.

`hip_offset` is the stepper's frame (see the table in `regia_animation::gait`): `[right, forward]` in
driven mode, which is what `GaitRig` uses by default.

## Loading and errors

`GaitAsset::from_ron(text)`:

1. reads `format_version` (`gait_parse` when it is missing; `gait_version` when it is not 1);
2. parses the typed document (`gait_parse`);
3. builds each pattern and the pattern set (`gait_pattern`);
4. checks the leg list against the patterns' leg count and for empty names (`gait_legs`);
5. builds `GaitParams` and runs the stepper's own `GaitParams::validate` (`gait_settings`, `gait_pattern`).

Reading a file that cannot be opened is `gait_io`. `GaitAssetError::code` gives the code; the message
starts with it. `anim.gait_validate` returns the same codes.

## Writing

`GaitFile::to_ron` writes canonical pretty RON at the current version (one save is a fixed point).
`GaitFile::from_params(&params, &legs, body)` writes a file that loads to the same settings bit for bit
(every pattern `Explicit`, every hip offset written, the shared reach limit taken from the first unused
`max_reach` entry).

## Example

[`gait.example.ron`](gait.example.ron): a six-leg walker with three patterns (wave, ripple, tripod), a
posture, a body joint and hip offsets for the front pair only.

## Versions

| version | change | migration |
|---|---|---|
| 1 | first version (gap G7); frozen in v0.7 format batch 2 (ADR-10266) | none: v1 passes through |

Registry: `docs/formats/format-hashes.ron` row `gait`, `cargo xtask check-formats`, `regia upgrade-project`
row `gait` (`.gait.ron`), fuzz target `anim.gait` (untrusted text: parse, validate, write back), format
corpus `crates/regia_agent/tests/fixtures/format-corpus/gait/`: `v1.ron` (a six-leg tripod) and, since the
freeze, `v1-hexapod-switching.ron` (four patterns with switching, an explicit pattern, a posture, mixed leg
overrides), `v1-quadruped.ron` (every four-leg preset) and `v1-biped.ron` (required fields only).

**Frozen (v0.7, ADR-10266).** v1 is frozen: any change to a field, a pattern variant or a nested block
is a version 2 with a migrator from v1, a `v2.ron` corpus file, and the v1 corpus files still loading.
Between them the v1 corpus files hold every pattern variant and both sides of every optional field;
`regia_animation --test gait_v1_freeze` fails if a variant or an optional field loses its corpus
coverage, if a nested block accepts an unknown field, or if another version loads.
