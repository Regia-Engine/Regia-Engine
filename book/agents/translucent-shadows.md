# Translucent shadows (glass, canopies, visors)

Alpha-blended and transmissive meshes cast a partial, tinted shadow instead of none. Beside each
sun cascade's opaque depth map the renderer keeps a coarser second map (half the cascade edge)
with the nearest translucent depth and the product of every translucent layer's RGB
transmittance; local-light shadows get the same pair at a quarter of the atlas edge. A surface
behind a translucent caster takes that light:

- a blended surface (`BLEND`, `PREMULTIPLIED`) lets through `1 − alpha`, untinted;
- a transmissive surface (`transmission > 0`) lets through `transmission × (1 − F0) × base color ×
  volume tint` — tinted glass casts a tinted shadow;
- a damaged pane's holes cast nothing, its cracks and crazing scatter light out of the beam;
- additive meshes and meshes with `ShadowCast::Off` cast nothing.

DO27's canopy glass (blended, alpha 0.9) therefore lets 10 % of the sun into the cockpit, as in
the Cycles references. Your own canopy still shadows the cockpit; only its refraction is off.
Opaque shadows are unchanged, and a frame without a translucent caster renders exactly as it did
without the feature. Glass is not tinted by other glass, and terrain and volumetric fog ignore
translucent casters.

## Toggle and compare

`render.set_translucent_shadows { "cascades": 0 }` turns the sun part off,
`{ "cascades": 4, "local": true }` back on (Desktop High). It returns
`{ "before": { "cascades", "local", "active_cascades" }, "after": { … } }`; `active_cascades` is
capped by the profile's sun cascades.

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `render.set_translucent_shadows { "cascades": 0, "local": false }`, `play.step`,
   `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; restore the previous values.

Look for: cockpit and cabin interiors behind dark glass going dark, a soft tinted patch under
colored glass, holes in a shattered pane letting full sunlight through. Glass whose shadow is as
black as an opaque wall, or a bright interior behind near-black glass, is a bug; report the
screenshot.

## Tiers and cost

`RenderProfile::translucent_shadow_cascades`: Mobile Low 0, Mobile High 1, Desktop Low and Deck 2,
Desktop High and Ultra 4. `translucent_local_shadows`: on except both Mobile tiers. The maps are
allocated the first frame a translucent caster appears and redrawn each frame (glass moves with
its ship): two draws per caster per cascade. The shadow report (`shadow_cache_report`) counts
`translucent_casters`, `translucent_passes` and `translucent_draws`; the cost lands in the shadow
GPU bucket. From lookdev: `do27_chase --off translucent-shadows`, and
`HERO_NO_TRANSLUCENT_SHADOWS=1 hero_ref …`, render the before.
