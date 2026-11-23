# BRDF energy model

A surface should never reflect more light than it receives. The shipped BRDF breaks that rule
at grazing angles. A white dielectric seen nearly edge-on reflects up to 1.12 times the incident
energy with the material's `multi_scatter` flag, and 1.68 times without it. The look-measure
audit found it, and `regia_pt`'s `cpu_brdf.rs` tests pin those bounds. The rims of light-coloured
plastic, paint and stone come out too bright, most in an even, bright environment.

ADR-2632 adds an energy-conserving model next to the shipped one:

| Model | What it does |
|---|---|
| `legacy` (default) | The shipped BRDF. The diffuse is Lambert × (1 − F(l·h)). With `multi_scatter`, the GGX lobe takes Turquin's scale and the diffuse takes 1 − the single-scatter albedo. Every shipped frame and golden image uses it. |
| `compensated` | Kulla & Conty 2017. The multiple-scattering lobe is added as a reciprocal term. The diffuse is coupled to the whole specular layer at both the view and the light cosine. The image-light diffuse takes the share the split-sum specular and its multiple scattering leave, but only where the shader adds that reflection. |

Under `compensated`, a white furnace returns at most 1 + 0.01 at every view angle (n·v 0.05 to
0.95), roughness (0.05 to 1) and metallic value, with and without `multi_scatter`. The 0.01 is
the 8-bit, 32 × 32 DFG table. The model is reciprocal: f(v, l) = f(l, v). White materials with
`multi_scatter` stay at 0.95 or more, so the excess is not traded for a deficit. The CPU sweep and
the path tracer's furnace on lavapipe check all of this. See `docs/evidence/brdf-energy/` for the
before and after plates.

The new model changes every lit pixel, so it is opt-in. Nothing changes until you choose
`compensated`.

## Where it applies

- **Forward shader.** `lab_forward.wgsl` with the define `BRDF_ENERGY_COMPENSATED` covers the
  sun, the local lights (the diffuse coupling sits at the light's centre for sphere lights) and
  the image-light, probe and lightmap diffuse. Area emitters (LTC) and the glass front layer keep
  the legacy form.
- **Visibility buffer.** The resolve is built from the forward shader and takes the same define.
- **Where the diffuse gives way.** The image-light diffuse gives up only what a reflection
  returns beside it. A flat ambient (no image light), an interior's neutral floor, and the share
  of the light no probe reflection covers keep the whole diffuse, as in `legacy`. Without that
  rule, a white sphere under a flat ambient lost the whole Fresnel share at its rim and nothing
  came back as a reflection. The raster furnace test checks that these frames match `legacy`.
- **Lite (Mobile) shader.** `forward_mobile.wgsl` has a cheap form. Every diffuse term that has
  a reflection beside it is multiplied by 1 − (F0·scale + bias) from its analytic split-sum fit.
  The floor, the flat ambient and an interior lightmap keep the whole diffuse. There is no table
  fetch and no cost per light. This form is not reciprocal.
- **Path tracer.** `PtScene::brdf_energy` sets it (`PtScene::new` takes the process-wide model), and the
  kernel reads it from `globals.dfg.z`.
  The tracer runs the same `pbr_brdf.wgsl::with_compensation` as the forward pass.
- **CPU mirror.** `regia_render::brdf_energy::compensated_terms` and
  `regia_pt::cpu_brdf::brdf_cos_energy` / `directional_albedo_energy` mirror the shader math.

## Methods

- `render.brdf_energy` reads the current state:
  - `mode`: the process-wide model that forward modules compose with.
  - `env`: the value of `REGIA_BRDF_ENERGY`.
  - `profile`: the render profile's `brdf_energy` field.
  - `preset`: the preset the profile still matches. Opting in does not make the profile custom.
  - `applies_to`: what a change reaches.

  `profile` and `preset` are `null` when the app has no `RenderSettings`.
- `render.set_brdf_energy {"mode": "compensated"}` sets the process-wide model and the profile
  field, then answers with the new state.

```json
{"method": "render.set_brdf_energy", "params": {"mode": "compensated"}}
{"method": "render.brdf_energy", "params": {}}
```

A change reaches every forward module composed after it. Pipelines a renderer has already built
keep their model until they are rebuilt. To render a whole session with the new model, set it
before the first frame with `REGIA_BRDF_ENERGY=compensated`.

## Errors

| Code | Meaning |
|---|---|
| `brdf_energy_bad_mode` | `mode` is missing, or is not `legacy` or `compensated` |

## From Rust

<!-- ignore-reason: a fragment of a host app; `settings` and `material` are the host's own -->
```rust,ignore
use regia_render::{BrdfEnergy, set_process_brdf_energy};

set_process_brdf_energy(BrdfEnergy::Compensated); // before the renderer builds its pipelines
// Or in the `RenderSettings` the app starts with: `RenderPlugin` reads it at startup and sets the
// process model. preset() keeps its row. A `legacy` profile leaves the process model alone.
settings.profile.brdf_energy = BrdfEnergy::Compensated;

let mut scene = regia_pt::scenes::furnace_sphere(material, 64);
scene.brdf_energy = BrdfEnergy::Compensated; // the path tracer, per scene
```

## Known limits

- The renderer's own pipeline key does not follow the profile field yet. The profile is read once
  at startup, the model is process-wide, and a change mid-session reaches only modules composed
  after it (ADR-2632 lists the hookup). Until then, a mid-session change can mix the two models in
  one frame: opaque surfaces keep the model their pipeline was built with, while variants composed
  later (subsurface, thin film, decals, the visibility-buffer resolve) take the new one. The lite
  tier's module cache does not key on the model, so a lite renderer keeps its first one. Set the
  model before the first frame.
- Clearcoat, sheen, area emitters and the glass front layer keep their own energy terms.
- A thin-film material's diffuse coupling uses the film's Schlick F0 at the view angle, not the
  film's Fresnel at each light, so a thin-film material is not reciprocal under `compensated`.
- The lite form scales by the view only, so it is not reciprocal.
