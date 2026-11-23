# Mutation testing (`cargo xtask mutants`)

A passing test suite says the code did what the tests asked. It does not say the tests would notice the code being
wrong. `cargo xtask mutants` makes small wrong versions of a crate, one at a time (a function returns a default
value, `<` becomes `<=`, a `!` or a match arm is deleted, `+` becomes `*`), runs the crate's tests against each
version, and lists the versions every test still passed. Each of those **missed mutants** is a behavior no test pins.

The command is repository tooling over the `cargo mutants` tool (ADR-3389, `docs/06-testing-and-ci.md` § 20). It
adds no engine API and no agent method: an agent runs it through the shell and reads `--json`.

## Running it

```text
CARGO_INSTALL_ROOT=tools cargo install cargo-mutants --locked      # once per checkout; tools/ is not committed

cargo xtask mutants --crate regia_core --file crates/regia_core/src/project_path.rs
cargo xtask mutants --crate regia_input --file crates/regia_input/src/rebind.rs --json target/mutants/input.json
cargo xtask mutants --crate regia_input --file crates/regia_input/src/rebind.rs --iterate   # after adding tests
cargo xtask mutants --crate regia_core --from-outcomes target/mutants/mutants.out/outcomes.json
```

| Option | Default | Meaning |
|---|---|---|
| `--crate <name>` | required | Crate whose code is mutated (repeatable). Only that crate's tests run against its mutants. |
| `--file <glob>` | every file | Only these files. A glob with `/` matches the path from the workspace root. |
| `--exclude <glob>` | none | Leave these files out (a file another lane is editing, generated code). |
| `--json <out>` | none | Also write the summary as JSON (`version` 1: `crates`, `files`, `missed`, `timeouts`). |
| `--timeout-mult N` | 5 | Per-mutant test timeout as a multiple of the unmutated test time, at least 60 s. |
| `--jobs N` | 4 | Parallel build trees, at most 6. |
| `--build-threads N` | 8 | Compiler processes shared by all jobs. |
| `--min-mem-gb N` | 45 | Refuse to start below this much available memory (0: off). |
| `--iterate` | off | Rerun only what the last run in the same target directory did not catch, and merge the results. |
| `--doctests` | off | Also run doc tests (each compiles again for every mutant: slow). |
| `--from-outcomes <file>` | none | Summarize saved `outcomes.json` files (repeatable; a later file wins per mutant). Runs nothing. |
| `--skip-list <file>` | `docs/ci/mutants-skip.toml` | The intentionally untested mutants. |
| `--target-dir <dir>` | `target/mutants` | Build copies and `mutants.out`; relative paths resolve against the workspace root. Inside the workspace it must be under `target/`: cargo-mutants copies the tree but never `target/`, so a scratch directory anywhere else would be copied into itself. |

A run takes about one incremental build and test of the crate per mutant: 1,300 mutants of `regia_core` take
about 1.5 hours at 4 jobs. Narrow it with `--file` while working, and run whole crates on a quiet host.

## Reading the result

```text
  score  caught missed timeout unviable skipped  crate
  97.1%     402     12       0      141       5  regia_core
...
missed mutants (12):
  crates/regia_core/src/project_path.rs:90: replace && with || in is_device
```

- **caught**: some test failed. Good.
- **missed**: every test passed. Read the line, work out what the change breaks for a caller, and write a test that
  asserts that behavior. Put it in a new test file of the crate. Run with `--iterate` to see it caught.
- **timeout**: the tests hung (a loop condition became always true). Counted apart from the score.
- **unviable**: the mutant did not compile (a default value for a type with no `Default`). Nothing to do.
- **skipped**: the skip list names it.
- **score**: `caught / (caught + missed)`.

A missed mutant is sometimes **equivalent**: it cannot change behavior (a guard that an earlier check already
guarantees, a setting equal to the library default). No test can catch it. Put it on the skip list and say why.
A missed mutant that shows a real bug gets a minimal fix and a regression test.

## The skip list

`docs/ci/mutants-skip.toml` holds the mutants that are intentionally untested: `Debug` and `Display` bodies, log and
`tracing` text, equivalent mutants. Each entry has a `reason`:

```toml
version = 1

[[skip]]
file = "crates/regia_core/src/memory.rs"
function = "<impl std::fmt::Display for Subsystem>::fmt"
reason = "Display forwards to Subsystem::name, whose strings the protocol tests pin"
```

`file` is exact; `function` (as `cargo mutants --list` prints it), `mutation` (a piece of the mutation text, such
as `with "xyzzy"`) and `line` narrow the entry when given. Give `line` only when two mutants share the function and
mutation text and one of them must stay in the run; an edit that moves the line makes the entry stale, never wider. Skipped mutants are not run. An entry that matches nothing in a file
the run covered prints a warning: the code moved or was removed, so fix or drop the entry.

## The first sweep (2026-11-07)

`regia_core` (`math/geom`, `project_path`, `memory`), `regia_ecs` and `regia_input` (`actions`, `rebind`, `assist`,
`config`). The scores before and after the new tests and the remaining missed list are in the lane report
`sprint-mutation-testing-core`; the tests are the `*_mutants.rs` and `memory_ledger_*.rs` files under each crate's
`tests/`.
