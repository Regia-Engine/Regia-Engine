# Spec 61: Starter content, the sample game and the project templates

Status: proposed (ADR-1579); the sample game (§ 3, the `sample` template and the `samplegame.*` methods) is dropped, not planned (owner 2026-11-12, ADR-8588: DO27 is the proof game); the starter content pack, the template matrix and the learn flow stay. Target rung: v1.0 ("sellable product"; the master plan places starter content and a sample game
as "still to place", and the v1.0 checklist asks for developer-ready templates and an outside-proven shipping path).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_templates::template_files` (compiled-in text templates `blank`, `first_person`, `third_person`, `quick`, ADR-1485), `regia_cli::new_project::{NewProjectRequest { name, template, parent }, create, CreatedProject, NewProjectError}`, `samples/arena`, `samples/third_person`; on `integ/ext2`: `regia_ai::abilities::Abilities` (ADR-1563 a), `regia_ui::subtitles::SubtitleQueue` and loc tools (ADR-1570), `regia_ai::state_tree` (ADR-1565). Built since (audit 2026-11-10, ADR-5643): the `templates` CI job (`cargo xtask templates-check`, ADR-2046); `top_down` and the `first_person` polish with agent smokes (ADR-2738); the networked template `net_game` (ADR-2039), which fills the `multiplayer` row; the `empty` id as an alias of `blank`, and one project-name rule and one project writer shared by `regia new` and the editor (`regia_templates::project_crate_name`, `write_project_dir`: an empty target folder is used, a failed generation leaves nothing; ADR-5643). Not built: `template.ron` descriptors, the `2d`, `ui_only` and `sample` templates, the starter pack and `regia_templates::starter`, `ember_vale`, the `NewProjectRequest`/`NewProjectError` extensions of § 6, `project.*`/`content.starter_*`/`starter.verify`/`learn.*`/`samplegame.*` agent methods and their guide pages, the learn panel.

Source area: editor-core/editor-core-overview and editor-core/editor-core-packaging, used only for template and packaging
vocabulary (project creation as an ordered flow, a starter-content on/off default, a default map that templates set
automatically); no set id was needed for most of this spec, which is written from Regia's own material (`docs/05`,
`docs/versions.md`, `docs/master-plan.md`, `docs/07`, `crates/regia_templates`, `samples/`, `book/`).

## 1. Purpose and scope

A new game developer needs three things on day one: a project that builds and runs, art and sound to fill it with that they may
legally ship, and a complete example showing how the engine's systems fit together. Today Regia has four templates
(`blank`, `first_person`, `third_person`, `quick`), labs and two game samples, no packaged starter content, and no
`templates` CI job. This spec adds:

1. A **starter content pack**: a small, licence-clean, cooked-through-the-normal-pipeline asset library, opt-in at
   `regia new` and addable later from the editor.
2. The **sample game** ("Ember Vale", working title): a complete small game that is also the integration test of the engine's
   systems and the tutorial's subject.
3. A **template matrix** with a contract test: every template builds, boots and passes an agent smoke on every platform.
4. An in-editor **welcome and learn flow**: project browser, tutorials as agent-driven checklists.
5. The agent surface for all of the above.

It is not: an asset store or marketplace; a second game (the owner defers a second game until everything else is complete);
a content-creation tool (generators already exist, `specs/12`); a place for DO27 content (no DO27 asset enters starter
content, ever).

Baseline (exists): `regia_templates` compiles template file lists in with `include_str!` (ADR-1485); `regia new` and the editor's
project browser share `regia_cli::new_project::create`; `samples/arena` and `samples/third_person` exist (the arena budget test
fails on the integration branch); `regia_ai` (blackboard, perception, behavior tree, smart objects), `regia_framework` (flow,
`SaveGame`, damage, camera, character, ragdoll), `regia_ui`, localization (Fluent) and the loopgen sound format exist.
Everything below is **new** unless marked baseline.

## 2. Regia terms and model

| Term | Meaning |
|---|---|
| **Template** | A directory tree of project files plus a `template.ron` descriptor; instantiated by `project.new`. Compiled in (small text) or referenced (see 2.4). |
| **Starter pack** | One versioned content set `starter-content/<pack>/` with a `pack.ron` and an asset tree. v1.0 ships one pack, `core`. |
| **Pack asset** | An ordinary project asset (RON, glTF, KTX2 source PNG, OGG/WAV, font) carrying a sidecar `*.provenance.ron`. |
| **Provenance record** | Per-asset: origin, author, licence id (SPDX), source URL or generator id, seed, date, attribution required. |
| **Licence manifest** | `licenses.ron` at the pack root: the union of provenance records plus a licence-text table; rendered into `ATTRIBUTION.md`. |
| **Sample game** | The `samples/ember_vale` crate set (game, android shim, tests), also offered as the `sample` template. |
| **Learn checklist** | A tutorial expressed as an ordered list of steps, each with an agent-checkable condition. |

### 2.1 Project creation flow (state machine)

The editor's project browser and `regia new` follow the same ordered flow; a later step's options depend on earlier choices.

| State | Input | Next | Constraint |
|---|---|---|---|
| `choose_template` | template id | `defaults` | Template list is filtered by `kind` (3D, 2D, UI, sample). |
| `defaults` | platform targets, quality tier, starter content yes/no, locales | `location` | The template's `constraints` may hide or force options (e.g. `ui_only` hides physics; `2d` forces the 2D renderer feature). |
| `location` | parent dir, display name | `generate` | Name normalized to snake_case; target dir must not exist or be empty. |
| `generate` | none | `done` or `failed` | Writes files atomically into a temp dir, then renames; on failure nothing remains. As built (ADR-5643): the target is claimed (created, or an empty folder taken) and a failed write removes what was written, with the same outcome. |
| `done` | none | none | Sets `[project] startup_level` in `Regia.toml` (the existing key; a template always sets one, so a packaged build never starts black). Templates that build their scene in code set none: `quick` (no `Regia.toml`) and `net_game` (its client spawns camera and light; the level is the server's rotation) (ADR-5643). |

### 2.2 Template descriptor (`template.ron`)

Fields: `format_version: 1`, `id`, `title`, `summary`, `kind`, `platforms` (subset of desktop, android), `features` (cargo
features of the game crate), `starter_default: bool`, `constraints` (map of option to forced value or hidden), `smoke`
(agent smoke script path, 2.5), `min_engine`. The descriptor is the single source for the browser card, `regia new --list`
and `project.templates`.

### 2.3 Template matrix

| Id | State | Contains | Starter pack use |
|---|---|---|---|
| `empty` (today `blank`; `blank` stays as an alias) | baseline | Window, empty level with a camera and one light, `game/` crate, editor and launcher shims, android shim, `Regia.toml`, README | none by default |
| `quick` | baseline | Single crate, one `main.rs` (the `regia_quick` path), no editor shim | none |
| `2d` | new | `regia_2d` camera, sprite/tilemap level, input map, one scripted-by-Rust player controller, pixel-perfect scaling option | UI skin, fonts, sounds |
| `first_person` | baseline (polish) | Character controller, weapon-less interaction, one test room, crosshair UI | materials, props, sounds |
| `third_person` | baseline (polish) | Character, follow camera, animation state graph, one test level | materials, props, sounds |
| `top_down` | new | Click or stick movement, fixed-angle camera, navmesh agent, one enemy with `regia_ai`, health bar UI | all |
| `ui_only` | new | App shell with menu, settings, list/grid widgets, localization in 2 locales, no world | UI skin, fonts |
| `sample` | new | The full sample game (section 3), cloned for study | `core` required |
| `multiplayer` | built as `net_game` (also `net-game`, ADR-2039) | Dedicated server, client and shared game crate on `regia_net`; the first draft listed it as `available: false, reason: "needs_net"` until `regia_net` shipped | none |

Every template's level is built from generator output or small hand-authored RON, never a binary blob; sizes stay
under 1 MB per template excluding the starter pack.

### 2.4 Storage of templates and pack

Text-only templates stay compiled into `regia_templates` (ADR-1485 rule: no file outside the crate read at compile time). The
starter pack and the sample game's media are too large for `include_str!`: they live in the repo at `starter-content/` and
`samples/ember_vale/assets/`, are shipped in the engine distribution next to the binaries (`<engine>/starter-content/`), and
`regia_templates::starter` (new module of the existing crate) locates them through the engine-root lookup that `regia doctor` already uses. If the pack is
absent (source-only checkout without the pack), starter install fails with `starter_pack_missing`, and `regia new` without
starter content still works.

## 3. The sample game: "Ember Vale"

> **Dropped (owner 2026-11-12, ADR-8588).** The sample game is not planned: DO27 is the proof game, and its needs come
> first. This section, the `sample` template row, the `samplegame.*` agent methods and the sample-game tests elsewhere in
> this spec are kept as history and are not built; ADR-1525's "complete" no longer counts them. Building a sample game
> later needs a new owner decision and a new ADR. The starter content pack (§ 4) and the other templates are unaffected.

A small third-person action-adventure slice. It must be small enough to finish in 15 to 20 minutes yet touch every system a
game developer will ship. It doubles as the integration test (section 8) and as the subject of the book's tutorial track.

### 3.1 Content scope

- One playable character (reused from the starter pack's rigged mannequin, not a bespoke hero), 3 enemy types (melee grunt,
  ranged skirmisher, a mini-boss), 1 companion-free story told through 12 to 20 dialogue lines and 4 pickups.
- **Levels** (each a `.level.ron`, loaded through the normal level flow, each 2 to 5 minutes):
  1. `hub_village`: a daylight hub with baked light, an NPC with dialogue, a save shrine (checkpoint), the settings and
     inventory UIs, a door to each of the other levels. Teaches movement, interaction, UI.
  2. `ash_forest`: an outdoor arena with terrain, foliage, a patrol-and-chase encounter, an ability unlock, a locked
     gate puzzle with a smart-object lever. Teaches combat, AI, abilities.
  3. `ember_keep`: an interior with dynamic lights and a boss encounter, then an `end_screen` flow state with credits
     showing licence attributions. Teaches boss patterns, cutscene camera, completion.
- Intro and outro are a short sequencer shot (baseline sequencer) with subtitles.

### 3.2 Systems mapped to crates

| System | Crate (layer per `docs/02`) | Notes |
|---|---|---|
| Game flow (boot, menu, level, pause, end) | `regia_framework::flow` | Baseline state machine; sample adds no new flow code. |
| Character, camera, ragdoll | `regia_framework`, `regia_animation`, `regia_physics` | Third-person template parts. |
| Enemy AI | `regia_ai` (behavior tree, blackboard, perception), `regia_nav` | Grunt: chase and melee; skirmisher: keep distance with a smart-object cover slot; boss: phase tree. |
| Abilities | `regia_ai::abilities::Abilities` (built on `integ/ext2`, ADR-1563 a; spec 53 extends it), **optional** | Feature `abilities` of the sample crate: on uses `Abilities` for dash and attacks; off uses a plain dash and light/heavy attack in game code. The feature flag is a contract: both configurations pass the smoke. |
| Dialogue and subtitles | `regia_ui::subtitles::SubtitleQueue` (built, ADR-1570) for captions; spec 56 (dialogue) and spec 36 for the dialogue runtime, **optional** | Without the dialogue runtime the sample uses a minimal RON dialogue table in game code and `regia_ui` text; feature `dialogue` on switches to the spec 56 runtime. |
| Save and load | `regia_framework::save_game` | Three slots, autosave at shrines, versioned save with a migrator test. |
| Settings menu | `regia_ui`, `regia_input`, `regia_audio`, quality tier | Graphics tier, volume buses, input rebinding, subtitle size, locale, accessibility toggles. |
| Localization | Fluent files, `regia loc` | Three locales: `en`, `de`, `ja` (covers Latin, umlauts and CJK fallback fonts). Pseudo-locale test in CI. |
| Input | `regia_input` | Keyboard and mouse, gamepad, touch (virtual stick and buttons, Android). Rebindable; hot-swap between devices without restart. |
| Audio | `regia_audio` | Mixed buses, one loopgen ambience bed per level, music from pack. |
| UI | `regia_ui` | HUD, menus, subtitles, prompts that follow the active device. |
| Android | `samples/ember_vale/android` shim (`specs/21`) | Same game crate; touch layout; lifecycle (surface drop) exercised by the smoke. |

### 3.3 Quality bar

- Looks like the engine's default look (`book/user-guide/default-look.md`), not a lab: reviewed by the lead by eye against
  a recorded plate set; the sample uses no debug-only draws.
- Zero panics, zero `error`-level log lines in a scripted playthrough; every warning triaged in the run report.
- Every user-visible string is a localization key; no hard-coded text in game code (a CI grep over the crate plus the
  loc-validation command).
- Only starter-pack or sample-owned assets; the licence manifest validates (section 6).
- A 100 percent keyboard-only and gamepad-only playthrough (accessibility row of the v1.0 checklist).
- No `unwrap`/`expect` on recoverable paths (`AGENTS.md` section 3); the crate is held to the workspace lints and
  `missing_docs`.

### 3.4 What it demonstrates per v1.0 claim

| v1.0 claim | Evidence in the sample |
|---|---|
| Developer-ready: install to packaged build without engine source | The `sample` template packages with `regia package` on Linux, Windows, Android; run in CI from a clean machine job. |
| Stable, versioned formats with migrators | The sample ships a save fixture and a level fixture at an older `format_version`; the test opens them through the migrators and `regia upgrade-project`. |
| Agent-native engine | The whole smoke is driven through the protocol (`samplegame.smoke`), no private hooks. |
| Cross-platform incl. Android | Same level set runs on all three; Android check in section 8. |
| Localization, accessibility | Three locales, subtitles, rebinding, keyboard-only playthrough. |
| Performance budgets met (section 10) | Frame-time report on the min-spec class and a mid-tier Android phone. |
| Soak and lifecycle | Level cycle loop (hub, forest, keep, hub) 50 times with no resource growth beyond a stated ceiling; Android pause/resume 50 cycles. |
| A shippable game exists | Basis for the external demo (the fallback for the second-game row): the sample is the starting point, an external team finishes it. |

## 4. Starter content pack `core`

### 4.1 Contents

| Class | Items (counts are targets) | Provenance rule |
|---|---|---|
| Materials | 24 PBR material assets (stone, brick, wood, metal, plaster, grass, soil, fabric, glass, emissive, UI-safe flat colors), using the material graph; 6 terrain layers | Parameterized by procedural textures; no scanned data |
| Textures | About 60 maps (albedo, normal, ORM, height), up to 1K, procedural generators in the repo (seeded noise, tilers); icons as vectors rasterized at cook | Generated by repo tools (generator id and seed in provenance) or CC0 |
| Meshes | Primitives (cube, sphere, capsule, cylinder, plane, stairs, ramp, arch) plus about 40 props (crates, barrels, fences, rocks, trees, lamp, lever, door, pickup gem) and the rigged mannequin with 12 animation clips (locomotion set, attack, hit, death, interact) | Authored in-house in glTF or generated; CC0 where borrowed |
| Sounds | About 50 effects and 6 music loops; ambience beds are `.loopgen.ron` (deterministic, no recorded source needed) | loopgen output with seed, in-house recordings, or CC0 |
| UI skin | One theme (colors, 9-slice atlas, icon set, cursor) for `regia_ui`, in light and dark | In-house |
| Fonts | Two families: a text face and a monospace/display face, with CJK fallback for `ja` | SIL Open Font License only; licence text bundled |
| Prefabs and levels | 12 prefabs (player, three enemies, lever, door, pickup, checkpoint, camera rig, HUD, pause menu, settings menu), 2 demo levels used by templates | Authored in-repo |

### 4.2 Budgets

- Source: at most 60 MB (git-lfs is not assumed; the pack is checked in compressed, binary-diff friendly formats, no
  large PSD-like sources).
- Cooked: at most 25 MB for the full pack, textures in KTX2 (BC7 desktop, ASTC Android), audio as Opus or Vorbis, meshes
  meshlet-ready (baseline pipeline). A per-class budget table lives in `pack.ron` and `starter.verify` enforces it.
- A project that installs only part of the pack pays only for that part (section 4.4).

### 4.3 Licensing rules

Accepted: assets we generate with repo tools or author ourselves (released under CC0-1.0, ADR-10225; the earlier
`LicenseRef-Regia-Starter` is withdrawn), third-party CC0-1.0, and fonts under OFL-1.1. Template and starter code is
MIT-0 (ADR-10225). Not accepted: CC-BY
and CC-BY-SA assets (avoid attribution burden), CC-NC, GPL-family content, any asset of unclear origin, and any
asset derived from another engine's or product's content or samples. A built pack passes `starter.verify` only if every
asset has a provenance record, every licence id is on the allow list, and every OFL font includes its licence text. The
rendered `ATTRIBUTION.md` and the machine-readable `licenses.ron` are copied into every project that installs the pack,
and the packaging step (spec 21) includes them in `THIRD_PARTY_NOTICES.md` (the licence-text section) for shipped games. The
engine's own licence (`docs/07`) does not change: the pack licence grants use inside games, forbids reselling the pack as an
asset library.

### 4.4 Install behavior

- `regia new --starter` / `project.new { starter: true }` installs the whole `core` pack into `assets/starter/` inside the
  project (assets are copied, not linked, so the project is self-contained and the pack can be edited).
- `content.starter_install { classes?, ids? }` adds all or parts later from the editor or by agent; dependency closure is
  automatic (a prefab pulls its mesh, material, textures; a material pulls its textures). Installing is idempotent: a
  file whose content hash matches is skipped, one that was modified locally is never overwritten (reported as
  `skipped_modified`, optional `overwrite: true` for a deliberate refresh creating an undo entry).
- Installed assets keep their stable asset ids from the pack, so a second install, or a pack update later, maps onto the same
  ids (`specs/12` registry).
- Cook uses the normal pipeline; nothing is special-cased for starter assets except the provenance sidecars, which are
  carried into the asset registry as read-only metadata.
- Uninstall is deleting the folder; the registry reports dangling references as ordinary missing-asset diagnostics.

### 4.5 Provenance and licence record

`*.provenance.ron` (format below) sits beside each asset. A generated asset names its generator and parameters so it can be
regenerated bit-identically (section 7).

## 5. Files and formats

All formats start with `format_version: 1`. A v2 adds a migrator in `regia_asset` and `regia upgrade-project` runs it
(`specs/12` section 4a); the pack and templates are upgraded by the same command when a project updates.

```ron
// pack.ron
(
    format_version: 1,
    id: "core",
    version: "1.0.0",
    min_engine: "1.0",
    budget: (source_mb: 60, cooked_mb: 25, per_class: { "textures": 10, "audio": 8, "meshes": 4, "fonts": 2, "ui": 1 }),
    classes: { "materials": [...], "textures": [...] },   // class to asset ids
    locales_covered: ["en", "de", "ja"],
)
```

```ron
// <asset>.provenance.ron
(
    format_version: 1,
    asset: "textures/stone_wall_albedo.png",
    origin: Generated(generator: "regia_texgen:wall_tiler", seed: 90210, params_hash: "b3:..."),
    // or Authored(author: "Regia"), or Imported(source_url: "...", author: "...")
    license: "LicenseRef-Regia-Starter",   // SPDX id or LicenseRef
    attribution_required: false,
    attribution_text: None,
    created: "2026-11-02",
    content_hash: "b3:...",
)
```

`licenses.ron` holds `entries` (list of provenance records), `licenses` (id to full text and URL) and a `summary` count by
licence id. `template.ron` is described in 2.2. The sample game's `samples/ember_vale/Regia.toml` follows the standard
project file; its level, prefab, loc and save formats are the existing ones (no new runtime format is invented by the sample).

Learn checklists are RON: `learn/<id>.checklist.ron` with `title`, `summary`, `estimated_steps`, and a list of steps
`(id, text_key, hint_key, check)` where `check` is an agent expression of the `expect` family (a world query, a file
exists, an asset has a component, a method returns a value). Text lives in Fluent keys so tutorials localize.

## 6. API shape in Rust

Crates (pipeline/tools layers, desktop-only; no runtime crate depends on them): `regia_templates` (data and the new `starter` module; today it has no dependencies and no logic beyond `template_files`) and `regia_cli::new_project` (baseline, the single create path used by `regia new` and the editor).

**Decision (ADR-1585): keep the built create path.** `regia_cli::new_project::create(&NewProjectRequest) -> Result<CreatedProject, NewProjectError>` stays the one entry point; this spec extends it instead of adding a second `create`. New `NewProjectRequest` fields `starter: StarterChoice`, `locales: Vec<String>`, `platforms: Vec<Platform>` (all defaulted by a `Default` impl and a `NewProjectRequest::new(name, template, parent)` constructor; existing struct literals must add `..Default::default()`: listed under Upgrade in the release fragment). `NewProjectError` gains `NameInvalid(String)`, `TemplateUnavailable { id, reason }`, `Starter(StarterError)`; `UnknownTemplate`, `BadName` (kept as the variant behind agent `name_invalid`), `Exists`, `Io` stay. Progress reporting is a `&mut dyn FnMut(Progress)` passed to a new `create_with_progress`; `create` calls it with a no-op.

Handlers: `regia_agent` cannot depend on `regia_cli`, `regia_templates` or the editor (layering), so the methods in § 7 are registered through `AgentExtension` (`regia_agent::extension`) by the editor and by the headless editor host, which also gives them the normal permission checks and audit log. They do not exist in game builds.

```rust
// regia_templates (new items; TemplateKind, Platform, StarterClass are plain enums)
pub struct TemplateInfo { pub id: String, pub title: String, pub kind: TemplateKind, pub platforms: Vec<Platform>,
                          pub available: bool, pub unavailable_reason: Option<String>, pub starter_default: bool }
pub fn list_templates(engine_root: &Path) -> Vec<TemplateInfo>;
pub enum StarterChoice { None, All, Classes(Vec<StarterClass>) }

pub struct StarterPack { pub manifest: PackManifest, pub root: PathBuf }
pub fn open_pack(engine_root: &Path, id: &str) -> Result<StarterPack, StarterError>;
pub fn install(pack: &StarterPack, project: &Path, sel: &StarterSelection, opts: InstallOptions) -> Result<InstallReport, StarterError>;
pub fn verify(pack: &StarterPack) -> VerifyReport;          // licences, hashes, budgets, provenance coverage
pub fn render_attribution(report: &LicenseManifest) -> String;
```

`NewProjectError` and `StarterError` are `thiserror` enums mapping to the agent error codes in section 7. `InstallReport`
lists `installed`, `skipped_identical`, `skipped_modified`, `bytes`. Types derive `serde`, and the manifests also derive
`Reflect` so `regia schema` publishes them. `regia_templates` may use `regia_test_utils` as a dev-dependency for tests only. The learn flow lives in `regia_editor::learn` (editor-only; nothing in a runtime crate depends on it).

## 7. Agent-protocol surface (`book/agents/templates.md`, `book/agents/samplegame.md`)

Access classes follow `specs/23`: `read`, `edit_assets` (writes project files), `play` (runs the game), `build`.

| Method | Class | Params | Result |
|---|---|---|---|
| `project.templates` | read | `{ kind? }` | `{ templates: [TemplateInfo] }` |
| `project.new` | edit_assets | `{ name, template, parent?, starter?: "none"/"all"/classes, locales?, platforms?, open?: bool }` | `{ root, files, starter: InstallReport?, default_level }` |
| `content.starter_install` | edit_assets | `{ pack?: "core", classes?, ids?, overwrite?: false, dry_run?: false }` | `InstallReport` (with `dry_run` nothing is written) |
| `content.starter_list` | read | `{ pack? }` | classes, assets, sizes, licences, installed state in the open project |
| `starter.verify` | read | `{ pack? }` | `VerifyReport { ok, problems: [{asset, code}] , cooked_mb, source_mb }` |
| `learn.list` | read | none | checklists with completion state |
| `learn.status` | read | `{ id }` | steps with `done` booleans and the first failing step |
| `learn.advance` | play | `{ id, step? }` | evaluates the step `check`; `{ done, detail }` (never mutates the project itself) |
| `samplegame.smoke` | play | `{ platform?, level?, script?, timeout_steps?, screenshots?: bool }` | `SmokeReport { passed, steps, levels_reached, end_screen, log_errors, screenshots: [{name, path, ssim?}], frame_stats }` |

Error shape: `F0002` with `data.reason` set to one of the lowercase codes below (neighboring built methods do the same); the permission and "method unavailable in a game build" cases use the protocol's existing codes. Codes: `template_not_found`, `template_unavailable` (e.g. `needs_net`), `name_invalid`,
`target_exists`, `location_not_writable`, `starter_pack_missing`, `starter_pack_corrupt`, `starter_class_unknown`,
`starter_license_rejected`, `starter_budget_exceeded`, `starter_asset_modified` (only with `overwrite: false` and an explicit
`ids` request), `project_not_open`, `learn_unknown`, `smoke_failed`, `smoke_timeout`, `smoke_golden_missing`.

Behavior: `project.new` is atomic (2.1). `samplegame.smoke` works on any project built from the `sample` template and
also on any template through its `smoke` script (section 8); it drives play with `input.inject`, `step`, `wait_until`,
`expect` and `screenshot` only, so the same script runs on desktop and on Android through `adb forward`. Smoke scripts are
`.functest.ron` files of `specs/41` (`FuncTestScript` v1, the same condition grammar as the agent's `expect` and `wait_until`); **Decision (ADR-1585):** this spec does not define a second script format. It needs from spec 41's step list: load a level, inject input for N fixed steps, step N, wait until with a step ceiling, expect, screenshot (by name), save, load a save. A step spec 41 lacks is added to its grammar by the builder of whichever spec lands second (additive, recorded in the format doc). `samplegame.smoke` is a thin wrapper that runs the script and adds the log-error count, frame statistics and screenshot comparison to the report; spec 41 is a prerequisite of the smoke runner (not of the template or pack work).

Guide pages: `book/agents/templates.md` (create a project, install starter content, read provenance), `book/agents/samplegame.md`
(run and read the smoke). COVERAGE.md rows: "`project.templates`, `project.new`: create a project from a template with or
without starter content (`book/agents/templates.md`)"; "`content.starter_install`, `content.starter_list`, `starter.verify`:
add, inspect and verify the licensed starter pack (`book/agents/templates.md`)"; "`learn.*`: run tutorials as checklists
(`book/agents/templates.md`)"; "`samplegame.smoke`: scripted playthrough of the sample game (`book/agents/samplegame.md`)".

## 8. Editor surface (brief)

- **Welcome and project browser**: a start screen with recent projects, a template gallery (card per `TemplateInfo`: preview image,
  title, summary, platform badges), the defaults page (platforms, quality, starter content checkbox defaulting to the template's
  `starter_default`, locales), and location/name. Disabled templates show their reason.
- **Learn panel**: tutorials as checklists. Each step shows its text, a hint, and a live check mark computed by the step's
  `check`. "Next" focuses the relevant panel or selects the relevant entity (via ordinary editor commands). An agent can complete
  the same list headlessly through `learn.*`. Initial tracks: first level, character and camera, enemy with AI, UI and
  localization, save/load, package for Android. Each track is a tutorial chapter in `book/tutorials/` and a checklist file shipped
  with the `sample` template.
- **Content browser**: an "Add starter content" action calling `content.starter_install` with a class picker and a size preview;
  provenance appears in the asset inspector (origin, licence, attribution), plus a project-wide licences view.

## 9. Determinism notes

- Template output is byte-deterministic: the same request writes the same bytes (timestamps in files are fixed to the engine
  release date; any generated id derives from the project name via hash).
- Generated pack assets are regenerated bit-identically from `(generator, seed, params)` on all platforms; textures and loopgen
  audio use the integer/polynomial paths already required by `docs/formats/loopgen-ron.md`. A CI job regenerates the generated
  assets and compares hashes to `provenance.content_hash`.
- The sample game runs on fixed step with a project RNG seeded by the smoke script. Gameplay state (positions, health,
  enemy state, inventory) is bit-exact between two runs of the same script on the same platform; across platforms,
  state is compared at checkpoints with a tolerance of 1e-3 m for positions and exact for integers.
- Screenshots are tolerance-based (section 10 of tests below): temporal jitter off, fixed step, fixed resolution.

## 10. Tests and acceptance

1. `cargo test -p regia_templates`: every `template.ron` parses; `list_templates` ids are unique; `blank` aliases `empty`; a
   template with `available: false` cannot be instantiated (`template_unavailable`).
2. Create atomicity: an injected write failure at file N leaves no directory; a second `project.new` into a non-empty directory fails
   with `target_exists`.
3. Name handling: names with spaces, uppercase, leading digits, unicode and reserved words normalize or fail with `name_invalid`
   per the table in the test (a valid Rust crate name and a valid directory on all three OSes).
4. Template byte-determinism: instantiate each template twice, compare trees.
5. **Template contract (CI `templates` job, GPU-less where possible)**, for every available template on Linux and Windows:
   (a) `regia new`, (b) `cargo build` of the game crate, (c) boot headless with the null renderer through the agent protocol,
   (d) the template's `smoke` script passes (loads the default level, one scripted input, `expect` on a world query, one
   `screenshot` whose hash is only checked for non-black), (e) `regia package` produces a launchable folder. On Android
   the job runs `cargo ndk -t arm64-v8a check` on the generated android shim for every template (build and boot on a device
   is nightly on the lead's device, a second job). The job fails if a template is added without a smoke script.
6. `regia upgrade-project` is run on each template generated at the previous tag, against the current tag, and the result
   passes the contract in item 5 (the master plan's definition of done for upgrades).
7. Starter pack verification: `starter.verify` passes (every asset has a provenance record, license ids are on the allow list,
   OFL font texts present, source <= 60 MB, cooked <= 25 MB, no duplicate asset ids, no asset is referenced that is not in the
   pack). A mutation test removes a record and expects `starter_license_rejected`.
8. Install tests: install all; install by class; install by id with closure; second install is a no-op (`skipped_identical`);
   modified file is skipped; `dry_run` writes nothing; the installed project cooks with zero warnings.
9. Attribution: `render_attribution` output is stable (sorted, no wall-clock), mentions every non-`Regia` license present, and
   the packaged sample game's notices file contains the same entries.
10. **Sample game smoke (CI, `samplegame.smoke`)**: boot on the null renderer, then the scripted run: menu, new game, hub
    interaction and dialogue line, enter `ash_forest`, defeat one grunt (scripted input with a fixed seed), pull the lever,
    open the gate, enter `ember_keep`, defeat the boss, reach `end_screen`; save and reload at a shrine and compare state; switch
    locale to `de` and `ja` and expect key strings resolve with no missing-key log line; zero `error` logs. Counters and fixed
    steps only, no wall-clock waits; `WaitUntil` has a step ceiling.
11. Sample screenshots (GPU job): one screenshot per level and one of the settings menu, compared to blessed images with an
    SSIM threshold of 0.985 (UI shots 0.995) and per-vendor plates where the look gate already uses them. The images are a
    TODO for the lead; the builder never blesses a golden. A missing golden is a `smoke_golden_missing` warning, not a pass.
12. Input coverage: the same smoke runs a second time with a gamepad-only script, and a third with touch events on the Android
    shim (device nightly; on desktop the touch layer is exercised with synthetic touch events).
13. Soak: the level cycle loop 50 times, resource counters (entities, GPU resources, memory) return to within the stated ceilings
    (+2 percent after warmup); Android 50 pause/resume cycles with surface drop and recreate.
14. Learn tests: each checklist's final state is reachable by an agent script that only uses documented methods; step `check`
    expressions are validated against registered methods at test time.
15. Layer and coverage: `cargo xtask check-layers` and `cargo xtask agent-coverage` pass (every method has a schema row,
    contract test, guide topic and COVERAGE row); `cargo deny check` passes for any added dependency.

## 11. Platform notes

- **Linux/Windows**: identical behavior; path handling uses `std::path`; generated names avoid Windows reserved device names.
- **Android aarch64**: the pack cooks to ASTC textures and Opus/Vorbis audio; the sample's touch layout is the template of record
  for the Android UI row; smoke on device via `adb forward`; the `templates` job runs `cargo ndk` check, device runs are nightly.
  The pack size ceiling matters for the APK: the full sample must fit the store-size guideline the packaging spec states.
- **macOS**: testing-only; the template contract runs there when the Mac is available, never gating.
- Dependencies: none new (**Decision, ADR-1585**). `blake3` and `walkdir` are workspace dependencies already (desktop crates); `regia_templates::starter` adds them to its own `Cargo.toml` as `workspace = true`. Licence ids are checked against a fixed allow list of SPDX id strings in code (no `spdx` crate); atomic generation writes into a sibling temp directory named with pid and an atomic counter and renames it (no `tempfile` crate); the procedural texture writers use `image`, already a workspace dependency of the pipeline. The repo's own generators and loopgen are preferred over any external asset source.

## 12. Performance budget

| Item | Budget (min-spec machine class and Android mid-tier) | Span |
|---|---|---|
| Sample game frame time, every level | 60 fps sustained: p50 <= 12 ms, p99 <= 16.6 ms at the tier's default resolution | `samplegame_frame` (the framework's existing frame spans) |
| Main-thread CPU, 20 active AI agents | <= 3 ms desktop, <= 8 ms Android (`docs/06` section 4) | `ai_tick` |
| Level load to first frame | <= 3 s desktop, <= 6 s Android on the reference device | `level_load` |
| Peak memory, sample game | <= 1.5 GB desktop, <= 1.0 GB Android (to be pinned after the first measurement; the number is a ceiling, not a target) | process counters |
| `project.new` without the pack | <= 500 ms | `project_new` |
| `content.starter_install` of the full pack | <= 5 s on an SSD, disk-bound; cook of the pack <= 90 s cold | `starter_install`, `starter_cook` |
| `starter.verify` | <= 3 s | `starter_verify` |

Min-spec machine class is the low-end Windows machine (Intel N150-class laptop, integrated GPU) in the lab plus the Android
mid-tier device; the sample uses the auto-quality tier detector so the lowest tier meets the budget, and a hard CI assertion is
a frame-time percentile from a recorded run, not a wall-clock wait.

## 13. Out of scope / follow-ups

- Asset marketplace or store, in-editor purchase, paid packs.
- A second game (deferred by the owner until all other plan features are complete); a finished external demo is a separate effort
  that starts from the `sample` template.
- Multiplayer template (waits for `regia_net`); VR/XR templates; console templates.
- Additional packs (themed environments, sci-fi, characters) and a pack manager with versioned updates and a diff view; here a
  pack update is a manual `content.starter_install { overwrite }`.
- Voice acting; the sample's dialogue is text with subtitles.
- Online tutorial telemetry; the learn flow records progress only in the project's local settings.

## 14. Provenance rows

| Area | Algorithm/behavior | Public source |
|---|---|---|
| Content licensing | CC0 public-domain dedication for acceptable third-party assets; SPDX license identifiers | https://creativecommons.org/publicdomain/zero/1.0/ ; https://spdx.org/licenses/ |
| Fonts | SIL Open Font License 1.1 terms for bundling and redistribution | https://openfontlicense.org/ |
| Procedural textures | Seeded gradient/value noise and tileable cellular patterns for generated materials | Perlin, "An Image Synthesizer", SIGGRAPH 1985; Worley, "A Cellular Texture Basis Function", SIGGRAPH 1996 |
| Content hashing | BLAKE3 tree hash for provenance content hashes | https://github.com/BLAKE3-team/BLAKE3-specs |
| Project creation flow | Ordered flow of template, defaults, location/name, generate; templates setting a default map | Public user documentation of project creation and packaging in several engines (workflow only; no text reused) |
| Localization | Fluent message format; locale fallback chains | https://projectfluent.org/ |

### Decisions made in review (ADR-1585)

- Create path, error variants, handler placement, smoke script format (spec 41) and dependencies: § 6, § 7, § 11.
- Size and count targets (§ 4) and the performance numbers (§ 12) are placeholders to be measured by the first authoring pass; the budgets in `pack.ron` are enforced by `starter.verify`, so a missed target is a recorded change to the budget, not a silent overrun. The `ja` locale and its CJK fallback font count against the `fonts` budget (2 MB cooked); if it does not fit, the font is subset to the glyphs the three locales use.
- The mannequin's mesh and 12 clips must fit the `meshes` class budget (4 MB cooked); animation clips are quantized by the normal cook. If they do not fit, clips drop to the 8 required ones (locomotion set, attack, hit, death).
- Pack formats registered: `pack.ron`, `*.provenance.ron`, `licenses.ron`, `template.ron`, `*.checklist.ron`, all v1; the lead runs `cargo xtask check-formats --update` and `cargo xtask manual`. They start at `format_version: 1`, so no migrator is needed yet.
- Step order of the creation flow (§ 2.1) was derived from Regia's own ordering constraint (a template can force or hide options; the target directory is chosen last) and is not a copy of another tool's wizard.

### Owner decisions

1. Working title "Ember Vale", the third-person action-adventure genre, and template content/naming need owner sign-off (master plan v0.3.x sign-off row). Product decision; the spec builds on the working title until told otherwise.
2. ~~Acceptance of the pack licence `LicenseRef-Regia-Starter` text~~ Decided 2026-11-20 (ADR-10225): starter and template code MIT-0, starter assets CC0-1.0; `LicenseRef-Regia-Starter` is withdrawn.

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
