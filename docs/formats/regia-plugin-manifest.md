# `regia-plugin.toml`, format_version 1

The manifest of an extension (spec 57 § 4.1), parsed by `regia_framework::extension::manifest`.
It sits at the root of an extension directory. The manifest is the only thing this format covers;
code and content formats an extension introduces carry their own `format_version`.

| Key | Meaning |
|---|---|
| `format_version` | integer, 1. A higher value is `manifest_too_new` and nothing else is read. |
| `id` | `[a-z][a-z0-9_]{1,31}`; also the prefix of the extension's agent methods |
| `name`, `publisher`, `licence`, `description` | display metadata; `licence` is an SPDX expression shown as written, not parsed |
| `version` | semver |
| `auto_enable` | enabled for every project unless listed in `disabled` |
| `provides`, `replaces`, `conflicts` | virtual capabilities, ids whose `provides` this takes over, ids that cannot load together with this one |
| `platforms`, `profiles` | filters (`linux`, `windows`, `android`, `macos`; `dev`, `shipping`); absent = all, empty list = none |
| `capabilities` | requested capabilities: `systems`, `asset_loader`, `asset_importer`, `editor_panel`, `agent_method`, `render_pass`, `settings`, `content_mount`, `feature_pack`, `process`, `network` |
| `[engine]` | `min_version`, `max_version` (inclusive, optional) |
| `[[dependency]]` | `id`, `version` (semver range), `optional`, `platforms`, `profiles`; unknown keys are errors |
| `[[code]]` | `crate`, `audience` (`game`, `editor`, `server`, `tool`), `phase` (`foundation`, `early`, `default`, `late`), `library`; unknown keys are errors |
| `[[content]]` | `root`, relative, forward slashes, must not leave the directory |
| `[[tool]]` | `name`, `command`, `capabilities`, `timeout_seconds` |

Unknown top-level keys are warnings (`unknown_field`). A `capabilities` line written after a
`[[code]]` table belongs to that table in TOML; the parser accepts it there and merges it.
Paths that are absolute, contain `..`, a backslash or a drive colon are `path_escape`.

Capabilities are a declaration and review aid, not a sandbox: in-process extension code is fully
trusted native code. `systems` and `settings` are granted by enabling; every other capability needs
a grant under `[extensions.grants.<id>]` in `Regia.toml`.
