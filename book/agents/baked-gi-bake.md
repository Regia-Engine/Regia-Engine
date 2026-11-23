# Baking GI from meshes

`regia bake-gi` bakes a map's indirect light into a `.fgiv` volume with the reference path tracer
(using the volume at runtime: [Baked GI volumes](baked-gi.md)). This page covers the bake side: the
`.bake.ron` description with static meshes, interiors and automatic detail bricks, the dry run, the summary
and the error codes. Format reference: `docs/formats/bake-ron.md`. Design: ADR-279 and ADR-280.

## Commands

```text
regia bake-gi <scene> [--out FILE] [--rays N] [--samples N] [--bounces N] [--seed N]
                        [--probes-per-batch N] [--no-filter] [--dry-run] [--check-volume FILE.fgiv]
                        [--rr-start N] [--clamp-direct L] [--clamp-indirect L]
                        [--sample-groups N] [--firefly-threshold K]
```

The last line's quality controls (bounces, clamps, sample groups, firefly suppression, the noise estimate) are
covered in [Bake quality controls](baked-gi-bake-quality.md).

- `<scene>` is a built-in (`cornell`, `furnace`, `rolling-hills`) or a description (`.ron`, usually
  `<map>.bake.ron`).
- `--dry-run` validates the description, loads every mesh, checks the manifest and lays out the probes. It
  prints the layout and does not trace. It needs no GPU.
- `--check-volume FILE.fgiv` builds the job as `--dry-run` does and compares it with a baked volume instead of
  baking. It needs no GPU. See [Is the bake stale?](#is-the-bake-stale).
- Without `--dry-run` or `--check-volume`, the command bakes on the first GPU adapter and writes `--out` (default
  `<map>.fgiv`). `--cpu` bakes on the CPU instead (no GPU; for checks and small scenes): [Checking a bake end
  to end](baked-gi-bake-check.md).

Both print one JSON object on stdout. Progress goes to stderr.

## Describing a map

A v1 description adds static meshes to the terrain, sun and sky of v0:

```ron
(
    format_version: 1,
    root: Some("<build-dir>"),
    manifest: Some("MANIFEST.sha256"),
    terrain_mesh: Some((source: "terrain/terrain.gltf", spacing: 1.25)),
    static_meshes: Some("static_meshes.json"),
    interiors_file: Some("interiors.json"),
    sun: Some((toward: (0.3, 0.5, 0.2), illuminance: 50000.0)),
    layout: (
        terrain_spacing: Some((8.0, 8.0)),
        auto_bricks: Some((spacing: 1.0, margin: 2.0, layers: ["setpiece"])),
    ),
)
```

- **Meshes** are occluders and bounce surfaces with their base color. Emissive parts become lights. Sources
  are glTF (`.gltf`, `.glb`, every primitive with its node transforms, LOD0) or cooked meshes (`.fckd`).
  Transforms are column-major 4x4 in Regia Engine's frame. A mirrored transform (negative determinant) flips the
  winding, so the faces stay front-facing. `instances` places one source many times (rock hints). The bake
  does not sample textures: a textured part without a `material: Some((albedo: ...))` override bakes with
  `default_albedo` (0.5) times its base color factor. The summary counts these parts as `default_albedo_parts`.
- **`static_meshes`** reads DO27's `static_meshes.json` as it is. It loads `source`, never `mesh`. It skips
  non-static entries, `structure`/`dynamic` layers, and scatter classes whose instances Regia Engine generates
  (`instances: null` with a `placement`). The summary lists them under `plan.skipped`.
- **`terrain_mesh`** turns the terrain grid mesh into the traced heightfield. The terrain layer follows it, and
  uncovered samples (an open borehole) are holes. The same source in `static_meshes` is not placed again.
- **`interiors`** / **`interiors_file`**: a brick inside an interior's box is tagged with it. The tag asks for
  INTERIOR (no sky through the walls) unless the interior is `sky_open`. The flag is written into the brick in
  the `.fgiv`. It needs the brick's whole box inside the interior, so a cluster brick that reaches out of the room
  stays outdoor. An interior with `openings` is INTERIOR as a whole in v0.2.0.
- **`auto_bricks`** places one brick over each interior, then one per cluster of mesh placements. Each
  placement's box grows by `margin`, and overlapping boxes merge. When these bricks (interiors included)
  would hold more than `max_probes`, the cluster bricks coarsen first so the interiors keep their spacing. Only
  when the interiors alone leave fewer than 8 probes per cluster does every spacing coarsen by one factor.
  `plan.auto_bricks.scale`, `plan.auto_bricks.interior_scale` and `plan.auto_bricks.spacing` report the factors
  and the cluster spacing used. With thousands of small placements (rock hints), restrict `layers` to the set pieces.
- **Leak control**: probes inside mesh geometry are moved out by relocation or marked invalid
  (`INSIDE_GEOMETRY`), as for any bake. A sealed room's probes see no sky. Walls between neighboring probes and the
  INTERIOR bricks the bake writes: [Baked GI leak control](baked-gi-leak-control.md).

## Output

`--dry-run`:

```text
{
  "ok": true, "dry_run": true, "scene": "bore.bake.ron",
  "layers": {
    "terrain": { "origin": [..], "spacing": [8.0, 8.0], "counts": [160, 160], "levels": [..], "probes": 128000, "validity": null },
    "bricks": [ { "index": 0, "first_probe": 128000, "origin": [..], "spacing": [..], "counts": [..], "probes": 4096,
                  "plan": { "source": { "interior": { "id": "hangar" } }, "interior": "hangar", "interior_flag": true, "sky_open": false },
                  "validity": null } ],
    "probes": 140000, "valid": null, "invalid": null
  },
  "plan": { "format_version": 1, "terrain_from_mesh": true,
            "meshes": { "entries": 21, "sources": 14, "placements": 4030, "triangles": 2600000, "mirrored": 12, "skipped": 6, "default_albedo_parts": 9 },
            "skipped": [["scatter_rocks", "generated_placement"], ..], "interiors": 3,
            "bricks": [..], "auto_bricks": { "requested_spacing": 1.0, "spacing": 1.0, "scale": 1.0, "interior_scale": 1.0, "placements": 12, "clusters": 5, "probes": 9000, "interiors": 3 },
            "manifest_verified": 40, "volume_bytes": 19040000 },
  "settings": { .. }, "seconds": 12.5
}
```

The numbers above are illustrative. A bake prints the same object without `dry_run`, plus `out`, `bytes`,
`device`, `report` (probes, valid, under terrain, inside geometry, relocated, rays, `borderline_back_face`,
`dead_interior_cells`) and `build_seconds`. `dead_interior_cells` counts interior-brick cells whose eight probes
are all invalid: they read black at runtime, so a non-zero value asks for a finer brick or a looser interior box.
`borderline_back_face` counts probes within 1 % of the inside-geometry threshold. Their validity can differ on
another GPU, so compare bakes from one device. In a
bake, every terrain and brick entry has `validity`: `{ valid, invalid, under_terrain, inside_geometry,
relocated }`, and the totals `valid` / `invalid` are filled.

Brick `plan.source` is `"builtin"`, `{ "explicit": { "index" } }`, `{ "interior": { "id" } }` or
`{ "cluster": { "meshes": [ids], "placements" } }`.

## Is the bake stale?

Nothing rebakes a volume when a mesh, an interior or the sun moves. `--check-volume` tells you whether a volume still
matches its description (with the same command-line overrides as the bake):

```text
{
  "ok": false, "check": "bore.fgiv", "scene": "bore.bake.ron", "stale": true,
  "reasons": ["scene", "layout"], "code": "bake_volume_stale",
  "volume": { "probes": 140000, "interior_bricks": 0, "scene_hash": "6a1f..." },
  "description": { "probes": 140000, "interior_bricks": 2, "scene_hash": "93c0..." }
}
```

`reasons` lists what changed since the bake, in this order:

| Reason | What changed |
|---|---|
| `scene` | The traced scene: a mesh, its transform or material, the terrain, the sun or the sky (the scene hash). |
| `layout` | The probe layout: the terrain layer, a brick's box or spacing, a brick's INTERIOR flag, the moment resolution or the distance clamp. A volume baked before v0.1.28 wrote interior flags is stale here. |
| `lights` | The light levels the components were baked for (sun, sky, fill). |
| `settings` | Seed, rays, samples, bounces or the filter. The relocation rounds and thresholds are not recorded in the volume, so a change there is not reported. |
| `emitters` | The scene has emissive surfaces but the volume does not name them: a format v1 or v2 bake. On Desktop High and Ultra its emitters are counted twice (ADR-1386, `baked-gi.md` "Emitters the bake holds"). |

A fresh volume prints `"ok": true, "stale": false, "reasons": [], "code": null` and exits 0. A stale one exits
non-zero. Rebake it with the same command without `--check-volume`. The runtime also reports the sun drift live:
`render.get_gi_mode` `baked.sun_mismatch_deg` (above 3°, the baked bounce light is for another sun).

## Errors

A description error prints `{"ok": false, "code", "path", "message"}` on stdout and exits non-zero. `path` is
the key in the description, or in the file it names (`static_meshes.json: meshes[3].layer`).

| Code | Meaning |
|---|---|
| `bake_scene_unknown` | Neither a built-in nor a `.ron` file. |
| `bake_desc_parse` | RON (or JSON) syntax error, wrong shape, or unknown field. |
| `bake_desc_newer` | `format_version` above this engine's. Upgrade the engine. |
| `bake_desc_invalid` | A value serde cannot check: non-positive spacing, inverted box, albedo outside 0..1, `terrain` together with `terrain_mesh`, a bad DO27 header or entry, a degenerate transform, a bake control out of range (`bake.*`, also from a flag: [Bake quality controls](baked-gi-bake-quality.md)). |
| `bake_source_missing` | A file the description names cannot be read. |
| `bake_mesh_load` | A mesh source does not load (missing, not glTF or `.fckd`, import error). |
| `bake_manifest_missing_entry` | A file read under `root` is not in the manifest, or a mesh `source` lies outside `root` while a manifest is given. |
| `bake_manifest_mismatch` | A file's SHA-256 differs from the manifest. |
| `bake_layout` | The layout is empty or inconsistent, or `auto_bricks` cannot fit `max_probes` (8 probes per brick at least). |
| `bake_volume_stale` | `--check-volume`: the volume does not match the description (`reasons`). |
| `bake_volume_unreadable` | `--check-volume`: the volume file cannot be read or is not a valid `.fgiv`. |
| `bake_volume_too_large` | The volume would need more than 128 MiB in the renderer's one GPU storage binding (`plan.volume_bytes`). Coarsen `terrain_spacing` or `auto_bricks.spacing`, lower `max_probes`, or lower `moment_resolution`. |

## Upgrading descriptions

v0 descriptions (no `format_version`) still load. `regia upgrade-project` lists them as `bake` v0. With
`--write`, it adds `format_version: 2` and keeps the original as `<file>.bak`. v1 descriptions load too; `--write`
rewrites their version number to 2. v2 adds `sky_hdri` and `emitters` ([Bake light inputs](baked-gi-light-inputs.md),
whose page also lists the `bake_hdri_decode` and `bake_hdri_sun` errors) and the bake quality controls
([Bake quality controls](baked-gi-bake-quality.md)).

## Resumable bakes

**Since ADR-3095.** `regia bake-gi <scene> --checkpoint` (and `--from-scene <level> --bake --checkpoint`) checkpoints the
relocation's outcome and the main trace's probes beside the volume; run again, it continues and writes the same volume
byte for byte, on the CPU tracer and on the GPU. `level.export_bake` reports a resumable checkpoint of the level's
volume. See [Resumable bakes (checkpoints)](bake-checkpoints.md).
