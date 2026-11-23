//! Access-path selection for one table: which key range of the ordered
//! store to read for a set of predicates, and what that is expected to cost.
//!
//! Candidate paths, each costed from the table's statistics (`stats.rs`):
//!
//! | path                    | reads                                              |
//! |-------------------------|----------------------------------------------------|
//! | Primary Key Lookup      | one row (all primary-key columns equal constants)  |
//! | Primary Key IN Lookup   | one row per listed key                             |
//! | Primary Key Range Scan  | a contiguous run of rows (one-column key)          |
//! | Primary Key Prefix Scan | a run of rows sharing leading composite-key values |
//! | Index Scan              | index entries for a leftmost-prefix of equalities  |
//! |                         | plus an optional range on the next column, then    |
//! |                         | the rows they point at                             |
//! | Index IN Scan           | one index prefix per value of an IN list           |
//! | Seq Scan                | every row of the table                             |
//!
//! The cheapest is chosen; a primary-key point lookup is taken at once.
//! Paths only narrow the candidates: callers still check every row against
//! the full condition. Predicates may compare a column with a constant or
//! with a column of an enclosing query (inside a correlated subquery, whose
//! outer row is fixed for each execution).

use super::ast::*;
use super::catalog::*;
use super::eval::*;
use super::parser::to_sql;
use super::plan::Work;
use super::stats::Estimator;
use crate::error::{Error, Result};
use crate::keys;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub(super) type Rows = Vec<(Vec<u8>, Vec<Value>)>;

// Relative costs, in units of "read and decode one row sequentially".
// Calibrated with `examples/planner_bench.rs` (100k rows, release build):
// a sequential row costs ~0.7 us, a row fetched through an index ~1.7-2.8 us
// including its index entry; an index wins up to roughly half the table.
pub(super) const SEQ_ROW: f64 = 1.0;
pub(super) const IDX_ENTRY: f64 = 0.1;
pub(super) const FETCH_ROW: f64 = 1.7;
pub(super) const SEEK: f64 = 0.5;
pub(super) const HASH_BUILD: f64 = 0.1;
pub(super) const HASH_PROBE: f64 = 0.05;

// ================================================================ predicates

/// One bound of a range predicate on a column.
#[derive(Clone)]
pub(super) struct Bnd {
    pub v: Value,
    pub inclusive: bool,
    pub src: String,
}

/// Predicates on one table's columns that every matching row satisfies.
#[derive(Default)]
pub(super) struct Preds {
    /// column -> (value, how it was written)
    pub eq: HashMap<usize, (Value, String)>,
    pub ins: HashMap<usize, Vec<Value>>,
    pub lo: HashMap<usize, Bnd>,
    pub hi: HashMap<usize, Bnd>,
}

impl Preds {
    pub fn eq_val(&self, ci: usize) -> Option<&Value> {
        self.eq.get(&ci).map(|(v, _)| v)
    }
}

/// Where the constant side of a predicate may come from.
#[derive(Clone, Copy)]
pub(super) struct Consts<'a> {
    /// Every column of the FROM clause of the query being planned.
    pub local: &'a [(String, String)],
    /// The enclosing query's row, inside a subquery.
    pub outer: Option<&'a Env<'a>>,
    pub level: usize,
}

impl<'a> Consts<'a> {
    pub fn none(local: &'a [(String, String)]) -> Consts<'a> {
        Consts {
            local,
            outer: None,
            level: 0,
        }
    }
}

/// SQL text of an expression for plan output, with identifiers unquoted
/// where that is unambiguous and without the outermost parentheses.
pub(super) fn show(e: &Expr) -> String {
    let s = to_sql(e);
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\'' => {
                out.push('\'');
                i += 1;
                while i < c.len() {
                    out.push(c[i]);
                    if c[i] == '\'' {
                        if c.get(i + 1) == Some(&'\'') {
                            out.push('\'');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '"' => {
                let mut j = i + 1;
                let mut inner = String::new();
                while j < c.len() {
                    if c[j] == '"' {
                        if c.get(j + 1) == Some(&'"') {
                            inner.push('"');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    inner.push(c[j]);
                    j += 1;
                }
                let simple = !inner.is_empty()
                    && inner.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
                    && !inner.starts_with(|ch: char| ch.is_ascii_digit());
                if simple {
                    out.push_str(&inner);
                } else {
                    out.push_str(&c[i..=j.min(c.len() - 1)].iter().collect::<String>());
                }
                i = j + 1;
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    strip_parens(out)
}

fn strip_parens(s: String) -> String {
    if !(s.starts_with('(') && s.ends_with(')')) {
        return s;
    }
    let mut depth = 0i32;
    let mut quote = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => {
                depth -= 1;
                if depth == 0 && i != s.len() - 1 {
                    return s;
                }
            }
            _ => {}
        }
    }
    s[1..s.len() - 1].to_string()
}

fn flip(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        o => o,
    }
}

pub(super) fn table_col(s: &Schema, alias: &str, e: &Expr) -> Option<usize> {
    match e {
        Expr::Col(q, n) if q.as_ref().map_or(true, |q| q == alias) => s.cols.iter().position(|c| &c.name == n),
        _ => None,
    }
}

/// The value of an expression that is fixed while this query runs: a
/// constant, or one that reads only columns of enclosing queries.
pub(super) fn konst(e: &Expr, q: &QCtx, cc: Consts) -> Option<(Value, String)> {
    if has_subquery(e) || has_agg(e) {
        return None;
    }
    let v = if is_const(e) {
        const_eval(e, q).ok()?
    } else {
        let outer = cc.outer?;
        let mut cols = Vec::new();
        columns_of(e, &mut cols);
        if cols.iter().any(|(qq, n)| !matches!(find_col(cc.local, qq, n), Ok(None))) {
            return None;
        }
        // evaluated at this query's level, so the read of an outer column
        // marks the subquery as correlated (and keeps it out of the cache)
        let empty = Scope::new();
        eval(e, &Env::new(q, &empty, &[], Some(outer), cc.level)).ok()?
    };
    // the text is only for plan output
    (!v.is_null()).then(|| (v, if q.recording() { show(e) } else { String::new() }))
}

/// Split a condition into its AND-ed parts.
pub(super) fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        _ => out.push(e),
    }
}

/// A column-vs-fixed-value comparison: (column, operator as seen from the column, value).
fn comparison(e: &Expr, s: &Schema, alias: &str, q: &QCtx, cc: Consts) -> Option<(usize, Op, Value, String)> {
    let Expr::Bin(a, op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), b) = e else {
        return None;
    };
    match (table_col(s, alias, a), table_col(s, alias, b)) {
        (Some(ci), None) => konst(b, q, cc).map(|(v, src)| (ci, *op, v, src)),
        (None, Some(ci)) => konst(a, q, cc).map(|(v, src)| (ci, flip(*op), v, src)),
        _ => None,
    }
}

pub(super) fn collect_preds(e: &Expr, s: &Schema, alias: &str, q: &QCtx, cc: Consts, p: &mut Preds) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            collect_preds(a, s, alias, q, cc, p);
            collect_preds(b, s, alias, q, cc, p);
        }
        Expr::Bin(..) => {
            let Some((ci, op, v, src)) = comparison(e, s, alias, q, cc) else {
                return;
            };
            match op {
                Op::Eq => {
                    p.eq.entry(ci).or_insert((v, src));
                }
                Op::Gt | Op::Ge => {
                    let inclusive = op == Op::Ge;
                    let tighter = p.lo.get(&ci).map_or(true, |old| match v.total_cmp(&old.v) {
                        Ordering::Greater => true,
                        Ordering::Equal => !inclusive,
                        Ordering::Less => false,
                    });
                    if tighter {
                        p.lo.insert(ci, Bnd { v, inclusive, src });
                    }
                }
                Op::Lt | Op::Le => {
                    let inclusive = op == Op::Le;
                    let tighter = p.hi.get(&ci).map_or(true, |old| match v.total_cmp(&old.v) {
                        Ordering::Less => true,
                        Ordering::Equal => !inclusive,
                        Ordering::Greater => false,
                    });
                    if tighter {
                        p.hi.insert(ci, Bnd { v, inclusive, src });
                    }
                }
                _ => {}
            }
        }
        Expr::In(a, list, false) => {
            if let Some(ci) = table_col(s, alias, a) {
                let vals: Option<Vec<Value>> = list.iter().map(|x| konst(x, q, cc).map(|(v, _)| v)).collect();
                if let Some(vals) = vals {
                    p.ins.entry(ci).or_insert(vals);
                }
            }
        }
        _ => {}
    }
}

/// Estimated fraction of a table's rows satisfying one condition.
pub(super) fn selectivity(e: &Expr, s: &Schema, alias: &str, q: &QCtx, cc: Consts, est: &Estimator) -> f64 {
    use super::stats::{DEFAULT_OTHER_SEL, DEFAULT_RANGE_SEL};
    match e {
        Expr::Bin(a, Op::And, b) => selectivity(a, s, alias, q, cc, est) * selectivity(b, s, alias, q, cc, est),
        Expr::Bin(a, Op::Or, b) => {
            let (x, y) = (selectivity(a, s, alias, q, cc, est), selectivity(b, s, alias, q, cc, est));
            (x + y - x * y).min(1.0)
        }
        Expr::Not(a) => 1.0 - selectivity(a, s, alias, q, cc, est),
        Expr::Bin(..) => match comparison(e, s, alias, q, cc) {
            Some((ci, Op::Eq, v, _)) => est.eq_sel(ci, &v),
            Some((ci, Op::Ne, v, _)) => (1.0 - est.eq_sel(ci, &v)) * est.null_sel(ci, false),
            Some((ci, op, v, _)) => {
                let inc = matches!(op, Op::Le | Op::Ge);
                if matches!(op, Op::Gt | Op::Ge) {
                    est.range_sel(ci, Some((&v, inc)), None)
                } else {
                    est.range_sel(ci, None, Some((&v, inc)))
                }
            }
            // a column compared with another column of the same table, or with
            // something only known per row
            None => DEFAULT_OTHER_SEL,
        },
        Expr::In(a, list, neg) => match table_col(s, alias, a) {
            Some(ci) => {
                let vals: Option<Vec<Value>> = list.iter().map(|x| konst(x, q, cc).map(|(v, _)| v)).collect();
                let sel = vals.map_or(DEFAULT_RANGE_SEL, |v| est.in_sel(ci, &v));
                if *neg {
                    (1.0 - sel) * est.null_sel(ci, false)
                } else {
                    sel
                }
            }
            None => DEFAULT_OTHER_SEL,
        },
        Expr::IsNull(a, neg) => match table_col(s, alias, a) {
            Some(ci) => est.null_sel(ci, !*neg),
            None => DEFAULT_OTHER_SEL,
        },
        Expr::Like(..) => 0.1,
        Expr::Lit(Value::Bool(true)) => 1.0,
        Expr::Lit(Value::Bool(false)) | Expr::Lit(Value::Null) => 0.0,
        _ => DEFAULT_OTHER_SEL,
    }
}

/// Estimated fraction of rows satisfying all of `list` (AND-ed). Bounds and
/// equalities on the same column are combined first (`x >= 1 AND x <= 5` is
/// one range, not two independent conditions); the rest are assumed
/// independent of each other.
pub(super) fn conjunction_selectivity(list: &[&Expr], s: &Schema, alias: &str, q: &QCtx, cc: Consts, est: &Estimator) -> f64 {
    let mut p = Preds::default();
    let mut sel = 1.0;
    for c in list {
        let simple = match c {
            Expr::Bin(..) => matches!(comparison(c, s, alias, q, cc), Some((_, op, _, _)) if op != Op::Ne),
            Expr::In(a, l, false) => table_col(s, alias, a).is_some() && l.iter().all(|x| konst(x, q, cc).is_some()),
            _ => false,
        };
        if simple {
            collect_preds(c, s, alias, q, cc, &mut p);
        } else {
            sel *= selectivity(c, s, alias, q, cc, est);
        }
    }
    let mut cols: Vec<usize> = p.eq.keys().chain(p.ins.keys()).chain(p.lo.keys()).chain(p.hi.keys()).copied().collect();
    cols.sort_unstable();
    cols.dedup();
    for ci in cols {
        sel *= if let Some((v, _)) = p.eq.get(&ci) {
            est.eq_sel(ci, v)
        } else {
            let in_sel = p.ins.get(&ci).map_or(1.0, |v| est.in_sel(ci, v));
            let lo = p.lo.get(&ci).map(|b| (&b.v, b.inclusive));
            let hi = p.hi.get(&ci).map(|b| (&b.v, b.inclusive));
            in_sel.min(est.range_sel(ci, lo, hi))
        };
    }
    sel
}

// ============================================================== access paths

/// Can `v` be looked up in a key built from a column of type `ty`?
/// `encode_id` keys (one-column primary keys) need the exact type; index
/// encodings share one form for INT and REAL.
pub(super) fn key_compatible(ty: Ty, v: &Value, exact: bool) -> bool {
    match (ty, v) {
        (Ty::Int, Value::Int(_)) | (Ty::Text, Value::Text(_)) | (Ty::Bool, Value::Bool(_)) => true,
        (Ty::Int | Ty::Float, Value::Int(_) | Value::Float(_)) => !exact,
        _ => false,
    }
}

/// A value usable in an exact (one-column primary key) lookup on a column of
/// type `ty`, or None when no row can equal it. `2.0` finds the INT key `2`.
fn exact_key_value(ty: Ty, v: &Value) -> Option<Value> {
    match (ty, v) {
        (Ty::Int, Value::Float(f)) if f.fract() == 0.0 && f.abs() < 9.0e18 => Some(Value::Int(*f as i64)),
        _ if key_compatible(ty, v, true) => Some(v.clone()),
        _ => None,
    }
}

/// A run of encoded keys: rows (primary key) or index entries.
pub(super) struct Prefixed {
    /// Key prefix shared by every candidate entry (field prefix plus equality values).
    base: Vec<u8>,
    /// Entries are index entries pointing at rows, rather than the rows themselves.
    is_index: bool,
    /// Encoded values that follow `base` in an index entry, before the row id.
    total_components: usize,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
}

pub(super) enum Path {
    Full,
    PkPoint(Vec<u8>),
    PkIn(Vec<Vec<u8>>),
    Range(Prefixed),
    /// One index prefix per IN value.
    IndexIn(Vec<Prefixed>),
}

pub(super) struct Access {
    pub path: Path,
    /// Operator name for EXPLAIN.
    pub op: &'static str,
    pub index: Option<String>,
    /// The key or index condition, as written.
    pub cond: Option<String>,
    pub cost: f64,
}

/// Choose the cheapest path for `p`.
pub(super) fn plan_access(q: &QCtx, s: &Schema, p: &Preds, est: &Estimator) -> Result<Access> {
    // condition text is only built for EXPLAIN
    let rec = q.recording();
    let exact_pk = s.pk.len() == 1;
    let rows = est.rows();
    let col = |i: usize| &s.cols[i].name;
    // 1. primary-key point lookup: nothing can beat it
    if !s.pk.is_empty() {
        let vals: Option<Vec<Value>> =
            s.pk.iter()
                .map(|i| {
                    let v = p.eq_val(*i)?;
                    if exact_pk {
                        exact_key_value(s.cols[*i].ty, v)
                    } else {
                        key_compatible(s.cols[*i].ty, v, false).then(|| v.clone())
                    }
                })
                .collect();
        if let Some(vals) = vals {
            let mut row = vec![Value::Null; s.cols.len()];
            for (&i, v) in s.pk.iter().zip(vals) {
                row[i] = v;
            }
            return Ok(Access {
                path: Path::PkPoint(pk_key(s, &row)?),
                op: "Primary Key Lookup",
                index: None,
                cond: rec.then(|| {
                    s.pk.iter()
                        .map(|i| format!("{} = {}", col(*i), p.eq[i].1))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                }),
                cost: SEEK + FETCH_ROW,
            });
        }
    }
    let mut best = Access {
        path: Path::Full,
        op: "Seq Scan",
        index: None,
        cond: None,
        cost: SEEK + rows * SEQ_ROW,
    };
    let mut consider = |a: Access| {
        if a.cost < best.cost {
            best = a;
        }
    };
    let range_text = |ci: usize| -> Vec<String> {
        let mut v = Vec::new();
        if let Some(b) = p.lo.get(&ci) {
            v.push(format!("{} {} {}", col(ci), if b.inclusive { ">=" } else { ">" }, b.src));
        }
        if let Some(b) = p.hi.get(&ci) {
            v.push(format!("{} {} {}", col(ci), if b.inclusive { "<=" } else { "<" }, b.src));
        }
        v
    };
    let range_sel = |ci: usize| est.range_sel(ci, p.lo.get(&ci).map(|b| (&b.v, b.inclusive)), p.hi.get(&ci).map(|b| (&b.v, b.inclusive)));
    // 2. primary-key IN list
    if exact_pk {
        let pi = s.pk[0];
        if let Some(vals) = p.ins.get(&pi) {
            if vals.iter().all(|v| key_compatible(s.cols[pi].ty, v, true)) {
                let mut ks: Vec<Vec<u8>> = Vec::new();
                for v in vals {
                    let mut k = keys::row_prefix(&s.name);
                    k.extend(keys::encode_id(v)?);
                    ks.push(k);
                }
                ks.sort();
                ks.dedup();
                let n = ks.len() as f64;
                consider(Access {
                    cond: rec.then(|| format!("{} IN ({} keys)", col(pi), ks.len())),
                    path: Path::PkIn(ks),
                    op: "Primary Key IN Lookup",
                    index: None,
                    cost: n * (SEEK + FETCH_ROW),
                });
            }
        }
    }
    // 3. composite primary-key prefix, or a range on a one-column key
    if s.pk.len() > 1 {
        let mut k = 0;
        while k < s.pk.len() && p.eq_val(s.pk[k]).map_or(false, |v| key_compatible(s.cols[s.pk[k]].ty, v, false)) {
            k += 1;
        }
        let mut base = keys::row_prefix(&s.name);
        let mut sel = 1.0;
        let mut cond: Vec<String> = Vec::new();
        for &i in &s.pk[..k] {
            let (v, src) = &p.eq[&i];
            base.extend(enc_component(v));
            sel *= est.eq_sel(i, v);
            if rec {
                cond.push(format!("{} = {src}", col(i)));
            }
        }
        let (mut lo, mut hi) = (None, None);
        if k < s.pk.len() {
            let ci = s.pk[k];
            let ty = s.cols[ci].ty;
            lo = p.lo.get(&ci).filter(|b| key_compatible(ty, &b.v, false)).map(|b| enc_component(&b.v));
            hi = p.hi.get(&ci).filter(|b| key_compatible(ty, &b.v, false)).map(|b| enc_component(&b.v));
            if lo.is_some() || hi.is_some() {
                sel *= range_sel(ci);
                if rec {
                    cond.extend(range_text(ci));
                }
            }
        }
        if k > 0 || lo.is_some() || hi.is_some() {
            let e = rows * sel;
            consider(Access {
                path: Path::Range(Prefixed {
                    base,
                    is_index: false,
                    total_components: 0,
                    lo,
                    hi,
                }),
                op: if k > 0 { "Primary Key Prefix Scan" } else { "Primary Key Range Scan" },
                index: None,
                cond: rec.then(|| cond.join(" AND ")),
                cost: SEEK + e * SEQ_ROW,
            });
        }
    } else if exact_pk {
        let ci = s.pk[0];
        let ty = s.cols[ci].ty;
        let enc = |b: &Bnd| key_compatible(ty, &b.v, true).then(|| keys::encode_id(&b.v).ok()).flatten();
        let lo = p.lo.get(&ci).and_then(enc);
        let hi = p.hi.get(&ci).and_then(enc);
        if lo.is_some() || hi.is_some() {
            let e = rows * range_sel(ci);
            consider(Access {
                path: Path::Range(Prefixed {
                    base: keys::row_prefix(&s.name),
                    is_index: false,
                    total_components: 0,
                    lo,
                    hi,
                }),
                op: "Primary Key Range Scan",
                index: None,
                cond: rec.then(|| range_text(ci).join(" AND ")),
                cost: SEEK + e * SEQ_ROW,
            });
        }
    }
    // 4. secondary indexes: leftmost equalities, then an IN list or a range
    for ix in &s.indexes {
        let Ok(pos) = s.positions(&ix.cols) else { continue };
        let single = pos.len() == 1;
        let enc = |v: &Value| if single { keys::encode_index_value(v) } else { Some(enc_component(v)) };
        let mut k = 0;
        while k < pos.len() && p.eq_val(pos[k]).map_or(false, |v| key_compatible(s.cols[pos[k]].ty, v, false)) {
            k += 1;
        }
        let mut base = index_base(&s.name, &ix.cols);
        let eq_vals: Vec<&Value> = pos[..k].iter().map(|i| &p.eq[i].0).collect();
        if k > 0 {
            match index_values(&eq_vals) {
                Some(e) => base.extend(e),
                None => continue,
            }
        }
        let mut sel: f64 = pos[..k].iter().map(|&i| est.eq_sel(i, &p.eq[&i].0)).product();
        if k == pos.len() && ix.unique {
            sel = sel.min(1.0 / rows.max(1.0));
        }
        let mut cond: Vec<String> = if rec {
            pos[..k].iter().map(|&i| format!("{} = {}", col(i), p.eq[&i].1)).collect()
        } else {
            Vec::new()
        };
        let rest = pos.len() - k;
        if k < pos.len() {
            let ci = pos[k];
            let ty = s.cols[ci].ty;
            // IN list on the next column: one prefix per value
            if let Some(vals) = p.ins.get(&ci).filter(|v| v.iter().all(|x| key_compatible(ty, x, false))) {
                let mut seen = HashSet::new();
                let mut runs = Vec::new();
                for v in vals {
                    if let Some(e) = enc(v) {
                        if seen.insert(e.clone()) {
                            let mut b = base.clone();
                            b.extend(e);
                            runs.push(Prefixed {
                                base: b,
                                is_index: true,
                                total_components: rest - 1,
                                lo: None,
                                hi: None,
                            });
                        }
                    }
                }
                let entries = rows * sel * est.in_sel(ci, vals);
                let mut c = cond.clone();
                c.push(format!("{} IN ({} values)", col(ci), runs.len()));
                consider(Access {
                    cost: runs.len() as f64 * SEEK + entries * (IDX_ENTRY + FETCH_ROW),
                    path: Path::IndexIn(runs),
                    op: "Index IN Scan",
                    index: Some(ix.name.clone()),
                    cond: rec.then(|| c.join(" AND ")),
                });
            }
            let lo = p.lo.get(&ci).filter(|b| key_compatible(ty, &b.v, false)).and_then(|b| enc(&b.v));
            let hi = p.hi.get(&ci).filter(|b| key_compatible(ty, &b.v, false)).and_then(|b| enc(&b.v));
            if lo.is_some() || hi.is_some() {
                sel *= range_sel(ci);
                if rec {
                    cond.extend(range_text(ci));
                }
                let entries = rows * sel;
                consider(Access {
                    path: Path::Range(Prefixed {
                        base,
                        is_index: true,
                        total_components: rest,
                        lo,
                        hi,
                    }),
                    op: if k > 0 { "Index Scan" } else { "Index Range Scan" },
                    index: Some(ix.name.clone()),
                    cond: rec.then(|| cond.join(" AND ")),
                    cost: SEEK + entries * (IDX_ENTRY + FETCH_ROW),
                });
                continue;
            }
        }
        if k > 0 {
            let entries = rows * sel;
            consider(Access {
                path: Path::Range(Prefixed {
                    base,
                    is_index: true,
                    total_components: rest,
                    lo: None,
                    hi: None,
                }),
                op: "Index Scan",
                index: Some(ix.name.clone()),
                cond: rec.then(|| cond.join(" AND ")),
                cost: SEEK + entries * (IDX_ENTRY + FETCH_ROW),
            });
        }
    }
    Ok(best)
}

fn get_row(q: &QCtx, key: Vec<u8>) -> Result<Option<(Vec<u8>, Vec<Value>)>> {
    Ok(match q.ctx.get(&key) {
        Some(raw) => Some((key, decode_row(&raw)?)),
        None => None,
    })
}

/// Read the rows a path selects, in primary-key order.
pub(super) fn exec_access(q: &QCtx, s: &Schema, a: &Access) -> Result<(Rows, Work)> {
    let mut w = Work::default();
    let rows = match &a.path {
        Path::Full => {
            let r = all_rows(q.ctx, s)?;
            w.examined = r.len() as u64;
            r
        }
        Path::PkPoint(k) => {
            let r: Rows = get_row(q, k.clone())?.into_iter().collect();
            w.examined = r.len() as u64;
            r
        }
        Path::PkIn(ks) => {
            let mut out = Vec::new();
            for k in ks {
                out.extend(get_row(q, k.clone())?);
            }
            w.examined = out.len() as u64;
            out
        }
        Path::Range(c) => scan_prefixed(q, s, c, &mut w)?,
        Path::IndexIn(runs) => {
            let mut out = Vec::new();
            for c in runs {
                out.extend(scan_prefixed(q, s, c, &mut w)?);
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out.dedup_by(|a, b| a.0 == b.0);
            out
        }
    };
    Ok((rows, w))
}

fn scan_prefixed(q: &QCtx, s: &Schema, c: &Prefixed, w: &mut Work) -> Result<Rows> {
    let mut start = c.base.clone();
    if let Some(lo) = &c.lo {
        start.extend_from_slice(lo);
    }
    let end = match &c.hi {
        Some(hi) => {
            let mut e = c.base.clone();
            e.extend_from_slice(hi);
            prefix_end(&e)
        }
        None => prefix_end(&c.base),
    };
    let entries = q.ctx.scan_range(&c.base, &start, end.as_deref());
    if !c.is_index {
        w.examined += entries.len() as u64;
        return entries.into_iter().map(|(k, raw)| Ok((k, decode_row(&raw)?))).collect();
    }
    w.index_entries += entries.len() as u64;
    let mut out = Vec::with_capacity(entries.len());
    for (ik, _) in entries {
        let mut pos = c.base.len();
        for _ in 0..c.total_components {
            pos += component_len(&ik[pos..]).ok_or_else(|| Error::Corrupt("index entry is unreadable".into()))?;
        }
        let mut k = keys::row_prefix(&s.name);
        k.extend_from_slice(&ik[pos..]);
        out.extend(get_row(q, k)?);
    }
    w.examined += out.len() as u64;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// Statistics are only needed when the primary key cannot answer directly.
fn estimator_for<'s>(q: &QCtx, s: &'s Schema, p: &Preds) -> Estimator<'s> {
    if !s.pk.is_empty() && s.pk.iter().all(|i| p.eq.contains_key(i)) {
        Estimator { s, st: None }
    } else {
        Estimator::new(q.ctx, s)
    }
}

/// Candidate rows of a table for a WHERE clause (UPDATE / DELETE). The
/// caller still applies the full filter.
pub(super) fn fetch(q: &QCtx, s: &Schema, alias: &str, filter: Option<&Expr>) -> Result<Rows> {
    let mut p = Preds::default();
    if let Some(f) = filter {
        // no enclosing query, so no column is ever "outer"
        collect_preds(f, s, alias, q, Consts::none(&[]), &mut p);
    }
    let est = estimator_for(q, s, &p);
    Ok(exec_access(q, s, &plan_access(q, s, &p, &est)?)?.0)
}

/// Rows whose columns `cols` equal `vals` exactly (all non-NULL).
pub(super) fn find_eq(q: &QCtx, s: &Schema, cols: &[usize], vals: &[Value]) -> Result<Rows> {
    let mut p = Preds::default();
    for (c, v) in cols.iter().zip(vals) {
        p.eq.insert(*c, (v.clone(), String::new()));
    }
    let est = estimator_for(q, s, &p);
    let mut rows = exec_access(q, s, &plan_access(q, s, &p, &est)?)?.0;
    rows.retain(|(_, r)| cols.iter().zip(vals).all(|(c, v)| r[*c].total_cmp(v) == Ordering::Equal));
    Ok(rows)
}

// ============================================================ index probes

/// Repeated lookups into a table by the values of join columns: the inner
/// side of an index nested-loop join.
pub(super) struct Probe {
    kind: ProbeKind,
    /// Columns of the probed table, in key order, and for each the position
    /// in the outer row of the value to look up.
    pub cols: Vec<usize>,
    pub outer_pos: Vec<usize>,
    pub index: String,
    /// Rows expected per lookup.
    pub est: f64,
    pub cost: f64,
}

enum ProbeKind {
    PkPoint,
    PkPrefix,
    Index(Vec<String>),
}

/// The cheapest way to look rows up by equality on some of `keys`
/// (outer position, probed column), if any key or index allows it.
pub(super) fn choose_probe(s: &Schema, est: &Estimator, keys: &[(usize, usize)]) -> Option<Probe> {
    let outer_of = |c: usize| keys.iter().find(|(_, rc)| *rc == c).map(|(l, _)| *l);
    let rows = est.rows();
    let mut best: Option<Probe> = None;
    let mut consider = |p: Probe| {
        if best.as_ref().map_or(true, |b| p.cost < b.cost) {
            best = Some(p);
        }
    };
    let leading = |cols: &[usize]| cols.iter().take_while(|c| outer_of(**c).is_some()).count();
    if !s.pk.is_empty() && leading(&s.pk) == s.pk.len() {
        consider(Probe {
            kind: ProbeKind::PkPoint,
            outer_pos: s.pk.iter().map(|c| outer_of(*c).unwrap()).collect(),
            cols: s.pk.clone(),
            index: "primary key".into(),
            est: rows.min(1.0),
            cost: SEEK + FETCH_ROW,
        });
    } else if s.pk.len() > 1 {
        let k = leading(&s.pk);
        if k > 0 {
            let cols = s.pk[..k].to_vec();
            let e = rows * cols.iter().map(|c| est.eq_sel_unknown(*c)).product::<f64>();
            consider(Probe {
                kind: ProbeKind::PkPrefix,
                outer_pos: cols.iter().map(|c| outer_of(*c).unwrap()).collect(),
                cols,
                index: "primary key".into(),
                est: e,
                cost: SEEK + e * SEQ_ROW,
            });
        }
    }
    for ix in &s.indexes {
        let Ok(pos) = s.positions(&ix.cols) else { continue };
        let k = leading(&pos);
        if k == 0 {
            continue;
        }
        let cols = pos[..k].to_vec();
        let mut e = rows * cols.iter().map(|c| est.eq_sel_unknown(*c)).product::<f64>();
        if k == pos.len() && ix.unique {
            e = e.min(1.0);
        }
        consider(Probe {
            kind: ProbeKind::Index(ix.cols.clone()),
            outer_pos: cols.iter().map(|c| outer_of(*c).unwrap()).collect(),
            cols,
            index: ix.name.clone(),
            est: e,
            cost: SEEK + e * (IDX_ENTRY + FETCH_ROW),
        });
    }
    best
}

impl Probe {
    /// Rows of `s` whose probe columns equal the values in `outer`. A NULL
    /// or a value of a type no stored key can equal finds nothing (the same
    /// pairs a hash join would match).
    pub fn fetch(&self, q: &QCtx, s: &Schema, outer: &[Value], w: &mut Work) -> Result<Rows> {
        let mut vals = Vec::with_capacity(self.cols.len());
        for (&c, &p) in self.cols.iter().zip(&self.outer_pos) {
            let v = &outer[p];
            let exact = matches!(self.kind, ProbeKind::PkPoint) && s.pk.len() == 1;
            let v = if exact {
                exact_key_value(s.cols[c].ty, v)
            } else {
                key_compatible(s.cols[c].ty, v, false).then(|| v.clone())
            };
            match v {
                Some(v) => vals.push(v),
                None => return Ok(Vec::new()),
            }
        }
        match &self.kind {
            ProbeKind::PkPoint => {
                let mut row = vec![Value::Null; s.cols.len()];
                for (&c, v) in self.cols.iter().zip(vals) {
                    row[c] = v;
                }
                let r: Rows = get_row(q, pk_key(s, &row)?)?.into_iter().collect();
                w.examined += r.len() as u64;
                Ok(r)
            }
            ProbeKind::PkPrefix => {
                let mut base = keys::row_prefix(&s.name);
                for v in &vals {
                    base.extend(enc_component(v));
                }
                scan_prefixed(
                    q,
                    s,
                    &Prefixed {
                        base,
                        is_index: false,
                        total_components: 0,
                        lo: None,
                        hi: None,
                    },
                    w,
                )
            }
            ProbeKind::Index(cols) => {
                let mut base = index_base(&s.name, cols);
                let refs: Vec<&Value> = vals.iter().collect();
                match index_values(&refs) {
                    Some(e) => base.extend(e),
                    None => return Ok(Vec::new()),
                }
                let total_components = cols.len() - vals.len();
                scan_prefixed(
                    q,
                    s,
                    &Prefixed {
                        base,
                        is_index: true,
                        total_components,
                        lo: None,
                        hi: None,
                    },
                    w,
                )
            }
        }
    }
}
