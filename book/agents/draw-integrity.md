# Draw integrity and skinned bounds

Two renderer health readings on one page (`FrameRenderer::draw_integrity`, W2.2):

- **Render-world entries.** Every draw of a frame refers to a mesh, an object and a material entry
  in the renderer's caches. From the start of draw collection to the last encoded pass each of
  those entries stays (removals are deferred to the frame's end, the eviction sweep only takes
  entries idle before the frame). If one is gone anyway, the object is rebuilt on demand, or the
  draw is skipped with a once-per-id `warn` (`draw skipped: its … entry is gone`) instead of
  panicking. The counters tell you whether that last resort ever ran.
- **Draws the GPU cull missed.** On the GPU-driven path, collection sends a rigid opaque draw with
  a cull bound to the instanced batches, and it gets no per-object uniform. If the GPU cull then
  does not take it (its mesh lost the bound after collection), the draw is drawn per object
  instead: the renderer builds its object entry after the cull, counts it and warns once per
  object (`opaque draw missed by the GPU cull`). This was the v0.1.13 `object entry` panic, seen
  when DO27 drew one pilot mesh rigid for a hand-built pilot and skinned for another pilot in the
  same frame. Later versions skipped the draw instead, so the pilot vanished. ADR-4577 fixed that
  cause (the cache keeps the bound); ADR-8028 makes any other cause draw the pilot.
- **Skinned bounds.** A skinned mesh (characters, mechs) is frustum-culled by a conservative bound
  computed from its joint palette: each joint's bind-pose box of the vertices it weights, moved by
  that joint's matrix, unioned. A pose that throws an arm far outside the bind pose keeps the mesh
  visible; a character behind the camera is culled from the color passes and still casts its
  shadow. A mesh whose weights cannot be bounded (negative weights, a weight sum off by more than
  1 %, joint indices past its palette) has no bound and always draws, as before batch v020.

## Read

`render.draw_integrity` (no parameters):

```json
{}
→ { "skipped_draws": 0, "skipped_draws_total": 0,
    "objects_rebuilt": 0, "objects_rebuilt_total": 0,
    "cull_missed_draws": 0, "cull_missed_draws_total": 0,
    "deferred_removals": 0, "deferred_removals_total": 3,
    "skinned": { "bounds": true, "bounded": 10, "unbounded": 0, "culled": 4, "refused": 0 },
    "healthy": true }
```

- `skipped_draws` / `objects_rebuilt` are the last main-view frame's; the `_total` values count
  since the renderer was created, texture views included. Both should stay 0. Anything else is a
  renderer bug: capture the log lines `draw skipped` / `object entry rebuilt` (they name the kind
  and key) and the steps that led there.
- `cull_missed_draws` counts the last main-view frame's opaque draws the GPU cull did not take
  after collection sent them to the instanced batches; `cull_missed_draws_total` counts since the
  renderer was created. They were drawn per object, so nothing vanished, but the count should
  stay 0. A non-zero count is a renderer bug: capture the `opaque draw missed by the GPU cull`
  log lines (they name the object and mesh keys) and what the host submits for that mesh.
- `deferred_removals` counts mesh entries whose removal waited for the frame's end (a morph
  instance's deformed copy dropped while an earlier pass window of the frame may still draw it).
  Non-zero is fine; an object drawn under two mesh keys keeps a copy per key and defers nothing.
- `skinned.bounded` / `unbounded` split the last frame's skinned draws; `culled` is how many of the
  bounded ones were outside the view. `unbounded` > 0 with `bounds: true` means some skinned mesh
  has weights the bound does not cover (they draw every frame; check the asset's weights).
- `skinned.refused` counts the last frame's skinned draws that weight a joint at or past their
  palette's length, or at or past the 256-joint cap (`MAX_JOINTS_PER_SKIN`, ADR-5418). Drawn, they
  would skin from another unit's joints, so no pass draws them (and they cast no shadow). It
  should be 0: a non-zero count means the palette the host submits is shorter than the skeleton
  the mesh was skinned to, or the skin has more joints than the cap (split it; the cooked path
  refuses such a skin at load with `CookedSkinError::TooManyJoints`). A palette longer than the
  cap whose vertices stay under it draws with its first 256 joints and is not refused.

Telemetry summaries carry the same reading: `draws.skipped` (window total) and
`draws.skinned_culled_mean` ([telemetry](../user-guide/telemetry.md)).

## Switch the skinned bounds

`render.set_skinned_bounds { enabled }` turns the skinned-mesh culling off (every skinned mesh
draws whatever the view, the pre-batch-v020 behavior) or back on. Use it to rule the bound out when a
character seems to vanish at a screen edge:

```json
{ "enabled": false }
→ { "before": { …, "skinned": { "bounds": true, … } }, "after": { …, "skinned": { "bounds": false, … } } }
```

The bound is conservative by construction, so a mesh that disappears with bounds on and stays with
them off is a bug worth reporting (the mesh, its pose, the camera). Hosts call
`FrameRenderer::set_skinned_bounds` directly.

The setter returns `F0002` when `enabled` is missing or is not a boolean, and `F0003` when there is
no runtime renderer.

Captured on lavapipe: calling `render.set_skinned_bounds` with `{}` fails with `F0002`. The CLI's
JSON `data` value is:

```json
{"code":"F0002","hint":"check the method's parameters in the agent guide"}
```

## Check it

Call `render.draw_integrity` after a long play session (DO27: several minutes with AI wingmen) and
after memory-pressure events: `healthy` must still be `true` (`skipped_draws_total`,
`objects_rebuilt_total` and `cull_missed_draws_total` 0).
