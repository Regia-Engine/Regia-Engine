# Spec 27 calibration suite

The calibration suite scores every per-technique target of `docs/specs/27-renderer-excellence.md` § 2.2 against
the reference path tracer and writes one evidence page: target, measured value, pass or fail, scene, samples per
pixel and commit for each row (ADR-2660). It is how a ★ in § 2.2 becomes a measured number, and how a lane
learns which renderer row misses its target by the most. The latest pages are under `docs/evidence/spec27/`.

This page covers what each row measures, how to run the suite on lavapipe and on a card, how to read the
report, and how an agent scores the same rows on a live frame through `seeing.reference_trace` and
`seeing.compare`.

## What the suite measures

The suite is `crates/regia_pt/tests/spec27_calibration.rs`. Every test in it is `#[ignore]`d, so the merge gate
does not run it. The scenes are built in the file or taken from `regia_test_utils`, with no fetched or DO27
content.

The real-time frame has no AOVs, so the suite isolates each technique with two frames that differ only in that
technique. The tracer's side of each pair is an AOV.

| Row ids | Frame pair | Tracer side | Scene |
|---|---|---|---|
| `exposure_mid_grey` | display pixels | beauty through `compare::display_encode` at the same EV | 0.18 card under the sun |
| `exposure_colorchecker` | display pixels, CIEDE2000 per patch | the same | 24-patch chart |
| `direct_rel_rmse`, `direct_flip` | sun-only frame (LAB quality) | `direct` | `material_grid` |
| `sun_shadow_flip` | frame / frame without sun shadows | `shadow` (disc sun) | wall + cube overview |
| `sun_penumbra_1m`, `_5m`, `_20m` | the same, 10–90 % edge width | the same | wall edge, top-down windows |
| `sun_contact_gap` | the same at 2560×1440 | the same | 1 m cube |
| `local_shadow_flip_point`, `_spot`, `local_shadow_present` | frame / frame with the light's shadow off | `direct` with / without the caster | `light_shadows` |
| `ao_contact` | ground with the cube / open ground, 10 cm band | `beauty` the same way | 1 m cube, uniform sky |
| `fog_flip`, `fog_shaft_position` | beauty of a black room | `beauty` with the fog medium | slit room |
| `atmosphere_sky_bands` | HDR sky | `sky` from the same planet | Earth sky |
| `specular_mirror_floor` | frame with SSR − frame without | `indirect_specular` | mirror floor + red sphere |
| `decal_aovs` | the frame's normal and roughness AOVs (`seeing.frame_aov`) on the decal footprint, against the decal's analytic values | `normal`, `roughness` with a `PtDecal` | grey plane + one projected decal |
| `decal_flip` | HDR beauty with the `regia_vfx` decal layer on | `beauty` with the same decal | the same |
| `terrain_normal` | the terrain shader's shading normal (`TerrainSharedDraw::render_normal_view`) | `normal` of the traced heightfield | 129² rolling heightfield |

These rows come from the logs of older tests:

- `gi_diffuse_*` and `gi_leak_sealed_box` come from `gi_validation` (see [GI validation](gi-validation.md)).
  `gi_diffuse_p95` is the worst scene's `<scene> / probes_ssgi: p95 FLIP` line (ADR-2951).
- `specular_flip` comes from `sssr_specular`.
- `atmosphere_aerial` comes from `aerial_parity`.
- `clouds` comes from `cloud_field`.

A row whose source log is missing is recorded as `not measured`, with the reason. Every § 2.2 row now has a
metric on the lane scenes (ADR-2951 added the decal rows, `terrain_normal` and `gi_diffuse_p95`).

### The decal and terrain rows (ADR-2951)

- **`decal_aovs`** passes only when the decal's normal and roughness are in the AOVs of both renderers. "In the
  AOV" means within 2° of the decal's analytic normal (twice the 8-bit readback's worst case) and within 0.02 of
  its roughness, over the eroded decal footprint. The tracer applies the decal to the hit's material inputs, so
  its `normal` and `roughness` AOVs hold them. The frame's AOV is what the forward shader shaded with
  (`seeing.frame_aov`). The projected decal is `regia_vfx`'s layer, composited after the forward pass: it relights
  the lit colour from its own normal and adds a roughness-shaped lobe, and the forward pass never sees it. So
  on the footprint the frame's AOV holds the receiver's normal and roughness, and the row fails with the decal
  stripped. The measured cell gives both renderers' errors, the frame's distance from the receiver's values,
  and the frame AOV's own error on the bare plane, which shows that the capture itself is sound.
- **`decal_flip`** is the FLIP of the frame's HDR beauty against the trace's over the decal footprint. The
  measured cell also says how much the decal changed the frame there, which shows that the layer drew.
- **`terrain_normal`** is the angular RMSE between the terrain shader's shading normal (CDLOD mesh, the normal
  map built from the heightfield, relief and detail normals) and the traced heightfield's interpolated normal.

## Running it

The commands below are the lavapipe run from the repository root. Point `VK_ICD_FILENAMES` at the lavapipe ICD
(`/usr/share/vulkan/icd.d/lvp_icd.json` on the build server).

```sh
export REGIA_SPEC27_OUT=/tmp/spec27 REGIA_SPEC27_COMMIT=$(git rev-parse --short HEAD)
mkdir -p $REGIA_SPEC27_OUT/logs
for t in gi_validation sssr_specular aerial_parity cloud_field; do
  cargo test -p regia_pt --test $t -- --nocapture --test-threads=1 > $REGIA_SPEC27_OUT/logs/$t.log 2>&1
done
cargo test -p regia_pt --test spec27_calibration -- --ignored --nocapture --test-threads=1 --skip spec27_write_report --skip target_
cargo test -p regia_pt --test spec27_calibration -- --ignored --nocapture --exact spec27_write_report
# the red confirmations: every target_ test must FAIL, each with its reason
cargo test -p regia_pt --test spec27_calibration -- --ignored --test-threads=1 target_
cp $REGIA_SPEC27_OUT/report.md docs/evidence/spec27/$(date +%F)-lavapipe.md
```

### On a card

A card run uses the same commands with the card pinned, and the page is named for the adapter. On the build server, pin
one reserved card by UUID, or queue the run through the GPU job supervisor (`docs/06` § 12). Never use card
index 5 or 6.

```sh
nvidia-smi --query-gpu=index,uuid,name --format=csv            # pick a free card's UUID
unset VK_ICD_FILENAMES VK_LOADER_DRIVERS_SELECT WGPU_ADAPTER_NAME
export REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1
export REGIA_SPEC27_OUT=/tmp/spec27-card REGIA_SPEC27_COMMIT=$(git rev-parse --short HEAD)
mkdir -p $REGIA_SPEC27_OUT/logs
# the same four log runs and two suite runs as above, then:
cp $REGIA_SPEC27_OUT/report.md docs/evidence/spec27/$(date +%F)-rtx5090.md
```

Through the supervisor, build the test binary first:

```sh
cargo test -p regia_pt --test spec27_calibration --no-run
```

Then queue one job per step with `# card=nvidia`. The job's command line is the built binary followed by
`--ignored --nocapture --test-threads=1 --skip spec27_write_report --skip target_`, with `REGIA_SPEC27_OUT` set
in the job. The report header names the adapter the device picked, so check it before you commit the page.

## Reading the report

Each row shows the following:

- **Target**: the § 2.2 text. A ★ means it is still uncalibrated.
- **Measured**: the measured value with its units.
- **Result**: one of four values:
  - `pass`
  - `fail`
  - `provisional`: measured, but the scene or metric is known to be incomplete. The note says why. For example,
    `atmosphere_aerial` scores the in-scatter only, not the transmittance its target names. (The GI plate rows were
    provisional until `lane/sprint-dyn-gi-remainder` fixed the plates' coverage. They are pass or fail since
    ADR-2951.)
  - `not measured`
- **Scene** and **spp**: the scene and the samples per pixel of the reference.
- **Commit**: the commit the run was made on.

After the table, the failing rows are listed with the largest miss first. The miss is how many times past its
bound a row is, which ranks the fixes for later lanes. A lavapipe number is a lavapipe number. Card numbers go in
their own page.

Each failing row has a `target_<row>` test that asserts the § 2.2 target. It is ignored, and the ignore reason
gives the measured value and the size of the miss. The lane that fixes the row removes the `#[ignore]`. Do not
loosen the bound (AGENTS.md rule 4).

## The same rows on a live frame (agent protocol)

An agent can score a live frame against a trace of the same view without the test harness. Trace the view,
and compare it with the frame: a display-encoded PNG from `seeing.screenshot`, or a linear EXR that the host
wrote from `FrameRenderer::request_hdr_capture`. `seeing.compare` accepts either.

The protocol has no method that writes the frame's HDR. An agent-only run therefore scores the beauty through
the display transform. The AOV rows below need a host that writes the EXR.

```json
seeing.reference_trace { "scene": "current", "width": 640, "height": 360, "spp": 1024, "max_bounces": 6, "wait": true }
→ { "job": 1, "status": "done", "exr": ".../reference.exr", … }
seeing.compare { "a": { "job": 1 }, "b": "/tmp/frame_hdr.exr", "aov": "beauty", "ev100": 12.5 }
→ { "flip_mean": 0.041, "flip_p95": 0.12, "rel_rmse": 0.08, … }
```

- **Direct lighting**: capture a frame with the sun only (no sky, GI or AO), then compare with `"aov": "direct"`.
  The § 2.2 bounds are `rel_rmse` ≤ 0.01 and `flip_mean` ≤ 0.03.
- **GI**: compare the full frame minus the direct-only frame with `"aov": "indirect_diffuse"`
  ([GI validation](gi-validation.md)).
- **Shadows, AO, fog and the contact gap**: these need the frame pairs in the table above. Use the suite rather
  than a hand-made pair.
- The FLIP of a shadow row is taken on the visibility image (0–1 as grey), not on the beauty.
- **Normal and roughness**: `seeing.frame_aov { "aov": "normal" }` (or `"roughness"`, with an optional
  `rect`) returns the frame's shading normal or roughness with statistics over the rect
  ([Frame AOVs](frame-aov.md)). On the tracer's side the `normal` AOV and `PtImage::roughness` hold the
  same values, decals included.

Error codes are the ones of `seeing.reference_trace`, `seeing.compare` ([Renderer excellence](renderer-excellence.md))
and `seeing.frame_aov` ([Frame AOVs](frame-aov.md)). The suite itself adds no protocol method.
