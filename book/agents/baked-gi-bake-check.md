# Checking a bake end to end

A baked GI volume passes through five tools before it lights a frame: the level, its exported `.bake.ron`
([Exporting a map's bake input](baked-gi-export.md)), the bake ([Baking GI from meshes](baked-gi-bake.md)), the
level naming the volume, and the runtime install ([Baked GI from the level](baked-gi-maps.md)). This page covers
the tools that check that chain without a GPU: the CPU bake (`--cpu`), reading a volume back (`--sample`), and
`cargo xtask bake-check`, which runs the whole chain on a fixture project. Design: ADR-365.

## Bake on the CPU

```text
regia bake-gi <scene> --cpu [--rays N] [--samples N] [--bounces N] [--out FILE]
regia bake-gi --from-scene <level> --bake --cpu [--write-level]
```

`--cpu` traces the probes on the CPU probe tracer instead of the GPU path tracer. Everything else is the same
bake: relocation, validity, the 3×3×3 filter and the `.fgiv` encode. The summary gains `"backend": "cpu"`, and
`device` reads `cpu (N threads)`.

Use it for checks, CI and small scenes (thousands of probes, thousands of triangles). Bake maps on the GPU. The
CPU tracer:

- traces the same ray sets and folds them the same way (L1 SH per component, distance moments);
- refuses a description that sets `clamp_direct`, `clamp_indirect`, `firefly_threshold` or `sample_groups` above 1
  (the quality controls are GPU-only; the bake fails instead of returning an unclamped mean with no noise
  estimate), and caps `samples × rays_per_probe` at 2^24 as the GPU does;
- treats every surface as diffuse (base color × (1 − metallic)) and samples no textures;
- takes a uniform sky or none (`bake_desc` descriptions always give one). A sky table, an equirect
  map or a cloud field is refused with a message (bake those on the GPU);
- refuses a heightfield whose sample count, spacing or origin the GPU packer would refuse, with the
  same message, instead of baking a scene with no ground;
- lights vertices with the sun and with each sky body as a direction. Emitters light the volume when bounce rays
  hit them;
- does not apply `clamp_direct`, `clamp_indirect`, `sample_groups` or `firefly_threshold` (ADR-1096 is the GPU
  kernels). The summary still prints those fields from the file. The volume is the uncapped trace, and each
  probe's `clamped`, `clamp_loss`, `suppressed_groups`, `firefly_loss` and `noise` stay at their defaults;
- does not cap `samples` so that `samples × rays_per_probe` stays within 2^24 (the GPU tracer does, because its
  ray counter is an f32). `rays_per_probe` above 4096 is traced as 4096, the same as the GPU;
- ends a path at a back face, so a probe inside geometry sees nothing.

Its result does not depend on the thread count. It agrees with a GPU bake in expectation, not bit for bit.

## Read a volume back

```text
regia bake-gi --sample <volume.fgiv> --at x,y,z [--at x,y,z ...] [--normal x,y,z]
```

This prints the irradiance the runtime lights each point with. The lookup is the renderer's CPU mirror of the
shader (`regia_render::sample_static_gi`), with the default lookup settings and every component at its baked
light. The viewer is taken along the normal (default `0,1,0`). It needs no GPU and no running game.

```json
{"ok": true, "volume": "assets/maps/courtyard.fgiv", "probes": 1924, "valid": 1924, "bricks": 3,
 "interior_bricks": 0, "terrain": false,
 "baked": {"sun_illuminance": 98213.0, "sun_direction": [0.0, 0.96, -0.28], "sky_scale": 3891.2, "fill_lux": 0.0},
 "samples": [
   {"at": [14.8, 0.0, 0.0], "normal": [0.0, 1.0, 0.0], "irradiance": [10521.4, 12088.7, 15110.2],
    "components": {"sun_bounce": [3.1, 3.4, 3.9], "sky": [10518.3, 12085.3, 15106.3], "fill": [0.0, 0.0, 0.0],
                   "emissive": [0.0, 0.0, 0.0]},
    "coverage": 1.0}]}
```

The numbers above only show the shape.

- `irradiance` and every `components` entry are in lux (irradiance, not irradiance / π), per channel.
- `coverage` is 0 outside the volume and 1 well inside it. With every corner probe rejected it is 0 as well.
- A point inside an INTERIOR brick reads only that brick.

Errors print `{"ok": false, "code", "message"}` and exit non-zero:

| `code` | Meaning |
|---|---|
| `volume_not_found` | No file at the path. |
| `volume_unreadable` | Not a `.fgiv`, another format version, damaged, or unreadable. |
| `sample_normal_invalid` | `--normal` is the zero vector. |

A malformed `--at` or `--normal` is a usage error (exit 2, before anything runs).

## The whole chain: `cargo xtask bake-check`

```text
cargo xtask bake-check [--gpu] [--release] [--keep] [--rays N] [--samples N]
```

It copies the fixture project `crates/regia_cli/tests/fixtures/bake_e2e` to a new temporary directory. Then it
runs each step through the real `regia` binary (`cargo run -p regia_cli`) and prints one line per step:

1. `bake-gi --from-scene assets/levels/courtyard.level.ron --bake --cpu --write-level`: two static entities
   export (the dynamic one is skipped). The bake writes `assets/maps/courtyard.fgiv`, and the level now names
   `maps/courtyard.fgiv`.
2. `bake-gi --from-scene … --check` exits 0 (the input is current).
3. `bake-gi --sample` at the open cell (14.8, 0, 0) and at the sealed room's centre (0, 0, 0):
   - the open cell's sky component is 0.94 to 1.02 of π × the exported sky radiance;
   - the room's centre is exactly 0.
4. The level opens in a headless app. `BakedGiLevelPlugin` installs the volume through the asset server, and the
   lookup on the installed volume must equal step 3's.
5. Moving a static entity makes `--check` exit 2 (stale).

The last line is a JSON summary:
- `backend`, `device`, the bake `report`, the exported `sky`;
- the open cell's `irradiance`, `sky` and `sky_ratio`;
- the sealed cell's `irradiance`.

The exit code is 0 when every step passes. A failed step prints `FAIL` lines and a JSON line with the step's
name.

- `--gpu` bakes on the GPU path tracer against the same expectations: the GPU vs CPU check. It needs a GPU
  (lavapipe works).
- `--keep` keeps the temporary project and prints its path, so you can inspect it with `--sample`, or by opening
  the level.
- `cargo test -p regia_cli --test bake_e2e` runs the same chain as a test. It needs no GPU.

## The fixture

`assets/levels/courtyard.level.ron` (scene v10) holds:
- `Room` at the origin: a sealed room with 0.3 m walls (inner box ±2.5 m facing in, outer box ±2.8 m facing out);
- `Marker`: a 0.2-scale static copy 24 m east;
- `Rover`: a 0.2-scale `GiMobility::Dynamic` copy.

The sky is Earth's at noon (day 172, 40° N), and there is no ground. The two cells have exact expectations:
- The open cell sees the uniform sky in every direction except where the rooms are. The rooms hide about 3 %, and
  their walls bounce back a quarter of it.
- No light reaches the room's inside. The lookup corners there, and every neighbor the filter mixes into them,
  are inside probes.

The export's volume brick (4 m) and the automatic bricks (1 m, one per room) place no probe inside a wall.

## Interiors reach the volume

The bake now writes the INTERIOR flag into the `.fgiv` (`.fgiv` v2 brick flags). The bake plan's `interior_flag`
decides which bricks get it: a brick inside an interior that is not `sky_open`. `--sample`'s `interior_bricks`
and `render.get_gi_mode`'s `interior_bricks` count them. A description without interiors bakes the same bytes
as before.
