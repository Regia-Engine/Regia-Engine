# Bent normals (indirect light along the open direction)

Probe light is smooth: a probe a cell away from a corner holds roughly the light of the open
room, and interpolation spreads it into creases, the ground under a hull and the base of a wall
that the probe grid is too coarse to see. Read along the surface normal, that light comes in from
directions the surface cannot see. Bent normals read it along the directions the surface can see.

The SSAO pass (its GTAO rows) already measures, for each pixel, how far nearby surface points rise
above the pixel's tangent plane. With bent normals on, the same samples also lean the normal away
from the occluders they found: each occluded share pushes it back from that sample's direction.
The result is the pixel's bent normal, the mean direction of its unoccluded light (at most 60° from
the normal). A floor right beside a tall wall leans about 15° away from the wall at the kernel's
reach (an infinitely tall wall with an unlimited kernel would give 32.5°). An open floor keeps its
normal exactly.

The forward pass then reads its indirect light along the bent normal instead of the normal: the sky
irradiance, the irradiance probes (DDGI) and the baked GI volume's screen lookup. The bent normal
is stored as a lean from the depth normal, so a normal map keeps its detail: the forward pass adds
the lean to its own normal-mapped normal. Blended and transmissive surfaces keep the normal (they
lie in front of the depth the SSAO pass saw), as they do for the AO.

What it changes is the direction the light is read from, not how much the AO darkens: a crease
takes less of the light that arrives from the occluder's side (the sky above a wall's foot, or
probe light interpolated from beyond the occluder) and more from its open side. Probe light that
leaks in from every direction at once is not removed. The diffuse change is small (a few percent
in creases, on lavapipe and on a 5090 courtyard); the larger change is the specular occlusion
below. The SSAO pass sees only what the depth buffer shows within its reach (1.2 m, at most a
tenth of the screen): the ground under a hull whose underside the camera cannot see gets neither
AO nor a lean.

Where a baked volume lights the pixel, an occluder that the probes already hold (larger than about
half a probe cell; the probe-relative AO split) does not lean the normal: the probe light already
comes from the open side, and leaning it again would brighten the crease.

## Specular occlusion from the visibility cone

With bent normals on, the SSAO's share of the specular occlusion takes a cone form (the
material's occlusion texture keeps its AO-based form, the authored one included: cavities have no
direction; the two multiply, as the AO itself does). The visible
directions are taken as a cone around the bent normal, as wide as the SSAO allows (a cone of
half-angle α keeps sin²α of the cosine-weighted hemisphere, so cos α = √(1 − AO)). The specular
lobe is a cone around the mirror direction: twice the angle at which the GGX distribution falls to
half its peak, at least about 11° (perceptual roughness 0.5 about 19°, 0.8 about 58°, 0.92 and up
the whole hemisphere). The floor is there because a visibility cone from one AO value is no sharper
than that: a mirror-thin cone drew a hard black line along wall bases. The occlusion is the share
of the lobe's cone inside the visibility cone, relative to its share above the surface's horizon,
so an open surface keeps all of its reflection. A reflection that points toward the open side
keeps more than one that points into the crease. It is combined with the reflection occlusion and
the probe field's sky share as before (the smallest wins, once). The part it blocks beyond the
AO-based form reflects the enclosure (a mid-gray wall under the local indirect light), as the
reflection occlusion's blocked part does: a mirror floor at a wall's foot shows a dim wall, not
black.

## Where it runs

`RenderProfile::bent_normals` / `FrameQuality::bent_normals`: on for Desktop High and Ultra (with
their GTAO rows of SSAO), off on Desktop Low, the Medium tier (Desktop High at 0.67 scale), Deck
and mobile. Lab and editor baselines (`FrameQuality::LAB`, `FrameQuality::EDITOR`) keep it off, so
their goldens hold. It needs the depth prepass and a single-sample frame (no MSAA).

The bent normal has the AO texture's resolution: half resolution on Desktop High, and on every
frame lit by a baked volume (the baked-GI policy halves Ultra's AO there); full resolution on
Ultra otherwise. The forward pass reads it with the AO's nearest sampler, after the AO pass's
4×4 depth-aware blur.

The terrain keeps its own sky and probe lookups (its heightfield horizon already occludes its sky);
only meshes drawn by the forward pass use bent normals.

## Toggle and compare

`render.set_bent_normals { "enabled": false }` returns
`{ "before": { "enabled", "active" }, "after": { … } }`. `active` is false when the profile has no
GTAO row of SSAO, no depth prepass, or uses MSAA, even with `enabled: true`.

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `render.set_bent_normals { "enabled": false }`, `play.step`, `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; set `enabled` back to `true`.

Look for: inside corners, the base of walls and crates, the ground under vehicles and hulls, and
the inner faces of trenches losing light that came through the occluder (on a baked map, the bright
fringe where probe light leaks into a crease). Open ground, roofs and walls in the open must not
change. A crease that gets brighter on a baked map, or a dark halo on an open surface beside a
foreground object, is a bug; report the screenshots.

From lookdev: `LOOK_R2_PROFILE=bent=0` (or the chase's `--off bent`) turns it off for before/after shots
of the DO27 plates.

Errors: `F0002` (invalid params, JSON-RPC `-32602`) when `enabled` is missing or not a boolean;
`F0003` (not found) when the app has no `RenderSettings` resource (add `regia_render`'s
`RenderPlugin`).

## Cost

Inside the SSAO dispatch: a few multiply-adds per kernel sample and a second texel write, and in
its blur sixteen more texel loads. The forward pass reads one more AO texel and evaluates two cap
intersections. `GpuTimings::ssao_ms` includes the kernel part; compare `scene_ms` with it on and
off for the forward part. Measured on an RTX 5090 at 2560×1440 (the courtyard bench
`bench_bent_normals_at_1440p`, three alternating rounds of 240 frames, DDGI on; the range covers two
runs):

| Preset | SSAO | Forward | Scene | Frame |
|---|---|---|---|---|
| Desktop High (half-resolution AO) | +0.005 ms (0.117 → 0.122) | +0.027 to +0.029 ms | +0.032 to +0.035 ms | +0.033 to +0.037 ms |
| Desktop Ultra (full-resolution AO) | +0.023 ms (0.491 → 0.513) | +0.023 to +0.025 ms | +0.046 to +0.055 ms | +0.047 to +0.059 ms |

## Design

ADR-1375. Sources: Landis, "Production-Ready Global Illumination" (SIGGRAPH 2002 course 16) for the
bent normal; Klehm, Ritschel, Eisemann, Seidel, "Bent Normals and Cones in Screen-space" (VMV 2011)
for computing it in screen space; Jimenez, Wu, Pesce, Jarabek, "Practical Realtime Strategies for
Accurate Indirect Occlusion" (SIGGRAPH 2016) § 5 for the visibility cone and the cone specular
occlusion; Oat and Sander, "Ambient Aperture Lighting" (I3D 2007) for the cap intersection. The CPU
mirror and its tests are in `crates/regia_render/src/ssao/bent_normal.rs`.
