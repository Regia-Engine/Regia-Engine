# Golden images

Golden images are the owner's (AGENTS.md rule 4, specs/23 § 3.7, ADR-2821). An agent can run a
golden's test and see what it produced, and it can propose a new image with a reason. **An agent
alone can never bless a golden:** no protocol method writes under a `goldens` directory, with any
permission and under any tool scope. The owner accepts at a terminal.

## Running a golden's test: `agent.golden_test`

```sh
regia agent call agent.golden_test '{"name": "sponza"}'
```

<!-- not-a-request -->
```json
{ "job": "golden:1", "name": "regia_test_utils:sponza", "path": "crates/regia_test_utils/goldens/sponza.png" }
```

`name` is the stem of `goldens/<stem>.png`. When two folders hold the same stem, the call answers
`golden_ambiguous` with `data.matches`; name one as `<owner>:<stem>`, where the owner is the first
folder above the file that is not `goldens`. An unknown name answers `golden_unknown` with up to 5 near
names in `data.close`.

The test is found by a scan. Every `tests/*.rs` and `tests/*/main.rs` of the project is searched for
the name as a string literal (a numbered name `shadow_cam3` also as the format string `"shadow_cam{`),
starting with the package that owns the golden; then that package's `src/`. When the literal sits inside a `#[test]` function, only that function runs; otherwise the
whole target does. A test that builds the name any other way cannot be found: the job fails with
`golden_no_test`, and `agent.run_tests` runs it by hand. `dry_run: true` answers the test and the command
and runs nothing.

The method needs `build` (it runs project code, specs/23 § 5) and `read`. No tool scope can hold
`build`, so a tool token cannot call it. One test run at a time, shared with `agent.run_tests`
(`run_tests_busy`).

Poll it like any job ([Jobs and test runs](jobs.md)). Events: `resolved`, then what `agent.run_tests`
streams. The finished job's result is `agent.run_tests`'s, plus `golden`:

<!-- not-a-request -->
```json
{
  "ok": false, "passed": 0, "failed": 1,
  "golden": {
    "name": "regia_test_utils:sponza",
    "path": "crates/regia_test_utils/goldens/sponza.png",
    "expected": "crates/regia_test_utils/goldens/sponza.png",
    "actual": ".regia/agent/golden_jobs/0000019a1b2c3d4e-golden-1/plates/regia_test_utils-sponza.png",
    "diff": ".regia/agent/golden_jobs/0000019a1b2c3d4e-golden-1/diff.png",
    "expected_hash": "9f1c...", "actual_hash": "47ab...",
    "stats": {
      "width": 1280, "height": 720,
      "mean_lum": 96.2, "near_white_pct": 0.3, "near_black_pct": 2.1, "object_pct": 99.4,
      "ssim": 0.971, "psnr": 38.2, "max_channel_diff": 41, "same_size": true, "identical": false
    },
    "test": { "package": "regia_test_utils", "target": "golden_sponza", "filter": "sponza_matches", "file": "crates/regia_test_utils/tests/golden_sponza.rs" }
  }
}
```

`ok` is the test's verdict. `actual` comes from the plate the compare records (`REGIA_PLATE_DIR`,
`regia_test_utils::golden`), on pass and on fail. It is `null`, with a `note`, when the test failed
before its compare or compares some other way. `psnr` is `null` for identical frames or different
sizes. The stats are `measure_golden`'s numbers (a test holds them equal).

A run under the protocol only ever compares: `REGIA_BLESS` and `REGIA_SKIP_GOLDENS` are removed from
the test's environment in both spellings, also for `agent.run_tests`, so an app started with them set
cannot bless or skip anything this way.

## Proposing a new image: `agent.bless_golden`

```sh
regia agent call agent.bless_golden '{
  "name": "sponza",
  "reason": "the sun moved 10 degrees for the new time-of-day default, as asked",
  "job": "golden:1"
}'
```

<!-- not-a-request -->
```json
{
  "id": 7, "set": 3, "name": "regia_test_utils:sponza", "status": "pending",
  "path": "crates/regia_test_utils/goldens/sponza.png",
  "before": ".regia/agent/review/golden_proposals/7/before.png",
  "after": ".regia/agent/review/golden_proposals/7/after.png",
  "diff": ".regia/agent/review/golden_proposals/7/diff.png",
  "stats": { "ssim": 0.971 }, "persisted": true,
  "accept": "regia golden accept 7"
}
```

This writes nothing under any golden directory. It copies before, after and diff into the staging
folder and adds a change to the [review queue](staged-review.md). Permission: `assets`. `dry_run: true` runs
every check and stages nothing.

It is refused without a non-blank `reason` (`golden_reason_required`), and without a finished job of that
golden that really ran: `golden_job_not_found`, `golden_job_mismatch`, `golden_job_not_run` (still running,
cancelled, failed, no test ran, or no frame recorded). Job ids live in the running app, so a proposal
needs a test from this session. `golden_stale` means the golden or the job's image changed after the test.

`review.list`, `review.diff` and the editor's Agent Changes panel show the proposal: its reason, the three
image paths, and a text diff of the hashes and stats. `review.reject` withdraws it, as for any change, and
removes the staged images.

## Who accepts

The owner, at a terminal:

```sh
regia golden list
regia golden accept 7        # shows the proposal, then asks you to type the golden's name
regia golden reject 7 --note "the shadow is wrong"
```

`regia golden accept` refuses unless stdin and stderr are terminals (`golden_needs_terminal`), and there is no
flag or variable that skips the question. It checks the golden still holds the image the proposal replaced
(`review_conflict`) and the staged image is unchanged (`golden_staged_changed`), then installs the proposal's
exact bytes by temp file and rename.

Nothing else accepts. `review.accept` answers `golden_owner_only` in the change's row and leaves it pending,
even for a client holding the `review` permission and whatever name it reports, and so does the editor panel's
Accept button. There is no `bless` permission to grant.

**The reason reaches the commit.** Accepting appends a block to
`.regia/agent/review/golden_commit_message.txt`:

```text
bless golden regia_test_utils:sponza (crates/regia_test_utils/goldens/sponza.png)

Reason: the sun moved 10 degrees for the new time-of-day default, as asked
Measured against the old image: ssim 0.971, psnr 38.2, max_channel_diff 41
Proposed by claude (claude-opus-5-5) from golden:1; accepted by the owner at a terminal (ADR-2821).
```

Commit with `git commit -F .regia/agent/review/golden_commit_message.txt`, then remove the file. The owner's
own `REGIA_BLESS=1 cargo test` workflow is unchanged.

## What this does not stop

The boundary is the protocol: what it refuses is listed under "Where paths are stopped". An agent holding `build` runs project code as the owner's user, and an agent
with a shell and a pseudo-terminal is the owner's user; either can overwrite a PNG. There the backstop is the
sandbox the agent runs in and the git diff the owner reviews. Grant `build` as specs/23 § 5 says.

A first blessing (no image yet) records no plate, so `agent.bless_golden` cannot propose it; use
`REGIA_BLESS` yourself.

## Where paths are stopped

Before any handler runs, the dispatcher refuses a request whose string parameters name a path with a `goldens`
component (`golden_path_denied`), whatever the method: built-in, editor extension or registered. Each string is split on
what no path token holds (spaces, line breaks, quotes, brackets, `,` `:` `=`), so a path inside text counts too, such as
a `.render_job.ron` passed inline. `..`, `\`, case and trailing dots do not hide it. Some keys hold free text and are
not read (`reason`, `note`, `title`, `text`, `description`, `message`, `ron`, `source`, `script`, `content`, `summary`,
`comment`). Methods that take no path or only read or stage are exempt: `agent.golden_test`, `agent.bless_golden`,
`agent.run_tests` (test names), `seeing.compare_images` (base64), `review.list`, `review.diff`, `review.accept`,
`review.reject`, `agent.job_status`, `agent.job_cancel`, `agent.hello`, `agent.guide`, `agent.schema`, `agent.changes`,
`errors.explain`, `logs.read`.

Paths that come from a file or that a handler builds stop lower down: the shared path helpers (`regia_agent::req_path`)
and the render output's `out` and take folders answer the same code. `gen.run` refuses a generator that declares an
output under `goldens/`, and answers `gen_path_denied` for a file a folder output would write or remove there.

A handler with its own file-writing path, reading that path from a file on disk, is the one gap this does not close by
construction. The sweep tests call every method with golden paths, in every string parameter.

## Reasons

Errors carry `data.code` `F0002` (`F0003` for `golden_unknown` and `golden_no_test`, `F0001` for `golden_io`)
and a lowercase `data.reason`.

| Reason | When |
|---|---|
| `golden_unknown` | No golden has that name; `data.close` lists near names. |
| `golden_ambiguous` | Two folders hold that stem; `data.matches` names them. |
| `golden_bad_name` | `name` is not 1..=128 ASCII letters, digits, `_`, `-`, `.` or `:`, or starts with `-`. |
| `golden_no_test` | No test source names the golden as a literal. A failed job's `error.reason`, or a `dry_run` error. |
| `golden_io` | The job folder or the staged images could not be written. |
| `golden_reason_required` | `agent.bless_golden` without a non-blank `reason`. |
| `golden_job_not_found` | No such job in this session. |
| `golden_job_mismatch` | The job is another kind of job, or tested another golden. |
| `golden_job_not_run` | The job is unfinished, cancelled or failed, ran no test, or recorded no frame. |
| `golden_stale` | The golden or the job's images changed since the test. Run it again. |
| `golden_unchanged` | The actual image is identical to the golden. |
| `golden_already_pending` | The golden already has a pending proposal. Withdraw it first. |
| `golden_owner_only` | A per-change result of `review.accept`: only the owner's terminal accepts. |
| `golden_path_denied` | The request names a path under a `goldens` directory. |
| `review_queue_full` | `agent.bless_golden`: 256 changes already wait for review. |
| `run_tests_busy` | `agent.golden_test`: a test run is still going. |
| `run_tests_no_manifest` | `agent.golden_test`: the project root has no `Cargo.toml`. |
| `golden_needs_terminal` | `regia golden accept` without a terminal (CLI error text, not a protocol code). |
| `golden_confirmation_mismatch` | `regia golden accept`: the typed text is not the golden's name. |
| `golden_not_a_proposal` | `regia golden`: the change id is a text change. |
| `golden_staged_changed` | `regia golden accept`: the staged image no longer hashes as proposed. |
| `golden_queue_unreadable` | `regia golden`: the review queue file cannot be read. |
| `review_conflict` | `regia golden accept`: the golden changed since the proposal. |

## Tests

```sh
cargo test -p regia_agent --lib golden_tools       # names, jobs, reports, proposals, the hard rule, REGIA_BLESS
cargo test -p regia_cli --test golden_cli           # the real binary: no terminal, no bypass, a real prompt on a pty
cargo test -p regia_editor --test headless_editor   # every method of a real editor refuses a golden path
```
