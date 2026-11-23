# `generators/<name>.toml` (generator declaration, format_version 1)

Owner: `regia_asset_pipeline::generators` (specs/23 § 6, ADR-2412). One file per generator in the
project's `generators/` folder; the file stem is the generator's name (`[a-z0-9_-]+`, at most 64
bytes). Read by `regia gen list | run | verify` and the editor's `gen.*` agent methods
(`book/agents/generators.md`).

```toml
format_version = 1

[generator]
kind = "command"                 # "command" | "heightfield"
seed = 42                        # u64
description = "Rock scatter"     # optional
command = ["python3", "tools/rocks.py", "--seed", "{seed}", "--out", "{out}"]   # kind = "command" only
timeout_secs = 300               # optional, kind = "command" (default 600)
inputs = ["tools/rocks.py", "data/rocks/"]   # optional
outputs = ["assets/props/rocks/", "assets/props/rocks.prefab.ron"]

[generator.heightfield]          # kind = "heightfield" only
width = 513                      # 2..=8193
height = 513                     # 2..=8193
octaves = 5                      # 1..=16, default 5
frequency = 0.015625             # (0, 1], default 1/64: lattice cells per sample of the first octave
lacunarity = 2.0                 # [1, 4], default 2
gain = 0.5                       # (0, 1], default 0.5
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A newer file is refused with `gen_unsupported_version`; 0 or a missing value is `gen_invalid` |
| `generator.kind` | `command`: an executable; `heightfield`: the built-in heightfield |
| `generator.seed` | Passed to the command (`{seed}`, `REGIA_GEN_SEED`) and recorded in each output's `.meta` |
| `generator.description` | Free text for listings |
| `generator.command` | Program and arguments. Placeholders `{seed}`, `{out}` (the staging root), `{project}`, `{name}`. A relative program path containing a separator is project-relative; a bare name is looked up on `PATH` |
| `generator.timeout_secs` | The longest run (at least 1) |
| `generator.inputs` | Project-relative files or folders the generator reads; each must exist (`gen_input_missing`) |
| `generator.outputs` | Project-relative files, or folders with a trailing `/`, the generator writes under the staging root. At least one; no two may overlap. A folder output owns every file under it |
| `generator.heightfield` | Parameters of the built-in kind. It writes exactly one output ending in `.r16`: `width * height` 16-bit little-endian samples, row major (the terrain's RAW import layout) |

Every path is relative, uses `/`, has no `..`, and may not start with `.regia/`, `generators/` or
`.git` (compared case-insensitively). A folder output needs at least two components (`assets/gen/`,
not `assets/`), because a run removes the files under it that it did not write. A run or verify
refuses (`gen_path_denied`) an output that is a symlink or lies under a symlink leading outside the
project. Every path also takes the shared project-path rule on every host (ADR-4922): no drive
prefix or `:` (`C:x`, `a:b`), no Windows device name (`nul`, `con.r16`), no segment Windows reads as
`..` (`...`); such a path is `gen_invalid`.
Unknown keys are refused (`gen_invalid`), so a typo never silently changes what runs.

**Environment.** A `command` generator does not inherit the environment of whoever runs
`regia gen run` or `verify` (a CI step keeps its secrets there). It starts from an empty one plus
`REGIA_GEN_NAME`, `REGIA_GEN_SEED`, `REGIA_GEN_OUT`, `REGIA_PROJECT_DIR` and, when the caller has
them set, `PATH`, `HOME`, `LANG`, `LC_ALL`, `TMPDIR`, `TEMP`, `TMP`, `SYSTEMROOT`, `SYSTEMDRIVE`,
`WINDIR`, `COMSPEC`, `PATHEXT` and `USERPROFILE` (`generators::GENERATOR_ENV_ALLOW`, ADR-4922). A
generator that needs anything else reads it from a project file named in `inputs`.

The built-in heightfield is fractal value noise: lattice values from a SplitMix64 hash of the
integer point, the seed and the octave, blended with the cubic smoothstep, `octaves` layers summed
with `gain` and `lacunarity` and normalized, quantized to 16 bits. It uses only IEEE add, multiply,
divide and floor, so the bytes are the same on every platform.

**Provenance.** `gen run` records in each output's `.meta` (created when absent, importer id = the
file extension): `provenance: (origin: "generators/<name>.toml", tool: generator, generator:
"<name>", seed: <seed>, created_utc: "...", reviewer: pending)`, keeping project extension fields.
A `.meta` that already names the same generator, seed and origin is left untouched.

**Versioning.** Registered in `cargo xtask check-formats` (`generator`) and `regia upgrade-project`
(a `.toml` file with a top-level `[generator]` table; other TOML files are not this format).
`generator_migration_sources()` is where a later version's steps go.
