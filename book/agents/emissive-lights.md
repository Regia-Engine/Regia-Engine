# Emissive lights

Emissive trim (unit lights, building and window lights, strips under a hull) lights the surfaces
right next to it. The renderer fits a few area lights to every emissive mesh and shades them in
the clustered light loop, so a strip lights the hull beside it and throws a tight pool on the
ground, the way the path tracer (and Cycles) show it. The probes and the screen-space GI only
carry a blurred share of that light; the fitted lights carry it instead, and the screen-space GI
skips the emitters' pixels so the light is not counted twice.

## What gets a light

A mesh whose material emits (`emissive` × `emissive_intensity` > 0), is lit (not `unlit`), opaque
or masked (not blended or additive), has no material graph and is rigid (no joint palette). The
fitter splits each connected emissive part into:

- **tubes** for rods and bars (thin in two directions), lit like a glowing cylinder from every side;
- **quads** (rectangles or trapezoids) for the faces of plates and rings: a closed mesh emits from
  its outside only, an open sheet of a double-sided material from both faces;
- smaller pieces when a part is curved or a quad would cover its triangles poorly.

Each light keeps the flux of the triangles it replaces. An emissive texture masks and scales the
triangles (dark texels do not emit). A one-sided face never lights what is behind it.

## Shadows: distance probes

The first time a mesh is drawn, each of its lights traces a small distance probe on the CPU
(16 × 16 directions from ten points on a quad, or five on a tube — its centre and four points
along the axis — against the scene's triangles near it): how far the light reaches in every
direction before it hits something. The forward pass
compares each receiver's distance with it, so a light on top of a hull does not light the ground
under the hull. Rays stopped right at the emitter (a frame or grille pressed against a pad) scale
the light down by that share; a face resting on other geometry is dropped (`buried`). Probes are
not re-traced when nearby geometry moves; a changed emissive color, strength or map refits.

## Cost: cached by content, new work off the frame thread (v0.1.18)

A fit is cached by the mesh's content (vertex positions, indices, the texture coordinates when the
material has an emissive map, and the material's emission), not by the `Arc` that holds it. A mesh
landed again under a new `Arc` with the same content (a damage step that re-lands the intact mesh)
reuses the fit, and on the same object (entity) its probes too: nothing is fitted or traced. Two
different objects with equal meshes share the fit and trace their own probes.

After the first frame, the fit and the probe trace of a genuinely new mesh run on a background
thread. The frame thread only hashes the mesh, copies what the fit reads and gathers the
triangles around the lights, and starts no new piece once the collection has spent 1 ms
(`EMISSIVE_FRAME_BUDGET_MS`). Until the lights land (usually a few frames), a mesh that replaced
another on the same object draws with the old mesh's lights; a mesh new to its object draws without
its lights. The first frame does everything at once, as before, so a screenshot of a freshly
loaded scene has every light. `REGIA_EMISSIVE_SYNC=1` keeps everything on the frame thread
(v0.1.17 behavior) for A/B checks.

The runtime lends each frame's emissive meshes to the renderer
(`FrameRenderer::hold_mesh_identities`), so a mesh key cannot name another mesh while its fit is
cached. A host that builds `FrameScene`s itself may skip that; the cache then checks each key's
buffers and a sampled fingerprint every frame.

## Emissive map signatures (v0.5.1, ADR-7869)

Runtime materials keep their exact content signature across unchanged frames. The cache is keyed
by the registered material generation and dynamic texture write serials; a new generation or a
successful dynamic write hashes the map once, shared by every mesh using that material. A steady
frame hashes zero map bytes. The counted unit test
`tracked_emissive_maps_hash_no_bytes_in_a_steady_frame` verifies this without timing assertions.
Hosts building `FrameScene`s can use the existing `shading_model::material_key(&Arc<CpuMaterial>)`
for the material they borrow into each `FrameMesh`; its registered ownership enables the cache.

A host supplying borrowed `LabMaterial`s directly still gets an exact full hash: their public
`LabTexture.rgba` vectors can change in place without a write epoch. That compatibility path keeps
its existing cost. `material_signature` itself also remains an exact content function, so moving
identical bytes does not change its result. Dynamic image overrides retain the fitter's existing
behavior; this change caches its `emissive_map` signature, not a new texture sampling path.

The existing `render.emissive_lights` readout remains the agent surface: unchanged content should
have zero `refits`, and changing the emission should update the fitted lights. `fit_cache.hashes`
counts geometry hashes, not map bytes; use the counted test for the zero-byte work assertion.

## Budget per quality tier

`RenderProfile::emissive_lights` / `FrameQuality::emissive_lights`: Desktop High 96, Ultra 160;
Desktop Low, Deck and both Mobile tiers 0 (off). Lights whose range does not reach the view are
culled; over the budget the brightest on screen are kept. A light's range ends where it would add
4 % of middle gray to a white surface at the frame's exposure.

## Under a baked GI volume

A baked volume (`.fgiv` v3) already holds the light of the emitters it traced. The lights of those
emitters are tagged: the forward pass leaves their light to the volume where it covers, and over
the budget they go after the other lights. Emitters the bake did not trace (added or moved after
the bake, units' lights) keep their area lights everywhere. See
[Baked GI volumes](baked-gi.md#emitters-the-bake-holds) (ADR-1386).

## Checking it

`render.emissive_lights` reports the last frame:

```json
{ "enabled": true, "budget": 96, "meshes": 5, "fitted": 59, "in_view": 59,
  "uploaded": 59, "dropped": 0, "refits": 0, "cpu_ms": 0.03,
  "probes_traced": 0, "probes_deferred": 0, "buried": 0, "baked": 0,
  "fit_cache": { "fits_done": 5, "fits_deferred": 1, "cache_hits": 40, "probe_hits": 38,
                 "probes_traced": 61, "budget_holds": 0, "stand_ins": 3, "hashes": 45,
                 "frame_ms": 12.4, "worker_ms": 31.0, "pending": 0, "background": true,
                 "frame_budget_ms": 1.0 } }
```

`refits` is the number of mesh keys that took new content this frame (a new mesh, or a changed
emissive color, strength or map); most are served from the content cache. `fit_cache` holds
counters since the renderer started: `fits_done` (fits computed), `fits_deferred` (sent to the
background thread), `cache_hits` and `probe_hits` (new keys that reused a fit or probes),
`budget_holds` (mesh-frames whose new work waited for the frame budget), `stand_ins`
(mesh-frames drawn with the replaced mesh's lights), and the frame-thread and background
milliseconds. `cpu_ms` above `frame_budget_ms` past the first frame is a hitch worth reporting
(with `fit_cache`). `dropped` > 0 means the budget cut lights that reach the view. `baked`
counts the uploaded lights whose emitter the baked GI volume holds (0 without a v3 volume).
The 10 s telemetry summary carries the same counters per window under `emissive`.
To see the effect, take a `seeing.screenshot` with the quality's `emissive_lights` at 0 and at its
default and compare the ground and the surfaces next to the emitters.

The lookdev tool `emissive_bounce` measures it against the path tracer on a DO27 hero reference
(medians of the emitters' light in rings around them, on the ground and on the asset).
