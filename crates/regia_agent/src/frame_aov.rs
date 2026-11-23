// SPDX-License-Identifier: MPL-2.0
//! `seeing.frame_aov`: the frame's shading normal and roughness (book `agents/frame-aov.md`,
//! guide topic `frame_aov`, ADR-2951).
//!
//! Renders the extracted frame twice through the forward shader's normal and roughness outputs
//! on the app's runtime renderer (`regia_render::frame_aov::capture_frame_aov`) and returns the
//! chosen AOV as a PNG with statistics over the covered pixels (or a rectangle of them). What it
//! shows is what the forward pass shaded with: the projected decal layer, composited after the
//! forward pass, is not in it (the spec 27 decal row records that).

use regia_app::prelude::*;
use regia_render::frame_aov::{FrameAov, capture_frame_aov};
use regia_render::{CapturedImage, RenderDevice};
use serde_json::{Map, Value, json};

use crate::protocol::{INTERNAL_ERROR, NOT_FOUND, RpcError, UNSUPPORTED};

/// Largest capture edge.
const MAX_EDGE: u64 = 4096;

/// Which AOV the PNG shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Normal,
    Roughness,
}

/// The parsed request: size, shown AOV, optional statistics rectangle `[x0, y0, x1, y1)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Request {
    width: u32,
    height: u32,
    shown: Shown,
    rect: Option<[u32; 4]>,
}

fn edge(map: &Map<String, Value>, name: &str, default: u32) -> Result<u32, RpcError> {
    match map.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .filter(|n| (16..=MAX_EDGE).contains(n))
            .map(|n| n as u32)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` must be an integer from 16 to {MAX_EDGE}"))
            }),
    }
}

fn parse(params: &Value) -> Result<Request, RpcError> {
    let empty = Map::new();
    let map = match params {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => return Err(RpcError::invalid_params("params must be an object")),
    };
    let width = edge(map, "width", 640)?;
    let height = edge(map, "height", 360)?;
    let shown = match map.get("aov") {
        None | Some(Value::Null) => Shown::Normal,
        Some(Value::String(s)) if s == "normal" => Shown::Normal,
        Some(Value::String(s)) if s == "roughness" => Shown::Roughness,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`aov` must be \"normal\" or \"roughness\"",
            ));
        }
    };
    let rect = match map.get("rect") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) if a.len() == 4 => {
            let mut r = [0u32; 4];
            for (slot, v) in r.iter_mut().zip(a) {
                *slot = v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| RpcError::invalid_params("`rect` holds four pixel integers"))?;
            }
            if r[2] <= r[0] || r[3] <= r[1] || r[2] > width || r[3] > height {
                return Err(RpcError::invalid_params(
                    "`rect` is [x0, y0, x1, y1) with x0 < x1 <= width and y0 < y1 <= height",
                ));
            }
            Some(r)
        }
        Some(_) => return Err(RpcError::invalid_params("`rect` must be [x0, y0, x1, y1]")),
    };
    Ok(Request {
        width,
        height,
        shown,
        rect,
    })
}

/// The PNG of `shown`: the normal as `n * 0.5 + 0.5`, the roughness as grey; uncovered pixels
/// transparent black.
fn image(aov: &FrameAov, shown: Shown) -> CapturedImage {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut rgba = Vec::with_capacity(aov.coverage.len() * 4);
    for (i, covered) in aov.coverage.iter().enumerate() {
        if !covered {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        match shown {
            Shown::Normal => {
                let n = aov.normal[i];
                rgba.extend_from_slice(&[
                    byte(n[0] * 0.5 + 0.5),
                    byte(n[1] * 0.5 + 0.5),
                    byte(n[2] * 0.5 + 0.5),
                    255,
                ]);
            }
            Shown::Roughness => {
                let r = byte(aov.roughness[i]);
                rgba.extend_from_slice(&[r, r, r, 255]);
            }
        }
    }
    CapturedImage {
        width: aov.width,
        height: aov.height,
        rgba,
    }
}

/// Statistics over the covered pixels inside `rect` (the whole frame without one).
fn stats(aov: &FrameAov, rect: Option<[u32; 4]>) -> Value {
    let [x0, y0, x1, y1] = rect.unwrap_or([0, 0, aov.width, aov.height]);
    let mut n = 0usize;
    let mut sum_n = [0.0f64; 3];
    let (mut sum_r, mut sum_m) = (0.0f64, 0.0f64);
    let (mut min_r, mut max_r) = (f32::INFINITY, f32::NEG_INFINITY);
    for y in y0..y1.min(aov.height) {
        for x in x0..x1.min(aov.width) {
            let i = (y * aov.width + x) as usize;
            if !aov.coverage[i] {
                continue;
            }
            n += 1;
            for (k, s) in sum_n.iter_mut().enumerate() {
                *s += f64::from(aov.normal[i][k]);
            }
            sum_r += f64::from(aov.roughness[i]);
            sum_m += f64::from(aov.metallic[i]);
            min_r = min_r.min(aov.roughness[i]);
            max_r = max_r.max(aov.roughness[i]);
        }
    }
    if n == 0 {
        return json!({ "covered": 0 });
    }
    let len = (sum_n[0] * sum_n[0] + sum_n[1] * sum_n[1] + sum_n[2] * sum_n[2])
        .sqrt()
        .max(1e-12);
    let inv = 1.0 / n as f64;
    json!({
        "covered": n,
        "normal_mean": [sum_n[0] / len, sum_n[1] / len, sum_n[2] / len],
        "roughness": { "mean": sum_r * inv, "min": min_r, "max": max_r },
        "metallic_mean": sum_m * inv,
    })
}

fn no_frame() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "frame_aov_no_frame",
        "the app has no runtime renderer or no extracted frame to draw",
        "run it with regia_render::RenderPlugin and step one frame (play.step), then call again",
    )
}

/// `seeing.frame_aov { width?, height?, aov?, rect? }` → `{ width, height, aov, covered,
/// normal_mean, roughness: { mean, min, max }, metallic_mean, rect, mime, png_base64 }`.
pub(crate) fn frame_aov(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let request = parse(params)?;
    if !world.contains_resource::<RenderDevice>() {
        return Err(RpcError::new(
            NOT_FOUND,
            "frame_aov_no_device",
            "this app has no render device",
            "run it in an app with regia_render::RenderPlugin",
        ));
    }
    if !world.contains_resource::<regia_render::runtime::RuntimeRenderer>() {
        return Err(no_frame());
    }
    let aov = world.resource_scope(
        |world, mut runtime: Mut<regia_render::runtime::RuntimeRenderer>| {
            let (Some(device), Some(extracted)) = (
                world.get_resource::<RenderDevice>(),
                world.get_resource::<regia_render::runtime::ExtractedFrame>(),
            ) else {
                return Err(no_frame());
            };
            if !extracted.is_renderable() {
                return Err(no_frame());
            }
            let view = extracted.window_views().first().map_or(0, |(i, _)| *i);
            runtime.0.set_outdoor(extracted.outdoor.clone());
            runtime.0.set_valley_mist(extracted.valley_mist.clone());
            runtime.0.set_interior_scene(extracted.enclosure.clone());
            let frame = extracted.frame_scene(view, request.width, request.height);
            capture_frame_aov(&mut runtime.0, device, &frame).map_err(|e| {
                RpcError::new(
                    INTERNAL_ERROR,
                    "frame_aov_render_failed",
                    format!("an AOV frame failed: {e}"),
                    "check the log for the GPU error",
                )
            })
        },
    )?;
    let png = crate::image::png_base64(&image(&aov, request.shown)).map_err(RpcError::internal)?;
    let mut out = stats(&aov, request.rect);
    if let Value::Object(map) = &mut out {
        map.insert("width".into(), json!(aov.width));
        map.insert("height".into(), json!(aov.height));
        map.insert(
            "aov".into(),
            json!(match request.shown {
                Shown::Normal => "normal",
                Shown::Roughness => "roughness",
            }),
        );
        map.insert("rect".into(), json!(request.rect));
        map.insert("mime".into(), json!("image/png"));
        map.insert("png_base64".into(), json!(png));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: &RpcError) -> String {
        e.data.clone().unwrap_or_default()["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn params_parse_and_bad_ones_are_refused() {
        let r = parse(&json!({})).expect("defaults");
        assert_eq!(
            (r.width, r.height, r.shown, r.rect),
            (640, 360, Shown::Normal, None)
        );
        let r = parse(&json!({ "width": 64, "height": 32, "aov": "roughness", "rect": [1, 2, 10, 20] }))
            .expect("full");
        assert_eq!(r.shown, Shown::Roughness);
        assert_eq!(r.rect, Some([1, 2, 10, 20]));
        assert!(parse(&json!({ "aov": "albedo" })).is_err());
        assert!(parse(&json!({ "width": 8 })).is_err());
        assert!(parse(&json!({ "width": 64, "height": 32, "rect": [10, 0, 5, 4] })).is_err());
        assert!(parse(&json!({ "width": 64, "height": 32, "rect": [0, 0, 65, 4] })).is_err());
        assert!(parse(&json!([1])).is_err());
    }

    #[test]
    fn stats_cover_only_shaded_pixels_inside_the_rect() {
        let aov = FrameAov {
            width: 2,
            height: 1,
            normal: vec![[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
            roughness: vec![0.25, 0.75],
            metallic: vec![0.0, 1.0],
            coverage: vec![true, true],
        };
        let all = stats(&aov, None);
        assert_eq!(all["covered"], 2);
        assert!((all["roughness"]["mean"].as_f64().unwrap_or(0.0) - 0.5).abs() < 1e-6);
        let left = stats(&aov, Some([0, 0, 1, 1]));
        assert_eq!(left["covered"], 1);
        assert_eq!(left["normal_mean"], json!([0.0, 1.0, 0.0]));
        let img = image(&aov, Shown::Roughness);
        assert_eq!(&img.rgba[..4], &[64, 64, 64, 255]);
    }

    #[test]
    fn without_a_device_or_a_frame_the_codes_say_so() {
        let mut world = World::new();
        assert_eq!(
            code(&frame_aov(&mut world, &json!({})).expect_err("no device")),
            "frame_aov_no_device"
        );
        match RenderDevice::shared_headless() {
            Ok(device) => world.insert_resource(device),
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
        }
        assert_eq!(
            code(&frame_aov(&mut world, &json!({})).expect_err("no renderer")),
            "frame_aov_no_frame"
        );
    }
}
