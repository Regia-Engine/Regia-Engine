# Release notes from fragments

`cargo xtask release-notes` turns the release fragments of a revision range into one draft note, lints the fragments,
and moves used fragments out of the way between gates. It is for the person who cuts a tag; a lane only writes its
fragment (`docs/releases/unreleased/<lane>.md`, or `docs/releases/v<major>.<minor>.<patch>-<lane>.md` such as `v0.5.1-<lane>.md`, ADR-7624; the older `v0.1.28-<lane>.md` stays valid).

## Build a draft

```text
cargo xtask release-notes --from v0.2.13 --to HEAD --tag v0.2.14 --out docs/releases/v0.2.14-draft.md
```

- The fragments are the files the range **adds** under `docs/releases/` (`--include-modified` adds the modified ones too).
  Release notes, drafts and the README are never fragments.
- Each fragment is read at `--to`. Its `## New`, `## Changed`, `## Fixed`, `## Upgrade` and `## Known issues`
  sections (and any other `##` section) are split into items: a top-level list entry with its continuation lines, or
  a paragraph. A `###` heading names the area of the items under it.
- The note groups items by section and area. Every item keeps its text; it ends with `<!-- src: <fragment> -->`.
  Identical items from several fragments are merged with all their sources.
- Upgrade and Known issues items that only say there is nothing to report (`No API changes.`) become one
  `<!-- none: ... -->` comment; every other item in those sections is copied word for word, and the command checks it.
- The summary at the top is a placeholder (`<!-- SUMMARY: ... -->`) for the owner-voice text. The output has no
  dates and is the same for the same input.
- The command prints `N fragments, M items parsed, K items in the note` and fails if `M` and `K` differ.

## Lint the fragments

```text
cargo xtask release-notes --check
```

Rules: a `# ` title; `## Upgrade` and `## Known issues`; at least one of `## New`, `## Changed`, `## Fixed`; no empty
section; an Upgrade that is not empty when the commit that brought the fragment in changed a `docs/api-snapshots/*.txt`;
the naming rule. Findings of old fragments are listed in `xtask/release-notes-baseline.txt` and only counted. Any other
finding fails the command. `--write-baseline` rewrites the list; use it for old fragments only.

## Archive

```text
cargo xtask release-notes --from v0.2.13 --to HEAD --tag v0.2.14 --archive
```

After the note is written, the used fragments that exist in the working tree move to
`docs/releases/fragments/v0.2.14/`. Nothing else is renamed or deleted. `cargo xtask adr-index` reads that folder as the
notes of its tag, so "first named in" stays correct.
