// SPDX-License-Identifier: MPL-2.0
//! Leaderboards for games (`docs/specs/24-steam.md` § 2a, ADR-2340).
//!
//! The game talks to the [`Leaderboard`] resource and never to a platform SDK:
//!
//! ```ignore
//! fn finish_lap(mut boards: ResMut<Leaderboard>, lap: Res<LapTimer>) {
//!     boards.declare(LeaderboardSpec::new("fastest_lap").ascending().display(LeaderboardDisplay::TimeMilliseconds));
//!     boards.submit("fastest_lap", lap.millis, &[]).ok();
//!     boards.request("fastest_lap", LeaderboardRange::AroundUser { before: 4, after: 5 }).ok();
//! }
//! ```
//!
//! Every call queues work; [`leaderboard_system`] (`PreUpdate`) finds each board once, sends the
//! queued uploads, downloads and attachments to the [`LeaderboardService`] and turns the answers
//! into [`LeaderboardMessage`]s and the per-board [`BoardState`]. Platform services answer
//! asynchronously, so results land a frame or more later.
//!
//! Services: [`UnavailableLeaderboards`] (the default: every operation fails with
//! [`LeaderboardError::Unavailable`]), [`LocalLeaderboards`] (in memory, deterministic: offline
//! play, dev and tests) and, in `regia_steam`, the Steam service (`SteamLeaderboardPlugin`)
//! that `regia_steam::MockSteam` also backs with a [`LocalLeaderboards`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use regia_app::prelude::*;

/// Longest board name, in bytes (Steam's own limit, `k_cchLeaderboardNameMax`).
pub const MAX_NAME_BYTES: usize = 128;
/// Most detail values one entry may carry (Steam's `k_cLeaderboardDetailsMax`).
pub const MAX_DETAILS: usize = 64;
/// Most rows one download may ask for (a Regia Engine cap, so one request cannot stall a frame).
pub const MAX_DOWNLOAD_ROWS: u32 = 1000;

/// A board found on the service (Steam's `SteamLeaderboard_t`). Valid for the service that
/// produced it; [`Leaderboard::set_service`] forgets every handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaderboardHandle(pub u64);

/// One request to the service; its answer carries the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// A user-generated-content file handle attached to an entry (Steam's `UGCHandle_t`, from a
/// cloud file share). Opaque to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UgcHandle(pub u64);

/// Which scores rank first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LeaderboardSort {
    /// Lowest score first (lap times).
    Ascending,
    /// Highest score first (points). The default.
    #[default]
    Descending,
}

impl LeaderboardSort {
    /// `ascending` or `descending`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }

    /// Parses [`LeaderboardSort::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "ascending" => Some(Self::Ascending),
            "descending" => Some(Self::Descending),
            _ => None,
        }
    }

    /// Whether `a` ranks before `b` under this order (strictly better).
    pub fn better(self, a: i32, b: i32) -> bool {
        match self {
            Self::Ascending => a < b,
            Self::Descending => a > b,
        }
    }
}

/// How the platform UI shows a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LeaderboardDisplay {
    /// A plain number. The default.
    #[default]
    Numeric,
    /// The score is a number of seconds.
    TimeSeconds,
    /// The score is a number of milliseconds.
    TimeMilliseconds,
}

impl LeaderboardDisplay {
    /// `numeric`, `time_seconds` or `time_milliseconds`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Numeric => "numeric",
            Self::TimeSeconds => "time_seconds",
            Self::TimeMilliseconds => "time_milliseconds",
        }
    }

    /// Parses [`LeaderboardDisplay::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "numeric" => Some(Self::Numeric),
            "time_seconds" => Some(Self::TimeSeconds),
            "time_milliseconds" => Some(Self::TimeMilliseconds),
            _ => None,
        }
    }
}

/// What an upload does to an existing entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UploadMethod {
    /// Keep the better of the old and new score. The default.
    #[default]
    KeepBest,
    /// Replace the entry whatever its score.
    ForceUpdate,
}

impl UploadMethod {
    /// `keep_best` or `force_update`.
    pub fn name(self) -> &'static str {
        match self {
            Self::KeepBest => "keep_best",
            Self::ForceUpdate => "force_update",
        }
    }

    /// Parses [`UploadMethod::name`] (`force` is accepted for `force_update`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "keep_best" => Some(Self::KeepBest),
            "force_update" | "force" => Some(Self::ForceUpdate),
            _ => None,
        }
    }
}

/// Which rows a download returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderboardRange {
    /// Global ranks `start..=end` (1-based).
    Global {
        /// First rank, at least 1.
        start: u32,
        /// Last rank, at least `start`.
        end: u32,
    },
    /// The local user's row with `before` rows above and `after` below. Empty when the local user
    /// has no entry.
    AroundUser {
        /// Rows above the user.
        before: u32,
        /// Rows below the user.
        after: u32,
    },
    /// The local user and their friends, in global rank order with global ranks.
    Friends,
}

impl LeaderboardRange {
    /// The top `n` rows (`Global { start: 1, end: n }`).
    pub fn top(n: u32) -> Self {
        Self::Global {
            start: 1,
            end: n.max(1),
        }
    }

    /// `global`, `around_user` or `friends`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Global { .. } => "global",
            Self::AroundUser { .. } => "around_user",
            Self::Friends => "friends",
        }
    }

    /// Checks the bounds ([`MAX_DOWNLOAD_ROWS`] rows at most).
    pub fn check(self) -> Result<(), LeaderboardError> {
        match self {
            Self::Global { start, end } => {
                if start == 0 || end < start {
                    return Err(LeaderboardError::RangeInvalid(format!(
                        "global range needs 1 <= start <= end, got {start}..={end}"
                    )));
                }
                if end - start >= MAX_DOWNLOAD_ROWS {
                    return Err(LeaderboardError::RangeInvalid(format!(
                        "at most {MAX_DOWNLOAD_ROWS} rows per download"
                    )));
                }
            }
            Self::AroundUser { before, after } => {
                if u64::from(before) + u64::from(after) >= u64::from(MAX_DOWNLOAD_ROWS) {
                    return Err(LeaderboardError::RangeInvalid(format!(
                        "before + after + 1 must be at most {MAX_DOWNLOAD_ROWS}"
                    )));
                }
            }
            Self::Friends => {}
        }
        Ok(())
    }
}

/// A board's description: its name and how it is created when missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardSpec {
    /// The board's name (1..=128 bytes, no NUL).
    pub name: String,
    /// Sort order used when the service creates the board. An existing board keeps its own.
    pub sort: LeaderboardSort,
    /// Display type used when the service creates the board.
    pub display: LeaderboardDisplay,
    /// The upload method [`Leaderboard::submit`] uses.
    pub method: UploadMethod,
    /// Create the board when it does not exist (Steam's find-or-create). Off by default: a board
    /// made on the partner site is found by name.
    pub create: bool,
}

impl LeaderboardSpec {
    /// A find-only, descending, numeric, keep-best board.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sort: LeaderboardSort::Descending,
            display: LeaderboardDisplay::Numeric,
            method: UploadMethod::KeepBest,
            create: false,
        }
    }

    /// Lowest score ranks first.
    #[must_use]
    pub fn ascending(mut self) -> Self {
        self.sort = LeaderboardSort::Ascending;
        self
    }

    /// Sets the display type.
    #[must_use]
    pub fn display(mut self, display: LeaderboardDisplay) -> Self {
        self.display = display;
        self
    }

    /// Sets the upload method of [`Leaderboard::submit`].
    #[must_use]
    pub fn method(mut self, method: UploadMethod) -> Self {
        self.method = method;
        self
    }

    /// Creates the board when it is missing.
    #[must_use]
    pub fn create_if_missing(mut self) -> Self {
        self.create = true;
        self
    }
}

/// A found board as the service reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardInfo {
    /// The service's handle.
    pub handle: LeaderboardHandle,
    /// The board's name.
    pub name: String,
    /// The board's sort order (its own, not necessarily the spec's).
    pub sort: LeaderboardSort,
    /// The board's display type.
    pub display: LeaderboardDisplay,
    /// Entries on the board when it was found.
    pub entry_count: u32,
}

/// One downloaded row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardEntry {
    /// The user's platform id (the 64-bit Steam id on Steam).
    pub user: u64,
    /// Global rank, 1-based.
    pub rank: u32,
    /// The score.
    pub score: i32,
    /// Game-defined detail values (at most [`MAX_DETAILS`]).
    pub details: Vec<i32>,
    /// The attached UGC file, if any (always `None` where the service cannot report it).
    pub ugc: Option<UgcHandle>,
}

/// What an upload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadOutcome {
    /// The score that was uploaded.
    pub score: i32,
    /// Whether the board's entry changed (a keep-best upload of a worse score does not).
    pub changed: bool,
    /// The user's global rank after the upload.
    pub rank_new: u32,
    /// The user's rank before the upload; `None` when they had no entry.
    pub rank_previous: Option<u32>,
}

/// Why a leaderboard operation failed. [`LeaderboardError::code`] is the stable agent code.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum LeaderboardError {
    /// No service: Steam not running, the feature off, or no service configured.
    #[error("no leaderboard service is available")]
    Unavailable,
    /// A board name is empty, longer than 128 bytes or holds a NUL.
    #[error("leaderboard name must be 1..=128 bytes without NUL")]
    NameInvalid,
    /// The board does not exist (and was not to be created), or the handle is unknown.
    #[error("leaderboard `{0}` not found")]
    NotFound(String),
    /// More than [`MAX_DETAILS`] detail values.
    #[error("{0} detail values; at most 64")]
    DetailsTooLong(usize),
    /// A download range is out of bounds.
    #[error("invalid range: {0}")]
    RangeInvalid(String),
    /// The local user has no entry on the board (attaching UGC needs one).
    #[error("the local user has no entry on this board")]
    NoEntry,
    /// The service cannot attach UGC to entries.
    #[error("this leaderboard service cannot attach UGC")]
    UgcUnsupported,
    /// The platform reported a failure (network, IO, rate limit).
    #[error("the leaderboard platform refused: {0}")]
    Platform(String),
}

impl LeaderboardError {
    /// The stable lowercase code (agent protocol, `book/agents/steam-services.md`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => "leaderboard_unavailable",
            Self::NameInvalid => "name_invalid",
            Self::NotFound(_) => "leaderboard_not_found",
            Self::DetailsTooLong(_) => "details_too_long",
            Self::RangeInvalid(_) => "range_invalid",
            Self::NoEntry => "no_entry",
            Self::UgcUnsupported => "ugc_unsupported",
            Self::Platform(_) => "leaderboard_platform",
        }
    }
}

/// Checks a board name.
pub fn check_name(name: &str) -> Result<(), LeaderboardError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
        return Err(LeaderboardError::NameInvalid);
    }
    Ok(())
}

/// Checks an upload's detail values.
pub fn check_details(details: &[i32]) -> Result<(), LeaderboardError> {
    if details.len() > MAX_DETAILS {
        return Err(LeaderboardError::DetailsTooLong(details.len()));
    }
    Ok(())
}

/// A service's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    /// A find (or find-or-create) finished. `Ok(None)`: no such board.
    Found {
        /// The request.
        request: RequestId,
        /// The board name asked for.
        name: String,
        /// The board, or none.
        result: Result<Option<BoardInfo>, LeaderboardError>,
    },
    /// An upload finished.
    Uploaded {
        /// The request.
        request: RequestId,
        /// The board.
        board: LeaderboardHandle,
        /// What the upload did.
        result: Result<UploadOutcome, LeaderboardError>,
    },
    /// A download finished.
    Downloaded {
        /// The request.
        request: RequestId,
        /// The board.
        board: LeaderboardHandle,
        /// The range asked for.
        range: LeaderboardRange,
        /// The rows, in rank order.
        result: Result<Vec<LeaderboardEntry>, LeaderboardError>,
    },
    /// A UGC attachment finished.
    UgcAttached {
        /// The request.
        request: RequestId,
        /// The board.
        board: LeaderboardHandle,
        /// The handle attached.
        ugc: UgcHandle,
        /// Whether it was attached.
        result: Result<(), LeaderboardError>,
    },
}

impl ServiceEvent {
    /// The request this event answers.
    pub fn request(&self) -> RequestId {
        match self {
            Self::Found { request, .. }
            | Self::Uploaded { request, .. }
            | Self::Downloaded { request, .. }
            | Self::UgcAttached { request, .. } => *request,
        }
    }
}

/// A leaderboard platform. Requests return at once with a [`RequestId`]; the answers come from
/// [`LeaderboardService::poll`], which [`leaderboard_system`] calls once per frame. Implementations
/// are internally synchronized (`&self` everywhere).
pub trait LeaderboardService: Send + Sync {
    /// `none`, `local` or `steam`.
    fn name(&self) -> &'static str;
    /// The local user's platform id; `None` when unknown.
    fn local_user(&self) -> Option<u64>;
    /// Finds a board by name, creating it when `spec.create` and it is missing.
    fn find(&self, spec: &LeaderboardSpec) -> Result<RequestId, LeaderboardError>;
    /// Uploads the local user's score.
    fn upload(
        &self,
        board: LeaderboardHandle,
        method: UploadMethod,
        score: i32,
        details: &[i32],
    ) -> Result<RequestId, LeaderboardError>;
    /// Downloads a range of rows.
    fn download(
        &self,
        board: LeaderboardHandle,
        range: LeaderboardRange,
    ) -> Result<RequestId, LeaderboardError>;
    /// Attaches a UGC file to the local user's entry.
    fn attach_ugc(&self, board: LeaderboardHandle, ugc: UgcHandle) -> Result<RequestId, LeaderboardError>;
    /// Drains the answers that arrived since the last call.
    fn poll(&self) -> Vec<ServiceEvent>;
}

/// No platform: every request is [`LeaderboardError::Unavailable`].
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableLeaderboards;

impl LeaderboardService for UnavailableLeaderboards {
    fn name(&self) -> &'static str {
        "none"
    }
    fn local_user(&self) -> Option<u64> {
        None
    }
    fn find(&self, _spec: &LeaderboardSpec) -> Result<RequestId, LeaderboardError> {
        Err(LeaderboardError::Unavailable)
    }
    fn upload(
        &self,
        _board: LeaderboardHandle,
        _method: UploadMethod,
        _score: i32,
        _details: &[i32],
    ) -> Result<RequestId, LeaderboardError> {
        Err(LeaderboardError::Unavailable)
    }
    fn download(
        &self,
        _board: LeaderboardHandle,
        _range: LeaderboardRange,
    ) -> Result<RequestId, LeaderboardError> {
        Err(LeaderboardError::Unavailable)
    }
    fn attach_ugc(&self, _board: LeaderboardHandle, _ugc: UgcHandle) -> Result<RequestId, LeaderboardError> {
        Err(LeaderboardError::Unavailable)
    }
    fn poll(&self) -> Vec<ServiceEvent> {
        Vec::new()
    }
}

/// One stored score of [`LocalLeaderboards`].
#[derive(Debug, Clone)]
struct LocalEntry {
    user: u64,
    score: i32,
    details: Vec<i32>,
    ugc: Option<UgcHandle>,
    /// When the score was set (a counter): the earlier of two equal scores ranks first.
    seq: u64,
}

/// One board of [`LocalLeaderboards`]; `entries` stays in rank order.
#[derive(Debug, Clone)]
struct LocalBoard {
    name: String,
    sort: LeaderboardSort,
    display: LeaderboardDisplay,
    entries: Vec<LocalEntry>,
}

impl LocalBoard {
    fn info(&self, handle: LeaderboardHandle) -> BoardInfo {
        BoardInfo {
            handle,
            name: self.name.clone(),
            sort: self.sort,
            display: self.display,
            entry_count: u32::try_from(self.entries.len()).unwrap_or(u32::MAX),
        }
    }

    /// Re-sorts after a change: better score first, then earlier set, then lower user id.
    fn rank(&mut self) {
        let sort = self.sort;
        self.entries.sort_by(|a, b| {
            if sort.better(a.score, b.score) {
                std::cmp::Ordering::Less
            } else if sort.better(b.score, a.score) {
                std::cmp::Ordering::Greater
            } else {
                a.seq.cmp(&b.seq).then(a.user.cmp(&b.user))
            }
        });
    }

    fn position(&self, user: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.user == user)
    }

    fn row(&self, index: usize) -> LeaderboardEntry {
        let e = &self.entries[index];
        LeaderboardEntry {
            user: e.user,
            rank: rank_of(index),
            score: e.score,
            details: e.details.clone(),
            ugc: e.ugc,
        }
    }

    /// Writes `user`'s score with `method`; returns the outcome.
    fn write(
        &mut self,
        user: u64,
        method: UploadMethod,
        score: i32,
        details: &[i32],
        seq: u64,
    ) -> UploadOutcome {
        let previous = self.position(user);
        let rank_previous = previous.map(rank_of);
        let changed = match previous {
            None => {
                self.entries.push(LocalEntry {
                    user,
                    score,
                    details: details.to_vec(),
                    ugc: None,
                    seq,
                });
                true
            }
            Some(index) => {
                let entry = &mut self.entries[index];
                let write = match method {
                    UploadMethod::KeepBest => self.sort.better(score, entry.score),
                    UploadMethod::ForceUpdate => true,
                };
                if write {
                    let changed = entry.score != score || entry.details != details;
                    entry.score = score;
                    entry.details = details.to_vec();
                    entry.seq = seq;
                    changed
                } else {
                    false
                }
            }
        };
        self.rank();
        let rank_new = self.position(user).map_or(0, rank_of);
        UploadOutcome {
            score,
            changed,
            rank_new,
            rank_previous,
        }
    }

    fn rows(&self, range: LeaderboardRange, local: u64, friends: &BTreeSet<u64>) -> Vec<LeaderboardEntry> {
        let count = self.entries.len();
        let window = match range {
            LeaderboardRange::Global { start, end } => {
                let first = (start as usize).saturating_sub(1);
                let last = (end as usize).min(count);
                first..last.max(first)
            }
            LeaderboardRange::AroundUser { before, after } => match self.position(local) {
                Some(at) => {
                    let first = at.saturating_sub(before as usize);
                    let last = at.saturating_add(after as usize).saturating_add(1).min(count);
                    first..last
                }
                None => 0..0,
            },
            LeaderboardRange::Friends => {
                return (0..count)
                    .filter(|&i| {
                        let user = self.entries[i].user;
                        user == local || friends.contains(&user)
                    })
                    .map(|i| self.row(i))
                    .collect();
            }
        };
        window.map(|i| self.row(i)).collect()
    }
}

fn rank_of(index: usize) -> u32 {
    u32::try_from(index).map_or(u32::MAX, |i| i.saturating_add(1))
}

#[derive(Debug, Default)]
struct LocalState {
    local_user: u64,
    friends: BTreeSet<u64>,
    /// Board `i` has handle `i + 1`.
    boards: Vec<LocalBoard>,
    next_request: u64,
    next_seq: u64,
    events: Vec<ServiceEvent>,
    fail_next: Option<LeaderboardError>,
    ugc_supported: bool,
    requests: u64,
}

impl LocalState {
    fn request(&mut self) -> RequestId {
        self.next_request += 1;
        self.requests += 1;
        RequestId(self.next_request)
    }

    fn board_index(&self, board: LeaderboardHandle) -> Result<usize, LeaderboardError> {
        usize::try_from(board.0)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .filter(|&i| i < self.boards.len())
            .ok_or_else(|| LeaderboardError::NotFound(format!("handle {}", board.0)))
    }

    fn by_name(&self, name: &str) -> Option<usize> {
        self.boards.iter().position(|b| b.name == name)
    }

    fn create(&mut self, name: &str, sort: LeaderboardSort, display: LeaderboardDisplay) -> usize {
        if let Some(index) = self.by_name(name) {
            return index;
        }
        self.boards.push(LocalBoard {
            name: name.to_owned(),
            sort,
            display,
            entries: Vec::new(),
        });
        self.boards.len() - 1
    }

    fn handle(index: usize) -> LeaderboardHandle {
        LeaderboardHandle(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1))
    }
}

/// An in-memory leaderboard service: deterministic, single process. Use it for offline play and
/// dev ([`LeaderboardPlugin::local`]) and in tests; `regia_steam::MockSteam` backs its Steam
/// leaderboards with one. Answers arrive on the next [`LeaderboardService::poll`], like a real
/// platform's.
///
/// Ranking: better score first (by the board's sort), then the earlier set score, then the lower
/// user id. Keep-best uploads replace an entry only with a strictly better score.
#[derive(Debug)]
pub struct LocalLeaderboards {
    state: Mutex<LocalState>,
}

impl Default for LocalLeaderboards {
    fn default() -> Self {
        Self::new(1)
    }
}

impl LocalLeaderboards {
    /// A service whose local user is `local_user`. UGC attachments are supported.
    pub fn new(local_user: u64) -> Self {
        Self {
            state: Mutex::new(LocalState {
                local_user,
                ugc_supported: true,
                ..LocalState::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LocalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes the local user (uploads and around-user ranges use it).
    pub fn set_local_user(&self, user: u64) {
        self.lock().local_user = user;
    }

    /// Sets the local user's friends (the `Friends` range).
    pub fn set_friends(&self, friends: impl IntoIterator<Item = u64>) {
        self.lock().friends = friends.into_iter().collect();
    }

    /// Whether UGC attachments succeed (default) or fail with [`LeaderboardError::UgcUnsupported`].
    pub fn set_ugc_supported(&self, supported: bool) {
        self.lock().ugc_supported = supported;
    }

    /// Creates a board directly (as if it were made on the partner site). Existing boards are
    /// kept as they are.
    pub fn create_board(&self, name: &str, sort: LeaderboardSort, display: LeaderboardDisplay) {
        self.lock().create(name, sort, display);
    }

    /// Writes `user`'s score on board `name` (force update; the board is created descending and
    /// numeric when missing). For seeding other players in tests.
    pub fn seed_score(&self, name: &str, user: u64, score: i32, details: &[i32]) {
        let mut state = self.lock();
        let index = state.create(name, LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
        state.next_seq += 1;
        let seq = state.next_seq;
        state.boards[index].write(user, UploadMethod::ForceUpdate, score, details, seq);
    }

    /// Makes the next accepted request answer with `error` instead of its result (a platform
    /// failure, delivered asynchronously like the real one).
    pub fn fail_next(&self, error: LeaderboardError) {
        self.lock().fail_next = Some(error);
    }

    /// Every board name, in creation order.
    pub fn board_names(&self) -> Vec<String> {
        self.lock().boards.iter().map(|b| b.name.clone()).collect()
    }

    /// Every row of board `name` in rank order (empty when the board is missing).
    pub fn rows(&self, name: &str) -> Vec<LeaderboardEntry> {
        let state = self.lock();
        state
            .by_name(name)
            .map(|i| {
                let board = &state.boards[i];
                (0..board.entries.len()).map(|r| board.row(r)).collect()
            })
            .unwrap_or_default()
    }

    /// How many requests the service accepted (finds, uploads, downloads, attachments).
    pub fn requests(&self) -> u64 {
        self.lock().requests
    }

    /// Queues `event` for the next poll, replacing its result with the injected failure if any.
    fn answer(state: &mut LocalState, event: ServiceEvent) {
        let event = match state.fail_next.take() {
            None => event,
            Some(error) => match event {
                ServiceEvent::Found { request, name, .. } => ServiceEvent::Found {
                    request,
                    name,
                    result: Err(error),
                },
                ServiceEvent::Uploaded { request, board, .. } => ServiceEvent::Uploaded {
                    request,
                    board,
                    result: Err(error),
                },
                ServiceEvent::Downloaded {
                    request,
                    board,
                    range,
                    ..
                } => ServiceEvent::Downloaded {
                    request,
                    board,
                    range,
                    result: Err(error),
                },
                ServiceEvent::UgcAttached {
                    request, board, ugc, ..
                } => ServiceEvent::UgcAttached {
                    request,
                    board,
                    ugc,
                    result: Err(error),
                },
            },
        };
        state.events.push(event);
    }
}

impl LeaderboardService for LocalLeaderboards {
    fn name(&self) -> &'static str {
        "local"
    }

    fn local_user(&self) -> Option<u64> {
        Some(self.lock().local_user)
    }

    fn find(&self, spec: &LeaderboardSpec) -> Result<RequestId, LeaderboardError> {
        check_name(&spec.name)?;
        let mut state = self.lock();
        let request = state.request();
        // A failure injected for this request must not create the board.
        let index = match state.by_name(&spec.name) {
            Some(index) => Some(index),
            None if spec.create && state.fail_next.is_none() => {
                Some(state.create(&spec.name, spec.sort, spec.display))
            }
            None => None,
        };
        let result = Ok(index.map(|i| state.boards[i].info(LocalState::handle(i))));
        Self::answer(
            &mut state,
            ServiceEvent::Found {
                request,
                name: spec.name.clone(),
                result,
            },
        );
        Ok(request)
    }

    fn upload(
        &self,
        board: LeaderboardHandle,
        method: UploadMethod,
        score: i32,
        details: &[i32],
    ) -> Result<RequestId, LeaderboardError> {
        check_details(details)?;
        let mut state = self.lock();
        let index = state.board_index(board)?;
        let request = state.request();
        let result = if state.fail_next.is_some() {
            // The failure replaces the result; the board is left as it was.
            Err(LeaderboardError::Unavailable)
        } else {
            state.next_seq += 1;
            let (seq, user) = (state.next_seq, state.local_user);
            Ok(state.boards[index].write(user, method, score, details, seq))
        };
        Self::answer(
            &mut state,
            ServiceEvent::Uploaded {
                request,
                board,
                result,
            },
        );
        Ok(request)
    }

    fn download(
        &self,
        board: LeaderboardHandle,
        range: LeaderboardRange,
    ) -> Result<RequestId, LeaderboardError> {
        range.check()?;
        let mut state = self.lock();
        let index = state.board_index(board)?;
        let request = state.request();
        let rows = state.boards[index].rows(range, state.local_user, &state.friends);
        Self::answer(
            &mut state,
            ServiceEvent::Downloaded {
                request,
                board,
                range,
                result: Ok(rows),
            },
        );
        Ok(request)
    }

    fn attach_ugc(&self, board: LeaderboardHandle, ugc: UgcHandle) -> Result<RequestId, LeaderboardError> {
        let mut state = self.lock();
        let index = state.board_index(board)?;
        let request = state.request();
        let user = state.local_user;
        let result = if !state.ugc_supported {
            Err(LeaderboardError::UgcUnsupported)
        } else if state.fail_next.is_some() {
            Err(LeaderboardError::Unavailable)
        } else {
            match state.boards[index].position(user) {
                Some(at) => {
                    state.boards[index].entries[at].ugc = Some(ugc);
                    Ok(())
                }
                None => Err(LeaderboardError::NoEntry),
            }
        };
        Self::answer(
            &mut state,
            ServiceEvent::UgcAttached {
                request,
                board,
                ugc,
                result,
            },
        );
        Ok(request)
    }

    fn poll(&self) -> Vec<ServiceEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

/// Where a board's lookup stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lookup {
    /// Not asked yet (or the service changed).
    #[default]
    NotStarted,
    /// The find is in flight.
    Pending,
    /// Found: [`BoardState::info`] is set.
    Found,
    /// The service has no such board (and the spec does not create it).
    Missing,
    /// The find failed ([`BoardState::last_error`]); the next operation retries it.
    Failed,
}

impl Lookup {
    /// `not_started`, `pending`, `found`, `missing` or `failed`.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Pending => "pending",
            Self::Found => "found",
            Self::Missing => "missing",
            Self::Failed => "failed",
        }
    }
}

/// Everything the [`Leaderboard`] resource knows about one board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardState {
    /// The declared spec.
    pub spec: LeaderboardSpec,
    /// Where the lookup stands.
    pub lookup: Lookup,
    /// The board as found.
    pub info: Option<BoardInfo>,
    /// The last upload's outcome.
    pub last_upload: Option<UploadOutcome>,
    /// The last download: its range and rows.
    pub last_entries: Option<(LeaderboardRange, Vec<LeaderboardEntry>)>,
    /// The last UGC handle attached.
    pub attached_ugc: Option<UgcHandle>,
    /// The last failure on this board.
    pub last_error: Option<LeaderboardError>,
    /// Operations queued or in flight.
    pub pending: u32,
}

impl BoardState {
    fn new(spec: LeaderboardSpec) -> Self {
        Self {
            spec,
            lookup: Lookup::NotStarted,
            info: None,
            last_upload: None,
            last_entries: None,
            attached_ugc: None,
            last_error: None,
            pending: 0,
        }
    }
}

/// One queued operation on a board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Upload {
        score: i32,
        details: Vec<i32>,
        method: UploadMethod,
    },
    Download(LeaderboardRange),
    AttachUgc(UgcHandle),
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Self::Upload { .. } => "upload",
            Self::Download(_) => "download",
            Self::AttachUgc(_) => "attach_ugc",
        }
    }
}

/// What a request in flight was for.
#[derive(Debug, Clone)]
enum InFlight {
    Find(String),
    Op(String, Op),
}

/// What happened to a board, written once per answer by [`leaderboard_system`].
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub enum LeaderboardMessage {
    /// The board was found (or created).
    Found {
        /// The board name.
        board: String,
        /// The board as found.
        info: BoardInfo,
    },
    /// The service has no such board; its queued operations failed with `leaderboard_not_found`.
    Missing {
        /// The board name.
        board: String,
    },
    /// An upload finished.
    Uploaded {
        /// The board name.
        board: String,
        /// What it did.
        outcome: UploadOutcome,
    },
    /// A download finished; the rows are in [`BoardState::last_entries`].
    Entries {
        /// The board name.
        board: String,
        /// The range.
        range: LeaderboardRange,
        /// How many rows came back.
        rows: usize,
    },
    /// A UGC handle was attached to the local user's entry.
    UgcAttached {
        /// The board name.
        board: String,
        /// The handle.
        ugc: UgcHandle,
    },
    /// An operation failed.
    Failed {
        /// The board name.
        board: String,
        /// `find`, `upload`, `download` or `attach_ugc`.
        op: &'static str,
        /// Why.
        error: LeaderboardError,
    },
}

/// The game-facing leaderboard API. Insert it with [`LeaderboardPlugin`]; read it as
/// `Res<Leaderboard>`, change it as `ResMut<Leaderboard>`.
///
/// Calls validate their arguments and queue; nothing reaches the service until
/// [`leaderboard_system`] runs. Boards are found once by name (a board used before it is declared
/// gets [`LeaderboardSpec::new`]'s defaults).
#[derive(Resource)]
pub struct Leaderboard {
    service: Arc<dyn LeaderboardService>,
    boards: BTreeMap<String, BoardState>,
    queued: Vec<(String, Op)>,
    in_flight: BTreeMap<RequestId, InFlight>,
}

impl std::fmt::Debug for Leaderboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leaderboard")
            .field("service", &self.service.name())
            .field("boards", &self.boards)
            .field("queued", &self.queued.len())
            .field("in_flight", &self.in_flight.len())
            .finish()
    }
}

impl Default for Leaderboard {
    fn default() -> Self {
        Self::new(Arc::new(UnavailableLeaderboards))
    }
}

impl Leaderboard {
    /// A leaderboard over `service`.
    pub fn new(service: Arc<dyn LeaderboardService>) -> Self {
        Self {
            service,
            boards: BTreeMap::new(),
            queued: Vec::new(),
            in_flight: BTreeMap::new(),
        }
    }

    /// A leaderboard over a fresh [`LocalLeaderboards`] (local user 1).
    pub fn local() -> Self {
        Self::new(Arc::new(LocalLeaderboards::default()))
    }

    /// The service's name: `none`, `local` or `steam`.
    pub fn service_name(&self) -> &'static str {
        self.service.name()
    }

    /// The local user's platform id, when the service knows it.
    pub fn local_user(&self) -> Option<u64> {
        self.service.local_user()
    }

    /// Switches to another service. Every board is looked up again; operations in flight on the
    /// old service are queued again for the new one (their answers would never come).
    pub fn set_service(&mut self, service: Arc<dyn LeaderboardService>) {
        self.service = service;
        let in_flight = std::mem::take(&mut self.in_flight);
        let mut requeued: Vec<(String, Op)> = in_flight
            .into_values()
            .filter_map(|f| match f {
                InFlight::Op(board, op) => Some((board, op)),
                InFlight::Find(_) => None,
            })
            .collect();
        requeued.append(&mut self.queued);
        self.queued = requeued;
        for state in self.boards.values_mut() {
            state.lookup = Lookup::NotStarted;
            state.info = None;
        }
    }

    /// Declares (or re-declares) a board. A changed spec is looked up again.
    pub fn declare(&mut self, spec: LeaderboardSpec) -> Result<(), LeaderboardError> {
        check_name(&spec.name)?;
        match self.boards.get_mut(&spec.name) {
            Some(state) if state.spec == spec => {}
            Some(state) => {
                state.spec = spec;
                if state.lookup != Lookup::Pending {
                    state.lookup = Lookup::NotStarted;
                    state.info = None;
                }
            }
            None => {
                self.boards.insert(spec.name.clone(), BoardState::new(spec));
            }
        }
        Ok(())
    }

    fn queue(&mut self, name: &str, op: Op) -> Result<(), LeaderboardError> {
        check_name(name)?;
        let state = self
            .boards
            .entry(name.to_owned())
            .or_insert_with(|| BoardState::new(LeaderboardSpec::new(name)));
        state.pending = state.pending.saturating_add(1);
        // A board found missing or whose find failed is retried by new work.
        if matches!(state.lookup, Lookup::Missing | Lookup::Failed) {
            state.lookup = Lookup::NotStarted;
        }
        self.queued.push((name.to_owned(), op));
        Ok(())
    }

    /// Uploads the local user's `score` with the board's declared method.
    pub fn submit(&mut self, board: &str, score: i32, details: &[i32]) -> Result<(), LeaderboardError> {
        let method = self
            .boards
            .get(board)
            .map_or(UploadMethod::KeepBest, |s| s.spec.method);
        self.submit_with(board, score, details, method)
    }

    /// Uploads the local user's `score` with an explicit method.
    pub fn submit_with(
        &mut self,
        board: &str,
        score: i32,
        details: &[i32],
        method: UploadMethod,
    ) -> Result<(), LeaderboardError> {
        check_details(details)?;
        self.queue(
            board,
            Op::Upload {
                score,
                details: details.to_vec(),
                method,
            },
        )
    }

    /// Downloads `range`; the rows land in [`BoardState::last_entries`].
    pub fn request(&mut self, board: &str, range: LeaderboardRange) -> Result<(), LeaderboardError> {
        range.check()?;
        self.queue(board, Op::Download(range))
    }

    /// Attaches a UGC file (a shared cloud file) to the local user's entry.
    pub fn attach_ugc(&mut self, board: &str, ugc: UgcHandle) -> Result<(), LeaderboardError> {
        self.queue(board, Op::AttachUgc(ugc))
    }

    /// Asks the service for a declared board now (it is otherwise found on first use).
    pub fn find(&mut self, board: &str) -> Result<(), LeaderboardError> {
        check_name(board)?;
        let state = self
            .boards
            .entry(board.to_owned())
            .or_insert_with(|| BoardState::new(LeaderboardSpec::new(board)));
        if state.lookup != Lookup::Pending {
            state.lookup = Lookup::NotStarted;
        }
        Ok(())
    }

    /// One board's state.
    pub fn board(&self, name: &str) -> Option<&BoardState> {
        self.boards.get(name)
    }

    /// Every board, by name.
    pub fn boards(&self) -> impl Iterator<Item = (&str, &BoardState)> {
        self.boards.iter().map(|(name, state)| (name.as_str(), state))
    }

    /// The rows of the board's last download (empty before one arrives).
    pub fn entries(&self, board: &str) -> &[LeaderboardEntry] {
        self.boards
            .get(board)
            .and_then(|s| s.last_entries.as_ref())
            .map_or(&[], |(_, rows)| rows.as_slice())
    }

    /// Requests queued or in flight across all boards.
    pub fn pending(&self) -> usize {
        self.queued.len() + self.in_flight.len()
    }

    /// Runs one step: starts lookups, sends queued work for found boards, drains the service's
    /// answers and sends again for boards those answers found. [`leaderboard_system`] calls it
    /// once per frame; tests and tools may call it directly.
    pub fn update(&mut self) -> Vec<LeaderboardMessage> {
        let mut out = Vec::new();
        self.send(&mut out);
        for event in self.service.poll() {
            self.answer(event, &mut out);
        }
        self.send(&mut out);
        out
    }

    fn fail_board(
        &mut self,
        board: &str,
        op: &'static str,
        error: LeaderboardError,
        out: &mut Vec<LeaderboardMessage>,
    ) {
        if let Some(state) = self.boards.get_mut(board) {
            state.last_error = Some(error.clone());
        }
        out.push(LeaderboardMessage::Failed {
            board: board.to_owned(),
            op,
            error,
        });
    }

    fn done(&mut self, board: &str) {
        if let Some(state) = self.boards.get_mut(board) {
            state.pending = state.pending.saturating_sub(1);
        }
    }

    /// Starts every lookup a queued operation (or [`Leaderboard::find`]) waits for and sends the
    /// operations of found boards, in queue order.
    fn send(&mut self, out: &mut Vec<LeaderboardMessage>) {
        let finds: Vec<String> = self
            .boards
            .iter()
            .filter(|(_, s)| s.lookup == Lookup::NotStarted)
            .map(|(name, _)| name.clone())
            .collect();
        for name in finds {
            let Some(spec) = self.boards.get(&name).map(|s| s.spec.clone()) else {
                continue;
            };
            match self.service.find(&spec) {
                Ok(request) => {
                    if let Some(state) = self.boards.get_mut(&name) {
                        state.lookup = Lookup::Pending;
                    }
                    self.in_flight.insert(request, InFlight::Find(name));
                }
                Err(error) => {
                    if let Some(state) = self.boards.get_mut(&name) {
                        state.lookup = Lookup::Failed;
                    }
                    self.fail_board(&name, "find", error.clone(), out);
                    self.drop_queued(&name, &error, out);
                }
            }
        }
        let queued = std::mem::take(&mut self.queued);
        for (board, op) in queued {
            let (lookup, handle) = match self.boards.get(&board) {
                Some(s) => (s.lookup, s.info.as_ref().map(|i| i.handle)),
                None => (Lookup::Missing, None),
            };
            match (lookup, handle) {
                (Lookup::Found, Some(handle)) => {
                    let sent = match &op {
                        Op::Upload {
                            score,
                            details,
                            method,
                        } => self.service.upload(handle, *method, *score, details),
                        Op::Download(range) => self.service.download(handle, *range),
                        Op::AttachUgc(ugc) => self.service.attach_ugc(handle, *ugc),
                    };
                    match sent {
                        Ok(request) => {
                            self.in_flight.insert(request, InFlight::Op(board, op));
                        }
                        Err(error) => {
                            self.done(&board);
                            self.fail_board(&board, op.name(), error, out);
                        }
                    }
                }
                (Lookup::NotStarted | Lookup::Pending, _) => self.queued.push((board, op)),
                _ => {
                    // Missing or failed: the lookup's answer already failed this board's work.
                    self.done(&board);
                    let error = LeaderboardError::NotFound(board.clone());
                    self.fail_board(&board, op.name(), error, out);
                }
            }
        }
    }

    /// Fails every queued operation of `board` with `error`.
    fn drop_queued(&mut self, board: &str, error: &LeaderboardError, out: &mut Vec<LeaderboardMessage>) {
        let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued)
            .into_iter()
            .partition(|(b, _)| b == board);
        self.queued = kept;
        for (_, op) in dropped {
            self.done(board);
            self.fail_board(board, op.name(), error.clone(), out);
        }
    }

    fn answer(&mut self, event: ServiceEvent, out: &mut Vec<LeaderboardMessage>) {
        let Some(flight) = self.in_flight.remove(&event.request()) else {
            // An answer from before a service change, or one we never asked for.
            return;
        };
        match (flight, event) {
            (InFlight::Find(board), ServiceEvent::Found { result, .. }) => match result {
                Ok(Some(info)) => {
                    if let Some(state) = self.boards.get_mut(&board) {
                        state.lookup = Lookup::Found;
                        state.info = Some(info.clone());
                    }
                    out.push(LeaderboardMessage::Found { board, info });
                }
                Ok(None) => {
                    if let Some(state) = self.boards.get_mut(&board) {
                        state.lookup = Lookup::Missing;
                        state.info = None;
                    }
                    out.push(LeaderboardMessage::Missing { board: board.clone() });
                    self.drop_queued(&board, &LeaderboardError::NotFound(board.clone()), out);
                }
                Err(error) => {
                    if let Some(state) = self.boards.get_mut(&board) {
                        state.lookup = Lookup::Failed;
                    }
                    self.fail_board(&board, "find", error.clone(), out);
                    self.drop_queued(&board, &error, out);
                }
            },
            (InFlight::Op(board, _), ServiceEvent::Uploaded { result, .. }) => {
                self.done(&board);
                match result {
                    Ok(outcome) => {
                        if let Some(state) = self.boards.get_mut(&board) {
                            state.last_upload = Some(outcome);
                            if let Some(info) = state.info.as_mut()
                                && outcome.rank_previous.is_none()
                                && outcome.changed
                            {
                                info.entry_count = info.entry_count.saturating_add(1);
                            }
                        }
                        out.push(LeaderboardMessage::Uploaded { board, outcome });
                    }
                    Err(error) => self.fail_board(&board, "upload", error, out),
                }
            }
            (InFlight::Op(board, _), ServiceEvent::Downloaded { range, result, .. }) => {
                self.done(&board);
                match result {
                    Ok(rows) => {
                        let count = rows.len();
                        if let Some(state) = self.boards.get_mut(&board) {
                            state.last_entries = Some((range, rows));
                        }
                        out.push(LeaderboardMessage::Entries {
                            board,
                            range,
                            rows: count,
                        });
                    }
                    Err(error) => self.fail_board(&board, "download", error, out),
                }
            }
            (InFlight::Op(board, _), ServiceEvent::UgcAttached { ugc, result, .. }) => {
                self.done(&board);
                match result {
                    Ok(()) => {
                        if let Some(state) = self.boards.get_mut(&board) {
                            state.attached_ugc = Some(ugc);
                        }
                        out.push(LeaderboardMessage::UgcAttached { board, ugc });
                    }
                    Err(error) => self.fail_board(&board, "attach_ugc", error, out),
                }
            }
            (flight, event) => {
                tracing::warn!(
                    target: "regia_framework::leaderboard",
                    "leaderboard answer {event:?} does not match its request {flight:?}"
                );
                if let InFlight::Op(board, _) = flight {
                    self.done(&board);
                }
            }
        }
    }
}

/// `PreUpdate`: runs [`Leaderboard::update`] and writes its [`LeaderboardMessage`]s.
pub fn leaderboard_system(mut boards: ResMut<Leaderboard>, mut messages: MessageWriter<LeaderboardMessage>) {
    let _span = tracing::debug_span!("leaderboard_system").entered();
    if boards.pending() == 0 && !boards.boards.values().any(|s| s.lookup == Lookup::NotStarted) {
        return;
    }
    for message in boards.update() {
        messages.write(message);
    }
}

/// Adds the [`Leaderboard`] resource (unless the app inserted one), [`LeaderboardMessage`] and
/// [`leaderboard_system`] in `PreUpdate`.
///
/// The default service is [`UnavailableLeaderboards`]; [`LeaderboardPlugin::local`] uses an
/// in-memory [`LocalLeaderboards`] instead, and `regia_steam::SteamLeaderboardPlugin` attaches
/// Steam once it is ready.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeaderboardPlugin {
    /// Start on an in-memory [`LocalLeaderboards`] instead of no service.
    pub local: bool,
}

impl LeaderboardPlugin {
    /// Starts on an in-memory [`LocalLeaderboards`] (offline play, dev).
    pub fn local() -> Self {
        Self { local: true }
    }
}

impl Plugin for LeaderboardPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<Leaderboard>() {
            let boards = if self.local {
                Leaderboard::local()
            } else {
                Leaderboard::default()
            };
            app.insert_resource(boards);
        }
        app.add_message::<LeaderboardMessage>();
        app.add_systems(PreUpdate, leaderboard_system);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(boards: &mut Leaderboard) -> Vec<LeaderboardMessage> {
        // One update sends the find, the next drains it and sends the queued work, a third drains
        // that work.
        let mut out = boards.update();
        out.extend(boards.update());
        out
    }

    #[test]
    fn keep_best_keeps_the_better_score_and_force_replaces() {
        let service = Arc::new(LocalLeaderboards::new(7));
        let mut boards = Leaderboard::new(service.clone());
        boards
            .declare(LeaderboardSpec::new("points").create_if_missing())
            .expect("declare");
        boards.submit("points", 100, &[1, 2]).expect("submit");
        let out = found(&mut boards);
        assert!(matches!(&out[0], LeaderboardMessage::Found { board, .. } if board == "points"));
        let first = boards
            .board("points")
            .and_then(|s| s.last_upload)
            .expect("uploaded");
        assert_eq!(
            first,
            UploadOutcome {
                score: 100,
                changed: true,
                rank_new: 1,
                rank_previous: None
            }
        );

        boards.submit("points", 50, &[]).expect("worse");
        boards.update();
        let worse = boards
            .board("points")
            .and_then(|s| s.last_upload)
            .expect("uploaded");
        assert!(!worse.changed);
        assert_eq!(service.rows("points")[0].score, 100);
        assert_eq!(service.rows("points")[0].details, vec![1, 2]);

        boards
            .submit_with("points", 50, &[], UploadMethod::ForceUpdate)
            .expect("force");
        boards.update();
        let forced = boards
            .board("points")
            .and_then(|s| s.last_upload)
            .expect("uploaded");
        assert!(forced.changed);
        assert_eq!(forced.rank_previous, Some(1));
        assert_eq!(service.rows("points")[0].score, 50);
    }

    #[test]
    fn ranges_cover_global_around_user_and_friends() {
        let service = Arc::new(LocalLeaderboards::new(5));
        service.create_board(
            "laps",
            LeaderboardSort::Ascending,
            LeaderboardDisplay::TimeMilliseconds,
        );
        for user in 1..=9_u64 {
            // Users 1..=9 with times 1000, 2000, ...: user 1 is fastest.
            service.seed_score("laps", user, i32::try_from(user).unwrap_or(0) * 1000, &[]);
        }
        service.set_friends([2, 8]);
        let mut boards = Leaderboard::new(service.clone());
        boards
            .declare(LeaderboardSpec::new("laps").ascending())
            .expect("declare");

        boards.request("laps", LeaderboardRange::top(3)).expect("top");
        found(&mut boards);
        let ranks: Vec<(u64, u32)> = boards.entries("laps").iter().map(|e| (e.user, e.rank)).collect();
        assert_eq!(ranks, vec![(1, 1), (2, 2), (3, 3)]);

        boards
            .request("laps", LeaderboardRange::AroundUser { before: 1, after: 2 })
            .expect("around");
        boards.update();
        let users: Vec<u64> = boards.entries("laps").iter().map(|e| e.user).collect();
        assert_eq!(users, vec![4, 5, 6, 7]);

        boards
            .request("laps", LeaderboardRange::Friends)
            .expect("friends");
        boards.update();
        let friends: Vec<(u64, u32)> = boards.entries("laps").iter().map(|e| (e.user, e.rank)).collect();
        assert_eq!(friends, vec![(2, 2), (5, 5), (8, 8)]);

        // Past the end: clamped, not an error.
        boards
            .request("laps", LeaderboardRange::Global { start: 8, end: 20 })
            .expect("tail");
        boards.update();
        assert_eq!(boards.entries("laps").len(), 2);
    }

    #[test]
    fn ties_rank_the_earlier_score_first() {
        let service = Arc::new(LocalLeaderboards::new(1));
        service.seed_score("pts", 30, 10, &[]);
        service.seed_score("pts", 20, 10, &[]);
        service.seed_score("pts", 10, 11, &[]);
        let users: Vec<u64> = service.rows("pts").iter().map(|e| e.user).collect();
        assert_eq!(users, vec![10, 30, 20]);
    }

    #[test]
    fn a_missing_board_fails_its_queued_work_and_a_retry_finds_it_later() {
        let service = Arc::new(LocalLeaderboards::new(1));
        let mut boards = Leaderboard::new(service.clone());
        boards.submit("nope", 1, &[]).expect("queued");
        let out = found(&mut boards);
        assert!(out.contains(&LeaderboardMessage::Missing { board: "nope".into() }));
        assert!(out.iter().any(|m| matches!(
            m,
            LeaderboardMessage::Failed {
                op: "upload",
                error: LeaderboardError::NotFound(_),
                ..
            }
        )));
        assert_eq!(boards.board("nope").map(|s| s.pending), Some(0));
        assert_eq!(boards.pending(), 0);

        service.create_board("nope", LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
        boards.submit("nope", 3, &[]).expect("queued again");
        found(&mut boards);
        assert_eq!(boards.board("nope").map(|s| s.lookup), Some(Lookup::Found));
        assert_eq!(service.rows("nope").len(), 1);
    }

    #[test]
    fn validation_rejects_bad_names_details_and_ranges_at_once() {
        let mut boards = Leaderboard::local();
        assert_eq!(boards.submit("", 1, &[]), Err(LeaderboardError::NameInvalid));
        assert_eq!(boards.submit("a\0b", 1, &[]), Err(LeaderboardError::NameInvalid));
        assert_eq!(
            boards.submit(&"x".repeat(MAX_NAME_BYTES + 1), 1, &[]),
            Err(LeaderboardError::NameInvalid)
        );
        assert_eq!(
            boards.submit("ok", 1, &[0; MAX_DETAILS + 1]),
            Err(LeaderboardError::DetailsTooLong(MAX_DETAILS + 1))
        );
        assert!(matches!(
            boards.request("ok", LeaderboardRange::Global { start: 0, end: 3 }),
            Err(LeaderboardError::RangeInvalid(_))
        ));
        assert!(matches!(
            boards.request(
                "ok",
                LeaderboardRange::Global {
                    start: 1,
                    end: MAX_DOWNLOAD_ROWS + 1
                }
            ),
            Err(LeaderboardError::RangeInvalid(_))
        ));
        assert!(
            boards
                .request(
                    "ok",
                    LeaderboardRange::Global {
                        start: 1,
                        end: MAX_DOWNLOAD_ROWS
                    }
                )
                .is_ok()
        );
        assert_eq!(boards.pending(), 1);
    }

    #[test]
    fn unavailable_service_fails_every_operation() {
        let mut boards = Leaderboard::default();
        assert_eq!(boards.service_name(), "none");
        boards.submit("points", 1, &[]).expect("queued");
        let out = boards.update();
        assert_eq!(
            out,
            vec![
                LeaderboardMessage::Failed {
                    board: "points".into(),
                    op: "find",
                    error: LeaderboardError::Unavailable
                },
                LeaderboardMessage::Failed {
                    board: "points".into(),
                    op: "upload",
                    error: LeaderboardError::Unavailable
                },
            ]
        );
        assert_eq!(boards.pending(), 0);
        assert_eq!(boards.board("points").map(|s| s.lookup), Some(Lookup::Failed));
    }

    #[test]
    fn ugc_attaches_to_an_existing_entry_only() {
        let service = Arc::new(LocalLeaderboards::new(4));
        service.create_board(
            "replays",
            LeaderboardSort::Descending,
            LeaderboardDisplay::Numeric,
        );
        let mut boards = Leaderboard::new(service.clone());
        boards.attach_ugc("replays", UgcHandle(99)).expect("queued");
        let out = found(&mut boards);
        assert!(out.iter().any(|m| matches!(
            m,
            LeaderboardMessage::Failed {
                op: "attach_ugc",
                error: LeaderboardError::NoEntry,
                ..
            }
        )));
        boards.submit("replays", 10, &[]).expect("submit");
        boards.attach_ugc("replays", UgcHandle(99)).expect("queued");
        boards.update();
        boards.update();
        assert_eq!(
            boards.board("replays").and_then(|s| s.attached_ugc),
            Some(UgcHandle(99))
        );
        assert_eq!(service.rows("replays")[0].ugc, Some(UgcHandle(99)));
    }

    #[test]
    fn injected_failure_reaches_the_board_asynchronously() {
        let service = Arc::new(LocalLeaderboards::new(1));
        service.create_board("pts", LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
        let mut boards = Leaderboard::new(service.clone());
        boards.find("pts").expect("find");
        boards.update();
        service.fail_next(LeaderboardError::Platform("rate limited".into()));
        boards.submit("pts", 5, &[]).expect("queued");
        let sent = boards.update();
        // Sent this frame; the answer is drained by the same update's poll only if it was sent
        // before the poll, which it was.
        let failed: Vec<_> = sent
            .iter()
            .chain(boards.update().iter())
            .filter(|m| matches!(m, LeaderboardMessage::Failed { op: "upload", .. }))
            .cloned()
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(service.rows("pts").is_empty(), "a failed upload must not write");
        assert_eq!(
            boards.board("pts").and_then(|s| s.last_error.clone()),
            Some(LeaderboardError::Platform("rate limited".into()))
        );
    }

    #[test]
    fn swapping_the_service_requeues_work_in_flight() {
        let first = Arc::new(LocalLeaderboards::new(1));
        first.create_board("pts", LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
        let mut boards = Leaderboard::new(first.clone());
        boards.find("pts").expect("find");
        boards.update(); // found and polled
        boards.submit("pts", 9, &[]).expect("queued");
        // Send without polling: the upload is in flight on `first`.
        let mut out = Vec::new();
        boards.send(&mut out);
        assert_eq!(boards.in_flight.len(), 1);

        let second = Arc::new(LocalLeaderboards::new(1));
        second.create_board("pts", LeaderboardSort::Descending, LeaderboardDisplay::Numeric);
        boards.set_service(second.clone());
        assert_eq!(boards.board("pts").map(|s| s.lookup), Some(Lookup::NotStarted));
        boards.update();
        boards.update();
        assert_eq!(second.rows("pts").len(), 1);
        assert_eq!(boards.pending(), 0);
    }
}
