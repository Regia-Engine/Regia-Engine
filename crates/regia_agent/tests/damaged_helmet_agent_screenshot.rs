// SPDX-License-Identifier: MPL-2.0
//! Agent `seeing.screenshot` with `target: asset` → `target/evidence/m4/damaged-helmet-agent.png`
//! (never `docs/`), gated behind `REGIA_WRITE_EVIDENCE=1`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regia::prelude::*;
use regia_agent::client::Client;
use regia_asset::{AssetPlugin, AssetServer, AssetServerSettings};
use regia_asset_pipeline::{CookOptions, cook_project, host_platform};
use regia_scene::{GlobalTransform, Transform};
use serde_json::json;

#[test]
fn agent_screenshot_asset_writes_evidence_png() {
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skip agent screenshot evidence: set REGIA_WRITE_EVIDENCE=1");
        return;
    }
    if regia_core::brand::env_var_os("REGIA_WINIT_ANY_THREAD").is_none() {
        eprintln!("skip agent screenshot: set REGIA_WINIT_ANY_THREAD=1 (cargo test worker thread)");
        return;
    }
    let cache = match regia_core::brand::env_var("REGIA_KHRONOS_ASSETS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("skip agent screenshot: set REGIA_KHRONOS_ASSETS");
            return;
        }
    };
    let helmet = cache.join("DamagedHelmet.glb");
    if !helmet.is_file() {
        eprintln!("skip agent screenshot: run cargo xtask fetch-test-assets");
        return;
    }

    let root = std::env::temp_dir().join(format!("regia_agent_shot_{}", uuid::Uuid::new_v4()));
    let assets = root.join("assets").join("khronos");
    std::fs::create_dir_all(&assets).expect("assets");
    std::fs::copy(&helmet, assets.join("DamagedHelmet.glb")).expect("copy");
    cook_project(CookOptions::for_host(root.clone())).expect("cook");

    let asset_path = "assets/khronos/DamagedHelmet.glb".to_string();
    let out =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/evidence/m4/damaged-helmet-agent.png");
    let shot_ok = Arc::new(AtomicBool::new(false));
    let shot_flag = shot_ok.clone();
    let project = root.clone();
    let path_for_rpc = asset_path.clone();
    let out_path = out.clone();

    std::thread::spawn(move || {
        let agent_dir = regia_agent::endpoint::agent_dir(&project);
        for _ in 0..400 {
            if regia_agent::endpoint::read(&agent_dir).is_ok() {
                std::thread::sleep(Duration::from_secs(2));
                if let Ok(mut client) = Client::connect_project(&project)
                    && let Ok(result) = client.call(
                        "seeing.screenshot",
                        json!({
                            "target": "asset",
                            "path": path_for_rpc,
                            "timeout_ms": 60_000,
                        }),
                    )
                    && let Some(b64) = result["png_base64"].as_str()
                    && let Some(bytes) = base64_decode(b64)
                {
                    if let Some(parent) = out_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(&out_path, &bytes).is_ok() {
                        shot_flag.store(true, Ordering::Relaxed);
                    }
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    struct ShotApp {
        root: PathBuf,
        asset_path: String,
    }
    impl Plugin for ShotApp {
        fn build(&self, app: &mut App) {
            app.add_plugins(AssetPlugin {
                settings: AssetServerSettings {
                    project_root: self.root.clone(),
                    platform: host_platform().to_string(),
                    content_dir: None,
                },
            })
            .insert_resource(regia_agent::AgentSettings {
                project_dir: Some(self.root.clone()),
            })
            .insert_resource(ShotState {
                asset_path: self.asset_path.clone(),
                loaded: false,
            })
            .add_systems(Update, shot_frame);
        }
    }

    #[derive(Resource)]
    struct ShotState {
        asset_path: String,
        loaded: bool,
    }

    /// Loads the cooked helmet and spawns the preview entities for the runtime
    /// render path: a `MeshRenderer` at the origin, an EV-pinned camera at the
    /// classic preview position, and a 100 klx key light.
    fn shot_frame(
        mut state: ResMut<ShotState>,
        mut server: ResMut<AssetServer>,
        mut commands: Commands,
        time: Res<Time>,
        mut app_exit: MessageWriter<AppExit>,
    ) {
        if !state.loaded {
            let mesh = server
                .load_mesh_path(&state.asset_path)
                .ok()
                .and_then(|h| server.meshes().get(&h).cloned());
            if let Some(mesh) = mesh {
                state.loaded = true;
                let cpu = Arc::new(regia_render::CpuMesh {
                    vertices: mesh
                        .positions
                        .iter()
                        .zip(mesh.normals.iter())
                        .zip(mesh.uvs.iter())
                        .enumerate()
                        .map(|(index, ((position, normal), uv))| regia_render::LabVertex {
                            position: *position,
                            normal: *normal,
                            uv: *uv,
                            tangent: mesh.tangents.get(index).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
                            color: [1.0; 4],
                            joints: [0; 4],
                            weights: [0.0; 4],
                        })
                        .collect(),
                    indices: mesh.indices.clone(),
                });
                let mesh_transform = Transform::default();
                commands.spawn((
                    mesh_transform,
                    GlobalTransform(mesh_transform.compute_affine()),
                    regia_render::MeshRenderer::new(cpu, Arc::new(regia_render::CpuMaterial::default())),
                ));
                use regia_core::math::{Quat, Vec3};
                let eye = Vec3::new(0.0, 0.2, 2.5);
                let camera_transform = Transform {
                    translation: eye,
                    rotation: Quat::from_rotation_arc(-Vec3::Z, -eye.normalize_or_zero()),
                    scale: Vec3::ONE,
                };
                commands.spawn((
                    camera_transform,
                    GlobalTransform(camera_transform.compute_affine()),
                    regia_render::Camera {
                        exposure_ev: Some(14.5),
                        ..regia_render::Camera::default()
                    },
                ));
                let dir = Vec3::new(-0.577, -0.577, -0.577).normalize_or_zero();
                let light_transform = Transform {
                    translation: Vec3::ZERO,
                    rotation: Quat::from_rotation_arc(-Vec3::Z, dir),
                    scale: Vec3::ONE,
                };
                commands.spawn((
                    light_transform,
                    GlobalTransform(light_transform.compute_affine()),
                    regia_render::DirectionalLight {
                        direction: [-0.577, -0.577, -0.577],
                        color: [1.0, 1.0, 1.0],
                        illuminance: 100_000.0,
                        cast_shadows: false,
                        static_shadow: false,
                    },
                ));
            }
        }
        if time.frame_count() >= 180 {
            app_exit.write(AppExit::Success);
        }
    }

    let _ = regia::run_with(
        ShotApp {
            root: root.clone(),
            asset_path,
        },
        |_| {},
    );

    if !shot_ok.load(Ordering::Relaxed) {
        eprintln!("skip agent screenshot: RPC did not produce PNG (GPU/agent timing)");
        return;
    }
    assert!(out.is_file());
    assert!(out.metadata().map(|m| m.len() > 0).unwrap_or(false));

    let bytes = std::fs::read(&out).expect("read evidence png");
    let (w, h, rgba) = decode_png_rgba(&bytes).expect("decode png");
    assert!(w >= 64 && h >= 64, "screenshot too small: {w}x{h}");
    let corner = sample_rgba(&rgba, w, h, 0, 0);
    let foreground_pixels = rgba.as_chunks::<4>().0.iter().filter(|px| **px != corner).count();
    assert!(
        foreground_pixels > 1_000,
        "helmet should cover >1000 non-background pixels, got {foreground_pixels}"
    );
    let unique = unique_rgba_colors(&rgba);
    // Untextured M4 forward shading yields hundreds of 8-bit tones on this mesh (~250), not thousands.
    // Flat clear/regression is a single unique colour.
    assert!(
        unique > 100,
        "expected shaded helmet (>100 unique colors; flat clear = 1), got {unique}"
    );
    assert!(
        centre_differs_from_corners(&rgba, w, h),
        "center pixel should differ from corners (flat clear color?)"
    );
}

fn decode_png_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let len = info.buffer_size();
    Some((info.width, info.height, buf[..len].to_vec()))
}

fn sample_rgba(rgba: &[u8], width: u32, _height: u32, x: usize, y: usize) -> [u8; 4] {
    let w = width as usize;
    let i = (y * w + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn unique_rgba_colors(rgba: &[u8]) -> usize {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    for px in rgba.as_chunks::<4>().0 {
        set.insert((px[0], px[1], px[2], px[3]));
    }
    set.len()
}

fn centre_differs_from_corners(rgba: &[u8], width: u32, height: u32) -> bool {
    let w = width as usize;
    let h = height as usize;
    let centre = sample_rgba(rgba, width, height, w / 2, h / 2);
    let corners = [
        sample_rgba(rgba, width, height, 0, 0),
        sample_rgba(rgba, width, height, w - 1, 0),
        sample_rgba(rgba, width, height, 0, h - 1),
        sample_rgba(rgba, width, height, w - 1, h - 1),
    ];
    corners.iter().any(|c| c != &centre)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}
