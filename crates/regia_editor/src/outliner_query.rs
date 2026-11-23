// SPDX-License-Identifier: MPL-2.0
//! The Outliner's query language (`docs/specs/19-editor.md` § 2, ADR-7757): name, kind, component,
//! tag, property and ancestry terms combined with `and`, `or`, `not` and parentheses.
//!
//! ```text
//! query   := or?
//! or      := and ( "or" and )*
//! and     := unary ( "and"? unary )*          juxtaposition is "and"
//! unary   := "not" unary | "(" or ")" | term
//! term    := key ":" value                    kind: type: tag: has: under: parent: name:
//!          | path op value                    op: ~ = != < <= > >=
//!          | word | "quoted text"             name contains
//! ```
//!
//! Today's search tokens keep their meaning exactly: a run of bare words side by side is one
//! "name contains" phrase (`crate lid` looks for "crate lid"), `kind:`/`type:`/`tag:` filter as
//! before, a bare `kind:`, `type:` or `tag:` is ignored, a later `kind:` in a run replaces an
//! earlier one, and a `key:value` with a key the language does not know stays name text.
//!
//! Matching is case-insensitive. A property term (`PointLight.intensity>1000`,
//! `Transform.translation.y<=0`) names a component by its short type name (`Light` also matches
//! any component whose short name ends with it, such as `PointLight` or `SpotLight`) and a field
//! path inside it; a component the entity lacks, or a field it does not have, never matches.
//! `under:A/B` matches the descendants of an entity named `B` whose parent is named `A`
//! (`under:/A/B` anchors the path at a root); `parent:` is the same for the direct parent only.
//!
//! Malformed queries return a [`QueryError`] with a documented code and a 1-based column, never a
//! panic: the parser is recursive descent with a nesting limit ([`MAX_DEPTH`]).

use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, GetPath, PartialReflect, ReflectComponent};

use crate::kind_icons::{KindFilter, RowIcon};

/// The deepest parenthesis / `not` nesting a query may use (deeper ones are refused with
/// `query_too_deep`, so a hostile query cannot exhaust the stack).
pub const MAX_DEPTH: usize = 64;

/// Every error code a query can fail with (`book/agents/outliner.md`).
pub const ERROR_CODES: &[&str] = &[
    "query_unclosed_paren",
    "query_unexpected_paren",
    "query_unclosed_quote",
    "query_missing_operand",
    "query_missing_value",
    "query_not_a_number",
    "query_bad_field",
    "query_too_deep",
];

/// Why a query does not parse: a stable code (one of [`ERROR_CODES`]), the 1-based character
/// column the problem starts at, and a message for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// The error code (lowercase ascii, digits and `_`).
    pub code: &'static str,
    /// 1-based character column in the query text.
    pub column: usize,
    /// What is wrong, in plain words.
    pub message: String,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "column {}: {} ({})", self.column, self.message, self.code)
    }
}

impl std::error::Error for QueryError {}

/// How a property or name term compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `~`: text contains.
    Contains,
    /// `=`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

impl CompareOp {
    /// The operator's text.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Contains => "~",
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    fn ordered(self) -> bool {
        matches!(self, Self::Lt | Self::Le | Self::Gt | Self::Ge)
    }
}

/// A comparison's right-hand side.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A number (`1000`, `-0.5`).
    Number(f64),
    /// `true` or `false`.
    Bool(bool),
    /// Text (lowercase; compared case-insensitively).
    Text(String),
}

/// A name path of `under:` and `parent:` terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    /// The names, outermost first, lowercase.
    pub segments: Vec<String>,
    /// A leading `/`: the first segment is a root.
    pub anchored: bool,
}

impl PathPattern {
    fn parse(text: &str) -> Self {
        let anchored = text.starts_with('/');
        let segments = text
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(str::to_lowercase)
            .collect();
        Self { segments, anchored }
    }

    /// Whether the chain `path` (root first, lowercase or not) ends with this pattern (equals it
    /// when anchored).
    #[must_use]
    pub fn matches(&self, path: &[&str]) -> bool {
        if self.segments.is_empty() || self.segments.len() > path.len() {
            return false;
        }
        if self.anchored && self.segments.len() != path.len() {
            return false;
        }
        let tail = &path[path.len() - self.segments.len()..];
        tail.iter()
            .zip(&self.segments)
            .all(|(have, want)| have.to_lowercase() == *want)
    }
}

/// One query term.
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// Always true (a bare `kind:`, `type:` or `tag:`, which today's search ignores).
    True,
    /// The row label compared with text: `name~door`, `name=Door`, a bare word.
    Name {
        /// How it compares.
        op: CompareOp,
        /// The text, lowercase.
        text: String,
    },
    /// `kind:<name>`, `kind:none`, `kind:?`.
    Kind(KindFilter),
    /// `type:<Component>` and `has:<Component>`: the entity has a component of that short name.
    Has(String),
    /// `tag:<name>`.
    Tag(String),
    /// `under:<path>`: an ancestor at that path.
    Under(PathPattern),
    /// `parent:<path>`: the direct parent at that path.
    Parent(PathPattern),
    /// `Component.field.path <op> value`.
    Property {
        /// The component's short type name, lowercase.
        component: String,
        /// The reflected field path inside it (`translation.y`; empty = the whole value).
        path: String,
        /// How it compares.
        op: CompareOp,
        /// The value compared with.
        value: Literal,
    },
}

/// A parsed query.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Every operand matches.
    And(Vec<Expr>),
    /// Any operand matches.
    Or(Vec<Expr>),
    /// The operand does not match.
    Not(Box<Expr>),
    /// One term.
    Term(Term),
}

/// A parsed Outliner query (`Query::default()` matches everything).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    expr: Option<Expr>,
}

/// The facts of one row a query reads.
#[derive(Debug, Clone, Copy)]
pub struct RowFacts<'a> {
    /// The row's entity.
    pub entity: Entity,
    /// The row label.
    pub label: &'a str,
    /// The row icon (kind facts).
    pub icon: &'a RowIcon,
    /// Lowercase short component names.
    pub components: &'a [String],
    /// Lowercase tags.
    pub tags: &'a [String],
    /// Ancestor labels, root first, the parent last (folders count as ancestors).
    pub ancestors: &'a [&'a str],
}

/// A property value as a query compares it.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    /// Any integer or float field.
    Number(f64),
    /// A `bool` field.
    Bool(bool),
    /// A string field, or an enum's variant name.
    Text(String),
}

/// Where property terms read their values.
pub trait PropertySource {
    /// The values of `path` inside every component of `entity` whose lowercase short name is
    /// `component` (or ends with it when none is equal).
    fn properties(&self, entity: Entity, component: &str, path: &str) -> Vec<PropValue>;

    /// The Outliner folder path of a top-level `entity` (its folders count as ancestors for
    /// `under:` and `parent:`); `None` by default.
    fn folder(&self, entity: Entity) -> Option<String> {
        let _ = entity;
        None
    }
}

/// A source with no properties: property terms never match (for trees without a world).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProperties;

impl PropertySource for NoProperties {
    fn properties(&self, _: Entity, _: &str, _: &str) -> Vec<PropValue> {
        Vec::new()
    }
}

impl PropertySource for World {
    fn properties(&self, entity: Entity, component: &str, path: &str) -> Vec<PropValue> {
        world_properties(self, entity, component, path)
    }

    fn folder(&self, entity: Entity) -> Option<String> {
        crate::outliner_folders::folder_of(self, entity)
    }
}

/// Reads `path` from the entity's components named `component` through reflection.
fn world_properties(world: &World, entity: Entity, component: &str, path: &str) -> Vec<PropValue> {
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        return Vec::new();
    };
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    let short = |name: &str| -> String {
        let outer = name.split('<').next().unwrap_or(name);
        outer.rsplit("::").next().unwrap_or(outer).to_lowercase()
    };
    let named: Vec<(String, Option<std::any::TypeId>)> = infos
        .map(|(_, info)| (short(&info.name().to_string()), info.type_id()))
        .collect();
    let exact: Vec<_> = named.iter().filter(|(name, _)| name == component).collect();
    let chosen = if exact.is_empty() {
        named
            .iter()
            .filter(|(name, _)| name.ends_with(component))
            .collect()
    } else {
        exact
    };
    let registry = registry.read();
    let mut out = Vec::new();
    for (_, type_id) in chosen {
        let Some(registration) = type_id.and_then(|id| registry.get(id)) else {
            continue;
        };
        let Some(reflect) = registration.data::<ReflectComponent>() else {
            continue;
        };
        let Ok(entity_ref) = world.get_entity(entity) else {
            continue;
        };
        let Some(value) = reflect.reflect(entity_ref) else {
            continue;
        };
        let field: &dyn PartialReflect = if path.is_empty() {
            value.as_partial_reflect()
        } else {
            match value.reflect_path(path) {
                Ok(field) => field,
                Err(_) => continue,
            }
        };
        if let Some(found) = prop_value(field) {
            out.push(found);
        }
    }
    out
}

/// A reflected leaf as a [`PropValue`]: numbers, `bool`, strings and enum variants.
fn prop_value(value: &dyn PartialReflect) -> Option<PropValue> {
    macro_rules! number {
        ($($t:ty),*) => {
            $(if let Some(n) = value.try_downcast_ref::<$t>() {
                return Some(PropValue::Number(*n as f64));
            })*
        };
    }
    number!(f32, f64, i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);
    if let Some(b) = value.try_downcast_ref::<bool>() {
        return Some(PropValue::Bool(*b));
    }
    if let Some(s) = value.try_downcast_ref::<String>() {
        return Some(PropValue::Text(s.clone()));
    }
    if let Some(name) = value.try_downcast_ref::<regia_scene::prelude::Name>() {
        return Some(PropValue::Text(name.as_str().to_string()));
    }
    if let regia_ecs::reflect::ReflectRef::Enum(e) = value.reflect_ref() {
        return Some(PropValue::Text(e.variant_name().to_string()));
    }
    None
}

impl Query {
    /// Parses `text`.
    ///
    /// # Errors
    /// A [`QueryError`] with one of [`ERROR_CODES`] and the column it starts at.
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let tokens = lex(text)?;
        let mut parser = Parser {
            tokens,
            at: 0,
            end_column: text.chars().count() + 1,
        };
        if parser.tokens.is_empty() {
            return Ok(Self::default());
        }
        let expr = parser.or(0)?;
        if let Some(token) = parser.tokens.get(parser.at) {
            // The only token `or` stops at without consuming is a stray `)`.
            return Err(QueryError {
                code: "query_unexpected_paren",
                column: token.column,
                message: "this closing parenthesis has no opening one".to_string(),
            });
        }
        Ok(Self { expr: Some(expr) })
    }

    /// Whether the query keeps every row (empty, blank, or only ignored terms).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &self.expr {
            None => true,
            Some(Expr::Term(Term::True)) => true,
            Some(Expr::And(items)) => items.iter().all(|e| matches!(e, Expr::Term(Term::True))),
            Some(_) => false,
        }
    }

    /// Whether any term reads property values (the panel then re-runs it after edits).
    #[must_use]
    pub fn reads_properties(&self) -> bool {
        fn walk(expr: &Expr) -> bool {
            match expr {
                Expr::And(items) | Expr::Or(items) => items.iter().any(walk),
                Expr::Not(inner) => walk(inner),
                Expr::Term(term) => matches!(term, Term::Property { .. }),
            }
        }
        self.expr.as_ref().is_some_and(walk)
    }

    /// The parsed expression (`None` = match everything).
    #[must_use]
    pub fn expr(&self) -> Option<&Expr> {
        self.expr.as_ref()
    }

    /// Whether a row matches.
    #[must_use]
    pub fn matches(&self, row: &RowFacts<'_>, props: &dyn PropertySource) -> bool {
        self.expr.as_ref().is_none_or(|expr| eval(expr, row, props))
    }
}

/// Evaluates `expr` for one row.
#[must_use]
pub fn eval(expr: &Expr, row: &RowFacts<'_>, props: &dyn PropertySource) -> bool {
    match expr {
        Expr::And(items) => items.iter().all(|e| eval(e, row, props)),
        Expr::Or(items) => items.iter().any(|e| eval(e, row, props)),
        Expr::Not(inner) => !eval(inner, row, props),
        Expr::Term(term) => term_matches(term, row, props),
    }
}

fn term_matches(term: &Term, row: &RowFacts<'_>, props: &dyn PropertySource) -> bool {
    match term {
        Term::True => true,
        Term::Name { op, text } => compare(
            &PropValue::Text(row.label.to_string()),
            *op,
            &Literal::Text(text.clone()),
        ),
        Term::Kind(filter) => filter.matches(row.icon),
        Term::Has(name) => row.components.iter().any(|c| c == name),
        Term::Tag(tag) => row.tags.iter().any(|t| t == tag),
        Term::Under(pattern) => (1..=row.ancestors.len()).any(|end| pattern.matches(&row.ancestors[..end])),
        Term::Parent(pattern) => !row.ancestors.is_empty() && pattern.matches(row.ancestors),
        Term::Property {
            component,
            path,
            op,
            value,
        } => props
            .properties(row.entity, component, path)
            .iter()
            .any(|have| compare(have, *op, value)),
    }
}

/// Compares a property value with a literal; mismatched kinds never match.
fn compare(have: &PropValue, op: CompareOp, want: &Literal) -> bool {
    use std::cmp::Ordering;
    let ordering = match (have, want) {
        (PropValue::Number(a), Literal::Number(b)) => a.partial_cmp(b),
        (PropValue::Bool(a), Literal::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    };
    let text = |value: &PropValue| match value {
        PropValue::Number(n) => format!("{n}"),
        PropValue::Bool(b) => b.to_string(),
        PropValue::Text(t) => t.to_lowercase(),
    };
    let want_text = match want {
        Literal::Number(n) => format!("{n}"),
        Literal::Bool(b) => b.to_string(),
        Literal::Text(t) => t.clone(),
    };
    match op {
        CompareOp::Contains => text(have).contains(&want_text),
        CompareOp::Eq => match ordering {
            Some(o) => o == Ordering::Equal,
            None => matches!(want, Literal::Text(_)) && text(have) == want_text,
        },
        CompareOp::Ne => match ordering {
            Some(o) => o != Ordering::Equal,
            None => matches!(want, Literal::Text(_)) && text(have) != want_text,
        },
        CompareOp::Lt => ordering == Some(Ordering::Less),
        CompareOp::Le => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        CompareOp::Gt => ordering == Some(Ordering::Greater),
        CompareOp::Ge => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
    }
}

// -------------------------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Open,
    Close,
    And,
    Or,
    Not,
    /// A word (quotes removed, `quoted` when it was one quoted string).
    Word {
        text: String,
        quoted: bool,
        /// Per character of `text`: whether it stood inside quotes (operators there are text).
        literal: Vec<bool>,
    },
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    column: usize,
}

fn lex(text: &str) -> Result<Vec<Token>, QueryError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let column = i + 1;
        if c == '(' || c == ')' {
            tokens.push(Token {
                kind: if c == '(' {
                    TokenKind::Open
                } else {
                    TokenKind::Close
                },
                column,
            });
            i += 1;
            continue;
        }
        let mut word = String::new();
        let mut literal = Vec::new();
        let mut any_quote = false;
        let mut all_quoted = true;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || c == '(' || c == ')' {
                break;
            }
            if c == '"' {
                any_quote = true;
                let open = i + 1;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    word.push(chars[i]);
                    literal.push(true);
                    i += 1;
                }
                if !closed {
                    return Err(QueryError {
                        code: "query_unclosed_quote",
                        column: open,
                        message: "this quote is never closed".to_string(),
                    });
                }
                continue;
            }
            all_quoted = false;
            word.push(c);
            literal.push(false);
            i += 1;
        }
        let quoted = any_quote && all_quoted;
        let kind = if any_quote {
            TokenKind::Word {
                text: word,
                quoted,
                literal,
            }
        } else {
            match word.to_lowercase().as_str() {
                "and" => TokenKind::And,
                "or" => TokenKind::Or,
                "not" => TokenKind::Not,
                _ => TokenKind::Word {
                    text: word,
                    quoted: false,
                    literal,
                },
            }
        };
        tokens.push(Token { kind, column });
    }
    Ok(tokens)
}

// -------------------------------------------------------------------------------------------
// Parser

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    end_column: usize,
}

/// A parsed operand of an `and` run, with whether it was a bare name word (bare words side by
/// side join into one phrase, like today's search).
struct Operand {
    expr: Expr,
    bare: bool,
}

impl Parser {
    fn peek(&self) -> Option<&TokenKind> {
        self.tokens.get(self.at).map(|t| &t.kind)
    }

    fn column(&self) -> usize {
        self.tokens.get(self.at).map_or(self.end_column, |t| t.column)
    }

    fn deeper(&self, depth: usize) -> Result<usize, QueryError> {
        if depth >= MAX_DEPTH {
            return Err(QueryError {
                code: "query_too_deep",
                column: self.column(),
                message: format!("more than {MAX_DEPTH} nested parentheses or nots"),
            });
        }
        Ok(depth + 1)
    }

    fn or(&mut self, depth: usize) -> Result<Expr, QueryError> {
        let mut items = vec![self.and(depth)?];
        while matches!(self.peek(), Some(TokenKind::Or)) {
            self.at += 1;
            items.push(self.and(depth)?);
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            Expr::Or(items)
        })
    }

    fn and(&mut self, depth: usize) -> Result<Expr, QueryError> {
        let mut items: Vec<Operand> = Vec::new();
        loop {
            match self.peek() {
                None | Some(TokenKind::Close | TokenKind::Or) => {
                    if items.is_empty() {
                        return Err(self.missing_operand());
                    }
                    break;
                }
                Some(TokenKind::And) => {
                    if items.is_empty() {
                        return Err(self.missing_operand());
                    }
                    self.at += 1;
                    // An explicit `and` ends a run of bare words: the next operand never joins
                    // the phrase before it (a missing one is `query_missing_operand`).
                    let next = self.unary(depth)?;
                    items.push(next);
                }
                Some(_) => {
                    let next = self.unary(depth)?;
                    match (items.last_mut(), &next) {
                        (
                            Some(Operand {
                                expr:
                                    Expr::Term(Term::Name {
                                        op: CompareOp::Contains,
                                        text,
                                    }),
                                bare: true,
                            }),
                            Operand {
                                expr: Expr::Term(Term::Name { text: more, .. }),
                                bare: true,
                            },
                        ) => {
                            text.push(' ');
                            text.push_str(more);
                        }
                        _ => {
                            // A later `kind:` in a run replaces an earlier one (today's search).
                            if let Expr::Term(Term::Kind(_)) = next.expr
                                && let Some(at) = items
                                    .iter()
                                    .position(|o| matches!(o.expr, Expr::Term(Term::Kind(_))))
                            {
                                items.remove(at);
                            }
                            items.push(next);
                        }
                    }
                }
            }
        }
        let mut exprs: Vec<Expr> = items.into_iter().map(|o| o.expr).collect();
        Ok(if exprs.len() == 1 {
            exprs.remove(0)
        } else {
            Expr::And(exprs)
        })
    }

    fn missing_operand(&self) -> QueryError {
        QueryError {
            code: "query_missing_operand",
            column: self.column(),
            message: "a term is missing here (after and/or/not, or inside parentheses)".to_string(),
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Operand, QueryError> {
        let Some(token) = self.tokens.get(self.at).cloned() else {
            return Err(self.missing_operand());
        };
        match token.kind {
            TokenKind::Not => {
                let depth = self.deeper(depth)?;
                self.at += 1;
                let inner = self.unary(depth)?;
                Ok(Operand {
                    expr: Expr::Not(Box::new(inner.expr)),
                    bare: false,
                })
            }
            TokenKind::Open => {
                let depth = self.deeper(depth)?;
                self.at += 1;
                if matches!(self.peek(), Some(TokenKind::Close)) {
                    return Err(self.missing_operand());
                }
                let inner = self.or(depth)?;
                if !matches!(self.peek(), Some(TokenKind::Close)) {
                    return Err(QueryError {
                        code: "query_unclosed_paren",
                        column: token.column,
                        message: "this parenthesis is never closed".to_string(),
                    });
                }
                self.at += 1;
                Ok(Operand {
                    expr: inner,
                    bare: false,
                })
            }
            TokenKind::Close => Err(QueryError {
                code: "query_unexpected_paren",
                column: token.column,
                message: "this closing parenthesis has no opening one".to_string(),
            }),
            TokenKind::And | TokenKind::Or => Err(self.missing_operand()),
            TokenKind::Word {
                text,
                quoted,
                literal,
            } => {
                self.at += 1;
                let (term, bare) = word_term(&text, quoted, &literal, token.column)?;
                Ok(Operand {
                    expr: Expr::Term(term),
                    bare,
                })
            }
        }
    }
}

/// The keys of `key:value` terms.
const KEYS: &[&str] = &["kind", "type", "tag", "has", "under", "parent", "name"];

/// Parses one word into a term; the flag is whether it is a bare name word.
fn word_term(text: &str, quoted: bool, literal: &[bool], column: usize) -> Result<(Term, bool), QueryError> {
    let chars: Vec<char> = text.chars().collect();
    if quoted {
        return Ok((
            Term::Name {
                op: CompareOp::Contains,
                text: text.to_lowercase(),
            },
            false,
        ));
    }
    // The first unquoted operator character after the first character.
    let mut found: Option<(usize, &str)> = None;
    for i in 1..chars.len() {
        if literal.get(i).copied().unwrap_or(false) {
            continue;
        }
        let next = chars.get(i + 1).copied();
        let op = match (chars[i], next) {
            (':', _) => Some(":"),
            ('~', _) => Some("~"),
            ('!', Some('=')) => Some("!="),
            ('<', Some('=')) => Some("<="),
            ('>', Some('=')) => Some(">="),
            ('<', _) => Some("<"),
            ('>', _) => Some(">"),
            ('=', _) => Some("="),
            _ => None,
        };
        if let Some(op) = op {
            found = Some((i, op));
            break;
        }
    }
    let bare_name = || {
        (
            Term::Name {
                op: CompareOp::Contains,
                text: text.to_lowercase(),
            },
            true,
        )
    };
    let Some((at, op)) = found else {
        return Ok(bare_name());
    };
    let left: String = chars[..at].iter().collect();
    let value_start = at + op.chars().count();
    let value: String = chars[value_start..].iter().collect();
    let value_quoted = literal.get(value_start).copied().unwrap_or(false);
    let value_column = column + value_start;
    let key = left.to_lowercase();
    if op == ":" {
        if !KEYS.contains(&key.as_str()) {
            // Not a key the language knows: name text, as today.
            return Ok(bare_name());
        }
        let value_lower = value.to_lowercase();
        let missing = || QueryError {
            code: "query_missing_value",
            column: value_column,
            message: format!("`{key}:` needs a value"),
        };
        let term = match key.as_str() {
            "kind" => match value_lower.as_str() {
                "" => Term::True,
                "none" => Term::Kind(KindFilter::None),
                "?" | "unclassified" => Term::Kind(KindFilter::Unclassified),
                name => Term::Kind(KindFilter::Name(name.to_string())),
            },
            "type" | "has" => {
                if value_lower.is_empty() {
                    if key == "has" {
                        return Err(missing());
                    }
                    Term::True
                } else {
                    let short = value_lower
                        .rsplit("::")
                        .next()
                        .unwrap_or(&value_lower)
                        .to_string();
                    Term::Has(short)
                }
            }
            "tag" => {
                if value_lower.is_empty() {
                    Term::True
                } else {
                    Term::Tag(value_lower)
                }
            }
            "under" | "parent" => {
                let pattern = PathPattern::parse(&value);
                if pattern.segments.is_empty() {
                    return Err(missing());
                }
                if key == "under" {
                    Term::Under(pattern)
                } else {
                    Term::Parent(pattern)
                }
            }
            _ => {
                if value.is_empty() {
                    return Err(missing());
                }
                Term::Name {
                    op: CompareOp::Contains,
                    text: value_lower,
                }
            }
        };
        return Ok((term, false));
    }
    let op = match op {
        "~" => CompareOp::Contains,
        "=" => CompareOp::Eq,
        "!=" => CompareOp::Ne,
        "<" => CompareOp::Lt,
        "<=" => CompareOp::Le,
        ">" => CompareOp::Gt,
        _ => CompareOp::Ge,
    };
    if value.is_empty() && !value_quoted {
        return Err(QueryError {
            code: "query_missing_value",
            column: value_column,
            message: format!("`{left}{}` needs a value", op.symbol()),
        });
    }
    let literal_value = if value_quoted {
        Literal::Text(value.to_lowercase())
    } else if let Ok(number) = value.parse::<f64>()
        && number.is_finite()
    {
        Literal::Number(number)
    } else {
        match value.to_lowercase().as_str() {
            "true" => Literal::Bool(true),
            "false" => Literal::Bool(false),
            other => Literal::Text(other.to_string()),
        }
    };
    if op.ordered() && !matches!(literal_value, Literal::Number(_)) {
        return Err(QueryError {
            code: "query_not_a_number",
            column: value_column,
            message: format!("`{}` compares numbers; `{value}` is not one", op.symbol()),
        });
    }
    if key == "name" {
        return Ok((
            Term::Name {
                op,
                text: match literal_value {
                    Literal::Number(n) => format!("{n}"),
                    Literal::Bool(b) => b.to_string(),
                    Literal::Text(t) => t,
                },
            },
            false,
        ));
    }
    let (component, path) = match key.split_once('.') {
        Some((component, path)) => (component.to_string(), path.to_string()),
        None => (key.clone(), String::new()),
    };
    let valid_ident = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '[' || c == ']')
    };
    if !valid_ident(&component)
        || key.ends_with('.')
        || (!path.is_empty() && (!valid_ident(&path) || path.starts_with('.') || path.ends_with('.')))
    {
        return Err(QueryError {
            code: "query_bad_field",
            column,
            message: format!("`{left}` is not a Component.field path"),
        });
    }
    // Field paths are case-sensitive in reflection; component names are not.
    let original_path = left
        .split_once('.')
        .map(|(_, p)| p.to_string())
        .unwrap_or_default();
    Ok((
        Term::Property {
            component,
            path: original_path,
            op,
            value: literal_value,
        },
        false,
    ))
}

#[cfg(test)]
mod tests;
