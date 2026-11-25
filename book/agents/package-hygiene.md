# Package hygiene

Shipping packages check staged bytes before archiving (ADR-8062). Add project-specific
forbidden strings and deny terms to `[package_hygiene]` in `Regia.toml`. The same checks run
through `agent.package`, including dedicated-server packages built by the CLI. Dev builds
retain development strings; an explicit audit always enforces the policy. All package profiles
validate policy before general manifest decoding, so malformed policy produces redacted errors.

```toml
[package_hygiene]
scan_names = true
scan_secrets = true
scan_emails = false
max_file_bytes = 1073741824
max_total_bytes = 68719476736
max_entries = 100000
max_findings = 128
max_memory_bytes = 33554432
scan_window_bytes = 1048576
max_matcher_bytes = 8388608
max_work = 1099511627776

[[package_hygiene.rules]]
id = "dev_console"
kind = "string"
body = "--developer-console"

[[package_hygiene.rules]]
id = "internal_endpoint"
kind = "pattern"
body = '(?i)internal_endpoint_[0-9]+'

[[package_hygiene.rules]]
id = "prototype_word"
kind = "term"
body = "word:prototype"

[[package_hygiene.allow]]
rule = "prototype_word"
definition = "<definition_digest of term and word:prototype>"
path = "licenses/*.txt"
reason = "Reviewed public attribution uses this word"
```

`forbidden_strings` are case-sensitive literal substrings of raw bytes. They find adjacent
string literals merged into a binary even without a NUL or whitespace separator.
`forbidden_patterns` use Rust byte-regex syntax, case-sensitive unless the expression says
otherwise. No external `strings`, Python or shell tool is required.

`deny_terms` use `word:`, `sub:`, `re:` or a bare substring. Word and substring matches are
case-insensitive; a word's boundaries are ASCII letters, digits and underscore. Expressions
use Rust regex syntax (no lookaround or backreferences). Terms inspect binary and text
content, plus relative file/directory names when `scan_names` is true. Ignore files never
exclude shipped data. Empty terms, unknown policy keys and invalid expressions are errors.
There may be at most 1,024 configured rules and allowances combined; patterns are limited
to 16 KiB, compiled regex size to 2 MiB each, and the project manifest to 1 MiB.

The default secret scan checks private-key headers, plaintext Discord/Slack webhook URLs,
AWS access-key identifiers, GitHub tokens, `sk-` API keys, Slack tokens and bot-token shapes.
It checks raw bytes, including binary data. Files named `dev.key`, `id_rsa`, `id_dsa`,
`id_ecdsa` or `id_ed25519` are refused as private-key files even when their contents have no
recognizable header; `.pub` files are not refused by that name rule. Optional email detection checks UTF-8 text
without NULs; it defaults off because public license credits often contain email addresses.
These are signature checks, not proof that a credential is live or that all possible secrets
have been found. Encoded or encrypted secrets and unrecognized formats need project rules.

Configurable rules in `rules` have explicit ids (`dev_console`, `prototype_word`). `kind` is
`string`, `pattern` or `term`. A term body uses the same `word:`, `sub:`, `re:` or bare-substring
syntax as `deny_terms`. Ids are an ASCII letter followed by letters, digits or underscore, at
most 64 characters. Duplicate ids, an id that matches a built-in, and a policy that sets both
`rules` and the legacy lists are `package_hygiene_config` (`duplicate_rule_id` or `rules`).

The legacy lists `forbidden_strings`, `forbidden_patterns` and `deny_terms` still compile.
Without allowances their finding ids stay `forbidden_string_1`, `forbidden_pattern_1`,
`deny_term_1`, numbered from one in list order. Built-in ids stay `secret_private_key`,
`secret_webhook`, `secret_aws_access_key`, `secret_github_token`, `secret_api_key`,
`secret_slack_token`, `secret_bot_token`, `secret_email` and `secret_key_file`.

An allowance binds to one rule id, a path glob and the BLAKE3 digest of that rule's kind and
body (`definition_digest` in `regia_cli::package_hygiene`, text `regia.package_hygiene.definition.v1`,
then kind, a NUL and the body, prefixed `b3:`). A built-in allowance uses `definition_digest("builtin", id)`.
Reordering or inserting explicit rules does not move the waiver. Changing the body and leaving
the digest makes the allowance obsolete: it no longer waives, and the audit adds a finding
`allowance_obsolete` at `policy/allow/<index>/<rule-id>` with source `policy`. That finding
carries no rule text. A removed configurable rule with a stored digest is the same finding.
An unknown id with no digest, a disabled built-in, a blank reason or a bad glob is
`package_hygiene_config` at `allow_<n>`.

`Policy::from_manifest` stores the digest of each unbound allowance against the rule that id
names in that document. `Policy::migrated` then copies a legacy list that has allowances into
`rules`, using the positional ids as stable ids, and keeps any digest already stored. It does
not search the list for a body that matches an old digest, so an edited legacy list is not
silently applied to a different rule. A policy with no allowances is unchanged by `migrated`.
`Policy::effective` is the rule ids, digests, live allowances and `digest` for a later editor
policy job. It omits bodies and reasons.

`hygiene.policy_digest` on the CLI and `agent.package` job report is the BLAKE3 of that
effective policy (`b3:` plus hex). It changes when a rule body, live allowance or limit
changes, and it does not change when explicit rules are only reordered. The digest contains
no rule text.

An allowance names exactly one enabled rule ID and needs a nonblank reason. `*` and `?`
stay inside one path component; `**` crosses components; `**/` also matches zero directories.
A basename-only glob such as `CREDITS.txt` matches at any depth. Matching is case-sensitive.
Absolute paths, backslashes and `.`/`..` components are invalid. No implicit license-file
exception exists; review a specific rule/path when public attribution legitimately matches.
Allowances cannot suppress scan errors or resource limits. The report counts waived hits.

## Audit an existing package

Use an extracted, stable package directory. The audit does not build, execute the player,
modify files or create an archive. A platform argument is still required by `package` but
does not change this portable scan. `--audit-dir` conflicts with `--dry-run`, `--steam` and
`--out`.

```sh
regia package --platform linux --project . --audit-dir dist/linux/player --json
regia agent call agent.package '{"audit_dir":"dist/linux/player","confirm":true}'
```

The agent option is project-relative and must resolve to an existing directory inside the
project. It uses the existing `build` permission, confirmation and single-pipeline-job rules
of [Cook and package](cook-package.md), even though this particular operation only reads.
`dry_run: true` returns the command and cwd without running the audit. `path_outside_project`
refuses an escaping path; `F0003` refuses missing paths and non-directories.

Poll `agent.job_status` with the returned job ID. `result.report.hygiene` contains counters
and sorted findings; `result.ok` is false if the audit fails. No package artifact is created
by audit-only mode, so `result.artifact` is null. Each finding gives `path`, `location_id`, `rule`, `source`
(`content` or `name`) and a zero-based byte `offset`. Only the first match of each rule per
entry/source is retained. Reports and error messages omit patterns and matched content.
`path` is scrubbed display text; `location_id` identifies the entry within this scan (ADR-8229).
Use the ID for review, never the display path as an allowance or filesystem lookup.

The scanner reads regular files only. Symlinks (even internal or dangling), special files,
non-UTF-8 names, control characters, colons and backslashes in entry names are refused.
The tree must remain unchanged during the audit; this is not a sandbox for hostile concurrent
writers. A read failure or exceeded limit never becomes a successful partial scan. Directory
depth is limited to 64. File, total-byte, entry-count and finding limits are configurable.

`max_memory_bytes` (default 32 MiB) is the peak the audit may charge for its own buffers.
`scan_window_bytes` (default 1 MiB) is the window for literals and bounded patterns; it is
raised, up to the memory budget, when a bounded pattern is wider. The window overlap is one
byte shorter than the widest streamed pattern, so a match that crosses a window edge has the
same byte offset as a scan of the whole input. Unbounded patterns, including the default
secret signatures, run on the whole input when that input fits in `max_memory_bytes` and
otherwise stop with `incomplete_reason` `unbounded_regex`. They are never split across
windows. A file larger than 32 MiB therefore fails closed while secret scanning stays on,
unless the project raises `max_memory_bytes` or sets `scan_secrets` to false and keeps only
streamable rules. `max_matcher_bytes` (default 8 MiB) is reserved before each rule is
compiled; a rule that would pass the total is refused and is not compiled. `max_work`
(default 1 TiB) counts bytes presented to matchers. Overlap is not charged twice. A mapped
pak is not charged. An owned pak and a decoded entry are not kept together past the memory
budget (`nested_limit`). A declared entry larger than `max_memory_bytes` stops with
`decompression_bomb` before that buffer is allocated. `limits` on the report repeats the
ceilings that were in force. `peak_memory_bytes`, `work`, `matcher_bytes` and
`matchers_compiled` are the counters for that run.

Decoded `.fpak` payloads are scanned as well as their containers. Entry paths in reports are
`<scrubbed-pak>::<asset-id>`; offsets are relative to the decoded payload. Manifest names are checked
as names, never opened as filesystem paths. Malformed or undecodable paks and duplicate
asset IDs fail; content-hash verification stays with the existing pak integrity checks.
Nested archives and formats other than staged `.fpak` files are scanned as opaque bytes by
the staging audit; the final-artifact audit below opens ZIP, tar and gzip containers. Shipping Android checks staged raw assets, native libraries and the AAB's asset
packs before Gradle compression. Generated dex/resources and final archive integrity stay
with the existing packaging checks. Byte/entry limits apply per audited staging root.

## Failure codes

CLI exit is 0 for a completed clean audit, 1 for an audit failure, and 2 for CLI usage errors.
With `--json`, stdout is exactly one object. These are job report codes, not new JSON-RPC
error numbers.

A policy failure and an incomplete audit are different. `package_hygiene_failed` means the
scan finished (`hygiene.complete` is true) and `hygiene.findings` lists rejected rules. An
incomplete audit started and then stopped: `hygiene.complete` is false, `incomplete_reason`
names why, and the counters cover only the bytes checked so far. That result is not a clean
package and not a finished review. `hygiene` is null when the audit did not start, such
as a missing root or a policy that does not compile, and when a file exceeds `max_file_bytes`
before a scan buffer exists. The error object also carries
`matcher_bytes` and `matchers_compiled` for a matcher-budget refusal.

`incomplete_reason` is empty when `complete` is true. Otherwise it is one of `memory_budget`,
`file_bytes`, `total_bytes`, `work_budget`, `entry_count`, `finding_cap`, `depth`,
`unbounded_regex`, `decompression_bomb`, `pak_resident`, `nested_limit`,
`pattern_wider_than_memory`, `changed_file`, `read_error`, `path` or `pak`.

| Code | Meaning |
|---|---|
| `package_hygiene_failed` | The scan finished and found rejected rules. `hygiene.complete` is true; inspect `hygiene.findings`. |
| `allowance_obsolete` | Finding rule id, not a top-level error code. The named allowance's definition digest does not match the current rule. Path `policy/allow/<index>/<rule-id>`, source `policy`. No rule text. The scan still fails. |
| `duplicate_rule_id` | `package_hygiene_config` location when two configurable rules share an id, or a configurable id copies a built-in id. |
| `package_hygiene_config` | Invalid TOML/policy, expression, allowance or limit; location names the field (`allow_<n>`, `duplicate_rule_id`, `rules`, `rule_id`, `rule_kind`, `rule_body:<id>`, `matcher_budget`). `hygiene` is null. |
| `package_hygiene_io` | Missing/unreadable root, file or manifest, invalid manifest encoding, or a file that changed during the scan (`changed_file`). |
| `package_hygiene_path` | Symlink, special entry or unsupported entry name. |
| `package_hygiene_limit` | A byte, entry, finding, memory or work ceiling stopped the audit. When the scan had started, `hygiene.complete` is false. |
| `package_hygiene_pak` | A pak or its decoded payload could not be validated. |

The reusable Rust API is `regia_cli::package_hygiene::{Policy, Rule, audit_tree, check_tree,
check_project_tree, definition_digest}`. `Policy::migrated` and `Policy::effective` are the
stable-id model. `audit_tree` returns a completed report even when it has findings;
`check_tree` and `check_project_tree` reject those findings. Always handle incomplete-scan
errors as failures. `Policy::from_project` loads and validates policy through the bounded,
redacting manifest reader. Shipping preparation also scrubs build paths, drops internal
notes, enforces the stage allowlist and probes dev markers. `--audit-dir` stays read-only:
it reports those rules when the tree still violates them, and it does not rewrite, delete
or execute files.

## Stage preparation (ADR-8427)

A shipping package calls `prepare_stage` before the content audit. Dev packages skip it.
The same four rule IDs appear in the hygiene report. Successful rewrites and drops are
`actions` (they do not fail the package). Rejections are `findings` and the job code stays
`package_hygiene_failed`. Allowances may name these IDs. Reports still omit matched text.

```toml
[package_hygiene]
path_scrub = "rewrite" # rewrite (default), reject, or off

[package]
stage_allow = ["content/**", "player"]
legal_notices = ["content/legal/**"]
```

`path_scrub` applies to `.json`, `.ron`, `.gltf` and `.txt` that are valid UTF-8 and contain
no NUL. Absolute build paths under `/home/`, `/srv/`, `/media/`, `/tmp/` and `C:\Users\`
(including JSON-escaped backslashes) become the tail after the machine root and one
user or volume segment. `/tmp/` keeps every component after the root. A path with nothing
left to keep fails the file and leaves it unchanged. `reject` never writes. `off` skips the
check. `.toml` files and any file with a NUL or a non-text extension are never rewritten.
Binary files are never rewritten.

`stage_allow`, when present, names the only files that may ship. Globs use the allowance
language. An absent key allows every file. An empty list allows none. Directories are not
checked. A miss is rule `stage_allow`, source `name`.

Notes under a `content` directory are removed: `README`, `NOTES` and `TODO` (optional
extension, case-insensitive) and any `*.md`, unless the path matches `legal_notices` or an
`internal_notes` allowance. A root `README.md` outside `content` stays. Dropped paths are
actions with rule `internal_notes` and source `drop`. The read-only audit reports the same
files as findings and leaves them on disk.

Dev-feature markers `regia-dev-feature` and `regia-cheat-hook` in an ELF, PE or shebang
file are rule `dev_flag`. When the shipping prepare step can execute the file, it also runs
`--regia-dev-probe` and, if that is refused, `--regia-cheat-probe` with only `REGIA_CHEAT=1`
set. Exit 2 means the hook was refused. Exit 0 is a finding with source `probe`. Any other
exit, a timeout, or a file that cannot be started does not fail the package. The audit
reports markers and does not start the program.

Every staged executable detected by magic (ELF, PE, Mach-O, WebAssembly) is also scanned
for private build-machine strings (ADR-10354): the build farm's host names (`the build server`,
`the workstation`, `the test machine`, `the NVIDIA test machine`, `the test Mac`), the user and directory roots
of this network (`piercingxx`, `/media/`, `/srv/`, `/models/`, `/home/`), LAN addresses
(`192.168.`) and tailnet names (`.ts.net`, `tailscale`). Matching is case-insensitive; the
plain `the workstation` compound matches at token boundaries (a PE stores the channel labels
"left main" and "right main" back to back, and `strings` prints `mainright`, which names no
host), everything else matches as a substring. A hit is rule `host_string` with
source `content` and the byte offset in the executable; the report never echoes the matched
text, and the finding path is `[executable]` (the binary's own name could carry a private
host). Rebuild with the workspace's `--remap-path-prefix` privacy remaps (ADR-10354); a
reviewed false positive is an exact exception in the TOML file named by the optional
`[package] host_strings_allow` key of `Regia.toml` (a project-relative path):

```toml
# host-allow.toml
[[allow]]
path = "player.exe"                    # the staged path (basename for the standalone check)
text = "public example /srv/content"   # the exact printable run, 6 bytes or more
reason = "Public documentation example"
```

There are no default exceptions. The same check over any built executable is
`regia_cli::package_hygiene::check_executable_strings(path, allow_file)`, run by
`cargo xtask host-strings [--binary PATH]... [--allow FILE]` (default: the release
`hello_window` sample, built on demand; CI runs it). The read-only `--audit-dir` audit
reports the same rule IDs.

`regia new` and the editor's project scaffold write the same directory-root remaps
into the new project's `.cargo/config.toml`, then append
`--remap-path-prefix=<this machine's home>=/builder` as the last flag of every target
(ADR-10370). rustc applies the last matching prefix, so a plain `cargo build` of that
project does not keep the account name under `/builder`. Copying the template files
by hand does not append the home line. A `RUSTFLAGS` value still replaces the config
flags: restate them there, home directory last. `env!("CARGO_MANIFEST_DIR")` is not
remapped; read `CARGO_MANIFEST_DIR` from the environment at runtime.

Invalid stage globs use `package_hygiene_config` with location `stage_allow_N` or
`legal_notices_N`. `errors.explain` documents `build_path`, `dev_flag`, `stage_allow` and
`internal_notes`.

## Privacy-safe locations and local review

All scanner-produced findings and errors share the same location formatter. It uses
`regia_core::privacy::scrub_report` for home paths, keyed secrets and email spellings.
A path matching a configured literal, pattern, deny term or a built-in token signature is
replaced entirely with `[redacted]`. Whole-path replacement also handles overlapping
patterns and matches inside UTF-8 bytes. Display privacy applies even when name, email
or secret detection is disabled, or a matching rule is allowed. Unknown secret shapes
still need project rules; this is not a guarantee that every arbitrary private filename
can be recognized.

`location_id` is `location-N`, an ordinal in sorted traversal; root is `location-0`.
Two locations with identical scrubbed text keep different IDs. IDs contain no encoded path,
path hash or secret-derived fingerprint. They can reveal traversal order and are only
stable for the same immutable tree and traversal. Do not compare them across package edits.
Pak entries append `::<asset-id>` to their container ID, sharing that ID across manifest-name
and decoded-content findings; neither reports nor local review expose manifest names.

Incomplete-scan errors include their safe location and ID. CLI JSON uses
`error.message` and `error.location_id`; the Rust error serializer uses `location` and
`location_id`. Pre-traversal policy/manifest errors and the aggregate `package_hygiene_failed` error have a null ID;
its individual findings carry IDs. Unsupported entry names use `invalid_name` or
`non_utf8_name` with an ID, without echoing the name or escaped bytes.
Name-source byte offsets refer to the original name being checked, not to the scrubbed display.

Synthetic regression examples (the fixtures contain no real credentials):

```text
package_hygiene_failed: staged package rejected
  [redacted] [location-1]: forbidden_string_2 at content byte 7
  [redacted] [location-2]: forbidden_string_2 at content byte 7
```

```json
{"ok":false,"error":{"code":"package_hygiene_limit","message":"[redacted]","location_id":"location-1"},"hygiene":null}
```

For an authorized local review, rerun the Rust API
`audit_tree_with_locations(root, policy, &mut callback)` on the unchanged staging tree.
The callback receives `(location_id, exact_normalized_relative_path)` during traversal.
Resolve only the requested ID in a local UI or debugger. Do not send the callback values
to an agent, telemetry, a report file or a shared terminal log. The callback is opt-in;
normal CLI and `agent.package` calls do not install it, and reports retain no reveal map.
It can receive locations before a later scan failure; visitation does not mean acceptance.
Unsupported names are not revealed. A pak entry can be visited for both its manifest name
and decoded payload with the same ID. Allowances always match the original normalized path.

The `hygiene_report_privacy` integration workflow obtains a command through `agent.package`
preview, executes it, and feeds the real JSON to `CliTally`, exactly as package jobs do.
Its local test host publishes that report as a `hygiene_review` event using `push_host`;
this event is a test adapter, not an additional package-job event. One scheduled step runs
`play.wait_until` on the event's opaque ID, then `play.expect` verifies the ID is present
and synthetic filename/content values are absent. No sleeps or time-budget assertions are used.
Run it with `cargo test -p regia_cli --test hygiene_report_privacy`.

## Editor policy, waivers and audit review (ADR-8399)

Project Settings and the agent share one write. `editor.package_hygiene_get` (permission
`read`) returns the rules, allowances, effective thresholds, scan flags, legacy lists and
provenance. `editor.package_hygiene_set` (permission `edit_scene`) applies the same patch
the Packaging page uses. `save` defaults to true. `save: false` returns the policy that
would be written and does not touch `Regia.toml` or the undo stack. A successful save
records one undo step, `Edit package hygiene policy`. Read-only mode rejects the set with
`data.code` `F0012` and leaves the file unchanged.

The patch object may contain `rules`, `allowances`, `thresholds` and `target`. A present
`rules` or `allowances` array replaces that list; an empty array removes it; an absent key
leaves it. JSON `allowances` are stored as TOML `allow`. `target` is
`{scan_names?, scan_secrets?, scan_emails?}` written onto the existing flags. It is not a
new TOML table. `thresholds` accepts the six ADR-8244 ceilings plus `max_entries` and
`max_findings`. Unknown keys are refused. A failed validation does not rewrite the file,
so comments and unrelated keys stay as they were. A successful save uses the existing
pretty printer: unknown keys such as `studio`, `[steam]` and `package_hygiene.note` remain,
and comments do not.

Allowance `definition` is filled from the named rule before the write. A built-in allowance
uses kind `builtin` and the rule id, so the secret pattern text is not hashed. The editor
does not link `regia_cli` and does not copy the scanner. The digest bytes are the ADR-8247
string `regia.package_hygiene.definition.v1`, a newline, the kind, a NUL and the body,
hashed with the workspace `blake3` crate (CC0 OR Apache-2.0).

`data.reason` codes: `hygiene_rule_id`, `hygiene_duplicate_rule`, `hygiene_rule_kind`,
`hygiene_rule_body`, `hygiene_rules`, `hygiene_allowance`, `hygiene_allowance_path`,
`hygiene_threshold`, `hygiene_target`, `hygiene_params`, `hygiene_no_project`,
`needs_confirmation`, `hygiene_audit_busy`, `hygiene_audit_not_found`. Messages do not
quote a rule body. `path_outside_project` is unchanged. Hint:
`see book/agents/package-hygiene.md`.

`editor.package_hygiene_audit` (permission `build`) needs `confirm: true`. Without it, no
process starts (`needs_confirmation`). It runs
`regia package --platform <linux|windows|android> --profile <name> --audit-dir <relative> --project <root> --json`
through the existing job registry as `hygiene_audit:<n>`. `platform` defaults to `linux`
and `profile` to `dev`. Neither is stored in `Regia.toml`. The directory must already
exist inside the project. The call does not build a package and does not publish a staging
folder. One audit runs at a time (`hygiene_audit_busy`).

`editor.package_hygiene_audit_status` (permission `read`) reports `state`, `verdict`,
`clean`, `job`, `findings`, `receipt` and `truncated`. `clean` is true only for the current
generation that finished with no findings and was not cancelled. Starting or cancelling an
audit clears a previous clean result. A worker that finishes after cancel cannot publish.
`verdict` is `running`, `cancelled`, `failed`, `stage_clean`, `stage_rejected` or
`incomplete`. At most 128 findings are copied. More than that is `truncated: true`,
`clean: false` and `verdict: incomplete`.

`receipt` is `{status: "absent"}` unless the CLI JSON contains `receipt`,
`hygiene.receipt`, `report.receipt` or `report.hygiene.receipt`. Then it is
`{status: "present", receipt: ...}` with raw path keys and rule text removed. A stage-clean
audit is not a final-artifact receipt. That object appears only when a later job includes
it. Cancel and a new start clear any previous receipt.

Findings copied into Problems use severity Error, the rule id, validator
`package_hygiene`, the opaque `location_id` as the asset, and the text
`<source> offset <offset>`. Raw keys `raw_path`, `local_path`, `path_raw`,
`filesystem_path` and `absolute_path` are dropped. A kept string that contains one of those
raw paths becomes `[redacted]`. The first Done status of a generation pushes one
`hygiene_review` host event. The event is the scrubbed status object.

Synthetic sample (the value is not a credential). The report's `raw_path` is
`/tmp/sk-PLANTEDSECRETVALUE0001`, the display path is `[redacted]`, `location_id` is
`location-1`, `rule` is `secret_api_key`, `source` is `content` and `offset` is 12. After
`editor.package_hygiene_audit_status`:

```json
play.wait_until {
  "condition": { "events_contain": { "kind": "hygiene_review", "contains": "location-1" } },
  "timeout_ms": 120000
}
play.expect {
  "assertions": [
    { "name": "opaque id", "condition": { "events_contain": { "kind": "hygiene_review", "contains": "location-1" } } },
    { "name": "no planted secret", "condition": { "not": { "events_contain": { "kind": "hygiene_review", "contains": "sk-PLANTEDSECRETVALUE0001" } } } }
  ]
}
```

The condition is already true when status has published the event, so the wait does not
use a sleep. `editor.package_hygiene_audit_cancel` (permission `build`) answers
`hygiene_audit_not_found` when nothing is running. Run
`cargo test -p regia_editor --test hygiene_policy_ui`.

## Final-artifact audit and receipts

Stage evidence and final evidence are different (ADR-8348). `hygiene` on a package report
is the staging scan. After the archive is produced, a shipping package runs the final hook
on each artifact and writes `<artifact>.hygiene-receipt.json` atomically beside it. The CLI
`--json` object of a finished package carries that receipt as `final_audit`, with
`verified` (the receipt still matches the artifact and policy) and `receipt_path`.
`final_audit` is null for dev packages and dry runs. A stale receipt is removed before
every package, so a receipt beside an artifact always belongs to it.

Audit an existing finished package without building, extracting or writing:

```sh
regia package --platform android --project . --audit-artifact dist/android/game-release.apk --json
regia agent call agent.package '{"audit_artifact":"dist/android/game-release.apk","platform":"android","confirm":true}'
```

`audit_artifact` is project-relative and confined like `audit_dir` (`path_outside_project`,
`F0003`). The job's `result.report.final_audit` is the receipt. Add `--verify-receipt
<RECEIPT>` on the CLI to re-hash the artifact and check a receipt instead of rescanning;
success answers `receipt_verified: true`.

Receipt fields, in this order: `schema` (`regia.package_hygiene.receipt`), `version` (1),
`tool` (`regia_cli <version>`), `artifact` (scrubbed file name, `.` for a directory),
`artifact_kind`, `artifact_hash` (`b3:` hex; a directory hashes its sorted path, size and
file-hash listing), `artifact_bytes`, `policy_digest`, `strict_containers`,
`max_container_depth`, `coverage` and `hygiene` (the scan report, same shape as staging).
`coverage` has `supported` (`directory`, `fpak`, `gzip`, `tar`, `tar.gz`, `zip`), `complete`,
`containers` (`location_id`, `path`, `kind`, `depth`, `entries`, traversal order),
`uninspected` (`location_id`, `path`, `kind`, `reason`), `archive_entries` and `max_depth`.
Equal artifact bytes and policy give byte-identical receipts; no timestamp or absolute path
is recorded.

Inspection: ZIP central directory with local-header name and method cross-checks, zip64,
CRC-32, overlap checks; tar ustar, pax `path`/`size` and GNU long names; gzip members;
nested `.fpak` through the pak scan. Entry locations are `<artifact>::<entry>` and nest
(`game.zip::Game/content/base.fpak::<asset-id>`). Offsets are relative to the decoded entry.
Bytes outside inspected ZIP payloads (headers, central directory, APK signing block,
comment, gaps, raw bytes of uninspected entries) and tar headers are content of the
container location, so owner names in tar headers are checked. Each container counts
against `max_entries`; each entry against `max_file_bytes`, `max_total_bytes`,
`max_memory_bytes` and `max_work`. A nested container too large to hold within the memory
budget (a big `.fpak` inside a ZIP or tar.gz) is streamed once, scanned as it streams, into a
generated spool file (`.regia-hygiene-spool-<pid>-<n>`) in the artifact's directory, opened
from there and removed; entry names never become file names. If the spool cannot be written
the entry fails with `nested_limit`. A deflate entry that expands past its declared size
fails with `decompression_bomb`. Allowances match the in-archive path and the path with the
packager's root prefix removed (`<slug>/` for desktop and server archives, `assets/` and
`lib/` for an APK, `base/assets/` and `base/lib/` for an AAB).

Uninspected `reason` values: `encrypted` and `compression_method` (`kind` `zip_entry`), and
`unsupported_container` (`kind` `zstd`, `xz`, `bzip2`, `7z`, `rar`, `squashfs`, `lz4`,
`appimage`, `cab`, `xar` or `ar`). With `strict_containers = true` the audit refuses them.

| Code | Meaning |
|---|---|
| `package_hygiene_archive` | Malformed or ambiguous archive. `incomplete_reason` `archive` (bad record, CRC or size mismatch, local/central name mismatch, overlapping entries, trailing data) or `duplicate_name` (two entries equal up to ASCII case). |
| `package_hygiene_path` | Also: an entry name with `..`, `.` or empty components, or an absolute or drive path (`traversal_name`, location `traversal_name`, never the name; a backslash counts as a separator, so `..\` is caught too); a control character or non-UTF-8 name (`path`); a tar link or special entry (`path`). |
| `package_hygiene_limit` | Also `depth` when a container is nested deeper than `max_container_depth`. |
| `package_hygiene_uninspected` | Strict policy and at least one uninspected entry; location and `location_id` name the first one. `hygiene` is the completed report. |
| `package_hygiene_receipt` | `--verify-receipt` mismatch; `error.message` is the first failing field: `receipt`, `schema`, `version`, `tool`, `hygiene`, `strict_containers`, `coverage`, `max_container_depth`, `policy_digest`, `artifact_bytes` or `artifact_hash`. |

The Rust API is `regia_cli::package_hygiene::{audit_artifact, check_artifact,
verify_receipt, write_receipt, remove_receipt, receipt_path, ArtifactOptions, Receipt}`.
Safe remediation belongs to staging sanitation (C-10); release-archive jobs (C-15..17)
register their outputs by calling the same check and receipt writer. This audit never
repairs or rewrites an archive. A shipping package whose final artifact fails keeps the
archive on disk without a receipt; treat a missing or unverified receipt as not passed.

The `hygiene_final_artifact` integration workflow previews `agent.package` with
`audit_artifact`, runs the command, feeds its JSON to `CliTally`, publishes the job result as a
local `final_artifact_receipt` host event, then `play.wait_until` on the artifact hash and
`play.expect` on the schema and the absence of absolute paths. Run it with
`cargo test -p regia_cli --test hygiene_final_artifact`.
