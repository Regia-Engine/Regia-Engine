# Reflection occlusion (enclosed surfaces reflect the enclosure)

The sky and image-based light reach every surface's reflection unless something says they are
blocked. The AO-based specular occlusion only sees contact-scale geometry (the SSAO kernel
reaches about a meter and a tenth of the screen), so a cockpit seen through its canopy, or a room
seen through a window, mirrored the open sky: bluish streaks on surfaces that only see the
inside of the hull or the room. Reflection occlusion takes that part of the sky out.

The SSAO pass casts four short rays from each pixel through the depth buffer: the mirror
direction and three directions 30° around it. Each ray tests points out to 8 m (five on the
mirror ray, three on the others, closer together near the surface; a point seen against the sky
ends a ray as open); a point
hidden behind the depth buffer by a surface near enough to stand in its way blocks the ray (a
thin pole far in front of an open reflection does not). G of the AO texture holds the mirror
ray's result (glossy surfaces), B the four rays' (rough ones). The forward pass blends the two by
roughness and takes the smallest of three estimates as the specular visibility: the AO-based
term, this one, and the probe field's sky share. It applies once. The part of the reflection that
the enclosure blocks (beyond the contact-scale part, which stays dark) reflects the enclosure
instead: a mid-gray wall under the indirect light that reaches the surface. A surface in sun whose
mirror ray dips below the horizon keeps the environment, whose lower half already shows the
ground (a sunlit hull side over sand); in shadow (a cockpit inside its hull) the block stands. Very rough surfaces (perceptual
roughness above 0.6, fading out by 1) keep the AO-based term alone: their lobe is wider than the
rays. Glass and transmissive panes lie in front of the depth the SSAO rays saw. When
screen-space refraction is on (Desktop High, Ultra), the nearest pane traces its own mirror ray through
the refraction copy's opaque depth: sixteen steps out to 24 m, then a bisection. Where the ray
hits, the pane reflects that pixel's color from the copy: the hull beside a canopy, the cockpit
under its far pane, the ground at grazing angles. A ray that goes deep behind a nearby surface
(whose far side the screen does not show) is still blocked, as before. Panes behind the nearest one
(the far side of a canopy) keep that blocked-or-open test without a color. A miss (off screen, sky,
past the march) keeps the environment map. Before ADR-165 a hit only took the sky out, which turned a canopy over
Europa's ice black at grazing angles. Stochastic SSR still replaces the reflection of opaque
surfaces wherever its rays hit something on screen.

It runs on Desktop High and Ultra (`RenderProfile::reflection_occlusion`, with their GTAO rows of
SSAO). Desktop Low, Deck and mobile keep the AO-based specular occlusion alone; lab and editor
baselines (`FrameQuality::LAB`, `FrameQuality::EDITOR`) keep it off.

## Toggle and compare

`render.set_reflection_occlusion { "enabled": false }` returns
`{ "before": { "enabled", "active" }, "after": { … } }`. `active` is false when the profile has no
GTAO row, no depth prepass or uses MSAA, even with `enabled: true`.

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `render.set_reflection_occlusion { "enabled": false }`, `play.step`, `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; set `enabled` back to `true`.

Look for: cockpit walls, seats and the underside of canopy frames seen through the glass, and
floors and walls of rooms seen through windows, losing their blue sky sheen. Hull tops, roofs,
glass and metals in the open must not change. A dark halo on an exterior surface next to a
foreground object, or an open reflection that darkens, is a bug; report the screenshot.

## Cost

At most fourteen extra depth loads on half of the SSAO texels (a fixed checkerboard at half
resolution on Desktop High; the blur fills the rest), inside the SSAO dispatch. `GpuTimings::ssao_ms` includes it, but the forward pass waits on the
AO texture, so compare `scene_ms` with it on and off. Budget: ≤ 0.1 ms at 2560×1440 on Desktop High.

From lookdev: `LOOK_R2_PROFILE=reflocc=0` turns it off for before/after shots.
