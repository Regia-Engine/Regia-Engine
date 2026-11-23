// SPDX-License-Identifier: MPL-2.0
//! `render.set_studio_environment`: the studio image-based light on the agent protocol
//! (`book/agents/studio-environment.md`, guide topic `studio-environment`, ADR-1653).
//!
//! A horizon-to-zenith gradient with a ground colour, or an OpenEXR equirect image, replaces the
//! runtime renderer's IBL (diffuse irradiance and specular prefilter) and the sky draw's
//! background; `none` returns to the default IBL. The renderer rebuilds only when the
//! parameters changed.

use regia_app::prelude::*;
use regia_render::{
    RenderDevice, RuntimeRenderer, STUDIO_EQUIRECT_MAX_TEXELS, StudioEquirect, StudioGradient,
};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// What the call asks for, after validation.
enum Environment {
    None,
    Gradient(StudioGradient),
    Equirect(StudioEquirect, u64),
}

fn radiance(params: &Value, key: &str) -> Result<[f32; 3], RpcError> {
    let bad = || RpcError::invalid_params(format!("`{key}` must be [r, g, b] radiance in cd/m², each >= 0"));
    let list = params.get(key).and_then(Value::as_array).ok_or_else(bad)?;
    if list.len() != 3 {
        return Err(bad());
    }
    let mut out = [0.0f32; 3];
    for (slot, v) in out.iter_mut().zip(list) {
        let f = v
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .ok_or_else(bad)?;
        *slot = f as f32;
    }
    Ok(out)
}

pub(crate) fn scale(params: &Value) -> Result<f32, RpcError> {
    match params.get("scale") {
        None | Some(Value::Null) => Ok(1.0),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| f as f32)
            .filter(|f| f.is_finite() && *f > 0.0)
            .ok_or_else(|| RpcError::invalid_params("`scale` must be a finite positive f32 number")),
    }
}

/// The first RGBA layer of an OpenEXR file as `(width, height, texels)`, unscaled.
pub(crate) fn read_exr_texels(path: &std::path::Path) -> Result<(u32, u32, Vec<[f32; 3]>), RpcError> {
    type Pixels = (usize, Vec<[f32; 3]>);
    let image = exr::prelude::read_first_rgba_layer_from_file(
        path,
        |size: exr::prelude::Vec2<usize>, _| -> Pixels {
            let count = size.width().saturating_mul(size.height());
            // Over the limit nothing is stored; the size check below refuses the image.
            (
                size.width(),
                vec![
                    [0.0; 3];
                    if count <= STUDIO_EQUIRECT_MAX_TEXELS {
                        count
                    } else {
                        0
                    }
                ],
            )
        },
        |image: &mut Pixels, pos: exr::prelude::Vec2<usize>, (r, g, b, _a): (f32, f32, f32, f32)| {
            if let Some(t) = image.1.get_mut(pos.y() * image.0 + pos.x()) {
                *t = [r, g, b];
            }
        },
    )
    .map_err(|e| RpcError::invalid_params(format!("cannot read `path` as an OpenEXR image: {e}")))?;
    let size = image.layer_data.size;
    let (_, texels) = image.layer_data.channel_data.pixels;
    let width = u32::try_from(size.width()).unwrap_or(u32::MAX);
    let height = u32::try_from(size.height()).unwrap_or(u32::MAX);
    Ok((width, height, texels))
}

/// Linear texels times `scale` as a [`StudioEquirect`]; non-finite and negative samples read as 0
/// and are counted. `what` names the parameter in an error.
fn equirect_from_texels(
    what: &str,
    width: u32,
    height: u32,
    mut texels: Vec<[f32; 3]>,
    scale: f32,
) -> Result<(StudioEquirect, u64), RpcError> {
    let mut replaced = 0u64;
    for t in &mut texels {
        for c in t.iter_mut() {
            let scaled = *c * scale;
            if c.is_finite() && *c >= 0.0 && scaled.is_finite() && scaled >= 0.0 {
                *c = scaled;
            } else {
                *c = 0.0;
                replaced += 1;
            }
        }
    }
    let equirect = StudioEquirect::new(width, height, texels)
        .map_err(|e| RpcError::invalid_params(format!("`{what}`: {e}")))?;
    Ok((equirect, replaced))
}

/// Reads the first RGBA layer of an OpenEXR file as linear texels times `scale`. Non-finite and
/// negative samples read as 0; returns how many were replaced.
pub(crate) fn read_exr(path: &std::path::Path, scale: f32) -> Result<(StudioEquirect, u64), RpcError> {
    let (width, height, texels) = read_exr_texels(path)?;
    equirect_from_texels("path", width, height, texels, scale)
}

/// Level 0 of a cooked HDR texture, as one equirect image. A cube is resampled to an equirect
/// four faces wide and two faces high. `asset` is a project-relative path or an asset id.
pub(crate) fn cooked_asset_image(
    world: &mut World,
    params: &Value,
) -> Result<regia_asset::hdr_texture::HdrImage, RpcError> {
    let Some(mut server) = world.get_resource_mut::<regia_asset::AssetServer>() else {
        return Err(RpcError::not_found(
            "no AssetServer resource in this app",
            "`asset` reads a cooked texture through the asset server; use `path` for a raw OpenEXR file",
        ));
    };
    let (path, id) = crate::assets::resolve_asset(&server, params)?;
    let handle = server
        .load_texture(id)
        .map_err(|e| RpcError::invalid_params(format!("cannot load {path} as a texture: {e}")))?;
    let Some(texture) = server.textures().get(&handle) else {
        return Err(RpcError::internal(format!(
            "{path} loaded but is not in the texture store"
        )));
    };
    let level = regia_asset::hdr_texture::decode_hdr_ktx2(&texture.ktx2)
        .map_err(|e| RpcError::invalid_params(format!("`asset` {path}: {e}")))?;
    if level.is_cube() {
        let face = level.faces[0].width;
        let (width, height) = cube_equirect_dimensions(face)?;
        regia_asset::hdr_texture::cube_to_equirect(&level.faces, width, height)
            .map_err(|e| RpcError::invalid_params(format!("`asset` {path}: {e}")))
    } else {
        level
            .faces
            .into_iter()
            .next()
            .ok_or_else(|| RpcError::invalid_params(format!("`asset` {path}: the texture has no image")))
    }
}

/// Target dimensions, checked before the resampler allocates its output.
fn cube_equirect_dimensions(face: u32) -> Result<(u32, u32), RpcError> {
    face.checked_mul(4)
        .zip(face.checked_mul(2))
        .filter(|(width, height)| {
            face > 0
                && u64::from(*width) * u64::from(*height) <= STUDIO_EQUIRECT_MAX_TEXELS as u64
        })
        .ok_or_else(|| RpcError::invalid_params(format!(
                "`asset` cube face {face} must produce a non-empty equirect of at most {STUDIO_EQUIRECT_MAX_TEXELS} texels"
        )))
}

/// A cooked HDR texture as a studio environment, with the same scaling and sanitizing as
/// [`read_exr`].
pub(crate) fn read_cooked_asset(
    world: &mut World,
    params: &Value,
    scale: f32,
) -> Result<(StudioEquirect, u64), RpcError> {
    let image = cooked_asset_image(world, params)?;
    equirect_from_texels("asset", image.width, image.height, image.texels, scale)
}

fn parse(world: &mut World, params: &Value) -> Result<Environment, RpcError> {
    let kind = params
        .get("kind")
        .ok_or_else(|| RpcError::invalid_params("pass `kind`: \"gradient\", \"equirect\" or \"none\""))?
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("`kind` must be a string"))?;
    match kind {
        "none" => Ok(Environment::None),
        "gradient" => {
            let mut gradient = StudioGradient::new(
                radiance(params, "zenith")?,
                radiance(params, "horizon")?,
                radiance(params, "ground")?,
            );
            match params.get("falloff") {
                None | Some(Value::Null) => {}
                Some(v) => {
                    gradient.falloff = v
                        .as_f64()
                        .ok_or_else(|| RpcError::invalid_params("`falloff` must be a number"))?
                        as f32;
                }
            }
            gradient
                .validate()
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
            Ok(Environment::Gradient(gradient))
        }
        "equirect" if params.get("asset").is_some_and(|v| !v.is_null()) => {
            if params.get("path").is_some_and(|v| !v.is_null()) {
                return Err(RpcError::invalid_params(
                    "pass `asset` (a cooked texture) or `path` (a raw OpenEXR file), not both",
                ));
            }
            let (image, replaced) = read_cooked_asset(world, params, scale(params)?)?;
            Ok(Environment::Equirect(image, replaced))
        }
        "equirect" => {
            let path = params.get("path").and_then(Value::as_str).ok_or_else(|| {
                RpcError::invalid_params(
                    "`kind` \"equirect\" needs `path` (an OpenEXR file) or `asset` (a cooked HDR texture)",
                )
            })?;
            let path = crate::render_out::confine_path(world, "path", std::path::Path::new(path))?;
            let (image, replaced) = read_exr(&path, scale(params)?)?;
            Ok(Environment::Equirect(image, replaced))
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown `kind` \"{other}\": use \"gradient\", \"equirect\" or \"none\""
        ))),
    }
}

/// `render.set_studio_environment { kind, … }`.
///
/// `kind` is `"gradient"` (`zenith`, `horizon`, `ground` as `[r, g, b]` cd/m², optional
/// `falloff`), `"equirect"` (`path` to an OpenEXR file, or `asset`, a cooked HDR texture by
/// project-relative path or id; optional `scale`) or `"none"` (the
/// default IBL again); `resolution`, `budget` and `authority` (`"atmosphere"`, the default, or
/// `"studio"`, ADR-2016) apply from this call on. Returns
/// `{ kind, rebuilt, key, base_width, authority, width?, height?, replaced_samples? }`:
/// `rebuilt` is false when the installed environment already had these parameters, `key` is its
/// cache key as 16 hex digits (`null` for `none`).
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let environment = parse(world, params)?;
    let resolution = crate::studio_lighting::parse_resolution(params)?;
    let authority = crate::studio_lighting::parse_authority(params)?;
    let budget = match params.get("budget").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|b| u32::try_from(b).ok())
                .ok_or_else(|| RpcError::invalid_params("`budget` must be a width in texels"))?,
        ),
    };
    if world.get_resource::<RenderDevice>().is_none() {
        return Err(RpcError::not_found(
            "no RenderDevice resource in this app",
            "add regia_render::RenderPlugin and let it create the device (headless apps have none)",
        ));
    }
    if world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
             FrameRenderer::set_studio_gradient)",
        ));
    }
    world.resource_scope::<RuntimeRenderer, _>(|world, mut runtime| {
        let Some(device) = world.get_resource::<RenderDevice>() else {
            return Err(RpcError::internal("the RenderDevice went away"));
        };
        let renderer = &mut runtime.0;
        if let Some(resolution) = resolution {
            renderer.set_studio_env_resolution(resolution);
        }
        if let Some(budget) = budget {
            renderer.set_studio_env_budget(budget);
        }
        // Before the install: a switch to the studio authority rebuilds maps the atmosphere
        // wrote into (ADR-2016).
        if let Some(authority) = authority {
            renderer.set_environment_authority(authority);
        }
        let mut out = match &environment {
            Environment::None => {
                let _ = renderer.set_studio_gradient(device, None);
                json!({ "kind": "none", "rebuilt": false })
            }
            Environment::Gradient(g) => {
                let rebuilt = renderer
                    .set_studio_gradient(device, Some(g))
                    .map_err(|e| RpcError::invalid_params(e.to_string()))?;
                json!({ "kind": "gradient", "rebuilt": rebuilt })
            }
            Environment::Equirect(image, replaced) => {
                let rebuilt = renderer.set_studio_equirect(device, Some(image));
                json!({
                    "kind": "equirect",
                    "rebuilt": rebuilt,
                    "width": image.width(),
                    "height": image.height(),
                    "replaced_samples": replaced,
                })
            }
        };
        out["key"] = renderer
            .studio_environment_key()
            .map_or(Value::Null, |k| json!(format!("{k:016x}")));
        out["authority"] = json!(renderer.environment_authority().name());
        out["base_width"] = renderer
            .studio_env_info()
            .map_or(Value::Null, |info| json!(info.base_width));
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_over_the_equirect_cap_is_rejected_before_resample() {
        assert_eq!(cube_equirect_dimensions(2048).expect("at cap"), (8192, 4096));
        for face in [2049, 4096, 8192, 16384, u32::MAX, 0] {
            assert!(cube_equirect_dimensions(face).is_err(), "face {face}");
        }
    }

    #[test]
    fn overflowing_scale_is_rejected() {
        for value in [1.0e39, 1.0e-50] {
            assert!(scale(&json!({ "scale": value })).is_err(), "scale {value}");
        }
    }

    #[test]
    fn overflowing_scaled_sample_is_zeroed_and_counted() {
        let (image, replaced) =
            equirect_from_texels("path", 2, 1, vec![[3.0e38, 0.0, 1.0], [1.0, -1.0, f32::NAN]], 2.0)
                .expect("sanitized scaled samples");
        assert_eq!(replaced, 3, "one overflow, one negative, one NaN");
        let (expected, _) = equirect_from_texels("path", 2, 1, vec![[0.0, 0.0, 2.0], [2.0, 0.0, 0.0]], 1.0)
            .expect("expected");
        assert_eq!(image.cache_key(), expected.cache_key());
    }

    fn gradient_params() -> Value {
        json!({
            "kind": "gradient",
            "zenith": [2400.0, 4200.0, 8000.0],
            "horizon": [2600.0, 2800.0, 3000.0],
            "ground": [600.0, 600.0, 600.0],
        })
    }

    #[test]
    fn a_gradient_parses_with_its_default_falloff() {
        let mut world = World::new();
        let Ok(Environment::Gradient(g)) = parse(&mut world, &gradient_params()) else {
            panic!("a gradient");
        };
        assert_eq!(g.zenith, [2400.0, 4200.0, 8000.0]);
        assert_eq!(g.falloff, 1.0);
        let mut p = gradient_params();
        p["falloff"] = json!(2.5);
        let Ok(Environment::Gradient(g)) = parse(&mut world, &p) else {
            panic!("a gradient");
        };
        assert_eq!(g.falloff, 2.5);
        assert!(matches!(
            parse(&mut world, &json!({ "kind": "none" })),
            Ok(Environment::None)
        ));
    }

    #[test]
    fn bad_parameters_are_refused() {
        let mut world = World::new();
        for bad in [
            json!({}),
            json!({ "kind": 3 }),
            json!({ "kind": "sphere" }),
            json!({ "kind": "gradient" }),
            json!({ "kind": "equirect" }),
        ] {
            assert!(parse(&mut world, &bad).is_err(), "{bad}");
        }
        for (key, value) in [
            ("zenith", json!([1.0, 2.0])),
            ("horizon", json!([1.0, -2.0, 3.0])),
            ("ground", json!("dark")),
            ("falloff", json!(0.0)),
            ("falloff", json!(99.0)),
            ("falloff", json!("slow")),
        ] {
            let mut p = gradient_params();
            p[key] = value.clone();
            assert!(parse(&mut world, &p).is_err(), "{key} {value}");
        }
    }

    #[test]
    fn without_a_renderer_the_call_is_a_structured_error() {
        let mut world = World::new();
        let err = set(&mut world, &gradient_params()).expect_err("no device");
        assert!(err.message.contains("RenderDevice"), "{}", err.message);
        // Parameters are checked before the renderer is looked up.
        let err = set(&mut world, &json!({ "kind": "gradient" })).expect_err("bad");
        assert!(err.message.contains("zenith"), "{}", err.message);
    }

    #[test]
    fn an_exr_equirect_reads_scales_and_sanitizes() {
        let dir = regia_test_utils::unique_temp_dir("studio-environment");
        let path = dir.join("sky.exr");
        exr::prelude::write_rgba_file(&path, 4, 2, |x, y| {
            if (x, y) == (3, 1) {
                (f32::NAN, -1.0, 2.0, 1.0)
            } else {
                (x as f32, y as f32, 0.5, 1.0)
            }
        })
        .expect("write exr");
        let (image, replaced) = read_exr(&path, 2.0).expect("read");
        assert_eq!((image.width(), image.height()), (4, 2));
        assert_eq!(replaced, 2, "a NaN and a negative sample");
        // The same file read twice has the same cache key; another scale changes it.
        let (again, _) = read_exr(&path, 2.0).expect("read");
        assert_eq!(image.cache_key(), again.cache_key());
        let (other, _) = read_exr(&path, 3.0).expect("read");
        assert_ne!(image.cache_key(), other.cache_key());
        assert!(read_exr(&dir.join("missing.exr"), 1.0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Order-2 spherical harmonics of an equirect image (CPU projection, solid-angle weighted).
    fn sh9(width: u32, height: u32, texels: &[[f32; 3]]) -> [[f64; 3]; 9] {
        let mut sh = [[0.0f64; 3]; 9];
        for y in 0..height {
            let theta = (f64::from(y) + 0.5) / f64::from(height) * std::f64::consts::PI;
            let dw = theta.sin()
                * (std::f64::consts::TAU / f64::from(width))
                * (std::f64::consts::PI / f64::from(height));
            for x in 0..width {
                let d = regia_asset::hdr_texture::equirect_direction(
                    (x as f32 + 0.5) / width as f32,
                    (y as f32 + 0.5) / height as f32,
                );
                let basis = sh_basis(d);
                let t = texels[(y * width + x) as usize];
                for (acc, b) in sh.iter_mut().zip(basis) {
                    for (a, v) in acc.iter_mut().zip(t) {
                        *a += f64::from(v) * b * dw;
                    }
                }
            }
        }
        sh
    }

    fn sh_basis(d: [f32; 3]) -> [f64; 9] {
        let (x, y, z) = (f64::from(d[0]), f64::from(d[1]), f64::from(d[2]));
        [
            0.282_095,
            0.488_603 * y,
            0.488_603 * z,
            0.488_603 * x,
            1.092_548 * x * y,
            1.092_548 * y * z,
            0.315_392 * (3.0 * z * z - 1.0),
            1.092_548 * x * z,
            0.546_274 * (x * x - y * y),
        ]
    }

    /// Irradiance toward `n` from order-2 SH (the cosine-lobe convolution, bands 0 to 2).
    fn irradiance(sh: &[[f64; 3]; 9], n: [f32; 3]) -> [f64; 3] {
        let pi = std::f64::consts::PI;
        let band = [
            pi,
            2.0 * pi / 3.0,
            2.0 * pi / 3.0,
            2.0 * pi / 3.0,
            pi / 4.0,
            pi / 4.0,
            pi / 4.0,
            pi / 4.0,
            pi / 4.0,
        ];
        let basis = sh_basis(n);
        let mut out = [0.0f64; 3];
        for ((c, b), w) in sh.iter().zip(basis).zip(band) {
            for (o, v) in out.iter_mut().zip(c) {
                *o += v * b * w;
            }
        }
        out
    }

    #[test]
    fn a_cooked_asset_gives_the_irradiance_of_its_raw_exr() {
        use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
        let dir = regia_test_utils::unique_temp_dir("studio-environment-asset");
        let (w, h) = (64u32, 32u32);
        let sky = |x: usize, y: usize| {
            let d = regia_asset::hdr_texture::equirect_direction(
                (x as f32 + 0.5) / w as f32,
                (y as f32 + 0.5) / h as f32,
            );
            // A bright sky on one side, a dim ground: a broad patch of contrast.
            let lobe = (d[0] * 0.8 + d[1] * 0.6).max(0.0).powi(4);
            (
                400.0 + 3000.0 * d[1].max(0.0) + 9000.0 * lobe,
                500.0 + 2500.0 * d[1].max(0.0) + 6000.0 * lobe,
                900.0 + 2000.0 * d[1].max(0.0) + 3000.0 * lobe,
                1.0,
            )
        };
        let assets = dir.join("assets/sky");
        std::fs::create_dir_all(&assets).expect("dirs");
        exr::prelude::write_rgba_file(assets.join("day.exr"), w as usize, h as usize, sky).expect("exr");
        let report = cook_project(CookOptions {
            project_root: dir.clone(),
            platform: host_platform().to_string(),
            incremental: true,
        })
        .expect("cook");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        let mut world = World::new();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: dir.clone(),
            platform: host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        let id = server.id_for_path("assets/sky/day.exr").expect("the cooked sky");
        world.insert_resource(server);

        let (rw, rh, raw) = read_exr_texels(&assets.join("day.exr")).expect("raw");
        let by_path = cooked_asset_image(&mut world, &json!({ "asset": "sky/day.exr" })).expect("by path");
        let by_id = cooked_asset_image(&mut world, &json!({ "asset": id.to_string() })).expect("by id");
        assert_eq!(by_path, by_id);
        assert_eq!((by_path.width, by_path.height), (rw, rh));
        let want = sh9(rw, rh, &raw);
        let got = sh9(by_path.width, by_path.height, &by_path.texels);
        let mut worst = 0.0f64;
        for n in [
            [0.0, 1.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
        ] {
            let (a, b) = (irradiance(&want, n), irradiance(&got, n));
            for (x, y) in a.iter().zip(b) {
                worst = worst.max((x - y).abs() / x.abs().max(1.0));
            }
        }
        assert!(worst < 0.02, "irradiance differs by {worst}");

        // The parameter checks: both sources at once, no asset server, an unknown asset.
        let err = parse(
            &mut world,
            &json!({ "kind": "equirect", "asset": "sky/day.exr", "path": "x.exr" }),
        )
        .err()
        .expect("both");
        assert!(err.message.contains("not both"), "{}", err.message);
        assert!(cooked_asset_image(&mut world, &json!({ "asset": "sky/missing.exr" })).is_err());
        let mut bare = World::new();
        let err = cooked_asset_image(&mut bare, &json!({ "asset": "sky/day.exr" })).expect_err("no server");
        assert!(err.message.contains("AssetServer"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
