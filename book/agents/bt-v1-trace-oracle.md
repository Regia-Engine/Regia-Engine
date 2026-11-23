# Legacy behavior-tree trace regression

Run the bounded CPU regression with:

```sh
cargo test -p regia_ai --test bt_v1_trace_oracle
```

This exercises the existing `BehaviorTree::compile` / `tick` surfaces, without
adding an engine agent method. Existing coverage is the **Behavior tree**
`.bt.ron` row and `ai.bt_get`, `ai.bt_validate`, `ai.bt_edit`, `ai.bt_live` rows in
[COVERAGE](../COVERAGE.md). For interactive inspection, use the existing
[behavior tree editor](behavior-tree-editor.md) and [AI](ai.md) recipes.
The ordered test leaf log is test instrumentation, not a protocol field.

The v1 contract comes from `docs/specs/22-tier2.md` T2.2,
`docs/formats/bt-ron.md`, the AI guide's parallel tuning notes, and the Rust
documentation for `ParallelPolicy`, `NodeState::running_child` and `BtTask::reset`:

- Selector advances on Failure; Sequence advances on Success. Both suspend at
  the Running child and start at the first child after their own completion.
- Inverter swaps Success and Failure and preserves Running.
- Parallel visits unfinished children in authored order, including siblings
  after a decisive result. Terminal child results remain latched until the
  parallel completes. RequireAll fails on any Failure and succeeds when all
  succeed; RequireOne succeeds on any Success and fails when all fail.
- Parallel completion clears descendant control progress for the next run.
  The regression does not promise reset callback counts, ordering, or effects.

The reference interpreter uses recursive nodes and completed-child receipts;
the compiled runtime uses its existing flat arena and resume cursors. Each tick
must have the same root result and exact ordered `(leaf_id, returned_status)`
log. A failure prints the shape, independent leaf-script indices, zero-based
tick, and preceding frames as a reproducible counterexample.

The generated grammar is a leaf, a unary Inverter, or an ordered binary
Selector, Sequence, RequireAll Parallel or RequireOne Parallel. Enumerating
exact sizes 1 through 7 with root depth 0 and maximum depth 3 produces:

| Leaves | Shapes | Independent script assignments per shape |
|---|---:|---:|
| 1 | 4 | 6 |
| 2 | 56 | 36 |
| 3 | 384 | 216 |
| 4 | 320 | 1,296 |
| Total | 764 | 499,704 cases across all shapes |

Each leaf independently selects one six-tick script: `SSSSSS`, `FFFFFF`,
`RRRRRR`, `RSFRSF`, `RFSRFS`, or `RRSRRF`. Scripts are indexed by global fixed
tick, including ticks on which a leaf is skipped. That makes incorrectly
revisiting a completed prefix or terminal parallel child observable. Every
case runs six ticks: **2,998,224 generated tick comparisons**. This is exhaustive
within the stated grammar and catalog, not all possible v1 trees or schedules.

Four additional witness tests cover 18 cases / 108 ticks: resuming a later
serial child and restarting the prefix on completion; parallel completion
clearing a nested serial prefix; Success and Failure latches surviving changed
leaf scripts across two parallel runs under both policies; and all
six permutations of Success/Failure/Running under both parallel policies.
The nested witness distinguishes a stale resume cursor after an abort without
relying on task reset callbacks. Existing `tree::tests` already prove the basic
short circuits, inverter truth table, parallel policies, finished-child latch,
and running-sibling reset. `edge_cases::empty_composites_follow_the_identity_rules`
covers empty composites. The new oracle extends those interactions across
nested trees and exact traces; it does not cover timers, observers, services,
state trees, queries or GOAP.

For a negative control, temporarily change the reference Inverter's Success
mapping from Failure to Success. The generated case `Invert(Leaf)`, script 0,
tick 0 must fail against the runtime. Restore the mapping and rerun the suite.
Never commit the mutation or change expected output to accommodate a mismatch.

To check sensitivity to an ordered-trace error even when the root result is
unchanged, temporarily change `if receipt.is_some()` in the reference tick to
`if !serial && receipt.is_some()`. Run
`cargo test -p regia_ai --test bt_v1_trace_oracle resumption_skips_the_prefix_and_completion_restarts_it`.
Tick 1 of the Sequence witness must fail: both roots succeed, but the altered
reference calls leaf 0 again before leaf 1. Restore the guard and rerun all five
tests. Both mutations affect only the test reference, never production code.

Verified on the build server with Rust 1.99 on 2026-11-08: all five tests passed, each
temporary mutation failed with the counterexample described above, and all
five tests passed again after restoring the reference. The restored file was
also compared byte for byte with its pre-mutation copy.
