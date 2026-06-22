//! Pipeline orchestration: stage every source, then sort and first-wins
//! deduplicate into the configured output.

pub mod sortdedup;
pub mod stage;
pub mod writer;

use std::path::PathBuf;

use crate::config::{LotsOfSmiles, RunReport, SourceReport};
use crate::source::{SmilesSource, SourceStats};
use crate::{LosError, Result};

/// Stages all `sources` to scratch TSVs, then runs the sort/dedup pass into the
/// configured output. Source order is first-wins priority.
///
/// Sources are staged concurrently (one thread each); the sort/dedup pass runs
/// after all staging completes.
pub fn run_pipeline(
    config: &LotsOfSmiles,
    mut sources: Vec<Box<dyn SmilesSource + Send>>,
) -> Result<RunReport> {
    if sources.is_empty() {
        return Err(LosError::Config("no sources configured".into()));
    }
    if sources.len() > 10 {
        // Priority is a single ASCII digit in the stage format.
        return Err(LosError::Config("at most 10 sources are supported".into()));
    }

    std::fs::create_dir_all(config.scratch_dir()).map_err(|e| {
        LosError::io(
            format!("create scratch {}", config.scratch_dir().display()),
            e,
        )
    })?;

    let engine = crate::filter::FilterEngine::compile(config.filters());

    // Priority = position in the source list. Resolve tags and stage paths up
    // front so the staging threads only borrow shared, read-only data.
    let tags: Vec<String> = sources.iter().map(|s| s.id().tag().to_string()).collect();
    let stage_files: Vec<PathBuf> = tags
        .iter()
        .map(|tag| config.scratch_dir().join(format!("stage_{tag}.tsv")))
        .collect();

    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    let (multi, bars) = crate::progress::staging_bars(&tag_refs);

    // Stage every source concurrently. Each writes its own scratch file, so
    // there is no contention between threads.
    let stats: Vec<SourceStats> = std::thread::scope(|scope| -> Result<Vec<SourceStats>> {
        let engine = &engine;
        let handles: Vec<_> = sources
            .iter_mut()
            .enumerate()
            .map(|(priority, source)| {
                let stage_path = stage_files[priority].clone();
                let pb = bars[priority].clone();
                scope.spawn(move || {
                    stage::stage_source(
                        source.as_mut(),
                        engine,
                        priority as u8,
                        &stage_path,
                        Some(&pb),
                    )
                })
            })
            .collect();
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            out.push(
                handle
                    .join()
                    .map_err(|_| LosError::Config("a staging thread panicked".into()))??,
            );
        }
        Ok(out)
    })?;
    let _ = multi.clear();

    let mut source_reports = Vec::with_capacity(sources.len());
    for (priority, (tag, stats)) in tags.iter().zip(stats.iter()).enumerate() {
        log::info!(
            "staged {tag}: {} emitted, {} filtered, {} malformed",
            stats.emitted,
            stats.filtered_out,
            stats.malformed
        );
        source_reports.push(SourceReport {
            tag: tag.clone(),
            priority: priority as u8,
            stats: *stats,
        });
    }

    let mut writer = writer::OutputWriter::create(config.output())?;
    let dedup = sortdedup::sort_and_dedup(
        &stage_files,
        config.scratch_dir(),
        config.sort_parallelism(),
        config.sort_buffer(),
        &mut writer,
        &tag_refs,
    )?;
    writer.finish()?;

    log::info!(
        "dedup: {} unique from {} staged lines",
        dedup.unique_emitted,
        dedup.input_lines
    );

    let report = RunReport {
        sources: source_reports,
        dedup,
        output_path: config.output().path().to_path_buf(),
    };

    // Persist a JSON run report alongside the output for provenance. A failure
    // here must not invalidate the produced corpus, so it is best-effort.
    let sidecar = report_sidecar(config.output().path());
    match serde_json::to_string_pretty(&report) {
        Ok(json) => match std::fs::write(&sidecar, json) {
            Ok(()) => log::info!("wrote run report to {}", sidecar.display()),
            Err(e) => log::warn!("could not write run report to {}: {e}", sidecar.display()),
        },
        Err(e) => log::warn!("could not serialize run report: {e}"),
    }

    Ok(report)
}

/// Returns the sidecar report path for an output path by appending
/// `.report.json` to its file name.
fn report_sidecar(output: &std::path::Path) -> PathBuf {
    let mut name = output
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".report.json");
    match output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::filters::Filters;
    use crate::config::output::{Columns, OutputFormat};
    use crate::pipeline::stage::test_source::VecSource;
    use crate::source::SourceId;

    #[test]
    fn empty_sources_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LotsOfSmiles::builder()
            .scratch_dir(dir.path())
            .filters(Filters::builder().build().unwrap())
            .output(
                OutputFormat::builder()
                    .path(dir.path().join("o.smi"))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        assert!(matches!(
            cfg.run_with_sources(vec![]),
            Err(LosError::Config(_))
        ));
    }

    #[test]
    fn two_sources_dedup_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("corpus.tsv");
        let cfg = LotsOfSmiles::builder()
            .scratch_dir(dir.path().join("scratch"))
            .sort_parallelism(2)
            .sort_buffer("64M")
            .filters(Filters::builder().build().unwrap())
            .output(
                OutputFormat::builder()
                    .path(&out)
                    .columns(Columns::SmilesInchikeySource)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        // ZINC (priority 0) and PubChem (priority 1) share key A; ZINC must win.
        let zinc = Box::new(VecSource::new(
            SourceId::Zinc20,
            vec![
                ("AAAAAAAAAAAAAA-AAAAAAAAAA-A", "CCO"),
                ("CCCCCCCCCCCCCC-AAAAAAAAAA-A", "CCC"),
            ],
        ));
        let pubchem = Box::new(VecSource::new(
            SourceId::PubChem,
            vec![
                ("AAAAAAAAAAAAAA-AAAAAAAAAA-A", "OCC"),
                ("BBBBBBBBBBBBBB-AAAAAAAAAA-A", "CN"),
            ],
        ));

        let report = cfg.run_with_sources(vec![zinc, pubchem]).unwrap();
        assert_eq!(report.dedup.unique_emitted, 3);
        assert_eq!(report.dedup.duplicates_dropped, 1);
        assert_eq!(report.sources.len(), 2);

        let content = std::fs::read_to_string(&out).unwrap();
        // Key A kept ZINC's representative `CCO`.
        let expected = concat!(
            "CCO\tAAAAAAAAAAAAAA-AAAAAAAAAA-A\tzinc20\n",
            "CN\tBBBBBBBBBBBBBB-AAAAAAAAAA-A\tpubchem\n",
            "CCC\tCCCCCCCCCCCCCC-AAAAAAAAAA-A\tzinc20\n",
        );
        assert_eq!(content, expected);
    }
}
