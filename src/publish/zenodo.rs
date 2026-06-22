//! Zenodo deposition flow for the canonical SMILES corpus.
//!
//! [`ZenodoUpload`] wraps the [`zenodo-rs`](https://crates.io/crates/zenodo-rs)
//! client into a single, metadata-rich deposition: it creates a draft, attaches
//! fully populated dataset metadata (title, description, creators, license,
//! keywords, related software identifiers, and the required Enamine attribution),
//! uploads the corpus file with a progress bar, and stops at the draft stage so
//! the record can be reviewed in the Zenodo UI before publishing. Passing
//! [`ZenodoUploadBuilder::publish`] flips the final publish action on.
//!
//! Authentication uses a Zenodo personal access token (scopes `deposit:write`
//! and `deposit:actions`), taken from the builder or, by default, from the
//! `ZENODO_TOKEN` environment variable (`ZENODO_SANDBOX_TOKEN` for the sandbox).

use std::path::{Path, PathBuf};

use indicatif::{ProgressBar, ProgressStyle};
use zenodo_rs::model::Deposition;
use zenodo_rs::{
    AccessRight, Auth, BucketUrl, Creator, DepositMetadataUpdate, DepositionId, RelatedIdentifier,
    UploadType, ZenodoClient,
};

use crate::{LosError, Result};

/// Default Zenodo record title for the corpus.
pub const DEFAULT_TITLE: &str = "A large deduplicated collection of real-world SMILES";
/// Default record version.
pub const DEFAULT_VERSION: &str = "1.0.0";
/// Default creator name, in Zenodo `Family, Given` form.
pub const DEFAULT_CREATOR: &str = "Cappelletti, Luca";
/// Zenodo license identifier for Creative Commons Attribution 4.0 International.
pub const LICENSE_ID: &str = "cc-by-4.0";
/// GitHub repository of the pipeline that produced the corpus.
pub const PIPELINE_REPO: &str = "https://github.com/earth-metabolome-initiative/lots-of-smiles";
/// GitHub repository of the SMILES parser used for canonicalization.
pub const PARSER_REPO: &str = "https://github.com/earth-metabolome-initiative/smiles-parser";
/// sha256 of the reference `corpus.canonical.smi.zst` described by the default
/// metadata.
pub const REFERENCE_SHA256: &str =
    "8924d2a171562d8ef23649ef49c5b5790c795fe4c952071761d6209380d091e2";

/// Environment variable for the creator name (`Family, Given`).
pub const ENV_CREATOR: &str = "ZENODO_CREATOR";
/// Environment variable for the creator ORCID.
pub const ENV_ORCID: &str = "ZENODO_ORCID";
/// Environment variable for the creator affiliation.
pub const ENV_AFFILIATION: &str = "ZENODO_AFFILIATION";

/// Which Zenodo service to deposit to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Production service at <https://zenodo.org>.
    Production,
    /// Sandbox service at <https://sandbox.zenodo.org>, for testing.
    Sandbox,
}

impl Target {
    /// Environment variable holding the access token for this service.
    fn token_env(self) -> &'static str {
        match self {
            Target::Production => "ZENODO_TOKEN",
            Target::Sandbox => "ZENODO_SANDBOX_TOKEN",
        }
    }

    /// Base URL used to build the human-facing deposit URL.
    fn deposit_base(self) -> &'static str {
        match self {
            Target::Production => "https://zenodo.org",
            Target::Sandbox => "https://sandbox.zenodo.org",
        }
    }
}

/// The result of a deposition.
#[derive(Debug, Clone)]
pub struct ZenodoDeposit {
    /// Numeric Zenodo deposition identifier.
    pub deposition_id: u64,
    /// The reserved (draft) or minted (published) DOI, if Zenodo returned one.
    pub doi: Option<String>,
    /// Human-facing deposit URL for reviewing or publishing the draft.
    pub deposit_url: String,
    /// Whether the deposition was published (`true`) or left as a draft.
    pub published: bool,
}

/// A configured, validated Zenodo deposition of the corpus. Build via
/// [`ZenodoUpload::builder`].
#[derive(Debug, Clone)]
pub struct ZenodoUpload {
    files: Vec<PathBuf>,
    target: Target,
    token: Option<String>,
    deposition_id: Option<u64>,
    publish: bool,
    title: String,
    version: String,
    creator: String,
    orcid: Option<String>,
    affiliation: Option<String>,
    description_html: String,
}

impl ZenodoUpload {
    /// Starts a new [`ZenodoUploadBuilder`].
    pub fn builder() -> ZenodoUploadBuilder {
        ZenodoUploadBuilder::default()
    }

    /// Runs the deposition on a freshly built Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime cannot be created or any step of the
    /// deposition (authentication, draft creation, metadata, upload, publish)
    /// fails.
    pub fn upload(&self) -> Result<ZenodoDeposit> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| LosError::io("creating async runtime", e))?;
        runtime.block_on(self.upload_async())
    }

    /// Runs the deposition on the current Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if any step of the deposition fails.
    pub async fn upload_async(&self) -> Result<ZenodoDeposit> {
        let client = self.client()?;
        let metadata = self.metadata()?;

        // Reuse an existing draft when an id is given (so a retry after a failed
        // upload does not orphan a new draft), otherwise create a fresh one.
        let draft = match self.deposition_id {
            Some(id) => client
                .get_deposition(DepositionId::from(id))
                .await
                .map_err(publish_err)?,
            None => client.create_deposition().await.map_err(publish_err)?,
        };
        let id = draft.id;
        // Log the id and URL up front so a mid-upload failure is recoverable.
        log::info!(
            "zenodo: draft {} at {}/deposit/{} (retry uploads with --deposition {})",
            id,
            self.target.deposit_base(),
            id.0,
            id.0
        );

        let draft = client
            .update_metadata(id, &metadata)
            .await
            .map_err(publish_err)?;

        let bucket = draft.links.bucket.clone().ok_or_else(|| {
            LosError::Publish("Zenodo did not return a bucket URL for the draft".into())
        })?;

        // Files already in the draft (name -> size), to skip on resume.
        let existing: std::collections::HashMap<String, u64> = client
            .list_files(id)
            .await
            .map_err(publish_err)?
            .into_iter()
            .map(|f| (f.filename, f.filesize))
            .collect();

        for path in &self.files {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| {
                    LosError::Publish(format!("file has no valid name: {}", path.display()))
                })?
                .to_string();
            let len = std::fs::metadata(path)
                .map_err(|e| LosError::io("reading file size", e))?
                .len();
            if existing.get(&name) == Some(&len) {
                log::info!("zenodo: {name} already uploaded ({len} bytes), skipping");
                continue;
            }
            self.upload_one(&bucket, &name, path, len, id).await?;
        }

        let mut doi = reserved_doi(&draft);
        let mut published = false;
        if self.publish {
            let record = client.publish(id).await.map_err(publish_err)?;
            doi = record.doi.map(|d| d.to_string()).or(doi);
            published = true;
        }

        Ok(ZenodoDeposit {
            deposition_id: id.0,
            doi,
            deposit_url: format!("{}/deposit/{}", self.target.deposit_base(), id.0),
            published,
        })
    }

    /// Uploads one file into the draft bucket, retrying on failure.
    ///
    /// Each attempt uses a fresh client (hence a fresh connection), since large
    /// cumulative transfers tend to fail per connection rather than per request.
    async fn upload_one(
        &self,
        bucket: &BucketUrl,
        name: &str,
        path: &Path,
        len: u64,
        id: DepositionId,
    ) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 6;
        let mut attempt = 1u32;
        loop {
            log::info!("zenodo: uploading {name} ({len} bytes, attempt {attempt}/{MAX_ATTEMPTS})");
            let client = self.client()?;
            match client
                .upload_path_with_progress(bucket, name, path, upload_bar(len))
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let secs = (1u64 << attempt.min(5)).min(30);
                    log::warn!(
                        "zenodo: upload of {name} failed (attempt {attempt}/{MAX_ATTEMPTS}): {e}; retrying in {secs}s"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                    attempt += 1;
                }
                Err(e) => {
                    return Err(LosError::Publish(format!(
                        "upload of {name} to draft {} failed after {MAX_ATTEMPTS} attempts ({e}); resume with --deposition {}",
                        id.0, id.0
                    )));
                }
            }
        }
    }

    /// Builds the authenticated Zenodo client for the configured target.
    fn client(&self) -> Result<ZenodoClient> {
        let token = match &self.token {
            Some(token) => token.clone(),
            None => std::env::var(self.target.token_env()).map_err(|_| {
                LosError::Credentials(format!(
                    "no Zenodo token: set {} or pass one to the builder",
                    self.target.token_env()
                ))
            })?,
        };
        let mut builder = ZenodoClient::builder(Auth::new(token));
        if self.target == Target::Sandbox {
            builder = builder.sandbox();
        }
        builder
            .user_agent(concat!("lots-of-smiles/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(publish_err)
    }

    /// Assembles the full dataset metadata for the deposition.
    fn metadata(&self) -> Result<DepositMetadataUpdate> {
        let mut creator = Creator::builder().name(self.creator.as_str());
        if let Some(orcid) = &self.orcid {
            creator = creator.orcid(orcid.as_str());
        }
        if let Some(affiliation) = &self.affiliation {
            creator = creator.affiliation(affiliation.as_str());
        }
        let creator = creator.build().map_err(publish_err)?;

        let pipeline = software_identifier(PIPELINE_REPO)?;
        let parser = software_identifier(PARSER_REPO)?;

        DepositMetadataUpdate::builder()
            .title(self.title.as_str())
            .upload_type(UploadType::Dataset)
            .version(self.version.as_str())
            .description_html(self.description_html.as_str())
            .creator(creator)
            .access_right(AccessRight::Open)
            .license(LICENSE_ID)
            .keywords(keywords())
            .related_identifier(pipeline)
            .related_identifier(parser)
            .notes(NOTES)
            .build()
            .map_err(publish_err)
    }
}

/// Builder for [`ZenodoUpload`].
///
/// At least one file is required (pass the split parts to deposit a large corpus
/// as multiple files in one record). Everything else has a corpus-appropriate
/// default: production target, draft only (no publish), the canonical title,
/// version, and a rich description. The depositor identity is read from the
/// environment when not set explicitly: the token from `ZENODO_TOKEN`
/// (`ZENODO_SANDBOX_TOKEN` for the sandbox), and the creator details from
/// [`ENV_CREATOR`], [`ENV_ORCID`], and [`ENV_AFFILIATION`]. These are typically
/// supplied through a `.env` file loaded by the CLI.
#[derive(Debug, Clone, Default)]
pub struct ZenodoUploadBuilder {
    files: Vec<PathBuf>,
    target: Option<Target>,
    token: Option<String>,
    deposition_id: Option<u64>,
    publish: bool,
    title: Option<String>,
    version: Option<String>,
    creator: Option<String>,
    orcid: Option<String>,
    affiliation: Option<String>,
    description_html: Option<String>,
}

impl ZenodoUploadBuilder {
    /// Adds one file to upload. Each file is stored under its own base name.
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.files.push(path.into());
        self
    }
    /// Adds several files to upload (for example the split parts of a corpus).
    pub fn files<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.files.extend(paths.into_iter().map(Into::into));
        self
    }
    /// Selects the production or sandbox service (defaults to production).
    pub fn target(mut self, target: Target) -> Self {
        self.target = Some(target);
        self
    }
    /// Sets the access token explicitly (defaults to the environment variable
    /// for the selected target).
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
    /// Reuses an existing draft deposition instead of creating a new one. Use
    /// this to resume after a failed upload without orphaning a fresh draft.
    pub fn deposition_id(mut self, id: u64) -> Self {
        self.deposition_id = Some(id);
        self
    }
    /// Publishes the deposition after upload (defaults to leaving it as a draft).
    pub fn publish(mut self, yes: bool) -> Self {
        self.publish = yes;
        self
    }
    /// Overrides the record title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    /// Overrides the record version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
    /// Overrides the creator name (Zenodo `Family, Given` form).
    pub fn creator(mut self, creator: impl Into<String>) -> Self {
        self.creator = Some(creator.into());
        self
    }
    /// Sets the creator ORCID.
    pub fn orcid(mut self, orcid: impl Into<String>) -> Self {
        self.orcid = Some(orcid.into());
        self
    }
    /// Sets the creator affiliation.
    pub fn affiliation(mut self, affiliation: impl Into<String>) -> Self {
        self.affiliation = Some(affiliation.into());
        self
    }
    /// Overrides the HTML description (defaults to the corpus description).
    pub fn description_html(mut self, html: impl Into<String>) -> Self {
        self.description_html = Some(html.into());
        self
    }

    /// Validates and finalizes the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingField`] if no file was set, and
    /// [`LosError::MissingPath`] if any file does not exist.
    pub fn build(self) -> Result<ZenodoUpload> {
        if self.files.is_empty() {
            return Err(LosError::MissingField("zenodo.files"));
        }
        for path in &self.files {
            if !path.is_file() {
                return Err(LosError::MissingPath(path.clone()));
            }
        }
        Ok(ZenodoUpload {
            files: self.files,
            target: self.target.unwrap_or(Target::Production),
            token: self.token,
            deposition_id: self.deposition_id,
            publish: self.publish,
            title: self.title.unwrap_or_else(|| DEFAULT_TITLE.to_string()),
            version: self.version.unwrap_or_else(|| DEFAULT_VERSION.to_string()),
            creator: self
                .creator
                .or_else(|| env_value(ENV_CREATOR))
                .unwrap_or_else(|| DEFAULT_CREATOR.to_string()),
            orcid: self.orcid.or_else(|| env_value(ENV_ORCID)),
            affiliation: self.affiliation.or_else(|| env_value(ENV_AFFILIATION)),
            description_html: self
                .description_html
                .unwrap_or_else(default_description_html),
        })
    }
}

/// Reads a non-empty environment variable, or `None`.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Keywords attached to the record.
fn keywords() -> Vec<String> {
    [
        "SMILES",
        "cheminformatics",
        "molecules",
        "chemistry",
        "drug discovery",
        "PubChem",
        "ZINC20",
        "Enamine REAL",
        "machine learning",
        "deep learning",
    ]
    .iter()
    .map(|&s| s.to_string())
    .collect()
}

/// Plain-text additional notes, carrying the source attributions and the
/// required Enamine permission and trademark statement.
const NOTES: &str = "Source databases: PubChem (U.S. National Library of Medicine, public domain), \
ZINC20 (Irwin and Shoichet group, UCSF), and the Enamine REAL(TM) Database (Enamine Ltd). \
The REAL-derived structures are included with the express written permission of Enamine. \
REAL(TM) is a registered trademark of Enamine.";

/// Builds a `RelatedIdentifier` pointing at a supporting software repository.
fn software_identifier(url: &str) -> Result<RelatedIdentifier> {
    RelatedIdentifier::builder()
        .identifier(url)
        .relation("isSupplementedBy")
        .scheme("url")
        .resource_type("software")
        .build()
        .map_err(publish_err)
}

/// The default, corpus-specific HTML description (`&trade;` renders as the
/// trademark symbol on Zenodo while keeping the source ASCII).
fn default_description_html() -> String {
    format!(
        "<p>This dataset is a single corpus of 15,260,616,134 unique, canonical SMILES strings, \
assembled by merging and deduplicating three large public molecular collections: PubChem, ZINC20, \
and the Enamine REAL&trade; Database. Every record is a single SMILES string, with no catalogue \
identifiers, prices, computed properties, building blocks, synthons, or source labels.</p>\
<p><strong>Format.</strong> The corpus is a single Zstandard file holding one canonical SMILES per \
line in plain ASCII, byte-sorted and deduplicated, split into 5 GB parts named \
<code>corpus.canonical.smi.zst.part-*</code> for upload. Reassemble and decompress with \
<code>cat corpus.canonical.smi.zst.part-* &gt; corpus.canonical.smi.zst</code> then \
<code>zstd -d --long=31 corpus.canonical.smi.zst</code> (the 2 GB long-distance window requires the \
matching flag). Stereochemistry uses the OpenSMILES extended tetrahedral notation and aromatic rings \
are written in Kekule form. The sha256 of the reassembled compressed file is \
<code>{REFERENCE_SHA256}</code>.</p>\
<p><strong>Construction.</strong> Each source was read in full: PubChem (about 120 million), the \
ZINC20 two-dimensional catalogue (about 1.93 billion), and the entire Enamine REAL&trade; Database \
across all heavy-atom-count partitions (about 13.6 billion). The only structural filter was a \
single-connected-component requirement. Records were deduplicated first by the InChIKey shipped \
with each source, then re-canonicalized with a pure-Rust canonicalizer and deduplicated again on \
the canonical string. The funnel was 15,685,365,054 ingested, 15,399,489,475 unique by shipped \
InChIKey, and 15,260,616,134 unique canonical SMILES.</p>\
<p><strong>Reproducibility.</strong> Produced by the open-source lots-of-smiles pipeline \
(<a href=\"{PIPELINE_REPO}\">{PIPELINE_REPO}</a>), using the smiles-parser crate \
(<a href=\"{PARSER_REPO}\">{PARSER_REPO}</a>).</p>\
<p><strong>License and copyright.</strong> This compilation is made available by Luca Cappelletti \
under the Creative Commons Attribution 4.0 International license (CC BY 4.0), which covers any \
copyright and sui generis database rights in the compilation to the extent they apply. Reuse, \
including commercial use, is permitted as long as the original sources (PubChem, ZINC20, and the \
Enamine REAL&trade; Database) are attributed. The REAL-derived structures are published with the \
express written permission of Enamine: the Enamine REAL&trade; Database must be credited as their \
source, and the notice that REAL&trade; is a registered trademark of Enamine must be preserved.</p>"
    )
}

/// Reads the reserved DOI from a draft deposition, falling back to the
/// pre-reserved DOI carried in the raw metadata.
fn reserved_doi(deposition: &Deposition) -> Option<String> {
    if let Some(doi) = &deposition.doi {
        return Some(doi.to_string());
    }
    deposition
        .metadata
        .get("prereserve_doi")
        .and_then(|p| p.get("doi"))
        .and_then(|d| d.as_str())
        .map(str::to_string)
}

/// Progress bar for the file upload, sized to the file length in bytes.
fn upload_bar(len: u64) -> ProgressBar {
    let bar = ProgressBar::new(len);
    if let Ok(style) = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
    ) {
        bar.set_style(style.progress_chars("=>-"));
    }
    // Refresh the rate and ETA even when a chunk stalls on the network.
    bar.enable_steady_tick(std::time::Duration::from_millis(500));
    bar
}

/// Maps any error with a `Display` impl into [`LosError::Publish`].
fn publish_err(error: impl std::fmt::Display) -> LosError {
    LosError::Publish(error.to_string())
}
