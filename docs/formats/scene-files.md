# Scene file formats — `.level.ron` and `.prefab.ron` (current format version 13)

Published in M6 (`specs/14` § A2). **Format version 1 is frozen**: after the first shipped content
build, loaders keep accepting files in this exact shape; new fields get defaults and a version bump
(each bump ships together with a migrator, `specs/12` § 4a; the current version is `SCENE_FORMAT_VERSION`, see Versioning below). Both file kinds share one document
schema — a `.prefab.ron` is a scene file with **exactly one root entity** (a level may have any
number of roots). JSON Schema: `scene-files.schema.json`; examples: `level.example.ron`,
`prefab.example.ron`.

Runtime: `regia_scene::format::SceneFile` (`from_ron_str` / `to_ron_string`, and postcard for the
binary authoring form). The runtime load path uses the *cooked* form — `regia_scene::cooked`
(postcard, type paths interned in a string table, values pre-encoded) — with the same structure and
the same version rules.

## Schema

```ron
SceneFile(
    // Required. The current version is 14 (`SCENE_FORMAT_VERSION`). Loaders reject a newer
    // version and run registered migrators for an older one; a change ships with a migrator
    // (`docs/specs/12-assets.md` § 4a; `regia upgrade-project` runs them).
    format_version: 14,

    // Required. Entity records. Order is irrelevant: parents may be declared after children.
    entities: [
        (
            // Required. Stable identity (32-hex-digit GUID). Unique across the whole file,
            // including `added_children`. Within a prefab it doubles as the *local id* that
            // overrides address.
            guid: "8f1c9e60-6f2a-4b77-9a3e-2c51b0d4e7aa",

            // Optional. Human-readable name (outliner, find-by-name). Never an addressing key.
            name: "Door",

            // Optional. Parent entity guid, `Some("...")` in files (the bare `"..."` form is
            // also accepted). May reference any entity of the file — including entities inside
            // an instantiated prefab's added children.
            parent: None,

            // Optional. Component values by **full type path**. Each value is one RON value
            // written in canonical form (`regia_ecs` canonical RON: fields in declaration
            // order, one list item per line, shortest round-trip floats). Unknown types are
            // skipped with a warning at runtime (or preserved verbatim by the editor's
            // re-save); unknown fields of known types are dropped with a warning; missing
            // fields take the type's `Default`.
            components: {
                "regia_scene::transform::Transform": (
                    translation: (0.0, 0.0, -3.0),
                    rotation: (0.0, 0.0, 0.0, 1.0),
                    scale: (1.0, 1.0, 1.0),
                ),
            },

            // Optional. A prefab instantiation record (below). The entity carrying it is the
            // *instance root*; the instantiated subtree hangs under it.
            prefab_instance: None,
        ),
    ],

    // Optional. Resource values by full type path, same value rules as components. Levels
    // normally carry at least `regia_scene::level::LevelSettings` (environment, sky, gravity,
    // `GameRules` type path, ambient audio; `specs/14` A4) and, since v2, the planet's sky.
    resources: {
        "regia_scene::level::LevelSettings": (
            gravity: (0.0, -9.81, 0.0),
            environment: None,
            game_rules: None,
            ambient_audio: None,
            streaming_enabled: false,
            atmosphere: (
                preset: Earth,
                time_of_day: Some((hour: 16.5, day_of_year: 172, latitude_deg: 40.0)),
                sun_illuminance_lux: None,
                cloud_coverage: 0.3,
                cloud_type: 0.5,
                sky_body: None,
                moons: [],
                haze: (density: 0.0, scale_height_m: 200.0, base_height_m: 0.0),
                // Optional (Earth only). Aerosol turbidity = vertical Mie optical depth τ at the
                // ground. Omitted keeps today's near-clean air (≈ 0.0053); 0.1 is a clear day.
                aerosol_turbidity: 0.1,
            ),
            // Since v3: the map's exposure meter and colour grade. `None` keeps the view's.
            look: (
                exposure_bias_ev: 0.0,
                min_ev100: None,
                max_ev100: None,
                meter: None, // Some((low_percentile: 0.4, high_percentile: 0.95, center_weight: 0.0))
                contrast: None,
                saturation: None,
                grade: None, // Some((white_balance_kelvin: 6500.0, tint: 0.0, lift: (0, 0, 0), gamma: (1, 1, 1), gain: (1, 1, 1)))
                lut: None, // Since v4: Some("looks/switchbacks.cube") or a PNG strip (N slices of N×N).
                // A relative path resolves through the asset server first (a .cube cooked into a
                // pak or the dev cache), then as a loose file under the project root; `\` is
                // accepted as a separator. See docs/formats/post-volume-v3.md, row `lut`.
                local_exposure: None, // Since v9: Some((shadow_contrast: 0.75, highlight_contrast: 1.0, detail_strength: 1.0))
                baked_gi: None, // Since v10: Some((volume: "maps/bore_twelve.fgiv", settings: (sky_scale: None)))
                // Since v11: lightmap: Some((file: "maps/yard.flmap", enabled: None, intensity: None)); not written when None
                // Since v13: captures: Some((file: "maps/yard.fcap")); not written when None
            ),
            // Since v12: terrain: Some((layers: "maps/arena/terrain.layers.ron", map: "maps/arena",
            //     world_width_m: Some(512.0), height_scale_m: 256.0, height_offset_m: 0.0,
            //     sample_layout: VertexCentered)); not written when None. Since v14: sample_layout
        ),
    },
)
```

The bare form `(format_version: 10, ...)` (no `SceneFile` name) parses identically; the canonical
writer always emits the named form.

## Prefab instances

```ron
prefab_instance: (
    // Required. The prefab asset, `"asset:<guid>"` (the `asset:` prefix is optional when reading).
    prefab: "asset:44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0",

    // Optional. Per-field value overrides on prefab-internal entities. `target` is the target
    // entity's local id (guid) **inside the prefab** — never a name. `path` is a reflection
    // field path with a leading dot (`.intensity`, `.0`, `.color.x`); `""` (or `"."`) replaces
    // the whole component. `value` is one RON value decoded against the field's type.
    overrides: [
        (
            target: "0f1e8a44-90d3-4f2e-b7c5-6a2d0e4f8b11",
            component: "regia_render::PointLight",
            path: ".intensity",
            value: 800.0,
        ),
    ],

    // Optional. Components added to prefab-internal entities that the prefab does not define.
    added_components: [
        (target: "0f1e...", component: "regia_scene::components::Tags", value: (["door"])),
    ],

    // Optional. Components removed from prefab-internal entities.
    removed_components: [
        (target: "0f1e...", component: "regia_render::MeshRenderer"),
    ],

    // Optional. Whole entity subtrees attached into the instance (created for this instance
    // only). Same record shape as a top-level entity; `parent` may point at a prefab-internal
    // guid, at another added child of this instance, or default to the instance root. An added
    // child may itself carry `prefab_instance` (counts as one nesting level).
    added_children: [],

    // Optional. Local ids (with their subtrees) of the prefab not to instantiate. Pruning a
    // parent prunes its whole subtree.
    removed_children: [],
),
```

### Addressing and nesting rules

- Prefab-internal entities are addressed by their **local id** (the guid inside the prefab file),
  never by name; the editor displays names. At spawn, each instantiated entity gets a fresh world
  guid (`EntityGuid`) and keeps its local id in the `PrefabLink` component, so two instances of one
  prefab never collide.
- **Nested prefabs** compose *outer-wins*: a prefab file may itself contain `prefab_instance`
  records; when the outer instance instantiates, the inner subtree is created first and the outer
  overrides (and structural edits) apply afterwards, over everything the inner prefab set up.
- An override target that does not exist in the instantiated subtree (pruned, or a typo) is a
  warning, not an error: the rest of the instance still spawns.
- Nesting is depth-limited (64 levels; a deeper chain means a cycle).

## Rules

- **Identity:** guids are unique file-wide and never nil. Moving/renaming files must not change
  guids (asset ids come from the `.meta` sidecar, `specs/12` § 2).
- **Canonical text:** writers emit `regia_ecs` canonical RON (4-space indent, one item per line,
  shortest round-trip floats). An `Option` field that is `None` is written (`nothing: None`); it is
  not omitted. Re-saving an unchanged world is byte-identical. Readers accept compact single-line
  values too. A missing key still loads as `Default`, which for `Option` is `None`.
- **Component value forms:** a `Name`-like string serialises bare (`"Door"`, so
  `Tags: (["crate", "loot"])`); a `Vec3` is `(x, y, z)`; a `Quat` is `(x, y, z, w)`. Values decode
  through reflection, so any registered component type can appear in the map — the schema can only
  mark them opaque (see below).
- **Leniency (loading):** unknown component/resource types — skipped with a warning (runtime) or
  preserved verbatim (editor); unknown fields — dropped with a warning; missing fields of a named
  struct — the type's `Default`. A component type with its own `#[reflect(Deserialize)]` uses that
  serde instead (no Default-fill, no unknown-field warning). Structural errors (duplicate/nil guids,
  unknown parents, parent cycles) are hard errors.
- **Versioning:** a `format_version` newer than this build's (13) is rejected with the supported
  version in the error. Older versions migrate through the registered chain
  (`regia_scene::format::migrate_to_latest`, the registry `regia upgrade-project` walks).
  The shipped `level.example.ron` and `prefab.example.ron` are stamped v8 on purpose: they are
  older-version files that load through the migration chain (the test checks they come out at the current version).
  - **v2** (M15c) added `LevelSettings::atmosphere`: `preset` (`Earth` | `Mars` | `Airless`),
    `time_of_day` (`hour`, `day_of_year`, `latitude_deg`), `sun_illuminance_lux`,
    `cloud_coverage`, `cloud_type` and `sky_body` (`direction`, `angular_diameter_deg`, `tint`,
    `bands`). The v1 → v2 step rewrites an existing `LevelSettings` with the block spelled out as
    `Earth`, so a v1 map renders as before. Every field has a default; omitting the block is
    Earth with the authored sun.
  - **v3** (look director round 2) added `LevelSettings::look` (`exposure_bias_ev`,
    `min_ev100`, `max_ev100`, `meter` (`low_percentile`, `high_percentile`, `center_weight`),
    `contrast`, `saturation`, `grade` (`white_balance_kelvin`, `tint`, `lift`, `gamma`, `gain`))
    and two atmosphere fields: `moons` (a list of `sky_body` records) and `haze` (`density`,
    `scale_height_m`, `base_height_m`). The v2 → v3 step spells them out at their neutral
    defaults (engine look, no moons, clear air), so a v2 map renders as before.
  - **v4** (look director round 3) added `LevelLook::lut`: a project-relative `.cube` file or PNG
    strip applied after the grade. The v3 → v4 step spells it out as `None` (identity).
  - **v5** (look director round 6) added `SkyBody::fill_lux`: planet-shine, a shadowless fill from
    the sky body (Jupiter-shine on Europa). `None` is the physical value at the body's phase;
    `Some(lux)` is authored. The v4 → v5 step spells it out as `None`.
  - **v6** (audio-medium lane) added `LevelSettings::audio_medium` (`airborne`,
    `speed_of_sound_mps`, `level_offset_db`, `absorption_db_per_m_khz2`, `ground`): overrides of
    the acoustic medium the atmosphere preset implies (`docs/specs/16-audio.md` § 3c). Every field
    is `None` by default (the preset's air and ground); the v5 → v6 step spells them out as `None`.
  - **v7** (look director round 11) added `LevelMeter::bright_threshold_cd_m2` and
    `bright_bias_ev`: bright-subject (snow) exposure compensation. A metered pixel from the
    threshold up to 16× it counts as bright subject; the bias (EV, brighter) applies in proportion
    to its coverage of the metered frame (none below 25 %, all above 60 %). Both are 0 (off) by
    default; the v6 → v7 step spells them out as 0. DO27's Bore Twelve uses 250 cd/m² and +1 EV.
  - **v8** (earth-aerosol lane) added `LevelAtmosphere::aerosol_turbidity` (aerosol optical depth
    τ, Earth preset only). Its serde default is today's near-clean air (≈ 0.0053,
    `TODAY_AEROSOL_TURBIDITY`), so an existing Earth level renders unchanged; a *new* level
    defaults to a typical clear day (`CLEAR_DAY_AEROSOL_TURBIDITY` = 0.1). The v7 → v8 step spells
    it out at today's value. Mars and airless presets ignore it.
  - **v9** (ADR-212) added `LevelLook::local_exposure` (`shadow_contrast`, `highlight_contrast`,
    `detail_strength`): edge-aware local tone adjustment before the tonemap. `None` is off; the
    v8 → v9 step spells it out as `None`.
  - **v10** (ADR-283) added `LevelLook::baked_gi`: `Some((volume, settings))` names the map's baked
    GI volume (`volume`: a `.fgiv` path under `assets/`, `docs/formats/baked-gi.md`) and the
    per-map overrides of the renderer's baked-GI settings (`settings`: `intensity`, `sky_scale`,
    `emissive_scale`, `replace_live_fill`, each `None` = the renderer's value). The framework loads
    the volume when the level loads and installs it (`book/agents/baked-gi-maps.md`). The v9 → v10
    step spells it out as `None`. Entities may also carry `GiMobility` (`Static` | `Dynamic`, default
    `Dynamic`): which ones a bake reads. It is a component, not a format field, so it needs no step.
  - **v11** (ADR-2313) added `LevelLook::lightmap`: `Some((file, enabled, intensity))` names the
    map's lightmap (`file`: a `.flmap` path under `assets/`, `docs/formats/lightmap.md`), whether
    receivers shade with it while the level is open (`enabled`: `None` = on, `Some(false)` = load
    but keep the probe light) and an intensity override (`None` = the renderer's). `None` names no
    lightmap and is not written, so a level without one saves exactly as a v10 writer saved it
    apart from the version. `regia bake-lightmaps --scene <level> --write-level` writes it. When
    the level opens the framework asks the renderer to warm the lightmap variants, installs the
    file and gives every drawn entity an entry was baked for a receiver (the level entity's own
    mesh is part 0, a `MeshPartOf` part entity is its part). Entities may also save
    `regia_render::lightmap::LightmapReceiver: (entry: "...")`, which the open keeps. The
    v10 → v11 step re-reads the settings and changes nothing else.
  - **v12** (ADR-2411) added `LevelSettings::terrain`: `Some((layers, map, world_width_m,
    height_scale_m, height_offset_m))` names the level's terrain. `layers` is the
    `.terrain.layers.ron` under `assets/` (empty: no heightfield named); its `.meta` id is the map
    id the cooked tiled heightfield `terrain/heightfield` derives from (`docs/formats/terrain.md`
    § 9). `map` is the path the side files are named after (`<map>.holes.ron`, `<map>.weights.bin`,
    `<map>.scatterpaint.bin`, `<map>.grassdensity.bin`); empty means the level file's own path.
    `world_width_m` (`None`: the cooked terrain index's `size_m`, else one metre per sample),
    `height_scale_m` (default 256: metres from sample 0 to sample 65535) and `height_offset_m`
    (default 0: the height of sample 0) are the world mapping. Paths are relative to `assets/`,
    `/`-separated, without `..`, a drive or a root (`level_terrain_invalid` otherwise). The editor
    (`regia_editor::terrain_panel::load_open_level_terrain`) and the runtime
    (`regia_terrain::level_terrain`, `LevelTerrainPlugin`) build the terrain from it. `None` names
    no terrain and is not written: the host supplies the terrain and the side files keep the
    level's own name, which is the pre-v12 convention. The v11 → v12 step re-reads the settings and
    changes nothing else, so a migrated level loads the same terrain (the same side files and the
    host's heightfield) and re-saves as before apart from the version.
  - **v13** (ADR-2860) added `LevelLook::captures`: `Some((file: "maps/yard.fcap"))` names the
    level's local reflection capture bake ([`fcap.md`](fcap.md)), a path under `assets/`. Entities
    may save `regia_render::LocalCapture: (id: 1, half_extent: (4.0, 2.5, 4.0), weight: 1.0,
    fade: 1.0)`; the capture point is the entity's world position. When the level opens the
    framework reads the bake and every capture baked at its position shows its map with no face
    rendered; the others bake as before. `None` names no bake and is not written. The v12 → v13
    step re-reads the settings and changes nothing else.
  - **v14** (ADR-10079) added `LevelTerrain::sample_layout`: where the terrain's `n` samples per
    axis sit in `world_width_m`. `VertexCentered` (the default, every earlier level) puts sample
    `i` at `i / (n - 1) * W - W / 2`, the first and last on the edges. `CellCentered` reads `W` as
    `n` equal cells and puts sample `i` at its cell's center, `(i + 0.5) * W / n - W / 2`
    (spacing `W / n`): a cell-registered raster or a cell-spaced simulation grid. With no
    `world_width_m` a cell-centered terrain takes the cooked index's `size_m` (the span between
    the outer samples) times `n / (n - 1)`, else one metre a cell. A terrain built from a cooked
    count mapping (ADR-8471) places its samples by the mapping and refuses `CellCentered`
    (`level_terrain_mapping`). The v13 → v14 step re-reads the settings: a terrain block gains
    `sample_layout: VertexCentered` and its samples sit where they did; a level that names no
    terrain changes only its version.
- **Levels vs prefabs:** a level file may contain any number of root entities and a
  `LevelSettings` resource. A prefab file must have exactly one root and normally carries no
  resources. Levels instantiate prefabs; a prefab may instantiate nested prefabs.
- **Agent protocol:** agents read/write these files directly (canonical RON, `specs/23` § 1) and
  address prefab-internal entities by local id in overrides; the JSON Schema is the machine
  contract for generators.
