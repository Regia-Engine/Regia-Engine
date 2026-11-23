// SPDX-License-Identifier: MPL-2.0
//! Capsule occluders over a lightmap receiver (ADR-2656, `specs/lightmaps.md` § 7). The baked screen
//! pass writes one visibility word per texel while capsules are set, and a receiver multiplies its
//! baked diffuse term by it, so a parked vehicle darkens a lightmapped floor too.
//!
//! The frame is compared with the CPU's [`capsule_visibility`] read the way the frame reads it: at the
//! half-resolution lookup's texels, mixed with the forward pass's weights (`lookup_visibility`). The receiver
//! is the only light in the frame (no sun, no sky map, a volume that holds nothing), so a pixel's
//! light is the lightmap's irradiance times the visibility. The tone curve between light and bytes
//! is read from frames of the same scene with the lightmap's intensity scaled, so a pixel's bytes
//! turn back into a visibility with no assumption about the post stack. No golden image: every check
//! compares frames rendered here (lavapipe, deterministic). Each path's renderer is kept across its
//! frames: compiling the forward pipelines once per frame is what makes such a test slow.

use std::sync::{Arc, OnceLock};

use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiVolume, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta, TerrainLayer,
    VolumeLayout,
};
use regia_asset::lightmap::{
    Coverage, Lightmap, LightmapBakeInfo, LightmapContents, LightmapEntry, LightmapLayout, MeshUnwrap,
};
use regia_asset_pipeline::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_core::glam::camera::rh::proj::directx::perspective;
use regia_core::glam::camera::rh::view::look_at_mat4;
use regia_core::glam::{Vec3, Vec4};
use regia_render::baked_gi::{GiCapsule, capsule_visibility};
use regia_render::forward_lite::ForwardLite;
use regia_render::lightmap::{LightmapReceiverMesh, LightmapSettings, receiver_mesh};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, GiLookupResolution,
    LabMaterial, LabMesh, LabScene, LabVertex, MeshletBuilder, PostSettings, RenderDevice, RenderProfile,
    RenderProfilePreset, SsaoMode, horizontal_plus_fov_y,
};

const W: u32 = 160;
const H: u32 = 96;
const EYE: [f32; 3] = [0.0, 2.4, 2.6];
const TARGET: [f32; 3] = [0.0, 0.0, -0.2];
const FOV_Y: f32 = 0.9;
const FLOOR: u64 = 0x0A0F_100E;
const OTHER_HALF: u64 = 0x0A0F_100F;
/// The lightmap's uniform irradiance (lux).
const BAKED_LUX: f32 = 3_000.0;
const UP: [f32; 3] = [0.0, 1.0, 0.0];
/// Largest difference between the frame's visibility and the CPU's. The frame reads a half-resolution
/// lookup through a tone curve of 8-bit bytes.
const VISIBILITY_TOLERANCE: f32 = 0.035;
/// How far a non-receiver's luma may move when the lightmap beside it turns on. Well under the
/// darkening the pixels counted as darkened show, so a second darkening would not pass.
const SAME_LIGHT_LUMA: f32 = 4.0;

fn device() -> &'static RenderDevice {
    static D: OnceLock<RenderDevice> = OnceLock::new();
    D.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => {
            eprintln!(
                "lightmap_capsule_occluders adapter: {:?}",
                device.adapter.get_info()
            );
            device
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    })
}

/// A vehicle-sized body parked on the floor, 0.15 m clear of it: two capsules in a row.
fn vehicle() -> Vec<GiCapsule> {
    vec![
        GiCapsule::new([-0.9, 0.5, -0.2], [0.0, 0.5, -0.2], 0.35),
        GiCapsule::new([0.0, 0.5, -0.2], [0.9, 0.5, -0.2], 0.35),
    ]
}

/// An 8 m floor quad at y = 0 facing up, UV 0..1 over it.
fn floor() -> (Vec<LabVertex>, Vec<u32>) {
    let n = UP;
    (
        vec![
            LabVertex::from_pos_normal_uv([-4.0, 0.0, 4.0], n, [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([4.0, 0.0, 4.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([4.0, 0.0, -4.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-4.0, 0.0, -4.0], n, [0.0, 0.0]),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

/// One entry `floor` over the whole 4 × 4 atlas, every texel `BAKED_LUX`.
fn lightmap() -> Arc<Lightmap> {
    let map = Lightmap::from_irradiance(
        LightmapLayout {
            width: 4,
            height: 4,
            density: 1.0,
            padding: 0,
            entries: vec![LightmapEntry {
                name: "floor".into(),
                source_vertices: 4,
                rect: [0, 0, 4, 4],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 0,
                index_count: 0,
                first_vertex: 0,
                first_index: 0,
                charts: 1,
                area_m2: 64.0,
                texels_per_metre: 0.5,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: vec![0, 1, 2, 3],
                uv: vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                indices: vec![0, 1, 2, 0, 2, 3],
            }],
        },
        &[[BAKED_LUX; 3]; 16],
        vec![Coverage::BAKED; 16],
        LightmapBakeInfo {
            contents: LightmapContents::SKY,
            ..LightmapBakeInfo::default()
        },
    );
    match map {
        Ok(map) => Arc::new(map),
        Err(err) => panic!("valid lightmap: {err:?}"),
    }
}

/// A volume covering the floor that holds `level` of every component (0: a frame whose only light
/// is the lightmap; the screen pass still runs).
fn volume(level: f32) -> Arc<BakedGiVolume> {
    let layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-8.0, -8.0],
            spacing: [8.0, 8.0],
            counts: [3, 3],
            levels: vec![0.5],
            ground: vec![0.0; 9],
        }),
        bricks: Vec::new(),
        moment_resolution: 4,
        max_distance: 16.0,
    };
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|_| ProbeData {
            sh: [[[level, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let lights = BakeLights {
        sky_scale: 1.0,
        ..BakeLights::default()
    };
    match BakedGiVolume::from_probes(&layout, lights, BakeInfo::default(), &probes) {
        Ok(volume) => Arc::new(volume),
        Err(err) => panic!("valid volume: {err:?}"),
    }
}

/// No sun, no sky map, no fill, no plain mesh: the receiver is the frame.
fn lab() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 50.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: floor().0,
            indices: floor().1,
            ..LabMesh::unskinned()
        }],
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Path {
    Cpu,
    GpuDriven,
    Visibility,
    LiteLow,
    LiteHigh,
}

fn quality(path: Path) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = false;
    q.render_scale_permille = 1000;
    q.ssao = SsaoMode::Off;
    q.baked_gi = true;
    q.gpu_driven = matches!(path, Path::GpuDriven | Path::Visibility);
    q.visibility_buffer = path == Path::Visibility;
    // Every frame runs its screen passes: the static-screen reuse would hold a frame whose capsules
    // alone changed (`a_held_view_*` below).
    q.paged_sun_shadows = false;
    assert!(!q.reuse_static_screen());
    q
}

/// Builds the clusters a visibility-buffer frame draws.
fn meshlet_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// One path's renderer and its scene: its pipelines are compiled once, and the frames below only
/// change the lightmap's intensity and the capsules.
struct Rig {
    path: Path,
    renderer: FrameRenderer,
    map: Arc<Lightmap>,
    built: LightmapReceiverMesh,
    /// The frame may reuse its screen passes while nothing in the static-screen key moves, as a
    /// shipped desktop profile does.
    reuse: bool,
}

impl Rig {
    /// `level`: what the volume holds (0: the lightmap is the frame's only light).
    fn new(path: Path, level: f32) -> Self {
        let map = lightmap();
        let (fv, fi) = floor();
        let built = match receiver_mesh(&map, 0, &fv, &fi) {
            Ok(built) => built,
            Err(err) => panic!("the baked mesh: {err:?}"),
        };
        let mut renderer = FrameRenderer::new();
        if path == Path::Visibility {
            renderer.set_meshlet_builder(Some(meshlet_builder()));
        }
        match path {
            Path::LiteLow => renderer.set_forward_lite(Some(ForwardLite::Low)),
            Path::LiteHigh => renderer.set_forward_lite(Some(ForwardLite::High)),
            _ => {}
        }
        renderer.set_baked_gi(Some(volume(level)));
        Self {
            path,
            renderer,
            map,
            built,
            reuse: false,
        }
    }

    /// The frame's pixels, two frames in so every history the passes keep is settled. `enabled`: the
    /// lightmap lights its receiver.
    fn frame(&mut self, intensity: f32, capsules: &[GiCapsule], enabled: bool) -> Vec<u8> {
        self.renderer.set_gi_occluders(capsules);
        self.renderer.set_lightmap(
            Some(self.map.clone()),
            LightmapSettings {
                enabled,
                intensity,
                ..LightmapSettings::default()
            },
        );
        self.renderer.set_lightmap_receivers(&[(FLOOR, 0)]);
        let lab = lab();
        let mut q = quality(self.path);
        q.paged_sun_shadows = self.reuse;
        let base = lab.as_frame_scene_with_quality(q);
        let scene = FrameScene {
            meshes: vec![FrameMesh {
                mesh_key: 0x0A0F_0001 << 3,
                object_key: FLOOR,
                vertices: &self.built.vertices,
                indices: &self.built.indices,
                uv1: &self.built.uv2,
                ..base.meshes[0]
            }],
            ..base.clone()
        };
        let mut last = Vec::new();
        for _ in 0..2 {
            last = match self.renderer.render_offscreen(device(), &scene) {
                Ok(frame) => frame.pixels,
                Err(err) => panic!("frame: {err:?}"),
            };
        }
        assert_eq!(last.len(), (W * H * 4) as usize);
        let status = self.renderer.baked_gi_status();
        assert!(
            status.active,
            "{:?}: the baked screen pass runs: {status:?}",
            self.path
        );
        if enabled {
            let stats = self.renderer.lightmap_forward_stats();
            assert_eq!(
                (stats.drawn, stats.fallback),
                (1, 0),
                "{:?}: {stats:?}",
                self.path
            );
            match self.path {
                Path::Cpu => {}
                Path::GpuDriven => assert_eq!(stats.instanced, 1, "{stats:?}"),
                Path::Visibility => assert_eq!(stats.visbuffer, 1, "{stats:?}"),
                Path::LiteLow | Path::LiteHigh => assert_eq!(stats.lite, 1, "{stats:?}"),
            }
        }
        last
    }
}

fn luma(img: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f32::from(img[i]) + 0.7152 * f32::from(img[i + 1]) + 0.0722 * f32::from(img[i + 2])
}

/// The vertical FOV the frame draws: the profile's horizontal-plus FOV widens `FOV_Y` below 16:9, and
/// this frame (160 × 96) is narrower, so it shows about 0.95 rad. Projecting pixels with `FOV_Y` puts
/// them on the wrong floor points, far enough to matter where the visibility changes fast.
fn drawn_fov_y() -> f32 {
    if quality(Path::Cpu).horizontal_plus {
        horizontal_plus_fov_y(FOV_Y, W as f32 / H as f32)
    } else {
        FOV_Y
    }
}

/// World point on the floor plane under the point `(x, y)` of the frame (pixel units), and whether
/// it lies on the 8 m floor quad. `None` when the ray does not go down.
fn floor_hit(x: f32, y: f32) -> Option<(Vec3, bool)> {
    let view = look_at_mat4(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj = perspective(drawn_fov_y(), W as f32 / H as f32, 0.1, 50.0);
    let inv = (proj * view).inverse();
    let ndc = [x / W as f32 * 2.0 - 1.0, 1.0 - y / H as f32 * 2.0];
    let far = inv * Vec4::new(ndc[0], ndc[1], 1.0, 1.0);
    let far = far.truncate() / far.w;
    let eye = Vec3::from_array(EYE);
    let dir = far - eye;
    if dir.y >= 0.0 {
        return None;
    }
    let hit = eye + dir * (-eye.y / dir.y);
    Some((hit, hit.x.abs() <= 4.0 && hit.z.abs() <= 4.0))
}

/// World point on the floor under the centre of pixel `(x, y)`, `None` off the floor.
fn floor_point(x: u32, y: u32) -> Option<[f32; 3]> {
    let (hit, _) = floor_hit(x as f32 + 0.5, y as f32 + 0.5)?;
    (hit.x.abs() < 3.5 && hit.z.abs() < 3.5).then(|| hit.to_array())
}

/// The capsules' visibility at pixel `(x, y)` as the frame reads it, on the CPU: the screen pass
/// stores `capsule_visibility` at each lookup texel's own pixel (texel `t` holds pixel `step · t`),
/// and `ddgi_screen_occluder_visibility` mixes the 2 × 2 texels around the pixel bilinearly, each
/// weighted by how close its eye distance is to the pixel's (a texel off the floor holds none). This
/// is the reference the frame must match: comparing with the visibility at the pixel itself would
/// charge the frame with the lookup's half resolution, which is steep at a capsule's end.
fn lookup_visibility(capsules: &[GiCapsule], x: u32, y: u32) -> Option<f32> {
    let lookup = GiLookupResolution::default();
    let step = lookup.step();
    let (lw, lh) = lookup.lookup_size(W, H);
    let eye = Vec3::from_array(EYE);
    let (p, on) = floor_hit(x as f32 + 0.5, y as f32 + 0.5)?;
    if !on {
        return None;
    }
    let dist = (p - eye).length();
    let hc = [x as f32 / step as f32, y as f32 / step as f32];
    let fl = [hc[0].floor(), hc[1].floor()];
    let f = [hc[0] - fl[0], hc[1] - fl[1]];
    let (mut sum, mut wsum, mut best, mut best_err) = (0.0_f32, 0.0_f32, 0.0_f32, f32::MAX);
    for k in 0..4_u32 {
        let o = [k & 1, k >> 1];
        let t = [
            (fl[0] as i32 + o[0] as i32).clamp(0, lw as i32 - 1) as u32,
            (fl[1] as i32 + o[1] as i32).clamp(0, lh as i32 - 1) as u32,
        ];
        let w_bi = (if o[0] == 1 { f[0] } else { 1.0 - f[0] }) * (if o[1] == 1 { f[1] } else { 1.0 - f[1] });
        let Some((tp, true)) = floor_hit((t[0] * step) as f32 + 0.5, (t[1] * step) as f32 + 0.5) else {
            continue;
        };
        let d = (tp - eye).length();
        let err = (d - dist).abs() / (0.02 * dist + 0.05);
        let w = (w_bi + 1e-3) * (1.0 - err).max(0.0);
        let occlusion = 1.0 - capsule_visibility(capsules, tp.to_array(), UP);
        if err < best_err {
            best = occlusion;
            best_err = err;
        }
        sum += occlusion * w;
        wsum += w;
    }
    Some(1.0 - if wsum <= 1e-4 { best } else { sum / wsum })
}

/// The tone curve's frames, by rising visibility: `(visibility, frame)`, each the no-capsule frame
/// with the lightmap's intensity at that value.
struct Curve {
    frames: Vec<(f32, Vec<u8>)>,
}

impl Curve {
    fn of(rig: &mut Rig) -> Self {
        let frames = (3..=10)
            .map(|k| {
                let v = k as f32 / 10.0;
                (v, rig.frame(v, &[], true))
            })
            .collect();
        Self { frames }
    }

    /// This pixel's light follows the lightmap's, and is not clipped at the top.
    fn responds(&self, x: u32, y: u32) -> bool {
        let (Some((_, low)), Some((_, high))) = (self.frames.first(), self.frames.last()) else {
            return false;
        };
        let (low, high) = (luma(low, x, y), luma(high, x, y));
        high > low + 20.0 && high < 250.0
    }

    /// The visibility whose no-capsule frame has this pixel's luma.
    fn visibility(&self, x: u32, y: u32, value: f32) -> f32 {
        let mut prev = (0.0, 0.0);
        for (v, frame) in &self.frames {
            let l = luma(frame, x, y);
            if value <= l {
                let t = (value - prev.1) / (l - prev.1).max(1e-3);
                return prev.0 + (v - prev.0) * t.clamp(0.0, 1.0);
            }
            prev = (*v, l);
        }
        prev.0
    }
}

/// With `REGIA_CAPSULE_DIR` set, writes `pixels` there as binary PPM, to read by eye.
fn dump(path: Path, name: &str, pixels: &[u8]) {
    let Some(dir) = std::env::var_os("REGIA_CAPSULE_DIR") else {
        return;
    };
    let mut ppm = format!("P6\n{W} {H}\n255\n").into_bytes();
    for rgba in pixels.as_chunks::<4>().0 {
        ppm.extend_from_slice(&rgba[..3]);
    }
    let file = std::path::Path::new(&dir).join(format!("{path:?}_{name}.ppm"));
    if let Err(err) = std::fs::write(&file, ppm) {
        panic!("{}: {err}", file.display());
    }
}

fn compare_with_the_cpu(path: Path) {
    let mut rig = Rig::new(path, 0.0);
    let curve = Curve::of(&mut rig);
    let capsules = vehicle();
    let open = rig.frame(1.0, &[], true);
    let dark = rig.frame(1.0, &capsules, true);
    dump(path, "open", &open);
    dump(path, "dark", &dark);
    let (mut sampled, mut darkened) = (0, 0);
    // Against the lookup's reading (the reference); the distance from the visibility at the pixel
    // itself is reported, to show what the half resolution costs.
    let mut worst = (0.0_f32, [0.0; 3], 0.0, 0.0);
    let mut worst_pixel = (0.0_f32, [0.0; 3], 0.0, 0.0);
    for y in (2..H - 2).step_by(3) {
        for x in (2..W - 2).step_by(3) {
            let Some(p) = floor_point(x, y) else {
                continue;
            };
            let cpu = capsule_visibility(&capsules, p, UP);
            let Some(lookup) = lookup_visibility(&capsules, x, y) else {
                continue;
            };
            // The frame's tone curve is only measured from 0.3 up, and a pixel the lightmap does not
            // light (the floor's far edge against the clear color) has no curve to read.
            if cpu.min(lookup) < 0.35 || !curve.responds(x, y) {
                continue;
            }
            sampled += 1;
            let got = curve.visibility(x, y, luma(&dark, x, y));
            if cpu < 0.85 {
                darkened += 1;
            }
            let err = (got - lookup).abs();
            if err > worst.0 {
                worst = (err, p, got, lookup);
            }
            if (got - cpu).abs() > worst_pixel.0 {
                worst_pixel = ((got - cpu).abs(), p, got, cpu);
            }
        }
    }
    eprintln!(
        "{path:?}: {sampled} floor pixels, {darkened} under the vehicle; worst {:.4} at {:?} (frame {:.3}, lookup {:.3}); \
         against the pixel's own visibility {:.4} at {:?} (frame {:.3}, pixel {:.3})",
        worst.0, worst.1, worst.2, worst.3, worst_pixel.0, worst_pixel.1, worst_pixel.2, worst_pixel.3
    );
    assert!(
        luma(&open, W / 2, H / 2) > 20.0,
        "{path:?}: the floor is lit, not black"
    );
    assert!(sampled > 200, "{path:?}: {sampled} floor pixels");
    assert!(
        darkened > 40,
        "{path:?}: the vehicle darkens the floor ({darkened} pixels)"
    );
    assert!(
        worst.0 <= VISIBILITY_TOLERANCE,
        "{path:?}: the frame's visibility {:.3} against the lookup's {:.3} at {:?}",
        worst.2,
        worst.3,
        worst.1
    );
}

#[test]
fn the_frame_matches_the_cpu_visibility_on_the_cpu_loop() {
    compare_with_the_cpu(Path::Cpu);
}

#[test]
fn the_frame_matches_the_cpu_visibility_on_the_gpu_driven_batches() {
    compare_with_the_cpu(Path::GpuDriven);
}

#[test]
fn the_frame_matches_the_cpu_visibility_in_the_visibility_buffer() {
    compare_with_the_cpu(Path::Visibility);
}

#[test]
fn the_frame_matches_the_cpu_visibility_on_the_lite_tiers() {
    compare_with_the_cpu(Path::LiteLow);
    compare_with_the_cpu(Path::LiteHigh);
}

/// With no capsule the channel is neither written nor read: the same bytes as a renderer that never
/// heard of occluders, and clearing a list gives them back.
#[test]
fn no_capsule_gives_the_same_image() {
    for path in [Path::Cpu, Path::GpuDriven, Path::LiteHigh] {
        let mut rig = Rig::new(path, 0.0);
        let never = rig.frame(1.0, &[], true);
        assert_eq!(
            never,
            rig.frame(1.0, &[], true),
            "{path:?}: the frame is deterministic"
        );
        let dark = rig.frame(1.0, &vehicle(), true);
        assert_ne!(dark, never, "{path:?}: the capsules darken the floor");
        assert_eq!(
            rig.frame(1.0, &[], true),
            never,
            "{path:?}: clearing the list gives the frame back"
        );
    }
}

/// A receiver and a non-receiver in one GPU-driven batch, each half of the floor: the non-receiver
/// reads the capsules from the ambient cube alone. Its half is the same with the lightmap on as with
/// it off (one darkening, not two), and the capsules do darken it.
#[test]
fn a_non_receiver_beside_a_receiver_is_darkened_once() {
    let mut q = quality(Path::GpuDriven);
    q.gpu_driven = true;
    let map = lightmap();
    let (fv, fi) = floor();
    let built = match receiver_mesh(&map, 0, &fv, &fi) {
        Ok(built) => built,
        Err(err) => panic!("the baked mesh: {err:?}"),
    };
    let lab = lab();
    let base = lab.as_frame_scene_with_quality(q);
    let shifted = |x: f32| {
        let mut model = base.meshes[0].model;
        model[3][0] = x;
        model
    };
    let half = |key: u64, x: f32| FrameMesh {
        mesh_key: 0x0A0F_0001 << 3,
        object_key: key,
        vertices: &built.vertices,
        indices: &built.indices,
        uv1: &built.uv2,
        model: shifted(x),
        ..base.meshes[0]
    };
    let scene = FrameScene {
        meshes: vec![half(FLOOR, -4.0), half(OTHER_HALF, 4.0)],
        ..base.clone()
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_baked_gi(Some(volume(1_500.0)));
    renderer.set_lightmap_receivers(&[(FLOOR, 0)]);
    let mut frame = |capsules: &[GiCapsule], enabled: bool| {
        renderer.set_gi_occluders(capsules);
        renderer.set_lightmap(
            Some(map.clone()),
            LightmapSettings {
                enabled,
                ..LightmapSettings::default()
            },
        );
        let mut last = Vec::new();
        for _ in 0..2 {
            last = match renderer.render_offscreen(device(), &scene) {
                Ok(frame) => frame.pixels,
                Err(err) => panic!("frame: {err:?}"),
            };
        }
        last
    };
    let open = frame(&[], false);
    let off = frame(&vehicle(), false);
    let on = frame(&vehicle(), true);
    let (mut checked, mut darkened) = (0, 0);
    for y in (2..H - 2).step_by(3) {
        for x in (2..W - 2).step_by(3) {
            let Some(p) = floor_point(x, y) else {
                continue;
            };
            if p[0] < 0.1 {
                continue;
            }
            checked += 1;
            let (lit_off, lit_on) = (luma(&off, x, y), luma(&on, x, y));
            // The brighter half beside it moves the post stack's glow by a level or two.
            assert!(
                (lit_off - lit_on).abs() <= SAME_LIGHT_LUMA,
                "the non-receiver at {p:?} takes the capsules once: {lit_off} with the lightmap off, {lit_on} on"
            );
            if lit_off < luma(&open, x, y) - 2.0 * SAME_LIGHT_LUMA {
                darkened += 1;
            }
        }
    }
    assert!(checked > 100, "{checked} pixels of the non-receiver");
    assert!(darkened > 5, "the ambient cube darkens it ({darkened} pixels)");
}

/// Capsules arriving on a view the static-screen reuse holds: the first frames with capsules must keep
/// the light the lookup holds. A lookup buffer that grew for the occluder words read as empty here, a
/// black frame until something else moved. The held frame is compared with a renderer that never
/// reuses its screen passes and draws the same capsules, so the capsules' own darkening (the screen
/// pass runs again once the static-screen key holds them) does not count as lost light.
#[test]
fn capsules_on_a_held_view_keep_the_light() {
    let mut rig = Rig::new(Path::Cpu, 1_500.0);
    rig.reuse = true;
    rig.frame(1.0, &[], false);
    let open = rig.frame(1.0, &[], false);
    let mut dark_share = 0;
    let held = rig.frame(1.0, &vehicle(), false);
    let fresh = Rig::new(Path::Cpu, 1_500.0).frame(1.0, &vehicle(), false);
    for y in (2..H - 2).step_by(3) {
        for x in (2..W - 2).step_by(3) {
            if floor_point(x, y).is_some() && luma(&held, x, y) < 0.5 * luma(&fresh, x, y) {
                dark_share += 1;
            }
        }
    }
    assert!(
        luma(&open, W / 2, H / 2) > 20.0,
        "the volume lights the floor: {}",
        luma(&open, W / 2, H / 2)
    );
    assert_eq!(dark_share, 0, "the held frame keeps its light");
}

/// Capsules that change while nothing else moves take effect on the next frame, on the cube and on a
/// receiver, and clearing them gives the frame back: the static-screen key holds the capsules
/// (`frame/steps/screen_reuse.rs`, ADR-2656).
#[test]
fn a_held_view_takes_changed_capsules() {
    let mut rig = Rig::new(Path::Cpu, 0.0);
    rig.reuse = true;
    let open = rig.frame(1.0, &[], true);
    assert_eq!(open, rig.frame(1.0, &[], true), "a held view");
    let capsules = vehicle();
    let dark = rig.frame(1.0, &capsules, true);
    let mut darkened = 0;
    for y in (2..H - 2).step_by(3) {
        for x in (2..W - 2).step_by(3) {
            let Some(p) = floor_point(x, y) else {
                continue;
            };
            if capsule_visibility(&capsules, p, UP) < 0.7 && luma(&dark, x, y) < luma(&open, x, y) - 5.0 {
                darkened += 1;
            }
        }
    }
    assert!(
        darkened > 20,
        "the capsules reach the held view ({darkened} pixels)"
    );
    assert_eq!(
        rig.frame(1.0, &[], true),
        open,
        "clearing them gives the frame back"
    );
}
