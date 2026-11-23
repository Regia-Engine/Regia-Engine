// SPDX-License-Identifier: MPL-2.0
//! Steam leaderboards over the `steamworks` 0.13 bindings (ISteamUserStats; spec 24 § 2a,
//! ADR-2340). Compiled only with the `steamworks` feature on desktop.
//!
//! Every request registers a Steam call result; the callback runs inside `run_callbacks` (the
//! plugin's frame pump) and queues its answer, which [`LeaderboardService::poll`] hands to the
//! framework's `Leaderboard` resource later in the same `PreUpdate`. Board handles are kept by
//! their raw id, because the bindings do not let a handle be rebuilt from one.
//!
//! Known limits of the bindings (ADR-2340): attaching UGC to an entry (`AttachLeaderboardUGC`) is
//! not exposed, so [`LeaderboardService::attach_ugc`] answers `ugc_unsupported` and downloaded
//! rows carry no UGC handle; the around-user range's negative start is passed through the
//! bindings' `usize` parameter as its two's-complement bit pattern, which their `as i32` cast
//! turns back into the negative offset Steam expects.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_framework::leaderboard::{
    BoardInfo, LeaderboardDisplay, LeaderboardEntry, LeaderboardError, LeaderboardHandle, LeaderboardRange,
    LeaderboardService, LeaderboardSort, LeaderboardSpec, MAX_DETAILS, RequestId, ServiceEvent, UgcHandle,
    UploadMethod, UploadOutcome, check_details, check_name,
};

/// An answer as the callback saw it: finds still need the board's details, which only the
/// client can read (the callbacks hold no client).
enum Raw {
    Found {
        request: RequestId,
        name: String,
        result: Result<Option<u64>, LeaderboardError>,
    },
    Ready(ServiceEvent),
}

/// State shared with the call-result callbacks.
#[derive(Default)]
struct Shared {
    answers: Mutex<Vec<Raw>>,
    handles: Mutex<BTreeMap<u64, steamworks::Leaderboard>>,
}

impl Shared {
    fn push(&self, raw: Raw) {
        self.answers.lock().unwrap_or_else(|e| e.into_inner()).push(raw);
    }
}

fn platform(err: steamworks::SteamError) -> LeaderboardError {
    LeaderboardError::Platform(err.to_string())
}

/// The `start` and `end` arguments the bindings' `download_leaderboard_entries` takes for `range`.
/// The bindings cast both `as i32` (`int` in the SDK). Global ranks above `i32::MAX` are clamped,
/// because they would wrap negative. Steam takes `-before..=after` around the user; the start's
/// two's-complement pattern arrives as `-before` after the cast. Friends ignore both.
fn steam_window(range: LeaderboardRange) -> (usize, usize) {
    let rank = |r: u32| usize::try_from(r.min(i32::MAX.unsigned_abs())).unwrap_or(0);
    match range {
        LeaderboardRange::Global { start, end } => (rank(start), rank(end)),
        LeaderboardRange::AroundUser { before, after } => (rank(before).wrapping_neg(), rank(after)),
        LeaderboardRange::Friends => (0, 0),
    }
}

/// The Steam leaderboard service of one connection.
pub(crate) struct RealLeaderboards {
    client: steamworks::Client,
    shared: Arc<Shared>,
    next: AtomicU64,
}

impl RealLeaderboards {
    pub(crate) fn new(client: steamworks::Client) -> Self {
        Self {
            client,
            shared: Arc::default(),
            next: AtomicU64::new(1),
        }
    }

    fn request(&self) -> RequestId {
        RequestId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn handle(&self, board: LeaderboardHandle) -> Result<steamworks::Leaderboard, LeaderboardError> {
        self.shared
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&board.0)
            .cloned()
            .ok_or_else(|| LeaderboardError::NotFound(format!("handle {}", board.0)))
    }

    /// The details of a found board.
    fn info(&self, raw: u64) -> Option<BoardInfo> {
        let board = self
            .shared
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&raw)
            .cloned()?;
        let stats = self.client.user_stats();
        let sort = match stats.get_leaderboard_sort_method(&board) {
            Some(steamworks::LeaderboardSortMethod::Ascending) => LeaderboardSort::Ascending,
            _ => LeaderboardSort::Descending,
        };
        let display = match stats.get_leaderboard_display_type(&board) {
            Some(steamworks::LeaderboardDisplayType::TimeSeconds) => LeaderboardDisplay::TimeSeconds,
            Some(steamworks::LeaderboardDisplayType::TimeMilliSeconds) => {
                LeaderboardDisplay::TimeMilliseconds
            }
            _ => LeaderboardDisplay::Numeric,
        };
        Some(BoardInfo {
            handle: LeaderboardHandle(raw),
            name: stats.get_leaderboard_name(&board),
            sort,
            display,
            entry_count: u32::try_from(stats.get_leaderboard_entry_count(&board)).unwrap_or(0),
        })
    }
}

impl LeaderboardService for RealLeaderboards {
    fn name(&self) -> &'static str {
        "steam"
    }

    fn local_user(&self) -> Option<u64> {
        Some(self.client.user().steam_id().raw())
    }

    fn find(&self, spec: &LeaderboardSpec) -> Result<RequestId, LeaderboardError> {
        // The bindings abort on a NUL (CString unwrap); the engine ships panic=abort.
        check_name(&spec.name)?;
        let request = self.request();
        let shared = self.shared.clone();
        let name = spec.name.clone();
        let done = move |result: Result<Option<steamworks::Leaderboard>, steamworks::SteamError>| {
            let result = match result {
                Ok(Some(board)) => {
                    let raw = board.raw();
                    shared
                        .handles
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(raw, board);
                    Ok(Some(raw))
                }
                Ok(None) => Ok(None),
                Err(err) => Err(platform(err)),
            };
            shared.push(Raw::Found {
                request,
                name,
                result,
            });
        };
        let stats = self.client.user_stats();
        if spec.create {
            let sort = match spec.sort {
                LeaderboardSort::Ascending => steamworks::LeaderboardSortMethod::Ascending,
                LeaderboardSort::Descending => steamworks::LeaderboardSortMethod::Descending,
            };
            let display = match spec.display {
                LeaderboardDisplay::Numeric => steamworks::LeaderboardDisplayType::Numeric,
                LeaderboardDisplay::TimeSeconds => steamworks::LeaderboardDisplayType::TimeSeconds,
                LeaderboardDisplay::TimeMilliseconds => steamworks::LeaderboardDisplayType::TimeMilliSeconds,
            };
            stats.find_or_create_leaderboard(&spec.name, sort, display, done);
        } else {
            stats.find_leaderboard(&spec.name, done);
        }
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
        let handle = self.handle(board)?;
        let request = self.request();
        let shared = self.shared.clone();
        let method = match method {
            UploadMethod::KeepBest => steamworks::UploadScoreMethod::KeepBest,
            UploadMethod::ForceUpdate => steamworks::UploadScoreMethod::ForceUpdate,
        };
        self.client
            .user_stats()
            .upload_leaderboard_score(&handle, method, score, details, move |result| {
                let result = match result {
                    Ok(Some(up)) => Ok(UploadOutcome {
                        score: up.score,
                        changed: up.was_changed,
                        rank_new: u32::try_from(up.global_rank_new).unwrap_or(0),
                        rank_previous: u32::try_from(up.global_rank_previous).ok().filter(|&r| r > 0),
                    }),
                    Ok(None) => Err(LeaderboardError::Platform(
                        "Steam reported the upload failed".into(),
                    )),
                    Err(err) => Err(platform(err)),
                };
                shared.push(Raw::Ready(ServiceEvent::Uploaded {
                    request,
                    board,
                    result,
                }));
            });
        Ok(request)
    }

    fn download(
        &self,
        board: LeaderboardHandle,
        range: LeaderboardRange,
    ) -> Result<RequestId, LeaderboardError> {
        range.check()?;
        let handle = self.handle(board)?;
        let request = self.request();
        let shared = self.shared.clone();
        let (start, end) = steam_window(range);
        let kind = match range {
            LeaderboardRange::Global { .. } => steamworks::LeaderboardDataRequest::Global,
            LeaderboardRange::AroundUser { .. } => steamworks::LeaderboardDataRequest::GlobalAroundUser,
            LeaderboardRange::Friends => steamworks::LeaderboardDataRequest::Friends,
        };
        self.client.user_stats().download_leaderboard_entries(
            &handle,
            kind,
            start,
            end,
            MAX_DETAILS,
            move |result| {
                let result = result.map_err(platform).map(|rows| {
                    rows.into_iter()
                        .map(|row| LeaderboardEntry {
                            user: row.user.raw(),
                            rank: u32::try_from(row.global_rank).unwrap_or(0),
                            score: row.score,
                            details: row.details,
                            ugc: None,
                        })
                        .collect()
                });
                shared.push(Raw::Ready(ServiceEvent::Downloaded {
                    request,
                    board,
                    range,
                    result,
                }));
            },
        );
        Ok(request)
    }

    fn attach_ugc(&self, _board: LeaderboardHandle, _ugc: UgcHandle) -> Result<RequestId, LeaderboardError> {
        Err(LeaderboardError::UgcUnsupported)
    }

    fn poll(&self) -> Vec<ServiceEvent> {
        let answers = std::mem::take(&mut *self.shared.answers.lock().unwrap_or_else(|e| e.into_inner()));
        answers
            .into_iter()
            .map(|raw| match raw {
                Raw::Ready(event) => event,
                Raw::Found {
                    request,
                    name,
                    result,
                } => ServiceEvent::Found {
                    request,
                    name,
                    result: result.map(|found| found.and_then(|raw| self.info(raw))),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the SDK receives after the bindings' `as i32` casts.
    fn sdk(range: LeaderboardRange) -> (i32, i32) {
        let (start, end) = steam_window(range);
        // The bindings' own `as _` cast.
        (start as i32, end as i32)
    }

    #[test]
    fn around_user_start_reaches_the_sdk_negative() {
        assert_eq!(sdk(LeaderboardRange::AroundUser { before: 4, after: 5 }), (-4, 5));
        assert_eq!(sdk(LeaderboardRange::AroundUser { before: 0, after: 0 }), (0, 0));
        assert_eq!(
            sdk(LeaderboardRange::AroundUser {
                before: 999,
                after: 0
            }),
            (-999, 0)
        );
    }

    #[test]
    fn global_ranks_pass_through_and_never_wrap() {
        assert_eq!(sdk(LeaderboardRange::top(10)), (1, 10));
        assert_eq!(sdk(LeaderboardRange::Global { start: 41, end: 60 }), (41, 60));
        let far = LeaderboardRange::Global {
            start: u32::MAX - 5,
            end: u32::MAX,
        };
        assert_eq!(sdk(far), (i32::MAX, i32::MAX));
        assert_eq!(sdk(LeaderboardRange::Friends), (0, 0));
    }
}
