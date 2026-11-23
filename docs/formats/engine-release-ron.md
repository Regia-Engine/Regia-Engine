# `regia-engine.release.ron` (packaged engine release manifest, format_version 1)

Owner: `regia_cli::distribution::release` (spec 94 § 4.1). Sits at the root of a packaged engine release
(`regia-engine-<version>/`), written by `regia engine package` and read by `regia engine verify` and the pin checks
(`read_release`). A pure function of the file bytes: sorted paths, no timestamps, so packaging one tree twice gives the
same manifest.

```ron
(
    format_version: 1,
    version: "0.2.13",
    commit: Some("f612a1ada"),
    rust_version: "1.99",
    crates: ["regia", "regia_core"],
    files: [
        (path: "Cargo.toml", size: 512, blake3: "<64 lowercase hex digits>"),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A newer manifest is refused with `release_format_newer` |
| `version` | The engine version (`[workspace.package] version`) |
| `commit` | `git rev-parse HEAD` of the packaged checkout; `None` outside git |
| `rust_version` | `[workspace.package] rust-version` (empty when none) |
| `crates` | Directories under `crates/` that hold a `Cargo.toml`, sorted |
| `files` | Every file except the manifest, sorted by path: `/`-separated `path` inside the release, `size`, `blake3` |

A listed path that is absolute or leaves the release is `release_manifest_invalid`.

**Versioning.** Registered in `cargo xtask check-formats` (`engine-release`) and `regia upgrade-project` since
ADR-2316. `EngineRelease::migrate` is where a later version's step goes. `regia upgrade-project` skips engine trees
(a vendored release is not project content), so its entry only names the format.
