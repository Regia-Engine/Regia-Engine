// SPDX-License-Identifier: MPL-2.0
//! The renderer behind the player: `regia_viewer`, drawing to whatever [`FrameTarget`] the host has (the
//! canvas surface in a browser, a texture in a test).

use std::collections::BTreeMap;

use regia_looks::Studio;
use regia_viewer::{
    BakedEnv, ContactShadow, DrawHandle, LutHandle, MaterialHandle, MeshHandle, ShaderFamily, ViewerCamera,
    ViewerConfig, ViewerMaterial, ViewerMeshData, ViewerRenderer, ViewerStyle, ViewerVertex,
};
use regia_viewer_pack::MeshData;

use crate::WebError;
use crate::seams::{CameraPose, DrawOutcome, PlannedMaterial, ProductModel, SceneBackend};

/// Where frames go.
pub trait FrameTarget {
    /// The drawing size changed.
    fn resize(&mut self, width: u32, height: u32);
    /// Runs `draw` on the next frame's texture and presents it, or says there is no frame right now.
    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(&wgpu::Texture) -> Result<(), WebError>,
    ) -> Result<DrawOutcome, WebError>;
}

/// The contact shadow's strength.
const SHADOW_OPACITY: f32 = 0.3;

fn render_error(e: regia_viewer::ViewerError) -> WebError {
    WebError::new("render_failed", e.to_string())
}

/// The renderer configuration of every product player: the `regia_looks` shaders for every family. The web player
/// and the Android viewer both come through here, so they draw the same looks as the stills.
pub fn player_config(target_format: wgpu::TextureFormat, sample_count: u32) -> ViewerConfig {
    let mut config = ViewerConfig::new(target_format);
    config.sample_count = sample_count;
    config.hooks = regia_looks::viewer::hooks();
    config
}

/// The renderer's material for a look: the `regia_looks` block of its family. `lut`: the row of an anodized metal's
/// lookup.
pub fn viewer_material(planned: &PlannedMaterial, lut: Option<LutHandle>) -> ViewerMaterial {
    regia_looks::viewer::viewer_material(&planned.look, planned.stone, lut)
}

/// How close two flat normals must be, per component, to be one facet.
const FACET_TOLERANCE: f32 = 1e-3;

/// Facet ids from flat normals: each new normal starts a facet, and a normal within
/// [`FACET_TOLERANCE`] of an earlier facet's normal (per component) takes the earliest such facet. The
/// facets sit in a grid of tolerance-sized cells, so a vertex looks at its own and the 26 next cells
/// instead of every facet so far (that made a smooth stone mesh O(vertices x facets)). A cell never holds
/// two facets: any two normals in one cell are within the tolerance.
#[derive(Default)]
struct FacetIds {
    normals: Vec<[f32; 3]>,
    cells: std::collections::HashMap<[i64; 3], u32>,
}

#[cfg(test)]
thread_local! {
    /// Facet normals compared, for the test that pins the numbering as linear.
    static FACET_COMPARES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl FacetIds {
    fn cell(normal: &[f32; 3]) -> [i64; 3] {
        // `as` saturates, and a non-finite normal is refused by the mesh check before upload.
        normal.map(|c| (c / FACET_TOLERANCE).floor() as i64)
    }

    fn id(&mut self, normal: &[f32; 3]) -> u32 {
        let home = Self::cell(normal);
        let mut best: Option<u32> = None;
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let key = [home[0] + dx, home[1] + dy, home[2] + dz];
                    let Some(&f) = self.cells.get(&key) else {
                        continue;
                    };
                    #[cfg(test)]
                    FACET_COMPARES.with(|n| n.set(n.get() + 1));
                    let n = self.normals[f as usize];
                    let same = (0..3).all(|c| (n[c] - normal[c]).abs() < FACET_TOLERANCE);
                    if same && best.is_none_or(|b| f < b) {
                        best = Some(f);
                    }
                }
            }
        }
        if let Some(f) = best {
            return f;
        }
        let f = self.normals.len() as u32;
        self.normals.push(*normal);
        self.cells.insert(home, f);
        f
    }
}

/// A package mesh as the renderer's vertices. A stone's facets get their ids from its flat normals: each new
/// normal starts a facet.
pub fn viewer_mesh(mesh: &MeshData) -> ViewerMeshData {
    let mut facets = FacetIds::default();
    let faceted = mesh.material_slot.starts_with("gem:");
    let vertices = mesh
        .positions
        .iter()
        .zip(&mesh.normals)
        .enumerate()
        .map(|(i, (position, normal))| {
            let facet = if faceted { facets.id(normal) as f32 } else { 0.0 };
            ViewerVertex {
                position: *position,
                normal: *normal,
                uv: mesh
                    .uvs
                    .as_ref()
                    .and_then(|u| u.get(i))
                    .copied()
                    .unwrap_or([0.0, 0.0]),
                facet,
            }
        })
        .collect();
    ViewerMeshData {
        vertices,
        indices: mesh.indices.clone(),
    }
}

/// A renderer camera from a view pose.
pub fn viewer_camera(pose: &CameraPose) -> ViewerCamera {
    ViewerCamera {
        eye: pose.eye,
        target: pose.look_at,
        up: pose.up,
        fov_deg: pose.fov_short_deg,
        near_mm: pose.near,
        far_mm: pose.far,
    }
}

struct LoadedDraw {
    handle: DrawHandle,
    mesh: String,
    zone: String,
}

/// The renderer plus what it holds for the open product.
pub struct ViewerScene<T: FrameTarget> {
    renderer: ViewerRenderer,
    target: T,
    serial: Option<u64>,
    meshes: BTreeMap<String, MeshHandle>,
    materials: BTreeMap<String, (MaterialHandle, String)>,
    /// Anodize lookups of the open product by their entries (bit patterns), not by material key: two
    /// products may use one material id with different tables.
    luts: BTreeMap<Vec<[u32; 3]>, LutHandle>,
    draws: Vec<LoadedDraw>,
    looks: bool,
}

impl<T: FrameTarget> ViewerScene<T> {
    /// A scene on a device. All pipelines are built here, so nothing later can hitch on one.
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: ViewerConfig,
        env: &BakedEnv,
        target: T,
    ) -> Result<Self, WebError> {
        let looks = ShaderFamily::ALL
            .iter()
            .all(|f| config.hooks.hook(*f) == regia_looks::viewer::hook_source(*f));
        let mut renderer = ViewerRenderer::new(device, queue, config).map_err(render_error)?;
        renderer.set_environment(env);
        renderer.set_style(&ViewerStyle {
            key_lights: regia_looks::viewer::key_lights(&Studio::default()),
            ..*renderer.style()
        });
        Ok(ViewerScene {
            renderer,
            target,
            serial: None,
            meshes: BTreeMap::new(),
            materials: BTreeMap::new(),
            luts: BTreeMap::new(),
            draws: Vec::new(),
            looks,
        })
    }

    /// The renderer, for its counters.
    pub fn renderer(&self) -> &ViewerRenderer {
        &self.renderer
    }

    /// The target.
    pub fn target(&self) -> &T {
        &self.target
    }

    fn unload(&mut self) -> Result<(), WebError> {
        for draw in self.draws.drain(..) {
            self.renderer.remove_draw(draw.handle).map_err(render_error)?;
        }
        for (_, mesh) in std::mem::take(&mut self.meshes) {
            self.renderer.remove_mesh(mesh).map_err(render_error)?;
        }
        for (_, (material, _)) in std::mem::take(&mut self.materials) {
            self.renderer.remove_material(material).map_err(render_error)?;
        }
        // No material names a lookup row now: free the rows, so a page that opens product after
        // product never runs out of them, and the next product's tables are its own.
        self.luts.clear();
        self.renderer.clear_luts();
        Ok(())
    }

    fn sync_materials(&mut self, plan: &crate::seams::ScenePlan) -> Result<(), WebError> {
        for (zone, planned) in &plan.materials {
            if self
                .materials
                .get(zone)
                .is_some_and(|(_, key)| *key == planned.key)
            {
                continue;
            }
            let lut = match planned.look.anodize_lut() {
                Some(entries) => {
                    let bits: Vec<[u32; 3]> = entries.iter().map(|e| e.map(f32::to_bits)).collect();
                    let handle = match self.luts.get(&bits) {
                        Some(h) => *h,
                        None => {
                            let h = self.renderer.add_lut(entries).map_err(render_error)?;
                            self.luts.insert(bits, h);
                            h
                        }
                    };
                    Some(handle)
                }
                None => None,
            };
            let material = viewer_material(planned, lut);
            match self.materials.get_mut(zone) {
                Some((handle, key)) => {
                    self.renderer
                        .set_material(*handle, &material)
                        .map_err(render_error)?;
                    key.clone_from(&planned.key);
                }
                None => {
                    let handle = self.renderer.create_material(&material);
                    self.materials.insert(zone.clone(), (handle, planned.key.clone()));
                }
            }
        }
        Ok(())
    }

    fn sync_draws(&mut self, plan: &crate::seams::ScenePlan) -> Result<(), WebError> {
        for (i, wanted) in plan.draws.iter().enumerate() {
            let mesh = *self
                .meshes
                .get(&wanted.mesh)
                .ok_or_else(|| WebError::new("package_invalid", format!("no mesh {}", wanted.mesh)))?;
            let material = self.materials.get(&wanted.zone).map(|(h, _)| *h).ok_or_else(|| {
                WebError::new("package_invalid", format!("no material for {}", wanted.zone))
            })?;
            if let Some(loaded) = self.draws.get_mut(i) {
                if loaded.mesh != wanted.mesh {
                    self.renderer
                        .set_draw_mesh(loaded.handle, mesh)
                        .map_err(render_error)?;
                    loaded.mesh.clone_from(&wanted.mesh);
                }
                if loaded.zone != wanted.zone {
                    self.renderer
                        .set_draw_material(loaded.handle, material)
                        .map_err(render_error)?;
                    loaded.zone.clone_from(&wanted.zone);
                }
                self.renderer
                    .set_draw_transforms(loaded.handle, &wanted.transforms)
                    .map_err(render_error)?;
            } else {
                let handle = self
                    .renderer
                    .add_draw(mesh, material, &wanted.transforms)
                    .map_err(render_error)?;
                self.draws.push(LoadedDraw {
                    handle,
                    mesh: wanted.mesh.clone(),
                    zone: wanted.zone.clone(),
                });
            }
        }
        for extra in self.draws.split_off(plan.draws.len()) {
            self.renderer.remove_draw(extra.handle).map_err(render_error)?;
        }
        let shadow = self
            .renderer
            .bounds()
            .map(|b| ContactShadow::under(&b, SHADOW_OPACITY));
        self.renderer.set_style(&ViewerStyle {
            contact_shadow: shadow,
            ..*self.renderer.style()
        });
        Ok(())
    }
}

impl<T: FrameTarget> SceneBackend for ViewerScene<T> {
    fn sync_product(&mut self, product: &dyn ProductModel, geometry: bool) -> Result<(), WebError> {
        let _span = tracing::info_span!("web_scene_sync").entered();
        let fresh = self.serial != Some(product.serial());
        // The plan and every upload come first: a product that fails either leaves the one shown intact (the
        // player keeps showing it), instead of a scene emptied for a product that never arrived.
        let plan = product.plan()?;
        if fresh {
            let mut uploaded: BTreeMap<String, MeshHandle> = BTreeMap::new();
            for mesh in product.meshes() {
                // A repeated name: the first mesh wins, as the product's own lookup finds it (a second upload
                // under the name would also orphan the first handle).
                if uploaded.contains_key(&mesh.name) {
                    continue;
                }
                match self.renderer.upload_mesh(&viewer_mesh(mesh)) {
                    Ok(handle) => {
                        uploaded.insert(mesh.name.clone(), handle);
                    }
                    Err(e) => {
                        for (_, handle) in uploaded {
                            // Freshly made handles; a failure here leaves nothing worse than the error below.
                            let _ = self.renderer.remove_mesh(handle);
                        }
                        return Err(render_error(e));
                    }
                }
            }
            let unloaded = self.unload();
            self.meshes = uploaded;
            self.serial = Some(product.serial());
            unloaded?;
        }
        self.sync_materials(&plan)?;
        if fresh || geometry {
            self.sync_draws(&plan)?;
        }
        self.renderer.request_redraw();
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.target.resize(width, height);
    }

    fn draw(&mut self, pose: &CameraPose) -> Result<DrawOutcome, WebError> {
        let _span = tracing::info_span!("web_scene_draw").entered();
        self.renderer
            .set_camera(&viewer_camera(pose))
            .map_err(render_error)?;
        let renderer = &mut self.renderer;
        self.target
            .with_frame(&mut |texture| renderer.render(texture).map_err(render_error))
    }

    fn pipelines(&self) -> usize {
        // Created so far, not the size of the warm set: a creation after open must show
        // (`warm_set_miss`, spec 42 § 7).
        usize::try_from(self.renderer.stats().pipelines_created).unwrap_or(usize::MAX)
    }

    fn draws_looks(&self) -> bool {
        self.looks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbering before the grid: the first facet so far within the tolerance.
    fn linear_ids(normals: &[[f32; 3]]) -> Vec<u32> {
        let mut facets: Vec<[f32; 3]> = Vec::new();
        normals
            .iter()
            .map(|normal| {
                let same = |n: &[f32; 3]| (0..3).all(|c| (n[c] - normal[c]).abs() < FACET_TOLERANCE);
                match facets.iter().position(same) {
                    Some(f) => f as u32,
                    None => {
                        facets.push(*normal);
                        (facets.len() - 1) as u32
                    }
                }
            })
            .collect()
    }

    /// A fixed pseudo-random sequence (xorshift), so the test is the same on every run.
    fn normals(count: usize, spread: f32) -> Vec<[f32; 3]> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32
        };
        (0..count)
            .map(|_| [next() * spread, next() * spread, next() * spread])
            .collect()
    }

    fn gem_mesh(normals: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            name: "cut:test".into(),
            material_slot: "gem:s1".into(),
            positions: vec![[0.0; 3]; normals.len()],
            normals,
            uvs: None,
            indices: Vec::new(),
        }
    }

    #[test]
    fn the_grid_numbers_facets_exactly_as_the_linear_search_did() {
        // Close normals (a spread of a few tolerances) so many fall near cell edges and match across cells.
        for spread in [0.004, 0.02, 1.0] {
            let n = normals(3000, spread);
            let mut ids = FacetIds::default();
            let got: Vec<u32> = n.iter().map(|v| ids.id(v)).collect();
            assert_eq!(got, linear_ids(&n), "spread {spread}");
        }
        // Repeated flat normals share one id, in first-seen order.
        let flat = [
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 1.0005],
        ];
        let mesh = viewer_mesh(&gem_mesh(flat.to_vec()));
        let facet: Vec<f32> = mesh.vertices.iter().map(|v| v.facet).collect();
        assert_eq!(facet, [0.0, 1.0, 0.0, 0.0]);
    }

    /// Audit fix (sprint-fix-audit-jw, finding 5): a smooth stone mesh (every normal its own facet) costs
    /// a few comparisons per vertex, not one per facet so far.
    #[test]
    fn a_smooth_stone_mesh_numbers_its_facets_in_linear_work() {
        let count = 20_000;
        let mesh = gem_mesh(normals(count, 1.0));
        FACET_COMPARES.with(|c| c.set(0));
        let out = viewer_mesh(&mesh);
        let compares = FACET_COMPARES.with(std::cell::Cell::get);
        assert_eq!(out.vertices.len(), count);
        assert!(
            compares <= 27 * count as u64,
            "{compares} comparisons for {count} vertices"
        );
    }
}
