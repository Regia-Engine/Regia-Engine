// SPDX-License-Identifier: MPL-2.0
//! Hidden lights light nothing (spec 13 § 6 visibility note, spec 14 § A1, ADR-7200, ADR-7490,
//! ADR-7670).
//!
//! - A point, spot, rect, tube or directional light whose own `Visibility` is `Hidden` is not
//!   extracted: no light-list entry (so no shadow-atlas slot and no fog pick), no area-light input,
//!   no sun. Shown again, it is extracted the next frame.
//! - A light child of a hidden parent (no `Visibility` of its own: every light component requires
//!   `InheritedVisibility`) is not extracted either.
//! - On lavapipe, a frame with each kind of hidden light is byte-identical to a world that never had
//!   it (same frame number, fresh renderers), and differs from one where it is shown (the control).
//!
//! The ECS tests are CPU only; the image tests need a headless device (lavapipe).

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::{
    Camera, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame, FrameRenderer, LabVertex, LodViewport,
    MeshRenderer, PointLight, RectLight, RenderDevice, RenderProfilePreset, RenderSettings, SpotLight,
    TubeLight, ViewVisibilityStats, extract_frame, update_view_visibility,
};
use regia_scene::Transform;
use regia_scene::components::{InheritedVisibility, Visibility};
use regia_scene::propagate::propagate_transforms;

/// A unit cube (±0.5 m) around the origin.
fn cube() -> Arc<CpuMesh> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                0.5 * (n[0] + su * u[0] + sv * v[0]),
                0.5 * (n[1] + su * u[1] + sv * v[1]),
                0.5 * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(
                p,
                n,
                [0.5 * (su + 1.0), 0.5 * (sv + 1.0)],
            ));
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

/// The five light kinds the extraction reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Point,
    Spot,
    Rect,
    Tube,
    Sun,
}

const KINDS: [Kind; 5] = [Kind::Point, Kind::Spot, Kind::Rect, Kind::Tube, Kind::Sun];

/// An app with propagation in `PostUpdate` and extraction plus the `ViewVisibility` write in
/// `Render`, the `DesktopHigh` profile (it has an emitter budget, so rect and tube lights shade), a
/// window camera at the origin looking down −Z, a 64×48 viewport, and a ground slab whose top is
/// 1 m below the eye, in view.
fn app() -> App {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.init_resource::<ViewVisibilityStats>();
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
    app.insert_resource(LodViewport {
        width: 64,
        height: 48,
    });
    app.add_systems(PostUpdate, propagate_transforms);
    app.add_systems(Render, (extract_frame, update_view_visibility).chain());
    app.world_mut().spawn((
        Transform::default(),
        Camera {
            exposure_ev: Some(8.0),
            ..Camera::default()
        },
    ));
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, -1.25, -10.0),
            scale: Vec3::new(30.0, 0.5, 30.0),
            ..Transform::default()
        },
        MeshRenderer::new(
            cube(),
            Arc::new(CpuMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                ..CpuMaterial::default()
            }),
        ),
    ));
    app
}

/// Spawns one light of `kind` over the ground in view (1.5 m above it, 4 m ahead), its own
/// `visibility` when given, as a child of `parent` when given.
fn spawn_light(app: &mut App, kind: Kind, visibility: Option<Visibility>, parent: Option<Entity>) -> Entity {
    // Children sit at the parent's origin; roots at the spot over the ground.
    let at = if parent.is_some() {
        Vec3::ZERO
    } else {
        Vec3::new(0.0, 0.5, -4.0)
    };
    // −Z (the emitting/forward axis of spots and rects) turned to point down.
    let down = Quat::from_rotation_x(-FRAC_PI_2);
    let mut entity = app.world_mut().spawn(Transform::from_translation(at));
    match kind {
        Kind::Point => {
            entity.insert(PointLight {
                color: [1.0, 0.9, 0.8],
                intensity: 60_000.0,
                radius: 12.0,
                cast_shadows: true,
                ..PointLight::default()
            });
        }
        Kind::Spot => {
            entity.insert((
                SpotLight {
                    color: [0.8, 0.9, 1.0],
                    intensity: 8_000.0,
                    range: 12.0,
                    inner_angle: 0.4,
                    outer_angle: 0.8,
                    cast_shadows: true,
                    ..SpotLight::default()
                },
                Transform {
                    translation: at,
                    rotation: down,
                    ..Transform::default()
                },
            ));
        }
        Kind::Rect => {
            entity.insert((
                RectLight {
                    intensity: 60_000.0,
                    width: 1.0,
                    height: 1.0,
                    range: 12.0,
                    ..RectLight::default()
                },
                Transform {
                    translation: at,
                    rotation: down,
                    ..Transform::default()
                },
            ));
        }
        Kind::Tube => {
            entity.insert(TubeLight {
                intensity: 60_000.0,
                length: 1.0,
                radius: 0.05,
                range: 12.0,
                ..TubeLight::default()
            });
        }
        Kind::Sun => {
            entity.insert(DirectionalLight {
                direction: Vec3::new(0.3, -1.0, -0.2).normalize().to_array(),
                color: [1.0; 3],
                illuminance: 2_000.0,
                cast_shadows: true,
                static_shadow: false,
            });
        }
    }
    if let Some(visibility) = visibility {
        entity.insert(visibility);
    }
    if let Some(parent) = parent {
        entity.insert(ChildOf(parent));
    }
    entity.id()
}

/// How many lights of `kind` the last extraction took.
fn extracted_count(app: &App, kind: Kind) -> usize {
    let frame = app.world().resource::<ExtractedFrame>();
    match kind {
        Kind::Point => frame
            .local_lights
            .iter()
            .filter(|l| matches!(l, regia_render::LabLocalLight::Point(_)))
            .count(),
        Kind::Spot => frame
            .local_lights
            .iter()
            .filter(|l| matches!(l, regia_render::LabLocalLight::Spot(_)))
            .count(),
        // Rects and tubes share the area-light list; each test spawns one kind.
        Kind::Rect | Kind::Tube => frame.light_extras.area_lights.len(),
        Kind::Sun => usize::from(frame.directional.is_some()),
    }
}

#[test]
fn a_hidden_light_of_every_kind_is_not_extracted_and_returns_when_shown() {
    for kind in KINDS {
        let mut app = app();
        let light = spawn_light(&mut app, kind, Some(Visibility::Hidden), None);
        app.update();
        assert_eq!(
            extracted_count(&app, kind),
            0,
            "{kind:?}: a Hidden light is not extracted"
        );
        if kind == Kind::Sun {
            let frame = app.world().resource::<ExtractedFrame>();
            assert!(!frame.directional_shadows, "a hidden sun casts no cascades");
        }

        app.world_mut().entity_mut(light).insert(Visibility::Visible);
        app.update();
        assert_eq!(
            extracted_count(&app, kind),
            1,
            "{kind:?}: shown, it is extracted the next frame"
        );
    }
}

#[test]
fn a_light_child_of_a_hidden_parent_is_not_extracted() {
    for kind in KINDS {
        let mut app = app();
        let parent = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.5, -4.0)),
                Visibility::Hidden,
            ))
            .id();
        // No `Visibility` on the child: the light component brings `InheritedVisibility`.
        let child = spawn_light(&mut app, kind, None, Some(parent));
        app.update();
        assert_eq!(
            app.world().get::<InheritedVisibility>(child).map(|v| v.0),
            Some(false),
            "{kind:?}: the light child resolves hidden"
        );
        assert_eq!(
            extracted_count(&app, kind),
            0,
            "{kind:?}: a light under a hidden parent is not extracted"
        );

        app.world_mut().entity_mut(parent).insert(Visibility::Inherited);
        app.update();
        assert_eq!(
            extracted_count(&app, kind),
            1,
            "{kind:?}: the parent shown, the light is extracted the next frame"
        );
    }
}

/// The sun is the lowest-entity directional light (audit G): when that one is hidden, the pick
/// runs over drawn lights only and the other light is the sun, with its own shadow flag.
#[test]
fn a_hidden_lowest_entity_sun_leaves_the_pick_to_the_drawn_one() {
    let mut app = app();
    let sun = |illuminance: f32, cast_shadows: bool| DirectionalLight {
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        illuminance,
        cast_shadows,
        static_shadow: false,
    };
    let a = app.world_mut().spawn(sun(2_000.0, true)).id();
    let b = app.world_mut().spawn(sun(500.0, false)).id();
    // Hide whichever the pick would take; the other must be the sun.
    let (lowest, other, other_lux) = if a.to_bits() < b.to_bits() {
        (a, b, 500.0)
    } else {
        (b, a, 2_000.0)
    };
    app.update();
    let picked = app
        .world()
        .resource::<ExtractedFrame>()
        .directional
        .map(|d| d.illuminance);
    assert_ne!(
        picked,
        Some(other_lux),
        "both shown, the lowest entity is the sun"
    );
    app.world_mut().entity_mut(lowest).insert(Visibility::Hidden);
    app.update();
    let frame = app.world().resource::<ExtractedFrame>();
    let picked = frame.directional.map(|d| d.illuminance);
    assert_eq!(
        picked,
        Some(other_lux),
        "the drawn light {other:?} is the sun, not the hidden {lowest:?}"
    );
    assert_eq!(
        frame.directional_shadows,
        other_lux == 2_000.0,
        "the sun's shadow flag is the drawn light's"
    );
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The scene's first frame, rendered by a fresh renderer (frame number 1 for every world) with
/// the frame's light extras (rect and tube lights, profiles) installed as the runtime does.
fn render_first_frame(device: &RenderDevice, app: &mut App) -> Vec<u8> {
    app.update();
    let mut extracted = app.world_mut().resource_mut::<ExtractedFrame>();
    // No ambient: the light under test is the only light.
    extracted.environment.ambient_lux = 0.0;
    let extras = extracted.light_extras.clone();
    let scene = extracted.frame_scene(0, 64, 48);
    let mut renderer = FrameRenderer::new();
    renderer.set_light_extras(extras);
    renderer.render_offscreen(device, &scene).expect("frame").pixels
}

/// Hidden, never there, shown: the hidden frame equals the never frame byte for byte, and the
/// shown frame differs (the control: the light really lights the ground in view).
fn assert_hidden_is_absent(device: &RenderDevice, kind: Kind, hidden: Vec<u8>) {
    let never = render_first_frame(device, &mut app());
    let mut shown_app = app();
    spawn_light(&mut shown_app, kind, Some(Visibility::Visible), None);
    let shown = render_first_frame(device, &mut shown_app);
    assert_eq!(hidden.len(), never.len());
    let differing = hidden.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "{kind:?}: the hidden light left {differing} bytes on the image"
    );
    let lit = shown.iter().zip(&never).filter(|(a, b)| a != b).count();
    assert!(
        lit > 64,
        "{kind:?}: the shown light changes the image ({lit} bytes differ): the test sees it"
    );
}

#[test]
fn a_hidden_light_of_every_kind_renders_like_a_world_that_never_had_it() {
    let device = device();
    for kind in KINDS {
        let mut hidden_app = app();
        spawn_light(&mut hidden_app, kind, Some(Visibility::Hidden), None);
        let hidden = render_first_frame(&device, &mut hidden_app);
        assert_hidden_is_absent(&device, kind, hidden);
    }
}

#[test]
fn a_light_under_a_hidden_parent_renders_like_a_world_that_never_had_it() {
    let device = device();
    for kind in [Kind::Point, Kind::Sun] {
        let mut hidden_app = app();
        let parent = hidden_app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.5, -4.0)),
                Visibility::Hidden,
            ))
            .id();
        spawn_light(&mut hidden_app, kind, None, Some(parent));
        let hidden = render_first_frame(&device, &mut hidden_app);
        assert_hidden_is_absent(&device, kind, hidden);
    }
}
