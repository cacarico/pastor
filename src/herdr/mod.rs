pub mod client;
// The test double behind the unit tests, the integration tests and the
// fake-herdr binary; a default build and the published crate leave it out.
// Gated on the feature alone, not `cfg(test)` too: fake.rs is excluded from
// the crates.io package (Cargo.toml `include`), so `cfg(test)` there would
// make a plain `cargo test` on the downloaded crate fail with a missing
// file. The modules that use it gate their own `mod tests` the same way.
#[cfg(feature = "fake-herdr")]
pub mod fake;
pub mod protocol;
pub mod transport;

pub use client::*;
pub use protocol::*;
pub use transport::*;

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("herdr error {code}: {message}")]
    Api { code: String, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    /// The connection itself failed in a way worth spelling out: a bridge process
    /// that died before replying reports its command, exit status and stderr here.
    #[error("{0}")]
    Transport(String),
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
