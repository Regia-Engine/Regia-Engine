// SPDX-License-Identifier: MPL-2.0
//! Foliage sway on the visibility path (ADR-3916): persistent sets of adopted plants against the
//! forward path, at fixed clock and wind values. The mesh-list hookup and masked plants are tested with
//! the frame hookup (`tests/visbuffer_wind.rs`, `visbuffer_differential`).

use super::super::sway::{SWAY_FADE_END, sway_fade, voxel_switch};
use super::super::{VisInstanceSet, VoxelFarProxy};
use super::*;
use crate::FoliageSway;

const PLANT_MESH: u64 = 60;
const PLANT_MATERIAL: u64 = 1060;
const SET: u64 = 8;

/// A sheet `width` × `height` facing +Z, rooted at its object origin, with `columns` × `rows` quads.
fn sheet(columns: u32, rows: u32, width: f32, height: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    for j in 0..=rows {
        for i in 0..=columns {
            let (u, v) = (i as f32 / columns as f32, j as f32 / rows as f32);
            vertices.push(LabVertex::from_pos_normal_uv(
                [(u - 0.5) * width, v * height, 0.0],
                [0.0, 0.0, 1.0],
                [u, 1.0 - v],
            ));
        }
    }
    let at = |i: u32, j: u32| j * (columns + 1) + i;
    let mut indices = Vec::new();
    for j in 0..rows {
        for i in 0..columns {
            indices.extend_from_slice(&[
                at(i, j),
                at(i + 1, j),
                at(i + 1, j + 1),
                at(i, j),
                at(i + 1, j + 1),
                at(i, j + 1),
            ]);
        }
    }
    (vertices, indices)
}

fn sway() -> FoliageSway {
    FoliageSway {
        stiffness: 0.05,
        height_m: 2.0,
        flutter_m: 0.02,
        gust_period_s: 4.0,
        max_wind_m_per_s: 20.0,
        ..FoliageSway::default()
    }
}

fn plant_material() -> LabMaterial {
    LabMaterial {
        albedo: Some(checker(32)),
        double_sided: true,
        ..LabMaterial::default()
    }
}

/// `renderer`'s clock and wind, on both of its paths. The frame hookup copies the renderer's values into
/// the pass each frame; without the hookup the pass keeps what this writes.
fn set_clock(renderer: &mut FrameRenderer, now: f32, prev: f32, wind: [f32; 2]) {
    renderer.set_material_clock(now, prev);
    renderer.set_foliage_wind(Some(wind));
    renderer
        .visbuffer_pass_mut()
        .set_sway_clock(Some((now, prev)), Some(wind));
}

fn plant_models() -> Vec<[[f32; 4]; 4]> {
    (0..5)
        .map(|i| translate((i as f32 - 2.0) * 1.2, 0.0, -(i as f32) * 0.5, 1.0))
        .collect()
}

/// A plant's mesh, material and models.
type PlantDraw<'a> = (
    &'a (Vec<LabVertex>, Vec<u32>),
    &'a LabMaterial,
    &'a [[[f32; 4]; 4]],
);

/// The ground as the frame's mesh list, for a set of plants (or, forward, the plants in the list too).
fn plant_scene<'a>(
    f: &'a Fixture,
    post: &'a PostSettings,
    plant: Option<PlantDraw<'a>>,
    vis: bool,
) -> FrameScene<'a> {
    let mut s = scene(f, post, vis, false);
    // Native resolution: the velocity and voxel tests read pixels and pixels per unit at `SIZE`.
    s.quality.render_scale_permille = 1000;
    s.quality.taau = false;
    s.meshes.truncate(1);
    if let Some((mesh, material, models)) = plant {
        for (i, model) in models.iter().enumerate() {
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: PLANT_MESH,
                material_key: PLANT_MATERIAL,
                object_key: 500 + i as u64,
                vertices: &mesh.0,
                indices: &mesh.1,
                material,
                model: *model,
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
    }
    s
}

fn set_of<'a>(
    mesh: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
    models: &'a [[[f32; 4]; 4]],
) -> VisInstanceSet<'a> {
    VisInstanceSet {
        mesh_key: PLANT_MESH,
        vertices: &mesh.0,
        indices: &mesh.1,
        material_key: PLANT_MATERIAL,
        material,
        models,
        shadow: ShadowCast::All,
        instance_shadows: &[],
    }
}

/// A visibility renderer drawing `material` as a persistent set of `models`.
fn set_renderer(
    device: &crate::RenderDevice,
    mesh: &(Vec<LabVertex>, Vec<u32>),
    material: &LabMaterial,
    models: &[[[f32; 4]; 4]],
    settings: VisBufferSettings,
) -> FrameRenderer {
    let mut renderer = comparison_renderer(device);
    renderer.set_meshlet_builder(Some(single_level_builder()));
    renderer.set_visbuffer_settings(settings);
    assert!(renderer.set_instance_set(SET, &set_of(mesh, material, models)));
    renderer
}

/// Calm air is the frame of the same material without sway, byte for byte, with the same lists: the
/// cull does not grow its bounds, and no vertex moves.
#[test]
fn zero_wind_is_bit_identical_to_the_same_material_without_sway() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 32, 1.0, 2.0);
    let models = plant_models();
    let plain = plant_material();
    let adopted = sway().adopt(&plain).expect("adopt");
    let s = plant_scene(&f, &settings, None, true);
    for wind in [[0.0, 0.0], [1.0e-6, 0.0]] {
        let mut without = set_renderer(&device, &mesh, &plain, &models, VisBufferSettings::default());
        let mut with = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
        for frame in 0..3 {
            let time = 1.25 + frame as f32 * 0.5;
            set_clock(&mut without, time, time - 0.5, wind);
            set_clock(&mut with, time, time - 0.5, wind);
            let a = render(&mut without, &device, &s);
            let b = render(&mut with, &device, &s);
            assert!(a == b, "frame {frame} under {wind:?} moved");
            let stats = with.visbuffer_stats();
            assert_eq!(stats.sway_instances, models.len() as u32);
            assert_eq!(stats.sway_materials, 1);
            assert!(!stats.sway_moving, "calm air does not grow the bounds");
            assert_eq!(without.visbuffer_stats().sway_instances, 0);
        }
        let lists = |r: &mut FrameRenderer| {
            r.visbuffer_pass_mut()
                .read_cull_lists(&device)
                .expect("readback")
                .expect("a frame ran")
        };
        let (a, b) = (lists(&mut without), lists(&mut with));
        assert_eq!(a.state, b.state, "same list counts");
        assert_eq!(a.clusters, b.clusters, "same lists");
    }
}

/// A swaying plant on the path draws what the forward path draws at fixed wind times, within the
/// differential's strict floor. The wind really moved the frame (a negative control).
#[test]
fn a_swaying_set_matches_the_forward_path_at_fixed_wind_times() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 32, 1.0, 2.0);
    let models = plant_models();
    let adopted = sway().adopt(&plant_material()).expect("adopt");
    let wind = [8.0, 3.0];
    let mut listed = plant_scene(&f, &settings, Some((&mesh, &adopted, &models)), false);
    let mut ground = plant_scene(&f, &settings, None, true);
    // The motion only. A set's pages refresh through the frame hookup: its shadow under a moving clock is
    // held by `tests/visbuffer_wind.rs`, and a held clock's by `a_swaying_cluster_shadow_matches_...` here.
    listed.directional_shadows = false;
    ground.directional_shadows = false;
    let mut forward = comparison_renderer(&device);
    let mut vis = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    let mut still = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    for (frame, (now, prev)) in [(0.5, 0.45), (1.25, 1.2), (2.9, 2.85), (3.7, 3.65)]
        .into_iter()
        .enumerate()
    {
        set_clock(&mut forward, now, prev, wind);
        set_clock(&mut vis, now, prev, wind);
        set_clock(&mut still, now, prev, [0.0, 0.0]);
        let reference = render(&mut forward, &device, &listed);
        let pixels = render(&mut vis, &device, &ground);
        let at_rest = render(&mut still, &device, &ground);
        if frame == 0 {
            continue;
        }
        let (worst, close) = diff(&reference, &pixels);
        eprintln!(
            "frame {frame}: {:.3} % within 2 levels, worst {worst}",
            close * 100.0
        );
        assert!(
            close >= close_floor(&device, 0.985),
            "frame {frame}: {:.2}% within 2 levels (worst {worst})",
            close * 100.0
        );
        let (_, unmoved) = diff(&at_rest, &pixels);
        assert!(unmoved < 0.97, "the wind moved the plants: {unmoved}");
        let stats = vis.visbuffer_stats();
        assert_eq!(stats.sway_instances, models.len() as u32);
        assert!(stats.sway_moving);
    }
}

/// The velocity pass's vector at a vertex is the displacement of that vertex between two fixed wind
/// times, computed with the sway's CPU mirror.
#[test]
fn velocity_is_the_analytic_displacement_between_two_wind_times() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(16, 64, 1.0, 2.0);
    let models = vec![translate(0.0, 0.0, 0.0, 1.0)];
    let plant = FoliageSway {
        flutter_m: 0.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    let wind = [9.0, 0.0];
    // Near the gust's trough and peak (a 4 s lap, the plant at the origin): 0.50 and 0.91 of the bend.
    let (now, prev) = (2.55, 0.38);
    // No ground: a pixel the sheet's silhouette just misses must not read the ground's zero motion.
    let mut s = plant_scene(&f, &settings, None, true);
    s.meshes.clear();
    s.eye = [0.0, 1.0, 3.0];
    s.target = [0.0, 1.0, 0.0];
    let mut vis = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    set_clock(&mut vis, now, prev, wind);
    for _ in 0..3 {
        render(&mut vis, &device, &s);
    }
    let view_proj = wind_view_proj(&s);
    let read = read_velocity(&mut vis, &device);
    let uv_of = |p: [f32; 3]| {
        let clip = view_proj * regia_core::glam::Vec4::new(p[0], p[1], p[2], 1.0);
        let ndc = clip.truncate() / clip.w;
        [ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5]
    };
    let moved = |p: [f32; 3], normal: [f32; 3], red: f32, time: f32| {
        let o = plant.offset(p, normal, red, [0.0; 3], time, wind);
        [p[0] + o[0], p[1] + o[1], p[2] + o[2]]
    };
    let (mut checked, mut largest, mut worst) = (0, 0.0f32, 0.0f32);
    for v in &mesh.0 {
        let here = moved(v.position, v.normal, v.color[0], now);
        let uv = uv_of(here);
        let px = (uv[0] * SIZE as f32).floor() as i32;
        let py = (uv[1] * SIZE as f32).floor() as i32;
        if px < 1 || py < 1 || px >= SIZE as i32 - 1 || py >= SIZE as i32 - 1 {
            continue;
        }
        let texel = read[py as usize * SIZE as usize + px as usize];
        if texel[3] < 0.5 {
            continue; // Not written: the vertex sits on the sheet's silhouette.
        }
        let there = uv_of(moved(v.position, v.normal, v.color[0], prev));
        let want = [there[0] - uv[0], there[1] - uv[1]];
        let error = ((want[0] - texel[0]).powi(2) + (want[1] - texel[1]).powi(2)).sqrt();
        largest = largest.max((want[0] * want[0] + want[1] * want[1]).sqrt());
        worst = worst.max(error);
        checked += 1;
    }
    eprintln!(
        "{checked} vertices: largest motion {:.2} px, worst error {:.3} px",
        largest * SIZE as f32,
        worst * SIZE as f32
    );
    assert!(checked > 200, "{checked} vertices in view");
    assert!(largest * SIZE as f32 > 2.0, "the motion is real: {largest}");
    assert!(
        worst * SIZE as f32 <= 0.25,
        "worst {} px of {} px",
        worst * SIZE as f32,
        largest * SIZE as f32
    );
}

/// The vertical field of view the frame draws `s` with: a target narrower than 16:9 keeps the 16:9
/// horizontal view (`output::horizontal_plus_fov_y`), so on these square frames it is wider than `fov_y`.
fn frame_fov_y(s: &FrameScene<'_>) -> f32 {
    crate::output::horizontal_plus_fov_y(s.fov_y, s.width as f32 / s.height as f32)
}

/// The frame's unjittered view-projection, as `scene` asks for it. Only `x`, `y`, `w` are read here,
/// which the renderer's depth convention does not change.
fn wind_view_proj(s: &FrameScene<'_>) -> regia_core::glam::Mat4 {
    use regia_core::glam::Vec3;
    let view = crate::view_math::look_at_view(Vec3::from_array(s.eye), Vec3::from_array(s.target), Vec3::Y);
    let aspect = s.width as f32 / s.height as f32;
    regia_core::glam::camera::rh::proj::directx::perspective(frame_fov_y(s), aspect, s.near, s.far) * view
}

/// The velocity pass over the last frame's visibility pixels, into a fresh target: `[u, v, 0, written]`
/// per pixel.
fn read_velocity(renderer: &mut FrameRenderer, device: &crate::RenderDevice) -> Vec<[f32; 4]> {
    let extent = wgpu::Extent3d {
        width: SIZE,
        height: SIZE,
        depth_or_array_layers: 1,
    };
    let make = |format, usage| {
        device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wind velocity test"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let target = make(
        wgpu::TextureFormat::Rgba16Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = make(
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
    );
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("wind velocity test clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &target_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    });
    renderer
        .visbuffer_pass_mut()
        .record_velocity(&device.device, &mut encoder, &target_view, &depth_view);
    let bytes_per_row = (SIZE * 8).next_multiple_of(256);
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("wind velocity test"),
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        extent,
    );
    device.queue.submit([encoder.finish()]);
    staging.slice(..).map_async(wgpu::MapMode::Read, |_| ());
    device
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let mapped = staging.slice(..).get_mapped_range().expect("mapped");
    let mut out = Vec::with_capacity((SIZE * SIZE) as usize);
    for row in mapped.chunks(bytes_per_row as usize) {
        for texel in row[..(SIZE * 8) as usize].as_chunks::<8>().0 {
            let half = |i: usize| half_to_f32(u16::from_le_bytes([texel[i], texel[i + 1]]));
            out.push([half(0), half(2), half(4), half(6)]);
        }
    }
    out
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let mantissa = f32::from(bits & 0x3ff);
    sign * match exponent {
        0 => mantissa * 2.0f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + mantissa / 1024.0) * 2.0f32.powi(e - 15),
    }
}

/// A plant just off the view whose swayed top reaches into it, at the sway's limit. Returns the
/// clusters with a displaced triangle in the frustum that are in neither phase's list.
fn clusters_lost_in_the_frustum(device: &crate::RenderDevice, reach_off: bool) -> (usize, usize) {
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 64, 0.2, 4.0);
    let plant = FoliageSway {
        stiffness: 0.2,
        height_m: 4.0,
        flutter_m: 0.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    let mut s = plant_scene(&f, &settings, None, true);
    s.meshes.clear();
    s.eye = [0.0, 1.5, 6.0];
    s.target = [0.0, 1.5, 0.0];
    let half = 6.0 * (frame_fov_y(&s) * 0.5).tan();
    let model = translate(half + 0.7, 0.0, 0.0, 1.0);
    let models = vec![model];
    let origin = [model[3][0], model[3][1], model[3][2]];
    let (now, wind) = (1.0, [-plant.max_wind_m_per_s, 0.0]);
    let mut vis = comparison_renderer(device);
    vis.set_meshlet_builder(Some(single_level_builder()));
    vis.visbuffer_pass_mut().set_sway_reach_off(reach_off);
    assert!(vis.set_instance_set(SET, &set_of(&mesh, &adopted, &models)));
    set_clock(&mut vis, now, now, wind);
    for _ in 0..3 {
        render(&mut vis, device, &s);
    }
    let lists = vis
        .visbuffer_pass_mut()
        .read_cull_lists(device)
        .expect("readback")
        .expect("a frame ran");
    let base = vis
        .visbuffer_pass_mut()
        .meshlet_base(PLANT_MESH)
        .expect("resident");
    let cap = lists.capacity as usize;
    let mut listed = std::collections::HashSet::new();
    for phase in 0..2 {
        let kept = lists.state[45 + phase] as usize;
        let kept_double = lists.state[47 + phase] as usize;
        for i in 0..kept {
            listed.insert(lists.clusters[2 * (phase * cap + i) + 1]);
        }
        for i in 0..kept_double {
            listed.insert(lists.clusters[2 * (phase * cap + cap - 1 - i) + 1]);
        }
    }
    let positions: Vec<[f32; 3]> = mesh.0.iter().map(|v| v.position).collect();
    let meshlets = MeshletMesh::single_level(&positions, &mesh.1);
    let view_proj = wind_view_proj(&s);
    let (mut visible, mut lost) = (0, 0);
    for (index, m) in meshlets.meshlets.iter().enumerate() {
        let shows = (0..m.triangle_count).any(|t| {
            let packed = meshlets.triangles[(m.triangle_offset + t) as usize];
            (0..3).any(|corner| {
                let local = (packed >> (corner * 8)) & 0xff;
                let v = &mesh.0[meshlets.vertices[(m.vertex_offset + local) as usize] as usize];
                let world = [
                    v.position[0] + origin[0],
                    v.position[1] + origin[1],
                    v.position[2] + origin[2],
                ];
                let o = plant.offset(world, v.normal, v.color[0], origin, now, wind);
                let clip = view_proj
                    * regia_core::glam::Vec4::new(world[0] + o[0], world[1] + o[1], world[2] + o[2], 1.0);
                clip.x.abs() < clip.w && clip.y.abs() < clip.w && clip.z > 0.0 && clip.z < clip.w
            })
        });
        if shows {
            visible += 1;
            if !listed.contains(&(base + index as u32)) {
                lost += 1;
            }
        }
    }
    (visible, lost)
}

/// No displaced triangle is culled while visible, counted on a held fixture at the sway's limit. With
/// the reach off the same fixture loses clusters, so the count can tell.
#[test]
fn no_displaced_triangle_is_culled_while_visible() {
    let device = crate::test_device();
    let (visible, lost) = clusters_lost_in_the_frustum(&device, false);
    eprintln!("reach on: {visible} clusters show, {lost} lost");
    assert!(visible > 0, "the swayed top reaches into the view");
    assert_eq!(lost, 0, "of {visible}");
    let (visible, lost) = clusters_lost_in_the_frustum(&device, true);
    eprintln!("reach off: {visible} clusters show, {lost} lost");
    assert!(
        lost > 0,
        "without the reach the fixture loses clusters: {visible} show"
    );
}

/// A plant hidden behind a wall in its rest pose whose top sways past the wall's edge: the Hi-Z test
/// takes the grown sphere, so the top draws. With the reach off the top is lost.
///
/// Eye at z = 6, wall at z = 2, plant at z = -2, so a point on the plant shows at half its x on the wall.
/// The wall's edge is at x = 0.25 and the plant stands at x = 2.5: its rest clusters' whole Hi-Z footprints
/// are behind the wall, and the bend of up to 4 m takes the top past the edge.
#[test]
fn a_plant_swaying_out_from_behind_an_occluder_is_not_occluded() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 64, 0.3, 8.0);
    let plant = FoliageSway {
        stiffness: 0.25,
        height_m: 8.0,
        flutter_m: 0.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    let wall = crate::box_mesh([6.0, 6.0, 0.1]);
    let models = vec![translate(2.5, 0.0, -2.0, 1.0)];
    let wind = [-20.0, 0.0];
    let frame = |renderer: &mut FrameRenderer, vis: bool| {
        let mut s = plant_scene(&f, &settings, None, vis);
        s.meshes.clear();
        s.eye = [0.0, 4.0, 6.0];
        s.target = [0.0, 4.0, 0.0];
        s.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: 61,
            material_key: 1061,
            object_key: 61,
            vertices: &wall.0,
            indices: &wall.1,
            material: &f.floor,
            model: translate(6.25, 4.0, 2.0, 1.0),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
        if !vis {
            s.meshes.push(FrameMesh {
                is_static: false,
                mesh_key: PLANT_MESH,
                material_key: PLANT_MATERIAL,
                object_key: 62,
                vertices: &mesh.0,
                indices: &mesh.1,
                material: &adopted,
                model: models[0],
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
        let mut last = Vec::new();
        for _ in 0..4 {
            last = render(renderer, &device, &s);
        }
        last
    };
    let mut forward = comparison_renderer(&device);
    set_clock(&mut forward, 1.0, 1.0, wind);
    let reference = frame(&mut forward, false);
    let draw = |reach_off: bool| {
        let mut renderer = comparison_renderer(&device);
        renderer.set_meshlet_builder(Some(single_level_builder()));
        renderer.visbuffer_pass_mut().set_sway_reach_off(reach_off);
        assert!(renderer.set_instance_set(SET, &set_of(&mesh, &adopted, &models)));
        set_clock(&mut renderer, 1.0, 1.0, wind);
        frame(&mut renderer, true)
    };
    let wrong = |pixels: &[u8]| {
        reference
            .as_chunks::<4>()
            .0
            .iter()
            .zip(pixels.as_chunks::<4>().0)
            .filter(|(a, b)| a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > 24))
            .count()
    };
    let (on, off) = (wrong(&draw(false)), wrong(&draw(true)));
    eprintln!("pixels off by more than 24 levels: reach on {on}, off {off}");
    assert!(on <= 2, "{on} pixels differ from the forward frame");
    assert!(off > on + 4, "the fixture can tell: reach off {off}, on {on}");
}

/// The cluster shadow of a swaying caster lands where the forward caster's does, within the bound
/// `cluster_shadows_match_forward_shadows` holds rigid casters to.
#[test]
fn a_swaying_cluster_shadow_matches_the_forward_caster() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    // Tall and off the view, as that test's pillar: only its shadow reaches the frame.
    let mesh = sheet(8, 64, 2.0, 8.0);
    let plant = FoliageSway {
        stiffness: 0.3,
        height_m: 8.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    let models = vec![translate(-5.0, 0.0, 0.0, 1.0)];
    let wind = [10.0, 0.0];
    let listed = plant_scene(&f, &settings, Some((&mesh, &adopted, &models)), false);
    let ground = plant_scene(&f, &settings, None, true);
    let mut forward = comparison_renderer(&device);
    set_clock(&mut forward, 1.0, 1.0, wind);
    render(&mut forward, &device, &listed);
    let reference = render(&mut forward, &device, &listed);
    let mut calm = comparison_renderer(&device);
    set_clock(&mut calm, 1.0, 1.0, [0.0, 0.0]);
    render(&mut calm, &device, &listed);
    let at_rest = render(&mut calm, &device, &listed);
    let mut vis = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    set_clock(&mut vis, 1.0, 1.0, wind);
    render(&mut vis, &device, &ground);
    let clusters = render(&mut vis, &device, &ground);
    let [work, drawn, lost_clusters, lost_work] = vis
        .visbuffer_shadow_counters(&device)
        .expect("readback")
        .expect("shadow counters");
    // One plant reaches every dirty page: its work items must fit, or its later pages clear and stay empty.
    assert_eq!(
        (lost_clusters, lost_work),
        (0, 0),
        "the shadow lists hold the job's casters"
    );
    assert!(work > 0 && drawn > 0, "{work} items, {drawn} clusters");
    let (worst, close) = diff(&reference, &clusters);
    eprintln!("cluster shadow vs forward: {:.3} %, worst {worst}", close * 100.0);
    assert!(
        close > close_floor(&device, 0.995),
        "vs the forward frame: {:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );
    // The pixels the wind changes in the forward frame's shadow change the same way here.
    let px = |v: &[u8], i: usize| -> [u8; 3] { [v[i * 4], v[i * 4 + 1], v[i * 4 + 2]] };
    let differs = |a: [u8; 3], b: [u8; 3]| a.iter().zip(&b).any(|(x, y)| x.abs_diff(*y) > 8);
    let moved: Vec<usize> = (0..reference.len() / 4)
        .filter(|&i| differs(px(&at_rest, i), px(&reference, i)))
        .collect();
    assert!(
        moved.len() > 20,
        "the wind moves the shadow: {} pixels",
        moved.len()
    );
    let matching = moved
        .iter()
        .filter(|&&i| !differs(px(&reference, i), px(&clusters, i)))
        .count();
    assert!(
        matching * 10 >= moved.len() * 9,
        "{matching} of {} moved pixels match",
        moved.len()
    );
}

/// A sheet leaning away from the camera, so its near clusters stay in hardware and its far ones take
/// the compute rasterizer: under wind no gap opens between them, against the all-hardware frame.
///
/// The size test sees the sphere grown by the reach, so the plant's reach stays small next to its far
/// clusters' distance: 0.5 m, a cluster of about 0.4 m, 24 px from about 5 m. The sheet lies nearly flat,
/// so `height_m` is the height its top reaches, and the top bends by the whole amplitude.
#[test]
fn software_clusters_do_not_crack_against_hardware_neighbours_under_wind() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 128, 0.6, 12.0);
    let plant = FoliageSway {
        stiffness: 0.04,
        height_m: 2.1,
        flutter_m: 0.05,
        max_wind_m_per_s: 12.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    // Laid back by 80 degrees about X: the top is far away and small.
    let (sin, cos) = 80.0f32.to_radians().sin_cos();
    let model = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, cos, -sin, 0.0],
        [0.0, sin, cos, 0.0],
        [0.0, 0.2, 2.0, 1.0],
    ];
    let models = vec![model];
    let mut s = plant_scene(&f, &settings, None, true);
    s.eye = [0.0, 1.6, 4.0];
    s.target = [0.0, 0.0, -2.0];
    let wind = [12.0, 2.0];
    let draw = |software: bool| {
        let visibility = VisBufferSettings {
            software_raster: software,
            software_raster_max_px: 24,
            ..VisBufferSettings::default()
        };
        let mut renderer = set_renderer(&device, &mesh, &adopted, &models, visibility);
        set_clock(&mut renderer, 1.3, 1.3, wind);
        render(&mut renderer, &device, &s);
        let pixels = render(&mut renderer, &device, &s);
        let counters = renderer
            .visbuffer_counters(&device)
            .expect("readback")
            .expect("counters");
        (pixels, counters)
    };
    let (hardware, _) = draw(false);
    let (software, counters) = draw(true);
    eprintln!("{counters:?}");
    assert!(
        counters.software_clusters > 0,
        "some clusters in compute: {counters:?}"
    );
    // The phase counts are the hardware lists; software clusters count apart.
    assert!(
        counters.phase1_clusters + counters.phase2_clusters > 0,
        "and some in hardware: {counters:?}"
    );
    let (worst, close) = diff(&hardware, &software);
    eprintln!("{:.3} % within 2 levels, worst {worst}", close * 100.0);
    assert!(
        close >= close_floor(&device, 0.995),
        "{:.2}% within 2 levels (worst {worst})",
        close * 100.0
    );
    let cracks = hardware
        .as_chunks::<4>()
        .0
        .iter()
        .zip(software.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > 32))
        .count();
    assert_eq!(cracks, 0, "pixels a crack would show");
}

/// A warm swaying frame creates nothing, with the clock and the wind changing every frame, and neither
/// does the first gust after calm air, which sends the plant's pages to the dynamic class.
#[test]
fn a_warm_swaying_frame_creates_nothing() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 32, 1.0, 2.0);
    let models = plant_models();
    let adopted = sway().adopt(&plant_material()).expect("adopt");
    let s = plant_scene(&f, &settings, None, true);
    let mut vis = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    for frame in 0..8 {
        let t = frame as f32 * 0.37;
        set_clock(&mut vis, t, t - 0.37, [6.0 + t, 1.0]);
        render(&mut vis, &device, &s);
        if frame >= 3 {
            let c = vis.visbuffer_create_stats().last_frame;
            assert_eq!(c.objects(), 0, "frame {frame}: {c:?}");
        }
    }
    let mut gusting = set_renderer(&device, &mesh, &adopted, &models, VisBufferSettings::default());
    for frame in 0..8 {
        let t = frame as f32 * 0.37;
        let wind = if frame < 4 { [0.0, 0.0] } else { [8.0, 1.0] };
        set_clock(&mut gusting, t, t - 0.37, wind);
        render(&mut gusting, &device, &s);
        let c = gusting.visbuffer_create_stats().last_frame;
        // The first shadow job since frame 0 rebuilds the job's two bind groups, as for any caster that
        // moves: frame 0 replaced a path buffer after its shadow cull. The gust adds nothing.
        if frame == 4 {
            assert_eq!(
                (c.buffers, c.pipelines, c.textures),
                (0, 0, 0),
                "first gust: {c:?}"
            );
        } else if frame > 4 {
            assert_eq!(c.objects(), 0, "gust, frame {frame}: {c:?}");
        }
    }
}

/// A mesh with one fabricated voxel far level, as `VoxelFarProxy::extend_meshlets` leaves it: the plain
/// clusters are the roots, their parent the voxel level's sphere.
fn with_voxel_level(mesh: &(Vec<LabVertex>, Vec<u32>), error: f32) -> MeshletMesh {
    let positions: Vec<[f32; 3]> = mesh.0.iter().map(|v| v.position).collect();
    let mut meshlets = MeshletMesh::single_level(&positions, &mesh.1);
    let sphere = meshlets.bounds;
    for root in &mut meshlets.meshlets {
        root.lod = sphere;
        root.parent = sphere;
        root.parent_error = error;
    }
    let mut voxel = meshlets.meshlets[0];
    voxel.level = 1;
    voxel.lod_error = error;
    voxel.parent_error = crate::visbuffer::ROOT_ERROR;
    voxel.cone = [0.0, 0.0, 1.0, 1.0];
    meshlets.meshlets.push(voxel);
    meshlets.validate().expect("valid");
    meshlets
}

/// The sway reaches zero before the first voxel level can be selected: at 96 % of the switch distance
/// two wind times draw the same bytes and the triangle level is still the one drawn; at half the
/// distance they differ; past the switch the voxel level is drawn, at rest.
#[test]
fn the_sway_fades_out_before_the_voxel_switch() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 32, 4.0, 8.0);
    let error = 0.5;
    let meshlets = Arc::new(with_voxel_level(&mesh, error));
    let found = voxel_switch(&meshlets).expect("the voxel level is found");
    assert_eq!(found.level, 1);
    assert_eq!(found.error, error);
    let plant = FoliageSway {
        stiffness: 0.2,
        height_m: 8.0,
        ..sway()
    };
    let adopted = plant.adopt(&plant_material()).expect("adopt");
    let model = translate(0.0, 0.0, 0.0, 1.0);
    let models = vec![model];
    let frames_at = |distance: f32| {
        let mut s = plant_scene(&f, &settings, None, true);
        s.meshes.clear();
        let center = meshlets.bounds;
        s.eye = [center[0], center[1], center[2] + distance];
        s.target = [center[0], center[1], center[2]];
        s.far = distance * 4.0 + 100.0;
        s.directional_shadows = false;
        // Two renderers at the same frame number: temporal state moves a frame on its own, so only the
        // wind time may differ between what is compared.
        let mut last = [Vec::new(), Vec::new()];
        let mut voxel_listed = false;
        for (i, t) in [0.6, 1.9].into_iter().enumerate() {
            let mut vis = comparison_renderer(&device);
            vis.insert_meshlets(PLANT_MESH, Arc::clone(&meshlets));
            assert!(vis.set_instance_set(SET, &set_of(&mesh, &adopted, &models)));
            set_clock(&mut vis, t, t, [15.0, 0.0]);
            render(&mut vis, &device, &s);
            last[i] = render(&mut vis, &device, &s);
            let lists = vis
                .visbuffer_pass_mut()
                .read_cull_lists(&device)
                .expect("readback")
                .expect("a frame ran");
            let base = vis
                .visbuffer_pass_mut()
                .meshlet_base(PLANT_MESH)
                .expect("resident");
            let voxel_index = base + meshlets.meshlets.len() as u32 - 1;
            let cap = lists.capacity as usize;
            voxel_listed |= (0..2).any(|phase| {
                let kept = lists.state[45 + phase] as usize;
                let kept_double = lists.state[47 + phase] as usize;
                (0..kept).any(|i| lists.clusters[2 * (phase * cap + i) + 1] == voxel_index)
                    || (0..kept_double)
                        .any(|i| lists.clusters[2 * (phase * cap + cap - 1 - i) + 1] == voxel_index)
            });
        }
        (last, voxel_listed)
    };
    let threshold = VisBufferSettings::default().lod_threshold_px;
    let fov_y = frame_fov_y(&plant_scene(&f, &settings, None, true));
    let pixels_per_unit = SIZE as f32 / (2.0 * (fov_y * 0.5).tan());
    let switch = VoxelFarProxy::switch_distance(error, found.sphere[3], pixels_per_unit, threshold);
    assert_eq!(sway_fade(SWAY_FADE_END * switch, switch), 0.0);

    let ([a, b], voxel) = frames_at(0.5 * switch);
    assert!(a != b, "at half the switch distance the plant sways");
    assert!(!voxel, "the triangles are drawn there");
    let ([a, b], voxel) = frames_at(0.96 * switch);
    assert!(a == b, "at 96 % of the switch distance the plant is at rest");
    assert!(!voxel, "and still the triangle level is the one drawn");
    let ([a, b], voxel) = frames_at(1.1 * switch);
    assert!(voxel, "past the switch the voxel level is drawn");
    assert!(a == b, "and it does not sway");
}

/// A moving frame clock is a change to what a time-reading graph draws. Since the shaders read the
/// frame clock (ADR-5160) a clock-only change rewrites no graph block, so the material epoch must
/// move by itself: it keys the static-screen reuse, and a held view of a swaying plant under a
/// moving clock otherwise kept last frame's screen passes (ADR-5554). A held clock leaves it.
#[test]
fn a_moving_clock_moves_the_material_epoch_and_a_held_one_does_not() {
    let device = crate::test_device();
    let f = fixture(1);
    let settings = post();
    let mesh = sheet(8, 32, 1.0, 2.0);
    let models = plant_models();
    let adopted = sway().adopt(&plant_material()).expect("adopt");
    let listed = plant_scene(&f, &settings, Some((&mesh, &adopted, &models)), false);
    let mut forward = comparison_renderer(&device);
    set_clock(&mut forward, 1.0, 0.95, [8.0, 3.0]);
    render(&mut forward, &device, &listed);
    render(&mut forward, &device, &listed);
    let held = forward.material_epoch_for_tests();
    render(&mut forward, &device, &listed);
    assert_eq!(
        forward.material_epoch_for_tests(),
        held,
        "control: a held clock and an unchanged scene leave the epoch alone"
    );
    set_clock(&mut forward, 1.25, 1.0, [8.0, 3.0]);
    render(&mut forward, &device, &listed);
    assert_ne!(
        forward.material_epoch_for_tests(),
        held,
        "the clock moved the plants under an unchanged screen key"
    );
}
