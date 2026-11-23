# `config/xr.ron`: XR project settings

Owner: `regia_xr::config` (spec 58 § 4.1, ADR-1676). Current `format_version`: **1**. Loaded by `XrPlugin`;
every field has a default, so `(format_version: 1)` is a valid file. Unknown fields are a load error that names the
field (`deny_unknown_fields`). A newer `format_version` is refused (`FormatTooNew`); there is no migrator yet
because there is only one version. Registered in `cargo xtask check-formats` (`xr-config`) and `regia upgrade-project`
since ADR-2316.

## Fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `format_version` | u32 | 1 | Must be 1 (0 and newer values are refused). |
| `backend` | `Auto`, `OpenXr`, `Simulator`, `Off` | `Auto` | Which backend opens. `Auto` and `OpenXr` resolve to `Off` with the reason `xr_no_loader` in this build (the real OpenXR binding is not built); `Simulator` runs the scripted backend with no GPU or headset. |
| `required` | bool | false | True: the app refuses to start without a session. False: a missing headset leaves the desktop game running. |
| `tracking_space` | `Local`, `Stage`, `LocalFloor` | `Stage` | Requested space; the actual one may fall back (`LocalFloor` is emulated from `Local` plus `comfort.eye_height_m`). |
| `render_scale` | f32 | 1.0 | Multiplier on the runtime's recommended per-eye size, 0.5 to 2.0. |
| `foveation` | `Off`, `Low`, `Medium`, `High` | `Off` | Fixed-foveation level, best effort. |
| `mirror` | `Off`, `Left`, `Right`, `Both` | `Left` | The desktop mirror window. |
| `comfort` | struct | see below | Comfort options. |
| `preferred_refresh_hz` | `Option<f32>` | `None` | Refresh rate to request, 30 to 240. |
| `retry_frames` | u32 | 120 | Fixed steps between attempts to reach a headset or re-create a lost session, 1 to 7200. |
| `near_m` | f32 | 0.05 | Near plane of the eye projections in meters; finite and above 0. |

### `comfort`

| Field | Default | Range | Meaning |
|---|---|---|---|
| `snap_turn_deg` | 30 | 0 to 180 | Degrees per snap; 0 is smooth turning. |
| `snap_threshold` | 0.6 | `snap_rearm` < value <= 1 | Stick deflection that fires a snap. |
| `snap_rearm` | 0.3 | above 0 | Deflection below which the snap re-arms. |
| `snap_cooldown_frames` | 8 | | Fixed steps after a snap before the next. |
| `smooth_turn_deg_per_s` | 90 | 0 to 720 | Smooth-turn speed at full deflection. |
| `vignette_strength` | 0.5 | 0 to 1 | Peak darkening; 0 disables the vignette. |
| `vignette_inner`, `vignette_outer` | 0.6, 1.0 | 0 <= inner < outer | Radii where darkening starts and is full. |
| `vignette_ease_frames` | 6 | | Easing time constant in fixed steps. |
| `eye_height_m` | 1.65 | 0.5 to 2.5 | Eye height for an emulated floor. |
| `dominant_hand` | `Right` | `Left`, `Right` | Dominant hand. |
| `seated` | false | | Seated mode: the `Local` space plus the eye-height offset. |

## Example

```ron
(format_version: 1, backend: Simulator, tracking_space: Stage, render_scale: 1.0, mirror: Both,
 comfort: (snap_turn_deg: 45.0, vignette_strength: 0.3))
```

## Errors

`invalid_value` names the key and the allowed range (for example `render_scale: must be 0.5..=2.0`);
`format_too_new` names the supported version. Agent surface: `book/agents/xr.md`.
