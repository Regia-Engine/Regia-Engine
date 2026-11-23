# Procedural seed slot isolation (internal primitive)

`regia_procgen::seeds::{element_rng, draw, q, cell_key}` are internal implementation primitives. They have no direct agent-protocol method; the existing `procgen.*` methods exercise complete graph runs and are not a way to call a single slot draw. This page documents the deterministic regression check, not an agent workflow. No lowercase error codes apply to these direct Rust calls.

The contract is in procedural content graphs (`docs/specs/62-procedural-content-graph.md` § 6.2): an element draw is keyed by its element seed, node seed, and fixed slot. The random-access property is checked in `crates/regia_procgen/tests/seed_slot_isolation.rs` against explicit `regia_core::math::Rng::with_stream` construction.

The test table includes signed element and node words, several slots, and signed cell coordinates. The same fixed keys are evaluated in forward, reverse, and interleaved uneven orders, with unrelated streams consumed between requests. It compares `f64::to_bits()` against both the fixed baseline and the independent explicit-stream oracle. Separate checks pin half-step rounding away from zero and saturating float-to-`i32` conversion at finite extremes. Hash outputs are not asserted globally unique.

Run the regression with:

```sh
cargo test -p regia_procgen --test seed_slot_isolation
```

These tests establish deterministic stream addressing and quantization boundaries. They do not claim statistical quality, graph executor parity, or parallel execution behavior.
