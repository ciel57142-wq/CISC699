//! Shared query infrastructure used by SQL, PBSQL, and the document and
//! key-value front ends: expression evaluation, planning and execution.

pub mod eval;
pub mod exec;
pub mod plan;
