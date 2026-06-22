//! Enamine REAL source adapter.
//!
//! Enamine REAL ships as nine bzip2-compressed CXSMILES files, bucketed by
//! heavy-atom count (HAC). Each file is a tab-separated table whose first
//! column is a CXSMILES string and whose **last** column is the InChIKey
//! (header literally spelled `InChiKey`). This adapter:
//!
//! - selects which HAC files are relevant given the configured atom-count
//!   bounds (skipping whole files that fall entirely outside them),
//! - ensures each selected file is present in the download directory (fetching
//!   it via the authenticated [`EnamineClient`](crate::download::enamine::EnamineClient)
//!   if absent and credentials are configured),
//! - streams each file through a bzip2 decoder, extracting the core SMILES
//!   (the token before any CXSMILES extension) and the InChIKey.
//!
//! The live login + download path requires valid Enamine.net credentials and
//! has not been exercised in CI; the parsing and HAC-selection logic are
//! covered by tests against synthetic bzip2 fixtures.

use std::path::{Path, PathBuf};

use crate::config::credentials::EnamineCredentials;
use crate::config::filters::{AtomCount, Filters};
use crate::io::{InchiKey, read_trimmed_line};
use crate::source::{RawRecord, SmilesSource, SourceId, SourceStats};
use crate::{LosError, Result};

/// Base URL of the Enamine site (used for login and the download component).
pub const DEFAULT_BASE_URL: &str = "https://enamine.net";

/// One Enamine REAL distribution file: the local name to save it under, the
/// Joomla download-component file id, and the heavy-atom-count range it covers.
///
/// Downloads use the site's Joomla download component
/// (`/component/download/?task=file.download&f=<file_id>`), which requires an
/// authenticated session. The `file_id` values were read from the REAL
/// database download page and may change if Enamine re-publishes the files.
#[derive(Debug, Clone, Copy)]
pub struct HacFile {
    /// Local file name to save the download as.
    pub local_name: &'static str,
    /// Joomla download-component file id.
    pub file_id: u32,
    /// Inclusive lower bound on heavy-atom count in this file.
    pub hac_min: u32,
    /// Inclusive upper bound on heavy-atom count in this file.
    pub hac_max: u32,
}

/// The nine Enamine REAL CXSMILES files, their download-component file ids, and
/// HAC ranges (per the REAL database download page).
pub const HAC_FILES: &[HacFile] = &[
    HacFile {
        local_name: "Enamine_REAL_HAC_11_21.cxsmiles.bz2",
        file_id: 1100,
        hac_min: 11,
        hac_max: 21,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_22_23.cxsmiles.bz2",
        file_id: 1101,
        hac_min: 22,
        hac_max: 23,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_24.cxsmiles.bz2",
        file_id: 1102,
        hac_min: 24,
        hac_max: 24,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_25.cxsmiles.bz2",
        file_id: 1103,
        hac_min: 25,
        hac_max: 25,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_26.cxsmiles.bz2",
        file_id: 1104,
        hac_min: 26,
        hac_max: 26,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_27.cxsmiles.bz2",
        file_id: 1106,
        hac_min: 27,
        hac_max: 27,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_28.cxsmiles.bz2",
        file_id: 1108,
        hac_min: 28,
        hac_max: 28,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_29_38_Part_1.cxsmiles.bz2",
        file_id: 1110,
        hac_min: 29,
        hac_max: 38,
    },
    HacFile {
        local_name: "Enamine_REAL_HAC_29_38_Part_2.cxsmiles.bz2",
        file_id: 1163,
        hac_min: 29,
        hac_max: 38,
    },
];

/// Returns whether a HAC file can be skipped entirely given the filters.
///
/// A file is skippable only when the atom-count bounds use heavy-atom counting
/// (the HAC metric) and the file's whole HAC range lies outside `[min, max]`.
pub fn file_is_skippable(file: &HacFile, filters: &Filters) -> bool {
    if filters.atom_count_mode_or_default() != AtomCount::Heavy {
        return false;
    }
    if let Some(max) = filters.max_atoms
        && file.hac_min > max
    {
        return true;
    }
    if let Some(min) = filters.min_atoms
        && file.hac_max < min
    {
        return true;
    }
    false
}

/// Validated Enamine REAL source configuration. Build via [`EnamineConfig::builder`].
#[derive(Debug, Clone)]
pub struct EnamineConfig {
    download_dir: PathBuf,
    credentials: Option<EnamineCredentials>,
    base_url: String,
    delete_after_extract: bool,
}

impl EnamineConfig {
    /// Starts a new [`EnamineConfigBuilder`].
    pub fn builder() -> EnamineConfigBuilder {
        EnamineConfigBuilder::default()
    }

    /// Directory where the `.cxsmiles.bz2` files are stored.
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }
    /// Credentials used to authenticate downloads, if configured.
    pub fn credentials(&self) -> Option<&EnamineCredentials> {
        self.credentials.as_ref()
    }
    /// Base URL for file downloads.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
    /// Whether each `.bz2` is deleted after it has been fully streamed.
    pub fn delete_after_extract(&self) -> bool {
        self.delete_after_extract
    }
}

/// Builder for [`EnamineConfig`].
#[derive(Debug, Clone, Default)]
pub struct EnamineConfigBuilder {
    download_dir: Option<PathBuf>,
    credentials: Option<EnamineCredentials>,
    base_url: Option<String>,
    delete_after_extract: Option<bool>,
}

impl EnamineConfigBuilder {
    /// Sets the directory where `.cxsmiles.bz2` files are stored / downloaded.
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = Some(path.into());
        self
    }
    /// Sets the credentials used to authenticate downloads. Omit when the files
    /// have already been downloaded into the download directory.
    pub fn credentials(mut self, credentials: EnamineCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }
    /// Overrides the download base URL (defaults to [`DEFAULT_BASE_URL`]).
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
    /// Sets whether each `.bz2` is deleted after streaming (default `false`).
    pub fn delete_after_extract(mut self, yes: bool) -> Self {
        self.delete_after_extract = Some(yes);
        self
    }

    /// Validates and finalizes the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingField`] if the download directory is unset.
    pub fn build(self) -> Result<EnamineConfig> {
        let download_dir = self
            .download_dir
            .ok_or(LosError::MissingField("enamine.download_dir"))?;
        Ok(EnamineConfig {
            download_dir,
            credentials: self.credentials,
            base_url: self
                .base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            delete_after_extract: self.delete_after_extract.unwrap_or(false),
        })
    }
}

/// Parses one Enamine REAL CXSMILES row into `(core_smiles, inchikey_bytes)`.
///
/// The first tab-separated column is a CXSMILES string; the core SMILES is the
/// token before any space-separated CXSMILES extension. The InChIKey is the
/// last tab-separated column. For stereo-ambiguous entries (~10% of REAL),
/// that column holds several comma-separated InChIKeys, one per stereoisomer;
/// since the core SMILES we keep is the flat structure shared by all of them,
/// we use the first InChIKey as the deduplication key.
fn parse_enamine_row(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut fields = line.split(|&b| b == b'\t');
    let smiles_col = fields.next()?;
    // The InChIKey is the final column; take it from the back without scanning.
    let inchikey_col = fields.next_back()?;
    // A multi-stereoisomer row lists comma-separated keys; keep the first.
    let inchikey = inchikey_col
        .split(|&b| b == b',')
        .next()
        .unwrap_or(inchikey_col);
    // Strip any CXSMILES extension: take the token before the first space.
    let smiles = smiles_col
        .split(|&b| b == b' ')
        .next()
        .unwrap_or(smiles_col);
    Some((smiles, inchikey))
}

/// A streaming reader over the selected Enamine REAL files.
pub struct EnamineSource {
    files: std::vec::IntoIter<PathBuf>,
    current_path: Option<PathBuf>,
    current: Option<crate::io::LineReader>,
    at_file_start: bool,
    delete_after_extract: bool,
    line: Vec<u8>,
    stats: SourceStats,
    selected: u64,
}

/// Selects the relevant HAC files given `filters` and ensures each is present
/// in the download directory, downloading any that are missing. Returns the
/// local paths of the selected files (in order).
///
/// This is the download step shared by [`EnamineSource::open`] and the
/// CLI's download-only mode.
pub fn ensure_downloaded(config: &EnamineConfig, filters: &Filters) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(config.download_dir())
        .map_err(|e| LosError::io(format!("mkdir {}", config.download_dir().display()), e))?;

    let selected: Vec<&HacFile> = HAC_FILES
        .iter()
        .filter(|f| !file_is_skippable(f, filters))
        .collect();
    log::info!(
        "enamine: {} of {} HAC files selected after atom-count bounds",
        selected.len(),
        HAC_FILES.len()
    );

    let mut local_files = Vec::with_capacity(selected.len());
    let client = EnamineClientHandle::new(config);
    for file in &selected {
        let dest = config.download_dir().join(file.local_name);
        if !dest.exists() || std::fs::metadata(&dest).map_or(true, |m| m.len() == 0) {
            client.ensure(file.file_id, file.local_name, &dest)?;
        } else {
            log::info!("enamine: {} already present", file.local_name);
        }
        local_files.push(dest);
    }
    Ok(local_files)
}

impl EnamineSource {
    /// Opens the source: selects relevant HAC files given `filters`, ensures
    /// each is present (downloading if necessary), and prepares to stream them.
    pub fn open(config: &EnamineConfig, filters: &Filters) -> Result<Self> {
        let local_files = ensure_downloaded(config, filters)?;
        Ok(Self {
            selected: local_files.len() as u64,
            files: local_files.into_iter(),
            current_path: None,
            current: None,
            at_file_start: false,
            delete_after_extract: config.delete_after_extract(),
            line: Vec::with_capacity(512),
            stats: SourceStats::default(),
        })
    }

    /// Number of HAC files selected for this run.
    pub fn selected_files(&self) -> u64 {
        self.selected
    }

    fn open_next_file(&mut self) -> Result<bool> {
        // Optionally delete the file we just finished.
        if self.delete_after_extract
            && let Some(prev) = self.current_path.take()
        {
            let _ = std::fs::remove_file(&prev);
            log::info!("enamine: removed {} after extraction", prev.display());
        }
        if let Some(path) = self.files.next() {
            self.current = Some(crate::io::open_lines(&path)?);
            self.current_path = Some(path);
            self.at_file_start = true;
            Ok(true)
        } else {
            self.current = None;
            Ok(false)
        }
    }
}

impl SmilesSource for EnamineSource {
    fn id(&self) -> SourceId {
        SourceId::Enamine
    }

    fn next_raw(&mut self) -> Result<Option<RawRecord>> {
        loop {
            if self.current.is_none() && !self.open_next_file()? {
                return Ok(None);
            }
            let reader = self.current.as_mut().expect("current set");
            let n = read_trimmed_line(reader, &mut self.line)
                .map_err(|e| LosError::io("reading enamine file", e))?;
            if n == 0 {
                self.current = None;
                continue;
            }
            if self.at_file_start {
                self.at_file_start = false;
                if self.line.starts_with(b"smiles\t") {
                    continue;
                }
            }
            if self.line.is_empty() {
                continue;
            }
            self.stats.rows_read += 1;

            let Some((smiles, inchikey)) = parse_enamine_row(&self.line) else {
                self.stats.malformed += 1;
                continue;
            };
            if smiles.is_empty() {
                self.stats.malformed += 1;
                continue;
            }
            let Some(key) = InchiKey::from_bytes(inchikey) else {
                self.stats.missing_inchikey += 1;
                continue;
            };
            return Ok(Some(RawRecord {
                inchikey: key,
                smiles: smiles.to_vec().into_boxed_slice(),
                shipped_mass: None,
            }));
        }
    }

    fn stats(&self) -> &SourceStats {
        &self.stats
    }
}

/// Lazily-constructed download client. Building the authenticated client (and
/// logging in) is deferred until a download is actually needed, so a run over
/// already-present files needs no credentials or network.
struct EnamineClientHandle<'a> {
    config: &'a EnamineConfig,
    client: std::cell::OnceCell<crate::download::enamine::EnamineClient>,
}

impl<'a> EnamineClientHandle<'a> {
    fn new(config: &'a EnamineConfig) -> Self {
        Self {
            config,
            client: std::cell::OnceCell::new(),
        }
    }

    fn ensure(&self, file_id: u32, name: &str, dest: &Path) -> Result<()> {
        if self.client.get().is_none() {
            let creds = self.config.credentials().ok_or_else(|| {
                LosError::Credentials(format!(
                    "{name} is not present in {} and no Enamine credentials were configured to download it",
                    dest.parent().map(Path::display).map(|d| d.to_string()).unwrap_or_default()
                ))
            })?;
            let client =
                crate::download::enamine::EnamineClient::login(creds, self.config.base_url())?;
            let _ = self.client.set(client);
        }
        self.client
            .get()
            .expect("client set")
            .download(file_id, name, dest)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use bzip2::Compression;
    use bzip2::write::BzEncoder;

    use super::*;
    use crate::config::filters::AtomCount;

    #[test]
    fn parses_core_smiles_and_last_column_inchikey() {
        // A row with a CXSMILES extension and ~5 columns; InChIKey is last.
        let line = b"CC(C)O |xyz| \tEN300-12345\t...\t...\tRDHQFKQIGNGIED-UHFFFAOYSA-N";
        let (smiles, key) = parse_enamine_row(line).unwrap();
        assert_eq!(smiles, b"CC(C)O"); // extension stripped
        assert_eq!(key, b"RDHQFKQIGNGIED-UHFFFAOYSA-N");
    }

    #[test]
    fn parses_plain_smiles() {
        let line = b"CCO\tEN300-1\tx\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N";
        let (smiles, key) = parse_enamine_row(line).unwrap();
        assert_eq!(smiles, b"CCO");
        assert_eq!(key, b"LFQSCWFLJHTTHZ-UHFFFAOYSA-N");
    }

    #[test]
    fn parses_multi_stereoisomer_inchikey_taking_first() {
        // ~10% of REAL rows carry comma-separated InChIKeys (one per
        // stereoisomer); we key on the first.
        let line = b"CC(C)O\tEN300-2\tx\tLCYYDAOJCFEFAP-JGVFFNPUSA-N,LCYYDAOJCFEFAP-SFYZADRCSA-N";
        let (smiles, key) = parse_enamine_row(line).unwrap();
        assert_eq!(smiles, b"CC(C)O");
        assert_eq!(key, b"LCYYDAOJCFEFAP-JGVFFNPUSA-N");
        // And it is now a valid 27-byte key.
        assert!(crate::io::InchiKey::from_bytes(key).is_some());
    }

    #[test]
    fn hac_skip_respects_max_atoms() {
        // max_atoms = 20 -> every file with hac_min > 20 is skippable.
        let filters = Filters::builder()
            .max_atoms(20)
            .atom_count_mode(AtomCount::Heavy)
            .build()
            .unwrap();
        let kept: Vec<&str> = HAC_FILES
            .iter()
            .filter(|f| !file_is_skippable(f, &filters))
            .map(|f| f.local_name)
            .collect();
        // Only the 11-21 bucket overlaps [.., 20].
        assert_eq!(kept, vec!["Enamine_REAL_HAC_11_21.cxsmiles.bz2"]);
    }

    #[test]
    fn hac_skip_respects_min_atoms() {
        let filters = Filters::builder()
            .min_atoms(29)
            .atom_count_mode(AtomCount::Heavy)
            .build()
            .unwrap();
        let kept: Vec<&str> = HAC_FILES
            .iter()
            .filter(|f| !file_is_skippable(f, &filters))
            .map(|f| f.local_name)
            .collect();
        // Only the two 29-38 parts have hac_max >= 29.
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|n| n.contains("29_38")));
    }

    #[test]
    fn hac_skip_disabled_for_total_mode() {
        // With Total counting, HAC file-skip must not fire.
        let filters = Filters::builder()
            .max_atoms(20)
            .atom_count_mode(AtomCount::Total)
            .build()
            .unwrap();
        assert!(HAC_FILES.iter().all(|f| !file_is_skippable(f, &filters)));
    }

    fn write_bz2(path: &Path, body: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = BzEncoder::new(file, Compression::default());
        enc.write_all(body.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn streams_a_real_bz2_file() {
        // Place a pre-downloaded bz2 named like the 11-21 bucket so no download
        // or credentials are needed; restrict to that bucket via max_atoms.
        let dir = tempfile::tempdir().unwrap();
        let name = HAC_FILES[0].local_name;
        write_bz2(
            &dir.path().join(name),
            "smiles\tidnumber\tType\tInChiKey\n\
             CCO\tEN1\tnormal\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N\n\
             c1ccccc1 |coords|\tEN2\tnormal\tUHOVQNZJYSORNB-UHFFFAOYSA-N\n\
             BADROW\tEN3\tnormal\tTOOSHORT\n",
        );
        let cfg = EnamineConfig::builder()
            .download_dir(dir.path())
            .build()
            .unwrap();
        let filters = Filters::builder()
            .max_atoms(21)
            .atom_count_mode(AtomCount::Heavy)
            .build()
            .unwrap();
        let mut src = EnamineSource::open(&cfg, &filters).unwrap();
        assert_eq!(src.selected_files(), 1);

        let r1 = src.next_raw().unwrap().unwrap();
        assert_eq!(&*r1.smiles, b"CCO");
        let r2 = src.next_raw().unwrap().unwrap();
        assert_eq!(&*r2.smiles, b"c1ccccc1"); // CXSMILES extension stripped
        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 3);
        assert_eq!(src.stats().missing_inchikey, 1);
    }

    #[test]
    fn missing_file_without_credentials_errors() {
        // 11-21 bucket selected but absent, and no credentials -> error.
        let dir = tempfile::tempdir().unwrap();
        let cfg = EnamineConfig::builder()
            .download_dir(dir.path())
            .build()
            .unwrap();
        let filters = Filters::builder()
            .max_atoms(21)
            .atom_count_mode(AtomCount::Heavy)
            .build()
            .unwrap();
        let err = EnamineSource::open(&cfg, &filters);
        assert!(matches!(err, Err(LosError::Credentials(_))));
    }
}
