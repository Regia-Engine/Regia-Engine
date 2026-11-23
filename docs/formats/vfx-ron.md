# .vfx.ron — Frozen VFX system v8

Spec: `docs/specs/26-terrain-vfx-highend.md` § 3.

## Overview

`.vfx.ron` is the asset format for GPU particle effects. Each file defines one named system (the event
router and scripts trigger it by name via `SpawnEffect`). The system groups one or more emitters with
optional per-system timing, and each emitter has a four-stage module stack: **Spawn**, **Init**,
**Update**, **Render**.

The format is RON (Rusty Object Notation) with `#[serde(deny_unknown_fields)]`. The current version
is **8**. A v1, v2, v3, v4, v5, v6 or v7 file still loads: the loader migrates it. v2 fields take their defaults on a
v1 file (emissive 1, unlit, self-shadow 0.65, no screen floor, flipbook `random_start` /
`blend_frames` off). v3 adds the `FireAnim` sprite; older files keep the sprites they named. v4 adds
the `FireballAnim` sprite and `RenderModule::heat_haze` (default 0, off). v5 adds the `Planar`
velocity mode (ADR-1398); v4 files load unchanged. v6 adds the scene wind (`wind_response`,
`wind_drag`), event spawns (`events`) and the `Event` spawn mode (ADR-2642); a v5 file loads with
`wind_response` 0 and no events, and simulates bit-identically. v7 adds the mesh-surface and
skinned-mesh shapes, the `Normal` velocity, `kill_volume` and the `Ribbon` render mode with
`RenderModule::ribbon` (ADR-2800); a v6 file loads with no kill volume and simulates
bit-identically. v8 adds `RenderModule::mesh`, the mesh asset a `Mesh` particle draws (ADR-7210);
a v7 file loads with no mesh and keeps the built-in cube. A
newer version is rejected before the body is parsed (`specs/12` § 4a). `regia
upgrade-project` registers `.vfx.ron` at `VFX_FORMAT_VERSION` and upgrades an older file by loading
it (the migrator chain) and re-saving it at the current version. The in-memory loader migrates old
files at load time. All `(min, max)`
ranges are inclusive. A range is always the explicit struct; a bare float is a parse error, so write
`(min: 0.4, max: 0.4)` for a fixed value. Every float field is checked finite — the set is closed
since v0.5.1 (ADR-7560), extending ADR-2077: RON spells `NaN` and `inf`, and a file using one
anywhere (a rate, `every`, any range including speeds, sphere radius and cone length, a curve key
time or value, `gravity`, `drag`, `curl`, `vortex`, `attractor`, `kill_below`,
`depth_collision.offset`, `count_scale`, `inherit`, `emissive`, `alpha_scale`, `softness`,
`stretch`, `heat_haze`, a flipbook `Fps`, a particle light, a ribbon width key or a LOD distance or
rate scale) is refused. `delay`, `duration`, `drag`, `count_scale`, LOD rate scales, `alpha_scale`,
`softness`, `stretch.velocity_scale`, `heat_haze`, flipbook `Fps` and particle-light
`intensity`/`radius` must also be at least 0. An event link whose worst-case child count
(`count` × the child's `count_scale` × the largest LOD rate scale × 4, the intensity cap) exceeds
the 24-bit packed count field (16 777 215) is refused (ADR-7560).

## Schema

```
VfxSystemDef {
    version: u32,                          // Must be 8 (v1 to v7 migrate)
    name: String,                          // Stable identifier (event router key)
    system: SystemModule,                  // Global budget + LOD
    emitters: [EmitterDef],                // ≥ 1; played together on trigger
}

SystemModule {
    budget: u32 = 4096,                    // Max live particles per system instance
    lod: Option<LodDef>,                   // Distance LOD bands
    bounds: Option<FixedBoundsDef> = None, // v8 fold: authored fixed bounds (culling); None = computed
}

FixedBoundsDef {                           // v8 fold (ADR-8105 row (h), P-225)
    min: (f32, f32, f32),                  // Local-space lower corner, metres from the instance origin
    max: (f32, f32, f32),                  // >= min on every axis; every value finite
}

LodDef {
    distances: [f32],                      // Strictly increasing band boundaries (m)
    rate_scale: [f32],                     // len = distances.len() + 1; fraction of spawn rate
}

EmitterDef {
    name: String,                          // Unique within system
    delay: f32 = 0.0,                      // Seconds after system trigger
    duration: Option<f32>,                 // How long spawning lasts; None = forever
    spawn: SpawnModule,
    init: InitModule,
    update: UpdateModule = default,
    render: RenderModule,
}

SpawnModule {
    mode: SpawnMode,
    lifetime: RangeDef,                    // Particle lifetime (s); min > 0, max ≥ min
    count_scale: f32 = 1.0,               // Multiplier (convenience)
}

SpawnMode = Rate(f32)                     // Particles per second
          | Burst(u32)                    // One-shot count
          | PerDistance(every: f32)       // One particle per `every` metres of travel
          | Event                         // v6: only when a parent's `events` names this emitter

InitModule {
    shape: ShapeDef = Point,
    velocity: VelocityDef = default,
    size: RangeDef,                        // Base size in metres
    rotation: RangeDef = (min: 0.0, max: 0.0),  // Radians
    spin: RangeDef = (min: 0.0, max: 0.0),      // Radians/s
    color: [ColorKey] = [],                // Gradient sampled at t=0
}

ShapeDef = Point
         | Sphere { radius: RangeDef, surface: bool = false }
         | Box { half_extents: (f32, f32, f32), surface: bool = false }
         | Cone { angle: f32, length: RangeDef }
         | MeshSurface { mesh: String }                                   // v7
         | SkinnedMesh { mesh: String, max_per_frame: u32 = 256 }         // v7, 1..=4096
// angle = half-angle in degrees, 0..89 (0 = beam along +Z); validate rejects anything else
// Mesh shapes sample the surface of a mesh the host registered under `mesh`
// (`VfxLibrary::add_mesh` / `add_cpu_mesh`): a triangle with probability proportional to its
// area (an alias table built at registration), then a uniform point on it. The mesh's object
// space is the instance frame; the spawn normal is the face normal. SkinnedMesh deforms the
// picked triangle with the instance's joint palette (`VfxInstance::set_skin_palette`, linear
// blend of the four strongest influences) on the CPU, at most `max_per_frame` spawns per frame;
// without a palette it samples the bind pose. An unregistered, empty or zero-area mesh (or a
// rigid mesh under SkinnedMesh) spawns nothing: the agent reports `missing_mesh` / `bad_mesh`.
// An `Event` child cannot use a mesh shape (its spawns start on the GPU).

VelocityDef {
    mode: VelocityMode,
    inherit: f32 = 0.0,                    // Fraction of instance world velocity inherited
}

VelocityMode = Radial { speed: RangeDef }  // Unrotated local offset (not the rotated spawn normal)
             | Directional { dir: (f32, f32, f32), spread: f32 = 0.0, speed: RangeDef }
             | Planar { speed: RangeDef, lift: RangeDef = (min: 0, max: 0) }  // v5
             | Normal { speed: RangeDef }                                     // v7
// Normal: along the spawn normal rotated into the instance frame (the face normal of a mesh
// shape, outward for a sphere or box, apex to point for a cone, local +Z for a point). Not on an
// `Event` child.
// dir is normalised when sampled (the loaded value keeps its authored length); spread = half-angle in degrees, 0..=180 (validate rejects others)
// Radial takes its direction from the spawn offset *before* the instance rotation (v1 meaning,
// kept: the offset's x is mirrored against the rotated position). Planar rotates the offset into
// the instance frame and keeps only its part perpendicular to local +Z: outward along the ground
// when +Z is the ground normal, plus `lift` m/s along +Z. A spawn on the axis gets a random
// direction in the plane. Use Planar for rings (a blast's dust sheet, a wall jet).

UpdateModule {
    gravity: (f32, f32, f32) = (0, 0, 0),  // m/s²
    drag: f32 = 0.0,
    curl: Option<(f32, f32, f32)>,          // (amount, frequency, speed)
    vortex: Option<VortexDef>,
    attractor: Option<AttractorDef>,
    size_over_life: [FloatKey] = [],
    color_over_life: [ColorKey] = [],
    alpha_over_life: [FloatKey] = [],
    rotation_over_life: [FloatKey] = [],
    kill_below: Option<f32>,                // World Y plane
    depth_collision: Option<DepthCollisionDef>,
    wind_response: f32 = 0.0,               // v6: 0..=1, how strongly the scene wind carries these
    wind_drag: f32 = 1.0,                   // v6: linear drag toward the wind per second at response 1
    events: [EventSpawnDef] = [],           // v6: at most 4 child spawns
    kill_volume: Option<KillVolumeDef>,     // v7
}

KillVolumeDef {                             // v7
    shape: KillShape,                       // Box { half_extents: (f32, f32, f32) } | Sphere { radius: f32 }
    center: (f32, f32, f32) = (0, 0, 0),    // In the instance frame, like the attractor
    kill: KillSide = Inside,                // Inside | Outside
}
// The volume turns and scales with the instance (a box along its axes); the boundary counts as
// inside. It is tested after each step's move, like `kill_below`, by the GPU update kernel, the
// CPU reference (`probe`) and the CPU ribbon simulation with one rule (`kill_volume.rs`); a kill
// fires the `Death` event. Extents must be finite and > 0.

EventSpawnDef {                             // v6
    on: EventTrigger,                       // Spawn | Death | Collision
    child: String,                          // An emitter of this system with spawn mode Event
    count: u32,                             // 1..=64 children per firing
    probability: f32 = 1.0,                 // 0..=1, all of `count` or nothing per firing
    inherit_velocity: f32 = 0.0,            // 0..=1 of the parent's velocity, added to the child's
}

VortexDef { axis: (f32, f32, f32), strength: f32, radial: f32 = 0.0 }
// `strength`: tangential acceleration (m/s²) around the axis through the instance origin, plus a
// built-in outward 0.25 × strength. `radial`: extra radial acceleration (m/s², inward when
// negative) added to that, acting even when `strength` is 0. Before ADR-5629 `radial` was loaded
// but ignored by the GPU kernel and the CPU ribbons.

AttractorDef { position: (f32, f32, f32), strength: f32 }

DepthCollisionDef { kill: bool, offset: f32 = 1.0 }

Depth collision acts on a particle that has just passed through the visible depth surface:
deeper than the surface by more than `offset`, but by less than `offset + 2 m + 2·|v|·dt`. A
particle hidden far behind nearer geometry (a ridge, a wall) is not snapped onto it. `kill:
false` projects the particle onto the surface and damps its velocity, so ballistic debris lands
and rests where it hit.

FloatKey { t: f32, v: f32 }                // t ∈ [0, 1], piecewise-linear

ColorKey { t: f32, color: String }         // sRGB hex "#rrggbb" or "#rrggbbaa"

RenderModule {
    mode: RenderMode = Billboard,
    sprite: SpriteId = SoftDisc,
    flipbook: Option<FlipbookDef>,
    blend: BlendMode = Additive,
    softness: f32 = 0.0,                   // World-metre soft-particle fade width
    alpha_scale: f32 = 1.0,
    stretch: StretchDef = default,
    light: Option<ParticleLightDef>,
    emissive: f32 = 1.0,                   // HDR scale. Lit puffs use it as an albedo scale
    lit: bool = false,                     // Spherical sun + ambient shade (smoke)
    self_shadow: f32 = 0.65,               // Top-lit gradient, 0 = off, 1 = black underneath
    screen_floor_px: f32 = 0.0,            // Minimum on-screen size; 0 uses the profile floor
    heat_haze: f32 = 0.0,                  // v4. Pixels of refraction; > 0 draws a haze card
                                           // (no colour) before all particles, where a scene
                                           // copy is bound (Desktop High HDR frames)
    ribbon: RibbonDef = default,           // v7, Ribbon only (see Ribbons below)
    mesh: Option<String> = None,           // v8, Mesh only: a registered mesh name; None = cube
}

RenderMode = Billboard | Stretched | Mesh | Ribbon

SpriteId = SoftDisc | HardDisc | Ring | Spark | Puff | Flare | Shard | PuffAnim | FireAnim
         | FireballAnim
// PuffAnim is the first tile of a 16-frame puff sheet (atlas tiles 8..24)
// FireAnim is the first tile of an 8-frame flame sheet (atlas tiles 24..32)
// FireballAnim (v4) is the first tile of an 8-frame fireball-body sheet (atlas tiles 32..40):
// a whole billowing fireball per frame with a baked temperature ramp (white-yellow core,
// orange body, sooty rim). Tint it with a neutral grey; the texture carries the hue

FlipbookDef {
    cols: u32, rows: u32, frames: u32,     // frames ≤ cols * rows, all ≥ 1
    mode: FlipbookMode,
    random_start: bool = false,            // phase in 0..1, stable for that particle
    blend_frames: bool = false,            // mix the current tile with the next
}

FlipbookMode = Fps(f32) | OverLife

BlendMode = Additive | Alpha

StretchDef { velocity_scale: f32 = 0.05, min_length: f32 = 0.02 }   // min_length < 0: × the particle's current size

ParticleLightDef { color: String, intensity: f32, radius: f32 }

RangeDef { min: f32, max: f32 }            // inclusive
```

## Wind and event spawns (v6)

**Wind.** With `c = wind_response * wind_drag` the update integrates
`dv/dt = g - drag * v + c * (wind - v)` by implicit Euler, per simulated step:
`v = (v + (g + c * wind) * dt) / (1 + (drag + c) * dt)`. The terminal velocity is
`(g + c * wind) / (drag + c)` at any step: without still-air `drag` or gravity a particle ends up
moving with the wind, and `wind_response` sets how fast it gets there. `drag` sets how much of the
wind it keeps. `wind` is `SceneWind` (`regia_scene::weather`), read every frame: calm where the
resource is missing or `Off`, and where the level has no air. At `wind_response` 0 the wind and
`wind_drag` are not read at all.

**Events.** A particle of an emitter with `events` fires:

- `Spawn` on its first simulated step;
- `Death` when it ends: old age, the `kill_below` plane or a killing depth collision;
- `Collision` when it goes through the depth buffer (`depth_collision` must be set). A sliding
  particle fires on every step it stays in contact; a killing one fires `Collision`, then `Death`.

On the next simulated frame each link with that trigger rolls `probability` once, and on success
spawns `count` particles of the `child` at the parent's position: the child's `init` shape,
velocity and ranges, in the instance's frame, plus `inherit_velocity` of the parent's velocity.
`count` is scaled by the child's `count_scale`, the LOD band and the instance's intensity, rounded
like a burst, and clamped to 16 777 215: the packed link field carries the count in 24 bits, where
2^24 would wrap to zero (the link would silently stop firing).

Rules, checked on load: a child must have spawn mode `Event` and at least one parent; an `Event`
emitter has no `delay`, `duration` or `light` (its position is not known on the CPU); at most 4
links per emitter; links must not form a cycle or nest more than 4 deep; and a link whose worst-case
scaled count (`count` × `count_scale` × the largest LOD rate scale × the intensity cap 4) exceeds
the packed field is refused (ADR-7560).

The rolls and the children's attributes are hashes of the parent's identity, the step, the trigger
and the link, the same on the GPU and the CPU reference (`regia_vfx::probe`), so a replay fires the
same events and counts are deterministic while nothing overflows. When an overflow does happen,
how many are lost is exact and *which* are lost is deterministic too: the first `event_capacity`
fired events survive, in slot order, wherever their slots sit in the window (a frame that fires
fewer events than the budget keeps all of them).

**Limits.** Events one frame can fire: `VfxProfile::event_capacity` (Desktop 8192, Mobile 1024).
Child slots: `VfxProfile::child_capacity` (Desktop 16384, Mobile 2048), a separate ring from
`particle_capacity`. Past them events and children are counted, never written out of bounds:
`events_overflow` and `children_dropped` in `vfx.systems` with `gpu: true`
(`VfxGpu::read_pool_stats`). System budgets (`system.budget`) count scheduled particles only;
children have the ring. An instance stays alive until its last possible child is dead.

```ron,vfx
(
    version: 6,
    name: "shell_impact",
    system: (budget: 256),
    emitters: [
        (
            name: "shell",
            spawn: (mode: Burst(12), lifetime: (min: 3.0, max: 3.5)),
            init: (
                velocity: (mode: Directional(dir: (0.0, 1.0, 0.0), spread: 35.0, speed: (min: 8.0, max: 14.0))),
                size: (min: 0.2, max: 0.3),
            ),
            update: (
                gravity: (0.0, -9.81, 0.0),
                depth_collision: Some((kill: true, offset: 0.05)),
                events: [
                    (on: Collision, child: "sparks", count: 12, inherit_velocity: 0.3),
                    (on: Death, child: "dust", count: 3, probability: 0.5),
                ],
            ),
            render: (mode: Billboard, sprite: Shard),
        ),
        (
            name: "sparks",
            spawn: (mode: Event, lifetime: (min: 0.2, max: 0.5)),
            init: (
                velocity: (mode: Radial(speed: (min: 2.0, max: 6.0))),
                shape: Sphere(radius: (min: 0.05, max: 0.1)),
                size: (min: 0.03, max: 0.06),
                color: [(t: 0.0, color: "#ffd9a0")],
            ),
            update: (gravity: (0.0, -9.81, 0.0)),
            render: (mode: Stretched, sprite: Spark, emissive: 4.0),
        ),
        (
            name: "dust",
            spawn: (mode: Event, lifetime: (min: 1.5, max: 2.5)),
            init: (
                velocity: (mode: Radial(speed: (min: 0.2, max: 0.8))),
                shape: Sphere(radius: (min: 0.1, max: 0.3)),
                size: (min: 0.4, max: 0.9),
                color: [(t: 0.0, color: "#9a9288")],
            ),
            update: (drag: 1.0, wind_response: 1.0, wind_drag: 2.0),
            render: (mode: Billboard, sprite: Puff, blend: Alpha, softness: 1.0, lit: true),
        ),
    ],
)
```

### Migrating to v7

Nothing to do: a v6 file means the same thing at v7 (`tests/v6_bit_identity.rs` pins it).
`regia upgrade-project --write` re-saves it with `version: 7`. A host that builds the structs in
code adds `kill_volume: None` to an `UpdateModule` literal and `ribbon: RibbonDef::default()` to a
`RenderModule` literal (or uses `..Default::default()`); an exhaustive `match` needs the new
`ShapeDef`, `VelocityMode` and `RenderMode` arms.

## Mesh particles (v8)

`mode: Mesh` draws each particle as an oriented mesh scaled by its size, rotated by its rotation
about a per-particle random axis, unlit, premultiplied over, depth-tested without depth writes and
sorted back to front with the other mesh particles. Without `mesh` it is the built-in unit cube
(edge = size), as in v7. With `mesh: Some("name")` it draws the mesh the host registered under
that name with `VfxLibrary::add_mesh` / `add_cpu_mesh` (the registration path of the
`MeshSurface` and `SkinnedMesh` shapes), in its own units times the particle size, through that
mesh's own vertex and index buffers (uploaded once, on first use). With `lit: true` a mesh asset is
shaded by the scene sun and ambient on its face normals (flat shading) and draws its front faces
only, counter-clockwise as glTF winds them (ADR-7261); without it (the default) the mesh is the
flat tint, both faces, as before. The built-in cube is never lit, whatever `lit` says. Each
registered mesh is one instanced draw over that mesh's own particles. A name with nothing registered
draws nothing and is reported as `missing_mesh`; a mesh the GPU refuses (more than 65 536
triangles, or more than 64 distinct render meshes at once) draws nothing and is reported as
`bad_mesh` (`VfxScene::render_mesh_issues`, the agent's `vfx.describe`). `mesh` with another mode,
or an empty name, fails validation.

```ron
render: (mode: Mesh, blend: Alpha, mesh: Some("meshes/shard.mesh")),
```

### Fixed bounds (v8 fold)

`system.bounds: Some((min: (x, y, z), max: (x, y, z)))` is an authored box in the system's local
space: metres from the instance origin, before the instance transform (its right, up and forward
axes and its scale, as the kill volume). When set, the box transformed by the instance is the
instance's culling volume (the scene significance ranking: in view or not, distance and projected
size) and the editor's bounds readout; without it the computed bound is used. It never kills or
clips particles (that is `kill_volume`). The loader refuses a non-finite value or `min > max` on an
axis. It is a fold into v8 (ADR-8105 row (h)): the version stays 8, the field is written only when
set, so a v8 file without it is unchanged. A v8 build from before the fold refuses a file that sets
it (an unknown field). A host that builds `SystemModule` in code adds `bounds: None`.

```ron
system: (budget: 512, bounds: Some((min: (-2.5, -0.5, -2.5), max: (2.5, 6.0, 2.5)))),
```

### Migrating to v8

Nothing to do: a v7 file means the same thing at v8 (a `Mesh` particle without `mesh` is the
cube). `regia upgrade-project --write` re-saves it with `version: 8`; the saved body has no `mesh`
field unless one is set. A host that builds the structs in code adds `mesh: None` to a
`RenderModule` literal (or uses `..Default::default()`).

## Ribbons (v7)

```
RenderMode = Billboard | Stretched | Mesh | Ribbon     // Ribbon: v7

RibbonDef {                                 // RenderModule.ribbon = default; read by Ribbon only
    link: RibbonLink = Emitter,             // Emitter: one strip per emitter of an instance,
                                            // SpawnGroup: one strip per frame's spawns
    max_points: u32 = 64,                   // 2..=1024 points per strip; the oldest go first
    width: [FloatKey] = [],                 // Width over length (t: 0 head .. 1 tail) x particle size
    color: [ColorKey] = [],                 // Colour over length, multiplies (key alpha too)
    texture: RibbonTexture = None,          // None | Stretch | Tile(metres > 0)
}
```

A ribbon emitter's particles live on the CPU: the scene moves its spawn jobs into a ring per
instance, steps it with the update kernel's model (gravity, drag, wind, vortex, attractor,
`kill_below`, `kill_volume`) and builds camera-facing triangle strips through the particles in
spawn order, newest at the head. A strip of `n >= 2` points draws `6 (n - 1)` vertices; the
scene stops at the trail-vertex cap (65 536) and counts what it left out. They draw through the
trail pipelines: `blend: Additive` emits, `Alpha` is premultiplied over. `Tile(m)` repeats the
sprite every `m` metres along the strip, `Stretch` runs it once; the cross-section comes from the
sprite's alpha (the fireball sheet is not available to ribbons). A ribbon cannot fire or be an
event spawn and cannot use curl noise, depth collision, heat haze or a light (validation says
why). `VfxScene::ribbon_vertices` holds the frame's strips; the runtime plugin adds them to
`VfxTrails`, a host drawing a scene itself passes them to `VfxGpu::write_trails`.

### Migrating to v6

Nothing to do: a v5 file means the same thing at v6. `regia upgrade-project --write` re-saves it
with `version: 6` (the new fields at their defaults) and the loader migrates it in memory
meanwhile. A host that builds `VfxSystemDef` in code uses `..UpdateModule::default()` already or
adds the three fields; an exhaustive `match` on `SpawnMode` needs an `Event` arm.

## Example — DO27 muzzle flash

```ron
(
    version: 1,
    name: "muzzle_flash",
    system: (budget: 256, lod: Some((distances: [20.0, 80.0], rate_scale: [1.0, 0.5, 0.0]))),
    emitters: [
        (
            name: "flash",
            delay: 0.0,
            spawn: (
                mode: Burst(24),
                lifetime: (min: 0.05, max: 0.12),
            ),
            init: (
                shape: Cone(angle: 12.0, length: (min: 0.2, max: 0.8)),
                velocity: (mode: Directional(
                    dir: (0.0, 0.0, -1.0),    // along the barrel
                    spread: 8.0,
                    speed: (min: 8.0, max: 20.0),
                )),
                size: (min: 0.1, max: 0.4),
                color: [(t: 0.0, color: "#fff2c8")],
            ),
            update: (drag: 6.0),
            render: (
                mode: Billboard,
                sprite: Flare,
                blend: Additive,
                stretch: (velocity_scale: 0.02),
            ),
        ),
        (
            name: "smoke_puff",
            delay: 0.02,
            spawn: (
                mode: Burst(12),
                lifetime: (min: 0.3, max: 0.7),
            ),
            init: (
                shape: Sphere(radius: (min: 0.05, max: 0.15)),
                velocity: (mode: Radial(speed: (min: 0.5, max: 2.0))),
                size: (min: 0.15, max: 0.4),
                color: [(t: 0.0, color: "#88888880")],
            ),
            update: (
                gravity: (0.0, 0.5, 0.0),
                drag: 2.0,
                size_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 3.0)],
                alpha_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Puff,
                blend: Alpha,
            ),
        ),
    ],
)
```

## Example — exhaust smoke

```ron
(
    version: 1,
    name: "exhaust",
    system: (budget: 4096),
    emitters: [
        (
            name: "smoke",
            spawn: (
                mode: Rate(80.0),
                lifetime: (min: 1.2, max: 2.5),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.15)),
                velocity: (mode: Directional(
                    dir: (0.0, 0.0, -1.0),
                    spread: 25.0,
                    speed: (min: 1.0, max: 6.0),
                ), inherit: 1.0),
                size: (min: 0.15, max: 0.5),
                color: [(t: 0.0, color: "#555555cc"), (t: 1.0, color: "#22222200")],
            ),
            update: (
                gravity: (0.0, 1.0, 0.0),
                drag: 1.5,
                curl: (1.5, 0.8, 0.3),
                size_over_life: [(t: 0.0, v: 0.3), (t: 1.0, v: 1.0)],
                alpha_over_life: [(t: 0.0, v: 0.8), (t: 0.5, v: 0.4), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Puff,
                blend: Alpha,
                softness: 0.3,
            ),
        ),
    ],
)
```

## Example — dust trail

```ron
(
    version: 1,
    name: "dust_trail",
    system: (budget: 2048),
    emitters: [
        (
            name: "dust",
            spawn: (
                mode: PerDistance(every: 0.3),
                lifetime: (min: 0.8, max: 2.0),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.3)),
                velocity: (mode: Radial(speed: (min: 0.2, max: 1.5))),
                size: (min: 0.2, max: 0.6),
                color: [(t: 0.0, color: "#99886680")],
            ),
            update: (
                gravity: (0.0, 0.2, 0.0),
                drag: 0.5,
                curl: (0.8, 0.5, 0.2),
                size_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 2.5)],
                alpha_over_life: [(t: 0.0, v: 0.6), (t: 1.0, v: 0.0)],
                kill_below: Some(-0.2),
            ),
            render: (
                mode: Billboard,
                sprite: Puff,
                blend: Alpha,
                softness: 0.2,
            ),
        ),
    ],
)
```

## Example — tracer

```ron
(
    version: 1,
    name: "tracer",
    system: (budget: 1024),
    emitters: [
        (
            name: "core",
            spawn: (
                mode: Rate(200.0),
                lifetime: (min: 0.15, max: 0.3),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.05)),
                velocity: (mode: Directional(
                    dir: (0.0, 0.0, -1.0),
                    spread: 2.0,
                    speed: (min: 20.0, max: 50.0),
                ), inherit: 1.0),
                size: (min: 0.08, max: 0.2),
                color: [(t: 0.0, color: "#ff6600")],
            ),
            update: (drag: 12.0),
            render: (
                mode: Stretched,
                sprite: Spark,
                blend: Additive,
                stretch: (velocity_scale: 0.04, min_length: 0.15),
            ),
        ),
        (
            name: "glow",
            spawn: (
                mode: Rate(30.0),
                lifetime: (min: 0.1, max: 0.2),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.05)),
                velocity: (mode: Directional(
                    dir: (0.0, 0.0, -1.0),
                    spread: 5.0,
                    speed: (min: 30.0, max: 60.0),
                ), inherit: 1.0),
                size: (min: 0.3, max: 0.6),
                color: [(t: 0.0, color: "#ffaa44")],
            ),
            update: (
                drag: 8.0,
                alpha_over_life: [(t: 0.0, v: 0.3), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: SoftDisc,
                blend: Additive,
            ),
        ),
    ],
)
```

## Example — impact (generic, metal-on-metal)

```ron
(
    version: 1,
    name: "impact",
    system: (budget: 512, lod: Some((distances: [30.0], rate_scale: [1.0, 0.3]))),
    emitters: [
        (
            name: "sparks",
            spawn: (
                mode: Burst(40),
                lifetime: (min: 0.2, max: 0.6),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.1)),
                velocity: (mode: Radial(speed: (min: 3.0, max: 15.0))),
                size: (min: 0.02, max: 0.08),
                color: [(t: 0.0, color: "#ffcc00"), (t: 0.5, color: "#ff6600"), (t: 1.0, color: "#330000")],
            ),
            update: (
                gravity: (0.0, -3.0, 0.0),
                drag: 1.0,
                color_over_life: [(t: 0.0, color: "#ffcc00"), (t: 0.5, color: "#ff6600"), (t: 1.0, color: "#330000")],
                alpha_over_life: [(t: 0.0, v: 1.0), (t: 0.7, v: 1.0), (t: 1.0, v: 0.0)],
                kill_below: Some(-0.5),
            ),
            render: (
                mode: Stretched,
                sprite: Spark,
                blend: Additive,
                stretch: (velocity_scale: 0.03, min_length: 0.03),
            ),
        ),
        (
            name: "flash",
            spawn: (
                mode: Burst(1),
                lifetime: (min: 0.05, max: 0.08),
            ),
            init: (
                shape: Point,
                velocity: (mode: Radial(speed: (min: 0.0, max: 0.0))),
                size: (min: 1.0, max: 1.5),
                color: [(t: 0.0, color: "#ffffff"), (t: 1.0, color: "#ffffff00")],
            ),
            update: (
                alpha_over_life: [(t: 0.0, v: 0.8), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Flare,
                blend: Additive,
            ),
        ),
    ],
)
```

## Example — explosion

```ron
(
    version: 1,
    name: "explosion",
    system: (budget: 4096, lod: Some((distances: [40.0, 120.0], rate_scale: [1.0, 0.5, 0.1]))),
    emitters: [
        (
            name: "flash_core",
            delay: 0.0,
            spawn: (
                mode: Burst(1),
                lifetime: (min: 0.1, max: 0.15),
            ),
            init: (
                shape: Point,
                velocity: (mode: Radial(speed: (min: 0.0, max: 0.0))),
                size: (min: 4.0, max: 6.0),
                color: [(t: 0.0, color: "#ffffff")],
            ),
            update: (
                size_over_life: [(t: 0.0, v: 0.2), (t: 1.0, v: 1.0)],
                alpha_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Flare,
                blend: Additive,
            ),
        ),
        (
            name: "smoke_puff",
            delay: 0.05,
            spawn: (
                mode: Burst(60),
                lifetime: (min: 1.0, max: 3.0),
            ),
            init: (
                shape: Sphere(radius: (min: 0.3, max: 1.5)),
                velocity: (mode: Radial(speed: (min: 2.0, max: 12.0))),
                size: (min: 0.5, max: 2.0),
                color: [(t: 0.0, color: "#555555cc"), (t: 1.0, color: "#22222200")],
            ),
            update: (
                gravity: (0.0, 2.0, 0.0),
                drag: 2.0,
                curl: (2.0, 0.5, 0.4),
                size_over_life: [(t: 0.0, v: 0.5), (t: 1.0, v: 1.5)],
                alpha_over_life: [(t: 0.0, v: 0.9), (t: 0.6, v: 0.5), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Puff,
                blend: Alpha,
                softness: 0.3,
            ),
        ),
        (
            name: "fireball",
            delay: 0.02,
            spawn: (
                mode: Burst(16),
                lifetime: (min: 0.3, max: 0.8),
            ),
            init: (
                shape: Sphere(radius: (min: 0.2, max: 1.0)),
                velocity: (mode: Radial(speed: (min: 3.0, max: 10.0))),
                size: (min: 0.3, max: 1.0),
                color: [(t: 0.0, color: "#ff8800"), (t: 1.0, color: "#ff000000")],
            ),
            update: (
                gravity: (0.0, 1.0, 0.0),
                drag: 3.0,
                alpha_over_life: [(t: 0.0, v: 1.0), (t: 0.4, v: 0.7), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: SoftDisc,
                blend: Additive,
            ),
        ),
        (
            name: "debris",
            delay: 0.03,
            spawn: (
                mode: Burst(30),
                lifetime: (min: 1.5, max: 4.0),
            ),
            init: (
                shape: Sphere(radius: (min: 0.1, max: 0.6)),
                velocity: (mode: Radial(speed: (min: 5.0, max: 25.0))),
                size: (min: 0.05, max: 0.3),
                rotation: (min: 0.0, max: 6.283),
                spin: (min: -10.0, max: 10.0),
                color: [(t: 0.0, color: "#888888")],
            ),
            update: (
                gravity: (0.0, -8.0, 0.0),
                drag: 0.5,
                kill_below: Some(-0.5),
            ),
            render: (
                mode: Mesh,
                sprite: Shard,
                blend: Alpha,
            ),
        ),
    ],
)
```

## Example — sparks (continuous, e.g. grinding)

```ron
(
    version: 1,
    name: "sparks",
    system: (budget: 2048),
    emitters: [
        (
            name: "sparks",
            spawn: (
                mode: Rate(60.0),
                lifetime: (min: 0.3, max: 1.0),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.1)),
                velocity: (mode: Directional(
                    dir: (0.0, 1.0, 0.0),
                    spread: 60.0,
                    speed: (min: 3.0, max: 12.0),
                )),
                size: (min: 0.02, max: 0.06),
                color: [(t: 0.0, color: "#ffcc00"), (t: 1.0, color: "#ff3300")],
            ),
            update: (
                gravity: (0.0, -2.0, 0.0),
                drag: 0.5,
                color_over_life: [(t: 0.0, color: "#ffcc00"), (t: 0.6, color: "#ff6600"), (t: 1.0, color: "#330000")],
                alpha_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)],
                kill_below: Some(-0.5),
            ),
            render: (
                mode: Stretched,
                sprite: Spark,
                blend: Additive,
                stretch: (velocity_scale: 0.02, min_length: 0.02),
            ),
        ),
    ],
)
```

## Example — smoke column

```ron
(
    version: 1,
    name: "smoke",
    system: (budget: 8192, lod: Some((distances: [50.0, 150.0], rate_scale: [1.0, 0.3, 0.0]))),
    emitters: [
        (
            name: "column",
            spawn: (
                mode: Rate(40.0),
                lifetime: (min: 2.0, max: 5.0),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.5)),
                velocity: (mode: Directional(
                    dir: (0.0, 1.0, 0.0),
                    spread: 15.0,
                    speed: (min: 1.0, max: 4.0),
                )),
                size: (min: 0.3, max: 1.0),
                color: [(t: 0.0, color: "#33333399"), (t: 1.0, color: "#11111100")],
            ),
            update: (
                gravity: (0.0, 0.3, 0.0),
                drag: 0.8,
                curl: (2.5, 0.3, 0.15),
                size_over_life: [(t: 0.0, v: 0.3), (t: 1.0, v: 1.0)],
                alpha_over_life: [(t: 0.0, v: 0.6), (t: 0.7, v: 0.3), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: Puff,
                blend: Alpha,
                softness: 0.5,
            ),
        ),
        (
            name: "base_glow",
            spawn: (
                mode: Rate(8.0),
                lifetime: (min: 0.8, max: 1.5),
            ),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.3)),
                velocity: (mode: Radial(speed: (min: 0.2, max: 1.0))),
                size: (min: 0.5, max: 1.5),
                color: [(t: 0.0, color: "#ff440044")],
            ),
            update: (
                gravity: (0.0, 0.5, 0.0),
                alpha_over_life: [(t: 0.0, v: 0.4), (t: 1.0, v: 0.0)],
            ),
            render: (
                mode: Billboard,
                sprite: SoftDisc,
                blend: Additive,
            ),
        ),
    ],
)
```

The additive `base_glow` above is the old ground disc. The gallery smoke
(`showcase::smoke`) does not use it: both emitters are lit alpha puffs
(`blend: Alpha`, `lit: true`, `sprite: PuffAnim`, softness about 1.6 m).
Fire ramps stay in the orange/yellow/white family after the sRGB hex is decoded
to linear. The present pass does not repaint a pixel to get there (ADR-049).
The gallery fireball is `FireballAnim`, premultiplied alpha, emissive 6: a few large
overlapping body cards, so the blast reads as one fire body with a hot core. Its grey tint
keeps tint × texture × 6 under the `Rgba16Float` ceiling so AgX does not swap red and green.
