// SPDX-License-Identifier: MIT-0
//! The client's presentation (feature `client`): a ground plate, a light, an overhead camera, one
//! box per avatar in its slot's color, and keyboard and gamepad input. The server never links it.
//! TODO(template): replace the boxes with your characters. The HUD is [`crate::hud`].

use std::sync::Arc;

use regia::core::math::{Quat, Vec3};
use regia::net::NetOwnedLocally;
use regia::prelude::*;
use regia_input::prelude::*;
use regia_render::{Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, LabVertex, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::transform::Transform;

use crate::{ARENA_HALF, Avatar, BODY_CENTER, Pad, PadSource};

/// Meshes, camera and light; with `input`, WASD / left stick run and Space / South fire.
pub struct NetGameView {
    /// Add the input plugin and write [`PadSource::Manual`] from it (off for headless captures).
    pub input: bool,
}

impl Plugin for NetGameView {
    fn build(&self, app: &mut App) {
        if self.input {
            let config = InputConfig::new()
                .action("Move", ActionKind::Axis2D)
                .action("Fire", ActionKind::Button)
                .action("Scores", ActionKind::Button)
                .binding(Binding::new(
                    "Move",
                    InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
                ))
                .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
                .binding(Binding::new("Fire", InputSource::key("Space")))
                .binding(Binding::new("Scores", InputSource::key("Tab")))
                .binding(Binding::new(
                    "Fire",
                    InputSource::gamepad_button(GamepadButton::South),
                ));
            app.add_plugins(InputPlugin::new(config))
                .add_systems(Update, read_pad);
        }
        app.add_systems(Startup, setup_view)
            .add_systems(Update, dress_avatars)
            // Before the HUD reads where the avatars are.
            .add_systems(
                PostUpdate,
                place_avatars
                    .after(regia::net::NetSet::Interpolate)
                    .before(PostUpdateSet::UiLayout),
            );
    }
}

/// The colors of the first four slots (then they repeat).
pub const SLOT_COLORS: [[f32; 3]; 4] = [
    [0.9, 0.35, 0.3],
    [0.3, 0.55, 0.95],
    [0.95, 0.8, 0.3],
    [0.45, 0.85, 0.45],
];

fn setup_view(mut commands: Commands) {
    let ground = Vec3::new(ARENA_HALF + 1.0, 0.05, ARENA_HALF + 1.0);
    let at = Transform::from_translation(Vec3::new(0.0, -0.05, 0.0));
    commands.spawn((
        at,
        GlobalTransform(at.compute_affine()),
        MeshRenderer::new(Arc::new(cube_mesh(ground)), Arc::new(material([0.35, 0.4, 0.35]))),
        DirectionalLight {
            direction: [0.3, -1.0, -0.4],
            color: [1.0, 0.97, 0.92],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    let eye = Transform {
        translation: Vec3::new(0.0, 24.0, 18.0),
        rotation: Quat::from_rotation_x(-0.93),
        scale: Vec3::ONE,
    };
    commands.spawn((
        eye,
        GlobalTransform(eye.compute_affine()),
        Camera {
            exposure_ev: Some(15.0),
            clear: ClearMode::Color([0.06, 0.07, 0.09, 1.0]),
            ..Camera::default()
        },
    ));
}

/// Keyboard and gamepad to this tick's [`Pad`]. Fire aims at the nearest other avatar's shown
/// center (an aim assist that keeps the template free of mouse picking).
fn read_pad(
    actions: Res<Actions>,
    mut source: ResMut<PadSource>,
    mine: Query<&Transform, (With<Avatar>, With<NetOwnedLocally>)>,
    others: Query<&Transform, (With<Avatar>, Without<NetOwnedLocally>)>,
) {
    let PadSource::Manual(pad) = &mut *source else {
        return;
    };
    let dir = actions.axis2d("Move");
    let here = mine.iter().next().map_or(Vec3::ZERO, |t| t.translation);
    let target = crate::nearest_center(here, others.iter().map(|t| t.translation));
    *pad = Pad {
        move_x: dir.x,
        move_z: -dir.y,
        fire: actions.pressed("Fire") && target.is_some(),
        aim: target.unwrap_or_default(),
    };
}

/// Avatars have no parent: their world transform is their (predicted or interpolated) transform.
/// Written here so the boxes follow without the scene plugin's propagation (headless captures).
fn place_avatars(mut avatars: Query<(&Transform, &mut GlobalTransform), With<Avatar>>) {
    for (t, mut global) in &mut avatars {
        global.0 = t.compute_affine();
    }
}

/// Gives each new avatar a box in its slot's color.
fn dress_avatars(
    mut commands: Commands,
    new: Query<(Entity, &Avatar, Option<&NetOwnedLocally>), Without<MeshRenderer>>,
) {
    for (e, avatar, mine) in &new {
        let mut color = SLOT_COLORS[avatar.slot as usize % SLOT_COLORS.len()];
        if mine.is_some() {
            color = color.map(|c| (c * 1.2).min(1.0));
        }
        commands.entity(e).insert((
            GlobalTransform(Transform::default().compute_affine()),
            MeshRenderer::new(
                Arc::new(cube_mesh(Vec3::new(0.4, BODY_CENTER, 0.4)).offset(Vec3::Y * BODY_CENTER)),
                Arc::new(material(color)),
            ),
        ));
    }
}

fn material(rgb: [f32; 3]) -> CpuMaterial {
    CpuMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        ..CpuMaterial::default()
    }
}

trait Offset {
    fn offset(self, by: Vec3) -> Self;
}

impl Offset for CpuMesh {
    fn offset(mut self, by: Vec3) -> Self {
        for v in &mut self.vertices {
            v.position = [v.position[0] + by.x, v.position[1] + by.y, v.position[2] + by.z];
        }
        self
    }
}

/// An axis-aligned box mesh with the given half-extents.
fn cube_mesh(half: Vec3) -> CpuMesh {
    let h = [half.x, half.y, half.z];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    // Each face: normal and the two axes its quad spans, ordered so u x v = n (front faces).
    let faces: [([f32; 3], usize, usize); 6] = [
        ([1.0, 0.0, 0.0], 1, 2),
        ([-1.0, 0.0, 0.0], 2, 1),
        ([0.0, 1.0, 0.0], 2, 0),
        ([0.0, -1.0, 0.0], 0, 2),
        ([0.0, 0.0, 1.0], 0, 1),
        ([0.0, 0.0, -1.0], 1, 0),
    ];
    for (normal, ua, va) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let mut position = [0.0_f32; 3];
            for (axis, value) in position.iter_mut().enumerate() {
                *value = normal[axis] * h[axis]
                    + if axis == ua { su * h[ua] } else { 0.0 }
                    + if axis == va { sv * h[va] } else { 0.0 };
            }
            vertices.push(LabVertex {
                position,
                normal,
                uv: [su * 0.5 + 0.5, sv * 0.5 + 0.5],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    CpuMesh { vertices, indices }
}
