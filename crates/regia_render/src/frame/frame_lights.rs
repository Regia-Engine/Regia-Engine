// SPDX-License-Identifier: MPL-2.0
//! The frame's lights in `FrameRenderer::render_into` (ADR-1552 split, line m): the visible
//! local lights, the sun and fill light, the scene export and daylit-interior updates, the CPU
//! shadow plan, the light upload report and the emissive meshes' fitted lights. Moved out of
//! `frame.rs` unchanged.

use super::*;

/// What the frame's lights are selected and planned from.
pub(super) struct FrameLightInputs<'a> {
    /// The frame's scene (lights, sun, eye, quality).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame renders into a texture view, not the main view.
    pub(super) view_pass: bool,
    /// The frame clock.
    pub(super) now: u64,
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Output aspect ratio.
    pub(super) aspect: f32,
    /// The planet IBL lights the frame.
    pub(super) planet_ibl: bool,
    /// The unjittered view-projection.
    pub(super) unjittered_vp: regia_core::glam::Mat4,
    /// The post settings this frame records, when the post stack runs.
    pub(super) forced_post: &'a Option<crate::post::PostSettings>,
    /// The post stack's exposure this frame.
    pub(super) post_exposure: f32,
    /// The baked volume lights this frame.
    pub(super) baked_on: bool,
}

/// The frame's lights and the CPU shadow plan.
pub(super) struct FrameLights {
    /// The local lights uploaded this frame.
    pub(super) uploaded: Vec<crate::LabLocalLight>,
    /// The frame's sun.
    pub(super) directional: DirectionalLightUniform,
    /// The sky body's fill light, when there is one.
    pub(super) fill_light: Option<[[f32; 3]; 2]>,
    /// The CPU shadow plan (cascades, atlas tiles, shadowed lights).
    pub(super) plan: shadow_plan::ShadowPlan,
    /// The emissive meshes' fitted light records.
    pub(super) emissive_records: Vec<[[f32; 4]; 4]>,
    /// The emissive lights' probe texels for the probe atlas.
    pub(super) emissive_probes: Vec<f32>,
}

impl FrameRenderer {
    /// Selects the frame's visible lights, resolves the sun, plans the shadows and fits the
    /// emissive meshes' lights.
    pub(super) fn frame_lights(&mut self, inputs: &FrameLightInputs<'_>) -> FrameLights {
        let FrameLightInputs {
            scene,
            view_pass,
            now,
            fov_y,
            aspect,
            planet_ibl,
            unjittered_vp,
            forced_post,
            post_exposure,
            baked_on,
        } = *inputs;
        let light_view = crate::light_budget::LightView::new(
            scene.eye,
            scene.target,
            fov_y,
            aspect,
            scene.near,
            scene.far,
        );
        let selection = self.light_warnings.select_visible_lights(
            scene.local_lights,
            &light_view,
            scene.quality.max_visible_lights,
        );
        self.light_extras.note_selection(&selection.kept);
        let mut uploaded = self.scratch.lights.take();
        uploaded.extend(
            selection
                .kept
                .iter()
                .filter_map(|&index| scene.local_lights.get(index).copied()),
        );
        // A texture view takes the sun of the tables the main view built: its own eye height
        // would rebuild them (~50 ms of CPU) on every switch.
        let directional = if view_pass {
            self.outdoor.sun_from_tables(planet_ibl, scene.directional)
        } else {
            self.outdoor
                .apply_sun(planet_ibl, scene.directional, scene.eye[1])
        };
        let fill_light = self.outdoor.fill_light(directional.direction);
        if !view_pass && let Some(exporter) = self.scene_export.as_mut() {
            let light = crate::scene_export::ExportLight {
                sun: directional,
                fill_light,
                extra_directional: self.lights_views.extra_lights(),
                outdoor: self.outdoor.params(),
                fov_y,
            };
            exporter.record(now, scene, &light);
        }
        if !view_pass {
            self.update_daylit_interior(scene, directional, &uploaded);
        }
        // Every cascade may keep its placement. Volumetric fog reads one cascade at every
        // froxel inside it (not just inside its slice) and fades the shadow out toward the
        // cascade edge (`volumetric_fog.wgsl` `shadow_at`), so a kept placement only moves
        // that faded edge when the cascade is re-placed.
        let sticky_mask = if scene.quality.paged_sun_shadows {
            0b1111
        } else {
            0
        };
        let forward_tier = self.frame_forward_tier(scene.quality.forward_lite);
        let precip = (!view_pass)
            .then(|| self.wanted_precip_tile(scene, &uploaded, directional.direction))
            .flatten();
        // All screenshot tiles use the same sun-cascade fit and near-tile frustum.
        let full_scene = self.stereo.capture_size.map(|[width, height]| FrameScene {
            width,
            height,
            ..scene.clone()
        });
        let shadow_vp = if self.stereo.capture_size.is_some() {
            self.stereo.camera.map_or(unjittered_vp, |camera| camera.cull)
        } else {
            unjittered_vp
        };
        let mut plan = plan_shadows(
            full_scene.as_ref().unwrap_or(scene),
            &uploaded,
            &light_view,
            directional.direction,
            CascadeSticky {
                anchors: &mut self.cascade_anchors,
                near_anchor: &mut self.near_anchor,
                mask: sticky_mask,
                inv_view_proj: shadow_vp.inverse(),
                sun_range_m: crate::forward_lite::sun_range_m(forward_tier)
                    .min(self.sun_shadow_range.unwrap_or(f32::INFINITY)),
                precip,
            },
            &self.light_warnings,
        );
        plan.clock = super::gpu_types::graph_clock_row(
            self.material_time.unwrap_or((0.0, 0.0)),
            self.foliage_wind.unwrap_or([0.0, 0.0]),
        );
        plan.clock_live = self.material_time.is_some() || self.foliage_wind.is_some();
        if !view_pass {
            self.note_precip_tile(&plan);
        }
        let mut upload = crate::light_budget::LightUploadReport {
            authored: selection.authored,
            uploaded: uploaded.len() as u32,
            dropped: selection.dropped,
            cap: selection.cap,
            cluster_overflow: 0,
        };
        if uploaded.len() > scene.quality.max_lights_per_cluster as usize {
            let (_stored, overflow) = self.light_warnings.assign_clusters(
                &uploaded,
                &light_view,
                scene.quality.max_lights_per_cluster,
            );
            upload.cluster_overflow = overflow;
        }
        self.light_upload = upload;
        // Emissive meshes as shadowless area lights after the authored ones, within the
        // buffer's room. Their range ends where they would add 4 % of middle grey to a white
        // surface at this frame's exposure.
        let exposure_mult = if forced_post.is_some() {
            post_exposure
        } else {
            crate::post::exposure_from_ev100(scene.exposure_ev)
        };
        let emissive_budget = crate::emissive_lights::EmissiveBudget {
            max_lights: scene.quality.emissive_lights,
            cutoff_share: crate::emissive_lights::EmissiveBudget::CUTOFF_SHARE,
        };
        let room = crate::MAX_UPLOADED_LIGHTS.saturating_sub(uploaded.len() as u32);
        // v0.1.18: new fits and probe traces past the load frame go to a worker thread.
        self.emissive_cache.set_background(true);
        // ADR-1386: the lights of emitters the baked volume holds are tagged, and the forward
        // pass leaves their light to the volume where it covers.
        let baked_emitters = if baked_on {
            self.baked_gi.emitter_set()
        } else {
            None
        };
        self.emissive_cache.set_baked_emitters(baked_emitters);
        self.emissive_cache.set_origin_offset(self.origin_offset);
        // ADR-2592: authored rect and tube lights join the emitters; profile bands sit under their tiles.
        self.emissive_cache
            .set_authored_area_lights(&self.light_extras.area_lights);
        let (emissive_records, mut emissive_probes, emissive_report) =
            crate::emissive_lights::collect_frame_lights(
                &mut self.emissive_cache,
                &scene.meshes,
                &light_view,
                emissive_budget
                    .max_lights
                    .min(room)
                    .min(self.light_extras.emitter_tile_cap()),
                emissive_budget.cutoff_lux(0.18 / exposure_mult.max(1e-12)),
            );
        self.light_extras.write_profile_bands(&mut emissive_probes);
        self.emissive_report = emissive_report;
        // Under GI Depth the probes take only the far part of the emission these fitted lights
        // carry (per mesh: the range of its uploaded lights), so it is not counted twice.
        if !view_pass {
            self.ddgi.emitter_local_ranges.clear();
            self.ddgi
                .emitter_local_ranges
                .extend_from_slice(self.emissive_cache.mesh_ranges());
        }
        FrameLights {
            uploaded,
            directional,
            fill_light,
            plan,
            emissive_records,
            emissive_probes,
        }
    }
}
