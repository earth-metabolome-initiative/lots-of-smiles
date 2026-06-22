//! `lots-of-smiles` assembles one large, deduplicated collection of real-world
//! SMILES from multiple molecular databases (PubChem, ZINC20, Enamine REAL) for
//! use as training data, e.g. for an autoencoder.
//!
//! The pipeline reads each source, applies a configurable [filter][config::filters]
//! pass, stages `(inchikey, smiles)` records to disk, then sorts and
//! first-wins-deduplicates them by InChIKey into a configurable
//! [output][config::output] format. Canonicalization is delegated to each
//! source's shipped InChIKey column; no chemistry toolkit is invoked.
//!
//! # Example
//!
//! ```no_run
//! use lots_of_smiles::{Filters, LotsOfSmiles, OutputFormat, Zinc20Config};
//!
//! let report = LotsOfSmiles::builder()
//!     .scratch_dir("/mnt/nvme/los/scratch")
//!     .zinc20(Zinc20Config::builder().root("/mnt/bfd/zinc20/2D").build()?)
//!     .filters(Filters::builder().max_mass_da(900.0).build()?)
//!     .output(OutputFormat::builder().path("/mnt/nvme/los/corpus.smi").build()?)
//!     .build()?
//!     .run()?;
//! println!("{} unique molecules", report.dedup.unique_emitted);
//! # Ok::<(), lots_of_smiles::LosError>(())
//! ```
#![deny(missing_docs)]

pub mod config;
pub mod download;
pub mod error;
pub mod filter;
pub mod io;
pub mod pipeline;
pub mod progress;
#[cfg(feature = "zenodo")]
pub mod publish;
pub mod source;

pub use crate::config::credentials::EnamineCredentials;
pub use crate::config::filters::{AtomCount, ElementSet, Filters, FiltersBuilder, MassKind};
pub use crate::config::output::{
    Columns, Compression, OutputFormat, OutputFormatBuilder, Sharding,
};
pub use crate::config::{LotsOfSmiles, LotsOfSmilesBuilder, RunReport, SourceReport};
pub use crate::error::{LosError, Result};
pub use crate::filter::{FilterEngine, FilterOutcome, RejectReason};
pub use crate::io::InchiKey;
pub use crate::pipeline::sortdedup::DedupStats;
#[cfg(feature = "zenodo")]
pub use crate::publish::zenodo::{Target as ZenodoTarget, ZenodoDeposit, ZenodoUpload};
pub use crate::source::enamine::{
    EnamineConfig, EnamineConfigBuilder, EnamineSource,
    ensure_downloaded as enamine_ensure_downloaded,
};
pub use crate::source::pubchem::{PubChemConfig, PubChemConfigBuilder, PubChemSource};
pub use crate::source::zinc20::{Zinc20Config, Zinc20ConfigBuilder, Zinc20Source};
pub use crate::source::{RawRecord, SmilesSource, SourceId, SourceStats};
