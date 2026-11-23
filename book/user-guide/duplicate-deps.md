# Duplicate dependencies

One crate, two versions in one build is compile time and binary size spent twice. This page shows
the ratchet that keeps the count from growing. Decision record: ADR-3301.

## The check

```text
cargo xtask duplicate-deps [--update]
```

`cargo tree -d --workspace -e normal --locked --all-features` (every shipping target: x86_64
Linux/Windows, aarch64/x86_64 Android) lists every crate the workspace resolves to more than one
version, including duplicates only a Windows adapter, an Android dependency or an optional feature
would pull in. The check compares that list with the checked-in baseline
`docs/ci/duplicate-deps.txt` (one row per crate: name, every resolved version, the reason it
stays):

- A duplicate **not in the baseline fails** — either unify the versions (a `Cargo.lock`-only
  `cargo update -p <crate> --precise <v>`, or bump the direct dependency when the API is
  unchanged), or accept it deliberately: run `cargo xtask duplicate-deps --update` and write the
  reason on the new row. A row without a reason also fails.
- A recorded duplicate that **gains a version fails** — the ratchet exists to stop the list from
  growing, so a merge that adds a third resolved version to a recorded duplicate must be unified
  or accepted with `--update` and a written reason, not noted and passed. Losing a version is a
  note.
- A duplicate that disappeared is reported as a note; `--update` drops the row. The ratchet only
  ever tightens.

`cargo xtask preflight` runs the check after `check-versions`, so a lane that introduces a new
duplicate version fails before the merge gate.

## What not to unify

The coupling groups of `docs/03-workspace-and-dependencies.md` § 2 (`wgpu`/`egui`/`winit`;
`glam` with `bevy_*`/`rapier3d`/`kira`) move together — never bump one member to shrink the
duplicate list. Duplicates that wait on an upstream release (for example a new `tiled` or
`snow`) stay in the baseline with that reason until the parent crate moves.

The wider audit picture — licences, advisories, the SBOM — is in
[Bill of materials and licence audit](sbom.md) and `docs/03-workspace-and-dependencies.md` § 4.
