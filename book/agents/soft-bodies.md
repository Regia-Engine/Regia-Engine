# Soft bodies (agents)

Soft bodies are cloth particle stores with a volume model (`regia_physics::cloth::SoftSolver`,
`docs/specs/55-cloth-and-soft-body.md` § 2.8). `ClothPlugin` keeps them in a `SoftSet` resource and steps them in
`FixedUpdate` after the cloth solvers. They have no methods of their own: the [cloth](cloth.md) methods take a soft
body's name as `target`.

| Model | Input | What it keeps |
|---|---|---|
| `pressure` | A closed, outward-wound triangle surface | The enclosed volume at `pressure` times the rest volume (`pressure` live, clamped to 0.05..8) |
| `tetra` | Tetrahedra with positive rest volume | Each tetrahedron's rest volume; inverted tets at runtime are allowed and counted |
| `shape_match` | Clusters of at least 4 non-coplanar particles | Each cluster's rest shape, rotated by the best-fit rotation (or blended with the linear fit by `beta`) |

- `physics.cloth_inspect` lists soft bodies with `kind: "soft"` and adds `model`, `volume`, `rest_volume`,
  `pressure` (pressure model, else `null`) and `inverted_tets` (tetra model, else 0).
- `physics.cloth_set_params` takes `pressure` for a pressure soft body (`cloth_bad_param` for any other target).
  Finite pressure is clamped to 0.05..8; `params.pressure` reports the stored value. A rejected patch changes no
  settings, including when it names a non-pressure soft body.
- `physics.cloth_pin`, `cloth_unpin`, `cloth_wind`, `cloth_step` and `cloth_reset` work on soft bodies as on cloth.

## Checks agents run

- **A ball holds its pressure:** `physics.cloth_set_params { patch: { pressure: 1.5 } }`, `physics.cloth_step 600`,
  `physics.cloth_inspect`: `volume / rest_volume` within a few percent of 1.5.
- **A tet body is not inverted:** `inverted_tets` is 0 once it is at rest.
- **It is deterministic:** `physics.cloth_reset`, `physics.cloth_step N`, compare `digest` across runs.

## Errors

Building a soft body in game code can fail with `soft_not_closed`, `soft_inverted_rest`, `soft_bad_cluster` or
`cloth_bad_param` (a surface wound inside out, a stiffness or beta outside `[0, 1]`, a non-positive mass); see the
[cloth](cloth.md) page for the full list. The agent methods answer with the reasons listed there.

## Soft-body assets

A `.soft.ron` (`docs/formats/cloth-ron.md`) names a closed surface in a glTF file and a model: `Pressure(pressure,
volume_compliance)`, `Tetra(tets | cell, volume_compliance)` or `ShapeMatch(clusters, stiffness, beta)`. The
desktop cooker (`regia_asset_pipeline::cloth_cook::cook_soft`) writes a `.soft.bin`; a `Tetra` model without listed
tets runs the voxel tet generator (`regia_asset_pipeline::tet_gen::generate_tets`) at `cell` meters and reports the
tet count and the smallest-over-largest tet volume ratio. An open surface fails with `soft_not_closed`.

A `regia_framework::cloth::SoftSim { asset, section, .. }` on an entity gets a solver named `soft:<entity bits>`; the
cloth methods take `{ entity }` of that entity as their `target`. Its surface is published to the `section`
entity's `MeshRenderer` every frame (tet bodies through their surface binding).

The entity takes body proxies (a skinned body only its own character's), pauses while hidden and follows
`lod_distances`; cloth and soft bodies share `ClothBudget`, so `lod` in `physics.cloth_inspect` can drop because a
nearer cloth needs the budget. A body with a `reach` or `pose_pull` map attaches to a pose: `SoftSim.skinned_mesh`
when its surface has a skin, else the entity's transform. A free body has no pose: after a long hide or LOD off it
wakes where it stopped (`state` goes to `settling`), and only `physics.cloth_reset` returns it to its rest shape.

The shared cloth methods and their parameter shapes, defaults, bounds and errors are documented in
[Cloth and ropes](cloth.md); this page adds no separate soft-body protocol method. Workflow: inspect
the named soft body, apply a model-appropriate `physics.cloth_set_params` patch, step a fixed number of
ticks, and inspect `volume`/`inverted_tets` and the deterministic `digest` after reset and replay.

The shared cloth methods also return `cloth_plugin_missing` (`F0003`) when `ClothPlugin` is absent.
Captured failing cloth call (lavapipe; plugin absent):

```console
$ regia agent call physics.cloth_set_params '{"target":{"name":"missing"},"patch":[]}' --project samples/arena
error: method call failed: this app has no cloth plugin (-32001): {"code":"F0003","hint":"add regia_physics::cloth::ClothPlugin and call install_cloth_methods","reason":"cloth_plugin_missing"}
```

## Plasticity

A `shape_match` soft body can be plastic (ADR-9070): once a cluster is stretched or squashed past `yield`, its rest
shape creeps toward the current shape and the body keeps the deformation after it is let go. An elastic body (the
default) springs back.

- `physics.cloth_set_params { target, patch: { plasticity: { yield, creep, max } } }` turns it on; `plasticity: null`
  turns it off (the creep stops; the rest shape keeps what it already took). Defaults for omitted fields: `yield` 0.1,
  `creep` 1, `max` 1. All three must be finite and at least 0 (`cloth_non_finite`, `cloth_negative`).
  - `yield`: the Frobenius norm of the cluster's stretch minus the identity, `‖S - I‖`, above which the rest shape
    creeps. A squash to half height at constant volume is about 0.77.
  - `creep`: the fraction of the excess stretch taken into the rest shape per second (per substep `h·creep`, at
    most 1).
  - `max`: the cap of the plastic strain `‖S_p - I‖`.
- Any other target (a cloth solver, a pressure or tetra soft body) refuses `plasticity` with `cloth_bad_param` and
  the patch changes nothing.
- `params.plasticity` echoes the setting (`null` when off) for every soft body; `physics.cloth_inspect` adds
  `plasticity` and `plastic_strain` (the largest `‖S_p - I‖` over the clusters; 0 for an elastic body).
- Plasticity is a runtime setting: the `.soft.ron` asset has no field for it. Game code calls
  `SoftSolver::set_plasticity(Some(Plasticity { yield_threshold, creep, max }))`; `SoftSolver::reset_plastic` gives
  the clusters their authored rest shape back.

**Check:** squash the body (for example by pinning its particles and moving them with game code), `physics.cloth_step
60`, `physics.cloth_unpin` every particle, `physics.cloth_step 240`, then `physics.cloth_inspect`: a plastic body's
`plastic_strain` is above 0 and its bounds stay squashed; an elastic body's `plastic_strain` is 0 and its bounds are back
to the rest size.
