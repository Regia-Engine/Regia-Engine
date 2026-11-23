# Upgrading a project to v0.7 (format batch 2)

This guide covers what changes for a project that was made with v0.5.0 or v0.6.0 when it moves to v0.7. v0.7 is the
format window called batch 2 (`docs/specs/12-assets.md` § 4a, ADR-7724, ADR-10183). Each frozen content format moved
at most one version step from what v0.6.0 shipped. Every step has a migrator, and every old file still loads.
`cargo xtask check-formats` checks the one-step rule against `docs/formats/format-hashes-v0.6.0.ron`.

## 1. The steps

1. Commit or back up the project. `regia upgrade-project --write` keeps a `<file>.bak` next to each file it rewrites.
   `--backup <dir>` also copies the whole project's source files to a dated folder before anything changes.
2. Dry run: `regia upgrade-project --project <dir>` (add `--json` for one machine-readable document). It lists every
   file it recognizes with its version and one of three statuses: `current`, `migrate` (an older version this engine
   upgrades) or an error (a newer version, or a file that does not parse).
3. Upgrade: `regia upgrade-project --project <dir> --write`. A second `--write` changes no byte. Add
   `--engine <source>` to move the project's engine dependency in the same run (spec 94 § 3.3).
4. Cook again: `regia cook`. Several cooked caches moved (section 3), and a v0.7 cook writes them at the new version.
5. Rebuild every client and server together. The net wire is version 6 (section 4), and v0.6 peers cannot join v0.7
   peers.

Files v0.7 writes cannot be read by v0.6.0. For example, the v0.6.0 loader refuses a v3 `.decal.ron` with
`decal prefab has version 3, but this engine reads version 2`. Do not hand upgraded files back to a v0.6 build.

## 2. Source files `regia upgrade-project --write` rewrites

| Format | v0.6.0 → v0.7 | What changes | What to do |
|---|---|---|---|
| `scene` (`.level.ron`, `.prefab.ron`) | 13 → 14 | A level's terrain block gains `sample_layout` (`VertexCentered`, the old meaning, or `CellCentered`), ADR-10079 | Run `--write`. A level whose heightfield is cell-registered (one sample per cell center) can now say `sample_layout: CellCentered` instead of correcting the spacing in host code |
| `decal` (`.decal.ron`) | 2 → 3 | Optional custom `texture` and optional footstep `surface` (ADR-8121, ADR-8829) | Run `--write` (the version number is the only change) |
| `logic` (`.logic.ron`) | 1 → 2 | Optional typed `entity_vars` and `world_vars` (ADR-8900) | Run `--write` |
| `uianim` (`.uianim.ron`) | 1 → 2 | Optional frame `events` per animation (ADR-8900) | Run `--write`. A file with no `format_version` is saved again at v2 |
| `playlist-ron` (`.playlist.ron`) | 1 → 2 | Same shape. Unknown fields inside `order` are refused (ADR-10064) | Run `--write`, then fix any field the stricter reader names |
| `net-settings` (`config/net.ron`) | 3 → 4 | Optional `session.rate_control.first_flight` (the rate controller's first flight, opt-in and off by default; ADR-10156, ADR-10231) | Run `--write` (the version number is the only change; a v1 to v3 file also loads with the default) |
| `terrain-mesh-layers-cooked` (`<map>.meshlayers`) | 1 → 2 | The canonical section stores its counts at a fixed width behind a hashed head (ADR-8816). The chunk section gains each layer's `collider` (ADR-8872) | `--write` rewrites a v1 file, or `regia cook` writes it again. `content_hash` is unchanged for the same map, so a v1 file and its v2 rewrite pass the lockstep match handshake together |
| `terrain-index` (`terrain/index` in a cooked map) | 2 → 3 | Two optional entries: the source grid's Z spacing and a valid-data mask (ADR-8871) | `--write` migrates a cooked index. A full `regia cook` writes v3. An incremental cook keeps an unchanged terrain's v2 index, which still loads |

New formats in v0.7, all at v1, with nothing to migrate: `rules-ron` (`.rules.ron`), `match-result` (finished-match
JSON), `gait` (`.gait.ron`), `lsreplay` (`*.lsreplay`), `netcapture` (`*.netcapture`), `release-signature`
(`manifest-<platform>.json.sig`) and `hub-toml` (`hub.toml`). `upgrade-project` lists them and refuses a newer version.

## 3. Caches and platform data that are cooked again, not rewritten

| Format | v0.6.0 → v0.7 | What to do |
|---|---|---|
| `nav-cell` (navigation cells inside streamed cell blobs) | 2 → 3 | Re-cook: `regia cook` writes v3 cells on every run. v1 and v2 cells still load with a warning. Their heights come from the old center-sampled rasterizer, so thin walls and planks may be missing, and a runtime region rebuild next to such a cell can disagree at the seam (ADR-8899). Navigation determinism pins move |
| `gpu-mesh` (the GPU-ready mesh in desktop paks) | 2 → 3 | Re-cook the paks: the next cook writes v3, which stores skinned and morphing meshes GPU-ready (ADR-10080). v1 and v2 entries still load |
| `lobby-keys` (the `regia.*` lobby data) | 1 → 2 | Nothing: two optional keys, `regia.mode` and `regia.playlist` (ADR-8897). A v1 lobby reads unchanged |

## 4. Not files: the net wire

`REGIA_NET_WIRE_VERSION` is 6 (5 in v0.6.0). Wire 6 adds the one-round-trip resume.
Peers on different wire versions refuse each other at the handshake, so ship clients, listen servers and dedicated
servers from the same build. Captures (`.netcapture`) and lockstep replays (`.lsreplay`) belong to the protocol that
wrote them, and nothing rewrites them.

## 5. What DO27 must cook again

DO27 moves onto v0.7. These are the items its content is affected by. Run `regia upgrade-project --write` on the DO27
project first, then a full `regia cook`.

- **Navigation cells v3** (`nav-cell`): re-cook every streamed level so its cells use conservative rasterization.
  Re-cut the navigation determinism pins after the re-cook.
- **The mesh-layer index section** (`terrain-mesh-layers-cooked` v2): the canonical section's fixed-width counts.
  `--write` or the cook rewrites every `<map>.meshlayers`. Every peer of a lockstep match must load the same cook. A
  v1 file and its v2 rewrite have the same `content_hash`.
- **TerrainMap spacing** (`scene` 14): DO27's cell-spaced terrain can set `sample_layout: CellCentered` in its level
  files, or call `TerrainMap::with_sample_layout(SampleLayout::CellCentered)` from host code (ADR-10079). Either one
  replaces the ADR-8179 spacing arithmetic. `--write` gives every existing level `VertexCentered`, so nothing moves
  until DO27 chooses the layout.
- **GPU mesh v3** (`gpu-mesh`): re-cook the desktop paks.
- **Decals v3** (`decal`): `--write` re-stamps every `.decal.ron` (the version number is the only change).
- **Terrain index v3** (`terrain-index`): a full cook writes it. Maps with unequal X/Z spacing, or with samples that
  hold no data, need that cook to use the new entries.

## 6. Checking a project

- `regia upgrade-project --project <dir> --json` after `--write` should list every file as `current`.
- The engine's own check runs the same migrators over the old-bytes corpus as a v0.5.0 project and as a v0.6.0
  project: `a_v050_project_upgrades_to_the_current_formats` and `a_v060_project_upgrades_to_the_current_formats` in
  `crates/regia_cli/tests/format_corpus_upgrade.rs`.

## 7. Formats that were not on the line when the freeze ran

These rows were planned for batch 2 but had not landed when the freeze ran (ADR-10183). Each one either lands with
its own one-step row before the v0.7 tag, or waits for batch 3 (v0.8):

- the Stage B sim-version step (`MATH_BITS_VERSION`, lane v07-det-atan2-canon),
- `server-toml` 4 → 5 (only if the `--relay` off switch lands, lane v07-net-ki-hosting-lobby),
- `bake` (`.bake.ron`) 3 → 4 (only if the GI export ADR picks a shape change, lane v07-meshlayers-gi-export).
