# Source-node tracks (v1)

`node-tracks` is a versioned postcard body embedded in `cooked-clip` v3
(ADR-8157). It is presentation data, not a simulation or lockstep format.
It must merge before the v0.6 format freeze.

`NodeAnimation` stores `version: 1`, `frame` (source scene to imported scene),
`start`, `end` (source-time interval), and `nodes`. Each `NodeTrack` stores
source `node { index, name }`, `parent` (an earlier slot, not a source index),
`is_joint`, local `bind` TRS, an optional exact static `matrix`, and optional
`translations`, `rotations`, `scales` curves. Names need not be unique.

A `NodeCurve` stores `times`, `values`, and `interpolation`: `Step`, `Linear`,
or `CubicSpline`. Cubic values are incoming tangent / value / outgoing tangent
triples. Vector channels interpolate linearly or by cubic Hermite. Rotation
LINEAR uses shortest-path slerp; cubic rotation interpolates all four components
with the authored derivatives, then normalizes the result. Tangents are neither
normalized nor hemisphere-flipped. STEP changes at the exact key time.

All channels have finite values, strictly increasing nonnegative times and
matching counts. Quaternion keys must be nondegenerate. Unknown body versions,
duplicate source indices, forward/cyclic parent slots and animated matrix nodes
are rejected. Static matrices preserve shear; zero scale is allowed. A cubic
rotation whose interpolated value is zero falls back to that node's bind rotation.

Sampling clamps clip-local time to the retained interval, adds `start`, and
composes the hierarchy. Untracked channels keep their bind values. `world` is
in the imported scene frame, `local` in the source parent frame. `delta` is
`world * inverse(bind_world)` for geometry already baked into the imported
scene frame; it is absent when bind-world is singular. Hosts must distinguish
joint nodes from plain parts using `is_joint` and bind by source `index`.
Slices only change `start`/`end`, retaining exact STEP boundaries and tangents.

The outer payload remains `FCLP`, little-endian version 3, postcard enum body:

- Variant 0: `Raw` (unchanged pre-P-11 clip fields).
- Variant 1: `Compressed` (unchanged joint compression fields and base clip).
- Variant 2: `RawNodes` (base clip, node-track body).
- Variant 3: `CompressedNodes` (compressed base, node-track body).

Both node variants store node keys losslessly. Old clips load with
`node_animation: None`; v1/v2 migration and `regia upgrade-project` continue
through the existing cooked-clip migrator. There is no separate project-file
suffix to upgrade for the embedded body. `NodeAnimation::from_payload` /
`to_payload` expose the same versioned body for tooling and corpus checks.
No prior node-track version exists, so its migration-source list is empty.
