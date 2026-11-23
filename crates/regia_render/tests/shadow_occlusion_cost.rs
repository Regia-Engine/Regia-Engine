// SPDX-License-Identifier: MPL-2.0
//! What the shadow casters' two-phase cull (ADR-2072) adds to a steady culled frame, by counters
//! (ADR-2482): compute passes and dispatches (`shadow_occlusion_work`) and heap allocations per
//! frame against the same frame unculled. No wall clock.
//!
//! The scene is `shadow_occlusion_steady.rs`'s: 256 small cubes under a slab, two cascades
//! redrawn every frame, so most casters are culled. The card-costs run (2026-11-02) measured
//! +391 allocations per culled frame here; most of it is what wgpu allocates for every pass and
//! for the indirect-draw validation of each pass that draws from the cull's commands.
//!
//! Its own test binary: it installs a counting global allocator that counts every thread.

use regia_core::math::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    IndirectStrategy, LabMaterial, LabVertex, RenderDevice, RenderProfile, RenderProfilePreset, ShadowCast,
    ShadowOcclusionWork, box_mesh, shadow_occlusion_work,
};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): every thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Allocation requests of every thread so far.
fn allocs() -> u64 {
    alloc_counter::process_allocs()
}

const BATCH: u64 = 8;

struct Geometry {
    ground: (Vec<LabVertex>, Vec<u32>),
    slab: (Vec<LabVertex>, Vec<u32>),
    cube: (Vec<LabVertex>, Vec<u32>),
    material: LabMaterial,
}

impl Geometry {
    fn mesh<'g>(
        &'g self,
        key: u64,
        mesh_key: u64,
        geometry: &'g (Vec<LabVertex>, Vec<u32>),
        at: Vec3,
    ) -> FrameMesh<'g> {
        FrameMesh {
            is_static: false,
            mesh_key,
            material_key: 1,
            object_key: key,
            vertices: geometry.0.as_slice(),
            indices: geometry.1.as_slice(),
            material: &self.material,
            model: Mat4::from_translation(at).to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    fn scene(&self) -> FrameScene<'_> {
        let mut meshes = vec![
            self.mesh(1, 1, &self.ground, Vec3::new(0.0, -0.05, 0.0)),
            self.mesh(2, 2, &self.slab, Vec3::new(0.0, 3.0, 0.0)),
        ];
        for i in 0..256usize {
            let (col, row) = ((i % 16) as f32, (i / 16) as f32);
            let at = Vec3::new(-2.4 + col * 0.32, 0.25, -2.4 + row * 0.32);
            meshes.push(self.mesh(i as u64 + 10, 3, &self.cube, at));
        }
        let base = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        FrameScene {
            width: 96,
            height: 64,
            eye: [0.0, 2.5, 7.5],
            target: [0.0, 0.5, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 40.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.5,
            double_sided: false,
            post: None,
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([-0.15, -1.0, -0.1], [1.0, 1.0, 1.0], 3.0),
            local_lights: &[],
            quality: FrameQuality {
                csm_cascade_count: 2,
                csm_resolution: 512,
                // Paging off: every cascade is redrawn (and culled) every frame.
                paged_sun_shadows: false,
                taau: false,
                ..base
            },
            meshes,
        }
    }
}

/// Allocations per frame of `BATCH` steady frames and the cull's work per frame.
fn run(device: &RenderDevice, on: bool) -> (f64, ShadowOcclusionWork, usize) {
    let geometry = Geometry {
        ground: box_mesh([8.0, 0.05, 8.0]),
        slab: box_mesh([3.2, 0.05, 3.2]),
        cube: box_mesh([0.1, 0.1, 0.1]),
        material: LabMaterial::default(),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_shadow_occlusion(on);
    renderer.set_shadow_occlusion_min_casters(Some(64));
    let scene = geometry.scene();
    for _ in 0..6 {
        renderer.render_offscreen(device, &scene).expect("warm frame");
    }
    let kinds = renderer
        .shadow_occlusion_stats()
        .kinds
        .iter()
        .filter(|k| k.active)
        .count();
    let work_before = shadow_occlusion_work();
    let start = allocs();
    for _ in 0..BATCH {
        renderer.render_offscreen(device, &scene).expect("steady frame");
    }
    let per_frame = (allocs() - start) as f64 / BATCH as f64;
    let w = shadow_occlusion_work();
    let work = ShadowOcclusionWork {
        compute_passes: (w.compute_passes - work_before.compute_passes) / BATCH,
        pyramid_passes: (w.pyramid_passes - work_before.pyramid_passes) / BATCH,
        phase1_dispatches: (w.phase1_dispatches - work_before.phase1_dispatches) / BATCH,
        phase2_dispatches: (w.phase2_dispatches - work_before.phase2_dispatches) / BATCH,
    };
    (per_frame, work, kinds)
}

/// The cull's own passes on a culled frame: one compute pass for every culled kind's phase 1, and
/// one per kind for its pyramid with its phase 2 (until ADR-2482: three per kind). On the base of
/// the change this frame took six compute passes and +391 allocations.
#[test]
fn a_culled_frame_records_one_phase1_pass_and_one_pass_per_kind() {
    let device = RenderDevice::headless().expect("headless device (lavapipe)");
    if !IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
        eprintln!("skip: the device draws no GPU objects");
        return;
    }
    let (off, off_work, _) = run(&device, false);
    let (on, on_work, kinds) = run(&device, true);
    let delta = on - off;
    eprintln!(
        "allocations per frame: off {off}, on {on} (+{delta}); work per culled frame {on_work:?}, {kinds} kinds"
    );
    assert_eq!(
        off_work,
        ShadowOcclusionWork::default(),
        "the unculled frame records no cull"
    );
    assert!(kinds >= 2, "both cascades cull");
    assert_eq!(
        on_work,
        ShadowOcclusionWork {
            compute_passes: 1 + kinds as u64,
            pyramid_passes: kinds as u64,
            phase1_dispatches: kinds as u64,
            phase2_dispatches: kinds as u64,
        }
    );
    // Measured +322 on lavapipe after the merge (+391 before it). What is left is wgpu's: the
    // phase 2 render pass of each kind, the indirect-draw validation of both passes of each kind,
    // and the compute passes; the engine's own lists are reused (`shadow_occlusion_steady.rs`).
    // The exact budget holds on the lavapipe reference; another adapter's wgpu backend allocates
    // differently, so there only the per-pass bound applies.
    let budget = if lavapipe(&device) {
        ALLOC_BUDGET_LAVAPIPE
    } else {
        WGPU_PER_PASS * (on_work.compute_passes + 3 * kinds as u64) as f64
    };
    assert!(
        delta <= budget,
        "a culled frame allocates {delta} more than an unculled one (budget {budget})"
    );
}

/// The most a culled frame of this scene may allocate per frame over the unculled one on
/// lavapipe: the measured +322 and a little slack (the base of ADR-2482 measured +391).
const ALLOC_BUDGET_LAVAPIPE: f64 = 330.0;

/// Generous allocations per pass (render or compute, with its share of `finish` and the
/// indirect-draw validation) on other adapters.
const WGPU_PER_PASS: f64 = 60.0;

fn lavapipe(device: &RenderDevice) -> bool {
    device
        .adapter
        .get_info()
        .name
        .to_ascii_lowercase()
        .contains("llvmpipe")
}
