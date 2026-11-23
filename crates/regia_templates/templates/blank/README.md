# {{name}}

A Regia Engine project from the Blank template (`docs/specs/21` § 3, `docs/02-architecture.md` § 10).

## Open it in the editor

From an engine checkout that can build `regia-editor`:

```
regia-editor --project <this directory>
```

The editor opens `assets/levels/main.level.ron` (the startup level in `Regia.toml`).
Play / Pause / Stop run a copy of that level. This template's game crate adds no systems,
so play uses the engine's built-in systems.

## Run the desktop launcher

```
cargo run -p {{crate_name}}_launcher
```

The first build compiles the engine through the path dependency written by `regia new`
(`{{engine}}`). That path is this engine checkout; move the project and the dependency
breaks until you point it at the engine again.

## Game code

`game/src/lib.rs` is the gameplay crate. New scripts go in `game/src/scripts/` and are
registered at the `// REGIA:SCRIPTS` marker (`editor.create_script`, or by hand).
Hot reload of that crate is the editor host in `docs/specs/20-hot-reload.md`.
