# Mask recolor and decal slots

Per-instance recolor and decals on a mesh (`regia_render::mask_decals`, Tincal B7): one material
and one set of textures, many looks. DO27 uses it for faction colors, chest numbers, callsigns,
faction marks and visor tint. An entity without a `MaskDecals` component draws exactly as before.

- **Palette**: `primary`, `secondary`, `trim` multiply the base color where the instance's mask
  texture has R, G, B; `emissive` × `emissive_intensity` glows where it has A.
- **Decal slots** (up to 8): an atlas `cell` (or none: a solid rectangle) placed over `uv_rect`
  (`[min_u, min_v, max_u, max_v]` in the mesh's base UV set), rotated by `rotation` radians about
  its center, tinted by `color` (`[r, g, b, a]`, a = opacity), blended by `blend`:
  `alpha` (paint over; metallic goes to 0 under it), `multiply` (tints, visor tint) or
  `emissive` (adds light × `emissive_intensity`).

Colors are linear `[r, g, b]`, or `"#RRGGBB"` strings (sRGB, decoded). The mask and atlas textures
come from the game; the agent edits values only. An instance the agent creates has no textures, so
its palette does nothing and its decals draw as solid rectangles until the game supplies them.

## Read

`render.mask_decals { entity }` reads one instance:

```json
{ "entity": 4294967321 }
→ { "entity": 4294967321,
    "palette": { "primary": [0.6, 0.01, 0.03], "secondary": [0.02, 0.03, 0.35], "trim": [0.8, 0.6, 0.02],
                 "emissive": [1.0, 0.26, 0.0], "emissive_intensity": 2.0 },
    "decals": [ { "cell": 7, "uv_rect": [0.42, 0.30, 0.58, 0.46], "rotation": 0.0,
                  "color": [1.0, 1.0, 1.0, 1.0], "blend": "alpha", "emissive_intensity": 0.0 } ],
    "drawn_slots": 1, "mask": [1024, 1024], "atlas": [512, 512], "atlas_grid": [8, 8] }
```

Without `entity` it lists every masked instance and the renderer's counters for its last frame:

```json
{}
→ { "instances": [ … ],
    "renderer": { "instances": 12, "masked_draws": 12, "fallback_draws": 0, "unsupported_draws": 0,
                  "gpu_entries": 12, "blocks_created": 12, "block_writes": 1, "block_bytes": 464 } }
```

- `fallback_draws` > 0: the instances' shader variant was still building in the background
  (mid-play, first frames after they appeared); they draw with the plain material meanwhile.
- `unsupported_draws` > 0: the material is a material graph, transmissive, premultiplied or
  additive; those draw without the feature.
- `blocks_created` grows by one per new instance, never per frame. `block_writes` counts the
  instances whose values changed last frame.

## Edit

`render.set_mask_decals { entity, palette?, decals?, remove? }`. Omitted palette entries keep their
value; `decals` replaces the whole list; `remove: true` drops the component.

```json
{ "entity": 4294967321,
  "palette": { "primary": "#1E3A8A", "trim": [0.9, 0.9, 0.9] },
  "decals": [ { "cell": 3, "uv_rect": [0.42, 0.30, 0.58, 0.46] },
              { "uv_rect": [0.0, 0.0, 1.0, 1.0], "color": [0.55, 0.42, 0.1, 0.6], "blend": "multiply" } ] }
→ { "before": { … }, "after": { … } }
```

Errors: no `MeshRenderer` on the entity (`NOT_FOUND`), a color that is not three numbers or a hex
string, a slot without `uv_rect`, an unknown `blend`, more than 8 slots (`INVALID_PARAMS`).

Check the result with `seeing.screenshot`: each faction region takes its palette color over the
albedo's shading, decals sit where their rectangles are on the UV layout.
