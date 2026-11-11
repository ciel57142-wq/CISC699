use std::fmt;

/// Every failure the engine can report.
#[derive(Debug)]
pub enum Error {
    /// Underlying file system failure.
    Io(std::io::Error),
    /// The command, SQL statement or JSON text could not be parsed.
    Parse(String),
    /// Unknown table/column/collection, or a conflicting definition.
    Schema(String),
    /// Primary key / NOT NULL / duplicate id violations.
    Constraint(String),
    /// A value has the wrong type for the operation.
    Type(String),
    /// On-disk data failed validation.
    Corrupt(String),
    /// Misuse of BEGIN / COMMIT / ROLLBACK.
    Txn(String),
    /// The transaction read data that another session changed before it
    /// could commit. Nothing was written; retry the transaction.
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Parse(m) => write!(f, "parse error: {m}"),
            Error::Schema(m) => write!(f, "schema error: {m}"),
            Error::Constraint(m) => write!(f, "constraint violation: {m}"),
            Error::Type(m) => write!(f, "type error: {m}"),
            Error::Corrupt(m) => write!(f, "corrupt data: {m}"),
            Error::Txn(m) => write!(f, "transaction error: {m}"),
            Error::Conflict(m) => write!(f, "serialization conflict: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
