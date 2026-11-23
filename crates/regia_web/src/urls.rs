// SPDX-License-Identifier: MPL-2.0
//! Package URLs: a folder URL plus a file name from a manifest, with no way out of the folder.

use crate::WebError;

/// The URL of `file` in the package folder `folder`. `file` comes from a manifest, so it is untrusted: a
/// path that leaves the folder, names another host or carries a query is refused.
pub fn join(folder: &str, file: &str) -> Result<String, WebError> {
    let safe = !file.is_empty()
        && !file.starts_with('/')
        && file
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
    if !safe {
        return Err(WebError::new(
            "package_invalid",
            format!("unsafe file name in the manifest: {file:?}"),
        ));
    }
    if folder.ends_with('/') {
        Ok(format!("{folder}{file}"))
    } else {
        Ok(format!("{folder}/{file}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_with_one_slash() {
        assert_eq!(
            join("products/a1/", "mesh.rvm").expect("safe"),
            "products/a1/mesh.rvm"
        );
        assert_eq!(
            join("products/a1", "mesh.rvm").expect("safe"),
            "products/a1/mesh.rvm"
        );
        assert_eq!(
            join("https://x.example/p/", "textures/a.png").expect("safe"),
            "https://x.example/p/textures/a.png"
        );
    }

    #[test]
    fn refuses_a_way_out_of_the_folder() {
        for bad in [
            "../secret",
            "a/../../b",
            "/etc/passwd",
            "//other.example/x",
            "https://other.example/x",
            "a?b=1",
            "a#b",
            "a\\b",
            "a//b",
            "./a",
            "",
        ] {
            assert_eq!(join("p/", bad).expect_err(bad).code, "package_invalid", "{bad}");
        }
    }
}
