# Public benchmark scene set

The scenes a published Regia Engine number may come from (`docs/master-plan.md` § 5.3 item 9).
No published number may depend on content that is not on this list. A row marked "not confirmed"
is excluded until someone reads the licence text and records it here.

`regia bench` runs this set (`book/agents/bench.md`). Fetched scenes are pinned to one commit of
Khronos glTF-Sample-Assets with a size and SHA-256 per file, in
`crates/regia_cli/src/commands/bench/scenes.toml`. Their licences and credits were read from each
model's own `metadata.json` at that commit on 2026-11-01.

| Scene id | Source | Licence | In the public set | Run with |
|---|---|---|---|---|
| `synthetic_cubes` | Regia Engine original: 10k cubes, 100 materials | Regia Engine's own code | yes | `regia bench synthetic_cubes` |
| `synthetic_atrium` | Regia Engine original: a pillared two-floor hall from boxes, 500 materials, 64 point lights, camera inside. It stands in for the layout of a large architecture scene; it has no textures | Regia Engine's own code | yes | `regia bench synthetic_atrium` |
| `synthetic_grass_1m` | Regia Engine original (ADR-2695): a terrain patch 80 m square with a deterministic field of 1,000,000 blades (10 x 10 grass cells, 625 clumps a cell, 16 blades a clump, fixed seed) through `regia_terrain`'s grass path at the ultra tier, wind on. Generated: no third-party content | Regia Engine's own code, generated, no third-party content | yes | `regia bench synthetic_grass_1m` (`--small`: 10,000 blades) |
| `template_npc_200` | Regia Engine original (ADR-2695): a plaza with 200 navigation agents walking two opposing flows on a fixed path, each animated by the crowd animator and drawn skinned from a blocky body on the demo rig. Generated: no third-party content, no fetched model | Regia Engine's own code, generated, no third-party content | yes | `regia bench template_npc_200` (`--small`: 20 NPCs) |
| `flight_helmet` | Khronos glTF-Sample-Assets, FlightHelmet (Gary Hsu, donated by Microsoft) | CC0 1.0 | yes | `regia bench flight_helmet` |
| `sci_fi_helmet` | Khronos glTF-Sample-Assets, SciFiHelmet (Michael Pavlovic, Norbert Nopper) | CC0 1.0 | yes | `regia bench sci_fi_helmet` |
| `corset` | Khronos glTF-Sample-Assets, Corset (Microsoft) | CC0 1.0 | yes | `regia bench corset` |
| `lantern` | Khronos glTF-Sample-Assets, Lantern (sbtron, Frank Galligan) | CC0 1.0 | yes | `regia bench lantern` |
| `toy_car` | Khronos glTF-Sample-Assets, ToyCar (Guido Odendahl, Eric Chadwick) | CC0 1.0 | yes | `regia bench toy_car` |
| `sheen_chair` | Khronos glTF-Sample-Assets, SheenChair (Eric Chadwick) | CC0 1.0 | yes | `regia bench sheen_chair` |
| `a_beautiful_game` | Khronos glTF-Sample-Assets, ABeautifulGame (MaterialX Project, Ed Mackey) | CC BY 4.0: credit required | yes, with credit | `regia bench a_beautiful_game` |
| `fox` | Khronos glTF-Sample-Assets, Fox (PixelMannen; rig and animation tomkranis, AsoboStudio, scurest) | CC0 1.0 (mesh), CC BY 4.0 (rig, animation): credit required | yes, with credit | `regia bench fox` (bind pose) |
| `cesium_man` | Khronos glTF-Sample-Assets, CesiumMan (Cesium) | CC BY 4.0, Cesium trademark terms: credit required | yes, with credit | `regia bench cesium_man` (bind pose) |
| `synthetic_100k` | Regia Engine original: 100k instances, 500 materials | Regia Engine's own code | yes | `render_lab stress_100k` |
| `synthetic_lights` | Regia Engine original: many clustered lights | Regia Engine's own code | yes | `render_lab many_lights` |
| `sponza_khronos` | Khronos glTF-Sample-Assets, Sponza (Crytek) | Cryengine Limited License Agreement | **no**: local tests only, never published or redistributed | `regia bench --restricted sponza_khronos`, `render_lab sponza` |
| `damaged_helmet` | Khronos glTF-Sample-Models, DamagedHelmet (theblueturtle_) | CC BY-NC (non-commercial) | **no**: local tests only | `cargo xtask fetch-test-assets`, then a model view |
| `sponza_intel` | Intel graphics research sample | not confirmed | no, until confirmed | - |
| `bistro` | Amazon Lumberyard Bistro | not confirmed | no, until confirmed | - |
| `synthetic_forest` | Regia Engine original: 100 000 procedural trees (tapered trunk, 900 bent leaf cards with a masked six-leaf alpha, no textures from outside), the crown cooked with its alpha-tested voxel far levels (ADR-3108), on a jittered 7 m grid | Regia Engine's own code | yes | counted table: `cargo test --release -p regia_render --test visbuffer_dense_bench forest_cost_table_counts -- --nocapture`; card run: `cargo test --release -p regia_render --test visbuffer_dense_bench forest_voxel_far_bench -- --ignored --nocapture` (`REGIA_FOREST_INSTANCES`, `REGIA_FOREST_TIERS`, `REGIA_VIS_BENCH_FRAMES`) |
| `hlod_scatter_forest` | Regia Engine original: an 8 x 8-cell streamed world (128 m cells, 1024 m terrain) with two generated tree classes as terrain scatter, voxelized into one voxel proxy per HLOD node (ADR-3253) | Regia Engine's own code | yes | `cargo test -p regia_asset_pipeline --lib hlod_scatter_bench -- --ignored --nocapture` (table below) |

**No textured Sponza-class scene is in the public set yet.** The textured scene of that class we can
fetch is `sponza_khronos`, and its licence keeps it local. `sponza_intel` and `bistro` are the candidates:
someone has to read their licence text at the source and confirm a download in a format the bench loads
(glTF or GLB) before either joins. Until then `synthetic_atrium` carries the layout and light load and
`a_beautiful_game` the texture load.

The two rows of § 5.3 item 9 that needed new content, the 1M-blade grass synthetic and the
third-person-style template with 200 NPCs, are built (ADR-2695) as generated scenes: nothing is fetched,
the seeds and paths are fixed, and each row carries counters beside its timings (`counters` in
`regia bench --json`): the blades the renderer reports and the wind clock for the grass; the skinned
draws and the animation updates for the crowd. `--small` runs both at a size a software rasterizer can
do. The crowd is the engine's own crowd code (`regia_nav` agents, `regia_animation::CrowdAnimator`), not
the Fox of the `third_person` sample, which is fetched content.

The set is fetched, never committed. `scripts/fetch-bench-scenes.sh` (`regia bench --fetch`) downloads
into the gitignored `.regia/bench-scenes/`, checks every file, and writes `LICENSES.txt` beside the
scenes. `cargo xtask fetch-test-assets` is the separate cache of the golden tests: its labels are
pinned by a test (`restricted_models_are_not_labelled_cc0`), since before 2026-10-30 it labelled Sponza
and DamagedHelmet as CC0, which their READMEs do not say.

Every published number names its scene id, the card (exclusive, under its lock), the load average
and the build, and goes into `docs/perf-log.md`.

## HLOD scatter voxel proxies: 8 x 8-cell generated forest (ADR-3253)

`hlod_scatter_forest`: 7371 trees (two classes of a 300-triangle synthetic tree, 16 m placement cells, patchy noise
thinning) over a 1024 m terrain cut into 8 x 8 streaming cells of 128 m, one authored cube in one cell. HLOD tree:
64 leaves, 85 nodes in 4 levels. Every node gets a voxel proxy of the trees under it. Measured on the test machine
(Ryzen 9950X3D, `dev` profile, which is optimized in this workspace; one cook per row, shared host, so the seconds are
indicative only). Triangles are the voxel-box faces of the proxies the HLOD cut draws from a viewer 256 m, 1 km and
4 km west of the world on its center line (`HlodTree::select`, CPU count, no GPU).

| Voxels per node edge | Cook (s) | Voxel bytes, all nodes | Leaf proxy: KiB / voxels / voxel edge | Root proxy: KiB / voxels / voxel edge | Drawn at 256 m: proxies / triangles | at 1 km | at 4 km |
|---|---|---|---|---|---|---|---|
| 16 | 15.4 | 0.88 MiB | 10.1 / 312 / 8.3 m | 13.0 / 453 / 64.4 m | 36 / 116 172 (+4 full cells) | 10 / 41 492 | 1 / 3 074 |
| 32 (default) | 16.7 | 2.70 MiB | 30.0 / 924 / 4.2 m | 47.8 / 1 686 / 32.2 m | 36 / 315 234 (+4 full cells) | 10 / 128 158 | 1 / 14 714 |
| 64 | 19.2 | 8.64 MiB | 96.2 / 3 139 / 2.1 m | 175.1 / 6 151 / 16.1 m | 36 / 977 326 (+4 full cells) | 10 / 351 306 | 1 / 60 698 |

Middle levels at 32: level 1 39.1 KiB and 1 283 voxels per node (8.2 m voxels), level 2 43.9 KiB and 1 532 voxels
(16.2 m). Most of the cook time is placing and posing the instances and voxelizing the root over every instance; the
voxel resolution moves it little. The drawn triangle counts are voxel-box faces (up to 12 per occupied voxel), not
source triangles: the full forest is 7371 x 300 = 2.2 million triangles at every distance without the proxies.
