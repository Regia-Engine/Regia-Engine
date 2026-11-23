# v0.5 starter-content review — X06

Status: owner-ready inventory; scope remains unplaced. No starter pack is packaged.
Reviewed candidate: `75e1a779454ac96ab40810c1adb3081d4de7a6b9` (v0.2.19 candidate,
not a passed release). The owner-placement target in the November 8 burn-down
is November 12. This review completes the inventory fallback of that brief;
it does not accept X06 as shipped or advance an owner milestone gate.

[v05-candidates.toml](v05-candidates.toml) records every selected source's SHA-256,
byte length, current license reference, last source-change commit, provenance,
project-relative destination, template IDs and integration caveat. Its scope is
all five tracked files beneath the templates' `assets/` directories, plus three
explicit sample sources. It is not an inventory of every asset in the repository.
The TOML is review data; the engine does not consume it. It does not replace the
versioned `pack.ron`, `licenses.ron` or BLAKE3 provenance sidecars specified by
[spec 61](../specs/61-starter-content-and-sample-game.md).

## Proposed minimal selection

| Sources | Reuse | Destination in a generated project | Limit |
|---|---|---|---|
| `room_mesh` | Self-contained glTF room geometry and one flat material | `assets/starter/meshes/room.gltf` | Host must instantiate it; no texture, collider, prefab or baked lightmap included. |
| `wind_bed`, `music_pad` | Procedural ambient wind and a quiet chord pad; no recordings | `assets/starter/audio/ambient.loopgen.ron`, `assets/starter/audio/music.loopgen.ron` | Host must render and play the definitions with the audio runtime. |
| Five existing template levels | Preserve the baseline and its existing import paths | `assets/levels/main.level.ron`, or `assets/levels/arena.level.ron` for net-game | Existing template files, not additional pack payload. Only transforms, or an empty entity list. |

The three proposed pack sources total **5,043 bytes**. All eight inventory rows
total **7,485 bytes**; seven distinct SHA-256 payloads total **6,699 bytes**.
Blank and first-person have identical level bytes and separate destinations.
Their entity GUIDs also coincide: do not combine these scenes as additive levels
without deliberate identity handling. Do not overwrite an existing project's
startup level when adding content.

These sources offer a small geometry/audio seed set. They do not satisfy spec
61's full materials, props, rigged character, fonts, UI, prefab or sample-game
targets. Existing template geometry, gameplay and synthesized cues also live in
Rust code; this inventory does not extract that code into new assets. `quick`
has no tracked asset files and no ordinary project asset destination in this
review. Net-game's two IDs select its existing scene; no Android client shim or
audio/mesh integration is inferred from that row.

## Rights and provenance

The selected files are repo-authored candidates: the glTF identifies its
hand-written fixture generator, the loops retain their original authoring
comments and seeds, and template membership is explicit in
`crates/regia_templates/src/lib.rs`. Git source commits and exact bytes are
preserved for review. This supports an origin assessment; it is not an independent
legal certification of ownership.

Licence (ADR-10225, 2026-11-20): template, sample and starter code is MIT-0
(`crates/regia_templates/templates/LICENSE-MIT-0.txt`, `samples/LICENSE-MIT-0.txt`) and
starter assets are CC0-1.0 (the `LICENSE-CC0-1.0.txt` beside them); the engine itself is
MPL-2.0 ([LICENSE](../../LICENSE)). The proposed `LicenseRef-Regia-Starter` of spec 61 is
withdrawn. The inventory below predates that decision and still names the earlier
repository licence file as its reference; that file no longer exists.

All rows therefore retain the current repository-license reference and remain
**not cleared as a standalone starter pack**. `packaging_authorized = false` and
`package_assets = []` are intentional. Retain the original comments, license text
and applicable generated notices if the owner later authorizes a package. No
license text or permission is invented here; the licence is now settled (MIT-0 code,
CC0-1.0 assets, ADR-10225), and packaging still waits for the owner's go.

## Explicit exclusions and coordination

- No DO27 repository, private game content, supplier data, costs, catalogue data,
  external asset directories, local caches, binary downloads or source symlinks.
  Only exact tracked Regia source paths named in the inventory are eligible.
- No Fox model or fetch-at-first-run behavior from `samples/third_person`.
  Selecting its two audio definitions does not select that sample's whole tree.
- No fonts or other third-party assets, even ones already present in the repo.
  A later font addition needs its own exact source, hash and original license text.
- No procedural biome/scatter graph closure, arena/combat fixture set or sample
  game. Their host registrations and content dependencies need separate review.
- No `lightmap_room` level: it names `maps/room.flmap`, which is generated and not
  checked in. The selected glTF has an embedded data buffer, an inline material
  and no external buffer/image dependency. It does not depend on that level.
- No showcase source or demo asset is selected. The required v0.5 free-camera
  showcase, its asset manifest, D01–D05 packaging and fresh-install acceptance
  remain separate under [the showcase plan](../showcase-plan.md). The existing
  demo-package branch's `asset_manifest.json` has its own sample-license labels;
  those labels do not clear reusable starter assets.

The J08 demo-package report currently stops on unintegrated demo-shell/package
prerequisites. J11 owns the release decision packet; public-release-scrub owns
its release snapshot scrub; J16 owns template/upgrade acceptance. This review
does not edit their manifests, duplicate their implementation or claim their
acceptance results. The existing `b34-starter-content` and `starter-pack-recut`
deliveries are historical implementation leads, not permission to rebuild or
merge their pack. The lead should reuse their work only after review and an
explicit integration decision.

## Cook and runtime assumptions

These are required follow-up checks if the owner places the pack, **not platform
passes from this documentation review**. Destinations are normalized, lowercase,
project-relative paths. The four desktop/Android 3D templates can accommodate
these ordinary source extensions; copying a file does not register a mesh,
start audio or prove it loads through a packaged asset registry.

| Target | Cook/import assumption | Runtime evidence still required |
|---|---|---|
| Linux | Desktop pipeline imports glTF 2.0 and retains the inline material. Generate fresh `.meta` identities for these candidate copies, then preserve them in a real pack. Legacy v1 template scenes use the existing level migrators. | Generated-project load and packaged registry/pak resolution; visible mesh and audible/agent-observable loops. No source-tree fallback. |
| Windows | Cook on the supported desktop pipeline; paths retain exact case and portable separators. Re-cook for the target rather than treating Linux cooked data as universally portable. | Native build/load and packaged resolution; matching source hashes and deterministic audio sample hashes, with real audio backend smoke. |
| Android aarch64 | Cook on a desktop, ship runtime content only. No `regia_editor`, `regia_asset_pipeline` or `regia_templates` runtime dependency. glTF has no textures, so no BC7/ASTC conversion is implied here. | Runtime build and a short test on dedicated TEST Pixel 9 Pro `54101FDAP0015H`; lifecycle/audio restart and pak access without host paths. No low-end device claim. |

The existing sample renders both loops in memory using `LoopGenDef`; it does not
establish an Opus/Vorbis cook path for these definitions. At 48 kHz/f32, the wind
bed's six seconds of stereo PCM occupies 2,304,000 bytes and the eight-second
mono pad occupies 1,536,000 bytes: **3,840,000 bytes combined**, before graph,
cache and playback overhead. Source size is not runtime memory or cooked size.
Do not render on the frame/audio callback. The third-person template already has
an ambient bed; replacement or mixing is a deliberate integration step.

After placement, use the normal source/import/cook path in [spec 12](../specs/12-assets.md),
create the spec 61 manifests and provenance sidecars with stable IDs, measure
cooked budgets, and preserve the frozen formats. Any format change still needs
its version, migrator and `regia upgrade-project` support. macOS is testing-only.

## Validation and acceptance boundary

Test-first gap check: `test -s docs/starter-content/v05-candidates.toml` failed
before the inventory existed. Document-data validation then checked the exact
source allowlist before opening any source, tracked-file completeness for all
five template assets, path normalization/no symlink escapes, all source/license
hashes and lengths, pinned Git bytes, source-commit ancestry/bytes, license
references, template destinations and the glTF's embedded dependency closure.
The selected text was screened for private/game/supplier/path/download markers;
that screen supplements the explicit allowlist and manual source inspection.

Twenty in-memory negative cases cover a missing template row, unknown source,
parent escape, absolute Unix/Windows paths, DO27/supplier/private-path
substitution, source case drift, duplicate ID or template destination, altered
hash/bytes, missing license reference, invented permissive license, malformed
TOML, invented platform passes and accidental packaging authorization. No
on-disk asset was changed to exercise them. This is a selected-source review,
not a repository-wide private-content scrub.

From the repo root, this cheap check rechecks the exact license/source bytes:

```sh
awk -F '"' '
  /^(source|path) = "/ { path = $2 }
  /^sha256 = "/ { print $2 "  " path }
' docs/starter-content/v05-candidates.toml | sha256sum --check --strict -
git diff --check
```

Review the source allowlist before running a hash command on an edited inventory;
do not follow arbitrary new paths. A source change requires a fresh provenance
and dependency review before updating its pin. No cargo build, runtime smoke,
agent screenshot, native platform test or pack installer test was run here:
there is no packaging code, runtime feature or asset modification. The sprint
report records the actual audit results and remaining lead commands.

## Questions for owner

1. By November 12, keep broader X06 starter-game work unplaced, or place the
   three-source geometry/audio seed set? The required showcase remains required
   in either case. Inventory completion does not authorize packaging.
2. If placed, approve the exact asset list and ownership assessment, supply the
   accepted starter-content license text and credits, and identify the approved
   distribution audience. Until then, no standalone pack is staged.
3. Should the lead integrate and finish the earlier `starter-pack-recut` work,
   or keep it deferred? Its placeholder terms and incomplete verification are
   recorded in its report and must be resolved before reuse.
