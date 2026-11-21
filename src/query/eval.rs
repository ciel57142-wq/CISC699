//! Expression evaluation shared by every operator: SQL three-valued logic,
//! arithmetic, comparisons, scalar functions, date/time values and
//! subquery hooks. Expressions arrive already bound (names -> slots).

use crate::error::{Error, Result};
use crate::pbsql::ast::{Expr, Op, SubRef, Ty};
use crate::value::Value;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// Runs a bound subquery for the evaluator. `outer` is the stack of
/// enclosing rows (innermost first) that correlated references read.
pub trait SubRunner {
    /// Rows of subquery `id`; `limit` caps how many are needed (EXISTS needs 1).
    fn run_sub(
        &self,
        id: usize,
        outer: &[Vec<Value>],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>>;
}

/// No subqueries available (DDL defaults, constant folding).
pub struct NoSubs;
impl SubRunner for NoSubs {
    fn run_sub(&self, _: usize, _: &[Vec<Value>], _: Option<usize>) -> Result<Vec<Vec<Value>>> {
        Err(Error::Parse("subqueries are not allowed here".into()))
    }
}

pub struct Env<'a> {
    pub row: &'a [Value],
    pub outer: &'a [Vec<Value>],
    pub subs: &'a dyn SubRunner,
}

impl<'a> Env<'a> {
    pub fn new(row: &'a [Value], outer: &'a [Vec<Value>], subs: &'a dyn SubRunner) -> Env<'a> {
        Env { row, outer, subs }
    }
}

// ------------------------------------------------------------------ dates

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn num(s: &str, digits: usize) -> Option<u32> {
    (s.len() == digits && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn parse_date(s: &str) -> Option<String> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let y = num(y, 4)? as i64;
    let m = num(m, 2).or_else(|| num(m, 1))?;
    let d = num(d, 2).or_else(|| num(d, 1))?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

fn parse_time(s: &str) -> Option<String> {
    let (hms, frac) = match s.split_once('.') {
        Some((a, f)) if !f.is_empty() && f.len() <= 9 && f.bytes().all(|b| b.is_ascii_digit()) => {
            (a, Some(f))
        }
        Some(_) => return None,
        None => (s, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, mi, sec) = match parts.as_slice() {
        [h, m] => (*h, *m, "00"),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    let h = num(h, 2).or_else(|| num(h, 1))?;
    let mi = num(mi, 2)?;
    let sec = num(sec, 2)?;
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let mut out = format!("{h:02}:{mi:02}:{sec:02}");
    if let Some(f) = frac {
        let f = f.trim_end_matches('0');
        if !f.is_empty() {
            out.push('.');
            out.push_str(&f[..f.len().min(6)]);
        }
    }
    Some(out)
}

fn parse_timestamp(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('Z');
    let (d, t) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, t.trim()),
        None => (s, "00:00:00"),
    };
    Some(format!("{} {}", parse_date(d)?, parse_time(t)?))
}

/// Days since 1970-01-01 -> (year, month, day). (Howard Hinnant's algorithm.)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current UTC time as a canonical TIMESTAMP string.
pub fn now_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        t / 3600,
        (t / 60) % 60,
        t % 60
    )
}

/// Convert a value to a column type, validating and canonicalising it.
/// Dates and times are stored as canonical ISO text, whose byte order is
/// chronological order, so comparisons and indexes work on them directly.
pub fn coerce_type(ty: Ty, v: Value) -> Result<Value> {
    let bad = |v: &Value| {
        Error::Type(format!(
            "expected {}, got {}",
            ty.name().to_uppercase(),
            v.to_json()
        ))
    };
    Ok(match (ty, v) {
        (_, Value::Null) => Value::Null,
        (Ty::Int, Value::Int(i)) => Value::Int(i),
        (Ty::Int, Value::Float(f)) if f.fract() == 0.0 && f.abs() < 9e18 => Value::Int(f as i64),
        (Ty::Float, Value::Int(i)) => Value::Float(i as f64),
        (Ty::Float, Value::Float(f)) => Value::Float(f),
        (Ty::Text, Value::Text(s)) => Value::Text(s),
        (Ty::Bool, Value::Bool(b)) => Value::Bool(b),
        (Ty::Date, Value::Text(s)) => {
            let s2 = s.trim();
            let d = parse_date(s2).or_else(|| parse_timestamp(s2).map(|t| t[..10].to_string()));
            Value::Text(
                d.ok_or_else(|| Error::Type(format!("invalid DATE '{s}' (expected YYYY-MM-DD)")))?,
            )
        }
        (Ty::Time, Value::Text(s)) => Value::Text(
            parse_time(s.trim())
                .ok_or_else(|| Error::Type(format!("invalid TIME '{s}' (expected HH:MM:SS)")))?,
        ),
        (Ty::Timestamp, Value::Text(s)) => Value::Text(parse_timestamp(&s).ok_or_else(|| {
            Error::Type(format!(
                "invalid TIMESTAMP '{s}' (expected YYYY-MM-DD HH:MM:SS)"
            ))
        })?),
        (Ty::Json, Value::Text(s)) => match Value::parse(&s) {
            Ok(v @ (Value::Object(_) | Value::Array(_))) => v,
            _ => Value::Text(s),
        },
        (Ty::Json, v) => v,
        (_, v) => return Err(bad(&v)),
    })
}

/// CAST: like coercion, but also converts between text and numbers.
fn cast(v: Value, ty: Ty) -> Result<Value> {
    match (ty, &v) {
        (_, Value::Null) => Ok(Value::Null),
        (Ty::Text, Value::Text(_)) => Ok(v),
        (Ty::Text, Value::Object(_) | Value::Array(_)) => Ok(Value::Text(v.to_json())),
        (Ty::Text, _) => Ok(Value::Text(v.display())),
        (Ty::Int, Value::Text(s)) => s
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .or_else(|_| {
                s.trim()
                    .parse::<f64>()
                    .map(|f| Value::Int(f.trunc() as i64))
            })
            .map_err(|_| Error::Type(format!("cannot cast '{s}' to INT"))),
        (Ty::Int, Value::Float(f)) => Ok(Value::Int(f.trunc() as i64)),
        (Ty::Int, Value::Bool(b)) => Ok(Value::Int(*b as i64)),
        (Ty::Float, Value::Text(s)) => s
            .trim()
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| Error::Type(format!("cannot cast '{s}' to REAL"))),
        (Ty::Bool, Value::Text(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "f" | "no" | "0" => Ok(Value::Bool(false)),
            _ => Err(Error::Type(format!("cannot cast '{s}' to BOOL"))),
        },
        (Ty::Bool, Value::Int(i)) => Ok(Value::Bool(*i != 0)),
        _ => coerce_type(ty, v),
    }
}

// ------------------------------------------------------------- operators

/// SQL three-valued logic: NULL is "unknown".
pub fn tri(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::Type(format!(
            "expected a boolean, got {}",
            other.to_json()
        ))),
    }
}

pub fn is_true(v: &Value) -> Result<bool> {
    Ok(tri(v)? == Some(true))
}

fn from_tri(t: Option<bool>) -> Value {
    t.map_or(Value::Null, Value::Bool)
}

pub fn like(s: &[char], p: &[char]) -> bool {
    // iterative wildcard match with backtracking to the last '%'
    let (mut si, mut pi) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == s[si]) && p[pi] != '%' {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

pub fn arith(op: Op, l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        if matches!(op, Op::Div | Op::Mod) && *b == 0 {
            return Err(Error::Type("division by zero".into()));
        }
        let res = match op {
            Op::Add => a.checked_add(*b),
            Op::Sub => a.checked_sub(*b),
            Op::Mul => a.checked_mul(*b),
            Op::Div => a.checked_div(*b),
            _ => a.checked_rem(*b),
        };
        return res
            .map(Value::Int)
            .ok_or_else(|| Error::Type("integer overflow".into()));
    }
    match (l.as_f64(), r.as_f64()) {
        (Some(a), Some(b)) => Ok(Value::Float(match op {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div => a / b,
            _ => a % b,
        })),
        _ => Err(Error::Type(format!(
            "cannot apply '{}' to {} and {}",
            op.text(),
            l.to_json(),
            r.to_json()
        ))),
    }
}

/// SQL comparison: NULL if either side is NULL.
pub fn compare(op: Op, l: &Value, r: &Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    if !matches!(op, Op::Eq | Op::Ne) && l.rank() != r.rank() {
        // ordering values of different types (a number and a text) is unknown
        return Value::Null;
    }
    let o = l.total_cmp(r);
    Value::Bool(match op {
        Op::Eq => o == Ordering::Equal,
        Op::Ne => o != Ordering::Equal,
        Op::Lt => o == Ordering::Less,
        Op::Le => o != Ordering::Greater,
        Op::Gt => o == Ordering::Greater,
        _ => o != Ordering::Less,
    })
}

/// Follow a dotted path into a value. Text holding a JSON object or array
/// (as key-value values usually do) is looked into as well. Missing -> NULL.
pub fn walk_path(v: &Value, path: &[String]) -> Value {
    if path.is_empty() {
        return v.clone();
    }
    let parsed;
    let mut cur = v;
    if let Value::Text(s) = v {
        let t = s.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            match Value::parse(s) {
                Ok(p) => {
                    parsed = p;
                    cur = &parsed;
                }
                Err(_) => return Value::Null,
            }
        }
    }
    for part in path {
        cur = match cur {
            Value::Object(o) => match o.get(part) {
                Some(x) => x,
                None => return Value::Null,
            },
            Value::Array(a) => match part.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(x) => x,
                None => return Value::Null,
            },
            _ => return Value::Null,
        };
    }
    cur.clone()
}

fn date_part(name: &str, v: &Value) -> Result<Value> {
    let Value::Text(s) = v else {
        return if v.is_null() {
            Ok(Value::Null)
        } else {
            Err(Error::Type(format!("{name}() needs a date or time")))
        };
    };
    let ts = parse_timestamp(s);
    let time = parse_time(s);
    let field = |a: usize, b: usize, src: &str| {
        src.get(a..b)
            .and_then(|x| x.parse::<i64>().ok())
            .map(Value::Int)
    };
    let out = match name {
        "year" => ts.as_deref().and_then(|t| field(0, 4, t)),
        "month" => ts.as_deref().and_then(|t| field(5, 7, t)),
        "day" => ts.as_deref().and_then(|t| field(8, 10, t)),
        "hour" => ts
            .as_deref()
            .map(|t| &t[11..])
            .or(time.as_deref())
            .and_then(|t| field(0, 2, t)),
        "minute" => ts
            .as_deref()
            .map(|t| &t[11..])
            .or(time.as_deref())
            .and_then(|t| field(3, 5, t)),
        "second" => ts
            .as_deref()
            .map(|t| &t[11..])
            .or(time.as_deref())
            .and_then(|t| field(6, 8, t)),
        _ => None,
    };
    out.ok_or_else(|| Error::Type(format!("{name}() cannot read '{s}' as a date/time")))
}

fn func(name: &str, vals: Vec<Value>) -> Result<Value> {
    let one = || match vals.as_slice() {
        [v] => Ok(v.clone()),
        _ => Err(Error::Parse(format!("{name}() takes exactly one argument"))),
    };
    Ok(match name {
        "version" => Value::Text(format!("polydb {}", env!("CARGO_PKG_VERSION"))),
        "coalesce" | "ifnull" => vals
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => match vals.as_slice() {
            [a, b] => {
                if !a.is_null() && a.total_cmp(b) == Ordering::Equal {
                    Value::Null
                } else {
                    a.clone()
                }
            }
            _ => return Err(Error::Parse("nullif() takes two arguments".into())),
        },
        "concat" => Value::Text(
            vals.iter()
                .filter(|v| !v.is_null())
                .map(Value::display)
                .collect(),
        ),
        "lower" | "upper" | "length" | "trim" => match one()? {
            Value::Null => Value::Null,
            Value::Text(s) => match name {
                "lower" => Value::Text(s.to_lowercase()),
                "upper" => Value::Text(s.to_uppercase()),
                "trim" => Value::Text(s.trim().to_string()),
                _ => Value::Int(s.chars().count() as i64),
            },
            Value::Array(a) if name == "length" => Value::Int(a.len() as i64),
            other => {
                return Err(Error::Type(format!(
                    "{name}() needs text, got {}",
                    other.to_json()
                )))
            }
        },
        "substr" | "substring" => match vals.as_slice() {
            [Value::Null, ..] => Value::Null,
            [Value::Text(s), Value::Int(start), rest @ ..] => {
                let chars: Vec<char> = s.chars().collect();
                let from = (*start - 1).max(0) as usize;
                let len = match rest {
                    [Value::Int(n)] => (*n).max(0) as usize,
                    [] => chars.len(),
                    _ => {
                        return Err(Error::Type(
                            "substr(text, start[, length]) needs integers".into(),
                        ))
                    }
                };
                Value::Text(chars.iter().skip(from).take(len).collect())
            }
            _ => {
                return Err(Error::Type(
                    "substr(text, start[, length]) needs text and integers".into(),
                ))
            }
        },
        "abs" => match one()? {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(
                i.checked_abs()
                    .ok_or_else(|| Error::Type("integer overflow".into()))?,
            ),
            Value::Float(f) => Value::Float(f.abs()),
            other => {
                return Err(Error::Type(format!(
                    "abs() needs a number, got {}",
                    other.to_json()
                )))
            }
        },
        "round" => match vals.as_slice() {
            [Value::Null, ..] => Value::Null,
            [v] => match v {
                Value::Int(i) => Value::Int(*i),
                _ => Value::Float(
                    v.as_f64()
                        .ok_or_else(|| Error::Type("round() needs a number".into()))?
                        .round(),
                ),
            },
            [v, Value::Int(d)] => {
                let f = v
                    .as_f64()
                    .ok_or_else(|| Error::Type("round() needs a number".into()))?;
                let m = 10f64.powi(*d as i32);
                Value::Float((f * m).round() / m)
            }
            _ => return Err(Error::Parse("round(x[, digits])".into())),
        },
        "now" | "current_timestamp" | "localtimestamp" => Value::Text(now_timestamp()),
        "current_date" => Value::Text(now_timestamp()[..10].to_string()),
        "current_time" => Value::Text(now_timestamp()[11..].to_string()),
        "date" => coerce_type(Ty::Date, one()?)?,
        "time" => match one()? {
            Value::Text(s) => match parse_timestamp(&s) {
                Some(t) => Value::Text(t[11..].to_string()),
                None => coerce_type(Ty::Time, Value::Text(s))?,
            },
            v => coerce_type(Ty::Time, v)?,
        },
        "timestamp" | "datetime" => coerce_type(Ty::Timestamp, one()?)?,
        "year" | "month" | "day" | "hour" | "minute" | "second" => date_part(name, &one()?)?,
        "json_type" | "typeof" => Value::Text(
            match one()? {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Int(_) => "integer",
                Value::Float(_) => "real",
                Value::Text(_) => "text",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
            }
            .into(),
        ),
        _ => return Err(Error::Parse(format!("unknown function '{name}'"))),
    })
}

pub fn known_function(name: &str) -> bool {
    matches!(
        name,
        "version"
            | "coalesce"
            | "ifnull"
            | "nullif"
            | "concat"
            | "lower"
            | "upper"
            | "length"
            | "trim"
            | "substr"
            | "substring"
            | "abs"
            | "round"
            | "now"
            | "current_timestamp"
            | "localtimestamp"
            | "current_date"
            | "current_time"
            | "date"
            | "time"
            | "timestamp"
            | "datetime"
            | "year"
            | "month"
            | "day"
            | "hour"
            | "minute"
            | "second"
            | "json_type"
            | "typeof"
    )
}

fn sub_id(s: &SubRef) -> Result<usize> {
    match s {
        SubRef::Bound(i) => Ok(*i),
        SubRef::Parsed(_) => Err(Error::Parse("internal error: unbound subquery".into())),
    }
}

pub fn eval(e: &Expr, env: &Env) -> Result<Value> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Slot {
            level, idx, path, ..
        } => {
            let row = if *level == 0 {
                env.row
            } else {
                env.outer.get(level - 1).map_or(&[][..], |r| r.as_slice())
            };
            match row.get(*idx) {
                Some(v) => walk_path(v, path),
                None => Value::Null,
            }
        }
        Expr::Col(p) => {
            return Err(Error::Schema(format!(
                "unresolved column '{}'",
                p.join(".")
            )))
        }
        Expr::Untyped(t) => infer_untyped(t),
        Expr::Default => {
            return Err(Error::Parse(
                "DEFAULT is only allowed in INSERT ... VALUES".into(),
            ))
        }
        Expr::Not(a) => from_tri(tri(&eval(a, env)?)?.map(|b| !b)),
        Expr::Neg(a) => match eval(a, env)? {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(
                i.checked_neg()
                    .ok_or_else(|| Error::Type("integer overflow".into()))?,
            ),
            Value::Float(f) => Value::Float(-f),
            other => return Err(Error::Type(format!("cannot negate {}", other.to_json()))),
        },
        Expr::IsNull(a, neg) => Value::Bool(eval(a, env)?.is_null() != *neg),
        Expr::Like(a, p, neg) => match (eval(a, env)?, eval(p, env)?) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::Text(s), Value::Text(p)) => {
                let (s, p): (Vec<char>, Vec<char>) = (s.chars().collect(), p.chars().collect());
                Value::Bool(like(&s, &p) != *neg)
            }
            _ => return Err(Error::Type("LIKE needs text on both sides".into())),
        },
        Expr::In(a, list, neg) => {
            let v = eval(a, env)?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                items.push(eval(item, env)?);
            }
            in_list(&v, items.iter(), *neg)
        }
        Expr::InQuery(a, sub, neg) => {
            let v = eval(a, env)?;
            let rows = run(env, sub_id(sub)?)?;
            in_list(
                &v,
                rows.iter().map(|r| r.first().unwrap_or(&Value::Null)),
                *neg,
            )
        }
        Expr::Exists(sub, neg) => {
            Value::Bool(run_limited(env, sub_id(sub)?, Some(1))?.is_empty() == *neg)
        }
        Expr::ScalarQuery(sub) => {
            let rows = run_limited(env, sub_id(sub)?, Some(2))?;
            match rows.len() {
                0 => Value::Null,
                1 => rows[0].first().cloned().unwrap_or(Value::Null),
                _ => {
                    return Err(Error::Type(
                        "scalar subquery returned more than one row".into(),
                    ))
                }
            }
        }
        Expr::Bin(a, Op::And, b) => {
            let l = tri(&eval(a, env)?)?;
            if l == Some(false) {
                return Ok(Value::Bool(false));
            }
            let r = tri(&eval(b, env)?)?;
            from_tri(match (l, r) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Bin(a, Op::Or, b) => {
            let l = tri(&eval(a, env)?)?;
            if l == Some(true) {
                return Ok(Value::Bool(true));
            }
            let r = tri(&eval(b, env)?)?;
            from_tri(match (l, r) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        Expr::Bin(a, op, b) => {
            let (l, r) = (eval(a, env)?, eval(b, env)?);
            match op {
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => arith(*op, &l, &r)?,
                Op::Concat => {
                    if l.is_null() || r.is_null() {
                        Value::Null
                    } else {
                        Value::Text(l.display() + &r.display())
                    }
                }
                _ => compare(*op, &l, &r),
            }
        }
        Expr::Func(name, args) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, env)?);
            }
            func(name, vals)?
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            let base = match operand {
                Some(o) => Some(eval(o, env)?),
                None => None,
            };
            for (w, then) in whens {
                let hit = match &base {
                    Some(b) => compare(Op::Eq, b, &eval(w, env)?) == Value::Bool(true),
                    None => is_true(&eval(w, env)?)?,
                };
                if hit {
                    return eval(then, env);
                }
            }
            match otherwise {
                Some(o) => eval(o, env)?,
                None => Value::Null,
            }
        }
        Expr::Cast(a, ty) => cast(eval(a, env)?, *ty)?,
        Expr::Agg(..) => {
            return Err(Error::Parse(
                "aggregate functions are not allowed here".into(),
            ))
        }
    })
}

fn run(env: &Env, id: usize) -> Result<Vec<Vec<Value>>> {
    run_limited(env, id, None)
}

fn run_limited(env: &Env, id: usize, limit: Option<usize>) -> Result<Vec<Vec<Value>>> {
    let mut stack: Vec<Vec<Value>> = Vec::with_capacity(env.outer.len() + 1);
    stack.push(env.row.to_vec());
    stack.extend_from_slice(env.outer);
    env.subs.run_sub(id, &stack, limit)
}

fn in_list<'v>(v: &Value, items: impl Iterator<Item = &'v Value>, neg: bool) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let mut saw_null = false;
    for x in items {
        if x.is_null() {
            saw_null = true;
        } else if x.total_cmp(v) == Ordering::Equal {
            return Value::Bool(!neg);
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(neg)
    }
}

/// An untyped parameter with no context: an integer or a decimal when it is
/// written as one, otherwise text.
pub fn infer_untyped(t: &str) -> Value {
    let canonical_int = |s: &str| {
        let d = s.strip_prefix('-').unwrap_or(s);
        !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) && (d == "0" || !d.starts_with('0'))
    };
    if canonical_int(t) {
        if let Ok(i) = t.parse::<i64>() {
            return Value::Int(i);
        }
    }
    let looks_decimal = t.contains('.')
        && t.bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-');
    if looks_decimal {
        if let Ok(f) = t.parse::<f64>() {
            return Value::Float(f);
        }
    }
    Value::Text(t.to_string())
}

/// An untyped parameter used where a column of type `ty` is expected.
/// Text that does not convert stays text (and then fails coercion or compares unequal).
pub fn untyped_as(ty: Ty, t: &str) -> Value {
    let text = Value::Text(t.to_string());
    match ty {
        Ty::Int => t.trim().parse::<i64>().map(Value::Int).unwrap_or(text),
        Ty::Float => t.trim().parse::<f64>().map(Value::Float).unwrap_or(text),
        Ty::Bool => match t.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "yes" | "on" => Value::Bool(true),
            "f" | "false" | "0" | "no" | "off" => Value::Bool(false),
            _ => text,
        },
        Ty::Json => Value::parse(t).unwrap_or(text),
        _ => coerce_type(ty, text.clone()).unwrap_or(text),
    }
}

/// Evaluate an expression that may not reference any row (defaults, LIMIT, literals).
pub fn eval_const(e: &Expr) -> Result<Value> {
    eval(e, &Env::new(&[], &[], &NoSubs))
}
