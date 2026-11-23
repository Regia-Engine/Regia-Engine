// SPDX-License-Identifier: MPL-2.0
//! Checkpointed lightmap bakes (ADR-3095): the CPU baker's texels in batches, each batch's
//! estimates kept in a `.bakeck` file, so a bake that dies continues from its last checkpoint and
//! gives the bytes an uninterrupted bake gives.
//!
//! The payload is `rays:u64 count:u64` and `count` texel estimates in task order (the tasks
//! `prepare_tasks` rasterised; a checkpoint always holds a prefix of them), each
//! `e:[f32;3] dir:[f32;3] rel_err:f32 back:u32 taken:u32` (36 bytes).

use std::sync::atomic::AtomicUsize;

use regia_asset::bake_checkpoint::BakeCheckpointKind;
use regia_asset::lightmap::{Lightmap, LightmapLayout};
use regia_core::glam::Vec3;

use super::{
    LightmapBakeReport, LightmapBakeSettings, LightmapScene, TexelEstimate, estimate_tasks, finish_bake,
    prepare_tasks,
};
use crate::PtError;
use crate::checkpoint::{
    BakeCheckpointPlan, BakeCheckpointStats, BakeProgressEstimate, Checkpointer,
    DEFAULT_LIGHTMAP_BATCH_TEXELS, Dec, Enc, InputHasher, damaged,
};

/// Bytes of one texel estimate in the payload.
const TEXEL_BYTES: usize = 36;

/// The input hash of a lightmap bake: the flattened scene, the layout, the receivers, every
/// setting but the worker threads (they never change the result) and the baker `kind`. Two bakes
/// with the same hash give the same lightmap bit for bit.
#[must_use]
pub fn lightmap_input_hash(
    scene: &LightmapScene,
    layout: &LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    kind: BakeCheckpointKind,
) -> [u8; 32] {
    let _span = tracing::info_span!("lightmap_input_hash").entered();
    let mut h = InputHasher::new("regia-bake-checkpoint/lightmap/1");
    h.bytes(kind.name().as_bytes());
    h.debug(&LightmapBakeSettings {
        threads: 0,
        ..*settings
    });
    h.debug(layout);
    h.debug(&receivers);
    h.debug(scene);
    h.finish()
}

/// Encodes the finished prefix of a bake.
pub(super) fn encode(estimates: &[TexelEstimate], rays: u64) -> Vec<u8> {
    let mut e = Enc(Vec::with_capacity(16 + estimates.len() * TEXEL_BYTES));
    e.u64(rays);
    e.u64(estimates.len() as u64);
    for t in estimates {
        for v in t.e.to_array().into_iter().chain(t.dir.to_array()) {
            e.f32(v);
        }
        e.f32(t.rel_err);
        e.u32(t.back);
        e.u32(t.taken);
    }
    e.0
}

/// Decodes a payload of `done` estimates of a bake of `total` tasks.
pub(super) fn decode(payload: &[u8], done: u64, total: usize) -> Result<(Vec<TexelEstimate>, u64), PtError> {
    let mut d = Dec::new(payload);
    let rays = d.u64()?;
    let n = d.count(total as u64, TEXEL_BYTES)?;
    if n as u64 != done {
        return Err(damaged(format!(
            "the payload holds {n} texels, the header {done}"
        )));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut v = [0.0f32; 6];
        for x in &mut v {
            *x = d.f32()?;
        }
        out.push(TexelEstimate {
            e: Vec3::new(v[0], v[1], v[2]),
            dir: Vec3::new(v[3], v[4], v[5]),
            rel_err: d.f32()?,
            back: d.u32()?,
            taken: d.u32()?,
        });
    }
    d.end()?;
    Ok((out, rays))
}

/// [`super::bake_lightmap`] with a checkpoint (ADR-3095): the texels are estimated in batches of
/// `plan.batch_units` ([`DEFAULT_LIGHTMAP_BATCH_TEXELS`] by default), and the estimates finished so
/// far are written to `plan.path` as the plan's cadence asks. With `plan.resume` and a checkpoint of
/// this bake at that path, the bake continues after its last texel. The checkpoint is removed when
/// the bake finishes. The lightmap is byte-identical to [`super::bake_lightmap`]'s, resumed or not.
/// `progress` sees the units done over every run and the estimate of the rest.
///
/// # Errors
///
/// As [`super::bake_lightmap`]; `bake_checkpoint_mismatch` (another bake's checkpoint),
/// `bake_checkpoint_damaged` / `bake_checkpoint_version` (one that does not read),
/// `bake_checkpoint_io` (it cannot be written), `bake_interrupted` (the plan's stop hook), each
/// starting the message ([`crate::checkpoint::bake_checkpoint_error_code`]).
pub fn bake_lightmap_checkpointed(
    scene: &LightmapScene,
    layout: LightmapLayout,
    receivers: &[usize],
    settings: &LightmapBakeSettings,
    plan: &BakeCheckpointPlan,
    progress: impl Fn(&BakeProgressEstimate) + Sync,
) -> Result<(Lightmap, LightmapBakeReport, BakeCheckpointStats), PtError> {
    let _span = tracing::info_span!("lightmap_bake_checkpointed", entries = layout.entries.len()).entered();
    let prepared = prepare_tasks(scene, &layout, receivers, settings)?;
    let total = prepared.tasks.len();
    let hash = lightmap_input_hash(
        scene,
        &layout,
        receivers,
        settings,
        BakeCheckpointKind::LightmapCpu,
    );
    let (mut ck, found) = Checkpointer::open(plan, BakeCheckpointKind::LightmapCpu, hash)?;
    let (mut estimates, mut rays) = match found {
        Some(c) => {
            if c.header.units_total != total as u64 {
                return Err(damaged(format!(
                    "the checkpoint counts {} texels, the bake {total}",
                    c.header.units_total
                )));
            }
            decode(&c.payload, c.header.units_done, total)?
        }
        None => (Vec::with_capacity(total), 0),
    };
    let batch = plan.batch_or(DEFAULT_LIGHTMAP_BATCH_TEXELS).max(1);
    let base_seconds = ck.work_seconds();
    let started = std::time::Instant::now();
    while estimates.len() < total {
        let start = estimates.len();
        let end = (start + batch).min(total);
        let done = AtomicUsize::new(start);
        let report = |now: usize, all: usize| {
            progress(&BakeProgressEstimate {
                done: now as u64,
                total: all as u64,
                work_seconds: base_seconds + started.elapsed().as_secs_f64(),
            });
        };
        let (part, part_rays) = estimate_tasks(
            scene,
            &prepared.tasks[start..end],
            settings,
            &done,
            total,
            &report,
        );
        estimates.extend(part);
        rays += part_rays;
        ck.batch_done(0, total as u64, end as u64, end == total, || {
            encode(&estimates, rays)
        })?;
    }
    progress(&ck.estimate(total as u64, total as u64));
    let (map, report) = finish_bake(scene, layout, settings, &prepared, &estimates, rays)?;
    Ok((map, report, ck.finish()))
}

#[cfg(test)]
mod tests {
    use super::super::{bake_lightmap, quad_box_scene};
    use super::*;
    use crate::checkpoint::{
        BAKE_CHECKPOINT_DAMAGED, BAKE_CHECKPOINT_MISMATCH, BAKE_INTERRUPTED, bake_checkpoint_error_code,
    };
    use regia_asset::lightmap::{LightmapEntry, MeshUnwrap};

    /// The quad-box scene (a floor under a box, so shadows and bounces) with the floor unwrapped
    /// onto a `res × res` atlas.
    pub(crate) fn quad_box(res: u32) -> (LightmapScene, LightmapLayout, Vec<usize>) {
        let q = quad_box_scene();
        let scene = LightmapScene::from_pt_scene(&q.scene).expect("scene");
        let layout = LightmapLayout {
            width: res,
            height: res,
            density: 1.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, res, res],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 1.0,
                texels_per_metre: 1.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: q.floor_uv.clone(),
                indices: vec![0, 2, 1, 0, 3, 2],
            }],
        };
        (scene, layout, vec![q.floor])
    }

    fn settings() -> LightmapBakeSettings {
        LightmapBakeSettings {
            samples: 8,
            bounces: 2,
            threads: 3,
            ..LightmapBakeSettings::default()
        }
    }

    fn plan(dir: &std::path::Path) -> BakeCheckpointPlan {
        BakeCheckpointPlan {
            min_interval: std::time::Duration::ZERO,
            batch_units: 37,
            ..BakeCheckpointPlan::new(dir.join(".quad.flmap.bakeck"))
        }
    }

    /// The core claim: a bake stopped at batch K and resumed (with another thread count) writes the
    /// bytes of a bake that never stopped, and the checkpoint is gone afterwards.
    #[test]
    fn a_resumed_bake_is_byte_identical_to_one_that_never_stopped() {
        let (scene, layout, receivers) = quad_box(24);
        let s = settings();
        let (reference, _) =
            bake_lightmap(&scene, layout.clone(), &receivers, &s, |_, _| {}).expect("reference");
        let reference = reference.to_bytes().expect("encodes");
        let dir = regia_test_utils::unique_temp_dir("pt_lightmap_resume");
        // Stop after batch 2, then after batch 5 (two crashes), then finish.
        let mut p = plan(&dir);
        for stop in [2, 5] {
            p.stop_after_batches = Some(stop);
            let Err(e) = bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &s, &p, |_| {})
            else {
                panic!("stopped expected")
            };
            assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_INTERRUPTED), "{e}");
            let header = regia_asset::bake_checkpoint::read_header(&p.path).expect("checkpoint");
            assert_eq!(header.batches_done, stop);
            assert_eq!(header.units_done, 37 * stop);
        }
        p.stop_after_batches = None;
        let s4 = LightmapBakeSettings { threads: 1, ..s };
        let seen = std::sync::Mutex::new(Vec::new());
        let (map, _, stats) = bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &s4, &p, |e| {
            if let Ok(mut v) = seen.lock() {
                v.push((e.done, e.remaining()));
            }
        })
        .expect("resumed");
        assert_eq!(
            map.to_bytes().expect("encodes"),
            reference,
            "resumed bytes differ"
        );
        assert!(stats.resumed && stats.removed && !p.path.exists());
        assert_eq!(stats.resumed_units, 37 * 5);
        assert!(stats.largest_bytes > 0);
        let seen = seen.into_inner().unwrap_or_default();
        assert!(
            seen.iter().all(|&(done, _)| done >= 37 * 5),
            "progress starts at the resumed texels"
        );
        assert_eq!(seen.last().map(|s| s.1), Some(0));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A checkpoint of another seed is refused; a truncated one is damaged; neither panics.
    #[test]
    fn another_bakes_or_a_damaged_checkpoint_is_refused() {
        let (scene, layout, receivers) = quad_box(12);
        let dir = regia_test_utils::unique_temp_dir("pt_lightmap_resume_refuse");
        let mut p = plan(&dir);
        p.stop_after_batches = Some(1);
        let s = settings();
        let _ = bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &s, &p, |_| {});
        assert!(p.path.exists());
        p.stop_after_batches = None;
        let reseeded = LightmapBakeSettings { seed: 9, ..s };
        let Err(e) = bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &reseeded, &p, |_| {})
        else {
            panic!("mismatch expected")
        };
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_MISMATCH));
        let bytes = std::fs::read(&p.path).expect("read");
        std::fs::write(&p.path, &bytes[..bytes.len() - 3]).expect("truncate");
        let Err(e) = bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &s, &p, |_| {}) else {
            panic!("damaged expected")
        };
        assert_eq!(bake_checkpoint_error_code(&e), Some(BAKE_CHECKPOINT_DAMAGED));
        // `resume: false` starts over and overwrites it.
        p.resume = false;
        let (_, _, stats) =
            bake_lightmap_checkpointed(&scene, layout.clone(), &receivers, &s, &p, |_| {}).expect("fresh");
        assert!(!stats.resumed && !p.path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Seeded mutation of a real payload: an error or estimates, never a panic.
    #[test]
    fn mutated_lightmap_payloads_never_panic() {
        let t = TexelEstimate::default();
        let good = encode(&[t; 5], 7);
        let mut x = 0x2545_f491_u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..2000 {
            let mut b = good.clone();
            match next() % 3 {
                0 => {
                    let i = next() as usize % b.len();
                    b[i] ^= 1 << (next() % 8);
                }
                1 => b.truncate(next() as usize % b.len()),
                _ => b[8..16].copy_from_slice(&u64::from(next()).wrapping_mul(0x1_0000_0001).to_le_bytes()),
            }
            let _ = decode(&b, 5, 10);
        }
    }

    #[test]
    fn the_payload_round_trips_and_refuses_wrong_counts() {
        let t = TexelEstimate {
            e: Vec3::new(1.0, 2.0, 3.0),
            back: 4,
            taken: 5,
            dir: Vec3::new(0.0, 1.0, 0.0),
            rel_err: f32::NAN,
        };
        let bytes = encode(&[t, t], 99);
        let (back, rays) = decode(&bytes, 2, 10).expect("decodes");
        assert_eq!(rays, 99);
        assert_eq!(back.len(), 2);
        assert!(back[0].rel_err.is_nan() && back[1].taken == 5);
        assert!(decode(&bytes, 3, 10).is_err(), "header disagrees");
        assert!(decode(&bytes, 2, 1).is_err(), "more texels than the bake");
        assert!(decode(&bytes[..bytes.len() - 1], 2, 10).is_err(), "short");
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode(&long, 2, 10).is_err(), "trailing bytes");
    }
}
