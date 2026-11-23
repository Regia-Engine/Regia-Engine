// SPDX-License-Identifier: MIT-0
//! The theme sheet page (`specs/ui-default-design.md` § 8, lane D3): every existing widget in every
//! style state and every size, so a theme change (both built-in schemes) is visible at a glance and
//! the baseline goldens can pin the look before the default-design lanes change it.
//!
//! The sheet is static: cells carry the state markers (`Hovered`, `Pressed`, `Focused`, `Selected`,
//! `Invalid`, `Disabled`) and a [`WidgetSize`] directly, and are neither `Interactable` nor
//! `Focusable`, so the pointer and focus systems never touch them. The page is reached from the
//! gallery toolbar button (focusable, so a gamepad can open it) or the `5` key.

use regia::prelude::*;
use regia_ui::style::{Align, FlexDirection, JustifyContent};
use regia_ui::{
    Button, Classes, Dimension, Disabled, Dropdown, Edges, Focused, Hovered, Invalid, NodeName, NodeStyle,
    Pressed, ProgressBar, Selected, Slider, Style, TabBar, Text, TextInput, TextSpan, TextStyle, Themed,
    Toggle, UiDensity, UiNode, WidgetSize, WrapMode,
};

/// A widget kind shown on the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SheetWidget {
    /// Default button.
    Button,
    /// Button with the `primary` class.
    PrimaryButton,
    /// On/off switch (its `checked` flag follows the `Selected` state).
    Toggle,
    /// Slider track.
    Slider,
    /// Progress bar.
    Progress,
    /// Single-line text input.
    Input,
    /// Dropdown.
    Dropdown,
    /// Tab bar.
    Tab,
}

/// A style state shown on the sheet (the § 3 states; `Normal` is the base look).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SheetState {
    /// No state marker.
    Normal,
    /// Pointer over.
    Hover,
    /// Pointer down.
    Pressed,
    /// Keyboard or gamepad focus.
    Focused,
    /// Selected, checked or open.
    Selected,
    /// Invalid value.
    Error,
    /// Disabled.
    Disabled,
}

/// Every widget on the sheet, in row-group order.
pub const SHEET_WIDGETS: [SheetWidget; 8] = [
    SheetWidget::Button,
    SheetWidget::PrimaryButton,
    SheetWidget::Toggle,
    SheetWidget::Slider,
    SheetWidget::Progress,
    SheetWidget::Input,
    SheetWidget::Dropdown,
    SheetWidget::Tab,
];

/// Every state on the sheet, in column order.
pub const SHEET_STATES: [SheetState; 7] = [
    SheetState::Normal,
    SheetState::Hover,
    SheetState::Pressed,
    SheetState::Focused,
    SheetState::Selected,
    SheetState::Error,
    SheetState::Disabled,
];

/// Every size on the sheet, in row order within a widget group.
pub const SHEET_SIZES: [WidgetSize; 3] = [WidgetSize::Sm, WidgetSize::Md, WidgetSize::Lg];

/// Width of one state column in UI px.
const CELL_W: f32 = 168.0;
/// Width of the size label column in UI px.
const LABEL_W: f32 = 40.0;

impl SheetWidget {
    /// Lowercase name used in node names and captions.
    pub fn name(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::PrimaryButton => "primary",
            Self::Toggle => "toggle",
            Self::Slider => "slider",
            Self::Progress => "progress",
            Self::Input => "input",
            Self::Dropdown => "dropdown",
            Self::Tab => "tab",
        }
    }
}

impl SheetState {
    /// Lowercase name used in node names and column headers.
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Hover => "hover",
            Self::Pressed => "pressed",
            Self::Focused => "focused",
            Self::Selected => "selected",
            Self::Error => "error",
            Self::Disabled => "disabled",
        }
    }
}

/// Lowercase size name (`sm`, `md`, `lg`).
pub fn size_name(size: WidgetSize) -> &'static str {
    match size {
        WidgetSize::Sm => "sm",
        WidgetSize::Md => "md",
        WidgetSize::Lg => "lg",
    }
}

/// One cell of the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SheetCell {
    /// Widget kind.
    pub widget: SheetWidget,
    /// Style state.
    pub state: SheetState,
    /// Control size.
    pub size: WidgetSize,
}

impl SheetCell {
    /// The cell's `NodeName`: `sheet.<widget>.<size>.<state>`, for `ui.query` and `ui.node_state`.
    pub fn node_name(&self) -> String {
        format!(
            "sheet.{}.{}.{}",
            self.widget.name(),
            size_name(self.size),
            self.state.name()
        )
    }
}

/// Every cell of the sheet: widgets × sizes × states, in page order.
pub fn sheet_cells() -> Vec<SheetCell> {
    let mut cells = Vec::with_capacity(SHEET_WIDGETS.len() * SHEET_SIZES.len() * SHEET_STATES.len());
    for widget in SHEET_WIDGETS {
        for size in SHEET_SIZES {
            for state in SHEET_STATES {
                cells.push(SheetCell { widget, state, size });
            }
        }
    }
    cells
}

fn caption(commands: &mut Commands, parent: Entity, text: &str, width: f32, px: f32) {
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(width), Dimension::Auto],
                ..Default::default()
            }),
            Text {
                spans: vec![TextSpan::lit(text).with_style(TextStyle {
                    font_px: px,
                    ..Default::default()
                })],
                wrap: WrapMode::None,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(parent).add_child(entity);
}

fn row(commands: &mut Commands, parent: Entity) -> Entity {
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                size: [Dimension::Percent(100.0), Dimension::Auto],
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                align_items: Align::Center,
                ..Default::default()
            }),
        ))
        .id();
    commands.entity(parent).add_child(entity);
    entity
}

/// Spawns one cell under `parent` and returns it.
fn spawn_cell(commands: &mut Commands, parent: Entity, cell: SheetCell) -> Entity {
    let height = UiDensity::Comfortable.control_height(cell.size);
    let style = |width: f32, pad: f32| {
        NodeStyle::new(Style {
            size: [Dimension::Points(width), Dimension::Points(height)],
            padding: Edges::px(pad),
            justify_content: JustifyContent::Center,
            align_items: Align::Center,
            ..Default::default()
        })
    };
    let mut entity = commands.spawn((UiNode, NodeName(cell.node_name()), cell.size, Themed::default()));
    match cell.widget {
        SheetWidget::Button | SheetWidget::PrimaryButton => {
            let classes = if cell.widget == SheetWidget::Button {
                vec!["button".to_owned()]
            } else {
                vec!["button".to_owned(), "primary".to_owned()]
            };
            entity.insert((style(CELL_W, 8.0), Button, Classes(classes)));
        }
        SheetWidget::Toggle => {
            entity.insert((
                style(CELL_W, 0.0),
                Toggle {
                    checked: cell.state == SheetState::Selected,
                },
                Classes(vec!["toggle".into()]),
            ));
        }
        SheetWidget::Slider => {
            entity.insert((
                style(CELL_W, 0.0),
                Slider {
                    value: 0.6,
                    min: 0.0,
                    max: 1.0,
                    step: 0.05,
                },
                Classes(vec!["slider-track".into()]),
            ));
        }
        SheetWidget::Progress => {
            entity.insert((
                style(CELL_W, 0.0),
                ProgressBar { value: 0.35 },
                Classes(vec!["surface".into()]),
            ));
        }
        SheetWidget::Input => {
            let mut input = TextInput::default();
            input.placeholder = "Name".to_owned();
            entity.insert((style(CELL_W, 8.0), input, Classes(vec!["Panel.field".into()])));
        }
        SheetWidget::Dropdown => {
            entity.insert((
                style(CELL_W, 8.0),
                Dropdown {
                    options: vec!["Alpha".into(), "Beta".into()],
                    selected: Some(0),
                    // The Selected marker below shows the state. An open dropdown is a live
                    // popup with its own input layer (light dismiss), which would take every
                    // press in the gallery, on any page (ADR-1384).
                    open: false,
                },
                Classes(vec!["Button.field".into()]),
            ));
        }
        SheetWidget::Tab => {
            entity.insert((
                style(CELL_W, 8.0),
                TabBar {
                    tabs: vec!["One".into(), "Two".into()],
                    active: 0,
                },
                Classes(vec!["tab".into()]),
            ));
        }
    }
    match cell.state {
        SheetState::Normal => {}
        SheetState::Hover => {
            entity.insert(Hovered);
        }
        SheetState::Pressed => {
            entity.insert(Pressed);
        }
        SheetState::Focused => {
            entity.insert(Focused);
        }
        SheetState::Selected => {
            entity.insert(Selected);
        }
        SheetState::Error => {
            entity.insert(Invalid);
        }
        SheetState::Disabled => {
            entity.insert(Disabled);
        }
    }
    let id = entity.id();
    // Buttons and tabs show a label so the text color of each state is visible.
    if matches!(
        cell.widget,
        SheetWidget::Button | SheetWidget::PrimaryButton | SheetWidget::Tab | SheetWidget::Dropdown
    ) {
        let label = commands
            .spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(cell.state.name())],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ))
            .id();
        commands.entity(id).add_child(label);
    }
    commands.entity(parent).add_child(id);
    id
}

/// Fills `page` with the theme sheet: a header row of state names, then per widget a caption and
/// one row per size holding one cell per state.
pub fn spawn_theme_sheet(commands: &mut Commands, page: Entity) {
    let header = row(commands, page);
    caption(commands, header, "", LABEL_W, 14.0);
    for state in SHEET_STATES {
        caption(commands, header, state.name(), CELL_W, 14.0);
    }
    for widget in SHEET_WIDGETS {
        caption(commands, page, widget.name(), CELL_W, 14.0);
        for size in SHEET_SIZES {
            let r = row(commands, page);
            caption(commands, r, size_name(size), LABEL_W, 12.0);
            for state in SHEET_STATES {
                spawn_cell(commands, r, SheetCell { widget, state, size });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn cells_cover_the_full_product_with_unique_names() {
        let cells = sheet_cells();
        assert_eq!(cells.len(), 8 * 3 * 7);
        let names: HashSet<String> = cells.iter().map(SheetCell::node_name).collect();
        assert_eq!(names.len(), cells.len(), "node names must be unique");
        assert!(names.contains("sheet.primary.lg.disabled"));
        assert!(names.contains("sheet.toggle.sm.selected"));
    }

    #[test]
    fn every_state_and_size_name_is_distinct() {
        let states: HashSet<_> = SHEET_STATES.iter().map(|s| s.name()).collect();
        assert_eq!(states.len(), SHEET_STATES.len());
        let sizes: HashSet<_> = SHEET_SIZES.iter().map(|s| size_name(*s)).collect();
        assert_eq!(sizes.len(), SHEET_SIZES.len());
    }
}
