// SPDX-License-Identifier: MIT-0
//! The `viewer_app.*` methods on the agent protocol's existing transport (`book/agents/product-viewer.md`).
//! A dev build only: `regia_agent` links the whole engine, so the app itself never has this.

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, Permission};
use regia_app::prelude::*;
use regia_platform::{SafeArea, Window, WindowHandles};
use serde_json::{Value, json};

use crate::app::{PanelUi, ViewerCore, ViewerState, primary_window_scale};
use crate::viewer_api::{self, ApiError};

const fn method(
    name: &'static str,
    description: &'static str,
    result: &'static str,
    mutating: bool,
    permission: Permission,
) -> MethodSchema {
    MethodSchema {
        name,
        tool: "",
        description,
        params: &[],
        result,
        mutating,
        permission,
    }
}

fn refused(err: ApiError) -> RpcError {
    RpcError {
        code: INVALID_PARAMS,
        message: err.message,
        data: Some(
            json!({ "code": "F0002", "reason": err.code, "hint": "see book/agents/product-viewer.md" }),
        ),
    }
}

/// Runs `call` with the host's state and the player. The frame system holds the player only while it runs, and
/// a request is handled in another system, so the player is in the world here.
fn with_core<T>(
    world: &mut World,
    call: impl FnOnce(&mut ViewerState, &mut ViewerCore) -> T,
) -> Result<T, RpcError> {
    let Some(mut core) = world.remove_non_send::<ViewerCore>() else {
        return Err(refused(ApiError {
            code: "no_product",
            message: "the viewer is not running".to_owned(),
        }));
    };
    let result = world.resource_scope(|_, mut state: Mut<ViewerState>| call(&mut state, &mut core));
    world.insert_non_send(core);
    Ok(result)
}

fn options(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    with_core(world, |state, core| viewer_api::options(state, core))
}

fn set_pick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_core(world, |state, core| viewer_api::set_pick(state, core, params))?.map_err(refused)
}

fn build(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    with_core(world, |_, core| viewer_api::build(core))?.map_err(refused)
}

fn view(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    with_core(world, |_, core| viewer_api::view(core))?.map_err(refused)
}

fn set_view(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_core(world, |_, core| viewer_api::set_view(core, params))?.map_err(refused)
}

fn reset_view(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    with_core(world, |_, core| viewer_api::reset_view(core))?.map_err(refused)
}

fn touch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scale = primary_window_scale(world);
    let panel = world.resource::<PanelUi>().hit_rect_px(scale);
    let mut state = world.resource_mut::<ViewerState>();
    viewer_api::touch(&mut state, panel, scale, params).map_err(refused)
}

/// Runs `call` with the panel, the state, the player and the window's values.
fn with_frame<T>(
    world: &mut World,
    call: impl FnOnce(&mut ViewerState, &mut PanelUi, &mut ViewerCore, (u32, u32), f32, &SafeArea) -> T,
) -> Result<T, RpcError> {
    let (size, scale) = world
        .resource::<WindowHandles>()
        .iter()
        .next()
        .and_then(|(entity, _)| world.get::<Window>(entity))
        .map_or(((1, 1), 1.0), |window| {
            (window.physical_size, window.scale_factor.max(0.5) as f32)
        });
    let safe = *world.resource::<SafeArea>();
    let Some(mut core) = world.remove_non_send::<ViewerCore>() else {
        return Err(refused(ApiError {
            code: "no_product",
            message: "the viewer is not running".to_owned(),
        }));
    };
    let result = world.resource_scope(|world, mut state: Mut<ViewerState>| {
        world.resource_scope(|_, mut panel: Mut<PanelUi>| {
            call(&mut state, &mut panel, &mut core, size, scale, &safe)
        })
    });
    world.insert_non_send(core);
    Ok(result)
}

fn set_deterministic(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_frame(world, |_, panel, core, _, _, _| {
        viewer_api::set_deterministic(panel, core, params)
    })?
    .map_err(refused)
}

fn step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_frame(world, |state, panel, core, size, scale, safe| {
        viewer_api::step(state, panel, core, size, scale, safe, params)
    })?
    .map_err(refused)
}

fn stats(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    with_core(world, |state, core| viewer_api::stats(state, core))
}

fn stats_clear(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    Ok(viewer_api::stats_clear(&mut world.resource_mut::<ViewerState>()))
}

fn packages(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    Ok(viewer_api::packages(world.resource::<ViewerState>()))
}

fn open_package(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    viewer_api::open_package(&mut world.resource_mut::<ViewerState>(), params).map_err(refused)
}

fn fetch_package(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    viewer_api::fetch_package(&mut world.resource_mut::<ViewerState>(), params).map_err(refused)
}

/// Registers the methods on the world's [`AgentExtension`] and adds the agent server.
pub fn install(app: &mut App) {
    let world = app.world_mut();
    if world.get_resource::<AgentExtension>().is_none() {
        world.insert_resource(AgentExtension::new());
    }
    let mut extension = world.resource_mut::<AgentExtension>();
    let read = Permission::Read;
    let play = Permission::Play;
    let assets = Permission::Assets;
    let rows: [(MethodSchema, regia_agent::extension::AgentHandler); 14] = [
        (
            method(
                "viewer_app.options",
                "The open product, its option groups and the status line.",
                "{product, open, groups, status, status_is_error, showing_products}",
                false,
                read,
            ),
            options,
        ),
        (
            method(
                "viewer_app.set_pick",
                "Applies a sold pick; a refusal changes nothing. Params: group, value.",
                "{ok, build}",
                true,
                play,
            ),
            set_pick,
        ),
        (
            method(
                "viewer_app.build",
                "The current jewelry.build/1, with no client data.",
                "the build",
                false,
                read,
            ),
            build,
        ),
        (
            method(
                "viewer_app.view",
                "The view shown now and whether it still moves.",
                "{view, moving}",
                false,
                read,
            ),
            view,
        ),
        (
            method(
                "viewer_app.set_view",
                "Sets the view's target, clamped to its limits. Params: tilt, spin, pan, zoom, animate.",
                "{view, clamped}",
                true,
                play,
            ),
            set_view,
        ),
        (
            method(
                "viewer_app.reset_view",
                "Animates back to the default view.",
                "{ok}",
                true,
                play,
            ),
            reset_view,
        ),
        (
            method(
                "viewer_app.set_deterministic",
                "Scripted capture: while on, the panel and the picture move only in viewer_app.step, on its clock. Params: on.",
                "{ok, deterministic}",
                true,
                play,
            ),
            set_deterministic,
        ),
        (
            method(
                "viewer_app.step",
                "One scripted frame. Params: dt_ms, force (draw even when nothing changed), capture (hash or png).",
                "{drew, frame, clock_ms, width?, height?, hash?, png_base64?}",
                true,
                play,
            ),
            step,
        ),
        (
            method(
                "viewer_app.touch",
                "Feeds finger events through the app's routing. Params: events [{id, phase, x, y, at_ms}].",
                "{ok}",
                true,
                play,
            ),
            touch,
        ),
        (
            method(
                "viewer_app.stats",
                "Draws, wake-ups, frame and switch times, the surface's history.",
                "{wakes, wakes_per_second, idle, frame_interval_ms, switch_ms, player, ...}",
                false,
                read,
            ),
            stats,
        ),
        (
            method(
                "viewer_app.stats_clear",
                "Forgets the timing samples, so a measurement starts clean.",
                "{ok}",
                true,
                play,
            ),
            stats_clear,
        ),
        (
            method(
                "viewer_app.packages",
                "The packages on the device and the folders they are read from.",
                "{packages, roots, busy}",
                false,
                read,
            ),
            packages,
        ),
        (
            method(
                "viewer_app.open_package",
                "Reads and checks a package in the background. Params: slug.",
                "{ok, started}",
                true,
                assets,
            ),
            open_package,
        ),
        (
            method(
                "viewer_app.fetch_package",
                "Downloads and keeps a package in the background. Params: url.",
                "{ok, started}",
                true,
                assets,
            ),
            fetch_package,
        ),
    ];
    for (schema, handler) in rows {
        extension.register(schema, handler);
    }
    app.add_plugins(regia_agent::AgentPlugin);
}
