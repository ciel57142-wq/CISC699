//! Relational model: typed tables with primary keys, queried through SQL.
//!
//! Lexer -> recursive-descent parser -> tree-walking executor.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

// ====================================================================== lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Unquoted word, lower-cased: a keyword or an identifier.
    Word(String),
    /// Quoted identifier, case preserved.
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Sym(&'static str),
}

fn perr<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Parse(msg.into()))
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            out.push(Tok::Word(cs[start..i].iter().collect::<String>().to_lowercase()));
        } else if c.is_ascii_digit() || (c == '.' && next.map_or(false, |n| n.is_ascii_digit())) {
            let start = i;
            let mut float = false;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                float |= cs[i] == '.';
                i += 1;
            }
            if i < cs.len() && (cs[i] == 'e' || cs[i] == 'E') {
                let mut j = i + 1;
                if j < cs.len() && (cs[j] == '+' || cs[j] == '-') {
                    j += 1;
                }
                if j < cs.len() && cs[j].is_ascii_digit() {
                    float = true;
                    i = j;
                    while i < cs.len() && cs[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = cs[start..i].iter().collect();
            let tok = if float { None } else { text.parse::<i64>().ok().map(Tok::Int) };
            match tok {
                Some(t) => out.push(t),
                None => match text.parse::<f64>() {
                    Ok(f) => out.push(Tok::Float(f)),
                    Err(_) => return perr(format!("invalid number '{text}'")),
                },
            }
        } else if c == '\'' {
            i += 1;
            let mut s = String::new();
            loop {
                match cs.get(i) {
                    None => return perr("unterminated string literal"),
                    Some('\'') if cs.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(*ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c == '"' || c == '`' {
            i += 1;
            let start = i;
            while i < cs.len() && cs[i] != c {
                i += 1;
            }
            if i >= cs.len() {
                return perr("unterminated quoted identifier");
            }
            out.push(Tok::Ident(cs[start..i].iter().collect()));
            i += 1;
        } else {
            let two: Option<&'static str> = match (c, next) {
                ('<', Some('=')) => Some("<="),
                ('>', Some('=')) => Some(">="),
                ('<', Some('>')) => Some("<>"),
                ('!', Some('=')) => Some("!="),
                ('|', Some('|')) => Some("||"),
                _ => None,
            };
            if let Some(s) = two {
                out.push(Tok::Sym(s));
                i += 2;
                continue;
            }
            let one: &'static str = match c {
                '(' => "(",
                ')' => ")",
                ',' => ",",
                '*' => "*",
                '=' => "=",
                '<' => "<",
                '>' => ">",
                '+' => "+",
                '-' => "-",
                '/' => "/",
                '%' => "%",
                '.' => ".",
                ';' => ";",
                _ => return perr(format!("unexpected character '{c}'")),
            };
            out.push(Tok::Sym(one));
            i += 1;
        }
    }
    Ok(out)
}

// ======================================================================== AST

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::And => "and",
            Op::Or => "or",
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Mod => "%",
            Op::Concat => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone)]
enum Expr {
    Lit(Value),
    Col(Option<String>, String),
    Bin(Box<Expr>, Op, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    Agg(AggFn, Option<Box<Expr>>),
    Func(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ty {
    Int,
    Float,
    Text,
    Bool,
}

impl Ty {
    fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "real",
            Ty::Text => "text",
            Ty::Bool => "bool",
        }
    }
    fn from_name(s: &str) -> Option<Ty> {
        Some(match s {
            "int" | "integer" | "bigint" | "smallint" => Ty::Int,
            "real" | "float" | "double" | "numeric" | "decimal" => Ty::Float,
            "text" | "varchar" | "char" | "string" => Ty::Text,
            "bool" | "boolean" => Ty::Bool,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    ty: Ty,
    pk: bool,
    not_null: bool,
}

#[derive(Debug)]
struct TableRef {
    name: String,
    alias: String,
}

#[derive(Debug)]
struct Join {
    table: TableRef,
    on: Expr,
    left: bool,
}

#[derive(Debug)]
enum Item {
    Star,
    TableStar(String),
    Expr(Expr, Option<String>),
}

#[derive(Debug)]
struct Select {
    distinct: bool,
    items: Vec<Item>,
    from: Option<TableRef>,
    joins: Vec<Join>,
    filter: Option<Expr>,
    group_by: Vec<Expr>,
    having: Option<Expr>,
    order_by: Vec<(Expr, bool)>,
    limit: Option<usize>,
    offset: usize,
}

#[derive(Debug)]
enum Stmt {
    CreateTable { name: String, cols: Vec<Column>, if_not_exists: bool },
    DropTable { name: String, if_exists: bool },
    CreateIndex { name: String, table: String, col: String, if_not_exists: bool },
    DropIndex { name: String, table: String },
    Insert { table: String, cols: Option<Vec<String>>, rows: Vec<Vec<Expr>> },
    Select(Select),
    Update { table: String, sets: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
}

// ===================================================================== parser

const RESERVED: &[&str] = &[
    "where", "join", "inner", "left", "outer", "on", "group", "order", "by", "limit", "offset", "having", "as",
    "set", "values", "from", "and", "or", "not", "asc", "desc", "union",
];

/// Limits on one SQL expression. The expression parser, the evaluator and
/// the code that frees an expression tree are all recursive, so an
/// expression that is nested or chained without limit overflows the stack
/// and aborts the process.
/// The values leave a wide margin on the 2 MB stack of a connection thread,
/// including in unoptimised builds, where each evaluator frame is several kilobytes.
const MAX_SQL_NESTING: usize = 32; // parentheses, NOT, unary minus, function calls
const MAX_SQL_OPERATORS: usize = 100; // chained operators plus nested sub-expressions in one expression

struct P {
    t: Vec<Tok>,
    i: usize,
    /// How many nested sub-expressions the parser is currently inside.
    nest: usize,
    /// An upper bound on how deep the tree of the current top-level
    /// expression is: chained operators plus nested sub-expressions so far.
    ops: usize,
}

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn found(&self) -> String {
        match self.peek() {
            None => "end of statement".into(),
            Some(Tok::Word(w)) | Some(Tok::Ident(w)) => format!("'{w}'"),
            Some(Tok::Int(n)) => n.to_string(),
            Some(Tok::Float(f)) => f.to_string(),
            Some(Tok::Str(s)) => format!("'{s}'"),
            Some(Tok::Sym(s)) => format!("'{s}'"),
        }
    }

    fn peek_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w == k)
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

    fn sym(&mut self, s: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Sym(x)) if *x == s);
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

    fn statement(&mut self) -> Result<Stmt> {
        if self.kw("select") {
            return self.select().map(Stmt::Select);
        }
        if self.kw("insert") {
            self.expect_kw("into")?;
            let table = self.ident()?;
            let cols = if self.sym("(") {
                let mut c = vec![self.ident()?];
                while self.sym(",") {
                    c.push(self.ident()?);
                }
                self.expect_sym(")")?;
                Some(c)
            } else {
                None
            };
            self.expect_kw("values")?;
            let mut rows = Vec::new();
            loop {
                self.expect_sym("(")?;
                let mut row = vec![self.expr()?];
                while self.sym(",") {
                    row.push(self.expr()?);
                }
                self.expect_sym(")")?;
                rows.push(row);
                if !self.sym(",") {
                    break;
                }
            }
            return Ok(Stmt::Insert { table, cols, rows });
        }
        if self.kw("update") {
            let table = self.ident()?;
            self.expect_kw("set")?;
            let mut sets = Vec::new();
            loop {
                let col = self.ident()?;
                self.expect_sym("=")?;
                sets.push((col, self.expr()?));
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
            if self.kw("unique") {
                return perr("UNIQUE indexes are not supported");
            }
            let is_index = self.kw("index");
            if !is_index {
                self.expect_kw("table")?;
            }
            let if_not_exists = if self.kw("if") {
                self.expect_kw("not")?;
                self.expect_kw("exists")?;
                true
            } else {
                false
            };
            let name = self.ident()?;
            if is_index {
                self.expect_kw("on")?;
                let table = self.ident()?;
                self.expect_sym("(")?;
                let col = self.ident()?;
                if self.sym(",") {
                    return perr("multi-column indexes are not supported");
                }
                self.expect_sym(")")?;
                return Ok(Stmt::CreateIndex { name, table, col, if_not_exists });
            }
            self.expect_sym("(")?;
            let mut cols: Vec<Column> = Vec::new();
            let mut table_pk: Option<String> = None;
            loop {
                if self.kw("primary") {
                    self.expect_kw("key")?;
                    self.expect_sym("(")?;
                    table_pk = Some(self.ident()?);
                    if self.sym(",") {
                        return perr("composite primary keys are not supported");
                    }
                    self.expect_sym(")")?;
                } else {
                    let cname = self.ident()?;
                    let tname = self.ident()?;
                    let Some(ty) = Ty::from_name(&tname) else {
                        return perr(format!("unknown column type '{tname}' (use INT, REAL, TEXT or BOOL)"));
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
                    let mut col = Column { name: cname, ty, pk: false, not_null: false };
                    loop {
                        if self.kw("primary") {
                            self.expect_kw("key")?;
                            col.pk = true;
                        } else if self.kw("not") {
                            self.expect_kw("null")?;
                            col.not_null = true;
                        } else if !self.kw("null") {
                            break;
                        }
                    }
                    cols.push(col);
                }
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            if let Some(pk) = table_pk {
                match cols.iter_mut().find(|c| c.name == pk) {
                    Some(c) => c.pk = true,
                    None => return perr(format!("PRIMARY KEY names unknown column '{pk}'")),
                }
            }
            return Ok(Stmt::CreateTable { name, cols, if_not_exists });
        }
        if self.kw("drop") {
            if self.kw("index") {
                let name = self.ident()?;
                self.expect_kw("on")?;
                return Ok(Stmt::DropIndex { name, table: self.ident()? });
            }
            self.expect_kw("table")?;
            let if_exists = if self.kw("if") {
                self.expect_kw("exists")?;
                true
            } else {
                false
            };
            return Ok(Stmt::DropTable { name: self.ident()?, if_exists });
        }
        perr(format!("unsupported SQL statement starting at {}", self.found()))
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        let name = self.ident()?;
        let alias = self.alias()?.unwrap_or_else(|| name.clone());
        Ok(TableRef { name, alias })
    }

    fn select(&mut self) -> Result<Select> {
        let distinct = self.kw("distinct");
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
            s.from = Some(self.table_ref()?);
            loop {
                let left = if self.kw("left") {
                    self.kw("outer");
                    true
                } else {
                    self.kw("inner");
                    false
                };
                if !self.kw("join") {
                    if left {
                        return perr("expected JOIN after LEFT");
                    }
                    break;
                }
                let table = self.table_ref()?;
                self.expect_kw("on")?;
                s.joins.push(Join { table, on: self.expr()?, left });
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
                s.order_by.push((e, desc));
                if !self.sym(",") {
                    break;
                }
            }
        }
        if self.kw("limit") {
            s.limit = Some(self.usize_lit()?);
        }
        if self.kw("offset") {
            s.offset = self.usize_lit()?;
        }
        Ok(s)
    }

    // Precedence, loosest first: OR, AND, NOT, comparison, + - ||, * / %, unary -
    fn expr(&mut self) -> Result<Expr> {
        if self.nest == 0 {
            self.ops = 0;
        }
        self.enter()?;
        let e = self.or_expr();
        self.nest -= 1;
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
            Tok::Sym("(") => {
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Tok::Word(w) => match w.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
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

    fn column(&mut self, first: String) -> Result<Expr> {
        if self.sym(".") {
            Ok(Expr::Col(Some(first), self.ident()?))
        } else {
            Ok(Expr::Col(None, first))
        }
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
                return Ok(Expr::Agg(f, None));
            }
            if self.peek_kw("distinct") {
                return perr("DISTINCT inside aggregates is not supported");
            }
            let arg = self.expr()?;
            self.expect_sym(")")?;
            return Ok(Expr::Agg(f, Some(bx(arg))));
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
}

fn parse(sql: &str) -> Result<Stmt> {
    let mut p = P { t: lex(sql)?, i: 0, nest: 0, ops: 0 };
    let st = p.statement()?;
    p.sym(";");
    if p.peek().is_some() {
        return perr(format!("unexpected {} after the end of the statement", p.found()));
    }
    Ok(st)
}

// =================================================================== catalog

struct Schema {
    name: String,
    cols: Vec<Column>,
    /// Secondary indexes as (index name, column name).
    indexes: Vec<(String, String)>,
}

impl Schema {
    fn pk_index(&self) -> Option<usize> {
        self.cols.iter().position(|c| c.pk)
    }

    fn col_index(&self, name: &str) -> Result<usize> {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| Error::Schema(format!("table '{}' has no column '{name}'", self.name)))
    }

    fn to_json(&self) -> String {
        let cols = self
            .cols
            .iter()
            .map(|c| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), Value::Text(c.name.clone()));
                o.insert("type".to_string(), Value::Text(c.ty.name().to_string()));
                o.insert("pk".to_string(), Value::Bool(c.pk));
                o.insert("not_null".to_string(), Value::Bool(c.not_null));
                Value::Object(o)
            })
            .collect();
        let indexes = self
            .indexes
            .iter()
            .map(|(n, c)| Value::Array(vec![Value::Text(n.clone()), Value::Text(c.clone())]))
            .collect();
        let mut o = BTreeMap::new();
        o.insert("cols".to_string(), Value::Array(cols));
        o.insert("indexes".to_string(), Value::Array(indexes));
        Value::Object(o).to_json()
    }

    fn load(ctx: &Ctx, name: &str) -> Result<Schema> {
        let raw = ctx.get(&keys::table_meta(name)).ok_or_else(|| Error::Schema(format!("no such table '{name}'")))?;
        let bad = || Error::Corrupt(format!("schema of table '{name}' is unreadable"));
        let v = Value::parse(std::str::from_utf8(&raw).map_err(|_| bad())?).map_err(|_| bad())?;
        let Some(Value::Array(cols)) = v.get("cols") else {
            return Err(bad());
        };
        let mut out = Vec::new();
        for c in cols {
            out.push(Column {
                name: c.get("name").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string(),
                ty: c.get("type").and_then(|v| v.as_str()).and_then(Ty::from_name).ok_or_else(bad)?,
                pk: c.get("pk") == Some(&Value::Bool(true)),
                not_null: c.get("not_null") == Some(&Value::Bool(true)),
            });
        }
        let mut indexes = Vec::new();
        if let Some(Value::Array(list)) = v.get("indexes") {
            for pair in list {
                if let Value::Array(p) = pair {
                    if let (Some(Value::Text(n)), Some(Value::Text(c))) = (p.first(), p.get(1)) {
                        indexes.push((n.clone(), c.clone()));
                    }
                }
            }
        }
        Ok(Schema { name: name.to_string(), cols: out, indexes })
    }
}

pub fn tables(ctx: &Ctx) -> Vec<String> {
    ctx.scan(keys::TABLE_META_PREFIX)
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::TABLE_META_PREFIX.len()..]).into_owned())
        .collect()
}

fn decode_row(raw: &[u8]) -> Result<Vec<Value>> {
    let parsed = std::str::from_utf8(raw).ok().and_then(|s| Value::parse(s).ok());
    match parsed {
        Some(Value::Array(a)) => Ok(a),
        _ => Err(Error::Corrupt("stored row is unreadable".into())),
    }
}

fn encode_row(row: &[Value]) -> Vec<u8> {
    Value::Array(row.to_vec()).to_json().into_bytes()
}

fn coerce(col: &Column, v: Value) -> Result<Value> {
    match (col.ty, v) {
        (_, Value::Null) => {
            if col.not_null || col.pk {
                Err(Error::Constraint(format!("column '{}' cannot be NULL", col.name)))
            } else {
                Ok(Value::Null)
            }
        }
        (Ty::Int, Value::Int(i)) => Ok(Value::Int(i)),
        (Ty::Int, Value::Float(f)) if f.fract() == 0.0 && f.abs() < 9e18 => Ok(Value::Int(f as i64)),
        (Ty::Float, Value::Int(i)) => Ok(Value::Float(i as f64)),
        (Ty::Float, Value::Float(f)) => Ok(Value::Float(f)),
        (Ty::Text, Value::Text(s)) => Ok(Value::Text(s)),
        (Ty::Bool, Value::Bool(b)) => Ok(Value::Bool(b)),
        (ty, v) => Err(Error::Type(format!(
            "column '{}' expects {}, got {}",
            col.name,
            ty.name().to_uppercase(),
            v.to_json()
        ))),
    }
}

/// Storage key of a row: its primary key, or a hidden auto-increment row id.
fn row_key(ctx: &mut Ctx, s: &Schema, row: &[Value]) -> Result<Vec<u8>> {
    let mut k = keys::row_prefix(&s.name);
    match s.pk_index() {
        Some(i) => k.extend(keys::encode_id(&row[i])?),
        None => {
            let id = keys::next_seq(ctx, &format!("t:{}", s.name))?;
            k.extend(keys::encode_id(&Value::Int(id))?);
        }
    }
    Ok(k)
}

fn owner(table: &str) -> String {
    format!("t:{table}")
}

/// Secondary-index entries pointing at one row.
fn index_entries(s: &Schema, key: &[u8], row: &[Value]) -> Vec<Vec<u8>> {
    let id = &key[keys::row_prefix(&s.name).len()..];
    s.indexes
        .iter()
        .filter_map(|(_, col)| {
            let ci = s.cols.iter().position(|c| &c.name == col)?;
            let mut k = keys::index_prefix(&owner(&s.name), col);
            k.extend(keys::encode_index_value(&row[ci])?);
            k.extend_from_slice(id);
            Some(k)
        })
        .collect()
}

fn put_row(ctx: &mut Ctx, s: &Schema, key: Vec<u8>, row: &[Value]) {
    for k in index_entries(s, &key, row) {
        ctx.put(k, Vec::new());
    }
    ctx.put(key, encode_row(row));
}

fn del_row(ctx: &mut Ctx, s: &Schema, key: Vec<u8>, row: &[Value]) {
    for k in index_entries(s, &key, row) {
        ctx.delete(k);
    }
    ctx.delete(key);
}

/// Collect `column = literal` terms that every matching row must satisfy.
fn eq_conjuncts<'e>(e: &'e Expr, alias: &str, out: &mut Vec<(&'e str, &'e Value)>) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            eq_conjuncts(a, alias, out);
            eq_conjuncts(b, alias, out);
        }
        Expr::Bin(a, Op::Eq, b) => {
            if let (Expr::Col(q, n), Expr::Lit(v)) | (Expr::Lit(v), Expr::Col(q, n)) = (&**a, &**b) {
                if q.as_ref().map_or(true, |q| q == alias) {
                    out.push((n.as_str(), v));
                }
            }
        }
        _ => {}
    }
}

/// Candidate rows of a table. Uses, in order of preference: a direct lookup
/// when the filter pins the primary key, a secondary index when it pins an
/// indexed column, a full scan otherwise. The caller still applies the filter.
fn fetch(ctx: &Ctx, s: &Schema, alias: &str, filter: Option<&Expr>) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let mut eqs = Vec::new();
    if let Some(f) = filter {
        eq_conjuncts(f, alias, &mut eqs);
    }
    if let Some(pk) = s.cols.iter().find(|c| c.pk) {
        for (n, v) in &eqs {
            let typed = matches!((pk.ty, *v), (Ty::Int, Value::Int(_)) | (Ty::Text, Value::Text(_)));
            if *n == pk.name && typed {
                let mut k = keys::row_prefix(&s.name);
                k.extend(keys::encode_id(v)?);
                return Ok(match ctx.get(&k) {
                    Some(raw) => vec![(k, decode_row(&raw)?)],
                    None => vec![],
                });
            }
        }
    }
    for (n, v) in &eqs {
        if !s.indexes.iter().any(|(_, c)| c == n) {
            continue;
        }
        let Some(enc) = keys::encode_index_value(v) else {
            continue;
        };
        let mut prefix = keys::index_prefix(&owner(&s.name), n);
        prefix.extend(enc);
        let mut out = Vec::new();
        for (ik, _) in ctx.scan(&prefix) {
            let mut k = keys::row_prefix(&s.name);
            k.extend_from_slice(&ik[prefix.len()..]);
            if let Some(raw) = ctx.get(&k) {
                out.push((k, decode_row(&raw)?));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        return Ok(out);
    }
    ctx.scan(&keys::row_prefix(&s.name)).into_iter().map(|(k, raw)| Ok((k, decode_row(&raw)?))).collect()
}

// ================================================================ evaluation

type Scope = Vec<(String, String)>; // (table alias, column name)

struct Env<'a> {
    scope: &'a Scope,
    row: &'a [Value],
    /// Rows of the current group when evaluating aggregates.
    group: Option<&'a [Vec<Value>]>,
}

fn resolve(scope: &Scope, q: &Option<String>, n: &str) -> Result<usize> {
    let mut found = None;
    for (i, (a, c)) in scope.iter().enumerate() {
        if c == n && q.as_ref().map_or(true, |q| q == a) {
            if found.is_some() {
                return Err(Error::Schema(format!("column '{n}' is ambiguous; qualify it with a table name")));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| match q {
        Some(q) => Error::Schema(format!("no such column '{q}.{n}'")),
        None => Error::Schema(format!("no such column '{n}'")),
    })
}

/// SQL three-valued logic: NULL is "unknown".
fn tri(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::Type(format!("expected a boolean, got {}", other.to_json()))),
    }
}

fn from_tri(t: Option<bool>) -> Value {
    t.map_or(Value::Null, Value::Bool)
}

fn like(s: &[char], p: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('%') => (0..=s.len()).any(|i| like(&s[i..], &p[1..])),
        Some('_') => !s.is_empty() && like(&s[1..], &p[1..]),
        Some(c) => s.first() == Some(c) && like(&s[1..], &p[1..]),
    }
}

fn arith(op: Op, l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        if matches!(op, Op::Div | Op::Mod) && *b == 0 {
            return Err(Error::Type("division by zero".into()));
        }
        let res = match op {
            Op::Add => a.checked_add(*b),
            Op::Sub => a.checked_sub(*b),
            Op::Mul => a.checked_mul(*b),
            Op::Div => a.checked_div(*b),
            _ => a.checked_rem(*b),
        };
        return res.map(Value::Int).ok_or_else(|| Error::Type("integer overflow".into()));
    }
    match (l.as_f64(), r.as_f64()) {
        (Some(a), Some(b)) => Ok(Value::Float(match op {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div => a / b,
            _ => a % b,
        })),
        _ => Err(Error::Type(format!("cannot apply '{}' to {} and {}", op.text(), l.to_json(), r.to_json()))),
    }
}

fn eval(e: &Expr, env: &Env) -> Result<Value> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Col(q, n) => env.row[resolve(env.scope, q, n)?].clone(),
        Expr::Not(a) => from_tri(tri(&eval(a, env)?)?.map(|b| !b)),
        Expr::Neg(a) => match eval(a, env)? {
            Value::Null => Value::Null,
            Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| Error::Type("integer overflow".into()))?),
            Value::Float(f) => Value::Float(-f),
            other => return Err(Error::Type(format!("cannot negate {}", other.to_json()))),
        },
        Expr::IsNull(a, neg) => Value::Bool(eval(a, env)?.is_null() != *neg),
        Expr::Like(a, p, neg) => match (eval(a, env)?, eval(p, env)?) {
            (Value::Null, _) | (_, Value::Null) => Value::Null,
            (Value::Text(s), Value::Text(p)) => {
                let (s, p): (Vec<char>, Vec<char>) = (s.chars().collect(), p.chars().collect());
                Value::Bool(like(&s, &p) != *neg)
            }
            _ => return Err(Error::Type("LIKE needs text on both sides".into())),
        },
        Expr::In(a, list, neg) => {
            let v = eval(a, env)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            let mut hit = false;
            for item in list {
                let x = eval(item, env)?;
                if x.is_null() {
                    saw_null = true;
                } else if x.total_cmp(&v) == Ordering::Equal {
                    hit = true;
                    break;
                }
            }
            if hit {
                Value::Bool(!*neg)
            } else if saw_null {
                Value::Null
            } else {
                Value::Bool(*neg)
            }
        }
        Expr::Bin(a, Op::And, b) => {
            let l = tri(&eval(a, env)?)?;
            if l == Some(false) {
                return Ok(Value::Bool(false));
            }
            let r = tri(&eval(b, env)?)?;
            from_tri(match (l, r) {
                (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Bin(a, Op::Or, b) => {
            let l = tri(&eval(a, env)?)?;
            if l == Some(true) {
                return Ok(Value::Bool(true));
            }
            let r = tri(&eval(b, env)?)?;
            from_tri(match (l, r) {
                (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        Expr::Bin(a, op, b) => {
            let (l, r) = (eval(a, env)?, eval(b, env)?);
            match op {
                Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => arith(*op, &l, &r)?,
                Op::Concat => {
                    if l.is_null() || r.is_null() {
                        Value::Null
                    } else {
                        Value::Text(l.display() + &r.display())
                    }
                }
                _ => {
                    if l.is_null() || r.is_null() {
                        return Ok(Value::Null);
                    }
                    let o = l.total_cmp(&r);
                    Value::Bool(match op {
                        Op::Eq => o == Ordering::Equal,
                        Op::Ne => o != Ordering::Equal,
                        Op::Lt => o == Ordering::Less,
                        Op::Le => o != Ordering::Greater,
                        Op::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    })
                }
            }
        }
        Expr::Func(name, args) => {
            let vals = args.iter().map(|a| eval(a, env)).collect::<Result<Vec<_>>>()?;
            let one = || match vals.as_slice() {
                [v] => Ok(v),
                _ => Err(Error::Parse(format!("{name}() takes exactly one argument"))),
            };
            match name.as_str() {
                "version" => Value::Text(format!("polydb {}", env!("CARGO_PKG_VERSION"))),
                "coalesce" => vals.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
                "lower" | "upper" | "length" => match one()? {
                    Value::Null => Value::Null,
                    Value::Text(s) => match name.as_str() {
                        "lower" => Value::Text(s.to_lowercase()),
                        "upper" => Value::Text(s.to_uppercase()),
                        _ => Value::Int(s.chars().count() as i64),
                    },
                    other => return Err(Error::Type(format!("{name}() needs text, got {}", other.to_json()))),
                },
                "abs" => match one()? {
                    Value::Null => Value::Null,
                    Value::Int(i) => Value::Int(i.checked_abs().ok_or_else(|| Error::Type("integer overflow".into()))?),
                    Value::Float(f) => Value::Float(f.abs()),
                    other => return Err(Error::Type(format!("abs() needs a number, got {}", other.to_json()))),
                },
                _ => return Err(Error::Parse(format!("unknown function '{name}'"))),
            }
        }
        Expr::Agg(f, arg) => {
            let Some(group) = env.group else {
                return Err(Error::Parse("aggregate functions are not allowed here".into()));
            };
            let Some(arg) = arg else {
                return Ok(Value::Int(group.len() as i64)); // COUNT(*)
            };
            let mut vals = Vec::new();
            for r in group {
                let v = eval(arg, &Env { scope: env.scope, row: r, group: None })?;
                if !v.is_null() {
                    vals.push(v);
                }
            }
            match f {
                AggFn::Count => Value::Int(vals.len() as i64),
                AggFn::Min => vals.into_iter().min_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
                AggFn::Max => vals.into_iter().max_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
                AggFn::Sum | AggFn::Avg => {
                    if vals.is_empty() {
                        return Ok(Value::Null);
                    }
                    let n = vals.len() as f64;
                    let mut total = Value::Int(0);
                    for v in &vals {
                        total = arith(Op::Add, &total, v)?;
                    }
                    if *f == AggFn::Avg {
                        Value::Float(total.as_f64().unwrap_or(0.0) / n)
                    } else {
                        total
                    }
                }
            }
        }
    })
}

fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg(..) => true,
        Expr::Lit(_) | Expr::Col(..) => false,
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) => has_agg(a),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => has_agg(a) || has_agg(b),
        Expr::In(a, list, _) => has_agg(a) || list.iter().any(has_agg),
        Expr::Func(_, args) => args.iter().any(has_agg),
    }
}

/// Column header for an unaliased select expression.
fn label(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => v.display(),
        Expr::Col(_, n) => n.clone(),
        Expr::Bin(a, op, b) => format!("{} {} {}", label(a), op.text(), label(b)),
        Expr::Neg(a) => format!("-{}", label(a)),
        Expr::Not(a) => format!("not {}", label(a)),
        Expr::Agg(f, arg) => {
            let name = match f {
                AggFn::Count => "count",
                AggFn::Sum => "sum",
                AggFn::Avg => "avg",
                AggFn::Min => "min",
                AggFn::Max => "max",
            };
            format!("{name}({})", arg.as_ref().map_or("*".to_string(), |a| label(a)))
        }
        Expr::Func(n, args) => format!("{n}({})", args.iter().map(label).collect::<Vec<_>>().join(", ")),
        _ => "expr".to_string(),
    }
}

fn is_true(v: &Value) -> Result<bool> {
    Ok(tri(v)? == Some(true))
}

// ================================================================== executor

pub fn execute(ctx: &mut Ctx, sql: &str) -> Result<Output> {
    match parse(sql)? {
        Stmt::CreateTable { name, cols, if_not_exists } => create_table(ctx, name, cols, if_not_exists),
        Stmt::DropTable { name, if_exists } => {
            if ctx.get(&keys::table_meta(&name)).is_none() {
                return if if_exists {
                    Ok(Output::Done(format!("table '{name}' does not exist, nothing dropped")))
                } else {
                    Err(Error::Schema(format!("no such table '{name}'")))
                };
            }
            for prefix in [keys::row_prefix(&name), keys::index_owner_prefix(&owner(&name))] {
                for (k, _) in ctx.scan(&prefix) {
                    ctx.delete(k);
                }
            }
            ctx.delete(keys::table_meta(&name));
            ctx.delete(keys::seq(&format!("t:{name}")));
            Ok(Output::Done(format!("table '{name}' dropped")))
        }
        Stmt::CreateIndex { name, table, col, if_not_exists } => {
            let mut schema = Schema::load(ctx, &table)?;
            schema.col_index(&col)?;
            if schema.indexes.iter().any(|(n, _)| n == &name) {
                return if if_not_exists {
                    Ok(Output::Done(format!("index '{name}' already exists")))
                } else {
                    Err(Error::Schema(format!("index '{name}' already exists on table '{table}'")))
                };
            }
            let already = schema.indexes.iter().any(|(_, c)| c == &col);
            schema.indexes.push((name.clone(), col.clone()));
            if !already {
                for (key, raw) in ctx.scan(&keys::row_prefix(&table)) {
                    for k in index_entries(&schema, &key, &decode_row(&raw)?) {
                        ctx.put(k, Vec::new());
                    }
                }
            }
            ctx.put(keys::table_meta(&table), schema.to_json().into_bytes());
            Ok(Output::Done(format!("index '{name}' created on {table}({col})")))
        }
        Stmt::DropIndex { name, table } => {
            let mut schema = Schema::load(ctx, &table)?;
            let Some(pos) = schema.indexes.iter().position(|(n, _)| n == &name) else {
                return Err(Error::Schema(format!("no index '{name}' on table '{table}'")));
            };
            let (_, col) = schema.indexes.remove(pos);
            if !schema.indexes.iter().any(|(_, c)| c == &col) {
                for (k, _) in ctx.scan(&keys::index_prefix(&owner(&table), &col)) {
                    ctx.delete(k);
                }
            }
            ctx.put(keys::table_meta(&table), schema.to_json().into_bytes());
            Ok(Output::Done(format!("index '{name}' dropped")))
        }
        Stmt::Insert { table, cols, rows } => insert(ctx, &table, cols, rows),
        Stmt::Select(s) => select(ctx, &s),
        Stmt::Update { table, sets, filter } => update(ctx, &table, sets, filter),
        Stmt::Delete { table, filter } => {
            let schema = Schema::load(ctx, &table)?;
            let scope: Scope = schema.cols.iter().map(|c| (table.clone(), c.name.clone())).collect();
            let mut n = 0u64;
            for (key, row) in fetch(ctx, &schema, &table, filter.as_ref())? {
                let keep = match &filter {
                    Some(f) => !is_true(&eval(f, &Env { scope: &scope, row: &row, group: None })?)?,
                    None => false,
                };
                if !keep {
                    del_row(ctx, &schema, key, &row);
                    n += 1;
                }
            }
            Ok(Output::Done(format!("{n} row(s) deleted")))
        }
    }
}

fn create_table(ctx: &mut Ctx, name: String, cols: Vec<Column>, if_not_exists: bool) -> Result<Output> {
    if !keys::valid_name(&name) {
        return Err(Error::Schema(format!("invalid table name '{name}'")));
    }
    if ctx.get(&keys::table_meta(&name)).is_some() {
        return if if_not_exists {
            Ok(Output::Done(format!("table '{name}' already exists")))
        } else {
            Err(Error::Schema(format!("table '{name}' already exists")))
        };
    }
    let mut seen = HashSet::new();
    for c in &cols {
        if c.name.contains('\0') {
            return Err(Error::Schema("column names cannot contain NUL".into()));
        }
        if !seen.insert(c.name.as_str()) {
            return Err(Error::Schema(format!("duplicate column '{}'", c.name)));
        }
    }
    let pks: Vec<&Column> = cols.iter().filter(|c| c.pk).collect();
    if pks.len() > 1 {
        return Err(Error::Schema("composite primary keys are not supported".into()));
    }
    if let Some(pk) = pks.first() {
        if !matches!(pk.ty, Ty::Int | Ty::Text) {
            return Err(Error::Schema("a primary key must be INT or TEXT".into()));
        }
    }
    let schema = Schema { name: name.clone(), cols, indexes: Vec::new() };
    ctx.put(keys::table_meta(&name), schema.to_json().into_bytes());
    Ok(Output::Done(format!("table '{name}' created")))
}

fn insert(ctx: &mut Ctx, table: &str, cols: Option<Vec<String>>, rows: Vec<Vec<Expr>>) -> Result<Output> {
    let schema = Schema::load(ctx, table)?;
    let targets: Vec<usize> = match &cols {
        Some(names) => names.iter().map(|n| schema.col_index(n)).collect::<Result<_>>()?,
        None => (0..schema.cols.len()).collect(),
    };
    let empty_scope = Scope::new();
    let n = rows.len();
    for exprs in rows {
        if exprs.len() != targets.len() {
            return Err(Error::Parse(format!("expected {} value(s) per row, got {}", targets.len(), exprs.len())));
        }
        let mut row = vec![Value::Null; schema.cols.len()];
        for (e, &ci) in exprs.iter().zip(&targets) {
            row[ci] = eval(e, &Env { scope: &empty_scope, row: &[], group: None })?;
        }
        for (ci, col) in schema.cols.iter().enumerate() {
            row[ci] = coerce(col, std::mem::replace(&mut row[ci], Value::Null))?;
        }
        let key = row_key(ctx, &schema, &row)?;
        if ctx.get(&key).is_some() {
            let pk = schema.pk_index().map(|i| row[i].to_json()).unwrap_or_default();
            return Err(Error::Constraint(format!("duplicate primary key {pk} in table '{table}'")));
        }
        put_row(ctx, &schema, key, &row);
    }
    Ok(Output::Done(format!("{n} row(s) inserted")))
}

fn update(ctx: &mut Ctx, table: &str, sets: Vec<(String, Expr)>, filter: Option<Expr>) -> Result<Output> {
    let schema = Schema::load(ctx, table)?;
    let scope: Scope = schema.cols.iter().map(|c| (table.to_string(), c.name.clone())).collect();
    let sets: Vec<(usize, Expr)> =
        sets.into_iter().map(|(n, e)| Ok((schema.col_index(&n)?, e))).collect::<Result<_>>()?;
    let pk = schema.pk_index();
    let mut n = 0u64;
    for (key, row) in fetch(ctx, &schema, table, filter.as_ref())? {
        let env = Env { scope: &scope, row: &row, group: None };
        if let Some(f) = &filter {
            if !is_true(&eval(f, &env)?)? {
                continue;
            }
        }
        let mut new = row.clone();
        for (ci, e) in &sets {
            new[*ci] = coerce(&schema.cols[*ci], eval(e, &env)?)?;
        }
        let mut new_key = key.clone();
        if let Some(pi) = pk {
            if new[pi].total_cmp(&row[pi]) != Ordering::Equal {
                new_key = keys::row_prefix(table);
                new_key.extend(keys::encode_id(&new[pi])?);
                if ctx.get(&new_key).is_some() {
                    return Err(Error::Constraint(format!(
                        "duplicate primary key {} in table '{table}'",
                        new[pi].to_json()
                    )));
                }
            }
        }
        del_row(ctx, &schema, key, &row);
        put_row(ctx, &schema, new_key, &new);
        n += 1;
    }
    Ok(Output::Done(format!("{n} row(s) updated")))
}

fn select(ctx: &Ctx, s: &Select) -> Result<Output> {
    // 1. FROM and JOINs build the working rows and the column scope.
    let mut scope = Scope::new();
    let mut rows: Vec<Vec<Value>> = vec![vec![]];
    if let Some(from) = &s.from {
        let schema = Schema::load(ctx, &from.name)?;
        scope.extend(schema.cols.iter().map(|c| (from.alias.clone(), c.name.clone())));
        let pushdown = if s.joins.is_empty() { s.filter.as_ref() } else { None };
        rows = fetch(ctx, &schema, &from.alias, pushdown)?.into_iter().map(|(_, r)| r).collect();
        for j in &s.joins {
            let js = Schema::load(ctx, &j.table.name)?;
            if scope.iter().any(|(a, _)| a == &j.table.alias) {
                return Err(Error::Schema(format!("table name '{}' used twice; give it an alias", j.table.alias)));
            }
            scope.extend(js.cols.iter().map(|c| (j.table.alias.clone(), c.name.clone())));
            let right: Vec<Vec<Value>> = fetch(ctx, &js, &j.table.alias, None)?.into_iter().map(|(_, r)| r).collect();
            let mut joined = Vec::new();
            for l in &rows {
                let mut matched = false;
                for r in &right {
                    let mut combined = l.clone();
                    combined.extend(r.iter().cloned());
                    if is_true(&eval(&j.on, &Env { scope: &scope, row: &combined, group: None })?)? {
                        joined.push(combined);
                        matched = true;
                    }
                }
                if !matched && j.left {
                    let mut combined = l.clone();
                    combined.resize(scope.len(), Value::Null);
                    joined.push(combined);
                }
            }
            rows = joined;
        }
    }

    // 2. WHERE
    if let Some(f) = &s.filter {
        let mut kept = Vec::new();
        for r in rows {
            if is_true(&eval(f, &Env { scope: &scope, row: &r, group: None })?)? {
                kept.push(r);
            }
        }
        rows = kept;
    }

    // 3. Expand the select list.
    let mut items: Vec<(Expr, String, bool)> = Vec::new(); // (expr, header, has explicit alias)
    for it in &s.items {
        match it {
            Item::Star => {
                if scope.is_empty() {
                    return perr("SELECT * needs a FROM clause");
                }
                items.extend(scope.iter().map(|(a, c)| (Expr::Col(Some(a.clone()), c.clone()), c.clone(), false)));
            }
            Item::TableStar(t) => {
                let before = items.len();
                items.extend(
                    scope
                        .iter()
                        .filter(|(a, _)| a == t)
                        .map(|(a, c)| (Expr::Col(Some(a.clone()), c.clone()), c.clone(), false)),
                );
                if items.len() == before {
                    return Err(Error::Schema(format!("no table '{t}' in this query")));
                }
            }
            Item::Expr(e, alias) => items.push((e.clone(), alias.clone().unwrap_or_else(|| label(e)), alias.is_some())),
        }
    }

    // ORDER BY may name a select alias or a 1-based output position.
    let order: Vec<(Expr, bool)> = s
        .order_by
        .iter()
        .map(|(e, desc)| {
            let e = match e {
                Expr::Col(None, n) => {
                    items.iter().find(|(_, name, aliased)| *aliased && name == n).map_or(e.clone(), |i| i.0.clone())
                }
                Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= items.len() => items[*k as usize - 1].0.clone(),
                _ => e.clone(),
            };
            (e, *desc)
        })
        .collect();

    // 4. Project, grouping first if the query aggregates.
    let aggregated = !s.group_by.is_empty()
        || items.iter().any(|i| has_agg(&i.0))
        || s.having.as_ref().map_or(false, has_agg)
        || order.iter().any(|o| has_agg(&o.0));

    let project = |env: &Env| -> Result<(Vec<Value>, Vec<Value>)> {
        let out = items.iter().map(|i| eval(&i.0, env)).collect::<Result<Vec<_>>>()?;
        let keys = order.iter().map(|o| eval(&o.0, env)).collect::<Result<Vec<_>>>()?;
        Ok((keys, out))
    };

    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    if aggregated {
        let groups: Vec<Vec<Vec<Value>>> = if s.group_by.is_empty() {
            vec![rows]
        } else {
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut groups: Vec<Vec<Vec<Value>>> = Vec::new();
            for r in rows {
                let env = Env { scope: &scope, row: &r, group: None };
                let key = s.group_by.iter().map(|e| eval(e, &env)).collect::<Result<Vec<_>>>()?;
                let gi = *index.entry(Value::Array(key).to_json()).or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[gi].push(r);
            }
            groups
        };
        let nulls = vec![Value::Null; scope.len()];
        for g in &groups {
            let env = Env { scope: &scope, row: g.first().unwrap_or(&nulls), group: Some(g) };
            if let Some(h) = &s.having {
                if !is_true(&eval(h, &env)?)? {
                    continue;
                }
            }
            out.push(project(&env)?);
        }
    } else {
        if s.having.is_some() {
            return perr("HAVING needs GROUP BY or an aggregate");
        }
        for r in &rows {
            out.push(project(&Env { scope: &scope, row: r, group: None })?);
        }
    }

    // 5. DISTINCT, ORDER BY, OFFSET / LIMIT
    if s.distinct {
        let mut seen = HashSet::new();
        out.retain(|(_, row)| seen.insert(Value::Array(row.clone()).to_json()));
    }
    if !order.is_empty() {
        out.sort_by(|a, b| {
            for (i, (_, desc)) in order.iter().enumerate() {
                let o = a.0[i].total_cmp(&b.0[i]);
                if o != Ordering::Equal {
                    return if *desc { o.reverse() } else { o };
                }
            }
            Ordering::Equal
        });
    }
    let rows = out.into_iter().map(|(_, r)| r).skip(s.offset).take(s.limit.unwrap_or(usize::MAX)).collect();
    Ok(Output::Rows { columns: items.into_iter().map(|i| i.1).collect(), rows })
}
