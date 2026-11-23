// SPDX-License-Identifier: MPL-2.0
//! Accessibility audit of a live UI screen (ADR-3025): finds defects in what a game actually put on
//! screen, where `ui.theme_lint` checks a theme's token pairs and `ui.nav_lint` the focus graph alone.
//!
//! [`audit_ui`] gathers an [`AuditScene`] from the world (the computed layout, the accessibility tree
//! from [`build_a11y_tree`], each node's resolved [`Themed`] look, and the focus graph's own
//! `NavGraph::lint`), and [`audit_scene`] is a pure function from that scene to an [`AuditReport`].
//! The findings come in a stable order and carry a node path that names no entity, so two runs over
//! the same screen print the same bytes ([`AuditReport::to_lines`]).
//!
//! Rules ([`AuditRule`]), each citing a WCAG 2.2 success criterion by number:
//!
//! - `a11y_unnamed_interactive` (4.1.2): a focusable or pressable node with no accessible name.
//! - `a11y_text_contrast` (1.4.3): text against its effective background, 4.5:1, or 3:1 for large
//!   text. The background comes from a walk down the paint order through solid and translucent
//!   fills, as `extract_ui` paints them, not just up the node's ancestors: a panel that is a
//!   sibling behind the text counts. A background that is an image, or that nothing opaque backs,
//!   gets severity `info` and "needs eyes", never a pass.
//! - `a11y_nontext_contrast` (1.4.11): the focus ring and a control's boundary, 3:1.
//! - `a11y_target_size` (2.5.8): at least 24 UI px per side; the touch profile raises it to the
//!   theme's `size.touch-min`, measured in device-independent px (physical px / the OS scale
//!   factor, ADR-4932). The criterion's exceptions are named when used ([`TargetExemption`]).
//! - `a11y_focus_unreachable` (2.4.3): from `NavGraph::lint`, reused, plus a control that takes
//!   clicks but cannot take focus.
//! - `a11y_no_focus_indicator` (2.4.7): a focusable node that would draw no ring.
//! - `a11y_text_clipped_at_scale` (1.4.4): text clipped, cut short or pushed off screen at 200 % text
//!   scale. This lays the screen out in a scratch world with the real [`run_ui_layout`], so nothing
//!   in the live UI moves.
//! - `a11y_motion_ignores_setting` (no level AA criterion: this is the engine's own reduced-motion
//!   contract, spec `ui-default-design` § 2.6): a tween that says it follows reduced motion but would
//!   still animate a transform, or fade past `motion.fast`.
//!
//! The audit runs on request only: no system is added to the frame, so it costs nothing until it is
//! called. Contrast is computed by [`contrast_ratio`], the same function the theme lint uses.
//!
//! Agent method: `ui.a11y_audit` (book `agents/ui-a11y-audit.md`).

use std::collections::{HashMap, HashSet};

use regia_app::prelude::*;

use crate::a11y::{A11yNode, A11yRole, Accessible, MAX_A11Y_DEPTH, build_a11y_tree};
use crate::captions::{CaptionBlock, CaptionRow};
use crate::color::UiColor;
use crate::default_widgets::{RadioButton, SpinBox};
use crate::focus::{DEFAULT_FOCUS_RING_WIDTH, FocusRingPolicy, Focusable, focus_ring_geometry};
use crate::focus_graph::{NavIssueKind, world_defaults, world_graph};
use crate::group_layer::RoundedClip;
use crate::interaction::TouchTarget;
use crate::layout::{LayoutDirty, run_ui_layout};
use crate::localization::Localization;
use crate::motion::{MotionTokens, UiMotion, reduced_motion};
use crate::node::{
    Background, BackgroundKind, Border, ClipChildren, ComputedNode, Interactable, NodeName, NodeStyle,
    Opacity, UiCanvas, UiImage, UiNode, UiRoot, ZOffset,
};
use crate::scale::{ScaleMode, UiAccessibility, UiScales, resolve_window_size};
use crate::scroll::{ListView, ScrollView};
use crate::style::{Dimension, Display, Overflow};
use crate::text::{Text, TextContent, TextOverflow, TextSystem};
use crate::theme::Themed;
use crate::theme_color_ops::contrast_ratio;
use crate::theme_resolve::{ActiveTheme, ThemeResolver};
use crate::tween::{TweenPlan, TweenProperty, UiTween};
use crate::ui_material::UiMaterial;
use crate::widgets::{Dropdown, ProgressBar, Slider, TextInput, Toggle};

/// The fewest UI px a target may measure per side (WCAG 2.2 SC 2.5.8).
pub const DEFAULT_MIN_TARGET_PX: f32 = 24.0;
/// The touch profile's minimum when the theme defines no `size.touch-min`.
pub const TOUCH_MIN_TARGET_PX: f32 = 44.0;
/// WCAG 2.2 SC 1.4.3: body text.
pub const TEXT_CONTRAST: f32 = crate::theme_lint::TEXT_CONTRAST;
/// WCAG 2.2 SC 1.4.3: large text; SC 1.4.11: focus ring and control boundary.
pub const NONTEXT_CONTRAST: f32 = crate::theme_lint::BOUNDARY_CONTRAST;
/// "Large scale" text in UI px: 18 pt at 1.333 px per point.
pub const LARGE_TEXT_PX: f32 = 24.0;
/// "Large scale" bold text in UI px: 14 pt at 1.333 px per point.
pub const LARGE_BOLD_TEXT_PX: f32 = 18.66;
/// The text scale the clipping rule lays out at by default (WCAG 2.2 SC 1.4.4).
pub const AUDIT_TEXT_SCALE: f32 = 2.0;
/// The most visible nodes one audit walks; past it the report says `truncated`.
pub const MAX_AUDIT_NODES: usize = 32_768;

/// How far text may pass a box before it counts as overflow, in physical px: layout rounds a box
/// sized to its text to whole pixels.
const OVERFLOW_SLACK_PX: f32 = 1.5;
/// A layer covering less of a rect than this fraction is ignored (a divider touching a label).
const IGNORE_FRACTION: f32 = 0.02;
/// A layer covering at least this fraction of a rect counts as covering it (rounded corners).
const COVER_FRACTION: f32 = 0.98;
/// Stops a gradient is sampled at, for the worst contrast along it.
const GRADIENT_SAMPLES: usize = 9;
/// A color alpha at or above this is opaque.
const OPAQUE_ALPHA: f32 = 0.999;
/// A color alpha at or below this draws nothing.
const CLEAR_ALPHA: f32 = 0.001;

/// One audit rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AuditRule {
    /// A focusable or pressable node with no accessible name.
    UnnamedInteractive,
    /// Text against its effective background.
    TextContrast,
    /// The focus ring and a control's boundary against what is next to them.
    NontextContrast,
    /// A target smaller than the minimum.
    TargetSize,
    /// A node keyboard or gamepad navigation cannot reach.
    FocusUnreachable,
    /// A focusable node that draws no focus indicator.
    NoFocusIndicator,
    /// Text lost when the text scale is raised.
    TextClippedAtScale,
    /// An animation that runs with reduced motion on.
    MotionIgnoresSetting,
}

impl AuditRule {
    /// Every rule, in report order.
    pub const ALL: [Self; 8] = [
        Self::UnnamedInteractive,
        Self::TextContrast,
        Self::NontextContrast,
        Self::TargetSize,
        Self::FocusUnreachable,
        Self::NoFocusIndicator,
        Self::TextClippedAtScale,
        Self::MotionIgnoresSetting,
    ];

    /// The stable rule code.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnnamedInteractive => "a11y_unnamed_interactive",
            Self::TextContrast => "a11y_text_contrast",
            Self::NontextContrast => "a11y_nontext_contrast",
            Self::TargetSize => "a11y_target_size",
            Self::FocusUnreachable => "a11y_focus_unreachable",
            Self::NoFocusIndicator => "a11y_no_focus_indicator",
            Self::TextClippedAtScale => "a11y_text_clipped_at_scale",
            Self::MotionIgnoresSetting => "a11y_motion_ignores_setting",
        }
    }

    /// Parses [`Self::code`].
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.code() == code)
    }

    /// The WCAG 2.2 success criterion number; `engine` for a rule that is the engine's own contract.
    pub fn criterion(self) -> &'static str {
        match self {
            Self::UnnamedInteractive => "4.1.2",
            Self::TextContrast => "1.4.3",
            Self::NontextContrast => "1.4.11",
            Self::TargetSize => "2.5.8",
            Self::FocusUnreachable => "2.4.3",
            Self::NoFocusIndicator => "2.4.7",
            Self::TextClippedAtScale => "1.4.4",
            Self::MotionIgnoresSetting => "engine",
        }
    }
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AuditSeverity {
    /// The rule is broken.
    Error,
    /// Probably broken, or broken in a way that loses nothing.
    Warning,
    /// The audit cannot tell: a person has to look. Never read this as a pass.
    Info,
}

impl AuditSeverity {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// Which pointer the target-size rule assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuditProfile {
    /// Pointer: [`DEFAULT_MIN_TARGET_PX`], with the criterion's spacing exception.
    #[default]
    Desktop,
    /// Touch: the theme's `size.touch-min` ([`TOUCH_MIN_TARGET_PX`] without it), in
    /// device-independent px (physical px / the OS scale factor), so a UI whose reference scale
    /// shrinks it below the screen density fails on a phone (ADR-4932). A fingertip needs the size
    /// itself, so spacing does not excuse a small target.
    Touch,
}

impl AuditProfile {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Touch => "touch",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Desktop, Self::Touch]
            .into_iter()
            .find(|p| p.name() == name)
    }
}

/// A WCAG 2.2 SC 2.5.8 exception the author declares on a node, because the audit cannot see it. The
/// fifth exception, spacing, is geometry, and the audit measures it. Each use is listed in
/// [`AuditReport::exemptions`], so a reviewer sees what was excused.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetExemption {
    /// Another control on the same screen does the same thing and meets the size.
    Equivalent,
    /// The target sits in a sentence or a block of text.
    Inline,
    /// The platform decides the size and the game did not change it.
    UserAgentControl,
    /// This size is essential to what the control conveys.
    Essential,
}

impl TargetExemption {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Equivalent => "equivalent",
            Self::Inline => "inline",
            Self::UserAgentControl => "user_agent_control",
            Self::Essential => "essential",
        }
    }
}

/// What one audit looks at.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditOptions {
    /// Only this node and its subtree (everything still paints, so backgrounds stay right).
    pub root: Option<Entity>,
    /// Which pointer the target-size rule assumes.
    pub profile: AuditProfile,
    /// The rules to run; empty runs all.
    pub rules: Vec<AuditRule>,
    /// Replaces the profile's minimum target, in UI px (device-independent px under
    /// [`AuditProfile::Touch`]).
    pub min_target_px: Option<f32>,
    /// The text scale the clipping rule lays out at, `1.0..=3.0`.
    pub text_scale: f32,
}

impl Default for AuditOptions {
    fn default() -> Self {
        Self {
            root: None,
            profile: AuditProfile::Desktop,
            rules: Vec::new(),
            min_target_px: None,
            text_scale: AUDIT_TEXT_SCALE,
        }
    }
}

impl AuditOptions {
    fn runs(&self, rule: AuditRule) -> bool {
        self.rules.is_empty() || self.rules.contains(&rule)
    }

    /// [`Self::text_scale`] limited to `1.0..=`[`UiAccessibility::MAX_TEXT_SCALE`]; a non-finite
    /// value counts as [`AUDIT_TEXT_SCALE`].
    pub fn effective_text_scale(&self) -> f32 {
        if self.text_scale.is_finite() {
            self.text_scale.clamp(1.0, UiAccessibility::MAX_TEXT_SCALE)
        } else {
            AUDIT_TEXT_SCALE
        }
    }
}

/// One defect, or one place a person has to look.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditFinding {
    /// The rule.
    pub rule: AuditRule,
    /// How much it matters.
    pub severity: AuditSeverity,
    /// The node (`Entity::to_bits`), valid in this world only. Left out of [`AuditReport::to_lines`].
    pub node: u64,
    /// The node path: [`NodeName`]s, else `role[child index]`. It names no entity, so it stays the
    /// same between runs.
    pub path: String,
    /// What was measured (`2.10:1`, `12.0x12.0 px`, `not_focusable`).
    pub measured: String,
    /// What the rule needs (`4.50:1`, `24.0x24.0 px`).
    pub required: String,
    /// The success criterion number, [`AuditRule::criterion`].
    pub criterion: &'static str,
    /// How to fix it.
    pub hint: String,
    /// The audit could not decide: a person has to look.
    pub needs_eyes: bool,
}

impl AuditFinding {
    /// `rule severity path`: what a checked-in list of allowed findings compares.
    pub fn key(&self) -> String {
        format!("{} {} {}", self.rule.code(), self.severity.name(), self.path)
    }
}

/// A node a rule let pass because of a named exception.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditExemptionUse {
    /// The rule that would have looked at the node.
    pub rule: AuditRule,
    /// The node path.
    pub path: String,
    /// The exception's name (`spacing`, `inline`, `inactive_component`, `decoration`, `captions`,
    /// `text_identifies_control`, ...).
    pub exemption: &'static str,
}

/// The result of one audit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AuditReport {
    /// The findings: by rule, then severity, then path.
    pub findings: Vec<AuditFinding>,
    /// The exceptions used, in the same order.
    pub exemptions: Vec<AuditExemptionUse>,
    /// Rules that could not run, with why.
    pub skipped: Vec<(AuditRule, &'static str)>,
    /// Visible nodes the audit looked at.
    pub nodes: usize,
    /// More than [`MAX_AUDIT_NODES`] were visible: the rest went unchecked.
    pub truncated: bool,
    /// The active theme.
    pub theme: String,
    /// Whether reduced motion is on now. The motion rule does not depend on it.
    pub reduced_motion: bool,
    /// The minimum target the audit used, UI px.
    pub min_target_px: f32,
    /// The text scale the clipping rule laid out at.
    pub text_scale: f32,
}

impl AuditReport {
    /// Whether nothing was found. `info` findings count: they are not passes.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Findings of one rule.
    pub fn of_rule(&self, rule: AuditRule) -> impl Iterator<Item = &AuditFinding> {
        self.findings.iter().filter(move |f| f.rule == rule)
    }

    /// The report as text, one finding per line, with no entity ids: the same screen prints the
    /// same bytes.
    pub fn to_lines(&self) -> String {
        let mut out = String::new();
        for f in &self.findings {
            out.push_str(&format!(
                "{} | {} | {} | {} | wcag {}{}\n",
                f.key(),
                f.measured,
                f.required,
                f.hint,
                f.criterion,
                if f.needs_eyes { " | needs eyes" } else { "" },
            ));
        }
        for e in &self.exemptions {
            out.push_str(&format!("exempt {} {} {}\n", e.rule.code(), e.path, e.exemption));
        }
        for (rule, why) in &self.skipped {
            out.push_str(&format!("skipped {} {why}\n", rule.code()));
        }
        out
    }
}

/// What one painted layer shows.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Paint {
    /// A single color.
    Solid(UiColor),
    /// A two-stop gradient: the worst stop counts.
    Gradient(UiColor, UiColor),
    /// An image, a material or a slider's chrome: the audit does not know the pixels.
    Unknown(&'static str),
}

/// One fill in paint order.
#[derive(Debug, Clone, Copy)]
struct Layer {
    /// Which draw list the layer lands in (a camera or world target stacks on its own).
    group: usize,
    /// Paint order: `z`, then tree preorder, as `extract_ui` sorts its quads.
    key: (f64, u32),
    /// The visible rect: the fill clipped by its ancestors.
    rect: [f32; 4],
    paint: Paint,
    /// Opacity down the hierarchy.
    alpha: f32,
    /// The owning node's index, to name it in a finding.
    owner: usize,
}

/// A span's look.
#[derive(Debug, Clone, Copy)]
struct SpanLook {
    color: UiColor,
    large: bool,
}

/// What a node's text needs for the contrast rule.
#[derive(Debug, Clone)]
struct TextLook {
    spans: Vec<SpanLook>,
    /// Where the glyphs land.
    ink: [f32; 4],
    /// Outline or glow layers that give the glyphs an edge of their own.
    outline: Option<UiColor>,
}

/// One tween that would keep moving under reduced motion.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MotionDefect {
    Transform(TweenProperty, f32),
    LongFade(f32),
}

/// One visible node.
#[derive(Debug, Clone)]
struct SceneNode {
    id: u64,
    path: String,
    group: usize,
    key: (f64, u32),
    rect: [f32; 4],
    /// The hit box: `rect`, grown by a [`TouchTarget`].
    hit: [f32; 4],
    clip: [f32; 4],
    scale: f32,
    alpha: f32,
    in_scope: bool,
    disabled: bool,
    /// [`Accessible::hidden`]: decoration, on this node or an ancestor.
    decoration: bool,
    focusable: bool,
    /// Takes clicks ([`Interactable`] that does not pick through).
    interactable: bool,
    /// In the accessibility tree: its role and name.
    role: Option<A11yRole>,
    name: Option<String>,
    themed: Option<Themed>,
    /// The solid fill it paints (the theme's, else its [`Background`] color).
    fill: Option<UiColor>,
    border: Option<(UiColor, f32)>,
    /// A slider's, toggle's, radio button's, spin box's, field's or combo box's kind name.
    needs_boundary: Option<&'static str>,
    /// A slider's or toggle's knob: it identifies the control too.
    has_knob: bool,
    has_motion_preset: bool,
    text: Option<TextLook>,
    /// Text or an image shows somewhere in the node's subtree.
    shows_text: bool,
    shows_image: bool,
    target_exemption: Option<TargetExemption>,
    motion_defects: Vec<MotionDefect>,
}

/// Text lost at the raised scale.
#[derive(Debug, Clone, PartialEq)]
struct ClipProblem {
    /// The node index in [`AuditScene::nodes`].
    node: usize,
    severity: AuditSeverity,
    measured: String,
    hint: &'static str,
    /// The same problem shows at 100 % too.
    at_full_size: bool,
}

/// A navigation problem from [`crate::focus_graph::NavGraph::lint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NavProblem {
    node: u64,
    kind: NavIssueKind,
}

/// Everything [`audit_scene`] looks at, gathered from a world by [`AuditScene::collect`].
#[derive(Debug, Clone, Default)]
pub struct AuditScene {
    nodes: Vec<SceneNode>,
    /// Sorted by [`Layer::key`].
    layers: Vec<Layer>,
    nav: Vec<NavProblem>,
    clips: Vec<ClipProblem>,
    clip_exemptions: Vec<(usize, &'static str)>,
    skipped: Vec<(AuditRule, &'static str)>,
    truncated: bool,
    theme: String,
    reduced_motion: bool,
    ring_policy: FocusRingPolicy,
    touch_min: Option<f32>,
    motion_fast: f32,
    /// The window's OS scale factor (physical px per device-independent px); 0 when unknown.
    density: f32,
}

/// Audits the screen in `world` (see the module docs). Costs nothing unless it is called; the UI is
/// left as it was.
pub fn audit_ui(world: &mut World, options: &AuditOptions) -> AuditReport {
    let _span = tracing::trace_span!("ui_a11y_audit").entered();
    let scene = AuditScene::collect(world, options);
    audit_scene(&scene, options)
}

// ---------------------------------------------------------------------------------------------
// Rects and colors
// ---------------------------------------------------------------------------------------------

fn area(r: [f32; 4]) -> f32 {
    r[2].max(0.0) * r[3].max(0.0)
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

/// How far `inner` passes `outer` on its worst side, in px (0 when it stays inside).
fn escape(inner: [f32; 4], outer: [f32; 4]) -> f32 {
    let left = outer[0] - inner[0];
    let top = outer[1] - inner[1];
    let right = (inner[0] + inner[2]) - (outer[0] + outer[2]);
    let bottom = (inner[1] + inner[3]) - (outer[1] + outer[3]);
    left.max(top).max(right).max(bottom).max(0.0)
}

/// `fg` over the opaque `bg`, straight alpha, in linear light (what the theme lint composites in).
fn over(fg: UiColor, alpha: f32, bg: UiColor) -> UiColor {
    let a = (fg.0[3] * alpha).clamp(0.0, 1.0);
    UiColor([
        fg.0[0] * a + bg.0[0] * (1.0 - a),
        fg.0[1] * a + bg.0[1] * (1.0 - a),
        fg.0[2] * a + bg.0[2] * (1.0 - a),
        1.0,
    ])
}

fn ratio_text(r: f32) -> String {
    format!("{r:.2}:1")
}

/// What is behind a rect, composited.
#[derive(Debug, Clone, PartialEq)]
enum Backdrop {
    /// The opaque colors the rect can sit on: one, or the samples along a gradient.
    Known(Vec<UiColor>),
    /// The audit does not know: why.
    Unknown(String),
}

impl AuditScene {
    /// What shows behind `rect` in paint order below `upper` (layers before that index in
    /// [`Self::layers`]).
    fn backdrop(&self, group: usize, rect: [f32; 4], upper: usize) -> Backdrop {
        let total = area(rect);
        if total <= 0.0 {
            return Backdrop::Unknown("empty rect".to_owned());
        }
        let mut stack: Vec<&Layer> = Vec::new();
        let mut opaque = false;
        for layer in self.layers[..upper].iter().rev() {
            if layer.group != group {
                continue;
            }
            let covered = area(intersect(layer.rect, rect)) / total;
            if covered < IGNORE_FRACTION {
                continue;
            }
            let drawn = match layer.paint {
                Paint::Solid(c) => c.0[3] * layer.alpha > CLEAR_ALPHA,
                Paint::Gradient(a, b) => a.0[3].max(b.0[3]) * layer.alpha > CLEAR_ALPHA,
                Paint::Unknown(_) => layer.alpha > CLEAR_ALPHA,
            };
            if !drawn {
                continue;
            }
            if let Paint::Unknown(what) = layer.paint {
                return Backdrop::Unknown(format!("{what} at {}", self.nodes[layer.owner].path));
            }
            if covered < COVER_FRACTION {
                return Backdrop::Unknown(format!(
                    "partly over the fill of {}",
                    self.nodes[layer.owner].path
                ));
            }
            stack.push(layer);
            let solid = match layer.paint {
                Paint::Solid(c) => c.0[3] * layer.alpha >= OPAQUE_ALPHA,
                Paint::Gradient(a, b) => a.0[3].min(b.0[3]) * layer.alpha >= OPAQUE_ALPHA,
                Paint::Unknown(_) => false,
            };
            if solid {
                opaque = true;
                break;
            }
        }
        if !opaque {
            return Backdrop::Unknown(
                "no opaque fill behind it: the scene or the clear color shows".to_owned(),
            );
        }
        let gradient = stack.iter().any(|l| matches!(l.paint, Paint::Gradient(..)));
        let samples = if gradient { GRADIENT_SAMPLES } else { 1 };
        let colors = (0..samples)
            .map(|i| {
                let t = if samples > 1 {
                    i as f32 / (samples - 1) as f32
                } else {
                    0.0
                };
                stack.iter().rev().fold(UiColor::BLACK, |under, layer| {
                    let top = match layer.paint {
                        Paint::Solid(c) => c,
                        Paint::Gradient(a, b) => a.lerp(b, t),
                        Paint::Unknown(_) => UiColor::TRANSPARENT,
                    };
                    over(top, layer.alpha, under)
                })
            })
            .collect();
        Backdrop::Known(colors)
    }

    /// The index in [`Self::layers`] of the first layer that paints after `key` (`inclusive`) or at
    /// or after it.
    fn layers_below(&self, key: (f64, u32), inclusive: bool) -> usize {
        self.layers.partition_point(|l| {
            if inclusive {
                l.key.partial_cmp(&key).is_some_and(|o| o.is_le())
            } else {
                l.key.partial_cmp(&key).is_some_and(|o| o.is_lt())
            }
        })
    }

    /// Solid layers painted over `rect`: `None` when one hides it, else the translucent ones, lowest
    /// first, which tint glyphs and background alike (a dialog's scrim).
    fn overlays(&self, group: usize, rect: [f32; 4], from: usize) -> Option<Vec<(UiColor, f32)>> {
        let total = area(rect);
        let mut out = Vec::new();
        for layer in &self.layers[from..] {
            if layer.group != group || area(intersect(layer.rect, rect)) / total.max(1e-3) < COVER_FRACTION {
                continue;
            }
            match layer.paint {
                Paint::Solid(c) => {
                    let a = c.0[3] * layer.alpha;
                    if a >= OPAQUE_ALPHA {
                        return None;
                    }
                    if a > CLEAR_ALPHA {
                        out.push((c, layer.alpha));
                    }
                }
                Paint::Gradient(a, b) => {
                    if a.0[3].min(b.0[3]) * layer.alpha >= OPAQUE_ALPHA {
                        return None;
                    }
                }
                Paint::Unknown(_) => {}
            }
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Gathering
// ---------------------------------------------------------------------------------------------

/// What a node hands down its subtree.
#[derive(Debug, Clone, Copy)]
struct Inherited {
    z_base: f64,
    alpha: f32,
    disabled: bool,
    decoration: bool,
    in_scope: bool,
    group: usize,
    /// The clip the parent hands its children; `None` for a root.
    clip: Option<[f32; 4]>,
}

struct Walk<'a> {
    world: &'a World,
    tree: HashMap<u64, &'a A11yNode>,
    scope: Option<Entity>,
    live_text_scale: f32,
    scene: AuditScene,
    seen: HashSet<Entity>,
    next_seq: u32,
    /// Live entity to node index, for the clipping rule.
    index: HashMap<Entity, usize>,
}

impl Walk<'_> {
    /// The path segment of `e`, the `child_index`-th child of its parent.
    fn segment(&self, e: Entity, child_index: usize) -> String {
        if let Some(name) = self.world.get::<NodeName>(e).filter(|n| !n.0.is_empty()) {
            return name.0.clone();
        }
        let kind = if let Some(node) = self.tree.get(&e.to_bits()) {
            node.role().name()
        } else if self.world.get::<Text>(e).is_some() {
            "text"
        } else if self.world.get::<Interactable>(e).is_some() {
            "interactable"
        } else {
            "node"
        };
        format!("{kind}[{child_index}]")
    }

    /// Adds `e` and its subtree. Returns (shows text, shows an image), for the parent.
    fn visit(
        &mut self,
        e: Entity,
        parent: Option<usize>,
        child_index: usize,
        up: Inherited,
        depth: usize,
    ) -> (bool, bool) {
        if depth >= MAX_A11Y_DEPTH || !self.seen.insert(e) {
            return (false, false);
        }
        if self.scene.nodes.len() >= MAX_AUDIT_NODES {
            self.scene.truncated = true;
            return (false, false);
        }
        let world = self.world;
        // The same early-outs `extract_ui` takes: no layout, hidden or empty draws nothing, and
        // nothing below it either.
        let Some(computed) = world.get::<ComputedNode>(e).copied() else {
            return (false, false);
        };
        let style = world.get::<NodeStyle>(e).map(|s| &s.0);
        if style.is_some_and(|s| s.display == Display::None) {
            return (false, false);
        }
        let rect = computed.rect;
        if rect[2] <= 0.0 || rect[3] <= 0.0 {
            return (false, false);
        }
        let themed = world.get::<Themed>(e).copied();
        let z_base = up.z_base + world.get::<ZOffset>(e).map_or(0.0, |z| f64::from(z.0));
        // A group layer composites its subtree at the group's opacity (`extract_ui`); a root is
        // never a group. A faded-out dialog is not an opaque cover, and its text is that faint.
        let group_opacity = world
            .get::<crate::group_layer::GroupOpacity>(e)
            .filter(|_| world.get::<UiRoot>(e).is_none())
            .map_or(1.0, |g| if g.0.is_finite() { g.0.clamp(0.0, 1.0) } else { 1.0 });
        let alpha = up.alpha
            * group_opacity
            * world.get::<Opacity>(e).map_or(1.0, |o| o.0.clamp(0.0, 1.0))
            * themed.map_or(1.0, |t| t.opacity.unwrap_or(1.0).clamp(0.0, 1.0));
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let key = (z_base, seq);
        let index = self.scene.nodes.len();
        self.index.insert(e, index);

        let scale = computed.scale.max(0.001);
        // Layout folds a clipping node's own rect into its `clip_rect`, for its children. The node
        // itself paints under its ancestors' clip, as in `extract_ui`: its ring may pass its rect.
        let clips_itself = world.get::<ClipChildren>(e).is_some()
            || world.get::<RoundedClip>(e).is_some()
            || style.is_some_and(|s| s.overflow.contains(&Overflow::Clip));
        let own_clip = match up.clip {
            Some(parent_clip) if clips_itself => parent_clip,
            _ => computed.clip_rect,
        };
        let visible = intersect(rect, own_clip);
        let in_scope = up.in_scope || self.scope == Some(e);
        let disabled = up.disabled || crate::node::is_disabled(world, e);
        let accessible = world.get::<Accessible>(e);
        let decoration = up.decoration || accessible.is_some_and(|a| a.hidden);

        self.push_layers(e, index, up.group, key, rect, visible, themed, alpha);

        let tree_node = self.tree.get(&e.to_bits()).copied();
        let name = tree_node.map(|n| n.name.clone());
        let role = tree_node.map(A11yNode::role);
        let segment = self.segment(e, child_index);
        let path = match parent {
            Some(p) => format!("{}/{segment}", self.scene.nodes[p].path),
            None => segment,
        };
        let border = match world
            .get::<Border>(e)
            .filter(|b| b.widths.iter().any(|w| *w > 0.0))
        {
            Some(b) => Some((b.color, b.widths.iter().copied().fold(0.0, f32::max))),
            None => themed.and_then(|t| {
                t.border_width
                    .filter(|w| *w > 0.0)
                    .map(|w| (t.border_color.unwrap_or(UiColor([0.2, 0.22, 0.28, 1.0])), w))
            }),
        };
        let needs_boundary = if world.get::<Slider>(e).is_some() {
            Some("slider")
        } else if world.get::<Toggle>(e).is_some() {
            Some("toggle")
        } else if world.get::<RadioButton>(e).is_some() {
            Some("radio_button")
        } else if world.get::<SpinBox>(e).is_some() {
            Some("spin_box")
        } else if world.get::<TextInput>(e).is_some() {
            Some("text_input")
        } else if world.get::<Dropdown>(e).is_some() {
            Some("combo_box")
        } else {
            None
        };
        let fill = solid_fill(world, e, themed);
        let text = world
            .get::<Text>(e)
            .and_then(|text| self.text_look(e, text, rect, scale));
        let motion_defects = world
            .get::<UiTween>(e)
            .filter(|t| t.follows_reduced_motion)
            .map(|t| {
                let mut out = Vec::new();
                motion_defects(&t.plan, self.scene.motion_fast, &mut out);
                out
            })
            .unwrap_or_default();
        let own_text = text.is_some();
        let own_image = world.get::<UiImage>(e).is_some()
            || matches!(
                world.get::<Background>(e),
                Some(Background {
                    kind: BackgroundKind::Image { .. }
                })
            );
        self.scene.nodes.push(SceneNode {
            id: e.to_bits(),
            path,
            group: up.group,
            key,
            rect,
            hit: world.get::<TouchTarget>(e).map_or(rect, |t| t.grow(rect, scale)),
            clip: own_clip,
            scale,
            alpha,
            in_scope,
            disabled,
            decoration,
            focusable: world.get::<Focusable>(e).is_some(),
            interactable: world.get::<Interactable>(e).is_some_and(|i| !i.pick_through),
            role,
            name,
            themed,
            fill,
            border,
            needs_boundary,
            has_knob: world.get::<Slider>(e).is_some() || world.get::<Toggle>(e).is_some(),
            has_motion_preset: world.get::<UiMotion>(e).is_some(),
            text,
            shows_text: own_text,
            shows_image: own_image,
            target_exemption: world.get::<TargetExemption>(e).copied(),
            motion_defects,
        });

        let down = Inherited {
            z_base,
            alpha,
            disabled,
            decoration,
            in_scope,
            group: up.group,
            clip: Some(computed.clip_rect),
        };
        let children: Vec<Entity> = world
            .get::<Children>(e)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let (mut shows_text, mut shows_image) = (own_text, own_image);
        for (i, child) in children.into_iter().enumerate() {
            let (t, m) = self.visit(child, Some(index), i, down, depth + 1);
            shows_text |= t;
            shows_image |= m;
        }
        let node = &mut self.scene.nodes[index];
        node.shows_text = shows_text;
        node.shows_image = shows_image;
        (shows_text, shows_image)
    }

    /// The fills `e` paints, in the order `extract_ui` paints them.
    #[allow(clippy::too_many_arguments)]
    fn push_layers(
        &mut self,
        e: Entity,
        index: usize,
        group: usize,
        key: (f64, u32),
        rect: [f32; 4],
        visible: [f32; 4],
        themed: Option<Themed>,
        alpha: f32,
    ) {
        let world = self.world;
        let mut push = |paint: Paint, rect: [f32; 4]| {
            self.scene.layers.push(Layer {
                group,
                key,
                rect,
                paint,
                alpha,
                owner: index,
            });
        };
        if let Some(c) = solid_fill(world, e, themed) {
            push(Paint::Solid(c), visible);
        }
        match world.get::<Background>(e).map(|b| &b.kind) {
            Some(BackgroundKind::Gradient { start, end, .. }) => {
                push(Paint::Gradient(*start, *end), visible);
            }
            Some(BackgroundKind::Image { .. }) => push(Paint::Unknown("a background image"), visible),
            _ => {}
        }
        if world.get::<UiMaterial>(e).is_some() {
            push(Paint::Unknown("a UI material"), visible);
        }
        if let Some(canvas) = world.get::<UiCanvas>(e) {
            for r in &canvas.rects {
                let part = [
                    rect[0] + r.min[0] * rect[2],
                    rect[1] + r.min[1] * rect[3],
                    (r.max[0] - r.min[0]) * rect[2],
                    (r.max[1] - r.min[1]) * rect[3],
                ];
                push(Paint::Solid(r.color), intersect(part, visible));
            }
        }
        if world.get::<UiImage>(e).is_some_and(|i| i.tint.0[3] > 0.0) {
            push(Paint::Unknown("an image"), visible);
        }
        if let Some(toggle) = world.get::<Toggle>(e)
            && toggle.checked
        {
            let accent = themed
                .and_then(|t| t.accent)
                .unwrap_or(UiColor([0.24, 0.62, 0.94, 1.0]));
            push(Paint::Solid(accent.lerp(UiColor::BLACK, 0.45)), visible);
        }
        let fraction = world
            .get::<Slider>(e)
            .map(Slider::fraction)
            .or_else(|| world.get::<ProgressBar>(e).map(|b| b.value));
        if let Some(fraction) = fraction
            && fraction > 0.0
        {
            let part = [rect[0], rect[1], rect[2] * fraction.clamp(0.0, 1.0), rect[3]];
            push(
                Paint::Unknown("a slider or progress fill"),
                intersect(part, visible),
            );
        }
    }

    fn text_look(&self, e: Entity, text: &Text, rect: [f32; 4], scale: f32) -> Option<TextLook> {
        let loc = self.world.get_resource::<Localization>();
        let spans: Vec<SpanLook> = text
            .spans
            .iter()
            .filter(|span| match &span.content {
                TextContent::Lit(s) => !s.trim().is_empty(),
                content => loc.is_none_or(|l| !content.resolve(l).trim().is_empty()),
            })
            .map(|span| {
                let px = span.style.font_px * self.live_text_scale;
                let bold = span.style.bold || span.style.weight.is_some_and(|w| w >= 700);
                SpanLook {
                    color: span.style.color,
                    large: px >= LARGE_TEXT_PX || (bold && px >= LARGE_BOLD_TEXT_PX),
                }
            })
            .collect();
        if spans.is_empty() {
            return None;
        }
        let outline = text
            .spans
            .iter()
            .flat_map(|s| s.style.effects.iter())
            .find(|fx| fx.width > 0.0 && fx.color.0[3] > 0.5)
            .map(|fx| fx.color);
        // The ink: from the glyphs when the text is shaped, else the content box.
        let pad = content_padding(self.world.get::<NodeStyle>(e), scale);
        let content = [
            rect[0] + pad[0],
            rect[1] + pad[1],
            (rect[2] - pad[0] - pad[2]).max(0.0),
            (rect[3] - pad[1] - pad[3]).max(0.0),
        ];
        let ink = self
            .world
            .get::<crate::text::ShapedText>(e)
            .filter(|s| !s.glyphs.is_empty())
            .map(|s| ink_rect(content, s))
            .unwrap_or(content);
        Some(TextLook { spans, ink, outline })
    }
}

/// The solid color a node fills with: the theme's beats the explicit [`Background`], as in
/// `extract_ui`.
fn solid_fill(world: &World, e: Entity, themed: Option<Themed>) -> Option<UiColor> {
    themed
        .and_then(|t| t.background)
        .or_else(|| match world.get::<Background>(e).map(|b| &b.kind) {
            Some(BackgroundKind::Color(c)) => Some(*c),
            _ => None,
        })
}

/// Padding in physical px (l, t, r, b): points only, as `extract_ui` reads it.
fn content_padding(style: Option<&NodeStyle>, scale: f32) -> [f32; 4] {
    let dim = |d: Dimension| match d {
        Dimension::Points(v) => v * scale,
        _ => 0.0,
    };
    style.map_or([0.0; 4], |s| {
        let p = &s.0.padding;
        [dim(p.left), dim(p.top), dim(p.right), dim(p.bottom)]
    })
}

/// Where a shaped block lands in its content box: centered vertically, as `extract_ui` places it.
fn ink_rect(content: [f32; 4], shaped: &crate::text::ShapedText) -> [f32; 4] {
    let left = shaped
        .glyphs
        .iter()
        .map(|g| g.pen[0] as f32)
        .fold(f32::MAX, f32::min);
    let y = content[1] + ((content[3] - shaped.size[1]) * 0.5).max(0.0);
    [content[0] + left, y, shaped.size[0], shaped.size[1]]
}

/// Walks a plan for what would still move under reduced motion: anything but opacity with a duration,
/// or a fade past `fast` (spec `ui-default-design` § 2.6).
fn motion_defects(plan: &TweenPlan, fast: f32, out: &mut Vec<MotionDefect>) {
    match plan {
        TweenPlan::Tween(t) => {
            // The runtime holds the start value through the delay (`Tween::sample_at`), so only
            // the duration moves anything: a delayed jump is still a jump, and a capped fade
            // after a delay is still capped.
            let moves = t.duration;
            if t.property == TweenProperty::Opacity {
                if moves > fast + 1e-4 {
                    out.push(MotionDefect::LongFade(moves));
                }
            } else if moves > 0.0 {
                out.push(MotionDefect::Transform(t.property, moves));
            }
        }
        TweenPlan::Sequence(steps) | TweenPlan::Parallel(steps) => {
            for step in steps {
                motion_defects(step, fast, out);
            }
        }
    }
}

impl AuditScene {
    /// Gathers the scene from `world`. Needs `&mut World` for queries and the scratch layout; every
    /// resource it borrows goes back unchanged.
    pub fn collect(world: &mut World, options: &AuditOptions) -> Self {
        let tree = build_a11y_tree(world);
        let graph = world_graph(world);
        let defaults = world_defaults(world);
        let start = world
            .get_resource::<crate::focus::FocusState>()
            .and_then(|f| f.focused)
            .filter(|f| graph.contains(*f));
        let nav: Vec<NavProblem> = graph
            .lint(start, &defaults)
            .into_iter()
            .filter(|i| matches!(i.kind, NavIssueKind::Unreachable | NavIssueKind::DeadEnd))
            .map(|i| NavProblem {
                node: i.node.to_bits(),
                kind: i.kind,
            })
            .collect();
        let mut roots: Vec<(i32, Entity)> = world
            .query::<(Entity, &UiRoot)>()
            .iter(world)
            .map(|(e, r)| (r.sort_order, e))
            .collect();
        roots.sort_by_key(|(order, e)| (*order, e.to_bits()));
        let active = crate::output::active_variant(world);
        roots.retain(|(_, e)| crate::output::root_active(world, *e, active));

        let shared: &World = world;
        let mut walk = Walk {
            world: shared,
            tree: tree.nodes.iter().map(|n| (n.id, n)).collect(),
            scope: options.root,
            live_text_scale: shared
                .get_resource::<UiAccessibility>()
                .map_or(1.0, UiAccessibility::effective_text_scale),
            scene: AuditScene {
                nav,
                theme: shared
                    .get_resource::<ThemeResolver>()
                    .map(|r| r.active().to_owned())
                    .unwrap_or_default(),
                reduced_motion: reduced_motion(
                    shared.get_resource::<ActiveTheme>(),
                    shared.get_resource::<UiAccessibility>(),
                ),
                ring_policy: shared
                    .get_resource::<FocusRingPolicy>()
                    .copied()
                    .unwrap_or_default(),
                touch_min: shared
                    .get_resource::<ThemeResolver>()
                    .and_then(ThemeResolver::resolved_theme)
                    .and_then(|t| t.number("size.touch-min")),
                motion_fast: MotionTokens::from_resolver(shared.get_resource::<ThemeResolver>()).fast,
                density: shared
                    .get_resource::<UiScales>()
                    .map_or(0.0, |s| s.scale_factor as f32),
                ..AuditScene::default()
            },
            seen: HashSet::new(),
            next_seq: 0,
            index: HashMap::new(),
        };
        let mut live_roots = Vec::new();
        for (slot, (order, root)) in roots.iter().copied().enumerate() {
            // A window root stacks with the other window roots; a camera or world target alone.
            let on_window = crate::world_target::root_target_size(shared, root).is_none();
            let group = if on_window { 0 } else { slot + 1 };
            let up = Inherited {
                z_base: f64::from(order),
                alpha: 1.0,
                disabled: false,
                decoration: false,
                in_scope: options.root.is_none(),
                group,
                clip: None,
            };
            walk.visit(root, None, slot, up, 0);
            live_roots.push(root);
        }
        let index = std::mem::take(&mut walk.index);
        let mut scene = walk.scene;
        scene
            .layers
            .sort_by(|a, b| a.key.partial_cmp(&b.key).unwrap_or(std::cmp::Ordering::Equal));
        if options.runs(AuditRule::TextClippedAtScale) {
            scratch_layout_checks(world, options, &live_roots, &index, &mut scene);
        }
        scene
    }
}

// ---------------------------------------------------------------------------------------------
// The scratch layout: text at the raised scale
// ---------------------------------------------------------------------------------------------

/// Copies what layout reads of `e`'s subtree into `scratch`, keeping the hierarchy order. Returns the
/// scratch entity; `map` goes from scratch to live.
fn copy_for_layout(
    live: &World,
    scratch: &mut World,
    e: Entity,
    parent: Option<Entity>,
    map: &mut HashMap<Entity, Entity>,
    depth: usize,
) -> Option<Entity> {
    if depth >= MAX_A11Y_DEPTH {
        return None;
    }
    let mut copy = scratch.spawn(UiNode);
    if let Some(style) = live.get::<NodeStyle>(e) {
        copy.insert(style.clone());
    }
    if let Some(text) = live.get::<Text>(e) {
        copy.insert(text.clone());
    }
    if live.get::<ClipChildren>(e).is_some() {
        copy.insert(ClipChildren);
    }
    if live.get::<RoundedClip>(e).is_some() {
        copy.insert(RoundedClip);
    }
    if let Some(parent) = parent {
        copy.insert(ChildOf(parent));
    }
    let id = copy.id();
    map.insert(id, e);
    if let Some(children) = live.get::<Children>(e) {
        for child in children.iter() {
            copy_for_layout(live, scratch, child, Some(id), map, depth + 1);
        }
    }
    Some(id)
}

/// A text's problems at one text scale, keyed by the live entity and the kind of problem.
type ScaleProblems = HashMap<(Entity, &'static str), ClipProblem>;

/// Lays every root out at `text_scale` and at 100 % in scratch worlds and records lost text.
fn scratch_layout_checks(
    world: &mut World,
    options: &AuditOptions,
    roots: &[Entity],
    index: &HashMap<Entity, usize>,
    scene: &mut AuditScene,
) {
    let Some(mut system) = world.remove_resource::<TextSystem>() else {
        scene
            .skipped
            .push((AuditRule::TextClippedAtScale, "the app has no TextSystem"));
        return;
    };
    let saved_scale = system.text_scale;
    let mut loc = world.remove_resource::<Localization>().unwrap_or_default();
    let scales = world.get_resource::<UiScales>().copied().unwrap_or_default();
    let policy = world.get_resource::<crate::scale::UiDevicePolicy>().copied();
    let target_scale = options.effective_text_scale();
    for &root in roots {
        let live: &World = world;
        // The size and scale the live pass uses, so the scratch pass needs no window or policy.
        let (window, padding) = resolve_window_size(live, root, &scales);
        let mode = live.get::<UiRoot>(root).map(|r| r.scale_mode).unwrap_or_default();
        let live_scale =
            scales.scale_with_policy(&mode, policy.as_ref()) * crate::output::render_out_boost(live, &mode);
        let mut full_size = ScaleProblems::new();
        for (text_scale, is_target) in [(1.0, false), (target_scale, true)] {
            system.text_scale = text_scale;
            let (out_system, out_loc, problems) = layout_at(
                live,
                root,
                window,
                padding,
                live_scale,
                system,
                loc,
                index,
                &scene.nodes,
            );
            system = out_system;
            loc = out_loc;
            if is_target {
                let mut list: Vec<ClipProblem> = problems
                    .into_iter()
                    .filter_map(|(key, mut problem)| {
                        let earlier = full_size.get(&key);
                        // A label that escapes its parent at 100 % too was placed there on purpose;
                        // anything lost is lost at both sizes, and still worth saying.
                        if key.1 == "escapes" && earlier.is_some() {
                            return None;
                        }
                        problem.at_full_size = earlier.is_some();
                        Some(problem)
                    })
                    .collect();
                scene.clips.append(&mut list);
            } else {
                full_size = problems;
            }
        }
    }
    let live: &World = world;
    for (&e, &i) in index {
        if live.get::<Text>(e).is_some() && under_captions(live, e) {
            scene.clip_exemptions.push((i, "captions"));
        }
    }
    system.text_scale = saved_scale;
    world.insert_resource(system);
    world.insert_resource(loc);
}

/// Text in a caption block: SC 1.4.4 excepts captions, and they have their own size option.
fn under_captions(world: &World, e: Entity) -> bool {
    let mut current = Some(e);
    let mut steps = 0;
    while let Some(c) = current {
        if world.get::<CaptionBlock>(c).is_some() || world.get::<CaptionRow>(c).is_some() {
            return true;
        }
        steps += 1;
        if steps > MAX_A11Y_DEPTH {
            return false;
        }
        current = world.get::<ChildOf>(c).map(|p| p.0);
    }
    false
}

/// One scratch layout of `root`. The text system and localization move in and come back out.
#[allow(clippy::too_many_arguments)]
fn layout_at(
    live: &World,
    root: Entity,
    window: [f32; 2],
    padding: [f32; 4],
    live_scale: f32,
    system: TextSystem,
    loc: Localization,
    index: &HashMap<Entity, usize>,
    nodes: &[SceneNode],
) -> (TextSystem, Localization, ScaleProblems) {
    let mut scratch = World::new();
    let mut map = HashMap::new();
    let Some(scratch_root) = copy_for_layout(live, &mut scratch, root, None, &mut map, 0) else {
        return (system, loc, ScaleProblems::new());
    };
    // `ConstantPhysicalSize` at the live scale gives the live layout scale without a policy or a
    // reference size; the insets the live pass resolved become this root's padding.
    scratch.entity_mut(scratch_root).insert(UiRoot {
        scale_mode: ScaleMode::ConstantPhysicalSize,
        ..UiRoot::default()
    });
    if padding.iter().any(|p| *p != 0.0) {
        scratch.entity_mut(scratch_root).insert(crate::SafeAreaPadding);
        scratch.insert_resource(regia_platform::SafeArea {
            left: padding[0] as u32,
            top: padding[1] as u32,
            right: padding[2] as u32,
            bottom: padding[3] as u32,
        });
    }
    scratch.insert_resource(UiScales {
        window_size: window,
        scale_factor: f64::from(live_scale.max(0.001)),
    });
    scratch.insert_resource(LayoutDirty(true));
    let saved_scale = system.text_scale;
    scratch.insert_resource(system);
    scratch.insert_resource(loc);
    if let Err(err) = run_ui_layout(&mut scratch) {
        tracing::error!(target: "regia_ui", "scratch layout failed: {err}");
    }
    // A failed layout drops the two resources it held out; the live world still gets working ones.
    let mut system = scratch.remove_resource::<TextSystem>().unwrap_or_else(|| {
        let mut fresh = TextSystem::embedded_only();
        fresh.text_scale = saved_scale;
        fresh
    });
    let loc = scratch.remove_resource::<Localization>().unwrap_or_default();
    let problems = find_lost_text(live, &scratch, &map, window, &mut system, &loc, index, nodes);
    (system, loc, problems)
}

/// What the raised text does in the scratch layout.
#[allow(clippy::too_many_arguments)]
fn find_lost_text(
    live: &World,
    scratch: &World,
    map: &HashMap<Entity, Entity>,
    window: [f32; 2],
    system: &mut TextSystem,
    loc: &Localization,
    index: &HashMap<Entity, usize>,
    nodes: &[SceneNode],
) -> ScaleProblems {
    let mut out = ScaleProblems::new();
    let mut texts: Vec<(Entity, Entity)> = map
        .iter()
        .filter(|(s, _)| scratch.get::<Text>(**s).is_some())
        .map(|(s, l)| (*s, *l))
        .collect();
    texts.sort_by_key(|(_, l)| l.to_bits());
    for (scratch_e, live_e) in texts {
        // Nodes the live walk did not reach are not on screen; decoration needs no text.
        let Some(&node) = index.get(&live_e) else {
            continue;
        };
        if live.get::<Accessible>(live_e).is_some_and(|a| a.hidden) || under_captions(live, live_e) {
            continue;
        }
        let (Some(text), Some(computed)) = (
            scratch.get::<Text>(scratch_e),
            scratch.get::<ComputedNode>(scratch_e).copied(),
        ) else {
            continue;
        };
        if text
            .spans
            .iter()
            .all(|s| s.content.resolve(loc).trim().is_empty())
        {
            continue;
        }
        let rect = computed.rect;
        if rect[2] <= 0.0 || rect[3] <= 0.0 {
            continue;
        }
        let scale = computed.scale.max(0.001);
        let pad = content_padding(scratch.get::<NodeStyle>(scratch_e), scale);
        let content = [
            rect[0] + pad[0],
            rect[1] + pad[1],
            (rect[2] - pad[0] - pad[2]).max(0.0),
            (rect[3] - pad[1] - pad[3]).max(0.0),
        ];
        let max_w = (content[2] > 1.0).then_some(content[2]);
        let shaped = system.shape(text, scale, max_w, loc);
        let mut add = |kind: &'static str, severity, measured: String, hint: &'static str| {
            out.insert(
                (live_e, kind),
                ClipProblem {
                    node,
                    severity,
                    measured,
                    hint,
                    at_full_size: false,
                },
            );
        };
        if shaped.truncated {
            add(
                "ellipsized",
                AuditSeverity::Error,
                "cut short with an ellipsis".to_owned(),
                "let the box grow or the text wrap, or show the whole text another way",
            );
            continue;
        }
        if text.overflow == TextOverflow::ScaleToFit {
            // The text fits by shrinking back: nothing lost, but it did not grow as asked. Measure
            // the size it reached against the size it had at the same box.
            let natural = {
                let saved = system.text_scale;
                system.text_scale = 1.0;
                let at_one = system.shape(text, scale, None, loc);
                system.text_scale = saved;
                at_one.size[0]
            };
            if natural > 0.0 && shaped.size[0] / natural < system.text_scale - 0.05 {
                add(
                    "no_growth",
                    AuditSeverity::Warning,
                    format!("fit-to-box holds the text at {:.2}x", shaped.size[0] / natural),
                    "give the box room to grow, so the text can reach the asked size",
                );
            }
            continue;
        }
        let ink = if shaped.glyphs.is_empty() {
            content
        } else {
            ink_rect(content, &shaped)
        };
        if text.overflow == TextOverflow::Clip && escape(ink, rect) > OVERFLOW_SLACK_PX {
            add(
                "own_clip",
                AuditSeverity::Error,
                format!(
                    "text needs {:.0}x{:.0} px in a {:.0}x{:.0} px box that clips",
                    ink[2], ink[3], rect[2], rect[3]
                ),
                "let the box grow or the text wrap, or use a scroll view",
            );
        }
        // Up the hierarchy: the first scroll view makes everything below it reachable.
        let mut reachable = false;
        let mut escaped_fill = None;
        let mut current = scratch.get::<ChildOf>(scratch_e).map(|p| p.0);
        let mut clipped_by = None;
        while let Some(ancestor) = current {
            let Some(&live_a) = map.get(&ancestor) else { break };
            if live.get::<ScrollView>(live_a).is_some() || live.get::<ListView>(live_a).is_some() {
                // A view whose content fits it grew with the content and has nothing to scroll to.
                let scrolling = scratch.get::<ComputedNode>(ancestor).filter(|c| {
                    c.content_size[0] > c.rect[2] + OVERFLOW_SLACK_PX
                        || c.content_size[1] > c.rect[3] + OVERFLOW_SLACK_PX
                });
                if let Some(view) = scrolling {
                    if escape(view.rect, [0.0, 0.0, window[0], window[1]]) <= OVERFLOW_SLACK_PX {
                        reachable = true;
                        break;
                    }
                    // Below the fold itself: it reaches what it clips, and an outer view may
                    // reach it.
                    current = scratch.get::<ChildOf>(ancestor).map(|p| p.0);
                    continue;
                }
            }
            if let Some(c) = scratch.get::<ComputedNode>(ancestor) {
                let by = escape(ink, c.rect);
                if by > OVERFLOW_SLACK_PX {
                    let clips = scratch.get::<ClipChildren>(ancestor).is_some()
                        || scratch.get::<RoundedClip>(ancestor).is_some()
                        || scratch
                            .get::<NodeStyle>(ancestor)
                            .is_some_and(|s| s.0.overflow.contains(&Overflow::Clip));
                    if clips && clipped_by.is_none() {
                        clipped_by = Some((live_a, by));
                    } else if escaped_fill.is_none() && paints_box(live, live_a) {
                        escaped_fill = Some((live_a, by));
                    }
                }
            }
            current = scratch.get::<ChildOf>(ancestor).map(|p| p.0);
        }
        if let Some((by_node, by)) = clipped_by {
            let who = index
                .get(&by_node)
                .and_then(|&i| nodes.get(i))
                .map_or("an ancestor", |n| n.path.as_str());
            add(
                "ancestor_clip",
                AuditSeverity::Error,
                format!("{who} cuts {by:.0} px of the text"),
                "let the container grow, or make it a scroll view",
            );
        }
        if !reachable && escape(ink, [0.0, 0.0, window[0], window[1]]) > OVERFLOW_SLACK_PX {
            add(
                "off_screen",
                AuditSeverity::Error,
                format!(
                    "text ends {:.0} px past the screen",
                    escape(ink, [0.0, 0.0, window[0], window[1]])
                ),
                "wrap the text or put the screen in a scroll view",
            );
        }
        // Text already reported as lost needs no second line for spilling on its way out.
        let lost = ["own_clip", "ancestor_clip", "off_screen"]
            .iter()
            .any(|kind| out.contains_key(&(live_e, *kind)));
        if let Some((_, by)) = escaped_fill.filter(|_| !lost) {
            out.insert(
                (live_e, "escapes"),
                ClipProblem {
                    node,
                    severity: AuditSeverity::Warning,
                    measured: format!("text spills {by:.0} px out of its container"),
                    hint: "let the container grow with its text",
                    at_full_size: false,
                },
            );
        }
    }
    out
}

/// Whether a node draws a box a label can visibly spill out of.
fn paints_box(world: &World, e: Entity) -> bool {
    let fill = world
        .get::<Themed>(e)
        .and_then(|t| t.background)
        .is_some_and(|c| c.0[3] > CLEAR_ALPHA)
        || world.get::<Background>(e).is_some_and(|b| match &b.kind {
            BackgroundKind::Color(c) => c.0[3] > CLEAR_ALPHA,
            _ => true,
        });
    let edge = world
        .get::<Border>(e)
        .is_some_and(|b| b.widths.iter().any(|w| *w > 0.0))
        || world
            .get::<Themed>(e)
            .and_then(|t| t.border_width)
            .is_some_and(|w| w > 0.0);
    fill || edge
}

// ---------------------------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------------------------

/// The pure audit: from a gathered scene to a report in a stable order.
pub fn audit_scene(scene: &AuditScene, options: &AuditOptions) -> AuditReport {
    let min_target_px = options
        .min_target_px
        .filter(|m| m.is_finite() && *m > 0.0)
        .unwrap_or(match options.profile {
            AuditProfile::Desktop => DEFAULT_MIN_TARGET_PX,
            AuditProfile::Touch => scene.touch_min.unwrap_or(TOUCH_MIN_TARGET_PX),
        });
    let mut out = Rules {
        scene,
        options,
        min_target_px,
        findings: Vec::new(),
        exemptions: Vec::new(),
    };
    let run = |rule: AuditRule| options.runs(rule);
    if run(AuditRule::UnnamedInteractive) {
        out.unnamed();
    }
    if run(AuditRule::TextContrast) {
        out.text_contrast();
    }
    if run(AuditRule::NontextContrast) {
        out.nontext_contrast();
    }
    if run(AuditRule::TargetSize) {
        out.target_size();
    }
    if run(AuditRule::FocusUnreachable) {
        out.focus_unreachable();
    }
    if run(AuditRule::NoFocusIndicator) {
        out.no_focus_indicator();
    }
    if run(AuditRule::TextClippedAtScale) {
        out.text_clipped();
    }
    if run(AuditRule::MotionIgnoresSetting) {
        out.motion();
    }
    let Rules {
        mut findings,
        mut exemptions,
        ..
    } = out;
    findings.sort_by(|a, b| {
        (a.rule, a.severity, &a.path, &a.measured, &a.required, a.node).cmp(&(
            b.rule,
            b.severity,
            &b.path,
            &b.measured,
            &b.required,
            b.node,
        ))
    });
    exemptions.sort_by(|a, b| (a.rule, &a.path, a.exemption).cmp(&(b.rule, &b.path, b.exemption)));
    exemptions.dedup();
    AuditReport {
        findings,
        exemptions,
        skipped: scene.skipped.clone(),
        nodes: scene.nodes.len(),
        truncated: scene.truncated,
        theme: scene.theme.clone(),
        reduced_motion: scene.reduced_motion,
        min_target_px,
        text_scale: options.effective_text_scale(),
    }
}

struct Rules<'a> {
    scene: &'a AuditScene,
    options: &'a AuditOptions,
    min_target_px: f32,
    findings: Vec<AuditFinding>,
    exemptions: Vec<AuditExemptionUse>,
}

/// Roles that exist only to hold other things: a drag surface there is not a control to name.
fn is_container(role: Option<A11yRole>) -> bool {
    matches!(
        role,
        Some(A11yRole::Window | A11yRole::ScrollView | A11yRole::List | A11yRole::Dialog | A11yRole::Region)
    )
}

impl Rules<'_> {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        rule: AuditRule,
        severity: AuditSeverity,
        node: &SceneNode,
        measured: String,
        required: String,
        hint: impl Into<String>,
        needs_eyes: bool,
    ) {
        self.findings.push(AuditFinding {
            rule,
            severity,
            node: node.id,
            path: node.path.clone(),
            measured,
            required,
            criterion: rule.criterion(),
            hint: hint.into(),
            needs_eyes,
        });
    }

    fn exempt(&mut self, rule: AuditRule, node: &SceneNode, exemption: &'static str) {
        self.exemptions.push(AuditExemptionUse {
            rule,
            path: node.path.clone(),
            exemption,
        });
    }

    fn unnamed(&mut self) {
        let rule = AuditRule::UnnamedInteractive;
        let scene = self.scene;
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            if !(node.focusable || node.interactable) || is_container(node.role) && !node.focusable {
                continue;
            }
            if node.decoration && !node.focusable {
                self.exempt(rule, node, "decoration");
                continue;
            }
            let name = node.name.as_deref().unwrap_or("").trim();
            // The localization table's own marker for a missing key is not a name.
            let missing_key = name.starts_with('\u{27e6}');
            if !name.is_empty() && !missing_key {
                continue;
            }
            // A control is an error; a bare pointer surface with no role may be a gesture catcher.
            let severity = if node.focusable || node.role.is_some() {
                AuditSeverity::Error
            } else {
                AuditSeverity::Warning
            };
            self.add(
                rule,
                severity,
                node,
                if missing_key {
                    format!("name is a missing localization key: {name}")
                } else {
                    "no accessible name".to_owned()
                },
                "a non-empty name".to_owned(),
                "give the node a Text child or Accessible::with_label",
                false,
            );
        }
    }

    fn text_contrast(&mut self) {
        let rule = AuditRule::TextContrast;
        let scene = self.scene;
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            let Some(text) = &node.text else { continue };
            if node.disabled {
                self.exempt(rule, node, "inactive_component");
                continue;
            }
            if node.decoration {
                self.exempt(rule, node, "decoration");
                continue;
            }
            if node.alpha <= CLEAR_ALPHA {
                self.exempt(rule, node, "not_visible");
                continue;
            }
            let ink = intersect(text.ink, node.clip);
            if area(ink) <= 0.0 {
                continue;
            }
            let upper = scene.layers_below(node.key, true);
            let Some(overlays) = scene.overlays(node.group, ink, upper) else {
                self.exempt(rule, node, "not_visible");
                continue;
            };
            let required = |s: &SpanLook| if s.large { NONTEXT_CONTRAST } else { TEXT_CONTRAST };
            let tightest = text.spans.iter().map(required).fold(0.0, f32::max);
            let backdrop = match scene.backdrop(node.group, ink, upper) {
                Backdrop::Known(colors) => colors,
                Backdrop::Unknown(why) => {
                    let measured = match text.outline {
                        Some(_) => format!("unknown: {why}; the text has an outline"),
                        None => format!("unknown: {why}"),
                    };
                    self.add(
                        rule,
                        AuditSeverity::Info,
                        node,
                        measured,
                        ratio_text(tightest),
                        "look at it on the real background, or give the text a solid panel or an outline",
                        true,
                    );
                    continue;
                }
            };
            // The worst span against the worst stop; and how many stops it fails on.
            let mut worst: Option<(f32, f32)> = None;
            let mut stops_failing = 0;
            for &bg in &backdrop {
                let bg = overlays.iter().fold(bg, |under, (c, a)| over(*c, *a, under));
                let mut failed = false;
                for span in &text.spans {
                    let need = required(span);
                    let fg = overlays
                        .iter()
                        .fold(over(span.color, node.alpha, bg), |under, (c, a)| {
                            over(*c, *a, under)
                        });
                    let ratio = contrast_ratio(fg, bg);
                    failed |= ratio < need;
                    if worst.is_none_or(|(r, n)| ratio / need < r / n) {
                        worst = Some((ratio, need));
                    }
                }
                stops_failing += usize::from(failed);
            }
            let Some((ratio, need)) = worst else { continue };
            if stops_failing == 0 {
                continue;
            }
            let on_gradient = backdrop.len() > 1;
            let outline_edge = text.outline.is_some_and(|c| {
                text.spans
                    .iter()
                    .any(|s| contrast_ratio(s.color, c) >= required(s))
            });
            let (severity, needs_eyes) = if outline_edge {
                (AuditSeverity::Info, true)
            } else if on_gradient && stops_failing < backdrop.len() {
                (AuditSeverity::Warning, true)
            } else {
                (AuditSeverity::Error, false)
            };
            let hint = if outline_edge {
                "the outline gives the glyphs an edge; check it by eye"
            } else if on_gradient {
                "part of the gradient is too close to the text color: change either"
            } else {
                "change the text color or its background, or use the theme's text tokens"
            };
            self.add(
                rule,
                severity,
                node,
                ratio_text(ratio),
                ratio_text(need),
                hint,
                needs_eyes,
            );
        }
    }

    fn nontext_contrast(&mut self) {
        let rule = AuditRule::NontextContrast;
        let scene = self.scene;
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            let control = node.focusable || node.needs_boundary.is_some();
            if !control || area(intersect(node.rect, node.clip)) <= 0.0 {
                continue;
            }
            if node.disabled {
                self.exempt(rule, node, "inactive_component");
                continue;
            }
            let under = scene.layers_below(node.key, false);
            let surroundings = scene.backdrop(node.group, node.rect, under);
            self.ring_contrast(node, &surroundings);
            self.boundary_contrast(node, &surroundings);
        }
    }

    fn ring_contrast(&mut self, node: &SceneNode, surroundings: &Backdrop) {
        let rule = AuditRule::NontextContrast;
        let Some(ring) = node.themed.and_then(|t| t.focus_ring) else {
            return; // `a11y_no_focus_indicator` reports a missing ring.
        };
        let width = node
            .themed
            .and_then(|t| t.focus_ring_width)
            .unwrap_or(DEFAULT_FOCUS_RING_WIDTH);
        if width <= 0.0 {
            return;
        }
        // A ring drawn inside the node (a list row's negative offset) sits on the node's own fill.
        let inset = node.themed.and_then(|t| t.focus_ring_offset).unwrap_or(0.0) < 0.0;
        let scene = self.scene;
        let backdrop = if inset {
            scene.backdrop(node.group, node.rect, scene.layers_below(node.key, true))
        } else {
            surroundings.clone()
        };
        let need = ratio_text(NONTEXT_CONTRAST);
        match backdrop {
            Backdrop::Unknown(why) => self.add(
                rule,
                AuditSeverity::Info,
                node,
                format!("focus ring on unknown: {why}"),
                need,
                "look at the focused node on the real background",
                true,
            ),
            Backdrop::Known(colors) => {
                let worst = colors
                    .iter()
                    .map(|bg| contrast_ratio(over(ring, node.alpha, *bg), *bg))
                    .fold(f32::MAX, f32::min);
                if worst < NONTEXT_CONTRAST {
                    self.add(
                        rule,
                        AuditSeverity::Error,
                        node,
                        format!("focus ring {}", ratio_text(worst)),
                        need,
                        "change `color.focus-ring` or the surface behind the control",
                        false,
                    );
                }
            }
        }
    }

    fn boundary_contrast(&mut self, node: &SceneNode, surroundings: &Backdrop) {
        let rule = AuditRule::NontextContrast;
        if node.needs_boundary.is_none() && node.shows_text {
            self.exempt(rule, node, "text_identifies_control");
            return;
        }
        if is_container(node.role) {
            return;
        }
        let need = ratio_text(NONTEXT_CONTRAST);
        let colors = match surroundings {
            Backdrop::Known(colors) => colors,
            Backdrop::Unknown(why) => {
                self.add(
                    rule,
                    AuditSeverity::Info,
                    node,
                    format!("boundary on unknown: {why}"),
                    need,
                    "look at the control on the real background",
                    true,
                );
                return;
            }
        };
        if node.shows_image && node.needs_boundary.is_none() {
            // An icon button: the icon's pixels identify it, and the audit does not read pixels.
            self.add(
                rule,
                AuditSeverity::Info,
                node,
                "an icon identifies the control".to_owned(),
                need,
                "check that the icon stands out from what is behind it",
                true,
            );
            return;
        }
        let border = node.border.map(|(c, _)| c);
        let knob = node.has_knob.then_some(UiColor::WHITE);
        let parts = [border, node.fill, knob];
        // The best part around the worst stop of what surrounds the control.
        let best = colors
            .iter()
            .map(|bg| {
                parts
                    .iter()
                    .flatten()
                    .map(|c| contrast_ratio(over(*c, node.alpha, *bg), *bg))
                    .fold(1.0, f32::max)
            })
            .fold(f32::MAX, f32::min);
        if best < NONTEXT_CONTRAST {
            let kind = node.needs_boundary.unwrap_or("control");
            self.add(
                rule,
                AuditSeverity::Error,
                node,
                format!("{kind} boundary {}", ratio_text(best)),
                need,
                "give the control a border, or a fill that stands out from the surface behind it",
                false,
            );
        }
    }

    fn target_size(&mut self) {
        let rule = AuditRule::TargetSize;
        let min = self.min_target_px;
        let scene = self.scene;
        let targets: Vec<&SceneNode> = scene
            .nodes
            .iter()
            .filter(|n| {
                (n.focusable || n.interactable)
                    && !n.disabled
                    && !(is_container(n.role) && !n.focusable)
                    && area(intersect(n.hit, n.clip)) > 0.0
            })
            .collect();
        let touch = self.options.profile == AuditProfile::Touch;
        // A fingertip has a physical size: under the touch profile a target is measured in
        // device-independent px (physical px / the OS scale factor), not UI px, which a reference
        // scale can shrink below the density (ADR-4932). Without a known factor, UI px.
        let density = (scene.density.is_finite() && scene.density > 0.0).then_some(scene.density);
        let unit = |n: &SceneNode| match density {
            Some(d) if touch => d,
            _ => n.scale,
        };
        let small = |n: &SceneNode| n.hit[2] / unit(n) + 0.01 < min || n.hit[3] / unit(n) + 0.01 < min;
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            if node.disabled && (node.focusable || node.interactable) && small(node) {
                self.exempt(rule, node, "inactive_component");
            }
        }
        for node in targets.iter().copied().filter(|n| n.in_scope && small(n)) {
            if let Some(exemption) = node.target_exemption {
                self.exempt(rule, node, exemption.name());
                continue;
            }
            if !touch && spaced_apart(node, &targets, min, &small) {
                self.exempt(rule, node, "spacing");
                continue;
            }
            self.add(
                rule,
                AuditSeverity::Error,
                node,
                format!(
                    "{:.1}x{:.1} px",
                    node.hit[2] / unit(node),
                    node.hit[3] / unit(node)
                ),
                format!("{min:.1}x{min:.1} px"),
                "make the hit box larger; it can be larger than what it draws (`TouchTarget`)",
                false,
            );
        }
    }

    fn focus_unreachable(&mut self) {
        let rule = AuditRule::FocusUnreachable;
        let scene = self.scene;
        let by_id: HashMap<u64, &SceneNode> = scene.nodes.iter().map(|n| (n.id, n)).collect();
        for problem in &scene.nav {
            let Some(node) = by_id.get(&problem.node).copied().filter(|n| n.in_scope) else {
                continue;
            };
            let (severity, hint) = match problem.kind {
                NavIssueKind::DeadEnd => (
                    AuditSeverity::Warning,
                    "add an explicit link or a focus group edge that leaves this node",
                ),
                _ => (
                    AuditSeverity::Error,
                    "add an explicit link or put the node in a focus group that reaches it",
                ),
            };
            self.add(
                rule,
                severity,
                node,
                problem.kind.code().to_owned(),
                "reachable by navigation".to_owned(),
                hint,
                false,
            );
        }
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            let control = matches!(
                node.role,
                Some(
                    A11yRole::Button
                        | A11yRole::CheckBox
                        | A11yRole::Slider
                        | A11yRole::TextInput
                        | A11yRole::ComboBox
                )
            );
            if control && node.interactable && !node.focusable && !node.disabled {
                self.add(
                    rule,
                    AuditSeverity::Error,
                    node,
                    "not_focusable".to_owned(),
                    "reachable by navigation".to_owned(),
                    "add Focusable: this control takes clicks but no keyboard or gamepad can reach it",
                    false,
                );
            }
        }
    }

    fn no_focus_indicator(&mut self) {
        let rule = AuditRule::NoFocusIndicator;
        let scene = self.scene;
        for node in scene.nodes.iter().filter(|n| n.in_scope && n.focusable) {
            if node.disabled {
                self.exempt(rule, node, "inactive_component");
                continue;
            }
            if scene.ring_policy == FocusRingPolicy::Never {
                self.add(
                    rule,
                    AuditSeverity::Info,
                    node,
                    "FocusRingPolicy::Never: the game draws its own".to_owned(),
                    "a visible focus indicator".to_owned(),
                    "focus the node and check that the game's own indicator shows",
                    true,
                );
                continue;
            }
            let ring = node.themed.and_then(|t| {
                let color = t.focus_ring?;
                let width = t.focus_ring_width.unwrap_or(DEFAULT_FOCUS_RING_WIDTH);
                (width > 0.0 && color.0[3] * node.alpha > CLEAR_ALPHA).then_some((width, t))
            });
            let Some((width, themed)) = ring else {
                if node.has_motion_preset {
                    self.add(
                        rule,
                        AuditSeverity::Info,
                        node,
                        "no ring; a motion preset moves on focus".to_owned(),
                        "a visible focus indicator".to_owned(),
                        "check that the pose alone shows focus, or give the node a theme class",
                        true,
                    );
                } else {
                    self.add(
                        rule,
                        AuditSeverity::Error,
                        node,
                        "no ring".to_owned(),
                        "a visible focus indicator".to_owned(),
                        "give the node a theme class (Themed), or a `focus_ring` property with a width",
                        false,
                    );
                }
                continue;
            };
            // A band wholly outside the clip draws nothing; a partly clipped one still shows.
            let geometry = focus_ring_geometry(
                node.rect,
                themed.radius.map_or(0.0, |r| r * node.scale),
                width * node.scale,
                themed.focus_ring_offset.unwrap_or(0.0) * node.scale,
            );
            if let Some(g) = geometry {
                let inner = [
                    g.rect[0] + g.width,
                    g.rect[1] + g.width,
                    g.rect[2] - 2.0 * g.width,
                    g.rect[3] - 2.0 * g.width,
                ];
                if escape(intersect(g.rect, node.clip), inner) <= 0.0
                    && area(intersect(node.rect, node.clip)) > 0.0
                {
                    self.add(
                        rule,
                        AuditSeverity::Error,
                        node,
                        "the clip of an ancestor cuts the whole ring".to_owned(),
                        "a visible focus indicator".to_owned(),
                        "use an inset ring (`border.width.focus-inset`) or give the node room inside the clip",
                        false,
                    );
                }
            }
        }
    }

    fn text_clipped(&mut self) {
        let rule = AuditRule::TextClippedAtScale;
        let scene = self.scene;
        let percent = (self.options.effective_text_scale() * 100.0).round();
        for &(i, exemption) in &scene.clip_exemptions {
            if let Some(node) = scene.nodes.get(i).filter(|n| n.in_scope) {
                self.exempt(rule, node, exemption);
            }
        }
        for clip in &scene.clips {
            let Some(node) = scene.nodes.get(clip.node).filter(|n| n.in_scope) else {
                continue;
            };
            let suffix = if clip.at_full_size { " (at 100 % too)" } else { "" };
            self.add(
                rule,
                clip.severity,
                node,
                format!("{}{suffix}", clip.measured),
                format!("no text lost at {percent:.0} %"),
                clip.hint,
                false,
            );
        }
    }

    fn motion(&mut self) {
        let rule = AuditRule::MotionIgnoresSetting;
        let scene = self.scene;
        let fast = scene.motion_fast;
        for node in scene.nodes.iter().filter(|n| n.in_scope) {
            for defect in &node.motion_defects {
                let (measured, required, hint) = match defect {
                    MotionDefect::Transform(property, secs) => (
                        format!("{} moves for {secs:.2} s", property.name()),
                        "0 s under reduced motion".to_owned(),
                        "clear follows_reduced_motion, or plan this property with no duration",
                    ),
                    MotionDefect::LongFade(secs) => (
                        format!("fade of {secs:.2} s"),
                        format!("at most {fast:.2} s under reduced motion"),
                        "cap the fade at `motion.fast`, or clear follows_reduced_motion",
                    ),
                };
                self.add(rule, AuditSeverity::Error, node, measured, required, hint, false);
            }
        }
    }
}

/// SC 2.5.8's spacing exception: a circle as wide as the minimum, centered on the target, touches no
/// other target and no other undersized target's circle. The target is the hit box (`TouchTarget`
/// grows it past the drawn rect); measuring the drawn rect lets a circle miss a neighbor it overlaps.
fn spaced_apart(
    node: &SceneNode,
    targets: &[&SceneNode],
    min: f32,
    small: &dyn Fn(&SceneNode) -> bool,
) -> bool {
    let radius = min * node.scale * 0.5;
    let center = [node.hit[0] + node.hit[2] * 0.5, node.hit[1] + node.hit[3] * 0.5];
    targets.iter().filter(|o| o.id != node.id).all(|other| {
        // The distance from the center to the other hit box.
        let dx = (other.hit[0] - center[0])
            .max(center[0] - (other.hit[0] + other.hit[2]))
            .max(0.0);
        let dy = (other.hit[1] - center[1])
            .max(center[1] - (other.hit[1] + other.hit[3]))
            .max(0.0);
        if dx * dx + dy * dy < radius * radius {
            return false;
        }
        if small(other) {
            let c = [
                other.hit[0] + other.hit[2] * 0.5,
                other.hit[1] + other.hit[3] * 0.5,
            ];
            let (x, y) = (c[0] - center[0], c[1] - center[1]);
            let both = radius + min * other.scale * 0.5;
            return x * x + y * y >= both * both;
        }
        true
    })
}

#[cfg(test)]
#[path = "a11y_audit_tests.rs"]
mod tests;
