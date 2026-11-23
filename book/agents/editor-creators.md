# Editor creators (agents)

The editor's creators (spec 19 § 2, § 5, § 6, ADR-7234): the Content Browser's **New** entries, the **Data Asset**
panel, the Project Settings **Physics layers**, **Localization**, **Packaging** and **Audio buses** pages, the
**New script** wizard and the project browser's **template picker**. Each one's widgets and its `editor.*` methods
call the same functions, so an agent can do what a person can and read back what the panel shows. All of them are
desktop-editor only.

Errors are `invalid_params` (or `not_found` for the `*_not_found` codes) with a lowercase `data.reason` and a hint.

## Content Browser: New

New ▸ Material, Prefab, Level, Anim graph, Sound cue, UI layout, Data asset ▸ *type*, Material graph, Material
function, Sequence, Shot list, Attribute set, Effect, Ability, Material instance ▸ *parent*, Procedural graph, VFX
system, Conversation, Curve table, Chooser, Data table, Folder creates the asset in the
shown folder under the first free name (`<kind>-<n>.<ext>`) and selects it; a new data asset opens in the Data Asset
panel. `editor.create_asset` makes the same files at a path you choose:

| `kind` | Extension | What the new file holds |
|---|---|---|
| `material` | `.material.ron` | The default standard material. |
| `prefab` | `.prefab.ron` | One root entity named after the file, with a `Transform`. |
| `level` | `.level.ron` | An empty level with default settings. |
| `anim_graph` | `.animgraph.ron` | A root clip node playing `idle`, looping (a graph needs a root; the Anim Graph editor replaces it). |
| `sound_cue` | `.cue.ron` | A cue named after the file with no variations yet (the Cue Editor adds them). |
| `ui_layout` | `.ui.ron` | A full-size column root, as the UI Designer's New. |
| `data_asset` | `.data.ron` | The default value of `type` (required; a registered data asset type — see `editor.data_asset_types`). |
| `material_graph` | `.matgraph.ron` | An empty material graph named after the file. |
| `material_function` | `.matfunc.ron` | A material function passing one `Vec3` input `x` through. |
| `sequence` | `.seq.ron` | A sequence named after the file with no tracks, an end marker at 5 s and 30 fps, at the current version (v5): what `editor.seq_new` writes. Open it with `editor.seq_curve_open`. |
| `shot_list` | `.shots.ron` | A shot list named after the file: one still shot (`shot1`) at the origin looking down −Z, 60° field of view, 5 s at 30 fps, at the current version (v2). Open it in the Shots panel with `editor.shot_panel_open`. |
| `attribute_set` | `.attrs.ron` | One `health` attribute (base 100, 0..=100), current version (ADR-7472). |
| `effect` | `.effect.ron` | An Instant effect named after the file with no modifiers yet, current version (ADR-7472). |
| `ability` | `.ability.ron` | An ability named after the file with the format's defaults, current version (ADR-7472). |
| `procgen_graph` | `.pgraph.ron` | The procedural graph panel's blank graph (`in.self` → `sample.region` → `out.points`), current version (v1, ADR-7725). |
| `vfx_system` | `.vfx.ron` | A VFX system named after the file with one emitter `main` (the VFX editor's template), current version (v8, ADR-7725). |
| `conversation` | `.conv.ron` | A conversation whose `id` is the file's stem, with no lines yet, current version (v1, ADR-7725). |
| `curve_table` | `.curves.ron` | A linear curve table with no curves, current version (v1, ADR-7725). |
| `chooser` | `.chooser.ron` | A first-match chooser whose results are asset references of any kind, with no columns or rows, current version (v1, ADR-7725). |
| `data_table` | `.table.ron` | A data table with no rows; `row_type` is `type` when given (a row struct's type path), else unnamed (any typed load accepts it), current version (v1, ADR-7725). |
| `montage` | `.montage.ron` | A montage named after the file: one track on the default slot (`default`, full body) playing a clip named `idle` to its end, current version (ADR-7731). Open it in the Montage timeline with `editor.montage_panel_open`. |
| `cloth` | `.cloth.ron` | A cloth named after the file with default parameters; its sim mesh is node `<name>` of `<name>.glb` beside it (export the mesh there, or edit `sim_mesh`), current version (ADR-7731). Open it with `editor.cloth_panel_edit { path }` or `editor.open_asset`. |
| `soft_body` | `.soft.ron` | A pressure soft body named after the file (pressure 1, incompressible) whose surface is node `<name>` of `<name>.glb` beside it, current version (ADR-7731). It opens in the Cloth panel the same way. |
| `camera` | `.camera.ron` | A camera asset named after the file: a `look` input slot and one rig `follow` (pivot on the driver's target, look input, a 4 m boom, 70° field of view) that a fixed director runs; valid with no warnings, current version (v2). It opens on the Cameras panel (`editor.camera_panel_load`), whose rig editor edits it (ADR-7734). |
| `smart_object` | `.smartobject.ron` | A smart-object definition with one slot (`slot_0`) at the object's origin, written by the format's own writer, current version (ADR-7736). Edit it in the Smart Object definition panel (`book/agents/smart-object-editor.md`). |
| `tilemap` | `.tilemap.ron` | A tile map at the current format version: 16 × 16 cells of 16 px, one empty `ground` tile layer and no tileset yet; bind one in the 2D tab (`editor.tilemap_tilesets`, ADR-7742). |
| `motion_database` | `.motiondb.ron` | A motion database with the schema defaults, no tags and no clips, current version; Content ▸ New opens it in the Motion database editor window (`book/agents/motion-database-editor.md`, ADR-7740). |
| `biome` | `.biome.ron` | A biome named after the file with no layers yet, current version; edit it in the Biome editor (`book/agents/biome-editor.md`). |
| `logic_graph` | `.logic.ron` | An empty logic graph at format v1 (no rules, locals or world boards): the bytes the Logic Graph panel saves for an empty graph, which `logic.compile` accepts. Open it with `editor.open_asset` or `editor.logic_open` (`book/agents/logic-graph.md`). |
| `folder` | — | A folder under `assets/`. |

`editor.asset_kinds` lists these kinds in the New menu's order (`create: [{ kind, label, extension }]`), with the
kinds `editor.open_asset` opens (see `book/agents/content-browser.md`). Each new file loads through its runtime
loader at the current version, and the menu and `editor.create_asset` write the same bytes for the same file name.

```json
{"method": "editor.create_asset", "params": {"kind": "data_asset", "path": "assets/waves/first.data.ron", "type": "my_game::waves::WaveSpec"}}
```

## Data Asset panel (`editor.data_asset_*`)

A data asset is one reflected value in a `.data.ron` (`docs/formats/data-asset-ron.md`). A type registered as a
data asset (a `#[derive(Asset)]` type registered with `register_data_asset` or `#[reflect(Asset)]`; the derive
alone registers nothing) can be one — only those load in a game (ADR-7490); the New
menu lists the registered data asset types that are neither components nor resources, outside the standard library
and math crates. Creating or opening any other type is refused (`data_asset_unknown_type`): a game's runtime loader
would refuse the file. The panel draws the value with the inspector's
reflection widgets; every edit is one step on the editor's undo stack, and Save writes the file atomically.
Double-clicking a `.data.ron` in the Content Browser opens it.

| Method | What it does |
|---|---|
| `editor.data_asset_types` | `{ types }`: the type paths the New menu offers. |
| `editor.data_asset_open { path, discard? }` | Open a `.data.ron`: `{ path, type, value, dirty, discarded }`. Refused (`data_asset_unsaved`) while the open asset has unsaved edits, unless `discard: true` drops them first (`discarded` says whether there were any). The panel's **Revert** is `discard: true` with the open path. |
| `editor.data_asset_set { field?, value }` | Set a field (`count`, `weights[1]`, `drop.chance`; empty for the whole value) from the reflect serializer's JSON form. One undo step (`editor.undo`). Answers the open asset plus `changed`. |
| `editor.data_asset_save` | Write the file: `{ path, bytes }`. |

Codes: `data_asset_no_project`, `data_asset_no_registry`, `data_asset_bad_path`, `data_asset_exists`,
`data_asset_not_found`, `data_asset_parse`, `data_asset_version`, `data_asset_unknown_type` (also when the type is
registered but not as a data asset; `data_asset_no_default` is retired — a data asset type is `Default` by its
trait), `data_asset_no_field`, `data_asset_bad_value`, `data_asset_not_open`, `data_asset_unsaved`,
`data_asset_serialize`, `data_asset_io`, `data_asset_bad_discard` (`discard` is not a boolean).

## Project Settings pages

`editor.project_settings_get` carries the four pages under `pages`:
`{ physics: { path, exists, max_layers, layers, collides, queries }, localization: { path, source, locales }, packaging: { android, linux }, audio: { path, buses } }`.
A page whose file does not load shows `{ code, message }` in its place.

Physics layers have one editor: the **Physics layers (config/physics.ron)** section of Project Settings and
`editor.physics_layers_get` / `editor.physics_layers_edit` ([Physics layer matrix](physics-layers.md), ADR-7267).
`pages.physics` is exactly the `editor.physics_layers_get` result. Only the last layer can be removed
(`layer_not_last`): colliders and saved levels store layer indices.

| Method | What it does |
|---|---|
| `editor.project_localization_edit { edits }` | Edit `assets/localization/localization.ron`: `source { locale }`, `add_locale { locale }`, `remove_locale { locale }`. Tags must be BCP 47. All or nothing, one undo step. |
| `editor.project_packaging_edit { packaging, save? }` | Edit `[platforms.android]` (`min_sdk`, `target_sdk`, `orientation`, `abis`, `permissions`, `keystore`, `keystore_password`, `key_alias`, `key_password`) and `[platforms.linux] formats` in `Regia.toml`, keeping every other key. Passwords must be `env:VAR` references (`packaging_secret_literal`): a password typed into `Regia.toml` would be committed with the project. Saves by default. |

The Audio buses page lists `config/audio.ron`'s buses (the default tree when there is no file) and opens the Audio
Mixer, which edits them (`editor.audio_mixer_*`).

Codes: `no_project`, `localization_parse`, `localization_bad_locale`, `localization_bad_edit`,
`localization_io`, `packaging_invalid`, `packaging_secret_literal`, `project_settings_read`,
`project_settings_invalid`, `audio_buses_parse`.

## New script wizard

File ▸ New script… opens the wizard: a name, a template and Create. Create writes `game/src/scripts/<name>.rs` and
registers it at the `// REGIA:SCRIPTS` marker of `game/src/lib.rs`, the same code as `editor.create_script`.

| Template | The file |
|---|---|
| `empty` | A module doc comment only (the default). |
| `component` | `#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)] #[reflect(Component, Default)] pub struct <Name> {}`; register it with `app.register_type::<Name>()` in the game plugin so the editor can add it. |

| Method | What it does |
|---|---|
| `editor.create_script { name, template? }` | Create the script (`script_bad_template` for an unknown template). |
| `editor.script_wizard { open? }` | Show or hide the wizard; answers `{ open, name, template, templates, status }`. |

## Project browser templates

Launching the editor without a project shows the template picker: Blank, First Person, Third Person and Top Down
(the `regia new` ids `blank`, `first_person`, `third_person`, `top_down`). Create project scaffolds the chosen template
and opens it. `editor.new_project { name, path?, template? }` takes the same ids.
