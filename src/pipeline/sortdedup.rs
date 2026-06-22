//! External sort plus deterministic first-wins dedup.
//!
//! Stage files contain lines `INCHIKEY \t PRIORITY \t SMILES`. We invoke GNU
//! `sort` to order by `(INCHIKEY asc, PRIORITY asc)` under `LC_ALL=C` byte
//! ordering, then stream its stdout through an adjacent-dedup pass that keeps
//! the first line of each InChIKey run. Because the run is ordered by ascending
//! priority, "first line" is exactly the lowest-priority (highest-precedence)
//! source: deterministic first-wins.
//!
//! We deliberately do not use `sort -u`, which keeps an arbitrary representative
//! among equal keys and so cannot honor first-wins.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::io::InchiKey;
use crate::pipeline::writer::OutputWriter;
use crate::{LosError, Result};

/// Counters from the sort/dedup stage.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DedupStats {
    /// Total lines read from the sorted stream.
    pub input_lines: u64,
    /// Unique InChIKeys emitted to the output.
    pub unique_emitted: u64,
    /// Lines dropped as duplicates of an already-emitted key.
    pub duplicates_dropped: u64,
    /// Lines skipped because they were structurally invalid.
    pub malformed_lines: u64,
}

/// Sorts the `stage_files` and writes the first-wins-deduplicated records to
/// `writer`. `priority_to_tag` maps a priority digit to its source tag for the
/// output's optional source column.
pub fn sort_and_dedup(
    stage_files: &[PathBuf],
    scratch_dir: &Path,
    parallelism: usize,
    buffer: &str,
    writer: &mut OutputWriter,
    priority_to_tag: &[&str],
) -> Result<DedupStats> {
    if stage_files.is_empty() {
        return Ok(DedupStats::default());
    }

    let mut cmd = Command::new("sort");
    cmd.env("LC_ALL", "C")
        .env("TMPDIR", scratch_dir)
        .arg("-k1,1")
        .arg("-k2,2n")
        .arg("-t")
        .arg("\t")
        .arg(format!("--parallel={parallelism}"))
        .arg("-S")
        .arg(buffer)
        .arg("-T")
        .arg(scratch_dir);
    for f in stage_files {
        cmd.arg(f);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| LosError::Sort(format!("failed to spawn `sort`: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LosError::Sort("no stdout from `sort`".into()))?;
    let reader = BufReader::with_capacity(1 << 20, stdout);

    let stats = dedup_sorted(reader, writer, priority_to_tag)?;

    let exit_status = child
        .wait()
        .map_err(|e| LosError::Sort(format!("waiting for `sort`: {e}")))?;
    if !exit_status.success() {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut err);
        }
        return Err(LosError::Sort(format!(
            "`sort` exited with {exit_status}: {}",
            err.trim()
        )));
    }
    Ok(stats)
}

/// Reads an already-sorted `(key, priority, smiles)` stream and emits the first
/// line of each key run. Public for unit testing with synthetic sorted input.
pub fn dedup_sorted<R: BufRead>(
    mut reader: R,
    writer: &mut OutputWriter,
    priority_to_tag: &[&str],
) -> Result<DedupStats> {
    let mut stats = DedupStats::default();
    let mut last_key: Option<InchiKey> = None;
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| LosError::io("reading sorted stream", e))?;
        if n == 0 {
            break;
        }
        crate::io::trim_line_end(&mut line);
        if line.is_empty() {
            continue;
        }
        stats.input_lines += 1;

        // Parse INCHIKEY \t PRIORITY \t SMILES.
        let mut fields = line.splitn(3, |&b| b == b'\t');
        let key_bytes = fields.next();
        let prio_bytes = fields.next();
        let smiles = fields.next();
        let (Some(key_bytes), Some(prio_bytes), Some(smiles)) = (key_bytes, prio_bytes, smiles)
        else {
            stats.malformed_lines += 1;
            continue;
        };
        let Some(key) = InchiKey::from_bytes(key_bytes) else {
            stats.malformed_lines += 1;
            continue;
        };

        // Only the first line of each key run is kept.
        if last_key == Some(key) {
            stats.duplicates_dropped += 1;
            continue;
        }

        let tag = prio_bytes
            .first()
            .map(|b| b.wrapping_sub(b'0') as usize)
            .and_then(|i| priority_to_tag.get(i).copied())
            .unwrap_or("");
        writer.write_record(&key, smiles, tag)?;
        stats.unique_emitted += 1;
        last_key = Some(key);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::output::{Columns, OutputFormat};

    fn writer_for(path: &Path, columns: Columns) -> OutputWriter {
        let fmt = OutputFormat::builder()
            .path(path)
            .columns(columns)
            .build()
            .unwrap();
        OutputWriter::create(&fmt).unwrap()
    }

    #[test]
    fn first_wins_keeps_lowest_priority() {
        // Pre-sorted input: key A appears from priority 0 (zinc) and 1 (pubchem).
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.tsv");
        let mut w = writer_for(&out, Columns::SmilesInchikeySource);
        let sorted = concat!(
            "AAAAAAAAAAAAAA-AAAAAAAAAA-A\t0\tCCO\n",
            "AAAAAAAAAAAAAA-AAAAAAAAAA-A\t1\tOCC\n",
            "BBBBBBBBBBBBBB-AAAAAAAAAA-A\t1\tCN\n",
        );
        let tags = ["zinc20", "pubchem", "enamine"];
        let stats = dedup_sorted(sorted.as_bytes(), &mut w, &tags).unwrap();
        w.finish().unwrap();
        assert_eq!(stats.input_lines, 3);
        assert_eq!(stats.unique_emitted, 2);
        assert_eq!(stats.duplicates_dropped, 1);
        let content = std::fs::read_to_string(&out).unwrap();
        // Key A kept the priority-0 (zinc20) representative `CCO`.
        assert_eq!(
            content,
            "CCO\tAAAAAAAAAAAAAA-AAAAAAAAAA-A\tzinc20\nCN\tBBBBBBBBBBBBBB-AAAAAAAAAA-A\tpubchem\n"
        );
    }

    #[test]
    fn handles_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.smi");
        let mut w = writer_for(&out, Columns::SmilesOnly);
        let sorted = concat!(
            "not-a-valid-line\n",
            "AAAAAAAAAAAAAA-AAAAAAAAAA-A\t0\tCCO\n"
        );
        let stats = dedup_sorted(sorted.as_bytes(), &mut w, &["zinc20"]).unwrap();
        w.finish().unwrap();
        assert_eq!(stats.malformed_lines, 1);
        assert_eq!(stats.unique_emitted, 1);
    }

    #[test]
    fn end_to_end_with_real_sort() {
        // Exercises the actual `sort` subprocess with unsorted stage files.
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();

        let stage_a = dir.path().join("stage_zinc20.tsv");
        let stage_b = dir.path().join("stage_pubchem.tsv");
        // Unsorted, with a cross-file collision on key B.
        std::fs::write(
            &stage_a,
            "CCCCCCCCCCCCCC-AAAAAAAAAA-A\t0\tCCC\nBBBBBBBBBBBBBB-AAAAAAAAAA-A\t0\tCN\n",
        )
        .unwrap();
        std::fs::write(
            &stage_b,
            "BBBBBBBBBBBBBB-AAAAAAAAAA-A\t1\tNC\nAAAAAAAAAAAAAA-AAAAAAAAAA-A\t1\tCO\n",
        )
        .unwrap();

        let out = dir.path().join("corpus.tsv");
        let mut w = writer_for(&out, Columns::SmilesInchikeySource);
        let stats = sort_and_dedup(
            &[stage_a, stage_b],
            &scratch,
            2,
            "64M",
            &mut w,
            &["zinc20", "pubchem"],
        )
        .unwrap();
        w.finish().unwrap();

        assert_eq!(stats.input_lines, 4);
        assert_eq!(stats.unique_emitted, 3);
        assert_eq!(stats.duplicates_dropped, 1);

        let content = std::fs::read_to_string(&out).unwrap();
        // Sorted by key; key B kept the zinc20 (priority 0) representative.
        let expected = concat!(
            "CO\tAAAAAAAAAAAAAA-AAAAAAAAAA-A\tpubchem\n",
            "CN\tBBBBBBBBBBBBBB-AAAAAAAAAA-A\tzinc20\n",
            "CCC\tCCCCCCCCCCCCCC-AAAAAAAAAA-A\tzinc20\n",
        );
        assert_eq!(content, expected);
    }
}
