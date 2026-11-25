# VFX and decals

<!-- index: Particles -->
<!-- index: Decal -->
<!-- index: Trail -->
<!-- index: VFX -->

`regia_vfx` provides GPU particles, projected decals, ribbon trails and camera shake. Particles
are simulated and sorted in compute shaders; decals are box-projected volumes binned into screen
tiles; everything draws in the frame's translucent pass. Specification:
`docs/specs/26-terrain-vfx-highend.md` § 3. Precipitation and ambient particle counts follow
`QualityTier::content_tier`: VeryLow uses Low's counts. The surface a decal lands on is a
material ([Materials](../how-to/picture/materials.md)). Translucent particles draw after the
upscaler resolve, so they do not write the upscaler's reactive mask
([Upscalers](../agents/upscalers.md)).

## Setting it up

```rust
use regia::prelude::*;
use regia_vfx::scene::VfxLibrary;
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, VfxScene};

pub struct Effects;

impl Plugin for Effects {
    fn build(&self, app: &mut App) {
        // VfxPlugin: effect requests, decals, shake, the CPU scheduler.
        // VfxRuntimePlugin: the GPU side, drawn through the renderer's translucent layer.
        app.add_plugins((VfxPlugin, VfxRuntimePlugin));

        // The effect library: the built-in showcase set (muzzle_flash, impact, explosion, …).
        let mut library = VfxLibrary::new();
        for effect in regia_vfx::showcase_effects() {
            if let Err(err) = library.add(effect) {
                warn!("effect rejected: {err}");
            }
        }
        app.insert_resource(VfxScene::new(library));
    }
}
```

`VfxPlugin` alone runs the deterministic CPU side (spawn scheduling, budgets, decal pool,
shake); add `VfxRuntimePlugin` after it to draw. Your own `.vfx.ron` files load with
`VfxLibrary::load_dir(path)`, or set `VfxSettings::library_dir` and the plugin scans that
directory at startup. `VfxSettings::profile` (`VfxProfile`) sets pool sizes, sorting and the
particle-light budget per quality tier.

## Spawning effects

Effects are requested with messages, so any system (or a script, or the sim bridge) can trigger
them without holding the VFX state:

```rust
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::{CameraShakeRequest, SpawnEffect};

fn on_explosion(mut effects: MessageWriter<SpawnEffect>, mut shake: MessageWriter<CameraShakeRequest>) {
    let at = Vec3::new(3.0, 0.0, -6.0);
    effects.write(SpawnEffect {
        effect: "explosion".into(),
        position: at,
        forward: Vec3::Y,
        seed: None,
    });
    shake.write(CameraShakeRequest { position: at, strength: 0.6, radius: 12.0 });
}
```

`forward` is the effect's local +Z (the direction a muzzle flash fires). `seed: None` takes the
next value of a per-run counter, so a replay with the same inputs spawns identical particles.
Camera shake accumulates trauma that decays over time (`VfxSettings::shake_decay`,
`shake_amplitude`, `shake_roll`); `CameraShake` holds the current offset for your camera rig.

## Authoring a particle system

A `.vfx.ron` file describes one named system of one or more emitters, each a stack of Spawn,
Init, Update and Render modules. The current format is version 5 (`docs/formats/vfx-ron.md`);
older files load and migrate in memory. This version-1 muzzle flash still loads, with the newer
fields at their defaults:

```ron,vfx
(
    version: 1,
    name: "muzzle_flash",
    system: (budget: 256),
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
                    dir: (0.0, 0.0, -1.0),
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
    ],
)
```

| Module | Controls |
|---|---|
| `spawn` | `Burst(n)`, `Rate(per_second)` or `PerDistance` (trails of puffs behind a mover), particle lifetime range |
| `init` | emission shape (`Point`, `Sphere`, `Box`, `Cone`), velocity (including the `Radial` mode), size, color gradient |
| `update` | gravity, drag, curl noise, vortex, attractor, size / color / alpha / rotation over life, depth collision against the scene |
| `render` | `Billboard`, stretched or mesh particles; sprite or flipbook; `Additive` or `Alpha` blend; soft-particle `softness`; `lit` smoke with `self_shadow`; HDR `emissive`; `screen_floor_px` so distant sparks never vanish below a pixel |

Version 2 added lit smoke, emissive, flipbook blending and the screen-size floor; version 3 the
animated fire sheet (`FireAnim`); version 4 the fireball body sheet (`FireballAnim`) and
`heat_haze` cards; version 5 `Planar` velocity. The [agent guide's VFX page](../agents/vfx.md) has a full version-5 smoke
example and the look rules (fire hue in linear light, bloom on HDR frames only, alpha emitters
sorted back to front).

Effects can also emit **particle lights**: a fireball lights the ground under it through the
renderer's local lights. `vfx.get_look` / `vfx.set_look` (or the `VfxLook` resource) toggle heat
haze and fire light and scale them.

## Decals

A decal is a box that projects albedo (and optionally normal, roughness and emissive) onto
whatever is inside it. Prefabs are `.decal.ron` files (`docs/formats/decal-ron.md`, version 1);
the engine ships `scorch`, `pad_marking`, `rut` and `grime`, and a level adds more by placing
`*.decal.ron` files in the VFX library directory.

- **On a hit**: write a `SpawnDecal` message, or call `vfx.spawn_decal` with a position and a
  surface normal; the decal's local +Z aligns with the normal.
- **Along a path**: `vfx.place_rut` stamps overlapping decals along points (a quadratic curve
  with `curve: true`), for tire ruts.
- **Static markings**: put a `StaticDecal { prefab, yaw, receiver_mask }` component on a level
  entity; the plugin copies it into the pool when it appears.

Each decal has a **receiver mask**: bit 0 terrain, bit 1 static meshes, bit 2 characters. The
built-in prefabs use `3`, so scorch marks land on the ground and walls but never on a character.
The renderer writes a receiver buffer each frame (terrain as layer 0, rigid meshes as 1, skinned
meshes as 2, or `MeshRenderer::decal_layer` when set) that the decal pass reads. Desktop High
keeps 256 visible decals and Desktop Ultra 512; when more are visible the highest priority win.
`vfx.decal_budget` reports the live count and culling.

## Trails

`TrailRig::new(TrailStyle { width, lifetime, color, min_segment })` records an entity's path as a
ribbon that fades from head to tail: tracer rounds, vehicle wakes, sword swipes. `VfxTrails`
holds the live trails for the runtime.

## Checking effects

Particles move, so a single screenshot rarely tells you much. `seeing.capture_sequence` returns a
strip of frames and `seeing.contact_sheet` tiles them into one image for review.
`samples/vfx_lab` renders the showcase set in a window or, with `--evidence <dir>`, offscreen to
PNGs; `regia_vfx::measure_effect` and `assert_effect_plausible` are the not-empty, not-black,
not-blown-out checks its tests use.
