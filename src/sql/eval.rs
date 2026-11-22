//! Expression evaluation: SQL three-valued logic, arithmetic, functions,
//! aggregates over a group, and subqueries (correlated or not).

use super::ast::*;
use super::catalog::{group_key, Scope};
use super::plan::{NodeId, Profile, Running, Work};
use super::query::run_query;
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::cell::{Cell, OnceCell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

/// Per-statement query state: the data view, the subquery cache and, for
/// EXPLAIN / EXPLAIN ANALYZE, the operator tree being recorded.
pub(super) struct QCtx<'a> {
    pub ctx: &'a Ctx<'a>,
    cache: RefCell<HashMap<usize, Rc<SubResult>>>,
    /// Lowest query level whose columns were read since this was last reset.
    /// A subquery that reads nothing from an enclosing level is uncorrelated,
    /// and its result is cached for the rest of the statement.
    min_level: Cell<usize>,
    /// Present while the statement is being explained.
    pub prof: Option<RefCell<Profile>>,
}

impl<'a> QCtx<'a> {
    pub fn new(ctx: &'a Ctx<'a>) -> QCtx<'a> {
        QCtx { ctx, cache: RefCell::new(HashMap::new()), min_level: Cell::new(usize::MAX), prof: None }
    }

    /// A context that records the plan; `analyze` also times every operator.
    pub fn explaining(ctx: &'a Ctx<'a>, analyze: bool) -> QCtx<'a> {
        let mut q = QCtx::new(ctx);
        q.prof = Some(RefCell::new(Profile { analyze, ..Profile::default() }));
        q
    }

    /// Is the plan being recorded (EXPLAIN)?
    pub fn recording(&self) -> bool {
        self.prof.is_some()
    }

    /// Register an operator node without running it (see `begin`).
    pub fn register(&self, label: impl FnOnce() -> String, est: f64, inputs: &[Option<NodeId>]) -> Option<NodeId> {
        let p = self.prof.as_ref()?;
        let label = label();
        let inputs = inputs.iter().flatten().copied().collect();
        Some(p.borrow_mut().node(label.clone(), label, Some(est), inputs))
    }

    /// Start an operator: registers its node and makes it the parent of any
    /// subquery that runs until `done`.
    pub fn begin(&self, label: impl FnOnce() -> String, est: f64, inputs: &[Option<NodeId>]) -> Running {
        let id = self.register(label, est, inputs);
        self.push(id)
    }

    pub fn push_node(&self, id: Option<NodeId>) -> Running {
        self.push(id)
    }

    fn push(&self, id: Option<NodeId>) -> Running {
        let Some(p) = &self.prof else {
            return Running { id: None, start: None };
        };
        let mut p = p.borrow_mut();
        if let Some(id) = id {
            p.started(id);
            p.stack.push(id);
        }
        Running { id, start: p.analyze.then(Instant::now) }
    }

    pub fn done(&self, r: Running, rows: usize, work: Work) -> Option<NodeId> {
        let id = r.id?;
        let p = self.prof.as_ref()?;
        let mut p = p.borrow_mut();
        if let Some(at) = p.stack.iter().rposition(|&x| x == id) {
            p.stack.truncate(at);
        }
        p.finish(id, rows, work, r.start.map(|s| s.elapsed()));
        Some(id)
    }

    /// Record one execution of a registered node that has no `begin` (index probes).
    pub fn record(&self, id: Option<NodeId>, rows: usize, work: Work, start: Option<Instant>) {
        if let (Some(id), Some(p)) = (id, &self.prof) {
            p.borrow_mut().finish(id, rows, work, start.map(|s| s.elapsed()));
        }
    }

    /// A timer, only when EXPLAIN ANALYZE is measuring.
    pub fn timer(&self) -> Option<Instant> {
        match &self.prof {
            Some(p) if p.borrow().analyze => Some(Instant::now()),
            _ => None,
        }
    }

    pub fn detail(&self, id: Option<NodeId>, line: impl FnOnce() -> String) {
        if let (Some(id), Some(p)) = (id, &self.prof) {
            p.borrow_mut().detail(id, line());
        }
    }

    pub fn set_label(&self, id: Option<NodeId>, label: String) {
        if let (Some(id), Some(p)) = (id, &self.prof) {
            p.borrow_mut().nodes[id].label = label;
        }
    }

    /// Remember that a table was planned, and whether it had statistics.
    pub fn table_stats(&self, table: &str, analyzed_rows: Option<f64>) {
        if let Some(p) = &self.prof {
            p.borrow_mut().tables.insert(table.to_string(), analyzed_rows);
        }
    }
}

pub(super) struct SubResult {
    pub cols: usize,
    pub rows: Vec<Vec<Value>>,
    /// For IN: encoded keys of the non-NULL values, and whether a NULL was seen.
    set: OnceCell<(HashSet<Vec<u8>>, bool)>,
}

#[derive(Clone, Copy)]
pub(super) struct Env<'a> {
    pub scope: &'a [(String, String)],
    pub row: &'a [Value],
    /// Rows of the current group when evaluating aggregates.
    pub group: Option<&'a [Vec<Value>]>,
    pub q: &'a QCtx<'a>,
    /// The row of the enclosing query, for correlated subqueries.
    pub outer: Option<&'a Env<'a>>,
    /// 0 for the statement, +1 per subquery.
    pub level: usize,
}

impl<'a> Env<'a> {
    pub fn new(q: &'a QCtx<'a>, scope: &'a [(String, String)], row: &'a [Value], outer: Option<&'a Env<'a>>, level: usize) -> Env<'a> {
        Env { scope, row, group: None, q, outer, level }
    }
}

pub(super) fn find_col(scope: &[(String, String)], q: &Option<String>, n: &str) -> Result<Option<usize>> {
    let mut found = None;
    for (i, (a, c)) in scope.iter().enumerate() {
        if c == n && q.as_ref().map_or(true, |q| q == a) {
            if found.is_some() {
                return Err(Error::Schema(format!("column '{n}' is ambiguous; qualify it with a table name")));
            }
            found = Some(i);
        }
    }
    Ok(found)
}

fn no_column(q: &Option<String>, n: &str) -> Error {
    match q {
        Some(q) => Error::Schema(format!("no such column '{q}.{n}'")),
        None => Error::Schema(format!("no such column '{n}'")),
    }
}

/// Look a column up in this row, then in the rows of enclosing queries.
fn col_value(env: &Env, q: &Option<String>, n: &str) -> Result<Value> {
    let mut cur = Some(env);
    while let Some(e) = cur {
        if let Some(i) = find_col(e.scope, q, n)? {
            if e.level < env.level {
                env.q.min_level.set(env.q.min_level.get().min(e.level));
            }
            return Ok(e.row[i].clone());
        }
        cur = e.outer;
    }
    Err(no_column(q, n))
}

/// SQL three-valued logic: NULL is "unknown".
pub(super) fn tri(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::Type(format!("expected a boolean, got {}", other.to_json()))),
    }
}

fn from_tri(t: Option<bool>) -> Value {
    t.map_or(Value::Null, Value::Bool)
}

pub(super) fn is_true(v: &Value) -> Result<bool> {
    Ok(tri(v)? == Some(true))
}

/// LIKE matching with `%` (any run) and `_` (one character). Iterative,
/// with single-point backtracking, so long strings cannot exhaust the stack
/// and the cost is O(len(s) * len(p)) in the worst case.
pub(super) fn like(s: &[char], p: &[char]) -> bool {
    let (mut si, mut pi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '_' || (p[pi] != '%' && p[pi] == s[si])) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

pub(super) fn arith(op: Op, l: &Value, r: &Value) -> Result<Value> {
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
        return res.map(Value::Int).ok_or_else(|| Error::Type("integer overflow".into()));
    }
    match (l.as_f64(), r.as_f64()) {
        (Some(a), Some(b)) => {
            if matches!(op, Op::Div | Op::Mod) && b == 0.0 {
                return Err(Error::Type("division by zero".into()));
            }
            let f = match op {
                Op::Add => a + b,
                Op::Sub => a - b,
                Op::Mul => a * b,
                Op::Div => a / b,
                _ => a % b,
            };
            if !f.is_finite() {
                return Err(Error::Type("numeric overflow".into()));
            }
            Ok(Value::Float(f))
        }
        _ => Err(Error::Type(format!("cannot apply '{}' to {} and {}", op.text(), l.to_json(), r.to_json()))),
    }
}

/// Explicit conversion between the four column types.
pub(super) fn cast(v: Value, ty: Ty) -> Result<Value> {
    let fail = |v: &Value| Err(Error::Type(format!("cannot convert {} to {}", v.to_json(), ty.name().to_uppercase())));
    Ok(match (ty, v) {
        (_, Value::Null) => Value::Null,
        (Ty::Int, Value::Int(i)) => Value::Int(i),
        (Ty::Int, Value::Float(f)) => {
            let r = f.round();
            if !r.is_finite() || r.abs() >= 9.2e18 {
                return fail(&Value::Float(f));
            }
            Value::Int(r as i64)
        }
        (Ty::Int, Value::Bool(b)) => Value::Int(b as i64),
        (Ty::Int, Value::Text(s)) => match s.trim().parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => return fail(&Value::Text(s)),
        },
        (Ty::Float, Value::Int(i)) => Value::Float(i as f64),
        (Ty::Float, Value::Float(f)) => Value::Float(f),
        (Ty::Float, Value::Bool(b)) => Value::Float(b as i64 as f64),
        (Ty::Float, Value::Text(s)) => match s.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Value::Float(f),
            _ => return fail(&Value::Text(s)),
        },
        (Ty::Text, Value::Bool(b)) => Value::Text(if b { "true" } else { "false" }.into()),
        (Ty::Text, v) => Value::Text(v.display()),
        (Ty::Bool, Value::Bool(b)) => Value::Bool(b),
        (Ty::Bool, Value::Int(i)) => Value::Bool(i != 0),
        (Ty::Bool, Value::Text(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Value::Bool(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Value::Bool(false),
            _ => return fail(&Value::Text(s)),
        },
        (_, v) => return fail(&v),
    })
}

pub(super) fn eval(e: &Expr, env: &Env) -> Result<Value> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(q, n) => col_value(env, q, n)?,
        Expr::Not(a) => from_tri(tri(&eval(a, env)?)?.map(|b| !b)),
        Expr::Neg(a) => match eval(a, env)? {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| Error::Type("integer overflow".into()))?),
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
        Expr::In(a, list, neg) => eval_in(a, list, *neg, env)?,
        Expr::InSub(a, sq, neg) => eval_in_sub(a, sq, *neg, env)?,
        Expr::Exists(sq) => Value::Bool(!subquery(sq, env)?.rows.is_empty()),
        Expr::Subquery(sq) => eval_scalar_sub(sq, env)?,
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
            binary(*op, &l, &r)?
        }
        Expr::Func(name, args) => call(name, args, env)?,
        Expr::Cast(a, ty) => cast(eval(a, env)?, *ty)?,
        Expr::Case(op, whens, els) => eval_case(op.as_deref(), whens, els.as_deref(), env)?,
        Expr::Agg(f, arg, distinct) => aggregate(*f, arg.as_deref(), *distinct, env)?,
    })
}

fn binary(op: Op, l: &Value, r: &Value) -> Result<Value> {
    Ok(match op {
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => arith(op, l, r)?,
        Op::Concat => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Text(l.display() + &r.display())
            }
        }
        _ => {
            if l.is_null() || r.is_null() {
                return Ok(Value::Null);
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
    })
}

#[inline(never)]
fn eval_in(a: &Expr, list: &[Expr], neg: bool, env: &Env) -> Result<Value> {
    let v = eval(a, env)?;
    if v.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for item in list {
        let x = eval(item, env)?;
        if x.is_null() {
            saw_null = true;
        } else if x.total_cmp(&v) == Ordering::Equal {
            return Ok(Value::Bool(!neg));
        }
    }
    Ok(if saw_null { Value::Null } else { Value::Bool(neg) })
}

#[inline(never)]
fn eval_case(op: Option<&Expr>, whens: &[(Expr, Expr)], els: Option<&Expr>, env: &Env) -> Result<Value> {
    let base = op.map(|o| eval(o, env)).transpose()?;
    for (w, t) in whens {
        let hit = match &base {
            Some(b) => {
                let wv = eval(w, env)?;
                !b.is_null() && !wv.is_null() && b.total_cmp(&wv) == Ordering::Equal
            }
            None => is_true(&eval(w, env)?)?,
        };
        if hit {
            return eval(t, env);
        }
    }
    els.map_or(Ok(Value::Null), |e| eval(e, env))
}

/// Run (or fetch from the cache) a subquery in the context of `env`.
#[inline(never)]
fn subquery(sq: &SubQ, env: &Env) -> Result<Rc<SubResult>> {
    if let Some(r) = env.q.cache.borrow().get(&sq.id) {
        return Ok(r.clone());
    }
    // The subquery's operators hang under this node, which hangs under the
    // operator evaluating the expression.
    let node = env.q.prof.as_ref().map(|p| {
        p.borrow_mut().repeated_node(format!("subplan {}", sq.id), "SubPlan".into())
    });
    let run = env.q.push_node(node);
    let saved = env.q.min_level.replace(usize::MAX);
    let res = run_query(env.q, &sq.q, Some(env), env.level + 1);
    let reached = env.q.min_level.get();
    env.q.min_level.set(saved.min(reached));
    let correlated = reached <= env.level;
    env.q.set_label(
        node,
        if correlated { "SubPlan (correlated, evaluated per row)" } else { "SubPlan (uncorrelated, evaluated once)" }.into(),
    );
    env.q.done(run, res.as_ref().map_or(0, |r| r.1.len()), Work::default());
    let (cols, rows) = res?;
    let r = Rc::new(SubResult { cols: cols.len(), rows, set: OnceCell::new() });
    if !correlated {
        env.q.cache.borrow_mut().insert(sq.id, r.clone());
    }
    Ok(r)
}

fn one_column(r: &SubResult) -> Result<()> {
    if r.cols != 1 {
        return Err(Error::Schema(format!("subquery must return exactly one column, got {}", r.cols)));
    }
    Ok(())
}

#[inline(never)]
fn eval_scalar_sub(sq: &SubQ, env: &Env) -> Result<Value> {
    let r = subquery(sq, env)?;
    one_column(&r)?;
    match r.rows.len() {
        0 => Ok(Value::Null),
        1 => Ok(r.rows[0][0].clone()),
        n => Err(Error::Type(format!("scalar subquery returned {n} rows; it must return at most one"))),
    }
}

#[inline(never)]
fn eval_in_sub(a: &Expr, sq: &SubQ, neg: bool, env: &Env) -> Result<Value> {
    let v = eval(a, env)?;
    if v.is_null() {
        return Ok(Value::Null);
    }
    let r = subquery(sq, env)?;
    one_column(&r)?;
    let (set, saw_null) = r.set.get_or_init(|| {
        let mut set = HashSet::new();
        let mut saw_null = false;
        for row in &r.rows {
            if row[0].is_null() {
                saw_null = true;
            } else {
                set.insert(group_key([&row[0]]));
            }
        }
        (set, saw_null)
    });
    Ok(if set.contains(&group_key([&v])) {
        Value::Bool(!neg)
    } else if *saw_null {
        Value::Null
    } else {
        Value::Bool(neg)
    })
}

#[inline(never)]
fn aggregate(f: AggFn, arg: Option<&Expr>, distinct: bool, env: &Env) -> Result<Value> {
    let Some(group) = env.group else {
        return Err(Error::Parse("aggregate functions are not allowed here".into()));
    };
    let Some(arg) = arg else {
        return Ok(Value::Int(group.len() as i64)); // COUNT(*)
    };
    if has_agg(arg) {
        return Err(Error::Parse("aggregate functions cannot be nested".into()));
    }
    let mut vals = Vec::new();
    let mut seen = HashSet::new();
    for r in group {
        let v = eval(arg, &Env { row: r, group: None, ..*env })?;
        if !v.is_null() && (!distinct || seen.insert(group_key([&v]))) {
            vals.push(v);
        }
    }
    Ok(match f {
        AggFn::Count => Value::Int(vals.len() as i64),
        AggFn::Min => vals.into_iter().min_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
        AggFn::Max => vals.into_iter().max_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
        AggFn::Sum => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = Value::Int(0);
            for v in &vals {
                if v.as_f64().is_none() {
                    return Err(Error::Type(format!("SUM needs numbers, got {}", v.to_json())));
                }
                total = arith(Op::Add, &total, v)?;
            }
            total
        }
        AggFn::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = 0.0;
            for v in &vals {
                total += v.as_f64().ok_or_else(|| Error::Type(format!("AVG needs numbers, got {}", v.to_json())))?;
            }
            Value::Float(total / vals.len() as f64)
        }
    })
}

#[inline(never)]
fn call(name: &str, args: &[Expr], env: &Env) -> Result<Value> {
    let vals = args.iter().map(|a| eval(a, env)).collect::<Result<Vec<_>>>()?;
    let arity = |lo: usize, hi: usize| -> Result<()> {
        if vals.len() < lo || vals.len() > hi {
            let want = if lo == hi { format!("{lo}") } else { format!("{lo} to {hi}") };
            return Err(Error::Parse(format!("{name}() takes {want} argument(s), got {}", vals.len())));
        }
        Ok(())
    };
    let text = |v: &Value| -> Result<Option<String>> {
        match v {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s.clone())),
            other => Err(Error::Type(format!("{name}() needs text, got {}", other.to_json()))),
        }
    };
    let int = |v: &Value| -> Result<Option<i64>> {
        match v {
            Value::Null => Ok(None),
            Value::Int(i) => Ok(Some(*i)),
            other => Err(Error::Type(format!("{name}() needs an integer, got {}", other.to_json()))),
        }
    };
    Ok(match name {
        "version" => {
            arity(0, 0)?;
            Value::Text(format!("polydb {}", env!("CARGO_PKG_VERSION")))
        }
        "coalesce" => vals.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        "ifnull" => {
            arity(2, 2)?;
            if vals[0].is_null() { vals[1].clone() } else { vals[0].clone() }
        }
        "nullif" => {
            arity(2, 2)?;
            if !vals[0].is_null() && !vals[1].is_null() && vals[0].total_cmp(&vals[1]) == Ordering::Equal {
                Value::Null
            } else {
                vals[0].clone()
            }
        }
        "greatest" | "least" => {
            if vals.is_empty() {
                return Err(Error::Parse(format!("{name}() needs at least one argument")));
            }
            let it = vals.iter().filter(|v| !v.is_null());
            let pick = if name == "greatest" { it.max_by(|a, b| a.total_cmp(b)) } else { it.min_by(|a, b| a.total_cmp(b)) };
            pick.cloned().unwrap_or(Value::Null)
        }
        "lower" | "upper" | "length" | "trim" | "ltrim" | "rtrim" => {
            arity(1, 1)?;
            match text(&vals[0])? {
                None => Value::Null,
                Some(s) => match name {
                    "lower" => Value::Text(s.to_lowercase()),
                    "upper" => Value::Text(s.to_uppercase()),
                    "trim" => Value::Text(s.trim().to_string()),
                    "ltrim" => Value::Text(s.trim_start().to_string()),
                    "rtrim" => Value::Text(s.trim_end().to_string()),
                    _ => Value::Int(s.chars().count() as i64),
                },
            }
        }
        "substr" | "substring" => {
            arity(2, 3)?;
            let (Some(s), Some(start)) = (text(&vals[0])?, int(&vals[1])?) else {
                return Ok(Value::Null);
            };
            let len = match vals.get(2) {
                Some(v) => match int(v)? {
                    None => return Ok(Value::Null),
                    Some(n) if n < 0 => return Err(Error::Type("substr() length cannot be negative".into())),
                    Some(n) => Some(n),
                },
                None => None,
            };
            // 1-based; positions before the start of the string eat into the length
            let chars: Vec<char> = s.chars().collect();
            let from = start.max(1);
            let to = match len {
                Some(n) => start.saturating_add(n),
                None => i64::MAX,
            };
            let (a, b) = ((from - 1) as usize, (to.max(from) - 1).min(chars.len() as i64) as usize);
            Value::Text(if a >= chars.len() || a >= b { String::new() } else { chars[a..b].iter().collect() })
        }
        "replace" => {
            arity(3, 3)?;
            match (text(&vals[0])?, text(&vals[1])?, text(&vals[2])?) {
                (Some(s), Some(from), Some(to)) => {
                    Value::Text(if from.is_empty() { s } else { s.replace(&from, &to) })
                }
                _ => Value::Null,
            }
        }
        "concat" => Value::Text(vals.iter().filter(|v| !v.is_null()).map(|v| v.display()).collect()),
        "abs" => {
            arity(1, 1)?;
            match &vals[0] {
                Value::Null => Value::Null,
                Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| Error::Type("integer overflow".into()))?),
                Value::Float(f) => Value::Float(f.abs()),
                other => return Err(Error::Type(format!("abs() needs a number, got {}", other.to_json()))),
            }
        }
        "round" | "floor" | "ceil" | "ceiling" => {
            if name == "round" { arity(1, 2)? } else { arity(1, 1)? };
            let digits = match vals.get(1) {
                Some(v) => match int(v)? {
                    None => return Ok(Value::Null),
                    Some(d) => d.clamp(-18, 18) as i32,
                },
                None => 0,
            };
            match &vals[0] {
                Value::Null => Value::Null,
                Value::Int(i) if digits >= 0 => Value::Int(*i),
                v @ (Value::Int(_) | Value::Float(_)) => {
                    let f = v.as_f64().unwrap_or(0.0);
                    let m = 10f64.powi(digits);
                    let r = match name {
                        "round" => (f * m).round() / m,
                        "floor" => f.floor(),
                        _ => f.ceil(),
                    };
                    match v {
                        Value::Int(_) => cast(Value::Float(r), Ty::Int)?,
                        _ => Value::Float(r),
                    }
                }
                other => return Err(Error::Type(format!("{name}() needs a number, got {}", other.to_json()))),
            }
        }
        "typeof" => {
            arity(1, 1)?;
            Value::Text(
                match &vals[0] {
                    Value::Null => "null",
                    Value::Bool(_) => "bool",
                    Value::Int(_) => "int",
                    Value::Float(_) => "real",
                    Value::Text(_) => "text",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                }
                .into(),
            )
        }
        _ => return Err(Error::Parse(format!("unknown function '{name}'"))),
    })
}

pub(super) fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg(..) => true,
        Expr::Lit(_) | Expr::Col(..) | Expr::Exists(_) | Expr::Subquery(_) => false,
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::InSub(a, _, _) => has_agg(a),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => has_agg(a) || has_agg(b),
        Expr::In(a, list, _) => has_agg(a) || list.iter().any(has_agg),
        Expr::Func(_, args) => args.iter().any(has_agg),
        Expr::Case(op, whens, els) => {
            op.as_deref().map_or(false, has_agg)
                || whens.iter().any(|(w, t)| has_agg(w) || has_agg(t))
                || els.as_deref().map_or(false, has_agg)
        }
    }
}

/// Does the expression read columns or run subqueries (so cannot be folded to a constant)?
pub(super) fn is_const(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) => true,
        Expr::Col(..) | Expr::Agg(..) | Expr::Exists(_) | Expr::Subquery(_) | Expr::InSub(..) => false,
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => is_const(a),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => is_const(a) && is_const(b),
        Expr::In(a, list, _) => is_const(a) && list.iter().all(is_const),
        Expr::Func(_, args) => args.iter().all(is_const),
        Expr::Case(op, whens, els) => {
            op.as_deref().map_or(true, is_const)
                && whens.iter().all(|(w, t)| is_const(w) && is_const(t))
                && els.as_deref().map_or(true, is_const)
        }
    }
}

/// Does the expression contain a subquery anywhere?
pub(super) fn has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Exists(_) | Expr::Subquery(_) | Expr::InSub(..) => true,
        Expr::Lit(_) | Expr::Col(..) => false,
        Expr::Agg(_, a, _) => a.as_deref().map_or(false, has_subquery),
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => has_subquery(a),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => has_subquery(a) || has_subquery(b),
        Expr::In(a, list, _) => has_subquery(a) || list.iter().any(has_subquery),
        Expr::Func(_, args) => args.iter().any(has_subquery),
        Expr::Case(op, whens, els) => {
            op.as_deref().map_or(false, has_subquery)
                || whens.iter().any(|(w, t)| has_subquery(w) || has_subquery(t))
                || els.as_deref().map_or(false, has_subquery)
        }
    }
}

/// Every column an expression names (for CHECK validation and renames).
pub(super) fn columns_of<'e>(e: &'e Expr, out: &mut Vec<(&'e Option<String>, &'e str)>) {
    match e {
        Expr::Col(q, n) => out.push((q, n)),
        Expr::Lit(_) | Expr::Exists(_) | Expr::Subquery(_) => {}
        Expr::Agg(_, a, _) => {
            if let Some(a) = a {
                columns_of(a, out)
            }
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::InSub(a, _, _) => columns_of(a, out),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => {
            columns_of(a, out);
            columns_of(b, out);
        }
        Expr::In(a, list, _) => {
            columns_of(a, out);
            list.iter().for_each(|x| columns_of(x, out));
        }
        Expr::Func(_, args) => args.iter().for_each(|x| columns_of(x, out)),
        Expr::Case(op, whens, els) => {
            if let Some(o) = op {
                columns_of(o, out);
            }
            for (w, t) in whens {
                columns_of(w, out);
                columns_of(t, out);
            }
            if let Some(x) = els {
                columns_of(x, out);
            }
        }
    }
}

/// Rewrite column references in place: drop qualifiers and apply a rename.
pub(super) fn rewrite_columns(e: &mut Expr, f: &impl Fn(&str) -> String) {
    match e {
        Expr::Col(q, n) => {
            *q = None;
            *n = f(n);
        }
        Expr::Lit(_) | Expr::Exists(_) | Expr::Subquery(_) => {}
        Expr::Agg(_, a, _) => {
            if let Some(a) = a {
                rewrite_columns(a, f)
            }
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::InSub(a, _, _) => {
            rewrite_columns(a, f)
        }
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => {
            rewrite_columns(a, f);
            rewrite_columns(b, f);
        }
        Expr::In(a, list, _) => {
            rewrite_columns(a, f);
            list.iter_mut().for_each(|x| rewrite_columns(x, f));
        }
        Expr::Func(_, args) => args.iter_mut().for_each(|x| rewrite_columns(x, f)),
        Expr::Case(op, whens, els) => {
            if let Some(o) = op {
                rewrite_columns(o, f);
            }
            for (w, t) in whens {
                rewrite_columns(w, f);
                rewrite_columns(t, f);
            }
            if let Some(x) = els {
                rewrite_columns(x, f);
            }
        }
    }
}

/// Evaluate an expression that reads no columns.
pub(super) fn const_eval(e: &Expr, q: &QCtx) -> Result<Value> {
    let scope = Scope::new();
    eval(e, &Env::new(q, &scope, &[], None, 0))
}

/// Hash-join / IN key: None when any part is NULL (NULL never equals anything).
pub(super) fn join_key<'v>(vals: impl IntoIterator<Item = &'v Value>) -> Option<Vec<u8>> {
    let mut k = Vec::new();
    for v in vals {
        k.extend(keys::encode_index_value(v)?);
    }
    Some(k)
}

/// Column header for an unaliased select expression.
pub(super) fn label(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => v.display(),
        Expr::Col(_, n) => n.clone(),
        Expr::Bin(a, op, b) => format!("{} {} {}", label(a), op.text(), label(b)),
        Expr::Neg(a) => format!("-{}", label(a)),
        Expr::Not(a) => format!("not {}", label(a)),
        Expr::Agg(f, arg, distinct) => format!(
            "{}({}{})",
            f.name(),
            if *distinct { "distinct " } else { "" },
            arg.as_ref().map_or("*".to_string(), |a| label(a))
        ),
        Expr::Func(n, args) => format!("{n}({})", args.iter().map(label).collect::<Vec<_>>().join(", ")),
        Expr::Cast(a, _) => label(a),
        Expr::Case(..) => "case".to_string(),
        Expr::Exists(_) => "exists".to_string(),
        _ => "expr".to_string(),
    }
}
