// SPDX-License-Identifier: MPL-2.0
//! Runtime game UI (`docs/specs/18-runtime-ui.md`): retained widget tree, taffy layout,
//! cosmic-text shaping with a dynamic glyph atlas, focus navigation, styling/themes, `.ui.ron`
//! authoring, localization and data binding.
//!
//! The editor uses egui instead (ADR-002); everything here is the game-facing runtime UI.
//!
//! - [`prelude`]: common imports for game code.
//! - [`UiPlugin`]: one plugin wires the whole system into an app.
//!
//! # How the pieces fit
//!
//! A [`UiRoot`] entity anchors a widget tree; its descendants carry [`UiNode`] + [`Style`] plus
//! visual components ([`Background`], [`Border`], [`Shadow`], [`UiImage`], [`Text`]) and widget
//! components ([`Button`], [`TextInput`], [`Slider`], [`ListView`]…). Each frame [`UiPlugin`] runs
//! layout (taffy, [`layout`]), text shaping ([`text`], cosmic-text with a dynamic glyph atlas),
//! focus navigation ([`focus`]), interaction ([`interaction`]) and widget updates, writes
//! [`ComputedNode`] rects, then extracts a draw list for `regia_render`'s overlay pass. Widgets
//! can be built in code or authored in `.ui.ron` ([`authoring`]); [`binding`] connects widget
//! values to reflected data, and [`localization`] translates strings.
//!
//! See `book/user-guide/ui.md` (widgets, `.ui.ron`) and `book/agents/editor.md`.
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_ui::prelude::*;
//!
//! let mut app = App::new();
//! app.add_plugins(UiPlugin::code_only());
//! assert!(app.world().get_resource::<TextSystem>().is_some());
//! ```

// Systems take many `Query`/`Res` params; `UiError` embeds `ron::SpannedError`.
#![allow(clippy::too_many_arguments, clippy::type_complexity, clippy::result_large_err)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::field_reassign_with_default)
)]

pub mod agent_bridge;
pub mod anchors;
pub mod anim_pack;
pub mod anim_timeline;
pub mod atlas;
pub mod authoring;
pub(crate) mod backdrop;
pub mod binding;
pub mod captions;
pub mod choice;
pub mod clipboard;
pub mod color;
pub mod color_picker;
pub mod cue_exchange;
pub mod debug_overlay;
pub mod default_widgets;
pub mod dragdrop;
pub mod draw;
pub mod extract;
pub mod focus;
pub mod focus_graph;
pub mod fuzzy;
pub mod gesture;
pub mod hints;
pub mod inspect;
pub mod interaction;
pub mod keyboard;
pub mod layers;
pub mod layout;
pub mod line_break_sa;
pub mod list;
pub mod loc_compile;
// The in-memory key manifest of the ADR-1570 pipeline (was `loc_gather` on integ/ext2; ADR-1589).
pub mod loc_glossary;
pub mod loc_manifest;
pub mod loc_pseudo;
pub mod locale_startup;
pub mod localization;
pub mod menu;
pub mod motion;
// Multi-channel signed distance fields for crisp UI/world text (docs/msdf-glyphs.md).
pub mod msdf;
pub mod node;
pub mod offscreen;
pub mod output;
pub mod popup;
pub mod radial;
pub mod rebind_panel;
pub mod render;
pub mod scale;
pub mod screens;
pub mod scroll;
pub mod scrollbar;
pub mod shortcuts;
pub mod soft_cursor;
pub mod style;
pub mod subtitles;
pub mod text;
pub mod text_edit;
pub mod text_input;
pub mod theme;
#[path = "theme/agent.rs"]
pub mod theme_agent;
#[path = "theme/color_ops.rs"]
pub mod theme_color_ops;
#[path = "theme/file.rs"]
pub mod theme_file;
#[path = "theme/lint.rs"]
pub mod theme_lint;
#[path = "theme/migrate.rs"]
pub mod theme_migrate;
#[path = "theme/motion.rs"]
pub mod theme_motion;
#[path = "theme/resolve.rs"]
pub mod theme_resolve;
#[path = "theme/tokens.rs"]
pub mod theme_tokens;
pub mod tree;
pub mod tween;
pub mod ui_sounds;
pub mod widget_style;
pub mod widgets;
// Scroll motion and nested scrolling (ADR-520).
pub mod scroll_motion;
// Accessibility tree export and screen-reader hooks (UI L14, ADR-865).
pub mod a11y;
// Accessibility audit of a live screen: rules over layout, the a11y tree and the theme (ADR-3025).
pub mod a11y_audit;
// Runtime theme switching: text follows the theme, accessibility, scheme preference (UI D15, ADR-865).
#[path = "theme/runtime.rs"]
pub mod theme_runtime;
// Icon atlases, sizes, tinting and scaling rules (UI D16, ADR-865).
pub mod icons;
// Localization workflow: plural table, pseudo mode, validator, gather, config (ADR-1466).
pub mod loc_bundle;
pub mod loc_check;
pub mod loc_config;
pub mod loc_fit;
pub mod loc_gather;
pub mod loc_glyphs;
pub mod loc_po;
pub mod loc_status;
pub mod loc_xliff;
pub mod plural;
pub mod pseudo;
// View-model bindings and UI commands (ADR-1811, `docs/specs/81-ui-designer.md`).
pub mod viewmodel;
// The screen-reader OS bridge (ADR-1998): the agent's view (always) and the UI half (`accesskit`
// feature); both documented by their inner docs.
pub mod a11y_platform;
#[cfg(feature = "accesskit")]
pub mod accesskit_bridge;
// Text outline/shadow/decorations/spacing and procedural UI materials (ADR-273 T07/T09, ADR-2045).
pub mod text_effects;
pub mod ui_material;
// Drawn tree view, data grid, color picker and separator over the ADR-1842 models (ADR-2045).
pub mod parity_widgets;
// Signed-distance-field text, opt-in per node (ADR-273 L07, ADR-2045).
pub mod sdf_text;
// Video in the UI: `UiVideo` and its transport widgets (specs/50 § 2.7, ADR-2044); opt-in plugin.
pub mod video;
// Camera-target and world-space UI, `ui.world_click` and `ui.set_target` (Y13/Y14, ADR-2406).
pub mod world_target;
// Binding converters and collection item templates (spec 81, ADR-2489).
pub mod converter;
pub mod repeat;
// Comment-preserving `.ui.ron` saves (spec 81, ADR-2489).
pub mod ron_merge;
// Culture-aware number, date, relative-time and list formatting; NUMBER() and DATETIME() (L04, ADR-2698).
pub mod loc_format;
// Isolated group opacity, rounded clips and image masks (T11, ADR-2866).
pub mod group_layer;
// Reusable `.ui.ron` components: `use`, parameters, slots, bind roots (spec 111, ADR-3147).
pub mod ui_component;
// Images and masks named by asset path or icon in `.ui.ron` (T2.13, ADR-3148).
pub mod ui_image_assets;
// Lobby list, scoreboard, radar, world markers and target brackets (ADR-2958).
pub mod game_widgets;
// The `InputGlyph` table: glyph per action per device family (spec 11 § 7, ADR-7070).
pub mod input_glyphs;
// The UI font atlas as the renderer's debug-label glyph source (ADR-7254).
pub mod debug_labels;
// Layout variants by window orientation and size (P-271, ADR-7858).
pub mod layout_variants;

use regia_app::prelude::*;

pub use atlas::GlyphAtlases;
pub use authoring::{
    UI_RON_VERSION, UiFile, UiLibrary, UiMigrators, UiTemplate, parse_ui_ron, ui_migration_sources,
    ui_schema_fingerprint, upgrade_ui_ron, write_ui_ron,
};
pub use backdrop::{BackdropStats, cpu_backdrop_blur};
pub use binding::{Bind, BindMode, BindSource, BindTarget};
pub use clipboard::{ClipboardProvider, MemoryClipboard, SystemClipboard, UiClipboard};
pub use color::UiColor;
pub use converter::BindConverter;
pub use default_widgets::{
    Breadcrumbs, CrumbIndex, FoldBody, FoldHeader, Foldable, RadioButton, SpinBox, Split, SplitAxis,
    SplitDivider, TabPage, TabPages, Toast, ToastHost, ToastQueue, spawn_breadcrumbs, spawn_foldable,
    spawn_radio_button, spawn_split,
};
pub use dragdrop::{
    DragActive, DragCancelReason, DragDrop, DragDropEvent, DragOperation, DragPayload, DragSource, DropHover,
    DropTarget, cancel_drag,
};
pub use draw::{TextureRef, UiQuad};
pub use extract::{UiDamage, UiDamageSet};
pub use focus::{DefaultFocus, FocusScope, FocusState, Focusable, NavDirection, NavLinks};
pub use focus::{FocusRingGeometry, FocusRingPolicy, focus_ring_geometry};
pub use focus_graph::{EdgeRule, FocusGroup, FocusMemory, NavGraph, NavReason, TabIndex, TabRule};
pub use hints::{
    HintAction, HintBar, HintDevice, HintEntry, TooltipDismiss, TooltipNavText, TooltipPolicy, TooltipState,
    TooltipTrigger, expand_hints,
};
pub use interaction::TouchTarget;
pub use interaction::{
    LongPressable, RepeatOnHold, UiButton, UiEvent, UiEventKind, UiInputConfig, UiInputState, UiModifiers,
    UiPointer, UiValue, bubble_ui_event,
};
// Bubbling, the gameplay pointer gate and the `Menu` context path (ADR-7070).
pub use binding::{AnimationRepeat, BindAnimation};
pub use binding::{ScriptFieldFn, ScriptFieldReader};
pub use focus::{MenuNavState, menu_context_active};
pub use input_glyphs::{
    INPUT_GLYPHS_PATH, INPUT_GLYPHS_VERSION, InputGlyphTable, InputGlyphs, input_glyphs_migration_sources,
    input_glyphs_schema_fingerprint,
};
pub use interaction::{UiBubbledEvent, UiStopBubble, bubble_path, bubble_ui_events, bubbles};
pub use layers::{InputLayer, InputLayers, LayerInputMode};
pub use layout::{LayoutChanges, LayoutDirty, UiLayoutContext, run_ui_layout};
pub use list::{
    CustomEntries, ListActivated, ListEntryBind, ListGeometry, ListHeaders, ListHeights, ListRow,
    ListSelection, ListSelectionChanged, ListState, ListTiles, RepeatRows, RowMetrics, ScrollAlign,
    ScrollToItem, SelectionMode, TileGrid,
};
pub use loc_compile::{CompileOptions, CompileOutput, LocalizationReport};
pub use loc_manifest::{GatherSource, KeyManifest};
pub use locale_startup::{LocaleSource, LocaleStartup, LocaleWriteBack, PlayerSetLocale};
pub use localization::{AudioLocaleChanged, LocalizationOrigin};
pub use localization::{LocText, LocaleChanged, Localization, LocalizationGeneration, LocalizationPlugin};
pub use motion::{
    ExitAction, MotionPhase, MotionPose, MotionSpeed, MotionTokens, MotionTrigger, PoseKind, RevealPreset,
    UiMotion, UiMotionState,
};
pub use node::*;
pub use offscreen::OffscreenTarget;
pub use output::{UiLayoutVariant, UiOutput, UiVariant, begin_render_out, default_safe_area, end_render_out};
pub use radial::{
    RadialConfirm, RadialEvent, RadialEventKind, RadialGeometry, RadialMenu, RadialOpen, RadialSector,
    RadialSelection, RadialStick, RadialSticks, spawn_radial_menu,
};
pub use rebind_panel::{
    RebindButton, RebindDeadZone, RebindInvert, RebindPanel, RebindPanelOptions, RebindProfileButton,
    RebindProfileStatus, RebindReset, RebindResetAll, RebindStatus,
};
pub use render::{OutputEncoding, SharedUiRenderer, UiRenderSettings, UiRenderer, UiRendererHandle};
pub use repeat::{RepeatBound, RepeatHost, RepeatStats};
pub use scale::{
    HANDHELD_MIN_TEXT_PX, ScaleMode, UiAccessibility, UiDevicePolicy, UiScales, resolve_window_size,
};
pub use screens::{ScreenChanged, ScreenError, ScreenInfo, ScreenStack, ScreenTransition, UiScreen};
pub use scroll::{ItemCount, ListItem, ListItems, ListView, ScrollOffset, ScrollView};
pub use scrollbar::{ScrollAxis, ScrollbarNodes, ScrollbarPart, Scrollbars};
pub use style::{Dimension, Display, Edges, JustifyContent, PositionType, Style, TrackSizing};
pub use subtitles::{SubtitleLine, SubtitleQueue};
pub use text::{CaretCluster, CaretLine, CaretMap};
pub use text::{
    FontSource, ShapedLine, ShapedText, Text, TextOverflow, TextSpan, TextStyle, TextSystem, WrapMode,
    parse_markup,
};
pub use text_edit::InputFilter;
pub use text_input::{TextInputKeys, TextInputPart, focus_node};
pub use theme::{ThemeClasses, ThemeColors, Themed, UiTheme, UiThemes};
pub use theme_file::{
    THEME_FORMAT_VERSION, ThemeError, ThemeFile, ThemeSet, parse_theme_ron, theme_migration_sources,
    theme_schema_fingerprint,
};
pub use theme_resolve::{ActiveTheme, ThemeOverride, ThemeResolver, ThemeScope, ThemeStats};
pub use tween::{
    Ease, HitTestSpace, Reveal, Tween, TweenEvent, TweenPlan, TweenProperty, TweenValue, UiAnimEventFired,
    UiAnimEventLog, UiLayoutBox, UiTransform, UiTween, UiTweenFinished,
};
pub use viewmodel::{
    BindingStatus, UiCommand, UiCommandName, ViewBindings, ViewModelAppExt, ViewModelField, ViewModelInfo,
    ViewModels,
};
pub use widget_style::{DISABLED_OPACITY, StyleState, UiDensity, WidgetSize};
pub use widgets::{
    Button, Dropdown, Modal, ProgressBar, Slider, TabBar, TextInput, Toggle, Tooltip, VirtualAxes,
    VirtualButton, VirtualButtons, VirtualStick,
};
pub use world_target::{Billboard, UiTargetKey, UiTargetNode, WorldPanel};
// Scroll containers, nested scrolling and scrollbar styles (ADR-520).
pub use layout::{ScrollDirty, UiHotPathCounts, UiLayoutStats};
pub use scroll::{ScrollMotion, ScrollVelocity, scroll_container};
pub use scroll_motion::{Overscroll, ScrollAxes, ScrollChaining, ScrollInput};
pub use scrollbar::ScrollbarStyle;

pub use group_layer::{
    GroupOpacity, LayerPlan, LayerStats, MaskFit, MaskShape, RoundedClip, UiClipMasks, UiLayerBudget, UiMask,
    UiMaskAsset,
};
pub use scale::{
    DpiCurve, UI_SETTINGS_PATH, UI_SETTINGS_VERSION, UiScaleSettings, UiSettingsFile,
    migrate_ui_settings_ron, parse_ui_settings_ron, ui_settings_migration_sources,
    ui_settings_schema_fingerprint, validate_scale_mode,
};
pub use ui_image_assets::{
    UI_IMAGE_FALLBACK, UI_IMAGE_PLACEHOLDER, UiImageAsset, UiImageAssets, UiImageHandle, UiImageRef,
    UiImageSource, UiImageStatus, UiMaskHandle, UiMaskIcon, ui_asset_dependencies, ui_image_references,
};

pub use shortcuts::{
    Chord, ConflictKind, ShortcutConflict, ShortcutError, ShortcutScope, ShortcutTriggered, UiShortcut,
    UiShortcuts,
};

/// Common imports for code that builds UIs.
pub mod prelude {
    pub use crate::localization::Localization;
    pub use crate::screens::UiScreen;
    pub use crate::{
        Background, Bind, Border, Button, Classes, ClipChildren, ComputedNode, Dimension, Disabled, Display,
        Dropdown, Edges, FocusScope, Focusable, Interactable, Invalid, JustifyContent, ListItems, ListView,
        Modal, NavLinks, NodeStyle, Opacity, PickThrough, PositionType, ProgressBar, RadialMenu,
        RadialSector, SafeAreaPadding, ScaleMode, ScrollOffset, ScrollView, Selected, Shadow, ShapedText,
        Slider, Style, TabBar, Text, TextInput, TextOverflow, TextSpan, TextStyle, TextSystem, Themed,
        Toggle, Tooltip, UiColor, UiDensity, UiEvent, UiEventKind, UiImage, UiNode, UiPlugin, UiRenderer,
        UiRoot, UiTheme, UiValue, VirtualButton, VirtualStick, WidgetSize, WrapMode, parse_markup,
    };
}

/// Errors of the UI crate.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// A `.ui.ron` file could not be parsed.
    #[error("ui ron parse error in {path}: {source}")]
    Ron {
        /// File the asset came from.
        path: String,
        /// The underlying RON error.
        source: ron::error::SpannedError,
    },
    /// A `.ui.ron` file declares an unknown format version.
    #[error(
        "ui file {path} declares version {found}; this engine loads version {supported} and no migrator is registered for it"
    )]
    UnsupportedVersion {
        /// File the asset came from.
        path: String,
        /// Version declared by the file.
        found: u32,
        /// Version this engine loads.
        supported: u32,
    },
    /// Localization files could not be loaded.
    #[error("localization: {0}")]
    Localization(String),
    /// A referenced font or image is not loaded.
    #[error("ui asset not found: {0}")]
    AssetNotFound(String),
    /// A `.ui.ron` document could not be written ([`write_ui_ron`]).
    #[error("ui ron write error: {0}")]
    Write(String),
    /// A UI file parsed but holds a value this engine refuses (a mask naming no image, an invalid
    /// DPI curve).
    #[error("ui file {path}: {reason}")]
    Invalid {
        /// File the asset came from.
        path: String,
        /// What is wrong.
        reason: String,
    },
    /// A data-binding path could not be resolved.
    #[error("binding {path}: {reason}")]
    Binding {
        /// The binding source path.
        path: String,
        /// Why it failed.
        reason: String,
    },
}

/// Installs the runtime UI: layout, text, interaction, widgets, themes, authoring, localization,
/// binding and the overlay render pass (`regia_render::WindowOverlay`).
#[derive(Default)]
pub struct UiPlugin {
    /// Load localization and `.ui.ron` templates from the project's `assets://` mount at startup.
    /// Turn off for tests that build their UI in code.
    pub load_project_assets: bool,
}

impl UiPlugin {
    /// A UI plugin that does not touch the VFS (for tests and code-built UIs).
    pub fn code_only() -> Self {
        Self {
            load_project_assets: false,
        }
    }
}

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TextSystem>()
            .init_resource::<LayoutDirty>()
            .init_resource::<layout::LayoutChanges>()
            .init_resource::<extract::UiDamage>()
            .init_resource::<extract::UiDamageSet>()
            .init_resource::<UiScales>()
            .init_resource::<scale::UiDevicePolicy>()
            .init_resource::<scale::UiAccessibility>()
            .init_resource::<UiThemes>()
            .init_resource::<theme_resolve::ActiveTheme>()
            .init_resource::<theme_resolve::ThemeResolver>()
            .init_resource::<UiPointer>()
            .init_resource::<UiInputState>()
            .init_resource::<Localization>()
            .init_resource::<UiMigrators>()
            .init_resource::<UiLibrary>()
            .init_resource::<layers::InputLayers>()
            .init_resource::<screens::ScreenStack>()
            .init_resource::<subtitles::SubtitleQueue>()
            .add_message::<screens::ScreenChanged>()
            .add_plugins(choice::ChoicePlugin)
            .init_resource::<interaction::UiInputConfig>()
            .init_resource::<interaction::UiClickState>()
            .init_resource::<dragdrop::DragDrop>()
            .add_message::<dragdrop::DragDropEvent>()
            .init_resource::<focus::FocusState>()
            .init_resource::<focus::UiNavActions>()
            .init_resource::<focus::UiInputMode>()
            .init_resource::<hints::TooltipPolicy>()
            .init_resource::<hints::TooltipState>()
            .init_resource::<hints::HintDevice>()
            .init_resource::<hints::HintBar>()
            .init_resource::<focus::GamepadNavConfig>()
            .init_resource::<focus::GamepadNavState>()
            .init_resource::<focus::FocusRingPolicy>()
            .init_resource::<VirtualAxes>()
            .init_resource::<VirtualButtons>()
            .init_resource::<default_widgets::ToastQueue>()
            .init_resource::<radial::RadialSticks>()
            .add_message::<radial::RadialEvent>()
            .add_message::<UiEvent>()
            .add_message::<interaction::UiBubbledEvent>()
            .init_resource::<focus::MenuNavState>()
            .init_resource::<input_glyphs::InputGlyphs>()
            .init_resource::<soft_cursor::SoftCursor>()
            .init_resource::<keyboard::KeyboardPolicy>()
            .add_systems(PostUpdate, keyboard::sync_on_screen_keyboard)
            .init_resource::<ui_sounds::UiSounds>()
            .init_resource::<ui_sounds::UiSoundLog>()
            .add_message::<ui_sounds::UiSoundRequest>()
            .add_systems(Update, ui_sounds::emit_ui_sounds.after(scroll::ui_scroll))
            .add_message::<regia_platform::GamepadButtonChanged>()
            .add_message::<regia_platform::GamepadAxisChanged>()
            .init_resource::<authoring::PendingUiReloads>()
            .add_message::<regia_asset::SourceAssetChanged>()
            .add_systems(
                Update,
                (authoring::collect_ui_changes, authoring::apply_ui_reloads)
                    .chain()
                    .before(interaction::ui_interaction),
            )
            // Layout variants follow the window (P-271, ADR-7858): after a live reload, before input.
            .add_systems(
                Update,
                layout_variants::apply_layout_variants
                    .after(authoring::apply_ui_reloads)
                    .before(interaction::ui_interaction),
            )
            .add_systems(Startup, scale::init_window_size)
            .add_systems(
                PreUpdate,
                (
                    scale::track_window_size,
                    scale::apply_ui_device_policy,
                    scale::apply_ui_accessibility,
                    // The touch flag themes read must be this frame's.
                    interaction::update_pointer.before(theme::apply_themes),
                    soft_cursor::drive_soft_cursor.after(interaction::update_pointer),
                    theme::apply_themes,
                ),
            )
            .add_systems(
                Update,
                (
                    screens::sync_screens,
                    layers::sync_layers,
                    interaction::ui_interaction,
                    dragdrop::ui_drag_drop,
                    focus::ui_gamepad_navigation,
                    radial::radial_stick_input,
                    radial::radial_menu_update,
                    focus::ui_focus_navigation,
                    widgets::ui_widgets,
                    scroll::ui_scroll,
                )
                    .chain(),
            )
            // `specs/18` § 4 (ADR-7070): the gameplay pointer gate follows the interaction pass, the
            // `Menu` input context fills the navigation actions before the raw paths, and every
            // event of the frame's UI passes bubbles to its ancestors.
            .add_systems(
                Update,
                (
                    interaction::sync_pointer_gate.after(interaction::ui_interaction),
                    focus::ui_menu_navigation
                        .after(dragdrop::ui_drag_drop)
                        .before(focus::ui_gamepad_navigation),
                    interaction::bubble_ui_events.after(scroll::ui_scroll),
                    // Declared `.ui.ron` focus neighbors (ADR-7827) before any navigation step.
                    authoring::resolve_declared_nav
                        .before(focus::ui_menu_navigation)
                        .before(focus::ui_gamepad_navigation),
                ),
            )
            .add_systems(
                PostUpdate,
                (
                    scroll::virtualise_lists,
                    binding::update_bindings,
                    layout::mark_layout_dirty,
                )
                    .chain()
                    .in_set(PostUpdateSet::UiLayout),
            )
            .add_systems(
                PostUpdate,
                (
                    layout::run_ui_layout.pipe(layout::log_layout_error),
                    // Visual transforms map the finished layout rects before shaping and damage see them.
                    tween::apply_ui_transforms,
                    text::shape_text,
                    extract::detect_ui_damage,
                )
                    .chain()
                    // Layout must see this frame's changes: without the edge the two chains
                    // raced and a style edit could reach layout a frame late.
                    .after(layout::mark_layout_dirty)
                    .in_set(PostUpdateSet::UiLayout),
            )
            // The font-fallback locale follows the game locale (ADR-1618), before anything shapes.
            .add_systems(
                PostUpdate,
                text::sync_font_locale
                    .before(layout::mark_layout_dirty)
                    .in_set(PostUpdateSet::UiLayout),
            )
            .add_systems(Extract, extract::extract_ui)
            // Camera-target and world-space UI (ADR-2406): pointer routing tags after layout, and
            // the frame passes exist only while such a root does.
            .add_systems(
                PostUpdate,
                world_target::tag_target_nodes
                    .after(extract::detect_ui_damage)
                    .in_set(PostUpdateSet::UiLayout),
            )
            .add_systems(
                Extract,
                world_target::sync_target_passes.after(extract::extract_ui),
            );
        if self.load_project_assets {
            app.add_systems(
                Startup,
                (
                    localization::load_localization,
                    authoring::load_ui_assets,
                    input_glyphs::load_input_glyphs,
                ),
            );
        }
        // Debug labels draw from the UI font atlas (ADR-7254).
        regia_render::install_debug_label_glyphs(app, Box::new(debug_labels::UiDebugLabelGlyphs::default()));
        // Focus groups, wrap/stop rules, Tab index and focus repair (ADR-516).
        app.add_plugins(focus_graph::FocusGraphPlugin);
        output::install(app);
        app.add_plugins(agent_bridge::UiAgentBridgePlugin);
        app.add_plugins(shortcuts::UiShortcutsPlugin);
        app.add_plugins(tween::UiTweenPlugin);
        app.add_plugins(motion::UiMotionPlugin);
        app.add_plugins((list::ListPlugin, scrollbar::ScrollbarPlugin));
        app.add_plugins(theme_agent::ThemeAgentPlugin {
            load_project_themes: self.load_project_assets,
        });
        app.init_resource::<layout::ScrollDirty>()
            .init_resource::<layout::UiLayoutStats>()
            .init_resource::<layout::UiHotPathCounts>();
        app.add_plugins(a11y::ScreenReaderPlugin);
        app.add_plugins(theme_runtime::ThemeRuntimePlugin);
        app.add_plugins(icons::IconPlugin);
        app.add_plugins(localization::LocalizationPlugin);
        // View models, two-way bindings and UI commands (ADR-1811).
        app.add_plugins(viewmodel::ViewModelPlugin);
        app.init_resource::<inspect::UiEventLog>()
            .add_systems(Last, inspect::record_events);
        // Animated UI materials (ADR-2045).
        app.init_resource::<ui_material::MaterialTick>()
            .add_systems(Update, ui_material::tick_ui_materials);
        app.add_plugins(parity_widgets::ParityWidgetsPlugin);
        // Lobby list, scoreboard, radar, world markers and target brackets (ADR-2958).
        game_widgets::install(app);
        // Theme audio items from the active theme's `sounds` (ADR-2045).
        app.init_resource::<ui_sounds::ThemeSoundsApplied>()
            .add_systems(PostUpdate, ui_sounds::apply_theme_sounds);
        // Live multi-touch gestures, opt-in (ADR-2045).
        app.init_resource::<gesture::UiGestureInput>()
            .add_message::<gesture::UiGestureEvent>()
            .add_systems(PreUpdate, gesture::feed_live_gestures);
        // GPU overlay: created lazily on first draw; the shared handle lets extract fill the
        // frame's draw list and keeps all wgpu state in one place.
        let renderer: UiRendererHandle = UiRenderer::new_handle();
        app.insert_resource(SharedUiRenderer(renderer.clone()));
        app.add_systems(Startup, {
            let renderer = renderer;
            move |overlay: Option<ResMut<regia_render::WindowOverlay>>| {
                let Some(mut overlay) = overlay else {
                    tracing::debug!(
                        target: "regia_ui",
                        "WindowOverlay resource missing; UI will not draw (add RenderPlugin)"
                    );
                    return;
                };
                overlay.set(render::OverlayShim::new(renderer.clone()));
            }
        });
        // Group layers and masks (ADR-2866): the budget, and the masks hit testing reads.
        app.init_resource::<group_layer::UiLayerBudget>()
            .init_resource::<scale::UiScaleSettings>()
            .add_systems(
                PostUpdate,
                group_layer::update_clip_masks
                    .run_if(group_layer::clip_masks_stale)
                    .after(extract::detect_ui_damage)
                    .in_set(PostUpdateSet::UiLayout),
            );
        // Mouse presses on UI belong to the UI until release (ADR-7976): the claim is written
        // after this frame's pointer and devices, before the action evaluation reads the mouse.
        app.init_resource::<regia_input::PointerClaims>().add_systems(
            PreUpdate,
            interaction::claim_pointer_buttons
                .after(interaction::update_pointer)
                .after(regia_input::InputSet::Devices)
                .before(regia_input::InputSet::Evaluation),
        );
        // Images and masks named by asset path or icon, loaded in the background (ADR-3148); it
        // also resolves `UiMaskAsset`, which `group_layer::resolve_mask_assets` loaded on the frame.
        app.add_plugins(ui_image_assets::UiImageAssetsPlugin);
        // The project's UI settings (`assets://ui/project.uisettings.ron`), which ship with the game.
        // The saved scale rule applies even when the file is not loaded here (a test or the game
        // may insert `UiScaleSettings` itself).
        app.add_systems(
            PostUpdate,
            scale::apply_project_scale_mode
                .before(layout::mark_layout_dirty)
                .in_set(PostUpdateSet::UiLayout),
        );
        if self.load_project_assets {
            app.add_systems(Startup, scale::load_ui_settings);
        }
    }
}
