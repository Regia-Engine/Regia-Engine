// SPDX-License-Identifier: MPL-2.0
//! Generic content-format version migration loop (`docs/specs/12-assets.md` § 4a).
//!
//! Every frozen Regia Engine content format carries a `version` (or `format_version`) field. A loader
//! rejects a *newer* version with a clear error and, for an *older* version, runs the registered
//! migration chain one step at a time until it reaches the current version. This module is the
//! shared stepping loop used by every format crate so the chain semantics (gap detection, version
//! stamping, a convergence guard) live in one place.

/// One migration step for a versioned content format.
///
/// `from` and `to` name the versions the step bridges; `migrate` rewrites `value` in place. Steps
/// must chain so that every version between the format's first and current version has exactly one
/// step leaving it (a missing step is a hard error, reported by [`migrate`]). A step must **advance**
/// the version (`to > from`): an identity or backward step is rejected, because it would otherwise
/// shadow a later step leaving the same version and loop the chain forever.
#[derive(Clone, Copy, Debug)]
pub struct VersionStep<T> {
    /// Version the step reads.
    pub from: u32,
    /// Version the step writes (must be greater than [`from`](Self::from)).
    pub to: u32,
    /// The transform. Returning `Err` aborts the chain with [`VersionMigrationError`].
    pub migrate: fn(&mut T) -> Result<(), String>,
}

/// A version migration failed: a gap in the chain or a step returned an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionMigrationError {
    /// Version the failing step reads.
    pub from: u32,
    /// Version the chain was migrating toward.
    pub to: u32,
    /// Why it failed.
    pub reason: String,
}

impl std::fmt::Display for VersionMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "migration v{} -> v{} failed: {}",
            self.from, self.to, self.reason
        )
    }
}

impl std::error::Error for VersionMigrationError {}

/// Steps `value`'s version from `version` up to `target` through `steps`.
///
/// Returns the final version (equal to `target` on success). Errors on a gap in the chain, a step
/// that fails, a step that does not advance the version or that overshoots `target`, and bails if
/// the chain does not converge within 32 steps.
///
/// Steps mutate `value` in place. If a step fails, mutations from earlier steps and from the
/// failing step remain; this function does not roll the payload back.
pub fn migrate<T>(
    value: &mut T,
    version: u32,
    steps: &[VersionStep<T>],
    target: u32,
) -> Result<u32, VersionMigrationError> {
    let mut version = version;
    let mut guard = 0u32;
    while version < target {
        let Some(step) = steps.iter().find(|s| s.from == version) else {
            return Err(VersionMigrationError {
                from: version,
                to: target,
                reason: format!("no migration registered for v{version}"),
            });
        };
        // A step must advance the version. Rejecting a non-advancing step (rather than letting it
        // run) stops an identity `v1 -> v1` entry from shadowing a later `v1 -> v2` step and
        // looping the chain into the convergence bail.
        if step.to <= version {
            return Err(VersionMigrationError {
                from: version,
                to: target,
                reason: format!(
                    "migration step v{} -> v{} does not advance the version",
                    step.from, step.to
                ),
            });
        }
        // Overshooting `target` would stamp a body of a newer version as a successful migration.
        if step.to > target {
            return Err(VersionMigrationError {
                from: version,
                to: target,
                reason: format!(
                    "migration step v{} -> v{} overshoots the target v{target}",
                    step.from, step.to
                ),
            });
        }
        if guard == 32 {
            return Err(VersionMigrationError {
                from: version,
                to: target,
                reason: "migration chain did not converge".into(),
            });
        }
        (step.migrate)(value).map_err(|reason| VersionMigrationError {
            from: version,
            to: step.to,
            reason,
        })?;
        version = step.to;
        guard += 1;
    }
    Ok(version)
}

/// Reads the `version` / `format_version` field from the top level of a RON document without fully
/// deserialising it.
///
/// A loader calls this *before* the full deserialise so it can reject a *newer* version with a
/// clear "upgrade the engine" error even when the file's body no longer matches the current struct
/// (which a full deserialise would report as a RON parse error, hiding the version problem). The
/// document is parsed as a [`ron::Value`] map, so both the bare `(…)` and named `Struct(…)` RON
/// forms are accepted.
///
/// Returns the version, or an error when the top level is not a map or neither key is present.
pub fn peek_ron_version(text: &str) -> Result<u32, String> {
    let value: ron::Value = ron::from_str(text).map_err(|e| e.to_string())?;
    let ron::Value::Map(map) = value else {
        return Err("top level is not a map".to_string());
    };
    // `format_version` names the format generation; a separate `version` may be a content revision,
    // so it wins when both are present.
    for key in ["format_version", "version"] {
        let Some(found) = map.get(&ron::Value::String(key.to_owned())) else {
            continue;
        };
        return match found {
            ron::Value::Number(number) => ron::Value::Number(*number)
                .into_rust::<u32>()
                .map_err(|_| format!("`{key}` is not a valid u32")),
            _ => Err(format!("`{key}` is not a number")),
        };
    }
    Err("file has no `version` or `format_version` field".to_string())
}

/// `text` with the number after the top-level `key:` replaced by `version`, every other byte
/// kept: comments, order and formatting survive a migration that only bumps the version. Strings,
/// comments and nested values are skipped. `None` when the top level has no such key.
#[must_use]
pub fn rewrite_ron_version(text: &str, key: &str, version: u32) -> Option<String> {
    fn gap(bytes: &[u8], mut at: usize) -> Option<usize> {
        loop {
            match bytes.get(at).copied() {
                Some(b) if b.is_ascii_whitespace() => at += 1,
                Some(b'/') if bytes.get(at + 1) == Some(&b'/') => {
                    while bytes.get(at).is_some_and(|b| *b != b'\n') {
                        at += 1;
                    }
                }
                Some(b'/') if bytes.get(at + 1) == Some(&b'*') => {
                    at += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        let b = *bytes.get(at)?;
                        if b == b'/' && bytes.get(at + 1) == Some(&b'*') {
                            depth += 1;
                            at += 2;
                        } else if b == b'*' && bytes.get(at + 1) == Some(&b'/') {
                            depth -= 1;
                            at += 2;
                        } else {
                            at += 1;
                        }
                    }
                }
                _ => return Some(at),
            }
        }
    }
    let bytes = text.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut nest = 1;
                i += 2;
                while i < bytes.len() && nest > 0 {
                    match (bytes[i], bytes.get(i + 1)) {
                        (b'/', Some(b'*')) => {
                            nest += 1;
                            i += 2;
                        }
                        (b'*', Some(b'/')) => {
                            nest -= 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 1
                && bytes[i..].starts_with(key.as_bytes())
                && (i == 0 || !ident(bytes[i - 1]))
                && !bytes.get(i + key.len()).is_some_and(|b| ident(*b)) =>
            {
                let mut j = gap(bytes, i + key.len())?;
                if bytes.get(j) != Some(&b':') {
                    i += key.len();
                    continue;
                }
                j = gap(bytes, j + 1)?;
                let start = j;
                while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                if j == start {
                    return None;
                }
                return Some(format!("{}{version}{}", &text[..start], &text[j..]));
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_version_gaps_allow_line_and_nested_block_comments() {
        for gap in [" /* keep /* nested */ comment */ ", " // keep\n "] {
            let text = format!("(version{gap}:{gap}1, name: \"keep\")");
            let expected = format!("(version{gap}:{gap}2, name: \"keep\")");
            assert_eq!(rewrite_ron_version(&text, "version", 2), Some(expected));
        }
    }

    #[test]
    fn a_version_rewrite_changes_only_the_root_number() {
        let text = "// version: 9 in a comment\n(\n    name: \"version: 9\", /* version: 8 */\n    \
                    nested: (version: 7),\n    format_version: 6,\n    version: 1, // keep me\n)\n";
        let out = rewrite_ron_version(text, "version", 2).expect("found");
        assert_eq!(out, text.replace("version: 1,", "version: 2,"));
        assert_eq!(rewrite_ron_version("(name: \"x\")", "version", 2), None);
        assert_eq!(rewrite_ron_version("(version: \"x\")", "version", 2), None);
        assert_eq!(
            rewrite_ron_version("Seq(version:1)", "version", 12).as_deref(),
            Some("Seq(version:12)")
        );
    }

    /// A fake value whose migration steps record themselves and rewrite a field.
    #[derive(Debug, Default)]
    struct Doc {
        version: u32,
        name: String,
        seen: Vec<u32>,
    }

    fn bump(doc: &mut Doc) -> Result<(), String> {
        doc.seen.push(doc.version);
        Ok(())
    }

    fn step_from_0(doc: &mut Doc) -> Result<(), String> {
        doc.seen.push(0);
        Ok(())
    }

    fn step_from_2(doc: &mut Doc) -> Result<(), String> {
        doc.seen.push(2);
        Ok(())
    }

    #[test]
    fn migrates_through_each_step_and_returns_target() {
        let chain = [
            VersionStep {
                from: 0,
                to: 2,
                migrate: step_from_0,
            },
            VersionStep {
                from: 2,
                to: 5,
                migrate: step_from_2,
            },
        ];
        let mut doc = Doc::default();
        let to = migrate(&mut doc, 0, &chain, 5).expect("chain");
        assert_eq!(to, 5);
        assert_eq!(doc.seen, vec![0, 2], "both steps ran in order");
    }

    #[test]
    fn no_work_when_already_at_target() {
        let mut doc = Doc::default();
        let to = migrate(&mut doc, 1, &[], 1).expect("no-op");
        assert_eq!(to, 1);
        assert!(doc.seen.is_empty());
    }

    #[test]
    fn appended_step_from_the_current_version_runs() {
        // Regression: the deleted `v1 -> v1` identity step used to win the `find` and send an
        // appended `v1 -> v2` step into the 32-step bail. A registered step leaving the current
        // version must be found directly.
        let chain = [VersionStep {
            from: 1,
            to: 2,
            migrate: |doc: &mut Doc| {
                doc.seen.push(1);
                Ok(())
            },
        }];
        let mut doc = Doc::default();
        let to = migrate(&mut doc, 1, &chain, 2).expect("v1 -> v2");
        assert_eq!(to, 2);
        assert_eq!(doc.seen, vec![1]);
    }

    #[test]
    fn a_registered_step_rewrites_content() {
        // The mechanism the loaders rely on: a step changes the value (not just the version) and
        // the result is observed by the caller.
        let chain = [VersionStep {
            from: 0,
            to: 1,
            migrate: |doc: &mut Doc| {
                doc.name = "migrated".to_string();
                Ok(())
            },
        }];
        let mut doc = Doc {
            name: "old".to_string(),
            ..Doc::default()
        };
        let to = migrate(&mut doc, 0, &chain, 1).expect("step");
        assert_eq!(to, 1);
        assert_eq!(doc.name, "migrated");
    }

    #[test]
    fn gap_in_chain_is_an_error() {
        let chain = [VersionStep {
            from: 0,
            to: 2,
            migrate: bump,
        }];
        let mut doc = Doc::default();
        let err = migrate(&mut doc, 3, &chain, 5).expect_err("gap");
        assert_eq!(err.from, 3, "{err}");
        assert!(err.reason.contains("no migration"), "{err}");
    }

    #[test]
    fn step_failure_is_reported() {
        fn fail(_: &mut Doc) -> Result<(), String> {
            Err("boom".into())
        }
        let chain = [VersionStep {
            from: 0,
            to: 1,
            migrate: fail,
        }];
        let mut doc = Doc::default();
        let err = migrate(&mut doc, 0, &chain, 1).expect_err("step failed");
        assert_eq!(err.reason, "boom");
    }

    #[test]
    fn non_advancing_step_is_rejected() {
        // An identity `v1 -> v1` step must be rejected outright, not looped into the convergence
        // bail (which is what the old implementation did).
        let chain = [VersionStep {
            from: 1,
            to: 1,
            migrate: bump,
        }];
        let mut doc = Doc::default();
        let err = migrate(&mut doc, 1, &chain, 2).expect_err("non-advancing step");
        assert!(err.reason.contains("does not advance"), "{err}");
    }

    #[test]
    fn non_converging_chain_bails() {
        // With the "must advance" check a step can no longer loop the chain, but a chain of more
        // than 32 advancing steps still trips the convergence guard rather than running forever.
        let mut chain = Vec::new();
        for i in 0..40u32 {
            chain.push(VersionStep {
                from: i,
                to: i + 1,
                migrate: |doc: &mut Doc| {
                    doc.seen.push(doc.version);
                    Ok(())
                },
            });
        }
        let mut doc = Doc::default();
        let err = migrate(&mut doc, 0, &chain, 40).expect_err("no convergence");
        assert!(err.reason.contains("did not converge"), "{err}");
    }

    #[test]
    fn peek_reads_version_before_deserialise() {
        assert_eq!(
            peek_ron_version(r#"( format_version: 1, name: "x", shader: Standard )"#).expect("v"),
            1
        );
        assert_eq!(peek_ron_version(r#"( version: 2, root: () )"#).expect("v"), 2);
        // Named-struct scene form.
        assert_eq!(
            peek_ron_version("SceneFile( format_version: 1, entities: [], resources: {} )").expect("v"),
            1
        );
    }

    #[test]
    fn peek_ignores_a_body_that_does_not_match() {
        // A newer file whose body uses a field the v1 struct does not know: the version is still
        // readable, so the loader can reject it as a version error rather than a RON error.
        let text = r#"( format_version: 2, future_field: true, base: () )"#;
        assert_eq!(peek_ron_version(text).expect("v"), 2);
    }

    #[test]
    fn migrate_rejects_a_step_past_the_target() {
        let chain = [VersionStep {
            from: 1,
            to: 3,
            migrate: bump,
        }];
        let mut doc = Doc::default();
        let err = migrate(&mut doc, 1, &chain, 2).expect_err("overshoot");
        assert!(err.reason.contains("overshoots"), "{err}");
        assert!(doc.seen.is_empty(), "the step must not run");
    }

    #[test]
    fn peek_prefers_format_version_over_version() {
        assert_eq!(
            peek_ron_version("( version: 1, format_version: 5 )").expect("v"),
            5
        );
        assert_eq!(
            peek_ron_version("( format_version: 5, version: 9 )").expect("v"),
            5
        );
    }

    #[test]
    fn peek_non_integer_version_is_an_error() {
        assert!(peek_ron_version("( format_version: 1.5 )").is_err());
        assert!(peek_ron_version(r#"( version: "2" )"#).is_err());
    }

    #[test]
    fn peek_missing_version_is_an_error() {
        assert!(peek_ron_version(r#"( name: "no version here" )"#).is_err());
    }
}
