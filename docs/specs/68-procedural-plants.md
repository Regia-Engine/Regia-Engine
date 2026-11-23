# Spec 68: Procedural plants (growing trees and bushes from rules)

Status: proposed (ADR-1596). Part of the procedural content graph (spec 62). Behavior written from the public-sources spec
set, area `runtime-procedural` (set spec `runtime-procedural-vegetation`, ADR-1526, behavior only). That set spec cites a
single page (ADR-1526 § 5 lists it as thinly sourced), so the growth model here is taken from the published plant-modeling
literature cited in § 13, not from the set; the set only confirmed which workflow stages users expect. All names, node
ids, settings and defaults are Regia's own.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: nothing of this spec is built (`regia_procgen` is created by spec 62). Built and reused: the tree glTF
conventions (`docs/formats/gltf-conventions.md`, `regia_asset_pipeline::gltf_conventions`), `meshopt 0.6` (workspace dependency
of `regia_asset_pipeline`), the CPU impostor bake in `regia_terrain::scatter::impostor`.

## 1. Purpose

Forests need many distinct trees and bushes. Today Regia imports tree glTFs made elsewhere (`book/agents/scatter-forest.md`:
`_LOD<n>` nodes, twig-card parts, billboards, `_WIND`/`_WIND_PIVOT` attributes). This spec adds an asset-pipeline generator
that **grows** plants from rules (light competition, branching pattern, tropisms), meshes them, dresses them with leaves
or twig cards, and exports glTF files in the conventions the existing tree import path reads (`_LOD<n>` nodes, parts,
extras), with any number of seeded variants.

Plant graphs are procedural graphs (spec 62) in a separate **plant domain**: same file format, editor, parameters,
variants, determinism rules and agent methods, with plant-only node types. They run in the pipeline (editor and CLI) only;
games ship the exported meshes, never the growth simulation.

What this spec takes from specs 62 and 63 (so it stands alone): a procedural graph is nodes with typed pins, settings and
a seed; data travels as **items** (a bundle of one payload kind plus tags) and every node is a deterministic function of
inputs, settings and seeds; `PointSet` payloads carry positions, a per-point seed and named attributes; `Shape` payloads are
boxes, spheres and capsules; graphs carry `flags { starter, tool, domain }` and are run by the evaluator
(`regia_procgen::exec`) with a cancel token and limits; per-element random draws come from a counter-based generator keyed
by seeds (spec 62 § 6.2) and all float math that affects output avoids platform-dependent transcendentals (`detmath`).

## 2. Domain and data kinds

- `ProcGraph.flags.domain: World | Plant` (spec 62 § 4; default `World`). The node registry marks each node type with the
  domains it supports: generic nodes (attributes, flow, `graph.call`, debug, `gen.*`, `sample.*` on shapes) work in both;
  `plant.*` nodes only in `Plant`; `in.*` world readers and `spawn.*` only in `World`. Using a node outside its domain is a
  compile error `node_wrong_domain` (reported to the agent inside `procgen_graph_invalid`, or as `procgen_wrong_domain`
when a run is requested). Plant node types are registered only when `regia_procgen`'s `pipeline` cargo feature is on
(spec 62 § 13); a build without it that is asked to run a `Plant` graph refuses with `plant_runtime_forbidden`
(agent: `procgen_plant_runtime_forbidden`).
- New data kinds (plant domain):
  - `PlantSkeleton` (named so it never collides with the animation `regia_scene::Skeleton`): per plant a tree of nodes; each node has `position` (m), `parent` (index or none),
    `radius` (m), `order` (0 = trunk, 1 = first branches, ...), `age` (growth step created), `light` (0-1, last exposure),
    `dir` (unit growth direction), `bud` state (`Active`, `Dormant`, `Dead`, `Bloom`), plus custom attributes. Plants in one
    item are independent trees (one per seed point).
  - `PlantMesh`: vertex and index buffers per material slot, plus per-vertex wind data (§ 7) and per-plant bounds.
- A `PlantSkeleton` previews as lines with radius tubes; a `PlantMesh` previews as the mesh. Only `PlantMesh` can be exported
  (`plant_not_meshed` otherwise).

## 3. Starting plants: `plant.seed`

Pins `in` (points; default one point at the origin), `out` (PlantSkeleton). Settings: `facing_deg` (initial yaw of the first
lateral pattern), `up: v3(0,1,0)`, `tilt_deg: [0, 0]` (random initial lean range), `start_radius_m: 0.01`. One plant per
input point; the point's seed becomes the plant seed. Typical sources: `gen.one`, `gen.grid` (a clump), `sample.poisson`
over a small disc (a bush cluster).

## 4. Growth: `plant.grow`

Grows skeletons in discrete **growth steps** with a bud-based, light-driven model (self-organizing tree model, § 13). Input:
`PlantSkeleton` (from `plant.seed`, a previous `plant.grow`, or `plant.attach`), so growth can run in stages with different
settings (for example a straight young trunk, then a spreading crown). Optional pin `obstacles` (Shape items) and pin
`envelope` (a Shape or Mesh that buds may not leave).

Settings (default), grouped:

| Group | Settings |
|---|---|
| Steps | `growth_steps: 12` |
| Shoot structure | `internode_m: 0.35`, `internode_decay: 0.92` (per order), `buds_per_node: 1` (1 alternate/spiral, 2 opposite, n whorled), `divergence_deg: 137.5` (between successive nodes; 90 with 2 buds per node gives decussate), `branch_angle_deg: 40`, `branch_angle_decay: 0.9` (per order), `max_order: 4` |
| Vigor and apical control | `vigor: 1.0`, `apical: 0.55` (share of resource sent to the main axis at each fork, Borchert-Honda λ), `min_vigor: 0.15` (buds with less stay dormant) |
| Light | `voxel_m: 0.25`, `shadow_depth: 6` (voxels), `shadow_strength: 0.6`, `shadow_spread: 1` (pyramid widening per voxel), `full_light: 1.0` |
| Tropisms | `up_pull: 0.08` (bend toward up per internode), `light_pull: 0.15` (toward the lighter neighbor voxel), `wander_deg: 6` (random per internode), `optimal_dir_weight: 0.3` |
| Shedding | `shed_below_light: 0.12`, `shed_after_steps: 3` (a branch whose total light stays below the threshold this long is removed with its children) |
| Flowering | `bloom_after_steps: None`, `bloom_chance: 0` (terminal buds that become `Bloom` stop growing; their nodes carry `bloom = true`) |

Per growth step, deterministic order (plants in item order, nodes in index order):
1. **Light.** Rasterize every node into a voxel grid (`voxel_m`); each node adds shadow to the voxels below it in a
   pyramid (`shadow_depth` voxels deep, widening by `shadow_spread`), strength decaying geometrically by
   `shadow_strength`. A bud's light = `max(0, full_light − shadow at its voxel)`. `obstacles` voxels are fully shadowed.
2. **Resource.** Light is summed from the tips to the root (each node's `light` = own + children). From the root, the
   plant's resource `vigor × root light` is split at every fork: the main (continuing) axis gets `apical × v_main /
   (apical × v_main + (1 − apical) × v_lat)` of it, laterals the rest, where `v` is the light collected by each child
   subtree (Borchert-Honda allocation).
3. **Bud fate.** A bud with resource ≥ `min_vigor` extends a shoot of `floor(resource / min_vigor)` internodes (capped at 4
   per step), each of length `internode_m × internode_decay^order`; otherwise it stays dormant (and dies after
   `shed_after_steps` dormant steps).
4. **Direction.** Each new internode direction = normalize(previous dir + `optimal_dir_weight` × direction to the most
   lit neighboring voxel × `light_pull` + up × `up_pull` + a random rotation up to `wander_deg` (draws keyed by
   plant seed, node index, step)). Buds never step outside `envelope` (they become dormant instead).
5. **New buds.** Each new node gets `buds_per_node` lateral buds at angle `branch_angle_deg × branch_angle_decay^order`
   from the axis, rotated by `divergence_deg` from the previous node's buds, plus a terminal bud. Buds above
   `max_order` are not created.
6. **Shedding.** Branches failing the shed rule are removed (whole subtrees).

Validation: `growth_steps` ≤ 64, nodes per plant ≤ 200 000 (`limit_exceeded`). The run report lists per step the node count,
active buds and shed branches.

Alternative growth mode `plant.grow_envelope` (space colonization, § 13): attraction points sampled in an `envelope`
(count, kill distance, influence distance, step length); nodes grow toward the average direction of points that list them
as nearest; points within the kill distance are removed; stops when no points remain or after `growth_steps`. Use it for crowns
with a designed silhouette (topiary, stylized trees).

## 5. Shaping the skeleton

| Id | Settings (default) | Behavior |
|---|---|---|
| `plant.radius` | `tip_m: 0.004`, `exponent: 2.5`, `trunk_scale: 1.0`, `base_swell: curve` (radius multiplier by height near the base, default 1.6 at 0 m to 1.0 at 0.6 m) | Pipe model: tips get `tip_m`; each parent's radius satisfies `r_p^e = Σ r_c^e` from the tips down; then `base_swell` multiplies trunk radii near the ground |
| `plant.bend` | `stiffness: 1.0`, `load_per_m: 1.0`, `max_deg: 35` | Static sag post-pass: each branch segment rotates downward about its base by `min(max_deg, k · M / (E · r^4))`, `M` the moment of the carried length beyond the segment (cantilever approximation), processed from the tips toward the trunk so loads accumulate |
| `plant.lean` | `deg: 10`, `recover_height_m: 2`, `dir: v3` | Leans the trunk base then straightens it with height (a hillside tree), rotating whole subtrees rigidly |
| `plant.cut` | `by: height\|radius\|order\|shape\|distance`, `min`, `max`, `shape: pin` | Removes branches (with all children) whose base fails the test; a cut inside a segment shortens that segment to the boundary |
| `plant.simplify` | `angle_deg: 4`, `min_len_m: 0.05` | Removes skeleton nodes whose removal changes direction by less than `angle_deg` and nodes closer than `min_len_m` to their parent (never forks, tips or roots) |
| `plant.scale` | `factor: 1.0` | Uniform scale of positions and radii (and any length attributes) |
| `plant.twist` | `deg_per_node: 0`, `order: 1..` | Alternating roll of lateral branches (for flat inputs) |
| `plant.attach` | pins: `base`, `parts`; `sites: tips\|along\|attr`, `per_m: 1`, `order: [min,max]`, `align: along\|up`, `scale_by_radius: true` | Attaches part skeletons (twigs, smaller branches) at sites on the base skeleton, scaled by the base radius at the site; the result is one skeleton that `plant.grow` can continue |

## 6. Meshing and dressing

### 6.1 `plant.mesh`

Sweeps a ring along every branch (generalized cylinders): ring frames by parallel transport from the branch base; ring
vertex count `sides = clamp(round(2π r / side_len_m), 3, 24)` per branch (`side_len_m: 0.04`); rings at every skeleton node
plus extra rings where the turn exceeds 10°; a child branch starts inside its parent (overlap = parent radius) so the
junction has no gap. Settings: `profile: curve?` (a periodic radius multiplier around the ring, for buttresses and bark
ridges), `noise_m: 0.0` (radial noise amplitude, seeded), `bark_tile_m: 1.0` (V coordinate = distance along the branch /
`bark_tile_m`; U = 0..1 around), `materials: by_order [..]` or `by_radius [(max_r, material)..]`, `min_radius_m: 0.003`
(thinner branches are not meshed; leaves cover them). Output: `PlantMesh` with bark material slots and branch-level wind
data (§ 7).

### 6.2 `plant.leaves`

Distributes leaf meshes or twig cards onto a skeleton or meshed plant. Pins `in` (PlantSkeleton or PlantMesh; the result keeps
the input mesh and adds leaf parts), `mask` (optional regions where nothing is placed). Settings: `choices: [(mesh or
card, weight, size_m: [lo, hi], rules: [...])]`, `where: tips|along|both`, `orders: [min, max]`, `spacing_m: 0.08`,
`per_branch_max: 200`, `orient: sun|up|along|random` with `orient_jitter_deg`, `size_to_tip: curve` (size by distance from
the tip), `keep_clear_m: 0.0` (keep this far from leaves already placed by earlier `plant.leaves` nodes, so a second pass such as
blossoms after foliage does not overlap the first). Each choice has a list of `rules` that score a site: `height(min, max)`,
`light(min, max)`, `order(min, max)`, `radius(min, max)`; a rule contributes its weight when the site's value is inside the
range, and the site picks among the choices by their summed weights (ties by index). A choice with `leave_bare: true` places
nothing but still wins its sites (for example, no twigs on branches thinner than a given radius).

### 6.3 `plant.export`

Crate split (Decision, ADR-1585): `regia_procgen` must not depend on `regia_asset_pipeline` (layer 3 cannot depend on layer
5, and the pipeline depends on `regia_procgen` for the CLI and editor). So the `plant.export` node only validates its input
and records an `ExportJob { mesh: PlantMesh, settings, out }` in the run result (`RunResult::exports`); the host writes it
through `regia_asset_pipeline::plant_export::write_job(&ExportJob) -> Result<ExportReport, ExportError>`, which does LOD
reduction (meshoptimizer simplification on bark, whole-card thinning on leaves), the impostor bake and the glTF writer. The CLI (§ 8) and the editor call it after a successful
run; a run via `procgen.run` without such a host reports the job as `not_written`. Settings of the node:  `lods: [1.0, 0.5, 0.2, 0.05]`
(per-LOD triangle ratios). Bark uses meshoptimizer simplification (`simplify`, borders locked, target error 0.05). Leaves are not simplified and are not merged: each LOD keeps whole 6-index cards where `ceil((j+1)·r) > ceil(j·r)` (about `ceil(n·r)` cards; ratio 0 drops every card), then scales the survivors about each card's centroid by `sqrt(1 / max(ratio, 0.01))`, capped at 2, so a thinned LOD keeps coverage. meshoptimizer is not called on leaves.
`billboard: true` (an impostor at the last LOD via the existing CPU bake in `regia_terrain::scatter::impostor`), `lod_screen_size:
[..]`, `shadow_lod: n`, `collision: none|capsules` (one capsule per trunk and order-1 branch thicker than 5 cm),
`out: path`, `name`. Writes a glTF 2.0 binary with `_LOD<n>` nodes, material parts, extras `lod_screen_size` and
`shadow_lod`, and the wind attributes, following the conventions of `docs/formats/gltf-conventions.md` and the tree
import in `regia_asset_pipeline::gltf_conventions`. Writes are atomic (tmp + rename).

## 7. Wind data

Every mesh vertex carries two attributes named like the ones imported trees already carry (`_WIND`, `_WIND_PIVOT`,
`book/agents/scatter-forest.md`). The engine does not read them yet and no layout for them is written down in the repo,
so this spec defines the layout as Regia's own, documented by the builder in a new page `docs/formats/plant-wind.md`:
- `_WIND` (vec4 float): x = branch level (0 trunk, 1-3 branch orders, 3 for leaves), y = phase 0-1 (hash of the branch id),
  z = stiffness 0-1 (from the branch radius relative to the trunk base radius), w = leaf flutter weight 0-1 (1 on leaf
  vertices, 0 on bark);
- `_WIND_PIVOT` (vec3 float): object-space position of the base of the vertex's branch.
Decision (ADR-1585): this layout is Regia's own and stands; if the builder has an exported tree from another tool at hand
it may note in the format page whether the attributes agree, and the importer converts when they differ (not a blocker).
`docs/formats/plant-wind.md` is a new format page and `_WIND`/`_WIND_PIVOT` in plant exports carry `extras` key
`regia_wind_layout: 1` so a later layout change is detectable. Rendering the sway is the planned vertex-stage work of the scatter-forest page; this spec only produces
the data.

## 8. Tools and surfaces

- CLI: `regia procgen plant <graph> --out <dir> [--variants N] [--seed S] [--param name=value]...` writes `N` glTFs
  (`<name>_<k>.glb`, seeds `S + k`) and a `<name>.plants.ron` index (new format, check-formats name `plants-index`, `format_version: 1`: variant → seed, triangle
  counts, bounds, graph hash). Exit 1 on any
  failure.
- Editor (spec 67): plant graphs open in the same panel; the preview shows the selected node's skeleton or mesh in an
  asset preview viewport (thumbnail renderer) instead of the level; a "variants" strip shows 6 seeds side by side.
- Agent (page section "Plants" in `book/agents/procgen.md`; one `book/COVERAGE.md` row per method):
  `procgen.plant_build { graph, params?, seed?, variants?, out }` (assets; editor or CLI host only; heavy) →
  `{ files: [{ path, seed, triangles_per_lod, bounds }] }`; `procgen.plant_stats { graph, seed?, node? }` (read; heavy) →
  per plant: node count, max order, height, crown width, branches shed, triangles. `triangles` is the whole mesh
  when the skeleton has one plant and a mesh was passed, and 0 on every plant when the skeleton has more than one
  (the mesh keeps no per-plant triangle range). Errors: `procgen_plant_not_meshed`,
  `procgen_plant_runtime_forbidden`, `procgen_wrong_domain`, `procgen_limit_exceeded`, `procgen_unknown_graph`,
  `procgen_graph_invalid`.
- Starters: `starter/plant_broadleaf.pgraph.ron`, `starter/plant_conifer.pgraph.ron` (high `apical`, whorled,
  `buds_per_node: 5`), `starter/plant_bush.pgraph.ron` (several seeds in a disc, low `apical`), each with bark and leaf
  placeholders from the engine's default content.

## 9. Determinism

Spec 62 § 6 applies: draws keyed by (plant seed, node index, step, slot); the voxel grid and resource passes iterate in
index order; no libm transcendentals (`detmath`); glTF output is byte-stable (sorted maps, fixed float formatting in JSON,
fixed buffer order). The same graph, parameters and seed give the same `.glb` bytes on Linux and Windows x86-64 (tested).
LOD simplification runs through meshoptimizer (C++), whose floating-point results on other architectures (macOS arm64)
are not guaranteed identical; there the skeleton and LOD 0 must match and lower LODs may differ (the test records which).

## 10. Tests and acceptance

Unit tests (`regia_procgen::plant`, pipeline feature, no GPU):
1. Growth hash: the broadleaf starter with seed 7 gives a recorded skeleton hash (node count, positions) on Linux and
   Windows.
2. Apical control: with `apical` 0.4, 0.55, 0.7 and everything else fixed, the height / crown-width ratio increases
   strictly.
3. Light competition: an obstacle box on one side reduces the branch count on that side to less than half of the free
   side (recorded counts).
4. Shedding: with `shed_below_light` raised to 0.5, lower interior branches are removed (count of order ≥ 1 nodes below
   2 m drops by > 50 %).
5. Pipe model: at every fork `|r_p^e − Σ r_c^e| ≤ 1e-6 · r_p^e` after `plant.radius`.
6. Phyllotaxis: with `buds_per_node: 1` and `wander_deg: 0`, the yaw difference between successive lateral buds on one axis is 137.5° ± 0.01°.
7. Mesh: every branch tube's rings are closed (first and last vertex of a ring coincide in position, distinct by UV seam),
   no NaN, normals unit length; triangle count follows the `sides` rule.
8. Leaves: a `leave_bare` choice under `height(0, 2)` leaves no leaf below 2 m; `keep_clear_m` keeps a second `plant.leaves` pass
   at least that far from the first pass's leaves.
9. Export: the `.glb` loads through `regia_asset_pipeline`'s glTF import with the tree conventions
   (`gltf_conventions`) as a mesh with the requested LOD count, decreasing triangle counts, a billboard part on the last
   LOD and the two wind attributes on every vertex (the test lives in `regia_asset_pipeline`, which may depend on
   `regia_procgen` with the `pipeline` feature).
10. Domain: a `plant.grow` node in a `World` graph fails with `node_wrong_domain`; a `Plant` graph started by a runtime
    build fails with `plant_runtime_forbidden`.

Acceptance: `regia procgen plant starter/plant_conifer.pgraph.ron --variants 6` produces six visibly different conifers;
they are scattered by a spec 65 biome on the sample map and reviewed by eye against reference photos in the owner's daily
look review (no golden; lead TODO).

## 11. Platforms and performance

Pipeline only: Linux, Windows, macOS (testing). Budgets (desktop reference, one core): a 12-step broadleaf of
≈ 20 000 nodes grows in ≤ 1.5 s; meshing ≤ 0.5 s; leaves ≤ 0.5 s; export with 4 LODs and a billboard ≤ 3 s. Memory ≤ 1 GiB
for the largest starter.

## 12. Out of scope

Extracting skeletons from photographs or from existing meshes; interactive sculpting of skeletons in the viewport (edits
are graph nodes such as `plant.cut`; a later spec may add manual edits as recorded node data); texture-atlas baking of
bark and leaf sheets; runtime growth or seasonal animation; root systems below ground.

## 13. Provenance

Workflow stages (seed, grow in stages, shape, mesh, dress with leaves, attach parts, export with wind data) as described in
public user documentation, via the public-sources spec set area `runtime-procedural` (set spec id
`runtime-procedural-vegetation`; its single Sources page; thinly sourced, so used only to confirm the stage list).
Algorithms, all public: W. Palubicki, K. Horel, S. Longay, A. Runions, B. Lane, R. Měch, P. Prusinkiewicz,
"Self-organizing tree models for image synthesis", ACM TOG (SIGGRAPH) 2009 (bud fate, shadow propagation, Borchert-Honda
resource allocation, apical control); R. Borchert and H. Honda, "Control of development in the bifurcating branch system
of Tabebuia rosea", Botanical Gazette 1984; A. Runions, B. Lane, P. Prusinkiewicz, "Modeling Trees with a Space
Colonization Algorithm", Eurographics Workshop on Natural Phenomena 2007; K. Shinozaki et al., "A quantitative analysis of
plant form: the pipe model theory", Japanese Journal of Ecology 1964, and the da Vinci rule exponent (Eloy, "Leonardo's
rule, self-similarity and wind-induced stresses in trees", PRL 2011); H. Vogel 1979 (golden-angle divergence);
P. Prusinkiewicz and A. Lindenmayer, *The Algorithmic Beauty of Plants* (1990, phyllotaxis patterns, tropisms); J. Weber
and J. Penn, "Creation and Rendering of Realistic Trees", SIGGRAPH 1995 (shape parameters, leaves along branches);
parallel-transport frames (Bishop, "There is more than one way to frame a curve", 1975); cantilever beam deflection
(textbook). meshoptimizer simplification (existing dependency and row). Settings, defaults, the stage order inside a growth step
and the domain split are Regia's own.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>

## 14. Owner decisions

None.

## Addendum (ADR-2731): built state of § 4, § 6.3 and § 8

- **`envelope` as a Mesh.** Plant-only node `plant.envelope_mesh { asset }` (pin `out`, Mesh) is the source. The rule is ray parity along +X with a
  half-open edge rule, surface points inside, in add, subtract, multiply, divide and compare only. Vertices at one position weld. Refused with
  `envelope_empty`, `envelope_not_manifold` or `envelope_invalid`.
- **`billboard`.** `plant_export` calls `regia_terrain::scatter::impostor::bake_cards` on LOD 0 and embeds both atlases. The last LOD draws two quads on the
  captures nearest +Z and +X; `regia_impostor` extras on its node describe the atlas. Colors are one tint per slot kind (§ 12: no sheets are baked).
- **`procgen.plant_build`** writes through `PlantExportHook`, which the editor installs; other hosts answer `procgen_plant_host_required`. It shares
  `plant::build_plant_variants` with `regia procgen plant`, and answers `procgen_plant_no_export` for a graph with no export.

## Addendum (ADR-5647): conformance audit, 2026-11-10

- **Leaf choice** (§ 6.2, ADR-1666 (10)): a choice scores its `weight` plus the weights of its satisfied rules; a choice whose
  rules all fail scores 0; the highest score wins, ties to the lower index; nothing above 0 leaves the site bare.
- **Growth details** (§ 4, ADR-1666 (9)): lateral buds are a separate list per plant, a node's `bud` is its terminal bud;
  shadow starts one voxel below a node; buds sample light half a voxel ahead; buds may not step into obstacles either (they go
  dormant); shedding uses the subtree's mean light.
- **`plant.scale`** scales positions about each plant's root and radii; custom attributes are not scaled (none carries a unit).
- **Built after v0.5.0 (ADR-7215):** the editor's plant preview (§ 8): an open Plant graph shows the selected node's
  skeleton (or the last node with plant data) in an asset viewport drawn with the egui painter (a side view of the
  branches; no GPU-lit thumbnail), and a six-seed variants strip (`first_seed .. first_seed + 5`); a click sets the
  plant's seed. Agents: `editor.plant_preview { asset, seeds?, seed?, node? }`. Since ADR-7275, on a render device a
  mesh node's variants also get lit thumbnails (the batch thumbnail renderer, flat bark and leaf colors) in the viewport
  and the strip (`editor.plant_preview { lit: true }`); the skeleton strokes stay the fallback.
- **Verification still owed:** the Linux/Windows byte-identical `.glb` check of § 9 (the byte-stability test runs on one
  platform per run) and the § 11 timing budgets on the desktop reference (hand-run test `plant_budget_hand_run`).

