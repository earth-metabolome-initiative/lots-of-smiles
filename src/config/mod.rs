//! Top-level pipeline configuration, its builder, and the run report.

pub mod credentials;
pub mod filters;
pub mod output;

use std::path::{Path, PathBuf};

use crate::config::filters::Filters;
use crate::config::output::OutputFormat;
use crate::pipeline::sortdedup::DedupStats;
use crate::source::enamine::{EnamineConfig, EnamineSource};
use crate::source::pubchem::{PubChemConfig, PubChemSource};
use crate::source::zinc20::{Zinc20Config, Zinc20Source};
use crate::source::{SmilesSource, SourceStats};
use crate::{LosError, Result};

/// Default `sort -S` buffer specification.
const DEFAULT_SORT_BUFFER: &str = "1G";

/// A fully validated pipeline configuration. Build via [`LotsOfSmiles::builder`].
///
/// Sources are configured on the builder ([`zinc20`], [`pubchem`], [`enamine`])
/// and opened when [`run`] is called. For full control, pre-opened sources can
/// instead be passed to [`run_with_sources`].
///
/// [`zinc20`]: LotsOfSmilesBuilder::zinc20
/// [`pubchem`]: LotsOfSmilesBuilder::pubchem
/// [`enamine`]: LotsOfSmilesBuilder::enamine
/// [`run`]: LotsOfSmiles::run
/// [`run_with_sources`]: LotsOfSmiles::run_with_sources
#[derive(Debug, Clone)]
pub struct LotsOfSmiles {
    scratch_dir: PathBuf,
    sort_parallelism: usize,
    sort_buffer: String,
    filters: Filters,
    output: OutputFormat,
    zinc20: Option<Zinc20Config>,
    pubchem: Option<PubChemConfig>,
    enamine: Option<EnamineConfig>,
}

impl LotsOfSmiles {
    /// Starts a new [`LotsOfSmilesBuilder`].
    pub fn builder() -> LotsOfSmilesBuilder {
        LotsOfSmilesBuilder::default()
    }

    /// The scratch directory used for stage files and `sort` spill.
    pub fn scratch_dir(&self) -> &Path {
        &self.scratch_dir
    }
    /// The configured `sort --parallel` degree.
    pub fn sort_parallelism(&self) -> usize {
        self.sort_parallelism
    }
    /// The configured `sort -S` buffer specification.
    pub fn sort_buffer(&self) -> &str {
        &self.sort_buffer
    }
    /// The configured filters.
    pub fn filters(&self) -> &Filters {
        &self.filters
    }
    /// The configured output format.
    pub fn output(&self) -> &OutputFormat {
        &self.output
    }

    /// The configured ZINC20 source, if any.
    pub fn zinc20(&self) -> Option<&Zinc20Config> {
        self.zinc20.as_ref()
    }

    /// The configured PubChem source, if any.
    pub fn pubchem(&self) -> Option<&PubChemConfig> {
        self.pubchem.as_ref()
    }

    /// The configured Enamine source, if any.
    pub fn enamine(&self) -> Option<&EnamineConfig> {
        self.enamine.as_ref()
    }

    /// Opens every configured source adapter and runs the pipeline.
    ///
    /// Sources are staged in a fixed precedence order (ZINC20, then PubChem,
    /// then Enamine); earlier sources win InChIKey collisions.
    pub fn run(&self) -> Result<RunReport> {
        let mut sources: Vec<Box<dyn SmilesSource + Send>> = Vec::new();
        if let Some(cfg) = &self.zinc20 {
            sources.push(Box::new(Zinc20Source::open(cfg)?));
        }
        if let Some(cfg) = &self.pubchem {
            sources.push(Box::new(PubChemSource::open(cfg)?));
        }
        if let Some(cfg) = &self.enamine {
            // Enamine needs the filters to decide which HAC files to fetch.
            sources.push(Box::new(EnamineSource::open(cfg, &self.filters)?));
        }
        if sources.is_empty() {
            return Err(LosError::Config("no sources configured".into()));
        }
        self.run_with_sources(sources)
    }

    /// Runs the pipeline over the supplied, already-opened sources.
    ///
    /// Sources are staged in the order given; that order is their first-wins
    /// priority (earlier sources win InChIKey collisions).
    pub fn run_with_sources(
        &self,
        sources: Vec<Box<dyn SmilesSource + Send>>,
    ) -> Result<RunReport> {
        crate::pipeline::run_pipeline(self, sources)
    }
}

/// Builder for [`LotsOfSmiles`].
///
/// Defaults: `sort_parallelism = num_cpus`, `sort_buffer = "1G"`. The scratch
/// directory, filters, and output are required.
#[derive(Debug, Clone, Default)]
pub struct LotsOfSmilesBuilder {
    scratch_dir: Option<PathBuf>,
    sort_parallelism: Option<usize>,
    sort_buffer: Option<String>,
    filters: Option<Filters>,
    output: Option<OutputFormat>,
    zinc20: Option<Zinc20Config>,
    pubchem: Option<PubChemConfig>,
    enamine: Option<EnamineConfig>,
}

impl LotsOfSmilesBuilder {
    /// Sets the scratch directory for stage files and `sort` spill. It is
    /// created if absent.
    pub fn scratch_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.scratch_dir = Some(path.into());
        self
    }
    /// Sets the `sort --parallel` degree (default: number of CPUs).
    pub fn sort_parallelism(mut self, n: usize) -> Self {
        self.sort_parallelism = Some(n);
        self
    }
    /// Sets the `sort -S` buffer specification, e.g. `"16G"` (default `"1G"`).
    pub fn sort_buffer(mut self, spec: impl Into<String>) -> Self {
        self.sort_buffer = Some(spec.into());
        self
    }
    /// Sets the filter configuration.
    pub fn filters(mut self, filters: Filters) -> Self {
        self.filters = Some(filters);
        self
    }
    /// Sets the output format.
    pub fn output(mut self, output: OutputFormat) -> Self {
        self.output = Some(output);
        self
    }
    /// Configures the ZINC20 source.
    pub fn zinc20(mut self, config: Zinc20Config) -> Self {
        self.zinc20 = Some(config);
        self
    }
    /// Configures the PubChem source.
    pub fn pubchem(mut self, config: PubChemConfig) -> Self {
        self.pubchem = Some(config);
        self
    }
    /// Configures the Enamine REAL source.
    pub fn enamine(mut self, config: EnamineConfig) -> Self {
        self.enamine = Some(config);
        self
    }

    /// Validates and finalizes the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingField`] if scratch dir, filters, or output is
    /// unset, or [`LosError::Config`] if `sort_parallelism` is zero.
    pub fn build(self) -> Result<LotsOfSmiles> {
        let scratch_dir = self
            .scratch_dir
            .ok_or(LosError::MissingField("scratch_dir"))?;
        let filters = self.filters.ok_or(LosError::MissingField("filters"))?;
        let output = self.output.ok_or(LosError::MissingField("output"))?;
        let sort_parallelism = self.sort_parallelism.unwrap_or_else(num_cpus::get);
        if sort_parallelism == 0 {
            return Err(LosError::Config("sort_parallelism must be > 0".into()));
        }
        Ok(LotsOfSmiles {
            scratch_dir,
            sort_parallelism,
            sort_buffer: self
                .sort_buffer
                .unwrap_or_else(|| DEFAULT_SORT_BUFFER.to_string()),
            filters,
            output,
            zinc20: self.zinc20,
            pubchem: self.pubchem,
            enamine: self.enamine,
        })
    }
}

/// Per-source entry in the [`RunReport`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceReport {
    /// The source tag (e.g. `"zinc20"`).
    pub tag: String,
    /// The first-wins priority assigned to the source (0 = highest).
    pub priority: u8,
    /// The source's final counters.
    pub stats: SourceStats,
}

/// Summary of a completed pipeline run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunReport {
    /// Per-source staging stats, in priority order.
    pub sources: Vec<SourceReport>,
    /// Sort/dedup stats.
    pub dedup: DedupStats,
    /// The output path (single) or path stem (sharded).
    pub output_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::output::OutputFormat;

    fn minimal_output(dir: &std::path::Path) -> OutputFormat {
        OutputFormat::builder()
            .path(dir.join("out.smi"))
            .build()
            .unwrap()
    }

    #[test]
    fn requires_fields() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            LotsOfSmiles::builder().build(),
            Err(LosError::MissingField(_))
        ));
        assert!(matches!(
            LotsOfSmiles::builder().scratch_dir(dir.path()).build(),
            Err(LosError::MissingField(_))
        ));
    }

    #[test]
    fn builds_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LotsOfSmiles::builder()
            .scratch_dir(dir.path())
            .filters(Filters::builder().build().unwrap())
            .output(minimal_output(dir.path()))
            .build()
            .unwrap();
        assert!(cfg.sort_parallelism() >= 1);
        assert_eq!(cfg.sort_buffer(), "1G");
    }

    #[test]
    fn rejects_zero_parallelism() {
        let dir = tempfile::tempdir().unwrap();
        let e = LotsOfSmiles::builder()
            .scratch_dir(dir.path())
            .sort_parallelism(0)
            .filters(Filters::builder().build().unwrap())
            .output(minimal_output(dir.path()))
            .build();
        assert!(matches!(e, Err(LosError::Config(_))));
    }
}
