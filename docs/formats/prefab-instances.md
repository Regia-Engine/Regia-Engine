# Prefab instance set (`regia_asset::PrefabInstanceSet`), format version 1

The GPU-instanced nodes of an imported glTF (`EXT_mesh_gpu_instancing`, ADR-3016). It is the payload of the glTF's
`prefab/instances` sub-asset (`AssetId::sub_asset(gltf, "prefab/instances")`), a cooked file (`FCKD` header, type tag
`regia_asset::PrefabInstanceSet`) in the dev cache (`.regia/cooked/<os>/<id>.fckd`) and in paks. The schema hash is
`prefab-instances` in [`format-hashes.ron`](format-hashes.ron).

**Only a file with instances has one.** A glTF without the extension writes no such sub-asset and no `.meta` entry for
it, so its cook is what it was before this format existed.

## Fields

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 1 |
| `nodes` | list | Instanced nodes, in the order the node walk meets them |
| `nodes[].node` | string | The glTF node's name (`Unnamed` when it has none) |
| `nodes[].parts` | list of asset ids | The node's cooked mesh sub-assets, one per primitive; also parts of the `model` sub-asset and the sub-asset's dependencies |
| `nodes[].transforms` | list of `[f32; 16]` | One column-major matrix per instance |

## What a transform means

A cooked part already has its node's world transform `W` and the importer's axis turn in its vertices. A consumer that
ignores this set draws one copy where the node stands, which is what a viewer without the extension draws. A transform
is relative to that copy: instance `i` of a part draws at `model_world * transforms[i] * vertex`. The authored instance
transform is `I`, so `transforms[i] = W * I * W⁻¹`, and the identity instance is the identity matrix. A node whose `W`
has no inverse (a scale of zero) gets no entry and draws once, with a warning.

## Why this and not more parts

- The geometry stays cooked once. One part per instance would repeat the mesh N times in the pak.
- `CookedModelPart` has no transform, and giving it one would change every cooked model and every consumer.
- The renderer already batches draws of one mesh and material, so one entity per transform draws instanced. That is how
  the editor's named hierarchy spawns these.

`regia_render::cooked_model`, which builds a model's draws from a pak, does not read this set yet: it draws the single
copy. See the deferred list of ADR-3016.

## Rules

`PrefabInstanceSet::validate` refuses, with the agent code `prefab_instances_invalid`: more than `MAX_INSTANCED_NODES`
nodes, a name over `MAX_INSTANCED_NODE_NAME_BYTES` bytes, a node with no part or over `MAX_INSTANCED_PARTS`, no
transform or over `MAX_NODE_INSTANCES`, and a transform with a non-finite number. The other codes:
`prefab_instances_corrupt` (does not decode), `prefab_instances_version` (another version) and
`prefab_instances_wrong_type` (a cooked file of another type).

## Migration

Version 1 is the first version: there is nothing to migrate. `regia upgrade-project` knows the format
(`prefab-instances`, `.fckd`, cooked files of other types skipped) and reports such files as current. A cache cooked
before this format has no such sub-assets; the next `regia cook` re-imports every glTF (the importer moved to 1.12.0) and
a glTF with instances gets a new cache key (its settings hash takes this format's version).
