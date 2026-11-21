//! Recursive-descent parser for PBSQL.
//!
//! The grammar is conventional SQL extended with model-qualified names
//! (`RELATIONAL.t`, `DOCUMENT.c`, `KV`, `KV.ns`), dotted document paths,
//! inline JSON literals, key-value statements and EXPLAIN [ANALYZE].
//! Plain polydb SQL is exactly the subset that uses unqualified names.

use super::ast::*;
use super::lexer::{lex, perr, Tok};
use crate::error::Result;
use crate::value::Value;

const RESERVED: &[&str] = &[
    "where", "join", "inner", "left", "outer", "cross", "on", "group", "order", "by", "limit",
    "offset", "having", "as", "set", "values", "from", "and", "or", "not", "asc", "desc", "union",
    "select", "when", "then", "else", "end", "case", "in", "is", "like", "between", "exists",
    "all", "distinct",
];

/// Limits on one expression. The parser, binder and evaluator are recursive,
/// so unbounded nesting would overflow the stack and abort the process.
pub const MAX_SQL_NESTING: usize = 32; // parentheses, NOT, unary minus, function calls, subqueries
pub const MAX_SQL_OPERATORS: usize = 100; // chained operators plus nested sub-expressions in one expression

pub struct Parser {
    t: Vec<Tok>,
    i: usize,
    nest: usize,
    ops: usize,
}

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

/// Parse exactly one statement (a trailing `;` is allowed).
pub fn parse(src: &str) -> Result<Stmt> {
    let mut p = Parser {
        t: lex(src)?,
        i: 0,
        nest: 0,
        ops: 0,
    };
    let st = p.statement()?;
    p.sym(";");
    if p.peek().is_some() {
        return perr(format!(
            "unexpected {} after the end of the statement",
            p.found()
        ));
    }
    Ok(st)
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k)
    }

    fn found(&self) -> String {
        match self.peek() {
            None => "end of statement".into(),
            Some(Tok::Word { orig, .. }) => format!("'{orig}'"),
            Some(Tok::Ident(w)) => format!("'{w}'"),
            Some(Tok::Int(n)) => n.to_string(),
            Some(Tok::Float(f)) => f.to_string(),
            Some(Tok::Str(s)) => format!("'{s}'"),
            Some(Tok::Json(v)) => v.to_json(),
            Some(Tok::Sym(s)) => format!("'{s}'"),
        }
    }

    fn word_is(t: Option<&Tok>, k: &str) -> bool {
        matches!(t, Some(Tok::Word { lower, .. }) if lower == k)
    }

    fn peek_kw(&self, k: &str) -> bool {
        Self::word_is(self.peek(), k)
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
            perr(format!(
                "expected {}, found {}",
                k.to_uppercase(),
                self.found()
            ))
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

    /// A name, lower-cased unless quoted (relational names are case-insensitive).
    fn ident(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::Word { lower, .. }) => {
                self.i += 1;
                Ok(lower)
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(w)
            }
            _ => perr(format!("expected a name, found {}", self.found())),
        }
    }

    /// A name with its original case (document fields and collections).
    fn ident_orig(&mut self) -> Result<String> {
        match self.peek().cloned() {
            Some(Tok::Word { orig, .. }) => {
                self.i += 1;
                Ok(orig)
            }
            Some(Tok::Ident(w)) => {
                self.i += 1;
                Ok(w)
            }
            _ => perr(format!("expected a name, found {}", self.found())),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect_sym("(")?;
        let mut c = vec![self.ident()?];
        while self.sym(",") {
            c.push(self.ident()?);
        }
        self.expect_sym(")")?;
        Ok(c)
    }

    fn alias(&mut self) -> Result<Option<String>> {
        if self.kw("as") {
            return self.ident().map(Some);
        }
        match self.peek().cloned() {
            Some(Tok::Word { lower, .. }) if !RESERVED.contains(&lower.as_str()) => {
                self.i += 1;
                Ok(Some(lower))
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
            _ => perr(format!(
                "expected a non-negative integer, found {}",
                self.found()
            )),
        }
    }

    /// `name`, `RELATIONAL.name`, `DOCUMENT.coll`, `DOCUMENT.ns.coll`, `KV`, `KV.ns`.
    fn object_name(&mut self, default: Model) -> Result<ObjectName> {
        let model = match self.peek() {
            Some(Tok::Word { lower, .. }) => match lower.as_str() {
                "relational" if self.peek_at(1) == Some(&Tok::Sym(".")) => Some(Model::Relational),
                "document" | "doc" if self.peek_at(1) == Some(&Tok::Sym(".")) => {
                    Some(Model::Document)
                }
                "kv" => Some(Model::Kv),
                _ => None,
            },
            _ => None,
        };
        let Some(model) = model else {
            return Ok(ObjectName {
                model: default,
                name: self.ident()?,
            });
        };
        self.i += 1;
        match model {
            Model::Kv => {
                let name = if self.peek_sym(".")
                    && matches!(self.peek_at(1), Some(Tok::Word { .. } | Tok::Ident(_)))
                {
                    self.i += 1;
                    self.ident()?
                } else {
                    String::new()
                };
                Ok(ObjectName { model, name })
            }
            Model::Relational => {
                self.expect_sym(".")?;
                Ok(ObjectName {
                    model,
                    name: self.ident()?,
                })
            }
            Model::Document => {
                self.expect_sym(".")?;
                let mut name = self.ident_orig()?;
                if self.peek_sym(".")
                    && matches!(self.peek_at(1), Some(Tok::Word { .. } | Tok::Ident(_)))
                {
                    self.i += 1;
                    name = format!("{name}.{}", self.ident_orig()?);
                }
                Ok(ObjectName { model, name })
            }
        }
    }

    fn statement(&mut self) -> Result<Stmt> {
        if self.kw("explain") {
            let analyze = self.kw("analyze");
            let stmt = self.statement()?;
            return Ok(Stmt::Explain {
                analyze,
                stmt: Box::new(stmt),
            });
        }
        if self.peek_kw("select") || self.peek_sym("(") {
            return self.query().map(Stmt::Query);
        }
        if self.kw("insert") {
            self.expect_kw("into")?;
            let target = self.object_name(Model::Relational)?;
            let cols = if self.peek_sym("(") && !Self::word_is(self.peek_at(1), "select") {
                Some(self.ident_list()?)
            } else {
                None
            };
            if self.peek_kw("select") || self.peek_sym("(") {
                return Ok(Stmt::Insert {
                    target,
                    cols,
                    source: InsertSource::Query(Box::new(self.query()?)),
                });
            }
            self.expect_kw("values")?;
            let mut rows = Vec::new();
            loop {
                if let Some(Tok::Json(v)) = self.peek().cloned() {
                    // a document literal is a one-column row
                    self.i += 1;
                    rows.push(vec![Expr::Lit(v)]);
                } else {
                    self.expect_sym("(")?;
                    let mut row = vec![self.value_expr()?];
                    while self.sym(",") {
                        row.push(self.value_expr()?);
                    }
                    self.expect_sym(")")?;
                    rows.push(row);
                }
                if !self.sym(",") {
                    break;
                }
            }
            return Ok(Stmt::Insert {
                target,
                cols,
                source: InsertSource::Values(rows),
            });
        }
        if self.kw("update") {
            let target = self.object_name(Model::Relational)?;
            let alias = self.alias()?.unwrap_or_else(|| default_alias(&target));
            self.expect_kw("set")?;
            let mut sets = Vec::new();
            loop {
                let mut path = vec![self.ident_orig()?];
                while self.sym(".") {
                    path.push(self.ident_orig()?);
                }
                self.expect_sym("=")?;
                sets.push((path, self.expr()?));
                if !self.sym(",") {
                    break;
                }
            }
            let filter = if self.kw("where") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Stmt::Update {
                target,
                alias,
                sets,
                filter,
            });
        }
        if self.kw("delete") {
            self.expect_kw("from")?;
            let target = self.object_name(Model::Relational)?;
            let alias = self.alias()?.unwrap_or_else(|| default_alias(&target));
            let filter = if self.kw("where") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Stmt::Delete {
                target,
                alias,
                filter,
            });
        }
        if self.kw("create") {
            return self.create();
        }
        if self.kw("drop") {
            if self.kw("index") {
                let name = self.ident()?;
                self.expect_kw("on")?;
                return Ok(Stmt::DropIndex {
                    name,
                    table: self.ident()?,
                });
            }
            self.expect_kw("table")?;
            let if_exists = if self.kw("if") {
                self.expect_kw("exists")?;
                true
            } else {
                false
            };
            return Ok(Stmt::DropTable {
                name: self.ident()?,
                if_exists,
            });
        }
        if self.kw("alter") {
            self.expect_kw("table")?;
            let table = self.ident()?;
            let op = if self.kw("add") {
                self.kw("column");
                AlterOp::AddColumn(self.column_def()?)
            } else if self.kw("drop") {
                self.kw("column");
                AlterOp::DropColumn(self.ident()?)
            } else if self.kw("rename") {
                if self.kw("to") {
                    AlterOp::RenameTable(self.ident()?)
                } else {
                    self.kw("column");
                    let from = self.ident()?;
                    self.expect_kw("to")?;
                    AlterOp::RenameColumn(from, self.ident()?)
                }
            } else {
                return perr(format!(
                    "expected ADD, DROP or RENAME after ALTER TABLE, found {}",
                    self.found()
                ));
            };
            return Ok(Stmt::AlterTable { table, op });
        }
        // key-value statements
        if self.kw("get") {
            self.expect_kw("from")?;
            let ns = self.kv_name()?;
            return Ok(Stmt::KvGet {
                ns,
                key: self.expr()?,
            });
        }
        if self.peek_kw("set") && Self::word_is(self.peek_at(1), "kv") {
            self.i += 1;
            let ns = self.kv_name()?;
            let key = self.add_expr_top()?;
            self.expect_sym("=")?;
            return Ok(Stmt::KvSet {
                ns,
                key,
                value: self.expr()?,
            });
        }
        if self.peek_kw("exists") && Self::word_is(self.peek_at(1), "kv") {
            self.i += 1;
            let ns = self.kv_name()?;
            return Ok(Stmt::KvExists {
                ns,
                key: self.expr()?,
            });
        }
        if self.kw("increment") {
            self.expect_kw("kv")?;
            let ns = self.kv_name_after_kw()?;
            let key = self.add_expr_top()?;
            let by = if self.kw("by") {
                self.expr()?
            } else {
                Expr::Lit(Value::Int(1))
            };
            return Ok(Stmt::KvIncr { ns, key, by });
        }
        perr(format!(
            "unsupported statement starting at {}",
            self.found()
        ))
    }

    /// After `KV` has been consumed (or not, for `INCREMENT 'k'`): optional `.ns`.
    fn kv_name_after_kw(&mut self) -> Result<String> {
        if self.peek_sym(".") && matches!(self.peek_at(1), Some(Tok::Word { .. } | Tok::Ident(_))) {
            self.i += 1;
            return self.ident();
        }
        Ok(String::new())
    }

    /// `KV` or `KV.ns`.
    fn kv_name(&mut self) -> Result<String> {
        let o = self.object_name(Model::Relational)?;
        if o.model != Model::Kv {
            return perr("expected KV or KV.<namespace>");
        }
        Ok(o.name)
    }

    fn add_expr_top(&mut self) -> Result<Expr> {
        if self.nest == 0 {
            self.ops = 0;
        }
        self.enter()?;
        let e = self.add_expr();
        self.nest -= 1;
        e
    }

    fn value_expr(&mut self) -> Result<Expr> {
        if self.kw("default") {
            return Ok(Expr::Default);
        }
        self.expr()
    }

    fn create(&mut self) -> Result<Stmt> {
        let unique = self.kw("unique");
        if unique || self.peek_kw("index") {
            self.expect_kw("index")?;
            let if_not_exists = self.if_not_exists()?;
            let name = self.ident()?;
            self.expect_kw("on")?;
            let target = self.object_name(Model::Relational)?;
            self.expect_sym("(")?;
            let mut cols = Vec::new();
            loop {
                // document field paths keep their case; relational columns are lower-cased later
                let mut path = vec![self.ident_orig()?];
                while self.sym(".") {
                    path.push(self.ident_orig()?);
                }
                cols.push(path.join("."));
                // CREATE INDEX ... (col ASC|DESC): accepted, order is always ascending
                if !self.kw("asc") {
                    self.kw("desc");
                }
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return Ok(Stmt::CreateIndex {
                name,
                target,
                cols,
                unique,
                if_not_exists,
            });
        }
        self.expect_kw("table")?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let mut cols: Vec<ColumnDef> = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.kw("constraint") {
                self.ident()?; // constraint names are accepted and not stored
            }
            if self.kw("primary") {
                self.expect_kw("key")?;
                constraints.push(TableConstraint::PrimaryKey(self.ident_list()?));
            } else if self.kw("unique") {
                constraints.push(TableConstraint::Unique(self.ident_list()?));
            } else if self.kw("foreign") {
                self.expect_kw("key")?;
                let cols = self.ident_list()?;
                constraints.push(TableConstraint::ForeignKey(self.references(cols)?));
            } else {
                cols.push(self.column_def()?);
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(Stmt::CreateTable {
            name,
            cols,
            constraints,
            if_not_exists,
        })
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

    /// `REFERENCES table (cols) [ON DELETE CASCADE|RESTRICT|SET NULL|NO ACTION]`
    fn references(&mut self, cols: Vec<String>) -> Result<FkDef> {
        self.expect_kw("references")?;
        let table = self.ident()?;
        let ref_cols = if self.peek_sym("(") {
            self.ident_list()?
        } else {
            Vec::new()
        };
        let mut on_delete = FkAction::Restrict;
        while self.kw("on") {
            let is_delete = if self.kw("delete") {
                true
            } else {
                self.expect_kw("update")?;
                false
            };
            let action = if self.kw("cascade") {
                FkAction::Cascade
            } else if self.kw("restrict") {
                FkAction::Restrict
            } else if self.kw("set") {
                self.expect_kw("null")?;
                FkAction::SetNull
            } else if self.kw("no") {
                self.expect_kw("action")?;
                FkAction::Restrict
            } else {
                return perr(format!(
                    "expected CASCADE, RESTRICT, SET NULL or NO ACTION, found {}",
                    self.found()
                ));
            };
            if is_delete {
                on_delete = action;
            } else if action != FkAction::Restrict {
                return perr("only ON UPDATE RESTRICT / NO ACTION is supported");
            }
        }
        Ok(FkDef {
            cols,
            table,
            ref_cols,
            on_delete,
        })
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.ident()?;
        let tname = self.ident()?;
        let Some(ty) = Ty::from_name(&tname) else {
            return perr(format!(
                "unknown column type '{tname}' (use INT, REAL, TEXT, BOOL, DATE, TIME, TIMESTAMP or JSON)"
            ));
        };
        self.kw("precision"); // DOUBLE PRECISION
        if self.sym("(") {
            // VARCHAR(255), DECIMAL(10,2): accepted, not enforced
            self.usize_lit()?;
            if self.sym(",") {
                self.usize_lit()?;
            }
            self.expect_sym(")")?;
        }
        let mut col = ColumnDef {
            name,
            ty,
            pk: false,
            not_null: false,
            unique: false,
            default: None,
            references: None,
        };
        loop {
            if self.kw("primary") {
                self.expect_kw("key")?;
                col.pk = true;
            } else if self.kw("not") {
                self.expect_kw("null")?;
                col.not_null = true;
            } else if self.kw("unique") {
                col.unique = true;
            } else if self.kw("default") {
                col.default = Some(self.unary_top()?);
            } else if self.peek_kw("references") {
                col.references = Some(self.references(vec![col.name.clone()])?);
            } else if !self.kw("null") {
                break;
            }
        }
        Ok(col)
    }

    fn unary_top(&mut self) -> Result<Expr> {
        if self.nest == 0 {
            self.ops = 0;
        }
        self.enter()?;
        let e = self.unary();
        self.nest -= 1;
        e
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        if self.sym("(") {
            self.enter()?;
            let q = self.query();
            self.nest -= 1;
            let q = q?;
            self.expect_sym(")")?;
            let Some(alias) = self.alias()? else {
                return perr("a subquery in FROM needs an alias");
            };
            return Ok(TableRef {
                source: Source::Subquery(Box::new(q)),
                alias,
            });
        }
        let name = self.object_name(Model::Relational)?;
        let alias = self.alias()?.unwrap_or_else(|| default_alias(&name));
        Ok(TableRef {
            source: Source::Named(name),
            alias,
        })
    }

    /// `select_core (UNION [ALL] select_core)* [ORDER BY ..] [LIMIT n] [OFFSET n]`
    pub fn query(&mut self) -> Result<Query> {
        let mut body = self.set_term()?;
        while self.kw("union") {
            let all = self.kw("all");
            if !all {
                self.kw("distinct");
            }
            let right = self.set_term()?;
            body = SetExpr::Union(Box::new(body), Box::new(right), all);
        }
        let mut q = Query {
            body,
            order_by: Vec::new(),
            limit: None,
            offset: 0,
        };
        if self.kw("order") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                let desc = if self.kw("desc") {
                    true
                } else {
                    self.kw("asc");
                    false
                };
                q.order_by.push((e, desc));
                if !self.sym(",") {
                    break;
                }
            }
        }
        if self.kw("limit") {
            q.limit = Some(self.usize_lit()?);
        }
        if self.kw("offset") {
            q.offset = self.usize_lit()?;
        }
        Ok(q)
    }

    fn set_term(&mut self) -> Result<SetExpr> {
        if self.sym("(") {
            self.enter()?;
            let q = self.query();
            self.nest -= 1;
            let q = q?;
            self.expect_sym(")")?;
            if !q.order_by.is_empty() || q.limit.is_some() || q.offset > 0 {
                // a parenthesised branch with its own ORDER BY / LIMIT: keep it as a derived table
                let sel = Select {
                    distinct: false,
                    items: vec![Item::Star],
                    from: Some(TableRef {
                        source: Source::Subquery(Box::new(q)),
                        alias: "_branch".into(),
                    }),
                    joins: vec![],
                    filter: None,
                    group_by: vec![],
                    having: None,
                };
                return Ok(SetExpr::Select(Box::new(sel)));
            }
            return Ok(q.body);
        }
        self.expect_kw("select")?;
        self.select_core().map(|s| SetExpr::Select(Box::new(s)))
    }

    fn select_core(&mut self) -> Result<Select> {
        let distinct = self.kw("distinct");
        if !distinct {
            self.kw("all");
        }
        let mut items = Vec::new();
        loop {
            if self.sym("*") {
                items.push(Item::Star);
            } else if matches!(self.peek(), Some(Tok::Word { .. } | Tok::Ident(_)))
                && self.peek_at(1) == Some(&Tok::Sym("."))
                && self.peek_at(2) == Some(&Tok::Sym("*"))
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
        };
        if self.kw("from") {
            s.from = Some(self.table_ref()?);
            loop {
                if self.sym(",") {
                    s.joins.push(Join {
                        table: self.table_ref()?,
                        on: None,
                        kind: JoinKind::Cross,
                    });
                    continue;
                }
                let kind = if self.kw("left") {
                    self.kw("outer");
                    JoinKind::Left
                } else if self.kw("cross") {
                    JoinKind::Cross
                } else {
                    self.kw("inner");
                    JoinKind::Inner
                };
                if !self.kw("join") {
                    if kind != JoinKind::Inner {
                        return perr(format!("expected JOIN, found {}", self.found()));
                    }
                    break;
                }
                let table = self.table_ref()?;
                let on = if kind == JoinKind::Cross {
                    None
                } else {
                    self.expect_kw("on")?;
                    Some(self.expr()?)
                };
                s.joins.push(Join { table, on, kind });
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

    // Precedence, loosest first: OR, AND, NOT, comparison, + - ||, * / %, unary -
    pub fn expr(&mut self) -> Result<Expr> {
        if self.nest == 0 {
            self.ops = 0;
        }
        self.enter()?;
        let e = self.or_expr();
        self.nest -= 1;
        e
    }

    fn enter(&mut self) -> Result<()> {
        if self.nest >= MAX_SQL_NESTING {
            return perr(format!(
                "expression is nested deeper than {MAX_SQL_NESTING} levels"
            ));
        }
        self.nest += 1;
        if self.nest > 1 {
            self.op()?;
        }
        Ok(())
    }

    fn op(&mut self) -> Result<()> {
        self.ops += 1;
        if self.ops > MAX_SQL_OPERATORS {
            return perr(format!(
                "expression is too complex: more than {MAX_SQL_OPERATORS} operators and nested sub-expressions"
            ));
        }
        Ok(())
    }

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

    fn subquery(&mut self) -> Result<SubRef> {
        self.enter()?;
        let q = self.query();
        self.nest -= 1;
        Ok(SubRef::Parsed(Box::new(q?)))
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        if self.peek_kw("exists") && self.peek_at(1) == Some(&Tok::Sym("(")) {
            self.i += 2;
            let q = self.subquery()?;
            self.expect_sym(")")?;
            return Ok(Expr::Exists(q, false));
        }
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
            if self.peek_kw("select") {
                let q = self.subquery()?;
                self.expect_sym(")")?;
                return Ok(Expr::InQuery(bx(l), q, neg));
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
        for (s, op) in [
            ("=", Op::Eq),
            ("!=", Op::Ne),
            ("<>", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
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
            self.primary()
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
            Tok::Json(v) => Ok(Expr::Lit(v)),
            Tok::Sym("(") => {
                if self.peek_kw("select") {
                    let q = self.subquery()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::ScalarQuery(q));
                }
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Word { lower, orig } => match lower.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                "case" => self.case(),
                "cast" if self.peek_sym("(") => {
                    self.i += 1;
                    let e = self.expr()?;
                    self.expect_kw("as")?;
                    let tname = self.ident()?;
                    let Some(ty) = Ty::from_name(&tname) else {
                        return perr(format!("unknown type '{tname}' in CAST"));
                    };
                    if self.sym("(") {
                        self.usize_lit()?;
                        if self.sym(",") {
                            self.usize_lit()?;
                        }
                        self.expect_sym(")")?;
                    }
                    self.expect_sym(")")?;
                    Ok(Expr::Cast(bx(e), ty))
                }
                // typed literals: DATE '2024-01-31', TIME '10:00', TIMESTAMP '2024-01-31 10:00:00'
                "date" | "time" | "timestamp" | "datetime"
                    if matches!(self.peek(), Some(Tok::Str(_))) =>
                {
                    let Some(Tok::Str(s)) = self.peek().cloned() else {
                        unreachable!()
                    };
                    self.i += 1;
                    let ty = Ty::from_name(&lower).unwrap_or(Ty::Text);
                    let v = crate::query::eval::coerce_type(ty, Value::Text(s))?;
                    Ok(Expr::Lit(v))
                }
                "current_date" | "current_time" | "current_timestamp" | "localtimestamp"
                    if !self.peek_sym("(") =>
                {
                    Ok(Expr::Func(lower, vec![]))
                }
                _ if RESERVED.contains(&lower.as_str()) => {
                    self.i -= 1;
                    perr(format!(
                        "expected an expression, found keyword {}",
                        lower.to_uppercase()
                    ))
                }
                _ if self.sym("(") => self.call(lower),
                _ => self.column(orig),
            },
            Tok::Ident(w) => self.column(w),
            _ => {
                self.i -= 1;
                perr(format!("unexpected {}", self.found()))
            }
        }
    }

    fn case(&mut self) -> Result<Expr> {
        (|| {
            let operand = if self.peek_kw("when") {
                None
            } else {
                Some(bx(self.expr()?))
            };
            let mut whens = Vec::new();
            while self.kw("when") {
                let w = self.expr()?;
                self.expect_kw("then")?;
                whens.push((w, self.expr()?));
            }
            if whens.is_empty() {
                return perr("CASE needs at least one WHEN");
            }
            let otherwise = if self.kw("else") {
                Some(bx(self.expr()?))
            } else {
                None
            };
            self.expect_kw("end")?;
            Ok(Expr::Case {
                operand,
                whens,
                otherwise,
            })
        })()
    }

    /// `a`, `t.a`, `doc.a.b.c` (names keep their case; resolution decides).
    fn column(&mut self, first: String) -> Result<Expr> {
        let mut path = vec![first];
        while self.peek_sym(".") {
            match self.peek_at(1).cloned() {
                Some(Tok::Word { orig, .. }) => path.push(orig),
                Some(Tok::Ident(w)) => path.push(w),
                Some(Tok::Int(n)) if n >= 0 => path.push(n.to_string()),
                _ => break,
            }
            self.i += 2;
        }
        Ok(Expr::Col(path))
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

        (|| {
            if let Some(f) = agg {
                if f == AggFn::Count && self.sym("*") {
                    self.expect_sym(")")?;
                    return Ok(Expr::Agg(f, None, false));
                }
                let distinct = self.kw("distinct");
                let arg = self.expr()?;
                self.expect_sym(")")?;
                return Ok(Expr::Agg(f, Some(bx(arg)), distinct));
            }
            if name == "extract" {
                // EXTRACT(YEAR FROM x) -> year(x)
                let field = self.ident()?;
                self.expect_kw("from")?;
                let e = self.expr()?;
                self.expect_sym(")")?;
                return Ok(Expr::Func(field, vec![e]));
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
            Ok(Expr::Func(name.clone(), args))
        })()
    }
}

/// The alias a source gets when none is written: its last name component.
pub fn default_alias(o: &ObjectName) -> String {
    match o.model {
        Model::Kv if o.name.is_empty() => "kv".into(),
        _ => o.name.rsplit('.').next().unwrap_or(&o.name).to_lowercase(),
    }
}
