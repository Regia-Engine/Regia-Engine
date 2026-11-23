# Baked GI and screen-space AO

A baked GI volume ([Baked GI volumes](baked-gi.md)) already holds the occlusion of the geometry it
was baked with. A probe beside a wall sees the wall, so the probe light at the wall's foot is
already darker. The screen-space AO (SSAO, GTAO rows) then darkens the same light again for the
same wall. Without a correction, the foot of a baked wall or the inside of a bay lost its sky light
twice. The capsule occluders of dynamic units have the same problem: the capsule darkens the baked
light next to a unit, and the SSAO sees the unit too.

Since v0.1.28 the renderer splits the work by scale (ADR-1104). This is on by default.

## What the split does

- Where the volume's light covers a pixel, each SSAO sample counts for the probe light only if its
  point is low over the surface: below about half the probe cell (`PROBE_AO_RESOLVE` = 0.5 × the
  cell edge, ramped from a quarter to three quarters of the cell). Higher points are above the
  probes, so the probe light already holds them.
- A detail brick resolves at its largest cell edge. With 1 m bricks, the SSAO keeps contact
  shade up to about 0.25 m and drops it by 0.75 m.
- The terrain layer resolves at its column spacing. 8 m columns resolve nothing the 1.2 m kernel
  sees, so terrain-lit pixels keep the whole SSAO, as before.
- The sky light outside the volume's coverage keeps the whole kernel.
- Each capsule occluder leaves to the SSAO the share of its occlusion the kernel counts. That is the
  kernel's falloff at the capsule's distance, times the same height weight. Beyond the kernel's
  1.2 m reach, the capsule keeps all of its occlusion. With SSAO off, capsules keep all of theirs.

The split never darkens a pixel compared with the old product, and the probe share is never lighter
than the probe light alone.

## What it costs

The baked screen pass now runs before the SSAO. It writes three extra words per lookup texel (the
two shares, the brick's and the terrain layer's resolve distances; about 0.7 MB more at 1440p).
Where several baked volumes blend (tiles, a streamed level's bake, a nested set piece), the shares
and resolve distances blend with their light, so each texel carries its own. The SSAO kernel reads
them once per AO texel and does two extra multiply-adds per sample. Frames without a baked volume
run the kernel exactly as before.

## Host API

- `BakedGiSettings::probe_relative_ao` (default `true`). Set it with
  `FrameRenderer::set_baked_gi_settings`. `false` multiplies the whole SSAO and the whole capsule
  occlusion into the baked light (the old look).
- `BakedGiStatus::probe_relative_ao` reports whether the last main-view frame ran the split: the
  setting is on, a volume lit the frame and the SSAO ran.
- `StaticGiSample` (from `sample_static_gi` and `sample_static_gi_blend`) carries `brick_share`,
  `terrain_share`, `brick_cell` and `terrain_cell`. Each share is the layer's part of the coverage
  as the lookup blends it (weighted by how much of its cell sees the point).
- `regia_render::baked_gi::ProbeAoSplit` is the split on the CPU. `ProbeAoSplit::of(&sample)`
  builds it, and `ssao_visibility` and `capsule_ssao_share` mirror the kernel and the
  screen pass. `capsule_visibility_split` is `capsule_visibility` with the SSAO's share taken out.

## Agent methods

- `render.get_gi_mode` returns `baked.probe_relative_ao` (the status above).
- `render.set_baked_gi_ao {"probe_relative": false}` switches the split off, and `true` switches
  it back on. It returns `{ before, probe_relative, ran_last_frame, volume_installed, note }`. It
  applies from the next frame.
- Errors: `F0002` (invalid params) when `probe_relative` is missing or not a boolean. `F0003` (not
  found) when the app has no runtime renderer.

## Checking it

1. On a baked map, `render.get_gi_mode`. `baked.active` and `baked.probe_relative_ao` are both
   `true`. If `probe_relative_ao` is `false`, check that the preset runs SSAO (`render.get_profile`).
2. Frame a set piece's wall foot, a doorway or a vehicle bay under a detail brick. `play.pause`, then
   `seeing.screenshot`.
3. `render.set_baked_gi_ao {"probe_relative": false}`, `play.step 1`, and screenshot again. The old
   look is darker in a band along the wall foot and inside the bay. Contact lines right at the
   corner stay dark in both.
4. With a unit and its capsules (`render.set_gi_occluders`), compare the ground beside the unit.
   The old look has a darker, wider halo.
5. Switch back with `{"probe_relative": true}`.

## Evidence in the repo

`crates/regia_render/src/baked_gi/ao_split.rs` tests the split on model scenes. A floor receiver
sits beside static geometry lit through a 1 m brick, or beside a dynamic sphere with a capsule. The
exact cosine-weighted visibility is ray cast. The probe field and an idealised kernel are modelled,
and the mean error is compared over receivers 0.05 to 1.2 m from the occluder:

| Scene | Old product | Split |
|---|---|---|
| 3 m wall | 0.089 | 0.072 |
| Overhang 0.6 m up | 0.110 | 0.087 |
| 1 m block | 0.025 | 0.017 |
| 0.3 m curb (under the probes) | 0.018 | 0.019 |
| 1 m sphere, capsule, terrain-lit | 0.056 | 0.011 |
| 1 m sphere, capsule, brick-lit | 0.056 | 0.013 |

The split leaves a residual. The probe field's visibility is a smoothed field, not a ray cast from
each receiver, so part of a tall wall's double count remains at its foot. Geometry that is neither
baked nor a capsule (a dynamic prop without capsules) keeps only its low SSAO under a brick.

`crates/regia_render/src/baked_gi.rs` checks that the lookup reports its brick and terrain shares,
and that the shared WGSL ramp is the same text in `ssao.wgsl` and `baked_gi_screen.wgsl`.
