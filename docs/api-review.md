# API review — areas DO27 touches

List-only review of the public surface a game developer reaches through the `regia` umbrella.
For each crate: things that look **unstable** (may change) or **surprising** (naming, units,
panics, missing error types) for someone outside the engine team. **No signatures are changed
here** — the lead decides what to act on. This covers M18 ("API review of everything DO27
touches"). The checked snapshots of these crates are in `docs/api-snapshots/` (`cargo xtask api-snapshot`,
ADR-1942).

---

## regia_app

- `App::add_plugins` silently ignores a plugin whose `Plugin::name()` was already added (a
  `warn!`, not an error; the dedup lives in the private `add_boxed_plugin`). A game adding the
  same plugin twice will not fail loudly.
- `App::new()` installs a **headless** runner (`run_headless`); a window appears only when the
  platform layer replaces it. Tests/servers benefit, but "why no window?" is a plausible surprise.
- `AppExit` is an ECS **message** (buffered), not a return value: `App::run()` returns the first
  exit request a runner observed. `AppExit::Error(NonZeroU8)` is a process code, not a `Result`.
- Frame delta is clamped to `MAX_DELTA` (250 ms); a debugger pause does not inject a huge `Time`
  delta. Desirable, but worth documenting to a game developer expecting raw wall time.
- `Plugin::name()` defaults to the type path — two unit-struct plugins in the same module share a
  name and one is dropped with a warning.

## regia_scene

- `LevelMember(pub u128)` stores a **raw `u128`**, not a `Guid` (reflection has no `Guid` type).
  Game developers read `LevelMember::level()` to get a real `LevelId`; the raw field is surprising.
- `LevelManager::unload_level` **no-ops silently** on an unknown level (`Ok(())`); there is no
  "not found" error.
- `open_level` returns immediately even when the level data is not loaded yet, recording a
  `Loading` state. Async-by-design, but a sync caller sees no error and no entities that frame.
- `GlobalTransform`/`PreviousGlobalTransform` are derived and never saved in scene files; a
  developer snapshotting a world loses them (they are recomputed on load). Correct, but a subtle
  invariant if they diff snapshot text.
- `Transform::from_affine` is exact only for pure TRS; sheared/mirrored matrices decompose
  approximately (identity rotation fallback). Reparenting with `keep_world_transform` relies on it.
- `LevelSettings.game_rules` is a **type path string** (`Option<String>`), not a type. The level
  loader resolves it by registered name (`GameRulesRegistry`) at level load and replaces the
  active `GameRulesResource`; an unknown name falls back to `DefaultRules` with a warning, so a
  typo'd level still plays. A game developer must register their rules name during
  `GamePlugin::register` for a level to select them.

## regia_render

- `Camera.viewport` is **normalized** `[x, y, w, h]` (0..1), not pixels; `RenderView` carries the
  pixel `width`/`height` separately.
- `Camera.exposure_ev` is `Option<f32>` in **EV100**; `far: f32::INFINITY` selects the reversed-Z
  infinite projection. Units are only in the doc comments.
- `CpuMesh`/`CpuMaterial` are shared through `Arc`; the renderer caches GPU uploads by `Arc`
  identity, so **mutating a shared mesh in place is not picked up** — replace the `Arc` to re-upload.
- `TEXTURE_TARGET_SIZE` hardcodes `RenderTarget::Texture` cameras to 512×512 until texture handles
  exist — an obvious temporary in a game developer-facing API.
- `RenderProfile.render_scale` documents 0.0–1.0 but accepts >1.0 (supersampling); `FrameQuality`
  clamps to `[0.05, 1.0]`, so the two disagree at the edges.
- `LightUnits` is `#[allow(dead_code)]` and unused: intensities are bare `f32` (lux/lumens/candela)
  with units only in doc comments. A game developer cannot attach a unit to a value.
- `Environment.ambient_lux` is a flat lux fallback used only when `ibl` is off; the default
  (12 000) is a magic constant without a documented source.

## regia_input

- Axis convention is mixed: **x = right, y = forward/up** for actions, but touch/gesture positions
  are window pixels with **y down**. Callers converting between the two must flip Y.
- `Actions` accessors return `false` / zero for **unknown action names** rather than panicking.
  Friendly, but a misspelled action is a silent bug.
- `InputConfig::to_ron()` uses `unwrap_or_default()` — a serialise failure returns an **empty
  string** instead of an error.
- `Modifier::Smooth` keeps per-binding state keyed by `(context, binding index)`; reordering
  bindings changes which smoothing state applies.
- `InputSource::Keys2D` normalises diagonal input to length 1 (unit circle), so W+D is not faster
  than W alone. Correct for many games, surprising for others.

## regia_ui

- `UiError::Ron` embeds `ron::error::SpannedError` — a large, layout-heavy error type crossing the
  crate boundary.
- `UiTarget::Camera` and `UiTarget::WorldSpace` are documented **Tier 2 / SDF text** — the enum
  variants exist but are not implemented in Tier 1. A game developer can construct them and get nothing.
- `UiPlugin::code_only()` disables `.ui.ron`/localization loading; forgetting it in tests makes
  asset lookups fail with `AssetNotFound`.
- `Text.line_height` is a multiplier of font size (default 1.2), not pixels — units are implicit.
- `Style` uses taffy's flex/grid model directly; property names mirror CSS but the supported subset
  is not obvious from the type alone.

## regia_audio

- `.cue.ron` **v1 had no `version` field** (implicit version). Resolved: v0.1.24 added `version: 2` and a
  migrator from the versionless form (ADR-272, which supersedes ADR-023); the hash guard records cue version 2.
- `Audio::play`/`play_at`/`play_cue` queue a one-shot that is drained per frame; the `Result` only
  reports immediate validation, not playback success. Fire-and-forget semantics.
- `AudioSettings.headless` is required for offline/CI runs; the default opens the real device, so a
  headless test that forgets the flag fails at device init.
- `SoundCue` pitch/volume ranges are **tuples** `(min, max)`, not named fields, and volume ranges
  are in dB while `volume_db` is the base — units vary by field.
- Distance attenuation / Doppler follow the OpenAL models; `AudioEmitter.rolloff` is a
  `RolloffCurve` (inverse/linear/log/custom) selecting the model per-emitter.

## regia_framework

- `GameRules` is object-safe, but **state must live in reflected resources/components**, not in the
  trait object, so rules survive hot reload. This is a real constraint a game developer will hit when
  their rules need fields.
- `SettingsPath(None)` silently disables persistence; on Android the default is `None` (no
  `user_dir` until the shell provides one), so settings are in-memory only there.
- `GameSettings::from_ron` is a plain `ron::from_str` with no `deny_unknown_fields`, so
  **unknown fields are silently dropped** — a settings file from a newer game version loads but
  loses its unknown sections (no error, no forward-compatibility signal).
- `GameSettings.to_ron` returns `ron::Error` but `from_ron` returns `ron::error::SpannedError` —
  asymmetric error types for the same value.
- `CameraRig` pitch/yaw are **radians**, `ThirdPerson` boom/lag are metres/1/s; the mixed units are
  only in doc comments.
- `CharacterPlugin` still **defaults** to the flat-plane stub (`FlatPlaneQuery`) even though the
  rapier backend (`RapierCharacterQuery`, installed via `use_rapier_character_controller`) has
  landed — a game must opt in, or characters stand on an infinite plane at Y = 0.

## regia_terrain

- `TerrainMap` stores heights as **f32 metres** derived from `u16/65535 * scale + offset`; the
  `u16` source precision is collapsed to f32 on load (fine at ≤1 cm agreement, but it is lossy).
- `sample_bilinear`/`height_at_sample` **border-clamp** outside the map — no wrapping; a query off
  the edge returns the border height, which may surprise consumers expecting an error or a range.
- `Terrain` requires a cooked `regia_asset::Heightfield`; there is no in-code constructor that
  skips the asset type (unlike `TerrainMap::from_heights`).
- `TerrainSettings.max_selections` truncates LOD selection with a warning above 64 terrains — a
  hidden cap.

## regia_vfx

- `VfxGpu` has **no colour format** — render pipelines are built lazily per format on first use;
  the integration contract (depth view read-only, sRGB/Unorm colour) is documented but not
  type-enforced.
- `CameraShake` uses the trauma² model (`strength` squared) and a fixed ~35 Hz noise; the amplitude
  scale is linear in metres at full trauma but the perceptual mapping is implicit.
- `CameraShakeRequest.radius` is "full-strength radius; linear falloff to zero at 2×radius" — the
  falloff shape is fixed and not a field.
- `SpawnEffect`/`SpawnDecal` are fire-and-forget messages; spawn counts are budgeted and silently
  clamped (`sim`), so requesting too many is not an error.

## regia_physics

- `PhysicsQuery` is a `SystemParam`; tooling that owns a `&World` (the agent protocol) must build
  `WorldQueries` instead. Two entry points for the same operation.
- Determinism is **per-platform** by default; cross-platform lockstep requires the
  `physics-cross-platform-determinism` feature (rapier `enhanced-determinism`), which costs speed.
- `RigidBody`/`Collider` carry rapier-specific concepts (`dominance_group: i8`, `CombineRule`,
  `ActiveEvents`) whose exact semantics are only in doc comments and the rapier docs.
- `QueryFilter` takes `exclude_entities: &[Entity]` and a predicate closure — lifetime-bound
  builder, so a filter cannot be stored and reused across frames without care.

## regia_agent

- `AgentSettings.project_dir` is `Option<PathBuf>`; when `None` the server falls back to the log
  directory, and with no writable data dir the agent server is **silently disabled** (a `warn!`).
- The protocol is versioned (`protocol::PROTOCOL_VERSION`); a client/tool built against an older
  schema will not be warned unless it calls `agent.hello` and checks capabilities.
- Mutating calls are gated by `.regia/agent.toml` permissions and rate-limited; a call that needs
  `edit_scene`/`play`/`assets` permission is rejected with a specific error — a game developer driving
  the protocol must know to configure permissions.
- `AgentTimeControl` pause/step mutates `Time.paused` directly; a game that also pauses `Time`
  will see the agent's step/pause interact with its own pause state.

## sim_bridge (sample)

- `HoverSim::submit_input` records the last `BridgeInput` but does **not** move the discs —
  authority stays with the sim. This is the point of the sample (authority model) but a reader
  expecting input to affect motion will be surprised.
- `ExitAfterFrames(pub u64)` is a smoke-run helper leaked into the sample's public API; a game developer
  copying the sample inherits an automatic-exit hook.
- `HoverSnapshot`/`HoverSim` fields are mostly private, so the sample is a read/reference example,
  not an extension point.

---

# Resolution

Each item above, resolved by the **api-review-fixes** lane (non-breaking only). The implementation
commit is `4aeba17` (the `try_*`/strict/`NotFound` additions and the `add_plugins` warning test);
documentation fixes and this resolution are in the commit that carries this change.

## regia_app

- `add_plugins` double add — **done**. Already logs a `warn!` in `add_boxed_plugin` and skips the
  duplicate; added a regression test asserting the warning and documented it on `App::add_plugins`.
- `App::new()` headless runner — **not-an-issue**. Documented on `App::new()`.
- `AppExit` is a buffered message — **not-an-issue**. Documented on `AppExit`.
- Frame delta clamped to `MAX_DELTA` — **not-an-issue**. Documented on `Time`/`MAX_DELTA`.
- `Plugin::name()` type-path default — **not-an-issue**. Documented on `Plugin::name()`.

## regia_scene

- `LevelMember(pub u128)` — **not-an-issue**. Documented (`raw id is a u128 … use
  LevelMember::level()`); blocked on reflection gaining a `Guid` type.
- `unload_level` no-ops on unknown level — **done**. Added `LevelNotLoaded`,
  `LevelManager::try_unload_level` and the free `try_unload_level` (both return
  `Result<_, LevelNotLoaded>`); the original `unload_level` keeps its no-op behaviour and
  `LevelError` is unchanged (no public signature changed).
- `open_level` returns before data arrives — **not-an-issue**. Documented (async-by-design).
- `GlobalTransform`/`PreviousGlobalTransform` derived, never saved — **not-an-issue**. Documented
  ("Derived data — never authored, never saved in scene files").
- `Transform::from_affine` approximate for shears — **not-an-issue**. Documented ("Exact for pure
  TRS inputs; sheared … decompose approximately").
- `LevelSettings.game_rules` type-path string — **not-an-issue**. Documented (resolve via
  `GameRulesRegistry`, unknown name → `DefaultRules` + warning).

## regia_render

- `Camera.viewport` normalized — **done**. Clarified the doc (normalized `0..=1`, not pixels;
  `RenderView` carries the pixel extent).
- `Camera.exposure_ev` EV100 / `far` infinite — **not-an-issue**. Documented.
- `CpuMesh`/`CpuMaterial` `Arc` identity — **not-an-issue**. Documented ("replace the `Arc` to
  re-upload").
- `TEXTURE_TARGET_SIZE` 512×512 — **not-an-issue**. Documented (temporary until texture handles).
- `render_scale` 0–1 vs `FrameQuality` clamp — **done**. Corrected the doc: `1.0` native, `>1.0`
  supersampling, clamped to `[0.05, 1.0]` by `FrameQuality::from_profile`.
- `LightUnits` unused — **not-an-issue**. `#[allow(dead_code)]` placeholder for future typed units.
- `Environment.ambient_lux` magic constant — **done**. Documented the 12 000 lux source
  (daylight ≈ 10 000–15 000 lux).

## regia_input

- Axis convention (x = right, y = up) vs touch pixels (y down) — **not-an-issue**. Documented.
- `Actions` accessors false/zero for unknown names — **not-an-issue**. Documented ("Reading an
  unknown action is false, not a panic").
- `InputConfig::to_ron()` `unwrap_or_default()` — **done**. Added `try_to_ron()` (also on
  `InputOverrides`); `to_ron()` keeps its empty-string fallback.
- `Modifier::Smooth` per-binding state — **not-an-issue**. Documented (keyed by
  `(context, binding index)`).
- `InputSource::Keys2D` diagonal normalized — **not-an-issue**. Documented.

## regia_ui

- `UiError::Ron` embeds `SpannedError` — **not-an-issue**. The span context is the point; no
  developer churn observed.
- `UiTarget::Camera`/`WorldSpace` Tier 2 — **not-an-issue**. Documented.
- `UiPlugin::code_only()` — **not-an-issue**. Documented.
- `Text.line_height` multiplier — **not-an-issue**. Documented.
- `Style` mirrors taffy — **not-an-issue**. Documented (provenance).

## regia_audio

- `.cue.ron` v1 had no version field — **fixed** by ADR-272 (`version: 2` plus a migrator from the
  versionless v1 form; standing rule `specs/12` § 4a).
- `play`/`play_at`/`play_cue` fire-and-forget — **not-an-issue**. Documented (`Result` reports
  immediate validation only).
- `AudioSettings.headless` — **not-an-issue**. Documented.
- `SoundCue` pitch/volume tuples — **not-an-issue**. Documented (units per field).
- Distance attenuation / Doppler — **not-an-issue**. Documented (provenance).

## regia_framework

- `GameRules` state must live in resources — **not-an-issue**. Documented (survives hot reload).
- `SettingsPath(None)` — **not-an-issue**. Documented (in-memory only; Android default).
- `GameSettings::from_ron` drops unknown fields — **done**. Added `from_ron_strict` +
  `GameSettingsStrictError` (rejects unknown top-level fields). The lenient `from_ron` is
  unchanged so DO27 files with extra fields keep loading; making the *default* strict would be
  breaking and is deferred.
- `to_ron`/`from_ron` asymmetric errors — **not-an-issue**. Documented (`ron::Error` vs
  `SpannedError`, inherent to serialize vs parse).
- `CameraRig` radians — **not-an-issue**. Documented.
- `CharacterPlugin` flat-plane default — **not-an-issue**. Documented (opt-in rapier backend).

## regia_terrain

- `TerrainMap` u16→f32 lossy — **not-an-issue**. Documented (≤1 cm agreement).
- `sample_bilinear`/`height_at_sample` border-clamp — **not-an-issue**. Documented.
- `Terrain` requires a cooked `Heightfield` — **not-an-issue**. Documented (unlike
  `TerrainMap::from_heights`).
- `TerrainSettings.max_selections` cap — **not-an-issue**. Documented (warning on truncation).

## regia_vfx

- `VfxGpu` no colour format — **not-an-issue**. Documented (lazy per-format pipelines).
- `CameraShake` trauma² model — **not-an-issue**. Documented (provenance).
- `CameraShakeRequest.radius` fixed falloff — **not-an-issue**. Documented.
- `SpawnEffect`/`SpawnDecal` budget-clamped — **not-an-issue**. Documented.

## regia_physics

- `PhysicsQuery` vs `WorldQueries` — **not-an-issue**. Documented.
- Determinism per-platform — **not-an-issue**. Documented (feature-gated).
- `RigidBody`/`Collider` rapier concepts — **not-an-issue**. Documented.
- `QueryFilter` lifetime-bound — **not-an-issue**. Documented.

## regia_agent

- `AgentSettings.project_dir` `None` — **not-an-issue**. Documented (fallback + silent-disable warn).
- Protocol versioning — **not-an-issue**. Documented (`PROTOCOL_VERSION`).
- Mutating-call permissions — **not-an-issue**. Documented.
- `AgentTimeControl` mutates `Time.paused` — **not-an-issue**. Documented.

## sim_bridge (sample)

- `submit_input` records-only — **not-an-issue**. Documented (the authority-model point).
- `ExitAfterFrames(pub u64)` — **not-an-issue**. Documented (smoke-run helper).
- `HoverSnapshot`/`HoverSim` private fields — **not-an-issue**. Read/reference example by design.

---

## regia_framework (the `extension` module; spec 57, ADR-1885)

The extension prelude is public surface from its first release (`specs/57` § 11.1): `RegiaExtension`, `ExtensionContext`,
`ExtensionError`, `ExtensionEvent`, `AgentMethodDesc`, `AgentFault`, `MethodAccess`, `AssetLoaderDesc`, `ContentMount`,
`FeaturePackDesc`, `ComponentSpec`, `LevelFeatures`, `ExtensionSet`, `DynamicStateBlob`, `pack_active`, the re-exported
`FramePass`, `FramePassContext` and `GraphSlot`, `ExtensionPlugin::for_game` with `LinkedExtensions`, and the macros
`regia::extension!` and `regia::register_extensions!`.

- **Surprising:** `AssetLoaderDesc` is not called by the `AssetServer` load methods: a caller decodes with
  `ExtensionRegistry::load_cooked_content`, because `regia_asset` sits below the registry.
- **Surprising:** `ExtensionContext::add_plugin` answers `restart_required` during a live reload, because a plugin builds
  against the app. `add_systems` works then; its systems must be `Send + Sync + 'static` to wait for the frame boundary.
- **`abi_epoch`:** any layout change of a type reachable from this list bumps `extension::stamp::ABI_EPOCH`, even in a patch
  release.

---

# API policy (spec 57 § 11.1, ADR-2527)

### What counts as public

A crate is public when at least one is true: the `regia` umbrella re-exports it; this review names it; a generated
project template or extension depends on it. Public crates have a snapshot in `docs/api-snapshots/<crate>.txt`
(`STABLE_CRATES` in `xtask/src/api_snapshot.rs`). The editor extension surface (`regia_editor::extension`, the module
third-party editor tools compile against) has a module-scoped snapshot, `regia_editor.extension.txt`
(`SCOPED_SURFACES`). Everything else is internal and may change in any release: `regia_editor` apart from `extension`,
`regia_cli`, `regia_asset_pipeline`, `regia_dylib`, `regia_templates`, `regia_test_utils`, `regia_pt`, `regia_dlss`
and `regia_macros` (a procedural-macro crate; the scan cannot read what its macros accept). ADR-2527 gives the reason per
crate. A new crate that fits the rule joins the list in the commit that adds it.

### Deprecation window

An item leaves the public API in two steps. First it is marked `#[deprecated(since = "<version>", note = "use <X>")]`,
which the snapshot records. It may be removed no sooner than **one minor release** later before 1.0, and **two minor
releases** later from 1.0 (a removal then also needs a major version, § 11.1). The deprecation is listed in the release
fragment's **Upgrade** section (`docs/releases/`), one line per item with the replacement. A removal whose old snapshot
had the item deprecated passes `cargo xtask api-snapshot` (`api-snapshot: ... removed after deprecation`). A removal
without that is `removed_without_deprecation`: a warning while `docs/api-snapshots/policy.toml` says `freeze = false`,
an error no reason accepts once it says `freeze = true`.

### Recording a deliberate break

When a lane must change or remove an item without the window (before the freeze only), it runs
`cargo xtask api-snapshot --update --breaking "<why, with the ADR>"`, commits the snapshot with the code, and names the
break under Upgrade in its release fragment. `BREAKING.md` lists the rows of the unreleased versions; the release lane
moves a released version's rows out with `cargo xtask api-snapshot --rotate <version>` into
`docs/api-snapshots/breaking/<version>.md`.
