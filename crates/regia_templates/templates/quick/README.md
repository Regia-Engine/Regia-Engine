# {{name}}

A Regia Engine quick app: one crate, one `src/main.rs`. Run it with

```
cargo run
```

The first build compiles the engine through the path dependency written by `regia new`
(`{{engine}}`). Move the project and that path must be updated.

A release binary should not name the machine that built it. The engine's own data and the
packaging gate keep private build-machine strings out; if your code needs the project
directory at runtime, read the `CARGO_MANIFEST_DIR` environment variable instead of
`env!("CARGO_MANIFEST_DIR")` (the compile-time form embeds this machine's absolute path in
the binary), and if you set `RUSTFLAGS`, put your build machine's home directory last among
your `--remap-path-prefix` flags (rustc applies the last matching one; see ADR-10354).

`src/main.rs` has three functions:

- `model` creates the game state once (a `#[derive(Reflect)]` struct, so the agent protocol and
  hot reload can see it);
- `update` advances it every frame from input and time;
- `view` draws it with `q.draw()` (2D, window-centred, +Y up) or `q.draw_3d()`.

When the game outgrows one file, read "Sketch → App → systems → project" in the Regia Engine book:
the same `Quick` parameter works in ordinary Regia Engine systems, and `regia new --template blank`
gives the full project layout (editor, launcher, Android).
