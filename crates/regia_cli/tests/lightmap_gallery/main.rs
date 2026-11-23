// SPDX-License-Identifier: MPL-2.0
//! The v0.3.0 interior plate (ADR-3001): an underground gallery built in Regia Engine
//! ([`gallery`]), lightmapped by `regia bake-lightmaps --scene` and scored against the path tracer
//! at the same camera (`specs/27` § 2.2 GI rows, the chart-seam metric of ADR-2024). Plates and
//! numbers: `docs/evidence/lightmaps-v030/` (`gallery_*`, `SUMMARY.md` § 8).
//!
//! - `the_fixture_is_the_generator_output` (CPU): the checked-in project
//!   `tests/fixtures/lightmap_gallery` is what [`gallery::project_files`] writes, so the plates'
//!   regeneration command bakes the geometry this file describes. `REGIA_GALLERY_WRITE=1`
//!   rewrites the fixture after a deliberate change of the generator.
//! - `texcoord0_only_meshes_unwrap_at_bake` (CPU): meshes with `TEXCOORD_0` only and no lightmap
//!   UV set bake through the real binary. The bake lays out the second UV set itself and stores it
//!   in the `.flmap` (its geometry section), never in the source meshes: the glTF files are
//!   byte-identical after the bake, and nothing is written under `assets/` but the `.flmap`. Every
//!   entry fits its imported mesh (`receiver_mesh`), so the runtime draws it with the bake's UV2.
//! - `the_cook_carries_the_unwrap` (CPU): `regia cook` after the bake keeps the unwrap in the cooked
//!   lightmap, it still fits the imported meshes, and the source glTF files are still byte-identical.
//! - `gallery_lightmap_against_the_path_tracer` (GPU, any adapter, lavapipe in CI): the bake on the
//!   frame against the tracer's indirect light, beside the flat ambient. Quick settings by default
//!   (what the gate runs); the evidence settings (8192 spp, 1024 samples, 6 bounces, a density sweep
//!   for the tier budgets) with `REGIA_LIGHTMAP_EVIDENCE_DIR` set, which also writes the plates.
//!   `REGIA_GALLERY_SIZE=WxH` sets the plate size (default 160x96), `REGIA_GALLERY_SPP` and
//!   `REGIA_GALLERY_SAMPLES` the reference's and the bake's samples.
//!
//! The level declares its interior air ([`interiors`], ADR-4757): the hall under its ceiling, the
//! adit and the ramp's roofed run. The frame's aerial perspective takes no sky in-scatter there, so
//! the beauty's color is held against the tracer's per region (`MAX_BEAUTY_BG_ERROR`), and the
//! sun-only frame adds nothing where the sun never reaches (`MAX_SUN_ONLY_SHARE_IN_SHADOW`).
//! `REGIA_GALLERY_NO_INTERIORS=1` renders the frames without the boxes (the before plates).
//!
//! Since ADR-4762 each view is also drawn with the receivers' indirect specular off (the lightmap's
//! own light, which the diffuse checks hold against the tracer's diffuse lobe) and, at the evidence
//! size, with the reflection-capture candidate (the image light on and one local capture of the
//! gallery, with the bake filling uncovered reflection shares). The frame with the specular holds the floor's whole
//! indirect light against the tracer's (`WHOLE_INDIRECT_BAND`).
//!
//! The reference is two renders with different seeds, averaged. The tracer's remaining noise scores
//! as error on any technique, and more in a room lit through small openings, so each view reports it:
//! `reference_noise_floor` is one half-budget render scored against the average, the same noise
//! amplitude a noiseless perfect image would meet (the difference of the two is half of `A - B`, whose
//! variance is the full-budget render's).

mod gallery;
mod lmspace;
mod screen;

// Lightmaps for every package tier from one bake (ADR-3120).
mod tiers;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};

use regia_asset::lightmap::{Lightmap, LightmapEntryKey, LightmapTexelEncoding};
use regia_asset::{AssetId, CookedMesh, CpuMesh};
use regia_asset_pipeline::{Forward, TextureCookQuality, host_platform, import_gltf_path};
use regia_core::glam::Vec3;
use regia_pt::compare::{LoadedImage, compare_images, psnr_db};
use regia_pt::extract::{ExtractOptions, from_frame};
use regia_pt::{PathTracer, PtImage, PtSettings, PtSky};
use regia_render::compare::{DEFAULT_PPD, display_encode, flip_heatmap_rgba8};
use regia_render::lightmap::{LightmapReceiverMesh, LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, EnvironmentAuthority, FrameQuality, FrameRenderer, FrameScene, GiMode,
    InteriorVolume, LabMaterial, LabMesh, LabScene, LabVertex, LocalCaptureDesc, OutdoorParams, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, StudioEquirect,
};
use regia_test_utils::gi_error::{
    GiError, GiErrorInput, add_planes, luminance, measure, shadow_mask, sub_planes, surface_mask,
};
use serde_json::{Value, json};

/// The checked-in fixture project.
const FIXTURE: &str = "tests/fixtures/lightmap_gallery";
/// Rewrites the fixture from the generator when set to `1`.
const WRITE_ENV: &str = "REGIA_GALLERY_WRITE";
/// Where the plates and numbers go when set (the evidence settings).
const EVIDENCE_ENV: &str = "REGIA_LIGHTMAP_EVIDENCE_DIR";
/// The plate size, `WxH`.
const SIZE_ENV: &str = "REGIA_GALLERY_SIZE";
/// The reference's samples per pixel, over the setting's default.
const SPP_ENV: &str = "REGIA_GALLERY_SPP";
/// The bake's samples per texel, over the setting's default.
const SAMPLES_ENV: &str = "REGIA_GALLERY_SAMPLES";
/// Renders the frames without the level's interior boxes when set to `1` (the before plates of
/// ADR-4757; the color checks then fail, as they should).
const NO_INTERIORS_ENV: &str = "REGIA_GALLERY_NO_INTERIORS";
/// § 2.2 GI (diffuse) mean FLIP target.
const FLIP_TARGET: f32 = 0.08;
/// § 2.2 GI (diffuse) FLIP p95 target.
const FLIP_P95_TARGET: f32 = 0.20;
/// The shipping package's default seam budget (`[lightmaps] seam_budget`).
const SHIPPING_SEAM_BUDGET: f64 = 0.25;
/// Frames per still capture (no technique here keeps a history).
const STILL_FRAMES: u32 = 2;
/// Display exposure of the plates (EV100): the gallery's indirect light is a few hundred cd/m².
const EXPOSURE_EV: f32 = 9.0;
/// The tier sweep (ADR-3001): name, density (texels per metre) and whether the bake keeps its
/// direction section. `ULTRA_LOW` twice (with and without directions: the direction section is 4 bytes
/// a texel in every encoding), `LOW`, `HIGH` (the CLI default), `ULTRA`.
const TIER_SWEEP: [(&str, f32, bool); 5] = [
    ("ultra_low_flat", 2.0, false),
    ("ultra_low", 2.0, true),
    ("low", 4.0, true),
    ("high", 8.0, true),
    ("ultra", 16.0, true),
];

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dirs");
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).expect("copy");
        }
    }
}

/// The fixture copied to a temporary project.
fn project(tag: &str) -> Cleanup {
    let dir = regia_test_utils::unique_temp_dir(tag);
    copy_tree(&fixture_dir(), &dir);
    Cleanup(dir)
}

fn regia(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run regia")
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Every file under `dir`, relative, sorted, with its bytes.
fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.expect("walk");
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .expect("under dir")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, std::fs::read(entry.path()).expect("read")));
        }
    }
    out
}

/// Bakes the project's level through the real binary; `extra` overrides the defaults.
fn bake(dir: &Path, out: &str, extra: &[&str]) -> Value {
    let mut args = vec!["bake-lightmaps", "--scene", gallery::LEVEL, "--out", out];
    args.extend_from_slice(extra);
    let output = regia(dir, &args);
    let v = json_of(&output);
    assert!(
        output.status.success() && v["ok"] == true,
        "bake failed: {v}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    v
}

/// The imported mesh of `part` as the bake sees it (the importer's vertex order).
fn imported(dir: &Path, part: &gallery::Part) -> CpuMesh {
    let path = dir.join(part.mesh_path());
    let result = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        host_platform(),
        None,
        TextureCookQuality::Low,
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let meshes: Vec<CpuMesh> = result
        .sub_assets
        .iter()
        .filter(|s| s.type_path == "regia_asset::CookedMesh")
        .map(|s| {
            CookedMesh::from_postcard(&s.payload)
                .expect("decode the cooked mesh")
                .to_cpu_mesh()
                .expect("CPU mesh")
        })
        .collect();
    assert_eq!(meshes.len(), 1, "{}: one mesh", part.name);
    meshes.into_iter().next().expect("one mesh")
}

fn lab_vertices(mesh: &CpuMesh) -> Vec<LabVertex> {
    (0..mesh.positions.len())
        .map(|i| LabVertex {
            position: mesh.positions[i],
            normal: mesh.normals[i],
            uv: mesh.uvs.get(i).copied().unwrap_or([0.0; 2]),
            tangent: mesh.tangents.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn entry_key(part: &gallery::Part) -> LightmapEntryKey {
    LightmapEntryKey {
        entity: part.guid.parse().expect("a guid"),
        part: 0,
    }
}

#[test]
fn the_fixture_is_the_generator_output() {
    let dir = fixture_dir();
    if regia_core::brand::env_var_os(WRITE_ENV).is_some_and(|v| v == "1") {
        gallery::write_project(&dir);
    }
    for (rel, text) in gallery::project_files() {
        let on_disk = std::fs::read_to_string(dir.join(&rel)).unwrap_or_else(|e| {
            panic!(
                "{rel}: {e} (regenerate with {WRITE_ENV}=1 cargo test -p regia_cli --test lightmap_gallery)"
            )
        });
        assert!(
            on_disk == text,
            "{rel} is not the generator's output; regenerate with {WRITE_ENV}=1 after a deliberate change"
        );
    }
    // Every mesh has POSITION, NORMAL and TEXCOORD_0, and no second UV set.
    for p in gallery::parts() {
        let text = std::fs::read_to_string(dir.join(p.mesh_path())).expect("read");
        let v: Value = serde_json::from_str(&text).expect("glTF JSON");
        let attributes = v["meshes"][0]["primitives"][0]["attributes"]
            .as_object()
            .expect("attributes");
        let mut names: Vec<&str> = attributes.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["NORMAL", "POSITION", "TEXCOORD_0"], "{}", p.name);
        // TEXCOORD_0 tiles: it is no lightmap layout (outside 0..1).
        assert!(
            p.uv0.iter().flatten().any(|c| !(0.0..=1.0).contains(c)),
            "{}: TEXCOORD_0 tiles",
            p.name
        );
    }
}

/// The geometry is what the gallery promises: 18 m wide, an 11 m ceiling, closed blocks.
#[test]
fn the_gallery_has_its_promised_shape() {
    let parts = gallery::parts();
    assert_eq!(2.0 * gallery::HALF_WIDTH, 18.0);
    assert!((10.0..=12.0).contains(&gallery::CEILING));
    for p in &parts {
        assert_eq!(p.positions.len(), p.normals.len());
        assert_eq!(p.positions.len(), p.uv0.len());
        assert!(p.indices.len().is_multiple_of(3) && !p.indices.is_empty());
        // Every triangle's winding agrees with its vertices' normal (counter-clockwise from outside).
        for t in p.indices.as_chunks::<3>().0 {
            let [a, b, c] = t.map(|i| regia_core::glam::Vec3::from_array(p.positions[i as usize]));
            let n = regia_core::glam::Vec3::from_array(p.normals[t[0] as usize]);
            let g = (b - a).cross(c - a);
            assert!(g.length() > 1.0e-6, "{}: a degenerate triangle", p.name);
            assert!(
                g.dot(n) > 0.0,
                "{}: a triangle faces away from its normal",
                p.name
            );
        }
    }
    let triangles: usize = parts.iter().map(|p| p.indices.len() / 3).sum();
    assert!(triangles < 2000, "{triangles} triangles");
}

#[test]
fn texcoord0_only_meshes_unwrap_at_bake() {
    let tmp = project("lightmap_gallery_unwrap");
    let dir = &tmp.0;
    let before = tree(dir);
    let v = bake(
        dir,
        gallery::DEFAULT_OUT,
        &[
            "--density",
            "1",
            "--samples",
            "4",
            "--bounces",
            "1",
            "--threads",
            "2",
        ],
    );
    assert_eq!(v["atlas"]["generator"], "atlas");
    let parts = gallery::parts();
    assert_eq!(v["level"]["statics"], parts.len(), "{v}");

    // Nothing but the lightmap was written: the meshes are byte-identical, no sidecar beside them.
    let after = tree(dir);
    let new: Vec<&str> = after
        .iter()
        .filter(|(rel, _)| !before.iter().any(|(r, _)| r == rel))
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert_eq!(new, [gallery::DEFAULT_OUT], "only the lightmap is new");
    for (rel, bytes) in &before {
        let now = &after.iter().find(|(r, _)| r == rel).expect("still there").1;
        assert!(now == bytes, "{rel} changed during the bake");
    }

    // The unwrap is in the lightmap, and fits the imported meshes.
    let map = Lightmap::read(&dir.join(gallery::DEFAULT_OUT)).expect("read the lightmap");
    map.validate().expect("a valid lightmap");
    for p in &parts {
        let e = map
            .entry_for_key(entry_key(p))
            .unwrap_or_else(|| panic!("{}: no entry", p.name));
        let entry = &map.entries[e];
        let unwrap = &map.unwraps[e];
        assert!(entry.charts > 0, "{}: no charts", p.name);
        assert!(
            unwrap.uv.iter().flatten().all(|c| (0.0..=1.0).contains(c)),
            "{}: UV2 outside its rectangle",
            p.name
        );
        let mesh = imported(dir, p);
        assert!(mesh.uvs.iter().flatten().any(|c| !(0.0..=1.0).contains(c)));
        assert_eq!(entry.source_vertices as usize, mesh.positions.len(), "{}", p.name);
        let rx = receiver_mesh(&map, e, &lab_vertices(&mesh), &mesh.indices)
            .unwrap_or_else(|err| panic!("{}: {err}", p.name));
        assert_eq!(rx.uv2.len(), rx.vertices.len());
        assert_eq!(rx.indices.len(), mesh.indices.len(), "{}: same triangles", p.name);
    }
}

/// `regia cook` carries the bake's unwrap into the cooked output: the package has what the runtime
/// draws receivers from, and the source meshes are still the bytes they were.
#[test]
fn the_cook_carries_the_unwrap() {
    let tmp = project("lightmap_gallery_cook");
    let dir = &tmp.0;
    let parts = gallery::parts();
    let meshes: Vec<(String, Vec<u8>)> = parts
        .iter()
        .map(|p| {
            (
                p.mesh_path(),
                std::fs::read(dir.join(p.mesh_path())).expect("read a mesh"),
            )
        })
        .collect();
    bake(
        dir,
        gallery::DEFAULT_OUT,
        &[
            "--density",
            "1",
            "--samples",
            "4",
            "--bounces",
            "1",
            "--threads",
            "2",
        ],
    );
    let baked = Lightmap::read(&dir.join(gallery::DEFAULT_OUT)).expect("read the lightmap");
    let out = regia(dir, &["cook", "--project", ".", "--platform", "linux"]);
    assert!(
        out.status.success(),
        "cook failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for (rel, bytes) in &meshes {
        let now = std::fs::read(dir.join(rel)).expect("read a mesh");
        assert!(&now == bytes, "{rel} changed during the bake or the cook");
    }

    let cache = regia_asset::cooked_cache_dir(dir, "linux");
    let cooked: Vec<Lightmap> = tree(&cache)
        .iter()
        .filter(|(rel, _)| rel.ends_with(".fckd"))
        .filter_map(|(_, bytes)| {
            let (_, payload) = regia_asset::decode_cooked_file(bytes).ok()?;
            Lightmap::from_bytes(payload).ok()
        })
        .collect();
    assert_eq!(cooked.len(), 1, "the cook holds the one lightmap");
    let map = &cooked[0];
    assert_eq!(map.entries.len(), baked.entries.len());
    for p in &parts {
        let e = map
            .entry_for_key(entry_key(p))
            .unwrap_or_else(|| panic!("{}: no entry", p.name));
        let b = baked.entry_for_key(entry_key(p)).expect("baked entry");
        assert!(
            map.unwraps[e].uv == baked.unwraps[b].uv,
            "{}: the unwrap's UV2 survives",
            p.name
        );
        assert!(
            map.unwraps[e].xref == baked.unwraps[b].xref,
            "{}: the unwrap's xref survives",
            p.name
        );
        let mesh = imported(dir, p);
        receiver_mesh(map, e, &lab_vertices(&mesh), &mesh.indices)
            .unwrap_or_else(|err| panic!("{}: the cooked unwrap fits its mesh: {err}", p.name));
    }
}

// --- The plate against the path tracer -----------------------------------------------------------

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| RenderDevice::headless().expect("the gallery plate needs a GPU; lavapipe is a GPU here"))
}

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| PathTracer::new(device()).expect("compile the path tracer"))
}

/// Desktop High as the room evidence runs it (`regia_pt` `lightmap_pt_evidence`): native scale, no
/// temporal upscaling, fog, clouds or reflections, the flat ambient as the GI (the lightmap replaces
/// it on receivers).
fn base_quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

/// Run settings: the gate's quick ones or the evidence's.
#[derive(Clone, Copy, Debug)]
struct Quality {
    spp: u32,
    samples: u32,
    bounces: u32,
    density: f32,
    width: u32,
    height: u32,
}

fn quality(evidence: bool) -> Quality {
    let (width, height) = regia_core::brand::env_var_os(SIZE_ENV)
        .and_then(|v| {
            let s = v.to_string_lossy().to_string();
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((160, 96));
    let count = |name: &str, default: u32| {
        regia_core::brand::env_var_os(name)
            .and_then(|v| v.to_string_lossy().parse().ok())
            .unwrap_or(default)
    };
    if evidence {
        Quality {
            spp: count(SPP_ENV, 8192),
            samples: count(SAMPLES_ENV, 1024),
            bounces: 6,
            density: 8.0,
            width,
            height,
        }
    } else {
        Quality {
            spp: count(SPP_ENV, 96),
            samples: count(SAMPLES_ENV, 32),
            // Match the reference's depth: truncating the warm interreflection at three
            // bounces biases B/G even with no indoor sky in-scatter (ADR-4757).
            bounces: 6,
            density: 2.0,
            width,
            height,
        }
    }
}

/// A camera of the plate.
#[derive(Clone, Copy, Debug)]
struct View {
    name: &'static str,
    eye: [f32; 3],
    target: [f32; 3],
}

/// The plate's cameras: down the gallery from the south end (+x is to the left: the adit's mouth on
/// the left, the skylight's pool, the pillars, the ramp's mouth on the far wall), and across at the
/// adit.
const VIEWS: [View; 2] = [
    View {
        name: "gallery_main",
        eye: [-5.0, 1.7, -13.5],
        target: [1.5, 3.2, 6.0],
    },
    View {
        name: "gallery_adit",
        eye: [-3.0, 1.7, -1.0],
        target: [9.5, 2.4, 6.0],
    },
];

/// The level's light as the bake sees it (`regia_agent::bake_export::level_lights`).
struct Light {
    sun: DirectionalLightUniform,
    sun_angular_radius: f32,
    sky: [f32; 3],
}

fn level_light() -> Light {
    let settings: regia_scene::level::LevelSettings =
        ron::from_str(gallery::LEVEL_SETTINGS_RON).expect("parse the level settings");
    let lights = regia_agent::bake_export::level_lights(&settings, None, None);
    let sun = lights.sun.expect("a noon sun");
    let sky = lights.sky.expect("an Earth sky");
    Light {
        sun: DirectionalLightUniform::new(sun.toward.map(|c| -c), sun.color, sun.illuminance),
        sun_angular_radius: sun.angular_radius,
        sky,
    }
}

/// The gallery as a lab frame: one mesh per part in level order, the imported vertices.
fn lab_scene(meshes: &[CpuMesh], view: View, q: Quality, light: &Light) -> LabScene {
    let parts = gallery::parts();
    let mut post = PostSettings::golden_deterministic();
    post.exposure_ev = EXPOSURE_EV;
    LabScene {
        width: q.width,
        height: q.height,
        eye: view.eye,
        target: view.target,
        fov_y: 1.0,
        near: 0.05,
        far: 400.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: EXPOSURE_EV,
        ambient_lux: std::f32::consts::PI * luminance(light.sky),
        double_sided: false,
        post: Some(post),
        directional: light.sun,
        local_lights: Vec::new(),
        materials: parts
            .iter()
            .map(|p| LabMaterial {
                base_color: [
                    p.material.base_color[0],
                    p.material.base_color[1],
                    p.material.base_color[2],
                    1.0,
                ],
                metallic: 0.0,
                roughness: p.material.roughness,
                ..LabMaterial::default()
            })
            .collect(),
        meshes: meshes
            .iter()
            .enumerate()
            .map(|(k, m)| LabMesh {
                vertices: lab_vertices(m),
                indices: m.indices.clone(),
                material: k,
                ..LabMesh::unskinned()
            })
            .collect(),
    }
}

/// The tracer's view of the gallery: two seeds averaged, and the noise that leaves.
struct Reference {
    beauty: Vec<[f32; 3]>,
    direct: Vec<[f32; 3]>,
    indirect: Vec<[f32; 3]>,
    surface: Vec<bool>,
    shadow: Vec<bool>,
    noise_floor: GiError,
    /// One seed's indirect light (the noise floor of the screen-space checks).
    indirect_a: Vec<[f32; 3]>,
    /// The diffuse lobe's share of `indirect` (two seeds): what a lightmap holds.
    indirect_diffuse: Vec<[f32; 3]>,
    /// World position and normal of each pixel's first hit (`None` for the sky).
    hits: Vec<Option<(Vec3, Vec3)>>,
}

fn trace(lab: &LabScene, light: &Light, q: Quality, seed: u32) -> PtImage {
    let frame = lab.as_frame_scene_with_quality(base_quality());
    let pt = from_frame(
        &frame,
        &ExtractOptions {
            sky: Some(PtSky::Uniform(light.sky)),
            sun_angular_radius: light.sun_angular_radius,
            ..ExtractOptions::default()
        },
    );
    let image = tracer()
        .render(
            &pt,
            &PtSettings {
                spp: q.spp.div_ceil(2),
                max_bounces: 6,
                seed,
                jitter: false,
                samples_per_submit: 8,
                ..PtSettings::default()
            },
        )
        .expect("trace the reference");
    assert_eq!((image.width, image.height), (q.width, q.height));
    image
}

fn average(a: &[[f32; 3]], b: &[[f32; 3]]) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(a, b)| [0.5 * (a[0] + b[0]), 0.5 * (a[1] + b[1]), 0.5 * (a[2] + b[2])])
        .collect()
}

fn reference(lab: &LabScene, view: View, light: &Light, q: Quality) -> Reference {
    let (a, b) = (trace(lab, light, q, 23), trace(lab, light, q, 24));
    let hits = screen::first_hits(&a.depth, &a.normal, view.eye, view.target, q.width, q.height);
    let surface = surface_mask(&a.depth, q.width, q.height, 0.05);
    let shadow = shadow_mask(&surface, &a.shadow, &a.direct, 0.05);
    let indirect_a = add_planes(&a.indirect_diffuse, &a.indirect_specular);
    let indirect_b = add_planes(&b.indirect_diffuse, &b.indirect_specular);
    let indirect = average(&indirect_a, &indirect_b);
    let noise_floor = measure(&GiErrorInput {
        reference: &indirect,
        test: &indirect_a,
        surface: &surface,
        shadow: &shadow,
        width: q.width,
        height: q.height,
        ev100: EXPOSURE_EV,
        scale: None,
    });
    Reference {
        beauty: average(&a.beauty, &b.beauty),
        direct: average(&a.direct, &b.direct),
        indirect,
        surface,
        shadow,
        noise_floor,
        indirect_diffuse: average(&a.indirect_diffuse, &b.indirect_diffuse),
        indirect_a,
        hits,
    }
}

/// The gallery's interior air as the level's host declares it (ADR-4757): the hall under its
/// ceiling (the skylight's shaft above it stays open air), the adit out to the cliff face, and the
/// ramp's roofed run (its box holds rock above and below the slope, which no ray reaches). The
/// frame's aerial perspective takes each view ray's stretch inside them out of the air. Empty with
/// `REGIA_GALLERY_NO_INTERIORS=1`.
fn interiors() -> Vec<InteriorVolume> {
    if regia_core::brand::env_var_os(NO_INTERIORS_ENV).is_some_and(|v| v == "1") {
        return Vec::new();
    }
    let (w, h, l) = (gallery::HALF_WIDTH, gallery::CEILING, gallery::HALF_LENGTH);
    let (az0, az1) = gallery::ADIT_Z;
    let (rx0, rx1) = gallery::RAMP_X;
    // Where the ramp's roof meets the plateau's underside (`gallery::parts`).
    let roof_end = l + (gallery::SURFACE - gallery::SLAB - gallery::RAMP_HEADROOM) / gallery::RAMP_SLOPE;
    let ramp_top = (roof_end - l) * gallery::RAMP_SLOPE + gallery::RAMP_HEADROOM;
    let boxed = |lo: [f32; 3], hi: [f32; 3]| {
        InteriorVolume::with_floor(
            [
                0.5 * (lo[0] + hi[0]),
                0.5 * (lo[1] + hi[1]),
                0.5 * (lo[2] + hi[2]),
            ],
            [
                0.5 * (hi[0] - lo[0]),
                0.5 * (hi[1] - lo[1]),
                0.5 * (hi[2] - lo[2]),
            ],
        )
    };
    vec![
        boxed([-w, 0.0, -l], [w, h, l]),
        boxed([w, 0.0, az0], [gallery::CLIFF_X, gallery::ADIT_HEIGHT, az1]),
        boxed([rx0, 0.0, l], [rx1, ramp_top, roof_end]),
    ]
}

fn capture(renderer: &mut FrameRenderer, frame: &FrameScene<'_>, q: Quality) -> Vec<[f32; 3]> {
    renderer.set_outdoor(OutdoorParams {
        interiors: interiors(),
        ..renderer.outdoor_params().clone()
    });
    for i in 0..STILL_FRAMES {
        if i + 1 == STILL_FRAMES {
            renderer.request_hdr_capture();
        }
        renderer.render_offscreen(device(), frame).expect("frame");
    }
    let hdr = renderer
        .take_hdr_capture(device())
        .expect("read back")
        .expect("the frame recorded its HDR copy");
    assert_eq!((hdr.width, hdr.height), (q.width, q.height));
    hdr.rgb
}

/// The sun with its shadows alone: no sky, no GI, no AO, no lightmap.
fn realtime_direct(lab: &LabScene, q: Quality, studio_sky: Option<[f32; 3]>) -> Vec<[f32; 3]> {
    let mut lab = lab.clone();
    lab.ambient_lux = 0.0;
    lab.ibl = false;
    let quality = FrameQuality {
        ssao: SsaoMode::Off,
        ..base_quality()
    };
    let mut renderer = FrameRenderer::new();
    if let Some(sky) = studio_sky {
        install_capture_sky(&mut renderer, sky);
    }
    capture(&mut renderer, &lab.as_frame_scene_with_quality(quality), q)
}

/// The capture candidate and its sun-only control use the same environment authority.
fn install_capture_sky(renderer: &mut FrameRenderer, sky: [f32; 3]) {
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    let image = StudioEquirect::new(64, 32, vec![sky; 64 * 32]).expect("a uniform sky image");
    assert!(
        renderer.set_studio_equirect(device(), Some(&image)),
        "the sky installs"
    );
}

fn realtime_ambient(lab: &LabScene, q: Quality) -> Vec<[f32; 3]> {
    capture(
        &mut FrameRenderer::new(),
        &lab.as_frame_scene_with_quality(base_quality()),
        q,
    )
}

/// Where a lightmapped frame's receivers take their indirect specular (ADR-4762).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Specular {
    /// From the lightmap (`LightmapSettings::specular`, the default).
    Lightmap,
    /// None: the plate's settings have no image light, probes or reflections (before ADR-4762).
    Off,
    /// The reflection-capture candidate: one local capture of the gallery (ADR-2860), baked
    /// under the tracer's uniform sky radiance. It keeps the same energy partition as the
    /// bake-only candidate, with baked reflection filling uncovered capture shares.
    Capture([f32; 3]),
}

/// The frame with `map` on every part (`entries[k]` is part `k`'s entry).
fn realtime_lightmapped(lab: &LabScene, map: &Arc<Lightmap>, entries: &[usize], q: Quality) -> Vec<[f32; 3]> {
    realtime_lightmapped_with(lab, map, entries, q, Specular::Lightmap)
}

/// [`realtime_lightmapped`] with the receivers' specular from `specular`.
fn realtime_lightmapped_with(
    lab: &LabScene,
    map: &Arc<Lightmap>,
    entries: &[usize],
    q: Quality,
    specular: Specular,
) -> Vec<[f32; 3]> {
    let mut lab = lab.clone();
    lab.ibl = matches!(specular, Specular::Capture(_));
    let meshes: Vec<LightmapReceiverMesh> = entries
        .iter()
        .enumerate()
        .map(|(k, &e)| {
            let m = &lab.meshes[k];
            receiver_mesh(map, e, &m.vertices, &m.indices).unwrap_or_else(|err| panic!("part {k}: {err}"))
        })
        .collect();
    let mut frame = lab.as_frame_scene_with_quality(base_quality());
    let mut keys = Vec::new();
    for (k, (rx, &e)) in meshes.iter().zip(entries).enumerate() {
        let fm = &mut frame.meshes[k];
        fm.vertices = &rx.vertices;
        fm.indices = &rx.indices;
        fm.uv1 = &rx.uv2;
        fm.mesh_key ^= 0x11A7_0000_0000_0000;
        keys.push((fm.object_key, e as u32));
    }
    let mut renderer = FrameRenderer::new();
    if let Specular::Capture(sky) = specular {
        install_capture_sky(&mut renderer, sky);
    }
    renderer.set_lightmap(
        Some(map.clone()),
        LightmapSettings {
            enabled: true,
            intensity: 1.0,
            // Both candidates reserve diffuse energy; only the legacy baseline opts out.
            specular: specular != Specular::Off,
            ..LightmapSettings::default()
        },
    );
    renderer.set_lightmap_receivers(&keys);
    if matches!(specular, Specular::Capture(_)) {
        let half = [gallery::HALF_WIDTH, 0.5 * gallery::CEILING, gallery::HALF_LENGTH];
        renderer
            .place_local_capture(LocalCaptureDesc::new(1, [0.0, half[1], 0.0], half))
            .expect("place the capture");
        let started = std::time::Instant::now();
        let info = renderer
            .bake_local_capture(device(), &frame, 1)
            .expect("bake the capture");
        eprintln!(
            "capture: coverage {:.3}, baked in {:.2} s (six faces and the filter chain)",
            info.coverage,
            started.elapsed().as_secs_f64()
        );
    }
    let rgb = capture(&mut renderer, &frame, q);
    let stats = renderer.lightmap_forward_stats();
    assert!(stats.active, "the lightmap is installed: {stats:?}");
    assert_eq!(
        stats.receivers,
        entries.len(),
        "every part is a receiver: {stats:?}"
    );
    assert_eq!(stats.fallback, 0, "no receiver fell back: {stats:?}");
    assert!(stats.drawn > 0, "receivers draw with the lightmap: {stats:?}");
    rgb
}

fn linear_image(rgb: &[[f32; 3]], q: Quality) -> LoadedImage {
    LoadedImage {
        width: q.width,
        height: q.height,
        rgb: rgb.to_vec(),
        linear: true,
        denoiser: None,
    }
}

/// `seeing.compare`'s scores (PSNR included) over the surface: outside `mask` the test reads the
/// reference.
fn compare_aov(reference: &[[f32; 3]], test: &[[f32; 3]], mask: &[bool], q: Quality) -> Value {
    let test: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let (report, _) = compare_images(
        &linear_image(reference, q),
        &linear_image(&test, q),
        EXPOSURE_EV,
        DEFAULT_PPD,
    )
    .expect("compare");
    json!({
        "flip_mean": report.flip_mean,
        "flip_p95": report.flip_p95,
        "ssim": report.ssim,
        "rel_rmse": if report.rel_rmse.is_finite() { json!(report.rel_rmse) } else { Value::Null },
        "psnr_db": psnr_db(&linear_image(reference, q), &linear_image(&test, q), EXPOSURE_EV),
    })
}

fn gi_json(e: &GiError) -> Value {
    json!({
        "pixels": e.pixels,
        "flip_mean": e.flip_mean,
        "rel_rmse": e.rel_rmse,
        "energy_ratio": e.energy_ratio,
        "shadow_bias": e.shadow_bias,
        "mean_reference": e.mean_reference,
        "mean_test": e.mean_test,
    })
}

fn encode_display(img: &[[f32; 3]]) -> Vec<u8> {
    display_encode(img, EXPOSURE_EV)
        .iter()
        .flat_map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8))
        .collect()
}

/// sRGB-encodes indirect light at the plate exposure + 2 EV (indirect light is dim).
fn encode_indirect(img: &[[f32; 3]]) -> Vec<u8> {
    let exposure = 4.0 / (1.2 * 2f32.powf(EXPOSURE_EV));
    let e = |v: f32| {
        let v = (v * exposure).clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0 + 0.5) as u8
    };
    img.iter().flat_map(|c| c.map(e)).collect()
}

fn flip_plate(reference: &[[f32; 3]], test: &[[f32; 3]], q: Quality) -> Vec<u8> {
    let (_, flip) = compare_images(
        &linear_image(reference, q),
        &linear_image(test, q),
        EXPOSURE_EV,
        DEFAULT_PPD,
    )
    .expect("compare for the FLIP plate");
    flip_heatmap_rgba8(&flip.map)
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn evidence_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(regia_core::brand::env_var_os(EVIDENCE_ENV)?);
    std::fs::create_dir_all(&dir).expect("create the evidence directory");
    Some(dir)
}

/// Writes RGB8 `planes` side by side as `dir/name`.
fn write_strip(dir: &Path, name: &str, planes: &[Vec<u8>], q: Quality) {
    let row = (q.width * 3) as usize;
    let mut rgb = Vec::with_capacity(row * planes.len() * q.height as usize);
    for y in 0..q.height as usize {
        for p in planes {
            rgb.extend_from_slice(&p[y * row..(y + 1) * row]);
        }
    }
    let img = image::RgbImage::from_raw(q.width * planes.len() as u32, q.height, rgb).expect("plate size");
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    regia_test_utils::write_atomic(&dir.join(name), bytes.get_ref()).expect("write the plate");
}

/// Worst seam figures over the bake's entries (`regia bake-lightmaps`' `seams`).
fn seams_summary(v: &Value) -> Value {
    let mut worst = 0.0f64;
    let mut edges = 0u64;
    let mut entries = Vec::new();
    for e in v["seams"]["entries"].as_array().into_iter().flatten() {
        if let Some(p95) = e["p95"].as_f64() {
            worst = worst.max(p95);
        }
        edges += e["edges"].as_u64().unwrap_or(0);
        entries.push(e.clone());
    }
    json!({ "worst_p95": worst, "edges": edges, "entries": entries })
}

/// Fewest cells a region needs in a view to be scored.
const MIN_REGION_CELLS: usize = 12;

/// The screen-space checks' cell size, metres: half a metre at the evidence size, a metre at the
/// gate's 160 × 96 (fewer pixels a cell).
fn screen_cell(q: Quality) -> f32 {
    if q.width >= 480 { 0.5 } else { 1.0 }
}

/// The gallery's large flat regions for the screen-space checks (`screen`), inset 0.25 m from
/// their corners.
fn screen_regions() -> Vec<screen::Region> {
    let (w, l, h) = (gallery::HALF_WIDTH, gallery::HALF_LENGTH, gallery::CEILING);
    let wall = [-l + 0.25, l - 0.25, 0.25, h - 0.25];
    vec![
        screen::Region {
            name: "east_wall",
            axis: 0,
            value: w,
            n: -Vec3::X,
            ab: (2, 1),
            rect: wall,
        },
        screen::Region {
            name: "west_wall",
            axis: 0,
            value: -w,
            n: Vec3::X,
            ab: (2, 1),
            rect: wall,
        },
        screen::Region {
            name: "floor_south",
            axis: 1,
            value: 0.0,
            n: Vec3::Y,
            ab: (0, 2),
            rect: [-w + 0.25, w - 0.25, -l + 0.25, -5.75],
        },
        screen::Region {
            name: "ceiling",
            axis: 1,
            value: h,
            n: -Vec3::Y,
            ab: (0, 2),
            rect: [-w + 0.25, w - 0.25, -l + 0.25, l - 0.25],
        },
    ]
}

/// The east wall's chart borders for the screen-space seam step (the same lines as
/// [`wall_borders`]).
fn screen_borders() -> Vec<screen::ScreenBorder> {
    let east = screen_regions()[0];
    let (az0, az1, ah, l) = (
        gallery::ADIT_Z.0,
        gallery::ADIT_Z.1,
        gallery::ADIT_HEIGHT,
        gallery::HALF_LENGTH,
    );
    let above = (ah + 0.25, gallery::CEILING - 0.25);
    let b = |name, across, at, run| screen::ScreenBorder {
        name,
        region: east,
        across,
        at,
        run,
    };
    vec![
        b("east_z8", 0, az1, above),
        b("east_z4", 0, az0, above),
        b("east_y4_5_south", 1, ah, (-l + 1.0, az0 - 0.5)),
        b("east_y4_5_north", 1, ah, (az1 + 0.5, l - 1.0)),
    ]
}

/// One view's scores: `(indirect error of the lightmap, of the ambient, per-view JSON)`.
fn score_view(
    view: View,
    reference: &Reference,
    meshes: &[CpuMesh],
    light: &Light,
    baked: (&Arc<Lightmap>, &[usize]),
    q: Quality,
    plates: Option<&Path>,
) -> (GiError, GiError, Value) {
    let (map, entries) = baked;
    let lab = lab_scene(meshes, view, q, light);
    let surface = &reference.surface;
    let direct = realtime_direct(&lab, q, None);
    let ambient = realtime_ambient(&lab, q);
    let lightmapped = realtime_lightmapped(&lab, map, entries, q);
    // ADR-4762: the same frame without the receivers' specular (the lightmap's own diffuse light,
    // what the diffuse checks hold), and at the evidence size the reflection-capture candidate.
    let lightmapped_off = realtime_lightmapped_with(&lab, map, entries, q, Specular::Off);
    let captured = plates.map(|_| {
        (
            realtime_lightmapped_with(&lab, map, entries, q, Specular::Capture(light.sky)),
            realtime_direct(&lab, q, Some(light.sky)),
        )
    });
    let score = |frame: &[[f32; 3]], direct: &[[f32; 3]]| {
        let test = sub_planes(frame, direct);
        let e = measure(&GiErrorInput {
            reference: &reference.indirect,
            test: &test,
            surface,
            shadow: &reference.shadow,
            width: q.width,
            height: q.height,
            ev100: EXPOSURE_EV,
            scale: None,
        });
        (e, test)
    };
    let (lm, lm_indirect) = score(&lightmapped, &direct);
    let (lm_off, lm_off_indirect) = score(&lightmapped_off, &direct);
    let captured = captured.map(|(c, capture_direct)| {
        let (e, indirect) = score(&c, &capture_direct);
        (e, indirect, c)
    });
    let (amb, amb_indirect) = score(&ambient, &direct);
    // The screen-space checks (ADR-4108): per region, against the tracer at this frame.
    let planes = screen::Planes {
        hits: &reference.hits,
        frame_indirect: &lm_indirect,
        reference_indirect: &reference.indirect,
        reference_a: &reference.indirect_a,
        reference_diffuse: &reference.indirect_diffuse,
        frame_beauty: &lightmapped,
        reference_beauty: &reference.beauty,
    };
    let planes_off = screen::Planes {
        frame_indirect: &lm_off_indirect,
        frame_beauty: &lightmapped_off,
        ..planes
    };
    let cell = screen_cell(q);
    let mut regions = serde_json::Map::new();
    for region in screen_regions() {
        let scores = screen::score_region(&region, &planes, cell);
        if scores.cells < MIN_REGION_CELLS {
            continue;
        }
        let mut row = scores.json;
        // The lightmap's own light against the tracer's diffuse lobe is read without the
        // receivers' specular (ADR-4762); with it, the whole indirect light is the tracer's whole.
        let off = screen::score_region(&region, &planes_off, cell).json;
        row["indirect_diffuse_colour"] = off["indirect_diffuse_colour"].clone();
        row["specular_off"] = json!({
            "indirect_colour": off["indirect_colour"],
            "beauty_colour": off["beauty_colour"],
        });
        if let Some((_, indirect, beauty)) = &captured {
            let c = screen::score_region(
                &region,
                &screen::Planes {
                    frame_indirect: indirect,
                    frame_beauty: beauty,
                    ..planes
                },
                cell,
            )
            .json;
            row["capture"] = json!({
                "indirect_colour": c["indirect_colour"],
                "beauty_colour": c["beauty_colour"],
            });
        }
        // What the frame's sun-only image adds where the sun never reaches (the tracer's direct
        // light is zero there): a share of the lightmapped frame's luminance.
        let mask: Vec<bool> = reference
            .hits
            .iter()
            .zip(&reference.shadow)
            .map(|(h, s)| *s && region.coords(*h).is_some())
            .collect();
        if mask.iter().any(|m| *m) {
            let sum = |img: &[[f32; 3]]| -> f64 {
                img.iter()
                    .zip(&mask)
                    .filter(|(_, m)| **m)
                    .map(|(c, _)| f64::from(luminance(*c)))
                    .sum()
            };
            let frame_sum = sum(&lightmapped);
            row["sun_only_share_in_shadow"] = json!((frame_sum > 0.0).then(|| sum(&direct) / frame_sum));
            row["sun_only_colour_in_shadow"] = json!(screen::mean_rgb(&direct, &mask));
        }
        regions.insert(region.name.into(), row);
    }
    let mut seams_screen = serde_json::Map::new();
    for border in screen_borders() {
        seams_screen.insert(
            border.name.into(),
            screen::seam_step(&border, &reference.hits, &lm_indirect, &reference.indirect, 8),
        );
    }
    let row = json!({
        "view": view.name,
        "eye": view.eye,
        "target": view.target,
        "surface_pixels": surface.iter().filter(|m| **m).count(),
        "reference_noise_floor": gi_json(&reference.noise_floor),
        "indirect": {
            "lightmap": gi_json(&lm),
            "lightmap_specular_off": gi_json(&lm_off),
            "capture": captured.as_ref().map(|(e, _, _)| gi_json(e)),
            "ambient": gi_json(&amb),
        },
        "indirect_compare": {
            "lightmap": compare_aov(&reference.indirect, &lm_indirect, surface, q),
            "lightmap_specular_off": compare_aov(&reference.indirect, &lm_off_indirect, surface, q),
            "capture": captured.as_ref().map(|(_, i, _)| compare_aov(&reference.indirect, i, surface, q)),
            "ambient": compare_aov(&reference.indirect, &amb_indirect, surface, q),
        },
        "beauty": {
            "lightmap": compare_aov(&reference.beauty, &lightmapped, surface, q),
            "lightmap_specular_off": compare_aov(&reference.beauty, &lightmapped_off, surface, q),
            "capture": captured.as_ref().map(|(_, _, b)| compare_aov(&reference.beauty, b, surface, q)),
            "ambient": compare_aov(&reference.beauty, &ambient, surface, q),
        },
        "direct": compare_aov(&reference.direct, &direct, surface, q),
        "regions": regions,
        "seam_step_screen": seams_screen,
    });
    if let Some(dir) = plates {
        // Beauty: path tracer | lightmap | flat ambient.
        write_strip(
            dir,
            &format!("{}_beauty.png", view.name),
            &[
                encode_display(&reference.beauty),
                encode_display(&lightmapped),
                encode_display(&ambient),
            ],
            q,
        );
        write_strip(
            dir,
            &format!("{}_indirect.png", view.name),
            &[
                encode_indirect(&reference.indirect),
                encode_indirect(&lm_indirect),
                encode_indirect(&amb_indirect),
            ],
            q,
        );
        // ADR-4762, path tracer | the receivers' specular off | from the lightmap | from a capture.
        let mut beauty = vec![
            encode_display(&reference.beauty),
            encode_display(&lightmapped_off),
            encode_display(&lightmapped),
        ];
        let mut indirect = vec![
            encode_indirect(&reference.indirect),
            encode_indirect(&lm_off_indirect),
            encode_indirect(&lm_indirect),
        ];
        if let Some((_, i, b)) = &captured {
            beauty.push(encode_display(b));
            indirect.push(encode_indirect(i));
        }
        write_strip(dir, &format!("{}_beauty_specular.png", view.name), &beauty, q);
        write_strip(dir, &format!("{}_indirect_specular.png", view.name), &indirect, q);
        // The sun alone, both sides: path tracer | the frame's sun and shadows.
        write_strip(
            dir,
            &format!("{}_direct.png", view.name),
            &[encode_display(&reference.direct), encode_display(&direct)],
            q,
        );
        let masked = |t: &[[f32; 3]]| -> Vec<[f32; 3]> {
            t.iter()
                .zip(&reference.indirect)
                .zip(surface)
                .map(|((t, r), m)| if *m { *t } else { *r })
                .collect()
        };
        write_strip(
            dir,
            &format!("{}_indirect_flip.png", view.name),
            &[
                flip_plate(&reference.indirect, &masked(&lm_indirect), q),
                flip_plate(&reference.indirect, &masked(&amb_indirect), q),
            ],
            q,
        );
    }
    (lm, amb, row)
}

/// The lightmap's bytes as each storage would ship them (RGB9E5 as baked, BC6H, ASTC HDR), and how
/// long the BC6H file takes to decode from memory (reported, not asserted; a load's CPU share).
fn storage_bytes(map: &Lightmap) -> Value {
    let rgb9e5 = map.to_bytes().map(|b| b.len()).unwrap_or(0);
    let mut out = json!({ "texels": map.texels.len(), "rgb9e5": rgb9e5 });
    for (name, enc) in [
        ("bc6h", LightmapTexelEncoding::Bc6h),
        ("astc_hdr", LightmapTexelEncoding::AstcHdr),
    ] {
        let mut m = map.clone();
        out[name] = match m.encode_texels(enc).and_then(|_| m.to_bytes()) {
            Ok(b) => {
                if enc == LightmapTexelEncoding::Bc6h {
                    let started = std::time::Instant::now();
                    let decoded = Lightmap::from_bytes(&b).is_ok();
                    out["bc6h_decode_ms"] = json!(decoded.then(|| started.elapsed().as_secs_f64() * 1000.0));
                }
                json!(b.len())
            }
            Err(e) => json!(format!("{e}")),
        };
    }
    out
}

// --- Lightmap-space checks (ADR-4108) -----------------------------------------------------------

/// The floor's part index in [`gallery::parts`].
const PART_FLOOR: usize = 0;
/// The gallery rock's part index (walls and ceiling).
const PART_GALLERY: usize = 1;

/// The flat regions the lightmap-space checks read, with their part: each single chart wall face
/// (so the mottle is the light's, not a chart border's), inset 0.25 m from the corners.
fn wall_grids() -> Vec<(usize, lmspace::WallGrid)> {
    let (w, l) = (gallery::HALF_WIDTH, gallery::HALF_LENGTH);
    let steps = |m: f32| (m / lmspace::STEP_M).round() as usize + 1;
    vec![
        // The west wall is one block: one chart, 32 × 11 m.
        (
            PART_GALLERY,
            lmspace::WallGrid {
                name: "west_wall",
                origin: Vec3::new(-w, 0.25, -l + 0.25),
                u: Vec3::Z,
                v: Vec3::Y,
                n: Vec3::X,
                na: steps(2.0 * l - 0.5),
                nb: steps(gallery::CEILING - 0.5),
            },
        ),
        // The east wall's upper north block (z 8..16, y 4.5..11): one chart beside the adit.
        (
            PART_GALLERY,
            lmspace::WallGrid {
                name: "east_wall_upper_north",
                origin: Vec3::new(w, gallery::ADIT_HEIGHT + 0.25, gallery::ADIT_Z.1 + 0.25),
                u: Vec3::Z,
                v: Vec3::Y,
                n: -Vec3::X,
                na: steps(l - gallery::ADIT_Z.1 - 0.5),
                nb: steps(gallery::CEILING - gallery::ADIT_HEIGHT - 0.5),
            },
        ),
        // The floor's south part, nearest the main camera, away from the sunlit pool.
        (
            PART_FLOOR,
            lmspace::WallGrid {
                name: "floor_south",
                origin: Vec3::new(-w + 0.25, 0.0, -l + 0.25),
                u: Vec3::X,
                v: Vec3::Z,
                n: Vec3::Y,
                na: steps(2.0 * w - 0.5),
                nb: steps(10.0),
            },
        ),
    ]
}

/// The chart borders the seam check reads on the east wall (a grid of blocks around the adit, so
/// its inner face is five charts), and two controls on the west wall (one chart) at the same
/// heights, where the light is continuous by construction.
fn wall_borders() -> Vec<(usize, lmspace::Border)> {
    let (w, l) = (gallery::HALF_WIDTH, gallery::HALF_LENGTH);
    let (az0, az1, ah) = (gallery::ADIT_Z.0, gallery::ADIT_Z.1, gallery::ADIT_HEIGHT);
    let above = gallery::CEILING - ah - 0.5;
    let border = |name, start: Vec3, along, length, across, n| {
        (
            PART_GALLERY,
            lmspace::Border {
                name,
                start,
                along,
                length,
                across,
                n,
            },
        )
    };
    vec![
        border(
            "east_z8",
            Vec3::new(w, ah + 0.25, az1),
            Vec3::Y,
            above,
            Vec3::Z,
            -Vec3::X,
        ),
        border(
            "east_z4",
            Vec3::new(w, ah + 0.25, az0),
            Vec3::Y,
            above,
            Vec3::Z,
            -Vec3::X,
        ),
        border(
            "east_y4_5_south",
            Vec3::new(w, ah, -l + 1.0),
            Vec3::Z,
            az0 - 0.5 - (-l + 1.0),
            Vec3::Y,
            -Vec3::X,
        ),
        border(
            "east_y4_5_north",
            Vec3::new(w, ah, az1 + 1.0),
            Vec3::Z,
            l - 1.0 - (az1 + 1.0),
            Vec3::Y,
            -Vec3::X,
        ),
        border(
            "control_west_z8",
            Vec3::new(-w, ah + 0.25, az1),
            Vec3::Y,
            above,
            Vec3::Z,
            Vec3::X,
        ),
        border(
            "control_west_y4_5",
            Vec3::new(-w, ah, -l + 1.0),
            Vec3::Z,
            2.0 * l - 2.0,
            Vec3::Y,
            Vec3::X,
        ),
    ]
}

/// The lightmap-space figures of a bake (`lmspace`): the mottle and mean colour of each region of
/// [`wall_grids`] and the seam step across each border of [`wall_borders`].
fn lightmap_space_report(map: &Lightmap, entries: &[usize], meshes: &[CpuMesh]) -> Value {
    let geometry: Vec<lmspace::EntryGeometry> = entries
        .iter()
        .zip(meshes)
        .map(|(&e, m)| lmspace::EntryGeometry::new(map, e, m))
        .collect();
    let mut mottle = serde_json::Map::new();
    let mut colour = serde_json::Map::new();
    for (part, grid) in wall_grids() {
        let samples = grid.sample(map, &geometry[part]);
        let on = samples.iter().filter(|s| s.is_some()).count();
        assert!(
            on * 10 >= samples.len() * 9,
            "{}: only {on} of {} samples on the face",
            grid.name,
            samples.len()
        );
        mottle.insert(
            grid.name.into(),
            json!(lmspace::mottle(&samples, grid.na, grid.nb)),
        );
        colour.insert(grid.name.into(), json!(lmspace::mean_colour(&samples)));
    }
    let mut seams = serde_json::Map::new();
    let mut edges = serde_json::Map::new();
    for (part, border) in wall_borders() {
        let geo = &geometry[part];
        let step = lmspace::seam_step(|p, n| geo.at(map, p, n), &border);
        seams.insert(border.name.into(), json!(step));
        let edge = lmspace::edge_step(|p, n| geo.at(map, p, n), &border);
        edges.insert(border.name.into(), json!(edge));
    }
    json!({ "mottle": mottle, "seam_step": seams, "edge_step": edges, "mean_irradiance": colour })
}

/// Largest hard step where two of the east wall's charts meet (lookups 2 cm either side, relative):
/// the old per-chart denoiser left up to 1.6 % (ADR-4108); a wall's own light changes by about
/// 0.35 % over those 4 cm.
const MAX_EDGE_STEP: f64 = 0.01;
/// Largest step between 1 m line fits either side of a chart border (relative): the old denoiser
/// left 3 % to 8.5 % at the HIGH tier (ADR-4108); a border-free line on the west wall reads 0.5 % to
/// 1 %.
const MAX_FIT_STEP: f64 = 0.025;
/// Largest wall mottle (the 0.5 m over 3 m band-pass, relative RMS) on a single-chart wall.
const MAX_MOTTLE: f64 = 0.035;

/// Largest mottle a region may show beyond the tracer's own noise at the same cells (the frame's
/// cell-ratio spread with the tracer's noise taken out in quadrature): the old denoiser's
/// patches read 3.4 % to 3.6 % on the walls at the evidence size (ADR-4108).
const MAX_EXCESS_MOTTLE: f64 = 0.03;
/// Largest screen-space step at an east-wall chart border beyond the tracer's own step there.
const MAX_SCREEN_STEP_EXCESS: f64 = 0.02;
/// The lightmap's indirect light against the tracer's diffuse lobe (what a lightmap holds), per
/// region: the luminance ratio's band and the largest chromaticity error.
const DIFFUSE_LUMINANCE_BAND: (f64, f64) = (0.92, 1.08);
/// See [`DIFFUSE_LUMINANCE_BAND`].
const MAX_DIFFUSE_CHROMA_ERROR: f64 = 0.025;

/// Largest `B/G` error of a region's beauty against the tracer's (ADR-4757). The outdoor aerial
/// perspective inside the gallery read +4 % to +13 % here; what is left with the interior boxes is
/// the lightmap's own color (its indirect `B/G` against the tracer's whole indirect, −1.2 % to
/// −1.5 % at the evidence settings) and the tracer's noise.
const MAX_BEAUTY_BG_ERROR: f64 = 0.03;
/// Largest share of a region's light, in the sun's shadow, that the sun-only frame (no sky, no GI)
/// shows: the aerial in-scatter put 2.3 % to 5.3 % there (ADR-4108); with the interior boxes only
/// shadow filtering at the region's edges is left.
const MAX_SUN_ONLY_SHARE_IN_SHADOW: f64 = 0.005;

/// The regions whose flatness is held to [`MAX_EXCESS_MOTTLE`]: the large flat-lit walls and the
/// floor. The ceiling holds the skylight's opening, whose sharp light spreads its cell ratios for
/// reasons other than mottle; it is reported, not held.
const FLAT_REGIONS: [&str; 3] = ["east_wall", "west_wall", "floor_south"];
/// The frame's whole indirect light (the lightmap's diffuse plus the receivers' specular,
/// ADR-4762) against the tracer's whole indirect light, per region in [`WHOLE_INDIRECT_REGIONS`]
/// at the evidence size (luminance ratio).
const WHOLE_INDIRECT_BAND: (f64, f64) = (0.95, 1.05);
/// The regions [`WHOLE_INDIRECT_BAND`] holds: the floor, which the tracer lights about 12 % by
/// its specular lobe (ADR-4108 § 7(b)).
const WHOLE_INDIRECT_REGIONS: [&str; 1] = ["floor_south"];

/// The screen-space figures of one view (`screen`) against the limits above, at the evidence size
/// only (the gate's 160 × 96 at 96 spp leaves the tracer's noise far above what these limits
/// test; it reports them): one message per failure.
fn check_screen(row: &Value, q: Quality) -> Vec<String> {
    let mut failures = Vec::new();
    if q.width < 480 {
        return failures;
    }
    let view = row["view"].as_str().unwrap_or("?");
    if view == VIEWS[0].name {
        for region in WHOLE_INDIRECT_REGIONS {
            if row["regions"].get(region).is_none() {
                failures.push(format!("{view}: required floor region {region} was not measured"));
            }
        }
    }
    for (name, r) in row["regions"].as_object().into_iter().flatten() {
        let f = &r["flatness"];
        if let (Some(spread), Some(noise)) = (f["ratio_spread"].as_f64(), f["reference_noise"].as_f64())
            && FLAT_REGIONS.contains(&name.as_str())
        {
            let excess = (spread * spread - noise * noise).max(0.0).sqrt();
            if excess > MAX_EXCESS_MOTTLE {
                failures.push(format!(
                    "{view} {name}: mottle {excess} beyond the tracer's noise ({spread} spread, {noise} noise)"
                ));
            }
        }
        let c = &r["indirect_diffuse_colour"];
        if let (Some(l), Some(chroma)) = (c["luminance_ratio"].as_f64(), c["chroma_error"].as_f64()) {
            if !(DIFFUSE_LUMINANCE_BAND.0..=DIFFUSE_LUMINANCE_BAND.1).contains(&l) {
                failures.push(format!(
                    "{view} {name}: indirect {l} of the tracer's diffuse light"
                ));
            }
            if chroma > MAX_DIFFUSE_CHROMA_ERROR {
                failures.push(format!("{view} {name}: chromaticity off by {chroma}"));
            }
        }
        if WHOLE_INDIRECT_REGIONS.contains(&name.as_str()) {
            match r["indirect_colour"]["luminance_ratio"].as_f64() {
                Some(l) if (WHOLE_INDIRECT_BAND.0..=WHOLE_INDIRECT_BAND.1).contains(&l) => {}
                Some(l) => failures.push(format!(
                    "{view} {name}: whole indirect {l} of the tracer's (diffuse and specular)"
                )),
                None => failures.push(format!("{view} {name}: no whole indirect ratio measured")),
            }
        }
    }
    for (name, s) in row["seam_step_screen"].as_object().into_iter().flatten() {
        if let (Some(frame), Some(reference)) = (s["frame"].as_f64(), s["reference"].as_f64())
            && frame > reference + MAX_SCREEN_STEP_EXCESS
        {
            failures.push(format!(
                "{view} {name}: step {frame} at a chart border, the tracer's {reference}"
            ));
        }
    }
    failures
}

/// The interior air checks of one view (ADR-4757), at every size: each region's beauty `B/G`
/// against the tracer's within [`MAX_BEAUTY_BG_ERROR`], and the sun-only frame's share in the sun's
/// shadow within [`MAX_SUN_ONLY_SHARE_IN_SHADOW`]. One message per failure.
fn check_interior_air(row: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    let view = row["view"].as_str().unwrap_or("?");
    for (name, r) in row["regions"].as_object().into_iter().flatten() {
        if let Some(bg) = r["beauty_colour"]["bg_error"].as_f64()
            && bg.abs() > MAX_BEAUTY_BG_ERROR
        {
            failures.push(format!(
                "{view} {name}: beauty B/G off the tracer's by {bg} (limit {MAX_BEAUTY_BG_ERROR})"
            ));
        }
        if let Some(share) = r["sun_only_share_in_shadow"].as_f64()
            && share > MAX_SUN_ONLY_SHARE_IN_SHADOW
        {
            failures.push(format!(
                "{view} {name}: the sun-only frame shows {share} of the light in the sun's shadow (limit {MAX_SUN_ONLY_SHARE_IN_SHADOW})"
            ));
        }
    }
    failures
}

/// Per region: the beauty's `B/G` error against the tracer's and the sun-only share in shadow.
fn interior_air_summary(row: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (name, r) in row["regions"].as_object().into_iter().flatten() {
        out.insert(
            name.clone(),
            json!({
                "beauty_bg_error": r["beauty_colour"]["bg_error"],
                "beauty_rg_error": r["beauty_colour"]["rg_error"],
                "sun_only_share_in_shadow": r["sun_only_share_in_shadow"],
            }),
        );
    }
    Value::Object(out)
}

/// Fewest samples a texel takes for the absolute lightmap-space limits (the evidence's 256): below
/// it the bake's own noise sets the figures, so the borders are held against the border-free
/// control lines instead and the mottle is reported.
const ABSOLUTE_LIMIT_SAMPLES: u32 = 256;
/// At fewer samples, a border's fit step may be at most this many times the larger control line's.
const STEP_OVER_CONTROL: f64 = 3.0;

/// The lightmap-space figures (`lightmap_space_report`) against the limits above for a bake of
/// `samples` per texel: one message per failure (a figure that could not be measured is one).
fn check_lightmap_space(v: &Value, samples: u32) -> Vec<String> {
    let mut failures = Vec::new();
    let control = v["seam_step"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.starts_with("control"))
        .filter_map(|(_, x)| x.as_f64())
        .fold(0.0f64, f64::max);
    let absolute = samples >= ABSOLUTE_LIMIT_SAMPLES;
    let fit_limit = if absolute {
        MAX_FIT_STEP
    } else {
        STEP_OVER_CONTROL * control
    };
    let mut limits = vec![
        ("edge_step", MAX_EDGE_STEP, "hard step at a chart border"),
        ("seam_step", fit_limit, "step between the charts' light"),
    ];
    if absolute {
        limits.push(("mottle", MAX_MOTTLE, "mottle"));
    }
    for (key, limit, what) in limits {
        for (name, x) in v[key].as_object().into_iter().flatten() {
            if name.starts_with("control") {
                continue;
            }
            match x.as_f64() {
                Some(x) if x <= limit => {}
                Some(x) => failures.push(format!("{name}: {what} {x} over {limit}")),
                None => failures.push(format!("{name}: no {what} measured")),
            }
        }
    }
    failures
}

/// The base bake arguments of the study: the CLI's defaults at the evidence's samples and bounces.
const STUDY_BASE: [&str; 6] = ["--samples", "256", "--bounces", "6", "--threads", "0"];
/// `name=flags;name=flags`: the study's variants (flags over [`STUDY_BASE`]).
const STUDY_ENV: &str = "REGIA_GALLERY_STUDY";

/// A directory: the study also writes each bake's east and west walls, as the runtime's lookup
/// reads them, as `<variant>_walls.png` (8 samples a metre, each wall scaled to its own mean).
const STUDY_DUMP_ENV: &str = "REGIA_GALLERY_STUDY_DUMP";

/// Writes the gallery's east (top) and west (bottom) walls of `map` as a grey image.
fn dump_walls(dir: &Path, name: &str, map: &Lightmap, entries: &[usize], meshes: &[CpuMesh]) {
    let geo = lmspace::EntryGeometry::new(map, entries[PART_GALLERY], &meshes[PART_GALLERY]);
    let (w, l, h) = (gallery::HALF_WIDTH, gallery::HALF_LENGTH, gallery::CEILING);
    let (nz, ny) = ((2.0 * l * 8.0) as usize, (h * 8.0) as usize);
    let mut rows: Vec<Vec<u8>> = Vec::new();
    for (x, n) in [(w, -Vec3::X), (-w, Vec3::X)] {
        let vals: Vec<Option<f32>> = (0..ny)
            .rev()
            .flat_map(|j| (0..nz).map(move |i| (i, j)))
            .map(|(i, j)| {
                let p = Vec3::new(x, (j as f32 + 0.5) / 8.0, -l + (i as f32 + 0.5) / 8.0);
                geo.at(map, p, n).map(lmspace::lum)
            })
            .collect();
        let known: Vec<f32> = vals.iter().flatten().copied().collect();
        let mean = known.iter().sum::<f32>() / known.len().max(1) as f32;
        for r in vals.chunks(nz) {
            rows.push(
                r.iter()
                    .map(|v| v.map_or(0, |v| ((v / mean * 0.5).clamp(0.0, 1.0) * 255.0) as u8))
                    .collect(),
            );
        }
        rows.push(vec![255; nz]);
    }
    let img = image::GrayImage::from_raw(nz as u32, rows.len() as u32, rows.concat()).expect("image size");
    img.save(dir.join(format!("{name}_walls.png")))
        .expect("write the wall dump");
}

/// Diagnostic (ADR-4108), CPU only: bakes the gallery once per variant of `REGIA_GALLERY_STUDY`
/// (for example `base=;nodenoise=--denoise off;s1024=--samples 1024`) and prints each bake's
/// lightmap-space figures and time as one JSON line. Without the variable it does nothing.
#[test]
fn gallery_lightmap_space_study() {
    let Some(spec) = regia_core::brand::env_var_os(STUDY_ENV) else {
        return;
    };
    let spec = spec.to_string_lossy().to_string();
    let tmp = project("lightmap_gallery_study");
    let dir = &tmp.0;
    let parts = gallery::parts();
    let meshes: Vec<CpuMesh> = parts.iter().map(|p| imported(dir, p)).collect();
    for variant in spec.split(';').filter(|v| !v.trim().is_empty()) {
        let (name, flags) = variant.split_once('=').unwrap_or((variant, ""));
        // `name=@path`: measure a lightmap baked elsewhere (an older build's bake) instead.
        if let Some(path) = flags.trim().strip_prefix('@') {
            let map = Lightmap::read(Path::new(path)).expect("read the lightmap");
            let entries: Vec<usize> = parts
                .iter()
                .map(|p| map.entry_for_key(entry_key(p)).expect("an entry per part"))
                .collect();
            let row = json!({
                "variant": name.trim(),
                "flags": flags.trim(),
                "seconds": Value::Null,
                "lightmap_space": lightmap_space_report(&map, &entries, &meshes),
            });
            eprintln!("STUDY {row}");
            if let Some(dump) = regia_core::brand::env_var_os(STUDY_DUMP_ENV) {
                dump_walls(Path::new(&dump), name.trim(), &map, &entries, &meshes);
            }
            continue;
        }
        let out = format!("assets/maps/study_{}.flmap", name.trim());
        // A variant's flag replaces the base's value for it.
        let own: Vec<&str> = flags.split_whitespace().collect();
        let mut args: Vec<&str> = STUDY_BASE
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|pair| !own.contains(&pair[0]))
            .flatten()
            .copied()
            .collect();
        args.extend(&own);
        let v = bake(dir, &out, &args);
        let map = Lightmap::read(&dir.join(&out)).expect("read the bake");
        let entries: Vec<usize> = parts
            .iter()
            .map(|p| map.entry_for_key(entry_key(p)).expect("an entry per part"))
            .collect();
        let row = json!({
            "variant": name.trim(),
            "flags": flags.trim(),
            "seconds": v["seconds"],
            "noise": v["report"]["noise"],
            "denoised_noise": v["report"]["denoised_noise"],
            "stitched": [v["report"]["seam_edges_stitched"], v["report"]["texels_stitched"]],
            "lightmap_space": lightmap_space_report(&map, &entries, &meshes),
        });
        eprintln!("STUDY {row}");
        if let Some(dump) = regia_core::brand::env_var_os(STUDY_DUMP_ENV) {
            dump_walls(Path::new(&dump), name.trim(), &map, &entries, &meshes);
        }
    }
}

#[test]
fn gallery_lightmap_against_the_path_tracer() {
    let plates = evidence_dir();
    let q = quality(plates.is_some());
    let tmp = project("lightmap_gallery_plate");
    let dir = &tmp.0;
    let parts = gallery::parts();
    let meshes: Vec<CpuMesh> = parts.iter().map(|p| imported(dir, p)).collect();
    let light = level_light();

    let bake_at = |density: f32, directional: bool, out: &str| -> (Value, Arc<Lightmap>, Vec<usize>) {
        let (d, s, b) = (density.to_string(), q.samples.to_string(), q.bounces.to_string());
        let mut args = vec![
            "--density",
            d.as_str(),
            "--samples",
            s.as_str(),
            "--bounces",
            b.as_str(),
            "--threads",
            "0",
        ];
        if !directional {
            args.push("--no-directional");
        }
        let v = bake(dir, out, &args);
        let map = Lightmap::read(&dir.join(out)).expect("read the bake");
        let entries: Vec<usize> = parts
            .iter()
            .map(|p| map.entry_for_key(entry_key(p)).expect("an entry per part"))
            .collect();
        (v, Arc::new(map), entries)
    };

    let (bake_json, map, entries) = bake_at(q.density, true, "assets/maps/gallery.flmap");
    let seams = seams_summary(&bake_json);
    let references: Vec<Reference> = VIEWS
        .iter()
        .map(|view| reference(&lab_scene(&meshes, *view, q, &light), *view, &light, q))
        .collect();
    let mut views = Vec::new();
    // The look checks' failures (ADR-4108), asserted after the evidence is written.
    let mut failures: Vec<String> = Vec::new();
    for (view, reference) in VIEWS.into_iter().zip(&references) {
        let (lm, amb, row) = score_view(
            view,
            reference,
            &meshes,
            &light,
            (&map, &entries),
            q,
            plates.as_deref(),
        );
        eprintln!(
            "{}: lightmap indirect FLIP {:.3} (reference noise floor {:.3}, p95 {}, energy {:?}, rel RMSE {:.3}); ambient FLIP {:.3} (energy {:?})",
            view.name,
            lm.flip_mean,
            reference.noise_floor.flip_mean,
            row["indirect_compare"]["lightmap"]["flip_p95"],
            lm.energy_ratio,
            lm.rel_rmse,
            amb.flip_mean,
            amb.energy_ratio
        );
        assert!(lm.pixels > 0, "{}: the view sees the gallery", view.name);
        assert!(
            lm.flip_mean < amb.flip_mean,
            "{}: the lightmap is closer to the tracer than the flat ambient ({} vs {})",
            view.name,
            lm.flip_mean,
            amb.flip_mean
        );
        let off = |e: Option<f32>| (e.expect("an energy ratio") - 1.0).abs();
        assert!(
            off(lm.energy_ratio) < off(amb.energy_ratio),
            "{}: the lightmap's indirect energy is nearer the tracer's ({:?} vs {:?})",
            view.name,
            lm.energy_ratio,
            amb.energy_ratio
        );
        for (name, r) in row["regions"].as_object().into_iter().flatten() {
            eprintln!(
                "{} {name}: whole indirect / tracer's {} (specular off {}, capture {}); lightmap / tracer's diffuse {}",
                view.name,
                r["indirect_colour"]["luminance_ratio"],
                r["specular_off"]["indirect_colour"]["luminance_ratio"],
                r["capture"]["indirect_colour"]["luminance_ratio"],
                r["indirect_diffuse_colour"]["luminance_ratio"],
            );
        }
        failures.extend(check_screen(&row, q));
        failures.extend(check_interior_air(&row));
        eprintln!("{}: interior air {}", view.name, interior_air_summary(&row));
        views.push(row);
    }
    let worst_seam = seams["worst_p95"].as_f64().unwrap_or(0.0);
    assert!(
        worst_seam <= SHIPPING_SEAM_BUDGET,
        "chart seams p95 {worst_seam} over the shipping budget"
    );
    // The lightmap-space checks (ADR-4108), on the bake itself: no step where the east wall's
    // charts meet, and walls without patches the light does not have.
    let lightmap_space = lightmap_space_report(&map, &entries, &meshes);
    eprintln!("lightmap space: {lightmap_space}");
    failures.extend(check_lightmap_space(&lightmap_space, q.samples));

    let Some(dir_out) = plates else {
        assert!(
            failures.is_empty(),
            "the gallery's look checks:\n{}",
            failures.join("\n")
        );
        return;
    };
    // The tier sweep: what each package's density costs in bytes and gives on the main view.
    let mut tiers = Vec::new();
    for (tier, density, directional) in TIER_SWEEP {
        let out = format!("assets/maps/gallery_{tier}.flmap");
        let (v, m, e) = bake_at(density, directional, &out);
        let (lm, _, row) = score_view(VIEWS[0], &references[0], &meshes, &light, (&m, &e), q, None);
        let floor = &row["regions"]["floor_south"];
        tiers.push(json!({
            "tier": tier,
            "density": density,
            "directional": directional,
            "atlas": v["atlas"],
            "bytes": storage_bytes(&m),
            "seams": seams_summary(&v)["worst_p95"],
            "bake_seconds": v["seconds"],
            "main_view_indirect": gi_json(&lm),
            "floor_south_indirect_ratio": {
                "specular": floor["indirect_colour"]["luminance_ratio"],
                "specular_off": floor["specular_off"]["indirect_colour"]["luminance_ratio"],
            },
        }));
    }
    let info = device().adapter.get_info();
    let adapter = format!("{} ({}; {})", info.name, info.driver, info.driver_info);
    let out = json!({
        "scene": "gallery",
        "adapter": adapter,
        "quality": {
            "spp": q.spp, "samples": q.samples, "bounces": q.bounces, "density": q.density,
            "width": q.width, "height": q.height, "exposure_ev": EXPOSURE_EV,
        },
        "targets": { "flip_mean": FLIP_TARGET, "flip_p95": FLIP_P95_TARGET, "seam_budget": SHIPPING_SEAM_BUDGET },
        "sun": { "direction": light.sun.direction, "illuminance": light.sun.illuminance, "angular_radius": light.sun_angular_radius },
        "sky_radiance": light.sky,
        "bake": {
            "atlas": bake_json["atlas"],
            "report": bake_json["report"],
            "settings": bake_json["settings"],
            "bytes": storage_bytes(&map),
        },
        "seams": seams,
        "lightmap_space": lightmap_space,
        "views": views,
        "tiers": tiers,
    });
    let text = serde_json::to_string_pretty(&out).expect("JSON");
    regia_test_utils::write_atomic(&dir_out.join("gallery.json"), text.as_bytes())
        .expect("write the numbers");
    eprintln!("{text}");
    assert!(
        failures.is_empty(),
        "the gallery's look checks:\n{}",
        failures.join("\n")
    );
}

/// The acceptance check must catch the pre-feature dark floor, excess energy and missing data,
/// even in the quick CI suite, which does not pay for a high-sample path-traced plate.
#[test]
fn the_floor_luminance_check_rejects_dark_bright_and_missing_evidence() {
    let q = Quality {
        spp: 4096,
        samples: 256,
        bounces: 6,
        density: 8.0,
        width: 640,
        height: 384,
    };
    let row = |ratio| {
        json!({
            "view": "gallery_main",
            "regions": { "floor_south": { "indirect_colour": { "luminance_ratio": ratio } } },
        })
    };
    assert!(check_screen(&row(1.0), q).is_empty());
    for ratio in [0.90, 1.16] {
        let failures = check_screen(&row(ratio), q);
        assert!(
            failures.iter().any(|s| s.contains("whole indirect")),
            "{failures:?}"
        );
    }
    assert!(!check_screen(&json!({ "view": "gallery_main", "regions": {} }), q).is_empty());
    let missing = json!({ "view": "gallery_main", "regions": { "floor_south": {} } });
    assert!(!check_screen(&missing, q).is_empty());
}
