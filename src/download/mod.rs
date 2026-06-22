//! HTTP download helpers.
//!
//! [`ensure_file`] is a simple "fetch this file if absent" primitive used for
//! the PubChem `CID-InChI-Key.gz` file. The [`enamine`] submodule adds an
//! authenticated, resumable downloader for the much larger Enamine REAL files.

pub mod enamine;

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::{LosError, Result};

/// Ensures `dest` exists, downloading it from `url` if it does not.
///
/// If `dest` already exists and is non-empty, the download is skipped. The
/// download is written to a sibling `*.part` file and atomically renamed on
/// success, so an interrupted download never leaves a truncated file at `dest`.
pub fn ensure_file(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() && std::fs::metadata(dest).is_ok_and(|m| m.len() > 0) {
        log::info!("download: {} already present, skipping", dest.display());
        return Ok(());
    }
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| LosError::io(format!("mkdir {}", parent.display()), e))?;
    }

    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));
    log::info!("download: GET {url} -> {}", dest.display());

    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| LosError::Download(format!("building HTTP client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| LosError::Download(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(LosError::Download(format!(
            "GET {url} returned status {}",
            resp.status()
        )));
    }

    {
        let total = resp.content_length();
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download")
            .to_string();
        let pb = crate::progress::download_bar(name, total);
        let mut reader = pb.wrap_read(resp);
        let file = File::create(&part)
            .map_err(|e| LosError::io(format!("create {}", part.display()), e))?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        std::io::copy(&mut reader, &mut writer)
            .map_err(|e| LosError::io(format!("writing {}", part.display()), e))?;
        pb.finish_and_clear();
    }
    std::fs::rename(&part, dest).map_err(|e| {
        LosError::io(
            format!("rename {} -> {}", part.display(), dest.display()),
            e,
        )
    })?;
    log::info!("download: completed {}", dest.display());
    Ok(())
}
