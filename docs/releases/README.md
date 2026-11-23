# Release notes

One note per tag. A game or shell that pins a Regia Engine tag reads the note of every tag it crosses, starting with
**Upgrade: host changes since ...** (`regia engine info --json` and the `regia upgrade-project` plan list them as `docs/releases/v<line>.*.md`).

## Notes per tag

| Tag | Commit | Date | Note |
|---|---|---|---|
| v0.2.0 | da3995894 | 2026-10-24 | [v0.2.0.md](v0.2.0.md) |
| v0.2.1 | 340850ab3 | 2026-10-26 | [v0.2.1.md](v0.2.1.md) |
| v0.2.2 | 015594917 | 2026-10-28 | [v0.2.2.md](v0.2.2.md) |
| (v0.2.3, not tagged) | - | - | [v0.2.3-red-rouge.md](v0.2.3-red-rouge.md), first carried by v0.2.4 |
| v0.2.4 | 7936711d9 | 2026-10-29 | [v0.2.4.md](v0.2.4.md) |
| v0.2.5 | 28fb09309 | 2026-10-29 | [v0.2.5.md](v0.2.5.md) |
| v0.2.6 | 806750dbb | 2026-10-30 | [v0.2.6.md](v0.2.6.md) |
| v0.2.7 | e4defa90d | 2026-10-30 | [v0.2.7.md](v0.2.7.md) |
| v0.2.8 | 18bfbe1bb | 2026-10-31 | [v0.2.8.md](v0.2.8.md) |
| v0.2.9 | 57bae2ea1 | 2026-10-31 | [v0.2.9.md](v0.2.9.md) |
| v0.2.10 | 4306ead36 | 2026-11-01 | [v0.2.10.md](v0.2.10.md) |
| v0.2.11 | 58e0b02dc | 2026-11-02 | [v0.2.11.md](v0.2.11.md) |
| v0.2.12 (candidate `integ/v033`) | 298822d11 | draft | [v0.2.12-draft.md](v0.2.12-draft.md) |
| v0.2.13 (candidate `integ/v034`) | f612a1ada | draft | [v0.2.13-draft.md](v0.2.13-draft.md) |
| v0.5.0 (covers v0.2.14 to v0.2.17 too) | 59ff4b61d | 2026-11-15 | [v0.5.0.md](v0.5.0.md), sources in [v0.5.0-section-map.md](v0.5.0-section-map.md) |
| v0.6.0 (covers the untagged v0.5.1 line) | 02381fd90 | 2026-11-20 | [v0.6.0.md](v0.6.0.md), sources in [v0.6.0-section-map.md](v0.6.0-section-map.md) |

The v0.1.x notes (`v0.1.0.md` to `v0.1.29.md`) are from the line named Ferrite, before the rename (ADR-1388). <!-- rename-keep -->

## How a note is made (ADR-2312)

- **Fragments.** Every lane writes one fragment for its own change, named `docs/releases/unreleased/<lane>.md`, in
  the sections of a note: New / Changed / Fixed / Upgrade / Known issues. Upgrade lists every host-visible API change
  (a new pub struct field breaks struct literals, a new enum variant breaks exhaustive matches, a changed signature,
  a format version bump with its migrator and `regia upgrade-project` support, a changed default, an agent-protocol
  change). The fragment file never carries a version number: the tag that first contains it decides its release.
- **Older fragments.** The `v0.1.28-<lane>.md` files at the top of this folder are the fragments of v0.2.4 to
  v0.2.13 (the prefix is historical: lanes kept the v0.1.28 brief template after v0.2.0). They stay where they are,
  because the notes above cite them and moving them would conflict with lanes still in flight. A `v0.1.28-*.md`
  fragment added after v0.2.13 is treated as unreleased, exactly like a file in `unreleased/`.
- **At a tag.** The lead lists the fragments the tag adds or changes:
  `git diff --name-status <previous tag> <tag> -- docs/releases/`, and consolidates them into `v<tag>.md`
  (title `# Regia Engine v<tag>`, a "Builds on v<previous>" intro, New / Changed / Fixed /
  Upgrade: host changes since v<previous> / Known issues, and Golden changes and Documentation when the fragments
  name any). Consumed `unreleased/` fragments then move to `fragments/v<tag>/<lane>.md` in the same commit, so
  `unreleased/` only ever holds what no tag carries yet.
- **Drafts.** A candidate line (`integ/vNNN`) may carry `v<tag>-draft.md` with the branch and commit in its second
  line; the lead renames it to `v<tag>.md` when the tag is cut. Draft names do not parse as a version, so
  `cargo xtask adr-index` ignores them until then.
- **Names `cargo xtask adr-index` reads.** Only files named `v<digits>(.<digits>)*.md` count as releases for the
  index's "First named in" column; fragments, drafts and this README do not.
