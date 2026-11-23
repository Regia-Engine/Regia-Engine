# `.renv`: baked studio environment of the product viewer (format v1)

Writer: `regia_viewer_bake_env` (`regia_viewer::bake::bake`, `BakedEnv::to_bytes`). Reader:
`regia_viewer::BakedEnv::parse`. Decision: ADR-2539.

One environment: prefiltered specular cube mips and diffuse spherical harmonics. It is baked offline and only
parsed and uploaded by the player. A package may carry its own, or use the default inside the crate.

## Layout

Little-endian. No padding.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Magic `RENV` |
| 4 | 4 | `format_version`: u32, currently 1 |
| 8 | 4 | `face_size`: u32, texels per face at mip 0, a power of two from 4 to 2048 |
| 12 | 4 | `mip_count`: u32, 1 to log2(`face_size`) + 1 |
| 16 | 108 | Spherical harmonics: 9 coefficients of 3 f32, red, green, blue |
| 124 | rest | Texels: u32 RGB9E5, mip 0 faces +X, -X, +Y, -Y, +Z, -Z, then mip 1, and so on |

The file's length must be exactly what the header implies. A reader refuses anything else (`EnvError`).

## Meaning

- **Texels** are RGB9E5 as in `EXT_texture_shared_exponent`: 9 bits per channel, 5 shared exponent bits, base 2,
  bias 15, red in the low bits. Linear Rec.709 radiance.
- **Faces** follow the cube lookup of Vulkan and WebGPU, so a shader's `textureSampleLevel(cube, dir, lod)` returns the
  radiance arriving from `dir`.
- **Mips:** mip 0 is the mirror level. Mip `k` of `n` is the GGX prefilter at perceptual roughness
  `(k / (n - 1))^2`, so a shader reads `lod = sqrt(roughness) * (n - 1)`.
- **Harmonics** are real spherical harmonics up to band 2, in the order Y00, Y1-1, Y10, Y11, Y2-2, Y2-1, Y20, Y21, Y22,
  already convolved with the cosine lobe and divided by pi. Their sum at a normal is the light a white diffuse
  surface reflects. World: +X right, +Y up, +Z toward the viewer of a front view.

## Versions and migration

v1 is the first version, so there is nothing to migrate. A later layout bumps `format_version` and keeps the reader for
v1. `.renv` files are baked outputs: the migration for any version is to bake again from the source image with
`regia_viewer_bake_env`, so `regia upgrade-project` reports them and does not rewrite them.
