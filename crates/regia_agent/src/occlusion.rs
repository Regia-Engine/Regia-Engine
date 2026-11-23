// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion of the depth prepass on the agent protocol (`book/agents/occlusion.md`,
//! `specs/98`, ADR-1887).
//!
//! `render.get_occlusion` reports whether the forward depth prepass culls in two phases: the
//! switch in effect, the player's override, the preset's column and, with a renderer, what the
//! last main-view frame did (or why it skipped). `render.set_occlusion` writes the override to
//! [`RenderSettings::two_phase_occlusion`] for the next extracted frame.
//!
//! The `shadows` block reports the shadow casters' two-phase cull (ADR-2072): per shadow-map
//! kind (sun cascades, the local-light atlas) whether it culled, its jobs, how many had last
//! redraw's pyramid, its caster instances and, when the last frame culled, how many casters each
//! phase drew (a blocking GPU readback, only then). `render.set_shadow_occlusion` switches it on
//! the renderer (off by default; no profile turns it on).
//!
//! The `masked` block reports the CPU masked occlusion cull of the forward draw list
//! (ADR-10072): the profile's column (on in Desktop Low and on the Deck) and what the last
//! main-view frame culled. `render.set_occlusion { masked }` sets the column for the next frame.

use regia_app::prelude::*;
use regia_render::{RenderDevice, RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RenderSettings)",
    )
}

/// The shadow casters' two-phase cull of `renderer` (`render.get_occlusion` `shadows`). With a
/// `device` and a last frame that culled, the per-phase counts are read back from the GPU
/// (blocking); otherwise they are null, so a frame that did not cull waits for nothing.
fn shadows(renderer: &regia_render::FrameRenderer, device: Option<&RenderDevice>) -> Value {
    let s = renderer.shadow_occlusion_stats();
    let counters = device.filter(|_| s.active).and_then(|device| {
        renderer.shadow_occlusion_counters(device).unwrap_or_else(|err| {
            tracing::warn!(target: "regia_agent", %err, "shadow occlusion counters unavailable");
            None
        })
    });
    let kinds: Vec<Value> = s
        .kinds
        .iter()
        .enumerate()
        .map(|(kind, k)| {
            let counts = counters.as_ref().filter(|_| k.active).map(|c| c.kinds[kind]);
            json!({
                "kind": regia_render::shadow_kind_name(kind),
                "active": k.active,
                "jobs": k.jobs,
                "history_jobs": k.history_jobs,
                "instances": k.instances,
                "unbounded": k.unbounded,
                "phase1": counts.map(|c| c.phase1),
                "phase2": counts.map(|c| c.phase2),
                "culled": counts.map(|c| c.culled),
            })
        })
        .collect();
    json!({
        "enabled": s.enabled,
        "active": s.active,
        "skipped": s.skipped.map(|why| why.name()),
        "min_casters": s.min_casters,
        "instances": s.kinds.iter().map(|k| k.instances).sum::<u32>(),
        "phase1": counters.as_ref().map(|c| c.total.phase1),
        "phase2": counters.as_ref().map(|c| c.total.phase2),
        "culled": counters.as_ref().map(|c| c.total.culled),
        "pyramid_bytes": s.pyramid_bytes,
        "frames": s.frames,
        "kinds": kinds,
    })
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| {
        let s = r.0.prepass_occlusion_stats();
        json!({
            "enabled": s.enabled,
            "active": s.active,
            "skipped": s.skipped.map(|why| why.name()),
            "history": s.history,
            "instances": s.instances,
            "unbounded": s.unbounded,
            "batches": s.batches,
            "frames": s.frames,
            "hiz_occlusion": r.0.hiz_occlusion(),
        })
    });
    let shadow = world
        .get_resource::<RuntimeRenderer>()
        .map(|r| shadows(&r.0, world.get_resource::<RenderDevice>()));
    let masked_renderer = world.get_resource::<RuntimeRenderer>().map(|r| {
        let m = r.0.masked_occlusion_report();
        json!({
            "enabled": m.enabled,
            "active": m.active,
            "skipped": m.skipped.map(regia_render::MaskedCullSkip::as_str),
            "occluders": m.occluders,
            "occluder_triangles": m.occluder_triangles,
            "tested": m.tested,
            "culled": m.culled,
            "raster_us": m.raster_micros,
            "test_us": m.test_micros,
            "frames": m.frames,
        })
    });
    Ok(json!({
        "two_phase": settings.effective_two_phase_occlusion(),
        "setting": settings.two_phase_occlusion,
        "preset": settings.profile.two_phase_occlusion,
        "visibility_buffer": settings.profile.visibility_buffer,
        "renderer": renderer,
        "shadows": shadow,
        // The shadow casters' cull as a profile column (ADR-2482): the override and the preset's.
        "shadow_setting": settings.shadow_occlusion,
        "shadow_preset": settings.profile.shadow_occlusion,
        // The CPU masked occlusion cull of the draw list (ADR-10072): the profile's column and
        // the renderer's last main-view frame.
        "masked": {
            "enabled": settings.profile.masked_occlusion,
            "preset": settings.preset().map(|p| regia_render::RenderProfile::from_preset(p).masked_occlusion),
            "renderer": masked_renderer,
        },
    }))
}

/// `render.get_occlusion`: the two-phase prepass switch in effect, the override, the preset's
/// column and the renderer's last frame.
pub(crate) fn get_occlusion(world: &mut World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_occlusion`: `two_phase` (true, false, or null for the preset's column) and/or
/// `masked` (true or false: the profile's masked occlusion column, ADR-10072). At least one is
/// required. Returns the state before and after.
pub(crate) fn set_occlusion(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let two_phase = match params.get("two_phase") {
        Some(Value::Bool(on)) => Some(Some(*on)),
        Some(Value::Null) => Some(None),
        None => None,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`two_phase` must be true, false or null, got {other}"
            )));
        }
    };
    let masked = match params.get("masked") {
        Some(Value::Bool(on)) => Some(*on),
        None => None,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`masked` must be true or false, got {other}"
            )));
        }
    };
    if two_phase.is_none() && masked.is_none() {
        return Err(RpcError::invalid_params(
            "give `two_phase` (true, false or null for the preset's choice) and/or `masked` (true or false)",
        ));
    }
    let before = state(world)?;
    if let Some(mut settings) = world.get_resource_mut::<RenderSettings>() {
        if let Some(two_phase) = two_phase {
            settings.two_phase_occlusion = two_phase;
        }
        if let Some(masked) = masked {
            settings.profile.masked_occlusion = masked;
        }
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; the first frame after it turns on has no \
                 pyramid history (phase 1 draws everything in view), and frames with fewer than \
                 2048 depth instances skip it (renderer.skipped); masked culls on the CPU and keeps \
                 the frame identical (masked.renderer reports its last frame)",
    }))
}

/// `render.set_shadow_occlusion`: `on` (true or false) and optionally `min_casters` (a count, or
/// null for the default). Returns the `shadows` block before and after.
pub(crate) fn set_shadow_occlusion(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = match params.get("on") {
        Some(Value::Bool(on)) => *on,
        None => return Err(RpcError::invalid_params("give `on` (true or false)")),
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "`on` must be true or false, got {other}"
            )));
        }
    };
    let min_casters = match params.get("min_casters") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => match value.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n >= 1 => Some(Some(n)),
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "`min_casters` must be a whole number from 1, or null for the default, got {value}"
                )));
            }
        },
    };
    let (before, after) = {
        let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
            return Err(RpcError::not_found(
                "no RuntimeRenderer resource: this app draws no frames",
                "add regia_render::RenderPlugin with a window or a headless renderer",
            ));
        };
        let before = shadows(&renderer.0, None);
        renderer.0.set_shadow_occlusion(on);
        if let Some(min) = min_casters {
            renderer.0.set_shadow_occlusion_min_casters(min);
        }
        (before, shadows(&renderer.0, None))
    };
    // The render systems hand `RenderSettings::effective_shadow_occlusion` to the renderer every
    // frame (ADR-2482), so the switch is kept there too.
    if let Some(mut settings) = world.get_resource_mut::<RenderSettings>() {
        settings.shadow_occlusion = Some(on);
    }
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next frame on the main view; a kind culls only when it is redrawn \
                 and its jobs hold at least min_casters caster instances; the shadow maps are the \
                 unculled maps either way",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    fn world(preset: RenderProfilePreset) -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(preset));
        world
    }

    #[test]
    fn get_reports_the_preset_column() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let s = get_occlusion(&mut w).expect("state");
        assert_eq!(s["two_phase"], true);
        assert_eq!(s["preset"], true);
        assert!(s["setting"].is_null());
        assert!(s["renderer"].is_null(), "no renderer in this world");
        let mut w = world(RenderProfilePreset::Deck);
        assert_eq!(get_occlusion(&mut w).expect("state")["two_phase"], false);
    }

    #[test]
    fn get_reports_the_prepass_and_shadow_columns_of_every_preset() {
        // specs/98 § 2: the prepass column is on in Desktop High and Ultra only; § 10a and
        // ADR-2482: the shadow casters' column is off in every preset.
        for preset in RenderProfilePreset::ALL {
            let mut w = world(preset);
            let s = get_occlusion(&mut w).expect("state");
            let on = matches!(
                preset,
                RenderProfilePreset::DesktopHigh | RenderProfilePreset::DesktopUltra
            );
            assert_eq!(s["preset"], on, "{preset:?} prepass column");
            assert_eq!(s["two_phase"], on, "{preset:?} prepass switch");
            assert_eq!(s["shadow_preset"], false, "{preset:?} shadow column");
            assert!(s["shadow_setting"].is_null(), "{preset:?} shadow override");
            assert!(s["shadows"].is_null(), "no renderer in this world");
        }
    }

    #[test]
    fn the_shadow_switch_writes_the_shadow_override() {
        // ADR-2482: the render systems hand the override to the renderer every frame, so
        // render.set_shadow_occlusion keeps it in RenderSettings too, and get reports it.
        let mut w = world(RenderProfilePreset::DesktopHigh);
        w.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        set_shadow_occlusion(&mut w, &json!({ "on": true })).expect("on");
        assert_eq!(w.resource::<RenderSettings>().shadow_occlusion, Some(true));
        let s = get_occlusion(&mut w).expect("state");
        assert_eq!(s["shadow_setting"], true);
        assert_eq!(s["shadow_preset"], false, "the preset's column is unchanged");
        set_shadow_occlusion(&mut w, &json!({ "on": false })).expect("off");
        assert_eq!(get_occlusion(&mut w).expect("state")["shadow_setting"], false);
    }

    #[test]
    fn set_writes_the_override_and_null_clears_it() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        let r = set_occlusion(&mut w, &json!({ "two_phase": false })).expect("off");
        assert_eq!(r["before"]["two_phase"], true);
        assert_eq!(r["after"]["two_phase"], false);
        assert_eq!(r["after"]["setting"], false);
        assert_eq!(w.resource::<RenderSettings>().two_phase_occlusion, Some(false));
        let r = set_occlusion(&mut w, &json!({ "two_phase": null })).expect("default");
        assert_eq!(r["after"]["two_phase"], true);
        assert_eq!(w.resource::<RenderSettings>().two_phase_occlusion, None);
    }

    #[test]
    fn masked_reports_the_column_and_set_switches_it() {
        // ADR-10072: on in Desktop Low and on the Deck, off elsewhere.
        for preset in RenderProfilePreset::ALL {
            let mut w = world(preset);
            let s = get_occlusion(&mut w).expect("state");
            let on = matches!(
                preset,
                RenderProfilePreset::DesktopLow | RenderProfilePreset::Deck
            );
            assert_eq!(s["masked"]["enabled"], on, "{preset:?} masked column");
            assert_eq!(s["masked"]["preset"], on, "{preset:?} masked preset column");
            assert!(s["masked"]["renderer"].is_null(), "no renderer in this world");
        }
        let mut w = world(RenderProfilePreset::DesktopLow);
        w.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let r = set_occlusion(&mut w, &json!({ "masked": false })).expect("off");
        assert_eq!(r["before"]["masked"]["enabled"], true);
        assert_eq!(r["after"]["masked"]["enabled"], false);
        assert_eq!(r["after"]["masked"]["renderer"]["culled"], 0);
        // The column is masked in preset(): the profile still reads as Desktop Low.
        assert_eq!(
            w.resource::<RenderSettings>().preset(),
            Some(RenderProfilePreset::DesktopLow)
        );
        assert_eq!(r["after"]["masked"]["preset"], true);
        // The two-phase override is untouched by a masked-only call.
        assert!(r["after"]["setting"].is_null());
        assert!(
            set_occlusion(&mut w, &json!({})).is_err(),
            "one of the two is required"
        );
        assert!(set_occlusion(&mut w, &json!({ "masked": null })).is_err());
        assert!(set_occlusion(&mut w, &json!({ "masked": "on" })).is_err());
    }

    #[test]
    fn shadow_switch_needs_a_renderer_and_checks_its_params() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        assert!(get_occlusion(&mut w).expect("state")["shadows"].is_null());
        let err = set_shadow_occlusion(&mut w, &json!({ "on": true })).expect_err("no renderer");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
        assert!(set_shadow_occlusion(&mut w, &json!({})).is_err());
        assert!(set_shadow_occlusion(&mut w, &json!({ "on": "yes" })).is_err());
        assert!(set_shadow_occlusion(&mut w, &json!({ "on": true, "min_casters": 0 })).is_err());
        assert!(set_shadow_occlusion(&mut w, &json!({ "on": true, "min_casters": -3 })).is_err());
    }

    #[test]
    fn shadow_switch_reports_before_and_after() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        w.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        let s = get_occlusion(&mut w).expect("state");
        assert_eq!(s["shadows"]["enabled"], false);
        assert_eq!(s["shadows"]["kinds"].as_array().map(Vec::len), Some(5));
        assert_eq!(s["shadows"]["kinds"][4]["kind"], "atlas");
        let r = set_shadow_occlusion(&mut w, &json!({ "on": true, "min_casters": 64 })).expect("on");
        assert_eq!(r["before"]["enabled"], false);
        assert_eq!(r["after"]["enabled"], true);
        assert_eq!(r["after"]["min_casters"], 64);
        assert!(w.resource::<RuntimeRenderer>().0.shadow_occlusion());
        let r = set_shadow_occlusion(&mut w, &json!({ "on": false, "min_casters": null })).expect("off");
        assert_eq!(r["after"]["enabled"], false);
        assert_eq!(
            r["after"]["min_casters"],
            u64::from(regia_render::SHADOW_OCCLUSION_MIN_CASTERS)
        );
    }

    /// The agent's view of a real culled frame: a slab over a field of small boxes, lit by the sun
    /// and a spot light from above, drawn twice (the second frame has the pyramids' history).
    #[test]
    fn get_occlusion_reports_the_gpu_counts_of_a_culled_frame() {
        use regia_render::{
            DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameScene, IndirectStrategy,
            LabLocalLight, LabMaterial, LabSpotLight, LabVertex, RenderProfile, ShadowCast, box_mesh,
        };
        fn mesh<'a>(
            key: u64,
            mesh_key: u64,
            geometry: &'a (Vec<LabVertex>, Vec<u32>),
            material: &'a LabMaterial,
            at: [f32; 3],
        ) -> FrameMesh<'a> {
            FrameMesh {
                is_static: false,
                mesh_key,
                material_key: 1,
                object_key: key,
                vertices: geometry.0.as_slice(),
                indices: geometry.1.as_slice(),
                material,
                model: [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [at[0], at[1], at[2], 1.0],
                ],
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        }
        let Ok(device) = crate::seeing::headless_render_device() else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            eprintln!("skipping: the device draws no GPU objects");
            return;
        }
        let slab = box_mesh([3.2, 0.05, 3.2]);
        let cube = box_mesh([0.1, 0.1, 0.1]);
        let material = LabMaterial::default();
        let mut meshes = vec![mesh(1, 1, &slab, &material, [0.0, 3.0, 0.0])];
        for i in 0..64u64 {
            let (col, row) = ((i % 8) as f32, (i / 8) as f32);
            let at = [-1.4 + col * 0.4, 0.2, -1.4 + row * 0.4];
            meshes.push(mesh(i + 10, 2, &cube, &material, at));
        }
        let lights = [LabLocalLight::Spot(LabSpotLight {
            position: [0.3, 6.0, 0.2],
            direction: [0.0, -1.0, 0.0],
            color: [1.0, 0.9, 0.8],
            intensity: 400.0,
            range: 14.0,
            inner_angle: 0.4,
            outer_angle: 0.8,
            cast_shadows: true,
            source_radius: None,
        })];
        let scene = FrameScene {
            width: 64,
            height: 48,
            eye: [0.0, 2.5, 7.5],
            target: [0.0, 0.5, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 40.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.5,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([-0.15, -1.0, -0.1], [1.0, 1.0, 1.0], 3.0),
            local_lights: &lights,
            quality: FrameQuality {
                csm_cascade_count: 1,
                csm_resolution: 256,
                local_shadow_atlas_size: 256,
                paged_sun_shadows: false,
                taau: false,
                ..FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh))
            },
            meshes,
        };
        let mut w = world(RenderProfilePreset::DesktopHigh);
        w.insert_resource(RuntimeRenderer(regia_render::FrameRenderer::new()));
        w.insert_resource(device.clone());
        set_shadow_occlusion(&mut w, &json!({ "on": true, "min_casters": 16 })).expect("on");
        let frame = |w: &mut World| {
            w.resource_mut::<RuntimeRenderer>()
                .0
                .render_offscreen(&device, &scene)
                .expect("frame");
        };
        frame(&mut w);
        let first = get_occlusion(&mut w).expect("state")["shadows"].clone();
        assert_eq!(first["active"], true, "{first}");
        assert_eq!(first["culled"], 0, "no history yet: {first}");
        assert_eq!(first["phase1"], first["instances"], "{first}");

        frame(&mut w);
        let second = get_occlusion(&mut w).expect("state")["shadows"].clone();
        assert_eq!(second["active"], true, "{second}");
        assert!(second["culled"].as_u64().unwrap_or(0) > 0, "{second}");
        assert_eq!(second["phase2"], 0, "{second}");
        for kind in second["kinds"].as_array().expect("kinds") {
            if kind["active"] == true {
                assert_eq!(
                    kind["phase1"].as_u64().unwrap_or(0)
                        + kind["phase2"].as_u64().unwrap_or(0)
                        + kind["culled"].as_u64().unwrap_or(0),
                    kind["instances"].as_u64().unwrap_or(u64::MAX),
                    "every caster is counted once: {kind}"
                );
                assert_eq!(kind["history_jobs"], kind["jobs"], "{kind}");
            } else {
                assert!(
                    kind["culled"].is_null(),
                    "a kind that did not cull has no counts: {kind}"
                );
            }
        }
        // The switch's own report never waits on the GPU.
        let r = set_shadow_occlusion(&mut w, &json!({ "on": false })).expect("off");
        assert!(r["before"]["culled"].is_null(), "{r}");
        // Off: the next frame culls nothing, so there is nothing to read back.
        frame(&mut w);
        let off = get_occlusion(&mut w).expect("state")["shadows"].clone();
        assert_eq!(off["skipped"], "off", "{off}");
        assert!(off["culled"].is_null(), "{off}");
        assert_eq!(off["pyramid_bytes"], 0, "{off}");
    }

    #[test]
    fn bad_params_and_missing_settings_are_errors() {
        let mut w = world(RenderProfilePreset::DesktopHigh);
        assert!(set_occlusion(&mut w, &json!({})).is_err());
        assert!(set_occlusion(&mut w, &json!({ "two_phase": "yes" })).is_err());
        let mut empty = World::new();
        let err = get_occlusion(&mut empty).expect_err("no settings");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("F0003")
        );
    }
}
