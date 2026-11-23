// SPDX-License-Identifier: MPL-2.0
//! The editor's SpaceMouse seam. `regia_input::spacemouse` exists on desktop targets only
//! (`cfg(all(feature = "devices", not(target_os = "android")))`), and the Android workspace check
//! compiles the editor, so editor code names these items through this module: on desktop it
//! re-exports the real backend, and Android builds get small no-op stand-ins (ADR-10356). The
//! editor stays desktop-only at run time; the stand-ins only let the crate type-check.
//!
//! A new item the editor uses must be added to both halves: the Android workspace check
//! (`cargo ndk -t arm64-v8a check --workspace`) fails on a name the Android half lacks.

#[cfg(not(target_os = "android"))]
pub use regia_input::spacemouse::{
    GesturePhase, NavContext, NavDelta, ObjectGesture, SpaceMouseAction, SpaceMouseDevices, SpaceMouseNav,
    SpaceMousePlugin, SpaceMouseState, SpaceMouseWaker, actions, nav,
};

#[cfg(target_os = "android")]
mod stub {
    use std::sync::Arc;

    // The glob matters: bevy derive expansions name `bevy_ecs`, which `regia_ecs::prelude`
    // re-exports (the desktop plugin file does the same).
    use regia_app::prelude::*;
    use regia_core::math::Vec2;

    /// Android stub of the real action message.
    #[derive(Debug, Clone, Default, Message)]
    pub struct SpaceMouseAction {
        /// The action name.
        pub action: String,
    }

    /// Android stub of the device registry.
    #[derive(Resource, Debug, Clone, Default)]
    pub struct SpaceMouseDevices;

    impl SpaceMouseDevices {
        /// How many devices are connected.
        #[must_use]
        pub fn connected(&self) -> usize {
            0
        }
    }

    /// The gesture phase of one object-tool pass.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub enum GesturePhase {
        /// No gesture is active.
        #[default]
        Idle,
        /// A gesture began this frame.
        Began,
        /// A gesture is moving.
        Moving,
        /// A gesture ended this frame.
        Ended,
    }

    /// Android stub of one navigation context.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct NavContext;

    impl NavContext {
        /// Orbit context at `distance`.
        #[must_use]
        pub fn orbit(_distance: f32) -> Self {
            Self
        }

        /// Android stub; keeps `self`.
        #[must_use]
        pub fn with_pitch(self, _pitch: f32) -> Self {
            self
        }

        /// Android stub; keeps `self`.
        #[must_use]
        pub fn with_config<T>(self, _config: &T) -> Self {
            self
        }

        /// Android stub; keeps `self`.
        #[must_use]
        pub fn with_toggles(self, _toggles: &SpaceMouseNav) -> Self {
            self
        }
    }

    /// Android stub of one navigation delta.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct NavDelta {
        /// View-plane translation.
        pub pan: Vec2,
        /// Orbit yaw and pitch.
        pub orbit: Vec2,
        /// Zoom multiplier.
        pub zoom: f32,
        /// Roll delta.
        pub roll: f32,
    }

    impl NavDelta {
        /// The no-op delta.
        pub const IDENTITY: Self = Self {
            pan: Vec2::ZERO,
            orbit: Vec2::ZERO,
            zoom: 1.0,
            roll: 0.0,
        };
    }

    impl Default for NavDelta {
        fn default() -> Self {
            Self::IDENTITY
        }
    }

    /// Android stub of the object-tool gesture tracker.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct ObjectGesture;

    impl ObjectGesture {
        /// Whether a gesture is open.
        #[must_use]
        pub fn is_active(&self) -> bool {
            false
        }

        /// Updates the gesture state from one sample.
        #[must_use]
        pub fn update(&mut self, _state: &SpaceMouseState, _dt: f32) -> GesturePhase {
            GesturePhase::Idle
        }
    }

    /// Android stub of the editor toggles.
    #[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct SpaceMouseNav {
        /// Whether the Object tool is on.
        pub object_tool: bool,
    }

    /// Android stub of the live device state.
    #[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
    pub struct SpaceMouseState;

    impl SpaceMouseState {
        /// Android stub; every axis is at rest.
        #[must_use]
        pub fn axes(&self) -> [f32; 6] {
            [0.0; 6]
        }
    }

    /// Android stub of the event-loop waker.
    #[derive(Resource, Clone)]
    pub struct SpaceMouseWaker(pub Arc<dyn Fn() + Send + Sync>);

    impl SpaceMouseWaker {
        /// Builds a waker from `wake`.
        #[must_use]
        pub fn new<F>(wake: F) -> Self
        where
            F: Fn() + Send + Sync + 'static,
        {
            Self(Arc::new(wake))
        }
    }

    /// Desktop-only action names that the editor still references in Android builds.
    pub mod actions {
        /// Toggle the Object tool.
        pub const OBJECT_TOOL_TOGGLE: &str = "spacemouse.object_tool_toggle";
        /// Frame the selection.
        pub const FIT_SELECTION: &str = "view.fit_selection";
        /// Reset to the home camera.
        pub const HOME: &str = "view.home";
    }

    /// Android stub of the navigation helper module.
    pub mod nav {
        use super::{NavContext, NavDelta, SpaceMouseState};

        /// Android stub; always answers the identity delta.
        #[must_use]
        pub fn step(_state: &SpaceMouseState, _dt: f32, _ctx: NavContext) -> NavDelta {
            NavDelta::IDENTITY
        }
    }

    /// Android stub of the desktop plugin.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SpaceMousePlugin;

    impl Plugin for SpaceMousePlugin {
        fn build(&self, _app: &mut App) {}
    }
}

#[cfg(target_os = "android")]
pub use stub::{
    GesturePhase, NavContext, NavDelta, ObjectGesture, SpaceMouseAction, SpaceMouseDevices, SpaceMouseNav,
    SpaceMousePlugin, SpaceMouseState, SpaceMouseWaker, actions, nav,
};
