// SPDX-License-Identifier: MPL-2.0
//! Theme sounds for UI events (ADR-273 T12, ADR-1842): hover, press, focus, confirm, toggle and
//! cancel cues named by a [`UiSounds`] table.
//!
//! [`emit_ui_sounds`] reads [`UiEvent`]s and writes one [`UiSoundRequest`] per event that has a
//! cue in the table. `regia_ui` does not play audio: the `regia` facade forwards each request to
//! `Audio::play_cue`, and tests read the requests (or [`UiSoundLog`]) instead. A node can mute its
//! own sounds or replace a cue with [`UiSoundOverride`]. At most [`UiSounds::max_per_frame`]
//! requests are sent per frame, so a pointer sweeping a menu cannot stack voices.
//!
//! The agent surface is `ui.sounds` (`book/agents/ui-sounds.md`).

use std::collections::{BTreeMap, VecDeque};

use regia_app::prelude::*;

use crate::interaction::{UiEvent, UiEventKind, UiValue};

/// Requests kept in [`UiSoundLog`].
pub const LOG_CAPACITY: usize = 64;

/// Which moment a sound belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UiSoundCue {
    /// The pointer entered a node.
    Hover,
    /// A node was pressed or activated (click).
    Press,
    /// Keyboard or gamepad focus arrived on a node.
    Focus,
    /// A value was committed (text submitted, slider or dropdown changed).
    Confirm,
    /// A toggle changed.
    Toggle,
    /// A context click or other secondary action.
    Cancel,
}

impl UiSoundCue {
    /// Every cue.
    pub const ALL: [UiSoundCue; 6] = [
        Self::Hover,
        Self::Press,
        Self::Focus,
        Self::Confirm,
        Self::Toggle,
        Self::Cancel,
    ];

    /// Lowercase name for the agent protocol.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Press => "press",
            Self::Focus => "focus",
            Self::Confirm => "confirm",
            Self::Toggle => "toggle",
            Self::Cancel => "cancel",
        }
    }

    /// The cue called `name`.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The cue an event raises, if any.
    pub fn of_event(kind: &UiEventKind) -> Option<Self> {
        match kind {
            UiEventKind::PointerEnter => Some(Self::Hover),
            UiEventKind::Click => Some(Self::Press),
            UiEventKind::FocusChanged(true) => Some(Self::Focus),
            UiEventKind::ValueChanged(UiValue::Bool(_)) => Some(Self::Toggle),
            UiEventKind::ValueChanged(_) | UiEventKind::Submitted(_) => Some(Self::Confirm),
            UiEventKind::ContextClick => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// The cue-to-sound table. Sound names are `Audio` cue names.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct UiSounds {
    /// Master switch.
    pub enabled: bool,
    /// Cue name per UI cue; a missing entry is silent.
    pub sounds: BTreeMap<UiSoundCue, String>,
    /// Most requests sent in one frame.
    pub max_per_frame: usize,
}

impl Default for UiSounds {
    fn default() -> Self {
        Self {
            enabled: true,
            sounds: BTreeMap::new(),
            max_per_frame: 4,
        }
    }
}

/// Per-node sound control.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct UiSoundOverride {
    /// The node makes no sounds.
    pub muted: bool,
    /// Replacement sound per cue (an empty string silences that cue).
    pub sounds: BTreeMap<UiSoundCue, String>,
}

/// A request to play a UI sound.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct UiSoundRequest {
    /// Why.
    pub cue: UiSoundCue,
    /// The audio cue name to play.
    pub sound: String,
    /// The node the event was about.
    pub entity: Entity,
}

/// The last [`LOG_CAPACITY`] requests, oldest first, for tests and `ui.sounds`.
#[derive(Resource, Debug, Default, Clone)]
pub struct UiSoundLog {
    /// Recent requests.
    pub recent: VecDeque<UiSoundRequest>,
    /// Requests dropped by the per-frame cap since the start.
    pub dropped: u64,
}

/// Picks the sound for `cue` on a node: its override first, then the table. `None` means silence.
pub fn resolve_sound<'a>(
    table: &'a UiSounds,
    node: Option<&'a UiSoundOverride>,
    cue: UiSoundCue,
) -> Option<&'a str> {
    if !table.enabled {
        return None;
    }
    if let Some(o) = node {
        if o.muted {
            return None;
        }
        if let Some(s) = o.sounds.get(&cue) {
            return (!s.is_empty()).then_some(s.as_str());
        }
    }
    table
        .sounds
        .get(&cue)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

/// `Update`, after interaction: turns [`UiEvent`]s into [`UiSoundRequest`]s.
pub fn emit_ui_sounds(
    mut events: MessageReader<UiEvent>,
    table: Res<UiSounds>,
    overrides: Query<&UiSoundOverride>,
    mut out: MessageWriter<UiSoundRequest>,
    mut log: ResMut<UiSoundLog>,
) {
    let _span = tracing::debug_span!("emit_ui_sounds").entered();
    let mut sent = 0;
    for event in events.read() {
        let Some(cue) = UiSoundCue::of_event(&event.kind) else {
            continue;
        };
        let Some(sound) = resolve_sound(&table, overrides.get(event.target).ok(), cue) else {
            continue;
        };
        if sent >= table.max_per_frame {
            log.dropped += 1;
            continue;
        }
        sent += 1;
        let request = UiSoundRequest {
            cue,
            sound: sound.to_owned(),
            entity: event.target,
        };
        if log.recent.len() == LOG_CAPACITY {
            log.recent.pop_front();
        }
        log.recent.push_back(request.clone());
        out.write(request);
    }
}

/// What [`apply_theme_sounds`] last wrote: the theme generation and the cues it set.
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct ThemeSoundsApplied {
    /// The theme generation applied (0 = none yet).
    pub generation: u64,
    /// The cues the theme set, with the sound it set them to.
    pub cues: BTreeMap<UiSoundCue, String>,
}

/// The cue table a theme's `sounds` map asks for: known cue names only (unknown keys are
/// skipped), empty strings kept as silencing entries.
pub fn theme_cues(sounds: &BTreeMap<String, String>) -> BTreeMap<UiSoundCue, String> {
    sounds
        .iter()
        .filter_map(|(k, v)| UiSoundCue::parse(k).map(|c| (c, v.clone())))
        .collect()
}

/// Applies `next` theme cues over `table`, first taking back the cues the previous theme set
/// (`applied`) where the table still holds the theme's value (a cue the game set by hand since
/// stays). An empty sound removes the cue (silence). Returns whether the table changed.
pub fn swap_theme_cues(
    table: &mut BTreeMap<UiSoundCue, String>,
    applied: &BTreeMap<UiSoundCue, String>,
    next: &BTreeMap<UiSoundCue, String>,
) -> bool {
    let before = table.clone();
    for (cue, sound) in applied {
        if table.get(cue) == Some(sound) {
            table.remove(cue);
        }
    }
    for (cue, sound) in next {
        if sound.is_empty() {
            table.remove(cue);
        } else {
            table.insert(*cue, sound.clone());
        }
    }
    *table != before
}

/// `PostUpdate`: when the active theme changes, writes its `sounds` (ADR-2045) into
/// [`UiSounds`]. A theme without sounds (every built-in one) changes nothing, so the table a game
/// fills in code keeps working as before.
pub fn apply_theme_sounds(
    resolver: Option<Res<crate::theme_resolve::ThemeResolver>>,
    mut applied: ResMut<ThemeSoundsApplied>,
    mut sounds: ResMut<UiSounds>,
) {
    let Some(resolver) = resolver else {
        return;
    };
    let generation = resolver.generation();
    if generation == applied.generation {
        return;
    }
    let next = resolver
        .resolved_theme()
        .map(|t| theme_cues(&t.sounds))
        .unwrap_or_default();
    applied.generation = generation;
    if next.is_empty() && applied.cues.is_empty() {
        return;
    }
    let previous = std::mem::take(&mut applied.cues);
    let mut table = sounds.sounds.clone();
    if swap_theme_cues(&mut table, &previous, &next) {
        sounds.sounds = table;
    }
    applied.cues = next.into_iter().filter(|(_, s)| !s.is_empty()).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(target: Entity, kind: UiEventKind) -> UiEvent {
        UiEvent::new(target, kind)
    }

    fn app_with(table: UiSounds) -> App {
        let mut app = App::new();
        app.insert_resource(table)
            .init_resource::<UiSoundLog>()
            .add_message::<UiEvent>()
            .add_message::<UiSoundRequest>()
            .add_systems(Update, emit_ui_sounds);
        app
    }

    fn table() -> UiSounds {
        let mut t = UiSounds::default();
        t.sounds.insert(UiSoundCue::Hover, "ui_hover".into());
        t.sounds.insert(UiSoundCue::Press, "ui_press".into());
        t
    }

    #[test]
    fn events_map_to_cues() {
        assert_eq!(
            UiSoundCue::of_event(&UiEventKind::PointerEnter),
            Some(UiSoundCue::Hover)
        );
        assert_eq!(UiSoundCue::of_event(&UiEventKind::Click), Some(UiSoundCue::Press));
        assert_eq!(
            UiSoundCue::of_event(&UiEventKind::FocusChanged(true)),
            Some(UiSoundCue::Focus)
        );
        assert_eq!(UiSoundCue::of_event(&UiEventKind::FocusChanged(false)), None);
        assert_eq!(
            UiSoundCue::of_event(&UiEventKind::ValueChanged(UiValue::Bool(true))),
            Some(UiSoundCue::Toggle)
        );
        assert_eq!(UiSoundCue::of_event(&UiEventKind::DragEnd), None);
        for c in UiSoundCue::ALL {
            assert_eq!(UiSoundCue::parse(c.name()), Some(c));
        }
    }

    #[test]
    fn requests_are_sent_and_logged() {
        let mut app = app_with(table());
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().write_message(event(e, UiEventKind::PointerEnter));
        app.world_mut().write_message(event(e, UiEventKind::Click));
        app.world_mut().write_message(event(e, UiEventKind::DragEnd));
        app.update();
        let log = app.world().resource::<UiSoundLog>();
        let got: Vec<_> = log.recent.iter().map(|r| (r.cue, r.sound.as_str())).collect();
        assert_eq!(
            got,
            [(UiSoundCue::Hover, "ui_hover"), (UiSoundCue::Press, "ui_press")]
        );
    }

    #[test]
    fn overrides_mute_replace_and_silence() {
        let mut app = app_with(table());
        let muted = app
            .world_mut()
            .spawn(UiSoundOverride {
                muted: true,
                ..Default::default()
            })
            .id();
        let mut custom = UiSoundOverride::default();
        custom.sounds.insert(UiSoundCue::Press, "big_click".into());
        custom.sounds.insert(UiSoundCue::Hover, String::new());
        let custom = app.world_mut().spawn(custom).id();
        for (e, k) in [
            (muted, UiEventKind::Click),
            (custom, UiEventKind::Click),
            (custom, UiEventKind::PointerEnter),
        ] {
            app.world_mut().write_message(event(e, k));
        }
        app.update();
        let log = app.world().resource::<UiSoundLog>();
        assert_eq!(log.recent.len(), 1);
        assert_eq!(log.recent[0].sound, "big_click");
    }

    #[test]
    fn disabled_table_and_cap() {
        let mut t = table();
        t.enabled = false;
        let mut app = app_with(t);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().write_message(event(e, UiEventKind::Click));
        app.update();
        assert!(app.world().resource::<UiSoundLog>().recent.is_empty());

        let mut t = table();
        t.max_per_frame = 2;
        let mut app = app_with(t);
        let e = app.world_mut().spawn_empty().id();
        for _ in 0..5 {
            app.world_mut().write_message(event(e, UiEventKind::Click));
        }
        app.update();
        let log = app.world().resource::<UiSoundLog>();
        assert_eq!(log.recent.len(), 2);
        assert_eq!(log.dropped, 3);
    }

    #[test]
    fn log_keeps_the_newest() {
        let mut app = app_with(table());
        let e = app.world_mut().spawn_empty().id();
        for _ in 0..(LOG_CAPACITY + 10) {
            app.world_mut().write_message(event(e, UiEventKind::Click));
            app.update();
        }
        assert_eq!(app.world().resource::<UiSoundLog>().recent.len(), LOG_CAPACITY);
    }

    #[test]
    fn theme_cues_swap_and_keep_hand_set_entries() {
        let theme_a: BTreeMap<String, String> = [("hover", "a_hover"), ("press", "a_press"), ("whoosh", "x")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let a = theme_cues(&theme_a);
        assert_eq!(a.len(), 2, "unknown cue names are skipped");
        let mut table: BTreeMap<UiSoundCue, String> = [(UiSoundCue::Confirm, "game_ok".to_owned())]
            .into_iter()
            .collect();
        assert!(swap_theme_cues(&mut table, &BTreeMap::new(), &a));
        assert_eq!(table.len(), 3);
        // The game replaces press by hand; theme B sets only focus and silences confirm.
        table.insert(UiSoundCue::Press, "game_press".into());
        let b = theme_cues(
            &[("focus", "b_focus"), ("confirm", "")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        );
        assert!(swap_theme_cues(&mut table, &a, &b));
        assert_eq!(
            table.get(&UiSoundCue::Hover),
            None,
            "theme A's own cue is taken back"
        );
        assert_eq!(
            table.get(&UiSoundCue::Press).map(String::as_str),
            Some("game_press")
        );
        assert_eq!(table.get(&UiSoundCue::Focus).map(String::as_str), Some("b_focus"));
        assert_eq!(table.get(&UiSoundCue::Confirm), None, "an empty sound silences");
        assert!(!swap_theme_cues(
            &mut table.clone(),
            &BTreeMap::new(),
            &BTreeMap::new()
        ));
    }

    #[test]
    fn theme_sounds_merge_through_extends() {
        let mut set = crate::theme_file::ThemeSet::with_builtins();
        set.load_ron(
            r#"(format_version: 1, name: "loud", extends: Some("regia-dark"), sounds: {"hover": "tick", "press": "click"})"#,
            "loud.theme.ron",
        )
        .unwrap();
        set.load_ron(
            r#"(format_version: 1, name: "louder", extends: Some("loud"), sounds: {"press": "clack"})"#,
            "louder.theme.ron",
        )
        .unwrap();
        let t = set.resolve("louder", None).unwrap();
        assert_eq!(t.sounds.get("hover").map(String::as_str), Some("tick"));
        assert_eq!(t.sounds.get("press").map(String::as_str), Some("clack"));
        assert!(
            set.resolve("regia-dark", None).unwrap().sounds.is_empty(),
            "built-ins stay silent"
        );
    }
}
