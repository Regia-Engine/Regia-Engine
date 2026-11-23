# Spec 23 — Agent-native engine (`regia_agent`)

**Regia Engine's defining feature.** Every capability a human has in the editor is available to an AI agent
through a typed, documented, testable API — and every agent action is observable, undoable, attributable,
and reviewable. Agents are first-class users, not a plugin bolted onto a human tool.

Context: Direct Order 27 already drives the legacy engine through third-party MCP plugins and editor-Python
generators that emit opaque binary `.uasset` files. Regia Engine removes that friction: native protocol,
text assets, self-describing types, deterministic capture.

## 1. Pillars

| Pillar | Meaning | Mechanism |
|---|---|---|
| **Text-first** | Everything an agent authors is diffable text | RON for scenes, prefabs, materials, VFX, UI, audio patches, input, configs; canonical serialization (sorted map keys, shortest round-trip floats, stable entity order) so re-saving an unchanged asset is byte-identical |
| **Self-describing** | Agents discover what exists and what it means | Reflection → JSON Schema per registered type, including doc comments (`bevy_reflect` `reflect_documentation` feature) and editor attributes (ranges, units, tooltips) |
| **Observable** | Agents can *see* results | Screenshots from any viewport/camera/asset preview, structured logs, diagnostics, profiler summaries, world queries, event streams |
| **Controllable** | Agents can act and test | Edit commands (same undo system as humans), play/step/pause, input injection, time control, build/hot-reload, cook/package |
| **Deterministic** | Same request → same result | Fixed-seed play sessions, exact frame stepping, capture settings without temporal jitter |
| **Safe** | Owner stays in control | Permission profiles, staged review queue, provenance on every agent-touched asset, audit log |

## 2. Architecture

```
 MCP client (Claude Code, other agents) ──stdio/HTTP──► `regia mcp` (rmcp adapter)
                                                             │ JSON-RPC 2.0
 scripts / CI / `regia agent call ...` ────────────────────┤
                                                             ▼
                         Agent Protocol server (in editor, or in a dev-build game, or headless editor)
                                                             │
                     EditorCommand / UndoStack · World queries · Capture · PIE control · Asset DB · Builder
```

- **Crate `regia_agent`** (layer 3, runtime-safe): protocol types (serde), the JSON-RPC server, and the
  runtime-side handlers (world query, property get/set, capture, input injection, step, logs). Feature
  `agent` — on in dev builds and the editor, **compiled out of shipping builds** unless the game opts in.
- **Editor handlers** in `regia_editor::agent`: assets, undo-integrated edits, PIE, builds, cook, review queue.
- **Transport:** WebSocket on `127.0.0.1` (random port written to `.regia/agent/endpoint.json`) or
  Unix socket / Windows named pipe; bearer token in `.regia/agent/token` (0600). Android dev builds:
  same server, reached via `adb forward`.
- **MCP adapter:** `regia mcp [--project DIR] [--attach|--headless]` runs an MCP server over stdio (and
  Streamable HTTP with `--http`) using `rmcp` (official Rust MCP SDK, Apache-2.0, add in M2). It attaches
  to a running editor or starts a **headless editor** (`regia editor --headless`: full editor, offscreen
  rendering, no window) — the mode used by CI and cloud agents. `--http` binds to `127.0.0.1` unless
  told otherwise, requires the bearer token and validates the `Origin` header (the MCP specification's
  DNS-rebinding protection).
- **Protocol versioning:** `agent.hello` returns protocol version + capability list; additive changes only
  within a major version; schema published as `regia://protocol/schema.json`.

## 3. Tool surface

Names below are MCP tool names (JSON-RPC methods use the same names with `.` separators).
A method applies `dry_run: true` only when its schema lists that parameter: the handler returns the preview and does not apply the edit. The dispatcher also skips the change log for a successful `edit_scene` call whose schema lists `dry_run` and whose params set it to true, or whose result has `applied: false` (before ADR-5628 it skipped any call that set the flag, so an edit applied by a method without `dry_run` left no record). A mutating method whose schema does not list `dry_run` (for example `nav.add_obstacle`) ignores the flag, applies the change, and returns its normal result. It does not return a diff or an undo id.

*Status (ADR-5628, conformance audit 2026-11-10).* The names in §§ 3.1 to 3.7 describe the surface; the
shipped JSON-RPC methods are listed in `book/reference/agent-methods.md` (generated from
`regia_agent::schema::METHODS`) and the editor's extension methods (`regia_editor::agent_ext` and the
`*_agent.rs` modules). Where a spec name is shipped under another name, or not at all:

| Spec name | Shipped as | Note |
|---|---|---|
| `frame(entity\|bounds)` | `editor.focus` (MCP `focus`) | frames entities by their bounds; no bounds-only form |
| `select`, `undo`, `redo`, `save_level`, `play`, `stop`, `import`, `create_asset`, `create_script` | `editor.*` of the same name | editor only (`editor_not_running` elsewhere) |
| `pause`, `step` | `play.pause`, `play.step` (runtime) and `editor.pause`, `editor.step` (PIE) | MCP `pause`/`step` name the runtime methods |
| (all editor methods over MCP) | `regia mcp` lists the attached app's `agent.schema` host methods with a tool name | since ADR-5628; before it, the MCP tool list held only the runtime table, so no `editor.*` method was an MCP tool. A tool name a runtime method already uses stays the runtime method's |
| `list_scripts` | `scripts.list` (MCP `scripts_list`) | |
| `find_references` | `registry.referencers` | the reverse of `registry.dependencies` |
| `validate_asset` | `editor.asset_validate` | |
| `get_import_settings` | `assets.import_rules` | read only |
| `profile(frames)` | `profiler.frame`, `profiler.capture` | |
| `record_input`, `run_recording` | `demo.record_start`, `demo.record_stop`, `demo.play` | shipped: dispatched by `regia_agent::dispatch` (`demo.*` arms) when the app has `DemoWorldPlugin` (`demo_not_installed` otherwise); `agents/demo.md` |
| `edit_asset` | `editor.edit_asset` | patch over the typed document's JSON view, canonically re-saved, one undo entry; `.material.ron`/`.prefab.ron`/`.level.ron`/`.variant.ron` only (ADR-5706) |
| `reimport` | `editor.reimport` | re-cooks the source with its cache ignored, keeping the `.meta` (id, settings, provenance) |
| `move_asset` | `editor.move_asset` | renames the file with its `.meta`; refused while its path is mentioned |
| `delete_asset` | `editor.delete_asset` | `confirm: true` required (§ 5); referencers reported |
| `set_import_settings` | `editor.set_import_settings` | writes the `.meta` settings (over folder rules) and re-cooks; read stays `assets.import_rules` |
| `build_game`, `hot_reload` | `agent.build_game`, `agent.hot_reload` (MCP `build_game`, `hot_reload`) | since ADR-5707: `build_game` runs `cargo build` on a `build:<n>` job with structured cargo-JSON diagnostics; `hot_reload` asks the editor for a rebuild on a `hot:<n>` job (`editor_not_running` elsewhere, `hot_reload_busy` while a reload runs) |
| `cook`, `package` | `agent.cook`, `agent.package` (MCP `cook`, `package`) | since ADR-7217: jobs (`cook:<n>`, `package:<n>`) that run `regia cook --json` / `regia package --json`, stream the output lines as progress and end with the CLI's JSON object and the artifact path; `build` permission; `package` needs `confirm: true`; desktop only (`cook_desktop_only` on Android); `book/agents/cook-package.md`. `cook.cache_stats`, `cook.chunks` read cook state |

No row of this table is unshipped any more: the last one, `record_input`/`run_recording`, ships as `demo.*` (audit 2026-11-15).

Build/launch parity (P-342–P-344, ADR-7843): `editor.launch_profiles` lists/saves/deletes
project recipes; `editor.launch` accepts a saved `profile`. `editor.platform_check` reads
or re-runs doctor diagnostics as a job. `editor.package_size_report` reads or refreshes
measured package contents as a job. Contracts and explicit deferred Android/telemetry
limits: `book/agents/editor-launch.md`. All three panels have keyboard paths.

### 3.1 Discovery
`project_info` · `list_types(filter)` · `describe_type(type_path)` (JSON Schema + docs + attributes +
example value) · `list_assets(query, kind, folder)` · `describe_asset(id|path)` · `guide(topic)` (bundled
docs written for agents) · `explain_error(code)`.

### 3.2 Scene
`query_entities(filter)` — by name glob, tag, component, prefab, spatial (within radius / box / frustum),
level · `get_entity(id, components?)` · `spawn(spec)` (components and/or prefab, parent, transform) ·
`spawn_primitive(shape, params, transform)` (greybox shape, `regia_render::Primitive`) ·
`set_property(entity, component, path, value)` · `add_component` · `remove_component` · `reparent` ·
`despawn` · `duplicate` · `apply_edits([...])` (atomic transaction, one undo step) · `select` ·
`frame(entity|bounds)` · `undo` · `redo` · `save_level`.

### 3.3 Assets
`import(source_path, settings)` · `create_asset(kind, path, value)` · `edit_asset(id, patch)` (patch =
list of `{path, value}` on the reflected asset) · `reimport` · `move_asset` · `delete_asset` (needs
confirmation, § 5) · `find_references` · `validate_asset` · `get_import_settings` / `set_import_settings`.

### 3.4 Seeing
`screenshot(target, size, view_mode, overlays)` → PNG as MCP image content. Targets: editor viewport,
camera entity, play window, asset preview (mesh/material/VFX on a neutral stage), UI screen at a given
resolution/safe area. Overlays: selection outline, entity labels, colliders, bounds, grid, light radii.
`contact_sheet(entity|asset, angles: n)` → one image of N orbit views · `compare_images(a, b)` → SSIM +
diff image · `capture_sequence(frames, every_n)` → animated strip for motion/VFX review.

### 3.5 Playing and testing
`play(mode: game|simulate, players, level?, seed?)` · `stop` · `pause` · `step(frames|seconds)` ·
`set_time_scale` · `inject_input(actions: [{action, value, at_frame, duration}] | raw events)` ·
`wait_until(condition, timeout)` (condition = world query + predicate, e.g. "entity Player within 2 m of
Door") · `expect(assertions)` → pass/fail report · `run_recording(id)` · `record_input(start|stop)` ·
`get_diagnostics` · `profile(frames)` → summarised top CPU spans and GPU passes.

### 3.6 Code
`build_game(profile)` → structured diagnostics (cargo JSON: file, line, message, suggestion) ·
`hot_reload` · `run_tests(filter)` · `create_script(name, template)` · `list_scripts` ·
`open_in_editor(file, line)` (for humans watching).

Shipped shape (ADR-5707): `agent.build_game` runs `cargo build --message-format=json` and answers
a `build:<n>` job whose result carries the structured diagnostics; `agent.hot_reload` asks the
running editor to rebuild and swap the game crate and answers a `hot:<n>` job whose result
carries the reload's outcome. Both refuse a second call while one runs, and both need the `build`
permission. `book/agents/code.md` has the parameters, events and error codes.

### 3.7 Logs, events, pipeline
`read_logs(since, level, target, contains)` · resource subscriptions for log and event streams ·
`cook(platform, quality)` · `package(platform, profile)` (confirmation) · `golden_test(name)` ·
`bless_golden(name, reason, job)` (stages a proposal; only the owner accepts it, at a terminal, with
`regia golden accept`; ADR-2821).

`diagnostics.report_sources` (ADR-8058) lists configured local log, crash, feedback
and pipeline-cache artifacts without uploads or report-body reads. It returns
configured directories, path-sorted artifact metadata, parsed cache-header keys,
scan issues and a truncation flag. The Rust API is
`regia_framework::report_sources` (existing `client` feature). The agent method
uses the default scan limits; Rust hosts can supply their own directories and
limits. `book/agents/report-sources.md` defines the result and error codes.

`report.outbox_status` (ADR-8402) reads the incremental report outbox's ledger
(`regia_crash::outbox`, `outbox.ron` beside the logs): per session log its size,
confirmed offset, pending bytes, crash-resend flag and last error. Read-only: it
reads no log contents and sends nothing. Error `outbox_unavailable`: no logs
directory and no ledger file. `book/agents/report-outbox.md` defines the result.

### 3.8 Resources (MCP)
`regia://project`, `regia://scene/current` (RON), `regia://types/{path}`, `regia://assets/{id}`,
`regia://logs/recent`, `regia://guide/{topic}`, `regia://protocol/schema.json`.

### 3.9 Editor UI parity (as built, ADR-7307)
Every editor button, menu item, toggle and checkbox has an agent path: the action registry
(`regia_editor::agent_parity::ACTIONS`, the shell's menus and toolbars drawn through its helpers) or a
classified row of `crates/regia_editor/agent-parity-allow.ron` (method, `editor.command` id, os_dialog,
confirm, view_only, or gap for the follow-up list). The `agent_parity_scan` test fails on an unclassified
control, and on a `gap` row its commented `KNOWN_GAPS` list does not name with a follow-up (ADR-7452; the
scan also counts the `a11y_audit::glyph_button(`/`bare_checkbox(` call sites); `editor.actions { panel?,
class? }` lists both. `cargo xtask editor-agent-coverage` (run by
`book-check`) holds every editor method to § 4's rules from `docs/api-snapshots/editor-agent-methods.txt`.
As built (ADR-7747): value editors are scanned too (drag values, combo boxes, selectable values, sliders, text
edits, angle and color edits), in the allowlist's last section with the same classes; a value-editor `view_only`
or `gap` row carries a `note` (why no route is needed; what is missing and the follow-up), and the reflect-driven
Inspector's widgets are `method` rows via `editor.set_property`. A `confirm` row must name its method; the
level's changed-on-disk prompt is `editor.level_reload_resolve`.

## 4. Agent-quality requirements on the rest of the engine
- **Error messages** carry a stable code (`F0412`), the offending asset/entity/path, and a fix hint;
  `explain_error` returns the long form.
- **Canonical text output** for all RON writers (§ 1) — enforced by a round-trip test per asset type.
- **No hidden state:** every editor-visible property is reachable through reflection; editor-only UI
  state that matters (selection, active level, play state) is queryable.
- **Long operations** (import, cook, build, bake) return a job id; `job_status(id)` streams progress;
  nothing blocks the protocol thread.
- **Source lint jobs** (ADR-8391): `asset.content_lint_start` returns a job id. `asset.content_lint_status` and `asset.content_lint_read` do not block. Cancel's code is `content_lint_cancelled`. Threshold edits and `asset.content_lint_apply_fix` need the `assets` permission. Apply does not write the source.
- **Agent guide** (`book/agents/`): task-oriented pages ("make a material", "add a script", "verify
  gameplay with wait_until/expect", "capture before/after for review"), served via `guide`.

## 5. Safety, review and provenance
- **Permission profiles** in `.regia/agent.toml` per client name/token. The names a profile can
  grant are `read`, `edit_scene`, `play`, `assets`, `tools`, `build` and `review`. A missing file
  grants `read`, `edit_scene`, `assets` and `play`. `build`, `tools` and `review` are not in that
  default. A string the parser does not know (`edit_assets`, `cook`, `package`, `delete`, and
  anything else) grants nothing.
  There is no `bless` permission and no way to pre-approve one: no protocol method writes under a
  `goldens` directory, and a golden proposal is accepted only by `regia golden accept` at a terminal
  (ADR-2821).
- **Staged review mode** (project setting, default on for asset edits): agent edits land in a staging
  layer shown in the editor's **Agent Changes** panel — per change: RON diff, before/after screenshot,
  agent's stated reason, accept/reject (single or batch). Accepted changes become normal undo entries.
  Mirrors the DO27 rule that the owner reviews every file before it enters a pack.
- **Provenance:** every asset created or modified through the protocol records in its `.meta`:
  `{ tool: "agent", client, session, model (if the client reports it), generator?, seed?, created_utc,
  reviewer: "pending" | "owner" }`. Policy in `Regia.toml`: `[provenance] require_review_for_shipping =
  true` makes `cook --quality final` refuse unreviewed assets. Projects can extend the record schema
  (DO27 needs `origin`, `foreign_game_file`, `third_party_mod_file`). The extension schema and the shipping policy live in
  `config/provenance.ron` (ADR-2428); `editor.asset_provenance` reads an asset's record and its policy findings.
- **Audit log:** `.regia/agent/audit.jsonl` — one JSON object per call with `seq`, `frame`,
  `unix_ms`, `method`, `params_hash`, `outcome`, `error_code`, `duration_us` and `client`.
  There is no result summary and no undo id.
- **Sandbox:** request paths use the project root and each method's documented explicit roots
  (see `book/agents/safety.md`); the shared output/replay rule also allows the system temp directory.
  The working directory is not an additional root (ADR-7871); it is only the project-root fallback
  when the host supplies no project. No network access from protocol handlers;
  rate limit (default 50 mutating calls/s) and per-call timeouts. This covers the protocol handlers
  only: `build_game`, `hot_reload`, `run_tests` and generators compile and run project code with the
  owner's privileges, so the `build` and `play` permissions amount to code execution. Grant them only to
  agents trusted like a developer with a shell on the machine, or run the headless editor in a
  container or VM. Narrow exception (ADR-7842): the opt-in editor collaboration controls
  `editor.collab_host` and `editor.collab_join` open one nonblocking UDP presence socket on an
  explicitly supplied numeric private, link-local or loopback address; it carries only
  camera/selection presence (no edits, no dispatch), and `editor.collab_leave` closes it. It never
  carries agent-protocol requests and does not change where the agent listener binds.
- **Scoped tool tokens** (ADR-1523): out-of-process editor tools connect to a second loopback listener
  with a token minted per launch and bound to the tool manifest's scope (method patterns and
  permissions; `.regia/agent.toml` still applies on top). The `tools` permission (launch, stop and
  drive tools from the protocol) is not in the default profile, and no tool scope can grant it.

## 6. Generators (procedural content as a first-class asset source)
- A **generator** is any executable (Rust bin in the project, a Python/Blender script, etc.) declared in
  `generators/<name>.toml`: command, inputs, seed, declared outputs. `regia gen run <name>` runs it;
  outputs are imported with provenance `generator + seed`; `regia gen verify` re-runs all generators and
  fails if bytes differ (DO27's pack-doctor rule, built in).
- Because materials, VFX, UI, audio patches and levels are RON, generators can emit engine-native assets
  directly — no editor in the loop. Meshes come in as glTF (Blender exports glTF natively).
- `regia_gen` helper crate (Tier 1.5): procedural mesh building (extrude, lathe, bevel, boolean-lite,
  UV box/planar projection), texture synthesis helpers (noise, masks, wear), heightfield tools.
  *As built (ADR-7240):* the mesh-building half, plus `gen.mesh`; *(ADR-7280):* the texture and heightfield
  half, plus `gen.texture` and `gen.heightfield`; see § 9.

## 7. Builder dogfooding
From M2 on, the building agent verifies its own milestones through this protocol: acceptance screenshots
in `docs/progress.md` come from `screenshot`, gameplay checks use `wait_until`/`expect`. If a check is
awkward through the protocol, that is a protocol bug to fix in the same milestone.

## 8. Acceptance
- **M2 (protocol v0, runtime only):** `hello`, `query_entities`, `get_entity`, `set_property`,
  `screenshot(window)` (camera and asset targets follow the renderer in M3/M4), `inject_input` (raw
  events; actions from M3), `step`, `read_logs` over WebSocket; `regia agent call` CLI;
  `regia mcp` stdio server passes the MCP conformance checks of `rmcp`'s test client; used by the
  builder to verify `hello_window` and the M3 slice.
- **M11 (full surface):** every tool in § 3 implemented with integration tests; staged review, permissions
  and audit log tested (denied calls, confirmations, provenance written).
- **Agent benchmark (owner-run at M11 and every later milestone):** `bench/agent_tasks/*.toml` — 25
  scripted tasks with machine-checkable success criteria, e.g. "place 30 rocks on the terrain, none on the
  road spline", "make the sun lower and warmer and show me before/after", "fix the broken material
  reference in level X", "add a script that opens the door when the player is within 3 m and prove it in
  play". Target: ≥ 80% success by a current Claude model through MCP with no human help; failures become
  protocol/docs issues. Each run uses only tasks whose features exist at that milestone (terrain tasks
  join the set after M13); the set is topped up to 25 as features land.

## 9. Conformance status (ADR-5628, audit 2026-11-10)

What the audit of this spec found on the v0.5 line, beyond the § 3 name table. Built and tested items
are not listed; the lane report has the full table with test names.

- **§ 2 transports:** WebSocket on `127.0.0.1` plus the local transport: a Unix domain socket
  (Linux/macOS, `.regia/agent/agent.sock`, 0600 in the 0700 agent dir) or a Windows named pipe
  (`\\.\pipe\regia-agent-<pid>-<serial>`, owner-only DACL, first instance), same JSON-RPC messages over length-prefixed
  frames with an acknowledged bearer handshake (ADR-5878), advertised in the owner-only `socket` file and preferred by
  `Client::connect_project` (ADR-5708, `book/agents/local-transport.md`,
  `tests/protocol_transport.rs` `spec23_sec2_*`). No local transport on Android (`adb forward`
  to the WebSocket port). The `agent` feature is a default feature of `regia`: packaged
  shipping builds turn default features off and `regia ship-check` fails a named shipping
  profile that still links the server, but the plain `shipping` profile only warns
  (`ship_check.rs`, `check_binary_features`).
- **§ 3.4 seeing:** as built (ADR-7217), the runtime `seeing.screenshot` serves the `camera` target (one
  camera entity's view, by `entity` or `name`), the `ui` target (the runtime UI layer alone over
  transparent black, at a size and HUD layout) and the `colliders`, `light_radii`, `bounds` and `grid`
  overlays (CPU-drawn over the read-back picture, no depth test). `selection` and `labels` stay editor
  state (`F0013`, `overlay_needs_editor`); `view_mode` still answers `F0013`. The editor's viewport
  capture has grid, labels, bounds, the selection outline and, since ADR-7217, collider and
  light-radius overlays (`editor.set_viewport { colliders, light_radii }`). UI safe-area previews are
  the UI designer's (`editor.ui_designer_*`).
- **§ 4 guide:** `agent.guide` serves topic text compiled into `regia_agent` (`GUIDE_TOPICS`), not the
  `book/agents/` pages themselves; some topics have no page of the same name.
- **§ 5 staged review:** staging is an explicit API (`review.propose`), not the default for asset edits
  (ADR-2346, deviation recorded there).
- **§ 6 `regia_gen`:** built (ADR-7240, `book/agents/gen-mesh.md`): extrude with a rounded cap bevel,
  lathe, box, plane clip with cap, convex intersection, disjoint merge, planar and box UV projection
  over `CpuMesh`, bit-identical on every platform, and the agent method `gen.mesh { recipe, mesh? }`.
  Texture and heightfield half built (ADR-7280, `book/agents/gen-textures.md`): seeded value, gradient
  and cellular noise with fBm or ridged octaves, tileable on request; threshold, levels, blend, blur over
  an R32F image with RGBA8 / R16 / R32F output; edge and cavity masks from a mesh's dihedral angles baked
  into UV space; an edge-wear and grime recipe; `Heightfield` noise fill, terrace, flatten-to-pad, clamp,
  normalize, thermal erosion, slope and normal maps, terrain-heightmap and mesh-grid export; agent methods
  `gen.texture { recipe }` and `gen.heightfield { recipe }`. Mesh primitives and contours built
  (ADR-8044, `book/agents/gen-mesh.md`): rounded box, chamfered and knurled cylinder, annulus,
  tube, rounded-rectangle sweep and section sweep; marching-squares contours (`gen.contour`).
  Not built: a union or difference of
  overlapping solids (the pipeline's generator value noise stays as it is; graph nodes in `regia_procgen`).
- **§ 8:** the 25 tasks and the in-process oracle exist (ADR-2621); the scored model run is owner work.
  rmcp HTTP conformance needs `reqwest` (not approved). The § 3 name table has no unshipped row left
  (`record_input`/`run_recording` ship as `demo.record_start`/`demo.record_stop`/`demo.play`).
- **§ 2 / § 5, as built (ADR-7454):** the WebSocket and local inboxes share the 64-requests-a-frame
  budget in turns (one answered request each, WebSocket first every frame); `agent.changes` pages
  (`limit`, default 200, clamped to 1..=1000; `next` is the following page's `since`); the audit
  `params_hash` streams the params' JSON into FNV-1a without building the string
  (`book/agents/local-transport.md`, `book/agents/safety.md`).

Frame inspection (ADR-7824, proposed): the editor extension installs
`editor.frame_capture { action: start|state|resume }` and `editor.frame_pass { index }`.
They share the panel's operations and read permission, return explicit missing-target
and missing-draw-detail status, and leave authored content and undo history unchanged.
The capture request completes on the next main viewport render; the pass route returns
checkpoint targets as PNG base64 images. Contracts and limits: `book/agents/frame-debugger.md`.

Frame inspection pass copies (ADR-7860, proposed): `editor.frame_capture` `start` takes
optional `passes` (built-in pass names) and `budget_bytes`; state adds `pass_copies`, `skipped`
and `request`, and `editor.frame_pass` rows add `skipped`. New error codes
`capture_unknown_pass` and `frame_capture_budget` (book `agents/frame-debugger.md`).

Viewport overlay parity (ADR-8199): `editor.viewport_overlays` shares its schema
between runtime discovery and the editor extension. Runtime-only hosts return
`editor_not_running`; the editor dispatches the same layout, preset and selection
operations as the keyboard menus. Contract: `book/agents/viewport-tools.md`.
