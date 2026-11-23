//! Recursive-descent parser for the relational part of PBSQL, plus the
//! printer that turns an expression back into SQL text (used to store CHECK
//! constraints). Tokens come from the shared PBSQL lexer (`pbsql::lexer`),
//! so every syntax error reports the line and column it was found at.

use super::ast::*;
use crate::error::{Error, Result};
use crate::value::Value;
use crate::pbsql::lexer::{self, Span, Tok, Token};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

pub(super) fn perr<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Parse(msg.into()))
}

// ===================================================================== parser

const RESERVED: &[&str] = &[
    "where", "join", "inner", "left", "right", "full", "cross", "outer", "on", "group", "order", "by", "limit",
    "offset", "having", "as", "set", "values", "from", "and", "or", "not", "asc", "desc", "union", "intersect",
    "except", "when", "then", "else", "end", "select",
];

/// Limits on one SQL expression. The expression parser, the evaluator and
/// the code that frees an expression tree are all recursive, so an
/// expression that is nested or chained without limit overflows the stack
/// and aborts the process.
/// The values leave a wide margin on the 2 MB stack of a connection thread,
/// including in unoptimised builds, where each evaluator frame is several kilobytes.
pub(super) const MAX_SQL_NESTING: usize = 32; // parentheses, NOT, unary minus, function calls, subqueries
pub(super) const MAX_SQL_OPERATORS: usize = 100; // chained operators plus nested sub-expressions in one expression
/// Subqueries inside subqueries. Each level is a full query execution on the stack.
pub(super) const MAX_SUBQUERY_DEPTH: usize = 6;

static NEXT_SUBQUERY_ID: AtomicUsize = AtomicUsize::new(1);

struct P {
    t: Vec<Tok>,
    /// Byte span of each token in `t`, for error positions.
    spans: Vec<Span>,
    /// Byte offset just past the statement, the position of "end of statement" errors.
    end: usize,
    i: usize,
    /// How many nested sub-expressions the parser is currently inside.
    nest: usize,
    /// An upper bound on how deep the tree of the current top-level
    /// expression is: chained operators plus nested sub-expressions so far.
    ops: usize,
    /// Nesting level at which an expression counts as top-level. Inside a
    /// subquery this is the subquery's level, so each of its clauses starts
    /// from the operator budget already used by the enclosing expression.
    base_nest: usize,
    base_ops: usize,
    /// Deepest budget reached by any top-level expression of the current subquery.
    peak: usize,
    subq: usize,
}

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

impl P {
    fn new(sql: &str) -> Result<P> {
        Ok(P::from_tokens(lexer::tokenize(sql)?, sql.len()))
    }

    fn from_tokens(toks: Vec<Token>, end: usize) -> P {
        let (t, spans) = toks.into_iter().map(|t| (t.tok, t.span)).unzip();
        P { t, spans, end, i: 0, nest: 0, ops: 0, base_nest: 0, base_ops: 0, peak: 0, subq: 0 }
    }

    /// Byte offset of the current token (or of the end of the statement).
    fn here(&self) -> usize {
        self.spans.get(self.i).map_or(self.end, |s| s.start)
    }

    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn found(&self) -> String {
        self.peek().map_or_else(|| "end of statement".into(), Tok::describe)
    }

    fn peek_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w == k)
    }

    fn peek_kw_at(&self, off: usize, k: &str) -> bool {
        matches!(self.t.get(self.i + off), Some(Tok::Word(w)) if w == k)
    }

    fn kw(&mut self, k: &str) -> bool {
        let hit = self.peek_kw(k);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn expect_kw(&mut self, k: &str) -> Result<()> {
        if self.kw(k) {
            Ok(())
        } else {
            perr(format!("expected {}, found {}", k.to_uppercase(), self.found()))
        }
    }

    fn peek_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }

    fn sym(&mut self, s: &str) -> bool {
        let hit = self.peek_sym(s);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.sym(s) {
            Ok(())
        } else {
            perr(format!("expected '{s}', found {}", self.found()))
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::Word(w)) | Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(w)
            }
            _ => perr(format!("expected a name, found {}", self.found())),
        }
    }

    /// `(a, b, c)`
    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect_sym("(")?;
        let mut v = vec![self.ident()?];
        while self.sym(",") {
            v.push(self.ident()?);
        }
        self.expect_sym(")")?;
        Ok(v)
    }

    fn alias(&mut self) -> Result<Option<String>> {
        if self.kw("as") {
            return self.ident().map(Some);
        }
        match self.peek().cloned() {
            Some(Tok::Word(w)) if !RESERVED.contains(&w.as_str()) => {
                self.i += 1;
                Ok(Some(w))
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(Some(w))
            }
            _ => Ok(None),
        }
    }

    fn usize_lit(&mut self) -> Result<usize> {
        match self.peek().cloned() {
            Some(Tok::Int(n)) if n >= 0 => {
                self.i += 1;
                Ok(n as usize)
            }
            _ => perr(format!("expected a non-negative integer, found {}", self.found())),
        }
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn if_exists(&mut self) -> Result<bool> {
        if self.kw("if") {
            self.expect_kw("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ------------------------------------------------------------ statements

    fn statement(&mut self) -> Result<Stmt> {
        if self.kw("select") {
            return self.query().map(Stmt::Query);
        }
        if self.kw("explain") {
            let analyze = self.kw("analyze");
            self.expect_kw("select")?;
            return self.query().map(|query| Stmt::Explain { query, analyze });
        }
        if self.kw("analyze") {
            if self.kw("table") {
                return Ok(Stmt::Analyze(Some(self.ident()?)));
            }
            if self.peek().is_none() {
                return Ok(Stmt::Analyze(None));
            }
            return Ok(Stmt::Analyze(Some(self.ident()?)));
        }
        if self.kw("insert") {
            return self.insert();
        }
        if self.kw("update") {
            let table = self.ident()?;
            self.expect_kw("set")?;
            let mut sets = Vec::new();
            loop {
                let col = self.ident()?;
                self.expect_sym("=")?;
                let e = if self.kw("default") { None } else { Some(self.expr()?) };
                sets.push((col, e));
                if !self.sym(",") {
                    break;
                }
            }
            let filter = if self.kw("where") { Some(self.expr()?) } else { None };
            return Ok(Stmt::Update { table, sets, filter });
        }
        if self.kw("delete") {
            self.expect_kw("from")?;
            let table = self.ident()?;
            let filter = if self.kw("where") { Some(self.expr()?) } else { None };
            return Ok(Stmt::Delete { table, filter });
        }
        if self.kw("create") {
            for what in ["view", "trigger", "schema", "database", "sequence", "function", "procedure", "type", "role", "user"] {
                if self.peek_kw(what) || (self.peek_kw("or") && self.peek_kw_at(2, what)) {
                    return perr(format!("CREATE {} is not supported", what.to_uppercase()));
                }
            }
            let unique = self.kw("unique");
            if unique || self.kw("index") {
                if unique {
                    self.expect_kw("index")?;
                }
                let if_not_exists = self.if_not_exists()?;
                let name = self.ident()?;
                self.expect_kw("on")?;
                let table = self.ident()?;
                let cols = self.ident_list()?;
                return Ok(Stmt::CreateIndex { name, table, cols, unique, if_not_exists });
            }
            self.expect_kw("table")?;
            return self.create_table().map(Stmt::CreateTable);
        }
        if self.kw("drop") {
            if self.kw("index") {
                let if_exists = self.if_exists()?;
                let name = self.ident()?;
                let table = if self.kw("on") { Some(self.ident()?) } else { None };
                return Ok(Stmt::DropIndex { name, table, if_exists });
            }
            self.expect_kw("table")?;
            let if_exists = self.if_exists()?;
            let name = self.ident()?;
            let cascade = if self.kw("cascade") {
                true
            } else {
                self.kw("restrict");
                false
            };
            return Ok(Stmt::DropTable { name, if_exists, cascade });
        }
        if self.kw("alter") {
            self.expect_kw("table")?;
            let table = self.ident()?;
            let mut actions = vec![self.alter_action()?];
            while self.sym(",") {
                actions.push(self.alter_action()?);
            }
            return Ok(Stmt::AlterTable { table, actions });
        }
        if self.kw("truncate") {
            self.kw("table");
            return Ok(Stmt::Truncate(self.ident()?));
        }
        if self.kw("describe") || self.kw("desc") {
            self.kw("table");
            return Ok(Stmt::Describe(self.ident()?));
        }
        if self.kw("check") {
            if self.kw("tables") || self.peek().is_none() {
                return Ok(Stmt::CheckTable(None));
            }
            self.expect_kw("table")?;
            return Ok(Stmt::CheckTable(Some(self.ident()?)));
        }
        if self.kw("reindex") {
            if self.kw("table") {
                return Ok(Stmt::Reindex(Some(self.ident()?)));
            }
            if self.peek().is_none() {
                return Ok(Stmt::Reindex(None));
            }
            return Ok(Stmt::Reindex(Some(self.ident()?)));
        }
        if let Some(Tok::Word(w)) = self.peek() {
            let what = match w.as_str() {
                "with" => Some("WITH (common table expressions)"),
                "merge" => Some("MERGE"),
                "grant" | "revoke" => Some("GRANT / REVOKE (access control)"),
                "savepoint" | "release" => Some("SAVEPOINT"),
                "vacuum" => Some("VACUUM"),
                "copy" => Some("COPY"),
                "upsert" | "replace" => Some("REPLACE / UPSERT"),
                _ => None,
            };
            if let Some(what) = what {
                return perr(format!("{what} is not supported"));
            }
        }
        perr(format!("unsupported SQL statement starting at {}", self.found()))
    }

    fn insert(&mut self) -> Result<Stmt> {
        self.expect_kw("into")?;
        let table = self.ident()?;
        if self.kw("default") {
            self.expect_kw("values")?;
            return Ok(Stmt::Insert { table, cols: None, source: InsertSource::DefaultValues });
        }
        let cols = if self.peek_sym("(") && !self.peek_kw_at(1, "select") { Some(self.ident_list()?) } else { None };
        if self.kw("select") {
            return Ok(Stmt::Insert { table, cols, source: InsertSource::Query(self.query()?) });
        }
        if self.peek_sym("(") && self.peek_kw_at(1, "select") {
            self.i += 2;
            let q = self.query()?;
            self.expect_sym(")")?;
            return Ok(Stmt::Insert { table, cols, source: InsertSource::Query(q) });
        }
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect_sym("(")?;
            let mut row = Vec::new();
            loop {
                row.push(if self.kw("default") { None } else { Some(self.expr()?) });
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            rows.push(row);
            if !self.sym(",") {
                break;
            }
        }
        Ok(Stmt::Insert { table, cols, source: InsertSource::Values(rows) })
    }

    // ----------------------------------------------------------------- DDL

    fn create_table(&mut self) -> Result<TableDef> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let mut cols = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.at_table_constraint() {
                constraints.push(self.table_constraint()?);
            } else {
                let (c, mut cons) = self.column_def()?;
                cols.push(c);
                constraints.append(&mut cons);
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(TableDef { name, cols, constraints, if_not_exists })
    }

    fn at_table_constraint(&self) -> bool {
        ["constraint", "primary", "unique", "check", "foreign"].iter().any(|k| self.peek_kw(k))
    }

    /// `[CONSTRAINT name] PRIMARY KEY (..) | UNIQUE (..) | CHECK (..) | FOREIGN KEY (..) REFERENCES ..`
    fn table_constraint(&mut self) -> Result<Constraint> {
        let name = if self.kw("constraint") { Some(self.ident()?) } else { None };
        if self.kw("primary") {
            self.expect_kw("key")?;
            return Ok(Constraint::PrimaryKey(self.ident_list()?));
        }
        if self.kw("unique") {
            return Ok(Constraint::Unique(name, self.ident_list()?));
        }
        if self.kw("check") {
            return Ok(Constraint::Check(name, self.paren_expr()?));
        }
        if self.kw("foreign") {
            self.expect_kw("key")?;
            let cols = self.ident_list()?;
            self.expect_kw("references")?;
            return self.references(name, cols).map(Constraint::Foreign);
        }
        perr(format!("expected PRIMARY KEY, UNIQUE, CHECK or FOREIGN KEY, found {}", self.found()))
    }

    fn paren_expr(&mut self) -> Result<Expr> {
        self.expect_sym("(")?;
        let e = self.expr()?;
        self.expect_sym(")")?;
        Ok(e)
    }

    /// After `REFERENCES`: `table [(cols)] [ON DELETE action] [ON UPDATE action]`
    fn references(&mut self, name: Option<String>, cols: Vec<String>) -> Result<FkDef> {
        let ref_table = self.ident()?;
        let ref_cols = if self.peek_sym("(") { Some(self.ident_list()?) } else { None };
        let mut fk = FkDef { name, cols, ref_table, ref_cols, on_delete: FkAction::Restrict, on_update: FkAction::Restrict };
        while self.kw("on") {
            let delete = if self.kw("delete") {
                true
            } else {
                self.expect_kw("update")?;
                false
            };
            let action = if self.kw("cascade") {
                FkAction::Cascade
            } else if self.kw("restrict") {
                FkAction::Restrict
            } else if self.kw("no") {
                self.expect_kw("action")?;
                FkAction::Restrict
            } else if self.kw("set") {
                if self.kw("default") {
                    return perr("ON ... SET DEFAULT is not supported; use CASCADE, SET NULL, RESTRICT or NO ACTION");
                }
                self.expect_kw("null")?;
                FkAction::SetNull
            } else {
                return perr(format!("expected CASCADE, SET NULL, RESTRICT or NO ACTION, found {}", self.found()));
            };
            if delete {
                fk.on_delete = action;
            } else {
                fk.on_update = action;
            }
        }
        Ok(fk)
    }

    /// `name type [(n[,m])]` then column constraints. Returns the column and
    /// the constraints that are hoisted to table level (UNIQUE, CHECK,
    /// REFERENCES, PRIMARY KEY).
    fn column_def(&mut self) -> Result<(ColumnDef, Vec<Constraint>)> {
        let name = self.ident()?;
        let (ty, max_len, serial) = self.type_spec()?;
        let mut col = Column { name: name.clone(), ty, not_null: serial, default: None, max_len, auto: serial };
        let mut default = None;
        let mut cons = Vec::new();
        loop {
            let cname = if self.kw("constraint") { Some(self.ident()?) } else { None };
            if self.kw("primary") {
                self.expect_kw("key")?;
                cons.push(Constraint::PrimaryKey(vec![name.clone()]));
            } else if self.kw("not") {
                self.expect_kw("null")?;
                col.not_null = true;
            } else if self.kw("null") {
            } else if self.kw("unique") {
                cons.push(Constraint::Unique(cname, vec![name.clone()]));
            } else if self.kw("default") {
                default = Some(self.default_expr()?);
            } else if self.kw("check") {
                cons.push(Constraint::Check(cname, self.paren_expr()?));
            } else if self.kw("references") {
                cons.push(Constraint::Foreign(self.references(cname, vec![name.clone()])?));
            } else if self.kw("autoincrement") || self.kw("auto_increment") {
                col.auto = true;
            } else {
                if cname.is_some() {
                    return perr(format!("expected a constraint after CONSTRAINT name, found {}", self.found()));
                }
                break;
            }
        }
        Ok((ColumnDef { col, default }, cons))
    }

    /// Type name with optional size: returns (type, max length, is SERIAL).
    fn type_spec(&mut self) -> Result<(Ty, Option<usize>, bool)> {
        let tname = self.ident()?;
        if matches!(tname.as_str(), "serial" | "bigserial" | "smallserial") {
            return Ok((Ty::Int, None, true));
        }
        let Some(ty) = Ty::from_name(&tname) else {
            return perr(format!("unknown column type '{tname}' (use INT, REAL, TEXT or BOOL)"));
        };
        if tname == "double" {
            self.kw("precision");
        }
        if tname == "character" {
            self.kw("varying");
        }
        let mut max_len = None;
        if self.sym("(") {
            let n = self.usize_lit()?;
            if self.sym(",") {
                self.usize_lit()?; // DECIMAL(10,2): scale accepted, not enforced
            }
            self.expect_sym(")")?;
            if matches!(tname.as_str(), "varchar" | "char" | "character") {
                if n == 0 {
                    return perr(format!("{}(0) is not a valid length", tname.to_uppercase()));
                }
                max_len = Some(n);
            }
        }
        Ok((ty, max_len, false))
    }

    /// A DEFAULT value: an additive expression, so that `DEFAULT 0 NOT NULL` parses.
    fn default_expr(&mut self) -> Result<Expr> {
        self.ops = self.base_ops;
        self.enter()?;
        let e = self.add_expr();
        self.nest -= 1;
        e
    }

    fn alter_action(&mut self) -> Result<Alter> {
        if self.kw("add") {
            if self.at_table_constraint() {
                return self.table_constraint().map(Alter::AddConstraint);
            }
            self.kw("column");
            if self.if_not_exists()? {
                return perr("ADD COLUMN IF NOT EXISTS is not supported");
            }
            let (c, cons) = self.column_def()?;
            return Ok(Alter::AddColumn(c, cons));
        }
        if self.kw("drop") {
            if self.kw("constraint") {
                let if_exists = self.if_exists()?;
                return Ok(Alter::DropConstraint { name: self.ident()?, if_exists });
            }
            self.kw("column");
            let if_exists = self.if_exists()?;
            return Ok(Alter::DropColumn { name: self.ident()?, if_exists });
        }
        if self.kw("rename") {
            if self.kw("to") {
                return Ok(Alter::RenameTable(self.ident()?));
            }
            self.kw("column");
            let from = self.ident()?;
            self.expect_kw("to")?;
            return Ok(Alter::RenameColumn { from, to: self.ident()? });
        }
        if self.kw("alter") {
            self.kw("column");
            let col = self.ident()?;
            if self.kw("set") {
                if self.kw("default") {
                    return Ok(Alter::SetDefault(col, Some(self.default_expr()?)));
                }
                if self.kw("not") {
                    self.expect_kw("null")?;
                    return Ok(Alter::SetNotNull(col, true));
                }
                if self.kw("data") {
                    self.expect_kw("type")?;
                    let (ty, len, serial) = self.type_spec()?;
                    if serial {
                        return perr("cannot change a column to SERIAL");
                    }
                    return Ok(Alter::SetType(col, ty, len));
                }
                return perr(format!("expected DEFAULT, NOT NULL or DATA TYPE, found {}", self.found()));
            }
            if self.kw("drop") {
                if self.kw("default") {
                    return Ok(Alter::SetDefault(col, None));
                }
                self.expect_kw("not")?;
                self.expect_kw("null")?;
                return Ok(Alter::SetNotNull(col, false));
            }
            if self.kw("type") {
                let (ty, len, serial) = self.type_spec()?;
                if serial {
                    return perr("cannot change a column to SERIAL");
                }
                return Ok(Alter::SetType(col, ty, len));
            }
            return perr(format!("expected SET, DROP or TYPE, found {}", self.found()));
        }
        perr(format!("expected ADD, DROP, RENAME or ALTER, found {}", self.found()))
    }

    // --------------------------------------------------------------- queries

    /// A query after its first `SELECT` keyword.
    fn query(&mut self) -> Result<Query> {
        let mut head = self.select_core()?;
        let mut rest = Vec::new();
        loop {
            let op = if self.kw("union") {
                SetOp::Union
            } else if self.kw("intersect") {
                SetOp::Intersect
            } else if self.kw("except") {
                SetOp::Except
            } else {
                break;
            };
            let all = self.kw("all");
            if !all {
                self.kw("distinct");
            }
            self.expect_kw("select")?;
            rest.push((op, all, self.select_core()?));
        }
        let mut order_by = Vec::new();
        if self.kw("order") {
            self.expect_kw("by")?;
            loop {
                let expr = self.expr()?;
                let desc = if self.kw("desc") {
                    true
                } else {
                    self.kw("asc");
                    false
                };
                let nulls_first = if self.kw("nulls") {
                    if self.kw("first") {
                        Some(true)
                    } else {
                        self.expect_kw("last")?;
                        Some(false)
                    }
                } else {
                    None
                };
                order_by.push(OrderItem { expr, desc, nulls_first });
                if !self.sym(",") {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = 0;
        if self.kw("limit") {
            limit = if self.kw("all") { None } else { Some(self.usize_lit()?) };
        }
        if self.kw("offset") {
            offset = self.usize_lit()?;
            self.kw("rows");
            self.kw("row");
        }
        if rest.is_empty() {
            head.order_by = order_by;
            head.limit = limit;
            head.offset = offset;
            Ok(Query { head, rest, order_by: Vec::new(), limit: None, offset: 0 })
        } else {
            Ok(Query { head, rest, order_by, limit, offset })
        }
    }

    fn from_item(&mut self) -> Result<FromItem> {
        if self.sym("(") {
            self.expect_kw("select")?;
            let sq = self.subquery_body()?;
            self.expect_sym(")")?;
            let Some(alias) = self.alias()? else {
                return perr("a subquery in FROM needs an alias: (SELECT ...) AS name");
            };
            return Ok(FromItem { src: Source::Derived(sq.q), alias });
        }
        let name = self.ident()?;
        // Cross-model sources: DOCUMENT(collection) and KV(['prefix']).
        if self.sym("(") {
            let (src, default_alias) = match name.as_str() {
                "document" | "documents" => {
                    let coll = self.ident()?;
                    (Source::Collection(coll.clone()), coll)
                }
                "kv" => {
                    let prefix = match self.peek().cloned() {
                        Some(Tok::Str(p)) => {
                            self.i += 1;
                            p
                        }
                        _ if self.peek_sym(")") => String::new(),
                        _ => return perr(format!("KV() takes an optional key prefix string, found {}", self.found())),
                    };
                    (Source::Kv(prefix), "kv".to_string())
                }
                other => {
                    return perr(format!(
                        "'{other}(...)' is not a table source; use a table name, DOCUMENT(collection) or KV('prefix')"
                    ))
                }
            };
            self.expect_sym(")")?;
            let alias = self.alias()?.unwrap_or(default_alias);
            return Ok(FromItem { src, alias });
        }
        let alias = self.alias()?.unwrap_or_else(|| name.clone());
        Ok(FromItem { src: Source::Table(name), alias })
    }

    fn select_core(&mut self) -> Result<Select> {
        let distinct = if self.kw("distinct") {
            true
        } else {
            self.kw("all");
            false
        };
        let mut items = Vec::new();
        loop {
            if self.sym("*") {
                items.push(Item::Star);
            } else if matches!(self.peek(), Some(Tok::Word(_)) | Some(Tok::Ident(_)))
                && self.t.get(self.i + 1) == Some(&Tok::Sym("."))
                && self.t.get(self.i + 2) == Some(&Tok::Sym("*"))
            {
                items.push(Item::TableStar(self.ident()?));
                self.i += 2;
            } else {
                let e = self.expr()?;
                items.push(Item::Expr(e, self.alias()?));
            }
            if !self.sym(",") {
                break;
            }
        }
        let mut s = Select {
            distinct,
            items,
            from: None,
            joins: Vec::new(),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: 0,
        };
        if self.kw("from") {
            s.from = Some(self.from_item()?);
            loop {
                let kind = if self.sym(",") {
                    s.joins.push(Join { item: self.from_item()?, kind: JoinKind::Cross, on: None });
                    continue;
                } else if self.kw("left") {
                    self.kw("outer");
                    JoinKind::Left
                } else if self.kw("right") {
                    self.kw("outer");
                    JoinKind::Right
                } else if self.kw("full") {
                    self.kw("outer");
                    JoinKind::Full
                } else if self.kw("cross") {
                    JoinKind::Cross
                } else if self.kw("inner") || self.peek_kw("join") {
                    JoinKind::Inner
                } else {
                    break;
                };
                self.expect_kw("join")?;
                let item = self.from_item()?;
                let on = if kind == JoinKind::Cross {
                    None
                } else {
                    self.expect_kw("on")?;
                    Some(self.expr()?)
                };
                s.joins.push(Join { item, kind, on });
            }
        }
        if self.kw("where") {
            s.filter = Some(self.expr()?);
        }
        if self.kw("group") {
            self.expect_kw("by")?;
            loop {
                s.group_by.push(self.expr()?);
                if !self.sym(",") {
                    break;
                }
            }
        }
        if self.kw("having") {
            s.having = Some(self.expr()?);
        }
        Ok(s)
    }

    /// A subquery after its `SELECT` keyword. It counts as one nesting level
    /// of the enclosing expression; its own clauses each start from the
    /// enclosing expression's operator budget.
    fn subquery_body(&mut self) -> Result<SubQ> {
        if self.subq >= MAX_SUBQUERY_DEPTH {
            return perr(format!("subqueries are nested deeper than {MAX_SUBQUERY_DEPTH} levels"));
        }
        self.enter()?;
        let saved = (self.base_nest, self.base_ops, self.peak);
        self.base_nest = self.nest;
        self.base_ops = self.ops;
        self.peak = self.ops;
        self.subq += 1;
        let q = self.query();
        let peak = self.peak;
        (self.base_nest, self.base_ops, self.peak) = saved;
        self.subq -= 1;
        self.ops = peak;
        self.nest -= 1;
        Ok(SubQ { id: NEXT_SUBQUERY_ID.fetch_add(1, AtomicOrdering::Relaxed), q: Box::new(q?) })
    }

    // ----------------------------------------------------------- expressions

    // Precedence, loosest first: OR, AND, NOT, comparison, + - ||, * / %, unary -, ::
    fn expr(&mut self) -> Result<Expr> {
        if self.nest == self.base_nest {
            self.ops = self.base_ops;
        }
        self.enter()?;
        let e = self.or_expr();
        self.nest -= 1;
        if self.nest == self.base_nest {
            self.peak = self.peak.max(self.ops);
        }
        e
    }

    /// Go one level deeper; the caller decrements `nest` when it returns.
    /// Anything nested inside the top-level expression also makes the tree
    /// a level deeper, so it counts against the operator budget too.
    fn enter(&mut self) -> Result<()> {
        if self.nest >= MAX_SQL_NESTING {
            return perr(format!("expression is nested deeper than {MAX_SQL_NESTING} levels"));
        }
        self.nest += 1;
        if self.nest > 1 {
            self.op()?;
        }
        Ok(())
    }

    /// Count one level of tree depth: a chained operator or a nested sub-expression.
    fn op(&mut self) -> Result<()> {
        self.ops += 1;
        if self.ops > MAX_SQL_OPERATORS {
            return perr(format!(
                "expression is too complex: more than {MAX_SQL_OPERATORS} operators and nested sub-expressions"
            ));
        }
        Ok(())
    }

    /// Parse one element of a comma-separated list. List elements sit side by
    /// side in the tree, so the list is as deep as its deepest element, not
    /// the sum of them: `base` is the budget used before the list began and
    /// `peak` collects the deepest element seen.
    fn list_item(&mut self, base: usize, peak: &mut usize) -> Result<Expr> {
        self.ops = base;
        let e = self.expr()?;
        *peak = (*peak).max(self.ops);
        Ok(e)
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut l = self.and_expr()?;
        while self.kw("or") {
            self.op()?;
            l = Expr::Bin(bx(l), Op::Or, bx(self.and_expr()?));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut l = self.not_expr()?;
        while self.kw("and") {
            self.op()?;
            l = Expr::Bin(bx(l), Op::And, bx(self.not_expr()?));
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.kw("not") {
            self.enter()?;
            let e = self.not_expr();
            self.nest -= 1;
            Ok(Expr::Not(bx(e?)))
        } else {
            self.cmp_expr()
        }
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let l = self.add_expr()?;
        if self.kw("is") {
            let neg = self.kw("not");
            self.expect_kw("null")?;
            return Ok(Expr::IsNull(bx(l), neg));
        }
        let neg = self.kw("not");
        if self.kw("like") {
            return Ok(Expr::Like(bx(l), bx(self.add_expr()?), neg));
        }
        if self.kw("in") {
            self.expect_sym("(")?;
            if self.kw("select") {
                let sq = self.subquery_body()?;
                self.expect_sym(")")?;
                return Ok(Expr::InSub(bx(l), sq, neg));
            }
            let (base, mut peak) = (self.ops, self.ops);
            let mut list = vec![self.list_item(base, &mut peak)?];
            while self.sym(",") {
                list.push(self.list_item(base, &mut peak)?);
            }
            self.ops = peak;
            self.expect_sym(")")?;
            return Ok(Expr::In(bx(l), list, neg));
        }
        if self.kw("between") {
            let lo = self.add_expr()?;
            self.expect_kw("and")?;
            let hi = self.add_expr()?;
            let e = Expr::Bin(
                bx(Expr::Bin(bx(l.clone()), Op::Ge, bx(lo))),
                Op::And,
                bx(Expr::Bin(bx(l), Op::Le, bx(hi))),
            );
            return Ok(if neg { Expr::Not(bx(e)) } else { e });
        }
        if neg {
            return perr("expected LIKE, IN or BETWEEN after NOT");
        }
        for (s, op) in
            [("=", Op::Eq), ("!=", Op::Ne), ("<>", Op::Ne), ("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)]
        {
            if self.sym(s) {
                return Ok(Expr::Bin(bx(l), op, bx(self.add_expr()?)));
            }
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut l = self.mul_expr()?;
        loop {
            let op = if self.sym("+") {
                Op::Add
            } else if self.sym("-") {
                Op::Sub
            } else if self.sym("||") {
                Op::Concat
            } else {
                return Ok(l);
            };
            self.op()?;
            l = Expr::Bin(bx(l), op, bx(self.mul_expr()?));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = if self.sym("*") {
                Op::Mul
            } else if self.sym("/") {
                Op::Div
            } else if self.sym("%") {
                Op::Mod
            } else {
                return Ok(l);
            };
            self.op()?;
            l = Expr::Bin(bx(l), op, bx(self.unary()?));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.sym("-") {
            self.enter()?;
            let e = self.unary();
            self.nest -= 1;
            Ok(Expr::Neg(bx(e?)))
        } else if self.sym("+") {
            self.enter()?;
            let e = self.unary();
            self.nest -= 1;
            e
        } else {
            let mut e = self.primary()?;
            while self.sym("::") {
                self.op()?;
                let (ty, _, serial) = self.type_spec()?;
                if serial {
                    return perr("cannot cast to SERIAL");
                }
                e = Expr::Cast(bx(e), ty);
            }
            Ok(e)
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let Some(tok) = self.peek().cloned() else {
            return perr("unexpected end of statement");
        };
        self.i += 1;
        match tok {
            Tok::Int(n) => Ok(Expr::Lit(Value::Int(n))),
            Tok::Float(f) => Ok(Expr::Lit(Value::Float(f))),
            Tok::Str(s) => Ok(Expr::Lit(Value::Text(s))),
            Tok::Sym("(") => {
                if self.kw("select") {
                    let sq = self.subquery_body()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::Subquery(sq));
                }
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Word(w) => match w.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                "case" => self.case(),
                "exists" => {
                    self.expect_sym("(")?;
                    self.expect_kw("select")?;
                    let sq = self.subquery_body()?;
                    self.expect_sym(")")?;
                    Ok(Expr::Exists(sq))
                }
                "cast" if self.peek_sym("(") => {
                    self.i += 1;
                    self.enter()?;
                    let e = self.expr();
                    self.nest -= 1;
                    let e = e?;
                    self.expect_kw("as")?;
                    let (ty, _, serial) = self.type_spec()?;
                    if serial {
                        return perr("cannot cast to SERIAL");
                    }
                    self.expect_sym(")")?;
                    Ok(Expr::Cast(bx(e), ty))
                }
                _ if RESERVED.contains(&w.as_str()) => {
                    self.i -= 1;
                    perr(format!("expected an expression, found keyword {}", w.to_uppercase()))
                }
                _ if self.sym("(") => self.call(w),
                _ => self.column(w),
            },
            Tok::Ident(w) => self.column(w),
            _ => {
                self.i -= 1;
                perr(format!("unexpected {}", self.found()))
            }
        }
    }

    /// After `CASE`. Each WHEN/THEN pair sits side by side in the tree.
    fn case(&mut self) -> Result<Expr> {
        self.enter()?;
        let r = self.case_body();
        self.nest -= 1;
        r
    }

    fn case_body(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut peak = base;
        let operand = if self.peek_kw("when") { None } else { Some(bx(self.list_item(base, &mut peak)?)) };
        let mut whens = Vec::new();
        while self.kw("when") {
            let w = self.list_item(base, &mut peak)?;
            self.expect_kw("then")?;
            let t = self.list_item(base, &mut peak)?;
            whens.push((w, t));
        }
        if whens.is_empty() {
            return perr(format!("expected WHEN, found {}", self.found()));
        }
        let els = if self.kw("else") { Some(bx(self.list_item(base, &mut peak)?)) } else { None };
        self.expect_kw("end")?;
        self.ops = peak;
        Ok(Expr::Case(operand, whens, els))
    }

    /// `col`, `alias.col`, or `alias.col.field.field...`: a longer dotted
    /// name reaches into a document value and becomes `get_path(alias.col, 'field.field')`.
    fn column(&mut self, first: String) -> Result<Expr> {
        if !self.sym(".") {
            return Ok(Expr::Col(None, first));
        }
        let col = Expr::Col(Some(first), self.ident()?);
        let mut path = Vec::new();
        while self.sym(".") {
            path.push(match self.peek().cloned() {
                Some(Tok::Int(n)) if n >= 0 => {
                    self.i += 1;
                    n.to_string()
                }
                _ => self.ident()?,
            });
        }
        if path.is_empty() {
            return Ok(col);
        }
        self.op()?;
        Ok(Expr::Func("get_path".into(), vec![col, Expr::Lit(Value::Text(path.join(".")))]))
    }

    fn call(&mut self, name: String) -> Result<Expr> {
        let agg = match name.as_str() {
            "count" => Some(AggFn::Count),
            "sum" => Some(AggFn::Sum),
            "avg" => Some(AggFn::Avg),
            "min" => Some(AggFn::Min),
            "max" => Some(AggFn::Max),
            _ => None,
        };
        if let Some(f) = agg {
            if f == AggFn::Count && self.sym("*") {
                self.expect_sym(")")?;
                return Ok(Expr::Agg(f, None, false));
            }
            let distinct = if self.kw("distinct") {
                true
            } else {
                self.kw("all");
                false
            };
            let arg = self.expr()?;
            self.expect_sym(")")?;
            return Ok(Expr::Agg(f, Some(bx(arg)), distinct));
        }
        let mut args = Vec::new();
        if !self.sym(")") {
            let (base, mut peak) = (self.ops, self.ops);
            loop {
                args.push(self.list_item(base, &mut peak)?);
                if !self.sym(",") {
                    break;
                }
            }
            self.ops = peak;
            self.expect_sym(")")?;
        }
        Ok(Expr::Func(name, args))
    }

    fn finish(&mut self) -> Result<()> {
        self.sym(";");
        if self.peek().is_some() {
            return perr(format!("unexpected {} after the end of the statement", self.found()));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn parse(sql: &str) -> Result<Stmt> {
    parse_tokens(sql, lexer::tokenize(sql)?)
}

/// Parse one statement from tokens of `src` (the whole command text, so
/// that error positions are relative to what the user typed).
pub(crate) fn parse_tokens(src: &str, toks: Vec<Token>) -> Result<Stmt> {
    let mut p = P::from_tokens(toks, src.trim_end().len());
    let res = p.statement().and_then(|st| p.finish().map(|_| st));
    res.map_err(|e| lexer::locate(e, src, p.here()))
}

/// Parse a lone expression (stored CHECK constraints).
pub(super) fn parse_expr(sql: &str) -> Result<Expr> {
    let mut p = P::new(sql)?;
    let e = p.expr()?;
    p.finish()?;
    Ok(e)
}

// ==================================================================== printer

pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn lit_sql(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) if *i == i64::MIN => "(-9223372036854775807 - 1)".into(),
        Value::Int(i) if *i < 0 => format!("({i})"),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if *f < 0.0 => format!("({f:?})"),
        Value::Float(f) => format!("{f:?}"),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_json().replace('\'', "''")),
    }
}

/// SQL text for an expression, fully parenthesised, that parses back to the
/// same tree. Subqueries are not printable (CHECK constraints refuse them).
pub(super) fn to_sql(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => lit_sql(v),
        Expr::Col(Some(q), n) => format!("{}.{}", quote_ident(q), quote_ident(n)),
        Expr::Col(None, n) => quote_ident(n),
        Expr::Bin(a, op, b) => format!("({} {} {})", to_sql(a), op.text().to_uppercase(), to_sql(b)),
        Expr::Not(a) => format!("(NOT {})", to_sql(a)),
        Expr::Neg(a) => format!("(- {})", to_sql(a)),
        Expr::IsNull(a, neg) => format!("({} IS {}NULL)", to_sql(a), if *neg { "NOT " } else { "" }),
        Expr::Like(a, p, neg) => format!("({} {}LIKE {})", to_sql(a), if *neg { "NOT " } else { "" }, to_sql(p)),
        Expr::In(a, list, neg) => format!(
            "({} {}IN ({}))",
            to_sql(a),
            if *neg { "NOT " } else { "" },
            list.iter().map(to_sql).collect::<Vec<_>>().join(", ")
        ),
        // display only: CHECK constraints, the one stored use, cannot hold subqueries
        Expr::InSub(a, _, neg) => format!("({} {}IN (SELECT ...))", to_sql(a), if *neg { "NOT " } else { "" }),
        Expr::Exists(..) => "EXISTS (SELECT ...)".into(),
        Expr::Subquery(..) => "(SELECT ...)".into(),
        Expr::Agg(f, arg, distinct) => format!(
            "{}({}{})",
            f.name(),
            if *distinct { "DISTINCT " } else { "" },
            arg.as_ref().map_or("*".to_string(), |a| to_sql(a))
        ),
        Expr::Func(n, args) => format!("{n}({})", args.iter().map(to_sql).collect::<Vec<_>>().join(", ")),
        Expr::Cast(a, ty) => format!("CAST({} AS {})", to_sql(a), ty.name()),
        Expr::Case(op, whens, els) => {
            let mut s = String::from("(CASE");
            if let Some(o) = op {
                s += &format!(" {}", to_sql(o));
            }
            for (w, t) in whens {
                s += &format!(" WHEN {} THEN {}", to_sql(w), to_sql(t));
            }
            if let Some(e) = els {
                s += &format!(" ELSE {}", to_sql(e));
            }
            s + " END)"
        }
    }
}
