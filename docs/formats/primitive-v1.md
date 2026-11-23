# `Primitive` component, format version 1

A greybox shape stored inside `.level.ron` / `.prefab.ron` files like any reflected component
(`regia_render::primitives::Primitive`, ported meshers in `regia_render::primitives`). The renderer
meshes it at load; the editor's **Create ▸ Shape** and the agent's `scene.spawn_primitive` create it.

```ron
"regia_render::primitives::Primitive": (
    format_version: 1,
    shape: Ramp(size: (6.0, 1.5, 3.0)),
    color: (0.72, 0.72, 0.72, 1.0),
    roughness: 0.65,
    metallic: 0.0,
),
```

| Variant | Fields (metres unless noted) |
|---|---|
| `Cuboid` | `size: Vec3` |
| `Sphere` | `radius`, `sectors: u32` (≥ 3), `stacks: u32` (≥ 2) |
| `Capsule` | `radius`, `length` (cylindrical middle), `longitudes` (≥ 3), `latitudes` (≥ 4) |
| `Cylinder` | `radius`, `height`, `resolution` (≥ 3), `segments` (≥ 1), `caps: bool` |
| `Cone` | `radius`, `height`, `resolution` (≥ 3) |
| `ConicalFrustum` | `radius_top`, `radius_bottom`, `height`, `resolution` (≥ 3) |
| `Plane` | `size: Vec2` (X, Z), `subdivisions` — single-sided, faces +Y |
| `Torus` | `minor_radius`, `major_radius`, `minor_resolution`, `major_resolution` (≥ 3) |
| `Tetrahedron` | `vertices: [Vec3; 4]` (either orientation) |
| `Ramp` | `size: Vec3` — a wedge rising towards +X |
| `Prism` | `points: [Vec2]` (3–256, simple polygon in XY, either winding), `depth` (along Z) |

Every shape is centred on the entity origin. Sizes must be finite and at most 10 000 m. Extent
axes must be positive, except a capsule's cylindrical length and either frustum radius, which may
be 0. Both frustum radii 0 is rejected. Resolutions at most 512. Invalid data logs a warning and
the entity draws no primitive mesh. An update that fails the same way drops the mesh this component
last inserted. The rendered roughness is the stored value clamped to 0.02–1.

**Versioning** (`docs/specs/12-assets.md` § 4a): `format_version` 0 (a hand-written component that
left the field out) is upgraded to 1 at load; a version above the engine's
`PRIMITIVE_FORMAT_VERSION` is refused with an error instead of being misread. The reflected field
list is hashed by `cargo xtask check-formats` (entry `primitive`), so a field change without a
version bump and a migrator in `Primitive::migrate` fails CI. `regia upgrade-project` works per
file type and does not rewrite components inside level files; the load-time migration covers them.
