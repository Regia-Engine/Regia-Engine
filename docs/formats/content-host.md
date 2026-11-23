# Chunk host record (`content-host.ron`, format `content-host` v1)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.6. Decision record: ADR-1876. Guide:
`book/agents/content-chunks.md`.

Written by `regia cook` / `regia package` next to `content-manifest.ron` when `Regia.toml` has
`[content] base_url`, removed when it does not, and staged into the package's `content/` (not into
`dist/<platform>/chunks/`: the host does not need its own address). A package does not ship
`Regia.toml`, so this is how a running game learns where `external` chunks live.

```ron
(
    format_version: 1,
    base_url: "https://cdn.example.com/game",
)
```

`base_url` is an `http://` or `https://` URL; chunks live at `<base_url>/<build_id>/<file>`.

Version rules: a shape change bumps `format_version`. The file is derived data: an older or missing
file means no HTTP source at startup, and a recook writes the current version. A newer or broken file
is logged and ignored (the game runs without the source); no migrator is needed.
