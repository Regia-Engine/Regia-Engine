# Versions

The plan is organized by **version**, not by tiers (owner decision 2026-10-21, ADR-1327). Older ADRs
and progress entries still say "Tier 1 / 1.5 / 2 / 3"; they are history and are not edited. Mapping:

| Old wording | Now |
|---|---|
| Tier 1 | through v0.3.x / v1.0 core |
| Tier 1.5 | optional extras (built only after the milestone's acceptance passes; never gate anything) |
| Tier 2 | v0.4.0 (`specs/22-tier2.md`, retitled "Spec 22: v0.4.0 feature set") |
| Tier 3 | parked |

## What "complete" means (owner decision 2026-10-27, ADR-1525)

**Complete** = every feature on this ladder through v0.6.0 is fully built, plus the eight items that were
unplaced (now placed below) and the items from the public-sources review (scratch report), except the
**Parked** row. Complete is required before the sellable v1.0 release. Items are built as engine
features, not tied to a game. A second game is considered only after complete: the v0.6.0 rung is built
anyway, but the demo/second-game rung waits.

**Amended 2026-11-12 (owner; recorded in ADR-8588):** the sample game is no longer part of "complete" (dropped; DO27 is
the proof game). The v1.0-alpha truth pass (the "v1.0 alpha" row) checks "complete" without it.

Priority: P0 is the Red-Rouge requests (v0.2.3) and the showcase-video features; P1 is everything else.
The sprint starts when DO27 and Red-Rouge finish their current tasks. The path is a hybrid: everything
that can run in parallel does (see "Parallel workstreams"), each item still gated for correctness.
Nice-to-have items (the "next tier" of the review, ML/XR if cheap) run in parallel only if they cannot
slow the critical path.

## Version table

**Every item on this ladder is a candidate, not a promise. It is built only if it actually improves the engine; each one needs a measured or demonstrated gain (or an owner call) before it is scheduled, and the owner may drop or move it.**

| Version | Scope |
|---|---|
| v0.2.0 | **Baked light.** Probe volumes: bake from scenes, runtime, near field, dynamic objects; four-GPU player sweep green; look sign-off by the owner. |
| v0.2.x | Baked-light extras: quality, leak control, specular from probes, incremental bake, editor bake panel, debug views, clouds/weather (candidates: foliage/grass at scale, weather such as rain, snow and wind, alongside the existing scatter and clouds work, v0.2.x to v0.3.x), bindless, other renderer follow-ups as they land. **Added 2026-10-22 (ADR-1380):** Windows on real hardware as a gate item (the test machine; `docs/master-plan.md` § 3.2a); terrain virtual texturing on High beyond a near band, after finding why AMD is excluded; VRAM budgets per tier with a capped streaming pool; `docs/perf-log.md` as the performance record (append-only, exclusive-card runs only); a public benchmark scene set; per-vendor plate PSNR floors in the real-GPU gate; an external beauty ground truth beside the path tracer; the nested-prefab fix; the `templates` CI job. |
| v0.2.3 | **Red-Rouge requests RR-1..RR-7 (P0, added 2026-10-27, ADR-1525).** RR-1 two clocks (render dt vs sim dt) and render-out manifest v2; RR-2 replay speed/interpolation (presentation-only, v2_full still refused); RR-3 per-bus audio stems; RR-4 AOVs including instance id and velocity; RR-5 fade composited after AOV capture; RR-6 scoped tool-proxy tokens (`tool.<name>.*`); RR-7 `.frec` input recording. Design constraints: the Red-Rouge lane design constraints (scratch plan, 2026-10-27). Runs first (workstream W-G). |
| v0.3.0 | **Lightmaps.** UV2 atlas, density rules, baker, storage plus migrator, runtime sampling. |
| v0.3.x | Three lines (owner decision 2026-10-22): **(m) renderer modularization first** (passes, resources and pipelines owned by the render graph, `frame.rs` split by pass, the forward shader split into imported modules with a bounded permutation set, a pass extension point); **(a) the renderer line, headed by dynamic GI** (`specs/27` stage 2a: SDF-traced probes with visibility) ahead of the rest: two-phase occlusion, visibility buffer plus meshlets, upscaler seam (DLSS/XeSS on the Windows rig; FSR3 frame generation prep only), hardware RT shadows; (b) general-engine completion (old M20), run in parallel: gameplay physics, character/save framework, templates, samples, guides; candidates also placed here: character controller polish, ragdolls, replay and demo recording, spectator camera, navmesh off-mesh links and dynamic obstacles, a basic sequencer; **added 2026-10-22:** the start of world streaming, HLOD and large-world coordinates, prefab variants, post-process volumes, a physical camera, an in-editor profiler, retargeting on real content, terrain editing, FBX import (pipeline-only, ADR first), the developer distribution and upgrade story, the headless dedicated-server target, the start of `regia_net` from DO27's `ir-net` (owner consent), and embedded Lua for mods as an optional crate (owner-preferred, ADR first); **added 2026-10-27:** the localization pipeline, built on the existing Fluent (`.ftl`) files (string tables, locale switching, subtitles, editor extraction/validation). |
| v0.4.0 "content creation" | First of the three releases the `specs/22-tier2.md` feature set was split into (owner decision 2026-10-22): world streaming finished, material graph editor remainder (with the shader-permutation policy in force), the full sequencer, post volumes remainder, 2D toolkit remainder, foliage wind and grass on a dynamic-caster shadow class, lens effects, OIT, screen-space SSS, water in the runtime frame (streams, lakes, a flat sea and the terrain's plane are drawn since ADR-1399 and ADR-1620; swell with surface queries is on the v0.1.28 line; the shore bake and floating bodies remain). An FFT ocean is a candidate for v0.4.x. **Added 2026-10-27:** DCC live link (editor-side, pipeline only); motion matching (animation, on top of retargeting); cloth and soft body (previously a v0.4.x candidate, now committed to complete); the plugin/extension API (stabilizing the v0.3.x (m) pass extension point and editor/runtime extension traits, with API snapshots); starter content (the asset set the templates and the sample game draw on). |
| v0.5.0 "multiplayer and tools" | `regia_net` completed from the `ir-net` extraction (replication, prediction, lag compensation, Steam sockets, lobbies, content handshake), the dedicated-server package, the UI designer, more importers, loudness normalization. The free-camera showcase demo (a packaged forest scene) is built and released here, not earlier; see `docs/showcase-plan.md`. **Added 2026-10-27:** voice chat (on `regia_net`, capture, codec, positional playback) and OpenXR (runtime and input; rendering through the existing swapchain seam). |
| v0.6.0 "editor complete" (owner plan 2026-11-12, ADR-8588) | **The editor complete**, including the material graph editor, the sequencer, the UI designer and the profiler (with the frame debugger); **tooling for the second-game systems** (generic vehicles, destruction level 2, procedural generation: their runtime systems shipped early, in v0.2.5 to v0.2.10, so this rung builds their editor tooling as engine features, and no second game has to be named first); **one format bump** carrying every frozen-format change of the line (ADR-7724; the rows and their state are in `specs/12-assets.md` § 4a "As planned for v0.6.0"); agent parity, editor extensions, stability, docs and accessibility. About four weeks after the v0.5.0 tag. Acceptance: `05-milestones.md` "v0.6.0". Before it, v0.5.x carries the v0.5.0 known issues and the reviewed runtime lanes, with no frozen-format change. Was (history, before ADR-8588): "second-game systems" (was v0.5.0 until 2026-10-22): Candidates, built only when the second game needs them, so the kinks get worked out against a real game, and only with a measured or demonstrated gain (or an owner call): generic vehicle physics; destruction and fracture level 2 (pre-fractured chunks: a fracture tool in the asset pipeline, chunk rigid bodies and breakable joints, pooling and caps; in multiplayer the server decides break events and clients play the fixed pre-fractured result); procedural generation (scatter and fill, a content graph built on the material-graph tooling, deterministic seeded and bakeable output; layout and asset-generation helpers only unless the game asks for more). If no second game is named by the v0.4.0 tag, Regia Engine's third-person template is finished to a free Steam demo by a team that does not build the engine, and that demo drives this rung. **Added 2026-10-27:** the sample game (built as an engine feature on the starter content; not a second game for the product). Under the "complete" definition the v0.6.0 items are built even without a second game; only the demo rung waits (see above). The owner dropped the sample game on 2026-11-12 (DO27 is the proof game). |
| v1.0 alpha (target about 2027-01-24; owner plan 2026-11-12, ADR-8588) | **No new features.** A truth pass on "complete" (ADR-1525, without the sample game since ADR-8588); a format and API freeze; the clean-machine developer path (`master-plan.md` § 3.7a: from install to a packaged build on a machine that never built the engine); soaks; platform passes; outside alpha testers, the forest-demo team first. About three weeks after v0.6.0. The v1.0 alpha is the complete engine in outside hands, not the sellable v1.0: DO27 live on Steam, the second game or outside-built demo row, the trademark and a support path stay v1.0 checklist rows. Acceptance: `05-milestones.md` "v1.0 alpha". |
| v1.0 | A viable product that can be sold. See the proposed checklist below. |
| v1.2 | After the sellable v1.0. Candidate, same caveat: destruction and fracture level 3 (runtime fracture, carving and deformation, with collider, navmesh and network sync). |
| macOS/iOS release | Owner-gated beside v0.4.0-v0.6.0 (Apple account and signing). |
| Parked | (not part of "complete") Consoles, web, digital humans, visual-graph codegen, Google Play services, mod SDK (WASM): parked, owner to place. Embedded Lua left this list on 2026-10-22 (v0.3.x candidate). |

## Not yet placed

Nothing is unplaced. The eight items that were listed here are placed (2026-10-27, ADR-1525):

| Item | Rung |
|---|---|
| Localization pipeline (on the existing Fluent files) | v0.3.x (b) |
| DCC live link | v0.4.0 |
| Motion matching | v0.4.0 |
| Cloth and soft body | v0.4.0 |
| Plugin/extension API | v0.4.0 (pass extension point from v0.3.x (m)) |
| Starter content | v0.4.0 |
| Voice chat | v0.5.0 |
| OpenXR | v0.5.0 |
| Sample game | Not planned: dropped by the owner on 2026-11-12, DO27 is the proof game (ADR-8588); the starter content and templates stay. Was: v0.6.0 (engine feature on the starter content) |

Placed earlier on 2026-10-22 (`docs/master-plan.md` § 3.10): realtime GI without a bake (v0.3.x (a)); post-stack
volumes, prefab variants, animation retargeting and the in-editor profiler (v0.3.x (b)); nested prefab fix
(v0.2.x); matchmaking (v0.5.0). Found to exist already: foot IK, HDR output. The master plan's feature
matrix (`docs/master-plan.md` § 4) records what a check of the code found for each.

## Parallel workstreams (owner decision 2026-10-27, ADR-1525)

Each workstream has its own integ line and conflict domain; one shared gate. Merges to main go in small
batches after the gate. Renderer `frame.rs` edits happen only in W-A. Format changes need migrators.
Every item gets an ADR, an agent surface and a guide page. Tags only on gated commits.

| Id | Workstream | Scope |
|---|---|---|
| W-A | Renderer | v0.3.x (m) modularization first, then (a): dynamic GI, visibility buffer, upscalers and RT shadows, lens effects, OIT, SSS, ocean. Owns `frame.rs`. |
| W-B | Light | v0.3.0 lightmaps (UV2, baker, storage plus migrator, runtime sampling); touches the asset pipeline bake first, `frame.rs` only at the sampling step. |
| W-C | Gameplay, AI, animation | Environment queries, state trees, smart objects, montages and warping, camera framework, data/curve tables and choosers, abilities, motion matching, retargeting, ragdolls, crowds. |
| W-D | Editor, tools, content | Validators and reference viewer, localization, per-entity levels and data layers, functional tests, material graph editor, sequencer, UI designer, prefab variants, terrain editing, in-editor profiler, DCC live link, starter content and sample game (the sample game dropped by the owner on 2026-11-12, ADR-8588), plugin API, Lua mods. |
| W-E | World, physics | World streaming, HLOD, large-world coordinates, vehicles, destruction L1-L3, cloth and soft body, procedural content graph, foliage and grass wind, water finish. |
| W-F | Net, platform, media | `regia_net`, dedicated server, lobbies and matchmaking, voice chat, chunks/DLC/Android packs, video playback, OpenXR, FBX import, per-release Windows/Android/Mac/Deck gates. |
| W-G | Red-Rouge and showcase (P0, first) | v0.2.3 RR lanes, showcase integration, and the showcase-video features. |

Prerequisites: an ADR registering the public-spec source class, then spec-writer lanes producing
Regia-term specs (builders never read the original text), then builder lanes.

**Truth pass 2026-11-01 (ADR-2033).** Every item below that was "still to place" has since been built and
shipped in a tag (v0.2.4 to v0.2.10); the master plan's matrix (§ 4) has the evidence per item.

| Item | Status at v0.2.10 | Shipped in |
|---|---|---|
| DCC scene feed | built (`dcc_feed`, format `dcc-feed-protocol-v1`) | v0.2.6, finished v0.2.9 |
| Motion matching | built (`regia_animation::motion_matching`) | v0.2.8 |
| Cloth | built (PBD, LOD, self-collision) and soft bodies (`regia_physics::cloth::soft`: pressure, tetra and shape-matching models, spec 55 § 2.8) | v0.2.7 |
| Localization pipeline | built (startup locale and fonts, gather, bundles, plurals) | v0.2.4, v0.2.9 |
| Plugin/extension API | built (editor and framework extension hosts, `extensions.ron`); no stable binary ABI | v0.2.9 |
| Starter content and a sample game | still thin: templates and samples only, no packaged starter content | not placed |
| OpenXR | `regia_xr` built on a scripted backend; the real loader binding needs a headset runtime and is not built | v0.2.6 (gaps), v0.2.10 (stereo) |
| Voice chat | built (capture, Opus, `voice-opus`) | v0.2.5, v0.2.9 |

Also shipped from rungs that were listed as future: vehicles, destruction level 2 and convex decomposition (v0.6.0
candidates, built early in v0.2.5 to v0.2.7); FBX and OBJ import; `regia_net` with replication, prediction, lag
compensation, lobbies and a dedicated-server package (the v0.5.0 rung, v0.2.5 to v0.2.10); the UI designer; EBU R128
loudness; embedded Lua (`regia_script_lua`); the nested-prefab fix; HLOD, stream grid and floating origin in
`regia_scene`; a terrain grass field; the sequencer; an FFT ocean (v0.2.10); soft bodies (v0.2.7, with cloth).

**Truth pass 2026-11-02 (ADR-2312), updated 2026-11-05 (ADR-2767).** Built on the v0.2.12 candidate (`integ/v033`) and
carried into the v0.2.13 candidate (`integ/v034`) on 2026-11-02: the FSR 3 frame interpolator
(`frame_gen::INTERPOLATOR_PORTED` is `true`; it was `false` in v0.2.11) and GPU order-independent transparency
(`regia_render::frame::oit_gpu`). Both are in the v0.2.13 tag (2026-11-03) and the v0.2.14 tag (2026-11-05); v0.2.12 was
never tagged (checked with `git grep` at the tags). Not built: a bundled DLSS/XeSS SDK (`XessSdk` and `DlssSdk` in
`crates/regia_render/src/upscaler/vendor.rs` are stub bindings). Added in the v0.2.14 tag: terrain and scatter casters
for the ray-traced sun (`regia_terrain::trace_proxy`, ADR-2310), off in every preset.

## Two-week burn-down update (2026-11-08)

The owner's v0.5.0 deadline is **Sunday 2026-11-22**. The ladder above remains the scope/candidate record;
its historical implementation notes are superseded, where contradicted, by the pinned code/test evidence in
[the v0.5.0 burn-down](plans/v05-burndown.md) and the replacement calendar in `master-plan.md` § 3.9.
ADR-4587 records proposed scope refinements; no acceptance criterion or owner gate is silently waived.

The audited v0.2.19 candidate (`origin/lane/sprint-integ-v041` 1a9420a40, based on v040 b81b90c34) already
contains lightmap format v7, tier derivation/multipage atlases, SDF/visibility GI and radiance caching, ReSTIR DI,
GPU OIT, the FSR3 interpolator, physical cameras, prefab variants, editor flame captures, streaming/HLOD, 2D
auto-tiling, sequencer tracks, UI designer, importers and loudness normalization. This is implementation evidence,
not all-platform acceptance. Real graph-driven built-ins, final lightmap/GI look and device evidence, failed
Android lifecycle acceptance, real network matches and demo integration/package proof drive the remaining work.
The XeSS evaluation remains a stub; OpenXR's real loader remains unbuilt and unplaced. Several finished demo and
network acceptance branches are outside the candidate and must be integrated rather than re-created.

Targets: v0.3.0 Nov 11; minimal m Nov 12 (only if approved), b Nov 13, a Nov 16; v0.4.0 Nov 18;
v0.5.0 Nov 22. These are conditional gate dates. The November 18 cut line moves only approved candidates/depth
work to v0.5.x; it does not turn failed mandatory tests, unavailable devices/accounts or unsigned look verdicts
into passes. macOS remains testing-only and the DO27 track remains separate.

## DO27 track (separate, not dated by us)

Gate C/D, M18 stabilisation and M19 Steam depend on Direct Order 27 and are not dated by the engine
plan. They run beside the version ladder.

## v1.0 "sellable" checklist (PROPOSAL for the owner to edit)

- [ ] Developer-ready docs and project templates (a new game developer gets from install to a packaged build without engine source).
- [ ] Stable, versioned content and API formats, each with a migrator and `regia upgrade-project` support.
- [ ] A support and update path (release cadence, upgrade notes, how issues are reported).
- [x] Licence terms final (`07-licensing.md`) (owner decision 2026-10-27).
- [ ] Rename and trademark (owner-gated). The rename is done: Regia Engine since v0.2.0 (ADR-1388); the trademark stays with the owner.
- [ ] DO27 live on Steam running Regia Engine.
- [ ] A second game (considered only after complete, 2026-10-27), or a Regia Engine demo built by a team that does not build the engine, shipped on Steam (added 2026-10-22).
- [ ] Windows verified on real hardware on every release (a gate item since v0.2.x).
- [ ] Developer distribution and upgrade story proven on a clean machine (added 2026-10-22).
- [ ] Consoles and scripting positions stated in `07-licensing.md` (added 2026-10-22).
- [ ] A blind look panel with people other than the owner (added 2026-10-22).
- [ ] Performance budgets (`06`) met on the reference hardware.
- [ ] Soak and lifecycle tests pass.
- [ ] Accessibility (UI and input) reviewed.
- [x] API stability snapshots (`docs/api-snapshots/`) taken for the public crates.

## Truth pass 2026-11-05 (ADR-2828)

Expected dates from the gap audit (`<scratch>`; confidence in `progress.md` "Rung status"):
v0.3.0 about 2026-11-08, v0.4.0 about 11-14, v0.5.0 about 11-19. They assume the owner answers within a day and that the
audit's lists are frozen scope. Since the last pass: lightmaps are built through v0.2.14 (UV2 atlas in the baker, GPU
baker, every tier, editor panel, levels, denoise and HDR block encoding); PIE with N clients in one process is on the
v0.2.15 candidate line; Steam sockets and lobbies are in tags but mock-tested only. Still open per rung are listed in
`progress.md`; the version table above is unchanged in scope.
