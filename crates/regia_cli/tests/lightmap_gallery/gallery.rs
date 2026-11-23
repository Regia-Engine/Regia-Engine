// SPDX-License-Identifier: MPL-2.0
//! The underground gallery: a procedural interior built in Regia Engine for the v0.3.0 lightmap plate
//! review (ADR-3001). Original geometry, no imported or bought art.
//!
//! A gallery 18 m wide (x), 32 m long (z) with an 11 m ceiling, cut 5 m under a plateau. It is lit only
//! through three openings:
//!
//! - a 5 × 5 m skylight shaft through the ceiling's centre, up to the plateau: at noon the sun falls
//!   down it onto the floor;
//! - a side adit, 4 m wide and 4.5 m high, from the +x wall 12 m east to a cliff face over a sunlit
//!   valley floor;
//! - a ramp, 5 m wide, climbing 1:2 from the far (+z) end wall to the plateau, roofed for its first
//!   22 m and an open trench after.
//!
//! Two stone pillars stand in the gallery (smooth cylinders, so the chart-seam metric has curved
//! surfaces to measure). Every slab is a closed six-sided block 0.5 m thick. Blocks overlap where they
//! meet at an angle, and walls and the ceiling around an opening are a grid of blocks with shared
//! corners (no T-junctions), so no crack lets light in or shows the outside.
//!
//! Every mesh carries `POSITION`, `NORMAL` and `TEXCOORD_0` only: `TEXCOORD_0` is a tiling planar
//! texture coordinate (2 m a repeat, far outside 0..1, overlapping), as most game content has, and
//! there is no lightmap UV set, so `regia bake-lightmaps` must lay out the second UV set itself. The
//! level is lit by Earth's atmosphere at noon on day 172 at 40° N (sun 73° high in the south, toward
//! −z).

use std::fmt::Write as _;
use std::path::Path;

use base64::Engine as _;

/// Interior half width (x), metres.
pub const HALF_WIDTH: f32 = 9.0;
/// Interior ceiling height, metres.
pub const CEILING: f32 = 11.0;
/// Interior half length (z), metres.
pub const HALF_LENGTH: f32 = 16.0;
/// The plateau's top, metres.
pub const SURFACE: f32 = 16.5;
/// Slab thickness, metres.
pub const SLAB: f32 = 0.5;
/// The skylight's half size (x and z), metres.
pub const SKYLIGHT_HALF: f32 = 2.5;
/// The adit's interior: z range, height, and the cliff face's x.
pub const ADIT_Z: (f32, f32) = (4.0, 8.0);
/// The adit's interior height, metres.
pub const ADIT_HEIGHT: f32 = 4.5;
/// The cliff's outer face (x), where the adit opens.
pub const CLIFF_X: f32 = 21.5;
/// The ramp's interior x range.
pub const RAMP_X: (f32, f32) = (-7.0, -2.0);
/// The ramp's rise per metre of run.
pub const RAMP_SLOPE: f32 = 0.5;
/// The ramp's headroom (vertical), metres.
pub const RAMP_HEADROOM: f32 = 5.0;
/// Repeat of the tiling `TEXCOORD_0`, metres.
pub const UV0_REPEAT_M: f32 = 2.0;

/// The level's `LevelSettings` resource, written into the level and parsed by the tests so the
/// frame and the reference see the light the bake sees.
pub const LEVEL_SETTINGS_RON: &str =
    "(atmosphere: (preset: Earth, time_of_day: Some((hour: 12.0, day_of_year: 172, latitude_deg: 40.0))))";

/// The level's path in the fixture project.
pub const LEVEL: &str = "assets/levels/gallery.level.ron";
/// Where `regia bake-lightmaps --scene` writes by default.
pub const DEFAULT_OUT: &str = "assets/maps/gallery.flmap";

/// A material: linear base colour, roughness (metallic 0).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Linear base colour.
    pub base_color: [f32; 3],
    /// Perceptual roughness.
    pub roughness: f32,
}

/// One static entity of the level: one mesh, one material, world space (identity transform).
#[derive(Clone, Debug)]
pub struct Part {
    /// Entity name.
    pub name: &'static str,
    /// Entity guid (the lightmap entry's key).
    pub guid: &'static str,
    /// Mesh file stem (`assets/meshes/<file>.gltf`).
    pub file: &'static str,
    /// The material.
    pub material: Material,
    /// Positions, metres.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals.
    pub normals: Vec<[f32; 3]>,
    /// Tiling texture coordinates (`TEXCOORD_0`).
    pub uv0: Vec<[f32; 2]>,
    /// Triangle list, counter-clockwise from outside.
    pub indices: Vec<u32>,
}

impl Part {
    fn new(name: &'static str, guid: &'static str, file: &'static str, material: Material) -> Self {
        Self {
            name,
            guid,
            file,
            material,
            positions: Vec::new(),
            normals: Vec::new(),
            uv0: Vec::new(),
            indices: Vec::new(),
        }
    }

    /// The mesh's path relative to the project.
    pub fn mesh_path(&self) -> String {
        format!("assets/meshes/{}.gltf", self.file)
    }

    /// Adds a planar quad `q` (counter-clockwise seen along `normal`) with a flat normal.
    fn quad(&mut self, q: [[f32; 3]; 4], normal: [f32; 3]) {
        let base = self.positions.len() as u32;
        // Planar texture coordinates on the two axes the normal is not dominant in.
        let (a, b) = dominant_plane(normal);
        for p in q {
            self.positions.push(p);
            self.normals.push(normal);
            self.uv0.push([p[a] / UV0_REPEAT_M, p[b] / UV0_REPEAT_M]);
        }
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// Adds a closed six-sided block from its eight corners: `c[0..4]` one ring, `c[4..8]` the
    /// opposite ring in the same order. Each face is emitted facing out of the block.
    fn hexa(&mut self, c: [[f32; 3]; 8]) {
        let centre = c.iter().fold([0.0; 3], |s, p| add(s, scale(*p, 1.0 / 8.0)));
        let faces = [
            [0, 1, 2, 3],
            [4, 5, 6, 7],
            [0, 1, 5, 4],
            [1, 2, 6, 5],
            [2, 3, 7, 6],
            [3, 0, 4, 7],
        ];
        for f in faces {
            let mut q = f.map(|i| c[i]);
            let mut n = normalize(cross(sub(q[2], q[0]), sub(q[3], q[1])));
            let face_centre = q.iter().fold([0.0; 3], |s, p| add(s, scale(*p, 0.25)));
            if dot(n, sub(face_centre, centre)) < 0.0 {
                q.reverse();
                n = scale(n, -1.0);
            }
            self.quad(q, n);
        }
    }

    /// Adds an axis-aligned block.
    fn block(&mut self, min: [f32; 3], max: [f32; 3]) {
        let [x0, y0, z0] = min;
        let [x1, y1, z1] = max;
        assert!(x1 > x0 && y1 > y0 && z1 > z0, "an empty block {min:?} {max:?}");
        self.hexa([
            [x0, y0, z0],
            [x1, y0, z0],
            [x1, y0, z1],
            [x0, y0, z1],
            [x0, y1, z0],
            [x1, y1, z0],
            [x1, y1, z1],
            [x0, y1, z1],
        ]);
    }

    /// Adds the blocks of a grid over the three axes' cuts, without the cells `skip` refuses: an
    /// opening in a slab whose neighbours share their corners, so the slab has no T-junctions.
    fn grid(&mut self, x: &[f32], y: &[f32], z: &[f32], skip: impl Fn(usize, usize, usize) -> bool) {
        for (i, xs) in x.windows(2).enumerate() {
            for (j, ys) in y.windows(2).enumerate() {
                for (k, zs) in z.windows(2).enumerate() {
                    if !skip(i, j, k) {
                        self.block([xs[0], ys[0], zs[0]], [xs[1], ys[1], zs[1]]);
                    }
                }
            }
        }
    }

    /// Adds a block over `x` and `z` whose bottom and top follow `y(z) = (bottom, top)` linearly
    /// between `z.0` and `z.1` (a sloped slab or wall).
    fn sloped(&mut self, x: (f32, f32), z: (f32, f32), y: impl Fn(f32) -> (f32, f32)) {
        let (b0, t0) = y(z.0);
        let (b1, t1) = y(z.1);
        self.hexa([
            [x.0, b0, z.0],
            [x.1, b0, z.0],
            [x.1, b1, z.1],
            [x.0, b1, z.1],
            [x.0, t0, z.0],
            [x.1, t0, z.0],
            [x.1, t1, z.1],
            [x.0, t1, z.1],
        ]);
    }

    /// Adds a smooth cylinder standing on `y0` up to `y1` around `(cx, cz)` (side only; its ends
    /// sit in the floor and the ceiling slabs).
    fn cylinder(&mut self, centre: (f32, f32), radius: f32, y: (f32, f32), segments: u32) {
        let base = self.positions.len() as u32;
        for i in 0..=segments {
            let a = std::f32::consts::TAU * i as f32 / segments as f32;
            let n = [a.cos(), 0.0, a.sin()];
            let arc = radius * a / UV0_REPEAT_M;
            for yy in [y.0, y.1] {
                self.positions
                    .push([centre.0 + radius * n[0], yy, centre.1 + radius * n[2]]);
                self.normals.push(n);
                self.uv0.push([arc, yy / UV0_REPEAT_M]);
            }
        }
        for i in 0..segments {
            let (b0, t0, b1, t1) = (base + 2 * i, base + 2 * i + 1, base + 2 * i + 2, base + 2 * i + 3);
            // Counter-clockwise seen from outside (the normal points away from the axis).
            self.indices.extend_from_slice(&[b0, t0, t1, b0, t1, b1]);
        }
    }
}

fn dominant_plane(n: [f32; 3]) -> (usize, usize) {
    let a = n.map(f32::abs);
    if a[0] >= a[1] && a[0] >= a[2] {
        (2, 1)
    } else if a[1] >= a[2] {
        (0, 2)
    } else {
        (0, 1)
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn normalize(a: [f32; 3]) -> [f32; 3] {
    scale(a, 1.0 / dot(a, a).sqrt())
}

/// Floor stone: dark and slightly warm.
pub const FLOOR: Material = Material {
    base_color: [0.30, 0.29, 0.27],
    roughness: 0.9,
};
/// The gallery's rock: a pale limestone.
pub const ROCK: Material = Material {
    base_color: [0.55, 0.50, 0.43],
    roughness: 0.95,
};
/// The plateau and the valley floor outside.
pub const GROUND: Material = Material {
    base_color: [0.36, 0.33, 0.26],
    roughness: 0.95,
};
/// The pillars: dressed stone, a little lighter and smoother.
pub const PILLAR: Material = Material {
    base_color: [0.62, 0.60, 0.56],
    roughness: 0.8,
};

/// Every static part of the gallery, in level order.
#[must_use]
pub fn parts() -> Vec<Part> {
    let (w, h, l, s, t) = (HALF_WIDTH, CEILING, HALF_LENGTH, SURFACE, SLAB);
    let k = SKYLIGHT_HALF;
    let (az0, az1) = ADIT_Z;
    let (rx0, rx1) = RAMP_X;
    let ramp_floor = |z: f32| (z - l) * RAMP_SLOPE;
    // Where the ramp's roof meets the plateau's underside, and where its floor reaches the surface.
    let roof_end = l + (s - t - RAMP_HEADROOM) / RAMP_SLOPE;
    let ramp_end = l + s / RAMP_SLOPE;
    // The plateau's extent.
    let (px0, pz0, pz1) = (-16.0, -22.0, ramp_end + 7.0);

    let mut floor = Part::new(
        "Floor",
        "00000000-0000-0000-0000-00000000f001",
        "gallery_floor",
        FLOOR,
    );
    floor.block([-w - t, -t, -l - t], [w + t, 0.0, l + t]);
    // The adit's floor runs out to the cliff face.
    floor.block([w, -t, az0 - t], [CLIFF_X, 0.0, az1 + t]);
    // The ramp's floor, from the end wall up to the surface.
    floor.sloped((rx0 - t, rx1 + t), (l, ramp_end), |z| {
        (ramp_floor(z) - t, ramp_floor(z))
    });

    let mut gallery = Part::new(
        "Gallery",
        "00000000-0000-0000-0000-00000000f002",
        "gallery_rock",
        ROCK,
    );
    // Ceiling around the skylight.
    gallery.grid(
        &[-w - t, -k, k, w + t],
        &[h, h + t],
        &[-l - t, -k, k, l + t],
        |i, _, k| i == 1 && k == 1,
    );
    // West wall, whole.
    gallery.block([-w - t, 0.0, -l - t], [-w, h, l + t]);
    // East wall around the adit's mouth.
    gallery.grid(
        &[w, w + t],
        &[0.0, ADIT_HEIGHT, h],
        &[-l - t, az0, az1, l + t],
        |_, j, k| j == 0 && k == 1,
    );
    // South end wall, whole; north end wall around the ramp's mouth.
    gallery.block([-w - t, 0.0, -l - t], [w + t, h, -l]);
    gallery.grid(
        &[-w - t, rx0, rx1, w + t],
        &[0.0, RAMP_HEADROOM, h],
        &[l, l + t],
        |i, j, _| i == 1 && j == 0,
    );

    let mut shaft = Part::new(
        "Skylight shaft",
        "00000000-0000-0000-0000-00000000f003",
        "gallery_shaft",
        ROCK,
    );
    shaft.block([-k - t, h, -k - t], [k + t, s, -k]);
    shaft.block([-k - t, h, k], [k + t, s, k + t]);
    shaft.block([-k - t, h, -k], [-k, s, k]);
    shaft.block([k, h, -k], [k + t, s, k]);

    let mut adit = Part::new(
        "Adit",
        "00000000-0000-0000-0000-00000000f004",
        "gallery_adit",
        ROCK,
    );
    adit.block([w, ADIT_HEIGHT, az0 - t], [CLIFF_X, ADIT_HEIGHT + t, az1 + t]);
    adit.block([w + t, 0.0, az0 - t], [CLIFF_X, ADIT_HEIGHT, az0]);
    adit.block([w + t, 0.0, az1], [CLIFF_X, ADIT_HEIGHT, az1 + t]);

    let mut ramp = Part::new(
        "Ramp",
        "00000000-0000-0000-0000-00000000f005",
        "gallery_ramp",
        ROCK,
    );
    // The roof, from the end wall to where it meets the plateau.
    ramp.sloped((rx0 - t, rx1 + t), (l, roof_end), |z| {
        let under = ramp_floor(z) + RAMP_HEADROOM;
        (under, under + t)
    });
    for x in [(rx0 - t, rx0), (rx1, rx1 + t)] {
        // Side walls under the roof, then the open trench's walls up to the surface.
        ramp.sloped(x, (l, roof_end), |z| {
            (ramp_floor(z) - t, ramp_floor(z) + RAMP_HEADROOM + t)
        });
        ramp.sloped(x, (roof_end, ramp_end), |z| (ramp_floor(z) - t, s));
    }

    let mut surface = Part::new(
        "Surface",
        "00000000-0000-0000-0000-00000000f006",
        "gallery_surface",
        GROUND,
    );
    let plateau = |p: &mut Part, x: (f32, f32), z: (f32, f32)| p.block([x.0, s - t, z.0], [x.1, s, z.1]);
    let px1 = CLIFF_X;
    let kk = k + t;
    plateau(&mut surface, (px0, px1), (pz0, -kk));
    plateau(&mut surface, (px0, -kk), (-kk, kk));
    plateau(&mut surface, (kk, px1), (-kk, kk));
    plateau(&mut surface, (px0, px1), (kk, roof_end));
    plateau(&mut surface, (px0, rx0 - t), (roof_end, ramp_end));
    plateau(&mut surface, (rx1 + t, px1), (roof_end, ramp_end));
    plateau(&mut surface, (px0, px1), (ramp_end, pz1));
    // The cliff face around the adit's portal, and the valley floor below it.
    surface.block([CLIFF_X - t, -t, pz0], [CLIFF_X, s, az0 - t]);
    surface.block([CLIFF_X - t, -t, az1 + t], [CLIFF_X, s, pz1]);
    surface.block([CLIFF_X - t, ADIT_HEIGHT + t, az0 - t], [CLIFF_X, s, az1 + t]);
    surface.block([CLIFF_X, -t, pz0], [CLIFF_X + 24.0, 0.0, pz1]);

    let mut pillars = Part::new(
        "Pillars",
        "00000000-0000-0000-0000-00000000f007",
        "gallery_pillars",
        PILLAR,
    );
    for x in [-4.5, 4.5] {
        pillars.cylinder((x, 10.0), 0.6, (0.0, h), 24);
    }

    vec![floor, gallery, shaft, adit, ramp, surface, pillars]
}

/// The glTF text of `part`: one buffer as a `data:` URI, one mesh with one primitive carrying
/// `POSITION`, `NORMAL` and `TEXCOORD_0`, one material, one node.
#[must_use]
pub fn gltf_text(part: &Part) -> String {
    let mut bin = Vec::new();
    let push = |bin: &mut Vec<u8>, values: &mut dyn Iterator<Item = f32>| {
        let start = bin.len();
        for v in values {
            bin.extend_from_slice(&v.to_le_bytes());
        }
        (start, bin.len() - start)
    };
    let (pos_off, pos_len) = push(&mut bin, &mut part.positions.iter().flatten().copied());
    let (nrm_off, nrm_len) = push(&mut bin, &mut part.normals.iter().flatten().copied());
    let (uv_off, uv_len) = push(&mut bin, &mut part.uv0.iter().flatten().copied());
    let idx_off = bin.len();
    for i in &part.indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let idx_len = bin.len() - idx_off;
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &part.positions {
        for a in 0..3 {
            min[a] = min[a].min(p[a]);
            max[a] = max[a].max(p[a]);
        }
    }
    let n = part.positions.len();
    let c = part.material.base_color;
    let uri = base64::engine::general_purpose::STANDARD.encode(&bin);
    let mut s = String::new();
    // Hand-written JSON keeps the file stable byte for byte (no map ordering, fixed float text).
    let _ = write!(
        s,
        concat!(
            "{{\n",
            "  \"asset\": {{\"version\": \"2.0\", \"generator\": \"regia_cli tests/lightmap_gallery (ADR-3001)\"}},\n",
            "  \"scene\": 0,\n",
            "  \"scenes\": [{{\"nodes\": [0]}}],\n",
            "  \"nodes\": [{{\"name\": \"{name}\", \"mesh\": 0}}],\n",
            "  \"materials\": [{{\"name\": \"{file}\", \"pbrMetallicRoughness\": {{\"baseColorFactor\": [{r:?}, {g:?}, {b:?}, 1.0], \"metallicFactor\": 0.0, \"roughnessFactor\": {rough:?}}}}}],\n",
            "  \"meshes\": [{{\"name\": \"{name}\", \"primitives\": [{{\"attributes\": {{\"POSITION\": 0, \"NORMAL\": 1, \"TEXCOORD_0\": 2}}, \"indices\": 3, \"material\": 0}}]}}],\n",
            "  \"accessors\": [\n",
            "    {{\"bufferView\": 0, \"componentType\": 5126, \"count\": {n}, \"type\": \"VEC3\", \"min\": [{min0:?}, {min1:?}, {min2:?}], \"max\": [{max0:?}, {max1:?}, {max2:?}]}},\n",
            "    {{\"bufferView\": 1, \"componentType\": 5126, \"count\": {n}, \"type\": \"VEC3\"}},\n",
            "    {{\"bufferView\": 2, \"componentType\": 5126, \"count\": {n}, \"type\": \"VEC2\"}},\n",
            "    {{\"bufferView\": 3, \"componentType\": 5125, \"count\": {ni}, \"type\": \"SCALAR\"}}\n",
            "  ],\n",
            "  \"bufferViews\": [\n",
            "    {{\"buffer\": 0, \"byteOffset\": {pos_off}, \"byteLength\": {pos_len}, \"target\": 34962}},\n",
            "    {{\"buffer\": 0, \"byteOffset\": {nrm_off}, \"byteLength\": {nrm_len}, \"target\": 34962}},\n",
            "    {{\"buffer\": 0, \"byteOffset\": {uv_off}, \"byteLength\": {uv_len}, \"target\": 34962}},\n",
            "    {{\"buffer\": 0, \"byteOffset\": {idx_off}, \"byteLength\": {idx_len}, \"target\": 34963}}\n",
            "  ],\n",
            "  \"buffers\": [{{\"byteLength\": {blen}, \"uri\": \"data:application/octet-stream;base64,{uri}\"}}]\n",
            "}}\n"
        ),
        name = part.name,
        file = part.file,
        r = c[0],
        g = c[1],
        b = c[2],
        rough = part.material.roughness,
        n = n,
        ni = part.indices.len(),
        min0 = min[0],
        min1 = min[1],
        min2 = min[2],
        max0 = max[0],
        max1 = max[1],
        max2 = max[2],
        pos_off = pos_off,
        pos_len = pos_len,
        nrm_off = nrm_off,
        nrm_len = nrm_len,
        uv_off = uv_off,
        uv_len = uv_len,
        idx_off = idx_off,
        idx_len = idx_len,
        blen = bin.len(),
        uri = uri,
    );
    s
}

/// The level text: one static entity per part at the origin, and [`LEVEL_SETTINGS_RON`].
#[must_use]
pub fn level_text(parts: &[Part]) -> String {
    let mut s = String::from("SceneFile(\n    format_version: 10,\n    entities: [\n");
    for p in parts {
        let _ = writeln!(
            s,
            "        (guid: \"{}\", name: \"{}\", components: {{\n            \"regia_scene::transform::Transform\": (translation: (0.0, 0.0, 0.0)),\n            \"regia_scene::components::GiMobility\": Static,\n            \"regia_render::runtime::MeshSource\": (mesh: \"{}\", material: \"\"),\n        }}),",
            p.guid,
            p.name,
            p.mesh_path()
        );
    }
    let _ = write!(
        s,
        "    ],\n    resources: {{\n        \"regia_scene::level::LevelSettings\": {LEVEL_SETTINGS_RON},\n    }},\n)\n"
    );
    s
}

/// Every file of the fixture project, `(relative path, text)`.
#[must_use]
pub fn project_files() -> Vec<(String, String)> {
    let parts = parts();
    let mut files = vec![
        (
            "Regia.toml".to_string(),
            "[project]\nname = \"lightmap_gallery\"\nid = \"dev.regia.lightmap_gallery\"\nversion = \"0.1.0\"\n".to_string(),
        ),
        (LEVEL.to_string(), level_text(&parts)),
    ];
    for p in &parts {
        files.push((p.mesh_path(), gltf_text(p)));
    }
    files
}

/// Writes the fixture project under `dir`.
///
/// # Panics
///
/// On a file system error (test code).
pub fn write_project(dir: &Path) {
    for (rel, text) in project_files() {
        let path = dir.join(&rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the project's directories");
        std::fs::write(&path, text).expect("write a project file");
    }
}
