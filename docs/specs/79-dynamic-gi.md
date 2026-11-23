# Spec 79: Dynamic GI (SDF-traced probes with visibility), quality tiers and the runtime switch

Status: draft (lane `sprint-r1-dynamic-gi`, 2026-10-29). ADR-1679. Master plan § 3.4 (a) headline, `docs/versions.md`
v0.3.x (a), `specs/27` § 4.2 stage 2a, `specs/22` T2.8. No reviewed public spec exists for this item: the Regia
docs and the papers cited in § 10 are the sources.

## 1. Purpose

Leak-free dynamic diffuse GI on every tier without a bake. Stage 2a of `specs/27` asks for the probe field
traced against per-mesh signed distance fields and the terrain heightfield, with probe visibility so leaks reach
the `specs/27` § 2.2 bound, running on the RX 7900 and the Steam Deck.

What already exists (ADR-132, ADR-133, ADR-182, ADR-424, the Depth lighting option):

- camera-relative cascades of irradiance probes traced in compute against the exact heightfield (a max-height
  pyramid) and per-mesh signed-distance bricks, sun shadow rays through the same scene, multi-bounce through
  the previous frame's probes, an emitter list for emissive meshes;
- octahedral irradiance and distance-moment tiles per probe, Chebyshev visibility in every lookup, probe
  relocation and classification, hysteresis;
- a half-resolution screen lookup through which the forward and terrain passes read the probes;
- the Depth lighting option (`GiLook::Depth`), on by default on Desktop High and Ultra, off on Deck, Low and
  mobile, never on Android.

What was missing, and what this spec adds:

1. **Quality tiers.** The field had one size (4 cascades of 20 × 10 × 20, 16 000 probes, 128 000 rays a frame
   settled). A Deck player who picked Depth got the desktop field. `DynamicGiTier` sizes the field per tier, and
   each preset names its tier in the profile table.
2. **A runtime switch for the tier**, next to the lighting option that switches the probes on and off, for a
   game's options menu and for agents.
3. **Leak acceptance per tier**: the sealed-room bound holds at the Deck tier and with walls a twentieth of the
   probe spacing, not only at the 1 m test spacing.
4. **A cost surface**: each tier's budget (probes, rays, bytes, reach) and the last frame's measured GPU time per
   stage, on the protocol.

## 2. Tiers

A tier writes only these `DdgiSettings` fields; every other field (biases, hysteresis, emitters, diagnostics,
probe specular) stays what the host set.

| Tier | Presets | Cascades | Probes per axis | Finest spacing, growth | Settled re-trace | Bricks per cascade | Brick pool |
|---|---|---|---|---|---|---|---|
| `Low` | Deck, Desktop Low (mobile: never runs) | 4 | 12 × 8 × 12 | 2 m, ×3 | every 16 frames | 32 | 16 MB |
| `Medium` | none (player choice) | 4 | 16 × 8 × 16 | 2 m, ×3 | every 8 frames | 48 | 24 MB |
| `High` | Desktop High | 4 | 20 × 12 × 20 (ADR-5290) | 1 m, ×3.8 (ADR-5001) | every 8 frames | 64 | 32 MB |
| `Ultra` | Desktop Ultra, Cinematic | 4 | 20 × 12 × 20 (ADR-5290) | 1 m, ×3.8 (ADR-5001) | every 8 frames | 128 | 64 MB |

- A renderer starts at `High` (ADR-5001). Until ADR-5001 `High` equalled `DdgiSettings::default()`; the default now
  keeps that old layout (2 m, × 3, no ADR-2605 options) for hosts that build their own settings on it, and `High` is
  the default with the 1 m layout and the ADR-2605 options.
- `Ultra` keeps High's probe grid; it traces more mesh bricks per cascade, which changes dense scenes only. A
  faster re-trace on Ultra waits for an RTX 5080 measurement.
- Low and Medium keep the 2 m finest spacing; High and Ultra space their finest probes 1 m apart (each coarser
  cascade 3.8 times the last, 1, 3.8, 14.4 and 54.9 m), because 2 m probes straddle the walls of a 3–6 m room and
  read it over or under the path tracer (ADR-5001).
- A settled probe volume (its traced scene unchanged) stops updating once each finest probe has taken 512 updates at
  the tier's divisor, at least 600 frames (`DdgiSettings::freeze_frames`, ADR-5001).
- Every tier's finest cascade holds full weight from at least 3 m under the eye to 4 m over it, so a room around
  a standing eye reads the finest probes on every surface (§ 9.1, ADR-1949). High and Ultra are placed for 3 m
  under the eye and 3.5 m over it (1 m probes in 12 layers, `DdgiSettings::finest_headroom_m`, ADR-5290; with ten
  1 m layers High held 2 m over and 2.5 m under the eye, and a 6 m room's ceiling read the 3.8 m cascade alone,
  2.16 %). Low and Medium are placed for 3 m over the eye and 2 m under it (`finest_headroom_m` 3, ADR-5909; 2 m
  over and 3 m under before). Since ADR-5909 the lookup holds half a cell more of the finest cascade at each end
  of each axis (§ 9.12): 3 m under the eye and 4 m over it on Low and Medium, 3.5 m under and 4 m over on High and
  Ultra.
- A tier change that changes the probe count recreates the probe buffers: the field re-converges (new probes
  trace every frame for their first sixteen updates, ADR-5506; eight before).
- Low and Medium also seed a probe that starts over (`seed_updates` 4), keep a point on the finest cascade where a
  coarser one cannot see it (`visibility_blend`), weigh a young probe's later updates more (`recent_weight` 3) and
  reserve a smaller screen lookup region (`screen_max` 960 × 540 and 1280 × 720). High and Ultra turn on the first
  three since ADR-5001 and keep the default lookup region (§ 9.2, ADR-2605).
- Every tier sets `front_weights`: a probe behind the plane of the surface a point lies on does not light it
  (§ 2.1; Low alone until ADR-5716). Low and Medium also set `edge_visibility`, because 2 m probes straddle a house
  wall: a probe past the edge of the point's surface has to see the point without doubt (§ 2.1, ADR-5566).
  Neither option costs a ray, a probe or a byte.

### 2.1 The Low tier at a doorway: a probe behind the wall's plane, a probe past the ceiling's edge (ADR-5566)

Two settings since ADR-5716: `DdgiSettings::front_weights` (`ctrl.w` bit 23, the first term below; every tier sets
it) and `DdgiSettings::edge_visibility` (bit 24, the second term and its cover word; Low and Medium set it). They
were one setting, Low's alone, when this section was written; § 9.10 has what moved them. On 2 m probes a
6 m room holds three probe planes and the next one stands a metre outside its walls. The probe outside a door
then passes the visibility test for points inside (it sees them through the door) while its light is the other
side's: the wall's sunlit outer face, the sunlit ground. Low's door room read 1.54 times the path tracer's
indirect light. The lookups at a surface (the screen lookup's depth texels and the probes' own ray hits,
`ddgi_sample.wgsl`) take two more terms; a lookup in free space (the radiance cache's cells, which pass no view
vector) has no surface and keeps its weights:

- **A probe behind the surface's plane** (`ddgi_front_weight`). Its weight fades over 0.15 spacings behind the
  plane of the point's surface to a thousandth. From behind that plane the hemisphere about the normal holds the
  back of the wall the point is on. The wrap weight keeps a floor of 0.2 for such a probe and relies on the
  visibility test, which a probe behind a wall passes through an opening in that wall. The floor is not zero: where
  every visible probe stands behind the plane, they share the point as before.
- **A probe past the surface's edge** (`ddgi_edge_visibility`). The irradiance blend keeps a cover word per probe
  (probe record word 11): per cube direction, five bits, how much of the probe's distance lobe toward the surface
  behind that face ends within 1.05 spacings (1 − (mean − reach) / (limit − reach) of the moments' mean distance
  there; a probe on its first update counts as covered). For a point more than 0.25 spacings behind a probe along
  a face (in full from 0.4), the open share of that side of the probe's lobe says the point's surface does not
  reach behind the probe: the probe outside the door has the sky where the room's ceiling would lie. Where that
  share passes 0.6 (in full from 0.9) the probe has to see the point without doubt: its Chebyshev bound is cut at
  up to 0.9 (the bound's tail under the cut counts as hidden, as light-bleeding reduction does for variance shadow
  maps). The bound is loose exactly there: the probe's distance lobe straddles the lintel's lower edge, and the
  view bias (0.6 m on Low) moves the tested point from behind the lintel toward the opening. A probe AT the edge
  (the one in a tunnel's mouth plane, half under the roof) is a true neighbor of the wall beside it and keeps the
  plain bound, and so does any probe that sees the point cleanly (the probe before a room's open front): with
  every uncovered probe strict the tunnel lost light it does receive, and dropping probes by cover alone read
  the tunnel 0.151 from 0.119.

Measured on the acceptance scenes (RTX 5090, Low, mean FLIP): door room 0.319 → 0.238 (energy 1.54 → 1.26; an
oracle that drops every probe across the room's shell reads 0.212), tunnel 0.119 → 0.115 (p95 0.241 → 0.200, update
noise 0.0104 → 0.0094), color room 0.215 → 0.213, shaft 0.191 → 0.191, trench 0.107 → 0.107; at the tier's own
divisor, frozen, door room 0.316 → 0.230 and tunnel 0.115 → 0.108. Medium with the option: door room 0.315 →
0.244. Cost: no ray, probe or byte; one more word read per probe that may not see the point, one word written
per probe update.

What it does not do: the floor inside the door still blends with the probe outside (that probe stands in front
of the floor, over the same ground, and sees it), and the shaft and the color room keep their errors on 2 m
probes: those have both signs inside one probe cell, which no weight moves (ADR-5566 lists what would; § 9.10's
creases across the cell take 0.023 off the shaft and 0.016 off the color room).

## 3. API (crate `regia_render`)

- `regia_render::dynamic_gi` (new module, `src/dynamic_gi.rs`):
  - `enum DynamicGiTier { Low, Medium, High, Ultra }` (`Default` = `High`), `ALL`, `name()`, `parse()`,
    `of_preset(RenderProfilePreset)`, `apply(&mut DdgiSettings)`, `settings()`, `matches(&DdgiSettings)`,
    `budget()`.
  - `struct DynamicGiBudget { probes, rays_per_frame, probe_bytes, sdf_pool_bytes, reach_m, finest_above_m,
    finest_below_m, room_rule, screen_bytes, young_budget, young_settle_frames }` (the last two ADR-7870) and
    `budget_of(&DdgiSettings)`.
  - `struct DynamicGiStatus { tier, custom, active, enabled_override, budget, settled_frames, pending_bakes,
    emitters, gpu_ms, gpu_stages_ms }`.
- `RenderProfile::gi_tier: DynamicGiTier` (new profile column, `specs/13` § 3).
- `RenderSettings::dynamic_gi_tier: Option<DynamicGiTier>` (the player's override; `None` = the profile's
  column) and `RenderSettings::effective_dynamic_gi_tier()`.
- `regia_framework::settings::GiQuality { Low, Medium, High, Ultra }` and
  `GameSettings::graphics.gi_quality: Option<GiQuality>` (the persisted options-menu value, applied live to
  `RenderSettings::dynamic_gi_tier` with the lighting option's rule: a host value stays while it is unset).
- `ExtractedFrame::dynamic_gi_tier` (the effective tier; the render systems apply it each frame).
- `FrameRenderer::set_dynamic_gi_tier(tier) -> bool`, `dynamic_gi_tier()`, `dynamic_gi_status()`. Setting the
  tier already applied does nothing, so a host's own `set_ddgi_settings` survives until the tier changes.

On/off stays the lighting option (`RenderSettings::gi_look`, `GameSettings::graphics.lighting`,
`FrameRenderer::set_ddgi_enabled`); the tier only decides what the probes cost when they run.

## 4. Files and formats

No new file format and no change to a frozen one (`specs/12` § 4a). `RenderProfile` and `RenderSettings` are not
serialized. The player's settings file (`settings.ron`, `GameSettings`) gains an optional
`graphics.gi_quality` with `#[serde(default)]`, the way `graphics.lighting` and `graphics.calibration` were
added: a file written before it loads with `None` (tested), so no migrator is needed; the settings file is not
in the frozen list and carries no `format_version`. An older build's strict loader (`from_ron_strict`) refuses
a newer file that sets the field, as for every earlier optional field. **Migration note:** a host that builds `RenderProfile`, `RenderSettings` or `ExtractedFrame` with a
struct literal adds the new field (or uses `..` from a preset or `Default`); nothing on disk migrates.

## 5. Agent protocol (`specs/23`)

| Method | Params | Result |
|---|---|---|
| `render.get_dynamic_gi` | none | `{ tier, setting, preset_tier, lighting, probes, available, tiers: [{ name, probes, rays_per_frame, probe_bytes, sdf_pool_bytes, reach_m, finest_above_m, finest_below_m, room_rule }], renderer: { tier, custom, active, enabled_override, budget, settled_frames, pending_bakes, emitters, gpu_ms, gpu_stages_ms: { trace, relocate, irradiance, distance, lookup } } \| null }` |
| `render.set_dynamic_gi` | `tier`: `"low"\|"medium"\|"high"\|"ultra"\|"default"`; `enabled`: `true\|false\|null`; `young_budget`: integer or `null` (ADR-7870; at least one) | `{ before, after, note }` |

`render.get_dynamic_gi` also returns `persisted` (`GameSettings.graphics.gi_quality`, `"default"` when unset,
null without `GameSettings`); `render.set_dynamic_gi` writes it with the tier. `enabled` goes through `render.set_gi_mode` (`depth`, `balanced`, `default`), so it also writes the
persisted lighting option. Error codes: `F0002` (bad params), `F0003` (no renderer settings),
`unknown_gi_tier`, `dynamic_gi_unavailable` (`enabled: true` on Android), `dynamic_gi_no_renderer` (`young_budget`
without a runtime renderer, ADR-7870). Every budget also reports `young_budget` and `young_settle_frames`, and
`renderer` reports `young_budget`. Guide topic `dynamic-gi`.

## 6. Guide page

`book/agents/dynamic-gi.md`: what the tiers are, how to switch them, how to wait for the field to settle
before a capture, how to read the cost, the error codes.

## 7. Performance budget

Per tier (`DynamicGiTier::budget`, unit-tested so the numbers here cannot drift):

| Tier | Probes | Rays per frame (settled, all active) | Probe buffers | Brick pool | Reach (half extent of the coarsest cascade) |
|---|---|---|---|---|---|
| Low | 4 608 | 8 640 | 14.4 MB | 16 MB | 324 m |
| Medium | 8 192 | 30 720 | 25.7 MB | 24 MB | 432 m |
| High | 19 200 | 72 000 | 60.1 MB | 32 MB | 549 m |
| Ultra | 19 200 | 72 000 | 60.1 MB | 64 MB | 549 m |

Rays per frame: a settled probe of cascade `c` re-traces every `update_divisor << min(c, 3)` frames (`ddgi_trace.wgsl`
`classify`), so a frame traces on average `(probes / 4) · (1 + 1/2 + 1/4 + 1/8) / update_divisor` probes of 64 rays (fixed
2026-11-09, audit render10 #3: the figures were `probes · 64 / update_divisor`, as if every cascade re-traced at the finest rate).

Probe buffers: 526 words of record and tiles plus 64 rays of 16 bytes plus one queue word, 3 132 bytes a probe.
`finest_above_m` and `finest_below_m` are the finest cascade's full-weight span around the eye: 3 m below and 4 m
above on Low and Medium, 3.5 m below and 4 m above on High and Ultra (ADR-5909: the placement's window, 2 m below
and 3 m above on Low and Medium, 3 m below and 3.5 m above on High and Ultra, plus the half cell the lookup holds
past it at each end; before ADR-5909 3 m below on every tier, 2 m above on Low and Medium and 3.5 m above on High
and Ultra, ADR-5290; 2 m above and 2.5 m below with ADR-5001's ten layers, 7 m below before it).
The screen lookup region is not counted in the probe buffers; `DynamicGiBudget::screen_bytes` reports it: 66.4 MB on
High and Ultra (sized for a 3840 × 2160 internal frame at half resolution), 16.6 MB on Low (1920 × 1080; the Deck's
1280 × 800 needs 8.2 MB), 29.5 MB on Medium (2560 × 1440). Below the default region a frame whose half-resolution
lookup does not fit takes a quarter-resolution one (ADR-2605).

GPU time budget (`specs/27` § 2.5 lines; ★ first guesses, replaced by the measurement): Desktop High probes ≤ 0.6 ms at 1440p on the RX 7900 and the RTX 5080
(ADR-133 measured +0.26–0.57 ms on the RTX 5090 at sustained clocks); Deck tier ≤ 1.5 ms of the Deck's 25 ms
frame at 800p ★. The lavapipe numbers in `docs/progress.md` and the lane report are correctness runs, not
budgets; the GPU numbers are the lead's (§ 9).

## 8. Determinism

The tier is pure data: `apply` writes constants, and the probe update is unchanged (the per-frame ray rotation
is seeded by the frame counter, `random_rotation`). The same tier, scene and frame sequence give the same
probes. A tier change resets the field like any probe-volume resize. Nothing here enters the simulation.

## 9. Tests and acceptance

- Unit (`dynamic_gi.rs`): High equals the defaults; a tier writes only its fields; budgets decrease going down
  and every tier reaches ≥ 250 m and fits the instance list and the clamp; the § 7 numbers; names and presets.
- Integration (`tests/dynamic_gi.rs`, lavapipe): **sealed room at the Low tier**, 20 cm walls, interior floor
  ≤ 0.5 % of the exterior ground (`specs/27` § 2.2), scored at the steady state (probes placed with the eye inside);
  **sealed room with 10 cm walls at the Medium tier**, same bound; **every surface** (floor, ceiling, wall, upper
  wall) under the same bound at the Low and High tiers; **a room with a door** still takes its daylight, against the
  1 m layout; **a walk on the Low and High tiers** at their own update rates meets the bars of `tests/ddgi_temporal.rs`
  (§ 9.1); the tier switch without a device (resizes, keeps the host's fields, same tier is a no-op); presets carry
  their tier and the player's override does not make a profile custom; `tier_cost` (ignored, measurement only)
  prints the per-stage GPU time and the frame's CPU time per tier.
- Agent (`regia_agent::dynamic_gi` unit tests and `coverage_calls`): get and set, the error codes, a refused call
  changes nothing.
- Needs hardware (the lead): `tier_cost` on the RX 7900 (RADV), the RTX 5080 and the Deck; the `specs/27` § 2.2
  GI rows on the tunnel and borehole plates at the Deck tier against the path tracer (the master plan's
  starting numbers, GI in sun shadow vs the PT: tunnel_interior 2.56×, set_piece_borehole 1.63×, `specs/27`
  § 2.0).

### 9.1 Finding: a steady-state leak at the shipped layout (fixed, ADR-1949)

`sealed_room_diagnostics` (lavapipe, every probe re-traced every frame) found a steady state, not a transient: 7.25 %
at 2 m × 4 cascades against 0.04 % at the 1 m × 2-cascade layout the old `ddgi` test used. The cause, confirmed by the
measurements below, is the probes' height placement. The blend window (`ddgi_blend_inset`) needs 1.75 cells from
either end for full weight and ends 2 cells short of the top, and the volume kept 60 % of its height under the eye. With 8
layers the top of every cascade's full-weight span lay 0.55 cells *under* the eye: a point at the eye's height was
45 % cascade 0, and anything 1 m over it read the 6, 18 and 54 m cascades alone. Those cannot resolve a 20 cm roof,
and relocation pushes their probes out of a small room, so the sky came in twice: directly on the upper surfaces,
and through the finest probes, whose rays hit those surfaces.

The fix (ADR-1949) has two parts:

1. **Placement.** `eye_lift_cells`: every cascade reaches full weight at the eye's height, and the finest one keeps
   1.8 m under it first (a standing eye's floor) and then up to 2 m over it, which is a cell at every tier's 2 m. A host
   layout finer than that gets the floor first: 1 m with 8 layers holds 1.8 m under and 0.7 m over. A volume with 10 or more layers keeps its coarser cascades where they were, so
   Desktop High's far ground under a high camera keeps its reach; with 8 layers they sit a little lower around the
   eye, and cascade 0 of every tier moves up.
2. **Ray read rule.** `ddgi_irradiance_traced`: a ray hit reads the tracing probe's cascade or finer wherever those
   hold the hit; only a hit beyond its own volume takes a coarser cascade.

Both apply only to a layout whose coarsest cascade is wider than 3 m (`DdgiSettings::needs_room_rule`, every tier). A
layout of 1 and 3 m cascades does not leak (0.04 %) and keeps its placement and its ray read unchanged, because the
rule pulls light toward the finest probes and such fine probes are too young to hold it under a fast camera. The
measurements below.

Interior over exterior ground (6 × 3 × 6 m sealed room, 100 frames with the probes placed with the eye inside):

| Layout | Walls | Floor | Ceiling | Wall | Upper wall |
|---|---|---|---|---|---|
| Low, before | 20 cm | 6.814 % | 15.207 % | 11.986 % | 14.552 % |
| Low, ray rule alone | 20 cm | 0.098 % | 13.707 % | 7.676 % | 12.539 % |
| Low, after | 20 cm | 0.000 % | 0.009 % | 0.001 % | 0.002 % |
| Low, before | 10 cm | 6.834 % | 16.186 % | 14.020 % | 18.317 % |
| Low, after | 10 cm | 0.067 % | 0.121 % | 0.119 % | 0.113 % |
| High, before | 20 cm | 3.930 % | 1.010 % | 0.993 % | 0.914 % |
| High, ray rule alone | 20 cm | 0.000 % | 0.086 % | 0.009 % | 0.036 % |
| High, after | 20 cm | 0.000 % | 0.010 % | 0.002 % | 0.002 % |
| High, before | 10 cm | 4.077 % | 1.335 % | 1.312 % | 1.204 % |
| High, after | 10 cm | 0.068 % | 0.123 % | 0.119 % | 0.114 % |

The table comes from an earlier switch build with a different wall view. The final code reads, 20 cm walls, floor /
ceiling / wall / upper wall: Low 0.000 / 0.009 / 0.026 / 0.032 %, High 0.000 / 0.010 / 0.028 / 0.033 %. A room with a door
keeps its daylight: floor 0.00706 at Low, 0.00731 at High, 0.00504 at the 1 m layout, which keeps its old placement.

On lavapipe the placement alone cost about 15 % of the High probe update in the `tier_cost` scene; the GPU numbers are
the lead's (§ 9).

**Rooms taller than the finest span** (`room_matrix`, shares of the exterior ground, 20 cm walls; a surface over 2 m above
the eye reads coarser cascades directly):

| Room | Layout | Floor | Ceiling | Wall | Upper wall |
|---|---|---|---|---|---|
| Sealed 6 m, before | Low | 4.645 % | 12.870 % | 13.577 % | 14.312 % |
| Sealed 6 m, placement alone | Low | 3.622 % | 13.763 % | 3.291 % | 12.399 % |
| Sealed 6 m, after | Low | 0.017 % | 10.931 % | 0.000 % | 7.733 % |
| Sealed 6 m, before | High | 5.341 % | 10.344 % | 3.165 % | 9.789 % |
| Sealed 6 m, placement alone | High | 3.407 % | 9.839 % | 2.590 % | 9.634 % |
| Sealed 6 m, after | High | 0.018 % | 6.932 % | 0.000 % | 5.511 % |
| Door 3 m, before | Low | 22.108 % | 19.033 % | 13.490 % | 17.825 % |
| Door 3 m, after | Low | 16.434 % | 9.173 % | 2.227 % | 2.077 % |
| Door 3 m, before | High | 21.167 % | 10.253 % | 4.395 % | 4.469 % |
| Door 3 m, after | High | 16.616 % | 9.232 % | 2.293 % | 2.136 % |
| Door 6 m, before | Low | 21.848 % | 13.983 % | 16.252 % | 15.672 % |
| Door 6 m, after | Low | 18.860 % | 17.547 % | 4.316 % | 13.319 % |
| Door 6 m, before | High | 24.719 % | 18.881 % | 8.924 % | 16.574 % |
| Door 6 m, after | High | 19.007 % | 15.869 % | 4.450 % | 11.743 % |

So the ray rule earns its place: it takes the 6 m room's floor and walls from about 3.5 % to under 0.02 %. **Still open:** the top
of a 6 m room reads 5.5 to 10.9 %, above the 0.5 % bound. That surface is above the finest cascade's full-weight span,
and Low's 2.5-cell span cannot hold a 6 m room at all (it reaches 5 m). What share of the door rooms' numbers is real door
light has no reference yet: the path-traced plates of `specs/27` § 2.2 decide that.

**Under motion** (`tests/ddgi_temporal.rs`, the camera walks an invariant corridor; mean difference from the held view in luma
levels, bar base + 2 = 2.113):

| Layout, own update rate | Before | After |
|---|---|---|
| Low, 0.25 m a frame | 2.312 (fails) | 1.679 |
| High, 0.25 m a frame | 1.293 | 1.274 |
| Low, 0.5 m a frame | 2.481 (fails) | 1.942 |
| High, 0.5 m a frame | 1.628 | 1.644 |
| 1 and 3 m cascades, 12 × 8 × 12, 0.25 m a frame | 1.790 | 2.214 with the rule; 1.790 without |

The last row is why the rule is gated. Those 1 m probes live about 48 frames at this speed, and what the camera faces is
younger than that. The rule moves reads onto them and the walk reads darker (signed mean −0.42 before, −0.77 with
the rule). A settle of 240 frames does not close it (1.586 before, 2.373 with the rule), so the probes under the walk are the
cause, not an unconverged reference. The cost shows at speed on the shipped layouts too, in one number: 60 frames
after a walk at 0.5 m a frame the light is still 2.583 from the held view on Low (2.127 before, bar about 2.15) and 2.065 on
High (1.831 before). Low at that speed fails before on the walk's bar and after on the held bar. Seeding a new
probe from the next cascade is the direct fix; it is not built.

### 9.2 Seeding and the visibility blend (ADR-2605)

Two faults § 9.1 left open, fixed on the Low and Medium tiers and available to every layout:

1. **The top of a tall room** (`DdgiSettings::visibility_blend`). A sealed 6 m room's ceiling lies 4.4 m over a standing
   eye, above the finest cascade's full-weight span, and the blend window handed it to the 6 m cascade, whose probes stand
   outside the roof. `ddgi_sample_cascade` reports the cascade's visible trilinear mass at the point (a corner not traced,
   inside geometry or behind a wall sees nothing). With the blend on, the lookup samples the finest cascade whose probes hold
   the point first; a coarser cascade keeps only `clamp(v_coarse / (0.75 · v_fine), 0, 1)` of its share (the rest reads the
   finest one), and where the finest one sees the point its light caps the coarser one's at 1.25 times (a coarse probe inside
   the room sees the ceiling well but gathered its light at a spacing that cannot resolve the roof). On open ground the
   cascades agree and nothing moves. The probes' own ray read keeps the plain lookup (with the blend there too, a door
   room's floor doubled through far-field reads the ray rule never needed to touch). The keep and the cap fade out over the
   last quarter cell of the finest cascade's unsnapped blend window, so where the lookup leaves that cascade the seam moves with
   the eye instead of jumping a cell on each scroll (the pop ADR-424 removed from the cascade blend).
2. **The light after a fast camera stops** (`DdgiSettings::seed_updates`). A probe that starts over gets a seed worth that
   many of its own updates: one whose slot scrolled to a new cell from its neighbor one cell toward the volume's inside
   (the probe the scroll stepped past), else from the next coarser cascade at its position; one leaving geometry from the
   next coarser cascade, else that neighbor. Every texel of a tile finds a seed or none is written, and nothing seeds from
   a probe that has not traced. The distance tile is never seeded.

   A seed's source must see the probe: the neighbor only when its distance moments put no surface between them, the
   coarser cascade only where its probes that see the point carry 0.9 of the trilinear weight (an unchecked coarse seed on
   the floor-layer probes, which relocate out of the ground, lit a sealed room's floor to 4.4 %). A probe leaving geometry with a
   seed restarts its running average with it, so the seed counts for its weight and not for the settled hysteresis (about 33
   updates at 0.97). On a light cut nothing seeds: every source still holds the old light.
3. **Slow multi-bounce convergence** (`DdgiSettings::recent_weight`, default 1). A young probe's running average weighs its
   early updates, which carry fewer bounces, like its late ones, so its light approaches the answer as `n^−(1 − ρ)` for a
   bounce gain ρ. Weighing the `k`-th update `k^(w − 1)` makes that `n^−w(1 − ρ)`.

High and Ultra keep all three off, so Desktop High and Ultra frames do not move; turning them on is a look change for the owner
(the `specs/27` § 2.2 plates and a by-eye check).
Since ADR-5001 High and Ultra turn all three on, with their 1 m finest cascade (§ 9.3); the owner's GI review (11-15/16)
decides the look.

Measured on lavapipe (`tests/dynamic_gi.rs`, `tests/ddgi_temporal.rs`):

| Case | Before | After |
|---|---|---|
| Sealed 6 m room, Low, ceiling / upper wall | 10.93 % / 7.73 % | 0.110 % / 0.115 % |
| Sealed 6 m room, High layout with the blend, ceiling / upper wall | 6.9 % / 5.5 % | 0.029 % / 0.060 % |
| Sealed 3 m room, every surface, Low and High | ≤ 0.033 % | ≤ 0.035 % |
| Door room floor, Low / 1 m layout | 0.00706 / 0.00496 | 0.0115 / 0.00591 (open: § 9.2 note) |
| 60 frames after a stop from 0.5 m a frame, Low (bar 2.149) | 2.583 | 1.723 |
| Same, High (unchanged default) | 2.065 | 2.065 |

The door room's floor on Low rose with `recent_weight` 3. It reads the same at 100, 200, 400 and 800 frames at either weight (0.0115
and 0.00706). The room tests' quality opts into the static-screen cache, which holds a view's composite from about its ninth frame,
so these are early readings and the settled value is not measured yet; `dynamic_gi` `rooms_settled` measures it with the cache off.

### 9.3 The freeze, the probes' sun shadow ray and the High layout (ADR-5001)

Three fixes from the J06 measurement (ADR-4948, `crates/regia_pt/tests/gi_acceptance.rs`):

1. The probe update freezes after 512 updates of a finest probe at the layout's divisor (at least 600 frames), not
   after 600 frames: the shipped divisors gave 37–75 updates and held a sealed box's start-up light (High 7.49 %).
   After the fix the sealed box reads 0.07–0.40 % on every tier at its own divisor (RTX 5090).
2. The probes' sun shadow ray skips the first two `eps` bands of its march (`sdf_trace`, any-hit mode), so a ray hit
   on a thick floor keeps its sun: the tunnel's 0.5 m floor reads 0.92 of a 0.1 m one's sun bounce (was 0.57).
3. High and Ultra space their finest probes 1 m apart (× 3.8 per cascade) with the § 9.2 options. With fix 2 alone
   the 2 m layout read a door room at 1.51 × the path tracer; the 1 m layout reads it at 1.29 × and improves every
   `gi_acceptance` scene (mean FLIP trench 0.104, color room 0.152, door room 0.271, tunnel 0.147, shaft 0.187).

Still open: no plate meets the `specs/27` § 2.2 mean-FLIP bar (0.08); the door room reads over and the tunnel under
on High; Low and Medium keep 2 m (the door room reads 1.49 × with fix 2). A sealed 6 m room's ceiling reads about 2 % on
High's 1 m layout (its finest cascade ends 4.75 m over the eye; the 2 m layout with the § 9.2 options read 0.04 %), and
Low's light 60 frames after a fast stop misses the `ddgi_temporal` bar by 0.13–0.30 luma levels because the fixed probes
carry more sun bounce (ADR-5001).

### 9.4 A finest cascade that reaches a 6 m ceiling, and a converged reference (ADR-5290)

- The 6 m room's ceiling read the 3.8 m cascade alone because the visibility blend acts only inside the finest
  cascade's blend window, which ended 0.65 cells under it. `DdgiSettings::finest_headroom_m` sets how far over the eye
  the finest cascade holds (default 2 m); High and Ultra take 12 layers and 3.5 m (§ 2): ceiling 0.0002 % (RTX 5090).
- Low's fast stop: the stop recovers (0.84 luma levels from a field held at the stop's own position); the test's
  remainder is the probe field differing along the corridor (+1.91 at 48 m from its centre, +2.71 at 72 m, with Low's
  two finest cascades alone too). The test is unchanged; its cause was the brick baker's precision (§ 9.5).
- The `gi_acceptance` reference traces 32 bounces: at 6 it missed 9.8 % of the door room's converged indirect light
  (tunnel 5.3 %, color room 2.4 %), which the probes carry. Against the 32-bounce reference the remaining error is
  the probe irradiance itself (door room 1.18 ×, tunnel 0.80 × on High); SSAO, the screen-space pass, the ambient
  cube and the update noise each move a scene's mean FLIP by 0.015 or less, the tracer's own noise is 0.03–0.06.

### 9.5 Exact distance bricks and surface probes (ADR-5338)

- A mesh's brick has a sample plane on each face of its bounds (one border sample beyond each), so a box is exact in
  its brick, edges included, and plates that touch stay closed at their joint. Before, the faces lay halfway between
  samples and a coarse axis shaved a plate's edges by up to half a voxel: the tunnel's roof/wall joints were open slits
  the probes saw the sunlit outside through (the seam tears of ADR-4948 finding G), and walls floated over floors.
- The baker's closest-point search runs in f64: in f32 it was metres off on long two-triangle faces (a 240 m corridor
  wall), differently at each voxel column, and the probe field changed with where the camera stood (the Low fast
  stop, § 9.4). The sign at an edge comes from the tied triangle the offset is most nearly normal to.
- A probe whose nearest back face lies within 0.02 spacing (on a surface) relocates along the mean of those rays,
  not the first one this frame's ray rotation lists, so its offset does not depend on the camera's path.
- Results (RTX 5090): `ddgi_temporal` passes on every tier (Low fast stop 0.72 against 2.14); door room High 0.168
  mean FLIP, fine 0.087. The tunnel and shaft read their true sun-bounce deficit (tunnel High 0.73 × the tracer's
  indirect energy, sun only 0.72 ×, sky only 0.93 ×), which the leaks used to hide. It is not resolution: a 0.5 m
  layout reads 0.60 × sun only.

### 9.6 The lookup at the surface point, rays past the cascade reach, exact axis lobes (ADR-5374)

- **Lookup.** On every tier (it rides `DdgiSettings::visibility_blend`) the shading lookup and the probes' own ray
  read choose the cell and the trilinear weights at the surface point. The normal and view bias (0.1 and 0.3
  spacings) move only the point of the visibility test, as Majercik et al. 2019 list it. Weighted at the biased
  point, a surface took part of its light from the probe layer in front of it, each bounce again, and the light
  followed the view direction. A layout without the option (`DdgiSettings::default()`, the validation plates)
  keeps the weights at the biased point: on a surface that lies on a probe plane those average two probe layers
  and show a quarter less of the probes' update noise, which the unseeded lab layout's walk bar
  (`tests/ddgi_temporal.rs`) is sized for.
- **Rays.** A probe ray marches its own cascade's instance list out to that cascade's reach (spacing × the largest
  probe count per axis) and then each coarser cascade's list over the stretch past the one before; a hit's sun
  shadow ray starts in the list that held the hit. A ray used to read the sky past its own reach (20 m on High), so
  the far sunlit ground a tunnel or a doorway is lit by was missing, more of it the finer the layout.
- **Irradiance tiles.** On every tier (the same option) the 16 texels next to a coordinate axis integrate their
  cosine lobe about the axis, so the lookup toward an axis (all the shading lookup reads, through its ambient cube)
  is exact. The interpolated lobes are tilted 19.5° or 11.3° off the axis and see, from a probe on a surface, that
  surface's own sunlit radiance, which the surface-point weights would read undiluted. A layout without the option
  keeps every texel on its own direction.
- Results and what is left: ADR-5374 (`gi_acceptance` `acceptance_scenes`, `term_table`).

### 9.7 Probe noise, and the light at a surface behind a probe (ADR-5457)

- **Ray sets and rotations (measured, unchanged).** Every probe traces the same spherical Fibonacci set, turned by
  one independent rotation a frame. A per-probe orientation lowered a held view's frame-to-frame change by 4 % but
  made the probes along an invariant corridor differ, so a walking view strayed from its held image (Low tier
  1.13 → 1.91 luma levels, bar 2.11). Quasi-random rotation sequences fail at the strides a settled probe takes
  them at (every 8th, 16th, ... frame): an additive recurrence's every-8th points cover the cube worse than
  independent numbers, and a Halton coordinate climbs in even steps where the stride is one more than a multiple
  of its base (8 and 7, 16 and 5).
- **Shading copy.** Once a probe's history runs at its settled rate, the copy the lookups read (one RGB9E5 word a
  texel) follows it through a second filter that keeps half of its own last value. The history's step of
  (1 − hysteresis) × this frame's error reaches the frame spread over the next updates: about a third less change
  from frame to frame (Desktop High door room 0.0101 → 0.0065) for one update of lag. A younger probe's copy is its
  history (the plain mean of its updates).
- **Update count.** A probe that leaves geometry restarts its update count with its history. The count ran on
  before, which gave the restart's rays, the probe's least converged, the weight of every update spent inside: a
  floor or a wall on a probe plane puts a whole layer of probes there for their first trace.
- **Surface irradiance** (tier layouts, with `DdgiSettings::visibility_blend`). Of the irradiance texels that hold
  an axis's lobe (§ 9.6), those of odd parity hold the irradiance at the surface that lies behind the probe along
  that axis, between 0.15 and 1.05 spacings away and flat across the probe's distance-moment lobe. The blend
  estimates it from the same 64 rays: each hit is weighed by the cosine at the surface point and by its solid angle
  from there over its solid angle from the probe (the squared distances and how the hit's surface faces either
  point; the hit's normal rides in the ray buffer, in the six lowest mantissa bits of each radiance channel), at
  most eight times a ray's own weight. A probe's texels lean on the surface from its 16th update, in full from its
  32nd: its distance moments are still filling before. The surface point keeps a quarter of a spacing from any
  other surface beside the probe. The shading copy of the tile's corner texel holds the six distances (five bits
  each), and a lookup reads,
  per ambient-cube face, the probe's irradiance for a point at the probe's plane, the surface's for a point on
  that surface, and their mix in between (`ddgi_axis_irradiance`). The probes' own ray read takes the same faces,
  so a hit and the frame agree. Before this a wall between two probe planes read the irradiance half a cell in
  front of it: a tunnel's walls, lit at a grazing angle through its mouths, read short (the wall holds 1.09 times
  the light of the probe half a metre before it in `tests/ddgi.rs`'s tunnel), and each bounce read short again.
- Not changed, with the measurements in ADR-5457: the tier lookup stays on tier layouts only (with it on every
  layout the radiance cache's gather falls behind the plain gather on the trench plate), the probes' hits stay
  Lambertian (the tracer's rough dielectrics carry 5 % more light down the tunnel), and the Low tier's 2 m probes
  still interpolate across a doorway.

### 9.8 One lookup on every layout, lightness blending, and hits that reflect as the forward pass (ADR-5506)

- **Every layout has the lookup of § 9.6 and § 9.7** (`ddgi_tier_lookup` is true): the probes weighted at the
  surface point, exact axis lobes, the light at the surface behind a probe. `DdgiSettings::visibility_blend` keeps
  only § 9.2's blend between cascades. The radiance cache's cells and ReSTIR GI's probe read compose the same
  functions, so they follow. The cached gather needed no change (its red test in ADR-5457 came from a variant
  build that left the cache's copy of the lookup on the old one).
- **A new probe takes sixteen updates at one a frame** (`DDGI_YOUNG_UPDATES`; eight before), the count its distance
  moments take to fill (§ 9.7). At the surface point a floor on a probe plane reads four probes where the biased
  point read eight, so a walk showed the newest probes' error a quarter more (the invariant corridor 2.43 luma
  levels against 1.91; 1.94 with sixteen updates and the rest of this section). A settled field traces as before.
- **Each ambient-cube face is interpolated between the probes as its cube root** (`ddgi_blend_root`: summed as
  cube roots, cubed after the normalization; CIE 1976 lightness is a cube root, and Majercik et al. 2019 blend
  at a gamma of 2). Irradiance falls off faster than linearly from a small or near bright surface, so a linear
  blend reads too much between a probe beside it and the next one, and every bounce reads the excess again.
  Between probes of like light nothing changes (0.4 % at a ratio of 0.8); halfway between a lit and an unlit
  probe the blend reads an eighth of the lit one. The cascade blend, the screen lookup's 2 × 2 gather and the
  temporal blend stay linear.
- **`relocate` pushes a probe off a near front face along the mean of its near rays**, each weighed by how far
  inside `min_frontface` it hit (the surface's normal over a face). Along the single nearest ray a probe also slid
  one or two centimetres along the surface, in a direction that followed the frame's ray rotation.
- **A probe ray's hit is where the ray crosses the surface** (`sdf_trace`): the first sample inside the brick's
  surface, moved back to the crossing by the two distances on either side of it. The march used to stop within
  half a voxel of any surface, which also stopped rays passing an edge, so every opening read half a voxel
  narrower on each side (a 0.4 m roof slit let in 0.909 of its light; 1.008 now). A shadow ray keeps the band and
  its own-surface rule (§ 9.3).
- **A hit reflects as the forward pass's dielectric.** `SdfInstanceGpu` carries the material's perceptual
  roughness (`albedo.w`, at least 0.05; 0 marks a hit without a material model, the terrain) and metallic factor
  (`emissive.w`). `hit_brdf_pi` reflects the sun and the fill light through `pbr_brdf.wgsl`'s lobes (Lambert under
  1 − F(l·h), GGX with the height-correlated Smith term, Schlick at F0 0.04, roughness at least 0.45);
  `hit_reflectance` reflects the probes' irradiance through their directional albedo under even light:
  albedo × (0.96 − 0.96 × 0.02717 (1 − μ)^2.5) + `hit_specular_albedo(μ, roughness)`, μ = n·v, the latter a cubic
  in 1 − μ whose coefficients are cubics in the roughness (least squares over roughness 0.45 to 1 and μ 0.05 to 1,
  within 0.0032 of the integral). The metallic share of a hit keeps the plain albedo. A roughness or metallic map
  is not read (the factors stand for the surface).
- Measured on an RTX 5080 (Desktop High, mean FLIP, bar 0.08): trench 0.098 → 0.070, tunnel 0.097 → 0.075, door
  room 0.076 → 0.066, shaft 0.102 → 0.088, color room 0.111 → 0.092; Low door room 0.319 → 0.259. Probe update
  at 1440p 0.485 ms (bar 0.6). The remaining error is listed in ADR-5506.

### 9.9 Crease texels (ADR-5552)

- **A probe holds the irradiance at the crease points of its faces.** § 9.7's surface texel is the light at one
  point of the surface behind a probe, kept a quarter spacing from the surfaces beside it. Between that point and
  the next surface the light changes fastest (the surface beside it fills up to half the hemisphere, and the
  strip next to the crease is what counts), and the lookup read that whole stretch as the one point. The tile's
  border ring (24 of its 28 texels; octahedral border copies no lookup has read since § 9.8) now holds, per face
  and per side across it, the irradiance at the crease point: on the face's surface, 0.08 spacings
  (`DDGI_CREASE_CLEARANCE`) from the flat surface the distance moments hold toward that side
  (`ddgi_crease_texel`, `ddgi_blend.wgsl` `crease_point`).
- **The estimate** re-weighs the probe's 64 rays for the crease point as § 9.7 does (`surface_weight`, gain cap
  8). A hit past the plane of the surface beside the point is hidden from it and does not count, nor does the sky
  on that side. The sum is divided by its own weights: the strip beside the point is one or two rays wide and the
  cap binds on it, so the weights do not sum to the hemisphere (divided by the probe's cosine sum every crease
  lost light). With no surface on that side, with the probe nearer to it than the clearance, or under 16 updates,
  the texel holds the face's own light.
- **The surface word lists the surfaces a probe stands on** (nearer than 0.15 spacings) besides the ones its odd
  texels lean on. The odd texel of such a direction is the probe's own lobe, so the listing changes no lookup.
- **The lookup** (`ddgi_axis_irradiance`, `ddgi_crease_gain`): on a face's surface the light is scaled by crease /
  surface (per channel, within 1/8 to 8) at the crease point and nearer, by 1 from the face's surface point on
  (the point a quarter spacing from the surface beside it where the texel leans on a surface, the probe's own
  distance from it where the probe stands on the face's surface), in proportion between. A seed (§ 9.2) gives a
  crease texel its face's light.
- **Which read takes which crease.** The screen lookup the frame shades with takes every crease. A read that
  feeds a bounce (the probes' own ray read, the radiance cache's cells, ReSTIR GI's probe read:
  `ddgi_sample_cascade`) takes a crease toward a surface the probe stands on (nearer than 0.18 spacings,
  `DDGI_CREASE_STANDING`) in full, and one toward a farther surface only where it adds light. A far crease is
  estimated across the cell: few rays reach the strip beside it and the cap binds on them, so the estimate leans
  on the far field; taking light away on it in the bounce cost the scenes lit along their walls (a tunnel 1.4 %
  of its light, a shaft 1.0 %), and every bounce reads an under-read again. The frame and the bounce so differ
  at a far crease that darkens.
- No setting and no memory: the tile keeps 64 texels. `DdgiReadback::crease_irradiance(slot, face, side)` reads
  a crease texel.
- A crease toward a surface the probe stands on is placed by `DdgiSettings::min_frontface` (the distance a probe
  pushed off a surface stands at), not by the surface word's step, which rounds just there (0.12 spacings are 3.5
  steps).
- Measured on an RTX 5080 (mean FLIP / p95; bars 0.08 / 0.20). Desktop High: shaft 0.0876 / 0.234 → 0.0794 /
  0.194, tunnel 0.0746 → 0.0699, trench 0.0700 → 0.0705, door room 0.0664 → 0.0668, color room 0.0924 / 0.212 →
  0.0911 / 0.195: four of five under the mean bar, all five under the p95 bar. Low: door room 0.259 → 0.240,
  color room 0.227 → 0.181, trench 0.106 → 0.087, shaft 0.185 → 0.169, tunnel 0.100 → 0.100. The validation
  plates' own finer layouts read worse in two scenes (color room at 0.4 m 0.077 → 0.084, shaft at 0.75 m 0.078 →
  0.080): there the clearance is 3 to 6 cm, near the bricks' voxel. Probe update at 1440p 0.555 ms (0.487
  before; bar 0.6). What is left is listed in ADR-5552.

### 9.10 Flat meshes, front weights on every tier, and creases across a cell (ADR-5716)

- **A mesh without thickness bakes a finite brick.** The baker's closest-point search (`ddgi_bake.rs`
  `TriGrid::closest`) starts from the grid's nearest cell when a sample lies outside the grid. A flat mesh (a
  studio ground quad, a foliage card) has a grid 0.1 mm thick and a brick 1 mm thick: its samples more than three
  cells before the plane found no cell and held +∞, so a probe ray from the mesh's front left the brick with one
  step. Lavapipe's probes did not see such a mesh from its front at all; an NVIDIA card's saw it along steep rays
  only (the non-finite sample makes the march crawl by its least step). Both now trace it: 30 of a probe's 32
  downward rays stop on a ground quad under it (the rest pass within a few degrees of the horizon).
- **Front weights on every tier** (§ 2.1's first term). On 1 m probes the probe just inside a room's open front
  stands behind the plane of the room's wall ends and lit them with the wall's sunlit inner side (a color room's
  end faces 5 to 8 % bright). The second term stays with the 2 m tiers as `edge_visibility`: on 1 m probes it
  changed no acceptance scene and costs a word per probe in the lookup.
- **A crease belongs to the probe row nearest its surface** (`ddgi_blend.wgsl` `crease_point`): where the probe
  one cell toward the surface is active and stands before it, this probe holds no crease toward it (its texel is
  the face's own light). A floor on a probe plane has a row standing on it and a row a cell above that sees it
  within reach; both held the wall's crease toward the floor, and a wall point between them took that gradient
  twice.
- **A crease across the cell is weighed in two halves** (`blend_irradiance`). Toward a surface farther than a
  quarter spacing (`DDGI_CREASE_BESIDE`) the crease point's rays that end on that surface and the rest are
  averaged apart and count half each: the surface fills half of the point's cosine-weighted view whatever the
  clearance, while in § 9.9's one mean the few rays that end on its strip counted for a fraction of that. A crease
  toward a nearer surface keeps the one mean.
- **Which read takes which crease** (replaces § 9.9's bound of 0.18 spacings). A read that feeds a bounce takes a
  crease in full from a probe within a quarter spacing of the surface beside it and only where it adds light
  past that. `DDGI_CREASE_STANDING` keeps placing a surface the probe was pushed off.
- **No crease scaling under 5 cm of clearance** (`DDGI_CREASE_LEAST`; probe spacings under 0.625 m): the plates'
  0.4 m layouts read as before § 9.9 again.
- **The image light's ground plane is in the probes' world.** A probe ray that meets no surface and lands on the
  plane reads the plane's radiance (`ddgi_trace.wgsl` `miss_radiance`); the forward pass no longer swaps the plane
  into the GPU probes' irradiance (it still does for a baked volume's lookup). The plane's numbers ride spare
  components of the probe uniform (400 bytes as before); setting, moving or removing the plane is a light cut.
  A ray that starts on the plane or under it lands straight under its origin: the probe layer at the plane's own
  height carries most of the weight of every point in the first cell over the plane, and with the image's lower
  half in its downward rays the plane did not reach the underside of anything standing on it.
- **The probes read an adopted foliage material's maps** (`ddgi.rs` `map_texels`). `FoliageSway::adopt` moves a
  plant's maps into its graph's slots; the probes took the base colour and emissive means from the standard
  slots alone, so a swaying plant in the frame's mesh list reflected its base colour factor (white for most
  plants) while the same plant in an instance set, drawn under its plain twin, reflected its map's mean. Both
  now read the same texels, as ADR-5457's rule for instance sets asks (§ 9.7; the visibility buffer's wind pair).
- Measured on an RTX 5090 and an RTX 5080, which agree to four digits (clean build; mean FLIP / p95; bars 0.08 /
  0.20). Desktop High: trench 0.0705 → 0.0666, color room 0.0911 / 0.195 → 0.0787 / 0.180, door room 0.0668 →
  0.0661, tunnel 0.0699 → 0.0745, shaft 0.0794 → 0.0743: all five under both bars, at the tier's own update divisor
  too (0.0662, 0.0792, 0.0620, 0.0758, 0.0740). Low: shaft 0.167 → 0.144, color room 0.181 → 0.165, trench 0.085 →
  0.084, door room and tunnel unchanged (0.196, 0.095). Worse: the High tunnel (the row rule takes away two creases
  that brightened its walls) and the Low color room's p95 (0.344 → 0.363). Cost at 1440p on the RTX 5080: 0.557 ms
  (0.555; bar 0.6). The visibility buffer's wind pair reads 100 % on lavapipe and 99.98 % or more on both cards;
  `studio_measure` reads the same far sides on the three devices. What is left is listed in ADR-5716.

### 9.11 The screen lookup's normal at the frame's border and at a crease; the Low layout measured (ADR-5796)

- **A lookup texel's normal** (`ddgi_screen.wgsl` `screen_normal`) is `ddgi_depth_normal`'s, the shorter depth
  difference on each axis, with two corrections. A neighbor outside the frame is a zero difference by
  construction: it used to be the texel's own pixel read through a clamp and subtracted from itself, which is 0
  on lavapipe and a rounding remainder on an NVIDIA card, where the texels of the frame's first row and column
  then held normals that pointed anywhere (on the Low tier single texels read the probes over the roof: dashes
  of daylight along the frame's top and left edge). And where one of the two differences the rule left stands
  more than `SCREEN_PLANAR` (0.05, a sine) off the plane of the two it took, the pair is tested against the
  diagonal pixel between its neighbors (`screen_quadrant`): a diagonal off their plane means the pair straddles
  a crease or an edge, its cross product points along the crease, and the quadrant whose diagonal lies nearest
  its own plane gives the normal instead (if it is at least twice as flat). With the mixed pair no probe stood
  behind the normal's plane, so the probes behind the roof or the wall passed § 2.1's front weight: dots along
  the creases near the eye on the Low tier. A texel on a flat surface keeps its normal bit for bit and reads no
  more depth than before; one beside a crease or an edge reads three more texels, one whose pair straddles it
  fifteen.
- **The Low and Medium layouts stay 2 m × 3.** Measured (`gi_acceptance` configs `low_15m`, `low_15m_wide`,
  `low_17m`, `low_1m`; RTX 5090, mean FLIP; 2 m / 1.5 m / 1.7 m / 1 m on Low's 12 × 8 × 12 grid): door room 0.196 /
  0.120 / 0.294 / 0.441, color room 0.165 / 0.197 / 0.168 / 0.081, tunnel 0.095 / 0.120 / 0.124 / 0.116, shaft
  0.144 / 0.128 / 0.174 / 0.121, trench 0.084 / 0.084 / 0.100 / 0.113. A finer spacing on the same grid moves the
  error from one scene to another: which scene reads well is where the grid falls on its walls (1.5 m probes
  stand on the walls of a 6 m room and a 3 m shaft; the 2.4 m color room holds two probes at 2 m and at 1.5 m).
- Measured and kept (ADR-5796 finding 3): the bounce read's crease rule (§ 9.10), the surface gain cap of 8
  (§ 9.7) and the cube-root blend (§ 9.8). Each alternative moves error between the acceptance scenes.
- Measured with the two corrections (clean build on an RTX 5090; an RTX 5080 reads the same four digits; mean
  FLIP, trench, color room, door room, tunnel, shaft): Desktop High 0.0665, 0.0784, 0.0659, 0.0742, 0.0743 (p95
  0.158, 0.179, 0.127, 0.131, 0.172: all under both bars); Low 0.0837, 0.1633, 0.1949, 0.0940, 0.1443; Medium
  0.0744, 0.1633, 0.1950, 0.0890, 0.1445. Probe update at 1440p on the RTX 5080: 0.556 to 0.559 ms (0.555 to
  0.556 with the old shader; bar 0.6).

### 9.12 Light through an opening is weighed from the opening (ADR-5909)

- **The fault.** A probe's surface texels (§ 9.7) and crease texels (§ 9.9) hold the light at a point up to 1.05
  spacings from the probe, estimated from the probe's own rays: a hit is weighed for that point by where it lies
  (`ddgi_blend.wgsl` `surface_weight`), a miss by its direction. That is exact while the point sees what the probe
  saw. Light that reached the probe past the edge of a nearer surface (through a doorway or a window, out of a
  tunnel's mouth, over a trench's rim) was weighed by the ground or the sky far behind the opening, which keeps the
  solid angle the probe saw the opening under. A wall beside the probe sees a door at a flatter angle and from
  further off; a tunnel's or a shaft's wall sees more of the mouth than the probes on the centre line do. On 2 m
  probes, where the walls of a 6 m room stand a metre from the probe planes, the door room read 1.13 times the path
  tracer's indirect light (0.97 with its walls moved onto the probe planes, `REGIA_GI_SHIFT`), and the tunnel and
  the shaft 0.95 and 0.94 (0.96 and 0.975 on 1 m probes). An oracle with that room's exact geometry (the door's
  light weighed by the door's true cosine-weighted solid angle from each point) takes the Low door room from 0.195
  to 0.123.
- **The rule** (`ddgi_blend.wgsl` `opening`). A ray passed such an edge where it crosses the plane of another
  ray's hit from the front, at most three quarters of the way to its own hit (`DDGI_OPENING_FAR`; a miss has no
  hit), and that other ray lies within 35° of it (`DDGI_OPENING_CONE`; the 64 rays stand about 25° apart). The
  surface in that plane ends between the two rays. For the probe's surface and crease texels the ray's light is
  then weighed from where it crosses the nearest such plane, as a patch there: the opening as an area source,
  which carries its own solid angle to the point. Where the ray meets the plane steeply (the cosine over
  `DDGI_OPENING_STEEP`, 0.6) the patch lies in the plane: a hole in a wall before the probe, for which the transfer
  is exact. Where it grazes the plane (under `DDGI_OPENING_GRAZE`, 0.25: a tunnel's wall, past its mouth) the edge
  it passed lies somewhere before the crossing and the patch faces the probe, which keeps its solid angle and moves
  its direction alone; in proportion between. A patch of the wall's plane at a grazing crossing over-lit the Low
  tunnel (1.09 times the tracer), and a patch that faces the probe everywhere kept less of the door room's gain
  (0.159 for 0.143).
- **What is unchanged.** The probe's own lobes (the even axis texels), the distance moments, the lookups, the ray
  count and every buffer. One ray an invocation of the irradiance blend finds its opening (64 plane tests) before
  the texels are weighed, shared through workgroup memory together with the decoded hit normals. Every tier.
- **The finest cascade's blend margin** (`ddgi_sample.wgsl` `ddgi_blend_weight`). A cascade takes none of its turn
  within a margin of its blend window's end and all of it a cell further in. The margin is 0.75 cells; for the
  finest cascade of a layout that needs the room rule (§ 9.1: every tier) it is a quarter cell
  (`DDGI_ROOM_MARGIN`). The ramp is the same cell wide, so the blend moves no faster under a moving eye, and the
  volumes stand where they stood: the lookup holds half a cell more of the finest probes at each end of each axis
  (`DynamicGiBudget::finest_above_m` and `finest_below_m` report the wider window). Past the window a point reads a
  cascade three or four times as coarse. The view up the acceptance shaft read the 6 m cascade's two probes from
  4.5 m over the eye on Low: with the finest cascade holding its whole volume (a diagnostic, it pops on a scroll)
  the Low shaft reads 0.099 for 0.140, with a half-cell ramp behind the quarter-cell margin 0.110, with 3.2 m of
  headroom in place of 2 m (and 1.8 m under the eye in place of 3 m) 0.108. The quarter-cell margin alone reads
  0.117 and changes no slope and no placement. Test layouts and the validation plates (no room rule) keep their
  window.
- **Low and Medium are placed for 3 m of headroom** (`DynamicGiTier::apply`, `finest_headroom_m` 3 for 2). With
  the quarter-cell margin the 2 m placement gave the lookup 3 m over the eye and 4 m under it; placed a metre
  higher it holds 4 m over the eye and the 3 m under it that every tier held before ADR-5909. The Low shaft reads
  0.102 for 0.117, and the trench, the color room, the door room and the tunnel read the same four digits (their
  ceilings lie within either window); 4 m of headroom reads the shaft 0.100 and leaves 2 m under the eye.
- **Measured, the opening rule alone** (RTX 5080; mean FLIP, trench, color room, door room, tunnel, shaft): Low
  0.0837, 0.1633, 0.1949, 0.0940, 0.1443 → 0.0778, 0.1577, 0.1432, 0.0743, 0.1401; High 0.0665, 0.0784, 0.0659,
  0.0742, 0.0743 → 0.0652, 0.0785, 0.0653, 0.0637, 0.0680.
- **Measured, with the quarter-cell margin** (clean build, RTX 5080; an RTX 5090 reads the same digits): Low
  0.0760, 0.1577, 0.1361, 0.0704, 0.1169 (the trench and the tunnel pass both bars); Medium 0.0704, 0.1577, 0.1358,
  0.0680, 0.1169; High 0.0646, 0.0785, 0.0653, 0.0633, 0.0653. At the tier's own divisor: High 0.0645, 0.0795,
  0.0619, 0.0681, 0.0665. Update noise unchanged. Probe update on the RTX 5080: 1440p High 0.560 → 0.572 ms (bar
  0.6), 800p Low 0.330 → 0.257 ms (fewer lookup texels read two cascades); on the Steam Deck at 800p Low 6.59 →
  4.44 ms (its first timing; the 1.5 ms line of § 7 is a guess).
- **Measured, with 3 m of headroom on Low and Medium** (the shipped state; clean build, RTX 5080): Low 0.0760,
  0.1577, 0.1361, 0.0704, **0.1015**; High as above.
- **What the Low tier still misses by** (door room 0.136, color room 0.158, shaft 0.102) is the room's own
  structure on one probe column or three probe planes: the sun's edge on a floor or a wall crosses a crease
  between two probes and the crease holds one value for the cell. The reference's own noise (two seeds at 1024
  spp) is 0.031, 0.039, 0.056, 0.061, 0.039 on the five scenes. ADR-5909 has the experiments (the screen-space
  gather, the ambient occlusion and the update rate ruled out; the lattice moved against the scene).

### 9.13 The Low tier's remaining miss is the 2 m lattice; the dense candidate (ADR-5938, measurement)

Nothing in this section changes a shader, a setting or a tier. It records what was measured after § 9.12.

- **Where the surfaces fall on the lattice decides a scene's Low score.** The committed Low layout with a scene
  moved against the probes (`REGIA_GI_SHIFT`; RTX 5080, mean FLIP, trench, color room, door room, tunnel, shaft):
  unmoved 0.0760, 0.1577, 0.1361, 0.0704, 0.1015; by (1, 0, 1) m 0.0762, 0.1181, 0.1053, 0.0838, 0.1151; by
  (0, 1, 0) m 0.1104, 0.1793, 0.1549, 0.0869, 0.0951; by (1, 1, 1) m 0.1240, 0.1822, 0.1238, 0.0831, 0.0822; by
  (0, 0.5, 0) m 0.1272, 0.1767, 0.1846, 0.0803, 0.1069. No phase passes all five.
- **Measured and not adopted** (shader variants on one binary; ADR-5938 has the numbers): four more surface points
  a face in the tile texels no lookup reads (the color room and the shaft gain 0.01, the trench, the door room and
  the tunnel lose 0.01 to 0.02); a twentieth of the weight for a probe that does not hold the point's surface (the
  door room 0.122 with its energy at 1.00, the tunnel 0.106); a probe taking the plane its neighbor holds (the dark
  halo round a door goes, the energy rises to 1.10). Ruled out as causes: the blend's faster step on a large change
  (the same digits without it), and the view dependence of a hit's radiance re-aimed at a surface point (0.004 of
  the Low door room's 0.06 of excess energy).
- **The dense candidate.** High's grid (1 m, × 3.8, 20 × 12 × 20) with Low's brick pool and lookup region, each
  settled probe re-traced every 64 frames: 9 000 rays a settled frame for Low's 8 640, 60.1 MB of probe buffers for
  14.4 MB. Frozen at its own divisor (RTX 5090): 0.0644, 0.0800, 0.0632, 0.0636, 0.0661 (p95 0.157, 0.180, 0.128,
  0.126, 0.168) where Low reads 0.0751, 0.1578, 0.1346, 0.0710, 0.1007 and High 0.0645, 0.0795, 0.0619, 0.0681,
  0.0665; the color room lies on its bar (the acceptance row marks 0.0800 a miss in the fifth digit). 300 frames
  after a cold start it reads 0.0749, 0.1113, 0.0925, 0.1365, 0.1333 for Low's 0.0814, 0.1932, 0.1859, 0.2446,
  0.1528; after 1 200 frames 0.0713, 0.0974, 0.0794, 0.1107, 0.0879 for 0.0736, 0.1584, 0.1459, 0.0901, 0.1005.
- **Under a moving camera** (`tests/ddgi_temporal.rs` `the_low_dense_candidate_under_a_moving_camera`, RTX 5090,
  luma levels): the walk against the held view reads 1.010 on Low, 1.203 on High and 1.382 on the candidate (bar
  2.113); 60 frames after a fast stop 0.615, 1.102 and 1.384 (bar 2.139). It holds the shipped tiers' bars.
- **Its cost.** Probe update, settled, 800p: RTX 5080 0.258 to 0.278 ms for Low's 0.254 to 0.261 ms; Steam Deck
  4.18 and 4.25 ms for Low's 4.19 to 4.72 ms (the lookup is 3.5 of it and does not depend on the probe count). The
  first 12 frames, every probe young: Steam Deck 61.7 and 62.0 ms a frame for Low's 22.9 ms; RTX 5080 1.46 ms for
  0.52 ms. Not adopted here: the memory, that start-up load and a four times slower settled update are a decision
  for the owner and the lead (ADR-5938 decision 3 lists what adoption takes).
- **How to repeat it.** `REGIA_GI_CONFIGS=low_dense_shipped,low_shipped,high_shipped` (and `low_dense_300`,
  `low_dense_1200`, `low_shipped_300`, `low_shipped_1200`) with `gi_acceptance` `term_table` or
  `acceptance_scenes`; `tier_cost` prints a `low_dense` row, and with `DYNAMIC_GI_COST_SETTLE=0
  DYNAMIC_GI_COST_FRAMES=12` the start-up load; `cargo test -p regia_render --test ddgi_temporal -- --ignored
  --nocapture the_low_dense_candidate` prints the walk and the fast stop.

**Addendum: a per-frame budget for young probes (ADR-7870).** `DdgiSettings::young_budget` (0 = off, every tier)
caps the young probes (fewer than sixteen updates) one probe update traces. Past it a young probe waits: the B
least of a unique key go first (distance from the eye in finest spacings, then in view, then the frames since its
last trace, then the slot), picked on the GPU by a four-digit radix select (`ddgi_trace.wgsl` `young_histogram`,
`young_select`; atomics only count, so the set does not depend on thread order). Settled probes keep their cadence.
A field that restarted settles within `⌊16 · probes / B⌋ + 16` frames (`DdgiSettings::young_settle_frames`), and the
update freeze waits for that. With the budget off the probe buffers are byte for byte those of the code before it
(lavapipe, 64 frames with a light cut and a scroll, a 288-probe field and High's 19 200).

- **Start-up and light-cut load on the Steam Deck** (800p, door room, `tier_cost` with `DYNAMIC_GI_COST_SETTLE=0`,
  worst frame of 16 or 32): Low as shipped 23.3 ms (mean of 16: 21.6 to 21.9 ms); the dense candidate 85 ms (mean
  56.3 ms); with a budget of 2 048 16.3 ms, 2 560 19.3 ms, **3 072 22.4 to 22.5 ms**, 3 584 25.5 ms, 4 096 29.5 ms.
  3 072 is the largest budget measured whose every start-up frame stays under Low's 22.9 ms; it settles a restart
  in at most 116 frames (Low: 16) and costs a settled frame 0.10 to 0.13 ms more (4.74 ms for the unbudgeted 4.61
  ms; Low 4.59 ms).
- **What it costs in the frames** (RTX 5090, `acceptance_scenes`, mean FLIP, trench, color room, door room, tunnel,
  shaft): the dense candidate 300 frames after a cold start 0.0749, 0.1113, 0.0925, 0.1365, 0.1333, with a budget
  of 3 072 0.0760, 0.1113, 0.0906, 0.1424, 0.1333; after 1 200 frames 0.0713, 0.0974, 0.0794, 0.1107, 0.0879 and
  0.0718, 0.0974, 0.0782, 0.1138, 0.0879 (Low: 0.0814, 0.1932, 0.1859, 0.2446, 0.1528 and 0.0736, 0.1584, 0.1459,
  0.0901, 0.1005); frozen (32 832 frames) 0.0644, 0.0800, 0.0632, 0.0636, 0.0661 without and 0.0645, 0.0800, 0.0632,
  0.0636, 0.0661 with the budget. The Steam Deck reads the 300 and 1 200-frame rows the same to 0.001. A scene whose pixels and bounces read only the probes
  near the eye (the color room, the shaft) reads the same digits with the budget.
- **What the dense candidate's 60.1 MB are**: per probe 3 132 B, of which the distance moments 1 024 B (19.66 MB), the
  rays 1 024 B (19.66 MB), the irradiance history 768 B (14.75 MB), the packed irradiance 256 B (4.92 MB), the record
  48 B, the header 8 B and the queue word 4 B. The rays are read only in the frame that traced them; a ray scratch of
  B + 142 rows (the most settled probes due in a frame) would hold 3.29 MB at B = 3 072, 43.8 MB in all. Not built
  (ADR-7870 decision 8 has what it touches).
- **How to repeat it**: `DYNAMIC_GI_COST_YOUNG_BUDGET=2048,3072,4096 DYNAMIC_GI_COST_LAYOUTS=low,low_dense,low_dense_young3072
  DYNAMIC_GI_COST_SIZES=800p DYNAMIC_GI_COST_SETTLE=0 DYNAMIC_GI_COST_FRAMES=16` with `tier_cost`; the FLIP rows with
  `REGIA_GI_CONFIGS=low_dense_young_300,low_dense_young_1200,low_dense_young_shipped` (`REGIA_GI_YOUNG_BUDGET`,
  default 3 072) and `acceptance_scenes` or `term_table`.

## 10. Platform notes

- **Linux, Windows:** the probes run under Depth on every desktop tier; the tier picks the size.
- **Steam Deck:** the Deck preset's tier is Low. Its default lighting option stays Balanced (ADR-133) until the
  Deck measurement shows the Low tier inside its budget; a player can pick Depth today.
- **Android aarch64:** builds; the probes never run (`GiLook::depth_available`), the tier is carried but unused.
- **macOS (testing only):** as Linux.

Sources: Majercik, Guertin, Nowrouzezahrai, McGuire, "Dynamic Diffuse Global Illumination with Ray-Traced
Irradiance Fields", JCGT 8(2) 2019 (probe grid, distance moments, Chebyshev visibility); Majercik, Marrs,
Spjut, McGuire, "Scaling Probe-Based Real-Time Dynamic Global Illumination for Production", JCGT 10(2) 2021
(cascaded scrolling volumes, per-volume budgets, update rates and probe counts as the scaling levers);
Hart, "Sphere Tracing", 1996 (brick traversal).

## 11. Out of scope (stage 2a phase 2 and later)

- **Radiance cache for the screen-space pass** (`specs/27` stage 2a's third item): built, default off, § 12.
- Turning Depth on by default on the Deck (an owner decision after the Deck measurement).
- ~~A per-tier screen lookup region~~: built, § 7 (ADR-2605).
- ReSTIR GI on hardware ray queries (stage 2b).

## 12. The radiance cache behind screen-space GI (ADR-2085)

Stage 2a's third item. A world-space cache of the probes' light, which a depth-buffer ray march reads on its
misses. Off in every preset (`RenderProfile::radiance_cache`), so a default frame is unchanged: the plain gather
still compiles from the unchanged text of `ssgi.wgsl`, apart from one clamp in `fs_upsample` that a non-negative
gather never reaches. The cached gather is `ssgi_cached.wgsl`, composed after it.

**The cache** (`radiance_cache.rs`, `radiance_cache_common.wgsl`). `levels` nested camera-centered grids of `dims`
cells, addressed toroidally like the probes, `spacing_scale` × the finest probe spacing apart at the finest level and
`level_ratio` times coarser per level. A cell holds the radiance arriving at its center as an ambient cube rebuilt from the
probes' irradiance (below; RGB9E5, no exposure) and a tag: its world cell, valid only when the probes covered it. A slot whose
tag names another cell reads as absent.

**The budget** (`CachePlanner`, `radiance_cache_update.wgsl`). The CPU lists at most `budget_cells` cells a frame:
cells a scroll brought in first, oldest first, then a round-robin refresh giving half of what is left to the finest level
still due, half of the rest to the next. While the probes move or converge the refresh runs on; once they settle, one
full pass per level and the cache rests (`idle`). A jump past a whole grid restarts that level's refresh instead of
listing every cell. A camera cut, or a new probe spacing, clears the cells. The list keeps its capacity: a steady plan
allocates nothing.

**The gather** (`ssgi_cached.wgsl`). 8 cosine-weighted rays of `ray_scale` × the finest probe spacing (6 m on the tiers), 8
steps each. The forward pass already holds the probes' light at the pixel, so a ray counts only against what the cache
holds toward its direction there (`co`), scaled by the nominal receiver albedo:

| The ray | Counts |
|---|---|
| hits a surface on screen | fade × (its lit color − `co`) |
| hits an emitter a fitted area light carries | nothing |
| misses, with `miss_cap` 0 (the default, ADR-2605) | nothing |
| misses with `miss_cap` > 0, having reached `x` in known free space | min(C(x, ω), `miss_cap` × `co_free`) − `co_free` |
| misses with no valid cell at `x` | nothing |

`co_free` is `co` without the cells within ¼ spacing of the receiver's plane, and C(x, ω) cannot pick those cells either. A cell on a surface reads
brighter through the radiance inversion than one above it, so a miss that compared against `co` would darken the pixel by that step and not by
anything along the ray (measured in ADR-2085). The sum over the rays is added to the pixel; `darken_limit` (0 by default) bounds how far it may go below zero, as a share
of the probe light the pixel holds. So by default the pass only adds what exceeds the probes' light, where the plain gather
adds every hit in full. A pixel outside the cache, or with no valid cell on its side of its own plane, is left alone.

**Radiance, not irradiance.** The probes hold light already cosine-filtered around each face. A cell stores the ambient
cube B with L(ω) = Σ ω_a² B(sign ω_a) whose cosine integral gives back the probes' irradiance E at the six axes:
E(+a) = ⅜ B(+a) − ⅛ B(−a) + ⅛ S with S = Σ B = Σ E, which inverts to B(+a) = 3 E(+a) + E(−a) − S / 2. Uniform light comes
back unchanged. A negative face is clamped to none.

**Keeping light out of a closed room.** A miss reads only where the ray last stood in known free space: it stops at the
screen's edge and where it passes behind the depth buffer, so the read is on the camera's side of every visible wall.
The read is capped at a multiple of what the cache holds at the pixel, and neither read takes a cell behind the receiver's
plane. A cell that does reach through a wall inflates `co`, which only subtracts.

**Where it runs.** The main view of a frame that runs both the probes and screen-space GI, at full or half resolution (the half
gather stores the difference alone and the upsample adds it, as the plain one). Texture views, XR eyes (both alike, so an XR session
creates no cache), probe captures and baked frames take the plain gather. Android never runs the probes, so never this. An
`Rg11b10Ufloat` half target cannot hold a negative, so `darken_limit` has no effect at half resolution there.

**Agent protocol.** `render.get_radiance_cache`, `render.set_radiance_cache` (`book/agents/radiance-cache.md`).

**The gather's cost (ADR-2605).** `RadianceCacheSettings::rays` (1, 2, 4 or 8) rays a pixel a frame take turns through the
eight-ray pattern by the temporal index, so temporal AA sums the whole pattern; `steps` (2 to 8) steps a ray;
`sky_early_out` stops a ray at the first sky pixel and reads the cache there. `RadianceCacheSettings::economy()` is 4 rays of
6 steps with the sky stop: at most 24 depth taps a pixel to the default's 64. The defaults are unchanged. A change of these,
or of anything else that only says how the gather reads the cells, keeps the cells; a new layout or budget empties them. With
`count_work` the gather counts pixels, rays, steps and cell reads on the GPU (`RadianceCacheStatus::work`, a few frames late,
never a count of a frame that gathered nothing; `render.set_radiance_cache {count_work}`, `renderer.work`), which is how
`tests/radiance_cache.rs` proves the cut on lavapipe. `FrameRenderer::set_ssgi_half_res` halves the gather on any tier (a quarter of the pixels, counted in the same test). Card times are the lead's.

**Re-tuned on honest plates (ADR-2605).** The probes' 25 to 41 % deficit on the trench and color_room plates was the plates': their layouts stopped short of what the cameras see, and a pixel outside every window reads the flat sky term. With layouts that hold the view, probe energy over the tracer is 0.961 and 1.064 and the sealed box leaks 0.02 %. On those plates the miss read adds noise and no accuracy, so `miss_cap` defaults to 0: a miss counts nothing, and the miss row of the table above needs `miss_cap` > 0. Hits only ties the plain gather's relative RMSE under temporal AA (0.157 against 0.158 on the trench, 0.154 and 0.154 in the color room) with better FLIP and energy. `darken_limit` stays 0 and `strength` 1.

**Acceptance.** `tests/radiance_cache.rs`: after switching off, a frame equals a renderer that never had it, byte for
byte, with no buffers left; the budget holds on every frame, every cell is written, the cache rests and a move wakes it; a held
view keeps gathering until the cache has filled; every surface of the sealed 3 m room ≤ 0.5 % at the Low and High tiers with the
cache on, with the full and with the half-resolution gather; the half gather adds about what the full one adds and never goes below the
frame it corrects; a held half-resolution view reuses a gather only from a cache at rest; an XR eye's image is the image without the cache; open
ground hardly changes; no pipeline on a warmed frame or a scroll. `tests/radiance_cache_alloc.rs`: no allocation
growth. `regia_pt` `tests/gi_radiance_cache.rs`: against the path tracer on the three GI validation plates, the cached frame stays
within a tolerance of the plain gather's error (relative RMSE + 0.02, FLIP + 0.005, energy), the economy gather likewise under temporal AA at full and half resolution, a signed result stays within 5 % of the probe frame's mean on the lit plates, and the sealed box takes no
more light than the frame it corrects. The hashes of the 47 reference frames and `studio_frame_pin` hold with the cache off.

As built (w7-render-gi-smalls-z, 2026-11-12, ADR-7456): a level's grid corner saturates like the probe field's
`cascade_base` (a finite eye past the `i32` cell grid no longer overflows the planner) and stays at most
`i32::MAX - dims`, and the scroll compares corners in `i64`; inside the grid the corner is unchanged. The probe field's
`DdgiSettings::clamped` also caps `sdf_pool_bytes` at one storage binding (128 MiB, the bound the probe buffers already
had); every tier's pool is under it.

Sources: Ward, Rubinstein, Clear, "A Ray Tracing Solution for Diffuse Interreflection", SIGGRAPH 1988; Křivánek,
Gautron, Pattanaik, Bouatouch, "Radiance Caching for Efficient Global Illumination Computation", IEEE TVCG 11(5) 2005;
Tanner, Migdal, Jones, "The Clipmap: A Virtual Mipmap", SIGGRAPH 1998; McGuire, Mara, "Efficient GPU Screen-Space Ray
Tracing", JCGT 3(4) 2014; Majercik et al., JCGT 8(2) 2019; Duff et al., "Building an Orthonormal Basis, Revisited", JCGT
6(1) 2017.

## 13. ReSTIR GI on hardware ray queries (stage 2b, ADR-3349)

`RenderProfile::restir_gi` and `FrameQuality::restir_gi` select the stage-2b path. Both are false in every
preset and baseline, and the runtime preset match ignores the opt-in. Device creation requests ray queries
when ReSTIR GI or traced sun shadows asks, and hit-vertex return where offered. Enabling after device creation
cannot add either feature: the frame keeps this spec's probe tier and reports the fallback reason.

Each visible pixel sends one cosine-distributed ray through the frame's TLAS. The hit's geometric normal comes
from the committed triangle, and its radiance is the existing probe lookup plus material-colored direct sun
with a shadow ray and emissive. Temporal reservoirs reproject with camera motion, reject changed surfaces and
occluded connections, cap history at 30, and periodically reevaluate radiance. Spatial reuse applies the
solid-angle Jacobian and a visibility ray before the existing GI denoiser. The implementation uses the public
papers' practical biased normalization, not unbiased MIS. Hit base color and emissive are constant per draw;
hit texture albedo is not sampled. Camera reprojection of a still scene is unchanged. A history
sample is also dropped when the hit's object motion disagrees with the stored world point by more
than 0.05 m (`HIT_MOTION_METRES`, ADR-4001).

A frame-safe adapter in `restir_gi` runs the unchanged `gi_denoise.wgsl` passes without submitting, polling or
allocating output images on the CPU. The final pass writes the existing eight-word DDGI screen texels before
the forward pass, so the forward and terrain shaders need no ReSTIR permutation and no DDGI source changes.
The trace and denoise run at internal resolution; only composition downsamples. Asynchronous counters report
rays, temporal and spatial reuse, history rejections, validation rejections and object-motion rejections,
with null before readback.

`render.restir_gi` reports the switch, device, settings, path, fallback, history and work counters.
`render.set_restir_gi` changes `enabled`, `temporal`, `spatial_taps`, `history_cap`, or `reset_history`.
Invalid calls answer `F0002`, missing renderer resources `F0003`; unsupported hardware is not an RPC error.
`book/agents/restir-gi.md` is the guide.

Acceptance uses `regia_pt/tests/restir_gi_validation.rs` at fixed small sizes and frame counts: trench,
color_room and sealed_box report indirect-diffuse FLIP mean and p95 beside this harness's DDGI rows; sealed-box
leak stays within spec 27 § 2.2's 0.5 %, a held view's 32 post-convergence frame-to-frame FLIP values stay
within § 2.3's 0.01, steady frames create no GPU objects, and a device without ray queries draws the probe
frame bit-identically. Plate rows improve the DDGI mean FLIP or the lane report gives the numbers and reason.
Card cost is measured separately by the lead; lavapipe is never a frame-time budget.
