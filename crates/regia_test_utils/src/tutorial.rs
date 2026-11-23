// SPDX-License-Identifier: MPL-2.0
//! Book tutorials as doc tests (ADR-9810): the steps of a `book/tutorials/*.md` page, read from
//! the page itself, so a test that runs them fails when the page and the engine drift apart.
//!
//! A step is one line of a fence whose info string is exactly `sh`. Other fences (`text`,
//! `console`, `json`, ...) are illustration and are not run. Three line forms are steps:
//!
//! ```text
//! regia agent call editor.play_state                                  # no parameters
//! regia agent call scene.spawn_primitive '{"shape":"cuboid"}'         # one JSON object in '...'
//! SUN=$(regia agent call editor.spawn '{"name":"Sun"}' | jq .entity)  # keep a result field
//! regia agent call editor.add_component '{"entity":'"$SUN"',"component":"DirectionalLight"}'
//! regia agent call editor.play_state  # until /mode == "playing"
//! regia agent call editor.save_level '{}'  # expect /path == "assets/maps/first.level.ron"
//! regia new "First Scene" --template blank                            # a CLI step
//! cd first_scene                                                      # moves the CLI steps
//! ```
//!
//! `'"$NAME"'` inside the parameters is the shell's way of splicing a variable into single-quoted
//! JSON; [`AgentStep::params_with`] replaces it with the kept value. A trailing `# expect P == V`
//! checks the answer once; `# until P == V` repeats the call until the answer matches, so it must
//! name a read call: [`refuse_mutating_until`] refuses one the schema marks mutating, with the
//! page and line (ADR-7451). `P` is a JSON pointer, `V` JSON text. Any other `# ...` is a comment. Blank lines and
//! lines starting with `#` are skipped. Every other line in an `sh` fence is an error, so a page
//! cannot hold a step the test silently skips.

/// A check on an agent answer: the value at a JSON pointer equals JSON text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerCheck {
    /// `true` for `# until` (repeat the call until it matches), `false` for `# expect`.
    pub until: bool,
    /// The JSON pointer (`/mode`, `/entities`, `/project/version`).
    pub pointer: String,
    /// The expected value as JSON text (`"playing"`, `3`, `true`).
    pub value: String,
}

/// One `regia agent call` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStep {
    /// 1-based line of the page.
    pub line: usize,
    /// The method (`editor.spawn`).
    pub method: String,
    /// The parameter JSON text as written (with `'"$NAME"'` splices); `{}` when none.
    pub params: String,
    /// `NAME=$(... | jq .field)`: keep `field` of the answer as `$NAME`.
    pub keep: Option<(String, String)>,
    /// The trailing `# expect` / `# until`.
    pub check: Option<AnswerCheck>,
}

impl AgentStep {
    /// The parameter text with every `'"$NAME"'` replaced by `lookup(NAME)` (JSON text). An
    /// unknown name is an error naming the page line.
    ///
    /// # Errors
    ///
    /// A splice of a name `lookup` does not know.
    pub fn params_with(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = self.params.as_str();
        while let Some(start) = rest.find("'\"$") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 3..];
            let Some(end) = after.find("\"'") else {
                return Err(format!("line {}: unclosed '\"$NAME\"' splice", self.line));
            };
            let name = &after[..end];
            let value = lookup(name)
                .ok_or_else(|| format!("line {}: ${name} is not set by an earlier step", self.line))?;
            out.push_str(&value);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// One step of a tutorial page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TutorialStep {
    /// `regia agent call ...`.
    Agent(AgentStep),
    /// `regia <args>` (not `agent call`): the CLI's arguments, shell-split.
    Cli {
        /// 1-based line of the page.
        line: usize,
        /// The arguments after `regia`.
        args: Vec<String>,
    },
    /// `cd <dir>`: later CLI steps run there.
    Cd {
        /// 1-based line of the page.
        line: usize,
        /// The directory, relative to the current one.
        dir: String,
    },
}

/// Splits a shell line into words: whitespace separates, `'...'` and `"..."` group (no escapes
/// inside single quotes; `\"` inside double quotes).
///
/// # Errors
///
/// An unclosed quote.
pub fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(format!("unclosed ' in {line:?}")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if chars.peek() == Some(&'"') => {
                            word.push('"');
                            chars.next();
                        }
                        Some(c) => word.push(c),
                        None => return Err(format!("unclosed \" in {line:?}")),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Splits `text  # comment` at the first ` #` that is outside single quotes.
fn split_comment(text: &str) -> (&str, Option<&str>) {
    let mut quoted = false;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' => quoted = !quoted,
            b'#' if !quoted && i > 0 && bytes[i - 1].is_ascii_whitespace() => {
                return (text[..i].trim_end(), Some(text[i + 1..].trim()));
            }
            _ => {}
        }
    }
    (text.trim_end(), None)
}

fn parse_check(comment: &str, line: usize) -> Result<Option<AnswerCheck>, String> {
    let (until, rest) = if let Some(rest) = comment.strip_prefix("until ") {
        (true, rest)
    } else if let Some(rest) = comment.strip_prefix("expect ") {
        (false, rest)
    } else {
        return Ok(None);
    };
    let Some((pointer, value)) = rest.split_once(" == ") else {
        return Err(format!("line {line}: `# {comment}` is not `POINTER == JSON`"));
    };
    let pointer = pointer.trim();
    if !pointer.starts_with('/') {
        return Err(format!("line {line}: {pointer:?} is not a JSON pointer"));
    }
    Ok(Some(AnswerCheck {
        until,
        pointer: pointer.to_string(),
        value: value.trim().to_string(),
    }))
}

/// The agent call in `code` (`regia agent call METHOD ['JSON']`), with an optional
/// `NAME=$( ... | jq .field)` around it.
fn parse_agent(code: &str, line: usize) -> Result<Option<AgentStep>, String> {
    let (keep_name, call) = match code.split_once("=$(") {
        Some((name, rest))
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            (Some(name.to_string()), rest)
        }
        _ => (None, code),
    };
    let Some(after) = call.strip_prefix("regia agent call ") else {
        return Ok(None);
    };
    let (call, keep) = if let Some(name) = keep_name {
        let Some(inner) = after.strip_suffix(')') else {
            return Err(format!("line {line}: `{name}=$(` is not closed"));
        };
        let Some((call, jq)) = inner.rsplit_once(" | jq ") else {
            return Err(format!("line {line}: `{name}=$(...)` must end in `| jq .field`"));
        };
        let field = jq.trim().trim_start_matches("-r ").trim();
        let Some(field) = field.strip_prefix('.') else {
            return Err(format!("line {line}: jq filter {field:?} is not `.field`"));
        };
        (call, Some((name, field.to_string())))
    } else {
        (after, None)
    };
    let call = call.trim();
    let (method, params) = match call.split_once(char::is_whitespace) {
        Some((method, rest)) => {
            let rest = rest.trim();
            let Some(inner) = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) else {
                return Err(format!(
                    "line {line}: the parameters {rest:?} are not one '...' JSON object"
                ));
            };
            (method, inner.to_string())
        }
        None => (call, "{}".to_string()),
    };
    if method.is_empty() || !method.contains('.') {
        return Err(format!("line {line}: {method:?} is not a method name"));
    }
    Ok(Some(AgentStep {
        line,
        method: method.to_string(),
        params,
        keep,
        check: None,
    }))
}

/// The steps of a tutorial page, in page order.
///
/// # Errors
///
/// A line in an `sh` fence that is not a step, a malformed step, or an unclosed fence; each
/// error names the page line.
pub fn tutorial_steps(markdown: &str) -> Result<Vec<TutorialStep>, String> {
    let mut steps = Vec::new();
    let mut fence: Option<bool> = None;
    for (index, raw) in markdown.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if let Some(info) = trimmed.strip_prefix("```") {
            fence = match fence {
                None => Some(info.trim() == "sh"),
                Some(_) => None,
            };
            continue;
        }
        if fence != Some(true) || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (code, comment) = split_comment(trimmed);
        if let Some(mut step) = parse_agent(code, line)? {
            step.check = match comment {
                Some(comment) => parse_check(comment, line)?,
                None => None,
            };
            steps.push(TutorialStep::Agent(step));
        } else if let Some(dir) = code.strip_prefix("cd ") {
            steps.push(TutorialStep::Cd {
                line,
                dir: dir.trim().to_string(),
            });
        } else if code.starts_with("regia ") {
            let words = shell_words(code).map_err(|e| format!("line {line}: {e}"))?;
            steps.push(TutorialStep::Cli {
                line,
                args: words[1..].to_vec(),
            });
        } else {
            return Err(format!(
                "line {line}: {code:?} in an `sh` fence is not a tutorial step (put illustration in a `text` fence)"
            ));
        }
    }
    if fence.is_some() {
        return Err("a fence is not closed".to_string());
    }
    Ok(steps)
}

/// The agent steps of `steps`, in order.
#[must_use]
pub fn agent_steps(steps: &[TutorialStep]) -> Vec<&AgentStep> {
    steps
        .iter()
        .filter_map(|step| match step {
            TutorialStep::Agent(step) => Some(step),
            _ => None,
        })
        .collect()
}

/// Refuses a page whose `# until` names a call that is not known to be a read call (ADR-7451
/// (5)): `# until` repeats its call up to thousands of times, so a mutating method would spawn,
/// save or step the editor once per repeat. `mutating(method)` answers the schema's `mutating`
/// flag, `None` for a method the schema does not list (which is refused too: it cannot be shown
/// to be a read call). `# expect` checks its answer once and may follow any call.
///
/// # Errors
///
/// The first offending step, as `PAGE:LINE: ...` naming the method.
pub fn refuse_mutating_until(
    page: &str,
    steps: &[TutorialStep],
    mutating: impl Fn(&str) -> Option<bool>,
) -> Result<(), String> {
    for step in agent_steps(steps) {
        if !step.check.as_ref().is_some_and(|c| c.until) {
            continue;
        }
        match mutating(&step.method) {
            Some(false) => {}
            Some(true) => {
                return Err(format!(
                    "{page}:{}: `# until` repeats {}, which the schema marks mutating: repeat a read \
                     call (`# until`) after it, or check this call once with `# expect`",
                    step.line, step.method
                ));
            }
            None => {
                return Err(format!(
                    "{page}:{}: `# until` repeats {}, which the schema does not list, so it is not \
                     known to be a read call",
                    step.line, step.method
                ));
            }
        }
    }
    Ok(())
}

/// Reads `book/tutorials/<page>` of the workspace at run time (an edit to the page needs no
/// rebuild of the test).
///
/// # Errors
///
/// The page cannot be read.
pub fn read_page(page: &str) -> Result<String, String> {
    let path = crate::book::workspace_root().join("book/tutorials").join(page);
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"# A page

Text with `regia agent call not.a_step` in prose.

```sh
regia new "First Scene" --template blank
cd first_scene
# a comment line
regia agent call editor.play_state  # until /mode == "playing"
SUN=$(regia agent call editor.spawn '{"name":"Sun # one"}' | jq .entity)
regia agent call editor.add_component '{"entity":'"$SUN"',"component":"DirectionalLight"}'  # expect /added == true
regia agent call editor.save_level '{}'  # saves the level
```

```text
regia agent call ignored.here
anything goes
```
"#;

    #[test]
    fn steps_come_from_sh_fences_in_page_order() {
        let steps = tutorial_steps(PAGE).expect("parses");
        assert_eq!(steps.len(), 6, "{steps:#?}");
        assert_eq!(
            steps[0],
            TutorialStep::Cli {
                line: 6,
                args: vec![
                    "new".into(),
                    "First Scene".into(),
                    "--template".into(),
                    "blank".into()
                ],
            }
        );
        assert_eq!(
            steps[1],
            TutorialStep::Cd {
                line: 7,
                dir: "first_scene".into()
            }
        );
        let agent = agent_steps(&steps);
        assert_eq!(agent.len(), 4);
        assert_eq!(agent[0].method, "editor.play_state");
        assert_eq!(agent[0].params, "{}");
        assert_eq!(
            agent[0].check,
            Some(AnswerCheck {
                until: true,
                pointer: "/mode".into(),
                value: "\"playing\"".into()
            })
        );
        assert_eq!(agent[1].keep, Some(("SUN".into(), "entity".into())));
        assert_eq!(
            agent[1].params, r#"{"name":"Sun # one"}"#,
            "a # inside quotes is not a comment"
        );
        assert_eq!(agent[2].check.as_ref().map(|c| c.until), Some(false));
        assert_eq!(agent[3].check, None, "a plain comment is no check");
    }

    #[test]
    fn splices_take_kept_values_and_unknown_names_fail() {
        let steps = tutorial_steps(PAGE).expect("parses");
        let add = agent_steps(&steps)[2].clone();
        let filled = add
            .params_with(|name| (name == "SUN").then(|| "42".to_string()))
            .expect("known");
        assert_eq!(filled, r#"{"entity":42,"component":"DirectionalLight"}"#);
        let err = add.params_with(|_| None).expect_err("unknown");
        assert!(err.contains("$SUN") && err.contains("line 11"), "{err}");
    }

    #[test]
    fn a_stray_line_in_an_sh_fence_is_an_error() {
        let err = tutorial_steps("```sh\necho hi\n```\n").expect_err("not a step");
        assert!(err.contains("line 2"), "{err}");
        let err = tutorial_steps("```sh\nregia agent call x.y '{\n```\n").expect_err("bad params");
        assert!(err.contains("line 2"), "{err}");
        assert!(
            tutorial_steps("```sh\nregia agent call a.b\n").is_err(),
            "unclosed fence"
        );
    }

    /// ADR-7451 (5): `# until` repeats its call, so it may only name a read call: a page whose
    /// `# until` names a method the schema marks mutating (or does not list) is refused with the
    /// page and line; read calls and `# expect` on a mutating call pass.
    #[test]
    fn until_on_a_mutating_method_is_refused_with_page_and_line() {
        const FIXTURE: &str = r#"```sh
regia agent call editor.play_state  # until /mode == "playing"
regia agent call editor.spawn '{"name":"Crate"}'  # expect /name == "Crate"
regia agent call editor.spawn '{"name":"Again"}'  # until /entities == 3
```
"#;
        let schema = |method: &str| match method {
            "editor.play_state" => Some(false),
            "editor.spawn" => Some(true),
            _ => None,
        };
        let steps = tutorial_steps(FIXTURE).expect("the page parses");
        let err = refuse_mutating_until("fixture.md", &steps, schema).expect_err("a mutating until");
        assert!(
            err.contains("fixture.md:4") && err.contains("editor.spawn") && err.contains("mutating"),
            "{err}"
        );
        // Without the bad line the page passes: a read `# until`, a mutating `# expect`.
        let fine: String = FIXTURE
            .lines()
            .filter(|l| !l.contains("Again"))
            .map(|l| format!("{l}\n"))
            .collect();
        let steps = tutorial_steps(&fine).expect("parses");
        refuse_mutating_until("fixture.md", &steps, schema).expect("read until and mutating expect");
        // A method the schema does not list cannot be shown to be a read call.
        let steps =
            tutorial_steps("```sh\nregia agent call editor.nope  # until /a == 1\n```\n").expect("parses");
        let err = refuse_mutating_until("other.md", &steps, schema).expect_err("unknown");
        assert!(err.contains("other.md:2") && err.contains("editor.nope"), "{err}");
    }

    #[test]
    fn shell_words_group_quotes() {
        assert_eq!(
            shell_words(r#"package --platform linux --out "my dir" 'a b'"#).expect("words"),
            ["package", "--platform", "linux", "--out", "my dir", "a b"]
        );
        assert!(shell_words("'open").is_err());
    }
}
