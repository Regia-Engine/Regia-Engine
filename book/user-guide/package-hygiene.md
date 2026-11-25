# Package hygiene

<!-- index: Package hygiene -->
<!-- index: package_hygiene -->
<!-- index: Allowance -->

Shipping packages can reject staged files that contain strings, patterns or terms you list in
`Regia.toml`. The same check runs for an extracted package:

```console
$ regia package --platform linux --project . --audit-dir dist/linux/player --json
```

A clean audit exits 0. A rejected package exits 1 and prints one JSON object. Findings name a
stable rule id, a path, `content` or `name`, and a byte offset. They do not include the rule
text or the bytes that matched. `hygiene.policy_digest` is a `b3:` digest of the rules that
are actually in force.

This audit does not compare a development run with a packaged run. That is
`regia package parity` on [Packaging](packaging.md) and in
[Package a game](../how-to/platforms/package-a-game.md). Pak hashes are
`regia doctor --verify-paks` ([Doctor](doctor.md)).

## Rules with stable ids

Give each project rule an id. `kind` is `string` (exact bytes), `pattern` (a Rust regex) or
`term` (`word:`, `sub:`, `re:`, or a bare substring).

```toml
[[package_hygiene.rules]]
id = "dev_console"
kind = "string"
body = "--developer-console"

[[package_hygiene.rules]]
id = "prototype_word"
kind = "term"
body = "word:prototype"
```

Putting a rule earlier or later in the file does not change what an allowance waives. Two
rules cannot share an id (`duplicate_rule_id`). An id cannot copy a built-in id such as
`secret_github_token`.

Built-in secret checks stay on unless you set `scan_secrets = false`. Their ids do not
change: `secret_private_key`, `secret_webhook`, `secret_aws_access_key`,
`secret_github_token`, `secret_api_key`, `secret_slack_token`, `secret_bot_token`,
`secret_key_file`, and, when `scan_emails = true`, `secret_email`.

## Allowances

An allowance waives one rule on one path, and only while that rule's text is still the text
you reviewed. `definition` is the digest of that text. `reason` must contain a non-blank
review note. The note is not printed in the audit JSON.

```toml
[[package_hygiene.allow]]
rule = "prototype_word"
definition = "<digest from the migration below>"
path = "licenses/*.txt"
reason = "Reviewed public attribution uses this word"
```

If you change `body` and leave `definition` as it was, the package fails. The JSON includes
a finding whose rule is `allowance_obsolete`, whose source is `policy`, and whose path is
`policy/allow/0/prototype_word`. The old and new rule text are not in the JSON. Fill in a
new digest only after you review the new text.

`*` and `?` stay inside one path component. `**` crosses directories. A pattern with no
slash matches that file name at any depth.

## Migrating a positional policy

Older policies numbered rules `forbidden_string_1`, `forbidden_pattern_1` and `deny_term_1`.
An allowance that used those numbers followed the list position, so inserting a line changed
which check it waived.

Load the file you have now, not a hand-edited list, and let `Policy::migrated` record the
rule that each number currently names:

```toml
# Before
[package_hygiene]
forbidden_strings = ["KEEP_ME", "OTHER"]

[[package_hygiene.allow]]
rule = "forbidden_string_1"
path = "reviewed/keep.txt"
reason = "Reviewed keep pair"
```

After migration the same waiver is an explicit rule. `forbidden_string_1` is now an id, not
a position, and `definition` is the digest of `KEEP_ME`:

```toml
[[package_hygiene.rules]]
id = "forbidden_string_1"
kind = "string"
body = "KEEP_ME"

[[package_hygiene.rules]]
id = "forbidden_string_2"
kind = "string"
body = "OTHER"

[[package_hygiene.allow]]
rule = "forbidden_string_1"
definition = "<digest of string KEEP_ME>"
path = "reviewed/keep.txt"
reason = "Reviewed keep pair"
```

You can then reorder the `[[package_hygiene.rules]]` tables. The allowance still waives
`KEEP_ME` on `reviewed/keep.txt`.

Do not edit the old list and leave a stored `definition` expecting the allowance to follow
the text to its new row. If `forbidden_string_1` no longer has the digested body, the
allowance is obsolete. It does not start waiving whatever moved into that slot, and it does
not search the list for the old text.

A policy that has no `allow` tables behaves as before. `forbidden_strings = ["--developer-console"]`
still reports `forbidden_string_1`. You do not need to migrate it until you add an allowance.

`Policy::effective` exposes the same ids and digest without the rule text. A later editor
policy job should read that model instead of numbering list rows.

## CI

| Code | What to do |
|---|---|
| `package_hygiene_failed` | Read `hygiene.findings`. A rule id is a content or name match. |
| `allowance_obsolete` | The allowance's digest does not match the rule. Review the new text or restore the body. |
| `duplicate_rule_id` | Give every configurable rule its own id. |
| `package_hygiene_config` | The policy did not load. `allow_<n>` is an allowance; the message does not quote the rule text. |
| `package_hygiene_io` | A staged file or `Regia.toml` could not be read. |
| `package_hygiene_path` | A symlink, special file or rejected name. |
| `package_hygiene_limit` | A size, entry or finding limit was exceeded. |
| `package_hygiene_pak` | A `.fpak` could not be decoded. |

Agents use `agent.package` with `audit_dir` and `confirm: true`, then `agent.job_status` until
the job is done. The job's `report.hygiene` object is the same JSON as the CLI, including
`policy_digest`. `play.wait_until` and `play.expect` watch a running world; they are not the
wait for this read-only package job.

## Editor policy and audit review

Project Settings ▸ Packaging keeps the six audit ceilings on the existing Save button.
Under that, Package hygiene policy edits the stable rules, allowances and scan flags.
Add rule and Add allowance write immediately, the same way
`editor.package_hygiene_set` does. An allowance needs a rule id, a project-relative glob
and a non-blank reason. The editor stores the definition digest of the rule as it is now.
Preview a change with `save: false` before writing. Undo restores the previous
`Regia.toml`. A value the policy refuses leaves the file, including its comments, unchanged.

Audit directory asks for a folder inside the project and runs
`regia package --audit-dir` without building a package. Cancel stops that job. The editor
stays usable while the audit runs. A cancelled audit does not keep an earlier "clean"
mark, and a result that arrives after cancel is dropped. Problems lists each finding by
its opaque location id, rule id, source and byte offset. It does not copy a raw secret
path. The receipt line stays absent until a later final-artifact report includes a receipt
object. A clean look at one directory is not that receipt.

## Stage versus final evidence

The staging audit checks the files Regia copied into the package directory, before `tar`,
`zip`, `appimagetool` or Gradle builds the archive. It cannot see anything a later step
adds, and it treats nested archives as opaque bytes. Since ADR-8348 a shipping package also
audits the finished artifact itself and writes a receipt beside it:

```text
dist/linux/my-game-linux.tar.gz
dist/linux/my-game-linux.tar.gz.hygiene-receipt.json
```

The final audit reads ZIP files (also APK and AAB), tar, tar.gz and `.fpak` files in place,
including archives inside archives up to `max_container_depth` levels (default 3). Nothing
is extracted to disk and the artifact is never changed, so a signed APK stays signed. The
receipt records the artifact's BLAKE3 hash and size, the policy digest, the tool version,
which containers were opened and what could not be inspected. Dev packages skip the audit
and remove any old receipt.

Some content cannot be decoded: encrypted ZIP entries, unknown ZIP compression methods,
and formats such as zstd, xz, 7z, squashfs or an AppImage payload. They appear in
`coverage.uninspected` and `coverage.complete` is false. To refuse such packages, set:

```toml
[package_hygiene]
strict_containers = true
max_container_depth = 3
```

Duplicate or unsafe entry names (`../` or `..\`, absolute paths, drive letters),
corrupt archives and anything over a limit stop the package with an error. Allowances
written for staging paths still apply: the audit strips the package's top directory
(`my-game/`, or `assets/` and `lib/` in an APK) before matching them.

Check a package someone else built, or check that a receipt still matches its artifact:

```sh
regia package --platform linux --audit-artifact dist/linux/my-game-linux.tar.gz --json
regia package --platform linux --audit-artifact dist/linux/my-game-linux.tar.gz \
  --verify-receipt dist/linux/my-game-linux.tar.gz.hygiene-receipt.json --json
```

Neither command builds or writes anything. Verification re-hashes the artifact and fails
with `package_hygiene_receipt` if the artifact, the policy, the container settings or the
tool version changed. Receipts and errors are redacted like staging reports: no matched
text, no patterns, no absolute paths. This audit never repairs an archive; remove the
offending content from the project and package again.
