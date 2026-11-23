// SPDX-License-Identifier: MPL-2.0
//! `rules.match_results` and `rules.match_result`: the finished-match records under
//! [`MatchResultsSettings::root`] (`book/agents/match-results.md`).

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_framework::match_results::{
    MatchResults, MatchResultsSettings, date_days, find_result, list_results,
};
use serde_json::{Value, json};

use crate::params::{check_known_and_types, opt_str, opt_u64};
use crate::protocol::{self, RpcError};

/// Records one `rules.match_results` page holds at most.
pub const MAX_RESULTS_PAGE: usize = 200;
/// Records a page holds when `limit` is omitted.
pub const DEFAULT_RESULTS_PAGE: usize = 20;

fn unavailable(why: &str) -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "match_results_unavailable",
        why,
        "add regia_framework::match_results::MatchResultsPlugin and set MatchResultsSettings.root",
    )
}

fn root(world: &World) -> Result<std::path::PathBuf, RpcError> {
    world
        .get_resource::<MatchResultsSettings>()
        .ok_or_else(|| unavailable("no MatchResultsSettings resource in this app"))?
        .root
        .clone()
        .ok_or_else(|| unavailable("MatchResultsSettings.root is not set: no records are written"))
}

fn coded(code: &str, message: String, hint: &str) -> RpcError {
    let mut out = RpcError::new(protocol::INVALID_PARAMS, code, message, hint);
    if let Some(Value::Object(data)) = out.data.as_mut() {
        data.insert("reason".into(), json!(code));
    }
    out
}

/// `rules.match_results { since?, limit? }`: the newest records first, at most `limit` (default
/// [`DEFAULT_RESULTS_PAGE`], capped at [`MAX_RESULTS_PAGE`]), dated `since` (`YYYY-MM-DD`, UTC) or
/// later. A folder that does not exist yet lists nothing.
pub fn match_results(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_results", params, &[])?;
    let root = root(world)?;
    let since = match opt_str(params, "since")? {
        None => None,
        Some(text) => Some(date_days(text).ok_or_else(|| {
            coded(
                "match_results_bad_since",
                format!("`since` must be a UTC date YYYY-MM-DD, got {text:?}"),
                "pass a date folder name, e.g. \"2026-11-18\"",
            )
        })?),
    };
    let asked = opt_u64(params, "limit")?.unwrap_or(DEFAULT_RESULTS_PAGE as u64);
    if asked == 0 {
        return Err(RpcError::invalid_params("`limit` must be at least 1"));
    }
    let limit = usize::try_from(asked).unwrap_or(usize::MAX).min(MAX_RESULTS_PAGE);
    let page = if root.exists() {
        list_results(&root, since, limit)
            .map_err(|e| unavailable(&format!("cannot read {}: {e}", root.display())))?
    } else {
        regia_framework::match_results::ResultPage {
            results: Vec::new(),
            total: 0,
            skipped: 0,
        }
    };
    let state = world.get_resource::<MatchResults>();
    Ok(json!({
        "root": root.display().to_string(),
        "results": page.results,
        "total": page.total,
        "skipped": page.skipped,
        "limit": limit,
        "pending": state.map_or(0, MatchResults::pending),
        "written": state.map_or(0, |s| s.written),
        "failed": state.map_or(0, |s| s.failed),
        "dropped": state.map_or(0, |s| s.dropped),
    }))
}

/// `rules.match_result { match_id }`: one whole record and its date folder.
pub fn match_result(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_result", params, &[])?;
    let text =
        opt_str(params, "match_id")?.ok_or_else(|| RpcError::invalid_params("`match_id` is required"))?;
    let id: Guid = text.parse().map_err(|_| {
        coded(
            "match_result_bad_id",
            format!("`match_id` must be a GUID (32 hex digits, dashes optional), got {text:?}"),
            "rules.match_results lists the ids",
        )
    })?;
    let root = root(world)?;
    let (date, result) = find_result(&root, id).map_err(|e| {
        let code = e.split(':').next().unwrap_or("match_result_not_found").to_owned();
        let code = match code.as_str() {
            "match_result_not_found"
            | "match_result_malformed"
            | "match_result_newer"
            | "match_result_too_large" => code,
            _ => "match_result_read_failed".to_owned(),
        };
        let mut err = coded(
            &code,
            e.clone(),
            "rules.match_results lists the records that read",
        );
        if code == "match_result_not_found" {
            err.code = protocol::NOT_FOUND;
        }
        err
    })?;
    Ok(json!({ "date": date, "result": result }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::match_results::{MatchResult, write_result};

    fn world_with(root: &std::path::Path) -> World {
        let mut world = World::new();
        world.insert_resource(MatchResultsSettings {
            root: Some(root.to_path_buf()),
            ..MatchResultsSettings::default()
        });
        world.insert_resource(MatchResults::default());
        world
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn match_results_method_pages_newest_first_with_a_limit_cap() {
        let dir = regia_test_utils::TempDir::new("agent-match-results");
        for day in 1..=9u128 {
            let result = MatchResult {
                match_id: Guid::from_u128(day),
                ..MatchResult::example()
            };
            write_result(&dir, &format!("2026-11-{day:02}"), &result, 0, 0).expect("write");
        }
        let mut world = world_with(&dir);
        let page = match_results(&mut world, &json!({ "limit": 3 })).expect("page");
        let dates: Vec<&str> = page["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|r| r["date"].as_str())
            .collect();
        assert_eq!(dates, ["2026-11-09", "2026-11-08", "2026-11-07"], "newest first");
        assert_eq!(page["total"], 9);

        let since = match_results(&mut world, &json!({ "since": "2026-11-08" })).expect("since");
        assert_eq!(since["total"], 2);
        assert_eq!(since["results"].as_array().map(Vec::len), Some(2));

        let capped = match_results(&mut world, &json!({ "limit": 100_000 })).expect("cap");
        assert_eq!(capped["limit"], MAX_RESULTS_PAGE);

        let err = match_results(&mut world, &json!({ "since": "yesterday" })).expect_err("bad since");
        assert_eq!(reason(&err), "match_results_bad_since");
        assert!(match_results(&mut world, &json!({ "limit": 0 })).is_err());

        let one =
            match_result(&mut world, &json!({ "match_id": Guid::from_u128(4).to_string() })).expect("one");
        assert_eq!(one["date"], "2026-11-04");
        assert_eq!(one["result"]["map_id"], "harbor");
        let err = match_result(
            &mut world,
            &json!({ "match_id": Guid::from_u128(77).to_string() }),
        )
        .expect_err("absent");
        assert_eq!(reason(&err), "match_result_not_found");
        let err = match_result(&mut world, &json!({ "match_id": "nope" })).expect_err("bad id");
        assert_eq!(reason(&err), "match_result_bad_id");
    }

    #[test]
    fn without_a_root_the_methods_are_unavailable() {
        let mut world = World::new();
        let err = match_results(&mut world, &json!({})).expect_err("no plugin");
        assert_eq!(reason(&err), "match_results_unavailable");
        world.insert_resource(MatchResultsSettings::default());
        let err = match_result(&mut world, &json!({ "match_id": Guid::from_u128(1).to_string() }))
            .expect_err("no root");
        assert_eq!(reason(&err), "match_results_unavailable");
    }
}
