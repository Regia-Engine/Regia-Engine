# API stability snapshots

`docs/api-snapshots/<crate>.txt` lists every public item of the crates under the semver policy of
`specs/57` § 11.1: every public crate (`STABLE_CRATES` in `xtask/src/api_snapshot.rs`, 32 crates, ADR-2527) and the
module-scoped `regia_editor.extension.txt` (`SCOPED_SURFACES`: the module third-party editor tools compile
against). What counts as public is in `docs/api-review.md` "API policy". `cargo xtask api-snapshot` compares them with the sources. It runs in
`cargo xtask preflight` and in CI (ADR-1942).

## What fails

| Change | Result |
|---|---|
| An item is gone | **fail** (`removed`) |
| A signature changed: a type, a generic, a bound, `const`/`unsafe`, a `#[cfg]` | **fail** (`changed`) |
| A new field on a struct whose fields are all public and that is not `#[non_exhaustive]`: struct literals stop compiling | **fail** (`changed`, on the struct) |
| A struct gains a private field: struct literals stop compiling | **fail** (`changed`, on the struct) |
| A new variant on an enum that is not `#[non_exhaustive]`: an exhaustive `match` stops compiling | **fail** (`changed`, on the enum) |
| A new required trait item: implementers stop compiling | **fail** (`changed`, on the trait) |
| A removed `derive` or trait impl | **fail** (`removed`) |
| An item removed that the old snapshot had `#[deprecated]` (itself or an outer item) | passes, with a note (a retirement) |
| An item removed that was not deprecated | **fail**; named `removed_without_deprecation`, a warning before `freeze`, an error `--breaking` cannot accept after it |
| A new item, method, provided trait method, field of a `#[non_exhaustive]` or private-field struct, or variant of a `#[non_exhaustive]` enum | passes, with a note |
| Doc comments, function bodies, argument names, rustfmt wrapping, private items | no effect |

A new item passes without a snapshot update because it breaks nobody, and lanes that merge in
parallel would otherwise collide on every addition. The release lane refreshes the snapshots.

## Update the snapshots

An addition, at release time or whenever you like:

```sh
cargo xtask api-snapshot --update
```

A removed or changed item, when the break is intended:

```sh
cargo xtask api-snapshot --update --breaking "RenderProfile gained bloom_tint (ADR-1234)"
```

This writes the snapshots and appends one row per item to `BREAKING.md` (version, crate, kind, item,
reason), which is the approved entry `specs/57` § 11.1 asks for. Without `--breaking`, an update that
would drop or change an item writes nothing and exits 1. Commit the snapshot with the code that
changed it, and name the break under Upgrade in the release notes. Before 1.0 a minor version may
break; from 1.0 only a major version may (`specs/57` § 11.1).

`--crate <name>` limits a run to one crate.

**A merge conflict in a snapshot:** never resolve it by hand. Take either side, then run the update
again; the sources decide. A snapshot with a conflict marker fails the check as corrupt.

**A new stable crate:** add it to `STABLE_CRATES` (sorted) and run `--update`. A crate reviewed in
`docs/api-review.md` must be in the list; a test checks that.

## What a line means

Each line is `<public path><TAB><signature>`, sorted. A path is where a user names the item, not
where it is defined; an item re-exported in several places has its signature at the shortest path
and `pub use <that path>` at the others.

| Line | Meaning |
|---|---|
| `App::new` | an inherent method |
| `Sealed.open` | a public field of a struct that cannot be built with a literal |
| `Open::A` | a variant of a `#[non_exhaustive]` enum |
| `Shape::name` | a provided trait method |
| `Literal impl Clone` | a trait impl; `derive(Clone)` when derived |
| `exported!` | a `#[macro_export]` macro |
| `*` | a glob re-export from another crate |

## Limits

The scan reads the source with `syn` and compiles nothing, so it needs no nightly compiler, costs
seconds, and answers the same on every host. A `#[cfg(..)]` item is listed with its condition,
Android items included. That has a price:

- Items a macro invocation expands to are invisible (`bitflags!`, `pub_items!`). A hand-written
  wrapper around one is seen.
- A re-export from another crate is one line naming that crate's path; the target's own signature is
  tracked only when that crate is stable too.
- Types are compared as written: `Vec<T>` and `std::vec::Vec<T>` count as a change.
- A derive inside `#[cfg_attr(..)]` is not listed.

## Deprecations, the freeze and the rotation (ADR-2527)

A `#[deprecated(since = "..", note = "..")]` on an item, an inherent method, a trait default method, a public
field or a variant of a `#[non_exhaustive]` enum adds a line `<path> deprecated<TAB>since <version>: <note>`. Adding
or dropping such a line breaks nobody. Removing the item itself is a retirement (passes) only when the **old**
snapshot had that line, so deprecate in one release, refresh the snapshot, and remove in a later one.

`policy.toml` holds `freeze = false`. With `freeze = true` a removal without a prior deprecation fails the check
and the update, even with `--breaking`.

`cargo xtask api-snapshot --rotate <version>` moves the `BREAKING.md` rows whose Version cell is `<version>` into
`breaking/<version>.md`. Version is the workspace version when the row was recorded. It refuses the workspace's
current version, an unreadable Version cell, and a version with no rows, and then changes nothing. Rotate only
versions that have been released.

`--crate regia_editor.extension` selects the module-scoped snapshot.
