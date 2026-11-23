// SPDX-License-Identifier: MPL-2.0
//! Fog of war presentation, CPU side (audit gap FW-1): the fog texture uploads only dirty rows, its
//! staged bytes are the grid's states, and its fade does not depend on the frame rate.

use regia_detsim::fog::{CellState, FogEntity, FogGrid, FogRules, FogWorld, SightClass, TeamId, TeamVision};
use regia_detsim::los::OpenGround;
use regia_framework::fog_texture::{FOG_EXPLORED, FOG_UNEXPLORED, FOG_VISIBLE, FogTexture, fog_texel};

/// Scouts on an open field: `(id, team, position)`.
struct Field(Vec<(u64, TeamId, [f32; 3])>);

impl FogWorld for Field {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.0
            .iter()
            .map(|(id, team, at)| FogEntity::new(*id, "scout", SightClass::Unit, *team, *at))
    }
}

/// One scout of team 1 at `at`, the grid updated for team 1.
fn view(grid: &mut FogGrid, at: [f32; 3]) {
    let field = Field(vec![(1, 1, at), (2, 2, [500.0, 0.0, 500.0])]);
    let vision = TeamVision::new(&field, &FogRules::default(), 1);
    grid.update(&vision, &OpenGround);
}

fn uploaded_rows(texture: &FogTexture) -> Vec<usize> {
    texture
        .upload_runs()
        .iter()
        .flat_map(|run| run.first_row as usize..(run.first_row + run.rows) as usize)
        .collect()
}

#[test]
fn fog_texture_uploads_only_dirty_rows() {
    let mut grid = FogGrid::covering(640.0, 640.0);
    let mut texture = FogTexture::new().with_fade_s(0.0);
    view(&mut grid, [0.0, 0.0, 0.0]);
    texture.sync(&mut grid, 1.0 / 60.0);
    let stats = texture.stats();
    assert_eq!((stats.cols, stats.rows), (40, 40));
    assert_eq!(
        stats.rows_uploaded, 40,
        "the first sync uploads the whole texture"
    );
    assert_eq!(stats.upload_bytes, 40 * 40);
    assert_eq!(stats.full_uploads, 1);
    assert_eq!(grid.dirty_row_count(), 0, "a sync consumes the dirty rows");

    // Nothing moves: nothing uploads.
    view(&mut grid, [0.0, 0.0, 0.0]);
    texture.sync(&mut grid, 1.0 / 60.0);
    assert_eq!(texture.stats().rows_uploaded, 0);
    assert_eq!(texture.stats().upload_bytes, 0);
    assert!(texture.upload_runs().is_empty());

    // The scout moves north: exactly the grid's dirty rows upload (no fade).
    view(&mut grid, [0.0, 0.0, -200.0]);
    let dirty: Vec<usize> = grid.dirty_rows().collect();
    assert!(!dirty.is_empty() && dirty.len() < grid.rows, "{dirty:?}");
    texture.sync(&mut grid, 1.0 / 60.0);
    assert_eq!(uploaded_rows(&texture), dirty);
    assert_eq!(texture.stats().dirty_rows as usize, dirty.len());
    assert_eq!(texture.stats().upload_bytes, (dirty.len() * 40) as u64);
    assert_eq!(texture.stats().full_uploads, 1);

    // With a fade the changed rows keep uploading while they fade, then stop; no other row ever
    // uploads.
    texture.set_fade_s(0.1);
    view(&mut grid, [0.0, 0.0, 0.0]);
    let dirty: Vec<usize> = grid.dirty_rows().collect();
    let mut frames_with_uploads = 0;
    for _ in 0..120 {
        texture.sync(&mut grid, 1.0 / 60.0);
        let rows = uploaded_rows(&texture);
        assert!(
            rows.iter().all(|r| dirty.contains(r)),
            "{rows:?} not in {dirty:?}"
        );
        frames_with_uploads += usize::from(!rows.is_empty());
    }
    assert!(frames_with_uploads > 1, "the fade spreads over frames");
    assert!(!texture.is_fading());
    texture.sync(&mut grid, 1.0 / 60.0);
    assert_eq!(
        texture.stats().rows_uploaded,
        0,
        "a finished fade uploads nothing"
    );
    assert_eq!(texture.stats().full_uploads, 1);
}

#[test]
fn fog_texture_values_match_the_grid() {
    let mut grid = FogGrid::covering(640.0, 400.0);
    let mut texture = FogTexture::new().with_fade_s(0.0);
    view(&mut grid, [-300.0, 0.0, 0.0]);
    view(&mut grid, [200.0, 0.0, 100.0]);
    texture.sync(&mut grid, 0.016);
    let check = |grid: &FogGrid, texture: &FogTexture| {
        assert_eq!(texture.staged().len(), grid.cols * grid.rows);
        let mut seen = [0usize; 3];
        for row in 0..grid.rows {
            let bytes = texture.row_bytes(row);
            assert_eq!(bytes.len(), grid.cols);
            for (col, byte) in bytes.iter().enumerate() {
                let state = grid.state(col, row).unwrap_or(CellState::Unexplored);
                assert_eq!(*byte, fog_texel(state), "cell ({col}, {row})");
                assert_eq!(texture.staged()[row * grid.cols + col], *byte);
                seen[usize::from(*byte == FOG_EXPLORED) + 2 * usize::from(*byte == FOG_VISIBLE)] += 1;
            }
        }
        assert!(seen.iter().all(|n| *n > 0), "all three states present: {seen:?}");
    };
    check(&grid, &texture);
    assert_eq!(texture.row_bytes(grid.rows), &[] as &[u8]);
    assert_eq!(texture.origin(), grid.origin);
    assert_eq!(texture.cell_m(), grid.cell_m);
    // Fading textures land on the same bytes once the fade is over.
    let mut faded = FogTexture::new().with_fade_s(0.2);
    faded.sync(&mut grid.clone(), 0.016);
    view(&mut grid, [0.0, 0.0, -300.0]);
    let mut copy = grid.clone();
    texture.sync(&mut grid, 0.016);
    for _ in 0..200 {
        faded.sync(&mut copy, 0.016);
    }
    check(&grid, &texture);
    assert_eq!(faded.staged(), texture.staged());
    assert!(texture.staged().contains(&FOG_UNEXPLORED));
}

#[test]
fn fade_is_frame_rate_independent() {
    // Every rate starts from the same synced grid, then the scout jumps: the cells it left fade to
    // explored, the cells it reached fade to visible.
    let mut grid = FogGrid::covering(640.0, 640.0);
    view(&mut grid, [0.0, 0.0, 0.0]);
    let rates = [30u32, 60, 120];
    let mut runs: Vec<(FogTexture, FogGrid)> = rates
        .iter()
        .map(|_| {
            let mut g = grid.clone();
            let mut t = FogTexture::new().with_fade_s(0.1);
            t.sync(&mut g, 0.0);
            view(&mut g, [0.0, 0.0, -250.0]);
            (t, g)
        })
        .collect();
    let probe_visible = (20usize, 12usize);
    let probe_explored = (20usize, 20usize);
    assert_eq!(
        runs[0].1.state(probe_visible.0, probe_visible.1),
        Some(CellState::Visible)
    );
    assert_eq!(
        runs[0].1.state(probe_explored.0, probe_explored.1),
        Some(CellState::Explored)
    );
    // Compare at the times all three rates reach: every 1/30 s for 0.7 s (a 255 step settles after 0.1 * ln(510) = 0.62 s).
    let mut mid_fade = false;
    for tick in 1..=21u32 {
        let mut values = Vec::new();
        for (rate, (texture, g)) in rates.iter().zip(runs.iter_mut()) {
            let steps = rate / 30;
            for _ in 0..steps {
                texture.sync(g, 1.0 / *rate as f32);
            }
            values.push((
                texture.shown(probe_visible.0, probe_visible.1).unwrap_or(-1.0),
                texture.shown(probe_explored.0, probe_explored.1).unwrap_or(-1.0),
                texture.staged().to_vec(),
            ));
        }
        for v in &values[1..] {
            assert!(
                (v.0 - values[0].0).abs() <= 0.5,
                "t {tick}/30: {} vs {}",
                v.0,
                values[0].0
            );
            assert!(
                (v.1 - values[0].1).abs() <= 0.5,
                "t {tick}/30: {} vs {}",
                v.1,
                values[0].1
            );
            let worst =
                v.2.iter()
                    .zip(&values[0].2)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap_or(0);
            assert!(worst <= 1, "t {tick}/30: bytes differ by {worst}");
        }
        mid_fade |= values[0].0 > 10.0 && values[0].0 < 245.0;
        // The analytic value: target - (target - start) * exp(-t / tau).
        let t = tick as f32 / 30.0;
        let expected = 255.0 - 255.0 * (-t / 0.1f32).exp();
        if 255.0 - expected >= 0.6 {
            assert!(
                (values[0].0 - expected).abs() < 0.05,
                "t {t}: {} vs {expected}",
                values[0].0
            );
        }
    }
    assert!(mid_fade, "the probe was seen mid-fade");
    for (texture, _) in &runs {
        assert_eq!(texture.shown(probe_visible.0, probe_visible.1), Some(255.0));
        assert_eq!(texture.shown(probe_explored.0, probe_explored.1), Some(128.0));
        assert!(!texture.is_fading());
    }
}
