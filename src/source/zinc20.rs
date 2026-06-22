//! ZINC20 source adapter.
//!
//! Reads the local ZINC20 2D tranche files laid out as
//! `<root>/<XX>/<YYYY>.txt`, each a tab-separated table with the header
//! `smiles  zinc_id  inchikey  mwt  logp  reactive  purchasable  tranche_name
//! features`. The adapter pulls the `smiles` (column 0) and `inchikey`
//! (column 2) directly and exposes `mwt` (column 3, an average molecular mass)
//! as the record's shipped mass so the filter engine can apply average-mass
//! bounds without parsing.

use std::path::{Path, PathBuf};

use crate::io::{InchiKey, LineReader, open_lines, read_trimmed_line};
use crate::source::{RawRecord, SmilesSource, SourceId, SourceStats};
use crate::{LosError, Result};

/// Column indices in a ZINC20 tranche row.
const COL_SMILES: usize = 0;
const COL_INCHIKEY: usize = 2;
const COL_MWT: usize = 3;

/// Default root directory holding the `<XX>/<YYYY>.txt` tranche tree.
pub const DEFAULT_ROOT: &str = "/mnt/bfd/zinc20/2D";

/// Validated ZINC20 source configuration. Build via [`Zinc20Config::builder`].
#[derive(Debug, Clone)]
pub struct Zinc20Config {
    root: PathBuf,
}

impl Zinc20Config {
    /// Starts a new [`Zinc20ConfigBuilder`].
    pub fn builder() -> Zinc20ConfigBuilder {
        Zinc20ConfigBuilder::default()
    }

    /// The configured tranche-tree root.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Builder for [`Zinc20Config`]. The root defaults to [`DEFAULT_ROOT`].
#[derive(Debug, Clone, Default)]
pub struct Zinc20ConfigBuilder {
    root: Option<PathBuf>,
}

impl Zinc20ConfigBuilder {
    /// Sets the tranche-tree root directory (containing `<XX>/<YYYY>.txt`).
    pub fn root(mut self, path: impl Into<PathBuf>) -> Self {
        self.root = Some(path.into());
        self
    }

    /// Validates and finalizes the configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LosError::MissingPath`] if the root directory does not exist.
    pub fn build(self) -> Result<Zinc20Config> {
        let root = self.root.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
        if !root.is_dir() {
            return Err(LosError::MissingPath(root));
        }
        Ok(Zinc20Config { root })
    }
}

/// A streaming reader over all ZINC20 tranche files under a root.
pub struct Zinc20Source {
    files: std::vec::IntoIter<PathBuf>,
    current: Option<LineReader>,
    at_file_start: bool,
    line: Vec<u8>,
    stats: SourceStats,
    total_files: u64,
}

impl Zinc20Source {
    /// Opens the source, enumerating every `*.txt` tranche file under the
    /// configured root (sorted for deterministic ordering).
    pub fn open(config: &Zinc20Config) -> Result<Self> {
        let mut files = Vec::new();
        collect_txt_files(config.root(), &mut files)?;
        files.sort();
        let total_files = files.len() as u64;
        log::info!(
            "zinc20: {total_files} tranche files under {}",
            config.root().display()
        );
        Ok(Self {
            files: files.into_iter(),
            current: None,
            at_file_start: false,
            line: Vec::with_capacity(256),
            stats: SourceStats::default(),
            total_files,
        })
    }

    /// Number of tranche files discovered.
    pub fn file_count(&self) -> u64 {
        self.total_files
    }

    /// Advances `self.current` to the next file, returning `false` when no files
    /// remain.
    fn open_next_file(&mut self) -> Result<bool> {
        if let Some(path) = self.files.next() {
            self.current = Some(open_lines(&path)?);
            self.at_file_start = true;
            Ok(true)
        } else {
            self.current = None;
            Ok(false)
        }
    }
}

impl SmilesSource for Zinc20Source {
    fn id(&self) -> SourceId {
        SourceId::Zinc20
    }

    fn size_hint_rows(&self) -> Option<u64> {
        None
    }

    fn next_raw(&mut self) -> Result<Option<RawRecord>> {
        loop {
            if self.current.is_none() && !self.open_next_file()? {
                return Ok(None);
            }
            let reader = self.current.as_mut().expect("current set");
            let n = read_trimmed_line(reader, &mut self.line)
                .map_err(|e| LosError::io("reading zinc20 tranche", e))?;
            if n == 0 {
                // End of this file; move to the next on the following iteration.
                self.current = None;
                continue;
            }

            // Skip a leading header row (`smiles\t...`) at the start of a file.
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

            // Split into the columns we need.
            let mut smiles: Option<&[u8]> = None;
            let mut inchikey: Option<&[u8]> = None;
            let mut mwt: Option<&[u8]> = None;
            for (idx, field) in self.line.split(|&b| b == b'\t').enumerate() {
                match idx {
                    COL_SMILES => smiles = Some(field),
                    COL_INCHIKEY => inchikey = Some(field),
                    COL_MWT => mwt = Some(field),
                    _ => {}
                }
            }

            let (Some(smiles), Some(inchikey)) = (smiles, inchikey) else {
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
            let shipped_mass = mwt
                .and_then(|m| std::str::from_utf8(m).ok())
                .and_then(|m| m.trim().parse::<f64>().ok());

            return Ok(Some(RawRecord {
                inchikey: key,
                smiles: smiles.to_vec().into_boxed_slice(),
                shipped_mass,
            }));
        }
    }

    fn stats(&self) -> &SourceStats {
        &self.stats
    }
}

/// Recursively collects `*.txt` files beneath `dir` into `out`.
fn collect_txt_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| LosError::io(format!("read_dir {}", dir.display()), e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| LosError::io(format!("dir entry under {}", dir.display()), e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| LosError::io(format!("file_type {}", path.display()), e))?;
        if file_type.is_dir() {
            collect_txt_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("txt") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tranche(root: &Path, sub: &str, name: &str, body: &str) {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn missing_root_rejected() {
        let e = Zinc20Config::builder().root("/no/such/zinc/dir").build();
        assert!(matches!(e, Err(LosError::MissingPath(_))));
    }

    #[test]
    fn reads_rows_and_shipped_mass() {
        let dir = tempfile::tempdir().unwrap();
        write_tranche(
            dir.path(),
            "AA",
            "AAAA.txt",
            "smiles\tzinc_id\tinchikey\tmwt\tlogp\treactive\tpurchasable\ttranche_name\tfeatures\n\
             CO[C@H]1OC[C@@H](O)[C@H](O)[C@H]1O\t4371221\tZBDGHWFPLXXWRD-MOJAZDJTSA-N\t164.157\t-1.928\t0\t50\tAAAA\t\n\
             C[S@@](=O)CC(N)=O\t34310585\tTTXPTDCYRCXFHQ-ZETCQYMHSA-N\t121.161\t-1.150\t0\t50\tAAAA\t\n",
        );
        let cfg = Zinc20Config::builder().root(dir.path()).build().unwrap();
        let mut src = Zinc20Source::open(&cfg).unwrap();
        assert_eq!(src.file_count(), 1);

        let r1 = src.next_raw().unwrap().unwrap();
        assert_eq!(r1.inchikey.as_str(), "ZBDGHWFPLXXWRD-MOJAZDJTSA-N");
        assert_eq!(&*r1.smiles, b"CO[C@H]1OC[C@@H](O)[C@H](O)[C@H]1O");
        assert_eq!(r1.shipped_mass, Some(164.157));

        let r2 = src.next_raw().unwrap().unwrap();
        assert_eq!(r2.inchikey.as_str(), "TTXPTDCYRCXFHQ-ZETCQYMHSA-N");

        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 2);
        assert_eq!(src.stats().malformed, 0);
        assert_eq!(src.stats().missing_inchikey, 0);
    }

    #[test]
    fn counts_malformed_and_missing_inchikey() {
        let dir = tempfile::tempdir().unwrap();
        write_tranche(
            dir.path(),
            "AB",
            "ABAA.txt",
            "smiles\tzinc_id\tinchikey\tmwt\n\
             CCO\t1\tRDHQFKQIGNGIED-UHFFFAOYSA-N\t46.07\n\
             \t2\tWRONGLENGTHKEY\t10.0\n\
             CCC\t3\tTOOSHORT\t44.1\n",
        );
        let cfg = Zinc20Config::builder().root(dir.path()).build().unwrap();
        let mut src = Zinc20Source::open(&cfg).unwrap();
        let r = src.next_raw().unwrap().unwrap();
        assert_eq!(&*r.smiles, b"CCO");
        // Remaining two rows are bad: empty smiles (malformed) and short key.
        assert!(src.next_raw().unwrap().is_none());
        assert_eq!(src.stats().rows_read, 3);
        assert_eq!(src.stats().malformed, 1); // empty smiles
        assert_eq!(src.stats().missing_inchikey, 1); // short inchikey
    }

    #[test]
    fn walks_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let body = "smiles\tzinc_id\tinchikey\tmwt\nCCO\t1\tRDHQFKQIGNGIED-UHFFFAOYSA-N\t46.07\n";
        write_tranche(dir.path(), "AA", "AAAA.txt", body);
        write_tranche(dir.path(), "AB", "ABAA.txt", body);
        let cfg = Zinc20Config::builder().root(dir.path()).build().unwrap();
        let mut src = Zinc20Source::open(&cfg).unwrap();
        assert_eq!(src.file_count(), 2);
        let mut count = 0;
        while src.next_raw().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
        assert_eq!(src.stats().rows_read, 2);
    }
}
