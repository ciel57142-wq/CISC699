//! Canonical PBSQL text for relational statements: the query normalizer.
//!
//! The output is deterministic for a given syntax tree and parses back to
//! the same tree, so `normalize(normalize(s)) == normalize(s)`:
//!
//! * keywords and function names in upper case, one space between tokens;
//! * identifiers bare when they are lower-case words that are not
//!   keywords, otherwise double-quoted;
//! * string literals single-quoted with `''` for a quote; floats always
//!   carry a decimal point;
//! * the minimum parentheses operator precedence needs;
//! * sugar removed: `BETWEEN` is shown as the two comparisons it means,
//!   `a, b` in FROM as `CROSS JOIN`, `!=` as `<>`, an alias equal to the
//!   table name is dropped.
//!
//! (Stored CHECK constraints use the separate, fully parenthesised
//! `parser::to_sql`, whose output format is part of the on-disk catalog.)

use super::ast::*;
use crate::value::Value;

/// Words that must be quoted when used as identifiers.
const KEYWORDS: &[&str] = &[
    "all", "alter", "and", "as", "asc", "between", "by", "case", "cast", "check", "constraint", "create", "cross",
    "default", "delete", "desc", "distinct", "drop", "else", "end", "except", "exists", "explain", "false", "first",
    "foreign", "from", "full", "group", "having", "in", "index", "inner", "insert", "intersect", "into", "is", "join",
    "key", "last", "left", "like", "limit", "not", "null", "nulls", "offset", "on", "or", "order", "outer", "primary",
    "references", "right", "select", "set", "table", "then", "true", "union", "unique", "update", "values", "when",
    "where",
];

pub(crate) fn ident(name: &str) -> String {
    let mut chars = name.chars();
    let plain = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !KEYWORDS.contains(&name);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

pub(crate) fn string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(true) => "TRUE".into(),
        Value::Bool(false) => "FALSE".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Text(s) => string(s),
        other => format!("JSON({})", string(&other.to_json())),
    }
}

fn ty(t: Ty, max_len: Option<usize>) -> String {
    match (t, max_len) {
        (Ty::Text, Some(n)) => format!("VARCHAR({n})"),
        (Ty::Int, _) => "INT".into(),
        (Ty::Float, _) => "REAL".into(),
        (Ty::Text, _) => "TEXT".into(),
        (Ty::Bool, _) => "BOOL".into(),
    }
}

// Binding strength, loosest first; mirrors the parser's precedence climb.
const P_OR: u8 = 1;
const P_AND: u8 = 2;
const P_NOT: u8 = 3;
const P_CMP: u8 = 4;
const P_ADD: u8 = 5;
const P_MUL: u8 = 6;
const P_NEG: u8 = 7;
const P_ATOM: u8 = 9;

fn op_prec(op: Op) -> u8 {
    match op {
        Op::Or => P_OR,
        Op::And => P_AND,
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => P_CMP,
        Op::Add | Op::Sub | Op::Concat => P_ADD,
        Op::Mul | Op::Div | Op::Mod => P_MUL,
    }
}

/// How tightly an operator binds (higher binds tighter).
pub(crate) fn op_binding(op: Op) -> u8 {
    op_prec(op)
}

/// How tightly an expression's outermost operator binds.
pub(crate) fn binding(e: &Expr) -> u8 {
    prec(e)
}

fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Bin(_, op, _) => op_prec(*op),
        Expr::Not(_) => P_NOT,
        Expr::IsNull(..) | Expr::Like(..) | Expr::In(..) | Expr::InSub(..) => P_CMP,
        Expr::Neg(_) => P_NEG,
        Expr::Lit(Value::Int(i)) if *i < 0 => P_NEG,
        Expr::Lit(Value::Float(f)) if f.is_sign_negative() => P_NEG,
        _ => P_ATOM,
    }
}

/// An expression in canonical form.
pub(crate) fn expr(e: &Expr) -> String {
    at(e, 0)
}

/// `e`, parenthesised if it binds more loosely than `min`.
fn at(e: &Expr, min: u8) -> String {
    let s = bare(e);
    if prec(e) < min {
        format!("({s})")
    } else {
        s
    }
}

fn list(items: &[Expr]) -> String {
    items.iter().map(expr).collect::<Vec<_>>().join(", ")
}

fn not(neg: bool) -> &'static str {
    if neg {
        "NOT "
    } else {
        ""
    }
}

fn is_path_segment(s: &str) -> bool {
    !s.is_empty() && ident(s) == s
}

fn bare(e: &Expr) -> String {
    match e {
        Expr::Lit(v) => literal(v),
        Expr::Col(Some(q), n) => format!("{}.{}", ident(q), ident(n)),
        Expr::Col(None, n) => ident(n),
        Expr::Bin(a, op, b) => {
            let p = op_prec(*op);
            let sym = match op {
                Op::Ne => "<>".to_string(),
                o => o.text().to_uppercase(),
            };
            if p == P_CMP {
                format!("{} {sym} {}", at(a, P_ADD), at(b, P_ADD))
            } else {
                format!("{} {sym} {}", at(a, p), at(b, p + 1))
            }
        }
        Expr::Not(a) => format!("NOT {}", at(a, P_NOT)),
        Expr::Neg(a) => format!("-{}", at(a, P_NEG + 1)),
        Expr::IsNull(a, neg) => format!("{} IS {}NULL", at(a, P_ADD), not(*neg)),
        Expr::Like(a, p, neg) => format!("{} {}LIKE {}", at(a, P_ADD), not(*neg), at(p, P_ADD)),
        Expr::In(a, items, neg) => format!("{} {}IN ({})", at(a, P_ADD), not(*neg), list(items)),
        Expr::InSub(a, sq, neg) => format!("{} {}IN ({})", at(a, P_ADD), not(*neg), query(&sq.q)),
        Expr::Exists(sq) => format!("EXISTS ({})", query(&sq.q)),
        Expr::Subquery(sq) => format!("({})", query(&sq.q)),
        Expr::Agg(f, None, _) => format!("{}(*)", f.name().to_uppercase()),
        Expr::Agg(f, Some(a), distinct) => {
            format!("{}({}{})", f.name().to_uppercase(), if *distinct { "DISTINCT " } else { "" }, expr(a))
        }
        Expr::Func(name, args) if name == "get_path" && args.len() == 2 => match (&args[0], &args[1]) {
            (Expr::Col(Some(_), _), Expr::Lit(Value::Text(p))) if p.split('.').all(is_path_segment) => {
                format!("{}.{p}", bare(&args[0]))
            }
            _ => format!("GET_PATH({})", list(args)),
        },
        Expr::Func(name, args) => format!("{}({})", name.to_uppercase(), list(args)),
        Expr::Cast(a, t) => format!("CAST({} AS {})", expr(a), ty(*t, None)),
        Expr::Case(op, whens, els) => {
            let mut s = String::from("CASE");
            if let Some(o) = op {
                s += &format!(" {}", expr(o));
            }
            for (w, t) in whens {
                s += &format!(" WHEN {} THEN {}", expr(w), expr(t));
            }
            if let Some(x) = els {
                s += &format!(" ELSE {}", expr(x));
            }
            s + " END"
        }
    }
}

// ------------------------------------------------------------------ queries

fn from_item(f: &FromItem) -> String {
    let alias = |default: &str| if f.alias == default { String::new() } else { format!(" AS {}", ident(&f.alias)) };
    match &f.src {
        Source::Table(t) => format!("{}{}", ident(t), alias(t)),
        Source::Derived(q) => format!("({}) AS {}", query(q), ident(&f.alias)),
        Source::Collection(c) => format!("DOCUMENT({}){}", ident(c), alias(c)),
        Source::Kv(p) if p.is_empty() => format!("KV(){}", alias("kv")),
        Source::Kv(p) => format!("KV({}){}", string(p), alias("kv")),
    }
}

fn order_by(items: &[OrderItem]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|o| {
            let mut s = expr(&o.expr);
            if o.desc {
                s += " DESC";
            }
            match o.nulls_first {
                Some(true) => s += " NULLS FIRST",
                Some(false) => s += " NULLS LAST",
                None => {}
            }
            s
        })
        .collect();
    format!(" ORDER BY {}", parts.join(", "))
}

fn tail(order: &[OrderItem], limit: Option<usize>, offset: usize) -> String {
    let mut s = String::new();
    if !order.is_empty() {
        s += &order_by(order);
    }
    if let Some(n) = limit {
        s += &format!(" LIMIT {n}");
    }
    if offset > 0 {
        s += &format!(" OFFSET {offset}");
    }
    s
}

fn select(s: &Select) -> String {
    let items: Vec<String> = s
        .items
        .iter()
        .map(|it| match it {
            Item::Star => "*".to_string(),
            Item::TableStar(t) => format!("{}.*", ident(t)),
            Item::Expr(e, None) => expr(e),
            Item::Expr(e, Some(a)) => format!("{} AS {}", expr(e), ident(a)),
        })
        .collect();
    let mut out = format!("SELECT {}{}", if s.distinct { "DISTINCT " } else { "" }, items.join(", "));
    if let Some(f) = &s.from {
        out += &format!(" FROM {}", from_item(f));
        for j in &s.joins {
            let kind = match j.kind {
                JoinKind::Inner => "JOIN",
                JoinKind::Left => "LEFT JOIN",
                JoinKind::Right => "RIGHT JOIN",
                JoinKind::Full => "FULL JOIN",
                JoinKind::Cross => "CROSS JOIN",
            };
            out += &format!(" {kind} {}", from_item(&j.item));
            if let Some(on) = &j.on {
                out += &format!(" ON {}", expr(on));
            }
        }
    }
    if let Some(w) = &s.filter {
        out += &format!(" WHERE {}", expr(w));
    }
    if !s.group_by.is_empty() {
        out += &format!(" GROUP BY {}", list(&s.group_by));
    }
    if let Some(h) = &s.having {
        out += &format!(" HAVING {}", expr(h));
    }
    out + &tail(&s.order_by, s.limit, s.offset)
}

pub(crate) fn query(q: &Query) -> String {
    let mut out = select(&q.head);
    for (op, all, s) in &q.rest {
        let op = match op {
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        };
        out += &format!(" {op}{} {}", if *all { " ALL" } else { "" }, select(s));
    }
    out + &tail(&q.order_by, q.limit, q.offset)
}

// ---------------------------------------------------------------------- DDL

fn idents(names: &[String]) -> String {
    format!("({})", names.iter().map(|n| ident(n)).collect::<Vec<_>>().join(", "))
}

fn named(name: &Option<String>) -> String {
    name.as_ref().map_or(String::new(), |n| format!("CONSTRAINT {} ", ident(n)))
}

fn references(fk: &FkDef) -> String {
    let mut s = format!("REFERENCES {}", ident(&fk.ref_table));
    if let Some(cols) = &fk.ref_cols {
        s += &format!(" {}", idents(cols));
    }
    if fk.on_delete != FkAction::Restrict {
        s += &format!(" ON DELETE {}", fk.on_delete.name().to_uppercase());
    }
    if fk.on_update != FkAction::Restrict {
        s += &format!(" ON UPDATE {}", fk.on_update.name().to_uppercase());
    }
    s
}

fn table_constraint(c: &Constraint) -> String {
    match c {
        Constraint::PrimaryKey(cols) => format!("PRIMARY KEY {}", idents(cols)),
        Constraint::Unique(n, cols) => format!("{}UNIQUE {}", named(n), idents(cols)),
        Constraint::Check(n, e) => format!("{}CHECK ({})", named(n), expr(e)),
        Constraint::Foreign(fk) => format!("{}FOREIGN KEY {} {}", named(&fk.name), idents(&fk.cols), references(fk)),
    }
}

/// A constraint written on its column (`ALTER TABLE ... ADD COLUMN`).
fn column_constraint(c: &Constraint) -> String {
    match c {
        Constraint::PrimaryKey(_) => "PRIMARY KEY".into(),
        Constraint::Unique(n, _) => format!("{}UNIQUE", named(n)),
        Constraint::Check(n, e) => format!("{}CHECK ({})", named(n), expr(e)),
        Constraint::Foreign(fk) => format!("{}{}", named(&fk.name), references(fk)),
    }
}

fn column_def(cd: &ColumnDef) -> String {
    let c = &cd.col;
    let mut s = format!("{} {}", ident(&c.name), ty(c.ty, c.max_len));
    if c.not_null {
        s += " NOT NULL";
    }
    if let Some(d) = &cd.default {
        s += &format!(" DEFAULT {}", at(d, P_ADD));
    }
    if c.auto {
        s += " AUTOINCREMENT";
    }
    s
}

fn alter(a: &Alter) -> String {
    match a {
        Alter::AddColumn(cd, cons) => {
            let mut s = format!("ADD COLUMN {}", column_def(cd));
            for c in cons {
                s += &format!(" {}", column_constraint(c));
            }
            s
        }
        Alter::DropColumn { name, if_exists } => {
            format!("DROP COLUMN {}{}", if *if_exists { "IF EXISTS " } else { "" }, ident(name))
        }
        Alter::RenameColumn { from, to } => format!("RENAME COLUMN {} TO {}", ident(from), ident(to)),
        Alter::RenameTable(t) => format!("RENAME TO {}", ident(t)),
        Alter::SetDefault(c, Some(e)) => format!("ALTER COLUMN {} SET DEFAULT {}", ident(c), at(e, P_ADD)),
        Alter::SetDefault(c, None) => format!("ALTER COLUMN {} DROP DEFAULT", ident(c)),
        Alter::SetNotNull(c, true) => format!("ALTER COLUMN {} SET NOT NULL", ident(c)),
        Alter::SetNotNull(c, false) => format!("ALTER COLUMN {} DROP NOT NULL", ident(c)),
        Alter::SetType(c, t, len) => format!("ALTER COLUMN {} TYPE {}", ident(c), ty(*t, *len)),
        Alter::AddConstraint(c) => format!("ADD {}", table_constraint(c)),
        Alter::DropConstraint { name, if_exists } => {
            format!("DROP CONSTRAINT {}{}", if *if_exists { "IF EXISTS " } else { "" }, ident(name))
        }
    }
}

fn where_(f: &Option<Expr>) -> String {
    f.as_ref().map_or(String::new(), |e| format!(" WHERE {}", expr(e)))
}

/// A whole relational statement in canonical form.
pub(crate) fn stmt(s: &Stmt) -> String {
    match s {
        Stmt::CreateTable(def) => {
            let mut parts: Vec<String> = def.cols.iter().map(column_def).collect();
            parts.extend(def.constraints.iter().map(table_constraint));
            format!(
                "CREATE TABLE {}{} ({})",
                if def.if_not_exists { "IF NOT EXISTS " } else { "" },
                ident(&def.name),
                parts.join(", ")
            )
        }
        Stmt::DropTable { name, if_exists, cascade } => format!(
            "DROP TABLE {}{}{}",
            if *if_exists { "IF EXISTS " } else { "" },
            ident(name),
            if *cascade { " CASCADE" } else { "" }
        ),
        Stmt::CreateIndex { name, table, cols, unique, if_not_exists } => format!(
            "CREATE {}INDEX {}{} ON {} {}",
            if *unique { "UNIQUE " } else { "" },
            if *if_not_exists { "IF NOT EXISTS " } else { "" },
            ident(name),
            ident(table),
            idents(cols)
        ),
        Stmt::DropIndex { name, table, if_exists } => format!(
            "DROP INDEX {}{}{}",
            if *if_exists { "IF EXISTS " } else { "" },
            ident(name),
            table.as_ref().map_or(String::new(), |t| format!(" ON {}", ident(t)))
        ),
        Stmt::AlterTable { table, actions } => {
            format!("ALTER TABLE {} {}", ident(table), actions.iter().map(alter).collect::<Vec<_>>().join(", "))
        }
        Stmt::Truncate(t) => format!("TRUNCATE TABLE {}", ident(t)),
        Stmt::Insert { table, cols, source } => {
            let mut s = format!("INSERT INTO {}", ident(table));
            if let Some(c) = cols {
                s += &format!(" {}", idents(c));
            }
            match source {
                InsertSource::DefaultValues => s + " DEFAULT VALUES",
                InsertSource::Query(q) => s + " " + &query(q),
                InsertSource::Values(rows) => {
                    let rows: Vec<String> = rows
                        .iter()
                        .map(|r| {
                            let vals: Vec<String> =
                                r.iter().map(|v| v.as_ref().map_or_else(|| "DEFAULT".to_string(), expr)).collect();
                            format!("({})", vals.join(", "))
                        })
                        .collect();
                    s + " VALUES " + &rows.join(", ")
                }
            }
        }
        Stmt::Query(q) => query(q),
        Stmt::Explain { query: q, analyze } => format!("EXPLAIN {}{}", if *analyze { "ANALYZE " } else { "" }, query(q)),
        Stmt::Analyze(None) => "ANALYZE".into(),
        Stmt::Analyze(Some(t)) => format!("ANALYZE TABLE {}", ident(t)),
        Stmt::Update { table, sets, filter } => {
            let sets: Vec<String> = sets
                .iter()
                .map(|(c, e)| format!("{} = {}", ident(c), e.as_ref().map_or_else(|| "DEFAULT".to_string(), expr)))
                .collect();
            format!("UPDATE {} SET {}{}", ident(table), sets.join(", "), where_(filter))
        }
        Stmt::Delete { table, filter } => format!("DELETE FROM {}{}", ident(table), where_(filter)),
        Stmt::Describe(t) => format!("DESCRIBE {}", ident(t)),
        Stmt::CheckTable(None) => "CHECK TABLES".into(),
        Stmt::CheckTable(Some(t)) => format!("CHECK TABLE {}", ident(t)),
        Stmt::Reindex(None) => "REINDEX".into(),
        Stmt::Reindex(Some(t)) => format!("REINDEX TABLE {}", ident(t)),
    }
}
