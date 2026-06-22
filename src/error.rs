//! Error type for the `lots-of-smiles` pipeline.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, LosError>;

/// Errors produced while configuring or running the pipeline.
#[derive(Debug, thiserror::Error)]
pub enum LosError {
    /// A builder was finalized with an invalid or inconsistent configuration.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// A required builder field was not set before calling `build`.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),

    /// A filesystem path that was expected to exist does not.
    #[error("path does not exist: {0}")]
    MissingPath(PathBuf),

    /// An I/O error occurred while reading inputs or writing outputs.
    #[error("I/O error{}: {source}", .context.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Io {
        /// Optional human-readable context describing what was being done.
        context: Option<String>,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The external `sort` process failed or could not be spawned.
    #[error("external sort failed: {0}")]
    Sort(String),

    /// A network download failed.
    #[error("download error: {0}")]
    Download(String),

    /// Credentials required for a source were missing or could not be resolved.
    #[error("credential error: {0}")]
    Credentials(String),

    /// Publishing the corpus to an external archive (for example, Zenodo) failed.
    #[error("publish error: {0}")]
    Publish(String),
}

impl LosError {
    /// Wraps an [`std::io::Error`] with human-readable context.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: Some(context.into()),
            source,
        }
    }
}

impl From<std::io::Error> for LosError {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            context: None,
            source,
        }
    }
}
