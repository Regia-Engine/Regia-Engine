# 30 — Post volumes and the physical camera

Status: v0.1.28 lane `sprint-post-volumes-camera` (ADR-1464, proposed); lens effects § 9 (ADR-1863, proposed);
convex hulls, per-volume LUTs, gizmo data and format v3 § 10 (ADR-2323, proposed). Extends `13-renderer.md` § 5 step 12
(post stack) and § 9 (cameras and views). Agent surface: `book/agents/post-volumes.md`. User guide:
`book/user-guide/post-volumes.md`.

This page is the design. It was written from public sources only (`docs/01-cleanroom.md` § 2): user-facing
documentation of what post-process volumes and eye adaptation do (cited in `docs/provenance.md`), the photographic
exposure definitions of ISO 2720 as described in the Filament documentation ("Physically based camera") and in
Lagarde and de Rousiers, "Moving Frostbite to PBR" (SIGGRAPH 2014 course notes, § 5.1), and the thin-lens model the
depth of field already uses (`dof.rs`). The blending rules below are Regia's own.

## 1. What a user gets

- **Post volumes.** A `PostVolume` component on an entity changes the look of every camera whose eye is inside it
  (or inside its blend band): exposure, the auto-exposure band and meter, tonemap, bloom, the colour grade, vignette,
  grain, local exposure, ambient occlusion strength and a fog tint. Each setting is an *override*: a volume changes
  only the settings it names, and everything else keeps the value from the project, the level look and the camera.
  A volume is a box, a sphere, or *unbound* (it covers the whole world, used for a level-wide look that a smaller
  volume can override).
- **Priority, weight and blend distance.** Where volumes overlap, the higher priority wins at full weight. The blend
  distance is a band outside the shape across which the volume fades in, so walking through a doorway is a smooth
  transition and not a cut. The weight (0–1) scales the whole volume.
- **Eye adaptation targets.** A volume may also name an EV range and adaptation speeds for the eye. This is the same
  behaviour as an exposure zone (ADR-257, ADR-322), and volumes and zones combine into one eye.
- **Physical camera exposure.** `PhysicalCamera` gains an exposure mode: `Inherit` (today's behaviour: auto exposure,
  or the fixed EV), `Manual { ev100 }`, or `Physical` — the exposure comes from the lens's ISO, shutter time and
  aperture, the shutter time also sets the motion blur, and the aperture also sets the depth of field. An optional
  focal length sets the field of view from the sensor height.

## 2. Data

### 2.1 `PostVolume` (component, reflected, scene format `post-volume`, v3 since ADR-2323; v1 when introduced)

| Field | Type | Default | Meaning |
|---|---|---|---|
| `format_version` | `u32` | `3` | `POST_VOLUME_FORMAT_VERSION` when written by this engine (§ 6; 2 since ADR-1863 added the optional lens overrides, 3 since ADR-2323 added the convex hull and `lut` (§ 10); older versions read unchanged). |
| `enabled` | `bool` | `true` | A disabled volume contributes nothing. |
| `shape` | `PostVolumeShape` | `Box { half_extents: [1, 1, 1] }` | `Box { half_extents }`, `Sphere { radius }`, `ConvexHull { planes }` (§ 10.1) or `Unbound`. Local space; the entity's `GlobalTransform` places and scales it. |
| `priority` | `i32` | `0` | Higher tiers are applied after lower ones (§ 3). |
| `blend_distance` | `f32` | `0` | Meters outside the shape across which the weight rises from 0 to 1. 0 (or negative, or not finite) is a hard edge. Ignored by `Unbound`. |
| `weight` | `f32` | `1` | Scales the volume, clamped to 0–1; not finite counts as 0. |
| `overrides` | `PostOverrides` | all `None` | The settings this volume changes (§ 2.2). |
| `lut` | `Option<String>` | `None` | A colour LUT the volume blends into the view's (§ 10.2; format v3, off unless the host turns volume LUTs on). |

### 2.2 `PostOverrides` (every field optional; `None` = this volume does not touch it)

| Field | Type | Blend space | Applies to |
|---|---|---|---|
| `exposure_compensation` | `f32` EV | linear | `PostSettings::exposure_compensation` |
| `exposure_min_ev100`, `exposure_max_ev100` | `f32` EV100 | linear | the auto-exposure band |
| `meter_center_weight` | `f32` 0–1 | linear | `ExposureMeter::center_weight` (0 = average, 1 = centre-weighted) |
| `tonemap` | `TonemapOperator` | discrete (§ 3.3) | `PostSettings::tonemap` |
| `contrast`, `saturation` | `f32` | linear | the default look |
| `bloom_intensity` | `f32` 0–1 | linear | `PostSettings::bloom_intensity` |
| `white_balance_kelvin` | `f32` K | mired (10⁶ / K) | `ColorGrade::white_balance_kelvin` |
| `tint` | `f32` −1..1 | linear | `ColorGrade::tint` |
| `lift`, `gamma`, `gain` | `[f32; 3]` | linear per channel | `ColorGrade` |
| `vignette`, `film_grain` | `f32` 0–1 | linear | `PostSettings` |
| `local_shadow_contrast`, `local_highlight_contrast`, `local_detail_strength` | `f32` | linear | `LocalExposure` (starting from the identity when the view has none) |
| `ao_intensity` | `f32` 0–4 | linear | `PostSettings::ao_intensity` (new; scales the screen-space AO strength, 1 = unchanged) |
| `fog_tint` | `[f32; 3]` | linear per channel | `PostSettings::fog_tint` (new; multiplies the view's fog colour, `[1, 1, 1]` = unchanged) |
| `eye` | `EyeRange { ev_min, ev_max, adapt_up_speed, adapt_down_speed }` | exposure-zone rules (§ 4) | the main view's eye |
| `lens_flare_intensity`, `lens_flare_threshold`, `lens_dirt_intensity`, `chromatic_aberration` | `f32` | linear | `PostSettings::lens_effects` (format v2, ADR-1863; § 9) |

A value that is not finite is treated as `None`, and so is a `white_balance_kelvin` of 0 or below. Bloom has no
threshold setting in Regia (the bloom is an energy-conserving blend), so there is none to override.

### 2.3 `PostSettings` gains two fields

`ao_intensity: f32` (default 1) and `fog_tint: [f32; 3]` (default `[1, 1, 1]`). With the defaults every frame is
bit-identical to before. The frame multiplies the SSAO strength by `ao_intensity` and the view's fog colour by
`fog_tint`; both are per view.

### 2.4 `PhysicalCamera` gains exposure and focal length

| Field | Default | Meaning |
|---|---|---|
| `exposure` | `CameraExposureMode::Inherit` | `Inherit`, `Manual { ev100 }` or `Physical` (§ 5). |
| `iso` | `100` | Sensor sensitivity (ISO arithmetic speed). |
| `shutter_seconds` | `1/60` | Shutter time. |
| `shutter_reference_fps` | `30` | Frame rate the shutter time is measured against for motion blur in `Physical` mode. |
| `focal_length` | `None` | Meters. `Some(f)` sets the view's vertical field of view from `sensor_height`. |

Existing fields keep their meaning; `aperture_f_stops` is the aperture of both the depth of field and, in `Physical`
mode, the exposure.

## 3. Blending (deterministic)

### 3.1 Weight at a point

For a volume `v` and a camera eye `p` (world space):

```
spatial(v, p) = 1                                   for Unbound
              = zone_weight(sd(v, p), blend_distance) otherwise
w(v, p)       = enabled · clamp(weight, 0, 1) · spatial(v, p)
```

`sd` is the signed distance (positive outside) of the exposure-zone module (`zone_signed_distance`): the box is
exact under any scale on each axis; the sphere is the zone capsule with a zero half height, so its radius scales with
the mean of the entity's X and Z scale. `zone_weight` is 1 inside and on the surface, a Hermite smoothstep across the
band, and 0 beyond it. A volume whose `format_version` is newer than the engine's is skipped (§ 6).

### 3.2 Tiers

The base is the view's post settings before volumes: the project's `RenderSettings::post`, then the level look
(`apply_level_look`), then the camera's `LensEffects` component. The camera's exposure pin and its lens apply after the
volumes (§ 10.3 gives the whole order). Volumes with `w > 0` are grouped by `priority` and the tiers are applied from
the lowest priority to the highest. Inside one tier, for each field `f`, over the volumes `S` that set `f`:

```
W    = Σ w_i                    (i in S)
mean = Σ w_i · x_i / W          (in the field's blend space)
α    = min(1, W)
x    ← x + α · (mean − x)
```

Consequences, all intended:

- **Order-independent inside a tier.** Equal-priority volumes are weight-averaged, so the result does not depend on
  entity or query order. Two equal-priority volumes both at full weight give their average.
- **A full-weight higher tier replaces lower tiers** for the fields it sets, and leaves the others alone.
- **Continuous in the camera position.** The weights are continuous (smoothstep band) and every step is a lerp, so a
  camera moving through a blend band never sees a jump in any continuous field.
- **Saturation.** Weights that sum past 1 inside a tier are normalised (the mean), not added.

### 3.3 Discrete fields

`tonemap` cannot be interpolated. Inside a tier, if `W ≥ 0.5` the field takes the value of the volume with the largest
`w` (ties: the lowest stable key, the entity bits); otherwise it keeps the current value. It therefore switches once,
at the middle of the band.

### 3.4 Sanitising after each tier

A field is clamped right after each tier that sets it (so a later, partial tier starts from a clamped value); a field
that no volume sets keeps the base value as it is (ADR-5631). The ranges: `contrast ≥ 0.05`, `saturation ≥ 0`,
`bloom_intensity`, `vignette`, `film_grain`, `meter_center_weight` in 0–1, `ao_intensity` in 0–4, `fog_tint` channels
≥ 0, the three EV fields (`exposure_compensation`, the band's ends) in ±64, `tint` and each `lift` channel in −1..1,
`gamma ≥ 0.05`, `gain ≥ 0`, the lens-effect strengths in the ranges of § 9, the local exposure factors in the ranges
of `LocalExposure::sanitized` (shadow and highlight contrast 0–1, detail 0–4; an identity result turns local exposure
off). White balance: each volume's kelvin is clamped to 1667–25 000 before the mired mean, the current value likewise,
and the result again. A band whose ends cross is used as the ordered band (the auto-exposure code already orders it).
Tested in `tests/post_volumes_conformance.rs`.

### 3.5 Per camera

Every camera, window or texture, evaluates the volumes at its own eye, in `extract_frame`. Split-screen views and
render-to-texture views each get their own blend. The work is `O(cameras × volumes)` with a reused scratch vector;
no per-frame allocation once it has grown.

## 4. Eye adaptation targets (exposure zones folded in)

A volume whose overrides set `eye` acts as an exposure zone with the same weight `w(v, p)` and the same priority: the
framework system that weighs exposure zones at the active camera (`sync_exposure_zones`, ADR-322) also gathers these
volumes, and `combine_weighted_zones` combines both kinds (highest priority wins, saturated weights, the open band for
any uncovered weight). The eye therefore stays one eye: a tunnel can be an exposure zone, a post volume with an `eye`,
or both. As with zones, this applies to the main window view only, and manual or physical exposure ignores it.

## 5. Physical camera exposure

- `Inherit`: the lens does not touch exposure (the behaviour before this page).
- `Manual { ev100 }`: auto exposure off, the fixed EV100 is `ev100`.
- `Physical`: auto exposure off and

```
EV100    = log2(N² / t) − log2(S / 100)        (N f-stops, t seconds, S ISO; ISO 2720 reflected-light form)
exposure = 1 / (1.2 · 2^(EV100 − compensation))  (saturation-based sensitivity, the existing exposure_from_ev100)
```

  Sunny 16 (f/16, 1/125 s, ISO 100) is EV100 ≈ 15. The inputs are floored (`N ≥ 0.1`, `t ≥ 1 µs`, `S ≥ 1`), so a
  zeroed or negative lens gives a finite exposure. Exposure compensation still applies (a volume's compensation
  shifts a physical camera the way a photographer's dial does). In `Physical` mode the motion blur's shutter fraction
  is `shutter_seconds × shutter_reference_fps` instead of `shutter_angle` (1/60 s at 30 fps = 0.5, a 180° shutter).
  The depth of field already uses `aperture_f_stops`, so stopping down both darkens the frame and deepens the focus.
- The lens mode is applied after `Camera::exposure_ev`, so a lens in `Manual` or `Physical` mode wins over the camera's
  EV pin; `Inherit` leaves the pin in charge.
- `focal_length = Some(f)` sets the view's vertical field of view to `2 · atan(sensor_height / (2 f))` (the inverse of
  `regia_render::focal_length`), so the depth of field's circle of confusion and the framing agree.

Circle of confusion (unchanged, `DepthOfField::circle_of_confusion`): the thin-lens diameter as a fraction of the
image height, `f² / (sensor_height · N) · |d − s| / (d · (s − f))` for depth `d` and focus distance `s`.

## 6. Format and migration

`PostVolume` lives in scene files (`.level.ron`, `.prefab.ron`) as the reflected component
`"regia_render::post_volumes::PostVolume"` (a pinned type path). It is a frozen content format from v0.1.28:
`post-volume` in `cargo xtask check-formats` (v1 then, v3 now) (`post_volume_schema_fingerprint`: every reflected field and type of
`PostVolume`, `PostVolumeShape`, `PostOverrides` and `EyeRange`, plus the variant names of `TonemapOperator`). Rules:

- A component without `format_version` (hand-written) loads as v1 (`PostVolume::migrate` maps 0 to 1).
- A component with a newer version is skipped by the renderer with one warning, not misread.
- A shape change bumps `POST_VOLUME_FORMAT_VERSION`, adds the step to `PostVolume::migrate` and its source to
  `post_volume_migration_sources`, and re-records the hash (`cargo xtask check-formats --update`). Like the primitive
  component, the migration runs where the component is read; a v2 that renames fields must also teach
  `regia upgrade-project` to rewrite the component inside scene files.

Example (inside a scene entity's `components` map):

```ron
"regia_render::post_volumes::PostVolume": (
    format_version: 1,
    enabled: true,
    shape: Box(half_extents: (6.0, 3.0, 10.0)),
    priority: 1,
    blend_distance: 2.0,
    weight: 1.0,
    overrides: (
        exposure_compensation: Some(0.5),
        white_balance_kelvin: Some(3600.0),
        saturation: Some(1.1),
        fog_tint: Some((1.0, 0.85, 0.7)),
        eye: Some((ev_min: 6.0, ev_max: 8.0, adapt_up_speed: 2.0, adapt_down_speed: 1.5)),
    ),
),
```

The editor inspector edits it through reflection; agents use `scene.add_component` or the dedicated
`render.set_post_volume` (§ 7).

## 7. Agent protocol

- `render.post_volumes { camera?, point? }` (read): every volume (entity, enabled, shape, priority, weight, blend
  distance, the names of the fields it overrides), the volumes that contribute at the camera's eye (or at `point`)
  with their effective weights in tier order, and the resolved values of every overridable field there.
- `render.set_post_volume { entity, ... }` (edit scene): adds or edits a volume, validated before any field is
  applied; `overrides` takes a partial object where `null` clears a field.
- `render.get_physical_camera { entity? }` (read): the lens of a camera (default: the active window camera), with the
  derived EV100, exposure mode, effective shutter fraction and vertical field of view.
- `render.set_physical_camera { entity?, ... }` (edit scene): sets the exposure mode, ISO, shutter, aperture, focal
  length, sensor height, focus distance and depth-of-field kernel, inserting the component when the camera has none.
  On the wire the focal length and sensor height are millimeters (`focal_length_mm`, `sensor_height_mm`); the
  component stores meters.

Error reasons (`data.reason`) are listed in `book/agents/post-volumes.md`.

## 8. Not in scope (v0.1.28)

- Post materials and arbitrary user passes in volumes. (Per-volume LUT blending arrived with ADR-2323, § 10.2.)
- Overrides of the lens effects (motion blur, depth of field) by volumes: they belong to the camera.
- A bloom threshold (Regia's bloom has none). Chromatic aberration, lens flares and dirt were out of scope here;
  they arrived with ADR-1863 (§ 9), with volume overrides in format v2.
- Volume shapes other than box, sphere, convex hull (§ 10.1) and unbound; a hull built from a mesh is an
  authoring step (the editor or a tool writes its planes), not a shape.
- A measured-frame-rate shutter: `shutter_reference_fps` is fixed, so the blur is deterministic for tests and
  render-out.

## 9. Lens effects (ADR-1863)

`regia_render::lens_effects` draws a ghost lens flare, lens dirt and lateral chromatic aberration over the
scene-linear HDR image, as the registered frame pass `lens_effects` at the `before_post` slot (before the camera's
motion blur and depth of field, exposure, bloom and the tonemap).

- **Settings.** `LensEffects { flare_intensity, flare_threshold, coating, dirt_intensity, chromatic_aberration }`;
  every strength 0 (`LensEffects::OFF`, the default) is off and the pass records nothing. The view's value starts at
  the project's `PostSettings::lens_effects`; a `LensEffects` component on the camera replaces it; post volumes
  override the four fields of § 2.2 (blended from the camera's value like every scalar, clamped to: intensity
  0–1000, threshold 0–65 504, dirt 0–10, aberration 0–0.05); then `gate_lens_effects` turns it off unless
  `RenderProfile::lens_effects` (Desktop High, Desktop Ultra, cinematic).
- **Ghosts.** A built-in synthetic lens is traced with the reference (`post_ref::lens_flare`) once per coating; the
  most visible ranked two-bounce ghosts (energy per footprint area; at most 8, half of them held for ghosts across
  the frame centre from the light, `m < 0`) keep, per red/green/blue probe wavelength,
  the landed energy (tinted by a quarter-wave single-layer film on coated air-glass bounces, each surface's film
  tuned to its own wavelength, so ghosts differ in colour) and the
  magnification `m` (a light at centred frame position `c` puts the ghost at `m · c`), and the footprint radius. The
  GPU reduces the thresholded image to a 16 × 9 grid of cells (energy, luminance-weighted centroid, RMS spread) and
  draws, per lit cell and ghost, a sprite of the six-bladed iris at `m · c` with the channel's own `m`, the cell's
  energy spread over the ghost's area (a cell's spread widens and softens the sprite, so a broad source stays
  continuous), times the traced energy and `flare_intensity` (1 = the traced energy). The
  part of a pixel's exposed luminance (scene luminance × the view's exposure, 1 at display white) above
  `flare_threshold` casts; the pass compares it in the HDR target's units, `flare_threshold × pre_exposure /
  exposure`, so the whole-stop steps of the pre-exposure do not move what casts. Added light is capped at 4096 per
  texel, which keeps the sum with the scene a finite half float.
- **Dirt.** A procedural mask lit by the ghosts and mip 4 of the bright image (about 64 viewport pixels per texel,
  read through a cubic B-spline so the glare is round),
  times `dirt_intensity` and a scatter fraction of 0.02.
- **Chromatic aberration.** Red is sampled at `0.5 + d (1 + a)`, blue at `0.5 + d (1 − a)` (`d` the offset from the
  centre in viewport uv), green on its own texel; it needs a copy of the HDR target (skipped and counted when the
  target cannot be copied).
- **Agent protocol.** `render.lens_effects { entity? }` and `render.set_lens_effects { entity?, ..., remove? }`
  (`book/agents/lens-effects.md`).
- **Warm.** `FramePass::prewarm` (a provided method, no-op by default) builds the pass's pipelines in the
  game-frame warm (`prewarm_mid_play`), and traces the default coating's table; its targets and the dirt mask are
  created on the first frame that asks. Android warms nothing (no preset allows the pass).
- **Not in scope yet.** Lights outside the frame (the flare is screen-space), starburst, a lens picker
  or prescription file format (`LensEffectsPass::with_ghost_table` takes a host-traced table), and running after the
  camera's depth of field (the slot is before it).

## 10. Convex hulls, per-volume LUTs and gizmo data (ADR-2323, format v3)

Written from public sources only: the half-space form of a convex polyhedron and its closest-point
characterisation (Boyd and Vandenberghe, *Convex Optimization*, 2004, § 2.2.4 and § 8.1; Ericson,
*Real-Time Collision Detection*, 2005, § 5.1 closest-point queries), the plane transform under an affine
map by the inverse transpose (any graphics text, e.g. Akenine-Möller et al., *Real-Time Rendering*,
4th ed., § 4.1.7), and trilinear 3D LUT lookup as already used by `lut.rs` (the Cube LUT specification
v1.0). The rules below are Regia's own.

### 10.1 `ConvexHull { planes }`

- **Data.** `planes: Vec<[f32; 4]>`, each `[nx, ny, nz, d]`: the half-space `n · x ≤ d` in local space,
  outward normal of any non-zero length (`d` is in units of `|n|`). 1 to `MAX_HULL_PLANES` = 16 planes.
  Zero planes, more than 16, a non-finite value or a normal shorter than 1e-6 make the shape invalid
  (`PostVolumeShape::is_valid`): the volume weighs 0 everywhere and has no outline; the agent refuses it
  (`invalid_shape`).
- **World planes.** For `world_from_local = (M, t)` a local plane maps to `n' = M⁻ᵀ n`, `d' = d + n' · t`,
  then both are divided by `|n'|`. The distance is therefore in world meters under any rotation, scale
  (also per axis) and shear. A transform with `|det M| ≤ 1e-12` is degenerate: weight 0.
- **Signed distance** (`hull_signed_distance`, `f64` internally; the world-plane transform above runs in `f32` and
  is widened before the search):
  - `s = max_i (n̂_i · p − d_i)`. If `s ≤ 0` the point is inside and `s` is exact (for a convex region the
    nearest boundary point lies on the nearest face plane).
  - Outside, `s` is a lower bound of the distance. If `s` is beyond the volume's blend distance the
    weight is 0 and the search below is skipped (`hull_signed_distance_within`).
  - Otherwise the nearest point of the hull lies in the relative interior of a face, an edge or a
    vertex; for each it is respectively the projection of `p` onto the face's plane, the projection onto
    the line of the two planes, or the vertex where three planes meet. The distance is the minimum over
    the candidates that satisfy every plane (tolerance `1e-9 · (1 + max |p_k|)`). At most 16 + 120 + 560
    candidates; no candidate (planes that do not meet) is an infinite distance, weight 0.
- **Weight** as every shape: `zone_weight(sd, blend_distance)` (§ 3.1), 1 inside and on the surface, a
  Hermite smoothstep across the band, 0 beyond. A hull may be unbounded (one plane is a half-space; a
  floor volume with a band above it).
- **Sphere.** Unchanged (the zone capsule with a zero half height; its radius scales with the mean of
  the X and Z scale). Under a uniform scale its distance and band are exact world meters.

### 10.2 Per-volume LUT blending

- **Off by default.** `regia_render::PostVolumeLuts` (a resource the render plugin inserts) has
  `enabled: false`. While it is off, `PostVolume::lut` is ignored and nothing is loaded: every frame is
  byte-identical to the frame without this feature, whatever the volumes name. A host sets `enabled`, or
  an agent calls `render.set_post_volume_luts { enabled: true }`.
- **Loading.** `load_post_volume_luts` (`PreUpdate`) loads each path the first time a volume names it
  (`.cube` or PNG strip, `regia_render::lut::load_named`; `\` is a separator too; a relative `.cube`
  resolves through the asset server first — mounted paks or the cooked cache, the only route a
  packaged Android client has — then as a loose file under the project root, as the level LUT;
  ADR-4830). A failed load warns once and is not retried until `PostVolumeLuts::clear` (agent:
  `reload: true`). At most 64 distinct paths are kept. Hosts may `insert` tables they built themselves.
- **Blend** (`blend_luts`, in `extract_frame` after the scalar blend, at each camera's eye). The base is
  the view's LUT before volumes (the level look's, or the identity). The volumes whose table loaded are
  weighed with `PostVolume::weight_at` (each weight rounded to a 1/256 step first, so the blend cache holds while a
  camera moves inside one step; ADR-5159) and grouped in tiers by priority. Inside a tier the tables are
  weight-averaged and the running table moves toward the mean by `α = min(1, Σ w)`, exactly the scalar
  rule of § 3.2, entry by entry. Tables blend on the grid of the largest table taking part; a smaller one
  is resampled at those grid points with its own trilinear lookup. Because trilinear lookup is linear in
  the entries, a blend of equal-size tables looks up exactly the weighted blend of the looked-up
  colours. A full-weight top tier with a single volume hands that volume's table over unchanged (the
  same content key as using it as the level LUT).
- **Where it applies.** The result is the view's `PostSettings::lut`, which the post stack's colour-grade
  step samples after the parametric grade, so the blended LUT sits exactly where a level LUT sits.
- **Cost.** A blend is `O(N³ × volumes)` on the CPU (N ≤ 64; 0.1 ms order for 33³). The last four
  blends are cached by a key of their inputs (table keys, weights, priorities), so a camera that stands
  still, or two cameras at one spot, blend once. The post pass uploads a changed table into its
  existing 3D texture when the edge is the same, so a camera walking through a band rewrites one texture
  instead of creating one per frame.

### 10.3 Precedence (tested in `tests/post_volumes_v3.rs`)

From first to last, each step overriding what it touches: the project's `RenderSettings::post` → the
level look (meter, grade, `look.lut`) → the camera's `LensEffects` component → post volumes (scalar
overrides, then volume LUTs) → the camera's lens (motion blur and depth of field; volumes cannot set
them) → the preset's lens-effects gate → the camera's `exposure_ev` pin (turns auto exposure off; a
volume's compensation still applies) → a `Manual` or `Physical` lens exposure (wins over the pin; the
compensation still applies).

### 10.4 Gizmo data (headless)

`regia_render::post_volume_outline(volume, world)` returns the editor gizmo's data: world-space line
segments of the shape and of the outer edge of its blend band, and whether the outline is closed.

- Box: its 12 edges; the band is the box with each face pushed out by the blend distance in world meters.
- Sphere: three great circles of 24 segments; the band is the sphere of radius `r + blend` (exact).
- Convex hull: the edges of the polytope (for each pair of planes, the segment between the extreme
  vertices on their line); the band is the hull with every plane pushed out by the blend distance (the
  exact band has rounded edges and corners; the outline bounds it). `bounded` is false for a hull with
  fewer than four planes or with an edge line that runs to infinity; those rays are not drawn.
- Unbound and invalid shapes: no outline.

Agents read it with `render.post_volume_gizmos { entity? }`; a drawing overlay can feed the segments to
the gizmo buffer.
