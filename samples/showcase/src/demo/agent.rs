// SPDX-License-Identifier: MIT-0
//! The demo's agent methods (`book/agents/showcase-demo.md`), registered on the world's `AgentExtension`:
//! `demo.state`, `demo.fly`, `demo.set_time_of_day`, `demo.set_weather`, `demo.set_quality`, `demo.screenshot`,
//! `demo.benchmark`, `demo.credits` and `demo.show`. Each mutating method builds the same [`DemoCommand`] the
//! panel and the keys build and runs it through [`super::apply`]. A refusal carries the demo's lowercase code
//! ([`DemoErrorCode`]) in `data.code`.

use std::path::PathBuf;

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_framework::camera::FreeLookInput;
use regia_render::{Camera, RenderSettings};
use regia_scene::Transform;
use serde_json::{Value, json};

use super::fly::{DemoCamera, FlyOverride, MAX_FLY_FRAMES};
use super::fps::FpsMeter;
use super::quality::{parse_tier, picker_tiers, tier_summary};
use super::{DemoCommand, DemoError, DemoErrorCode, DemoLastError, DemoPage, DemoSettings, DemoState, apply};

/// The capability marker `agent.hello` lists when the demo runs.
pub const CAPABILITY: &str = "showcase_demo";

/// Every method the demo registers, in the book page's order.
pub const METHODS: [&str; 9] = [
    "demo.state",
    "demo.fly",
    "demo.set_time_of_day",
    "demo.set_weather",
    "demo.set_quality",
    "demo.screenshot",
    "demo.benchmark",
    "demo.credits",
    "demo.show",
];

fn schemas() -> Vec<(MethodSchema, regia_agent::extension::AgentHandler)> {
    vec![
        (
            MethodSchema {
                name: "demo.state",
                tool: "demo_state",
                description: "The interactive showcase demo: page, camera pose, time of day, weather sliders, quality tier with the detection in words, the measured frame rate, screenshots, the benchmark's progress or last table, credits summary and the last refused control.",
                params: &[],
                result: "{ page, camera: { position, yaw, pitch, fov_y_degrees }?, time_of_day: { hour, day_of_year, latitude_deg }, weather: { preset?, coverage, precipitation, wind_m_per_s }, quality: { tier, source, summary, draws_sky, sky_note?, picker, detection?, render_scale }, fps: { measured, fps?, avg_ms?, worst_ms?, low_1pct_fps?, window_frames?, last_ms?, frames, text }, screenshots: { count, last?, error?, folder }, benchmark: { running, run?, last?, runs, text }, credits: { assets, unlisted, error? }, last_error?: { code, message }, frame }",
                mutating: false,
                permission: Permission::Read,
            },
            demo_state,
        ),
        (
            MethodSchema {
                name: "demo.fly",
                tool: "demo_fly",
                description: "Fly the demo camera for a number of frames as the keys, mouse and sticks would: move (strafe, forward), look (yaw, pitch radians per frame), rise and sprint. Ignored while a benchmark runs.",
                params: &[
                    ParamSchema {
                        name: "move",
                        kind: "array",
                        required: false,
                        description: "[strafe_right, forward], each -1..=1 (clamped to the unit circle)",
                    },
                    ParamSchema {
                        name: "look",
                        kind: "array",
                        required: false,
                        description: "[yaw, pitch] radians per frame, each -0.5..=0.5; positive yaw turns right, positive pitch looks up",
                    },
                    ParamSchema {
                        name: "rise",
                        kind: "number",
                        required: false,
                        description: "-1..=1, positive flies up",
                    },
                    ParamSchema {
                        name: "sprint",
                        kind: "boolean",
                        required: false,
                        description: "fly four times faster",
                    },
                    ParamSchema {
                        name: "frames",
                        kind: "number",
                        required: false,
                        description: "frames to hold the input, 1..=600 (default 1)",
                    },
                ],
                result: "{ frames, move, look, rise, sprint }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_fly,
        ),
        (
            MethodSchema {
                name: "demo.set_time_of_day",
                tool: "demo_set_time_of_day",
                description: "Move the sun: the time-of-day slider over the level's sun clock (LevelTimeOfDay). Omitted fields keep their value.",
                params: &[
                    ParamSchema {
                        name: "hour",
                        kind: "number",
                        required: false,
                        description: "local solar hour, 0..=24",
                    },
                    ParamSchema {
                        name: "day_of_year",
                        kind: "number",
                        required: false,
                        description: "1..=365",
                    },
                    ParamSchema {
                        name: "latitude_deg",
                        kind: "number",
                        required: false,
                        description: "-90..=90",
                    },
                ],
                result: "{ hour, day_of_year, latitude_deg }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_set_time_of_day,
        ),
        (
            MethodSchema {
                name: "demo.set_weather",
                tool: "demo_set_weather",
                description: "Set the weather like the demo's sliders: a preset (render.get_weather lists them), then cloud cover, rain or snow and wind speed on top, at once or over transition_s seconds.",
                params: &[
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "a weather preset name (clear, fair, overcast, ...)",
                    },
                    ParamSchema {
                        name: "coverage",
                        kind: "number",
                        required: false,
                        description: "cloud cover 0..=1",
                    },
                    ParamSchema {
                        name: "precipitation",
                        kind: "number",
                        required: false,
                        description: "rain or snow 0..=1",
                    },
                    ParamSchema {
                        name: "wind_m_per_s",
                        kind: "number",
                        required: false,
                        description: "wind speed 0..=30 m/s (keeps the wind's direction)",
                    },
                    ParamSchema {
                        name: "transition_s",
                        kind: "number",
                        required: false,
                        description: "seconds the change takes, 0..=600 (default 0)",
                    },
                ],
                result: "{ preset?, coverage, precipitation, wind_m_per_s, transition_s }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_set_weather,
        ),
        (
            MethodSchema {
                name: "demo.set_quality",
                tool: "demo_set_quality",
                description: "Pick the quality tier like the demo's picker: a tier name, or auto to detect one from the GPU with the reason in plain words.",
                params: &[ParamSchema {
                    name: "tier",
                    kind: "string",
                    required: true,
                    description: "auto, Low, Medium, High, Ultra, Deck, MobileLow or MobileHigh",
                }],
                result: "{ tier, source, why?, reason? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_set_quality,
        ),
        (
            MethodSchema {
                name: "demo.screenshot",
                tool: "demo_screenshot",
                description: "Take a screenshot like F12: the window's next frame (or an offscreen render in a headless app) as a PNG under the demo's screenshots folder.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "file name relative to the screenshots folder, ending in .png (default: a numbered file)",
                }],
                result: "{ path, written, pending, width?, height? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_screenshot,
        ),
        (
            MethodSchema {
                name: "demo.benchmark",
                tool: "demo_benchmark",
                description: "Start or stop the benchmark flythrough: every view of the scene's perf_views.json at the running tier, then showcase-table.md and .json in the showcase-table format. Progress and results are in demo.state.",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: false,
                        description: "start (default) or cancel",
                    },
                    ParamSchema {
                        name: "warmup",
                        kind: "number",
                        required: false,
                        description: "warm-up frames per view, 0..=3600 (default 30)",
                    },
                    ParamSchema {
                        name: "frames",
                        kind: "number",
                        required: false,
                        description: "measured frames per view, 1..=3600 (default 240)",
                    },
                    ParamSchema {
                        name: "out",
                        kind: "string",
                        required: false,
                        description: "folder relative to the benchmark folder (default: the benchmark folder)",
                    },
                ],
                result: "{ started, views, warmup, frames, tier, out } or { cancelled, markdown, json }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_benchmark,
        ),
        (
            MethodSchema {
                name: "demo.credits",
                tool: "demo_credits",
                description: "The credits page: the scene's asset_manifest.json rows (path, title, author, license, source), the scene files without a row, and the manifest's error if it is missing or malformed.",
                params: &[],
                result: "{ engine, manifest, format_version?, assets: [{ path, title, author, license, source, notes? }], unlisted, error?, lines }",
                mutating: false,
                permission: Permission::Read,
            },
            demo_credits,
        ),
        (
            MethodSchema {
                name: "demo.show",
                tool: "demo_show",
                description: "Show a page of the demo's panel: controls, credits, or hidden (the picture and the frame-rate overlay only).",
                params: &[ParamSchema {
                    name: "page",
                    kind: "string",
                    required: true,
                    description: "controls, credits or hidden",
                }],
                result: "{ page }",
                mutating: true,
                permission: Permission::EditScene,
            },
            demo_show,
        ),
    ]
}

/// Registers the demo's methods and its capability on the world's `AgentExtension`.
pub fn install(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    for (schema, handler) in schemas() {
        extension.register(schema, handler);
    }
    extension.add_capability(
        CAPABILITY,
        "the interactive showcase demo (demo.* methods, book agents/showcase-demo)",
    );
    world.insert_resource(extension);
}

/// A demo refusal as an agent error: the lowercase code in `data.code`.
#[must_use]
pub fn rpc_error(error: DemoError) -> RpcError {
    let (kind, hint) = match error.code {
        DemoErrorCode::BadValue
        | DemoErrorCode::UnknownTier
        | DemoErrorCode::UnknownWeather
        | DemoErrorCode::UnknownPage => (
            INVALID_PARAMS,
            "check the method's parameters in book/agents/showcase-demo.md",
        ),
        DemoErrorCode::NoViews | DemoErrorCode::BenchIdle => (NOT_FOUND, "see demo.state"),
        DemoErrorCode::NoRenderer => (
            UNSUPPORTED,
            "wait for the first frame, or run with a GPU (lavapipe works)",
        ),
        _ => (INVALID_PARAMS, "see demo.state for the demo's state"),
    };
    RpcError::new(kind, error.code.as_str(), error.message, hint)
}

fn bad(message: impl Into<String>) -> RpcError {
    rpc_error(DemoError::new(DemoErrorCode::BadValue, message))
}

fn installed(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<DemoState>() {
        Ok(())
    } else {
        Err(RpcError::not_found(
            "the showcase demo is not running",
            "start the showcase with --demo",
        ))
    }
}

fn number(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| Some(n as f32))
            .ok_or_else(|| bad(format!("`{key}` must be a number"))),
    }
}

fn whole(params: &Value, key: &str) -> Result<Option<u32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| bad(format!("`{key}` must be a whole number"))),
    }
}

fn text<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| bad(format!("`{key}` must be a string"))),
    }
}

fn pair(params: &Value, key: &str, limit: f32) -> Result<Option<Vec2>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let values: Option<Vec<f32>> = v
                .as_array()
                .filter(|a| a.len() == 2)
                .map(|a| a.iter().filter_map(Value::as_f64).map(|n| n as f32).collect());
            match values {
                Some(xy) if xy.len() == 2 && xy.iter().all(|n| n.is_finite() && n.abs() <= limit) => {
                    Ok(Some(Vec2::new(xy[0], xy[1])))
                }
                _ => Err(bad(format!(
                    "`{key}` must be [x, y] with each within -{limit}..={limit}"
                ))),
            }
        }
    }
}

fn run(world: &mut World, command: DemoCommand) -> Result<Value, RpcError> {
    installed(world)?;
    apply(world, command).map_err(rpc_error)
}

/// `demo.state`.
fn demo_state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let camera = {
        let mut q = world.query_filtered::<(&Transform, &Camera), With<DemoCamera>>();
        q.iter(world).next().map(|(t, c)| {
            let (yaw, pitch) = super::fly::yaw_pitch(t.rotation);
            json!({
                "position": t.translation.to_array(),
                "yaw": yaw,
                "pitch": pitch,
                "fov_y_degrees": c.fov_y.to_degrees(),
            })
        })
    };
    let render_scale = world
        .get_resource::<RenderSettings>()
        .map(|s| s.profile.render_scale);
    let fps = world.resource::<FpsMeter>().to_json();
    let last_error = world
        .resource::<DemoLastError>()
        .0
        .as_ref()
        .map(|e| json!({ "code": e.code.as_str(), "message": e.message }));
    let folder = world.resource::<DemoSettings>().0.out_dir.join("screenshots");
    let state = world.resource::<DemoState>();
    Ok(json!({
        "page": state.page.name(),
        "camera": camera,
        "time_of_day": {
            "hour": state.time_of_day.hour,
            "day_of_year": state.time_of_day.day_of_year,
            "latitude_deg": state.time_of_day.latitude_deg,
        },
        "weather": {
            "preset": state.weather.preset,
            "coverage": state.weather.coverage,
            "precipitation": state.weather.precipitation,
            "wind_m_per_s": state.weather.wind_m_per_s,
        },
        "quality": {
            "tier": state.tier.name(),
            "source": state.tier_source.name(),
            "summary": tier_summary(state.tier),
            "draws_sky": super::sun::draws_sky(state.tier),
            "sky_note": super::sun::sky_note(state.tier),
            "picker": picker_tiers().iter().map(|t| t.name()).collect::<Vec<_>>(),
            "detection": state.detection.as_ref().map(super::quality::TierWords::to_json),
            "render_scale": render_scale,
        },
        "fps": fps,
        "screenshots": {
            "count": state.screenshots,
            "last": state.last_screenshot.as_ref().map(|p| p.display().to_string()),
            "error": state.screenshot_error,
            "folder": folder.display().to_string(),
        },
        "benchmark": state.bench.to_json(),
        "credits": {
            "assets": state.credits.manifest.as_ref().map_or(0, |m| m.assets.len()),
            "unlisted": state.credits.unlisted.len(),
            "error": state.credits.error,
        },
        "last_error": last_error,
        "frame": state.frame,
    }))
}

/// `demo.fly { move?, look?, rise?, sprint?, frames? }`.
fn demo_fly(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let move_axes = pair(params, "move", 1.0)?.unwrap_or(Vec2::ZERO);
    let look = pair(params, "look", 0.5)?.unwrap_or(Vec2::ZERO);
    let rise = number(params, "rise")?.unwrap_or(0.0);
    if !rise.is_finite() || rise.abs() > 1.0 {
        return Err(bad("`rise` must be within -1..=1"));
    }
    let sprint = match params.get("sprint") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("`sprint` must be a boolean")),
    };
    let frames = whole(params, "frames")?.unwrap_or(1);
    if !(1..=MAX_FLY_FRAMES).contains(&frames) {
        return Err(bad(format!("`frames` must be within 1..={MAX_FLY_FRAMES}")));
    }
    let input = FreeLookInput {
        look,
        move_axes: if move_axes.length_squared() > 1.0 {
            move_axes.normalize_or_zero()
        } else {
            move_axes
        },
        rise,
        sprint,
    };
    world.insert_resource(FlyOverride { input, frames });
    Ok(json!({
        "frames": frames,
        "move": input.move_axes.to_array(),
        "look": input.look.to_array(),
        "rise": rise,
        "sprint": sprint,
    }))
}

/// `demo.set_time_of_day { hour?, day_of_year?, latitude_deg? }`.
fn demo_set_time_of_day(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let day_of_year = match whole(params, "day_of_year")? {
        None => None,
        Some(day) => Some(u16::try_from(day).map_err(|_| bad("`day_of_year` must be within 1..=365"))?),
    };
    run(
        world,
        DemoCommand::SetTimeOfDay {
            hour: number(params, "hour")?,
            day_of_year,
            latitude_deg: number(params, "latitude_deg")?,
        },
    )
}

/// `demo.set_weather { preset?, coverage?, precipitation?, wind_m_per_s?, transition_s? }`.
fn demo_set_weather(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    run(
        world,
        DemoCommand::SetWeather {
            preset: text(params, "preset")?.map(str::to_owned),
            coverage: number(params, "coverage")?,
            precipitation: number(params, "precipitation")?,
            wind_m_per_s: number(params, "wind_m_per_s")?,
            transition_s: number(params, "transition_s")?.unwrap_or(0.0),
        },
    )
}

/// `demo.set_quality { tier }`.
fn demo_set_quality(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = text(params, "tier")?.ok_or_else(|| bad("`tier` is required (auto or a tier name)"))?;
    let command = if name.eq_ignore_ascii_case("auto") {
        DemoCommand::DetectTier
    } else {
        DemoCommand::SetTier(parse_tier(name).map_err(rpc_error)?)
    };
    run(world, command)
}

/// `demo.screenshot { path? }`.
fn demo_screenshot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = text(params, "path")?.map(PathBuf::from);
    run(world, DemoCommand::Screenshot(path))
}

/// `demo.benchmark { action?, warmup?, frames?, out? }`.
fn demo_benchmark(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match text(params, "action")?.unwrap_or("start") {
        "start" => {
            let command = DemoCommand::StartBenchmark {
                warmup: whole(params, "warmup")?,
                frames: whole(params, "frames")?,
                out: text(params, "out")?.map(PathBuf::from),
            };
            run(world, command)
        }
        "cancel" => run(world, DemoCommand::CancelBenchmark),
        other => Err(bad(format!("`action` must be start or cancel, got {other}"))),
    }
}

/// `demo.credits`.
fn demo_credits(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let credits = &world.resource::<DemoState>().credits;
    let mut out = credits.to_json();
    out["lines"] = json!(credits.lines());
    Ok(out)
}

/// `demo.show { page }`.
fn demo_show(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = text(params, "page")?.ok_or_else(|| bad("`page` is required: controls, credits or hidden"))?;
    let page = DemoPage::from_name(name).ok_or_else(|| {
        rpc_error(DemoError::new(
            DemoErrorCode::UnknownPage,
            format!("unknown page `{name}` (controls, credits or hidden)"),
        ))
    })?;
    run(world, DemoCommand::ShowPage(page))
}

/// Calls a demo method on `world` directly (tests and the panel's own checks; agents go through the server).
///
/// # Errors
/// What the method answers; `F0002` for a name that is not one of [`METHODS`].
pub fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    let handler = schemas()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .map(|(_, handler)| handler)
        .ok_or_else(|| RpcError::invalid_params(format!("{method} is not a demo method")))?;
    handler(world, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_registered_with_the_capability() {
        let mut world = World::new();
        install(&mut world);
        let extension = world.resource::<AgentExtension>();
        let mut names = extension.method_names();
        names.retain(|n| n.starts_with("demo."));
        assert_eq!(names, METHODS.to_vec());
        assert!(extension.capabilities().iter().any(|(n, _)| *n == CAPABILITY));
        for (schema, _) in schemas() {
            assert!(schema.tool == schema.name.replace('.', "_"), "{}", schema.name);
        }
    }

    #[test]
    fn methods_refuse_before_the_demo_runs() {
        let mut world = World::new();
        for method in METHODS {
            let answer = call(&mut world, method, &json!({ "tier": "High", "page": "credits" }));
            let Err(error) = answer else {
                panic!("{method} must be refused without the demo");
            };
            assert_eq!(error.code, NOT_FOUND, "{method}");
        }
        assert!(call(&mut world, "demo.nope", &json!({})).is_err());
    }

    #[test]
    fn refusals_carry_the_lowercase_code() {
        for code in DemoErrorCode::ALL {
            let error = rpc_error(DemoError::new(code, "x"));
            assert_eq!(
                error.data.as_ref().and_then(|d| d["code"].as_str()),
                Some(code.as_str())
            );
        }
    }
}
