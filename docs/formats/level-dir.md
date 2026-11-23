# Level directory (`*.level.d/`), format `level-dir` v1

The directory form of a level (spec 37 § 3): one file per entity, so version control merges per
entity. Authoring only; never shipped. `regia_scene::level_dir`, `LEVEL_DIR_FORMAT_VERSION = 1`.

```text
assets/levels/forest.level.d/
  level.ron                                        # LevelDirFile
  entities/
    8f/8f1c3a00000000000000000000000001.entity.ron # EntityFile, shard = first two hex digits
```

`level.ron`:

```ron
LevelDirFile(
    format_version: 1,
    scene_format_version: 10,
    resources: {
        "regia_scene::level::LevelSettings": (gravity: (0.0, -9.81, 0.0)),
    },
)
```

One entity file (the `entity` record is exactly a `SceneFile` entity record):

```ron
EntityFile(
    format_version: 1,
    scene_format_version: 10,
    entity: (
        guid: "8f1c3a00-0000-0000-0000-000000000001",
        name: "Crate",
        components: {
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
        },
    ),
)
```

Rules:

- The asset id is in `forest.level.d.meta` next to the directory. A path exists in one form only
  (`ambiguous_level_form`).
- The set of entity files is the set of entities; there is no index. File names are the 32
  lowercase hex digits of the guid plus `.entity.ron` (45 bytes), in the shard directory named
  by the first two digits.
- Reading assembles one `SceneFile` with entities sorted by guid; anything under `entities/` that
  is not a shard directory or an entity file is ignored with a warning.
- Every file declares the same `scene_format_version` (`mixed_scene_versions` otherwise); the
  assembled file is migrated once. A newer version is `unsupported_version`.
- Writing renders each entity with the canonical pretty RON of scene files, writes only files
  whose bytes changed (temp file, fsync, rename), deletes files of removed entities only after
  every write succeeded, removes empty shards, and writes the header last.

Migration: the directory form is additive (no `SceneEntity` shape change, no `SCENE_FORMAT_VERSION`
bump). `regia level split|merge` convert losslessly; `regia upgrade-project --write` rewrites every
level directory at the latest versions (mixed versions are migrated in groups and reassembled),
keeping a `.level.d.bak` copy. A future shape change bumps `LEVEL_DIR_FORMAT_VERSION`, adds a step
to `upgrade_level_dir`, and re-records the hash (`cargo xtask check-formats --update`).
