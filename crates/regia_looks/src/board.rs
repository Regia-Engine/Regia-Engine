// SPDX-License-Identifier: MPL-2.0
//! The look board: every stand-in cut, clear and colored stones, frosted, cabochons, opals, stones in
//! bezel cups, ring bands in each metal and finish, and anodized beads, laid out in rows (millimeters,
//! camera straight on the front axis). The owner judges the look from a still of it; the GPU tests
//! and both renderers draw it.

use glam::{Mat4, Vec3};

use crate::cuts::{CutType, GemCut};
use crate::material::{Library, LookError, LookMaterial, StoneSize};
use crate::mesh::LookMesh;

/// A perspective camera in millimeters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookCamera {
    /// Eye position.
    pub eye: Vec3,
    /// Look-at target.
    pub target: Vec3,
    /// Vertical field of view in degrees.
    pub fov_y_deg: f32,
}

impl LookCamera {
    /// World to clip space for a target of this aspect ratio (width / height).
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let dist = self.eye.distance(self.target).max(1e-3);
        let proj = glam::camera::rh::proj::directx::perspective(
            self.fov_y_deg.to_radians(),
            aspect,
            dist * 0.05,
            dist * 20.0,
        );
        proj * glam::camera::rh::view::look_at_mat4(self.eye, self.target, Vec3::Y)
    }
}

/// One drawable: a mesh, its material and its placement.
#[derive(Debug, Clone)]
pub struct LookItem {
    /// The mesh (millimeters).
    pub mesh: LookMesh,
    /// The material.
    pub material: LookMaterial,
    /// Model transform (rotation and uniform scale only).
    pub model: Mat4,
    /// Stone size (ignored by metals).
    pub stone: StoneSize,
}

/// A still scene.
#[derive(Debug, Clone)]
pub struct LookScene {
    /// The camera.
    pub camera: LookCamera,
    /// Draw list (opaque kinds first is not required; every look writes depth).
    pub items: Vec<LookItem>,
    /// Exposure multiplier before tone mapping.
    pub exposure: f32,
}

/// The look board: four rows in millimeters, camera straight on the front axis.
pub fn look_board(tilt_deg: f32) -> Result<LookScene, LookError> {
    let lib = Library::builtin()?;
    let look = |id: &str| lib.look(id, None);
    let metal = |id: &str, finish: &str| lib.look(id, Some(finish));
    let tilt = Mat4::from_rotation_x(-tilt_deg.to_radians());
    let mut items = Vec::new();
    let gem =
        |cut: CutType, width: f32, mat: LookMaterial, at: Vec3, spin: f32, items: &mut Vec<LookItem>| {
            let c = GemCut::new(cut, [width * cut.length_to_width(), width]);
            items.push(LookItem {
                mesh: c.mesh(),
                material: mat,
                model: Mat4::from_translation(at) * tilt * Mat4::from_rotation_z(spin.to_radians()),
                stone: c.stone_size(),
            });
        };
    // Row 1: loose faceted stones, every stand-in cut.
    let y1 = 12.0;
    gem(
        CutType::RoundBrilliant,
        6.0,
        look("diamond-white")?,
        Vec3::new(-27.0, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::Marquise,
        4.0,
        look("sapphire-blue")?,
        Vec3::new(-16.5, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::Pear,
        4.6,
        look("ruby-red")?,
        Vec3::new(-6.5, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::Princess,
        5.0,
        look("emerald-green")?,
        Vec3::new(3.0, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::Heart,
        5.6,
        look("amethyst")?,
        Vec3::new(12.5, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::RoundBrilliant,
        5.6,
        look("cz-frosted")?,
        Vec3::new(22.0, y1, 0.0),
        0.0,
        &mut items,
    );
    gem(
        CutType::RoundBrilliant,
        5.0,
        look("citrine")?,
        Vec3::new(30.5, y1, 0.0),
        0.0,
        &mut items,
    );
    // Row 2: cabochons and opals, and stones set in bezel cups.
    let y2 = 3.0;
    let cab = |d: f32, mat: LookMaterial, at: Vec3, items: &mut Vec<LookItem>| {
        items.push(LookItem {
            mesh: LookMesh::cabochon(d, d * 0.33, 64, 20),
            material: mat,
            model: Mat4::from_translation(at) * tilt,
            stone: StoneSize::round(d, d * 0.33),
        });
    };
    cab(5.5, look("opal-white")?, Vec3::new(-27.0, y2, 0.0), &mut items);
    cab(5.5, look("opal-dark")?, Vec3::new(-19.0, y2, 0.0), &mut items);
    cab(
        5.5,
        look("garnet-red")?.for_cut(false),
        Vec3::new(-11.0, y2, 0.0),
        &mut items,
    );
    let set = |stone: &str,
               metal_mat: LookMaterial,
               at: Vec3,
               items: &mut Vec<LookItem>|
     -> Result<(), LookError> {
        let d = 5.0;
        let c = GemCut::new(CutType::RoundBrilliant, [d, d]);
        let place = Mat4::from_translation(at) * tilt;
        items.push(LookItem {
            mesh: LookMesh::bezel_cup(d + 0.8, 0.45, 3.0, 96),
            material: metal_mat,
            model: place * Mat4::from_translation(Vec3::new(0.0, 0.0, 0.15)),
            stone: StoneSize::default(),
        });
        items.push(LookItem {
            mesh: c.mesh(),
            material: lib.look(stone, None)?,
            model: place,
            stone: c.stone_size(),
        });
        Ok(())
    };
    set(
        "diamond-white",
        metal("gold-yellow-18k", "finish-polish")?,
        Vec3::new(-1.5, y2, 0.0),
        &mut items,
    )?;
    set(
        "aquamarine",
        metal("gold-rose-14k", "finish-polish")?,
        Vec3::new(7.5, y2, 0.0),
        &mut items,
    )?;
    set(
        "peridot",
        metal("titanium-anodized-gold", "finish-polish")?,
        Vec3::new(16.5, y2, 0.0),
        &mut items,
    )?;
    set(
        "garnet-red",
        metal("platinum", "finish-polish")?,
        Vec3::new(25.5, y2, 0.0),
        &mut items,
    )?;
    // Row 3: ring bands, polished and brushed.
    let y3 = -6.5;
    let ring_tilt = Mat4::from_rotation_x(-55f32.to_radians());
    let rings = [
        ("gold-yellow-18k", "finish-polish"),
        ("gold-rose-14k", "finish-polish"),
        ("rhodium", "finish-polish"),
        ("platinum", "finish-brushed"),
        ("titanium", "finish-polish"),
        ("niobium-anodized-violet", "finish-polish"),
        ("steel", "finish-sandblast"),
        ("silver", "finish-polish"),
    ];
    for (i, (id, finish)) in rings.iter().enumerate() {
        items.push(LookItem {
            mesh: LookMesh::ring_band(7.0, 1.5, 128, 24),
            material: metal(id, finish)?,
            model: Mat4::from_translation(Vec3::new(-28.0 + i as f32 * 8.2, y3, 0.0)) * ring_tilt,
            stone: StoneSize::default(),
        });
    }
    // Row 4: anodized beads (titanium, then niobium).
    let y4 = -14.5;
    let beads = [
        "titanium-anodized-bronze",
        "titanium-anodized-purple",
        "titanium-anodized-blue",
        "titanium-anodized-light-blue",
        "titanium-anodized-gold",
        "titanium-anodized-rose",
        "titanium-anodized-magenta",
        "titanium-anodized-teal",
        "titanium-anodized-green",
        "niobium-anodized-blue",
        "niobium-anodized-pink",
        "niobium-anodized-green",
    ];
    for (i, id) in beads.iter().enumerate() {
        items.push(LookItem {
            mesh: LookMesh::ball(4.0, 64, 32),
            material: metal(id, "finish-polish")?,
            model: Mat4::from_translation(Vec3::new(-29.0 + i as f32 * 5.3, y4, 0.0)),
            stone: StoneSize::default(),
        });
    }
    Ok(LookScene {
        camera: LookCamera {
            eye: Vec3::new(0.0, -1.0, 150.0),
            target: Vec3::new(1.0, -1.0, 0.0),
            fov_y_deg: 15.5,
        },
        items,
        exposure: 1.0,
    })
}
