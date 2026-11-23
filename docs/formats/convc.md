# `<id>.convc` (cooked conversation, format_version 1)

Owner: `regia_framework::conversation::cooked` (spec 56 § 4.3, ADR-1882). Postcard, `format_version` first. `regia
package` cooks every `assets/conversations/<id>.conv.ron` into `conversations/<id>.convc` for the staged locales; a
packaged game (or an Android APK) loads it with `CookedConversation::from_bytes` when the source is absent.

| Field (postcard order) | Meaning |
|---|---|
| `format_version` | u32 varint, 1. Any other version is refused: cook again |
| `asset` | The conversation as authored (`docs/formats/conversation-ron.md`): lines, steps, text keys |
| `clips` | Staged locale → line id → clip: `clip` (path under `assets/`, `None` when the line has none), `duration_us`, `fallback` (the clip belongs to another locale of the chain), `clip_locale` |

Text is not cooked in: it stays in the locale bundles, so a string fix never re-cooks an audio table. A clip missing in a
staged locale with `clip_policy: Require` is the cook error `conv_missing_clip` (a warning otherwise).

**Versioning.** A cook output: a layout change bumps `format_version` and an old file is cooked again, never migrated.
Registered in `cargo xtask check-formats` (`convc`; the hash includes the embedded conversation's own shape) and
`regia upgrade-project` (reports the version) since ADR-2316.
