//! Boolean expression filters over parsed logfmt pairs.
//!
//! Each `--if` / `--where` argument is a boolean expression with:
//! - leaf predicates `<key> <op> <value>` (`=`, `!=`, `<`, `<=`, `>`, `>=`,
//!   `=~`)
//! - logical operators `and`, `or`, `not` (case-insensitive; `&&`, `||`,
//!   `!` also accepted)
//! - parenthesised grouping
//! - double- or single-quoted strings for values that contain whitespace
//!
//! Multiple `--if` flags are AND-ed together.
//!
//! Examples:
//! - `level=error`
//! - `level >= warn and (facil=net or facil=db)`
//! - `dur >= 100 and not msg =~ "noisy.*timeout"`
//!
//! Quantifier note: a leaf predicate is satisfied if *any* pair with the
//! matching key satisfies the operator. Consequently `level != info` and
//! `not level = info` agree on lines where `level` appears once or is
//! absent, but diverge on lines that carry multiple `level=` pairs:
//! `level != info` is true if any value is non-info, while `not level=info`
//! is true only if no value is info.

use regex::Regex;

use crate::timestamp::strip_quotes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    ReMatch,
}

#[derive(Debug)]
struct Predicate {
    key: String,
    op: Op,
    rhs: String,
    rhs_num: Option<f64>,
    rhs_level: Option<u8>,
    re: Option<Regex>,
}

impl Predicate {
    fn new(key: String, op: Op, rhs: String) -> anyhow::Result<Self> {
        let rhs_num = rhs.parse::<f64>().ok();
        let rhs_level = level_rank(&rhs);
        let re = if matches!(op, Op::ReMatch) {
            Some(Regex::new(&rhs).map_err(|e| anyhow::anyhow!("invalid regex `{rhs}`: {e}"))?)
        } else {
            None
        };
        Ok(Predicate {
            key,
            op,
            rhs,
            rhs_num,
            rhs_level,
            re,
        })
    }

    fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        // A predicate matches if at least one pair with the matching key
        // satisfies the operator.
        let mut any_seen = false;
        for (k, v) in pairs {
            if *k != self.key {
                continue;
            }
            any_seen = true;
            let value = strip_quotes(v);
            if self.eval(value) {
                return true;
            }
        }
        // For `!=`, absence of the key counts as satisfied (vacuously true).
        // Other ops require the key to be present.
        !any_seen && matches!(self.op, Op::Ne)
    }

    fn eval(&self, value: &str) -> bool {
        match self.op {
            Op::Eq => value == self.rhs,
            Op::Ne => value != self.rhs,
            Op::ReMatch => self.re.as_ref().is_some_and(|re| re.is_match(value)),
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                use std::cmp::Ordering;
                // Discriminate on the parse-time-known rhs first so we don't
                // do per-line value-side work that's guaranteed to be unused
                // (e.g. lowercase-into-buffer for `dur>=100`).
                let cmp = if let Some(b) = self.rhs_level
                    && let Some(a) = level_rank(value)
                {
                    a.cmp(&b)
                } else if let Some(b) = self.rhs_num
                    && let Ok(a) = value.parse::<f64>()
                {
                    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
                } else {
                    value.cmp(self.rhs.as_str())
                };
                match self.op {
                    Op::Lt => cmp == Ordering::Less,
                    Op::Le => cmp != Ordering::Greater,
                    Op::Gt => cmp == Ordering::Greater,
                    Op::Ge => cmp != Ordering::Less,
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[derive(Debug)]
enum Expr {
    Pred(Predicate),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
}

impl Expr {
    fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        match self {
            Self::Pred(p) => p.matches(pairs),
            Self::And(xs) => xs.iter().all(|x| x.matches(pairs)),
            Self::Or(xs) => xs.iter().any(|x| x.matches(pairs)),
            Self::Not(x) => !x.matches(pairs),
        }
    }
}

#[derive(Debug, Default)]
pub struct Filter {
    exprs: Vec<Expr>,
}

impl Filter {
    pub fn parse(specs: &[String]) -> anyhow::Result<Self> {
        let mut exprs = Vec::with_capacity(specs.len());
        for s in specs {
            exprs.push(parse_expr(s)?);
        }
        Ok(Filter { exprs })
    }

    pub fn parse_one(spec: &str) -> anyhow::Result<Self> {
        Ok(Filter {
            exprs: vec![parse_expr(spec)?],
        })
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty()
    }

    pub fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        self.exprs.iter().all(|e| e.matches(pairs))
    }
}

/// Severity rank for log-level strings. Ascending order:
/// trace < debug < info < notice < warn < error < critical < fatal.
fn level_rank(s: &str) -> Option<u8> {
    let mut buf = [0u8; 16];
    let bytes = s.as_bytes();
    if bytes.len() > buf.len() {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    match &buf[..bytes.len()] {
        b"trace" => Some(0),
        b"debug" => Some(1),
        b"info" => Some(2),
        b"notice" => Some(3),
        b"warn" | b"warning" => Some(4),
        b"error" | b"err" => Some(5),
        b"critical" | b"crit" => Some(6),
        b"fatal" | b"alert" | b"emerg" | b"panic" => Some(7),
        _ => None,
    }
}

// ─── tokenizer ──────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Op(Op),
    Word(String),
}

fn tokenize(input: &str) -> anyhow::Result<Vec<Token>> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' => {
                out.push(Token::LParen);
                i += 1;
            }
            b')' => {
                out.push(Token::RParen);
                i += 1;
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'~') {
                    out.push(Token::Op(Op::ReMatch));
                    i += 2;
                } else {
                    out.push(Token::Op(Op::Eq));
                    i += 1;
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(Op::Ne));
                    i += 2;
                } else {
                    out.push(Token::Not);
                    i += 1;
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(Op::Le));
                    i += 2;
                } else {
                    out.push(Token::Op(Op::Lt));
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Token::Op(Op::Ge));
                    i += 2;
                } else {
                    out.push(Token::Op(Op::Gt));
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let quote = c;
                i += 1;
                let mut buf = String::new();
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        let esc = bytes[i + 1];
                        buf.push(match esc {
                            b'n' => '\n',
                            b't' => '\t',
                            b'r' => '\r',
                            b'\\' => '\\',
                            b'"' => '"',
                            b'\'' => '\'',
                            _ => esc as char,
                        });
                        i += 2;
                    } else {
                        // Single byte (ASCII fast path); fall through to char for non-ASCII.
                        let ch = input[i..].chars().next().unwrap();
                        buf.push(ch);
                        i += ch.len_utf8();
                    }
                }
                if i >= bytes.len() {
                    anyhow::bail!("unterminated quoted string in `{input}`");
                }
                i += 1; // consume closing quote
                out.push(Token::Word(buf));
            }
            b'&' if bytes.get(i + 1) == Some(&b'&') => {
                out.push(Token::And);
                i += 2;
            }
            b'|' if bytes.get(i + 1) == Some(&b'|') => {
                out.push(Token::Or);
                i += 2;
            }
            _ => {
                let start = i;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b.is_ascii_whitespace()
                        || matches!(b, b'(' | b')' | b'=' | b'!' | b'<' | b'>' | b'"' | b'\'')
                    {
                        break;
                    }
                    i += 1;
                }
                let word = &input[start..i];
                let tok = match word.to_ascii_lowercase().as_str() {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    _ => Token::Word(word.to_string()),
                };
                out.push(tok);
            }
        }
    }
    Ok(out)
}

// ─── recursive-descent parser ───────────────────────────────────────────

struct ParserState {
    iter: std::iter::Peekable<std::vec::IntoIter<Token>>,
}

impl ParserState {
    fn new(toks: Vec<Token>) -> Self {
        Self {
            iter: toks.into_iter().peekable(),
        }
    }

    fn peek(&mut self) -> Option<&Token> {
        self.iter.peek()
    }

    fn advance(&mut self) -> Option<Token> {
        self.iter.next()
    }

    fn parse_or(&mut self) -> anyhow::Result<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            let right = self.parse_and()?;
            left = match left {
                Expr::Or(mut xs) => {
                    xs.push(right);
                    Expr::Or(xs)
                }
                other => Expr::Or(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> anyhow::Result<Expr> {
        let mut left = self.parse_not()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.advance();
            let right = self.parse_not()?;
            left = match left {
                Expr::And(mut xs) => {
                    xs.push(right);
                    Expr::And(xs)
                }
                other => Expr::And(vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> anyhow::Result<Expr> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.advance();
            return Ok(Expr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> anyhow::Result<Expr> {
        match self.peek() {
            Some(Token::LParen) => {
                self.advance();
                let e = self.parse_or()?;
                match self.advance() {
                    Some(Token::RParen) => Ok(e),
                    _ => anyhow::bail!("expected `)`"),
                }
            }
            _ => Ok(Expr::Pred(self.parse_predicate()?)),
        }
    }

    fn parse_predicate(&mut self) -> anyhow::Result<Predicate> {
        let key = match self.advance() {
            Some(Token::Word(w)) => w,
            Some(t) => anyhow::bail!("expected key, got {t:?}"),
            None => anyhow::bail!("expected key"),
        };
        let op = match self.advance() {
            Some(Token::Op(op)) => op,
            Some(t) => anyhow::bail!("expected operator after `{key}`, got {t:?}"),
            None => anyhow::bail!("expected operator after `{key}`"),
        };
        let rhs = match self.advance() {
            Some(Token::Word(w)) => w,
            Some(t) => anyhow::bail!("expected value after `{key}` operator, got {t:?}"),
            None => anyhow::bail!("expected value after `{key}` operator"),
        };
        Predicate::new(key, op, rhs)
    }
}

fn parse_expr(input: &str) -> anyhow::Result<Expr> {
    let toks = tokenize(input)?;
    if toks.is_empty() {
        anyhow::bail!("empty filter expression");
    }
    let mut p = ParserState::new(toks);
    let e = p.parse_or()?;
    if p.peek().is_some() {
        anyhow::bail!("trailing tokens after expression in `{input}`");
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(specs: &[&str]) -> Filter {
        let owned: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        Filter::parse(&owned).unwrap()
    }

    #[test]
    fn eq_basic() {
        let f = filter(&["level=error"]);
        assert!(f.matches(&[("level", "error"), ("msg", "boom")]));
        assert!(!f.matches(&[("level", "info"), ("msg", "ok")]));
    }

    #[test]
    fn ne_absent_key_passes() {
        let f = filter(&["level!=info"]);
        assert!(f.matches(&[("msg", "hi")]));
        assert!(!f.matches(&[("level", "info")]));
        assert!(f.matches(&[("level", "warn")]));
    }

    #[test]
    fn numeric_compare() {
        let f = filter(&["dur>=100"]);
        assert!(f.matches(&[("dur", "150")]));
        assert!(f.matches(&[("dur", "100")]));
        assert!(!f.matches(&[("dur", "99")]));
    }

    #[test]
    fn numeric_falls_back_to_lex() {
        let f = filter(&["name>=alpha"]);
        assert!(f.matches(&[("name", "beta")]));
        assert!(!f.matches(&[("name", "Alpha")]));
    }

    #[test]
    fn regex_match() {
        let f = filter(&["msg=~connection.*reset"]);
        assert!(f.matches(&[("msg", "connection by peer was reset")]));
        assert!(!f.matches(&[("msg", "all good")]));
    }

    #[test]
    fn quoted_value_stripped() {
        let f = filter(&["msg=\"hello world\""]);
        assert!(f.matches(&[("msg", "\"hello world\"")]));
    }

    #[test]
    fn multiple_anded() {
        let f = filter(&["level=error", "dur>=100"]);
        assert!(f.matches(&[("level", "error"), ("dur", "200")]));
        assert!(!f.matches(&[("level", "error"), ("dur", "50")]));
        assert!(!f.matches(&[("level", "info"), ("dur", "200")]));
    }

    #[test]
    fn empty_filter_matches_all() {
        let f = Filter::default();
        assert!(f.matches(&[]));
        assert!(f.matches(&[("anything", "goes")]));
    }

    #[test]
    fn level_severity_ordering() {
        let f = filter(&["level>=warn"]);
        assert!(!f.matches(&[("level", "debug")]));
        assert!(!f.matches(&[("level", "info")]));
        assert!(f.matches(&[("level", "warn")]));
        assert!(f.matches(&[("level", "error")]));
        assert!(f.matches(&[("level", "critical")]));

        let f = filter(&["level<error"]);
        assert!(f.matches(&[("level", "debug")]));
        assert!(f.matches(&[("level", "info")]));
        assert!(f.matches(&[("level", "warn")]));
        assert!(!f.matches(&[("level", "error")]));
        assert!(!f.matches(&[("level", "critical")]));
    }

    #[test]
    fn level_severity_case_insensitive() {
        let f = filter(&["level>=ERROR"]);
        assert!(f.matches(&[("level", "Error")]));
        assert!(f.matches(&[("level", "CRITICAL")]));
        assert!(!f.matches(&[("level", "warn")]));
    }

    #[test]
    fn parse_errors() {
        assert!(Filter::parse(&["nofoperator".to_string()]).is_err());
        assert!(Filter::parse(&["=novalue".to_string()]).is_err());
        assert!(Filter::parse(&["msg=~[invalid".to_string()]).is_err());
    }

    #[test]
    fn boolean_and_with_spaces() {
        let f = filter(&["level >= warn and facil = net"]);
        assert!(f.matches(&[("level", "error"), ("facil", "net")]));
        assert!(!f.matches(&[("level", "info"), ("facil", "net")]));
        assert!(!f.matches(&[("level", "error"), ("facil", "auth")]));
    }

    #[test]
    fn boolean_or() {
        let f = filter(&["facil=net or facil=db"]);
        assert!(f.matches(&[("facil", "net")]));
        assert!(f.matches(&[("facil", "db")]));
        assert!(!f.matches(&[("facil", "auth")]));
    }

    #[test]
    fn precedence_or_inside_and_with_parens() {
        let f = filter(&["level >= warn and (facil=net or facil=db)"]);
        assert!(f.matches(&[("level", "error"), ("facil", "net")]));
        assert!(f.matches(&[("level", "warn"), ("facil", "db")]));
        assert!(!f.matches(&[("level", "info"), ("facil", "net")]));
        assert!(!f.matches(&[("level", "error"), ("facil", "auth")]));
    }

    #[test]
    fn precedence_and_binds_tighter_than_or() {
        // a or b and c  ≡  a or (b and c)
        let f = filter(&["facil=db or level=error and msg=fail"]);
        assert!(f.matches(&[("facil", "db"), ("level", "info"), ("msg", "ok")]));
        assert!(f.matches(&[("facil", "x"), ("level", "error"), ("msg", "fail")]));
        assert!(!f.matches(&[("facil", "x"), ("level", "error"), ("msg", "ok")]));
    }

    #[test]
    fn negation() {
        let f = filter(&["not level=info"]);
        assert!(f.matches(&[("level", "warn")]));
        assert!(!f.matches(&[("level", "info")]));

        // `!` alias
        let f = filter(&["!level=info"]);
        assert!(f.matches(&[("level", "warn")]));
        assert!(!f.matches(&[("level", "info")]));

        let f = filter(&["not (level=info or level=debug)"]);
        assert!(f.matches(&[("level", "warn")]));
        assert!(!f.matches(&[("level", "info")]));
        assert!(!f.matches(&[("level", "debug")]));
    }

    #[test]
    fn quoted_value_with_spaces() {
        let f = filter(&["msg = \"hello world\""]);
        assert!(f.matches(&[("msg", "hello world")]));
        assert!(!f.matches(&[("msg", "hello")]));
    }

    #[test]
    fn keywords_case_insensitive() {
        let f = filter(&["level=warn AND facil=net"]);
        assert!(f.matches(&[("level", "warn"), ("facil", "net")]));
        let f = filter(&["facil=net OR facil=db"]);
        assert!(f.matches(&[("facil", "db")]));
        let f = filter(&["NOT level=info"]);
        assert!(f.matches(&[("level", "warn")]));
    }

    #[test]
    fn ampersand_pipe_aliases() {
        let f = filter(&["level=warn && facil=net"]);
        assert!(f.matches(&[("level", "warn"), ("facil", "net")]));
        let f = filter(&["facil=net || facil=db"]);
        assert!(f.matches(&[("facil", "db")]));
    }
}
