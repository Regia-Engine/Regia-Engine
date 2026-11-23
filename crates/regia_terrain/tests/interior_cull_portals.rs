// SPDX-License-Identifier: MPL-2.0
//! Plan v0.7 item E (ADR-8843, KI-RN-008): an eye inside a sealed cell of the Switchbacks cook
//! draws only the grass, scatter impostors and water its portals show, as W-22 (ADR-8413) did for
//! the terrain nodes and the scatter meshes. Counters only, no wall clock, no golden. Every test
//! renders through `FrameRenderer` on the headless device (lavapipe).
//!
//! The cells are the ones the W-21 cook emits for Switchbacks (`<map>.viscells.ron`); the terrain
//! is a flat heightfield at the height of the lowest mouth, over the cook's 1280 m map (the
//! fixture of `interior_terrain_scatter_cull.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use regia_core::math::{Mat4, Vec3};
use regia_render::viscells::{
    OUTDOOR_CELL, OutdoorCullCounts, OutdoorPortals, TraversalScratch, VisCells, outdoor_portals,
};
use regia_render::{
    DirectionalLightUniform, FrameRenderer, FrameTarget, Frustum, LabMaterial, LabScene, RenderDevice,
    SharedPassDraw,
};
use regia_terrain::water_body::WaterFrameStats;
use regia_terrain::{
    CdlodConfig, GrassLayer, GrassMask, GrassMaskInputs, GrassRequest, GrassSet, ImpostorLayer,
    ImpostorSettings, RockStyle, ScatterField, ScatterLibrary, ScatterSet, ScatterSource, ScatterView,
    TerrainGrassHandle, TerrainLayers, TerrainMap, TerrainSharedDraw, WaterBody, WaterLook, WaterRenderer,
    procedural_rock,
};
use regia_world_geom::checks::{only_ramp_facet_failures, waive_ramp_facets};
use regia_world_geom::compat::fixtures::for_map;
use regia_world_geom::compat::sampler::{HeightGrid, TerrainSampler};
use regia_world_geom::cook::{CookError, CookInputs, GroundInput, cook};
use regia_world_geom::passages::PassagesDocument;

const W: u32 = 96;
const H: u32 = 54;
const FOV_Y: f32 = 1.0;
const NEAR: f32 = 0.1;
const FAR: f32 = 2000.0;
/// The cook's map width, metres (centred on the origin).
const MAP_M: f32 = 1280.0;

/// The Switchbacks cells, cooked as in `viscells_from_bake.rs`.
fn switchbacks_cells() -> &'static VisCells {
    static CELLS: OnceLock<VisCells> = OnceLock::new();
    CELLS.get_or_init(|| {
        let terrain = TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).unwrap(), MAP_M)
            .with_zero_counts(16384);
        let ground = GroundInput {
            lattice_n: 4,
            width_m: MAP_M,
            offset_m: -512.0,
            step_m: 1.0 / 32.0,
            counts: vec![8192; 16],
        };
        let passages = PassagesDocument::from_layout(for_map("switchbacks").unwrap())
            .to_ron()
            .unwrap();
        let read = |_: &str| Ok(passages.as_bytes().to_vec());
        let doc = "(version: 1, ground_tile_cells: 16, layers: [(id: \"tubes\", source: Passages(path: \"sb.passages.ron\"))])";
        let run = |document: &str| {
            cook(&CookInputs {
                document: document.as_bytes(),
                ground: &ground,
                terrain: Some(&terrain),
                read_file: &read,
            })
        };
        let out = match run(doc) {
            Ok(out) => out,
            Err(CookError::Checks(report)) if only_ramp_facet_failures(&report) => {
                run(&waive_ramp_facets(doc, &report).unwrap()).unwrap()
            }
            Err(e) => panic!("{e} ({})", e.code()),
        };
        VisCells::from_ron(&out.viscells_ron).expect("the runtime loads the emitted file")
    })
}

/// The portals between an indoor cell and the outdoors, as (cell index, portal index).
fn mouths(cells: &VisCells) -> Vec<(usize, usize)> {
    let outdoor = cells.index_of(OUTDOOR_CELL).unwrap();
    cells
        .portals
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            if p.a as usize == outdoor {
                Some((p.b as usize, i))
            } else if p.b as usize == outdoor {
                Some((p.a as usize, i))
            } else {
                None
            }
        })
        .collect()
}

/// The terrain height: the bottom of the lowest mouth (the ground the tubes open onto).
fn ground_height(cells: &VisCells) -> f32 {
    mouths(cells)
        .iter()
        .flat_map(|&(_, p)| cells.portals[p].quad.iter().map(|q| q.y))
        .fold(f32::INFINITY, f32::min)
}

/// The tube eye of the plan bar: the centre of the first indoor cell's first box that locates to
/// that cell (the cells come in id order).
fn tube_eye(cells: &VisCells) -> (usize, Vec3) {
    let outdoor = cells.index_of(OUTDOOR_CELL).unwrap();
    for (index, cell) in cells.cells.iter().enumerate() {
        if index == outdoor {
            continue;
        }
        for b in &cell.bounds {
            let centre = Vec3::from_array(b.center);
            if cells.locate_index(centre) == index {
                return (index, centre);
            }
        }
    }
    panic!("no indoor cell box locates to its own cell");
}

/// An eye inside a cell, some metres in from one of its mouths, past the mouth's doorway, and that
/// mouth's portal index.
fn eye_by_a_mouth(cells: &VisCells) -> (Vec3, usize) {
    for (cell, portal) in mouths(cells) {
        let p = &cells.portals[portal];
        for b in &cells.cells[cell].bounds {
            let inward = (Vec3::from_array(b.center) - p.center).normalize_or_zero();
            let inward = Vec3::new(inward.x, 0.0, inward.z).normalize_or_zero();
            for metres in [6.0_f32, 10.0, 15.0, 20.0, 30.0, 40.0] {
                let eye = p.center + inward * metres;
                if cells.locate_index(eye) != cell {
                    continue;
                }
                let toward = portals_for(cells, eye, eye + (p.center - eye) * 50.0);
                if toward.is_restricted() && !toward.frusta().is_empty() {
                    return (eye, portal);
                }
            }
        }
    }
    panic!("no eye found inside a cell that sees out of its mouth");
}

fn view(eye: Vec3, target: Vec3) -> ScatterView {
    ScatterView::look_at(
        eye.to_array(),
        target.to_array(),
        FOV_Y,
        W as f32 / H as f32,
        NEAR,
        FAR,
    )
}

fn frustum(eye: Vec3, target: Vec3) -> [[f32; 4]; 6] {
    Frustum::from_view_proj(&Mat4::from_cols_array_2d(&view(eye, target).view_proj)).planes()
}

/// The outdoor list the walk gives an eye looking at `target`.
fn portals_for(cells: &VisCells, eye: Vec3, target: Vec3) -> OutdoorPortals {
    let mut scratch = TraversalScratch::default();
    let mut out = OutdoorPortals::default();
    outdoor_portals(
        cells,
        eye.to_array(),
        frustum(eye, target),
        [W, H],
        &mut scratch,
        &mut out,
    );
    out
}

/// Eight horizontal headings around `eye`, 100 m out.
fn headings(eye: Vec3) -> Vec<Vec3> {
    (0..8)
        .map(|i| {
            let a = i as f32 * std::f32::consts::FRAC_PI_4;
            eye + Vec3::new(a.cos() * 100.0, 0.0, a.sin() * 100.0)
        })
        .collect()
}

fn layers() -> TerrainLayers {
    TerrainLayers::from_ron(
        r#"TerrainLayers(version: 1, wetness_default: 0.0, layers: [LayerDesc(name: "ground",
            albedo: (0.30, 0.28, 0.22), roughness: 0.9, tiling: 4.0, slope_min: 0.0, slope_max: 1.6,
            altitude_min: -5000.0, altitude_max: 5000.0, blend_sharpness: 0.15, triplanar: false)])"#,
    )
    .expect("layers")
}

/// A flat 257 × 257 map at `height` over the cook's map.
fn flat_map(height: f32) -> TerrainMap {
    let n = 257;
    TerrainMap::from_heights(n, n, vec![height; (n * n) as usize], MAP_M, 1.0, 0.0).expect("map")
}

fn device() -> &'static RenderDevice {
    static DEVICE: OnceLock<RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// The tests of this binary share one device and take turns on it.
static SERIAL_GPU: Mutex<()> = Mutex::new(());

/// A rock field whose far rocks (past 60 m) are drawn as impostors out to the class's far range.
fn rock_set() -> ScatterSet {
    ScatterSet::from_ron(
        r#"ScatterSet(version: 2, seed: 31, classes: [ScatterClass(name: "boulders",
            meshes: [ScatterMeshRef(mesh: "rock_a", weight: 1.0, min_scale: 2.0, max_scale: 4.0, scale_bias: 1.0)],
            cell_size: 10.0, density: 0.5, slope_max: 0.8, contact_ao: 0.0, cast_shadows: false,
            fade_start: 40.0, fade_end: 60.0)])"#,
        Some(&layers()),
    )
    .expect("scatter set")
}

/// What a [`Rig`] binds to its terrain draw and its frame renderer.
#[derive(Clone, Copy, Default)]
struct Bind {
    grass: bool,
    impostors: bool,
    /// Lakes in a 4 × 4 grid over the map (16 bodies).
    lakes: bool,
}

struct Rig {
    renderer: FrameRenderer,
    grass: Option<TerrainGrassHandle>,
    field: Option<ScatterField>,
    impostors: Option<ImpostorLayer>,
    water: Option<WaterRenderer>,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    lab: LabScene,
}

/// The lakes of [`Bind::lakes`]: 16 squares of 60 m, 300 m apart, a little over the ground.
fn lakes(height: f32) -> Vec<WaterBody> {
    let mut out = Vec::new();
    for j in 0..4 {
        for i in 0..4 {
            let c = [-450.0 + 300.0 * i as f32, height + 0.2, -450.0 + 300.0 * j as f32];
            out.push(WaterBody::plane(c, [30.0, 30.0], WaterLook::lake()));
        }
    }
    out
}

impl Rig {
    fn new(height: f32, bind: Bind) -> Self {
        Self::with_lakes(height, bind, &lakes(height))
    }

    fn with_lakes(height: f32, bind: Bind, lake_list: &[WaterBody]) -> Self {
        let device = device();
        let map = flat_map(height);
        let layers = layers();
        let mut draw =
            TerrainSharedDraw::new(device, map.clone(), &layers, CdlodConfig::default(), 6).expect("terrain");
        let mut grass = None;
        if bind.grass {
            let set = GrassSet::winter();
            let mask = GrassMask::build(
                &set,
                &GrassMaskInputs {
                    layers: Some(&layers),
                    texel_m: 1.0,
                    ..GrassMaskInputs::new(&map)
                },
            )
            .expect("mask");
            let layer = GrassLayer::new(&device.device, &device.queue, set, mask).expect("grass layer");
            let handle = layer.handle();
            handle.set_request(GrassRequest {
                enabled: true,
                ..GrassRequest::default()
            });
            draw.set_grass(Some(layer));
            grass = Some(handle);
        }
        let (mut field, mut impostors) = (None, None);
        if bind.impostors {
            let mut library = ScatterLibrary::new();
            library.insert(
                "rock_a".into(),
                procedural_rock(1, RockStyle::angular([0.3, 0.24, 0.2])),
            );
            let source = ScatterSource {
                map: Arc::new(map.clone()),
                holes: None,
                layers: Some(layers.clone()),
                density_maps: regia_terrain::DensityMaps::new(),
            };
            let mut f = ScatterField::new(&rock_set(), source, &library, 0xB7).expect("field");
            f.set_background_streaming(false);
            let settings = ImpostorSettings {
                grid: 8,
                cell: 32,
                ..ImpostorSettings::default()
            };
            f.set_impostors(Some(settings));
            let atlas = f.bake_impostors(Some(settings));
            let layer = ImpostorLayer::new(&device.device, &device.queue, &atlas);
            draw.set_impostors(Some(&layer));
            field = Some(f);
            impostors = Some(layer);
        }
        draw.prewarm(wgpu::TextureFormat::Rgba16Float);
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let mut water = None;
        if bind.lakes {
            let w = WaterRenderer::new(device);
            let refs: Vec<&WaterBody> = lake_list.iter().collect();
            w.set_bodies(&refs);
            w.set_ambient_lux(8_000.0);
            renderer.set_translucent_layers(vec![w.layer()]);
            water = Some(w);
        }
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("interior cull portals"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let lab = LabScene {
            width: W,
            height: H,
            eye: [0.0; 3],
            target: [0.0, 0.0, -1.0],
            fov_y: FOV_Y,
            near: NEAR,
            far: FAR,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.3, -0.8, -0.4], [1.0, 0.97, 0.92], 110_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: Vec::new(),
        };
        Self {
            renderer,
            grass,
            field,
            impostors,
            water,
            _texture: texture,
            view,
            lab,
        }
    }

    /// One frame from `eye` toward `target` with `portals` handed to the frame (`None`: the cull
    /// off). Returns what the terrain draw culled and the water's frame stats.
    fn frame(
        &mut self,
        eye: Vec3,
        target: Vec3,
        portals: Option<&OutdoorPortals>,
    ) -> (OutdoorCullCounts, WaterFrameStats) {
        if let (Some(field), Some(layer)) = (self.field.as_mut(), self.impostors.as_mut()) {
            field.update(eye.to_array());
            layer.update(&device().queue, field);
        }
        self.renderer.set_outdoor_portals(portals);
        self.lab.eye = eye.to_array();
        self.lab.target = target.to_array();
        let scene = self.lab.as_frame_scene();
        let target = FrameTarget::full(&self.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H);
        self.renderer
            .render_into(device(), &scene, target)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        let counts = self.renderer.outdoor_cull_counts().expect("the terrain reports");
        let water = self.water.as_ref().map(WaterRenderer::stats).unwrap_or_default();
        (counts, water)
    }
}

/// From inside the tube, summed over eight headings, the grass camera patches drop to at most half
/// of the cull-off count (the W-22 terrain bar), every patch is either drawn or culled, and the
/// grass report's camera patches follow the counts.
#[test]
fn in_tube_grass_patches_drop() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (cell, eye) = tube_eye(cells);
    let mut rig = Rig::new(
        ground_height(cells),
        Bind {
            grass: true,
            ..Bind::default()
        },
    );
    let handle = rig.grass.clone().expect("grass bound");
    let (mut off, mut on, mut culled) = (0u32, 0u32, 0u32);
    for target in headings(eye) {
        let portals = portals_for(cells, eye, target);
        assert!(portals.is_restricted(), "the eye is inside cell index {cell}");
        let (base, _) = rig.frame(eye, target, None);
        assert!(!base.restricted);
        assert_eq!(base.grass_patches_culled, 0);
        let report_off = handle.report().camera_patches;
        assert_eq!(base.grass_patches_drawn, report_off);
        let (with, _) = rig.frame(eye, target, Some(&portals));
        assert!(with.restricted, "{with:?}");
        let report_on = handle.report().camera_patches;
        assert_eq!(with.grass_patches_drawn, report_on);
        assert!(report_on <= report_off, "the cull never adds a camera patch");
        eprintln!(
            "heading {target}: frusta {} grass camera patches off {report_off} on {report_on} culled {}",
            portals.frusta().len(),
            with.grass_patches_culled
        );
        off += report_off;
        on += report_on;
        culled += with.grass_patches_culled;
    }
    eprintln!("eye {eye} in cell index {cell}: grass camera patches off {off}, on {on} ({culled} culled)");
    assert!(off > 0, "the view frustum holds grass");
    assert!(culled > 0);
    assert!(
        on * 2 <= off,
        "{on} of {off} camera patches drawn: less than half fewer"
    );
}

/// From inside the tube, summed over eight headings, the impostor instances the GPU cull walks
/// drop to at most half: the tiles in no portal frustum are masked out.
#[test]
fn in_tube_impostors_drop() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (cell, eye) = tube_eye(cells);
    let mut rig = Rig::new(
        ground_height(cells),
        Bind {
            impostors: true,
            ..Bind::default()
        },
    );
    let (mut off, mut on, mut culled) = (0u32, 0u32, 0u32);
    for target in headings(eye) {
        let portals = portals_for(cells, eye, target);
        assert!(portals.is_restricted(), "the eye is inside cell index {cell}");
        let (base, _) = rig.frame(eye, target, None);
        assert_eq!(base.impostors_culled, 0);
        let live = rig.impostors.as_ref().map_or(0, ImpostorLayer::count);
        assert_eq!(
            base.impostors_kept, live,
            "unrestricted: every live impostor is walked"
        );
        let (with, _) = rig.frame(eye, target, Some(&portals));
        assert!(with.restricted);
        assert_eq!(
            with.impostors_kept + with.impostors_culled,
            live,
            "every live impostor is kept or culled"
        );
        eprintln!(
            "heading {target}: frusta {} impostors live {live} kept {} culled {}",
            portals.frusta().len(),
            with.impostors_kept,
            with.impostors_culled
        );
        off += base.impostors_kept;
        on += with.impostors_kept;
        culled += with.impostors_culled;
    }
    eprintln!("eye {eye} in cell index {cell}: impostors off {off}, on {on} ({culled} culled)");
    assert!(off > 0, "the far field holds impostors");
    assert!(culled > 0);
    assert!(
        on * 2 <= off,
        "{on} of {off} impostors kept: less than half fewer"
    );
}

/// From inside the tube, summed over eight headings, the lakes drawn drop to at most half.
#[test]
fn in_tube_water_bodies_drop() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (cell, eye) = tube_eye(cells);
    let mut rig = Rig::new(
        ground_height(cells),
        Bind {
            lakes: true,
            ..Bind::default()
        },
    );
    let (mut off, mut on, mut culled) = (0u32, 0u32, 0u32);
    for target in headings(eye) {
        let portals = portals_for(cells, eye, target);
        assert!(portals.is_restricted(), "the eye is inside cell index {cell}");
        let (_, base) = rig.frame(eye, target, None);
        assert!(!base.portal_restricted);
        assert_eq!(base.bodies_drawn, 16, "{base:?}");
        let (_, with) = rig.frame(eye, target, Some(&portals));
        assert!(
            with.portal_restricted,
            "the water matched the frame's eye: {with:?}"
        );
        assert_eq!(with.bodies_drawn + with.portal_culled_bodies, 16);
        eprintln!(
            "heading {target}: frusta {} lakes drawn {} culled {}",
            portals.frusta().len(),
            with.bodies_drawn,
            with.portal_culled_bodies
        );
        off += base.bodies_drawn;
        on += with.bodies_drawn;
        culled += with.portal_culled_bodies;
    }
    eprintln!("eye {eye} in cell index {cell}: lakes off {off}, on {on} ({culled} culled)");
    assert!(culled > 0);
    assert!(on * 2 <= off, "{on} of {off} lakes drawn: less than half fewer");
}

/// From inside, looking out of a mouth: a lake straight out of the mouth is drawn, a lake behind
/// the eye is not; the grass patches the mouth shows stay camera patches.
#[test]
fn portal_view_keeps_what_the_mouth_shows() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (eye, portal) = eye_by_a_mouth(cells);
    let mouth = &cells.portals[portal];
    let h = ground_height(cells);
    let out = Vec3::new(mouth.center.x - eye.x, 0.0, mouth.center.z - eye.z).normalize_or_zero();
    let ahead = mouth.center + out * 60.0;
    let behind = eye - out * 400.0;
    let pair = [
        WaterBody::plane(
            [ahead.x, mouth.center.y - 2.0, ahead.z],
            [20.0, 20.0],
            WaterLook::lake(),
        ),
        WaterBody::plane([behind.x, h + 0.2, behind.z], [20.0, 20.0], WaterLook::lake()),
    ];
    let mut rig = Rig::with_lakes(
        h,
        Bind {
            grass: true,
            lakes: true,
            ..Bind::default()
        },
        &pair,
    );
    let target = eye + (mouth.center - eye) * 50.0;
    let portals = portals_for(cells, eye, target);
    assert!(portals.is_restricted() && !portals.frusta().is_empty());
    let (base, water_off) = rig.frame(eye, target, None);
    let (with, water_on) = rig.frame(eye, target, Some(&portals));
    eprintln!(
        "eye {eye} out of mouth {portal}: lakes off {} on {} (culled {}); grass patches off {} on {} (culled {})",
        water_off.bodies_drawn,
        water_on.bodies_drawn,
        water_on.portal_culled_bodies,
        base.grass_patches_drawn,
        with.grass_patches_drawn,
        with.grass_patches_culled
    );
    assert_eq!(water_off.bodies_drawn, 2);
    assert!(water_on.portal_restricted);
    assert_eq!(
        (water_on.bodies_drawn, water_on.portal_culled_bodies),
        (1, 1),
        "the lake out of the mouth stays, the one behind the eye goes"
    );
    assert!(with.grass_patches_drawn > 0, "the mouth shows grass");
}

/// An eye outdoors is never restricted: grass, impostors and water draw what they draw without
/// cells.
#[test]
fn outdoor_eye_draws_the_same() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let h = ground_height(cells);
    let mut rig = Rig::new(
        h,
        Bind {
            grass: true,
            impostors: true,
            lakes: true,
        },
    );
    let handle = rig.grass.clone().expect("grass bound");
    let eye = Vec3::new(37.0, h + 3.0, 410.0);
    assert_eq!(cells.locate_index(eye), cells.index_of(OUTDOOR_CELL).unwrap());
    for target in headings(eye) {
        let portals = portals_for(cells, eye, target);
        assert!(!portals.is_restricted(), "{portals:?}");
        let (base, water_off) = rig.frame(eye, target, None);
        let patches_off = handle.report().camera_patches;
        let (with, water_on) = rig.frame(eye, target, Some(&portals));
        assert_eq!(handle.report().camera_patches, patches_off);
        assert_eq!(base, with, "heading {target}");
        assert_eq!(with.grass_patches_culled + with.impostors_culled, 0);
        assert!(!water_on.portal_restricted);
        assert_eq!(water_on.bodies_drawn, water_off.bodies_drawn);
        assert_eq!(water_on.portal_culled_bodies, 0);
    }
}
