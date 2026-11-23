# Entity kinds and the project map (`kinds.list`, `kinds.of`, `kinds.lint`, `project.describe`)

An **entity kind** is a declared label and contract for one game type: `Crate`, `EnemySpawner`, `Door`. A kind
names what an entity *is* and which components it must carry, and gives it an icon and a color in the editor. The
**project map** is a generated overview of a project: kinds, prefabs, components, the systems that read and write
them, and the levels that use them (`docs/specs/101-entity-kinds-and-project-map.md`, ADR-2488). The user's side is
[Entity kinds and the project map](../user-guide/entity-kinds.md).

Composition stays the model: an entity is any set of components. Kinds are opt-in, and **kinds are not a class
hierarchy**: no kind has a parent kind, nothing is inherited between kinds, an entity has at most one kind, and a kind
never changes which systems run. Capability marker: `entity_kinds` (in `agent.hello`).

## Where a kind comes from

- **Code kind**: a zero-size marker component implementing `regia_scene::kind::EntityKind`, registered with
  `app.register_kind::<T>()`. Its required components are the marker's `#[require(...)]` list.
- **Data kind**: a `.kind.ron` file (`docs/formats/kind-ron.md`) plus the `regia_scene::kind::KindTag("Name")`
  component on the entity. The editor loads the project's `.kind.ron` files (under `assets/`) when it opens the
  project; `kinds.list` and `kinds.of` also read the app's project declarations on each call, so data kinds are listed
  with `source.type: "data"` and a tagged entity reports `declared: true`. A declaration that differs from a code kind
  of the same name gets a drift note; an unreadable file gets a note `<path>: <code>: <message>` (ADR-5609).
- **Prefab**: a prefab root that carries a marker or a `KindTag` names its prefab's kind. Every instance inherits it:
  the instance root (the entity with `PrefabInstance`) reports the kind of the template root under it
  (`via_prefab: true`).

An entity with two code markers reports the first by name and lists the rest in `conflicts`. A `KindTag` naming no
declared kind reports the name with `declared: false`.

## Roles and unclassified entities

Entities without a kind get a **derived role** from their components, first match wins: `sun`, `spot`, `point`,
`camera`, `decal`, `audio`, `character`, `vehicle`, `trigger` (a sensor collider), `body`, `mesh`, `collider`,
`terrain`, `probe`, `spline`, `volume`, `ui`. Each role has a family (`light`, `camera`, `decal`, `audio`, `actor`,
`trigger`, `physical`, `terrain`, `shape`, `ui`); a mesh with a collider and a rigid body is one `physical` job. An
entity with **no kind and two or more families** is *unclassified*: the outliner marks it with `?` and the lint
suggests "two roles on one entity, consider a child".

## Methods

| Method | Params | Result |
|---|---|---|
| `kinds.list` | — | `{ count, categories: [{ category, kinds: [{ name, icon, color, category, description, required, source }] }], notes }` |
| `kinds.of` | `entity` | `{ entity, kind, declared, info, via_prefab, conflicts, role, roles, families, unclassified }` |
| `kinds.lint` | `project_dir?`, `strictness?` | `{ strictness, levels, prefabs, findings: [{ file, entity, code, severity, name, message }], errors, passed }` |
| `project.describe` | `project_dir?`, `write?`, `out_dir?` | `{ map, markdown, written, record_pending }` |

`project.describe` can write `project-map.md` and `project-map.json`, so it needs the `edit_scene` permission and
counts as a mutating call (ADR-3285); a `read_only` client reads the map offline with `regia describe`.

`scene.query_entities` takes `kind` (a kind name, `none`, or `unclassified`) and every row carries `kind`;
`scene.get_entity` reports `kind`, `role` and `unclassified`. The editor's `editor.hierarchy` nodes carry `kind`,
`role`, `unclassified` and `category`.

`kinds.lint` reads the project's level and prefab files (`project_dir` defaults to the app's project root; paths are
confined to the allowed roots) at the project's `[kinds] strictness`, or the `strictness` given. Finding codes:

| Code | Meaning | At `warn` | At `deny` |
|---|---|---|---|
| `kind_missing` | A level entity with game components (or two roles) has no kind | warning | error |
| `kind_required_missing` | A kinded entity lacks a component its kind requires | warning | error |
| `kind_unknown` | A `KindTag` names no declared kind | warning | error |
| `kind_conflict` | Two code markers on one entity | warning | error |
| `kind_two_roles` | Two role families on one entity: consider a child | note | note |
| `kind_overridden` | An instance overrides its prefab's `KindTag` | note | note |
| `kind_parse` | A file could not be read | warning | error |

Exempt from `kind_missing`: entities with one engine role (a light, a static mesh), group nodes, and every entity
under a kinded entity (its parts). The cook and code kinds: offline tools know a code kind through a `.kind.ron`
whose `marker` names the marker type, or through the code kinds of a committed `project-map.json` from a live run.

`project.describe` builds the map from the live world: the `KindRegistry`, the project's files and the system access
of every schedule. The agent serves requests inside `PreUpdate`, which is out of the world while it runs; the first
call asks the agent plugin to record every schedule once from `Last` (`record_pending: true`), so call it again a frame
later for the complete system list. Schedules still unread are listed in `map.unread_schedules`. With `write: true`
the map is written to `project-map.md` and `project-map.json` in `out_dir` (default: the project folder). Output is
deterministic: sorted lists, project-relative paths, no time stamps.

```json
{"method": "project.describe", "params": {"write": true}}
```

## Error codes

| Code | When | Fix |
|---|---|---|
| `kind_not_found` | `kinds.of`: the entity does not exist | `scene.query_entities` lists the entities |
| `kind_bad_param` | `strictness` is not `off`/`warn`/`deny`; a `.kind.ron` field is invalid | Fix the value the message names |
| `kind_version_newer` | A `.kind.ron` is newer than this build (a `kinds.list` note, not an RPC error) | Upgrade the engine |
| `kind_parse` | A `.kind.ron`, level or prefab file did not parse (a `kinds.list` note or a `kinds.lint` finding, not an RPC error) | Fix the file |
| `describe_no_project` | `project_dir` is not a folder | Pass a folder with `Regia.toml` and `assets/` |
| `describe_write_failed` | The map could not be written | Check `out_dir` is writable |

## From the command line

`regia describe` writes the same map offline (no systems: the CLI does not link the game) or, with `--live`, from the
project's running app. `regia preflight` runs the lint and exits 1 on an error finding. `cargo xtask check-kinds`
runs it over the samples.
