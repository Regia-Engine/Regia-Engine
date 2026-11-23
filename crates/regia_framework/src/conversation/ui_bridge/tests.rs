// SPDX-License-Identifier: MPL-2.0
use regia_ui::captions::{CaptionBoard, CaptionPlugin};
use regia_ui::choice::{ChoiceBoard, ChoiceInput, ChoiceLayout, ChoicePlugin};

use super::*;
use crate::conversation::asset::{
    ChoiceOption as ConvOption, ConversationAsset, ConversationLine, DurationSource, Speaker, Step, VoiceSpec,
};
use crate::conversation::runner::PlayRequest;
use crate::conversation::{ConversationPlugin, ConversationRunner};

fn line(id: &str) -> ConversationLine {
    ConversationLine {
        id: id.into(),
        speaker: "guard".into(),
        text: format!("k-{id}"),
        note: String::new(),
        emotion: None,
        tags: Vec::new(),
        voice: VoiceSpec::None,
        duration: DurationSource::Fixed(2000),
        cues: Vec::new(),
        priority: None,
        keep_breaks: false,
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((
        CaptionPlugin,
        ChoicePlugin,
        ConversationPlugin,
        ConversationChoiceBridgePlugin,
    ));
    let mut library = ConversationLibrary::default();
    library
        .speakers
        .speakers
        .insert("guard".into(), Speaker::default());
    library.conversations.insert(
        "c".into(),
        ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: vec![line("la"), line("lb")],
            steps: vec![
                Step::Choice {
                    prompt: None,
                    options: vec![
                        ConvOption {
                            text: "opt-a".into(),
                            cond: None,
                            goto: "a".into(),
                        },
                        ConvOption {
                            text: "opt-b".into(),
                            cond: None,
                            goto: "b".into(),
                        },
                    ],
                },
                Step::Label("a".into()),
                Step::Line("la".into()),
                Step::End,
                Step::Label("b".into()),
                Step::Line("lb".into()),
                Step::End,
            ],
        },
    );
    *app.world_mut().resource_mut::<ConversationLibrary>() = library;
    app
}

fn play(app: &mut App) {
    let library = app.world().resource::<ConversationLibrary>().clone();
    let mut voice = crate::conversation::NullVoicePlayer;
    let env = WorldEnv {
        library: &library,
        loc: None,
        voice: &mut voice,
    };
    app.world_mut()
        .resource_mut::<ConversationRunner>()
        .play(PlayRequest::new("c"), &env)
        .expect("play");
}

fn caption_sources(app: &App) -> Vec<String> {
    app.world()
        .resource::<CaptionBoard>()
        .snapshot()
        .entries
        .iter()
        .filter_map(|e| e["source"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn a_choice_step_opens_a_widget_and_a_pick_resumes_the_run() {
    let mut app = app();
    play(&mut app);
    app.update();
    let ui = app.world().resource::<ConversationChoiceUi>();
    let waiting = ui.waiting();
    assert_eq!(waiting.len(), 1);
    let widget = waiting[0].0;
    let board = app.world().resource::<ChoiceBoard>();
    let w = board.get(widget).expect("widget open");
    assert_eq!(w.options().len(), 2);
    // Fallback text is the key itself (no Localization in this world).
    assert_eq!(w.options()[1].label, "opt-b");
    assert_eq!(w.kind(), ChoiceLayout::List);

    app.world_mut()
        .resource_mut::<ChoiceBoard>()
        .queue_input(widget, ChoiceInput::Digit(2));
    app.update();
    app.update();
    assert!(
        app.world()
            .resource::<ConversationChoiceUi>()
            .waiting()
            .is_empty()
    );
    assert!(app.world().resource::<ChoiceBoard>().open_ids().is_empty());
    let sources = caption_sources(&app);
    assert!(sources.iter().any(|s| s.ends_with("c#lb")), "{sources:?}");
    assert!(!sources.iter().any(|s| s.ends_with("c#la")), "{sources:?}");
}

#[test]
fn a_cancel_reopens_the_same_choice() {
    let mut app = app();
    play(&mut app);
    app.update();
    let widget = app.world().resource::<ConversationChoiceUi>().waiting()[0].0;
    app.world_mut()
        .resource_mut::<ChoiceBoard>()
        .queue_input(widget, ChoiceInput::Cancel);
    app.update();
    let waiting = app.world().resource::<ConversationChoiceUi>().waiting();
    assert_eq!(waiting.len(), 1);
    assert_ne!(waiting[0].0, widget, "a new widget for the same run");
    assert_eq!(app.world().resource::<ChoiceBoard>().open_ids().len(), 1);
}

#[test]
fn the_timer_picks_the_default_for_the_run() {
    let mut app = app();
    {
        let mut ui = app.world_mut().resource_mut::<ConversationChoiceUi>();
        ui.timer_steps = Some(2);
        ui.default_index = 1;
    }
    play(&mut app);
    app.update();
    for _ in 0..4 {
        app.world_mut().run_schedule(FixedUpdate);
        app.update();
    }
    assert!(
        app.world()
            .resource::<ConversationChoiceUi>()
            .waiting()
            .is_empty()
    );
    let sources = caption_sources(&app);
    assert!(sources.iter().any(|s| s.ends_with("c#lb")), "{sources:?}");
}
