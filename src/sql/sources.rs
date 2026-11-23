//! The common execution interface between PBSQL queries and the three
//! storage models.
//!
//! Every base FROM item – a table, `DOCUMENT(collection)` or `KV('prefix')` –
//! is opened as a [`Relation`]. The query executor (joins, filtering,
//! grouping, ordering) only ever sees the rows a relation hands back, so
//! one SELECT can join and filter across models.
//!
//! Each relation reads *through its own storage engine* using the
//! statement's [`Ctx`](crate::storage::Ctx): tables through the relational
//! access-path planner and catalog, documents through `doc::find` (so `_id`
//! and field indexes are used), keys through the key-value module. Reads
//! therefore join the session's transaction read set and are validated at
//! COMMIT exactly like reads made through any other interface; nothing in
//! this layer touches raw keys of another model.
//!
//! A relation may receive the enclosing WHERE clause as a *pushdown hint*.
//! It only uses the hint to narrow what it reads, and only in ways that
//! keep every row the full predicate would keep; the executor re-applies the
//! complete predicate to every row.

use super::ast::*;
use super::catalog::Schema;
use super::eval::{const_eval, is_const, QCtx};
use super::query::fetch;
use crate::doc::{self, FindOptions};
use crate::error::Result;
use crate::kv;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Which storage model a relation reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Model {
    Relational,
    Document,
    KeyValue,
}

impl Model {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Model::Relational => "relational",
            Model::Document => "document",
            Model::KeyValue => "key-value",
        }
    }
}

/// Rows produced by a relation, with one column name per value.
pub(super) struct RowSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// A base relation of one storage model.
pub(super) trait Relation {
    fn model(&self) -> Model;
    /// Read candidate rows. `hint` is the enclosing WHERE clause when it may
    /// be pushed down; the caller still applies the full predicate.
    fn read(&self, q: &QCtx, hint: Option<&Expr>) -> Result<RowSet>;
}

/// Open the relation behind a FROM item. Derived tables are not base relations.
///
/// `fields` are the names the query reads through the item's alias; a
/// document relation exposes them as columns even when no document has
/// them (their value is then NULL, see "NULL semantics" in docs/PBSQL.md).
/// `sole` says the item is the query's only source, so unqualified column
/// names in the hint can only mean its columns.
pub(super) fn open<'a>(item: &'a FromItem, fields: Vec<String>, sole: bool) -> Option<Box<dyn Relation + 'a>> {
    match &item.src {
        Source::Table(name) => Some(Box::new(TableRelation { name, alias: &item.alias })),
        Source::Collection(coll) => Some(Box::new(DocRelation { coll, alias: &item.alias, fields, sole })),
        Source::Kv(prefix) => Some(Box::new(KvRelation { prefix, alias: &item.alias, sole })),
        Source::Derived(_) => None,
    }
}

// --------------------------------------------------------------- relational

struct TableRelation<'a> {
    name: &'a str,
    alias: &'a str,
}

impl Relation for TableRelation<'_> {
    fn model(&self) -> Model {
        Model::Relational
    }

    fn read(&self, q: &QCtx, hint: Option<&Expr>) -> Result<RowSet> {
        let s = Schema::load(q.ctx, self.name)?;
        let rows = fetch(q, &s, self.alias, hint)?.into_iter().map(|(_, r)| r).collect();
        Ok(RowSet { columns: s.cols.iter().map(|c| c.name.clone()).collect(), rows })
    }
}

// ----------------------------------------------------------------- pushdown

/// The conjuncts of a WHERE clause.
fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        other => out.push(other),
    }
}

/// A non-NULL constant (a literal or a column-free expression).
fn constant(e: &Expr, q: &QCtx) -> Option<Value> {
    if !is_const(e) {
        return None;
    }
    const_eval(e, q).ok().filter(|v| !v.is_null())
}

/// `alias.col`, an unqualified `col` when the relation is the only source,
/// or `alias.col.path` (`get_path(alias.col, 'path')`): the dotted field path.
fn field_ref(e: &Expr, alias: &str, sole: bool) -> Option<String> {
    match e {
        Expr::Col(Some(q), n) if q == alias => Some(n.clone()),
        Expr::Col(None, n) if sole => Some(n.clone()),
        Expr::Func(f, args) if f == "get_path" && args.len() == 2 => match (&args[0], &args[1]) {
            (inner, Expr::Lit(Value::Text(p))) => field_ref(inner, alias, sole).map(|c| format!("{c}.{p}")),
            _ => None,
        },
        _ => None,
    }
}

// ----------------------------------------------------------------- document

struct DocRelation<'a> {
    coll: &'a str,
    alias: &'a str,
    fields: Vec<String>,
    sole: bool,
}

impl DocRelation<'_> {
    /// A document filter implied by the WHERE clause. Only equality and IN
    /// on scalar constants are pushed: document equality is the same total
    /// order SQL `=` uses, and on an array field it also matches when an
    /// element is equal, so the pushed filter never drops a row SQL keeps.
    /// (Range operators are not pushed: document `$gt` does not compare
    /// across types, SQL `>` does.)
    fn filter(&self, hint: Option<&Expr>, q: &QCtx) -> Value {
        let mut f = BTreeMap::new();
        let mut parts = Vec::new();
        if let Some(h) = hint {
            conjuncts(h, &mut parts);
        }
        let scalar = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_) | Value::Text(_) | Value::Bool(_));
        for c in parts {
            match c {
                Expr::Bin(a, Op::Eq, b) => {
                    let pair = match (field_ref(a, self.alias, self.sole), field_ref(b, self.alias, self.sole)) {
                        (Some(path), None) => constant(b, q).map(|v| (path, v)),
                        (None, Some(path)) => constant(a, q).map(|v| (path, v)),
                        _ => None,
                    };
                    if let Some((path, v)) = pair.filter(|(_, v)| scalar(v)) {
                        f.entry(path).or_insert(v);
                    }
                }
                Expr::In(a, list, false) => {
                    if let Some(path) = field_ref(a, self.alias, self.sole) {
                        let vals: Option<Vec<Value>> = list.iter().map(|x| constant(x, q).filter(scalar)).collect();
                        if let Some(vals) = vals {
                            let mut op = BTreeMap::new();
                            op.insert("$in".to_string(), Value::Array(vals));
                            f.entry(path).or_insert(Value::Object(op));
                        }
                    }
                }
                _ => {}
            }
        }
        Value::Object(f)
    }
}

impl Relation for DocRelation<'_> {
    fn model(&self) -> Model {
        Model::Document
    }

    fn read(&self, q: &QCtx, hint: Option<&Expr>) -> Result<RowSet> {
        let filter = self.filter(hint, q);
        q.note(|| {
            let path = doc::describe_access(q.ctx, self.coll, &filter).unwrap_or_else(|e| e.to_string());
            let pushed = if filter.get("_id").is_some() || matches!(&filter, Value::Object(o) if !o.is_empty()) {
                format!(", filter {} pushed down", filter.to_json())
            } else {
                String::new()
            };
            format!("{} AS {}: {path}{pushed}", self.coll, self.alias)
        });
        let docs = doc::find(q.ctx, self.coll, &filter, &FindOptions::default())?;
        // Columns: _id, every top-level field present, then fields the query
        // names that no document has.
        let mut names = BTreeSet::new();
        for d in &docs {
            if let Value::Object(o) = d {
                names.extend(o.keys().filter(|k| *k != "_id").cloned());
            }
        }
        let mut columns = vec!["_id".to_string()];
        columns.extend(names);
        for f in &self.fields {
            if !columns.contains(f) {
                columns.push(f.clone());
            }
        }
        let rows = docs
            .into_iter()
            .map(|d| columns.iter().map(|c| d.get(c).cloned().unwrap_or(Value::Null)).collect())
            .collect();
        Ok(RowSet { columns, rows })
    }
}

// ---------------------------------------------------------------- key-value

struct KvRelation<'a> {
    prefix: &'a str,
    alias: &'a str,
    sole: bool,
}

/// The literal text before the first wildcard of a LIKE pattern.
fn like_prefix(p: &str) -> &str {
    &p[..p.find(['%', '_']).unwrap_or(p.len())]
}

impl Relation for KvRelation<'_> {
    fn model(&self) -> Model {
        Model::KeyValue
    }

    fn read(&self, q: &QCtx, hint: Option<&Expr>) -> Result<RowSet> {
        let columns = vec!["key".to_string(), "value".to_string()];
        let text = |v: Vec<u8>| Value::Text(String::from_utf8_lossy(&v).into_owned());
        let mut parts = Vec::new();
        if let Some(h) = hint {
            conjuncts(h, &mut parts);
        }
        let is_key = |e: &Expr| field_ref(e, self.alias, self.sole).as_deref() == Some("key");
        // `key = 'k'`: one point lookup.
        for c in &parts {
            if let Expr::Bin(a, Op::Eq, b) = c {
                let k = match (is_key(a), is_key(b)) {
                    (true, false) => constant(b, q),
                    (false, true) => constant(a, q),
                    _ => None,
                };
                if let Some(Value::Text(k)) = k {
                    q.note(|| format!("kv AS {}: key lookup", self.alias));
                    let rows = match kv::get(q.ctx, &k) {
                        Some(v) if k.starts_with(self.prefix) => vec![vec![Value::Text(k), text(v)]],
                        _ => vec![],
                    };
                    return Ok(RowSet { columns, rows });
                }
            }
        }
        // `key LIKE 'abc%'`: narrow the prefix scan.
        let mut prefix = self.prefix.to_string();
        for c in &parts {
            if let Expr::Like(a, p, false) = c {
                if let (true, Some(Value::Text(p))) = (is_key(a), constant(p, q)) {
                    let lp = like_prefix(&p);
                    if lp.starts_with(&prefix) {
                        prefix = lp.to_string();
                    } else if !prefix.starts_with(lp) {
                        // the two prefixes are disjoint: nothing can match
                        q.note(|| format!("kv AS {}: empty (LIKE prefix outside the scanned prefix)", self.alias));
                        return Ok(RowSet { columns, rows: vec![] });
                    }
                }
            }
        }
        q.note(|| format!("kv AS {}: prefix scan '{prefix}'", self.alias));
        let rows = kv::scan(q.ctx, &prefix).into_iter().map(|(k, v)| vec![Value::Text(k), text(v)]).collect();
        Ok(RowSet { columns, rows })
    }
}
