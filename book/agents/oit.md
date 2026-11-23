# Order-independent transparency

Translucent meshes normally draw back to front, one whole mesh at a time. That fails on what no mesh order can draw: two surfaces that
pass through each other, or one mesh wrapped around another. Weighted blended transparency needs no order. Every fragment adds its light
and coverage to two pooled targets, and a full-screen composite lays the result over the image.

## Turning it on

One switch, off by default: `RenderProfile::order_independent_transparency`, or `render.set_oit { enabled }`. No preset turns it on yet.
Nothing changes in a material: alpha-blended and premultiplied materials take the new path, and a frame without one costs nothing.

## How it works

1. **Accumulation.** Translucent draws, material graphs included, leave the sorted run and draw through the `OIT` forward variant, after the opaque draws and the sky.
   One target sums each fragment's premultiplied light and coverage, scaled by a weight that falls with distance from the eye. The other
   multiplies together what each fragment lets through (the revealage). Nothing writes depth.
2. **Composite.** A full-screen pass inside the draws' screen bounds lays the weighted average of the light over the image, in proportion to what
   the layers covered, and writes the reactive mask.
3. **The rest of the blended run** then draws as before: glass and other transmissive surfaces, additive materials, x-ray overrides, masked instances
   and thin films. Refraction sees the composite.

A hair card stays in the sorted run. The pass rejects `draw.hair`, including a card whose `HairAlpha::Sorted` is alpha-blended, so crossing hair cards still depend on draw order. A hashed card alpha-tests and writes depth, and never comes here. ADR-2732 (proposed) describes sorted hair taking the pass; that route is not drawn.

The share of the background that shows is exact, in any order. One layer is exactly the sorted result, and so are layers of one color. The color of
several different layers is an approximation that favors nearer ones: where a sorted frame shows only the front layer's color, this one shows a little of the back's.

## The reactive mask

The composite writes an `R8Unorm` mask at the render size: the covered share of each pixel × 0.9, and 0 where nothing translucent drew. A translucent surface writes no motion
vectors and its light sits over a background that may move, so the history cannot follow it. The mask tells the temporal upscaler to trust this frame there:

| Upscaler | How it reads the mask |
|---|---|
| TAAU | History weight × (1 − mask). |
| TSR, FSR 3 | Their reactive input. |
| DLSS and other vendor upscalers | `UpscaleFrame::reactive`; DLSS takes it as its current-color bias. |

A host's own temporal pass reads the last frame's mask with `FrameRenderer::oit_reactive_mask`. It is `None` on a frame that did not run the pass, and an upscaler then
runs as it does without the feature.

## What it costs

Nothing unless the switch is on and a translucent mesh is visible. Such a frame takes three pooled targets the size of the HDR image (8, 2 and 1 bytes per pixel), one more
render pass per frame for the draws, one for the composite and one to go on with the blended run. While such a mesh is only culled the targets stay held,
so its return creates nothing. A first use mid-play never stalls: the mesh draws sorted while its pipelines build. A game-frame warm that sees the material builds them first.

## Where it falls back

A frame that cannot run the pass draws its translucent meshes sorted and says why in `render.oit`'s `frame.reason`:

| Code | Meaning |
|---|---|
| `profile_off` | The profile does not ask. |
| `no_material` | No draw in the frame can take the pass. |
| `no_hdr_target` | The frame has no HDR image (no post stack and no temporal upscale). |
| `msaa` | The frame is multisampled. This stays: see below. |
| `forward_lite` | A Mobile lite tier. |
| `no_independent_blend` | The device cannot blend two targets differently. |
| `pipelines_building` | The pipelines are still compiling. |
| `no_visible_draw` | The frame's translucent draws are all culled. The targets stay held. |

A multisampled frame keeps the sorted run by decision (ADR-2732). Accumulating per sample needs multisampled targets, a per-sample composite and the frame's resolve order to change, for a mode no preset that could use this pass runs. Accumulating against resolved depth would leave translucent edges unmatched with the opaque samples behind them.

Limits:

- A translucent surface in front of a pane draws under it, tinted by it: the composite comes before glass, so that a surface behind a canopy keeps the canopy's reflection over it.
- Additive materials and particles draw after the composite, so they show undimmed through a translucent surface in front of them. Particles keep their own sorted layers.
- The depth weight is at its floor past about 160 meters, so layers beyond that average evenly.
- A very bright layer next to a dim one in the same pixel loses some of its share: its weight is held so it fits the half-float target.

## Agent methods

| Method | What it does |
|---|---|
| `render.oit` | `{ enabled, frame: { requested, drawn, fallback, sorted, passes, covered_permille, reactive_mask, reason } \| null }` |
| `render.set_oit { enabled }` | Switches the active profile's knob. A preset change turns it off. |

`drawn` counts the draws in the pass, `fallback` the ones that could have been but drew sorted this frame, `sorted` the blended draws that stay in the sorted run
(glass and other transmissive draws, additive materials, x-ray overrides, masked instances, thin films and hair)
on a frame that ran it. There is no `frame.hair` field. `covered_permille` is the share of the image inside the composite's bounds. `F0002` for a non-boolean `enabled`. `F0003` when the app has no `RenderSettings`.
`render.graph` lists the `oit_accum` and `oit_composite` nodes after `forward` (they record in its middle), and `taau` reads `reactive_mask`.

## How it was checked

`crates/regia_render/src/oit.rs` holds the arithmetic and its unit tests: a single layer is the "over" operator, the order changes nothing, the coverage is the exact
product. `crates/regia_render/tests/oit_frame.rs` holds the frame to that model and to the sorted frame, shows crossing plates changing with the draw order when sorted
and not with the pass, reads the mask back, and checks that each built-in upscaler takes up a translucent plate's new color faster with it. A frame that cannot run the pass is byte for byte the
sorted frame. `oit_frame_hitch.rs` is the hitch gate (no pipeline and no target created on a warmed frame's play frames) and `oit_frame_alloc.rs` counts a steady frame's allocations.
