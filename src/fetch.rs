//! Artifact download for OTA updates.
//!
//! Edge devices sit on flaky links, so downloads go to a `.part` file, are
//! retried with exponential backoff, and are only accepted when the number of
//! bytes received matches `Content-Length`. A truncated or stalled transfer
//! never reaches verification, let alone an app slot.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use log::warn;

use crate::error::{EdgeError, Result};
use crate::fsutil;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Timeout for a whole attempt (connect + headers + body).
    pub timeout: Duration,
    /// Extra attempts after the first one.
    pub retries: u32,
    /// Initial backoff, doubled after every failed attempt.
    pub backoff: Duration,
    /// Largest artifact accepted.
    pub max_bytes: u64,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            timeout: Duration::from_secs(60),
            retries: 3,
            backoff: Duration::from_millis(500),
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

fn is_remote(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Fetch `source` (path, `file://` URL or `http(s)://` URL) into `dest`.
pub fn fetch_bundle(source: &str, dest: &Path, opts: &FetchOptions) -> Result<u64> {
    if is_remote(source) {
        return download_with_retries(source, dest, opts);
    }
    let path = Path::new(source.strip_prefix("file://").unwrap_or(source));
    let len = fs::metadata(path)
        .map_err(|e| EdgeError::Fetch(format!("artifact not found at {}: {e}", path.display())))?
        .len();
    if len > opts.max_bytes {
        return Err(EdgeError::Fetch(format!(
            "artifact is {len} bytes, limit is {}",
            opts.max_bytes
        )));
    }
    fs::copy(path, dest)?;
    Ok(len)
}

fn download_with_retries(url: &str, dest: &Path, opts: &FetchOptions) -> Result<u64> {
    let mut delay = opts.backoff;
    let mut last = String::new();
    for attempt in 0..=opts.retries {
        match download_once(url, dest, opts) {
            Ok(n) => return Ok(n),
            Err(e) => {
                last = e;
                warn!(
                    "download attempt {}/{} for {url} failed: {last}",
                    attempt + 1,
                    opts.retries + 1
                );
                if attempt < opts.retries {
                    thread::sleep(delay);
                    delay = delay.saturating_mul(2);
                }
            }
        }
    }
    Err(EdgeError::Fetch(format!(
        "{url}: giving up after {} attempts: {last}",
        opts.retries + 1
    )))
}

fn download_once(url: &str, dest: &Path, opts: &FetchOptions) -> std::result::Result<u64, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(opts.timeout))
        .http_status_as_error(true)
        .build()
        .into();
    let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let expected: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok());
    if let Some(n) = expected {
        if n > opts.max_bytes {
            return Err(format!(
                "artifact is {n} bytes, limit is {}",
                opts.max_bytes
            ));
        }
    }

    let part = partial_path(dest);
    let result = (|| {
        let mut out = File::create(&part).map_err(|e| e.to_string())?;
        let mut body = resp.body_mut().with_config().limit(opts.max_bytes).reader();
        let got =
            io::copy(&mut body, &mut out).map_err(|e| format!("transfer interrupted: {e}"))?;
        if let Some(n) = expected {
            if got != n {
                return Err(format!("truncated transfer: received {got} of {n} bytes"));
            }
        }
        out.sync_all().map_err(|e| e.to_string())?;
        Ok(got)
    })();
    match result {
        Ok(got) => {
            fs::rename(&part, dest).map_err(|e| e.to_string())?;
            Ok(got)
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Fetch an artifact into `staging/` and unpack it into `staging/layout`.
pub fn stage(source: &str, staging: &Path, opts: &FetchOptions) -> Result<PathBuf> {
    fs::create_dir_all(staging)?;
    let bundle = staging.join("bundle.tar");
    fetch_bundle(source, &bundle, opts)?;
    let layout = staging.join("layout");
    fsutil::safe_unpack(File::open(&bundle)?, &layout)?;
    fs::remove_file(&bundle)?;
    Ok(layout)
}
