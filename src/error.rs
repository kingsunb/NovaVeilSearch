use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovaVeilSearchError {
    #[error("missing required config: {0}")]
    MissingConfig(&'static str),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("upstream timeout: {0}")]
    Timeout(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("oauth error: {0}")]
    OAuth(String),
    #[error("parse error: {0}")]
    Parse(String),
}

impl NovaVeilSearchError {
    /// JSON-RPC 2.0 error code mapping. See https://www.jsonrpc.org/specification#error_object
    pub fn code(&self) -> i32 {
        match self {
            // -32700 Parse error: invalid JSON
            NovaVeilSearchError::Parse(_) => -32700,
            // -32602 Invalid params
            NovaVeilSearchError::InvalidParams(_) => -32602,
            // -32004 (server-defined) resource not found
            NovaVeilSearchError::NotFound(_) => -32004,
            // -32002 (server-defined) upstream timeout
            NovaVeilSearchError::Timeout(_) => -32002,
            // -32001 (server-defined) upstream / provider failure
            NovaVeilSearchError::Provider(_) => -32001,
            // -32005 (server-defined) OAuth setup / refresh failure
            NovaVeilSearchError::OAuth(_) => -32005,
            // -32003 (server-defined) missing config
            NovaVeilSearchError::MissingConfig(_) => -32003,
        }
    }
}

pub type Result<T> = std::result::Result<T, NovaVeilSearchError>;
