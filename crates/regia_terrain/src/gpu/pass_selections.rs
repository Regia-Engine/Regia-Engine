// SPDX-License-Identifier: MPL-2.0
//! The passes the terrain draw ran last frame, each with the selection it drew and its morph
//! centre (spec 26 § 2a.11, brief W-15 item 4, ADR-8470): `terrain.seam_report` reads a pass from
//! here instead of selecting again, so the report measures exactly what was drawn.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;

use crate::cdlod::{LodSelection, NodeDraw};

/// One pass the draw ran: `main` (the camera) or `cascade0`..`cascade3`.
#[derive(Clone, Debug, Default)]
pub struct DrawnPass {
    /// Pass name.
    pub pass: String,
    /// Morph centre (x, z) the pass's vertices morphed around.
    pub eye: [f32; 2],
    /// The selection the pass drew.
    pub selection: LodSelection,
    /// The mouth bands' vertices as this pass drew them ([`crate::MouthBands::place`] on this
    /// pass's selection and eye, ADR-8569); empty when no band is drawn.
    pub band: Vec<[f32; 3]>,
}

#[derive(Debug, Default)]
struct Shared {
    linked: bool,
    frame: u64,
    passes: Vec<DrawnPass>,
    /// The draw's counters, for [`TerrainDrawPasses::band_draws`] (ADR-8824).
    stats: Option<Arc<super::TerrainDrawStats>>,
}

/// A handle on the passes a [`crate::TerrainSharedDraw`] drew last frame
/// ([`crate::TerrainSharedDraw::draw_passes_handle`]). [`crate::TerrainPlugin`] inserts an unlinked
/// one; a host links its draw with `world.insert_resource(draw.draw_passes_handle())`, next to
/// the holes handle.
#[derive(Resource, Clone, Debug, Default)]
pub struct TerrainDrawPasses(Arc<Mutex<Shared>>);

impl TerrainDrawPasses {
    /// A handle a draw writes to (tools and tests that draw the terrain themselves publish to
    /// it with [`Self::publish`]).
    #[must_use]
    pub fn linked() -> Self {
        let handle = Self::default();
        handle.lock().linked = true;
        handle
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `true` once a draw owns this handle.
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.lock().linked
    }

    /// Frames published so far (0: none yet).
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.lock().frame
    }

    /// The names of the passes drawn last frame, in draw order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.lock().passes.iter().map(|p| p.pass.clone()).collect()
    }

    /// A copy of pass `name` as drawn last frame.
    #[must_use]
    pub fn pass(&self, name: &str) -> Option<DrawnPass> {
        self.lock().passes.iter().find(|p| p.pass == name).cloned()
    }

    /// Replaces the passes with this frame's `(name, morph eye, drawn nodes)`. Reuses the stored
    /// lists, so a steady frame allocates nothing.
    pub fn publish<'a>(&self, passes: impl IntoIterator<Item = (&'static str, [f32; 2], &'a [NodeDraw])>) {
        let mut shared = self.lock();
        let mut n = 0;
        for (name, eye, nodes) in passes {
            if shared.passes.len() <= n {
                shared.passes.push(DrawnPass::default());
            }
            let slot = &mut shared.passes[n];
            if slot.pass != name {
                slot.pass.clear();
                slot.pass.push_str(name);
                slot.band.clear();
            }
            slot.eye = eye;
            slot.selection.nodes.clear();
            slot.selection.nodes.extend_from_slice(nodes);
            n += 1;
        }
        shared.passes.truncate(n);
        shared.frame = shared.frame.wrapping_add(1);
    }
}

impl TerrainDrawPasses {
    /// Sets pass `name`'s band as drawn (ADR-8569); a pass not published this frame keeps none.
    pub fn set_band(&self, name: &str, band: &[[f32; 3]]) {
        let mut shared = self.lock();
        if let Some(pass) = shared.passes.iter_mut().find(|p| p.pass == name) {
            pass.band.clear();
            pass.band.extend_from_slice(band);
        }
    }

    /// Drops every pass's band (the draw stopped drawing bands).
    pub fn clear_bands(&self) {
        for pass in &mut self.lock().passes {
            pass.band.clear();
        }
    }

    /// Reads the band draw counters from `stats` ([`crate::TerrainSharedDraw::draw_passes_handle`]
    /// links its own, ADR-8824).
    pub fn link_stats(&self, stats: Arc<super::TerrainDrawStats>) {
        self.lock().stats = Some(stats);
    }

    /// The mouth-band draws of every terrain pass since the draw was made: the main camera's and
    /// the texture views' ([`super::TerrainDrawStats::band_draws`], ADR-8824). `None` until a draw
    /// links its counters.
    #[must_use]
    pub fn band_draws(&self) -> Option<(super::BandDrawCounts, super::BandDrawCounts)> {
        self.lock()
            .stats
            .as_ref()
            .map(|stats| (stats.band_draws(), stats.view_band_draws()))
    }
}
