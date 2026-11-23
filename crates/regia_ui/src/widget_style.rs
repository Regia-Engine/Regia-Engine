// SPDX-License-Identifier: MPL-2.0
//! Widget style surface (`specs/ui-default-design.md` § 2.4, § 3): control sizes, density and the
//! state snapshot that [`crate::theme::StyleClass::resolve_state`] composes in the fixed state
//! order `normal → size → selected → hover → pressed → error → focused → disabled`.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

/// Control size of a widget (`sm` / `md` / `lg`); `md` is the default. As a component it sizes
/// the node; without it the node keeps its authored size.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub enum WidgetSize {
    /// Small control.
    Sm,
    /// Default control.
    #[default]
    Md,
    /// Large control.
    Lg,
}

/// UI density: one theme, two size tables. Colors and shapes are shared.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub enum UiDensity {
    /// Games, TV, Deck, phones: 44 px control height.
    #[default]
    Comfortable,
    /// Tools, debug panels, the editor: 28 px control height.
    Compact,
}

impl UiDensity {
    fn pick(self, comfortable: [f32; 3], compact: [f32; 3], size: WidgetSize) -> f32 {
        let table = match self {
            Self::Comfortable => comfortable,
            Self::Compact => compact,
        };
        match size {
            WidgetSize::Sm => table[0],
            WidgetSize::Md => table[1],
            WidgetSize::Lg => table[2],
        }
    }

    /// `size.control.<size>`: control height in UI px.
    pub fn control_height(self, size: WidgetSize) -> f32 {
        self.pick([32.0, 44.0, 56.0], [22.0, 28.0, 36.0], size)
    }

    /// `size.icon.<size>`: icon edge in UI px.
    pub fn icon(self, size: WidgetSize) -> f32 {
        self.pick([16.0, 24.0, 32.0], [12.0, 16.0, 20.0], size)
    }

    /// `size.pad-x.<size>`: control inline padding in UI px.
    pub fn pad_x(self, size: WidgetSize) -> f32 {
        self.pick([12.0, 16.0, 24.0], [6.0, 8.0, 12.0], size)
    }

    /// `size.touch-min`: minimum touch target edge.
    pub fn touch_min(self) -> f32 {
        match self {
            Self::Comfortable => 44.0,
            Self::Compact => 28.0,
        }
    }

    /// `size.row`: list/tree row height.
    pub fn row(self) -> f32 {
        match self {
            Self::Comfortable => 44.0,
            Self::Compact => 24.0,
        }
    }

    /// `size.scrollbar`: `(rest, hover)` thickness.
    pub fn scrollbar(self) -> (f32, f32) {
        match self {
            Self::Comfortable => (10.0, 14.0),
            Self::Compact => (8.0, 10.0),
        }
    }
}

/// Opacity of a disabled widget over its normal look (the one disabled rule, § 3).
pub const DISABLED_OPACITY: f32 = 0.45;

/// Everything state resolution looks at for one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleState {
    /// Pointer is over the node.
    pub hovered: bool,
    /// Primary button/finger is down on the node.
    pub pressed: bool,
    /// Node has keyboard/gamepad focus.
    pub focused: bool,
    /// Node is disabled.
    pub disabled: bool,
    /// Node is selected/checked/open.
    pub selected: bool,
    /// Node holds an invalid value.
    pub error: bool,
    /// Control size.
    pub size: WidgetSize,
    /// Input is touch: hover is not applied.
    pub touch: bool,
    /// The focus came from keyboard or gamepad navigation, so the focus is shown (ADR-7827): the
    /// `focus_visible` layer applies over `focused`. Set only while `focused` and the
    /// [`crate::FocusRingPolicy`] shows the ring in the current [`crate::focus::UiInputMode`]
    /// (pointer focus never sets it under the default policy).
    pub focus_visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_tables_match_spec() {
        use WidgetSize::*;
        let c = UiDensity::Comfortable;
        assert_eq!(
            [c.control_height(Sm), c.control_height(Md), c.control_height(Lg)],
            [32.0, 44.0, 56.0]
        );
        assert_eq!([c.icon(Sm), c.icon(Md), c.icon(Lg)], [16.0, 24.0, 32.0]);
        assert_eq!([c.pad_x(Sm), c.pad_x(Md), c.pad_x(Lg)], [12.0, 16.0, 24.0]);
        assert_eq!(
            (c.touch_min(), c.row(), c.scrollbar()),
            (44.0, 44.0, (10.0, 14.0))
        );
        let k = UiDensity::Compact;
        assert_eq!(
            [k.control_height(Sm), k.control_height(Md), k.control_height(Lg)],
            [22.0, 28.0, 36.0]
        );
        assert_eq!([k.icon(Sm), k.icon(Md), k.icon(Lg)], [12.0, 16.0, 20.0]);
        assert_eq!([k.pad_x(Sm), k.pad_x(Md), k.pad_x(Lg)], [6.0, 8.0, 12.0]);
        assert_eq!((k.touch_min(), k.row(), k.scrollbar()), (28.0, 24.0, (8.0, 10.0)));
    }

    #[test]
    fn defaults_are_md_comfortable() {
        assert_eq!(WidgetSize::default(), WidgetSize::Md);
        assert_eq!(UiDensity::default(), UiDensity::Comfortable);
        assert!(!StyleState::default().disabled);
    }

    #[test]
    fn compact_is_always_smaller_than_comfortable() {
        for s in [WidgetSize::Sm, WidgetSize::Md, WidgetSize::Lg] {
            assert!(UiDensity::Compact.control_height(s) < UiDensity::Comfortable.control_height(s));
            assert!(UiDensity::Compact.pad_x(s) < UiDensity::Comfortable.pad_x(s));
        }
    }
}
