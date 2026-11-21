//! PBSQL: PolyDB's unified native query language.
//!
//! One grammar reaches all three data models:
//!
//! ```text
//! SELECT u.name, p.profile.city, s.value
//! FROM RELATIONAL.users AS u
//! JOIN DOCUMENT.profiles AS p ON p.user_id = u.id
//! LEFT JOIN KV.sessions AS s ON s.key = CONCAT('session:', u.id)
//! WHERE u.age >= 18 ORDER BY u.name LIMIT 10;
//!
//! INSERT INTO DOCUMENT.users VALUES {"name": "Alice", "profile": {"city": "Berlin"}};
//! SET KV 'user:1' = '{"name":"Alice"}';     GET FROM KV 'user:1';
//! EXPLAIN ANALYZE SELECT * FROM users WHERE age >= 25;
//! ```
//!
//! Pipeline: [`lexer`] -> [`parser`] -> [`ast`] -> binding/validation and
//! planning in [`crate::query::plan`] -> execution in [`crate::query::exec`].
//! Statements that change data run through [`exec`] on the shared
//! transaction context, so PBSQL statements join any open transaction.

pub mod ast;
pub mod exec;
pub mod lexer;
pub mod parser;

pub use exec::execute;
