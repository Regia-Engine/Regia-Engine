# Release evidence ledger (`cargo xtask release-ledger`)

The v0.5.0 release is tracked in `docs/release/v0.5.0-evidence-ledger.md`: one table row per burn-down item (L01-L07,
M01-M06, A01-A15, B01-B20, C01-C12, N01-N13, D01-D05, X01-X06) with its cut class, item status, job, lane, the lane's
branch head, whether that head is in an integration line, the evidence files, the platforms the evidence covers and what
is still open. The open noncuttable rows are listed at the top. ADR-4935 has the rules.

The command is repository tooling. It adds no engine API and no agent method: an agent runs it through the shell and
reads the printed lines.

## Running it

```text
git fetch origin
cargo xtask release-ledger                      # every check
cargo xtask release-ledger --print-open         # also print the open-noncuttable list the table implies
cargo xtask release-ledger --no-scratch         # host without the lead's scratch disk: scratch paths are skipped
cargo xtask release-ledger --no-ancestry        # clone without the integration refs: ancestry is skipped
cargo xtask release-ledger --repo /path/to/checkout
cargo xtask release-ledger --ledger <file> --scratch <dir>   # another ledger file (repo-relative) or scratch root
cargo xtask release-ledger --strict             # final release-candidate pass: moved lane tips fail
cargo xtask release-ledger --json               # one JSON object instead of text lines
```

It prints one `release-ledger: skipped …` line per skipped check, one `release-ledger: note: …` line per row whose lane branch
(`origin/lane/sprint-<lane>`) moved past the cited head or is missing on the remote (a row to refresh; a failure with
`--strict`), `release-ledger: <n> rows (<status counts>); <n> open noncuttable`, then `release-ledger: ok` (exit 0) or one line per problem and `release-ledger: <n> problems` (exit 1).

`--json` prints one object instead: `format_version` (1), `ok`, `integ_lines`, `rows` (each row's `id`, `item`, `cut`,
`status`, `job`, `lane`, `head` or null, `integration`, `evidence`, `open`, `is_open`), `open_noncuttable`, `problems`,
`notes` and `skipped`. The exit code is the same as the text form's.

## What it checks

- Every burn-down id appears once; no unknown id.
- `Cut` is `noncuttable`, `cuttable` or `optional`; `Status` is `done`, `partial`, `blocked`, `not-done`,
  `in-progress` or `not-run`.
- `Head` is a 7-40 digit hex commit that exists in the repository (`git cat-file -e <sha>^{commit}`), or `—`.
- Every `Evidence` path exists: `scratch:<path>` under the scratch root (lane reports), any other path in the tree of
  the row's head (`git cat-file -e <head>:<path>`), or in the working tree for a row without a head. Absolute paths and
  `..` are rejected.
- `Integration` is a ref from the `Integ lines:` line that contains the head, or `pending integration` exactly when no
  listed line contains it. A row that still says pending after its lane landed fails, so refresh it.
- A `done` row cites evidence; an open row (not `done`, or pending integration) says what is open.
- `Job` is a burn-down job id (`J05`).
- Every `## Supporting lanes` row names its lane once, cites a head that is a commit, and a report that exists.
- The `## Open noncuttable rows` list equals the noncuttable rows that are open.

## Updating a row

When a lane lands, set `Integration` to the line that has it. When a lane is relaunched, replace `Lane`, `Head` and
`Evidence` with the new lane's. Status is the item's acceptance state from the lane's final notes, not the lane's own
`DONE`: a lane can be done while its item still waits for an owner decision or a device run. Then run
`cargo xtask release-ledger --print-open` and paste its list under the top heading.
