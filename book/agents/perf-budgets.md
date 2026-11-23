# CPU budgets (`cargo xtask perf-budgets`)

`perf-budgets` runs the CPU measurements the specs' budgets rest on and says which budgets hold. `docs/perf/budgets.ron`
ties each budget to a release `cargo test` or `cargo bench` command and to the number to read from its output
([the format](../reference/formats.md)). There are no GPU measurements here.

The command is repository tooling: it adds no engine API and no agent method. An agent runs it through the shell and reads
`--json`.

## Running it

```text
cargo xtask perf-budgets                               # everything: table on stdout, exit code 0
cargo xtask perf-budgets --filter regia_scene          # by name, spec or crate
cargo xtask perf-budgets --filter "spec 32"
cargo xtask perf-budgets --json                        # the report as JSON on stdout
cargo xtask perf-budgets --list                        # what would run; nothing is spawned
cargo xtask perf-budgets --counts-only --enforce       # the CI gate: allocation counts only
cargo xtask perf-budgets --update-baseline             # write this run's numbers into the baselines
```

| Option | Default | Meaning |
|---|---|---|
| `--filter <text>` | all | Only entries whose name, spec or crate contains the text. Nothing selected is an error, so a typo in a CI line cannot read as green. |
| `--enforce` | off | Exit 1 when an entry is over budget, regressed or not measured, or a command failed. Without it the exit code is 0 whatever was measured. |
| `--baseline <file>` | `docs/perf/budget-baseline-<host>.json` | The times to compare with; with `--counts-only`, the counts. |
| `--update-baseline` | off | Also write the measured numbers; the report still compares with the old ones. Numbers a `--filter` left out stay, and names the registry lost go. |
| `--counts-only` | off | Only the `perf-count` entries. They do not depend on the machine, and their baseline is `docs/perf/budget-counts.json`. |
| `--json` | off | The report on stdout instead of the table. Progress stays on stderr. |
| `--list` | off | Print the selected entries and exit. |
| `--tolerance <ratio>` | 1.5 | A time regressed when it is more than this many times its baseline. |
| `--count-tolerance <ratio>` | 1.0 | The same for a count; 1.0 means any growth. |
| `--host <name>` | this machine | The name in the baseline's file name. |
| `--registry <file>` | `docs/perf/budgets.ron` | Another registry. |

Each command's output is in `<target>/perf-budgets/<hash>.log`, and the last report in `<target>/perf-budgets/report.json`.

## Reading the report

The table is markdown, so a run pastes into `docs/perf-log.md`. Each `rows[]` element of the JSON has:

| Field | Meaning |
|---|---|
| `name`, `spec`, `package`, `stat`, `note` | From the registry. |
| `kind` | `time` (values in nanoseconds) or `count` (values per operation). |
| `budget`, `measured`, `baseline` | The numbers; `null` when absent. |
| `margin_percent` | `(budget - measured) / budget`, in percent. Negative is over budget. |
| `baseline_ratio` | `measured / baseline`. |
| `budget_verdict` | `within`, `over`, `unbudgeted` or `not_measured`. |
| `baseline_verdict` | `ok`, `regressed`, `improved`, `no_baseline` or `not_measured`. `improved` is not a failure; it says the baseline is stale. |
| `error_code`, `error` | Set when no number was read. |

The top level has `format_version`, `host`, `failed_commands` (each `command` and exit `code`) and `pass`, which is what
`--enforce` turns into the exit code.

### Error codes

| `error_code` | Meaning | What to do |
|---|---|---|
| `metric_missing` | The regex matched nothing, or the command did not run. | Read the log. A compile failure shows here, and the command is in `failed_commands`. |
| `metric_ambiguous` | The regex matched more than one place. | Tighten the registry's regex. |
| `metric_unreadable` | The match does not hold the statistic. | The printed line and the registry's `stat` disagree. |

## Rules for reading a number

- A time compares only with one from the same machine. `over` on a shared host at load 100 says less than `over` on the
  reference machine; the host, the load and the commit belong next to any number you quote (`docs/perf-log.md`).
- Budgets are for the reference machine their spec names, often a desktop or a min-spec class. The entry's `note` says so.
- A count compares everywhere. If a count entry regresses, an allocation really appeared, or the code path changed; update
  the baseline in the commit that means it.
- A miss is reported, not fixed here, and a budget is never widened to make a run pass (ADR-2704).

## Adding a measurement

1. Write an `#[ignore]` test that prints `perf-micro <name>: n= p50= p99= max=`, or a criterion bench. Assert that the work
   happened, never a time.
2. For a count, install a counting global allocator in that test binary and print
   `perf-count <name>: ops= allocs= bytes=` from a test whose name holds `perf_count`, so the count command stays cheap.
   Count the measuring thread only, run the work single-threaded (the multi-threaded executor moves systems between threads
   with the host's load), and assert that two rounds give the same number.
3. Add the entry to `docs/perf/budgets.ron`, run `cargo xtask perf-budgets --filter <name> --update-baseline`, and commit
   the baselines with the host and load in the message.

The counts-only enforce check is also the final step of `cargo xtask preflight`; it calls the same harness directly,
so it does not start another Cargo process or run timed measurements. A count regression makes preflight fail.
