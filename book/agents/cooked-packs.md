# Cooked packs (agents)

A cooked pack is a pak whose every entry loads with no source parse and no transcode: meshes
GPU-ready (desktop), textures KTX2 in the platform's GPU block family (BC on desktop, ASTC on
Android) without supercompression, no glTF, PNG or JPEG anywhere. The runtime crates cannot read
glTF or PNG at all; this page is how an agent checks that a running game, and the paks it ships,
hold up the rest of the promise. User guide: [Cooked packs](../user-guide/cooked-packs.md).
Decision: ADR-1594.

## What the running game mounted and loaded

```json
{"jsonrpc":"2.0","id":1,"method":"assets.cooked_pack","params":{"inspect":true}}
```

```json
{
  "platform": "linux",
  "paks": [{
    "name": "base", "entries": 412,
    "meshes": { "gpu_ready": 61, "compact": 4 },
    "textures": 188, "texture_families": { "bc": 188 },
    "supercompressed_textures": 0, "texture_family_mismatches": 0,
    "mip_metadata_textures": 40, "mip_capped_textures": 6, "unstreamed_textures": 1, "textures_inspected": true,
    "images_rgba8": 3, "large_images_rgba8": 0, "other": 150, "unreadable": 0,
    "kinds": {
      "mesh_gpu_ready": { "entries": 61, "bytes": 9400112, "stored_bytes": 3120440 },
      "texture": { "entries": 188, "bytes": 52100200, "stored_bytes": 52100200 },
      "audio": { "entries": 6, "bytes": 1210400, "stored_bytes": 1190000 },
      "other": { "entries": 150, "bytes": 211000, "stored_bytes": 90000 }
    },
    "submesh_ranges": 18, "meshes_with_submeshes": 9, "meshes_without_submeshes": 52,
    "findings": [{
      "code": "compact_meshes", "severity": "warning", "count": 4,
      "message": "meshes stored compact: …",
      "examples": ["assets/pilot.glb#mesh/body/primitive_0"]
    }]
  }],
  "loads": { "gpu_meshes": 40, "compact_meshes": 2, "legacy_meshes": 0, "textures": 97,
             "images_rgba8": 1, "pak_loads": 140, "cache_loads": 0, "textures_cpu_decoded": 0 },
  "errors": 0, "warnings": 1, "cooked_only": false
}
```

- `inspect` (default `false`) also reads every texture entry's KTX2 header (`vkFormat` and the
  supercompression scheme) and checks its family against the platform. Without it only the pak's
  table of contents is read, `texture_families` is empty and `textures_inspected` is `false`.
  Inspecting reads every texture entry of every pak: call it once after a load, not every frame.
- `kinds` (always filled, from the table of contents) gives the entry count and the bytes per kind:
  `mesh_gpu_ready`, `mesh_compact`, `mesh_legacy`, `texture`, `image`, `audio` and `other`. `bytes` is the
  uncompressed payload and `stored_bytes` what the pak holds. `audio` counts the cooked sounds (the source
  file's bytes plus its loudness record, so a packaged game plays them with no loose file; ADR-2788).
  Together with `meshes` and `textures` this is the pak's size report: sort `kinds` by `bytes` to see
  what a pak is made of. `regia cook --pack-report` prints the same fields.
- `submesh_ranges`, `meshes_with_submeshes` and `meshes_without_submeshes` (need `inspect`) count the
  submesh ranges of the GPU-ready meshes (layout v2 and later). A mesh with ranges splits by material, so a
  destruction chunk draws its cut faces with the cut material from a pak; a mesh without ranges is a
  one-material mesh, or was packed before v2 and its cut faces draw with the surface material:
  `regia cook --ship` again.
- `mip_metadata_textures` counts the textures with cooked mip streaming metadata (needs `inspect`); a smaller
  number than `textures` means an older cook. `mip_capped_textures` is how many of those have top levels that
  add no detail and stay out of memory, and `unstreamed_textures` how many a `.meta` keeps fully resident
  ([Rendering](../user-guide/rendering.md#mip-streaming-metadata)).
- `meshes`: `gpu_ready` loads as one copy of the vertex bytes; `compact` is unpacked and gets
  MikkTSpace tangents at load (the only form on Android; a vertex-colored mesh is GPU-ready since
  ADR-7919, a skinned or morphing one since GPU mesh layout v3, ADR-10080, which keeps the skin and
  the morph deltas after the indices; a pak cooked before v3 still lists those as `compact`); `legacy` is a cook from before M4 round 3.
- `loads` counts what this asset server (and the renderer's model loader, which decodes on worker
  threads) loaded so far: typed loads (`load_mesh`, ...) and counted loads (`load::<CpuMesh>`, ...,
  including `load_cooked_models_counted`) alike (ADR-7284). `cache_loads` are reads from the dev cooked cache (`.regia/cooked`): a
  shipped game has none. `textures_cpu_decoded` is process-wide: block-compressed textures the
  renderer decoded to RGBA8 because the device cannot sample the format.
- `cooked_only` is `true` when at least one pak is mounted, no pak has an error finding, and
  nothing loaded so far needed decode work (no compact or legacy mesh, no CPU texture decode, no
  dev-cache read). A dev run without paks is never `cooked_only`.

## Findings

Each pak carries `findings`, errors first. Codes are stable:

| Code | Severity | Meaning | Fix |
|---|---|---|---|
| `texture_family_mismatch` | error | A texture the platform cannot sample as stored (BC on Android; ASTC or ETC2 on desktop): the runtime decodes it to RGBA8 on the CPU, or cannot load it. | Cook the paks for the platform: `regia cook --platform <p> --ship`. |
| `supercompressed_textures` | error | A KTX2 with BasisLZ, Zstandard or ZLIB supercompression: the runtime has no transcoder. | Cook the source image instead of shipping a prebuilt `.ktx2`. |
| `unreadable_textures` | error | A texture entry that does not read or validate. | Recook. |
| `legacy_meshes` | error | Mesh entries from a cook before M4 round 3. | `regia cook --clean --ship`. |
| `compact_meshes` | warning (desktop only) | Meshes stored compact: unpacked and given tangents at load. | `regia cook --ship` or `--gpu-meshes` (skinned and morphing meshes included since GPU mesh v3, ADR-10080). |
| `large_uncompressed_images` | warning | RGBA8 images of 256 × 256 or more: no decode, but four to eight times the memory of a block-compressed texture. | `compress: true` in the image's `.meta`. |

`examples` lists up to eight manifest paths of affected entries.

## Errors

`F0016` when the app has no `AssetServer` (add `regia_asset::AssetPlugin`). A non-boolean
`inspect` is `F0002`.

## The same check without a running game

`regia cook --ship` writes the paks, then runs these rules over every pak it wrote and fails on an
error finding; the report is `build/paks/<platform>/cooked_pack.json` (the same per-pak objects as
above under `paks[].summary` and `paks[].findings`). `regia cook --pack-report` prints the report
for the existing paks as JSON without cooking (non-zero exit on an error). A shipping
`regia package` runs the same check on the paks it stages. Typical agent loop:

1. `regia cook --platform linux --ship` → exit 0 and `cooked_pack.json` with `"errors": 0`.
2. Launch the packaged game with the agent feature (a playtest profile), load the first map.
3. `assets.cooked_pack` → `loads.cache_loads == 0`, `loads.compact_meshes == 0` (skinned
   characters are GPU-ready since GPU mesh v3, ADR-10080), `loads.textures_cpu_decoded == 0`.

## Verify shared external glTF textures (v0.5.1, ADR-8055)

Use two models referencing one external PNG with equal effective settings, then cook and mount
the pak. These existing calls require no new method or protocol parameter:

```json
{"method":"registry.query","params":{"type":"CookedTexture","refresh":true}}
{"method":"registry.describe","params":{"asset":"assets/textures/paint.png"}}
{"method":"assets.cooked_pack","params":{"inspect":true}}
```

For a fixture with no other textures, require `registry.query.total == 1`, both materials in the
image's `referencers`, no missing dependencies, and `paks[0].textures == 1`. The standalone image's
`.meta` owns the shared id. Different effective settings retain separate ids and produce the cook
warning `shared_texture_settings`; that is a cook diagnostic, not an RPC failure. The registry's
variant path ends in `#gltf_texture/<settings hash>`. Embedded images remain private. No new agent
error codes were added.

The deterministic protocol regression is
`cargo test -p regia_agent --test gltf_texture_dedupe`; it sends requests through `AgentInbox` and
advances the headless app with fixed updates (no GPU or timing assertions). Pipeline regressions:
`cargo test -p regia_asset_pipeline --test gltf_texture_dedupe -- --nocapture`.

The [user guide's Upgrade instructions](../user-guide/cooked-packs.md#upgrade-stored-texture-sub-asset-ids)
explain replacing persisted ids and model texture sub-path lookups. Payload-only ORM sharing is
reserved for the v0.6 pak-index design; v1 still preserves each entry's distinct cooked header.

### Explicit heightfield sidecars (ADR-8143)

A 16-bit grayscale PNG referenced by a glTF is a texture input, including `../` URI spellings.
To also cook it as a standalone heightfield, put an explicit rule in `config/import_rules.ron`:

```ron
(format_version: 1, rules: [
    (glob: "assets/terrain/shared.png", importer: "png", settings: "(heightfield: Some(true))"),
])
```

The same setting in the image's `.meta` overrides rules. `heightfield: Some(false)` opts out;
unreferenced L16 sources keep their heightfield default when unset. With the setting enabled,
`registry.describe` reports the primary Heightfield and a separate shared texture variant for
material slots. `assets.import_rules` exposes the effective setting and its rule or metadata
source. Query those existing methods after recooking; no new method or error code is introduced.

## Inspecting a texture tier

Cook with repeated `--texture-tier NAME=MAX_EDGE_PX`, then launch the host with
`AssetServerSettings::content_dir` pointing at one complete
`build/paks/<platform>/texture_tiers/<NAME>/` directory. `assets.cooked_pack {"inspect": true}`
uses the same rules and fields above: eligible material-map bytes reflect the retained mip chain;
UI, HDR/float, cube, terrain VT, HLOD and voxel textures keep their full bytes (ADR-8144).
`assets.status` inspects the host-loaded, unchanged asset IDs, and `loads.cache_loads == 0` confirms pak-only loads.
The CLI `--ship` report includes every requested tier; `--pack-report` still reads ordinary paks.
See [texture tiers](texture-tiers.md) for the selection and verification recipe.
