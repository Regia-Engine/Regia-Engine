# Spec 27 — Renderer excellence: ground truth, numeric targets, reference plates as the visual gate

Status: accepted (ADR-042, 2026-10-07). Owns the post-alpha renderer programme; amends `specs/13`, `22`, `26`
where § 4.4 says so. Written from the owner's visual-quality goals of 2026-10-06
and the initial visual-comparison sheet
(`docs/evidence/parity/2026-10-07/`).

## 0. Goal

1. **Visual:** Regia frames must meet the owner's visual-quality target, judged against the retained
   DO27 reference plates using the same cameras, exposure and time of day. The final judge is the owner's eye. The working judge is an
   Opus look director reading plates side by side (§ 3). Quality is never reported as test counts.
2. **Performance:** the frame-cost target stays below the recorded reference on the RTX 5080, the RX 7900 XTX and the Steam Deck
   (§ 2.5). Performance work never trades look by default: goldens stay unchanged, or the saving is opt-in
   (ADR-041).
3. **Zero hitches is a hard gate** (§ 2.4). Regia Engine uses pipelines built before gameplay,
   non-blocking streaming and frame pacing. A
   build that hitches does not pass, however good the stills are.
4. **Ground truth is a reference path tracer built in Regia Engine first** (§ 1). Every real-time technique is
   scored by its error against that reference (§ 2), so workers optimise numbers, and the owner's eye settles
   what the numbers cannot.

## 1. Ground truth: the reference path tracer

### 1.1 Shape
- Crate `regia_pt` (layer 2, next to `regia_terrain` and `regia_vfx`; depends on `regia_render`,
  never the reverse). Offline tool, never shipped in a game binary. Reached from `render_lab pt <scene>`,
  `regia_cli render --reference`, the editor's look-dev panel (M11 surface) and the agent protocol (§ 5).
  `cargo xtask check-layers` enforces "never shipped": no runtime crate or game sample may declare `regia_pt` or turn
  on a feature that links it (`lookdev`, which holds the offline `pt_plates`, is the one exception; `regia_agent`
  keeps it optional behind the tool-only `reference-trace`). The editor panel (`editor.lookdev_*`) traces the
  editor camera's or the game camera's view of the open level through the runtime extraction, on its own device
  (ADR-5896).
- **Same inputs as the real-time path, by construction.** It consumes the extracted render world
  (`FrameMesh` lists, materials, lights, `Environment`, `LevelSettings.atmosphere`, `TerrainSharedDraw`
  heights and layers, decals, VFX billboards as emissive/absorbing quads) — not a second scene description.
  A scene that renders in the runtime frame traces in the PT with no extra authoring.
  The terrain's material is the terrain shader's own output: a map-wide top-down bake of its albedo for
  every hit, and, at camera-ray hits, the albedo and shading normal the frame shaded that pixel with
  (`PtViewAlbedo`, rendered from the frame's camera), so footprint-dependent layer blends, near-field
  detail and normal maps match the raster pixel for pixel (ADR-150 "terrain view inputs").
- **Same BRDF, by construction.** `pbr_lighting.wgsl` (`d_ggx`, `v_smith_ggx_correlated`, Schlick, the DFG
  energy-compensation LUT) and `pbr_surface.wgsl` are `#import`ed through naga_oil into the PT kernels; the
  PT adds only sampling routines (§ 1.3). A BRDF change therefore moves both renderers, and a test
  (§ 1.5 item 4) proves the forward shader's direct term equals the PT's one-bounce direct AOV on a sphere.
- **GPU compute, one code path.** Wavefront kernels (generate → intersect → shade/NEE → next bounce) over a
  persistent path-state buffer in storage memory; a two-level BVH (per-mesh BLAS, scene TLAS; binned SAH
  build on the CPU, PBRT 4th ed. ch. 7) uploaded as flat nodes. Terrain is the cooked heightfield at its finest
  LOD traced as an implicit primitive: a min/max mip pyramid over the cells (Tevs et al. 2008) with the CDLOD
  grid's diagonal split, so the 4096² DO27 maps need ~110 MB instead of a 33 M-triangle BLAS (refined in
  ADR-070); displacement at the photoreal detail scale is not in v1. Hardware ray queries are an optional
  acceleration later; v1 is software traversal so lavapipe, RADV and NVIDIA produce the same image.
  **There is no separate CPU implementation:** CI and lavapipe run the same kernels at tiny sizes
  (64×36, 16 spp), which keeps "identical by construction" true.
- **Progressive and deterministic.** Sample index drives a per-(pixel, sample, bounce, dimension) PCG stream
  (a 32-bit-state PCG with RXS-M-XS output seeded by PCG hashes; WGSL has no u64 for `regia_core`'s PCG32)
  with a fixed scene seed, so frame N of two runs matches bit-for-bit on
  one device and within 1e-3 relative across devices. Converged output is the mean over N samples with a
  variance AOV, so the compare tool can report confidence.
- **Light transport.** Unidirectional path tracing with next-event estimation and multiple importance
  sampling (Veach 1997), Russian roulette after bounce 3, max 12 bounces (configurable). Lights: sun as a
  disc of the preset's angular radius and illuminance (ADR-040), sky from the same atmosphere tables the
  runtime samples (sky-view LUT importance-sampled by luminance), point/spot/area lights in physical units,
  emissive surfaces (ADR-031 emissive units). Participating media (v2): the froxel fog parameters as a
  height fog medium and the planet's Rayleigh/Mie air as "aerial" layers, all analytic in height, so
  transmittance is exact and only the scattering distance is sampled (spectral MIS over channels; refined
  in ADR-079, no delta/ratio tracking needed); sky bodies (Jupiter, Io) as lit discs that light the scene
  with the real-time fill's illuminance. Clouds are traced only on request (`PtScene::clouds`,
  `seeing.reference_trace {clouds}`, ADR-2127): the raster sky's own density between the planet's curved shells,
  with delta tracking for free flights and ratio tracking for shadow rays, since that density has no closed-form
  optical depth. A frame's reference holds no clouds by default.
- **Camera.** Pinhole v1; thin lens (aperture radius, focus distance) in v2; physical shutter/ISO and
  motion blur later.

### 1.2 Outputs
Linear EXR (32-bit FLOAT channels, written by `regia_pt::exr` from the public OpenEXR layout: the `image`
crate is pipeline-only and a physical sun disc, ~1e9 cd/m², overflows half floats) plus an 8-bit AgX preview PNG
through the runtime's display transform (CPU AgX + default look) so the reference and the real-time frame pass
through one tonemap. AOVs, each its own EXR layer: `beauty`, `albedo`, `normal` (world), `depth`, `direct` (sun + local
lights, first hit), `indirect_diffuse`, `indirect_specular`, `emissive`, `volumetric`, `sky` (background and
sky light seen directly), `shadow` (visibility of the sun at the first hit, 0–1), `variance`, `spp`.
Definitions used by v1: `direct` = sun, lamps and emissive surfaces reflected once at the first hit;
`indirect_diffuse` / `indirect_specular` = everything else reflected at the first hit (sky light and all
multi-bounce light), split by the BRDF lobe; `sky` = background (sky and sun disc) seen directly by the camera.
`beauty = emissive + direct + indirect_diffuse + indirect_specular + sky (+ volumetric)`.
Metadata: scene hash, seed, spp, device, git tag, wall time.

### 1.3 Sampling (public sources)
- GGX visible-normal sampling (Heitz 2018, "Sampling the GGX Distribution of Visible Normals"); multi-scatter
  energy compensation (Kulla & Conty 2017) so rough metals do not darken — matches the runtime DFG LUT.
- Lambert cosine sampling; clearcoat, sheen, transmission per glTF KHR definitions with the PBRT 4 layered
  BSDF for clearcoat.
- Light sampling: power-weighted light selection, sun disc uniform cone, sky-view LUT 2-D CDF, spherical
  area light solid-angle sampling (PBRT 4 for spheres). Quad lights are two emissive triangles sampled by
  area, the pdf converted to solid angle and weighted by MIS against the BSDF (ADR-5630: Urena et al. 2013
  spherical-rectangle sampling is not used; `quad_light_matches_lamberts_polygon_irradiance` holds the result).
- Media: closed-form optical depth per layer; free flights invert the scattering depth (one channel by
  throughput, channel-averaged pdf), absorption applied exactly (PBRT 4 § 14.2; ADR-079).
- Terrain displacement: the PT intersects the same displaced heightfield triangles the cook emits, so
  relief parity between PT and runtime is a cook property, not a tracer property.

### 1.4 Provenance rows to add (`docs/provenance.md`)
| Area | What | Source |
|---|---|---|
| PT / transport | Path tracing with NEE + MIS, Russian roulette | Veach, "Robust Monte Carlo Methods for Light Transport Simulation", PhD thesis 1997; PBRT 4th ed. ch. 13–14 |
| PT / BVH | Binned SAH BVH build, flat node layout, stackless/stack traversal | PBRT 4th ed. ch. 7; Wald 2007 "On fast construction of SAH-based BVHs" |
| PT / sampling | GGX VNDF sampling | Heitz 2018 (JCGT) |
| PT / sampling | Multi-scatter GGX energy compensation | Kulla & Conty, SIGGRAPH 2017 course |
| PT / sampling | Quad solid-angle sampling | Urena, Fajardo, King 2013 — not used: quads are sampled as emissive triangles by area (ADR-5630) |
| PT / media | Ratio/delta tracking | Woodcock et al. 1965 and Coleman 1968 (delta); Novák, Selle, Jarosz 2014 (ratio); survey: Novák, Georgiev, Hanika, Jarosz 2018 (EG STAR). The provenance row cites the original papers (ADR-2127, ADR-5630) |
| PT / sky | Importance sampling of a tabulated sky | PBRT 4th ed. ch. 12 (image infinite light); Hillaire 2020 tables already in provenance |
| Metrics | FLIP | Andersson, Nilsson, Akenine-Möller, Oskarsson, Åström, Fairchild 2020 (HPG) — re-implemented from the paper, no code port |
| Metrics | SSIM | Wang et al. 2004 (already used by goldens) |
| Validation | White furnace, Cornell box reference values | PBRT 4th ed. § 16; Cornell box data (Cornell University, public) |

### 1.5 PT validation (acceptance of `regia_pt` itself)
1. **Furnace:** a white Lambert sphere inside a uniform white environment converges to radiance 1 ± 0.5 %
   at 1024 spp for every roughness in {0.05, 0.25, 0.5, 1.0}, metal and dielectric (energy compensation on).
2. **White furnace with GI:** a closed white enclosure with an emitting part converges to the analytic
   radiosity solution within 1 % (bounce series). v1 uses the inside of a sphere — the one enclosure whose bounce
   series has a closed form (every point sees every area element alike: L = ρ f Le / (1 − ρ)); a box has none.
3. **Analytic irradiance:** a plane under a disc sun of illuminance E at elevation θ reads E·sin θ / π in the
   `direct` AOV within 0.5 %; a sphere light of radius r at distance d matches the analytic form factor.
4. **Forward == PT direct:** `lab_forward.wgsl` and the PT `direct` AOV agree within 1 % mean relative error
   on the material-sphere grid under the sun preset (this is the "same BRDF" test).
5. **Cornell box:** the standard box with the standard light converges to within 2 % mean relative error of
   the published reference at 4096 spp; the image is committed as evidence. v1 scores the room (no blocks) against
   an independent radiosity solve (patches + Lambert's point-to-polygon form factors + final gather): the Cornell
   data page no longer serves a reference image, and no published per-pixel values are available offline.
6. **Determinism:** two runs with the same seed on lavapipe are bit-identical; lavapipe vs RADV vs NVIDIA
   within 1e-3 relative per pixel at 256 spp.
7. **Atmosphere:** the `sky` AOV of an empty scene equals the runtime sky-view LUT within 1 % (same tables).

### 1.6 v1 status (2026-10-07, lane pathtracer, lavapipe)
Measured by `cargo test -p regia_pt` (`tests/validation.rs`, `tests/forward_parity.rs`):
| Item | Result |
|---|---|
| 1 Furnace | PT = BRDF directional albedo within 0.31 % (ideal Lambert 0.9996; metal/dielectric at roughness 0.05/0.25/0.5/1 with energy compensation). The *BRDF itself* is not neutral at roughness 1: metal 0.943, dielectric 1.026 (0.5: 1.004–1.005) — a renderer finding |
| 2 GI furnace | closed sphere ρ 0.5: 0.24294 vs analytic 0.24293 |
| 3 Analytic | plane under a delta sun: exact (1e-9); disc sun 1.5e-5; sphere light 1e-5; emissive quad (NEE + MIS) 1e-4 — references include the shared BRDF (its diffuse lobe carries 1 − F(l·h)) or an ideal Lambert |
| 4 Forward == PT | material grid (dielectric, metal, clearcoat × 5 roughness): mean relative error 0.078 %. The forward frame's Rgba16F HDR clipped 0.1-roughness sun highlights (> 65504 cd/m²) — fixed by the pre-exposed HDR target (§ 1.7) |
| 5 Cornell | room vs radiosity: mean relative error 0.73 % at 4096 spp (1438 px) |
| 6 Determinism | same seed bit-identical on lavapipe and on the RTX 5090; lavapipe vs RTX 5090 at 256 spp: mean relative 2.0e-6, 99.97 % of channels within 1e-3 (max 1.8 % where a path branches on a last-ulp difference) |
| 5 evidence | `docs/evidence/pt/2026-10-07/cornell_4096.png` (512², 4096 spp, 5.0 s on the RTX 5090) |
| 7 Atmosphere | sky AOV vs `sample_sky_table`: worst pixel 0.03 % |

### 1.7 v2 status (2026-10-07, lane pt-v2, lavapipe)
v2 traces media (air + height fog), sky bodies and their fill, quad area lights, a thin lens, anisotropy,
detail maps, packed ORM and the terrain shader's own albedo (`regia_pt::terrain_bake`); the real-time HDR
target is pre-exposed, so item 4's highlights no longer clip (ADR-079). Measured by `cargo test -p regia_pt`
(`tests/v2.rs`, `tests/forward_parity_v2.rs`, `tests/terrain_bake.rs`):
| Item | Result |
|---|---|
| Media: transmittance | absorbing slab + exponential layer vs the closed form: mean error 0, worst pixel 0 (absorption is exact); an aerial layer leaves escaping rays untouched |
| Media: furnace | conservative chromatic HG + Rayleigh fog in a uniform sky (64 bounces): 1.002 / 0.999 / 0.992 |
| Sky body | uniform disc r = 5° at 40° from zenith over a Lambert plane: 2.9093 vs analytic 2.9095; light scale ×2 doubles it |
| Quad light | one-sided quad over a Lambert plane vs Lambert's polygon formula: bias −0.3 %, a quad facing away lights nothing |
| Thin lens | point 4 m out, focus 2 m, aperture 0.15 m: blur radius 4.82 px vs circle of confusion 4.70 px; in focus and pinhole ≤ 1 px |
| Forward == PT (v2 features) | anisotropic metals (3), packed ORM, detail albedo/roughness/normal: worst sphere mean relative error 0.39 % |
| Terrain bake | the bake reproduces the shader's layer albedos in the heightfield's UV layout (row 0 at −Z) |

### 1.8 Airless maps and the frame's meshes (2026-10-08, lane pt-airless, ADR-095)
DO27 reported `seeing.reference_trace` black on Bore Twelve (Europa) with terrain only. The tracer now reads
what the renderer drew (`FrameRenderer::set_scene_export` → `FrameExport`: every drawn instance, skinned at
the frame's pose, its cooked material incl. packed ORM, IOR and thin transmission, the frame's sun after the
sun clock) plus the shared draw's terrain (`SharedPassDraw::reference_heightfield`); bottom-level BVHs persist
between traces (`PtCache`). Airless: black sky + the planet's stars, the sun disc, and the sky bodies of § 1.7
as area lights with the frame's fill. Glass is a thin straight-through lobe with the forward's weight; shadow
rays pass it. Measured by `cargo test -p regia_pt` (`tests/airless.rs`, `tests/validation.rs`), lavapipe
and RTX 5090:
| Item | Result |
|---|---|
| Airless fixture (terrain + building + hull, 18 klx sun, Jupiter 14° with 1600 lx fill), 240×135, 48 spp | surface luminance PT 1042 vs raster 1092 cd/m²; whole frame 651 vs 682; Jupiter peak 1311 vs 1307 cd/m²; hero masks 2829/2829 and 1654/1654 px inside their screen boxes; moved camera rebuilds 0 BLAS |
| Clear pane, head-on | 95.9 vs 1 − F(0) = 96.0 (IOR 1.5); 91.7 vs 91.8 (IOR 1.8) |
| Glass shadow | floor under a glass pane: 1.0000 of the bare floor; an opaque pane shadows it |
| Watertight on NVIDIA | rays along a quad's shared diagonal hit (were lost on the 5090: sphere light −3 %, emissive quad −19 %) |

## 2. Metrics: how a real-time technique is scored

All metrics run through one tool, `regia_render::compare` (agent `seeing.compare`, § 5), on linear EXR
inputs, and emit one JSON report plus a FLIP error map PNG. Numbers marked ★ are first guesses, to be
calibrated in the first week the PT exists (§ 4.2 stage 1) by measuring the current renderer and UE's
own plates against the PT; the calibrated values are written into this section with the measurement.

### 2.0 Measured (2026-10-08, PT v2, `docs/evidence/pt/2026-10-08/`)
Twelve DO27 parity plates, 1280×720, 2048 spp, 12 bounces, real-time fog and aerial perspective on (the PT
traces both). Beauty FLIP mean / p95: bore_twelve_wide 0.203 / 0.683, bore_twelve_mid 0.234 / 0.581,
bore_twelve_close 0.365 / 0.679, switchbacks_wide 0.251 / 0.556, switchbacks_mid 0.245 / 0.460,
switchbacks_close 0.281 / 0.431, tunnel_interior 0.307 / 0.778, cockpit 0.021 / 0.117, dusk_vista
0.211 / 0.583, set_piece_borehole 0.325 / 0.732, fireball_20m 0.054 / 0.289, smoke_80m 0.023 / 0.083.
GI in sun shadow (real-time / PT luminance): Mars canyon plates 0.69–1.01×, dusk_vista 0.70×,
tunnel_interior 2.56×, set_piece_borehole 1.63×, Europa plates 1.05–1.35×. The terrain plates' FLIP is
dominated by relief the PT does not carry (sub-metre terrain normals); the § 2.2 ★ targets stay
uncalibrated until the bake also carries the shader's shading normal.

### 2.1 Image metrics (per frame vs the PT at ≥ 1024 spp)
| Metric | Definition | Role |
|---|---|---|
| FLIP (mean, p95) | Andersson et al. 2020, on the AgX-tonemapped 8-bit pair at the viewer's distance/PPD | Primary perceptual error; what workers minimise |
| RMSE (linear) | On linear radiance, per AOV | Energy correctness (exposure, albedo, sky) |
| SSIM | Existing golden metric | Structure; keeps the golden rule set unchanged |
| AOV error | Same three metrics on each AOV pair | Attributes the error to a technique |

First measurement (2026-10-07, `pt_plates`, DO27 content, 1280×720, PT 1024 spp, fog and aerial off; details in
`docs/evidence/pt/2026-10-07/`): beauty FLIP mean 0.26–0.46 (p95 0.59–0.79), rel. RMSE 0.39–0.54. GI in full
sun shadow: the real-time frame is 2.3× the reference on switchbacks_wide (FLIP 0.47) and 1.3× on
switchbacks_close, where it also misses a ridge shadow the reference casts (sun blocked 217 m out). These are
far from the ★ targets. Part of the beauty error is the PT's own v1 gaps (strata, detail normals, relief, Jupiter
fill), so the GI-in-shadow ratio is the number to drive down first.

### 2.2 Per-technique targets (acceptance scenes, § 3.2)
| Area | Scored AOV | Target | Notes |
|---|---|---|---|
| Exposure / tonemap | beauty, mid-grey card | mid grey within ±0.1 EV of PT; ColorChecker ΔE2000 ≤ 2 under the sun preset (measured 2026-11-05: +0.01 EV, worst patch 0.43) | Both frames through the same AgX |
| Direct lighting + materials | direct | mean rel. RMSE ≤ 1 % (RMSE over the reference mean, as `seeing.compare`'s `rel_rmse`; 11.3 % on 2026-11-05, a fail); FLIP ≤ 0.03 (measured 0.0042) | Same BRDF, so this is a plumbing check |
| Sun shadows | shadow | FLIP ≤ 0.05; penumbra width within 20 % of PT at 5 m and 20 m from contact (occluder–receiver distance along the sun ray; measured 1.02×, 0.82×), at 1 m ★ (2.07× on 2026-11-05, a fail); contact gap ≤ 1 px at 1440p (measured 0 px) | PCSS vs disc-sun PT |
| Local-light shadows | shadow (per light) | FLIP ≤ 0.06; no missing shadow for any light that casts in the PT (measured 2026-11-05: point 0.014, spot 0.0096; 98 % / 99 % of the traced shadow covered) | Atlas budget drops are a fail, not a warning (not yet exercised: two lights) |
| GI (diffuse) | indirect_diffuse | mean FLIP ≤ 0.08, p95 ≤ 0.20 ★; **leak:** inside a closed box lit only from outside, mean interior luminance ≤ 0.5 % of exterior ground luminance ★ | Leak is the DDGI/probe failure mode |
| GI (specular) / SSR / probes | indirect_specular | mean FLIP ≤ 0.10 ★; a mirror floor under a lit object shows the object | ADR-041 lost the cockpit floor reflection; that is a fail |
| AO | indirect_diffuse near contact | contact darkening within 15 % of PT in a 10 cm band ★ | Alchemy/GTAO vs PT |
| Atmosphere / aerial | sky, beauty far band | sky-view luminance within 5 % per band; aerial transmittance error ≤ 3 % ★ | Dusk plates failed this by eye |
| Volumetric fog | volumetric | FLIP ≤ 0.10 ★; light-shaft position within 1 px | Froxel vs ratio-tracked |
| Terrain relief | normal, beauty | normal-AOV angular RMSE ≤ 5° vs the PT's displaced surface ★; strata and crust visible at 1 m, 10 m, 100 m by eye | Cook/LOD, not shading |
| Decals | albedo, normal | decal normal/roughness present in the AOVs (not stripped); FLIP on the decal area ≤ 0.06 ★ | r4 failure mode |
| VFX (fireball/smoke) | beauty | by eye only in v1 (PT does not trace particles) | § 3 |
| Volumetric clouds | transmittance, in-scatter, ground light | transmittance and sun map within 0.03 of the trace (`cloud_field` tests); in-scatter reported in stops, no bar until the look changes (ADR-2127) | `cloud_compare` |

**Measured 2026-11-05 (lane spec27-calibration, ADR-2660, lavapipe; `docs/evidence/spec27/2026-11-05-lavapipe.md`).** The
suite `regia_pt/tests/spec27_calibration.rs` (`docs/06` § 16) scores every row above. It passes exposure, ColorChecker, direct FLIP,
sun-shadow FLIP (0.0034), penumbra at 5 m and 20 m, the contact gap, local-light shadows, the sky-view bands and the cloud tests. It
fails these rows, largest miss first:
- fog shaft position: p95 22 px;
- direct rel. RMSE: 11.3 %, all of it in the brightest 1 % of pixels;
- sealed-box leak: 3.87 %;
- specular FLIP: 0.612;
- fog FLIP: 0.48. The froxel fog is lit under a roof that shadows it in the trace;
- AO contact: 0.32× the traced darkening;
- penumbra at 1 m: 2.07×;
- mirror floor: SSR covers 56 % of the traced reflection.

The GI plate rows (trench 0.494, colour room 0.385) and the aerial row (in-scatter worst 20 %) are provisional. Terrain normal,
decals and GI p95 have no metric on the lane scenes yet. A ★ goes only where a run supports the number.

**Measured 2026-11-06 (lane spec27-decal-terrain-rows, ADR-2951, lavapipe; `docs/evidence/spec27/2026-11-06-lavapipe.md`).**
The remaining rows now have metrics. The GI rows are scored, now that the plates cover the view:

- terrain normal: RMSE 0.34° on a rolling heightfield, a pass;
- decal AOVs: a fail. The tracer holds the decal's normal and roughness. The frame's AOV holds the receiver's,
  17.5° and 0.55 off, because the projected decal relights after the forward pass;
- decal FLIP: 0.755, a fail. The bare plane around the decal agrees (FLIP 0.002);
- GI: trench mean FLIP 0.108, colour room 0.141, p95 0.262 (all fails), sealed-box leak 0.023 % (a pass).

**Measured 2026-11-07 (lane sprint-gi-leak-plates-p95, ADR-3701, the test machine lavapipe; head `610dab6c6`).**
Harness truth of the GI rows above. The sealed box through every harness at the
same settings (64 settled probe frames, Desktop High, cache state stated) agrees: `gi_validation`
0.0227 %, the attribution harness 0.0149 % with the static-screen cache on and 0.0236 % with it
off, probes alone 0.0231 % (bound 0.5 %). The 2026-11-05 disagreement (3.87 % against
dyn-gi-remainder's 0.02 %) was the pre-layout probe deficit, not a harness fault. The p95 rows
stand: trench 0.221, colour room 0.262.

**Attribution re-measured 2026-11-10 (lane sprint-fixrev-24-k, ADR-5455, the build server lavapipe), at the
settled frame.** The 2026-11-07 attribution this paragraph carried read "the screen gather corrects
the colour room (0.139 → 0.109)" and "SSAO helps both". That harness built every row on a fresh
renderer at a different frame count (64–68), so each row's probe hysteresis, SSGI temporal state
and static-screen cache age differed, and the 0.109 was the *held early gather* of the
static-screen cache, not the settled gather. The harness now settles one renderer once and toggles
one term per row at that state, every row reading the same number of frames
(`regia_pt/tests/gi_attribution.rs`). The settled rows (FLIP, energy):

| row | trench | colour room |
|---|---|---|
| `probes_ssgi` (held cache, the Desktop High default) | 0.112 (0.947) | 0.109 (0.986) |
| `probes` | 0.112 (0.961) | 0.139 (1.064) |
| `probes_ssgi_no_ao` | 0.104 (0.980) | 0.156 (1.121) |
| `probes_ssgi_no_reuse` (settled) | 0.113 (0.967) | 0.144 (1.093) |
| `probes_ssgi_cache` | 0.118 (0.955) | 0.144 (1.077) |

The settled attribution: the probes carry the plates — `probes` alone scores trench 0.112 (energy
0.961, shadow bias −0.054) and colour room 0.139 (energy 1.064, bias +0.107), and every other row
stands on them. The screen gather at the settled frame does not correct the colour room (0.139 →
0.144, energy +0.029) and leaves the trench alone (0.112 → 0.113, energy +0.006 — the additive
behaviour `gi_radiance_cache` measures); the 0.109 a held view reports is the early gather the
static-screen cache keeps, which happens to sit closer to the reference on this plate (energy
0.986) — a cache artifact, not a gather correction. SSAO is scene-dependent: at the settled frame
it helps the colour room (0.156 off → 0.144 on) and hurts the trench (0.104 off → 0.113 on,
within cross-renderer noise). The radiance cache (hits only, its default) ties the settled plain
gather. What remains of the miss is the probes' own irradiance error (a DDGI term, job 1010): the
colour room's settled probes over-light the room (+6-9 % energy) and the trench's under-read by
4-5 % with a −0.06 shadow bias. One harness caveat the frame lane owns: a held static view keeps
the early gather (frame-lines, `frame/steps/screen_reuse.rs`), so flat numbers can be early light;
every row above states its cache state.

**Measured 2026-11-06 (lane forward-decals-shading, ADR-3255, lavapipe on the test machine; Metal on the Mac gives the
same digits).** With forward decals on (`REGIA_FORWARD_DECALS=1`, default off), both decal rows pass:

- decal AOVs: 0.36° from the decal's normal and roughness 0.001 off (was 17.51° and 0.547), a pass;
- decal FLIP: 0.003, p95 0.005 (was 0.755). The trace over the frame on the footprint is ×1.001 (was ×0.185).

The default (projected) path still fails both. Its ×1.69 brightening is the normal relight dividing by a flipped
geometric N·L (ADR-3255 item 8); with the one-line fix the row reads FLIP 0.166, still a fail on the paint
approximation. `target_decal_rows_pass_with_forward_decals` keeps the forward numbers; the default path keeps its
ignores.

### 2.3 Temporal metrics (TAA, TAAU, the TSR-grade upscaler)
Test: a 2 px/frame horizontal pan and a 1°/frame orbit over a chart scene (line pairs 1–8 px, 1 px lattices,
a moving high-contrast object over a textured ground), 120 frames.
| Metric | Target |
|---|---|
| Motion clarity | MTF50 of the panning line-pair chart ≥ 85 % of the native, un-jittered still ★ |
| Thin geometry | ≥ 90 % of 1 px lattice bars resolved while panning ★ (0.67 TAAU lost the tower lattices) |
| Ghosting | residual energy of the moving object's trail after 8 frames ≤ 2 % of its contrast ★ |
| Disocclusion | no visible smear > 2 px behind a disoccluding edge (FLIP map) ★ |
| Stability | static-scene frame-to-frame FLIP ≤ 0.01 after convergence ★ |
An upscaled frame is accepted only when it meets these at its scale **and** the still meets § 2.2.

**Measured 2026-10-07 (motion-look lane, native TAA, Desktop High; `regia_render/tests/taa_motion.rs`
on the chart, `do27_chase --motion` on DO27's maps, `docs/evidence/motion-look/`):** motion clarity
(MTF50 panning 2.5 px/frame over native) 0.39 → **0.74** (★ 0.85 not met: the history weight that would
reach it, ~0.36 mid-texel, gives up most anti-aliasing in motion; the test floor is 0.70); thin geometry
(1 px bars while panning) 0.31 → **0.98**; ghosting after 8 frames 18.5 % → **0.3 %**; stability (held,
frame-to-frame mean |ΔL| in 8-bit levels, the FLIP stand-in until `seeing.compare` lands) chart 2.5 →
**0.05**, DO27 0.50–0.60 → **0.002**. On DO27 at 1440p the moving frame keeps 0.61–0.90 of the native
frame's gradient energy (was 0.18–0.37) and a held still 0.97–0.98 (was 0.19–0.30).

**Round 2 (motion-look2, 2026-10-07):** motion clarity **0.90** of native, ★ met (history fetch with a
Keys a = −1 cubic: 0.77; plus a display-only, per-axis, motion-scaled unsharp mask in log luminance after
the resolve, `taau_sharpen.wgsl`; no band above native, edge energy ≤ 1.05× native; test floor 0.87).
On DO27 Switchbacks at 1440p the moving frame keeps 0.97 (chase) / 1.02 (pan) / 1.13 (turn) of native
(round 1: 0.66 / 0.76 / 0.68); shimmer after rotation compensation rises 0.78 → 0.93 on the pan (no TAA:
0.96). Held views now converge 32 jitters before the static-screen cache serves them and are
antialiased (share of a 4×4 SS reference's partly covered edge pixels kept 0.00 → 0.88) and bit-stable;
an antialiased still keeps ~0.6 of the aliased native frame's gradient energy on DO27's ground (round 1's
0.97 was one jittered, aliased frame). Cost: TAAU 0.160 → 0.184 ms at 1440p (RTX 5090).

**Measured 2026-11-04 (tsr-quality lane, ADR-2638; the chart through the upscalers at a render scale,
`regia_render::temporal_chart`, `seeing.temporal_chart`; lavapipe, 320 × 180, 120 frames, `depth_velocity` on as in
every shipped preset; `cargo test -p regia_render --test temporal_chart -- --ignored --nocapture chart_table`).**
Clarity and thin geometry for the pan (2 px/frame) / orbit (1°/frame); ghosting is the trail bias (absolute residual
in brackets); smear in output px; stability is held frame-to-frame FLIP (mean |ΔL| in 8-bit levels in brackets).

| Resolve | Clarity ★ 0.85 | Thin ★ 0.90 | Ghosting ★ 0.02 | Smear ★ 2 px | Stability ★ 0.01 |
|---|---|---|---|---|---|
| native TAA | 1.00 / 1.00 | 1.00 / 1.00 | 0.006 (0.040) | 0 | 0.0000 (0.00) |
| **TSR 0.67** | **1.00 / 1.00** | **1.00 / 0.92** | **0.017 (0.042)** | **0** | **0.0093 (0.20)** |
| TSR 0.5 | 0.69 / 1.00 † | 0.98 / 0.65 | 0.027 (0.061) | 0 | 0.0115 (0.25) |
| TAAU 0.67 | 0.43 / 0.48 | 0.44 / 0.40 | 0.000 (0.046) | 4 | 0.0842 (4.87) |
| TAAU 0.5 | 0.31 / 0.39 | 0.00 / 0.51 | 0.000 (0.081) | 3 | 0.1659 (16.60) |
| FSR 3.1 0.67 | 1.00 / 1.00 | 0.66 / 0.57 | 0.018 (0.046) | 3 | 0.0165 (0.71) |
| FSR 3.1 0.5 | 0.54 / 0.55 | 0.52 / 0.37 | 0.042 (0.062) | 3 | 0.1196 (13.41) |

† Likely aliasing, not a resolved band: the line-pair MTF reads aliased contrast as modulation, and this frame's
lattice keeps only 0.65. Not checked by eye.
TSR at 0.67 meets all five targets on the chart, pan and orbit. Without the missed-thin search and the one-layer
restart, with the same harness, it read 0.86 thin on the orbit and 4 px of smear; an earlier 0.70 orbit figure was an
artifact of the lab's per-mesh motion (ADR-2638). Thin geometry counts column contrast only: by eye the orbit's lattice cells keep residual
blotches at the left of the frame, far fewer than before. TSR at 0.5 misses clarity on the pan, thin geometry on the
orbit and ghosting; it is reported, not held to the targets. § 2.3 also requires the still to meet § 2.2, which this
lane did not measure. Desktop High stays native (owner decision).

**Measured 2026-11-09 (v05-tsr-half-close-b, ADR-4859; same harness, lavapipe, `half_scale_probe`).** An output pixel
no jittered sample lands in now keeps the luma a sample inside it last saw where its reprojection is trusted, instead
of being clipped to the one phase all 3 × 3 samples share at 0.5. TSR 0.5: pan clarity 0.69 → **1.00** (the 1 px band
resolved by eye: neighbor-column contrast 0.3 → 137.6 of the native still's 140.4), held stability 0.0115 →
**0.0092**, smear 0; orbit thin geometry 0.65 → 0.68 and ghosting 0.0275 still miss. TSR 0.67 unchanged or better
(orbit thin 0.92 → 0.94, held 0.0092). Finding: the 0.67 orbit's 1 px band is unresolved before and after
(column contrast 23 of 140); its clarity 1.00 is moiré read as modulation by the percentile line-pair contrast.

**Measured 2026-11-09 (v05-tsr-half-close-c, ADR-4934; RTX 5090 and lavapipe).** Young pixels: a restart with no
sample of its own keeps the borrowed nearest sample only as a placeholder, and until it has some weight a pixel takes
only a sample inside it. TSR 0.5 ghosting 0.028 → **0.0199** (lavapipe 0.0195; it was the box's freshly uncovered
ground re-converging from the neighboring stripe's sample, not the box's color), TSR 0.67 0.0176 → 0.0098, pan and
orbit unchanged. TSR 0.5 now misses only orbit thin geometry (0.68). Cause: a missed member is claimed a whole output
pixel per frame, so under the orbit's sub-pixel motion it runs ahead of its true pixel, which restarts as wall. A
sub-pixel lock position reaches about 0.73 at 0.5 but costs 0.67 its 0.90, so it is not shipped and the half-scale
promotion is cut. One 120-frame orbit number moves with the jitter phase it ends on: 0.51–0.68 at 0.5 and 0.86–0.94 at
0.67, over 104–136 frames. `tsr_050_orbit_thin_section_2_3` (ignored, known miss) holds the target for a follow-up.

**Measured 2026-11-14 (v050-tsr-half-finish, ADR-7694; RTX 5090 and lavapipe).** Tracked locks: a lock carries where in
its pixel the thin feature lies, the feature's motion moves that point, and only the pixel it lands in owns the lock and
takes the locked colour unfiltered; a pixel that owns no thin feature keeps other pixels' thin samples out of its clip
box; a moving pixel whose own sample lands on its thin feature takes most of it; a claimed pixel no longer takes a luma
valley for its feature. TSR 0.5 orbit thin geometry 0.683 → **0.905** (57 of 63 bars, one bar of margin; mean over run
lengths 104–136 0.616 → 0.879), TSR 0.67 0.937 → **1.000** (mean 0.911 → 0.994). Pan, ghosting (0.0185 at 0.5, 0.0095 at
0.67) and held stability (0.0090 and 0.0084 on the card, 0.0088 and 0.0083 on lavapipe) unchanged within 0.0001.
Cause at 0.5: a 1 px member is half a render texel wide, the Halton base-2 x jitter alternates texel halves, so a member
moving near an odd number of pixels per frame goes unsampled for 4–8 frames in a row, and whole-pixel claims ran it up
to a pixel ahead. TSR 0.5 now meets every § 2.3 row on this chart; `tsr_050_orbit_thin_section_2_3` asserts it. The TSR
pass costs 0.162 → 0.210 ms (0.5) and 0.177 → 0.225 ms (0.67) at 1920 × 1080 on the RTX 5090, 0.577 → 0.751 and
0.642 → 0.816 ms at 3840 × 2160.

**As built 2026-11-14 (w10-fsr3-ghosting, ADR-7678; lavapipe).** FSR 3.1 at 0.67 meets the ghosting target: 0.0293 →
**0.0188** (residual 0.0516 → 0.0423), held stability 0.0165 → 0.0157, clarity, thin geometry and smear unchanged
(0.0293 was the post-ADR-5890 value; the 0.018 in the 2026-11-04 table predates that ground color). The ghost was
stripe-edge pixels re-converging from their restart: a restart borrowed the 3×3 mean's light (or, half disoccluded,
kept the box's), and three frames later the accumulated pixel's 3σ box let it stay. Now a restarting pixel takes its
nearest sample, a pixel at least half disoccluded restarts (`min_disocclusion_accumulation` −1, SDK −1/3), and
accumulation ramps over four frames (`accumulation_added_per_frame` 1/4, SDK 1/3). FSR 3.1 at 0.5: ghosting 0.0619 →
0.0461, orbit thin 0.365 → 0.381, the rest unchanged or better except the bracketed residual (0.0709 → 0.0765) and
held ΔL (13.41 → 13.53). `fsr3_067_ghosting_meets_spec27` holds the 0.67 target.

**Measured 2026-11-15 (v051-tsr-metal-thin-orbit, ADR-7876; RTX 5090, lavapipe and Metal on a MacBook Air M5).** The 0.5
orbit row passed by one bar on x86_64 and failed on Metal (0.889, bar 290 lost as well). A member is kept or lost row by
row; the lock follow now takes any lock above 0.1 instead of 0.3, so a member whose tracked point moved to the next pixel
is carried there. TSR 0.5 orbit thin geometry: card and lavapipe 0.905 → **0.952**, Metal 0.889 → **0.921** (mean over run
lengths 104–136 0.879 / 0.883 / 0.879 → 0.911 / 0.914 / 0.905); TSR 0.67 unchanged (1.000); pan, ghosting and held
stability identical on each backend; TSR pass time unchanged within noise. Some run lengths still read under 0.90 (112
frames: 0.873 on all three).

**Measured 2026-11-15 (v051-fsr31-reduced-scale, ADR-7875 and ADR-7933).** The FSR 3.1 port matches the SDK v1.1.4
reference pass by pass (audit in ADR-7875); its misses are the algorithm's. ADR-7875's accumulate change (young pixels and
a still pixel's own observation) met 0.67 held stability (0.0157 → 0.0076) and 0.5 ghosting (0.046 → 0.018) but raised
the ghost residual at both scales and the 0.5 smear, and 0.67 still missed thin geometry and smear, so it is held
(ADR-7933) and the FSR 3.1 rows stay ADR-7678's:

| Resolve | Clarity ★ 0.85 | Thin ★ 0.90 | Ghosting ★ 0.02 | Smear ★ 2 px | Stability ★ 0.01 |
|---|---|---|---|---|---|
| FSR 3.1 0.67 (lavapipe / RTX 5090) | 1.00 / 1.00 | 0.66 / 0.57 (0.59) | 0.019 (0.042) | 3 | 0.0157 (0.69) |
| FSR 3.1 0.5 (lavapipe / RTX 5090) | 0.54 / 0.55 (0.53) | 0.52 / 0.38 (0.41) | 0.046 (0.077) | 3 (4) | 0.1184 (13.53) |

Bracketed: the RTX 5090 (the build server) where it differs, and beside ghosting and stability the residual and held ΔL.
`fsr31_ghost_residual_holds_at_067_and_050` pins the residual (≤ 0.05 at 0.67, ≤ 0.08 at 0.5). Thin geometry is the share
of 1 px members the last frame sampled: in motion FSR clips history to its tightest box. FSR 3.1 stays opt-in; no
preset changes.

### 2.4 Hitch metrics (hard gate, every merge from the alpha on)
Measured on the DO27 60 s chase drive and the Sponza/Bistro flythrough, release build, after a 2 s warm-up:
| Metric | Target |
|---|---|
| Pipeline creations, bind-group creations on the frame path, blocking polls | **0** (`TrackedDevice`, ADR-035) |
| Frames > 50 ms | **0** |
| Frames > 2 × p50 | ≤ 0.1 % of frames ★ |
| p99 / p50 | ≤ 1.3 ★; p99.9 / p50 ≤ 2.0 ★ |
| Longest frame | ≤ 2.5 × p50 ★ |
| Streaming | no frame waits on a load (loads are async with a per-frame upload budget) |
| GPU submits per frame | 1 (ADR-037) |

As built (post-v0.5.0): `play.hitch_report` reports the streaming row in `streaming_waits`,
over the measured window — `texture_held_frames` (frames the texture streamer held back mip
uploads, within `MIP_HOLD` of its last out-of-memory signal) and `texture_backing_off_frames`
(frames it backed off after a streamed texture failed to allocate), plus `render_residency`
(`waits`/`timeouts` deltas of the stream render gate's GPU-residency hold, captured when the
warm-up completes; `null` without the render plugin). These are memory-pressure signals, not a
measured per-frame upload budget, and the row is reported but not judged: `pass`/`failures` do
not read it (no numeric target yet).

### 2.5 Frame-time budgets (GPU, Desktop High unless stated; a measured number replaces ★)
| Platform | 1440p | 1080p | Notes |
|---|---|---|---|
| RTX 5080 | ≤ 5.5 ms ★ (180 fps) | ≤ 3.5 ms ★ | Measure when the box GPU is back |
| RX 7900 XTX (RADV) | ≤ 7.0 ms (DO27's bar) | ≤ 4.5 ms ★ | Sponza 1080p p50 1.31 ms at `0404`; DO27 Bore Twelve p50 14.15 / p99 15.45 ms at `0404` is the number to beat |
| Steam Deck, Deck profile (800p, upscaled from ~70 %) | ≤ 25 ms ★ (40 fps cap) | — | Medium look; still no leak, no hitch |
UE comparison: DO27 records the UE v13 frame time on the same drive and the same card; "surpass" means
Regia Engine's p50 and p99 are both lower at equal or better plates.

## 3. The visual gate: UE plates

### 3.1 Protocol
- Cameras from DO27's `ue_camera_spec.json` (12 shots: bore_twelve wide/mid/close, switchbacks wide/mid/close,
  tunnel_interior, cockpit, dusk_vista, set_piece_borehole, fireball_20m, smoke_80m), same EV, same time of
  day, same assets at the same pin. DO27 renders `<shot>_{ue,regia}.png`; Regia Engine builds the sheet and the
  verdict table under `docs/evidence/parity/<date>/` daily (`README.md` + `sheet.png`).
- Added engine scenes (so the gate does not depend on DO27 content): Sponza (Khronos), Bistro if a licence-
  clean copy is available (else Sponza + the material grid), the M15c planet shots, the lookdev shot list.
- **Judge:** an Opus look director reads each pair and writes 2–4 concrete observations per shot in the
  categories of the 2026-10-07 README; the owner's eye is final. Cheap models and metric tests never judge
  look; they only run § 2.
- **"Holds up" per shot:** no severity ≥ 4 row in the verdict table; no placeholder material, missing
  asset, horizon band, terrace stepping, fog wall, binary AO or unlit sprite visible; the Opus verdict says
  "same map, as good or better". "Surpasses" means the owner picks the Regia Engine plate blind.
- Goldens: lavapipe goldens stay the regression net (`docs/06` § 2). A look change re-blesses them with the
  reason; a perf change that moves a golden is rejected or made opt-in (ADR-041).

### 3.2 Acceptance scene list (for § 2 numbers)
`material_grid`, `light_shadows`, `ibl_spheres`, `sponza`, `outdoor` (terrain + sky), `planets`, a closed
box (leak), the temporal chart (§ 2.3), the 12 DO27 shots re-captured through `seeing.capture_plate`.

## 4. Programme and order

### 4.1 Through the alpha (2026-10-07 → 2026-10-15): look-parity sprint
Ranked from `docs/evidence/parity/2026-10-07/README.md`; each row is an existing lane, judged by the Opus
look director against the plates, re-captured at `≥ green-20261007-0732`:
| # | Gap | Lane / item | Acceptance (by eye, plus § 2 where the PT is not needed) |
|---|---|---|---|
| 1 | Terrain relief, strata, terrace stepping, slope tint | `terrain-strata` | strata and crust at 1/10/100 m; no stair-steps on any slope in the 6 terrain shots |
| 2 | No scatter | `scatter-instancing` | rock density and size distribution read like the UE plates in wide/mid/cockpit |
| 3 | Placeholder set-piece materials, pads | `hard-surface`, DO27 asset pin | no flat blue material in any shot |
| 4 | Mars dusk fog wall, haze contrast | M15c leftovers (`followups`) + look director | dusk_vista keeps mesa contrast at 500 m; sky gradient visible |
| 5 | Decals (road/pad ribbons) | `decals` r4 | normal + roughness on decals; crisp pad paint on dust |
| 6 | Interior black shadows, binary AO | `lights-polish` r2 (ADR-034) | tunnel floor shows bounce, soft penumbra, no pure black |
| 7 | Per-map exposure and grade | look director round 2 (grading LUT, TAA sharpness) | switchbacks_close within 0.3 EV of UE; cockpit/tunnel metering |
| 8 | VFX lighting, smoke range | `vfx-quality` r3 | lit fireball with ground light; smoke visible at 80 m |
| 9 | Sky body size/phase/texture | M15c leftovers | Jupiter matches the UE disc size and phase |
Daily: sheet + verdict to the owner. 10-15: all rows severity ≤ 2, DO27 re-measures the chase bench.
Calendar drivers: Grok/DeepSeek rounds (1–3 h each), one Opus look-director round per day, the RX 7900 window
(< 10 min, flock), DO27's re-capture turnaround. Confidence: medium; rows 1 and 4 are the risk.

### 4.2 After the alpha (from 2026-10-18): the excellence programme
Opus-led lanes, one architecture lane at a time, Grok/DeepSeek for plumbing. Each stage ships with its
agent surface and guide page, its § 2 numbers, a plate verdict and a 7900/5080/Deck budget line before the
next starts. Dates are calendar estimates (what drives them: Opus lane rounds of ~2–4 h, the gate at ~40 min,
one GPU window per merge, owner review); they are revised in `progress.md` when measured.
| Stage | Dates | Scope | Acceptance |
|---|---|---|---|
| 1. Reference path tracer | 10-18 → 10-24 | `regia_pt` v1 (§ 1), `seeing.compare` with FLIP, calibration of every ★ | § 1.5 all pass; ★ numbers measured and committed |
| 2. GI | 10-25 → 11-08 | **2a** probe GI (existing DDGI-style field, ADR-031/034) traced against per-mesh SDFs + terrain heightfield, probe visibility (Majercik 2019) so leaks go to the § 2.2 bound, radiance cache for the screen-space pass; runs on the 7900 and the Deck. **2b** ReSTIR GI (Ouyang et al. 2021) for Desktop Ultra behind hardware ray queries once wgpu's ray-query feature is stable on RADV and NVIDIA; SDF tracing stays the fallback. Order: 2a first because every tier needs leak-free GI and neither the 7900 nor the Deck can carry per-pixel RT; 2b is the headline on the 5080 | GI rows of § 2.2; tunnel and cockpit plates; budget lines |
| 3. Visibility buffer + meshlets | 11-09 → 11-22 | meshopt clusters + simplification DAG at cook, per-cluster cull/LOD, visibility-buffer shading, foliage/scatter first (rocks, then props) | `specs/26` § 6 M15d geometry item; scatter plates at 10× the rock count at the same frame time |
| 4. TSR-grade upscaler | 11-23 → 12-06 | temporal upscaler rebuilt around § 2.3 (history rectification by motion-aware variance clipping, thin-feature preservation, disocclusion handling); FSR 2/3 port as the comparison baseline | § 2.3 at 0.67 and 0.5 scale; Desktop High may then default to 0.67 (closes ADR-041's condition) |
| 5. Material graph | 12-07 → 12-20 | `specs/22` T2.4 generating the `material_surface` contract; the PT shades the same graph | glTF extensions and layered materials match PT within § 2.2 |
Stages 2–5 are the former M15d pulled forward (`05-milestones.md` Phase G); baked GI for Mobile/Low stays
after stage 2. Parallel, continuous: the hitch gate (§ 2.4) on every merge, and the daily plate sheet.
Stage 3's far end (ADR-2445, opt-in): past a mesh's last cluster LOD, a cooked sparse-brick voxel proxy
(`voxel_far` in the glTF `.meta`) joins the cluster DAG as coarser levels, so the cut moves onto voxel faces at
the same one-pixel error rule, in the main view and the shadow jobs, instead of an impostor swap
(`book/agents/voxel-far.md`).

### 4.3 Ways Regia Engine surpasses UE (what the programme is aimed at)
Zero hitches as a gate; leak-free low-noise GI across tiers; an upscaler judged on motion clarity, not
stills; foliage-first virtual geometry; mid-range and Deck budgets as first-class targets; agent-native
iteration (every capture, compare and bless is a protocol call); Rust stability; the licence.

### 4.4 Spec amendments
- `specs/13` § 12: add "the reference path tracer (`specs/27` § 1) consumes the same extracted world; a change
  to `pbr_lighting.wgsl` must keep § 1.5 item 4 passing."
- `specs/22` T2.4, T2.8, T2.9: marked "pulled forward into the spec 27 programme (stages 5, 2, 3)".
- `specs/26` § 4 rows marked M15d: milestone column now reads "spec 27 § 4.2 stage n"; § 6 M15d acceptance
  gains "plus `specs/27` § 2 numbers and § 3 plate verdict".
- `05-milestones.md` Phase G M15d: note added (done with this spec).

## 5. Agent-native surface (`specs/23` conventions; names follow the existing `seeing.*` / `play.*` groups)
| Method | What |
|---|---|
| `seeing.reference_trace(scene, spp, width, height, aovs[], seed, denoise?)` → job id; result = EXR paths + preview PNG | Runs `regia_pt`; `scene: current` traces the frame's own camera, so there is no `camera` parameter (ADR-5630); progressive, cancellable; MCP image content for the preview. Behind `regia_agent`'s `reference-trace` feature (a shipped game never links the tracer); `scene` = `current` or a validation scene. CLI: `regia render --reference` (polls the job); offline: `render_lab pt <scene>`, `pt_plates --evidence` (nightly twelve plates). `denoise: true` (feature `denoise`, Intel OIDN) adds a preview, never a reference (ADR-108) |
| `seeing.compare(a, b, mode: flip|rmse|ssim|all, aov?)` → JSON + FLIP map PNG | The § 2 tool; `a`/`b` may be EXR, PNG or a reference job id. Refuses a denoised image (`regia.denoiser` EXR attribute / PNG `tEXt`; the pre-rename key of older files too, ADR-8670) as the reference `a` (ADR-108) |
| `seeing.capture_plate(shot_spec, tag?)` → PNG | Renders one `ue_camera_spec`-style shot through the runtime frame at the current pin; the sheet builder is a CLI over it |
| `seeing.temporal_chart(pan_px, frames)` → § 2.3 report | Runs the chart scene and returns the five temporal numbers |
| `play.hitch_report(seconds, drive?)` → § 2.4 report | Frame-time percentiles, > 2×p50 count, longest frame, `TrackedDevice` counters, streaming waits |
| `render.set_gi_mode`, `render.set_upscaler`, `render.set_reference_overlay(off|blend|wipe|difference|flip)` | Live A/B of a technique against a reference in the viewport. The overlay draws the PNG given in `path` (a trace's `preview_png`), and its `flip` is a per-pixel HyAB heat map, not the FLIP score (ADR-2463, ADR-5630) |
Guide page: `book/agents/renderer-excellence.md` (how to trace, compare, read FLIP, what each ★ means, how
to capture and judge a plate). Writing the page is a lane task in stage 1. `bless_golden` stays owner-only.

## 6. Risks and non-goals
- **Non-goals for the alpha:** no virtual texturing, no hardware RT, no vis-buffer, no PT; the alpha is won
  by § 4.1 on the existing passes.
- **The PT is offline.** It never ships in a game build and never gates a frame at runtime; `regia_pt` is
  not a dependency of runtime crates.
- **Deck has no usable RT:** stage 2b is Ultra-only; the Deck and the 7900 live on 2a. A § 2.2 target that
  the Deck cannot meet gets a measured Deck-specific bound in this spec, not a quiet lowering.
- **wgpu ray queries** may not be stable in time for 2b; the SDF path is the plan of record, 2b the upside.
- **FLIP on AgX output** measures what the viewer sees but hides linear errors; that is why RMSE per AOV is
  reported beside it. A technique must not be tuned to FLIP alone.
- **The PT can be wrong.** § 1.5 (furnace, analytic, Cornell) is the defence; a PT change re-runs all of it.
- **Content vs engine:** missing props, placeholder materials and camera-axis mismatches on the plates are
  DO27's; the verdict table names them so engine lanes are not blamed for them, and vice versa.
- **Budgets ★ are guesses** until measured on the 5080 (box GPU currently off the bus) and the 7900.
