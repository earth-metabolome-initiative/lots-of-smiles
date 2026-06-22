//! Byte-level I/O helpers shared across source adapters: the fixed-width
//! [`InchiKey`] newtype and constructors for the compressed-stream decoders.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

/// Length in bytes of a standard InChIKey, e.g. `RDHQFKQIGNGIED-UHFFFAOYSA-N`.
///
/// The layout is 14 connectivity chars, a hyphen, 8 chars, a stereo/proto
/// char, a hyphen, and a final version/checksum char: `14 + 1 + 8 + 1 + 1 + 1 = 27`.
pub const INCHIKEY_LEN: usize = 27;

/// A standard InChIKey stored inline as 27 ASCII bytes.
///
/// Using a fixed-width array avoids a heap allocation per record, which matters
/// at billion-row scale. Ordering is byte-wise, matching the `LC_ALL=C` order
/// used by the external sort.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InchiKey([u8; INCHIKEY_LEN]);

impl InchiKey {
    /// Builds an [`InchiKey`] from raw bytes, validating only the length.
    ///
    /// Returns `None` if `bytes` is not exactly [`INCHIKEY_LEN`] long. We
    /// deliberately do not validate the internal hyphen layout here: sources
    /// ship well-formed keys and the length check is enough to reject obvious
    /// garbage cheaply.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != INCHIKEY_LEN {
            return None;
        }
        let mut buf = [0u8; INCHIKEY_LEN];
        buf.copy_from_slice(bytes);
        Some(Self(buf))
    }

    /// Returns the key as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the key as a `&str` (InChIKeys are always ASCII).
    pub fn as_str(&self) -> &str {
        // Safe in practice: keys are ASCII. Fall back to a lossy check only in debug.
        debug_assert!(self.0.is_ascii());
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl core::fmt::Debug for InchiKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "InchiKey({})", self.as_str())
    }
}

impl core::fmt::Display for InchiKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A boxed line-yielding reader over a (possibly compressed) text stream.
pub type LineReader = Box<dyn BufRead + Send>;

/// Compression of an input file, inferred from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCompression {
    /// Uncompressed plain text.
    Plain,
    /// gzip (`.gz`).
    Gzip,
    /// bzip2 (`.bz2`).
    Bzip2,
    /// zstandard (`.zst`).
    Zstd,
}

impl InputCompression {
    /// Infers the compression from a path's extension, defaulting to [`Plain`].
    ///
    /// [`Plain`]: InputCompression::Plain
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("gz") => Self::Gzip,
            Some("bz2") => Self::Bzip2,
            Some("zst") => Self::Zstd,
            _ => Self::Plain,
        }
    }
}

/// Opens `path`, wraps it in the decoder implied by its extension, and returns
/// a buffered line reader.
pub fn open_lines(path: &Path) -> crate::Result<LineReader> {
    let file =
        File::open(path).map_err(|e| crate::LosError::io(format!("open {}", path.display()), e))?;
    let raw: Box<dyn Read + Send> = match InputCompression::from_path(path) {
        InputCompression::Plain => Box::new(file),
        InputCompression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(file)),
        InputCompression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(file)),
        InputCompression::Zstd => {
            Box::new(zstd::stream::read::Decoder::new(file).map_err(|e| {
                crate::LosError::io(format!("zstd decoder for {}", path.display()), e)
            })?)
        }
    };
    // 1 MiB buffer: these are multi-GB sequential scans.
    Ok(Box::new(BufReader::with_capacity(1 << 20, raw)))
}

/// Splits a tab-separated line into at most `n` fields without allocating.
///
/// Returns an iterator over the byte-slice fields. Trailing `\r`/`\n` should be
/// stripped by the caller before calling this.
pub fn tab_fields(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(|&b| b == b'\t')
}

/// Strips a trailing `\n` and optional `\r` from a line buffer in place.
pub fn trim_line_end(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}

/// Reads the next line into `buf` (cleared first), returning the number of
/// bytes read (0 at EOF). The trailing newline is stripped.
pub fn read_trimmed_line<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<usize> {
    buf.clear();
    let n = reader.read_until(b'\n', buf)?;
    if n > 0 {
        trim_line_end(buf);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inchikey_roundtrip() {
        let raw = b"RDHQFKQIGNGIED-UHFFFAOYSA-N";
        assert_eq!(raw.len(), INCHIKEY_LEN);
        let key = InchiKey::from_bytes(raw).expect("valid length");
        assert_eq!(key.as_bytes(), raw);
        assert_eq!(key.as_str(), "RDHQFKQIGNGIED-UHFFFAOYSA-N");
    }

    #[test]
    fn inchikey_rejects_wrong_length() {
        assert!(InchiKey::from_bytes(b"too-short").is_none());
        assert!(InchiKey::from_bytes(b"RDHQFKQIGNGIED-UHFFFAOYSA-NEXTRA").is_none());
    }

    #[test]
    fn inchikey_orders_bytewise() {
        let a = InchiKey::from_bytes(b"AAAAAAAAAAAAAA-AAAAAAAAAA-A").unwrap();
        let b = InchiKey::from_bytes(b"BAAAAAAAAAAAAA-AAAAAAAAAA-A").unwrap();
        assert!(a < b);
    }

    #[test]
    fn compression_inference() {
        assert_eq!(
            InputCompression::from_path(Path::new("x.gz")),
            InputCompression::Gzip
        );
        assert_eq!(
            InputCompression::from_path(Path::new("x.bz2")),
            InputCompression::Bzip2
        );
        assert_eq!(
            InputCompression::from_path(Path::new("x.zst")),
            InputCompression::Zstd
        );
        assert_eq!(
            InputCompression::from_path(Path::new("x.txt")),
            InputCompression::Plain
        );
        assert_eq!(
            InputCompression::from_path(Path::new("x")),
            InputCompression::Plain
        );
    }

    #[test]
    fn tab_fields_splits() {
        let line = b"smiles\tZINC123\tINCHIKEY";
        let fields: Vec<&[u8]> = tab_fields(line).collect();
        assert_eq!(
            fields,
            vec![&b"smiles"[..], &b"ZINC123"[..], &b"INCHIKEY"[..]]
        );
    }

    #[test]
    fn trim_handles_crlf() {
        let mut v = b"hello\r\n".to_vec();
        trim_line_end(&mut v);
        assert_eq!(v, b"hello");
        let mut v2 = b"world\n".to_vec();
        trim_line_end(&mut v2);
        assert_eq!(v2, b"world");
    }
}
