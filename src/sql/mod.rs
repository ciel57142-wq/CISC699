//! Relational model: typed tables with primary keys, constraints and
//! secondary indexes, queried through the relational part of PBSQL.
//!
//! Shared PBSQL lexer -> recursive-descent parser -> semantic analysis ->
//! planner / executor over the shared ordered key-value store.
//!
//! | file         | role                                                        |
//! |--------------|-------------------------------------------------------------|
//! | `ast.rs`     | syntax tree                                                 |
//! | `parser.rs`  | parser, CHECK-constraint printer                            |
//! | `sema.rs`    | name resolution, type checking, aggregate and shape rules   |
//! | `printer.rs` | canonical statement text (query normalization)              |
//! | `walk.rs`    | syntax-tree traversal                                       |
//! | `catalog.rs` | schemas, row and index-entry layout                         |
//! | `sources.rs` | common interface to relational, document and KV storage     |
//! | `eval.rs`    | expression evaluation, functions, aggregates, subqueries    |
//! | `query.rs`   | access-path selection, joins, grouping, set operations      |
//! | `dml.rs`     | INSERT / UPDATE / DELETE and constraint enforcement         |
//! | `ddl.rs`     | CREATE / DROP / ALTER, indexes, CHECK TABLE, REINDEX        |

pub(crate) mod ast;
mod catalog;
mod ddl;
mod dml;
mod eval;
pub(crate) mod parser;
pub(crate) mod printer;
mod query;
pub(crate) mod sema;
pub(crate) mod sources;
#[cfg(test)]
mod tests;
pub(crate) mod walk;

use crate::error::{Error, Result};
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use ast::{InsertSource, Stmt};
use eval::QCtx;
use std::time::Instant;

pub use catalog::tables;

/// Does this statement only read? (Read-only statements take the shared lock.)
pub(crate) fn is_read_only(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Query(_) | Stmt::Explain(_) | Stmt::Describe(_) | Stmt::CheckTable(_))
}

/// Parse, check and run one relational statement.
#[cfg(test)]
pub fn execute(ctx: &mut Ctx, sql: &str) -> Result<Output> {
    execute_stmt(ctx, parser::parse(sql)?)
}

/// Check a parsed statement against the catalog, then run it.
pub(crate) fn execute_stmt(ctx: &mut Ctx, stmt: Stmt) -> Result<Output> {
    sema::Analyzer::new(Some(ctx)).statement(&stmt)?;
    run(ctx, stmt)
}

fn run(ctx: &mut Ctx, stmt: Stmt) -> Result<Output> {
    match stmt {
        Stmt::CreateTable(def) => ddl::create_table(ctx, def),
        Stmt::DropTable { name, if_exists, cascade } => ddl::drop_table(ctx, &name, if_exists, cascade),
        Stmt::CreateIndex { name, table, cols, unique, if_not_exists } => {
            ddl::create_index(ctx, &name, &table, cols, unique, if_not_exists)
        }
        Stmt::DropIndex { name, table, if_exists } => ddl::drop_index(ctx, &name, table.as_deref(), if_exists),
        Stmt::AlterTable { table, actions } => ddl::alter_table(ctx, &table, actions),
        Stmt::Truncate(table) => dml::truncate(ctx, &table),
        Stmt::Insert { table, cols, source } => dml::insert(ctx, &table, cols, source),
        Stmt::Update { table, sets, filter } => dml::update(ctx, &table, sets, filter),
        Stmt::Delete { table, filter } => dml::delete(ctx, &table, filter),
        Stmt::Query(query) => {
            let q = QCtx::new(ctx);
            let (columns, rows) = query::run_query(&q, &query, None, 0)?;
            Ok(Output::Rows { columns, rows })
        }
        Stmt::Explain(query) => explain(ctx, Stmt::Query(query), false),
        Stmt::Describe(table) => ddl::describe(ctx, &table),
        Stmt::CheckTable(table) => ddl::check_tables(ctx, table.as_deref()),
        Stmt::Reindex(table) => ddl::reindex(ctx, table.as_deref()),
    }
}

fn plan_output(lines: Vec<String>) -> Output {
    Output::Rows { columns: vec!["plan".into()], rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect() }
}

/// The access paths a query uses (and, with `analyze`, what it read and returned).
fn query_plan(ctx: &Ctx, query: &ast::Query, analyze: bool) -> Result<(Vec<String>, usize)> {
    let mut q = QCtx::new(ctx);
    q.explain = true;
    q.analyze = analyze;
    let (_, rows) = query::run_query(&q, query, None, 0)?;
    Ok((q.plan.into_inner(), rows.len()))
}

fn elapsed(start: Instant) -> String {
    format!("execution time: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0)
}

/// `EXPLAIN [ANALYZE] statement`. Plain EXPLAIN never changes data. EXPLAIN
/// ANALYZE runs the statement for real (inside the session's transaction
/// or as its own atomic statement, like any other) and reports what it did.
pub(crate) fn explain(ctx: &mut Ctx, stmt: Stmt, analyze: bool) -> Result<Output> {
    sema::Analyzer::new(Some(ctx)).statement(&stmt)?;
    let start = Instant::now();
    let where_plan = |ctx: &Ctx, table: &str, filter: &Option<ast::Expr>| -> Result<Vec<String>> {
        let select = ast::Query {
            head: ast::Select {
                distinct: false,
                items: vec![ast::Item::Star],
                from: Some(ast::FromItem { src: ast::Source::Table(table.to_string()), alias: table.to_string() }),
                joins: vec![],
                filter: filter.clone(),
                group_by: vec![],
                having: None,
                order_by: vec![],
                limit: None,
                offset: 0,
            },
            rest: vec![],
            order_by: vec![],
            limit: None,
            offset: 0,
        };
        Ok(query_plan(ctx, &select, false)?.0)
    };
    let mut lines = Vec::new();
    match stmt {
        Stmt::Query(query) | Stmt::Explain(query) => {
            let (plan, n) = query_plan(ctx, &query, analyze)?;
            lines = plan;
            if analyze {
                lines.push(format!("actual: {n} row(s) returned"));
            }
        }
        stmt @ (Stmt::Update { .. } | Stmt::Delete { .. }) => {
            let (verb, table, filter) = match &stmt {
                Stmt::Update { table, filter, .. } => ("update", table.clone(), filter.clone()),
                Stmt::Delete { table, filter } => ("delete from", table.clone(), filter.clone()),
                _ => unreachable!(),
            };
            lines.push(format!("{verb} {table}"));
            lines.extend(where_plan(ctx, &table, &filter)?.into_iter().map(|l| format!("  {l}")));
            if analyze {
                lines.push(format!("actual: {}", run(ctx, stmt)?));
            }
        }
        Stmt::Insert { table, cols, source } => {
            lines.push(format!("insert into {table}"));
            match &source {
                InsertSource::Values(rows) => lines.push(format!("  {} row(s) from VALUES", rows.len())),
                InsertSource::DefaultValues => lines.push("  1 row of defaults".into()),
                InsertSource::Query(q) => {
                    lines.extend(query_plan(ctx, q, false)?.0.into_iter().map(|l| format!("  {l}")));
                }
            }
            if analyze {
                lines.push(format!("actual: {}", run(ctx, Stmt::Insert { table, cols, source })?));
            }
        }
        _ => return Err(Error::Parse("EXPLAIN supports SELECT, INSERT, UPDATE and DELETE".into())),
    }
    if analyze {
        lines.push(elapsed(start));
    }
    Ok(plan_output(lines))
}
