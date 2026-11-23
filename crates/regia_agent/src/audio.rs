// SPDX-License-Identifier: MPL-2.0
//! Agent methods for sound propagation (`docs/specs/16-audio.md` § 3c): which medium the level
//! has, what the listener touches, and which paths a sound takes to the listener.
//!
//! Read-only. A world without [`regia_audio::Audio`] returns `UNSUPPORTED`.

use regia_app::prelude::*;
use regia_audio::{
    Audio, AudioEmitter, AudioPropagation, EmitterCategory, EmitterFrame, GroundMaterial, LoopGenCache,
    LoopStats, PathFrame, Propagation, SoundAssets,
};
use regia_core::math::Vec3;
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn require_audio(world: &World) -> Result<&Audio, RpcError> {
    world.get_resource::<Audio>().ok_or_else(|| {
        RpcError::new(
            protocol::UNSUPPORTED,
            "F0013",
            "no audio in this app",
            "add regia_audio::AudioPlugin so audio.* has a mixer",
        )
    })
}

fn path_json(name: &str, path: &PathFrame) -> Value {
    json!({
        "path": name,
        "delay_ms": f64::from(path.delay_secs) * 1000.0,
        "gain_db": path.gain_db(),
        "cutoff_hz": path.effective_cutoff_hz(),
        "absorption_cutoff_hz": path.cutoff_hz,
        "coupling_cutoff_hz": path.coupling_cutoff_hz,
        "coda_s": path.coda_secs,
        "felt": path.felt,
        "pan": path.pan,
        "pitch_ratio": path.pitch_ratio,
        "audible": path.audible(),
    })
}

fn propagation_json(p: &Propagation) -> Value {
    let mut paths = Vec::new();
    if let Some(air) = p.airborne.as_ref() {
        paths.push(path_json(if p.bypass { "direct" } else { "airborne" }, air));
    }
    if let Some(ground) = p.ground.as_ref() {
        paths.push(path_json("ground", ground));
    }
    json!({
        "paths": paths,
        "bypass": p.bypass,
        "air": p.air.map(|m| m.name()),
        "ground_material": p.ground_material,
        "coupled": p.coupled,
        "distance": p.distance,
        "audible": p.audible(),
    })
}

/// `audio.medium`: the level's medium, its default ground, the listener's contact and the ground
/// haptics.
pub fn medium(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let audio = require_audio(world)?;
    let medium = audio.medium();
    let ground = GroundMaterial::named_or(&medium.ground, "soil");
    let coupling = audio.listener_coupling();
    let haptics = audio.haptics();
    let voices = audio.path_voices();
    Ok(json!({
        "kind": medium.kind.name(),
        "airborne": medium.carries_sound(),
        "speed_of_sound": medium.speed_of_sound,
        "level_offset_db": medium.level_offset_db,
        "absorption_db_per_m_khz2": medium.absorption_db_per_m_khz2,
        "ground": {
            "name": ground.name,
            "wave_speed_mps": ground.wave_speed_mps,
            "q": ground.q,
            "coda_s": ground.coda_secs(),
        },
        "listener": {
            "contact": coupling.contact.name(),
            "coupling_weight": audio.coupling_weight(),
            "vehicle": coupling.vehicle.map(Entity::to_bits),
            "structure": coupling.structure,
            "ground": coupling.ground,
        },
        "interiors": audio.interiors().len(),
        "haptics": haptics,
        "path_voices": {
            "airborne": voices.iter().filter(|(role, _)| *role == "airborne").count(),
            "ground": voices.iter().filter(|(role, _)| *role == "ground").count(),
        },
    }))
}

fn category_from(name: &str) -> Option<EmitterCategory> {
    use EmitterCategory::*;
    Some(match name {
        "impact" => Impact,
        "explosion" => Explosion,
        "collapse" => Collapse,
        "footstep" => Footstep,
        "landing" => Landing,
        "machinery" => Machinery,
        "hull_landing" => HullLanding,
        "hull_scrape" => HullScrape,
        "hull_hover" => HullHover,
        "planted_weapon" => PlantedWeapon,
        "projectile" => Projectile,
        "radio" => Radio,
        "ui" => Ui,
        "cockpit" => Cockpit,
        "body" => Body,
        _ => return None,
    })
}

fn vec3_param(params: &Value, field: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(value) = params.get(field) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be three numbers")))?
            as f32;
        if !slot.is_finite() {
            return Err(RpcError::invalid_params(format!(
                "`{field}` must be three finite numbers within f32 range"
            )));
        }
    }
    Ok(Some(Vec3::from_array(out)))
}

/// `audio.propagation`: the paths from an emitter entity (or a hypothetical sound at `position`)
/// to the listener: airborne/ground, delay, gain, cutoff, coupling.
pub fn propagation(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_audio(world)?;
    if params.get("entity").is_some() {
        let entity = crate::scene::entity_from(params)?;
        let entity_ref = world.get_entity(entity).map_err(|_| {
            RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "use scene.query_entities",
            )
        })?;
        let emitter = entity_ref.get::<AudioEmitter>().cloned().ok_or_else(|| {
            RpcError::invalid_params(format!("entity {} has no AudioEmitter", entity.to_bits()))
        })?;
        // World position (parented emitters), else the local one.
        let position = entity_ref
            .get::<GlobalTransform>()
            .map(|g| g.0.to_scale_rotation_translation().2)
            .filter(|p| *p != Vec3::ZERO)
            .or_else(|| entity_ref.get::<Transform>().map(|t| t.translation))
            .unwrap_or(Vec3::ZERO);
        let settings = entity_ref.get::<AudioPropagation>().cloned().unwrap_or_default();
        let audio = require_audio(world)?;
        // Prefer what the mixer solved this frame (velocity included); else solve now.
        let solved = audio.emitter_propagation(entity).cloned().unwrap_or_else(|| {
            let bus = emitter.bus.clone().unwrap_or_else(|| "World".into());
            let frame = EmitterFrame {
                position,
                ..EmitterFrame::default()
            };
            audio.solve_propagation(&frame, &emitter, &settings, &bus)
        });
        let mut out = propagation_json(&solved);
        out["entity"] = json!(entity.to_bits());
        return Ok(out);
    }
    let position = vec3_param(params, "position")?
        .ok_or_else(|| RpcError::invalid_params("`entity` or `position` ([x, y, z]) is required"))?;
    let settings = match crate::params::opt_str(params, "category")? {
        Some(name) => AudioPropagation::for_category(
            category_from(name)
                .ok_or_else(|| RpcError::invalid_params(format!("unknown category `{name}`")))?,
        ),
        None => AudioPropagation::default(),
    };
    let max_distance = match crate::params::opt_f64(params, "max_distance")? {
        None => 10_000.0,
        Some(d) if d >= 0.0 && (d as f32).is_finite() => d as f32,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`max_distance` must be finite, nonnegative and within f32 range",
            ));
        }
    };
    let emitter = AudioEmitter {
        max_distance,
        ..AudioEmitter::default()
    };
    let bus = crate::params::opt_str(params, "bus")?.unwrap_or("World");
    let frame = EmitterFrame {
        position,
        ..EmitterFrame::default()
    };
    let audio = require_audio(world)?;
    Ok(propagation_json(
        &audio.solve_propagation(&frame, &emitter, &settings, bus),
    ))
}

fn stats_json(s: &LoopStats) -> Value {
    json!({
        "frames": s.frames,
        "duration_secs": s.duration_secs,
        "rms": s.rms,
        "peak": s.peak,
        "seam_jump": s.seam_jump,
        "max_step": s.max_step,
        "hash": format!("{:016x}", s.hash),
    })
}

/// The loop generators of the world for `audio.list_emitters`: the sounds registered from a
/// `.loopgen.ron` (name, content hash) and the render cache counters.
pub(crate) fn loopgens_json(world: &World) -> Value {
    let Some(sounds) = world.get_resource::<SoundAssets>() else {
        return json!({ "registered": [], "cache": { "entries": 0, "hits": 0, "misses": 0 } });
    };
    let cache = sounds.loopgens();
    let registered: Vec<Value> = sounds
        .loopgen_names()
        .map(|(name, hash)| json!({ "name": name, "content_hash": format!("{hash:016x}") }))
        .collect();
    let loops: Vec<Value> = cache
        .entries()
        .iter()
        .map(|e| {
            let mut v = stats_json(&e.stats);
            v["name"] = json!(e.name);
            v["content_hash"] = json!(format!("{:016x}", e.content_hash));
            v
        })
        .collect();
    json!({
        "registered": registered,
        "loops": loops,
        "cache": { "entries": cache.len(), "hits": cache.hits(), "misses": cache.misses() },
    })
}

/// Reads a project-relative `*.loopgen.ron` text file (`regia_core::project_path` rules).
fn read_project_text(world: &World, path: &str) -> Result<String, RpcError> {
    if !path.to_ascii_lowercase().ends_with(regia_audio::LOOPGEN_SUFFIX) {
        return Err(RpcError::invalid_params(
            "`path` must be a relative `*.loopgen.ron` path inside the project (no `..`)",
        ));
    }
    let file = project_file(world, path)?;
    std::fs::read_to_string(file)
        .map_err(|e| RpcError::not_found(format!("{path}: {e}"), "check the path with assets.list or ls"))
}

/// The file at the project-relative `path` (`regia_core::project_path` rules, no symlink leaving
/// the project), for a method that reads any kind of file.
pub(crate) fn project_file(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    // ADR-8798: the shared root, so `AgentSettings::project_dir` counts when no asset server is set.
    crate::req_path::project_file(world, "path", path)
}

/// Resolves `out_dir` under the agent's allowed roots (`req_path::allowed_path_in`): relative paths
/// land under the project root; absolute ones must lie under the project root, the system temp
/// directory or an `output_roots` entry; `..` and symlinks leading out are
/// refused (ADR-1816: the old text-only check let a symlink in the project or the temp dir lead out).
pub(crate) fn confined_out_dir(world: &World, dir: &str) -> Result<std::path::PathBuf, RpcError> {
    if dir.is_empty() {
        return Err(RpcError::invalid_params("`out_dir` must not be empty"));
    }
    // ADR-8798: the shared root, so a relative `out_dir` joins `AgentSettings::project_dir` when no
    // asset server is set, never the working directory as well.
    crate::req_path::allowed_path(world, "out_dir", dir)
}

/// Writes `bytes` to `dir/file` through a temp file and a rename.
pub(crate) fn write_atomic(
    dir: &std::path::Path,
    file: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let target = dir.join(file);
    let tmp = dir.join(format!("{file}.{}.tmp", std::process::id()));
    // The temp name is predictable, so a leftover (or a planted link) is removed, not opened.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?
        .write_all(bytes)?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// Maps an available base directory to the default WAV directory by appending the
/// `regia_loopgen` suffix; `None` (no base available) stays `None` so the caller can answer
/// a clear error instead of guessing a directory. Shared by the desktop and Android arms of
/// [`default_wav_dir`] so the composition is testable on host CI.
fn wav_dir_for_cache(base: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    base.map(|dir| dir.join("regia_loopgen"))
}

/// Default `out_dir` for `audio.loopgen_render`'s WAV when the call gives none. Desktop:
/// `<system temp dir>/regia_loopgen`. Android: `<app cache dir>/regia_loopgen` from the
/// `AndroidAppHandle` resource — the system temp dir there is `/data/local/tmp` (mode 0771,
/// owned by `shell`), which a normal app process cannot create, so the desktop default always
/// failed with `PermissionDenied` (audit-g-agent6-platform). `None` when no app cache dir is
/// available; the call then answers a clear error instead of an OS permission error.
fn default_wav_dir(world: &World) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "android")]
    {
        wav_dir_for_cache(
            world
                .get_resource::<regia_platform::AndroidAppHandle>()
                .and_then(|handle| regia_platform::paths::cache_dir(&handle.0)),
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = world;
        wav_dir_for_cache(Some(std::env::temp_dir()))
    }
}

/// `audio.loopgen_render`: renders a `.loopgen.ron` (inline `ron` text, or a project-relative
/// `path`) and returns its measurements; with `wav: true` also writes a 16-bit WAV into
/// `out_dir` (default: `<system temp dir>/regia_loopgen` on desktop, `<app cache
/// dir>/regia_loopgen` on Android). Works without an audio plugin. A bad graph returns
/// `INVALID_PARAMS` with `data.key_path`.
pub fn loopgen_render(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let inline = crate::params::opt_str(params, "ron")?;
    let path = crate::params::opt_str(params, "path")?;
    let text = match (inline, path) {
        (Some(text), None) => text.to_string(),
        (None, Some(path)) => read_project_text(world, path)?,
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `ron` (inline text) or `path` (a project-relative .loopgen.ron)",
            ));
        }
    };
    let wav_dir = if crate::params::opt_bool(params, "wav")?.unwrap_or(false) {
        Some(match params.get("out_dir") {
            None | Some(Value::Null) => default_wav_dir(world).ok_or_else(|| {
                RpcError::internal(
                    "the default `out_dir` is the app cache dir, which is unavailable here; pass `out_dir` explicitly",
                )
            })?,
            Some(Value::String(dir)) => confined_out_dir(world, dir)?,
            Some(_) => return Err(RpcError::invalid_params("`out_dir` must be a string")),
        })
    } else {
        None
    };
    let local = LoopGenCache::default();
    let cache = world
        .get_resource::<SoundAssets>()
        .map_or(&local, SoundAssets::loopgens);
    let hits = cache.hits();
    let rendered = cache.render_text(&text).map_err(|e| {
        let mut error = RpcError::invalid_params(format!("invalid loop generator: {e}"));
        if let Some(data) = error.data.as_mut() {
            data["key_path"] = json!(e.key_path);
        }
        error
    })?;
    let cached = cache.hits() > hits;
    let stats = rendered.stats();
    let mut out = json!({
        "name": rendered.name,
        "sample_rate": rendered.sample_rate,
        "channels": rendered.channels,
        "cached": cached,
        "content_hash": format!("{:016x}", LoopGenCache::content_hash(&text)),
        "stats": stats_json(&stats),
    });
    if let Some(dir) = wav_dir {
        let clean: String = rendered
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let file = format!("{clean}-{:016x}.wav", stats.hash);
        let written = write_atomic(&dir, &file, &rendered.to_wav_bytes())
            .map_err(|e| RpcError::internal(format!("writing {file}: {e}")))?;
        out["wav"] = json!(written.display().to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit ADR-3206 finding 13: `write_atomic`'s temp name is predictable, so a link planted at it
    /// (the project is often a cloned repository) took the write and truncated its target.
    #[cfg(unix)]
    #[test]
    fn write_atomic_never_writes_through_a_planted_temp_link() {
        let base = regia_test_utils::unique_temp_dir("audio-write-atomic");
        let victim = base.join("victim");
        std::fs::write(&victim, "keep").expect("victim");
        let dir = base.join("out");
        std::fs::create_dir_all(&dir).expect("dir");
        let tmp = dir.join(format!("a.wav.{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&victim, &tmp).expect("link");
        let written = write_atomic(&dir, "a.wav", b"RIFF").expect("writes");
        assert_eq!(std::fs::read(written).expect("target"), b"RIFF");
        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep");
        assert!(!tmp.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    use regia_audio::{AudioPlugin, AudioSettings, ListenerContact, ListenerCoupling};

    fn app_world() -> World {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        std::mem::take(app.world_mut())
    }

    #[test]
    fn medium_reports_the_preset_and_contact() {
        let mut world = app_world();
        world
            .resource_mut::<Audio>()
            .set_medium(regia_audio::AudioMedium::airless());
        let out = medium(&mut world, &json!({})).expect("medium");
        assert_eq!(out["kind"], "airless");
        assert_eq!(out["airborne"], false);
        assert_eq!(out["ground"]["name"], "ice");
        assert_eq!(out["listener"]["contact"], "boots");
    }

    #[test]
    fn audit_propagation_rejects_nonfinite_f32_position_and_range() {
        let mut world = app_world();
        let valid = json!({ "position": [10, 0, 0], "max_distance": 100 });
        let before = propagation(&mut world, &valid).expect("valid solve");
        for params in [
            json!({ "position": [1e308, 0, 0] }),
            json!({ "position": [0, -1e308, 0] }),
            json!({ "position": [0, 0, 1e308] }),
            json!({ "position": [0, 0, 0], "max_distance": 1e308 }),
            json!({ "position": [0, 0, 0], "max_distance": -1 }),
            json!({ "position": [0, 0, 0], "max_distance": -1e-100 }),
        ] {
            let err = propagation(&mut world, &params).expect_err("invalid solver inputs");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{params}");
            assert_eq!(propagation(&mut world, &valid).expect("unchanged"), before);
        }
        propagation(&mut world, &json!({ "position": [0, 0, 0], "max_distance": 0 }))
            .expect("nonnegative range");
    }

    #[test]
    fn propagation_at_a_position_reports_both_paths() {
        let mut world = app_world();
        world
            .resource_mut::<Audio>()
            .set_medium(regia_audio::AudioMedium::airless());
        let out = propagation(
            &mut world,
            &json!({ "position": [500.0, 0.0, 0.0], "category": "explosion" }),
        )
        .expect("propagation");
        let paths = out["paths"].as_array().expect("paths");
        assert_eq!(paths.len(), 1, "airless: ground only: {out}");
        assert_eq!(paths[0]["path"], "ground");
        let delay = paths[0]["delay_ms"].as_f64().expect("delay");
        assert!((delay - 500.0 / 3.8).abs() < 0.5, "{delay}");
        assert_eq!(out["coupled"], true);

        world
            .resource_mut::<Audio>()
            .set_listener_coupling(ListenerCoupling {
                contact: ListenerContact::None,
                ..ListenerCoupling::default()
            });
        // The fade runs in the tick; force it through.
        for _ in 0..4 {
            world.resource_mut::<Audio>().tick(1.0 / 60.0);
        }
        let out = propagation(
            &mut world,
            &json!({ "position": [500.0, 0.0, 0.0], "category": "explosion" }),
        )
        .expect("propagation");
        assert_eq!(out["paths"].as_array().map(Vec::len), Some(0), "{out}");
        assert_eq!(out["audible"], false);
    }

    #[test]
    fn errors_are_clear() {
        let mut world = app_world();
        assert!(propagation(&mut world, &json!({})).is_err());
        assert!(propagation(&mut world, &json!({ "position": [0, 0, 0], "category": "nope" })).is_err());
        let mut empty = World::new();
        assert!(medium(&mut empty, &json!({})).is_err());
    }

    const LOOP: &str = r#"(version: 1, name: "wind", length_secs: 0.5, seed: 3,
        nodes: [(id: "n", node: Noise(color: Pink, gain: 0.8)),
                (id: "lp", node: Filter(input: "n", mode: LowPass, cutoff_hz: 400.0))],
        output: "lp")"#;

    #[test]
    fn loopgen_render_reports_stats_and_caches() {
        let mut world = app_world();
        world.insert_resource(SoundAssets::default());
        let first = loopgen_render(&mut world, &json!({ "ron": LOOP })).expect("renders");
        assert_eq!(first["cached"], false);
        assert_eq!(first["stats"]["frames"], 24_000);
        assert!(first["stats"]["rms"].as_f64().expect("rms") > 0.001);
        let second = loopgen_render(&mut world, &json!({ "ron": LOOP })).expect("renders");
        assert_eq!(second["cached"], true);
        assert_eq!(first["stats"]["hash"], second["stats"]["hash"]);
        let listed = loopgens_json(&world);
        assert_eq!(listed["cache"]["entries"], 1);
        assert_eq!(listed["loops"][0]["name"], "wind");
    }

    #[cfg(unix)]
    #[test]
    fn loopgen_render_refuses_an_out_dir_outside_the_allowed_roots() {
        let mut world = app_world();
        let error = loopgen_render(
            &mut world,
            &json!({ "ron": LOOP, "wav": true, "out_dir": "/usr/share/regia_loopgen_refused" }),
        )
        .expect_err("outside every allowed root");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(error.message.contains("out_dir"), "{}", error.message);
        assert!(!std::path::Path::new("/usr/share/regia_loopgen_refused").exists());
    }

    #[test]
    fn loopgen_render_writes_a_wav_and_names_bad_key_paths() {
        let mut world = app_world();
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-loopgen-agent-{}-{n}", std::process::id()));
        let out = loopgen_render(
            &mut world,
            &json!({ "ron": LOOP, "wav": true, "out_dir": dir.display().to_string() }),
        )
        .expect("renders");
        let wav = std::fs::read(out["wav"].as_str().expect("wav path")).expect("wav file");
        assert_eq!(&wav[..4], b"RIFF");
        std::fs::remove_dir_all(&dir).expect("cleanup");

        let bad = LOOP.replace("cutoff_hz: 400.0", "cutoff_hz: 400000.0");
        let error = loopgen_render(&mut world, &json!({ "ron": bad })).expect_err("invalid");
        let data = error.data.expect("data");
        assert_eq!(data["key_path"], "nodes[1].node.Filter.cutoff_hz");
        assert!(loopgen_render(&mut world, &json!({})).is_err());
        assert!(loopgen_render(&mut world, &json!({ "path": "../x.loopgen.ron" })).is_err());
    }

    /// fixrev-05-k (audit-kimirev-fix-agent7-platform-muse, Med 1): with no `out_dir` the WAV
    /// must land under the scratch output dir — the system temp directory on desktop, the app
    /// cache directory on Android — where the app can always write, and the reply must point at
    /// the file that exists.
    #[test]
    fn loopgen_render_defaults_the_wav_to_the_scratch_out_dir() {
        let mut world = app_world();
        let out = loopgen_render(&mut world, &json!({ "ron": LOOP, "wav": true })).expect("renders");
        let want = crate::scratch_out_dir(&world, "regia_loopgen");
        let wav = out["wav"].as_str().expect("wav path");
        assert!(
            wav.starts_with(&*want.to_string_lossy()),
            "wav at {wav}, expected under {}",
            want.display()
        );
        let bytes = std::fs::read(wav).expect("wav file");
        assert_eq!(&bytes[..4], b"RIFF");
        let _ = std::fs::remove_file(wav);
    }

    #[test]
    fn loopgen_render_refuses_an_escaping_out_dir() {
        let mut world = app_world();
        for bad in ["../escape", "/etc/regia-loopgen", "", "a/../../b"] {
            let error = loopgen_render(&mut world, &json!({ "ron": LOOP, "wav": true, "out_dir": bad }))
                .expect_err(bad);
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        let error = loopgen_render(&mut world, &json!({ "ron": LOOP, "wav": true, "out_dir": 5 }))
            .expect_err("number");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
    }

    /// audit-agent-sec (Medium): the lexical check let a symlink under the temp dir (or the
    /// project) carry a WAV or a resource dump outside both roots.
    #[cfg(unix)]
    #[test]
    fn an_out_dir_through_a_symlink_that_leaves_the_roots_is_refused() {
        let base = regia_test_utils::unique_temp_dir("agent-outdir-link");
        std::os::unix::fs::symlink("/usr", base.join("link")).expect("symlink");
        let through = base.join("link").join("regia-out-dir-refused");
        let world = World::new();
        let error = confined_out_dir(&world, &through.display().to_string()).expect_err("symlink out");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(error.message.contains("symlink"), "{}", error.message);
        // A plain directory under the temp dir, existing or not, still resolves.
        let fine = base.join("renders").join("sub");
        assert_eq!(
            confined_out_dir(&world, &fine.display().to_string()).expect("plain"),
            fine
        );
        let mut world = app_world();
        let error = loopgen_render(
            &mut world,
            &json!({ "ron": LOOP, "wav": true, "out_dir": through.display().to_string() }),
        )
        .expect_err("loopgen through the link");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert!(!through.exists());
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    /// audit-g-agent6-platform (Medium): with no `out_dir` the default used to be
    /// `std::env::temp_dir()/regia_loopgen`, which on Android is `/data/local/tmp` (mode 0771,
    /// owned by `shell`): a normal app process always got `PermissionDenied` from
    /// `create_dir_all`. Desktop keeps the temp-dir default; Android uses the app cache dir
    /// (and refuses to fall back to the shell-owned directory).
    #[cfg(not(target_os = "android"))]
    #[test]
    fn loopgen_render_default_wav_dir_stays_in_the_system_temp_dir() {
        let world = World::new();
        assert_eq!(
            default_wav_dir(&world).expect("a default out dir on desktop"),
            std::env::temp_dir().join("regia_loopgen"),
            "the documented desktop default is pinned"
        );
    }

    #[test]
    fn wav_dir_for_cache_appends_the_regia_loopgen_suffix() {
        // The host-runnable pin for the Android arm of `default_wav_dir` (the composition
        // itself needs an `AndroidAppHandle`, which no test can build): an available cache
        // dir maps to `<cache>/regia_loopgen`, and none stays none.
        assert_eq!(
            wav_dir_for_cache(Some(std::path::PathBuf::from("/cache"))),
            Some(std::path::PathBuf::from("/cache/regia_loopgen"))
        );
        assert_eq!(wav_dir_for_cache(None), None);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn loopgen_render_default_wav_dir_never_uses_the_shell_owned_tmp() {
        // A unit test cannot build an `AndroidApp`, so without the handle there must be no
        // default at all: answering a clear error beats the old PermissionDenied from
        // /data/local/tmp on every call.
        assert_eq!(default_wav_dir(&World::new()), None);
    }

    #[cfg(unix)]
    #[test]
    fn loopgen_render_rejects_a_symlink_that_leaves_the_project() {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "regia-agent-loopgen-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let project = base.join("project");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::write(base.join("outside.loopgen.ron"), LOOP).expect("outside");
        std::os::unix::fs::symlink(base.join("outside.loopgen.ron"), project.join("leak.loopgen.ron"))
            .expect("symlink");
        std::fs::write(project.join("ok.loopgen.ron"), LOOP).expect("inside");
        // The helper the handlers read through; a symlink leading out is refused, a file inside is read.
        let leaked = crate::req_path::project_file_in(&project, "path", "leak.loopgen.ron");
        assert!(leaked.is_err());
        let fine = crate::req_path::project_file_in(&project, "path", "ok.loopgen.ron");
        assert!(fine.is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }
}
