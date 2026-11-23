# High-end lighting

Desktop High and Ultra turn these on. Mobile Low, Desktop Low, and Deck leave most of
them off: ambient (or the existing IBL) instead of probe GI and no screen-space
reflections. Paged sun shadows (cached cascades) are on for Desktop Low, High, Ultra
and Deck, and off for both Mobile tiers and the lab baseline, which redraw every
shadow map every frame. The lab golden harness (`FrameQuality::LAB`)
matches that off state, so older screenshots stay put. Screen-space GI is the cheaper
tier (`GiMode::ScreenSpace`). It is not the Low preset.

## Local lights

`max_visible_lights` is how many point and spot lights a view uploads. Desktop High
is 256, Desktop Ultra 512, Deck 96, Mobile Low 32. The buffer holds 512. The lab
harness and the editor stay at 64, and under that cap the order is the authored
order, so a cached shadow tile does not move. Over the cap the dimmer lights (by
screen importance) are dropped and a warning is logged. A froxel still stops at
`max_lights_per_cluster`; that overflow is counted on `FrameRenderer::light_upload`
and warned. There is no engine constant of 48. The old lab path kept 64 and said
nothing.

Shadowed spots take one atlas tile, points take six. The atlas has 64 tiles.
When the casters fit, assignment stays in authored order. When they do not, a
near spot can take a tile a far point light cannot. `local_shadow_max_lights` is
the profile's count (Desktop High 32, the lab 16).

## Sun shadows

`paged_sun_shadows` caches each cascade as a 4×4 page grid. A caster that moves
dirties the pages its old and new bounds cover. A light-matrix change (a new sun
direction, or a camera move of a shadow texel or more) redraws that cascade. The
cascade center is snapped in light space, including depth, so a smaller camera move
keeps the matrix and the pages. Local lights remember which atlas tile a caster
overlaps. A warm atlas clears and redraws only those tiles. PCSS-lite is the
Desktop High / Ultra filter. The lab stays on 3×3 PCF and does not snap.

## Global illumination

The preset's probes (Desktop High and Ultra) are the GPU DDGI volume; see
[GI probes](gi-probes.md). `FrameRenderer::set_gi_field` below is the optional,
separate CPU volume and stacks with it if both are on.

`GiMode::Probes` is a dynamic irradiance grid in the sense of Majercik et al. 2019,
reduced to an L1 spherical harmonic and a small octahedral depth. For the CPU field, rays are marched
on the CPU against SDFs (boxes, spheres, a mesh voxelization, the terrain
heightfield). Upload the field with `FrameRenderer::set_gi_field` before the frame.
The forward pass adds it on top of ambient when the grid is non-empty. A closed room
does not pick up the sky: probes inside the shell see the shell, and a hit is tested
against the sun. `set_gi_cpu_ms` / `gi_cpu_ms` is the trace time. It is not inside
`gpu_timings`.

`GiMode::ScreenSpace` is one depth-aware gather in a composite pass. It needs a
depth prepass, a single sample, and a linear HDR target (post or TAAU).

## Reflections

SSR is the same class of pass: a short screen march on the floor, then a reflection
probe if the march misses. On the linear march (lab, editor, probe capture) the mix is scaled by
roughness (`ssr_roughness_weight`): a floor at 0.35 stays a mirror, and roughness 0.7 and above
keeps the shaded color. Stock Desktop High and Ultra run stochastic SSR instead: cutoff 0.55,
fade from 0.40, and a probe on any downward-pointing reflection ray.
The forward HDR buffer carries that roughness in alpha; the post stack writes alpha 1.
`FrameRenderer::capture_reflection_probe` renders six 16² faces and packs them
with `equirect_from_faces`. Each face uses `cube_face_axes` for its look direction
and its camera up. World +Y is parallel to the ±Y looks, so those faces cannot be
shot with the ordinary camera up. Capture forces GI, screen-space GI, SSR, other
probes, and paging off, and it renders a native 90° square so the pack matches the
face basis. `max_reflection_probes` is how many of those probes are blended for a
pixel. Box projection uses the probe's center and half-extent.

## Interiors

A tunnel or a cockpit that only sees local lights is black at a sunny EV. AgX's
toe maps 40 lux at EV 15 to display 0. The same 40 lux at EV 8 (`INTERIOR_EXPOSURE_EV`),
under the corrected AgX and the default look, prints near luma 37 for an albedo
around one half. EV 6 prints that floor near luma 91, which reads as a wash. Put an `InteriorVolume` on the
room. When the camera eye is inside it, the extracted frame sets
`quality.interior_ambient`, drops sky IBL, and uses the volume's lux (0 means the
40 lux floor). Without a volume, insert an `EnclosureScene` (a hollow shell, not a
solid box) and extraction calls `detect_enclosure`: four of six cardinal traces
must hit. `build_interior_lighting` runs `plan_interior_coverage` and traces the
grid with the local lights. A miss stores black, so an open mouth is not painted
with the sky. The grid coarsens its longest axis before it exceeds `MAX_PROBES`,
which keeps a tunnel's cross-section. Bind the field with `GiMode::Probes` and
capture one reflection probe at the coverage center. Walls take their bounce from
the field. Outdoor probes replace the sky (ADR-031). An interior adds the bounce
on top of the 40 lux floor (`camera_pos.w`; `quality.w` is the shadow-noise index).
`INTERIOR_BOUNCE_GAIN` (2.5) is the rest of the diffuse series after
that one bounce, so a wall beside a light is not left at the 40 lux floor. On the linear march the probe only reaches floors that pass the reflection gate
(roughness under 0.7, normal.y at least 0.72); stochastic SSR has no `normal.y` test. Do not expect the floor to survive
an outdoor exposure. Pin EV 8, or leave auto-exposure on for a view that does not
also see the sun. The 40 lux term is a floor, not a wash: the lights still have to
read as pools.

## What an agent should expect

- Turning GI off removes the color bleed and leaves ambient.
- Turning SSR off leaves the probe color on reflective floors.
- `gpu_timings().ssgi_ms` and `ssr_ms` are the composite passes. A pass that ran
  reports a positive lavapipe timestamp; zero means that pass did not record.
  Those milliseconds are not a frame budget. The floor-tier number still needs a
  discrete GPU. The RTX 5080 on the build box did not enumerate.
- Do not add these fields to a scene extract. The renderer holds the grids.
