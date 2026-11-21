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
}

#[derive(Debug, Clone)]
pub enum TableConstraint {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    ForeignKey(FkDef),
}

#[derive(Debug, Clone)]
pub enum AlterOp {
    AddColumn(ColumnDef),
    DropColumn(String),
    RenameColumn(String, String),
    RenameTable(String),
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
        table: String,
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
