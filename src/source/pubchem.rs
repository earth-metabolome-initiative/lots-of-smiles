//! PubChem source adapter.
//!
//! PubChem distributes two CID-sorted files: `CID-SMILES.gz` (`CID  SMILES`)
//! and `CID-InChI-Key.gz` (`CID  InChI  InChIKey`). Neither alone has both the
//! SMILES and the InChIKey we need, so this adapter performs a linear,
//! zero-memory merge-join by numeric CID: both files are sorted ascending, so a
//! two-pointer scan emits `(inchikey, smiles)` for every CID present in both.
//! CIDs present in only one file are skipped.
//!
//! `CID-InChI-Key.gz` is downloaded from NCBI on first use if absent;
//! `CID-SMILES.gz` is expected to already be on disk.

use std::path::{Path, PathBuf};

use crate::io::{InchiKey, LineReader, open_lines, read_trimmed_line};
use crate::source::{RawRecord, SmilesSource, SourceId, SourceStats};
use crate::{LosError, Result};

/// Default URL for the PubChem CID-to-InChIKey mapping on the NCBI FTP server.
pub const DEFAULT_CID_INCHIKEY_URL: &str =
    "https://ftp.ncbi.nlm.nih.gov/pubchem/Compound/Extras/CID-InChI-Key.gz";

/// Validated PubChem source configuration. Build via [`PubChemConfig::builder`].
#[derive(Debug, Clone)]
pub struct PubChemConfig {
    cid_smiles_gz: PathBuf,
    cid_inchikey_gz: PathBuf,
    cid_inchikey_url: String,
}

impl PubChemConfig {
    /// Starts a new [`PubChemConfigBuilder`].
    pub fn builder() -> PubChemConfigBuilder {
        PubChemConfigBuilder::default()
    }

    /// Path to the local `CID-SMILES.gz`.
    pub fn cid_smiles_gz(&self) -> &Path {
        &self.cid_smiles_gz
    }
    /// Path where `CID-InChI-Key.gz` is stored (downloaded if absent).
    pub fn cid_inchikey_gz(&self) -> &Path {
        &self.cid_inchikey_gz
    }
    /// URL used to download `CID-InChI-Key.gz`.
    pub fn cid_inchikey_url(&self) -> &str {
        &self.cid_inchikey_url
    }
}

/// Builder for [`PubChemConfig`].
#[derive(Debug, Clone, Default)]
pub struct PubChemConfigBuilder {
    cid_smiles_gz: Option<PathBuf>,
    cid_inchikey_gz: Option<PathBuf>,
    cid_inchikey_url: Option<String>,
}

impl PubChemConfigBuilder {
    /// Sets the path to the local `CID-SMILES.gz` (required, must exist).
    pub fn cid_smiles_gz(mut self, path: impl Into<PathBuf>) -> Self {
        self.cid_smiles_gz = Some(path.into());
        self
    }
    /// Sets the path where `CID-InChI-Key.gz` is stored (required; downloaded
    /// if absent).
    pub fn cid_inchikey_gz(mut self, path: impl Into<PathBuf>) -> Self {
        self.cid_inchikey_gz = Some(path.into());
        self
    }
    /// Overrides the download URL for `CID-InChI-Key.gz` (defaults to NCBI).
    pub fn cid_inchikey_url(mut self, url: impl Into<String>) -> Self {
        self.cid_inchikey_url = Some(url.into());
        self
    }

    /// Validates and finalizes the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingField`] if a required path is unset, or
    /// [`LosError::MissingPath`] if `CID-SMILES.gz` does not exist.
    pub fn build(self) -> Result<PubChemConfig> {
        let cid_smiles_gz = self
            .cid_smiles_gz
            .ok_or(LosError::MissingField("pubchem.cid_smiles_gz"))?;
        if !cid_smiles_gz.is_file() {
            return Err(LosError::MissingPath(cid_smiles_gz));
        }
        let cid_inchikey_gz = self
            .cid_inchikey_gz
            .ok_or(LosError::MissingField("pubchem.cid_inchikey_gz"))?;
        Ok(PubChemConfig {
            cid_smiles_gz,
            cid_inchikey_gz,
            cid_inchikey_url: self
                .cid_inchikey_url
                .unwrap_or_else(|| DEFAULT_CID_INCHIKEY_URL.to_string()),
        })
    }
}

/// One side of the merge: a reader plus its current `(cid, payload)` row.
struct Cursor {
    reader: LineReader,
    line: Vec<u8>,
    /// Current row's numeric CID, or `None` once exhausted.
    cid: Option<u64>,
}

/// Streaming merge-join over `CID-SMILES.gz` and `CID-InChI-Key.gz`.
pub struct PubChemSource {
    smiles: Cursor,
    inchikey: Cursor,
    // The current row payloads, valid when the matching `cid` is `Some`.
    current_smiles: Vec<u8>,
    current_inchikey: Vec<u8>,
    stats: SourceStats,
}

impl PubChemSource {
    /// Opens the source, downloading `CID-InChI-Key.gz` if necessary, then
    /// priming both cursors at their first rows.
    pub fn open(config: &PubChemConfig) -> Result<Self> {
        crate::download::ensure_file(config.cid_inchikey_url(), config.cid_inchikey_gz())?;
        log::info!(
            "pubchem: joining {} with {}",
            config.cid_smiles_gz().display(),
            config.cid_inchikey_gz().display()
        );

        let mut smiles = Cursor {
            reader: open_lines(config.cid_smiles_gz())?,
            line: Vec::with_capacity(256),
            cid: None,
        };
        let mut inchikey = Cursor {
            reader: open_lines(config.cid_inchikey_gz())?,
            line: Vec::with_capacity(256),
            cid: None,
        };
        let mut stats = SourceStats::default();
        advance(&mut smiles, &mut stats)?;
        advance(&mut inchikey, &mut stats)?;
        Ok(Self {
            current_smiles: smiles.line.clone(),
            current_inchikey: inchikey.line.clone(),
            smiles,
            inchikey,
            stats,
        })
    }
}

/// Advances a cursor to its next well-formed row, updating `cid`. Rows whose
/// CID does not parse are skipped and counted as malformed. Sets `cid` to
/// `None` at end of file.
fn advance(cursor: &mut Cursor, stats: &mut SourceStats) -> Result<()> {
    loop {
        let n = read_trimmed_line(&mut cursor.reader, &mut cursor.line)
            .map_err(|e| LosError::io("reading pubchem file", e))?;
        if n == 0 {
            cursor.cid = None;
            return Ok(());
        }
        if cursor.line.is_empty() {
            continue;
        }
        // CID is the first tab-separated field.
        let first = cursor.line.split(|&b| b == b'\t').next().unwrap_or(b"");
        if let Some(cid) = std::str::from_utf8(first)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            cursor.cid = Some(cid);
            return Ok(());
        }
        stats.malformed += 1;
    }
}

/// Extracts a field by index from a tab-separated line.
fn field(line: &[u8], index: usize) -> Option<&[u8]> {
    line.split(|&b| b == b'\t').nth(index)
}

impl SmilesSource for PubChemSource {
    fn id(&self) -> SourceId {
        SourceId::PubChem
    }

    fn next_raw(&mut self) -> Result<Option<RawRecord>> {
        loop {
            let (Some(cid_s), Some(cid_i)) = (self.smiles.cid, self.inchikey.cid) else {
                // One side exhausted: no further matches possible.
                return Ok(None);
            };
            match cid_s.cmp(&cid_i) {
                std::cmp::Ordering::Less => {
                    advance(&mut self.smiles, &mut self.stats)?;
                    self.current_smiles.clone_from(&self.smiles.line);
                }
                std::cmp::Ordering::Greater => {
                    advance(&mut self.inchikey, &mut self.stats)?;
                    self.current_inchikey.clone_from(&self.inchikey.line);
                }
                std::cmp::Ordering::Equal => {
                    self.stats.rows_read += 1;
                    // SMILES is field 1 of CID-SMILES; InChIKey is field 2 of
                    // CID-InChI-Key.
                    let smiles = field(&self.current_smiles, 1).map(<[u8]>::to_vec);
                    let inchikey_bytes = field(&self.current_inchikey, 2);
                    let key = inchikey_bytes.and_then(InchiKey::from_bytes);

                    // Advance both sides past this CID before returning.
                    advance(&mut self.smiles, &mut self.stats)?;
                    self.current_smiles.clone_from(&self.smiles.line);
                    advance(&mut self.inchikey, &mut self.stats)?;
                    self.current_inchikey.clone_from(&self.inchikey.line);

                    match (smiles, key) {
                        (Some(smiles), Some(key)) if !smiles.is_empty() => {
                            return Ok(Some(RawRecord {
                                inchikey: key,
                                smiles: smiles.into_boxed_slice(),
                                shipped_mass: None,
                            }));
                        }
                        (_, None) => {
                            self.stats.missing_inchikey += 1;
                        }
                        _ => {
                            self.stats.malformed += 1;
                        }
                    }
                }
            }
        }
    }

    fn stats(&self) -> &SourceStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    fn write_gz(path: &Path, body: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = GzEncoder::new(file, Compression::default());
        enc.write_all(body.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    fn config_with(dir: &Path, smiles_body: &str, inchikey_body: &str) -> PubChemConfig {
        let smiles_path = dir.join("CID-SMILES.gz");
        let inchikey_path = dir.join("CID-InChI-Key.gz");
        write_gz(&smiles_path, smiles_body);
        write_gz(&inchikey_path, inchikey_body);
        PubChemConfig::builder()
            .cid_smiles_gz(&smiles_path)
            .cid_inchikey_gz(&inchikey_path)
            // URL unused: the file already exists so no download happens.
            .build()
            .unwrap()
    }

    #[test]
    fn missing_smiles_file_rejected() {
        let e = PubChemConfig::builder()
            .cid_smiles_gz("/no/such/CID-SMILES.gz")
            .cid_inchikey_gz("/tmp/whatever.gz")
            .build();
        assert!(matches!(e, Err(LosError::MissingPath(_))));
    }

    #[test]
    fn merge_join_matches_on_cid() {
        let dir = tempfile::tempdir().unwrap();
        // CID 2 is missing from SMILES; CID 4 missing from InChI-Key. Both
        // must be skipped; CIDs 1 and 3 match.
        let smiles = "1\tCCO\n3\tc1ccccc1\n4\tCN\n";
        let inchikey = "1\tInChI=1S/...\tLFQSCWFLJHTTHZ-UHFFFAOYSA-N\n\
                        2\tInChI=1S/...\tZZZZZZZZZZZZZZ-UHFFFAOYSA-N\n\
                        3\tInChI=1S/...\tUHOVQNZJYSORNB-UHFFFAOYSA-N\n";
        let cfg = config_with(dir.path(), smiles, inchikey);
        let mut src = PubChemSource::open(&cfg).unwrap();

        let r1 = src.next_raw().unwrap().unwrap();
        assert_eq!(&*r1.smiles, b"CCO");
        assert_eq!(r1.inchikey.as_str(), "LFQSCWFLJHTTHZ-UHFFFAOYSA-N");

        let r2 = src.next_raw().unwrap().unwrap();
        assert_eq!(&*r2.smiles, b"c1ccccc1");
        assert_eq!(r2.inchikey.as_str(), "UHOVQNZJYSORNB-UHFFFAOYSA-N");

        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 2);
    }

    #[test]
    fn counts_missing_inchikey() {
        let dir = tempfile::tempdir().unwrap();
        // CID 1 matches but its InChIKey field is malformed (too short).
        let smiles = "1\tCCO\n";
        let inchikey = "1\tInChI=1S/...\tTOOSHORT\n";
        let cfg = config_with(dir.path(), smiles, inchikey);
        let mut src = PubChemSource::open(&cfg).unwrap();
        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 1);
        assert_eq!(src.stats().missing_inchikey, 1);
    }

    #[test]
    fn handles_empty_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with(dir.path(), "", "");
        let mut src = PubChemSource::open(&cfg).unwrap();
        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 0);
    }
}
