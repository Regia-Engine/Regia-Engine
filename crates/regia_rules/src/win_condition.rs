// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/win_condition.rs, commit c9513623f), ADR-8012.
// Mark: adapt. Named issues: StrategyWin becomes the WinCondition trait; the two stock rules are
// lives-out and named-structures-destroyed, and the structure stems are data on the rule; the
// solo choice is WinRuleState instead of a process-global AtomicU8; the AI forfeit name and the
// timed AI concede path are not copied; starting lives are a setting (default 3, range 1..=10).

//! Win conditions a match evaluates against a [`MatchView`].
//!
//! [`WinCondition::evaluate`] returns the winner when the rule says the match is over.
//! [`LivesOut`] ends the match when at most one fighting team still has a seat with lives and
//! has not forfeited. [`StructuresDestroyed`] ends it when at most one fighting team still holds
//! a configured structure stem, and a team that never had those stems stays in. A game supplies
//! its own rule by implementing the trait. The lobby words `production` and `clones` parse as
//! the structures rule and the lives rule; they are not type names.

use serde::{Deserialize, Serialize};

use crate::RulesError;

/// Lowest starting-lives value the host may set.
pub const LIVES_MIN: u32 = 1;

/// Highest starting-lives value the host may set.
pub const LIVES_MAX: u32 = 10;

/// Starting lives when the host does not set them.
pub const DEFAULT_LIVES: u32 = 3;

/// Accept `lives` when it is inside [`LIVES_MIN`]..=[`LIVES_MAX`].
pub fn clamp_lives(lives: u32) -> Result<u32, RulesError> {
    if (LIVES_MIN..=LIVES_MAX).contains(&lives) {
        Ok(lives)
    } else {
        Err(RulesError::LivesRange {
            lives,
            min: LIVES_MIN,
            max: LIVES_MAX,
        })
    }
}

/// One seat's lives, as the win rule sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatLives {
    /// Fighting team.
    pub team: u8,
    /// Slot on that team.
    pub slot: u8,
    /// Lives left. Zero means this seat is out.
    pub lives: u32,
}

/// Whether one named structure is present for a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructureFact {
    /// The team has had this stem at some point in the match.
    pub seen: bool,
    /// The stem is still standing.
    pub alive: bool,
}

/// The match as a win rule reads it. The host fills this from its own sim, in a stable order.
pub trait MatchView {
    /// Seats that started on a fighting team.
    fn seats(&self) -> &[SeatLives];
    /// True when `team` has forfeited.
    fn forfeited(&self, team: u8) -> bool;
    /// The named structure on `team`. A stem the view has never recorded is unseen and not alive.
    fn structure(&self, team: u8, stem: &str) -> StructureFact;
    /// Fighting teams, in ascending order. A team that is not listed is not in the match.
    fn fighting_teams(&self) -> &[u8];
}

/// A plain roster a test or a host can fill in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RosterView {
    /// Seats.
    pub seats: Vec<SeatLives>,
    /// Teams that forfeited.
    pub forfeited: Vec<u8>,
    /// Structures the match has recorded.
    pub structures: Vec<StructureRow>,
    /// Fighting teams.
    pub teams: Vec<u8>,
}

/// One structure row on a [`RosterView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureRow {
    /// The team that owns it.
    pub team: u8,
    /// The stem name. The rule's configured list decides whether it counts.
    pub stem: String,
    /// The team has had this stem.
    pub seen: bool,
    /// It is still standing.
    pub alive: bool,
}

impl MatchView for RosterView {
    fn seats(&self) -> &[SeatLives] {
        &self.seats
    }

    fn forfeited(&self, team: u8) -> bool {
        self.forfeited.contains(&team)
    }

    fn structure(&self, team: u8, stem: &str) -> StructureFact {
        match self
            .structures
            .iter()
            .find(|row| row.team == team && row.stem == stem)
        {
            Some(row) => StructureFact {
                seen: row.seen,
                alive: row.alive,
            },
            None => StructureFact {
                seen: false,
                alive: false,
            },
        }
    }

    fn fighting_teams(&self) -> &[u8] {
        &self.teams
    }
}

/// Why a rule ended the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The winning team. 0 when every fighting team is out.
    pub winner: u8,
    /// The rule that decided it (`"lives"`, `"structures"`, or a custom rule's own word).
    pub rule: &'static str,
}

/// A rule that can end a match.
pub trait WinCondition {
    /// `Some` when this rule ends the match now.
    fn evaluate(&self, view: &dyn MatchView) -> Option<Outcome>;
}

/// A team is in while any of its seats has lives left and it has not forfeited.
/// An empty seat list means the team is not in. The match ends when at least one fighting
/// team is out and at most one remains.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LivesOut;

impl WinCondition for LivesOut {
    fn evaluate(&self, view: &dyn MatchView) -> Option<Outcome> {
        let fighting = view.fighting_teams();
        if fighting.is_empty() {
            return None;
        }
        let mut still = Vec::new();
        for team in fighting {
            if team_has_lives(view, *team) {
                still.push(*team);
            }
        }
        if still.len() < fighting.len() && still.len() <= 1 {
            Some(Outcome {
                winner: still.first().copied().unwrap_or(0),
                rule: "lives",
            })
        } else {
            None
        }
    }
}

fn team_has_lives(view: &dyn MatchView, team: u8) -> bool {
    if view.forfeited(team) {
        return false;
    }
    for seat in view.seats() {
        if seat.team == team && seat.lives > 0 {
            return true;
        }
    }
    false
}

/// A team stays in while any configured stem is alive, or while it has never had one of them.
/// An empty stem list keeps every team in. A stem that is not configured does not hold a team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuresDestroyed<'a> {
    /// Stems that count. The game supplies the names.
    pub stems: &'a [String],
}

impl WinCondition for StructuresDestroyed<'_> {
    fn evaluate(&self, view: &dyn MatchView) -> Option<Outcome> {
        let fighting = view.fighting_teams();
        if fighting.is_empty() || self.stems.is_empty() {
            return None;
        }
        let mut still = Vec::new();
        for team in fighting {
            if team_holds_structures(view, *team, self.stems) {
                still.push(*team);
            }
        }
        if still.len() < fighting.len() && still.len() <= 1 {
            Some(Outcome {
                winner: still.first().copied().unwrap_or(0),
                rule: "structures",
            })
        } else {
            None
        }
    }
}

fn team_holds_structures(view: &dyn MatchView, team: u8, stems: &[String]) -> bool {
    let mut any_seen = false;
    let mut any_alive = false;
    for stem in stems {
        let fact = view.structure(team, stem);
        if fact.seen {
            any_seen = true;
        }
        if fact.alive {
            any_alive = true;
        }
    }
    any_alive || !any_seen
}

/// The two stock rules. Wire id 0 is lives, 1 is structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StockRule {
    /// Seats out of lives, or a forfeit.
    #[default]
    Lives,
    /// Named structures destroyed. Lives do not end the match.
    Structures,
}

impl StockRule {
    /// Both stock rules, lives first.
    pub const ALL: [StockRule; 2] = [StockRule::Lives, StockRule::Structures];

    /// Lobby word: `"lives"` or `"structures"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lives => "lives",
            Self::Structures => "structures",
        }
    }

    /// Player-facing label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lives => "Lives",
            Self::Structures => "Structures",
        }
    }

    /// Parse a lobby word. `clones` is lives. `production` is structures. Those words are
    /// aliases for a game moving an older lobby file across; they are not type names.
    pub fn from_lobby_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lives" | "clones" => Some(Self::Lives),
            "structures" | "production" => Some(Self::Structures),
            _ => None,
        }
    }

    /// Stable id. 0 is lives, 1 is structures. Never renumber.
    pub fn wire_id(self) -> u8 {
        match self {
            Self::Lives => 0,
            Self::Structures => 1,
        }
    }

    /// The rule for a wire id.
    pub fn from_wire_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Lives),
            1 => Some(Self::Structures),
            _ => None,
        }
    }
}

/// The match's chosen rule and the structure stems. This is ordinary state: nothing global.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinRuleState {
    /// The stock rule. The default is lives.
    pub rule: StockRule,
    /// Stems the structures rule counts. Ignored by the lives rule.
    #[serde(default)]
    pub stems: Vec<String>,
    /// Starting lives the game applies. The default is [`DEFAULT_LIVES`].
    #[serde(default = "default_lives")]
    pub lives: u32,
}

fn default_lives() -> u32 {
    DEFAULT_LIVES
}

impl Default for WinRuleState {
    fn default() -> Self {
        Self {
            rule: StockRule::Lives,
            stems: Vec::new(),
            lives: DEFAULT_LIVES,
        }
    }
}

impl WinRuleState {
    /// Replace the stock rule.
    pub fn set_rule(&mut self, rule: StockRule) {
        self.rule = rule;
    }

    /// The stock rule.
    pub fn rule(&self) -> StockRule {
        self.rule
    }

    /// Set the rule from its wire id. An unknown id leaves the state unchanged.
    pub fn set_wire_id(&mut self, id: u8) -> bool {
        match StockRule::from_wire_id(id) {
            Some(rule) => {
                self.rule = rule;
                true
            }
            None => false,
        }
    }
}

impl WinCondition for WinRuleState {
    fn evaluate(&self, view: &dyn MatchView) -> Option<Outcome> {
        match self.rule {
            StockRule::Lives => LivesOut.evaluate(view),
            StockRule::Structures => StructuresDestroyed { stems: &self.stems }.evaluate(view),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(team: u8, slot: u8, lives: u32) -> SeatLives {
        SeatLives { team, slot, lives }
    }

    #[test]
    fn commander_lives_default_three_and_clamp_one_to_ten() {
        assert_eq!(DEFAULT_LIVES, 3);
        assert!(clamp_lives(1).is_ok());
        assert!(clamp_lives(10).is_ok());
        assert!(clamp_lives(0).is_err());
        assert!(clamp_lives(11).is_err());
        assert_eq!(WinRuleState::default().lives, 3);
    }

    #[test]
    fn team_with_two_seats_survives_until_both_are_out() {
        let mut view = RosterView {
            teams: vec![1, 2],
            seats: vec![seat(1, 0, 3), seat(1, 1, 0), seat(2, 0, 3)],
            ..RosterView::default()
        };
        assert!(
            LivesOut.evaluate(&view).is_none(),
            "one live seat keeps the team in"
        );
        view.seats[1].lives = 0;
        view.seats[0].lives = 0;
        let outcome = LivesOut.evaluate(&view).expect("the other team wins");
        assert_eq!(outcome.winner, 2);
        assert_eq!(outcome.rule, "lives");
    }

    #[test]
    fn a_forfeit_ends_the_conceding_team() {
        let view = RosterView {
            teams: vec![1, 2],
            seats: vec![seat(1, 0, 3), seat(2, 0, 3)],
            forfeited: vec![1],
            ..RosterView::default()
        };
        let outcome = LivesOut.evaluate(&view).expect("forfeit ends the match");
        assert_eq!(outcome.winner, 2);
    }

    #[test]
    fn a_team_that_never_built_stays_in_while_a_stripped_team_is_out() {
        let stems = vec!["forge".to_string(), "yard".to_string()];
        let rule = StructuresDestroyed { stems: &stems };
        let mut view = RosterView {
            teams: vec![1, 2],
            structures: vec![
                StructureRow {
                    team: 2,
                    stem: "forge".into(),
                    seen: true,
                    alive: false,
                },
                StructureRow {
                    team: 2,
                    stem: "yard".into(),
                    seen: true,
                    alive: true,
                },
            ],
            ..RosterView::default()
        };
        assert!(
            rule.evaluate(&view).is_none(),
            "two sides remain while one structure stands"
        );
        view.structures[1].alive = false;
        let outcome = rule.evaluate(&view).expect("the stripped team is out");
        assert_eq!(outcome.winner, 1, "a team that never built stays in");

        view.teams.push(3);
        assert!(
            rule.evaluate(&view).is_none(),
            "the match does not end while two sides remain"
        );
    }

    #[test]
    fn the_rule_choice_is_state_and_defaults_to_lives() {
        let state = WinRuleState::default();
        assert_eq!(state.rule(), StockRule::Lives);
        assert_eq!(state.rule().wire_id(), 0);
        let text = serde_json::to_string(&state).expect("serialize");
        let back: WinRuleState = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, state);

        let mut picked = WinRuleState::default();
        assert!(picked.set_wire_id(1));
        assert_eq!(picked.rule(), StockRule::Structures);
        assert_eq!(StockRule::from_wire_id(1).map(StockRule::wire_id), Some(1));
        assert!(!picked.set_wire_id(9));
        assert_eq!(picked.rule(), StockRule::Structures);
        let text = serde_json::to_string(&picked).expect("serialize");
        let back: WinRuleState = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.rule(), StockRule::Structures);
        assert_eq!(
            StockRule::from_lobby_str("production"),
            Some(StockRule::Structures)
        );
        assert_eq!(StockRule::from_lobby_str("clones"), Some(StockRule::Lives));
        assert_eq!(
            StockRule::from_lobby_str("structures"),
            Some(StockRule::Structures)
        );
    }

    #[test]
    fn structures_count_only_the_configured_stems() {
        let stems = vec!["forge".to_string()];
        let rule = StructuresDestroyed { stems: &stems };
        let view = RosterView {
            teams: vec![1, 2],
            structures: vec![
                StructureRow {
                    team: 1,
                    stem: "forge".into(),
                    seen: true,
                    alive: false,
                },
                StructureRow {
                    team: 1,
                    stem: "yard".into(),
                    seen: true,
                    alive: true,
                },
                StructureRow {
                    team: 2,
                    stem: "forge".into(),
                    seen: true,
                    alive: true,
                },
            ],
            ..RosterView::default()
        };
        let outcome = rule
            .evaluate(&view)
            .expect("the unlisted stem does not hold the team");
        assert_eq!(outcome.winner, 2);
    }

    struct FirstTeam;

    impl WinCondition for FirstTeam {
        fn evaluate(&self, view: &dyn MatchView) -> Option<Outcome> {
            view.fighting_teams().first().copied().map(|winner| Outcome {
                winner,
                rule: "custom",
            })
        }
    }

    #[test]
    fn win_condition_trait_runs_a_custom_rule() {
        let view = RosterView {
            teams: vec![2, 1],
            ..RosterView::default()
        };
        let outcome = FirstTeam.evaluate(&view).expect("the custom rule ran");
        assert_eq!(outcome.winner, 2);
        assert_eq!(outcome.rule, "custom");
    }
}
