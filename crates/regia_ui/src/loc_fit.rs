// SPDX-License-Identifier: MPL-2.0
//! Headless text-fit checking for `.ui.ron` documents (ADR-3359).
//!
//! [`check_document`] lays out every text node of one parsed `.ui.ron` document through the same
//! shaping ([`crate::text::TextSystem::shape`]) and the same taffy style conversion
//! ([`crate::style::Style::to_taffy_dir`]) the runtime layout uses, against a given viewport
//! size, without a GPU or an ECS world, and reports each text whose laid-out extent exceeds its
//! content box. A translation that does not fit its box is found before it ships.
//!
//! What is checked: nodes with `text`, honouring wrap, line height, padding and border, in LTR or
//! RTL per the locale. `overflow: ellipsis` and `overflow: scale_to_fit` nodes are skipped (the
//! engine adapts the text); `visible` and `clip` report — in both cases a player loses text —
//! with the measured excess in physical px. Widget chrome (button padding applied at spawn) and
//! virtualized lists are not modelled: the check is about authored text boxes.

use std::collections::HashMap;

use crate::authoring::{LocSpan, TextDef, TextSpanDef, UiFile, UiMigrators, UiNodeDef, parse_ui_ron};
use crate::layout::TextMeasure;
use crate::text::{Text, TextOverflow, TextSpan, TextSystem};
use crate::{Localization, style};

/// A viewport size to check at, in physical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitSize {
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

/// Which axis overflowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FitKind {
    /// The laid-out text is wider than the content box.
    WidthOverflow,
    /// The laid-out text is taller than the content box.
    HeightOverflow,
}

/// One text node that does not fit.
#[derive(Debug, Clone, PartialEq)]
pub struct FitFinding {
    /// The document path, as passed to [`check_document`].
    pub document: String,
    /// The node's path (`root/row[1]/label`): names where present, else the 0-based index.
    pub node: String,
    /// The first `Loc` key of the node's text, when it has one.
    pub key: Option<String>,
    /// The locale the text was laid out in.
    pub locale: String,
    /// The viewport size checked at.
    pub size: FitSize,
    /// Which axis overflowed.
    pub kind: FitKind,
    /// The measured text extent on that axis, physical px.
    pub measured: f32,
    /// The content box on that axis, physical px.
    pub box_size: f32,
    /// `measured - box_size`.
    pub excess: f32,
}

/// The answer for one document, locale and size.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FitReport {
    /// Every text node that overflowed or clipped, sorted by node then kind.
    pub findings: Vec<FitFinding>,
    /// Text nodes that were laid out and fit.
    pub nodes_checked: usize,
}

/// The comparison slack, physical px: layout rounds node sizes to whole pixels, so a box can be
/// up to this much narrower than the text it just fit (the runtime uses the same bound).
const SLACK_PX: f32 = 0.75;

/// The taffy leaf measure: the pre-shaped text size, honouring known dimensions (a copy of the
/// runtime measure function, `layout.rs`).
fn measure_fn(
    inputs: taffy::tree::LayoutInput,
    _node: taffy::tree::NodeId,
    context: Option<&mut TextMeasure>,
    _style: &taffy::style::Style,
) -> taffy::tree::LayoutOutput {
    let m = context.map(|c| c.size).unwrap_or([0.0; 2]);
    let w = inputs.known_dimensions.width.unwrap_or(m[0]);
    let h = inputs.known_dimensions.height.unwrap_or(m[1]);
    taffy::tree::LayoutOutput {
        size: taffy::geometry::Size {
            width: w.max(0.0),
            height: h.max(0.0),
        },
        ..taffy::tree::LayoutOutput::DEFAULT
    }
}

/// A text node being laid out.
struct TextNode {
    /// The taffy node.
    node: taffy::tree::NodeId,
    /// The node's path in the document.
    path: String,
    /// The first `Loc` key, when present.
    key: Option<String>,
    /// The text (spans unresolved; shaping resolves per `loc`).
    text: Text,
    /// The natural (unwrapped) size from the first measure pass.
    natural: [f32; 2],
}

/// Converts an authored text block to the runtime [`Text`] (the same conversion the spawner
/// does).
fn text_of(def: &TextDef) -> Text {
    Text {
        spans: def
            .spans
            .iter()
            .map(|s| match s {
                TextSpanDef::Lit(text) => TextSpan::lit(text.clone()).with_style(def.style.clone()),
                TextSpanDef::Loc(LocSpan::Loc(key)) => {
                    TextSpan::loc(key.clone()).with_style(def.style.clone())
                }
                TextSpanDef::Full { text, style } => {
                    let mut span = TextSpan::lit(text.clone());
                    if let Some(key) = text.strip_prefix("loc:") {
                        span = TextSpan::loc(key.to_owned());
                    }
                    span.with_style(style.clone())
                }
            })
            .collect(),
        align: def.align,
        wrap: def.wrap,
        overflow: def.overflow,
        line_height: def.line_height,
    }
}

/// The cross axis a flex container stretches its items along (the runtime's `CrossAxis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossAxis {
    Width,
    Height,
}

fn flex_item_cross(tstyle: &taffy::style::Style) -> Option<CrossAxis> {
    use taffy::style::FlexDirection as D;
    if tstyle.display != taffy::style::Display::Flex {
        return None;
    }
    Some(match tstyle.flex_direction {
        D::Row | D::RowReverse => CrossAxis::Height,
        D::Column | D::ColumnReverse => CrossAxis::Width,
    })
}

/// Gives a flex item with an `Auto` cross minimum the used value the runtime floors it at (its
/// padding plus border): `layout.rs::floor_flex_item_cross_min`, re-derived here so the check
/// mirrors the runtime box exactly.
fn floor_flex_item_cross_min(tstyle: &mut taffy::style::Style, cross: Option<CrossAxis>) {
    let Some(cross) = cross else {
        return;
    };
    if tstyle.aspect_ratio.is_some() || tstyle.position == taffy::style::Position::Absolute {
        return;
    }
    let edges = match cross {
        CrossAxis::Width => [
            tstyle.padding.left,
            tstyle.padding.right,
            tstyle.border.left,
            tstyle.border.right,
        ],
        CrossAxis::Height => [
            tstyle.padding.top,
            tstyle.padding.bottom,
            tstyle.border.top,
            tstyle.border.bottom,
        ],
    };
    let floor = if tstyle.box_sizing == taffy::style::BoxSizing::BorderBox {
        edges
            .iter()
            .map(|e| match e.expand() {
                taffy::style::ExpandedLengthPercentage::Length(v) => v.max(0.0),
                _ => 0.0,
            })
            .sum()
    } else {
        0.0
    };
    let min = match cross {
        CrossAxis::Width => &mut tstyle.min_size.width,
        CrossAxis::Height => &mut tstyle.min_size.height,
    };
    if min.is_auto() {
        *min = taffy::style::LengthPercentageAuto::length(floor);
    }
}

/// Builds the taffy node of `def` and its subtree. `texts` collects text nodes with their
/// natural measures.
#[allow(clippy::too_many_arguments)]
fn build_node(
    tree: &mut taffy::TaffyTree<TextMeasure>,
    def: &UiNodeDef,
    path: &str,
    scale: f32,
    inherited_rtl: bool,
    cross: Option<CrossAxis>,
    texts: &mut Vec<TextNode>,
    text_system: &mut TextSystem,
    loc: &Localization,
) -> Result<taffy::tree::NodeId, taffy::TaffyError> {
    let node_rtl = def.style.text_dir.is_rtl(inherited_rtl);
    let mut tstyle = def.style.to_taffy_dir(scale, node_rtl);
    floor_flex_item_cross_min(&mut tstyle, cross);
    let child_cross = flex_item_cross(&tstyle);
    let mut text_node: Option<taffy::tree::NodeId> = None;
    if def.children.is_empty()
        && let Some(t) = &def.text
        && !t.spans.is_empty()
    {
        let text = text_of(t);
        let natural = text_system.shape(&text, scale, None, loc).size;
        let id = tree.new_leaf_with_context(tstyle.clone(), TextMeasure { size: natural })?;
        let key = t.spans.iter().find_map(|s| match s {
            TextSpanDef::Loc(LocSpan::Loc(k)) => Some(k.clone()),
            TextSpanDef::Full { text, .. } => text.strip_prefix("loc:").map(|k| k.to_owned()),
            TextSpanDef::Lit(_) => None,
        });
        texts.push(TextNode {
            node: id,
            path: path.to_owned(),
            key,
            text,
            natural,
        });
        text_node = Some(id);
    }
    if let Some(text_id) = text_node {
        return Ok(text_id);
    }
    let mut children = Vec::with_capacity(def.children.len());
    for (i, child) in def.children.iter().enumerate() {
        let seg = child.name.clone().unwrap_or_else(|| format!("{i}"));
        let child_path = format!("{path}/{seg}");
        children.push(build_node(
            tree,
            child,
            &child_path,
            scale,
            node_rtl,
            child_cross,
            texts,
            text_system,
            loc,
        )?);
    }
    tree.new_with_children(tstyle, &children)
}

/// Lays out `file` at `size` in `loc`'s selected locale and reports every text node whose text
/// overflows or clips its content box. `document` names the file in findings; `scale` is the UI
/// scale (physical px per UI px, 1.0 on desktop).
///
/// # Errors
///
/// taffy failed to build or compute the tree (a style value it rejects).
pub fn check_document(
    document: &str,
    file: &UiFile,
    loc: &Localization,
    text_system: &mut TextSystem,
    size: FitSize,
    scale: f32,
) -> Result<FitReport, taffy::TaffyError> {
    let _span = tracing::debug_span!("loc_fit", document, locale = loc.locale()).entered();
    let locale_rtl = style::locale_is_rtl(loc.locale());
    let mut tree: taffy::TaffyTree<TextMeasure> = taffy::TaffyTree::new();
    let mut texts = Vec::new();
    // The root fills the viewport, as `run_ui_layout` frames it.
    let mut root_style = file.root.style.to_taffy_dir(scale, locale_rtl);
    root_style.size.width = taffy::style::Dimension::percent(1.0);
    root_style.size.height = taffy::style::Dimension::percent(1.0);
    let mut root_children = Vec::new();
    for (i, child) in file.root.children.iter().enumerate() {
        let seg = child.name.clone().unwrap_or_else(|| format!("{i}"));
        root_children.push(build_node(
            &mut tree,
            child,
            &format!("root/{seg}"),
            scale,
            locale_rtl,
            None,
            &mut texts,
            text_system,
            loc,
        )?);
    }
    let root = if file.root.children.is_empty()
        && let Some(t) = &file.root.text
    {
        let text = text_of(t);
        let natural = text_system.shape(&text, scale, None, loc).size;
        let id = tree.new_leaf_with_context(root_style, TextMeasure { size: natural })?;
        let key = t.spans.iter().find_map(|s| match s {
            TextSpanDef::Loc(LocSpan::Loc(k)) => Some(k.clone()),
            TextSpanDef::Full { text, .. } => text.strip_prefix("loc:").map(|k| k.to_owned()),
            TextSpanDef::Lit(_) => None,
        });
        texts.push(TextNode {
            node: id,
            path: "root".to_owned(),
            key,
            text,
            natural,
        });
        id
    } else {
        tree.new_with_children(root_style, &root_children)?
    };
    let available = taffy::geometry::Size {
        width: taffy::style::AvailableSpace::Definite(size.width),
        height: taffy::style::AvailableSpace::Definite(size.height),
    };
    tree.compute_layout_with_measure(root, available, measure_fn)?;
    // Refinement pass (the runtime's `refine_wrapped_text`): re-shape texts at their final
    // content width; wrapped heights may change the layout, so recompute when they do.
    let mut changed = false;
    for t in &texts {
        let Ok(layout) = tree.layout(t.node) else {
            continue;
        };
        let Ok(tstyle) = tree.style(t.node) else {
            continue;
        };
        let [pad_l, pad_r] = pad_x(tstyle);
        let content_w = layout.size.width - pad_l - pad_r;
        if (content_w - t.natural[0]).abs() < SLACK_PX {
            continue;
        }
        let shaped = text_system
            .shape(&t.text, scale, Some(content_w.max(1.0)), loc)
            .size;
        if t.text.wrap != crate::text::WrapMode::None && (shaped[1] - t.natural[1]).abs() > SLACK_PX {
            changed = true;
        }
        tree.set_node_context(t.node, Some(TextMeasure { size: shaped }))?;
    }
    if changed {
        tree.compute_layout_with_measure(root, available, measure_fn)?;
    }
    let mut report = FitReport::default();
    for t in &texts {
        if t.text.overflow == TextOverflow::Ellipsis || t.text.overflow == TextOverflow::ScaleToFit {
            continue;
        }
        let Ok(layout) = tree.layout(t.node) else {
            continue;
        };
        let Ok(tstyle) = tree.style(t.node) else {
            continue;
        };
        let [pad_l, pad_r] = pad_x(tstyle);
        let [pad_t, pad_b] = pad_y(tstyle);
        let content_w = (layout.size.width - pad_l - pad_r).max(0.0);
        let content_h = (layout.size.height - pad_t - pad_b).max(0.0);
        let measure = tree.get_node_context(t.node).map(|m| m.size).unwrap_or(t.natural);
        // Width: with wrapping, `shape` already wrapped at the content width, so a wider result
        // is an unbreakable word sticking out; without wrapping it is the plain natural width.
        if measure[0] > content_w + SLACK_PX {
            report.findings.push(FitFinding {
                document: document.to_owned(),
                node: t.path.clone(),
                key: t.key.clone(),
                locale: loc.locale().to_owned(),
                size,
                kind: FitKind::WidthOverflow,
                measured: measure[0],
                box_size: content_w,
                excess: measure[0] - content_w,
            });
        }
        if measure[1] > content_h + SLACK_PX {
            report.findings.push(FitFinding {
                document: document.to_owned(),
                node: t.path.clone(),
                key: t.key.clone(),
                locale: loc.locale().to_owned(),
                size,
                kind: FitKind::HeightOverflow,
                measured: measure[1],
                box_size: content_h,
                excess: measure[1] - content_h,
            });
        }
        report.nodes_checked += 1;
    }
    report
        .findings
        .sort_by(|a, b| (a.node.as_str(), a.kind).cmp(&(b.node.as_str(), b.kind)));
    Ok(report)
}

/// A `LengthPercentage` as px (percentages resolve against an unknown basis: 0, the way the
/// runtime's cross-axis floor treats them).
fn lp_px(lp: taffy::style::LengthPercentage) -> f32 {
    match lp.expand() {
        taffy::style::ExpandedLengthPercentage::Length(v) => v,
        _ => 0.0,
    }
}

/// The horizontal padding plus border of `style` in px (the runtime's `pad_x_px`).
fn pad_x(style: &taffy::style::Style) -> [f32; 2] {
    [
        lp_px(style.padding.left) + lp_px(style.border.left),
        lp_px(style.padding.right) + lp_px(style.border.right),
    ]
}

/// The vertical padding plus border of `style` in px.
fn pad_y(style: &taffy::style::Style) -> [f32; 2] {
    [
        lp_px(style.padding.top) + lp_px(style.border.top),
        lp_px(style.padding.bottom) + lp_px(style.border.bottom),
    ]
}

/// Parses and checks every `.ui.ron` document of `sources` (`(path, text)` pairs) in every
/// locale of `locales` at every `size`. `locales` are `(tag, merged ftl text)` pairs; each is
/// selected in turn (the fallback chain of `Localization` applies). Returns one report per
/// (document, locale, size), findings first.
///
/// # Errors
///
/// A document does not parse, or taffy failed on one.
pub fn check_documents(
    sources: &[(String, String)],
    locales: &[(String, String)],
    sizes: &[FitSize],
    scale: f32,
) -> Result<Vec<FitReport>, crate::UiError> {
    let mut loc = Localization::new(locales.to_vec(), None)?;
    let mut text_system = TextSystem::embedded_only();
    let mut out = Vec::new();
    let mut by_doc: HashMap<String, UiFile> = HashMap::new();
    for (tag, _) in locales {
        // `set_locale` returns whether the selection *changed*; the first locale is usually
        // already selected (the fallback), so the return value is not a failure.
        loc.set_locale(tag);
        text_system.set_locale(tag);
        for (path, text) in sources {
            let file = match by_doc.get(path) {
                Some(f) => f.clone(),
                None => {
                    let f = parse_ui_ron(text, path, &UiMigrators::default())?;
                    by_doc.insert(path.clone(), f.clone());
                    f
                }
            };
            for size in sizes {
                let report = check_document(path, &file, &loc, &mut text_system, *size, scale)
                    .map_err(|e| crate::UiError::Localization(format!("fit layout of {path}: {e}")))?;
                out.push(report);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: FitSize = FitSize {
        width: 1920.0,
        height: 1080.0,
    };

    /// One named box holding `spans`; `style_extra` is appended to a 200x40 base style.
    fn doc(style_extra: &str, spans: &str, text_extra: &str) -> String {
        let style = if style_extra.contains("size:") {
            // A full size override replaces the base.
            style_extra.trim_start_matches(", ").to_owned()
        } else {
            format!("size: (200, 40){style_extra}")
        };
        format!(
            "(version: 6, root: (children: [(name: \"label\", style: ({style}), text: (spans: [{spans}]{text_extra}))]))"
        )
    }

    fn check_with_locales(locales: Vec<(String, String)>, ui: &str, tag: &str) -> FitReport {
        let file = parse_ui_ron(ui, "test.ui.ron", &UiMigrators::default()).expect("parses");
        let mut loc = Localization::new(locales, Some(tag.to_owned())).expect("loc");
        loc.set_locale(tag);
        let mut ts = TextSystem::embedded_only();
        ts.set_locale(tag);
        check_document("test.ui.ron", &file, &loc, &mut ts, SIZE, 1.0).expect("check")
    }

    fn check(en: &str, ui: &str) -> FitReport {
        check_with_locales(vec![("en".to_owned(), en.to_owned())], ui, "en")
    }

    const LONG_DE: &str = "EinstellungenundnochvielmehrTextdernichtinpasst";

    #[test]
    fn a_long_unwrapped_german_string_reports_width_overflow() {
        let report = check("k = x\n", &doc("", &format!("\"{LONG_DE}\""), ", wrap: none"));
        assert_eq!(report.findings.len(), 1, "{report:?}");
        let f = &report.findings[0];
        assert_eq!(f.kind, FitKind::WidthOverflow);
        assert_eq!(f.node, "root/label");
        assert_eq!(f.key, None);
        assert!(f.excess > 0.0 && f.measured > f.box_size);
        assert_eq!(f.box_size, 200.0, "no padding: the content box is the node box");
    }

    #[test]
    fn a_wider_box_fixes_the_overflow() {
        // The same long word fits a 900 px box.
        let report = check(
            "k = x\n",
            &doc(", size: (900, 40)", &format!("\"{LONG_DE}\""), ", wrap: none"),
        );
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.nodes_checked, 1);
    }

    #[test]
    fn wrapped_text_reports_height_overflow_and_a_taller_box_fixes_it() {
        let en = "k = One two three four five six seven eight nine ten eleven twelve\n";
        let report = check(en, &doc("", "Loc(\"k\")", ""));
        assert_eq!(report.findings.len(), 1, "{report:?}");
        assert_eq!(report.findings[0].kind, FitKind::HeightOverflow);
        assert_eq!(report.findings[0].key.as_deref(), Some("k"));
        assert_eq!(report.findings[0].box_size, 40.0);
        // A taller box fits the wrapped lines.
        let report = check(en, &doc(", size: (200, 400)", "Loc(\"k\")", ""));
        assert!(report.findings.is_empty(), "{report:?}");
    }

    #[test]
    fn clip_behaviour_reports_like_visible_but_ellipsis_and_scale_to_fit_do_not() {
        let en = format!("k = {LONG_DE}\n");
        for extra in ["", ", overflow: clip", ", overflow: visible"] {
            let report = check(&en, &doc("", "Loc(\"k\")", extra));
            assert_eq!(report.findings.len(), 1, "{extra}: {report:?}");
        }
        for extra in [", overflow: ellipsis", ", overflow: scaletofit"] {
            let report = check(&en, &doc("", "Loc(\"k\")", extra));
            assert!(report.findings.is_empty(), "{extra}: {report:?}");
        }
    }

    #[test]
    fn cjk_and_rtl_text_lay_out_without_false_reports() {
        // CJK wraps between characters; a roomy box reports nothing.
        let ui = doc(", size: (400, 200)", "Loc(\"k\")", "");
        let report = check_with_locales(
            vec![(
                "ja".to_owned(),
                "k = 設定とオプションについての詳細な説明文です\n".to_owned(),
            )],
            &ui,
            "ja",
        );
        assert!(report.findings.is_empty(), "{report:?}");
        assert_eq!(report.nodes_checked, 1);
        // RTL: alignment flips with the locale, the box does not.
        let report = check_with_locales(vec![("ar".to_owned(), "k = الإعدادات\n".to_owned())], &ui, "ar");
        assert!(report.findings.is_empty(), "{report:?}");
    }

    #[test]
    fn padding_shrinks_the_content_box() {
        // 200 px box, 50 px horizontal padding: the content box is 100 px.
        let report = check(
            "k = x\n",
            &doc(
                ", padding: (left: 50, right: 50)",
                &format!("\"{LONG_DE}\""),
                ", wrap: none",
            ),
        );
        assert_eq!(report.findings.len(), 1);
        assert!(
            (report.findings[0].box_size - 100.0).abs() < 1.0,
            "{}",
            report.findings[0].box_size
        );
    }

    #[test]
    fn check_documents_runs_every_locale_and_size() {
        let dir = regia_test_utils::unique_temp_dir("loc-fit-docs");
        let ui_path = dir.join("menu.ui.ron");
        std::fs::write(&ui_path, doc("", "Loc(\"k\")", "")).expect("write");
        let sources = vec![(
            "menu.ui.ron".to_owned(),
            std::fs::read_to_string(&ui_path).expect("read"),
        )];
        let locales = vec![
            ("en".to_owned(), "k = One two three\n".to_owned()),
            (
                "de".to_owned(),
                "k = EinstellungenundnochvielmehrTextdernichtinpasst\n".to_owned(),
            ),
        ];
        let sizes = vec![
            FitSize {
                width: 1920.0,
                height: 1080.0,
            },
            FitSize {
                width: 1280.0,
                height: 720.0,
            },
        ];
        let reports = check_documents(&sources, &locales, &sizes, 1.0).expect("check");
        assert_eq!(reports.len(), 4, "2 locales x 2 sizes");
        // The German long word overflows at both sizes; English wraps and fits.
        let de: usize = reports
            .iter()
            .filter(|r| r.findings.iter().any(|f| f.locale == "de"))
            .count();
        assert_eq!(de, 2, "{reports:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
