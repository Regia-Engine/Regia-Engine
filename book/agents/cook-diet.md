# Cook diet rules

A cook diet slims what a build ships without touching the source assets (ADR-8461). It caps texture
edges by path glob, holds a build profile's meshes under a triangle ceiling by dropping their finest
LODs, and keeps one member of each group of asset variants. The rules live in `Regia.toml`:

```toml
[content.diet]
texture_caps = [
    { glob = "**/props/**", max_edge = 2048 },   # the first glob that matches wins
    { glob = "**/damage/**", max_edge = 2048 },
]
default_max_edge = 4096        # textures no glob matches; 0 = no cap
max_triangles = 1500000        # a profile's ceiling over every shipped mesh; 0 = none
variants = [{ glob = "props/rocks/*", pick = "seeded" }]   # or pick = "first"
seed = 0                       # the seed of the seeded picks
path_aliases = [{ from = "old_tree/", to = "new_tree/" }]  # read an old prefix as a new one

[profiles.mobile.diet]         # a build profile's diet replaces the keys it names
max_triangles = 400000
seed = 7
```

Without a `[content.diet]` or `[profiles.<name>.diet]` table nothing is capped, dropped or picked.
A table that names no key takes the defaults above (DO27's values: 4096 texels, 1 500 000
triangles). Globs match the path under `assets/` (`*`, `**`, `?`; ASCII case ignored; `**/props/**`
matches a whole `props` folder at any depth, never `myprops`).

## Texture caps (in the cook)

`regia cook` box-filters every texture a source cooks until its largest edge is at most the source's
cap, after the texture's own `.meta` `max_size` (the smaller wins). A glTF's material maps take the
glTF's cap, so a damage-stage model's maps can be smaller than the intact model's. The cook cache
keys on each source's cap, so changing a cap re-cooks only the sources it covers. A profile's
texture keys are reported but do not re-cook: the cook has no profile.

## The triangle ceiling (in the profile's pak)

`regia package --profile <name>` counts each mesh of the profile's closure at its finest LOD. While
the total is over `max_triangles`, the mesh with the most triangles at its current level (ties: the
smaller path, then the smaller id) loses that level, and the pak ships it rewritten: its coarser
levels only, its index buffer compacted, its submesh moved to the new finest level. A mesh's last LOD
is never dropped, and a mesh with more than one submesh is never touched (`diet_mesh_pinned`). When
no mesh can lose a level the total stays over the ceiling (`diet_ceiling_unreached`).

## Variant picks (in the profile's pak)

A `variants` glob's matches are grouped by folder (a sub-asset goes with its file). `first` keeps the
first path in byte order; `seeded` keeps member `fnv1a32(folder) % members` with `seed = 0`, and
hashes the seed's eight little-endian bytes before the folder otherwise. The same rules, seed and
files always give the same picks. The other members leave the pak, unless a shipped asset depends
on one: that member ships and the report says so (`diet_variant_referenced`).

## The report: `assets.diet_report`

`regia package --profile <name>` writes `build/profiles/<name>/paks/<platform>/diet-report.json`
(not shipped) and prints one summary line. Read it with:

```json
{"method":"assets.diet_report","params":{"profile":"release","platform":"linux"}}
```

`platform` defaults to the running asset server's. The result:

```json
assets.diet_report { "profile": "release", "platform": "linux" }
→ {
  "version": 1, "path": "build/profiles/release/paks/linux/diet-report.json",
  "profile": "release", "platform": "linux", "max_triangles": 1500000, "seed": 0,
  "ceiling_reached": true,
  "before": { "assets": 120, "bytes": 81234567, "triangles": 1550000 },
  "after":  { "assets": 118, "bytes": 70123456, "triangles": 850000 },
  "textures": [{ "path": "assets/props/crate.png", "max_edge": 2048, "rule": "**/props/**",
                 "width": 2048, "height": 2048 }],
  "lods": [{ "path": "assets/hulls/a.glb", "id": "…", "levels": 3, "dropped": 1,
             "triangles_before": 900000, "triangles_after": 200000 }],
  "variants": [{ "rule": "props/rocks/*", "pick": "seeded", "group": "props/rocks",
                 "members": ["…"], "picked": "…", "left_out": ["…"], "kept_referenced": [] }],
  "warnings": []
}
```

Warnings start with their code: `diet_ceiling_unreached`, `diet_mesh_pinned`,
`diet_variant_referenced`, `diet_texture_over_cap` (a cooked texture above its cap: re-cook).

### Errors

| Code | Reason | What to do |
|---|---|---|
| F0002 `diet_report_bad_param` | `profile` is missing, or a profile or platform name is not letters, digits, `_` and `-` | Pass a `[profiles.<name>]` of `Regia.toml` |
| F0016 `diet_report_not_written` | The profile's pak has no report for that platform: no diet, or not packaged yet | Add a diet table and run `regia package --profile <name>` |
| F0013 `diet_report_unreadable` | The file does not parse, or a newer build wrote it | Package the profile again with this build |

A malformed diet table fails `regia cook` and `regia package` with `diet_invalid`, naming the key;
a cooked file the diet cannot read fails the package with `diet_cooked_unreadable`.

## The catalog-path rule

`path_aliases` lets a project rename an asset tree without rewriting every catalog row at once: a
path that starts with `from` is read as starting with `to` before any glob matches, and
`regia_asset_pipeline::cook_diet::collect_catalog` resolves catalog rows the same way (a row whose
file exists under neither name is a stale note, not an error). Only a leading prefix is rewritten.

## Verify

- `cargo test -p regia_asset_pipeline --test cook_diet`: the ceiling (`lod_dropped_over_1_5m_triangles`,
  `diet_never_drops_the_last_lod`), `texture_cap_first_match_wins`, `variant_pick_is_deterministic`,
  the pak plan over a cooked cache, the cap in a real cook, and DO27's three path tests.
- `cargo test -p regia_cli --bin regia a_profile_diet_trims_lods_and_variants_in_the_pak`: the pak.
- `cargo test -p regia_agent --lib diet_report`: the method through the dispatcher.
