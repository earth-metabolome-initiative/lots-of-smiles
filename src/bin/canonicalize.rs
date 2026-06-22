//! `canonicalize` subcommand: a streaming, parallel SMILES canonicalizer.
//!
//! Reads one record per line from stdin (the first tab-separated field is taken
//! as the SMILES), parses and canonicalizes it with `smiles-parser`, and writes
//! the canonical SMILES (one per line) to stdout. Output order is arbitrary, it
//! is meant to be piped into `sort -u`. Lines whose SMILES fails to parse are
//! dropped and counted (reported to stderr at the end).
//!
//! Example:
//! ```text
//! zstd -dc --long=31 corpus.smiles-only.smi.zst \
//!   | lots-of-smiles canonicalize --threads 64 \
//!   | LC_ALL=C sort -u --parallel=64 -S 200G \
//!   | zstd -19 --long=31 -T0 -o corpus.canonical.smi.zst
//! ```

use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use clap::Args;
use indicatif::{ProgressBar, ProgressStyle};
use lots_of_smiles::{LosError, Result};
use smiles_parser::prelude::Smiles;

/// Flags for the `canonicalize` subcommand.
#[derive(Debug, Args)]
pub struct CanonicalizeArgs {
    /// Number of worker threads.
    #[arg(long, default_value_t = default_threads())]
    threads: usize,
    /// Lines per work batch handed to a worker.
    #[arg(long, default_value_t = 16384)]
    batch: usize,
    /// Expected total input lines, for a progress bar with percentage and ETA.
    /// If omitted, a count/rate spinner is shown instead. The bar is drawn on
    /// stderr, so stdout stays clean for the canonical SMILES stream.
    #[arg(long)]
    total: Option<u64>,
    /// Write each SMILES that fails to parse (with its error) to this file,
    /// tab-separated as `SMILES<TAB>error`.
    #[arg(long)]
    failed: Option<PathBuf>,
    /// Parse only, to find failures: skip canonicalization and stdout output.
    /// ~100x faster than a full run when you just want the failure list.
    #[arg(long)]
    check_only: bool,
}

/// Sink for failing records, shared across workers. Failures are rare, so the
/// mutex is effectively uncontended.
type FailSink = Option<Arc<Mutex<BufWriter<std::fs::File>>>>;

/// Builds the stderr progress bar (percentage bar with ETA when `total` is
/// known, otherwise a count/rate spinner).
fn make_progress(total: Option<u64>) -> ProgressBar {
    if let Some(t) = total {
        let pb = ProgressBar::new(t);
        if let Ok(style) = ProgressStyle::with_template(
            "{spinner:.green} {human_pos}/{human_len} ({percent}%, {per_sec}, ETA {eta})",
        ) {
            pb.set_style(style);
        }
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        if let Ok(style) =
            ProgressStyle::with_template("{spinner:.green} {human_pos} canonicalized ({per_sec})")
        {
            pb.set_style(style);
        }
        pb
    }
}

fn default_threads() -> usize {
    num_cpus::get().max(1)
}

/// Runs the canonicalization pass described by `args`.
#[allow(
    clippy::too_many_lines,
    reason = "single-purpose driver: thread setup, the reader loop, and shutdown read most clearly inline"
)]
pub fn run(args: &CanonicalizeArgs) -> Result<()> {
    let threads = args.threads.max(1);
    let batch = args.batch.max(1);

    let total = Arc::new(AtomicU64::new(0));
    let parsed = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let check_only = args.check_only;

    // Optional failure sink: failing SMILES + parse error, tab-separated.
    let fail_sink: FailSink = match &args.failed {
        Some(path) => {
            let file = std::fs::File::create(path).map_err(|e| {
                LosError::io(format!("creating --failed file {}", path.display()), e)
            })?;
            Some(Arc::new(Mutex::new(BufWriter::new(file))))
        }
        None => None,
    };

    // One bounded input channel per worker (main round-robins batches), and a
    // single shared output channel many workers -> one writer.
    let mut work_tx = Vec::with_capacity(threads);
    let mut work_rx = Vec::with_capacity(threads);
    for _ in 0..threads {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<String>>(4);
        work_tx.push(tx);
        work_rx.push(rx);
    }
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(threads * 4);

    std::thread::scope(|scope| {
        // Writer thread: drains output buffers to stdout.
        scope.spawn(move || {
            let stdout = std::io::stdout();
            let mut w = BufWriter::with_capacity(1 << 20, stdout.lock());
            while let Ok(buf) = out_rx.recv() {
                if w.write_all(&buf).is_err() {
                    break;
                }
            }
            let _ = w.flush();
        });

        // Worker threads: parse (and, unless --check-only, canonicalize) each
        // line in their batches.
        for rx in work_rx {
            let out_tx = out_tx.clone();
            let (parsed, failed) = (Arc::clone(&parsed), Arc::clone(&failed));
            let fail_sink = fail_sink.clone();
            scope.spawn(move || {
                let mut local_parsed = 0u64;
                let mut local_failed = 0u64;
                let mut fail_buf: Vec<u8> = Vec::new();
                while let Ok(lines) = rx.recv() {
                    let mut buf = Vec::with_capacity(if check_only { 0 } else { lines.len() * 48 });
                    for line in lines {
                        let smi = line.split('\t').next().unwrap_or("").trim();
                        if smi.is_empty() {
                            continue;
                        }
                        match smi.parse::<Smiles>() {
                            Ok(s) => {
                                if !check_only {
                                    let canon = s.canonicalize().to_string();
                                    buf.extend_from_slice(canon.as_bytes());
                                    buf.push(b'\n');
                                }
                                local_parsed += 1;
                            }
                            Err(e) => {
                                local_failed += 1;
                                if fail_sink.is_some() {
                                    fail_buf.clear();
                                    fail_buf.extend_from_slice(smi.as_bytes());
                                    fail_buf.push(b'\t');
                                    fail_buf.extend_from_slice(format!("{e}").as_bytes());
                                    fail_buf.push(b'\n');
                                    if let Some(sink) = &fail_sink
                                        && let Ok(mut w) = sink.lock()
                                    {
                                        let _ = w.write_all(&fail_buf);
                                    }
                                }
                            }
                        }
                    }
                    if !buf.is_empty() && out_tx.send(buf).is_err() {
                        break;
                    }
                }
                parsed.fetch_add(local_parsed, Ordering::Relaxed);
                failed.fetch_add(local_failed, Ordering::Relaxed);
            });
        }
        drop(out_tx); // workers hold the only remaining senders

        // Main thread: read stdin, batch lines, round-robin to workers.
        // The progress bar advances as input is consumed; bounded channels
        // apply backpressure, so the read rate tracks the canonicalization rate.
        let pb = make_progress(args.total);
        pb.enable_steady_tick(std::time::Duration::from_millis(500));
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut line = String::new();
        let mut batch_buf: Vec<String> = Vec::with_capacity(batch);
        let mut next = 0usize;
        let mut count: u64 = 0;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            batch_buf.push(trimmed.to_string());
            count += 1;
            if batch_buf.len() >= batch {
                let n = batch_buf.len() as u64;
                let chunk = std::mem::replace(&mut batch_buf, Vec::with_capacity(batch));
                if work_tx[next].send(chunk).is_err() {
                    break;
                }
                next = (next + 1) % work_tx.len();
                pb.inc(n);
            }
        }
        if !batch_buf.is_empty() {
            let n = batch_buf.len() as u64;
            let _ = work_tx[next].send(batch_buf);
            pb.inc(n);
        }
        pb.finish_and_clear();
        total.store(count, Ordering::Relaxed);
        drop(work_tx); // close inputs so workers finish
    });

    // Flush the failure file now that all workers have finished.
    if let Some(sink) = &fail_sink
        && let Ok(mut w) = sink.lock()
    {
        let _ = w.flush();
    }

    let verb = if check_only {
        "checked"
    } else {
        "canonicalized"
    };
    eprintln!(
        "canonicalize: {} read, {} {verb}, {} failed",
        total.load(Ordering::Relaxed),
        parsed.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
    );
    Ok(())
}
