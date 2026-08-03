use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// `Cargo.lock` no longer matches `[assignment].cargo-lock-sha256` --
    /// `deps::lock::verify`'s message is already self-describing.
    #[error("{0}")]
    StaleLock(String),

    #[error("failed to parse {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("failed to parse csv at {path}: {source}")]
    Csv {
        path: PathBuf,
        #[source]
        source: Box<csv::Error>,
    },

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
