# Wave Function Collapse (`regia_procgen::wfc`)

Wave Function Collapse (Gumin 2016) fills a grid one cell at a time. Each cell
starts out able to hold any tile (or pattern); repeatedly, the solver picks
the undecided cell of lowest Shannon entropy, collapses it to one tile chosen
in proportion to the tile weights, and propagates: any tile that no longer
has a compatible neighbor left in some adjacent cell is deleted there,
cascading as far as the constraints reach. The result is a grid in which
every adjacent pair satisfies the model's adjacency rules.

This implementation adds bounded chronological backtracking on top of the
classic loop: collapses are decisions on a stack, every propagation change is
recorded on an undo trail, and a contradiction unwinds to the newest decision
and forbids its choice there, instead of discarding the whole grid.

## The two models

**Tiled model.** Declare tiles (`TileDef`, with optional weights) and
adjacency rules (`AdjacencyRule`): for each tile and each of the six
`Direction`s (east/west/south/north/up/down), which tiles may sit on that
side. A side with no rule allows every tile. `TiledModel::with_adjacency`
builds the same thing from a predicate, which suits edge-socket tilesets.

```rust
use regia_procgen::wfc::{AdjacencyRule, Direction, TileDef, TiledModel};

let model = TiledModel::new(
    vec![TileDef::with_weight("grass", 6.0), TileDef::new("water")],
    vec![
        // Grass may touch anything; water may only touch water going east.
        AdjacencyRule::new(1, Direction::East, vec![1]),
    ],
)?;
```

**Overlapping model.** Give a `SampleGrid` of `u32` symbols and a pattern
size `N`. Every `N`×`N` (or `N`×`N`×`N` in 3D) block in the sample becomes a
pattern weighted by its frequency; patterns may sit next to each other only
when they agree on their overlap, so every `N`-block of the output occurs in
the sample. `OverlappingModel::from_sample` treats the sample as periodic
when extracting blocks; `from_sample_with` also supports non-wrapping
extraction. The output `WfcGrid::tiles()` holds pattern ids and
`WfcGrid::values()` holds the reconstructed symbols, directly comparable to
the sample.

## Solving

```rust
use regia_procgen::wfc::{
    solve, Boundary, CellConstraints, GridSize, WfcModel, WfcOptions,
};

let size = GridSize::new_2d(32, 32);
// Every border cell must be tile 0 ("wall", say).
let constraints = CellConstraints::with_border_2d(size, &[0]);
let opts = WfcOptions::default()
    .with_boundary(Boundary::Fixed)
    .with_max_backtracks(10_000);
let grid = solve(&WfcModel::Tiled(model), size, &constraints, 42, &opts)?;
```

`WfcSpec` bundles the same five arguments as one value, and each model type
(and `WfcModel`) has a matching `solve` method. Two or three dimensions are
just the `depth` of `GridSize` (`1` = 2D); an overlapping model built from a
2D sample refuses a 3D size and vice versa.

- **Boundary.** `Boundary::Fixed` leaves edge cells unconstrained on their
  outer side; `Boundary::Periodic` wraps every axis, producing tileable
  output whose wrapping edges are valid too.
- **Weights.** Tile weights and pattern frequencies bias both the entropy
  ordering and the weighted choice at each collapse.
- **Constraints.** `CellConstraints::force*` pins individual cells to a
  subset of tiles before solving (and propagation does the rest).
- **Determinism.** All randomness comes from `regia_core::math::Rng` seeded
  with the caller's `seed`; the same input always produces the same grid.

Agents run the same solver through `procgen.wfc_solve` (inline tileset or sample, at most
65 536 cells); see `book/agents/procgen.md` § Wave Function Collapse.

## Failures

`solve` never panics on bad input; it returns `WfcError`:

| Variant | Meaning |
|---|---|
| `InvalidModel` | Malformed input: empty tileset, NaN/non-positive weight, zero or oversized grid, out-of-range rule or constraint, `N` larger than the sample, dimension mismatch |
| `Contradiction` | The model plus constraints admit no valid grid (the search was exhausted) |
| `BacktrackLimit` | A contradiction was reached after the `max_backtracks` budget ran out; raise the budget or relax the model |

Use `max_backtracks` to keep generation inside a frame budget: a small budget
fails fast on hostile tilesets at the price of occasionally rejecting a
solvable request.

## Provenance

The algorithm is derived from Maxim Gumin, "Wave Function Collapse
Algorithm" (2016), with support-count arc consistency (AC-4, Mohr &
Henderson 1986) as the propagation mechanism; see the entry at the end of `docs/provenance.md`. The
code in `crates/regia_procgen/src/wfc.rs` is original; do not add derivative
code from any other WFC implementation (cleanroom rules, `docs/01-cleanroom.md`).
