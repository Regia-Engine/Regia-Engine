// SPDX-License-Identifier: MPL-2.0
//! Video capture presets for `render.out_start` and `render.capture_plan` (ADR-2449,
//! `book/agents/capture-presets.md`).
//!
//! Hosting sites re-encode what they are given, so a clip that is already soft or blocky comes
//! back worse. A capture preset bundles the choices that keep a clip sharp through that:
//!
//! - **Render big, deliver exact.** The frames render at the delivery size times a supersampling
//!   factor and average down (the render-out's `supersample`), so edges and foliage are clean
//!   before any codec sees them.
//! - **Master first.** The render-out writes a mezzanine movie (ProRes 4444) or lossless frames.
//! - **Deliver at a high bitrate.** [`delivery_args`] builds the `ffmpeg` command that turns the
//!   master into a delivery file at a bitrate well above the hosting default, with BT.709 colour
//!   tags and fast-start. The engine never runs it: it only writes the arguments, so a build with
//!   no `ffmpeg` still plans a capture.
//! - **Deterministic camera.** The camera moves on a shot sequence (`render.sequence`, a
//!   `.shots.ron` file): every pose is a pure function of the clock, and the render-out steps the
//!   clock by exactly one frame, so two renders of the same file and request give the same frames.
//!
//! Everything here is a pure function of the request.

use std::borrow::Cow;

use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

use super::plan::{MAX_RENDER_PIXELS, Request};

/// A delivery codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (`libx264`): plays everywhere.
    H264,
    /// HEVC (`libx265`): the same look at about two thirds of the H.264 bitrate.
    Hevc,
    /// AV1 (`libsvtav1`): about half the H.264 bitrate.
    Av1,
}

impl Codec {
    /// Parses a codec name (`h264`, `hevc`, `av1`).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "h264" => Some(Self::H264),
            "hevc" | "h265" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    /// The codec's wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    /// Bitrate relative to H.264 for the same look.
    fn scale(self) -> f32 {
        match self {
            Self::H264 => 1.0,
            Self::Hevc => 0.65,
            Self::Av1 => 0.5,
        }
    }
}

/// One capture preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapturePreset {
    /// Wire name.
    pub name: &'static str,
    /// What it is for.
    pub description: &'static str,
    /// Delivered width, pixels.
    pub width: u32,
    /// Delivered height, pixels.
    pub height: u32,
    /// Frame rate (numerator over one).
    pub fps: u32,
    /// Supersampling factor of the render-out.
    pub supersample: u32,
    /// Master format (a render-out `format`).
    pub master: &'static str,
    /// H.264 delivery bitrate, megabits per second.
    pub h264_mbps: f32,
}

/// The presets, in the order they are listed.
pub const PRESETS: &[CapturePreset] = &[
    CapturePreset {
        name: "showcase_4k60",
        description: "4K 60 fps for a showcase video: 2x2 supersampled, ProRes 4444 master, 85 Mbps H.264 delivery",
        width: 3840,
        height: 2160,
        fps: 60,
        supersample: 2,
        master: "prores",
        h264_mbps: 85.0,
    },
    CapturePreset {
        name: "showcase_4k30",
        description: "4K 30 fps: 2x2 supersampled, ProRes 4444 master, 55 Mbps H.264 delivery",
        width: 3840,
        height: 2160,
        fps: 30,
        supersample: 2,
        master: "prores",
        h264_mbps: 55.0,
    },
    CapturePreset {
        name: "showcase_1440p60",
        description: "1440p 60 fps: 2x2 supersampled, ProRes 4444 master, 40 Mbps H.264 delivery",
        width: 2560,
        height: 1440,
        fps: 60,
        supersample: 2,
        master: "prores",
        h264_mbps: 40.0,
    },
];

/// The preset called `name`.
pub fn preset(name: &str) -> Option<&'static CapturePreset> {
    PRESETS.iter().find(|p| p.name == name)
}

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn unknown_preset(name: &str) -> RpcError {
    let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
    refused(
        "capture_unknown_preset",
        format!("unknown capture preset {name:?}"),
        &format!("one of: {}", names.join(", ")),
    )
}

/// `params` with the preset's render-out settings filled in for every key the request did not
/// give (an explicit key always wins). Without `preset` the request is returned unchanged.
///
/// # Errors
/// `preset` is not a string, or names no preset (`capture_unknown_preset`).
pub fn with_preset(params: &Value) -> Result<Cow<'_, Value>, RpcError> {
    let Some(value) = params.get("preset").filter(|v| !v.is_null()) else {
        return Ok(Cow::Borrowed(params));
    };
    let name = value
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("`preset` must be a string"))?;
    let preset = preset(name).ok_or_else(|| unknown_preset(name))?;
    let mut merged = params.as_object().cloned().unwrap_or_default();
    merged.remove("preset");
    for (key, value) in [
        ("width", json!(preset.width)),
        ("height", json!(preset.height)),
        ("fps", json!(preset.fps)),
        ("supersample", json!(preset.supersample)),
        ("format", json!(preset.master)),
    ] {
        merged.entry(key).or_insert(value);
    }
    Ok(Cow::Owned(Value::Object(merged)))
}

/// The `ffmpeg` arguments (after the program name) that turn the master movie `master` into the
/// delivery file `output`: `codec` at `mbps` megabits per second (capped at 1.25 times that, with
/// a two-second buffer; AV1 takes the target alone, SVT-AV1 allows a cap only in constant-quality mode), 4:2:0 BT.709 tagged limited range, fast-start. Pure.
pub fn delivery_args(codec: Codec, mbps: f32, master: &str, output: &str) -> Vec<String> {
    // SVT-AV1 accepts a target up to 100 Mbps.
    let mbps = if codec == Codec::Av1 {
        mbps.min(100.0)
    } else {
        mbps
    };
    let rate = format!("{}M", mbps.round().max(1.0) as u32);
    let ceiling = format!("{}M", (mbps * 1.25).round().max(1.0) as u32);
    let buffer = format!("{}M", (mbps * 2.0).round().max(1.0) as u32);
    let mut args: Vec<String> = ["-hide_banner", "-y", "-i", master]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    args.extend(
        ["-vf", "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"]
            .iter()
            .map(|s| (*s).to_string()),
    );
    let video: &[&str] = match codec {
        Codec::H264 => &["-c:v", "libx264", "-preset", "slow", "-profile:v", "high"],
        Codec::Hevc => &["-c:v", "libx265", "-preset", "slow", "-tag:v", "hvc1"],
        Codec::Av1 => &["-c:v", "libsvtav1", "-preset", "5"],
    };
    args.extend(video.iter().map(|s| (*s).to_string()));
    // SVT-AV1 takes a maximum rate only in constant-quality mode, so AV1 gets the target alone.
    let capped: &[(&str, &str)] = if codec == Codec::Av1 {
        &[("-b:v", rate.as_str())]
    } else {
        &[
            ("-b:v", rate.as_str()),
            ("-maxrate", ceiling.as_str()),
            ("-bufsize", buffer.as_str()),
        ]
    };
    for (key, value) in capped.iter().copied().chain([
        ("-colorspace", "bt709"),
        ("-color_primaries", "bt709"),
        ("-color_trc", "bt709"),
        ("-color_range", "tv"),
        ("-c:a", "aac"),
        ("-b:a", "320k"),
        ("-movflags", "+faststart"),
    ]) {
        args.push(key.into());
        args.push(value.into());
    }
    args.push(output.into());
    args
}

/// `render.capture_plan`: with no `preset`, lists the presets. With one, the `render.out_start`
/// parameters it implies, the delivery command, and the sizes. Reads nothing and writes nothing.
///
/// Params: `preset`; `name` (file stem, default `showcase`); `codec` (`h264` default, `hevc`,
/// `av1`); `bitrate_mbps` (override the delivery bitrate); `duration_s` or `frames`; `sequence`
/// (the `.shots.ron` path the camera moves on, echoed into the steps); and any `render.out_start`
/// parameter, which overrides the preset's.
pub(crate) fn plan(params: &Value) -> Result<Value, RpcError> {
    let listed: Vec<Value> = PRESETS
        .iter()
        .map(|p| {
            json!({
                "name": p.name, "description": p.description, "width": p.width, "height": p.height,
                "fps": p.fps, "supersample": p.supersample, "master": p.master,
                "h264_mbps": p.h264_mbps,
                "render_pixels": u64::from(p.width) * u64::from(p.height) * u64::from(p.supersample).pow(2),
            })
        })
        .collect();
    if params.get("preset").is_none_or(Value::is_null) {
        // The listing reads nothing else; a wrong-typed parameter is still refused.
        crate::params::check_types("render.capture_plan", params)?;
        return Ok(json!({ "presets": listed, "codecs": ["h264", "hevc", "av1"] }));
    }
    let merged = with_preset(params)?;
    let name = crate::params::opt_str(params, "preset")?.unwrap_or_default();
    let preset = preset(name).ok_or_else(|| unknown_preset(name))?;
    let stem = match params.get("name") {
        None | Some(Value::Null) => "showcase",
        Some(Value::String(s))
            if !s.is_empty()
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
        {
            s.as_str()
        }
        Some(_) => {
            return Err(refused(
                "capture_bad_name",
                "`name` must be 1-64 letters, digits, `_` or `-`",
                "it becomes the file stem of the master and the delivery file",
            ));
        }
    };
    let codec = match params.get("codec") {
        None | Some(Value::Null) => Codec::H264,
        Some(Value::String(s)) => Codec::parse(s).ok_or_else(|| {
            refused(
                "capture_unknown_codec",
                format!("unknown codec {s:?}"),
                "h264, hevc or av1",
            )
        })?,
        Some(_) => return Err(RpcError::invalid_params("`codec` must be a string")),
    };
    let mbps = match params.get("bitrate_mbps") {
        None | Some(Value::Null) => preset.h264_mbps * codec.scale(),
        Some(v) => v
            .as_f64()
            .filter(|m| m.is_finite() && (1.0..=500.0).contains(m))
            .ok_or_else(|| RpcError::invalid_params("`bitrate_mbps` must be a number in 1..=500"))?
            as f32,
    };
    // Validate exactly as render.out_start will, so the plan never promises a refused job.
    let mut out_start = merged.as_ref().clone();
    if let Some(object) = out_start.as_object_mut() {
        for key in ["name", "codec", "bitrate_mbps", "sequence"] {
            object.remove(key);
        }
        object
            .entry("out")
            .or_insert_with(|| json!(format!("renders/{stem}")));
        object.entry("name").or_insert_with(|| json!(stem));
        if !object.contains_key("frames") && !object.contains_key("duration_s") {
            object.insert("duration_s".into(), json!(10));
        }
    }
    let request = Request::parse(&out_start)?;
    let render_pixels =
        u64::from(request.width) * u64::from(request.height) * u64::from(request.supersample).pow(2);
    let master_ext = request.format.extension();
    let master_file = format!("{stem}.{master_ext}");
    let delivery_file = format!("{stem}_delivery.mp4");
    let ffmpeg = delivery_args(codec, mbps, &master_file, &delivery_file);
    let mut steps = Vec::new();
    if let Some(sequence) = params.get("sequence").filter(|v| !v.is_null()) {
        let path = sequence
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`sequence` must be a path string"))?;
        steps.push(json!({
            "method": "render.sequence",
            "file": path,
            "params": { "action": "load", "ron": format!("<the text of {path}>"), "play": true },
            "why": "the camera moves on the shot sequence: a pure function of the clock, so the render is repeatable",
        }));
    }
    steps.push(json!({ "method": "render.out_start", "params": out_start }));
    steps.push(json!({
        "run": "ffmpeg",
        "args": ffmpeg,
        "why": "after the job: master to a high-bitrate delivery file (the engine does not run this)",
    }));
    Ok(json!({
        "preset": preset.name,
        "width": request.width,
        "height": request.height,
        "fps": request.fps_num,
        "frames": request.frames,
        "supersample": request.supersample,
        "render_pixels": render_pixels,
        "render_pixel_limit": MAX_RENDER_PIXELS,
        "master": request.format.name(),
        "master_file": master_file,
        "delivery": {
            "codec": codec.name(),
            "bitrate_mbps": mbps,
            "file": delivery_file,
            "ffmpeg_args": ffmpeg,
        },
        "steps": steps,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::plan::Format;
    use super::*;

    #[test]
    fn presets_fit_the_render_out_limits_and_parse() {
        for p in PRESETS {
            // `Request::parse` applies the preset itself.
            let request =
                Request::parse(&json!({ "preset": p.name, "out": "o", "frames": 2 })).expect(p.name);
            assert_eq!((request.width, request.height), (p.width, p.height));
            assert_eq!(request.supersample, p.supersample);
            assert_eq!(request.fps_num, p.fps);
            assert_eq!(request.format, Format::parse(p.master).expect("format"));
            assert!(
                u64::from(p.width) * u64::from(p.height) * u64::from(p.supersample).pow(2)
                    <= MAX_RENDER_PIXELS
            );
        }
    }

    /// Pin: the showcase 4K preset's numbers. A change here changes every clip made with it.
    #[test]
    fn showcase_4k60_pin() {
        let p = preset("showcase_4k60").expect("preset");
        assert_eq!((p.width, p.height, p.fps, p.supersample), (3840, 2160, 60, 2));
        assert_eq!(p.master, "prores");
        assert_eq!(p.h264_mbps, 85.0);
    }

    #[test]
    fn explicit_keys_win_and_no_preset_changes_nothing() {
        let params =
            json!({ "preset": "showcase_4k60", "out": "o", "frames": 1, "fps": 30, "supersample": 3 });
        let merged = with_preset(&params).expect("merged");
        assert_eq!(merged["fps"], 30);
        assert_eq!(merged["supersample"], 3);
        assert_eq!(merged["width"], 3840);
        assert!(merged.get("preset").is_none());
        let plain = json!({ "out": "o", "frames": 1 });
        assert!(matches!(with_preset(&plain), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn unknown_preset_codec_and_name_are_refused_with_a_reason() {
        let reason = |e: RpcError| {
            e.data.expect("data")["reason"]
                .as_str()
                .expect("reason")
                .to_string()
        };
        assert_eq!(
            reason(with_preset(&json!({ "preset": "8k_imax" })).expect_err("refused")),
            "capture_unknown_preset"
        );
        assert_eq!(
            reason(plan(&json!({ "preset": "showcase_4k60", "codec": "vp9" })).expect_err("refused")),
            "capture_unknown_codec"
        );
        assert_eq!(
            reason(plan(&json!({ "preset": "showcase_4k60", "name": "../x" })).expect_err("refused")),
            "capture_bad_name"
        );
        assert!(plan(&json!({ "preset": "showcase_4k60", "bitrate_mbps": 0 })).is_err());
        assert!(plan(&json!({ "preset": 5 })).is_err());
    }

    #[test]
    fn delivery_args_are_a_high_bitrate_tagged_encode() {
        let args = delivery_args(Codec::H264, 85.0, "a.mov", "a.mp4");
        let has = |k: &str, v: &str| args.windows(2).any(|w| w[0] == k && w[1] == v);
        assert!(has("-b:v", "85M"));
        assert!(has("-maxrate", "106M"));
        assert!(has("-bufsize", "170M"));
        assert!(has("-c:v", "libx264"));
        assert!(has("-colorspace", "bt709"));
        assert!(has("-movflags", "+faststart"));
        assert_eq!(args.last().map(String::as_str), Some("a.mp4"));
        assert!(has("-i", "a.mov"));
        let hevc = delivery_args(Codec::Hevc, 55.0, "a.mov", "a.mp4");
        assert!(hevc.windows(2).any(|w| w[0] == "-c:v" && w[1] == "libx265"));
        assert_eq!(
            delivery_args(Codec::H264, 85.0, "a.mov", "a.mp4"),
            args,
            "deterministic"
        );
        // SVT-AV1 refuses a cap outside constant-quality mode (checked against ffmpeg 7.x).
        let av1 = delivery_args(Codec::Av1, 40.0, "a.mov", "a.mp4");
        assert!(av1.iter().any(|a| a == "-b:v"));
        assert!(av1.iter().all(|a| a != "-maxrate" && a != "-bufsize"));
    }

    #[test]
    fn the_plan_lists_the_steps_in_order() {
        let listing = plan(&json!({})).expect("list");
        assert_eq!(listing["presets"].as_array().map(Vec::len), Some(PRESETS.len()));
        let planned = plan(&json!({
            "preset": "showcase_4k60", "name": "forest", "codec": "hevc",
            "duration_s": 20, "sequence": "shots/forest.shots.ron"
        }))
        .expect("plan");
        assert_eq!(planned["frames"], 1200);
        assert_eq!(planned["render_pixels"], 3840_u64 * 2160 * 4);
        assert_eq!(planned["delivery"]["codec"], "hevc");
        let mbps = planned["delivery"]["bitrate_mbps"].as_f64().expect("bitrate");
        assert!((mbps - 55.25).abs() < 1e-3, "{mbps}");
        let steps = planned["steps"].as_array().expect("steps");
        assert_eq!(steps[0]["method"], "render.sequence");
        assert_eq!(steps[1]["method"], "render.out_start");
        assert_eq!(steps[1]["params"]["supersample"], 2);
        assert_eq!(steps[2]["run"], "ffmpeg");
    }
}
