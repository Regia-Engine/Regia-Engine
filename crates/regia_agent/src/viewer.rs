// SPDX-License-Identifier: MPL-2.0
//! `viewer.*`: the product viewer's renderer from the agent protocol (`book/agents/viewer-renderer.md`,
//! ADR-2539).
//!
//! The viewer is its own lean renderer with its own device, not the running game's. These methods check it
//! headlessly: what it supports, a still of the stand-in piece with chosen materials and view, and whether
//! an environment file loads. Refusals are `F0002` with `data.reason` starting `viewer_`; a machine without
//! any adapter answers `F0003` with `viewer_no_adapter`.

use regia_app::prelude::World;
use regia_core::glam::{Affine3A, Vec3};
use regia_viewer::gpu::sample_count_for;
use regia_viewer::material::ShaderFamily;
use regia_viewer::standin::{stud, stud_visible_bounds};
use regia_viewer::{
    AnodizeFilm, AnodizedParams, BakedEnv, CabochonParams, ContactShadow, FrostedParams, GemParams,
    MetalParams, OpalParams, ViewerCamera, ViewerConfig, ViewerError, ViewerMaterial, ViewerRenderer,
    ViewerStyle, capture_still,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// The largest still a call may ask for, per side.
const MAX_STILL_SIDE: u64 = 4096;

fn refusal(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": reason,
            "hint": "see book/agents/viewer-renderer.md",
        })),
    }
}

/// A number in `object[key]`, `fallback` when absent.
fn number(object: &Value, key: &str, fallback: f32) -> Result<f32, RpcError> {
    match object.get(key) {
        None => Ok(fallback),
        Some(v) => v
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite())
            .ok_or_else(|| refusal("viewer_bad_value", format!("`{key}` must be a finite number"))),
    }
}

/// Three numbers in `object[key]`, `fallback` when absent.
fn color(object: &Value, key: &str, fallback: [f32; 3]) -> Result<[f32; 3], RpcError> {
    let Some(value) = object.get(key) else {
        return Ok(fallback);
    };
    color_value(value).ok_or_else(|| refusal("viewer_bad_value", format!("`{key}` must be three numbers")))
}

fn color_value(value: &Value) -> Option<[f32; 3]> {
    let list = value.as_array().filter(|l| l.len() == 3)?;
    let mut out = [0.0; 3];
    for (slot, v) in out.iter_mut().zip(list) {
        *slot = v.as_f64().map(|v| v as f32).filter(|v| v.is_finite())?;
    }
    Some(out)
}

fn check_fields(object: &Value, known: &[&str]) -> Result<(), RpcError> {
    let Some(map) = object.as_object() else {
        return Err(refusal("viewer_bad_value", "expected an object"));
    };
    if let Some(unknown) = map.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(refusal(
            "viewer_unknown_field",
            format!("unknown field `{unknown}`; known: {}", known.join(", ")),
        ));
    }
    Ok(())
}

/// One material from its JSON form. Lookup tables are added to `viewer` as they are read.
fn material(value: &Value, viewer: &mut ViewerRenderer) -> Result<ViewerMaterial, RpcError> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| refusal("viewer_bad_material", "a material needs a `kind` string"))?;
    Ok(match kind {
        "metal" => {
            check_fields(value, &["kind", "base_color", "roughness"])?;
            ViewerMaterial::Metal(MetalParams {
                base_color: color(value, "base_color", [0.91, 0.92, 0.92])?,
                roughness: number(value, "roughness", 0.15)?,
            })
        }
        "anodized" => {
            check_fields(value, &["kind", "roughness", "color", "lut"])?;
            let film = match value.get("lut") {
                Some(table) => {
                    let rows: Option<Vec<[f32; 3]>> = table
                        .as_array()
                        .map(|rows| rows.iter().map(color_value).collect())
                        .unwrap_or(None);
                    let rows = rows.ok_or_else(|| {
                        refusal("viewer_bad_value", "`lut` must be a list of three-number colors")
                    })?;
                    AnodizeFilm::Lut(
                        viewer
                            .add_lut(&rows)
                            .map_err(|e| refusal("viewer_bad_value", format!("`lut`: {e}")))?,
                    )
                }
                None => AnodizeFilm::Color(color(value, "color", [0.1, 0.45, 0.7])?),
            };
            ViewerMaterial::Anodized(AnodizedParams {
                roughness: number(value, "roughness", 0.2)?,
                film,
            })
        }
        "gem" => {
            check_fields(
                value,
                &[
                    "kind",
                    "tint",
                    "roughness",
                    "ior",
                    "dispersion",
                    "sparkle",
                    "seed",
                ],
            )?;
            ViewerMaterial::Gem(GemParams {
                tint: color(value, "tint", [1.0, 1.0, 1.0])?,
                roughness: number(value, "roughness", 0.02)?,
                ior: number(value, "ior", 1.77)?,
                dispersion: number(value, "dispersion", 0.3)?,
                sparkle: number(value, "sparkle", 0.5)?,
                seed: number(value, "seed", 0.0)?,
            })
        }
        "opal" => {
            check_fields(
                value,
                &["kind", "body", "roughness", "fire", "fire_scale_mm", "seed"],
            )?;
            ViewerMaterial::Opal(OpalParams {
                body: color(value, "body", [0.9, 0.9, 0.86])?,
                roughness: number(value, "roughness", 0.1)?,
                fire: number(value, "fire", 0.7)?,
                fire_scale_mm: number(value, "fire_scale_mm", 0.5)?,
                seed: number(value, "seed", 0.0)?,
            })
        }
        "cabochon" => {
            check_fields(value, &["kind", "body", "roughness", "ior", "translucency"])?;
            ViewerMaterial::Cabochon(CabochonParams {
                body: color(value, "body", [0.2, 0.55, 0.4])?,
                roughness: number(value, "roughness", 0.05)?,
                ior: number(value, "ior", 1.55)?,
                translucency: number(value, "translucency", 0.3)?,
            })
        }
        "frosted" => {
            check_fields(value, &["kind", "tint", "roughness", "ior"])?;
            ViewerMaterial::Frosted(FrostedParams {
                tint: color(value, "tint", [0.85, 0.9, 1.0])?,
                roughness: number(value, "roughness", 0.6)?,
                ior: number(value, "ior", 1.5)?,
            })
        }
        other => {
            return Err(refusal(
                "viewer_bad_material",
                format!(
                    "unknown material kind `{other}`; known: metal, anodized, gem, opal, cabochon, frosted"
                ),
            ));
        }
    })
}

/// `viewer.capabilities {}`: what this build's viewer renderer supports. Touches no GPU.
pub fn capabilities(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let families: Vec<&str> = ShaderFamily::ALL.iter().map(|f| f.name()).collect();
    let env = BakedEnv::studio_default();
    Ok(json!({
        "families": families,
        "material_kinds": ["metal", "anodized", "gem", "opal", "cabochon", "frosted"],
        "pipelines": ShaderFamily::ALL.len() * 2 + 1,
        "lookup_rows": regia_viewer::LUT_ROWS,
        "lookup_width": regia_viewer::LUT_WIDTH,
        "env_format_version": regia_viewer::ENV_FORMAT_VERSION,
        "default_env": { "face_size": env.face_size(), "mips": env.mip_count() },
        "limits": "webgl2",
    }))
}

/// `viewer.env_info { path }`: whether a `.renv` file loads, and its sizes. A file that fails is a result
/// (`ok: false` with the reason), not a call error.
pub fn env_info(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(path) = params.get("path").and_then(Value::as_str) else {
        return Err(RpcError::invalid_params("`path` (a .renv file) is required"));
    };
    let file = crate::req_path::allowed_path(world, "path", path)?;
    let bytes = crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES)
        .map_err(|e| refusal("viewer_env_unreadable", format!("cannot read `{path}`: {e}")))?;
    Ok(match BakedEnv::parse(&bytes) {
        Ok(env) => json!({
            "ok": true,
            "format_version": regia_viewer::ENV_FORMAT_VERSION,
            "face_size": env.face_size(),
            "mips": env.mip_count(),
            "bytes": bytes.len(),
        }),
        Err(e) => json!({ "ok": false, "error": e.to_string(), "bytes": bytes.len() }),
    })
}

fn still_side(params: &Value, key: &str) -> Result<u32, RpcError> {
    match params.get(key) {
        None => Ok(512),
        Some(v) => v
            .as_u64()
            .filter(|s| (1..=MAX_STILL_SIDE).contains(s))
            .map(|s| s as u32)
            .ok_or_else(|| {
                refusal(
                    "viewer_bad_size",
                    format!("`{key}` must be a whole number from 1 to {MAX_STILL_SIDE}"),
                )
            }),
    }
}

fn viewer_error(e: &ViewerError) -> RpcError {
    refusal("viewer_render_failed", e.to_string())
}

/// `viewer.render_still { width?, height?, materials?, view?, exposure?, contact_shadow?, env?, path? }`:
/// the stand-in piece through the viewer's forward pass on its own headless device.
pub fn render_still(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("viewer_render_still").entered();
    check_fields(
        params,
        &[
            "width",
            "height",
            "materials",
            "view",
            "exposure",
            "contact_shadow",
            "env",
            "path",
        ],
    )?;
    let width = still_side(params, "width")?;
    let height = still_side(params, "height")?;
    let exposure = number(params, "exposure", 1.0)?;
    // An opacity: checked here with the other values so a bad one makes no device.
    let contact_opacity = match params.get("contact_shadow") {
        None => None,
        Some(_) => {
            let opacity = number(params, "contact_shadow", 0.5)?;
            if !(0.0..=1.0).contains(&opacity) {
                return Err(refusal(
                    "viewer_bad_value",
                    "`contact_shadow` must be a number from 0 to 1",
                ));
            }
            Some(opacity)
        }
    };
    // Paths are checked first: a refused path renders nothing.
    let out = params
        .get("path")
        .map(|v| {
            let path = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`path` is a string"))?;
            crate::req_path::allowed_path(world, "path", path)
        })
        .transpose()?;
    let env = params
        .get("env")
        .map(|v| {
            let path = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`env` is a string"))?;
            let file = crate::req_path::allowed_path(world, "env", path)?;
            let bytes = crate::req_path::read_file_capped(&file, crate::req_path::MAX_READ_BYTES)
                .map_err(|e| refusal("viewer_env_unreadable", format!("cannot read `{path}`: {e}")))?;
            BakedEnv::parse(&bytes).map_err(|e| refusal("viewer_bad_env", format!("`{path}`: {e}")))
        })
        .transpose()?;

    // A host that opened no GPU on purpose keeps it that way (ADR-5180).
    crate::seeing::GpuDisabled::refuse(world)?;
    let (adapter, device, queue) = crate::seeing::viewer_device().map_err(|e| match e {
        ViewerError::NoAdapter(_) => {
            let mut error = RpcError::new(
                NOT_FOUND,
                "F0003",
                e.to_string(),
                "the viewer needs a GPU adapter; on a headless machine install the lavapipe driver",
            );
            if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
                data.insert("reason".into(), json!("viewer_no_adapter"));
            }
            error
        }
        other => viewer_error(&other),
    })?;
    let info = adapter.get_info();
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut config = ViewerConfig::new(format);
    config.sample_count = sample_count_for(&adapter, format);
    let mut viewer = ViewerRenderer::new(device, queue, config).map_err(|e| viewer_error(&e))?;
    if let Some(env) = &env {
        viewer.set_environment(env);
    }

    let overrides = params.get("materials").cloned().unwrap_or(Value::Null);
    if !overrides.is_null() {
        check_fields(&overrides, &["metal", "stone"])?;
    }
    let metal = overrides
        .get("metal")
        .map(|v| material(v, &mut viewer))
        .transpose()?;
    let stone = overrides
        .get("stone")
        .map(|v| material(v, &mut viewer))
        .transpose()?;
    for part in stud() {
        let chosen = match (part.name, metal, stone) {
            ("stone", _, Some(stone)) => stone,
            ("stone", _, None) => part.material,
            (_, Some(metal), _) => metal,
            (_, None, _) => part.material,
        };
        let mesh = viewer.upload_mesh(&part.mesh).map_err(|e| viewer_error(&e))?;
        let handle = viewer.create_material(&chosen);
        let transforms: Vec<Affine3A> = part.transforms;
        viewer
            .add_draw(mesh, handle, &transforms)
            .map_err(|e| viewer_error(&e))?;
    }

    let view = params.get("view").cloned().unwrap_or(Value::Null);
    if !view.is_null() {
        check_fields(&view, &["tilt_deg", "turn_deg", "distance_mm"])?;
    }
    let tilt = number(&view, "tilt_deg", 0.0)?.to_radians();
    let turn = number(&view, "turn_deg", 0.0)?.to_radians();
    let distance = number(&view, "distance_mm", 36.0)?.max(1.0);
    let base = ViewerCamera::front(distance, 100.0, 36.0);
    let eye = Vec3::new(turn.sin() * tilt.cos(), tilt.sin(), turn.cos() * tilt.cos()) * distance;
    viewer
        .set_camera(&ViewerCamera {
            eye: eye.to_array(),
            ..base
        })
        .map_err(|e| refusal("viewer_bad_value", format!("`view`: {e}")))?;

    // The post is hidden: the shadow falls on the surface behind the visible parts.
    let contact_shadow = contact_opacity.map(|opacity| ContactShadow::under(&stud_visible_bounds(), opacity));
    viewer.set_style(&ViewerStyle {
        exposure,
        contact_shadow,
        ..ViewerStyle::default()
    });

    let still = capture_still(&mut viewer, width, height).map_err(|e| viewer_error(&e))?;
    let mut bytes = None;
    if let Some(file) = &out {
        let encoded = still.encode_png().map_err(|e| viewer_error(&e))?;
        write_atomic(file, &encoded)
            .map_err(|e| refusal("viewer_write_failed", format!("cannot write the still: {e}")))?;
        bytes = Some(encoded.len());
    }
    let stats = viewer.stats();
    Ok(json!({
        "width": still.width,
        "height": still.height,
        "hash": format!("{:016x}", still.hash()),
        "adapter": { "name": info.name, "backend": format!("{:?}", info.backend), "type": format!("{:?}", info.device_type) },
        "sample_count": viewer.sample_count(),
        "pipelines": viewer.pipeline_count(),
        "stats": {
            "pipelines_created": stats.pipelines_created,
            "frames_drawn": stats.frames_drawn,
            "draw_calls": stats.draw_calls,
            "instances_drawn": stats.instances_drawn,
            "triangles_drawn": stats.triangles_drawn,
            "material_writes": stats.material_writes,
        },
        "path": params.get("path"),
        "bytes": bytes,
    }))
}

pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The temp name stays `{path}.tmp{pid}`. `std::fs::write` follows a symlink planted
    // there and truncates the link target; unlink it, then create the file new.
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp{}", std::process::id()));
    let tmp = std::path::PathBuf::from(name);
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()));
    written
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("reason")?.as_str()
    }

    #[test]
    fn capabilities_name_every_family() {
        let mut world = World::new();
        let result = capabilities(&mut world, &json!({})).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(result["families"][1], "gem");
        assert_eq!(result["pipelines"], 11);
        assert_eq!(result["env_format_version"], 1);
    }

    #[test]
    fn bad_parameters_are_refused_before_any_device_is_made() {
        let mut world = World::new();
        for (params, expected) in [
            (json!({ "width": 0 }), "viewer_bad_size"),
            (json!({ "height": 100_000 }), "viewer_bad_size"),
            (json!({ "exposure": "bright" }), "viewer_bad_value"),
            (json!({ "nonsense": 1 }), "viewer_unknown_field"),
        ] {
            let error = render_still(&mut world, &params).expect_err("refused");
            assert_eq!(reason(&error), Some(expected), "{params}");
        }
    }

    #[test]
    fn an_env_path_outside_the_project_is_refused() {
        let mut world = World::new();
        let error = render_still(&mut world, &json!({ "env": "../x.renv" })).expect_err("refused");
        assert_eq!(reason(&error), Some("path_outside_project"));
        let error = env_info(&mut world, &json!({ "path": "../x.renv" })).expect_err("refused");
        assert_eq!(reason(&error), Some("path_outside_project"));
        assert!(env_info(&mut world, &json!({})).is_err());
    }

    /// corr-agent (ADR-5597): `.renv` paths were read with `std::fs::read`, so a FIFO under an
    /// allowed root (the temp dir is one) blocked the world thread forever (this test hung before
    /// the fix). Both readers now refuse anything but a regular file at once.
    #[cfg(unix)]
    #[test]
    fn an_env_path_that_is_a_pipe_is_refused_without_blocking() {
        let dir = regia_test_utils::unique_temp_dir("viewer-env-fifo");
        let fifo = dir.join("x.renv");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success());
        assert_eq!(made.ok(), Some(true), "mkfifo");
        let path = fifo.display().to_string();
        let mut world = World::new();
        let error = env_info(&mut world, &json!({ "path": path })).expect_err("a pipe");
        assert_eq!(reason(&error), Some("viewer_env_unreadable"), "{error:?}");
        let error = render_still(&mut world, &json!({ "env": path })).expect_err("a pipe");
        assert_eq!(reason(&error), Some("viewer_env_unreadable"), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_still_answers_a_stable_hash_and_constant_pipelines() {
        let mut world = World::new();
        let params = json!({
            "width": 64,
            "height": 64,
            "materials": {
                "metal": { "kind": "metal", "base_color": [0.9, 0.9, 0.9], "roughness": 0.2 },
                "stone": { "kind": "gem", "tint": [0.2, 0.3, 0.9] },
            },
            "view": { "tilt_deg": 10.0 },
            "contact_shadow": 0.4,
        });
        let first = match render_still(&mut world, &params) {
            Ok(result) => result,
            Err(error) => {
                // A machine without any adapter: the refusal is the documented one.
                assert_eq!(reason(&error), Some("viewer_no_adapter"), "{error:?}");
                return;
            }
        };
        let second = render_still(&mut world, &params).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(first["hash"], second["hash"]);
        assert_eq!(first["pipelines"], 11);
        assert_eq!(first["stats"]["pipelines_created"], 11);
        assert_eq!(first["stats"]["frames_drawn"], 1);
    }

    #[test]
    fn a_material_of_unknown_kind_or_field_is_refused() {
        let mut world = World::new();
        for (value, expected) in [
            (json!({ "metal": { "kind": "wood" } }), "viewer_bad_material"),
            (json!({ "metal": {} }), "viewer_bad_material"),
            (
                json!({ "metal": { "kind": "metal", "shiny": true } }),
                "viewer_unknown_field",
            ),
            (
                json!({ "metal": { "kind": "metal", "base_color": [1.0] } }),
                "viewer_bad_value",
            ),
            (
                json!({ "metal": { "kind": "anodized", "lut": [[0.0, 0.0, 0.0]] } }),
                "viewer_bad_value",
            ),
            (json!({ "ring": {} }), "viewer_unknown_field"),
        ] {
            match render_still(
                &mut world,
                &json!({ "width": 8, "height": 8, "materials": value }),
            ) {
                Err(error) => {
                    let got = reason(&error);
                    assert!(
                        got == Some(expected) || got == Some("viewer_no_adapter"),
                        "{value}: {error:?}"
                    );
                }
                Ok(result) => panic!("{value} rendered: {result}"),
            }
        }
    }

    #[test]
    fn a_still_writes_a_png_and_an_env_file_is_checked() {
        let mut world = World::new();
        let dir = regia_test_utils::unique_temp_dir("viewer_agent");
        let path = dir.join("still.png");
        let params = json!({ "width": 16, "height": 16, "path": path.to_string_lossy() });
        match render_still(&mut world, &params) {
            Ok(result) => {
                let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{e}"));
                assert_eq!(result["bytes"], bytes.len());
                assert_eq!(&bytes[1..4], b"PNG");
            }
            Err(error) => assert_eq!(reason(&error), Some("viewer_no_adapter"), "{error:?}"),
        }

        let env = dir.join("studio.renv");
        std::fs::write(&env, BakedEnv::uniform([1.0; 3]).to_bytes()).unwrap_or_else(|e| panic!("{e}"));
        let info = env_info(&mut world, &json!({ "path": env.to_string_lossy() }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(info["ok"], true);
        assert_eq!(info["face_size"], 4);

        let bad = dir.join("bad.renv");
        std::fs::write(&bad, b"nope").unwrap_or_else(|e| panic!("{e}"));
        let info = env_info(&mut world, &json!({ "path": bad.to_string_lossy() }))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(info["ok"], false);

        let error = env_info(
            &mut world,
            &json!({ "path": dir.join("missing.renv").to_string_lossy() }),
        )
        .expect_err("refused");
        assert_eq!(reason(&error), Some("viewer_env_unreadable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn headless_world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::seeing::GpuDisabled {
            code: "editor_headless".into(),
            reason: "the headless editor has no GPU adapter (started with --no-gpu)".into(),
            hint: "start it without --no-gpu".into(),
        });
        world
    }

    fn data_code(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    /// ADR-5180: a host that opened no GPU on purpose refuses a still with its own code.
    #[test]
    fn a_host_without_a_gpu_refuses_a_still() {
        let mut world = headless_world();
        let error = render_still(&mut world, &json!({ "width": 64, "height": 48 })).expect_err("refused");
        assert_eq!(data_code(&error), Some("editor_headless"), "{error:?}");
    }

    /// corr-agent finding 20 (ADR-5895): `contact_shadow` is documented as an opacity from 0 to 1,
    /// but any finite value was taken (the renderer clamped it silently) and it was read only after
    /// the device was made, so even a non-number got past the parameter check. It is now checked
    /// with the other parameters, before any device: a host without a GPU answers `viewer_bad_value`
    /// for it, not `editor_headless`.
    #[test]
    fn a_contact_shadow_outside_zero_to_one_is_refused_before_any_device() {
        let mut world = headless_world();
        for value in [json!(1.5), json!(-0.1), json!("dark"), json!(f64::MAX)] {
            let error =
                render_still(&mut world, &json!({ "contact_shadow": value.clone() })).expect_err("refused");
            assert_eq!(reason(&error), Some("viewer_bad_value"), "{value}: {error:?}");
        }
        for value in [json!(0.0), json!(0.4), json!(1.0)] {
            let error = render_still(&mut world, &json!({ "contact_shadow": value.clone() }))
                .expect_err("no GPU here");
            assert_eq!(data_code(&error), Some("editor_headless"), "{value}: {error:?}");
        }
    }

    /// A planted symlink at the predictable temp name must not be followed: `std::fs::write`
    /// truncates the link target, then the rename publishes that link as the still.
    #[cfg(unix)]
    #[test]
    fn write_atomic_does_not_follow_a_planted_tmp_symlink() {
        let dir = regia_test_utils::unique_temp_dir("viewer-atomic-link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious").expect("victim");
        let dest = dir.join("still.png");
        let tmp = {
            let mut name = dest.as_os_str().to_owned();
            name.push(format!(".tmp{}", std::process::id()));
            std::path::PathBuf::from(name)
        };
        std::os::unix::fs::symlink(&victim, &tmp).expect("plant link");
        write_atomic(&dest, b"png-bytes").expect("write");
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim"),
            "precious",
            "the planted temp symlink was followed"
        );
        assert_eq!(std::fs::read(&dest).expect("still"), b"png-bytes");
        assert!(
            dest.is_file() && !dest.is_symlink(),
            "the still must be a regular file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
