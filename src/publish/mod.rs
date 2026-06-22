//! Depositing the assembled corpus to external archives.
//!
//! This module is available only when the `zenodo` feature is enabled. It
//! currently provides [`zenodo`], a typed Zenodo deposition flow that uploads
//! the canonical SMILES corpus with rich, dataset-specific metadata.

pub mod zenodo;
