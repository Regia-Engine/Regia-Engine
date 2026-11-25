# Plan: a complete Regia Engine manual (v0.7.1)

Plan only. This lane does not rewrite the book. The inventory it commits is the
source the writing lanes share.

- Base: v0.7.0 candidate `425b388676` (`lane/sprint-manual-audit`).
- Inventory: `docs/manual/feature-inventory.md` (reading copy) and
  `docs/manual/feature-inventory.csv` (one row per feature: name, area, status,
  documented, owner, kind, notes).
- Decision: ADR-10417 (proposed).
- Release fragment: `docs/releases/v0.1.28-manual-audit.md`.

The manual's source stays the mdBook in `book/`. `book/SUMMARY.md` is the table
of contents. `cargo xtask manual` keeps generating `book/reference/` and the
single-file `docs/manual/manual.html`. `cargo xtask llms-txt` keeps generating
`dist/public-root/llms.txt` and `llms-full.txt`. Writing lanes do not edit
those generated files.

## How to read the inventory

5631 rows on this commit. Status: built 5597, partial 26, deferred 8.
Acceptance was not re-run. **built** means the feature is present in this tree.
**partial** means the code or `docs/versions.md` names a remaining gap.
**deferred** means `docs/versions.md` parks it or places it after v0.7.

| Kind | Rows | Documented NONE | What a NONE cell means |
|---|---:|---:|---|
| agent-method | 2342 | 0 | Every method name in the COVERAGE method table or the extension table has a book page. |
| cli-command | 181 | 2 | `regia package parity` and `regia export usda` have no page. |
| cli-flag | 751 | 197 | The flag text was not found on that command's pages. A heuristic, not 197 missing manuals. |
| crate | 56 | 0 | Every crate is named somewhere in the book. |
| editor-panel | 74 | 6 | Six panel ids have no matching editor-guide page. |
| feature | 155 | 0 | Named product features (quality, platforms, net, packaging, licensing, gameplay). |
| format | 247 | 0 | Every `format-hashes.ron` key is mentioned in `book/reference/formats.md`. A short key can match as a substring. |
| ladder | 8 | 8 | Parked or post-v0.7 items. They need a limits page, not a how-to. |
| module | 1716 | 1456 | Public Rust modules. A missing stem match is not a missing user feature. |
| spec | 101 | 43 | No book page cites that spec's filename. Traceability only. The feature may already have a guide. |

`regia_agent::schema::METHODS` has 1407 names. All 1407 appear in the COVERAGE
method table or the extension table. The 2342 agent rows are that union.
A wider scan of every `name:` string also matches fuzz fixtures and bone names.
Those strings are not features. Do not add them to the catalog.

`book/COVERAGE.md` stays the machine map (crate features, agent methods, CLI
commands, content formats, extension methods). The Feature Catalog is the human
map. It lists features, specs, CLI commands, editor panels, and the version
ladder. It links `book/reference/agent-methods.md` for methods and
`book/reference/formats.md` for formats. It does not paste the 1716 modules or
the 751 flags.

## Coverage gaps

A local checker followed `xtask/src/book.rs` for dead links, SUMMARY
reachability, and heading anchors (the slug in `xtask/src/manual.rs`: lowercase,
`_` and `-` kept, whitespace to `-`, other characters dropped; an anchor is
valid only when exactly one heading has that slug). Misses already listed in
`book/anchor-baseline.json` were not counted as new. The checker reproduced the
baseline exactly.

- Dead links: 0.
- Pages missing from `book/SUMMARY.md`: 0 (989 markdown files, 988 paths linked;
  `SUMMARY.md` itself is the file that is not a link target).
- New unresolved anchors: 0.
- Stale baseline rows: 0.
- SUMMARY paths linked more than once: 64.

This is not `cargo xtask book-check`. That command also checks schema coverage,
agent dispatch, RON fences, and Rust fences, and it compiles them. This lane
did not run it (no cargo on this machine). The lead runs it. See Commands the
lead runs.

### Pages to add

There is no Feature Catalog, no glossary, no A–Z index, no troubleshooting
page, no FAQ, no known-issues page, no upgrade page, and no print page.
`book/getting-started.md` is "Your first project" and
`book/start/first-agent-check.md` is "Checking your game with an agent". They
are real pages. They are not the two quickstarts this plan adds (install
through a running game; connect to the editor and drive a project).

Linux install steps live in `book/start/install.md`. There is no
`book/user-guide/linux.md` beside the Windows, macOS, Android, and Steam Deck
pages.

Six editor panel ids have no editor-guide page:

| Panel id | Owning crate |
|---|---|
| `net` | `regia_editor` |
| `mode2d` | `regia_editor` |
| `cameras` | `regia_editor` |
| `motion_matching` | `regia_editor` |
| `smart_object_def` | `regia_editor` |
| `match_rules` | `regia_editor` |

`smart_objects` is only weakly tied to `editor-guide/state-trees.md`. The
editor lane confirms that page or writes a smart-objects page. Combined pages
already cover `selected_debug` and `visual_log` (`debug-visual-log.md`) and
`notes` and `activity` (`notes-activity.md`).

Two CLI commands have no page:

- `regia package parity` (attached in `crates/regia_cli/src/main.rs` before clap, ADR-8428).
- `regia export usda` (`crates/regia_cli/src/commands/export_usda.rs`, ADR-4604).

Eight ladder rows have no page. They belong on the known-issues page, each as
a limit of this version: consoles, a general web game target, digital humans,
visual-graph code generation, Google Play services, a WASM mod SDK, an iOS
release, and destruction level 3. Destruction level 2 is built.

### Pages that are stale or easy to misread

Spot checks against this tree:

- `book/user-guide/rendering.md` opens as a forward+ Vulkan renderer. This tree
  also has a DX12 backend, Metal on macOS testing builds, and WebGPU and WebGL2
  on the product viewer (`regia_web`). The opening has to name each of those.
- `book/user-guide/auto-quality.md` names the VeryLow tier.
- `book/reference/render-profiles.md` does not mention VeryLow or the Cinematic
  profile. That page is generated. Do not hand-edit it. The lead regenerates it
  with `cargo xtask manual` after the picture lane confirms the preset list in
  `regia_render`.
- `book/agents/xr.md` already says `xr_no_loader`. Keep that. The real OpenXR
  loader, instance, and swapchains are not in `regia_xr`. `XrBackendKind::OpenXr`
  resolves to Off.
- `book/reference/cli.md` mentions `contact-sheet`, `play-gate`, and `logs`.
- `book/user-guide/networking.md` mentions lockstep, rollback, and relay.
- `book/user-guide/editor-guide/vehicle-tuning.md` is a real Vehicle Tuning tab
  (car, trailer, aircraft, bike, hover hull). SUMMARY.md also links it as
  "Vehicle tuning (v0.6 stub)", and it links the play window, asset creators,
  autosave, and panel timings the same way. The stub label is stale. The pages
  are linked twice.
- `book/user-guide/android.md` mentions the API level it targets.
- The first COVERAGE.md method table repeats 14 method names. Writers leave
  COVERAGE.md alone. The catalog lane lists the 14 names in its report. The
  lead removes the duplicates in one integration commit.
- COVERAGE.md section 4 has two GPU-ready mesh rows (layout v1 and layout v2).
  `docs/formats/format-hashes.ron` is the version of record.
- `regia_render`'s upscaler module still says no vendor SDK is in the tree.
  That is true of the SDK binaries. `regia_dlss` and `regia_xess` are the
  optional link crates. A rendering page that only repeats the older sentence
  hides them.
- `dist/public-root/llms.txt` describes llms-full.txt as the start, user guide,
  systems, and agent pages (about 378 pages in the committed header). New
  manual pages stay invisible to assistants until `cargo xtask llms-txt` runs.
  The generator must include `book/manual/`.

### Duplicate table-of-contents lines

SUMMARY.md links 64 paths more than once. The user-facing ones to repair (the
catalog lane is the only writer of SUMMARY.md):

- User-guide chapters linked from Getting started and again from Engine systems
  or later chapters: rendering, materials, the default look, lookdev, physics,
  animation, audio, gameplay, navigation, scenes, terrain-vfx, terrain-photoreal,
  terrain-data, the editor, gizmos, content-lint, deterministic-simulation
  (6 links), do27-host-checklist, do27-interface-contract, host-look-checklist,
  user-guide/index.md, merge-gate, perf-benches.
- Editor-guide pages linked twice, once with a "v0.6 stub" label: vehicle-tuning,
  play-window, asset-creators, autosave-recovery, panel-timings.
- Many `book/agents/` pages linked from more than one chapter (det-golden five
  times, det-ids-broadphase five times, net-input seven times, net-soak four
  times). Keep one link in the chapter a reader opens for that task. Other
  chapters link the chapter page, not the same agent page again.

Fundamentals currently labels `user-guide/index.md` as "Crates and imports".
That file is the user-guide introduction. The catalog lane points the line at
a real page. If none exists, `manual-create` adds
`book/fundamentals/crates-and-imports.md` and sends the SUMMARY line. Nobody
else creates that file.

### Partial rows, and which ones to trust

The catalog prints a status column. Use this classification. Do not copy every
inventory "partial" into the catalog as if the feature were missing.

Trusted partials (say so in the manual, with the limit in one sentence):

| Feature | Limit to write |
|---|---|
| Vendor upscaler DLSS | Optional `sdk` feature plus an external `DLSS_SDK`. Not bundled. Android and macOS never link. Falls back when not linked. |
| Vendor upscaler XeSS | Windows Vulkan, external `XESS_SDK`. Not bundled. Otherwise not linked. |
| Platform macOS | Testing only (ADR-009, ADR-264). Not a dedicated-server package target. |
| Graphics backend Metal | macOS testing builds. |
| Graphics backend WebGPU and WebGL2 | The product viewer (`regia_web`), not a general web game. |
| Cursor grab and confinement | Locked falls back to Confined on Windows and X11. Confined falls back to Locked on macOS. |
| Android lifecycle | Surface drop on suspend is specified. `docs/versions.md` (2026-11-08) still listed failed lifecycle acceptance. Not re-run here. |
| Multiple OS windows | Spec 11: only the primary window becomes an OS window today. |
| Steam sockets and lobbies | Code is in the tree. The 2026-11-05 versions note recorded mock tests. A two-account Steam session is owner-run. Do not write that a real match has been proven. |
| QUIC transport | `regia_net::quic` exists. v0.5 session scope names raw UDP and Steam sockets. Call QUIC partial until a guide says it is a supported session transport. |
| Steam upload, input manifest, and layout | `regia steam`. Credentials come from the environment only. The owner creates the Steam app. |
| Voxel terrain | Spec 122. The world lane confirms `user-guide/volume-mesh.md` against the crate before the catalog says built. |
| Video playback | Spec 50. Codecs are Cargo features and off by default. |
| DCC scene feed | Spec 60. Cargo feature `dcc-feed`, off by default, dev builds. |
| Spec 24 Steam | Same limit as Steam sockets. |
| Spec 58 OpenXR | Engine-side XR is in `regia_xr` (config, input, comfort, simulator, stereo render). The loader is not. `xr_no_loader`. |
| Spec 61 starter content | Templates exist. ADR-8588 dropped the sample game. Starter content was still thin at the 2026-11-01 truth pass. |

Human check before the catalog prints "partial". The word "proposed" or an
early milestone sentence in the first screen of the spec set the flag. The
body, or a later feature row, says the work is in the tree:

| Spec | What to do |
|---|---|
| 15 Physics | The opening still describes an early milestone. `user-guide/physics.md` and `regia_physics` have grown. The play lane reads both and tells the catalog lane the status. |
| 21 Tooling | The opening still says macOS has no cook target. macOS testing exists. Stale header. |
| 40 Content chunks | Guides exist (`user-guide/content-chunks.md`, agent pages). The feature row is built. Likely built. |
| 41 Functional tests | `agents/functional-tests.md` and `editor-guide/tests.md` exist. The spec filename is not cited. Likely built. Cite the filename on the tests page. |
| 57 Plugin API | Source plugins are built (feature row built). A later version holds dynamic extensions and marketplace metadata. Say that, with status built for source plugins. |
| 67 Procedural graph editor | The status line says proposed. The body says built. `editor-guide/procgen.md` exists. Cite the spec filename. |
| UI default design | Proposed ADR, largely built. The spec's own audit list is the note, after the play lane confirms which items are still open. |

### What this pass is

The book already has 989 pages, a table of contents, search, a generated
reference, and a single-file HTML manual. v0.7.1 adds the missing map
(catalog, glossary, index), two task quickstarts, task how-tos, a Linux
platform page, troubleshooting, FAQ, known limits, and upgrade notes. It
repairs the stale openings and the duplicate contents lines. It does not
rewrite the 752 agent pages. A lane edits an agent page only when this plan
names that file.

## Target structure

Keep the mdBook. Add one top-level section, owned by the catalog lane, placed
after Getting started:

```text
# Using this manual
- Feature catalog          manual/catalog.md          (generated)
- Glossary                 manual/glossary.md
- Index                    manual/index.md            (generated)
- Troubleshooting          manual/troubleshooting.md
- FAQ                       manual/faq.md
- Known issues and limits  manual/known-issues.md
- Upgrade notes            manual/upgrade.md
- Printing the manual      manual/print.md
```

Add two quickstarts under Getting started, ahead of the existing tour:

- `book/start/quickstart-human.md` — install, a new project, a running game.
- `book/start/quickstart-agent.md` — connect to the editor or the agent
  protocol and drive a project (stdio MCP and the HTTP option, one worked call).

Add task how-tos. Each one is a procedure with a finished check. The chapter
pages stay the reference for that system.

```text
book/how-to/create/     import a model, cook, levels and prefabs, content formats
book/how-to/picture/    quality and upscalers, light a level, materials
book/how-to/play/       character, combat and abilities, UI and audio
book/how-to/world/      terrain, streaming, procedural content
book/how-to/online/     host a game, lockstep and rollback
book/how-to/platforms/  package a game
book/user-guide/linux.md
```

A reader (human or agent) uses this order:

1. The quickstart for who they are.
2. The catalog, to see whether the feature exists in this version and its status.
3. The how-to for the task.
4. The user-guide chapter for the details.
5. The agent page, when they are driving the editor.
6. `book/reference/` for commands, formats, methods, and render profiles.

### Index

Writers mark a term with an HTML comment. mdBook's markdown renderer passes
HTML comments through, so the marker is invisible on the page and visible to
the generator.

```html
<!-- index: Lockstep -->
<!-- index: Dedicated server | see: Listen server -->
```

`cargo xtask manual` (extended by the catalog lane only) collects those
markers from every page SUMMARY.md reaches, plus every level-2 and level-3
heading. It writes `book/manual/index.md`, grouped A–Z then digits, US English
case-fold sort, one row of links per term. A `see:` marker is a cross-reference
with no page link. The page header says it is generated. `cargo xtask manual
--check` fails when the committed index is stale. Writers add markers on pages
they already own. They do not edit `book/manual/index.md`.

Headings alone are a poor index (many pages title a task, not the term a
reader looks up). Markers are how "DLSS", "join code", and "VeryLow" land on
the page that explains them even when the heading says something else.

### Catalog

The catalog lane generates `book/manual/catalog.md` from
`docs/manual/feature-inventory.csv`. One table per area. Columns: feature,
status, guide. Status is the trusted status in this plan, updated when a
writing lane's report corrects a human-check row. Formats are one paragraph
plus a link to `book/reference/formats.md`. Agent methods are one paragraph
plus a link to `book/reference/agent-methods.md`. Modules and per-flag rows
stay in the CSV. The page header says it is generated. Writers do not edit
the catalog or the CSV. They send corrections in their lane report. The
catalog lane applies them and regenerates.

### Glossary

Hand-written by the catalog lane, US English, one short definition per term,
a link to the chapter, no ranking language. Other lanes send terms in their
reports (`term — one sentence — page`). The catalog lane is the only editor
of `book/manual/glossary.md`. Include at least: project, level, prefab, cook,
pak, content chunk, quality tier, render profile, listen server, dedicated
server, lockstep, rollback, prediction, interpolation, lag compensation,
relay, lobby hub, join code, agent protocol, MCP, play in editor, and the
eight quality tier names.

### Printable copy

Do not add a PDF crate. The existing generator already writes one
self-contained HTML file, `docs/manual/manual.html`. The catalog lane adds
print CSS to that generator: a table of contents, a page break before each
top-level chapter, and readable type when the browser prints to PDF.
`book/manual/print.md` tells a reader to open that HTML and print it. A later
PDF dependency needs the lead's approval and a row in
`docs/03-workspace-and-dependencies.md` naming the license. ADR-10417 records
this.

`book/reference/` stays generated. `docs/manual/manual.html`,
`dist/public-root/llms.txt`, and `dist/public-root/llms-full.txt` stay
generated. Nobody hand-edits them.

### What else a complete manual needs

- An error-code index. Agent error codes are lowercase ASCII, digits, and `_`,
  and each code is documented on its agent page. The catalog lane adds
  `book/manual/error-codes.md` only as a generated list if `cargo xtask manual`
  can collect them without a new source of truth. Otherwise the FAQ points at
  the agent pages and the work waits. Do not invent codes.
- A keymap page. An editor-keymap format already exists. The editor lane adds
  `book/user-guide/editor-guide/keymap.md` describing the defaults a user can
  change, and sends the SUMMARY line.
- Three doors on `book/introduction.md`: a human making a game, a programmer
  linking the crates, and an agent driving a project. Each door is a short
  list of links. The catalog lane owns that file.
- Known issues for this version, tied to `docs/versions.md`. The known-issues
  page summarizes. It does not become a second ladder.
- Frozen format samples stay in `docs/formats/`. The create lane links them.
  It does not copy the bytes into the book.
- mdBook search stays on. The A–Z index is for the printed HTML and for an
  agent reading one file.
- One manual. Do not start a second wiki, a second HTML site, or a parallel
  set of pages outside `book/`.
- Link text that says where it goes. "Click here" is not a heading and not a
  link label.
- US English on every page a lane edits. Leave untouched pages alone, including
  agent pages this plan does not name.
- Cleanroom language, from `docs/01-cleanroom.md`. Describe Regia from its own
  code, specs, and ADRs. No claim that it is the first, only, or best engine.
  No comparison with another engine.
- `llms.txt` / `llms-full.txt` include `book/manual/` and `book/how-to/` after
  the lead regenerates them, or an assistant never sees the new front door.
- Per-feature limits on the partial rows above, so an agent does not invent a
  loader, a bundled vendor SDK, or a finished Steam match.
- Commands in the human quickstart and in `book/getting-started.md` stay the
  commands `crates/regia_cli/tests/getting_started_walkthrough.rs` runs, or
  that test is updated in the same change and the lead runs it.
- One platform matrix on `book/user-guide/linux.md`'s sibling pages, maintained
  on `book/manual/known-issues.md` as a single table: Linux, Windows, macOS
  testing, Android, Steam Deck, the product viewer, and the parked ladder.
  The ship lane writes the table into its report. The catalog lane puts it on
  the known-issues page so the six platform pages do not drift apart.

## Lane briefs

Eight lanes. About 2–5 hours each. They share no files. Lanes B–H run in
parallel. Lane A starts its new pages at the same time and does a second pass
after the other reports arrive: SUMMARY lines, catalog, index, glossary,
FAQ bullets, known issues, upgrade notes.

Every lane:

- Reads `docs/01-cleanroom.md` and this plan before writing.
- US English. No first / only / best claims. No other engine's name.
- Creates or edits only the files in its list. A new page under that list is
  in scope. A page not on the list is out of scope, even when the prose nearby
  is wrong. Report it.
- Does not edit `book/SUMMARY.md`, `book/COVERAGE.md`, `book/manual/catalog.md`,
  `book/manual/index.md`, `book/manual/glossary.md`, `docs/manual/*`,
  `book/reference/*`, `docs/manual/manual.html`, or `dist/public-root/llms*.txt`.
  Lane A is the exception for the files its own list names.
- Sends proposed SUMMARY lines, glossary terms, index markers it added, FAQ
  bullets, and status corrections in its lane report.
- Does not hand-edit a generated page. If a generated page is stale, the report
  says so and names the command.
- Adds `<!-- index: Term -->` markers on the pages it edits, for the terms a
  reader would look up.
- Documents a CLI flag when a user must know it to finish the task on that
  page. Does not add a page per flag. The 197 missing flag strings are a
  heuristic.
- Leaves agent pages alone unless the list names the file.
- Does not change engine behavior, formats, or the agent protocol. A format
  shape change is out of scope (spec 12 § 4a).
- Does not run cargo on the workstation. Book-check, `cargo xtask manual`, and
  `cargo xtask llms-txt` are lead commands unless the lane's own brief says
  it changed `xtask/` and must compile that change.

### A. `manual-map` (5 h)

Sole writer of the map. Runs a second pass after B–H report.

Owns:

- `book/SUMMARY.md` (may remove duplicate lines; the append-only rule does not
  apply to this sole writer)
- `book/introduction.md`
- `book/getting-started.md` (keep the walkthrough test's commands valid)
- `book/start/**`, including new `book/start/quickstart-human.md` and
  `book/start/quickstart-agent.md`
- `book/manual/catalog.md`, `glossary.md`, `index.md`, `faq.md`,
  `troubleshooting.md`, `known-issues.md`, `upgrade.md`, `print.md`
- `book/tutorials/**` (spot-check commands against the walkthrough; do not restyle)
- `book/anchor-baseline.json` (only when a reported heading rename requires it)
- `xtask/src/manual.rs` and the index-marker and catalog parts of
  `xtask/src/book.rs`
- `docs/manual/feature-inventory.csv` status and documented cells, from other
  lanes' reports

Does not edit `book/reference/*`, `docs/manual/manual.html` by hand, or
`llms.txt`. The generator writes the HTML and, when the lead runs it, the
llms files. Does not edit `book/COVERAGE.md`.

Covers: navigation, both quickstarts (install through a running game; connect
and drive a project), glossary terms collected from the other lanes, the
generated index, known limits (OpenXR loader, DLSS and XeSS unbundled, macOS
testing-only, the parked ladder, the sample game dropped by ADR-8588),
`regia upgrade-project`, and how to print the HTML manual.

Removes the duplicate SUMMARY lines listed above, including the "v0.6 stub"
labels on pages that are no longer stubs. Keeps one link per page.

### B. `manual-create` (3 h)

Owns:

- New `book/how-to/create/import-a-model.md`
- New `book/how-to/create/cook-a-project.md`
- New `book/how-to/create/levels-and-prefabs.md`
- New `book/how-to/create/content-formats.md`
- `book/fundamentals/ecs.md`
- `book/fundamentals/app-and-plugins.md`
- `book/fundamentals/assets.md`
- `book/fundamentals/reflection.md`
- `book/fundamentals/crates-and-imports.md` only if the catalog lane asks for it
- `book/user-guide/project-structure.md`
- `book/user-guide/scenes.md`
- `book/user-guide/importing-assets.md`
- `book/user-guide/import-rules.md`
- `book/user-guide/asset-ids.md`
- `book/user-guide/asset-registry.md`
- `book/user-guide/cooked-packs.md`
- `book/user-guide/content-chunks.md`
- `book/user-guide/data-tables.md`
- `book/changing-a-content-format.md`

Covers: ECS, app and plugins, assets, cook, import of glTF, FBX, and OBJ,
content formats (link `book/reference/formats.md` and `docs/formats/`; do not
edit the reference page and do not copy frozen samples), levels, prefabs,
content chunks. Documents `regia export usda` on the import page. Sends one
upgrade paragraph (what `regia upgrade-project` does, and that a format change
needs a version, a migrator, and that command) to lane A.

Human-check: spec 40. If the pages match the crate, tell lane A the status is
built.

### C. `manual-picture` (4 h)

Owns:

- New `book/how-to/picture/quality-and-upscalers.md`
- New `book/how-to/picture/light-a-level.md`
- New `book/how-to/picture/materials.md`
- `book/user-guide/rendering.md`
- `book/user-guide/auto-quality.md`
- `book/user-guide/dynamic-resolution.md`
- `book/user-guide/default-look.md`
- `book/user-guide/display-calibration.md`
- `book/user-guide/post-volumes.md`
- `book/user-guide/materials.md`
- `book/user-guide/shading-models.md`
- `book/user-guide/skinning.md`
- `book/user-guide/lookdev.md`
- `book/user-guide/bake-panel.md`
- `book/user-guide/render-out.md`
- `book/systems/lighting.md`
- `book/systems/material-graph.md`
- `book/systems/vfx.md`

Covers: quality tiers MobileLow, MobileHigh, Deck, VeryLow, Low, Medium, High,
Ultra; render profiles including Cinematic; upscale qualities Native, Quality,
Balanced, Performance, Ultra Performance; temporal upscalers TAAU, TSR, and
FSR 3; frame generation; DLSS (partial); XeSS (partial); present modes Vsync,
VsyncRelaxed, Immediate; dynamic resolution; automatic quality; GI, lightmaps,
the path tracer, post, materials, shading models, skinning.

Rewrites the opening of `rendering.md` so it names Vulkan, DX12, Metal on
macOS testing builds, and the product viewer's WebGPU and WebGL2. Does not
call any of them the only backend. States that DLSS and XeSS are optional link
crates against SDKs that are not in the repository.

Does not edit `book/reference/render-profiles.md`. Reports that the generated
page omits VeryLow and Cinematic so the lead can regenerate it.

### D. `manual-play` (4 h)

Owns:

- New `book/how-to/play/character.md`
- New `book/how-to/play/combat-abilities.md`
- New `book/how-to/play/ui-and-audio.md`
- `book/user-guide/physics.md`
- `book/user-guide/animation.md`
- `book/user-guide/audio.md`
- `book/user-guide/ui.md`
- `book/user-guide/gameplay.md`
- `book/user-guide/vehicles.md`
- `book/user-guide/2d.md`
- `book/user-guide/navigation-ai.md`
- `book/user-guide/goap.md`
- `book/user-guide/dialogue.md`
- `book/user-guide/localization.md`
- `book/user-guide/cloth.md`
- `book/user-guide/motion-matching.md`
- `book/user-guide/montages.md`
- `book/user-guide/input.md`
- `book/systems/physics-ccd.md`

Covers: physics, cloth, vehicles, destruction level 2 (level 3 stays deferred),
animation, montages, motion matching at runtime, audio, UI, dialogue,
localization, navigation, GOAP, 2D, input including cursor-grab fallbacks.
The editor panel for motion matching belongs to lane G. This lane owns the
runtime chapter only.

Human-check: spec 15 against `user-guide/physics.md`, and the UI default-design
audit list. Report the status to lane A. Cite spec filenames that the pages
do not cite today when the page is one this lane already edits.

### E. `manual-world` (3 h)

Owns:

- New `book/how-to/world/terrain.md`
- New `book/how-to/world/streaming.md`
- New `book/how-to/world/procedural.md`
- `book/user-guide/terrain-vfx.md`
- `book/user-guide/terrain-photoreal.md`
- `book/user-guide/terrain-data.md`
- `book/user-guide/terrain-sources.md`
- `book/user-guide/volume-mesh.md`
- `book/user-guide/world-streaming.md`
- `book/user-guide/sky-lab.md`

Covers: terrain, voxels (status stays partial until `volume-mesh.md` matches
spec 122), weather, water, world streaming and HLOD, procedural content
(specs 62–68) at the user-guide level. Does not edit `book/user-guide/editor-guide/`.
Sends panel notes for terrain, biome, and the procgen graph to lane G.
Human-check: spec 67 is built if the how-to and the existing procgen panel
page agree. Tell lane A.

### F. `manual-online` (4 h)

Owns:

- New `book/how-to/online/host-a-game.md`
- New `book/how-to/online/lockstep-and-rollback.md`
- `book/user-guide/networking.md`
- `book/user-guide/dedicated-server.md`
- `book/user-guide/deterministic-simulation.md`
- `book/user-guide/steam.md`
- `book/user-guide/steam-real-run.md`
- `book/user-guide/replay-sim.md`
- `book/systems/sim-bridge.md`
- `book/systems/snapshot-delta.md`
- `book/agents/voice-chat.md`
- `book/agents/net-dedicated-server.md`
- `book/agents/net-lockstep.md`
- `book/agents/net-prediction.md`

Covers: client, listen server, dedicated server, raw UDP, Steam sockets
(partial: mock versus a real session), lobbies (same limit), lobby hub, relay,
lockstep, rollback, replication, prediction, interpolation, lag compensation,
relevance, content handshake, voice, replay, desync, NAT and port mapping,
reconnect, join codes, host migration, the network conditioner, QUIC (partial),
password and bans, and Steam upload / input manifest / layout (credentials
from the environment, owner-created app). Does not write that a real Steam
match has been proven. Does not edit `book/user-guide/packaging.md` (lane H).

### G. `manual-editor` (4 h)

Sole writer of the editor guide.

Owns:

- `book/user-guide/editor.md`
- `book/user-guide/editor-guide/**`
- New pages, and only these, when the combined page does not already document
  the panel: `editor-guide/net.md`, `editor-guide/mode-2d.md`,
  `editor-guide/cameras.md`, `editor-guide/motion-matching.md`,
  `editor-guide/smart-objects.md`, `editor-guide/match-rules.md`
- `book/user-guide/editor-guide/keymap.md` (new; default shortcuts a user can change)
- `book/agents/video-playback.md` (codecs off by default; keep it consistent
  with `editor-guide/video.md`)

Updates `editor-guide/index.md` so each built-in panel id is listed once.
The 74 ids are in the inventory's editor-panel table. Combined pages stay
combined: `debug-visual-log.md` names `selected_debug` and `visual_log`;
`notes-activity.md` names `notes` and `activity`. Confirm `smart_objects`
against `state-trees.md` before adding a second page. Name the panel id in
the first screen of each page so the next audit's stem match succeeds.

Covers every built-in panel, play in editor, multiple OS windows (only the
primary window is an OS window), the DCC scene feed (feature `dcc-feed`, off
by default), source plugins (built; later dynamic extensions stay a note),
and spec 41's filename on the tests page. Sends SUMMARY lines for the new
pages to lane A. Other lanes do not edit this directory. They send panel notes
in their reports.

### H. `manual-ship` (3 h)

Owns:

- New `book/user-guide/linux.md`
- New `book/how-to/platforms/package-a-game.md`
- `book/user-guide/windows.md`
- `book/user-guide/macos.md`
- `book/user-guide/android.md`
- `book/user-guide/android-bringup.md`
- `book/user-guide/steam-deck.md`
- `book/user-guide/packaging.md`
- `book/user-guide/licensing.md`
- `book/user-guide/doctor.md`
- `book/user-guide/binary-size.md`
- `book/user-guide/sbom.md`
- `book/user-guide/package-hygiene.md`
- `book/user-guide/demo-package.md`
- `book/user-guide/test-bundles.md`
- `book/agents/web-player.md`
- `book/agents/xr.md`

Covers: Linux, Windows, Android, macOS testing, Steam Deck, the product viewer
versus the parked general web target, OpenXR (the loader gap stays; do not
invent a loader), packaging (Linux archive, Windows zip, Android APK and AAB,
dedicated server, AppImage, container image), licenses (MPL-2.0 engine,
MIT-0 templates and samples code, CC0 template and sample assets, CC-BY-4.0
book, CLA, trademarks, the engine credit line and `--no-engine-credit`),
doctor, SBOM, package hygiene. Documents `regia package parity` on the
packaging or package-hygiene page. Writes the platform-matrix rows into the
report for lane A's known-issues page. Sends FAQ bullets (install failures,
doctor, missing vendor SDK, Android install) to lane A.

`book/start/install.md` belongs to lane A. Link it from `linux.md`. Do not edit it.

## Commands the lead runs

This lane did not run cargo. After the writing lanes land, and after lane A
changes `xtask/`:

```text
cargo xtask book-check
cargo xtask manual
cargo xtask manual --check
cargo xtask llms-txt
cargo xtask check-formats --update
cargo test -p regia_cli --test getting_started_walkthrough
```

`check-formats --update` is owed because this lane cannot run xtask. The audit
did not change a format. The command should report a clean tree. If it would
rewrite a hash file, stop and do not commit that rewrite from this plan.

`book-check` is the authority for links, anchors, schema coverage, agent
dispatch, RON fences, and Rust fences. The local checker covered links,
SUMMARY reachability, and anchors only.

## Out of scope for every writing lane

- Engine behavior, public API, content-format shape, agent-protocol methods.
- `book/reference/*`, `docs/manual/manual.html`, `dist/public-root/llms.txt`,
  `dist/public-root/llms-full.txt`, except by running the generator (lead).
- Rewriting the agent guide as a set.
- A new dependency, including a PDF crate.
- Goldens, tests weakened to pass, and `#[allow]` to silence a lint.
- Comparing Regia with another engine, or claiming a first, only, or best.
