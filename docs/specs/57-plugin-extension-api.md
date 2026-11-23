# Spec 57: Plugin and extension API

Status: proposed (ADR-1579). Target rung: v0.4.x (manifest, resolution, source extensions, agent surface), v1.0 (prebuilt
dynamic extensions, API stability guarantees, extension marketplace-ready metadata).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built: lanes b23 (manifest, resolver) and plugin-api (the host, ADR-1885). ADR-1885 changes these points below: the stamp is read from the library file before the dynamic loader runs, level features are a component, `toolchain_mismatch` is reserved, `regia plugin` lives in `regia_cli`, `plugin.tool_run` joins the six methods, a reload removes systems instead of disabling them, `add_systems` joins the context, `add_render_pass` takes a `FramePass` at the frame graph's slots, a packaged game links its set, and a tool-only capability gates only that tool.
Conformance audit (2026-11-10): ADR-5783 records where the text below follows the built code and which pieces are not built yet.
Built baseline: `regia_editor::hot_reload::load` (dylib load, `REGIA_ABI_HASH`, `regia_game_entry`), `regia_app::Plugin`, agent `settings.get/set` (implemented, spec records gaps).

Source area: runtime-core/runtime-core-modules-plugins, runtime-gameplay/runtime-gameplay-modular-features.

Crates: `regia_app` (layer 0a, `Plugin` baseline), `regia_framework` (layer 3, new `extension` module: manifest,
resolver, registry, `ExtensionContext`), `regia` (facade, layer 4, re-export and macros), `regia_dylib` (layer 4),
`regia_agent` (layer 3; already depends on `regia_framework`, so `plugin.*` handlers read the registry directly),
`regia_editor` and `regia_cli` (layer 6, desktop only). Editor-side loading, importers and panels live in
`regia_editor::extension`.

## 1. Purpose and scope

A game developer needs to add capability to a game or to the editor without forking the engine: a vendor's
dialogue system, a studio's in-house importer, a set of art assets, a gameplay feature that levels can
switch on. This spec defines what an **extension** is, how it is described, discovered, ordered, enabled,
loaded and inspected, which seams it may plug into, and what trust it carries.

It is not:

- a scripting layer (Regia has none; extensions are Rust, `AGENTS.md` § 1.2);
- a sandbox. In-process extension code runs with the full privileges of the host process (§ 2.7). The only
  isolation offered is process isolation for out-of-process tools;
- a package registry or store. The manifest carries the metadata a registry would need; hosting one is
  out of scope (§ 11);
- a replacement for `Plugin` in `regia_app`, which stays the unit of app configuration. An extension is a
  packaged, versioned, discoverable bundle that *contains* one or more `Plugin`s plus optional data.

Baseline (exists today) versus new:

| Piece | State |
|---|---|
| `regia_app::Plugin` (`build`, `finish`, `cleanup`, `name`; duplicate names skipped with a warning), tuple `Plugins` | baseline |
| Hot reload of game code as a salted dylib, never unloaded (`specs/20`): `regia_editor::hot_reload::load` checks the `REGIA_ABI_HASH` static (FNV-1a 64 of the engine build inputs, `regia_editor/build.rs`), fingerprints the engine dylib, calls `regia_game_entry`; unique file name per load | built (implemented, spec records gaps: no per-extension identity, no stamp record) |
| `regia_dylib` force-link of one engine copy, `hot-reload` feature anchor | baseline |
| `settings.get` / `settings.set` agent methods and `regia_framework::settings` | baseline (extension settings reuse them, § 3.3.5) |
| `Regia.toml` project manifest (`specs/21` § 2), `toml` workspace dependency | baseline |
| `regia_agent` method registry, guide pages, coverage table (`specs/23`) | baseline |
| Extension manifest, resolver, registry resource, enable/disable, capability grants | new |
| Extension points beyond `Plugin`: importers, editor panels, agent methods, render pass hook, content mounts, feature packs | new (each says whether a seam exists) |
| Prebuilt dynamic extensions with ABI stamp | new (v1.0) |
| `plugin.*` agent methods, `regia plugin` CLI | new |

## 2. Regia terms and model

### 2.1 Nouns

- **Extension**: a versioned bundle with one manifest, identified by a lowercase **extension id**
  (`[a-z][a-z0-9_]{1,31}`, globally unique within one project's resolved set; a dot-free namespace that
  also prefixes its agent methods, § 5.2). An id equal to an engine agent namespace (`scene`, `plugin`, `settings`, `editor`, ... every namespace in the protocol schema) is `reserved_id`.
- **Source extension** (default): a Rust crate in the project workspace or a path/git/registry
  dependency, compiled with the game by cargo. Its code arrives through ordinary cargo features and a
  generated registration list. This is the only form allowed in shipping builds on every platform.
- **Dynamic extension** (desktop dev only in v0.4.x; opt-in): a prebuilt shared library produced
  from a source extension by `regia plugin build`, loaded by the hot-reload host in `regia_editor` with the
  mechanism of `specs/20`. Used for editor-time experimentation and vendor trials. Loading dynamic code in a
  *shipping* player is deferred to v1.0 (§ 2.8, Decision ADR-1585).
- **Content-only extension**: a manifest plus a directory of assets and data, no code.
- **Extension point**: a named registration surface (§ 3.3) that an extension declares a **capability**
  for (§ 2.4) and then fills in its `build` function through `ExtensionContext`.
- **Feature pack**: a content-only or code extension whose components and systems apply to entities that
  carry a given tag, but only while a level enables the matching **level feature** (§ 3.3.8).
- **Resolved set**: the ordered list of extensions that will load for a (project, platform, target) triple.

### 2.2 Manifest summary and load phases

The manifest is `regia-plugin.toml` at the root of an extension directory (§ 4). Each code entry names a
**phase**, a coarse slot in startup order:

| Phase | Runs | Typical use |
|---|---|---|
| `foundation` | before the asset server starts | content mounts, settings defaults, asset source overrides |
| `early` | before engine default plugins finish `build` | replacing or wrapping an engine plugin |
| `default` (when omitted) | with game plugins | gameplay, importers, agent methods |
| `late` | after every `default` extension `build`, before `finish` | cross-extension glue |

Within a phase the order is the dependency order (§ 2.5); ties break by extension id ascending, so a given
resolved set always yields the same sequence (bit-exact, § 7).

### 2.3 Audience and scope filters

Each code entry carries an `audience` list drawn from `game`, `editor`, `server`, `tool` (headless
commands such as `regia cook`). An entry loads only when the running host matches. Each extension and each
dependency may carry `platforms` (`linux`, `windows`, `android`, `macos`) and `profiles` (`dev`,
`shipping`) lists; an absent list means all. An **explicit empty list** means none (the extension is
effectively off and is reported as `filtered`; as built, ADR-7237: so is an extension whose code entries all name
other audiences and that has no content or tools, in the host where none of its code loads). Editor-audience entries never load in a shipping build, and
the editor crate is never a dependency of a runtime extension (`docs/02` layer rules); a combined extension
therefore splits into a runtime crate and an editor crate listed as two entries.

### 2.4 Capabilities

A **capability** is a string the manifest requests and the project grants. Requests are declarative so
that a project, the CLI and an agent can see in advance what an extension intends to touch. They gate which
`ExtensionContext` registration calls succeed; they do not restrict arbitrary Rust (§ 2.7).

| Capability | Registration it permits |
|---|---|
| `systems` | adding systems, resources, messages, schedules through the app handle |
| `asset_loader` | registering a runtime asset loader for a file extension |
| `asset_importer` | registering an importer for the asset pipeline (editor/tool audience only) |
| `editor_panel` | registering an editor panel or an inspector customization (editor audience only) |
| `agent_method` | registering agent methods under `<id>.` (§ 5.2) |
| `render_pass` | adding a pass through the render pass hook (§ 3.3.6) |
| `settings` | declaring an extension settings resource (§ 3.3.5) |
| `content_mount` | mounting a read-only content root |
| `feature_pack` | declaring feature packs |
| `process` | launching an out-of-process helper tool (§ 2.7) |
| `network` | opening outbound sockets in a helper tool; informational for in-process code |

A request the host does not know is an error (`unknown_capability`), so a manifest written for a newer
engine fails loudly instead of loading partially.

### 2.5 Resolution

Input: the project's `enabled` list (§ 2.6), every discovered manifest, the engine version, the platform,
the target audience and the profile. Output: the resolved set or a list of diagnostics.

1. **Discover**: scan, in order, `<project>/extensions/`, workspace members that contain a manifest,
   user-configured `extension_paths` (project settings), and `<engine>/extensions/`. Recurse into
   subdirectories but never below a directory that already holds a manifest. A directory with a manifest
   whose `id` was already found is a duplicate (rule 4), except across the two sides (ADR-9850 (a)): a manifest
   under `<engine>/extensions/` whose `id` a project root also declares is **overridden**: it leaves the
   discovery with the info diagnostic `extension_overridden` and is neither loaded nor a duplicate. The editor
   passes its engine directory (`REGIA_ENGINE_DIR`, else the checkout it was built from); a game's development
   run passes none.
2. **Select roots**: the extensions named `enabled` for this platform/profile, plus every extension whose
   manifest sets `auto_enable = true` and is not named in `disabled`. Engine-shipped extensions marked
   `auto_enable` are enabled for every project unless the project lists them in `disabled`.
3. **Expand dependencies**: for each root, add each non-optional dependency whose `platforms` and
   `profiles` filters match, recursively. An optional dependency that is not discovered is ignored with an
   info diagnostic; one that is discovered and matches is loaded and ordered before the dependent.
4. **Validate**, producing diagnostics (each has a lowercase code, the extension id and a message):
   - `duplicate_id`: two discovered manifests share an id and differ in path (the first in discovery order
     wins and the second is reported; with two enabled sources of the same id it is an error that stops
     resolution);
   - `missing_dependency`: a required dependency is not discovered;
   - `version_unsatisfied`: a dependency's discovered version does not satisfy the requested range
     (semver range syntax as in the Cargo reference; pre-releases only match ranges that name one);
   - `engine_incompatible`: the running engine version lies outside `engine.min_version` ..
     `engine.max_version` (max inclusive, optional);
   - `dependency_cycle`: the dependency graph has a cycle (the cycle path is listed);
   - `conflict`: an enabled extension lists another enabled extension in `conflicts` (symmetric: either
     side naming the other is enough);
   - `provides_clash`: two enabled extensions claim the same `provides` token (a virtual capability such as
     `dialogue`); exactly one provider may be enabled, unless one lists the other in `replaces`;
   - `unknown_capability`, `capability_not_granted` (§ 2.7), `layer_violation` (a code entry's audience
     contradicts its declared crate layer, e.g. a `game` entry that links an editor crate);
   - `abi_mismatch` (dynamic extensions, § 2.8); a different compiler or target is an `abi_mismatch` naming
     `rustc_hash` or `target_triple`, not a separate code (ADR-1885, ADR-2844).
   Any error in the transitive set of a root makes that root fail; other roots still resolve (a failed
   root and everything that depends only on it are reported as `failed`, the rest continue). Warnings never
   block.
5. **Order**: topological order over dependency edges, grouped by phase (a dependency in a later phase than
   its dependent is `phase_inversion`, an error). Ties by id ascending.
6. **Result**: `ResolvedSet { order: Vec<ResolvedExtension>, diagnostics: Vec<Diagnostic> }`. The resolver
   is a pure function of its inputs (no filesystem access beyond parsing manifests already read), so it is
   unit-testable.

### 2.6 Enable and disable

The project file The project manifest `Regia.toml` (`specs/21` § 2; `regia_cli::manifest::Manifest`, which ignores unknown keys and has no `format_version`) gains an optional `[extensions]` table, modeled as a new `#[serde(default)]` field `extensions`; no version bump is needed. Example:

```toml
[extensions]
enabled  = ["dialogue", "cheats"]            # roots
disabled = ["telemetry_extras"]              # beats auto_enable
extension_paths = ["../shared/extensions"]

[extensions.platform.android]
disabled = ["dev_console"]                   # per-platform delta, applied after the base lists

[extensions.settings.dialogue]               # opaque to the engine; handed to the extension as RON/TOML value
max_line_chars = 240
```

Changing the list affects the next process start; there is no live unload of code (§ 2.8). `plugin.enable`
updates the list and reports `restart_required: true` for code extensions. A **content-only** extension
mounts and unmounts live. A code extension that was never enabled this session and has no dynamic
artifact is also not loadable live. A disabled extension is never partially active: the resolver
removes dependents that required it (reported `failed`, cause `dependency_disabled`).

### 2.7 Trust model

- **In-process code is fully trusted.** A source or dynamic extension is native code linked into the host
  process. It can read any memory, call any syscall, open files and sockets and exit the process. Regia
  does not and cannot sandbox it, and no document, UI string or agent response may describe it as sandboxed
  or "safe". Install an extension only from a source whose code you reviewed or whose publisher you trust.
- Capabilities (§ 2.4) are a **declaration and review aid**: they let a project owner see and pin the
  intended surface (`plugin.info` lists requested versus granted), let CI reject an update that adds a
  capability without sign-off, and gate the registration API. They are not a security boundary.
- **Grants** live in `Regia.toml` under `[extensions.grants.<id>]` (a list of capabilities). A
  request without a grant is `capability_not_granted`, except for `systems` and `settings`, which are granted
  by enabling (`plugin.info` lists them as `implicit` so the review artifact stays complete). The editor shows a one-time review prompt and writes the grant; `regia plugin enable
  --grant-all` is the scripted form. Agents using the `plugin.enable` method need the `build` permission (code-execution class, `specs/23` § 5; the schema class is `write`) plus an explicit `grant` parameter (the review queue of `specs/23` records the change).
- **Out-of-process tools** (importers or generators shipped as separate executables, declared under
  `[[tool]]` in the manifest) get process separation only: the host runs them as child processes, passes
  inputs by path and stdin, bounds time and output size, kills them on timeout, and does not pass its
  environment secrets. The spec promises no OS-level isolation (job objects, namespaces, `seccomp` and similar
  are possible follow-ups, not claims). A tool listing `network` runs only after a grant, which is informational
  since nothing blocks sockets. Tool output is data (files and JSON) validated like any other import; it never
  loads code into the host.
- **Provenance**: the manifest `licence` (SPDX expression, shown as written; not parsed in v0.4.x) and `publisher` fields are shown wherever an
  extension is listed. Extensions with `licence = "LicenseRef-Proprietary"` are accepted; the engine's own
  licence (MPL-2.0, `docs/07-licensing.md`) applies file by file to engine code only, so a developer's
  closed-source extension in its own files may link engine crates and ship compiled only, as for any game code.

### 2.8 Dynamic extensions and the ABI guard

Rust has no stable ABI (the Rust reference states that layout and calling convention of Rust items are
not guaranteed across compiler versions or even builds). Regia therefore treats a dynamic extension as
**same-toolchain, same-engine-build** code, exactly as the hot-reload game dylib of `specs/20`, and refuses
anything it cannot prove matches.

Every dynamic extension exports one C-ABI symbol, `regia_extension_stamp`, returning a fixed-layout
(`#[repr(C)]`) record:

| Field | Meaning |
|---|---|
| `magic` | constant 8 bytes `REGIAEXT` |
| `stamp_version` | u32, 1 |
| `engine_version` | semver string (fixed 32 bytes, NUL padded) |
| `abi_epoch` | u32, bumped by the engine whenever a public type layout in the extension surface changes |
| `rustc_hash` | 32 bytes: hash of `rustc -vV` output at build time |
| `target_triple` | fixed 48 bytes |
| `profile_hash` | 32 bytes: hash of the profile, opt-level, panic strategy and enabled engine features |
| `abi_hash` | u64: the `REGIA_ABI_HASH` value of the engine build the extension was compiled against (built, `regia_editor/build.rs`; the same value `regia_game_entry` hosts already check) |
| `engine_fingerprint` | 32 bytes: the fingerprint of the engine dylib the extension was linked against (`specs/20` § 1) |
| `manifest_hash` | 32 bytes: hash of the manifest bytes embedded in the library |

The stamp is an addition to, not a replacement of, the built game-library checks: a dynamic extension also exports the `REGIA_ABI_HASH` static, and the loader reuses `regia_editor::hot_reload::load` (unique copy, no unload, fingerprint check), adding the stamp comparison and a second entry symbol `regia_extension_entry` (returns the `RegiaExtension` export table, analogous to `regia_game_entry`). The stamp is a `static` of the library, exported as the data symbol `REGIA_EXTENSION_STAMP` (`regia_extension_stamp()` returns its address), so the host reads it from the **file** before the dynamic loader runs anything: bounds-checked readers for ELF64 `.dynsym`, the PE32+ export table and the Mach-O 64 symbol table find the symbol and its bytes through the file's own address mapping (`regia_framework::extension::objfile`, ADR-2844). Searching the file for the magic is not a trust boundary (a record planted elsewhere proves nothing). The host compares each field to its own values, and compares the exported record again after opening the copy (ADR-1885). The same readers list the libraries the host executable needs, which is how Windows and macOS hosts tell a dynamically linked `std` (`std-*.dll`, `libstd-*.dylib`) from a static one.
Any difference refuses the load with `abi_mismatch` (naming the first differing field) and the extension
stays unloaded; the process continues. A missing symbol, a short read or a bad `magic` is
`not_an_extension`. The host copies the library to a unique file name per load and never unloads it
(`specs/20`: thread-local destructors make unloading unsafe), so `plugin.reload` loads a new copy and
swaps registrations (§ 3.4) without unmapping the old one. Dynamic extensions must link `regia_dylib` so
there is one engine copy (statics, type registry, allocator). Panics must not unwind across the symbol
boundary; the entry point is `extern "C"` and catches unwinds (the `regia::extension!` macro emits `#[unsafe(no_mangle)]` under a local `#[allow(unsafe_code)]`, the same precedent as `regia::android_main!` in `docs/04` § 5, so extension crates need no opt-out), turning them into `load_failed`.

A dynamic extension may use the facade's Rust types because the stamp guarantees the same build; this is
an engineering guarantee for developer and studio use, not a distribution format for third parties across
engine versions. A cross-version, vendor-neutral plugin ABI (a C interface or a versioned boundary built on
a crate such as `abi_stable`) is a follow-up (§ 11) and would be added behind a separate manifest field.

## 3. API shape in Rust

### 3.1 Placement and layering

- `regia_framework::extension` (layer 3): `ExtensionManifest`, `Resolver`, `ResolvedSet`,
  `ExtensionRegistry` (resource), `ExtensionContext`, `RegiaExtension` (trait), capability enum,
  `ExtensionPlugin` (installs the registry and applies the resolved set). Depends only on layers below
  and on `semver` and `toml` (both already permissive; **proposed** entries in `docs/03` if absent).
- `regia_dylib`: unchanged role. The stamp record, its constants and `regia::extension!` (which emits
  `regia_extension_stamp`) live in `regia_framework::extension` and are re-exported by `regia` (ADR-5783).
- `regia_editor::extension` (layer 6): library loader (reuses `hot_reload::load`; `libloading` and its
  `unsafe` are already allowed for this crate in `docs/04` § 5, so this spec adds no new `unsafe` row),
  `ToolExtensionContext` (§ 3.2), editor panel registry, inspector customization hooks, the importer registry.
  A future shipping-player loader (v1.0) would be a new layer-4 crate,
  `regia_extload`, which needs its own `docs/04` § 5 row and ADR; it is not part of this spec.
- `regia_agent` (layer 3, already depends on `regia_framework`): the `plugin.*` handlers read `ExtensionRegistry`;
  extension-registered methods (plain `AgentMethodDesc` data defined in `regia_framework::extension`, below) are
  mounted into the method table at startup; in the editor, enable/reload call the editor module through the
  existing editor-handler seam. `regia_framework` never names `regia_agent` types.
- `regia_cli`: `regia plugin {list,info,check,enable,disable,sync,build,new,tool}`. The bodies live here, not in `regia_editor`, which `regia_cli` does not depend on (ADR-1885).
- `regia` (facade): re-exports `extension::prelude`, and the `register_extensions!` macro. Runtime crates
  never depend on `regia_editor`; editor-audience entries are compiled in a separate crate so the shipping
  graph stays clean.

### 3.2 Trait and context

```rust
/// Implemented once per extension crate; the macro `regia::extension!` exports it.
pub trait RegiaExtension: Send + Sync + 'static {
    /// Called once per phase entry after resolution; register everything through `cx`.
    fn build(&self, cx: &mut ExtensionContext<'_>) -> Result<(), ExtensionError>;
    /// Called after every extension built; the `Plugin::finish` analogue.
    fn finish(&self, _cx: &mut ExtensionContext<'_>) {}
    /// Optional: a snapshot of private state before a dynamic reload (reflected value) and its restore.
    fn on_unload(&self, _world: &mut World) -> Option<DynamicStateBlob> { None }
    fn on_load(&self, _world: &mut World, _prior: Option<DynamicStateBlob>) {}
}

pub struct ExtensionContext<'a> { /* app handle, manifest, grants, diagnostics sink */ }
impl ExtensionContext<'_> {
    pub fn id(&self) -> &str;
    pub fn settings<T: DeserializeOwned>(&self) -> Result<T, ExtensionError>;      // from [extensions.settings.<id>]
    pub fn add_systems<M>(&mut self, s: impl ScheduleLabel, sys: impl IntoScheduleConfigs<ScheduleSystem, M> + Send + Sync + 'static) -> Result<(), ExtensionError>; // needs `systems`
    pub fn add_plugin(&mut self, p: impl Plugin) -> Result<(), ExtensionError>;     // needs `systems`; `restart_required` during a reload
    pub fn register_asset_loader(&mut self, l: Box<dyn AssetLoaderDesc>) -> Result<(), ExtensionError>;
    pub fn register_agent_method(&mut self, m: AgentMethodDesc) -> Result<(), ExtensionError>;
    pub fn add_render_pass(&mut self, p: Box<dyn FramePass>) -> Result<(), ExtensionError>;
    pub fn declare_settings<T: Resource + Reflect + Default>(&mut self) -> Result<(), ExtensionError>;
    pub fn mount_content(&mut self, root: &Path) -> Result<ContentMount, ExtensionError>;
    pub fn declare_feature_pack(&mut self, p: FeaturePackDesc) -> Result<(), ExtensionError>;
}
/// Layer 6 (`regia_editor::extension`); editor code builds this from the context above with
/// `ToolExtensionContext::new(cx)?` (`not_supported` outside the editor host). It derefs to the above.
pub struct ToolExtensionContext<'c, 'a> { /* ExtensionContext + importer and panel registries */ }
impl ToolExtensionContext<'_, '_> {
    pub fn register_importer(&mut self, i: Box<dyn Importer>) -> Result<(), ExtensionError>;       // `asset_importer`
    pub fn register_editor_panel(&mut self, p: PanelDesc) -> Result<(), ExtensionError>;            // `editor_panel`
    pub fn register_inspector<T: Reflect>(&mut self, c: InspectorCustomization) -> Result<(), ExtensionError>;
}
```

Types that do not exist yet and are defined by this spec: `AssetLoaderDesc` (`regia_framework::extension`: `file_extensions() -> &[&str]`, `type_name() -> &str`, `load(&self, bytes: &[u8], path: &str) -> Result<Box<dyn Reflect>, String>`; the asset server consults the registry after its built-in loaders); `Importer` (`regia_asset_pipeline`, layer 5: `file_extensions()`, `import(&self, input: &Path, out: &mut ImportOutput) -> Result<(), ImportError>`, mirroring the built importers' inputs and outputs); `PanelDesc`, `InspectorCustomization` (`regia_editor`); `AgentMethodDesc { name, summary, access, params_schema: serde_json::Value, result_schema, handler: Arc<dyn Fn(&mut World, serde_json::Value) -> Result<serde_json::Value, AgentFault> + Send + Sync> }` with `AgentFault { code: String, data: serde_json::Value }` (plain data, defined in `regia_framework` so the layer rule holds). A render pass is a `regia_render::FramePass` (ADR-1552). `access` is `read`, `play` or `edit_scene`, the built permission names of `specs/23` § 5 (`regia_agent::schema::Permission`).

Every registration call checks the capability, records `(extension id, kind, name)` in the registry so
`plugin.info` and unloading can enumerate it, and rejects a second registration of the same
`(kind, name)` with `registration_conflict` (the second extension in order loses; the first stays).
`ExtensionError` is a `thiserror` enum for the registration and build-time codes of § 5.3 (`code()` gives the protocol
code); resolver diagnostics carry their codes as strings, so the enum is not one-to-one with every code (ADR-5783).

A source extension declares itself with `regia::extension!(MyExtension)`, which emits a `pub fn
regia_extension() -> Box<dyn RegiaExtension>` and the manifest text for static linking and, under the `dynamic-extension`
feature, the exported stamp and entry symbols for § 2.8. A project's generated `extensions_generated.rs`
(`regia plugin sync`, committed) holds the explicit list `register_extensions![dialogue, cheats]` so a
source build links exactly the enabled crates; the registry cross-checks that list against the manifests and
reports `manifest_not_linked` (manifest enabled, crate absent) and `linked_not_enabled`. The generated list also holds the `[extensions]` table and the manifests of the extensions it does not link, because a packaged game has no project on disk: `ExtensionPlugin::for_game(project, linked_extension_set())` reads the project when a `Regia.toml` is there and otherwise uses the compiled-in set (ADR-1885).

### 3.3 Extension points

1. **Systems and plugins.** `add_plugin` wraps the baseline `Plugin`; the extension's plugins are named
   `ext/<id>/<name>`. Systems an extension adds, with `add_systems` or inside a plugin, run in the ordinary
   schedules under the system set label `ExtensionSet(id)`, so an agent can list them and a project can order
   sets. `build` runs under a `TypeOwner`, so this needs nothing from the extension (ADR-1885).
2. **Asset loaders and importers.** A loader maps a file extension to an asset type at run time (baseline
   `AssetLoader`). An importer (editor/tool audience) produces cooked-intermediate artifacts for the
   pipeline (`specs/12`). A file extension claimed by two registrants is `registration_conflict`.
   Importer output formats the extension invents carry their own `format_version` (§ 4.3).
3. **Editor panels and inspector customizations.** `PanelDesc` supplies an id (`<ext>.<panel>`), title
   key (a localization key), default dock region and a draw callback. An inspector customization replaces
   the default widget for one reflected type or for fields carrying a named attribute. Both appear in the
   layout save file; a missing extension leaves the panel slot out silently and keeps its saved position.
   **Actions and asset tools** (ADR-9850): `ActionDesc { id, title, menu, toolbar, run }` is an entry of the
   editor's Extensions menu (grouped by `menu`) and, with `toolbar`, a toolbar button; `AssetToolDesc { id,
   title, suffixes, run }` acts on one project asset whose name ends in a listed suffix. Both use the
   `<ext>.<name>` ids and the `editor_panel` capability, are registrations of kind `editor_action` and
   `asset_tool`, and report a callback's error or panic instead of propagating it.
4. **Agent methods.** Names are `<extension id>.<verb>[.<noun>]`, lowercase, dots and underscores only.
   The `plugin.` prefix and every engine namespace are reserved; an extension cannot register under
   another extension's id. Each method supplies a JSON Schema for params and result, an access class
   (`read`, `play`, `edit_scene`), and doc text; the coverage tool (§ 5.4) requires all of them.
5. **Settings.** There is no console-variable system in the engine, so an extension declares one reflected
   settings resource (`declare_settings::<T>()`), initialized from `[extensions.settings.<id>]` of `Regia.toml`
   and registered under the name `<id>` with the existing `settings.get` / `settings.set` methods and
   `regia_framework::settings` persistence. Unknown keys in the table are warnings (`unknown_field`). Built: the
   resource, its initialization, the warning and the `settings.get` / `settings.set` routing (`<id>`, `<id>.<field>`;
   a `GameSettings` field of the same name wins); persistence to the user settings file (`GameSettings::extensions`,
   the RON text per extension id, applied over the table at the next start; ADR-7237).
6. **Render pass hook.** The frame graph has named slots (`after_opaque`, `after_transparent`,
   `before_post`, `after_post`, `overlay`; ADR-1552). A `FramePass` supplies a name, the slot, declared
   reads and writes and a record function. A malformed name is `render_pass_invalid` at registration; a
   name the graph refuses is dropped with one warning, also `render_pass_invalid`, and the frame continues.
   A pass that fails while recording is skipped for that frame. Shader source must be WGSL shipped inside
   the extension and compiled through the normal shader path.
7. **Content mounts.** `mount_content(root)` adds a read-only asset root under `ext://<id>/`. Lookup is
   path-exact; a path present in the project's own `assets/` shadows the extension's (so a game overrides
   extension content), and two extensions never shadow each other: a duplicate path across extension
   mounts is `content_collision`, reported at mount and the later mount loses that path. Mounts appear in
   the asset database (`specs/12`) with `origin = extension:<id>`; cooking includes only the referenced
   closure, as for project assets. Extension content may not be written to by the editor; "edit" on it
   offers a copy into the project.
8. **Feature packs (runtime features that switch on by level tag).** A pack is declared as
   `FeaturePackDesc { id, tag: String, components: Vec<ComponentSpec>, requires_level_feature: String }`; the pack's
   systems are gated with the `pack_active(id)` run condition rather than a `systems` field (ADR-5783).
   A level lists its features in a reflected `LevelFeatures { features: ["rain_weather"] }` component on any entity of the level (ADR-1885: no level format bump, and it loads and unloads with the level). Pack life cycle:

   | State | Meaning | Transition |
   |---|---|---|
   | `declared` | registered, level feature off | level load with the feature -> `active` |
   | `active` | components added to every entity carrying `tag` (at spawn and when the tag is added later); the pack's system set runs | feature turned off or level unload -> `withdrawing` |
   | `withdrawing` | one frame: components removed from entities, `PackDeactivated` message sent | -> `declared` |

   Activation is reference counted per `(pack id, feature)` so two overlapping levels (a persistent and a
   streamed one) that both enable it keep it on until both drop it. A component the pack adds that the
   entity already has is kept untouched and not removed on withdrawal. Systems of an inactive pack are
   disabled through a run condition, not removed from the schedule. Packs are the only mechanism by which
   an extension modifies entities it did not spawn, and every such modification is listed by
   `plugin.info` under `packs`.

### 3.4 Reload of a dynamic extension

`reload(id)` does: load a new copy (§ 2.8) and run the stamp check, which change nothing; call `on_unload`
and keep the returned state; remove the registrations recorded for the id (agent methods, panels, loaders,
packs, settings, render passes, mounts); `build`; then `on_load` with the saved state. The old copy's systems
are removed, and the new copy's added, at the next frame boundary (ADR-1885); a pack the new copy declares
unchanged keeps its state and the entities it touched, and any other withdraws. A failing `build` restores the
previous registrations and reports `reload_failed` (the old code remained loaded, so this is always possible).
A new copy adds systems with `add_systems`; `add_plugin` needs the app and answers `restart_required`. Source extensions reload only as part of the game hot reload
of `specs/20`, which covers all of the game crate's extensions at once.

### 3.5 Events

`ExtensionEvent` messages: `Resolved { count, errors }`, `Built { id, phase }`, `Failed { id, code }`,
`Enabled { id }`, `Disabled { id }`, `Reloaded { id, generation }`, `PackActivated { pack }`,
`PackDeactivated { pack }`. Reflected and serde-derived so agents can wait on them (`wait_until`).

## 4. Files and formats

### 4.1 `regia-plugin.toml`, `format_version = 1`

```toml
format_version = 1
id        = "dialogue"
name      = "Dialogue Trees"                 # display; a localization key is allowed
version   = "1.4.2"                          # semver
publisher = "Acme Tools"
licence   = "MIT OR Apache-2.0"              # SPDX expression, or "LicenseRef-Proprietary"
description = "Branching conversation assets and a runtime player."
auto_enable = false
provides  = ["dialogue"]                     # optional virtual capabilities
replaces  = []                               # extension ids whose `provides` this one takes over
conflicts = ["old_dialogue"]
platforms = ["linux", "windows", "android"]  # absent = all

[engine]
min_version = "0.4.0"
max_version = "0.4.99"                       # optional, inclusive

[[dependency]]
id       = "ui_extras"
version  = ">=2.1, <3"
optional = false
platforms = ["linux", "windows"]             # filters apply to this edge only

[[code]]
crate    = "dialogue_runtime"                # cargo package name (source extension)
audience = ["game", "server"]
phase    = "default"
library  = "bin/libdialogue_runtime.so"      # optional prebuilt (dynamic); per-platform keys allowed

[[code]]
crate    = "dialogue_editor"
audience = ["editor"]

capabilities = ["systems", "asset_loader", "agent_method", "editor_panel", "content_mount", "feature_pack"]

[[content]]
root = "content"                             # mounted at ext://dialogue/

[[tool]]
name = "dialogue-lint"
command = "bin/dialogue-lint"                # relative to the extension dir
capabilities = []                            # `network` only with a grant
timeout_seconds = 30
```

Rules: unknown top-level keys are warnings (`unknown_field`) so older engines tolerate additions in
metadata, unknown `[[code]]` or `[[dependency]]` keys are errors. `id` must equal the directory name for
discovered content-only extensions and the `[package] metadata.regia.id` for a crate (as built, ADR-7237: discovery
refuses a mismatch with `manifest_invalid`; a crate without the table is not checked). Paths are relative,
forward-slash, and must not escape the extension directory (`path_escape`). A crate-published
extension reads its manifest from the crate root (the crate `package.include` must list it; `regia plugin
check` verifies).

### 4.2 Layout

```
<ext>/regia-plugin.toml
<ext>/Cargo.toml  src/…        source (optional)
<ext>/content/…                assets and data (optional)
<ext>/bin/<target-triple>/…    prebuilt libraries and tools (optional)
<ext>/LICENSE*  NOTICE         shown by plugin.info
```

### 4.3 Versioning of formats

`format_version` starts at 1. A future 2 ships a migrator in `regia_framework::extension::migrate`
(manifest v1 to v2, lossless for known keys) and `regia upgrade-project` (`specs/12` § 4a) rewrites
manifests it finds in the project; the loader of version N reads N-1 with a deprecation diagnostic for one
minor release. A manifest with a `format_version` higher than the host knows is `manifest_too_new`
(never partially read). The ABI stamp has its own `stamp_version`. Content formats an extension introduces
are the extension's responsibility, but must carry `format_version` and a migrator hook
(`register_migrator`, a follow-up) to join `regia upgrade-project`.

### 4.4 Cook

Cooking packs the content of enabled extensions that are referenced (never unreferenced content), writes
the resolved set to `build/paks/<platform>/extensions.ron` (`format_version`, ordered ids, versions, manifest
hashes) and embeds it in the pak at `ext://regia/extensions.ron` so a shipping player can print `plugin.list`
equivalents and refuse a pak/binary pairing that disagrees about extensions (`extension_set_mismatch`). A
reference is an `ext://<id>/<path>` string in a text file under `assets/` or `src/`, followed through the
extension files it reaches (ADR-1885). Dynamic libraries are never placed in
an Android pak (§ 9).

## 5. Agent-protocol surface

### 5.1 Methods

| Method | Access | Params | Result |
|---|---|---|---|
| `plugin.list` | read | `{ state?: "active"\|"failed"\|"filtered"\|"disabled"\|"available" }` | `{ extensions: [{ id, version, state, kind: "source"\|"dynamic"\|"content", phase, diagnostics: [code] }], resolved_hash }` |
| `plugin.info` | read | `{ id }` | manifest fields, `requested` and `granted` capabilities, dependency graph (resolved versions), registrations by kind (methods, panels, loaders, importers, settings, passes, mounts, packs, system sets), diagnostics, library path and stamp (dynamic), `generation` |
| `plugin.check` | read | `{ enabled?: [id], platform?, profile? }` | the resolver run as a dry run: `{ order, diagnostics }` without changing anything |
| `plugin.enable` | write (`build`) | `{ id, grant?: [capability], platform? }` | `{ enabled: true, restart_required, diagnostics }` |
| `plugin.disable` | write (`build`) | `{ id, platform? }` | `{ disabled: true, restart_required, dependents_affected: [id] }` |
| `plugin.reload` | write (`build`) | `{ id }` | `{ generation, removed: counts, added: counts }`; dynamic extensions only |
| `editor.extension_ui` | read | `{}` | `{ panels, actions, asset_tools }` of active extensions, each with its `owner` (ADR-9850) |
| `editor.extension_action` | edit_scene | `{ id }` | `{ id, owner, message }`; as the menu or toolbar does |
| `editor.extension_asset_tool` | edit_scene | `{ id, path }` | `{ id, path, owner, result }` |
| `editor.extension_panel` | read | `{ id }` | `{ id, title, dock, owner, text: [string] }`: the panel drawn once off screen |
| `plugin.tool_run` | write (`build`) | `{ id, name, args?, stdin? }` | `{ status, stdout, stderr }`; a `[[tool]]` helper as a bounded child process (ADR-1885) |

`state` values: `active` (built), `failed` (resolution or build error, reason in `diagnostics`),
`filtered` (disabled by platform/profile/audience scope), `disabled` (project lists it), `available`
(discovered, not enabled). `resolved_hash` lets an agent detect that the set changed.

### 5.2 Namespacing

Methods registered by extensions appear in `agent.hello` capability lists and the protocol schema under
their own id prefix, with `provider: "extension:<id>"`. When the provider is disabled or failed, calls to
its methods return `method_unavailable` with the extension id and state, not `method_not_found`.

### 5.3 Error codes

`not_found` (unknown id), `unknown_capability`, `capability_not_granted`, `duplicate_id`,
`missing_dependency`, `version_unsatisfied`, `engine_incompatible`, `dependency_cycle`, `conflict`,
`provides_clash`, `phase_inversion`, `layer_violation`, `abi_mismatch`, `toolchain_mismatch`,
`not_an_extension`, `load_failed`, `reload_failed`, `not_supported` (reload of a source extension, or
`render_pass` before the seam exists), `restart_required`, `manifest_invalid`, `manifest_too_new`,
`path_escape`, `registration_conflict`, `content_collision`, `render_pass_invalid`, `method_unavailable`,
`extension_set_mismatch`, `tool_timeout`, `tool_output_too_large`, `permission_denied` (agent profile lacks `build`; the built protocol answers it as `F0012`). `toolchain_mismatch` is reserved: a different compiler or target is `abi_mismatch` naming `rustc_hash` or `target_triple`, as § 2.8 and § 8 test 11 say (ADR-1885). All lowercase ascii, digits and
underscores per `specs/23`. Each error carries `{ id, field?, detail }` data.

### 5.4 Guide page and coverage

Guide page: `book/agents/plugins.md` (task flow: list, info, check before enabling, enable with grants,
restart, verify with `plugin.list` and `wait_until` on `ExtensionEvent::Built`, reload loop, reading
diagnostics). Every method an extension registers must have a schema, a guide section in the extension's
own `book/` page, and a COVERAGE row, enforced by `cargo xtask agent-coverage` extended to scan extension
manifests of the workspace.

COVERAGE.md rows, one per method, in the built format (`| method | schema name | [page](agents/page.md) |`): `| plugin.list | plugin_list | [plugins](agents/plugins.md) |`, and likewise `plugin.info`, `plugin.check`, `plugin.enable`, `plugin.disable`, `plugin.reload`. The method table in the contract (`regia_agent::contract`) gets the seven entries (with `plugin.tool_run`, ADR-1885) with the error codes of § 5.3.

## 6. Editor surface

An **Extensions** panel lists discovered extensions with state, version, publisher, licence, requested
versus granted capabilities (with a clear banner stating in-process extensions are trusted native code),
dependency tree, diagnostics and enable/disable toggles with a restart prompt for code. Extension panels
dock like built-ins; an inspector customization shows a small badge naming its extension. No editor
feature exists that the `plugin.*` methods cannot do.

## 7. Determinism notes

- Resolution is deterministic and order-stable: same manifests, same inputs, same `resolved_hash`
  (a hash of ordered ids, versions and manifest hashes). Bit-exact, asserted by tests.
- Extension system sets run in the schedule order the dependency order produced; two extensions without a
  dependency edge between them have no guaranteed relative order unless the project sets one with
  `[extensions.order.<id>]` (`before` and `after` lists; ADR-1885); the default tie-break (id ascending) is deterministic but should not be relied
  on for correctness.
- Feature-pack activation happens at fixed points (level load completion and the frame boundary), never
  mid-schedule, so fixed-step replays see the same entity component sets.
- A dynamic reload changes behavior at a frame boundary; replays that cross a reload are not
  reproducible and `plugin.reload` marks the session `nondeterministic_reload` in the log contract.
  Built (ADR-9850 (b)): every successful `reload_in_world` adds `{ code: "nondeterministic_reload", source:
  <id>, detail: "generation N" }` to `SessionDeterminism`; demos recorded in the session carry
  `!nondeterministic_reload:<id>` in `build_hash` (`DemoHeader::nondeterministic`, and `mismatches` reports
  `nondeterministic`); `sim.state_hash`, `sim.state_diff` and `demo.inspect` answer `nondeterministic`.
- Timing numbers in diagnostics are informational; no behavior depends on them.

## 8. Tests and acceptance

Fixtures are written inline in `crates/regia_framework/tests/extension_manifest.rs` and `extension_runtime.rs`
(source extensions with plugins, settings and agent methods; content-only extensions; pure-text manifests for the
resolver), and the dynamic extension is the `samples/ext_fixture` crate built by the test harness (ADR-5783).

1. **Manifest parse**: valid, minimal (`format_version`, id, version; without `format_version` it is `manifest_invalid`, ADR-5783), unknown top-level key (warning), unknown
   `[[code]]` key (error), bad id, escaping path, `manifest_too_new`. No GPU.
2. **Resolver ordering**: a diamond graph orders dependencies first, ties by id; the same inputs give the
   same `resolved_hash` over 100 shuffled discovery orders.
3. **Resolver errors**: one test each for `duplicate_id`, `missing_dependency`, `version_unsatisfied`
   (range edges, pre-release rule), `engine_incompatible` (min and max inclusive), `dependency_cycle`
   (cycle path listed), `conflict` (either direction), `provides_clash` with and without `replaces`,
   `phase_inversion`, `unknown_capability`, `capability_not_granted`.
4. **Partial failure**: a failed root does not stop an unrelated root; dependents of a disabled extension
   become `failed` with `dependency_disabled`.
5. **Optional dependency**: absent is info; present is ordered first; platform filter removes the edge.
6. **Per-platform and profile filter**: the same project resolves different sets for `android` versus
   `linux`; an empty `platforms` list yields `filtered`.
7. **Registration**: the fixture registers each kind; `plugin.info` lists them; a second extension
   registering the same agent method name or file extension gets `registration_conflict` and the first
   stays functional.
8. **Content mounts**: project path shadows extension path; extension-versus-extension collision reports
   `content_collision`; the asset database shows `origin = extension:<id>`; cook includes only referenced
   closure.
9. **Feature packs**: a pack applies to tagged entities only while the level feature is on; reference
   counting across two levels; withdrawal restores entities; a pre-existing component is untouched. Run
   with fixed steps and counters, no wall clock.
10. **Agent**: `plugin.list/info/check/enable/disable` round trip against the fixture project; reads work
    under the `read` profile, `enable` without `build` returns `permission_denied`; extension methods return
    `method_unavailable` when the provider failed; `cargo xtask agent-coverage` passes with the new rows.
11. **ABI stamp refusal** (desktop, no GPU): the harness builds `fixture_dyn`, then (a) flips one byte of
    each stamp field in turn on a copy and asserts `abi_mismatch` naming that field and that no entry
    symbol was called (a counter static in the host proves it); (b) truncates the library: `not_an_extension`;
    (c) loads a valid copy: success. A library with a corrupt `magic` is rejected before any entry is called (the
    file check, or for a damaged static the second comparison after the open; ADR-1885).
12. **Reload**: `plugin.reload` twice increments `generation`, removes old registrations, preserves
    reflected state across `on_unload`/`on_load`; a failing `build` after removal restores the previous
    registrations (`reload_failed`). The old library stays mapped (the unique-file-name rule).
13. **Source extension reload** returns `not_supported`; `extensions_generated.rs` mismatches report
    `manifest_not_linked` and `linked_not_enabled`.
14. **Out-of-process tool**: the fixture tool exceeds its timeout and is killed; output over the size
    bound is rejected; neither affects host state.
15. **Layering and builds**: `cargo xtask check-layers` passes (no runtime crate depends on
    `regia_editor`); `cargo ndk -t arm64-v8a check -p regia` passes with the fixture compiled in;
    a build with a `dynamic` entry for Android fails at `regia plugin check` with `not_supported`.
16. **Public-API snapshot**: the extension prelude is in `docs/api-snapshots/`, and the snapshot check fails on an unreviewed removal or change.

Golden images: none required; a fixture render-pass extension screenshot test is a TODO for the lead once
the render hook exists.

## 9. Platform notes

- **Linux and Windows**: source extensions everywhere; dynamic extensions in dev builds only (the editor's hot-reload host loads them). Shipping players load no dynamic extensions in v0.4.x; the setting `allow_dynamic_in_shipping` does not exist until the v1.0 loader crate (§ 3.1) does. The `prefer-dynamic`
  and library-path requirements of `specs/20` apply. Library names: `.so`, `.dll`; the host copies to a
  unique name before loading.
- **Android aarch64**: no dynamic extensions in store builds. Android apps can load libraries bundled in
  the package, but code downloaded or sideloaded at run time conflicts with store policy and the platform's
  restrictions on writable executable storage, and Regia does not rely on it. All extensions are source
  extensions linked into `libmain.so` through `extensions_generated.rs`; `regia plugin check --platform
  android` reports `not_supported` for any `[[code]]` entry whose only form is a prebuilt `library`.
  Content-only extensions are packed in the pak. Helper `[[tool]]` entries are editor-time only and are
  ignored on device. The dev build over `adb` may mount an extension content directory from the host.
- Any platform without run-time code loading follows the Android rule through `platform.dynamic_code = false`, consulted by the resolver.
- **macOS** (testing-only): source extensions and dev dynamic extensions work as on Linux (`.dylib`); no
  gating, no notarization work.
- Dependencies (Decision ADR-1585): `semver` (MIT OR Apache-2.0; already in `Cargo.lock` through cargo-adjacent crates,
  but not a workspace dependency yet) for version ranges, in `regia_framework`; `toml` is already a workspace
  dependency (MIT OR Apache-2.0); `libloading` 0.9 already used by `regia_editor`. Each new workspace entry needs
  a `docs/03` row and a passing `cargo deny check`. `spdx` is not added: the licence field is displayed, and
  validated only as a non-empty string (an SPDX parser is a follow-up). `abi_stable` is not added (cross-version
  ABI is out of scope, § 11).

## 10. Performance budget

Min-spec machine class (`docs/06`): four cores, NVMe or eMMC.

| Operation | Budget | Span |
|---|---|---|
| Discovery + manifest parse, 50 extensions | 20 ms (cold), 5 ms (warm, hashed cache in `.regia/extension-discovery.json`: a warm run parses no manifest, ADR-2844) | `extension_discover` |
| Resolution, 100 extensions, 300 edges | 2 ms | `extension_resolve` |
| `build` of a source extension | 1 ms of engine overhead beyond the extension's own work | `extension_build` (field: id) |
| Dynamic load incl. stamp check, copy and symbol lookup | 15 ms per library (excludes disk copy over 50 MB) | `extension_load_dynamic` |
| Per-frame cost of the registry when idle | under 0.02 ms (no per-frame work except pack activation checks on tag changes) | `extension_frame` |
| Feature-pack activation, 1,000 tagged entities | 1 ms | `extension_pack_activate` |
| `plugin.list` / `plugin.info` | 1 ms | `agent_plugin` |

Memory: registry plus manifests under 1 MB for 100 extensions. A startup-time report (`plugin.list`
includes `load_ms` per extension as an informational field) lets a studio find slow extensions; budgets are
asserted with counters and synthetic fixtures, not wall time.

## 11. Out of scope / follow-ups

- Cross-version binary ABI for third-party prebuilt extensions (C interface or versioned boundary).
- A registry/marketplace, signing and update checks; the manifest carries `publisher`, `licence` and
  `version` so one can be added.
- True sandboxing of in-process code (for example WebAssembly components), to be a separate ADR and a
  separate extension kind; nothing here may be read as promising it.
- Live code unload; schedules cannot remove systems at run time.
- Extension-defined migrators plugged into `regia upgrade-project`.
- Streaming install, download-on-demand and optional-pack content bundles.
- API stability policy details beyond § 11.1.

### 11.1 API stability policy (applies to the extension surface and all public crates)

- Public crates (`regia`, `regia_app`, `regia_framework`, `regia_agent`, `regia_ui`, and the others listed
  in `docs/api-review.md`) follow semver as defined at semver.org: before 1.0 a minor bump may break; from
  1.0 only a major bump may. The extension prelude is part of the public surface from the first
  `0.4.x` release.
- `abi_epoch` (§ 2.8) bumps on any layout-affecting change to a type reachable from the extension API,
  even in a patch release; dynamic extensions rebuild on each epoch.
- Deprecation: an item is marked `#[deprecated(since, note)]` at least one minor release (two from 1.0)
  before removal, the note names the replacement, and the release fragment lists it under Upgrade.
  The snapshot records each `#[deprecated]` item; removing a public item that was not deprecated in the
  previous snapshot is `removed_without_deprecation`, a warning until `freeze = true` is set in
  `docs/api-snapshots/policy.toml`, then an error that `--breaking` cannot accept (ADR-2527).
  `engine.max_version` in a manifest is advice, not enforced beyond the resolver check.
- Public means: re-exported by `regia`, named in `docs/api-review.md`, or a dependency of a generated
  template or extension; plus the `regia_editor::extension` module (a module-scoped snapshot). The list
  and the reason for each internal crate are in ADR-2527 and `docs/api-review.md` ("API policy").
- `docs/api-snapshots/` records the public items of each public crate (`cargo xtask api-snapshot`, in
  the preflight and CI; ADR-1942). A removed or altered signature fails CI until it has an approved entry
  (`--update --breaking "<why>"`, logged in `docs/api-snapshots/BREAKING.md`) and needs a version bump
  matching the rule above. A new item passes: it breaks nobody, and the release lane refreshes the snapshot.
  `BREAKING.md` keeps the rows of unreleased versions; `cargo xtask api-snapshot --rotate <version>` moves a released
  version's rows to `docs/api-snapshots/breaking/<version>.md`.
- Manifest keys follow the same discipline: adding an optional key is non-breaking; removing or changing
  meaning bumps `format_version`.

## 12. Provenance rows

| Area | Algorithm/behavior | Public source (paper/standard/doc URL) |
|---|---|---|
| Version ranges and pre-release matching | Caret/comparison requirement syntax, pre-release exclusion unless named, precedence ordering | https://semver.org/ ; https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html |
| Dependency resolution | Topological order with deterministic tie-break, cycle detection (depth-first search with path reporting) | Kahn, "Topological sorting of large networks", CACM 1962 ; Cargo reference, features and resolver chapters |
| No stable Rust ABI; same-compiler requirement | Why a stamp is compared before any call, why libraries are not unloaded | https://doc.rust-lang.org/reference/abi.html ; https://doc.rust-lang.org/reference/type-layout.html |
| Dynamic library loading | Open library, resolve one symbol, thread-local destructor caveat on unload | https://docs.rs/libloading ; https://docs.rs/abi_stable (MIT/Apache-2.0) for the deferred cross-version design |
| Manifest conventions and licence metadata | Package id, version, SPDX licence expression, per-target dependency tables | https://doc.rust-lang.org/cargo/reference/manifest.html ; https://spdx.github.io/spdx-spec/v2.3/SPDX-license-expressions/ |
| Reference-counted activation | Activation counted per owner so overlapping requests keep a feature on until the last releases | general resource-management practice (reference counting; no single source) |

### Decisions (ADR-1585)

- Wording: the load phases (`foundation`, `early`, `default`, `late`), audiences (`game`, `editor`, `server`, `tool`),
  and `auto_enable` are Regia's own names and defaults (the earlier `default_enabled` key was renamed to avoid the
  source's key). The rule "never recurse below a found manifest" is a plain discovery-cost choice.
- Home of the runtime half: `regia_framework::extension` (layer 3). `regia_agent` already depends on it.
- `systems` and `settings` are granted by enabling, shown as `implicit` in `plugin.info`.
- Render pass hook: the frame graph's slots (ADR-1552); ADR-1885 replaced the proposed insertion points.
- Dynamic extensions load only in the editor hot-reload host in v0.4.x; no dynamic code in shipping players until a
  v1.0 loader crate (own `docs/04` § 5 row and ADR). Closed-source vendor delivery therefore ships as source
  extensions (or compiled crates built against the exact engine) until then.
- Out-of-process tools: v0.4 promises only separate process, timeout, output-size bound and no inherited secrets;
  OS isolation mechanisms are *not* promised in the spec or the UI.
- No scripting, no sandbox claims anywhere in text (§ 2.7).

### Owner decisions

None.

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
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
