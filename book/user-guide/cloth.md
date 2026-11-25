# Cloth and ropes

<!-- index: Cloth -->
<!-- index: Rope -->
<!-- index: Soft body -->
<!-- index: XPBD -->

`regia_physics::cloth` is a small, deterministic CPU solver for flags, banners, capes, nets and
ropes. It uses extended position-based dynamics (XPBD): particles joined by distance and bending
constraints, each with a compliance (inverse stiffness), stepped at a fixed rate with substeps.
Particles collide with spheres, capsules and infinite planes, with Coulomb friction. It does not
handle cloth against cloth (self-collision is opt-in, see below), and it does not talk to the
rigid-body world: give it the shapes it should drape over.

## A flag on a pole

```rust
use regia::core::math::Vec3;
use regia::physics::cloth::{
    ClothCollider, ClothConfig, ClothGrid, ClothPlugin, ClothSet, ClothShape, ClothSolver,
};

fn make_flag() -> Result<ClothSolver, regia::physics::cloth::ClothError> {
    let mut flag = ClothSolver::new(ClothConfig::default())?;
    let grid = flag.add_grid(&ClothGrid {
        origin: Vec3::new(0.0, 3.0, 0.0),
        u_axis: Vec3::X,              // along the flag
        v_axis: Vec3::NEG_Y,          // down the pole
        columns: 16,
        rows: 10,
        spacing: 0.05,                // meters between particles
        particle_mass: 0.005,         // kilograms per particle
        stretch_compliance: 0.0,      // inextensible
        shear_compliance: 1e-4,
        bend_compliance: 1e-2,        // soft folds
    })?;
    // Pin the column against the pole.
    for row in 0..grid.rows {
        if let Some(i) = grid.index(0, row) {
            flag.pin(i)?;
        }
    }
    // The pole itself, so the flag cannot swing through it.
    flag.add_collider(ClothCollider {
        shape: ClothShape::Capsule {
            a: Vec3::new(-0.03, 0.0, 0.0),
            b: Vec3::new(-0.03, 3.2, 0.0),
            radius: 0.025,
        },
        friction: 0.3,
    })?;
    Ok(flag)
}
```

Add `ClothPlugin` to the app and put solvers in its `ClothSet` resource under a name; the plugin
steps every solver once per `FixedUpdate`, after the rigid-body step:

```rust,ignore
app.add_plugins(ClothPlugin);
if let Ok(flag) = make_flag() {
    app.world_mut().resource_mut::<ClothSet>().insert("flag", flag);
}
```

Read positions back with `ClothSet::get("flag")` and `ClothSolver::particles()` to update a mesh
each frame. Move pinned particles with `ClothSolver::set_position` to attach cloth to an animated
character or a moving vehicle, and move colliders with `ClothSolver::set_collider`.

## Ropes

`ClothSolver::add_rope` lays `segments + 1` particles from `start` to `end`, links neighbors with
the stretch compliance and links each particle to the one after next with the bend compliance (a
rope that resists kinking). Pin one end or both:

```rust,ignore
let rope = solver.add_rope(&ClothRope {
    start: Vec3::new(0.0, 5.0, 0.0),
    end: Vec3::new(2.0, 5.0, 0.0),
    segments: 20,
    particle_mass: 0.05,
    stretch_compliance: 0.0,
    bend_compliance: 1.0,
})?;
solver.pin(rope.first)?;
```

## Building your own topology

`add_particle(position, inverse_mass)` returns an index; `add_distance(a, b, compliance)` and
`add_bend(a, b, c, d, compliance)` take rest lengths and rest angles from the current positions.
`add_bend` is a hinge on the edge `a`–`b` between the triangles `(a, b, c)` and `(a, b, d)`. An
inverse mass of 0 pins a particle; a solver whose particles are all pinned never moves.

## Settings

| `ClothConfig` field | Default | Meaning |
|---|---|---|
| `gravity` | `(0, -9.81, 0)` | Acceleration on free particles (m/s²). |
| `time_step` | 1/60 s | Length of one fixed step. |
| `substeps` | 8 | Substeps per fixed step (up to 256). More substeps make stiff cloth converge better. |
| `iterations` | 1 | Constraint passes per substep (up to 64). |
| `damping` | 0.1 /s | Velocity damping. |
| `thickness` | 0.01 m | How far particles stay outside every collider. |
| `max_steps_per_call` | 4 | Fixed steps one `step(dt)` call may run; longer frames drop the excess time. |

Compliance is in meters per newton for distance links and radians per newton-meter for hinges.
`0` is rigid. Because XPBD scales compliance by the substep length, a cloth keeps the same
stiffness when you change `substeps` or `iterations`; only accuracy changes.

## Determinism and errors

The same build on the same platform with the same calls gives bit-identical particles;
`ClothSolver::state_digest` folds them into one value for comparisons. Every call validates its
input: NaN or infinite numbers, negative masses or compliances, out-of-range indices, repeated
particles and degenerate shapes are rejected with a typed `ClothError` and change nothing.

Agents read the solvers through `physics.cloth_stats` ([cloth (agents)](../agents/cloth.md)).

## Cost

The solver is single-threaded and allocation-free while stepping. Work per fixed step grows with
`substeps × iterations × (constraints + particles × colliders)`; a 16 × 10 flag with one collider
is a few thousand constraint updates per step.

## Skinned cloth, wind, self-collision and soft bodies

Spec 55 (`docs/specs/55-cloth-and-soft-body.md`, ADR-1672) adds to the same solver:

- **Pose binding.** Give each particle a pose target (where it would be if it were only skinned) with
  `set_pose_targets(prev, cur)` once per fixed step, and bind with `set_pose_maps`. `reach` (meters) keeps a
  particle inside a sphere around its target (0 = follow exactly), `standoff` keeps it on the outside of the
  target's normal plane, `pose_pull` adds a soft spring toward the target, `sim_mix` mixes pose and simulation in
  the output, `mass_scale` scales mass. `regia_animation::cpu_skin::skin_points` computes targets from a skin
  palette. `set_blend` (0 = pose, 1 = simulation) and `output` give the positions and normals to draw.
  `set_root_motion` lets a sprint not stretch a cape (`inertia_scale` 0 = move rigidly with the root) and resets
  the cloth after a teleport (`teleport_distance`, 2 m).
- **Tuning** (`ClothTuning`): gravity scale, strain caps, tethers (keep particles within a scale of their rest path
  length from the pins: cheap inextensibility), self-collision, wind model, tearing.
- **Wind.** `set_wind`, `set_gust` (deterministic gusts), `ClothSet::set_global_wind` and `WindField` components
  (directional, radial, vortex) that the plugin samples each fixed step. `Aerodynamic` wind acts on triangles
  (drag and lift); `Push` accelerates every particle and also moves ropes.
- **Self-collision** `SelfCollision::Spheres { radius }` keeps particles `2 × radius` apart (layers of a folded
  sheet stop passing through each other); `SelfCollision::VertexTriangle { radius }` keeps particles `radius` away
  from nearby sim triangles (ADR-2425).
- **Tearing** cuts over-stretched links (`TearParams`); `drain_torn` lists them.
- **Moving colliders.** Call `set_collider_previous` with last step's shape so a moving collider is swept across
  the substeps and drags cloth with its velocity.
- **World snapshots.** `set_snapshot(WorldSnapshot::new(shapes)?)` gives the solver rigid shapes to collide with:
  spheres, capsules, boxes, convex hulls, planes and triangle lists (ground, up to 4,096 triangles), each with a
  previous and current pose. The solver reads only the snapshot, never the rigid world.
- **States and LOD.** A bound cloth settles for 8 steps and ramps its blend in over 30. `set_paused` stops
  stepping; `set_lod_level` cheapens far cloth (LOD 1 and 2) or fades it to the pose (off).
- **Soft bodies.** `SoftSolver` with a `Pressure` model (balls: holds `pressure × rest volume`), a `Tetra` model
  (per-tet volume) or `ShapeMatch` clusters (jelly). Put them in the `SoftSet` resource to have the plugin step them.

## Cloth assets on a character

Author a cloth as data instead of code (ADR-2083, `docs/formats/cloth-ron.md`):

1. Model a coarse **sim mesh** (a few hundred vertices for a cape) and, optionally, keep the visible cape as one
   primitive of the character's render mesh. Skin the sim mesh to the same skeleton.
2. Write `cape.cloth.ron`: the sim mesh node, the render mesh node and primitive (`section`), parameters, and
   vertex maps. `reach: 0` on the shoulder row pins it to the body; a few centimeters of `reach` and a small
   `standoff` elsewhere keep the cape near and outside the body.

   ```ron
   (
     format_version: 1,
     name: "cape",
     skeleton: Some("hero.glb"),
     sim_mesh: (source: "cape_sim.glb", node: "Cape_Sim"),
     render_mesh: Some((source: "hero.glb", node: "Hero", section: 2)),
     params: (substeps: 8, damping: 0.2),
     maps: {
       "reach": Sparse(default: 0.3, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0)]),
       "standoff": Sparse(default: 0.01, entries: []),
     },
     proxies: ["spine_cap", "thigh_l_cap", "thigh_r_cap"],
   )
   ```
3. Put it under `assets/` next to the meshes and run `regia cook`. It cooks to a `.cloth.bin` like any other asset
   (the same source always gives the same bytes), and a change to a glTF it names cooks it again.
4. Cloth is opt-in, like physics: `DefaultPlugins` does not add it, so your game adds `ClothGluePlugin` once, next to
   `PhysicsPlugin` (play-in-editor gets it through your game's plugin). Without it a level still opens: the
   `ClothSimSource` is skipped with a logged warning and nothing simulates. A dedicated server build has no glue, so it skips
   saved cloth the same way, and `reaction` does not push bodies the server owns.

   In a level or prefab, save a `ClothSimSource` on an entity under the character. The glue loads the cooked asset,
   finds the skinned owner among the entity's ancestors, draws the section on the entity (give it a `MeshSource` with
   the cape's material) and follows hot reloads:

   ```ron
   "regia_framework::cloth::ClothSimSource": (
     asset: "assets/hero/cape.cloth.ron",
     layer: "default",
     solver: "",
     blend: 1.0,
     lod_distances: (15.0, 30.0, 60.0),
     reaction: false,
   ),
   ```

   From code, load it with `load_cloth_asset(&mut server, "assets/hero/cape.cloth.ron")` (works from paks and the
   cooked cache) and spawn the instance yourself. A failed load is logged and left in `ClothSourceState.error`.

   ```rust,ignore
   use regia::framework::cloth::{ClothBodyProxy, ClothGluePlugin, ClothSim, ProxyShape, load_cloth_asset};

   app.add_plugins(ClothGluePlugin);
   let section = commands.spawn((MeshRenderer::new(empty_mesh, cape_material), Transform::default())).id();
   let mut cape = ClothSim::new(load_cloth_asset(&mut server, "assets/hero/cape.cloth.ron")?);
   cape.skinned_mesh = Some(hero);
   cape.section = Some(section);
   commands.spawn((cape, Transform::default()));
   // On the spine joint entity:
   commands.entity(spine).insert(ClothBodyProxy {
       name: "spine_cap".into(),
       shape: ProxyShape::Capsule { a: Vec3::ZERO, b: Vec3::Y * 0.4, radius: 0.15 },
       friction: 0.3,
   });
   ```

The glue then skins the sim mesh every fixed step, collides the cape with the proxies and with rigid colliders on
its `layer` near it (a world snapshot), pauses it while hidden, lowers its LOD with camera distance, and draws the
render section by publishing a new mesh to the section entity each frame. `ClothSim.blend` fades between the
skinned pose (0) and the simulation (1). With `reaction: true` the cloth pushes dynamic bodies it rests on (capped
at `max_reaction_impulse`, 5 N·s per body per step), including bodies whose collider sits on a child entity. On
Android a cooked cloth or soft body runs at most 4 substeps whatever its `substeps` says (a `ClothSim.params` you set
in code is used as given). The instance reports `ClothStateChanged`, `ClothTeleported`
and `ClothTear` messages, each naming its entity.

A flag or banner needs no skeleton: leave `skinned_mesh` empty and the pinned (`reach: 0`) vertices follow the
entity's transform. Soft props work the same way with a `.soft.ron` and `SoftSim`: a `reach` or `pose_pull` map
attaches the body to its entity, or to a skeleton through `SoftSim.skinned_mesh` when the surface glTF is skinned.
With neither map the body is free, and a free body that was hidden or far away carries on from where it stopped.
A mirrored root (a negative scale, such as the left glove made by flipping the right one) works for both: the cloth
keeps its front side outward, and a soft body keeps its volume.

Vertex maps can be edited without the editor: `regia_physics::cloth::maps::apply_map_edit` (or the agent's
`physics.cloth_edit_map`) applies `Set`, `Sphere`, `Relax`, `Ramp`, `Flood` and `FromVertexColor` edits, and
`store_maps` writes them back into the source.

An asset's `lods` entry gives a far cloth a coarser sim mesh: past the second LOD distance it moves onto that mesh,
and back when the camera returns, keeping its motion. Tears do not carry over.

Garment layers and reaction torque (ADR-2425): a cooked asset's `layer` map puts its particles on layers
(`ClothSolver::set_layers`; inner layers push outer layers, never the reverse), and with `reaction: true` the body
under the cloth also receives the torque of the contact impulses about its center of mass.

## Scene wind and the Cloth panel

Cloth placed with `ClothSim` (or `ClothSimSource` in a level) moves with the scene wind: when the scene has a
`SceneWind` (driven by the weather, or set by hand), each fixed step adds its velocity to the cloth's wind. A cloth's
`wind_scale` parameter (0 to 10, default 1) says how much of it the cloth feels; set it to 0 for a cloth that should
ignore the weather:

```ron
params: (substeps: 8, wind_scale: 0.5),
```

Without a `SceneWind`, or with the wind off or calm, cloth behaves exactly as before.

In the editor, the **Cloth** tab (in the bottom tab group of the default layout) is the Cloth panel. Open a `.cloth.ron` (or `.soft.ron`) from the project to edit
its parameters, pick a starting point, set self-collision and the wind model, and, with a cloth entity selected, its
blend and LOD distances. Every change can be undone; **Save** writes the file. Tick **Paint** to paint a vertex map
(`reach`, `standoff`, ...) on the sim mesh with the brush in the viewport: the vertices show the map on a color ramp in
meters, Ctrl-click selects a vertex, and **Gizmos** draws the body proxies and the selected vertices' `reach` spheres
and `standoff` planes. **Simulate preview** runs the cloth in the panel without touching the level. Agents use the same
panel through `editor.cloth_panel_*` ([Cloth and ropes (agents)](../agents/cloth.md)).
