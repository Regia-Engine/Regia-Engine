# Play gate

`regia play-gate` checks a key track (`.track.ron`, format `key-track` v1, ADR-8403, `docs/formats/key-track-ron.md`). `check` parses it. `replay` walks it against a log on a virtual clock. `run` (ADR-8452) installs a package into a fresh home and drives physical keys. The same track and the same log produce the same edges.

The packaging acceptance run is still [`cargo xtask package-smoke`](package-smoke.md). `replay` is the scripted half of that check. `run` is the install half: a fresh home, scancodes, and expected log lines counted in frames.

## Commands

```text
regia play-gate check TRACK [--json]
regia play-gate replay TRACK [--log FILE] [--require-clean-log] [--json]
regia play-gate run ARCHIVE TRACK --fresh-home --out DIR [--keymap FILE] [--within-frames N] [--json]
```

`check` parses the track. `replay` walks it. `--log` is a text file. A line `MILLIS|text` is timed. Any other line is visible at time 0, so only the order of those lines matters. `--require-clean-log` needs `--log`. It fails when a line that starts with `{` is not JSON, or when a JSON line has `"level":"ERROR"`. Blank lines and other lines are ignored. `--json` prints one object on stdout.

Exit 0 when the command passes. Exit 1 when an expect misses, the clean-log scan fails, install fails, or the play fails. Exit 2 when the track or the arguments are refused. `run` uses those same exits. Its codes are in the table below.

## JSON

A pass:

```json
{"ok":true,"keys":[{"at_millis":0,"edge":"down","target":"input.confirm","section":""}],"matched":[{"pattern":"player ready","optional":false,"matched":true,"line_index":1}],"notes":[]}
```

`edge` is `down` or `up`. `line_index` is the log line that matched, counting from 0, or `null` on an optional miss. A failure:

```json
{"ok":false,"code":"track_expect_missed","pattern":"player ready","message":"track_expect_missed: player ready"}
```

## Error codes

| Code | Exit | Meaning |
|---|---|---|
| `track_parse` | 2 | The file is not a v1 track (an unknown field is refused and named), a target or a hold list is empty, a pattern is empty or not a valid regular expression, or `--require-clean-log` was set without `--log` |
| `track_version` | 2 | `format_version` is not 1 (read before the rest of the file, so a newer track with other steps still gets this code) |
| `track_io` | 2 | The track or the log could not be read |
| `track_expect_missed` | 1 | A required expect found no unused log line at or before its deadline |
| `track_log_error` | 1 | `--require-clean-log` found an `ERROR` record or a `{` line that is not JSON |
| `play_gate_fresh_home` | 2 | `--fresh-home` was not set. The report is written and `out/home` is not created |
| `play_gate_keymap` | 2 | A keymap value is not a scancode, or a track target has no scancode |
| `play_gate_install` | 1 | The archive could not be installed, or it has no `player` file |
| `play_gate_no_agent` | 1 | The installed build's feature list has no `agent` entry. Nothing was injected |
| `play_gate_expect_missed` | 1 | A required line was not in the player log inside the frame budget. The report names the pattern |
| `play_gate_log_error` | 1 | An error-line pattern matched the player log |
| `play_gate_script` | 1 | `script.json` is not the stand-in this driver reads |
| `play_gate_text` | agent | `play.inject_input` got a scancode and non-empty `text`. That batch writes nothing |

An optional expect that misses is a note, not a failure. The pattern language is the `regex` crate.

## Fresh home

`run` requires `--fresh-home`. Without it the command writes `play_gate.md` and `play_gate.json` and exits 2 (`play_gate_fresh_home`). It does not create `out/home`.

With `--fresh-home`, the package is installed under `OUT/home`. That directory is `HOME`. `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR` are new empty directories (`~/.config`, `~/.cache`, `~/.local/share`, `~/.local/state`, `~/run`). The package lands at `$XDG_DATA_HOME/regia-play/player`. A prior config is not copied.

`ARCHIVE` is an uncompressed ustar, or a directory that already holds `player` and `features.json`. A `.tar.gz` from `regia package` is extracted by the caller first. An entry larger than 256 MiB is refused. A path that leaves the archive (`..`, an absolute path) or a symlink is refused.

`--keymap` is `name = SCANCODE` lines. A value that is not a number is `play_gate_keymap` (exit 2) and nothing is installed. The name is never sent as text. `input.setup_done = 1` is appended when the file does not already set that name. The installed keymap is `keymap.txt` in the package.

## Physical keys

The driver calls [`play.inject_input`](testing.md) with a scancode and a per-run token. It does not send text. A target `scancode:N` is that number. Any other target is looked up in the keymap. A pointer step or a wheel step is a note and is not sent.

A package whose `features.json` list does not contain `agent` stops with `play_gate_no_agent` (exit 1). The driver does not read `script.json` and does not inject. A build that shipped without the agent feature is not given another way in.

The token is 32 hex digits derived from the archive path. Every call in one run shares it. Parent environment keys that look like credentials (a name ending in `_TOKEN`, `GH_TOKEN`, or a name containing `PASSWORD` or `SECRET`) are dropped. `install.log` records key names, never values.

On this path a duration stored as milliseconds in the track counts as that many frames. `replay` still reads those fields as milliseconds.

## Expected log lines

Each track `Expect` is one step. The deadline is the driver's frame clock plus `--within-frames` (default 2400 frames). A miss is `play_gate_expect_missed` (exit 1). The report names the pattern: `no matching log line: <pattern>`.

`run --json` prints the report object (`ok`, `code`, `message`, `exit_code`, `rows`, `calls`). The same object is `OUT/play_gate.json`. `OUT/play_gate.md` is the table. A `|` in a markdown cell is escaped.

Recorded budgets, not waits: install 7200, a second play 2100 frames, a per-run wait 240 frames. The installer copies the archive and does not sleep.

## What this does not do

`replay` and `check` do not install a build. `run` does not extract gzip, does not contact a network, and does not sleep. A package without the agent feature is not driven.
