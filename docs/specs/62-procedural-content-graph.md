# Spec 62: Procedural content graph (core: data, graphs, execution, generators)

Status: proposed (ADR-1596).
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: none of `regia_procgen` exists; the pieces reused (§ 1) do. Plan item "procedural content graph" (`docs/versions.md` v0.6.0 row, owner 2026-10-28: a core
pillar; W-E in the 10-27 plan). Behavior written from the public-sources spec set, area `runtime-procedural` (ADR-1526,
behavior only); every name, format, default and algorithm below is Regia's own. Builders read this spec, the specs it names
(63-68) and the repo; never the set.

Companion specs: 63 node library, 64 splines, polygons and roads, 65 biomes and vegetation scatter, 66 tiled and runtime
generation, 67 editor graph authoring, 68 procedural plants. This spec is the foundation the others build on; build it
first.

## 1. Purpose

A **procedural content graph** turns rules into placed content: a directed graph of nodes reads the world (terrain, splines,
shapes, tagged entities, tables, masks), produces and refines sets of points carrying attributes, and finally spawns mesh
instances, prefabs or data. One graph serves many places: a **generator** entity in a level binds a graph, its parameters
and a seed, and runs it over its own bounds. Output is deterministic: the same graph, parameters, seed and inputs give the
same bytes on Linux, Windows, Android aarch64 and macOS.

It generalizes what Regia already has for one case: the terrain scatter (`regia_terrain::scatter`, `.scatter.ron` v2,
spec 26 § 1) is a fixed pipeline of exactly the stages a content graph composes freely. The scatter stays as it is (DO27
depends on it); spec 65 adds a node that runs a scatter set inside a graph and a converter.

What already exists and is reused (baseline):
- `regia_core::math::Rng` (PCG32, `new`, `with_stream`, `f32`, `range`, `chance`; `crates/regia_core/src/math/rng.rs`) for every random draw.
- `regia_core::version` (`VersionStep<T>`, `migrate`, `peek_ron_version`) for the frozen-format machinery (`specs/12` § 4a).
- `regia_terrain::TerrainMap` (`sample_bilinear`, `normal_at`) and `TerrainLayers` for ground input.
- `regia_scene` prefabs (`prefab.rs`: `PrefabTemplate`, `PrefabInstance`), `EntityGuid` (`components.rs`), levels; `regia_asset::AssetRef<T>`.
- `regia_terrain::grass::select::pcg_hash` (the same PCG output hash § 6.2 names, private to grass today) and `regia_terrain::road` (`RoadDecal`, `RoadRibbon`: single straight segments; spec 64 § 7 builds spline roads on top and leaves them alone).
- The frame renderer's instanced path used by the runtime scatter (`regia_terrain::scatter::runtime`, ADR-1415): many
  instances, no entity per instance, GPU culled.
- `blake3` (already a workspace dependency) for content hashes.
- `specs/23` § 6 generators (`regia gen run/verify`): external generator programs. The content graph is the in-engine,
  data-driven counterpart; `regia procgen verify` (§ 9) follows the same "re-run and compare bytes" rule.

## 2. Regia terms

| Term | Meaning |
|---|---|
| **Procedural graph** (`ProcGraph`) | Asset `<name>.pgraph.ron`: declared parameters, nodes with settings, edges between named pins, graph-level input and output pins |
| **Graph variant** (`ProcGraphVariant`) | Asset `<name>.pgvar.ron`: a parent graph plus parameter overrides (the graph-level analog of a material instance, `.matinst.ron`) |
| **Node** | One operation: a node type id (`sample.ground`, `points.jitter`, ...; catalog in spec 63), settings, enabled flag |
| **Pin** | A named input or output of a node. Inputs accept a set of data kinds; an edge links one output pin to one input pin |
| **Item** (`DataItem`) | One unit of data on a wire: a payload of one kind plus tags and item-scope values (§ 3) |
| **Bundle** (`Bundle`) | The ordered list of items that flows over one pin in one evaluation |
| **Point set** (`PointSet`) | The main payload: points with built-in properties and named attribute columns |
| **Table** (`Table`) | A payload of rows with named typed columns and no spatial meaning (parameters, lookup tables, counts) |
| **Attribute** | A named typed column on a point set or table; also the item-scope single values |
| **Generator** (`ProcGenerator`) | A component on a level entity: graph or variant, parameter overrides, seed, mode, bounds. Runs the graph |
| **Run** | One evaluation of a generator's graph; has an id, a state and a result |
| **Output set** (`ProcOutput`) | Everything a run produced: mesh instance sets, spawned prefab entities, exported tables |
| **Bake** | Running a generator offline (editor, agent or CLI) and storing its output next to the level (§ 8) |

Glossary rows to add to `docs/04-conventions.md` § 3: Procedural graph, Generator, Graph variant (append-only).

## 3. Data model (`regia_procgen::data`)

### 3.1 Value types

`AttrType` (closed set; the order is the conversion priority used by § 3.5):
`Bool`, `Int` (i64), `Float` (f64), `Vec2`, `Vec3`, `Vec4` (f64 components), `Quat` (f64, unit), `Transform`
(translation `Vec3`, rotation `Quat`, scale `Vec3`), `Name` (interned UTF-8 string, ≤ 256 bytes), `Asset` (project-relative
asset path or `guid:<32 hex>`). Attribute math is done in f64; built-in point properties are f32 (§ 3.2) because they feed
the renderer. All conversions between f64 and f32 are round-to-nearest (`as f32`).

`AttrValue` is the matching enum of one value. `AttrColumn` stores one typed column (`Vec<bool>`, `Vec<i64>`, ...;
`Name` and `Asset` store indices into the item's string table).

### 3.2 Point set

Struct of arrays, all columns the same length `len`:

| Built-in property | Type | Default | Meaning |
|---|---|---|---|
| `position` | `[f32; 3]` | 0 | World space, meters, Y up |
| `rotation` | `[f32; 4]` quat | identity | Orientation |
| `scale` | `[f32; 3]` | 1 | Per-axis scale |
| `color` | `[f32; 4]` | 1 | Linear RGBA; spawners pass it as per-instance tint |
| `density` | f32 | 1 | Likelihood that the point exists, clamped to [0, 1] on every write |
| `seed` | u32 | position hash (§ 6.2) | The point's random stream key |
| `bounds_min`, `bounds_max` | `[f32; 3]` | −0.5, 0.5 | Local box of the thing the point stands for, before scale |
| `hardness` | f32 | 1 | How sharp the point's box edge is when the point is used as a region: 1 = full value to the box edge, 0 = value falls linearly from the center to the edge. Clamped to [0, 1] |

Custom attributes are extra `AttrColumn`s keyed by name. Built-in names, `index`, `count` and any name starting with `_` are
reserved; creating a custom attribute with a reserved name is a node error `attr_reserved_name`.

The world-space box of a point is `position + rotation * (scale * [bounds_min, bounds_max])` (an oriented box); helpers
`world_aabb(i)` and `contains(i, p) -> f32` (the region value at `p`: `density * falloff`, falloff from `hardness`) are part
of the API because filters and set operations (spec 63) use them.

### 3.3 Other payload kinds

`DataKind` (bit set, used by pins to declare what they accept):

| Kind | Payload | Spec |
|---|---|---|
| `Points` | `PointSet` | § 3.2 |
| `Table` | `Table` (rows × named columns) | § 3.1 |
| `Spline` | `SplineData`: control points, tangents, closed flag, per-control-point attributes | spec 64 |
| `Polygon` | `PolygonData`: one or more closed rings in a plane with a transform, holes allowed | spec 64 |
| `Shape` | `ShapeData`: box, sphere, capsule or convex hull with a transform and an edge band; a region test, no points. Read from the `regia_scene::Shape` component | spec 64 § 2.4; region rule spec 63 § 2 |
| `Ground` | A view of terrain heights, normals and layer weights over a rectangle (`GroundData`) | spec 63 § 3 |
| `Mask` | A 2D scalar or RGBA image mapped onto an XZ rectangle (`MaskData`) | spec 63 § 3 |
| `Mesh` | A reference to a mesh asset with its bounds; sampled by `sample.mesh_surface` | spec 63 §§ 3-4 |
| `PlantSkeleton` | Branch graphs of grown plants (plant domain only) | spec 68 § 2 |
| `PlantMesh` | Meshed plants with wind data (plant domain only) | spec 68 § 2 |

`DataKind` is part of the `pgraph` v1 shape (pins name kinds in files). `Points`, `Spline`, `Polygon`, `Shape`, `Ground`
and `Mask` are **spatial** kinds. Every spatial kind except `Points` and
`Table` can be turned into points by a sampler node (spec 63 § 4); filters accept any spatial kind as a region.

### 3.4 Items, tags, item scope and bundles

`DataItem { payload: Payload, tags: BTreeSet<Name>, scope: Table /* exactly one row */ }`. Tags are plain names used to
route and filter whole items (spec 63 § 8). Item-scope values are single values that belong to the item, not to an element
(for example a row count, or the source entity's guid).

`Bundle(Vec<DataItem>)` keeps order. Nodes that work per item process items in bundle order and emit results in the same
order. Nodes never reorder items unless their spec says so (`items.sort`).

### 3.5 Selectors

A selector string names what a node reads or writes:

- `name` → an element attribute or built-in property of the element scope (points of a point set, rows of a table,
  control points of a spline).
- `item.name` → an item-scope value.
- A component suffix after `:` picks vector parts: `position:y`, `color:rgb`, `velocity:xz` (letters `xyzw` or `rgba`,
  1-4 of them, any order, repeats allowed). A suffix on a scalar is an error.
- `$prev` → the last attribute written by the upstream node on this item (each item remembers the name of the last
  attribute a node wrote; nodes that create or modify an attribute update it). Default for read selectors that are left
  empty.

Type conversion on read: `Bool → Int → Float` widen silently; `Float → Int` truncates toward zero only where a node says
"integer input"; a scalar read as a vector broadcasts; a vector read as a scalar is an error `attr_type_mismatch` unless a
component suffix picks one part. `Name` and `Asset` never convert to numbers. Reads of a missing attribute are an error
`attr_missing` unless the node declares a default.

## 4. Graph asset (`<name>.pgraph.ron`, format name `pgraph`, `format_version: 1`)

```ron
ProcGraph(
    format_version: 1,
    description: "Rocks along the slope bands",
    params: [
        (name: "spacing", ty: Float, default: 4.0, min: Some(0.25), max: Some(64.0), doc: "Meters between candidates"),
        (name: "rock_mesh", ty: Asset, default: "meshes/rock_a.glb", doc: ""),
    ],
    inputs: [ (name: "in", kinds: ["Points", "Spline", "Shape"]) ],   // graph input pins (subgraph use)
    outputs: [ (name: "out", kinds: ["Points"]) ],
    nodes: {
        1: (ty: "in.self", settings: {}, at: (0.0, 0.0)),
        2: (ty: "sample.ground", settings: { "spacing_m": Bind("spacing"), "jitter": 0.8 }, at: (220.0, 0.0)),
        3: (ty: "spawn.mesh", settings: { "pick": Weighted, "entries": [ (mesh: Bind("rock_mesh"), weight: 1.0) ] }, at: (440.0, 0.0)),
    },
    edges: [ (from: (1, "out"), to: (2, "region")), (from: (2, "out"), to: (3, "in")) ],
    flags: (starter: false, tool: None, domain: World),
)
```

- `nodes` is a map keyed by a `NodeId(u32)`, unique and never reused within the file (the editor allocates `max + 1`).
- `ty` is a node type id from the registry (spec 63). An unknown id fails validation with `unknown_node_type` and names the
  node; the file still loads in the editor so the author can fix it.
- `settings` is a map of setting name → a plain RON value written the way the node type's settings struct serializes:
  numbers, booleans, strings (names and asset paths), tuples for vectors, lists, named structs and enum variants (for
  example `pick: Weighted`, `pick: ByAttr(attr: "kind")`, `entries: [(mesh: "a.glb", weight: 1.0)]`; the settings blocks
  in specs 63-68 are written in this form). Curves are lists of `(x, y)` pairs sorted by x (piecewise linear). Any value at
  any depth may instead be `Bind("name")`, bound to a graph parameter. Loading is two steps: the file keeps settings as
  untyped RON values; at compile, every `Bind` leaf is replaced by the parameter's effective value (§ 4.1) and the result is
  deserialized into the node type's typed settings struct (serde). Unknown setting names fail with `unknown_setting` (no
  silent drop); wrong types fail with `setting_type`; values outside a setting's documented range fail with
  `setting_out_of_range` naming the node, setting, value and range. Every numeric setting in specs 63-68 has a range: where
  a row gives none, lengths, spacings, radii and counts must be > 0 (spacings and cell sizes ≥ 0.01 m), fractions lie in
  [0, 1], and angles in [−360, 360]. Missing settings take the node type's documented default.
- Each node type carries a `settings_version: u32` in the registry. A node entry may store `v: N` when its settings were
  written by an older node version; the loader runs that node type's settings migrator chain. A node type change that is
  not a pure addition with a default bumps its `settings_version` and ships a migrator (this keeps the file's
  `format_version` stable while the library grows; record each bump in the node type's doc and in `check-formats`).
- `enabled: false` (optional, default true) **bypasses** the node: its first input pin's bundle passes to its first output
  pin unchanged, other outputs are empty. A node with no inputs, when bypassed, outputs empty bundles.
- `at` is the editor canvas position; it does not affect the result or the graph hash (§ 6.4).
- `comment` nodes (`ty: "note"`) and reroute nodes (`ty: "reroute"`) are editor aids: notes have no pins; a reroute has one
  input and one output and forwards its bundle.
- `inputs` and `outputs` declare the graph's own pins (used when the graph is called by `graph.call`, spec 63 § 11): a
  name and the accepted kinds (strings naming `DataKind` variants).
- `flags.starter: true` lists the graph in the editor's "new graph from starter" picker (spec 67). `flags.tool` names an
  interactive tool kind (spec 67 § 5) the graph serves. `flags.domain` is `World` (default) or `Plant` (pipeline-only
  plant graphs, spec 68); node types declare the domains they support.
- Frozen format rule (`specs/12` § 4a): register `pgraph` in `cargo xtask check-formats` with its schema fingerprint;
  any shape change bumps `format_version`, adds a `VersionStep` migrator and `regia upgrade-project` support. Files with a
  newer version are refused with `pgraph_version_newer`.

Graph variant, `<name>.pgvar.ron` (format name `pgvar`, `format_version: 1`):

```ron
ProcGraphVariant(format_version: 1, parent: "procgen/rocks.pgraph.ron", overrides: { "spacing": 2.0 })
```

A variant's parent may itself be a variant (chain ≤ 8). Overrides of unknown parameters are kept in the file, reported as
warnings `variant_unknown_param` and ignored, so renaming a parameter in the parent does not destroy data. A cycle in the
parent chain is refused with `variant_cycle`.

### 4.1 Parameters

A parameter is `(name, ty, default, min?, max?, doc)`. Names follow `[a-z][a-z0-9_]{0,63}`. A setting bound with
`Bind("x")` takes the effective value: generator override → variant chain overrides (nearest first) → graph default.
Parameter defaults and overrides are plain RON values of the parameter's type. Values outside `min..=max` are clamped and
reported once as a warning `param_clamped`. An `Int` parameter is clamped in `i64`. Its `min` and `max` must each be a
finite integer inside `i64`; any other bound is `param_type_mismatch`. An override float becomes an `Int` only when it
is that same kind of integer; otherwise the override fails `override_invalid`. A `Bind` whose type differs from
the setting's type converts by § 3.5 or fails validation with `param_type_mismatch`.

Parameters also arrive as data: every node type may expose a setting as an **override pin** (`@<setting>` input pin, shown
by the editor on request). When the pin receives a `Table`, the setting takes the value of the column with the setting's
name in row 0 (or the sole column when the table has one column); the type must convert. A connected override pin wins
over `Bind` and literal values.

### 4.2 Validation (compile)

`ProcGraph::compile(&registry) -> Result<CompiledGraph, GraphErrors>` checks, reporting every problem (not just the
first) as `(node id or none, code, message)`:
1. every edge names existing nodes and pins (`unknown_pin`); an input pin has at most one edge unless the pin is declared
   `multi` (then edges are ordered by `(from node id, from pin name)` and their bundles concatenate in that order);
2. kinds: the output pin's kinds intersect the input pin's accepted kinds (`pin_kind_mismatch`);
3. no cycle through edges (`graph_cycle`); repetition is only through `graph.loop` and recursion through `graph.call`
   (spec 63 § 11);
4. settings valid for the node type (`unknown_setting`, `setting_type`, `setting_out_of_range`), `Bind` names declared
   (`unknown_param`), node types allowed in the graph's domain (`node_wrong_domain`, spec 68 § 2);
5. required input pins connected (`pin_unconnected`), graph outputs reachable (warning `output_unreachable`).

`CompiledGraph` holds the evaluation order: a topological order where ties are broken by ascending `NodeId`, and a
**graph hash** (§ 6.4). Compilation is pure and cached by graph hash.

## 5. Execution (`regia_procgen::exec`)

### 5.1 The evaluator

`Evaluator::run(&CompiledGraph, &RunInputs, &CancelToken) -> Result<RunResult, RunError>`. It is a pure function of its
arguments: it never reads the ECS world, the clock, the file system or the GPU directly. World data reaches it only
through `RunInputs::world: Arc<dyn WorldSnapshot>`, captured before the run (§ 5.3).

- Each node type implements `trait NodeKind { fn pins(&self, settings) -> PinSpec; fn eval(&self, ctx: &mut NodeCtx,
  inputs: &[Bundle], settings: &Settings) -> Result<Vec<Bundle>, NodeError>; fn settings_version(&self) -> u32; ... }`.
  `eval` must be deterministic in its inputs, settings and `ctx` (seed, cell bounds, parameters).
- Nodes run in compiled order. Nodes whose inputs are all ready may run in parallel on the `bevy_tasks` compute pool; the
  result never depends on the schedule because node outputs are stored per node and consumers read them by id.
- Inside a node, work may be split into fixed-size chunks (4096 elements) processed in parallel and **concatenated in
  chunk order**. Reductions (sums, min/max) over floats combine chunk results in chunk order, never by thread arrival.
- A node whose output bundles are all empty still "runs"; downstream nodes receive empty bundles. Control-flow nodes
  (spec 63 § 10) mark untaken branches **inactive**: inactive nodes are skipped and output empty bundles, and the run
  report lists them.
- Output pins of the graph (or of the generator: the `out.*` nodes) collect the final bundles into the `RunResult`.

### 5.2 Errors, checks and cancellation

- A node error stops the run: `RunError::Node { node, code, message }`; no partial output is published. Nodes that
  check data (`debug.check`, spec 63 § 12) fail the run this way on purpose.
- `CancelToken` is checked between nodes and between chunks. A cancelled run returns `RunError::Cancelled` and publishes
  nothing.
- Limits (`RunLimits`, defaults per platform class, overridable per generator within the hard caps): points per item
  (desktop 4 M, Android 1 M), total live elements across all node outputs (desktop 32 M, Android 8 M), estimated memory
  (desktop 1 GiB, Android 256 MiB), loop iterations (65 536), call depth (32). Exceeding one fails with `limit_exceeded`
  and names the limit; nothing is clamped silently.
- **Non-finite values.** A node that would write NaN or ±infinity into a built-in property or attribute (`attr.math`
  `sqrt` of a negative, `asin` beyond ±1, `pow` overflow, division results, a NaN read from a table) fails the run with
  `attr_nan` naming the node, the attribute and the first element index. Inputs captured from the world are checked once at
  capture (`input_nan`). Nothing downstream ever sees a non-finite number, so `q()` (§ 6.2) and sorting are always defined.
- Warnings (clamped params, unknown variant overrides, empty inputs where data was expected) are collected in the run
  report, each at most once per (node, code).

### 5.3 World snapshot and dependencies

Before a run, the host builds a `WorldSnapshot` with exactly the data the graph's input nodes ask for: terrain tiles over
the run bounds, entities matching an input node's query (with their transforms, tags, splines, shapes), table and mask
assets, and physics ray-cast results when a node needs them (spec 63 § 3). Capture happens on the main thread
(`ProcGenSet::Capture` system set) within a time slice; evaluation runs on worker threads.

Each captured piece records a **dependency**: `Terrain { rect }`, `Entity { guid, what: Transform|Spline|Shape|Tags }`,
`Asset { path, hash }`, `Query { filter }`. The generator keeps the last run's dependency list. Changes that hit a
dependency (terrain edit events over an overlapping rect, a changed entity transform or spline, an asset hot reload, an
entity entering or leaving a query's match set) mark the generator **stale**. In the editor, stale generators with
`live: true` re-run after a 150 ms quiet period (debounce counted in editor frames at the editor's fixed tick, not wall
time in tests). At runtime, staleness only re-runs generators whose `refresh_on_change` is true.

Graph inputs are also part of the dependency set: the graph file, its variants, subgraphs it calls (transitively) and
assets its settings name.

### 5.4 Caching

Node outputs are cached by `blake3(node type, settings_version, resolved settings, input bundle hashes, node seeds,
tier, cell, quality tier, content hashes)`, where *content hashes* are the hashes of every asset the node's settings name
(meshes, tables, masks, prefabs) and, for `graph.call`/`graph.loop`, the called graph's hash including its own callees and
assets (computed once per run, transitively). An edit to any referenced asset therefore changes the key.
A bundle's hash is computed once when produced (over payload bytes in canonical order, tags and scope). The cache is an
LRU with a byte budget (editor 512 MiB, runtime 64 MiB desktop / 16 MiB Android, 0 for bake). A cache hit returns the
stored bundles (shared `Arc`). Caching never changes results; a test runs every acceptance graph twice with and without
the cache and compares output hashes.

## 6. Determinism and seeds

### 6.1 Rules

1. All random draws use `regia_core::math::Rng` (PCG32).
2. Per-element randomness depends only on the element's `seed`, the node seed and a fixed draw index; never on thread,
   chunk or evaluation order.
3. No `f32::sin/cos/tan/atan2/exp/ln/powf` or `cbrt` from the platform libm on placement paths: `regia_procgen::detmath` provides
   `sin_cos(radians)`, `atan2`, `exp2`, `log2`, `pow` and `cbrt` built from range reduction plus fixed polynomials using only
   `+ - * /`, `floor`, `sqrt` (IEEE-exact everywhere). Error ≤ 2e-6 absolute for `sin_cos`; the bound is a test.
4. No hash maps with random state in any path that affects output order (`BTreeMap`, or a `Vec` kept in insertion order;
   no new map dependency).
5. Sorting is always stable with a full tie-break (spec 63 states the key per node).
6. Floating-point reductions combine in a fixed order (§ 5.1).

### 6.2 Seeds

Two kinds of randomness exist, and only one may depend on the cell:

- **Generator seed** `gen_seed: u32` = `ProcGenerator::seed` when set, else derived from its `EntityGuid`
  (`mix(low 32 bits, high 32 bits of the guid's first u64)`), so a duplicated generator gets a new pattern unless the
  author pins the seed in the editor.
- **Tier seed** `tier_seed = mix(gen_seed, tier_index)` (`tier_index` 0 for untiled generators and the base tier,
  spec 66 § 3).
- **Node element seed** `node_seed = mix(tier_seed, seed_salt)`; `seed_salt: u32` is a per-node setting, default = its
  `NodeId`. It does **not** include the cell, so per-element draws are identical whichever cell computes the element.
- **Node run seed** `node_run_seed = mix(node_seed, cell_key)` with `cell_key = mix(i as u32, j as u32)` of the cell
  (0 untiled). Only nodes whose spec says "per-run draw" use it (none of the built-in nodes do; it exists for custom
  node types that need one draw per cell).
- **Point seed** of a point created by a sampler: `mix(mix(mix(q(x), q(y)), q(z)), tier_seed)` where `q(v)` is
  `(v * 1000.0).round() as i32 as u32` (millimeter grid). The same position gives the same seed in every cell and after
  unrelated edits.
- **Per-element draws:** `Rng::with_stream(((point.seed as u64) << 32) | node_seed as u64, draw_slot as u64)`; `draw_slot`
  is fixed per node and per use (specs 63-68 list them).
- `mix(a: u32, b: u32) -> u32` is defined once in `regia_core::math::hash` (new public module, `pub mod hash;` in `regia_core/src/math/mod.rs`; the copies in
  `regia_terrain::grass::select`, `regia_pt` and `regia_render` may switch to it later, with their outputs unchanged): `pcg_hash(a ^ pcg_hash(b))` with
  `pcg_hash(v) = { let s = v.wrapping_mul(747796405).wrapping_add(2891336453); let w = ((s >> ((s >> 28) + 4)) ^ s)
  .wrapping_mul(277803737); (w >> 22) ^ w }` (Jarzynski and Olano 2020). Test vectors are recorded in `regia_core`.
- `points.reseed` (spec 63 § 6) recomputes seeds when an author wants copies to diverge.

### 6.3 Platform equality

The acceptance graphs (§ 10) record a blake3 of their run output in the test source. The same test runs on Linux,
Windows and Android aarch64 CI (and macOS when tested); any mismatch is a bug, never a re-bless without a reason in the
commit message.

### 6.4 Graph hash

`graph hash = blake3(canonical encoding of: params, nodes in id order (type, settings_version, settings, enabled,
seed_salt), edges sorted, graph pins)`. Canvas positions, notes and descriptions are excluded. Reroute nodes are
collapsed before hashing so moving wires through reroutes does not change the hash.

## 7. Generators in the world (`regia_procgen::world`)

### 7.1 Components

```rust
/// Runs a procedural graph over this entity's bounds.
#[derive(Component, Reflect, Clone)]
#[reflect(Component, Default)]
pub struct ProcGenerator {
    pub graph: AssetRef<ProcGraphAsset>, // ProcGraphAsset = enum { Graph(ProcGraph), Variant(ProcGraphVariant) },
                                         // one asset type loaded from either extension
    pub params: BTreeMap<String, SettingValue>, // SettingValue = an untyped RON value (§ 4)
    pub seed: Option<u32>,               // None = derived from the EntityGuid (§ 6.2)
    pub mode: ProcGenMode,               // Baked (default) | OnLoad | Anchored (spec 66)
    pub bounds: ProcBounds,              // Box { half_extents } in entity space | FromShape | FromSpline { margin_m }
    pub live: bool,                      // editor: re-run when stale (default true)
    pub refresh_on_change: bool,         // runtime: re-run when stale (default false)
    pub limits: Option<RunLimitsOverride>,
    pub tiles: Option<TileSettings>,     // cells, tiers, anchors (spec 66); None = one run over the bounds
    pub editor_only: bool,               // removed from cooked levels with its output (default false)
    pub dependency_priority: i32,        // grass-handoff order, then guid; the run queue does not read it, and it is not clamped
    pub quality_tier: Option<QualityTierChoice>, // tier used by flow.quality for Baked runs (default High), § 7.4
    pub allow_terrain_edits: bool,       // spec 64 § 7.3 (default false)
    pub format_version: u32,             // 1
}
```

`Default` is implemented by hand (not derived) so the defaults above hold: `live: true`, `format_version: 1`,
`mode: Baked`, `bounds: Box { half_extents: (32, 64, 32) }`, `seed: None`, everything else off or empty. The same rule
applies to every saved type in specs 62-68 whose documented default is not the type's zero value.

`ProcGenerator` is a reflected component with a pinned type path (`regia_procgen::ProcGenerator`) and `format_version` 1
in the scene format; register it in `check-formats` (`procgen-generator`), migrator `ProcGenerator::migrate` (none yet).

Runtime state lives in a separate, non-saved component `ProcGenState { state, run_id, last_ok_run, output_hash,
deps, report }`. Generated entities carry `regia_scene::components::Generated { generator: Guid, run_id: u64 }`. This
marker is defined in `regia_scene` (layer 1) so the level saver can skip generated entities without depending on
`regia_procgen`.

### 7.2 Run lifecycle

States (`ProcGenRunState`, also the agent string): `idle`, `queued`, `capturing`, `running`, `applying`, `ready`,
`failed`, `cancelled`, `cleared`.

```
idle|ready|failed|cancelled|cleared --request--> queued --slot--> capturing --> running --> applying --> ready
                                                   running|capturing --cancel or new request--> cancelled
                                                   running --node error/limit--> failed
ready|failed --clear--> cleared
```

Events (ECS events, also forwarded to the agent event stream as `procgen.*`): `ProcGenStarted { entity, run_id }`,
`ProcGenFinished { entity, run_id, output_hash, counts }`, `ProcGenFailed { entity, run_id, node, code }`,
`ProcGenCancelled { entity, run_id }`, `ProcGenCleared { entity }`, `ProcGenDetached { entity, new_root }`.

Ordering guarantees:
- Every `Started` is followed by exactly one of `Finished`, `Failed`, `Cancelled` with the same `run_id`.
- A new request while a run is in flight cancels it: `Cancelled(old)` is sent before `Started(new)`.
- `applying` swaps outputs atomically in one frame: the old output set stays visible until the new one is complete; a
  failed or cancelled run leaves the previous output in place.
- `Cleared` is sent after the outputs are gone. Clearing a generator in `running` cancels first.
- Generators whose graph reads another generator's output (`in.generator_output`, spec 63 § 3) wait until that generator
  is `ready`. When the reader is queued, an `idle` or `cleared` source is queued ahead of it, so the source runs first,
  unless the source is tiled or `editor_only` in a host that is not the editor. A source that is already `ready`, or that
  has `failed` or been `cancelled`, is not started again; the read then warns `export_missing` when that source published
  nothing. A dependency cycle is a validation error `generator_cycle`. A generator never reads its own output. Explicit
  requests keep queue order; `on_load` admits and stale refreshes sort by guid. `dependency_priority` is not that
  tie-break: the grass handoff sorts by it, then guid, and the value is not clamped to −1000..=1000.

Concurrency: at most `ProcGenSettings::max_concurrent_runs` runs at once (desktop 4, Android 1). The queue keeps request
order: what was already waiting, then `on_load` admits in guid order, then stale refreshes in guid order.

### 7.3 Outputs

`ProcOutput` for a generator holds:
- **Instance sets**: per (mesh asset, material override, shadow setting, collision setting) a list of transforms plus
  per-instance color and up to 4 custom floats. Drawn through the frame renderer's instanced path without entities (as the
  runtime scatter is). Per-entry draw distance and fade (spec 63 § 13). Collision, when an entry asks for it, builds one
  static compound collider per generator (or per cell, spec 66) in `regia_physics`.
- **Spawned entities**: prefab instances, one per point, with attribute-driven overrides (spec 63 § 13). They carry
  `Generated`, are parented under one `ProcOutputRoot` entity per generator (identity transform, so world positions are
  exact), and are not written to the level file.
- **Data layers (as built, ADR-7214):** the output follows the generator's presence (spec 37). The generator is the only
  layer member; `follow_generator_layers` (`PostUpdate`, after `LayerSwitch`) parks every output root of a parked
  generator (its subtree follows), unparks it when the generator is live again, and despawns the roots of a generator
  the layer switch despawned (the respawned generator loads or runs its output again). The instanced draw skips parked
  outputs and instance sets whose `target_layer` (spec 63 § 13.1) is not `Live`; their colliders carry
  `ProcLayerFollow` and are parked with that layer.
- **Generated meshes**: meshes a graph builds (spline-deformed pieces, road ribbons; spec 64 § 6), uploaded through the
  runtime mesh-creation path and drawn like instance sets.
- **Exported masks**: named `Mask` items a graph exports (`out.mask`, spec 63 § 13.4), read by other systems through
  `ProcOutput::mask(name)` (the grass multiplier of spec 65 § 8).
- **Terrain edits**: height patches a graph applies at bake time (spec 64 § 7.3): rectangle, new samples, original samples.
  Only `Baked` generators with `allow_terrain_edits` may produce them.
- **Exported tables**: named tables the graph writes for later readers (other generators, game code through
  `ProcOutput::table(name)`).

Named outputs (tables, point sets, masks) hold one value per run: a later write of the same name replaces the earlier
one, in compiled order, including a write from a called graph (`graph.call`, `graph.loop`; the caller warns
`output_name_replaced`). Everything else a called graph publishes (instance sets, prefabs, generated meshes, terrain
edits, road decals, exports) accumulates in call order (v0.5.1, deep-zai-procgen-runtime findings 6 and 7). A tiled run
is one logical output over many cells: its merge appends same-named point sets across cells (spec 66).

Clearing removes all of them. **Detach** (`ProcGenerator` editor action and agent method) converts the current output into
ordinary level content: instance sets become one `InstancedMeshSet` entity per set (a saved component, spec 63 § 13),
spawned entities lose `Generated` and are saved, generated meshes are written as cooked mesh assets under
`<level>.procgen/detached/` and referenced by ordinary mesh entities, terrain edits stay applied (they become ordinary
terrain content), the generator's baked file is deleted, and `ProcGenDetached` names the new root. Detach is undoable in
the editor.

### 7.4 Modes

| Mode | When it runs | What is stored | Use |
|---|---|---|---|
| `Baked` (default) | Editor, agent or `regia procgen bake`; never at runtime | Output in the level's bake directory (§ 8), loaded with the level | Most content; zero runtime cost beyond drawing |
| `OnLoad` | At level load, before the level reports loaded, on worker threads | Nothing; the cooked graph ships | Content that depends on runtime choices (seeded maps, difficulty) |
| `Anchored` | At runtime in cells around anchors (spec 66) | Nothing | Dense near-camera detail over large worlds |

**Quality tier.** `flow.quality` (spec 63 § 10) reads `regia_render::quality::QualityTier` (seven tiers: `MobileLow`,
`MobileHigh`, `Deck`, `Low`, `Medium`, `High`, `Ultra`). `OnLoad` and `Anchored` runs use the running tier. `Baked` runs
use `ProcGenerator::quality_tier` (default `High`); with `quality_tier: Some(PerTier)` the bake produces one output per
tier in the project's bake list (`Regia.toml` `[procgen] bake_tiers`, new optional key; default all seven tiers) and the loader picks the running tier's output (falling back to the
nearest lower, then higher tier). The tier is part of the cache key (§ 5.4) and of the `.pgout` header.

In the editor every mode can be run on demand for preview. `OnLoad` runs count against the level's load time and appear in
the load report; the level's `loaded` event waits for them (with a per-level timeout that fails visibly, not silently).
**As built (ADR-2421):** a `PerTier` bake writes one `<guid>.<Tier>.pgout` per tier (no format change); outside the editor
`OnLoad` generators run once when they spawn; `LevelLoaded` itself is not delayed: the load gate (`ProcGenLoadGate`)
closes with `ProcGenLoadFinished` and a `ProcGenLoadReport`, the timeout is counted in frames
(`ProcGenSettings::on_load_timeout_frames`), and loading screens wait for the gate (agent `procgen.load_report`).

## 8. Baked output, cook and verify

- Baked output is written to `<level path without extension>.procgen/<generator guid>.pgout` (binary, format name `pgout`,
  `version: u32 = 3`). Layout: magic `RPGO`, version, graph hash, parameter hash, seed, input hash (blake3 of all dependency
  hashes in a canonical order: the dependencies are sorted by a total key before hashing, so the ECS query order the
  capture walked cannot change the header), output hash, quality tier, then a cell table (spec 66 § 2; one entry for untiled generators) whose
  entries point at length-prefixed sections per cell: instance sets, spawned prefab records (prefab path, name, transform,
  tags; no override fields), tables, point sets, masks, generated meshes (spec 64 § 6: vertex and index streams in the layout of
  `docs/formats/cooked-mesh.md`, each with its material and instance list), terrain edit records (rect, original
  samples, new samples; spec 64 § 7.3), road decals and the data layers of the instance sets that have one
  (`set_layers: u32[(set_index: u32, layer: str)]`, version 4, ADR-7214). Little endian; f32 IEEE. This build writes
  version 4. `decode` reads versions 1 through 4. Version 1 is the untiled cell table (no tier, no per-cell hash),
  version 2 ends its sections after the masks and version 3 after the road decals. `migrate` rewrites 1, 2 and 3
  (`regia upgrade-project`); a set without a layer hashes as before, so a migrated version 3 file keeps its hashes. Loaders reject any other
  version. **As built (ADR-7750, the ADR-7724 (h) fold):** a version 4 section may end with an optional override
  section (`prefab_overrides: u32[(prefab: u32, overrides: u32[(component, field, value)])]`, the value a one-element
  attribute column), written only when a prefab record carries `spawn.prefab` overrides; a section that ends after the
  set layers has none, so every earlier version 4 file reads unchanged. A bake keeps the overrides and a loaded bake
  applies them as a live run does.
- The level stores the generator component only. On load, a `Baked` generator loads its `.pgout`; a missing file logs a
  warning `procgen_bake_missing` naming the level and generator and leaves the generator `idle` (never runs at runtime).
- Writes are atomic (temporary file plus rename). A bake directory entry with no generator is reported by
  `regia procgen verify` as `orphan_bake` and removed by `regia procgen bake --prune`.
- The cook copies `.pgout` files into the pak; `editor_only` generators and their bakes are left out.
  **As built (ADR-7214, replacing ADR-2003 (24)'s loose files):** the cook stores each shipping `.pgout` byte for byte
  (after a full decode) as a cooked asset of type `regia_procgen::pgout::PgoutBytes` with its `.meta` id, so it is in the
  pak under its asset path (`assets/<level>.procgen/<guid>.pgout`); `regia package` ships no loose bakes. The loader reads
  the file on disk when it exists, else the cooked copy through the `AssetServer` (`read_bake_bytes`), so an Android game
  reads its bakes from the APK's pak; streamed (anchored) bakes open from those bytes in memory. A build profile's entry
  levels pull their bakes into the closure. **As built (ADR-7262):** every level the closure reaches (placed levels,
  streaming cells, sublevels a raw file names) pulls its bakes too. A generator a placed or additive level brought in
  loads from the primary directory first, else from its own level's directory (`ProcLevelBakeDirs`, filled by the `regia`
  default plugins from the loaded level sources) under its source guid; `ProcBakeOrigin` records the file, the level and
  disk or pak, and `procgen.list` reports it with `bake_levels`. **As built (ADR-7287):** anchored (streamed-cell)
  generators inside a placed level open their cell bake the same way (primary directory first, then their own level's
  under the source guid, disk or pak), and an inline member that came from a prefab instance's `added_children` maps back
  to its source guid too; the host level's own file wins wherever it exists. **As built (ADR-7331):** a generator inside a
  prefab *template* (a prefab-internal entity, `PrefabLink`) maps back through its instance root (the nearest ancestor whose
  guid derives its guid with its local id, `regia_scene::prefab::derived_guid`) to the guid it has in the source level.
  Re-bake and verify use the same rule (`world::bake_target`): `bake`, `bake_cells`, `verify`, `procgen.bake`,
  `procgen.verify`, `regia procgen bake` and `regia procgen verify` write and compare a placed or additive member's file
  in its source level's directory under its source guid unless the primary directory has the member's own file, and a
  `Baked` inline member with no pinned seed runs with its source guid's seed, so the bytes equal the source level's own
  bake. The CLI loads the levels a level places (`load_level_source`) and registers their directories; verify re-bakes
  into scratch copies of every directory involved. Verify also names each entity with a cell bake but no generator
  (`anchored_generator_missing`, `world::lost_anchored_generators`; levels saved while ADR-7287 (5) was open); nothing
  is rewritten. `editor_only` generators (with their descendants) and their bakes are left
  out (`regia_procgen::cook`, ADR-2421): the cook skips their bakes.
- CLI (`regia_cli`, desktop only):
  - `regia procgen bake [--level <path>] [--generator <guid>] [--prune]`: runs `Baked` generators headless and writes
    their files; exit 0 on success, 1 on any failed run (each failure printed with level, generator, node, code).
  - `regia procgen verify [--level <path>]`: re-runs every `Baked` generator and compares the output hash with the file;
    exit 1 on any difference (prints both hashes and the first differing section). CI runs it. Generators with terrain
    edits are verified against the terrain with their own edits restored first (original samples from the file), so a
    flattened terrain does not feed back into its own verification.
  - `regia procgen graph check <file>`: compiles a graph or variant and prints all errors (exit 1 on error).

## 9. Agent protocol surface (`regia_agent/src/procgen.rs`, page `book/agents/procgen.md`)

| Method | Class | Params | Result |
|---|---|---|---|
| `procgen.list` | read | `{}` | `[{ entity, guid, name, graph, mode, state, run_id, output: { instances, entities, tables }, stale }]` |
| `procgen.get` | read | `{ entity }` | the component fields, effective params (with source: generator/variant/default), state, last report |
| `procgen.run` | edit_scene | `{ entity, params?, seed?, wait?: bool, timeout_ms? }` | `{ run_id, state, output_hash?, counts?, report? }`; with `wait`, answers when the run ends (deferred, like `play.wait_until`) |
| `procgen.cancel` | edit_scene | `{ entity }` | `{ run_id, state }` |
| `procgen.clear` | edit_scene | `{ entity }` | `{ cleared: bool }` |
| `procgen.detach` | edit_scene | `{ entity }` | `{ root, entities, instance_sets }` (editor only) |
| `procgen.inspect` | read | `{ entity, node, pin?, item?, offset?, limit?, attributes?, tier?, cell?: [i, j] }` (`tier`/`cell` for tiled generators, spec 66) | `{ items: [{ kind, tags, scope, len, columns: [name, type], rows: [...] }] }` of the last run's cached data (needs the run made with `keep_debug: true`, spec 67 § 3) |
| `procgen.report` | read | `{ entity }` | per node: `{ id, ty, ms, items, elements, cache_hit, inactive, warnings }` plus totals |
| `procgen.graph_get` | read | `{ path }` | params, pins, nodes (`id, ty, settings, enabled`), edges, compile errors |
| `procgen.graph_edit` | assets | `{ path, ops: [...], save?: bool }` | `{ graph_hash, errors }`. Ops: `add_node {ty, settings?, at?}` (returns id), `remove_node {id}`, `set_setting {id, name, value}` (value may be `{"bind": "param"}`), `connect {from:[id,pin], to:[id,pin]}`, `disconnect {...}`, `set_enabled {id, enabled}`, `set_seed_salt {id, salt}`, `move {id, at}`, `add_note {text, at}`, `add_param {name, ty, default, min?, max?, doc?}`, `set_param_default {name, value}`, `rename_param {from, to}` (updates every `Bind`), `remove_param {name}`, `set_graph_pins {inputs?, outputs?}`, `set_flags {starter?, tool?, domain?}`, `set_description {text}` |
| `procgen.node_types` | read | `{ category? }` | registry: `ty`, category, pins, settings with types/defaults/ranges, doc line |
| `procgen.bake` | edit_scene | `{ entity? }` | as `regia procgen bake` for the open level |
| `procgen.verify` | read | `{ entity? }` | `{ ok, mismatches: [...] }` |
| `procgen.set_debug` | edit_scene | `{ tiles?: bool, regions?: bool }` | toggles the runtime debug overlays (spec 66 § 7); `{ before, after }` |
| `procgen.tiles`, `procgen.anchors`, `procgen.tiles_wait`, `procgen.tiles_set`, `procgen.tiles_rebuild` | | | tiled and runtime generation, defined in spec 66 § 6 |

Error convention (as the built agent modules do): an error answers `F0002` (invalid params) or `F0003` (unknown entity or
path) with `data.reason` set to the code below; the codes are lowercase ascii, digits and `_`, and are listed on the page.
Codes: `procgen_not_generator`, `procgen_unknown_graph`,
`procgen_graph_invalid` (with `data.errors` = the compile list), `procgen_unknown_node`, `procgen_unknown_pin`,
`procgen_unknown_param`, `procgen_param_type`, `procgen_busy` (a run is applying), `procgen_no_debug_data`,
`procgen_run_failed` (with node and code), `procgen_limit_exceeded`, `procgen_editor_only`, `procgen_wrong_domain`, plus the
node and compile codes
named in this spec and spec 63 inside `data.errors`. Limits: `ops` ≤ 512 per call; `limit` ≤ 10 000 rows; `timeout_ms`
≤ 600 000. `procgen.run`, `procgen.inspect` and `procgen.bake` are `heavy`.

A `book/COVERAGE.md` row lists every `procgen.*` method.

## 10. Tests and acceptance

Unit tests (`regia_procgen`, no GPU, no world):
1. Format round trip: write → read → write is byte-identical; a v1 file with an unknown setting fails with
   `unknown_setting` naming node and setting; a newer `format_version` fails with `pgraph_version_newer`.
2. Compile: cycle detection names a node on the cycle; multi-input ordering follows `(from node, from pin)`; kind
   mismatch reports both pins; all errors reported in one pass (a graph with 3 faults reports 3).
3. Parameters: generator override beats variant beats default; clamping warns once; variant cycle refused; unknown variant
   override kept and warned.
4. Determinism: the "rocks on slopes" acceptance graph (ground → filter → jitter → spawn) on a fixed 256 × 256 synthetic
   heightfield gives a recorded output hash; the same run with 1 and 8 worker threads and with/without cache gives the same
   hash; changing the seed changes it; changing a terrain sample at least 2 samples outside the run bounds does not
   (bilinear height and normal reads touch one sample beyond the edge).
5. `detmath`: `sin_cos` within 2e-6 of an f64 reference over 100 000 fixed inputs in [−1000, 1000]; `atan2` within 4e-6.
6. Bypass: a disabled jitter node passes its input through; an enabled one changes positions.
7. Cancellation: a cancel token tripped after node 2 of 5 (by a test node that trips it) returns `Cancelled` and the
   previous output stays published.
8. Limits: a graph that would create 2× the point cap fails with `limit_exceeded` naming `points_per_item`.
9. Seeds: a point at the same position gets the same seed and the same per-element draws whichever cell (cell key)
   computes it; changing the generator seed changes both; `points.reseed` changes all seeds; `mix` matches its recorded
   test vectors; `regia_core::math::hash::pcg_hash(v)` equals `regia_terrain::grass::select::pcg_hash(v)` for 10 000 fixed
   inputs (the test lives in `regia_procgen`, which sees both).

ECS tests (`regia_procgen` with a minimal app, headless):
10. Lifecycle: request → events `Started`, `Finished` in order with matching `run_id`; a second request mid-run produces
    `Cancelled(1)` then `Started(2)`; a failing node produces `Failed` and keeps the old output.
11. Ownership: spawned prefab entities carry `Generated`, sit under the output root, and are skipped by level save; clear
    removes them; detach keeps them and removes `Generated`.
12. Dependencies: moving a spline entity the graph reads marks the generator stale; moving an unrelated entity does not;
    a terrain edit inside the run rect marks it stale, outside does not.
13. Bake: bake writes `.pgout` atomically (temp dir from `regia_test_utils::unique_temp_dir`); reloading the level shows
    the same output hash without running the graph (run counter stays 0); verify passes; editing a parameter and not
    re-baking makes verify fail with the generator named.
14. Generator ordering: B reads A's output; requesting both runs A first; a cycle A↔B fails validation with
    `generator_cycle`.

Agent tests (`regia_agent`): every method's example params validate against its schema; `procgen.run {wait: true}` on the
acceptance level returns `ready` and the recorded hash; `procgen.graph_edit` with an invalid connect returns
`procgen_graph_invalid` with the error list; `procgen.inspect` without debug data returns `procgen_no_debug_data`.

Acceptance (milestone check, also through the agent):
- A generator bound to the acceptance graph places rocks that sit on the terrain (each instance's Y within 1 cm of
  `sample_bilinear` at its XZ), only where slope and altitude filters allow, with the recorded hash on all three targets.
- Changing a parameter changes the output without editing the graph; two generators with variants of one graph produce
  different output.
- `regia procgen verify` on the sample project exits 0, and exits 1 after a hand edit of a `.pgout`.

No golden images are needed; visual review uses `seeing.screenshot` on the sample level (lead TODO if a golden is wanted).

## 11. Platforms

- `regia_procgen` builds on Linux, Windows, Android aarch64 (and macOS for testing). It has no platform code; threads come
  from `bevy_tasks`. Android runs `OnLoad` and `Anchored` generators with the Android limits (§ 5.2) and 1 concurrent run.
- The editor, the CLI bake and `procgen.graph_edit` file writes are desktop only. File writing (bake output, graph saves,
  plant export) lives behind the `regia_procgen` cargo feature `pipeline` (off in shipping game builds; on in
  `regia_editor` and `regia_cli`); without it the crate only reads `.pgout` and graph files.
- The `.pgout` reader is endian-explicit (little endian) and tested on aarch64.

## 12. Performance budgets

Measured by a bench (`cargo bench -p regia_procgen --bench graph`, not by tests):
- Compile a 200-node graph: ≤ 2 ms desktop.
- Sample a 1 km × 1 km ground at 2 m spacing (250 000 candidates), filter by slope and altitude, jitter, prune and spawn:
  ≤ 400 ms on one desktop core of the reference box (7950X3D), ≤ 120 ms with 8 workers; Android reference (Pixel 6) ≤ 1.5 s
  with 4 workers.
- Main-thread cost per frame while a run is in flight: capture slices ≤ 1 ms, apply ≤ 0.5 ms per 10 000 instances (larger
  outputs apply over several frames, old output visible until the last slice).
- Memory: a 1 M-point set with 4 custom f64 attributes ≤ 160 MB (built-ins take 92 bytes per point, the attributes 32).
- Profiling: `tracing` spans per run (`procgen.run`), per node (`procgen.node` with `ty`), capture and apply.

## 13. Crate and layering

New crate `regia_procgen`, **layer 3** (gameplay), depends on `regia_core`, `regia_ecs`, `regia_asset`, `regia_scene`,
`regia_terrain`, `regia_physics`, `regia_render`; `regia_agent` gains an edge to it (same-layer edge, add to the layer table
in `docs/02-architecture.md` § 1 and `xtask/src/layers.rs`; ADR-1596 records it). The umbrella `regia` re-exports it
behind a default-on cargo feature `procgen`. Modules: `data`, `graph` (format, variant, compile, migrate), `exec`
(evaluator, cache, detmath, seeds), `nodes` (registry and families, spec 63), `world` (components, systems, outputs, bake
IO; the writers behind the `pipeline` feature), `polygon` and spline nodes (spec 64; the `Spline` component itself is
in `regia_scene`), `biome` (spec 65), `tiles` (spec 66), `plant` (spec 68, `pipeline` feature only: plant node types are
registered only when the feature is on, so a runtime build cannot run a plant graph and answers
`plant_runtime_forbidden`). The evaluator (`data`, `graph`, `exec`, `nodes`) must not use
ECS types so it can run in `regia_cli` and in tests without an app.

No new third-party dependency in this spec (blake3, ron, serde, glam, bevy_tasks are in the workspace); spec 64 § 3 may add
one polygon library under the conditions of ADR-1596.

## 14. Out of scope

- GPU execution of nodes (compute kernels per node). The seam is `NodeKind::eval`; a later ADR may add GPU variants for
  sampling and spawning. Dense GPU ground cover already exists (`regia_terrain::grass`).
- A scripting node that runs user code from data. Game code adds node types in Rust through the registry
  (`ProcNodeRegistry::register::<T>()`), which is the extension point (Rust only, AGENTS.md rule 2).
- Network replication of generation. Clients generate locally from the same seed (that is the point of determinism);
  a server-authoritative variant is a later item.
- Mesh boolean or remeshing operations (a separate geometry item).
- Reading other engines' assets of any kind.

## 15. Migration

New formats (`pgraph` v1, `pgvar` v1, `pgout` v1, component `procgen-generator` v1); no existing format changes. A new
public module `regia_core::math::hash` (`mix`, `pcg_hash`) is added. The scene
format gains the `Generated` marker component (not saved, so no level format change). The lead runs
`cargo xtask check-formats --update` after the builder registers the formats.

## 16. Guide page outline (`book/agents/procgen.md`)

What a generator is; run a generator and read its report; inspect a node's points; edit a graph through `graph_edit`;
bake and verify; the error codes table; limits; determinism rules for authors of custom node types.

## 17. Provenance

Behavior of graph-based procedural generation as described in public user documentation of other engines, via the
public-sources spec set area `runtime-procedural` (set spec ids `runtime-procedural-overview`,
`runtime-procedural-graph-authoring`, `runtime-procedural-data-attributes`; their Sources pages). No text, identifiers or
paths carried. Algorithms: PCG32 (O'Neill 2014, existing row); PCG output hash for seed mixing (Jarzynski and Olano, JCGT
2020, existing row); topological sort (Kahn 1962); blake3 (existing). Regia's own design: the point property set
including `hardness`, selector syntax, settings versioning per node type, the lifecycle states and their ordering rules,
the baked sidecar, the limits and caching key.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 18. Decisions and owner questions

Decision (ADR-1585): `ProcOutputRoot` copies the generator entity's `LayerMembership` (spec 37 § 4.1,
`regia_scene::layers::LayerMembership`) when an output is applied, so switching a layer off hides what the generator made. Until
spec 37 is built the root has no layer membership and nothing else changes. Generated children are not members themselves
(they follow the root).
**Superseded in part (ADR-7214):** a copied membership would put the root under the layer switch, which despawns an
`Absent` member's subtree and can bring back only entities with a template record; an output root has none, and instance
sets have no entities. The root follows the generator's presence instead (§ 7.3 "Data layers"); the effect the decision
asked for (switching the layer off hides what the generator made) is what is built.
As built (v0.5.1), merged into the v0.6 line by integ-v06g, so both run there: the root follows the generator's presence
as above, and also: the root and every tile cell's root copy the membership at apply time and get
the layer's presence then (a `Parked` or `Absent` layer parks them; they carry no level template guid, so `Absent`
parks rather than despawns), and a parked output's instance sets are not drawn. A root that copied a membership also gets
an `EntityGuid` derived from its scope (the generator's guid, or the tile cell's scope) with a fixed root salt, so every
later layer switch parks and unparks it with the generator (ADR-7556; the switch only walks members with a guid). A
generator without an `EntityGuid` has none to scope its root's guid with: the root's guid derives from the generator's
entity id instead, mixed in before a tile cell's scope, so two such generators' roots, whole or tiled, never share one guid
(ADR-7595, ADR-7621; nothing persists it, because level saves
and scene snapshots skip `Generated` entities, and level generators always have an `EntityGuid`). Moving a generator to
another layer (or joining one after the apply) moves the applied root's membership with it on the next update, without a
re-run; a generator that leaves every layer keeps its applied root until the next apply. Roots
of a generator that is gone (despawned, for example by an `Absent` switch of a generator with a template record, or
stripped of `ProcGenerator`) are despawned with their prefabs and colliders, so the respawned generator's new output is
not doubled. `detach` gives the detached root a new random guid.

Decision (ADR-1585): the default generator seed derives from the entity guid, so a duplicated generator produces a new
pattern; the editor's duplicate action may offer "keep seed" later. No owner input needed.

Decision (ADR-1585): `QualityTier` bake lists live in `Regia.toml` `[procgen] bake_tiers` (§ 7.4); the key does not exist
today, it is optional and additive (no project-format version change beyond the documented key).

Decision (ADR-1585): `regia_core::math::hash::pcg_hash` has the same output as `regia_terrain::grass::select::pcg_hash`;
a test pins that equality so the two cannot drift.

Owner decisions: none.

## Addendum (ADR-2724, 2026-11-04): parallel schedule as built

- `Evaluator::with_parallelism(Parallelism)`: `Serial` (one node at a time in compiled order, the reference), `Compute`
  (the global compute pool, the default) or `Pool(Arc<TaskPool>)`; `Parallelism::threads(n)` makes a dedicated pool,
  `threads(0)` is serial. `RunReport::threads` records the pool size; it is never hashed.
- Ready nodes run in waves (one `TaskPool::scope` per wave). Nodes that write run output (`NodeKind::writes_output`;
  built-ins: the uncached ones) also wait for the previous such node, so `RunOutput` is written in compiled order. Every
  other node writes into a scratch output that must stay empty; if one is written, the run is redone serially
  (`Evaluator::serial_fallbacks`).
- The reduction runs in compiled order: report lines, road lines, the first failing node and the cumulative
  `total_elements` / `memory_bytes` checks equal the serial run's. A failure at node `k`, or completed nodes up to `k`
  already over a cumulative cap, stops scheduling after `k`.
- Chunks: `NodeCtx::map_chunks(len, f)` returns per-chunk (4096 elements) results in chunk order;
  `NodeCtx::for_each_job(jobs, f)` runs disjoint jobs. Both check the cancel token per chunk and run on the run's pool.
- Spans: `procgen.run` (`threads`), `procgen.wave` (`index`, `nodes`), `procgen.task`, `procgen.node`, `procgen.chunk`.
- Equality with the serial schedule at 1, 2 and 8 threads is a test (`tests/parallel_eval.rs`: every sample, starter
  and fuzz fixture graph, a wide graph with eight writers, first-failure and limit cases, a shared cache, cancellation,
  a 1 M-point chunked chain).

## Addendum (ADR-2731): copy-on-write point data

`PointSet` columns and `AttrColumn` payloads are `Column<T>`: `clone` shares, the first write to a shared column copies it. An output
goes to its last reader instead of being shared, except in planned tile runs and `keep_debug` runs. `ProcGenSettings::threads`
picks the schedule of a generator's runs (§ 5.1).

## Addendum (ADR-5644, 2026-11-10): conformance pass as built

- **CLI (§ 8).** The as-built form is ADR-2422's: `regia procgen bake <level|generator> [--level L] [--project DIR]
  [--out DIR] [--prune]` and `regia procgen verify <level|generator> ...` (positional target; `--level` names the level
  when the target is a generator). Verify compares bytes with a fresh bake (`changed:`, `missing:`, `extra:`) instead of
  printing both hashes. An `extra:` line is the `orphan_bake` entry and says so; `bake --prune` deletes those `.pgout`
  files after a successful whole-level bake (other files and `detached/` stay). `--prune` with a single generator, and
  `verify --prune`, are refused.
  As built (ADR-7743): prune deletes a guid-named `.pgout` only when no entity or record of the level (or of a level it
  places, or of a prefab instance in it) holds the guid; a file whose entity is still there (one that lost its anchored
  generator, ADR-7331 (3), or a generator that is not `Baked`) stays with a `kept` line, and while a placed level or a
  prefab template is missing no guid-named file is deleted. `bake --prune --dry-run` lists without baking or deleting.
  The CLI session loads the prefab templates the level's instances need from the project's `.prefab.ron` files, so a
  `Baked` generator inside a template bakes and verifies under the guid the instance gives it.
  ADR-7793: a prefab instance of a loaded level whose members are not in the session (its record did not spawn, e.g. its
  data layer starts `Absent`; it still waits for a template; or it failed to instantiate) counts as unresolved too, so
  prune deletes no guid-named file while one exists.
- **`procgen_busy` (§ 9) is never returned.** `applying` starts and ends inside one app update (the run's result is
  applied in the same call that sets the state), and agent methods run between updates, so no call can see a generator in
  `applying`. The code stays reserved; the book page does not list it.
- **§ 12 bench.** `benches/graph.rs` (`cargo bench -p regia_procgen --bench graph`): `compile_200_nodes`,
  `ground_1km_serial` and `ground_1km_8_threads` (501 × 501 heightfield, about 250 000 candidates; the bench asserts the
  count). First run, indicative only (2026-11-11, a shared 64-core Threadripper 3990X at load 75-360, release):
  `compile_200_nodes` 1.21 ms (budget 2 ms), `ground_1km_serial` 288 ms median with a 220-431 ms spread (budget 400 ms),
  `ground_1km_8_threads` 173 ms (budget 120 ms: missed on that host; `points.prune` runs serially). A quiet run on the
  reference box replaces these numbers.
- **Tests added for codes that had none:** `tests/spec62_conformance.rs` (`attr_reserved_name`, `attr_type_mismatch`
  with the `:y` suffix accepted, `pin_unconnected`, `output_unreachable`, `export_missing`, the 8-variant chain limit
  and `variant_chain_too_long`, node settings migration with `v: N`, `setting_version_newer`, `setting_migration`,
  the parameter-name pattern `param_name`); the agent test
  `run_wait_on_the_acceptance_generator_returns_ready_and_the_recorded_hash` (§ 10, agent tests); the CLI test
  `prune_removes_orphan_bakes_and_verify_names_them`.
