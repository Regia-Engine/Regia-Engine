// SPDX-License-Identifier: MPL-2.0
//! `spline.modules`: modules along a path (spec 64 § 5).
//!
//! A pattern (Regia's own language) names modules — meshes or prefabs with a length along the
//! path — and four functions: `repeat(seq)` (as many as fit), `repeat(seq, n)`, `first(a, b, ...)`
//! (the first alternative that fits), `pick(a: w, b: w, ...)` (weighted random) and `gap(m)`.
//! Fitting is deterministic per spline: counted repeats expand first, then terms resolve left to
//! right against the remaining length minus the fixed length still to come (each `pick` draws from
//! the spline item's seed and its position in the expansion when reached), then the leftover is
//! spread by `fit`.

use std::collections::BTreeMap;

use regia_core::math::Vec3;

use crate::data::{AttrColumn, Bundle, DataKind, KindSet, Payload, Point, PointSet};
use crate::exec::NodeCtx;
use crate::nodes::spline::{SPLINE, frame_rot};
use crate::nodes::{
    BuiltinNode, NodeError, POINTS, PinSpec, SettingType as T, Settings, input, pin_in, pin_out, pin_req, set,
};
use crate::rot::Rot;
use crate::value::SettingValue;

/// Most modules one spline may get.
const MAX_MODULES: usize = 100_000;

/// Most terms a pattern may expand to once its counted repeats are written out (phase A): four
/// times `MAX_MODULES` (100 000, the most modules one spline may get). Every copy counts what
/// phase A keeps of it: a plain module 1, a `pick` or `first` of k alternatives 1 + the terms of
/// all k, an uncounted `repeat` 1 + its body. So a counted repeat of a `pick` or `first` with
/// three or more one-module alternatives is refused below `MAX_MODULES` resolved modules
/// (`repeat(pick(a: 1, b: 1, c: 1, d: 1), 100000)` counts 500 000), while a pattern of plain
/// modules fits up to `MAX_EXPANDED_TERMS`. The bound is on phase A memory, not on the result.
pub const MAX_EXPANDED_TERMS: usize = 4 * MAX_MODULES;

/// The family table.
pub(crate) const NODES: &[BuiltinNode] = &[BuiltinNode::new(
    "spline.modules",
    "spline",
    "Fills each spline with a pattern of modules (posts, panels, gates); one point per module at its start",
    &[
        set(
            "modules",
            T::Any,
            "{}",
            "Map name → (mesh | prefab, length_m, stretch: (lo, hi))",
        ),
        set(
            "pattern",
            T::Any,
            "\"\"",
            "The pattern, e.g. `post repeat(panel post) end`",
        ),
        set(
            "pattern_from",
            T::Name,
            "\"\"",
            "Item-scope Name value holding the pattern (overrides `pattern`)",
        ),
        set(
            "fit",
            T::Enum(&["Stretch", "Scale", "Gap"]),
            "Stretch",
            "Where the leftover length goes",
        ),
        set(
            "align",
            T::Enum(&["Curve", "Chord"]),
            "Curve",
            "Module axis along the tangent or the chord",
        ),
        set(
            "forward_axis",
            T::Enum(&["X", "Z"]),
            "X",
            "The module's length axis in mesh space",
        ),
        set(
            "follow_ground",
            T::Bool,
            "true",
            "Drop module ends onto the `ground` input",
        ),
    ],
    |_| {
        PinSpec::new(
            vec![
                pin_req("in", SPLINE),
                pin_in("ground", KindSet(1 << DataKind::Ground as u16)),
            ],
            vec![pin_out("out", POINTS)],
        )
    },
    eval_modules,
)
.checked(check_modules)];

/// One module definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDef {
    /// Mesh asset (empty when a prefab).
    pub mesh: String,
    /// Prefab asset (empty when a mesh).
    pub prefab: String,
    /// Nominal length along the path, meters.
    pub length_m: f64,
    /// Allowed stretch factors `(lo, hi)`.
    pub stretch: (f64, f64),
}

/// A pattern term.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    /// A module by name.
    Module(String),
    /// `repeat(seq)` (`None`) or `repeat(seq, n)`.
    Repeat(Vec<Term>, Option<u32>),
    /// `first(a, b, ...)`.
    First(Vec<Vec<Term>>),
    /// `pick(a: w, ...)`.
    Pick(Vec<(Vec<Term>, f64)>),
    /// `gap(m)`.
    Gap(f64),
}

/// A pattern error: a stable code, a message and the 1-based column (0 when not positional).
#[derive(Clone, Debug, PartialEq)]
pub struct PatternError {
    /// `pattern_syntax`, `pattern_unknown_module`, `pattern_nesting` or `pattern_too_large`.
    pub code: &'static str,
    /// Detail.
    pub message: String,
    /// 1-based column of the problem.
    pub column: usize,
}

impl PatternError {
    fn at(code: &'static str, column: usize, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            column,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    repeat_depth: u32,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn col(&self) -> usize {
        self.i + 1
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), PatternError> {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(PatternError::at(
                "pattern_syntax",
                self.col(),
                format!("expected `{}`", c as char),
            ))
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            self.i += 1;
        }
        (self.i > start).then(|| String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn number(&mut self) -> Result<f64, PatternError> {
        self.ws();
        let (start, col) = (self.i, self.col());
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || c == b'.' || c == b'-' || c == b'e' || c == b'+')
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i])
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or_else(|| PatternError::at("pattern_syntax", col, "expected a number"))
    }

    /// A sequence of terms up to `)`, `,`, `:` or the end.
    fn seq(&mut self) -> Result<Vec<Term>, PatternError> {
        let mut out = Vec::new();
        loop {
            self.ws();
            match self.peek() {
                None | Some(b')' | b',' | b':') => break,
                _ => out.push(self.term()?),
            }
        }
        Ok(out)
    }

    fn term(&mut self) -> Result<Term, PatternError> {
        let col = self.col();
        let Some(name) = self.ident() else {
            return Err(PatternError::at(
                "pattern_syntax",
                col,
                "expected a module name or a function",
            ));
        };
        if self.peek() != Some(b'(') {
            return Ok(Term::Module(name));
        }
        self.i += 1;
        let term = match name.as_str() {
            "repeat" => {
                if self.repeat_depth >= 2 {
                    return Err(PatternError::at(
                        "pattern_nesting",
                        col,
                        "repeat nests one level deep at most",
                    ));
                }
                self.repeat_depth += 1;
                let seq = self.seq()?;
                self.repeat_depth -= 1;
                self.ws();
                let n = if self.peek() == Some(b',') {
                    self.i += 1;
                    let ncol = self.col();
                    let v = self.number()?;
                    if v < 0.0 || v.fract() != 0.0 || v > 1.0e6 {
                        return Err(PatternError::at(
                            "pattern_syntax",
                            ncol,
                            "repeat count must be a whole number",
                        ));
                    }
                    Some(v as u32)
                } else {
                    None
                };
                if seq.is_empty() {
                    return Err(PatternError::at("pattern_syntax", col, "repeat needs a sequence"));
                }
                Term::Repeat(seq, n)
            }
            "first" => {
                let mut alts = vec![self.seq()?];
                self.ws();
                while self.peek() == Some(b',') {
                    self.i += 1;
                    alts.push(self.seq()?);
                    self.ws();
                }
                Term::First(alts)
            }
            "pick" => {
                let mut alts = Vec::new();
                loop {
                    let seq = self.seq()?;
                    self.eat(b':')?;
                    let w = self.number()?;
                    alts.push((seq, w.max(0.0)));
                    self.ws();
                    if self.peek() == Some(b',') {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Term::Pick(alts)
            }
            "gap" => Term::Gap(self.number()?.max(0.0)),
            other => {
                return Err(PatternError::at(
                    "pattern_syntax",
                    col,
                    format!("unknown function `{other}`"),
                ));
            }
        };
        self.eat(b')')?;
        Ok(term)
    }
}

/// Parses a pattern.
///
/// # Errors
///
/// `pattern_syntax` (with the column) or `pattern_nesting`.
pub fn parse_pattern(text: &str) -> Result<Vec<Term>, PatternError> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        repeat_depth: 0,
    };
    let seq = p.seq()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(PatternError::at(
            "pattern_syntax",
            p.col(),
            "unexpected character",
        ));
    }
    Ok(seq)
}

/// The number of terms `terms` expands to when every counted repeat is written out (what phase A
/// would build), saturating; counted without building anything.
pub fn expanded_terms(terms: &[Term]) -> usize {
    terms.iter().fold(0usize, |acc, t| {
        let n = match t {
            Term::Module(_) | Term::Gap(_) => 1,
            Term::Repeat(seq, Some(n)) => (*n as usize).saturating_mul(expanded_terms(seq)),
            Term::Repeat(seq, None) => expanded_terms(seq).saturating_add(1),
            Term::First(alts) => alts
                .iter()
                .fold(1usize, |a, s| a.saturating_add(expanded_terms(s))),
            Term::Pick(alts) => alts
                .iter()
                .fold(1usize, |a, (s, _)| a.saturating_add(expanded_terms(s))),
        };
        acc.saturating_add(n)
    })
}

/// Refuses a pattern whose counted repeats expand past [`MAX_EXPANDED_TERMS`].
///
/// # Errors
///
/// `pattern_too_large` with the expanded size.
pub fn check_pattern_size(terms: &[Term]) -> Result<(), PatternError> {
    let n = expanded_terms(terms);
    if n > MAX_EXPANDED_TERMS {
        return Err(PatternError::at(
            "pattern_too_large",
            0,
            format!("the counted repeats expand to {n} terms, more than {MAX_EXPANDED_TERMS}"),
        ));
    }
    Ok(())
}

fn check_names(terms: &[Term], defs: &BTreeMap<String, ModuleDef>) -> Result<(), PatternError> {
    for t in terms {
        match t {
            Term::Module(n) if !defs.contains_key(n) => {
                return Err(PatternError::at(
                    "pattern_unknown_module",
                    0,
                    format!("unknown module `{n}`"),
                ));
            }
            Term::Repeat(seq, _) => check_names(seq, defs)?,
            Term::First(alts) => alts.iter().try_for_each(|a| check_names(a, defs))?,
            Term::Pick(alts) => alts.iter().try_for_each(|(a, _)| check_names(a, defs))?,
            _ => {}
        }
    }
    Ok(())
}

/// Reads the `modules` setting.
///
/// # Errors
///
/// `setting_invalid` text for a malformed entry.
pub fn parse_modules(v: Option<&SettingValue>) -> Result<BTreeMap<String, ModuleDef>, String> {
    let mut out = BTreeMap::new();
    let Some(SettingValue::Map(entries)) = v else {
        return match v {
            None => Ok(out),
            Some(SettingValue::List(l)) if l.is_empty() => Ok(out),
            _ => Err("`modules` must be a map".to_string()),
        };
    };
    for (k, d) in entries {
        let name = k.as_text().ok_or("module names must be strings")?.to_string();
        let text = |f: &str| {
            d.field(f)
                .and_then(SettingValue::as_text)
                .unwrap_or_default()
                .to_string()
        };
        let length_m = d
            .field("length_m")
            .and_then(SettingValue::as_f64)
            .filter(|l| l.is_finite() && *l >= 0.0)
            .ok_or_else(|| format!("module `{name}` needs length_m >= 0"))?;
        let stretch = match d.field("stretch").and_then(SettingValue::as_items) {
            Some([lo, hi]) => (lo.as_f64().unwrap_or(1.0), hi.as_f64().unwrap_or(1.0)),
            _ => (1.0, 1.0),
        };
        if !(stretch.0 > 0.0 && stretch.0 <= 1.0 && stretch.1 >= 1.0 && stretch.1.is_finite()) {
            return Err(format!(
                "module `{name}` stretch must be (lo ≤ 1, hi ≥ 1), lo > 0"
            ));
        }
        let (mesh, prefab) = (text("mesh"), text("prefab"));
        if mesh.is_empty() == prefab.is_empty() {
            return Err(format!("module `{name}` needs exactly one of mesh or prefab"));
        }
        out.insert(
            name,
            ModuleDef {
                mesh,
                prefab,
                length_m,
                stretch,
            },
        );
    }
    Ok(out)
}

fn check_modules(s: &Settings) -> Vec<(String, String)> {
    let defs = match parse_modules(s.get("modules")) {
        Ok(d) => d,
        Err(e) => return vec![("setting_invalid".to_string(), e)],
    };
    if !s.text("pattern_from").is_empty() {
        return Vec::new();
    }
    match parse_pattern(s.text("pattern")).and_then(|t| {
        check_names(&t, &defs)?;
        check_pattern_size(&t)
    }) {
        Ok(()) => Vec::new(),
        Err(e) => vec![(e.code.to_string(), format!("{} (column {})", e.message, e.column))],
    }
}

/// One placement: a module index into the name list, or a gap.
#[derive(Clone, Debug, PartialEq)]
enum Slot {
    Module(String),
    Gap(f64),
}

struct Fitter<'a> {
    defs: &'a BTreeMap<String, ModuleDef>,
    seed: u32,
    draws: u32,
}

impl Fitter<'_> {
    fn nominal(&self, t: &Term) -> f64 {
        match t {
            Term::Module(n) => self.defs.get(n).map_or(0.0, |d| d.length_m),
            Term::Gap(m) => *m,
            Term::Repeat(_, None) => 0.0,
            Term::Repeat(seq, Some(n)) => f64::from(*n) * self.seq_nominal(seq),
            Term::First(alts) => alts
                .iter()
                .map(|a| self.seq_nominal(a))
                .fold(f64::INFINITY, f64::min),
            Term::Pick(alts) => alts
                .iter()
                .map(|(a, _)| self.seq_nominal(a))
                .fold(f64::INFINITY, f64::min),
        }
    }

    fn seq_nominal(&self, seq: &[Term]) -> f64 {
        seq.iter().map(|t| self.nominal(t)).sum::<f64>()
    }

    /// The minimum length (stretchable modules at their lower stretch).
    fn minimum(&self, t: &Term) -> f64 {
        match t {
            Term::Module(n) => self.defs.get(n).map_or(0.0, |d| d.length_m * d.stretch.0),
            Term::Gap(m) => *m,
            Term::Repeat(_, None) => 0.0,
            Term::Repeat(seq, Some(n)) => f64::from(*n) * seq.iter().map(|t| self.minimum(t)).sum::<f64>(),
            Term::First(alts) => alts
                .iter()
                .map(|a| a.iter().map(|t| self.minimum(t)).sum::<f64>())
                .fold(f64::INFINITY, f64::min),
            Term::Pick(alts) => alts
                .iter()
                .map(|(a, _)| a.iter().map(|t| self.minimum(t)).sum::<f64>())
                .fold(f64::INFINITY, f64::min),
        }
    }

    /// Phase A: expands counted repeats (picks stay for phase B). The caller bounds the result with
    /// [`check_pattern_size`] first, so this never builds more than [`MAX_EXPANDED_TERMS`] terms.
    fn expand(&mut self, terms: &[Term]) -> Vec<Term> {
        let mut out = Vec::new();
        for t in terms {
            match t {
                Term::Pick(alts) => out.push(Term::Pick(
                    alts.iter().map(|(a, w)| (self.expand(a), *w)).collect(),
                )),
                Term::Repeat(seq, Some(n)) => {
                    for _ in 0..*n {
                        out.extend(self.expand(seq));
                    }
                }
                Term::Repeat(seq, None) => out.push(Term::Repeat(self.expand(seq), None)),
                Term::First(alts) => out.push(Term::First(alts.iter().map(|a| self.expand(a)).collect())),
                other => out.push(other.clone()),
            }
        }
        out
    }

    /// A weighted choice for the `pick` at expansion position `self.draws`.
    fn choose(&mut self, alts: &[(Vec<Term>, f64)], ctx: &NodeCtx<'_>) -> Vec<Term> {
        let total: f64 = alts.iter().map(|(_, w)| w).sum();
        self.draws += 1;
        let r = ctx.draw(self.seed, self.draws) * total;
        let mut acc = 0.0;
        for (a, w) in alts {
            acc += w;
            if r < acc {
                return a.clone();
            }
        }
        alts.last().map(|(a, _)| a.clone()).unwrap_or_default()
    }

    /// The fixed length still to come after term `t`. Only `repeat(seq)` and `first` read it, so
    /// it is summed for those alone (a long run of plain modules stays linear).
    fn rest(&self, terms: &[Term], t: usize) -> f64 {
        terms[t + 1..].iter().map(|x| self.nominal(x)).sum()
    }

    /// Phase B: resolves `pick`, `repeat(seq)` and `first` left to right against `len`; each
    /// `pick` draws when it is reached, so every repeated copy draws on its own.
    fn resolve(&mut self, mut terms: Vec<Term>, len: f64, ctx: &NodeCtx<'_>) -> Result<Vec<Slot>, NodeError> {
        let mut out = Vec::new();
        let mut remaining = len;
        let mut t = 0;
        while t < terms.len() {
            match terms[t].clone() {
                Term::Module(n) => {
                    remaining -= self.nominal(&terms[t]);
                    out.push(Slot::Module(n));
                    t += 1;
                }
                Term::Gap(m) => {
                    remaining -= m;
                    out.push(Slot::Gap(m));
                    t += 1;
                }
                Term::Repeat(seq, _) => {
                    let rest = self.rest(&terms, t);
                    let unit = self.seq_nominal(&seq);
                    let k = if unit > 1e-9 {
                        ((remaining - rest) / unit + 1e-9).floor().max(0.0) as usize
                    } else {
                        0
                    };
                    if k.saturating_mul(seq.len()) > MAX_MODULES {
                        return Err(NodeError::new(
                            "limit_exceeded",
                            "a pattern expands to more than 100 000 modules",
                        ));
                    }
                    let copies: Vec<Term> = (0..k).flat_map(|_| seq.iter().cloned()).collect();
                    terms.splice(t..=t, copies);
                }
                Term::First(alts) => {
                    let rest = self.rest(&terms, t);
                    let pick = alts
                        .iter()
                        .find(|a| self.seq_nominal(a) <= remaining - rest + 1e-9)
                        .cloned()
                        .unwrap_or_default();
                    terms.splice(t..=t, pick);
                }
                Term::Pick(alts) => {
                    let chosen = self.choose(&alts, ctx);
                    terms.splice(t..=t, chosen);
                }
            }
            if out.len() > MAX_MODULES {
                return Err(NodeError::new(
                    "limit_exceeded",
                    "a pattern expands to more than 100 000 modules",
                ));
            }
        }
        Ok(out)
    }
}

/// The fit of `slots` into `len`: slot lengths, stretch factors and extra space before each slot.
fn fit_lengths(
    slots: &[Slot],
    defs: &BTreeMap<String, ModuleDef>,
    len: f64,
    fit: &str,
) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let nominal: Vec<f64> = slots
        .iter()
        .map(|s| match s {
            Slot::Module(n) => defs.get(n).map_or(0.0, |d| d.length_m),
            Slot::Gap(m) => *m,
        })
        .collect();
    let placed: f64 = nominal.iter().sum();
    let mut lens = nominal.clone();
    let mut factors = vec![1.0; slots.len()];
    let mut before = vec![0.0; slots.len()];
    let mut left = len - placed;
    match fit {
        "Scale" => {
            if placed > 0.0 {
                let f = len / placed;
                lens.iter_mut().for_each(|l| *l *= f);
                factors.iter_mut().for_each(|x| *x = f);
            }
            return (lens, factors, before);
        }
        "Gap" => {}
        _ => {
            // Stretch: spread over stretchable modules in proportion to length, within range.
            for _ in 0..4 {
                if left.abs() < 1e-12 {
                    break;
                }
                let free: Vec<usize> = (0..slots.len())
                    .filter(|&i| match &slots[i] {
                        Slot::Module(n) => defs.get(n).is_some_and(|d| {
                            let f = lens[i] / d.length_m.max(1e-12);
                            d.length_m > 0.0
                                && if left > 0.0 {
                                    f < d.stretch.1 - 1e-12
                                } else {
                                    f > d.stretch.0 + 1e-12
                                }
                        }),
                        Slot::Gap(_) => false,
                    })
                    .collect();
                let total: f64 = free.iter().map(|&i| nominal[i]).sum();
                if free.is_empty() || total <= 0.0 {
                    break;
                }
                let mut used = 0.0;
                for &i in &free {
                    let Slot::Module(n) = &slots[i] else { continue };
                    let Some(d) = defs.get(n) else { continue };
                    let want = lens[i] + left * nominal[i] / total;
                    let got = want.clamp(d.length_m * d.stretch.0, d.length_m * d.stretch.1);
                    used += got - lens[i];
                    lens[i] = got;
                    factors[i] = got / d.length_m;
                }
                left -= used;
            }
        }
    }
    // What is left becomes equal gaps between modules.
    if left > 1e-12 {
        let modules: Vec<usize> = (0..slots.len())
            .filter(|&i| matches!(slots[i], Slot::Module(_)))
            .collect();
        if modules.len() > 1 {
            let g = left / (modules.len() - 1) as f64;
            for &i in &modules[1..] {
                before[i] = g;
            }
        }
    }
    (lens, factors, before)
}

fn eval_modules(ctx: &mut NodeCtx<'_>, inputs: &[Bundle], s: &Settings) -> Result<Vec<Bundle>, NodeError> {
    let _span = tracing::info_span!(target: "regia_procgen", "procgen.spline_modules").entered();
    let defs = parse_modules(s.get("modules")).map_err(|e| NodeError::new("setting_invalid", e))?;
    let ground_b = input(inputs, 1);
    let ground = ground_b.0.iter().find_map(|i| match &i.payload {
        Payload::Ground(g) => Some(g.clone()),
        _ => None,
    });
    let follow = s.b("follow_ground") && ground.is_some();
    let chord = s.text("align") == "Chord";
    let axis_z = s.text("forward_axis") == "Z";
    let fit = s.text("fit").to_string();
    let mut out = Bundle::empty();
    for item in &input(inputs, 0).0 {
        ctx.check_cancel()?;
        let Payload::Spline(sd) = &item.payload else {
            continue;
        };
        let pattern_text = if s.text("pattern_from").is_empty() {
            s.text("pattern").to_string()
        } else {
            match item.scope.get(s.text("pattern_from")) {
                Some(crate::data::AttrValue::Name(n)) => n.clone(),
                _ => {
                    return Err(NodeError::new(
                        "attr_missing",
                        format!("item scope has no Name `{}`", s.text("pattern_from")),
                    ));
                }
            }
        };
        // An item-scope pattern is only known now: it gets the compile-time checks here, the size
        // check before phase A builds anything.
        let terms = parse_pattern(&pattern_text)
            .and_then(|t| {
                check_names(&t, &defs)?;
                check_pattern_size(&t)?;
                Ok(t)
            })
            .map_err(|e| NodeError::new(e.code, format!("{} (column {})", e.message, e.column)))?;
        let curve = sd.curve();
        let len = curve.length_f64();
        let seed = ctx.point_seed(curve.at_distance(0.0).position.to_array());
        let mut fitter = Fitter {
            defs: &defs,
            seed,
            draws: 0,
        };
        let expanded = fitter.expand(&terms);
        let l_min: f64 = expanded.iter().map(|t| fitter.minimum(t)).sum();
        let (slots, lens, factors, before) = if len + 1e-9 < l_min {
            ctx.warn(
                "pattern_too_long",
                format!("the pattern needs {l_min:.3} m, the spline is {len:.3} m: first module only"),
            );
            match first_module(&terms) {
                Some(n) => {
                    let l = defs.get(&n).map_or(0.0, |d| d.length_m);
                    (vec![Slot::Module(n)], vec![l], vec![1.0], vec![0.0])
                }
                None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            }
        } else {
            let slots = fitter.resolve(expanded, len, ctx)?;
            let (l, f, b) = fit_lengths(&slots, &defs, len, &fit);
            (slots, l, f, b)
        };
        let mut points = PointSet::new();
        let (mut names, mut meshes, mut prefabs, mut lengths, mut index, mut starts) = (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let mut at = 0.0f64;
        for (k, slot) in slots.iter().enumerate() {
            at += before[k];
            let l = lens[k];
            if let Slot::Module(n) = slot
                && let Some(d) = defs.get(n)
            {
                let a = curve.at_distance_f64(at);
                let b = curve.at_distance_f64(at + l);
                let mut pa = a.position;
                let mut pb = b.position;
                if follow && let Some(g) = &ground {
                    pa.y = g.height(pa.x, pa.z);
                    pb.y = g.height(pb.x, pb.z);
                }
                let (t, u) = if chord && (pb - pa).length() > 1e-6 {
                    let t = (pb - pa).normalize();
                    let u = (a.up - t * a.up.dot(t)).normalize_or(Vec3::Y);
                    (t, u)
                } else {
                    (a.tangent, a.up)
                };
                let rot = if axis_z {
                    // Local Z along the path: rotate the X-forward frame a quarter turn about Y.
                    frame_rot(t, u)
                        * Rot::from_xyzw(
                            0.0,
                            std::f32::consts::FRAC_1_SQRT_2,
                            0.0,
                            std::f32::consts::FRAC_1_SQRT_2,
                        )
                } else {
                    frame_rot(t, u)
                };
                let mut scale = [1.0f32; 3];
                scale[if axis_z { 2 } else { 0 }] = factors[k] as f32;
                let position = pa.to_array();
                points.push(Point {
                    position,
                    rotation: rot.normalize().to_array(),
                    scale,
                    seed: ctx.point_seed(position),
                    ..Point::default()
                });
                names.push(n.clone());
                meshes.push(d.mesh.clone());
                prefabs.push(d.prefab.clone());
                lengths.push(l);
                index.push(index.len() as i64);
                starts.push(at);
                ctx.check_points(points.len())?;
            }
            at += l;
        }
        points
            .attrs
            .insert("module".into(), AttrColumn::Name(names.into()));
        if meshes.iter().any(|m| !m.is_empty()) {
            points
                .attrs
                .insert("mesh".into(), AttrColumn::Asset(meshes.into()));
        }
        if prefabs.iter().any(|m| !m.is_empty()) {
            points
                .attrs
                .insert("prefab".into(), AttrColumn::Asset(prefabs.into()));
        }
        points
            .attrs
            .insert("length_m".into(), AttrColumn::Float(lengths.into()));
        points.attrs.insert("index".into(), AttrColumn::Int(index.into()));
        points
            .attrs
            .insert("distance_m".into(), AttrColumn::Float(starts.into()));
        out.0.push(item.with_payload(Payload::Points(points)));
    }
    Ok(vec![out])
}

fn first_module(terms: &[Term]) -> Option<String> {
    terms.iter().find_map(|t| match t {
        Term::Module(n) => Some(n.clone()),
        Term::Repeat(seq, _) => first_module(seq),
        Term::First(alts) => alts.iter().find_map(|a| first_module(a)),
        Term::Pick(alts) => alts.iter().find_map(|(a, _)| first_module(a)),
        Term::Gap(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_language_and_reports_columns() {
        let t = parse_pattern("post repeat(panel post) end").expect("parse");
        assert_eq!(t.len(), 3);
        assert!(matches!(&t[1], Term::Repeat(seq, None) if seq.len() == 2));
        let t = parse_pattern("first(gate, panel) pick(a b: 2, c: 1) gap(0.5) repeat(x, 3)").expect("parse");
        assert_eq!(t.len(), 4);
        let e = parse_pattern("post repeat(panel post end").expect_err("unclosed");
        assert_eq!((e.code, e.column), ("pattern_syntax", 27));
        let e = parse_pattern("post )").expect_err("stray");
        assert_eq!(e.column, 6);
        let e = parse_pattern("repeat(a repeat(b repeat(c)))").expect_err("deep");
        assert_eq!(e.code, "pattern_nesting");
        assert!(parse_pattern("repeat(a repeat(b))").is_ok());
    }

    /// rev-claude-61 finding 2: the documented edge of `MAX_EXPANDED_TERMS`. A counted pick of four
    /// one-module alternatives counts 1 + 4 per copy, so 100 000 copies (100 000 resolved modules,
    /// within `MAX_MODULES`) are refused; the same count of plain modules fits.
    #[test]
    fn a_counted_pick_counts_its_wrapper_and_every_alternative_per_copy() {
        let pick = parse_pattern("repeat(pick(a: 1, b: 1, c: 1, d: 1), 100000)").expect("parses");
        assert_eq!(expanded_terms(&pick), 500_000);
        assert_eq!(
            check_pattern_size(&pick).expect_err("refused").code,
            "pattern_too_large"
        );
        let plain = parse_pattern(&format!("repeat(a, {MAX_MODULES})")).expect("parses");
        assert_eq!(expanded_terms(&plain), MAX_MODULES);
        assert!(check_pattern_size(&plain).is_ok());
    }

    /// v0.5.1 (deep-zai-procgen-runtime finding 1): the expanded size is counted, never built.
    #[test]
    fn expanded_size_is_counted_without_building() {
        let huge = parse_pattern("repeat(repeat(post, 1000000), 1000000)").expect("parses");
        assert_eq!(expanded_terms(&huge), 1_000_000_000_000);
        assert_eq!(
            check_pattern_size(&huge).expect_err("too large").code,
            "pattern_too_large"
        );
        // Wrappers and alternatives count too: 1 + 3 x (pick + 2) + (repeat + 1) + 1.
        let ok = parse_pattern("post repeat(pick(panel: 1, gate: 1), 3) repeat(panel) end").expect("parses");
        assert_eq!(expanded_terms(&ok), 1 + 3 * 3 + 2 + 1);
        assert!(check_pattern_size(&ok).is_ok());
        let edge = parse_pattern(&format!("repeat(post, {MAX_EXPANDED_TERMS})")).expect("parses");
        assert!(check_pattern_size(&edge).is_ok(), "the cap itself is allowed");
        let over = parse_pattern(&format!("repeat(post, {})", MAX_EXPANDED_TERMS + 1)).expect("parses");
        assert!(check_pattern_size(&over).is_err());
        // Phase A builds exactly the counted size.
        let defs: BTreeMap<String, ModuleDef> = BTreeMap::new();
        let mut f = Fitter {
            defs: &defs,
            seed: 0,
            draws: 0,
        };
        assert_eq!(expanded_terms(&f.expand(&ok)), expanded_terms(&ok));
    }
}
