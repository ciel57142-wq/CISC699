//! # PBSQL – the PolyDB query language
//!
//! One language for the three storage models:
//!
//! * relational SQL (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, DDL), whose
//!   FROM clause can also read `DOCUMENT(collection)` and `KV('prefix')`,
//!   so a single query can join and filter across models;
//! * document commands, `db.<collection>.<method>(...)`;
//! * key-value commands, `SET`, `GET`, `DEL`, `EXISTS`, `KEYS`, `INCR`, `DECR`;
//! * `BEGIN` / `COMMIT` / `ROLLBACK`, `EXPLAIN [ANALYZE]`, `SHOW`, `CHECKPOINT`.
//!
//! Processing is a classic pipeline:
//!
//! 1. **lexing** (`lexer`) – span-carrying tokens, positioned errors;
//! 2. **parsing** (`parser`, `sql::parser`) – recursive descent into one
//!    [`Statement`](ast::Statement) tree;
//! 3. **semantic analysis** (`sql::sema`, [`check_document`]) – names,
//!    types, aggregate placement, result shapes, document argument shapes;
//! 4. **execution** (`exec`) – each statement runs against the real
//!    storage engines through the session's transaction context; queries
//!    reach every model through `sql::sources::Relation`.
//!
//! [`normalize`] prints the canonical form of a statement.
//! The language reference is `docs/PBSQL.md`.

pub(crate) mod ast;
pub(crate) mod exec;
pub(crate) mod lexer;
pub(crate) mod parser;
#[cfg(test)]
mod tests;

use crate::error::{Error, Result};
use crate::sql;
use crate::value::Value;
use ast::*;

pub(crate) use parser::parse;

/// Parse and check `src` without touching any database: syntax, plus every
/// semantic rule that does not need the catalog. Errors are the same ones
/// `Database::execute` would report for a statement that can never be valid.
pub fn validate(src: &str) -> Result<()> {
    check(&parse(src)?, None)
}

/// The canonical text of a statement (see `sql::printer` for the rules).
/// Two spellings of the same statement normalize to the same text, and the
/// result normalizes to itself.
pub fn normalize(src: &str) -> Result<String> {
    Ok(print(&parse(src)?))
}

/// Semantic checks; with a context, relational names and types are
/// resolved against the catalog.
pub(crate) fn check(stmt: &Statement, ctx: Option<&crate::storage::Ctx>) -> Result<()> {
    match stmt {
        Statement::Sql(s) => sql::sema::Analyzer::new(ctx).statement(s),
        Statement::Doc(d) => check_document(d),
        Statement::Kv(k) => check_kv(k),
        Statement::Explain { stmt, .. } => check(stmt, ctx),
        Statement::Txn(_) | Statement::Show(_) | Statement::Checkpoint => Ok(()),
    }
}

fn is_object(v: &Value) -> bool {
    matches!(v, Value::Object(_))
}

/// Shape rules for document commands. Filters and updates are compiled by
/// the document engine's own compilers, so PBSQL accepts exactly what the
/// engine accepts and rejects a bad one before any document is read.
pub(crate) fn check_document(d: &DocStmt) -> Result<()> {
    use crate::doc;
    if !crate::keys::valid_name(&d.collection) {
        return Err(Error::Schema(format!("invalid collection name '{}'", d.collection)));
    }
    let filter = |f: &Value| doc::filter::compile(f).map(|_| ());
    match &d.op {
        DocOp::Insert(Value::Array(docs)) => {
            if let Some(bad) = docs.iter().find(|d| !is_object(d)) {
                return Err(Error::Type(format!("insert() needs documents (objects), got {}", bad.to_json())));
            }
            Ok(())
        }
        DocOp::Insert(d) if !is_object(d) => {
            Err(Error::Type(format!("insert() needs a document (an object), got {}", d.to_json())))
        }
        DocOp::Find { filter: f, spec, .. } => {
            filter(f)?;
            if let Some(s) = &spec.sort {
                doc::aggregate::parse_sort(s)?;
            }
            Ok(())
        }
        DocOp::Count(f) | DocOp::Delete { filter: f, .. } | DocOp::Explain(f) => filter(f),
        DocOp::Distinct { field, filter: f } => {
            doc::path::validate_path(field)?;
            filter(f)
        }
        DocOp::Update { filter: f, update, method, .. } => {
            filter(f)?;
            let replacement = doc::update::is_replacement(update)?;
            match method {
                UpdateMethod::ReplaceOne if !replacement => {
                    Err(Error::Parse("replaceOne takes a document without operators".into()))
                }
                UpdateMethod::UpdateOne | UpdateMethod::UpdateMany if replacement => Err(Error::Parse(format!(
                    "{}() takes update operators such as $set; use replaceOne",
                    if *method == UpdateMethod::UpdateOne { "updateone" } else { "updatemany" }
                ))),
                _ if !replacement => doc::update::validate(update),
                _ => Ok(()),
            }
        }
        DocOp::CreateIndex { spec, .. } => doc::index_fields(spec).map(|_| ()),
        DocOp::Insert(_)
        | DocOp::Aggregate(_)
        | DocOp::BulkWrite(_)
        | DocOp::DropIndex(_)
        | DocOp::GetIndexes
        | DocOp::SetSchema(_)
        | DocOp::GetSchema
        | DocOp::Validate
        | DocOp::Reindex
        | DocOp::Drop => Ok(()),
    }
}

pub(crate) fn check_kv(k: &KvStmt) -> Result<()> {
    match k {
        KvStmt::Set { key, .. }
        | KvStmt::Get(key)
        | KvStmt::Del(key)
        | KvStmt::Exists(key)
        | KvStmt::Incr { key, .. }
            if key.is_empty() =>
        {
            Err(Error::Parse("a key cannot be empty".into()))
        }
        _ => Ok(()),
    }
}

// ----------------------------------------------------------- normalization

/// A quoted key-value string with backslash escapes.
fn quote(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out += "\\\\",
            '\'' => out += "\\'",
            '\n' => out += "\\n",
            '\t' => out += "\\t",
            '\r' => out += "\\r",
            '\0' => out += "\\0",
            c => out.push(c),
        }
    }
    out + "'"
}

/// A key: bare when that reads back unchanged, else quoted.
fn kv_word(s: &str, last: bool) -> String {
    let bare = !s.is_empty()
        && !s.starts_with(['\'', '"'])
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        && !(last && s.ends_with(';'));
    if bare {
        s.to_string()
    } else {
        quote(s)
    }
}

/// A SET value: verbatim text runs to the end of the statement, so inner
/// blanks are fine; anything that would not read back unchanged is quoted.
fn kv_value(v: &str) -> String {
    let verbatim = !v.is_empty()
        && v.trim() == v
        && !v.starts_with(['\'', '"'])
        && !v.ends_with(';')
        && !v.chars().any(|c| c.is_control());
    if verbatim {
        v.to_string()
    } else {
        quote(v)
    }
}

/// Canonical text of a statement.
pub(crate) fn print(stmt: &Statement) -> String {
    match stmt {
        Statement::Sql(s) => sql::printer::stmt(s),
        Statement::Txn(TxnOp::Begin) => "BEGIN".into(),
        Statement::Txn(TxnOp::Commit) => "COMMIT".into(),
        Statement::Txn(TxnOp::Rollback) => "ROLLBACK".into(),
        Statement::Show(ShowWhat::Tables) => "SHOW TABLES".into(),
        Statement::Show(ShowWhat::Collections) => "SHOW COLLECTIONS".into(),
        Statement::Checkpoint => "CHECKPOINT".into(),
        Statement::Explain { analyze, stmt } => {
            format!("EXPLAIN {}{}", if *analyze { "ANALYZE " } else { "" }, print(stmt))
        }
        Statement::Kv(k) => match k {
            KvStmt::Set { key, value } => format!("SET {} {}", kv_word(key, false), kv_value(value)),
            KvStmt::Get(key) => format!("GET {}", kv_word(key, true)),
            KvStmt::Del(key) => format!("DEL {}", kv_word(key, true)),
            KvStmt::Exists(key) => format!("EXISTS {}", kv_word(key, true)),
            KvStmt::Keys(p) if p.is_empty() => "KEYS".into(),
            KvStmt::Keys(p) => format!("KEYS {}", kv_word(p, true)),
            KvStmt::Incr { key, by: 1 } => format!("INCR {}", kv_word(key, true)),
            KvStmt::Incr { key, by } => format!("INCR {} {by}", kv_word(key, false)),
        },
        Statement::Doc(d) => {
            let c = &d.collection;
            let j = |v: &Value| v.to_json();
            let opt = |o: &Option<Value>| o.as_ref().map_or(String::new(), |v| format!(", {}", v.to_json()));
            let list = |a: &[Value]| Value::Array(a.to_vec()).to_json();
            match &d.op {
                DocOp::Insert(v) => format!("db.{c}.insert({})", j(v)),
                DocOp::Find { filter, spec, one, end } => {
                    let mut s = format!("db.{c}.{}({})", if *one { "findOne" } else { "find" }, j(filter));
                    if let Some(p) = &spec.projection {
                        s += &format!(".project({})", j(p));
                    }
                    if let Some(so) = &spec.sort {
                        s += &format!(".sort({})", j(so));
                    }
                    if spec.skip > 0 {
                        s += &format!(".skip({})", spec.skip);
                    }
                    if let (Some(n), false) = (spec.limit, *one) {
                        s += &format!(".limit({n})");
                    }
                    match end {
                        Some(FindEnd::Count) => s + ".count()",
                        Some(FindEnd::Explain) => s + ".explain()",
                        None => s,
                    }
                }
                DocOp::Count(f) => format!("db.{c}.count({})", j(f)),
                DocOp::Distinct { field, filter } => {
                    format!("db.{c}.distinct({}, {})", j(&Value::Text(field.clone())), j(filter))
                }
                DocOp::Aggregate(stages) => format!("db.{c}.aggregate({})", list(stages)),
                DocOp::Explain(f) => format!("db.{c}.explain({})", j(f)),
                DocOp::Update { filter, update, method, options } => {
                    let m = match method {
                        UpdateMethod::Update => "update",
                        UpdateMethod::UpdateOne => "updateOne",
                        UpdateMethod::UpdateMany => "updateMany",
                        UpdateMethod::ReplaceOne => "replaceOne",
                    };
                    format!("db.{c}.{m}({}, {}{})", j(filter), j(update), opt(options))
                }
                DocOp::Delete { filter, one } => {
                    format!("db.{c}.{}({})", if *one { "deleteOne" } else { "deleteMany" }, j(filter))
                }
                DocOp::BulkWrite(ops) => format!("db.{c}.bulkWrite({})", list(ops)),
                DocOp::CreateIndex { spec, options } => format!("db.{c}.createIndex({}{})", j(spec), opt(options)),
                DocOp::DropIndex(spec) => format!("db.{c}.dropIndex({})", j(spec)),
                DocOp::GetIndexes => format!("db.{c}.getIndexes()"),
                DocOp::SetSchema(v) => format!("db.{c}.setSchema({})", j(v)),
                DocOp::GetSchema => format!("db.{c}.getSchema()"),
                DocOp::Validate => format!("db.{c}.validate()"),
                DocOp::Reindex => format!("db.{c}.reindex()"),
                DocOp::Drop => format!("db.{c}.drop()"),
            }
        }
    }
}
