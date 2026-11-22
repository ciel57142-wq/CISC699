//! Relational model: typed tables with primary keys, constraints and
//! secondary indexes, queried through SQL.
//!
//! Lexer -> recursive-descent parser -> planner / executor over the shared
//! ordered key-value store.
//!
//! | file         | role                                                        |
//! |--------------|-------------------------------------------------------------|
//! | `ast.rs`     | syntax tree                                                 |
//! | `parser.rs`  | lexer, parser, expression printer                           |
//! | `catalog.rs` | schemas, row and index-entry layout                         |
//! | `eval.rs`    | expression evaluation, functions, aggregates, subqueries    |
//! | `access.rs`  | predicates, access paths (key / index / range / scan), probes |
//! | `stats.rs`   | ANALYZE statistics, selectivity and cardinality estimates   |
//! | `query.rs`   | pushdown, join order and algorithms, grouping, set ops      |
//! | `plan.rs`    | operator tree for EXPLAIN and EXPLAIN ANALYZE               |
//! | `dml.rs`     | INSERT / UPDATE / DELETE and constraint enforcement         |
//! | `ddl.rs`     | CREATE / DROP / ALTER, indexes, CHECK TABLE, REINDEX        |

mod access;
mod ast;
mod catalog;
mod ddl;
mod dml;
mod eval;
mod parser;
mod plan;
mod query;
mod stats;
#[cfg(test)]
mod tests;

use crate::error::Result;
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use ast::Stmt;
use eval::QCtx;

pub use catalog::tables;
pub use ddl::{repair_indexes, RepairReport};

/// Does this statement only read? (Read-only statements take the shared lock.)
pub fn is_read_only(statement: &str) -> bool {
    let w = statement.trim_start().split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_ascii_lowercase();
    matches!(w.as_str(), "select" | "explain" | "describe" | "desc" | "check")
}

pub fn execute(ctx: &mut Ctx, sql: &str) -> Result<Output> {
    match parser::parse(sql)? {
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
        Stmt::Explain { query, analyze } => {
            let mut q = QCtx::explaining(ctx, analyze);
            let start = std::time::Instant::now();
            let out = query::run_query_node(&q, &query, None, 0)?;
            let elapsed = start.elapsed();
            let mut prof = q.prof.take().expect("explaining").into_inner();
            if analyze {
                prof.note(format!(
                    "Execution time: {:.3} ms ({} row(s) returned)",
                    elapsed.as_secs_f64() * 1e3,
                    out.rows.len()
                ));
            }
            let rows = prof.render().into_iter().map(|l| vec![Value::Text(l)]).collect();
            Ok(Output::Rows { columns: vec!["plan".into()], rows })
        }
        Stmt::Analyze(table) => ddl::analyze(ctx, table.as_deref()),
        Stmt::Describe(table) => ddl::describe(ctx, &table),
        Stmt::CheckTable(table) => ddl::check_tables(ctx, table.as_deref()),
        Stmt::Reindex(table) => ddl::reindex(ctx, table.as_deref()),
    }
}
