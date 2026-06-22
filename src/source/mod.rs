//! The [`SmilesSource`] abstraction and shared source types.
//!
//! A source is a thin reader: it pulls rows from its underlying files and
//! yields [`RawRecord`]s of `(inchikey, smiles, shipped_mass)`. It does **not**
//! filter. Filtering is applied centrally during staging (see
//! [`crate::pipeline::stage`]) so the cost-ordered [`FilterEngine`] lives in one
//! place and every source benefits from it identically.
//!
//! [`FilterEngine`]: crate::filter::FilterEngine

use crate::io::InchiKey;

pub mod enamine;
pub mod pubchem;
pub mod zinc20;

/// Identifies a configured source. The position of a source in the run's source
/// list is its first-wins priority (earlier wins InChIKey collisions); this
/// enum only provides the human-readable [`tag`](SourceId::tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceId {
    /// ZINC20 tranche files.
    Zinc20,
    /// PubChem (CID-SMILES joined with CID-InChI-Key).
    PubChem,
    /// Enamine REAL.
    Enamine,
}

impl SourceId {
    /// A short lowercase tag used in scratch filenames and the output `source`
    /// column.
    pub fn tag(self) -> &'static str {
        match self {
            SourceId::Zinc20 => "zinc20",
            SourceId::PubChem => "pubchem",
            SourceId::Enamine => "enamine",
        }
    }
}

/// A single raw, pre-filter record yielded by a source.
#[derive(Debug, Clone)]
pub struct RawRecord {
    /// The canonical 27-byte InChIKey used as the dedup key.
    pub inchikey: InchiKey,
    /// The SMILES string carried as opaque bytes.
    pub smiles: Box<[u8]>,
    /// An optional source-supplied mass (average/molar Da), enabling the
    /// shipped-column mass fast path in the filter engine. `None` if the source
    /// does not ship a usable mass column.
    pub shipped_mass: Option<f64>,
}

/// Per-source counters.
///
/// `rows_read`, `malformed`, and `missing_inchikey` are maintained by the
/// source as it reads its files; `filtered_out` and `emitted` are filled in by
/// the staging step after applying the filter engine.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SourceStats {
    /// Rows read from the underlying files (excluding headers).
    pub rows_read: u64,
    /// Rows that could not be parsed into fields / were structurally invalid.
    pub malformed: u64,
    /// Rows lacking a usable InChIKey.
    pub missing_inchikey: u64,
    /// Rows rejected by the filter engine (including unparseable SMILES).
    pub filtered_out: u64,
    /// Rows emitted to the stage file.
    pub emitted: u64,
}

/// A streaming molecular source.
///
/// Each [`next_raw`](SmilesSource::next_raw) returns the next raw record,
/// `Ok(None)` at exhaustion, or an error for an unrecoverable read failure.
/// Rows the source itself cannot use (unparseable fields, missing InChIKey) are
/// skipped internally and accounted for in [`stats`](SmilesSource::stats).
pub trait SmilesSource {
    /// Which source this is.
    fn id(&self) -> SourceId;

    /// Total expected rows if cheaply knowable, for progress reporting.
    fn size_hint_rows(&self) -> Option<u64> {
        None
    }

    /// Advances to the next raw record. `Ok(None)` means exhausted.
    fn next_raw(&mut self) -> crate::Result<Option<RawRecord>>;

    /// Returns the running per-source counters (read-level fields only; the
    /// staging step augments these with filter outcomes).
    fn stats(&self) -> &SourceStats;
}
