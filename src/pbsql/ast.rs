//! The PBSQL syntax tree: one statement type covering all three storage
//! models. Relational statements reuse the relational syntax tree
//! (`sql::ast`), which also carries the cross-model FROM sources
//! `DOCUMENT(collection)` and `KV('prefix')`.

use crate::sql::ast::Stmt;
use crate::value::Value;

/// One parsed statement. (Built once per command and consumed by execution,
/// so the size difference between variants does not matter.)
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Statement {
    /// SELECT, INSERT, UPDATE, DELETE, DDL and the relational utilities.
    Sql(Stmt),
    /// `db.<collection>.<method>(...)`
    Doc(DocStmt),
    /// `SET`, `GET`, `DEL`, `EXISTS`, `KEYS`, `INCR`, `DECR`
    Kv(KvStmt),
    Txn(TxnOp),
    Show(ShowWhat),
    Checkpoint,
    /// `EXPLAIN [ANALYZE] <statement>`
    Explain { analyze: bool, stmt: Box<Statement> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxnOp {
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShowWhat {
    Tables,
    Collections,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DocStmt {
    /// Taken verbatim (case preserved), as in the MongoDB shell.
    pub collection: String,
    pub op: DocOp,
}

/// Options of `find` / `findOne` and their chained modifiers.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct FindSpec {
    pub sort: Option<Value>,
    pub skip: usize,
    pub limit: Option<usize>,
    pub projection: Option<Value>,
}

/// A terminal `.count()` or `.explain()` after `find(...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindEnd {
    Count,
    Explain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateMethod {
    /// `update(filter, update[, {multi, upsert}])`: every match unless `{multi: false}`.
    Update,
    UpdateOne,
    UpdateMany,
    /// `replaceOne(filter, document)`: a whole-document replacement.
    ReplaceOne,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DocOp {
    /// One document, or an array of documents.
    Insert(Value),
    Find { filter: Value, spec: FindSpec, one: bool, end: Option<FindEnd> },
    Count(Value),
    Distinct { field: String, filter: Value },
    Aggregate(Vec<Value>),
    /// `explain(filter)`: the access path a filter uses.
    Explain(Value),
    Update { filter: Value, update: Value, method: UpdateMethod, options: Option<Value> },
    Delete { filter: Value, one: bool },
    BulkWrite(Vec<Value>),
    CreateIndex { spec: Value, options: Option<Value> },
    DropIndex(Value),
    GetIndexes,
    SetSchema(Value),
    GetSchema,
    Validate,
    Reindex,
    Drop,
}

impl DocOp {
    /// Does the operation only read?
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(
            self,
            DocOp::Find { .. }
                | DocOp::Count(_)
                | DocOp::Distinct { .. }
                | DocOp::Aggregate(_)
                | DocOp::Explain(_)
                | DocOp::GetIndexes
                | DocOp::GetSchema
                | DocOp::Validate
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KvStmt {
    Set { key: String, value: String },
    Get(String),
    Del(String),
    Exists(String),
    /// Keys starting with the prefix ("" for all).
    Keys(String),
    /// `INCR key [n]`; `DECR key [n]` is `Incr` with `-n`.
    Incr { key: String, by: i64 },
}

impl Statement {
    /// Does the statement only read? Read-only statements share the database lock.
    pub(crate) fn is_read_only(&self) -> bool {
        match self {
            Statement::Sql(s) => crate::sql::is_read_only(s),
            Statement::Doc(d) => d.op.is_read_only(),
            Statement::Kv(k) => matches!(k, KvStmt::Get(_) | KvStmt::Exists(_) | KvStmt::Keys(_)),
            Statement::Show(_) => true,
            Statement::Explain { analyze, stmt } => !analyze || stmt.is_read_only(),
            Statement::Txn(_) | Statement::Checkpoint => false,
        }
    }

    /// The storage model the statement belongs to (for messages and tests).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Statement::Sql(_) => "relational",
            Statement::Doc(_) => "document",
            Statement::Kv(_) => "key-value",
            Statement::Txn(_) => "transaction",
            Statement::Show(_) | Statement::Checkpoint => "utility",
            Statement::Explain { .. } => "explain",
        }
    }
}
