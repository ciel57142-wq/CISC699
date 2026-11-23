//! The PBSQL statement parser.
//!
//! The first token decides which part of the grammar applies; from there
//! each part is a recursive-descent parser over the shared lexer:
//!
//! ```text
//! statement   := explain | transaction | utility | kv | document | sql
//! explain     := EXPLAIN [ANALYZE] statement
//! transaction := (BEGIN | START) [TRANSACTION | WORK]
//!              | (COMMIT | END) [TRANSACTION | WORK]
//!              | (ROLLBACK | ABORT) [TRANSACTION | WORK]
//! utility     := SHOW (TABLES | COLLECTIONS) | CHECKPOINT
//! kv          := SET key value | GET key | DEL key | EXISTS key
//!              | KEYS [prefix] | INCR key [integer] | DECR key [integer]
//! document    := db . collection . method ( [json {, json}] ) { . modifier ( json ) }
//! sql         := the relational grammar (sql::parser), see docs/PBSQL.md
//! ```

use super::ast::*;
use super::lexer::{position, Lexer, Tok, Token};
use crate::error::{Error, Result};
use crate::sql;
use crate::value::{JsonParser, Value};

const SQL_STARTS: &[&str] = &[
    "select", "insert", "update", "delete", "create", "drop", "alter", "truncate", "describe", "desc", "check",
    "reindex", // recognised so that they fail with a precise "not supported" message:
    "with", "merge", "grant", "revoke", "savepoint", "release", "vacuum", "analyze", "copy", "replace", "upsert",
];

/// Parse one PBSQL statement.
pub(crate) fn parse(src: &str) -> Result<Statement> {
    statement_at(src, 0, false)
}

fn err_at<T>(src: &str, at: usize, msg: impl Into<String>) -> Result<T> {
    Err(Error::Parse(format!("{}{}", msg.into(), position(src, at))))
}

fn statement_at(src: &str, pos: usize, nested: bool) -> Result<Statement> {
    let mut lx = Lexer::at(src, pos);
    let Some(first) = lx.next_token()? else {
        return err_at(src, src.len(), if nested { "EXPLAIN needs a statement to explain" } else { "empty command" });
    };
    let word = match &first.tok {
        Tok::Word(w) => w.clone(),
        other => {
            return err_at(src, first.span.start, format!("a statement cannot start with {}", other.describe()));
        }
    };
    match word.as_str() {
        "explain" => {
            if nested {
                return err_at(src, first.span.start, "EXPLAIN cannot be nested");
            }
            let save = lx.pos();
            let analyze = matches!(lx.next_token()?, Some(Token { tok: Tok::Word(w), .. }) if w == "analyze");
            if !analyze {
                lx.set_pos(save);
            }
            let inner_at = lx.pos();
            let stmt = statement_at(src, inner_at, true)?;
            check_explainable(&stmt, src, inner_at)?;
            Ok(Statement::Explain { analyze, stmt: Box::new(stmt) })
        }
        "db" if peek_sym(&mut lx, ".")? => doc_statement(&mut lx),
        "set" | "get" | "del" | "exists" | "keys" | "incr" | "decr" => kv_statement(&mut lx, &word, first.span.start),
        "begin" | "start" | "commit" | "end" | "rollback" | "abort" => {
            let op = match word.as_str() {
                "begin" => TxnOp::Begin,
                "start" => {
                    expect_word(&mut lx, "transaction")?;
                    TxnOp::Begin
                }
                "commit" | "end" => TxnOp::Commit,
                _ => TxnOp::Rollback,
            };
            if word != "start" {
                optional_word(&mut lx, &["transaction", "work"])?;
            }
            finish(&mut lx)?;
            Ok(Statement::Txn(op))
        }
        "show" => {
            let what = match lx.next_token()? {
                Some(Token { tok: Tok::Word(w), .. }) if w == "tables" => ShowWhat::Tables,
                Some(Token { tok: Tok::Word(w), .. }) if w == "collections" => ShowWhat::Collections,
                other => {
                    let at = other.map_or(src.len(), |t| t.span.start);
                    return err_at(src, at, "use SHOW TABLES or SHOW COLLECTIONS");
                }
            };
            finish(&mut lx)?;
            Ok(Statement::Show(what))
        }
        "checkpoint" => {
            finish(&mut lx)?;
            Ok(Statement::Checkpoint)
        }
        w if SQL_STARTS.contains(&w) => {
            let toks = Lexer::at(src, first.span.start).tokenize()?;
            sql::parser::parse_tokens(src, toks).map(Statement::Sql)
        }
        other => err_at(
            src,
            first.span.start,
            format!(
                "unknown command '{other}': expected SQL, a db.<collection>.<method>(...) call, \
                 SET/GET/DEL/EXISTS/KEYS/INCR/DECR, or BEGIN/COMMIT/ROLLBACK"
            ),
        ),
    }
}

/// EXPLAIN shows how data is reached, so it applies to statements that read or change rows.
fn check_explainable(stmt: &Statement, src: &str, at: usize) -> Result<()> {
    use crate::sql::ast::Stmt;
    let ok = match stmt {
        Statement::Sql(s) => matches!(s, Stmt::Query(_) | Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }),
        Statement::Doc(d) => !matches!(d.op, DocOp::CreateIndex(_) | DocOp::Drop),
        Statement::Kv(_) => true,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        err_at(src, at, format!("EXPLAIN cannot be applied to a {} statement of this kind", stmt.kind()))
    }
}

/// Is the next token the symbol `s`? Text the SQL lexer cannot read (the
/// start of a JSON literal, say) is simply "not that symbol".
fn peek_sym(lx: &mut Lexer, s: &str) -> Result<bool> {
    let save = lx.pos();
    let hit = matches!(lx.next_token(), Ok(Some(Token { tok: Tok::Sym(x), .. })) if x == s);
    lx.set_pos(save);
    Ok(hit)
}

fn expect_sym(lx: &mut Lexer, s: &str) -> Result<()> {
    match lx.next_token()? {
        Some(Token { tok: Tok::Sym(x), .. }) if x == s => Ok(()),
        other => unexpected(lx, other, &format!("'{s}'")),
    }
}

fn expect_word(lx: &mut Lexer, w: &str) -> Result<()> {
    match lx.next_token()? {
        Some(Token { tok: Tok::Word(x), .. }) if x == w => Ok(()),
        other => unexpected(lx, other, &w.to_uppercase()),
    }
}

fn optional_word(lx: &mut Lexer, words: &[&str]) -> Result<()> {
    let save = lx.pos();
    match lx.next_token()? {
        Some(Token { tok: Tok::Word(x), .. }) if words.contains(&x.as_str()) => Ok(()),
        _ => {
            lx.set_pos(save);
            Ok(())
        }
    }
}

fn unexpected<T>(lx: &Lexer, found: Option<Token>, wanted: &str) -> Result<T> {
    let src = lx.src();
    match found {
        Some(t) => err_at(src, t.span.start, format!("expected {wanted}, found {}", t.tok.describe())),
        None => err_at(src, src.trim_end().len(), format!("expected {wanted}, found end of statement")),
    }
}

/// Nothing but an optional `;` may follow.
fn finish(lx: &mut Lexer) -> Result<()> {
    let save = lx.pos();
    match lx.next_token()? {
        None => Ok(()),
        Some(Token { tok: Tok::Sym(";"), .. }) => match lx.next_token()? {
            None => Ok(()),
            Some(t) => err_at(lx.src(), t.span.start, "only one statement is allowed per command"),
        },
        Some(t) => {
            lx.set_pos(save);
            err_at(lx.src(), t.span.start, format!("unexpected {} after the end of the statement", t.tok.describe()))
        }
    }
}

// ---------------------------------------------------------------- key-value

/// Key-value text is literal: no comments, case kept, `--` is ordinary text.
fn kv_end(lx: &mut Lexer) -> Result<()> {
    let (rest, span) = lx.rest();
    if rest.is_empty() {
        Ok(())
    } else {
        err_at(lx.src(), span.start, format!("unexpected extra argument '{rest}'"))
    }
}

fn kv_statement(lx: &mut Lexer, word: &str, start: usize) -> Result<Statement> {
    let src = lx.src();
    if word == "keys" {
        let prefix = lx.raw_word()?.map(|(p, _)| p).unwrap_or_default();
        kv_end(lx)?;
        return Ok(Statement::Kv(KvStmt::Keys(prefix)));
    }
    let Some((key, _)) = lx.raw_word()? else {
        let what = if word == "set" { "a key and a value" } else { "a key" };
        return err_at(src, src.trim_end().len(), format!("{} needs {what}", word.to_uppercase()));
    };
    let stmt = match word {
        "set" => {
            let save = lx.pos();
            let (rest, _) = lx.rest();
            if rest.is_empty() {
                return err_at(src, src.trim_end().len(), "SET needs a key and a value");
            }
            // A quoted value is unescaped; anything else is taken verbatim to the end.
            let value = if rest.starts_with('"') || rest.starts_with('\'') {
                lx.set_pos(save);
                let (v, _) = lx.raw_word()?.expect("a quoted value is present");
                kv_end(lx)?;
                v
            } else {
                rest
            };
            KvStmt::Set { key, value }
        }
        "get" => KvStmt::Get(key),
        "del" => KvStmt::Del(key),
        "exists" => KvStmt::Exists(key),
        _ => {
            let by = match lx.raw_word()? {
                None => 1,
                Some((n, span)) => match n.parse::<i64>() {
                    Ok(n) => n,
                    Err(_) => return err_at(src, span.start, format!("{} amount must be an integer", word.to_uppercase())),
                },
            };
            let by = if word == "decr" {
                by.checked_neg().ok_or_else(|| Error::Parse(format!("DECR amount is out of range{}", position(src, start))))?
            } else {
                by
            };
            KvStmt::Incr { key, by }
        }
    };
    if !matches!(stmt, KvStmt::Set { .. }) {
        kv_end(lx)?;
    }
    Ok(Statement::Kv(stmt))
}

// ----------------------------------------------------------------- document

/// One argument: a JSON value (relaxed JSON: bare keys, single quotes),
/// read by the JSON parser at the lexer's position.
fn json_arg(lx: &mut Lexer) -> Result<Value> {
    lx.skip_trivia()?;
    let src = lx.src();
    let mut jp = JsonParser::at(src, lx.pos());
    let v = jp.value().map_err(|e| json_error(e, src))?;
    lx.set_pos(jp.pos);
    Ok(v)
}

/// JSON errors say "at byte N"; restate that as a line and column.
fn json_error(e: Error, src: &str) -> Error {
    match e {
        Error::Parse(m) => match m.rsplit_once(" at byte ") {
            Some((msg, n)) if n.parse::<usize>().is_ok() => {
                Error::Parse(format!("{msg}{}", position(src, n.parse().unwrap_or(0))))
            }
            _ => Error::Parse(m),
        },
        other => other,
    }
}

/// `name(arg, arg, ...)` after the dot.
fn call(lx: &mut Lexer) -> Result<(String, Vec<Value>, usize)> {
    let src = lx.src();
    let (name, at) = match lx.next_token()? {
        Some(Token { tok: Tok::Word(w), span }) => (w, span.start),
        other => return unexpected(lx, other, "a method name"),
    };
    if !peek_sym(lx, "(")? {
        return err_at(src, lx.pos(), format!("expected '(' after {name}"));
    }
    expect_sym(lx, "(")?;
    let mut args = Vec::new();
    if peek_sym(lx, ")")? {
        expect_sym(lx, ")")?;
        return Ok((name, args, at));
    }
    loop {
        args.push(json_arg(lx)?);
        match lx.next_token()? {
            Some(Token { tok: Tok::Sym(","), .. }) => continue,
            Some(Token { tok: Tok::Sym(")"), .. }) => break,
            other => return unexpected(lx, other, "',' or ')'"),
        }
    }
    Ok((name, args, at))
}

fn doc_statement(lx: &mut Lexer) -> Result<Statement> {
    let src = lx.src();
    expect_sym(lx, ".")?;
    // The collection name is case-sensitive: read it from the source text.
    let collection = match lx.next_token()? {
        Some(Token { tok: Tok::Word(_), span }) => src[span.start..span.end].to_string(),
        Some(Token { tok: Tok::Ident(name), .. }) => name,
        other => return unexpected(lx, other, "a collection name after 'db.'"),
    };
    expect_sym(lx, ".")?;
    let (method, args, at) = call(lx)?;
    let mut mods = Vec::new();
    while peek_sym(lx, ".")? {
        expect_sym(lx, ".")?;
        mods.push(call(lx)?);
    }
    finish(lx)?;

    let empty = || Value::Object(Default::default());
    let arg = |i: usize| args.get(i).cloned().unwrap_or_else(empty);
    let need = |i: usize, what: &str| -> Result<Value> {
        args.get(i).cloned().ok_or_else(|| Error::Parse(format!("{method}() needs {what}{}", position(src, at))))
    };
    let at_most = |n: usize| -> Result<()> {
        if args.len() > n {
            return err_at(src, at, format!("{method}() takes at most {n} argument(s), got {}", args.len()));
        }
        Ok(())
    };
    let is_find = matches!(method.as_str(), "find" | "findone");
    if !is_find {
        if let Some((_, _, mat)) = mods.first() {
            return err_at(src, *mat, format!("{method}() cannot be chained"));
        }
    }
    let op = match method.as_str() {
        "insert" | "insertone" | "insertmany" => {
            at_most(1)?;
            DocOp::Insert(need(0, "a document")?)
        }
        "find" | "findone" => {
            // A second argument (a projection) is accepted and ignored, as before.
            at_most(2)?;
            let (mut sort, mut skip, mut limit) = (None, 0, None);
            for (name, a, mat) in &mods {
                if !matches!(name.as_str(), "sort" | "limit" | "skip") {
                    return err_at(src, *mat, format!("unknown modifier .{name}(); use sort, limit or skip"));
                }
                let Some(first) = a.first() else {
                    return err_at(src, *mat, format!("{name}() needs an argument"));
                };
                if a.len() > 1 {
                    return err_at(src, *mat, format!("{name}() takes one argument"));
                }
                let count = || -> Result<usize> {
                    first
                        .as_i64()
                        .filter(|n| *n >= 0)
                        .map(|n| n as usize)
                        .ok_or_else(|| Error::Parse(format!("expected a non-negative integer{}", position(src, *mat))))
                };
                match name.as_str() {
                    "sort" => sort = Some(first.clone()),
                    "limit" => limit = Some(count()?),
                    _ => skip = count()?,
                }
            }
            let one = method == "findone";
            if one {
                limit = Some(1);
            }
            DocOp::Find { filter: arg(0), sort, skip, limit, one }
        }
        "update" | "updatemany" | "updateone" => {
            at_most(2)?;
            DocOp::Update {
                filter: need(0, "a filter and an update")?,
                update: need(1, "a filter and an update")?,
                multi: method != "updateone",
            }
        }
        "delete" | "deletemany" | "deleteone" | "remove" => {
            at_most(1)?;
            DocOp::Delete { filter: need(0, "a filter ({} deletes everything)")?, one: method == "deleteone" }
        }
        "createindex" => {
            at_most(1)?;
            let field = match need(0, "a field, e.g. createIndex({email: 1})")? {
                Value::Text(f) => f,
                Value::Object(o) if o.len() == 1 => o.keys().next().cloned().unwrap_or_default(),
                _ => return err_at(src, at, "createIndex takes one field: createIndex({email: 1})"),
            };
            DocOp::CreateIndex(field)
        }
        "count" | "countdocuments" => {
            at_most(1)?;
            DocOp::Count(arg(0))
        }
        "drop" => {
            at_most(0)?;
            DocOp::Drop
        }
        other => {
            return err_at(
                src,
                at,
                format!("unknown collection method '{other}': use insert, find, update, delete, count, createIndex or drop"),
            )
        }
    };
    Ok(Statement::Doc(DocStmt { collection, op }))
}
