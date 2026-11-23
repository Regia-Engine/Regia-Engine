// SPDX-License-Identifier: MPL-2.0
//! Engine-side rows of the DO27 alpha smoke list. CPU rows stand alone;
//! [`apply_gpu`] fills the rows the headless session measures.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder, RigidBodyType};
use rapier3d::geometry::ColliderBuilder;
use rapier3d::math::Vector;
use regia_animation::components::spawn_skeleton;
use regia_animation::ik::{IkChain, solve_two_bone};
use regia_animation::physics_blend::{PhysicsPoseBlend, blend_physics_pose};
use regia_animation::pose::translated;
use regia_animation::retarget::{RetargetProfile, Retargeter};
use regia_animation::skeleton::Skeleton;
use regia_animation::{AnimationClip, AnimationPlugin, Animator};
use regia_app::{App, FixedTime, MinimalPlugins, TimeUpdateStrategy, enable_fixed_step};
use regia_asset::{
    AssetId, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, LoadKind, LoadSet, LoadSets, PakEntry, PakReader,
    PakStreamer, PakToc, StreamRequest, encode_pak_header,
};
use regia_audio::{
    AudioMedium, AudioPropagation, EmitterCategory, GroundMaterial, ListenerFrame,
    MARS_ABSORPTION_DB_PER_M_KHZ2, MARS_LEVEL_OFFSET_DB, MARS_SPEED_OF_SOUND, PathClass,
};
use regia_core::math::{Quat, Vec3};
use regia_framework::input_recording::{InputRecorder, InputRecording, InputRecordingPlugin};
use regia_framework::ragdoll::{
    Ragdoll, RagdollBodies, RagdollBone, RagdollMode, RagdollPlugin, RagdollProfile, RagdollShape,
};
use regia_input::{ActionKind, Actions, InputConfig, InputInjection, InputPlugin};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsPose, PhysicsWorld, RigidBody};
use regia_render::glass::{GlassDamage, GlassView, GlassViewMode, INSIDE_SPECULAR_DEFAULT};
use regia_render::{
    AtmosphereParameters, BudgetTier, FogSettings, FrameQuality, GiLook, GroundBounce, MemoryBudget,
    MemoryUse, PostSettings, RenderProfile, RenderProfilePreset, SkyBody, TonemapOperator, admit,
    resident_cap,
};
use regia_scene::Transform;
use regia_scene::level::{AtmospherePreset, LevelLook};
use regia_terrain::{
    HoleShape, HolesMask, PlacementInputs, RockStyle, ScatterSet, TerrainMap, place_set, procedural_rock,
};
use regia_vfx::atmos_presets::explosion_in;
use regia_vfx::combat::{DO27_WEAPONS, HitKind, combat_effect_in};
use regia_vfx::format::VfxSystemDef;
use regia_vfx::glass::{BreachAtmosphere, GlassEffectSettings, breach_airless, breach_earth, breach_mars};
use regia_vfx::hover::{HoverSurface, hover_wash_in};
use regia_vfx::medium::{EUROPA_GRAVITY, VfxMedium};
use regia_vfx::plume::{PlumeKind, plume_in};
use regia_vfx::scene::VfxLibrary;

use super::gpu::GpuFacts;
use super::report::{Check, RSS_BUDGET_BYTES, Status, hitch_failures, rss_within_budget, within_boot_budget};

/// Rows 1–18 are the alpha list (eighteen areas, unchanged). Rows 19–21 are the
/// v0.1.5 scene rows: ultrawide, black sky, and many units. The many-units row
/// reports its timings; it does not fail the run. `content` is `REGIA_DO27_CONTENT`
/// when that directory exists; the checks only read it.
#[must_use]
pub fn engine_checks(workspace: &Path, content: Option<&Path>) -> Vec<Check> {
    vec![
        na(
            1,
            "Boot",
            "GPU session not run",
            "first present is measured on the headless device",
        ),
        lobby(),
        na(
            3,
            "Ready",
            "GPU session not run",
            "camera cut and the first present need a device",
        ),
        na(
            4,
            "Hitches",
            "GPU session not run",
            "the scripted window counts frames over 50 ms after warm-up",
        ),
        na(
            5,
            "Memory",
            memory_policy(),
            "RSS, texture residency and BC7 are measured in the GPU session",
        ),
        look(),
        terrain(workspace, content),
        combat(),
        explosions(),
        glass(),
        audio(),
        hud_format(),
        pilots(),
        na(14, "Hulls", "game content", "hull meshes are not an engine check"),
        na(15, "AI", "game sim", "squad AI is not an engine check"),
        determinism(),
        na(
            17,
            "Quit",
            "GPU session not run",
            "Vulkan validation is popped after the session",
        ),
        na(
            18,
            "Packaging",
            "Arch tarball",
            "packaging is not an engine runtime check",
        ),
        na(
            19,
            "Ultrawide",
            "GPU session not run",
            "a 3840x1600 frame is judged with the same color check as the smoke frame",
        ),
        na(
            20,
            "Black sky",
            "GPU session not run",
            "an airless camera jump with TAAU on is compared with a still of the same view",
        ),
        na(
            21,
            "Many units",
            "GPU session not run",
            "180 moving instances are timed against a box control; smoke reports the timings and the cargo test gates the slack",
        ),
    ]
}

/// Fills the rows the headless session measures.
pub(crate) fn apply_gpu(checks: &mut [Check], gpu: &GpuFacts) {
    let boot_ok = within_boot_budget(gpu.boot_ms);
    replace(
        checks,
        1,
        if boot_ok { Status::Pass } else { Status::Fail },
        format!(
            "device {:.1} ms, branding {:.1} ms, boot {:.1} ms on {} (prewarm {:.1} ms, first game frame {:.1} ms)",
            gpu.device_ms, gpu.splash_ms, gpu.boot_ms, gpu.adapter, gpu.prewarm_ms, gpu.first_present_ms
        ),
        if boot_ok {
            "device and the branding frame finished under 1 s".to_owned()
        } else {
            format!("boot (device + branding) {:.1} ms is not under 1 s", gpu.boot_ms)
        },
    );

    if let Some(row) = checks.iter_mut().find(|c| c.id == 2) {
        row.evidence = format!("{}; {}", row.evidence, gpu.warm_note);
        if gpu.warm_ok {
            row.reason = "the streamer delivered the pak and the warm queue opened the lobby only after boot"
                .to_owned();
        } else {
            row.status = Status::Fail;
            row.reason = "the warm queue did not keep the lobby closed until its own tier ran".to_owned();
        }
    }

    let ready_ok = within_boot_budget(gpu.first_present_ms) && gpu.colour_ok && gpu.camera_cuts > 0;
    replace(
        checks,
        3,
        if ready_ok { Status::Pass } else { Status::Fail },
        format!(
            "first game frame {:.1} ms, camera cuts {}, mean {:.1} stddev {:.1} near-white {:.3}, {}",
            gpu.first_present_ms,
            gpu.camera_cuts,
            gpu.colour.mean,
            gpu.colour.stddev,
            gpu.colour.near_white,
            gpu.screenshot
        ),
        if ready_ok {
            "first game frame after prewarm is under 1 s, the frame has RGB variance, and camera_cut counted"
                .to_owned()
        } else {
            "ready needs a game frame under 1 s, real RGB colour and a camera cut".to_owned()
        },
    );

    let mut hitch_bad = Vec::new();
    let mut hitch_evidence = Vec::new();
    for (map, report) in &gpu.hitches {
        hitch_evidence.push(format!("{map}: {}", report.summary()));
        for line in hitch_failures(report) {
            hitch_bad.push(format!("{map}: {line}"));
        }
    }
    for (map, creates) in &gpu.hitch_creates {
        if !creates.is_empty() {
            hitch_bad.push(format!("{map} creates by label: {creates}"));
        }
    }
    replace(
        checks,
        4,
        if hitch_bad.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        hitch_evidence.join(" | "),
        if hitch_bad.is_empty() {
            "no measured frame over 50 ms and no pipeline, bind-group, buffer or texture creates after warm-up"
                .to_owned()
        } else {
            hitch_bad.join("; ")
        },
    );

    let rss_ok = rss_within_budget(gpu.rss_bytes);
    let resident_ok = gpu.resident_bytes > 0;
    let mem_ok = !gpu.over_budget && rss_ok && gpu.bc7 && resident_ok;
    replace(
        checks,
        5,
        if mem_ok { Status::Pass } else { Status::Fail },
        format!(
            "RSS {} bytes sampled after the alpha chases and before the v0.1.5 scene rows (cap {RSS_BUDGET_BYTES}), textures {} / {} bytes, uncompressed {}, BC7 {}, {}",
            gpu.rss_bytes,
            gpu.resident_bytes,
            gpu.budget_bytes,
            gpu.uncompressed,
            gpu.bc7,
            memory_policy()
        ),
        if mem_ok {
            "content textures are resident inside the tier budget, RSS is within 3 GiB and the device has BC"
                .to_owned()
        } else if !resident_ok {
            "no content textures were resident".to_owned()
        } else {
            "texture budget, the 3 GiB RSS cap or BC support missed".to_owned()
        },
    );

    let nasa_dark_ev = 0.8 + lookdev::look_r2::NASA_DARK_EV;
    let exposure_ok = (gpu.mars_exposure_ev - nasa_dark_ev).abs() < 1e-3;
    let look_ok = gpu.checklist_errors.is_empty()
        && gpu.checklist_diffs == 0
        && gpu.frame_problems.is_empty()
        && gpu.shared_draw
        && exposure_ok;
    let look_diffs = if gpu.checklist_lines.is_empty() {
        String::new()
    } else {
        format!("; {}", gpu.checklist_lines.join(" | "))
    };
    replace(
        checks,
        6,
        if look_ok { Status::Pass } else { Status::Fail },
        format!(
            "exposure {:+.2} EV (nasa_dark {nasa_dark_ev:+.2}), shared draw {}, {} checklist errors, {} differences, frame problems [{}]; {}{look_diffs}",
            gpu.mars_exposure_ev,
            gpu.shared_draw,
            gpu.checklist_errors.len(),
            gpu.checklist_diffs,
            gpu.frame_problems.join(", "),
            gpu.level_notes
        ),
        if look_ok {
            "nasa_dark is applied, the shared terrain draw is installed and the host checklist matches"
                .to_owned()
        } else {
            format!(
                "checklist diffs {}, errors [{}], problems [{}], shared {}, exposure {:+.2} EV",
                gpu.checklist_diffs,
                gpu.checklist_errors.join(", "),
                gpu.frame_problems.join(", "),
                gpu.shared_draw,
                gpu.mars_exposure_ev
            )
        },
    );

    if let Some(row) = checks.iter_mut().find(|c| c.id == 7) {
        row.evidence = format!(
            "{}; soak rocks {:.0}, mouth rocks {:.0}, mouth dark {:.3} mid {:.3} mean {:.1} stddev {:.1}, {} {}",
            row.evidence,
            gpu.scatter_rocks,
            gpu.mouth_rocks,
            gpu.mouth_colour.dark,
            gpu.mouth_colour.mid,
            gpu.mouth_colour.mean,
            gpu.mouth_colour.stddev,
            gpu.screenshot,
            gpu.mouth_screenshot
        );
        if gpu.scatter_rocks > 0.0 && gpu.mouth_ok && gpu.colour_ok {
            row.reason = "placed scatter drew and the tunnel plate shows the mouth cuts".to_owned();
        } else {
            row.status = Status::Fail;
            row.reason =
                "the soak must draw scatter and the mouth plate must show a dark opening in a lit frame"
                    .to_owned();
        }
    }

    replace(
        checks,
        12,
        if gpu.hud_ok { Status::Pass } else { Status::Fail },
        format!("{}; format {:?}", gpu.hud_note, regia_ui::OffscreenTarget::FORMAT),
        if gpu.hud_ok {
            "a coloured quad was drawn into the offscreen HUD target and read back".to_owned()
        } else {
            "the offscreen HUD readback had no real colour".to_owned()
        },
    );

    let quit_ok = gpu.validation.is_empty();
    let validation = if quit_ok {
        "validation scope empty".to_owned()
    } else {
        gpu.validation.join(" | ")
    };
    replace(
        checks,
        17,
        if quit_ok { Status::Pass } else { Status::Fail },
        format!("{}; {validation}", gpu.quit_note),
        if quit_ok {
            "the warm queue shut down and the device dropped with an empty validation scope".to_owned()
        } else {
            "Vulkan validation errors on shutdown".to_owned()
        },
    );

    apply_scene_rows(checks, gpu);
}

/// Rows 19–21 only. `--scenes` uses this and leaves the soak rows untouched.
pub(crate) fn apply_scenes(checks: &mut [Check], gpu: &GpuFacts) {
    apply_scene_rows(checks, gpu);
}

fn apply_scene_rows(checks: &mut [Check], gpu: &GpuFacts) {
    replace(
        checks,
        19,
        if gpu.ultrawide_ok {
            Status::Pass
        } else {
            Status::Fail
        },
        gpu.ultrawide_note.clone(),
        if gpu.ultrawide_ok {
            "the 3840x1600 frame has real color under the existing smoke color check".to_owned()
        } else {
            "the 3840x1600 frame missed the existing smoke color check".to_owned()
        },
    );
    replace(
        checks,
        20,
        if gpu.black_sky_ok {
            Status::Pass
        } else {
            Status::Fail
        },
        gpu.black_sky_evidence.clone(),
        gpu.black_sky_reason.clone(),
    );
    let units_status = many_units_smoke_status(gpu.units_measured);
    let units_reason = if gpu.units_measured {
        format!(
            "report only; the cargo test gates the encode slack. {}",
            gpu.units_reason
        )
    } else {
        gpu.units_reason.clone()
    };
    replace(checks, 21, units_status, gpu.units_evidence.clone(), units_reason);
}

/// Row 21 when the GPU session ran.
///
/// A finite measurement is [`Status::Report`]: encode time moves with machine load,
/// and the 8 ms slack is calibrated on lavapipe, so smoke does not fail the release
/// on it. `moving_units_cpu` still asserts the slack. A missing or non-finite run
/// fails, because that is not a load spike.
#[must_use]
pub(crate) fn many_units_smoke_status(measured: bool) -> Status {
    if measured { Status::Report } else { Status::Fail }
}

/// Marks rows 19–21 failed when the scene session could not open a device.
/// The soak rows stay as [`engine_checks`] left them.
pub(crate) fn fail_scenes(checks: &mut [Check], error: &str) {
    let reason = format!("scene rows failed: {error}");
    for id in [19, 20, 21] {
        replace(checks, id, Status::Fail, error.to_owned(), reason.clone());
    }
}

/// Marks the device-backed rows failed when the session could not run.
pub(crate) fn fail_gpu(checks: &mut [Check], error: &str) {
    let reason = format!("GPU session failed: {error}");
    for id in [1, 3, 4, 5, 6, 12, 17, 19, 20, 21] {
        replace(checks, id, Status::Fail, error.to_owned(), reason.clone());
    }
    if let Some(row) = checks.iter_mut().find(|c| c.id == 2) {
        row.status = Status::Fail;
        row.evidence = format!("{}; {error}", row.evidence);
        row.reason = "the warm queue did not run".to_owned();
    }
    if let Some(row) = checks.iter_mut().find(|c| c.id == 7) {
        row.status = Status::Fail;
        row.evidence = format!("{}; {error}", row.evidence);
        row.reason = "scatter render did not run".to_owned();
    }
}

fn replace(checks: &mut [Check], id: u8, status: Status, evidence: String, reason: String) {
    if let Some(row) = checks.iter_mut().find(|c| c.id == id) {
        row.status = status;
        row.evidence = evidence;
        row.reason = reason;
    }
}

fn pass(id: u8, area: &str, evidence: impl Into<String>, reason: impl Into<String>) -> Check {
    row(id, area, Status::Pass, evidence, reason)
}

fn fail(id: u8, area: &str, evidence: impl Into<String>, reason: impl Into<String>) -> Check {
    row(id, area, Status::Fail, evidence, reason)
}

fn na(id: u8, area: &str, evidence: impl Into<String>, reason: impl Into<String>) -> Check {
    row(id, area, Status::Na, evidence, reason)
}

fn row(id: u8, area: &str, status: Status, evidence: impl Into<String>, reason: impl Into<String>) -> Check {
    Check {
        id,
        area: area.to_owned(),
        status,
        evidence: evidence.into(),
        reason: reason.into(),
    }
}

fn memory_policy() -> String {
    let budget = MemoryBudget::for_tier(BudgetTier::DesktopHigh);
    format!(
        "Desktop High policy RAM {} bytes, VRAM {} bytes, resident cap {}",
        budget.ram,
        budget.vram,
        resident_cap()
    )
}

fn lobby() -> Check {
    match lobby_stream() {
        Ok(evidence) => pass(
            2,
            "Lobby",
            evidence,
            "PakStreamer delivered the pak bytes and the load set reached 100% when the consumer marked them resident",
        ),
        Err(err) => fail(2, "Lobby", err.clone(), err),
    }
}

/// Writes a one-asset uncompressed pak, streams it, and deletes the file.
fn lobby_stream() -> Result<String, String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-smoke-pak-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|err| format!("creating {}: {err}", dir.display()))?;
    let path = dir.join("lobby.fpak");
    let result = stream_lobby_pak(&path);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn stream_lobby_pak(path: &Path) -> Result<String, String> {
    let id = AssetId::new_random();
    let payload = b"lobby-mesh".to_vec();
    write_uncompressed_pak(path, id, &payload)?;
    let reader = PakReader::open(path).map_err(|err| format!("opening {}: {err}", path.display()))?;
    let sets = LoadSets::default();
    let bytes = payload.len() as u64;
    sets.define(LoadSet::new("lobby").with(LoadKind::Mesh, "map", bytes, bytes));
    let before = sets.load_progress("lobby").ok_or("lobby load set missing")?;
    if before.is_resident() {
        return Err("the lobby set was resident before the streamer ran".to_owned());
    }
    let streamer = PakStreamer::new(vec![Arc::new(reader)], Some(sets.clone()))
        .map_err(|err| format!("starting PakStreamer: {err}"))?;
    streamer.request([StreamRequest {
        id,
        kind: LoadKind::Mesh,
        key: "map".to_owned(),
    }]);
    let asset = streamer
        .next_timeout(Duration::from_secs(2))
        .ok_or("PakStreamer delivered nothing in 2 s")?;
    if asset.bytes != payload {
        return Err(format!(
            "streamed {} bytes, want {}",
            asset.bytes.len(),
            payload.len()
        ));
    }
    let progress = streamer.progress();
    if progress.delivered == 0 || progress.failed != 0 || progress.bytes != bytes {
        return Err(format!(
            "streamer delivered {} failed {} bytes {}",
            progress.delivered, progress.failed, progress.bytes
        ));
    }
    // Residency is the consumer's mark, not the reader's.
    sets.mark_resident(LoadKind::Mesh, "map");
    let after = sets
        .load_progress("lobby")
        .ok_or("lobby load set missing after the stream")?;
    if !after.is_resident() || (after.fraction - 1.0).abs() > 1e-5 {
        return Err(format!("load fraction {:.3} after mark_resident", after.fraction));
    }
    let budget = MemoryBudget::for_tier(BudgetTier::DesktopHigh);
    // `admit` refuses a side it cannot measure (render10 correctness audit), so the default
    // `MemoryUse` (nothing measured) never fits. The no-GPU smoke opens no device, so its GPU use
    // is a measured 0; its RAM is this process's resident set (ADR-5667).
    let Some(ram) = regia_render::process_rss_bytes() else {
        return Ok(format!(
            "PakStreamer delivered {bytes} bytes from {}, fraction {:.2}; admit not measured: this OS reports no resident set",
            path.display(),
            after.fraction
        ));
    };
    let current = MemoryUse {
        vram_measured: true,
        ram,
        ..MemoryUse::default()
    };
    let admission = admit(&budget, &current, &sets, "lobby");
    if !admission.fits {
        return Err(format!(
            "the lobby set does not fit Desktop High: {}",
            admission.reason.unwrap_or_default()
        ));
    }
    Ok(format!(
        "PakStreamer delivered {bytes} bytes from {}, fraction {:.2}, admit {}",
        path.display(),
        after.fraction,
        admission.fits
    ))
}

fn write_uncompressed_pak(path: &Path, id: AssetId, payload: &[u8]) -> Result<(), String> {
    let mut data = vec![0u8; 60];
    let align = FPAK_BLOB_ALIGNMENT as usize;
    let rem = data.len() % align;
    if rem != 0 {
        data.resize(data.len() + align - rem, 0);
    }
    let offset = data.len() as u64;
    data.extend_from_slice(payload);
    let toc = PakToc {
        entries: vec![PakEntry {
            id,
            type_tag: regia_core::id::TypePathHash(0),
            offset,
            size: payload.len() as u64,
            compressed_size: payload.len() as u64,
            block_table_offset: 0,
        }],
        manifest: Vec::new(),
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).map_err(|err| format!("encoding the pak TOC: {err}"))?;
    let toc_offset = data.len() as u64;
    let toc_len = toc_bytes.len() as u64;
    data.extend_from_slice(&toc_bytes);
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_len, &[0; 32]);
    data[..60].copy_from_slice(&header);
    std::fs::write(path, &data).map_err(|err| format!("writing {}: {err}", path.display()))
}

fn look() -> Check {
    match look_inner() {
        Ok(evidence) => pass(
            6,
            "Look",
            evidence,
            "Desktop High is MSAA 1 with contact shadows; nasa_dark is the NASA grade minus 0.35 EV. The host checklist runs on the DO27 frames",
        ),
        Err(err) => fail(6, "Look", err.clone(), err),
    }
}

fn look_inner() -> Result<String, String> {
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    if quality.msaa_samples != 1
        || !quality.contact_shadows
        || !quality.physical_atmosphere
        || !quality.gpu_driven
    {
        return Err(format!(
            "Desktop High quality msaa {} contact {} atmosphere {} gpu_driven {}",
            quality.msaa_samples, quality.contact_shadows, quality.physical_atmosphere, quality.gpu_driven
        ));
    }
    let post = PostSettings::default();
    if post.tonemap != TonemapOperator::AgX || !post.auto_exposure || !post.bloom {
        return Err("default post is not AgX with auto exposure and bloom".to_owned());
    }
    if FogSettings::default().enabled {
        return Err("forward fog defaults on; the planet sky owns aerial perspective".to_owned());
    }
    let mars = AtmosphereParameters::mars();
    if mars.sun_illuminance < 1_000.0 {
        return Err(format!(
            "Mars sun illuminance {} lux is under 1000",
            mars.sun_illuminance
        ));
    }
    let jupiter = SkyBody::jupiter_from_europa();
    if jupiter.angular_diameter_deg <= 10.0 || jupiter.bands <= 0.0 {
        return Err(format!(
            "Jupiter disc {} deg bands {}",
            jupiter.angular_diameter_deg, jupiter.bands
        ));
    }
    let mut nasa = LevelLook::default();
    lookdev::look_r2::switchbacks_nasa_grade(&mut nasa);
    if (nasa.exposure_bias_ev - 0.8).abs() > 1e-4 {
        return Err(format!(
            "switchbacks_nasa_grade exposure is {:+.3} EV, want +0.8",
            nasa.exposure_bias_ev
        ));
    }
    let dark = lookdev::look_r2::NASA_DARK_EV;
    if (dark - (-0.35)).abs() > 1e-6 {
        return Err(format!("NASA_DARK_EV is {dark}, want -0.35"));
    }
    nasa.exposure_bias_ev += dark;
    let nasa_dark = 0.8 + dark;
    if (nasa.exposure_bias_ev - nasa_dark).abs() > 1e-4 {
        return Err(format!(
            "nasa_dark exposure is {:+.3} EV, want {nasa_dark:+.3}",
            nasa.exposure_bias_ev
        ));
    }
    round_trip_look(&nasa)?;
    let europa = LevelLook {
        min_ev100: Some(11.5),
        max_ev100: Some(11.7),
        ..LevelLook::default()
    };
    round_trip_look(&europa)?;
    let _bounce = GroundBounce::default();
    let profile = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
    let gi = match GiLook::of_profile(&profile) {
        GiLook::Balanced => "balanced",
        GiLook::Depth => "depth",
        GiLook::Baked => "baked",
    };
    Ok(format!(
        "msaa {}, Mars sun {:.0} lux, Jupiter {:.0} deg, AgX, GiLook {gi} (depth probes {}), nasa_dark {nasa_dark:+.2} EV",
        quality.msaa_samples,
        mars.sun_illuminance,
        jupiter.angular_diameter_deg,
        GiLook::Depth.runs_probes()
    ))
}

fn round_trip_look(look: &LevelLook) -> Result<(), String> {
    let text = ron::to_string(look).map_err(|e| e.to_string())?;
    let back: LevelLook = ron::from_str(&text).map_err(|e| e.to_string())?;
    if back != *look {
        return Err("LevelLook did not round-trip through RON".to_owned());
    }
    Ok(())
}

fn terrain(workspace: &Path, content: Option<&Path>) -> Check {
    match terrain_inner(workspace, content) {
        Ok(evidence) => pass(
            7,
            "Terrain",
            evidence,
            "four mouth trenches cut height samples and the stand-in scatter places; the rock is drawn in the GPU session",
        ),
        Err(err) => fail(7, "Terrain", err.clone(), err),
    }
}

fn terrain_inner(workspace: &Path, content: Option<&Path>) -> Result<String, String> {
    let shapes = vec![
        HoleShape::segment([2.0, 4.0], [10.0, 4.0], 2.0),
        HoleShape::segment([2.0, 12.0], [10.0, 12.0], 2.0),
        HoleShape::segment([16.0, 2.0], [16.0, 10.0], 2.0),
        HoleShape::segment([24.0, 16.0], [32.0, 24.0], 2.0),
    ];
    let mask = HolesMask::from_shapes(shapes).map_err(|e| e.to_string())?;
    if mask.all_shapes().count() != 4 {
        return Err(format!("built {} mouth shapes", mask.all_shapes().count()));
    }
    for (x, z) in [(6.0, 4.0), (6.0, 12.0), (16.0, 6.0), (28.0, 20.0)] {
        if !mask.contains(x, z) {
            return Err(format!("mouth centre ({x}, {z}) is not inside a hole"));
        }
    }
    let n = 9usize;
    let mut heights = vec![1.0_f32; n * n];
    let cut = mask.cut_heights(&mut heights, 0.0, |i| {
        [(i % n) as f32 * 4.0, (i / n) as f32 * 4.0]
    });
    if cut == 0 || !heights.iter().any(|h| h.is_nan()) {
        return Err("cut_heights left every sample finite".to_owned());
    }

    let stand_in = workspace.join("samples/lookdev/scatter/mars.scatter.ron");
    let text = std::fs::read_to_string(&stand_in).map_err(|e| format!("{}: {e}", stand_in.display()))?;
    let set = ScatterSet::from_ron(&text, None).map_err(|e| e.to_string())?;
    let map = TerrainMap::from_heights(8, 8, vec![0.0; 64], 16.0, 1.0, 0.0).map_err(|e| e.to_string())?;
    let placed = place_set(
        &set,
        PlacementInputs {
            map: &map,
            holes: Some(&mask),
            layers: None,
            density_maps: None,
        },
    )
    .map_err(|e| e.to_string())?;
    let rock = procedural_rock(1, RockStyle::angular([0.26, 0.14, 0.075]));
    let tris = rock.lods.first().map(|lod| lod.indices.len() / 3).unwrap_or(0);
    if tris == 0 {
        return Err("procedural rock LOD0 has no triangles".to_owned());
    }
    let mut evidence = format!(
        "4 mouths, {cut} samples cut to NaN, stand-in {} classes placed {}, rock LOD0 {tris} tris",
        set.classes.len(),
        placed.len()
    );
    if let Some(dir) = content {
        evidence.push_str("; ");
        evidence.push_str(&content_terrain(dir)?);
    }
    Ok(evidence)
}

fn content_terrain(content: &Path) -> Result<String, String> {
    let tunnels = content.join("maps/switchbacks/tunnels.json");
    let mouths = if tunnels.is_file() {
        let text = std::fs::read_to_string(&tunnels).map_err(|e| format!("{}: {e}", tunnels.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", tunnels.display()))?;
        let openings = json["layout"]["openings"]
            .as_array()
            .ok_or_else(|| format!("{} has no layout.openings array", tunnels.display()))?;
        if openings.len() < 4 {
            return Err(format!(
                "switchbacks has {} tunnel openings (want at least 4)",
                openings.len()
            ));
        }
        format!("{} openings in {}", openings.len(), tunnels.display())
    } else {
        format!("no {}", tunnels.display())
    };
    let scatter = content.join("maps/switchbacks/scatter.ron");
    let classes = if scatter.is_file() {
        let text = std::fs::read_to_string(&scatter).map_err(|e| format!("{}: {e}", scatter.display()))?;
        let set = ScatterSet::from_ron(&text, None).map_err(|e| format!("{}: {e}", scatter.display()))?;
        format!(", content scatter {} classes", set.classes.len())
    } else {
        String::new()
    };
    Ok(format!("{mouths}{classes}"))
}

fn combat() -> Check {
    match combat_inner() {
        Ok(evidence) => pass(
            8,
            "Combat",
            evidence,
            "every DO27 weapon resolves a muzzle, tracer or hit in Mars and airless air, plus hover washes and thruster plumes",
        ),
        Err(err) => fail(8, "Combat", err.clone(), err),
    }
}

fn combat_inner() -> Result<String, String> {
    let media = [VfxMedium::mars(), VfxMedium::airless(EUROPA_GRAVITY)];
    for weapon in DO27_WEAPONS {
        for medium in &media {
            if let Some(name) = weapon.muzzle {
                expect_effect(name, medium)?;
            }
            if let Some(name) = weapon.tracer {
                expect_effect(name, medium)?;
            }
            match weapon.hit {
                HitKind::Bullet(_) | HitKind::Beam => {
                    for surface in ["hit_metal", "hit_concrete", "hit_glass"] {
                        expect_effect(surface, medium)?;
                    }
                }
                HitKind::Explosive(_) => {
                    let boom = explosion_in(medium);
                    if boom.emitters.is_empty() {
                        return Err(format!("{} explosion has no emitters", weapon.weapon));
                    }
                }
            }
        }
    }
    for surface in HoverSurface::ALL {
        for medium in &media {
            let wash = hover_wash_in(surface, medium);
            if wash.emitters.is_empty() {
                return Err(format!(
                    "hover {} / {:?} has no emitters",
                    surface.name(),
                    medium.atmosphere
                ));
            }
        }
    }
    for kind in PlumeKind::ALL {
        for medium in &media {
            let plume = plume_in(kind, medium);
            if plume.emitters.is_empty() {
                return Err(format!("plume {} has no emitters", kind.name()));
            }
        }
    }
    Ok(format!(
        "{} weapons, {} hover surfaces, {} plume kinds, Mars and airless",
        DO27_WEAPONS.len(),
        HoverSurface::ALL.len(),
        PlumeKind::ALL.len()
    ))
}

fn expect_effect(base: &str, medium: &VfxMedium) -> Result<(), String> {
    let def = combat_effect_in(base, medium).ok_or_else(|| format!("no combat effect {base}"))?;
    if def.emitters.is_empty() {
        return Err(format!("{base} has no emitters"));
    }
    let renamed = def.name.contains('@');
    if base.starts_with("tracer") {
        if renamed {
            return Err(format!("tracer {base} was renamed to {}", def.name));
        }
    } else if !renamed {
        return Err(format!("{base} has no medium variant name ({})", def.name));
    }
    Ok(())
}

fn explosions() -> Check {
    match explosions_inner() {
        Ok(evidence) => pass(
            9,
            "Explosions",
            evidence,
            "airless is a short fireball plus ballistic debris; Mars keeps a fast fireball and a lingering dust billow",
        ),
        Err(err) => fail(9, "Explosions", err.clone(), err),
    }
}

fn explosions_inner() -> Result<String, String> {
    let airless = explosion_in(&VfxMedium::airless(EUROPA_GRAVITY));
    if has_emitter(&airless, "smoke_puff") || has_emitter(&airless, "heat_haze") {
        return Err("airless explosion still has smoke_puff or heat_haze".to_owned());
    }
    let air_fire = emitter(&airless, "fireball").ok_or("airless fireball missing")?;
    if air_fire.spawn.lifetime.max >= 0.5 {
        return Err(format!(
            "airless fireball lifetime {}",
            air_fire.spawn.lifetime.max
        ));
    }
    let debris = emitter(&airless, "debris").ok_or("airless debris missing")?;
    if debris.update.drag != 0.0 || debris.spawn.lifetime.max < 8.0 {
        return Err(format!(
            "airless debris drag {} lifetime {}",
            debris.update.drag, debris.spawn.lifetime.max
        ));
    }
    let mars = explosion_in(&VfxMedium::mars());
    let mars_fire = emitter(&mars, "fireball").ok_or("mars fireball missing")?;
    if mars_fire.spawn.lifetime.max >= 1.0 {
        return Err(format!("mars fireball lifetime {}", mars_fire.spawn.lifetime.max));
    }
    let dust = emitter(&mars, "dust_billow").ok_or("mars dust_billow missing")?;
    if dust.spawn.lifetime.max < 8.0 {
        return Err(format!("mars dust lifetime {}", dust.spawn.lifetime.max));
    }
    let soot = emitter(&mars, "smoke_puff").ok_or("mars smoke_puff missing")?;
    Ok(format!(
        "airless fireball {:.2} s, debris {:.0} s; mars fireball {:.2} s, dust {:.0} s, soot {:.1} s",
        air_fire.spawn.lifetime.max,
        debris.spawn.lifetime.max,
        mars_fire.spawn.lifetime.max,
        dust.spawn.lifetime.max,
        soot.spawn.lifetime.max
    ))
}

fn has_emitter(def: &VfxSystemDef, name: &str) -> bool {
    def.emitters.iter().any(|e| e.name == name)
}

fn emitter<'a>(def: &'a VfxSystemDef, name: &str) -> Option<&'a regia_vfx::format::EmitterDef> {
    def.emitters.iter().find(|e| e.name == name)
}

fn glass() -> Check {
    match glass_inner() {
        Ok(evidence) => pass(
            10,
            "Glass",
            evidence,
            "viewer-inside glass, laminate and pressure-pane damage, and the three breach presets resolve in the VFX library",
        ),
        Err(err) => fail(10, "Glass", err.clone(), err),
    }
}

fn glass_inner() -> Result<String, String> {
    let view = GlassView::viewer_inside();
    let mut laminate = GlassDamage::new("laminate");
    laminate.impact(Vec3::new(0.0, 0.2, 0.0), 1.0);
    let mut pane = GlassDamage::new("pressure_pane");
    let index = pane.impact_at_level(Vec3::new(0.1, 0.0, 0.2), 4.0, 3, 0.35);
    let level = pane.impacts.get(index).map(|hit| hit.level);
    let mars_kpa = BreachAtmosphere::from_preset(AtmospherePreset::Mars).outside_kpa();
    let airless_kpa = BreachAtmosphere::Airless.outside_kpa();
    if view.mode != GlassViewMode::ViewerInside
        || (view.inside_specular - INSIDE_SPECULAR_DEFAULT).abs() > 1e-6
        || laminate.set != "laminate"
        || laminate.impacts.is_empty()
        || pane.set != "pressure_pane"
        || level != Some(3)
        || airless_kpa != 0.0
        || mars_kpa <= 0.0
    {
        return Err(format!(
            "GlassView mode {:?} specular {:.2}, laminate {}, pressure_pane level {:?}, Mars {mars_kpa} kPa, airless {airless_kpa} kPa",
            view.mode,
            view.inside_specular,
            laminate.impacts.len(),
            level
        ));
    }
    let settings = GlassEffectSettings::default();
    let mut library = VfxLibrary::new();
    for preset in [breach_earth(), breach_mars(), breach_airless()] {
        library
            .add(preset)
            .map_err(|err| format!("breach preset rejected: {err}"))?;
    }
    let mut resolved = Vec::new();
    for atmosphere in [
        BreachAtmosphere::Earth,
        BreachAtmosphere::Mars,
        BreachAtmosphere::Airless,
    ] {
        let name = settings
            .breach_effect(atmosphere)
            .ok_or_else(|| format!("no breach effect for {atmosphere:?}"))?;
        let index = library
            .index(name)
            .ok_or_else(|| format!("breach preset {name} is not in the library"))?;
        let def = library.def(index);
        if def.emitters.is_empty() {
            return Err(format!("{name} has no emitters"));
        }
        resolved.push(format!("{name} {}", def.emitters.len()));
    }
    Ok(format!(
        "GlassView inside specular {:.2}, laminate impacts {}, pressure_pane level {:?}, Mars {mars_kpa} kPa, airless {airless_kpa} kPa, breaches {}",
        view.inside_specular,
        laminate.impacts.len(),
        level,
        resolved.join(", ")
    ))
}

fn audio() -> Check {
    let mars = AudioMedium::from_preset(AtmospherePreset::Mars);
    let airless = AudioMedium::from_preset(AtmospherePreset::Airless);
    let radio = AudioPropagation::for_category(EmitterCategory::Radio);
    let ui = AudioPropagation::for_category(EmitterCategory::Ui);
    let cockpit = AudioPropagation::for_category(EmitterCategory::Cockpit);
    let body = AudioPropagation::for_category(EmitterCategory::Body);
    let (azimuth, _) = ListenerFrame::default().direction_of(Vec3::X);
    let ok = mars.airborne
        && (mars.speed_of_sound - MARS_SPEED_OF_SOUND).abs() < 1e-3
        && (mars.level_offset_db - MARS_LEVEL_OFFSET_DB).abs() < 1e-3
        && (mars.absorption_db_per_m_khz2 - MARS_ABSORPTION_DB_PER_M_KHZ2).abs() < 1e-6
        && mars.ground == "mars_regolith"
        && !airless.airborne
        && airless.ground == "ice"
        && GroundMaterial::named("mars_regolith").is_some()
        && GroundMaterial::named("ice").is_some()
        && GroundMaterial::named("airless_rock").is_some()
        && radio.class == PathClass::Bypass
        && ui.class == PathClass::Bypass
        && cockpit.class == PathClass::World
        && cockpit.ground_coupling == 0.0
        && body.class == PathClass::Body
        && azimuth > 0.0;
    let evidence = format!(
        "Mars c {:.0} m/s {:+.0} dB k {}, ground {}; airless ground {}; +X azimuth {:.2}",
        mars.speed_of_sound,
        mars.level_offset_db,
        mars.absorption_db_per_m_khz2,
        mars.ground,
        airless.ground,
        azimuth
    );
    if ok {
        pass(
            11,
            "Audio",
            evidence,
            "Mars and airless media, ground presets, bypass paths and a right-positive azimuth are present",
        )
    } else {
        fail(
            11,
            "Audio",
            evidence,
            "a medium, path class or azimuth check missed",
        )
    }
}

fn hud_format() -> Check {
    let format = regia_ui::OffscreenTarget::FORMAT;
    if format == wgpu::TextureFormat::Rgba8UnormSrgb {
        pass(
            12,
            "HUD",
            format!("OffscreenTarget format {format:?}"),
            "render-to-texture is Rgba8UnormSrgb; constructing the target needs the GPU session, and a holo material is game content",
        )
    } else {
        fail(
            12,
            "HUD",
            format!("{format:?}"),
            "offscreen HUD format is not Rgba8UnormSrgb",
        )
    }
}

fn pilots() -> Check {
    match pilots_inner() {
        Ok(evidence) => pass(
            13,
            "Pilots",
            evidence,
            "two-bone IK, retarget through a shared role, a limp ragdoll step and a physics pose blend ran",
        ),
        Err(err) => fail(13, "Pilots", err.clone(), err),
    }
}

fn pilots_inner() -> Result<String, String> {
    let skeleton = arm("root")?;
    let mut pose = skeleton.bind_pose_pose();
    let before = tip_distance(&skeleton, &pose, Vec3::new(0.5, 0.6, 0.0));
    solve_two_bone(
        &skeleton,
        &mut pose,
        &IkChain {
            root: 0,
            mid: 1,
            tip: 2,
        },
        Vec3::new(0.5, 0.6, 0.0),
        Some(Vec3::X),
        1.0,
    );
    let after = tip_distance(&skeleton, &pose, Vec3::new(0.5, 0.6, 0.0));
    if after >= before {
        return Err(format!("two-bone IK did not move the tip ({before} -> {after})"));
    }
    let source = arm("root")?;
    let target = arm("root")?;
    let profile = RetargetProfile::default().with("root", "root");
    let retarget = Retargeter::new(&source, &profile, &target, &profile).map_err(|e| e.to_string())?;
    let mut out = target.bind_pose_pose();
    retarget.retarget_pose(&source.bind_pose_pose(), &mut out);
    if retarget.mapped_joints() == 0 {
        return Err("retarget mapped no joints".to_owned());
    }
    let fresh = Ragdoll::new(RagdollProfile {
        bones: vec![RagdollBone {
            joint: "root".into(),
            radius: 0.1,
            mass: 8.0,
            length: Some(0.4),
            limits: None,
            shape: RagdollShape::Capsule,
        }],
        ..RagdollProfile::default()
    });
    if fresh.mode != RagdollMode::Kinematic {
        return Err("a new ragdoll is not kinematic".to_owned());
    }
    let stepped = step_limp_ragdoll()?;
    let mut blend = PhysicsPoseBlend::new(skeleton.len());
    blend.joints[0] = Some((Vec3::new(0.2, 0.4, 0.0), Quat::IDENTITY));
    blend.weights[0] = 1.0;
    let mut blended = skeleton.bind_pose_pose();
    let start = blended.translations[0];
    blend_physics_pose(&skeleton, &mut blended, &blend);
    if blended.translations[0] == start {
        return Err("physics pose blend left the root translation unchanged".to_owned());
    }
    Ok(format!(
        "IK tip distance {before:.3} -> {after:.3}, retarget mapped {}, {stepped}",
        retarget.mapped_joints()
    ))
}

/// Builds the humanoid ragdoll test scene and steps it until the hips fall.
fn step_limp_ragdoll() -> Result<String, String> {
    const DT: f64 = 1.0 / 62.5;
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(PhysicsPlugin)
        .add_plugins((AnimationPlugin, RagdollPlugin));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(20.0, 0.5, 20.0),
            },
            ..Collider::default()
        },
    ));
    let skeleton = Arc::new(humanoid_skeleton()?);
    let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
    if ragdoll.profile.bones.len() != 11 {
        return Err(format!(
            "humanoid profile has {} bones",
            ragdoll.profile.bones.len()
        ));
    }
    ragdoll.mode = RagdollMode::Limp;
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("idle", 1.0), true),
            ragdoll,
        ))
        .id();
    let hips = |app: &App| -> Option<f32> {
        let entity = {
            let bodies = app.world().get::<RagdollBodies>(rig)?;
            bodies.bodies.first()?.entity
        };
        app.world()
            .get::<PhysicsPose>(entity)
            .map(|pose| pose.translation.y)
    };
    let mut stood = None;
    for _ in 0..4 {
        app.update();
        if let Some(y) = hips(&app) {
            stood = Some(y);
            break;
        }
    }
    let stood = stood.ok_or("the limp ragdoll was not built")?;
    for _ in 0..48 {
        app.update();
    }
    let fallen = hips(&app).ok_or("the limp ragdoll lost its hips")?;
    let mode = app.world().get::<Ragdoll>(rig).map(|body| body.mode);
    if mode != Some(RagdollMode::Limp) {
        return Err(format!("ragdoll mode after the step is {mode:?}"));
    }
    if fallen >= stood - 0.15 {
        return Err(format!("hips did not fall ({stood:.3} -> {fallen:.3})"));
    }
    Ok(format!("limp hips y {stood:.3} -> {fallen:.3}"))
}

fn humanoid_skeleton() -> Result<Skeleton, String> {
    let names = [
        "hips",
        "spine",
        "chest",
        "neck",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "upperarm_r",
        "lowerarm_r",
        "hand_r",
        "thigh_l",
        "calf_l",
        "foot_l",
        "thigh_r",
        "calf_r",
        "foot_r",
    ];
    let parents = vec![
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(3),
        Some(2),
        Some(5),
        Some(6),
        Some(2),
        Some(8),
        Some(9),
        Some(0),
        Some(11),
        Some(12),
        Some(0),
        Some(14),
        Some(15),
    ];
    let t = |x: f32, y: f32| translated(Vec3::new(x, y, 0.0));
    Skeleton::from_local_bind(
        names.iter().map(|name| (*name).to_owned()).collect(),
        parents,
        vec![
            t(0.0, 1.0),
            t(0.0, 0.15),
            t(0.0, 0.25),
            t(0.0, 0.25),
            t(0.0, 0.1),
            t(0.2, 0.2),
            t(0.3, 0.0),
            t(0.27, 0.0),
            t(-0.2, 0.2),
            t(-0.3, 0.0),
            t(-0.27, 0.0),
            t(0.1, -0.05),
            t(0.0, -0.45),
            t(0.0, -0.42),
            t(-0.1, -0.05),
            t(0.0, -0.45),
            t(0.0, -0.42),
        ],
    )
    .map_err(|err| err.to_string())
}

fn arm(root: &str) -> Result<Skeleton, String> {
    Skeleton::from_local_bind(
        vec![root.to_owned(), "mid".to_owned(), "tip".to_owned()],
        vec![None, Some(0), Some(1)],
        vec![
            translated(Vec3::new(0.0, 0.2, 0.0)),
            translated(Vec3::new(0.0, 0.5, 0.0)),
            translated(Vec3::new(0.0, 0.5, 0.0)),
        ],
    )
    .map_err(|e| e.to_string())
}

fn tip_distance(skeleton: &Skeleton, pose: &regia_animation::Pose, target: Vec3) -> f32 {
    let globals = skeleton.globals(pose, None);
    globals[2].w_axis.truncate().distance(target)
}

fn determinism() -> Check {
    match determinism_inner() {
        Ok(evidence) => pass(
            16,
            "Determinism",
            evidence,
            "the same recorded Move played back twice, and two worlds driven by that velocity ended on the same translation",
        ),
        Err(err) => fail(16, "Determinism", err.clone(), err),
    }
}

fn determinism_inner() -> Result<String, String> {
    let recording = record_move()?;
    let played_a = replay_move(&recording)?;
    let played_b = replay_move(&recording)?;
    if (played_a - played_b).abs() > 1e-6 {
        return Err(format!("replayed Move differs: {played_a} vs {played_b}"));
    }
    let vx = recording
        .frames
        .first()
        .and_then(|frame| frame.first())
        .map(|value| value.x)
        .unwrap_or(0.0);
    if (played_a - vx).abs() > 1e-5 || vx.abs() < 1.0 {
        return Err(format!("replayed Move {played_a} is not the recorded {vx}"));
    }
    let mut left = driven_world(vx);
    let mut right = driven_world(vx);
    step_world(&mut left);
    step_world(&mut right);
    let a = translation(&left);
    let b = translation(&right);
    if a != b {
        return Err(format!("translations differ: {a:?} vs {b:?}"));
    }
    if a.0.abs() < 0.5 {
        return Err(format!(
            "the recorded velocity did not move the body (x = {})",
            a.0
        ));
    }
    if a.1 >= 9.0 {
        return Err(format!("the body did not fall (y = {})", a.1));
    }
    Ok(format!(
        "Move replay {played_a:.2} twice; 60 steps of 1/60 s at vx {vx:.2}, translation ({:.4}, {:.4}, {:.4})",
        a.0, a.1, a.2
    ))
}

fn record_move() -> Result<InputRecording, String> {
    let mut app = input_app();
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .record_with(11, vec!["Move".to_owned()]);
    let frame = app.world().resource::<regia_app::Time>().frame_count() + 1;
    app.world_mut()
        .resource_mut::<InputInjection>()
        .schedule("Move", Vec3::new(4.0, 0.0, 0.0), frame, 1);
    app.update();
    app.world_mut().resource_mut::<InputRecorder>().stop();
    let recording = app.world().resource::<InputRecorder>().recording.clone();
    if recording.frames.len() != 1 {
        return Err(format!("recorded {} steps, want 1", recording.frames.len()));
    }
    Ok(recording)
}

fn replay_move(recording: &InputRecording) -> Result<f32, String> {
    let mut app = input_app();
    app.world_mut()
        .resource_mut::<InputRecorder>()
        .play(recording.clone());
    // The recorded step plays on the first frame, the frame it was recorded on (ADR-537, ADR-867).
    app.update();
    Ok(app.world().resource::<Actions>().value("Move").x)
}

fn input_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((
        InputPlugin::new(InputConfig::new().action("Move", ActionKind::Axis2D)),
        InputRecordingPlugin,
    ));
    enable_fixed_step(&mut app, 60.0);
    app
}

fn driven_world(vx: f32) -> (PhysicsWorld, rapier3d::dynamics::RigidBodyHandle) {
    let mut world = PhysicsWorld::default();
    // A collider-less body has zero mass, so Rapier’s gravity force (`mass * g`) is zero
    // and the translation never changes. The extra kilogram plus a ball matches the
    // debris step, which does fall.
    let body = RigidBodyBuilder::new(RigidBodyType::Dynamic)
        .translation(Vector::new(0.0, 10.0, 0.0))
        .linvel(Vector::new(vx, 0.0, 0.0))
        .additional_mass(1.0)
        .can_sleep(false)
        .build();
    let handle = world.bodies.insert(body);
    let collider = ColliderBuilder::ball(0.25).build();
    let PhysicsWorld {
        ref mut bodies,
        ref mut colliders,
        ..
    } = world;
    colliders.insert_with_parent(collider, handle, bodies);
    (world, handle)
}

fn step_world(world: &mut (PhysicsWorld, rapier3d::dynamics::RigidBodyHandle)) {
    let params = IntegrationParameters {
        dt: 1.0 / 60.0,
        ..IntegrationParameters::default()
    };
    let gravity = Vector::new(0.0, -3.71, 0.0);
    let PhysicsWorld {
        pipeline,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        ..
    } = &mut world.0;
    for _ in 0..60 {
        pipeline.step(
            gravity,
            &params,
            islands,
            broad_phase,
            narrow_phase,
            bodies,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            ccd_solver,
            &(),
            &(),
        );
    }
}

fn translation(world: &(PhysicsWorld, rapier3d::dynamics::RigidBodyHandle)) -> (f32, f32, f32) {
    let t = world.0.bodies[world.1].translation();
    (t.x, t.y, t.z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(std::path::PathBuf::from)
            .expect("xtask parent")
    }

    #[test]
    fn cpu_rows_cover_the_alpha_list() {
        let checks = engine_checks(&workspace(), None);
        assert_eq!(checks.len(), 21);
        assert_eq!(
            checks.iter().map(|c| c.id).collect::<Vec<_>>(),
            (1..=21).collect::<Vec<_>>()
        );
        let status = |id: u8| checks.iter().find(|c| c.id == id).expect("row").status;
        for id in [1, 3, 4, 5, 14, 15, 17, 18, 19, 20, 21] {
            assert_eq!(status(id), Status::Na, "row {id}");
        }
        for id in [2, 6, 7, 8, 9, 10, 11, 12, 13, 16] {
            assert_eq!(
                status(id),
                Status::Pass,
                "row {id} {}",
                checks.iter().find(|c| c.id == id).expect("row").reason
            );
        }
    }

    #[test]
    fn many_units_smoke_row_reports_a_measurement_and_fails_a_missing_one() {
        assert_eq!(many_units_smoke_status(true), Status::Report);
        assert_eq!(many_units_smoke_status(false), Status::Fail);
    }
}
