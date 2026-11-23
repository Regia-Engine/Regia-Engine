# Shader-permutation policy

A material graph compiles its own pipelines: each graph code has a **pipeline set** per render
target (forward, depth, the three blend modes, transmission, and on GPU-driven targets four
instanced variants; a graph with a vertex stage adds shadow and velocity pipelines). The engine's
own pipelines are a fixed set covered by the warm list, and the forward shader's define axes are
closed (`render.graph` → `forward_permutations`). Graphs are the open axis, so the engine
counts and limits them (master plan § 5.5a, ADR-2026):

1. **Cook-time enumeration.** Every cook reads back each cooked `.matgraph.ron` and
   `.matinst.ron` (cache hits included) and lists, for every render profile the platform runs,
   the graph pipeline sets that profile draws and the passes in each. Android enumerates
   MobileLow and MobileHigh; Linux enumerates DesktopLow, DesktopHigh, DesktopUltra and Deck;
   Windows and macOS enumerate the three Desktop profiles. The list is `permutations.json` in
   the platform's cooked cache (`.regia/cooked/<platform>/`, format below). Two instances that
   compile to the same code count once.
2. **A budget per profile.** Each profile has a ceiling on graph pipelines and on the
   estimated cold compile time (no driver cache). The cook prints one line per profile:

   ```text
     permutations DesktopHigh: 12 graph sets, 120 pipelines (budget 4096), 60 modules, 18432 B code, ~3.2 s cold compile (budget 120 s)
   ```

   These lines count pipelines per profile. The cook's other line, `forward shader permutation(s)
   from material graphs`, counts the forward shader's define sets (at most 4), which is a different
   number.

   `regia package` checks the list: over budget **fails a shipping package** and warns on a dev
   one. A dedicated-server package (`--server`) has no renderer, so it skips the check and ships no
   list. Defaults: Desktop and Deck 4096 pipelines and 120 s, Mobile 1024 pipelines and 90 s.
   Override them per profile in `Regia.toml`:

   ```toml
   [cook.permutation_budget.MobileHigh]
   max_pipelines = 512
   max_compile_ms = 45000
   ```

   The compile time is an estimate (`CompileCostModel`: per module, per pipeline and per KiB of
   graph code). Its constants are provisional until they are calibrated on the reference
   devices.
3. **The runtime guard.** `regia package` ships the list as `content/permutations.json`, next to
   the paks, and the render plugin installs it when the game starts. A dev run reads the list from
   the cooked cache instead. A graph the list names builds as before. A graph it does not name:
   - with `warn` (dev builds and dev runs) is built and counted, with one warning per graph;
   - with `fallback` (shipping builds) is never built: the draws of that graph keep the
     material's standard surface in the same pass.

   The check is on the graph code, because that is what content can grow without bound. A listed
   graph on another target is admitted: the double-sided set of a scene that turns culling off, or
   another sample count (a probe capture on an MSAA profile, a device without 4x). Those sets are
   counted in `off_target`, because they are above the cooked count.

   The guard decides once per set and remembers the answer, so it costs nothing per draw.
   Neither mode blocks the frame. With asynchronous material graphs (`RuntimeRenderer`) a
   listed set builds on worker threads, and its draws use the standard surface until the set is
   ready.

   There is no check when a package has no list, when the list names no graph at all (a project
   whose graphs are made in code), or when the file is damaged (a warning, then the game runs).
   `REGIA_PERMUTATION_POLICY=off|warn|fallback` overrides the mode at launch. A host that makes
   graphs in code calls `FrameRenderer::set_permutation_guard` with its own guard or
   `PermutationGuard::off()`: a guard a host installs wins over the automatic one.

## Graph bindings per device class and static switches (ADR-4901)

The pipeline count does not show whether a set can be created on a device. Every cook composes
each distinct graph code's pipeline set once (the forward module for the colour, instanced and
depth passes, and for a vertex-stage graph the shadow and velocity modules) and counts what each
entry point reads: uniform, storage and sampled bindings, samplers, bind groups and the largest
binding. The counts are held to the device class of every profile of the platform, the classes of
`cargo xtask check-shaders` (`docs/06-testing-and-ci.md`): `mobile` for MobileLow and MobileHigh,
a gate, and `desktop` for the Desktop and Deck profiles, which only reports. The cook prints one
line per class and one for the static switches:

```text
  material graphs graph bindings vs `mobile` (MobileLow, MobileHigh): 3 code(s), worst uniform_buffers <used>/<limit>, sampled_textures <used>/<limit>, ...
  material graphs static switches: 3 of 4 cooked graph(s) declare switches, 2 switch permutation(s) cooked, most 1 in one graph (limit 16)
```

These fail explicitly (`CookReport::permutation_problems`, and `regia package` fails a shipping
package with `material_graph_layout_invalid`):

- a set over a `mobile` limit, naming the graph, the profiles, the metric, the pass module and the
  entry point. The `desktop` class is wgpu's portable defaults, which the engine's own forward
  shader passes by design on desktop (it asks for the adapter's limits, ADR-3002), so a set over it
  only marks that class's line (`over this report-only class in sampled_textures`);
- cooked graph code a host shader rejects (`generated_code_invalid`; no authored node maps to it,
  so it is reported, never skipped);
- a cooked graph with more than 16 static switches.

The graph compiler bounds the two open axes inside one graph: 16 static switches and 4096 flat
nodes after the functions expand (`book/agents/material-graph.md`, Limits). The test
`every_shipped_profile_warms_exactly_the_sets_its_cook_lists` (`regia_matgraph`, lavapipe) warms
each shipped profile's graphs for the target the runtime draws them with
(`ProfileTarget::graph_target`) and checks the sets and pipelines built are the ones the cook
lists, and that a second warm builds none.

## Shipped shader cost per profile

The graph pipelines are the open axis. The other half of what a profile ships is the engine's own
forward shader: one composed module per define set (`render.graph` → `forward_permutations`). The
cook report counts it per render profile of the platform (ADR-2664):

```text
$ regia cook --size-report --platform android
shipped forward shaders (android):
  MobileLow: 14 permutations, 2310412 B WGSL, 3187552 B SPIR-V, compile cost ~1620 ms (estimate), naga 913 ms (measured)
  MobileHigh: 14 permutations, ...
```

- `permutations`: define sets that compose and validate. `failed` counts the ones that do not
  (a number above 0 is a shader bug; the shader matrix names it).
- `wgsl_bytes`: naga's WGSL re-emit of each composed module, summed. `spirv_bytes`: the SPIR-V naga
  writes for all entry points of each module, summed.
- `compile_cost_ms` is an **estimate** from `CompileCostModel` (per module, per pipeline, per KiB),
  the same provisional constants as the graph row. `measured_naga_ms` is a **measurement**: the
  time this host spent in naga's front end and back ends for the profile. It varies from run to
  run, is never part of a budget and nothing compares it.
- `regia cook` prints the lines after the cook summary; `regia cook --size-report --json` carries
  the same rows under `shaders` (`{ format_version: 1, platform, profiles: [...] }`, additive to
  schema 1); `render.permutations { shader_costs: true, platform? }` returns them as
  `shipped_shaders` (see below).

**Budget row.** Each profile has a ceiling on `wgsl_bytes` (Desktop and Deck 256 MiB, Mobile
48 MiB, provisional). A new permutation axis in the forward shader multiplies the define sets and
trips it. Override it in `Regia.toml`:

```toml
[cook.permutation_budget.MobileHigh]
max_shader_bytes = 50000000
```

### Platforms the project does not ship

`regia package` checks every platform the cook knows (`linux`, `windows`, `macos`, `android`), not
only the one being packaged. The project ships the platform it packages and any listed in
`Regia.toml`:

```toml
[cook]
ship_platforms = ["linux", "windows"]
```

| Code | Meaning | Effect |
|---|---|---|
| `permutation_budget_exceeded` | a graph-pipeline or shipped-shader-bytes row is over budget on a platform the project ships | fails a shipping package, warns a dev one |
| `permutation_budget_off_target` | the same overrun on a platform the project does not ship | always a warning |

The message reads `permutation_budget_off_target: android graph pipelines over budget: MobileLow:
12 material-graph pipelines, budget 10`. The codes are the first word of the message, so a script
can match them.

What to do when a profile is over budget: merge graphs that differ only in constants (use
parameters, which need no new pipelines), turn static switches that make many instances into
parameters, or raise the budget for that profile.

## `render.permutations`

Read-only. Returns:

```json
{
  "guard": {
    "enforcement": "fallback",
    "profile": "DesktopHigh",
    "source": "/opt/game/content/permutations.json",
    "listed_sets": 12,
    "listed_pipelines": 120,
    "admitted": 9,
    "off_target": 0,
    "unlisted_built": 0,
    "refused": 1,
    "unlisted_keys": ["9f2c0b6e41d7a385"],
    "cooked": [{ "profile": "DesktopHigh", "sets": 12, "pipelines": 120, "modules": 60,
                 "code_bytes": 18432, "estimated_compile_ms": 3200,
                 "budget": { "max_pipelines": 4096, "max_compile_ms": 120000 },
                 "over_budget": false }]
  },
  "graphs": { "ready": 9, "pending": 0, "failed": 0, "built_on_frame_path": 0,
              "built_off_frame_path": 9, "fallback_draws": 42 },
  "forward": { "count": 11, "budget": 256, "over_budget": false, "undeclared": [] },
  "default_budgets": [{ "profile": "MobileLow", "max_pipelines": 1024, "max_compile_ms": 90000 }]
}
```

- `guard.enforcement` is `off`, `warn` or `fallback`. `profile` is `null` while no list is
  installed. `source` is the `permutations.json` path, or `agent` for a list passed inline.
- `guard.cooked` holds the list's totals for every profile of its platform, so one call shows
  the counts and budgets of all of them.
- `off_target` counts the admitted sets the list did not count (see the guard above). A high
  number means a project draws its graphs on a target its profile does not describe.
- `refused` counts the sets the guard refused. `graphs.fallback_draws` counts the draws that
  used the standard surface, whether the set was refused or still building.
- `unlisted_keys` holds the first 32 unlisted graph keys (16 hex digits, the same key as in
  `permutations.json`).

F0003 when the app has no `RuntimeRenderer`.

Optional params: `shader_costs` (boolean, default false) and `platform` (`linux` default,
`windows`, `macos`, `android`). With `shader_costs: true` the answer also holds `shipped_shaders`,
the cook report's per-profile rows from [Shipped shader cost per profile](#shipped-shader-cost-per-profile):

```json
"shipped_shaders": { "format_version": 1, "platform": "android", "profiles": [
  { "profile": "MobileLow", "permutations": 14, "failed": 0, "wgsl_bytes": 2310412,
    "spirv_bytes": 3187552, "compile_cost_ms": 1620, "measured_naga_ms": 913.4,
    "budget": { "max_wgsl_bytes": 50331648 }, "over_budget": false } ] }
```

Composing the shaders takes seconds the first time in a process, which is why it is opt-in. An
unknown `platform` answers `permutation_unknown_platform` (invalid params).

## `render.set_permutation_policy`

Mutating (permission `assets`). Installs a list, or turns the check off:

```json
{ "enforcement": "fallback", "profile": "DesktopHigh", "path": ".regia/cooked/linux/permutations.json" }
```

| Param | Meaning |
|---|---|
| `enforcement` | `off`, `warn` (default) or `fallback`. `off` needs nothing else. |
| `profile` | The render profile whose list is checked. Default: the platform's default profile. |
| `manifest` | The cooked `permutations.json` object, inline. |
| `path` | Project-relative path of a cooked `permutations.json` (ADR-1816). Absolute paths, `..` and a symlink that leaves the project are `path_outside_project`. A missing path or a non-regular file (a FIFO) is `F0003` and is not read. The read is capped at 256 MiB. |
| `graphs` | `[{ key, vertex?, double_sided? }]`: graph codes to list now, with the default budgets. |
| `platform` | The platform whose profiles `graphs` is listed for (default `linux`). |

Answers the new `render.permutations` state. Error codes:

| Code | When |
|---|---|
| `permutation_bad_enforcement` | `enforcement` is not `off`, `warn` or `fallback`. |
| `permutation_manifest_unreadable` | No list given, the list is not the `permutations.json` shape, it is newer than the engine, a key is not 16 hex digits, or a file inside the project could not be read as text. |
| `permutation_unknown_profile` | The list has no enumeration for `profile`. |
| `path_outside_project` | `path` is absolute, contains `..`, or a symlink leaves the project (`F0002`). |
| `F0003` | The app has no `RuntimeRenderer`, or `path` is missing or not a regular file. |

A check an agent can run after a level loads: in a dev run the cooked list is already installed
with `warn`. Install it with `fallback` to see what a shipping build would draw, play the level,
then call `render.permutations`. `refused` should be 0. Any key in `unlisted_keys` is a graph the
cook did not see (a graph made at runtime, or a cache cooked before the graph was added). Recook.

## `permutations.json` (format version 1)

```json
{
  "format_version": 1,
  "platform": "linux",
  "graphs": 12,
  "profiles": [{
    "profile": "DesktopHigh",
    "target": { "format": "rgba16f", "samples": 1, "instanced": true },
    "entries": [{ "key": "9f2c0b6e41d7a385", "name": "assets/materials/rust.matgraph.ron",
                  "double_sided": false, "passes": ["depth", "forward", "blend", "premul",
                  "additive", "transmit_multiply", "forward_instanced", "forward_mask_instanced",
                  "depth_instanced", "depth_mask_instanced"] }],
    "pipelines": 120, "modules": 60, "code_bytes": 18432, "estimated_compile_ms": 3200,
    "budget": { "max_pipelines": 4096, "max_compile_ms": 120000 },
    "over_budget": false
  }]
}
```

The file is derived data: a cook writes it when the list changes, and nothing edits it by hand. A
reader refuses a `format_version` newer than its own. `target.instanced` and the colour format are
what the profile asks for. A device without GPU-driven draws builds fewer pipelines, and one that
cannot render `rg11b10` uses `rgba16f`. The guard decides on the graph key only; the sidedness and
the sample count feed `off_target`.
