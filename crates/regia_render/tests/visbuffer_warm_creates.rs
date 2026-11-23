// SPDX-License-Identifier: MPL-2.0
//! The warm leaves the visibility path's lists and targets ready (ADR-2689): a warmed profile that
//! selects by density creates no path object of any kind on the frames the scene turns dense.
//! `visbuffer_select_hitch` checks the pipelines alone; before ADR-2689 the first dense frame still
//! created its targets and lists.
//!
//! The scene's play frames differ from the warm's on purpose (no glass, other sizes gone), so the
//! renderer's bind epoch renews before the switch (ADR-184): the path's groups must survive that.

use std::sync::Arc;

use regia_render::pipeline_warm::canonical_frame;
use regia_render::visbuffer::{MeshletBuilder, MeshletMesh, VisBufferCreates};
use regia_render::{
    DenseSceneSelect, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice,
    VisibilityPath, VisibilitySelect, box_mesh,
};

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn scene(width: u32, height: u32, count: usize) -> LabScene {
    let (vertices, indices) = box_mesh([0.4, 0.4, 0.4]);
    let mut meshes = Vec::new();
    for i in 0..count {
        let mut model = [[0.0f32; 4]; 4];
        for (axis, row) in model.iter_mut().enumerate() {
            row[axis] = 1.0;
        }
        model[3] = [(i % 16) as f32 - 8.0, 0.0, -((i / 16) as f32), 1.0];
        meshes.push(LabMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
            material: i % 3,
            model,
            ..LabMesh::unskinned()
        });
    }
    LabScene {
        width,
        height,
        eye: [0.0, 6.0, 10.0],
        target: [0.0; 3],
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings::golden_deterministic()),
        directional: regia_render::DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: (0..3)
            .map(|m| LabMaterial {
                base_color: [0.4 + 0.2 * m as f32, 0.5, 0.6, 1.0],
                ..LabMaterial::default()
            })
            .collect(),
        meshes,
    }
}

/// GPU objects (not uploads) in `c`.
fn objects(c: &VisBufferCreates) -> u64 {
    c.pipelines + c.bind_groups + c.buffers + c.textures
}

/// `renderer`'s path objects created while it draws `frames` frames of `scene`.
fn created_over(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &regia_render::FrameScene<'_>,
    frames: usize,
) -> VisBufferCreates {
    let before = renderer.visbuffer_create_stats().total;
    for _ in 0..frames {
        renderer.render_offscreen(device, scene).expect("frame");
    }
    let after = renderer.visbuffer_create_stats().total;
    VisBufferCreates {
        pipelines: after.pipelines - before.pipelines,
        bind_groups: after.bind_groups - before.bind_groups,
        buffers: after.buffers - before.buffers,
        buffer_bytes: after.buffer_bytes - before.buffer_bytes,
        textures: after.textures - before.textures,
        texture_bytes: after.texture_bytes - before.texture_bytes,
        upload_bytes: after.upload_bytes - before.upload_bytes,
    }
}

#[test]
fn a_warmed_dense_selection_creates_no_lists_or_targets_when_the_scene_turns_dense() {
    let device = RenderDevice::headless().expect("device");
    let (w, h) = (160, 96);
    let path = VisibilityPath {
        select: VisibilitySelect::Dense(DenseSceneSelect {
            enter_triangles: u64::MAX,
            enter_instances: 16,
            enter_frames: 2,
            leave_frames: 2,
        }),
        single_pass_resolve: false,
    };
    let mut frame = canonical_frame();
    frame.quality.msaa_samples = 1;
    frame.quality.visibility_buffer = true;
    frame.quality.visibility_path = path;
    frame.depth_prepass = true;
    // Play draws with the warm frame's own quality, as a game's frames do.
    let quality = frame.quality;
    let sparse = scene(w, h, 2);
    let sparse_frame = sparse.as_frame_scene_with_quality(quality);
    // Ten times the instance threshold: denser than the warm scene by far, inside the reserve.
    let dense = scene(w, h, 160);
    let mut dense_frame = dense.as_frame_scene_with_quality(quality);
    // A scatter: one mesh, many instances. 160 distinct meshes would overflow the cluster arena's
    // first 256 KiB, and a growth is new content becoming resident (ADR-353), not the path's lists.
    let shared = dense_frame.meshes[0].mesh_key;
    for mesh in &mut dense_frame.meshes {
        mesh.mesh_key = shared;
    }

    let mut warmed = FrameRenderer::new();
    warmed.set_meshlet_builder(Some(single_level_builder()));
    warmed
        .warm_extracted_at(
            &device,
            &frame,
            wgpu::TextureFormat::Rgba8Unorm,
            true,
            Some((w, h)),
        )
        .expect("warm");
    let warm = warmed.visbuffer_create_stats().total;
    assert!(warm.textures > 0, "the warm drew a frame through the path");
    // Past the renderer's idle releases: the warm's small targets and its unused transient textures
    // go after 4 idle frames, each starting a new bind epoch (ADR-184).
    for _ in 0..8 {
        warmed.render_offscreen(&device, &sparse_frame).expect("sparse");
    }
    assert!(
        !warmed.visbuffer_active(),
        "the sparse scene stays on the classic path"
    );
    let created = created_over(&mut warmed, &device, &dense_frame, 6);
    assert!(warmed.visbuffer_active(), "the scene is dense");
    assert_eq!(warmed.visbuffer_stats().instances, 160);
    eprintln!("warm {warm:?}\nswitch {created:?}");
    assert_eq!(
        objects(&created),
        0,
        "the frames the scene turned dense created path objects: {created:?}"
    );
    // Nor does a later switch: back to sparse, then dense again.
    for _ in 0..4 {
        warmed
            .render_offscreen(&device, &sparse_frame)
            .expect("sparse again");
    }
    assert!(!warmed.visbuffer_active(), "the sparse scene leaves the path");
    let again = created_over(&mut warmed, &device, &dense_frame, 6);
    assert!(warmed.visbuffer_active(), "the scene is dense again");
    assert_eq!(
        objects(&again),
        0,
        "the second switch created path objects: {again:?}"
    );

    // The control: without the warm the same switch creates the targets and lists on the frame.
    let mut cold = FrameRenderer::new();
    cold.set_meshlet_builder(Some(single_level_builder()));
    for _ in 0..8 {
        cold.render_offscreen(&device, &sparse_frame)
            .expect("cold sparse");
    }
    let created = created_over(&mut cold, &device, &dense_frame, 6);
    assert!(cold.visbuffer_active());
    assert!(
        created.textures > 0 && created.buffers > 0,
        "without the warm the switch creates the path's targets and lists: {created:?}"
    );

    // A profile that does not select by density draws no warm frame through the path.
    let mut always = canonical_frame();
    always.quality.msaa_samples = 1;
    always.quality.visibility_buffer = true;
    always.depth_prepass = true;
    let mut plain = FrameRenderer::new();
    plain
        .warm_extracted_at(
            &device,
            &always,
            wgpu::TextureFormat::Rgba8Unorm,
            true,
            Some((w, h)),
        )
        .expect("plain warm");
    assert_eq!(
        plain.visbuffer_create_stats().total.textures,
        0,
        "no meshlets and no density selection: the path drew nothing"
    );
}

/// ADR-3149: the dense-scene default's own path ([`VisibilityPath::DENSE_DEFAULT`], the measured
/// thresholds: 4096 meshes or 4,000,000 triangles, per-material resolve) warms the same way. A
/// warmed renderer on that path creates no path object when a level crosses the shipped mesh
/// threshold, so turning the default on adds no hitch where a scene turns dense.
#[test]
fn the_dense_default_path_warms_its_lists_at_the_shipped_thresholds() {
    let device = RenderDevice::headless().expect("device");
    let (w, h) = (160, 96);
    let path = VisibilityPath::DENSE_DEFAULT;
    let VisibilitySelect::Dense(select) = path.select else {
        panic!("the default selects by density: {path:?}");
    };
    let mut frame = canonical_frame();
    frame.quality.msaa_samples = 1;
    frame.quality.visibility_buffer = true;
    frame.quality.visibility_path = path;
    frame.depth_prepass = true;
    let quality = frame.quality;
    let sparse = scene(w, h, 2);
    let sparse_frame = sparse.as_frame_scene_with_quality(quality);
    // Just over the shipped mesh threshold, one shared mesh (a scatter).
    let count = select.enter_instances as usize + 16;
    let dense = scene(w, h, count);
    let mut dense_frame = dense.as_frame_scene_with_quality(quality);
    let shared = dense_frame.meshes[0].mesh_key;
    for mesh in &mut dense_frame.meshes {
        mesh.mesh_key = shared;
    }

    let mut warmed = FrameRenderer::new();
    warmed.set_meshlet_builder(Some(single_level_builder()));
    warmed
        .warm_extracted_at(
            &device,
            &frame,
            wgpu::TextureFormat::Rgba8Unorm,
            true,
            Some((w, h)),
        )
        .expect("warm");
    assert!(
        warmed.visbuffer_create_stats().total.textures > 0,
        "the warm drew a frame through the path"
    );
    for _ in 0..8 {
        warmed.render_offscreen(&device, &sparse_frame).expect("sparse");
    }
    assert!(!warmed.visbuffer_active(), "two meshes stay on the classic path");
    // enter_frames frames to enter, then a few on the path.
    let created = created_over(
        &mut warmed,
        &device,
        &dense_frame,
        select.enter_frames as usize + 3,
    );
    assert!(
        warmed.visbuffer_active(),
        "the scene is dense at the shipped threshold"
    );
    assert_eq!(warmed.visbuffer_stats().instances as usize, count);
    assert!(
        !warmed.visbuffer_resolve_report().single_pass,
        "the default shades per material"
    );
    assert_eq!(
        objects(&created),
        0,
        "the frames the scene turned dense created path objects: {created:?}"
    );
}

/// A swaying frame through the path creates nothing once warm, with the clock and the wind moving
/// every frame (ADR-3916): the sway table is one uniform buffer, written in place.
#[test]
fn a_warm_swaying_frame_creates_nothing() {
    let device = RenderDevice::headless().expect("device");
    let mut lab = scene(160, 96, 32);
    for material in &mut lab.materials {
        *material = regia_render::FoliageSway::default()
            .adopt(material)
            .expect("a plain material");
    }
    let mut quality = regia_render::FrameQuality::from_profile(&regia_render::RenderProfile::from_preset(
        regia_render::RenderProfilePreset::DesktopHigh,
    ));
    quality.visibility_buffer = true;
    quality.visibility_path = VisibilityPath::DEFAULT;
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(single_level_builder()));
    for step in 0..12 {
        let t = step as f32 * 0.31;
        renderer.set_material_clock(t, t - 0.31);
        renderer.set_foliage_wind(Some([4.0 + t, 1.0]));
        renderer.render_offscreen(&device, &frame).expect("frame");
        let stats = renderer.visbuffer_stats();
        assert_eq!(
            stats.sway_instances, 32,
            "step {step}: every box sways on the path"
        );
        // Past the renderer's bind-epoch renewal (this file's header), a frame creates nothing.
        if step >= 8 {
            assert_eq!(
                objects(&renderer.visbuffer_create_stats().last_frame),
                0,
                "step {step}: {:?}",
                renderer.visbuffer_create_stats().last_frame
            );
        }
    }
}
