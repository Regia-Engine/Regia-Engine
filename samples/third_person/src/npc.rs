// SPDX-License-Identifier: MIT-0
//! The guide NPC: a short conversation (`assets/conversations/guide.conv.ron`: a greeting, a
//! question, one choice with two answers, a reply) started from the talk key, with subtitles
//! through the caption board and two locales (`assets/localization/{en,de}`).
//!
//! The framework's `ConversationPlugin` runs the steps; this module only starts the run when the
//! player is near the NPC (`KeyT`), answers a pending choice (`Digit1` / `Digit2`), switches the
//! locale (`KeyL`) and mirrors what happened into the reflected [`TalkState`] so an agent can read
//! it with `play.expect` (`docs/specs/56-dialogue-and-subtitles.md`, `book/agents/third-person-demo.md`).

use regia::prelude::*;
use regia_app::caption::{CaptionSource, CaptionStart};
use regia_core::math::Vec3;
use regia_framework::conversation::{
    ConversationChoiceRequested, ConversationError, ConversationEvent, ConversationFinished,
    ConversationLibrary, ConversationRunner, PlayRequest, WorldEnv, with_localization, with_voice_player,
};
use regia_scene::Transform;
use regia_ui::captions::{CaptionClock, CueEventMap};
use regia_ui::localization::Localization;

/// The conversation id of the guide.
pub const GUIDE_CONVERSATION: &str = "guide";

/// The message keys the guide's lines resolve (`assets/localization/{en,de}/dialogue.ftl`).
const DIALOGUE_KEYS: [&str; 4] = [
    "npc-guide-greet",
    "npc-guide-ask",
    "npc-guide-yes",
    "npc-guide-no",
];

/// How near (metres) the player must stand to talk.
pub const TALK_RADIUS: f32 = 4.0;

/// Where the guide stands (the ground top is y = 0.25).
pub const GUIDE_POSITION: Vec3 = Vec3::new(-6.0, 1.05, -2.0);

/// The English and German text of the conversation (also on disk under `assets/localization`).
const EN: &str = include_str!("../assets/localization/en/dialogue.ftl");
const DE: &str = include_str!("../assets/localization/de/dialogue.ftl");

/// A character the player can talk to.
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct Npc {
    /// The NPC's id.
    pub id: String,
    /// The conversation it starts.
    pub conversation: String,
    /// How near the player must be (metres).
    pub radius: f32,
}

/// What the conversation with an NPC has done so far (read by agents with `play.expect`).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct TalkState {
    /// The running run id (0 when none).
    pub run: u64,
    /// Whether a conversation is running.
    pub talking: bool,
    /// Whether the run waits for an answer.
    pub choice_pending: bool,
    /// The option texts offered (resolved in the run's locale).
    pub options: Vec<String>,
    /// Every line id shown since the start, as `<conversation>#<line>`, in order.
    pub lines_seen: Vec<String>,
    /// The answer last chosen (index; -1 when none yet).
    pub answer: i32,
    /// The last `Event` step name received.
    pub last_event: String,
    /// How many runs ended.
    pub finished: u32,
    /// The locale the last run started in.
    pub locale: String,
}

/// `Startup` (after the level): the localization, the conversation library and the guide.
pub fn setup(world: &mut World) {
    ensure_localization(world);
    ensure_guide_conversation(world);
    let half = Vec3::new(0.3, 0.8, 0.3);
    let guide = crate::level::spawn_box(world, GUIDE_POSITION, half, [0.91, 0.77, 0.28, 1.0], false);
    world.entity_mut(guide).insert((
        Name::new("npc-guide"),
        regia_scene::components::Name::new("npc-guide"),
        Npc {
            id: "guide".to_owned(),
            conversation: GUIDE_CONVERSATION.to_owned(),
            radius: TALK_RADIUS,
        },
        TalkState {
            answer: -1,
            ..Default::default()
        },
    ));
}

/// The guide's texts load even when a host already created a `Localization` (the UI plugin makes
/// an empty one, so testing the resource says nothing about the dialogue keys): only a locale
/// missing a dialogue key gets the embedded source, appended to what the host loaded — a host's
/// own messages are never dropped.
fn ensure_localization(world: &mut World) {
    let Some(mut loc) = world.get_resource_mut::<Localization>() else {
        match Localization::new(
            vec![("en".to_owned(), EN.to_owned()), ("de".to_owned(), DE.to_owned())],
            Some("en".to_owned()),
        ) {
            Ok(loc) => {
                world.insert_resource(loc);
            }
            Err(err) => tracing::warn!(target: "third_person", "npc: no localization: {err}"),
        }
        return;
    };
    for (locale, source) in [("en", EN), ("de", DE)] {
        if DIALOGUE_KEYS.iter().all(|key| loc.has_in(locale, key)) {
            continue;
        }
        let merged = match loc.source_text(locale) {
            Some(existing) => format!("{existing}\n{source}"),
            None => source.to_owned(),
        };
        if let Err(err) = loc.add_locale(locale.to_owned(), merged) {
            tracing::warn!(target: "third_person", "npc: cannot add the {locale} texts: {err}");
        }
    }
}

/// Adds the guide's conversation assets to the host's library when it lacks them: through the
/// VFS for a packaged mount (the Android APK, where the host file system has no `assets/`), from
/// the host file system otherwise. Only entries the library lacks are inserted — a host's own
/// conversations, speakers, clips and cue events are never overwritten.
fn ensure_guide_conversation(world: &mut World) {
    let present = world
        .get_resource::<ConversationLibrary>()
        .is_some_and(|library| library.conversations.contains_key(GUIDE_CONVERSATION));
    if present {
        return;
    }
    let loaded = match load_sample_library(world) {
        Ok(loaded) => loaded,
        Err(err) => {
            tracing::warn!(target: "third_person", "npc: no conversations: {err}");
            return;
        }
    };
    let cue_events = loaded.cue_events.clone();
    if let Some(mut library) = world.get_resource_mut::<ConversationLibrary>() {
        for (id, conversation) in loaded.conversations {
            library.conversations.entry(id).or_insert(conversation);
        }
        for (id, speaker) in loaded.speakers.speakers {
            library.speakers.speakers.entry(id).or_insert(speaker);
        }
        for (path, duration) in loaded.clips {
            library.clips.entry(path).or_insert(duration);
        }
        for (name, def) in loaded.cue_events {
            library.cue_events.entry(name).or_insert(def);
        }
    }
    // The caption board reads the cue events from the resource, not the library (the plugin
    // keeps them in step when it loads; a merge must do the same).
    if let Some(mut map) = world.get_resource_mut::<CueEventMap>() {
        for (name, def) in cue_events {
            map.0.entry(name).or_insert(def);
        }
    }
}

/// Loads the sample's conversation assets the way the plugin loads a project's: through the VFS
/// when the assets live in a package (Android), from the host file system when they are a plain
/// directory, and from the sample's source assets when there is no VFS at all (tests).
///
/// # Errors
/// The conversation asset parse error, naming `path:line:col`.
fn load_sample_library(world: &World) -> Result<ConversationLibrary, ConversationError> {
    let vfs = world.get_resource::<regia_platform::Vfs>();
    let root = vfs.and_then(|vfs| {
        regia_platform::VfsPath::parse("assets://")
            .ok()
            .and_then(|path| vfs.host_path(&path))
    });
    match (vfs, root) {
        (Some(vfs), None) => ConversationLibrary::load_vfs(vfs),
        (_, Some(root)) => ConversationLibrary::load_assets(&root),
        (None, None) => ConversationLibrary::load_assets(&crate::assets::project_dir().join("assets")),
    }
}

/// Runs `f` with the runner and the environment the conversation systems use.
fn with_runner<R>(
    world: &mut World,
    f: impl FnOnce(&mut ConversationRunner, &WorldEnv<'_>) -> R,
) -> Option<R> {
    if !world.contains_resource::<ConversationRunner>() || !world.contains_resource::<ConversationLibrary>() {
        return None;
    }
    let now = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    world.resource_scope(|world, mut runner: Mut<ConversationRunner>| {
        world.resource_scope(|world, library: Mut<ConversationLibrary>| {
            Some(with_localization(world, |world, loc| {
                with_voice_player(world, now, |voice| {
                    let env = WorldEnv {
                        library: &library,
                        loc,
                        voice,
                    };
                    f(&mut runner, &env)
                })
            }))
        })
    })
}

/// The guide nearest to player 0 and within its radius.
fn npc_in_reach(world: &mut World) -> Option<Entity> {
    let player = crate::input::possessed(world, 0)?;
    let at = world.get::<Transform>(player)?.translation;
    let mut best: Option<(f32, Entity)> = None;
    let mut query = world.query::<(Entity, &Transform, &Npc)>();
    for (entity, transform, npc) in query.iter(world) {
        let distance = transform.translation.distance(at);
        if distance <= npc.radius && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, entity));
        }
    }
    best.map(|(_, entity)| entity)
}

/// The talk key: starts the conversation of the NPC in reach. Returns the run id, or `None` when
/// nobody is in reach, the channel is busy or the conversation is not loaded.
pub fn interact(world: &mut World) -> Option<u64> {
    let npc = npc_in_reach(world)?;
    let conversation = world.get::<Npc>(npc)?.conversation.clone();
    let started = with_runner(world, |runner, env| {
        runner.play(PlayRequest::new(conversation), env)
    })?;
    match started {
        Ok(started) => {
            let run = started.run?.0;
            if let Some(mut talk) = world.get_mut::<TalkState>(npc) {
                talk.run = run;
                talk.talking = true;
                talk.choice_pending = false;
                talk.options.clear();
                talk.lines_seen.clear();
                talk.answer = -1;
                talk.last_event.clear();
                talk.locale = started.locale;
            }
            Some(run)
        }
        Err(err) => {
            tracing::debug!(target: "third_person", "npc: cannot start the conversation: {err}");
            None
        }
    }
}

/// Answers the pending choice with option `index` (0-based). Returns whether a choice was pending
/// and accepted.
pub fn answer(world: &mut World, index: usize) -> bool {
    let mut query = world.query::<(Entity, &TalkState)>();
    let Some((entity, run)) = query
        .iter(world)
        .find(|(_, talk)| talk.choice_pending)
        .map(|(entity, talk)| (entity, talk.run))
    else {
        return false;
    };
    let ok = with_runner(world, |runner, env| {
        runner
            .choose(regia_framework::conversation::RunId(run), index, env)
            .is_ok()
    })
    .unwrap_or(false);
    if ok && let Some(mut talk) = world.get_mut::<TalkState>(entity) {
        talk.choice_pending = false;
        talk.options.clear();
        talk.answer = i32::try_from(index).unwrap_or(i32::MAX);
    }
    ok
}

/// Switches between English and German (`KeyL`). Returns the locale now selected.
pub fn toggle_locale(world: &mut World) -> Option<String> {
    let mut loc = world.get_resource_mut::<Localization>()?;
    let next = if loc.locale() == "de" { "en" } else { "de" };
    loc.set_locale(next);
    Some(loc.locale().to_owned())
}

/// `Update`: mirrors the conversation messages into the [`TalkState`] whose run they belong to.
/// Every message carries the run id; a second conversation in the host must not leak lines,
/// choices, events or finishes into the guide's state.
#[tracing::instrument(skip_all, target = "third_person", name = "npc_events")]
pub fn track(
    mut captions: MessageReader<CaptionStart>,
    mut choices: MessageReader<ConversationChoiceRequested>,
    mut events: MessageReader<ConversationEvent>,
    mut finished: MessageReader<ConversationFinished>,
    mut talks: Query<&mut TalkState>,
) {
    let lines: Vec<(u64, String)> = captions
        .read()
        .filter_map(|c| match &c.source {
            CaptionSource::Dialogue { run, line } => Some((*run, line.clone())),
            _ => None,
        })
        .collect();
    let choices: Vec<(u64, Vec<String>)> = choices
        .read()
        .map(|c| (c.run.0, c.options.iter().map(|o| o.text.clone()).collect()))
        .collect();
    let events: Vec<(u64, String)> = events.read().map(|e| (e.run.0, e.name.clone())).collect();
    let finished: Vec<u64> = finished.read().map(|f| f.run.0).collect();
    for mut talk in &mut talks {
        if !talk.talking {
            continue;
        }
        let run = talk.run;
        talk.lines_seen.extend(
            lines
                .iter()
                .filter(|(message_run, _)| *message_run == run)
                .map(|(_, line)| line.clone()),
        );
        if let Some((_, options)) = choices.iter().find(|(message_run, _)| *message_run == run) {
            talk.choice_pending = true;
            talk.options.clone_from(options);
        }
        for (_, name) in events.iter().filter(|(message_run, _)| *message_run == run) {
            talk.last_event.clone_from(name);
        }
        let done = finished.iter().filter(|message_run| **message_run == run).count();
        if done > 0 {
            talk.talking = false;
            talk.choice_pending = false;
            talk.run = 0;
            talk.finished += u32::try_from(done).unwrap_or(u32::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::caption::{
        CaptionId, CaptionKind, CaptionPiece, CaptionStyleHint, CaptionText, CaptionTime,
    };
    use regia_framework::conversation::{ChoiceView, FinishReason, RunId};

    fn caption(run: u64, line: &str) -> CaptionStart {
        CaptionStart {
            id: CaptionId(run),
            source: CaptionSource::Dialogue {
                run,
                line: line.to_owned(),
            },
            kind: CaptionKind::Speech,
            locale: "en".to_owned(),
            pieces: vec![CaptionPiece {
                offset_us: 0,
                text: CaptionText::Key {
                    key: line.to_owned(),
                    args: Vec::new(),
                },
            }],
            speaker: None,
            priority: 0,
            start: CaptionTime::Now,
            planned_us: 1000,
            keep_breaks: false,
            hint: CaptionStyleHint::default(),
        }
    }

    #[test]
    fn track_mirrors_only_the_matching_run() {
        let mut app = App::new();
        app.add_message::<CaptionStart>()
            .add_message::<ConversationChoiceRequested>()
            .add_message::<ConversationEvent>()
            .add_message::<ConversationFinished>()
            .add_systems(Update, track);
        let guide = app
            .world_mut()
            .spawn(TalkState {
                run: 7,
                talking: true,
                ..Default::default()
            })
            .id();
        let w = app.world_mut();
        // A second conversation's messages in the same frames: none of it may reach the guide.
        w.write_message(caption(9, "other#line"));
        w.write_message(ConversationChoiceRequested {
            run: RunId(9),
            prompt: None,
            options: vec![ChoiceView {
                index: 0,
                text: "Foreign.".to_owned(),
            }],
        });
        w.write_message(ConversationEvent {
            run: RunId(9),
            name: "foreign_event".to_owned(),
        });
        w.write_message(ConversationFinished {
            run: RunId(9),
            reason: FinishReason::Finished,
        });
        w.write_message(caption(7, "guide#greet"));
        w.write_message(ConversationChoiceRequested {
            run: RunId(7),
            prompt: None,
            options: vec![ChoiceView {
                index: 0,
                text: "Gladly.".to_owned(),
            }],
        });
        w.write_message(ConversationEvent {
            run: RunId(7),
            name: "quest_given".to_owned(),
        });
        app.update();
        let talk = app.world().get::<TalkState>(guide).expect("the talk state");
        assert_eq!(talk.lines_seen, vec!["guide#greet".to_owned()]);
        assert!(talk.choice_pending);
        assert_eq!(talk.options, vec!["Gladly.".to_owned()]);
        assert_eq!(talk.last_event, "quest_given");
        assert!(talk.talking, "the foreign run's finish did not end the guide run");
        assert_eq!(talk.finished, 0);
        // The guide's own finish ends it.
        app.world_mut().write_message(ConversationFinished {
            run: RunId(7),
            reason: FinishReason::Finished,
        });
        app.update();
        let talk = app.world().get::<TalkState>(guide).expect("the talk state");
        assert!(!talk.talking);
        assert_eq!(talk.finished, 1);
    }

    #[test]
    fn ensure_localization_merges_into_an_existing_empty_localization() {
        // `UiPlugin` always creates the resource, possibly with no locales in it (no `.ftl`
        // found): the guide's texts must land anyway.
        let mut world = World::new();
        world.insert_resource(Localization::default());
        ensure_localization(&mut world);
        let loc = world.get_resource::<Localization>().expect("the resource");
        assert!(loc.has_in("en", "npc-guide-greet"));
        assert!(loc.has_in("de", "npc-guide-greet"));
    }

    #[test]
    fn ensure_localization_keeps_a_hosts_texts() {
        let mut world = World::new();
        let mut loc = Localization::default();
        loc.add_locale("en".to_owned(), "host-key = Host text".to_owned())
            .expect("the host locale");
        world.insert_resource(loc);
        ensure_localization(&mut world);
        let loc = world.get_resource::<Localization>().expect("the resource");
        assert!(loc.has_in("en", "host-key"), "the host's own message stays");
        assert!(loc.has_in("en", "npc-guide-greet"), "the guide text merged in");
    }

    #[test]
    fn ensure_guide_conversation_merges_without_overwriting() {
        let mut world = World::new();
        let mut library = ConversationLibrary::default();
        library.clips.insert("host/voice.ogg".to_owned(), Some(1000));
        world.insert_resource(library);
        ensure_guide_conversation(&mut world);
        let library = world.get_resource::<ConversationLibrary>().expect("the library");
        assert!(
            library.conversations.contains_key(GUIDE_CONVERSATION),
            "the guide conversation loaded"
        );
        assert!(
            library.speakers.speakers.contains_key("guide"),
            "the guide's speaker loaded"
        );
        assert_eq!(
            library.clips.get("host/voice.ogg"),
            Some(&Some(1000)),
            "a host's own entries are never replaced"
        );
    }
}
