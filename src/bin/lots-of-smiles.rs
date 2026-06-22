//! Command-line interface for the `lots-of-smiles` pipeline.
//!
//! Wires the global, filter, and output flags into a [`LotsOfSmiles`]
//! configuration and runs the configured sources (ZINC20, PubChem, Enamine
//! REAL). Also exposes the `canonicalize` subcommand.

mod canonicalize;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use lots_of_smiles::{
    AtomCount, Columns, Compression, ElementSet, EnamineConfig, EnamineCredentials, Filters,
    LotsOfSmiles, MassKind, OutputFormat, PubChemConfig, Sharding, Zinc20Config,
};

use crate::canonicalize::CanonicalizeArgs;

/// Build a large, deduplicated collection of real-world SMILES.
#[derive(Debug, Parser)]
#[command(name = "lots-of-smiles", version, about)]
struct Cli {
    /// Optional subcommand. When omitted, the top-level flags drive a corpus
    /// build.
    #[command(subcommand)]
    command: Option<Command>,

    /// Sources to include (repeatable).
    #[arg(long, value_enum)]
    source: Vec<SourceArg>,

    /// Scratch directory for stage files and `sort` spill.
    #[arg(long, default_value = "/mnt/nvme/los/scratch")]
    scratch_dir: PathBuf,

    /// `sort --parallel` degree (defaults to the number of CPUs).
    #[arg(long)]
    sort_parallelism: Option<usize>,

    /// `sort -S` buffer specification, e.g. `16G`.
    #[arg(long, default_value = "1G")]
    sort_buffer: String,

    /// ZINC20 tranche-tree root (`<XX>/<YYYY>.txt`).
    #[arg(long, default_value = lots_of_smiles::source::zinc20::DEFAULT_ROOT)]
    zinc20_root: PathBuf,

    /// PubChem local `CID-SMILES.gz` (required with `--source pubchem`).
    #[arg(long)]
    pubchem_cid_smiles_gz: Option<PathBuf>,
    /// Path where PubChem `CID-InChI-Key.gz` is stored (downloaded if absent).
    #[arg(long, default_value = "/mnt/nvme/los/CID-InChI-Key.gz")]
    pubchem_cid_inchikey_gz: PathBuf,
    /// Download URL for PubChem `CID-InChI-Key.gz`.
    #[arg(long, default_value = lots_of_smiles::source::pubchem::DEFAULT_CID_INCHIKEY_URL)]
    pubchem_cid_inchikey_url: String,

    /// Directory holding (or to download) the Enamine REAL `.cxsmiles.bz2` files.
    #[arg(long, default_value = "/mnt/nvme/los/enamine")]
    enamine_dir: PathBuf,
    /// Enamine.net username (or set `ENAMINE_USERNAME`). Only needed to download.
    #[arg(long, env = "ENAMINE_USERNAME")]
    enamine_username: Option<String>,
    /// Enamine.net password (or set `ENAMINE_PASSWORD`). Only needed to download.
    #[arg(long, env = "ENAMINE_PASSWORD", hide_env_values = true)]
    enamine_password: Option<String>,
    /// Delete each Enamine `.bz2` after it has been streamed (saves disk).
    #[arg(long)]
    enamine_delete_after_extract: bool,

    // --- Filters ---
    /// Lower bound on molecular mass (Da).
    #[arg(long)]
    min_mass_da: Option<f64>,
    /// Upper bound on molecular mass (Da).
    #[arg(long)]
    max_mass_da: Option<f64>,
    /// Which mass definition the bounds use.
    #[arg(long, value_enum, default_value_t = MassKindArg::Monoisotopic)]
    mass_kind: MassKindArg,
    /// Lower bound on atom count.
    #[arg(long)]
    min_atoms: Option<u32>,
    /// Upper bound on atom count.
    #[arg(long)]
    max_atoms: Option<u32>,
    /// Whether atom-count bounds count heavy atoms or all atoms.
    #[arg(long, value_enum, default_value_t = AtomCountArg::Heavy)]
    atom_count_mode: AtomCountArg,
    /// Allowed element symbols (comma-separated). Mutually exclusive with the
    /// blacklist.
    #[arg(long, value_delimiter = ',')]
    whitelist_elements: Vec<String>,
    /// Forbidden element symbols (comma-separated). Mutually exclusive with the
    /// whitelist.
    #[arg(long, value_delimiter = ',')]
    blacklist_elements: Vec<String>,
    /// Allow explicit isotopes (default: forbid).
    #[arg(long)]
    allow_isotopes: bool,
    /// Allow radicals (default: forbid).
    #[arg(long)]
    allow_radicals: bool,
    /// Require a single connected component (default: true).
    #[arg(long, default_value_t = true)]
    require_single_component: bool,
    /// Upper bound on absolute net formal charge.
    #[arg(long)]
    max_abs_charge: Option<u32>,

    /// Only download each source's prerequisite files (Enamine buckets,
    /// PubChem CID-InChI-Key.gz), then exit without building a corpus.
    #[arg(long)]
    download_only: bool,

    // --- Output ---
    /// Output path (single file) or shard path stem (sharded). Required unless
    /// `--download-only`.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Columns to emit.
    #[arg(long, value_enum, default_value_t = ColumnsArg::SmilesOnly)]
    columns: ColumnsArg,
    /// Output compression.
    #[arg(long, value_enum, default_value_t = CompressionArg::None)]
    compression: CompressionArg,
    /// zstd level (1..=22), used when `--compression zstd`.
    #[arg(long, default_value_t = 9)]
    zstd_level: i32,
    /// Number of output shards (1 = single file).
    #[arg(long, default_value_t = 1)]
    shards: u32,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Canonicalize a stream of SMILES (stdin to stdout), for piping into
    /// `sort -u` after a corpus build.
    Canonicalize(CanonicalizeArgs),
    /// Deposit the canonical corpus on Zenodo with full dataset metadata.
    #[cfg(feature = "zenodo")]
    PublishZenodo(PublishZenodoArgs),
}

/// Flags for the `publish-zenodo` subcommand.
///
/// Identity and credentials (token, creator name, ORCID, affiliation) are read
/// from the environment, typically a `.env` file, not from flags. See
/// `.env.example` for the variable names.
#[cfg(feature = "zenodo")]
#[derive(Debug, clap::Args)]
struct PublishZenodoArgs {
    /// Corpus file(s) to upload. Pass the split parts, for example
    /// `corpus.canonical.smi.zst.part-*`, to deposit them in one record.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Deposit to the sandbox service instead of production.
    #[arg(long)]
    sandbox: bool,
    /// Resume into an existing draft deposition id instead of creating a new
    /// one (use after a failed upload to avoid orphaning a fresh draft).
    #[arg(long)]
    deposition: Option<u64>,
    /// Publish the deposition after upload (default: leave it as a draft).
    #[arg(long)]
    publish: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceArg {
    Zinc20,
    Pubchem,
    Enamine,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MassKindArg {
    Monoisotopic,
    Average,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AtomCountArg {
    Heavy,
    Total,
}

// Variant names intentionally mirror the public `Columns` enum and the
// `smiles-*` CLI value names users type.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColumnsArg {
    SmilesOnly,
    SmilesInchikey,
    SmilesInchikeySource,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompressionArg {
    None,
    Gzip,
    Zstd,
}

fn build_filters(cli: &Cli) -> lots_of_smiles::Result<Filters> {
    let mut b = Filters::builder()
        .mass_kind(match cli.mass_kind {
            MassKindArg::Monoisotopic => MassKind::Monoisotopic,
            MassKindArg::Average => MassKind::Average,
        })
        .atom_count_mode(match cli.atom_count_mode {
            AtomCountArg::Heavy => AtomCount::Heavy,
            AtomCountArg::Total => AtomCount::Total,
        })
        .allow_isotopes(cli.allow_isotopes)
        .allow_radicals(cli.allow_radicals)
        .require_single_component(cli.require_single_component);

    if let Some(v) = cli.min_mass_da {
        b = b.min_mass_da(v);
    }
    if let Some(v) = cli.max_mass_da {
        b = b.max_mass_da(v);
    }
    if let Some(v) = cli.min_atoms {
        b = b.min_atoms(v);
    }
    if let Some(v) = cli.max_atoms {
        b = b.max_atoms(v);
    }
    if let Some(v) = cli.max_abs_charge {
        b = b.max_abs_charge(v);
    }

    if !cli.whitelist_elements.is_empty() && !cli.blacklist_elements.is_empty() {
        return Err(lots_of_smiles::LosError::Config(
            "--whitelist-elements and --blacklist-elements are mutually exclusive".into(),
        ));
    }
    if !cli.whitelist_elements.is_empty() {
        b = b.elements(ElementSet::Whitelist(parse_elements(
            &cli.whitelist_elements,
        )?));
    } else if !cli.blacklist_elements.is_empty() {
        b = b.elements(ElementSet::Blacklist(parse_elements(
            &cli.blacklist_elements,
        )?));
    }
    b.build()
}

fn parse_elements(symbols: &[String]) -> lots_of_smiles::Result<Vec<elements_rs::Element>> {
    symbols
        .iter()
        .map(|s| {
            s.parse::<elements_rs::Element>().map_err(|_| {
                lots_of_smiles::LosError::Config(format!("unknown element symbol `{s}`"))
            })
        })
        .collect()
}

fn build_output(cli: &Cli) -> lots_of_smiles::Result<OutputFormat> {
    let compression = match cli.compression {
        CompressionArg::None => Compression::None,
        CompressionArg::Gzip => Compression::Gzip,
        CompressionArg::Zstd => Compression::Zstd {
            level: cli.zstd_level,
        },
    };
    let sharding = match cli.shards {
        0 | 1 => Sharding::Single,
        n => Sharding::Shards(std::num::NonZeroU32::new(n).expect("n > 1")),
    };
    let columns = match cli.columns {
        ColumnsArg::SmilesOnly => Columns::SmilesOnly,
        ColumnsArg::SmilesInchikey => Columns::SmilesInchikey,
        ColumnsArg::SmilesInchikeySource => Columns::SmilesInchikeySource,
    };
    let path = cli.output.clone().ok_or_else(|| {
        lots_of_smiles::LosError::Config("--output is required (unless --download-only)".into())
    })?;
    OutputFormat::builder()
        .path(path)
        .columns(columns)
        .compression(compression)
        .sharding(sharding)
        .build()
}

/// Builds the Enamine config from the CLI flags (credentials optional).
fn build_enamine(cli: &Cli) -> lots_of_smiles::Result<EnamineConfig> {
    let mut enamine = EnamineConfig::builder()
        .download_dir(&cli.enamine_dir)
        .delete_after_extract(cli.enamine_delete_after_extract);
    if cli.enamine_username.is_some() || cli.enamine_password.is_some() {
        let mut creds = EnamineCredentials::builder();
        if let Some(u) = &cli.enamine_username {
            creds = creds.username(u);
        }
        if let Some(p) = &cli.enamine_password {
            creds = creds.password(p);
        }
        enamine = enamine.credentials(creds.build()?);
    }
    enamine.build()
}

/// Downloads each configured source's prerequisite files, then returns.
fn run_download_only(cli: &Cli) -> lots_of_smiles::Result<()> {
    let filters = build_filters(cli)?;
    for source in &cli.source {
        match source {
            SourceArg::Zinc20 => log::info!("zinc20: local source, nothing to download"),
            SourceArg::Pubchem => {
                lots_of_smiles::download::ensure_file(
                    &cli.pubchem_cid_inchikey_url,
                    &cli.pubchem_cid_inchikey_gz,
                )?;
            }
            SourceArg::Enamine => {
                let cfg = build_enamine(cli)?;
                let files = lots_of_smiles::enamine_ensure_downloaded(&cfg, &filters)?;
                log::info!(
                    "enamine: {} file(s) present in {}",
                    files.len(),
                    cli.enamine_dir.display()
                );
            }
        }
    }
    log::info!("download-only: complete");
    Ok(())
}

/// Deposits the corpus on Zenodo and prints the resulting draft or record.
#[cfg(feature = "zenodo")]
fn run_publish_zenodo(args: &PublishZenodoArgs) -> lots_of_smiles::Result<()> {
    use lots_of_smiles::{ZenodoTarget, ZenodoUpload};

    let target = if args.sandbox {
        ZenodoTarget::Sandbox
    } else {
        ZenodoTarget::Production
    };
    // Identity and token come from the environment (loaded from `.env`).
    let mut builder = ZenodoUpload::builder()
        .files(args.files.clone())
        .target(target)
        .publish(args.publish);
    if let Some(id) = args.deposition {
        builder = builder.deposition_id(id);
    }
    let deposit = builder.build()?.upload()?;
    let state = if deposit.published {
        "published"
    } else {
        "draft created (review and publish it in the Zenodo UI)"
    };
    log::info!("zenodo: {state}");
    println!("deposition id: {}", deposit.deposition_id);
    if let Some(doi) = &deposit.doi {
        println!("doi:           {doi}");
    }
    println!("deposit url:   {}", deposit.deposit_url);
    Ok(())
}

fn run() -> lots_of_smiles::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::Canonicalize(args)) => return canonicalize::run(args),
        #[cfg(feature = "zenodo")]
        Some(Command::PublishZenodo(args)) => return run_publish_zenodo(args),
        None => {}
    }

    if cli.source.is_empty() {
        return Err(lots_of_smiles::LosError::Config(
            "no --source selected".into(),
        ));
    }

    if cli.download_only {
        return run_download_only(&cli);
    }

    let filters = build_filters(&cli)?;
    let output = build_output(&cli)?;

    let mut builder = LotsOfSmiles::builder()
        .scratch_dir(&cli.scratch_dir)
        .sort_buffer(&cli.sort_buffer);
    if let Some(p) = cli.sort_parallelism {
        builder = builder.sort_parallelism(p);
    }
    builder = builder.filters(filters).output(output);

    for source in &cli.source {
        match source {
            SourceArg::Zinc20 => {
                builder = builder.zinc20(Zinc20Config::builder().root(&cli.zinc20_root).build()?);
            }
            SourceArg::Pubchem => {
                let cid_smiles = cli.pubchem_cid_smiles_gz.clone().ok_or_else(|| {
                    lots_of_smiles::LosError::Config(
                        "--pubchem-cid-smiles-gz is required with --source pubchem".into(),
                    )
                })?;
                builder = builder.pubchem(
                    PubChemConfig::builder()
                        .cid_smiles_gz(cid_smiles)
                        .cid_inchikey_gz(&cli.pubchem_cid_inchikey_gz)
                        .cid_inchikey_url(&cli.pubchem_cid_inchikey_url)
                        .build()?,
                );
            }
            SourceArg::Enamine => {
                builder = builder.enamine(build_enamine(&cli)?);
            }
        }
    }

    let config = builder.build()?;
    let report = config.run()?;

    log::info!(
        "done: {} unique molecules from {} staged rows -> {}",
        report.dedup.unique_emitted,
        report.dedup.input_lines,
        report.output_path.display(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize report")
    );
    Ok(())
}

fn main() -> ExitCode {
    // Load `.env` (if present) before reading any environment: this populates
    // env-backed flags like ENAMINE_USERNAME/ENAMINE_PASSWORD and RUST_LOG.
    // Existing process environment variables take precedence over `.env`.
    match dotenvy::dotenv() {
        Ok(path) => eprintln!("loaded environment from {}", path.display()),
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("warning: failed to load .env: {e}"),
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
