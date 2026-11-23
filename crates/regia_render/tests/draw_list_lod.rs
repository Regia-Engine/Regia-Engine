// SPDX-License-Identifier: MPL-2.0
//! LOD, cross-fade, object keys and shadow policy of keyed draws (plan row P-04
//! drawlist-lod-keys, ADR-8457).
//!
//! - `drawlist_item_crossfades_like_an_entity`: one chained mesh drawn as an entity and as a keyed
//!   draw at the same transform, under a scripted camera dolly out and back, gives the same LOD
//!   index, cross-fade coverage, shadow LOD and mesh-cache key on every frame.
//! - `drawlist_lod_memory_is_per_key`: hysteresis memory belongs to one key: a fresh key inside
//!   the hysteresis band picks its own level while an older key keeps its level, and a key whose
//!   number equals a chained entity's bits shares nothing with that entity.
//! - `shadow_cast_policy_component_reaches_extraction`: the ECS `MeshShadowCast` component (the
//!   row's `ShadowCastPolicy`) reaches the extracted frame for a `MeshRenderer` entity, through a
//!   LOD chain's separate shadow draw too; a keyed draw's `with_shadow` does the same.
//! - `a_keyed_draw_keeps_its_object_id_across_frames`: a keyed draw re-submitted every frame
//!   (sync passes, a new `KeyedDraw` value each time) keeps its object key in the frame scene
//!   across LOD switches and cross-fades; the fade's outgoing draw and the shadow LOD draw get the
//!   entity convention's tags, never another key's id.
//!
//! All CPU extraction tests: no device, no golden.

use std::collections::BTreeSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DrawKey, DrawList, ExtractedFrame, KeyedDraw, LabVertex, LodViewport,
    MESH_LOD_FADE_OBJECT_TAG, MESH_LOD_SHADOW_OBJECT_TAG, MeshLodChain, MeshLodLevel, MeshLodScales,
    MeshRenderer, MeshShadowCast, NotShadowCaster, ShadowCast, ShadowLodHint, extract_frame,
    select_mesh_lods,
};
use regia_scene::{GlobalTransform, Transform};

const FOV_Y: f32 = 0.9;
/// Screen-size thresholds of the three-level test chain (LOD 0, 1, 2) and its triangle counts.
const LEVELS: [(u32, f32); 3] = [(3, 0.4), (2, 0.1), (1, 0.0)];

fn place(pos: Vec3) -> (Transform, GlobalTransform) {
    let transform = Transform::from_translation(pos);
    (transform, GlobalTransform(transform.compute_affine()))
}

fn mesh() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0],); 3],
        indices: vec![0, 1, 2, 0, 1, 2],
    })
}

/// A chain whose level `i` has `LEVELS[i].0` triangles, so a draw's index count names its level.
fn chain() -> MeshLodChain {
    MeshLodChain {
        levels: LEVELS
            .iter()
            .map(|(tris, screen)| MeshLodLevel {
                indices: Arc::new(vec![0; (*tris * 3) as usize]),
                screen_size: *screen,
            })
            .collect(),
        bounds_center: [0.0; 3],
        bounds_radius: 1.0,
    }
}

/// An app with extraction and LOD selection, an empty `DrawList` and a window camera on +Z
/// looking down −Z at the origin. No `RenderSettings`: the LOD scales are the neutral defaults.
fn app() -> (App, Entity) {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<DrawList>();
    app.insert_resource(LodViewport {
        width: 1920,
        height: 1080,
    });
    app.add_systems(Render, (extract_frame, select_mesh_lods).chain());
    let camera = app
        .world_mut()
        .spawn((
            place(Vec3::new(0.0, 0.0, 4.0)),
            Camera {
                fov_y: FOV_Y,
                exposure_ev: Some(14.0),
                near: 0.1,
                far: 2000.0,
                ..Camera::default()
            },
        ))
        .id();
    (app, camera)
}

fn set_distance(app: &mut App, camera: Entity, distance: f32) {
    app.world_mut()
        .entity_mut(camera)
        .insert(place(Vec3::new(0.0, 0.0, distance)));
}

/// Distance at which the unit-radius chain crosses LOD `threshold` (neutral scales).
fn switch_distance(threshold: f32) -> f32 {
    MeshLodScales::default()
        .switch_distance(1.0, threshold, 1080, FOV_Y, 1.0, false)
        .expect("a positive threshold switches")
}

/// One extracted draw, reduced to what the selection decides.
#[derive(Clone, Debug, PartialEq)]
struct Pick {
    /// LOD index (from the drawn index count), `None` for the mesh's own buffer.
    lod: Option<usize>,
    lod_dither_bits: u32,
    lod_key: u64,
    shadow: ShadowCast,
    /// The draw's object-key tag relative to its owner (0, the fade tag or the shadow tag).
    tag: u64,
}

fn lod_of(indices: usize) -> usize {
    LEVELS
        .iter()
        .position(|(tris, _)| (*tris * 3) as usize == indices)
        .expect("a chain level's index count")
}

/// The draws of `owner_key` (an entity's bits or a keyed draw's object key), in frame order.
fn picks(app: &App, owner_key: u64) -> Vec<Pick> {
    let extracted = app.world().resource::<ExtractedFrame>();
    extracted
        .meshes
        .iter()
        .filter_map(|m| {
            let key = m.entity.to_bits() ^ m.object_key_xor;
            let tag = [0, MESH_LOD_FADE_OBJECT_TAG, MESH_LOD_SHADOW_OBJECT_TAG]
                .into_iter()
                .find(|tag| key ^ tag == owner_key)?;
            Some(Pick {
                lod: m.draw_indices.as_ref().map(|i| lod_of(i.len())),
                lod_dither_bits: m.lod_dither.to_bits(),
                lod_key: m.lod_key,
                shadow: m.shadow,
                tag,
            })
        })
        .collect()
}

/// The colour LOD of the draw tagged 0 (the incoming level of a fade).
fn color_lod(picks: &[Pick]) -> Option<usize> {
    picks.iter().find(|p| p.tag == 0).and_then(|p| p.lod)
}

#[test]
fn drawlist_item_crossfades_like_an_entity() {
    let (mut app, camera) = app();
    let (mesh, material) = (mesh(), Arc::new(CpuMaterial::default()));
    let chain = chain();
    let entity = app
        .world_mut()
        .spawn((
            place(Vec3::ZERO),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            chain.clone(),
        ))
        .id();
    let key = DrawKey::pair(27, 1);
    let shared_chain = Arc::new(chain);
    // The dolly: from inside LOD 0 out past LOD 2 and back, geometric steps, per-frame re-submit
    // as a mirror host does.
    let near = 0.5 * switch_distance(LEVELS[0].1);
    let far = 3.0 * switch_distance(LEVELS[1].1);
    let steps = 120usize;
    let mut path: Vec<f32> = (0..=steps)
        .map(|i| near * (far / near).powf(i as f32 / steps as f32))
        .collect();
    let back: Vec<f32> = path.iter().rev().copied().collect();
    path.extend(back);
    let mut lods_seen = BTreeSet::new();
    let (mut fade_frames, mut separate_shadow_frames) = (0usize, 0usize);
    for (frame, distance) in path.iter().enumerate() {
        set_distance(&mut app, camera, *distance);
        app.world_mut().resource_mut::<DrawList>().sync([(
            key,
            KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&material), Mat4::IDENTITY)
                .with_lod_chain(Arc::clone(&shared_chain)),
        )]);
        app.update();
        let as_entity = picks(&app, entity.to_bits());
        let as_draw = picks(&app, key.object_key());
        assert!(!as_entity.is_empty(), "frame {frame}: the entity draws");
        assert_eq!(
            as_draw, as_entity,
            "frame {frame} at {distance:.2} m: the keyed draw's LOD picks differ from the entity's"
        );
        if let Some(lod) = color_lod(&as_draw) {
            lods_seen.insert(lod);
        }
        fade_frames += usize::from(as_draw.iter().any(|p| p.tag == MESH_LOD_FADE_OBJECT_TAG));
        separate_shadow_frames += usize::from(as_draw.iter().any(|p| p.tag == MESH_LOD_SHADOW_OBJECT_TAG));
    }
    assert_eq!(
        lods_seen,
        BTreeSet::from([0, 1, 2]),
        "the dolly visits every level"
    );
    assert!(
        fade_frames >= 4 * 2,
        "four switches cross-fade over several frames each, got {fade_frames} fading frames"
    );
    assert!(
        separate_shadow_frames > 0,
        "the shadow LOD runs coarser on some frames"
    );
    let lod = app.world().resource::<DrawList>().lod_stats();
    assert_eq!(lod.selected, 1);
    assert_eq!(lod.selections, path.len() as u64);
}

#[test]
fn drawlist_lod_memory_is_per_key() {
    let (mut app, camera) = app();
    let (mesh, material) = (mesh(), Arc::new(CpuMaterial::default()));
    let chain = Arc::new(chain());
    let draw = || {
        KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&material), Mat4::IDENTITY)
            .with_lod_chain(Arc::clone(&chain))
    };
    // A chained entity and key A start close (LOD 0).
    let entity = app
        .world_mut()
        .spawn((
            place(Vec3::ZERO),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
            (*chain).clone(),
        ))
        .id();
    let a = DrawKey::pair(1, 0);
    app.world_mut().resource_mut::<DrawList>().insert(a, draw());
    set_distance(&mut app, camera, 0.5 * switch_distance(LEVELS[0].1));
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(color_lod(&picks(&app, a.object_key())), Some(0));
    // Step just past the LOD 0 → 1 switch, inside the hysteresis band: whoever has history at
    // LOD 0 keeps it.
    set_distance(&mut app, camera, 1.05 * switch_distance(LEVELS[0].1));
    app.update();
    assert_eq!(
        color_lod(&picks(&app, a.object_key())),
        Some(0),
        "A holds LOD 0 inside the band"
    );
    assert_eq!(color_lod(&picks(&app, entity.to_bits())), Some(0));
    // A fresh key B, and a key whose number equals the entity's bits, at the same transform:
    // no history, so both pick the level the size alone gives.
    let b = DrawKey::pair(1, 1);
    let twin = DrawKey(entity.to_bits());
    {
        let mut list = app.world_mut().resource_mut::<DrawList>();
        list.insert(b, draw());
        list.insert(twin, draw());
    }
    app.update();
    assert_eq!(
        color_lod(&picks(&app, a.object_key())),
        Some(0),
        "A keeps its own memory"
    );
    assert_eq!(
        color_lod(&picks(&app, b.object_key())),
        Some(1),
        "B starts from its own (empty) memory"
    );
    assert_eq!(
        color_lod(&picks(&app, twin.object_key())),
        Some(1),
        "a key equal to an entity's bits shares nothing with that entity"
    );
    assert_eq!(
        color_lod(&picks(&app, entity.to_bits())),
        Some(0),
        "the entity keeps its memory"
    );
    let lod = app.world().resource::<DrawList>().lod_stats();
    assert_eq!(lod.selected, 3);
    assert_eq!(lod.color_lods, vec![1, 2], "A at LOD 0, B and the twin at LOD 1");
    // Removing B and inserting it again a frame later is a fresh start (the entity rule for a
    // mesh missing a frame): back near, B returns at LOD 0 at once, without a fade.
    app.world_mut().resource_mut::<DrawList>().remove(b);
    app.update();
    assert!(picks(&app, b.object_key()).is_empty());
    set_distance(&mut app, camera, 0.5 * switch_distance(LEVELS[0].1));
    app.world_mut().resource_mut::<DrawList>().insert(b, draw());
    app.update();
    let returned = picks(&app, b.object_key());
    assert_eq!(color_lod(&returned), Some(0));
    assert!(
        returned.iter().all(|p| p.tag != MESH_LOD_FADE_OBJECT_TAG),
        "a returning key does not fade from a stale level"
    );
}

#[test]
fn shadow_cast_policy_component_reaches_extraction() {
    let (mut app, camera) = app();
    let (mesh, material) = (mesh(), Arc::new(CpuMaterial::default()));
    let local = ShadowCast::Local {
        reach: 3.0,
        max_cascades: 2,
        shadow_scale: 1.0,
        shadow_lift: 0.0,
        anchor: None,
        lod_switch_keeps_pages: false,
    };
    let renderer = || MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material));
    let plain = app.world_mut().spawn((place(Vec3::ZERO), renderer())).id();
    let policy = app
        .world_mut()
        .spawn((place(Vec3::ZERO), renderer(), MeshShadowCast(local)))
        .id();
    let off = app
        .world_mut()
        .spawn((
            place(Vec3::ZERO),
            renderer(),
            MeshShadowCast(local),
            NotShadowCaster,
        ))
        .id();
    // A chained one whose shadow is pinned two levels coarser: its colour draw casts nothing and a
    // separate shadow-only draw casts with the policy.
    let chained = app
        .world_mut()
        .spawn((
            place(Vec3::ZERO),
            renderer(),
            chain(),
            MeshShadowCast(local),
            ShadowLodHint::fixed(2),
        ))
        .id();
    let key = DrawKey::pair(2, 0);
    app.world_mut().resource_mut::<DrawList>().insert(
        key,
        KeyedDraw::new(Arc::clone(&mesh), Arc::clone(&material), Mat4::IDENTITY)
            .with_lod_chain(Arc::new(chain()))
            .with_shadow(local)
            .with_shadow_lod_hint(ShadowLodHint::fixed(2)),
    );
    set_distance(&mut app, camera, 0.5 * switch_distance(LEVELS[0].1));
    app.update();
    let shadows = |owner: u64| {
        picks(&app, owner)
            .into_iter()
            .map(|p| (p.tag, p.lod, p.shadow))
            .collect::<Vec<_>>()
    };
    assert_eq!(shadows(plain.to_bits()), vec![(0, None, ShadowCast::All)]);
    assert_eq!(
        shadows(policy.to_bits()),
        vec![(0, None, local)],
        "MeshShadowCast reaches the frame"
    );
    assert_eq!(
        shadows(off.to_bits()),
        vec![(0, None, ShadowCast::Off)],
        "NotShadowCaster wins"
    );
    let expected = vec![
        (0, Some(0), ShadowCast::Off),
        (MESH_LOD_SHADOW_OBJECT_TAG, Some(2), local),
    ];
    assert_eq!(shadows(chained.to_bits()), expected, "through the LOD chain");
    assert_eq!(shadows(key.object_key()), expected, "a keyed draw likewise");
    let list = app.world().resource::<DrawList>();
    assert_eq!(list.stats().local_casters, 1);
    assert_eq!(list.lod_stats().shadow_lods, vec![0, 0, 1]);
}

#[test]
fn a_keyed_draw_keeps_its_object_id_across_frames() {
    let (mut app, camera) = app();
    let (mesh, material) = (mesh(), Arc::new(CpuMaterial::default()));
    let chain = Arc::new(chain());
    let keys = [DrawKey::pair(9, 0), DrawKey::pair(9, 1), DrawKey::pair(9, 2)];
    let near = 0.5 * switch_distance(LEVELS[0].1);
    let far = 3.0 * switch_distance(LEVELS[1].1);
    let mut lods_seen = BTreeSet::new();
    let mut fades = 0usize;
    for frame in 0..80usize {
        let t = frame as f32 / 79.0;
        set_distance(&mut app, camera, near * (far / near).powf(t));
        // A new KeyedDraw value per key per frame, as a mirror host re-submits; key 1 is a
        // plain draw, keys 0 and 2 are chained.
        app.world_mut()
            .resource_mut::<DrawList>()
            .sync(keys.iter().enumerate().map(|(i, key)| {
                let x = i as f32 * 0.01;
                let item = KeyedDraw::new(
                    Arc::clone(&mesh),
                    Arc::clone(&material),
                    Mat4::from_translation(Vec3::new(x, 0.0, 0.0)),
                );
                let item = if i == 1 {
                    item
                } else {
                    item.with_lod_chain(Arc::clone(&chain))
                };
                (*key, item)
            }));
        app.update();
        let extracted = app.world().resource::<ExtractedFrame>();
        let scene = extracted.frame_scene(0, 1920, 1080);
        // Every object key in the frame scene is one of ours, possibly tagged.
        let mut colour_ids = Vec::new();
        for (m, frame_mesh) in extracted.meshes.iter().zip(scene.meshes.iter()) {
            let id = frame_mesh.object_key;
            let owner = keys
                .iter()
                .find(|k| {
                    [0, MESH_LOD_FADE_OBJECT_TAG, MESH_LOD_SHADOW_OBJECT_TAG]
                        .iter()
                        .any(|tag| k.object_key() ^ tag == id)
                })
                .unwrap_or_else(|| panic!("frame {frame}: object key {id:#x} names no draw key"));
            if id == owner.object_key() {
                colour_ids.push(id);
                if let Some(indices) = &m.draw_indices {
                    lods_seen.insert(lod_of(indices.len()));
                }
            }
            if id == owner.object_key() ^ MESH_LOD_FADE_OBJECT_TAG {
                fades += 1;
            }
        }
        // Each key's main draw carries the key's own object key, every frame.
        let mut want: Vec<u64> = keys.iter().map(|k| k.object_key()).collect();
        want.sort_unstable();
        colour_ids.sort_unstable();
        assert_eq!(
            colour_ids, want,
            "frame {frame}: one main draw per key under its object key"
        );
    }
    assert!(
        lods_seen.len() >= 2,
        "the chained keys switched LOD: {lods_seen:?}"
    );
    assert!(fades > 0, "and cross-faded");
}
