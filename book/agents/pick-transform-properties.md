# Pick transform properties (agents)

This page records the deterministic checks for the existing `world.pick` surface. It adds no agent method. The contract is
that a ray direction is normalized by the call, reported distance is metres, layer filters apply after a geometric hit,
and a mesh hit reports the world point and front-side unit normal. See [World pick](world-pick.md) for the full method.

## Reproduce

Run the crate integration test:

```sh
cargo test -p regia_pick --test world_pick pick_results_follow_scene_similarity_and_direction_rescaling
```

Its deterministic fixture uses seed 0 (no random generator): one asymmetric named triangle with vertex identity
`[0, 1, 2]`, plus 63 separated decoys. Three fixed translation / rotation / positive uniform scale tuples cover identity,
rigid motion and scale 2.5. A fixed one-joint palette translates the same named triangle before those transforms. A direction
with magnitude `1e-8` and a separately rescaled direction check the near-zero valid ray and normalized-distance convention.
The rigid cases compare `raycast_mesh_bvh` to the independent all-triangle `raycast_mesh` loop. Rotating the index triples
moves the target's storage index from 0 to 63 while its named vertex triple remains `[0, 1, 2]`. No coplanar geometry is used
for these invariance checks.

## Agent protocol check

With the sample world-pick scene open in a connected agent session, send the existing call twice, changing only the
direction magnitude:

```json
{"method":"world.pick","params":{"origin":[0,50,0],"direction":[0,-1,0],"layers":[0,1]}}
{"method":"world.pick","params":{"origin":[0,50,0],"direction":[0,-0.00000001,0],"layers":[0,1]}}
```

Both answers should have the same `hit` entity, layer, point, normal and distance. To verify layer filtering, repeat with
`"layers":[9]` and then with a layer absent from the scene; the first returns its layer-9 mesh if present, and the second
returns `{"hit":null,"count":0}`. A zero direction returns `invalid_params` with message `` `direction` must not be zero ``;
an unknown layer is valid input and simply yields no hit. These are existing `world.pick` behaviors, not new error codes.

The method reports `point`, `normal`, `distance`, and triangle storage index. The property test uses the vertex triple as
triangle identity when storage order changes; a storage index is expected to change under permutation. Coplanar equal-distance
ties are excluded from the transform claim and checked separately by
`coplanar_triangle_ties_keep_the_lowest_storage_index`, which verifies that both the brute-force loop and BVH select index 0.
