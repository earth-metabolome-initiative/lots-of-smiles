//! The configurable corpus output writer.
//!
//! Projects each surviving record into the configured [`Columns`], applies the
//! configured [`Compression`], and routes records to the correct shard under
//! [`Sharding`].

use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::config::output::{Columns, Compression, OutputFormat, Sharding};
use crate::io::InchiKey;
use crate::{LosError, Result};

/// A single shard's sink, owning the compression encoder so it can be finalized.
enum Sink {
    Plain(BufWriter<File>),
    Gzip(flate2::write::GzEncoder<BufWriter<File>>),
    Zstd(zstd::stream::write::Encoder<'static, BufWriter<File>>),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(buf),
            Sink::Gzip(w) => w.write(buf),
            Sink::Zstd(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Gzip(w) => w.flush(),
            Sink::Zstd(w) => w.flush(),
        }
    }
}

impl Sink {
    fn create(path: &Path, compression: Compression) -> Result<Self> {
        let file = File::create(path)
            .map_err(|e| LosError::io(format!("create {}", path.display()), e))?;
        let buf = BufWriter::with_capacity(1 << 20, file);
        Ok(match compression {
            Compression::None => Sink::Plain(buf),
            Compression::Gzip => Sink::Gzip(flate2::write::GzEncoder::new(
                buf,
                flate2::Compression::default(),
            )),
            Compression::Zstd { level } => {
                let enc = zstd::stream::write::Encoder::new(buf, level)
                    .map_err(|e| LosError::io(format!("zstd encoder for {}", path.display()), e))?;
                Sink::Zstd(enc)
            }
        })
    }

    /// Flushes and finalizes any compression frame.
    fn finish(self) -> Result<()> {
        match self {
            Sink::Plain(mut w) => {
                w.flush()?;
            }
            Sink::Gzip(w) => {
                w.finish().map_err(|e| LosError::io("finish gzip", e))?;
            }
            Sink::Zstd(w) => {
                w.finish().map_err(|e| LosError::io("finish zstd", e))?;
            }
        }
        Ok(())
    }
}

/// Writes surviving records to one or more shard files in the configured format.
pub struct OutputWriter {
    sinks: Vec<Sink>,
    columns: Columns,
    shard_count: u32,
    line: Vec<u8>,
}

impl OutputWriter {
    /// Creates the writer and opens all shard files.
    pub fn create(format: &OutputFormat) -> Result<Self> {
        let (paths, shard_count) = match format.sharding() {
            Sharding::Single => (vec![format.path().to_path_buf()], 1u32),
            Sharding::Shards(n) => {
                let n = n.get();
                let paths = (0..n).map(|i| shard_path(format.path(), i, n)).collect();
                (paths, n)
            }
        };
        let mut sinks = Vec::with_capacity(paths.len());
        for path in &paths {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| LosError::io(format!("mkdir {}", parent.display()), e))?;
            }
            sinks.push(Sink::create(path, format.compression())?);
        }
        Ok(Self {
            sinks,
            columns: format.columns(),
            shard_count,
            line: Vec::with_capacity(256),
        })
    }

    /// Writes one record. `source_tag` is only used for
    /// [`Columns::SmilesInchikeySource`].
    pub fn write_record(
        &mut self,
        inchikey: &InchiKey,
        smiles: &[u8],
        source_tag: &str,
    ) -> Result<()> {
        self.line.clear();
        match self.columns {
            Columns::SmilesOnly => {
                self.line.extend_from_slice(smiles);
            }
            Columns::SmilesInchikey => {
                self.line.extend_from_slice(smiles);
                self.line.push(b'\t');
                self.line.extend_from_slice(inchikey.as_bytes());
            }
            Columns::SmilesInchikeySource => {
                self.line.extend_from_slice(smiles);
                self.line.push(b'\t');
                self.line.extend_from_slice(inchikey.as_bytes());
                self.line.push(b'\t');
                self.line.extend_from_slice(source_tag.as_bytes());
            }
        }
        self.line.push(b'\n');
        let shard = self.shard_for(inchikey);
        self.sinks[shard]
            .write_all(&self.line)
            .map_err(|e| LosError::io("write record", e))?;
        Ok(())
    }

    /// Flushes and finalizes all shards.
    pub fn finish(self) -> Result<()> {
        for sink in self.sinks {
            sink.finish()?;
        }
        Ok(())
    }

    fn shard_for(&self, inchikey: &InchiKey) -> usize {
        if self.shard_count == 1 {
            return 0;
        }
        let mut hasher = ahash::AHasher::default();
        inchikey.as_bytes().hash(&mut hasher);
        (hasher.finish() % u64::from(self.shard_count)) as usize
    }
}

/// Builds a shard file path by inserting a zero-padded index before the
/// extension: `corpus.smi.zst` with 8 shards -> `corpus.000.smi.zst`.
fn shard_path(base: &Path, index: u32, count: u32) -> PathBuf {
    let width = (count - 1).to_string().len();
    let idx = format!("{index:0width$}");
    let file_name = base
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("corpus");
    // Split on the first '.' so multi-suffix names like `corpus.smi.zst` keep
    // both suffixes after the inserted index: `corpus.000.smi.zst`.
    let new_name = match file_name.split_once('.') {
        Some((stem, rest)) => format!("{stem}.{idx}.{rest}"),
        None => format!("{file_name}.{idx}"),
    };
    match base.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(new_name),
        _ => PathBuf::from(new_name),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::config::output::OutputFormat;

    #[test]
    fn shard_path_inserts_index() {
        let p = shard_path(Path::new("/data/corpus.smi.zst"), 3, 8);
        assert_eq!(p, PathBuf::from("/data/corpus.3.smi.zst"));
        let p2 = shard_path(Path::new("/data/corpus.smi.zst"), 3, 16);
        assert_eq!(p2, PathBuf::from("/data/corpus.03.smi.zst"));
        let p3 = shard_path(Path::new("corpus"), 1, 4);
        assert_eq!(p3, PathBuf::from("corpus.1"));
    }

    #[test]
    fn writes_smiles_only() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.smi");
        let fmt = OutputFormat::builder().path(&out).build().unwrap();
        let mut w = OutputWriter::create(&fmt).unwrap();
        let key = InchiKey::from_bytes(b"RDHQFKQIGNGIED-UHFFFAOYSA-N").unwrap();
        w.write_record(&key, b"CCO", "zinc20").unwrap();
        w.finish().unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        assert_eq!(content, "CCO\n");
    }

    #[test]
    fn writes_all_columns() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.tsv");
        let fmt = OutputFormat::builder()
            .path(&out)
            .columns(Columns::SmilesInchikeySource)
            .build()
            .unwrap();
        let mut w = OutputWriter::create(&fmt).unwrap();
        let key = InchiKey::from_bytes(b"RDHQFKQIGNGIED-UHFFFAOYSA-N").unwrap();
        w.write_record(&key, b"CCO", "pubchem").unwrap();
        w.finish().unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        assert_eq!(content, "CCO\tRDHQFKQIGNGIED-UHFFFAOYSA-N\tpubchem\n");
    }

    #[test]
    fn sharding_distributes_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.smi");
        let fmt = OutputFormat::builder()
            .path(&out)
            .sharding(Sharding::Shards(NonZeroU32::new(4).unwrap()))
            .build()
            .unwrap();
        let mut w = OutputWriter::create(&fmt).unwrap();
        for i in 0..100u32 {
            let raw = format!("{i:0>14}-AAAAAAAAAA-N");
            let key = InchiKey::from_bytes(raw.as_bytes()).unwrap();
            w.write_record(&key, b"CCO", "zinc20").unwrap();
        }
        w.finish().unwrap();
        // All four shard files exist and together hold 100 lines.
        let mut total = 0;
        for i in 0..4 {
            let p = shard_path(&out, i, 4);
            let lines = std::fs::read_to_string(&p).unwrap().lines().count();
            total += lines;
        }
        assert_eq!(total, 100);
    }

    #[test]
    fn zstd_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.smi.zst");
        let fmt = OutputFormat::builder()
            .path(&out)
            .compression(Compression::Zstd { level: 3 })
            .build()
            .unwrap();
        let mut w = OutputWriter::create(&fmt).unwrap();
        let key = InchiKey::from_bytes(b"RDHQFKQIGNGIED-UHFFFAOYSA-N").unwrap();
        w.write_record(&key, b"CCO", "zinc20").unwrap();
        w.finish().unwrap();
        let bytes = std::fs::read(&out).unwrap();
        let decoded = zstd::stream::decode_all(&bytes[..]).unwrap();
        assert_eq!(decoded, b"CCO\n");
    }
}
