//! Output-format configuration and its builder.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use crate::{LosError, Result};

/// Which columns to write for each surviving molecule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Columns {
    /// One SMILES per line, no other columns.
    SmilesOnly,
    /// `SMILES\tInChIKey`.
    SmilesInchikey,
    /// `SMILES\tInChIKey\tsource`.
    SmilesInchikeySource,
}

/// Output compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression.
    None,
    /// gzip.
    Gzip,
    /// zstandard at the given level.
    Zstd {
        /// zstd compression level (1..=22).
        level: i32,
    },
}

/// How the output is split across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharding {
    /// A single output file.
    Single,
    /// `n` shards, each record routed by a stable hash of its InChIKey.
    Shards(NonZeroU32),
}

/// Validated output-format configuration. Build via [`OutputFormat::builder`].
#[derive(Debug, Clone)]
pub struct OutputFormat {
    pub(crate) path: PathBuf,
    pub(crate) columns: Columns,
    pub(crate) compression: Compression,
    pub(crate) sharding: Sharding,
}

impl OutputFormat {
    /// Starts a new [`OutputFormatBuilder`].
    pub fn builder() -> OutputFormatBuilder {
        OutputFormatBuilder::default()
    }

    /// The configured output path (for [`Sharding::Single`]) or path stem
    /// (for [`Sharding::Shards`]).
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// The configured column layout.
    pub fn columns(&self) -> Columns {
        self.columns
    }
    /// The configured compression.
    pub fn compression(&self) -> Compression {
        self.compression
    }
    /// The configured sharding.
    pub fn sharding(&self) -> Sharding {
        self.sharding
    }
}

/// Builder for [`OutputFormat`].
///
/// Defaults: `columns = SmilesOnly`, `compression = None`, `sharding = Single`.
/// The output path is required.
#[derive(Debug, Clone, Default)]
pub struct OutputFormatBuilder {
    path: Option<PathBuf>,
    columns: Option<Columns>,
    compression: Option<Compression>,
    sharding: Option<Sharding>,
}

impl OutputFormatBuilder {
    /// Sets the output path (single file) or shard path stem (sharded).
    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }
    /// Sets which columns to emit (default [`Columns::SmilesOnly`]).
    pub fn columns(mut self, columns: Columns) -> Self {
        self.columns = Some(columns);
        self
    }
    /// Sets the compression (default [`Compression::None`]).
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = Some(compression);
        self
    }
    /// Sets the sharding (default [`Sharding::Single`]).
    pub fn sharding(mut self, sharding: Sharding) -> Self {
        self.sharding = Some(sharding);
        self
    }

    /// Validates and finalizes the [`OutputFormat`].
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingField`] if no path was set, or
    /// [`LosError::Config`] if a zstd level is out of range.
    pub fn build(self) -> Result<OutputFormat> {
        let path = self.path.ok_or(LosError::MissingField("output.path"))?;
        let compression = self.compression.unwrap_or(Compression::None);
        if let Compression::Zstd { level } = compression
            && !(1..=22).contains(&level)
        {
            return Err(LosError::Config(format!(
                "zstd level {level} out of range 1..=22"
            )));
        }
        Ok(OutputFormat {
            path,
            columns: self.columns.unwrap_or(Columns::SmilesOnly),
            compression,
            sharding: self.sharding.unwrap_or(Sharding::Single),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_path() {
        assert!(matches!(
            OutputFormat::builder().build(),
            Err(LosError::MissingField(_))
        ));
    }

    #[test]
    fn applies_defaults() {
        let o = OutputFormat::builder()
            .path("/tmp/out.smi")
            .build()
            .unwrap();
        assert_eq!(o.columns(), Columns::SmilesOnly);
        assert_eq!(o.compression(), Compression::None);
        assert_eq!(o.sharding(), Sharding::Single);
    }

    #[test]
    fn rejects_bad_zstd_level() {
        let e = OutputFormat::builder()
            .path("/tmp/o")
            .compression(Compression::Zstd { level: 50 })
            .build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }

    #[test]
    fn accepts_sharding() {
        let o = OutputFormat::builder()
            .path("/tmp/corpus")
            .sharding(Sharding::Shards(NonZeroU32::new(8).unwrap()))
            .build()
            .unwrap();
        assert!(matches!(o.sharding(), Sharding::Shards(n) if n.get() == 8));
    }
}
