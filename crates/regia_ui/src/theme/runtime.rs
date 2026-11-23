// SPDX-License-Identifier: MPL-2.0
//! Runtime theme switching (UI D15, ADR-865): what makes a light / dark / high-contrast swap
//! complete on screen.
//!
//! D6's resolver ([`crate::theme_resolve`]) already swaps the theme at runtime when
//! [`ActiveTheme`] changes and cross-fades every themed node's colors over `motion.base`. Three
//! pieces were missing, and live here:
//!
//! 1. **Text follows the theme.** The resolver computed and faded [`Themed::text_color`], but
//!    nothing applied it to a [`Text`], so after a dark-to-light swap labels stayed light on a
//!    light surface. [`apply_theme_text`] writes it into the spans of the themed node's own text
//!    and of the labels below it, through plain containers, frame by frame during the fade (glyphs
//!    are recolored without a reshape, `ShapedText::recolor`). Only theme-owned spans change: spans still at the default
//!    color or at the color the theme wrote last. Authored colors stay, and [`KeepTextColor`] opts
//!    a text out.
//! 2. **Accessibility settings reach the theme.** [`sync_theme_accessibility`] copies
//!    [`UiAccessibility::high_contrast`] and [`UiAccessibility::reduced_motion`] into
//!    [`ActiveTheme`] when they change (the player's setting picks the high-contrast theme; reduced
//!    motion makes the swap instant).
//! 3. **Scheme preference.** [`ThemeMode`] follows a dark, light or system preference by picking
//!    the matching theme name. The platform does not report the OS scheme yet, so the host writes
//!    [`ThemeMode::system_scheme`] (from winit's `WindowEvent::ThemeChanged`, for example).
//!
//! Agent method: `ui.theme_mode` (book `agents/ui-theme-switch.md`).
//!
//! UI D15 transitions (ADR-1309) add:
//!
//! 4. **A shaped swap fade.** [`ThemeTransition`] sets the swap's length, curve and blend space.
//!    By default a swap lasts the new theme's `motion.base`, follows its `ease.standard` curve and
//!    blends in OKLab ([`BlendSpace::Oklab`]), so a dark-to-light fade brightens evenly instead of
//!    flashing light in its first frames (a quarter of the way in, a linear-light blend from the
//!    dark to the light button surface is already half way in perceptual lightness). State
//!    transitions (hover, pressed) keep their own timing.
//! 5. **Change messages.** [`ThemeChanged`] is written in the frame a new theme generation applies
//!    and [`ThemeFadeFinished`] in the frame its cross-fade ends, so a game can switch its own
//!    non-UI colors (a clear color, a post-process tint) in step with the UI.
//! 6. **The OS scheme.** The platform's `SystemThemeChanged` message (Windows and macOS through
//!    winit, Android from the configuration's night mode; Linux reports none yet) sets
//!    [`ThemeMode::system_scheme`].

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::node::Background;
use crate::scale::UiAccessibility;
use crate::text::{Text, TextStyle};
use crate::theme::Themed;
use crate::theme_color_ops::mix;
use crate::theme_file::Scheme;
use crate::theme_resolve::{ActiveTheme, DEFAULT_THEME, ThemeResolver};
use crate::theme_tokens::Easing;
use crate::tween::Ease;

/// The longest swap fade [`ThemeTransition::duration`] may ask for, in seconds.
pub const MAX_SWAP_FADE_SECS: f32 = 10.0;

/// The color space a theme swap blends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendSpace {
    /// OKLab (perceptual): lightness moves evenly through the fade. The default.
    #[default]
    Oklab,
    /// Straight linear-light RGB (the D6 behavior).
    Linear,
}

impl BlendSpace {
    /// The protocol name (`"oklab"`, `"linear"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Oklab => "oklab",
            Self::Linear => "linear",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Oklab, Self::Linear].into_iter().find(|s| s.name() == name)
    }
}

/// How a theme swap cross-fades (spec § 6.4). It takes effect at the next swap; a running fade
/// keeps the settings it started with. Reduced motion (on [`ActiveTheme`] or in
/// [`UiAccessibility`]) still makes every swap instant.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct ThemeTransition {
    /// Fade length in seconds, clamped to `0..=MAX_SWAP_FADE_SECS`; `None` uses the new theme's
    /// `motion.base` token. `Some(0.0)` swaps instantly.
    pub duration: Option<f32>,
    /// Progress curve; `None` uses the new theme's `ease.standard` token (linear when the theme
    /// has none).
    pub ease: Option<Easing>,
    /// The color space of the blend.
    pub space: BlendSpace,
}

impl ThemeTransition {
    /// The fade length for a theme whose `motion.base` is `theme_base` (seconds).
    pub fn fade_secs(&self, theme_base: Option<f32>) -> f32 {
        let secs = self.duration.or(theme_base).unwrap_or(0.0);
        if secs.is_nan() {
            0.0
        } else {
            secs.clamp(0.0, MAX_SWAP_FADE_SECS)
        }
    }
}

/// Eased progress of `easing` at linear progress `k` (clamped to `0..=1`; `1.0` maps to `1.0`).
pub fn ease_progress(easing: Easing, k: f32) -> f32 {
    // The ends are exact whatever the solver's rounding.
    if k >= 1.0 {
        return 1.0;
    }
    if k.is_nan() || k <= 0.0 {
        return 0.0;
    }
    match easing {
        Easing::Linear => Ease::Linear.apply(k),
        Easing::CubicBezier(points) => Ease::CubicBezier(points).apply(k),
    }
}

/// One color of a theme swap at blend factor `t` (`0.0` = `from`, `1.0` = `to` exactly). A fully
/// transparent end takes the other end's color, so a fill fading in or out changes only its alpha
/// instead of passing through black.
pub fn blend_swap_color(from: UiColor, to: UiColor, t: f32, space: BlendSpace) -> UiColor {
    if t >= 1.0 {
        return to;
    }
    if t <= 0.0 {
        return from;
    }
    let from = if from.0[3] <= 0.0 {
        to.with_alpha(0.0)
    } else {
        from
    };
    let to_c = if to.0[3] <= 0.0 { from.with_alpha(0.0) } else { to };
    match space {
        BlendSpace::Oklab => mix(from, to_c, t),
        BlendSpace::Linear => from.lerp(to_c, t),
    }
}

/// Written in the frame a new theme generation applies: a theme, high-contrast, density or theme
/// set change (spec § 6.4). The first generation of a run has `from: None`.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct ThemeChanged {
    /// The theme nodes resolved against before (after the high-contrast and fallback rules).
    pub from: Option<String>,
    /// The theme nodes resolve against now.
    pub to: String,
    /// The new theme generation.
    pub generation: u64,
    /// How long the cross-fade runs (0 for an instant swap).
    pub fade_secs: f32,
}

/// Written in the frame the cross-fade of a theme change ends: every themed node shows the new
/// theme's colors. An instant swap writes it in the same frame as its [`ThemeChanged`]; a swap
/// replaced mid-fade by another one writes it only for the last.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct ThemeFadeFinished {
    /// The theme the fade ended on.
    pub theme: String,
    /// Its generation.
    pub generation: u64,
}

/// Keeps a [`Text`]'s span colors as authored: the theme never recolors it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeepTextColor;

/// The text color the theme wrote into this text last ([`apply_theme_text`]); spans still at
/// this color are theme-owned and follow the next theme.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct ThemeTextColor(pub UiColor);

/// Which scheme [`ThemeMode`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemePreference {
    /// Always the dark theme.
    #[default]
    Dark,
    /// Always the light theme.
    Light,
    /// The OS scheme ([`ThemeMode::system_scheme`]); dark until the host reports one.
    System,
}

impl SchemePreference {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Dark, Self::Light, Self::System]
            .into_iter()
            .find(|p| p.name() == name)
    }
}

/// Follows a scheme preference by selecting [`Self::dark`] or [`Self::light`] in [`ActiveTheme`].
/// With `follow: None` (the default) it does nothing and the host selects themes directly.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct ThemeMode {
    /// The preference to follow; `None` leaves [`ActiveTheme::name`] to the host.
    pub follow: Option<SchemePreference>,
    /// The theme for the dark scheme.
    pub dark: String,
    /// The theme for the light scheme.
    pub light: String,
    /// The OS scheme as the host last reported it (`None` = unknown, treated as dark).
    pub system_scheme: Option<Scheme>,
}

impl Default for ThemeMode {
    fn default() -> Self {
        Self {
            follow: None,
            dark: DEFAULT_THEME.to_owned(),
            light: "regia-light".to_owned(),
            system_scheme: None,
        }
    }
}

impl ThemeMode {
    /// The scheme the preference resolves to now, `None` when not following.
    pub fn scheme(&self) -> Option<Scheme> {
        self.follow.map(|p| match p {
            SchemePreference::Dark => Scheme::Dark,
            SchemePreference::Light => Scheme::Light,
            SchemePreference::System => self.system_scheme.unwrap_or(Scheme::Dark),
        })
    }

    /// The theme name the preference selects now, `None` when not following.
    pub fn wanted_theme(&self) -> Option<&str> {
        self.scheme().map(|s| match s {
            Scheme::Dark => self.dark.as_str(),
            Scheme::Light => self.light.as_str(),
        })
    }
}

/// `PreUpdate`, before `apply_themes`: while [`ThemeMode`] follows a preference, select its theme.
/// It writes [`ActiveTheme`] only when the name differs, so an idle frame never bumps the theme
/// generation.
pub fn follow_theme_mode(mode: Option<Res<ThemeMode>>, mut active: ResMut<ActiveTheme>) {
    let Some(wanted) = mode.as_deref().and_then(ThemeMode::wanted_theme) else {
        return;
    };
    if active.name != wanted {
        active.name = wanted.to_owned();
    }
}

/// `PreUpdate`, before `apply_themes`: copies the player's high-contrast and reduced-motion choices
/// ([`UiAccessibility`]) into [`ActiveTheme`] when they change. On the first frame only choices that
/// are on are copied, so a host that preset [`ActiveTheme::high_contrast`] keeps it.
pub fn sync_theme_accessibility(
    access: Option<Res<UiAccessibility>>,
    mut active: ResMut<ActiveTheme>,
    mut seen: Local<bool>,
) {
    let _span = tracing::debug_span!("sync_theme_accessibility").entered();
    let Some(access) = access else {
        return;
    };
    let first = !*seen;
    *seen = true;
    if !first && !access.is_changed() {
        return;
    }
    let (contrast, motion) = if first {
        (
            active.high_contrast || access.high_contrast,
            active.reduced_motion || access.reduced_motion,
        )
    } else {
        (access.high_contrast, access.reduced_motion)
    };
    if active.high_contrast != contrast {
        active.high_contrast = contrast;
    }
    if active.reduced_motion != motion {
        active.reduced_motion = motion;
    }
}

/// Whether a span of this color belongs to the theme: still the default color, or the color the
/// theme wrote last.
fn theme_owned(span_color: UiColor, last: Option<UiColor>) -> bool {
    span_color == TextStyle::default().color || last == Some(span_color)
}

/// Whether [`recolor_spans`] would change anything (read-only, so no change mark).
fn needs_recolor(text: &Text, color: UiColor, last: Option<UiColor>) -> bool {
    text.spans
        .iter()
        .any(|s| s.style.color != color && theme_owned(s.style.color, last))
}

/// Writes `color` into the theme-owned spans of `text`. Returns whether a span changed.
fn recolor_spans(text: &mut Text, color: UiColor, last: Option<UiColor>) -> bool {
    let mut changed = false;
    for span in &mut text.spans {
        if span.style.color != color && theme_owned(span.style.color, last) {
            span.style.color = color;
            changed = true;
        }
    }
    changed
}

/// The most nodes one themed node's text walk visits: hierarchies are trees, and a corrupt one (a
/// `Children` cycle) must not hang the frame.
const MAX_TEXT_WALK: usize = 65_536;

/// `PreUpdate`, after `apply_themes`: every themed node whose look changed this frame writes its
/// [`Themed::text_color`] into its own [`Text`] and into the labels below it (childless texts with no
/// look of their own), through plain containers: a caption in an unstyled row sits on the same
/// surface, and white text on a light panel is unreadable. A node with its own [`Background`] or
/// [`Themed`] look stops the walk, because what is below it sits on that surface instead. During a
/// cross-fade that is once a frame; glyphs recolor without a reshape.
pub fn apply_theme_text(
    changed: Query<(Entity, &Themed, Option<&Children>), Changed<Themed>>,
    containers: Query<&Children, (Without<Text>, Without<Themed>, Without<Background>)>,
    mut labels: Query<
        (&mut Text, Option<&ThemeTextColor>, Option<&Children>),
        (Without<KeepTextColor>, Without<Themed>),
    >,
    mut own: Query<(&mut Text, Option<&ThemeTextColor>), (With<Themed>, Without<KeepTextColor>)>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_theme_text").entered();
    let mut walk: Vec<Entity> = Vec::new();
    for (entity, themed, children) in &changed {
        let Some(color) = themed.text_color else {
            continue;
        };
        if let Ok((mut text, last)) = own.get_mut(entity) {
            let last = last.map(|l| l.0);
            // Check through `Deref` first: a mutable deref marks the text changed.
            if needs_recolor(&text, color, last) {
                recolor_spans(&mut text, color, last);
                commands.entity(entity).insert(ThemeTextColor(color));
            }
        }
        walk.clear();
        walk.extend(children.iter().flat_map(|c| c.iter()));
        let mut visited = 0;
        while let Some(child) = walk.pop() {
            visited += 1;
            if visited > MAX_TEXT_WALK {
                break;
            }
            if let Ok(below) = containers.get(child) {
                walk.extend(below.iter());
                continue;
            }
            let Ok((mut text, last, grandchildren)) = labels.get_mut(child) else {
                continue;
            };
            if grandchildren.is_some_and(|g| !g.is_empty()) {
                continue; // not a label: a container with its own content
            }
            let last = last.map(|l| l.0);
            if needs_recolor(&text, color, last) {
                recolor_spans(&mut text, color, last);
                commands.entity(child).insert(ThemeTextColor(color));
            }
        }
    }
}

/// `PreUpdate`, after [`apply_theme_text`]: a label added under a node whose look already settled
/// takes that node's [`Themed::text_color`] as well. [`apply_theme_text`] runs only when a look
/// changes, so without this a list row spawned into an open light panel stays default white (found
/// by the accessibility audit, ADR-3025). It walks up through the same plain containers the downward
/// walk goes through and stops at a node with its own [`Background`] or text. Costs one change-tick
/// check per text node a frame; recolors only labels added since the last run.
pub fn apply_theme_text_to_new_labels(
    mut added: Query<
        (
            Entity,
            &mut Text,
            Option<&ThemeTextColor>,
            Option<&Children>,
            &ChildOf,
        ),
        (Added<Text>, Without<KeepTextColor>, Without<Themed>),
    >,
    up: Query<(Option<&Themed>, Option<&ChildOf>, Has<Text>, Has<Background>)>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_theme_text_new").entered();
    for (entity, mut text, last, children, parent) in &mut added {
        if children.is_some_and(|c| !c.is_empty()) {
            continue; // not a label: a container with its own content
        }
        let mut at = parent.parent();
        let mut steps = 0;
        let color = loop {
            steps += 1;
            if steps > MAX_TEXT_WALK {
                break None;
            }
            let Ok((themed, above, has_text, has_background)) = up.get(at) else {
                break None;
            };
            if let Some(themed) = themed {
                break themed.text_color;
            }
            if has_text || has_background {
                break None; // the label sits on that node's surface, not the theme's
            }
            match above {
                Some(above) => at = above.parent(),
                None => break None,
            }
        };
        let Some(color) = color else {
            continue;
        };
        let last = last.map(|l| l.0);
        if needs_recolor(&text, color, last) {
            recolor_spans(&mut text, color, last);
            commands.entity(entity).insert(ThemeTextColor(color));
        }
    }
}

/// `PreUpdate`, before `apply_themes`: hands a changed [`ThemeTransition`] to the resolver, which
/// uses it from the next swap on.
pub fn sync_theme_transition(transition: Option<Res<ThemeTransition>>, mut resolver: ResMut<ThemeResolver>) {
    let Some(transition) = transition else {
        return;
    };
    if resolver.transition() != *transition {
        resolver.set_transition(*transition);
    }
}

/// `PreUpdate`, before `follow_theme_mode`: the OS scheme the platform reports
/// (`regia_platform::SystemThemeChanged`) becomes [`ThemeMode::system_scheme`]. The last message
/// of a frame wins.
pub fn follow_system_scheme(
    mut reports: MessageReader<regia_platform::SystemThemeChanged>,
    mode: Option<ResMut<ThemeMode>>,
) {
    let _span = tracing::debug_span!("follow_system_scheme").entered();
    let Some(last) = reports.read().last() else {
        return;
    };
    let scheme = if last.dark { Scheme::Dark } else { Scheme::Light };
    if let Some(mut mode) = mode
        && mode.system_scheme != Some(scheme)
    {
        mode.system_scheme = Some(scheme);
    }
}

/// What [`report_theme_changes`] saw last.
#[derive(Debug, Default)]
pub struct ThemeChangeWatch {
    generation: u64,
    active: String,
    fade_pending: bool,
}

/// `PreUpdate`, after `apply_themes`: writes [`ThemeChanged`] when the resolver starts a new
/// generation and [`ThemeFadeFinished`] when that generation's cross-fade has ended. An idle frame
/// writes nothing and allocates nothing.
pub fn report_theme_changes(
    resolver: Res<ThemeResolver>,
    mut watch: Local<ThemeChangeWatch>,
    mut changed: MessageWriter<ThemeChanged>,
    mut finished: MessageWriter<ThemeFadeFinished>,
) {
    let _span = tracing::debug_span!("report_theme_changes").entered();
    let generation = resolver.generation();
    if generation != watch.generation && generation > 0 {
        let from = (watch.generation > 0).then(|| std::mem::take(&mut watch.active));
        watch.active.clear();
        watch.active.push_str(resolver.active());
        watch.generation = generation;
        watch.fade_pending = true;
        changed.write(ThemeChanged {
            from,
            to: watch.active.clone(),
            generation,
            fade_secs: resolver.fade_secs(),
        });
    }
    if watch.fade_pending && !resolver.fading() {
        watch.fade_pending = false;
        finished.write(ThemeFadeFinished {
            theme: watch.active.clone(),
            generation,
        });
    }
}

/// Adds [`ThemeMode`], [`ThemeTransition`], the change messages and the systems around
/// `apply_themes`. [`crate::UiPlugin`] adds it.
pub struct ThemeRuntimePlugin;

impl Plugin for ThemeRuntimePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ThemeMode>()
            .init_resource::<ThemeTransition>()
            .add_message::<ThemeChanged>()
            .add_message::<ThemeFadeFinished>()
            .add_message::<regia_platform::SystemThemeChanged>()
            .add_systems(
                PreUpdate,
                (
                    (
                        follow_system_scheme.before(follow_theme_mode),
                        follow_theme_mode,
                        sync_theme_accessibility,
                        sync_theme_transition,
                    )
                        .before(crate::theme::apply_themes),
                    (
                        apply_theme_text,
                        apply_theme_text_to_new_labels.after(apply_theme_text),
                        report_theme_changes,
                    )
                        .after(crate::theme::apply_themes),
                ),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextSpan;

    #[test]
    fn preferences_pick_the_matching_theme() {
        let mut mode = ThemeMode::default();
        assert_eq!(mode.wanted_theme(), None, "not following by default");
        mode.follow = Some(SchemePreference::Light);
        assert_eq!(mode.wanted_theme(), Some("regia-light"));
        mode.follow = Some(SchemePreference::System);
        assert_eq!(
            mode.wanted_theme(),
            Some(DEFAULT_THEME),
            "unknown OS scheme reads as dark"
        );
        mode.system_scheme = Some(Scheme::Light);
        assert_eq!(mode.wanted_theme(), Some("regia-light"));
        for p in [
            SchemePreference::Dark,
            SchemePreference::Light,
            SchemePreference::System,
        ] {
            assert_eq!(SchemePreference::parse(p.name()), Some(p));
        }
        assert_eq!(SchemePreference::parse("sepia"), None);
    }

    #[test]
    fn only_theme_owned_spans_are_recolored() {
        let red = UiColor([1.0, 0.0, 0.0, 1.0]);
        let ink = UiColor([0.1, 0.1, 0.1, 1.0]);
        let paper = UiColor([0.9, 0.9, 0.9, 1.0]);
        let mut text = Text {
            spans: vec![
                TextSpan::lit("default "),
                TextSpan::lit("warning").with_style(TextStyle {
                    color: red,
                    ..TextStyle::default()
                }),
            ],
            ..Text::default()
        };
        assert!(recolor_spans(&mut text, ink, None));
        assert_eq!(text.spans[0].style.color, ink);
        assert_eq!(text.spans[1].style.color, red, "authored colors stay");
        // The next theme moves the span the theme wrote, still not the authored one.
        assert!(recolor_spans(&mut text, paper, Some(ink)));
        assert_eq!(text.spans[0].style.color, paper);
        assert_eq!(text.spans[1].style.color, red);
        assert!(!recolor_spans(&mut text, paper, Some(paper)), "nothing to do");
    }

    #[test]
    fn swap_blends_land_exactly_and_keep_hue_through_transparency() {
        let dark = UiColor::from_srgb8(0x1f, 0x26, 0x30, 255);
        let light = UiColor::from_srgb8(0xff, 0xff, 0xff, 255);
        for space in [BlendSpace::Oklab, BlendSpace::Linear] {
            assert_eq!(blend_swap_color(dark, light, 0.0, space), dark);
            assert_eq!(blend_swap_color(dark, light, 1.0, space), light);
            assert_eq!(blend_swap_color(dark, light, 2.0, space), light, "clamped");
        }
        // OKLab moves lightness evenly; linear light is far brighter a quarter of the way in.
        let l = |c: UiColor| crate::theme_color_ops::to_oklab(c).l;
        let (l0, l1) = (l(dark), l(light));
        let ok = (l(blend_swap_color(dark, light, 0.25, BlendSpace::Oklab)) - l0) / (l1 - l0);
        let lin = (l(blend_swap_color(dark, light, 0.25, BlendSpace::Linear)) - l0) / (l1 - l0);
        assert!((ok - 0.25).abs() < 1e-3, "{ok}");
        assert!(lin > 0.4, "{lin}");
        // A fill fading in from transparent keeps its own color and only gains alpha.
        let red = UiColor([0.8, 0.1, 0.1, 1.0]);
        for space in [BlendSpace::Oklab, BlendSpace::Linear] {
            let half = blend_swap_color(UiColor::TRANSPARENT, red, 0.5, space);
            assert!((half.0[3] - 0.5).abs() < 1e-4, "{space:?}: {half:?}");
            for i in 0..3 {
                assert!((half.0[i] - red.0[i]).abs() < 1e-3, "{space:?}: {half:?}");
            }
            let out = blend_swap_color(red, UiColor::TRANSPARENT, 0.5, space);
            assert!((out.0[0] - red.0[0]).abs() < 1e-3, "{space:?}: {out:?}");
        }
    }

    #[test]
    fn transition_lengths_fall_back_and_clamp() {
        let theme_base = Some(0.16);
        let t = ThemeTransition::default();
        assert_eq!(t.fade_secs(theme_base), 0.16, "the theme's motion.base");
        assert_eq!(
            t.fade_secs(None),
            0.0,
            "a theme without motion.base swaps at once"
        );
        let with = |d: f32| ThemeTransition {
            duration: Some(d),
            ..ThemeTransition::default()
        };
        assert_eq!(with(0.4).fade_secs(theme_base), 0.4);
        assert_eq!(with(-1.0).fade_secs(theme_base), 0.0);
        assert_eq!(with(f32::NAN).fade_secs(theme_base), 0.0);
        assert_eq!(with(f32::INFINITY).fade_secs(theme_base), MAX_SWAP_FADE_SECS);
        for s in [BlendSpace::Oklab, BlendSpace::Linear] {
            assert_eq!(BlendSpace::parse(s.name()), Some(s));
        }
        assert_eq!(BlendSpace::parse("hsl"), None);
        assert_eq!(ease_progress(Easing::Linear, 0.3), 0.3);
        assert_eq!(ease_progress(Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]), 1.0), 1.0);
        assert!(
            ease_progress(Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]), 0.5) > 0.5,
            "ease-out"
        );
    }
}
