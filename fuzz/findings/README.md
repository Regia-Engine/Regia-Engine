# Fuzzing findings

Minimized inputs that once failed a fuzz target (ADR-2723, `book/agents/fuzzing.md`).

- `<target>/<hash>`: fixed. In the normal gate `crates/regia_agent/tests/fuzz_findings.rs` runs each one through its target,
  and `xtask`'s `every_committed_finding_replays_clean` covers the targets that test cannot link, so a fixed crash stays fixed.
  `cargo xtask fuzz replay` does the same by hand.
- `pending/<target>/<hash>`: not fixed yet; the fix is owned elsewhere. Not replayed by the gate. `cargo xtask fuzz replay
  --pending` lists those that still fail. Move a file out of `pending/` with its fix.

`<hash>` is 128 bits of BLAKE3 of the file, in hex. The files are raw input with no header. Files directly in this directory are
not findings.
