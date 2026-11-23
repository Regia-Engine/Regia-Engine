// SPDX-License-Identifier: MPL-2.0
//! Procedural plants (`docs/specs/68-procedural-plants.md`): the plant-domain data kinds, the export
//! job the `plant.export` node records, and plant statistics.
//!
//! The data types are always compiled (they are [`crate::Payload`] variants); the `plant.*` node
//! types that grow, shape, mesh and dress plants are registered only with the `pipeline` feature
//! (spec 68 § 2): games ship the exported meshes, never the growth simulation.

pub mod envelope;

use std::collections::BTreeMap;
use std::sync::Arc;

/// The state of a bud (spec 68 § 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BudState {
    /// Grows when it receives enough resource.
    #[default]
    Active,
    /// Waiting (too little resource, outside the envelope); dies after `shed_after_steps` steps.
    Dormant,
    /// Gone: an interior node whose axis continued, or a bud that stayed dormant too long.
    Dead,
    /// A flower: stops growing; the node carries `bloom = true`.
    Bloom,
}

impl BudState {
    /// The agent and report name (`active`, `dormant`, `dead`, `bloom`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Dormant => "dormant",
            Self::Dead => "dead",
            Self::Bloom => "bloom",
        }
    }
}

/// One node of a plant skeleton (spec 68 § 2). The node's `bud` is its terminal bud: `Dead` once the
/// axis continued past it. Lateral buds that have not grown yet are [`PlantBud`]s on the plant.
#[derive(Clone, Debug, PartialEq)]
pub struct PlantNode {
    /// Position, meters (plant space = world space of the seed point).
    pub position: [f64; 3],
    /// Parent node index (`None` for the root).
    pub parent: Option<u32>,
    /// Branch radius at this node, meters.
    pub radius: f64,
    /// Branch order: 0 trunk, 1 first branches, ...
    pub order: u32,
    /// The growth step that created the node (0 for the seed root).
    pub age: u32,
    /// Light exposure 0-1 at the node's voxel in the last growth step.
    pub light: f64,
    /// Unit growth direction of the segment ending at this node (the root: the plant's up).
    pub dir: [f64; 3],
    /// The terminal bud.
    pub bud: BudState,
    /// Phyllotaxis yaw of this node's lateral buds around its axis, degrees.
    pub yaw_deg: f64,
    /// The node ended its axis in a flower.
    pub bloom: bool,
    /// Consecutive dormant steps of the terminal bud.
    pub dormant_steps: u32,
    /// Consecutive steps the branch starting at this node stayed below `shed_below_light`.
    pub dim_steps: u32,
}

impl PlantNode {
    /// A root node at `position` growing along `dir`.
    pub fn root(position: [f64; 3], dir: [f64; 3], radius: f64, yaw_deg: f64) -> Self {
        Self {
            position,
            parent: None,
            radius,
            order: 0,
            age: 0,
            light: 1.0,
            dir,
            bud: BudState::Active,
            yaw_deg,
            bloom: false,
            dormant_steps: 0,
            dim_steps: 0,
        }
    }
}

/// A lateral bud waiting on a node (spec 68 § 4 step 5).
#[derive(Clone, Debug, PartialEq)]
pub struct PlantBud {
    /// The node the bud sits on.
    pub node: u32,
    /// Unit direction the branch would start in.
    pub dir: [f64; 3],
    /// Order of the branch it starts.
    pub order: u32,
    /// State (`Active` or `Dormant`; dead buds are removed).
    pub state: BudState,
    /// Consecutive dormant steps.
    pub dormant_steps: u32,
    /// Yaw around the parent axis, degrees (phyllotaxis).
    pub yaw_deg: f64,
}

/// One plant: a tree of nodes grown from one seed point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plant {
    /// The plant seed (the seed point's seed).
    pub seed: u32,
    /// Nodes; a parent always has a lower index than its children.
    pub nodes: Vec<PlantNode>,
    /// Lateral buds not grown yet.
    pub buds: Vec<PlantBud>,
    /// Custom per-node float attributes (each as long as `nodes`).
    pub attrs: BTreeMap<String, Vec<f64>>,
    /// Branches removed by shedding so far.
    pub shed_branches: u32,
}

impl Plant {
    /// Children of every node, in index order.
    pub fn children(&self) -> Vec<Vec<u32>> {
        let mut c = vec![Vec::new(); self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            if let Some(p) = n.parent
                && let Some(slot) = c.get_mut(p as usize)
            {
                slot.push(i as u32);
            }
        }
        c
    }

    /// The plant as branch axes: an axis starts at the root or at a node that is not its parent's
    /// main continuation (the first child of the parent's order), and follows main continuations.
    pub fn axes(&self) -> Vec<PlantAxis> {
        let children = self.children();
        let main: Vec<Option<usize>> = (0..self.nodes.len())
            .map(|i| {
                children[i]
                    .iter()
                    .map(|&c| c as usize)
                    .find(|&c| self.nodes[c].order == self.nodes[i].order)
            })
            .collect();
        let mut out = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let starts = match node.parent {
                None => true,
                Some(par) => main.get(par as usize).copied().flatten() != Some(i),
            };
            if !starts {
                continue;
            }
            let mut nodes = vec![i];
            let mut cur = i;
            while let Some(m) = main[cur] {
                nodes.push(m);
                cur = m;
            }
            out.push(PlantAxis {
                attach: self.nodes[i].parent.map(|x| x as usize),
                nodes,
            });
        }
        out
    }

    /// Keeps the nodes with `keep[i]` (a kept node's parent must be kept), remapping parents, buds and
    /// attributes. Buds on removed nodes are dropped.
    pub fn retain_nodes(&mut self, keep: &[bool]) {
        let mut map = vec![u32::MAX; self.nodes.len()];
        let mut next = 0u32;
        for (i, k) in keep.iter().enumerate().take(self.nodes.len()) {
            if *k {
                map[i] = next;
                next += 1;
            }
        }
        let old = std::mem::take(&mut self.nodes);
        for (i, mut n) in old.into_iter().enumerate() {
            if !keep.get(i).copied().unwrap_or(false) {
                continue;
            }
            n.parent = n.parent.and_then(|p| {
                let m = map.get(p as usize).copied().unwrap_or(u32::MAX);
                (m != u32::MAX).then_some(m)
            });
            self.nodes.push(n);
        }
        self.buds.retain_mut(|b| {
            let m = map.get(b.node as usize).copied().unwrap_or(u32::MAX);
            b.node = m;
            m != u32::MAX
        });
        for col in self.attrs.values_mut() {
            let old = std::mem::take(col);
            *col = old
                .into_iter()
                .enumerate()
                .filter(|(i, _)| keep.get(*i).copied().unwrap_or(false))
                .map(|(_, v)| v)
                .collect();
        }
    }

    /// Height above the root, max over nodes (0 for an empty plant).
    pub fn height(&self) -> f64 {
        let Some(root) = self.nodes.first() else {
            return 0.0;
        };
        self.nodes
            .iter()
            .map(|n| n.position[1] - root.position[1])
            .fold(0.0, f64::max)
    }

    /// Crown width: the larger XZ extent of the nodes.
    pub fn crown_width(&self) -> f64 {
        if self.nodes.is_empty() {
            return 0.0;
        }
        let mut lo = [f64::INFINITY; 2];
        let mut hi = [f64::NEG_INFINITY; 2];
        for n in &self.nodes {
            for (k, a) in [0usize, 2].into_iter().enumerate() {
                lo[k] = lo[k].min(n.position[a]);
                hi[k] = hi[k].max(n.position[a]);
            }
        }
        (hi[0] - lo[0]).max(hi[1] - lo[1])
    }

    /// The highest branch order.
    pub fn max_order(&self) -> u32 {
        self.nodes.iter().map(|n| n.order).max().unwrap_or(0)
    }
}

/// One branch axis of a plant ([`Plant::axes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlantAxis {
    /// The node the axis grows from (its first node's parent), if any.
    pub attach: Option<usize>,
    /// Node indices along the axis, base first.
    pub nodes: Vec<usize>,
}

/// Plant skeletons: independent plants, one per seed point (spec 68 § 2). Named so it never
/// collides with the animation skeleton.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlantSkeleton {
    /// The plants in item order.
    pub plants: Vec<Plant>,
}

impl PlantSkeleton {
    /// Total nodes over all plants.
    pub fn node_count(&self) -> usize {
        self.plants.iter().map(|p| p.nodes.len()).sum()
    }

    /// World AABB `(min, max)` of the node positions grown by their radii (zero when empty).
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for n in self.plants.iter().flat_map(|p| &p.nodes) {
            for k in 0..3 {
                lo[k] = lo[k].min(n.position[k] - n.radius);
                hi[k] = hi[k].max(n.position[k] + n.radius);
            }
        }
        if lo[0] > hi[0] {
            return ([0.0; 3], [0.0; 3]);
        }
        (lo.map(|v| v as f32), hi.map(|v| v as f32))
    }

    /// Feeds the canonical encoding into a hasher. Every field growth or meshing reads is hashed
    /// (`audit-g-procgen2-determinism` finding 1: a skeleton that differs only in `yaw_deg`,
    /// `light`, dormancy, bud order or `shed_branches` must not share a cache key with the
    /// skeleton a cached `plant.grow` / `plant.leaves` would return), and each attribute key and
    /// column carries a length prefix so concatenated encodings stay unambiguous.
    pub fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"plant_skeleton");
        h.update(&(self.plants.len() as u64).to_le_bytes());
        for p in &self.plants {
            h.update(&p.seed.to_le_bytes());
            h.update(&p.shed_branches.to_le_bytes());
            h.update(&(p.nodes.len() as u64).to_le_bytes());
            for n in &p.nodes {
                for v in n.position.iter().chain(&n.dir) {
                    h.update(&v.to_bits().to_le_bytes());
                }
                h.update(&n.parent.unwrap_or(u32::MAX).to_le_bytes());
                h.update(&n.radius.to_bits().to_le_bytes());
                h.update(&n.order.to_le_bytes());
                h.update(&n.age.to_le_bytes());
                h.update(&n.light.to_bits().to_le_bytes());
                h.update(&n.yaw_deg.to_bits().to_le_bytes());
                h.update(&[n.bud as u8, u8::from(n.bloom)]);
                h.update(&n.dormant_steps.to_le_bytes());
                h.update(&n.dim_steps.to_le_bytes());
            }
            h.update(&(p.buds.len() as u64).to_le_bytes());
            for b in &p.buds {
                h.update(&b.node.to_le_bytes());
                for v in b.dir {
                    h.update(&v.to_bits().to_le_bytes());
                }
                h.update(&b.order.to_le_bytes());
                h.update(&[b.state as u8]);
                h.update(&b.dormant_steps.to_le_bytes());
                h.update(&b.yaw_deg.to_bits().to_le_bytes());
            }
            h.update(&(p.attrs.len() as u64).to_le_bytes());
            for (k, col) in &p.attrs {
                h.update(&(k.len() as u64).to_le_bytes());
                h.update(k.as_bytes());
                h.update(&(col.len() as u64).to_le_bytes());
                for v in col {
                    h.update(&v.to_bits().to_le_bytes());
                }
            }
        }
    }

    /// The canonical hash of the skeleton alone.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        self.hash_into(&mut h);
        *h.finalize().as_bytes()
    }
}

/// What a material slot of a [`PlantMesh`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotKind {
    /// Branch tubes.
    Bark,
    /// Leaves, cards or blossoms (`plant.leaves`).
    Leaf,
}

/// One material slot: an indexed triangle list with the wind attributes (spec 68 § 7).
#[derive(Clone, Debug, PartialEq)]
pub struct PlantMeshSlot {
    /// Material path or name.
    pub material: String,
    /// Bark or leaves.
    pub kind: SlotKind,
    /// Object-space positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals.
    pub normals: Vec<[f32; 3]>,
    /// Texture coordinates (bark: U around, V along / `bark_tile_m`).
    pub uvs: Vec<[f32; 2]>,
    /// `_WIND`: x branch level, y phase, z stiffness, w leaf flutter weight.
    pub wind: Vec<[f32; 4]>,
    /// `_WIND_PIVOT`: object-space base of the vertex's branch.
    pub wind_pivot: Vec<[f32; 3]>,
    /// Triangle-list indices (counter-clockwise front faces).
    pub indices: Vec<u32>,
}

impl PlantMeshSlot {
    /// An empty slot.
    pub fn new(material: &str, kind: SlotKind) -> Self {
        Self {
            material: material.to_string(),
            kind,
            positions: Vec::new(),
            normals: Vec::new(),
            uvs: Vec::new(),
            wind: Vec::new(),
            wind_pivot: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// Triangles in the slot.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

/// A meshed plant (spec 68 § 2): material slots with wind data, per-plant bounds, the skeleton it was
/// made from (so `plant.leaves` can dress a meshed plant) and the leaf sites placed so far
/// (`keep_clear_m`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlantMesh {
    /// Slots in creation order (bark slots first).
    pub slots: Vec<PlantMeshSlot>,
    /// Per-plant bounds `(min, max)`, in skeleton plant order.
    pub plant_bounds: Vec<([f32; 3], [f32; 3])>,
    /// The skeleton the mesh was made from.
    pub skeleton: Arc<PlantSkeleton>,
    /// Centers of every leaf placed by `plant.leaves` passes so far.
    pub leaf_sites: Vec<[f32; 3]>,
}

impl PlantMesh {
    /// Total triangles.
    pub fn triangles(&self) -> usize {
        self.slots.iter().map(PlantMeshSlot::triangles).sum()
    }

    /// The slot for a material and kind, created on first use.
    pub fn slot_mut(&mut self, material: &str, kind: SlotKind) -> &mut PlantMeshSlot {
        let pos = match self
            .slots
            .iter()
            .position(|s| s.material == material && s.kind == kind)
        {
            Some(p) => p,
            None => {
                self.slots.push(PlantMeshSlot::new(material, kind));
                self.slots.len() - 1
            }
        };
        &mut self.slots[pos]
    }

    /// AABB over every slot's positions (zero when empty).
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for p in self.slots.iter().flat_map(|s| &s.positions) {
            lo = std::array::from_fn(|k| lo[k].min(p[k]));
            hi = std::array::from_fn(|k| hi[k].max(p[k]));
        }
        if lo[0] > hi[0] {
            return ([0.0; 3], [0.0; 3]);
        }
        (lo, hi)
    }

    /// Feeds the canonical encoding into a hasher.
    pub fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"plant_mesh");
        for s in &self.slots {
            h.update(s.material.as_bytes());
            h.update(&[s.kind as u8]);
            let f = |h: &mut blake3::Hasher, v: &[f32]| {
                for x in v {
                    h.update(&x.to_bits().to_le_bytes());
                }
            };
            f(h, s.positions.as_flattened());
            f(h, s.normals.as_flattened());
            f(h, s.uvs.as_flattened());
            f(h, s.wind.as_flattened());
            f(h, s.wind_pivot.as_flattened());
            for i in &s.indices {
                h.update(&i.to_le_bytes());
            }
        }
        for p in &self.leaf_sites {
            for x in p {
                h.update(&x.to_bits().to_le_bytes());
            }
        }
        self.skeleton.hash_into(h);
    }

    /// The canonical hash of the mesh.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        self.hash_into(&mut h);
        *h.finalize().as_bytes()
    }
}

/// Collision written by an export (spec 68 § 6.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportCollision {
    /// No collision nodes.
    #[default]
    None,
    /// One capsule per trunk and order-1 branch thicker than 5 cm.
    Capsules,
}

/// Settings of a `plant.export` node (spec 68 § 6.3).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportSettings {
    /// Triangle ratio per LOD (LOD 0 first), e.g. `[1.0, 0.5, 0.2, 0.05]`.
    pub lods: Vec<f32>,
    /// Add an impostor billboard at the last LOD.
    pub billboard: bool,
    /// `lod_screen_size` extras per LOD (empty = the convention defaults).
    pub lod_screen_size: Vec<f32>,
    /// The LOD shadows use (`shadow_lod` extra).
    pub shadow_lod: u32,
    /// Collision.
    pub collision: ExportCollision,
    /// The asset stem (`<name>_LOD<n>` nodes).
    pub name: String,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            lods: vec![1.0, 0.5, 0.2, 0.05],
            billboard: true,
            lod_screen_size: Vec::new(),
            shadow_lod: 1,
            collision: ExportCollision::None,
            name: "plant".to_string(),
        }
    }
}

/// A plant export recorded by `plant.export` (spec 68 § 6.3). `regia_procgen` never writes it: the
/// host (CLI, editor) passes it to `regia_asset_pipeline::plant_export::write_job`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportJob {
    /// The id of the `plant.export` node that recorded it.
    pub node: u32,
    /// The mesh to write.
    pub mesh: Arc<PlantMesh>,
    /// Export settings.
    pub settings: ExportSettings,
    /// Output path (`.glb`), project-relative or absolute.
    pub out: String,
}

/// What a host's writer reports for one export job.
#[derive(Clone, Debug, PartialEq)]
pub struct WrittenPlant {
    /// Triangles per LOD (LOD 0 first).
    pub triangles_per_lod: Vec<usize>,
    /// Object-space bounds of LOD 0.
    pub bounds: ([f32; 3], [f32; 3]),
}

/// A failed plant build: a stable code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlantBuildError {
    /// `plant_no_export`, a host writer's code, or the run's own code (`limit_exceeded`, ...).
    pub code: String,
    /// What failed.
    pub message: String,
}

impl std::fmt::Display for PlantBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The result of [`build_plant_variants`].
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltPlants {
    /// The first export's name: the stem of the variant files and of `<stem>.plants.ron`.
    pub stem: String,
    /// One entry per file written.
    pub index: PlantsIndex,
}

/// The file stem of an export: its `name` (`plant` when empty). The name becomes part of every file
/// written, so it must be a plain file name: a name with a path separator (`/`, `\`), a drive colon,
/// a control character, or made only of dots is refused with `plant_bad_name`, so a graph setting
/// can never write outside the host's output directory. So is a name Windows cannot use as a file
/// name (a `<`, `>`, `"`, `|`, `?` or `*`, or a device name such as `CON` or `com1`), so a graph
/// exports the same files on every platform.
///
/// # Errors
///
/// `plant_bad_name` for such a name.
pub fn export_stem(settings: &ExportSettings) -> Result<&str, PlantBuildError> {
    let name = settings.name.as_str();
    if name.is_empty() {
        return Ok("plant");
    }
    // `<stem>.plants.ron` puts the name before the first dot, where Windows reads device names;
    // Windows reserves CON/PRN/AUX/NUL/COMn/LPTn with any extension, so `NUL.v2.plants.ron` is
    // the NUL device and the check reads the part of the name before the first dot.
    let device = {
        let stem = name.split('.').next().unwrap_or(name);
        let upper = stem.to_ascii_uppercase();
        let digit = |rest: &str| rest.len() == 1 && rest.as_bytes()[0].is_ascii_digit() && rest != "0";
        matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || upper.strip_prefix("COM").is_some_and(digit)
            || upper.strip_prefix("LPT").is_some_and(digit)
    };
    let bad = name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())
        || name.chars().all(|c| c == '.')
        || device;
    if bad {
        return Err(PlantBuildError {
            code: "plant_bad_name".to_string(),
            message: format!(
                "plant.export `name` {name:?} must be a plain file name: no path, no `<>\"|?*`, no device name such as CON"
            ),
        });
    }
    Ok(name)
}

/// The file of an export named `name` (of `jobs` in the run) in variant `k`.
fn variant_file(name: &str, k: u32, j: usize, jobs: usize) -> String {
    if jobs == 1 {
        format!("{name}_{k}.glb")
    } else {
        format!("{name}_{k}_{j}.glb")
    }
}

/// Runs a compiled Plant graph once per variant (seeds `seed + k`) and hands each export job to
/// `write` with its file name (`<name>_<k>.glb`, spec 68 § 8). The CLI and `procgen.plant_build` share
/// it, so both produce the same files.
///
/// # Errors
///
/// The run's failure, `plant_no_export` when the graph has no `plant.export`, `plant_bad_name` (see
/// [`export_stem`]), or `write`'s error.
pub fn build_plant_variants(
    compiled: &crate::graph::CompiledGraph,
    graph_path: &str,
    make_inputs: &dyn Fn(u32) -> crate::exec::RunInputs,
    (seed, variants): (u32, u32),
    cancel: &crate::exec::CancelToken,
    write: &mut dyn FnMut(&ExportJob, &str) -> Result<WrittenPlant, PlantBuildError>,
) -> Result<BuiltPlants, PlantBuildError> {
    let mut index = PlantsIndex {
        graph: graph_path.to_string(),
        graph_hash: compiled.graph_hash.iter().map(|b| format!("{b:02x}")).collect(),
        variants: Vec::new(),
    };
    let mut stem = String::new();
    for k in 0..variants.max(1) {
        let variant_seed = seed.wrapping_add(k);
        let result = crate::exec::Evaluator::new()
            .run(compiled, &make_inputs(variant_seed), cancel)
            .map_err(|e| PlantBuildError {
                code: e.code().to_string(),
                message: format!("variant {k} (seed {variant_seed}): {e}"),
            })?;
        let jobs = result.exports();
        if jobs.is_empty() {
            return Err(PlantBuildError {
                code: "plant_no_export".to_string(),
                message: format!("{graph_path}: the graph has no plant.export node"),
            });
        }
        for (j, job) in jobs.iter().enumerate() {
            let name = export_stem(&job.settings)?;
            if stem.is_empty() {
                stem = name.to_string();
            }
            let file = variant_file(name, k, j, jobs.len());
            let written = write(job, &file)?;
            index.variants.push(PlantsIndexEntry {
                file,
                seed: variant_seed,
                triangles_per_lod: written.triangles_per_lod.iter().map(|t| *t as u64).collect(),
                bounds: written.bounds,
            });
        }
    }
    Ok(BuiltPlants { stem, index })
}

/// The assets the graph's `plant.envelope_mesh` nodes name: what a host loads into its snapshot.
pub fn envelope_assets(compiled: &crate::graph::CompiledGraph) -> Vec<String> {
    let mut assets: Vec<String> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "plant.envelope_mesh")
        .map(|n| n.settings.text("asset").to_string())
        .filter(|a| !a.is_empty())
        .collect();
    assets.sort();
    assets.dedup();
    assets
}

/// Per-plant statistics (`procgen.plant_stats`, spec 68 § 8).
#[derive(Clone, Debug, PartialEq)]
pub struct PlantStats {
    /// The plant seed.
    pub seed: u32,
    /// Skeleton nodes.
    pub nodes: usize,
    /// Highest branch order.
    pub max_order: u32,
    /// Height above the root, meters.
    pub height_m: f64,
    /// Larger XZ extent, meters.
    pub crown_width_m: f64,
    /// Branches removed by shedding.
    pub branches_shed: u32,
    /// Triangles of the plant's mesh. 0 when no mesh was passed, or when the skeleton has more than
    /// one plant (the mesh keeps no per-plant triangle range).
    pub triangles: usize,
}

/// Statistics of every plant in a skeleton; `triangles` are the whole mesh's when `mesh` is given
/// and there is one plant, else 0 per plant (meshes do not keep per-plant triangle ranges).
pub fn plant_stats(skeleton: &PlantSkeleton, mesh: Option<&PlantMesh>) -> Vec<PlantStats> {
    let single = skeleton.plants.len() == 1;
    skeleton
        .plants
        .iter()
        .map(|p| PlantStats {
            seed: p.seed,
            nodes: p.nodes.len(),
            max_order: p.max_order(),
            height_m: p.height(),
            crown_width_m: p.crown_width(),
            branches_shed: p.shed_branches,
            triangles: if single {
                mesh.map_or(0, PlantMesh::triangles)
            } else {
                0
            },
        })
        .collect()
}

/// `format_version` of the `<name>.plants.ron` variant index (`plants-index`, spec 68 § 8).
pub const PLANTS_INDEX_FORMAT_VERSION: u32 = 1;

/// The shape fingerprint of `plants-index` (field list and value types), for `check-formats`.
pub fn plants_index_schema_fingerprint() -> String {
    "PlantsIndex(format_version:u32,graph:str,graph_hash:hex32,variants[(file:str,seed:u32,\
     triangles_per_lod:[u64],bounds:((f32,f32,f32),(f32,f32,f32)))])"
        .to_string()
}

/// `plants-index` migration sources (none: new at version 1).
pub fn plants_index_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// One exported variant in a [`PlantsIndex`].
#[derive(Clone, Debug, PartialEq)]
pub struct PlantsIndexEntry {
    /// The `.glb` file name, relative to the index.
    pub file: String,
    /// The generator seed of the variant.
    pub seed: u32,
    /// Triangles per LOD, LOD 0 first.
    pub triangles_per_lod: Vec<u64>,
    /// Object-space bounds `(min, max)`.
    pub bounds: ([f32; 3], [f32; 3]),
}

/// The `<name>.plants.ron` index `regia procgen plant` writes next to the variants (format
/// `plants-index`, `format_version: 1`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlantsIndex {
    /// The plant graph path.
    pub graph: String,
    /// The graph hash (hex).
    pub graph_hash: String,
    /// Variants in seed order.
    pub variants: Vec<PlantsIndexEntry>,
}

impl PlantsIndex {
    /// The RON text (fixed layout and float formatting, so the same index gives the same bytes).
    pub fn to_ron(&self) -> String {
        use crate::value::SettingValue as V;
        // RON strings, not Rust's `{:?}`: a path holding a control character must load again (ADR-1917).
        let quoted = |text: &str| V::Str(text.to_owned()).write();
        let v3 = |v: [f32; 3]| format!("({:?}, {:?}, {:?})", v[0], v[1], v[2]);
        let mut out = format!(
            "PlantsIndex(\n    format_version: {PLANTS_INDEX_FORMAT_VERSION},\n    graph: {},\n    graph_hash: {},\n    variants: [\n",
            quoted(&self.graph),
            quoted(&self.graph_hash)
        );
        for e in &self.variants {
            let tris: Vec<String> = e.triangles_per_lod.iter().map(u64::to_string).collect();
            out.push_str(&format!(
                "        (file: {}, seed: {}, triangles_per_lod: [{}], bounds: ({}, {})),\n",
                quoted(&e.file),
                e.seed,
                tris.join(", "),
                v3(e.bounds.0),
                v3(e.bounds.1)
            ));
        }
        out.push_str("    ],\n)\n");
        out
    }

    /// Parses [`PlantsIndex::to_ron`] text.
    ///
    /// # Errors
    ///
    /// A message for text that is not a version-1 index.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        use crate::value::SettingValue as V;
        let v = V::parse(text).map_err(|e| e.to_string())?;
        let version = v.field("format_version").and_then(V::as_i64);
        if version != Some(i64::from(PLANTS_INDEX_FORMAT_VERSION)) {
            return Err(format!("plants-index format_version {version:?} is not 1"));
        }
        let text_of = |x: &V, k: &str| {
            x.field(k)
                .and_then(V::as_text)
                .map(str::to_string)
                .ok_or(format!("missing `{k}`"))
        };
        let mut out = Self {
            graph: text_of(&v, "graph")?,
            graph_hash: text_of(&v, "graph_hash")?,
            variants: Vec::new(),
        };
        for e in v.field("variants").and_then(V::as_items).unwrap_or(&[]) {
            let b = e.field("bounds").and_then(V::as_items).unwrap_or(&[]);
            let corner = |k: usize| -> [f32; 3] {
                b.get(k)
                    .and_then(V::as_vec3)
                    .map_or([0.0; 3], |c| c.map(|x| x as f32))
            };
            out.variants.push(PlantsIndexEntry {
                file: text_of(e, "file")?,
                seed: e
                    .field("seed")
                    .and_then(V::as_i64)
                    .and_then(|s| u32::try_from(s).ok())
                    .ok_or("missing `seed`")?,
                triangles_per_lod: e
                    .field("triangles_per_lod")
                    .and_then(V::as_items)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|t| t.as_i64().and_then(|t| u64::try_from(t).ok()))
                    .collect(),
                bounds: (corner(0), corner(1)),
            });
        }
        Ok(out)
    }
}

/// The shipped plant starter graphs (spec 68 § 8), by their `starter/...` path.
#[cfg(feature = "pipeline")]
pub const STARTERS: [(&str, &str); 3] = [
    (
        "starter/plant_broadleaf.pgraph.ron",
        include_str!("../../starter/plant_broadleaf.pgraph.ron"),
    ),
    (
        "starter/plant_conifer.pgraph.ron",
        include_str!("../../starter/plant_conifer.pgraph.ron"),
    ),
    (
        "starter/plant_bush.pgraph.ron",
        include_str!("../../starter/plant_bush.pgraph.ron"),
    ),
];

/// The text of a shipped starter graph by its `starter/...` path.
#[cfg(feature = "pipeline")]
pub fn starter(path: &str) -> Option<&'static str> {
    STARTERS.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names Windows cannot write are refused on every platform, so one graph exports the same files
    /// everywhere (corr-procgen); ordinary names that merely contain a device name still pass.
    #[test]
    fn export_names_windows_cannot_write_are_refused() {
        let stem = |name: &str| {
            export_stem(&ExportSettings {
                name: name.to_string(),
                ..ExportSettings::default()
            })
            .map(str::to_string)
            .map_err(|e| e.code)
        };
        for bad in [
            "a?b", "a*", "x<y", "x>y", "q\"t", "p|q", "CON", "con", "Nul", "aux", "PRN", "COM1", "lpt9",
            // Windows reserves the device names with any extension, so the check reads the part
            // before the first dot (`NUL.v2.plants.ron` is the NUL device).
            "nul.v2", "COM1.txt", "LPT3.foo",
        ] {
            assert_eq!(stem(bad), Err("plant_bad_name".to_string()), "{bad:?}");
        }
        for good in ["oak", "COM0", "COM10", "console", "aux_tree", "con-1", "LPT"] {
            assert_eq!(stem(good), Ok(good.to_string()), "{good:?}");
        }
        assert_eq!(stem(""), Ok("plant".to_string()));
    }

    fn node(parent: Option<u32>, y: f64) -> PlantNode {
        PlantNode {
            parent,
            ..PlantNode::root([0.0, y, 0.0], [0.0, 1.0, 0.0], 0.01, 0.0)
        }
    }

    /// The ADR-1917 fuzz finding: the index wrote strings with Rust's `{:?}`, so a NUL came out as `\\0`, which RON
    /// refuses, and the saved index no longer loaded.
    #[test]
    fn an_index_with_hostile_strings_loads_again() {
        let index = PlantsIndex {
            graph: "a\0b\"c\\d".into(),
            graph_hash: "\n".into(),
            variants: vec![PlantsIndexEntry {
                file: "\u{7f}\u{1F332}.glb".into(),
                seed: 1,
                triangles_per_lod: vec![3],
                bounds: ([0.0; 3], [1.0; 3]),
            }],
        };
        let text = index.to_ron();
        let again = PlantsIndex::from_ron(&text).expect("the written index loads");
        assert_eq!(again.to_ron(), text);
        assert_eq!(again.graph, index.graph);
        assert_eq!(again.variants[0].file, index.variants[0].file);
    }

    #[test]
    fn retain_remaps_parents_buds_and_attrs() {
        let mut p = Plant {
            nodes: vec![
                node(None, 0.0),
                node(Some(0), 1.0),
                node(Some(1), 2.0),
                node(Some(0), 0.5),
            ],
            buds: vec![
                PlantBud {
                    node: 2,
                    dir: [1.0, 0.0, 0.0],
                    order: 1,
                    state: BudState::Active,
                    dormant_steps: 0,
                    yaw_deg: 0.0,
                },
                PlantBud {
                    node: 3,
                    dir: [1.0, 0.0, 0.0],
                    order: 1,
                    state: BudState::Active,
                    dormant_steps: 0,
                    yaw_deg: 0.0,
                },
            ],
            ..Plant::default()
        };
        p.attrs.insert("a".into(), vec![0.0, 1.0, 2.0, 3.0]);
        p.retain_nodes(&[true, false, false, true]);
        assert_eq!(p.nodes.len(), 2);
        assert_eq!(p.nodes[1].parent, Some(0));
        assert_eq!(p.buds.len(), 1);
        assert_eq!(p.buds[0].node, 1);
        assert_eq!(p.attrs["a"], vec![0.0, 3.0]);
    }

    #[test]
    fn plants_index_round_trips() {
        let idx = PlantsIndex {
            graph: "starter/plant_conifer.pgraph.ron".into(),
            graph_hash: "ab12".into(),
            variants: vec![PlantsIndexEntry {
                file: "plant_conifer_0.glb".into(),
                seed: 4,
                triangles_per_lod: vec![900, 450, 180, 50],
                bounds: ([-1.5, 0.0, -1.25], [1.5, 6.5, 1.0]),
            }],
        };
        let text = idx.to_ron();
        assert_eq!(PlantsIndex::from_ron(&text), Ok(idx));
        assert!(PlantsIndex::from_ron(&text.replace("format_version: 1", "format_version: 2")).is_err());
    }

    #[test]
    fn stats_measure_height_and_width() {
        let mut p = Plant {
            nodes: vec![node(None, 0.0), node(Some(0), 3.0)],
            ..Plant::default()
        };
        p.nodes[1].position[0] = 2.0;
        let s = plant_stats(&PlantSkeleton { plants: vec![p] }, None);
        assert_eq!(s[0].nodes, 2);
        assert!((s[0].height_m - 3.0).abs() < 1e-12);
        assert!((s[0].crown_width_m - 2.0).abs() < 1e-12);
    }

    /// `audit-g-procgen2-determinism` finding 1: the hash feeds the `ProcCache` key of cacheable
    /// `plant.grow` / `plant.leaves` nodes, so every field growth or meshing reads must change it.
    #[test]
    fn hash_covers_every_field_growth_and_meshing_read() {
        let base = Plant {
            nodes: vec![node(None, 0.0)],
            buds: vec![PlantBud {
                node: 0,
                dir: [1.0, 0.0, 0.0],
                order: 1,
                state: BudState::Active,
                dormant_steps: 0,
                yaw_deg: 0.0,
            }],
            shed_branches: 0,
            ..Plant::default()
        };
        let skel = || PlantSkeleton {
            plants: vec![base.clone()],
        };
        let base_hash = skel().hash();

        let mut yaw = skel();
        yaw.plants[0].nodes[0].yaw_deg = 90.0;
        assert_ne!(yaw.hash(), base_hash, "node yaw_deg");

        let mut light = skel();
        light.plants[0].nodes[0].light = 0.25;
        assert_ne!(light.hash(), base_hash, "node light");

        let mut dormant = skel();
        dormant.plants[0].nodes[0].dormant_steps = 3;
        assert_ne!(dormant.hash(), base_hash, "node dormant_steps");

        let mut dim = skel();
        dim.plants[0].nodes[0].dim_steps = 3;
        assert_ne!(dim.hash(), base_hash, "node dim_steps");

        let mut bud_order = skel();
        bud_order.plants[0].buds[0].order = 2;
        assert_ne!(bud_order.hash(), base_hash, "bud order");

        let mut bud_dormant = skel();
        bud_dormant.plants[0].buds[0].dormant_steps = 3;
        assert_ne!(bud_dormant.hash(), base_hash, "bud dormant_steps");

        let mut bud_yaw = skel();
        bud_yaw.plants[0].buds[0].yaw_deg = 137.5;
        assert_ne!(bud_yaw.hash(), base_hash, "bud yaw_deg");

        let mut shed = skel();
        shed.plants[0].shed_branches = 1;
        assert_ne!(shed.hash(), base_hash, "shed_branches");

        // Attribute keys and columns are length-prefixed: re-splitting the same bytes under
        // different key boundaries must not collide. The first columns are empty so the pair
        // collides without the prefixes (the byte stream is "abc" + the same f32 bits either
        // way); non-empty columns would already differ pre-prefix by stream offset and length.
        let split_a = Plant {
            attrs: BTreeMap::from([("ab".to_string(), vec![]), ("c".to_string(), vec![1.0])]),
            ..base.clone()
        };
        let split_b = Plant {
            attrs: BTreeMap::from([("a".to_string(), vec![]), ("bc".to_string(), vec![1.0])]),
            ..base.clone()
        };
        let attr_len_a = Plant {
            attrs: BTreeMap::from([("k".to_string(), vec![1.0])]),
            ..base.clone()
        };
        let attr_len_b = Plant {
            attrs: BTreeMap::from([("k".to_string(), vec![1.0, 0.0])]),
            ..base.clone()
        };
        assert_ne!(
            PlantSkeleton {
                plants: vec![split_a]
            }
            .hash(),
            PlantSkeleton {
                plants: vec![split_b]
            }
            .hash(),
            "attribute key boundaries"
        );
        assert_ne!(
            PlantSkeleton {
                plants: vec![attr_len_a]
            }
            .hash(),
            PlantSkeleton {
                plants: vec![attr_len_b]
            }
            .hash(),
            "attribute column length"
        );
    }
}

/// Spec 68 § 10 test 10, second half: a build without the `pipeline` feature (a shipping game)
/// refuses to compile a Plant graph (`cargo test -p regia_procgen --lib` without features).
#[cfg(all(test, not(feature = "pipeline")))]
mod runtime_tests {
    use crate::nodes::Domain;
    use crate::{ProcGraph, ProcNodeRegistry};

    #[test]
    fn plant_graphs_are_forbidden_at_runtime() {
        let mut g = ProcGraph::default();
        g.flags.domain = Domain::Plant;
        let e = g
            .compile(&ProcNodeRegistry::with_builtins())
            .expect_err("no plant graphs without the pipeline");
        assert!(e.has("plant_runtime_forbidden"));
        assert!(ProcNodeRegistry::with_builtins().get("plant.grow").is_none());
    }
}
