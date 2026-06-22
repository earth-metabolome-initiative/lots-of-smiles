//! Staging: pull raw records from a [`SmilesSource`], apply the
//! [`FilterEngine`], and write survivors to a scratch TSV in the
//! `INCHIKEY \t PRIORITY \t SMILES` layout consumed by the sort/dedup stage.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use indicatif::ProgressBar;

use crate::filter::{FilterEngine, FilterOutcome};
use crate::source::{SmilesSource, SourceStats};
use crate::{LosError, Result};

/// Advance the progress bar in batches of this many emitted rows, to keep the
/// per-row cost negligible at billion-row scale.
const PROGRESS_BATCH: u64 = 65_536;

/// Drains `source` fully into `out_path`, applying `engine` to each record and
/// tagging survivors with `priority`. Returns the merged source + filter stats.
///
/// If `progress` is supplied, it is advanced by the emitted-row count.
pub fn stage_source(
    source: &mut dyn SmilesSource,
    engine: &FilterEngine,
    priority: u8,
    out_path: &Path,
    progress: Option<&ProgressBar>,
) -> Result<SourceStats> {
    let file = File::create(out_path)
        .map_err(|e| LosError::io(format!("create stage {}", out_path.display()), e))?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    let prio_byte = b'0' + priority;

    let mut filtered_out: u64 = 0;
    let mut emitted: u64 = 0;
    let mut since_tick: u64 = 0;

    while let Some(record) = source.next_raw()? {
        match engine.check(&record.smiles, record.shipped_mass) {
            FilterOutcome::Pass => {}
            FilterOutcome::Reject(_) | FilterOutcome::Malformed => {
                filtered_out += 1;
                continue;
            }
        }
        // Guard the TSV invariant: SMILES must not contain tab or newline.
        if record.smiles.contains(&b'\t') || record.smiles.contains(&b'\n') {
            filtered_out += 1;
            continue;
        }
        writer
            .write_all(record.inchikey.as_bytes())
            .map_err(|e| LosError::io("write stage", e))?;
        writer
            .write_all(&[b'\t', prio_byte, b'\t'])
            .map_err(|e| LosError::io("write stage", e))?;
        writer
            .write_all(&record.smiles)
            .map_err(|e| LosError::io("write stage", e))?;
        writer
            .write_all(b"\n")
            .map_err(|e| LosError::io("write stage", e))?;
        emitted += 1;
        since_tick += 1;
        if since_tick >= PROGRESS_BATCH
            && let Some(pb) = progress
        {
            pb.inc(since_tick);
            since_tick = 0;
        }
    }
    writer.flush().map_err(|e| LosError::io("flush stage", e))?;

    let mut stats = *source.stats();
    stats.filtered_out = filtered_out;
    stats.emitted = emitted;
    if let Some(pb) = progress {
        pb.inc(since_tick);
        pb.finish_with_message(format!("done: {emitted} emitted, {filtered_out} filtered"));
    }
    Ok(stats)
}

#[cfg(test)]
pub(crate) mod test_source {
    //! An in-memory [`SmilesSource`] for testing the pipeline without real data.

    use crate::io::InchiKey;
    use crate::source::{RawRecord, SmilesSource, SourceId, SourceStats};

    /// A source that yields a fixed list of `(inchikey, smiles)` pairs.
    pub struct VecSource {
        id: SourceId,
        items: std::vec::IntoIter<(InchiKey, Vec<u8>)>,
        stats: SourceStats,
    }

    impl VecSource {
        pub fn new(id: SourceId, items: Vec<(&str, &str)>) -> Self {
            let parsed: Vec<(InchiKey, Vec<u8>)> = items
                .into_iter()
                .map(|(ik, smi)| {
                    (
                        InchiKey::from_bytes(ik.as_bytes()).expect("valid inchikey"),
                        smi.as_bytes().to_vec(),
                    )
                })
                .collect();
            Self {
                id,
                items: parsed.into_iter(),
                stats: SourceStats::default(),
            }
        }
    }

    impl SmilesSource for VecSource {
        fn id(&self) -> SourceId {
            self.id
        }
        fn next_raw(&mut self) -> crate::Result<Option<RawRecord>> {
            match self.items.next() {
                Some((inchikey, smiles)) => {
                    self.stats.rows_read += 1;
                    Ok(Some(RawRecord {
                        inchikey,
                        smiles: smiles.into_boxed_slice(),
                        shipped_mass: None,
                    }))
                }
                None => Ok(None),
            }
        }
        fn stats(&self) -> &SourceStats {
            &self.stats
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_source::VecSource;
    use super::*;
    use crate::config::filters::Filters;
    use crate::source::SourceId;

    #[test]
    fn stages_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("stage_zinc20.tsv");
        let engine = FilterEngine::compile(&Filters::builder().build().unwrap());
        // Second item is a salt (multi-component) and must be filtered out by
        // the default single-component rule.
        let mut src = VecSource::new(
            SourceId::Zinc20,
            vec![
                ("RDHQFKQIGNGIED-UHFFFAOYSA-N", "CCO"),
                ("INCSWYKICIYAHB-UHFFFAOYSA-N", "CC.O"),
            ],
        );
        let stats = stage_source(&mut src, &engine, 0, &out, None).unwrap();
        assert_eq!(stats.rows_read, 2);
        assert_eq!(stats.emitted, 1);
        assert_eq!(stats.filtered_out, 1);
        let content = std::fs::read_to_string(&out).unwrap();
        assert_eq!(content, "RDHQFKQIGNGIED-UHFFFAOYSA-N\t0\tCCO\n");
    }
}
