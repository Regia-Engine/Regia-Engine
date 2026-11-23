// SPDX-License-Identifier: MPL-2.0
//! Number-field expressions (`docs/specs/19-editor.md` inspector, UI parity addendum § 7.7,
//! ADR-7785): what a typed number field accepts besides a plain number.
//!
//! - arithmetic with the usual precedence: `+ - * / %`, `^` (power, right-associative), unary
//!   minus, parentheses: `2*(3+4)`, `-2^2` is `-4`;
//! - constants `pi`, `tau`, `e`; functions `sqrt abs sin cos tan asin acos atan floor ceil round
//!   ln log exp deg rad` (one argument), `min max pow` (two) and `clamp` (three); trigonometry works
//!   in radians, `deg(x)` turns radians into degrees and `rad(x)` degrees into radians;
//! - a relative edit of the current value: a leading `+`, `*` or `/` (`+10`, `*2`, `/4`) or an
//!   assignment operator `+= -= *= /=` (`-=5`). A leading `-` is a negative number, not relative;
//! - a unit after a number or a parenthesized group, converted to the field's unit: lengths
//!   `mm cm m km in ft yd mi`, angles `deg ° rad turn`, times `ms s min h`. `5cm` in a meter field
//!   is `0.05`, `90deg` in a radian field is `pi/2`. A field without a unit takes the base unit of
//!   the dimension (meters, degrees, seconds); a unit of another dimension is an error.
//!
//! Evaluation never panics and never yields a non-finite number (the fuzz test feeds it 10 000
//! random inputs). A failed expression leaves the field's value as it was; the inspector flashes
//! the field and shows the error's message.

use std::fmt;

/// The longest input the evaluator reads (bytes).
pub const MAX_EXPR_LEN: usize = 256;

/// The deepest nesting of parentheses, unary operators and powers.
pub const MAX_EXPR_DEPTH: usize = 48;

/// Why an expression was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    /// The agent error code: `expr_empty`, `expr_too_long`, `expr_too_deep`, `expr_syntax`,
    /// `expr_unknown_name`, `expr_unknown_unit`, `expr_unit_mismatch`, `expr_arity` or
    /// `expr_not_finite`.
    pub code: &'static str,
    /// What is wrong, for the field's tooltip and the agent's message.
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ExprError {}

fn err(code: &'static str, message: impl Into<String>) -> ExprError {
    ExprError {
        code,
        message: message.into(),
    }
}

/// What a unit measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// Lengths (base: meters).
    Length,
    /// Angles (base: degrees).
    Angle,
    /// Times (base: seconds).
    Time,
}

/// A unit the expressions know: its dimension and how many base units one of it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnitDef {
    /// What it measures.
    pub dimension: Dimension,
    /// Base units (meters, degrees, seconds) per one of this unit.
    pub factor: f64,
}

/// The units an expression may name, by spelling.
const UNITS: &[(&str, Dimension, f64)] = &[
    ("mm", Dimension::Length, 0.001),
    ("cm", Dimension::Length, 0.01),
    ("m", Dimension::Length, 1.0),
    ("km", Dimension::Length, 1000.0),
    ("in", Dimension::Length, 0.0254),
    ("ft", Dimension::Length, 0.3048),
    ("yd", Dimension::Length, 0.9144),
    ("mi", Dimension::Length, 1609.344),
    ("deg", Dimension::Angle, 1.0),
    ("°", Dimension::Angle, 1.0),
    ("rad", Dimension::Angle, 180.0 / std::f64::consts::PI),
    ("turn", Dimension::Angle, 360.0),
    ("ms", Dimension::Time, 0.001),
    ("s", Dimension::Time, 1.0),
    ("sec", Dimension::Time, 1.0),
    ("min", Dimension::Time, 60.0),
    ("h", Dimension::Time, 3600.0),
];

/// The unit spelled `name` (`"cm"`, `"°"`), if the expressions know it.
#[must_use]
pub fn unit(name: &str) -> Option<UnitDef> {
    UNITS
        .iter()
        .find(|(spelling, _, _)| *spelling == name)
        .map(|&(_, dimension, factor)| UnitDef { dimension, factor })
}

/// The unit of a field from its display suffix (`" m"`, `"°"`, `" deg"`, `" s"`); `None` for no
/// suffix or one the expressions do not convert (`" m/s"`, `" dB"`).
#[must_use]
pub fn field_unit(suffix: &str) -> Option<UnitDef> {
    let trimmed = suffix.trim();
    if trimmed.is_empty() {
        return None;
    }
    unit(trimmed)
}

/// The value of `input` for a field whose value is `current` and whose display suffix is
/// `suffix` (`""` when it has none).
///
/// # Errors
///
/// [`ExprError`] with the code of what is wrong; the caller keeps the old value.
pub fn evaluate(input: &str, current: f64, suffix: &str) -> Result<f64, ExprError> {
    if input.len() > MAX_EXPR_LEN {
        return Err(err(
            "expr_too_long",
            format!("expressions are at most {MAX_EXPR_LEN} bytes"),
        ));
    }
    let tokens = lex(input)?;
    if tokens.is_empty() {
        return Err(err("expr_empty", "type a number or an expression"));
    }
    let target = field_unit(suffix);
    let unknown_field_unit = target.is_none() && !suffix.trim().is_empty();
    let (relative, start) = relative_op(&tokens);
    let mut parser = Parser {
        tokens: &tokens,
        pos: start,
        depth: 0,
        target,
        unknown_field_unit,
        suffix: suffix.trim(),
    };
    if parser.pos >= tokens.len() {
        return Err(err("expr_syntax", "a relative edit needs a number after it"));
    }
    let value = parser.sum()?;
    if let Some(token) = tokens.get(parser.pos) {
        return Err(err("expr_syntax", format!("unexpected {token} at the end")));
    }
    let result = match relative {
        None => value,
        Some(Relative::Add) => current + value,
        Some(Relative::Sub) => current - value,
        Some(Relative::Mul) => current * value,
        Some(Relative::Div) => current / value,
    };
    finite(result)
}

/// A relative edit of the current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relative {
    Add,
    Sub,
    Mul,
    Div,
}

/// The leading relative operator, if any, and the index of the first token after it.
fn relative_op(tokens: &[Token]) -> (Option<Relative>, usize) {
    let op = match tokens.first() {
        Some(Token::Op('+')) => Relative::Add,
        Some(Token::Op('-')) => Relative::Sub,
        Some(Token::Op('*')) => Relative::Mul,
        Some(Token::Op('/')) => Relative::Div,
        _ => return (None, 0),
    };
    if tokens.get(1) == Some(&Token::Assign) {
        return (Some(op), 2);
    }
    match op {
        // `-5` is a negative number, `-=5` the relative edit.
        Relative::Sub => (None, 0),
        _ => (Some(op), 1),
    }
}

fn finite(value: f64) -> Result<f64, ExprError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(err("expr_not_finite", "the result is not a finite number"))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Ident(String),
    Op(char),
    Assign,
    Open,
    Close,
    Comma,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Num(n) => write!(f, "`{n}`"),
            Self::Ident(name) => write!(f, "`{name}`"),
            Self::Op(op) => write!(f, "`{op}`"),
            Self::Assign => f.write_str("`=`"),
            Self::Open => f.write_str("`(`"),
            Self::Close => f.write_str("`)`"),
            Self::Comma => f.write_str("`,`"),
        }
    }
}

fn lex(input: &str) -> Result<Vec<Token>, ExprError> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || c == '.' {
            let start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit() || *c == '.') {
                i += 1;
            }
            // An exponent only when digits follow (`2e3`, `1e-4`); `2e` stays `2` then `e`.
            if matches!(chars.get(i), Some('e' | 'E')) {
                let digit_at = |k: usize| chars.get(k).is_some_and(char::is_ascii_digit);
                let sign = matches!(chars.get(i + 1), Some('+' | '-'));
                if digit_at(i + 1) || (sign && digit_at(i + 2)) {
                    i += if sign { 2 } else { 1 };
                    while chars.get(i).is_some_and(char::is_ascii_digit) {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            let value: f64 = text
                .parse()
                .map_err(|_| err("expr_syntax", format!("`{text}` is not a number")))?;
            tokens.push(Token::Num(value));
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '°' {
            let start = i;
            if c == '°' {
                i += 1;
            } else {
                while chars.get(i).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
                    i += 1;
                }
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let token = match c {
            '+' | '*' | '/' | '%' | '^' => Token::Op(c),
            '-' | '\u{2212}' => Token::Op('-'),
            '=' => Token::Assign,
            '(' => Token::Open,
            ')' => Token::Close,
            ',' => Token::Comma,
            other => {
                return Err(err(
                    "expr_syntax",
                    format!("`{other}` is not part of an expression"),
                ));
            }
        };
        tokens.push(token);
        i += 1;
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    depth: usize,
    target: Option<UnitDef>,
    unknown_field_unit: bool,
    suffix: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn descend(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(err(
                "expr_too_deep",
                format!("expressions nest at most {MAX_EXPR_DEPTH} levels"),
            ));
        }
        Ok(())
    }

    /// `sum := product (('+' | '-') product)*`
    fn sum(&mut self) -> Result<f64, ExprError> {
        let mut value = self.product()?;
        while let Some(Token::Op(op @ ('+' | '-'))) = self.peek() {
            let op = *op;
            self.pos += 1;
            let rhs = self.product()?;
            value = if op == '+' { value + rhs } else { value - rhs };
        }
        Ok(value)
    }

    /// `product := unary (('*' | '/' | '%') unary)*`
    fn product(&mut self) -> Result<f64, ExprError> {
        let mut value = self.unary()?;
        while let Some(Token::Op(op @ ('*' | '/' | '%'))) = self.peek() {
            let op = *op;
            self.pos += 1;
            let rhs = self.unary()?;
            value = match op {
                '*' => value * rhs,
                '/' => value / rhs,
                _ => value % rhs,
            };
        }
        Ok(value)
    }

    /// `unary := ('-' | '+') unary | power`
    fn unary(&mut self) -> Result<f64, ExprError> {
        self.descend()?;
        let value = match self.peek() {
            Some(Token::Op('-')) => {
                self.pos += 1;
                self.unary().map(|v| -v)
            }
            Some(Token::Op('+')) => {
                self.pos += 1;
                self.unary()
            }
            _ => self.power(),
        };
        self.depth -= 1;
        value
    }

    /// `power := postfix ('^' unary)?` (right-associative through `unary`).
    fn power(&mut self) -> Result<f64, ExprError> {
        let base = self.postfix()?;
        if self.peek() == Some(&Token::Op('^')) {
            self.pos += 1;
            let exponent = self.unary()?;
            return Ok(base.powf(exponent));
        }
        Ok(base)
    }

    /// `postfix := primary unit?` where the unit follows a number or a parenthesized group.
    fn postfix(&mut self) -> Result<f64, ExprError> {
        let takes_unit = matches!(self.peek(), Some(Token::Num(_) | Token::Open));
        let value = self.primary()?;
        if takes_unit && let Some(Token::Ident(name)) = self.peek() {
            let name = name.clone();
            self.pos += 1;
            return self.convert(value, &name);
        }
        Ok(value)
    }

    fn convert(&self, value: f64, name: &str) -> Result<f64, ExprError> {
        let Some(from) = unit(name) else {
            return Err(err("expr_unknown_unit", format!("`{name}` is not a unit")));
        };
        if self.unknown_field_unit {
            return Err(err(
                "expr_unit_mismatch",
                format!(
                    "this field is in `{}`; `{name}` does not convert to it",
                    self.suffix
                ),
            ));
        }
        match self.target {
            None => Ok(value * from.factor),
            Some(to) if to.dimension == from.dimension => Ok(value * from.factor / to.factor),
            Some(_) => Err(err(
                "expr_unit_mismatch",
                format!(
                    "this field is in `{}`; `{name}` measures something else",
                    self.suffix
                ),
            )),
        }
    }

    /// `primary := number | '(' sum ')' | name | name '(' sum (',' sum)* ')'`
    fn primary(&mut self) -> Result<f64, ExprError> {
        match self.peek().cloned() {
            Some(Token::Num(value)) => {
                self.pos += 1;
                Ok(value)
            }
            Some(Token::Open) => {
                self.pos += 1;
                self.descend()?;
                let value = self.sum()?;
                self.depth -= 1;
                self.expect_close()?;
                Ok(value)
            }
            Some(Token::Ident(name)) => {
                self.pos += 1;
                if self.peek() == Some(&Token::Open) {
                    self.pos += 1;
                    self.descend()?;
                    let mut args = vec![self.sum()?];
                    while self.peek() == Some(&Token::Comma) {
                        self.pos += 1;
                        args.push(self.sum()?);
                    }
                    self.depth -= 1;
                    self.expect_close()?;
                    return call(&name, &args);
                }
                constant(&name)
            }
            Some(token) => Err(err("expr_syntax", format!("unexpected {token}"))),
            None => Err(err("expr_syntax", "the expression ends too early")),
        }
    }

    fn expect_close(&mut self) -> Result<(), ExprError> {
        if self.peek() == Some(&Token::Close) {
            self.pos += 1;
            Ok(())
        } else {
            Err(err("expr_syntax", "a `(` has no matching `)`"))
        }
    }
}

fn constant(name: &str) -> Result<f64, ExprError> {
    match name {
        "pi" | "PI" => Ok(std::f64::consts::PI),
        "tau" | "TAU" => Ok(std::f64::consts::TAU),
        "e" => Ok(std::f64::consts::E),
        _ if unit(name).is_some() => Err(err(
            "expr_syntax",
            format!("the unit `{name}` needs a number before it (`5{name}`)"),
        )),
        _ => Err(err("expr_unknown_name", format!("`{name}` is not a constant"))),
    }
}

fn call(name: &str, args: &[f64]) -> Result<f64, ExprError> {
    let arity = |n: usize| -> Result<(), ExprError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(err(
                "expr_arity",
                format!("`{name}` takes {n} argument(s), not {}", args.len()),
            ))
        }
    };
    let one = |f: fn(f64) -> f64| -> Result<f64, ExprError> {
        arity(1)?;
        Ok(f(args[0]))
    };
    match name {
        "sqrt" => one(f64::sqrt),
        "abs" => one(f64::abs),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "floor" => one(f64::floor),
        "ceil" => one(f64::ceil),
        "round" => one(f64::round),
        "ln" => one(f64::ln),
        "log" => one(f64::log10),
        "exp" => one(f64::exp),
        "deg" => one(f64::to_degrees),
        "rad" => one(f64::to_radians),
        "min" => {
            arity(2)?;
            Ok(args[0].min(args[1]))
        }
        "max" => {
            arity(2)?;
            Ok(args[0].max(args[1]))
        }
        "pow" => {
            arity(2)?;
            Ok(args[0].powf(args[1]))
        }
        "clamp" => {
            arity(3)?;
            let (lo, hi) = (args[1].min(args[2]), args[1].max(args[2]));
            Ok(args[0].clamp(lo, hi))
        }
        _ => Err(err("expr_unknown_name", format!("`{name}` is not a function"))),
    }
}

/// Whether `input` reads as a plain number (the default parser's job): digits, one sign, one
/// point, an exponent. The inspector evaluates everything else as an expression.
#[must_use]
pub fn is_plain_number(input: &str) -> bool {
    let trimmed = input.trim();
    !trimmed.is_empty() && !trimmed.starts_with('+') && trimmed.parse::<f64>().is_ok_and(f64::is_finite)
}

/// The number in a reflected numeric value (floats and integers).
fn reflected_number(value: &dyn regia_ecs::reflect::PartialReflect) -> Option<(f64, bool)> {
    macro_rules! numbers {
        ($($ty:ty => $integral:expr),*) => {$(
            if let Some(v) = value.try_downcast_ref::<$ty>() {
                return Some((egui::emath::Numeric::to_f64(*v), $integral));
            }
        )*};
    }
    numbers!(f32 => false, f64 => false, i8 => true, i16 => true, i32 => true, i64 => true, isize => true,
        u8 => true, u16 => true, u32 => true, u64 => true, usize => true);
    None
}

/// The JSON value `editor.set_property { expr }` writes: `expr` evaluated against the field's
/// current value in the unit the Inspector shows (degrees for a radian angle, the `@Units` or
/// name-inferred suffix otherwise), as the field's number type.
///
/// # Errors
///
/// `(code, message)`: `expr_not_number` when the field is not a number, `path_unknown` when the
/// path does not resolve, or the expression's own code ([`ExprError`]).
pub fn property_expr_value(
    world: &regia_ecs::prelude::World,
    entity: regia_ecs::prelude::Entity,
    component: &str,
    path: &str,
    expr: &str,
) -> Result<serde_json::Value, (&'static str, String)> {
    let current = crate::command::read_property(world, entity, component, path)
        .map_err(|e| ("path_unknown", e.to_string()))?;
    let Some((number, integral)) = reflected_number(current.as_ref()) else {
        return Err((
            "expr_not_number",
            format!("`{component}.{path}` is not a number; `expr` takes number fields only"),
        ));
    };
    // The field's attributes come from its parent struct.
    let (parent, field) = match path.rsplit_once('.') {
        Some((parent, field)) => (parent, field),
        None => ("", path),
    };
    let attrs = crate::command::read_property(world, entity, component, parent)
        .ok()
        .and_then(|parent| match parent.get_represented_type_info() {
            Some(regia_ecs::reflect::TypeInfo::Struct(info)) => info
                .field(field)
                .map(|f| regia_ecs::attrs::EditorAttrs::from_custom(f.custom_attributes())),
            _ => None,
        })
        .unwrap_or_default();
    let options = super::options::FieldOptions::for_field(component, field, attrs);
    let degrees = options.radians_as_degrees() && !integral;
    let shown = if degrees { number.to_degrees() } else { number };
    let value = super::impls::parse_number_text(expr, shown, &options.suffix(), integral)
        .map_err(|e| (e.code, e.message))?;
    let stored = if degrees { value.to_radians() } else { value };
    if integral {
        // Out-of-range whole numbers are refused by the field's deserializer.
        if stored < 0.0 {
            return Ok(serde_json::json!(stored as i64));
        }
        return Ok(serde_json::json!(stored as u64));
    }
    Ok(serde_json::json!(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    /// The 40-input acceptance table: input, current value, field suffix, expected value.
    #[test]
    fn forty_inputs_evaluate_to_their_values() {
        let table: [(&str, f64, &str, f64); 40] = [
            ("2", 0.0, "", 2.0),
            ("-3.5", 0.0, "", -3.5),
            ("1e3", 0.0, "", 1000.0),
            ("2.5e-2", 0.0, "", 0.025),
            ("2*pi", 0.0, "", 2.0 * PI),
            ("tau/4", 0.0, "", PI / 2.0),
            ("1+2*3", 0.0, "", 7.0),
            ("(1+2)*3", 0.0, "", 9.0),
            ("2^3^2", 0.0, "", 512.0),
            ("-2^2", 0.0, "", -4.0),
            ("7 % 4", 0.0, "", 3.0),
            ("sqrt(16)", 0.0, "", 4.0),
            ("sqrt(2)*sqrt(2)", 0.0, "", 2.0),
            ("deg(pi)", 0.0, "", 180.0),
            ("rad(180)", 0.0, "", PI),
            ("abs(-4)", 0.0, "", 4.0),
            ("min(3, 9)", 0.0, "", 3.0),
            ("max(3, 9)", 0.0, "", 9.0),
            ("clamp(12, 0, 10)", 0.0, "", 10.0),
            ("pow(2, 10)", 0.0, "", 1024.0),
            ("round(2.6)", 0.0, "", 3.0),
            ("+10", 5.0, "", 15.0),
            ("+=10", 5.0, "", 15.0),
            ("-=5", 12.0, "", 7.0),
            ("*2", 4.5, "", 9.0),
            ("*=3", 2.0, "", 6.0),
            ("/4", 10.0, "", 2.5),
            ("/=2", 10.0, "", 5.0),
            ("+ 2*3", 1.0, "", 7.0),
            ("5cm", 0.0, " m", 0.05),
            ("2ft", 0.0, " m", 0.6096),
            ("1m + 50cm", 0.0, " m", 1.5),
            ("3 km", 0.0, " m", 3000.0),
            ("90deg", 0.0, "°", 90.0),
            ("1rad", 0.0, "°", 180.0 / PI),
            ("0.25turn", 0.0, " deg", 90.0),
            ("10ms", 0.0, " s", 0.01),
            ("3s", 0.0, " ms", 3000.0),
            ("2min", 0.0, " s", 120.0),
            ("+=5cm", 1.0, " m", 1.05),
        ];
        for (input, current, suffix, want) in table {
            let got = evaluate(input, current, suffix);
            assert!(
                got.as_ref().is_ok_and(|got| close(*got, want)),
                "{input:?} (current {current}, unit {suffix:?}): got {got:?}, want {want}"
            );
        }
    }

    #[test]
    fn unitless_fields_take_the_base_unit() {
        assert!(close(evaluate("5cm", 0.0, "").unwrap_or(f64::NAN), 0.05));
        assert!(close(evaluate("1rad", 0.0, "").unwrap_or(f64::NAN), 180.0 / PI));
        assert!(close(evaluate("10ms", 0.0, "").unwrap_or(f64::NAN), 0.01));
    }

    #[test]
    fn bad_inputs_report_their_code() {
        let cases: [(&str, &str, &str); 15] = [
            ("", "", "expr_empty"),
            ("   ", "", "expr_empty"),
            ("2+", "", "expr_syntax"),
            ("(1+2", "", "expr_syntax"),
            ("1)", "", "expr_syntax"),
            ("2 3", "", "expr_syntax"),
            ("foo", "", "expr_unknown_name"),
            ("foo(2)", "", "expr_unknown_name"),
            ("5parsecs", " m", "expr_unknown_unit"),
            ("5s", " m", "expr_unit_mismatch"),
            ("90deg", " s", "expr_unit_mismatch"),
            ("5cm", " m/s", "expr_unit_mismatch"),
            ("min(1)", "", "expr_arity"),
            ("1/0", "", "expr_not_finite"),
            ("sqrt(-1)", "", "expr_not_finite"),
        ];
        for (input, suffix, code) in cases {
            let got = evaluate(input, 1.0, suffix);
            assert_eq!(
                got.as_ref().err().map(|e| e.code),
                Some(code),
                "{input:?}: {got:?}"
            );
        }
        assert_eq!(
            evaluate(&"(".repeat(200), 0.0, "").err().map(|e| e.code),
            Some("expr_too_deep")
        );
        assert_eq!(
            evaluate(&"1+".repeat(200), 0.0, "").err().map(|e| e.code),
            Some("expr_too_long")
        );
        assert_eq!(
            evaluate(&"-".repeat(100), 0.0, "").err().map(|e| e.code),
            Some("expr_too_deep")
        );
    }

    #[test]
    fn plain_numbers_are_told_apart() {
        assert!(is_plain_number("2.5"));
        assert!(is_plain_number(" -3 "));
        assert!(!is_plain_number("+3"));
        assert!(!is_plain_number("2*pi"));
        assert!(!is_plain_number("inf"));
        assert!(!is_plain_number(""));
    }

    /// 10 000 random inputs over the expression alphabet: evaluation returns (never panics),
    /// and every value it returns is finite.
    #[test]
    fn ten_thousand_random_inputs_never_panic() {
        const ALPHABET: &[&str] = &[
            "0", "1", "2", "9", ".", "e", "E", "+", "-", "*", "/", "%", "^", "=", "(", ")", ",", " ", "pi",
            "tau", "sqrt", "deg", "rad", "min", "max", "clamp", "cm", "m", "ft", "s", "ms", "°", "1e308",
            "1e-308", "x", "\u{2212}", "√", "∞", "nan", "inf", "\0",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let suffixes = ["", " m", "°", " s", " m/s"];
        for _ in 0..10_000 {
            let len = (next() % 24) as usize;
            let mut input = String::new();
            for _ in 0..len {
                input.push_str(ALPHABET[(next() % ALPHABET.len() as u64) as usize]);
            }
            let current = [0.0, 1.0, -7.5, 1e300][(next() % 4) as usize];
            let suffix = suffixes[(next() % suffixes.len() as u64) as usize];
            if let Ok(value) = evaluate(&input, current, suffix) {
                assert!(value.is_finite(), "{input:?} gave {value}");
            }
        }
    }
}
