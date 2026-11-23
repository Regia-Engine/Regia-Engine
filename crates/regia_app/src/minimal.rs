// SPDX-License-Identifier: MPL-2.0
//! `MinimalPlugins`: a headless app (tests, servers, tools).

use crate::app::run_headless;
use crate::diagnostics::DiagnosticsPlugin;
use crate::{App, Plugin};

/// Headless setup: diagnostics and the [`run_headless`] runner. Time, the type registry and the
/// frame schedules are part of every [`App`]; insert a
/// [`crate::TimeUpdateStrategy::ManualDuration`] for deterministic frame stepping and a
/// [`crate::FramePacing`] to cap the rate.
#[derive(Default)]
pub struct MinimalPlugins;

impl Plugin for MinimalPlugins {
    fn build(&self, app: &mut App) {
        app.add_plugins(DiagnosticsPlugin::default())
            .set_runner(run_headless);
    }
}
