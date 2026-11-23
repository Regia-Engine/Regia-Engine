# Product viewer renderer (agents)

`regia_viewer` is the lean renderer behind the web player and the Android viewer: one forward pass, one
baked environment, uniform buffers and textures only. It does not use the engine's renderer and has its own
device, so these methods check it headlessly and do not touch the running game's frame.

The capability is always present in a build with the agent. There is no marker to ask for.

## `viewer.capabilities`

```json
{ "method": "viewer.capabilities", "params": {} }
```

Answers the shader families, the material kinds, the fixed pipeline count, the lookup atlas size and the
environment format version. It touches no GPU.

## `viewer.render_still`

Renders the stand-in piece (a stud with a bezel, a post, a ring of beads and one round cut stone) on a
headless device and answers its hash, adapter, sample count, pipeline count and counters.

Requires `play` and is mutating because optional `path` writes a PNG (ADR-4760).
A read-only token is refused even when `path` is omitted; the default profile permits it.

```json
{
  "method": "viewer.render_still",
  "params": {
    "width": 512,
    "height": 512,
    "materials": {
      "metal": { "kind": "metal", "base_color": [1.0, 0.77, 0.34], "roughness": 0.14 },
      "stone": { "kind": "gem", "tint": [0.2, 0.3, 0.9], "ior": 1.77, "sparkle": 0.6 }
    },
    "view": { "tilt_deg": 10.0, "turn_deg": 0.0, "distance_mm": 36.0 },
    "exposure": 1.0,
    "contact_shadow": 0.4,
    "path": "stills/stud.png"
  }
}
```

- Every parameter is optional. `width` and `height` default to 512 and may be at most 4096, or the device's largest texture when that is
  smaller (`viewer_render_failed`).
- `materials.metal` replaces the metal parts, `materials.stone` the stone. A material is an object with a
  `kind` (`metal`, `anodized`, `gem`, `opal`, `cabochon`, `frosted`) and that kind's fields, which are the
  fields of the `regia_viewer` params structs; an `anodized` material takes `color` or `lut`, a list of
  cos(theta) colors.
- `contact_shadow` is the shadow's opacity, a number from 0 to 1 (anything else is `viewer_bad_value`, before a
  device is made). Without it there is no shadow.
- `env` is a `.renv` file to light with instead of the default studio.
- `path` writes a PNG. Like `env`, it follows the output-root rule of `render.out_start`
  (`book/agents/safety.md`).

The hash is stable: the same inputs on the same adapter and driver give the same hash, and `pipelines_created`
always equals `pipelines`. Compare hashes to prove a change moved no pixel.

## `viewer.env_info`

```json
{ "method": "viewer.env_info", "params": { "path": "studio.renv" } }
```

Answers `{ ok: true, format_version, face_size, mips, bytes }`, or `{ ok: false, error, bytes }` for a file that
fails to parse. The format is [`docs/formats/viewer-env.md`](../../docs/formats/viewer-env.md).

## Errors

All refusals are `F0002` with `data.reason`, except a machine without an adapter, which answers `F0003`.

| `data.reason` | When |
|---|---|
| `viewer_bad_size` | `width` or `height` is not a whole number from 1 to 4096. |
| `viewer_bad_value` | A parameter has the wrong type or is not finite, or `contact_shadow` is outside 0 to 1. |
| `viewer_unknown_field` | A field no version of the method knows. |
| `viewer_bad_material` | A material without a `kind`, or with an unknown one. |
| `viewer_bad_env` | The `env` file fails to parse; the message says why. |
| `viewer_env_unreadable` | The `env` or `path` file cannot be read. |
| `viewer_render_failed` | The renderer refused a value or the read-back failed. |
| `viewer_write_failed` | The PNG cannot be written. |
| `viewer_no_adapter` | No GPU adapter (`F0003`). Install the lavapipe driver on a headless machine. |
| `path_outside_project` | A path breaks the output-root rule. |
| `editor_headless` (`data.code`) | The host opened no GPU on purpose (a headless editor started with `--no-gpu`, ADR-5180): `-32004`, nothing is rendered and no device is opened. |

## The Rust interface

A host creates a device with `gpu::request_viewer_device` (WebGL2's limits on every backend, so a binding WebGL2
cannot hold fails in native tests too) and a `ViewerRenderer` on it.

| Call | What it does |
|---|---|
| `upload_mesh`, `remove_mesh` | A `ViewerMeshData`: position, normal, uv and facet number per vertex, in millimeters. |
| `create_material`, `set_material`, `remove_material` | A material slot. `set_material` writes one uniform block and builds nothing. |
| `add_lut` | One row of the lookup atlas, for an anodize film's cos(theta) table. |
| `set_look_texture` | `look_tex0` or `look_tex1` for a look's own noise or sparkle table. |
| `add_draw`, `set_draw_material`, `set_draw_mesh`, `set_draw_transforms`, `remove_draw` | One mesh under one material at one or more transforms. Many transforms are one draw call. A mirrored transform is drawn with the other winding. |
| `set_camera`, `set_style`, `set_environment` | The view, the exposure, clear color and contact shadow, and the light. |
| `render_if_needed`, `needs_redraw`, `request_redraw` | The lazy loop: nothing is drawn while nothing changed, and setting an equal value changes nothing. |
| `render` | Draws one frame into a texture of the configured format. |
| `capture_still`, `capture_still_async` | The same pass offscreen, read back as RGBA8 (`Still`, with `hash` and, with the `tools` feature, `encode_png`). |
| `stats`, `pipeline_count` | Counters. `pipelines_created` stays at `pipeline_count` for the renderer's life. |

The renderer never sees a surface. On Android the host drops the surface on `suspended`, keeps the renderer and
hands it the new surface's textures after `resumed`.

## Materials and shader hooks

`ViewerMaterial` has the kinds `Metal`, `Anodized`, `Gem`, `Opal`, `Cabochon` and `Frosted`. Each belongs to a
`ShaderFamily` (metal and anodized share one), and each family has one prebuilt pipeline per winding. All the
values are data; a new parameter means a new field in the block, never a new pipeline.

A hook is WGSL for one family, passed with `ShaderHooks::with_hook` or edited in
`crates/regia_viewer/src/shaders/family_*.wgsl`. It defines one function:

```wgsl
fn shade_surface(s: Surface) -> vec3<f32> {
    // s: world_pos, normal, view, n_dot_v, uv, local_pos, local_normal, facet, basis
    // material.base, material.p0, material.p1, material.p2: the block of this draw
    return env_radiance(reflect(-s.view, s.normal), material.base.w) * material.base.rgb;
}
```

It returns linear radiance before exposure. The prelude gives it `env_radiance(dir, roughness)`,
`env_diffuse(normal)`, `dfg(n_dot_v, roughness)`, `env_specular(s, f0, roughness)`, `lut_sample(row, coord)`,
`antialiased_roughness(normal, roughness)`, `fresnel_dielectric(cos, ior)`, `hash11(x)`, the textures `look_tex0`,
`look_tex1` with `look_sampler`, and three fixed studio lights for glints, `frame.key_dir[i]` and `frame.key_col[i]`
(`ViewerStyle::key_lights`). `s.basis` holds the instance's unit axes as columns: `s.basis * d` turns a direction from the
mesh's space into the world and `s.basis[2]` is a stone's axis. The entry shader builds `s.basis` from normalized matrix
columns (orthonormal), so `transpose(s.basis) * d` is the exact way back; a non-orthonormal basis would need its true
inverse.

What a block's fields mean per family:

| Family | `base` | `p0` |
|---|---|---|
| metal | rgb reflectance, roughness | x: 1 = read lookup row y |
| gem | tint, roughness | ior, dispersion, sparkle, seed |
| opal | body, roughness | fire, patch size in mm, seed |
| cabochon | body, roughness | ior, translucency |
| frosted | tint, roughness | ior |

A look whose fields differ from this table packs its own block with `ViewerMaterial::Custom { family, block }`. Non-finite
values read as zero; the hook clamps the rest.

A hook must run on WebGL2: no storage buffers, no compute, no new bindings, no loops over scene data, and no time,
because a frame is only drawn on a change and a still must hash the same. `cargo test -p regia_viewer` compiles every
built-in hook to GLSL ES 3.00.

## Baking an environment

```text
cargo run -p regia_viewer --bin regia_viewer_bake_env -- input.hdr out.renv [--face 64] [--mips 5] [--samples 256]
cargo run -p regia_viewer --bin regia_viewer_bake_env -- --studio out.renv
```

It reads a Radiance `.hdr` (equirectangular, `-Y h +X w`) or the procedural default studio. The default studio inside
the crate is 64 texels per face, 131 KB before compression.
