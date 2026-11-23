// SPDX-License-Identifier: MPL-2.0
//! Fog of war presentation, CPU side (audit gap FW-1): the per-viewer fog texture.
//!
//! A [`FogTexture`] turns a viewer's [`FogGrid`] (see [`crate::fog_view::FogViewSim::with_grid`])
//! into one R8 texel per cell: [`FOG_UNEXPLORED`] (0), [`FOG_EXPLORED`] (128) or [`FOG_VISIBLE`]
//! (255). Cells do not pop: the shown value moves toward its target with an exponential fade whose
//! time constant is [`FogTexture::fade_s`], stepped by the frame's `dt`, so 30, 60 and 120 Hz reach
//! the same value at the same time (`exp(-a) * exp(-b) = exp(-(a + b))`). A value within half a
//! step of its target snaps to it, so a fade ends.
//!
//! Uploads are by row: a [`FogTexture::sync`] looks only at the grid's dirty rows
//! ([`FogGrid::dirty_rows`]) and the rows still fading, writes their texels into the staged buffer
//! ([`FogTexture::staged`]) and lists the rows whose bytes changed as [`FogRowRun`]s (contiguous
//! row ranges, [`FogTexture::upload_runs`]). A view that does not change uploads nothing. The GPU
//! overlay (`regia_render::fog_overlay`) and the minimap layer (`regia_ui`'s `RadarFog`) consume
//! the same staged buffer.
//!
//! [`FogPresentationPlugin`] wires it up for a [`FogViewSim`]: it syncs the texture every frame,
//! publishes [`FogTextureStats`] (the agent's `fog.texture_stats`) and, in client builds, feeds the
//! world overlay and the minimap fog image.
//!
//! The fade and the run coalescing are Regia Engine-original (`docs/provenance.md`).

use std::marker::PhantomData;

use regia_app::prelude::*;
use regia_detsim::fog::{CellState, FogGrid, FogWorld};

use crate::fog_view::FogViewSim;
use crate::sim_bridge::ExternalSim;

/// Texel of a cell the viewer has never seen.
pub const FOG_UNEXPLORED: u8 = 0;
/// Texel of a cell seen before but not now.
pub const FOG_EXPLORED: u8 = 128;
/// Texel of a cell in sight now.
pub const FOG_VISIBLE: u8 = 255;
/// Default fade time constant in seconds: a changed cell is 63 % of the way after this long and
/// within half a step of its target after about `fade_s * ln(510)` (6.2 time constants).
pub const DEFAULT_FOG_FADE_S: f32 = 0.12;

/// The texel of a cell state.
#[must_use]
pub fn fog_texel(state: CellState) -> u8 {
    match state {
        CellState::Unexplored => FOG_UNEXPLORED,
        CellState::Explored => FOG_EXPLORED,
        CellState::Visible => FOG_VISIBLE,
    }
}

/// Rows `first_row..first_row + rows` of the texture changed in the last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FogRowRun {
    /// First changed row.
    pub first_row: u32,
    /// Number of rows.
    pub rows: u32,
}

/// Counters of the last [`FogTexture::sync`], and running totals (`fog.texture_stats`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct FogTextureStats {
    /// Texture width (grid columns); 0 before the first sync.
    pub cols: u32,
    /// Texture height (grid rows).
    pub rows: u32,
    /// World X and Z of the texture's south-west corner (texel (0, 0)).
    pub origin: [f32; 2],
    /// Cell edge in metres.
    pub cell_m: f32,
    /// Grid rows that were dirty at the last sync.
    pub dirty_rows: u32,
    /// Rows still fading after the last sync.
    pub fading_rows: u32,
    /// Rows whose bytes changed at the last sync (the rows uploaded).
    pub rows_uploaded: u32,
    /// Bytes the last sync asked to upload (`rows_uploaded * cols`).
    pub upload_bytes: u64,
    /// Contiguous row runs of the last sync (one texture write each).
    pub runs: u32,
    /// Syncs so far.
    pub syncs: u64,
    /// Bytes asked to upload so far.
    pub total_upload_bytes: u64,
    /// Syncs that (re)built the whole texture (first sync, a grid of another size or place).
    pub full_uploads: u64,
}

/// One viewer's fog texture on the CPU: target texels, the faded shown values and the staged
/// bytes (see the module docs).
#[derive(Debug, Clone)]
pub struct FogTexture {
    cols: usize,
    rows: usize,
    origin: [f32; 2],
    cell_m: f32,
    fade_s: f32,
    shown: Vec<f32>,
    staged: Vec<u8>,
    /// One bit per row: the row has a cell whose shown value is not its target yet.
    fading: Vec<u64>,
    runs: Vec<FogRowRun>,
    stats: FogTextureStats,
}

impl Default for FogTexture {
    fn default() -> Self {
        Self::new()
    }
}

fn bit(bits: &[u64], row: usize) -> bool {
    bits.get(row / 64).is_some_and(|w| w & (1u64 << (row % 64)) != 0)
}

fn set_bit(bits: &mut [u64], row: usize, on: bool) {
    if let Some(w) = bits.get_mut(row / 64) {
        if on {
            *w |= 1u64 << (row % 64);
        } else {
            *w &= !(1u64 << (row % 64));
        }
    }
}

impl FogTexture {
    /// An empty texture with the [`DEFAULT_FOG_FADE_S`] fade; the first sync sizes it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cols: 0,
            rows: 0,
            origin: [0.0; 2],
            cell_m: 0.0,
            fade_s: DEFAULT_FOG_FADE_S,
            shown: Vec::new(),
            staged: Vec::new(),
            fading: Vec::new(),
            runs: Vec::new(),
            stats: FogTextureStats::default(),
        }
    }

    /// With a fade time constant of `fade_s` seconds (0 or less, or not finite: no fade).
    #[must_use]
    pub fn with_fade_s(mut self, fade_s: f32) -> Self {
        self.set_fade_s(fade_s);
        self
    }

    /// Sets the fade time constant (see [`Self::with_fade_s`]).
    pub fn set_fade_s(&mut self, fade_s: f32) {
        self.fade_s = if fade_s.is_finite() { fade_s.max(0.0) } else { 0.0 };
    }

    /// The fade time constant in seconds (0: no fade).
    #[must_use]
    pub fn fade_s(&self) -> f32 {
        self.fade_s
    }

    /// Texture width in texels (grid columns).
    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Texture height in texels (grid rows).
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// World X and Z of texel (0, 0)'s south-west corner.
    #[must_use]
    pub fn origin(&self) -> [f32; 2] {
        self.origin
    }

    /// Cell edge in metres.
    #[must_use]
    pub fn cell_m(&self) -> f32 {
        self.cell_m
    }

    /// The staged texels, row-major from the origin (row = +Z, column = +X), one byte each: what
    /// the GPU texture holds after the last sync's runs are uploaded.
    #[must_use]
    pub fn staged(&self) -> &[u8] {
        &self.staged
    }

    /// The staged bytes of one row (empty out of range).
    #[must_use]
    pub fn row_bytes(&self, row: usize) -> &[u8] {
        if row >= self.rows {
            return &[];
        }
        &self.staged[row * self.cols..(row + 1) * self.cols]
    }

    /// The shown (faded) value of cell `(col, row)`, 0–255; `None` out of range.
    #[must_use]
    pub fn shown(&self, col: usize, row: usize) -> Option<f32> {
        (col < self.cols && row < self.rows).then(|| self.shown[row * self.cols + col])
    }

    /// The rows the last sync changed, as contiguous runs (ascending).
    #[must_use]
    pub fn upload_runs(&self) -> &[FogRowRun] {
        &self.runs
    }

    /// Counters of the last sync.
    #[must_use]
    pub fn stats(&self) -> FogTextureStats {
        self.stats
    }

    /// True when a row still has a cell between texel values.
    #[must_use]
    pub fn is_fading(&self) -> bool {
        self.fading.iter().any(|w| *w != 0)
    }

    /// Brings the texture up to date with `grid` after `dt_s` seconds: reads the grid's dirty rows
    /// and the fading rows, steps their fade, stages their bytes, lists the changed rows in
    /// [`Self::upload_runs`] and clears the grid's dirty rows. A grid of another size or place
    /// rebuilds the texture without a fade (every row uploads). A negative or non-finite `dt_s`
    /// counts as 0.
    pub fn sync(&mut self, grid: &mut FogGrid, dt_s: f32) {
        let _span = tracing::trace_span!(target: "regia_framework", "fog_texture_sync").entered();
        self.runs.clear();
        let reshaped = grid.cols != self.cols
            || grid.rows != self.rows
            || grid.origin != self.origin
            || grid.cell_m != self.cell_m;
        let dirty_rows = grid.dirty_row_count() as u32;
        let mut uploaded = 0u32;
        if reshaped {
            self.cols = grid.cols;
            self.rows = grid.rows;
            self.origin = grid.origin;
            self.cell_m = grid.cell_m;
            let cells = self.cols * self.rows;
            self.staged.clear();
            self.staged.extend((0..cells).map(|i| {
                fog_texel(
                    grid.state(i % self.cols.max(1), i / self.cols.max(1))
                        .unwrap_or(CellState::Unexplored),
                )
            }));
            self.shown.clear();
            self.shown.extend(self.staged.iter().map(|&b| f32::from(b)));
            self.fading = vec![0; self.rows.div_ceil(64)];
            if self.rows > 0 {
                self.push_run(0, self.rows);
            }
            uploaded = self.rows as u32;
            self.stats.full_uploads += 1;
        } else {
            let dt = if dt_s.is_finite() { dt_s.max(0.0) } else { 0.0 };
            // Share of the remaining distance a cell covers in `dt`; 1 = snap.
            let k = if self.fade_s > 0.0 {
                1.0 - (-dt / self.fade_s).exp()
            } else {
                1.0
            };
            for row in 0..self.rows {
                if !grid.is_row_dirty(row) && !bit(&self.fading, row) {
                    continue;
                }
                let mut changed = false;
                let mut fading = false;
                for col in 0..self.cols {
                    let i = row * self.cols + col;
                    let target = f32::from(fog_texel(grid.state(col, row).unwrap_or(CellState::Unexplored)));
                    let mut shown = self.shown[i];
                    if shown != target {
                        shown += (target - shown) * k;
                        if (target - shown).abs() < 0.5 {
                            shown = target;
                        }
                        self.shown[i] = shown;
                    }
                    fading |= shown != target;
                    let byte = shown.round().clamp(0.0, 255.0) as u8;
                    if self.staged[i] != byte {
                        self.staged[i] = byte;
                        changed = true;
                    }
                }
                set_bit(&mut self.fading, row, fading);
                if changed {
                    uploaded += 1;
                    self.push_run(row, 1);
                }
            }
        }
        grid.clear_dirty_rows();
        let s = &mut self.stats;
        s.cols = self.cols as u32;
        s.rows = self.rows as u32;
        s.origin = self.origin;
        s.cell_m = self.cell_m;
        s.dirty_rows = dirty_rows;
        s.fading_rows = self.fading.iter().map(|w| w.count_ones()).sum();
        s.rows_uploaded = uploaded;
        s.upload_bytes = u64::from(uploaded) * self.cols as u64;
        s.runs = self.runs.len() as u32;
        s.syncs += 1;
        s.total_upload_bytes += s.upload_bytes;
    }

    fn push_run(&mut self, row: usize, rows: usize) {
        let (row, rows) = (row as u32, rows as u32);
        if let Some(last) = self.runs.last_mut()
            && last.first_row + last.rows == row
        {
            last.rows += rows;
            return;
        }
        self.runs.push(FogRowRun { first_row: row, rows });
    }
}

/// The current viewer's fog texture, synced every frame by [`FogPresentationPlugin`].
#[derive(Resource, Debug, Clone, Default)]
pub struct FogTextureState {
    /// The texture.
    pub texture: FogTexture,
}

/// What the editor's fog debug view and agents read: the watchers of the last filtered snapshot
/// and a copy of the staged texels. Filled only while [`FogDebugView::enabled`] is set (the
/// editor's Show ▸ Fog flag), so a game pays nothing for it.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct FogDebugView {
    /// Fill the view every frame.
    pub enabled: bool,
    /// The living own and allied watchers: `(position, sight radius)`.
    pub watchers: Vec<([f32; 3], f32)>,
    /// Texture width.
    pub cols: u32,
    /// Texture height.
    pub rows: u32,
    /// World X and Z of texel (0, 0)'s south-west corner.
    pub origin: [f32; 2],
    /// Cell edge in metres.
    pub cell_m: f32,
    /// The staged texels (row-major, one byte each).
    pub texels: Vec<u8>,
}

/// Syncs the fog texture of a [`FogViewSim<S>`] that has a grid ([`FogViewSim::with_grid`]) every
/// frame, publishes [`FogTextureStats`] and fills [`FogDebugView`] when asked. In client builds it
/// also feeds `regia_render`'s fog overlay (when a [`regia_render::FogOverlayHandle`] resource
/// exists) and every `regia_ui` minimap with a `RadarFog` layer.
pub struct FogPresentationPlugin<S>(PhantomData<fn() -> S>);

impl<S> Default for FogPresentationPlugin<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S: ExternalSim + Sync> Plugin for FogPresentationPlugin<S>
where
    S::Snapshot: FogWorld,
{
    fn build(&self, app: &mut App) {
        app.init_resource::<FogTextureState>()
            .init_resource::<FogTextureStats>()
            .init_resource::<FogDebugView>()
            .add_systems(PostUpdate, sync_fog_texture::<S>);
        #[cfg(feature = "client")]
        app.add_systems(PostUpdate, client::feed_fog_layers.after(sync_fog_texture::<S>));
    }

    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// `PostUpdate`: syncs the texture from the viewer's grid with the frame's `dt`.
pub fn sync_fog_texture<S: ExternalSim + Sync>(
    time: Option<Res<Time>>,
    sim: Option<ResMut<FogViewSim<S>>>,
    mut state: ResMut<FogTextureState>,
    mut stats: ResMut<FogTextureStats>,
    mut debug: ResMut<FogDebugView>,
) where
    S::Snapshot: FogWorld,
{
    let Some(mut sim) = sim else {
        return;
    };
    let dt = time.map_or(0.0, |t| t.delta_secs());
    let Some(grid) = sim.grid_mut() else {
        return;
    };
    state.texture.sync(grid, dt);
    *stats = state.texture.stats();
    if debug.enabled {
        let texture = &state.texture;
        debug.cols = texture.cols() as u32;
        debug.rows = texture.rows() as u32;
        debug.origin = texture.origin();
        debug.cell_m = texture.cell_m();
        debug.texels.clear();
        debug.texels.extend_from_slice(texture.staged());
        debug.watchers.clear();
        debug.watchers.extend(sim.watcher_circles());
    }
}

#[cfg(feature = "client")]
mod client {
    use regia_app::prelude::*;
    use regia_render::{FogLook, FogOverlayHandle};
    use regia_ui::game_widgets::radar::{RadarFog, radar_fog_rgba};

    use super::FogTextureState;

    /// Hands the synced texture's changed rows to the overlay and refreshes the minimap fog images.
    pub(super) fn feed_fog_layers(
        state: Res<FogTextureState>,
        look: Option<Res<FogLook>>,
        overlay: Option<Res<FogOverlayHandle>>,
        mut radars: Query<&mut RadarFog>,
        ui: Option<Res<regia_ui::render::SharedUiRenderer>>,
        mut images: Local<Vec<u64>>,
    ) {
        let texture = &state.texture;
        if texture.cols() == 0 {
            return;
        }
        let size = [
            texture.cols() as f32 * texture.cell_m(),
            texture.rows() as f32 * texture.cell_m(),
        ];
        if let Some(overlay) = overlay {
            if let Some(look) = look {
                overlay.set_look(*look);
            }
            overlay.set_grid(
                texture.cols() as u32,
                texture.rows() as u32,
                texture.origin(),
                size,
            );
            for run in texture.upload_runs() {
                let first = run.first_row as usize;
                let rows = run.rows as usize;
                let bytes = &texture.staged()[first * texture.cols()..(first + rows) * texture.cols()];
                overlay.write_rows(run.first_row, run.rows, bytes);
            }
        }
        let changed = !texture.upload_runs().is_empty();
        let rect = [
            texture.origin()[0],
            texture.origin()[1],
            texture.origin()[0] + size[0],
            texture.origin()[1] + size[1],
        ];
        let mut rgba: Option<std::sync::Arc<[u8]>> = None;
        for mut fog in &mut radars {
            if fog.world_rect != rect {
                fog.world_rect = rect;
            }
            let fresh = !images.contains(&fog.image);
            if !(changed || fresh) {
                continue;
            }
            let Some(ui) = ui.as_ref() else {
                continue;
            };
            let data = rgba
                .get_or_insert_with(|| {
                    radar_fog_rgba(texture.cols(), texture.rows(), texture.staged(), &fog.look).into()
                })
                .clone();
            ui.lock()
                .queue_dynamic_image(fog.image, texture.cols() as u32, texture.rows() as u32, data);
            if fresh {
                images.push(fog.image);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texels_of_states() {
        assert_eq!(fog_texel(CellState::Unexplored), 0);
        assert_eq!(fog_texel(CellState::Explored), 128);
        assert_eq!(fog_texel(CellState::Visible), 255);
    }

    #[test]
    fn runs_coalesce_adjacent_rows() {
        let mut t = FogTexture::new();
        t.push_run(2, 1);
        t.push_run(3, 1);
        t.push_run(5, 2);
        assert_eq!(
            t.upload_runs(),
            &[
                FogRowRun {
                    first_row: 2,
                    rows: 2
                },
                FogRowRun {
                    first_row: 5,
                    rows: 2
                }
            ]
        );
    }

    #[test]
    fn bad_fade_settings_mean_no_fade() {
        assert_eq!(FogTexture::new().with_fade_s(f32::NAN).fade_s(), 0.0);
        assert_eq!(FogTexture::new().with_fade_s(-1.0).fade_s(), 0.0);
        assert_eq!(FogTexture::new().with_fade_s(0.5).fade_s(), 0.5);
    }
}
