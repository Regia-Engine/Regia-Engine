// SPDX-License-Identifier: MPL-2.0
//! Plan row W-22 (ADR-8413): an eye inside a sealed cell of the Switchbacks cook draws only the
//! terrain nodes and scatter instances its portals show (`regia_render::viscells::OutdoorPortals`).
//! Counters only, no wall clock, no golden. The terrain tests render through `FrameRenderer` on
//! the headless device (lavapipe); the scatter tests are CPU only.
//!
//! The cells are the ones the W-21 cook emits for Switchbacks (`<map>.viscells.ron`); the terrain
//! is a flat heightfield at the height of the lowest mouth, over the cook's 1280 m map.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use regia_core::math::{Mat4, Vec3};
use regia_render::viscells::{OUTDOOR_CELL, OutdoorPortals, TraversalScratch, VisCells, outdoor_portals};
use regia_render::{
    DirectionalLightUniform, ExtractedMesh, FrameRenderer, FrameTarget, Frustum, LabMaterial, LabScene,
    RenderDevice, SharedPassDraw,
};
use regia_terrain::scatter::field::{ScatterField, ScatterLibrary, ScatterSource, ScatterView};
use regia_terrain::scatter::rock::{RockStyle, procedural_rock};
use regia_terrain::scatter::set::{ScatterClass, ScatterMeshRef, ScatterSet};
use regia_terrain::{CdlodConfig, TerrainDrawStats, TerrainLayers, TerrainMap, TerrainSharedDraw};
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
        // The Switchbacks test cook authors ADR-8359's ramp-facet waiver, as the other test cooks do (ADR-8436).
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

/// An eye inside a cell, some metres in from one of its mouths, past the mouth's doorway (an eye in
/// a doorway to the outdoors is never restricted), and that mouth's portal index.
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

struct Rig {
    renderer: FrameRenderer,
    stats: Arc<TerrainDrawStats>,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    lab: LabScene,
}

impl Rig {
    fn new(height: f32) -> Self {
        let device = device();
        let mut draw = TerrainSharedDraw::new(device, flat_map(height), &layers(), CdlodConfig::default(), 6)
            .expect("terrain");
        draw.prewarm(wgpu::TextureFormat::Rgba16Float);
        let stats = draw.stats();
        stats.set_record_camera_nodes(true);
        let mut renderer = FrameRenderer::new();
        renderer.set_shared_draw(Some(Box::new(draw) as Box<dyn SharedPassDraw + Send + Sync>));
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("interior cull"),
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
            stats,
            _texture: texture,
            view,
            lab,
        }
    }

    /// One frame from `eye` toward `target` with `portals` handed to the terrain (`None`: the
    /// cull off). Returns the drawn node boxes.
    fn frame(
        &mut self,
        eye: Vec3,
        target: Vec3,
        portals: Option<&OutdoorPortals>,
    ) -> Vec<([f32; 3], [f32; 3])> {
        self.renderer.set_outdoor_portals(portals);
        self.lab.eye = eye.to_array();
        self.lab.target = target.to_array();
        let scene = self.lab.as_frame_scene();
        let target = FrameTarget::full(&self.view, wgpu::TextureFormat::Rgba8UnormSrgb, W, H);
        self.renderer
            .render_into(device(), &scene, target)
            .unwrap_or_else(|err| panic!("frame: {err}"));
        let bounds = self.stats.camera_node_bounds();
        assert_eq!(bounds.len() as u32, self.stats.camera_nodes());
        bounds
    }
}

fn bits(list: &[([f32; 3], [f32; 3])]) -> Vec<[u32; 6]> {
    list.iter()
        .map(|(lo, hi)| [lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]].map(f32::to_bits))
        .collect()
}

/// The plan bar (spec research § 6, do27-absorb-plan § 4.7): from inside the tube, summed over
/// eight headings, the camera passes draw at least half fewer terrain nodes than with the cull off.
#[test]
fn in_tube_terrain_nodes_drawn_drop_by_half() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (cell, eye) = tube_eye(cells);
    let mut rig = Rig::new(ground_height(cells));
    let (mut off, mut on, mut culled) = (0u32, 0u32, 0u32);
    for target in headings(eye) {
        let portals = portals_for(cells, eye, target);
        assert!(portals.is_restricted(), "the eye is inside cell index {cell}");
        off += rig.frame(eye, target, None).len() as u32;
        assert_eq!(rig.stats.portal_culled_nodes(), 0);
        let counts = rig.renderer.outdoor_cull_counts().expect("the terrain reports");
        assert!(!counts.restricted);
        on += rig.frame(eye, target, Some(&portals)).len() as u32;
        let counts = rig.renderer.outdoor_cull_counts().expect("the terrain reports");
        assert!(counts.restricted, "{counts:?}");
        assert_eq!(counts.nodes_drawn, rig.stats.camera_nodes());
        culled += rig.stats.portal_culled_nodes();
        eprintln!(
            "heading {target}: frusta {} drawn {} culled {}",
            portals.frusta().len(),
            counts.nodes_drawn,
            counts.nodes_culled
        );
    }
    eprintln!("eye {eye} in cell index {cell}: cull off {off} nodes, on {on} ({culled} culled)");
    assert!(off > 0, "the view frustum holds terrain");
    assert_eq!(
        on + culled,
        off,
        "every node is either drawn or culled by the portals"
    );
    assert!(on * 2 <= off, "{on} of {off} nodes drawn: less than half fewer");
}

/// An eye outdoors is never restricted: the drawn node list is bit for bit the one drawn without
/// cells, at every heading.
#[test]
fn outdoor_eye_draws_the_same() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let h = ground_height(cells);
    let mut rig = Rig::new(h);
    let eye = Vec3::new(37.0, h + 40.0, 410.0);
    assert_eq!(cells.locate_index(eye), cells.index_of(OUTDOOR_CELL).unwrap());
    for target in headings(eye).into_iter().map(|t| t - Vec3::Y * 30.0) {
        let portals = portals_for(cells, eye, target);
        assert!(!portals.is_restricted(), "{portals:?}");
        let base = rig.frame(eye, target, None);
        let with = rig.frame(eye, target, Some(&portals));
        assert!(!base.is_empty());
        assert_eq!(bits(&base), bits(&with), "heading {target}");
        assert_eq!(rig.stats.portal_culled_nodes(), 0);
    }
}

/// The square-pyramid test, written apart from the engine's: the box `lo`..`hi` against the
/// planes through `eye` and each edge of `quad` and the quad's own plane (facing away from the
/// eye). True when no plane has the whole box outside.
fn box_meets_pyramid(eye: Vec3, quad: &[Vec3; 4], lo: [f32; 3], hi: [f32; 3]) -> bool {
    let centre = (quad[0] + quad[1] + quad[2] + quad[3]) / 4.0;
    let mut planes = Vec::new();
    for i in 0..4 {
        let (a, b) = (quad[i], quad[(i + 1) % 4]);
        let mut n = (a - eye).cross(b - eye).normalize();
        if n.dot(centre - eye) < 0.0 {
            n = -n;
        }
        planes.push((n, -n.dot(eye)));
    }
    let mut n = (quad[1] - quad[0]).cross(quad[3] - quad[0]).normalize();
    if n.dot(centre - eye) < 0.0 {
        n = -n;
    }
    planes.push((n, -n.dot(centre)));
    planes.iter().all(|(n, d)| {
        let far = Vec3::new(
            if n.x >= 0.0 { hi[0] } else { lo[0] },
            if n.y >= 0.0 { hi[1] } else { lo[1] },
            if n.z >= 0.0 { hi[2] } else { lo[2] },
        );
        n.dot(far) + d >= 0.0
    })
}

/// From inside, looking out of a mouth: every node the cull-off frame draws that meets the mouth's
/// pyramid is still drawn, and the walls hide the rest.
#[test]
fn portal_view_keeps_what_the_mouth_shows() {
    let _gpu = SERIAL_GPU.lock().unwrap_or_else(PoisonError::into_inner);
    let cells = switchbacks_cells();
    let (eye, portal) = eye_by_a_mouth(cells);
    let mouth = &cells.portals[portal];
    // Look out through the mouth's centre, then a little off to each side of it.
    let out = mouth.center - eye;
    let side = Vec3::new(-out.z, 0.0, out.x).normalize_or_zero() * out.length() * 0.4;
    let mut rig = Rig::new(ground_height(cells));
    let mut through = 0usize;
    for (i, target) in [mouth.center, mouth.center + side, mouth.center - side]
        .into_iter()
        .enumerate()
    {
        let target = eye + (target - eye) * 50.0;
        let mut scratch = TraversalScratch::default();
        let mut portals = OutdoorPortals::default();
        let walk = outdoor_portals(
            cells,
            eye.to_array(),
            frustum(eye, target),
            [W, H],
            &mut scratch,
            &mut portals,
        );
        // Straight out of the mouth the list is restricted; turned aside the walk may reach a cap
        // and leave the outdoors unrestricted (then nothing is culled, which also keeps the mouth).
        assert!(i > 0 || portals.is_restricted(), "{portals:?} {walk:?}");
        let off = rig.frame(eye, target, None);
        let on = bits(&rig.frame(eye, target, Some(&portals)));
        let shown: Vec<_> = off
            .iter()
            .filter(|(lo, hi)| box_meets_pyramid(eye, &mouth.quad, *lo, *hi))
            .collect();
        through += shown.len();
        for node in &shown {
            assert!(
                on.contains(&bits(&[**node])[0]),
                "a node the mouth shows was culled: {node:?} (eye {eye}, mouth {:?})",
                mouth.quad
            );
        }
        eprintln!(
            "eye {eye} target {target}: restricted {} frusta {} off {} on {} through the mouth {} ({walk:?})",
            portals.is_restricted(),
            portals.frusta().len(),
            off.len(),
            on.len(),
            shown.len()
        );
    }
    assert!(through > 0, "the mouth shows terrain");
}

/// A rock field on the flat map: one rock about every 4 m out to 200 m.
fn rock_field(height: f32) -> ScatterField {
    let set = ScatterSet {
        classes: vec![ScatterClass {
            name: "boulders".into(),
            meshes: vec![ScatterMeshRef {
                mesh: "rock_a".into(),
                weight: 1.0,
                min_scale: 0.5,
                max_scale: 1.5,
                scale_bias: 1.0,
            }],
            cell_size: 4.0,
            density: 1.0,
            fade_start: 150.0,
            fade_end: 200.0,
            ..ScatterClass::default()
        }],
        ..ScatterSet::default()
    };
    let mut library = ScatterLibrary::new();
    library.insert(
        "rock_a".into(),
        procedural_rock(3, RockStyle::angular([0.5, 0.4, 0.35])),
    );
    let mut field =
        ScatterField::new(&set, ScatterSource::new(Arc::new(flat_map(height))), &library, 1).expect("field");
    field.set_background_streaming(false);
    field
}

fn keys(list: &[ExtractedMesh]) -> Vec<(u64, [[u32; 4]; 4], u64)> {
    list.iter()
        .map(|m| (m.object_key_xor, m.model.map(|c| c.map(f32::to_bits)), m.lod_key))
        .collect()
}

/// From inside the tube, summed over the eight headings, the scatter emits at least half fewer
/// instances than with the cull off: the same plan bar as the terrain. The blocks are kept with the
/// instances' radius and shadow reach as margin, and caster blocks within shadow reach of a mouth
/// stay, so the bar is lower than what the walls hide; at least half still holds because the
/// mouths take a small part of the eight headings' view.
#[test]
fn in_tube_scatter_instances_drop() {
    let cells = switchbacks_cells();
    let (_, eye) = tube_eye(cells);
    // Two identical fields, so the LOD state of one call never feeds the other.
    let (mut plain, mut culling) = (rock_field(ground_height(cells)), rock_field(ground_height(cells)));
    let (mut off, mut on, mut culled) = (0u32, 0u32, 0u32);
    for target in headings(eye) {
        let view = view(eye, target);
        plain.update(eye.to_array());
        culling.update(eye.to_array());
        let mut base = Vec::new();
        let stats_off = plain.extract_meshes(&view, &mut base);
        let portals = portals_for(cells, eye, target);
        let mut through = Vec::new();
        let stats_on = culling.extract_meshes_through(&view, &portals, &mut through);
        assert_eq!(stats_off.portal_culled, 0);
        off += stats_off.emitted;
        on += stats_on.emitted;
        culled += stats_on.portal_culled;
        // What is still drawn is drawn exactly as before (a subset, same keys and matrices).
        let base = keys(&base);
        for k in keys(&through) {
            assert!(base.contains(&k), "an emitted instance changed: {k:?}");
        }
        eprintln!(
            "heading {target}: emitted off {} on {} portal_culled {}",
            stats_off.emitted, stats_on.emitted, stats_on.portal_culled
        );
    }
    eprintln!("scatter: off {off} on {on} portal_culled {culled}");
    assert!(off > 0, "the rock field is in view");
    assert!(culled > 0);
    assert!(
        on * 2 <= off,
        "{on} of {off} instances emitted: less than half fewer"
    );
}

/// The scatter outdoors: `extract_meshes_through` with the outdoor eye's (unrestricted) list emits
/// exactly what `extract_meshes` does.
#[test]
fn outdoor_eye_emits_the_same_scatter() {
    let cells = switchbacks_cells();
    let h = ground_height(cells);
    let eye = Vec3::new(37.0, h + 3.0, 410.0);
    let (mut plain, mut through) = (rock_field(h), rock_field(h));
    for target in headings(eye) {
        let view = view(eye, target);
        plain.update(eye.to_array());
        through.update(eye.to_array());
        let portals = portals_for(cells, eye, target);
        assert!(!portals.is_restricted());
        let (mut base, mut with) = (Vec::new(), Vec::new());
        let a = plain.extract_meshes(&view, &mut base);
        let b = through.extract_meshes_through(&view, &portals, &mut with);
        assert!(a.emitted > 0);
        assert_eq!(a, b);
        assert_eq!(keys(&base), keys(&with));
    }
}
