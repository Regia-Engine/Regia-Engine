// SPDX-License-Identifier: MPL-2.0
//! Streamed levels' baked GI (ADR-1097, `book/agents/baked-gi-seams.md`).
//!
//! A level loaded on top of the open one (`LoadMode::Additive`: a streamed region, a sub-area)
//! may name its own `.fgiv` in its `LevelLook::baked_gi`. [`sync_streamed_baked_gi`] loads it
//! (the cooked asset or the file under `assets/`, as `baked_gi_level` does for the open level)
//! and blends it beside the level's own volume as a `regia_render::baked_gi::BakedGiLayer`
//! (`RuntimeRenderer::set_baked_gi_layers`). The renderer blends the volumes continuously across
//! their seams; this module fades each streamed volume in over
//! [`BakedGiStreamSettings::fade_seconds`] after its level loads and out after it unloads, so a
//! region streaming in or out never pops the light.
//!
//! The fade follows `RealTime` (a manual time step in tests and captures makes it exact), so it
//! runs while gameplay is paused. A missing or unreadable file sets the tile's error and code
//! (`volume_not_found`, `volume_unreadable`) and installs nothing for it; the other volumes keep
//! lighting the frame. A streamed level that names the open level's own volume adds nothing.
//! The open level's split-volume parts (ADR-2618, `baked_gi_level`) come first in the layers it
//! sets, and a streamed level's split volume joins with every part at the tile's weight; a part
//! that does not load leaves the others in the blend and is listed in
//! [`StreamedBakedGi::failed_parts`] (ADR-3006).
//!
//! **Host layers** (ADR-3006): the plugin keeps every layer it did not put there. They follow its
//! own in the list, while tiles stream and after, and a host may change them at any time; a
//! list the host replaced gets the plugin's layers back on the next update. A host that blends
//! layers of its own beside no streamed level is never touched.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::AssetServer;
use regia_asset::baked_gi::BakedGiVolume;
use regia_render::RuntimeRenderer;
use regia_render::baked_gi::BakedGiLayer;
use regia_scene::level::{LevelId, LevelLibrary, LevelManager, LevelState};

use crate::baked_gi_level::{
    FailedPart, LevelBakedGiState, Source, Stamp, load_set, relative_path, resolve_set, stamp_if_cached,
};

/// How streamed levels' volumes join the blend. Insert it to change the defaults.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct BakedGiStreamSettings {
    /// Seconds a streamed volume takes to fade in after its level loads, and out after it
    /// unloads (`RealTime`). 0 or less: at once.
    pub fade_seconds: f32,
    /// Blend distance of streamed volumes, meters (`BakedGiLayer::blend_distance`): 0 for tiles
    /// cut one probe column apart, the overlap for tiles baked with one.
    pub blend_distance: f32,
    /// Priority of streamed volumes; the open level's own volume has 0. Equal priorities blend
    /// by weight; a higher one lights its inside alone (a nested set-piece bake).
    pub priority: i32,
}

impl Default for BakedGiStreamSettings {
    fn default() -> Self {
        Self {
            fade_seconds: 1.0,
            blend_distance: 0.0,
            priority: 0,
        }
    }
}

/// One streamed level's volume.
#[derive(Clone, Debug)]
pub struct StreamedBakedGi {
    /// The streamed level.
    pub level: LevelId,
    /// The volume path as the level names it.
    pub volume: String,
    /// Its weight in the blend now, 0–1 (the fade).
    pub weight: f32,
    /// Its level is still loaded (`false`: fading out, removed at weight 0).
    pub loaded: bool,
    /// Why the volume could not be loaded; `None` when it loaded.
    pub error: Option<String>,
    /// A stable code for `error`: `volume_not_found` or `volume_unreadable`.
    pub error_code: Option<&'static str>,
    /// Probes in the loaded volume (0 when none loaded); every part of a split volume counts.
    pub probes: usize,
    /// The parts of a split volume that did not load; the others are in the blend.
    pub failed_parts: Vec<FailedPart>,
    stamp: Option<Vec<Stamp>>,
    /// Part sources from the last resolve. Empty until then.
    sources: Vec<Source>,
    /// `AssetServer::manifest_epoch` at that resolve.
    manifest_epoch: u64,
    data: Option<Arc<BakedGiVolume>>,
    /// Parts 1.. of a split volume (ADR-2618), blended at the tile's weight.
    parts: Vec<Arc<BakedGiVolume>>,
}

impl StreamedBakedGi {
    /// The volume is loaded and in the blend (at its current [`Self::weight`]).
    #[must_use]
    pub fn installed(&self) -> bool {
        self.data.is_some() && self.weight > 0.0
    }

    /// Records `failure` (none: clears the error), with one warning.
    fn note_failure(&mut self, failure: Option<crate::baked_gi_level::LoadError>) {
        let Some((code, error)) = failure else {
            self.error = None;
            self.error_code = None;
            return;
        };
        tracing::warn!(
            target: "regia_framework",
            "streamed level baked GI {}: {error}; the other volumes keep lighting the frame",
            self.volume
        );
        self.error = Some(error);
        self.error_code = Some(code);
    }
}

/// The streamed levels' volumes, for the agent protocol (`level.get_baked_gi_stream`) and host
/// UIs.
#[derive(Resource, Default)]
pub struct BakedGiStreamState {
    /// Every streamed volume: loaded, fading in, or fading out after its level unloaded.
    pub tiles: Vec<StreamedBakedGi>,
    /// The tile volumes last handed to the renderer: these layers are the plugin's, the ones not
    /// listed (and not the level's parts) a host's.
    pushed: Vec<Arc<BakedGiVolume>>,
}

impl BakedGiStreamState {
    /// `true` when `volume` is a streamed volume this plugin handed to the renderer.
    #[must_use]
    pub fn holds_volume(&self, volume: &Arc<BakedGiVolume>) -> bool {
        self.pushed.iter().any(|p| Arc::ptr_eq(p, volume))
    }
}

/// The two lists hold the same layers in the same order (volume identity and blend values).
fn same_layers(a: &[BakedGiLayer], b: &[BakedGiLayer]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            Arc::ptr_eq(&x.volume, &y.volume)
                && x.weight.to_bits() == y.weight.to_bits()
                && x.priority == y.priority
                && x.blend_distance.to_bits() == y.blend_distance.to_bits()
        })
}

/// Registers the streamed-volume system after `sync_level_baked_gi` (`BakedGiLevelPlugin`).
pub(crate) fn add_to(app: &mut App) {
    app.init_resource::<BakedGiStreamState>()
        .init_resource::<BakedGiStreamSettings>()
        .add_systems(
            Update,
            sync_streamed_baked_gi.after(crate::baked_gi_level::sync_level_baked_gi),
        );
}

/// `Update`: loads, fades and removes streamed levels' volumes and hands them to the renderer.
/// Does nothing without a `RuntimeRenderer`.
pub fn sync_streamed_baked_gi(
    (manager, library): (Option<Res<LevelManager>>, Option<Res<LevelLibrary>>),
    (settings, real): (Option<Res<BakedGiStreamSettings>>, Option<Res<RealTime>>),
    primary: Option<Res<LevelBakedGiState>>,
    mut assets: Option<ResMut<AssetServer>>,
    runtime: Option<ResMut<RuntimeRenderer>>,
    mut state: ResMut<BakedGiStreamState>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };
    let settings = settings.as_deref().copied().unwrap_or_default();
    let wanted = streamed_levels(
        manager.as_deref(),
        library.as_deref(),
        primary.as_ref().and_then(|p| p.volume.as_deref()),
    );
    let dt = real.as_ref().map_or(f32::INFINITY, |r| r.delta().as_secs_f32());
    state.reconcile(&wanted, assets.as_deref_mut());
    state.fade(dt, settings.fade_seconds);
    let tiles = state.layers(&settings);
    let parts = primary.as_ref().map_or_else(Vec::new, |p| p.part_layers());
    // Nothing of ours in the list now or before: it is the host's, untouched.
    if tiles.is_empty() && parts.is_empty() && state.pushed.is_empty() {
        return;
    }
    let current = runtime.0.baked_gi_layers();
    let is_ours = |volume: &Arc<BakedGiVolume>| {
        state.holds_volume(volume) || primary.as_ref().is_some_and(|p| p.holds_part(volume))
    };
    let mut layers = parts;
    layers.extend(tiles.iter().cloned());
    layers.extend(current.iter().filter(|l| !is_ours(&l.volume)).cloned());
    if !same_layers(&layers, current) {
        runtime.0.set_baked_gi_layers(&layers);
    }
    state.pushed = tiles.into_iter().map(|l| l.volume).collect();
}

/// The streamed levels (loaded, active, not the current one) that name a volume other than the
/// open level's, ordered by level id (a stable blend order).
fn streamed_levels(
    manager: Option<&LevelManager>,
    library: Option<&LevelLibrary>,
    primary: Option<&str>,
) -> Vec<(LevelId, String)> {
    let (Some(manager), Some(library)) = (manager, library) else {
        return Vec::new();
    };
    let primary = primary.map(relative_path);
    let current = manager.current();
    let mut out: Vec<(LevelId, String)> = manager
        .levels()
        .filter(|(id, state)| *state == LevelState::Active && Some(*id) != current)
        .filter_map(|(id, _)| {
            let level = library.get(id)?;
            let path = level.settings.look.baked_gi.as_ref()?.volume.trim().to_string();
            let same_as_primary = primary.as_deref() == Some(relative_path(&path).as_str());
            (!path.is_empty() && !same_as_primary).then_some((id, path))
        })
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

impl BakedGiStreamState {
    /// Marks tiles whose level unloaded (or names another path now) as fading out, adds new
    /// ones, and (re)loads every loaded tile whose file changed.
    fn reconcile(&mut self, wanted: &[(LevelId, String)], mut assets: Option<&mut AssetServer>) {
        for tile in &mut self.tiles {
            tile.loaded = tile.loaded
                && wanted
                    .iter()
                    .any(|(id, path)| *id == tile.level && *path == tile.volume);
        }
        for (id, path) in wanted {
            // A region that streams back in while it fades out picks up from its weight.
            if let Some(t) = self
                .tiles
                .iter_mut()
                .find(|t| t.level == *id && t.volume == *path)
            {
                t.loaded = true;
            } else {
                self.tiles.push(StreamedBakedGi {
                    level: *id,
                    volume: path.clone(),
                    weight: 0.0,
                    loaded: true,
                    error: None,
                    error_code: None,
                    probes: 0,
                    failed_parts: Vec::new(),
                    stamp: None,
                    sources: Vec::new(),
                    manifest_epoch: 0,
                    data: None,
                    parts: Vec::new(),
                });
            }
        }
        for tile in self.tiles.iter_mut().filter(|t| t.loaded) {
            if let Some(stamp) = stamp_if_cached(
                &tile.volume,
                &tile.sources,
                tile.manifest_epoch,
                assets.as_deref(),
            ) && tile.stamp.as_ref() == Some(&stamp)
            {
                continue;
            }
            let resolved = resolve_set(&tile.volume, assets.as_deref_mut());
            tile.sources = resolved.sources;
            tile.manifest_epoch = resolved.epoch;
            if tile.stamp.as_ref() == Some(&resolved.stamps) {
                continue;
            }
            tile.stamp = Some(resolved.stamps);
            let source = resolved.source;
            match load_set(&tile.volume, &source, assets.as_deref_mut()) {
                Ok(set) => {
                    let failure = set.error();
                    tile.failed_parts = set.failed;
                    // A reload of the same path never loses lit parts, as the open level's.
                    let lit = usize::from(tile.data.is_some()) + tile.parts.len();
                    if set.volumes.len() >= lit {
                        let mut volumes = set.volumes.into_iter().map(Arc::new);
                        tile.data = volumes.next();
                        tile.parts = volumes.collect();
                        tile.probes = tile
                            .data
                            .iter()
                            .chain(tile.parts.iter())
                            .map(|v| v.probe_count())
                            .sum();
                    }
                    tile.note_failure(failure);
                }
                // A failed reload keeps the volume that was loaded (as the open level does).
                Err(failure) => {
                    tile.failed_parts.clear();
                    tile.note_failure(Some(failure));
                }
            }
        }
    }

    /// Moves every tile's weight toward its target (1 while its level is loaded and its volume
    /// is, else 0) by `dt / fade_seconds`, and drops tiles that faded out.
    fn fade(&mut self, dt: f32, fade_seconds: f32) {
        let step = if fade_seconds > 0.0 && dt.is_finite() {
            (dt.max(0.0) / fade_seconds).min(1.0)
        } else {
            1.0
        };
        for tile in &mut self.tiles {
            let target = if tile.loaded && tile.data.is_some() {
                1.0
            } else {
                0.0
            };
            tile.weight = if tile.weight < target {
                (tile.weight + step).min(target)
            } else {
                (tile.weight - step).max(target)
            };
        }
        self.tiles.retain(|t| t.loaded || t.weight > 0.0);
    }

    /// The renderer layers of the tiles in the blend.
    fn layers(&self, settings: &BakedGiStreamSettings) -> Vec<BakedGiLayer> {
        self.tiles
            .iter()
            .filter(|t| t.weight > 0.0)
            .flat_map(|t| {
                t.data
                    .iter()
                    .chain(t.parts.iter().filter(|_| t.data.is_some()))
                    .map(|volume| BakedGiLayer {
                        volume: volume.clone(),
                        priority: settings.priority,
                        blend_distance: settings.blend_distance.max(0.0),
                        weight: t.weight,
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer, VolumeLayout,
    };
    use regia_asset::{AssetServer, AssetServerSettings};
    use regia_core::id::Guid;
    use regia_scene::level::{Level, LoadMode, open_level, unload_level};
    use regia_scene::prelude::ScenePlugin;
    use regia_test_utils::unique_temp_dir;

    use crate::baked_gi_level::BakedGiLevelPlugin;

    /// A 4 × 4-column terrain volume from `x0` (4 m columns, one level at 1 m) lit `sky`.
    fn tile_volume(x0: f32, sky: f32) -> BakedGiVolume {
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [x0, 0.0],
                spacing: [4.0, 4.0],
                counts: [4, 4],
                levels: vec![1.0, 4.0],
                ground: vec![0.0; 16],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let k = 4.0 * std::f32::consts::PI * regia_asset::baked_gi::SH_Y00;
        let lit = [[sky * k, 0.0, 0.0, 0.0]; 3];
        let dark = [[0.0f32; 4]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [dark, lit, dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("tile volume")
    }

    fn write(root: &Path, name: &str, volume: &BakedGiVolume) {
        let path = root.join("assets").join("maps").join(name);
        regia_test_utils::write_atomic(&path, &volume.to_bytes().expect("encode")).expect("write");
    }

    fn level_ron(volume: Option<&str>) -> String {
        let look = volume.map_or(String::new(), |v| {
            format!(r#""regia_scene::level::LevelSettings": (look: (baked_gi: Some((volume: "{v}")))),"#)
        });
        format!("SceneFile(format_version: 10, entities: [], resources: {{ {look} }})")
    }

    struct Rig {
        app: App,
        root: PathBuf,
        map: LevelId,
    }

    /// An app with the level plugin, a renderer, a 0.25 s manual time step and a map level
    /// naming `maps/a.fgiv` (written, lit 1) and opened.
    fn rig(tag: &str, fade_seconds: f32) -> Rig {
        let root = unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets").join("maps")).expect("assets dir");
        write(&root, "a.fgiv", &tile_volume(0.0, 1.0));
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open asset server");
        let mut app = App::new();
        app.add_plugins((ScenePlugin, BakedGiLevelPlugin));
        app.insert_resource(server);
        app.insert_resource(RuntimeRenderer::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(250)));
        app.insert_resource(BakedGiStreamSettings {
            fade_seconds,
            ..BakedGiStreamSettings::default()
        });
        let map = add_level(&mut app, Some("maps/a.fgiv"));
        open_level(app.world_mut(), map, LoadMode::Replace).expect("open map");
        app.update();
        Rig { app, root, map }
    }

    fn add_level(app: &mut App, volume: Option<&str>) -> LevelId {
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, &level_ron(volume)).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        id
    }

    fn layers(app: &App) -> Vec<BakedGiLayer> {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_layers()
            .to_vec()
    }

    fn weights(app: &App) -> Vec<f32> {
        layers(app).iter().map(|l| l.weight).collect()
    }

    /// Once a streamed volume is installed, a later update does not look its path up again.
    #[test]
    fn an_unchanged_streamed_volume_does_not_look_up_its_path() {
        let mut rig = rig("gi-stream-lookup-once", 0.0);
        write(&rig.root, "b.fgiv", &tile_volume(16.0, 3.0));
        let region = add_level(&mut rig.app, Some("maps/b.fgiv"));
        open_level(rig.app.world_mut(), region, LoadMode::Additive).expect("stream in");
        rig.app.update();
        assert!(
            rig.app.world().resource::<BakedGiStreamState>().tiles[0].installed(),
            "the streamed volume loaded"
        );
        let before = rig.app.world().resource::<AssetServer>().path_lookup_count();
        rig.app.update();
        let after = rig.app.world().resource::<AssetServer>().path_lookup_count();
        assert_eq!(
            after, before,
            "an unchanged streamed volume must not call id_for_path"
        );
    }

    /// A streamed level's volume fades in over the fade time in fixed steps, stays beside the
    /// map's own, and fades out and goes after its level unloads; the map's volume and settings
    /// never move.
    #[test]
    fn a_streamed_levels_volume_fades_in_beside_the_maps_and_out_after_unload() {
        let mut rig = rig("gi-stream-fade", 1.0);
        write(&rig.root, "b.fgiv", &tile_volume(16.0, 3.0));
        let app = &mut rig.app;
        let primary = app.world().resource::<RuntimeRenderer>().0.baked_gi().cloned();
        assert!(primary.is_some(), "the map's own volume is installed");
        assert!(layers(app).is_empty());

        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        let mut seen = Vec::new();
        for _ in 0..5 {
            app.update();
            seen.push(weights(app));
        }
        // 0.25 s steps over a 1 s fade: a quarter per frame, then full and steady.
        assert_eq!(seen, [vec![0.25], vec![0.5], vec![0.75], vec![1.0], vec![1.0]]);
        let state = app.world().resource::<BakedGiStreamState>();
        assert_eq!(state.tiles.len(), 1);
        assert!(state.tiles[0].installed() && state.tiles[0].loaded);
        assert_eq!(state.tiles[0].probes, 32);
        assert_eq!(state.tiles[0].level, region);
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert!(
            renderer
                .baked_gi()
                .cloned()
                .zip(primary.clone())
                .is_some_and(|(a, b)| Arc::ptr_eq(&a, &b))
        );
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(rig.map));
        assert!(app.world().resource::<LevelBakedGiState>().installed);

        unload_level(app.world_mut(), region).expect("stream out");
        let mut seen = Vec::new();
        for _ in 0..5 {
            app.update();
            seen.push(weights(app));
        }
        assert_eq!(seen, [vec![0.75], vec![0.5], vec![0.25], vec![], vec![]]);
        assert!(app.world().resource::<BakedGiStreamState>().tiles.is_empty());
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert!(
            renderer
                .baked_gi()
                .cloned()
                .zip(primary)
                .is_some_and(|(a, b)| Arc::ptr_eq(&a, &b))
        );
    }

    /// The blended light at a point inside the streamed tile moves in small steps while it
    /// fades in: no pop when a region streams in.
    #[test]
    fn the_light_moves_without_a_pop_while_a_region_streams_in() {
        let mut rig = rig("gi-stream-pop", 1.0);
        // The region overlaps the map's tile (x = 8..20 vs 0..12) and is three times as bright.
        write(&rig.root, "b.fgiv", &tile_volume(8.0, 3.0));
        let app = &mut rig.app;
        let sample = |app: &App| {
            app.world().resource::<RuntimeRenderer>().0.sample_baked_gi(
                [10.0, 1.5, 6.0],
                [0.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            )
        };
        let before = sample(app);
        assert!((before.irradiance_over_pi[0] - 1.0).abs() < 0.02, "{before:?}");
        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        let mut prev = before.irradiance_over_pi[0];
        for _ in 0..4 {
            app.update();
            let e = sample(app).irradiance_over_pi[0];
            assert!(
                e > prev,
                "the light rises while the region fades in: {prev} → {e}"
            );
            // At most 0.4 of the full 1.0 change in one frame (the first quarter of the fade).
            assert!(e - prev < 0.45, "no pop: {prev} → {e}");
            prev = e;
        }
        assert!((prev - 2.0).abs() < 0.05, "an even blend at full weight: {prev}");
    }

    /// No fade time: the volume joins at once. A missing file is an error on its tile, never a
    /// panic, and appears in the blend once it is written.
    #[test]
    fn a_missing_streamed_volume_is_reported_and_picked_up_when_written() {
        let mut rig = rig("gi-stream-missing", 0.0);
        let app = &mut rig.app;
        let region = add_level(app, Some("maps/later.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        app.update();
        let state = app.world().resource::<BakedGiStreamState>();
        assert_eq!(state.tiles.len(), 1);
        assert_eq!(state.tiles[0].error_code, Some("volume_not_found"));
        assert!(!state.tiles[0].installed());
        assert!(layers(app).is_empty());
        assert!(
            app.world().resource::<LevelBakedGiState>().installed,
            "the map keeps its own"
        );

        write(&rig.root, "later.fgiv", &tile_volume(16.0, 2.0));
        app.update();
        assert_eq!(weights(app), [1.0], "no fade time: at once");
        assert_eq!(app.world().resource::<BakedGiStreamState>().tiles[0].error, None);
    }

    /// A streamed level that names the map's own volume, or none, adds nothing; a host's layers
    /// stay while nothing streams.
    #[test]
    fn levels_without_a_bake_or_with_the_maps_add_nothing_and_host_layers_stay() {
        let mut rig = rig("gi-stream-none", 1.0);
        let app = &mut rig.app;
        let host = Arc::new(tile_volume(40.0, 1.0));
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(&[BakedGiLayer::new(host.clone())]);
        for volume in [None, Some("assets/maps/a.fgiv")] {
            let region = add_level(app, volume);
            open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
            app.update();
            app.update();
        }
        assert!(app.world().resource::<BakedGiStreamState>().tiles.is_empty());
        let kept = layers(app);
        assert_eq!(kept.len(), 1);
        assert!(Arc::ptr_eq(&kept[0].volume, &host));
    }

    /// A host's layers stay while a streamed level fades in beside them and after it fades out
    /// (the stream used to replace them while it faded and clear them when it had gone), and the
    /// host can change them meanwhile.
    #[test]
    fn host_layers_survive_a_streamed_level_in_and_out() {
        let mut rig = rig("gi-stream-host", 0.5);
        write(&rig.root, "b.fgiv", &tile_volume(16.0, 3.0));
        let app = &mut rig.app;
        let host = Arc::new(tile_volume(40.0, 1.0));
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(&[BakedGiLayer::new(host.clone())]);
        let holds = |app: &App, v: &Arc<BakedGiVolume>| layers(app).iter().any(|l| Arc::ptr_eq(&l.volume, v));

        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        app.update();
        assert!(
            holds(app, &host),
            "the host's layer stays while the tile fades in"
        );
        assert_eq!(layers(app).len(), 2);
        assert_eq!(weights(app), [0.5, 1.0], "the tile first, at its fade weight");

        // The host swaps its layer while the tile is in the blend.
        let second = Arc::new(tile_volume(60.0, 1.0));
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(&[BakedGiLayer::new(second.clone())]);
        app.update();
        assert!(holds(app, &second));
        assert!(!holds(app, &host), "the host's own call replaced its own layer");
        assert_eq!(weights(app), [1.0, 1.0], "the tile came back, fully faded in");

        unload_level(app.world_mut(), region).expect("stream out");
        for _ in 0..3 {
            app.update();
        }
        assert!(app.world().resource::<BakedGiStreamState>().tiles.is_empty());
        let kept = layers(app);
        assert_eq!(kept.len(), 1, "only the host's layer is left");
        assert!(Arc::ptr_eq(&kept[0].volume, &second));
    }

    /// A streamed split volume with a part missing blends the part it has and reports the other.
    #[test]
    fn a_streamed_split_volume_with_a_missing_part_blends_the_rest() {
        let mut rig = rig("gi-stream-split-lost", 0.0);
        let parts = write_split(&rig.root, "b", 200.0, 3.0, 24);
        std::fs::remove_file(rig.root.join("assets").join("maps").join("b.part1.fgiv")).expect("remove");
        let app = &mut rig.app;
        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        app.update();
        let tile = &app.world().resource::<BakedGiStreamState>().tiles[0];
        assert!(tile.installed());
        assert_eq!(tile.error_code, Some("volume_not_found"));
        assert_eq!(tile.failed_parts.len(), 1);
        assert_eq!(tile.failed_parts[0].index, 1);
        assert_eq!(tile.probes, parts[0].probe_count());
        assert_eq!(layers(app).len(), 1);
    }

    /// A streamed region whose split crops a brick at the cut (ADR-3237, format version 5) blends
    /// every part at the tile's weight.
    #[test]
    fn a_streamed_split_with_cropped_bricks_blends_every_part() {
        let mut rig = rig("gi-stream-split-cropped", 0.0);
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [200.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [24, 4],
                levels: vec![1.0],
                ground: vec![0.0; 96],
            }),
            bricks: vec![regia_asset::baked_gi::DetailBrick {
                origin: [202.0, 0.0, 2.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [85, 6, 6],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let k = 4.0 * std::f32::consts::PI * regia_asset::baked_gi::SH_Y00;
        let lit = [[3.0 * k, 0.0, 0.0, 0.0]; 3];
        let dark = [[0.0f32; 4]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [dark, lit, dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("region volume");
        let parts = whole
            .split_parts(layout.bricks[0].probe_count() as u64 * 136 * 7 / 10)
            .expect("the brick is cropped at the cut");
        assert!(parts.iter().any(|p| {
            p.split
                .as_ref()
                .is_some_and(|s| s.cropped_bricks(&p.layout()) > 0)
        }));
        regia_asset::baked_gi_split::write_split_set(
            &parts,
            &rig.root.join("assets").join("maps").join("b.fgiv"),
        )
        .expect("write parts");
        let app = &mut rig.app;
        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        app.update();
        let tile = &app.world().resource::<BakedGiStreamState>().tiles[0];
        assert!(tile.installed());
        assert_eq!(tile.error_code, None);
        assert_eq!(
            tile.probes,
            parts.iter().map(BakedGiVolume::probe_count).sum::<usize>()
        );
        assert_eq!(layers(app).len(), parts.len());
    }

    #[test]
    fn the_fade_moves_toward_its_target_and_drops_faded_tiles() {
        let mut state = BakedGiStreamState::default();
        let tile = |loaded: bool, weight: f32, data: bool| StreamedBakedGi {
            level: LevelId(Guid::new_random()),
            volume: "maps/x.fgiv".into(),
            weight,
            loaded,
            error: None,
            error_code: None,
            probes: 0,
            failed_parts: Vec::new(),
            stamp: None,
            sources: Vec::new(),
            manifest_epoch: 0,
            data: data.then(|| Arc::new(tile_volume(0.0, 1.0))),
            parts: Vec::new(),
        };
        state.tiles = vec![
            tile(true, 0.0, true),
            tile(false, 0.3, true),
            tile(true, 0.5, false),
        ];
        state.fade(0.1, 0.5);
        let w: Vec<f32> = state.tiles.iter().map(|t| t.weight).collect();
        assert_eq!(w.len(), 3);
        assert!(
            (w[0] - 0.2).abs() < 1e-6 && (w[1] - 0.1).abs() < 1e-6 && (w[2] - 0.3).abs() < 1e-6,
            "{w:?}"
        );
        state.fade(0.1, 0.5);
        assert_eq!(state.tiles.len(), 2, "the unloaded tile faded out and went");
        // Without a clock (no `RealTime`) or a fade time, at once.
        state.fade(f32::INFINITY, 0.5);
        assert_eq!(
            state.tiles.iter().map(|t| t.weight).collect::<Vec<_>>(),
            [1.0, 0.0]
        );
        let layers = state.layers(&BakedGiStreamSettings {
            blend_distance: -2.0,
            priority: 3,
            ..BakedGiStreamSettings::default()
        });
        assert_eq!(layers.len(), 1);
        assert_eq!((layers[0].priority, layers[0].blend_distance), (3, 0.0));
    }

    /// A wide terrain volume from `x0` (`columns` × 4 columns of 4 m) lit `sky`, split at 70 %
    /// of its runtime bytes and written as `maps/<stem>.fgiv` plus its part files (ADR-2618).
    fn write_split(root: &Path, stem: &str, x0: f32, sky: f32, columns: u32) -> Vec<BakedGiVolume> {
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [x0, 0.0],
                spacing: [4.0, 4.0],
                counts: [columns, 4],
                levels: vec![1.0],
                ground: vec![0.0; columns as usize * 4],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let k = 4.0 * std::f32::consts::PI * regia_asset::baked_gi::SH_Y00;
        let lit = [[sky * k, 0.0, 0.0, 0.0]; 3];
        let dark = [[0.0f32; 4]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [dark, lit, dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("wide volume");
        let parts = whole
            .split_parts(layout.runtime_buffer_bytes() * 7 / 10)
            .expect("the volume splits");
        assert!(parts.len() >= 2);
        regia_asset::baked_gi_split::write_split_set(
            &parts,
            &root.join("assets").join("maps").join(format!("{stem}.fgiv")),
        )
        .expect("write parts");
        parts
    }

    /// The map's split parts stay first in the layers while a streamed split region fades in
    /// with every part at its weight, and stay after it fades out (ADR-2618).
    #[test]
    fn split_parts_of_the_map_and_of_a_streamed_region_share_the_layers() {
        let mut rig = rig("gi-stream-split", 0.5);
        let map_parts = write_split(&rig.root, "a", 0.0, 1.0, 24);
        let region_parts = write_split(&rig.root, "b", 200.0, 3.0, 24);
        let app = &mut rig.app;
        // The map's file changed under the open level: it reloads as a split set.
        app.update();
        let split_of = |l: &BakedGiLayer| l.volume.split.as_ref().map(|s| (s.group, s.index));
        let part_ids = |parts: &[BakedGiVolume]| -> Vec<_> {
            parts
                .iter()
                .map(|p| p.split.as_ref().map(|s| (s.group, s.index)))
                .collect()
        };
        let map_layers = part_ids(&map_parts[1..]);
        assert_eq!(layers(app).iter().map(split_of).collect::<Vec<_>>(), map_layers);

        let region = add_level(app, Some("maps/b.fgiv"));
        open_level(app.world_mut(), region, LoadMode::Additive).expect("stream in");
        app.update();
        let got = layers(app);
        let mut want = map_layers.clone();
        want.extend(part_ids(&region_parts));
        assert_eq!(got.iter().map(split_of).collect::<Vec<_>>(), want);
        let n_map = map_layers.len();
        assert!(got[..n_map].iter().all(|l| l.weight == 1.0));
        assert!(got[n_map..].iter().all(|l| l.weight == 0.5), "{:?}", weights(app));
        let tile = &app.world().resource::<BakedGiStreamState>().tiles[0];
        assert_eq!(
            tile.probes,
            region_parts.iter().map(BakedGiVolume::probe_count).sum::<usize>()
        );

        unload_level(app.world_mut(), region).expect("stream out");
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(layers(app).iter().map(split_of).collect::<Vec<_>>(), map_layers);
        assert!(app.world().resource::<BakedGiStreamState>().tiles.is_empty());
    }
}
