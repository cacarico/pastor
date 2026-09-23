pub mod protocol;

pub use protocol::*;

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("herdr error {code}: {message}")]
    Api { code: String, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("connection closed")]
    Closed,
}

impl HerdrError {
    pub fn code(&self) -> Option<&str> {
        match self {
            HerdrError::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}
