# Near sun shadow (the first meters of the view)

The sun's cascaded shadow maps split the view on a ladder from 16 m to 4 km (ADR-061, ADR-140).
Cascade 0 covers the first 16 m, so on a 2048² map one of its texels is 1.5–2 cm wide. A hull flank
the sun grazes stretches each texel by 1/cos of the sun's angle to it. One to five meters from the
camera, a shadow edge on that flank then shows steps 4–10 cm long, which read as "garbled
letters" on the panels. A finer filter cannot remove them (the hard 2×2 compare shows the same
steps), and a 4096² cascade only halves them.

The near sun tile is a second sun view, orthographic like the cascades. It covers the first 4 m
of the view (`regia_render::NEAR_SUN_RANGE_M`) and is drawn into one square of the local
shadow atlas, the first square. On Desktop High it is 2048² with about 6 mm texels for a 60°
view; on Ultra it is 4096² with about 3 mm. Mesh receivers within that range sample it with
the cascades' receiver-plane PCSS filter. Over the last fifth of the range they blend back into
the cascades.

- **Casters.** The tile draws every sun caster whose footprint touches it and that reaches
  cascade 0 at all.
- **Terrain.** The terrain heightfield is not drawn into the atlas, so its shadow comes from
  cascade 0. Only blockers higher than the tile's topmost mesh surface over the receiver count
  there, plus 25 cm. A mesh part the tile already shadows with is never counted again at cascade
  0's coarser texels: no double shadow and no coarse fringe.
- **When it is drawn.** Nothing is drawn or sampled while no mesh lies within range of the eye.
  The tile is snapped to its own texel grid. With paged shadows it also covers a sphere a quarter wider than the 4 m slice and keeps its center until the slice's sphere leaves it, so a moving camera does not move it every frame.
  It is redrawn when its matrix moves or when cascade 0 has a dirty page.

## Toggle and compare

`render.set_near_sun_shadow { "tile": 0 }` turns it off. It returns
`{ "before": { "tile", "range_m", "active" }, "after": { … } }`. `tile` is the edge in texels
(256–8192); 0 is off.

1. Put the camera 1–3 m from a hull side that the sun grazes (sun high, vertical flank). Take a
   `seeing.screenshot` (target `"offscreen"`) and save it as `on.png`.
2. Run `render.set_near_sun_shadow { "tile": 0 }`, then `play.step`, then `seeing.screenshot`, and
   save it as `off.png`.
3. Run `seeing.compare_images` on the two. Set `tile` back to 2048 (High) or 4096 (Ultra).

**What to look for.** With the tile on, shadow edges on the flank (under intakes, ledges and
canopy frames) are straight and clean. With it off they show stair-steps. A character's feet keep
one contact shadow, not two. The far side of the 4 m range must show no seam.

**What is a bug.** A lit patch where cascade 0 shadowed the terrain over the mesh is a bug, and so
is a step at 3.2–4 m. Report the screenshot.

## Cost

When a mesh is in range, each pixel within 4 m pays a second filter: a 3×3 lookup of cascade 0
for the terrain on top of the tile's PCSS. The tile also needs one more atlas draw of the
casters over it whenever it is dirty. Measured numbers are in ADR-164.
