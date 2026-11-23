# Generators

A generator is procedural content declared as a re-runnable asset source (specs/23 § 6, ADR-2412):
a file `generators/<name>.toml` names what runs, its seed, what it reads and what it writes. `regia
gen run <name>` runs it and installs the outputs with provenance `generator + seed` in each
output's `.meta`; `regia gen verify` re-runs every generator into a temporary folder and fails if a
single byte differs from the project (the "pack doctor" rule, built in). The format is
`docs/formats/generator-toml.md`.

```toml
# generators/canyon-ground.toml
format_version = 1

[generator]
kind = "heightfield"          # built in: fractal value noise, 16-bit .r16
seed = 7
description = "Canyon floor height"
outputs = ["assets/terrain/canyon.r16"]

[generator.heightfield]
width = 513
height = 513
octaves = 6
```

```toml
# generators/rocks.toml
format_version = 1

[generator]
kind = "command"
seed = 42
command = ["python3", "tools/rocks.py", "--seed", "{seed}", "--out", "{out}"]
inputs = ["tools/rocks.py", "data/rocks/"]
outputs = ["assets/props/rocks/"]     # a folder output owns every file under it
timeout_secs = 300
```

## The command contract

A `command` generator starts in the project root with:

| Variable / placeholder | Value |
|---|---|
| `REGIA_GEN_NAME`, `{name}` | the generator's name |
| `REGIA_GEN_SEED`, `{seed}` | the seed from the file |
| `REGIA_GEN_OUT`, `{out}` | the staging root: write each declared output at the same relative path under it |
| `REGIA_PROJECT_DIR`, `{project}` | the project root (read inputs from here) |

Nothing else of the caller's environment reaches it, except `PATH`, `HOME`, `LANG`, `LC_ALL`, `TMPDIR`, `TEMP`, `TMP`,
`SYSTEMROOT`, `SYSTEMDRIVE`, `WINDIR`, `COMSPEC`, `PATHEXT` and `USERPROFILE` when they are set
(`GENERATOR_ENV_ALLOW`, ADR-4922): a CI step's tokens stay out. Paths in the file take the shared project-path rule on every
host (no `C:x`, `nul`, `a:b` or `...`; `gen_invalid`).

It must not write into the project. When it exits 0, every declared output must exist under the
staging root (else `gen_output_missing`); files it wrote outside the declared outputs are ignored.
For the same seed and inputs it must write the same bytes: `gen verify` re-runs it and compares.

## Running, verifying

```sh
regia gen list [--project DIR] [--json]
regia gen run <name>... [--project DIR] [--json]     # or --all
regia gen verify [<name>...] [--project DIR] [--json]  # exit 1 and a diff list on any difference
```

`gen run` installs only the files whose bytes changed (each written atomically), removes files under
a folder output that the run no longer writes (with their `.meta`), and stamps each output's `.meta`
with `tool: generator`, `generator`, `seed`, `origin: "generators/<name>.toml"` and `reviewer:
pending`. A `.meta` that already records the same generator and seed is not rewritten, so running a
generator twice leaves the project byte-identical. With `[provenance] require_review_for_shipping =
true`, `regia cook --quality final` refuses generated assets until they are reviewed
([Staged review](staged-review.md)).

`gen verify` never writes the project. Each difference is one line (and one `diffs` entry with
`--json`):

```text
rocks: ok (12 file(s))
canyon-ground: DIFFERS
  canyon-ground: assets/terrain/canyon.r16 differs (first differing byte 100)
error: gen_verify_failed: 1 file(s) differ, 0 generator(s) failed
```

| Diff kind | Meaning |
|---|---|
| `changed` | both have the file, the bytes differ (`first_difference` is the byte offset) |
| `missing` | the run writes it, the project does not have it |
| `extra` | the project has a file under a folder output that the run does not write |

## Agent methods

| Method | Permission | What it does |
|---|---|---|
| `gen.list {}` | `read` | `{ generators: [{ name, ok, kind, seed, description, inputs, outputs, error }] }` |
| `gen.run { name, wait_ms? }` | `build` | runs and installs; `{ generator, kind, seed, outputs: [{ path, bytes, hash, written, provenance_written }], removed }`, or `{ job, state: "running" }` |
| `gen.verify { names?, wait_ms? }` | `build` | `{ ok, generators: [{ generator, files, ok, error }], diffs: [...] }`, or `{ job, state: "running" }`; never writes the project |

**A long generator does not hold the editor.** `gen.run` and `gen.verify` run on a worker as job `gen:<n>`. The call waits
for it up to `wait_ms` (default 5000, at most 25000): a generator that ends in time answers its report or its error, as
it always did; one still running answers `{ "job": "gen:1", "state": "running" }`. Pass `wait_ms: 0` for a generator you
know is long, so the edit world is never held. Then [poll the job](jobs.md):

```sh
regia agent call gen.run '{"name": "rocks", "wait_ms": 0}'
regia agent call agent.job_status '{"job": "gen:1", "since": 0}'   # events: { kind: "output", line } per stderr line
regia agent call agent.job_cancel '{"job": "gen:1"}'               # kills the command's process tree; nothing is installed
```

**What a cancel stops.** On Linux and macOS a generator job's command runs in a process group of its own, and the cancel
kills the whole group, including processes the command started in the background and left running after it exited
itself (`sh -c "...; server &"`). A job that fails (the command, a missing output, a refused path) stops its group the same way; one that succeeds and is not
cancelled leaves what it started running (a generator may start a build server on purpose). A process that leaves the
group (a daemon that calls `setsid`) is not stopped. On Windows the cancel stops the command's live process tree
(`taskkill /T`): a process the command started is stopped only while the command itself still runs; one left behind
after the command exited keeps running (ADR-5974).

The finished job's `result` is the report, and its `error.reason` the code below. One generator job runs at a time
(`gen_busy`). An editor that exits stops a running generator and its process tree first.

**A quiet failure can take up to ~5.5 s to report.** A failing command whose stderr stays open
with nothing written (for example a background process it started holds the pipe) can take up to
~5.5 s to report: a 500 ms grace wait for the stream to end, then up to 5 s for its first bytes,
before `gen.run` answers `gen_failed`. A failing command that writes nothing and exits (its stream
closes) reports at once, as does one that prints to stderr as soon as its stream ends.

`gen.run` and `gen.verify` run project code with the owner's privileges, so they need the `build`
permission, which is not in the default profile: grant it in `.regia/agent.toml`
(`permissions = ["read", "edit_scene", "assets", "play", "build"]`) only to agents trusted like a
developer with a shell (specs/23 § 5). The methods are registered by the editor (windowed or
[headless](headless-editor.md)); the editor's capability list names `generators`.

Error codes (the CLI prints them first; the protocol answers them in `data.code`):

| Code | When |
|---|---|
| `gen_not_found` | no `generators/<name>.toml` (or the name is not `[a-z0-9_-]+`) |
| `gen_invalid` | the file does not parse or breaks a rule (paths outside the project, `..`, overlapping outputs, a heightfield without one `.r16` output, ...) |
| `gen_unsupported_version` | `format_version` is newer than this engine reads |
| `gen_input_missing` | a declared input does not exist |
| `gen_spawn_failed` | the command did not start |
| `gen_failed` | the command exited with a failure status (the end of its stderr is in the message) |
| `gen_timeout` | the command ran longer than `timeout_secs` (default 600) and was stopped |
| `gen_cancelled` | `agent.job_cancel` stopped the run (the job ends `cancelled`) |
| `gen_busy` | `gen.run` or `gen.verify` while another generator job runs |
| `golden_path_denied` | `gen.run`: the generator declares an output under a `goldens` directory ([Golden images](goldens.md)) |
| `gen_output_missing` | the run did not write a declared output (nothing is installed) |
| `gen_path_denied` | an output is a symlink, or a folder on its way is a symlink leading outside the project (nothing is written or removed) |
| `gen_io` | a file operation failed |
| `gen_verify_failed` | `regia gen verify` only: some file differs or some generator failed (exit 1) |
| `no_project` | a `gen.*` call with no project open in the editor |

## Tests

```sh
cargo test -p regia_asset_pipeline --lib generators   # run twice byte-identical, verify catches a hand edit, provenance, command contract, codes
cargo test -p regia_cli --test gen_cli                # the real `regia gen` binary: run twice, verify exit codes and diff list
cargo test -p regia_editor --test headless_editor     # gen.list, gen.run, gen.verify through a headless editor
```
