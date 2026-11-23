// SPDX-License-Identifier: MPL-2.0
//! Headless rendering configuration (`specs/13` § 12).
//!
//! [`HeadlessRender`] copies [`crate::RenderPlugin::headless`] when the plugin builds.
//! Window systems are registered from that flag. Later writes to the resource do not
//! add or remove them.

use regia_app::prelude::*;

/// Copy of [`crate::RenderPlugin::headless`] taken when that plugin builds.
///
/// Window presentation follows the flag the plugin was built with. A later write does not
/// register or remove window systems: `true` on a windowed plugin still presents, and
/// `false` on a headless plugin leaves those systems out. Golden tests build the plugin
/// through [`crate::RenderDevice::headless`]. The frame-generation probe reads this
/// resource for its `headless` blocker, which can disagree with presentation after a write.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeadlessRender(pub bool);
