//! Predicate filters over parsed logfmt pairs. Multiple predicates are AND-ed.
//!
//! Spec strings: `<key><op><value>`, where op ∈ `=`, `!=`, `<`, `<=`, `>`,
//! `>=`, `=~` (regex). Numeric comparison ops fall back to lexicographic when
//! either side is not parseable as `f64`.

use regex::Regex;

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

/// Severity rank for log-level strings. Ascending order:
/// trace < debug < info < notice < warn < error < critical < fatal.
/// Returns `None` for unknown strings.
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

#[derive(Debug, Default)]
pub struct Filter {
    preds: Vec<Predicate>,
}

impl Filter {
    pub fn parse(specs: &[String]) -> anyhow::Result<Self> {
        let mut preds = Vec::with_capacity(specs.len());
        for s in specs {
            preds.push(parse_one(s)?);
        }
        Ok(Filter { preds })
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.preds.is_empty()
    }

    /// Return true iff every predicate is satisfied by the pairs.
    pub fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        self.preds.iter().all(|p| p.matches(pairs))
    }
}

impl Predicate {
    fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        // A predicate matches if at least one pair with the matching key
        // satisfies the operator. (Same as ripgrep semantics for repeated
        // logfmt keys.)
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
        // For `!=`, absence of the key counts as satisfied (the constraint is
        // vacuously true). Other ops require the key to be present.
        !any_seen && matches!(self.op, Op::Ne)
    }

    fn eval(&self, value: &str) -> bool {
        match self.op {
            Op::Eq => value == self.rhs,
            Op::Ne => value != self.rhs,
            Op::ReMatch => self.re.as_ref().is_some_and(|re| re.is_match(value)),
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                use std::cmp::Ordering;
                let cmp = if let (Some(a), Some(b)) = (level_rank(value), self.rhs_level) {
                    a.cmp(&b)
                } else if let (Ok(a), Some(b)) = (value.parse::<f64>(), self.rhs_num) {
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

fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn parse_one(spec: &str) -> anyhow::Result<Predicate> {
    // Longest-match operator scan.
    let (op_pos, op_len, op) = find_op(spec).ok_or_else(|| {
        anyhow::anyhow!("filter `{spec}` has no operator (=, !=, <, <=, >, >=, =~)")
    })?;
    let key = &spec[..op_pos];
    let rhs = &spec[op_pos + op_len..];
    if key.is_empty() {
        anyhow::bail!("filter `{spec}` has empty key");
    }
    let rhs_num = rhs.parse::<f64>().ok();
    let rhs_level = level_rank(rhs);
    let re = if matches!(op, Op::ReMatch) {
        Some(Regex::new(rhs).map_err(|e| anyhow::anyhow!("invalid regex in `{spec}`: {e}"))?)
    } else {
        None
    };
    Ok(Predicate {
        key: key.to_string(),
        op,
        rhs: rhs.to_string(),
        rhs_num,
        rhs_level,
        re,
    })
}

fn find_op(spec: &str) -> Option<(usize, usize, Op)> {
    let b = spec.as_bytes();
    // Order matters: longer ops first within each starting char.
    for i in 0..b.len() {
        match b[i] {
            b'=' => {
                if b.get(i + 1) == Some(&b'~') {
                    return Some((i, 2, Op::ReMatch));
                }
                return Some((i, 1, Op::Eq));
            }
            b'!' => {
                if b.get(i + 1) == Some(&b'=') {
                    return Some((i, 2, Op::Ne));
                }
            }
            b'<' => {
                if b.get(i + 1) == Some(&b'=') {
                    return Some((i, 2, Op::Le));
                }
                return Some((i, 1, Op::Lt));
            }
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    return Some((i, 2, Op::Ge));
                }
                return Some((i, 1, Op::Gt));
            }
            _ => {}
        }
    }
    None
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
        // Non-numeric comparison goes lexicographic.
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
        let f = filter(&["msg=hello world"]);
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
        // debug < info < warn < error < critical
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
}
