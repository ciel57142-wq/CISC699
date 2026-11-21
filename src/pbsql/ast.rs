//! PBSQL abstract syntax tree, shared by every front end that feeds the
//! planner (the SQL dialect is the relational subset of this grammar).

use crate::value::Value;

/// Which data model a name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Relational,
    Document,
    Kv,
}

impl Model {
    pub fn name(self) -> &'static str {
        match self {
            Model::Relational => "RELATIONAL",
            Model::Document => "DOCUMENT",
            Model::Kv => "KV",
        }
    }
}

/// A data source name: `users`, `RELATIONAL.users`, `DOCUMENT.app.events`, `KV`, `KV.sessions`.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectName {
    pub model: Model,
    /// Table, collection (possibly `namespace.collection`) or KV namespace ("" = default keyspace).
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
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
    pub fn text(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::And => "AND",
            Op::Or => "OR",
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Mod => "%",
            Op::Concat => "||",
        }
    }

    /// The comparison seen from the other side (`a < b` is `b > a`).
    pub fn flip(self) -> Op {
        match self {
            Op::Lt => Op::Gt,
            Op::Le => Op::Ge,
            Op::Gt => Op::Lt,
            Op::Ge => Op::Le,
            o => o,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFn {
    pub fn name(self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::Sum => "sum",
            AggFn::Avg => "avg",
            AggFn::Min => "min",
            AggFn::Max => "max",
        }
    }
}

/// Column types of relational tables.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ty {
    Int,
    Float,
    Text,
    Bool,
    Date,
    Time,
    Timestamp,
    Json,
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "real",
            Ty::Text => "text",
            Ty::Bool => "bool",
            Ty::Date => "date",
            Ty::Time => "time",
            Ty::Timestamp => "timestamp",
            Ty::Json => "json",
        }
    }
    pub fn from_name(s: &str) -> Option<Ty> {
        Some(match s {
            "int" | "integer" | "bigint" | "smallint" => Ty::Int,
            "real" | "float" | "double" | "numeric" | "decimal" => Ty::Float,
            "text" | "varchar" | "char" | "string" => Ty::Text,
            "bool" | "boolean" => Ty::Bool,
            "date" => Ty::Date,
            "time" => Ty::Time,
            "timestamp" | "datetime" => Ty::Timestamp,
            "json" | "jsonb" | "document" => Ty::Json,
            _ => return None,
        })
    }
}

/// A subquery attached to an expression. Before binding it holds the parsed
/// query; the binder replaces it with an index into the plan's subplans.
#[derive(Debug, Clone)]
pub enum SubRef {
    Parsed(Box<Query>),
    Bound(usize),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    /// Unresolved name path as written: `col`, `t.col`, `doc.a.b`.
    Col(Vec<String>),
    /// Resolved reference: column `idx` of the row `level` scopes out
    /// (0 = current row, 1 = enclosing query's row, ...), then a JSON path into it.
    Slot {
        level: usize,
        idx: usize,
        path: Vec<String>,
        name: String,
    },
    Bin(Box<Expr>, Op, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    InQuery(Box<Expr>, SubRef, bool),
    Exists(SubRef, bool),
    ScalarQuery(SubRef),
    Agg(AggFn, Option<Box<Expr>>, bool),
    Func(String, Vec<Expr>),
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    Cast(Box<Expr>, Ty),
    /// `DEFAULT` inside an INSERT's VALUES list.
    Default,
    /// A bound parameter sent without a type (PostgreSQL OID 0): its type is
    /// resolved from context, like an untyped literal in PostgreSQL.
    Untyped(String),
}

#[derive(Debug, Clone)]
pub enum Source {
    Named(ObjectName),
    Subquery(Box<Query>),
}

#[derive(Debug, Clone)]
pub struct TableRef {
    pub source: Source,
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub struct Join {
    pub table: TableRef,
    pub on: Option<Expr>,
    pub kind: JoinKind,
}

#[derive(Debug, Clone)]
pub enum Item {
    Star,
    TableStar(String),
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<Item>,
    pub from: Option<TableRef>,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum SetExpr {
    Select(Box<Select>),
    Union(Box<SetExpr>, Box<SetExpr>, bool),
}

#[derive(Debug, Clone)]
pub struct Query {
    pub body: SetExpr,
    pub order_by: Vec<(Expr, bool)>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FkAction {
    Restrict,
    Cascade,
    SetNull,
}

impl FkAction {
    pub fn name(self) -> &'static str {
        match self {
            FkAction::Restrict => "restrict",
            FkAction::Cascade => "cascade",
            FkAction::SetNull => "set null",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FkDef {
    pub cols: Vec<String>,
    pub table: String,
    pub ref_cols: Vec<String>,
    pub on_delete: FkAction,
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Ty,
    pub pk: bool,
    pub not_null: bool,
    pub unique: bool,
    pub default: Option<Expr>,
    pub references: Option<FkDef>,
    pub checks: Vec<Expr>,
}

#[derive(Debug, Clone)]
pub enum TableConstraint {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    ForeignKey(FkDef),
    Check(Option<String>, Expr),
}

#[derive(Debug, Clone)]
pub enum AlterOp {
    AddColumn(ColumnDef),
    DropColumn(String),
    RenameColumn(String, String),
    RenameTable(String),
    AddCheck(Option<String>, Expr),
    DropConstraint(String),
}

#[derive(Debug, Clone)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Query(Query),
    Explain {
        analyze: bool,
        stmt: Box<Stmt>,
    },
    CreateTable {
        name: String,
        cols: Vec<ColumnDef>,
        constraints: Vec<TableConstraint>,
        if_not_exists: bool,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        name: String,
        target: ObjectName,
        cols: Vec<String>,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        target: ObjectName,
    },
    AlterTable {
        table: String,
        op: AlterOp,
    },
    Insert {
        target: ObjectName,
        cols: Option<Vec<String>>,
        source: InsertSource,
    },
    Update {
        target: ObjectName,
        alias: String,
        sets: Vec<(Vec<String>, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        target: ObjectName,
        alias: String,
        filter: Option<Expr>,
    },
    KvGet {
        ns: String,
        key: Expr,
    },
    KvSet {
        ns: String,
        key: Expr,
        value: Expr,
        /// Time-to-live in seconds.
        ttl: Option<Expr>,
    },
    KvExists {
        ns: String,
        key: Expr,
    },
    KvIncr {
        ns: String,
        key: Expr,
        by: Expr,
    },
    KvDel {
        ns: String,
        key: Expr,
    },
}

/// Print an (unbound) expression back as PBSQL text that parses to the same
/// expression. Used to store CHECK constraints in the schema.
pub fn to_sql(e: &Expr) -> String {
    let list = |v: &[Expr]| v.iter().map(to_sql).collect::<Vec<_>>().join(", ");
    match e {
        Expr::Lit(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Lit(Value::Null) => "NULL".into(),
        Expr::Lit(Value::Float(f)) => {
            let t = format!("{f:?}");
            if f.is_finite() {
                t
            } else {
                "NULL".into()
            }
        }
        Expr::Lit(v) => v.to_json(),
        Expr::Untyped(t) => format!("'{}'", t.replace('\'', "''")),
        Expr::Col(p) => p
            .iter()
            .map(|x| format!("\"{}\"", x.replace('"', "")))
            .collect::<Vec<_>>()
            .join("."),
        Expr::Slot { name, .. } => format!("\"{name}\""),
        Expr::Bin(a, op, b) => format!("({} {} {})", to_sql(a), op.text(), to_sql(b)),
        Expr::Not(a) => format!("(NOT {})", to_sql(a)),
        Expr::Neg(a) => format!("(-{})", to_sql(a)),
        Expr::IsNull(a, n) => format!("({} IS {}NULL)", to_sql(a), if *n { "NOT " } else { "" }),
        Expr::Like(a, p, n) => format!(
            "({} {}LIKE {})",
            to_sql(a),
            if *n { "NOT " } else { "" },
            to_sql(p)
        ),
        Expr::In(a, l, n) => format!(
            "({} {}IN ({}))",
            to_sql(a),
            if *n { "NOT " } else { "" },
            list(l)
        ),
        Expr::Func(n, a) => format!("{n}({})", list(a)),
        Expr::Agg(f, a, d) => format!(
            "{}({}{})",
            f.name(),
            if *d { "DISTINCT " } else { "" },
            a.as_ref().map_or("*".into(), |x| to_sql(x))
        ),
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            let mut s = "(CASE".to_string();
            if let Some(o) = operand {
                s.push_str(&format!(" {}", to_sql(o)));
            }
            for (w, t) in whens {
                s.push_str(&format!(" WHEN {} THEN {}", to_sql(w), to_sql(t)));
            }
            if let Some(o) = otherwise {
                s.push_str(&format!(" ELSE {}", to_sql(o)));
            }
            s.push_str(" END)");
            s
        }
        Expr::Cast(a, t) => format!("CAST({} AS {})", to_sql(a), t.name()),
        Expr::Default => "DEFAULT".into(),
        Expr::InQuery(..) | Expr::Exists(..) | Expr::ScalarQuery(_) => "(subquery)".into(),
    }
}

/// Visit every unresolved column path in an expression.
pub fn walk_cols(e: &mut Expr, f: &mut dyn FnMut(&mut Vec<String>)) {
    match e {
        Expr::Col(p) => f(p),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => {
            walk_cols(a, f);
            walk_cols(b, f);
        }
        Expr::Not(a)
        | Expr::Neg(a)
        | Expr::IsNull(a, _)
        | Expr::Cast(a, _)
        | Expr::InQuery(a, ..) => walk_cols(a, f),
        Expr::In(a, l, _) => {
            walk_cols(a, f);
            l.iter_mut().for_each(|x| walk_cols(x, f));
        }
        Expr::Func(_, a) => a.iter_mut().for_each(|x| walk_cols(x, f)),
        Expr::Agg(_, Some(a), _) => walk_cols(a, f),
        Expr::Case {
            operand,
            whens,
            otherwise,
        } => {
            if let Some(o) = operand {
                walk_cols(o, f);
            }
            for (w, t) in whens {
                walk_cols(w, f);
                walk_cols(t, f);
            }
            if let Some(o) = otherwise {
                walk_cols(o, f);
            }
        }
        _ => {}
    }
}
