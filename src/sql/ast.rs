//! Syntax tree of the SQL subset, shared by the parser, the catalog (which
//! stores CHECK expressions as SQL text) and the executor.

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Op {
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
    pub(super) fn text(self) -> &'static str {
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
pub(super) enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFn {
    pub(super) fn name(self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::Sum => "sum",
            AggFn::Avg => "avg",
            AggFn::Min => "min",
            AggFn::Max => "max",
        }
    }
}

/// A subquery inside an expression. `id` is unique for the life of the
/// process, so results can be cached per statement even when the tree is cloned.
#[derive(Debug, Clone)]
pub(super) struct SubQ {
    pub id: usize,
    pub q: Box<Query>,
}

#[derive(Debug, Clone)]
pub(super) enum Expr {
    Lit(Value),
    Col(Option<String>, String),
    Bin(Box<Expr>, Op, Box<Expr>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool),
    In(Box<Expr>, Vec<Expr>, bool),
    /// `x [NOT] IN (SELECT ...)`
    InSub(Box<Expr>, SubQ, bool),
    /// `EXISTS (SELECT ...)`
    Exists(SubQ),
    /// `(SELECT ...)` used as a value
    Subquery(SubQ),
    /// aggregate, argument (None for `COUNT(*)`), DISTINCT
    Agg(AggFn, Option<Box<Expr>>, bool),
    Func(String, Vec<Expr>),
    Cast(Box<Expr>, Ty),
    /// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`
    Case(Option<Box<Expr>>, Vec<(Expr, Expr)>, Option<Box<Expr>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Ty {
    Int,
    Float,
    Text,
    Bool,
}

impl Ty {
    pub(super) fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "real",
            Ty::Text => "text",
            Ty::Bool => "bool",
        }
    }
    pub(super) fn from_name(s: &str) -> Option<Ty> {
        Some(match s {
            "int" | "integer" | "bigint" | "smallint" | "int2" | "int4" | "int8" | "tinyint" => Ty::Int,
            "real" | "float" | "double" | "numeric" | "decimal" | "float4" | "float8" => Ty::Float,
            "text" | "varchar" | "char" | "string" | "character" | "clob" => Ty::Text,
            "bool" | "boolean" => Ty::Bool,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct Column {
    pub name: String,
    pub ty: Ty,
    pub not_null: bool,
    /// Constant default, evaluated when the column was defined.
    pub default: Option<Value>,
    /// Maximum length in characters, from `VARCHAR(n)` / `CHAR(n)`.
    pub max_len: Option<usize>,
    /// Filled from a per-column sequence when no value (or NULL) is given.
    pub auto: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum FkAction {
    /// RESTRICT and NO ACTION: refuse the change while child rows refer to the key.
    Restrict,
    Cascade,
    SetNull,
}

impl FkAction {
    pub(super) fn name(self) -> &'static str {
        match self {
            FkAction::Restrict => "restrict",
            FkAction::Cascade => "cascade",
            FkAction::SetNull => "set null",
        }
    }
    pub(super) fn from_name(s: &str) -> Option<FkAction> {
        Some(match s {
            "restrict" | "no action" => FkAction::Restrict,
            "cascade" => FkAction::Cascade,
            "set null" => FkAction::SetNull,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct FkDef {
    pub name: Option<String>,
    pub cols: Vec<String>,
    pub ref_table: String,
    /// None: the referenced table's primary key.
    pub ref_cols: Option<Vec<String>>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

#[derive(Debug, Clone)]
pub(super) enum Constraint {
    PrimaryKey(Vec<String>),
    Unique(Option<String>, Vec<String>),
    Check(Option<String>, Expr),
    Foreign(FkDef),
}

/// A column definition as written: the column plus its DEFAULT expression.
#[derive(Debug, Clone)]
pub(super) struct ColumnDef {
    pub col: Column,
    pub default: Option<Expr>,
}

#[derive(Debug)]
pub(super) struct TableDef {
    pub name: String,
    pub cols: Vec<ColumnDef>,
    pub constraints: Vec<Constraint>,
    pub if_not_exists: bool,
}

#[derive(Debug)]
pub(super) enum Alter {
    /// column, constraints written on the column
    AddColumn(ColumnDef, Vec<Constraint>),
    DropColumn { name: String, if_exists: bool },
    RenameColumn { from: String, to: String },
    RenameTable(String),
    SetDefault(String, Option<Expr>),
    SetNotNull(String, bool),
    SetType(String, Ty, Option<usize>),
    AddConstraint(Constraint),
    DropConstraint { name: String, if_exists: bool },
}

#[derive(Debug, Clone)]
pub(super) enum Source {
    Table(String),
    Derived(Box<Query>),
}

#[derive(Debug, Clone)]
pub(super) struct FromItem {
    pub src: Source,
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub(super) struct Join {
    pub item: FromItem,
    pub kind: JoinKind,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone)]
pub(super) enum Item {
    Star,
    TableStar(String),
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone)]
pub(super) struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
    /// Explicit NULLS FIRST (true) / NULLS LAST (false).
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone)]
pub(super) struct Select {
    pub distinct: bool,
    pub items: Vec<Item>,
    pub from: Option<FromItem>,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum SetOp {
    Union,
    Intersect,
    Except,
}

/// A query: one SELECT, or SELECTs combined with UNION / INTERSECT / EXCEPT
/// (evaluated left to right). For a single SELECT its own ORDER BY / LIMIT
/// are used; for a compound query the ones here apply to the combined result.
#[derive(Debug, Clone)]
pub(super) struct Query {
    pub head: Select,
    pub rest: Vec<(SetOp, bool, Select)>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Debug)]
pub(super) enum InsertSource {
    /// Rows of expressions; `None` is the DEFAULT keyword.
    Values(Vec<Vec<Option<Expr>>>),
    Query(Query),
    DefaultValues,
}

#[derive(Debug)]
pub(super) enum Stmt {
    CreateTable(TableDef),
    DropTable { name: String, if_exists: bool, cascade: bool },
    CreateIndex { name: String, table: String, cols: Vec<String>, unique: bool, if_not_exists: bool },
    DropIndex { name: String, table: Option<String>, if_exists: bool },
    AlterTable { table: String, actions: Vec<Alter> },
    Truncate(String),
    Insert { table: String, cols: Option<Vec<String>>, source: InsertSource },
    Query(Query),
    Explain(Query),
    /// `SET col = expr`, `None` is DEFAULT.
    Update { table: String, sets: Vec<(String, Option<Expr>)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
    Describe(String),
    CheckTable(Option<String>),
    Reindex(Option<String>),
}
