//! Authenticated, resumable downloader for Enamine REAL files.
//!
//! Enamine.net runs Joomla. Logging in requires a form POST to
//! `/login?task=user.login` carrying the standard `username`/`password` fields
//! plus a per-session CSRF token (a hidden input whose name is a 32-hex string
//! and whose value is `1`), which is scraped from the login page. The resulting
//! session cookie authorizes downloads through the Joomla download component at
//! `/component/download/?task=file.download&f=<file_id>`, which redirects to the
//! actual `.cxsmiles.bz2` stream.
//!
//! Downloads are resumable via HTTP range requests: a partial `*.part` file is
//! continued with a `Range` header when the server returns `206 Partial
//! Content`, and restarted from scratch on a `200 OK`.
//!
//! NOTE: the file ids and login mechanism reflect the current Enamine site and
//! may change if Enamine restructures the page.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;

use crate::config::credentials::EnamineCredentials;
use crate::{LosError, Result};

/// An authenticated Enamine download session.
///
/// Holds the credentials so it can re-authenticate before each file: Enamine's
/// download component appears to authorize only a single download per session,
/// so a fresh login is performed for every file.
pub struct EnamineClient {
    client: Client,
    base_url: String,
    username: String,
    password: String,
}

impl EnamineClient {
    /// Builds a client and validates the credentials with an initial login.
    /// `base_url` is the Enamine site root, e.g. `https://enamine.net`.
    pub fn login(credentials: &EnamineCredentials, base_url: &str) -> Result<Self> {
        let client = Client::builder()
            .cookie_store(true)
            .build()
            .map_err(|e| LosError::Download(format!("building HTTP client: {e}")))?;
        let me = Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            username: credentials.username.clone(),
            password: credentials.password.clone(),
        };
        // Validate credentials early so a bad password fails before downloading.
        me.authenticate()?;
        log::info!("enamine: logged in as {}", me.username);
        Ok(me)
    }

    /// Performs the Joomla form login on this client's cookie jar, establishing
    /// (or refreshing) an authenticated session.
    fn authenticate(&self) -> Result<()> {
        let base = &self.base_url;
        // 1. Fetch the login page for a session cookie and the CSRF token.
        let login_page = format!("{base}/login");
        let html = self
            .client
            .get(&login_page)
            .send()
            .map_err(|e| LosError::Download(format!("GET {login_page}: {e}")))?
            .error_for_status()
            .map_err(|e| LosError::Download(format!("login page error: {e}")))?
            .text()
            .map_err(|e| LosError::Download(format!("reading login page: {e}")))?;
        let token = scrape_csrf_token(&html)
            .ok_or_else(|| LosError::Credentials("could not find the login CSRF token".into()))?;

        // 2. POST the credentials.
        let body = encode_form(&[
            ("username", self.username.as_str()),
            ("password", self.password.as_str()),
            ("remember", "yes"),
            ("return", "MTAx"),
            (token.as_str(), "1"),
        ]);
        let resp = self
            .client
            .post(format!("{base}/login?task=user.login"))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .map_err(|e| LosError::Download(format!("login POST failed: {e}")))?;
        // Joomla redirects (followed by reqwest) to the return page on success,
        // or back to the login page on failure.
        if resp.url().path().contains("login") {
            return Err(LosError::Credentials(
                "Enamine login failed (still on the login page); check ENAMINE_USERNAME/ENAMINE_PASSWORD".into(),
            ));
        }
        Ok(())
    }

    /// Downloads the file with download-component `file_id` into `dest`,
    /// resuming a prior partial download when possible. `name` is used only for
    /// log messages.
    ///
    /// Bytes are written to a sibling `*.part` file and atomically renamed to
    /// `dest` on success.
    pub fn download(&self, file_id: u32, name: &str, dest: &Path) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 6;
        let url = format!(
            "{}/component/download/?task=file.download&f={file_id}",
            self.base_url
        );
        let part = dest.with_extension("part");

        for attempt in 1..=MAX_ATTEMPTS {
            // Resume from whatever is already on disk; re-authenticate each
            // attempt (Enamine authorizes one download per session).
            let existing = std::fs::metadata(&part).map_or(0, |m| m.len());
            log::info!(
                "enamine: downloading {name} (f={file_id}, attempt {attempt}/{MAX_ATTEMPTS}, resume from {existing} bytes) -> {}",
                dest.display()
            );
            self.authenticate()?;

            let mut request = self.client.get(&url);
            if existing > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
            }
            let resp = match request.send() {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    log::warn!("enamine: request for {name} failed ({e}); retrying");
                    Self::backoff(attempt);
                    continue;
                }
                Err(e) => return Err(LosError::Download(format!("GET {url}: {e}"))),
            };
            if resp.url().path().contains("login") {
                if attempt < MAX_ATTEMPTS {
                    log::warn!("enamine: session not authorized for {name}; re-authenticating");
                    Self::backoff(attempt);
                    continue;
                }
                return Err(LosError::Credentials(format!(
                    "download of {name} redirected to login; the session is not authorized"
                )));
            }

            // Total size: a fresh 200 gives the full Content-Length; a resumed
            // 206 gives the remaining length on top of what is already on disk.
            let remaining = resp.content_length();
            let (mut writer, start, total) = match resp.status() {
                StatusCode::PARTIAL_CONTENT => {
                    let file = OpenOptions::new().append(true).open(&part).map_err(|e| {
                        LosError::io(format!("open {} for append", part.display()), e)
                    })?;
                    (
                        BufWriter::with_capacity(1 << 20, file),
                        existing,
                        remaining.map(|r| existing + r),
                    )
                }
                StatusCode::OK => {
                    // The server ignored the range (or this is a fresh start):
                    // (re)create the file from scratch.
                    let file = std::fs::File::create(&part)
                        .map_err(|e| LosError::io(format!("create {}", part.display()), e))?;
                    (BufWriter::with_capacity(1 << 20, file), 0, remaining)
                }
                other => {
                    return Err(LosError::Download(format!(
                        "GET {url} returned status {other}"
                    )));
                }
            };

            let pb = crate::progress::download_bar(name.to_string(), total);
            pb.set_position(start);
            let mut reader = pb.wrap_read(resp);
            let copy_result = std::io::copy(&mut reader, &mut writer);
            let _ = writer.flush();
            drop(writer);
            pb.finish_and_clear();

            match copy_result {
                Ok(_) => {
                    std::fs::rename(&part, dest).map_err(|e| {
                        LosError::io(
                            format!("rename {} -> {}", part.display(), dest.display()),
                            e,
                        )
                    })?;
                    let size = std::fs::metadata(dest).map_or(0, |m| m.len());
                    log::info!("enamine: completed {} ({size} bytes)", dest.display());
                    return Ok(());
                }
                Err(e) if attempt < MAX_ATTEMPTS => {
                    log::warn!(
                        "enamine: stream interrupted for {name} ({e}); re-authenticating and resuming"
                    );
                    Self::backoff(attempt);
                }
                Err(e) => return Err(LosError::io(format!("writing {}", part.display()), e)),
            }
        }
        Err(LosError::Download(format!(
            "download of {name} failed after {MAX_ATTEMPTS} attempts"
        )))
    }

    /// Sleeps with capped exponential backoff before a retry.
    fn backoff(attempt: u32) {
        let secs = (1u64 << attempt.min(5)).min(30);
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

/// Finds the Joomla CSRF token in a login page: a hidden input whose name is a
/// 32-character hex string and whose value is `1`.
fn scrape_csrf_token(html: &str) -> Option<String> {
    for (idx, _) in html.match_indices("value=\"1\"") {
        let prefix = &html[..idx];
        let Some(name_pos) = prefix.rfind("name=\"") else {
            continue;
        };
        let after = &prefix[name_pos + 6..];
        let Some(end) = after.find('"') else {
            continue;
        };
        let candidate = &after[..end];
        if candidate.len() == 32 && candidate.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Percent-encodes a value for `application/x-www-form-urlencoded`.
fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

/// Encodes `(key, value)` pairs into a form-urlencoded body.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrapes_joomla_token() {
        let html = r#"
            <input type="hidden" name="return" value="MTAx">
            <input type="hidden" name="5a363b530e50fde8a7cc4b14aede6ea7" value="1">
            <input type="hidden" name="cf[form_id]" value="2">
        "#;
        assert_eq!(
            scrape_csrf_token(html).as_deref(),
            Some("5a363b530e50fde8a7cc4b14aede6ea7")
        );
    }

    #[test]
    fn token_absent_returns_none() {
        let html = r#"<input type="hidden" name="return" value="MTAx">"#;
        assert!(scrape_csrf_token(html).is_none());
    }

    #[test]
    fn form_encoding_escapes_special_chars() {
        // Passwords with shell/URL-special characters must be encoded.
        let body = encode_form(&[("username", "a@b.com"), ("password", "$~Y+x #1")]);
        assert_eq!(body, "username=a%40b.com&password=%24~Y%2Bx+%231");
    }
}
