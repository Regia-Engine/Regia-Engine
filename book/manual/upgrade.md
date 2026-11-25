# Upgrade notes

<!-- index: regia upgrade-project -->

v0.7.2 adds this manual and the hosting work in `docs/releases/v0.7.2.md` (v0.7.1 was never
released; v0.7.2 carries it). One content format moved: `hub.toml` is format 2.
`regia upgrade-project` migrates a version-1 file and leaves the relay off until you turn it on.
The hub handshake stays protocol 1, so a v0.7.0 client keeps working against a v0.7.2 hub. Hub
protocol types gained fields and variants and are `#[non_exhaustive]`; the release note lists the
one-line match fixes. No new CLI subcommand, shader, agent method, or fuzz target was added. Other
frozen formats are unchanged. On Windows, a project an earlier `regia new` wrote with a
`//?/C:/...` engine path does not build ("invalid path url"): run
`regia upgrade-project --engine path:<engine checkout> --write` in it once. When you move a project
onto this checkout, use `regia upgrade-project`, below.

The format rules, the migrator registry and a worked field addition are
[Changing a content format](../changing-a-content-format.md). The generated format table is
[Formats](../reference/formats.md).

## See what would change

From the project directory, a dry run lists files and does not write:

```console
$ regia upgrade-project
```

Each line is a content file and its version. Current files are listed as current. Files on an
older version are listed as needing a rewrite. The command does not modify the project.

## Write the migration

```console
$ regia upgrade-project --write
```

`--write` rewrites files that are on an older version and keeps a `<file>.bak` of each original
([`regia upgrade-project`](../reference/cli.md)). It is two-phase (ADR-8811). Every file and
level directory is migrated first (files in memory, level directories in a staged copy). If any
migrator fails, nothing in the project changes, no staged file and no `.bak` is left, and the
command exits non-zero and lists every error. Only then are the new files staged next to the
sources, the backups written, and the names swapped into place.

A run that is killed during the swap can leave a staging directory whose name is a dot, the
file name, a dot, the process id, and `.stage`. The upgrade plan and the cook do not read it
as a second copy of the level. The next dry run lists it (`--write` would remove it). The next
`--write` removes it and prints a note (ADR-8861).

Loaders still accept an older version and migrate it in memory when the file was not rewritten.
A newer version than the engine knows is rejected with an error that tells you to upgrade the
engine. A release moves each frozen format at most one version step (ADR-7724). A format shape
change needs a version bump, a migrator from the previous version, a corpus fixture of the old
bytes, a row in this command's registry, and `cargo xtask check-formats --update`. The USDA
file from `regia export usda` is not a content format and is not migrated.
See [Upgrading content](../fundamentals/assets.md#upgrading-content) and
[Content formats](../how-to/create/content-formats.md).

## After a pin change

`--engine SRC` moves the project's engine dependencies to `path:<dir>`, `git:<url>#<tag>`, or
`registry:<name>[@<version>]`. `SRC` has to name this engine's version. Run the dry run, read
the list, then `--write`. Cook again before you package.
[Adopting features](../user-guide/adopting-features.md) is the checklist around that pin bump.

## Finished check

A second `regia upgrade-project` with no `--write` lists every file as current, and the project
still builds with `regia run` or `cargo build -p <launcher>`.
