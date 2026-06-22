//! Progress-bar helpers built on `indicatif`.
//!
//! Staging runs one spinner per source showing the running emitted-row count
//! and rate. Bars are cheap to clone (`Arc`-backed) and `Send`/`Sync`, so each
//! staging thread holds its own handle.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Builds a byte-oriented progress bar for a download, showing downloaded /
/// total bytes, rate, and ETA. When `total` is unknown a spinner with the
/// downloaded byte count and rate is used instead.
pub fn download_bar(message: impl Into<String>, total: Option<u64>) -> ProgressBar {
    let pb = match total {
        Some(t) => ProgressBar::new(t),
        None => ProgressBar::new_spinner(),
    };
    let template = if total.is_some() {
        "{spinner:.green} {msg} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})"
    } else {
        "{spinner:.green} {msg} {bytes} ({bytes_per_sec})"
    };
    if let Ok(style) = ProgressStyle::with_template(template) {
        pb.set_style(style);
    }
    pb.set_message(message.into());
    pb
}

/// Builds a [`MultiProgress`] with one spinner per source `tag`.
pub fn staging_bars(tags: &[&str]) -> (MultiProgress, Vec<ProgressBar>) {
    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{spinner:.green} {prefix:.bold} {human_pos} emitted ({per_sec})",
    )
    .unwrap_or_else(|_| ProgressStyle::default_spinner());
    let bars = tags
        .iter()
        .map(|tag| {
            let pb = multi.add(ProgressBar::new_spinner());
            pb.set_style(style.clone());
            pb.set_prefix(tag.to_string());
            pb
        })
        .collect();
    (multi, bars)
}
