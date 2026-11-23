# `config/provenance.ron` (provenance policy, v1)

Spec: `docs/specs/12-assets.md` § 1a and `docs/specs/23-agent-native.md` § 5. Proposed in ADR-2428; read by
`regia assets validate` (validator `provenance_policy`), `regia cook --quality final`, `regia package`, the
editor's Content Browser badges and `editor.asset_provenance`. Registered in `cargo xtask check-formats` as
`provenance-policy`. Frozen from its first release: a later shape change needs version 2, a migrator and
`regia upgrade-project` support (`docs/specs/12-assets.md` § 4a). No earlier version exists. A project without
the file has no policy.

The full example is [`provenance.example.ron`](provenance.example.ron) (the pipeline keeps a byte-identical copy in `crates/regia_asset_pipeline/fixtures/` for `cargo package`, held equal by a test):

```ron
(
    format_version: 1,                  // must be 1; a newer version is refused
    fields: [                           // the project's own `extensions` keys
        (name: "licence", description: "SPDX id or `owner`"),
        (name: "foreign_game_file", values: ["false"]),          // `values`: allowed values (empty: any)
        (name: "third_party_mod_file", values: ["true", "false"]),
    ],
    warn_undeclared: true,              // an undeclared `extensions` key is a warning (default false)
    kinds: {                            // required fields per `.meta` importer id; `*` = every kind
        "*": (required: ["origin", "tool"]),
        "png": (required: ["licence"]),
    },
    tools: [hand, agent, generator, import],      // allowed tools (empty: any); also `unknown`
    origins: ["dcc:*", "https://polyhaven.com/**"], // globs over `origin` (empty: any)
    licence_field: "licence",           // the extension key holding the licence (default `licence`)
    licences: ["CC0-1.0", "owner"],     // allowed licences, exact (empty: any)
    review: (
        required_for: [agent, generator], // tools whose assets need a review (default: none)
        accepted: [owner, named],         // states that count as reviewed (default [owner, named])
    ),
    exclude: ["assets/dev/**"],         // source globs the policy skips
)
```

- Every key is optional. Unknown fields or variants are errors (`deny_unknown_fields`), reported as
  `invalid_config` with the line and column. Optional values are written bare (`licence_field: "licence"`).
- `fields[].name` and kind keys are lowercase ascii, digits and `_` (1 to 64 bytes); a field may not repeat or
  use a built-in name (`origin`, `tool`, `generator`, `seed`, `client`, `session`, `model`, `created_utc`,
  `reviewer`). A `required` entry must be a built-in name, a declared field or the licence field.
- A field is *present* when an optional string is set and not blank, `tool` is not `unknown`, `seed` is set,
  `reviewer` is not `pending`, or an extension key has a non-blank value.
- Globs: `*` any run except `/`, `**` any run including `/`, `?` one character except `/`.
- Rules and their findings (`missing_field`, `tool_not_allowed`, `origin_not_allowed`, `licence_not_allowed`,
  `value_not_allowed`, `unreviewed`, `undeclared_field`, `unreadable_meta`) are listed on
  `book/agents/asset-provenance.md`.
- The file is at most 1 MiB.
- Fuzzed by `crates/regia_asset_pipeline/tests/fuzz.rs` `provenance_policy_never_panics` (listed in the fuzz registry's covered-elsewhere table), fixture = the example above.
