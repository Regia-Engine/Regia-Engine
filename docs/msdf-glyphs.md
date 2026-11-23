# MSDF glyph generation (`regia_ui::msdf`)

Multi-channel signed distance fields (MSDF) render crisp text at any scale: a glyph is baked
once into a small RGB texture, and the shader reconstructs pixel coverage from the **median**
of the three channels. Unlike a single-channel SDF, whose isopleths round off at corners, an
MSDF keeps sharp corners sharp because the two edges that meet at a corner are measured by
different channels. This module implements the published method — Viktor
Chlumský, *Shape Decomposition for Multi-channel Distance Fields* (master's thesis, Czech
Technical University in Prague, 2015; see `docs/provenance.md`) — in plain safe Rust with no
dependencies, for UI and world-space text that must stay crisp when magnified.

## API

| Item | What it does |
|---|---|
| `Outline`, `Contour`, `Segment`, `Point2` | The input: contours of line/quadratic/cubic segments in font units, y up. Nested contours with opposite winding are holes. |
| `parse_glyf_outline(font_bytes, glyph_index)` | Recovers a glyph outline (including composite glyphs) from raw TrueType bytes — no font crate needed. Returns `FontGlyph { outline, units_per_em }`. |
| `generate(outline, size, range, scale, translate)` | The full pipeline → `MsdfBitmap` (RGB `f32`, row 0 at the top). `generate_uncorrected` skips the last pass. |
| `MsdfBitmap` | `to_u8()` for atlas bytes, `texel`/`set_texel`, `coverage(u, v)` — the CPU reference sampler (bilinear per channel → median → smoothstep), and `correct_errors()` (exposed for advanced use). |
| `median3` / `median3_f64` | The median of three — the reconstruction operator. |
| `color_outline(outline, turn_threshold_deg)` | The edge colouring step on its own (cyan/magenta/yellow), for inspection and tests. |
| `EdgeColor::channels()` | Which of the R/G/B fields measure an edge of that colour. |

## Usage

```rust
use regia_ui::msdf::{generate, parse_glyf_outline};

let font = std::fs::read("Inter-Regular.ttf")?;
let glyph = parse_glyf_outline(&font, 68)?;          // glyph id for 'A'
// 64 px square bitmap, field spread over 4 texels, glyph em → texels:
let scale = [64.0 / f64::from(glyph.units_per_em); 2];   // texels per font unit
let bitmap = generate(&glyph.outline, [64, 64], 4.0, scale, [6.0, 10.0])?;
let atlas_bytes = bitmap.to_u8();                    // RGB8, 128 = on the edge
```

Mapping: texel `(x, y)` samples the font point `translate + (texel + 0.5) / scale`, with
row 0 at the top (font y up, bitmap y down). A channel stores `0.5 + 0.5 · d / range` where
`d` is the signed distance in texels (positive inside), so `0.5` is exactly on the edge and
the field spans `range` texels on either side. In the shader:

```wgsl
let d = median(msdf.r, msdf.g, msdf.b) - 0.5;
let coverage = smoothstep(0.0, 1.0, d * px_range + 0.5);
```

`MsdfBitmap::coverage` runs exactly this reconstruction (bilinear sampling included) on the
CPU; the tests use it as the reference.

## The method, step by step (Chlumský 2015)

1. **Edge colouring** (ch. 3). Every edge gets one of cyan, magenta, yellow; a channel
   measures an edge iff the colour contains that channel's primary, so each edge feeds
   exactly two of the three fields. Walking a contour, the colour switches at every corner
   whose tangent turns more than `DEFAULT_TURN_THRESHOLD_DEG` (3°); smooth curve junctions
   keep the colour. Consecutive contours start on different colours, and a closing-vertex
   repair guarantees the two edges of a sharp corner never share a colour pair — even on
   all-corner contours like a square, where 3 colours must colour a 4-cycle.
2. **Exact distance** (§ 2.2). The distance to a Bézier is found by solving
   `(p − γ(t)) · γ'(t) = 0` — a cubic for quadratic segments, a quintic for cubics — with a
   robust solver: recursively find the derivative's roots, which bracket monotone intervals,
   then bisect each sign change to machine precision. Lines use the clamped projection.
3. **Nearest edge and signed pseudo-distance** (§ 2.3). Per channel, the edge of that
   channel's colours with the smallest true distance is chosen; at a corner vertex, where the
   two edges tie, the edge the point is more perpendicular to wins. The channel stores that
   edge's *signed* distance, replaced beyond the edge's endpoint by the signed distance to its
   tangent line (the pseudo-distance). Near a corner the channels therefore disagree in sign
   — the one measuring only the far edge's line reads "inside" just outside the corner — and
   the median of the three reconstructs a sharp corner instead of a rounded one.
4. **Sign by winding** (§ 2.3.2). The edge side becomes inside/outside through the outline's
   orientation (its signed area), so clockwise (TrueType) and counter-clockwise contours give
   the same field. At every texel the median is then checked against the winding number
   (nonzero rule, crossings of a horizontal ray, with vertices on the ray classified
   consistently); where they disagree the channels are flipped. Each contour finds its own
   filled side the same way (winding just left and right of its longest segment), so holes
   (the 'O'), mirrored composite components and oppositely wound contours are all right.
5. **Error correction** (ch. 4). A channel clash is an artifact of bilinear filtering: two
   texels whose channels measure different edges mix into a median on the wrong side of the
   outline. `generate` reconstructs the midpoint of every horizontal and vertical texel pair
   and the center of every 2×2 cell, compares it with the exact signed distance of the outline
   there, and where the texels themselves are right but their mix is wrong by at least half a
   texel, those texels fall back to their median in all three channels. Texels without a
   clash keep their channels. `MsdfBitmap::correct_errors` is an older shape-free heuristic
   kept for callers that only have a bitmap; `generate` does not use it.

Channels with no edges of their colour store `DEAD_CHANNEL_DISTANCE` (saturates to 0/255,
never becomes the median).

## Guarantees and limits

* **Overlapping contours** (common in variable-font default instances) are not merged: the
  sign is repaired, but the distance near an edge that lies inside another contour is the
  distance to that interior edge, so the coverage dips to one half along it (a visible seam).
  Remove overlaps before baking.

* **Deterministic** — no hash maps, clocks or seeds; identical input gives identical bytes.
* **Total** — empty outlines, degenerate and zero-length segments yield a defined field;
  NaN/∞ input returns `MsdfError::NotFinite`; malformed font bytes return errors, never
  panic. No `unsafe`, no new dependencies (the module only uses `std` and `thiserror`,
  which the crate already had).
* **Validation** (45 unit tests plus one measurement test): a square's corners reconstruct
  within 0.5 px at 32×32/range 4 (measured error 0); a 4-arc circle matches the analytic
  distance within 1e-2 everywhere (measured 0.004 texels, the cubic-arc approximation); an
  'O' hole has the outside sign; sharp-corner neighbours never share a channel pair; the
  channels disagree in sign beyond a corner; both contour orientations give the same field;
  reconstructed coverage vs a scanline rasterizer has IoU > 0.98 on six synthetic shapes
  (measured 0.996 to 1.0); byte determinism; a glyf parser round-trip against hand-assembled
  TrueType fonts, including composite glyphs with x/y-scale and 2×2 transforms.
* **Cost**: about 1 to 3 ms for a 48×48 field of a polygon and 25 to 40 ms for one of cubic
  curves (one optimized core on a loaded build host): cook-time or first-use work, not per
  frame.

The module is pure CPU code intended for glyph-atlas bakes (cook time or first-use); it is
not on the per-frame path. The runtime dynamic atlas (`regia_ui::atlas`) keeps its swash
alpha-mask path; MSDF pages are an alternative placement when a text element is magnified
after rasterization (world-space text, diegetic screens).
