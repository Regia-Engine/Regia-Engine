// SPDX-License-Identifier: MPL-2.0
//! Runtime UI to scripts (lane post05-ui-input, ADR-7070): spec 18 § 4 UI events reach scripts on the
//! target and, bubbled, on its ancestors; spec 18 § 5 a `ScriptField` binding shows a script's field;
//! spec 11 § 2/§ 3 the `[runtime]` table of `Regia.toml` sets `PauseOnFocusLoss` and `KeepScreenOn`.
//! Headless: no window, no GPU, fixed steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::prelude::*;
use regia_ecs::reflect::PartialReflect;
use regia_framework::project_runtime::{ProjectRuntimeConfig, ProjectRuntimePlugin};
use regia_framework::script::{
    Script, ScriptCtx, ScriptEvent, ScriptPlugin, ScriptSlot, ScriptUiEvent, Scripts,
};
use regia_platform::{ButtonState, KeepScreenOn, MouseButton, PauseOnFocusLoss};
use regia_ui::binding::Bind;
use regia_ui::prelude::*;
use regia_ui::text::{Text, TextContent};

type Log = Arc<Mutex<Vec<(Entity, Entity, ScriptUiEvent)>>>;

/// Records every UI event its entity receives: (handler, origin, event).
struct Recorder(Log);

impl Script for Recorder {
    fn on_event(&mut self, _ctx: &mut ScriptCtx, event: &ScriptEvent) {
        if let ScriptEvent::Ui {
            entity,
            origin,
            event,
        } = *event
        {
            self.0.lock().unwrap().push((entity, origin, event));
        }
    }
}

fn slot(script: impl Script) -> Scripts {
    let mut scripts = Scripts::default();
    scripts.0.push(ScriptSlot {
        type_path: "test::Recorder".into(),
        order: 0,
        instance: Some(Box::new(script)),
        started: false,
        enabled: true,
    });
    scripts
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ScriptPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn computed(rect: [f32; 4]) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        ..Default::default()
    }
}

/// Spec 18 § 4: a click on a button reaches the scripts on the button (origin = itself) and, bubbled,
/// the scripts on its ancestor (origin = the button).
#[test]
fn a_click_reaches_the_button_script_and_bubbles_to_the_ancestor_script() {
    let mut app = app();
    let button_log = Log::default();
    let panel_log = Log::default();
    let panel = app.world_mut().spawn(slot(Recorder(panel_log.clone()))).id();
    let button = app
        .world_mut()
        .spawn((
            Interactable::default(),
            computed([0.0, 0.0, 100.0, 40.0]),
            ChildOf(panel),
            slot(Recorder(button_log.clone())),
        ))
        .id();
    app.update();
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (50.0, 20.0),
        timestamp: Default::default(),
    });
    app.update();
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Left,
            state,
            timestamp: Default::default(),
        });
        app.update();
    }
    // Delivery can trail the click by one frame (scripts may run before the UI pass).
    app.update();

    let clicks = |log: &Log| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(_, _, e)| matches!(e, ScriptUiEvent::Click { .. }))
            .map(|(handler, origin, _)| (*handler, *origin))
            .collect::<Vec<_>>()
    };
    assert_eq!(clicks(&button_log), vec![(button, button)], "the button's script");
    assert_eq!(clicks(&panel_log), vec![(panel, button)], "the ancestor's script");
    assert!(
        button_log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, e)| *e == ScriptUiEvent::PointerEnter),
        "hover reaches the target's script"
    );
    assert!(
        !panel_log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, e)| *e == ScriptUiEvent::PointerEnter),
        "hover does not bubble"
    );
}

/// A HUD script whose fields a label binds to.
#[derive(Reflect, Default)]
struct Hud {
    score: u32,
}

impl Script for Hud {
    fn reflect_fields(&self) -> Option<&dyn PartialReflect> {
        Some(self)
    }
}

fn label_text(app: &App, label: Entity) -> TextContent {
    app.world().get::<Text>(label).unwrap().spans[0].content.clone()
}

/// Spec 18 § 5: `BindSource::ScriptField("Hud.score")` on a label under the HUD's entity shows the script
/// field and follows it when it changes.
#[test]
fn a_script_field_binding_updates_the_label_when_the_field_changes() {
    let mut app = app();
    let hud = app
        .world_mut()
        .spawn(Scripts(
            std::iter::once(ScriptSlot {
                type_path: std::any::type_name::<Hud>().into(),
                order: 0,
                instance: Some(Box::new(Hud { score: 7 })),
                started: false,
                enabled: true,
            })
            .collect(),
        ))
        .id();
    let label = app
        .world_mut()
        .spawn((
            Text::default(),
            Bind::script_field("Hud.score", Some("Score: {:03}")),
            ChildOf(hud),
        ))
        .id();
    app.update();
    assert_eq!(label_text(&app, label), TextContent::Lit("Score: 007".into()));

    {
        let mut scripts = app.world_mut().get_mut::<Scripts>(hud).unwrap();
        let instance = scripts.0[0].instance.as_mut().unwrap();
        let hud: &mut dyn std::any::Any = instance.as_mut();
        hud.downcast_mut::<Hud>().unwrap().score = 42;
    }
    app.update();
    assert_eq!(label_text(&app, label), TextContent::Lit("Score: 042".into()));

    let missing = app
        .world_mut()
        .spawn((Text::default(), Bind::script_field("Nope.score", None)))
        .id();
    app.update();
    let error = app
        .world()
        .get::<Bind>(missing)
        .unwrap()
        .last_error()
        .map(str::to_owned);
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.starts_with("binding_unresolved")),
        "an unknown script reports binding_unresolved: {error:?}"
    );
}

/// Spec 11 § 2/§ 3: `[runtime] keep_screen_on = true` sets `KeepScreenOn`, `pause_on_focus_loss = true`
/// sets `PauseOnFocusLoss`; an absent table or keys leave both off (no version bump: defaults).
#[test]
fn regia_toml_runtime_keys_set_keep_screen_on_and_pause_on_focus_loss() {
    let on = ProjectRuntimeConfig::from_manifest(
        "[project]\nname = \"x\"\n\n[runtime]\nkeep_screen_on = true\npause_on_focus_loss = true\n",
    )
    .expect("parses");
    assert!(on.keep_screen_on && on.pause_on_focus_loss);
    let off = ProjectRuntimeConfig::from_manifest("[project]\nname = \"x\"\n").expect("parses");
    assert_eq!(off, ProjectRuntimeConfig::default());
    assert!(ProjectRuntimeConfig::from_manifest("[runtime]\nkeep_screen_on = \"yes\"\n").is_err());

    let root = regia_test_utils::unique_temp_dir("post05_runtime_toml");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"x\"\n\n[runtime]\nkeep_screen_on = true\npause_on_focus_loss = true\n",
    )
    .unwrap();
    let mut app = App::new();
    app.add_plugins(ProjectRuntimePlugin {
        project_root: Some(root.clone()),
        config: None,
    });
    assert_eq!(
        app.world().get_resource::<KeepScreenOn>(),
        Some(&KeepScreenOn(true))
    );
    assert_eq!(
        app.world().get_resource::<PauseOnFocusLoss>(),
        Some(&PauseOnFocusLoss(true))
    );

    let empty = regia_test_utils::unique_temp_dir("post05_runtime_toml_off");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join("Regia.toml"), "[project]\nname = \"x\"\n").unwrap();
    let mut app = App::new();
    app.add_plugins(ProjectRuntimePlugin {
        project_root: Some(empty.clone()),
        config: None,
    });
    assert_eq!(
        app.world().get_resource::<KeepScreenOn>(),
        Some(&KeepScreenOn(false))
    );
    assert_eq!(
        app.world().get_resource::<PauseOnFocusLoss>(),
        Some(&PauseOnFocusLoss(false))
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}
