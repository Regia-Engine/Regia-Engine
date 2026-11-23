// SPDX-License-Identifier: MPL-2.0
//! The bindless step-2 census on the frame renderer (ADR-356 step 2a, `crate::texture_sets`).
//!
//! Every standard material entry records the key of the seven views its group binds when the
//! group is built. The resident census reads those keys on demand. The pass census (off by
//! default) walks each main-view frame's opaque list once after collection. Neither binds,
//! draws or reorders anything.

use super::{FrameRenderer, MaterialEntry};
use crate::texture_sets::{self, SetIdentity, TextureSetStats};

/// What `entry` (cached under `key`) binds in group 2.
fn set_identity(key: u64, entry: &MaterialEntry) -> SetIdentity {
    if entry.graph_key.is_some() {
        return SetIdentity::Own(key);
    }
    // A streamed material rebuilds its group on each residency swap: key its current views.
    match (entry.views.as_deref(), entry.texture_set) {
        (Some(views), _) => SetIdentity::Shared(texture_sets::texture_set_key(views)),
        (None, Some(set)) => SetIdentity::Shared(set),
        (None, None) => SetIdentity::Own(key),
    }
}

impl FrameRenderer {
    /// Turns the bindless step-2 pass census on or off (off by default; ADR-356).
    ///
    /// While on, each main-view frame walks its opaque draw list once after collection and
    /// counts the material-group changes it makes against those it would make with groups
    /// shared by texture set ([`Self::texture_set_stats`]). It binds and reorders nothing, so
    /// frames are byte-identical either way. A change of the switch restarts the counts.
    pub fn set_texture_set_census(&mut self, enabled: bool) {
        self.texture_set_census.set_enabled(enabled);
    }

    /// Whether the pass census runs ([`Self::set_texture_set_census`]).
    #[must_use]
    pub fn texture_set_census(&self) -> bool {
        self.texture_set_census.enabled()
    }

    /// The bindless step-2 census: texture sets among the resident materials (counted now)
    /// and, while the census is on, the last main-view frame's opaque list
    /// ([`TextureSetStats`]). Agents read it with `render.texture_sets`.
    #[must_use]
    pub fn texture_set_stats(&self) -> TextureSetStats {
        let resident = texture_sets::resident_census(
            self.materials
                .iter()
                .map(|(key, entry)| set_identity(*key, entry)),
        );
        self.texture_set_census.stats(resident)
    }

    /// Records one main-view frame's opaque list (`materials`: each draw's material key, in
    /// order). A draw whose material has no entry is skipped, as the draw loops skip it.
    pub(super) fn note_opaque_texture_sets(&mut self, materials: impl Iterator<Item = u64>) {
        let _span = tracing::info_span!("texture_set_census").entered();
        let pass = texture_sets::pass_census(materials.filter_map(|key| {
            self.materials
                .get(&key)
                .map(|entry| (key, set_identity(key, entry)))
        }));
        self.texture_set_census.record(pass);
    }
}
