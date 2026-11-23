# 05 — Milestones

Strictly sequential, in the order of this document (see `AGENTS.md` § 1.3; M15d deliberately runs after
M19). Each milestone lists goal, scope and **acceptance** (all
must pass and be recorded in `docs/progress.md`). Size is relative effort, measured as what a human
engineer would need (S ≈ days, M ≈ 1–2 weeks, L ≈ 3–5 weeks, XL ≈ 6+ weeks). It is used only to order
and compare milestones and is **not** a calendar estimate for the agent; forecasts come from the times
measured and recorded in `progress.md` (ADR-010).

**Direct Order 27 launches on Steam with Regia Engine** (owner decision, 2026-10-03). Regia Engine *is* DO27's
critical path, so milestones are ordered **DO27-critical first**; general-engine features DO27 doesn't
need (gameplay physics, character/save framework, Android depth, general samples) come after DO27 is
launch-ready. Steam comes **after the engine is stable**.

**Game assets are built in parallel with the engine.** Content formats are therefore frozen early and
versioned (`specs/12` § 4a); asset work never waits for the editor.

**Owner gates** A–D stop the build until the owner approves continuing. At each gate the builder writes a
gate report in `progress.md`: what works, measured numbers, cost so far vs estimate, risks, recommendation.

**Test-first rule:** each milestone starts by turning its acceptance criteria into failing tests,
scripts or agent-protocol checks.

**Platforms.** Active targets are Linux, Windows and Android; macOS release is deferred (ADR-009; testing-only bring-up in ADR-264, the owner has a
Mac) and is v0.4.0. Windows is verified in the owner's Windows VM only; what cannot be verified
there does not ship on Windows (`06` § 8).

**Platform depth before M20.** Every milestone builds for all three targets, launches its sample on
Android and passes the suspend/resume check (`AGENTS.md` § 1.10, § 5). Android *performance budgets* and
*physical-device* checks listed in spec acceptance sections for M4–M19 (`specs/11` § 8 M7, `specs/13`
§ 13, `specs/16` § 6, `specs/18` § 7, `specs/26` § 6) are measured and recorded in `progress.md` but only
become gating in M20 ("Android depth"). M1's lifecycle test is gating in M1.

**Samples before M20.** `samples/third_person` needs character movement and gameplay physics, which
arrive in M20. Until then, acceptance and budgets that need "the sample" use `samples/sim_bridge`
(from M3) and the per-feature labs (`input_lab`, `ui_gallery`, `audio_lab`, `anim_lab`, `vfx_lab`).

---

## Phase A — Prove the approach

### M0 — Workspace, CI, fast builds (S)
Bump `bevy_*` from `=0.20.0-rc.2` to stable `0.20.x` if released; `git init`; `deny.toml`;
`THIRD_PARTY_NOTICES.md`; build-speed setup (`03` § 6) with recorded baseline; CI (Linux/Windows
fmt, clippy, test, doc; Android check; cargo-deny; layer check via a new `xtask` crate; build-time report).
Toolchain: one rustup-managed stable compiler pinned in `rust-toolchain.toml` (with the Android targets)
for the owner's machine, CI and Android builds, plus `cargo-ndk` and `cargo-deny`.
**Owner prerequisites:** on Arch, replace the distro `rust` package with `rustup`; install the Android
SDK + NDK r27+; provide a Git remote; make the Windows VM reachable by the builder (SSH or a self-hosted
CI runner, `06` § 8).
**Acceptance:** CI green; one glam version in runtime crates; build-time baseline recorded.

### M1 — Platform bring-up, all three targets (M)
`specs/11` §§ 1–5, wgpu device/surface/clear, logging incl. logcat; `samples/hello_window`; the minimal
Android Gradle shim from `specs/21` § 4 (debug-signed APK, install, logcat) so that this and every later
sample can run on Android — M16 completes packaging (AAB, signing, icons).
**Acceptance:** `specs/11` § 8 M1.

### M2 — Core runtime, agent protocol v0, hot-reload spike (L)
`specs/10`; `regia_agent` v0 + `regia mcp` (`specs/23` § 8 M2); hot-reload spike comparing dylib
reload vs `subsecond` hot-patching on Linux and Windows → ADR (`specs/20` § 1a). The spike links the
full skeleton dependency set, not a toy crate (a large engine dylib can exceed Windows' limit of 65,535
exported symbols), and includes a game that owns an external simulation thread (the DO27 case,
`specs/25` § 3).
**Acceptance:** `specs/10` § 4; protocol v0 verifies `hello_window`; spike ADR accepted.

### M3 — Vertical slice: "hover slice" (L)
Thin end-to-end: glTF + PNG dev import, PBR + sun + 2-cascade shadows + sky + tonemap, basic heightfield
from 16-bit PNG, `sim_bridge` with `samples/sim_bridge` mock sim, chase camera, minimal input actions,
event one-shot sounds, egui debug inspector; **DO27 `shell-ferrite/`** driving one hull on a real DO27
map in a local player-host match (Linux + Windows); Android APK of the mock slice. The debug inspector
and the hot-reload host are the first slice of `regia_editor` — the only crate allowed to use egui and
`libloading` (`06` § 7, `04` § 5): a bare window that loads the game dylib, with no docking or panels
until M10.
**Acceptance:** DO27 slice playable 10 minutes without errors; rule A1 frame-rate test passes; mock slice
on all 3 platforms; agent protocol screenshots and drives both; hot reload per the M2 ADR works.

### ▶ GATE A — owner review (feel, frame times, iteration speed, agent protocol, cost) → continue/adjust/stop

---

## Phase B — Foundations DO27 needs

| # | Milestone | Size | Scope | Acceptance |
|---|---|---|---|---|
| M4 | Assets v1 + **content formats v1** | L | `specs/12` §§ 1–8, § 4a format freeze (materials, prefabs, levels, provenance `.meta`); `regia preview <asset>` standalone viewer + agent `screenshot(asset)` so asset work can be checked before the editor exists; migrator registry + `regia upgrade-project` skeleton, so every later format change ships with its migrator | `specs/12` § 9 M4 |
| M5 | Renderer v1 | XL | `specs/13` §§ 2–12 incl. layered material (`specs/26` § 4 row) | `specs/13` § 13 |
| M6 | Scene, levels, prefabs | M | `specs/14` Part A; `.prefab.ron` and `.level.ron` frozen | `specs/14` M6 |
| M7 | Input actions + runtime UI | L | `specs/11` §§ 6–7, `specs/18`; `.ui.ron` format frozen | `specs/11` M7, `specs/18` § 7 |
| M8 | Audio | M | `specs/16` incl. parametric patches (format frozen), voice capture, announcer helpers | `specs/16` § 6 |
| M9 | Framework core | M | `specs/14` B1, B2, B5, B6, settings; full `sim_bridge` (`specs/25` § 3) | `specs/14` M9 |

---

## Phase C — Editor and agent-native workflow

| # | Milestone | Size | Scope | Acceptance |
|---|---|---|---|---|
| M10 | Editor shell | XL | `specs/19` §§ 1–3 (of § 2: Viewport, Outliner, Inspector, Output Log, basic Content Browser) | `specs/19` § 8 M10 |
| M11 | Editor workflows + full agent protocol | XL | `specs/19` § 2 (remaining panels, and asset editors for the features that exist), §§ 4–7; `specs/20`; `specs/23` complete; generators; Blank template | `specs/19` § 8 M11, `specs/20` § 6, `specs/23` § 8 M11 |

### ▶ GATE B — owner builds a small level/material/prefab/script by hand and again only through an agent (MCP); agent benchmark ≥ 80%

---

## Phase D — DO27 look

| # | Milestone | Size | Scope | Acceptance |
|---|---|---|---|---|
| M12 | Animation | L | `specs/17` (DO27: pilots, walkers, turrets, sockets); glTF clip import; `.animgraph.ron` frozen | `specs/17` § 6 |
| M13 | Terrain and tunnels | L | `specs/26` §§ 1–2; terrain layer and scatter rule formats frozen | `specs/26` § 6 terrain |
| M14 | GPU VFX, decals, cosmetic debris | L | `specs/26` § 3 + debris row of § 4 (rapier used cosmetically only); `.vfx.ron` frozen | `specs/26` § 6 VFX |
| M15a | High-end renderer: scale and output | L | `specs/26` § 4 rows marked M15a — GPU-driven rendering, temporal upscaling + Deck profile, output (HDR, ultrawide, VRR, frame caps) | `specs/26` § 6 M15a |
| M15b | High-end renderer: light | XL | `specs/26` § 4 rows marked M15b — cached sun and local-light shadows, dynamic probe GI + screen-space GI/AO, SSR | `specs/26` § 6 M15b |
| M15c | High-end renderer: atmosphere | L | `specs/26` § 4 rows marked M15c — physically based atmosphere + time of day, volumetric fog, volumetric clouds | `specs/26` § 6 M15c |

M15a–c are what DO27's floor and mid tiers need. The showcase extras (M15d: cluster geometry, stochastic
many-lights, FSR port, baked GI) are not needed for Gate C and run after DO27's launch — see Phase G.

---

## Phase E — DO27 complete

| # | Milestone | Size | Scope | Acceptance |
|---|---|---|---|---|
| M16 | Packaging (desktop + Android, no Steam yet) | L | `specs/21` minus Steam; Linux build in Steam Linux Runtime container already (so the Steam step is only upload later) | `specs/21` § 6 |
| M17 | DO27 shell complete | XL | `specs/25` § 4 full inventory | `specs/25` § 6 |

### ▶ GATE C — DO27 feature-complete on Regia Engine (`specs/25` § 6)

---

## Phase F — Stable, then Steam

### M18 — Engine stabilisation: "stable" (L)
Soaks (30 min, 16-player DO27 sessions), crash-free runs, memory/perf budgets (`06`), content formats
v1.0 with migration tooling (`regia upgrade-project`), API review of everything DO27 touches, docs for
those areas (`book/` started).
**Acceptance:** zero crashes in 20 soak sessions; budgets met; no format change without a migrator;
owner signs off "engine stable".

### M19 — Steam + DO27 launch preparation (M)
`specs/24`; Steam Deck profile; depots/branches; DO27 playtest branch on Steam.
**Acceptance:** `specs/24` § 4.

### ▶ GATE D — DO27 Steam launch (owner)

---

## Phase G — General-purpose engine v1.0

### M15d — Showcase renderer (XL)
The `specs/26` § 4 rows marked M15d: cluster (meshlet) geometry with visibility-buffer shading, stochastic
many-light sampling (Ultra), the FSR 2/3 port, baked GI for Mobile/Low, and ray-traced paths if wgpu's
ray queries are stable by then. Split out of the old M15 (ADR-009) because DO27's floor and mid tiers do
not need it; it serves DO27's best-effort showcase tier as a post-launch update, and the general engine.
The owner may pull a single item forward at Gate B or after M15c.
**2026-10-07 (ADR-042):** M15d is pulled forward as the post-alpha renderer-excellence programme, `specs/27` § 4.2,
in the order reference path tracer → GI → visibility buffer + meshlets → TSR-grade upscaler → material graph, from
2026-10-18; each stage is accepted by `specs/27` § 2 numbers, the § 3 UE-plate verdict and a frame-time budget.
**Acceptance:** `specs/26` § 6 M15d, plus `specs/27` §§ 2–3.

**Showcase:** forest flythrough video (v0.4) and a held free-camera demo (v0.5): staged plan, demo definition, release options and owner decisions in `docs/showcase-plan.md` (ADR-2447, proposed).

### M20 — Complete the general engine (XL)
Full physics for gameplay (`specs/15`), framework gameplay (`specs/14` B3, B4, B7), foot IK and the
`third_person` animation integration (`specs/17` § 6), the Steam cloud backend for `SaveGame`
(`specs/24` § 2), Android depth (budgets, 50-cycle lifecycle, touch UI), `samples/third_person` and
`samples/arena` (a first-person bot arena, defined in `specs/14` acceptance M20), First Person and Third
Person templates, complete user and agent guides. Tag Regia Engine `v1.0.0` — ready for outside game
developers (`07-licensing.md`).
**Acceptance:** `specs/15` § 6, `specs/14` M20, `00-vision.md` § 7.

---

## v0.4.0 (see `versions.md`)
Content creation (split from `specs/22` on 2026-10-22): world streaming finished, material graph editor remainder,
the full sequencer, post volumes remainder, 2D toolkit remainder, foliage wind and grass, lens effects, OIT,
screen-space SSS, water in the runtime frame. Multiplayer, the UI designer and more importers moved to v0.5.0
(multiplayer and tools); second-game systems are v0.6.0; navigation/AI shipped earlier. macOS and iOS are owner-gated
beside the rungs (testing-only builds, ADR-264). Status per rung: `progress.md` "Rung status".

## v0.6.0 (see `versions.md`)
The editor complete: the second stage of the owner's plan to a v1.0 alpha (2026-11-12, ADR-8588). Scope is the
`versions.md` v0.6.0 row: the editor complete (material graph editor, sequencer, UI designer, profiler), tooling for
the second-game systems, one format bump carrying every frozen-format change (ADR-7724), agent parity, editor
extensions, stability, docs and accessibility. No second game has to be named; the sample game is dropped (DO27 is the
proof game). v0.5.x before it carries the v0.5.0 known issues and the reviewed runtime lanes, with no frozen-format
change.
**Acceptance** (all recorded in `progress.md` before the tag):
1. **Editor parity.** Every MUST row of the w12 editor parity matrix (ADR-7752: 185 of 372 rows) is DONE, OUT by
   ADR-7752, or deferred by an owner decision that an ADR records (ADR-7752's deferred rows list the owner's calls).
2. **The format freeze.** Every row of `specs/12-assets.md` § 4a "As planned for v0.6.0" has landed with its migrator,
   `regia upgrade-project` row and corpus fixture, or has moved to v0.7 by a recorded decision; `cargo xtask
   check-formats` is green against `docs/formats/format-hashes-v0.5.0.ron` (at most one version step per format,
   ADR-7724); `docs/releases/v0.6.0-upgrade-draft.md` matches the frozen table (`v060_note_truth`); at the tag the
   release lead commits `docs/formats/format-hashes-v0.6.0.ron` and points `RELEASE_BASELINE` at it.
3. **Smoke on four desktop hosts.** The editor smoke is green on the build server (lavapipe), the Windows rig on Vulkan and on
   DX12, and the Mac on Metal, with no open bug from the all-host smoke list unless the owner defers it.
4. **Accessibility.** The editor's accessibility tree is checked through the OS accessibility tree in real windows
   (the real-window checks of `book/agents/editor-a11y.md`), not only in the headless export.
5. **Gate B owner session** (Phase C above; spec 23 § 8): the owner builds a small level, material, prefab and script by
   hand and again only through an agent (MCP), and the agent benchmark is at 80 % or more.

## v1.0 alpha (see `versions.md`)
The third stage of the owner's plan (target about 2027-01-24, ADR-8588): the complete engine in outside hands. **No new
features.** It is not the sellable v1.0 (Phase G and the `versions.md` v1.0 checklist).
**Acceptance:** a truth pass on "complete" (ADR-1525, without the sample game since ADR-8588) with every item built or
an owner decision recorded; the format and API freeze (the v0.6.0 format table and `docs/api-snapshots/` unchanged
except by recorded fixes); the clean-machine developer path (`master-plan.md` § 3.7a: install to a packaged build on a
machine that never built the engine); the soak and lifecycle sessions (`06` § 1, the M18 rules); platform passes on
Linux, Windows, Android and the Steam Deck (macOS testing only); outside alpha testers, the forest-demo team first, with
their reports triaged.

## Critical-path risks and early checks

| Risk | Checked in | Fallback |
|---|---|---|
| **DO27's launch date now depends on Regia Engine** | every gate | Re-scope DO27's showcase-tier visuals before scope that blocks launch; gates A–C are the decision points |
| Asset work wasted by format churn | M4, M6, M7, M8, M12, M13, M14 freezes | Versioned formats + migrators; neutral sources (glTF/PNG/WAV) until a format is frozen |
| Code hot reload across OSes | M2 spike passed (ADR-015): dylib reload with a per-reload `-C metadata` salt, Linux and Windows | Fast relaunch with a state snapshot |
| Engine dylib export count (Windows limit 65,535) | M2 spike: 34,479 exports. CI warns at 50,000 and fails at 60,000 (`06`) | Move the platform/GPU stack into `regia_dylib_platform`, linked by `regia_dylib` |
| DO27 slice exposes bridge/perf problems | M3 | Adjust `specs/25` before foundations harden |
| High-end renderer misses DO27 floor tier | M15a–c | Simpler techniques per `specs/26` § 5 |
| wgpu ray-query support immature | M15b | Software SDF/probe tracing |
| wgpu HDR surface output (colour space, metadata) incomplete | M15a | Ship SDR; add HDR when wgpu exposes it |
| Windows VM cannot verify something (no GPU passthrough, overlay, audio devices) | M1, every gate | That feature does not ship on Windows; frame-rate gates are measured on Linux (`06` § 8) |
| Android Vulkan driver bugs | M1, M3, M5 (build), M20 (depth) | Device blocklist, reduced paths |
| intel_tex_2 portability | M4 | Cook on Linux/Windows; pure-Rust encoder |
| egui editor scalability | M10 | Virtualised lists, custom widgets |
| Steamworks + wgpu overlay | M19 | Workarounds; report upstream |
| bevy_* churn | each bump | Facade isolates; stay pinned |
