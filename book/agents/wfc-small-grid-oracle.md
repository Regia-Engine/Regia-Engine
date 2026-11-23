# WFC tiny-grid satisfiability regression

Use this CPU regression recipe when changing tiled WFC adjacency, neighbor lookup,
forced cells or chronological backtracking. It tests the existing Rust solver in
`regia_procgen::wfc`. The existing agent surface is `procgen.wfc_solve`, documented
in the [procedural guide](procgen.md#wave-function-collapse) and its existing
`procgen.wfc_solve` row in [COVERAGE](../COVERAGE.md). This recipe adds no agent method.

Run from a normal repository checkout on an authorized build host:

```sh
cargo test -p regia_procgen --test wfc_small_grid_oracle -- --nocapture
cargo test -p regia_procgen
```

The oracle reads the authored `AdjacencyRule` lists directly. An unspecified
tile/direction row allows everything; repeated rows union their allowed ids;
an explicitly empty row allows nothing. Every directed edge must pass, so an
asymmetric pair of opposite rows constrains their shared edge by intersection.
The reference enumerates every assignment and checks coordinates, directed
neighbors and the last authored constraint for each cell. It calls neither
`WfcGrid::validate`, model adjacency queries, production direction arithmetic nor
production propagation. A width or height of one still has a self-neighbor on
that periodic axis. Fixed boundaries have no outside neighbor.

The main catalog is finite:

| Dimension | Cases |
|---|---|
| Two ids | All 16 horizontal × 16 vertical two-by-two bit matrices |
| Three ids | All 512 horizontal three-by-three matrices; vertical = nine-bit rotate-left by three, xor `0x12d` (a bijection), rather than all 512² pairs |
| Authored encoding | Selected by `(horizontal + 3 * vertical) % 4`: forward rows, reverse rows, disagreeing opposite rows, or matching opposite rows; includes omitted universal rows and split/duplicate/empty rows |
| Shape | `1×1`, `1×2`, `2×1`, `1×3`, `3×1`, `1×4`, `4×1`, `2×2`, `1×5`, `5×1`, `1×6`, `6×1`, `2×3`, `3×2` |
| Boundary | Fixed and periodic |
| Constraint history | None; origin forced to id 0; origin forced to 0 then last cell forced to 1 (replacement on `1×1`) |
| Seeds | `0`, `1`, `0x5eed`, `u64::MAX` |

That is 64,512 reference cases, 12,033,024 enumerated assignments and 258,048
complete-budget solves. Each case enumerates at most `3^6 = 729` assignments.
Hand-counted anchors and budget controls bring the whole test binary to 258,224
solver calls: 258,160 with a complete budget and 64 with zero backtracks.

For `c` cells and `t` ids, the complete budget is `2^(c*(t-1)+1)` (at most 8192).
Each binary decision either assigns one id or forbids it; either branch removes
at least one excess domain value. The maximum depth is `c*(t-1)`, and the full
binary tree has fewer nodes than that budget, independent of variable ordering.
Every success must be an exact member of the reference solution set. With that
budget, `Contradiction` must agree with an empty set, and `BacktrackLimit` fails
the test. A small-budget `BacktrackLimit` is inconclusive: separate three-cell
rings demonstrate it both with exactly one solution and with no solution.

Hand-counted anchors check unrestricted grids, constant grids, unique forced
solutions, contradictory different forced cells, odd/even periodic rings and
one-cell self-neighbors. Directed three-id cycles independently check horizontal
and vertical orientation, subset constraints with duplicate ids and replacement
of a previous constraint.

Existing focused coverage is retained: inline
`periodic_one_and_two_cell_axes_respect_self_adjacency` checks a specific weighted
periodic strip, `contradictory_forced_cells_return_contradiction` checks roads,
and `trap_ring_with_zero_backtrack_budget_hits_the_limit` uses four ids. The new
tests extend those interactions with exhaustive reference sets and two/three ids.
Cancellation, socket classes, graph scheduling and node overrides belong to their
existing suites; this catalog concerns explicit tiled rules.

To verify that the oracle detects a neighbor regression, temporarily replace
the periodic arm in `neighbor_index` with a fixed-boundary check, run
`reference_sets_match_hand_counted_boundary_and_forcing_cases`, and require a
failure. The `1×1` periodic alternating model must have zero solutions; the
mutation incorrectly returns a tile. Restore `wfc.rs` byte-for-byte, confirm the
source diff is empty, then rerun the unmodified test binary and full crate suite.
Never commit a negative-control mutation or alter an expected solution to make
production pass. Failures include the authored rules, dimensions, boundary,
constraint history, seed, budget and returned grid/reference count for a
reproducible counterexample.

Through the existing agent surface, `wfc_contradiction` means an exhausted search
and `wfc_backtrack_limit` means an inconclusive bounded search. These existing
lowercase error reasons are documented in the procedural guide. No new CLI,
format, dependency, Cargo feature or host API is introduced. Windows/Android
physical-device, listening, feel and performance checks are outside this CPU
contract recipe and must be reported as unverified when not run.
