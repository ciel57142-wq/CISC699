//! Execution of PBSQL statements against the storage engines.
//!
//! Every function here receives the statement's [`Ctx`]: the session's
//! transaction overlay and read set (inside `BEGIN`), or a fresh atomic
//! write set (outside). Relational statements go through the relational
//! engine (catalog, constraints, indexes), document commands through
//! `doc`, key-value commands through `kv`. No path writes storage keys of
//! another model directly, so constraints, index maintenance, atomicity and
//! serializable validation hold for every statement.
//!
//! Transaction control and CHECKPOINT act on the session, not on a context,
//! and are run by `Database::execute_statement`.

use super::ast::*;
use crate::doc::{self, FindOptions};
use crate::error::{Error, Result};
use crate::kv;
use crate::sql;
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use std::time::Instant;

/// Run a data statement (anything but transaction control and CHECKPOINT).
/// Semantic analysis happens here, against the live catalog, before anything runs.
pub(crate) fn execute(ctx: &mut Ctx, stmt: Statement) -> Result<Output> {
    match stmt {
        Statement::Sql(s) => sql::execute_stmt(ctx, s),
        Statement::Doc(d) => {
            super::check_document(&d)?;
            document(ctx, d)
        }
        Statement::Kv(k) => {
            super::check_kv(&k)?;
            key_value(ctx, k)
        }
        Statement::Show(what) => {
            let (header, names) = match what {
                ShowWhat::Tables => ("table", sql::tables(ctx)),
                ShowWhat::Collections => ("collection", doc::collections(ctx)),
            };
            Ok(Output::Rows {
                columns: vec![header.to_string()],
                rows: names.into_iter().map(|n| vec![Value::Text(n)]).collect(),
            })
        }
        Statement::Explain { analyze, stmt } => explain(ctx, *stmt, analyze),
        Statement::Txn(_) | Statement::Checkpoint => {
            Err(Error::Txn("transaction control is handled by the session, not inside a statement".into()))
        }
    }
}

fn document(ctx: &mut Ctx, d: DocStmt) -> Result<Output> {
    let coll = d.collection.as_str();
    let flag = |o: &Option<Value>, key: &str| o.as_ref().and_then(|o| o.get(key)) == Some(&Value::Bool(true));
    match d.op {
        DocOp::Insert(Value::Array(docs)) => Ok(Output::Scalar(Value::Array(doc::insert_many(ctx, coll, docs)?))),
        DocOp::Insert(doc) => Ok(Output::Scalar(doc::insert(ctx, coll, doc)?)),
        DocOp::Find { filter, spec, end, .. } => match end {
            Some(FindEnd::Count) => Ok(Output::Scalar(Value::Int(doc::count(ctx, coll, &filter)? as i64))),
            Some(FindEnd::Explain) => Ok(Output::Scalar(doc::explain(ctx, coll, &filter)?)),
            None => {
                let opts = FindOptions { sort: spec.sort, skip: spec.skip, limit: spec.limit, projection: spec.projection };
                Ok(Output::Docs(doc::find(ctx, coll, &filter, &opts)?))
            }
        },
        DocOp::Count(filter) => Ok(Output::Scalar(Value::Int(doc::count(ctx, coll, &filter)? as i64))),
        DocOp::Distinct { field, filter } => Ok(Output::Scalar(Value::Array(doc::distinct(ctx, coll, &field, &filter)?))),
        DocOp::Aggregate(stages) => Ok(Output::Docs(doc::aggregate(ctx, coll, &stages)?)),
        DocOp::Explain(filter) => Ok(Output::Scalar(doc::explain(ctx, coll, &filter)?)),
        DocOp::Update { filter, update, method, options } => {
            // Plain update() changes every match unless given {multi: false}.
            let multi = match method {
                UpdateMethod::UpdateMany => true,
                UpdateMethod::Update => {
                    options.as_ref().and_then(|o| o.get("multi")).map_or(true, |m| *m == Value::Bool(true))
                }
                UpdateMethod::UpdateOne | UpdateMethod::ReplaceOne => false,
            };
            let r = doc::update(ctx, coll, &filter, &update, multi, flag(&options, "upsert"))?;
            let mut msg = format!("{} document(s) matched, {} modified", r.matched, r.modified);
            if let Some(id) = r.upserted_id {
                msg.push_str(&format!(", upserted _id {}", id.to_json()));
            }
            Ok(Output::Done(msg))
        }
        DocOp::Delete { filter, one } => {
            let n = doc::delete(ctx, coll, &filter, one.then_some(1))?;
            Ok(Output::Done(format!("{n} document(s) deleted")))
        }
        DocOp::BulkWrite(ops) => Ok(Output::Scalar(doc::bulk_write(ctx, coll, &ops)?)),
        DocOp::CreateIndex { spec, options } => {
            let fields = doc::index_fields(&spec)?;
            let unique = flag(&options, "unique");
            let created = doc::create_index(ctx, coll, fields.clone(), unique)?;
            let label = fields.join(", ");
            Ok(Output::Done(if created {
                format!("{}index created on {coll}({label})", if unique { "unique " } else { "" })
            } else {
                format!("{coll}({label}) is already indexed")
            }))
        }
        DocOp::DropIndex(spec) => {
            let dropped = doc::drop_index(ctx, coll, &spec)?;
            Ok(Output::Done(if dropped { "index dropped".into() } else { "no such index, nothing dropped".into() }))
        }
        DocOp::GetIndexes => Ok(Output::Docs(doc::list_indexes(ctx, coll)?)),
        DocOp::SetSchema(schema) => {
            doc::set_schema(ctx, coll, &schema)?;
            Ok(Output::Done(format!("schema of '{coll}' updated")))
        }
        DocOp::GetSchema => Ok(Output::Scalar(doc::get_schema(ctx, coll)?)),
        DocOp::Validate => Ok(Output::Scalar(doc::validate(ctx, coll)?)),
        DocOp::Reindex => Ok(Output::Done(format!("{} index entr(y/ies) rebuilt", doc::reindex(ctx, coll)?))),
        DocOp::Drop => {
            let existed = doc::drop(ctx, coll)?;
            Ok(Output::Done(if existed {
                format!("collection '{coll}' dropped")
            } else {
                format!("collection '{coll}' does not exist, nothing dropped")
            }))
        }
    }
}

fn key_value(ctx: &mut Ctx, k: KvStmt) -> Result<Output> {
    Ok(match k {
        KvStmt::Set { key, value } => {
            kv::set(ctx, &key, value.as_bytes());
            Output::Done("OK".into())
        }
        KvStmt::Get(key) => Output::Scalar(match kv::get(ctx, &key) {
            Some(v) => Value::Text(String::from_utf8_lossy(&v).into_owned()),
            None => Value::Null,
        }),
        KvStmt::Del(key) => Output::Scalar(Value::Int(kv::del(ctx, &key) as i64)),
        KvStmt::Exists(key) => Output::Scalar(Value::Bool(kv::get(ctx, &key).is_some())),
        KvStmt::Keys(prefix) => Output::Rows {
            columns: vec!["key".into()],
            rows: kv::keys(ctx, &prefix).into_iter().map(|k| vec![Value::Text(k)]).collect(),
        },
        KvStmt::Incr { key, by } => Output::Scalar(Value::Int(kv::incr(ctx, &key, by)?)),
    })
}

fn plan(lines: Vec<String>) -> Output {
    Output::Rows { columns: vec!["plan".into()], rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect() }
}

/// `EXPLAIN [ANALYZE]`. Plain EXPLAIN reads nothing it does not need and
/// writes nothing. EXPLAIN ANALYZE also runs the statement, for real.
fn explain(ctx: &mut Ctx, stmt: Statement, analyze: bool) -> Result<Output> {
    let start = Instant::now();
    let mut lines = Vec::new();
    match stmt {
        Statement::Sql(s) => return sql::explain(ctx, s, analyze),
        Statement::Doc(d) => {
            super::check_document(&d)?;
            let coll = d.collection.clone();
            let filter = match &d.op {
                DocOp::Find { filter, .. } | DocOp::Count(filter) => filter.clone(),
                DocOp::Update { filter, .. } | DocOp::Delete { filter, .. } => filter.clone(),
                _ => Value::Object(Default::default()),
            };
            let verb = match &d.op {
                DocOp::Find { .. } => "find",
                DocOp::Count(_) => "count",
                DocOp::Update { .. } => "update",
                DocOp::Delete { .. } => "delete",
                _ => "insert",
            };
            let mut examined = 0;
            if let DocOp::Insert(v) = &d.op {
                let n = if let Value::Array(a) = v { a.len() } else { 1 };
                lines.push(format!("insert into collection {coll}: {n} document(s)"));
            } else {
                let (path, n) = crate::sql::sources::doc_access(ctx, &coll, &filter)?;
                examined = n;
                lines.push(format!("{verb}: collection {coll}: {path}"));
                lines.push(format!("  filter {}", filter.to_json()));
            }
            if let DocOp::Find { spec, .. } = &d.op {
                if let Some(s) = &spec.sort {
                    lines.push(format!("  sort {}", s.to_json()));
                }
                if spec.skip > 0 || spec.limit.is_some() {
                    lines.push(format!("  skip {}, limit {}", spec.skip, spec.limit.map_or("none".into(), |n| n.to_string())));
                }
            }
            if analyze {
                if verb != "insert" {
                    lines.push(format!("  candidates examined: {examined}"));
                }
                let out = document(ctx, d)?;
                lines.push(match out {
                    Output::Docs(docs) => format!("actual: {} document(s) returned", docs.len()),
                    Output::Scalar(Value::Array(ids)) => format!("actual: {} document(s) inserted", ids.len()),
                    Output::Scalar(v) if verb == "insert" => format!("actual: 1 document inserted, _id {}", v.to_json()),
                    Output::Scalar(v) => format!("actual: {}", v.display()),
                    other => format!("actual: {other}"),
                });
            }
        }
        Statement::Kv(k) => {
            super::check_kv(&k)?;
            lines.push(match &k {
                KvStmt::Keys(p) => format!("key-value prefix scan '{p}'"),
                KvStmt::Get(key) | KvStmt::Exists(key) => format!("key-value lookup '{key}'"),
                KvStmt::Set { key, .. } => format!("key-value write '{key}'"),
                KvStmt::Del(key) => format!("key-value delete '{key}'"),
                KvStmt::Incr { key, .. } => format!("key-value read-modify-write '{key}'"),
            });
            if analyze {
                let out = key_value(ctx, k)?;
                lines.push(match out {
                    Output::Rows { rows, .. } => format!("actual: {} key(s)", rows.len()),
                    other => format!("actual: {other}"),
                });
            }
        }
        other => {
            return Err(Error::Parse(format!("EXPLAIN cannot be applied to a {} statement of this kind", other.kind())))
        }
    }
    if analyze {
        lines.push(format!("Execution time: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0));
    }
    Ok(plan(lines))
}
