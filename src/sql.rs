//! The SQL entry point. polydb's SQL dialect is the relational subset of
//! PBSQL, so statements are parsed by the PBSQL parser and run through the
//! shared planner and executor (see [`crate::pbsql`] and [`crate::query`]).

use crate::error::Result;
use crate::storage::Ctx;
use crate::Output;

pub fn execute(ctx: &mut Ctx, sql: &str) -> Result<Output> {
    crate::pbsql::execute(ctx, sql)
}

pub fn tables(ctx: &Ctx) -> Vec<String> {
    crate::rel::tables(ctx)
}
