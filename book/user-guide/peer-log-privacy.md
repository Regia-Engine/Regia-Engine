# Peer log privacy

When a peer session ends without a clean marker, the next launch writes a local recovery
report. That report is for a person to read. It is not the original log.

The published directory is `unclean-<session>-peer<id>/`. It holds three files:

- `report.txt` names the session and says the previous run did not finish cleanly.
- `lastlines.log` is a bounded tail of the human log lines.
- `privacy.txt` says the report was sanitized and whether the tail is complete.

Before those files are published, home directory paths are replaced with `~`, values that
read as secrets (a token, a password, an API key) are replaced with `[redacted]`, and email
addresses are replaced with `[email]`. The same replacement is what a crash report already
uses. The original `session.log` and the `.hash` checkpoint files in the session directory
are left byte for byte as the session wrote them. Keep that directory private. The recovery
report is the copy that is safe to hand to someone else.

`privacy.txt` is a short list of `key=value` lines. `policy` is `regia.recovery.privacy` and
`policy_version` is `1`. `sanitized=1` means the replacement ran. `complete=1` means every
selected line fit after replacement. `complete=0` means the tail started in the middle of a
line, the window was not valid text, or a line was dropped so the file stayed inside its
size cap. A line is never cut through the middle of a replacement. The cap is the session's
`tail_bytes` setting, and it is never larger than 8 MiB.

A report counts as published only when the directory name starts with `unclean-` and
`privacy.txt` matches this policy. A `pending-` directory is an unfinished write. If
publication is interrupted, that directory is removed and no complete report is left behind.
The publisher will not follow a link, and it will not read a file whose name is not one of
the three names above.

Nothing in this step uploads the report or asks the player for consent. A game that wants to
send the directory somewhere else does that itself, and it should ask first. Telemetry field
lists are a separate setting and are not changed here.

An agent sees the same flags on the existing unclean log record: `sanitized=1`, `complete=0`
or `complete=1`, and `policy_version=1`. The agent guide is
[Peer logs and checkpoint traces](../agents/log-extras.md).
