// SPDX-License-Identifier: MPL-2.0
//! What the gameplay debug panels share (`book/agents/editor-gameplay-panels.md`, ADR-2030): which
//! world they look at, and the record of what a queued call did.
//!
//! The agent server runs on the edit world, while a running game lives in the play world. So the
//! panels ([`crate::state_tree_panel`], [`crate::smart_object_panel`], [`crate::camera_panel`]) keep a
//! snapshot in a resource on the edit world: each frame their `pump` copies it from the play world
//! and runs what the buttons and the agent queued there. With no play session they read and act on
//! the edit world directly. A panel and an agent therefore read the same model.

use regia_agent::protocol::RpcError;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// Which world a panel's snapshot came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LiveSource {
    /// The edit world.
    #[default]
    Edit,
    /// The play-in-editor world.
    Play,
}

impl LiveSource {
    /// `edit` or `play`, as the agent reports it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Play => "play",
        }
    }

    /// The panel's header text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Edit => "Edit world",
            Self::Play => "Play world (live)",
        }
    }
}

/// Whether a play session is running, as the editor loop last saw it.
#[must_use]
pub fn play_running(edit: &World) -> bool {
    edit.get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some())
}

/// `invalid_params` with a stable `data.reason`.
#[must_use]
pub fn reason_error(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::invalid_params(message.into());
    if let Some(data) = error.data.as_mut() {
        data["reason"] = json!(reason);
    }
    error
}

/// The `data.reason` of an error, when it carries one.
#[must_use]
pub fn reason_of(error: &RpcError) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data["reason"].as_str())
        .map(str::to_owned)
}

/// A call waiting for the frame's pump.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelCall {
    /// The method, one of `regia_agent::panel_calls::PANEL_METHODS` or a panel's own.
    pub method: String,
    /// Its parameters.
    pub params: Value,
}

impl PanelCall {
    /// A call of `method` with `params`.
    #[must_use]
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            method: method.to_owned(),
            params,
        }
    }
}

/// What the last call of a panel did, for its status line and the agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallRecord {
    /// The method.
    pub method: String,
    /// The world it ran on.
    pub source: LiveSource,
    /// The handler's answer, on success.
    pub answer: Option<Value>,
    /// The stable reason code, on failure.
    pub reason: Option<String>,
    /// What went wrong, on failure.
    pub message: Option<String>,
}

impl CallRecord {
    /// The record of `method`'s `result` on `source`.
    #[must_use]
    pub fn of(method: &str, source: LiveSource, result: Result<Value, RpcError>) -> Self {
        match result {
            Ok(answer) => Self {
                method: method.to_owned(),
                source,
                answer: Some(answer),
                ..Self::default()
            },
            Err(error) => Self {
                method: method.to_owned(),
                source,
                reason: Some(reason_of(&error).unwrap_or_else(|| "call_failed".to_owned())),
                message: Some(error.message),
                answer: None,
            },
        }
    }

    /// The record as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "method": self.method,
            "source": self.source.name(),
            "ok": self.reason.is_none(),
            "answer": self.answer,
            "reason": self.reason,
            "message": self.message,
        })
    }

    /// Draws the line under a panel's controls when the call failed.
    pub fn draw(&self, ui: &mut egui::Ui) {
        if let Some(reason) = &self.reason {
            // The sentence for people; the method and reason code on hover (bug 20, ADR-8586).
            ui.colored_label(
                egui::Color32::from_rgb(230, 90, 90),
                crate::data_panels::sentence(self.message.as_deref().unwrap_or("The call failed")),
            )
            .on_hover_text(format!("Agent: {}\nAgent error code: {reason}", self.method));
        }
    }
}

/// The header every gameplay panel starts with: its title, the world it shows, a count.
pub fn header(ui: &mut egui::Ui, title: &str, source: LiveSource, detail: &str) {
    ui.horizontal(|ui| {
        ui.strong(title);
        let color = match source {
            LiveSource::Edit => egui::Color32::GRAY,
            LiveSource::Play => egui::Color32::from_rgb(120, 220, 120),
        };
        ui.colored_label(color, source.label());
        ui.weak(detail);
    });
    ui.separator();
}

/// An entity from a request's `key`, when present.
///
/// # Errors
/// `invalid_params` when the value is not an entity id.
pub fn entity_param(params: &Value, key: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an entity id"))),
    }
}
