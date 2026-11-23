// SPDX-License-Identifier: MPL-2.0
//! A running game seen and driven from another process (the editor's Game tabs,
//! `book/agents/pie-net.md` § "Game tabs", ADR-7831): `play.frame` and `play.mute`.
//!
//! - `play.frame` answers the newest picture of the game, scaled down to fit `max_width` x
//!   `max_height` (box filter, aspect kept) and encoded as a fast PNG, with the frame number it was
//!   taken on. The picture comes from the renderer (the primary window's next present, as
//!   `seeing.screenshot` takes it) or, in an app without a renderer, from a [`FrameFeed`] the app
//!   fills itself (a headless tool, a test game).
//! - `play.mute` silences the game's output (its root audio bus) and puts back the bus's own mute
//!   when unmuted; in an app without audio it only records the state.
//!
//! The time scale is `play.set_time_scale`; pause and step are `play.pause`, `play.resume` and
//! `play.step`; input is `play.inject_input`.

use std::time::Instant;

use crossbeam_channel::Sender;
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, Response, RpcError};
use crate::seeing::Outcome;

/// Default `max_width` of `play.frame`.
pub const DEFAULT_MAX_WIDTH: u32 = 640;
/// Default `max_height` of `play.frame`.
pub const DEFAULT_MAX_HEIGHT: u32 = 360;
/// Smallest `max_width` / `max_height` `play.frame` accepts.
pub const MIN_SIDE: u32 = 16;
/// Largest `max_width` / `max_height` `play.frame` accepts.
pub const MAX_SIDE: u32 = 3840;

/// A picture an app without a renderer offers to `play.frame` (RGBA8, row-major, top row first).
/// An app with a renderer leaves it empty: the window's present is used.
#[derive(Resource, Debug, Clone, Default)]
pub struct FrameFeed {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes; empty for no picture yet.
    pub rgba: Vec<u8>,
}

impl FrameFeed {
    /// Whether it holds a whole picture.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.width > 0 && self.height > 0 && self.rgba.len() == self.width as usize * self.height as usize * 4
    }
}

/// What `play.mute` set, and the root bus's own mute to put back.
#[derive(Resource, Debug, Clone, Default)]
pub struct PlayMute {
    /// The game is muted by `play.mute`.
    pub muted: bool,
    /// The root bus was already muted before `play.mute` muted it (it stays muted on unmute).
    pub(crate) bus_was_muted: bool,
}

/// The size an image of `width` x `height` gets to fit inside `max_w` x `max_h`: the same aspect,
/// never larger than the source, at least 1 x 1.
#[must_use]
pub fn fit_size(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    if width <= max_w && height <= max_h {
        return (width, height);
    }
    let scale = (f64::from(max_w) / f64::from(width)).min(f64::from(max_h) / f64::from(height));
    let w = ((f64::from(width) * scale).floor() as u32).clamp(1, max_w);
    let h = ((f64::from(height) * scale).floor() as u32).clamp(1, max_h);
    (w, h)
}

/// Scales an RGBA8 image down to `out_w` x `out_h` with a box filter: each output pixel is the
/// average of the source pixels its footprint covers (whole pixels, at least one). Separable: the
/// source rows of one output row are summed first (one pass over contiguous memory), then the
/// columns of each output pixel.
#[must_use]
pub fn box_downscale(rgba: &[u8], width: u32, height: u32, out_w: u32, out_h: u32) -> Vec<u8> {
    let (w, h, ow, oh) = (width as usize, height as usize, out_w as usize, out_h as usize);
    if ow == w && oh == h {
        return rgba.to_vec();
    }
    let mut out = vec![0_u8; ow * oh * 4];
    if w == 0 || h == 0 || ow == 0 || oh == 0 || rgba.len() < w * h * 4 {
        return out;
    }
    let mut column = vec![0_u32; w * 4];
    for oy in 0..oh {
        let y0 = oy * h / oh;
        let y1 = ((oy + 1) * h / oh).max(y0 + 1).min(h);
        column.fill(0);
        for row in rgba[y0 * w * 4..y1 * w * 4].chunks_exact(w * 4) {
            for (sum, v) in column.iter_mut().zip(row) {
                *sum += u32::from(*v);
            }
        }
        let rows = (y1 - y0) as u32;
        let out_row = &mut out[oy * ow * 4..(oy + 1) * ow * 4];
        for (ox, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x0 = ox * w / ow;
            let x1 = ((ox + 1) * w / ow).max(x0 + 1).min(w);
            let mut sum = [0_u32; 4];
            for src in column[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                for (s, v) in sum.iter_mut().zip(src) {
                    *s += *v;
                }
            }
            let n = rows * (x1 - x0) as u32;
            for (o, s) in px.iter_mut().zip(sum) {
                *o = ((s + n / 2) / n) as u8;
            }
        }
    }
    out
}

/// Encodes RGBA8 as a PNG with the fast DEFLATE level (the Game tab's stream, ADR-7831 measured it).
///
/// # Errors
/// The encoder refused the image (a size that does not match the bytes).
pub fn fast_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}

/// The answer of `play.frame` for one picture.
fn frame_answer(
    rgba: &[u8],
    width: u32,
    height: u32,
    max: (u32, u32),
    frame: u64,
    source: &str,
) -> Result<Value, RpcError> {
    let _span = tracing::info_span!("play_frame_encode").entered();
    let started = Instant::now();
    let (w, h) = fit_size(width, height, max.0, max.1);
    let scaled = box_downscale(rgba, width, height, w, h);
    let png = fast_png(&scaled, w, h).map_err(RpcError::internal)?;
    Ok(json!({
        "frame": frame,
        "width": w,
        "height": h,
        "source_width": width,
        "source_height": height,
        "source": source,
        "mime": "image/png",
        "png_bytes": png.len(),
        "encode_ms": started.elapsed().as_secs_f64() * 1000.0,
        "png_base64": crate::image::base64(&png),
    }))
}

/// A picture waiting to be scaled and encoded for `play.frame`.
struct Picture {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    max: (u32, u32),
    frame: u64,
    source: &'static str,
}

/// Scales and encodes `picture` on a short-lived thread (so the game's frame does not pay for it)
/// and answers `reply`; encodes on this thread when no thread can be started.
fn encode_and_reply(picture: Picture, id: Value, reply: Sender<Response>) {
    let job = std::sync::Arc::new(std::sync::Mutex::new(Some((picture, id, reply))));
    let job_thread = std::sync::Arc::clone(&job);
    let spawned = std::thread::Builder::new()
        .name("play-frame-encode".into())
        .spawn(move || run_encode(&job_thread));
    if spawned.is_err() {
        run_encode(&job);
    }
}

/// The job slot `encode_and_reply` hands to the encode thread (or keeps when none starts).
type EncodeJob = std::sync::Mutex<Option<(Picture, Value, Sender<Response>)>>;

/// Takes the job out of its slot (once) and answers it.
fn run_encode(job: &EncodeJob) {
    let Some((picture, id, reply)) = job.lock().ok().and_then(|mut j| j.take()) else {
        return;
    };
    let answer = frame_answer(
        &picture.rgba,
        picture.width,
        picture.height,
        picture.max,
        picture.frame,
        picture.source,
    );
    let response = match answer {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::err(id, error),
    };
    let _ = reply.send(response);
}

/// One side limit of `play.frame` (`MIN_SIDE..=MAX_SIDE`, default `default`).
fn side(params: &Value, key: &str, default: u32) -> Result<u32, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Ok(default),
        Some(v) => u32::try_from(v)
            .ok()
            .filter(|v| (MIN_SIDE..=MAX_SIDE).contains(v))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{key}` must be an integer in {MIN_SIDE}..={MAX_SIDE}"))
            }),
    }
}

/// `play.frame`: the newest picture, scaled to fit and PNG-encoded.
pub(crate) fn frame(world: &mut World, params: &Value, id: Value, reply: Sender<Response>) -> Outcome {
    let max = match (
        side(params, "max_width", DEFAULT_MAX_WIDTH),
        side(params, "max_height", DEFAULT_MAX_HEIGHT),
    ) {
        (Ok(w), Ok(h)) => (w, h),
        (Err(e), _) | (_, Err(e)) => return Outcome::Done(Err(e)),
    };
    let frame = world.get_resource::<Time>().map_or(0, Time::frame_count);
    if let Some(feed) = world.get_resource::<FrameFeed>().filter(|f| f.ready()) {
        let picture = Picture {
            rgba: feed.rgba.clone(),
            width: feed.width,
            height: feed.height,
            max,
            frame,
            source: "feed",
        };
        encode_and_reply(picture, id, reply);
        return Outcome::Deferred;
    }
    let Some(mut requests) = world.get_resource_mut::<regia_render::ScreenshotRequests>() else {
        return Outcome::Done(Err(RpcError::new(
            protocol::UNSUPPORTED,
            "play_frame_no_source",
            "no renderer and no FrameFeed picture in this app",
            "add regia_render::RenderPlugin, or fill regia_agent::play_view::FrameFeed",
        )));
    };
    requests.request(
        None,
        Box::new(move |result| {
            if crate::server::take_abandoned(&reply) {
                return;
            }
            match result {
                Ok(img) => encode_and_reply(
                    Picture {
                        rgba: img.rgba,
                        width: img.width,
                        height: img.height,
                        max,
                        frame,
                        source: "window",
                    },
                    id,
                    reply,
                ),
                Err(e) => {
                    let _ = reply.send(Response::err(
                        id,
                        RpcError::internal(format!("capture failed: {e}")),
                    ));
                }
            }
        }),
    );
    Outcome::Deferred
}

/// The root bus (no parent) of the game's mixer.
fn root_bus(audio: &regia_audio::Audio) -> Option<(String, bool)> {
    audio
        .bus_status()
        .into_iter()
        .find(|b| b.parent.is_none())
        .map(|b| (b.name, b.muted))
}

/// `play.mute`: mutes (`muted: true`) or unmutes the game's output.
pub(crate) fn mute(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let muted = crate::params::opt_bool(params, "muted")?
        .ok_or_else(|| RpcError::invalid_params("`muted` (true or false) is required"))?;
    let mut state = world.remove_resource::<PlayMute>().unwrap_or_default();
    let before = state.muted;
    let mut bus = None;
    let mut failed = None;
    if let Some(mut audio) = world.get_resource_mut::<regia_audio::Audio>()
        && let Some((name, bus_muted)) = root_bus(&audio)
    {
        if muted && !state.muted {
            state.bus_was_muted = bus_muted;
        }
        let want = muted || state.bus_was_muted;
        match audio.set_bus_mute(&name, want) {
            Ok(_) => bus = Some(name),
            Err(e) => failed = Some(e.to_string()),
        }
    }
    if let Some(message) = failed {
        world.insert_resource(state);
        return Err(RpcError::internal(message));
    }
    if !muted {
        state.bus_was_muted = false;
    }
    state.muted = muted;
    world.insert_resource(state);
    Ok(json!({ "before": before, "muted": muted, "bus": bus }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_the_aspect_and_never_upscales() {
        assert_eq!(fit_size(1920, 1080, 640, 360), (640, 360));
        assert_eq!(fit_size(1920, 1200, 640, 360), (576, 360));
        assert_eq!(fit_size(320, 200, 640, 360), (320, 200));
        assert_eq!(fit_size(10_000, 1, 16, 16), (16, 1));
        assert_eq!(fit_size(0, 5, 16, 16), (0, 0));
    }

    #[test]
    fn box_downscale_averages_each_footprint() {
        // 4 x 2 -> 2 x 1: each output pixel averages a 2 x 2 block.
        let mut src = Vec::new();
        for v in [0_u8, 100, 200, 250, 0, 100, 200, 250] {
            src.extend([v, v, v, 255]);
        }
        let out = box_downscale(&src, 4, 2, 2, 1);
        assert_eq!(out, vec![50, 50, 50, 255, 225, 225, 225, 255]);
        // Same size: a copy.
        assert_eq!(box_downscale(&src, 4, 2, 4, 2), src);
    }

    #[test]
    fn fast_png_round_trips() {
        let rgba: Vec<u8> = (0..64 * 36 * 4).map(|i| (i % 251) as u8).collect();
        let png = fast_png(&rgba, 64, 36).expect("encodes");
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        assert_eq!((info.width, info.height), (64, 36));
        assert_eq!(&buf[..info.buffer_size()], &rgba[..]);
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.update();
        app
    }

    fn call_frame(world: &mut World, params: Value) -> Result<Value, RpcError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        match frame(world, &params, json!(1), tx) {
            Outcome::Done(result) => result,
            Outcome::Deferred => {
                let response = rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("answer");
                response
                    .error
                    .map_or_else(|| Ok(response.result.unwrap_or_default()), Err)
            }
        }
    }

    #[test]
    fn play_frame_answers_the_feed_scaled_with_its_frame_number() {
        let mut app = app();
        let world = app.world_mut();
        let err = call_frame(world, json!({})).expect_err("no source");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("play_frame_no_source"),
            "{err:?}"
        );
        world.insert_resource(FrameFeed {
            width: 64,
            height: 32,
            rgba: vec![200; 64 * 32 * 4],
        });
        let out = call_frame(world, json!({ "max_width": 32, "max_height": 32 })).expect("frame");
        assert_eq!(out["source"], "feed");
        assert_eq!(
            (out["width"].as_u64(), out["height"].as_u64()),
            (Some(32), Some(16))
        );
        assert_eq!(out["frame"], world.resource::<Time>().frame_count());
        let png = crate::image::base64_decode(out["png_base64"].as_str().expect("png")).expect("base64");
        assert_eq!(png.len() as u64, out["png_bytes"].as_u64().expect("bytes"));
        let bad = call_frame(world, json!({ "max_width": 4 })).expect_err("too small");
        assert_eq!(bad.code, protocol::INVALID_PARAMS);
    }

    #[test]
    fn play_mute_records_the_state_without_audio() {
        let mut app = app();
        let world = app.world_mut();
        let out = mute(world, &json!({ "muted": true })).expect("mute");
        assert_eq!(out, json!({ "before": false, "muted": true, "bus": null }));
        assert!(world.resource::<PlayMute>().muted);
        let out = mute(world, &json!({ "muted": false })).expect("unmute");
        assert_eq!(out["before"], true);
        assert!(!world.resource::<PlayMute>().muted);
        assert_eq!(
            mute(world, &json!({})).expect_err("required").code,
            protocol::INVALID_PARAMS
        );
    }

    /// The cost the Game tab's stream pays per frame (ADR-7831): run by hand with
    /// `cargo test -p regia_agent --lib play_view::tests::stream_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, prints timings"]
    fn stream_cost() {
        let (w, h) = (1280_u32, 720_u32);
        let mut rgba = vec![0_u8; (w * h * 4) as usize];
        let mut seed = 0x9e37_79b9_u32;
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed & 15) as u8;
            *px = [
                (x / 5) as u8 ^ noise,
                (y / 3) as u8,
                ((x + y) / 8) as u8 ^ noise,
                255,
            ];
        }
        let runs = 20;
        let t = Instant::now();
        let mut scaled = Vec::new();
        for _ in 0..runs {
            scaled = box_downscale(&rgba, w, h, 640, 360);
        }
        let scale_ms = t.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
        for (name, level) in [
            ("fast", png::Compression::Fast),
            ("balanced", png::Compression::Balanced),
            ("none", png::Compression::NoCompression),
        ] {
            let t = Instant::now();
            let mut size = 0;
            for _ in 0..runs {
                let mut bytes = Vec::new();
                let mut encoder = png::Encoder::new(&mut bytes, 640, 360);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_compression(level);
                let mut writer = encoder.write_header().expect("header");
                writer.write_image_data(&scaled).expect("data");
                drop(writer);
                size = bytes.len();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
            let t = Instant::now();
            let b64 = crate::image::base64(&vec![7_u8; size]);
            let b64_ms = t.elapsed().as_secs_f64() * 1000.0;
            println!(
                "png {name}: {ms:.2} ms, {size} bytes ({} base64, {b64_ms:.2} ms); scale 1280x720->640x360 {scale_ms:.2} ms; raw {} bytes",
                b64.len(),
                640 * 360 * 4
            );
        }
    }
}
