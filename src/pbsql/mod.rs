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

/// Shape rules for document commands.
pub(crate) fn check_document(d: &DocStmt) -> Result<()> {
    if !crate::keys::valid_name(&d.collection) {
        return Err(Error::Schema(format!("invalid collection name '{}'", d.collection)));
    }
    let filter = |f: &Value| -> Result<()> {
        if is_object(f) {
            Ok(())
        } else {
            Err(Error::Type(format!("a filter must be a JSON object, got {}", f.to_json())))
        }
    };
    match &d.op {
        DocOp::Insert(Value::Array(docs)) => {
            if let Some(bad) = docs.iter().find(|d| !is_object(d)) {
                return Err(Error::Type(format!("insert() needs documents (objects), got {}", bad.to_json())));
            }
            Ok(())
        }
        DocOp::Insert(doc) if !is_object(doc) => {
            Err(Error::Type(format!("insert() needs a document (an object), got {}", doc.to_json())))
        }
        DocOp::Insert(_) | DocOp::Drop => Ok(()),
        DocOp::Find { filter: f, sort, .. } => {
            filter(f)?;
            match sort {
                Some(Value::Object(spec)) => {
                    if let Some((k, v)) = spec.iter().find(|(_, v)| v.as_f64().map_or(true, |n| n != 1.0 && n != -1.0)) {
                        return Err(Error::Type(format!("sort direction for '{k}' must be 1 or -1, got {}", v.to_json())));
                    }
                    Ok(())
                }
                Some(other) => Err(Error::Type(format!("sort must be an object like {{\"field\": 1}}, got {}", other.to_json()))),
                None => Ok(()),
            }
        }
        DocOp::Count(f) | DocOp::Delete { filter: f, .. } => filter(f),
        DocOp::Update { filter: f, update, .. } => {
            filter(f)?;
            if !is_object(update) {
                return Err(Error::Type(format!("an update must be a JSON object, got {}", update.to_json())));
            }
            Ok(())
        }
        DocOp::CreateIndex(field) => {
            if field.is_empty() || field.starts_with('$') {
                return Err(Error::Schema(format!("cannot index field '{field}'")));
            }
            Ok(())
        }
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
            match &d.op {
                DocOp::Insert(v) => format!("db.{c}.insert({})", v.to_json()),
                DocOp::Find { filter, sort, skip, limit, one } => {
                    let mut s = format!("db.{c}.{}({})", if *one { "findOne" } else { "find" }, filter.to_json());
                    if let Some(so) = sort {
                        s += &format!(".sort({})", so.to_json());
                    }
                    if *skip > 0 {
                        s += &format!(".skip({skip})");
                    }
                    if let (Some(n), false) = (limit, *one) {
                        s += &format!(".limit({n})");
                    }
                    s
                }
                DocOp::Count(f) => format!("db.{c}.count({})", f.to_json()),
                DocOp::Update { filter, update, multi } => format!(
                    "db.{c}.{}({}, {})",
                    if *multi { "updateMany" } else { "updateOne" },
                    filter.to_json(),
                    update.to_json()
                ),
                DocOp::Delete { filter, one } => {
                    format!("db.{c}.{}({})", if *one { "deleteOne" } else { "deleteMany" }, filter.to_json())
                }
                DocOp::CreateIndex(f) => {
                    let mut o = std::collections::BTreeMap::new();
                    o.insert(f.clone(), Value::Int(1));
                    format!("db.{c}.createIndex({})", Value::Object(o).to_json())
                }
                DocOp::Drop => format!("db.{c}.drop()"),
            }
        }
    }
}
