# Open kit: view Tincal kits

The editor opens the vehicle, prop and character kits Tincal exports for modders. A kit is a
folder; **File ▸ Open kit…** (or **Open kit…** on the project browser, which needs no project)
shows it in the **Kit** panel: the model with its LOD chain, the cockpit at its socket, the clips,
the sockets, the `kit.json` metadata and the `SHA256SUMS` result.

## The kit folder

```text
jmc_hauler_v3/
  kit.json                      the manifest
  jmc_hauler_v3.glb             the model: LOD0 to LOD3, sockets, clips
  cockpit_jmc_heavy.glb         optional: cockpit_<faction>_<class>.glb
  rig.json                      optional: Tincal's rig sidecar (clip names and lengths)
  sockets.json                  optional: Tincal's socket list
  SHA256SUMS                    optional: `sha256sum` lines for the files above
  textures/x.png                optional: other files in subfolders, listed in SHA256SUMS
```

`kit.json`:

```json
{
  "kit_id": "jmc_hauler_v3",
  "kind": "hull",
  "class": "heavy",
  "faction": "jmc",
  "replaces": "",
  "stats_from": "jmc_hauler",
  "contract": "production-plan 2026-10-18",
  "version": 3
}
```

`kit_id`, `kind`, `faction` and `version` are required. `kind` is one of `hull`, `prop`,
`structure`, `pilot`, `weapon`; `class` one of `light`, `battle`, `heavy`, `walker`, `utility`,
`apc`, `producer`, `turret`, `structure`; `faction` one of `rem`, `a52`, `tru`, `jmc`, or `generic` for shared assets (weapons, props;
a cockpit would be `cockpit_generic_<class>.glb`). Another
value, an unknown key, an empty `stats_from` or a `kit_id` that differs from the folder name opens
the kit with a warning.

## What the Kit panel shows

- **Metadata:** every `kit.json` field, and whether the model is a character kit (the skin's
  skeleton root carries the glTF extras `{"tincal_kit": 1}`; read when present, never required).
- **SHA256SUMS:** `verified` (green) when every listed file matches, `FAILED` (red) when one
  differs, is missing or was refused, `no SHA256SUMS` when the kit has none. Expand **Checksums**
  for each file's result. Files the list leaves out are warnings.
- **Warnings:** everything that did not stop the kit from opening, one line each, naming the file.
- **Clips:** the model's glTF animations with their lengths, then the `rig.json` clips the model
  has no animation for (marked `rig.json only`). Click a clip to play it in the **Clip** panel
  (the animation preview); the model becomes the preview rig when it is skinned.
- **Sockets:** the model's `socket_*` nodes merged with `sockets.json`, with their positions and
  where each came from. **Socket gizmos** draws them over the render; click one to highlight it.
- **Budget:** the model's LOD0 measured against a class's limits (the class comes from `kit.json`
  and can be changed in the panel), one meter per limit plus the LOD chain
  ([budget meters](../agents/editor-budget.md)).
- **Render:** drag to orbit. The LOD buttons switch levels: an authored chain (mesh nodes named
  `..._LOD0` to `..._LOD3`) when the model has one, otherwise the viewer's generated chain (the
  asset viewer's simplifier, marked `generated`). The cockpit is drawn at the model's
  `socket_cockpit` node; without that node it sits at the origin and a warning says so.
  The editor draws the hull and the cockpit both and does not hide nodes, so when a cockpit glb is
  shipped the hull glb must not also contain the static cockpit geometry, or it shows twice.

## What the editor refuses

A kit folder comes from someone else, so the viewer reads it defensively:

- It reads only regular files inside the folder. `SHA256SUMS` may list a subpath such as
  `textures/x.png` (forward slashes, every listed file is checksummed); `kit.json`, the model, the
  cockpit, `rig.json` and `sockets.json` stay directly in the folder. A name with `..`, a leading
  `/`, a backslash, a drive letter or colon, a NUL or an empty component (`a//b`), a symlink (or
  linked subfolder) that leaves the folder, a dangling symlink, or a folder where a file should be
  is refused and reported, never followed.
- Every read is capped: `kit.json` 64 KiB, `rig.json`, `sockets.json` and `SHA256SUMS` 4 MiB each,
  a GLB 1 GiB (and its JSON chunk 64 MiB). A GLB whose header claims more bytes than the file has
  is refused before anything is allocated.
- A GLB must embed its data: a buffer or image that points at an outside file is refused.
- Without `kit.json` or `<kit_id>.glb`, or with either unsafe, too large or malformed, the kit does
  not open and the panel shows why. Every other problem (a bad `rig.json`, a checksum mismatch, a
  cockpit that does not load) is a warning and the rest of the kit still shows.

## From code

<!-- ignore-reason: uses `regia_editor`, which is desktop-only and not a dependency of the book's compile check -->
```rust,ignore
# use std::path::Path;
use regia_editor::kit::{open_kit, ChecksumVerdict};

let kit = open_kit(Path::new("kits/jmc_hauler_v3"))?;   // KitError on a refused folder
println!("{} v{}", kit.manifest.kit_id, kit.manifest.version);
if kit.checksums.verdict != ChecksumVerdict::Verified {
    for warning in &kit.warnings {
        eprintln!("{warning}");                         // "rig.json: checksum mismatch: ..."
    }
}
```

`regia_editor::kit_view::open_kit_in_world` opens a kit into the editor's `KitView` resource
(meshes, preview rig) the way the menu does. Agents use `editor.open_kit`
([agent page](../agents/open-kit.md)).

## Not covered

The viewer does not check the kit-test contract (scale, budget table, node and clip names);
Tincal's checker and DO27's kit-test mode do. It does not write kits or package anything.
