# Hard-surface materials

Hulls and set pieces pick up detail, a clearcoat lobe, brushed-metal anisotropy, specular
occlusion, multi-scatter, parallax panel lines, and curvature edge wear from the standard
material. The factors default off on `LabMaterial::EMPTY`, so a lab scene that does not set
them keeps the previous shading. A `.material.ron` that migrates from v1 turns multi-scatter
and specular occlusion on.

The decision is ADR-043. The formulas and their papers are in `docs/provenance.md`.

## What is shaded

| Factor | Uniform | When it changes pixels |
|---|---|---|
| Detail albedo / normal / roughness | `detail`, `detail_fade` | Any strength above zero. Weight is fade × mask × strength |
| Clearcoat | `coat.x`, `coat.y` | Factor above zero. Dielectric F0 is 0.04. Uses the base normal |
| Anisotropy | `coat.z`, `coat.w` | Strength above zero and a non-zero tangent. Rotation is radians |
| Specular occlusion | `response.x` | Above 0.5. Multiplies the indirect specular (base, multi-scatter, coat), from AO |
| Multi-scatter | `response.y` | Above 0.5. Rough metals stay brighter |
| Parallax | `response.z`, `pom.x` | Scale above zero and `FrameQuality::parallax_occlusion` |
| Edge wear | `wear`, `wear_tint` | Intensity above zero and source 1 (curvature) or 2 (AO) |

`FrameQuality::parallax_occlusion` is on for Desktop High and Desktop Ultra and off for
Mobile, Desktop Low, Deck, the lab baseline, and the editor. The lab uploads it as
`LabFrame.pcss.z`. `quality.w` is the PCSS noise index. The view is normalized before the
step count. A height of 1 does not move the UV.

Six maps pack into three textures at upload: detail albedo (RGB) + roughness (A), detail
normal (RGB) + mask (A), height (R) + curvature (G). Missing channels stay neutral
(albedo 0.5, roughness 0.5, mask 1, height 1, curvature 0.5).

## Authoring

`docs/formats/material-v2.example.ron` is the v2 block. v1 files stay frozen;
`migrate_material_v1_to_v2` fills the defaults. `regia upgrade-project` writes them back.
The user-guide section is [Materials](../user-guide/materials.md).

`LabMaterial::from_authored` copies the factors and leaves texture slots empty.
`LabMaterial::from_authored_with_maps` copies resolved detail, height, and curvature
images; the mesh bind group uploads those three packed textures. The lab scenes in
`regia_render::hard_surface` bind procedural maps themselves.

## Look-dev

```sh
cargo run -p lookdev -- --hull /tmp/hull-closeup.png
```

That is the hull close-up: painted metal, clearcoat spheres, 960×540 at EV 14.5, lab
quality plus the parallax gate.
The cost table (second-frame forward milliseconds, 1280×720, 12 hulls, one factor at a
time) is:

```sh
flock /tmp/regia-gpu.lock flock /tmp/ferrite-gpu.lock env VK_LOADER_DRIVERS_SELECT='*nvidia*' \
  cargo run -p lookdev --release -- --hull-cost docs/evidence/hard-surface/cost.md
```

## Goldens

`cargo test -p regia_render --test hard_surface` renders each feature on and off on
lavapipe at 960×540, EV 14.5. Mean luma must sit in 60..=180. A coat glint must be present
only when the lobe is on, specular occlusion must darken a cavity, and the hull close-up
must show those glints. The on frame must also match
`docs/evidence/hard-surface/<feature>.png` (luma SSIM ≥ 0.98). Bless only on lavapipe:

```sh
env VK_LOADER_DRIVERS_SELECT='*lvp*' REGIA_BLESS=1 \
  cargo test -p regia_render --test hard_surface
```

A parallax material with the profile gate off must match the feature-off frame.
