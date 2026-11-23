// SPDX-License-Identifier: MPL-2.0
//! SpaceMouse HID report decoding (`docs/specs/29-spacemouse.md` § 3), pure functions only.
//!
//! Sources are public: the USB HID 1.11 specification (report-descriptor item encoding, the
//! Generic Desktop and Button usage pages) and the open-source spacenav project's protocol notes
//! for the known report layouts. No vendor SDK, driver source or header is used.
//!
//! Two ways to decode a report:
//!
//! * **By descriptor** ([`ReportLayout::from_descriptor`]) — the device publishes its HID report
//!   descriptor (Linux exposes it in sysfs); the Input items give each axis and button field's
//!   report id, bit offset and size.
//! * **By the known layouts** (`ReportLayout`s the module knows (the product table below)) when no descriptor is available:
//!   report 1 translation `x, y, z` as little-endian `i16` (12 bytes on models that send
//!   translation and rotation together), report 2 rotation `rx, ry, rz`, report 3 a button
//!   bitmask.
//!
//! The device's own axis frame (+X right, +Y toward the user, +Z down, right-handed rotations
//! about those axes) is mapped to the spec frame by [`device_to_spec`].

use super::SpaceMouseState;

/// The USB vendor id of 3Dconnexion.
pub const VENDOR_3DCONNEXION: u16 = 0x256F;
/// The USB vendor id of Logitech (legacy 3Dconnexion devices were sold under it).
pub const VENDOR_LOGITECH: u16 = 0x046D;

/// Legacy 3Dconnexion product ids under [`VENDOR_LOGITECH`] (public USB id lists).
const LOGITECH_PRODUCTS: &[u16] = &[
    0xC603, 0xC605, 0xC606, 0xC621, 0xC623, 0xC625, 0xC626, 0xC627, 0xC628, 0xC629, 0xC62B,
];

/// 3Dconnexion product ids that are 6-DoF devices (used only when the descriptor cannot be
/// read; with a descriptor, any multi-axis controller under [`VENDOR_3DCONNEXION`] counts).
const KNOWN_3DCONNEXION_PRODUCTS: &[u16] = &[0xC62E, 0xC62F, 0xC631, 0xC632, 0xC633, 0xC635, 0xC636, 0xC652];

/// The SpaceMouse Compact (its recorded descriptor is
/// [`tests::COMPACT_DESCRIPTOR_RECORDED`]: split reports, two buttons, constant padding).
const COMPACT_PRODUCT: u16 = 0xC635;

/// Two-button models by `(vendor, product)` (public USB id lists): the SpaceNavigator and the
/// SpaceNavigator for Notebooks under [`VENDOR_LOGITECH`]; the SpaceMouse Wireless (cable and
/// receiver) and the SpaceMouse Compact under [`VENDOR_3DCONNEXION`].
const TWO_BUTTON_PRODUCTS: &[(u16, u16)] = &[
    (VENDOR_LOGITECH, 0xC626),
    (VENDOR_LOGITECH, 0xC628),
    (VENDOR_3DCONNEXION, 0xC62E),
    (VENDOR_3DCONNEXION, 0xC62F),
    (VENDOR_3DCONNEXION, COMPACT_PRODUCT),
];

/// Report-3 button bits the known layout decodes for `vendor:product` when the device's
/// descriptor cannot be read: 2 for the two-button models (their other bits are padding: a set
/// padding bit must not read as a pressed button, it toggles dominant-axis mode via button 3;
/// audit finding 5, ADR-4768), 32 for every other product (a SpaceMouse Pro has 15 buttons, an
/// Enterprise 31; without its descriptor, which Windows does not report, its extra buttons would
/// be lost: fixrev review finding 4, corr-input-world F3).
#[must_use]
pub fn known_button_count(vendor: u16, product: u16) -> u8 {
    if TWO_BUTTON_PRODUCTS.contains(&(vendor, product)) {
        2
    } else {
        32
    }
}

/// [`known_button_count`] when only the product id is known: an unidentified product (`0`) and
/// any product id of the two-button table read two bits, every other product all 32. The known
/// product ids of the two vendors do not overlap.
fn product_button_count(product_id: u16) -> u8 {
    if product_id == 0 || TWO_BUTTON_PRODUCTS.iter().any(|&(_, p)| p == product_id) {
        2
    } else {
        32
    }
}

/// Whether `vendor:product` is a SpaceMouse-family device. `descriptor_says_multi_axis` is
/// `Some(true)` when the device's report descriptor declares a Generic Desktop multi-axis
/// controller collection, `None` when it could not be read.
pub fn is_spacemouse(vendor: u16, product: u16, descriptor_says_multi_axis: Option<bool>) -> bool {
    match vendor {
        VENDOR_LOGITECH => LOGITECH_PRODUCTS.contains(&product),
        VENDOR_3DCONNEXION => {
            descriptor_says_multi_axis.unwrap_or_else(|| KNOWN_3DCONNEXION_PRODUCTS.contains(&product))
        }
        _ => false,
    }
}

/// What a decoded field carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    /// Device axis 0..6 (`x, y, z, rx, ry, rz` in the device frame).
    Axis(u8),
    /// Button `n` (1-based).
    Button(u8),
}

/// One field of an input report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Field {
    report_id: u8,
    bit_offset: u32,
    bit_size: u32,
    signed: bool,
    kind: FieldKind,
}

/// Where each axis and button lives in the device's input reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLayout {
    fields: Vec<Field>,
    /// Whether reports start with a report id byte.
    numbered: bool,
}

/// One decoded report: the parts it updated, in the device frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportUpdate {
    /// Device-frame `x, y, z`, raw counts.
    pub translation: Option<[i32; 3]>,
    /// Device-frame `rx, ry, rz`, raw counts.
    pub rotation: Option<[i32; 3]>,
    /// Pressed buttons, bit `n - 1` for button `n`.
    pub buttons: Option<u32>,
}

impl ReportUpdate {
    /// True when the report carried nothing this decoder understands.
    pub fn is_empty(&self) -> bool {
        self.translation.is_none() && self.rotation.is_none() && self.buttons.is_none()
    }
}

const GENERIC_DESKTOP: u32 = 0x01;
const BUTTON_PAGE: u32 = 0x09;
const USAGE_MULTI_AXIS: u32 = 0x08;
const USAGE_X: u32 = 0x30;
const USAGE_RZ: u32 = 0x35;

/// The global item state of the descriptor parser (HID 1.11 § 6.2.2.7), pushable.
#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    usage_page: u32,
    logical_min: i32,
    report_size: u32,
    report_id: u8,
    report_count: u32,
}

fn item_unsigned(data: &[u8]) -> u32 {
    data.iter().rev().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
}

fn item_signed(data: &[u8]) -> i32 {
    match data.len() {
        1 => i32::from(data[0] as i8),
        2 => i32::from(i16::from_le_bytes([data[0], data[1]])),
        4 => i32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        _ => 0,
    }
}

/// A usage as `(page << 16) | id`, completing a 1- or 2-byte local usage with the current page.
fn full_usage(data: &[u8], page: u32) -> u32 {
    let value = item_unsigned(data);
    if data.len() == 4 {
        value
    } else {
        ((page & 0xFFFF) << 16) | value
    }
}

impl ReportLayout {
    /// The known layouts (§ 3) for devices whose descriptor cannot be read: report 1
    /// translation (or translation + rotation when it is 12 bytes long), report 2 rotation,
    /// report 3 buttons, all little-endian. `product_id` selects the button count the layout
    /// decodes (`product_button_count`): two-button products keep their padding-bit rejection,
    /// multi-button products decode every button bit their report carries.
    pub fn known(product_id: u16) -> Self {
        Self::known_with_buttons(product_button_count(product_id))
    }

    /// The known layouts with `buttons` report-3 button bits (clamped to `1..=32`). Without a
    /// descriptor, [`known_button_count`] picks the count by product: the two-button models read
    /// two bits (the rest of their byte is padding), every other product all 32, as before
    /// ADR-4768 (corr-input-world: the Windows HID backend never reads a descriptor, so a
    /// SpaceMouse Pro or Enterprise there lost every button from 3 on).
    pub fn known_with_buttons(buttons: u8) -> Self {
        let axis = |report_id: u8, index: u8, slot: u32| Field {
            report_id,
            bit_offset: slot * 16,
            bit_size: 16,
            signed: true,
            kind: FieldKind::Axis(index),
        };
        let mut fields = Vec::new();
        for i in 0..3u8 {
            fields.push(axis(1, i, u32::from(i)));
            // The combined layout: rotation follows translation in report 1.
            fields.push(axis(1, i + 3, u32::from(i) + 3));
            fields.push(axis(2, i + 3, u32::from(i)));
        }
        for n in 1..=buttons.clamp(1, 32) {
            fields.push(Field {
                report_id: 3,
                bit_offset: u32::from(n - 1),
                bit_size: 1,
                signed: false,
                kind: FieldKind::Button(n),
            });
        }
        Self {
            fields,
            numbered: true,
        }
    }

    /// Reads the axis and button fields from a HID report descriptor. Returns `None` when the
    /// descriptor is malformed or declares no axes.
    pub fn from_descriptor(descriptor: &[u8]) -> Option<Self> {
        let mut globals = Globals::default();
        let mut stack: Vec<Globals> = Vec::new();
        let mut usages: Vec<u32> = Vec::new();
        let mut usage_min: Option<u32> = None;
        let mut usage_max: Option<u32> = None;
        let mut offsets: [u32; 256] = [0; 256];
        let mut fields = Vec::new();
        let mut numbered = false;
        let mut i = 0;
        while i < descriptor.len() {
            let prefix = descriptor[i];
            if prefix == 0xFE {
                // Long item: size byte, tag byte, data (none are defined for reports).
                let size = usize::from(*descriptor.get(i + 1)?);
                i += 3 + size;
                continue;
            }
            let size = match prefix & 0x03 {
                3 => 4,
                n => usize::from(n),
            };
            let data = descriptor.get(i + 1..i + 1 + size)?;
            i += 1 + size;
            let item_type = (prefix >> 2) & 0x03;
            let tag = prefix >> 4;
            match (item_type, tag) {
                // Main: Input.
                (0, 0x8) => {
                    let flags = item_unsigned(data);
                    let constant = flags & 0x01 != 0;
                    let variable = flags & 0x02 != 0;
                    let id = usize::from(globals.report_id);
                    let mut offset = offsets[id];
                    for k in 0..globals.report_count {
                        let usage = if let (Some(min), Some(max)) = (usage_min, usage_max) {
                            min.checked_add(k).filter(|u| *u <= max)
                        } else {
                            usages.get(k as usize).or(usages.last()).copied()
                        };
                        if !constant
                            && variable
                            && let Some(usage) = usage
                            && let Some(kind) = classify(usage)
                        {
                            fields.push(Field {
                                report_id: globals.report_id,
                                bit_offset: offset,
                                bit_size: globals.report_size,
                                signed: globals.logical_min < 0,
                                kind,
                            });
                        }
                        offset = offset.saturating_add(globals.report_size);
                    }
                    offsets[id] = offset;
                }
                // Main: Output, Feature, Collection, End Collection — only clear the locals.
                (0, _) => {}
                // Global items.
                (1, 0x0) => globals.usage_page = item_unsigned(data),
                (1, 0x1) => globals.logical_min = item_signed(data),
                (1, 0x7) => globals.report_size = item_unsigned(data).min(32),
                (1, 0x8) => {
                    globals.report_id = u8::try_from(item_unsigned(data)).ok()?;
                    numbered = true;
                }
                (1, 0x9) => globals.report_count = item_unsigned(data).min(1024),
                (1, 0xA) => stack.push(globals),
                (1, 0xB) => globals = stack.pop()?,
                (1, _) => {}
                // Local items.
                (2, 0x0) => usages.push(full_usage(data, globals.usage_page)),
                (2, 0x1) => usage_min = Some(full_usage(data, globals.usage_page)),
                (2, 0x2) => usage_max = Some(full_usage(data, globals.usage_page)),
                _ => {}
            }
            if item_type == 0 {
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
        }
        fields
            .iter()
            .any(|f| matches!(f.kind, FieldKind::Axis(_)))
            .then_some(Self { fields, numbered })
    }

    /// Decodes one input report (as read from the OS: report id first when the device numbers
    /// its reports). Fields that do not fit the report are skipped; for the combined known
    /// layout, rotation is only taken from report 1 when the report is long enough.
    pub fn decode(&self, report: &[u8]) -> ReportUpdate {
        let (id, payload) = if self.numbered {
            match report.split_first() {
                Some((id, rest)) => (*id, rest),
                None => return ReportUpdate::default(),
            }
        } else {
            (0, report)
        };
        let bits = payload.len() as u32 * 8;
        let mut axes: [Option<i32>; 6] = [None; 6];
        let mut buttons: Option<u32> = None;
        for field in self.fields.iter().filter(|f| f.report_id == id) {
            if field.bit_offset + field.bit_size > bits || field.bit_size == 0 {
                continue;
            }
            let raw = read_bits(payload, field.bit_offset, field.bit_size);
            match field.kind {
                FieldKind::Axis(index) => {
                    let value = if field.signed {
                        sign_extend(raw, field.bit_size)
                    } else {
                        raw as i32
                    };
                    axes[usize::from(index)] = Some(value);
                }
                FieldKind::Button(n) => {
                    let mask = buttons.get_or_insert(0);
                    if raw != 0 && n <= 32 {
                        *mask |= 1 << (n - 1);
                    }
                }
            }
        }
        let triple = |a: &[Option<i32>]| match a {
            [Some(x), Some(y), Some(z)] => Some([*x, *y, *z]),
            _ => None,
        };
        ReportUpdate {
            translation: triple(&axes[0..3]),
            rotation: triple(&axes[3..6]),
            buttons,
        }
    }
}

/// Whether a descriptor declares a Generic Desktop multi-axis controller (the SpaceMouse's
/// application collection usage).
pub fn declares_multi_axis(descriptor: &[u8]) -> bool {
    // Usage Page (Generic Desktop) followed by Usage (Multi-axis Controller) before the first
    // Collection; a short scan of the first items is enough.
    let mut page = 0;
    let mut i = 0;
    while i < descriptor.len() {
        let prefix = descriptor[i];
        if prefix == 0xFE {
            return false;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => usize::from(n),
        };
        let Some(data) = descriptor.get(i + 1..i + 1 + size) else {
            return false;
        };
        match prefix & 0xFC {
            0x04 => page = item_unsigned(data),
            0x08 if full_usage(data, page) == (GENERIC_DESKTOP << 16) | USAGE_MULTI_AXIS => return true,
            0xA0 => return false,
            _ => {}
        }
        i += 1 + size;
    }
    false
}

fn classify(usage: u32) -> Option<FieldKind> {
    let (page, id) = (usage >> 16, usage & 0xFFFF);
    match page {
        GENERIC_DESKTOP if (USAGE_X..=USAGE_RZ).contains(&id) => Some(FieldKind::Axis((id - USAGE_X) as u8)),
        BUTTON_PAGE if (1..=32).contains(&id) => Some(FieldKind::Button(id as u8)),
        _ => None,
    }
}

fn read_bits(bytes: &[u8], offset: u32, size: u32) -> u32 {
    let mut value: u64 = 0;
    for k in 0..size {
        let bit = offset + k;
        let byte = bytes[(bit / 8) as usize];
        if byte >> (bit % 8) & 1 != 0 {
            value |= 1 << k;
        }
    }
    value as u32
}

fn sign_extend(raw: u32, bits: u32) -> i32 {
    if bits >= 32 {
        return raw as i32;
    }
    let shift = 32 - bits;
    ((raw << shift) as i32) >> shift
}

/// Maps device-frame raw values (+X right, +Y toward the user, +Z down) to the spec frame
/// (§ 2.1: +X right, +Y up, +Z toward the viewer; `rx` tilt away, `ry` twist counter-clockwise
/// from above, `rz` tilt left all positive).
pub fn device_to_spec(translation: [i32; 3], rotation: [i32; 3]) -> ([i32; 3], [i32; 3]) {
    let [x, y, z] = translation;
    let [rx, ry, rz] = rotation;
    // A right-handed rotation about the device's +X tips the cap top toward the user, about
    // +Z (down) turns it clockwise seen from above, about +Y (toward the user) tilts it left.
    (
        [x, z.saturating_neg(), y],
        [rx.saturating_neg(), rz.saturating_neg(), ry],
    )
}

/// Decoding state of one device: the latest value of every part, since devices send
/// translation, rotation and buttons in separate reports.
#[derive(Debug, Clone)]
pub struct DeviceDecoder {
    layout: ReportLayout,
    translation: [i32; 3],
    rotation: [i32; 3],
    buttons: u32,
}

impl DeviceDecoder {
    /// A decoder using `descriptor` when it parses, the known layouts otherwise.
    /// `product_id` selects the descriptor-less button count (see
    /// [`ReportLayout::known`]); pass `0` when the product is unknown (the conservative
    /// two-button layout).
    pub fn new(descriptor: Option<&[u8]>, product_id: u16) -> Self {
        let layout = descriptor
            .and_then(ReportLayout::from_descriptor)
            .unwrap_or_else(|| ReportLayout::known(product_id));
        Self::with_layout(layout)
    }

    /// A decoder for the device `vendor:product`: `descriptor` when it parses, else the known
    /// layouts with that product's button count ([`known_button_count`]). The HID backend builds
    /// its decoders with this; on Windows it never has a descriptor.
    pub fn for_device(descriptor: Option<&[u8]>, vendor: u16, product: u16) -> Self {
        let layout = descriptor
            .and_then(ReportLayout::from_descriptor)
            .unwrap_or_else(|| ReportLayout::known_with_buttons(known_button_count(vendor, product)));
        Self::with_layout(layout)
    }

    fn with_layout(layout: ReportLayout) -> Self {
        Self {
            layout,
            translation: [0; 3],
            rotation: [0; 3],
            buttons: 0,
        }
    }

    /// Feeds one report; returns true when it changed the state.
    pub fn feed(&mut self, report: &[u8]) -> bool {
        let update = self.layout.decode(report);
        let before = (self.translation, self.rotation, self.buttons);
        if let Some(t) = update.translation {
            self.translation = t;
        }
        if let Some(r) = update.rotation {
            self.rotation = r;
        }
        if let Some(b) = update.buttons {
            self.buttons = b;
        }
        before != (self.translation, self.rotation, self.buttons)
    }

    /// The device's state, normalised in the spec frame.
    pub fn state(&self) -> SpaceMouseState {
        let (t, r) = device_to_spec(self.translation, self.rotation);
        SpaceMouseState::from_raw(t, r, self.buttons)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn oversized_usage_page_does_not_overflow() {
        // Usage Page 0x00010000 (4 bytes), then a 1-byte Usage: the shift must not overflow.
        let _ = full_usage(&[0x30], 0x0001_0000);
        let _ = declares_multi_axis(&[0x07, 0x00, 0x00, 0x01, 0x00, 0x09, 0x30]);
    }

    #[test]
    fn usage_range_at_u32_max_does_not_overflow() {
        let d = [
            0x06, 0xFF, 0xFF, 0x1A, 0xFF, 0xFF, 0x2A, 0xFF, 0xFF, 0x75, 0x20, 0x95, 0x02, 0x81, 0x02,
        ];
        let _ = ReportLayout::from_descriptor(&d);
        let _ = DeviceDecoder::new(Some(&d), 0);
    }

    #[test]
    fn device_to_spec_survives_i32_min() {
        let (t, r) = device_to_spec([0, 0, i32::MIN], [i32::MIN, 0, i32::MIN]);
        assert_eq!(t[1], i32::MAX);
        assert_eq!(r, [i32::MAX, i32::MAX, 0]);
    }

    /// A split-report descriptor of the classic wired SpaceMouse shape, written from the HID
    /// 1.11 item encoding: report 1 `X, Y, Z`, report 2 `Rx, Ry, Rz` (16-bit, ±350), report 3
    /// two buttons plus padding.
    pub(crate) const SPLIT_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x08, // Usage (Multi-axis Controller)
        0xA1, 0x01, // Collection (Application)
        0xA1, 0x00, //   Collection (Physical)
        0x85, 0x01, //     Report ID 1
        0x16, 0xA2, 0xFE, // Logical Minimum (-350)
        0x26, 0x5E, 0x01, // Logical Maximum (350)
        0x09, 0x30, 0x09, 0x31, 0x09, 0x32, // Usage X, Y, Z
        0x75, 0x10, 0x95, 0x03, // Report Size 16, Count 3
        0x81, 0x02, // Input (Data, Var, Abs)
        0xC0, //   End Collection
        0xA1, 0x00, //   Collection (Physical)
        0x85, 0x02, //     Report ID 2
        0x09, 0x33, 0x09, 0x34, 0x09, 0x35, // Usage Rx, Ry, Rz
        0x75, 0x10, 0x95, 0x03, 0x81, 0x02, // Input 3 x 16 bit
        0xC0, //   End Collection
        0xA1, 0x02, //   Collection (Logical)
        0x85, 0x03, //     Report ID 3
        0x05, 0x09, //     Usage Page (Button)
        0x19, 0x01, 0x29, 0x02, // Usage Minimum 1, Maximum 2
        0x15, 0x00, 0x25, 0x01, // Logical 0..1
        0x75, 0x01, 0x95, 0x02, 0x81, 0x02, // Input 2 x 1 bit
        0x95, 0x06, 0x81, 0x03, // Input 6 x 1 bit constant (padding)
        0xC0, //   End Collection
        0xC0, // End Collection
    ];

    /// A combined-report descriptor (the newer wireless shape): report 1 carries all six axes,
    /// report 3 a 16-button field.
    pub(crate) const COMBINED_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, 0x09, 0x08, 0xA1, 0x01, // Generic Desktop / Multi-axis, Application
        0x85, 0x01, // Report ID 1
        0x16, 0xA2, 0xFE, 0x26, 0x5E, 0x01, // Logical ±350
        0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x33, 0x09, 0x34, 0x09, 0x35, // X Y Z Rx Ry Rz
        0x75, 0x10, 0x95, 0x06, 0x81, 0x02, // Input 6 x 16 bit
        0x85, 0x03, // Report ID 3
        0x05, 0x09, 0x19, 0x01, 0x29, 0x10, // Buttons 1..16
        0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x10, 0x81, 0x02, // Input 16 x 1 bit
        0xC0,
    ];

    /// The report descriptor a SpaceMouse Compact (256f:c635) publishes, recorded 2026-10-15
    /// from `/sys/class/hidraw/*/device/report_descriptor` on the owner's machine: split
    /// reports (1 = X Y Z, 2 = Rx Ry Rz, 16-bit ±350, relative), report 3 = two buttons + 14
    /// padding bits, an LED output report and vendor feature reports.
    pub(crate) const COMPACT_DESCRIPTOR_RECORDED: &[u8] = &[
        0x05, 0x01, 0x09, 0x08, 0xA1, 0x01, 0xA1, 0x00, 0x85, 0x01, 0x16, 0xA2, 0xFE, 0x26, 0x5E, 0x01, 0x36,
        0x88, 0xFA, 0x46, 0x78, 0x05, 0x55, 0x0C, 0x65, 0x11, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x75, 0x10,
        0x95, 0x03, 0x81, 0x06, 0xC0, 0xA1, 0x00, 0x85, 0x02, 0x09, 0x33, 0x09, 0x34, 0x09, 0x35, 0x75, 0x10,
        0x95, 0x03, 0x81, 0x06, 0xC0, 0xA1, 0x02, 0x85, 0x03, 0x05, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x02,
        0x15, 0x00, 0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x95, 0x0E, 0x81,
        0x03, 0xC0, 0xA1, 0x02, 0x85, 0x04, 0x05, 0x08, 0x09, 0x4B, 0x15, 0x00, 0x25, 0x01, 0x95, 0x01, 0x75,
        0x01, 0x91, 0x02, 0x95, 0x01, 0x75, 0x07, 0x91, 0x03, 0xC0, 0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x02,
        0x15, 0x80, 0x25, 0x7F, 0x75, 0x08, 0x09, 0x3A, 0xA1, 0x02, 0x85, 0x05, 0x09, 0x20, 0x95, 0x01, 0xB1,
        0x02, 0xC0, 0xA1, 0x02, 0x85, 0x06, 0x09, 0x21, 0x95, 0x01, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x07,
        0x09, 0x22, 0x95, 0x01, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x08, 0x09, 0x23, 0x95, 0x07, 0xB1, 0x02,
        0xC0, 0xA1, 0x02, 0x85, 0x09, 0x09, 0x24, 0x95, 0x07, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x0A, 0x09,
        0x25, 0x95, 0x07, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x0B, 0x09, 0x26, 0x95, 0x01, 0xB1, 0x02, 0xC0,
        0xA1, 0x02, 0x85, 0x13, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x19, 0x09, 0x31,
        0x95, 0x04, 0xB1, 0x02, 0xC0, 0xA1, 0x02, 0x85, 0x1A, 0x09, 0x32, 0x95, 0x07, 0xB1, 0x02, 0xC0, 0xC0,
        0xC0,
    ];

    fn le(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn report(id: u8, values: &[i16]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(le(values));
        out
    }

    #[test]
    fn device_frame_maps_to_the_spec_frame() {
        // Device: push right (+x), push away (−y, away from the user), lift (−z, up).
        let (t, r) = device_to_spec([100, -200, -300], [10, 20, 30]);
        assert_eq!(t, [100, 300, -200], "x right, lift = +y, push away = −z");
        assert_eq!(r, [-10, -30, 20]);
    }

    #[test]
    fn split_layout_recorded_bytes() {
        // Recorded shape: report 1 (x, y, z), report 2 (rx, ry, rz), report 3 (buttons).
        let reports = [
            report(1, &[350, 0, -175]),
            report(2, &[-350, 70, 0]),
            vec![3, 0b10],
        ];
        // Split products decode the same bytes with and without their descriptor; without it,
        // the product id selects the two-button layout (a legacy Logitech id here).
        for (descriptor, product) in [(Some(SPLIT_DESCRIPTOR), 0), (None, 0xC626)] {
            let mut decoder = DeviceDecoder::new(descriptor, product);
            for r in &reports {
                assert!(decoder.feed(r), "{descriptor:?}: {r:?} changed the state");
            }
            let s = decoder.state();
            assert_eq!(s.translation, [1.0, 0.5, 0.0], "{descriptor:?}");
            assert_eq!(s.rotation, [1.0, 0.0, 0.2], "{descriptor:?}");
            assert_eq!(s.buttons, 0b10, "{descriptor:?}: button 2 (right)");
            assert!(!decoder.feed(&reports[0]), "a repeat changes nothing");
            assert!(decoder.feed(&[3, 0]), "buttons released");
            assert_eq!(decoder.state().buttons, 0);
        }
    }

    #[test]
    fn combined_layout_recorded_bytes() {
        let combined = report(1, &[-35, 350, 0, 0, -350, 175]);
        for (descriptor, product) in [(Some(COMBINED_DESCRIPTOR), 0), (None, 0xC62E)] {
            let mut decoder = DeviceDecoder::new(descriptor, product);
            assert!(decoder.feed(&combined));
            let s = decoder.state();
            assert_eq!(s.translation, [-0.1, 0.0, 1.0], "{descriptor:?}");
            assert_eq!(s.rotation, [0.0, -0.5, -1.0], "{descriptor:?}");
        }
        // Buttons: the descriptor knows all 16. Without a descriptor (Windows reports none),
        // the product id selects the button count (fixrev review, finding 4): a multi-button
        // product decodes every bit its report carries...
        let mut decoder = DeviceDecoder::new(Some(COMBINED_DESCRIPTOR), 0);
        assert!(decoder.feed(&[3, 0x01, 0x80]));
        assert_eq!(decoder.state().buttons, 0x8001, "buttons 1 and 16");
        // integ-v048c: was 0xC62E, the two-button SpaceMouse Wireless (public USB id lists;
        // corr-input-world's table); the Enterprise (0xC633) is a multi-button product.
        let mut known_multi = DeviceDecoder::new(None, 0xC633);
        assert!(known_multi.feed(&[3, 0x01, 0x80]));
        assert_eq!(
            known_multi.state().buttons,
            0x8001,
            "a descriptor-less multi-button product keeps buttons 3+ (Windows)"
        );
        // ...while the two-button split products keep their padding-bit rejection (audit
        // finding 5, ADR-4768): bits beyond the two real buttons must not read as pressed.
        for product in [0, COMPACT_PRODUCT, 0xC626] {
            let mut known_two = DeviceDecoder::new(None, product);
            assert!(known_two.feed(&[3, 0x01, 0x80]));
            assert_eq!(
                known_two.state().buttons,
                0x0001,
                "product {product:#06x}: bits beyond the two real buttons are padding"
            );
        }
    }

    /// Without a descriptor (always the case on Windows), a many-button product keeps its
    /// buttons from 3 on, and a two-button product still ignores the padding bits
    /// (corr-input-world: at integ-v047 every descriptor-less device decoded only two bits).
    #[test]
    fn a_descriptor_less_many_button_product_keeps_its_buttons() {
        // SpaceMouse Pro (wired, Logitech vendor id): button 3 and button 15.
        let mut pro = DeviceDecoder::for_device(None, VENDOR_LOGITECH, 0xC62B);
        assert!(pro.feed(&[3, 0x04, 0x40]));
        assert_eq!(pro.state().buttons, 0x4004, "buttons 3 and 15");
        // SpaceMouse Enterprise: button 31.
        let mut enterprise = DeviceDecoder::for_device(None, VENDOR_3DCONNEXION, 0xC633);
        assert!(enterprise.feed(&[3, 0x00, 0x00, 0x00, 0x40]));
        assert_eq!(enterprise.state().buttons, 1 << 30, "button 31");
        // SpaceMouse Compact: two buttons, the rest of the byte is padding.
        let mut compact = DeviceDecoder::for_device(None, VENDOR_3DCONNEXION, 0xC635);
        assert!(compact.feed(&[3, 0x07]));
        assert_eq!(compact.state().buttons, 0x0003, "only buttons 1 and 2");
        assert_eq!(known_button_count(VENDOR_LOGITECH, 0xC626), 2, "SpaceNavigator");
        assert_eq!(known_button_count(VENDOR_LOGITECH, 0xC629), 32, "SpacePilot Pro");
        // A descriptor still wins over the product table.
        let mut described = DeviceDecoder::for_device(Some(COMBINED_DESCRIPTOR), VENDOR_3DCONNEXION, 0xC635);
        assert!(described.feed(&[3, 0x01, 0x80]));
        assert_eq!(described.state().buttons, 0x8001);
    }

    #[test]
    fn recorded_compact_descriptor_parses_to_the_split_layout() {
        let d = COMPACT_DESCRIPTOR_RECORDED;
        assert_eq!(d.len(), 239);
        assert!(declares_multi_axis(d));
        assert!(is_spacemouse(
            VENDOR_3DCONNEXION,
            0xC635,
            Some(declares_multi_axis(d))
        ));
        let layout = ReportLayout::from_descriptor(d).expect("parses");
        let mut decoder = DeviceDecoder::new(Some(d), COMPACT_PRODUCT);
        assert!(decoder.feed(&report(1, &[-350, 175, 0])));
        assert!(decoder.feed(&report(2, &[0, 0, 350])));
        assert!(decoder.feed(&[3, 0b01, 0]));
        let s = decoder.state();
        assert_eq!(s.translation, [-1.0, 0.0, 0.5]);
        assert_eq!(s.rotation, [0.0, -1.0, 0.0]);
        assert_eq!(s.buttons, 1, "left button");
        // Output (LED) and vendor feature reports are not inputs.
        assert!(layout.decode(&[4, 1]).is_empty());
        assert!(layout.decode(&[5, 0x7f]).is_empty());
    }

    #[test]
    fn short_and_unknown_reports_are_ignored() {
        let layout = ReportLayout::from_descriptor(SPLIT_DESCRIPTOR).expect("parses");
        assert!(layout.decode(&[]).is_empty());
        assert!(layout.decode(&[1, 0x10]).is_empty(), "truncated");
        assert!(layout.decode(&[9, 1, 2, 3, 4, 5, 6]).is_empty(), "unknown id");
        let known = ReportLayout::known(0);
        let short = known.decode(&report(1, &[1, 2, 3]));
        assert_eq!(short.translation, Some([1, 2, 3]));
        assert_eq!(short.rotation, None, "6-byte report 1 is translation only");
    }

    #[test]
    fn descriptor_identifies_multi_axis_controllers() {
        assert!(declares_multi_axis(SPLIT_DESCRIPTOR));
        assert!(declares_multi_axis(COMBINED_DESCRIPTOR));
        // A mouse: Generic Desktop / Mouse (0x02).
        assert!(!declares_multi_axis(&[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xC0]));
        assert!(!declares_multi_axis(&[0x05]), "truncated");
        assert!(ReportLayout::from_descriptor(&[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0xC0]).is_none());
        assert!(ReportLayout::from_descriptor(&[0x85]).is_none(), "truncated item");
        assert!(is_spacemouse(VENDOR_3DCONNEXION, 0xC635, None));
        assert!(is_spacemouse(VENDOR_3DCONNEXION, 0x1234, Some(true)));
        assert!(
            !is_spacemouse(VENDOR_3DCONNEXION, 0xC635, Some(false)),
            "a CadMouse or keyboard"
        );
        assert!(is_spacemouse(VENDOR_LOGITECH, 0xC626, None));
        assert!(
            !is_spacemouse(VENDOR_LOGITECH, 0xC52B, None),
            "a Logitech receiver"
        );
    }
}
