// SPDX-License-Identifier: MIT-0
//! The option panel and the product list, drawn with egui over the piece
//! (`docs/specs/42-product-viewer.md` § 9). Pure UI: it reads a model and reports actions, so tests
//! drive it with synthetic pointer events and no GPU.

use egui::{Color32, FontData, FontDefinitions, FontFamily, RichText, Vec2};

use serde_json::Value;

use crate::store::PackageEntry;

/// Smallest touch target, points (platform guidance: 48 dp).
pub const CHIP_HEIGHT: f32 = 44.0;

/// One value of a group, as the player's `options()` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelValue {
    /// What a pick names.
    pub value: String,
    /// What the chip says.
    pub label: String,
    /// A pick would resolve to something sold, one step from the current picks.
    pub available: bool,
}

/// One option group, as the player's `options()` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelGroup {
    /// The group's name.
    pub group: String,
    /// Takes a list of values.
    pub multi: bool,
    /// Picked values.
    pub current: Vec<String>,
    /// The values.
    pub values: Vec<PanelValue>,
}

/// The groups to offer, from the player's `options()` JSON. An add-on that does not apply to the current
/// variant is left out; anything malformed is left out too (the player never produces it).
#[must_use]
pub fn panel_groups(options: &Value) -> Vec<PanelGroup> {
    let text = |value: &Value, name: &str| value.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    options
        .as_array()
        .into_iter()
        .flatten()
        .filter(|group| group.get("applies").and_then(Value::as_bool).unwrap_or(true))
        .map(|group| PanelGroup {
            group: text(group, "group"),
            multi: group.get("kind").and_then(Value::as_str) == Some("multi"),
            current: match group.get("current") {
                Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
                Some(Value::Array(many)) => many
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            },
            values: group
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|value| PanelValue {
                    value: text(value, "value"),
                    label: text(value, "label"),
                    available: value.get("available").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect(),
        })
        .collect()
}

/// What a tap on `value` asks the player to pick: the label, or a multi group's list with it toggled.
#[must_use]
pub fn pick_value(group: &PanelGroup, value: &str) -> Value {
    if group.multi {
        let mut chosen = group.current.clone();
        if let Some(at) = chosen.iter().position(|c| c == value) {
            chosen.remove(at);
        } else {
            chosen.push(value.to_owned());
        }
        Value::from(chosen)
    } else {
        Value::from(value)
    }
}

/// What the person asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// A value was tapped. Selectable or not: the host applies it or shows the refusal.
    Pick {
        /// The group.
        group: String,
        /// The value's label.
        value: String,
    },
    /// Show the product list.
    ShowProducts,
    /// Back to the open product.
    HideProducts,
    /// Open a package from the list.
    Open(String),
}

/// What the panel shows this frame.
#[derive(Debug, Clone, Copy)]
pub struct PanelModel<'a> {
    /// The open product's name.
    pub title: &'a str,
    /// Its options, as of the current picks.
    pub groups: &'a [PanelGroup],
    /// The status line: what the current build is, a refusal, a download's progress.
    pub status: &'a str,
    /// The status line is a refusal or an error.
    pub status_is_error: bool,
    /// The product list replaces the options.
    pub showing_products: bool,
    /// A product is open, so the list has something to go back to.
    pub product_open: bool,
    /// Packages on the device.
    pub products: &'a [PackageEntry],
    /// Where packages can be copied to, for the empty list's hint.
    pub push_hint: &'a str,
    /// Insets the OS keeps for itself, points: left, top, right, bottom.
    pub safe: [f32; 4],
}

/// The panel's one font: the sample's copy of `regia_ui`'s Inter Regular (OFL-1.1, licence alongside), kept in
/// this crate so it builds from its own folder (ADR-1485). `tests/font_copy.rs` checks the bytes still match.
pub const INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");

/// Sets up egui for the app: one small font, larger touch-friendly spacing, dark.
pub fn configure(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    fonts.font_data.insert(
        "inter".to_owned(),
        std::sync::Arc::new(FontData::from_static(INTER_REGULAR)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("inter".to_owned());
    }
    ctx.set_fonts(fonts);
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(14.0, 8.0);
    });
}

/// The panel's side and size for a window of `size` points: below the piece in portrait, beside it
/// in landscape.
#[must_use]
pub fn panel_is_at_bottom(size: Vec2) -> bool {
    size.y >= size.x
}

/// Draws the panel, returning what was tapped and the rectangle it took (points).
pub fn show(ui: &mut egui::Ui, model: &PanelModel<'_>) -> (Vec<PanelAction>, egui::Rect) {
    let mut actions = Vec::new();
    let size = ui.max_rect().size();
    let frame = egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(18, 18, 22, 235))
        .inner_margin(egui::Margin {
            left: (10.0 + model.safe[0]) as i8,
            right: (10.0 + model.safe[2]) as i8,
            top: 8,
            bottom: (8.0 + model.safe[3]) as i8,
        });
    let at_bottom = panel_is_at_bottom(size);
    let panel = if at_bottom {
        egui::Panel::bottom("options")
            .resizable(false)
            .frame(frame)
            .exact_size(panel_extent(size, model))
    } else {
        egui::Panel::right("options")
            .resizable(false)
            .frame(frame)
            .exact_size((size.x * 0.38).clamp(220.0, 420.0))
    };
    let rect = panel
        .show(ui, |ui| {
            contents(ui, model, &mut actions);
        })
        .response
        .rect;
    (actions, rect)
}

/// The bottom panel's height: every row if it fits, never more than half the window.
fn panel_extent(size: Vec2, model: &PanelModel<'_>) -> f32 {
    let rows = if model.showing_products {
        model.products.len().max(2) + 1
    } else {
        model.groups.len()
    } as f32;
    // The header (title, status line, rule) and each row (label, chip, spacing, a finger's reach under it).
    let wanted = 76.0 + rows * (CHIP_HEIGHT + 25.0) + model.safe[3];
    wanted.min(size.y * 0.5)
}

fn contents(ui: &mut egui::Ui, model: &PanelModel<'_>, actions: &mut Vec<PanelAction>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(model.title).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if model.showing_products {
                // With nothing open (a first start, a refused package) there is nothing to go back to.
                if model.product_open && ui.button("Back").clicked() {
                    actions.push(PanelAction::HideProducts);
                }
            } else if ui.button("Products").clicked() {
                actions.push(PanelAction::ShowProducts);
            }
        });
    });
    let color = if model.status_is_error {
        Color32::from_rgb(255, 170, 120)
    } else {
        Color32::from_gray(190)
    };
    // One line, so the chips below do not move when a pick changes the statement. A refusal may wrap: it has to be read.
    let status = egui::Label::new(RichText::new(model.status).color(color).small());
    ui.add(if model.status_is_error {
        status
    } else {
        status.truncate()
    });
    ui.separator();
    touch_scroll(egui::ScrollArea::vertical())
        .auto_shrink(false)
        .show(ui, |ui| {
            if model.showing_products {
                product_rows(ui, model, actions);
            } else {
                option_rows(ui, model, actions);
            }
        });
}

/// The panel's scroll areas move by dragging their contents, with no bar. egui only drag-scrolls by default once
/// it has seen touch events, and the app hands it touches as pointer events, so on a phone a row did not move
/// under a finger: its bar, just under the chips, was the only way along it, and a finger that missed a chip
/// there jumped the row sideways and picked nothing. A chip cut at the edge shows there is more.
fn touch_scroll(area: egui::ScrollArea) -> egui::ScrollArea {
    area.scroll_source(egui::scroll_area::ScrollSource {
        drag: egui::scroll_area::DragScroll::Always,
        ..egui::scroll_area::ScrollSource::default()
    })
    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
}

fn option_rows(ui: &mut egui::Ui, model: &PanelModel<'_>, actions: &mut Vec<PanelAction>) {
    for group in model.groups {
        ui.label(RichText::new(&group.group).small().color(Color32::from_gray(170)));
        touch_scroll(egui::ScrollArea::horizontal())
            .id_salt(&group.group)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for value in &group.values {
                        let selected = group.current.contains(&value.value);
                        let text = if value.available || selected {
                            RichText::new(&value.label)
                        } else {
                            RichText::new(&value.label)
                                .color(Color32::from_gray(95))
                                .strikethrough()
                        };
                        let chip =
                            egui::Button::selectable(selected, text).min_size(Vec2::new(44.0, CHIP_HEIGHT));
                        // The current value of a single-choice group is already picked.
                        if ui.add(chip).clicked() && (group.multi || !selected) {
                            actions.push(PanelAction::Pick {
                                group: group.group.clone(),
                                value: value.value.clone(),
                            });
                        }
                    }
                });
            });
    }
}

fn product_rows(ui: &mut egui::Ui, model: &PanelModel<'_>, actions: &mut Vec<PanelAction>) {
    if model.products.is_empty() {
        ui.label(RichText::new(model.push_hint).small());
    }
    for entry in model.products {
        ui.horizontal(|ui| {
            let label = if entry.package_id.is_empty() {
                entry.slug.as_str()
            } else {
                entry.package_id.as_str()
            };
            let enabled = entry.problem.is_none();
            let button = egui::Button::new(label).min_size(Vec2::new(44.0, CHIP_HEIGHT));
            if ui.add_enabled(enabled, button).clicked() {
                actions.push(PanelAction::Open(entry.slug.clone()));
            }
            if let Some(problem) = &entry.problem {
                ui.label(
                    RichText::new(problem)
                        .small()
                        .color(Color32::from_rgb(255, 170, 120)),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect};
    use serde_json::json;

    use super::*;

    fn input(size: Vec2, events: Vec<Event>, time: f64) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            events,
            time: Some(time),
            ..RawInput::default()
        }
    }

    fn press(at: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn run(
        ctx: &egui::Context,
        size: Vec2,
        model: &PanelModel<'_>,
        events: Vec<Event>,
        time: f64,
    ) -> Vec<PanelAction> {
        let mut actions = Vec::new();
        let mut output = ctx.run_ui(input(size, events, time), |ui| {
            actions = show(ui, model).0;
        });
        // No painter here: egui asserts that a dropped delta was dealt with.
        output.textures_delta.clear();
        actions
    }

    /// The shape `Player::options` answers.
    fn groups() -> Vec<PanelGroup> {
        panel_groups(&json!([
            {"group": "Metal", "kind": "single", "current": "Gold", "applies": true, "values": [
                {"value": "Gold", "label": "Gold", "available": true},
                {"value": "Titanium", "label": "Titanium", "available": false, "reason": "not_sold_with"},
            ]},
            {"group": "Gem", "kind": "single", "current": "Clear", "applies": true, "values": [
                {"value": "Clear", "label": "Clear", "available": true},
                {"value": "Blue", "label": "Blue", "available": true},
            ]},
            {"group": "Extra", "kind": "single", "current": null, "applies": false, "values": [
                {"value": "Hidden", "label": "Hidden", "available": true},
            ]},
        ]))
    }

    fn model<'a>(groups: &'a [PanelGroup], products: &'a [PackageEntry], showing: bool) -> PanelModel<'a> {
        PanelModel {
            title: "Sample",
            groups,
            status: "ok",
            status_is_error: false,
            showing_products: showing,
            product_open: true,
            products,
            push_hint: "copy here",
            safe: [0.0; 4],
        }
    }

    /// Finds a tap position by sweeping the panel; the panel is laid out deterministically.
    fn tap_finding(
        ctx: &egui::Context,
        size: Vec2,
        model: &PanelModel<'_>,
        wanted: impl Fn(&PanelAction) -> bool,
    ) -> Option<PanelAction> {
        tap_position(ctx, size, model, wanted).map(|(_, found)| found)
    }

    /// Like `tap_finding`, and says where the tap landed.
    fn tap_position(
        ctx: &egui::Context,
        size: Vec2,
        model: &PanelModel<'_>,
        wanted: impl Fn(&PanelAction) -> bool,
    ) -> Option<(Pos2, PanelAction)> {
        let mut time = 1.0;
        for y in (0..size.y as i32).rev().step_by(6) {
            for x in (6..size.x as i32).step_by(14) {
                let at = Pos2::new(x as f32, y as f32);
                time += 1.0;
                run(ctx, size, model, vec![Event::PointerMoved(at)], time);
                time += 0.02;
                run(ctx, size, model, vec![press(at, true)], time);
                time += 0.02;
                let actions = run(ctx, size, model, vec![press(at, false), Event::PointerGone], time);
                if let Some(found) = actions.into_iter().find(&wanted) {
                    return Some((at, found));
                }
            }
        }
        None
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        configure(&ctx);
        ctx
    }

    const PHONE: Vec2 = Vec2::new(400.0, 800.0);

    /// A pick rewrites the status line, so a statement of any length must leave the chips where the finger is.
    #[test]
    fn a_long_status_leaves_the_chips_where_they_are() {
        const NARROW: Vec2 = Vec2::new(180.0, 360.0);
        let groups = groups();
        let is_blue = |a: &PanelAction| matches!(a, PanelAction::Pick { value, .. } if value == "Blue");
        let short = model(&groups, &[], false);
        let (at, _) = tap_position(&context(), NARROW, &short, is_blue).expect("a chip with a short status");
        let long = PanelModel {
            status: "Stand-in | Material: 14K Yellow Gold | Gem Size: 3x1.5mm | Threading: Threadless | Gauge: 18g | Gem: Clear CZ",
            ..model(&groups, &[], false)
        };
        let ctx = context();
        run(&ctx, NARROW, &long, vec![Event::PointerMoved(at)], 1.0);
        run(&ctx, NARROW, &long, vec![press(at, true)], 1.02);
        let actions = run(
            &ctx,
            NARROW,
            &long,
            vec![press(at, false), Event::PointerGone],
            1.04,
        );
        assert!(actions.iter().any(is_blue), "{actions:?}");
    }

    #[test]
    fn a_tap_on_a_chip_reports_its_pick() {
        let (ctx, groups) = (context(), groups());
        let model = model(&groups, &[], false);
        let found = tap_finding(
            &ctx,
            PHONE,
            &model,
            |a| matches!(a, PanelAction::Pick { group, value } if group == "Gem" && value == "Blue"),
        );
        assert!(found.is_some());
    }

    #[test]
    fn an_unsold_value_is_still_tappable_so_the_refusal_can_be_said() {
        let (ctx, groups) = (context(), groups());
        let model = model(&groups, &[], false);
        let found = tap_finding(
            &ctx,
            PHONE,
            &model,
            |a| matches!(a, PanelAction::Pick { value, .. } if value == "Titanium"),
        );
        assert!(found.is_some());
    }

    #[test]
    fn a_tap_on_the_current_value_of_a_single_group_asks_nothing() {
        let (ctx, groups) = (context(), groups());
        let model = model(&groups, &[], false);
        let found = tap_finding(
            &ctx,
            PHONE,
            &model,
            |a| matches!(a, PanelAction::Pick { value, .. } if value == "Gold" || value == "Clear"),
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_multi_group_toggles_and_a_single_group_names_the_value() {
        let multi = PanelGroup {
            group: "Extras".to_owned(),
            multi: true,
            current: vec!["A".to_owned()],
            values: Vec::new(),
        };
        assert_eq!(pick_value(&multi, "B"), json!(["A", "B"]));
        assert_eq!(pick_value(&multi, "A"), json!([]));
        let single = PanelGroup {
            multi: false,
            ..multi
        };
        assert_eq!(pick_value(&single, "B"), json!("B"));
    }

    #[test]
    fn a_group_that_does_not_apply_is_not_offered() {
        let (ctx, groups) = (context(), groups());
        let model = model(&groups, &[], false);
        let found = tap_finding(
            &ctx,
            PHONE,
            &model,
            |a| matches!(a, PanelAction::Pick { group, .. } if group == "Extra"),
        );
        assert_eq!(found, None);
    }

    #[test]
    fn the_list_offers_back_only_when_a_product_is_open() {
        const NARROW: Vec2 = Vec2::new(180.0, 360.0);
        let is_back = |action: &PanelAction| *action == PanelAction::HideProducts;
        let groups = Vec::new();
        let open = model(&groups, &[], true);
        assert!(
            tap_finding(&context(), NARROW, &open, is_back).is_some(),
            "Back is there"
        );
        let nothing_open = PanelModel {
            product_open: false,
            ..open
        };
        assert!(
            tap_finding(&context(), NARROW, &nothing_open, is_back).is_none(),
            "no Back with nothing to go back to"
        );
    }

    #[test]
    fn the_product_list_offers_good_packages_and_shows_bad_ones() {
        let (ctx, groups) = (context(), Vec::new());
        let products = [
            PackageEntry {
                slug: "good".to_owned(),
                dir: PathBuf::new(),
                package_id: "h/good".to_owned(),
                problem: None,
            },
            PackageEntry {
                slug: "bad".to_owned(),
                dir: PathBuf::new(),
                package_id: String::new(),
                problem: Some("pack_bad_manifest".to_owned()),
            },
        ];
        let model = model(&groups, &products, true);
        let opened = tap_finding(&ctx, PHONE, &model, |a| matches!(a, PanelAction::Open(_)));
        assert_eq!(opened, Some(PanelAction::Open("good".to_owned())));
    }

    /// One tap: hover, press, release, as a finger's touch arrives through winit.
    fn tap_at(
        ctx: &egui::Context,
        size: Vec2,
        model: &PanelModel<'_>,
        at: Pos2,
        time: &mut f64,
    ) -> Vec<PanelAction> {
        *time += 1.0;
        run(ctx, size, model, vec![Event::PointerMoved(at)], *time);
        *time += 0.02;
        run(ctx, size, model, vec![press(at, true)], *time);
        *time += 0.02;
        run(
            ctx,
            size,
            model,
            vec![press(at, false), Event::PointerGone],
            *time,
        )
    }

    /// A product whose first row is wider than a phone, with two short rows under it.
    fn long_row_groups() -> Vec<PanelGroup> {
        let long = |v: &str| json!({"value": v, "label": format!("14K {v} Gold Alloy"), "available": true});
        let short = |v: &str| json!({"value": v, "label": v, "available": true});
        panel_groups(&json!([
            {"group": "Metal", "kind": "single", "current": "A", "applies": true,
             "values": [long("A"), long("B"), long("C"), long("D"), long("E"), long("F")]},
            {"group": "Size", "kind": "single", "current": "S", "applies": true,
             "values": [short("S"), short("M")]},
            {"group": "Gem", "kind": "single", "current": "Clear", "applies": true,
             "values": [short("Clear"), short("Blue")]},
        ]))
    }

    /// Seen on a Fairphone 5 (11-05): a tap just under a chip row, where the row's horizontal scroll bar
    /// sits, jumped the row sideways and picked nothing, so the next tap on a chip hit another chip.
    #[test]
    fn a_tap_under_a_row_that_overflows_does_not_scroll_it() {
        let ctx = context();
        let groups = long_row_groups();
        let model = model(&groups, &[], false);
        let mut time = 0.0;
        let picks_b = |actions: &[PanelAction]| {
            actions
                .iter()
                .any(|a| matches!(a, PanelAction::Pick { value, .. } if value == "B"))
        };
        // The chip `B`, found from the top down so no probe lands under the row before it is found.
        let mut chip = None;
        'sweep: for y in (0..PHONE.y as i32).step_by(4) {
            for x in (6..PHONE.x as i32).step_by(10) {
                let at = Pos2::new(x as f32, y as f32);
                if picks_b(&tap_at(&ctx, PHONE, &model, at, &mut time)) {
                    chip = Some(at);
                    break 'sweep;
                }
            }
        }
        let chip = chip.expect("chip B is on screen");
        // Down the chip to its lowest tappable point (taps on the chip itself are harmless).
        let mut bottom = chip.y;
        while picks_b(&tap_at(
            &ctx,
            PHONE,
            &model,
            Pos2::new(chip.x, bottom + 1.0),
            &mut time,
        )) {
            bottom += 1.0;
        }
        // The band under the row, across the whole width.
        // The phone tap that jumped the row was 7 px (about 2 points) under the chip.
        for dy in 1..=10 {
            for x in (6..PHONE.x as i32).step_by(20) {
                let at = Pos2::new(x as f32, bottom + dy as f32);
                // Near the chip, a tap may still pick it (egui's touch radius); it must never move the row.
                tap_at(&ctx, PHONE, &model, at, &mut time);
                assert!(
                    picks_b(&tap_at(&ctx, PHONE, &model, chip, &mut time)),
                    "a tap at {at:?} under the row moved it"
                );
            }
        }
        let again = tap_at(&ctx, PHONE, &model, chip, &mut time);
        assert_eq!(
            again,
            vec![PanelAction::Pick {
                group: "Metal".to_owned(),
                value: "B".to_owned()
            }],
            "the row moved under the finger"
        );
    }

    /// A long row still scrolls with no bar: a finger drag moves it, and the release picks nothing.
    #[test]
    fn a_drag_along_a_row_that_overflows_scrolls_it() {
        let ctx = context();
        let groups = long_row_groups();
        let model = model(&groups, &[], false);
        let mut time = 0.0;
        let mut chip = None;
        'sweep: for y in (0..PHONE.y as i32).step_by(4) {
            for x in (6..PHONE.x as i32).step_by(10) {
                let at = Pos2::new(x as f32, y as f32);
                let actions = tap_at(&ctx, PHONE, &model, at, &mut time);
                if actions
                    .iter()
                    .any(|a| matches!(a, PanelAction::Pick { value, .. } if value == "B"))
                {
                    chip = Some(at);
                    break 'sweep;
                }
            }
        }
        let chip = chip.expect("chip B is on screen");
        let from = Pos2::new(PHONE.x - 20.0, chip.y + 4.0);
        time += 1.0;
        run(&ctx, PHONE, &model, vec![Event::PointerMoved(from)], time);
        time += 0.02;
        run(&ctx, PHONE, &model, vec![press(from, true)], time);
        let mut released = Vec::new();
        for step in 1..=30 {
            let at = Pos2::new(from.x - step as f32 * 10.0, from.y);
            time += 0.016;
            run(&ctx, PHONE, &model, vec![Event::PointerMoved(at)], time);
            if step == 30 {
                time += 0.016;
                released = run(
                    &ctx,
                    PHONE,
                    &model,
                    vec![press(at, false), Event::PointerGone],
                    time,
                );
            }
        }
        assert_eq!(released, Vec::new(), "the end of a drag is not a tap");
        let after = tap_at(&ctx, PHONE, &model, chip, &mut time);
        assert!(
            matches!(after.as_slice(), [PanelAction::Pick { value, .. }] if value != "B"),
            "the row did not move: {after:?}"
        );
    }

    /// The tappable height of chip `b` in `group`, at the first x where a tap picks it.
    fn chip_height(ctx: &egui::Context, model: &PanelModel<'_>, group: &str, time: &mut f64) -> f32 {
        let is_b = |actions: &[PanelAction]| {
            actions
                .iter()
                .any(|a| matches!(a, PanelAction::Pick { group: g, value } if g == group && value == "b"))
        };
        let mut x_hit = None;
        'find: for y in (0..PHONE.y as i32).step_by(4) {
            for x in (6..PHONE.x as i32).step_by(10) {
                if is_b(&tap_at(ctx, PHONE, model, Pos2::new(x as f32, y as f32), time)) {
                    x_hit = Some(x as f32 + 5.0);
                    break 'find;
                }
            }
        }
        let x = x_hit.unwrap_or_else(|| panic!("chip b of {group} is on screen"));
        (0..PHONE.y as i32)
            .filter(|y| is_b(&tap_at(ctx, PHONE, model, Pos2::new(x, *y as f32), time)))
            .count() as f32
    }

    /// The bottom panel is tall enough for its rows: the last row is not cut by the panel's edge.
    #[test]
    fn the_last_row_of_a_short_product_is_not_cut() {
        let product = |rows: usize| {
            panel_groups(&serde_json::Value::Array(
                (0..rows)
                    .map(|i| {
                        json!({"group": format!("G{i}"), "kind": "single", "current": "a", "applies": true,
                               "values": [{"value": "a", "label": "Aa", "available": true},
                                          {"value": "b", "label": "Bb", "available": true}]})
                    })
                    .collect(),
            ))
        };
        // A chip in the top row is never cut: its height to a finger is the reference.
        let groups = product(4);
        let mut time = 0.0;
        let full = chip_height(&context(), &model(&groups, &[], false), "G0", &mut time);
        for rows in 1..=4 {
            let groups = product(rows);
            let last = chip_height(
                &context(),
                &model(&groups, &[], false),
                &format!("G{}", rows - 1),
                &mut time,
            );
            assert!(
                (full - last).abs() <= 1.0,
                "{rows} rows: the last chip is {last} points tall to a finger, a whole one {full}"
            );
        }
    }

    #[test]
    fn the_panel_never_takes_over_half_a_portrait_window_and_goes_beside_in_landscape() {
        let (ctx, groups) = (context(), groups());
        let model = model(&groups, &[], false);
        let mut rect = Rect::NOTHING;
        let mut output = ctx.run_ui(input(Vec2::new(400.0, 500.0), vec![], 0.0), |ui| {
            rect = show(ui, &model).1;
        });
        output.textures_delta.clear();
        assert!(rect.height() <= 250.5, "{rect:?}");
        assert!(rect.width() >= 399.0, "{rect:?}");
        let mut output = ctx.run_ui(input(Vec2::new(800.0, 400.0), vec![], 0.1), |ui| {
            rect = show(ui, &model).1;
        });
        output.textures_delta.clear();
        assert!(rect.width() <= 800.0 * 0.38 + 0.5, "{rect:?}");
        assert!(rect.height() >= 399.0, "{rect:?}");
    }
}
