// SPDX-License-Identifier: MPL-2.0
//! Wave Function Collapse (WFC) procedural generation on 2D and 3D grids.
//!
//! Two models are provided, after Maxim Gumin's original formulation (2016):
//!
//! - [`TiledModel`]: the caller declares tiles and, for each tile and each
//!   [`Direction`], which tiles may sit next to it in that direction.
//! - [`OverlappingModel`]: the caller gives a sample grid; every `N`×`N`
//!   (or `N`×`N`×`N`) block occurring in the sample becomes a pattern, weighted
//!   by its frequency, and two patterns may be adjacent exactly when they agree
//!   on their overlap. Every `N`-block of the output therefore occurs in the
//!   sample.
//!
//! Solving picks the undecided cell of lowest Shannon entropy (with a small
//! seeded tie-break noise), collapses it to one pattern chosen in proportion
//! to the pattern weights, and propagates the consequences with an AC-4 style
//! arc-consistency pass driven by per-cell *support counts* (for each cell,
//! direction and pattern, how many patterns still possible in the neighboring
//! cell support it). A pattern whose support in some direction reaches zero is
//! removed from its cell, which may cascade.
//!
//! Contradictions are handled by bounded chronological backtracking, not by
//! restarting from scratch: every collapse is a decision on a stack, every
//! domain/support change is recorded on a trail, and on a contradiction the
//! solver undoes the trail back to the most recent decision and forbids the
//! pattern it had chosen there. [`WfcOptions::max_backtracks`] bounds how many
//! such backtracks a solve may perform.
//!
//! Everything is deterministic: the only randomness comes from
//! [`regia_core::math::Rng`] seeded from the caller's `seed`, iteration is
//! always in index order (no hash-map order dependence), and there is no wall
//! clock input. Invalid input (empty tilesets, NaN or non-positive weights,
//! zero-sized grids, out-of-range constraints, …) returns
//! [`WfcError::InvalidModel`] instead of panicking.
//!
//! Coordinates: `x` grows east, `y` grows south (row index), `z` grows up.
//! Cells are stored x-fastest: `index = x + width * (y + height * z)`.

use std::collections::BTreeMap;

use regia_core::math::Rng;

use crate::detmath;

/// A tile (tiled model) or pattern (overlapping model) identifier: an index
/// into the model's tile/pattern list, in `0..tile_count`.
pub type TileId = usize;

/// Largest number of output cells a single solve may request. Bounds memory
/// use on degenerate input; larger requests return
/// [`WfcError::InvalidModel`].
pub const MAX_CELLS: usize = 4_194_304;

/// Largest `cells * patterns * 6` support table a single solve may allocate
/// (4 bytes per entry, so 1 GiB). [`MAX_CELLS`] alone does not bound memory:
/// a model with many patterns multiplies it. Larger requests return
/// [`WfcError::InvalidModel`] instead of aborting on allocation failure.
pub const MAX_SUPPORT_ENTRIES: usize = 1 << 28;

/// The six axis directions. `East` is `+x`, `South` is `+y`, `Up` is `+z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// `+x`.
    East,
    /// `-x`.
    West,
    /// `+y` (the next row).
    South,
    /// `-y` (the previous row).
    North,
    /// `+z` (the next layer).
    Up,
    /// `-z` (the previous layer).
    Down,
}

impl Direction {
    /// The four directions of a 2D grid, in index order.
    pub const ALL_2D: [Direction; 4] = [
        Direction::East,
        Direction::West,
        Direction::South,
        Direction::North,
    ];

    /// All six directions of a 3D grid, in index order.
    pub const ALL_3D: [Direction; 6] = [
        Direction::East,
        Direction::West,
        Direction::South,
        Direction::North,
        Direction::Up,
        Direction::Down,
    ];

    /// A dense index in `0..6`, stable across runs.
    pub const fn index(self) -> usize {
        match self {
            Direction::East => 0,
            Direction::West => 1,
            Direction::South => 2,
            Direction::North => 3,
            Direction::Up => 4,
            Direction::Down => 5,
        }
    }

    /// The direction from `0..6`, or `None` outside that range.
    pub const fn from_index(index: usize) -> Option<Direction> {
        match index {
            0 => Some(Direction::East),
            1 => Some(Direction::West),
            2 => Some(Direction::South),
            3 => Some(Direction::North),
            4 => Some(Direction::Up),
            5 => Some(Direction::Down),
            _ => None,
        }
    }

    /// The opposite direction.
    pub const fn opposite(self) -> Direction {
        match self {
            Direction::East => Direction::West,
            Direction::West => Direction::East,
            Direction::South => Direction::North,
            Direction::North => Direction::South,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    /// The `(dx, dy, dz)` step to the neighboring cell in this direction.
    pub const fn delta(self) -> [i32; 3] {
        match self {
            Direction::East => [1, 0, 0],
            Direction::West => [-1, 0, 0],
            Direction::South => [0, 1, 0],
            Direction::North => [0, -1, 0],
            Direction::Up => [0, 0, 1],
            Direction::Down => [0, 0, -1],
        }
    }
}

/// How cells on the grid edge treat their missing neighbors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Boundary {
    /// Edge cells have no neighbor outside the grid and are unconstrained on
    /// that side.
    #[default]
    Fixed,
    /// The grid wraps around on every axis (a torus in 2D): the east neighbor
    /// of the rightmost cell is the leftmost cell of the same row, and so on.
    Periodic,
}

/// The dimensions of an output grid. A `depth` of 1 is a 2D grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GridSize {
    /// Number of cells along `x`.
    pub width: usize,
    /// Number of cells along `y`.
    pub height: usize,
    /// Number of cells along `z`; 1 for a 2D grid.
    pub depth: usize,
}

impl GridSize {
    /// A 2D size (`depth == 1`).
    pub const fn new_2d(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            depth: 1,
        }
    }

    /// A 3D size.
    pub const fn new_3d(width: usize, height: usize, depth: usize) -> Self {
        Self { width, height, depth }
    }

    /// Total number of cells, or `None` on overflow.
    pub fn cell_count(self) -> Option<usize> {
        self.width
            .checked_mul(self.height)
            .and_then(|n| n.checked_mul(self.depth))
    }

    /// `2` when `depth <= 1`, otherwise `3`.
    pub const fn dimensions(self) -> usize {
        if self.depth <= 1 { 2 } else { 3 }
    }

    /// The linear (x-fastest) index of `(x, y, z)`, or `None` if outside.
    pub fn index(self, x: usize, y: usize, z: usize) -> Option<usize> {
        if x < self.width && y < self.height && z < self.depth {
            Some(x + self.width * (y + self.height * z))
        } else {
            None
        }
    }

    /// The `(x, y, z)` coordinates of a linear index, or `None` if outside.
    pub fn coords(self, index: usize) -> Option<[usize; 3]> {
        let count = self.cell_count()?;
        if index >= count || self.width == 0 || self.height == 0 {
            return None;
        }
        let x = index % self.width;
        let y = (index / self.width) % self.height;
        let z = index / (self.width * self.height);
        Some([x, y, z])
    }
}

impl From<[usize; 2]> for GridSize {
    fn from([width, height]: [usize; 2]) -> Self {
        Self::new_2d(width, height)
    }
}

impl From<[usize; 3]> for GridSize {
    fn from([width, height, depth]: [usize; 3]) -> Self {
        Self::new_3d(width, height, depth)
    }
}

impl From<(usize, usize)> for GridSize {
    fn from((width, height): (usize, usize)) -> Self {
        Self::new_2d(width, height)
    }
}

/// One tile of a [`TiledModel`]: a name (for debugging and content tools) and
/// a relative weight. When a cell collapses, each still-possible tile is
/// chosen with probability proportional to its weight. Weights must be finite
/// and strictly positive.
#[derive(Clone, Debug, PartialEq)]
pub struct TileDef {
    /// Human-readable tile name, e.g. `"grass"` or `"road_east_west"`.
    pub name: String,
    /// Relative choice weight; must be finite and `> 0`.
    pub weight: f64,
}

impl TileDef {
    /// A tile with weight 1.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            weight: 1.0,
        }
    }

    /// A tile with an explicit relative weight.
    pub fn with_weight(name: impl Into<String>, weight: f64) -> Self {
        Self {
            name: name.into(),
            weight,
        }
    }
}

/// One adjacency rule of a [`TiledModel`]: the tiles that may sit in direction
/// `direction` from tile `tile`.
///
/// A `(tile, direction)` pair with no rule at all allows every tile on that
/// side. The first rule for a pair replaces that default with its `allowed`
/// set; further rules for the same pair add to the set. An empty `allowed`
/// set therefore means "no tile may sit on this side" (the tile can then only
/// appear against a fixed boundary on that side).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdjacencyRule {
    /// The tile whose side this rule constrains.
    pub tile: TileId,
    /// The side of `tile` this rule constrains.
    pub direction: Direction,
    /// Tiles allowed on that side.
    pub allowed: Vec<TileId>,
}

impl AdjacencyRule {
    /// A rule allowing exactly `allowed` next to `tile` in `direction`.
    pub fn new(tile: TileId, direction: Direction, allowed: Vec<TileId>) -> Self {
        Self {
            tile,
            direction,
            allowed,
        }
    }

    /// A rule allowing a single tile `other` next to `tile` in `direction`.
    pub fn allow_one(tile: TileId, direction: Direction, other: TileId) -> Self {
        Self::new(tile, direction, vec![other])
    }
}

/// Why a WFC operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WfcError {
    /// The model, size, sample or constraints are malformed (empty tileset,
    /// non-finite or non-positive weight, zero grid dimension, out-of-range
    /// tile id or constraint coordinate, pattern size larger than the sample,
    /// dimension mismatch between an overlapping model and the output size…).
    /// The string names the specific problem.
    #[error("invalid WFC model: {0}")]
    InvalidModel(String),
    /// The constraints are unsatisfiable: the search space was exhausted
    /// without finding a complete, adjacency-valid grid.
    #[error("WFC contradiction: no valid grid satisfies the model and constraints")]
    Contradiction,
    /// A contradiction was reached but the configured backtrack budget
    /// ([`WfcOptions::max_backtracks`]) was already spent, so the solver gave
    /// up before proving satisfiability or unsatisfiability.
    #[error("WFC backtrack limit reached")]
    BacktrackLimit,
    /// The check passed to [`solve_cancellable`] asked the solver to stop.
    #[error("WFC solve cancelled")]
    Cancelled,
}

fn invalid(message: impl Into<String>) -> WfcError {
    WfcError::InvalidModel(message.into())
}

/// The compiled form shared by both models: pattern weights and, for each
/// direction, the patterns allowed next to each pattern in that direction,
/// plus the forward and reverse adjacency lists propagation walks.
#[derive(Clone, Debug)]
struct CompiledModel {
    /// Number of tiles/patterns.
    count: usize,
    /// Per-pattern weight, finite and `> 0`.
    weights: Vec<f64>,
    /// `weights[i] * log2(weights[i])`, precomputed for entropy evaluation.
    weight_log_weight: Vec<f64>,
    /// `allowed[((dir * count) + a) * count + b]`: may `b` sit in direction
    /// `dir` from `a`?
    allowed: Vec<bool>,
    /// `allowed_list[dir * count + a]`: patterns allowed in direction `dir`
    /// from `a`, ascending.
    allowed_list: Vec<Vec<usize>>,
    /// `reverse_list[dir * count + b]`: patterns `a`, ascending, such that `b`
    /// is allowed in direction `dir` from `a`. When `b` is removed from a
    /// cell, these are the patterns in the opposite neighbor whose support
    /// count for direction `dir` drops by one.
    reverse_list: Vec<Vec<usize>>,
}

impl CompiledModel {
    fn compile(weights: Vec<f64>, allowed: Vec<bool>) -> Result<Self, WfcError> {
        let count = weights.len();
        if count == 0 {
            return Err(invalid("the model has no tiles or patterns"));
        }
        for (i, w) in weights.iter().enumerate() {
            if !w.is_finite() || *w <= 0.0 {
                return Err(invalid(format!(
                    "tile/pattern {i} has weight {w}; weights must be finite and > 0"
                )));
            }
        }
        // Individually finite weights can still overflow when summed; such a
        // model would poison the entropy arithmetic, so reject it too.
        let total_weight: f64 = weights.iter().sum();
        if !total_weight.is_finite() {
            return Err(invalid("the sum of the weights overflows f64"));
        }
        let expected = 6usize.checked_mul(count).and_then(|n| n.checked_mul(count));
        if expected != Some(allowed.len()) {
            return Err(invalid("internal adjacency table size mismatch"));
        }
        let weight_log_weight: Vec<f64> = weights.iter().map(|w| w * detmath::log2(*w)).collect();
        let mut allowed_list: Vec<Vec<usize>> = vec![Vec::new(); 6 * count];
        let mut reverse_list: Vec<Vec<usize>> = vec![Vec::new(); 6 * count];
        for dir in 0..6 {
            for a in 0..count {
                for b in 0..count {
                    if allowed[((dir * count) + a) * count + b] {
                        allowed_list[dir * count + a].push(b);
                        reverse_list[dir * count + b].push(a);
                    }
                }
            }
        }
        Ok(Self {
            count,
            weights,
            weight_log_weight,
            allowed,
            allowed_list,
            reverse_list,
        })
    }

    fn allows(&self, a: TileId, direction: Direction, b: TileId) -> bool {
        if a >= self.count || b >= self.count {
            return false;
        }
        self.allowed[((direction.index() * self.count) + a) * self.count + b]
    }
}

/// The tiled WFC model: an explicit tile list plus per-direction adjacency
/// rules (see [`AdjacencyRule`]). Usable on both 2D and 3D output grids; the
/// `Up`/`Down` rules only take effect on 3D grids.
#[derive(Clone, Debug)]
pub struct TiledModel {
    tiles: Vec<TileDef>,
    compiled: CompiledModel,
    /// Per tile id: which declared tile it came from and how it was turned
    /// (identity for every tile of [`TiledModel::new`] and
    /// [`TiledModel::with_adjacency`]).
    variants: Vec<TileVariant>,
}

/// The symmetry group a WFC model expands its tiles or patterns with
/// (rotations are quarter turns about the vertical axis, clockwise seen from
/// above with `y` growing south; the reflection mirrors `x`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Symmetry {
    /// Only the tiles or patterns as given.
    #[default]
    None,
    /// The four quarter turns.
    Rotations,
    /// The identity and the `x` mirror.
    Reflections,
    /// All eight: the four quarter turns of the tile and of its mirror.
    All,
}

impl Symmetry {
    /// The `(quarter_turns, mirrored)` transforms of the group, in the fixed
    /// order variants are generated in (identity first).
    pub fn transforms(self) -> &'static [(u8, bool)] {
        match self {
            Self::None => &[(0, false)],
            Self::Rotations => &[(0, false), (1, false), (2, false), (3, false)],
            Self::Reflections => &[(0, false), (0, true)],
            Self::All => &[
                (0, false),
                (1, false),
                (2, false),
                (3, false),
                (0, true),
                (1, true),
                (2, true),
                (3, true),
            ],
        }
    }

    /// The settings name (`none`, `rotate`, `reflect`, `all`).
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Rotations => "rotate",
            Self::Reflections => "reflect",
            Self::All => "all",
        }
    }

    /// Parses [`Symmetry::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "none" => Self::None,
            "rotate" => Self::Rotations,
            "reflect" => Self::Reflections,
            "all" => Self::All,
            _ => return None,
        })
    }
}

/// Where a tile of a [`TiledModel`] came from: the declared tile and the
/// transform applied to it (mirror first, then `rotation` quarter turns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TileVariant {
    /// Index of the declared tile.
    pub base: usize,
    /// Clockwise quarter turns (seen from above), `0..4`.
    pub rotation: u8,
    /// Mirrored along `x` before the rotation.
    pub mirrored: bool,
}

/// A tile described by its four edge sockets, for [`TiledModel::from_sockets`].
///
/// Sockets are read clockwise around the tile (seen from above, `y` growing
/// south), in the order east, south, west, north. Two tiles may touch when
/// the socket of one equals the *reverse* of the facing socket of the other
/// (the shared edge is read in opposite directions), so a symmetric socket
/// such as `"a"` or `"aba"` matches itself and `"ab"` matches `"ba"`.
#[derive(Clone, Debug, PartialEq)]
pub struct SocketTile {
    /// Tile name; variants are named `name@r1`, `name@m`, `name@mr2`, ….
    pub name: String,
    /// Relative weight (every variant keeps it); finite and `> 0`.
    pub weight: f64,
    /// Sockets: east, south, west, north.
    pub sockets: [String; 4],
}

/// The symmetry class of a tile picture (ADR-3027), named after the letter
/// whose shape has the same symmetry. Orientation conventions, seen from
/// above with `x` east and `y` south:
///
/// - `X`: every quarter turn and mirror gives the same picture (1 variant).
/// - `I`: a bar running north-south; half turns and both mirrors keep it (2).
/// - `Slash` (`\`): a diagonal from the north-west to the south-east corner;
///   half turns and the two diagonal mirrors keep it (2).
/// - `T`: symmetric about the north-south axis only, e.g. a junction joining
///   east, west and south (4).
/// - `L`: a corner joining north and east, symmetric about that corner's
///   diagonal (4).
/// - `F`: no symmetry (8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileClass {
    /// Fully symmetric.
    X,
    /// A north-south bar.
    I,
    /// A north-west to south-east diagonal (`\`).
    Slash,
    /// Symmetric about the north-south axis.
    T,
    /// A north-east corner.
    L,
    /// No symmetry.
    F,
}

impl TileClass {
    /// The transforms `(quarter_turns, mirrored)` that leave the picture
    /// unchanged (identity first).
    pub fn stabilizer(self) -> &'static [(u8, bool)] {
        match self {
            Self::X => Symmetry::All.transforms(),
            Self::I => &[(0, false), (2, false), (0, true), (2, true)],
            Self::Slash => &[(0, false), (2, false), (1, true), (3, true)],
            Self::T => &[(0, false), (0, true)],
            Self::L => &[(0, false), (1, true)],
            Self::F => &[(0, false)],
        }
    }

    /// The settings name: `X`, `I`, `\`, `T`, `L` or `F`.
    pub fn name(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::I => "I",
            Self::Slash => "\\",
            Self::T => "T",
            Self::L => "L",
            Self::F => "F",
        }
    }

    /// The class for a settings name (`\` and `slash` both name `Slash`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "X" => Self::X,
            "I" => Self::I,
            "\\" | "slash" => Self::Slash,
            "T" => Self::T,
            "L" => Self::L,
            "F" => Self::F,
            _ => return None,
        })
    }

    /// Variants the class gives under the full group ([`Symmetry::All`]): 8
    /// divided by the stabilizer's size.
    pub fn cardinality(self) -> usize {
        8 / self.stabilizer().len()
    }

    /// Do transforms `g` and `h` give the same picture of a tile of this
    /// class? (`h = g · s` for some `s` of the stabilizer.)
    fn same_picture(self, g: (u8, bool), h: (u8, bool)) -> bool {
        let mg = d4_matrix(g.0, g.1);
        let mh = d4_matrix(h.0, h.1);
        self.stabilizer()
            .iter()
            .any(|&(r, m)| mat_mul(mg, d4_matrix(r, m)) == mh)
    }
}

/// A transform `(quarter_turns, mirrored)` as a row-major 2x2 integer matrix
/// acting on `(x, y)` (`x` east, `y` south): the `x` mirror first, then the
/// clockwise quarter turns (east goes to south).
fn d4_matrix(rotation: u8, mirrored: bool) -> [i8; 4] {
    let mut m: [i8; 4] = if mirrored { [-1, 0, 0, 1] } else { [1, 0, 0, 1] };
    for _ in 0..rotation % 4 {
        // The quarter turn [0 -1; 1 0] times m.
        m = [-m[2], -m[3], m[0], m[1]];
    }
    m
}

/// `a · b` for row-major 2x2 matrices.
fn mat_mul(a: [i8; 4], b: [i8; 4]) -> [i8; 4] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
    ]
}

/// The key a vertical socket compares by: its text, plus the transform when
/// it starts with `^` (turn-sensitive).
fn vertical_key(socket: &str, rotation: u8, mirrored: bool) -> String {
    if socket.starts_with('^') {
        format!("{socket}#{rotation}{}", if mirrored { "m" } else { "" })
    } else {
        socket.to_string()
    }
}

/// A tile for [`TiledModel::from_specs`]: a [`SocketTile`] plus an optional
/// symmetry class and optional vertical sockets (ADR-3027).
#[derive(Clone, Debug, PartialEq)]
pub struct TileSpec {
    /// Tile name; variants are named as in [`SocketTile::name`].
    pub name: String,
    /// Relative weight (every variant keeps it); finite and `> 0`.
    pub weight: f64,
    /// Sockets: east, south, west, north (as [`SocketTile::sockets`]).
    pub sockets: [String; 4],
    /// The picture's symmetry class; `None` keeps one variant per distinct
    /// set of turned sockets.
    pub class: Option<TileClass>,
    /// Vertical sockets `[up, down]` for 3D grids; `None` leaves `Up`/`Down`
    /// unconstrained for this tile.
    pub vertical: Option<[String; 2]>,
}

impl From<SocketTile> for TileSpec {
    fn from(t: SocketTile) -> Self {
        Self {
            name: t.name,
            weight: t.weight,
            sockets: t.sockets,
            class: None,
            vertical: None,
        }
    }
}

/// The sockets of `tile` after mirroring (when `mirrored`) and `rotation`
/// clockwise quarter turns.
fn turn_sockets(sockets: &[String; 4], rotation: u8, mirrored: bool) -> [String; 4] {
    let rev = |t: &str| t.chars().rev().collect::<String>();
    let [e, sth, w, n] = sockets;
    let mut out = if mirrored {
        [rev(w), rev(sth), rev(e), rev(n)]
    } else {
        sockets.clone()
    };
    for _ in 0..rotation % 4 {
        // A clockwise quarter turn: the north side now faces east.
        let [e, sth, w, n] = out;
        out = [n, e, sth, w];
    }
    out
}

/// One identity [`TileVariant`] per tile.
fn identity_variants(count: usize) -> Vec<TileVariant> {
    (0..count)
        .map(|base| TileVariant {
            base,
            ..TileVariant::default()
        })
        .collect()
}

/// `block` (x-fastest, `n` per side, `layers` layers) mirrored along `x`
/// (when `mirrored`) and then turned `rotation` clockwise quarter turns about
/// the vertical axis, layer by layer.
fn turn_block(block: &[u32], n: usize, layers: usize, rotation: u8, mirrored: bool) -> Vec<u32> {
    let mut cur = block.to_vec();
    let at = |v: &[u32], x: usize, y: usize, z: usize| v.get(x + n * (y + n * z)).copied().unwrap_or(0);
    if mirrored {
        let mut out = vec![0; cur.len()];
        for z in 0..layers {
            for y in 0..n {
                for x in 0..n {
                    out[x + n * (y + n * z)] = at(&cur, n - 1 - x, y, z);
                }
            }
        }
        cur = out;
    }
    for _ in 0..rotation % 4 {
        // Clockwise with `y` south: the cell at `(x, y)` comes from `(y, n - 1 - x)`.
        let mut out = vec![0; cur.len()];
        for z in 0..layers {
            for y in 0..n {
                for x in 0..n {
                    out[x + n * (y + n * z)] = at(&cur, y, n - 1 - x, z);
                }
            }
        }
        cur = out;
    }
    cur
}

impl TiledModel {
    /// Builds a model from tiles and adjacency rules.
    ///
    /// Returns [`WfcError::InvalidModel`] when the tile list is empty, any
    /// weight is NaN, infinite or `<= 0`, or any rule names a tile id outside
    /// the tile list.
    pub fn new(tiles: Vec<TileDef>, rules: Vec<AdjacencyRule>) -> Result<Self, WfcError> {
        let count = tiles.len();
        if count == 0 {
            return Err(invalid("a tiled model needs at least one tile"));
        }
        let Some(table_len) = 6usize.checked_mul(count).and_then(|n| n.checked_mul(count)) else {
            return Err(invalid("too many tiles"));
        };
        // Default: everything may sit next to everything. The first rule for
        // a (tile, direction) pair clears that row; later rules add to it.
        let mut allowed = vec![true; table_len];
        let mut specified = vec![[false; 6]; count];
        for rule in &rules {
            if rule.tile >= count {
                return Err(invalid(format!(
                    "adjacency rule names tile {} but there are only {count} tiles",
                    rule.tile
                )));
            }
            for other in &rule.allowed {
                if *other >= count {
                    return Err(invalid(format!(
                        "adjacency rule for tile {} allows tile {other} but there are only {count} tiles",
                        rule.tile
                    )));
                }
            }
            let dir = rule.direction.index();
            let row = ((dir * count) + rule.tile) * count;
            if !specified[rule.tile][dir] {
                for cell in allowed.iter_mut().skip(row).take(count) {
                    *cell = false;
                }
                specified[rule.tile][dir] = true;
            }
            for other in &rule.allowed {
                allowed[row + *other] = true;
            }
        }
        let weights: Vec<f64> = tiles.iter().map(|t| t.weight).collect();
        let compiled = CompiledModel::compile(weights, allowed)?;
        let variants = identity_variants(tiles.len());
        Ok(Self {
            tiles,
            compiled,
            variants,
        })
    }

    /// Builds a model from tiles and an adjacency predicate:
    /// `adjacent(a, direction, b)` answers whether tile `b` may sit in
    /// `direction` from tile `a`. Evaluated once per `(a, direction, b)`
    /// triple at build time, in ascending order.
    pub fn with_adjacency(
        tiles: Vec<TileDef>,
        adjacent: impl Fn(TileId, Direction, TileId) -> bool,
    ) -> Result<Self, WfcError> {
        let count = tiles.len();
        if count == 0 {
            return Err(invalid("a tiled model needs at least one tile"));
        }
        let Some(table_len) = 6usize.checked_mul(count).and_then(|n| n.checked_mul(count)) else {
            return Err(invalid("too many tiles"));
        };
        let mut allowed = vec![false; table_len];
        for dir in Direction::ALL_3D {
            for a in 0..count {
                for b in 0..count {
                    if adjacent(a, dir, b) {
                        allowed[((dir.index() * count) + a) * count + b] = true;
                    }
                }
            }
        }
        let weights: Vec<f64> = tiles.iter().map(|t| t.weight).collect();
        let compiled = CompiledModel::compile(weights, allowed)?;
        let variants = identity_variants(tiles.len());
        Ok(Self {
            tiles,
            compiled,
            variants,
        })
    }

    /// Builds a model from socket tiles expanded by `symmetry`: each declared
    /// tile contributes its distinct transformed variants (two transforms
    /// giving the same four sockets keep only the first, in
    /// [`Symmetry::transforms`] order), in declaration order. `b` may sit east
    /// of `a` when `a`'s east socket equals the reverse of `b`'s west socket,
    /// and likewise south/north; `Up`/`Down` are unconstrained.
    ///
    /// The same as [`TiledModel::from_specs`] with no class and no vertical
    /// sockets on any tile.
    ///
    /// Returns [`WfcError::InvalidModel`] for an empty list or a bad weight.
    pub fn from_sockets(tiles: &[SocketTile], symmetry: Symmetry) -> Result<Self, WfcError> {
        let specs: Vec<TileSpec> = tiles.iter().cloned().map(TileSpec::from).collect();
        Self::from_specs(&specs, symmetry)
    }

    /// Builds a model from tile specs expanded by `symmetry` (ADR-3027).
    ///
    /// A tile without a [`TileClass`] contributes its distinct transformed
    /// variants, as [`TiledModel::from_sockets`] does. A tile with a class
    /// contributes one variant per transform of the group that gives a
    /// different picture of the tile: transforms in the same coset of the
    /// class's stabilizer ([`TileClass::stabilizer`]) count once, the first in
    /// [`Symmetry::transforms`] order. With [`Symmetry::All`] that is 1 variant
    /// for `X`, 2 for `I` and `\`, 4 for `T` and `L` and 8 for `F`.
    ///
    /// Horizontal adjacency comes from the turned sockets (as
    /// [`TiledModel::from_sockets`]). `b` may sit on top of `a` when both have
    /// vertical sockets and `a`'s up key equals `b`'s down key; when either
    /// tile has none, `Up`/`Down` are unconstrained. A vertical socket is
    /// turn-invariant (its key is its text) unless it starts with `^`: then its
    /// key also holds the variant's `(quarter turns, mirrored)`, so it only
    /// meets a socket of the same text on a variant turned the same way.
    ///
    /// Returns [`WfcError::InvalidModel`] for an empty list, a bad weight, or a
    /// classed tile whose sockets are not invariant under its class's
    /// stabilizer (the class and the sockets disagree, so adjacency would
    /// depend on which variant is listed).
    pub fn from_specs(tiles: &[TileSpec], symmetry: Symmetry) -> Result<Self, WfcError> {
        let mut defs: Vec<TileDef> = Vec::new();
        let mut variants: Vec<TileVariant> = Vec::new();
        let mut sockets: Vec<[String; 4]> = Vec::new();
        let mut vertical: Vec<Option<[String; 2]>> = Vec::new();
        for (base, t) in tiles.iter().enumerate() {
            if let Some(class) = t.class {
                for &(r, m) in class.stabilizer() {
                    if turn_sockets(&t.sockets, r, m) != t.sockets {
                        return Err(invalid(format!(
                            "tile `{}` is declared `{}` but its sockets change under that class's symmetry \
                             (quarter turns {r}, mirrored {m})",
                            t.name,
                            class.name()
                        )));
                    }
                }
            }
            let mut own: Vec<([String; 4], Option<[String; 2]>)> = Vec::new();
            let mut chosen: Vec<(u8, bool)> = Vec::new();
            for &(rotation, mirrored) in symmetry.transforms() {
                let turned = turn_sockets(&t.sockets, rotation, mirrored);
                let keys = t.vertical.as_ref().map(|[up, down]| {
                    [
                        vertical_key(up, rotation, mirrored),
                        vertical_key(down, rotation, mirrored),
                    ]
                });
                match t.class {
                    Some(class) => {
                        if chosen
                            .iter()
                            .any(|&(hr, hm)| class.same_picture((hr, hm), (rotation, mirrored)))
                        {
                            continue;
                        }
                        chosen.push((rotation, mirrored));
                    }
                    None => {
                        let key = (turned.clone(), keys.clone());
                        if own.contains(&key) {
                            continue;
                        }
                        own.push(key);
                    }
                }
                let name = match (rotation, mirrored) {
                    (0, false) => t.name.clone(),
                    (r, false) => format!("{}@r{r}", t.name),
                    (0, true) => format!("{}@m", t.name),
                    (r, true) => format!("{}@mr{r}", t.name),
                };
                defs.push(TileDef::with_weight(name, t.weight));
                variants.push(TileVariant {
                    base,
                    rotation,
                    mirrored,
                });
                sockets.push(turned);
                vertical.push(keys);
            }
        }
        let rev = |t: &str| t.chars().rev().collect::<String>();
        let mut model = Self::with_adjacency(defs, |a, dir, b| {
            let (sa, sb) = (&sockets[a], &sockets[b]);
            match dir {
                Direction::East => sa[0] == rev(&sb[2]),
                Direction::South => sa[1] == rev(&sb[3]),
                Direction::West => sa[2] == rev(&sb[0]),
                Direction::North => sa[3] == rev(&sb[1]),
                Direction::Up => match (&vertical[a], &vertical[b]) {
                    (Some(va), Some(vb)) => va[0] == vb[1],
                    _ => true,
                },
                Direction::Down => match (&vertical[a], &vertical[b]) {
                    (Some(va), Some(vb)) => va[1] == vb[0],
                    _ => true,
                },
            }
        })?;
        model.variants = variants;
        Ok(model)
    }

    /// The tile definitions, in [`TileId`] order.
    pub fn tiles(&self) -> &[TileDef] {
        &self.tiles
    }

    /// The declared tile and transform behind tile `id` (identity unless the
    /// model came from [`TiledModel::from_sockets`]).
    pub fn variant(&self, id: TileId) -> Option<TileVariant> {
        self.variants.get(id).copied()
    }

    /// Number of tiles.
    pub fn tile_count(&self) -> usize {
        self.compiled.count
    }

    /// Per-tile weights, in [`TileId`] order.
    pub fn weights(&self) -> &[f64] {
        &self.compiled.weights
    }

    /// May tile `b` sit in `direction` from tile `a`?
    pub fn allows(&self, a: TileId, direction: Direction, b: TileId) -> bool {
        self.compiled.allows(a, direction, b)
    }

    /// The name of tile `id`, or `None` when out of range.
    pub fn tile_name(&self, id: TileId) -> Option<&str> {
        self.tiles.get(id).map(|t| t.name.as_str())
    }

    /// The id of the first tile named `name`, or `None`.
    pub fn tile_id(&self, name: &str) -> Option<TileId> {
        self.tiles.iter().position(|t| t.name == name)
    }

    /// Solves this model on a grid of `size`; shorthand for the free
    /// [`solve`] with `WfcModel::Tiled`.
    pub fn solve(
        &self,
        size: GridSize,
        constraints: &CellConstraints,
        seed: u64,
        opts: &WfcOptions,
    ) -> Result<WfcGrid, WfcError> {
        solve_inner(&self.compiled, None, None, size, constraints, seed, opts)
    }
}

/// A rectangular grid of symbol values used as the input of an
/// [`OverlappingModel`]. Symbols are opaque `u32`s (palette indices, tile ids
/// of another system, packed colors, …); only equality matters. Stored
/// x-fastest, like output grids. A `depth` of 1 is a 2D sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleGrid {
    size: GridSize,
    cells: Vec<u32>,
}

impl SampleGrid {
    /// A 2D sample from x-fastest cells. `cells.len()` must equal
    /// `width * height` and both dimensions must be non-zero.
    pub fn new_2d(width: usize, height: usize, cells: Vec<u32>) -> Result<Self, WfcError> {
        Self::new(GridSize::new_2d(width, height), cells)
    }

    /// A 3D sample from x-fastest cells. `cells.len()` must equal
    /// `width * height * depth` and every dimension must be non-zero.
    pub fn new_3d(width: usize, height: usize, depth: usize, cells: Vec<u32>) -> Result<Self, WfcError> {
        Self::new(GridSize::new_3d(width, height, depth), cells)
    }

    /// A sample of an explicit size from x-fastest cells.
    pub fn new(size: GridSize, cells: Vec<u32>) -> Result<Self, WfcError> {
        if size.width == 0 || size.height == 0 || size.depth == 0 {
            return Err(invalid("sample grid dimensions must all be non-zero"));
        }
        let Some(count) = size.cell_count() else {
            return Err(invalid("sample grid dimensions overflow"));
        };
        if cells.len() != count {
            return Err(invalid(format!(
                "sample grid holds {} cells but its size needs {count}",
                cells.len()
            )));
        }
        Ok(Self { size, cells })
    }

    /// The sample dimensions.
    pub fn size(&self) -> GridSize {
        self.size
    }

    /// All sample cells, x-fastest.
    pub fn cells(&self) -> &[u32] {
        &self.cells
    }

    /// The symbol at `(x, y, z)`, or `None` outside the sample.
    pub fn get(&self, x: usize, y: usize, z: usize) -> Option<u32> {
        let index = self.size.index(x, y, z)?;
        self.cells.get(index).copied()
    }

    fn at_wrapped(&self, x: usize, y: usize, z: usize) -> u32 {
        let wx = x % self.size.width;
        let wy = y % self.size.height;
        let wz = z % self.size.depth;
        self.get(wx, wy, wz).unwrap_or_default()
    }
}

/// One `N`×`N` (or `N`×`N`×`N`) block extracted from a sample, with the number
/// of times it occurs there. Cells are stored x-fastest within the block.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    /// The block's symbols, x-fastest; length `N²` in 2D, `N³` in 3D.
    pub cells: Vec<u32>,
    /// How often this block occurs in the sample; used as the pattern's
    /// weight.
    pub occurrences: u32,
}

impl Pattern {
    /// The block's origin symbol (its `(0, 0, 0)` cell): the symbol an output
    /// cell holding this pattern contributes to [`WfcGrid::values`].
    pub fn origin(&self) -> u32 {
        self.cells.first().copied().unwrap_or(0)
    }
}

/// The overlapping WFC model: patterns are the `N`-blocks occurring in a
/// sample grid, weighted by frequency; two patterns may be adjacent exactly
/// when they agree on their overlap (Gumin 2016). The model remembers whether
/// its sample was 2D or 3D and refuses output grids of the other kind.
#[derive(Clone, Debug)]
pub struct OverlappingModel {
    patterns: Vec<Pattern>,
    pattern_size: usize,
    dimensions: usize,
    origins: Vec<u32>,
    compiled: CompiledModel,
}

impl OverlappingModel {
    /// Builds a model from a sample, treating the sample as periodic when
    /// extracting patterns (blocks may wrap around the sample edges, as in
    /// Gumin's original). `n` is the block size: `N`×`N` for a 2D sample,
    /// `N`×`N`×`N` for a 3D sample.
    ///
    /// Returns [`WfcError::InvalidModel`] when `n == 0` or `n` exceeds any
    /// sample dimension.
    pub fn from_sample(sample: &SampleGrid, n: usize) -> Result<Self, WfcError> {
        Self::from_sample_with(sample, n, true)
    }

    /// Like [`OverlappingModel::from_sample`], with explicit control over
    /// whether extracted blocks may wrap around the sample edges. Without
    /// wrapping, only blocks fully inside the sample become patterns.
    pub fn from_sample_with(sample: &SampleGrid, n: usize, periodic_input: bool) -> Result<Self, WfcError> {
        Self::from_sample_symmetric(sample, n, periodic_input, Symmetry::None)
    }

    /// Like [`OverlappingModel::from_sample_with`], adding every block's
    /// transforms under `symmetry` (each transform of an occurrence counts as
    /// an occurrence, so symmetric blocks gain weight). Patterns are kept in
    /// first-occurrence order of the blocks and, per block, in
    /// [`Symmetry::transforms`] order; with [`Symmetry::None`] the result is
    /// exactly [`OverlappingModel::from_sample_with`]'s. A 3D sample turns
    /// about its vertical (`z`) axis, layer by layer.
    pub fn from_sample_symmetric(
        sample: &SampleGrid,
        n: usize,
        periodic_input: bool,
        symmetry: Symmetry,
    ) -> Result<Self, WfcError> {
        let size = sample.size();
        let dimensions = size.dimensions();
        if n == 0 {
            return Err(invalid("pattern size N must be at least 1"));
        }
        if n > size.width || n > size.height || (dimensions == 3 && n > size.depth) {
            return Err(invalid(format!(
                "pattern size N={n} exceeds the sample size {size:?}"
            )));
        }
        let Some(block_len) = n.checked_pow(dimensions as u32).filter(|len| *len > 0) else {
            return Err(invalid("pattern size overflows"));
        };
        // Extract distinct blocks in first-occurrence order (z, then y, then
        // x), counting occurrences. A BTreeMap keeps lookup deterministic.
        let (max_x, max_y, max_z) = if periodic_input {
            (size.width, size.height, size.depth)
        } else {
            (
                size.width + 1 - n,
                size.height + 1 - n,
                if dimensions == 3 { size.depth + 1 - n } else { 1 },
            )
        };
        let mut patterns: Vec<Pattern> = Vec::new();
        let mut seen: BTreeMap<Vec<u32>, usize> = BTreeMap::new();
        for z in 0..max_z {
            for y in 0..max_y {
                for x in 0..max_x {
                    let mut block = Vec::with_capacity(block_len);
                    for pz in 0..n {
                        for py in 0..n {
                            for px in 0..n {
                                if dimensions == 2 && pz > 0 {
                                    continue;
                                }
                                let v = if periodic_input {
                                    sample.at_wrapped(x + px, y + py, z + pz)
                                } else {
                                    sample.get(x + px, y + py, z + pz).unwrap_or_default()
                                };
                                block.push(v);
                            }
                        }
                    }
                    let layers = if dimensions == 3 { n } else { 1 };
                    for &(rotation, mirrored) in symmetry.transforms() {
                        let block = if (rotation, mirrored) == (0, false) {
                            block.clone()
                        } else {
                            turn_block(&block, n, layers, rotation, mirrored)
                        };
                        match seen.get(&block) {
                            Some(id) => {
                                if let Some(p) = patterns.get_mut(*id) {
                                    p.occurrences = p.occurrences.saturating_add(1);
                                }
                            }
                            None => {
                                seen.insert(block.clone(), patterns.len());
                                patterns.push(Pattern {
                                    cells: block,
                                    occurrences: 1,
                                });
                            }
                        }
                    }
                }
            }
        }
        if patterns.is_empty() {
            return Err(invalid("the sample produced no patterns"));
        }
        // Adjacency: pattern q may sit in direction d from pattern p when p
        // and q agree on the sub-block they would share, i.e. for every cell
        // offset t of p whose counterpart t-delta lies inside the block,
        // p[t] == q[t-delta]. For N == 1 there is no shared sub-block and
        // every pair is compatible.
        let count = patterns.len();
        let Some(table_len) = 6usize.checked_mul(count).and_then(|v| v.checked_mul(count)) else {
            return Err(invalid("too many patterns"));
        };
        let mut allowed = vec![false; table_len];
        for dir in Direction::ALL_3D {
            if dimensions == 2 && matches!(dir, Direction::Up | Direction::Down) {
                // Unused on 2D output; keep it permissive.
                for a in 0..count {
                    for b in 0..count {
                        allowed[((dir.index() * count) + a) * count + b] = true;
                    }
                }
                continue;
            }
            let delta = dir.delta();
            for (a, pa) in patterns.iter().enumerate() {
                for (b, pb) in patterns.iter().enumerate() {
                    if blocks_agree(&pa.cells, &pb.cells, n, dimensions, delta) {
                        allowed[((dir.index() * count) + a) * count + b] = true;
                    }
                }
            }
        }
        let weights: Vec<f64> = patterns.iter().map(|p| f64::from(p.occurrences.max(1))).collect();
        let origins: Vec<u32> = patterns.iter().map(Pattern::origin).collect();
        let compiled = CompiledModel::compile(weights, allowed)?;
        Ok(Self {
            patterns,
            pattern_size: n,
            dimensions,
            origins,
            compiled,
        })
    }

    /// All extracted patterns, in pattern-id order.
    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// The block size `N` the model was built with.
    pub fn pattern_size(&self) -> usize {
        self.pattern_size
    }

    /// `2` for a model built from a 2D sample, `3` for a 3D sample.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Number of distinct patterns.
    pub fn tile_count(&self) -> usize {
        self.compiled.count
    }

    /// Per-pattern weights (occurrence counts), in pattern-id order.
    pub fn weights(&self) -> &[f64] {
        &self.compiled.weights
    }

    /// May pattern `b` sit in `direction` from pattern `a` (do they agree on
    /// their overlap)?
    pub fn allows(&self, a: TileId, direction: Direction, b: TileId) -> bool {
        self.compiled.allows(a, direction, b)
    }

    /// Whether `block` (an x-fastest `N`-block of symbols) is one of the
    /// patterns extracted from the sample.
    pub fn contains_pattern(&self, block: &[u32]) -> bool {
        self.patterns.iter().any(|p| p.cells == block)
    }

    /// Solves this model on a grid of `size`; shorthand for the free
    /// [`solve`] with `WfcModel::Overlapping`.
    pub fn solve(
        &self,
        size: GridSize,
        constraints: &CellConstraints,
        seed: u64,
        opts: &WfcOptions,
    ) -> Result<WfcGrid, WfcError> {
        solve_inner(
            &self.compiled,
            Some(self.dimensions),
            Some(&self.origins),
            size,
            constraints,
            seed,
            opts,
        )
    }
}

/// Do blocks `p` and `q` agree where they would overlap if `q` were shifted
/// by `delta` (one step) from `p`? Blocks are x-fastest `n^dimensions`.
fn blocks_agree(p: &[u32], q: &[u32], n: usize, dimensions: usize, delta: [i32; 3]) -> bool {
    let depth = if dimensions == 3 { n } else { 1 };
    for z in 0..depth {
        for y in 0..n {
            for x in 0..n {
                let sx = x as i32 - delta[0];
                let sy = y as i32 - delta[1];
                let sz = z as i32 - delta[2];
                if sx < 0 || sy < 0 || sz < 0 {
                    continue;
                }
                let (sx, sy, sz) = (sx as usize, sy as usize, sz as usize);
                if sx >= n || sy >= n || sz >= depth {
                    continue;
                }
                let p_idx = x + n * (y + n * z);
                let q_idx = sx + n * (sy + n * sz);
                match (p.get(p_idx), q.get(q_idx)) {
                    (Some(a), Some(b)) if a == b => {}
                    (Some(_), Some(_)) => return false,
                    _ => {}
                }
            }
        }
    }
    true
}

/// Either WFC model, for callers that want to switch between them at runtime:
/// the free [`solve`] function takes this type.
#[derive(Clone, Debug)]
pub enum WfcModel {
    /// A model declared as tiles plus adjacency rules.
    Tiled(TiledModel),
    /// A model learned from a sample grid.
    Overlapping(OverlappingModel),
}

impl WfcModel {
    /// Number of tiles (tiled model) or patterns (overlapping model).
    pub fn tile_count(&self) -> usize {
        match self {
            WfcModel::Tiled(m) => m.tile_count(),
            WfcModel::Overlapping(m) => m.tile_count(),
        }
    }

    /// Per-tile/pattern weights, in id order.
    pub fn weights(&self) -> &[f64] {
        match self {
            WfcModel::Tiled(m) => m.weights(),
            WfcModel::Overlapping(m) => m.weights(),
        }
    }

    /// May tile/pattern `b` sit in `direction` from tile/pattern `a`?
    pub fn allows(&self, a: TileId, direction: Direction, b: TileId) -> bool {
        match self {
            WfcModel::Tiled(m) => m.allows(a, direction, b),
            WfcModel::Overlapping(m) => m.allows(a, direction, b),
        }
    }

    /// The dimensionality the model requires of the output grid: `Some(2)` or
    /// `Some(3)` for an overlapping model (from its sample), `None` for a
    /// tiled model, which works in both.
    pub fn dimensions(&self) -> Option<usize> {
        match self {
            WfcModel::Tiled(_) => None,
            WfcModel::Overlapping(m) => Some(m.dimensions()),
        }
    }

    /// Solves this model; shorthand for the free [`solve`] function.
    pub fn solve(
        &self,
        size: GridSize,
        constraints: &CellConstraints,
        seed: u64,
        opts: &WfcOptions,
    ) -> Result<WfcGrid, WfcError> {
        solve(self, size, constraints, seed, opts)
    }
}

impl From<TiledModel> for WfcModel {
    fn from(model: TiledModel) -> Self {
        WfcModel::Tiled(model)
    }
}

impl From<OverlappingModel> for WfcModel {
    fn from(model: OverlappingModel) -> Self {
        WfcModel::Overlapping(model)
    }
}

/// Per-cell pre-constraints: individual cells restricted to a subset of
/// tiles/patterns before solving starts (for example, "every border cell is a
/// wall"). Coordinates are `(x, y, z)`; use `z == 0` on 2D grids. Constraining
/// the same cell twice replaces the earlier subset.
///
/// Validity (coordinates inside the output size, non-empty subsets, tile ids
/// inside the model) is checked by [`solve`], which returns
/// [`WfcError::InvalidModel`] on any problem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellConstraints {
    by_cell: BTreeMap<[usize; 3], Vec<TileId>>,
}

impl CellConstraints {
    /// An empty constraint set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts the cell at `(x, y, z)` to the tiles in `allowed`, replacing
    /// any earlier restriction on that cell.
    pub fn force(&mut self, coord: [usize; 3], allowed: &[TileId]) -> &mut Self {
        // Store sorted and deduplicated so the solver sees a canonical set
        // regardless of the order the caller used.
        let mut set: Vec<TileId> = allowed.to_vec();
        set.sort_unstable();
        set.dedup();
        self.by_cell.insert(coord, set);
        self
    }

    /// Restricts the 2D cell at `(x, y)` to the tiles in `allowed`.
    pub fn force_2d(&mut self, x: usize, y: usize, allowed: &[TileId]) -> &mut Self {
        self.force([x, y, 0], allowed)
    }

    /// Restricts one cell to exactly one tile.
    pub fn force_one(&mut self, coord: [usize; 3], tile: TileId) -> &mut Self {
        self.force(coord, &[tile])
    }

    /// Constrains every border cell of the 2D grid `size` (first and last row
    /// and column, layer `z == 0`) to `allowed`.
    pub fn with_border_2d(size: GridSize, allowed: &[TileId]) -> Self {
        let mut out = Self::new();
        if size.width == 0 || size.height == 0 {
            return out;
        }
        for x in 0..size.width {
            out.force_2d(x, 0, allowed);
            out.force_2d(x, size.height - 1, allowed);
        }
        for y in 0..size.height {
            out.force_2d(0, y, allowed);
            out.force_2d(size.width - 1, y, allowed);
        }
        out
    }

    /// The subset a cell is restricted to, or `None` when unconstrained.
    pub fn get(&self, coord: [usize; 3]) -> Option<&[TileId]> {
        self.by_cell.get(&coord).map(Vec::as_slice)
    }

    /// Number of constrained cells.
    pub fn len(&self) -> usize {
        self.by_cell.len()
    }

    /// Whether no cell is constrained.
    pub fn is_empty(&self) -> bool {
        self.by_cell.is_empty()
    }

    /// Iterates `(coord, allowed)` pairs in coordinate order.
    pub fn iter(&self) -> impl Iterator<Item = ([usize; 3], &[TileId])> {
        self.by_cell
            .iter()
            .map(|(coord, allowed)| (*coord, allowed.as_slice()))
    }
}

/// Solver options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WfcOptions {
    /// Whether the output grid wraps around its edges.
    pub boundary: Boundary,
    /// Maximum number of chronological backtracks before giving up with
    /// [`WfcError::BacktrackLimit`]. `0` disables backtracking entirely: the
    /// first contradiction reached after a collapse decision fails the solve.
    pub max_backtracks: usize,
}

impl Default for WfcOptions {
    fn default() -> Self {
        Self {
            boundary: Boundary::Fixed,
            max_backtracks: 10_000,
        }
    }
}

impl WfcOptions {
    /// Options with a periodic (wrapping) boundary.
    pub fn periodic() -> Self {
        Self {
            boundary: Boundary::Periodic,
            ..Self::default()
        }
    }

    /// Returns these options with the backtrack budget replaced.
    pub fn with_max_backtracks(mut self, max_backtracks: usize) -> Self {
        self.max_backtracks = max_backtracks;
        self
    }

    /// Returns these options with the boundary replaced.
    pub fn with_boundary(mut self, boundary: Boundary) -> Self {
        self.boundary = boundary;
        self
    }
}

/// A solved grid: one tile/pattern id per cell, plus one output symbol per
/// cell. For a tiled model the symbol is the tile id itself; for an
/// overlapping model it is the origin symbol of the pattern the cell holds,
/// so `values` reads as a symbol grid of the same kind as the sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WfcGrid {
    size: GridSize,
    tiles: Vec<TileId>,
    values: Vec<u32>,
}

impl WfcGrid {
    /// The grid dimensions.
    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Number of cells along `x`.
    pub fn width(&self) -> usize {
        self.size.width
    }

    /// Number of cells along `y`.
    pub fn height(&self) -> usize {
        self.size.height
    }

    /// Number of cells along `z` (1 for a 2D grid).
    pub fn depth(&self) -> usize {
        self.size.depth
    }

    /// Total number of cells.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// Whether the grid has no cells (never true for a solved grid).
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// Tile/pattern id per cell, x-fastest.
    pub fn tiles(&self) -> &[TileId] {
        &self.tiles
    }

    /// Output symbol per cell, x-fastest (see the type documentation).
    pub fn values(&self) -> &[u32] {
        &self.values
    }

    /// The tile/pattern id at `(x, y, z)`, or `None` outside the grid.
    pub fn get(&self, x: usize, y: usize, z: usize) -> Option<TileId> {
        let index = self.size.index(x, y, z)?;
        self.tiles.get(index).copied()
    }

    /// The tile/pattern id at the 2D position `(x, y)` (`z == 0`).
    pub fn get_2d(&self, x: usize, y: usize) -> Option<TileId> {
        self.get(x, y, 0)
    }

    /// The output symbol at `(x, y, z)`, or `None` outside the grid.
    pub fn value(&self, x: usize, y: usize, z: usize) -> Option<u32> {
        let index = self.size.index(x, y, z)?;
        self.values.get(index).copied()
    }

    /// The output symbol at the 2D position `(x, y)` (`z == 0`).
    pub fn value_2d(&self, x: usize, y: usize) -> Option<u32> {
        self.value(x, y, 0)
    }

    /// Checks every adjacent pair of cells (including wrapping pairs when
    /// `boundary` is [`Boundary::Periodic`]) against `model`'s adjacency
    /// rules. Always true for a grid returned by [`solve`] with the same
    /// model and boundary; provided for tests and content validation.
    pub fn validate(&self, model: &WfcModel, boundary: Boundary) -> bool {
        let dims = self.size.dimensions();
        for index in 0..self.tiles.len() {
            let Some(coord) = self.size.coords(index) else {
                return false;
            };
            let Some(a) = self.tiles.get(index).copied() else {
                return false;
            };
            for dir in Direction::ALL_3D {
                if dims == 2 && matches!(dir, Direction::Up | Direction::Down) {
                    continue;
                }
                let Some(neighbor) = neighbor_index(self.size, coord, dir, boundary) else {
                    continue;
                };
                let Some(b) = self.tiles.get(neighbor).copied() else {
                    return false;
                };
                if !model.allows(a, dir, b) {
                    return false;
                }
            }
        }
        true
    }
}

/// A complete solve request bundled as one value: model, output size,
/// per-cell constraints, seed and options. [`WfcSpec::solve`] runs it.
#[derive(Clone, Debug)]
pub struct WfcSpec {
    /// The model to solve.
    pub model: WfcModel,
    /// The output grid size.
    pub size: GridSize,
    /// Per-cell pre-constraints.
    pub constraints: CellConstraints,
    /// Seed for the internal [`Rng`]; the same spec and seed always produce
    /// the same grid.
    pub seed: u64,
    /// Boundary handling and the backtrack budget.
    pub options: WfcOptions,
}

impl WfcSpec {
    /// A spec with no constraints and default options.
    pub fn new(model: WfcModel, size: GridSize, seed: u64) -> Self {
        Self {
            model,
            size,
            constraints: CellConstraints::new(),
            seed,
            options: WfcOptions::default(),
        }
    }

    /// Returns this spec with the constraints replaced.
    pub fn with_constraints(mut self, constraints: CellConstraints) -> Self {
        self.constraints = constraints;
        self
    }

    /// Returns this spec with the options replaced.
    pub fn with_options(mut self, options: WfcOptions) -> Self {
        self.options = options;
        self
    }

    /// Runs the solve described by this spec.
    pub fn solve(&self) -> Result<WfcGrid, WfcError> {
        solve(
            &self.model,
            self.size,
            &self.constraints,
            self.seed,
            &self.options,
        )
    }
}

/// Solves `model` on a grid of `size` under the per-cell `constraints`,
/// drawing all randomness from an [`Rng`] seeded with `seed`.
///
/// On success every cell holds exactly one tile/pattern and every adjacent
/// pair (wrapping pairs included for a periodic boundary) satisfies the
/// model's adjacency rules. Returns [`WfcError::InvalidModel`] for malformed
/// input, [`WfcError::Contradiction`] when no valid grid exists, and
/// [`WfcError::BacktrackLimit`] when the backtrack budget runs out first.
pub fn solve(
    model: &WfcModel,
    size: GridSize,
    constraints: &CellConstraints,
    seed: u64,
    opts: &WfcOptions,
) -> Result<WfcGrid, WfcError> {
    match model {
        WfcModel::Tiled(m) => solve_inner(&m.compiled, None, None, size, constraints, seed, opts),
        WfcModel::Overlapping(m) => solve_inner(
            &m.compiled,
            Some(m.dimensions),
            Some(&m.origins),
            size,
            constraints,
            seed,
            opts,
        ),
    }
}

/// [`solve`] that calls `cancelled` before every decision and every backtrack and stops with
/// [`WfcError::Cancelled`] once it answers `true` (the search scans every cell per decision, so a
/// large grid may take very long). A check that never trips gives [`solve`]'s grid.
///
/// # Errors
///
/// As [`solve`], plus [`WfcError::Cancelled`].
pub fn solve_cancellable(
    model: &WfcModel,
    size: GridSize,
    constraints: &CellConstraints,
    seed: u64,
    opts: &WfcOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<WfcGrid, WfcError> {
    let (compiled, overlap) = match model {
        WfcModel::Tiled(m) => (&m.compiled, None),
        WfcModel::Overlapping(m) => (&m.compiled, Some((m.dimensions, m.origins.as_slice()))),
    };
    let mut work = 0;
    solve_counted(
        compiled,
        overlap,
        size,
        constraints,
        seed,
        SolveControl { opts, cancelled },
        &mut work,
    )
}

/// The neighbor of the cell at `coord` in `dir`, as a linear index, or
/// `None` when a fixed boundary has no cell there (or the direction is along
/// `z` on a 2D grid).
fn neighbor_index(size: GridSize, coord: [usize; 3], dir: Direction, boundary: Boundary) -> Option<usize> {
    if size.dimensions() == 2 && matches!(dir, Direction::Up | Direction::Down) {
        return None;
    }
    let dims = [size.width, size.height, size.depth];
    let delta = dir.delta();
    let mut next = [0usize; 3];
    for axis in 0..3 {
        let dim = dims[axis];
        if dim == 0 {
            return None;
        }
        let moved = coord[axis] as i64 + i64::from(delta[axis]);
        next[axis] = match boundary {
            Boundary::Periodic => moved.rem_euclid(dim as i64) as usize,
            Boundary::Fixed => {
                if moved < 0 || moved >= dim as i64 {
                    return None;
                }
                moved as usize
            }
        };
    }
    size.index(next[0], next[1], next[2])
}

/// One reversible change made by propagation, recorded so a backtrack can
/// undo it.
#[derive(Clone, Copy, Debug)]
enum TrailEntry {
    /// Pattern `pattern` was removed from the domain of `cell`.
    Removed { cell: usize, pattern: usize },
    /// The support count of `pattern` in `cell` for direction `dir` was
    /// decremented by one.
    SupportDecremented { cell: usize, dir: usize, pattern: usize },
}

/// One collapse choice on the decision stack.
#[derive(Clone, Copy, Debug)]
struct Decision {
    /// The cell that was collapsed.
    cell: usize,
    /// The pattern it was collapsed to.
    pattern: usize,
    /// Trail length just before the collapse was applied; undoing back to
    /// this length restores the state the decision was made in.
    trail_checkpoint: usize,
}

/// Mutable solver state: domains, support counts, the undo trail and the
/// decision stack.
struct Solver<'a> {
    model: &'a CompiledModel,
    cells: usize,
    /// `neighbors[cell * 6 + dir]`: linear index of the neighboring cell,
    /// or `-1` when there is none.
    neighbors: Vec<i64>,
    /// `possible[cell * P + pattern]`: is `pattern` still possible in `cell`?
    possible: Vec<bool>,
    /// Number of still-possible patterns per cell.
    remaining: Vec<usize>,
    /// `supports[(cell * 6 + dir) * P + pattern]`: how many patterns still
    /// possible in the `dir` neighbor allow `pattern` here. Kept at 1 for
    /// directions without a neighbor, where it never changes.
    supports: Vec<i32>,
    /// Sum of weights of the still-possible patterns, per cell.
    weight_sum: Vec<f64>,
    /// Sum of `w * log2(w)` over the still-possible patterns, per cell.
    weight_log_sum: Vec<f64>,
    /// Small per-cell entropy tie-break noise, drawn once from the seeded RNG.
    noise: Vec<f64>,
    /// Cached per-cell entropy (noise included); see `entropy_dirty`.
    entropy_cache: Vec<f64>,
    /// Cells whose cached entropy is stale because their domain changed.
    /// Recomputing lazily keeps the per-collapse scan free of logarithm
    /// evaluations for untouched cells.
    entropy_dirty: Vec<bool>,
    trail: Vec<TrailEntry>,
    decisions: Vec<Decision>,
    backtracks_used: usize,
    /// Deterministic work count: propagation steps plus cells scanned by the
    /// entropy search plus trail entries undone. Lets tests bound the cost of
    /// a solve without a wall clock.
    work: u64,
    rng: Rng,
}

impl Solver<'_> {
    fn support_index(&self, cell: usize, dir: usize, pattern: usize) -> usize {
        (cell * 6 + dir) * self.model.count + pattern
    }

    fn neighbor(&self, cell: usize, dir: usize) -> Option<usize> {
        match self.neighbors.get(cell * 6 + dir) {
            Some(v) if *v >= 0 => Some(*v as usize),
            _ => None,
        }
    }

    /// Removes `pattern` from `cell`'s domain (recording the change on the
    /// trail and queueing it for propagation). Returns true when the pattern
    /// was still present.
    fn ban(&mut self, cell: usize, pattern: usize, stack: &mut Vec<(usize, usize)>) -> bool {
        let at = cell * self.model.count + pattern;
        match self.possible.get_mut(at) {
            Some(flag) if *flag => {
                *flag = false;
            }
            _ => return false,
        }
        if let Some(left) = self.remaining.get_mut(cell) {
            *left = left.saturating_sub(1);
        }
        if let (Some(w), Some(sum)) = (self.model.weights.get(pattern), self.weight_sum.get_mut(cell)) {
            *sum -= *w;
        }
        if let (Some(wl), Some(sum)) = (
            self.model.weight_log_weight.get(pattern),
            self.weight_log_sum.get_mut(cell),
        ) {
            *sum -= *wl;
        }
        if let Some(dirty) = self.entropy_dirty.get_mut(cell) {
            *dirty = true;
        }
        self.trail.push(TrailEntry::Removed { cell, pattern });
        stack.push((cell, pattern));
        true
    }

    /// AC-4 style (support-count) arc-consistency propagation over support counts, draining
    /// the stack of `(cell, pattern)` removals. Returns false the moment some
    /// cell's domain becomes empty (a contradiction); the partially updated
    /// state is exactly what the trail records, so a backtrack can undo it.
    fn propagate(&mut self, stack: &mut Vec<(usize, usize)>) -> bool {
        while let Some((cell, removed)) = stack.pop() {
            self.work += 1;
            // The removed pattern supported, in each direction dir, the
            // patterns of reverse_list[dir][removed] sitting in the cell on
            // the opposite side (the cell whose dir-neighbor is `cell`).
            for dir in 0..6 {
                let Some(direction) = Direction::from_index(dir) else {
                    continue;
                };
                let opposite = direction.opposite().index();
                let Some(other_cell) = self.neighbor(cell, opposite) else {
                    continue;
                };
                // Take the model reference out first (it is a copyable
                // shared reference) so the loop can mutate other fields.
                let model = self.model;
                let Some(supported) = model.reverse_list.get(dir * model.count + removed) else {
                    continue;
                };
                let mut to_ban: Vec<usize> = Vec::new();
                for pattern in supported.iter().copied() {
                    let at = self.support_index(other_cell, dir, pattern);
                    let Some(support) = self.supports.get_mut(at) else {
                        continue;
                    };
                    *support -= 1;
                    self.trail.push(TrailEntry::SupportDecremented {
                        cell: other_cell,
                        dir,
                        pattern,
                    });
                    if *support == 0 {
                        to_ban.push(pattern);
                    }
                }
                for pattern in to_ban {
                    if self.ban(other_cell, pattern, stack)
                        && self.remaining.get(other_cell).copied() == Some(0)
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Undoes trail entries until only `target_len` remain, restoring domains
    /// and support counts.
    fn undo_to(&mut self, target_len: usize) {
        while self.trail.len() > target_len {
            let Some(entry) = self.trail.pop() else {
                break;
            };
            self.work += 1;
            match entry {
                TrailEntry::Removed { cell, pattern } => {
                    let at = cell * self.model.count + pattern;
                    if let Some(flag) = self.possible.get_mut(at) {
                        *flag = true;
                    }
                    if let Some(left) = self.remaining.get_mut(cell) {
                        *left += 1;
                    }
                    if let (Some(w), Some(sum)) =
                        (self.model.weights.get(pattern), self.weight_sum.get_mut(cell))
                    {
                        *sum += *w;
                    }
                    if let (Some(wl), Some(sum)) = (
                        self.model.weight_log_weight.get(pattern),
                        self.weight_log_sum.get_mut(cell),
                    ) {
                        *sum += *wl;
                    }
                    if let Some(dirty) = self.entropy_dirty.get_mut(cell) {
                        *dirty = true;
                    }
                }
                TrailEntry::SupportDecremented { cell, dir, pattern } => {
                    let at = self.support_index(cell, dir, pattern);
                    if let Some(support) = self.supports.get_mut(at) {
                        *support += 1;
                    }
                }
            }
        }
    }

    /// The undecided cell (≥ 2 patterns left) of lowest entropy, or `None`
    /// when every cell is decided. Entropy is
    /// `log2(Σw) − (Σ w·log2 w) / Σw` over the cell's remaining patterns;
    /// the per-cell seeded noise breaks ties.
    fn lowest_entropy_cell(&mut self) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        self.work += self.cells as u64;
        for cell in 0..self.cells {
            if self.remaining.get(cell).copied() <= Some(1) {
                continue;
            }
            if self.entropy_dirty.get(cell).copied() == Some(true) {
                let mut total = self.weight_sum.get(cell).copied().unwrap_or(0.0);
                if !(total > 0.0 && total.is_finite()) {
                    // The running sums are updated by subtraction; with
                    // extreme weight ratios rounding can drive them to zero
                    // while two or more patterns remain. Rebuild them exactly.
                    total = self.recompute_sums(cell);
                }
                let entropy = if total > 0.0 && total.is_finite() {
                    let logged = self.weight_log_sum.get(cell).copied().unwrap_or(0.0);
                    let noise = self.noise.get(cell).copied().unwrap_or(0.0);
                    detmath::log2(total) - logged / total + noise
                } else {
                    // Still not positive (all remaining weights underflow):
                    // the cell is undecided and must be collapsed, so give it
                    // the lowest possible entropy rather than skipping it.
                    self.noise.get(cell).copied().unwrap_or(0.0)
                };
                if let Some(slot) = self.entropy_cache.get_mut(cell) {
                    *slot = entropy;
                }
                if let Some(dirty) = self.entropy_dirty.get_mut(cell) {
                    *dirty = false;
                }
            }
            let entropy = self.entropy_cache.get(cell).copied().unwrap_or(f64::INFINITY);
            if !entropy.is_finite() {
                continue;
            }
            match best {
                Some((_, best_entropy)) if entropy >= best_entropy => {}
                _ => best = Some((cell, entropy)),
            }
        }
        best.map(|(cell, _)| cell)
    }

    /// Rebuilds `cell`'s weight sums from its current domain and returns the
    /// new weight sum.
    fn recompute_sums(&mut self, cell: usize) -> f64 {
        let count = self.model.count;
        let mut total = 0.0;
        let mut logged = 0.0;
        for pattern in 0..count {
            if self.possible.get(cell * count + pattern).copied() == Some(true) {
                total += self.model.weights.get(pattern).copied().unwrap_or(0.0);
                logged += self.model.weight_log_weight.get(pattern).copied().unwrap_or(0.0);
            }
        }
        if let Some(slot) = self.weight_sum.get_mut(cell) {
            *slot = total;
        }
        if let Some(slot) = self.weight_log_sum.get_mut(cell) {
            *slot = logged;
        }
        total
    }

    /// Picks one still-possible pattern of `cell` with probability
    /// proportional to its weight.
    fn choose_pattern(&mut self, cell: usize) -> usize {
        // Sum the domain exactly here (this loop is O(patterns) anyway) so a
        // drifted running sum cannot bias the draw.
        let count = self.model.count;
        let total = self.recompute_sums(cell);
        let mut fallback = 0usize;
        let mut draw = self.rng.f64() * total;
        for pattern in 0..count {
            if self.possible.get(cell * count + pattern).copied() != Some(true) {
                continue;
            }
            fallback = pattern;
            let weight = self.model.weights.get(pattern).copied().unwrap_or(0.0);
            if draw < weight {
                return pattern;
            }
            draw -= weight;
        }
        fallback
    }

    /// Restricts `cell` to `pattern` alone and propagates. Returns false on a
    /// contradiction (in which case the state is the trail's responsibility).
    fn collapse_and_propagate(&mut self, cell: usize, pattern: usize) -> bool {
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for other in 0..self.model.count {
            if other != pattern {
                self.ban(cell, other, &mut stack);
            }
        }
        if self.remaining.get(cell).copied() == Some(0) {
            return false;
        }
        self.propagate(&mut stack)
    }

    /// Forbids `pattern` in `cell` and propagates. Returns false on a
    /// contradiction.
    fn forbid_and_propagate(&mut self, cell: usize, pattern: usize) -> bool {
        let mut stack: Vec<(usize, usize)> = Vec::new();
        self.ban(cell, pattern, &mut stack);
        if self.remaining.get(cell).copied() == Some(0) {
            return false;
        }
        self.propagate(&mut stack)
    }

    /// Chronological backtracking after a contradiction: repeatedly pops the
    /// newest decision, undoes the trail to that decision's checkpoint, and
    /// forbids the pattern that decision chose, until propagation succeeds
    /// again. Each popped decision costs one backtrack.
    fn backtrack(&mut self, max_backtracks: usize, cancelled: &dyn Fn() -> bool) -> Result<(), WfcError> {
        loop {
            if cancelled() {
                return Err(WfcError::Cancelled);
            }
            let Some(decision) = self.decisions.pop() else {
                return Err(WfcError::Contradiction);
            };
            if self.backtracks_used >= max_backtracks {
                return Err(WfcError::BacktrackLimit);
            }
            self.backtracks_used += 1;
            self.undo_to(decision.trail_checkpoint);
            if self.forbid_and_propagate(decision.cell, decision.pattern) {
                return Ok(());
            }
            // Forbidding that pattern also dead-ends: keep unwinding. The
            // earlier checkpoints are all below this trail position, so the
            // next undo also reverts this forbid, as it should.
        }
    }

    fn run(&mut self, max_backtracks: usize, cancelled: &dyn Fn() -> bool) -> Result<(), WfcError> {
        loop {
            if cancelled() {
                return Err(WfcError::Cancelled);
            }
            let Some(cell) = self.lowest_entropy_cell() else {
                // Every cell holds exactly one pattern (empty domains only
                // arise inside propagate, which reports them before this).
                return Ok(());
            };
            let pattern = self.choose_pattern(cell);
            self.decisions.push(Decision {
                cell,
                pattern,
                trail_checkpoint: self.trail.len(),
            });
            if !self.collapse_and_propagate(cell, pattern) {
                self.backtrack(max_backtracks, cancelled)?;
            }
        }
    }

    /// The single remaining pattern of each cell, after a successful run.
    fn solved_tiles(&self) -> Vec<TileId> {
        let mut out = Vec::with_capacity(self.cells);
        for cell in 0..self.cells {
            let mut chosen = 0usize;
            for pattern in 0..self.model.count {
                if self.possible.get(cell * self.model.count + pattern).copied() == Some(true) {
                    chosen = pattern;
                    break;
                }
            }
            out.push(chosen);
        }
        out
    }
}

/// The shared solve path for both models. `required_dimensions` is `Some(2)`
/// or `Some(3)` for an overlapping model, `None` for a tiled model;
/// `origins` maps pattern ids to output symbols for an overlapping model.
fn solve_inner(
    model: &CompiledModel,
    required_dimensions: Option<usize>,
    origins: Option<&[u32]>,
    size: GridSize,
    constraints: &CellConstraints,
    seed: u64,
    opts: &WfcOptions,
) -> Result<WfcGrid, WfcError> {
    let overlap = match (required_dimensions, origins) {
        (Some(dims), Some(origins)) => Some((dims, origins)),
        _ => None,
    };
    let mut work = 0;
    solve_counted(
        model,
        overlap,
        size,
        constraints,
        seed,
        SolveControl {
            opts,
            cancelled: &|| false,
        },
        &mut work,
    )
}

/// The options of a solve and its cancel check.
#[derive(Clone, Copy)]
struct SolveControl<'a> {
    opts: &'a WfcOptions,
    cancelled: &'a dyn Fn() -> bool,
}

/// [`solve_inner`] that also reports the solver's deterministic work count
/// (see `Solver::work`) through `work`, success or failure. `overlap` is the
/// overlapping model's required dimensionality and pattern origin symbols.
fn solve_counted(
    model: &CompiledModel,
    overlap: Option<(usize, &[u32])>,
    size: GridSize,
    constraints: &CellConstraints,
    seed: u64,
    control: SolveControl<'_>,
    work: &mut u64,
) -> Result<WfcGrid, WfcError> {
    let SolveControl { opts, cancelled } = control;
    if size.width == 0 || size.height == 0 || size.depth == 0 {
        return Err(invalid("output grid dimensions must all be non-zero"));
    }
    let Some(cells) = size.cell_count() else {
        return Err(invalid("output grid dimensions overflow"));
    };
    if cells > MAX_CELLS {
        return Err(invalid(format!(
            "output grid of {cells} cells exceeds the {MAX_CELLS}-cell limit"
        )));
    }
    if let Some((dims, _)) = overlap
        && dims != size.dimensions()
    {
        return Err(invalid(format!(
            "the model is {dims}D but the output size {size:?} is {}D",
            size.dimensions()
        )));
    }
    // Validate constraints before allocating solver state.
    for (coord, allowed) in constraints.iter() {
        if size.index(coord[0], coord[1], coord[2]).is_none() {
            return Err(invalid(format!(
                "constraint cell {coord:?} is outside the output size {size:?}"
            )));
        }
        if allowed.is_empty() {
            return Err(invalid(format!(
                "constraint cell {coord:?} allows no tiles; the allowed subset must be non-empty"
            )));
        }
        for tile in allowed {
            if *tile >= model.count {
                return Err(invalid(format!(
                    "constraint cell {coord:?} allows tile {tile} but the model has only {} tiles",
                    model.count
                )));
            }
        }
    }
    let count = model.count;
    let Some(domain_len) = cells.checked_mul(count) else {
        return Err(invalid("grid times tile count overflows"));
    };
    let Some(support_len) = domain_len.checked_mul(6) else {
        return Err(invalid("support table size overflows"));
    };
    if support_len > MAX_SUPPORT_ENTRIES {
        return Err(invalid(format!(
            "{cells} cells x {count} tiles needs {support_len} support counters, over the \
             {MAX_SUPPORT_ENTRIES} limit; use a smaller grid or fewer tiles/patterns"
        )));
    }
    // Neighbor table (also pins down the boundary rule in exactly one place).
    let mut neighbors = vec![-1i64; cells * 6];
    for cell in 0..cells {
        let Some(coord) = size.coords(cell) else {
            continue;
        };
        for dir in Direction::ALL_3D {
            if let Some(other) = neighbor_index(size, coord, dir, opts.boundary) {
                neighbors[cell * 6 + dir.index()] = other as i64;
            }
        }
    }
    let mut rng = Rng::new(seed);
    let noise: Vec<f64> = (0..cells).map(|_| rng.f64() * 1e-6).collect();
    let full_weight: f64 = model.weights.iter().sum();
    let full_weight_log: f64 = model.weight_log_weight.iter().sum();
    let mut solver = Solver {
        model,
        cells,
        neighbors,
        possible: vec![true; domain_len],
        remaining: vec![count; cells],
        supports: vec![1; support_len],
        weight_sum: vec![full_weight; cells],
        weight_log_sum: vec![full_weight_log; cells],
        noise,
        entropy_cache: vec![f64::INFINITY; cells],
        entropy_dirty: vec![true; cells],
        trail: Vec::new(),
        decisions: Vec::new(),
        backtracks_used: 0,
        work: 0,
        rng,
    };
    // Initial support counts against the full domains.
    for cell in 0..cells {
        for dir in 0..6 {
            let Some(neighbor) = solver.neighbor(cell, dir) else {
                continue;
            };
            for pattern in 0..count {
                let mut support = 0i32;
                if let Some(list) = model.allowed_list.get(dir * count + pattern) {
                    for other in list.iter().copied() {
                        if solver.possible.get(neighbor * count + other).copied() == Some(true) {
                            support += 1;
                        }
                    }
                }
                let at = solver.support_index(cell, dir, pattern);
                if let Some(slot) = solver.supports.get_mut(at) {
                    *slot = support;
                }
            }
        }
    }
    // Seed removals: patterns with no support in some direction that has a
    // neighbor can never survive there, and the pre-constraints.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for cell in 0..cells {
        for pattern in 0..count {
            let mut unsupported = false;
            for dir in 0..6 {
                if solver.neighbor(cell, dir).is_some() {
                    let at = solver.support_index(cell, dir, pattern);
                    if solver.supports.get(at).copied() == Some(0) {
                        unsupported = true;
                        break;
                    }
                }
            }
            if unsupported {
                solver.ban(cell, pattern, &mut stack);
            }
        }
    }
    for (coord, allowed) in constraints.iter() {
        let Some(cell) = size.index(coord[0], coord[1], coord[2]) else {
            continue;
        };
        let mut keep = vec![false; count];
        for tile in allowed.iter().copied() {
            if let Some(flag) = keep.get_mut(tile) {
                *flag = true;
            }
        }
        for pattern in 0..count {
            if keep.get(pattern).copied() != Some(true) {
                solver.ban(cell, pattern, &mut stack);
            }
        }
    }
    for cell in 0..cells {
        if solver.remaining.get(cell).copied() == Some(0) {
            return Err(WfcError::Contradiction);
        }
    }
    let initial_ok = solver.propagate(&mut stack);
    *work = solver.work;
    if !initial_ok {
        return Err(WfcError::Contradiction);
    }
    solver.trail.clear();
    // Decisions measure checkpoints against a fresh trail; the bans above are
    // part of the problem statement and must never be undone. Support decrements
    // they caused are already folded into the support table.
    let outcome = solver.run(opts.max_backtracks, cancelled);
    *work = solver.work;
    outcome?;
    let tiles = solver.solved_tiles();
    let values: Vec<u32> = match overlap {
        Some((_, origins)) => tiles
            .iter()
            .map(|tile| origins.get(*tile).copied().unwrap_or(0))
            .collect(),
        None => tiles.iter().map(|tile| *tile as u32).collect(),
    };
    Ok(WfcGrid { size, tiles, values })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Instant;

    use super::*;

    /// Unwraps a result in tests without `unwrap`/`expect` (workspace lints).
    fn must<T>(result: Result<T, WfcError>) -> T {
        match result {
            Ok(value) => value,
            Err(err) => panic!("unexpected WFC error: {err}"),
        }
    }

    fn must_err<T>(result: Result<T, WfcError>) -> WfcError {
        match result {
            Ok(_) => panic!("expected a WFC error but the operation succeeded"),
            Err(err) => err,
        }
    }

    // Edge sockets per tile: [east, south, west, north]; 1 = road, 0 = grass.
    // Neighbors are compatible exactly when their touching sockets agree.
    const ROAD_SOCKETS: [[u8; 4]; 4] = [
        [0, 0, 0, 0], // 0 grass
        [1, 0, 1, 0], // 1 road east-west
        [0, 1, 0, 1], // 2 road north-south
        [1, 1, 1, 1], // 3 crossroads
    ];

    fn socket_of(tile: usize, dir: Direction) -> u8 {
        let sockets = ROAD_SOCKETS[tile];
        match dir {
            Direction::East => sockets[0],
            Direction::South => sockets[1],
            Direction::West => sockets[2],
            Direction::North => sockets[3],
            Direction::Up | Direction::Down => 0,
        }
    }

    /// The road/grass tileset. All-grass is always a valid grid, so any solve
    /// of any size has a solution; roads make the solutions non-trivial.
    fn road_model() -> TiledModel {
        let tiles = vec![
            TileDef::with_weight("grass", 6.0),
            TileDef::new("road_ew"),
            TileDef::new("road_ns"),
            TileDef::new("crossroads"),
        ];
        let mut rules = Vec::new();
        for a in 0..4 {
            for dir in Direction::ALL_2D {
                let allowed: Vec<TileId> = (0..4)
                    .filter(|b| socket_of(*b, dir.opposite()) == socket_of(a, dir))
                    .collect();
                rules.push(AdjacencyRule::new(a, dir, allowed));
            }
        }
        must(TiledModel::new(tiles, rules))
    }

    /// Verifies every adjacent pair of the grid against the model sockets,
    /// computing neighbors independently of the solver's own tables.
    fn assert_road_grid_valid(grid: &WfcGrid, periodic: bool) {
        let size = grid.size();
        for z in 0..size.depth {
            for y in 0..size.height {
                for x in 0..size.width {
                    let a = match grid.get(x, y, z) {
                        Some(v) => v,
                        None => panic!("cell ({x},{y},{z}) missing"),
                    };
                    for dir in Direction::ALL_2D {
                        let d = dir.delta();
                        let nx = x as i32 + d[0];
                        let ny = y as i32 + d[1];
                        let (nx, ny) = if periodic {
                            (
                                nx.rem_euclid(size.width as i32) as usize,
                                ny.rem_euclid(size.height as i32) as usize,
                            )
                        } else {
                            if nx < 0 || ny < 0 || nx >= size.width as i32 || ny >= size.height as i32 {
                                continue;
                            }
                            (nx as usize, ny as usize)
                        };
                        let b = match grid.get(nx, ny, z) {
                            Some(v) => v,
                            None => panic!("neighbor ({nx},{ny},{z}) missing"),
                        };
                        assert_eq!(
                            socket_of(a, dir),
                            socket_of(b, dir.opposite()),
                            "edge ({x},{y},{z})={a} --{dir:?}--> ({nx},{ny},{z})={b} disagrees"
                        );
                    }
                }
            }
        }
    }

    fn solve_road(size: GridSize, seed: u64, opts: &WfcOptions) -> WfcGrid {
        let model = road_model();
        must(model.solve(size, &CellConstraints::new(), seed, opts))
    }

    /// A 3-cell ring (periodic width 3) over a tile-adjacency graph where the
    /// huge-weight trap tile 0 touches only tile 2. Arc consistency alone
    /// cannot see that putting the trap anywhere dead-ends the ring (its two
    /// ring neighbors would both have to be tile 2, which cannot sit next to
    /// itself), so the solver is forced into a contradiction right after its
    /// first, almost surely trap, collapse — and must backtrack to solve.
    fn trap_ring_model() -> TiledModel {
        // Tile-adjacency graph edges: 1-2, 2-3, 3-1 (a triangle: the ring
        // solution) plus the trap edge 0-2.
        let graph: [&[TileId]; 4] = [&[2], &[2, 3], &[0, 1, 3], &[1, 2]];
        let tiles = vec![
            TileDef::with_weight("trap", 1.0e12),
            TileDef::new("a"),
            TileDef::new("b"),
            TileDef::new("c"),
        ];
        let mut rules = Vec::new();
        for (a, neighbors) in graph.iter().enumerate() {
            for dir in [Direction::East, Direction::West] {
                rules.push(AdjacencyRule::new(a, dir, neighbors.to_vec()));
            }
        }
        must(TiledModel::new(tiles, rules))
    }

    fn trap_ring_solve(model: &TiledModel, seed: u64, opts: &WfcOptions) -> Result<WfcGrid, WfcError> {
        model.solve(GridSize::new_2d(3, 1), &CellConstraints::new(), seed, opts)
    }

    #[test]
    fn tiled_road_grass_valid_over_200_seeds() {
        let model = road_model();
        let wrapped = WfcModel::Tiled(road_model());
        for seed in 0..200u64 {
            let grid = must(model.solve(
                GridSize::new_2d(16, 16),
                &CellConstraints::new(),
                seed,
                &WfcOptions::default(),
            ));
            assert_road_grid_valid(&grid, false);
            assert!(grid.validate(&wrapped, Boundary::Fixed), "seed {seed}");
        }
    }

    #[test]
    fn forced_border_constraints_are_honored() {
        // Wall (0) and floor (1) may sit next to anything; only the border
        // constraint keeps walls on the border.
        let model = must(TiledModel::new(
            vec![TileDef::new("wall"), TileDef::with_weight("floor", 3.0)],
            Vec::new(),
        ));
        let size = GridSize::new_2d(10, 8);
        let constraints = CellConstraints::with_border_2d(size, &[0]);
        for seed in 0..20u64 {
            let grid = must(model.solve(size, &constraints, seed, &WfcOptions::default()));
            for x in 0..size.width {
                assert_eq!(grid.get_2d(x, 0), Some(0), "seed {seed} top border x={x}");
                assert_eq!(
                    grid.get_2d(x, size.height - 1),
                    Some(0),
                    "seed {seed} bottom border x={x}"
                );
            }
            for y in 0..size.height {
                assert_eq!(grid.get_2d(0, y), Some(0), "seed {seed} left border y={y}");
                assert_eq!(
                    grid.get_2d(size.width - 1, y),
                    Some(0),
                    "seed {seed} right border y={y}"
                );
            }
        }
    }

    #[test]
    fn forced_subset_constraint_is_honored() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        // The whole middle row must be road-bearing tiles (never plain grass).
        for x in 0..12 {
            constraints.force_2d(x, 5, &[1, 3]);
        }
        let grid = must(model.solve(GridSize::new_2d(12, 12), &constraints, 7, &WfcOptions::default()));
        for x in 0..12 {
            let tile = grid.get_2d(x, 5);
            assert!(
                tile == Some(1) || tile == Some(3),
                "middle-row cell x={x} holds {tile:?}, allowed road_ew/crossroads only"
            );
        }
        assert_road_grid_valid(&grid, false);
    }

    #[test]
    fn periodic_boundary_wraps_validly() {
        let model = road_model();
        let wrapped = WfcModel::Tiled(road_model());
        let opts = WfcOptions::periodic();
        for seed in 0..25u64 {
            let grid = must(model.solve(GridSize::new_2d(12, 10), &CellConstraints::new(), seed, &opts));
            // Includes the wrapping edges (rightmost vs leftmost, etc.).
            assert_road_grid_valid(&grid, true);
            assert!(grid.validate(&wrapped, Boundary::Periodic), "seed {seed}");
        }
    }

    #[test]
    fn unsatisfiable_ring_returns_contradiction() {
        // Two tiles that must differ from their east/west neighbors cannot
        // color an odd ring of three cells.
        let tiles = vec![TileDef::new("black"), TileDef::new("white")];
        let mut rules = Vec::new();
        for a in 0..2usize {
            for dir in [Direction::East, Direction::West] {
                rules.push(AdjacencyRule::new(a, dir, vec![1 - a]));
            }
        }
        let model = must(TiledModel::new(tiles, rules));
        for seed in 0..5u64 {
            let err = must_err(model.solve(
                GridSize::new_2d(3, 1),
                &CellConstraints::new(),
                seed,
                &WfcOptions::periodic(),
            ));
            assert_eq!(err, WfcError::Contradiction, "seed {seed}");
        }
    }

    #[test]
    fn contradictory_forced_cells_return_contradiction() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        // road_ew (1) east of road_ns (2): road_ew's west socket is road,
        // road_ns's east socket is grass — they can never touch this way.
        constraints.force_one([0, 0, 0], 2);
        constraints.force_one([1, 0, 0], 1);
        let err = must_err(model.solve(GridSize::new_2d(4, 4), &constraints, 3, &WfcOptions::default()));
        assert_eq!(err, WfcError::Contradiction);
    }

    #[test]
    fn backtracking_solves_the_trap_ring() {
        let model = trap_ring_model();
        let opts = WfcOptions::periodic();
        for seed in 0..10u64 {
            let grid = must(trap_ring_solve(&model, seed, &opts));
            // The only solutions are the a-b-c triangles: no trap tile, and
            // every ring edge joins graph neighbors.
            for index in 0..3 {
                let a = grid.tiles()[index];
                let b = grid.tiles()[(index + 1) % 3];
                assert_ne!(a, 0, "seed {seed}: trap tile in a solved ring");
                assert!(
                    model.allows(a, Direction::East, b),
                    "seed {seed}: invalid ring edge {a}->{b}"
                );
            }
        }
    }

    #[test]
    fn trap_ring_with_zero_backtrack_budget_hits_the_limit() {
        let model = trap_ring_model();
        let opts = WfcOptions::periodic().with_max_backtracks(0);
        for seed in 0..5u64 {
            let err = must_err(trap_ring_solve(&model, seed, &opts));
            assert_eq!(err, WfcError::BacktrackLimit, "seed {seed}");
        }
    }

    #[test]
    fn determinism_over_50_repeats_per_seed() {
        let model = road_model();
        let opts = WfcOptions::default();
        let size = GridSize::new_2d(14, 14);
        let first = must(model.solve(size, &CellConstraints::new(), 42, &opts));
        for repeat in 0..50 {
            let grid = must(model.solve(size, &CellConstraints::new(), 42, &opts));
            assert_eq!(grid, first, "repeat {repeat} differs");
        }
    }

    #[test]
    fn different_seeds_give_different_grids() {
        let model = road_model();
        let opts = WfcOptions::default();
        let size = GridSize::new_2d(14, 14);
        let mut distinct: BTreeSet<Vec<TileId>> = BTreeSet::new();
        for seed in 0..10u64 {
            let grid = must(model.solve(size, &CellConstraints::new(), seed, &opts));
            distinct.insert(grid.tiles().to_vec());
        }
        assert!(
            distinct.len() >= 8,
            "10 seeds produced only {} distinct grids",
            distinct.len()
        );
    }

    #[test]
    fn solve_3d_8x8x8() {
        // Cubes joined by pipes: sockets per tile [east, south, up] (+ the
        // mirrored opposite face), neighbors must agree on shared faces.
        // Tile 0 is solid (no pipe on any face) so a solution always exists.
        let faces: [[u8; 3]; 5] = [
            [0, 0, 0], // solid
            [1, 0, 0], // pipe along x
            [0, 1, 0], // pipe along y
            [0, 0, 1], // pipe along z
            [1, 1, 1], // junction
        ];
        let face_of = |tile: usize, dir: Direction| -> u8 {
            let f = faces[tile];
            match dir {
                Direction::East | Direction::West => f[0],
                Direction::South | Direction::North => f[1],
                Direction::Up | Direction::Down => f[2],
            }
        };
        let tiles: Vec<TileDef> = ["solid", "pipe_x", "pipe_y", "pipe_z", "junction"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i == 0 {
                    TileDef::with_weight(*name, 5.0)
                } else {
                    TileDef::new(*name)
                }
            })
            .collect();
        let mut rules = Vec::new();
        for a in 0..5 {
            for dir in Direction::ALL_3D {
                let allowed: Vec<TileId> = (0..5)
                    .filter(|b| face_of(*b, dir.opposite()) == face_of(a, dir))
                    .collect();
                rules.push(AdjacencyRule::new(a, dir, allowed));
            }
        }
        let model = must(TiledModel::new(tiles, rules));
        let grid = must(model.solve(
            GridSize::new_3d(8, 8, 8),
            &CellConstraints::new(),
            99,
            &WfcOptions::default(),
        ));
        assert_eq!(grid.len(), 512);
        // Independently verify all edges on all three axes.
        for z in 0..8 {
            for y in 0..8 {
                for x in 0..8 {
                    let a = match grid.get(x, y, z) {
                        Some(v) => v,
                        None => panic!("missing cell ({x},{y},{z})"),
                    };
                    for dir in Direction::ALL_3D {
                        let d = dir.delta();
                        let (nx, ny, nz) = (x as i32 + d[0], y as i32 + d[1], z as i32 + d[2]);
                        if nx < 0 || ny < 0 || nz < 0 || nx >= 8 || ny >= 8 || nz >= 8 {
                            continue;
                        }
                        let b = match grid.get(nx as usize, ny as usize, nz as usize) {
                            Some(v) => v,
                            None => panic!("missing neighbor"),
                        };
                        assert_eq!(
                            face_of(a, dir),
                            face_of(b, dir.opposite()),
                            "3D edge ({x},{y},{z})={a} --{dir:?}--> ({nx},{ny},{nz})={b}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn performance_guard_64x64_debug() {
        // The repo forbids wall-clock asserts in tests (shared, loaded build
        // hosts make them flaky), so the budget is a deterministic work count:
        // propagation steps + cells scanned by the entropy search + trail
        // entries undone. The classic solver is O(cells^2) in the entropy
        // scan; the budget allows twice that, so an accidental blow-up (a
        // backtracking storm, an extra loop level) fails here on every host.
        let model = road_model();
        let size = GridSize::new_2d(64, 64);
        let cells = 64u64 * 64;
        let mut work = 0u64;
        let start = Instant::now();
        let result = solve_counted(
            &model.compiled,
            None,
            size,
            &CellConstraints::new(),
            1234,
            SolveControl {
                opts: &WfcOptions::default(),
                cancelled: &|| false,
            },
            &mut work,
        );
        let elapsed = start.elapsed();
        let grid = must(result);
        assert_road_grid_valid(&grid, false);
        eprintln!("64x64 road solve: work {work}, wall {elapsed:?} (informational)");
        assert!(
            work <= 2 * cells * cells,
            "64x64 solve did {work} work units, budget {}",
            2 * cells * cells
        );
    }

    #[test]
    fn weights_bias_the_output() {
        // Two fully compatible tiles; the 100x heavier one must dominate a
        // 20x20 grid by a wide margin.
        let model = must(TiledModel::new(
            vec![
                TileDef::with_weight("heavy", 100.0),
                TileDef::with_weight("light", 1.0),
            ],
            Vec::new(),
        ));
        let grid = must(model.solve(
            GridSize::new_2d(20, 20),
            &CellConstraints::new(),
            5,
            &WfcOptions::default(),
        ));
        let heavy = grid.tiles().iter().filter(|t| **t == 0).count();
        assert!(
            heavy > 350,
            "heavy tile filled only {heavy}/400 cells despite a 100:1 weight"
        );
    }

    #[test]
    fn single_tile_single_cell() {
        let model = must(TiledModel::new(vec![TileDef::new("only")], Vec::new()));
        let grid = must(model.solve(
            GridSize::new_2d(1, 1),
            &CellConstraints::new(),
            0,
            &WfcOptions::default(),
        ));
        assert_eq!(grid.get_2d(0, 0), Some(0));
        assert_eq!(grid.value_2d(0, 0), Some(0));
    }

    #[test]
    fn forced_single_cell_is_honored() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        constraints.force_one([3, 3, 0], 3); // crossroads
        let grid = must(model.solve(GridSize::new_2d(8, 8), &constraints, 11, &WfcOptions::default()));
        assert_eq!(grid.get_2d(3, 3), Some(3));
        assert_road_grid_valid(&grid, false);
    }

    #[test]
    fn one_cell_wide_strip_solves() {
        let grid = solve_road(GridSize::new_2d(1, 24), 8, &WfcOptions::default());
        assert_road_grid_valid(&grid, false);
        let grid = solve_road(GridSize::new_2d(24, 1), 8, &WfcOptions::default());
        assert_road_grid_valid(&grid, false);
    }

    #[test]
    fn empty_tileset_is_invalid() {
        let err = must_err(TiledModel::new(Vec::new(), Vec::new()));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn nan_weight_is_invalid() {
        let err = must_err(TiledModel::new(
            vec![TileDef::with_weight("bad", f64::NAN)],
            Vec::new(),
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn non_positive_and_infinite_weights_are_invalid() {
        for weight in [0.0, -1.0, f64::INFINITY, f64::NEG_INFINITY] {
            let err = must_err(TiledModel::new(
                vec![TileDef::with_weight("bad", weight)],
                Vec::new(),
            ));
            assert!(
                matches!(err, WfcError::InvalidModel(_)),
                "weight {weight}: {err:?}"
            );
        }
    }

    #[test]
    fn rule_naming_a_missing_tile_is_invalid() {
        let err = must_err(TiledModel::new(
            vec![TileDef::new("a")],
            vec![AdjacencyRule::new(1, Direction::East, vec![0])],
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
        let err = must_err(TiledModel::new(
            vec![TileDef::new("a")],
            vec![AdjacencyRule::new(0, Direction::East, vec![7])],
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn zero_sized_output_is_invalid() {
        let model = road_model();
        for size in [
            GridSize::new_2d(0, 5),
            GridSize::new_2d(5, 0),
            GridSize::new_3d(5, 5, 0),
        ] {
            let err = must_err(model.solve(size, &CellConstraints::new(), 1, &WfcOptions::default()));
            assert!(matches!(err, WfcError::InvalidModel(_)), "{size:?}: {err:?}");
        }
    }

    #[test]
    fn huge_output_is_invalid() {
        let model = road_model();
        let err = must_err(model.solve(
            GridSize::new_2d(100_000, 100_000),
            &CellConstraints::new(),
            1,
            &WfcOptions::default(),
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn constraint_outside_the_grid_is_invalid() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        constraints.force_one([9, 0, 0], 0);
        let err = must_err(model.solve(GridSize::new_2d(4, 4), &constraints, 1, &WfcOptions::default()));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn constraint_with_empty_subset_is_invalid() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        constraints.force([0, 0, 0], &[]);
        let err = must_err(model.solve(GridSize::new_2d(4, 4), &constraints, 1, &WfcOptions::default()));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn constraint_naming_a_missing_tile_is_invalid() {
        let model = road_model();
        let mut constraints = CellConstraints::new();
        constraints.force([0, 0, 0], &[0, 99]);
        let err = must_err(model.solve(GridSize::new_2d(4, 4), &constraints, 1, &WfcOptions::default()));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    fn sample_from_rows(rows: &[&[u32]]) -> SampleGrid {
        let height = rows.len();
        let width = match rows.first() {
            Some(row) => row.len(),
            None => 0,
        };
        let mut cells = Vec::new();
        for row in rows {
            cells.extend_from_slice(row);
        }
        must(SampleGrid::new_2d(width, height, cells))
    }

    fn assert_output_patterns_come_from_sample(model: &OverlappingModel, grid: &WfcGrid) {
        let n = model.pattern_size();
        let size = grid.size();
        assert!(size.width >= n && size.height >= n, "test grid smaller than N");
        for y in 0..=(size.height - n) {
            for x in 0..=(size.width - n) {
                let mut block = Vec::new();
                for dy in 0..n {
                    for dx in 0..n {
                        match grid.value_2d(x + dx, y + dy) {
                            Some(v) => block.push(v),
                            None => panic!("missing value at ({},{})", x + dx, y + dy),
                        }
                    }
                }
                assert!(
                    model.contains_pattern(&block),
                    "output block at ({x},{y}) = {block:?} never occurs in the sample"
                );
            }
        }
    }

    #[test]
    fn overlapping_checkerboard_reproduces_only_sample_patterns() {
        let sample = sample_from_rows(&[
            &[0, 1, 0, 1, 0, 1],
            &[1, 0, 1, 0, 1, 0],
            &[0, 1, 0, 1, 0, 1],
            &[1, 0, 1, 0, 1, 0],
            &[0, 1, 0, 1, 0, 1],
            &[1, 0, 1, 0, 1, 0],
        ]);
        let model = must(OverlappingModel::from_sample(&sample, 2));
        assert_eq!(model.pattern_size(), 2);
        assert_eq!(model.dimensions(), 2);
        for seed in 0..5u64 {
            let grid = must(model.solve(
                GridSize::new_2d(12, 12),
                &CellConstraints::new(),
                seed,
                &WfcOptions::default(),
            ));
            assert_output_patterns_come_from_sample(&model, &grid);
        }
    }

    #[test]
    fn overlapping_stripes_with_n3_reproduce_only_sample_patterns() {
        // Vertical double-stripes: 0 0 1 1 2 2 across, constant down.
        let row: &[u32] = &[0, 0, 1, 1, 2, 2, 0, 0, 1, 1, 2, 2];
        let sample = sample_from_rows(&[row, row, row, row, row, row]);
        let model = must(OverlappingModel::from_sample(&sample, 3));
        let grid = must(model.solve(
            GridSize::new_2d(16, 10),
            &CellConstraints::new(),
            21,
            &WfcOptions::default(),
        ));
        assert_output_patterns_come_from_sample(&model, &grid);
        // The repeating structure must show in the output values.
        let distinct: BTreeSet<u32> = grid.values().iter().copied().collect();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn overlapping_output_is_deterministic() {
        let sample = sample_from_rows(&[&[0, 1, 0, 1], &[1, 0, 1, 0], &[0, 1, 0, 1], &[1, 0, 1, 0]]);
        let model = must(OverlappingModel::from_sample(&sample, 2));
        let size = GridSize::new_2d(10, 10);
        let first = must(model.solve(size, &CellConstraints::new(), 77, &WfcOptions::default()));
        for _ in 0..10 {
            let grid = must(model.solve(size, &CellConstraints::new(), 77, &WfcOptions::default()));
            assert_eq!(grid, first);
        }
    }

    #[test]
    fn overlapping_pattern_weights_are_occurrence_counts() {
        // Three 0s and one 1 in a 2x2 sample with N=1: patterns are the
        // single symbols, weighted 3 and 1.
        let sample = sample_from_rows(&[&[0, 0], &[0, 1]]);
        let model = must(OverlappingModel::from_sample(&sample, 1));
        assert_eq!(model.tile_count(), 2);
        let mut weights: Vec<u32> = model.weights().iter().map(|w| *w as u32).collect();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 3]);
    }

    #[test]
    fn overlapping_n1_output_uses_only_sample_symbols() {
        let sample = sample_from_rows(&[&[5, 7], &[7, 5]]);
        let model = must(OverlappingModel::from_sample(&sample, 1));
        let grid = must(model.solve(
            GridSize::new_2d(9, 9),
            &CellConstraints::new(),
            4,
            &WfcOptions::default(),
        ));
        for value in grid.values() {
            assert!(*value == 5 || *value == 7, "symbol {value} not in the sample");
        }
    }

    #[test]
    fn overlapping_zero_n_is_invalid() {
        let sample = sample_from_rows(&[&[0, 1], &[1, 0]]);
        let err = must_err(OverlappingModel::from_sample(&sample, 0));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn overlapping_n_larger_than_the_sample_is_invalid() {
        let sample = sample_from_rows(&[&[0, 1], &[1, 0]]);
        let err = must_err(OverlappingModel::from_sample(&sample, 3));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn sample_with_wrong_cell_count_is_invalid() {
        let err = must_err(SampleGrid::new_2d(3, 3, vec![0; 8]));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
        let err = must_err(SampleGrid::new_2d(0, 3, Vec::new()));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn overlapping_2d_model_refuses_a_3d_output() {
        let sample = sample_from_rows(&[&[0, 1], &[1, 0]]);
        let model = must(OverlappingModel::from_sample(&sample, 2));
        let err = must_err(model.solve(
            GridSize::new_3d(4, 4, 4),
            &CellConstraints::new(),
            1,
            &WfcOptions::default(),
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }

    #[test]
    fn overlapping_3d_sample_solves_a_3d_grid() {
        // A 3D checkerboard of period 2 along every axis.
        let mut cells = Vec::new();
        for z in 0..4u32 {
            for y in 0..4u32 {
                for x in 0..4u32 {
                    cells.push((x + y + z) % 2);
                }
            }
        }
        let sample = must(SampleGrid::new_3d(4, 4, 4, cells));
        let model = must(OverlappingModel::from_sample(&sample, 2));
        assert_eq!(model.dimensions(), 3);
        let grid = must(model.solve(
            GridSize::new_3d(6, 6, 6),
            &CellConstraints::new(),
            2,
            &WfcOptions::default(),
        ));
        // Every 2x2x2 window of the output values must occur in the sample.
        for z in 0..5 {
            for y in 0..5 {
                for x in 0..5 {
                    let mut block = Vec::new();
                    for dz in 0..2 {
                        for dy in 0..2 {
                            for dx in 0..2 {
                                match grid.value(x + dx, y + dy, z + dz) {
                                    Some(v) => block.push(v),
                                    None => panic!("missing 3D value"),
                                }
                            }
                        }
                    }
                    assert!(
                        model.contains_pattern(&block),
                        "3D block at ({x},{y},{z}) not in the sample"
                    );
                }
            }
        }
    }

    #[test]
    fn wfc_spec_round_trips_through_the_free_solve() {
        let spec = WfcSpec::new(WfcModel::Tiled(road_model()), GridSize::new_2d(10, 10), 123);
        let via_spec = must(spec.solve());
        let via_free = solve_road(GridSize::new_2d(10, 10), 123, &WfcOptions::default());
        assert_eq!(via_spec, via_free);
    }

    #[test]
    fn tiled_model_via_adjacency_predicate_matches_rules() {
        let names = vec![
            TileDef::new("grass"),
            TileDef::new("road_ew"),
            TileDef::new("road_ns"),
            TileDef::new("crossroads"),
        ];
        let via_predicate = must(TiledModel::with_adjacency(names, |a, dir, b| {
            if matches!(dir, Direction::Up | Direction::Down) {
                return true;
            }
            socket_of(a, dir) == socket_of(b, dir.opposite())
        }));
        let grid = must(via_predicate.solve(
            GridSize::new_2d(10, 10),
            &CellConstraints::new(),
            9,
            &WfcOptions::default(),
        ));
        assert_road_grid_valid(&grid, false);
    }

    #[test]
    fn direction_tables_are_consistent() {
        for dir in Direction::ALL_3D {
            assert_eq!(Direction::from_index(dir.index()), Some(dir));
            assert_eq!(dir.opposite().opposite(), dir);
            let d = dir.delta();
            let o = dir.opposite().delta();
            assert_eq!([d[0] + o[0], d[1] + o[1], d[2] + o[2]], [0, 0, 0]);
        }
        assert_eq!(Direction::from_index(6), None);
        assert_eq!(Direction::ALL_2D.len(), 4);
    }

    #[test]
    fn grid_size_index_and_coords_round_trip() {
        let size = GridSize::new_3d(4, 3, 2);
        for z in 0..2 {
            for y in 0..3 {
                for x in 0..4 {
                    let index = size.index(x, y, z);
                    assert!(index.is_some());
                    match index {
                        Some(i) => assert_eq!(size.coords(i), Some([x, y, z])),
                        None => panic!("index missing"),
                    }
                }
            }
        }
        assert_eq!(size.index(4, 0, 0), None);
        assert_eq!(size.coords(24), None);
        assert_eq!(size.cell_count(), Some(24));
    }

    #[test]
    fn constraint_replacement_and_lookup() {
        let mut constraints = CellConstraints::new();
        constraints.force_2d(1, 2, &[3, 1, 3]);
        assert_eq!(constraints.get([1, 2, 0]), Some(&[1, 3][..]));
        constraints.force_2d(1, 2, &[2]);
        assert_eq!(constraints.get([1, 2, 0]), Some(&[2][..]));
        assert_eq!(constraints.len(), 1);
        assert!(!constraints.is_empty());
        assert_eq!(constraints.get([0, 0, 0]), None);
    }

    // ---- Verification-lane probes (sprint-muse-verify-wfc) ----

    /// Tile 0 outweighs the others so far that `1e17 + 1 + 1 + 1` rounds to
    /// `1e17` and removing tile 0 from a cell's running weight sum leaves 0.0.
    /// The remaining three tiles must still be collapsed (a cell with two or
    /// more options left may never be skipped), so the grid stays valid.
    #[test]
    fn extreme_weight_ratio_never_leaves_a_cell_undecided() {
        let tiles = vec![
            TileDef::with_weight("huge", 1.0e17),
            TileDef::new("a"),
            TileDef::new("b"),
            TileDef::new("c"),
        ];
        // a -> b -> c -> a going east; "huge" goes with anything.
        let model = must(TiledModel::with_adjacency(tiles, |x, dir, y| {
            if x == 0 || y == 0 {
                return true;
            }
            match dir {
                Direction::East => y == x % 3 + 1,
                Direction::West => x == y % 3 + 1,
                _ => true,
            }
        }));
        let wrapped = WfcModel::Tiled(model.clone());
        let size = GridSize::new_2d(4, 1);
        let mut constraints = CellConstraints::new();
        for x in 0..4 {
            constraints.force_2d(x, 0, &[1, 2, 3]);
        }
        for seed in 0..10u64 {
            let grid = must(model.solve(size, &constraints, seed, &WfcOptions::default()));
            assert!(
                grid.validate(&wrapped, Boundary::Fixed),
                "seed {seed}: invalid grid {:?}",
                grid.tiles()
            );
        }
    }

    /// A room-and-corridor sample with N = 3: far richer than a checkerboard,
    /// so propagation regularly dead-ends and the solver has to backtrack.
    fn rooms_sample() -> SampleGrid {
        sample_from_rows(&[
            &[1, 1, 1, 1, 1, 1, 1, 1],
            &[1, 0, 0, 1, 0, 0, 0, 1],
            &[1, 0, 0, 1, 0, 0, 0, 1],
            &[1, 1, 0, 1, 1, 1, 0, 1],
            &[1, 0, 0, 0, 0, 1, 0, 1],
            &[1, 0, 1, 1, 0, 0, 0, 1],
            &[1, 0, 0, 1, 0, 0, 0, 1],
            &[1, 1, 1, 1, 1, 1, 1, 1],
        ])
    }

    #[test]
    fn overlapping_rooms_n3_reproduce_only_sample_patterns() {
        let model = must(OverlappingModel::from_sample(&rooms_sample(), 3));
        assert!(model.tile_count() > 10, "only {} patterns", model.tile_count());
        let mut solved = 0;
        for seed in 0..20u64 {
            for opts in [WfcOptions::default(), WfcOptions::periodic()] {
                match model.solve(GridSize::new_2d(20, 20), &CellConstraints::new(), seed, &opts) {
                    Ok(grid) => {
                        solved += 1;
                        assert_output_patterns_come_from_sample(&model, &grid);
                        assert!(grid.validate(&WfcModel::Overlapping(model.clone()), opts.boundary));
                    }
                    // A bounded search may give up; it must say so, not panic.
                    Err(err) => assert_eq!(err, WfcError::BacktrackLimit, "seed {seed}"),
                }
            }
        }
        assert!(solved >= 30, "only {solved}/40 rooms solves succeeded");
    }

    /// A seeded random symmetric tileset (10 tiles, each pair adjacent with
    /// probability 0.4 per axis). Arc consistency cannot see its global
    /// structure, so some solves dead-end after a collapse.
    fn random_tileset_model() -> TiledModel {
        let n = 10usize;
        let mut rng = Rng::new(27);
        let mut adj = vec![false; n * n * 2];
        for a in 0..n {
            for b in a..n {
                let h = rng.f64() < 0.4;
                adj[a * n + b] = h;
                adj[b * n + a] = h;
                let v = rng.f64() < 0.4;
                adj[n * n + a * n + b] = v;
                adj[n * n + b * n + a] = v;
            }
        }
        let tiles: Vec<TileDef> = (0..n)
            .map(|i| TileDef::with_weight(format!("t{i}"), 1.0 + i as f64))
            .collect();
        must(TiledModel::with_adjacency(tiles, |a, dir, b| match dir {
            Direction::East | Direction::West => adj[a * n + b],
            Direction::South | Direction::North => adj[n * n + a * n + b],
            Direction::Up | Direction::Down => true,
        }))
    }

    /// Restart-free solving matters: with no backtracking budget some solves
    /// of the random tileset dead-end, and with the default budget every one
    /// of those seeds solves to a valid grid.
    #[test]
    fn backtracking_rescues_solves_that_dead_end() {
        let model = random_tileset_model();
        let wrapped = WfcModel::Tiled(model.clone());
        let size = GridSize::new_2d(16, 16);
        let no_budget = WfcOptions::default().with_max_backtracks(0);
        let mut dead = 0;
        for seed in 0..40u64 {
            match model.solve(size, &CellConstraints::new(), seed, &no_budget) {
                Ok(grid) => assert!(grid.validate(&wrapped, Boundary::Fixed), "seed {seed}"),
                Err(err) => {
                    assert_eq!(err, WfcError::BacktrackLimit, "seed {seed}");
                    dead += 1;
                    let grid = must(model.solve(size, &CellConstraints::new(), seed, &WfcOptions::default()));
                    assert!(grid.validate(&wrapped, Boundary::Fixed), "rescued seed {seed}");
                }
            }
        }
        assert!(dead > 0, "no seed needed backtracking; the probe is too easy");
    }

    /// Determinism must hold on the backtracking path too, and over several
    /// seeds rather than one.
    #[test]
    fn determinism_over_50_repeats_for_several_seeds_and_both_models() {
        let road = road_model();
        let rooms = must(OverlappingModel::from_sample(&rooms_sample(), 3));
        // Seeds 0..40 of this model include solves that backtrack (see
        // backtracking_rescues_solves_that_dead_end).
        let random = random_tileset_model();
        for seed in 0..40u64 {
            let size = GridSize::new_2d(16, 16);
            let first = random.solve(size, &CellConstraints::new(), seed, &WfcOptions::default());
            for repeat in 0..5 {
                assert_eq!(
                    random.solve(size, &CellConstraints::new(), seed, &WfcOptions::default()),
                    first,
                    "random tileset seed {seed} repeat {repeat}"
                );
            }
        }
        for seed in [0u64, 1, 2, 3, 1_000_003] {
            let size = GridSize::new_2d(12, 12);
            let a = road.solve(size, &CellConstraints::new(), seed, &WfcOptions::periodic());
            let b = rooms.solve(size, &CellConstraints::new(), seed, &WfcOptions::default());
            for repeat in 0..50 {
                assert_eq!(
                    road.solve(size, &CellConstraints::new(), seed, &WfcOptions::periodic()),
                    a,
                    "road seed {seed} repeat {repeat}"
                );
                assert_eq!(
                    rooms.solve(size, &CellConstraints::new(), seed, &WfcOptions::default()),
                    b,
                    "rooms seed {seed} repeat {repeat}"
                );
            }
        }
    }

    #[test]
    fn overlapping_3d_slab_sample_reproduces_only_sample_patterns() {
        // Floors of 1 every third layer, with a 0/2 checker in between.
        let mut cells = Vec::new();
        for z in 0..6u32 {
            for y in 0..4u32 {
                for x in 0..4u32 {
                    cells.push(if z % 3 == 0 { 1 } else { 2 * ((x + y) % 2) });
                }
            }
        }
        let sample = must(SampleGrid::new_3d(4, 4, 6, cells));
        let model = must(OverlappingModel::from_sample(&sample, 2));
        for seed in 0..4u64 {
            let grid = must(model.solve(
                GridSize::new_3d(5, 5, 7),
                &CellConstraints::new(),
                seed,
                &WfcOptions::default(),
            ));
            for z in 0..6 {
                for y in 0..4 {
                    for x in 0..4 {
                        let mut block = Vec::new();
                        for dz in 0..2 {
                            for dy in 0..2 {
                                for dx in 0..2 {
                                    block.push(grid.value(x + dx, y + dy, z + dz).unwrap_or(u32::MAX));
                                }
                            }
                        }
                        assert!(model.contains_pattern(&block), "seed {seed} block ({x},{y},{z})");
                    }
                }
            }
        }
    }

    #[test]
    fn periodic_one_and_two_cell_axes_respect_self_adjacency() {
        // On a periodic 1-wide axis a cell is its own east neighbor, so only
        // self-compatible tiles may be chosen there.
        let model = must(TiledModel::with_adjacency(
            vec![TileDef::with_weight("loner", 50.0), TileDef::new("pair")],
            |a, dir, b| match dir {
                Direction::East | Direction::West => !(a == 0 && b == 0),
                _ => true,
            },
        ));
        let wrapped = WfcModel::Tiled(model.clone());
        for width in [1usize, 2] {
            for seed in 0..10u64 {
                let size = GridSize::new_2d(width, 3);
                let grid = must(model.solve(size, &CellConstraints::new(), seed, &WfcOptions::periodic()));
                assert!(
                    grid.validate(&wrapped, Boundary::Periodic),
                    "width {width} seed {seed}"
                );
                if width == 1 {
                    assert!(grid.tiles().iter().all(|t| *t == 1));
                }
            }
        }
    }

    #[test]
    fn exhausted_search_reports_contradiction_even_without_a_budget() {
        // Odd ring 2-coloring with max_backtracks = 0: the search needs a
        // backtrack to prove it, so the honest answer is BacktrackLimit;
        // with a budget it is Contradiction. Neither hangs nor panics.
        let model = must(TiledModel::with_adjacency(
            vec![TileDef::new("black"), TileDef::new("white")],
            |a, dir, b| match dir {
                Direction::East | Direction::West => a != b,
                _ => true,
            },
        ));
        let size = GridSize::new_2d(7, 1);
        let zero = WfcOptions::periodic().with_max_backtracks(0);
        assert_eq!(
            model.solve(size, &CellConstraints::new(), 1, &zero),
            Err(WfcError::BacktrackLimit)
        );
        assert_eq!(
            model.solve(size, &CellConstraints::new(), 1, &WfcOptions::periodic()),
            Err(WfcError::Contradiction)
        );
    }

    #[test]
    fn many_patterns_on_a_large_grid_is_invalid_not_an_abort() {
        // 1024 x 1024 cells is under MAX_CELLS, but with 64 tiles the support
        // table would need 1024 * 1024 * 64 * 6 entries (1.5 GiB).
        let tiles: Vec<TileDef> = (0..64).map(|i| TileDef::new(format!("t{i}"))).collect();
        let model = must(TiledModel::with_adjacency(tiles, |_, _, _| true));
        let err = must_err(model.solve(
            GridSize::new_2d(1024, 1024),
            &CellConstraints::new(),
            1,
            &WfcOptions::default(),
        ));
        assert!(matches!(err, WfcError::InvalidModel(_)), "{err:?}");
    }
}
