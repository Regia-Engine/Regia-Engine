# Log stitching and triage

Use `regia logs triage` for an offline tester drop. Use read-only `logs.triage` (MCP tool
`triage_logs`) when the caller already has the bundle text. Neither surface runs bundle contents,
uploads reports, or changes a running game.

```sh
regia logs triage reports/ --json
regia logs triage upload.part02of02.zip upload.part01of02.zip --out triaged --json
regia logs triage session.jsonl --bucket-seconds 10 --storm-factor 6 --storm-min-count 8
```

The CLI accepts directories, extracted bundles with `manifest.txt`, individual JSONL/log files,
and ZIPs (including a complete numbered part set). Input order does not determine stitch order.
ZIP members (stored or deflate) are decompressed in memory and checked against their length and
CRC-32; ZIP64, multi-disk and encrypted archives are refused (extract them first). Symlinks, duplicate ZIP entries, unsafe relative paths, archive wildcard
names, missing parts and conflicting totals are refused. ZIP members are never extracted.
No output directory is needed for analysis. `--out NEW_DIR` refuses existing paths and writes
scrubbed session files plus `report.json`, written last to signal completion. Exit 0 means
analysis completed; inspect `verdict`, gaps and malformed counts to decide whether a session is
healthy. Exit 1 means input, settings, archive or output failure. With `--json`, stdout is one JSON
object: `{ok:true,schema:1,sessions,files}` or `{ok:false,error:{code,message}}`.

## Agent request

```json
{
  "method": "logs.triage",
  "params": {
    "bundles": [{
      "name": "upload_s0001",
      "files": {
        "manifest.txt": "session: session-a\nseq: 1\nentry\tbytes\trange\nlogs/app.jsonl.from-0\t40\t0..100\n",
        "logs/app.jsonl.from-0": "{\"t_ms\":0,\"target\":\"app::start\"}\n"
      }
    }],
    "settings": {"bucket_seconds":60,"storm_factor":10,"storm_min_count":5},
    "include_stitched": false
  }
}
```

Result: `{schema:1,sessions:[...],files:[...]}`. `files` includes session, path, stored byte count,
source bundles/ranges, duplicate count and gaps. `include_stitched:true` adds scrubbed `text`.
The handler never opens filesystem paths. Existing transport and request-size limits still apply;
use the offline CLI for large drops. Permission: Read; rate class: heavy.

A manifest has `session: ID`, `seq: N` headers and a tab-separated table beginning `entry`.
Its third column is the half-open **original source** range `START..END`. A `.from-OFFSET` filename
provides a fallback start; absent a manifest range, its end is offset plus stored UTF-8 bytes.
Scrubbing can change stored length: source offsets are never used to slice scrubbed data.
Exact repeated pieces are counted once. Conflicting or partial overlaps fail. Missing ranges are
listed with their output offset; if needed a newline separates fragments across a gap. Whole
`.summary.json`, `-summary.json`, `-system.json`, and the canonical names `summary.json` and
`summary.partial.json`, use the newest `(seq,bundle name)` copy.
An equal sequence/name tie is deterministic by text. Application-neutral timestamp filenames
`<prefix>-YYYYMMDD-HHMMSS-PID` identify their session. Otherwise a single parent directory is the
session folder (`session-a/summary.json` stays with `session-a/events.jsonl`). Otherwise the
manifest session is used. A canonical summary at the bundle root joins other unstamped files in
that bundle under the bundle name. Otherwise the file stem is used. `logs/` is stripped. Paths
with more than one remaining directory are excluded to avoid double-counting per-match traces.

## Settings

Each agent setting has the corresponding hyphenated CLI flag.

| Setting | Default | Meaning |
|---|---|---|
| `bucket_seconds` | 60 | At least 0.001; rates count events per full bucket width |
| `gap_seconds` | 60 | Report consecutive timed records farther apart than this |
| `storm_factor` | 10 | At least 1; count must strictly exceed median × factor |
| `storm_min_count` | 5 | Positive minimum when median is zero |
| `top` | 15 | Maximum normalized templates; zero hides templates |
| `max_bytes` | 67108864 | Input text/name budget; CLI also charges compressed bytes and expanded text |
| `max_files` | 4096 | Visited directory entries and input files, shared across roots. Extracted bundles count every entry, including irrelevant files. ZIP members count separately against the same file budget |
| `slow_frame_ms` | 50 | Match frame-summary median p50 threshold |
| `slow_rtt_ms` | 200 | Network RTT p95 threshold |
| `loss_percent` | 5 | Network loss p95 threshold, in (0,100] |

Times accept `t_ms`, legacy `t` seconds, `unix_ms`, then `wall`/`timestamp`/`time`/`ts` numeric or
ISO timestamps (UTC when no offset is given). Numeric wall magnitudes below 1e11 are seconds;
otherwise milliseconds. Nonfinite times, and finite magnitudes above 1e20 ms, are untimed.
A finite span whose bucket index exceeds `u64::MAX` saturates `bucket_span` and still stores only
occupied buckets. Blank lines are ignored;
malformed/non-object JSON is counted and skipped. Valid untimed objects still count toward session
diagnostics and are listed separately from timed records. Avoid mixing unrelated clocks in one log.
Event data accepts nested `fields.data`, legacy JSON strings in `fields.data` or `fields.data_json`,
and flat `data`/`fields`. Frame-summary timings are read from `frame_ms.p50` and `frame_ms.p99`,
then legacy `p50_ms` and `p99_ms` on that object, then the same keys on the tracing fields or the
record when the selected object has no timings. Unknown targets are counted without inventing a diagnosis.

Storm medians include every bucket from first to last timed record, including empty buckets for
each target. Storage is proportional to observed buckets, so a long empty span does not allocate
an array for every bucket. Zero-median storms have `ratio:null`, not infinity. Numeric values,
hex values and UUIDs in messages normalize to `#`; template ties sort lexically. Report strings
use Regia's existing report scrubber; avoid treating any pattern scrubber as a guarantee that
arbitrary sensitive data is recognizable.

Final summary `exit.clean` overrides exit events. Canonical `summary.json` /
`summary.partial.json` and stemmed `.summary.json` / `.summary.partial.json` / `-summary.json`
names all count. Only partial summaries means unclean. An invalid
final is unknown. The summary reports warning/error counts, hitches, stalls, phase durations,
frame-summary median p50 and maximum p99, RTT/loss p95, desyncs, late inputs, match count and rates.
Only frame summaries during a match, excluding `tick:0`, affect the frame-rate verdict.
Verdicts in priority order: `crash_or_unclean`, `network`, `frame_rate`, `insufficient_data`,
`no_issue_detected`. They are evidence from the supplied logs, not proof of absence of a fault.

## Errors and verification

| Code | Action |
|---|---|
| `logs_triage_input` | Supply the documented object shape and supported settings |
| `logs_triage_settings` | Use finite positive thresholds and valid count/byte limits |
| `logs_triage_limit` | Reduce the input or explicitly raise the offline budget |
| `logs_triage_empty` | Supply log or session summary files |
| `logs_triage_path` | Remove unsafe paths, symlinks, duplicate archive entries or scrubbed-name collisions |
| `logs_triage_range` | Correct invalid offsets, sequences or reversed/duplicate manifest ranges |
| `logs_triage_overlap` | Supply nonoverlapping source ranges or a complete replacement log |
| `logs_triage_parts` | Supply exactly one copy of every part with consistent totals |
| `logs_triage_archive` | Obtain an intact, supported ZIP archive |
| `logs_triage_io` | Check file access, permissions and disk space |
| `logs_triage_output_exists` | Choose a new output directory |

`split_upload_stitched_in_order` pins reversed input ordering, scrubbed source lengths and newest
final-summary precedence. Unit and CLI tests cover duplicate uploads, gaps, overlap refusal,
malformed tails, timestamp variants, sparse storm medians, archive parts and no-clobber output.
The dispatcher test calls `logs.triage` in a headless app; no window, GPU or wall-clock timing is
needed. This feature creates no frozen content format and changes no simulation state.
