# `.regia_assetpkg.ron` (asset package manifest, v1)

Spec: the v0.6 spec addendum § 8.9 (Export with dependencies, Import package). Proposed in ADR-7788; written
and read by the editor (`regia_editor::asset_package`, agent methods `editor.export_package` and
`editor.import_package`, `book/agents/asset-packages.md`); the type is
`regia_asset_pipeline::asset_package::AssetPackageManifest`. Frozen from its first release: a later shape change
needs version 2, a migrator and `regia upgrade-project` support (`docs/specs/12-assets.md` § 4a). No earlier
version exists. Registered in `cargo xtask check-formats` (`assetpkg`).

An asset package is a folder: the packaged assets at their project-relative paths (`assets/...`), each with its
`.meta` sidecar (so asset ids stay), and this manifest at the top. The manifest lists; the files are the assets.

```ron
(
    format_version: 1,                          // must be 1; a newer version is refused (pkg_version)
    roots: [                                    // the assets the user picked
        "assets/props/crate.prefab.ron",
    ],
    assets: [                                   // every packaged file: the roots and their dependency closure
        (
            path: "assets/props/crate.glb",     // project-relative, `/`-separated, under assets/, no `..`
            id: Some("5f1d3c9a-0b7e-4c1a-9d0e-2a6b8c4f1e30"), // the .meta asset id; None without a sidecar
            bytes: 1024,                        // file length
            hash: "a3c7...16 hex digits",       // FNV-1a 64 of the bytes: a transfer check, not a signature
        ),
    ],
    created_utc: Some("2026-11-15T00:00:00Z"),  // optional, RFC 3339 UTC
)
```

Import refuses the whole package, copying nothing, when any listed path already exists in the project
(`kind: path`), any listed id is already used by a project asset (`kind: id`), or a file's length or hash does
not match (`damaged`). Unknown fields are refused (`pkg_bad_manifest`). Fuzzed by
`regia_asset_pipeline::asset_package::tests::manifest_survives_mutation`.
