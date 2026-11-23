# Temporal chart (agents)

Spec 27 § 2.3 judges a temporal upscaler on five numbers measured on a chart while the camera
moves. `seeing.temporal_chart` renders that chart through the real frame on the app's render
device, with the upscaler and render scale you name, and returns the five numbers with their
targets. Use it to check a change to TSR, TAAU or the FSR 3.1 port, or to decide whether a
render scale is good enough for a preset. Guide topic: `agent.guide {"topic": "temporal_chart"}`.
Design: ADR-2638.

The chart (`regia_render::temporal_chart`) is one world unit per output pixel at the head-on
pose, so every bar edge sits on a pixel edge and the native still is not aliased:

- six bands of vertical line pairs, bar widths 1, 2, 3, 4, 6 and 8 px;
- a lattice of 1 px members (vertical every 5 px, horizontal every 6 px) in front of a far wall
  at twice the distance, so the members are thin in luma and in depth and the wall moves with
  parallax;
- a separate scene where a bright 24 × 24 px box moves 4 px/frame over 4 px ground stripes.

## Method

- `seeing.temporal_chart { pan_px?, orbit_deg?, frames?, scale?, upscaler? }` returns
  `{ upscaler, scale, frames, motion, motion_clarity, thin_geometry, ghosting, disocclusion_px,
  stability, meets_targets, failing, checks, targets, detail, frames_rendered }`. Read-only;
  permission `read`; a heavy call: it blocks the app for the whole run (at the default 120
  frames about 400 frames of 320 × 180).
  - `upscaler`: `tsr` (default), `taau` or `fsr3`. Vendor upscalers are refused.
  - `scale`: render scale, 0.25 to 1 (default 0.67). `taau` at 1 is native TAA.
  - `pan_px`: horizontal pan in output pixels per frame, -32 to 32 (default 2, the spec's).
  - `orbit_deg`: orbit around the chart's vertical axis instead, degrees per frame, -32 to 32
    (the spec's is 1). The orbit goes back and forth within 30° either side and ends head-on,
    so the last frame is scored like the pan's.
  - `frames`: motion frames, 2 to 480 (default 120, the spec's).

```json
{"method": "seeing.temporal_chart", "params": {}}
{"method": "seeing.temporal_chart", "params": {"orbit_deg": 1, "scale": 0.5}}
{"method": "seeing.temporal_chart", "params": {"upscaler": "fsr3", "scale": 0.67}}
```

The app needs a render device: a windowed app with `RenderPlugin` has one; in a headless app call
`seeing.screenshot {"target": "offscreen"}` once first, which creates one.

## The five numbers

| Field | What it measures | Target |
|---|---|---|
| `motion_clarity` | MTF50 of the line pairs in the last moving frame over the native, un-jittered still of the same pose | ≥ 0.85 |
| `thin_geometry` | Share of the lattice's vertical members that keep at least half the native still's median member contrast | ≥ 0.90 |
| `ghosting` | 8 frames behind the moving box, the mean signed difference toward the box between the frame and the same run without the box, over the box/ground contrast (worst sampled frame) | ≤ 0.02 |
| `disocclusion_px` | Behind the box's trailing edge, the run of pixels whose FLIP against the native still of the same frame exceeds by 0.1 or more both the box-free run's FLIP and that of a box-free frame with its history dropped at the same jitter (what a pixel uncovered this frame can show at best); median over the box rows, worst scored frame | ≤ 2 |
| `stability` | A held chart's mean frame-to-frame FLIP over the last 8 of 48 frames | ≤ 0.01 |

`checks` holds `{ value, target, met }` per number and `failing` names the ones that miss.
`detail` adds the moving and native MTF50, the per-band modulation (1, 2, 3, 4, 6, 8 px), the
absolute trail residual (it also counts the uncovered ground's re-accumulation noise, so it is
reported beside the signed ghosting, not gated) and the held chart's mean |ΔL| in 8-bit levels.

The numbers depend only on the scene, the upscaler and the scale; lavapipe and a card agree
within a few hundredths on the chart (ADR-077). The ghosting and stability parts do not depend
on `pan_px` or `orbit_deg`.

## Errors

- `temporal_chart_unknown_upscaler`: `upscaler` names no upscaler (the hint lists the three).
- `temporal_chart_vendor_upscaler`: `dlss` or `xess`; the chart runs the built-in ones only.
- `temporal_chart_no_device`: the app has no render device (see above).
- `temporal_chart_render_failed`: a chart frame failed on the GPU; the message carries the error.
- `F0002`: a value out of range or of the wrong type, or both `pan_px` and `orbit_deg`.

## Measured

The table for TSR, TAAU and FSR 3.1 at 0.67 and 0.5 is in spec 27 § 2.3 ("Measured, TSR
quality") and ADR-2638. Reproduce it with
`cargo test -p regia_render --test temporal_chart -- --ignored --nocapture chart_table`.

The 2026-11-09 TSR change (ADR-4859) lifts TSR 0.5's pan clarity to 1.00 and its held stability
to 0.0092 on this chart; spec 27 § 2.3 has the before and after numbers. A `motion_clarity` of
1.00 on the orbit can come from moiré in the 1 px band. Before you read it as resolved bars,
look at the frame (`screenshot`) or compare it with the pan.

The 2026-11-14 change (ADR-7694, tracked locks) lifts TSR 0.5's orbit `thin_geometry` from 0.68
to 0.905 and TSR 0.67's from 0.94 to 1.00. At 0.5 the margin is one lattice bar (57 of 63), and
one 120-frame orbit number moves with the jitter phase it ends on: compare runs at the same
`frames`, not a number from another run length.

The 2026-11-15 change (ADR-7876) lifts that 0.5 orbit number to 0.952 on an RTX 5090 and on lavapipe
and to 0.921 on Metal, where it read 0.889 before. Different GPUs still lose different bars:
compare a change against a run on the same GPU, not against a number from another GPU.
