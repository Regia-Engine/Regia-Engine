# Nightly player sweep

`cargo xtask nightly-sweep` runs the player-sweep scenes once per quality profile and writes one row per profile and
scene to `sweep.csv` and `sweep.json`. It is the frame-time companion to `cargo xtask player-sweep` (which answers
whether a session survives on each machine and posts a pass/fail table). It is developer tooling: it has no
agent-protocol surface.

## Scenes and profiles

| Scene | `do27_chase` run |
|---|---|
| `cold` | first launch, empty pipeline cache, 2 frames |
| `replay` | `--replay-seconds` of play (default 120, 30 s warm-up; under 30 s uses a 2-frame warm-up) on the warm cache |
| `quit` | 1 frame on the warm cache, then a clean exit |

| Profile | Extra `do27_chase` arguments |
|---|---|
| `high` | none (host defaults, native resolution) |
| `mid` | `--tier mid` |
| `low` | `--tier mid --scale 0.67 --upscaler tsr` |
| `high-1080p` | `--size 1920x1080` |
| `mid-1080p` | `--tier mid --size 1920x1080` |
| `low-1080p` | `--tier mid --scale 0.67 --upscaler tsr --size 1920x1080` |

The three 1440p profiles are the default. The 1080p rows are for cards judged at 1080p (GTX 1080, Arc B65 class):
`--profiles 1080p` runs all three, and names can be mixed (`--profiles high,1080p`). The tier and scale are the same as
the 1440p row of the same name; only the output size differs.

Each profile starts from an empty pipeline cache; its `cold` scene seeds the cache for the other two. A scene passes
when the process exits 0 and its `summary.json` reports `exit.clean`. A timeout (exit 124), a non-zero exit or a
missing or unreadable summary is a `fail` row. The crash test stays in `player-sweep`.

## Running it

```sh
cargo xtask nightly-sweep --out target/nightly-sweep --content /path/to/do27-content
```

| Flag | Meaning |
|---|---|
| `--out DIR` | required; receives `sweep.csv`, `sweep.json`, and `<profile>/<scene>/{summary.json,run.log}` |
| `--profiles LIST` | comma list of `high`, `mid`, `low`, `high-1080p`, `mid-1080p`, `low-1080p`, or `1080p` for the last three (default `high,mid,low`) |
| `--replay-seconds N` | measured seconds of each replay (default 120) |
| `--bin PATH` | prebuilt `do27_chase`; otherwise built with `cargo build --release -p lookdev --bin do27_chase` |
| `--content DIR` | DO27 content root (the directory holding `maps/<map>/level.ron`); otherwise `REGIA_DO27_CONTENT` |
| `--timeout-seconds N` | per-launch ceiling (default 900) |
| `--date D` | date written to the JSON (default `date +%F`) |
| `--dry-run` | write the planned rows, launch nothing |

The command uses whatever card the environment selects (`REGIA_GPU_UUID`, `REGIA_GPU_INDEX`, ...). It exits 0
when every row passed and 1 when any failed; the files are written either way. An unknown profile or a
non-positive `--replay-seconds` is an error and writes nothing.

The Switchbacks route is 1728 m at 15 m/s, so the vehicle drives for 115.2 s and then parks. A replay longer than
115 s is shortened to 115 s and its row note says so (`replay shortened from 120 s to 115 s`); a parked camera is
not driving and starts held-view reuse, which would add a one-off bind-group cost to the last seconds. `player-sweep`
(default 600 s) does not shorten its replay but prints how many seconds of it are parked, and takes `--size WxH` for the
cold, replay and quit launches.

The `cold` and `quit` scenes run with `--warmup 0`, so `do27_chase` prints `verdict: n/a (no warm-up frames ...)` for
them instead of a spec 27 § 2.4 pass or fail: the first frames create every pipeline and bind group. The replay rows
keep the verdict.

`--replay-seconds` is simulated time at 60 Hz: the replay measures `N x 60` frames, so its wall time
(`seconds` in the output) is shorter than `N` on a card faster than 60 fps. Each child runs in the workspace, or in
`--out` when the workspace is not on the machine, so a copied `xtask` and `do27_chase` pair runs on a test box that has
no source tree (ADR-1336).

## Output

`sweep.csv` columns: `profile,scene,status,exit_code,p50_ms,p95_ms,p99_ms,hitches,seconds,cache,note`. Numbers
that the summary did not carry are empty cells. Fields containing a comma, quote or newline are double-quoted.

`sweep.json` is `{"schema": 1, "date", "passed", "failed", "rows": [...]}`; each row has the same keys as the CSV
columns, with `null` for missing numbers. Both files are written through a temporary file and a rename.

## Scheduling on the build machines

`maintainer-scripts/nightly-sweep.sh` (gitignored orchestration, not part of the repo) wraps the command the way
`the gate script` and `the GPU job wrapper` do:

- it exits 0 without running while a merge gate holds `<scratch>`, and while `GPU_JOBS_HOLD` exists;
- it builds on lavapipe first, then runs under the card's `flock /tmp/ferrite-gpu-<n>.lock` (taken by `the GPU job wrapper`),
  so nothing compiles while a GPU lock is held. The default card is `bench`; `--card` accepts `bench`, `shared4`,
  `shared5` and `arc`;
- it copies the results to `<scratch>>/`.

A crontab line for 03:30 is in the script header.
