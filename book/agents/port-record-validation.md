# Port record validation

`cargo xtask notices --check` validates the `[[do27]]` records in
`docs/ported-sources.toml`. This is a repository check, with diagnostics
and a nonzero exit status on failure; it has no runtime protocol method.

A pending record uses `planned` to name its plan lane and has no `files_to`.
When the implementation lands, remove the draft and retain the actual record
with its source revision, range, classification, mark and destination files.
An implementation derived from requirements stays marked `requirements`.

ADR-8242 adds two checks:

- A record cannot contain both `planned` and a nonempty `files_to` list.
- A pending source range cannot overlap a landed range of the same crate and
  path. Ranges are inclusive; `all` overlaps every range. Disjoint pending
  ranges remain valid. Split a partially landed draft around the existing
  landed ranges, preserving every source line still awaiting implementation.

These checks do not authorize copying a source or changing its classification.
The existing cleanroom approval and per-file review remain required. In
particular, the measured-hull fixture excluded by ADR-8164 has no copy draft;
Regia keeps its own vector. The arena implemented under ADR-8151 has one landed
requirements record, not a pending copy record.

Run `cargo test -p xtask --lib do27_ports` for the regression suite. It plants
overlapping draft/landed records in both orders, checks adjacent and touching
ranges, rejects a draft with destinations, and checks the real manifest's copy
coverage, requirements record and excluded-file set.
