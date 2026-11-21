//! The shared query planner.
//!
//! parse (pbsql::parser) -> bind & validate names (this module) ->
//! logical plan -> rule-based optimisation (predicate pushdown into scans
//! and joins, limit pushdown) -> physical plan (access path and join
//! strategy selection) -> execution (`exec`).
//!
//! Every data model plugs in at the scan level: relational tables, document
//! collections and key-value namespaces all produce rows, so filters,
//! joins (including cross-model joins), grouping, sorting and unions work
//! the same way over any mix of them.

use crate::error::{Error, Result};
use crate::pbsql::ast::*;
use crate::query::eval::{coerce_type, eval_const, known_function};
use crate::rel::{self, Index, Schema};
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::BTreeSet;

// =================================================================== scopes

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColKind {
    Column,
    /// The whole document of a collection source; paths read into it.
    Doc,
}

#[derive(Debug, Clone)]
pub struct ScopeCol {
    pub alias: String,
    pub name: String,
    pub kind: ColKind,
    /// Declared type of a relational column (used to type untyped parameters).
    pub ty: Option<Ty>,
}

pub type Scope = Vec<ScopeCol>;

// ============================================================ logical plan

#[derive(Debug, Clone)]
pub enum SourceKind {
    Table(Schema),
    Collection(String),
    Kv(String),
}

impl SourceKind {
    fn scope(&self, alias: &str) -> Scope {
        let col = |n: &str, kind| ScopeCol {
            alias: alias.to_string(),
            name: n.to_string(),
            kind,
            ty: None,
        };
        match self {
            SourceKind::Table(s) => s
                .cols
                .iter()
                .map(|c| ScopeCol {
                    ty: Some(c.ty),
                    ..col(&c.name, ColKind::Column)
                })
                .collect(),
            SourceKind::Collection(_) => vec![col(alias, ColKind::Doc)],
            SourceKind::Kv(_) => vec![col("key", ColKind::Column), col("value", ColKind::Column)],
        }
    }
}

#[derive(Debug, Clone)]
pub struct AggSpec {
    pub func: AggFn,
    pub arg: Option<Expr>,
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub enum Logical {
    /// One empty row (SELECT without FROM).
    Unit,
    Scan {
        src: SourceKind,
        alias: String,
        filter: Vec<Expr>,
    },
    Derived {
        input: Box<Logical>,
        alias: String,
    },
    Filter {
        input: Box<Logical>,
        pred: Expr,
    },
    Join {
        left: Box<Logical>,
        right: Box<Logical>,
        kind: JoinKind,
        on: Vec<Expr>,
        right_width: usize,
    },
    Aggregate {
        input: Box<Logical>,
        group: Vec<Expr>,
        aggs: Vec<AggSpec>,
        width: usize,
    },
    Project {
        input: Box<Logical>,
        exprs: Vec<Expr>,
    },
    Distinct {
        input: Box<Logical>,
    },
    Sort {
        input: Box<Logical>,
        keys: Vec<(Expr, bool)>,
    },
    Limit {
        input: Box<Logical>,
        limit: Option<usize>,
        offset: usize,
    },
    Union {
        left: Box<Logical>,
        right: Box<Logical>,
        all: bool,
    },
}

// =========================================================== physical plan

#[derive(Debug, Clone)]
pub struct Bound {
    pub expr: Expr,
    pub inclusive: bool,
}

#[derive(Debug, Clone)]
pub enum TablePath {
    FullScan,
    /// Point lookups by full primary key (one entry per IN-list value).
    PkPoints(Vec<Vec<Expr>>),
    /// Leading primary-key equalities plus an optional range on the next key column.
    PkRange {
        eq: Vec<Expr>,
        lo: Option<Bound>,
        hi: Option<Bound>,
    },
    /// Secondary index: equality on leading columns, optional range on the next one.
    Index {
        ix: Index,
        eq: Vec<Expr>,
        lo: Option<Bound>,
        hi: Option<Bound>,
    },
    /// Secondary index point lookups for an IN list on its first column.
    IndexIn {
        ix: Index,
        vals: Vec<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum DocPath {
    FullScan,
    Ids(Vec<Expr>),
    /// A (possibly compound) document index: equality on leading fields,
    /// optional range on the next one.
    Index(crate::docindex::Choice<Expr>),
}

#[derive(Debug, Clone)]
pub enum KvPath {
    FullScan,
    Keys(Vec<Expr>),
    Prefix(String),
    Range {
        lo: Option<Bound>,
        hi: Option<Bound>,
    },
}

#[derive(Debug, Clone)]
pub enum Access {
    Table { schema: Schema, path: TablePath },
    Doc { coll: String, path: DocPath },
    Kv { ns: String, path: KvPath },
}

#[derive(Debug, Clone)]
pub enum PhysOp {
    Unit,
    Scan {
        access: Access,
        alias: String,
        filter: Option<Expr>,
        limit: Option<usize>,
        /// Read the access path in descending key order (to satisfy ORDER BY ... DESC).
        reverse: bool,
    },
    Derived {
        input: Box<Node>,
        alias: String,
    },
    Filter {
        input: Box<Node>,
        pred: Expr,
    },
    NestedLoopJoin {
        left: Box<Node>,
        right: Box<Node>,
        on: Option<Expr>,
        kind: JoinKind,
        right_width: usize,
    },
    HashJoin {
        left: Box<Node>,
        right: Box<Node>,
        lkeys: Vec<Expr>,
        rkeys: Vec<Expr>,
        on: Option<Expr>,
        kind: JoinKind,
        right_width: usize,
    },
    /// For each left row, look matching rows up in the right source through
    /// its index / primary key / document id / key-value key.
    LookupJoin {
        left: Box<Node>,
        right: Box<Node>,
        on: Option<Expr>,
        kind: JoinKind,
        right_width: usize,
    },
    HashAggregate {
        input: Box<Node>,
        group: Vec<Expr>,
        aggs: Vec<AggSpec>,
        width: usize,
    },
    Project {
        input: Box<Node>,
        exprs: Vec<Expr>,
    },
    Distinct {
        input: Box<Node>,
    },
    Sort {
        input: Box<Node>,
        keys: Vec<(Expr, bool)>,
    },
    Limit {
        input: Box<Node>,
        limit: Option<usize>,
        offset: usize,
    },
    Union {
        left: Box<Node>,
        right: Box<Node>,
        all: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: usize,
    pub op: PhysOp,
}

/// A planned query: the root operator tree, subquery plans, output columns.
#[derive(Debug, Clone)]
pub struct Plan {
    pub root: Node,
    pub subs: Vec<SubPlan>,
    pub columns: Vec<String>,
    /// Static column types, where certain (for describing results before execution).
    pub types: Vec<Option<Ty>>,
    /// `SELECT *` from a single collection: return documents, not rows.
    pub docs_output: bool,
    pub node_count: usize,
}

#[derive(Debug, Clone)]
pub struct SubPlan {
    pub root: Node,
    pub correlated: bool,
}

// ================================================================== binder

pub struct Planner<'c, 'a> {
    ctx: &'c Ctx<'a>,
    /// Logical plans of subqueries, by id.
    subs: Vec<(Logical, bool)>,
    /// Scopes of enclosing queries, innermost first.
    outer: Vec<Scope>,
    /// Deepest outer level referenced while binding each query on the stack.
    used: Vec<usize>,
    /// Nesting depth (subqueries and derived tables) of the query being bound.
    depth: usize,
    /// The top-level query is `SELECT *` over one collection.
    docs_flag: bool,
    /// Static result types of the top-level query's columns (None = unknown).
    top_types: Option<Vec<Option<Ty>>>,
}

fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            conjuncts(*a, out);
            conjuncts(*b, out);
        }
        e => out.push(e),
    }
}

fn and_all(mut v: Vec<Expr>) -> Option<Expr> {
    let first = if v.is_empty() {
        return None;
    } else {
        v.remove(0)
    };
    Some(
        v.into_iter()
            .fold(first, |a, b| Expr::Bin(Box::new(a), Op::And, Box::new(b))),
    )
}

/// Visit every direct sub-expression.
fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Lit(_)
        | Expr::Col(_)
        | Expr::Slot { .. }
        | Expr::Default
        | Expr::Untyped(_)
        | Expr::Exists(..)
        | Expr::ScalarQuery(_) => vec![],
        Expr::Not(a)
        | Expr::Neg(a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::InQuery(a, ..) => vec![a],
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => vec![a, b],
        Expr::In(a, l, _) => std::iter::once(&**a).chain(l.iter()).collect(),
        Expr::Agg(_, a, _) => a.iter().map(|x| &**x).collect(),
        Expr::Func(_, a) => a.iter().collect(),
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => operand
            .iter()
            .map(|x| &**x)
            .chain(whens.iter().flat_map(|(w, t)| [w, t]))
            .chain(otherwise.iter().map(|x| &**x))
            .collect(),
    }
}

fn map_children(e: Expr, f: &mut dyn FnMut(Expr) -> Result<Expr>) -> Result<Expr> {
    let b = |x: Box<Expr>, f: &mut dyn FnMut(Expr) -> Result<Expr>| f(*x).map(Box::new);
    Ok(match e {
        Expr::Not(a) => Expr::Not(b(a, f)?),
        Expr::Neg(a) => Expr::Neg(b(a, f)?),
        Expr::IsNull(a, n) => Expr::IsNull(b(a, f)?, n),
        Expr::Cast(a, t) => Expr::Cast(b(a, f)?, t),
        Expr::InQuery(a, s, n) => Expr::InQuery(b(a, f)?, s, n),
        Expr::Bin(a, op, c) => Expr::Bin(b(a, f)?, op, b(c, f)?),
        Expr::Like(a, c, n) => Expr::Like(b(a, f)?, b(c, f)?, n),
        Expr::In(a, l, n) => Expr::In(
            b(a, f)?,
            l.into_iter().map(&mut *f).collect::<Result<_>>()?,
            n,
        ),
        Expr::Agg(func, a, d) => Expr::Agg(func, a.map(|x| b(x, f)).transpose()?, d),
        Expr::Func(n, a) => Expr::Func(n, a.into_iter().map(&mut *f).collect::<Result<_>>()?),
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => Expr::Case {
            operand: operand.map(|x| b(x, f)).transpose()?,
            whens: whens
                .into_iter()
                .map(|(w, t)| Ok((f(w)?, f(t)?)))
                .collect::<Result<_>>()?,
            otherwise: otherwise.map(|x| b(x, f)).transpose()?,
        },
        other => other,
    })
}

/// Level-0 slots an expression reads. `None` if it contains a subquery
/// (which might be correlated), so it must stay where it was written.
fn refs(e: &Expr, subs: &[(Logical, bool)]) -> Option<BTreeSet<usize>> {
    let mut out = BTreeSet::new();
    fn walk(e: &Expr, subs: &[(Logical, bool)], out: &mut BTreeSet<usize>) -> bool {
        let sub_ok =
            |s: &SubRef| matches!(s, SubRef::Bound(i) if !subs.get(*i).is_none_or(|x| x.1));
        match e {
            Expr::Slot { level: 0, idx, .. } => {
                out.insert(*idx);
            }
            Expr::Exists(s, _) | Expr::ScalarQuery(s) => return sub_ok(s),
            Expr::InQuery(a, s, _) => return sub_ok(s) && walk(a, subs, out),
            _ => {}
        }
        children(e).into_iter().all(|c| walk(c, subs, out))
    }
    walk(e, subs, &mut out).then_some(out)
}

fn has_agg(e: &Expr) -> bool {
    matches!(e, Expr::Agg(..)) || children(e).into_iter().any(has_agg)
}

/// Add `delta` to level-0 slot indexes (moving an expression to another row layout).
pub fn shift_slots(e: Expr, delta: isize) -> Expr {
    match e {
        Expr::Slot {
            level: 0,
            idx,
            path,
            name,
        } => Expr::Slot {
            level: 0,
            idx: (idx as isize + delta) as usize,
            path,
            name,
        },
        other => map_children(other, &mut |c| Ok(shift_slots(c, delta))).expect("infallible"),
    }
}

/// Raise the level of slots at or above `min_level` by `by` (an expression
/// moved one row-context deeper).
pub fn shift_levels(e: Expr, min_level: usize, by: usize) -> Expr {
    match e {
        Expr::Slot {
            level,
            idx,
            path,
            name,
        } if level >= min_level => Expr::Slot {
            level: level + by,
            idx,
            path,
            name,
        },
        other => {
            map_children(other, &mut |c| Ok(shift_levels(c, min_level, by))).expect("infallible")
        }
    }
}

fn label(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => v.display(),
        Expr::Col(p) => p.last().cloned().unwrap_or_default(),
        Expr::Slot { name, .. } => name.clone(),
        Expr::Bin(a, op, b) => format!("{} {} {}", label(a), op.text().to_lowercase(), label(b)),
        Expr::Neg(a) => format!("-{}", label(a)),
        Expr::Not(a) => format!("not {}", label(a)),
        Expr::Agg(f, arg, d) => format!(
            "{}({}{})",
            f.name(),
            if *d { "distinct " } else { "" },
            arg.as_ref().map_or("*".to_string(), |a| label(a))
        ),
        Expr::Func(n, args) => format!(
            "{n}({})",
            args.iter().map(label).collect::<Vec<_>>().join(", ")
        ),
        Expr::Cast(a, _) => label(a),
        Expr::ScalarQuery(_) => "subquery".into(),
        Expr::Exists(..) => "exists".into(),
        Expr::Case { .. } => "case".into(),
        _ => "expr".to_string(),
    }
}

pub fn valid_collection(name: &str) -> bool {
    let mut parts = name.split('.');
    let ok = parts.by_ref().take(2).all(crate::keys::valid_name);
    ok && parts.next().is_none() && !name.is_empty()
}

impl<'c, 'a> Planner<'c, 'a> {
    pub fn new(ctx: &'c Ctx<'a>) -> Self {
        Planner {
            ctx,
            subs: Vec::new(),
            outer: Vec::new(),
            used: vec![0],
            depth: 0,
            docs_flag: false,
            top_types: None,
        }
    }

    fn resolve_in(scope: &Scope, path: &[String]) -> Result<Option<(usize, Vec<String>, String)>> {
        if path.len() >= 2 {
            let a = path[0].to_lowercase();
            let cols: Vec<usize> = (0..scope.len()).filter(|i| scope[*i].alias == a).collect();
            if !cols.is_empty() {
                if let [i] = cols[..] {
                    if scope[i].kind == ColKind::Doc {
                        return Ok(Some((i, path[1..].to_vec(), path[1..].join("."))));
                    }
                }
                let want = path[1].to_lowercase();
                for i in cols {
                    if scope[i].name == path[1] || scope[i].name == want {
                        return Ok(Some((i, path[2..].to_vec(), path[1..].join("."))));
                    }
                }
                return Err(Error::Schema(format!(
                    "no such column '{}.{}'",
                    path[0], path[1]
                )));
            }
        }
        if path.len() == 1 {
            let a = path[0].to_lowercase();
            if let Some(i) = scope
                .iter()
                .position(|c| c.kind == ColKind::Doc && c.alias == a)
            {
                return Ok(Some((i, vec![], scope[i].name.clone())));
            }
        }
        let want = path[0].to_lowercase();
        let hits: Vec<usize> = (0..scope.len())
            .filter(|i| {
                scope[*i].kind == ColKind::Column
                    && (scope[*i].name == path[0] || scope[*i].name == want)
            })
            .collect();
        match hits.len() {
            1 => return Ok(Some((hits[0], path[1..].to_vec(), path.join(".")))),
            n if n > 1 => {
                return Err(Error::Schema(format!(
                    "column '{}' is ambiguous; qualify it with a table name",
                    path[0]
                )))
            }
            _ => {}
        }
        let docs: Vec<usize> = (0..scope.len())
            .filter(|i| scope[*i].kind == ColKind::Doc)
            .collect();
        match docs.len() {
            1 => Ok(Some((docs[0], path.to_vec(), path.join(".")))),
            0 => Ok(None),
            _ => Err(Error::Schema(format!(
                "field '{}' is ambiguous between several collections; qualify it with an alias",
                path.join(".")
            ))),
        }
    }

    fn resolve(&mut self, scope: &Scope, path: &[String]) -> Result<Expr> {
        if let Some((idx, p, name)) = Self::resolve_in(scope, path)? {
            return Ok(Expr::Slot {
                level: 0,
                idx,
                path: p,
                name,
            });
        }
        for (k, sc) in self.outer.iter().enumerate() {
            if let Some((idx, p, name)) = Self::resolve_in(sc, path)? {
                let level = k + 1;
                if let Some(u) = self.used.last_mut() {
                    *u = (*u).max(level);
                }
                return Ok(Expr::Slot {
                    level,
                    idx,
                    path: p,
                    name,
                });
            }
        }
        Err(match path {
            [q, n, ..] if path.len() == 2 => Error::Schema(format!("no such column '{q}.{n}'")),
            _ => Error::Schema(format!("no such column '{}'", path.join("."))),
        })
    }

    /// Resolve names, plan subqueries and validate functions.
    pub fn bind(&mut self, e: Expr, scope: &Scope) -> Result<Expr> {
        match e {
            Expr::Col(path) => self.resolve(scope, &path),
            Expr::InQuery(a, SubRef::Parsed(q), n) => {
                let a = self.bind(*a, scope)?;
                let id = self.subquery(*q, scope, Some(1))?;
                Ok(Expr::InQuery(Box::new(a), SubRef::Bound(id), n))
            }
            Expr::Exists(SubRef::Parsed(q), n) => Ok(Expr::Exists(
                SubRef::Bound(self.subquery(*q, scope, None)?),
                n,
            )),
            Expr::ScalarQuery(SubRef::Parsed(q)) => Ok(Expr::ScalarQuery(SubRef::Bound(
                self.subquery(*q, scope, Some(1))?,
            ))),
            Expr::Func(name, args) => {
                if !known_function(&name) {
                    return Err(Error::Parse(format!("unknown function '{name}'")));
                }
                let args = args
                    .into_iter()
                    .map(|a| self.bind(a, scope))
                    .collect::<Result<_>>()?;
                Ok(Expr::Func(name, args))
            }
            Expr::Bin(a, op, b)
                if matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge) =>
            {
                let (a, b) = (self.bind(*a, scope)?, self.bind(*b, scope)?);
                // an untyped parameter takes the type of the column it is compared with
                let (a, b) = match (&a, &b) {
                    (Expr::Untyped(t), slot) => (self.typed_untyped(t, slot, scope), b),
                    (slot, Expr::Untyped(t)) => {
                        let nb = self.typed_untyped(t, slot, scope);
                        (a, nb)
                    }
                    _ => (a, b),
                };
                Ok(Expr::Bin(Box::new(a), op, Box::new(b)))
            }
            Expr::In(a, list, n) => {
                let a = self.bind(*a, scope)?;
                let list = list
                    .into_iter()
                    .map(|e| match self.bind(e, scope)? {
                        Expr::Untyped(t) => Ok(self.typed_untyped(&t, &a, scope)),
                        other => Ok(other),
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Expr::In(Box::new(a), list, n))
            }
            other => map_children(other, &mut |c| self.bind(c, scope)),
        }
    }

    /// The static type of a bound expression when it is certain (used to
    /// describe result columns before execution).
    fn infer_ty(&self, e: &Expr, scope: &Scope) -> Option<Ty> {
        use crate::pbsql::ast::AggFn;
        Some(match e {
            Expr::Slot { .. } => return self.slot_ty(e, scope),
            Expr::Lit(v) => match v {
                Value::Int(_) => Ty::Int,
                Value::Float(_) => Ty::Float,
                Value::Bool(_) => Ty::Bool,
                Value::Text(_) => Ty::Text,
                Value::Object(_) | Value::Array(_) => Ty::Json,
                Value::Null => return None,
            },
            Expr::Bin(
                _,
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::And | Op::Or,
                _,
            )
            | Expr::Not(_)
            | Expr::IsNull(..)
            | Expr::Like(..)
            | Expr::In(..)
            | Expr::InQuery(..)
            | Expr::Exists(..) => Ty::Bool,
            Expr::Bin(_, Op::Concat, _) => Ty::Text,
            Expr::Bin(a, _, b) => match (self.infer_ty(a, scope)?, self.infer_ty(b, scope)?) {
                (Ty::Int, Ty::Int) => Ty::Int,
                (Ty::Int | Ty::Float, Ty::Int | Ty::Float) => Ty::Float,
                _ => return None,
            },
            Expr::Neg(a) => match self.infer_ty(a, scope)? {
                t @ (Ty::Int | Ty::Float) => t,
                _ => return None,
            },
            Expr::Agg(AggFn::Count, ..) => Ty::Int,
            Expr::Agg(AggFn::Avg, ..) => Ty::Float,
            Expr::Agg(_, Some(a), _) => match self.infer_ty(a, scope)? {
                Ty::Int => Ty::Int,
                Ty::Float => Ty::Float,
                t => t,
            },
            Expr::Cast(_, t) => *t,
            Expr::Func(n, _) => match n.as_str() {
                "length" | "year" | "month" | "day" | "hour" | "minute" | "second" => Ty::Int,
                "lower" | "upper" | "concat" | "trim" | "substr" | "substring" | "version"
                | "typeof" | "json_type" => Ty::Text,
                "now" | "current_timestamp" | "localtimestamp" | "timestamp" | "datetime" => {
                    Ty::Timestamp
                }
                "current_date" | "date" => Ty::Date,
                "current_time" | "time" => Ty::Time,
                _ => return None,
            },
            _ => return None,
        })
    }

    /// The declared type of a bound column reference, if known.
    fn slot_ty(&self, e: &Expr, scope: &Scope) -> Option<Ty> {
        match e {
            Expr::Slot {
                level, idx, path, ..
            } if path.is_empty() => {
                let sc = if *level == 0 {
                    Some(scope)
                } else {
                    self.outer.get(level - 1)
                };
                sc.and_then(|s| s.get(*idx)).and_then(|c| c.ty)
            }
            _ => None,
        }
    }

    /// An untyped parameter compared with `other`: a literal of the column's type
    /// when known (and convertible), otherwise inferred from its text.
    fn typed_untyped(&self, text: &str, other: &Expr, scope: &Scope) -> Expr {
        let v = match self.slot_ty(other, scope) {
            Some(ty) => crate::query::eval::untyped_as(ty, text),
            None => crate::query::eval::infer_untyped(text),
        };
        Expr::Lit(v)
    }

    fn subquery(&mut self, q: Query, scope: &Scope, want_cols: Option<usize>) -> Result<usize> {
        self.outer.insert(0, scope.clone());
        self.used.push(0);
        self.depth += 1;
        let res = self.query(q);
        self.depth -= 1;
        let used = self.used.pop().unwrap_or(0);
        self.outer.remove(0);
        let (plan, cols) = res?;
        if let Some(n) = want_cols {
            if cols.len() != n {
                return Err(Error::Schema(format!(
                    "subquery must return {n} column, not {}",
                    cols.len()
                )));
            }
        }
        if used > 1 {
            if let Some(u) = self.used.last_mut() {
                *u = (*u).max(used - 1);
            }
        }
        self.subs.push((plan, used >= 1));
        Ok(self.subs.len() - 1)
    }

    fn source(&mut self, tr: TableRef) -> Result<(Logical, Scope)> {
        match tr.source {
            Source::Named(o) => {
                let src = match o.model {
                    Model::Relational => SourceKind::Table(Schema::load(self.ctx, &o.name)?),
                    Model::Document => {
                        if !valid_collection(&o.name) {
                            return Err(Error::Schema(format!(
                                "invalid collection name '{}'",
                                o.name
                            )));
                        }
                        SourceKind::Collection(o.name)
                    }
                    Model::Kv => SourceKind::Kv(o.name),
                };
                let scope = src.scope(&tr.alias);
                Ok((
                    Logical::Scan {
                        src,
                        alias: tr.alias,
                        filter: vec![],
                    },
                    scope,
                ))
            }
            Source::Subquery(q) => {
                // a derived table cannot see the FROM items beside it, only enclosing queries
                self.depth += 1;
                let res = self.query(*q);
                self.depth -= 1;
                let (plan, cols) = res?;
                let scope = cols
                    .into_iter()
                    .map(|n| ScopeCol {
                        alias: tr.alias.clone(),
                        name: n,
                        kind: ColKind::Column,
                        ty: None,
                    })
                    .collect();
                Ok((
                    Logical::Derived {
                        input: Box::new(plan),
                        alias: tr.alias,
                    },
                    scope,
                ))
            }
        }
    }

    /// Plan a full query; returns the logical plan and its output column names.
    pub fn query(&mut self, q: Query) -> Result<(Logical, Vec<String>)> {
        let Query {
            body,
            order_by,
            limit,
            offset,
        } = q;
        if let SetExpr::Select(sel) = body {
            return self.select(*sel, order_by, limit, offset);
        }
        let (plan, cols) = self.set_expr(body)?;
        let scope: Scope = cols
            .iter()
            .map(|c| ScopeCol {
                alias: String::new(),
                name: c.clone(),
                kind: ColKind::Column,
                ty: None,
            })
            .collect();
        let mut plan = plan;
        if !order_by.is_empty() {
            let mut keys = Vec::new();
            for (e, desc) in order_by {
                let e = match e {
                    Expr::Lit(Value::Int(k)) if k >= 1 && (k as usize) <= cols.len() => {
                        Expr::Slot {
                            level: 0,
                            idx: k as usize - 1,
                            path: vec![],
                            name: cols[k as usize - 1].clone(),
                        }
                    }
                    e => self.bind(e, &scope)?,
                };
                keys.push((e, desc));
            }
            plan = Logical::Sort {
                input: Box::new(plan),
                keys,
            };
        }
        if limit.is_some() || offset > 0 {
            plan = Logical::Limit {
                input: Box::new(plan),
                limit,
                offset,
            };
        }
        Ok((plan, cols))
    }

    fn set_expr(&mut self, s: SetExpr) -> Result<(Logical, Vec<String>)> {
        match s {
            SetExpr::Select(sel) => self.select(*sel, vec![], None, 0),
            SetExpr::Union(l, r, all) => {
                let (lp, lc) = self.set_expr(*l)?;
                let (rp, rc) = self.set_expr(*r)?;
                if lc.len() != rc.len() {
                    return Err(Error::Schema(format!(
                        "each UNION branch must return the same number of columns ({} vs {})",
                        lc.len(),
                        rc.len()
                    )));
                }
                Ok((
                    Logical::Union {
                        left: Box::new(lp),
                        right: Box::new(rp),
                        all,
                    },
                    lc,
                ))
            }
        }
    }

    fn select(
        &mut self,
        s: Select,
        order_by: Vec<(Expr, bool)>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<(Logical, Vec<String>)> {
        // 1. FROM: sources and the combined scope
        let mut scope = Scope::new();
        let mut plan = Logical::Unit;
        // per source: (scope offset, width, nullable)
        let mut parts: Vec<(Logical, usize, usize, JoinKind, Option<Expr>)> = Vec::new();
        if let Some(from) = s.from {
            let (p, sc) = self.source(from)?;
            parts.push((p, 0, sc.len(), JoinKind::Inner, None));
            scope.extend(sc);
            for j in s.joins {
                if scope.iter().any(|c| c.alias == j.table.alias) {
                    return Err(Error::Schema(format!(
                        "table name '{}' used twice; give it an alias",
                        j.table.alias
                    )));
                }
                let (p, sc) = self.source(j.table)?;
                let off = scope.len();
                scope.extend(sc);
                let width = scope.len() - off;
                let on = match j.on {
                    Some(e) => Some(self.bind(e, &scope)?),
                    None => None,
                };
                parts.push((p, off, width, j.kind, on));
            }
        }

        // 2. WHERE, split into conjuncts
        let mut where_c = Vec::new();
        if let Some(f) = s.filter {
            let f = self.bind(f, &scope)?;
            if has_agg(&f) {
                return Err(Error::Parse(
                    "aggregate functions are not allowed in WHERE".into(),
                ));
            }
            conjuncts(f, &mut where_c);
        }

        // 3. predicate pushdown and the join tree
        if !parts.is_empty() {
            plan = self.build_joins(parts, where_c)?;
        } else if let Some(f) = and_all(where_c) {
            plan = Logical::Filter {
                input: Box::new(plan),
                pred: f,
            };
        }

        // 4. select list
        let mut items: Vec<(Expr, String, bool)> = Vec::new();
        let mut star_doc = false;
        for it in s.items {
            match it {
                Item::Star => {
                    if scope.is_empty() {
                        return Err(Error::Parse("SELECT * needs a FROM clause".into()));
                    }
                    for (i, c) in scope.iter().enumerate() {
                        star_doc |= c.kind == ColKind::Doc;
                        items.push((
                            Expr::Slot {
                                level: 0,
                                idx: i,
                                path: vec![],
                                name: c.name.clone(),
                            },
                            c.name.clone(),
                            false,
                        ));
                    }
                }
                Item::TableStar(t) => {
                    let before = items.len();
                    for (i, c) in scope.iter().enumerate().filter(|(_, c)| c.alias == t) {
                        items.push((
                            Expr::Slot {
                                level: 0,
                                idx: i,
                                path: vec![],
                                name: c.name.clone(),
                            },
                            c.name.clone(),
                            false,
                        ));
                    }
                    if items.len() == before {
                        return Err(Error::Schema(format!("no table '{t}' in this query")));
                    }
                }
                Item::Expr(e, alias) => {
                    let e = self.bind(e, &scope)?;
                    let name = alias.clone().unwrap_or_else(|| label(&e));
                    items.push((e, name, alias.is_some()));
                }
            }
        }
        let docs_output = star_doc && scope.len() == 1 && items.len() == 1;
        if self.depth == 0 && self.top_types.is_none() {
            self.top_types = Some(
                items
                    .iter()
                    .map(|(e, _, _)| self.infer_ty(e, &scope))
                    .collect(),
            );
        }

        // ORDER BY may name a select alias or a 1-based output position
        let mut order = Vec::new();
        for (e, desc) in order_by {
            let e = match e {
                Expr::Col(p)
                    if p.len() == 1
                        && items
                            .iter()
                            .any(|(_, n, a)| *a && n.eq_ignore_ascii_case(&p[0])) =>
                {
                    items
                        .iter()
                        .find(|(_, n, a)| *a && n.eq_ignore_ascii_case(&p[0]))
                        .unwrap()
                        .0
                        .clone()
                }
                Expr::Lit(Value::Int(k)) if k >= 1 && (k as usize) <= items.len() => {
                    items[k as usize - 1].0.clone()
                }
                e => self.bind(e, &scope)?,
            };
            order.push((e, desc));
        }
        let mut group = Vec::new();
        for e in s.group_by {
            let e = match e {
                Expr::Lit(Value::Int(k)) if k >= 1 && (k as usize) <= items.len() => {
                    items[k as usize - 1].0.clone()
                }
                Expr::Col(p)
                    if p.len() == 1
                        && !matches!(Self::resolve_in(&scope, &p)?, Some((i, _, _)) if scope[i].kind == ColKind::Column) =>
                {
                    match items
                        .iter()
                        .find(|(_, n, a)| *a && n.eq_ignore_ascii_case(&p[0]))
                    {
                        Some(i) => i.0.clone(),
                        None => self.bind(Expr::Col(p), &scope)?,
                    }
                }
                e => self.bind(e, &scope)?,
            };
            if has_agg(&e) {
                return Err(Error::Parse(
                    "aggregate functions are not allowed in GROUP BY".into(),
                ));
            }
            group.push(e);
        }
        let having = match s.having {
            Some(h) => Some(self.bind(h, &scope)?),
            None => None,
        };

        // 5. aggregation
        let aggregated = !group.is_empty()
            || items.iter().any(|i| has_agg(&i.0))
            || having.as_ref().is_some_and(has_agg)
            || order.iter().any(|o| has_agg(&o.0));
        if aggregated {
            let width = scope.len();
            let mut aggs: Vec<AggSpec> = Vec::new();
            let mut extract = |e: Expr| -> Result<Expr> { extract_aggs(e, width, &mut aggs) };
            for it in items.iter_mut() {
                it.0 = extract(it.0.clone())?;
            }
            let having = having.map(&mut extract).transpose()?;
            for o in order.iter_mut() {
                o.0 = extract(o.0.clone())?;
            }
            plan = Logical::Aggregate {
                input: Box::new(plan),
                group,
                aggs,
                width,
            };
            if let Some(h) = having {
                plan = Logical::Filter {
                    input: Box::new(plan),
                    pred: h,
                };
            }
        } else if having.is_some() {
            return Err(Error::Parse("HAVING needs GROUP BY or an aggregate".into()));
        }

        // 6. sort (before projection, so keys can use any column), project, distinct, limit
        if !order.is_empty() {
            plan = Logical::Sort {
                input: Box::new(plan),
                keys: order,
            };
        }
        let names: Vec<String> = items.iter().map(|i| i.1.clone()).collect();
        plan = Logical::Project {
            input: Box::new(plan),
            exprs: items.into_iter().map(|i| i.0).collect(),
        };
        if s.distinct {
            plan = Logical::Distinct {
                input: Box::new(plan),
            };
        }
        if limit.is_some() || offset > 0 {
            plan = Logical::Limit {
                input: Box::new(plan),
                limit,
                offset,
            };
        }
        if docs_output && self.depth == 0 {
            self.docs_flag = true;
        }
        Ok((plan, names))
    }

    /// Push WHERE / ON conjuncts as far down as is semantically safe and
    /// build a left-deep join tree.
    fn build_joins(
        &mut self,
        parts: Vec<(Logical, usize, usize, JoinKind, Option<Expr>)>,
        where_c: Vec<Expr>,
    ) -> Result<Logical> {
        let n = parts.len();
        // a source is null-supplying if it is the right side of a LEFT/FULL JOIN,
        // or sits left of a later RIGHT/FULL JOIN
        let nullable: Vec<bool> = (0..parts.len())
            .map(|k| {
                matches!(parts[k].3, JoinKind::Left | JoinKind::Full)
                    || parts[k + 1..]
                        .iter()
                        .any(|p| matches!(p.3, JoinKind::Right | JoinKind::Full))
            })
            .collect();
        let ranges: Vec<(usize, usize)> = parts.iter().map(|p| (p.1, p.1 + p.2)).collect();
        let owner_of = |set: &BTreeSet<usize>| -> Option<usize> {
            let mut owner = None;
            for s in set {
                let i = ranges.iter().position(|(a, b)| s >= a && s < b)?;
                if owner.is_some_and(|o| o != i) {
                    return None;
                }
                owner = Some(i);
            }
            owner
        };
        let last_source = |set: &BTreeSet<usize>| -> usize {
            set.iter()
                .map(|s| {
                    ranges
                        .iter()
                        .position(|(a, b)| s >= a && s < b)
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
        };
        let mut scan_filters: Vec<Vec<Expr>> = vec![Vec::new(); n];
        let mut join_on: Vec<Vec<Expr>> = vec![Vec::new(); n];
        let mut residual = Vec::new();

        // ON conditions: a conjunct about only the joined source is pushed into its scan
        let mut ons: Vec<Option<Expr>> = parts.iter().map(|p| p.4.clone()).collect();
        for j in 1..n {
            let Some(on) = ons[j].take() else { continue };
            let mut cs = Vec::new();
            conjuncts(on, &mut cs);
            for c in cs {
                if matches!(parts[j].3, JoinKind::Right | JoinKind::Full) {
                    join_on[j].push(c); // the preserved side cannot be filtered early
                    continue;
                }
                match refs(&c, &self.subs) {
                    Some(set) if !set.is_empty() && owner_of(&set) == Some(j) => {
                        scan_filters[j].push(c)
                    }
                    Some(set)
                        if matches!(parts[j].3, JoinKind::Inner | JoinKind::Cross)
                            && !set.is_empty()
                            && last_source(&set) < j =>
                    {
                        match owner_of(&set) {
                            Some(o) if !nullable[o] => scan_filters[o].push(c),
                            _ => join_on[last_source(&set).max(1)].push(c),
                        }
                    }
                    _ => join_on[j].push(c),
                }
            }
        }
        // WHERE conjuncts
        for c in where_c {
            match refs(&c, &self.subs) {
                Some(set) if set.is_empty() => residual.push(c),
                Some(set) => match owner_of(&set) {
                    Some(o) if !nullable[o] => scan_filters[o].push(c),
                    _ => {
                        let j = last_source(&set);
                        // an inner join's condition can absorb it (enables hash/lookup joins)
                        if j > 0
                            && matches!(parts[j].3, JoinKind::Inner | JoinKind::Cross)
                            && !(0..=j).any(|k| {
                                nullable[k]
                                    && set.iter().any(|s| *s >= ranges[k].0 && *s < ranges[k].1)
                            })
                        {
                            join_on[j].push(c);
                        } else {
                            residual.push(c);
                        }
                    }
                },
                None => residual.push(c),
            }
        }

        let mut it = parts.into_iter().zip(scan_filters).enumerate();
        let (_, ((first, off0, _, _, _), f0)) = it.next().expect("at least one source");
        let mut plan = push_scan_filter(first, f0, off0);
        for (j, ((p, off, width, kind, _), f)) in it {
            let right = push_scan_filter(p, f, off);
            let kind = if kind == JoinKind::Cross && !join_on[j].is_empty() {
                JoinKind::Inner
            } else {
                kind
            };
            plan = Logical::Join {
                left: Box::new(plan),
                right: Box::new(right),
                kind,
                on: std::mem::take(&mut join_on[j]),
                right_width: width,
            };
        }
        if let Some(f) = and_all(residual) {
            plan = Logical::Filter {
                input: Box::new(plan),
                pred: f,
            };
        }
        Ok(plan)
    }
}

/// Attach filters (bound to the combined scope) to a source, rebased to its own columns.
fn push_scan_filter(p: Logical, filters: Vec<Expr>, offset: usize) -> Logical {
    if filters.is_empty() {
        return p;
    }
    let local: Vec<Expr> = filters
        .into_iter()
        .map(|e| shift_slots(e, -(offset as isize)))
        .collect();
    match p {
        Logical::Scan {
            src,
            alias,
            mut filter,
        } => {
            filter.extend(local);
            Logical::Scan { src, alias, filter }
        }
        other => Logical::Filter {
            input: Box::new(other),
            pred: and_all(local).expect("non-empty"),
        },
    }
}

fn extract_aggs(e: Expr, width: usize, aggs: &mut Vec<AggSpec>) -> Result<Expr> {
    match e {
        Expr::Agg(func, arg, distinct) => {
            if arg.as_ref().is_some_and(|a| has_agg(a)) {
                return Err(Error::Parse("aggregate functions cannot be nested".into()));
            }
            let name = label(&Expr::Agg(func, arg.clone(), distinct));
            aggs.push(AggSpec {
                func,
                arg: arg.map(|a| *a),
                distinct,
            });
            Ok(Expr::Slot {
                level: 0,
                idx: width + aggs.len() - 1,
                path: vec![],
                name,
            })
        }
        other => map_children(other, &mut |c| extract_aggs(c, width, aggs)),
    }
}

// ======================================================= physical planning

/// A pushed conjunct of the shape `column <op> constant`.
struct Pred {
    col: usize,
    path: Vec<String>,
    op: PredOp,
}

enum PredOp {
    Cmp(Op, Expr),
    In(Vec<Expr>),
    Prefix(String),
}

/// Constant during one execution of the scan: no level-0 references, no subquery.
fn is_const(e: &Expr) -> bool {
    match e {
        Expr::Slot { level, .. } => *level > 0,
        Expr::Exists(..)
        | Expr::ScalarQuery(_)
        | Expr::InQuery(..)
        | Expr::Agg(..)
        | Expr::Col(_) => false,
        Expr::Func(name, _)
            if matches!(
                name.as_str(),
                "now" | "current_timestamp" | "current_date" | "current_time"
            ) =>
        {
            true
        }
        _ => children(e).into_iter().all(is_const),
    }
}

fn like_prefix(p: &str) -> Option<String> {
    let cut = p.find(['%', '_']).unwrap_or(p.len());
    (cut > 0 && cut < p.len() || (cut == p.len() && !p.is_empty())).then(|| p[..cut].to_string())
}

fn analyze(e: &Expr) -> Option<Pred> {
    match e {
        Expr::Bin(a, op @ (Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge), b) => {
            let (col, k, op) = match (&**a, &**b) {
                (
                    Expr::Slot {
                        level: 0,
                        idx,
                        path,
                        ..
                    },
                    k,
                ) if is_const(k) => ((*idx, path.clone()), k.clone(), *op),
                (
                    k,
                    Expr::Slot {
                        level: 0,
                        idx,
                        path,
                        ..
                    },
                ) if is_const(k) => ((*idx, path.clone()), k.clone(), op.flip()),
                _ => return None,
            };
            Some(Pred {
                col: col.0,
                path: col.1,
                op: PredOp::Cmp(op, k),
            })
        }
        Expr::In(a, list, false) => match &**a {
            Expr::Slot {
                level: 0,
                idx,
                path,
                ..
            } if list.iter().all(is_const) => Some(Pred {
                col: *idx,
                path: path.clone(),
                op: PredOp::In(list.clone()),
            }),
            _ => None,
        },
        Expr::Like(a, p, false) => match (&**a, &**p) {
            (
                Expr::Slot {
                    level: 0,
                    idx,
                    path,
                    ..
                },
                Expr::Lit(Value::Text(pat)),
            ) => like_prefix(pat).map(|pre| Pred {
                col: *idx,
                path: path.clone(),
                op: PredOp::Prefix(pre),
            }),
            _ => None,
        },
        _ => None,
    }
}

fn bound(e: &Expr, inclusive: bool) -> Option<Bound> {
    Some(Bound {
        expr: e.clone(),
        inclusive,
    })
}

/// Lower and upper bound from the range predicates on one column (the tightest literal wins).
fn range_of(preds: &[&Pred], col: usize, path: &[String]) -> (Option<Bound>, Option<Bound>) {
    let (mut lo, mut hi): (Option<Bound>, Option<Bound>) = (None, None);
    let tighter = |cur: &Option<Bound>, new: &Expr, lower: bool| -> bool {
        match (cur, new) {
            (None, _) => true,
            (
                Some(Bound {
                    expr: Expr::Lit(a), ..
                }),
                Expr::Lit(b),
            ) => {
                let o = b.total_cmp(a);
                if lower {
                    o == std::cmp::Ordering::Greater
                } else {
                    o == std::cmp::Ordering::Less
                }
            }
            _ => false,
        }
    };
    for p in preds.iter().filter(|p| p.col == col && p.path == path) {
        if let PredOp::Cmp(op, k) = &p.op {
            match op {
                Op::Gt | Op::Ge if tighter(&lo, k, true) => lo = bound(k, *op == Op::Ge),
                Op::Lt | Op::Le if tighter(&hi, k, false) => hi = bound(k, *op == Op::Le),
                _ => {}
            }
        }
    }
    (lo, hi)
}

fn eq_of<'p>(preds: &'p [&Pred], col: usize, path: &[String]) -> Option<&'p Expr> {
    preds.iter().find_map(|p| match &p.op {
        PredOp::Cmp(Op::Eq, k) if p.col == col && p.path == path => Some(k),
        _ => None,
    })
}

fn in_of<'p>(preds: &'p [&Pred], col: usize, path: &[String]) -> Option<&'p Vec<Expr>> {
    preds.iter().find_map(|p| match &p.op {
        PredOp::In(l) if p.col == col && p.path == path => Some(l),
        _ => None,
    })
}

/// A literal whose type the column cannot hold exactly can never use the index.
fn literal_fits(ty: Ty, e: &Expr) -> bool {
    match e {
        Expr::Lit(v) => {
            coerce_type(ty, v.clone()).is_ok_and(|c| !c.is_null() && c.rank() == v.rank())
        }
        _ => true, // checked again at execution time
    }
}

fn table_path(s: &Schema, filters: &[Expr]) -> TablePath {
    let preds: Vec<Pred> = filters
        .iter()
        .filter_map(analyze)
        .filter(|p| p.path.is_empty())
        .collect();
    let preds: Vec<&Pred> = preds.iter().collect();
    let ty = |c: usize| s.cols[c].ty;
    let fits = |c: usize, e: &Expr| literal_fits(ty(c), e);
    let eq = |c: usize| eq_of(&preds, c, &[]).filter(|e| fits(c, e));
    // 1. full primary key equality, or an IN list on a single-column key
    let pk = s.pk_positions();
    if !pk.is_empty() {
        if let Some(vals) = pk
            .iter()
            .map(|c| eq(*c).cloned())
            .collect::<Option<Vec<_>>>()
        {
            return TablePath::PkPoints(vec![vals]);
        }
        if pk.len() == 1 {
            if let Some(list) =
                in_of(&preds, pk[0], &[]).filter(|l| l.iter().all(|e| fits(pk[0], e)))
            {
                return TablePath::PkPoints(list.iter().map(|e| vec![e.clone()]).collect());
            }
        }
    }
    // 2. a unique index fully pinned, then the index with the longest equality prefix
    let mut best: Option<(usize, TablePath)> = None;
    for ix in &s.indexes {
        let cols: Vec<usize> = ix.cols.iter().filter_map(|c| s.find_col(c)).collect();
        if cols.len() != ix.cols.len() {
            continue;
        }
        let mut eqs = Vec::new();
        for c in &cols {
            match eq(*c) {
                Some(e) => eqs.push(e.clone()),
                None => break,
            }
        }
        let k = eqs.len();
        let (lo, hi) = if k < cols.len() {
            let (lo, hi) = range_of(&preds, cols[k], &[]);
            (
                lo.filter(|b| fits(cols[k], &b.expr)),
                hi.filter(|b| fits(cols[k], &b.expr)),
            )
        } else {
            (None, None)
        };
        let score = if k == cols.len() {
            if ix.unique {
                95
            } else {
                80 + k
            }
        } else if k > 0 {
            50 + 2 * k + usize::from(lo.is_some() || hi.is_some())
        } else if lo.is_some() || hi.is_some() {
            40
        } else if let Some(list) =
            in_of(&preds, cols[0], &[]).filter(|l| l.iter().all(|e| fits(cols[0], e)))
        {
            if best.as_ref().is_none_or(|b| b.0 < 45) {
                best = Some((
                    45,
                    TablePath::IndexIn {
                        ix: ix.clone(),
                        vals: list.clone(),
                    },
                ));
            }
            continue;
        } else {
            continue;
        };
        if best.as_ref().is_none_or(|b| b.0 < score) {
            best = Some((
                score,
                TablePath::Index {
                    ix: ix.clone(),
                    eq: eqs,
                    lo,
                    hi,
                },
            ));
        }
    }
    // 3. primary key prefix / range
    if !pk.is_empty() {
        let mut eqs = Vec::new();
        for c in &pk {
            match eq(*c) {
                Some(e) => eqs.push(e.clone()),
                None => break,
            }
        }
        let k = eqs.len();
        if k < pk.len() {
            let (lo, hi) = range_of(&preds, pk[k], &[]);
            let (lo, hi) = (
                lo.filter(|b| fits(pk[k], &b.expr)),
                hi.filter(|b| fits(pk[k], &b.expr)),
            );
            let score = if k > 0 {
                50 + 2 * k + usize::from(lo.is_some() || hi.is_some())
            } else {
                41
            };
            if (k > 0 || lo.is_some() || hi.is_some()) && best.as_ref().is_none_or(|b| b.0 < score)
            {
                // a range on a single-column key must be over integers or text (the key encoding)
                let ok = pk.len() > 1 || matches!(ty(pk[0]), Ty::Int | Ty::Text);
                if ok {
                    best = Some((score, TablePath::PkRange { eq: eqs, lo, hi }));
                }
            }
        }
    }
    best.map_or(TablePath::FullScan, |b| b.1)
}

fn doc_path(ctx: &Ctx, coll: &str, filters: &[Expr]) -> Result<DocPath> {
    let preds: Vec<Pred> = filters
        .iter()
        .filter_map(analyze)
        .filter(|p| !p.path.is_empty())
        .collect();
    let preds: Vec<&Pred> = preds.iter().collect();
    let id = vec!["_id".to_string()];
    let id_ok =
        |e: &Expr| !matches!(e, Expr::Lit(v) if !matches!(v, Value::Int(_) | Value::Text(_)));
    if let Some(e) = eq_of(&preds, 0, &id).filter(|e| id_ok(e)) {
        return Ok(DocPath::Ids(vec![e.clone()]));
    }
    if let Some(l) = in_of(&preds, 0, &id).filter(|l| l.iter().all(id_ok)) {
        return Ok(DocPath::Ids(l.clone()));
    }
    // SQL comparisons with NULL never hold, and arrays/objects compare as a whole
    let scalar = |_: &crate::docindex::DocIndex, e: &Expr| {
        !matches!(
            e,
            Expr::Lit(Value::Null | Value::Array(_) | Value::Object(_))
        )
    };
    let indexes = crate::doc::indexes(ctx, coll)?;
    let split = |p: &str| -> Vec<String> { p.split('.').map(String::from).collect() };
    let eq = |p: &str| eq_of(&preds, 0, &split(p)).cloned();
    let range = |p: &str| {
        let (lo, hi) = range_of(&preds, 0, &split(p));
        (
            lo.map(|b| (b.expr, b.inclusive)),
            hi.map(|b| (b.expr, b.inclusive)),
        )
    };
    Ok(
        match crate::docindex::choose(&indexes, &eq, &range, &scalar) {
            Some(c) => DocPath::Index(c),
            None => DocPath::FullScan,
        },
    )
}

fn kv_path(filters: &[Expr]) -> KvPath {
    let preds: Vec<Pred> = filters
        .iter()
        .filter_map(analyze)
        .filter(|p| p.col == 0 && p.path.is_empty())
        .collect();
    let preds: Vec<&Pred> = preds.iter().collect();
    if let Some(e) = eq_of(&preds, 0, &[]) {
        return KvPath::Keys(vec![e.clone()]);
    }
    if let Some(l) = in_of(&preds, 0, &[]) {
        return KvPath::Keys(l.clone());
    }
    if let Some(p) = preds.iter().find_map(|p| match &p.op {
        PredOp::Prefix(s) => Some(s.clone()),
        _ => None,
    }) {
        return KvPath::Prefix(p);
    }
    let (lo, hi) = range_of(&preds, 0, &[]);
    if lo.is_some() || hi.is_some() {
        return KvPath::Range { lo, hi };
    }
    KvPath::FullScan
}

pub struct PhysPlanner<'c, 'a> {
    ctx: &'c Ctx<'a>,
    next_id: usize,
}

fn equi_keys(on: &[Expr], left_width: usize) -> (Vec<Expr>, Vec<Expr>) {
    let (mut lk, mut rk) = (Vec::new(), Vec::new());
    for c in on {
        if let Expr::Bin(a, Op::Eq, b) = c {
            let side = |e: &Expr| -> Option<bool> {
                let set = refs(e, &[])?;
                if set.is_empty() || !is_free_of_outer_subs(e) {
                    return None;
                }
                if set.iter().all(|s| *s < left_width) {
                    Some(true)
                } else if set.iter().all(|s| *s >= left_width) {
                    Some(false)
                } else {
                    None
                }
            };
            match (side(a), side(b)) {
                (Some(true), Some(false)) => {
                    lk.push((**a).clone());
                    rk.push(shift_slots((**b).clone(), -(left_width as isize)));
                }
                (Some(false), Some(true)) => {
                    lk.push((**b).clone());
                    rk.push(shift_slots((**a).clone(), -(left_width as isize)));
                }
                _ => {}
            }
        }
    }
    (lk, rk)
}

fn is_free_of_outer_subs(e: &Expr) -> bool {
    !matches!(
        e,
        Expr::Exists(..) | Expr::ScalarQuery(_) | Expr::InQuery(..)
    ) && children(e).into_iter().all(is_free_of_outer_subs)
}

impl<'c, 'a> PhysPlanner<'c, 'a> {
    pub fn new(ctx: &'c Ctx<'a>) -> Self {
        PhysPlanner { ctx, next_id: 0 }
    }

    fn node(&mut self, op: PhysOp) -> Node {
        self.next_id += 1;
        Node {
            id: self.next_id - 1,
            op,
        }
    }

    fn access(&self, src: &SourceKind, filters: &[Expr]) -> Result<Access> {
        Ok(match src {
            SourceKind::Table(s) => Access::Table {
                schema: s.clone(),
                path: table_path(s, filters),
            },
            SourceKind::Collection(c) => Access::Doc {
                coll: c.clone(),
                path: doc_path(self.ctx, c, filters)?,
            },
            SourceKind::Kv(ns) => Access::Kv {
                ns: ns.clone(),
                path: kv_path(filters),
            },
        })
    }

    pub fn plan(&mut self, l: Logical) -> Result<Node> {
        self.plan_limited(l, None)
    }

    /// `limit`: the parent only needs this many rows from a streaming input.
    fn plan_limited(&mut self, l: Logical, limit: Option<usize>) -> Result<Node> {
        Ok(match l {
            Logical::Unit => self.node(PhysOp::Unit),
            Logical::Scan { src, alias, filter } => {
                let access = self.access(&src, &filter)?;
                let filter = and_all(filter);
                self.node(PhysOp::Scan {
                    access,
                    alias,
                    filter,
                    limit,
                    reverse: false,
                })
            }
            Logical::Derived { input, alias } => {
                let input = Box::new(self.plan(*input)?);
                self.node(PhysOp::Derived { input, alias })
            }
            Logical::Filter { input, pred } => {
                let input = Box::new(self.plan(*input)?);
                self.node(PhysOp::Filter { input, pred })
            }
            Logical::Join {
                left,
                right,
                kind,
                on,
                right_width,
            } => {
                let left_width = width_of(&left);
                let left = Box::new(self.plan(*left)?);
                // index lookup join: the right side is a base scan reachable by key from the left row
                let lookup_ok = matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Cross);
                if let (true, Logical::Scan { src, alias, filter }) = (lookup_ok, &*right) {
                    let (lk, rk) = equi_keys(&on, left_width);
                    let mut lookup: Vec<Expr> = filter
                        .iter()
                        .cloned()
                        .map(|e| shift_levels(e, 1, 1))
                        .collect();
                    for (l, r) in lk.iter().zip(&rk) {
                        // move the left expression one row-context deeper: it is read from the outer stack
                        let probe = shift_levels(l.clone(), 0, 1);
                        lookup.push(Expr::Bin(Box::new(r.clone()), Op::Eq, Box::new(probe)));
                    }
                    if !lk.is_empty() {
                        let access = self.access(src, &lookup)?;
                        let keyed = match &access {
                            Access::Table { path, .. } => !matches!(path, TablePath::FullScan),
                            Access::Doc { path, .. } => !matches!(path, DocPath::FullScan),
                            Access::Kv { path, .. } => matches!(path, KvPath::Keys(_)),
                        };
                        if keyed && uses_outer(&access) {
                            let rfilter = and_all(
                                filter
                                    .iter()
                                    .cloned()
                                    .map(|e| shift_levels(e, 1, 1))
                                    .collect(),
                            );
                            let right = Box::new(self.node(PhysOp::Scan {
                                access,
                                alias: alias.clone(),
                                filter: rfilter,
                                limit: None,
                                reverse: false,
                            }));
                            return Ok(self.node(PhysOp::LookupJoin {
                                left,
                                right,
                                on: and_all(on),
                                kind,
                                right_width,
                            }));
                        }
                    }
                }
                let (lk, rk) = equi_keys(&on, left_width);
                let right = Box::new(self.plan(*right)?);
                if !lk.is_empty() {
                    self.node(PhysOp::HashJoin {
                        left,
                        right,
                        lkeys: lk,
                        rkeys: rk,
                        on: and_all(on),
                        kind,
                        right_width,
                    })
                } else {
                    self.node(PhysOp::NestedLoopJoin {
                        left,
                        right,
                        on: and_all(on),
                        kind,
                        right_width,
                    })
                }
            }
            Logical::Aggregate {
                input,
                group,
                aggs,
                width,
            } => {
                let input = Box::new(self.plan(*input)?);
                self.node(PhysOp::HashAggregate {
                    input,
                    group,
                    aggs,
                    width,
                })
            }
            Logical::Project { input, exprs } => {
                // projection streams: a limit above it can be pushed below it
                let input = Box::new(self.plan_limited(*input, limit)?);
                self.node(PhysOp::Project { input, exprs })
            }
            Logical::Distinct { input } => {
                let input = Box::new(self.plan(*input)?);
                self.node(PhysOp::Distinct { input })
            }
            Logical::Sort { input, keys } => {
                let mut input = self.plan(*input)?;
                // the access path may already produce this order: no sort needed
                if let PhysOp::Scan {
                    access,
                    reverse,
                    limit: scan_limit,
                    ..
                } = &mut input.op
                {
                    if let Some(rev) = natural_order(access, &keys) {
                        *reverse = rev;
                        *scan_limit = limit;
                        return Ok(input);
                    }
                }
                let input = Box::new(input);
                self.node(PhysOp::Sort { input, keys })
            }
            Logical::Limit {
                input,
                limit: n,
                offset,
            } => {
                let want = n.map(|n| n.saturating_add(offset));
                let input = Box::new(self.plan_limited(*input, want)?);
                self.node(PhysOp::Limit {
                    input,
                    limit: n,
                    offset,
                })
            }
            Logical::Union { left, right, all } => {
                let left = Box::new(self.plan(*left)?);
                let right = Box::new(self.plan(*right)?);
                self.node(PhysOp::Union { left, right, all })
            }
        })
    }
}

/// If reading `access` forwards (Some(false)) or backwards (Some(true))
/// yields rows ordered by `keys`, say which; None if a sort is needed.
fn natural_order(access: &Access, keys: &[(Expr, bool)]) -> Option<bool> {
    let mut cols: Vec<(usize, Vec<String>)> = Vec::new();
    for (k, _) in keys {
        match k {
            Expr::Slot {
                level: 0,
                idx,
                path,
                ..
            } => cols.push((*idx, path.clone())),
            _ => return None,
        }
    }
    // the order an access path produces: (column, path, stored descending), and whether it can run backwards
    let (order, reversible): (Vec<(usize, Vec<String>, bool)>, bool) = match access {
        Access::Table { schema, path } => {
            let pk = schema.pk_positions();
            let by = |cs: &[usize]| cs.iter().map(|c| (*c, vec![], false)).collect::<Vec<_>>();
            match path {
                TablePath::FullScan => (by(&pk), true),
                TablePath::PkRange { eq, .. } => (by(&pk[eq.len()..]), true),
                TablePath::PkPoints(_) => (by(&pk), false),
                TablePath::Index { ix, eq, .. } => {
                    if ix.enc != crate::rel::Enc::Exact {
                        return None; // the original encoding merges large integers
                    }
                    let cs: Vec<usize> =
                        ix.cols.iter().filter_map(|c| schema.find_col(c)).collect();
                    if cs.len() != ix.cols.len() {
                        return None;
                    }
                    (by(&cs[eq.len()..]), true)
                }
                TablePath::IndexIn { .. } => return None,
            }
        }
        Access::Doc { path, .. } => match path {
            DocPath::FullScan => (vec![(0, vec!["_id".to_string()], false)], true),
            DocPath::Ids(_) => (vec![(0, vec!["_id".to_string()], false)], false),
            DocPath::Index(c) => {
                if c.index.legacy || c.index.multikey {
                    return None;
                }
                let o = c.index.fields[c.eq.len()..]
                    .iter()
                    .map(|(p, d)| (0, p.split('.').map(String::from).collect(), *d < 0))
                    .collect();
                (o, true)
            }
        },
        Access::Kv { path, .. } => match path {
            KvPath::Keys(_) => (vec![(0, vec![], false)], false),
            _ => (vec![(0, vec![], false)], true),
        },
    };
    if cols.len() > order.len() || cols.is_empty() {
        return None;
    }
    let mut rev = None;
    for ((c, p), ((oc, op, stored_desc), (_, desc))) in cols.iter().zip(order.iter().zip(keys)) {
        if c != oc || p != op {
            return None;
        }
        let r = stored_desc != desc;
        if rev.is_some_and(|x| x != r) {
            return None;
        }
        rev = Some(r);
    }
    let rev = rev?;
    (!rev || reversible).then_some(rev)
}

fn uses_outer(a: &Access) -> bool {
    let has = |e: &Expr| -> bool {
        fn walk(e: &Expr) -> bool {
            matches!(e, Expr::Slot { level, .. } if *level >= 1)
                || children(e).into_iter().any(walk)
        }
        walk(e)
    };
    let b = |x: &Option<Bound>| x.as_ref().is_some_and(|b| has(&b.expr));
    match a {
        Access::Table { path, .. } => match path {
            TablePath::FullScan => false,
            TablePath::PkPoints(v) => v.iter().flatten().any(has),
            TablePath::PkRange { eq, lo, hi } | TablePath::Index { eq, lo, hi, .. } => {
                eq.iter().any(has) || b(lo) || b(hi)
            }
            TablePath::IndexIn { vals, .. } => vals.iter().any(has),
        },
        Access::Doc { path, .. } => match path {
            DocPath::FullScan => false,
            DocPath::Ids(v) => v.iter().any(has),
            DocPath::Index(c) => {
                c.eq.iter().any(has)
                    || c.lo.as_ref().is_some_and(|x| has(&x.0))
                    || c.hi.as_ref().is_some_and(|x| has(&x.0))
            }
        },
        Access::Kv { path, .. } => match path {
            KvPath::Keys(v) => v.iter().any(has),
            _ => false,
        },
    }
}

/// Number of columns a logical plan produces.
pub fn width_of(l: &Logical) -> usize {
    match l {
        Logical::Unit => 0,
        Logical::Scan { src, alias, .. } => src.scope(alias).len(),
        Logical::Derived { input, .. }
        | Logical::Filter { input, .. }
        | Logical::Distinct { input }
        | Logical::Sort { input, .. }
        | Logical::Limit { input, .. } => width_of(input),
        Logical::Join {
            left, right_width, ..
        } => width_of(left) + right_width,
        Logical::Aggregate { width, aggs, .. } => width + aggs.len(),
        Logical::Project { exprs, .. } => exprs.len(),
        Logical::Union { left, .. } => width_of(left),
    }
}

impl<'c, 'a> Planner<'c, 'a> {
    /// Bind and plan a query all the way to a physical plan.
    pub fn plan_query(ctx: &'c Ctx<'a>, q: Query, outer: Vec<Scope>) -> Result<Plan> {
        let mut b = Planner::new(ctx);
        b.outer = outer;
        let (logical, columns) = b.query(q)?;
        let docs_output = b.docs_flag;
        let mut types = b.top_types.take().unwrap_or_default();
        types.resize(columns.len(), None);
        let mut pp = PhysPlanner::new(ctx);
        let root = pp.plan(logical)?;
        let mut subs = Vec::new();
        for (l, correlated) in std::mem::take(&mut b.subs) {
            subs.push(SubPlan {
                root: pp.plan(l)?,
                correlated,
            });
        }
        Ok(Plan {
            root,
            subs,
            columns,
            types,
            docs_output,
            node_count: pp.next_id,
        })
    }

    /// Bind an expression against a single source (UPDATE / DELETE), returning
    /// any subqueries it planned.
    pub fn bind_single(
        ctx: &'c Ctx<'a>,
        scope: &Scope,
        exprs: Vec<Expr>,
    ) -> Result<(Vec<Expr>, Vec<SubPlan>, usize)> {
        let mut b = Planner::new(ctx);
        let bound = exprs
            .into_iter()
            .map(|e| b.bind(e, scope))
            .collect::<Result<Vec<_>>>()?;
        let mut pp = PhysPlanner::new(ctx);
        let mut subs = Vec::new();
        for (l, correlated) in std::mem::take(&mut b.subs) {
            subs.push(SubPlan {
                root: pp.plan(l)?,
                correlated,
            });
        }
        Ok((bound, subs, pp.next_id))
    }

    /// Physical access for a single source with filters (bound to its scope),
    /// used by UPDATE / DELETE to find their target rows through indexes.
    pub fn access_for(ctx: &'c Ctx<'a>, src: &SourceKind, filters: &[Expr]) -> Result<Access> {
        let usable: Vec<Expr> = filters
            .iter()
            .filter(|f| refs(f, &[]).is_some())
            .cloned()
            .collect();
        PhysPlanner::new(ctx).access(src, &usable)
    }
}

pub fn split_conjuncts(e: Option<Expr>) -> Vec<Expr> {
    let mut out = Vec::new();
    if let Some(e) = e {
        conjuncts(e, &mut out);
    }
    out
}

pub fn source_scope(src: &SourceKind, alias: &str) -> Scope {
    src.scope(alias)
}

/// Fold a constant expression (e.g. a column DEFAULT) to a value.
pub fn constant(e: &Expr) -> Result<Value> {
    eval_const(e)
}

pub fn table_schema(ctx: &Ctx, name: &str) -> Result<Schema> {
    rel::Schema::load(ctx, name)
}
