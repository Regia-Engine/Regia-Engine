// SPDX-License-Identifier: MPL-2.0
//! Layout edge cases (ADR-518): seeded property tests over random style trees, plus pinned cases
//! for right-to-left mirroring, flex wrap, grid tracks, min/max, aspect ratio and safe areas.
//!
//! The property tests are deterministic: a fixed-seed `FuzzRng` generates every tree, so a
//! failure prints its case number and the tree, and reproduces on any machine. One app serves
//! all cases of a test (each case despawns its root), so the loops stay cheap.

use regia_app::prelude::*;
use regia_test_utils::fuzz::FuzzRng;
use regia_ui::prelude::*;
use regia_ui::style::{Align, FlexDirection, Overflow, SmallTracks, TextDir, TrackSizing};
use regia_ui::{UiDevicePolicy, UiScales};

/// The window every test lays out in (physical px; the roots use a constant pixel scale of 1).
const WINDOW: [f32; 2] = [1000.0, 600.0];
/// Random trees per property test.
const CASES: usize = 160;

/// An app with `UiPlugin`, the platform messages its systems read, no device text floor or
/// title-safe margin, and a `WINDOW`-sized window.
fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(UiDevicePolicy::DESKTOP);
    app.update();
    app.world_mut().resource_mut::<UiScales>().window_size = WINDOW;
    app
}

fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    app.world().get::<ComputedNode>(e).expect("laid out").rect
}

fn set_locale(app: &mut App, locale: &str) {
    let mut loc = app.world_mut().resource_mut::<Localization>();
    loc.selected = locale.to_owned();
    loc.generation.0 += 1;
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

/// A generated node: its style and children.
#[derive(Debug, Clone)]
struct Spec {
    style: Style,
    children: Vec<Spec>,
}

/// Spawns `spec` as a constant-pixel-size root; returns every entity in pre-order with its depth.
fn spawn_root(world: &mut World, spec: &Spec) -> Vec<(Entity, u32)> {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(spec.style.clone()),
        ))
        .id();
    let mut nodes = vec![(root, 0)];
    spawn_children(world, root, spec, 1, &mut nodes);
    nodes
}

fn spawn_children(
    world: &mut World,
    parent: Entity,
    spec: &Spec,
    depth: u32,
    nodes: &mut Vec<(Entity, u32)>,
) {
    for child in &spec.children {
        let e = world.spawn((UiNode, NodeStyle::new(child.style.clone()))).id();
        world.entity_mut(parent).add_child(e);
        nodes.push((e, depth));
        spawn_children(world, e, child, depth + 1, nodes);
    }
}

/// A fixed-size box child.
fn boxed(world: &mut World, parent: Entity, w: f32, h: f32) -> Entity {
    child(
        world,
        parent,
        Style {
            size: [Dimension::Points(w), Dimension::Points(h)],
            ..Default::default()
        },
    )
}

fn child(world: &mut World, parent: Entity, style: Style) -> Entity {
    let e = world.spawn((UiNode, NodeStyle::new(style))).id();
    world.entity_mut(parent).add_child(e);
    e
}

fn root(world: &mut World, style: Style) -> Entity {
    world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(style),
        ))
        .id()
}

/// Random style trees. `hostile` mixes in values layout must survive (NaN, infinities, negative
/// sizes, 1e30, zero and negative aspect ratios, `display: none`, absolute nodes); without it the
/// trees use only valid in-flow flex and grid values, so their layout must mirror exactly in RTL.
struct Gen {
    rng: FuzzRng,
    hostile: bool,
    budget: usize,
}

impl Gen {
    fn new(seed: u64, hostile: bool) -> Self {
        Self {
            rng: FuzzRng::new(seed),
            hostile,
            budget: 0,
        }
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.rng.below(items.len())]
    }

    fn hostile_f32(&mut self) -> f32 {
        self.pick(&[
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -50.0,
            1.0e30,
            -1.0e30,
            0.0,
            f32::MAX,
        ])
    }

    /// An integer in `lo..=hi` as `f32`, or (hostile, one time in five) a hostile value.
    fn len(&mut self, lo: usize, hi: usize) -> f32 {
        if self.hostile && self.rng.chance(5) {
            self.hostile_f32()
        } else {
            self.rng.range(lo, hi) as f32
        }
    }

    fn size_dim(&mut self) -> Dimension {
        match self.rng.below(8) {
            0 | 1 => Dimension::Auto,
            2 => Dimension::Percent(self.len(10, 100)),
            _ => Dimension::Points(self.len(8, 200)),
        }
    }

    fn opt_dim(&mut self, lo: usize, hi: usize) -> Dimension {
        if self.rng.chance(3) {
            Dimension::Points(self.len(lo, hi))
        } else {
            Dimension::Auto
        }
    }

    fn edge(&mut self, hi: usize) -> Dimension {
        match self.rng.below(6) {
            0 => Dimension::Auto,
            1 => Dimension::Percent(self.len(0, 10)),
            _ => Dimension::Points(self.len(0, hi)),
        }
    }

    fn edges(&mut self, hi: usize) -> Edges {
        Edges {
            left: self.edge(hi),
            top: self.edge(hi),
            right: self.edge(hi),
            bottom: self.edge(hi),
        }
    }

    fn track(&mut self) -> TrackSizing {
        match self.rng.below(4) {
            0 => TrackSizing::Px(self.len(20, 120)),
            1 => TrackSizing::Fr(self.len(1, 3)),
            2 => TrackSizing::Auto,
            _ => TrackSizing::Percent(self.len(10, 40)),
        }
    }

    fn tracks(&mut self) -> SmallTracks {
        let n = self.rng.range(1, 4);
        SmallTracks((0..n).map(|_| self.track()).collect())
    }

    fn style(&mut self, has_children: bool) -> Style {
        let grid = has_children && self.rng.chance(4);
        let mut s = Style {
            display: if grid { Display::Grid } else { Display::Flex },
            direction: self.pick(&[
                FlexDirection::Row,
                FlexDirection::RowReverse,
                FlexDirection::Column,
                FlexDirection::ColumnReverse,
            ]),
            justify_content: self.pick(&[
                JustifyContent::Start,
                JustifyContent::End,
                JustifyContent::Center,
                JustifyContent::SpaceBetween,
                JustifyContent::SpaceAround,
                JustifyContent::SpaceEvenly,
            ]),
            align_items: self.pick(&[
                Align::Auto,
                Align::Start,
                Align::End,
                Align::Center,
                Align::Stretch,
            ]),
            align_self: if self.rng.chance(4) {
                self.pick(&[Align::Start, Align::End, Align::Center, Align::Stretch])
            } else {
                Align::Auto
            },
            margin: self.edges(16),
            padding: self.edges(16),
            border: if self.rng.chance(4) {
                self.edges(4)
            } else {
                Edges::default()
            },
            size: [self.size_dim(), self.size_dim()],
            min_size: [self.opt_dim(0, 80), self.opt_dim(0, 80)],
            max_size: [self.opt_dim(40, 300), self.opt_dim(40, 300)],
            overflow: if self.rng.chance(5) {
                [Overflow::Clip; 2]
            } else {
                [Overflow::Visible; 2]
            },
            flex_wrap: self.rng.chance(3),
            flex_grow: if self.rng.chance(3) { self.len(1, 3) } else { 0.0 },
            flex_shrink: if self.rng.chance(2) { 1.0 } else { 0.0 },
            flex_basis: if self.rng.chance(5) {
                Dimension::Points(self.len(10, 120))
            } else {
                Dimension::Auto
            },
            gap: [
                Dimension::Points(self.len(0, 16)),
                Dimension::Points(self.len(0, 16)),
            ],
            grid_columns: if grid { self.tracks() } else { SmallTracks::empty() },
            grid_rows: if grid && self.rng.chance(3) {
                self.tracks()
            } else {
                SmallTracks::empty()
            },
            ..Style::default()
        };
        if self.rng.chance(8) {
            s.aspect_ratio = Some(self.pick(&[0.5, 1.0, 4.0 / 3.0, 16.0 / 9.0, 2.0]));
        }
        if self.hostile {
            if self.rng.chance(10) {
                s.display = Display::None;
            }
            if self.rng.chance(8) {
                s.position = PositionType::Absolute;
                s.inset = self.edges(100);
            }
            if self.rng.chance(6) {
                s.aspect_ratio = Some(self.hostile_f32());
            }
            if self.rng.chance(6) {
                s.flex_grow = self.hostile_f32();
            }
            if self.rng.chance(6) {
                s.flex_shrink = self.hostile_f32();
            }
        }
        s
    }

    fn tree(&mut self, depth: u32) -> Spec {
        let kids = if depth >= 3 {
            0
        } else {
            self.rng.below(5).min(self.budget)
        };
        self.budget -= kids;
        let children = (0..kids).map(|_| self.tree(depth + 1)).collect();
        Spec {
            style: self.style(kids > 0),
            children,
        }
    }

    /// One random tree of at most 31 nodes. The root fills the window with no margin and no
    /// min/max (a root is placed by the window, not by a parent, so nothing mirrors it).
    fn case(&mut self) -> Spec {
        self.budget = 30;
        let mut spec = self.tree(0);
        spec.style.margin = Edges::default();
        spec.style.min_size = [Dimension::Auto; 2];
        spec.style.max_size = [Dimension::Auto; 2];
        spec.style.aspect_ratio = None;
        spec.style.position = PositionType::Relative;
        if spec.style.display == Display::None {
            spec.style.display = Display::Flex;
        }
        spec
    }
}

#[test]
fn random_hostile_trees_lay_out_finite_non_negative_rects() {
    let mut app = ui_app();
    let mut g = Gen::new(0x1A70_0518, true);
    for case in 0..CASES {
        let spec = g.case();
        let nodes = spawn_root(app.world_mut(), &spec);
        // Some cases also opt into the safe area, with insets up to far beyond the window.
        if g.rng.chance(3) {
            let mut inset = || g.pick(&[0u32, 12, 300, 900, 5_000, u32::MAX]);
            let safe = regia_platform::SafeArea {
                left: inset(),
                top: inset(),
                right: inset(),
                bottom: inset(),
            };
            app.insert_resource(safe);
            app.world_mut().entity_mut(nodes[0].0).insert(SafeAreaPadding);
        } else {
            app.insert_resource(regia_platform::SafeArea::default());
        }
        settle(&mut app);
        for (i, (e, _)) in nodes.iter().enumerate() {
            let Some(c) = app.world().get::<ComputedNode>(*e) else {
                panic!("case {case}: node {i} was not laid out\n{spec:#?}");
            };
            let finite = c
                .rect
                .iter()
                .chain(&c.clip_rect)
                .chain(&c.content_size)
                .all(|v| v.is_finite());
            let non_negative =
                c.rect[2] >= 0.0 && c.rect[3] >= 0.0 && c.content_size[0] >= 0.0 && c.content_size[1] >= 0.0;
            assert!(finite && non_negative, "case {case}: node {i} {c:?}\n{spec:#?}");
        }
        app.world_mut().entity_mut(nodes[0].0).despawn();
    }
}

#[test]
fn an_rtl_locale_mirrors_random_flex_and_grid_trees() {
    let mut app = ui_app();
    let mut g = Gen::new(0x2B17_0518, false);
    for case in 0..CASES {
        let spec = g.case();
        set_locale(&mut app, "en-US");
        let nodes = spawn_root(app.world_mut(), &spec);
        settle(&mut app);
        let ltr: Vec<[f32; 4]> = nodes.iter().map(|(e, _)| rect(&app, *e)).collect();
        set_locale(&mut app, "ar");
        settle(&mut app);
        for (i, ((e, depth), l)) in nodes.iter().zip(&ltr).enumerate() {
            let r = rect(&app, *e);
            // Taffy rounds each node's position relative to its parent, so the absolute error of
            // a mirrored rect grows by at most a pixel per level.
            let tol = 1.01 + *depth as f32;
            let mirrored_x = WINDOW[0] - l[0] - l[2];
            let ok = (r[0] - mirrored_x).abs() <= tol
                && (r[1] - l[1]).abs() <= tol
                && (r[2] - l[2]).abs() <= tol
                && (r[3] - l[3]).abs() <= tol;
            assert!(
                ok,
                "case {case}: node {i} (depth {depth}) ltr {l:?} rtl {r:?}, expected x {mirrored_x}\n{spec:#?}"
            );
        }
        app.world_mut().entity_mut(nodes[0].0).despawn();
    }
}

#[test]
fn min_wins_over_max_and_both_clamp_the_size_on_either_axis() {
    let mut app = ui_app();
    let mut rng = FuzzRng::new(0x3C1A_0518);
    let r = root(
        app.world_mut(),
        Style {
            direction: FlexDirection::Column,
            align_items: Align::Start,
            ..Default::default()
        },
    );
    for case in 0..CASES {
        let [size, min, max] = [rng.range(0, 400), rng.range(0, 280), rng.range(0, 280)].map(|v| v as f32);
        let expected = size.min(max).max(min);
        // Horizontal: the cross axis of the column. Vertical: its main axis.
        let wide = child(
            app.world_mut(),
            r,
            Style {
                size: [Dimension::Points(size), Dimension::Points(10.0)],
                min_size: [Dimension::Points(min), Dimension::Auto],
                max_size: [Dimension::Points(max), Dimension::Auto],
                ..Default::default()
            },
        );
        let tall = child(
            app.world_mut(),
            r,
            Style {
                size: [Dimension::Points(10.0), Dimension::Points(size)],
                min_size: [Dimension::Auto, Dimension::Points(min)],
                max_size: [Dimension::Auto, Dimension::Points(max)],
                ..Default::default()
            },
        );
        settle(&mut app);
        let (w, t) = (rect(&app, wide), rect(&app, tall));
        assert!(
            near(w[2], expected) && near(t[3], expected),
            "case {case}: size {size} min {min} max {max}: width {} height {} (expected {expected})",
            w[2],
            t[3]
        );
        app.world_mut().entity_mut(wide).despawn();
        app.world_mut().entity_mut(tall).despawn();
    }
}

#[test]
fn aspect_ratio_derives_the_missing_axis_and_ignores_unusable_ratios() {
    let mut app = ui_app();
    let r = root(
        app.world_mut(),
        Style {
            direction: FlexDirection::Column,
            align_items: Align::Start,
            ..Default::default()
        },
    );
    let from_width = child(
        app.world_mut(),
        r,
        Style {
            size: [Dimension::Points(320.0), Dimension::Auto],
            aspect_ratio: Some(16.0 / 9.0),
            ..Default::default()
        },
    );
    let from_height = child(
        app.world_mut(),
        r,
        Style {
            size: [Dimension::Auto, Dimension::Points(90.0)],
            aspect_ratio: Some(2.0),
            ..Default::default()
        },
    );
    // Unusable ratios are ignored rather than producing NaN or infinite boxes.
    let zero = child(
        app.world_mut(),
        r,
        Style {
            size: [Dimension::Points(40.0), Dimension::Auto],
            aspect_ratio: Some(0.0),
            ..Default::default()
        },
    );
    let nan = child(
        app.world_mut(),
        r,
        Style {
            size: [Dimension::Points(40.0), Dimension::Auto],
            aspect_ratio: Some(f32::NAN),
            ..Default::default()
        },
    );
    settle(&mut app);
    let w = rect(&app, from_width);
    assert!(near(w[2], 320.0) && near(w[3], 180.0), "{w:?}");
    let h = rect(&app, from_height);
    assert!(near(h[2], 180.0) && near(h[3], 90.0), "{h:?}");
    for e in [zero, nan] {
        let z = rect(&app, e);
        assert!(near(z[2], 40.0) && near(z[3], 0.0), "{z:?}");
    }
}

#[test]
fn wrapping_rows_break_greedily_and_never_overlap() {
    let mut app = ui_app();
    let mut rng = FuzzRng::new(0x4D0F_0518);
    for case in 0..CASES {
        let width = rng.range(120, 600) as f32;
        let pad = rng.range(0, 20) as f32;
        let [gap_x, gap_y] = [rng.range(0, 16), rng.range(0, 16)].map(|v| v as f32);
        let r = root(
            app.world_mut(),
            Style {
                align_items: Align::Start,
                ..Default::default()
            },
        );
        let container = child(
            app.world_mut(),
            r,
            Style {
                direction: FlexDirection::Row,
                flex_wrap: true,
                align_items: Align::Start,
                size: [Dimension::Points(width), Dimension::Auto],
                padding: Edges::px(pad),
                gap: [Dimension::Points(gap_x), Dimension::Points(gap_y)],
                ..Default::default()
            },
        );
        let n = rng.range(1, 14);
        let kids: Vec<Entity> = (0..n)
            .map(|_| {
                let (w, h) = (rng.range(10, 160) as f32, rng.range(10, 60) as f32);
                boxed(app.world_mut(), container, w, h)
            })
            .collect();
        settle(&mut app);
        let c = rect(&app, container);
        let (left, right) = (c[0] + pad, c[0] + c[2] - pad);
        let mut line_top = c[1] + pad;
        let mut line_bottom = line_top;
        let mut prev: Option<[f32; 4]> = None;
        for (i, e) in kids.iter().enumerate() {
            let k = rect(&app, *e);
            let what = format!("case {case}: child {i} {k:?} in {c:?} (pad {pad}, gaps {gap_x}/{gap_y})");
            match prev {
                Some(p) if near(k[1], line_top) => {
                    // Same line: after the previous child plus the gap, inside the content box.
                    assert!(near(k[0], p[0] + p[2] + gap_x), "{what}: not packed after {p:?}");
                    assert!(k[0] + k[2] <= right + 0.5, "{what}: overflows the line");
                }
                Some(p) => {
                    // New line: below the previous one, at the start, and the child did not fit.
                    assert!(
                        k[1] >= line_bottom + gap_y - 0.5,
                        "{what}: overlaps the line above"
                    );
                    assert!(
                        near(k[0], left),
                        "{what}: line does not start at the content edge"
                    );
                    assert!(
                        p[0] + p[2] + gap_x + k[2] > right + 0.5,
                        "{what}: wrapped though it fit"
                    );
                    line_top = k[1];
                    line_bottom = line_top;
                }
                None => {
                    assert!(
                        near(k[0], left) && near(k[1], line_top),
                        "{what}: first child misplaced"
                    );
                }
            }
            line_bottom = line_bottom.max(k[1] + k[3]);
            prev = Some(k);
        }
        app.world_mut().entity_mut(r).despawn();
    }
}

#[test]
fn grid_auto_placement_fills_px_tracks_row_by_row() {
    let mut app = ui_app();
    let mut rng = FuzzRng::new(0x5E9D_0518);
    for case in 0..CASES {
        let cols: Vec<f32> = (0..rng.range(1, 5)).map(|_| rng.range(20, 150) as f32).collect();
        let [gap_x, gap_y] = [rng.range(0, 12), rng.range(0, 12)].map(|v| v as f32);
        let pad = rng.range(0, 16) as f32;
        let cell_h = rng.range(10, 50) as f32;
        // The grid is content-sized (an auto height), so `align-content: normal` has no free
        // space to stretch the implicit auto rows with.
        let r = root(
            app.world_mut(),
            Style {
                direction: FlexDirection::Column,
                align_items: Align::Start,
                ..Default::default()
            },
        );
        let grid = child(
            app.world_mut(),
            r,
            Style {
                display: Display::Grid,
                padding: Edges::px(pad),
                gap: [Dimension::Points(gap_x), Dimension::Points(gap_y)],
                grid_columns: SmallTracks(cols.iter().map(|w| TrackSizing::Px(*w)).collect()),
                ..Default::default()
            },
        );
        let n = rng.range(1, 12);
        let kids: Vec<Entity> = (0..n)
            .map(|_| {
                child(
                    app.world_mut(),
                    grid,
                    Style {
                        size: [Dimension::Auto, Dimension::Points(cell_h)],
                        ..Default::default()
                    },
                )
            })
            .collect();
        settle(&mut app);
        for (i, e) in kids.iter().enumerate() {
            let (col, row) = (i % cols.len(), i / cols.len());
            let x = pad + cols[..col].iter().sum::<f32>() + col as f32 * gap_x;
            let y = pad + row as f32 * (cell_h + gap_y);
            let k = rect(&app, *e);
            assert!(
                near(k[0], x) && near(k[1], y) && near(k[2], cols[col]),
                "case {case}: child {i} {k:?}, expected x {x} y {y} w {} (cols {cols:?})",
                cols[col]
            );
        }
        app.world_mut().entity_mut(r).despawn();
    }
}

#[test]
fn a_percent_grid_track_stays_fixed_when_its_content_is_wider() {
    let mut app = ui_app();
    let r = root(
        app.world_mut(),
        Style {
            display: Display::Grid,
            grid_columns: SmallTracks(vec![TrackSizing::Percent(50.0), TrackSizing::Fr(1.0)]),
            ..Default::default()
        },
    );
    let _wide = boxed(app.world_mut(), r, 800.0, 20.0);
    let second = boxed(app.world_mut(), r, 10.0, 20.0);
    settle(&mut app);
    // CSS `grid-template-columns: 50% 1fr`: the first track is 500 px whatever its content.
    let s = rect(&app, second);
    assert!(near(s[0], WINDOW[0] * 0.5), "{s:?}");
}

#[test]
fn justify_start_packs_a_reversed_row_from_its_first_item() {
    let mut app = ui_app();
    for (text_dir, first_right) in [(TextDir::Ltr, true), (TextDir::Rtl, false)] {
        let r = root(
            app.world_mut(),
            Style {
                direction: FlexDirection::RowReverse,
                align_items: Align::Start,
                text_dir,
                ..Default::default()
            },
        );
        let a = boxed(app.world_mut(), r, 100.0, 20.0);
        let b = boxed(app.world_mut(), r, 60.0, 20.0);
        settle(&mut app);
        let (ra, rb) = (rect(&app, a), rect(&app, b));
        if first_right {
            // LTR row-reverse: the first item hugs the right edge, the second sits to its left.
            assert!(
                near(ra[0] + ra[2], WINDOW[0]) && near(rb[0] + rb[2], ra[0]),
                "{ra:?} {rb:?}"
            );
        } else {
            // RTL row-reverse runs left to right: the first item hugs the left edge.
            assert!(near(ra[0], 0.0) && near(rb[0], ra[2]), "{ra:?} {rb:?}");
        }
        app.world_mut().entity_mut(r).despawn();
    }
}

#[test]
fn auto_text_dir_inherits_from_the_parent_and_ltr_islands_stay_ltr() {
    let mut app = ui_app();
    let r = root(
        app.world_mut(),
        Style {
            direction: FlexDirection::Column,
            align_items: Align::Start,
            text_dir: TextDir::Rtl,
            ..Default::default()
        },
    );
    let row = |text_dir| Style {
        size: [Dimension::Points(400.0), Dimension::Points(40.0)],
        text_dir,
        ..Default::default()
    };
    // An Auto row inside an Rtl root, in an LTR locale: it inherits RTL.
    let inherits = child(app.world_mut(), r, row(TextDir::Auto));
    let a = boxed(app.world_mut(), inherits, 50.0, 20.0);
    // An explicit Ltr island inside the same root.
    let island = child(app.world_mut(), r, row(TextDir::Ltr));
    let b = boxed(app.world_mut(), island, 50.0, 20.0);
    settle(&mut app);
    // The rows themselves start at the root's right edge (column cross-start in RTL).
    let (ri, rl) = (rect(&app, inherits), rect(&app, island));
    assert!(
        near(ri[0], WINDOW[0] - 400.0) && near(rl[0], WINDOW[0] - 400.0),
        "{ri:?} {rl:?}"
    );
    let (ra, rb) = (rect(&app, a), rect(&app, b));
    assert!(
        near(ra[0] + ra[2], WINDOW[0]),
        "inherited RTL hugs the row's right edge: {ra:?}"
    );
    assert!(near(rb[0], rl[0]), "the Ltr island keeps its left start: {rb:?}");
}

#[test]
fn flipping_a_parent_direction_relays_out_unchanged_descendants() {
    let mut app = ui_app();
    let r = root(
        app.world_mut(),
        Style {
            align_items: Align::Start,
            text_dir: TextDir::Ltr,
            ..Default::default()
        },
    );
    let panel = child(
        app.world_mut(),
        r,
        Style {
            size: [Dimension::Points(300.0), Dimension::Points(50.0)],
            ..Default::default()
        },
    );
    let leaf = boxed(app.world_mut(), panel, 40.0, 20.0);
    settle(&mut app);
    assert!(near(rect(&app, leaf)[0], 0.0));
    // Only the root's style changes: an incremental relayout, not a full one. The panel and the
    // leaf are Auto, so they inherit the new direction and must be rebuilt too.
    app.world_mut()
        .get_mut::<NodeStyle>(r)
        .expect("root style")
        .0
        .text_dir = TextDir::Rtl;
    settle(&mut app);
    let (p, l) = (rect(&app, panel), rect(&app, leaf));
    assert!(near(p[0], WINDOW[0] - 300.0), "panel at the right: {p:?}");
    assert!(
        near(l[0] + l[2], WINDOW[0]),
        "leaf at the panel's right edge: {l:?}"
    );
    // And back.
    app.world_mut()
        .get_mut::<NodeStyle>(r)
        .expect("root style")
        .0
        .text_dir = TextDir::Ltr;
    settle(&mut app);
    assert!(near(rect(&app, leaf)[0], 0.0));
}

#[test]
fn rtl_grid_columns_and_absolute_insets_mirror() {
    let mut app = ui_app();
    let r = root(
        app.world_mut(),
        Style {
            display: Display::Grid,
            grid_columns: SmallTracks(vec![TrackSizing::Px(100.0), TrackSizing::Px(200.0)]),
            text_dir: TextDir::Rtl,
            ..Default::default()
        },
    );
    let first = child(app.world_mut(), r, Style::default());
    let second = child(app.world_mut(), r, Style::default());
    // `inset.left` is the start inset: 10 px from the right edge in RTL.
    let pinned = child(
        app.world_mut(),
        r,
        Style {
            position: PositionType::Absolute,
            inset: Edges {
                left: Dimension::Points(10.0),
                top: Dimension::Points(5.0),
                ..Default::default()
            },
            size: [Dimension::Points(30.0), Dimension::Points(30.0)],
            ..Default::default()
        },
    );
    settle(&mut app);
    let (f, s, p) = (rect(&app, first), rect(&app, second), rect(&app, pinned));
    assert!(
        near(f[0], WINDOW[0] - 100.0) && near(f[2], 100.0),
        "first column at the right: {f:?}"
    );
    assert!(
        near(s[0], WINDOW[0] - 300.0) && near(s[2], 200.0),
        "second column to its left: {s:?}"
    );
    assert!(near(p[0], WINDOW[0] - 40.0) && near(p[1], 5.0), "{p:?}");
}

/// A 20 px wide column whose stretched children have margins wider than the column (34 px): one
/// with no padding (zero width), one with 6 px of start padding (6 px wide). Returns
/// `(column, bare, padded)`.
fn overconstrained_column(app: &mut App, direction: FlexDirection) -> (Entity, Entity, Entity) {
    let r = root(app.world_mut(), Style::default());
    let column = child(
        app.world_mut(),
        r,
        Style {
            direction,
            align_items: Align::Stretch,
            size: [Dimension::Points(20.0), Dimension::Points(100.0)],
            ..Default::default()
        },
    );
    let margin = Edges {
        left: Dimension::Points(4.0),
        right: Dimension::Points(30.0),
        ..Default::default()
    };
    let bare = child(
        app.world_mut(),
        column,
        Style {
            margin,
            size: [Dimension::Auto, Dimension::Points(10.0)],
            ..Default::default()
        },
    );
    let padded = child(
        app.world_mut(),
        column,
        Style {
            margin,
            padding: Edges {
                left: Dimension::Points(6.0),
                ..Default::default()
            },
            size: [Dimension::Auto, Dimension::Points(10.0)],
            ..Default::default()
        },
    );
    (column, bare, padded)
}

#[test]
fn an_overconstrained_stretched_item_mirrors_in_an_rtl_column() {
    // The stretched width is the column's 20 px minus 34 px of margins. LTR keeps the start
    // margin (x = 4) and overflows to the right; RTL must keep the start margin on the right
    // (right edge at 1000 - 4) and overflow to the left. taffy 0.14 let the stretched width go
    // negative and placed the RTL item from it: x = 1010 for both (ADR-1411).
    let mut app = ui_app();
    let (_, bare, padded) = overconstrained_column(&mut app, FlexDirection::Column);
    set_locale(&mut app, "en-US");
    settle(&mut app);
    let (b, p) = (rect(&app, bare), rect(&app, padded));
    assert!(near(b[0], 4.0) && near(b[2], 0.0), "LTR bare: {b:?}");
    assert!(near(p[0], 4.0) && near(p[2], 6.0), "LTR padded: {p:?}");
    set_locale(&mut app, "ar");
    settle(&mut app);
    let (rb, rp) = (rect(&app, bare), rect(&app, padded));
    assert!(
        near(rb[0], WINDOW[0] - b[0] - b[2]) && near(rb[2], b[2]),
        "RTL bare mirrors {b:?}: {rb:?}"
    );
    assert!(
        near(rp[0], WINDOW[0] - p[0] - p[2]) && near(rp[2], p[2]),
        "RTL padded mirrors {p:?}: {rp:?}"
    );
}

#[test]
fn a_row_turned_column_rebuilds_its_unchanged_children_for_the_new_cross_axis() {
    // Only the container's style changes, so an incremental run keeps the children's cached
    // nodes unless the moved cross axis forces them to be rebuilt.
    let mut app = ui_app();
    set_locale(&mut app, "ar");
    let (column, bare, padded) = overconstrained_column(&mut app, FlexDirection::Row);
    settle(&mut app);
    app.world_mut()
        .get_mut::<NodeStyle>(column)
        .expect("column style")
        .0
        .direction = FlexDirection::Column;
    settle(&mut app);
    let (rb, rp) = (rect(&app, bare), rect(&app, padded));
    assert!(near(rb[0], WINDOW[0] - 4.0) && near(rb[2], 0.0), "bare: {rb:?}");
    assert!(
        near(rp[0], WINDOW[0] - 4.0 - 6.0) && near(rp[2], 6.0),
        "padded: {rp:?}"
    );
}

#[test]
fn safe_area_insets_stay_physical_in_rtl_and_add_to_percent_padding() {
    let mut app = ui_app();
    app.insert_resource(regia_platform::SafeArea {
        left: 50,
        top: 10,
        right: 20,
        bottom: 0,
    });
    let r = root(
        app.world_mut(),
        Style {
            align_items: Align::Stretch,
            padding: Edges {
                left: Dimension::Percent(10.0),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    app.world_mut().entity_mut(r).insert(SafeAreaPadding);
    let fill = child(
        app.world_mut(),
        r,
        Style {
            flex_grow: 1.0,
            ..Default::default()
        },
    );
    settle(&mut app);
    // LTR: 10 % of the window width plus the 50 px left inset; the 20 px right inset.
    let f = rect(&app, fill);
    assert!(near(f[0], 100.0 + 50.0), "{f:?}");
    assert!(near(f[0] + f[2], WINDOW[0] - 20.0), "{f:?}");
    assert!(near(f[1], 10.0), "{f:?}");
    // RTL: the authored start padding moves to the right; the notch insets do not move.
    set_locale(&mut app, "he-IL");
    settle(&mut app);
    let f = rect(&app, fill);
    assert!(near(f[0], 50.0), "{f:?}");
    assert!(near(f[0] + f[2], WINDOW[0] - 100.0 - 20.0), "{f:?}");
}

#[test]
fn a_safe_area_wider_than_the_window_does_not_grow_the_root() {
    let mut app = ui_app();
    app.insert_resource(regia_platform::SafeArea {
        left: 3_000,
        top: 0,
        right: 1_000,
        bottom: u32::MAX,
    });
    let r = root(app.world_mut(), Style::default());
    app.world_mut().entity_mut(r).insert(SafeAreaPadding);
    let b = boxed(app.world_mut(), r, 10.0, 10.0);
    settle(&mut app);
    let rr = rect(&app, r);
    assert!(
        near(rr[2], WINDOW[0]) && near(rr[3], WINDOW[1]),
        "root keeps the window size: {rr:?}"
    );
    // The horizontal insets keep their 3:1 proportion inside the window.
    let k = rect(&app, b);
    assert!(near(k[0], 750.0), "{k:?}");
    assert!(k.iter().all(|v| v.is_finite()), "{k:?}");
}

/// Review rev-kimi-20 finding 6 (probe): an entity parented under a laid-out UI node in one frame
/// that only gets `(UiNode, NodeStyle)` in a later frame (a scene or prefab instantiates the
/// hierarchy first) is laid out, in place, by the incremental run of that later frame.
#[test]
fn a_ui_node_added_to_an_entity_parented_earlier_is_laid_out() {
    let mut app = ui_app();
    let world = app.world_mut();
    let r = root(
        world,
        Style {
            direction: FlexDirection::Column,
            ..Default::default()
        },
    );
    let first = boxed(world, r, 100.0, 30.0);
    settle(&mut app);
    // Frame N: parented, not a UI node yet; a grandchild comes with it.
    let late = app.world_mut().spawn(ChildOf(r)).id();
    let grandchild = app.world_mut().spawn(ChildOf(late)).id();
    settle(&mut app);
    // Frame N+1 and later: the components arrive.
    app.world_mut().entity_mut(late).insert((
        UiNode,
        NodeStyle::new(Style {
            size: [Dimension::Points(50.0), Dimension::Points(20.0)],
            ..Default::default()
        }),
    ));
    app.world_mut().entity_mut(grandchild).insert((
        UiNode,
        NodeStyle::new(Style {
            size: [Dimension::Points(10.0), Dimension::Points(5.0)],
            ..Default::default()
        }),
    ));
    settle(&mut app);
    assert_eq!(rect(&app, first), [0.0, 0.0, 100.0, 30.0]);
    assert_eq!(
        rect(&app, late),
        [0.0, 30.0, 50.0, 20.0],
        "the late node is laid out below the first"
    );
    assert_eq!(rect(&app, grandchild), [0.0, 30.0, 10.0, 5.0]);
}
