# Extensions

An **extension** (spec 57) is a versioned bundle a project enables without forking the engine: a dialogue system, an
in-house importer, a pack of art, a gameplay feature levels can switch on. A `regia-plugin.toml` manifest describes it
(`docs/formats/regia-plugin-manifest.md`). It contains Rust code, content, helper programs, or any mix.

**In-process extension code is trusted native code with the full privileges of the host process.** Capabilities are a
declaration and review aid, not a sandbox. Install an extension only from a source whose code you reviewed or whose
publisher you trust.

## Task flow

1. `plugin.list` shows every discovered extension with its state.
2. `plugin.info { id }` shows what it is and what it asks for: `requested` and `granted` capabilities, dependencies,
   registrations, packs, mounts, diagnostics, tools, library and stamp.
3. `plugin.check { enabled: [...], platform, profile }` runs the resolver as a dry run. Nothing changes. Use it before
   enabling, and with `platform: "android"` when the game ships there.
4. `plugin.enable { id, grant: [...] }` edits `[extensions]` in `Regia.toml`, keeping comments. A grant the manifest does
   not request is not stored. Every capability the manifest requests must be granted, except `systems` and `settings`, which enabling grants (`plugin.info` lists them
   as `implicit`). Without the grant the answer is `capability_not_granted` with `data.missing`.
5. Code takes effect at the next start (`restart_required: true`). A content-only extension mounts at once. After the
   restart, verify with `plugin.list`, and wait with `wait_until { condition: { events_contain: { kind: "extension",
   contains: "Built" } } }`: every `ExtensionEvent` is an `events.read` record of kind `extension`, named by its variant
   (`Resolved`, `Built`, `Failed`, `Enabled`, `Disabled`, `Reloaded`, `PackActivated`, `PackDeactivated`).
6. Read `diagnostics`. On `plugin.list` each entry's `diagnostics` is a list of lowercase code strings. On `plugin.info` (and on `plugin.check` and `plugin.enable`) each diagnostic is an object with `code`, `severity` and `message`.

`plugin.enable`, `plugin.disable`, `plugin.reload` and `plugin.tool_run` need the `build` permission. It is not in the
default profile: a person adds it to `permissions` in `.regia/agent.toml`, because it decides which native code runs.
A tool's scoped token can never hold it. Without it the call answers `F0012`.

## Methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `plugin.list` | read | `state?` | `extensions: [{ id, version, state, kind, phase, publisher, licence, diagnostics, generation, load_ms }]`. `diagnostics` here is a list of code strings, not objects. `load_ms` is informational, `null` when no code built here. Also `resolved_hash`, `order`, `restart_required`. `available: false` when the app has no project. |
| `plugin.info` | read | `id` | Manifest fields, `requested`, `granted`, `dependencies`, `code`, `content`, `tools`, `registrations` by kind, `packs`, `mounts`, `diagnostics`, `build`, `dynamic`, `generation`, `settings_type`, `licence_files`, `trust`. |
| `plugin.check` | read | `enabled?`, `platform?`, `profile?` | `order`, `diagnostics`, `states`, `resolved_hash`, `platform`, `profile`. |
| `plugin.enable` | build | `id`, `grant?`, `platform?` | `enabled`, `restart_required`, `state_after_restart`, `diagnostics`, `mounted`, `mount_errors`. |
| `plugin.disable` | build | `id`, `platform?` | `disabled`, `restart_required`, `dependents_affected`, `unmounted`. |
| `plugin.reload` | build | `id` | `generation`, `removed` and `added` counts by kind. Dynamic extensions, editor host only. |
| `plugin.tool_run` | build | `id`, `name`, `args?`, `stdin?` | `status`, `stdout`, `stderr`. Answers from its own thread when the child ends. |

`state` is `active` (resolved; `build.state` says whether its code built here), `failed`, `filtered` (platform, profile or
audience scope; an extension whose code entries all name other audiences and that has no content or tools is `filtered`
in this host, with an info diagnostic `filtered`, ADR-7237), `disabled` (the project lists it) or `available` (discovered, not enabled). `resolved_hash` lets an
agent detect that the set changed: the same manifests and inputs give the same hash.

`platform` on `plugin.enable` and `plugin.disable` edits the `[extensions.platform.<os>]` delta instead of the base lists.

## Extension methods

Extensions with the `agent_method` capability register methods named `<id>.<verb>[.<noun>]`. They appear in the `methods`
list of `agent.hello` and in the `extension_methods` array of `agent.schema` with `provider: "extension:<id>"`,
`permission`, `params_schema` and `result_schema`, and they go through the same permission, rate-limit and audit path as
built-in methods. Their access class is `read`, `play` or `edit_scene`.

A method in the namespace of a discovered extension that is disabled, failed or not built answers `method_unavailable`
with `data.id` and `data.state`, not the unknown-method error. A failure in an extension's handler carries the
extension's own lowercase code in `data.code` (`extension_error` when that code is empty or not lowercase ASCII, digits
and `_`), its data in `data.detail`, and `data.provider`.

The params of an extension method are checked against its `params_schema` before the handler runs (ADR-2136). A call
that breaks it answers `invalid_params` (-32602) with `data.path` (a JSON Pointer into the params, empty for the root),
`data.rule` (the schema keyword that failed) and `data.detail`; the handler is not called. Absent params count as `{}`.
The check covers a bounded subset of JSON Schema: `type` (including `integer`), `enum`, `const`, `required`,
`properties`, `additionalProperties`, `items`, `minItems`, `maxItems`, `minLength`, `maxLength`, `minimum`, `maximum`,
`exclusiveMinimum`, `exclusiveMaximum`, `allOf`, `anyOf`, `oneOf` and `not`. Other keywords (`pattern`, `$ref`, `format`)
are not checked, so the handler still validates what they would have said. Values nested deeper than 32 levels are refused
with `data.rule: "depth"`. The first violation in key order is reported, so a given call always gets the same answer.

The former `dialogue.say` request example has been removed: `dialogue.say` is not a built-in
method in the current agent schema. An extension must register its own method before clients can call it. The following reply is an example
from a registered extension method whose schema requires a string `line`:

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"dialogue.say: invalid_params: /line: expected \"string\", got integer (type)",
 "data":{"code":"invalid_params","id":"dialogue","path":"/line","rule":"type","detail":"expected \"string\", got integer",
         "provider":"extension:dialogue","hint":"see book/agents/plugins.md"}}}
```

## Linking source extensions into a game

A source extension is an ordinary crate. It implements `RegiaExtension` and ends with `regia::extension!(MyExtension);`,
which exports `regia_extension()`. `regia plugin sync` writes the committed `src/extensions_generated.rs` and names any crate
missing from the game's `Cargo.toml`. The game includes the list and installs the plugin:

```text
include!("extensions_generated.rs");

app.add_plugins(regia::extension::ExtensionPlugin::for_project(project_dir, linked_extensions()));
```

That is a development run: it reads `Regia.toml` and the manifests from disk. A packaged game has neither, on any platform, so
install the set the generated file carries, which works in both cases:

```text
include!("extensions_generated.rs");

app.add_plugins(regia::extension::ExtensionPlugin::for_game(project_dir, linked_extension_set()));
```

`for_game` reads the project when `project_dir` holds a `Regia.toml`, and otherwise uses what `regia plugin sync` compiled in:
the `[extensions]` table (grants, settings, order) and the manifest of every extension in the set. Run `regia plugin sync`
after changing the enabled set; a binary built from a stale list and content cooked from a newer one report
`extension_set_mismatch`.

An enabled extension whose crate is not in that list fails with `manifest_not_linked`; a linked crate that is not enabled
warns `linked_not_enabled`. A game that installs nothing still gets a registry when it runs with the agent, so `plugin.*`
can read the project; its code extensions then show `manifest_not_linked`.

## Reloading a dynamic extension

A **dynamic extension** is a source extension built by `regia plugin build` into a library the editor loads. It is
same-toolchain, same-engine-build code, for development only: it never loads in a shipping player and it does not exist on
Android. The loop:

1. Change the extension's code; run `regia plugin build <id>`.
2. `plugin.reload { id }` reads the new library's stamp from the file (the exported `REGIA_EXTENSION_STAMP` symbol, found by
   parsing the ELF, PE or Mach-O tables, never by searching for bytes) and compares every field with this build, before any
   of its code runs, so a stale build never loads. A mismatch answers `abi_mismatch` with the first differing field in
   `detail`, and nothing changes. The exported stamp is compared again after the open, which is what catches a corrupted file
   the first check cannot see; that case runs the library's initializers, though never its entry point.
3. The editor copies the library to a unique name, loads it, calls `on_unload` on the old copy, swaps the registrations,
   calls `build` and `on_load`, and increments `generation`. If `build` fails, the old registrations come back and the
   answer is `reload_failed`; the old code keeps running.

The old library stays mapped for the life of the process. The old copy's systems leave and the new copy's join at the next
frame boundary, including systems a plugin added at startup, so nothing runs twice. Systems in `First` wait for the end of
that frame, because a schedule cannot be changed while it runs. A new copy adds its systems with `cx.add_systems`: a plugin
builds against the app, which a running frame does not have, so `add_plugin` during a reload answers `restart_required` and
the reload fails with the old copy intact. A replay that crosses a reload is not reproducible; the log carries
`nondeterministic_reload`, and so do the session's determinism records (ADR-9850): `sim.state_hash` and `sim.state_diff`
list it under `nondeterministic`, and a demo recorded in the session carries it (`demo.inspect` `nondeterministic`). See
[Determinism](determinism.md).

A source extension reloads with the game's hot reload, so `plugin.reload` answers `not_supported` for it, and for a
content-only extension (disable and enable it to remount).

## What an extension can register

| Capability | Registration | Notes |
|---|---|---|
| `systems` | `cx.add_systems(schedule, systems)`, `cx.add_plugin(p)` | Implicit. Plugins are named `ext/<id>/<name>`. Both land in the extension's `ExtensionSet(id)`; `plugin.info` lists its schedules under `registrations.system_set`. Only `add_systems` works during a reload. |
| `settings` | `cx.declare_settings::<T>()` | Implicit. From `[extensions.settings.<id>]`; unknown keys warn `unknown_field`. `settings.get` / `settings.set` reach it as `<id>` or `<id>.<field>` (answers carry `provider: "extension:<id>"`; a `GameSettings` field of the same name wins). Changes are saved to the user settings file (`extensions.<id>`) and win over the table at the next start (ADR-7237). |
| `asset_loader` | `cx.register_asset_loader(...)` | By file extension. `ExtensionRegistry::load_content(path, ...)` resolves a mount and decodes through it; `load_cooked_content(server, path)` reads the cooked file, which is what a packaged game has. |
| `agent_method` | `cx.register_agent_method(...)` | Names are `<id>.<verb>[.<noun>]`. |
| `content_mount` | `cx.mount_content(root)`, or `[[content]]` in the manifest | Read-only `ext://<id>/`. The project's `assets/` shadows it. A path two extensions provide is `content_collision`: the later mount loses that path. Below: what a cook takes. |
| `feature_pack` | `cx.declare_feature_pack(...)` | Below. |
| `asset_importer`, `editor_panel` | `ToolExtensionContext::new(cx)?` in editor code | Editor host only; elsewhere the wrapper answers `not_supported`. `editor_panel` covers panels, inspectors, menu and toolbar actions (commands with shortcuts and selection predicates), asset tools, viewport tools, overlays, typed settings and build hooks: [Editor extensions](editor-extensions.md). |
| `render_pass` | `cx.add_render_pass(Box::new(pass))` | A `FramePass` at a frame-graph slot (`after_opaque`, `after_transparent`, `before_post`, `after_post`, `overlay`), recorded from the first frame the renderer exists. `render.graph` lists it. A malformed name is `render_pass_invalid`; a name the graph refuses at run time warns `render_pass_invalid` and the frame goes on. Passes leave and arrive with a reload. |
| `process`, `network` | `[[tool]]` entries | Below. |

Every registration checks the capability and records `(id, kind, name)`, which `plugin.info` lists. A second registration of
the same `(kind, name)` is `registration_conflict`; the first stays.

## System order

Extensions run in the order their dependencies give: an extension's systems run after those of everything it depends on, in
every schedule they share. Two extensions with no dependency edge have no defined order unless the project sets one:

```toml
[extensions.order.dialogue]
after = ["cheats"]
```

`before` works the same way. An entry that closes a cycle is `dependency_cycle` and is ignored; one naming an extension that
is not active warns `not_found`. Neither stops the app. Order against an extension's systems from game code with its
`ExtensionSet("id")` label.

## Content in a cook

`regia cook` and `regia package` take only what the project references. References are `ext://<id>/<path>` strings in text
files under `assets/` and `src/` (`.ron`, `.toml`, `.json`, `.gltf`, `.rs`), followed through the extension files they reach.
A path built at run time is invisible to this scan: write the full path in one of those files. Each file cooks like a project
asset and keeps its `ext://` path, so `AssetServer::id_for_path("ext://dialogue/talk.dlg")` finds it. A file type no built-in
importer takes is stored as it is, for the extension's loader. A project file at the same relative path under `assets/`
shadows the extension's, and the `ext://` path then names the project's asset. A reference nothing provides is a warning. A `[[content]]` root that leads out of the extension directory through a symlink is
`path_escape`: it is not mounted, and it stops a cook.
Sources are copied to `.regia/ext/` first, so a cook writes nothing into an extension directory.

## Feature packs

A pack adds reflected components to entities carrying a tag, only while a level asks for its feature. Put a
`LevelFeatures { features: ["rain_weather"] }` component on any entity of the level; it loads and unloads with the level.
Activation is counted per level, so two overlapping levels keep a pack on until both are gone. Withdrawal takes one frame
and removes what the pack added; a component the entity already had stays untouched. An entity that loses the tag loses what
the pack added to it. A reload keeps a pack's state when the new copy declares it unchanged; otherwise it withdraws at once. Pack systems use the
`pack_active("id")` run condition. `plugin.info` lists every pack with its state and how many entities it touches.

## Helper tools

`[[tool]]` entries are separate programs. `plugin.tool_run` and `regia plugin tool` run one with a cleared environment
(only `PATH` and the platform's minimal system variables), a timeout and an output bound, and kill it on either limit
(`tool_timeout`, `tool_output_too_large`). This is process separation only: no OS-level isolation is applied or promised.
The project must grant `process`; a tool listing `network` needs that grant too, which is informational since nothing blocks
sockets. A capability only a `[[tool]]` lists gates that tool: the extension loads without it, and `plugin.info` still shows it
under `requested`.

## Errors

| `data.code` | JSON-RPC | Meaning |
|---|---|---|
| `not_found` | -32001 | No such extension or tool. |
| `not_supported` | -32004 | No project directory, or a reload of a source or content extension. |
| `manifest_invalid` | -32602 | `Regia.toml` could not be edited in place, or a manifest did not parse. |
| `unknown_capability` | -32602 | `grant` names something that is not a capability. |
| `capability_not_granted` | -32602 | `data.missing` lists what to grant. |
| `io_failed` | -32603 | `Regia.toml` could not be read or written. |
| `method_unavailable` | -32001 | The method's extension is not active. |
| `invalid_params` | -32602 | An extension method's params break its `params_schema`; `data.path` and `data.rule` say where and how. |
| `abi_mismatch` | -32602 | A stamp field differs; rebuild with `regia plugin build`. |
| `not_an_extension` | -32602 | No readable stamp: missing symbol, short read, bad magic. Also a Mach-O library whose code-signature pages no longer match their hashes (a file changed after it was signed): on Apple silicon the system would stop the process that maps it, so the copy is refused before the loader opens it (ADR-3373). |
| `load_failed` | -32603 | The file, a dependency or the entry point failed. |
| `reload_failed` | -32603 | The new `build` failed or panicked, or `on_unload` panicked; the old registrations were restored. |
| `restart_required` | -32004 | `regia-plugin.toml` changed since start, so its capabilities and the set need resolving again. |
| `tool_timeout`, `tool_output_too_large` | -32002 | The child was killed. |

Each carries `data.id`, `data.detail` and `data.hint`.

A panic in an extension's `build` or `finish` marks that extension `failed` with `load_failed`, removes what it registered,
and the others go on. Under `panic = "abort"`, as in a shipping build, a panic ends the process like any game code, and the
editor refuses dynamic extensions.

## Diagnostic codes

Discovery: `extension_overridden` (info): a project extension replaced the engine's extension of the same id
(`<engine>/extensions/` is scanned after the project's roots; ADR-9850).

The resolver: `duplicate_id`, `missing_dependency`, `version_unsatisfied`, `engine_incompatible`, `dependency_cycle` (the
message lists the path), `conflict`, `provides_clash`, `phase_inversion`, `unknown_capability`, `capability_not_granted`,
`reserved_id`, `dependency_disabled`, `not_supported` (a prebuilt-library-only extension on a platform without run-time
code loading), `not_found`.

The host: `manifest_invalid` (also: a crate whose `[package.metadata.regia] id` differs from the manifest `id`,
ADR-7237), `manifest_too_new`, `path_escape`, `registration_conflict`, `content_collision`,
`unknown_field`, `render_pass_invalid`, `manifest_not_linked` (enabled, crate not in `extensions_generated.rs`), `linked_not_enabled`,
`extension_set_mismatch` (the content was cooked with another set), `abi_mismatch`, `not_an_extension`, `load_failed`.

`regia plugin check` adds what needs the crates on disk: `layer_violation` (a game or server entry that depends on an editor
crate) and `package_include`.

`toolchain_mismatch` is not a code: a different compiler or target is an `abi_mismatch` naming `rustc_hash` or
`target_triple`, so one code covers every stamp difference and `detail` names the field (ADR-1885, ADR-2844).

### Dynamic extensions on every platform

A dynamic extension also needs the editor to share Rust's dynamic `std`, or the library would get a second copy. Linux asks
the loader's map; **Windows and macOS read the editor executable** and look for `std-<hash>.dll` / `libstd-<hash>.dylib`
among the libraries it asks for. An editor that is statically linked answers `load_failed` with the steps (rebuild with the
hot-dylib feature and `-C prefer-dynamic`, put the toolchain library directory on the loader path). An executable the
reader cannot parse is not refused (nothing is proven). A library built before ADR-2844 has no `REGIA_EXTENSION_STAMP`
symbol and answers `not_an_extension`: run `regia plugin build` again. Universal Mach-O files, 32-bit images and ELF files
without section headers answer `not_an_extension` with the reason.

### Warm discovery cache

Discovery keeps each parsed manifest in `<project>/.regia/extension-discovery.json` (`format_version` 1, derived data: delete
it any time). The manifest file is read on every start and its hash decides: an unchanged manifest is not parsed again, a
changed one is. A missing, corrupt, other-version or other-engine file is a cold start, a failed write is ignored, and
invalid manifests are never cached. `regia_framework::extension::discover::discover_cached` returns counters (`parsed`,
`reused`, `state`, `written`).

### Typed loads through the asset server

`AssetServer::load_content::<Box<dyn Reflect>>("ext://chatter/hello.dlg")` (and `load_content_any`, `has_content_loader`)
decode cooked content with the loader of whichever active extension claims the file extension. The extension plugin installs
the `regia_asset::ContentLoaderHook` and keeps it equal to the active loaders; a path no loader claims, one that is not
cooked, or a loader that refuses the bytes is an `AssetLoadError::Invalid` with the reason.

## Command line

`regia plugin list|info|check|enable|disable|sync|build|new|tool`, each with `--project` and `--json`:

- `regia plugin check [--platform P] [--profile dev|shipping]` exits 1 on any error.
- `regia plugin enable <id> [--grant CAP]... [--grant-all] [--platform P]`.
- `regia plugin sync` writes the committed `src/extensions_generated.rs` (`regia::register_extensions! { ... }`): the
  crates a game links, the `[extensions]` table and the manifests of the rest of the set. It names crates missing from
  `Cargo.toml`.
- `regia plugin build <id> [--release] [--target-dir DIR]` writes `bin/<target-triple>/` inside the extension.
- `regia plugin new <id> [--content]` scaffolds under `extensions/`.
- `regia plugin tool <id> <name> -- ARGS`.

## Editor

The **Extensions** tab lists what `plugin.list` lists, with a banner stating that in-process extensions are trusted native
code, and shows version, publisher, licence, capabilities, dependencies, diagnostics, and enable, disable and reload
controls. It calls the same functions as the methods above, so nothing there is out of an agent's reach. Panels an
extension registers dock like built-ins and close when the extension goes away. An inspector customization shows a small
badge naming its extension.
