//! Health probes: process liveness, exec commands and HTTP endpoints.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::process::resolve_program;
use crate::spec::{HealthProbe, HealthSpec};

#[derive(Debug, Clone, PartialEq)]
pub enum ProbeResult {
    Healthy,
    Unhealthy(String),
}

impl ProbeResult {
    pub fn is_healthy(&self) -> bool {
        matches!(self, ProbeResult::Healthy)
    }
}

pub fn probe(spec: &HealthSpec, alive: bool, workdir: &Path) -> ProbeResult {
    if !alive {
        return ProbeResult::Unhealthy("process not running".into());
    }
    let timeout = Duration::from_millis(spec.timeout_ms);
    match &spec.probe {
        HealthProbe::Process => ProbeResult::Healthy,
        HealthProbe::Exec { command } => exec_probe(command, workdir, timeout),
        HealthProbe::Http { url } => http_probe(url, timeout),
    }
}

fn exec_probe(command: &[String], workdir: &Path, timeout: Duration) -> ProbeResult {
    let program = resolve_program(&command[0], workdir);
    let mut child = match Command::new(&program)
        .args(&command[1..])
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ProbeResult::Unhealthy(format!("probe failed to start: {e}")),
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return ProbeResult::Healthy,
            Ok(Some(status)) => {
                return ProbeResult::Unhealthy(format!("probe command exited with {status}"))
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeResult::Unhealthy(format!(
                    "probe command timed out after {}ms",
                    timeout.as_millis()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(e) => return ProbeResult::Unhealthy(format!("probe wait failed: {e}")),
        }
    }
}

/// Parse `http://host[:port][/path]`. TLS is intentionally out of scope for
/// local health endpoints.
pub fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

fn http_probe(url: &str, timeout: Duration) -> ProbeResult {
    match http_status(url, timeout) {
        Ok(code) if (200..300).contains(&code) => ProbeResult::Healthy,
        Ok(code) => ProbeResult::Unhealthy(format!("{url} returned HTTP {code}")),
        Err(e) => ProbeResult::Unhealthy(format!("{url}: {e}")),
    }
}

fn http_status(url: &str, timeout: Duration) -> std::io::Result<u16> {
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad url");
    let (host, port, path) = parse_http_url(url).ok_or_else(invalid)?;
    let addr = (host.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(invalid)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: edgeforge-health\r\nConnection: close\r\n\r\n"
    )?;
    let mut head = [0u8; 64];
    let mut n = 0;
    while n < head.len() {
        let r = stream.read(&mut head[n..])?;
        if r == 0 {
            break;
        }
        n += r;
        if head[..n].contains(&b'\n') {
            break;
        }
    }
    let line = String::from_utf8_lossy(&head[..n]);
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed HTTP status line",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:8080/healthz"),
            Some(("127.0.0.1".into(), 8080, "/healthz".into()))
        );
        assert_eq!(
            parse_http_url("http://device.local"),
            Some(("device.local".into(), 80, "/".into()))
        );
        assert_eq!(parse_http_url("https://x/"), None);
        assert_eq!(parse_http_url("http://:80/"), None);
    }

    #[test]
    fn http_probe_reads_status() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            for code in [200, 503] {
                let (mut s, _) = l.accept().unwrap();
                // Read the full request first; closing with unread data
                // would make the kernel send RST instead of our response.
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let url = format!("http://127.0.0.1:{port}/healthz");
        assert!(http_probe(&url, Duration::from_secs(2)).is_healthy());
        assert!(!http_probe(&url, Duration::from_secs(2)).is_healthy());
    }
}
