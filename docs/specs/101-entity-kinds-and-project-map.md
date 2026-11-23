# 101 — Entity kinds and project map

Status: proposed with ADR-2488 (owner request 2026-11-04). Crates: `regia_scene::kind` (runtime:
kinds, roles, lint, project map model), `regia_editor` (outliner), `regia_agent` (`kinds.*`,
`project.describe`, `kind` on scene queries), `regia_cli` (`regia describe`, `regia preflight`, the
cook's lint), `xtask` (`check-kinds` in preflight). Agent page: `book/agents/entity-kinds.md`; user
page: `book/user-guide/entity-kinds.md`. No other engine's documents or code were used.

**Everything here is opt-in.** A project that declares no kind and sets no strictness loads, cooks,
draws and simulates exactly as before; no golden image moves and no FixedUpdate system is added.

## 1. Purpose

A level is a flat list of entities, each an arbitrary set of components. That composition stays:
it is the engine's model. What was missing is a way to read a project at a glance:

1. The outliner showed names only, so a crate, an enemy spawner and a trigger looked alike.
2. Nothing said what a game object *is* or what it must carry.
3. Nobody could answer "which systems touch this component, and which levels use this prefab"
   without reading the code.

An **entity kind** is a declared label plus a contract for one game type. The **project map** is a
generated overview of kinds, prefabs, components, systems and levels.

## 2. Entity kinds

### 2.1 What a kind is

A kind has:

| Field | Meaning |
|---|---|
| `name` | Unique label (`Crate`, `EnemySpawner`), `[A-Za-z0-9_]`, 1-64 bytes. |
| `icon` | An icon name from the editor's set (§ 4.3); an unknown name shows the generic kind icon. |
| `color` | sRGB `#rrggbb`. |
| `description` | One line, at most 120 characters. |
| `category` | Optional group name (`Props`, `AI`); kinds without one are in `Uncategorized`. |
| `required` | Component type paths the kind's entities must carry. |

**Kinds are not a class hierarchy.** A kind has no parent kind, no fields of its own, no methods
and no inheritance between kinds: two kinds that share components simply both list them. An
entity has at most one kind. Behaviour stays in systems that query components; a kind never
changes which systems run. Grouping is by `category`, which is display only.

### 2.2 Code kinds

A code kind is a zero-size marker component that implements `regia_scene::kind::EntityKind`:

```rust
use regia_scene::kind::{EntityKind, KindMeta, KindAppExt};

#[derive(Component, Reflect, Default)]
#[reflect(Component, Default)]
#[require(Transform, Health)]          // the contract: bevy_ecs required components
pub struct Crate;

impl EntityKind for Crate {
    const KIND: KindMeta = KindMeta::new("Crate", "crate", [0xc0, 0x8a, 0x3e], "A breakable supply crate")
        .category("Props");
}

app.register_kind::<Crate>();
```

The `entity_kind!` macro writes the `impl` in one line. `register_kind` registers the component and
its reflection, and asserts at compile time that the marker is zero-size (`size_of::<T>() == 0`),
so a kind adds no per-entity memory and no system. The required components are read from the
component's registered required-components list (direct requirements only), so the contract is
the same `#[require]` that inserts them: spawning `Crate` inserts `Transform` and `Health`.

### 2.3 Data kinds: `.kind.ron`

For projects that declare kinds without code, a `.kind.ron` file (format `kind`, version 1, frozen
on merge; `docs/formats/kind-ron.md`) holds the same fields:

```ron
(
    format_version: 1,
    name: "Crate",
    icon: "crate",
    color: "#c08a3e",
    description: "A breakable supply crate",
    category: Some("Props"),
    required: ["regia_scene::transform::Transform", "regia_render::runtime::MeshSource"],
    marker: None,
)
```

An entity carries a data kind through the reflected component `regia_scene::kind::KindTag(String)`
(the kind's name). `marker: Some("my_game::Crate")` binds a declaration to a code kind's marker
type, so tools that do not link the game (the cook, `regia preflight`) recognise the marker; when
both exist the code kind's metadata wins and `kinds.list` reports a `drift` note if they differ.
Data kinds do not insert their required components; the lint (§ 5) checks them.

Loading data kinds (ADR-5609): the editor loads every `.kind.ron` under `<project>/assets` into the
`KindRegistry` when it opens a project (`regia_scene::kind::load_project_kinds`), replacing the
previous project's data kinds; code kinds keep their place. `kinds.list` and `kinds.of` merge the
app's project declarations into a copy of the registry on each call and never change it. A game
that wants data kinds in its own registry calls `load_project_kinds(world, root)`. Only `.kind.ron`
files under `assets/` are read; an unreadable one becomes a registry note
`<path>: <code>: <message>` (`kind_parse`, `kind_version_newer` or `kind_bad_param`).

Version rules: a file with a newer `format_version` is refused (`kind_version_newer`); version 0
does not exist; future versions migrate through `kind_migration_sources()` and `regia
upgrade-project` (the `kind` row). The serde field list is hashed by `cargo xtask check-formats`.

### 2.4 Prefabs name their kind; instances inherit it

A prefab root that carries a code marker or a `KindTag` names its kind. Instantiating copies the
root's components, so every instance root carries the same marker or tag: instances inherit the
kind with no extra field in the scene format (`.prefab.ron` and `.level.ron` are unchanged). A
nested prefab's root keeps its own kind. An instance override of the `KindTag` changes that
instance's kind; the lint reports it as `kind_overridden` (a note, never an error).

### 2.5 Resolving an entity's kind

`kind_of(world, entity)`: the registered code markers the entity carries, in kind-name order; if
none, its `KindTag`. Two or more markers is `kind_conflict` (the first by name is used). A
`KindTag` naming no registered kind is `kind_unknown` (shown with the generic icon and the name).
The registry (`KindRegistry`, a resource) is a sorted list, so every listing is deterministic.

## 3. Roles: the derived classification

Entities without a kind get a **derived role** from their components, by one priority rule shared
by the outliner, the agent, the lint and the map (`regia_scene::kind::roles`). Components are
matched by their short type name within the engine crates (`regia_*`, and the pre-rename
`ferrite_*` spellings in old files). First match wins: <!-- rename-keep -->

| # | Role | Components | Family |
|---|---|---|---|
| 1 | `sun` | `DirectionalLight` | light |
| 2 | `spot` | `SpotLight` | light |
| 3 | `point` | `PointLight` | light |
| 4 | `camera` | `Camera`, `CameraRig` | camera |
| 5 | `decal` | `StaticDecal` | decal |
| 6 | `audio` | `AudioEmitter`, `AudioListener` | audio |
| 7 | `character` | `Character`, `CharacterBody`, `PlayerController`, `AiController`, `NetCharacter`, `NavAgent` | actor |
| 8 | `vehicle` | `Vehicle`, `VehicleController`, `HoverController` | actor |
| 9 | `trigger` | `Collider` with `sensor: true` | trigger |
| 10 | `body` | `RigidBody` | physical |
| 11 | `mesh` | `MeshSource`, `MeshRenderer`, `SkinnedMesh` | physical |
| 12 | `collider` | `Collider` | physical |
| 13 | `terrain` | `Terrain` | terrain |
| 14 | `probe` | `ReflectionProbe` | light |
| 15 | `spline` | `Spline` | shape |
| 16 | `volume` | `Shape` | shape |
| 17 | `ui` | `UiRoot` | ui |

Rows 1-6 are the viewport's `icon_of` order, unchanged (the viewport keeps drawing gizmo icons for
exactly those six; its audio gizmo is drawn for `AudioEmitter` only, so a listener-only entity has
the `audio` role but no gizmo). A mesh with a collider and a rigid body is one role (`body`, family
`physical`): a collider is part of a physical object, not a second job.

**Unclassified.** An entity with no kind whose components span **two or more families** (say a
light and a trigger, or a character and an audio emitter) is *unclassified*: the outliner shows a
visible `?` marker next to its icon and the lint suggests splitting it (§ 5). An entity with no
role at all (a group node) shows no icon.

## 4. Outliner

### 4.1 Rows

Each row shows an icon before the name:

- a kind: the kind's icon glyph tinted with the kind's color; hovering shows `kind: <name>` and the
  description;
- no kind: the derived role's glyph in the neutral text color; hovering names the role;
- unclassified: the derived glyph plus an orange `?`; hovering lists the families found and the
  suggestion "two roles on one entity, consider a child".

The name stays the row's text (search, rename and drag are unchanged). The icon and color are
never the only signal: the tooltip and the `kind:` search say the same in text.

### 4.2 Filtering and categories

The search box accepts `kind:<name>` (exact, case-insensitive), `kind:none` (no kind) and
`kind:?` (unclassified), alone or before a name filter. A **Group by category** toggle in the
outliner header shows one collapsible section per category (kind categories in name order, then
`Uncategorized`, then `No kind`), each holding the matching rows as flat entries; the tree view
returns when it is off. Projects with many kinds use categories to keep the list short.

### 4.3 Icon names

`crate`, `box`, `enemy`, `person`, `door`, `key`, `coin`, `chest`, `flag`, `spawn`, `pickup`,
`weapon`, `vehicle`, `trigger`, `light`, `camera`, `sound`, `tree`, `rock`, `water`, `fire`,
`star`, `gear`, `kind` (generic) plus every role name of § 3. Each maps to a glyph from the editor's
default font; the table lives in `regia_editor::kind_icons`.

## 5. Strictness and the lint

`Regia.toml`:

```toml
[kinds]
strictness = "off"   # off (default) | warn | deny
```

The lint (`regia_scene::kind::lint`) reads level and prefab files (and the project's kind catalog,
§ 6.3) and reports findings, each with a code, a level path, an entity guid and name:

| Code | When | `warn` | `deny` |
|---|---|---|---|
| `kind_missing` | A level entity with no kind (directly or through its prefab) that carries game components (a type outside the engine crates) or spans two families | warning | error |
| `kind_required_missing` | A kinded entity lacks a component its kind requires | warning | error |
| `kind_unknown` | A `KindTag` names no known kind | warning | error |
| `kind_conflict` | Two code markers on one entity | warning | error |
| `kind_two_roles` | Two or more families on one entity (kinded or not): "two roles on one entity, consider a child" | note | note |
| `kind_overridden` | An instance overrides its prefab's `KindTag` | note | note |

Exempt from `kind_missing`: entities with one engine role only (a light, a camera, a static mesh,
a terrain), group nodes, and the entities inside a kinded prefab instance (they are parts of a
kinded object). With `off` nothing is reported, and `regia preflight` and the cook read nothing
(`kinds.lint` still scans the files to report its `levels`/`prefabs` counts). Notes never fail a
run. A file that does not parse is reported as `kind_parse` (warning at `warn`, error at `deny`).

Where it runs:

- `regia preflight [--project .]`: the project-side checks (today: the kind lint); prints findings,
  exits 1 when strictness is `deny` and an error exists.
- `regia cook`: runs the lint before cooking; `warn` prints, `deny` stops the cook with the list.
- `cargo xtask check-kinds` (a step of `cargo xtask preflight`): runs the lint over every sample
  project in `samples/` at its own strictness.

## 6. Project map

### 6.1 Contents

One entry per kind and per prefab, plus levels and an index:

- **kinds**: name, icon, color, category, description, source (`code` with the marker type path,
  or `data` with the `.kind.ron` path), required components, prefabs that name it, levels using it
  (with entity counts).
- **prefabs**: path, asset guid, kind, root components, entity count (`entities`: the root and every
  entity under it; ADR-5609), levels instancing it.
- **components**: for every component a kind requires or a prefab root carries: the systems that
  read it and the systems that write it, per schedule, from the schedule's system access metadata
  (`SystemAccess::Shared` read/write sets; exclusive systems are listed once under
  `exclusive_systems`, since they may touch anything).
- **levels**: path, entity count, kinds used (counts), prefabs used, unclassified count.
- **where to look**: a sorted index `name → file` for every kind (its `.kind.ron` or its marker type
  path), prefab and level.

### 6.2 Output

`project-map.md` (headed sections, tables) and `project-map.json` (`"map_version": 1`), written
atomically. Output is deterministic: every list is sorted, maps are ordered, no time stamps, no
absolute paths (paths are project-relative with `/`), so two runs over the same project produce the
same bytes. The map is regenerated on demand, never on save.

### 6.3 Sources

- `regia describe [--project .] [--out DIR] [--live] [--json]`: scans `assets/**` (`.kind.ron`,
  `.prefab.ron`, `.level.ron`) offline. Without `--live` the components section lists no systems
  (the CLI does not link the game) and says so; with `--live` it connects to the project's running
  app (`regia agent call` discovery) and takes kinds and the system access from
  `project.describe`.
- `project.describe` (agent): the same model from the live world: the `KindRegistry`, the system
  access of every schedule, plus the file scan when `project_dir` is given (or the app's project
  directory is known); `write: true` writes the two files.
- Offline tools learn code kinds from `.kind.ron` `marker` fields and from the `kinds` of an
  existing `project-map.json` written by a live run (its `source.marker`), so a project that commits
  its map can lint code kinds in the cook.

The agent runs its requests in `PreUpdate`, so that schedule is not in the world while a request is
served. The agent plugin records the system access of every schedule once, from `Last` on the first
frame after `project.describe` is first asked (`record_pending: true` in that first answer);
`project.describe` merges that record with what it reads live, and lists any schedule it still
could not read under `unread_schedules`.

## 7. Agent protocol

| Method | What |
|---|---|
| `kinds.list` | Registered kinds (code and data) by category, with icon, color, description, required, source, drift notes. |
| `kinds.of` | `{ entity }` → the entity's kind (or null), derived role, families, unclassified flag. |
| `kinds.lint` | `{ project_dir?, strictness? }` → the lint findings for the project's files (or the loaded world). |
| `project.describe` | `{ project_dir?, write?, out_dir? }` → the map (JSON) and its markdown. |
| `scene.query_entities` | gains `kind` (a kind name, `none` or `unclassified`); every row gains `kind`. |
| `scene.get_entity` | gains `kind`, `role`, `unclassified`. |

Capability marker `entity_kinds`. Error codes: `kind_not_found`, `kind_bad_param`,
`kind_version_newer`, `kind_parse`, `describe_no_project`, `describe_write_failed`. As RPC errors
the methods return `kind_not_found`, `kind_bad_param` (a bad `strictness`), `describe_no_project`
and `describe_write_failed`. A bad `.kind.ron` is not an RPC error: `kinds.list` notes carry its exact
code (`kind_parse`, `kind_version_newer` or `kind_bad_param`), and `kinds.lint` reports any
unreadable file as the finding `kind_parse`, whose message names the cause (ADR-5609).

## 8. Costs

Code kinds: a zero-size component, no system. Data kinds: one `String` per tagged entity. The role
rule runs only when the editor draws the outliner, the agent asks or a tool lints; it is a short
table lookup per component name. The agent plugin adds one `Last` system that does nothing until
`project.describe` is first asked, then reads the schedules once.

## 9. Not in scope

Kind-specific inspector layouts, a kind picker in the spawn menu, auto-adding a data kind's required
components, and kinds on non-scene entities. A kind assignment UI (beyond adding the marker or
`KindTag` component) is a later step.
