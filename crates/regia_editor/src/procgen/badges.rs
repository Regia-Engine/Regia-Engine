// SPDX-License-Identifier: MPL-2.0
//! Pin colors and node badges of the procedural graph canvas (spec 67 § 2.2): one theme color per
//! data kind (`ThemeTokens::data_kinds`, a colorblind-safe default set), multi-kind pins striped,
//! and the badges a node carries: bypassed, visualized, inspected, tier (spec 66, the tier's cell
//! size), cache hit, warning count and error. The canvas and `editor.procgen_badges` read the same
//! [`node_badges`].

use std::collections::{BTreeMap, BTreeSet};

use egui::Color32;
use egui_snarl::ui::{PinWireInfo, SnarlPin, SnarlStyle};
use regia_ecs::prelude::*;
use regia_procgen::{DataKind, KindSet};

use super::debug_view::ProcPreview;
use super::doc::{self, ProcGraphDoc};
use super::inspect::ProcInspect;
use crate::theme::ThemeTokens;

/// The color of a pin that takes every kind (or none): one neutral, not ten stripes.
pub const ANY_KIND: Color32 = Color32::from_gray(170);

/// The colors a pin of `kinds` draws: one per kind in `DataKind::ALL` order (a multi-kind pin is
/// striped), or [`ANY_KIND`] alone for a pin of every kind or of none.
#[must_use]
pub fn pin_colors(tokens: &ThemeTokens, kinds: KindSet) -> Vec<Color32> {
    let (colors, n) = pin_stripes(tokens, kinds);
    colors[..n].to_vec()
}

/// [`pin_colors`] without allocating: the colors and how many are used.
#[must_use]
pub fn pin_stripes(tokens: &ThemeTokens, kinds: KindSet) -> ([Color32; 10], usize) {
    let mut out = [ANY_KIND; 10];
    let mut n = 0;
    for (i, k) in DataKind::ALL.iter().enumerate() {
        if kinds.contains(*k) {
            out[n] = tokens.data_kinds[i];
            n += 1;
        }
    }
    if n == 0 || n == DataKind::ALL.len() {
        return ([ANY_KIND; 10], 1);
    }
    (out, n)
}

/// A canvas pin: a circle of one color, or a square of vertical stripes for a multi-kind pin.
/// Drawing it allocates nothing.
#[derive(Clone, Copy, Debug)]
pub struct StripedPin {
    colors: [Color32; 10],
    n: usize,
}

impl StripedPin {
    /// The pin of `kinds` under `tokens`.
    #[must_use]
    pub fn new(tokens: &ThemeTokens, kinds: KindSet) -> Self {
        let (colors, n) = pin_stripes(tokens, kinds);
        Self { colors, n }
    }
}

impl SnarlPin for StripedPin {
    fn draw(
        self,
        snarl_style: &SnarlStyle,
        style: &egui::Style,
        rect: egui::Rect,
        painter: &egui::Painter,
    ) -> PinWireInfo {
        let stroke = egui::Stroke::new(1.0, style.visuals.widgets.noninteractive.fg_stroke.color);
        let size = rect.width().min(rect.height());
        if self.n <= 1 {
            painter.circle(rect.center(), size / 2.0, self.colors[0], stroke);
        } else {
            let square = egui::Rect::from_center_size(rect.center(), egui::vec2(size, size));
            let w = square.width() / self.n as f32;
            for (k, c) in self.colors[..self.n].iter().enumerate() {
                let x0 = square.left() + w * k as f32;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0, square.top()),
                        egui::pos2(x0 + w, square.bottom()),
                    ),
                    0.0,
                    *c,
                );
            }
            painter.rect_stroke(square, 0.0, stroke, egui::StrokeKind::Outside);
        }
        PinWireInfo {
            color: self.colors[0],
            style: snarl_style
                .wire_style
                .unwrap_or(egui_snarl::ui::WireStyle::Bezier5),
        }
    }
}

/// The tier a node runs at in a tiled preview target (spec 66 § 3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TierBadge {
    /// Tier index (whole first, then coarse to fine). For a shared node, the coarsest tier it feeds.
    pub index: u8,
    /// The tier's cell size, meters (`None` for the whole tier).
    pub cell_m: Option<f64>,
    /// A tier-neutral node (a source, or fed only by sources) that runs in every tier consuming
    /// it, here more than one (post05w4).
    pub shared: bool,
    /// The tiers the node runs in, one bit per tier index (bit 0 = tier 0); a node placed at one
    /// tier has only its own bit.
    pub feeds: u16,
    /// A tier-neutral node (no tier of its own: it adopts its consumers' tiers), shared or not.
    /// A neutral node that feeds one tier is marked `~` before that tier's cell size
    /// (w11-biome-procgen-tooling item 5), so it does not read as a node placed at that tier.
    pub neutral: bool,
}

impl TierBadge {
    /// The tier indices of [`Self::feeds`], ascending.
    #[must_use]
    pub fn feeds_list(&self) -> Vec<u8> {
        (0u8..16).filter(|t| self.feeds & (1 << t) != 0).collect()
    }

    /// The badge's text: the tier's cell size (`whole`, `64 m`); a shared node names every tier it
    /// runs in by index (`T0+T1`); a tier-neutral node that feeds one tier gets `~` before its
    /// consumer's cell size (`~64 m`).
    #[must_use]
    pub fn label(&self) -> String {
        if self.shared {
            self.feeds_list()
                .iter()
                .map(|t| format!("T{t}"))
                .collect::<Vec<_>>()
                .join("+")
        } else {
            let size = self
                .cell_m
                .map_or_else(|| "whole".to_string(), |c| format!("{c} m"));
            if self.neutral { format!("~{size}") } else { size }
        }
    }
}

/// One badge on a node.
#[derive(Clone, Debug, PartialEq)]
pub enum Badge {
    /// `enabled: false` (a gray stripe).
    Bypassed,
    /// Visualized in the viewport (eye).
    Visualized,
    /// Shown in the data table.
    Inspected,
    /// The node's tier in a tiled preview target.
    Tier(TierBadge),
    /// Served from the cache in the last preview run (a small dot).
    CacheHit,
    /// Warnings of the last run and the compiler, counted.
    Warnings(usize),
    /// A compile error (or the last run's failure) on this node.
    Error {
        /// The stable code.
        code: String,
        /// The message.
        message: String,
    },
}

impl Badge {
    /// The badge's agent name (`bypassed`, `visualized`, `inspected`, `tier`, `cache_hit`,
    /// `warnings`, `error`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bypassed => "bypassed",
            Self::Visualized => "visualized",
            Self::Inspected => "inspected",
            Self::Tier(_) => "tier",
            Self::CacheHit => "cache_hit",
            Self::Warnings(_) => "warnings",
            Self::Error { .. } => "error",
        }
    }
}

/// Every badge of one node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeBadges {
    /// The badges, in the order above.
    pub list: Vec<Badge>,
    /// Warnings counted (0 when none).
    pub warnings: usize,
    /// The node's tier, when the preview target is tiled.
    pub tier: Option<TierBadge>,
    /// The error `(code, message)`, when there is one.
    pub error: Option<(String, String)>,
    /// Served from the cache in the last run.
    pub cache_hit: bool,
}

/// The tiers each compiled node runs in, one bit per tier index: a placed node its own tier; a
/// tier-neutral node every tier of its consumers (through other neutral nodes), as
/// `TierPlan::build`'s backward pass places it; tier 0 when nothing consumes it.
fn tier_feeds(compiled: &regia_procgen::CompiledGraph, plan: &regia_procgen::tiles::TierPlan) -> Vec<u16> {
    let n = compiled.nodes.len();
    let bit = |t: u8| if t < 16 { 1u16 << t } else { 0 };
    let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, node) in compiled.nodes.iter().enumerate() {
        for &(src, _) in node.inputs.iter().flatten() {
            if let Some(list) = consumers.get_mut(src) {
                list.push(k);
            }
        }
    }
    let mut feeds = vec![0u16; n];
    // Evaluation order is topological: consumers come after their sources.
    for k in (0..n).rev() {
        feeds[k] = match plan.node_tier(k) {
            Some(t) => bit(t),
            None => {
                let mask = consumers[k].iter().fold(0u16, |m, &c| m | feeds[c]);
                if mask == 0 { bit(0) } else { mask }
            }
        };
    }
    feeds
}

/// The tier of each node of `graph` for a tiled generator with base cell `cell_m`.
fn tiers(world: &World, graph: &regia_procgen::ProcGraph, cell_m: f64) -> BTreeMap<u32, TierBadge> {
    let registry = doc::registry(world);
    let Ok(compiled) = graph.compile(&registry) else {
        return BTreeMap::new();
    };
    let Ok(plan) = regia_procgen::tiles::TierPlan::build(&compiled, cell_m) else {
        return BTreeMap::new();
    };
    let feeds = tier_feeds(&compiled, &plan);
    compiled
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(k, n)| {
            // A tier-neutral node (a source, or fed only by sources) runs in every tier that
            // consumes it: its badge names them all (`shared`), its index is the coarsest.
            let mask = feeds.get(k).copied().unwrap_or(0);
            let index = match plan.node_tier(k) {
                Some(t) => t,
                None => (0u8..16).find(|t| mask & (1 << t) != 0).unwrap_or(0),
            };
            let t = plan.tier(index)?;
            Some((
                n.id,
                TierBadge {
                    index: t.index,
                    cell_m: t.cell_m,
                    shared: plan.node_tier(k).is_none() && mask.count_ones() > 1,
                    feeds: mask,
                    neutral: plan.node_tier(k).is_none(),
                },
            ))
        })
        .collect()
}

/// The badges of the open graph's nodes (`only`: just these ids), read from the document, the
/// preview target's last run report and tiling, the visualize set and the inspected node.
#[must_use]
pub fn node_badges(world: &World, only: Option<&BTreeSet<u32>>) -> BTreeMap<u32, NodeBadges> {
    let _span = tracing::info_span!(target: "regia_editor", "editor.procgen_badges").entered();
    let Some(d) = world.get_resource::<ProcGraphDoc>() else {
        return BTreeMap::new();
    };
    let preview = world.get_resource::<ProcPreview>();
    let target = preview.and_then(|p| p.target);
    let state = target.and_then(|t| world.get::<regia_procgen::world::ProcGenState>(t));
    let report = state.and_then(|s| s.report.as_ref());
    let tiered = target
        .and_then(|t| world.get::<regia_procgen::world::ProcGenerator>(t))
        .and_then(|g| g.tiles.as_ref())
        .map(|ts| tiers(world, &d.graph, f64::from(ts.cell_m)))
        .unwrap_or_default();
    let compile_warnings: Vec<(u32, usize)> = {
        let registry = doc::registry(world);
        match d.graph.compile(&registry) {
            Ok(c) => c.warnings.iter().filter_map(|w| w.node.map(|n| (n, 1))).collect(),
            Err(_) => Vec::new(),
        }
    };
    let inspected = world.get_resource::<ProcInspect>().and_then(|i| i.node);
    let run_error = state.and_then(|s| s.last_error.as_ref());
    d.graph
        .nodes
        .iter()
        .filter(|(id, _)| only.is_none_or(|o| o.contains(id)))
        .map(|(id, n)| {
            let mut b = NodeBadges::default();
            if !n.enabled {
                b.list.push(Badge::Bypassed);
            }
            if preview.is_some_and(|p| p.visualize.contains_key(id)) {
                b.list.push(Badge::Visualized);
            }
            if inspected == Some(*id) {
                b.list.push(Badge::Inspected);
            }
            if let Some(t) = tiered.get(id) {
                b.tier = Some(*t);
                b.list.push(Badge::Tier(*t));
            }
            let line = report.and_then(|r| r.nodes.iter().find(|x| x.id == *id));
            if line.is_some_and(|l| l.cache_hit) {
                b.cache_hit = true;
                b.list.push(Badge::CacheHit);
            }
            b.warnings = line.map_or(0, |l| l.warnings.len())
                + compile_warnings
                    .iter()
                    .filter(|(n, _)| n == id)
                    .map(|(_, c)| c)
                    .sum::<usize>();
            if b.warnings > 0 {
                b.list.push(Badge::Warnings(b.warnings));
            }
            let err = d
                .errors
                .iter()
                .find(|e| e.node == Some(*id))
                .map(|e| (e.code.clone(), e.message.clone()))
                .or_else(|| {
                    run_error
                        .filter(|(node, _, _)| *node == Some(*id))
                        .map(|(_, c, m)| (c.clone(), m.clone()))
                });
            if let Some((code, message)) = err {
                b.list.push(Badge::Error {
                    code: code.clone(),
                    message: message.clone(),
                });
                b.error = Some((code, message));
            }
            (*id, b)
        })
        .collect()
}

/// `"#rrggbb"` of a color (the theme's JSON form).
#[must_use]
pub fn hex(c: Color32) -> String {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}
