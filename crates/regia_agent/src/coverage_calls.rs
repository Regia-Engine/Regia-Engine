// SPDX-License-Identifier: MPL-2.0
//! Calls every schema method through the dispatcher.
//!
//! `cargo xtask agent-coverage` treats a method as tested only when test code contains its name
//! as a string literal and something actually invokes it. [`CALLED`] is that list: one literal
//! per method. [`every_dispatched_method_is_called`] sends each name through [`process_requests`]
//! and rejects JSON-RPC method-not-found (`F0008`). Handlers may still answer with a structured
//! error when the minimal app has no renderer, audio, or project — that is a real dispatch, not
//! a missing method.
//!
//! A loop over [`crate::schema::METHODS`] would not satisfy the check: the names would live in
//! the schema table, which is not test code.

#![cfg(test)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use regia_2d::{Sprite, TileMap};
use regia_animation::Animator;
use regia_app::prelude::*;
use regia_framework::camera::CameraRig;
use regia_platform::PrimaryWindow;
use regia_vfx::HoverWash;
use serde_json::{Value, json};

use crate::dispatch::{self, AgentInbox, RateLimit};
use crate::playtest::AgentTimeControl;
use crate::protocol::{self, Request, Response};
use crate::safety::{AgentConfig, AgentPermissions};
use crate::server::Inbound;

/// Every method, in schema order. Each entry is its own string literal on purpose.
const CALLED: &[&str] = &[
    "agent.hello",
    "agent.guide",
    "agent.schema",
    "agent.changes",
    "project.info",
    "types.list",
    "types.describe",
    "resources.list",
    "errors.explain",
    "scene.query_entities",
    "scene.get_entity",
    "scene.set_property",
    "scene.spawn",
    "scene.spawn_primitive",
    "scene.despawn",
    "scene.duplicate",
    "scene.reparent",
    "scene.add_component",
    "scene.remove_component",
    "scene.apply_edits",
    "scene.serialize",
    "scene.spawn_prefab",
    "assets.list",
    "assets.describe",
    "assets.load",
    "assets.reload",
    "assets.skin_check",
    "assets.status",
    "assets.watch_status",
    "seeing.screenshot",
    "seeing.capture_sequence",
    "seeing.contact_sheet",
    "seeing.compare_images",
    "seeing.reference_trace",
    "seeing.compare",
    "seeing.pick_entity",
    "seeing.memory_report",
    "seeing.list_resources",
    "seeing.dump_resource",
    "assets.texture_memory",
    "registry.query",
    "registry.describe",
    "registry.dependencies",
    "registry.referencers",
    "registry.tags",
    "assets.load_progress",
    "assets.handles",
    "render.boot_timeline",
    "render.pipeline_warm",
    "render.scene_ready",
    "render.memory_budget",
    "render.memory_pressure",
    "play.pause",
    "play.resume",
    "play.step",
    "play.set_time_scale",
    "play.inject_input",
    "input.spacemouse_status",
    "input.spacemouse_inject",
    "input.spacemouse_nav_defaults",
    "input.rebind_state",
    "input.rebind",
    "input.rebind_cancel",
    "input.rebind_reset",
    "input.rebind_axis",
    "input.rebind_save",
    "input.rebind_load",
    "input.rebind_profiles",
    "play.wait_until",
    "play.expect",
    "play.get_diagnostics",
    "play.hitch_report",
    "logs.read",
    "events.list",
    "events.read",
    "diagnostics.crash_reports",
    "fuzz.targets",
    "fuzz.run",
    "feedback.capture",
    "feedback.list",
    "telemetry.get_last_summary",
    "telemetry.live",
    "ui.nav_graph",
    "ui.nav_move",
    "ui.nav_lint",
    "scripts.list",
    "scripts.get",
    "camera.get",
    "camera.set_mode",
    "camera.blend_to",
    "settings.get",
    "settings.set",
    "level.open",
    "level.unload",
    "level.status",
    "rules.get",
    "timers.list",
    "tweens.list",
    "perf_overlay.get",
    "perf_overlay.toggle",
    "render.get_profile",
    "render.set_profile",
    "render.mesh_lod",
    "render.get_sssr_cutoff",
    "render.set_sssr_cutoff",
    "render.get_exposure_settings",
    "render.set_exposure_settings",
    "quality.get_detected",
    "quality.detect",
    "quality.get_terrain_vt_policy",
    "quality.set_terrain_vt_policy",
    "render.get_calibration",
    "render.set_calibration",
    "render.get_clouds",
    "render.set_clouds",
    "render.get_valley_mist",
    "render.set_valley_mist",
    "render.get_weather",
    "render.set_weather",
    "render.scene_wind",
    "render.set_scene_wind",
    "render.precipitation",
    "render.set_precipitation",
    "render.surface_weather",
    "render.set_surface_weather",
    "render.surface_exposure",
    "render.camera_cut",
    "ui.layers",
    "ui.shortcuts",
    "ui.shortcut_trigger",
    "ui.shortcut_contexts",
    "ui.accessibility",
    "ui.tween",
    "ui.fuzzy",
    "ui.set_anchor_preset",
    "ui.set_disabled",
    "ui.touch",
    "ui.keyboard",
    "ui.text_style",
    "ui.material",
    "ui.data_grid",
    "ui.lobby_list",
    "ui.scoreboard",
    "ui.radar",
    "ui.world_markers",
    "ui.target_bracket",
    "ui.touch_live",
    "ui.search_box",
    "ui.progress_dialog",
    "ui.soft_cursor",
    "ui.color_picker",
    "ui.tree",
    "ui.sounds",
    "ui.play_anim",
    "ui.debug_overlay",
    "ui.list",
    "ui.scroll_to",
    "ui.text_shape",
    "ui.set_default_font",
    "ui.text_breaks",
    "ui.inspect",
    "ui.pick_at",
    "ui.highlight",
    "ui.a11y_tree",
    "ui.a11y_announce",
    "ui.a11y_act",
    "ui.a11y_platform_tree",
    "ui.a11y_platform_connect",
    "ui.a11y_platform_action",
    "ui.a11y_audit",
    "ui.theme_mode",
    "ui.icons",
    "ui.glyph_atlas",
    "ui.motion",
    "render.out_start",
    "render.out_progress",
    "render.out_cancel",
    "render.out_job",
    "render.sequence",
    "camera.set_path",
    "camera.raycast",
    "ui.query",
    "ui.state",
    "ui.click",
    "ui.focus",
    "ui.type",
    "ui.set_value",
    "ui.set_locale",
    "ui.resolve",
    "ui.set_backdrop_blur",
    "ui.backdrop_stats",
    "ui.drag_drop",
    "ui.open_popup",
    "ui.close_popup",
    "ui.menu",
    "ui.menu_activate",
    "ui.theme_list",
    "ui.theme_get",
    "ui.theme_set",
    "ui.theme_select",
    "ui.theme_preview",
    "ui.theme_lint",
    "ui.theme_reload",
    "level.get_look",
    "level.set_look",
    "render.problems",
    "sim.counters",
    "sim.check_framerate_independence",
    "fog.status",
    "fog.set_viewer",
    "sim.host_status",
    "sim.host_hold",
    "sim.host_step",
    "sim.tick_profile",
    "sim.host_snapshot",
    "sim.host_restore",
    "sim.bridge_settings",
    "sim.set_bridge_settings",
    "fog.texture_stats",
    "fog.set_look",
    "sim.det_debug_draw",
    "sim.grid_flow",
    "anim.list",
    "anim.get_state",
    "anim.set_param",
    "anim.spring_list",
    "anim.spring_set",
    "anim.spring_reset",
    "anim.rig_attach",
    "anim.rig_state",
    "anim.profile_check",
    "anim.gait_validate",
    "anim.morph_weights",
    "anim.set_morph_weights",
    "anim.gaze_get",
    "anim.gaze_set",
    "anim.bind_pose_get",
    "anim.bind_pose_set",
    "anim.clip_report",
    "anim.motion_match",
    "anim.motion_match_set",
    "anim.motion_match_database",
    "anim.motion_match_databases",
    "anim.mm_predict",
    "physics.raycast",
    "physics.overlap",
    "physics.closest_point",
    "physics.stats",
    "physics.convex_decompose",
    "physics.vehicles",
    "physics.vehicle_input",
    "physics.vehicle_reset",
    "physics.vehicle_telemetry",
    "physics.vehicle_shift",
    "physics.hovercraft",
    "physics.vehicle_config_check",
    "physics.vehicle_tire_curve",
    "physics.debug_draw",
    "physics.material_check",
    "physics.material_register",
    "physics.teleport",
    "physics.parented_bodies",
    "nav.get_mesh",
    "nav.find_nearest",
    "nav.find_path",
    "nav.plan",
    "nav.add_obstacle",
    "nav.remove_obstacle",
    "functest.list",
    "functest.run",
    "functest.status",
    "functest.results",
    "functest.abort",
    "functest.create",
    "functest.bless",
    "smartobject.list",
    "smartobject.claim",
    "smartobject.release",
    "smartobject.search",
    "smartobject.begin_use",
    "smartobject.set_enabled",
    "smartobject.tags",
    "smartobject.messages",
    "smartobject.set_definition",
    "statetree.compile",
    "statetree.control",
    "statetree.reload",
    "statetree.debug",
    "logic.compile",
    "logic.control",
    "logic.emit",
    "logic.reload",
    "logic.status",
    "logic.debug",
    "logic.world",
    "logic.world_set",
    "ai.query_list",
    "ai.query_run",
    "ai.query_submit",
    "ai.query_poll",
    "ai.query_abort",
    "ai.query_stats",
    "ai.query_probe",
    "ai.bt_get",
    "ai.bt_validate",
    "ai.bt_edit",
    "ai.bt_live",
    "ai.perception_get",
    "ai.attitude_set",
    "ai.utility_list",
    "ai.utility_explain",
    "ai.goap_plan",
    "ai.squads",
    "ai.squad_set",
    "ai.tick_budget_get",
    "ai.tick_budget_set",
    "ai.query_impls",
    "nav.region_status",
    "nav.region_rebuild",
    "nav.stream_status",
    "nav.stream_begin",
    "nav.stream_cook_cell",
    "nav.stream_load_cell",
    "nav.stream_unload_cell",
    "nav.stream_find_path",
    "nav.det_agents",
    "nav.area_status",
    "nav.set_area_cost",
    "nav.add_area_volume",
    "nav.remove_area_volume",
    "nav.area_table",
    "nav.set_area_table",
    "nav.crowd_set_filter",
    "nav.lanes_set",
    "nav.lanes_status",
    "nav.lane_route",
    "nav.lane_nearest",
    "nav.lane_check",
    "nav.lane_set_enabled",
    "nav.lane_add_agent",
    "nav.lane_remove_agent",
    "nav.lane_set_destination",
    "nav.lane_step",
    "nav.lane_agents",
    "nav.tile_cache_status",
    "nav.tile_cache_add_obstacle",
    "nav.tile_cache_remove_obstacle",
    "nav.crowd_status",
    "nav.crowd_add_agent",
    "nav.crowd_set_target",
    "nav.crowd_step",
    "nav.links",
    "nav.set_links",
    "nav.set_link_enabled",
    "nav.generate_links",
    "nav.tile_cache_move_obstacle",
    "nav.crowd_set_link_queue",
    "nav.crowd_link_queues",
    "nav.lanes_from_splines",
    "nav.cook_triangle_areas",
    "nav.set_link_files",
    "nav.authored_links_status",
    "nav.link_traversals",
    "nav.crowd_set_link_arcs",
    "nav.tile_links",
    "nav.tile_cache_grow",
    "sprite.list",
    "sprite.get",
    "tilemap.list",
    "tilemap.get_tile",
    "view2d.get",
    "tilemap.set_tile",
    "tilemap.autotile_register",
    "tilemap.autotile_list",
    "tilemap.paint",
    "tilemap.autotile",
    "tilemap.stamp_copy",
    "tilemap.stamp_paste",
    "import.report",
    "debug.gizmos",
    "quick.draw_list",
    "quick.draw2d_stats",
    "debug.set_gizmos",
    "material.graph_get",
    "material.graph_set",
    "material.compile",
    "material.shading",
    "material.set_shading",
    "material.vertex_ao",
    "material.set_vertex_ao",
    "material.thin_film",
    "material.set_thin_film",
    "material.subsurface",
    "material.set_subsurface",
    "render.subsurface",
    "render.set_subsurface",
    "material.rough_transmission",
    "material.set_rough_transmission",
    "render.oit",
    "render.set_oit",
    "terrain.scatter",
    "terrain.set_scatter",
    "render.set_contact_shadows",
    "render.set_translucent_shadows",
    "render.set_reflection_occlusion",
    "render.set_bent_normals",
    "render.set_near_sun_shadow",
    "render.set_studio_environment",
    "render.get_studio_environment",
    "render.set_environment_ground",
    "render.set_studio_background",
    "render.get_debug_view",
    "render.set_debug_view",
    "render.lights",
    "render.place_local_capture",
    "render.bake_local_capture",
    "render.list_local_captures",
    "render.remove_local_capture",
    "render.get_forward_lite",
    "render.set_forward_lite",
    "render.get_output_dither",
    "render.set_output_dither",
    "render.get_star_flux",
    "render.set_star_flux",
    "render.get_sun_shadow_range",
    "render.set_sun_shadow_range",
    "render.get_gi_mode",
    "level.get_baked_gi",
    "level.export_bake",
    "level.get_baked_gi_stream",
    "level.set_baked_gi_stream",
    "level.sample_baked_gi",
    "render.set_gi_mode",
    "seeing.capture_plate",
    "render.set_reference_overlay",
    "render.set_gi_occluders",
    "render.sample_baked_gi",
    "render.get_gi_probes",
    "render.explain_gi",
    "render.set_gi_debug_overlay",
    "render.set_baked_gi_ao",
    "render.set_gi_sampling",
    "render.get_baked_gi_visibility",
    "render.set_baked_gi_visibility",
    "render.mesh_occluders",
    "render.emissive_lights",
    "world.pick",
    "terrain.stroke",
    "terrain.holes",
    "terrain.set_holes",
    "terrain.load_holes",
    "terrain.raycast",
    "terrain.vt",
    "terrain.set_vt",
    "terrain.water",
    "terrain.set_water",
    "terrain.grass",
    "terrain.set_grass",
    "players.list",
    "players.get",
    "scene.possess",
    "save.save",
    "save.load",
    "save.list_slots",
    "save.quick_save",
    "save.quick_load",
    "save.delete",
    "save.status",
    "character.move_to",
    "save.inspect",
    "audio.list_emitters",
    "audio.set_emitter",
    "audio.list_zones",
    "audio.set_zone",
    "audio.probe_zones",
    "loop.get",
    "loop.set",
    "quit.request",
    "coroutines.list",
    "replay.load",
    "replay.seek",
    "replay.step",
    "replay.play",
    "replay.pause",
    "moments.list",
    "moments.get",
    "state.list",
    "state.get",
    "state.set",
    "vfx.spawn_decal",
    "vfx.place_rut",
    "vfx.decal_budget",
    "vfx.get_look",
    "vfx.set_look",
    "vfx.medium",
    "vfx.hover_washes",
    "vfx.set_hover_wash",
    "vfx.hit",
    "vfx.weapons",
    "vfx.plumes",
    "vfx.set_plume",
    "vfx.stats",
    "vfx.systems",
    "vfx.describe",
    "vfx.spawn",
    "vfx.set_param",
    "vfx.spawn_probe",
    "vfx.fire_lights",
    "vfx.spawn_crash_fires",
    "gameplay.explode",
    "gameplay.explosion_stats",
    "camera.shock_shake",
    "camera.schedule_shock_shake",
    "camera.shake",
    "camera.shake_sensor",
    "camera.set_shake_sensor",
    "camera.rail_state",
    "camera.interior_state",
    "camera.crane_state",
    "camera.set_rail",
    "camera.set_crane",
    "camera.walk_bob",
    "camera.set_walk_bob",
    "camera.handheld",
    "camera.photographic_walk",
    "scene.season",
    "render.foliage_shadow",
    "render.capture_plan",
    "camera.assets",
    "camera.validate",
    "camera.inspect",
    "camera.set_var",
    "camera.set_param",
    "camera.set_input",
    "camera.reset_input",
    "camera.push_overlay",
    "camera.add_additive",
    "camera.release_layer",
    "camera.hold_overlay",
    "camera.set_driver",
    "camera.load",
    "vfx.debris_burst",
    "vfx.debris_state",
    "vfx.debris_clock",
    "vfx.impact_site",
    "vfx.entry_state",
    "render.glass_view",
    "render.set_glass_view",
    "render.canopy_glass",
    "render.set_canopy_glass",
    "render.glass_damage",
    "render.mask_decals",
    "render.set_mask_decals",
    "render.hair_get",
    "render.hair_set",
    "render.skinning",
    "render.set_skinning",
    "render.draw_integrity",
    "render.set_skinned_bounds",
    "render.texture_sets",
    "render.set_texture_set_census",
    "audio.medium",
    "audio.propagation",
    "audio.loopgen_render",
    "ui.scroll",
    "render.get_visbuffer",
    "render.temporal_quality",
    "render.get_exposure",
    "render.exposure_log",
    "render.get_upscalers",
    "render.set_upscaler",
    "render.reference_upscale",
    "render.upscaler_jitter",
    "render.frame_gen",
    "render.set_frame_gen",
    "render.shadow_stability",
    "render.material_clock",
    "seeing.gi_denoise_check",
    "render.get_probe_specular",
    "render.set_probe_specular",
    "render.probe_specular_lobe",
    "render.hwrt",
    "render.blue_noise",
    "render.set_blue_noise",
    "render.set_sun_shadow_trace",
    "render.frame_pipelining",
    "render.set_frame_pipelining",
    "render.set_visbuffer",
    "render.get_lightmaps",
    "render.set_lightmaps",
    "render.lightmap_encode_report",
    "render.get_frame_stats",
    "render.get_capabilities",
    "render.bindless_report",
    "render.texture_residency",
    "ui.key",
    "ui.text_state",
    "ui.text_select",
    "lifecycle.get",
    "lifecycle.drill",
    "thermal.get",
    "thermal.set_override",
    "input.touch_controls",
    "pacing.get",
    "pacing.set",
    "pacing.simulate",
    "pacing.trace",
    "executor.get",
    "executor.set",
    "xr.get",
    "xr.drive",
    "xr.set_option",
    "xr.recenter",
    "xr.capture_eye",
    "xr.layers",
    "xr.set_quad",
    "xr.walk_start",
    "xr.walk_set",
    "render.swapchain",
    "platform.paths",
    "sim.state_hash",
    "sim.state_diff",
    "tags.list",
    "tags.get",
    "tags.set",
    "tags.query",
    "tags.validate",
    "memory.report",
    "memory.mark",
    "audio.mix",
    "audio.bus_meters",
    "audio.program_loudness",
    "audio.set_bus",
    "audio.set_limiter",
    "audio.render_check",
    "audio.voice",
    "audio.capture_start",
    "audio.capture_stop",
    "audio.set_talking",
    "audio.voice_loopback_start",
    "audio.voice_loopback_stop",
    "audio.voice_place",
    "voice_chat.status",
    "water.floaters",
    "water.ocean",
    "water.ocean_check",
    "water.set_mode",
    "water.ocean_cost",
    "water.set_caustics",
    "water.caustics_stats",
    "voice_chat.set_channel",
    "voice_chat.mute",
    "voice_chat.loopback",
    "voice_chat.stats",
    "voice_chat.test_source",
    "voice_chat.simulate_network",
    "voice_chat.mic",
    "voice_chat.settings",
    "voice_chat.talk",
    "voice_chat.relay",
    "voice_chat.processing",
    "terrain.parity",
    "render.fog_lights",
    "render.stats",
    "render.set_skinned_instancing",
    "water.state",
    "water.probe",
    "water.set",
    "water.surface",
    "water.raycast",
    "vfx.crash",
    "vfx.crashes",
    "vfx.crash_profile",
    "vfx.crash_fireball",
    "vfx.blast_gust",
    "terrain.get_ground_look",
    "terrain.set_ground_look",
    "render.get_aerosol",
    "render.set_aerosol",
    "render.grey_card",
    "terrain.get_debug_view",
    "terrain.set_debug_view",
    "render.post_volumes",
    "render.set_post_volume",
    "render.get_physical_camera",
    "render.set_physical_camera",
    "render.set_post_volume_luts",
    "render.post_volume_gizmos",
    "render.lens_effects",
    "render.set_lens_effects",
    "anim.montage_state",
    "anim.montage_play",
    "anim.montage_stop",
    "anim.montage_jump",
    "anim.montage_set_warp_target",
    "anim.sync_state",
    "anim.warp_state",
    "anim.montage_describe",
    "anim.montage_edit",
    "anim.montage_slots",
    "anim.montage_slot_edit",
    "anim.montage_control",
    "anim.montage_commands",
    "anim.scripted_warp",
    "anim.root_motion_route",
    "tables.list",
    "tables.describe",
    "tables.get",
    "tables.set",
    "tables.validate",
    "tables.export",
    "plugin.list",
    "plugin.info",
    "plugin.check",
    "plugin.enable",
    "plugin.disable",
    "plugin.reload",
    "plugin.tool_run",
    "localization.status",
    "localization.set_locale",
    "localization.resolve",
    "localization.set_pseudo",
    "localization.validate",
    "localization.plural",
    "localization.set_key_display",
    "localization.coverage",
    "localization.glyph_coverage",
    "loc.status",
    "loc.check_fit",
    "localization.set_audio_locale",
    "anim.ragdoll_set",
    "anim.ragdoll_hit",
    "ui.screens",
    "scene.stream_plan",
    "scene.hlod_select",
    "scene.large_position",
    "demo.inspect",
    "demo.seek_plan",
    "spectator.get",
    "spectator.set",
    "spectator.step",
    "demo.status",
    "demo.record_start",
    "demo.record_stop",
    "demo.play",
    "demo.pause",
    "demo.seek",
    "demo.scrub_bar",
    "spectator.bind",
    "ui.localization",
    "ui.subtitles",
    "conversation.play",
    "conversation.say",
    "conversation.choose",
    "conversation.stop",
    "conversation.validate",
    "captions.state",
    "captions.set_options",
    "ui.choice_state",
    "ui.choice_pick",
    "captions.anchor_state",
    "captions.set_dialogue_style",
    "dialogue.list",
    "dialogue.resolve",
    "dialogue.play",
    "dialogue.stop",
    "subtitle_sets.status",
    "subtitle_sets.queue",
    "subtitle_sets.stop",
    "subtitle_sets.set_enabled",
    "subtitle_sets.sync",
    "render.graph",
    "sequence.player",
    "render.shadow_lod_hints",
    "render.set_shadow_lod_hint",
    "assets.cooked_pack",
    "assets.pak_diff",
    "assets.ids_export",
    "assets.ids_import",
    "terrain.set_hole",
    "terrain.patch_heights",
    "terrain.apply_operator",
    "terrain.get",
    "terrain.backdrop_status",
    "terrain.save_holes",
    "terrain.save_paint",
    "terrain.load_paint",
    "terrain.export_heights",
    "terrain.import_heights",
    "terrain.wind",
    "terrain.set_wind",
    "terrain.scatter_stroke",
    "terrain.scatter_paint",
    "terrain.save_scatter_paint",
    "terrain.load_scatter_paint",
    "physics.destruction_define",
    "physics.destruction_damage",
    "physics.destruction_state",
    "physics.destruction_budget",
    "physics.destruction_net",
    "physics.destruction_draw",
    "physics.destruction_playback",
    "physics.destruction_load_report",
    "physics.destruction_fx_table",
    "physics.destruction_fx_stats",
    "physics.destruction_impact_damage",
    "physics.destruction_impact_damage_set",
    "net.status",
    "net.resume",
    "net.rejections",
    "net.peers",
    "net.crypto",
    "net.lockstep",
    "net.migration",
    "net.desync",
    "net.desync_inject",
    "net.replays",
    "net.replay_audit",
    "net.external_stats",
    "net.quality",
    "net.conditioner_set",
    "net.rollback",
    "net.entity",
    "net.messages",
    "net.peer_flood",
    "net.set_owner",
    "net.replicated",
    "net.entity_state",
    "net.wake",
    "net.set_asleep",
    "net.resend",
    "net.pending_refs",
    "net.assets",
    "render.foliage_wind",
    "render.set_foliage_wind",
    "render.set_dynamic_caster_shadows",
    "material.sway",
    "material.set_sway",
    "dcc_feed.status",
    "dcc_feed.items",
    "dcc_feed.accept",
    "dcc_feed.disconnect",
    "dcc_feed.record",
    "dcc_feed.replay",
    "dcc_feed.listen",
    "dcc_feed.rotate_token",
    "dcc_feed.bake",
    "dcc_feed.bake_status",
    "procgen.list",
    "procgen.get",
    "procgen.run",
    "procgen.cancel",
    "procgen.clear",
    "procgen.report",
    "procgen.inspect",
    "procgen.graph_get",
    "procgen.graph_edit",
    "procgen.node_types",
    "procgen.bake",
    "procgen.verify",
    "procgen.set_debug",
    "procgen.detach",
    "stream.open",
    "stream.status",
    "stream.cells",
    "stream.sources",
    "stream.pin",
    "stream.unpin",
    "stream.teleport",
    "stream.ready",
    "stream.budget_set",
    "stream.timing_set",
    "stream.retry",
    "stream.hlod",
    "stream.origin",
    "stream.rebase",
    "stream.trace",
    "stream.overlay",
    "stream.bounds_set",
    "stream.collision_tiles",
    "net.relevance",
    "net.why_not_relevant",
    "net.bandwidth",
    "net.set_client_rate",
    "net.profiles",
    "net.grant_relevance",
    "net.interpolation",
    "net.proxy_timeline",
    "net.set_interp_delay",
    "net.anim_state",
    "net.state_channel",
    "net.entropy_stats",
    "content.chunks",
    "content.chunk",
    "content.why",
    "content.request",
    "content.progress",
    "content.pause",
    "content.resume",
    "content.cancel",
    "content.approve_metered",
    "content.mount",
    "content.unmount",
    "content.release",
    "content.set_source",
    "content.check_host",
    "content.inject",
    "content.network",
    "level.storage",
    "level.convert",
    "level.layers",
    "level.set_layer_state",
    "level.set_membership",
    "level.placed",
    "level.place_level",
    "level.break_placed",
    "level.switch",
    "level.switch_status",
    "render.permutations",
    "render.set_permutation_policy",
    "session.state",
    "session.host",
    "session.connect",
    "session.disconnect",
    "session.travel",
    "session.manifest",
    "net.conditioner_get",
    "net.conditioner_apply",
    "net.sim_clients",
    "net.stats",
    "net.traffic",
    "server.status",
    "server.kick",
    "server.ban",
    "server.unban",
    "server.bans",
    "server.change_level",
    "server.reload_config",
    "server.shutdown",
    "server.rotation_get",
    "server.rotation_skip",
    "server.save_now",
    "server.persistence_status",
    "server.restore",
    "server.lockstep_status",
    "spline.get",
    "spline.set",
    "spline.edit",
    "spline.sample",
    "spline.closest",
    "biome.areas",
    "biome.claim",
    "biome.stats",
    "biome.def_get",
    "biome.def_edit",
    "biome.preview",
    "biome.validate_ecology",
    "procgen.tiles",
    "procgen.anchors",
    "procgen.tiles_wait",
    "procgen.tiles_set",
    "procgen.tiles_rebuild",
    "procgen.plant_stats",
    "procgen.plant_build",
    "procgen.wfc_solve",
    "procgen.profile",
    "video.probe",
    "video.list",
    "video.status",
    "video.frame_hash",
    "video.open",
    "video.play",
    "video.pause",
    "video.seek",
    "video.step",
    "video.set_rate",
    "video.set_loop",
    "video.set_volume",
    "video.select_track",
    "video.skip",
    "video.close",
    "video.create_player",
    "physics.characters",
    "anim.ragdoll_settings",
    "physics.character_traversal",
    "physics.character_traversal_probe",
    "render.get_dynamic_gi",
    "render.set_dynamic_gi",
    "net.prediction",
    "net.input_buffer",
    "net.prediction_history",
    "net.force_correction",
    "lagcomp.stats",
    "lagcomp.history",
    "lagcomp.raycast",
    "lagcomp.last_queries",
    "lagcomp.echoes",
    "lagcomp.rewound_shapes",
    "anim.retarget_check",
    "anim.crowd_status",
    "video.codecs",
    "media.state",
    "media.play",
    "media.pause",
    "media.seek",
    "media.set_loop",
    "render.get_occlusion",
    "render.set_occlusion",
    "render.set_shadow_occlusion",
    "render.get_radiance_cache",
    "render.set_radiance_cache",
    "lobby.status",
    "lobby.list",
    "lobby.create",
    "lobby.join",
    "lobby.leave",
    "lobby.set_data",
    "lobby.ready",
    "lobby.chat",
    "lobby.launch",
    "matchmaking.start",
    "matchmaking.cancel",
    "matchmaking.status",
    "net.characters",
    "net.vehicles",
    "net.hovers",
    "audio.loudness",
    "audio.set_loudness_normalization",
    "audio.music_state",
    "audio.music_transition",
    "audio.music_stinger",
    "audio.listener_info",
    "audio.bus_effects",
    "audio.bus_effect_set",
    "audio.convolution_stats",
    "audio.ambience_play",
    "audio.ambience_stop",
    "audio.ambience_list",
    "audio.time_stretch_probe",
    "ui.view_models",
    "ui.collection_state",
    "ui.set_group",
    "ui.layer_stats",
    "ui.set_scale_mode",
    "ui.image_assets",
    "render.set_vendor_upscaler",
    "render.forward_studio",
    "render.set_forward_studio",
    "terrain.trace_proxy",
    "terrain.set_trace_proxy",
    "assets.collision_report",
    "agent.job_status",
    "agent.job_cancel",
    "agent.run_tests",
    "agent.build_game",
    "agent.hot_reload",
    "agent.cook",
    "agent.package",
    "agent.open_in_editor",
    "review.propose",
    "review.list",
    "review.diff",
    "review.accept",
    "review.reject",
    "agent.golden_test",
    "agent.bless_golden",
    "profiler.set_capture",
    "profiler.frame",
    "profiler.capture",
    "profiler.compare",
    "profiler.timeline",
    "world.hlod_proxy",
    "render.set_light_profile",
    "seeing.temporal_chart",
    "physics.time_of_impact",
    "fluid.create",
    "fluid.add_emitter",
    "fluid.step",
    "fluid.stats",
    "fluid.bake",
    "fluid.play",
    "fluid.destroy",
    "net.known_hosts",
    "net.trust",
    "net.forget",
    "net.player_stats",
    "net.stats_overlay",
    "net.handoffs",
    "net.bots",
    "terrain.level_terrain",
    "terrain.set_level_terrain",
    "terrain.load_level_terrain",
    "terrain.grass_density_stroke",
    "terrain.undo_grass_density",
    "terrain.redo_grass_density",
    "terrain.edit_layers",
    "terrain.edit_layer_create",
    "terrain.edit_layer_set",
    "terrain.edit_layer_delete",
    "terrain.edit_layer_reorder",
    "terrain.edit_layer_flatten",
    "terrain.edit_layer_weights",
    "terrain.edit_layer_paint_weights",
    "terrain.edit_layer_procgen",
    "terrain.grass_density",
    "terrain.save_grass_density",
    "terrain.load_grass_density",
    "terrain.scatter_rules",
    "terrain.set_scatter_rule",
    "media.boot_state",
    "media.boot_skip",
    "media.set_visible",
    "media.cues",
    "media.decoders",
    "media.validate",
    "procgen.load_report",
    "ui.world_click",
    "ui.set_target",
    "ui.targets",
    "mods.list",
    "mods.mount",
    "mods.unmount",
    "mods.order",
    "mods.hash",
    "mods.read",
    "sequencer.spatial_voices",
    "sequencer.sub_loops",
    "sequencer.subtitles",
    "sequencer.subtitle_settings",
    "sequencer.rename_binding",
    "configurator.contract_check",
    "configurator.contract_schema",
    "configurator.open_product",
    "configurator.options",
    "configurator.set_pick",
    "configurator.build",
    "configurator.resolve",
    "viewer.view_simulate",
    "viewer.pack_verify",
    "viewer.capabilities",
    "viewer.render_still",
    "viewer.env_info",
    "looks.list",
    "looks.material",
    "looks.cut",
    "looks.render_board",
    "audio.ambience",
    "audio.set_ambience",
    "audio.ambient_spots",
    "audio.set_ambient_spot",
    "audio.probe_ambience",
    "audio.footsteps",
    "audio.set_footsteps",
    "audio.footstep",
    "assets.voxel_far",
    "assets.font_subset",
    "assets.texture_sets",
    "render.set_many_light_sampling",
    "render.many_light_stats",
    "kinds.list",
    "kinds.of",
    "kinds.lint",
    "project.describe",
    "ragdoll.state",
    "ragdoll.active_set",
    "render.ssr_stats",
    "render.set_ssr_cost",
    "fluid.set_solids",
    "fluid.solids",
    "render.voxel_far_stats",
    "net.relevance_gizmos",
    "cook.cache_stats",
    "assets.import_rules",
    "input.assists",
    "audio.mono",
    "render.color_vision",
    "input.haptics",
    "render.set_baked_gi_brick_probe",
    "ui.format_preview",
    "content.update",
    "render.frame_work",
    "physics.trailers",
    "physics.trailer_couple",
    "physics.trailer_uncouple",
    "physics.trailer_config_check",
    "render.fog_shadow",
    "vfx.vehicle_fx",
    "vfx.vehicle_fx_set",
    "net.bodies",
    "ui.components",
    "render.get_contact_calibration",
    "render.set_contact_calibration",
    "physics.aircraft",
    "physics.aircraft_input",
    "physics.aircraft_trim",
    "physics.aircraft_config_check",
    "vehicle_ai.drive_to",
    "vehicle_ai.status",
    "vehicle_ai.set_params",
    "vehicle_ai.stop",
    "ragdoll.push",
    "ai.statetree_validate",
    "ai.statetree_edit",
    "render.save_local_captures",
    "render.load_local_captures",
    "render.get_capture_bake",
    "render.restir_gi",
    "render.set_restir_gi",
    "physics.debris_budget",
    "audio.vehicle_audio",
    "audio.vehicle_audio_set",
    "audio.vehicle_audio_probe",
    "fluid.sleep_mask",
    "physics.bike_step",
    "physics.bike_state",
    "app.startup_work",
    "memory.idle_ledger",
    "platform.power_state",
    "window.get",
    "window.set",
    "vfs.mounts",
    "vfs.exists",
    "vfs.list",
    "vfs.read",
    "vfs.mount_pak",
    "renderer.minspec_explain",
    "geometry.body_probe",
    "net.quic_status",
    "seeing.flash_check",
    "render.get_visbuffer_residency",
    "render.set_visbuffer_residency",
    "audio.cues",
    "sequencer.marker_sheet",
    "render.aerial_interiors",
    "render.set_aerial_interiors",
    "render.brdf_energy",
    "render.set_brdf_energy",
    "seeing.frame_aov",
    "import.materialx_report",
    "audio.impacts",
    "audio.impact_probe",
    "audio.impacts_set",
    "physics.fields",
    "physics.field_set",
    "physics.field_probe",
    "render.vis_lod_hysteresis",
    "ai.perception_hysteresis",
    "assets.terrain_tiles",
    "sequence.record_start",
    "sequence.record_stop",
    "sequence.record_state",
    "sequencer.video_sections",
    "cutscene.start",
    "cutscene.skip",
    "cutscene.stop",
    "cutscene.state",
    "net.replay_record_start",
    "net.replay_record_stop",
    "net.replay_ring_export",
    "net.replay_open",
    "net.replay_seek",
    "net.replay_play",
    "net.replay_close",
    "net.replay_state",
    "camera_body.get",
    "camera_body.set",
    "camera_body.wake",
    "camera_body.proximity",
    "camera_body.clearance",
    "camera_body.lift_path",
    "bird.spawn",
    "bird.list",
    "bird.remove",
    "bird.trigger",
    "bird.place_on_path",
    "render.sun_specular_disc",
    "render.set_sun_specular_disc",
    "terrain.forest_query",
    "terrain.forest_corridor",
    "terrain.forest_carve",
    "terrain.forest_flight",
    "terrain.set_forest_flight",
    "render.get_ssgi_gather",
    "render.set_ssgi_gather",
    "render.area_light_fallback",
    "render.set_area_light_fallback",
    "render.sized_light_ltc",
    "render.set_sized_light_ltc",
    "render.set_host_budget",
    "procgen.fingerprint",
    "render.ambient_life",
    "render.set_ambient_life",
    "render.set_ambient_birds",
    "stream.cell_relevance",
    "assets.texture_coverage",
    "nav.raycast",
    "nav.random_point",
    "nav.crowd_set_stuck",
    "assets.texpack_dry_run",
    "physics.track_step",
    "physics.track_state",
    "vfx.significance",
    "vfx.significance_set",
    "input.glyph",
    "audio.patch_state",
    "audio.patch_trigger",
    "render.visible_entities",
    "gen.mesh",
    "gen.contour",
    "render.viscells",
    "render.set_viscells",
    "gen.texture",
    "gen.heightfield",
    "assets.push",
    "play.frame",
    "play.mute",
    "terrain.mesh_layers",
    "ui.inspect_events",
    "ui.set_live_style",
    "rules.match_get",
    "rules.match_setup",
    "rules.match_input",
    "rules.match_objective",
    "rules.match_step",
    "rules.end_stats",
    "rules.series",
    "rules.vote_cast",
    "rules.vote_status",
    "terrain.inspect_source",
    "terrain.bake_overview",
    "sim.desync_rows",
    "render.scene_hold",
    "render.set_scene_hold",
    "play.freeze_state",
    "app.pacing",
    "app.set_pacing_state",
    "render.dynres",
    "render.load_budget",
    "render.set_load_budget",
    "net.socket_buffers",
    "diagnostics.report_sources",
    "terrain.unit_view",
    "editor.asset_library",
    "editor.viewport_overlays",
    "assets.access_trace",
    "net.relay_status",
    "ai.team_knowledge",
    "ai.senses",
    "ai.formation_get",
    "ai.formation_set",
    "sim_bridge.presentation_stats",
    "terrain.seam_report",
    "assets.check_contract",
    "anim.node_drivers",
    "render.draw_list",
    "package.parity",
    "logs.perf_gate",
    "logs.triage",
    "render.cluster_lod",
    "assets.diet_report",
    "report.outbox_status",
    "terrain.scatter_colliders",
    "detsim.projectile_simulate",
    "detsim.aim_solve",
    "net.host_check",
    "input.keyboard_layout",
    "photo.enter",
    "photo.set",
    "photo.capture",
    "photo.status",
    "input.pointer_claims",
    "cook.overlay_roots",
    "content.check_placement",
    "content.install_tier",
    "detsim.hover_step",
    "detsim.hover_probe",
    "detsim.hover_list",
    "detsim.hover_get",
    "detsim.hover_set_input",
    "detsim.hover_hull_check",
    "rules.match_assign",
    "ai.debug_layers",
    "ai.debug_layers_set",
    "lobby.hub_status",
    "lobby.hub_join_status",
    "terrain.surface_at",
    "terrain.bake_report",
    "terrain.blockers",
    "detsim.ground_vehicle_simulate",
    "rules.spawn_preview",
    "rules.ruleset_list",
    "rules.ruleset_get",
    "rules.ruleset_validate",
    "rules.match_report",
    "assets.provenance_import",
    "net.capture_start",
    "net.capture_save",
    "nav.check_stair",
    "vfx.damage_decal_atlas",
    "vfx.decal_surface_at",
    "rules.match_results",
    "rules.match_result",
    "anim.locomotion_state",
    "anim.gait_simulate",
    "anim.foot_plant_simulate",
    "anim.ik_report",
    "game.pause",
    "game.resume",
    "game.pause_state",
];

/// Parameters that keep a call on the immediate path.
///
/// Empty objects are enough for most handlers (they return a structured error when a plugin is
/// absent). Three methods would otherwise do real work or wait: `play.hitch_report` defaults to
/// ten seconds, `seeing.screenshot` defaults to the window, and `seeing.reference_trace` starts
/// a GPU trace when that feature is on.
fn params_for(method: &str) -> Value {
    match method {
        "renderer.minspec_explain" => json!({ "profile": "pascal" }),
        // Never write into the crate folder: an `out` outside the project is refused, and a
        // missing `file` is a parameter error.
        "assets.ids_export" => json!({ "out": "../outside.assetids.ron" }),
        "play.hitch_report" => json!({ "seconds": 0.0 }),
        "seeing.screenshot" => json!({ "target": "nope" }),
        "seeing.reference_trace" => json!({ "width": 0 }),
        // Every scenario at 24 frames is CPU work; one short sequence keeps the call quick.
        "render.temporal_quality" => json!({ "scenario": "moving_object", "frames": 4 }),
        "render.reference_upscale" => json!({ "frames": 1, "display": [8, 8] }),
        // Waits 120 frames by default; 0 is refused at once.
        "memory.idle_ledger" => json!({ "frames": 0 }),
        "scene.spawn_prefab" => json!({ "prefab": "asset:00000000-0000-0000-0000-000000000001" }),
        "level.open" => json!({ "level": "asset:00000000-0000-0000-0000-000000000001" }),
        "level.unload" => json!({ "level": "asset:00000000-0000-0000-0000-000000000001" }),
        "physics.track_step" => {
            json!({ "dt": 0.015625, "steps": 1, "left_throttle": 1.0, "right_throttle": 1.0 })
        }
        "physics.track_state" => json!({ "left_speed": 4.0, "right_speed": 0.0 }),
        "detsim.hover_step" => json!({ "input": { "throttle": 1.0 }, "steps": 1 }),
        "detsim.hover_probe" => json!({ "script": "hop" }),
        "detsim.ground_vehicle_simulate" => json!({ "model": "wheeled", "steps": 10 }),
        "anim.gait_simulate" | "anim.foot_plant_simulate" => json!({ "steps": 10 }),
        "camera.blend_to" => json!({ "translation": [0.0, 0.0, 0.0], "duration": 0.0 }),
        _ => json!({}),
    }
}

/// One throwaway project directory for every coverage app of this test process. Without it
/// `req_path::project_root` falls back to the working directory, which is the crate's source tree
/// under `cargo test`, and the contract examples write there (`assets.ids_export`'s example wrote
/// `crates/regia_agent/asset_ids.assetids.ron`; ADR-10129).
fn coverage_project() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| regia_test_utils::unique_temp_dir("agent-coverage-project"))
        .clone()
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(crate::AgentSettings {
            project_dir: Some(coverage_project()),
        })
        .insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: 10_000,
                heavy_rate_limit_per_second: 10_000,
                ..AgentConfig::default()
            },
            source: None,
        })
        .init_resource::<crate::safety::AuditLog>()
        .init_resource::<crate::safety::AgentSession>()
        .init_resource::<AgentTimeControl>()
        .init_resource::<RateLimit>()
        .init_resource::<AppTypeRegistry>();
    let world = app.world_mut();
    // Queries panic on a component the world has never seen. These handlers query
    // unconditionally when their other arguments are absent.
    let _ = world.register_component::<regia_scene::Transform>();
    let _ = world.register_component::<HoverWash>();
    let _ = world.register_component::<Sprite>();
    let _ = world.register_component::<TileMap>();
    let _ = world.register_component::<Animator>();
    let _ = world.register_component::<CameraRig>();
    let _ = world.register_component::<PrimaryWindow>();
    app
}

/// `play.step` stores the reply and returns it from `advance_step` after the frames run.
/// Finish that reply here so the test observes the dispatch without spinning the schedule.
fn finish_step(app: &mut App) {
    let world = app.world_mut();
    let pending = {
        let mut control = world.resource_mut::<AgentTimeControl>();
        let paused = control.paused;
        control.stepping.take().map(|(_, id, reply)| (id, reply, paused))
    };
    let Some((id, reply, paused)) = pending else {
        return;
    };
    {
        let mut time = world.resource_mut::<Time>();
        time.paused = paused;
    }
    let frame = world.resource::<Time>().frame_count();
    let _ = reply.send(Response::ok(id, json!({ "frame": frame, "paused": paused })));
}

fn call(app: &mut App, method: &str, mut params: Value) -> Result<Response, String> {
    let _power_directory = isolate_power_directory(method, &mut params);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    inbox_tx
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply: tx,
        })
        .map_err(|_| format!("{method}: failed to queue the request"))?;
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    dispatch::process_requests(app.world_mut());
    drop(inbox_tx);
    if method == "play.step" {
        finish_step(app);
    }
    rx.recv_timeout(Duration::from_secs(2))
        .map_err(|_| format!("{method} did not answer (deferred with no reply)"))
}

/// The coverage and seeded-fuzz apps resolve project paths into a temp directory, never into the
/// source tree (ADR-10129): `assets.ids_export`'s contract example `{ "out": "asset_ids.assetids.ron" }`
/// used to land in `crates/regia_agent/` because `project_root` fell back to the working directory.
#[test]
fn coverage_apps_write_outside_the_source_tree() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mut app = app();
    let root = crate::req_path::project_root(app.world());
    assert!(
        !root.starts_with(&workspace),
        "the coverage app's project root {} is inside the source tree {}",
        root.display(),
        workspace.display()
    );
    let example = crate::contract::contract("assets.ids_export")
        .map(crate::contract::MethodContract::example_json)
        .unwrap_or_default();
    let response = call(&mut app, "assets.ids_export", example.clone()).unwrap_or_else(|err| panic!("{err}")); // invariant: test-only (cfg(test)); a transport failure fails the test
    assert!(response.error.is_none(), "{example}: {:?}", response.error);
    let file = example["out"].as_str().unwrap_or("asset_ids.assetids.ron");
    assert!(
        root.join(file).is_file(),
        "the example wrote {}",
        root.join(file).display()
    );
}

#[test]
fn every_dispatched_method_is_called() {
    let mut schema: Vec<&str> = crate::schema::METHODS.iter().map(|m| m.name).collect();
    schema.sort_unstable();
    let mut called = CALLED.to_vec();
    called.sort_unstable();
    assert_eq!(
        called, schema,
        "CALLED must name every schema method once; add a literal and a dispatch call"
    );

    let mut app = app();
    for method in CALLED {
        let params = params_for(method);
        let response = catch_unwind(AssertUnwindSafe(|| call(&mut app, method, params)));
        let response = match response {
            Ok(response) => response.unwrap_or_else(|err| panic!("{err}")), // invariant: test-only (cfg(test)); a transport failure fails the coverage test
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic payload");
                panic!("{method} panicked: {message}"); // invariant: test-only (cfg(test)); re-raises a handler panic with its method name
            }
        };
        let regia = response
            .error
            .as_ref()
            .and_then(|error| error.data.as_ref())
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_ne!(
            response.error.as_ref().map(|error| error.code),
            Some(protocol::METHOD_NOT_FOUND),
            "{method} is not registered: {response:?}"
        );
        assert_ne!(regia, Some("F0008"), "{method} answered F0008: {response:?}");
    }
}

/// Hostile values for one parameter of JSON type `kind`: wrong types, extremes, empty and
/// non-ASCII text. Paths are deliberately absent so no case can write outside the sandbox.
fn hostile_values(kind: &str) -> Vec<Value> {
    let extremes = [json!(-1), json!(u64::MAX), json!(1e308), json!(1.5), json!(0)];
    let mut values = match kind {
        "string" => vec![json!(5), json!([]), json!(""), json!("\u{0}é"), json!(null)],
        "number" => vec![json!("x"), json!([]), json!(null), json!(true)],
        "boolean" => vec![json!("x"), json!(5), json!([])],
        "object" => vec![json!([]), json!("x"), json!(5), json!({}), json!(null)],
        "array" => vec![
            json!({}),
            json!("x"),
            json!(5),
            json!([]),
            json!([null]),
            json!([[], {}]),
        ],
        _ => vec![json!(null), json!([]), json!({}), json!("x")],
    };
    if matches!(kind, "number" | "any") {
        values.extend(extremes);
    }
    values
}

/// Every method, called with malformed parameters (non-object params, and each declared
/// parameter alone with hostile values), must answer — never panic (a panic aborts the
/// shipping build) and never come back as an unregistered method.
#[test]
fn malformed_params_never_panic_any_method() {
    let mut undeclared = Vec::new();
    for schema in crate::schema::METHODS {
        // A real GPU trace job; not a parameter-validation target.
        if schema.name == "seeing.reference_trace" {
            continue;
        }
        let mut cases: Vec<Value> = vec![json!(null), json!([]), json!("x"), json!(5), json!(true)];
        for param in schema.params {
            for value in hostile_values(param.kind) {
                let mut object = serde_json::Map::new();
                object.insert(param.name.to_string(), value);
                cases.push(Value::Object(object));
            }
        }
        for params in cases {
            let mut app = app();
            app.world_mut().insert_resource(RateLimit::default());
            let shown = params.to_string();
            let response = catch_unwind(AssertUnwindSafe(|| call_nowait(&mut app, schema.name, params)));
            let Ok(response) = response else {
                panic!("{} panicked on params {shown}", schema.name); // invariant: test-only (cfg(test)); names the method and the input that aborted it
            };
            if let Some(error) = response.and_then(|r| r.error) {
                assert_ne!(
                    error.code,
                    protocol::METHOD_NOT_FOUND,
                    "{} on {shown}",
                    schema.name
                );
                undeclared.extend(contract_violation(schema.name, &error, &shown));
            }
        }
    }
    assert!(
        undeclared.is_empty(),
        "errors outside the method contracts (add the code to the method's `errors` in contract.rs \
         if it is intended):\n{}",
        undeclared.join("\n")
    );
}

/// Why `error` breaks `method`'s contract, if it does: the `data.code` must be a well-formed
/// protocol code, listed by the contract or universal, and come with a hint.
fn contract_violation(method: &str, error: &protocol::RpcError, shown: &str) -> Option<String> {
    let data = error.data.as_ref();
    let Some(code) = data.and_then(|d| d.get("code")).and_then(Value::as_str) else {
        return Some(format!("{method} on {shown}: error without data.code: {error:?}"));
    };
    if !crate::contract::is_error_code(code) {
        return Some(format!("{method} on {shown}: malformed code {code:?}"));
    }
    if data.and_then(|d| d.get("hint")).and_then(Value::as_str).is_none() {
        return Some(format!("{method} on {shown}: {code} has no hint"));
    }
    let declared = crate::contract::contract(method).is_some_and(|c| c.errors.contains(&code));
    if !declared && !crate::contract::UNIVERSAL_ERRORS.contains(&code) {
        return Some(format!("{method} on {shown}: {code} ({})", error.message));
    }
    None
}

/// The seeded fuzz's strings all start with a NUL, so this walks the rest of the hostile paths
/// through the dispatcher, in every path parameter of the `configurator.*` methods: each answer
/// must stay inside the method's contract and carry no operating system text.
#[test]
fn configurator_path_params_answer_inside_the_contract() {
    let mut app = app();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate sits in crates/")
        .join("regia_configurator/tests/fixtures/runtime");
    let mut paths = crate::configurator::hostile_paths();
    paths.extend(
        [
            "",
            ".",
            "no-such-folder",
            "no-such-folder/x",
            "src",
            "Cargo.toml",
            "Cargo.toml/x",
        ]
        .map(str::to_owned),
    );
    paths.extend([
        "z".repeat(300),
        "a/".repeat(3000),
        "\u{202e}rtl".to_owned(),
        "a\nb".to_owned(),
    ]);
    let mut problems = Vec::new();
    let mut answered = 0;
    for path in &paths {
        let mut calls = vec![(
            "configurator.contract_check",
            json!({ "document": { "schema": "jewelry.build/1" }, "package_dir": path }),
        )];
        calls.push((
            "configurator.open_product",
            json!({ "product_key": "house-a/p0001", "root": path, "snapshot_id": "x" }),
        ));
        for name in [
            "option_graph",
            "recipe",
            "library",
            "snapshot_id",
            "library_version",
        ] {
            let mut params = json!({ "product_key": "house-a/p0001", "root": root, "snapshot_id": "2026-11-04-000000000000" });
            params[name] = json!(path);
            calls.push(("configurator.open_product", params));
        }
        calls.push((
            "configurator.open_product",
            json!({ "product_key": "house-a/p0001", "root": root, "snapshot_id": "2026-11-04-000000000000", "kits": [path] }),
        ));
        for (method, params) in calls {
            let shown = params.to_string();
            app.world_mut().insert_resource(RateLimit::default());
            let Some(response) = call_nowait(&mut app, method, params) else {
                problems.push(format!("{method} on {shown}: no answer"));
                continue;
            };
            answered += 1;
            if let Some(error) = response.error {
                problems.extend(contract_violation(method, &error, &shown));
                if error.message.contains("os error") {
                    problems.push(format!("{method} on {shown}: OS text: {}", error.message));
                }
            }
        }
    }
    assert_eq!(answered, paths.len() * 8, "{}", problems.join("\n"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Like [`call`], but a deferred method that has not answered yet is `None` instead of an error.
fn call_nowait(app: &mut App, method: &str, mut params: Value) -> Option<Response> {
    let _power_directory = isolate_power_directory(method, &mut params);
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    let _ = inbox_tx.send(Inbound {
        request: Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        },
        reply: tx,
    });
    app.world_mut().insert_resource(AgentInbox(inbox_rx));
    dispatch::process_requests(app.world_mut());
    drop(inbox_tx);
    if method == "play.step" {
        finish_step(app);
    }
    rx.try_recv().ok()
}

/// The ranges the audit found unbounded: each must be refused as `invalid_params`.
#[test]
fn out_of_range_params_are_invalid_params() {
    let cases = [
        ("play.step", json!({ "frames": u64::MAX })),
        ("play.step", json!({ "frames": 0 })),
        ("play.step", json!({ "frames": -1 })),
        ("play.step", json!({ "frames": "x" })),
        (
            "seeing.capture_sequence",
            json!({ "every_n": 1_000_000_000_000_000_000u64 }),
        ),
        ("seeing.capture_sequence", json!({ "every_n": 0 })),
        ("seeing.capture_sequence", json!({ "every_n": "x" })),
        (
            "seeing.screenshot",
            json!({ "target": "offscreen", "width": 4_294_967_297u64 }),
        ),
        (
            "seeing.screenshot",
            json!({ "target": "offscreen", "width": 4097 }),
        ),
        ("seeing.screenshot", json!({ "target": "offscreen", "height": 0 })),
        (
            "seeing.screenshot",
            json!({ "target": "offscreen", "height": "x" }),
        ),
        ("scene.apply_edits", json!({ "edits": [] })),
        ("scene.apply_edits", json!({ "edits": "x" })),
        ("scene.apply_edits", json!({ "edits": [{ "op": "nope" }] })),
        (
            "audio.loopgen_render",
            json!({ "ron": "x", "wav": true, "out_dir": "../x" }),
        ),
    ];
    for (method, params) in cases {
        let mut app = app();
        let shown = params.to_string();
        let response =
            call_nowait(&mut app, method, params).unwrap_or_else(|| panic!("{method} {shown}: no answer")); // invariant: test-only (cfg(test))
        let error = response
            .error
            .unwrap_or_else(|| panic!("{method} {shown}: accepted")); // invariant: test-only (cfg(test))
        assert_eq!(
            error.code,
            protocol::INVALID_PARAMS,
            "{method} {shown}: {error:?}"
        );
    }
}

/// Every [`crate::contract::Limit`] is enforced by the dispatcher before the handler runs, in an
/// app with no subsystems at all: one value just past each bound answers `F0002` and names the
/// limit in `data.limit`. Nothing reaches a handler, so no case has a side effect.
#[test]
fn every_contract_limit_is_enforced_before_the_handler() {
    use crate::contract::{CONTRACTS, Limit};
    let mut problems = Vec::new();
    for contract in CONTRACTS {
        for limit in contract.limits {
            let (cases, kind): (Vec<Value>, &str) = match *limit {
                Limit::Range { min, max, .. } => (vec![json!(max + 1.0), json!(min - 1.0)], "range"),
                Limit::Items { max, .. } => (vec![Value::Array(vec![Value::Null; max + 1])], "items"),
                Limit::Bytes { max, .. } => (vec![Value::String("x".repeat(max + 1))], "bytes"),
            };
            for value in cases {
                let mut params = serde_json::Map::new();
                params.insert(limit.param().to_string(), value);
                let mut app = app();
                // `agent.run_tests` needs `build`, `review.accept` needs `review` (ADR-2346) and
                // `assets.pak_diff` needs `tools`: the permission check runs first and must let the
                // call reach the limit under test.
                for permission in ["build", "review", "tools"] {
                    app.world_mut()
                        .resource_mut::<AgentPermissions>()
                        .config
                        .permissions
                        .insert(permission.to_owned());
                }
                let response = call_nowait(&mut app, contract.method, Value::Object(params));
                let error = response.and_then(|r| r.error);
                let data = error.as_ref().and_then(|e| e.data.clone()).unwrap_or_default();
                let refused = error.as_ref().is_some_and(|e| e.code == protocol::INVALID_PARAMS)
                    && data["limit"]["param"] == limit.param()
                    && data["limit"]["kind"] == kind;
                if !refused {
                    problems.push(format!("{} {}: {error:?}", contract.method, limit.describe()));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "limits not enforced:\n{}",
        problems.join("\n")
    );
}

/// The default caps hold for every method with array or string params and no own limit.
#[test]
fn default_size_caps_hold_for_every_method() {
    let mut app = app();
    // `plugin.*` writes need `build`: the permission check runs first and must let the call reach
    // the size check under test.
    app.world_mut()
        .resource_mut::<AgentPermissions>()
        .config
        .permissions
        .insert("build".to_owned());
    for schema in crate::schema::METHODS {
        let own = |name: &str| {
            crate::contract::contract(schema.name).is_some_and(|c| c.limits.iter().any(|l| l.param() == name))
        };
        for param in schema.params.iter().filter(|p| !own(p.name)) {
            let value = match param.kind {
                "array" => Value::Array(vec![Value::Null; crate::contract::DEFAULT_MAX_ITEMS + 1]),
                "string" => Value::String("x".repeat(crate::contract::DEFAULT_MAX_BYTES + 1)),
                _ => continue,
            };
            let mut params = serde_json::Map::new();
            params.insert(param.name.to_string(), value);
            app.world_mut().insert_resource(RateLimit::default());
            let error = call_nowait(&mut app, schema.name, Value::Object(params)).and_then(|r| r.error);
            assert_eq!(
                error.map(|e| e.code),
                Some(protocol::INVALID_PARAMS),
                "{} `{}` over the default cap",
                schema.name,
                param.name
            );
        }
    }
}

/// Methods left out of the seeded fuzz: real GPU work, or calls whose purpose is writing files or
/// ending the app. The rest only ever see strings with a NUL byte, which no file system accepts
/// as a name, so no case can create a file.
const FUZZ_SKIP: &[&str] = &[
    "seeing.reference_trace",
    "seeing.compare",
    "seeing.capture_plate",
    "save.save",
    "save.quick_save",
    "save.delete",
    "render.out_start",
    "audio.loopgen_render",
    "feedback.capture",
    "quit.request",
    "profiler.capture",
    // `write: true` without `project_dir` writes project-map.md/.json into the app's project root,
    // which is the crate folder under `cargo test`.
    "project.describe",
    // Two extra frames on the GPU (ADR-2951).
    "seeing.frame_aov",
];

/// A small deterministic generator (xorshift64*, Vigna 2016), so the fuzz needs no dependency and
/// every failure names a reproducible case.
struct Fuzz(u64);

impl Fuzz {
    fn seeded(name: &str) -> Self {
        // FNV-1a of the method name: a different stream per method, the same on every run.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in name.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
        Self(hash | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn number(&mut self) -> Value {
        let picks = [
            json!(0),
            json!(1),
            json!(-1),
            json!(0.5),
            json!(-0.0),
            json!(7),
            json!(1e308),
            json!(-1e308),
            json!(1e-300),
            json!(i64::MIN),
            json!(u64::MAX),
            json!(9_007_199_254_740_993u64),
            json!(4_294_967_296u64),
            json!(65_535),
        ];
        picks[self.below(picks.len())].clone()
    }

    fn string(&mut self) -> Value {
        let picks = ["\u{0}", "\u{0}é", "\u{0}../x", "\u{0}F0002", "\u{0}\u{202e}rtl"];
        let mut text = picks[self.below(picks.len())].to_string();
        if self.below(8) == 0 {
            text.push_str(&"z".repeat(300));
        }
        Value::String(text)
    }

    fn scalar(&mut self) -> Value {
        match self.below(4) {
            0 => Value::Null,
            1 => Value::Bool(self.below(2) == 0),
            2 => self.number(),
            _ => self.string(),
        }
    }

    fn any(&mut self, keys: &[&str], depth: u32) -> Value {
        if depth == 0 {
            return self.scalar();
        }
        match self.below(6) {
            0 => {
                let len = self.below(5);
                Value::Array((0..len).map(|_| self.any(keys, depth - 1)).collect())
            }
            1 => {
                let mut object = serde_json::Map::new();
                for _ in 0..self.below(4) {
                    let key = if keys.is_empty() || self.below(3) == 0 {
                        "x".to_string()
                    } else {
                        keys[self.below(keys.len())].to_string()
                    };
                    object.insert(key, self.any(keys, depth - 1));
                }
                Value::Object(object)
            }
            _ => self.scalar(),
        }
    }

    fn of_kind(&mut self, kind: &str, keys: &[&str]) -> Value {
        if self.below(5) < 2 {
            return self.any(keys, 2);
        }
        match kind {
            "number" | "integer" => self.number(),
            "string" => self.string(),
            "boolean" => Value::Bool(self.below(2) == 0),
            "array" => {
                let len = self.below(4);
                Value::Array((0..len).map(|_| self.any(keys, 1)).collect())
            }
            _ => self.any(keys, 2),
        }
    }
}

/// Seeded structural fuzz of the dispatcher: every method gets its contract example, then the
/// same 24 random parameter
/// objects on every run (declared params with hostile values of their kind or of any kind, plus
/// stray keys), one app per method so state from earlier calls carries over. Every answer must
/// come back without a panic, and every error must be well formed and inside the method's
/// contract. A failure names the method, the case number and the params.
#[test]
fn seeded_fuzz_answers_inside_the_contract() {
    const CASES: usize = 24;
    let mut problems = Vec::new();
    for schema in crate::schema::METHODS {
        if FUZZ_SKIP.contains(&schema.name) {
            continue;
        }
        let mut fuzz = Fuzz::seeded(schema.name);
        let keys: Vec<&str> = schema.params.iter().map(|p| p.name).collect();
        let mut app = app();
        // The contract's own example first: the gate must pass it, and the answer must stay
        // inside the contract like any other.
        if let Some(contract) = crate::contract::contract(schema.name) {
            let example = contract.example_json();
            let shown = example.to_string();
            let response = catch_unwind(AssertUnwindSafe(|| call_nowait(&mut app, schema.name, example)));
            let Ok(response) = response else {
                panic!("{} panicked on its contract example {shown}", schema.name); // invariant: test-only (cfg(test)); names the method whose example aborted
            };
            if let Some(error) = response.and_then(|r| r.error) {
                if error.data.as_ref().is_some_and(|d| d.get("limit").is_some()) {
                    problems.push(format!(
                        "{}: the gate refused its own example: {}",
                        schema.name, error.message
                    ));
                }
                problems.extend(contract_violation(schema.name, &error, &shown));
            }
        }
        for case in 0..CASES {
            let mut params = serde_json::Map::new();
            for param in schema.params {
                if fuzz.below(2) == 0 {
                    params.insert(param.name.to_string(), fuzz.of_kind(param.kind, &keys));
                }
            }
            if fuzz.below(4) == 0 {
                params.insert("x".to_string(), fuzz.scalar());
            }
            let params = Value::Object(params);
            let shown = params.to_string();
            app.world_mut().insert_resource(RateLimit::default());
            let response = catch_unwind(AssertUnwindSafe(|| call_nowait(&mut app, schema.name, params)));
            let Ok(response) = response else {
                panic!("{} case {case} panicked on params {shown}", schema.name); // invariant: test-only (cfg(test)); names the reproducible case
            };
            let Some(response) = response else {
                continue; // deferred: answered on a later frame
            };
            if response.result.is_some() == response.error.is_some() {
                problems.push(format!(
                    "{} case {case}: not exactly one of result/error",
                    schema.name
                ));
            }
            if let Some(error) = &response.error {
                problems.extend(contract_violation(schema.name, error, &shown));
            }
        }
    }
    assert!(problems.is_empty(), "seeded fuzz:\n{}", problems.join("\n"));
}

/// Keeps power coverage calls off the host sysfs, including omitted and null `dir` cases.
struct PowerDirectoryFixture(std::path::PathBuf);

impl Drop for PowerDirectoryFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn isolate_power_directory(method: &str, params: &mut Value) -> Option<PowerDirectoryFixture> {
    if method != "platform.power_state" {
        return None;
    }
    let object = params.as_object_mut()?;
    if object.get("dir").is_some_and(|dir| !dir.is_null()) {
        return None;
    }
    let directory = regia_test_utils::unique_temp_dir("power-coverage");
    object.insert("dir".into(), json!(directory));
    Some(PowerDirectoryFixture(directory))
}

#[test]
fn power_coverage_uses_and_removes_temporary_directories() {
    for mut params in [json!({}), json!({ "dir": null })] {
        let fixture = isolate_power_directory("platform.power_state", &mut params)
            .unwrap_or_else(|| panic!("host power reads need a temporary directory"));
        let directory = fixture.0.clone();
        let state = regia_platform::read_power_supply_dir(&directory)
            .unwrap_or_else(|err| panic!("temporary supply: {err}"));
        assert_eq!(state, regia_platform::PowerState::unknown());
        assert_eq!(params["dir"], json!(directory));
        drop(fixture);
        assert!(!directory.exists());
    }
    let mut explicit = json!({ "dir": "" });
    assert!(isolate_power_directory("platform.power_state", &mut explicit).is_none());
    assert_eq!(explicit["dir"], "");
}

/// ADR-7325: `profiler.timeline` through the dispatcher answers `timeline_off` until
/// `profiler.set_capture { timeline: true }`, then one row per thread of the recorded frame.
#[test]
fn profiler_timeline_answers_through_the_dispatcher() {
    let mut app = app();
    let profiler = regia_core::profiling::FrameProfiler::new();
    app.world_mut()
        .insert_resource(regia_telemetry::SpanProfiler(profiler.clone()));
    let reason = |response: &Response| {
        response
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d["reason"].as_str().map(str::to_owned))
    };
    let off = call(&mut app, "profiler.timeline", json!({})).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(reason(&off).as_deref(), Some("timeline_off"));
    let on =
        call(&mut app, "profiler.set_capture", json!({ "timeline": true })).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        on.result.as_ref().map(|r| r["timeline"].clone()),
        Some(json!(true))
    );
    let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        profiler.clone(),
    ));
    tracing::dispatcher::with_default(&dispatch, || {
        let outer = tracing::info_span!("dispatch_tl_outer").entered();
        let d = dispatch.clone();
        std::thread::Builder::new()
            .name("dispatch-tl-worker".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&d, || {
                    let _w = tracing::info_span!("dispatch_tl_worker").entered();
                });
            })
            .and_then(|h| h.join().map_err(|_| std::io::Error::other("panicked")))
            .unwrap_or_else(|e| panic!("worker: {e}"));
        drop(outer);
        profiler.end_frame();
    });
    let answer =
        call(&mut app, "profiler.timeline", json!({ "max_spans": 10 })).unwrap_or_else(|e| panic!("{e}"));
    let result = answer
        .result
        .unwrap_or_else(|| panic!("error: {:?}", answer.error));
    let names: Vec<&str> = result["threads"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(names.len(), 2, "{result:#}");
    assert!(names.contains(&"dispatch-tl-worker"), "{names:?}");
    assert_eq!(result["spans"], 2);
}

/// ADR-7325 (w6): `profiler.timeline { frames }` answers the spans of each retained frame on one
/// time axis, every frame with its start offset, and `profiler.frame` counts the systems past
/// the profiler's cap; the dispatcher's limit gate refuses `frames` outside 1..=8 first.
#[test]
fn profiler_timeline_frames_answer_through_the_dispatcher() {
    let mut app = app();
    let profiler = regia_core::profiling::FrameProfiler::new();
    profiler.set_timeline_enabled(true);
    app.world_mut()
        .insert_resource(regia_telemetry::SpanProfiler(profiler.clone()));
    let dispatch = tracing::Dispatch::new(tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        profiler.clone(),
    ));
    tracing::dispatcher::with_default(&dispatch, || {
        for i in 0..4u64 {
            let _span = tracing::info_span!("strip_dispatch_work", i = i).entered();
            profiler.end_frame();
        }
    });
    let answer = call(
        &mut app,
        "profiler.timeline",
        json!({ "frames": 3, "max_spans": 100 }),
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .result
    .unwrap_or_else(|| panic!("error"));
    let frames = answer["frames"].as_array().cloned().unwrap_or_default();
    assert_eq!(frames.len(), 3, "{answer:#}");
    assert_eq!(frames[0]["number"], 1, "the three newest frames, oldest first");
    assert_eq!(frames[0]["start_ns"], 0);
    let extent = frames[0]["span_ns"].as_u64().unwrap_or(0);
    assert!(extent > 0, "{answer:#}");
    assert_eq!(frames[1]["start_ns"], extent, "each frame starts past the last");
    assert_eq!(answer["spans"], 3, "{answer:#}");
    let spans: Vec<(u64, u64)> = answer["threads"]
        .as_array()
        .and_then(|t| t.first())
        .and_then(|t| t["spans"].as_array().map(|s| s.to_vec()))
        .unwrap_or_default()
        .iter()
        .filter_map(|s| Some((s["start_ns"].as_u64()?, s["end_ns"].as_u64()?)))
        .collect();
    assert_eq!(spans.len(), 3);
    assert!(
        spans[1].0 >= spans[0].1,
        "the later frames' spans sit at or after the earlier ones' ends: {spans:?}"
    );
    // A held frame ends the strip.
    let held = call(&mut app, "profiler.timeline", json!({ "frame": 2, "frames": 2 }))
        .unwrap_or_else(|e| panic!("{e}"))
        .result
        .unwrap_or_else(|| panic!("error"));
    assert_eq!(
        held["frames"]
            .as_array()
            .and_then(|f| f.first().and_then(|f| f["number"].as_u64())),
        Some(1),
        "{held:#}"
    );
    // The dispatcher's limit gate refuses an out-of-range strip before the handler runs.
    let refused = call(&mut app, "profiler.timeline", json!({ "frames": 9 }))
        .unwrap_or_else(|e| panic!("{e}"))
        .error
        .expect("refused");
    assert_eq!(
        refused.data.as_ref().unwrap_or(&serde_json::Value::Null)["limit"]["param"],
        "frames",
        "{refused:?}"
    );
    // `profiler.frame` reports the systems past the cap.
    let frame = call(&mut app, "profiler.frame", json!({}))
        .unwrap_or_else(|e| panic!("{e}"))
        .result
        .expect("frame");
    assert_eq!(
        frame["systems_over_cap"].as_u64(),
        Some(regia_core::profiling::systems_over_cap() as u64),
        "{frame:#}"
    );
}
