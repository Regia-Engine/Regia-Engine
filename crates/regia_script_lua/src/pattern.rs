// SPDX-License-Identifier: MIT
// Portions ported from Lua (https://www.lua.org, commit 5.1.5), file src/lstrlib.c: the pattern
// matcher and `str_find_aux`, `gmatch`, `str_gsub`, with a step budget and a depth limit added.
//
// Copyright (C) 1994-2012 Lua.org, PUC-Rio.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or
// substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT
// NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT
// OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! `string.find`, `match`, `gmatch` and `gsub` for mods (ADR-1957 § 2): Lua 5.1's matcher with
//! every step charged to the call's instruction budget.
//!
//! The C matcher runs as one VM instruction, so the count hook never fires inside it, and it
//! backtracks exponentially: `string.find(("a"):rep(60), ("a?"):rep(60) .. "b")` would hang the
//! game thread for good. The C version also recurses once per pattern item with no limit. This
//! port answers the same results and raises the same message text (a test runs both over one
//! corpus); it adds "pattern too complex" past [`MAX_MATCH_DEPTH`], the limit later Lua versions
//! adopted. Its errors reach a script the way every host function's do, as the binding's error
//! value, and an error object thrown from a `gsub` callback comes back as that value too.

use std::sync::Arc;

use mlua::{Lua, LuaString, MultiValue, Value};

use crate::sandbox::Budget;
use crate::value::lua_type;

/// Most captures in one pattern (`LUA_MAXCAPTURES`).
const MAX_CAPTURES: usize = 32;
/// Deepest matcher recursion, one level per backtracking pattern item.
const MAX_MATCH_DEPTH: usize = 200;
/// Matcher steps one budget instruction buys: a step is one turn of a matcher loop, cheaper than a
/// VM instruction, so ordinary text work stays a small share of a call.
const STEPS_PER_INSTRUCTION: u64 = 4;
/// Bytes compared per matcher step.
const BYTES_PER_STEP: u64 = 16;
/// The escape character.
const ESC: u8 = b'%';
/// A pattern with none of these takes the plain search (`SPECIALS`).
const SPECIALS: &[u8] = b"^$*+?.([%-";

/// How a capture ended, or that it has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureLen {
    Unfinished,
    Position,
    Closed(usize),
}

#[derive(Debug, Clone, Copy)]
struct Capture {
    init: usize,
    len: CaptureLen,
}

/// Why a match stopped short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchError {
    /// The step limit ran out.
    Budget,
    /// The pattern is malformed; the message Lua raises.
    Bad(&'static str),
}

/// What a replacement takes from a match.
enum CaptureText {
    /// These bytes of the subject.
    Span(std::ops::Range<usize>),
    /// A position capture, written as a number.
    Position(usize),
}

/// A `gsub` result being built on the Rust heap, which the Lua memory cap does not see.
struct GsubOut<'a> {
    bytes: Vec<u8>,
    max_bytes: u64,
    budget: &'a Budget,
}

/// `MatchState`: one subject, one pattern, the steps spent on them.
struct Matcher<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    level: usize,
    capture: [Capture; MAX_CAPTURES],
    depth: usize,
    steps: u64,
    step_limit: u64,
}

impl<'a> Matcher<'a> {
    fn new(src: &'a [u8], pat: &'a [u8], budget: &Budget) -> Self {
        Self {
            src,
            pat,
            level: 0,
            capture: [Capture {
                init: 0,
                len: CaptureLen::Unfinished,
            }; MAX_CAPTURES],
            depth: 0,
            steps: 0,
            step_limit: budget.left().saturating_mul(STEPS_PER_INSTRUCTION),
        }
    }

    /// The pattern byte at `p`, NUL past its end (the C code reads the terminator).
    fn p(&self, p: usize) -> u8 {
        self.pat.get(p).copied().unwrap_or(0)
    }

    /// The subject byte at `s`, NUL at its end.
    fn s(&self, s: usize) -> u8 {
        self.src.get(s).copied().unwrap_or(0)
    }

    fn tick(&mut self, steps: u64) -> Result<(), MatchError> {
        self.steps = self.steps.saturating_add(steps);
        if self.steps > self.step_limit {
            return Err(MatchError::Budget);
        }
        Ok(())
    }

    /// Moves the steps spent so far onto the call's budget.
    fn settle(&mut self, budget: &Budget) -> mlua::Result<()> {
        let spent = self.steps.div_ceil(STEPS_PER_INSTRUCTION);
        self.steps = 0;
        budget.charge(spent)?;
        self.step_limit = budget.left().saturating_mul(STEPS_PER_INSTRUCTION);
        Ok(())
    }

    /// The Lua error of `result`, with the steps charged: a `pcall` loop over a pattern that fails
    /// late must pay for what it walked.
    fn lift<T>(&mut self, budget: &Budget, result: Result<T, MatchError>) -> mlua::Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(MatchError::Bad(message)) => {
                self.settle(budget)?;
                Err(mlua::Error::runtime(message))
            }
            Err(MatchError::Budget) => {
                self.settle(budget)?;
                Err(budget.exhausted())
            }
        }
    }

    fn check_capture(&self, l: u8) -> Result<usize, MatchError> {
        let bad = MatchError::Bad("invalid capture index");
        let l = usize::from(l.checked_sub(b'1').ok_or(bad)?);
        if l >= self.level || self.capture[l].len == CaptureLen::Unfinished {
            return Err(bad);
        }
        Ok(l)
    }

    fn capture_to_close(&self) -> Result<usize, MatchError> {
        self.capture[..self.level]
            .iter()
            .rposition(|c| c.len == CaptureLen::Unfinished)
            .ok_or(MatchError::Bad("invalid pattern capture"))
    }

    /// The pattern position after the single-character class at `p`.
    fn class_end(&mut self, p: usize) -> Result<usize, MatchError> {
        let mut at = p + 1;
        match self.p(p) {
            ESC => {
                if self.p(at) == 0 {
                    return Err(MatchError::Bad("malformed pattern (ends with '%')"));
                }
                Ok(at + 1)
            }
            b'[' => {
                if self.p(at) == b'^' {
                    at += 1;
                }
                loop {
                    let c = self.p(at);
                    if c == 0 {
                        return Err(MatchError::Bad("malformed pattern (missing ']')"));
                    }
                    at += 1;
                    if c == ESC && self.p(at) != 0 {
                        at += 1;
                    }
                    if self.p(at) == b']' {
                        break;
                    }
                }
                self.tick((at - p) as u64 / BYTES_PER_STEP)?;
                Ok(at + 1)
            }
            _ => Ok(at),
        }
    }

    /// `matchbracketclass`: `p` is the `[`, `ec` the `]`.
    fn bracket_class(&self, c: u8, mut p: usize, ec: usize) -> bool {
        let mut sig = true;
        if self.p(p + 1) == b'^' {
            sig = false;
            p += 1;
        }
        loop {
            p += 1;
            if p >= ec {
                return !sig;
            }
            if self.p(p) == ESC {
                p += 1;
                if match_class(c, self.p(p)) {
                    return sig;
                }
            } else if self.p(p + 1) == b'-' && p + 2 < ec {
                p += 2;
                if self.p(p - 2) <= c && c <= self.p(p) {
                    return sig;
                }
            } else if self.p(p) == c {
                return sig;
            }
        }
    }

    fn single_match(&self, c: u8, p: usize, ep: usize) -> bool {
        match self.p(p) {
            b'.' => true,
            ESC => match_class(c, self.p(p + 1)),
            b'[' => self.bracket_class(c, p, ep - 1),
            other => other == c,
        }
    }

    fn match_balance(&mut self, s: usize, p: usize) -> Result<Option<usize>, MatchError> {
        if self.p(p) == 0 || self.p(p + 1) == 0 {
            return Err(MatchError::Bad("unbalanced pattern"));
        }
        if self.s(s) != self.p(p) {
            return Ok(None);
        }
        let (open, close) = (self.p(p), self.p(p + 1));
        let mut depth = 1u32;
        let mut at = s + 1;
        while at < self.src.len() {
            self.tick(1)?;
            let c = self.src[at];
            if c == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(Some(at + 1));
                }
            } else if c == open {
                depth += 1;
            }
            at += 1;
        }
        Ok(None)
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Result<Option<usize>, MatchError> {
        let cost = (ep - p) as u64;
        let mut i = 0;
        while s + i < self.src.len() && self.single_match(self.src[s + i], p, ep) {
            self.tick(cost)?;
            i += 1;
        }
        loop {
            if let Some(res) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(res));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn min_expand(&mut self, mut s: usize, p: usize, ep: usize) -> Result<Option<usize>, MatchError> {
        let cost = (ep - p) as u64;
        loop {
            if let Some(res) = self.do_match(s, ep + 1)? {
                return Ok(Some(res));
            }
            if s < self.src.len() && self.single_match(self.src[s], p, ep) {
                self.tick(cost)?;
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, len: CaptureLen) -> Result<Option<usize>, MatchError> {
        if self.level >= MAX_CAPTURES {
            return Err(MatchError::Bad("too many captures"));
        }
        self.capture[self.level] = Capture { init: s, len };
        self.level += 1;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.level -= 1;
        }
        Ok(res)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Result<Option<usize>, MatchError> {
        let l = self.capture_to_close()?;
        self.capture[l].len = CaptureLen::Closed(s - self.capture[l].init);
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.capture[l].len = CaptureLen::Unfinished;
        }
        Ok(res)
    }

    fn match_capture(&mut self, s: usize, l: u8) -> Result<Option<usize>, MatchError> {
        let l = self.check_capture(l)?;
        let Capture { init, len } = self.capture[l];
        let CaptureLen::Closed(len) = len else {
            return Ok(None);
        };
        self.tick(len as u64 / BYTES_PER_STEP)?;
        if self.src.len() - s >= len && self.src[init..init + len] == self.src[s..s + len] {
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }

    /// `match`: the subject position where the pattern from `p` ends, starting at `s`.
    fn do_match(&mut self, s: usize, p: usize) -> Result<Option<usize>, MatchError> {
        if self.depth >= MAX_MATCH_DEPTH {
            return Err(MatchError::Bad("pattern too complex"));
        }
        self.depth += 1;
        let res = self.match_here(s, p);
        self.depth -= 1;
        res
    }

    fn match_here(&mut self, mut s: usize, mut p: usize) -> Result<Option<usize>, MatchError> {
        loop {
            self.tick(1)?;
            match self.p(p) {
                b'(' => {
                    return if self.p(p + 1) == b')' {
                        self.start_capture(s, p + 2, CaptureLen::Position)
                    } else {
                        self.start_capture(s, p + 1, CaptureLen::Unfinished)
                    };
                }
                b')' => return self.end_capture(s, p + 1),
                ESC => match self.p(p + 1) {
                    b'b' => {
                        let Some(next) = self.match_balance(s, p + 2)? else {
                            return Ok(None);
                        };
                        s = next;
                        p += 4;
                        continue;
                    }
                    b'f' => {
                        p += 2;
                        if self.p(p) != b'[' {
                            return Err(MatchError::Bad("missing '[' after '%f' in pattern"));
                        }
                        let ep = self.class_end(p)?;
                        let previous = if s == 0 { 0 } else { self.src[s - 1] };
                        if self.bracket_class(previous, p, ep - 1)
                            || !self.bracket_class(self.s(s), p, ep - 1)
                        {
                            return Ok(None);
                        }
                        p = ep;
                        continue;
                    }
                    digit if digit.is_ascii_digit() => {
                        let Some(next) = self.match_capture(s, digit)? else {
                            return Ok(None);
                        };
                        s = next;
                        p += 2;
                        continue;
                    }
                    _ => {}
                },
                0 => return Ok(Some(s)),
                b'$' if self.p(p + 1) == 0 => return Ok((s == self.src.len()).then_some(s)),
                _ => {}
            }
            let ep = self.class_end(p)?;
            let matched = s < self.src.len() && self.single_match(self.src[s], p, ep);
            match self.p(ep) {
                b'?' => {
                    if matched && let Some(res) = self.do_match(s + 1, ep + 1)? {
                        return Ok(Some(res));
                    }
                    p = ep + 1;
                }
                b'*' => return self.max_expand(s, p, ep),
                b'+' => {
                    return if matched {
                        self.max_expand(s + 1, p, ep)
                    } else {
                        Ok(None)
                    };
                }
                b'-' => return self.min_expand(s, p, ep),
                _ => {
                    if !matched {
                        return Ok(None);
                    }
                    s += 1;
                    p = ep;
                }
            }
        }
    }

    /// `push_onecapture`: capture `i` of the match `s..e`.
    fn capture_value(&self, lua: &Lua, i: usize, s: usize, e: usize) -> mlua::Result<Value> {
        if i >= self.level {
            if i == 0 {
                return Ok(Value::String(lua.create_string(&self.src[s..e])?));
            }
            return Err(mlua::Error::runtime("invalid capture index"));
        }
        let Capture { init, len } = self.capture[i];
        match len {
            CaptureLen::Unfinished => Err(mlua::Error::runtime("unfinished capture")),
            CaptureLen::Position => Ok(Value::Number((init + 1) as f64)),
            CaptureLen::Closed(len) => Ok(Value::String(lua.create_string(&self.src[init..init + len])?)),
        }
    }

    /// `push_captures`: every capture, or the whole match `s..e` when the pattern has none.
    fn captures(&self, lua: &Lua, whole: Option<(usize, usize)>) -> mlua::Result<Vec<Value>> {
        let (s, e) = whole.unwrap_or((0, 0));
        let levels = if self.level == 0 && whole.is_some() {
            1
        } else {
            self.level
        };
        (0..levels).map(|i| self.capture_value(lua, i, s, e)).collect()
    }

    /// `add_s`: the replacement text with `%0`-`%9` expanded. One short replacement can name a
    /// long match many times, so the work is charged and the output capped as it grows.
    fn expand(&mut self, news: &[u8], s: usize, e: usize, out: &mut GsubOut<'_>) -> mlua::Result<()> {
        // A `gsub` result is charged one step per byte it gains, the rate of the lua lane's matcher
        // (ADR-1943 § 13), so a result that grows fast runs out of instructions before the memory
        // cap.
        let charged = self.tick(1 + news.len() as u64);
        self.lift(out.budget, charged)?;
        if (out.bytes.len() + news.len()) as u64 > out.max_bytes {
            self.settle(out.budget)?;
            return Err(out.budget.out_of_memory());
        }
        let mut i = 0;
        while i < news.len() {
            let c = news[i];
            if c != ESC {
                out.bytes.push(c);
            } else {
                i += 1;
                let d = news.get(i).copied().unwrap_or(0);
                if !d.is_ascii_digit() {
                    out.bytes.push(d);
                } else {
                    let text = if d == b'0' {
                        CaptureText::Span(s..e)
                    } else {
                        match self.capture_span(usize::from(d - b'1'), s, e) {
                            Ok(span) => span,
                            Err(error) => {
                                self.settle(out.budget)?;
                                return Err(error);
                            }
                        }
                    };
                    self.append(text, out)?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// Appends a range of the subject, charged and capped.
    fn append(&mut self, text: CaptureText, out: &mut GsubOut<'_>) -> mlua::Result<()> {
        let src = self.src;
        let bytes: std::borrow::Cow<'_, [u8]> = match text {
            CaptureText::Span(span) => src[span].into(),
            CaptureText::Position(at) => at.to_string().into_bytes().into(),
        };
        let cost = bytes.len() as u64;
        if (out.bytes.len() + bytes.len()) as u64 > out.max_bytes {
            self.settle(out.budget)?;
            return Err(out.budget.out_of_memory());
        }
        out.bytes.extend_from_slice(&bytes);
        let charged = self.tick(cost);
        self.lift(out.budget, charged)
    }

    /// What capture `i` of the match `s..e` puts in a replacement.
    fn capture_span(&self, i: usize, s: usize, e: usize) -> mlua::Result<CaptureText> {
        if i >= self.level {
            if i == 0 {
                return Ok(CaptureText::Span(s..e));
            }
            return Err(mlua::Error::runtime("invalid capture index"));
        }
        let Capture { init, len } = self.capture[i];
        match len {
            CaptureLen::Unfinished => Err(mlua::Error::runtime("unfinished capture")),
            CaptureLen::Position => Ok(CaptureText::Position(init + 1)),
            CaptureLen::Closed(len) => Ok(CaptureText::Span(init..init + len)),
        }
    }
}

/// `match_class` in the C locale. A class letter's capital negates it; other characters match
/// themselves.
fn match_class(c: u8, class: u8) -> bool {
    let res = match class.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => matches!(c, b' ' | 0x09..=0x0d),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        b'z' => c == 0,
        _ => return class == c,
    };
    if class.is_ascii_lowercase() { res } else { !res }
}

/// The pattern as C reads it: up to its first NUL.
fn until_nul(pattern: &[u8]) -> &[u8] {
    pattern
        .iter()
        .position(|&b| b == 0)
        .map_or(pattern, |end| &pattern[..end])
}

/// `posrelat` then the clamp of `str_find_aux`: the 0-based start of a search.
fn start_of(init: Option<f64>, len: usize) -> usize {
    let init = init.map_or(1, |n| n as i64);
    let len = i64::try_from(len).unwrap_or(i64::MAX);
    let at = if init < 0 {
        init.saturating_add(len + 1)
    } else {
        init
    };
    usize::try_from(at.max(0).saturating_sub(1).min(len)).unwrap_or(0)
}

/// `lmemfind`: where `needle` first occurs in `hay`. Charged per candidate, because a long needle
/// that almost matches everywhere costs as much as a backtracking pattern.
fn plain_find(hay: &[u8], needle: &[u8], budget: &Budget) -> mlua::Result<Option<usize>> {
    let Some((&first, rest)) = needle.split_first() else {
        return Ok(Some(0));
    };
    if needle.len() > hay.len() {
        return Ok(None);
    }
    let last = hay.len() - needle.len();
    let candidate_cost = 1 + rest.len() as u64 / BYTES_PER_STEP;
    let (mut steps, limit) = (0u64, budget.left().saturating_mul(STEPS_PER_INSTRUCTION));
    let mut found = None;
    for (i, _) in hay[..=last].iter().enumerate().filter(|(_, b)| **b == first) {
        steps = steps.saturating_add(candidate_cost);
        if steps > limit {
            break;
        }
        if &hay[i + 1..i + needle.len()] == rest {
            found = Some(i);
            break;
        }
    }
    budget.charge((steps + hay.len() as u64 / BYTES_PER_STEP).div_ceil(STEPS_PER_INSTRUCTION))?;
    Ok(found)
}

/// `str_find_aux`.
fn find_aux(
    lua: &Lua,
    budget: &Budget,
    find: bool,
    (subject, pattern, init, plain): (LuaString, LuaString, Option<f64>, Value),
) -> mlua::Result<MultiValue> {
    let (subject, pattern) = (subject.as_bytes(), pattern.as_bytes());
    let (src, whole_pattern) = (&subject[..], &pattern[..]);
    let pat = until_nul(whole_pattern);
    let init = start_of(init, src.len());
    let plain = !matches!(plain, Value::Nil | Value::Boolean(false));
    if find && (plain || !pat.iter().any(|b| SPECIALS.contains(b))) {
        let hit = plain_find(&src[init..], whole_pattern, budget)?;
        return Ok(match hit {
            Some(at) => MultiValue::from_vec(vec![
                Value::Number((init + at + 1) as f64),
                Value::Number((init + at + whole_pattern.len()) as f64),
            ]),
            None => MultiValue::from_vec(vec![Value::Nil]),
        });
    }
    let anchor = pat.first() == Some(&b'^');
    let pat = if anchor { &pat[1..] } else { pat };
    let mut m = Matcher::new(src, pat, budget);
    let mut s1 = init;
    loop {
        m.level = 0;
        let res = m.do_match(s1, 0);
        if let Some(end) = m.lift(budget, res)? {
            m.settle(budget)?;
            if !find {
                return Ok(MultiValue::from_vec(m.captures(lua, Some((s1, end)))?));
            }
            let mut out = vec![Value::Number((s1 + 1) as f64), Value::Number(end as f64)];
            out.extend(m.captures(lua, None)?);
            return Ok(MultiValue::from_vec(out));
        }
        let more = s1 < src.len() && !anchor;
        s1 += 1;
        if !more {
            break;
        }
    }
    m.settle(budget)?;
    Ok(MultiValue::from_vec(vec![Value::Nil]))
}

/// What `gsub` replaces a match with.
enum Replacement {
    Text(LuaString),
    Function(mlua::Function),
    Table(mlua::Table),
}

/// `str_gsub`. The result is built on the Rust heap, so it is capped at `max_bytes` (the state's
/// memory cap) and fails the same way the allocator would.
fn gsub(
    lua: &Lua,
    budget: &Budget,
    max_bytes: u64,
    (subject, pattern, replacement, max_subs): (LuaString, LuaString, Value, Option<f64>),
) -> mlua::Result<(Value, f64)> {
    let replacement = match replacement {
        Value::Function(f) => Replacement::Function(f),
        Value::Table(t) => Replacement::Table(t),
        other @ (Value::Integer(_) | Value::Number(_) | Value::String(_)) => {
            match lua.coerce_string(other)? {
                Some(s) => Replacement::Text(s),
                None => return Err(bad_replacement_type()),
            }
        }
        _ => return Err(bad_replacement_type()),
    };
    let (subject, pattern) = (subject.as_bytes(), pattern.as_bytes());
    let src = &subject[..];
    let pat = until_nul(&pattern);
    let max_subs = max_subs.map_or(
        i64::try_from(src.len()).unwrap_or(i64::MAX).saturating_add(1),
        // `luaL_optint`: the integer part narrowed through a 32-bit `int`.
        |n| i64::from(n as i64 as i32),
    );
    let anchor = pat.first() == Some(&b'^');
    let pat = if anchor { &pat[1..] } else { pat };
    let mut m = Matcher::new(src, pat, budget);
    let mut out = GsubOut {
        bytes: Vec::new(),
        max_bytes,
        budget,
    };
    let mut count = 0i64;
    let mut at = 0;
    while count < max_subs {
        m.level = 0;
        let res = m.do_match(at, 0);
        let end = m.lift(budget, res)?;
        if let Some(end) = end {
            count += 1;
            if let Err(error) = add_value(lua, &mut m, &replacement, at, end, &mut out) {
                m.settle(budget)?;
                return Err(error);
            }
        }
        match end {
            Some(end) if end > at => at = end,
            _ if at < src.len() => {
                out.bytes.push(src[at]);
                at += 1;
            }
            _ => break,
        }
        if anchor {
            break;
        }
    }
    m.append(CaptureText::Span(at..src.len()), &mut out)?;
    m.settle(budget)?;
    Ok((Value::String(lua.create_string(&out.bytes)?), count as f64))
}

fn bad_replacement_type() -> mlua::Error {
    mlua::Error::runtime("bad argument #3 to 'gsub' (string/function/table expected)")
}

/// `add_value`: appends what the match `s..e` becomes.
fn add_value(
    lua: &Lua,
    m: &mut Matcher<'_>,
    replacement: &Replacement,
    s: usize,
    e: usize,
    out: &mut GsubOut<'_>,
) -> mlua::Result<()> {
    let budget = out.budget;
    let value = match replacement {
        Replacement::Text(news) => return m.expand(&news.as_bytes(), s, e, out),
        Replacement::Function(f) => {
            // A callback runs VM instructions on the same budget.
            m.settle(budget)?;
            let args = MultiValue::from_vec(m.captures(lua, Some((s, e)))?);
            let value = f.call::<Value>(args)?;
            m.settle(budget)?;
            value
        }
        Replacement::Table(t) => t.get::<Value>(m.capture_value(lua, 0, s, e)?)?,
    };
    let text = match value {
        Value::Nil | Value::Boolean(false) => return m.append(CaptureText::Span(s..e), out),
        Value::String(text) => text,
        number @ (Value::Integer(_) | Value::Number(_)) => match lua.coerce_string(number)? {
            Some(text) => text,
            None => return Ok(()),
        },
        other => {
            return Err(mlua::Error::runtime(format!(
                "invalid replacement value (a {})",
                lua_type(&other)
            )));
        }
    };
    let text = text.as_bytes();
    if (out.bytes.len() + text.len()) as u64 > out.max_bytes {
        return Err(budget.out_of_memory());
    }
    out.bytes.extend_from_slice(&text);
    let charged = m.tick(text.len() as u64);
    m.lift(budget, charged)
}

/// `string.gmatch`: the iterator lives in Lua and asks Rust for one match at a time (ADR-1958). Its
/// subject stays a Lua string, so it counts against the state's memory cap: a Rust closure owning
/// the subject would keep it outside the cap, or hold a slot of the binding's small reference
/// stack, for as long as Lua kept the iterator.
const GMATCH: &str = r#"
local open, step = ...
return function(s, p)
  s, p = open(s, p)
  local at = 0
  local function took(next_at, ...)
    at = next_at
    return ...
  end
  return function() return took(step(s, p, at)) end
end
"#;

/// Bytes of a `gmatch` subject that cost one instruction when the iterator is made.
const GMATCH_OPEN_BYTES_PER_INSTRUCTION: u64 = 1024;

/// The next match at or after `start`: where the search goes on, then its captures. Only the
/// position when there are no more matches.
fn gmatch_step(
    lua: &Lua,
    budget: &Budget,
    (subject, pattern, mut start): (LuaString, LuaString, usize),
) -> mlua::Result<MultiValue> {
    let (subject, pattern) = (subject.as_bytes(), pattern.as_bytes());
    let src = &subject[..];
    let mut m = Matcher::new(src, &pattern, budget);
    while start <= src.len() {
        m.level = 0;
        let res = m.do_match(start, 0);
        if let Some(end) = m.lift(budget, res)? {
            m.settle(budget)?;
            let next = if end == start { end + 1 } else { end };
            let mut out = vec![Value::Number(next as f64)];
            out.extend(m.captures(lua, Some((start, end)))?);
            return Ok(MultiValue::from_vec(out));
        }
        start += 1;
    }
    m.settle(budget)?;
    Ok(MultiValue::from_vec(vec![Value::Number(start as f64)]))
}

/// The `gmatch` function: a Lua closure over [`GMATCH`] with two host functions.
fn gmatch(lua: &Lua, budget: &Arc<Budget>) -> mlua::Result<mlua::Function> {
    let open_budget = Arc::clone(budget);
    let open = lua.create_function(move |lua, (subject, pattern): (LuaString, LuaString)| {
        open_budget.charge(subject.as_bytes().len() as u64 / GMATCH_OPEN_BYTES_PER_INSTRUCTION)?;
        let cut = lua.create_string(until_nul(&pattern.as_bytes()))?;
        Ok((subject, cut))
    })?;
    let step_budget = Arc::clone(budget);
    let step = lua.create_function(move |lua, args| gmatch_step(lua, &step_budget, args))?;
    lua.load(GMATCH)
        .set_name("=regia.gmatch")
        .set_mode(mlua::chunk::ChunkMode::Text)
        .call::<mlua::Function>((open, step))
}

/// Replaces the C pattern functions in `string`. `string.gfind` is `gmatch` under its 5.0 name
/// and must not stay a way into the C matcher. `max_bytes` caps a `gsub` result.
pub(crate) fn install(
    lua: &Lua,
    string: &mlua::Table,
    budget: &Arc<Budget>,
    max_bytes: u64,
) -> mlua::Result<()> {
    for (name, find) in [("find", true), ("match", false)] {
        let budget = Arc::clone(budget);
        string.raw_set(
            name,
            lua.create_function(move |lua, args| find_aux(lua, &budget, find, args))?,
        )?;
    }
    let gsub_budget = Arc::clone(budget);
    string.raw_set(
        "gsub",
        lua.create_function(move |lua, args| gsub(lua, &gsub_budget, max_bytes, args))?,
    )?;
    let iterator = gmatch(lua, budget)?;
    string.raw_set("gmatch", &iterator)?;
    string.raw_set("gfind", iterator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::{LuaOptions, StdLib};

    /// Runs every case through the C functions and this port in one plain state and fails on the
    /// first difference in results, or in an error's text (without its position prefix).
    const PARITY_DRIVER: &str = r#"
local cases, c_find, c_match, c_gmatch, c_gsub = ...
-- A Rust error reaches pcall as "runtime error: <text>" plus a traceback; a C one as the text,
-- with its position when a Lua function called it.
local function strip(msg)
  local text = c_gsub(tostring(msg), "\nstack traceback:.*$", "")
  text = c_gsub(text, "^runtime error: ", "")
  text = c_gsub(text, "^[^:]*:%d+: ", "")
  return text
end
local function run(f, ...)
  local r = { pcall(f, ...) }
  if not r[1] then return "error: " .. strip(r[2]) end
  local parts = {}
  for i = 2, table.maxn(r) do parts[#parts + 1] = type(r[i]) .. ":" .. tostring(r[i]) end
  return table.concat(parts, "|")
end
local function all_gmatch(g, s, p)
  local out, n = {}, 0
  for a, b, c in g(s, p) do
    n = n + 1
    if n > 64 then break end
    out[#out + 1] = tostring(a) .. "," .. tostring(b) .. "," .. tostring(c)
  end
  return table.concat(out, ";")
end
local repl_table = { a = "1", b = false, ab = 2.5, x = "%1" }
local function repl_function(a, b) if a == "b" then return nil end return tostring(a) .. "/" .. tostring(b) end
local checked = 0
local function same(what, s, p, a, b)
  if a ~= b then
    error(what .. " differs on subject " .. string.format("%q", s) .. " pattern " ..
      string.format("%q", p) .. "\n  C:    " .. a .. "\n  port: " .. b, 0)
  end
  checked = checked + 1
end
for _, case in ipairs(cases) do
  local s, p = case[1], case[2]
  for _, init in ipairs({ false, 1, 2, -1, -100, 3, 100, 0 }) do
    local i = init or nil
    same("find", s, p, run(c_find, s, p, i), run(string.find, s, p, i))
    same("find plain", s, p, run(c_find, s, p, i, true), run(string.find, s, p, i, true))
    same("match", s, p, run(c_match, s, p, i), run(string.match, s, p, i))
  end
  same("gmatch", s, p, run(all_gmatch, c_gmatch, s, p), run(all_gmatch, string.gmatch, s, p))
  for _, r in ipairs({ "<%0>", "%1", "%2", "x%%y", "%", 12, repl_table, repl_function, "" }) do
    for _, n in ipairs({ false, 0, 1, 2 }) do
      local m = n or nil
      same("gsub", s, p, run(c_gsub, s, p, r, m), run(string.gsub, s, p, r, m))
    end
  end
end
return checked
"#;

    /// A plain state: the C functions are handed to the driver and `string` gets the port.
    fn parity(cases: &[(Vec<u8>, Vec<u8>)]) -> u64 {
        let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE, LuaOptions::default()).expect("lua");
        let globals = lua.globals();
        let string: mlua::Table = globals.get("string").expect("string");
        let originals: Vec<mlua::Function> = ["find", "match", "gmatch", "gsub"]
            .iter()
            .map(|n| string.get(*n).expect("c function"))
            .collect();
        let budget = Arc::new(Budget::default());
        budget.begin(u64::MAX / 8);
        install(&lua, &string, &budget, 1 << 24).expect("install");
        let table = lua.create_table().expect("table");
        for (i, (s, p)) in cases.iter().enumerate() {
            let case = lua.create_table().expect("case");
            case.raw_set(1, lua.create_string(s).expect("subject"))
                .expect("set");
            case.raw_set(2, lua.create_string(p).expect("pattern"))
                .expect("set");
            table.raw_set(i + 1, case).expect("set");
        }
        lua.load(PARITY_DRIVER)
            .set_name("=driver")
            .call::<u64>((
                table,
                originals[0].clone(),
                originals[1].clone(),
                originals[2].clone(),
                originals[3].clone(),
            ))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_port_answers_what_the_c_matcher_answers_on_a_hand_written_corpus() {
        let subjects: [&[u8]; 12] = [
            b"",
            b"a",
            b"hello world from Lua",
            b"THE (quick) fox",
            b"key = value, other=2",
            b"  trim me  ",
            b"f(a(b)c)d",
            b"THE (quick) fox\n\tsecond\0line",
            b"0x1F 42 -7 3.25e2",
            b"aaa bbb aaa",
            b"[[x]]%y^z$",
            b"\xff\x80 high bytes \x01",
        ];
        let patterns: [&[u8]; 55] = [
            b"",
            b"a",
            b"hello",
            b"l+",
            b"l*",
            b"l-",
            b"o?",
            b"^h",
            b"^",
            b"$",
            b"d$",
            b"a$b",
            b".",
            b".-",
            b".*",
            b"%a+",
            b"%A+",
            b"%d+",
            b"%D",
            b"%l+",
            b"%u+",
            b"%s+",
            b"%S+",
            b"%p",
            b"%w+",
            b"%x+",
            b"%c",
            b"%z",
            b"[abc]+",
            b"[^abc]+",
            b"[a-c]+",
            b"[%a_]+",
            b"[]]",
            b"[^]]",
            b"[a-]",
            b"()",
            b"()l()",
            b"(h)(e)",
            b"((a)(b))",
            b"(.)%1",
            b"(a*)(b*)%2",
            b"%b()",
            b"%f[%a]%a+",
            b"%f[%z]",
            b"%f[%w]",
            b"%(",
            b"%",
            b"[a",
            b"(",
            b")",
            b"%1",
            b"%f",
            b"%b",
            // Audit wave (script_lua correctness): a special after an embedded NUL. Lua 5.1's
            // `nospecials` walks every NUL-separated segment, so these never take the plain
            // search; the matcher itself stops at the first NUL.
            b"a\0.",
            b"l\0+",
        ];
        let cases: Vec<_> = subjects
            .iter()
            .flat_map(|s| patterns.iter().map(|p| (s.to_vec(), p.to_vec())))
            .collect();
        // 61 comparisons per case.
        assert_eq!(parity(&cases), 61 * cases.len() as u64);
    }

    #[test]
    fn the_port_answers_what_the_c_matcher_answers_on_generated_patterns() {
        // A fixed LCG, so a failure reproduces.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 33) % n
        };
        let atoms: [&[u8]; 26] = [
            b"a", b"b", b".", b"%a", b"%d", b"%s", b"[ab]", b"[^a]", b"[a-c]", b"(", b")", b"()", b"*", b"+",
            b"-", b"?", b"^", b"$", b"%1", b"%2", b"%b()", b"%f[a]", b"%", b"[", b"]", b"%z",
        ];
        let letters = b"aabbc1 ()\0";
        let mut cases = Vec::new();
        for _ in 0..4000 {
            let subject: Vec<u8> = (0..next(14)).map(|_| letters[next(10) as usize]).collect();
            let pattern: Vec<u8> = (0..next(7))
                .flat_map(|_| atoms[next(26) as usize].iter().copied())
                .collect();
            cases.push((subject, pattern));
        }
        assert!(parity(&cases) > 100_000);
    }

    fn sandbox(budget: u64) -> crate::sandbox::Sandbox {
        let limits = crate::sandbox::LuaLimits {
            instruction_budget: budget,
            ..crate::sandbox::LuaLimits::default()
        };
        let sandbox = crate::sandbox::new_state(&limits).expect("state");
        sandbox.budget.begin(budget);
        sandbox
    }

    #[test]
    fn a_backtracking_bomb_runs_out_of_budget_inside_one_call() {
        for chunk in [
            r#"return string.find(("a"):rep(60), ("a?"):rep(60) .. "b")"#,
            r#"return string.find(("a"):rep(5000), "^" .. ("a-"):rep(30) .. "b")"#,
            r#"return string.match(("a"):rep(5000), (".-"):rep(30) .. "b")"#,
            r#"return string.gsub(("a"):rep(5000), (".-"):rep(30) .. "b", "")"#,
            r#"for x in string.gmatch(("a"):rep(5000), (".-"):rep(30) .. "b") do end"#,
            r#"for x in string.gfind(("a"):rep(5000), (".-"):rep(30) .. "b") do end"#,
            // pcall does not swallow it.
            r#"return pcall(string.find, ("a"):rep(60), ("a?"):rep(60) .. "b")"#,
        ] {
            let sandbox = sandbox(100_000);
            let result = sandbox.lua.load(chunk).exec();
            assert!(result.is_err(), "{chunk} must not finish");
            assert!(sandbox.budget.tripped(), "{chunk} must trip the budget");
        }
    }

    #[test]
    fn a_long_plain_needle_that_almost_matches_is_charged_per_candidate() {
        let sandbox = sandbox(100_000);
        let chunk = r#"return string.find(("a"):rep(4000000), ("a"):rep(2000000) .. "b", 1, true)"#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.tripped());
    }

    #[test]
    fn a_pattern_past_the_depth_limit_is_an_error_and_not_a_stack_overflow() {
        let sandbox = sandbox(50_000_000);
        let chunk = r#"
            local ok, message = pcall(string.find, ("a"):rep(1000), ("a?"):rep(100000))
            return ok, tostring(message)
        "#;
        let (ok, message): (bool, String) = sandbox.lua.load(chunk).eval().expect("pcall returns");
        assert!(!ok);
        assert!(message.contains("pattern too complex"), "{message}");
        assert!(!sandbox.budget.tripped());
    }

    #[test]
    fn a_gsub_result_past_the_memory_cap_is_a_memory_trip() {
        let sandbox = sandbox(50_000_000);
        // 100 matches of a 512 KiB replacement: 50 MiB of Rust heap under a 16 MiB cap.
        let chunk = r#"return string.gsub(("a"):rep(100), "a", ("b"):rep(524288))"#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.memory_tripped());
    }

    #[test]
    fn ordinary_matching_costs_a_fraction_of_the_budget() {
        let sandbox = sandbox(1_000_000);
        let chunk = r#"
            local n = 0
            for word in ("the quick brown fox "):rep(1000):gmatch("%a+") do n = n + 1 end
            return n, (("k=v, "):rep(1000):gsub("(%w+)=(%w+)", "%2=%1"))
        "#;
        let (n, _text): (u32, String) = sandbox.lua.load(chunk).eval().expect("under budget");
        assert_eq!(n, 4000);
        assert!(!sandbox.budget.tripped());
        assert!(sandbox.budget.used() < 200_000, "{}", sandbox.budget.used());
    }

    #[test]
    fn a_replacement_that_names_a_long_match_many_times_is_stopped_while_it_grows() {
        // One match of 1 MiB, 2^20 times over: a terabyte if the result were built first.
        let sandbox = sandbox(50_000_000);
        let chunk = r#"return string.gsub(("a"):rep(1048576), "^.*", ("%0"):rep(1048576))"#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.tripped() || sandbox.budget.memory_tripped());
    }

    #[test]
    fn a_short_replacement_cannot_loop_for_free_over_empty_matches() {
        // Every empty match walks 2^20 replacement bytes and adds nothing.
        let sandbox = sandbox(100_000);
        let chunk = r#"return string.gsub(("a"):rep(1048576), "", ("%0"):rep(524288))"#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.tripped());
    }

    #[test]
    fn a_pattern_that_fails_late_still_pays_for_what_it_walked() {
        let sandbox = sandbox(100_000);
        // About 45,000 steps a call, under the limit, so only the charge can stop the loop.
        let chunk = r#"
            local s = ("a"):rep(300) .. "bc"
            n = 0
            while true do n = n + 1; pcall(string.find, s, "a-c%") end
        "#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.tripped());
        let calls: u32 = sandbox.lua.globals().get("n").expect("n");
        assert!(calls < 100, "{calls}");
    }

    #[test]
    fn a_failed_replacement_still_pays_for_the_match() {
        let sandbox = sandbox(100_000);
        let chunk = r#"
            local s = ("a"):rep(5000)
            n = 0
            while true do n = n + 1; pcall(string.gsub, s, "a*", "%9") end
        "#;
        assert!(sandbox.lua.load(chunk).exec().is_err());
        assert!(sandbox.budget.tripped());
        let calls: u32 = sandbox.lua.globals().get("n").expect("n");
        assert!(calls < 300, "{calls}");
    }

    #[test]
    fn an_embedded_nul_is_the_end_of_the_pattern_like_in_the_c_matcher() {
        // Lua 5.1.5 `lstrlib.c`: `str_find_aux` decides the plain search with
        // `strpbrk(p, SPECIALS)`, which reads only up to the pattern's first NUL, and the plain
        // search (`lmemfind`) then scans the whole pattern bytes. The matcher's `case '\0'`
        // ends the pattern with success. An audit once "fixed" these to 5.2+ semantics and the
        // parity corpus caught it; these cases pin 5.1.5.
        let sandbox = sandbox(1_000_000);
        let chunk = r#"
            local a, b = string.find("a", "a\0."), string.find("a\0.x", "a\0.")
            local c = string.match("a\0.x", "a\0.")
            local d, e = ("a\0x"):gsub("a\0", "b")
            local f, g = string.find("a\0.x", "a\0.", 1, true)
            return a, b, c, d, e, f, g
        "#;
        let (a, b, c, d, e, f, g): (mlua::Value, f64, mlua::LuaString, mlua::LuaString, f64, f64, f64) =
            sandbox.lua.load(chunk).eval().expect("eval");
        assert!(matches!(a, mlua::Value::Nil), "the NUL item is not in \"a\"");
        assert_eq!(b, 1.0, "find ends past the NUL matched by the plain search");
        assert_eq!(c.as_bytes(), b"a", "the matcher ends at the pattern's NUL");
        assert_eq!(d.as_bytes(), b"b\0x", "gsub cuts the pattern at the NUL too");
        assert_eq!(e, 1.0);
        assert_eq!(
            (f, g),
            (1.0, 3.0),
            "an explicit plain search uses the whole pattern"
        );
    }

    #[test]
    fn no_c_pattern_function_stays_reachable() {
        let sandbox = sandbox(1_000_000);
        // The depth limit is the port's alone: the C matcher takes these 300 items in stride. Each
        // name, and the method form through the string metatable, must fail with it.
        for call in [
            "string.find(s, p)",
            "string.match(s, p)",
            "string.gsub(s, p, '')",
            "string.gmatch(s, p)()",
            "string.gfind(s, p)()",
            "s:find(p)",
            "s:match(p)",
            "s:gsub(p, '')",
            "s:gmatch(p)()",
        ] {
            let chunk = format!(
                r#"
                local s, p = ("a"):rep(300), ("a?"):rep(300)
                local ok, message = pcall(function() return {call} end)
                return ok, tostring(message)
                "#
            );
            let (ok, message): (bool, String) =
                sandbox.lua.load(chunk.as_str()).eval().expect("pcall returns");
            assert!(
                !ok && message.contains("pattern too complex"),
                "{call}: {message}"
            );
        }
    }
}
