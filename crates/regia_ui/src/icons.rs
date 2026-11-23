// SPDX-License-Identifier: MPL-2.0
//! Iconography (UI D16, ADR-865; spec `ui-default-design.md` § 2.7): icon atlases, named icons with
//! several drawn sizes, tinting from the theme, and the scaling rules. The mechanism only: the icon
//! set itself is an owner choice (spec § 9 Q3), so no icon artwork ships here.
//!
//! - An [`IconSheet`] is RGBA pages plus named rectangles. Build one from a grid sheet
//!   ([`IconSheet::from_grid`]) or from any packed atlas (a cooked `.atlas.ron` sprite atlas maps
//!   onto it one to one: pages to [`IconPage`], sprites to [`IconSprite`]). A sprite named
//!   `gear@16` is the 16 px drawing of `gear`; a name with no `@size` is drawn at its source height.
//! - [`IconAtlas`] (a resource) holds the loaded sheets, registers their pages with the renderer as
//!   images ([`flush_icon_pages`]) and finds the variant to draw for a size ([`pick_variant`]).
//! - An [`Icon`] component names an icon, a size ([`IconSize`]: the theme's `size.icon.sm/md/lg`
//!   tokens, or pixels) and a tint ([`IconTint`]). [`resolve_icons`] turns it into the node's
//!   [`UiImage`] and size every frame something changed, and reports the outcome in [`IconState`].
//!
//! Scaling rules ([`pick_variant`]), for a target of `t` physical pixels:
//! 1. a variant within one pixel of `t` draws at its own size (crisp: texels map 1:1);
//! 2. otherwise the smallest variant larger than `t`, scaled down to `t` rounded to whole pixels;
//! 3. otherwise (every variant is smaller) the largest, scaled up, and [`IconState::upscaled`] is set
//!    so the agent and the lint can flag a blurry icon.
//!
//! Tinting: icons are drawn white on transparent and multiplied by the tint. [`IconTint::Theme`]
//! takes the nearest themed ancestor's `icon_color`, else its text color (so icons follow a theme
//! switch and its cross-fade), else the theme's `color.text`. [`IconTint::Original`] keeps
//! full-color artwork as drawn.
//!
//! Agent method: `ui.icons` (book `agents/ui-icons.md`).

use std::collections::HashMap;

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::node::{ComputedNode, NodeStyle, UiImage};
use crate::render::SharedUiRenderer;
use crate::style::Dimension;
use crate::theme::Themed;
use crate::theme_resolve::ThemeResolver;

/// First renderer image id used for icon pages (far above ids a host picks by hand).
pub const ICON_IMAGE_BASE: u64 = 0x1C0_0000_0000;
/// Largest icon page side accepted.
pub const MAX_PAGE_SIDE: u32 = 8192;

/// One RGBA8 page of an icon sheet (sRGB, straight alpha, rows top first).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IconPage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba8: Vec<u8>,
}

/// One named drawing on a page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IconSprite {
    /// `name` or `name@size` (the size in pixels this drawing is made for).
    pub name: String,
    /// Page index.
    pub page: u32,
    /// Rectangle on the page `[x, y, width, height]`, origin top-left.
    pub rect: [u32; 4],
}

/// Icon pages plus named rectangles: what an icon atlas loads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IconSheet {
    /// The pages.
    pub pages: Vec<IconPage>,
    /// The drawings.
    pub sprites: Vec<IconSprite>,
}

/// Why a sheet was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IconError {
    /// A page's byte length does not match its size, or a side is 0 or above [`MAX_PAGE_SIDE`].
    #[error("icon page {page} is {width}x{height} with {bytes} bytes")]
    BadPage {
        /// Page index.
        page: usize,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// Byte length.
        bytes: usize,
    },
    /// A sprite names a missing page or reaches outside its page, or is empty.
    #[error("icon `{name}`: rectangle {rect:?} is outside page {page}")]
    BadRect {
        /// Sprite name.
        name: String,
        /// Page index.
        page: u32,
        /// Rectangle.
        rect: [u32; 4],
    },
    /// A sprite has an empty name or an unreadable `@size`.
    #[error("icon name `{0}` is not `name` or `name@pixels`")]
    BadName(String),
    /// Two sprites draw one icon at one size.
    #[error("icon `{name}` has two drawings at {size} px")]
    Duplicate {
        /// Icon name.
        name: String,
        /// Size in pixels.
        size: u32,
    },
    /// A grid sheet's cell does not fit its page.
    #[error("grid cell {cell:?} does not fit a {width}x{height} page")]
    BadGrid {
        /// Cell size.
        cell: [u32; 2],
        /// Page width.
        width: u32,
        /// Page height.
        height: u32,
    },
}

impl IconError {
    /// A stable snake_case code for the agent protocol.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadPage { .. } => "icon_bad_page",
            Self::BadRect { .. } => "icon_bad_rect",
            Self::BadName(_) => "icon_bad_name",
            Self::Duplicate { .. } => "icon_duplicate",
            Self::BadGrid { .. } => "icon_bad_grid",
        }
    }
}

/// Splits `name@size` into the icon name and the size the drawing is made for.
pub fn parse_icon_name(name: &str) -> Option<(&str, Option<u32>)> {
    match name.rsplit_once('@') {
        None if !name.is_empty() => Some((name, None)),
        None => None,
        Some((base, size)) => {
            let size: u32 = size.parse().ok().filter(|s| *s > 0)?;
            (!base.is_empty()).then_some((base, Some(size)))
        }
    }
}

impl IconSheet {
    /// One page cut into `cell`-sized drawings, row by row, named by `names` (an empty name skips
    /// a cell). Names may carry `@size`.
    pub fn from_grid(page: IconPage, cell: [u32; 2], names: &[&str]) -> Result<Self, IconError> {
        if cell[0] == 0 || cell[1] == 0 || cell[0] > page.width || cell[1] > page.height {
            return Err(IconError::BadGrid {
                cell,
                width: page.width,
                height: page.height,
            });
        }
        let columns = page.width / cell[0];
        let rows = page.height / cell[1];
        let mut sprites = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            if i >= columns.saturating_mul(rows) {
                break;
            }
            if name.is_empty() {
                continue;
            }
            sprites.push(IconSprite {
                name: (*name).to_owned(),
                page: 0,
                rect: [(i % columns) * cell[0], (i / columns) * cell[1], cell[0], cell[1]],
            });
        }
        let sheet = Self {
            pages: vec![page],
            sprites,
        };
        sheet.validate()?;
        Ok(sheet)
    }

    /// Checks pages, rectangles and names.
    pub fn validate(&self) -> Result<(), IconError> {
        for (i, page) in self.pages.iter().enumerate() {
            let expected = u64::from(page.width) * u64::from(page.height) * 4;
            if page.width == 0
                || page.height == 0
                || page.width > MAX_PAGE_SIDE
                || page.height > MAX_PAGE_SIDE
                || page.rgba8.len() as u64 != expected
            {
                return Err(IconError::BadPage {
                    page: i,
                    width: page.width,
                    height: page.height,
                    bytes: page.rgba8.len(),
                });
            }
        }
        let mut seen: Vec<(&str, u32)> = Vec::new();
        for sprite in &self.sprites {
            let (base, size) =
                parse_icon_name(&sprite.name).ok_or_else(|| IconError::BadName(sprite.name.clone()))?;
            let page = usize::try_from(sprite.page).ok().and_then(|p| self.pages.get(p));
            let [x, y, w, h] = sprite.rect;
            let fits = page.is_some_and(|p| {
                w > 0
                    && h > 0
                    && x.checked_add(w).is_some_and(|r| r <= p.width)
                    && y.checked_add(h).is_some_and(|b| b <= p.height)
            });
            if !fits {
                return Err(IconError::BadRect {
                    name: sprite.name.clone(),
                    page: sprite.page,
                    rect: sprite.rect,
                });
            }
            let size = size.unwrap_or(h);
            if seen.contains(&(base, size)) {
                return Err(IconError::Duplicate {
                    name: base.to_owned(),
                    size,
                });
            }
            seen.push((base, size));
        }
        Ok(())
    }
}

/// One drawing of an icon, ready to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconVariant {
    /// The size it is drawn for (pixels).
    pub size_px: u32,
    /// Renderer image id of its page.
    pub image: u64,
    /// UV rectangle on the page.
    pub uv: [f32; 4],
    /// Width / height of the drawing.
    pub aspect: f32,
}

/// What [`pick_variant`] chose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconPick {
    /// The variant.
    pub variant: IconVariant,
    /// Draw size in physical pixels (height; width follows the aspect).
    pub draw_px: f32,
    /// The variant is smaller than the target (scaled up, so softer).
    pub upscaled: bool,
}

/// The scaling rules (module docs): `variants` sorted by size, `target_px` in physical pixels.
pub fn pick_variant(variants: &[IconVariant], target_px: f32) -> Option<IconPick> {
    if !target_px.is_finite() || target_px <= 0.0 {
        return None;
    }
    if let Some(v) = variants
        .iter()
        .filter(|v| (v.size_px as f32 - target_px).abs() <= 1.0)
        .min_by(|a, b| {
            let da = (a.size_px as f32 - target_px).abs();
            let db = (b.size_px as f32 - target_px).abs();
            da.total_cmp(&db)
        })
    {
        return Some(IconPick {
            variant: *v,
            draw_px: v.size_px as f32,
            upscaled: false,
        });
    }
    let draw_px = target_px.round().max(1.0);
    if let Some(v) = variants.iter().find(|v| v.size_px as f32 > target_px) {
        return Some(IconPick {
            variant: *v,
            draw_px,
            upscaled: false,
        });
    }
    variants.last().map(|v| IconPick {
        variant: *v,
        draw_px,
        upscaled: true,
    })
}

/// The loaded icons and the pages waiting to reach the renderer.
#[derive(Resource, Debug, Default)]
pub struct IconAtlas {
    icons: HashMap<String, Vec<IconVariant>>,
    pending: Vec<(u64, IconPage)>,
    next_image: u64,
    sheets: u32,
    /// Bumped whenever icons are added or removed.
    pub generation: u64,
}

impl IconAtlas {
    /// Loads a sheet: validates it, assigns its pages renderer image ids (uploaded by
    /// [`flush_icon_pages`]) and adds its drawings. A drawing of an icon at a size already loaded
    /// replaces the old one (a later sheet overrides an earlier one). Returns how many drawings
    /// were added.
    pub fn add_sheet(&mut self, sheet: IconSheet) -> Result<usize, IconError> {
        sheet.validate()?;
        let first = ICON_IMAGE_BASE + self.next_image;
        self.next_image += sheet.pages.len() as u64;
        let sizes: Vec<[u32; 2]> = sheet.pages.iter().map(|p| [p.width, p.height]).collect();
        for (i, page) in sheet.pages.into_iter().enumerate() {
            self.pending.push((first + i as u64, page));
        }
        let mut added = 0;
        for sprite in &sheet.sprites {
            let Some((base, size)) = parse_icon_name(&sprite.name) else {
                continue; // validated above
            };
            let Some([pw, ph]) = usize::try_from(sprite.page)
                .ok()
                .and_then(|p| sizes.get(p))
                .copied()
            else {
                continue;
            };
            let [x, y, w, h] = sprite.rect;
            let variant = IconVariant {
                size_px: size.unwrap_or(h),
                image: first + u64::from(sprite.page),
                uv: [
                    x as f32 / pw as f32,
                    y as f32 / ph as f32,
                    (x + w) as f32 / pw as f32,
                    (y + h) as f32 / ph as f32,
                ],
                aspect: w as f32 / h as f32,
            };
            let list = self.icons.entry(base.to_owned()).or_default();
            list.retain(|v| v.size_px != variant.size_px);
            list.push(variant);
            list.sort_by_key(|v| v.size_px);
            added += 1;
        }
        self.sheets += 1;
        self.generation += 1;
        Ok(added)
    }

    /// Removes every icon (pages already uploaded stay registered with the renderer).
    pub fn clear(&mut self) {
        self.icons.clear();
        self.pending.clear();
        self.generation += 1;
    }

    /// The drawings of `name`, smallest first.
    pub fn variants(&self, name: &str) -> &[IconVariant] {
        self.icons.get(name).map_or(&[], Vec::as_slice)
    }

    /// Whether `name` is loaded.
    pub fn contains(&self, name: &str) -> bool {
        self.icons.contains_key(name)
    }

    /// Every loaded icon name, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.icons.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Sheets loaded so far.
    pub fn sheet_count(&self) -> u32 {
        self.sheets
    }

    /// Pages waiting for [`flush_icon_pages`]: `(image id, width, height)`.
    pub fn pending_pages(&self) -> Vec<(u64, u32, u32)> {
        self.pending
            .iter()
            .map(|(id, p)| (*id, p.width, p.height))
            .collect()
    }
}

/// How big an icon is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum IconSize {
    /// The theme's `size.icon.sm` (16 comfortable, 12 compact).
    Small,
    /// The theme's `size.icon.md` (24 comfortable, 16 compact).
    #[default]
    Medium,
    /// The theme's `size.icon.lg` (32 comfortable, 20 compact).
    Large,
    /// A size in UI pixels.
    Px(f32),
}

impl IconSize {
    /// The size in UI pixels: the theme token, else the comfortable default.
    pub fn ui_px(self, resolver: Option<&ThemeResolver>) -> f32 {
        let (token, fallback) = match self {
            Self::Px(px) => return if px.is_finite() { px.max(0.0) } else { 0.0 },
            Self::Small => ("size.icon.sm", 16.0),
            Self::Medium => ("size.icon.md", 24.0),
            Self::Large => ("size.icon.lg", 32.0),
        };
        resolver
            .and_then(ThemeResolver::resolved_theme)
            .and_then(|t| t.number(token))
            .unwrap_or(fallback)
    }
}

/// What color an icon is multiplied by.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum IconTint {
    /// The nearest themed ancestor's `icon_color`, else its text color, else the theme's
    /// `color.text` (white without a theme).
    #[default]
    Theme,
    /// A fixed color.
    Color(UiColor),
    /// No tint: full-color artwork as drawn.
    Original,
}

/// A named icon drawn in the node (it replaces the node's [`UiImage`]).
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct Icon {
    /// The icon name (without `@size`).
    pub name: String,
    /// How big.
    pub size: IconSize,
    /// What color.
    pub tint: IconTint,
}

impl Icon {
    /// A medium icon tinted by the theme.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets the size.
    #[must_use]
    pub fn with_size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// Sets the tint.
    #[must_use]
    pub fn with_tint(mut self, tint: IconTint) -> Self {
        self.tint = tint;
        self
    }
}

/// What [`resolve_icons`] did for one [`Icon`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct IconState {
    /// The icon is not in the atlas (nothing is drawn).
    pub missing: bool,
    /// Size of the drawing used (pixels); 0 when missing.
    pub variant_px: u32,
    /// Drawn height in physical pixels.
    pub draw_px: f32,
    /// The drawing was scaled up (softer than a matching drawing would be).
    pub upscaled: bool,
    /// The tint applied.
    pub tint: UiColor,
    /// Size in UI pixels the node is laid out at.
    pub ui_px: f32,
}

/// The tint for an icon on `entity` (see [`IconTint`]).
fn tint_of(
    tint: IconTint,
    entity: Entity,
    themed: &Query<&Themed>,
    parents: &Query<&ChildOf>,
    resolver: Option<&ThemeResolver>,
) -> UiColor {
    match tint {
        IconTint::Color(c) => return c,
        IconTint::Original => return UiColor::WHITE,
        IconTint::Theme => {}
    }
    let mut current = Some(entity);
    let mut depth = 0;
    while let Some(e) = current {
        if let Ok(t) = themed.get(e)
            && let Some(c) = t.icon_color.or(t.text_color)
        {
            return c;
        }
        depth += 1;
        if depth > 256 {
            break;
        }
        current = parents.get(e).ok().map(|c| c.0);
    }
    resolver
        .and_then(ThemeResolver::resolved_theme)
        .and_then(|t| t.color("color.text"))
        .unwrap_or(UiColor::WHITE)
}

/// `PostUpdate`, before layout: for every [`Icon`], pick the drawing for its size at the node's UI
/// scale, write the node's [`UiImage`] (uv, page, tint) and, when the node's style leaves both sides
/// `Auto` (or still holds the size this system wrote), its size; and record [`IconState`]. It runs
/// every frame (icons follow theme fades and scale changes) but writes only values that changed, so
/// a steady frame marks nothing changed.
pub fn resolve_icons(
    atlas: Option<Res<IconAtlas>>,
    resolver: Option<Res<ThemeResolver>>,
    mut icons: Query<(
        Entity,
        &Icon,
        Option<&ComputedNode>,
        Option<&mut UiImage>,
        Option<&mut NodeStyle>,
        Option<&mut IconState>,
    )>,
    themed: Query<&Themed>,
    parents: Query<&ChildOf>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_icons").entered();
    let resolver = resolver.as_deref();
    for (entity, icon, computed, image, style, state) in &mut icons {
        let ui_px = icon.size.ui_px(resolver);
        let scale = computed.map_or(1.0, |c| c.scale).max(0.001);
        let tint = tint_of(icon.tint, entity, &themed, &parents, resolver);
        let pick = atlas
            .as_deref()
            .and_then(|a| pick_variant(a.variants(&icon.name), ui_px * scale));
        // The size this system wrote last: a node still at it follows the icon's size.
        let previous_px = state.as_ref().map(|s| s.ui_px);
        let next_state = IconState {
            missing: pick.is_none(),
            variant_px: pick.map_or(0, |p| p.variant.size_px),
            draw_px: pick.map_or(0.0, |p| p.draw_px),
            upscaled: pick.is_some_and(|p| p.upscaled),
            tint,
            ui_px,
        };
        match state {
            Some(mut s) => {
                if *s != next_state {
                    *s = next_state;
                }
            }
            None => {
                commands.entity(entity).insert(next_state);
            }
        }
        // Layout size: the icon's UI size, unless the author sized the node.
        let side = Dimension::Points(ui_px);
        match style {
            Some(mut s) => {
                let auto = matches!(s.0.size, [Dimension::Auto, Dimension::Auto]);
                let ours = previous_px.is_some_and(|px| s.0.size == [Dimension::Points(px); 2]);
                if (auto || ours) && s.0.size != [side, side] {
                    s.0.size = [side, side];
                }
            }
            None => {
                commands.entity(entity).insert(NodeStyle(crate::style::Style {
                    size: [side, side],
                    ..crate::style::Style::default()
                }));
            }
        }
        let Some(pick) = pick else {
            if image.is_some() {
                commands.entity(entity).remove::<UiImage>();
            }
            continue;
        };
        let next_image = UiImage {
            id: pick.variant.image,
            uv: pick.variant.uv,
            tint,
            preserve_aspect: true,
        };
        match image {
            Some(mut i) => {
                if *i != next_image {
                    *i = next_image;
                }
            }
            None => {
                commands.entity(entity).insert(next_image);
            }
        }
    }
}

/// `PostUpdate`: hands pages waiting in [`IconAtlas`] to the renderer (uploaded on its next draw).
/// Without a renderer (headless tests) the pages stay pending.
pub fn flush_icon_pages(atlas: Option<ResMut<IconAtlas>>, renderer: Option<Res<SharedUiRenderer>>) {
    let _span = tracing::debug_span!("flush_icon_pages").entered();
    let (Some(mut atlas), Some(renderer)) = (atlas, renderer) else {
        return;
    };
    if atlas.pending.is_empty() {
        return;
    }
    let pages = std::mem::take(&mut atlas.pending);
    let mut ui = renderer.lock();
    for (id, page) in pages {
        ui.queue_image_rgba(id, page.width, page.height, page.rgba8);
    }
}

/// Adds [`IconAtlas`], [`resolve_icons`] (before layout) and [`flush_icon_pages`]. [`crate::UiPlugin`]
/// adds it.
pub struct IconPlugin;

impl Plugin for IconPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IconAtlas>().add_systems(
            PostUpdate,
            (
                resolve_icons
                    .before(crate::layout::mark_layout_dirty)
                    .in_set(PostUpdateSet::UiLayout),
                flush_icon_pages,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(w: u32, h: u32) -> IconPage {
        IconPage {
            width: w,
            height: h,
            rgba8: vec![255; (w * h * 4) as usize],
        }
    }

    fn variants(sizes: &[u32]) -> Vec<IconVariant> {
        sizes
            .iter()
            .map(|s| IconVariant {
                size_px: *s,
                image: 1,
                uv: [0.0, 0.0, 1.0, 1.0],
                aspect: 1.0,
            })
            .collect()
    }

    #[test]
    fn names_carry_an_optional_size() {
        assert_eq!(parse_icon_name("gear"), Some(("gear", None)));
        assert_eq!(parse_icon_name("gear@16"), Some(("gear", Some(16))));
        assert_eq!(parse_icon_name("mail@home@24"), Some(("mail@home", Some(24))));
        for bad in ["", "@16", "gear@", "gear@0", "gear@x"] {
            assert_eq!(parse_icon_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn scaling_rules_pick_crisp_then_larger_then_largest() {
        let v = variants(&[16, 20, 24, 32]);
        // Rule 1: within a pixel draws at the drawing's own size.
        let p = pick_variant(&v, 24.6).expect("pick");
        assert_eq!((p.variant.size_px, p.draw_px, p.upscaled), (24, 24.0, false));
        let p = pick_variant(&v, 15.2).expect("pick");
        assert_eq!((p.variant.size_px, p.draw_px), (16, 16.0));
        // Rule 2: the smallest larger drawing, scaled down to whole pixels.
        let p = pick_variant(&v, 27.0).expect("pick");
        assert_eq!((p.variant.size_px, p.draw_px, p.upscaled), (32, 27.0, false));
        let p = pick_variant(&v, 12.0).expect("pick");
        assert_eq!((p.variant.size_px, p.draw_px), (16, 12.0));
        // Rule 3: nothing large enough: the largest, scaled up and flagged.
        let p = pick_variant(&v, 48.0).expect("pick");
        assert_eq!((p.variant.size_px, p.draw_px, p.upscaled), (32, 48.0, true));
        assert_eq!(pick_variant(&[], 24.0), None);
        assert_eq!(pick_variant(&v, 0.0), None);
        assert_eq!(pick_variant(&v, f32::NAN), None);
    }

    #[test]
    fn grid_sheets_and_validation() {
        let sheet = IconSheet::from_grid(page(48, 24), [24, 24], &["gear@24", "", "extra"]).expect("grid");
        assert_eq!(
            sheet.sprites.len(),
            1,
            "empty names skip, names past the grid are ignored"
        );
        assert_eq!(sheet.sprites[0].rect, [0, 0, 24, 24]);
        let err = IconSheet::from_grid(page(16, 16), [24, 24], &["x"]).expect_err("cell too big");
        assert_eq!(err.code(), "icon_bad_grid");

        let mut bad = sheet.clone();
        bad.pages[0].rgba8.pop();
        assert_eq!(bad.validate().map_err(|e| e.code()), Err("icon_bad_page"));
        let mut bad = sheet.clone();
        bad.sprites[0].rect = [40, 0, 24, 24];
        assert_eq!(bad.validate().map_err(|e| e.code()), Err("icon_bad_rect"));
        let mut bad = sheet.clone();
        bad.sprites[0].name = "gear@".to_owned();
        assert_eq!(bad.validate().map_err(|e| e.code()), Err("icon_bad_name"));
        let mut bad = sheet.clone();
        bad.sprites.push(IconSprite {
            name: "gear".to_owned(),
            page: 0,
            rect: [24, 0, 24, 24],
        });
        assert_eq!(bad.validate().map_err(|e| e.code()), Err("icon_duplicate"));
    }

    #[test]
    fn the_atlas_assigns_pages_and_sorts_variants() {
        let mut atlas = IconAtlas::default();
        let sheet = IconSheet {
            pages: vec![page(64, 32)],
            sprites: vec![
                IconSprite {
                    name: "gear@32".to_owned(),
                    page: 0,
                    rect: [32, 0, 32, 32],
                },
                IconSprite {
                    name: "gear@16".to_owned(),
                    page: 0,
                    rect: [0, 0, 16, 16],
                },
            ],
        };
        assert_eq!(atlas.add_sheet(sheet), Ok(2));
        let v = atlas.variants("gear");
        assert_eq!(v.iter().map(|v| v.size_px).collect::<Vec<_>>(), vec![16, 32]);
        assert_eq!(v[1].uv, [0.5, 0.0, 1.0, 1.0]);
        assert_eq!(v[0].image, ICON_IMAGE_BASE);
        assert_eq!(atlas.pending_pages(), vec![(ICON_IMAGE_BASE, 64, 32)]);
        assert_eq!(atlas.names(), vec!["gear"]);
        // A second sheet gets the next image id and replaces the 16 px drawing.
        let again = IconSheet::from_grid(page(16, 16), [16, 16], &["gear@16"]).expect("grid");
        assert_eq!(atlas.add_sheet(again), Ok(1));
        assert_eq!(atlas.variants("gear")[0].image, ICON_IMAGE_BASE + 1);
        assert_eq!(atlas.variants("gear").len(), 2);
        assert_eq!(atlas.generation, 2);
        assert!(atlas.variants("nope").is_empty());
    }
}
