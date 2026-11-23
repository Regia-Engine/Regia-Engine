// SPDX-License-Identifier: MPL-2.0
//! UI materials (ADR-273 T09, ADR-2045): a per-widget procedural fill drawn by the overlay
//! shader (kind 8) over the node's background and under its content.
//!
//! A material is a small closed set of parametric effects ([`UiMaterialKind`]) with two colors
//! and four numbers, not arbitrary WGSL: every kind is one branch of `ui-overlay.wgsl`, so a
//! material costs no pipeline, bind group or draw call of its own and batches with the rest of
//! the UI. Animated kinds advance [`UiMaterial::time`] from the frame clock in
//! [`tick_ui_materials`] without marking the node changed. Extract adds that frame's delta to the
//! material quads already in the draw list, so a steady panel stays cached. Nodes without a
//! [`UiMaterial`] draw exactly as before.
//!
//! [`material_mix`] is the CPU model of the shader for the kinds whose math is closed-form
//! (shine, radial wipe, scanlines, pulse), so tests and tools can check a pixel without a GPU.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::color::UiColor;
use crate::draw::{TextureRef, UiQuad};

/// The built-in material effects. The discriminant is the shader's material kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiMaterialKind {
    /// A soft band sweeping across the node (button highlight). Params: speed (sweeps per
    /// second), band width (fraction of the node), angle (degrees, 0 = left to right), pause
    /// (fraction of a cycle spent off screen).
    #[default]
    Shine = 0,
    /// A clockwise sector from 12 o'clock filled with `color2` (cooldown, radial progress).
    /// Params: fraction (0..1), edge softness (radians).
    RadialWipe = 1,
    /// Value-noise dissolve (appear/disappear). Params: threshold (0..1; pixels whose noise is
    /// below it vanish), cell size (px), edge width (noise units) tinted `color2`.
    Dissolve = 2,
    /// Horizontal scanlines. Params: period (px), scroll speed (px/s), strength (0..1).
    Scanlines = 3,
    /// `color` → `color2` → `color`. Params: period (s).
    Pulse = 4,
}

impl UiMaterialKind {
    /// Every kind, in discriminant order.
    pub const ALL: [UiMaterialKind; 5] = [
        UiMaterialKind::Shine,
        UiMaterialKind::RadialWipe,
        UiMaterialKind::Dissolve,
        UiMaterialKind::Scanlines,
        UiMaterialKind::Pulse,
    ];

    /// The wire name (`"shine"`, `"radial_wipe"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            UiMaterialKind::Shine => "shine",
            UiMaterialKind::RadialWipe => "radial_wipe",
            UiMaterialKind::Dissolve => "dissolve",
            UiMaterialKind::Scanlines => "scanlines",
            UiMaterialKind::Pulse => "pulse",
        }
    }

    /// Parses a wire name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// The default parameters of the kind.
    pub fn default_params(self) -> [f32; 4] {
        match self {
            UiMaterialKind::Shine => [0.5, 0.25, 20.0, 1.0],
            UiMaterialKind::RadialWipe => [0.25, 0.02, 0.0, 0.0],
            UiMaterialKind::Dissolve => [0.0, 8.0, 0.08, 0.0],
            UiMaterialKind::Scanlines => [4.0, 0.0, 0.25, 0.0],
            UiMaterialKind::Pulse => [1.5, 0.0, 0.0, 0.0],
        }
    }

    /// True when the kind changes with time (and so redraws every frame while animated).
    pub fn uses_time(self) -> bool {
        matches!(
            self,
            UiMaterialKind::Shine | UiMaterialKind::Scanlines | UiMaterialKind::Pulse
        )
    }
}

/// A procedural fill on a UI node (ADR-2045). Drawn over the node's background and under its
/// image, text, children and border, clipped to the node's rounded rect.
#[derive(Component, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiMaterial {
    /// Which effect.
    pub kind: UiMaterialKind,
    /// Base color (linear, straight alpha).
    pub color: UiColor,
    /// Second color (band, filled sector, edge, pulse peak).
    pub color2: UiColor,
    /// The kind's four parameters ([`UiMaterialKind`] documents each).
    pub params: [f32; 4],
    /// Material time in seconds.
    pub time: f32,
    /// Advance [`Self::time`] from the frame clock ([`tick_ui_materials`]).
    pub animate: bool,
}

impl Default for UiMaterial {
    fn default() -> Self {
        Self::new(UiMaterialKind::Shine)
    }
}

impl UiMaterial {
    /// A material of `kind` with its default parameters, a transparent base and a white
    /// second color, animating when the kind uses time.
    pub fn new(kind: UiMaterialKind) -> Self {
        Self {
            kind,
            color: UiColor::TRANSPARENT,
            color2: UiColor([1.0, 1.0, 1.0, 0.35]),
            params: kind.default_params(),
            time: 0.0,
            animate: kind.uses_time(),
        }
    }

    /// The material's draw quad over `rect` (x, y, w, h physical px) with `radius` corners.
    /// Non-finite parameters draw as 0.
    pub fn quad(&self, rect: [f32; 4], radius: [f32; 4]) -> UiQuad {
        let clean = |v: f32| if v.is_finite() { v } else { 0.0 };
        UiQuad {
            color: self.color,
            color2: self.color2,
            border_width: self.params.map(clean),
            angle: clean(self.time),
            blur: self.kind as u32 as f32,
            corner_radius: radius,
            texture: TextureRef::Material,
            ..UiQuad::fill([rect[0], rect[1]], [rect[2], rect[3]], self.color)
        }
    }
}

/// Longest material time kept before wrapping, in seconds (f32 keeps sub-millisecond steps
/// below it, so long sessions do not stutter).
pub const MATERIAL_TIME_WRAP: f32 = 3600.0;

/// Seconds extract should add to material quads already in the draw list. Cleared by extract.
///
/// [`tick_ui_materials`] writes it. Hosts do not construct it; [`crate::UiPlugin`] inserts it.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct MaterialTick {
    /// Frame delta not yet applied to extracted quads. Zero when this frame did not advance time.
    pub(crate) dt: f32,
}

/// `Update`: advances the time of every animated [`UiMaterial`] whose kind uses time. The write
/// bypasses change detection, so a steady panel is not rebuilt; extract adds that frame's `dt`
/// to the quads it already has. While a time-kind material with `animate: false` exists, the
/// write is a normal one and extract adds nothing (its quads would move too). Static materials
/// are not touched, and with reduced motion on none is.
pub fn tick_ui_materials(
    time: Option<Res<Time>>,
    theme: Option<Res<crate::theme_resolve::ActiveTheme>>,
    access: Option<Res<crate::scale::UiAccessibility>>,
    mut materials: Query<&mut UiMaterial>,
    mut tick: Option<ResMut<MaterialTick>>,
) {
    let reduced = crate::motion::reduced_motion(theme.as_deref(), access.as_deref());
    let dt = match time {
        Some(time) if !reduced => time.delta_secs().max(0.0),
        _ => 0.0,
    };
    // Extract's patch adds the delta to every time-kind material quad: a quad does not say
    // whether its material animates. While a held one (`animate: false`) exists, the patch would
    // move it too, so time is written with change detection instead and the rebuild reads it.
    let held = dt > 0.0 && materials.iter().any(|m| !m.animate && m.kind.uses_time());
    // Store this frame's delta (zero when skipped) so a stale delta is not applied again.
    if let Some(tick) = tick.as_mut() {
        tick.dt = if held { 0.0 } else { dt };
    }
    if dt <= 0.0 {
        return;
    }
    let _span = tracing::trace_span!("ui_materials_tick").entered();
    for mut m in &mut materials {
        if m.animate && m.kind.uses_time() {
            let time = (m.time + dt) % MATERIAL_TIME_WRAP;
            if held {
                m.time = time;
            } else {
                m.bypass_change_detection().time = time;
            }
        }
    }
}

/// CPU model of the shader's `color` → `color2` mix factor for a pixel at `local` (px from the
/// node's top-left) of a `size` node; `None` for [`UiMaterialKind::Dissolve`], whose noise is
/// only defined on the GPU.
pub fn material_mix(m: &UiMaterial, local: [f32; 2], size: [f32; 2]) -> Option<f32> {
    let k = m.params;
    let t = m.time;
    let p = [local[0] - size[0] * 0.5, local[1] - size[1] * 0.5];
    let tau = std::f32::consts::TAU;
    Some(match m.kind {
        UiMaterialKind::Shine => {
            let ang = k[2].to_radians();
            let dir = [ang.cos(), ang.sin()];
            let extent = dir[0].abs() * size[0] + dir[1].abs() * size[1];
            let s = (p[0] * dir[0] + p[1] * dir[1]) / extent.max(1e-4) + 0.5;
            let cycle = (t * k[0]).rem_euclid(1.0) * (1.0 + k[3]);
            let pos = cycle * (1.0 + 2.0 * k[1]) - k[1];
            let f = (1.0 - (s - pos).abs() / k[1].max(1e-3)).clamp(0.0, 1.0);
            f * f * (3.0 - 2.0 * f)
        }
        UiMaterialKind::RadialWipe => {
            if k[0] >= 1.0 {
                1.0
            } else {
                let a = p[0].atan2(-p[1]);
                let frac = (a / tau + 1.0).rem_euclid(1.0);
                ((k[0] - frac) / k[1].max(1e-4) * tau + 0.5).clamp(0.0, 1.0)
            }
        }
        UiMaterialKind::Scanlines => {
            let period = k[0].max(1.0);
            let phase = ((local[1] + t * k[1]) / period).rem_euclid(1.0);
            k[2].clamp(0.0, 1.0) * (0.5 - 0.5 * (phase * tau).cos())
        }
        UiMaterialKind::Pulse => 0.5 - 0.5 * (t * tau / k[0].max(1e-3)).cos(),
        UiMaterialKind::Dissolve => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for k in UiMaterialKind::ALL {
            assert_eq!(UiMaterialKind::from_name(k.name()), Some(k));
        }
        assert_eq!(UiMaterialKind::from_name("lava"), None);
    }

    #[test]
    fn quad_packs_kind_time_and_params() {
        let mut m = UiMaterial::new(UiMaterialKind::RadialWipe);
        m.params = [0.5, f32::NAN, 0.0, 0.0];
        m.time = 2.0;
        let q = m.quad([1.0, 2.0, 30.0, 40.0], [4.0; 4]);
        assert_eq!(q.texture, TextureRef::Material);
        assert_eq!(q.blur, 1.0);
        assert_eq!(q.angle, 2.0);
        assert_eq!(q.border_width, [0.5, 0.0, 0.0, 0.0]);
        let inst = crate::draw::instance_of(&q);
        assert_eq!(inst.kind_layer, [8.0, 0.0, 2.0, 1.0]);
        assert_eq!(inst.border_width, [0.5, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn radial_wipe_fills_clockwise_from_the_top() {
        let mut m = UiMaterial::new(UiMaterialKind::RadialWipe);
        m.params = [0.25, 0.01, 0.0, 0.0];
        let size = [100.0, 100.0];
        // Right of the centre is a quarter turn clockwise from 12 o'clock: the edge.
        let top_right = material_mix(&m, [60.0, 10.0], size).unwrap_or(-1.0);
        let bottom = material_mix(&m, [50.0, 95.0], size).unwrap_or(-1.0);
        let left = material_mix(&m, [5.0, 50.0], size).unwrap_or(-1.0);
        assert_eq!(top_right, 1.0);
        assert_eq!(bottom, 0.0);
        assert_eq!(left, 0.0);
        m.params[0] = 1.0;
        assert_eq!(material_mix(&m, [5.0, 50.0], size), Some(1.0));
    }

    #[test]
    fn pulse_and_scanlines_are_periodic() {
        let mut m = UiMaterial::new(UiMaterialKind::Pulse);
        m.params[0] = 2.0;
        let at = |m: &UiMaterial| material_mix(m, [0.0, 0.0], [10.0, 10.0]).unwrap_or(-1.0);
        assert!(at(&m).abs() < 1e-6);
        m.time = 1.0;
        assert!((at(&m) - 1.0).abs() < 1e-6);
        m.time = 2.0;
        assert!(at(&m).abs() < 1e-5);
        let mut s = UiMaterial::new(UiMaterialKind::Scanlines);
        s.params = [4.0, 0.0, 1.0, 0.0];
        let line = |y: f32| material_mix(&s, [0.0, y], [10.0, 10.0]).unwrap_or(-1.0);
        assert!(line(0.0).abs() < 1e-6 && (line(2.0) - 1.0).abs() < 1e-6);
        assert!((line(4.0) - line(0.0)).abs() < 1e-5);
        assert_eq!(
            material_mix(&UiMaterial::new(UiMaterialKind::Dissolve), [0.0; 2], [1.0; 2]),
            None
        );
    }

    #[test]
    fn shine_band_moves_across() {
        let mut m = UiMaterial::new(UiMaterialKind::Shine);
        m.params = [1.0, 0.2, 0.0, 0.0];
        let size = [100.0, 20.0];
        // At t = 0.5 the band centre is at 0.5 * 1.4 - 0.2 = 0.5 of the width.
        m.time = 0.5;
        let centre = material_mix(&m, [50.0, 10.0], size).unwrap_or(-1.0);
        let edge = material_mix(&m, [5.0, 10.0], size).unwrap_or(-1.0);
        assert!((centre - 1.0).abs() < 1e-5);
        assert_eq!(edge, 0.0);
    }

    #[test]
    fn tick_advances_only_animated_time_kinds() {
        // A bare world and schedule: the app's own clock systems would overwrite the delta.
        let mut world = World::new();
        let mut clock = Time::default();
        clock.advance(std::time::Duration::from_millis(250));
        world.insert_resource(clock);
        let shine = world.spawn(UiMaterial::new(UiMaterialKind::Shine)).id();
        let wipe = world.spawn(UiMaterial::new(UiMaterialKind::RadialWipe)).id();
        let mut paused = UiMaterial::new(UiMaterialKind::Pulse);
        paused.animate = false;
        let paused = world.spawn(paused).id();
        let mut schedule = Schedule::default();
        schedule.add_systems(tick_ui_materials);
        schedule.run(&mut world);
        let time = |e: Entity, world: &World| world.get::<UiMaterial>(e).map_or(-1.0, |m| m.time);
        assert!((time(shine, &world) - 0.25).abs() < 1e-6);
        assert_eq!(time(wipe, &world), 0.0);
        assert_eq!(time(paused, &world), 0.0);
        // Static materials are never touched, so they never mark their node changed.
        world.clear_trackers();
        schedule.run(&mut world);
        assert!((time(shine, &world) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn reduced_motion_holds_an_animated_material() {
        let mut world = World::new();
        let mut clock = Time::default();
        clock.advance(std::time::Duration::from_millis(250));
        world.insert_resource(clock);
        world.insert_resource(crate::scale::UiAccessibility {
            reduced_motion: true,
            ..Default::default()
        });
        let shine = world.spawn(UiMaterial::new(UiMaterialKind::Shine)).id();
        let mut schedule = Schedule::default();
        schedule.add_systems(tick_ui_materials);
        schedule.run(&mut world);
        assert_eq!(world.get::<UiMaterial>(shine).map(|m| m.time), Some(0.0));
        // The setting off again: it moves on from where it stopped.
        world
            .resource_mut::<crate::scale::UiAccessibility>()
            .reduced_motion = false;
        schedule.run(&mut world);
        let moved = world.get::<UiMaterial>(shine).map_or(0.0, |m| m.time);
        assert!((moved - 0.25).abs() < 1e-6, "{moved}");
    }

    fn platform_messages(app: &mut App) {
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
    }

    fn material_angle(app: &App) -> f32 {
        app.world()
            .resource::<crate::render::SharedUiRenderer>()
            .lock()
            .quads()
            .iter()
            .find(|quad| quad.texture == TextureRef::Material)
            .map(|quad| quad.angle)
            .unwrap_or(f32::NAN)
    }

    fn counters(app: &App) -> (u64, u64) {
        let renderer = app.world().resource::<crate::render::SharedUiRenderer>().lock();
        (renderer.quads_generation(), renderer.subtree_walks())
    }

    /// A steady animated material advances its quad time without walking or rebuilding the panel.
    #[test]
    fn animated_material_does_not_rebuild_the_panel() {
        use crate::prelude::*;
        use crate::style::FlexDirection;

        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only());
        platform_messages(&mut app);
        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                }),
            ))
            .id();
        let panel = world
            .spawn((
                UiNode,
                UiMaterial::new(UiMaterialKind::Shine),
                NodeStyle::new(Style {
                    size: [Dimension::Points(200.0), Dimension::Points(80.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(root).add_child(panel);
        for _ in 0..4 {
            app.update();
        }
        let (generation, walks) = counters(&app);
        let time_before = app
            .world()
            .get::<UiMaterial>(panel)
            .map(|m| m.time)
            .unwrap_or(-1.0);
        let angle_before = material_angle(&app);
        assert!(angle_before.is_finite(), "the settled panel has a material quad");

        // A fresh `run_system_once(detect_ui_damage)` sees every existing widget as changed.
        // The scheduled detect, after a fixed 16 ms step, is the one that must stay quiet.
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        app.update();
        assert!(
            !app.world().resource::<crate::extract::UiDamage>().0,
            "the time write must not mark the panel damaged"
        );

        let (generation2, walks2) = counters(&app);
        assert_eq!(
            (generation2, walks2),
            (generation, walks),
            "material time must not rebuild: generation {generation}->{generation2}, walks {walks}->{walks2}"
        );
        let time_after = app
            .world()
            .get::<UiMaterial>(panel)
            .map(|m| m.time)
            .unwrap_or(-1.0);
        let angle_after = material_angle(&app);
        assert!(
            (time_after - time_before - 0.016).abs() < 1e-3,
            "component time {time_before} -> {time_after}"
        );
        assert!(
            (angle_after - angle_before - 0.016).abs() < 1e-3,
            "extracted angle {angle_before} -> {angle_after}"
        );
    }

    /// A root with a column of children: the panel (a Shine material) and a fixed-size label.
    fn panel_and_label(app: &mut App, material: UiMaterial) -> (Entity, Entity) {
        use crate::prelude::*;
        use crate::style::FlexDirection;

        app.add_plugins(UiPlugin::code_only());
        platform_messages(app);
        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    direction: FlexDirection::Column,
                    ..Default::default()
                }),
            ))
            .id();
        let panel = world
            .spawn((
                UiNode,
                material,
                NodeStyle::new(Style {
                    size: [Dimension::Points(200.0), Dimension::Points(80.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let label = world
            .spawn((
                UiNode,
                crate::text::Text::new("0"),
                NodeStyle::new(Style {
                    size: [Dimension::Points(200.0), Dimension::Points(40.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(root).add_child(panel);
        world.entity_mut(root).add_child(label);
        for _ in 0..4 {
            app.update();
        }
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        (panel, label)
    }

    /// Corr-ui-b: material time is written past change detection and only a clean frame added it
    /// to the cached quads, so while another subtree redrew every frame (a clock, an FPS counter)
    /// the cached panel's shine stood still.
    #[test]
    fn animated_material_keeps_moving_while_another_subtree_redraws() {
        let mut app = App::new();
        let (_, label) = panel_and_label(&mut app, UiMaterial::new(UiMaterialKind::Shine));
        app.update();
        let before = material_angle(&app);
        assert!(before.is_finite(), "the panel has a material quad");
        let (_, walks) = counters(&app);
        for i in 1..=5 {
            app.world_mut()
                .entity_mut(label)
                .insert(crate::text::Text::new(format!("{i}")));
            app.update();
        }
        let (_, walks_after) = counters(&app);
        assert!(
            walks_after > walks,
            "the label redraws: walks {walks} -> {walks_after}"
        );
        let after = material_angle(&app);
        assert!(
            (after - before - 0.080).abs() < 1e-3,
            "the shine froze while the label redrew: {before} -> {after}"
        );
    }

    /// Corr-ui-b: extract's clean-frame patch moved every time-kind material quad, so a held
    /// material (`animate: false`, a fixed phase) drifted, then snapped back on the next rebuild.
    #[test]
    fn a_held_material_keeps_its_time_on_clean_frames() {
        let mut app = App::new();
        let (panel, _) = panel_and_label(
            &mut app,
            UiMaterial {
                time: 0.3,
                animate: false,
                ..UiMaterial::new(UiMaterialKind::Shine)
            },
        );
        for _ in 0..3 {
            app.update();
        }
        let angle = material_angle(&app);
        assert!((angle - 0.3).abs() < 1e-6, "a held material moved: {angle}");
        assert_eq!(app.world().get::<UiMaterial>(panel).map(|m| m.time), Some(0.3));
    }

    /// With a held material on screen, an animated one still moves (through the rebuild).
    #[test]
    fn an_animated_material_moves_beside_a_held_one() {
        use crate::prelude::*;

        let mut app = App::new();
        let (panel, _) = panel_and_label(&mut app, UiMaterial::new(UiMaterialKind::Pulse));
        let held = app
            .world_mut()
            .spawn((
                UiNode,
                UiMaterial {
                    time: 0.3,
                    animate: false,
                    ..UiMaterial::new(UiMaterialKind::Shine)
                },
                NodeStyle::new(Style {
                    size: [Dimension::Points(50.0), Dimension::Points(50.0)],
                    ..Default::default()
                }),
            ))
            .id();
        app.world_mut().entity_mut(panel).add_child(held);
        app.update();
        let time = |app: &App| app.world().get::<UiMaterial>(panel).map_or(-1.0, |m| m.time);
        let angles = |app: &App| -> Vec<f32> {
            app.world()
                .resource::<crate::render::SharedUiRenderer>()
                .lock()
                .quads()
                .iter()
                .filter(|quad| quad.texture == TextureRef::Material)
                .map(|quad| quad.angle)
                .collect()
        };
        let t0 = time(&app);
        for _ in 0..3 {
            app.update();
        }
        let t1 = time(&app);
        assert!((t1 - t0 - 0.048).abs() < 1e-3, "{t0} -> {t1}");
        let drawn = angles(&app);
        assert!(
            drawn.iter().any(|a| (a - t1).abs() < 1e-5),
            "animated quad at {t1}: {drawn:?}"
        );
        assert!(
            drawn.iter().any(|a| (a - 0.3).abs() < 1e-6),
            "held quad at 0.3: {drawn:?}"
        );
    }

    /// A `UiMaterial` inside a `ChildOf` cycle that is not under a root must not spin extract.
    /// The cache is filled first: an empty cache forces a full rebuild and never asks which subtree
    /// the material belongs to.
    #[test]
    fn material_inside_a_child_of_cycle_still_extracts() {
        use crate::prelude::*;

        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only());
        platform_messages(&mut app);
        let world = app.world_mut();
        let root = world
            .spawn((
                UiRoot::default(),
                NodeStyle::new(Style {
                    size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                    ..Default::default()
                }),
            ))
            .id();
        let panel = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(40.0), Dimension::Points(40.0)],
                    ..Default::default()
                }),
            ))
            .id();
        world.entity_mut(root).add_child(panel);
        for _ in 0..4 {
            app.update();
        }
        let walks = counters(&app).1;
        let a = app
            .world_mut()
            .spawn((UiNode, UiMaterial::new(UiMaterialKind::Shine)))
            .id();
        let b = app.world_mut().spawn(UiNode).id();
        app.world_mut().entity_mut(a).add_child(b);
        app.world_mut().entity_mut(b).add_child(a);
        // Point extract at the cycle. A full frame also runs ancestor walks this finding does
        // not cap (`layout::resolved_rtl`), so the measured step is extract alone.
        {
            let world = app.world_mut();
            world.resource_mut::<crate::extract::UiDamage>().0 = true;
            let mut set = world.resource_mut::<crate::extract::UiDamageSet>();
            set.full = false;
            set.entities.clear();
            set.entities.push(a);
        }
        crate::extract::extract_ui(app.world_mut());
        assert_eq!(
            counters(&app).1,
            walks,
            "the cycle is not a top-level subtree, so extract does not walk it"
        );
    }
}
