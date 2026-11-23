# Display calibration

Regia Engine's look is judged on a good monitor in a dim room. Players see it on dim laptop panels, TVs
that crush their first code values, phones in daylight and HDR TVs. Display calibration lets the
player correct for their screen without changing the content, the way a TV's picture settings do.

## What it changes

Calibration runs as a **display stage** on the finished frame: scene, VFX and UI, after the
tonemap, just before present. The calibration screen's own test patterns go through the same
curve they measure.

| Setting | Range (neutral) | Effect |
|---|---|---|
| `black_level` | −1 … 1 (0) | Shadow curve on the lower half of the range. For `v < 0.5`, `v' = ½(x + a·x·(1 − x)²)` with `x = 2v` (equivalently `v' = v + a·v·(1 − 2v)²`; `a = 2.5·s` lifting, `0.9·s` deepening). Black stays black, mid gray and everything brighter never move, slope 1 at mid gray. It is a curve, not a gain: the slope at black is up to 3.5×, and code 3 rises by about 3.38× at full lift. Code 120 barely moves. |
| `gamma` | 1.8 … 2.6 (2.2) | SDR only: `v' = v^(g / 2.2)`, the picture as a display decoding with gamma `g` would show it. Higher is darker mid-tones. |
| `paper_white_nits` | 80 … 500 (203) | HDR: luminance of UI and diffuse white (ITU-R BT.2408 reference white by default). |
| `peak_nits` | 200 … 10 000 (1000, or what the display reports) | HDR: the brightest highlight. The post blit expands tonemapped highlights above half of paper white up to `peak / paper` (C¹ quadratic). |
| `min_nits` | 0 … 1 (0) | HDR: the display's black; output is `min + n·(1 − min/peak)`. |

HDR output encodes in the stage: Rec.709 → Rec.2020 (ITU-R BT.2087) and SMPTE ST 2084 (PQ) for
HDR10, or linear ÷ 80 cd/m² for scRGB.

`DisplayCalibration::NEUTRAL` is the identity. On an SDR surface the stage does not run at all
at neutral, and a GPU test (`neutral_stage_is_byte_identical`) proves every 8-bit code value
comes back unchanged when it does run. Goldens and neutral screenshots are therefore unaffected.

## Defaults per platform

| Platform | Out of the box |
|---|---|
| Desktop | neutral |
| Android (phone/tablet) | `black_level` 0.3: outdoor-friendly shadow lift |
| Steam Deck (`SteamDeck=1` or the Deck render profile) | `black_level` 0.15: LCD panel, ~1000:1 |

Android reports the panel's HDR luminance (`Display.getHdrCapabilities`, API 24+); while the
calibration is still the platform default, its peak and minimum are taken from there. Desktop
winit exposes no display luminance yet (Windows DXGI `IDXGIOutput6::GetDesc1` is the follow-up).

## Persistence

The player's values live in `GameSettings.graphics.calibration` (`settings.ron`), with an `hdr`
flag (request HDR10) and `peak_nits: None` meaning "what the display reports". `None` for the
whole section means the platform default. Older settings files load unchanged.

## The calibration screen

```rust,wrap
use regia_framework::prelude::*;
# use regia_app::prelude::App;
# use regia_ecs::prelude::MessageReader;
# let mut app = App::new();

app.add_plugins(CalibrationScreenPlugin { open_on_first_launch: true });
// From the options menu:
app.world_mut().resource_mut::<CalibrationScreen>().open();
// React when it closes:
fn on_closed(mut closed: MessageReader<CalibrationScreenClosed>) { /* … */ }
```

Pages:

1. **Brightness**: two logos on true black, the left at code ≈ 9, the right at code ≈ 3.
   "Adjust until the logo on the left is barely visible and the logo on the right is not."
   On HDR this page also has the HDR black slider.
2. **Contrast** (SDR): a solid square at the 2.2 mid-gray code value inside 1-pixel black/white
   lines (50 % of white on any display). Matched, the screen decodes like the mastering display.
3. **HDR paper white**: a white card next to an 18 % gray card.
4. **HDR peak brightness**: the display stage draws a ring at the chosen peak on a
   10 000 cd/m² background. The background clips to the panel's real maximum, so the ring
   disappears once the peak reaches it.

Each page leaves a transparent strip along the bottom, so the live scene shows while the
player adjusts. Values preview live. **Done** saves, **Cancel** (or Back on the first page)
restores the previous calibration, **Reset** returns the page to the platform default.

Input: mouse and touch drive the sliders and buttons. Arrows and the d-pad move focus
(up/down) and step the focused slider (left/right). Enter or gamepad A activates, Escape or
gamepad B goes back.

Strings: a game overrides any `calibration-*` key in its own
`assets://localization/<locale>/*.ftl`. Otherwise the bundled strings are used (English, German,
French, Spanish; `crates/regia_framework/locales/`).

## Evidence

`docs/evidence/calibration/2026-10-08/` shows the DO27 GI sheet shots (Mars close, Europa trench)
through three simulated problem displays, at default and calibrated values. Regenerate with
`cargo run -p regia_render --example calibration_sheet -- <out> <shot.png>...`.
`DisplayModel` simulates a display as gain-offset-gamma plus a crush threshold and reflected
ambient light. `DisplayModel::solve` emulates a player running the screen.

Agents: see [Display calibration (agents)](../agents/display-calibration.md).
