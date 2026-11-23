// SPDX-License-Identifier: MPL-2.0
//! The shadow plan of `FrameRenderer::render_into` after the CPU plan (ADR-1552 split, line m):
//! the cascade and atlas targets, each caster's bounds and cascades, the translucent casters,
//! the page and atlas-tile caches, then the frame's shadow jobs and each job's caster list.
//! Moved out of `frame.rs` unchanged; `&mut self` methods because they keep the shadow caches.

use super::*;

/// Page-cache id base of the shared draw's dynamic shadow boxes (ADR-2028), one id per box index.
const SHARED_DYNAMIC_CASTER_ID: u64 = 0x5A4E_D0D7_0000_0000;
/// Caster id of a swaying persistent set's box in the dynamic class (ADR-3916).
const SWAY_SET_CASTER_ID: u64 = 0x5357_4159_5345_5400;

/// One dynamic-class caster for a world box, or `None` when the box must not enter the page cache.
///
/// A non-finite corner or an inverted box is refused. [`crate::paged_shadow::CasterPose::displacement_bound`]
/// returns infinity for a non-finite pose, and the cache then counts that caster as moved on every
/// update, so the pages it covers stay dirty. Shared grass boxes and swaying persistent sets both
/// go through here.
pub(crate) fn admit_dynamic_shadow_box(
    id: u64,
    min: [f32; 3],
    max: [f32; 3],
    cascades: u8,
) -> Option<crate::paged_shadow::ShadowCaster> {
    let finite = min.iter().chain(max.iter()).all(|v| v.is_finite());
    if !finite || (0..3).any(|axis| max[axis] < min[axis]) {
        return None;
    }
    let center = [0, 1, 2].map(|axis| 0.5 * (min[axis] + max[axis]));
    let half = [0, 1, 2].map(|axis| 0.5 * (max[axis] - min[axis]));
    Some(crate::paged_shadow::ShadowCaster {
        is_static: false,
        id,
        aabb: crate::paged_shadow::Aabb3 { min, max },
        cascades,
        pose: crate::paged_shadow::CasterPose {
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            center,
            radius: (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt(),
        },
        dynamic: true,
    })
}

/// What the casters' plan reads from the frame.
pub(super) struct ShadowCasterInputs<'a> {
    /// The frame's scene (shadow switches, quality, eye).
    pub(super) scene: &'a FrameScene<'a>,
    /// The CPU shadow plan; the near sun tile may be dropped and the cascade counts are set.
    pub(super) plan: &'a mut shadow_plan::ShadowPlan,
    /// The local lights uploaded this frame.
    pub(super) uploaded: &'a [crate::LabLocalLight],
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the shadow casters.
    pub(super) shadow_ix: &'a [usize],
    /// Instanced (storage-object) draws are on.
    pub(super) gpu_objects: bool,
    /// The visibility-buffer sets cast cluster shadows this frame.
    pub(super) vis_shadow_state: bool,
}

/// The casters' plan: targets, caster bounds and cascades, translucent casters, cache report.
pub(super) struct ShadowCasters {
    /// Sun cascades drawn this frame (0 without directional shadows).
    pub(super) active_cascades: u32,
    /// Sun cascade map edge in texels.
    pub(super) csm_size: u32,
    /// `active_cascades` as an index bound.
    pub(super) cascade_count: usize,
    /// Paged sun shadows are on.
    pub(super) paging: bool,
    /// Sun cascade split distances.
    pub(super) csm_splits: Vec<f32>,
    /// `(world aabb, cascade mask)` per shadow caster.
    pub(super) caster_info: Vec<(Option<crate::paged_shadow::Aabb3>, u8)>,
    /// Indices of the translucent casters.
    pub(super) trans_ix: Vec<usize>,
    /// Translucent sun cascades drawn this frame.
    pub(super) trans_cascades: u32,
    /// The translucent atlas pair is live.
    pub(super) trans_local: bool,
    /// `(world aabb, cascade mask)` per translucent caster.
    pub(super) trans_info: Vec<(Option<crate::paged_shadow::Aabb3>, u8)>,
    /// Visibility-buffer casters draw as clusters this frame.
    pub(super) vis_shadows: bool,
    /// Each caster's footprint per cascade.
    pub(super) cascade_uv_lists: scratch::ListOfLists<scratch::Footprint>,
    /// Light view-projection per drawn cascade; the first `vp_count` are set.
    pub(super) cascade_vps: [[[f32; 4]; 4]; crate::forward::CASCADE_COUNT as usize],
    /// Drawn cascades with a matrix in `cascade_vps`.
    pub(super) vp_count: usize,
    /// Bounded casters for the page and atlas caches.
    pub(super) casters: Vec<crate::paged_shadow::ShadowCaster>,
    /// `caster_info` index of each entry of `casters`.
    pub(super) caster_slot: Vec<usize>,
    /// The shadow cache report so far.
    pub(super) report: ShadowCacheReport,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// Per cascade: whole-cascade redraw for meshes.
    pub(super) mesh_full: Vec<bool>,
    /// Per cascade: the UV union of the mesh pages redrawn otherwise.
    pub(super) shadow_regions: [[f32; 4]; 4],
}

/// What the shadow jobs and caster lists read from the frame.
pub(super) struct ShadowJobInputs<'a> {
    /// The frame's scene (shadow switches).
    pub(super) scene: &'a FrameScene<'a>,
    /// The CPU shadow plan.
    pub(super) plan: &'a shadow_plan::ShadowPlan,
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the shadow casters.
    pub(super) shadow_ix: &'a [usize],
    /// `(world aabb, cascade mask)` per shadow caster.
    pub(super) caster_info: &'a [(Option<crate::paged_shadow::Aabb3>, u8)],
    /// Each caster's footprint per cascade.
    pub(super) cascade_uv: &'a [Vec<scratch::Footprint>],
    /// Light view-projection per drawn cascade.
    pub(super) vps: &'a [[[f32; 4]; 4]],
    /// Sun cascade map edge in texels.
    pub(super) csm_size: u32,
    /// Sun cascades drawn this frame, as an index bound.
    pub(super) cascade_count: usize,
    /// Paged sun shadows are on.
    pub(super) paging: bool,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// Per cascade: whole-cascade redraw for meshes.
    pub(super) mesh_full: &'a [bool],
    /// Visibility-buffer casters draw as clusters this frame.
    pub(super) vis_shadows: bool,
}

/// Cache key of the "every non-cluster caster" list. Cascades are `0..4`, so `u32::MAX`
/// does not collide with a sun page `(cascade, page)`.
const LOCAL_CASTER_KEY: (u32, Option<(u32, u32)>) = (u32::MAX, None);
/// Near-sun tile list. The page slot distinguishes it from the local-light list.
const NEAR_CASTER_KEY: (u32, Option<(u32, u32)>) = (u32::MAX, Some((0, 0)));
/// Precipitation-tile list.
const PRECIP_CASTER_KEY: (u32, Option<(u32, u32)>) = (u32::MAX, Some((0, 1)));

/// One cached caster list. `stamp_matches` is false when the filter's extra input (the near
/// rectangle, the precipitation view) changed; the entry is dropped and built again. The
/// caller still clears every list when the caster set itself is stale.
fn cached_caster_list(
    cache: &mut CasterListCache,
    key: (u32, Option<(u32, u32)>),
    stamp_matches: bool,
    rebuilt: &mut u32,
    build: impl FnOnce() -> std::sync::Arc<[usize]>,
) -> std::sync::Arc<[usize]> {
    if !stamp_matches {
        cache.lists.remove(&key);
    }
    if let Some(list) = cache.lists.get(&key) {
        return std::sync::Arc::clone(list);
    }
    *rebuilt += 1;
    let list = build();
    cache.lists.insert(key, std::sync::Arc::clone(&list));
    list
}

/// Rest-pose bounds of a static caster. A skinned draw ignores the stored rest pose (the joints
/// have moved the vertices); the caller falls back to its joint-palette bound (`Draw::skin_bound`),
/// and a caster with neither is drawn into every cascade.
fn static_caster_bounds(draw: &Draw, meshes: &crate::key_hash::KeyMap<u64, MeshEntry>) -> Option<MeshBounds> {
    if !draw.rigid {
        return None;
    }
    meshes.get(&draw.mesh).and_then(|entry| entry.bounds)
}

/// The frame's shadow jobs and each job's caster list.
pub(super) struct ShadowJobs {
    /// The shadow jobs (cascades, pages, atlas tiles).
    pub(super) shadow_jobs: Vec<ShadowJob>,
    /// Draw indices per job.
    pub(super) job_casters: Vec<std::sync::Arc<[usize]>>,
}

impl FrameRenderer {
    /// Allocates the shadow targets and builds the casters' bounds, cascades, translucent
    /// casters and the page and atlas-tile cache report.
    pub(super) fn plan_shadow_casters(
        &mut self,
        d: &crate::TrackedDevice,
        inputs: ShadowCasterInputs<'_>,
    ) -> ShadowCasters {
        let ShadowCasterInputs {
            scene,
            plan,
            uploaded,
            draws,
            shadow_ix,
            gpu_objects,
            vis_shadow_state,
        } = inputs;
        // ---- Shadow targets ----
        let active_cascades = if scene.directional_shadows {
            scene
                .quality
                .csm_cascade_count
                .clamp(1, crate::forward::CASCADE_COUNT)
        } else {
            0
        };
        let csm_size = if active_cascades > 0 {
            scene.quality.csm_resolution.max(16)
        } else {
            4
        };
        if self
            .csm
            .as_ref()
            .is_none_or(|(size, active, _)| *size != csm_size || *active != active_cascades)
        {
            // One texture per cascade: a cascade's clear must not erase its
            // sibling (one texture with several views shares storage). Inactive
            // cascade slots bind a 4² dummy.
            let mut views = Vec::with_capacity(4);
            for _ in 0..active_cascades {
                let (_, view) = depth_target(d, csm_size, csm_size, "csm");
                views.push(view);
            }
            let (_, dummy) = depth_target(d, 4, 4, "csm dummy");
            while views.len() < 4 {
                views.push(dummy.clone());
            }
            self.csm = Some((csm_size, active_cascades, views));
            self.csm_valid = false;
            self.sun_pages = crate::paged_shadow::SunPageCache::new(
                active_cascades.max(1),
                crate::paged_shadow::SUN_PAGES_PER_AXIS,
            );
        }
        if self
            .atlas
            .as_ref()
            .is_none_or(|(size, _)| *size != plan.atlas_size)
        {
            let (_, view) = depth_target(d, plan.atlas_size, plan.atlas_size, "shadow atlas");
            self.atlas = Some((plan.atlas_size, view));
            self.atlas_valid = false;
            self.atlas_tiles = crate::paged_shadow::AtlasTileCache::new();
        }

        // Page invalidation before any pipeline borrow: a moving caster dirties
        // only the pages its old and new bounds cover. A clean cascade is omitted.
        let cascade_count = active_cascades as usize;
        let paging = scene.quality.paged_sun_shadows;
        // Per caster (parallel to `shadow_ix`): world bounds and the sun cascades it is drawn
        // into ([`ShadowCast`]). A bounded skinned caster uses its palette bound (ADR-5418);
        // unbounded casters (skins without one, compute twins) go everywhere.
        let mut csm_splits = self.scratch.csm_splits.take();
        csm_splits.extend(
            plan.csm
                .cascades
                .iter()
                .take(cascade_count)
                .map(|c| c.split_distance),
        );
        let eye_v = regia_core::glam::Vec3::from_array(scene.eye);
        let mut caster_info = self.scratch.caster_info.take();
        caster_info.extend(shadow_ix.iter().map(|&index| {
            let draw = &draws[index];
            match static_caster_bounds(draw, &self.meshes).or(draw.skin_bound) {
                Some(bounds) => {
                    // The instanced shadow path draws the caster with its shadow model.
                    let model = if gpu_objects {
                        shadow_model(draw.model, draw.shadow)
                    } else {
                        draw.model
                    };
                    let mut aabb = world_caster_aabb(bounds, model);
                    // A vertex-stage graph's offset can move the caster past its mesh bound.
                    for axis in 0..3 {
                        aabb.min[axis] -= draw.inflate;
                        aabb.max[axis] += draw.inflate;
                    }
                    let c = regia_core::glam::Vec3::from_array([
                        (aabb.min[0] + aabb.max[0]) * 0.5,
                        (aabb.min[1] + aabb.max[1]) * 0.5,
                        (aabb.min[2] + aabb.max[2]) * 0.5,
                    ]);
                    let radius = (aabb.max[0] - aabb.min[0]) * 0.5;
                    let centre = match draw.shadow {
                        ShadowCast::Local { anchor: Some(a), .. } => regia_core::glam::Vec3::from_array(a),
                        _ => c,
                    };
                    (
                        Some(aabb),
                        cascade_mask(draw.shadow, centre.distance(eye_v), radius, &csm_splits),
                    )
                }
                None => (None, u8::MAX),
            }
        }));
        // The near sun tile is skipped entirely (no draw, no lookup) when no mesh lies within its
        // range of the eye (ADR-164). It is the last tile, so dropping it keeps every local
        // light's tile index.
        if let Some(near_index) = plan.near_tile {
            let range = crate::NEAR_SUN_RANGE_M;
            // The casters' world bounds are already at hand (`caster_info`): a mesh that casts
            // no sun shadow (rare on a hull) does not wake the tile on its own.
            let live = caster_info.iter().any(|&(aabb, _)| {
                aabb.is_some_and(|aabb| {
                    let d2: f32 = (0..3)
                        .map(|axis| {
                            let e = scene.eye[axis];
                            let gap = e - e.clamp(aabb.min[axis], aabb.max[axis]);
                            gap * gap
                        })
                        .sum();
                    d2 < range * range
                })
            });
            if !live {
                plan.tiles.truncate(near_index);
                plan.near_tile = None;
                self.near_tile_vp = None;
            }
        }
        plan.csm.count[3] = plan.near_tile.map_or(0.0, |index| (index + 1) as f32);
        // Translucent casters (alpha-blended and transmissive; additive light adds, never
        // blocks): they shadow by their transmittance through the translucency maps
        // (`translucent_shadow`), off-screen ones included. Each gets its bounds and cascades
        // like an opaque caster.
        // Hair cards cast through the translucency maps' alpha test; a tier without those maps
        // (Mobile, Desktop Low) draws them shadowless rather than as opaque card outlines.
        let hair_shadows =
            scene.quality.translucent_shadow_cascades > 0 || scene.quality.translucent_local_shadows;
        let mut trans_ix = self.scratch.trans_ix.take();
        trans_ix.extend(
            draws
                .iter()
                .enumerate()
                .filter(|(_, draw)| {
                    casts_translucent_shadow(draw.alpha_mode, draw.shadow, draw.over)
                        || (draw.hair
                            && hair_shadows
                            && casts_translucent_shadow(ALPHA_BLEND, draw.shadow, draw.over))
                })
                .map(|(index, _)| index),
        );
        let trans_cascades = if scene.directional_shadows && !trans_ix.is_empty() {
            scene.quality.translucent_shadow_cascades.min(active_cascades)
        } else {
            0
        };
        let trans_local = scene.quality.translucent_local_shadows
            && !trans_ix.is_empty()
            && !plan.tiles.is_empty()
            && plan.atlas_size > 0;
        let mut trans_info = self.scratch.trans_info.take();
        trans_info.extend(trans_ix.iter().map(|&index| {
            let draw = &draws[index];
            match static_caster_bounds(draw, &self.meshes) {
                Some(bounds) => {
                    let mut aabb = world_caster_aabb(bounds, draw.model);
                    for axis in 0..3 {
                        aabb.min[axis] -= draw.inflate;
                        aabb.max[axis] += draw.inflate;
                    }
                    let c = regia_core::glam::Vec3::new(
                        (aabb.min[0] + aabb.max[0]) * 0.5,
                        (aabb.min[1] + aabb.max[1]) * 0.5,
                        (aabb.min[2] + aabb.max[2]) * 0.5,
                    );
                    let radius = (aabb.max[0] - aabb.min[0]) * 0.5;
                    let centre = match draw.shadow {
                        ShadowCast::Local { anchor: Some(a), .. } => regia_core::glam::Vec3::from_array(a),
                        _ => c,
                    };
                    (
                        Some(aabb),
                        cascade_mask(draw.shadow, centre.distance(eye_v), radius, &csm_splits),
                    )
                }
                None => (None, u8::MAX),
            }
        }));
        // Placeholders bind until the first translucent caster; the maps then stay allocated.
        if let Some(pipes) = self.pipes.as_ref() {
            self.translucent_maps.ensure_base(d, &pipes.translucent);
        }
        if trans_cascades > 0 {
            self.translucent_maps
                .ensure_csm(d, translucent_shadow::csm_edge(csm_size), trans_cascades);
        }
        if trans_local {
            self.translucent_maps
                .ensure_atlas(d, translucent_shadow::atlas_edge(plan.atlas_size));
        }
        // The receiver's switches: y = translucent cascades, z = the atlas pair is live.
        plan.csm.count[1] = trans_cascades as f32;
        plan.csm.count[2] = if trans_local { 1.0 } else { 0.0 };
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.csm_bytes,
            self.csm_buf.as_ref().expect("csm buffer"),
            bytemuck::bytes_of(&plan.csm),
        );
        // Visibility-buffer casters draw as clusters: the cull reads each one's cascade choice.
        let vis_shadows = vis_shadow_state && self.visbuffer.instance_count() > 0;
        if vis_shadows {
            for (slot, &index) in shadow_ix.iter().enumerate() {
                if let Some(instance) = draws[index].vis_instance {
                    self.visbuffer.set_shadow_mask(instance, caster_info[slot].1);
                }
            }
        }
        // Each caster's light-space footprint (shadow-map UV min and max) per cascade,
        // projected once: the page cache and the per-page caster lists both use it.
        // A caster's light-space footprint (`scratch::Footprint`): shadow-map UV min and max.
        let mut cascade_uv_lists = self.scratch.cascade_uv.take();
        // One task per cascade instead of the loop below (ADR-2074): the same lists.
        let parallel_plan = scene.quality.frame_pipelining.effective().parallel_shadow_plan
            && crate::paged_shadow::worth_a_task_per_cascade(caster_info.len());
        if parallel_plan {
            let mut vps = [[[0.0f32; 4]; 4]; crate::forward::CASCADE_COUNT as usize];
            let count = cascade_count.min(vps.len());
            for (slot, cascade) in vps.iter_mut().zip(&plan.csm.cascades).take(count) {
                *slot = cascade.light_view_proj;
            }
            let lists = cascade_uv_lists.push_lists(count);
            crate::paged_shadow::project_footprints_parallel(lists, &vps[..count], &caster_info);
        }
        let serial_cascades = if parallel_plan { 0 } else { cascade_count };
        for (k, cascade) in plan.csm.cascades.iter().take(serial_cascades).enumerate() {
            // A caster outside this cascade's mask is never looked up here (the page cache
            // and the job lists test the mask first), so it is not projected.
            let bit = 1u8 << k.min(7);
            cascade_uv_lists
                .push_list()
                .extend(caster_info.iter().map(|&(aabb, mask)| {
                    aabb.filter(|_| mask & bit != 0).map(|aabb| {
                        crate::paged_shadow::project_aabb_to_shadow_uv(aabb, cascade.light_view_proj)
                    })
                }));
        }
        let cascade_uv = cascade_uv_lists.lists();
        // Light view-projection per drawn cascade (at most `CASCADE_COUNT`, no allocation).
        let mut cascade_vps = [[[0.0f32; 4]; 4]; crate::forward::CASCADE_COUNT as usize];
        let vp_count = cascade_count.min(plan.csm.cascades.len()).min(cascade_vps.len());
        for (slot, cascade) in cascade_vps.iter_mut().zip(&plan.csm.cascades).take(vp_count) {
            *slot = cascade.light_view_proj;
        }
        let vps = &cascade_vps[..vp_count];
        let mut casters = self.scratch.casters.take();
        // `caster_info` index of each entry of `casters`.
        let mut caster_slot = self.scratch.caster_slot.take();
        let mut unbounded = false;
        // The occlusion tile's cache key reads the casters on unpaged frames too.
        if paging || plan.precip_tile.is_some() {
            for (slot, (index, info)) in shadow_ix.iter().zip(&caster_info).enumerate() {
                let draw = &draws[*index];
                match info.0 {
                    Some(aabb) => {
                        // The same model and bounds as `caster_info`'s box: the box misses a
                        // turn about the sphere's centre, the pose does not.
                        let model = if gpu_objects {
                            shadow_model(draw.model, draw.shadow)
                        } else {
                            draw.model
                        };
                        let bounds = static_caster_bounds(draw, &self.meshes).or(draw.skin_bound);
                        // Bytes rewritten in place make a new caster id: the old one's pages
                        // and the new one's are redrawn, as for a mesh swap (ADR-5594).
                        let content_rev = self.meshes.get(&draw.mesh).map_or(0, |mesh| mesh.content_rev);
                        casters.push(crate::paged_shadow::ShadowCaster {
                            id: super::shadow_plan::caster_content_id(
                                shadow_caster_id(draw.object, draw.mesh, draw.shadow),
                                content_rev,
                            ),
                            aabb,
                            cascades: info.1,
                            pose: crate::paged_shadow::CasterPose {
                                model,
                                center: bounds.map_or([0.0; 3], |b| b.center),
                                radius: bounds.map_or(0.0, |b| b.radius),
                            },
                            dynamic: draw.animated,
                            is_static: draw.is_static,
                        });
                        caster_slot.push(slot);
                    }
                    None => unbounded = true,
                }
            }
        }
        // Dynamic-caster class (ADR-2028): casters whose vertex stage reads the clock move
        // inside their bounds, so their pages are refreshed in the policy's cascades.
        let dynamic_policy = self.dynamic_casters.resolve(&scene.quality);
        // Shared-draw content that sways in its shadow pass (grass) joins the class as one
        // dynamic caster per reported box, in the refreshed cascades it draws into. Its slot is
        // past `caster_info`, so its footprint is projected here rather than looked up. The
        // atlas cache stops short of these boxes: shared geometry draws into cascades only.
        let mesh_casters = casters.len();
        let mut shared_dynamic = 0u32;
        if paging && let Some(shared) = self.shared_draw.as_ref() {
            let box_cascades = shared.dynamic_shadow_box_cascades() & dynamic_policy.cascade_mask;
            let boxes = if box_cascades != 0 {
                shared.dynamic_shadow_boxes()
            } else {
                &[]
            };
            for (i, &[min, max]) in boxes.iter().enumerate() {
                let Some(caster) = admit_dynamic_shadow_box(
                    SHARED_DYNAMIC_CASTER_ID ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    min,
                    max,
                    box_cascades,
                ) else {
                    continue;
                };
                casters.push(caster);
                caster_slot.push(usize::MAX);
                shared_dynamic += 1;
            }
        }
        // A persistent set that sways moves inside its pages too, and no draw names them (ADR-3916).
        // Its clusters draw in every job that covers them, so its box joins the class.
        if paging && dynamic_policy.cascade_mask != 0 {
            for (i, &[min, max]) in self.visbuffer.sway_set_boxes().iter().enumerate() {
                let Some(caster) = admit_dynamic_shadow_box(
                    SWAY_SET_CASTER_ID ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    min,
                    max,
                    dynamic_policy.cascade_mask,
                ) else {
                    continue;
                };
                casters.push(caster);
                caster_slot.push(usize::MAX);
            }
        }
        let mut report = ShadowCacheReport {
            near_sun_tile: plan.near_tile.is_some(),
            // Static-marked casters admitted to the page caches (`specs/14` A1): the count
            // proves the marker reaches the caches; their keep test is strict (see
            // `paged_shadow::decide`).
            static_casters: casters.iter().filter(|caster| caster.is_static).count() as u32,
            dynamic: crate::foliage_wind::DynamicCasterReport {
                policy: dynamic_policy,
                animated_casters: shadow_ix.iter().filter(|&&index| draws[index].animated).count() as u32,
                skinned_casters: shadow_ix
                    .iter()
                    .filter(|&&index| draws[index].animated && draws[index].skin_bound.is_some())
                    .count() as u32,
                shared_boxes: shared_dynamic,
                ..Default::default()
            },
            sun_casters_per_cascade: sun_casters_per_cascade(
                &caster_info,
                if scene.directional_shadows {
                    cascade_count
                } else {
                    0
                },
            ),
            ..ShadowCacheReport::default()
        };
        if paging && scene.directional_shadows {
            self.sun_pages
                .set_parallel(scene.quality.frame_pipelining.effective().parallel_shadow_plan);
            let update = self.sun_pages.update_with_footprints(
                &casters,
                vps,
                dynamic_policy.cascade_mask,
                |cascade, index| {
                    cascade_uv
                        .get(cascade)
                        .and_then(|uv| uv.get(caster_slot.get(index).copied()?).copied().flatten())
                        .unwrap_or_else(|| {
                            crate::paged_shadow::project_aabb_to_shadow_uv(casters[index].aabb, vps[cascade])
                        })
                },
            );
            report.dirty_pages = update.dirty;
            report.cascade_full_redraw = update.cascade_full_redraw;
            report.caster_events = update.caster_events;
            report.dynamic.dynamic_pages = update.dynamic_pages;
            if unbounded || self.had_unbounded_caster {
                report.cascade_full_redraw = vec![true; cascade_count];
            }
            self.had_unbounded_caster = unbounded;
        }
        if paging {
            let mut lights = Vec::new();
            for &index in &plan.shadowed {
                let Some(light) = uploaded.get(index) else {
                    continue;
                };
                match light {
                    crate::LabLocalLight::Point(point) => lights.push(crate::paged_shadow::AtlasLight {
                        id: index as u64,
                        center: point.position,
                        radius: point.radius,
                    }),
                    crate::LabLocalLight::Spot(spot) => lights.push(crate::paged_shadow::AtlasLight {
                        id: index as u64,
                        center: spot.position,
                        radius: spot.range,
                    }),
                }
            }
            let (tiles, dynamic_tiles) = self.atlas_tiles.update_refreshing(
                &lights,
                &casters[..mesh_casters],
                dynamic_policy.local_lights,
            );
            report.dirty_atlas_tiles = tiles;
            report.dynamic.dynamic_atlas_tiles = dynamic_tiles;
        }
        self.weather.dirty = self.precip_tile_dirty(plan, &casters, unbounded, paging);
        self.bind_shared_precip_atlas();
        let samples = if scene.quality.msaa_samples >= 4 { 4 } else { 1 };
        // Per cascade: whole-cascade redraw for meshes, and the UV union of the mesh pages
        // that will be redrawn otherwise (what the shared draw may limit itself to).
        let mesh_full: Vec<bool> = (0..cascade_count)
            .map(|i| !paging || !self.csm_valid || report.cascade_full_redraw.get(i).copied().unwrap_or(true))
            .collect();
        let mut shadow_regions = [[0.0f32; 4]; 4];
        let page_n = self.sun_pages.pages_per_axis().max(1) as f32;
        for (i, region) in shadow_regions.iter_mut().enumerate().take(cascade_count) {
            if mesh_full[i] {
                *region = [0.0, 0.0, 1.0, 1.0];
                continue;
            }
            let mut r = [1.0f32, 1.0, 0.0, 0.0];
            for page in report.dirty_pages.iter().filter(|page| page.cascade == i as u32) {
                r[0] = r[0].min(page.x as f32 / page_n);
                r[1] = r[1].min(page.y as f32 / page_n);
                r[2] = r[2].max((page.x + 1) as f32 / page_n);
                r[3] = r[3].max((page.y + 1) as f32 / page_n);
            }
            *region = if r[2] > r[0] { r } else { [0.0; 4] };
        }
        ShadowCasters {
            active_cascades,
            csm_size,
            cascade_count,
            paging,
            csm_splits,
            caster_info,
            trans_ix,
            trans_cascades,
            trans_local,
            trans_info,
            vis_shadows,
            cascade_uv_lists,
            cascade_vps,
            vp_count,
            casters,
            caster_slot,
            report,
            samples,
            mesh_full,
            shadow_regions,
        }
    }

    /// Builds the frame's shadow jobs (whole cascades, dirty pages, atlas tiles) and each job's
    /// caster list (cached per page), and adds to the cache report.
    pub(super) fn build_shadow_jobs(
        &mut self,
        inputs: ShadowJobInputs<'_>,
        report: &mut ShadowCacheReport,
        shadow_cpu: &mut ShadowCpuTimings,
    ) -> ShadowJobs {
        let ShadowJobInputs {
            scene,
            plan,
            draws,
            shadow_ix,
            caster_info,
            cascade_uv,
            vps,
            csm_size,
            cascade_count,
            paging,
            samples,
            mesh_full,
            vis_shadows,
        } = inputs;
        // A retained cascade whose shared geometry changed (terrain LOD under it, a new
        // node in its box) is cleared and redrawn with its meshes, so no stale depth stays.
        let shared_changed: Vec<bool> = (0..cascade_count)
            .map(|i| {
                samples == 1
                    && self.shared_draw.as_ref().is_some_and(|draw| {
                        draw.cascade_shadow_key(i as u32).is_some_and(|key| {
                            self.shared_shadow_keys[i] != Some((plan.csm.cascades[i].light_view_proj, key))
                        })
                    })
            })
            .collect();
        let mut shadow_jobs = self.scratch.shadow_jobs.take();
        // Under a traced sun, cascades nothing reads are not drawn (ADR-2310; all kept otherwise).
        let keep_sun = self.sun_map_keep_mask(scene, &plan.csm, draws);
        if scene.directional_shadows {
            for i in 0..cascade_count {
                if keep_sun & (1 << i) == 0 {
                    continue;
                }
                let full = mesh_full[i] || shared_changed[i];
                if full {
                    shadow_jobs.push(ShadowJob {
                        slot: i,
                        kind: i as u8,
                        rect: [0, 0, csm_size, csm_size],
                        page: None,
                        page_clear: false,
                    });
                } else {
                    let jobs_before = shadow_jobs.len();
                    for page in report.dirty_pages.iter().filter(|page| page.cascade == i as u32) {
                        shadow_jobs.push(ShadowJob {
                            slot: i,
                            kind: i as u8,
                            rect: crate::paged_shadow::page_texel_rect(
                                page.x,
                                page.y,
                                self.sun_pages.pages_per_axis(),
                                csm_size,
                            ),
                            page: Some((page.x, page.y)),
                            page_clear: true,
                        });
                    }
                    if shadow_jobs.len() == jobs_before {
                        report.cascades_skipped += 1;
                    }
                }
            }
        }
        // Paging off, or a cache that was just created, clears the whole atlas.
        // A warm cache redraws only the tiles the CPU report marked dirty.
        let atlas_full = !plan.tiles.is_empty() && (!paging || !self.atlas_valid);
        if atlas_full {
            // The occlusion tile keeps its depth between frames on unpaged frames too: it stays
            // out of the job list while its key holds, and the other tiles clear only themselves.
            let keep_precip = plan.precip_tile.filter(|_| !self.weather.dirty);
            for (i, tile) in plan.tiles.iter().enumerate() {
                if Some(i) == keep_precip {
                    continue;
                }
                shadow_jobs.push(ShadowJob {
                    slot: cascade_count + i,
                    kind: 4,
                    rect: [tile.x, tile.y, tile.size, tile.size],
                    page: None,
                    page_clear: plan.precip_tile.is_some(),
                });
            }
            report.atlas_tiles_drawn = (plan.tiles.len() - usize::from(keep_precip.is_some())) as u32;
            report.near_sun_tile_drawn = plan.near_tile.is_some();
        } else if paging && !plan.tiles.is_empty() {
            if let Some(index) = plan.precip_tile.filter(|_| self.weather.dirty) {
                let tile = &plan.tiles[index];
                shadow_jobs.push(ShadowJob {
                    slot: cascade_count + index,
                    kind: 4,
                    rect: [tile.x, tile.y, tile.size, tile.size],
                    page: None,
                    page_clear: true,
                });
                report.atlas_tiles_drawn += 1;
            }
            // The near sun tile follows cascade 0: redrawn when its matrix moved (a camera step
            // over one of its texels), when cascade 0 has a dirty page (a caster moved, which
            // covers every caster over the near receivers), or when it held nothing valid.
            if let Some(near_index) = plan.near_tile.filter(|_| keep_sun & 1 != 0) {
                let tile = &plan.tiles[near_index];
                let cascade0_dirty = mesh_full.first().copied().unwrap_or(true)
                    || shared_changed.first().copied().unwrap_or(false)
                    || report.dirty_pages.iter().any(|page| page.cascade == 0);
                if cascade0_dirty || self.near_tile_vp != Some(tile.gpu.view_proj) {
                    report.near_sun_tile_drawn = true;
                    shadow_jobs.push(ShadowJob {
                        slot: cascade_count + near_index,
                        kind: 4,
                        rect: [tile.x, tile.y, tile.size, tile.size],
                        page: None,
                        page_clear: true,
                    });
                    report.atlas_tiles_drawn += 1;
                }
            }
            if report.dirty_atlas_tiles.is_empty() {
                report.atlas_skipped = true;
            } else {
                for &index in &report.dirty_atlas_tiles {
                    // The cache names dirty lights; a light is its tiles (a point light's six
                    // faces). Only the faces a moved box reaches are redrawn (ADR-7986). A plan
                    // that names no light tiles keeps the index as a tile index.
                    let tiles = match plan.light_faces.get(index as usize) {
                        Some(faces) => faces.tiles.clone(),
                        None => index as usize..index as usize + 1,
                    };
                    let named = plan.light_faces.get(index as usize).is_some();
                    for tile_index in tiles {
                        let Some(tile) = plan.tiles.get(tile_index) else {
                            continue;
                        };
                        let view_proj = tile.gpu.view_proj;
                        if named
                            && !self
                                .atlas_tiles
                                .face_dirty(index as usize, |aabb, center, radius| {
                                    super::shadow_plan::face_sees_caster(
                                        Some(aabb),
                                        center,
                                        radius,
                                        &view_proj,
                                    )
                                })
                        {
                            report.atlas_faces_kept += 1;
                            continue;
                        }
                        shadow_jobs.push(ShadowJob {
                            slot: cascade_count + tile_index,
                            kind: 4,
                            rect: [tile.x, tile.y, tile.size, tile.size],
                            page: None,
                            page_clear: true,
                        });
                        report.atlas_tiles_drawn += 1;
                    }
                }
            }
        }
        // Caster list per shadow job: a sun job draws the casters whose cascade mask includes
        // its cascade and whose light-space footprint touches the job's page (or the cascade);
        // a local-light tile that is not the near sun tile or the precipitation tile draws every
        // non-clustered caster, and those tiles share one list. Geometry outside a page is
        // scissored anyway, so the lists change cost, not pixels. Lists are cached until the
        // caster set, the cluster flags or a cascade matrix changes — a held camera rebuilds none.
        if let Some(near_index) = plan.near_tile {
            self.near_tile_vp = Some(plan.tiles[near_index].gpu.view_proj);
        }
        let near_slot = plan.near_tile.map(|index| cascade_count + index);
        let precip_slot = plan.precip_tile.map(|index| cascade_count + index);
        // The near tile's square in cascade 0's shadow UV (`plan.near_sphere` through cascade 0's
        // matrix, half-size = tile radius / cascade-0 radius / 2), for its caster list.
        let near_rect_c0 = match (plan.near_sphere, plan.csm.cascades.first()) {
            (Some((centre, radius)), Some(c0)) => {
                let m = regia_core::glam::Mat4::from_cols_array_2d(&c0.light_view_proj);
                let clip = m * regia_core::glam::Vec3::from_array(centre).extend(1.0);
                let uv = [clip.x * 0.5 + 0.5, 0.5 - clip.y * 0.5];
                let c0_radius = (c0.bias[0] * csm_size as f32 * 0.5).max(1e-3);
                let half = radius / c0_radius * 0.5 + 1e-3;
                ([uv[0] - half, uv[1] - half], [uv[0] + half, uv[1] + half])
            }
            _ => ([0.0, 0.0], [1.0, 1.0]),
        };
        let caster_span = tracing::info_span!("shadow_caster_lists").entered();
        let caster_lists_started = std::time::Instant::now();
        let pages_per_axis = self.sun_pages.pages_per_axis().max(1) as f32;
        // Light view-projection per cascade, for the cache key. The eager `cascade_uv` built
        // above already holds each caster's footprint per cascade, so the per-page lists only
        // filter it against the page (they are cached and rebuilt when the caster set or a
        // cascade matrix changes — a held camera rebuilds none).
        // A caster drawn as clusters this frame stays out of the lists. Masked draws keep their
        // forward shadow draws, which alpha-test (ADR-3915).
        let clustered = |index: usize| vis_shadows && draws[index].vis && !draws[index].vis_masked;
        if self
            .caster_list_cache
            .stale(shadow_ix, caster_info, vps, clustered)
        {
            self.caster_list_cache
                .reset(shadow_ix, caster_info, vps, clustered);
        }
        let mut rebuilt = 0u32;
        // Shared lists: a cached page's list is handed out without a copy.
        let job_casters: Vec<std::sync::Arc<[usize]>> = shadow_jobs
            .iter()
            .map(|job| -> std::sync::Arc<[usize]> {
                if job.kind >= 4 && Some(job.slot) == near_slot {
                    // The near sun tile draws the sun casters that reach cascade 0 and whose
                    // cascade-0 footprint (already projected, `cascade_uv`) touches the tile's
                    // square there: both views share the sun's basis, so the tile is a square of
                    // cascade 0's map around its centre. One list for the rectangle; a moved
                    // rectangle builds it again.
                    let (lo, hi) = near_rect_c0;
                    let same = self.caster_list_cache.near_rect == Some(near_rect_c0);
                    self.caster_list_cache.near_rect = Some(near_rect_c0);
                    return cached_caster_list(
                        &mut self.caster_list_cache,
                        NEAR_CASTER_KEY,
                        same,
                        &mut rebuilt,
                        || {
                            shadow_ix
                                .iter()
                                .enumerate()
                                .filter(|&(i, &index)| {
                                    if clustered(index) {
                                        return false;
                                    }
                                    if caster_info[i].1 & 1 == 0 {
                                        return false;
                                    }
                                    match cascade_uv.first().and_then(|uv| uv[i]) {
                                        Some((min, max)) => {
                                            max[0] >= lo[0]
                                                && min[0] <= hi[0]
                                                && max[1] >= lo[1]
                                                && min[1] <= hi[1]
                                        }
                                        None => true,
                                    }
                                })
                                .map(|(_, &index)| index)
                                .collect()
                        },
                    );
                }
                if job.kind >= 4 && Some(job.slot) == precip_slot {
                    // The occlusion tile draws the casters over its square. One list for this
                    // precipitation view.
                    let same = self.caster_list_cache.precip_plan == plan.precip_plan;
                    self.caster_list_cache.precip_plan = plan.precip_plan;
                    return cached_caster_list(
                        &mut self.caster_list_cache,
                        PRECIP_CASTER_KEY,
                        same,
                        &mut rebuilt,
                        || {
                            shadow_ix
                                .iter()
                                .enumerate()
                                .filter(|&(i, &index)| {
                                    !clustered(index)
                                        && match (caster_info[i].0, plan.precip_plan) {
                                            (Some(aabb), Some(view)) => {
                                                weather::caster_touches_tile(&aabb, &view)
                                            }
                                            _ => true,
                                        }
                                })
                                .map(|(_, &index)| index)
                                .collect()
                        },
                    );
                }
                if job.kind >= 4
                    && let Some(tile_index) = job.slot.checked_sub(cascade_count)
                    && let Some(faces) = plan
                        .light_faces
                        .iter()
                        .find(|faces| faces.tiles.contains(&tile_index))
                    && let Some(tile) = plan.tiles.get(tile_index)
                {
                    // A light's face draws the non-cluster casters that reach the light's sphere
                    // and are not wholly outside the face's side planes (ADR-7986): the others
                    // are clipped by the rasteriser in that face anyway. Cached per tile until
                    // the caster set or the face changes.
                    let stamp = super::shadow_plan::FaceStamp {
                        view_proj: tile.gpu.view_proj,
                        center: faces.center,
                        radius: faces.radius,
                    };
                    if let Some((kept, list)) = self.caster_list_cache.faces.get(&tile_index)
                        && *kept == stamp
                    {
                        return std::sync::Arc::clone(list);
                    }
                    rebuilt += 1;
                    let list: std::sync::Arc<[usize]> = shadow_ix
                        .iter()
                        .enumerate()
                        .filter(|&(i, &index)| {
                            !clustered(index)
                                && super::shadow_plan::face_sees_caster(
                                    caster_info.get(i).and_then(|info| info.0),
                                    faces.center,
                                    faces.radius,
                                    &tile.gpu.view_proj,
                                )
                        })
                        .map(|(_, &index)| index)
                        .collect();
                    self.caster_list_cache
                        .faces
                        .insert(tile_index, (stamp, std::sync::Arc::clone(&list)));
                    return list;
                }
                if job.kind >= 4 {
                    // Every local-light tile the plan names no light for draws the same
                    // non-cluster casters. One `Arc` is handed to all of them.
                    return cached_caster_list(
                        &mut self.caster_list_cache,
                        LOCAL_CASTER_KEY,
                        true,
                        &mut rebuilt,
                        || {
                            shadow_ix
                                .iter()
                                .copied()
                                .filter(|&index| !clustered(index))
                                .collect()
                        },
                    );
                }
                let k = usize::from(job.kind);
                let key = (k as u32, job.page);
                if let Some(list) = self.caster_list_cache.lists.get(&key) {
                    return std::sync::Arc::clone(list);
                }
                rebuilt += 1;
                let (u0, v0, u1, v1) = match job.page {
                    Some((x, y)) => (
                        x as f32 / pages_per_axis,
                        y as f32 / pages_per_axis,
                        (x + 1) as f32 / pages_per_axis,
                        (y + 1) as f32 / pages_per_axis,
                    ),
                    None => (0.0, 0.0, 1.0, 1.0),
                };
                let list: std::sync::Arc<[usize]> = shadow_ix
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        if clustered(shadow_ix[*i]) {
                            return false;
                        }
                        let (_, mask) = caster_info[*i];
                        if mask & (1u8 << k.min(7)) == 0 {
                            return false;
                        }
                        match cascade_uv.get(k).and_then(|uv| uv[*i]) {
                            Some((min, max)) => max[0] >= u0 && min[0] <= u1 && max[1] >= v0 && min[1] <= v1,
                            None => true,
                        }
                    })
                    .map(|(_, &index)| index)
                    .collect();
                self.caster_list_cache
                    .lists
                    .insert(key, std::sync::Arc::clone(&list));
                list
            })
            .collect();
        report.caster_lists_rebuilt = rebuilt;
        for (job, list) in shadow_jobs.iter().zip(&job_casters) {
            if let Some(draws) = report.sun_caster_draws.get_mut(usize::from(job.kind)) {
                *draws += list.len() as u32;
            }
        }
        report.atlas_caster_draws = shadow_jobs
            .iter()
            .zip(&job_casters)
            .filter(|(job, _)| job.kind >= 4)
            .map(|(_, list)| list.len() as u32)
            .sum();
        // Exit the caster span here: left to the end of the function it stays entered, and
        // every later phase and pass (and `submit`) nests under `shadow_plan` in a profile.
        drop(caster_span);
        shadow_cpu.caster_lists_ms = caster_lists_started.elapsed().as_secs_f32() * 1000.0;
        ShadowJobs {
            shadow_jobs,
            job_casters,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LOCAL_CASTER_KEY, NEAR_CASTER_KEY, PRECIP_CASTER_KEY, cached_caster_list};
    use crate::frame::shadow_plan::CasterListCache;

    /// Six local-light tiles (one point light) share one caster list. A second pass with the
    /// same casters builds none. A changed near-tile rectangle builds that list again.
    #[test]
    fn local_tiles_share_one_caster_list() {
        let casters: std::sync::Arc<[usize]> = std::sync::Arc::from([3usize, 1, 4]);
        let mut cache = CasterListCache::default();
        let mut rebuilt = 0u32;
        let mut tiles = Vec::new();
        for _ in 0..6 {
            tiles.push(cached_caster_list(
                &mut cache,
                LOCAL_CASTER_KEY,
                true,
                &mut rebuilt,
                || std::sync::Arc::clone(&casters),
            ));
        }
        assert_eq!(rebuilt, 1, "six tiles built {rebuilt} lists");
        for tile in &tiles {
            assert!(std::sync::Arc::ptr_eq(&tiles[0], tile));
            assert_eq!(&tile[..], &[3, 1, 4]);
        }
        let mut second = 0u32;
        let again = cached_caster_list(&mut cache, LOCAL_CASTER_KEY, true, &mut second, || {
            panic!("a second pass built the local list");
        });
        assert_eq!(second, 0);
        assert!(std::sync::Arc::ptr_eq(&tiles[0], &again));

        let mut near_builds = 0u32;
        let near = cached_caster_list(&mut cache, NEAR_CASTER_KEY, true, &mut near_builds, || {
            std::sync::Arc::from([9usize])
        });
        let near_again = cached_caster_list(&mut cache, NEAR_CASTER_KEY, true, &mut near_builds, || {
            panic!("the same near rectangle built the list");
        });
        assert!(std::sync::Arc::ptr_eq(&near, &near_again));
        assert_eq!(near_builds, 1);
        let moved = cached_caster_list(&mut cache, NEAR_CASTER_KEY, false, &mut near_builds, || {
            std::sync::Arc::from([8usize])
        });
        assert_eq!(near_builds, 2, "a moved near rectangle must build once more");
        assert!(!std::sync::Arc::ptr_eq(&near, &moved));
        assert_eq!(&moved[..], &[8]);

        assert_ne!(LOCAL_CASTER_KEY, NEAR_CASTER_KEY);
        assert_ne!(LOCAL_CASTER_KEY, PRECIP_CASTER_KEY);
        assert_ne!(NEAR_CASTER_KEY, PRECIP_CASTER_KEY);
        for cascade in 0..4u32 {
            assert_ne!(LOCAL_CASTER_KEY.0, cascade);
            assert_ne!(NEAR_CASTER_KEY.0, cascade);
            assert_ne!(PRECIP_CASTER_KEY.0, cascade);
        }
    }
}

#[cfg(test)]
mod audfix_tests {
    use super::admit_dynamic_shadow_box;

    #[test]
    fn audfix_non_finite_sway_box_is_not_admitted() {
        assert!(
            admit_dynamic_shadow_box(1, [f32::NAN; 3], [1.0; 3], 0b1).is_none(),
            "a NaN sway box joined the dynamic class"
        );
        assert!(
            admit_dynamic_shadow_box(1, [-1.0; 3], [f32::INFINITY; 3], 0b1).is_none(),
            "an infinite sway box joined the dynamic class"
        );
        assert!(
            admit_dynamic_shadow_box(1, [1.0; 3], [-1.0; 3], 0b1).is_none(),
            "an inverted sway box joined the dynamic class"
        );
        let finite = admit_dynamic_shadow_box(7, [-2.0, 0.0, 0.0], [2.0, 4.0, 0.0], 0b11)
            .expect("a finite box is a dynamic caster");
        assert!(finite.dynamic);
        assert_eq!(finite.cascades, 0b11);
        assert_eq!(finite.pose.center, [0.0, 2.0, 0.0]);
        let radius = (4.0f32 + 4.0).sqrt();
        assert!((finite.pose.radius - radius).abs() < 1.0e-5);
    }
}

/// Shared atlas caster list, end to end through `build_shadow_jobs` (perfaud-render-1).
#[cfg(test)]
mod perfaud_tests {
    use super::super::gpu_types::ShadowTileGpu;
    use super::super::shadow_plan::{PlannedTile, ShadowPlan};
    use super::super::{FrameRenderer, FrameScene, ShadowCacheReport, ShadowCpuTimings};
    use super::ShadowJobInputs;

    /// Mobile High leaves sun paging off, so every local-light face is an atlas tile and each
    /// tile's caster list is the same non-clustered set. One frame builds that list once;
    /// the next frame with the same casters builds none.
    #[test]
    fn unpaged_atlas_tiles_share_one_caster_list() {
        const TILES: usize = 24;
        const CASTERS: usize = 64;
        let mut renderer = FrameRenderer::new();
        renderer.atlas_valid = false;
        let shadow_ix: Vec<usize> = (0..CASTERS).collect();
        let tiles: Vec<PlannedTile> = (0..TILES)
            .map(|i| PlannedTile {
                gpu: <ShadowTileGpu as bytemuck::Zeroable>::zeroed(),
                x: (i as u32) * 64,
                y: 0,
                size: 64,
            })
            .collect();
        let plan = ShadowPlan {
            tiles,
            light_tile: vec![-1; TILES],
            shadowed: (0..TILES).collect(),
            light_faces: Vec::new(),
            atlas_size: 2048,
            csm: bytemuck::Zeroable::zeroed(),
            near_tile: None,
            near_sphere: None,
            precip_tile: None,
            precip_plan: None,
            clock: [0.0; 4],
            clock_live: false,
        };
        let scene = FrameScene {
            width: 64,
            height: 64,
            eye: [0.0, 2.0, 8.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 200.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.0,
            double_sided: false,
            post: None,
            fog: crate::FogSettings::default(),
            directional: crate::DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 1.0),
            local_lights: &[],
            quality: crate::FrameQuality::default(),
            meshes: Vec::new(),
        };
        let draws: &[super::super::Draw] = &[];
        let caster_info: &[(Option<crate::paged_shadow::Aabb3>, u8)] = &[];
        let cascade_uv: &[Vec<crate::frame::scratch::Footprint>] = &[];
        let vps: &[[[f32; 4]; 4]] = &[];
        let mesh_full: &[bool] = &[];
        let run = |renderer: &mut FrameRenderer| {
            let mut report = ShadowCacheReport::default();
            let mut shadow_cpu = ShadowCpuTimings::default();
            let jobs = renderer.build_shadow_jobs(
                ShadowJobInputs {
                    scene: &scene,
                    plan: &plan,
                    draws,
                    shadow_ix: &shadow_ix,
                    caster_info,
                    cascade_uv,
                    vps,
                    csm_size: 4,
                    cascade_count: 0,
                    paging: false,
                    samples: 1,
                    mesh_full,
                    vis_shadows: false,
                },
                &mut report,
                &mut shadow_cpu,
            );
            (jobs, report.caster_lists_rebuilt)
        };
        let (first, rebuilt) = run(&mut renderer);
        assert_eq!(first.job_casters.len(), TILES, "one job per atlas tile");
        assert_eq!(
            first.job_casters[0].as_ref(),
            shadow_ix.as_slice(),
            "an atlas tile draws every non-clustered caster"
        );
        let shared = std::sync::Arc::clone(&first.job_casters[0]);
        let distinct = first
            .job_casters
            .iter()
            .filter(|list| !std::sync::Arc::ptr_eq(list, &shared))
            .count();
        assert_eq!(
            distinct, 0,
            "built {} extra caster lists for {TILES} identical atlas tiles ({CASTERS} casters); one shared list",
            distinct
        );
        assert_eq!(rebuilt, 1, "the shared atlas list is one build");
        let (second, rebuilt_again) = run(&mut renderer);
        assert_eq!(rebuilt_again, 0, "an unchanged caster set rebuilds no atlas list");
        assert!(
            std::sync::Arc::ptr_eq(&second.job_casters[0], &shared),
            "the next frame must reuse the cached atlas list"
        );
    }
}

/// Mesh casters whose cascade mask includes each of the first `cascades` sun cascades (the
/// candidates of that cascade's caster lists; ADR-8414).
fn sun_casters_per_cascade<T>(caster_info: &[(T, u8)], cascades: usize) -> [u32; 4] {
    let mut counts = [0u32; 4];
    for (_, mask) in caster_info {
        for (c, count) in counts.iter_mut().enumerate().take(cascades) {
            *count += u32::from(mask & (1 << c) != 0);
        }
    }
    counts
}
