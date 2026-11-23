// SPDX-License-Identifier: MPL-2.0
//! The studio frame on the agent protocol (`book/agents/studio-environment.md`,
//! `book/agents/local-capture.md`, ADR-2000): the environment's effective resolution, the image
//! light's ground plane, the background chosen apart from the lighting, the render debug view and
//! the local reflection captures.

use regia_app::prelude::*;
use regia_render::{
    DebugView, EnvGroundPlane, EnvironmentAuthority, LocalCaptureDesc, LocalCaptureError, LocalCaptureInfo,
    RenderDevice, RuntimeRenderer, StudioBackground, StudioEnvResolution, StudioGradient,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls it directly)",
    )
}

fn no_device() -> RpcError {
    RpcError::not_found(
        "no RenderDevice resource in this app",
        "add regia_render::RenderPlugin and let it create the device (headless apps have none)",
    )
}

/// Runs `f` on the runtime renderer and the device.
fn with_renderer<R>(
    world: &mut World,
    f: impl FnOnce(&RenderDevice, &mut regia_render::FrameRenderer) -> Result<R, RpcError>,
) -> Result<R, RpcError> {
    if world.get_resource::<RenderDevice>().is_none() {
        return Err(no_device());
    }
    if world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(no_renderer());
    }
    world.resource_scope::<RuntimeRenderer, _>(|world, mut runtime| {
        let Some(device) = world.get_resource::<RenderDevice>() else {
            return Err(RpcError::internal("the RenderDevice went away"));
        };
        f(device, &mut runtime.0)
    })
}

fn renderer_mut(world: &mut World) -> Result<Mut<'_, RuntimeRenderer>, RpcError> {
    world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(no_renderer)
}

fn array3(params: &Value, key: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let bad = || RpcError::invalid_params(format!("`{key}` must be [x, y, z] finite numbers"));
    let Some(value) = params.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let list = value.as_array().ok_or_else(bad)?;
    if list.len() != 3 {
        return Err(bad());
    }
    let mut out = [0.0f32; 3];
    for (slot, v) in out.iter_mut().zip(list) {
        *slot = v.as_f64().filter(|f| f.is_finite()).ok_or_else(bad)? as f32;
    }
    Ok(Some(out))
}

fn number(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number"))),
    }
}

/// `authority` (`"atmosphere"` or `"studio"`), `None` when absent.
pub(crate) fn parse_authority(params: &Value) -> Result<Option<EnvironmentAuthority>, RpcError> {
    match params.get("authority").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(EnvironmentAuthority::from_name)
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`authority` must be \"atmosphere\" or \"studio\"")),
    }
}

/// `resolution` (`"standard"`, `"studio"` or `"source"`) or `base_width` (texels).
pub(crate) fn parse_resolution(params: &Value) -> Result<Option<StudioEnvResolution>, RpcError> {
    if let Some(value) = params.get("base_width").filter(|v| !v.is_null()) {
        return value
            .as_u64()
            .and_then(|w| u32::try_from(w).ok())
            .filter(|w| *w > 0)
            .map(|w| Some(StudioEnvResolution::Width(w)))
            .ok_or_else(|| RpcError::invalid_params("`base_width` must be a width in texels"));
    }
    match params.get("resolution").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => match v.as_str() {
            Some("standard") => Ok(Some(StudioEnvResolution::Standard)),
            Some("studio") => Ok(Some(StudioEnvResolution::Studio)),
            Some("source") => Ok(Some(StudioEnvResolution::Source)),
            _ => Err(RpcError::invalid_params(
                "`resolution` must be \"standard\", \"studio\" or \"source\"",
            )),
        },
    }
}

fn plane_json(plane: &EnvGroundPlane) -> Value {
    json!({
        "height": plane.height,
        "radius": plane.radius,
        "center": plane.center,
        "albedo": plane.albedo,
        "sun_bounce": plane.sun_bounce,
    })
}

/// `render.get_studio_environment`: what the frames read now.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let runtime = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let renderer = &runtime.0;
    let requested = renderer.studio_env_resolution();
    let installed = renderer.studio_env_info();
    let resolution = renderer.environment_resolution();
    Ok(json!({
        "key": renderer.studio_environment_key().map(|k| format!("{k:016x}")),
        "requested": {
            "resolution": requested.name(),
            "width": if let StudioEnvResolution::Width(w) = requested { Some(w) } else { None },
            "budget": renderer.studio_env_budget(),
        },
        "effective": resolution.map(|(width, height, mips)| json!({
            "width": width,
            "height": height,
            "mips": mips,
        })),
        "source": installed.and_then(|info| info.source).map(|(w, h)| json!([w, h])),
        "gpu_bytes": installed.map(|info| info.gpu_bytes),
        "ground": {
            "plane": renderer.environment_ground().as_ref().map(plane_json),
            "active": renderer.environment_ground_active(),
        },
        "background": renderer.studio_background().name(),
        "authority": renderer.environment_authority().name(),
    }))
}

fn parse_plane(params: &Value) -> Result<Option<EnvGroundPlane>, RpcError> {
    if crate::params::opt_bool(params, "enabled")? == Some(false) {
        return Ok(None);
    }
    let mut plane = EnvGroundPlane::default();
    if let Some(height) = number(params, "height")? {
        plane.height = height;
    }
    plane.radius = number(params, "radius")?;
    if let Some(value) = params.get("center").filter(|v| !v.is_null()) {
        let bad = || RpcError::invalid_params("`center` must be [x, z] finite numbers");
        let list = value.as_array().filter(|l| l.len() == 2).ok_or_else(bad)?;
        for (slot, v) in plane.center.iter_mut().zip(list) {
            *slot = v.as_f64().filter(|f| f.is_finite()).ok_or_else(bad)? as f32;
        }
    }
    if let Some(albedo) = array3(params, "albedo")? {
        plane.albedo = albedo;
    }
    if let Some(value) = params.get("sun_bounce").filter(|v| !v.is_null()) {
        plane.sun_bounce = value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`sun_bounce` must be a boolean"))?;
    }
    plane
        .validate()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    Ok(Some(plane))
}

/// `render.set_environment_ground { enabled?, height?, radius?, center?, albedo?, sun_bounce? }`.
pub(crate) fn set_ground(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let plane = parse_plane(params)?;
    let mut runtime = renderer_mut(world)?;
    runtime
        .0
        .set_environment_ground(plane)
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    Ok(json!({
        "plane": plane.as_ref().map(plane_json),
        "active": runtime.0.environment_ground_active(),
    }))
}

fn parse_background(world: &World, params: &Value) -> Result<StudioBackground, RpcError> {
    let kind = params.get("kind").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("pass `kind`: \"environment\", \"solid\", \"gradient\" or \"image\"")
    })?;
    let required = |key: &str| {
        array3(params, key)?
            .ok_or_else(|| RpcError::invalid_params(format!("`{kind}` needs `{key}` [r, g, b]")))
    };
    let background = match kind {
        "environment" => StudioBackground::Environment,
        "solid" => StudioBackground::Solid(required("color")?),
        "gradient" => {
            let mut gradient =
                StudioGradient::new(required("zenith")?, required("horizon")?, required("ground")?);
            if let Some(falloff) = number(params, "falloff")? {
                gradient.falloff = falloff;
            }
            StudioBackground::Gradient(gradient)
        }
        "image" => {
            let path = params
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`image` needs `path` (an OpenEXR file)"))?;
            let path = crate::render_out::confine_path(world, "path", std::path::Path::new(path))?;
            let scale = crate::studio_environment::scale(params)?;
            StudioBackground::Image(crate::studio_environment::read_exr(&path, scale)?.0)
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown `kind` \"{other}\": use \"environment\", \"solid\", \"gradient\" or \"image\""
            )));
        }
    };
    background
        .validate()
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    Ok(background)
}

/// `render.set_studio_background { kind, color?, zenith?, horizon?, ground?, falloff?, path?, scale? }`.
pub(crate) fn set_background(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let background = parse_background(world, params)?;
    let name = background.name();
    with_renderer(world, |device, renderer| {
        renderer
            .set_studio_background(device, background)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Ok(json!({ "background": name }))
    })
}

fn debug_view_name(mode: regia_render::DebugViewMode) -> &'static str {
    use regia_render::DebugViewMode as M;
    match mode {
        M::Lit => "lit",
        M::Wireframe => "wireframe",
        M::UvChecker { .. } => "uv_checker",
        M::TexelDensity { .. } => "texel_density",
        M::MaterialId => "material_id",
        M::CloudShadowMap => "cloud_shadow_map",
        M::ImageLight => "image_light",
        M::ImageLightOcclusion => "image_light_occlusion",
        M::LightmapDensity => "lightmap_density",
        M::LightmapOnly => "lightmap_only",
        M::LightmapInvalid => "lightmap_invalid",
        M::Unlit => "unlit",
        M::Normals => "normals",
        M::BaseColor => "base_color",
        M::Roughness => "roughness",
        M::Metallic => "metallic",
        M::AmbientOcclusion => "ambient_occlusion",
        M::Overdraw => "overdraw",
        M::LightComplexity => "light_complexity",
        M::ShadowCascades => "shadow_cascades",
        M::LodColor => "lod_color",
        M::MotionVectors => "motion_vectors",
        M::MipLevel => "mip_level",
    }
}

/// The names `render.set_debug_view` takes.
const DEBUG_VIEWS: [&str; 23] = [
    "lit",
    "wireframe",
    "uv_checker",
    "texel_density",
    "material_id",
    "cloud_shadow_map",
    "image_light",
    "image_light_occlusion",
    "lightmap_density",
    "lightmap_only",
    "lightmap_invalid",
    // ADR-7211.
    "unlit",
    "normals",
    "base_color",
    "roughness",
    "metallic",
    "ambient_occlusion",
    "overdraw",
    "light_complexity",
    "shadow_cascades",
    "lod_color",
    "motion_vectors",
    "mip_level",
];

fn no_debug_view() -> RpcError {
    RpcError::not_found(
        "no DebugView resource in this app",
        "add regia_render::RenderPlugin",
    )
}

/// `render.get_debug_view`.
pub(crate) fn get_debug_view(world: &mut World) -> Result<Value, RpcError> {
    let view = world.get_resource::<DebugView>().ok_or_else(no_debug_view)?;
    Ok(json!({ "mode": debug_view_name(view.mode), "modes": DEBUG_VIEWS }))
}

/// `render.set_debug_view { mode, tile_m?, target_px_per_m?, tolerance? }`.
pub(crate) fn set_debug_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("pass `mode`, a name from render.get_debug_view"))?;
    let mode = DebugView::mode_from_params(
        name,
        number(params, "tile_m")?,
        number(params, "target_px_per_m")?,
        number(params, "tolerance")?,
    )
    .ok_or_else(|| {
        RpcError::invalid_params(format!(
            "unknown debug view `{name}`: use {}",
            DEBUG_VIEWS.join(", ")
        ))
    })?;
    let mut view = world.get_resource_mut::<DebugView>().ok_or_else(no_debug_view)?;
    view.mode = mode;
    Ok(json!({ "mode": name }))
}

fn capture_json(info: &LocalCaptureInfo) -> Value {
    json!({
        "id": info.desc.id,
        "center": info.desc.center,
        "half_extent": info.desc.half_extent,
        "weight": info.desc.weight,
        "fade": info.desc.fade,
        "baked": info.baked,
        "pending": info.pending,
        "bakes": info.bakes,
        "coverage": info.coverage,
    })
}

fn capture_error(error: LocalCaptureError) -> RpcError {
    match error {
        LocalCaptureError::Unknown(id) => RpcError::not_found(
            format!("no local capture {id}"),
            "render.list_local_captures lists them",
        ),
        other => RpcError::invalid_params(other.to_string()),
    }
}

fn capture_id(params: &Value) -> Result<u64, RpcError> {
    params
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("pass `id`, a whole number"))
}

fn parse_capture(params: &Value) -> Result<LocalCaptureDesc, RpcError> {
    let center =
        array3(params, "center")?.ok_or_else(|| RpcError::invalid_params("pass `center` [x, y, z]"))?;
    let half_extent = array3(params, "half_extent")?
        .ok_or_else(|| RpcError::invalid_params("pass `half_extent` [x, y, z]"))?;
    let mut desc = LocalCaptureDesc::new(capture_id(params)?, center, half_extent);
    if let Some(weight) = number(params, "weight")? {
        desc.weight = weight;
    }
    if let Some(fade) = number(params, "fade")? {
        desc.fade = fade;
    }
    desc.validate().map_err(capture_error)?;
    Ok(desc)
}

/// `render.place_local_capture { id, center, half_extent, weight?, fade? }`.
pub(crate) fn place_capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let desc = parse_capture(params)?;
    let id = desc.id;
    let mut runtime = renderer_mut(world)?;
    runtime.0.place_local_capture(desc).map_err(capture_error)?;
    Ok(json!({ "id": id, "baked": false, "captures": runtime.0.local_captures().len() }))
}

/// `render.bake_local_capture { id? }`: queues the bake for the next frame.
pub(crate) fn bake_capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = match params.get("id").filter(|v| !v.is_null()) {
        None => None,
        Some(_) => Some(capture_id(params)?),
    };
    let mut runtime = renderer_mut(world)?;
    let queued = runtime.0.request_local_capture_bake(id).map_err(capture_error)?;
    Ok(json!({ "queued": queued }))
}

/// `render.list_local_captures`.
pub(crate) fn list_captures(world: &mut World) -> Result<Value, RpcError> {
    let runtime = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let list: Vec<Value> = runtime.0.local_captures().iter().map(capture_json).collect();
    Ok(json!({ "captures": list, "max": regia_render::MAX_LOCAL_CAPTURES }))
}

/// `render.remove_local_capture { id }`.
pub(crate) fn remove_capture(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = capture_id(params)?;
    let mut runtime = renderer_mut(world)?;
    if !runtime.0.remove_local_capture(id) {
        return Err(capture_error(LocalCaptureError::Unknown(id)));
    }
    Ok(json!({ "removed": id, "captures": runtime.0.local_captures().len() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<RuntimeRenderer>();
        world.init_resource::<DebugView>();
        world
    }

    #[test]
    fn the_authority_parses_by_name_and_reads_back_as_the_default() {
        assert_eq!(parse_authority(&json!({})).ok(), Some(None));
        assert_eq!(parse_authority(&json!({ "authority": null })).ok(), Some(None));
        assert_eq!(
            parse_authority(&json!({ "authority": "atmosphere" })).ok(),
            Some(Some(EnvironmentAuthority::Atmosphere))
        );
        assert_eq!(
            parse_authority(&json!({ "authority": "studio" })).ok(),
            Some(Some(EnvironmentAuthority::Studio))
        );
        for bad in [json!("Studio"), json!("sky"), json!(1), json!(true)] {
            assert!(parse_authority(&json!({ "authority": bad })).is_err(), "{bad}");
        }
        let mut world = world();
        let state = get(&mut world).expect("get");
        assert_eq!(
            state["authority"],
            json!("atmosphere"),
            "the default keeps v0.2.8's frames"
        );
    }

    #[test]
    fn a_resolution_parses_by_name_or_width() {
        assert_eq!(parse_resolution(&json!({})).ok(), Some(None));
        for (value, want) in [
            ("standard", StudioEnvResolution::Standard),
            ("studio", StudioEnvResolution::Studio),
            ("source", StudioEnvResolution::Source),
        ] {
            assert_eq!(
                parse_resolution(&json!({ "resolution": value })).ok(),
                Some(Some(want))
            );
        }
        assert_eq!(
            parse_resolution(&json!({ "base_width": 2048 })).ok(),
            Some(Some(StudioEnvResolution::Width(2048)))
        );
        assert!(parse_resolution(&json!({ "resolution": "huge" })).is_err());
        assert!(parse_resolution(&json!({ "resolution": 2048 })).is_err());
        for bad in [json!(0), json!(-1), json!(1.5), json!(true)] {
            assert!(parse_resolution(&json!({ "base_width": bad })).is_err());
        }
    }

    #[test]
    fn the_ground_plane_round_trips_and_refuses_bad_values() {
        let mut world = world();
        let reply = set_ground(
            &mut world,
            &json!({ "height": 0.5, "radius": 4.0, "center": [1.0, 2.0], "albedo": [0.3, 0.3, 0.3], "sun_bounce": false }),
        )
        .expect("set");
        assert_eq!(reply["plane"]["radius"], json!(4.0));
        assert_eq!(reply["active"], json!(false), "no studio IBL is installed");
        let state = get(&mut world).expect("get");
        assert_eq!(state["ground"]["plane"]["center"], json!([1.0, 2.0]));
        assert_eq!(state["ground"]["plane"]["sun_bounce"], json!(false));
        for bad in [
            json!({ "height": "low" }),
            json!({ "radius": 0.0 }),
            json!({ "center": [1.0] }),
            json!({ "albedo": [2.0, 0.0, 0.0] }),
            json!({ "sun_bounce": 1 }),
        ] {
            assert!(set_ground(&mut world, &bad).is_err(), "{bad}");
        }
        let off = set_ground(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(off["plane"], Value::Null);
    }

    #[test]
    fn a_background_parses_every_kind_and_refuses_the_rest() {
        let world = world();
        assert!(matches!(
            parse_background(&world, &json!({ "kind": "environment" })),
            Ok(StudioBackground::Environment)
        ));
        assert!(matches!(
            parse_background(&world, &json!({ "kind": "solid", "color": [10.0, 20.0, 30.0] })),
            Ok(StudioBackground::Solid(_))
        ));
        assert!(matches!(
            parse_background(
                &world,
                &json!({ "kind": "gradient", "zenith": [1.0, 1.0, 1.0], "horizon": [2.0, 2.0, 2.0], "ground": [3.0, 3.0, 3.0], "falloff": 2.0 })
            ),
            Ok(StudioBackground::Gradient(_))
        ));
        for bad in [
            json!({}),
            json!({ "kind": "plaid" }),
            json!({ "kind": "solid" }),
            json!({ "kind": "solid", "color": [-1.0, 0.0, 0.0] }),
            json!({ "kind": "gradient", "zenith": [1.0, 1.0, 1.0] }),
            json!({ "kind": "image" }),
        ] {
            assert!(parse_background(&world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_debug_view_is_set_by_name() {
        let mut world = world();
        assert_eq!(get_debug_view(&mut world).expect("get")["mode"], json!("lit"));
        for (name, mode) in [
            ("lightmap_density", regia_render::DebugViewMode::LightmapDensity),
            ("lightmap_only", regia_render::DebugViewMode::LightmapOnly),
            ("lightmap_invalid", regia_render::DebugViewMode::LightmapInvalid),
        ] {
            set_debug_view(&mut world, &json!({ "mode": name })).expect("set");
            assert_eq!(world.resource::<DebugView>().mode, mode);
            let got = get_debug_view(&mut world).expect("get");
            assert_eq!(got["mode"], json!(name));
            assert!(got["modes"].as_array().expect("modes").contains(&json!(name)));
        }
        set_debug_view(&mut world, &json!({ "mode": "image_light" })).expect("set");
        assert_eq!(
            world.resource::<DebugView>().mode,
            regia_render::DebugViewMode::ImageLight
        );
        assert_eq!(
            get_debug_view(&mut world).expect("get")["mode"],
            json!("image_light")
        );
        set_debug_view(&mut world, &json!({ "mode": "image_light_occlusion" })).expect("set");
        assert!(set_debug_view(&mut world, &json!({ "mode": "plasma" })).is_err());
        assert!(set_debug_view(&mut world, &json!({})).is_err());
    }

    #[test]
    fn captures_are_placed_queued_listed_and_removed() {
        let mut world = world();
        let place = json!({ "id": 3, "center": [0.0, 1.0, 0.0], "half_extent": [2.0, 1.0, 2.0] });
        assert_eq!(
            place_capture(&mut world, &place).expect("place")["captures"],
            json!(1)
        );
        let list = list_captures(&mut world).expect("list");
        assert_eq!(list["captures"][0]["baked"], json!(false));
        assert_eq!(list["captures"][0]["fade"], json!(0.5));
        assert_eq!(
            bake_capture(&mut world, &json!({})).expect("bake")["queued"],
            json!(1)
        );
        let list = list_captures(&mut world).expect("list");
        assert_eq!(list["captures"][0]["pending"], json!(true));
        let missing = bake_capture(&mut world, &json!({ "id": 9 })).expect_err("unknown");
        assert!(missing.message.contains('9'), "{}", missing.message);
        assert!(remove_capture(&mut world, &json!({ "id": 3 })).is_ok());
        assert!(remove_capture(&mut world, &json!({ "id": 3 })).is_err());
        for bad in [
            json!({ "id": 1, "center": [0.0, 0.0, 0.0] }),
            json!({ "center": [0.0, 0.0, 0.0], "half_extent": [1.0, 1.0, 1.0] }),
            json!({ "id": 1, "center": [0.0, 0.0, 0.0], "half_extent": [0.0, 1.0, 1.0] }),
            json!({ "id": 1, "center": [0.0, 0.0, 0.0], "half_extent": [1.0, 1.0, 1.0], "weight": 2.0 }),
        ] {
            assert!(place_capture(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn without_a_renderer_the_calls_are_structured_errors() {
        let mut world = World::new();
        assert!(
            get(&mut world)
                .expect_err("none")
                .message
                .contains("RuntimeRenderer")
        );
        assert!(
            set_background(&mut world, &json!({ "kind": "environment" }))
                .expect_err("none")
                .message
                .contains("RenderDevice")
        );
        assert!(list_captures(&mut world).is_err());
        assert!(
            get_debug_view(&mut world)
                .expect_err("none")
                .message
                .contains("DebugView")
        );
    }
}
