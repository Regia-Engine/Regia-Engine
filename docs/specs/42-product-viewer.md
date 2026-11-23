# 42 — Product viewer (web player and Android viewer)

Status: proposed (ADR-2536); built by the JW lanes (as-built notes in § 13, ADR-5637). Plan items: JW-L1 to JW-L6 (`docs/master-plan.md` § 13). Crates: `regia_configurator`
(new, no rendering), `regia_viewer` (new, the lean renderer and the view controller), `regia_web` (new, wasm32 only),
`samples/product_viewer` (new: desktop window and the Android app), `regia_cli` (`regia viewer pack`), `regia_agent`
(`configurator.*`, `viewer.*`).

Review: pending (ADR-1585). Builders read this file, `docs/formats/jewelry-*.md` once lane `jw-contract-v2` lands them,
`specs/21` (Android packaging) and `specs/12` § 4a (frozen formats). The contract names and fields come from the private
plan repo's `interfaces.md`, which wins over this file at the boundary. Pilot data is read at test time from
`$JEWELRY_DATA` / `$JEWELRY_OUT` and never enters this repo.

## 1. Purpose and scope

A product viewer shows **one configurable product** at a time. The person taps to change its options (metal, gem,
part choices) and drags to turn the view inside the limits the product allows. Every state it shows is one the
product's snapshot sells. It runs entirely on the viewer's device: a web page from static files, or an Android app.

The look is clean and believable, not photoreal: shiny metal from one baked environment, faceted stones that sparkle
from a cheap shader, soft fixed light. There is **no ray tracing, no bounce loop, no streaming, no server**.

The engine is generic: it knows parts, sockets, material zones, gem slots, option groups and resolution. It knows no
supplier and no catalog row. Names stay generic (`product`, `part`, `gem slot`); the wire names keep the plan's
`jewelry.` namespace.

**Out of scope** (cancelled with plan v1): path tracing and still refinement, a denoiser, remote refine, WebRTC or any
stream, the tablet thin client, physically exact gems and gem transport tables, spectral anodize solving at runtime,
pre-rendered stills, photo match and blind A/B tools, tenant licences and signing, render-out and session records, a
macOS release, pricing and carts (the viewer emits the build; it never prices).

## 2. Architecture

```
          page JS / Android activity
                 │  (open, options, setPick, build, setView, events)
   ┌─────────────┴─────────────┐
   │ regia_web (wasm32)        │ samples/product_viewer (native: Linux, Windows, Android)
   │ wasm-bindgen API, rAF,    │ winit window, touch, option panel (egui)
   │ pointer events, fetch     │ file/URL package source
   └─────────────┬─────────────┘
                 │
   regia_viewer ─┼─ renderer (wgpu: one forward pass, uniforms + textures only)
                 ├─ view controller (`view/`: drag, pinch, wheel, reset, damping, clamps)
                 └─ package loader (manifest, hashes, mesh, env)
                 │
   regia_configurator  (recipe, option graph, material library, part kit; picks; resolve; build JSON)
```

- **`regia_configurator`** is pure Rust: `serde_json`, no `wgpu`, no threads, no file system in the core (IO behind a
  `Source` trait that hands it bytes). It compiles on every target including `wasm32-unknown-unknown`.
- **`regia_viewer`** depends on `wgpu`, `glam`, `bytemuck`, `tracing`, `blake3` and `regia_configurator`. It does
  **not** depend on `bevy_ecs`, `regia_render`, `regia_app` or `regia_platform` (ADR-2536 (b)). It renders into a
  caller-supplied `wgpu::Surface` or an offscreen texture (tests and stills).
- **`regia_web`** is `cfg(target_arch = "wasm32")` in content (it builds as an empty crate elsewhere so
  `cargo check --workspace` stays green). It owns the browser loop and the JS API (§ 3).
- **`samples/product_viewer`** is the native host: a desktop window for development and the Android app (§ 9).

All four crates sit outside the default engine frame. No existing default frame, golden or format moves.

**wasm32 rules for the shared crates** (`regia_configurator`, `regia_viewer`): no threads, no `std::fs`, no
`std::time::Instant` (it panics on `wasm32-unknown-unknown`: the host passes the frame time in, from
`performance.now()` on the web), no randomness (tests seed their own generators; nothing pulls `getrandom`), no
blocking IO (the host fetches bytes and hands them over). `cargo check --target wasm32-unknown-unknown` of both crates
is part of the JW light gate.

## 3. Web player API (JW-L1)

The API is exported from Rust through `wasm-bindgen`. The glue JS that `wasm-bindgen` writes is generated, like the
Android Gradle shim, so no hand-written JS ships in the engine except the demo page (ADR-2536 (a)).

```js
import init, { RegiaViewer } from "./regia_web.js";
await init();                                    // fetches regia_web_bg.wasm
const viewer = await RegiaViewer.create(canvas, {
  studio: "studio/",                             // shared studio package URL (§ 8.3), cached by the browser
  backend: "auto",                               // "auto" | "webgpu" | "webgl2"
  maxPixelRatio: 2,                              // devicePixelRatio cap (cost control)
  msaa: 4,                                       // 1 or 4
});
viewer.on("ready",   e => …);                    // product open, first frame shown
viewer.on("changed", e => …);                    // picks or view changed; e.build is the new build
viewer.on("error",   e => …);                    // e.code, e.message
await viewer.open("products/a1/");               // package folder URL; resolves after the first frame
const groups = viewer.options();                 // § 3.2
const result = viewer.setPick("Material", "Titanium");      // § 3.3; multi groups take an array
const build  = viewer.build();                   // jewelry.build/1 (§ 5.4)
viewer.setView({ tilt: [10, -5], zoom: 1.5, animate: true });
viewer.resetView();
viewer.destroy();
```

### 3.1 Calls

| Call | Behavior | Errors (`e.code`) |
|---|---|---|
| `RegiaViewer.create(canvas, opts)` | Picks the backend (§ 4.1), creates the device, loads the studio package, sets `touch-action: none` on the canvas | `backend_unavailable`, `studio_fetch_failed`, `studio_invalid` |
| `open(url)` | Fetches `manifest.json`, checks every file hash, loads the configurator state and meshes, warms every pipeline, draws the first frame, fires `ready`. Opening a second product replaces the first | `package_fetch_failed`, `package_hash_mismatch`, `package_version_unsupported`, `package_invalid`, `product_not_orderable` |
| `options()` | Groups and values with availability for the current picks (§ 3.2). Never enumerates combinations | — |
| `setPick(group, value, opts?)` | Applies a pick only if the result is sold (§ 5.2). On refusal nothing changes | `pick_unknown_group`, `pick_unknown_value`, `pick_not_sold` |
| `build()` | The current build as JSON (§ 5.4) | — |
| `setView(view)` | Sets the view target; clamps to `view_range` and reports `clamped` (never an error) | — |
| `view()` | The current view state (§ 6.1) | — |
| `resetView()` | Animates to `camera_default` (also double-tap / double-click) | — |
| `on(name, fn)`, `off(name, fn)` | Event listeners. The same events are also dispatched on the canvas as `CustomEvent`s `regia-ready`, `regia-changed`, `regia-error` | — |
| `stats()` | Loop and draw counters, backend, last switch time, look problems (`book/agents/web-player.md`; ADR-2541) | — |
| `setDeterministic(on)`, `step(dtMs, force?)`, `frameHash()` | Scripted capture: frames come only from `step` with the given time, on the script's clock, so the same calls make the same frames; `force` draws even when nothing changed; `frameHash` hashes the canvas's pixels (plan JW-L7; ADR-2541, ADR-2757; `book/agents/capture-hooks.md`) | `bad_argument` |
| `destroy()` | Stops the loop, drops GPU objects, removes listeners | — |

Async calls return a `Promise` and reject with `{ code, message }`. Synchronous calls throw the same object. Error
codes are agent-protocol style: lowercase ASCII, digits and `_`, listed in `book/agents/product-viewer.md`.

### 3.2 `options()`

```jsonc
[{ "group": "Material", "kind": "single", "current": "Titanium", "visual": true,
   "values": [{ "value": "Titanium", "label": "Titanium", "available": true },
              { "value": "18K Rose Gold", "label": "18K Rose Gold", "available": false,
                "reason": "not_sold_with", "conflicts": ["Anodization"] }] },
 { "group": "Gem Sandblasting", "kind": "multi", "current": [], "visual": true, "values": [ … ] }]
```

- `available` is computed for **one step away** from the current picks: would `setPick(group, value)` resolve to a
  sold build. `reason` is `not_sold_with` (with the conflicting groups) or `not_orderable`.
- `visual: false` marks groups the recipe lists in `no_visual` (they change the build, not the picture).
- A product whose graph status is `incomplete` (for example a truncated listing) opens, carries
  `incomplete: true` on the `ready` event and in `build().resolution.reasons`, and is never presented as the full range.

### 3.3 `setPick`

- Default: **refuse.** If the result would not resolve to a sold variant, the call fails with `pick_not_sold` and the
  state, the picture and the build stay as they were.
- `setPick(group, value, { adjust: true })`: if refused, the configurator looks for the sold build that keeps the new
  pick and changes the **fewest** other groups, breaking ties by the option graph's group order, then by value order.
  The result names the groups it changed (`adjusted: ["Anodization"]`). It still never reaches an unsold state. The
  search is bounded (§ 5.2); if nothing is found inside the bound, it refuses.
- Success returns `{ ok: true, build, adjusted }` and fires `changed`. A material-only change shows on the next frame
  (§ 7.4); a geometry change within the switch budget (§ 10).

### 3.4 Loop, visibility and resize

- **Lazy loop.** A `requestAnimationFrame` callback is scheduled only while something is dirty: input, damping still
  moving (§ 6.3), a pick change, a resize, a context restore. When nothing is dirty no callback is pending: **zero
  draws and no timers while idle**.
- **Hidden.** On `visibilitychange` to hidden the loop stops; on visible it draws one frame.
- **Resize.** A `ResizeObserver` on the canvas sets the drawing size to CSS size × `min(devicePixelRatio,
  maxPixelRatio)` and draws one frame.
- **Context loss.** WebGL2 `webglcontextlost` or WebGPU `device.lost` fires `error` with `device_lost`; on restore the
  player recreates GPU objects from the loaded package (kept in memory) and draws.
- **Input.** Pointer events with pointer capture (mouse, touch, pen as one path), `wheel` for zoom, two pointers for
  pinch, `dblclick` and a double-tap detector (two taps within 300 ms and 20 px) for reset. Default browser gestures
  are suppressed on the canvas only.

## 4. Viewer render path (JW-L4 hooks, lane `jw-viewer-render`)

### 4.1 Backends

`wgpu` 30 on every target. Native: Vulkan (Linux, Android), DX12 or Vulkan (Windows), Metal on the Mac for testing.
Web: **WebGPU where the browser has it, WebGL2 otherwise** (`backend: "auto"`). The renderer is written for the
**WebGL2 limits on every backend** (`wgpu::Limits::downlevel_webgl2_defaults()` and the WebGL2 downlevel flags), so one
code path serves all and a WebGPU-only bug cannot hide (ADR-2536 (a)):

- **Uniform buffers and textures only.** No storage buffers, no storage textures, no compute, no indirect draws.
- Per-frame uniform block ≤ 1 KB (camera, SH9 irradiance, the fixed key lights); per-draw data through one uniform buffer with dynamic offsets (256 B aligned); whole
  uniform buffers ≤ 16 KB each.
- Per-instance data (gem stones of a pavé, repeated beads, chain links) in **instance-step vertex buffers**.
- No cube-map arrays, no texture arrays in a fragment loop, no `textureSampleLevel` on depth, no per-target blend
  state. Fragment shaders have no loops whose count depends on data.
- Colors go straight to the surface: tone mapping happens in the fragment shader (no HDR offscreen pass, no float
  render target, which WebGL2 does not guarantee). MSAA 4 by default, 1 on request.
- Depth `Depth24Plus`. World units are **millimetres** in the viewer (its own camera, no engine meter-scale constants).

### 4.2 Pass

One forward pass, opaque first (metal parts, cabochons, opal), then gems (opaque in the depth buffer; each stone is a
closed convex solid drawn front faces only, so no sorting is needed), then an optional soft contact shadow quad under
the piece (a pre-blurred radial texture scaled to the piece's footprint, alpha-blended, drawn last). No shadow maps,
no post-processing chain. Optional FXAA when MSAA is 1.

### 4.3 Materials

`ViewerMaterial` is a closed enum; every value is **data** from the material library (Tincal's
`jewelry.material_library/1`), never code. Each kind has one pipeline per pass and MSAA setting; a swap changes only a
uniform slot or a bind group, **never creates a pipeline** (a test counts creations).

| Kind | Inputs (from the library) | Shading (cheap) |
|---|---|---|
| `Metal` | `f0_rgb_linear`, optional `f82_tint_rgb`, roughness, finish | Schlick Fresnel from F0 (optionally the F82 tint term), prefiltered env sampled at the roughness mip, analytic environment BRDF fit already used by the engine's mobile shader (`env_brdf_approx`), plus 2-3 fixed studio key lights as GGX highlights |
| `Anodized` | `substrate_material_id`, `fresnel_lut` (32-64 entries, cos θ → RGB) or a plain color | The metal path with F0 replaced by a 1D LUT texture lookup on cos θ. No film solving at runtime |
| `Gem` | `n_d`, `abbe_vd`, `attenuation_rgb_per_mm` (or a tint), `surface_roughness` | § 4.4 |
| `Cabochon` | as `Gem`, smooth normals | Reflection + one refraction tap + tint; no dispersion, no glints |
| `Opal` | base and play-of-color parameters (`gem_special`) | View-dependent hue from a small noise texture; soft gloss |
| `Frosted` | as `Gem` plus roughness | Gem path with refraction sampled at a rough mip; no glints |
| `Finish` (modifier) | `finish` entry: polish, brushed, sandblast, hammered | Roughness and an optional shared detail normal map from the studio package |

### 4.4 The cheap gem (lane `jw-looks`; the owner approves the look from a still)

A faceted stone is a **closed convex mesh with flat facets** built at load from the cut's facet planes (part kit
`gem_cuts[].facets`, plane normal plus offset in mm) by half-space intersection, scaled to the slot's `size_mm`; concave
cuts (heart notch) use the cut's `mesh_ref`. Vertices are not shared across facets, so each facet keeps its own normal.
The fragment shader then does a fixed number of texture lookups, no loops:

1. **Reflection:** environment lookup along the reflected view ray, weighted by Schlick Fresnel with `F0 = ((n-1)/(n+1))²`.
2. **Fake refraction ("brilliance"):** refract the view ray into the stone at the facet; approximate the pavilion's
   return by mirroring that ray about the stone's axis (the slot's `z_axis`), refract out through the table normal, and
   look the environment up along the result. Weight by `(1-F)²` and the tint `exp(-attenuation × path)`, with `path`
   about twice the stone's depth.
3. **Fake dispersion ("fire"):** step 2 with three indices spread by the Abbe number, `Δn = (n_d - 1) / V_d`, one tap
   per RGB channel (three taps, unrolled).
4. **Glints ("sparkle"):** for each of the 2-3 fixed studio key lights, a very narrow highlight on the facet normal
   (Blinn-Phong half-vector with a high exponent). Because facets are flat and the lights are fixed, glints flash on and
   off as the view turns and are **stable while still** (no noise, no temporal flicker).
5. Tone map and output.

Cost target: at most 8 texture samples and no branch on data per gem fragment. The look lane may change steps 2-4 for
a better read at the same cost class and records the choice in its ADR; the owner signs off the still.

### 4.5 Environment

One **studio package** (§ 8.3) shared by every product: a prefiltered specular cube map (128² base, roughness mips),
irradiance as 9 spherical-harmonic coefficients in the frame uniform, the contact-shadow texture, and the optional finish
detail maps. Baked offline from an HDR by `regia viewer pack --studio`. Lights stay fixed with the studio (plan
decision D4); the piece turns, the studio does not.

### 4.6 Headless

`regia_viewer` renders the same frame into an offscreen texture and reads it back (`Viewer::render_still(width,
height) -> RgbaImage`). Tests and evidence stills use it on lavapipe; images go to `$JEWELRY_OUT/evidence/regia/`,
never into the repo. Hash-stable output on one host and backend.

## 5. Configurator (JW-L3, lane `jw-configurator`)

### 5.1 Inputs

The four files `interfaces.md` names, read through the `Source` trait: piece recipe (`jewelry.piece_recipe/1`), the
product's option-graph record (`jewelry.option_graph/1`), the material library entries the product can show
(`jewelry.material_library/1`) and the part kit (`jewelry.part_kit/1`). Contract tests validate each against the
owner's published `schema.json` under `$JEWELRY_OUT/exchange/` (lane `jw-contract-v2`). Refusals are stable codes:
`kit_invalid`, `recipe_invalid`, `recipe_drive_unknown`, `graph_invalid`, `library_invalid`, `house_forbidden`.

### 5.2 Resolve and refusal

- `resolve(product_key, picks) -> Resolution` is a Rust port of the shared behavior (`interfaces.md` § 9). It finds the
  variant, checks the add-ons, builds the cart lines or the manual line and writes the statement line. Packages carry no
  cost, so it reports no price.
- **Exact match** with `exchange/option_graph/1/fixtures/resolve_vectors.jsonl` on every field the vectors carry
  (`orderable`, `variant_key`, `sku`, cart lines or manual line, statement). The test reads the vectors by path at
  test time and fails hard when `REGIA_JW_REQUIRE_PILOT=1` and they are missing.
- **Never enumerate combinations.** Availability (§ 3.2) evaluates one step away. The `adjust` search (§ 3.3) visits at
  most 4,096 candidate pick sets in a fixed order, then refuses.
- **The reachable states equal the sold set.** A property test drives random pick sequences (seeded) and asserts no
  state ever resolves unsold. `not_orderable` and `duplicate` products do not open; `basic` products are never
  packaged.

### 5.3 Assembly

- One part instance per recipe `parts[]` entry, placed at its socket; `drives` map an option group to a material zone,
  a gem slot material, a part swap or a named value; `values` and `$name` references are evaluated per pick.
- A parameter is realized as the kit says: `variant_mesh` (a different mesh, by key) or `morph` (deltas blended on
  the CPU at pick time, never per frame). `bone_scale` maps to a per-part scale. Ranges are checked and clamped with a
  report.
- Gem slots expand `count` and `pattern` to stone instances (one instance-step record each).
- `geometry_key` is the hash of every pick except metal, gem material and color, and finish (`interfaces.md` § 0.2).
  A pick that keeps the geometry key is a **material-only** change.

### 5.4 The build

`jewelry.build/1` as `interfaces.md` § 7 fixes it, with these v2 rules:

- `camera` holds the view state (§ 6.1): `rig_preset` is `front`, `tilt_deg` is the tilt magnitude, plus `tilt_xy_deg`,
  `spin_deg`, `zoom`, `pan_mm`; optional fields within the major version (`interfaces.md` § 0.1). `focal_length_mm`,
  `sensor_mm` come from `camera_default`; `dof` is `false` (the viewer has no depth of field).
- `tier` is `"interactive"`; `studio_id` names the studio package; `output_gamut` is `"srgb"`.
- `tenant_id` comes from the package manifest (the tenant the package was built for).
- **Never** cost, client data or an address. A test checks the schema and the serialized form.
- Serialized canonically (RFC 8785) so `build_hash` is stable. A saved build reopens by re-resolving; if that fails it is
  marked `no_longer_orderable`.

## 6. Drag view (JW-L5, lane `jw-view-pack`, module `regia_viewer::view`)

### 6.1 State

`ViewState { tilt_xy_deg: [f32; 2], spin_deg: f32, zoom: f32, pan_mm: [f32; 2] }`. The camera looks at the piece's
pivot along the front axis (`view_range.front_axis`) rotated by `tilt_xy_deg` (x about the up axis, then y about the
right axis), at distance `camera_default.distance_mm / zoom`, with the focal length from `camera_default` and a 36 mm
sensor. Spin rotates the **piece** about the front axis.

### 6.2 Limits (from the recipe's `view_range`; binding)

| Field | Rule |
|---|---|
| `tilt_max_deg` (default 30) | `|tilt_xy_deg| ≤ tilt_max_deg` as a **circular cone** about the front axis; out-of-cone targets are projected back onto the cone edge |
| `spin` `none` (default) | spin stays 0 |
| `spin` `rock` | spin within `±rock_max_deg`; springs back to 0 when the drag ends |
| `spin` `about_front_axis` | two-pointer rotate turns the piece about the front axis, unbounded |
| `spin` `free` | horizontal drag turns the piece without limit; tilt is bounded at ±85° instead of the cone. Allowed only when the recipe's `back` is `exact` or `plausible` |
| `pan_bounds_mm` | pan only when zoomed in past 1.0; clamped to the bounds |
| `max_px_per_mm` | zoom-in limit: the projected scale never exceeds it on the current canvas |
| zoom-out | the piece's bounding sphere fits the shorter canvas side with 10 % margin |

### 6.3 Gestures and damping

- One-pointer drag: tilt (and spin under `free`). Sensitivity: a drag across the canvas's shorter side spans
  `2 × tilt_max_deg`. Two pointers: pinch zoom and pan (and rotate under `about_front_axis`). Wheel: zoom about the
  pivot. Double tap / double click: reset to `camera_default`, animated.
- The input sets a **target**; the shown state follows by exponential smoothing with a 70 ms time constant
  (`a = 1 - exp(-dt / τ)`), frame-rate independent. When every component is within 0.01° (or 0.001 zoom, 0.001 mm)
  of the target, the state snaps to it and **the loop stops** (§ 3.4).
- A fuzz test drives random input sequences through the controller and asserts no shown state ever leaves the cone,
  the spin rule, the pan bounds or the zoom limits.

### 6.4 What a free orbit needs

The renderer bakes nothing per view cone, so a free orbit costs no renderer work. It needs: `spin: free` in the recipe;
`back: exact` or `plausible` (the kit's parts carry `accuracy`, and posts and pins exist as generic hidden parts); and a
person checking the back by eye. The viewer refuses `free` on `back: generic` (falls back to `none`, reported on
`ready`).

## 7. Swaps

1. **Material-only change** (metal, gem material or color, finish): a uniform-slot or bind-group change; shown on the
   next frame. No upload except a LUT already resident.
2. **Geometry change** (a new geometry key): parts are rebuilt from meshes already in memory (the whole package is
   loaded at open); morph blends run on the CPU once. Shown within the switch budget (§ 10). No crossfade in v2; the
   view never shows a different combination labeled as the chosen one.
3. **The camera holds** across swaps unless the new piece no longer fits; then zoom-out is re-clamped.
4. **0 pipeline creations after open.** Every pipeline of every material kind the product can show is created during
   `open` (the warm set). A creation after `ready` is an error (`warm_set_miss`) in tests and a `problems` entry in the
   agent surface.

## 8. Product package (JW-L6, lane `jw-view-pack`; ADR-2536 (d))

### 8.1 Layout

A static folder; every file is fetched by relative URL. No server logic, no query strings, no cookies.

```
<product_slug>/
  manifest.json        jewelry.product_package/1 (§ 8.2)
  recipe.json          jewelry.piece_recipe/1, verbatim from Tincal
  options.json         the product's jewelry.option_graph/1 record, every cost and markup field stripped
  materials.json       the jewelry.material_library/1 entries this product's options can select
  kit.json             the jewelry.part_kit/1 entries this product uses (parts, gem cuts with facet planes)
  mesh.rvm             regia.viewer_mesh/1: every part mesh and variant the recipe can reach (§ 8.4)
  luts.bin             optional: anodize cos θ LUTs, RGBA8 × 64 per entry
  thumb.webp           optional: a poster image the page shows before the player is ready
```

The studio package is a sibling folder shared by all products (§ 8.3).

### 8.2 Manifest

```jsonc
{
  "schema": "jewelry.product_package/1",
  "format_version": 1,
  "package_version": 3,
  "regia_min_version": "0.2.15",
  "product_key": "…", "design_key": "…", "house": "…", "tenant_id": "shop:…",
  "snapshot_id": "…", "recipe": { "version": 3, "hash": "sha256:…" },
  "studio": { "studio_id": "studio-soft-1", "blake3": "…" },
  "files": [{ "name": "mesh.rvm", "kind": "mesh", "size": 412345, "blake3": "…" }, …],
  "package_hash": "blake3 of the RFC 8785 form of files[]",
  "sizes": { "total": 701234, "mesh": 412345, "data": 98765, "luts": 4096 }
}
```

- `files[]` keeps the chunk record fields of `interfaces.md` § 12 (`name`, `kind`, `size`, `blake3`), so a later
  per-tenant pack can list product packages as chunks. **No signature in v2** (signing is cancelled).
- The player checks every file's `size` and `blake3` before use and refuses the package on a mismatch
  (`package_hash_mismatch`).
- **Isolation:** a package holds one house plus `shared` parts only; the packer refuses anything else. **No cost:** the
  packer strips every `cost` and markup field and a test greps the package.
- `format_version` is checked: newer than the player knows → `package_version_unsupported`. The schema goes to
  `docs/formats/jewelry-product-package.schema.json` with a `check-formats` hash; a later shape change needs a version
  bump and a migrator (`specs/12` § 4a), run by `regia viewer pack --upgrade`.

### 8.3 Studio package

`studio/manifest.json` (`regia.viewer_studio/1`, same `files[]` record) plus `env.rvs` (`regia.viewer_env/1`: the
prefiltered cube map as RGB9E5 texels, little-endian, base face size, mip count, SH9 irradiance), `shadow.png` and
optional finish detail maps. Target 0.6-1.0 MB, fetched once and cached by the browser across products.

### 8.4 `regia.viewer_mesh/1` (`.rvm`)

A little-endian binary, 4-byte aligned, built for small size and a parse with no allocation per vertex.

| Section | Content |
|---|---|
| Header | magic `RVM1`, `format_version` u32, mesh count, section table (offset, length) |
| Mesh record | key (part id + variant), bounding box (mm, f32 × 6), vertex count, index count, index width (16 or 32), sub-mesh ranges each with a material zone index |
| Positions | u16 × 3 quantized within the mesh's box (0.15 µm steps on a 10 mm part) |
| Normals | octahedral, i16 × 2 (smooth metal highlights need the 16 bits) |
| UV | optional, u16 × 2 in [0, 1] |
| Indices | u16 or u32 |
| Morphs | optional: per target, sparse vertex ids (u16/u32) and i16 × 3 position deltas in the same quantization |

Gems are **not** stored as meshes: the player builds them from the kit's facet planes (§ 4.4). The cook
(`regia viewer pack`) reads the kit's `.glb` files desktop-side (`gltf` crate, already a workspace dependency); the
runtime reads only `.rvm`.

### 8.5 Size budget (targets, measured at JW-M1 and JW-M3)

| Item | Typical | Hard cap |
|---|---|---|
| Product package (all files, as transferred) | ≤ 1.0 MB | 3 MB; the packer fails above it |
| `mesh.rvm` | 0.2-0.7 MB | 2 MB |
| JSON (`recipe`, `options`, `materials`, `kit`) | ≤ 0.2 MB after HTTP compression | 0.6 MB |
| Studio package (once per site) | 0.6-1.0 MB | 1.5 MB |
| Web player (`.wasm` + glue JS, brotli) | report it; target ≤ 2.0 MB | 3 MB |

Metals and gems are parametric, so a typical product ships **no textures**. Static hosts compress JSON on the wire;
the packer also writes `.br` siblings for hosts that serve pre-compressed files. `regia viewer pack` prints a size report
per package and per file kind.

### 8.6 How a page loads it

1. The page loads `regia_web.js` (module) and shows `thumb.webp` meanwhile.
2. `RegiaViewer.create` fetches the wasm (cached) and the studio package (cached).
3. `open(url)` fetches `manifest.json`, then the files in parallel, checks hashes, parses, warms and draws.
4. Target: first frame ≤ 2 s on a normal mobile connection with a cold cache, ≤ 0.5 s warm.

## 9. Android viewer (JW-L2, lane `jw-android`; ADR-2536 (c))

- A **native build of the same `regia_viewer`** in `samples/product_viewer`: `winit` (the workspace's
  `android-game-activity` feature), `wgpu` on Vulkan with GLES fallback, an option panel drawn with `egui` (`egui-wgpu`
  on the same device, in a second pass over the viewer; both are pinned workspace dependencies), touch drag, pinch zoom, double-tap reset.
- Lifecycle: the `Surface` is dropped on `suspended` and recreated on `resumed`; the device and the loaded package
  survive (AGENTS.md § 5). Lazy loop: no redraw requested while idle, so the GPU idles.
- Packages: copied into the app's files directory by `adb push`, or downloaded once from a LAN `https://` URL and kept.
  Offline afterwards. A product list screen shows the packages present.
- Packaging: the existing Android path (`cargo ndk`, the generated Gradle shim of `specs/21`, sideload APK). If
  `regia package android` assumes a full engine project, the lane adds a small, default-off option for a viewer app and
  lists it under "Engine files touched".

## 10. Targets (plan § 4; measured, reported in `docs/perf-log.md`)

| Measure | Target | Where |
|---|---|---|
| Frame time while dragging | 60 fps on the Pixel 6 (app and Chrome); never below 30 fps on a mid-range phone browser | Pixel 6, Fairphone 5 |
| Idle | zero draws; no pending rAF or timer; CPU near zero | counters + browser profiler |
| Option switch | material-only next frame; geometry ≤ 100 ms p95; no frame over 33 ms during a switch | the agent's `viewer.stats` |
| Product open | package ≤ 1 MB typical (cap 3 MB); first frame ≤ 2 s cold, ≤ 0.5 s warm | page timing |
| Web player size | reported per release; target ≤ 2 MB brotli | `cargo xtask` size row |
| Combination safety | every unsold combination refused; resolve vectors match exactly | `regia_configurator` tests |

## 11. Agent protocol and tests

- **`configurator.*`** (lane `jw-configurator`): `open_product`, `options`, `set_pick`, `build`, `resolve`.
- **`viewer.*`** (lanes `jw-viewer-render`, `jw-view-pack`): `open_package`, `set_view`, `view`, `screenshot` (the
  headless still), `stats` (frames drawn, pipelines created, last switch time, idle state), `pack_report`.
- The web player exposes the same calls as JS (§ 3); the desktop sample and the Android app expose `viewer.*` over the
  existing agent transport.
- Each method gets a `book/COVERAGE.md` row; `book/agents/product-viewer.md` documents methods, events and error codes.
- **Tests, no goldens:** pipeline count constant across swaps; zero draws while idle (frame counter); view fuzz; resolve
  vectors; refusal property test; package hash and cost-strip checks; `.rvm` round trip; size-budget check in
  `regia viewer pack`; a headless still per material kind compared against itself on one host (hash-stable), images to
  the evidence folder. `cargo check -p regia_web -p regia_viewer -p regia_configurator --target
  wasm32-unknown-unknown` and `cargo ndk -t arm64-v8a check -p product_viewer` in the JW light gate.

## 12. Provenance (rows land with the code)

Schlick, "An Inexpensive BRDF Model for Physically-based Rendering" (1994: the Fresnel approximation); Snell's law and
the Abbe number (any optics text, e.g. Hecht, *Optics*); Greene, "Environment Mapping and Other Applications of World
Projections" (1986: cube-map environment lookup); Walter et al. 2007 (GGX highlights); Ramamoorthi & Hanrahan 2001
(SH9 irradiance); Cigolle et al., "A Survey of Efficient Representations for Independent Unit Vectors" (JCGT 2014:
octahedral normals); de Berg et al., *Computational Geometry* (half-space intersection for the facet solid); RFC 8785
(canonical JSON); the BLAKE3 specification. Gem cut proportions from published cut descriptions (for example
Tolkowsky, *Diamond Design*, 1919). Original code throughout.

## 13. As built (ADR-5637)

The conformance audit of 2026-11-10 read this spec against the code. Where the code refined it, the ADR that did so is named;
the text above is the original plan.

| Section | As built |
|---|---|
| § 3.1 | Extra error codes `no_product`, `open_superseded`, `destroyed`, `render_failed` (`book/agents/web-player.md`). |
| § 3.3, § 5.2 | `adjust` makes one pass over the sold variants instead of a 4,096-candidate bound (ADR-2534). |
| § 3.4 | Double tap: 300 ms, 30 px scaled by the device pixel ratio; the mouse goes through the same recognizer (ADR-2533). |
| § 4.3 | The finish is a roughness field of each material, not a separate kind (ADR-2587). |
| § 4.4 | Gem solids are built by the cook, one cut per offered size (ADR-2540). |
| § 4.6 | `regia_viewer::still::capture_still(renderer, width, height) -> Result<Still, ViewerError>`. |
| § 5.1 | Refusals use the `configurator_*` codes: `configurator_contract` for the four `*_invalid` cases, `configurator_house_not_allowed` for `house_forbidden` (ADR-2534, `book/agents/configurator.md`). |
| § 5.4 | `camera.rig_preset` is one of `front`, `pan`, `tilt`, `spin` (the `interfaces.md` contract wins at the boundary). |
| § 6.2 | `spin: free` is allowed with `back: generic` (ADR-2540). |
| § 6.3 | A drag across the shorter side turns 120° (`DEG_PER_VIEWPORT`); the view snaps within 1e-3°, 1e-4 mm and 1e-4 in ln zoom. |
| § 7 | `warm_set_miss` is a `stats().problems` entry of the web player (ADR-5637). |
| § 8.2 | The manifest schema is `regia.viewer_package/1`; a new version is cooked again, no migrator (ADR-2540). |
| § 8.3 | The environment is `env.renv` (ADR-2539). |
| § 8.5 | The 3 MB cap is reported; `regia viewer pack --strict-budget` fails above it (ADR-2540). |
| § 11 | Methods: `viewer.capabilities`, `viewer.render_still`, `viewer.env_info` (ADR-2539), `viewer.view_simulate`, `viewer.pack_verify` (ADR-2540), the apps' `viewer_app.*` (ADR-2533, `book/agents/product-viewer.md`). |

Not built yet (v0.5.x): morph targets and `bone_scale` (§ 5.3, § 8.4); pan only past zoom 1 and the `rock`
spring-back (§ 6.2); the mesh, JSON and studio sub-caps and the `.br` siblings (§ 8.5); a 16 KB cap on the material
uniform buffer (§ 4.1). Built since: `incomplete` in `build().resolution.reasons` (§ 3.2) and an unknown drive group
refused with `recipe_drive_unknown` (§ 5.1).

