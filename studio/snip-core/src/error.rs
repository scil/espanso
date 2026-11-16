use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Problem {
    /// Path relative to the espanso base directory, with `/` separators.
    pub file: String,
    /// `error` or `warning`.
    pub level: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SnipError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("the item changed on disk (expected hash {expected}, found {actual}); reload it")]
    Conflict { expected: String, actual: String },
    #[error("invalid YAML: {0}")]
    InvalidYaml(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unsupported file layout: {0}")]
    Unsupported(String),
    #[error("the change would add {} espanso error(s); nothing was written", .0.len())]
    Validation(Vec<Problem>),
    #[error("read-only: {0}")]
    ReadOnly(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SnipError {
    /// Stable code for JSON-RPC errors (application range -32000..-32099).
    pub fn code(&self) -> i64 {
        match self {
            SnipError::NotFound(_) => -32001,
            SnipError::Conflict { .. } => -32002,
            SnipError::InvalidYaml(_) => -32003,
            SnipError::InvalidInput(_) => -32004,
            SnipError::Unsupported(_) => -32005,
            SnipError::Validation(_) => -32006,
            SnipError::ReadOnly(_) => -32007,
            SnipError::Io(_) => -32010,
            SnipError::Other(_) => -32011,
        }
    }

    /// Extra data for JSON-RPC errors.
    pub fn data(&self) -> serde_json::Value {
        match self {
            SnipError::Validation(problems) => serde_json::json!({ "problems": problems }),
            SnipError::Conflict { expected, actual } => {
                serde_json::json!({ "expected": expected, "actual": actual })
            }
            _ => serde_json::Value::Null,
        }
    }
}

pub type Result<T> = std::result::Result<T, SnipError>;
