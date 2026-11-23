// SPDX-License-Identifier: MPL-2.0
//! Structural load (ADR-2734): gravity load relaxed through the bond graph to the support chunks.
//!
//! The model is a resistive network. Every attached chunk that some path of usable bonds joins to
//! a support chunk injects its weight (`mass * gravity`); support chunks are the ground (potential
//! 0); a bond conducts in proportion to its contact area. The potentials solve the weighted graph
//! Laplacian `L x = w` (Kirchhoff's current law at every non-support chunk), and a bond's load is
//! the flow through it, `area * (x_a - x_b)`. The flows conserve weight exactly: a column's bond
//! carries the weight above it, and an arch shares its load between its feet in proportion to how
//! well each side conducts. Chunks with no path to a support carry nothing (they are falling).
//!
//! The solve is a fixed number of conjugate-gradient steps (Hestenes and Stiefel, 1952) on the
//! symmetric positive definite reduced Laplacian, in `f64`, with sums taken in chunk and bond index
//! order. Only additions, multiplications and divisions are used, so a server reaches the same
//! loads on every platform. The model carries force, not bending moment: a cantilever fails at its
//! root bond only because that bond carries the whole overhang's weight.

use super::asset::DestructibleAsset;
use super::sim::ChunkState;

/// A bond's contact area below this (square metres) conducts as if it were this large, so a
/// zero-area bond still joins its chunks instead of leaving a singular system.
pub const MIN_LOAD_AREA: f64 = 1.0e-6;

/// The relative residual (squared) at which the solve stops early: the potentials are exact to
/// about twelve digits.
const STOP_RESIDUAL_SQ: f64 = 1.0e-24;

/// What one solve gives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSolution {
    /// Signed flow through each bond, newtons, positive from chunk `a` toward chunk `b`; 0 for a
    /// bond that is not usable or lies in an unsupported piece.
    pub flow: Vec<f32>,
    /// Weight (newtons) each support chunk receives through its bonds, as `(chunk, newtons)` in
    /// chunk order. A support's own weight is not in it.
    pub reactions: Vec<(u32, f32)>,
    /// Conjugate-gradient steps taken (at most the asset's `iterations`).
    pub iterations: u32,
    /// `sqrt(r.r / b.b)` after the last step (0 when there was nothing to solve).
    pub relative_residual: f64,
    /// Total weight (newtons) of the chunks that reach a support, supports excluded.
    pub supported_weight: f64,
    /// Attached chunks with no path to a support (they carry no load).
    pub unsupported_chunks: u32,
    /// `false` when a potential came out non-finite; every flow is then 0.
    pub finite: bool,
}

/// Solves the gravity load of one instance.
///
/// `usable[b]` says whether bond `b` carries load (intact and not about to break); `states` are the
/// chunk states; `chunk_bonds` lists each chunk's bonds. `gravity` is m/s^2 and `iterations` the
/// step cap.
#[must_use]
pub fn solve_load(
    asset: &DestructibleAsset,
    states: &[ChunkState],
    usable: &[bool],
    chunk_bonds: &[Vec<u32>],
    gravity: f32,
    iterations: u32,
) -> LoadSolution {
    let n = asset.chunks.len();
    let attached = |c: usize| states.get(c) == Some(&ChunkState::Attached);
    let other = |b: u32, c: usize| {
        let spec = &asset.bonds[b as usize];
        if spec.a as usize == c {
            spec.b as usize
        } else {
            spec.a as usize
        }
    };
    // Pieces joined by usable bonds; which reach a support.
    let mut piece = vec![u32::MAX; n];
    let mut piece_supported: Vec<bool> = Vec::new();
    let mut stack = Vec::new();
    for start in 0..n {
        if !attached(start) || piece[start] != u32::MAX {
            continue;
        }
        let label = piece_supported.len() as u32;
        let mut supported = false;
        piece[start] = label;
        stack.push(start);
        while let Some(c) = stack.pop() {
            supported |= asset.chunks[c].support;
            for &b in &chunk_bonds[c] {
                if !usable[b as usize] {
                    continue;
                }
                let o = other(b, c);
                if attached(o) && piece[o] == u32::MAX {
                    piece[o] = label;
                    stack.push(o);
                }
            }
        }
        piece_supported.push(supported);
    }
    let in_supported = |c: usize| piece[c] != u32::MAX && piece_supported[piece[c] as usize];
    // Unknowns: chunks in a supported piece that are not supports themselves.
    let mut unknown = vec![usize::MAX; n];
    let mut weights: Vec<f64> = Vec::new();
    let mut unsupported_chunks = 0u32;
    for (c, (slot, chunk)) in unknown.iter_mut().zip(&asset.chunks).enumerate() {
        if !attached(c) {
            continue;
        }
        if !in_supported(c) {
            unsupported_chunks += 1;
            continue;
        }
        if chunk.support {
            continue;
        }
        *slot = weights.len();
        weights.push(f64::from(chunk.mass) * f64::from(gravity));
    }
    // The bonds of the system, in bond order: (unknown or none for a, for b, conductance).
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    let mut edge_bond: Vec<usize> = Vec::new();
    for (b, spec) in asset.bonds.iter().enumerate() {
        let (a, c) = (spec.a as usize, spec.b as usize);
        if !usable[b] || !attached(a) || !attached(c) || !in_supported(a) {
            continue;
        }
        if unknown[a] == usize::MAX && unknown[c] == usize::MAX {
            continue; // Two supports: the ground holds both.
        }
        edges.push((unknown[a], unknown[c], f64::from(spec.area).max(MIN_LOAD_AREA)));
        edge_bond.push(b);
    }
    let m = weights.len();
    let mut x = vec![0.0f64; m];
    let mut taken = 0u32;
    let bb: f64 = weights.iter().map(|w| w * w).sum();
    let mut relative_residual = 0.0;
    if m > 0 && bb > 0.0 {
        let apply = |p: &[f64], out: &mut [f64]| {
            out.fill(0.0);
            for &(i, j, k) in &edges {
                let pi = if i == usize::MAX { 0.0 } else { p[i] };
                let pj = if j == usize::MAX { 0.0 } else { p[j] };
                let f = k * (pi - pj);
                if i != usize::MAX {
                    out[i] += f;
                }
                if j != usize::MAX {
                    out[j] -= f;
                }
            }
        };
        let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        let mut r = weights.clone();
        let mut p = r.clone();
        let mut ap = vec![0.0f64; m];
        let mut rs = bb;
        for _ in 0..iterations {
            if rs <= bb * STOP_RESIDUAL_SQ {
                break;
            }
            apply(&p, &mut ap);
            let pap = dot(&p, &ap);
            if !(pap > 0.0 && pap.is_finite()) {
                break;
            }
            let alpha = rs / pap;
            for ((xi, ri), (pi, api)) in x.iter_mut().zip(r.iter_mut()).zip(p.iter().zip(&ap)) {
                *xi += alpha * pi;
                *ri -= alpha * api;
            }
            let next = dot(&r, &r);
            let beta = next / rs;
            for (pi, ri) in p.iter_mut().zip(&r) {
                *pi = ri + beta * *pi;
            }
            rs = next;
            taken += 1;
        }
        relative_residual = (rs / bb).sqrt();
    }
    let mut finite = x.iter().all(|v| v.is_finite()) && relative_residual.is_finite();
    let mut flow = vec![0.0f32; asset.bonds.len()];
    let mut reaction = vec![0.0f64; n];
    if finite {
        for (&(i, j, k), &b) in edges.iter().zip(&edge_bond) {
            let xi = if i == usize::MAX { 0.0 } else { x[i] };
            let xj = if j == usize::MAX { 0.0 } else { x[j] };
            let f = k * (xi - xj);
            flow[b] = f as f32;
            let spec = &asset.bonds[b];
            if i == usize::MAX {
                reaction[spec.a as usize] -= f;
            }
            if j == usize::MAX {
                reaction[spec.b as usize] += f;
            }
        }
    }
    // A flow or reaction too large for `f32` is as unusable as a non-finite potential.
    if !(flow.iter().all(|f| f.is_finite()) && reaction.iter().all(|r| (*r as f32).is_finite())) {
        finite = false;
        flow.fill(0.0);
        reaction.fill(0.0);
    }
    let reactions = (0..n)
        .filter(|&c| attached(c) && asset.chunks[c].support && in_supported(c))
        .map(|c| (c as u32, reaction[c] as f32))
        .collect();
    LoadSolution {
        flow,
        reactions,
        iterations: taken,
        relative_residual,
        supported_weight: weights.iter().sum(),
        unsupported_chunks,
        finite,
    }
}

/// One bond in a [`LoadReport`].
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BondLoad {
    /// Bond index in the asset.
    pub bond: u32,
    /// Its lower chunk.
    pub a: u32,
    /// Its higher chunk.
    pub b: u32,
    /// The load it carries, newtons.
    pub load: f32,
    /// The load it carries without damage, newtons.
    pub max_load: f32,
    /// Its accumulated damage, newton-seconds.
    pub damage: f32,
    /// The damage that breaks it, newton-seconds.
    pub break_impulse: f32,
}

/// The structural load of one instance, solved on demand
/// ([`super::DestructionWorld::load_report`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LoadReport {
    /// The instance.
    pub destructible: super::sim::DestructibleId,
    /// The asset has `LoadSettings` (otherwise the default settings were used for this report,
    /// and the server never breaks a bond by load).
    pub enabled: bool,
    /// Gravity used, m/s^2.
    pub gravity: f32,
    /// Iteration cap of the solve.
    pub iterations: u32,
    /// Iterations the solve took.
    pub iterations_run: u32,
    /// Relative residual after the solve.
    pub relative_residual: f64,
    /// Weight (newtons) of the chunks that reach a support, supports excluded.
    pub supported_weight: f64,
    /// Attached chunks with no path to a support.
    pub unsupported_chunks: u32,
    /// Load-carrying bonds over their limit.
    pub over_limit: u32,
    /// Breaks decided and still held back by the per-tick cap.
    pub pending_breaks: u32,
    /// `Some("destruction_load_not_finite")` when the solve gave a non-finite potential (every load
    /// is then reported as 0 and no bond is overloaded).
    pub problem: Option<&'static str>,
    /// What each support chunk carries: `(chunk, newtons)`.
    pub supports: Vec<(u32, f32)>,
    /// The bonds nearest their limit, highest `load / max_load` first.
    pub top: Vec<BondLoad>,
}

/// Structural load totals of a world ([`super::DestructionWorld::load_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoadStats {
    /// Instances whose asset has `LoadSettings`.
    pub instances: u32,
    /// Solves run.
    pub solves: u64,
    /// Conjugate-gradient iterations over every solve.
    pub iterations: u64,
    /// Solves that gave a non-finite potential (their loads were dropped).
    pub non_finite: u64,
    /// Breaks decided and held back by the per-tick cap now.
    pub pending_breaks: u64,
}
