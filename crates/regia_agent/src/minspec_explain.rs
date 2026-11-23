// SPDX-License-Identifier: MPL-2.0
//! Agent explanation of the renderer's existing minimum-spec profile clamp.

use regia_render::MinspecProfile;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Return the feature and limit reasons for a minimum-spec profile.
pub(crate) fn explain(params: &Value) -> Result<Value, RpcError> {
    let profile_name = params
        .get("profile")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`profile` must be a string"))?;
    let profile = match profile_name {
        "pascal" => MinspecProfile::Pascal,
        "mobile" => MinspecProfile::Mobile,
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "minspec_profile_unknown",
                format!("unknown minimum-spec profile `{profile_name}`"),
                "use `pascal` or `mobile`",
            ));
        }
    };
    let explanation = profile.explain();
    Ok(json!({
        "profile": profile.name(),
        "features": explanation.features,
        "limits": explanation.limits,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_profile_has_a_stable_error_without_panicking() {
        let result = explain(&json!({ "profile": "unknown" }));
        let error = result.expect_err("unknown profile is rejected");
        assert_eq!(
            error.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("minspec_profile_unknown")
        );
    }

    #[test]
    fn known_profiles_return_the_projection_and_name() {
        for profile in [MinspecProfile::Pascal, MinspecProfile::Mobile] {
            let result = explain(&json!({ "profile": profile.name() })).expect("known profile");
            let expected = profile.explain();
            assert_eq!(result["profile"], profile.name());
            assert_eq!(result["features"], json!(expected.features));
            assert_eq!(result["limits"], json!(expected.limits));
        }
    }
}
