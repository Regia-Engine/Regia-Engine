// SPDX-License-Identifier: MPL-2.0
//! Render-world entry lifecycle (W2.2, ADR-248).
//!
//! **Invariant.** From the moment [`FrameRenderer::render_into`] starts collecting draws until its
//! last pass is encoded (the *draw window*), every key a draw refers to names a live entry: the
//! mesh and material of every draw, and the object of every draw that binds a per-object uniform
//! (`cpu_uniform`). The places an entry can disappear, and how each keeps the invariant:
//!
//! | Where | When | Kept by |
//! |---|---|---|
//! | `mesh_belt_keys` drop | frame start, before the window opens | outside the window |
//! | `release_meshes`, `pin_keys`, `prewarm`'s `objects.clear()`, `set_morph_instances` | between frames (`&mut self`) | outside the window; a released key drawn again re-uploads |
//! | `ensure_mesh` / `ensure_material` rebuild, key-0 (ephemeral) drop | collection, same key and draw | replaced in place before the draw is pushed |
//! | morph deformed copy the last frame did not draw (`morph_draw_key`, one copy per mesh key) | collection, the object's first draw of the frame | **deferred** to the window's close when a window is open ([`EntryLifecycle::defer_mesh_removal`]) |
//! | skin-palette growth | collection (`write_skin`) | nothing is removed: `ensure_object` rebinds each object on its next use |
//! | eviction sweep (`evict`) | after the last encode, window closed | every entry drawn this frame has `last_used == now`; the sweep only takes entries idle before `now` |
//! | anything else (a future site) | any | objects are rebuilt on demand after collection ([`FrameRenderer::repair_objects`]); a missing mesh or material skips the draw with a once-per-id warning and counts it ([`DrawIntegrity::skipped_draws`], the v0.1.13.1 last resort) |
//!
//! A frame that returns early (an error) leaves the window open with its deferred removals; the
//! next frame's open applies them first.

use std::sync::atomic::{AtomicU64, Ordering};

use super::FrameRenderer;

/// Render-world entry integrity and skinned-bound counts ([`FrameRenderer::draw_integrity`]).
///
/// The per-frame counts are the last main-view frame's (a texture view's frame adds to the
/// totals only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrawIntegrity {
    /// Draws the last frame skipped because a mesh, object or material entry was gone (the
    /// last-resort path; 0 when the lifecycle invariant held).
    pub skipped_draws: u64,
    /// [`Self::skipped_draws`] since the renderer was created, every view included.
    pub skipped_draws_total: u64,
    /// Object entries the last frame rebuilt after collecting its draws (0 when the invariant
    /// held).
    pub objects_rebuilt: u64,
    /// [`Self::objects_rebuilt`] since the renderer was created.
    pub objects_rebuilt_total: u64,
    /// Mesh-entry removals the last frame deferred to its end (a morph instance's deformed copy
    /// replaced while another draw of the frame still held it).
    pub deferred_removals: u64,
    /// [`Self::deferred_removals`] since the renderer was created.
    pub deferred_removals_total: u64,
    /// Skinned draws of the last frame with a joint-palette bound
    /// ([`FrameRenderer::set_skinned_bounds`]).
    pub skinned_bounded: usize,
    /// Skinned draws of the last frame without one (bounds off, ephemeral key, unboundable
    /// weights): they always draw.
    pub skinned_unbounded: usize,
    /// Skinned draws the last frame culled from the color passes by their bound.
    pub skinned_culled: usize,
    /// Skinned bounds are on ([`FrameRenderer::skinned_bounds`]).
    pub skinned_bounds: bool,
    /// Skinned draws the last frame refused because a vertex weights a joint at or past the
    /// draw's palette length or [`crate::MAX_JOINTS_PER_SKIN`] (ADR-5418): drawn, they would skin
    /// from another draw's joints. No pass draws them. A palette longer than the cap whose
    /// vertices stay under it draws with its first `MAX_JOINTS_PER_SKIN` joints and is not counted.
    pub skinned_refused: usize,
    /// Opaque draws of the last frame that collection sent to the GPU-driven batches but the GPU
    /// cull did not take (their mesh had lost its cull bound by then): they are drawn per object
    /// instead, with an object entry built after the cull (0 when the invariant held). Before
    /// this guard such a draw had no object entry: v0.1.13 panicked (`expect("object entry")`,
    /// DO27's hand-built pilot, 10-16) and later versions skipped it.
    pub cull_missed_draws: u64,
    /// [`Self::cull_missed_draws`] since the renderer was created, every view included.
    pub cull_missed_draws_total: u64,
}

/// Which entry a test fault drops.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryKind {
    Mesh,
    Object,
    Material,
}

/// The frame's draw window, the removals deferred to its close, and the counters.
#[derive(Default)]
pub(super) struct EntryLifecycle {
    /// Draws are being collected or encoded: entries a draw refers to must stay.
    open: bool,
    /// Mesh keys whose removal waits for the window to close.
    deferred_meshes: Vec<u64>,
    /// Draws skipped by the last-resort path since creation. Atomic: the encode closures hold
    /// `&self`.
    pub(super) skips: AtomicU64,
    /// [`Self::skips`] when the window opened.
    skips_at_open: u64,
    /// Object entries rebuilt after collection, this frame and in total.
    rebuilt: u64,
    rebuilt_total: u64,
    /// Opaque draws the GPU cull missed and the frame drew per object, this frame and in total.
    cull_missed: u64,
    cull_missed_total: u64,
    /// Removals deferred, this frame and in total.
    deferred: u64,
    deferred_total: u64,
    /// The last main-view frame's snapshot.
    last: DrawIntegrity,
    /// Test faults: entries dropped after collection, before (`.0`) or after (`.1`) the repair.
    #[cfg(test)]
    pub(super) faults: (Vec<Fault>, Vec<Fault>),
    /// Test hook: `ensure_mesh` behaves as before ADR-4577 (v0.1.13), where a skinned visit of
    /// a mesh cleared its cull bound and a static visit put it back.
    #[cfg(test)]
    pub(super) legacy_skinned_visit_clears_bound: bool,
}

/// A test fault: the entry of this kind and key is dropped (`EntryLifecycle::faults`).
#[cfg(test)]
type Fault = (EntryKind, u64);

impl EntryLifecycle {
    /// While the draw window is open, queues `key`'s mesh entry for removal at its close and
    /// returns `true`; returns `false` when the caller may remove it now.
    pub(super) fn defer_mesh_removal(&mut self, key: u64) -> bool {
        if !self.open {
            return false;
        }
        self.deferred_meshes.push(key);
        self.deferred += 1;
        true
    }
}

impl FrameRenderer {
    /// Render-world entry integrity of the last frame and since creation: draws skipped
    /// because an entry was gone, object entries rebuilt after collection, removals deferred to
    /// the frame's end, and the skinned-bound counts ([`DrawIntegrity`]). Telemetry reports
    /// `draws.skipped` from it; agents read it with `render.draw_integrity`.
    #[must_use]
    pub fn draw_integrity(&self) -> DrawIntegrity {
        let e = &self.entries;
        DrawIntegrity {
            skipped_draws_total: e.skips.load(Ordering::Relaxed),
            objects_rebuilt_total: e.rebuilt_total,
            cull_missed_draws_total: e.cull_missed_total,
            deferred_removals_total: e.deferred_total,
            skinned_bounds: self.skin_bounds.enabled,
            ..e.last
        }
    }

    /// Opens the frame's draw window (after the frame-start drops, before collection). Applies
    /// the removals a frame that returned early left behind.
    pub(super) fn open_draw_window(&mut self) {
        self.apply_deferred_removals();
        let e = &mut self.entries;
        e.open = true;
        e.skips_at_open = e.skips.load(Ordering::Relaxed);
        e.rebuilt = 0;
        e.cull_missed = 0;
        e.deferred = 0;
        self.skin_bounds.begin_frame();
    }

    /// Closes the draw window after the frame's last encode: applies the deferred removals and
    /// publishes the frame's counts (main view only).
    pub(super) fn close_draw_window(&mut self, view_pass: bool) {
        self.apply_deferred_removals();
        let e = &mut self.entries;
        e.open = false;
        e.rebuilt_total += e.rebuilt;
        e.cull_missed_total += e.cull_missed;
        e.deferred_total += e.deferred;
        if !view_pass {
            e.last = DrawIntegrity {
                skipped_draws: e.skips.load(Ordering::Relaxed).saturating_sub(e.skips_at_open),
                objects_rebuilt: e.rebuilt,
                cull_missed_draws: e.cull_missed,
                deferred_removals: e.deferred,
                skinned_bounded: self.skin_bounds.bounded,
                skinned_unbounded: self.skin_bounds.unbounded,
                skinned_culled: self.skin_bounds.culled,
                skinned_refused: self.skin_bounds.refused,
                ..DrawIntegrity::default()
            };
        }
    }

    fn apply_deferred_removals(&mut self) {
        for key in std::mem::take(&mut self.entries.deferred_meshes) {
            self.meshes.remove(&key);
            self.rigid_skin.remove(&key);
        }
    }

    /// Rebuilds the object entry of every key in `objects` (the draws that bind a per-object
    /// uniform) that is missing after collection, so the frame's per-object passes find it.
    pub(super) fn repair_objects(
        &mut self,
        d: &wgpu::Device,
        objects: impl IntoIterator<Item = super::ObjectUniformKey>,
        now: u64,
    ) {
        #[cfg(test)]
        self.drop_fault_entries(false);
        for key in objects {
            if self.objects.contains_key(&key) {
                continue;
            }
            self.ensure_object(d, key, now);
            if self.objects.contains_key(&key) {
                self.entries.rebuilt += 1;
                tracing::warn!(
                    target: "regia_render",
                    ?key,
                    "object entry rebuilt after draw collection"
                );
            }
        }
        #[cfg(test)]
        self.drop_fault_entries(true);
    }

    /// Draws per object every opaque draw in `opaque_ix` that collection sent to the GPU-driven
    /// batches (no per-object uniform) but the GPU cull did not take (no indirect command): the
    /// CPU colour loop draws every opaque draw without a command, and such a draw had no object
    /// entry there. The case of the v0.1.13 `expect("object entry")` panic (DO27, a hand-built
    /// pilot drawn rigid with a mesh another entity drew skinned, 10-16): a later visit of the
    /// mesh cleared its cull bound after the first draw was collected (fixed at the cache by
    /// ADR-4577; this keeps any other cause from dropping the draw). The entry is built and
    /// written now, through the queue: the frame's staging belt has flushed by the cull. Counted
    /// as [`DrawIntegrity::cull_missed_draws`] with a warning once per object key.
    pub(super) fn draw_cull_misses_per_object(
        &mut self,
        d: &wgpu::Device,
        q: &crate::device::TrackedQueue,
        draws: &mut [super::Draw],
        opaque_ix: &[usize],
        now: u64,
    ) {
        for &index in opaque_ix {
            let Some(draw) = draws.get_mut(index) else {
                continue;
            };
            if draw.cpu_uniform || draw.indirect.is_some() {
                continue;
            }
            draw.cpu_uniform = true;
            let key = draw.uniform_key();
            self.ensure_object(d, key, now);
            if let Some(entry) = self.objects.get_mut(&key) {
                let object = draw.object_data();
                if entry.uploaded != Some(object) {
                    q.write_buffer(&entry.buf, 0, bytemuck::bytes_of(&object));
                    entry.uploaded = Some(object);
                }
                entry.last_used = now;
            }
            self.entries.cull_missed += 1;
            warn_once("cull_missed", draw.object, || {
                tracing::warn!(
                    target: "regia_render",
                    object = draw.object,
                    mesh = draw.mesh,
                    "opaque draw missed by the GPU cull (its mesh lost its cull bound after collection); \
                     drawn per object"
                );
            });
        }
    }

    #[cfg(test)]
    fn drop_fault_entries(&mut self, after_repair: bool) {
        let faults = if after_repair {
            std::mem::take(&mut self.entries.faults.1)
        } else {
            std::mem::take(&mut self.entries.faults.0)
        };
        for (kind, key) in faults {
            match kind {
                EntryKind::Mesh => {
                    self.meshes.remove(&key);
                }
                EntryKind::Object => {
                    self.objects.retain(|&(object, _), _| object != key);
                }
                EntryKind::Material => self.forget_material(key),
            }
        }
    }
}

/// Counts a draw skipped because its `kind` entry is gone and warns once per id (v0.1.13.1: the
/// draw is skipped instead of panicking a render task).
pub(super) fn missing_draw_entry(skips: &AtomicU64, kind: &'static str, id: u64) {
    skips.fetch_add(1, Ordering::Relaxed);
    warn_once(kind, id, || {
        tracing::warn!(target: "regia_render", kind, id, "draw skipped: its {kind} entry is gone (evicted or removed after extraction)");
    });
}

/// Runs `warn` the first time `(kind, id)` is seen in this process.
fn warn_once(kind: &'static str, id: u64, warn: impl FnOnce()) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(&'static str, u64)>>> =
        std::sync::Mutex::new(None);
    let first = SEEN
        .lock()
        .map(|mut seen| seen.get_or_insert_with(Default::default).insert((kind, id)))
        .unwrap_or(false);
    if first {
        warn();
    }
}

#[cfg(test)]
mod tests {
    use super::EntryKind;
    use crate::{
        DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabMaterial, LabMesh,
        LabScene, LabVertex, RenderDevice,
    };

    fn device() -> RenderDevice {
        match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        }
    }

    /// A quad at `x`, skinned to joint 0 when `skinned`.
    fn quad(x: f32, skinned: bool) -> LabMesh {
        let mut vertices = vec![
            LabVertex::from_pos_normal_uv([x - 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, -0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.3, 0.3, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ];
        if skinned {
            for v in &mut vertices {
                v.weights = [1.0, 0.0, 0.0, 0.0];
            }
        }
        LabMesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            joints: if skinned {
                vec![JointMatrix::translation(0.0, 0.1, 0.0), JointMatrix::identity()]
            } else {
                Vec::new()
            },
            ..LabMesh::unskinned()
        }
    }

    fn lab() -> LabScene {
        LabScene {
            width: 48,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: vec![quad(-0.8, true), quad(0.0, false), quad(0.8, true)],
        }
    }

    /// `lab` with stable keys (mesh, material and object per mesh), on the per-object path or
    /// the GPU-driven one.
    fn keyed(lab: &LabScene, gpu_driven: bool) -> FrameScene<'_> {
        let quality = FrameQuality {
            gpu_driven,
            ..FrameQuality::default()
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        for (i, mesh) in scene.meshes.iter_mut().enumerate() {
            mesh.mesh_key = 0x11FE_0000 + i as u64;
            mesh.object_key = 0x11FE_1000 + i as u64;
            mesh.material_key = 0x11FE_2000;
        }
        scene
    }

    /// An object entry dropped after collection is rebuilt before any pass reads it; a mesh,
    /// object or material dropped after the repair takes the last-resort skip (counted, no
    /// panic), and the next frame draws everything again.
    #[test]
    fn dropped_entries_are_rebuilt_or_skipped_and_counted() {
        let device = device();
        let lab = lab();
        for gpu_driven in [false, true] {
            let scene = keyed(&lab, gpu_driven);
            let mut renderer = FrameRenderer::new();
            // The skinned quads keep their per-object entries: with skinned instancing (ADR-1368)
            // they would join the GPU-driven batches and have no object entry to drop.
            renderer.set_skinned_instancing(false);
            let clean = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
            assert_eq!(renderer.draw_integrity().skipped_draws, 0);

            // The skinned quad's object entry goes after collection: rebuilt, same image.
            renderer.entries.faults.0 = vec![(EntryKind::Object, scene.meshes[0].object_key)];
            let repaired = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
            let integrity = renderer.draw_integrity();
            assert_eq!(integrity.objects_rebuilt, 1, "{integrity:?}");
            assert_eq!(integrity.skipped_draws, 0, "{integrity:?}");
            assert_eq!(
                repaired, clean,
                "gpu_driven {gpu_driven}: the rebuilt object draws as before"
            );

            // Gone after the repair: the per-object passes skip the draws and count them.
            renderer.entries.faults.1 = vec![
                (EntryKind::Object, scene.meshes[2].object_key),
                (EntryKind::Mesh, scene.meshes[0].mesh_key),
                (EntryKind::Material, scene.meshes[1].material_key),
            ];
            renderer.render_offscreen(&device, &scene).expect("no panic");
            let integrity = renderer.draw_integrity();
            assert!(integrity.skipped_draws > 0, "{integrity:?}");
            assert_eq!(integrity.skipped_draws_total, integrity.skipped_draws);

            // The next frame re-ensures everything: clean again.
            let again = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
            assert_eq!(renderer.draw_integrity().skipped_draws, 0);
            assert_eq!(again, clean, "gpu_driven {gpu_driven}: recovered");
        }
    }

    /// Sweep with `last_used` far behind: every entry this frame draws was touched this frame,
    /// so a pressure sweep at its end keeps them, whatever their age before.
    #[test]
    fn a_sweep_keeps_what_the_frame_drew_whatever_its_last_use() {
        let device = device();
        let lab = lab();
        let scene = keyed(&lab, false);
        let mut renderer = FrameRenderer::new();
        let clean = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        // Age every entry: last used at frame 0, the clock thousands of frames on.
        renderer.frame_index += 50_000;
        for entry in renderer.meshes.values_mut() {
            entry.last_used = 0;
        }
        for entry in renderer.materials.values_mut() {
            entry.last_used = 0;
        }
        for entry in renderer.objects.values_mut() {
            entry.last_used = 0;
        }
        renderer.trim_memory();
        let image = renderer.render_offscreen(&device, &scene).expect("frame").pixels;
        assert_eq!(image, clean);
        assert_eq!(renderer.draw_integrity().skipped_draws, 0);
        assert_eq!(renderer.draw_integrity().objects_rebuilt, 0);
        let stats = renderer.cache_stats();
        assert_eq!(stats.meshes_evicted, 0, "{stats:?}");
        assert_eq!(renderer.objects.len(), 3, "every per-object draw's object kept");
        // A sweep on the next frame drops what that frame did not draw (the first quad).
        let fewer = FrameScene {
            meshes: scene.meshes[1..].to_vec(),
            ..scene.clone()
        };
        renderer.frame_index += 50_000;
        renderer.trim_memory();
        renderer.render_offscreen(&device, &fewer).expect("frame");
        assert!(!renderer.meshes.contains_key(&scene.meshes[0].mesh_key));
        assert!(renderer.meshes.contains_key(&scene.meshes[2].mesh_key));
        assert_eq!(renderer.draw_integrity().skipped_draws, 0);
    }

    /// One submitted part of a pilot-shaped sweep case.
    struct Part {
        mesh: LabMesh,
        keys: [u64; 3],
        lod_dither: f32,
        shadow: crate::ShadowCast,
    }

    /// A quad at `(x, z)`, skinned over `joints` joints: every vertex weights joint 0 when
    /// `joints == 1` (a one-joint rigid skin, folded), joints 0 and 1 half each otherwise; `0`
    /// joints is rigid.
    fn part_quad(x: f32, z: f32, size: f32, joints: usize, material: usize) -> LabMesh {
        let h = size * 0.5;
        let mut vertices = vec![
            LabVertex::from_pos_normal_uv([x - h, -h, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + h, -h, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + h, h, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - h, h, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ];
        for v in &mut vertices {
            match joints {
                0 => {}
                1 => v.weights = [1.0, 0.0, 0.0, 0.0],
                _ => {
                    v.joints = [0, 1, 0, 0];
                    v.weights = [0.5, 0.5, 0.0, 0.0];
                }
            }
        }
        LabMesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            joints: (0..joints)
                .map(|j| JointMatrix::translation(0.0, 0.05 * j as f32, 0.0))
                .collect(),
            ..LabMesh::unskinned()
        }
    }

    fn part(mesh: LabMesh, keys: [u64; 3]) -> Part {
        Part {
            mesh,
            keys,
            lod_dither: crate::LOD_DITHER_NONE,
            shadow: crate::ShadowCast::All,
        }
    }

    /// The pilot-shaped cases: a hull (rigid, keys `0x10..`) plus a pilot built by hand in
    /// the ways a host can (shared keys, zero keys, folded and skinned parts, a glass visor, a
    /// blended part, a LOD cross-fade, no shadow, behind the camera).
    fn pilot_cases() -> Vec<(&'static str, Vec<Part>)> {
        const HULL: [u64; 3] = [0x10, 0x11, 0x12];
        let body = |joints| part_quad(0.7, 0.0, 0.5, joints, 0);
        let helmet = |material| part_quad(0.7, 0.05, 0.2, 0, material);
        vec![
            ("own_keys", vec![part(body(2), [0x20, 0x21, 0x22])]),
            (
                "body_and_helmet_share_an_object",
                vec![
                    part(body(2), [0x20, 0x21, 0x22]),
                    part(helmet(0), [0x30, 0x21, 0x22]),
                ],
            ),
            (
                "shares_the_hull_object",
                vec![part(body(2), [0x20, HULL[1], 0x22])],
            ),
            (
                "shares_the_hull_mesh_key",
                vec![part(body(2), [HULL[0], 0x21, 0x22])],
            ),
            ("zero_keys", vec![part(body(2), [0, 0, 0])]),
            (
                "zero_keys_two_parts",
                vec![part(body(2), [0, 0, 0]), part(helmet(0), [0, 0, 0])],
            ),
            ("one_joint_folded", vec![part(body(1), [0x20, 0x21, 0x22])]),
            (
                "folded_and_skinned_share_an_object",
                vec![
                    part(body(1), [0x20, 0x21, 0x22]),
                    part(body(2), [0x40, 0x21, 0x22]),
                ],
            ),
            (
                "rigid_and_skinned_share_an_object",
                vec![
                    part(helmet(0), [0x30, 0x21, 0x22]),
                    part(body(2), [0x20, 0x21, 0x22]),
                ],
            ),
            (
                "glass_visor_shares_an_object",
                vec![
                    part(body(2), [0x20, 0x21, 0x22]),
                    part(helmet(1), [0x30, 0x21, 0x23]),
                ],
            ),
            (
                "blended_part_shares_an_object",
                vec![
                    part(body(2), [0x20, 0x21, 0x22]),
                    part(helmet(2), [0x30, 0x21, 0x24]),
                ],
            ),
            (
                "rigid_cross_fade_pair",
                vec![
                    Part {
                        lod_dither: 0.4,
                        ..part(helmet(0), [0x30, 0x21, 0x22])
                    },
                    Part {
                        lod_dither: -0.4,
                        ..part(helmet(0), [0x31, 0x21, 0x22])
                    },
                ],
            ),
            (
                "skinned_cross_fade_pair",
                vec![
                    Part {
                        lod_dither: 0.4,
                        ..part(body(2), [0x20, 0x21, 0x22])
                    },
                    Part {
                        lod_dither: -0.4,
                        ..part(body(2), [0x40, 0x21, 0x22])
                    },
                ],
            ),
            (
                "no_shadow",
                vec![Part {
                    shadow: crate::ShadowCast::Off,
                    ..part(body(2), [0x20, 0x21, 0x22])
                }],
            ),
            (
                "behind_the_camera",
                vec![part(part_quad(0.7, 5.0, 0.5, 2, 0), [0x20, 0x21, 0x22])],
            ),
        ]
    }

    /// Sweep for the v0.1.13 `expect("object entry")` crash on a hand-built pilot (DO27, 10-16):
    /// each pilot-shaped case spawns next to a hull mid-run, leaves and comes back, on the
    /// per-object path and the GPU-driven one, with skinned instancing on and off, and with the
    /// mesh cache as it is and as it was in v0.1.13 (`legacy_skinned_visit_clears_bound`). Every
    /// frame must draw every draw: no draw skipped and no object rebuilt after collection; a
    /// draw the GPU cull missed is drawn per object, and only the v0.1.13 cache makes one.
    #[test]
    fn hand_built_pilots_need_no_entry_repair() {
        const HULL: [u64; 3] = [0x10, 0x11, 0x12];
        let device = device();
        let mut materials = vec![LabMaterial::default(); 3];
        materials[1].transmission = 0.6;
        materials[1].alpha_mode = crate::ALPHA_BLEND;
        materials[2].alpha_mode = crate::ALPHA_BLEND;
        materials[2].base_color = [0.2, 0.4, 0.9, 0.5];
        let cases = pilot_cases();
        let labs: Vec<LabScene> = cases
            .iter()
            .map(|(_, parts)| {
                let mut meshes = vec![part_quad(0.0, 0.0, 0.8, 0, 0)];
                meshes.extend(parts.iter().map(|p| p.mesh.clone()));
                LabScene {
                    materials: materials.clone(),
                    meshes,
                    ..lab()
                }
            })
            .collect();
        let mut failures = Vec::new();
        // One renderer per path, every case in turn on it (the pilots spawn into a running
        // renderer). The v0.1.13 cache only matters where the GPU culls.
        let paths = [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (true, false, true),
            (true, true, true),
        ];
        for (gpu_driven, instancing, legacy) in paths {
            let mut renderer = FrameRenderer::new();
            renderer.set_skinned_instancing(instancing);
            renderer.entries.legacy_skinned_visit_clears_bound = legacy;
            'cases: for (index, ((name, parts), lab)) in cases.iter().zip(&labs).enumerate() {
                let quality = FrameQuality {
                    gpu_driven,
                    ..FrameQuality::default()
                };
                let mut with_pilot = lab.as_frame_scene_with_quality(quality);
                [
                    with_pilot.meshes[0].mesh_key,
                    with_pilot.meshes[0].object_key,
                    with_pilot.meshes[0].material_key,
                ] = HULL;
                // A case's own keys are its own: the same number in another case names other
                // content (the hull's keys and zero keep their meaning).
                let own = |key: u64| {
                    if key == 0 || HULL.contains(&key) {
                        key
                    } else {
                        key | ((index as u64 + 1) << 32)
                    }
                };
                for (mesh, p) in with_pilot.meshes[1..].iter_mut().zip(parts) {
                    [mesh.mesh_key, mesh.object_key, mesh.material_key] = p.keys.map(own);
                    mesh.lod_dither = p.lod_dither;
                    mesh.shadow = p.shadow;
                }
                let hull_only = FrameScene {
                    meshes: with_pilot.meshes[..1].to_vec(),
                    ..with_pilot.clone()
                };
                let frames = [&hull_only, &with_pilot, &with_pilot, &hull_only, &with_pilot];
                for (frame, scene) in frames.into_iter().enumerate() {
                    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        renderer.render_offscreen(&device, scene).map(|_| ())
                    }));
                    let integrity = renderer.draw_integrity();
                    let case = format!(
                        "{name} gpu_driven {gpu_driven} instancing {instancing} legacy {legacy} frame {frame}"
                    );
                    match rendered {
                        Err(_) => {
                            failures.push(format!("{case}: panicked"));
                            break 'cases;
                        }
                        Ok(Err(err)) => failures.push(format!("{case}: {err}")),
                        Ok(Ok(()))
                            if integrity.skipped_draws > 0
                                || integrity.objects_rebuilt > 0
                                || (!legacy && integrity.cull_missed_draws > 0) =>
                        {
                            failures.push(format!("{case}: {integrity:?}"));
                        }
                        Ok(Ok(())) => {}
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} failing frames:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// The DO27 shape: one pilot mesh (bound to joint 0 only, two joints submitted so it does not
    /// fold) drawn rigid by a hand-built pilot, then skinned by a pilot the sim spawned, next to a
    /// rigid hull. `skinned_first` swaps the two pilots in the submission.
    fn pilot_pair_scene(skinned_first: bool) -> LabScene {
        let pilot = |x: f32, skinned: bool| {
            let mut mesh = part_quad(0.0, 0.0, 0.5, 0, 0);
            for v in &mut mesh.vertices {
                v.weights = [1.0, 0.0, 0.0, 0.0];
            }
            mesh.model = regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(x, 0.0, 0.0))
                .to_cols_array_2d();
            if skinned {
                mesh.joints = vec![JointMatrix::identity(), JointMatrix::identity()];
            }
            mesh
        };
        let (hand_built, skinned) = (pilot(-0.9, false), pilot(0.9, true));
        let pilots = if skinned_first {
            [skinned, hand_built]
        } else {
            [hand_built, skinned]
        };
        let mut meshes = vec![part_quad(0.0, 0.0, 0.6, 0, 0)];
        meshes.extend(pilots);
        LabScene { meshes, ..lab() }
    }

    /// `pilot_pair_scene` on the GPU-driven path with stable keys: hull `0x10..`, both pilots
    /// mesh `0x50` and material `0x52`, the hand-built pilot object `0x51`, the other `0x61`.
    fn pilot_pair_frame(lab: &LabScene, skinned_first: bool) -> FrameScene<'_> {
        let mut scene = lab.as_frame_scene_with_quality(FrameQuality {
            gpu_driven: true,
            ..FrameQuality::default()
        });
        let (hand_built, skinned) = if skinned_first { (2, 1) } else { (1, 2) };
        [
            scene.meshes[0].mesh_key,
            scene.meshes[0].object_key,
            scene.meshes[0].material_key,
        ] = [0x10, 0x11, 0x12];
        [
            scene.meshes[hand_built].mesh_key,
            scene.meshes[hand_built].object_key,
            scene.meshes[hand_built].material_key,
        ] = [0x50, 0x51, 0x52];
        [
            scene.meshes[skinned].mesh_key,
            scene.meshes[skinned].object_key,
            scene.meshes[skinned].material_key,
        ] = [0x50, 0x61, 0x52];
        scene
    }

    /// The DO27 repro (v0.1.13 `expect("object entry")`, 10-16): a hand-built pilot drawn rigid,
    /// then a pilot the sim spawned drawing the same mesh skinned. With the v0.1.13 mesh cache the
    /// skinned visit clears the mesh's cull bound after the rigid draw was collected for the
    /// GPU-driven batches; the GPU cull then drops it, and the per-object colour loop met a draw
    /// with no object entry (v0.1.13 panicked, later versions skipped it: the pilot vanished).
    /// The draw must be drawn per object, the same picture as with today's cache (ADR-4577),
    /// and with today's cache the cull takes it. Submitted the other way round the static visit
    /// comes last and nothing is missed.
    #[test]
    fn a_hand_built_pilot_beside_a_skinned_one_always_draws() {
        let device = device();
        if !(device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance) {
            eprintln!("skip: the device draws no GPU objects (no multi-draw indirect)");
            return;
        }
        for skinned_first in [false, true] {
            let lab = pilot_pair_scene(skinned_first);
            let scene = pilot_pair_frame(&lab, skinned_first);
            let without_hand_built = FrameScene {
                meshes: scene
                    .meshes
                    .iter()
                    .copied()
                    .filter(|mesh| mesh.object_key != 0x51)
                    .collect(),
                ..scene.clone()
            };
            let mut renderer = FrameRenderer::new();
            let absent = renderer
                .render_offscreen(&device, &without_hand_built)
                .expect("frame")
                .pixels;
            let mut current = FrameRenderer::new();
            let mut legacy = FrameRenderer::new();
            legacy.entries.legacy_skinned_visit_clears_bound = true;
            for frame in 0..3 {
                let today = current.render_offscreen(&device, &scene).expect("frame").pixels;
                let integrity = current.draw_integrity();
                assert_eq!(
                    (
                        integrity.skipped_draws,
                        integrity.objects_rebuilt,
                        integrity.cull_missed_draws
                    ),
                    (0, 0, 0),
                    "skinned_first {skinned_first} frame {frame}: today's cache keeps the bound {integrity:?}"
                );
                assert_ne!(
                    today, absent,
                    "skinned_first {skinned_first}: the hand-built pilot is drawn"
                );

                let old = legacy.render_offscreen(&device, &scene).expect("no panic").pixels;
                let integrity = legacy.draw_integrity();
                assert_eq!(
                    integrity.skipped_draws, 0,
                    "skinned_first {skinned_first} frame {frame}: v0.1.13 cache, nothing skipped {integrity:?}"
                );
                assert_eq!(integrity.objects_rebuilt, 0, "{integrity:?}");
                assert_eq!(
                    integrity.cull_missed_draws,
                    u64::from(!skinned_first),
                    "skinned_first {skinned_first} frame {frame}: the cull misses the hand-built pilot only \
                     when the skinned visit comes last {integrity:?}"
                );
                let worst = old
                    .iter()
                    .zip(&today)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap_or(0);
                assert!(
                    worst <= 1,
                    "skinned_first {skinned_first} frame {frame}: drawn per object, the same picture \
                     (worst channel difference {worst})"
                );
            }
            let integrity = legacy.draw_integrity();
            assert_eq!(integrity.cull_missed_draws_total, 3 * u64::from(!skinned_first));
        }
    }

    /// A removal deferred while the window is open waits for its close; outside a window the
    /// caller removes at once.
    #[test]
    fn removals_inside_the_window_wait_for_its_close() {
        let mut renderer = FrameRenderer::new();
        assert!(!renderer.entries.defer_mesh_removal(7), "no window: remove now");
        renderer.open_draw_window();
        assert!(renderer.entries.defer_mesh_removal(7));
        assert_eq!(renderer.entries.deferred_meshes, vec![7]);
        renderer.close_draw_window(false);
        assert!(renderer.entries.deferred_meshes.is_empty());
        let integrity = renderer.draw_integrity();
        assert_eq!(integrity.deferred_removals, 1);
        assert_eq!(integrity.deferred_removals_total, 1);
        // A frame that returned early: the next open applies what it left.
        renderer.open_draw_window();
        assert!(renderer.entries.defer_mesh_removal(8));
        renderer.open_draw_window();
        assert!(renderer.entries.deferred_meshes.is_empty());
    }
}
