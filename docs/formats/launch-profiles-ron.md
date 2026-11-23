# Project launch profiles — v1

`config/launch_profiles.ron` is new desktop tool data (ADR-7843, P-342). It belongs to the
project and is never a runtime dependency. The shared loader is
`regia_asset_pipeline::launch_profiles::LaunchProfiles`; the editor manages it through
`editor.launch_profiles`. Unknown fields, duplicates and versions other than 1 are refused.
There is no pre-v1 layout to migrate; `upgrade-project` recognizes current/future versions.
Future shape changes require the normal version/migrator process.

```ron
(format_version: 1, profiles: [
    (name: "Desktop review", target: "standalone", build_profile: "dev", cook: "incremental",
     package: false, device: None, start_level: Some("levels/review.level.ron"),
     extra_arguments: ["--review", "two words"]),
])
```

The scalar and argument fields are required; omitted `device` and `start_level` values
default to `None`. Optional values use RON `None`/`Some(...)`. Names are
unique and case-sensitive (1–128 bytes without control characters); there are at most
256 profiles and the document is capped at 4 MiB. Target is `standalone` or `android`.
Build-profile names use ASCII letters/digits/underscore/hyphen (1–128 bytes). Cook is
`incremental`, `full` or `skip`; packaging can still cook required content. `package` is a
boolean; `device` is an optional adb serial. `start_level` is an optional relative path
under assets (no absolute prefix, backslash, colon, empty segment, control character or
`..`). `extra_arguments` is at most 128 tokens, each at most 4096 bytes without NUL.
Arguments retain token boundaries and are never shell commands.

The corpus fixture is hand-written in
`crates/regia_agent/tests/fixtures/format-corpus/launch-profiles/` and checked by the pipeline
format corpus test. The fingerprint covers both structs' serde field lists; string
choices above are validated by the loader. See [the agent contract](../../book/agents/editor-launch.md)
for execution limits, including deferred Android overrides.
