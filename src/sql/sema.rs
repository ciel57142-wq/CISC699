//! Semantic analysis and static type checking of relational statements.
//!
//! Runs after parsing and before anything is executed, so a statement that
//! cannot be valid fails as a whole, with a message naming the offending
//! expression, even when no row would ever reach it (an empty table, a WHERE
//! that matches nothing). It checks:
//!
//! * names: every table exists (when a catalog is available), every column
//!   resolves – in its own query or an enclosing one – and an unqualified
//!   name is not ambiguous;
//! * types: arithmetic and unary minus need numbers, AND / OR / NOT and
//!   every condition (WHERE, ON, HAVING, CASE WHEN, CHECK) need booleans,
//!   LIKE needs text, function arguments have the right kind (comparisons
//!   are defined between any two values, by the cross-type total order);
//! * functions: the name is known and the argument count is right;
//! * aggregates: not in WHERE, ON, GROUP BY, UPDATE or DELETE; not nested;
//! * shapes: a scalar or IN subquery returns one column, both sides of a
//!   set operation have the same width, VALUES rows have the right width.
//!
//! NULL is compatible with every type. Document fields and derived-table
//! columns are dynamically typed (`Any`): they pass the static checks and
//! are checked again by the evaluator at run time.

use super::ast::*;
use super::catalog::Schema;
use super::printer;
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;

/// A static type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlType {
    Null,
    Bool,
    Int,
    Float,
    Text,
    /// An array or object (document values).
    Json,
    /// Not known until run time.
    Any,
}

impl SqlType {
    pub(crate) fn name(self) -> &'static str {
        match self {
            SqlType::Null => "NULL",
            SqlType::Bool => "BOOL",
            SqlType::Int => "INT",
            SqlType::Float => "REAL",
            SqlType::Text => "TEXT",
            SqlType::Json => "JSON",
            SqlType::Any => "ANY",
        }
    }

    fn of_value(v: &Value) -> SqlType {
        match v {
            Value::Null => SqlType::Null,
            Value::Bool(_) => SqlType::Bool,
            Value::Int(_) => SqlType::Int,
            Value::Float(_) => SqlType::Float,
            Value::Text(_) => SqlType::Text,
            Value::Array(_) | Value::Object(_) => SqlType::Json,
        }
    }

    fn of_column(t: Ty) -> SqlType {
        match t {
            Ty::Int => SqlType::Int,
            Ty::Float => SqlType::Float,
            Ty::Text => SqlType::Text,
            Ty::Bool => SqlType::Bool,
        }
    }

    fn numeric(self) -> bool {
        matches!(self, SqlType::Int | SqlType::Float | SqlType::Null | SqlType::Any)
    }

    fn boolean(self) -> bool {
        matches!(self, SqlType::Bool | SqlType::Null | SqlType::Any)
    }

    fn textual(self) -> bool {
        matches!(self, SqlType::Text | SqlType::Null | SqlType::Any)
    }

    /// The type of a value that may come from either branch.
    fn unify(self, other: SqlType) -> SqlType {
        match (self, other) {
            (a, b) if a == b => a,
            (SqlType::Null, x) | (x, SqlType::Null) => x,
            (SqlType::Int, SqlType::Float) | (SqlType::Float, SqlType::Int) => SqlType::Float,
            _ => SqlType::Any,
        }
    }
}

fn type_err<T>(msg: String) -> Result<T> {
    Err(Error::Type(msg))
}

/// Columns visible from one FROM item.
#[derive(Clone)]
struct Rel {
    alias: String,
    /// None: any column name is accepted (documents, derived tables, tables
    /// when no catalog is available).
    cols: Option<Vec<(String, SqlType)>>,
}

/// The names visible at one query level.
#[derive(Clone, Default)]
struct Level {
    rels: Vec<Rel>,
    /// Output columns of the SELECT, usable by name in GROUP BY / ORDER BY.
    outputs: Vec<(String, SqlType)>,
}

/// Where an expression appears, for the aggregate rules.
#[derive(Clone, Copy, PartialEq)]
enum Clause {
    Select,
    Where,
    On,
    GroupBy,
    Having,
    OrderBy,
    Values,
    Set,
    Check,
}

impl Clause {
    fn aggregates_allowed(self) -> bool {
        matches!(self, Clause::Select | Clause::Having | Clause::OrderBy)
    }
    fn name(self) -> &'static str {
        match self {
            Clause::Select => "the select list",
            Clause::Where => "WHERE",
            Clause::On => "a JOIN condition",
            Clause::GroupBy => "GROUP BY",
            Clause::Having => "HAVING",
            Clause::OrderBy => "ORDER BY",
            Clause::Values => "VALUES",
            Clause::Set => "UPDATE ... SET",
            Clause::Check => "a CHECK constraint",
        }
    }
}

/// Argument kinds a function accepts.
#[derive(Clone, Copy)]
enum Arg {
    Any,
    Text,
    Num,
}

/// (name, min args, max args (None = any), argument kind, result type)
const FUNCTIONS: &[(&str, usize, Option<usize>, Arg, SqlType)] = &[
    ("version", 0, Some(0), Arg::Any, SqlType::Text),
    ("coalesce", 1, None, Arg::Any, SqlType::Any),
    ("ifnull", 2, Some(2), Arg::Any, SqlType::Any),
    ("nullif", 2, Some(2), Arg::Any, SqlType::Any),
    ("greatest", 1, None, Arg::Any, SqlType::Any),
    ("least", 1, None, Arg::Any, SqlType::Any),
    ("lower", 1, Some(1), Arg::Text, SqlType::Text),
    ("upper", 1, Some(1), Arg::Text, SqlType::Text),
    ("trim", 1, Some(1), Arg::Text, SqlType::Text),
    ("ltrim", 1, Some(1), Arg::Text, SqlType::Text),
    ("rtrim", 1, Some(1), Arg::Text, SqlType::Text),
    ("length", 1, Some(1), Arg::Text, SqlType::Int),
    ("substr", 2, Some(3), Arg::Any, SqlType::Text),
    ("substring", 2, Some(3), Arg::Any, SqlType::Text),
    ("replace", 3, Some(3), Arg::Text, SqlType::Text),
    ("concat", 0, None, Arg::Any, SqlType::Text),
    ("abs", 1, Some(1), Arg::Num, SqlType::Any),
    ("round", 1, Some(2), Arg::Num, SqlType::Any),
    ("floor", 1, Some(1), Arg::Num, SqlType::Any),
    ("ceil", 1, Some(1), Arg::Num, SqlType::Any),
    ("ceiling", 1, Some(1), Arg::Num, SqlType::Any),
    ("typeof", 1, Some(1), Arg::Any, SqlType::Text),
    ("get_path", 2, Some(2), Arg::Any, SqlType::Any),
    ("json", 1, Some(1), Arg::Any, SqlType::Any),
];

pub(crate) struct Analyzer<'c, 'a> {
    /// The catalog, when the statement is checked against a database.
    ctx: Option<&'c Ctx<'a>>,
}

impl<'c, 'a> Analyzer<'c, 'a> {
    pub(crate) fn new(ctx: Option<&'c Ctx<'a>>) -> Self {
        Analyzer { ctx }
    }

    /// Check one statement.
    pub(crate) fn statement(&self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Query(q) | Stmt::Explain { query: q, .. } => self.query(q, &[]).map(|_| ()),
            Stmt::Insert { table, cols, source } => self.insert(table, cols.as_deref(), source),
            Stmt::Update { table, sets, filter } => {
                let rel = self.table_rel(table, table)?;
                let mut seen = Vec::new();
                for (c, _) in sets {
                    if seen.contains(&c) {
                        return Err(Error::Schema(format!("column '{c}' is assigned twice")));
                    }
                    seen.push(c);
                    if let Some(cols) = &rel.cols {
                        if !cols.iter().any(|(n, _)| n == c) {
                            return Err(Error::Schema(format!("no such column '{c}' in table '{table}'")));
                        }
                    }
                }
                let levels = [Level { rels: vec![rel], outputs: vec![] }];
                for (_, e) in sets {
                    if let Some(e) = e {
                        self.expr(e, &levels, Clause::Set)?;
                    }
                }
                self.condition(filter.as_ref(), &levels, Clause::Where)
            }
            Stmt::Delete { table, filter } => {
                let levels = [Level { rels: vec![self.table_rel(table, table)?], outputs: vec![] }];
                self.condition(filter.as_ref(), &levels, Clause::Where)
            }
            Stmt::CreateTable(def) => {
                let mut cols: Vec<(String, SqlType)> = Vec::new();
                for cd in &def.cols {
                    if cols.iter().any(|(n, _)| *n == cd.col.name) {
                        return Err(Error::Schema(format!("column '{}' is defined twice", cd.col.name)));
                    }
                    cols.push((cd.col.name.clone(), SqlType::of_column(cd.col.ty)));
                }
                let levels = [Level { rels: vec![Rel { alias: def.name.clone(), cols: Some(cols) }], outputs: vec![] }];
                for c in &def.constraints {
                    if let Constraint::Check(_, e) = c {
                        self.condition(Some(e), &levels, Clause::Check)?;
                    }
                }
                for cd in &def.cols {
                    if let Some(d) = &cd.default {
                        self.expr(d, &[], Clause::Values)?;
                    }
                }
                Ok(())
            }
            Stmt::CreateIndex { cols, .. } => {
                for (i, c) in cols.iter().enumerate() {
                    if cols[..i].contains(c) {
                        return Err(Error::Schema(format!("column '{c}' appears twice in the index")));
                    }
                }
                Ok(())
            }
            Stmt::AlterTable { table, actions } => {
                for a in actions {
                    match a {
                        Alter::AddConstraint(Constraint::Check(_, e)) => {
                            let levels = [Level { rels: vec![self.table_rel(table, table)?], outputs: vec![] }];
                            self.condition(Some(e), &levels, Clause::Check)?;
                        }
                        Alter::SetDefault(_, Some(e)) => self.expr(e, &[], Clause::Values).map(|_| ())?,
                        _ => {}
                    }
                }
                Ok(())
            }
            Stmt::DropTable { .. }
            | Stmt::DropIndex { .. }
            | Stmt::Truncate(_)
            | Stmt::Analyze(_)
            | Stmt::Describe(_)
            | Stmt::CheckTable(_)
            | Stmt::Reindex(_) => Ok(()),
        }
    }

    fn insert(&self, table: &str, cols: Option<&[String]>, source: &InsertSource) -> Result<()> {
        let schema = match self.ctx {
            Some(ctx) => Some(Schema::load(ctx, table)?),
            None => None,
        };
        if let (Some(names), Some(s)) = (cols, &schema) {
            s.positions(names)?;
        }
        let width = cols.map(|c| c.len()).or_else(|| schema.as_ref().map(|s| s.cols.len()));
        match source {
            InsertSource::DefaultValues => Ok(()),
            InsertSource::Values(rows) => {
                for row in rows {
                    if let Some(w) = width {
                        if row.len() != w {
                            return Err(Error::Parse(format!("expected {w} value(s) per row, got {}", row.len())));
                        }
                    } else if row.len() != rows[0].len() {
                        return Err(Error::Parse("every VALUES row must have the same number of values".into()));
                    }
                    for e in row.iter().flatten() {
                        self.expr(e, &[], Clause::Values)?;
                    }
                }
                Ok(())
            }
            InsertSource::Query(q) => {
                let n = self.query(q, &[])?;
                match (n, width) {
                    (Some(n), Some(w)) if n.len() != w => Err(Error::Parse(format!(
                        "INSERT ... SELECT: the query returns {} column(s), {w} expected",
                        n.len()
                    ))),
                    _ => Ok(()),
                }
            }
        }
    }

    /// The columns of a table, from the catalog when there is one.
    fn table_rel(&self, table: &str, alias: &str) -> Result<Rel> {
        let cols = match self.ctx {
            Some(ctx) => {
                let s = Schema::load(ctx, table)?;
                Some(s.cols.iter().map(|c| (c.name.clone(), SqlType::of_column(c.ty))).collect())
            }
            None => None,
        };
        Ok(Rel { alias: alias.to_string(), cols })
    }

    fn source(&self, item: &FromItem, outer: &[Level]) -> Result<Rel> {
        Ok(match &item.src {
            Source::Table(t) => self.table_rel(t, &item.alias)?,
            Source::Collection(c) => {
                if !keys::valid_name(c) {
                    return Err(Error::Schema(format!("invalid collection name '{c}'")));
                }
                Rel { alias: item.alias.clone(), cols: None }
            }
            Source::Kv(_) => Rel {
                alias: item.alias.clone(),
                cols: Some(vec![("key".into(), SqlType::Text), ("value".into(), SqlType::Text)]),
            },
            Source::Derived(q) => {
                let cols = self.query(q, outer)?;
                Rel { alias: item.alias.clone(), cols }
            }
        })
    }

    /// Check a query; returns its output columns when they are known.
    fn query(&self, q: &Query, outer: &[Level]) -> Result<Option<Vec<(String, SqlType)>>> {
        let head = self.select(&q.head, outer)?;
        for (_, _, s) in &q.rest {
            let other = self.select(s, outer)?;
            if let (Some(a), Some(b)) = (&head, &other) {
                if a.len() != b.len() {
                    return Err(Error::Schema(format!(
                        "each side of a set operation must have the same number of columns ({} vs {})",
                        a.len(),
                        b.len()
                    )));
                }
            }
        }
        Ok(head)
    }

    fn select(&self, s: &Select, outer: &[Level]) -> Result<Option<Vec<(String, SqlType)>>> {
        let mut level = Level::default();
        if let Some(f) = &s.from {
            level.rels.push(self.source(f, outer)?);
        }
        let mut levels: Vec<Level> = outer.to_vec();
        for j in &s.joins {
            let rel = self.source(&j.item, outer)?;
            if level.rels.iter().any(|r| r.alias == rel.alias) {
                return Err(Error::Schema(format!("table name '{}' used twice; give it an alias", rel.alias)));
            }
            level.rels.push(rel);
            if let Some(on) = &j.on {
                levels.push(level.clone());
                self.condition(Some(on), &levels, Clause::On)?;
                levels.pop();
            }
        }
        levels.push(level);
        self.condition(s.filter.as_ref(), &levels, Clause::Where)?;

        // The select list; `None` when a star over an open relation hides the width.
        let mut outputs: Option<Vec<(String, SqlType)>> = Some(Vec::new());
        for it in &s.items {
            match it {
                Item::Star | Item::TableStar(_) => {
                    let me = levels.last().expect("current level");
                    if matches!(it, Item::Star) && me.rels.is_empty() {
                        return Err(Error::Parse("SELECT * needs a FROM clause".into()));
                    }
                    let rels: Vec<&Rel> = match it {
                        Item::TableStar(t) => {
                            let r: Vec<&Rel> = me.rels.iter().filter(|r| &r.alias == t).collect();
                            if r.is_empty() {
                                return Err(Error::Schema(format!("no table '{t}' in this query")));
                            }
                            r
                        }
                        _ => me.rels.iter().collect(),
                    };
                    for r in rels {
                        match (&mut outputs, &r.cols) {
                            (Some(out), Some(cols)) => out.extend(cols.iter().cloned()),
                            _ => outputs = None,
                        }
                    }
                }
                Item::Expr(e, alias) => {
                    let t = self.expr(e, &levels, Clause::Select)?;
                    if let Some(out) = &mut outputs {
                        out.push((alias.clone().unwrap_or_else(|| super::eval::label(e)), t));
                    }
                }
            }
        }
        if let Some(out) = &outputs {
            levels.last_mut().expect("current level").outputs = out.clone();
        }
        for g in &s.group_by {
            self.by_expr(g, &levels, Clause::GroupBy)?;
        }
        self.condition(s.having.as_ref(), &levels, Clause::Having)?;
        for o in &s.order_by {
            self.by_expr(&o.expr, &levels, Clause::OrderBy)?;
        }
        Ok(outputs)
    }

    /// GROUP BY / ORDER BY may also name an output column or its position.
    fn by_expr(&self, e: &Expr, levels: &[Level], clause: Clause) -> Result<SqlType> {
        let me = levels.last().expect("current level");
        match e {
            Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= me.outputs.len() => {
                Ok(me.outputs[*k as usize - 1].1)
            }
            Expr::Col(None, n) if self.resolve(&None, n, levels).is_err() => match me.outputs.iter().find(|(o, _)| o == n) {
                Some((_, t)) => Ok(*t),
                None => self.expr(e, levels, clause),
            },
            _ => self.expr(e, levels, clause),
        }
    }

    /// A condition: boolean-typed, aggregates only where allowed.
    fn condition(&self, e: Option<&Expr>, levels: &[Level], clause: Clause) -> Result<()> {
        let Some(e) = e else { return Ok(()) };
        let t = self.expr(e, levels, clause)?;
        if !t.boolean() {
            return type_err(format!(
                "{} must be a boolean condition, but `{}` is {}",
                clause.name(),
                printer::expr(e),
                t.name()
            ));
        }
        Ok(())
    }

    /// Resolve a column, innermost query level first.
    fn resolve(&self, q: &Option<String>, n: &str, levels: &[Level]) -> Result<SqlType> {
        for level in levels.iter().rev() {
            let mut found: Option<SqlType> = None;
            let mut open = false;
            for r in &level.rels {
                if q.as_ref().map_or(false, |q| q != &r.alias) {
                    continue;
                }
                match &r.cols {
                    Some(cols) => {
                        if let Some((_, t)) = cols.iter().find(|(c, _)| c == n) {
                            if found.is_some() {
                                return Err(Error::Schema(format!(
                                    "column '{n}' is ambiguous; qualify it with a table name"
                                )));
                            }
                            found = Some(*t);
                        }
                    }
                    None => open = true,
                }
            }
            if let Some(t) = found {
                return Ok(t);
            }
            if open {
                // an open relation (documents, derived tables) may have it
                return Ok(SqlType::Any);
            }
        }
        Err(match q {
            Some(q) => Error::Schema(format!("no such column '{q}.{n}'")),
            None => Error::Schema(format!("no such column '{n}'")),
        })
    }

    /// Infer the type of an expression, checking it on the way.
    fn expr(&self, e: &Expr, levels: &[Level], clause: Clause) -> Result<SqlType> {
        let show = || printer::expr(e);
        Ok(match e {
            Expr::Lit(v) => SqlType::of_value(v),
            Expr::Col(q, n) => self.resolve(q, n, levels)?,
            Expr::Bin(a, op, b) => {
                let (l, r) = (self.expr(a, levels, clause)?, self.expr(b, levels, clause)?);
                match op {
                    Op::And | Op::Or => {
                        for (side, t) in [(a, l), (b, r)] {
                            if !t.boolean() {
                                return type_err(format!(
                                    "{} needs boolean operands, but `{}` is {} in `{}`",
                                    op.text().to_uppercase(),
                                    printer::expr(side),
                                    t.name(),
                                    show()
                                ));
                            }
                        }
                        SqlType::Bool
                    }
                    Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
                        if !l.numeric() || !r.numeric() {
                            return type_err(format!(
                                "cannot apply '{}' to {} and {} in `{}`",
                                op.text(),
                                l.name(),
                                r.name(),
                                show()
                            ));
                        }
                        match (l, r) {
                            (SqlType::Null, _) | (_, SqlType::Null) => SqlType::Null,
                            (SqlType::Int, SqlType::Int) => SqlType::Int,
                            (SqlType::Any, _) | (_, SqlType::Any) => SqlType::Any,
                            _ => SqlType::Float,
                        }
                    }
                    Op::Concat => SqlType::Text,
                    // Comparisons are defined between any two values: values of
                    // different types follow the total order
                    // null < bool < number < text < array < object (docs/PBSQL.md).
                    Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => SqlType::Bool,
                }
            }
            Expr::Not(a) => {
                let t = self.expr(a, levels, clause)?;
                if !t.boolean() {
                    return type_err(format!("NOT needs a boolean, but `{}` is {}", printer::expr(a), t.name()));
                }
                SqlType::Bool
            }
            Expr::Neg(a) => {
                let t = self.expr(a, levels, clause)?;
                if !t.numeric() {
                    return type_err(format!("cannot negate {} in `{}`", t.name(), show()));
                }
                t
            }
            Expr::IsNull(a, _) => {
                self.expr(a, levels, clause)?;
                SqlType::Bool
            }
            Expr::Like(a, p, _) => {
                for side in [a, p] {
                    let t = self.expr(side, levels, clause)?;
                    if !t.textual() {
                        return type_err(format!(
                            "LIKE needs text on both sides, but `{}` is {}",
                            printer::expr(side),
                            t.name()
                        ));
                    }
                }
                SqlType::Bool
            }
            Expr::In(a, list, _) => {
                self.expr(a, levels, clause)?;
                for x in list {
                    self.expr(x, levels, clause)?;
                }
                SqlType::Bool
            }
            Expr::InSub(a, sq, _) => {
                self.expr(a, levels, clause)?;
                self.one_column(&sq.q, levels)?;
                SqlType::Bool
            }
            Expr::Exists(sq) => {
                self.query(&sq.q, levels)?;
                SqlType::Bool
            }
            Expr::Subquery(sq) => self.one_column(&sq.q, levels)?,
            Expr::Agg(f, arg, _) => {
                if !clause.aggregates_allowed() {
                    return Err(Error::Parse(format!("aggregate functions are not allowed in {}", clause.name())));
                }
                let t = match arg {
                    None => SqlType::Int,
                    Some(a) => {
                        if super::eval::has_agg(a) {
                            return Err(Error::Parse("aggregate functions cannot be nested".into()));
                        }
                        self.expr(a, levels, clause)?
                    }
                };
                match f {
                    AggFn::Count => SqlType::Int,
                    AggFn::Sum | AggFn::Avg if !t.numeric() => {
                        return type_err(format!("{} needs numbers, but its argument is {}", f.name().to_uppercase(), t.name()))
                    }
                    AggFn::Sum => t,
                    AggFn::Avg => SqlType::Float,
                    AggFn::Min | AggFn::Max => t,
                }
            }
            Expr::Func(name, args) => {
                let Some(&(_, lo, hi, kind, out)) = FUNCTIONS.iter().find(|f| f.0 == name) else {
                    return Err(Error::Parse(format!("unknown function '{name}'")));
                };
                if args.len() < lo || hi.map_or(false, |hi| args.len() > hi) {
                    let want = match hi {
                        Some(hi) if hi == lo => format!("{lo}"),
                        Some(hi) => format!("{lo} to {hi}"),
                        None => format!("at least {lo}"),
                    };
                    return Err(Error::Parse(format!("{name}() takes {want} argument(s), got {}", args.len())));
                }
                let mut types = Vec::with_capacity(args.len());
                for a in args {
                    let t = self.expr(a, levels, clause)?;
                    let ok = match kind {
                        Arg::Any => true,
                        Arg::Text => t.textual(),
                        Arg::Num => t.numeric(),
                    };
                    if !ok {
                        return type_err(format!("{name}() cannot take {} (`{}`)", t.name(), printer::expr(a)));
                    }
                    types.push(t);
                }
                match name.as_str() {
                    "substr" | "substring" => {
                        if !types[0].textual() || types[1..].iter().any(|t| !matches!(t, SqlType::Int | SqlType::Null | SqlType::Any)) {
                            return type_err(format!("{name}() takes (text, int [, int]) in `{}`", show()));
                        }
                    }
                    "round" if types.len() == 2 && !matches!(types[1], SqlType::Int | SqlType::Null | SqlType::Any) => {
                        return type_err(format!("round() digits must be an integer in `{}`", show()));
                    }
                    "coalesce" | "ifnull" | "greatest" | "least" => {
                        return Ok(types.into_iter().fold(SqlType::Null, SqlType::unify));
                    }
                    "nullif" | "abs" | "floor" | "ceil" | "ceiling" | "round" => return Ok(types[0]),
                    _ => {}
                }
                out
            }
            Expr::Cast(a, t) => {
                self.expr(a, levels, clause)?;
                SqlType::of_column(*t)
            }
            Expr::Case(op, whens, els) => {
                let base = op.as_ref().map(|o| self.expr(o, levels, clause)).transpose()?;
                let mut out = SqlType::Null;
                for (w, t) in whens {
                    let wt = self.expr(w, levels, clause)?;
                    if base.is_none() && !wt.boolean() {
                        return type_err(format!(
                            "CASE WHEN needs a boolean condition, but `{}` is {}",
                            printer::expr(w),
                            wt.name()
                        ));
                    }
                    out = out.unify(self.expr(t, levels, clause)?);
                }
                if let Some(x) = els {
                    out = out.unify(self.expr(x, levels, clause)?);
                }
                out
            }
        })
    }

    /// A subquery used as a value or after IN: exactly one column.
    fn one_column(&self, q: &Query, levels: &[Level]) -> Result<SqlType> {
        match self.query(q, levels)? {
            Some(cols) if cols.len() != 1 => {
                Err(Error::Schema(format!("subquery must return exactly one column, got {}", cols.len())))
            }
            Some(cols) => Ok(cols[0].1),
            None => Ok(SqlType::Any),
        }
    }
}

/// Infer the static type of a lone expression (no columns in scope).
#[cfg(test)]
pub(crate) fn type_of(e: &Expr) -> Result<SqlType> {
    Analyzer::new(None).expr(e, &[], Clause::Select)
}
